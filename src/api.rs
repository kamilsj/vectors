//! Actix Web interface for SQL execution and vector search.

mod admin;
mod embeddings;
mod graph;
mod ingest;
mod parameters;
mod relationships;
mod reranking;
mod response;
mod schema;

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use actix_web::dev::ServerHandle;
use actix_web::error::{BlockingError, InternalError, JsonPayloadError};
use actix_web::http::header::{
    EntityTag, IfNoneMatch, AUTHORIZATION, RETRY_AFTER, WWW_AUTHENTICATE,
};
use actix_web::http::{KeepAlive, StatusCode};
use actix_web::middleware::Compress;
use actix_web::{web, App, HttpMessage, HttpRequest, HttpResponse, HttpServer, ResponseError};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value as JsonValue};

use crate::{
    Column, DataType, Database, Error, ExecutionResult, InsertConflict, QueryIntent, QueryResult,
    Value, Vector, VectorFilterOperator, VectorSearch, VectorSearchFilter, VectorSearchMetric,
};

const DEFAULT_MAX_JSON_PAYLOAD_BYTES: usize = 32 * 1024 * 1024;
const MAX_JSON_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_MAX_BULK_ROWS: usize = 10_000;
const MAX_BULK_ROWS: usize = 1_000_000;
const DEFAULT_MAX_RESPONSE_ROWS: usize = 10_000;
const MAX_RESPONSE_ROWS: usize = 1_000_000;
const MAX_SEARCH_LIMIT: usize = 1_000;
const SHUTDOWN_FILE_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Bounds applied while decoding API requests and producing query responses.
///
/// The HTTP API buffers JSON before deserialization, so every deployment has
/// finite limits even when clients omit `Content-Length`. Large imports should
/// be split into batches and retried independently.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestLimits {
    max_json_payload_bytes: usize,
    max_bulk_rows: usize,
    max_response_rows: usize,
}

impl RequestLimits {
    /// Construct validated HTTP limits.
    pub fn new(
        max_json_payload_bytes: usize,
        max_bulk_rows: usize,
        max_response_rows: usize,
    ) -> io::Result<Self> {
        let limits = Self {
            max_json_payload_bytes,
            max_bulk_rows,
            max_response_rows,
        };
        limits.validate()?;
        Ok(limits)
    }

    /// Maximum accepted JSON request body in bytes.
    pub const fn max_json_payload_bytes(&self) -> usize {
        self.max_json_payload_bytes
    }

    /// Maximum rows accepted by one typed bulk-ingestion request.
    pub const fn max_bulk_rows(&self) -> usize {
        self.max_bulk_rows
    }

    /// Maximum total query rows returned by one SQL request. Structured search
    /// also bounds its requested limit by this value and its own 1,000-row cap.
    pub const fn max_response_rows(&self) -> usize {
        self.max_response_rows
    }

    fn validate(&self) -> io::Result<()> {
        validate_bounded_limit(
            "max_json_payload_bytes",
            self.max_json_payload_bytes,
            MAX_JSON_PAYLOAD_BYTES,
        )?;
        validate_bounded_limit("max_bulk_rows", self.max_bulk_rows, MAX_BULK_ROWS)?;
        validate_bounded_limit(
            "max_response_rows",
            self.max_response_rows,
            MAX_RESPONSE_ROWS,
        )
    }
}

impl Default for RequestLimits {
    fn default() -> Self {
        Self {
            max_json_payload_bytes: DEFAULT_MAX_JSON_PAYLOAD_BYTES,
            max_bulk_rows: DEFAULT_MAX_BULK_ROWS,
            max_response_rows: DEFAULT_MAX_RESPONSE_ROWS,
        }
    }
}

fn validate_bounded_limit(name: &str, value: usize, maximum: usize) -> io::Result<()> {
    if value == 0 || value > maximum {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must be between 1 and {maximum}"),
        ));
    }
    Ok(())
}

/// Capacity and connection settings for the standalone HTTP server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerConfig {
    pub workers: usize,
    pub max_blocking_threads_per_worker: usize,
    pub max_connections_per_worker: usize,
    pub max_concurrent_database_tasks: usize,
    pub keep_alive: Duration,
    pub client_request_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub request_limits: RequestLimits,
}

impl Default for ServerConfig {
    fn default() -> Self {
        let parallelism = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1);
        Self {
            workers: parallelism,
            max_blocking_threads_per_worker: 1,
            max_connections_per_worker: 4_096,
            max_concurrent_database_tasks: parallelism,
            keep_alive: Duration::from_secs(30),
            client_request_timeout: Duration::from_secs(5),
            shutdown_timeout: Duration::from_secs(30),
            request_limits: RequestLimits::default(),
        }
    }
}

impl ServerConfig {
    fn validate(&self) -> io::Result<()> {
        self.request_limits.validate()?;
        for (name, value) in [
            ("workers", self.workers),
            (
                "max_blocking_threads_per_worker",
                self.max_blocking_threads_per_worker,
            ),
            (
                "max_connections_per_worker",
                self.max_connections_per_worker,
            ),
            (
                "max_concurrent_database_tasks",
                self.max_concurrent_database_tasks,
            ),
        ] {
            if value == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{name} must be greater than zero"),
                ));
            }
        }
        for (name, value) in [
            ("keep_alive", self.keep_alive),
            ("client_request_timeout", self.client_request_timeout),
        ] {
            if value.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{name} must be greater than zero"),
                ));
            }
        }
        if self.shutdown_timeout.as_secs() == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "shutdown_timeout must be at least one second",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct DatabaseTaskLimiter {
    state: Arc<DatabaseTaskState>,
}

#[derive(Debug)]
struct DatabaseTaskState {
    in_flight: AtomicUsize,
    rejected: AtomicU64,
    limit: usize,
}

impl DatabaseTaskLimiter {
    fn new(limit: usize) -> Self {
        Self {
            state: Arc::new(DatabaseTaskState {
                in_flight: AtomicUsize::new(0),
                rejected: AtomicU64::new(0),
                limit,
            }),
        }
    }

    fn acquire(&self) -> Result<DatabaseTaskPermit, ApiError> {
        let acquired = self
            .state
            .in_flight
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current < self.state.limit).then_some(current + 1)
            })
            .is_ok();
        if !acquired {
            self.state.rejected.fetch_add(1, Ordering::Relaxed);
            return Err(ApiError::overloaded());
        }
        Ok(DatabaseTaskPermit {
            state: self.state.clone(),
        })
    }
}

#[derive(Debug)]
struct DatabaseTaskPermit {
    state: Arc<DatabaseTaskState>,
}

impl Drop for DatabaseTaskPermit {
    fn drop(&mut self) {
        self.state.in_flight.fetch_sub(1, Ordering::Release);
    }
}

/// Optional authentication settings for applications embedding the HTTP API.
#[derive(Clone, Debug)]
pub struct ApiSecurity {
    bearer_token: Arc<str>,
}

impl ApiSecurity {
    /// Require this bearer token on every `/v1` request.
    pub fn bearer_token(token: impl Into<String>) -> Self {
        Self {
            bearer_token: Arc::from(token.into()),
        }
    }
}

/// Register all HTTP routes on an Actix application.
pub fn configure(config: &mut web::ServiceConfig) {
    configure_with_limits(config, RequestLimits::default());
}

/// Register all HTTP routes with explicit request and response limits.
pub fn configure_with_limits(config: &mut web::ServiceConfig, limits: RequestLimits) {
    // RequestLimits values can only be created after validation.
    config
        .app_data(
            web::JsonConfig::default()
                .limit(limits.max_json_payload_bytes)
                .error_handler(json_payload_error),
        )
        .app_data(web::Data::new(limits))
        .configure(configure_routes);
}

