//! Actix Web interface for SQL execution and vector search.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;

use actix_web::error::{BlockingError, InternalError, JsonPayloadError};
use actix_web::http::header::{AUTHORIZATION, WWW_AUTHENTICATE};
use actix_web::http::StatusCode;
use actix_web::{web, App, HttpRequest, HttpResponse, HttpServer, ResponseError};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value as JsonValue};

use crate::{Column, DataType, Database, Error, ExecutionResult, QueryResult, Value, Vector};

const MAX_JSON_BYTES: usize = 16 * 1024 * 1024;
const MAX_BULK_ROWS: usize = 1_000;
const MAX_SEARCH_LIMIT: usize = 1_000;

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
    config
        .app_data(
            web::JsonConfig::default()
                .limit(MAX_JSON_BYTES)
                .error_handler(json_payload_error),
        )
        .route("/", web::get().to(console))
        .route("/assets/app.css", web::get().to(console_styles))
        .route("/assets/app.js", web::get().to(console_script))
        .route("/healthz", web::get().to(health))
        .service(
            web::scope("/v1")
                .route("/sql", web::post().to(execute_sql))
                .route("/tables", web::get().to(tables))
                .route("/tables/{table}/schema", web::get().to(table_schema))
                .route("/tables/{table}/indexes", web::get().to(table_indexes))
                .route("/tables/{table}/rows", web::post().to(insert_rows))
                .route("/embeddings/search", web::post().to(vector_search))
                .route("/vector/search", web::post().to(vector_search)),
        );
}

const CONSOLE_HTML: &str = include_str!("../web/index.html");
const CONSOLE_CSS: &str = include_str!("../web/app.css");
const CONSOLE_JS: &str = include_str!("../web/app.js");

async fn console() -> HttpResponse {
    console_asset(CONSOLE_HTML, "text/html; charset=utf-8")
}

async fn console_styles() -> HttpResponse {
    console_asset(CONSOLE_CSS, "text/css; charset=utf-8")
}

async fn console_script() -> HttpResponse {
    console_asset(CONSOLE_JS, "text/javascript; charset=utf-8")
}

fn console_asset(body: &'static str, content_type: &'static str) -> HttpResponse {
    HttpResponse::Ok()
        .insert_header(("content-type", content_type))
        .insert_header(("cache-control", "no-cache"))
        .insert_header(("x-content-type-options", "nosniff"))
        .insert_header(("referrer-policy", "no-referrer"))
        .insert_header((
            "content-security-policy",
            "default-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'",
        ))
        .body(body)
}

/// Start an HTTP server backed by the supplied database handle.
pub async fn serve(database: Database, bind_address: &str) -> io::Result<()> {
    serve_inner(database, bind_address, None).await
}

/// Start an HTTP server that requires a bearer token on every `/v1` route.
pub async fn serve_authenticated(
    database: Database,
    bind_address: &str,
    bearer_token: String,
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
    )
    .await
}

async fn serve_inner(
    database: Database,
    bind_address: &str,
    security: Option<ApiSecurity>,
) -> io::Result<()> {
    let database = web::Data::new(database);
    HttpServer::new(move || {
        let mut app = App::new().app_data(database.clone());
        if let Some(security) = security.clone() {
            app = app.app_data(web::Data::new(security));
        }
        app.configure(configure)
    })
    .bind(bind_address)?
    .run()
    .await
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
}

async fn health() -> web::Json<HealthResponse> {
    web::Json(HealthResponse { status: "ok" })
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
    database: web::Data<Database>,
) -> Result<web::Json<TablesResponse>, ApiError> {
    authorize(&http_request, security.as_ref().map(|data| data.get_ref()))?;
    let database = database.get_ref().clone();
    let (revision, tables) = web::block(move || {
        let revision = database.revision()?;
        let tables = database.table_info()?;
        Ok::<_, Error>((revision, tables))
    })
    .await
    .map_err(ApiError::from_blocking)??;
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
        rows: Vec<Vec<JsonValue>>,
        row_count: usize,
        rows_examined: usize,
    },
    Command {
        tag: &'static str,
        rows_affected: usize,
    },
}

