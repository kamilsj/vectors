use actix_web::http::header::{AUTHORIZATION, WWW_AUTHENTICATE};
use actix_web::http::StatusCode;
use actix_web::{test, web, App};
use serde_json::{json, Value};
use vectors::{api, Database};

fn api_database() -> Database {
    let database = Database::new();
    database
        .execute(
            "CREATE TABLE documents (
                id INTEGER PRIMARY KEY,
                title TEXT NOT NULL,
                category TEXT,
                embedding VECTOR(3)
            );
            CREATE INDEX documents_category_idx
                ON documents USING HASH (category);",
        )
        .expect("test schema should be valid");
    database
}

#[actix_web::test]
async fn executes_sql_and_returns_json_values() {
    let database = api_database();
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(database))
            .configure(api::configure),
    )
    .await;

    let insert = test::TestRequest::post()
        .uri("/v1/sql")
        .set_json(json!({
            "sql": "INSERT INTO documents VALUES (1, 'Rust', 'tech', ARRAY[1, 0, 0])"
        }))
        .to_request();
    let response: Value = test::call_and_read_body_json(&app, insert).await;
    assert_eq!(response["results"][0]["type"], "command");
    assert_eq!(response["results"][0]["rows_affected"], 1);

    let select = test::TestRequest::post()
        .uri("/v1/sql")
        .set_json(json!({ "sql": "SELECT id, embedding FROM documents" }))
        .to_request();
    let response: Value = test::call_and_read_body_json(&app, select).await;
    assert_eq!(response["results"][0]["rows"][0][0], 1);
    assert_eq!(response["results"][0]["rows"][0][1], json!([1.0, 0.0, 0.0]));
}

#[actix_web::test]
async fn bulk_ingests_embeddings_and_runs_filtered_search() {
    let database = api_database();
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(database))
            .configure(api::configure),
    )
    .await;

    let ingest = test::TestRequest::post()
        .uri("/v1/tables/documents/rows")
        .set_json(json!({
            "normalize_vectors": true,
            "rows": [
                {"id": 1, "title": "Rust", "category": "tech", "embedding": [1, 0, 0]},
                {"id": 2, "title": "Cooking", "category": "food", "embedding": [0, 1, 0]},
                {"id": 3, "title": "Databases", "category": "tech", "embedding": [0.8, 0.2, 0]}
            ]
        }))
        .to_request();
    let ingest_response = test::call_service(&app, ingest).await;
    assert_eq!(ingest_response.status(), StatusCode::OK);

    let retry = test::TestRequest::post()
        .uri("/v1/tables/documents/rows")
        .set_json(json!({
            "on_conflict": "do_nothing",
            "rows": [
                {"id": 1, "title": "duplicate", "embedding": [1, 0, 0]},
                {"id": 4, "title": "Archived", "category": "archive", "embedding": [0, 0, 1]}
            ]
        }))
        .to_request();
    let response: Value = test::call_and_read_body_json(&app, retry).await;
    assert_eq!(response["results"][0]["rows_affected"], 1);

    let invalid_upsert = test::TestRequest::post()
        .uri("/v1/tables/documents/rows")
        .set_json(json!({
            "on_conflict": "do_update",
            "update_columns": ["title"],
            "rows": [{"id": 1, "title": "missing target", "embedding": [1, 0, 0]}]
        }))
        .to_request();
    let invalid_response = test::call_service(&app, invalid_upsert).await;
    assert_eq!(invalid_response.status(), StatusCode::BAD_REQUEST);

    let upsert = test::TestRequest::post()
        .uri("/v1/tables/documents/rows")
        .set_json(json!({
            "normalize_vectors": true,
            "on_conflict": "do_update",
            "conflict_target": "id",
            "update_columns": ["title", "category", "embedding"],
            "rows": [
                {"id": 1, "title": "Rust revised", "category": "tech", "embedding": [3, 0, 0]},
                {"id": 5, "title": "New archive", "category": "archive", "embedding": [0, 0, 2]}
            ]
        }))
        .to_request();
    let response: Value = test::call_and_read_body_json(&app, upsert).await;
    assert_eq!(response["results"][0]["rows_affected"], 2);

    let search = test::TestRequest::post()
        .uri("/v1/embeddings/search")
        .set_json(json!({
            "table": "documents",
            "vector_column": "embedding",
            "query": [1, 0, 0],
            "metric": "cosine",
            "select": ["id", "title"],
            "filters": [{"column": "category", "operator": "eq", "value": "tech"}],
            "limit": 2
        }))
        .to_request();
    let response: Value = test::call_and_read_body_json(&app, search).await;
    assert_eq!(response["type"], "query");
    assert_eq!(response["row_count"], 2);
    assert_eq!(response["rows_examined"], 2);
    assert_eq!(response["rows"][0][0], 1);
    assert_eq!(response["rows"][0][1], "Rust revised");
    assert_eq!(response["rows"][0][2], 0.0);
}