fn configure_routes(config: &mut web::ServiceConfig) {
    config
        .service(
            web::resource("/")
                .wrap(Compress::default())
                .route(web::get().to(console)),
        )
        .service(
            web::resource("/assets/app.css")
                .wrap(Compress::default())
                .route(web::get().to(console_styles)),
        )
        .service(
            web::resource("/assets/app.js")
                .wrap(Compress::default())
                .route(web::get().to(console_script)),
        )
        .route("/healthz", web::get().to(health))
        .route("/readyz", web::get().to(readiness))
        .route("/metrics", web::get().to(metrics))
        .service(
            web::scope("/v1")
                .configure(admin::configure)
                .configure(graph::configure)
                .configure(relationships::configure)
                .configure(reranking::configure)
                .route("/settings/server", web::get().to(server_settings))
                .route(
                    "/settings/embeddings",
                    web::get().to(embeddings::get_settings),
                )
                .route(
                    "/settings/embeddings",
                    web::put().to(embeddings::update_settings),
                )
                .route(
                    "/embeddings",
                    web::post().to(embeddings::generate_embeddings),
                )
                .route("/sql", web::post().to(execute_sql))
                .route("/sql/intent", web::post().to(query_intent))
                .route("/tables", web::get().to(tables))
                .route("/tables/{table}/schema", web::get().to(table_schema))
                .route("/tables/{table}/indexes", web::get().to(table_indexes))
                .route("/tables/{table}/rows", web::post().to(insert_rows))
                .route("/embeddings/search", web::post().to(vector_search))
                .route("/vector/search", web::post().to(vector_search)),
        );
}

static CONSOLE_HTML: ConsoleAsset = ConsoleAsset::new(
    include_str!("../web/index.html"),
    "text/html; charset=utf-8",
);
static CONSOLE_CSS: ConsoleAsset =
    ConsoleAsset::new(include_str!("../web/app.css"), "text/css; charset=utf-8");
static CONSOLE_JS: ConsoleAsset = ConsoleAsset::new(
    include_str!("../web/app.js"),
    "text/javascript; charset=utf-8",
);

struct ConsoleAsset {
    body: &'static str,
    content_type: &'static str,
    etag: OnceLock<EntityTag>,
}

impl ConsoleAsset {
    const fn new(body: &'static str, content_type: &'static str) -> Self {
        Self {
            body,
            content_type,
            etag: OnceLock::new(),
        }
    }

    fn etag(&self) -> &EntityTag {
        self.etag.get_or_init(|| {
            let mut hasher = DefaultHasher::new();
            self.body.hash(&mut hasher);
            // Derive the validator from content, not the package version: local
            // builds can change assets without a version bump. Weak tags remain
            // valid across the different negotiated compression encodings.
            EntityTag::new_weak(format!("{:x}-{:x}", self.body.len(), hasher.finish()))
        })
    }
}

async fn console(request: HttpRequest) -> HttpResponse {
    console_asset(&request, &CONSOLE_HTML)
}

async fn console_styles(request: HttpRequest) -> HttpResponse {
    console_asset(&request, &CONSOLE_CSS)
}

async fn console_script(request: HttpRequest) -> HttpResponse {
    console_asset(&request, &CONSOLE_JS)
}

fn console_asset(request: &HttpRequest, asset: &ConsoleAsset) -> HttpResponse {
    let etag = asset.etag();
    let not_modified = match request.get_header::<IfNoneMatch>() {
        Some(IfNoneMatch::Any) => true,
        Some(IfNoneMatch::Items(tags)) => tags.iter().any(|tag| tag.weak_eq(etag)),
        None => false,
    };
    let mut response = HttpResponse::build(if not_modified {
        StatusCode::NOT_MODIFIED
    } else {
        StatusCode::OK
    });
    response
        .insert_header(("content-type", asset.content_type))
        .insert_header(("cache-control", "no-cache"))
        .insert_header(("etag", etag.to_string()))
        .insert_header(("vary", "Accept-Encoding"))
        .insert_header(("x-content-type-options", "nosniff"))
        .insert_header(("referrer-policy", "no-referrer"))
        .insert_header((
            "content-security-policy",
            "default-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'",
        ));
    if not_modified {
        response.finish()
    } else {
        response.body(asset.body)
    }
}

/// Start an HTTP server backed by the supplied database handle.
pub async fn serve(database: Database, bind_address: &str) -> io::Result<()> {
    serve_with_config(database, bind_address, ServerConfig::default()).await
}

/// Start an HTTP server with explicit production capacity settings.
pub async fn serve_with_config(
    database: Database,
    bind_address: &str,
    config: ServerConfig,
) -> io::Result<()> {
    serve_inner(database, bind_address, None, config, None).await
}

/// Start a configured HTTP server with a private file-based shutdown signal.
///
/// `shutdown_file` must be an absolute path in a directory writable only by
/// the service owner. Creating a regular file at that path asks Actix to stop
/// gracefully. The request is removed before shutdown begins.
pub async fn serve_with_config_and_shutdown_file(
    database: Database,
    bind_address: &str,
    config: ServerConfig,
    shutdown_file: PathBuf,
) -> io::Result<()> {
    serve_inner(database, bind_address, None, config, Some(shutdown_file)).await
}

/// Start an HTTP server that requires a bearer token on every `/v1` route.
pub async fn serve_authenticated(
    database: Database,
    bind_address: &str,
    bearer_token: String,
) -> io::Result<()> {
    serve_authenticated_with_config(
        database,
        bind_address,
        bearer_token,
        ServerConfig::default(),
    )
    .await
}

/// Start a configured HTTP server with bearer authentication on `/v1` routes.
pub async fn serve_authenticated_with_config(
    database: Database,
    bind_address: &str,
    bearer_token: String,
    config: ServerConfig,
) -> io::Result<()> {
    if bearer_token.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "bearer token cannot be empty",
        ));
    }
    serve_inner(
        database,
        bind_address,
        Some(ApiSecurity::bearer_token(bearer_token)),
        config,
        None,
    )
    .await
}

/// Start an authenticated, configured server with a private file-based
/// shutdown signal.
pub async fn serve_authenticated_with_config_and_shutdown_file(
    database: Database,
    bind_address: &str,
    bearer_token: String,
    config: ServerConfig,
    shutdown_file: PathBuf,
) -> io::Result<()> {
    if bearer_token.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "bearer token cannot be empty",
        ));
    }
    serve_inner(
        database,
        bind_address,
        Some(ApiSecurity::bearer_token(bearer_token)),
        config,
        Some(shutdown_file),
    )
    .await
}