async fn execute_sql(
    http_request: HttpRequest,
    security: Option<web::Data<ApiSecurity>>,
    database: web::Data<Database>,
    request: web::Json<SqlRequest>,
) -> Result<web::Json<SqlResponse>, ApiError> {
    authorize(&http_request, security.as_ref().map(|data| data.get_ref()))?;
    if request.sql.trim().is_empty() {
        return Err(ApiError::bad_request("empty_sql", "SQL cannot be empty"));
    }
    let sql = request.into_inner().sql;
    let database = database.get_ref().clone();
    let results = web::block(move || database.execute(&sql))
        .await
        .map_err(ApiError::from_blocking)??;
    Ok(web::Json(SqlResponse::from(results)))
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
    database: web::Data<Database>,
    table: web::Path<String>,
) -> Result<web::Json<SchemaResponse>, ApiError> {
    authorize(&http_request, security.as_ref().map(|data| data.get_ref()))?;
    let table = table.into_inner();
    let lookup = table.clone();
    let database = database.get_ref().clone();
    let columns = web::block(move || database.schema(&lookup))
        .await
        .map_err(ApiError::from_blocking)??;
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
    database: web::Data<Database>,
    table: web::Path<String>,
) -> Result<web::Json<IndexesResponse>, ApiError> {
    authorize(&http_request, security.as_ref().map(|data| data.get_ref()))?;
    let table = table.into_inner();
    let lookup = table.clone();
    let database = database.get_ref().clone();
    let indexes = web::block(move || database.indexes(&lookup))
        .await
        .map_err(ApiError::from_blocking)??;
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
    if request.rows.len() > MAX_BULK_ROWS {
        return Err(ApiError::bad_request(
            "too_many_rows",
            format!("a request may contain at most {MAX_BULK_ROWS} rows"),
        ));
    }

    let database_handle = database.get_ref().clone();
    let lookup = table.clone();
    let schema = web::block(move || database_handle.schema(&lookup))
        .await
        .map_err(ApiError::from_blocking)??;
    let sql = build_insert_sql(
        &table,
        &schema,
        &request.rows,
        request.normalize_vectors,
        request.on_conflict,
        request.conflict_target.as_deref(),
        &request.update_columns,
    )?;
    let database = database.get_ref().clone();
    let results = web::block(move || database.execute(&sql))
        .await
        .map_err(ApiError::from_blocking)??;
    Ok(web::Json(SqlResponse::from(results)))
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
    database: web::Data<Database>,
    request: web::Json<VectorSearchRequest>,
) -> Result<web::Json<ApiExecutionResult>, ApiError> {
    authorize(&http_request, security.as_ref().map(|data| data.get_ref()))?;
    let request = request.into_inner();
    if request.limit == 0 || request.limit > MAX_SEARCH_LIMIT {
        return Err(ApiError::bad_request(
            "invalid_limit",
            format!("limit must be between 1 and {MAX_SEARCH_LIMIT}"),
        ));
    }
    let database_handle = database.get_ref().clone();
    let table = request.table.clone();
    let schema = web::block(move || database_handle.schema(&table))
        .await
        .map_err(ApiError::from_blocking)??;
    let sql = build_search_sql(&request, &schema)?;
    let database = database.get_ref().clone();
    let mut results = web::block(move || database.execute(&sql))
        .await
        .map_err(ApiError::from_blocking)??;
    let result = results
        .pop()
        .ok_or_else(|| ApiError::internal("empty search execution result"))?;
    Ok(web::Json(ApiExecutionResult::from(result)))
}

