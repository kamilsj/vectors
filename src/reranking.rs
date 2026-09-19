//! Optional server-side Voyage reranking with bounded requests and write-only credentials.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;

const ENDPOINT: &str = "https://api.voyageai.com/v1/rerank";
pub(crate) const MAX_CANDIDATES: usize = 100;
// Conservative UTF-8 byte admission budgets, not measured token counts. Reserve
// space for provider instructions; truncation:false is the final provider guard.
// https://docs.voyageai.com/reference/reranker-api
const PROMPT_BUDGET: usize = 128;
pub(crate) const MAX_QUERY_BYTES: usize = 8_000 - PROMPT_BUDGET;
const MAX_PAIR_BUDGET: usize = 32_000;
const MAX_TOTAL_BUDGET: usize = 600_000;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_CONFIG_BYTES: usize = 8 * 1024;
static CONFIG_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    model: String,
    timeout_seconds: u64,
    max_concurrent_requests: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: "rerank-2.5".into(),
            timeout_seconds: 60,
            max_concurrent_requests: 4,
        }
    }
}

impl Config {
    fn validate(&self) -> Result<(), RerankingError> {
        if !matches!(self.model.as_str(), "rerank-2.5" | "rerank-2.5-lite") {
            return Err(RerankingError::Invalid("unsupported reranking model"));
        }
        if !(1..=120).contains(&self.timeout_seconds) {
            return Err(RerankingError::Invalid(
                "timeout_seconds must be between 1 and 120",
            ));
        }
        if !(1..=16).contains(&self.max_concurrent_requests) {
            return Err(RerankingError::Invalid(
                "max_concurrent_requests must be between 1 and 16",
            ));
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct ModelInfo {
    id: &'static str,
}

#[derive(Serialize)]
pub(crate) struct SettingsResponse {
    provider: &'static str,
    #[serde(flatten)]
    config: Config,
    pub(crate) configured: bool,
    models: [ModelInfo; 2],
    persistence: &'static str,
}

// Deliberately neither Serialize nor Debug: an incoming key is write-only.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SettingsUpdate {
    pub(crate) model: Option<String>,
    pub(crate) timeout_seconds: Option<u64>,
    pub(crate) max_concurrent_requests: Option<usize>,
    pub(crate) api_key: Option<String>,
    #[serde(default)]
    pub(crate) clear_api_key: bool,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct RerankResult {
    pub(crate) index: usize,
    pub(crate) relevance_score: f64,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Usage {
    pub(crate) total_tokens: u64,
}

#[derive(Debug, Serialize)]
pub(crate) struct RerankResponse {
    pub(crate) provider: &'static str,
    pub(crate) model: String,
    pub(crate) results: Vec<RerankResult>,
    pub(crate) usage: Usage,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum RerankingError {
    #[error("{0}")]
    Invalid(&'static str),
    #[error("configure a Voyage API key before requesting reranking")]
    NotConfigured,
    #[error("reranking service is busy; try again later")]
    Busy,
    #[error("reranking provider rejected the configured API key")]
    Authentication,
    #[error("reranking provider rate limit reached")]
    RateLimited,
    #[error("reranking provider rejected the request; check input limits and settings")]
    Rejected,
    #[error("reranking provider request timed out")]
    Timeout,
    #[error("reranking provider is unavailable")]
    Unavailable,
    #[error("reranking provider returned an invalid response")]
    InvalidResponse,
    #[error("could not persist reranking settings")]
    Persistence,
    #[error("reranking service settings are unavailable")]
    Internal,
}

struct RuntimeState {
    config: Config,
    key: Option<String>,
}

struct Inner {
    client: reqwest::Client,
    state: RwLock<RuntimeState>,
    updates: Mutex<()>,
    in_flight: AtomicUsize,
    settings_in_flight: AtomicUsize,
    config_path: Option<PathBuf>,
    endpoint: String,
}

/// Optional Voyage cross-encoder service. Construct once and share its HTTP pool.
/// Credentials are held in memory and never written to the settings file.
#[derive(Clone)]
pub struct RerankingService {
    inner: Arc<Inner>,
}

pub(crate) struct SettingsUpdatePermit(Arc<Inner>);

impl Drop for SettingsUpdatePermit {
    fn drop(&mut self) {
        self.0.settings_in_flight.store(0, Ordering::Release);
    }
}

struct RequestPermit(Arc<Inner>);

impl Drop for RequestPermit {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

impl RerankingService {
    /// Load non-secret settings, and read the optional write-only VOYAGE_API_KEY.
    /// A missing key leaves reranking disabled until one is configured.
    pub fn from_environment(config_path: Option<PathBuf>) -> io::Result<Self> {
        let config = load_config(config_path.as_deref())?;
        let key = match std::env::var("VOYAGE_API_KEY") {
            Ok(key) => Some(key.trim().to_owned()).filter(|key| !key.is_empty()),
            Err(std::env::VarError::NotPresent) => None,
            Err(std::env::VarError::NotUnicode(_)) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "VOYAGE_API_KEY must be valid Unicode",
                ));
            }
        };
        if let Some(key) = &key {
            validate_key(key).map_err(io::Error::other)?;
        }
        Self::build(config, key, config_path, ENDPOINT.into(), true)
    }

    fn build(
        config: Config,
        key: Option<String>,
        config_path: Option<PathBuf>,
        endpoint: String,
        https_only: bool,
    ) -> io::Result<Self> {
        let builder = reqwest::Client::builder()
            .https_only(https_only)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(10))
            .pool_max_idle_per_host(16);
        #[cfg(test)]
        let builder = if !https_only {
            builder.no_proxy()
        } else {
            builder
        };
        let client = builder
            .build()
            .map_err(|_| io::Error::other("could not initialize reranking HTTP client"))?;
        Ok(Self {
            inner: Arc::new(Inner {
                client,
                state: RwLock::new(RuntimeState { config, key }),
                updates: Mutex::new(()),
                in_flight: AtomicUsize::new(0),
                settings_in_flight: AtomicUsize::new(0),
                config_path,
                endpoint,
            }),
        })
    }

    pub(crate) fn settings(&self) -> Result<SettingsResponse, RerankingError> {
        let state = self
            .inner
            .state
            .read()
            .map_err(|_| RerankingError::Internal)?;
        Ok(settings_response(&state, self.inner.config_path.is_some()))
    }

    /// Preflight explicit reranking before the caller starts other paid work.
    pub(crate) fn ensure_configured(&self) -> Result<(), RerankingError> {
        let state = self
            .inner
            .state
            .read()
            .map_err(|_| RerankingError::Internal)?;
        state
            .key
            .as_ref()
            .map(|_| ())
            .ok_or(RerankingError::NotConfigured)
    }

    pub(crate) fn acquire_settings_update(&self) -> Result<SettingsUpdatePermit, RerankingError> {
        self.inner
            .settings_in_flight
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| RerankingError::Busy)?;
        Ok(SettingsUpdatePermit(self.inner.clone()))
    }

    // The caller moves its admission permit into a blocking worker. An accepted
    // durable settings update then completes even if the HTTP client disconnects.
    pub(crate) fn update_settings(
        &self,
        update: SettingsUpdate,
    ) -> Result<SettingsResponse, RerankingError> {
        let _update = self
            .inner
            .updates
            .lock()
            .map_err(|_| RerankingError::Internal)?;
        let mut config = self
            .inner
            .state
            .read()
            .map_err(|_| RerankingError::Internal)?
            .config
            .clone();
        if let Some(model) = update.model {
            config.model = model;
        }
        if let Some(timeout) = update.timeout_seconds {
            config.timeout_seconds = timeout;
        }
        if let Some(maximum) = update.max_concurrent_requests {
            config.max_concurrent_requests = maximum;
        }
        config.validate()?;
        if update.api_key.is_some() && update.clear_api_key {
            return Err(RerankingError::Invalid(
                "provide api_key or clear_api_key, not both",
            ));
        }
        let key = update.api_key.map(|key| key.trim().to_owned());
        if let Some(key) = &key {
            validate_key(key)?;
        }
        if let Some(path) = &self.inner.config_path {
            persist_config(path, &config)?;
        }
        let mut state = self
            .inner
            .state
            .write()
            .map_err(|_| RerankingError::Internal)?;
        if let Some(key) = key {
            state.key = Some(key);
        } else if update.clear_api_key {
            state.key = None;
        }
        state.config = config;
        Ok(settings_response(&state, self.inner.config_path.is_some()))
    }

    pub(crate) async fn rerank(
        &self,
        query: String,
        documents: Vec<String>,
    ) -> Result<RerankResponse, RerankingError> {
        validate_input(&query, &documents)?;
        let (config, key) = {
            let state = self
                .inner
                .state
                .read()
                .map_err(|_| RerankingError::Internal)?;
            (
                state.config.clone(),
                state.key.clone().ok_or(RerankingError::NotConfigured)?,
            )
        };
        self.inner
            .in_flight
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current < config.max_concurrent_requests).then_some(current + 1)
            })
            .map_err(|_| RerankingError::Busy)?;
        let _permit = RequestPermit(self.inner.clone());
        tokio::time::timeout(
            Duration::from_secs(config.timeout_seconds),
            self.send_request(&config, &key, &query, &documents),
        )
        .await
        .map_err(|_| RerankingError::Timeout)?
    }

    async fn send_request(
        &self,
        config: &Config,
        key: &str,
        query: &str,
        documents: &[String],
    ) -> Result<RerankResponse, RerankingError> {
        let mut response = self
            .inner
            .client
            .post(&self.inner.endpoint)
            .bearer_auth(key)
            .json(&json!({
                "query": query, "documents": documents, "model": config.model,
                "top_k": documents.len(), "return_documents": false, "truncation": false,
            }))
            .send()
            .await
            .map_err(map_transport_error)?;
        let status = response.status();
        if !status.is_success() {
            return Err(match status.as_u16() {
                401 | 403 => RerankingError::Authentication,
                429 => RerankingError::RateLimited,
                400 | 413 | 422 => RerankingError::Rejected,
                _ => RerankingError::Unavailable,
            });
        }
        if response
            .content_length()
            .is_some_and(|bytes| bytes > MAX_RESPONSE_BYTES as u64)
        {
            return Err(RerankingError::InvalidResponse);
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(map_transport_error)? {
            if chunk.len() > MAX_RESPONSE_BYTES - body.len() {
                return Err(RerankingError::InvalidResponse);
            }
            body.extend_from_slice(&chunk);
        }
        parse_response(&body, documents.len(), &config.model)
    }
}