async fn serve_inner(
    database: Database,
    bind_address: &str,
    security: Option<ApiSecurity>,
    config: ServerConfig,
    shutdown_file: Option<PathBuf>,
) -> io::Result<()> {
    config.validate()?;
    if let Some(path) = shutdown_file.as_deref() {
        prepare_shutdown_file(path)?;
    }
    let embeddings = web::Data::new(crate::EmbeddingService::from_environment(
        database
            .data_directory()
            .map(|directory| directory.join("embedding-settings.json")),
    )?);
    let reranking = web::Data::new(crate::RerankingService::from_environment(
        database
            .data_directory()
            .map(|directory| directory.join("reranking-settings.json")),
    )?);
    let runtime_config = web::Data::new(config.clone());
    let database = web::Data::new(database);
    let limiter = web::Data::new(DatabaseTaskLimiter::new(
        config.max_concurrent_database_tasks,
    ));
    let request_limits = config.request_limits.clone();
    let server = HttpServer::new(move || {
        let mut app = App::new()
            .app_data(database.clone())
            .app_data(embeddings.clone())
            .app_data(reranking.clone())
            .app_data(runtime_config.clone())
            .app_data(limiter.clone());
        if let Some(security) = security.clone() {
            app = app.app_data(web::Data::new(security));
        }
        let request_limits = request_limits.clone();
        app.configure(move |services| configure_with_limits(services, request_limits))
    })
    .workers(config.workers)
    .worker_max_blocking_threads(config.max_blocking_threads_per_worker)
    .max_connections(config.max_connections_per_worker)
    .keep_alive(KeepAlive::Timeout(config.keep_alive))
    .client_request_timeout(config.client_request_timeout)
    .shutdown_timeout(config.shutdown_timeout.as_secs())
    .tcp_nodelay(true)
    .bind(bind_address)?
    .run();

    let shutdown_watcher = shutdown_file.as_ref().map(|path| {
        let path = path.clone();
        let handle = server.handle();
        actix_web::rt::spawn(watch_shutdown_file(path, handle))
    });
    let result = server.await;
    if let Some(watcher) = shutdown_watcher {
        watcher.abort();
    }
    if let Some(path) = shutdown_file.as_deref() {
        if let Err(error) = remove_shutdown_request(path) {
            eprintln!(
                "failed to clean up shutdown request {}: {error}",
                path.display()
            );
        }
    }
    result
}

fn prepare_shutdown_file(path: &Path) -> io::Result<()> {
    if !path.is_absolute() || path.file_name().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "shutdown_file must be an absolute file path",
        ));
    }
    remove_shutdown_request(path)
}

fn remove_shutdown_request(path: &Path) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "shutdown request {} must be a regular file, not a directory or symbolic link",
                path.display()
            ),
        ));
    }
    fs::remove_file(path)
}

async fn watch_shutdown_file(path: PathBuf, handle: ServerHandle) {
    let mut last_error = None;
    loop {
        actix_web::rt::time::sleep(SHUTDOWN_FILE_POLL_INTERVAL).await;
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                last_error = None;
            }
            Err(error) => {
                let message = error.to_string();
                if last_error.as_deref() != Some(message.as_str()) {
                    eprintln!(
                        "cannot inspect shutdown request {}: {error}",
                        path.display()
                    );
                    last_error = Some(message);
                }
            }
            Ok(metadata) if !metadata.file_type().is_file() => {
                let message = "request is not a regular file";
                if last_error.as_deref() != Some(message) {
                    eprintln!("ignoring shutdown request {}: {message}", path.display());
                    last_error = Some(message.into());
                }
            }
            Ok(_) => match fs::remove_file(&path) {
                Ok(()) => {
                    eprintln!("cooperative shutdown requested through {}", path.display());
                    handle.stop(true).await;
                    return;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    last_error = None;
                }
                Err(error) => {
                    let message = error.to_string();
                    if last_error.as_deref() != Some(message.as_str()) {
                        eprintln!(
                            "cannot consume shutdown request {}: {error}",
                            path.display()
                        );
                        last_error = Some(message);
                    }
                }
            },
        }
    }
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
    version: &'static str,
    storage: &'static str,
    max_json_payload_bytes: usize,
}

async fn health(
    database: web::Data<Database>,
    limits: web::Data<RequestLimits>,
) -> web::Json<HealthResponse> {
    web::Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        storage: if database.data_directory().is_some() {
            "durable"
        } else {
            "memory"
        },
        max_json_payload_bytes: limits.max_json_payload_bytes,
    })
}

async fn server_settings(
    request: HttpRequest,
    security: Option<web::Data<ApiSecurity>>,
    config: Option<web::Data<ServerConfig>>,
    limits: web::Data<RequestLimits>,
    database: web::Data<Database>,
) -> Result<web::Json<JsonValue>, ApiError> {
    authorize(&request, security.as_ref().map(|data| data.get_ref()))?;
    let compute = database.compute_config();
    let capacity = config.map(|config| {
        serde_json::json!({
            "workers": config.workers,
            "max_blocking_threads_per_worker": config.max_blocking_threads_per_worker,
            "max_connections_per_worker": config.max_connections_per_worker,
            "max_concurrent_database_tasks": config.max_concurrent_database_tasks,
            "keep_alive_seconds": config.keep_alive.as_secs(),
            "client_request_timeout_seconds": config.client_request_timeout.as_secs(),
            "shutdown_timeout_seconds": config.shutdown_timeout.as_secs(),
        })
    });
    Ok(web::Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "storage": if database.data_directory().is_some() { "durable" } else { "memory" },
        "authentication": security.is_some(),
        "compute": {
            "device": compute.device.to_string(),
            "gpu_enabled": cfg!(feature = "gpu"),
            "gpu_min_elements": compute.gpu_min_elements,
            "gpu_cache_bytes": compute.gpu_cache_bytes,
        },
        "limits": {
            "max_json_payload_bytes": limits.max_json_payload_bytes,
            "max_bulk_rows": limits.max_bulk_rows,
            "max_response_rows": limits.max_response_rows,
            "max_search_limit": MAX_SEARCH_LIMIT.min(limits.max_response_rows),
        },
        "capacity": capacity,
    })))
}

#[derive(Debug, Serialize)]
struct ReadinessResponse {
    status: &'static str,
    version: &'static str,
    storage: &'static str,
    revision: u64,
    database_tasks_in_flight: usize,
    database_task_limit: Option<usize>,
    max_json_payload_bytes: usize,
    max_bulk_rows: usize,
    max_response_rows: usize,
}

async fn readiness(
    database: web::Data<Database>,
    limiter: Option<web::Data<DatabaseTaskLimiter>>,
    limits: web::Data<RequestLimits>,
) -> Result<web::Json<ReadinessResponse>, ApiError> {
    let storage = if database.data_directory().is_some() {
        "durable"
    } else {
        "memory"
    };
    let database = database.get_ref().clone();
    let revision = web::block(move || database.revision())
        .await
        .map_err(ApiError::from_blocking)??;
    let (database_tasks_in_flight, database_task_limit) = limiter
        .as_ref()
        .map(|limiter| {
            (
                limiter.state.in_flight.load(Ordering::Acquire),
                Some(limiter.state.limit),
            )
        })
        .unwrap_or((0, None));
    Ok(web::Json(ReadinessResponse {
        status: "ready",
        version: env!("CARGO_PKG_VERSION"),
        storage,
        revision,
        database_tasks_in_flight,
        database_task_limit,
        max_json_payload_bytes: limits.max_json_payload_bytes,
        max_bulk_rows: limits.max_bulk_rows,
        max_response_rows: limits.max_response_rows,
    }))
}

