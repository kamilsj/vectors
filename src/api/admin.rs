//! Typed, bounded table administration for the browser console.

use super::schema::{decode_columns, normalized_identifier, CreateColumn};
use super::*;
use std::collections::HashSet;

const MAX_PAGE_ROWS: usize = 250;
const DEFAULT_PAGE_ROWS: usize = 50;
const MAX_ADMIN_COLUMNS: usize = 256;

pub(super) fn configure(config: &mut web::ServiceConfig) {
    config.service(
        web::scope("/admin")
            .app_data(web::QueryConfig::default().error_handler(|error, _| {
                ApiError::bad_request("invalid_pagination", error.to_string()).into()
            }))
            .route("/tables", web::post().to(create_table))
            .service(
                web::resource("/tables/{table}/rows")
                    .route(web::get().to(browse_rows))
                    .route(web::patch().to(update_row))
                    .route(web::delete().to(delete_row)),
            )
            .route("/tables/{table}", web::delete().to(drop_table)),
    );
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PageRequest {
    limit: Option<usize>,
    #[serde(default)]
    offset: usize,
}

#[derive(Debug, Serialize)]
struct PageResponse {
    table: String,
    columns: Vec<String>,
    schema: Vec<ColumnResponse>,
    rows: Vec<Vec<JsonValue>>,
    total_rows: usize,
    limit: usize,
    offset: usize,
    revision: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateTableRequest {
    name: String,
    columns: Vec<CreateColumn>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RowKey {
    column: String,
    value: JsonValue,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateRowRequest {
    expected_revision: u64,
    key: RowKey,
    values: Map<String, JsonValue>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteRowRequest {
    expected_revision: u64,
    key: RowKey,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DropTableRequest {
    expected_revision: u64,
    confirm_table: String,
}

async fn browse_rows(
    http_request: HttpRequest,
    security: Option<web::Data<ApiSecurity>>,
    limiter: Option<web::Data<DatabaseTaskLimiter>>,
    limits: web::Data<RequestLimits>,
    database: web::Data<Database>,
    table: web::Path<String>,
    request: web::Query<PageRequest>,
) -> Result<web::Json<PageResponse>, ApiError> {
    authorize(&http_request, security.as_ref().map(|data| data.get_ref()))?;
    let table = normalized_identifier(&table)?;
    let requested_limit = request.limit.unwrap_or(DEFAULT_PAGE_ROWS);
    if requested_limit == 0 || requested_limit > MAX_PAGE_ROWS {
        return Err(ApiError::bad_request(
            "invalid_pagination",
            format!("limit must be between 1 and {MAX_PAGE_ROWS}"),
        ));
    }
    let limit = requested_limit.min(limits.max_response_rows);
    let offset = request.offset;
    let database = database.get_ref().clone();
    let lookup = table.clone();
    let page = run_database_task(limiter.as_ref(), move || {
        database.table_page(&lookup, offset, limit)
    })
    .await?;
    Ok(web::Json(PageResponse {
        table,
        columns: page
            .columns
            .iter()
            .map(|column| column.name.clone())
            .collect(),
        schema: page
            .columns
            .into_iter()
            .map(|column| ColumnResponse {
                name: column.name,
                data_type: column.data_type.to_string(),
                nullable: column.nullable,
                unique: column.unique,
            })
            .collect(),
        rows: page
            .rows
            .into_iter()
            .map(|row| row.into_iter().map(json_value).collect())
            .collect(),
        total_rows: page.total_rows,
        limit,
        offset,
        revision: page.revision,
    }))
}

async fn create_table(
    http_request: HttpRequest,
    security: Option<web::Data<ApiSecurity>>,
    limiter: Option<web::Data<DatabaseTaskLimiter>>,
    database: web::Data<Database>,
    request: web::Json<CreateTableRequest>,
) -> Result<web::Json<SqlResponse>, ApiError> {
    authorize(&http_request, security.as_ref().map(|data| data.get_ref()))?;
    let request = request.into_inner();
    let name = normalized_identifier(&request.name)?;
    let columns = decode_columns(request.columns, 1, MAX_ADMIN_COLUMNS)?;
    let definitions = columns
        .iter()
        .map(|column| {
            format!(
                "{} {}{}{}",
                quote_identifier(&column.name),
                column.data_type,
                if column.nullable { "" } else { " NOT NULL" },
                if column.unique { " UNIQUE" } else { "" }
            )
        })
        .collect::<Vec<_>>();
    let sql = format!(
        "CREATE TABLE {} ({})",
        quote_identifier(&name),
        definitions.join(", ")
    );
    let database = database.get_ref().clone();
    let results = run_database_task(limiter.as_ref(), move || database.execute(&sql)).await?;
    Ok(web::Json(SqlResponse::from(results)))
}

async fn update_row(
    http_request: HttpRequest,
    security: Option<web::Data<ApiSecurity>>,
    limiter: Option<web::Data<DatabaseTaskLimiter>>,
    database: web::Data<Database>,
    table: web::Path<String>,
    request: web::Json<UpdateRowRequest>,
) -> Result<web::Json<SqlResponse>, ApiError> {
    authorize(&http_request, security.as_ref().map(|data| data.get_ref()))?;
    let table = normalized_identifier(&table)?;
    let request = request.into_inner();
    if request.values.is_empty() {
        return Err(ApiError::bad_request(
            "empty_update",
            "provide at least one column value to update",
        ));
    }
    let database = database.get_ref().clone();
    let results = run_database_task(limiter.as_ref(), move || {
        let schema = mutation_schema(&database, &table, request.expected_revision)?;
        let predicate = key_predicate(&request.key, &schema)?;
        let mut seen = HashSet::new();
        let assignments = request
            .values
            .iter()
            .map(|(name, value)| {
                let column = resolve_column(&schema, name)?;
                if !seen.insert(column.name.to_ascii_lowercase()) {
                    return Err(ApiError::bad_request(
                        "duplicate_column",
                        format!("column '{}' appears more than once", column.name),
                    ));
                }
                let literal = json_literal(value, &column.data_type, &column.name, false)?;
                Ok(format!("{} = {literal}", quote_identifier(&column.name)))
            })
            .collect::<Result<Vec<_>, ApiError>>()?;
        let sql = format!(
            "UPDATE {} SET {} WHERE {predicate}",
            quote_identifier(&table),
            assignments.join(", ")
        );
        let results = database.execute_if_revision(&sql, request.expected_revision)?;
        ensure_row_found(&results)?;
        Ok::<_, ApiError>(results)
    })
    .await?;
    Ok(web::Json(SqlResponse::from(results)))
}

async fn delete_row(
    http_request: HttpRequest,
    security: Option<web::Data<ApiSecurity>>,
    limiter: Option<web::Data<DatabaseTaskLimiter>>,
    database: web::Data<Database>,
    table: web::Path<String>,
    request: web::Json<DeleteRowRequest>,
) -> Result<web::Json<SqlResponse>, ApiError> {
    authorize(&http_request, security.as_ref().map(|data| data.get_ref()))?;
    let table = normalized_identifier(&table)?;
    let request = request.into_inner();
    let database = database.get_ref().clone();
    let results = run_database_task(limiter.as_ref(), move || {
        let schema = mutation_schema(&database, &table, request.expected_revision)?;
        let predicate = key_predicate(&request.key, &schema)?;
        let sql = format!("DELETE FROM {} WHERE {predicate}", quote_identifier(&table));
        let results = database.execute_if_revision(&sql, request.expected_revision)?;
        ensure_row_found(&results)?;
        Ok::<_, ApiError>(results)
    })
    .await?;
    Ok(web::Json(SqlResponse::from(results)))
}

async fn drop_table(
    http_request: HttpRequest,
    security: Option<web::Data<ApiSecurity>>,
    limiter: Option<web::Data<DatabaseTaskLimiter>>,
    database: web::Data<Database>,
    table: web::Path<String>,
    request: web::Json<DropTableRequest>,
) -> Result<web::Json<SqlResponse>, ApiError> {
    authorize(&http_request, security.as_ref().map(|data| data.get_ref()))?;
    let table = normalized_identifier(&table)?;
    if request.confirm_table != table {
        return Err(ApiError::bad_request(
            "confirmation_required",
            "confirm_table must exactly match the table name",
        ));
    }
    let expected_revision = request.expected_revision;
    let sql = format!("DROP TABLE {}", quote_identifier(&table));
    let database = database.get_ref().clone();
    let results = run_database_task(limiter.as_ref(), move || {
        database.execute_if_revision(&sql, expected_revision)
    })
    .await?;
    Ok(web::Json(SqlResponse::from(results)))
}

fn mutation_schema(
    database: &Database,
    table: &str,
    expected: u64,
) -> Result<Vec<Column>, ApiError> {
    let schema = database.schema(table);
    let actual = database.revision()?;
    if actual != expected {
        return Err(ApiError::from(Error::RevisionConflict { expected, actual }));
    }
    schema.map_err(ApiError::from)
}

fn key_predicate(key: &RowKey, schema: &[Column]) -> Result<String, ApiError> {
    let column = resolve_column(schema, &key.column)?;
    if !column.unique || key.value.is_null() || matches!(column.data_type, DataType::Vector(_)) {
        return Err(ApiError {
            status: StatusCode::CONFLICT,
            code: "unsafe_row_key",
            message: "row changes require a non-null value in a scalar UNIQUE column".into(),
        });
    }
    let literal = json_literal(&key.value, &column.data_type, &column.name, false)?;
    Ok(format!("{} = {literal}", quote_identifier(&column.name)))
}

fn ensure_row_found(results: &[ExecutionResult]) -> Result<(), ApiError> {
    if results.iter().any(|result| {
        matches!(
            result,
            ExecutionResult::Command {
                rows_affected: 1,
                ..
            }
        )
    }) {
        return Ok(());
    }
    Err(ApiError {
        status: StatusCode::NOT_FOUND,
        code: "row_not_found",
        message: "the selected row no longer exists; refresh the table".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::test;
    use serde_json::json;

    fn database() -> Database {
        let database = Database::new();
        database.execute("CREATE TABLE documents (id INTEGER PRIMARY KEY, title TEXT NOT NULL, embedding VECTOR(2)); INSERT INTO documents VALUES (1, 'one', ARRAY[1, 0]), (2, 'two', ARRAY[0, 1]), (3, 'three', ARRAY[1, 1]);").unwrap();
        database
    }

    #[actix_web::test]
    async fn browses_only_requested_page_with_coherent_metadata_and_limits() {
        let database = database();
        let revision = database.revision().unwrap();
        let app = test::init_service(App::new().app_data(web::Data::new(database)).configure(
            |config| configure_with_limits(config, RequestLimits::new(4096, 10, 2).unwrap()),
        ))
        .await;
        let response: JsonValue = test::call_and_read_body_json(
            &app,
            test::TestRequest::get()
                .uri("/v1/admin/tables/documents/rows?limit=250&offset=1")
                .to_request(),
        )
        .await;
        assert_eq!(response["columns"], json!(["id", "title", "embedding"]));
        assert_eq!(response["schema"][0]["unique"], true);
        assert_eq!(response["schema"][2]["data_type"], "VECTOR(2)");
        assert_eq!(
            response["rows"],
            json!([[2, "two", [0.0, 1.0]], [3, "three", [1.0, 1.0]]])
        );
        assert_eq!(response["total_rows"], 3);
        assert_eq!(response["limit"], 2);
        assert_eq!(response["offset"], 1);
        assert_eq!(response["revision"], revision);
        let empty: JsonValue = test::call_and_read_body_json(
            &app,
            test::TestRequest::get()
                .uri("/v1/admin/tables/documents/rows?offset=999999999")
                .to_request(),
        )
        .await;
        assert_eq!(empty["rows"], json!([]));
        assert_eq!(empty["total_rows"], 3);
        for query in [
            "limit=0",
            "limit=251",
            "offset=-1",
            "limit=abc",
            "surprise=true",
        ] {
            let response = test::call_service(
                &app,
                test::TestRequest::get()
                    .uri(&format!("/v1/admin/tables/documents/rows?{query}"))
                    .to_request(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query}");
            let body: JsonValue = test::read_body_json(response).await;
            assert_eq!(body["error"]["code"], "invalid_pagination");
        }
    }

    #[actix_web::test]
    async fn creates_typed_tables_without_interpreting_names_or_types_as_sql() {
        let database = database();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(database.clone()))
                .configure(super::super::configure),
        )
        .await;
        let name = "notes\"; DROP TABLE documents; --";
        let response = test::call_service(&app, test::TestRequest::post().uri("/v1/admin/tables").set_json(json!({"name": name, "columns": [{"name": "ID", "data_type": "INTEGER", "nullable": false, "unique": true}, {"name": "body\"; DROP TABLE documents; --", "data_type": "TEXT"}, {"name": "vector", "data_type": "VECTOR(3)"}]})).to_request()).await;
        assert_eq!(response.status(), StatusCode::OK);
        let schema = database.schema(name).unwrap();
        assert_eq!(schema[0].name, "id");
        assert!(!schema[0].nullable);
        assert!(schema[0].unique);
        assert!(schema[1].nullable);
        assert_eq!(database.table_info().unwrap().len(), 2);
        for columns in [
            json!([]),
            json!([{"name":"id", "data_type":"INTEGER); DROP TABLE documents; --"}]),
            json!([{"name":"id", "data_type":"INTEGER"}, {"name":"ID", "data_type":"TEXT"}]),
            json!([{"name":"v", "data_type":"VECTOR(0)"}]),
            json!([{"name":"v", "data_type":"VECTOR(65536)"}]),
        ] {
            let response = test::call_service(
                &app,
                test::TestRequest::post()
                    .uri("/v1/admin/tables")
                    .set_json(json!({"name":"invalid", "columns":columns}))
                    .to_request(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
        assert!(database.schema("invalid").is_err());
        assert!(database.schema("documents").is_ok());
    }

    #[actix_web::test]
    async fn updates_and_deletes_exactly_one_row_using_typed_values() {
        let database = database();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(database.clone()))
                .configure(super::super::configure),
        )
        .await;
        let title = "quoted ' value; DELETE FROM documents; --";
        let response: JsonValue = test::call_and_read_body_json(&app, test::TestRequest::patch().uri("/v1/admin/tables/documents/rows").set_json(json!({"expected_revision":database.revision().unwrap(), "key":{"column":"id", "value":2}, "values":{"title":title, "embedding":[0.5, 0.25]}})).to_request()).await;
        assert_eq!(response["results"][0]["rows_affected"], 1);
        let page = database.table_page("documents", 0, 10).unwrap();
        assert_eq!(page.total_rows, 3);
        assert_eq!(page.rows[1][1], Value::Text(title.into()));
        let response: JsonValue = test::call_and_read_body_json(
            &app,
            test::TestRequest::delete()
                .uri("/v1/admin/tables/documents/rows")
                .set_json(
                    json!({"expected_revision":page.revision, "key":{"column":"id", "value":2}}),
                )
                .to_request(),
        )
        .await;
        assert_eq!(response["results"][0]["rows_affected"], 1);
        let page = database.table_page("documents", 0, 10).unwrap();
        assert_eq!(
            page.rows
                .iter()
                .map(|row| row[0].clone())
                .collect::<Vec<_>>(),
            vec![Value::Integer(1), Value::Integer(3)]
        );
    }

    #[actix_web::test]
    async fn rejects_stale_mutations_and_requires_exact_drop_confirmation() {
        let database = database();
        let revision = database.revision().unwrap();
        database
            .execute("INSERT INTO documents VALUES (4, 'four', ARRAY[0, 0])")
            .unwrap();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(database.clone()))
                .configure(super::super::configure),
        )
        .await;
        for (method, uri, body) in [
            (
                actix_web::http::Method::PATCH,
                "/v1/admin/tables/documents/rows",
                json!({"expected_revision":revision, "key":{"column":"id", "value":1}, "values":{"title":"stale"}}),
            ),
            (
                actix_web::http::Method::DELETE,
                "/v1/admin/tables/documents/rows",
                json!({"expected_revision":revision, "key":{"column":"id", "value":1}}),
            ),
            (
                actix_web::http::Method::DELETE,
                "/v1/admin/tables/documents",
                json!({"expected_revision":revision, "confirm_table":"documents"}),
            ),
        ] {
            let response = test::call_service(
                &app,
                test::TestRequest::default()
                    .method(method)
                    .uri(uri)
                    .set_json(body)
                    .to_request(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::CONFLICT);
            let body: JsonValue = test::read_body_json(response).await;
            assert_eq!(body["error"]["code"], "stale_revision");
        }
        let response = test::call_service(&app, test::TestRequest::delete().uri("/v1/admin/tables/documents").set_json(json!({"expected_revision":database.revision().unwrap(), "confirm_table":"DOCUMENTS"})).to_request()).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            database.table_page("documents", 0, 10).unwrap().total_rows,
            4
        );
        let response = test::call_service(&app, test::TestRequest::delete().uri("/v1/admin/tables/documents").set_json(json!({"expected_revision":database.revision().unwrap(), "confirm_table":"documents"})).to_request()).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(database.schema("documents").is_err());
    }

    #[actix_web::test]
    async fn rejects_unsafe_keys_empty_updates_and_invalid_values_without_writing() {
        let database = database();
        let revision = database.revision().unwrap();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(database.clone()))
                .configure(super::super::configure),
        )
        .await;
        for key in [
            json!({"column":"title", "value":"one"}),
            json!({"column":"id", "value":null}),
            json!({"column":"embedding", "value":[1,0]}),
        ] {
            let response = test::call_service(
                &app,
                test::TestRequest::delete()
                    .uri("/v1/admin/tables/documents/rows")
                    .set_json(json!({"expected_revision":revision, "key":key}))
                    .to_request(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::CONFLICT);
        }
        for values in [
            json!({}),
            json!({"id":1.5}),
            json!({"unknown":1}),
            json!({"title":"a", "TITLE":"b"}),
            json!({"embedding":[1,2,3]}),
        ] {
            let response = test::call_service(&app, test::TestRequest::patch().uri("/v1/admin/tables/documents/rows").set_json(json!({"expected_revision":revision, "key":{"column":"id", "value":1}, "values":values})).to_request()).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
        let missing = test::call_service(
            &app,
            test::TestRequest::delete()
                .uri("/v1/admin/tables/documents/rows")
                .set_json(json!({"expected_revision":revision, "key":{"column":"id", "value":99}}))
                .to_request(),
        )
        .await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        let unguarded = test::call_service(
            &app,
            test::TestRequest::delete()
                .uri("/v1/admin/tables/documents/rows")
                .set_json(json!({"key":{"column":"id", "value":1}}))
                .to_request(),
        )
        .await;
        assert_eq!(unguarded.status(), StatusCode::BAD_REQUEST);
        assert_eq!(database.revision().unwrap(), revision);
        assert_eq!(
            database.table_page("documents", 0, 10).unwrap().total_rows,
            3
        );
    }

    #[actix_web::test]
    async fn admin_routes_apply_authentication_and_task_capacity() {
        let database = database();
        let revision = database.revision().unwrap();
        let limiter = DatabaseTaskLimiter::new(1);
        let _permit = limiter.acquire().unwrap();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(database))
                .app_data(web::Data::new(ApiSecurity::bearer_token("secret")))
                .app_data(web::Data::new(limiter))
                .configure(super::super::configure),
        )
        .await;
        for (method, uri, body) in [
            (
                actix_web::http::Method::GET,
                "/v1/admin/tables/documents/rows",
                json!(null),
            ),
            (
                actix_web::http::Method::POST,
                "/v1/admin/tables",
                json!({"name":"other", "columns":[{"name":"id", "data_type":"INTEGER"}]}),
            ),
            (
                actix_web::http::Method::PATCH,
                "/v1/admin/tables/documents/rows",
                json!({"expected_revision":revision, "key":{"column":"id", "value":1}, "values":{"title":"changed"}}),
            ),
            (
                actix_web::http::Method::DELETE,
                "/v1/admin/tables/documents/rows",
                json!({"expected_revision":revision, "key":{"column":"id", "value":1}}),
            ),
            (
                actix_web::http::Method::DELETE,
                "/v1/admin/tables/documents",
                json!({"expected_revision":revision, "confirm_table":"documents"}),
            ),
        ] {
            let response = test::call_service(
                &app,
                test::TestRequest::default()
                    .method(method.clone())
                    .uri(uri)
                    .set_json(&body)
                    .to_request(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            let response = test::call_service(
                &app,
                test::TestRequest::default()
                    .method(method)
                    .uri(uri)
                    .insert_header((AUTHORIZATION, "Bearer secret"))
                    .set_json(&body)
                    .to_request(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        }
    }
}