fn build_insert_sql(
    table: &str,
    schema: &[Column],
    rows: &[Map<String, JsonValue>],
    normalize_vectors: bool,
    on_conflict: InsertConflictPolicy,
    conflict_target: Option<&str>,
    update_columns: &[String],
) -> Result<String, ApiError> {
    let known = schema
        .iter()
        .map(|column| (column.name.to_ascii_lowercase(), column))
        .collect::<HashMap<_, _>>();
    for row in rows {
        let mut normalized_names = std::collections::HashSet::new();
        for name in row.keys() {
            if !known.contains_key(&name.to_ascii_lowercase()) {
                return Err(ApiError::bad_request(
                    "unknown_column",
                    format!("column '{name}' does not exist"),
                ));
            }
            if !normalized_names.insert(name.to_ascii_lowercase()) {
                return Err(ApiError::bad_request(
                    "duplicate_column",
                    format!("column '{name}' appears more than once"),
                ));
            }
        }
    }

    let columns = schema
        .iter()
        .map(|column| quote_identifier(&column.name))
        .collect::<Vec<_>>()
        .join(", ");
    let mut values = Vec::with_capacity(rows.len());
    for row in rows {
        let normalized = row
            .iter()
            .map(|(name, value)| (name.to_ascii_lowercase(), value))
            .collect::<HashMap<_, _>>();
        let row_values = schema
            .iter()
            .map(|column| {
                let value = normalized
                    .get(&column.name.to_ascii_lowercase())
                    .copied()
                    .unwrap_or(&JsonValue::Null);
                json_literal(value, &column.data_type, &column.name, normalize_vectors)
            })
            .collect::<Result<Vec<_>, _>>()?;
        values.push(format!("({})", row_values.join(", ")));
    }
    let conflict_clause =
        build_conflict_clause(schema, on_conflict, conflict_target, update_columns)?;
    Ok(format!(
        "INSERT INTO {} ({columns}) VALUES {}{conflict_clause}",
        quote_identifier(&table.to_ascii_lowercase()),
        values.join(", ")
    ))
}

fn build_conflict_clause(
    schema: &[Column],
    policy: InsertConflictPolicy,
    conflict_target: Option<&str>,
    update_columns: &[String],
) -> Result<String, ApiError> {
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
        InsertConflictPolicy::Fail => Ok(String::new()),
        InsertConflictPolicy::DoNothing => Ok(match target {
            Some(target) => format!(
                " ON CONFLICT ({}) DO NOTHING",
                quote_identifier(&target.name)
            ),
            None => " ON CONFLICT DO NOTHING".into(),
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
            let assignments = update_columns
                .iter()
                .map(|name| {
                    let column = resolve_column(schema, name)?;
                    if !seen.insert(column.name.to_ascii_lowercase()) {
                        return Err(ApiError::bad_request(
                            "duplicate_column",
                            format!("update column '{}' appears more than once", column.name),
                        ));
                    }
                    let identifier = quote_identifier(&column.name);
                    Ok(format!("{identifier} = excluded.{identifier}"))
                })
                .collect::<Result<Vec<_>, ApiError>>()?;
            Ok(format!(
                " ON CONFLICT ({}) DO UPDATE SET {}",
                quote_identifier(&target.name),
                assignments.join(", ")
            ))
        }
    }
}