async fn metrics(
    database: web::Data<Database>,
    limiter: Option<web::Data<DatabaseTaskLimiter>>,
    limits: web::Data<RequestLimits>,
) -> Result<HttpResponse, ApiError> {
    let database = database.get_ref().clone();
    let revision = web::block(move || database.revision())
        .await
        .map_err(ApiError::from_blocking)??;
    let (in_flight, limit, rejected) = limiter
        .as_ref()
        .map(|limiter| {
            (
                limiter.state.in_flight.load(Ordering::Acquire),
                limiter.state.limit,
                limiter.state.rejected.load(Ordering::Relaxed),
            )
        })
        .unwrap_or((0, 0, 0));
    let body = format!(
        "# HELP vectors_up Whether this vectors process is running.\n\
         # TYPE vectors_up gauge\n\
         vectors_up 1\n\
         # HELP vectors_catalog_revision Current committed catalog revision.\n\
         # TYPE vectors_catalog_revision gauge\n\
         vectors_catalog_revision {revision}\n\
         # HELP vectors_database_tasks_in_flight Database tasks queued or executing, including cancelled requests still doing work.\n\
         # TYPE vectors_database_tasks_in_flight gauge\n\
         vectors_database_tasks_in_flight {in_flight}\n\
         # HELP vectors_database_task_limit Maximum concurrent database tasks.\n\
         # TYPE vectors_database_task_limit gauge\n\
         vectors_database_task_limit {limit}\n\
         # HELP vectors_database_tasks_rejected_total Database tasks rejected by overload protection.\n\
         # TYPE vectors_database_tasks_rejected_total counter\n\
         vectors_database_tasks_rejected_total {rejected}\n\
         # HELP vectors_http_max_json_payload_bytes Maximum JSON request body size in bytes.\n\
         # TYPE vectors_http_max_json_payload_bytes gauge\n\
         vectors_http_max_json_payload_bytes {}\n\
         # HELP vectors_http_max_bulk_rows Maximum rows per typed ingestion request.\n\
         # TYPE vectors_http_max_bulk_rows gauge\n\
         vectors_http_max_bulk_rows {}\n\
         # HELP vectors_http_max_response_rows Maximum query rows returned by one SQL request.\n\
         # TYPE vectors_http_max_response_rows gauge\n\
         vectors_http_max_response_rows {}\n",
        limits.max_json_payload_bytes, limits.max_bulk_rows, limits.max_response_rows
    );
    Ok(HttpResponse::Ok()
        .insert_header(("content-type", "text/plain; version=0.0.4; charset=utf-8"))
        .body(body))
}

#[derive(Debug, Serialize)]
struct TablesResponse {
    revision: u64,
    tables: Vec<TableSummaryResponse>,
}

#[derive(Debug, Serialize)]
struct TableSummaryResponse {
    name: String,
    row_count: usize,
    column_count: usize,
    index_count: usize,
}

async fn tables(
    http_request: HttpRequest,
    security: Option<web::Data<ApiSecurity>>,
    limiter: Option<web::Data<DatabaseTaskLimiter>>,
    database: web::Data<Database>,
) -> Result<web::Json<TablesResponse>, ApiError> {
    authorize(&http_request, security.as_ref().map(|data| data.get_ref()))?;
    let database = database.get_ref().clone();
    let (revision, tables) = run_database_task(limiter.as_ref(), move || {
        let revision = database.revision()?;
        let tables = database.table_info()?;
        Ok::<_, Error>((revision, tables))
    })
    .await?;
    Ok(web::Json(TablesResponse {
        revision,
        tables: tables
            .into_iter()
            .map(|table| TableSummaryResponse {
                name: table.name,
                row_count: table.row_count,
                column_count: table.column_count,
                index_count: table.index_count,
            })
            .collect(),
    }))
}

/// Request body accepted by `POST /v1/sql`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SqlRequest {
    pub sql: String,
    /// Optional positional `$1`, `$2`, ... values, including numeric vectors.
    #[serde(default)]
    pub parameters: Option<Vec<JsonValue>>,
}

/// Response returned by SQL and ingestion endpoints.
#[derive(Debug, Serialize)]
pub struct SqlResponse {
    pub results: Vec<ApiExecutionResult>,
}

/// JSON representation of an engine execution result.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApiExecutionResult {
    Query {
        columns: Vec<String>,
        schema: Vec<ApiResultColumn>,
        rows: Vec<Vec<JsonValue>>,
        row_count: usize,
        rows_examined: usize,
    },
    Command {
        tag: &'static str,
        rows_affected: usize,
    },
}

/// Name and declared SQL type of one query result column.
#[derive(Debug, Serialize)]
pub struct ApiResultColumn {
    pub name: String,
    pub data_type: Option<String>,
}

async fn execute_sql(
    http_request: HttpRequest,
    security: Option<web::Data<ApiSecurity>>,
    limiter: Option<web::Data<DatabaseTaskLimiter>>,
    limits: web::Data<RequestLimits>,
    database: web::Data<Database>,
    request: web::Json<SqlRequest>,
) -> Result<HttpResponse, ApiError> {
    authorize(&http_request, security.as_ref().map(|data| data.get_ref()))?;
    if request.sql.trim().is_empty() {
        return Err(ApiError::bad_request("empty_sql", "SQL cannot be empty"));
    }
    let request = request.into_inner();
    let max_response_rows = limits.max_response_rows;
    let database = database.get_ref().clone();
    let body = run_database_task(limiter.as_ref(), move || {
        let sql = parameters::bind(request)?;
        let results = database.execute_with_row_limit(&sql, max_response_rows)?;
        enforce_response_row_limit(&results, max_response_rows)?;
        response::encode_sql(&results)
    })
    .await?;
    Ok(HttpResponse::Ok()
        .content_type("application/json")
        .body(body))
}

#[derive(Debug, Serialize)]
struct QueryIntentResponse {
    operation: &'static str,
    table: Option<String>,
    columns: Vec<QueryIntentColumnResponse>,
    distinct: bool,
    aggregation: bool,
    filter: Option<String>,
    group_by: Vec<String>,
    having: Option<String>,
    order_by: Vec<String>,
    limit: Option<usize>,
    offset: usize,
    vector_search: Option<VectorQueryIntentResponse>,
    summary: String,
}

#[derive(Debug, Serialize)]
struct QueryIntentColumnResponse {
    output_name: String,
    source_column: Option<String>,
    data_type: Option<String>,
    role: String,
}

#[derive(Debug, Serialize)]
struct VectorQueryIntentResponse {
    metric: String,
    column: String,
    dimensions: usize,
    descending: bool,
    optimized: bool,
}

async fn query_intent(
    http_request: HttpRequest,
    security: Option<web::Data<ApiSecurity>>,
    limiter: Option<web::Data<DatabaseTaskLimiter>>,
    database: web::Data<Database>,
    request: web::Json<SqlRequest>,
) -> Result<web::Json<QueryIntentResponse>, ApiError> {
    authorize(&http_request, security.as_ref().map(|data| data.get_ref()))?;
    if request.sql.trim().is_empty() {
        return Err(ApiError::bad_request("empty_sql", "SQL cannot be empty"));
    }
    let request = request.into_inner();
    let database = database.get_ref().clone();
    let intent = run_database_task(limiter.as_ref(), move || {
        let sql = parameters::bind(request)?;
        database.query_intent(&sql).map_err(ApiError::from)
    })
    .await?;
    Ok(web::Json(QueryIntentResponse::from(intent)))
}

impl From<QueryIntent> for QueryIntentResponse {
    fn from(intent: QueryIntent) -> Self {
        Self {
            operation: "select",
            table: intent.table,
            columns: intent
                .columns
                .into_iter()
                .map(|column| QueryIntentColumnResponse {
                    output_name: column.output_name,
                    source_column: column.source_column,
                    data_type: column.data_type.map(|data_type| data_type.to_string()),
                    role: column.role.to_string(),
                })
                .collect(),
            distinct: intent.distinct,
            aggregation: intent.aggregation,
            filter: intent.filter,
            group_by: intent.group_by,
            having: intent.having,
            order_by: intent.order_by,
            limit: intent.limit,
            offset: intent.offset,
            vector_search: intent
                .vector_search
                .map(|vector| VectorQueryIntentResponse {
                    metric: vector.metric,
                    column: vector.column,
                    dimensions: vector.dimensions,
                    descending: vector.descending,
                    optimized: vector.optimized,
                }),
            summary: intent.summary,
        }
    }
}