fn map_transport_error(error: reqwest::Error) -> RerankingError {
    if error.is_timeout() {
        RerankingError::Timeout
    } else {
        RerankingError::Unavailable
    }
}

fn validate_input(query: &str, documents: &[String]) -> Result<(), RerankingError> {
    if query.trim().is_empty() || query.len() > MAX_QUERY_BYTES || query.contains('\0') {
        return Err(RerankingError::Invalid(
            "query must contain nonempty text of at most 7872 UTF-8 bytes, without NUL characters",
        ));
    }
    if documents.is_empty() || documents.len() > MAX_CANDIDATES {
        return Err(RerankingError::Invalid(
            "reranking requires between 1 and 100 candidate documents",
        ));
    }
    let mut total = 0usize;
    for document in documents {
        if document.trim().is_empty() || document.contains('\0') {
            return Err(RerankingError::Invalid(
                "candidate documents must contain nonempty text without NUL characters",
            ));
        }
        let pair = query
            .len()
            .saturating_add(document.len())
            .saturating_add(PROMPT_BUDGET);
        if pair > MAX_PAIR_BUDGET {
            return Err(RerankingError::Invalid(
                "query and each candidate document exceed the reranking pair byte budget",
            ));
        }
        total = total.saturating_add(pair);
        if total > MAX_TOTAL_BUDGET {
            return Err(RerankingError::Invalid(
                "combined reranking input exceeds the total byte budget",
            ));
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct ProviderResponse {
    data: Vec<RerankResult>,
    usage: Usage,
    model: Option<String>,
}

fn parse_response(
    bytes: &[u8],
    count: usize,
    model: &str,
) -> Result<RerankResponse, RerankingError> {
    let mut response: ProviderResponse =
        serde_json::from_slice(bytes).map_err(|_| RerankingError::InvalidResponse)?;
    if response.data.len() != count || response.model.as_deref().is_some_and(|echo| echo != model) {
        return Err(RerankingError::InvalidResponse);
    }
    let mut seen = vec![false; count];
    for result in &mut response.data {
        if result.index >= count
            || seen[result.index]
            || !result.relevance_score.is_finite()
            || !(0.0..=1.0).contains(&result.relevance_score)
        {
            return Err(RerankingError::InvalidResponse);
        }
        seen[result.index] = true;
        if result.relevance_score == 0.0 {
            result.relevance_score = 0.0;
        }
    }
    response.data.sort_unstable_by(|a, b| {
        b.relevance_score
            .total_cmp(&a.relevance_score)
            .then_with(|| a.index.cmp(&b.index))
    });
    Ok(RerankResponse {
        provider: "voyage",
        model: model.into(),
        results: response.data,
        usage: response.usage,
    })
}

fn validate_key(key: &str) -> Result<(), RerankingError> {
    if key.is_empty() || key.len() > 4096 || reqwest::header::HeaderValue::from_str(key).is_err() {
        return Err(RerankingError::Invalid(
            "API key must be nonempty and contain valid HTTP header characters",
        ));
    }
    Ok(())
}

fn settings_response(state: &RuntimeState, durable: bool) -> SettingsResponse {
    SettingsResponse {
        provider: "voyage",
        config: state.config.clone(),
        configured: state.key.is_some(),
        models: [
            ModelInfo { id: "rerank-2.5" },
            ModelInfo {
                id: "rerank-2.5-lite",
            },
        ],
        persistence: if durable { "durable" } else { "memory" },
    }
}

fn load_config(path: Option<&Path>) -> io::Result<Config> {
    let config = if let Some(path) = path {
        match fs::File::open(path) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(MAX_CONFIG_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)?;
                if bytes.len() > MAX_CONFIG_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "reranking settings file is too large",
                    ));
                }
                serde_json::from_slice(&bytes).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid reranking settings file",
                    )
                })?
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Config::default(),
            Err(error) => return Err(error),
        }
    } else {
        Config::default()
    };
    config.validate().map_err(io::Error::other)?;
    Ok(config)
}