fn build_search_sql(request: &VectorSearchRequest, schema: &[Column]) -> Result<String, ApiError> {
    let vector_column = resolve_column(schema, &request.vector_column)?;
    let dimensions = match vector_column.data_type {
        DataType::Vector(dimensions) => dimensions,
        _ => {
            return Err(ApiError::bad_request(
                "not_a_vector",
                format!("column '{}' is not a vector", vector_column.name),
            ))
        }
    };
    let query = Vector::new(request.query.clone()).map_err(ApiError::from)?;
    if query.dimensions() != dimensions {
        return Err(ApiError::from(Error::DimensionMismatch {
            left: dimensions,
            right: query.dimensions(),
        }));
    }

    let mut selected = if request.select.is_empty() {
        schema
            .iter()
            .filter(|column| !matches!(column.data_type, DataType::Vector(_)))
            .collect::<Vec<_>>()
    } else {
        request
            .select
            .iter()
            .map(|name| resolve_column(schema, name))
            .collect::<Result<Vec<_>, _>>()?
    };
    if selected.is_empty() {
        selected.push(vector_column);
    }

    let vector = format!(
        "ARRAY[{}]",
        query
            .as_slice()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    );
    let (function, direction) = match request.metric {
        SearchMetric::Cosine => ("cosine_distance", "ASC"),
        SearchMetric::L2 => ("l2_distance", "ASC"),
        SearchMetric::SquaredL2 => ("squared_l2_distance", "ASC"),
        SearchMetric::DotProduct => ("dot_product", "DESC"),
    };
    let distance = format!(
        "{function}({}, {vector}) AS distance",
        quote_identifier(&vector_column.name)
    );
    let mut projection = selected
        .into_iter()
        .map(|column| quote_identifier(&column.name))
        .collect::<Vec<_>>();
    projection.push(distance);

    let mut filters = Vec::with_capacity(request.filters.len());
    for filter in &request.filters {
        let column = resolve_column(schema, &filter.column)?;
        let identifier = quote_identifier(&column.name);
        if filter.value.is_null() {
            let predicate = match filter.operator {
                FilterOperator::Eq => format!("{identifier} IS NULL"),
                FilterOperator::Ne => format!("{identifier} IS NOT NULL"),
                _ => {
                    return Err(ApiError::bad_request(
                        "invalid_null_filter",
                        "NULL filters only support eq and ne",
                    ))
                }
            };
            filters.push(predicate);
            continue;
        }
        if matches!(column.data_type, DataType::Vector(_)) {
            return Err(ApiError::bad_request(
                "invalid_filter_column",
                "vector columns cannot be used as structured scalar filters",
            ));
        }
        let operator = match filter.operator {
            FilterOperator::Eq => "=",
            FilterOperator::Ne => "!=",
            FilterOperator::Gt => ">",
            FilterOperator::Gte => ">=",
            FilterOperator::Lt => "<",
            FilterOperator::Lte => "<=",
        };
        let literal = json_literal(&filter.value, &column.data_type, &column.name, false)?;
        filters.push(format!("{identifier} {operator} {literal}"));
    }

    let where_clause = if filters.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", filters.join(" AND "))
    };
    Ok(format!(
        "SELECT {} FROM {}{where_clause} ORDER BY distance {direction} LIMIT {}",
        projection.join(", "),
        quote_identifier(&request.table.to_ascii_lowercase()),
        request.limit
    ))
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
    if value.is_null() {
        return Ok("NULL".into());
    }
    let invalid = || {
        ApiError::bad_request(
            "invalid_value",
            format!("value for column '{column}' must be {data_type}"),
        )
    };
    match data_type {
        DataType::Integer => value
            .as_i64()
            .map(|value| value.to_string())
            .ok_or_else(invalid),
        DataType::Float => value
            .as_f64()
            .filter(|value| value.is_finite())
            .map(|value| value.to_string())
            .ok_or_else(invalid),
        DataType::Text => value
            .as_str()
            .map(|value| format!("'{}'", value.replace('\'', "''")))
            .ok_or_else(invalid),
        DataType::Boolean => value
            .as_bool()
            .map(|value| if value { "TRUE" } else { "FALSE" }.into())
            .ok_or_else(invalid),
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
            Ok(format!(
                "ARRAY[{}]",
                vector
                    .as_slice()
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
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
        Self::Query {
            columns: result.columns,
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

    fn from_blocking(error: BlockingError) -> Self {
        Self::internal(format!("database worker failed: {error}"))
    }
}

impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        let (status, code) = match error {
            Error::TableNotFound(_) | Error::ColumnNotFound(_) | Error::IndexNotFound(_) => {
                (StatusCode::NOT_FOUND, "not_found")
            }
            Error::TableAlreadyExists(_)
            | Error::IndexAlreadyExists(_)
            | Error::UniqueViolation(_)
            | Error::NullViolation(_) => (StatusCode::CONFLICT, "constraint_violation"),
            Error::LockPoisoned | Error::StorageIo(_) | Error::CorruptSnapshot(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
            }
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
    let response = HttpResponse::BadRequest().json(ApiErrorBody {
        error: ApiErrorDetails {
            code: "invalid_json",
            message: error.to_string(),
        },
    });
    InternalError::from_response(error, response).into()
}