#[derive(Debug, Serialize)]
struct SchemaResponse {
    table: String,
    columns: Vec<ColumnResponse>,
}

#[derive(Debug, Serialize)]
struct ColumnResponse {
    name: String,
    data_type: String,
    nullable: bool,
    unique: bool,
}

async fn table_schema(
    http_request: HttpRequest,
    security: Option<web::Data<ApiSecurity>>,
    limiter: Option<web::Data<DatabaseTaskLimiter>>,
    database: web::Data<Database>,
    table: web::Path<String>,
) -> Result<web::Json<SchemaResponse>, ApiError> {
    authorize(&http_request, security.as_ref().map(|data| data.get_ref()))?;
    let table = table.into_inner();
    let lookup = table.clone();
    let database = database.get_ref().clone();
    let columns = run_database_task(limiter.as_ref(), move || database.schema(&lookup)).await?;
    Ok(web::Json(SchemaResponse {
        table,
        columns: columns
            .into_iter()
            .map(|column| ColumnResponse {
                name: column.name,
                data_type: column.data_type.to_string(),
                nullable: column.nullable,
                unique: column.unique,
            })
            .collect(),
    }))
}

#[derive(Debug, Serialize)]
struct IndexesResponse {
    table: String,
    indexes: Vec<IndexResponse>,
}

#[derive(Debug, Serialize)]
struct IndexResponse {
    name: String,
    column: String,
}

async fn table_indexes(
    http_request: HttpRequest,
    security: Option<web::Data<ApiSecurity>>,
    limiter: Option<web::Data<DatabaseTaskLimiter>>,
    database: web::Data<Database>,
    table: web::Path<String>,
) -> Result<web::Json<IndexesResponse>, ApiError> {
    authorize(&http_request, security.as_ref().map(|data| data.get_ref()))?;
    let table = table.into_inner();
    let lookup = table.clone();
    let database = database.get_ref().clone();
    let indexes = run_database_task(limiter.as_ref(), move || database.indexes(&lookup)).await?;
    Ok(web::Json(IndexesResponse {
        table,
        indexes: indexes
            .into_iter()
            .map(|index| IndexResponse {
                name: index.name,
                column: index.column,
            })
            .collect(),
    }))
}

/// Request body for typed bulk ingestion.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InsertRowsRequest {
    pub rows: Vec<Map<String, JsonValue>>,
    /// Normalize every non-null vector value before insertion.
    #[serde(default)]
    pub normalize_vectors: bool,
    /// Policy used when any unique constraint conflicts with an input row.
    #[serde(default)]
    pub on_conflict: InsertConflictPolicy,
    /// Unique column used to identify a conflicting row.
    #[serde(default)]
    pub conflict_target: Option<String>,
    /// Columns replaced from the incoming row when `on_conflict` is `do_update`.
    #[serde(default)]
    pub update_columns: Vec<String>,
}

/// Conflict behavior for typed bulk ingestion.
#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InsertConflictPolicy {
    #[default]
    Fail,
    DoNothing,
    DoUpdate,
}

async fn insert_rows(
    http_request: HttpRequest,
    security: Option<web::Data<ApiSecurity>>,
    limiter: Option<web::Data<DatabaseTaskLimiter>>,
    limits: web::Data<RequestLimits>,
    database: web::Data<Database>,
    table: web::Path<String>,
    request: web::Json<InsertRowsRequest>,
) -> Result<web::Json<SqlResponse>, ApiError> {
    authorize(&http_request, security.as_ref().map(|data| data.get_ref()))?;
    let table = table.into_inner();
    if request.rows.is_empty() {
        return Err(ApiError::bad_request(
            "empty_rows",
            "at least one row is required",
        ));
    }
    if request.rows.len() > limits.max_bulk_rows {
        return Err(ApiError::bad_request(
            "too_many_rows",
            format!(
                "a request may contain at most {} rows; split larger imports into batches",
                limits.max_bulk_rows
            ),
        ));
    }
    let request = request.into_inner();
    let database = database.get_ref().clone();
    let response = run_database_task(limiter.as_ref(), move || {
        let schema = database.schema(&table)?;
        let rows = ingest::build_insert_values(&schema, request.rows, request.normalize_vectors)?;
        let conflict = build_insert_conflict(
            &schema,
            request.on_conflict,
            request.conflict_target.as_deref(),
            &request.update_columns,
        )?;
        let rows_affected = database.insert_rows_if_schema(&table, &schema, rows, conflict)?;
        Ok::<_, ApiError>(SqlResponse {
            results: vec![ApiExecutionResult::Command {
                tag: "INSERT",
                rows_affected,
            }],
        })
    })
    .await?;
    Ok(web::Json(response))
}

/// Distance metric accepted by the structured search endpoint.
#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchMetric {
    #[default]
    Cosine,
    L2,
    SquaredL2,
    DotProduct,
}

/// Scalar comparison operator used by a search filter.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterOperator {
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
}

/// One scalar predicate in a structured vector search.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchFilter {
    pub column: String,
    pub operator: FilterOperator,
    pub value: JsonValue,
}

/// Request body accepted by `POST /v1/vector/search`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VectorSearchRequest {
    pub table: String,
    pub vector_column: String,
    pub query: Vec<f32>,
    #[serde(default)]
    pub metric: SearchMetric,
    #[serde(default)]
    pub select: Vec<String>,
    #[serde(default)]
    pub filters: Vec<SearchFilter>,
    #[serde(default = "default_search_limit")]
    pub limit: usize,
}

async fn vector_search(
    http_request: HttpRequest,
    security: Option<web::Data<ApiSecurity>>,
    limiter: Option<web::Data<DatabaseTaskLimiter>>,
    limits: web::Data<RequestLimits>,
    database: web::Data<Database>,
    request: web::Json<VectorSearchRequest>,
) -> Result<HttpResponse, ApiError> {
    authorize(&http_request, security.as_ref().map(|data| data.get_ref()))?;
    let request = request.into_inner();
    let maximum = MAX_SEARCH_LIMIT.min(limits.max_response_rows);
    if request.limit == 0 || request.limit > maximum {
        return Err(ApiError::bad_request(
            "invalid_limit",
            format!("limit must be between 1 and {maximum}"),
        ));
    }
    let database = database.get_ref().clone();
    let body = run_database_task(limiter.as_ref(), move || {
        let request = typed_search(request)?;
        let result = database.search_vectors(request).map_err(search_error)?;
        response::encode_query(&result)
    })
    .await?;
    Ok(HttpResponse::Ok()
        .content_type("application/json")
        .body(body))
}