fn persist_config(path: &Path, config: &Config) -> Result<(), RerankingError> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let sequence = CONFIG_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".reranking-settings-{}-{sequence}.tmp",
        std::process::id()
    ));
    let result = (|| -> io::Result<()> {
        fs::create_dir_all(parent)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        serde_json::to_writer_pretty(&mut file, config).map_err(io::Error::other)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        #[cfg(unix)]
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(|_| RerankingError::Persistence)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::thread;

    pub(crate) fn test_service(path: Option<PathBuf>) -> RerankingService {
        RerankingService::build(
            Config::default(),
            None,
            path,
            "http://127.0.0.1:1".into(),
            false,
        )
        .unwrap()
    }

    pub(crate) fn configured(endpoint: &str) -> RerankingService {
        RerankingService::build(
            Config {
                max_concurrent_requests: 1,
                ..Config::default()
            },
            Some("synthetic-voyage-key".into()),
            None,
            endpoint.into(),
            false,
        )
        .unwrap()
    }

    pub(crate) fn mock_provider(
        count: usize,
        respond: impl Fn(usize, serde_json::Value, &str) -> (u16, serde_json::Value) + Send + 'static,
    ) -> (String, thread::JoinHandle<()>) {
        mock_raw(count, move |index, body, headers| {
            let (status, body) = respond(index, body, headers);
            http_response(status, &serde_json::to_vec(&body).unwrap())
        })
    }

    fn http_response(status: u16, body: &[u8]) -> Vec<u8> {
        let mut response = format!("HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
        response.extend_from_slice(body);
        response
    }

    fn mock_raw(
        count: usize,
        respond: impl Fn(usize, serde_json::Value, &str) -> Vec<u8> + Send + 'static,
    ) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}/v1/rerank", listener.local_addr().unwrap());
        let handle = thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(20);
            for index in 0..count {
                let stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            assert!(
                                std::time::Instant::now() < deadline,
                                "mock provider timed out on request {} of {count}",
                                index + 1
                            );
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("mock accept failed: {error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(stream);
                let mut headers = String::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    assert!(!line.is_empty());
                    headers.push_str(&line);
                }
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .map(str::to_owned)
                    })
                    .unwrap()
                    .parse()
                    .unwrap();
                let mut bytes = vec![0; length];
                reader.read_exact(&mut bytes).unwrap();
                let response = respond(index, serde_json::from_slice(&bytes).unwrap(), &headers);
                let _ = reader.into_inner().write_all(&response);
            }
        });
        (endpoint, handle)
    }

    fn update(value: serde_json::Value) -> SettingsUpdate {
        serde_json::from_value(value).unwrap()
    }

    fn valid_response() -> serde_json::Value {
        json!({"data":[{"index":0,"relevance_score":0.75}],"usage":{"total_tokens":12}})
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "vectors-reranking-test-{}-{}",
                std::process::id(),
                CONFIG_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn settings_are_nonsecret_and_restart_never_recovers_a_runtime_key() {
        let directory = TempDir::new();
        let path = directory.0.join("settings.json");
        let service = test_service(Some(path.clone()));
        assert!(matches!(
            service.ensure_configured(),
            Err(RerankingError::NotConfigured)
        ));
        let settings = service
            .update_settings(update(json!({
                "model":"rerank-2.5-lite", "timeout_seconds":23,
                "max_concurrent_requests":2, "api_key":"runtime-secret-only"
            })))
            .unwrap();
        service.ensure_configured().unwrap();
        let settings = serde_json::to_value(settings).unwrap();
        assert_eq!(settings["provider"], "voyage");
        assert_eq!(settings["configured"], true);
        assert_eq!(settings["persistence"], "durable");
        assert_eq!(
            settings["models"],
            json!([{"id":"rerank-2.5"},{"id":"rerank-2.5-lite"}])
        );
        let persisted = fs::read_to_string(&path).unwrap();
        assert!(!persisted.contains("secret"));
        assert!(!persisted.contains("key"));
        assert!(!settings.to_string().contains("secret"));
        assert!(!settings.to_string().contains("api_key"));
        let config = load_config(Some(&path)).unwrap();
        assert_eq!(config.model, "rerank-2.5-lite");
        assert_eq!(config.timeout_seconds, 23);
        assert_eq!(config.max_concurrent_requests, 2);
        let restarted = RerankingService::build(
            config,
            None,
            Some(path.clone()),
            "http://127.0.0.1:1".into(),
            false,
        )
        .unwrap();
        assert!(!restarted.settings().unwrap().configured);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        service
            .update_settings(update(json!({"clear_api_key":true})))
            .unwrap();
        assert!(matches!(
            service.ensure_configured(),
            Err(RerankingError::NotConfigured)
        ));
    }

    #[test]
    fn invalid_settings_leave_credentials_and_durable_configuration_unchanged() {
        let directory = TempDir::new();
        let path = directory.0.join("settings.json");
        let service = test_service(Some(path.clone()));
        service
            .update_settings(update(json!({"api_key":"original-key"})))
            .unwrap();
        let original = fs::read(&path).unwrap();
        for value in [
            json!({"model":"rerank-3"}),
            json!({"model":"other"}),
            json!({"timeout_seconds":0}),
            json!({"timeout_seconds":121}),
            json!({"max_concurrent_requests":0}),
            json!({"max_concurrent_requests":17}),
            json!({"api_key":"  "}),
            json!({"api_key":"bad\r\nkey"}),
            json!({"api_key":"x".repeat(4097)}),
            json!({"api_key":"new", "clear_api_key":true}),
        ] {
            assert!(matches!(
                service.update_settings(update(value)),
                Err(RerankingError::Invalid(_))
            ));
            assert_eq!(fs::read(&path).unwrap(), original);
            assert_eq!(
                service.inner.state.read().unwrap().key.as_deref(),
                Some("original-key")
            );
        }
        assert!(serde_json::from_value::<SettingsUpdate>(json!({"provider":"other"})).is_err());
        assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
    }

    #[test]
    fn settings_admission_and_failed_persistence_do_not_publish_changes() {
        let directory = TempDir::new();
        let occupied = directory.0.join("not-a-directory");
        fs::write(&occupied, "occupied").unwrap();
        let service = test_service(Some(occupied.join("settings.json")));
        let permit = service.acquire_settings_update().unwrap();
        assert!(matches!(
            service.acquire_settings_update(),
            Err(RerankingError::Busy)
        ));
        assert!(matches!(
            service.update_settings(update(
                json!({"model":"rerank-2.5-lite","api_key":"secret"})
            )),
            Err(RerankingError::Persistence)
        ));
        assert!(!service.settings().unwrap().configured);
        assert_eq!(
            service.inner.state.read().unwrap().config.model,
            "rerank-2.5"
        );
        drop(permit);
        assert!(service.acquire_settings_update().is_ok());
    }

    #[test]
    fn configuration_loading_rejects_oversized_secret_and_invalid_files() {
        let directory = TempDir::new();
        let path = directory.0.join("settings.json");
        for bytes in [
            vec![b' '; MAX_CONFIG_BYTES + 1],
            b"{\"api_key\":\"secret\"}".to_vec(),
            b"{\"timeout_seconds\":0}".to_vec(),
        ] {
            fs::write(&path, bytes).unwrap();
            assert!(load_config(Some(&path)).is_err());
        }
        fs::write(&path, "{}").unwrap();
        assert_eq!(load_config(Some(&path)).unwrap().model, "rerank-2.5");
    }

    #[actix_web::test]
    async fn provider_request_is_explicit_and_results_are_complete_sorted_and_indexed() {
        let (endpoint, mock) = mock_provider(1, |_, body, headers| {
            assert!(headers.starts_with("POST /v1/rerank HTTP/1.1"));
            assert!(headers.contains("Bearer synthetic-voyage-key"));
            assert_eq!(
                body,
                json!({"query":"question", "documents":["first","second","third"], "model":"rerank-2.5", "top_k":3,"return_documents":false,"truncation":false})
            );
            (
                200,
                json!({"model":"rerank-2.5", "data":[
                {"index":2,"relevance_score":0.9}, {"index":0,"relevance_score":0.2}, {"index":1,"relevance_score":0.9}
            ],"usage":{"total_tokens":42}}),
            )
        });
        let service = configured(&endpoint);
        let response = service
            .rerank(
                "question".into(),
                vec!["first".into(), "second".into(), "third".into()],
            )
            .await
            .unwrap();
        assert_eq!(
            response
                .results
                .iter()
                .map(|item| item.index)
                .collect::<Vec<_>>(),
            vec![1, 2, 0]
        );
        assert_eq!(response.model, "rerank-2.5");
        assert_eq!(response.provider, "voyage");
        assert_eq!(response.usage.total_tokens, 42);
        assert_eq!(service.inner.in_flight.load(Ordering::Acquire), 0);
        mock.join().unwrap();
    }

    #[actix_web::test]
    async fn every_invalid_input_is_rejected_before_admission_or_network() {
        let service = configured("http://127.0.0.1:1");
        let invalid = vec![
            (" ".into(), vec!["document".into()]),
            ("x".repeat(MAX_QUERY_BYTES + 1), vec!["document".into()]),
            ("q\0".into(), vec!["document".into()]),
            ("q".into(), vec![]),
            ("q".into(), vec!["d".into(); MAX_CANDIDATES + 1]),
            ("q".into(), vec!["\n\t".into()]),
            ("q".into(), vec!["d\0".into()]),
            (
                "q".into(),
                vec!["d".repeat(MAX_PAIR_BUDGET - PROMPT_BUDGET)],
            ),
            (
                "q".repeat(MAX_QUERY_BYTES),
                vec!["d".into(); MAX_CANDIDATES],
            ),
            ("q".into(), vec!["d".repeat(6_000); MAX_CANDIDATES]),
        ];
        for (query, documents) in invalid {
            assert!(matches!(
                service.rerank(query, documents).await,
                Err(RerankingError::Invalid(_))
            ));
            assert_eq!(service.inner.in_flight.load(Ordering::Acquire), 0);
        }
        assert!(matches!(
            test_service(None)
                .rerank("q".into(), vec!["d".into()])
                .await,
            Err(RerankingError::NotConfigured)
        ));
    }

    #[test]
    fn conservative_byte_budgets_accept_exact_boundaries_and_count_repeated_queries() {
        let query = "q".repeat(MAX_QUERY_BYTES);
        validate_input(&query, &["é".repeat(12_000)]).unwrap(); // 7,872 + 24,000 + 128
        assert!(validate_input(&query, &["é".repeat(12_001)]).is_err());
        let documents = vec!["d".repeat(5_871); MAX_CANDIDATES];
        validate_input("q", &documents).unwrap(); // 100 * (1 + 5,871 + 128)
        assert!(validate_input("qq", &documents).is_err());
        assert!(validate_input(&"é".repeat(MAX_QUERY_BYTES / 2 + 1), &["d".into()]).is_err());
    }

    #[actix_web::test]
    async fn malformed_indexes_scores_usage_and_model_are_rejected() {
        let invalid = vec![
            json!({"data":[],"usage":{"total_tokens":1}}),
            json!({"data":[{"index":0,"relevance_score":0.5},{"index":0,"relevance_score":0.6}],"usage":{"total_tokens":1}}),
            json!({"data":[{"index":1,"relevance_score":0.5}],"usage":{"total_tokens":1}}),
            json!({"data":[{"index":-1,"relevance_score":0.5}],"usage":{"total_tokens":1}}),
            json!({"data":[{"index":0,"relevance_score":-0.1}],"usage":{"total_tokens":1}}),
            json!({"data":[{"index":0,"relevance_score":1.1}],"usage":{"total_tokens":1}}),
            json!({"data":[{"index":0,"relevance_score":null}],"usage":{"total_tokens":1}}),
            json!({"data":[{"index":0,"relevance_score":0.5}]}),
            json!({"data":[{"index":0,"relevance_score":0.5}],"usage":{"total_tokens":-1}}),
            json!({"data":[{"index":0,"relevance_score":0.5}],"usage":{}}),
            json!({"data":[{"index":0,"relevance_score":0.5}],"usage":{"total_tokens":1},"model":"rerank-2.5-lite"}),
        ];
        let count = invalid.len();
        let (endpoint, mock) =
            mock_provider(count, move |index, _, _| (200, invalid[index].clone()));
        let service = configured(&endpoint);
        for _ in 0..count {
            assert!(matches!(
                service.rerank("q".into(), vec!["d".into()]).await,
                Err(RerankingError::InvalidResponse)
            ));
        }
        mock.join().unwrap();
        let malformed =
            b"{\"data\":[{\"index\":0,\"relevance_score\":NaN}],\"usage\":{\"total_tokens\":1}}";
        assert!(matches!(
            parse_response(malformed, 1, "rerank-2.5"),
            Err(RerankingError::InvalidResponse)
        ));
        let duplicates = br#"{"data":[{"index":0,"relevance_score":0.1},{"index":0,"relevance_score":0.2}],"usage":{"total_tokens":1}}"#;
        assert!(parse_response(duplicates, 2, "rerank-2.5").is_err());
    }

    #[actix_web::test]
    async fn provider_failures_are_sanitized_and_never_retried() {
        let statuses = [401, 403, 429, 400, 413, 422, 500, 503];
        let (endpoint, mock) = mock_provider(statuses.len(), move |index, _, _| {
            (
                statuses[index],
                json!({"error":"secret provider text synthetic-voyage-key"}),
            )
        });
        let service = configured(&endpoint);
        for status in statuses {
            let error = service
                .rerank("q".into(), vec!["d".into()])
                .await
                .unwrap_err();
            match status {
                401 | 403 => assert!(matches!(error, RerankingError::Authentication)),
                429 => assert!(matches!(error, RerankingError::RateLimited)),
                400 | 413 | 422 => assert!(matches!(error, RerankingError::Rejected)),
                _ => assert!(matches!(error, RerankingError::Unavailable)),
            }
            assert!(!error.to_string().contains("synthetic-voyage-key"));
            assert!(!error.to_string().contains("secret provider text"));
            assert_eq!(service.inner.in_flight.load(Ordering::Acquire), 0);
        }
        mock.join().unwrap();
    }

    #[actix_web::test]
    async fn oversized_content_length_and_chunked_responses_are_bounded() {
        let (endpoint, mock) = mock_raw(2, |index, _, _| {
            if index == 0 {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    MAX_RESPONSE_BYTES + 1
                )
                .into_bytes()
            } else {
                let body = vec![b' '; MAX_RESPONSE_BYTES + 1];
                let mut response = format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n", body.len()).into_bytes();
                response.extend(body);
                response.extend_from_slice(b"\r\n0\r\n\r\n");
                response
            }
        });
        let service = configured(&endpoint);
        for _ in 0..2 {
            assert!(matches!(
                service.rerank("q".into(), vec!["d".into()]).await,
                Err(RerankingError::InvalidResponse)
            ));
        }
        mock.join().unwrap();
    }

    #[actix_web::test]
    async fn redirects_are_not_followed_or_given_credentials() {
        let target = TcpListener::bind("127.0.0.1:0").unwrap();
        target.set_nonblocking(true).unwrap();
        let address = target.local_addr().unwrap();
        let (endpoint, mock) = mock_raw(1, move |_, _, _| {
            format!("HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{address}/stolen\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes()
        });
        let service = configured(&endpoint);
        assert!(matches!(
            service.rerank("q".into(), vec!["d".into()]).await,
            Err(RerankingError::Unavailable)
        ));
        mock.join().unwrap();
        assert_eq!(
            target.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[actix_web::test]
    async fn timeouts_release_capacity_without_automatic_retries() {
        let (endpoint, mock) = mock_provider(1, |_, _, _| {
            thread::sleep(Duration::from_millis(1_100));
            (200, valid_response())
        });
        let service = configured(&endpoint);
        service
            .update_settings(update(json!({"timeout_seconds":1})))
            .unwrap();
        assert!(matches!(
            service.rerank("q".into(), vec!["d".into()]).await,
            Err(RerankingError::Timeout)
        ));
        assert_eq!(service.inner.in_flight.load(Ordering::Acquire), 0);
        mock.join().unwrap();
    }

    #[actix_web::test]
    async fn cancelling_in_flight_request_releases_capacity_and_does_not_retry() {
        let started = Arc::new(AtomicUsize::new(0));
        let provider_started = started.clone();
        let (release, gate) = std::sync::mpsc::channel();
        let (endpoint, mock) = mock_provider(2, move |index, _, _| {
            provider_started.fetch_add(1, Ordering::Release);
            if index == 0 {
                gate.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            (200, valid_response())
        });
        let service = configured(&endpoint);
        let first_service = service.clone();
        let first = actix_web::rt::spawn(async move {
            first_service.rerank("q".into(), vec!["d".into()]).await
        });
        for _ in 0..500 {
            if started.load(Ordering::Acquire) == 1 {
                break;
            }
            actix_web::rt::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(started.load(Ordering::Acquire), 1);
        assert!(matches!(
            service.rerank("q".into(), vec!["d".into()]).await,
            Err(RerankingError::Busy)
        ));
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        assert_eq!(service.inner.in_flight.load(Ordering::Acquire), 0);
        release.send(()).unwrap();
        service.rerank("q".into(), vec!["d".into()]).await.unwrap();
        assert_eq!(started.load(Ordering::Acquire), 2);
        mock.join().unwrap();
    }

    #[actix_web::test]
    async fn a_settings_change_does_not_relabel_an_in_flight_model() {
        let started = Arc::new(AtomicUsize::new(0));
        let provider_started = started.clone();
        let (release, gate) = std::sync::mpsc::channel();
        let (endpoint, mock) = mock_provider(1, move |_, body, _| {
            assert_eq!(body["model"], "rerank-2.5");
            provider_started.store(1, Ordering::Release);
            gate.recv_timeout(Duration::from_secs(5)).unwrap();
            (200, valid_response())
        });
        let service = configured(&endpoint);
        let request_service = service.clone();
        let request = actix_web::rt::spawn(async move {
            request_service.rerank("q".into(), vec!["d".into()]).await
        });
        for _ in 0..500 {
            if started.load(Ordering::Acquire) == 1 {
                break;
            }
            actix_web::rt::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(started.load(Ordering::Acquire), 1);
        service
            .update_settings(update(
                json!({"model":"rerank-2.5-lite", "clear_api_key":true}),
            ))
            .unwrap();
        release.send(()).unwrap();
        assert_eq!(request.await.unwrap().unwrap().model, "rerank-2.5");
        assert!(matches!(
            service.ensure_configured(),
            Err(RerankingError::NotConfigured)
        ));
        mock.join().unwrap();
    }
}