#[actix_web::test]
async fn optionally_normalizes_vectors_during_ingestion() {
    let database = api_database();
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(database))
            .configure(api::configure),
    )
    .await;

    let ingest = test::TestRequest::post()
        .uri("/v1/tables/documents/rows")
        .set_json(json!({
            "normalize_vectors": true,
            "rows": [{
                "id": 1,
                "title": "normalized",
                "embedding": [3, 4, 0]
            }]
        }))
        .to_request();
    let response = test::call_service(&app, ingest).await;
    assert_eq!(response.status(), StatusCode::OK);

    let inspect = test::TestRequest::post()
        .uri("/v1/sql")
        .set_json(json!({
            "sql": "SELECT vector_norm(embedding), embedding FROM documents WHERE id = 1"
        }))
        .to_request();
    let response: Value = test::call_and_read_body_json(&app, inspect).await;
    let norm = response["results"][0]["rows"][0][0]
        .as_f64()
        .expect("norm should be numeric");
    assert!((norm - 1.0).abs() < 1.0e-7);
    let embedding = response["results"][0]["rows"][0][1]
        .as_array()
        .expect("embedding should be an array");
    assert!((embedding[0].as_f64().unwrap() - 0.6).abs() < 1.0e-6);
    assert!((embedding[1].as_f64().unwrap() - 0.8).abs() < 1.0e-6);
    assert_eq!(embedding[2], 0.0);
}

#[actix_web::test]
async fn rejects_wrong_embedding_dimensions_with_json_error() {
    let database = api_database();
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(database))
            .configure(api::configure),
    )
    .await;

    let request = test::TestRequest::post()
        .uri("/v1/tables/documents/rows")
        .set_json(json!({
            "rows": [{"id": 1, "title": "bad", "embedding": [1, 2]}]
        }))
        .to_request();
    let response = test::call_service(&app, request).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = test::read_body_json(response).await;
    assert_eq!(body["error"]["code"], "dimension_mismatch");
}

#[actix_web::test]
async fn exposes_health_schema_and_index_metadata() {
    let database = api_database();
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(database))
            .configure(api::configure),
    )
    .await;

    let health = test::TestRequest::get().uri("/healthz").to_request();
    let response: Value = test::call_and_read_body_json(&app, health).await;
    assert_eq!(response["status"], "ok");

    let console = test::TestRequest::get().uri("/").to_request();
    let response = test::call_service(&app, console).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-security-policy").unwrap(),
        "default-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'"
    );
    let body = test::read_body(response).await;
    assert!(body
        .windows("SQL-first vector database".len())
        .any(|window| window == b"SQL-first vector database"));

    let tables = test::TestRequest::get().uri("/v1/tables").to_request();
    let response: Value = test::call_and_read_body_json(&app, tables).await;
    assert_eq!(response["tables"][0]["name"], "documents");
    assert_eq!(response["tables"][0]["row_count"], 0);
    assert_eq!(response["tables"][0]["column_count"], 4);
    assert_eq!(response["tables"][0]["index_count"], 1);
    assert!(response["revision"].as_u64().unwrap() > 0);

    let schema = test::TestRequest::get()
        .uri("/v1/tables/documents/schema")
        .to_request();
    let response: Value = test::call_and_read_body_json(&app, schema).await;
    assert_eq!(response["columns"].as_array().unwrap().len(), 4);

    let indexes = test::TestRequest::get()
        .uri("/v1/tables/documents/indexes")
        .to_request();
    let response: Value = test::call_and_read_body_json(&app, indexes).await;
    assert_eq!(response["indexes"][0]["name"], "documents_category_idx");
}

#[actix_web::test]
async fn bearer_authentication_protects_v1_but_not_health() {
    let database = api_database();
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(database))
            .app_data(web::Data::new(api::ApiSecurity::bearer_token(
                "correct-horse-battery-staple",
            )))
            .configure(api::configure),
    )
    .await;

    let health = test::TestRequest::get().uri("/healthz").to_request();
    assert_eq!(
        test::call_service(&app, health).await.status(),
        StatusCode::OK
    );

    let missing = test::TestRequest::post()
        .uri("/v1/sql")
        .set_json(json!({ "sql": "SELECT 1" }))
        .to_request();
    let response = test::call_service(&app, missing).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(response.headers().get(WWW_AUTHENTICATE).unwrap(), "Bearer");

    let tables = test::TestRequest::get().uri("/v1/tables").to_request();
    assert_eq!(
        test::call_service(&app, tables).await.status(),
        StatusCode::UNAUTHORIZED
    );

    let wrong = test::TestRequest::post()
        .uri("/v1/sql")
        .insert_header((AUTHORIZATION, "Bearer incorrect"))
        .set_json(json!({ "sql": "SELECT 1" }))
        .to_request();
    assert_eq!(
        test::call_service(&app, wrong).await.status(),
        StatusCode::UNAUTHORIZED
    );

    let authorized = test::TestRequest::post()
        .uri("/v1/sql")
        .insert_header((AUTHORIZATION, "Bearer correct-horse-battery-staple"))
        .set_json(json!({ "sql": "SELECT 1" }))
        .to_request();
    assert_eq!(
        test::call_service(&app, authorized).await.status(),
        StatusCode::OK
    );

    let error = api::serve_authenticated(Database::new(), "127.0.0.1:0", String::new())
        .await
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}