fn typed_search(request: VectorSearchRequest) -> Result<VectorSearch, ApiError> {
    let filters = request
        .filters
        .into_iter()
        .map(|filter| {
            let invalid = || {
                ApiError::bad_request(
                    "invalid_value",
                    format!(
                        "filter for column '{}' must be a scalar value",
                        filter.column
                    ),
                )
            };
            let value = match filter.value {
                JsonValue::Null => {
                    if !matches!(filter.operator, FilterOperator::Eq | FilterOperator::Ne) {
                        return Err(ApiError::bad_request(
                            "invalid_null_filter",
                            "NULL filters only support eq and ne",
                        ));
                    }
                    Value::Null
                }
                JsonValue::Bool(value) => Value::Boolean(value),
                JsonValue::Number(value) => match value.as_i64() {
                    Some(value) => Value::Integer(value),
                    None => Value::Float(
                        value
                            .as_f64()
                            .filter(|value| value.is_finite())
                            .ok_or_else(invalid)?,
                    ),
                },
                JsonValue::String(value) => Value::Text(value),
                JsonValue::Array(_) | JsonValue::Object(_) => return Err(invalid()),
            };
            Ok(VectorSearchFilter {
                column: filter.column,
                operator: match filter.operator {
                    FilterOperator::Eq => VectorFilterOperator::Eq,
                    FilterOperator::Ne => VectorFilterOperator::Ne,
                    FilterOperator::Gt => VectorFilterOperator::Gt,
                    FilterOperator::Gte => VectorFilterOperator::Gte,
                    FilterOperator::Lt => VectorFilterOperator::Lt,
                    FilterOperator::Lte => VectorFilterOperator::Lte,
                },
                value,
            })
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    Ok(VectorSearch {
        table: request.table,
        vector_column: request.vector_column,
        query: Vector::new(request.query)?,
        metric: match request.metric {
            SearchMetric::Cosine => VectorSearchMetric::Cosine,
            SearchMetric::L2 => VectorSearchMetric::L2,
            SearchMetric::SquaredL2 => VectorSearchMetric::SquaredL2,
            SearchMetric::DotProduct => VectorSearchMetric::DotProduct,
        },
        select: request.select,
        filters,
        limit: request.limit,
    })
}

fn search_error(error: Error) -> ApiError {
    match error {
        Error::ColumnNotFound(ref name) => {
            ApiError::bad_request("unknown_column", format!("column '{name}' does not exist"))
        }
        Error::DuplicateColumn(ref name) => ApiError::bad_request(
            "duplicate_column",
            format!("column '{name}' appears more than once"),
        ),
        Error::InvalidFilterColumn(_) => {
            ApiError::bad_request("invalid_filter_column", error.to_string())
        }
        Error::TypeMismatch { ref expected, .. } if expected == "VECTOR" => {
            ApiError::bad_request("not_a_vector", error.to_string())
        }
        Error::TypeMismatch { .. } => ApiError::bad_request("invalid_value", error.to_string()),
        _ => ApiError::from(error),
    }
}

fn build_insert_conflict(
    schema: &[Column],
    policy: InsertConflictPolicy,
    conflict_target: Option<&str>,
    update_columns: &[String],
) -> Result<InsertConflict, ApiError> {
    if matches!(policy, InsertConflictPolicy::Fail)
        && (conflict_target.is_some() || !update_columns.is_empty())
    {
        return Err(ApiError::bad_request(
            "invalid_conflict_options",
            "conflict_target and update_columns require a non-fail conflict policy",
        ));
    }
    if matches!(policy, InsertConflictPolicy::DoNothing) && !update_columns.is_empty() {
        return Err(ApiError::bad_request(
            "invalid_conflict_options",
            "update_columns can only be used with do_update",
        ));
    }

    let target = conflict_target
        .map(|name| resolve_column(schema, name))
        .transpose()?;
    if let Some(target) = target {
        if !target.unique {
            return Err(ApiError::bad_request(
                "invalid_conflict_target",
                format!("column '{}' is not unique", target.name),
            ));
        }
    }

    match policy {
        InsertConflictPolicy::Fail => Ok(InsertConflict::Fail),
        InsertConflictPolicy::DoNothing => Ok(InsertConflict::DoNothing {
            target: target.map(|column| column.name.clone()),
        }),
        InsertConflictPolicy::DoUpdate => {
            let target = target.ok_or_else(|| {
                ApiError::bad_request(
                    "missing_conflict_target",
                    "conflict_target is required for do_update",
                )
            })?;
            if update_columns.is_empty() {
                return Err(ApiError::bad_request(
                    "missing_update_columns",
                    "at least one update column is required for do_update",
                ));
            }
            let mut seen = std::collections::HashSet::new();
            let update_columns = update_columns
                .iter()
                .map(|name| {
                    let column = resolve_column(schema, name)?;
                    if !seen.insert(column.name.to_ascii_lowercase()) {
                        return Err(ApiError::bad_request(
                            "duplicate_column",
                            format!("update column '{}' appears more than once", column.name),
                        ));
                    }
                    Ok(column.name.clone())
                })
                .collect::<Result<Vec<_>, ApiError>>()?;
            Ok(InsertConflict::DoUpdate {
                target: target.name.clone(),
                update_columns,
            })
        }
    }
}

fn resolve_column<'a>(schema: &'a [Column], name: &str) -> Result<&'a Column, ApiError> {
    schema
        .iter()
        .find(|column| column.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| {
            ApiError::bad_request("unknown_column", format!("column '{name}' does not exist"))
        })
}

fn json_literal(
    value: &JsonValue,
    data_type: &DataType,
    column: &str,
    normalize_vector: bool,
) -> Result<String, ApiError> {
    let value = json_typed_value(value, data_type, column, normalize_vector)?;
    Ok(match value {
        Value::Null => "NULL".into(),
        Value::Integer(value) => value.to_string(),
        Value::Float(value) => value.to_string(),
        Value::Text(value) => format!("'{}'", value.replace('\'', "''")),
        Value::Boolean(value) => {
            if value {
                "TRUE".into()
            } else {
                "FALSE".into()
            }
        }
        Value::Vector(vector) => format!(
            "ARRAY[{}]",
            vector
                .as_slice()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    })
}

fn json_typed_value(
    value: &JsonValue,
    data_type: &DataType,
    column: &str,
    normalize_vector: bool,
) -> Result<Value, ApiError> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    let invalid = || {
        ApiError::bad_request(
            "invalid_value",
            format!("value for column '{column}' must be {data_type}"),
        )
    };
    match data_type {
        DataType::Integer => value.as_i64().map(Value::Integer).ok_or_else(invalid),
        DataType::Float => value
            .as_f64()
            .filter(|value| value.is_finite())
            .map(Value::Float)
            .ok_or_else(invalid),
        DataType::Text => value
            .as_str()
            .map(|value| Value::Text(value.into()))
            .ok_or_else(invalid),
        DataType::Boolean => value.as_bool().map(Value::Boolean).ok_or_else(invalid),
        DataType::Vector(dimensions) => {
            let values = value.as_array().ok_or_else(invalid)?;
            if values.len() != *dimensions {
                return Err(ApiError::bad_request(
                    "dimension_mismatch",
                    format!(
                        "column '{column}' expects {dimensions} dimensions, received {}",
                        values.len()
                    ),
                ));
            }
            let values = values
                .iter()
                .map(|value| {
                    value
                        .as_f64()
                        .filter(|value| (*value as f32).is_finite())
                        .map(|value| value as f32)
                        .ok_or_else(invalid)
                })
                .collect::<Result<Vec<_>, _>>()?;
            let vector = Vector::new(values).map_err(ApiError::from)?;
            let vector = if normalize_vector {
                vector.normalized().map_err(ApiError::from)?
            } else {
                vector
            };
            Ok(Value::Vector(vector))
        }
    }
}

fn quote_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn default_search_limit() -> usize {
    10
}

fn authorize(request: &HttpRequest, security: Option<&ApiSecurity>) -> Result<(), ApiError> {
    let Some(security) = security else {
        return Ok(());
    };
    let supplied = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|header| header.to_str().ok())
        .and_then(|header| header.strip_prefix("Bearer "));
    if supplied.is_some_and(|token| constant_time_eq(token, &security.bearer_token)) {
        Ok(())
    } else {
        Err(ApiError::unauthorized())
    }
}

// A blocking task can outlive a cancelled request. Its closure must own the
// permit so queued and running work retain capacity until completion or unwind.
async fn run_database_task<F, T, E>(
    limiter: Option<&web::Data<DatabaseTaskLimiter>>,
    task: F,
) -> Result<T, ApiError>
where
    F: FnOnce() -> Result<T, E> + Send + 'static,
    T: Send + 'static,
    E: Into<ApiError> + Send + 'static,
{
    let permit = limiter.map(|limiter| limiter.acquire()).transpose()?;
    web::block(move || {
        let _permit = permit;
        task()
    })
    .await
    .map_err(ApiError::from_blocking)?
    .map_err(Into::into)
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.as_bytes()
        .iter()
        .zip(right.as_bytes())
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn enforce_response_row_limit(
    results: &[ExecutionResult],
    max_response_rows: usize,
) -> Result<(), ApiError> {
    let row_count = results.iter().fold(0_usize, |total, result| match result {
        ExecutionResult::Query(result) => total.saturating_add(result.row_count()),
        ExecutionResult::Command { .. } => total,
    });
    if row_count > max_response_rows {
        return Err(ApiError::response_too_large(row_count, max_response_rows));
    }
    Ok(())
}

impl From<Vec<ExecutionResult>> for SqlResponse {
    fn from(results: Vec<ExecutionResult>) -> Self {
        Self {
            results: results.into_iter().map(ApiExecutionResult::from).collect(),
        }
    }
}

impl From<ExecutionResult> for ApiExecutionResult {
    fn from(result: ExecutionResult) -> Self {
        match result {
            ExecutionResult::Query(result) => Self::from(result),
            ExecutionResult::Command { tag, rows_affected } => Self::Command { tag, rows_affected },
        }
    }
}

impl From<QueryResult> for ApiExecutionResult {
    fn from(result: QueryResult) -> Self {
        let row_count = result.row_count();
        let schema = result
            .columns
            .iter()
            .cloned()
            .zip(result.column_types.iter())
            .map(|(name, data_type)| ApiResultColumn {
                name,
                data_type: data_type.as_ref().map(ToString::to_string),
            })
            .collect();
        Self::Query {
            columns: result.columns,
            schema,
            rows: result
                .rows
                .into_iter()
                .map(|row| row.into_iter().map(json_value).collect())
                .collect(),
            row_count,
            rows_examined: result.rows_examined,
        }
    }
}

fn json_value(value: Value) -> JsonValue {
    match value {
        Value::Null => JsonValue::Null,
        Value::Integer(value) => JsonValue::Number(value.into()),
        Value::Float(value) => Number::from_f64(value)
            .map(JsonValue::Number)
            .unwrap_or(JsonValue::Null),
        Value::Text(value) => JsonValue::String(value),
        Value::Boolean(value) => JsonValue::Bool(value),
        Value::Vector(value) => JsonValue::Array(
            value
                .as_slice()
                .iter()
                .map(|value| {
                    Number::from_f64(f64::from(*value))
                        .map(JsonValue::Number)
                        .unwrap_or(JsonValue::Null)
                })
                .collect(),
        ),
    }
}

#[derive(Debug, Serialize)]
struct ApiErrorBody {
    error: ApiErrorDetails,
}

#[derive(Debug, Serialize)]
struct ApiErrorDetails {
    code: &'static str,
    message: String,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn bad_request(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal_error",
            message: message.into(),
        }
    }

    fn unauthorized() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            code: "unauthorized",
            message: "a valid bearer token is required".into(),
        }
    }

    fn overloaded() -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "overloaded",
            message: "database task capacity is exhausted; retry later".into(),
        }
    }

    fn response_too_large(row_count: usize, limit: usize) -> Self {
        Self {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            code: "response_too_large",
            message: format!(
                "query produced {row_count} rows, exceeding the {limit}-row HTTP response limit; add LIMIT or split the query"
            ),
        }
    }

    fn from_blocking(error: BlockingError) -> Self {
        Self::internal(format!("database worker failed: {error}"))
    }
}

impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        if let Error::ResultLimitExceeded {
            found_at_least,
            max,
        } = &error
        {
            return Self::response_too_large(*found_at_least, *max);
        }
        let (status, code) = match error {
            Error::TableNotFound(_) | Error::ColumnNotFound(_) | Error::IndexNotFound(_) => {
                (StatusCode::NOT_FOUND, "not_found")
            }
            Error::TableAlreadyExists(_)
            | Error::IndexAlreadyExists(_)
            | Error::UniqueViolation(_)
            | Error::NullViolation(_) => (StatusCode::CONFLICT, "constraint_violation"),
            Error::RevisionConflict { .. } => (StatusCode::CONFLICT, "stale_revision"),
            Error::SchemaChanged { .. } => (StatusCode::CONFLICT, "schema_changed"),
            Error::ResultLimitExceeded { .. } => {
                (StatusCode::UNPROCESSABLE_ENTITY, "response_too_large")
            }
            Error::TableRowLimit { .. } => (StatusCode::UNPROCESSABLE_ENTITY, "table_too_large"),
            Error::LockPoisoned
            | Error::StorageIo(_)
            | Error::StorageBusy(_)
            | Error::CorruptSnapshot(_)
            | Error::CorruptWal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
            Error::Unsupported(_) => (StatusCode::NOT_IMPLEMENTED, "unsupported_sql"),
            _ => (StatusCode::BAD_REQUEST, "invalid_request"),
        };
        Self {
            status,
            code,
            message: error.to_string(),
        }
    }
}

impl ResponseError for ApiError {
    fn status_code(&self) -> StatusCode {
        self.status
    }

    fn error_response(&self) -> HttpResponse {
        let mut response = HttpResponse::build(self.status);
        if self.status == StatusCode::UNAUTHORIZED {
            response.insert_header((WWW_AUTHENTICATE, "Bearer"));
        } else if self.status == StatusCode::SERVICE_UNAVAILABLE {
            response.insert_header((RETRY_AFTER, "1"));
        }
        response.json(ApiErrorBody {
            error: ApiErrorDetails {
                code: self.code,
                message: self.message.clone(),
            },
        })
    }
}

fn json_payload_error(error: JsonPayloadError, _: &actix_web::HttpRequest) -> actix_web::Error {
    let (status, code, message) = match &error {
        JsonPayloadError::OverflowKnownLength { limit, .. }
        | JsonPayloadError::Overflow { limit } => (
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            format!(
                "JSON request body exceeds the configured {limit}-byte limit; split the request into smaller batches"
            ),
        ),
        _ => (
            StatusCode::BAD_REQUEST,
            "invalid_json",
            error.to_string(),
        ),
    };
    let response = HttpResponse::build(status).json(ApiErrorBody {
        error: ApiErrorDetails { code, message },
    });
    InternalError::from_response(error, response).into()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;

    use super::*;

    static SHUTDOWN_PATH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn shutdown_test_path(label: &str) -> PathBuf {
        let sequence = SHUTDOWN_PATH_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "vectors-{label}-{}-{sequence}.request",
            std::process::id()
        ))
    }

    #[test]
    fn console_validator_changes_when_same_length_content_changes() {
        let original = ConsoleAsset::new("old content", "text/plain");
        let identical = ConsoleAsset::new("old content", "text/plain");
        let updated = ConsoleAsset::new("new content", "text/plain");
        assert!(original.etag().weak_eq(identical.etag()));
        assert!(!original.etag().weak_eq(updated.etag()));
    }

    #[test]
    fn validates_server_capacity_and_releases_database_permits() {
        let config = ServerConfig::default();
        assert!(config.validate().is_ok());
        assert!(RequestLimits::new(0, 1, 1).is_err());
        assert!(RequestLimits::new(MAX_JSON_PAYLOAD_BYTES + 1, 1, 1).is_err());
        assert!(RequestLimits::new(1, MAX_BULK_ROWS + 1, 1).is_err());
        assert!(RequestLimits::new(1, 1, MAX_RESPONSE_ROWS + 1).is_err());
        let mut invalid = config.clone();
        invalid.max_concurrent_database_tasks = 0;
        assert_eq!(
            invalid.validate().unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );

        let mut invalid = config.clone();
        invalid.keep_alive = Duration::ZERO;
        assert_eq!(
            invalid.validate().unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );

        let mut invalid = config;
        invalid.shutdown_timeout = Duration::from_millis(999);
        assert_eq!(
            invalid.validate().unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );

        let limiter = DatabaseTaskLimiter::new(1);
        let permit = limiter.acquire().unwrap();
        let error = limiter.acquire().unwrap_err();
        assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
        let response = error.error_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers().get(RETRY_AFTER).unwrap(), "1");
        assert_eq!(limiter.state.rejected.load(Ordering::Relaxed), 1);
        drop(permit);
        assert!(limiter.acquire().is_ok());
    }

    #[actix_web::test]
    async fn cancelled_database_request_retains_capacity_until_worker_finishes() {
        let limiter = web::Data::new(DatabaseTaskLimiter::new(1));
        let worker_limiter = limiter.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let request = actix_web::rt::spawn(async move {
            run_database_task(Some(&worker_limiter), move || {
                started_tx.send(()).unwrap();
                // Dropping the sender also releases the worker if an assertion fails.
                let _ = release_rx.recv_timeout(Duration::from_secs(5));
                Ok::<_, Error>(())
            })
            .await
        });
        web::block(move || started_rx.recv_timeout(Duration::from_secs(5)))
            .await
            .unwrap()
            .expect("database worker did not start");

        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        assert_eq!(limiter.state.in_flight.load(Ordering::Acquire), 1);
        let error = limiter.acquire().unwrap_err();
        assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            error.error_response().headers().get(RETRY_AFTER).unwrap(),
            "1"
        );

        release_tx.send(()).unwrap();
        actix_web::rt::time::timeout(Duration::from_secs(5), async {
            while limiter.state.in_flight.load(Ordering::Acquire) != 0 {
                actix_web::rt::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("finished database worker did not release its capacity");
        assert!(limiter.acquire().is_ok());
    }

    #[actix_web::test]
    async fn database_workers_release_capacity_on_success_error_and_panic() {
        let limiter = web::Data::new(DatabaseTaskLimiter::new(1));

        let result = run_database_task(Some(&limiter), || Ok::<_, Error>(42))
            .await
            .unwrap();
        assert_eq!(result, 42);
        assert_eq!(limiter.state.in_flight.load(Ordering::Acquire), 0);

        let error = run_database_task(Some(&limiter), || {
            Err::<(), _>(Error::TableNotFound("missing".into()))
        })
        .await
        .unwrap_err();
        assert_eq!(error.status, StatusCode::NOT_FOUND);
        assert_eq!(limiter.state.in_flight.load(Ordering::Acquire), 0);

        let error = run_database_task(Some(&limiter), || -> Result<(), Error> {
            panic!("injected database worker failure");
        })
        .await
        .unwrap_err();
        assert_eq!(error.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(limiter.state.in_flight.load(Ordering::Acquire), 0);

        let _permit = limiter.acquire().unwrap();
        let error = run_database_task(Some(&limiter), || -> Result<(), Error> {
            panic!("an overloaded task must never run");
        })
        .await
        .unwrap_err();
        assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(limiter.state.in_flight.load(Ordering::Acquire), 1);
        assert_eq!(limiter.state.rejected.load(Ordering::Relaxed), 1);
    }

    #[actix_web::test]
    async fn all_database_routes_reject_overload_and_resume_after_capacity_is_released() {
        use actix_web::{http::Method, test};
        use serde_json::json;

        let database = Database::new();
        database
            .execute("CREATE TABLE documents (id INTEGER PRIMARY KEY, embedding VECTOR(3))")
            .unwrap();
        let limiter = web::Data::new(DatabaseTaskLimiter::new(1));
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(database))
                .app_data(limiter.clone())
                .configure(configure),
        )
        .await;
        let search = json!({
            "table": "documents",
            "vector_column": "embedding",
            "query": [1, 0, 0],
            "limit": 1
        });
        let routes = [
            (Method::GET, "/v1/tables", None),
            (Method::GET, "/v1/tables/documents/schema", None),
            (Method::GET, "/v1/tables/documents/indexes", None),
            (
                Method::POST,
                "/v1/sql",
                Some(json!({"sql": "SELECT id FROM documents"})),
            ),
            (
                Method::POST,
                "/v1/sql/intent",
                Some(json!({"sql": "SELECT id FROM documents"})),
            ),
            (
                Method::POST,
                "/v1/tables/documents/rows",
                Some(json!({"rows": [{"id": 1, "embedding": [1, 0, 0]}]})),
            ),
            (Method::POST, "/v1/vector/search", Some(search.clone())),
            (Method::POST, "/v1/embeddings/search", Some(search)),
        ];

        let mut permit = Some(limiter.acquire().unwrap());
        for overloaded in [true, false] {
            if !overloaded {
                drop(permit.take());
            }
            for (method, path, body) in &routes {
                let mut request = test::TestRequest::default()
                    .method(method.clone())
                    .uri(path);
                if let Some(body) = body {
                    request = request.set_json(body);
                }
                let response = test::call_service(&app, request.to_request()).await;
                if overloaded {
                    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{path}");
                    assert_eq!(response.headers().get(RETRY_AFTER).unwrap(), "1");
                    let body: JsonValue = test::read_body_json(response).await;
                    assert_eq!(body["error"]["code"], "overloaded");
                } else {
                    assert_eq!(response.status(), StatusCode::OK, "{path}");
                }
            }
            assert_eq!(
                limiter.state.in_flight.load(Ordering::Acquire),
                usize::from(overloaded)
            );
            let response =
                test::call_service(&app, test::TestRequest::get().uri("/metrics").to_request())
                    .await;
            assert_eq!(response.status(), StatusCode::OK);
            let body = test::read_body(response).await;
            let body = std::str::from_utf8(&body).unwrap();
            assert!(body.lines().any(|line| {
                line == format!(
                    "vectors_database_tasks_in_flight {}",
                    usize::from(overloaded)
                )
            }));
            assert!(body
                .lines()
                .any(|line| line == "vectors_database_tasks_rejected_total 8"));
        }
    }

    #[test]
    fn shutdown_file_preparation_is_safe_and_clears_stale_requests() {
        let path = shutdown_test_path("stale-shutdown");
        fs::write(&path, b"").unwrap();
        prepare_shutdown_file(&path).unwrap();
        assert!(!path.exists());

        let relative = PathBuf::from("vectors-relative-shutdown.request");
        assert_eq!(
            prepare_shutdown_file(&relative).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );

        let directory = shutdown_test_path("shutdown-directory");
        fs::create_dir(&directory).unwrap();
        assert_eq!(
            prepare_shutdown_file(&directory).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        fs::remove_dir(directory).unwrap();
    }

    #[actix_web::test]
    async fn shutdown_file_stops_a_running_server_and_is_consumed() {
        let path = shutdown_test_path("cooperative-shutdown");
        let config = ServerConfig {
            workers: 1,
            ..ServerConfig::default()
        };
        let server = actix_web::rt::spawn(serve_with_config_and_shutdown_file(
            Database::new(),
            "127.0.0.1:0",
            config,
            path.clone(),
        ));

        actix_web::rt::time::sleep(Duration::from_millis(150)).await;
        fs::write(&path, b"").unwrap();
        let result = actix_web::rt::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("server did not honor the shutdown request")
            .expect("server task panicked");

        assert!(result.is_ok());
        assert!(!path.exists());
    }
}
