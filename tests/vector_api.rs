use actix_web::http::StatusCode;
use actix_web::{test, web, App};
use serde_json::{json, Value};
use vectors::{api, Database, ExecutionResult};

async fn post(database: &Database, uri: &str, body: Value) -> (StatusCode, Value) {
    post_with_limits(database, uri, body, api::RequestLimits::default()).await
}

async fn post_with_limits(
    database: &Database,
    uri: &str,
    body: Value,
    limits: api::RequestLimits,
) -> (StatusCode, Value) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(database.clone()))
            .configure(move |services| api::configure_with_limits(services, limits)),
    )
    .await;
    let response = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(uri)
            .set_json(body)
            .to_request(),
    )
    .await;
    (response.status(), test::read_body_json(response).await)
}

fn stored_rows(database: &Database, table: &str) -> Vec<Vec<vectors::Value>> {
    let results = database
        .execute(&format!("SELECT * FROM {table} ORDER BY id"))
        .unwrap();
    let ExecutionResult::Query(result) = &results[0] else {
        panic!("expected query result");
    };
    result.rows.clone()
}

fn query(table: &str) -> Value {
    json!({
        "table": table, "vector_column": "embedding", "query": [1, 0, 0],
        "select": ["id"], "limit": 10
    })
}

#[actix_web::test]
async fn indexed_filters_and_residual_predicates_match_full_scan_for_every_metric() {
    let database = Database::new();
    for table in ["indexed", "unindexed"] {
        database.execute(&format!(
            "CREATE TABLE {table} (id INTEGER PRIMARY KEY, category TEXT, active BOOLEAN, weight DOUBLE, embedding VECTOR(3));
             INSERT INTO {table} VALUES
             (1, 'keep', TRUE, 10, ARRAY[1, 0, 0]),
             (2, 'keep', FALSE, 20, ARRAY[1, 0, 0]),
             (3, 'keep', TRUE, 20, ARRAY[0.8, 0.2, 0]),
             (4, 'other', TRUE, 20, ARRAY[1, 0, 0]),
             (5, 'keep', TRUE, 2, ARRAY[0, 1, 0]),
             (6, 'keep', TRUE, 15, ARRAY[0, 0, 1]),
             (7, NULL, TRUE, 20, ARRAY[1, 0, 0]),
             (8, 'keep', TRUE, 20, NULL);"
        )).unwrap();
    }
    database
        .execute("CREATE INDEX category_idx ON indexed USING HASH (category)")
        .unwrap();
    for metric in ["cosine", "l2", "squared_l2", "dot_product"] {
        let mut request = query("indexed");
        request["metric"] = json!(metric);
        request["filters"] = json!([
            {"column": "CATEGORY", "operator": "eq", "value": "keep"},
            {"column": "active", "operator": "eq", "value": true},
            {"column": "weight", "operator": "gte", "value": 10}
        ]);
        let (status, indexed) = post(&database, "/v1/vector/search", request.clone()).await;
        assert_eq!(status, StatusCode::OK, "{metric}: {indexed}");
        request["table"] = json!("unindexed");
        let (status, unindexed) = post(&database, "/v1/vector/search", request).await;
        assert_eq!(status, StatusCode::OK, "{metric}: {unindexed}");
        for field in ["columns", "schema", "rows", "row_count"] {
            assert_eq!(indexed[field], unindexed[field], "{metric}: {field}");
        }
        assert_eq!(indexed["row_count"], 4);
        assert_eq!(indexed["rows_examined"], 6);
        assert_eq!(unindexed["rows_examined"], 8);
        let ids = indexed["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row[0].as_i64().unwrap())
            .collect::<Vec<_>>();
        if metric == "dot_product" {
            assert_eq!(ids, [8, 1, 3, 6]);
            assert!(indexed["rows"][0][1].is_null());
        } else {
            assert_eq!(ids, [1, 3, 6, 8]);
            assert!(indexed["rows"][3][1].is_null());
        }
    }
}

#[actix_web::test]
async fn computed_score_ranks_results_when_a_selected_column_is_named_distance() {
    let database = Database::new();
    database
        .execute(
            "CREATE TABLE documents (id INTEGER PRIMARY KEY, distance DOUBLE, embedding VECTOR(3));
         INSERT INTO documents VALUES
         (1, 2, ARRAY[1, 0, 0]), (2, 1, ARRAY[0, 1, 0]), (3, 3, ARRAY[0, 0, 1]);",
        )
        .unwrap();
    for metric in ["cosine", "l2", "squared_l2", "dot_product"] {
        let mut request = query("documents");
        request["metric"] = json!(metric);
        request["select"] = json!(["id", "distance"]);
        request["limit"] = json!(1);
        let (status, body) = post(&database, "/v1/vector/search", request).await;
        assert_eq!(status, StatusCode::OK, "{metric}: {body}");
        // The existing response shape permits repeated labels. The stored
        // attribute must never replace the computed score used for ranking.
        assert_eq!(body["columns"], json!(["id", "distance", "distance"]));
        assert_eq!(body["rows"][0][0], 1, "{metric}: {body}");
        assert_eq!(body["rows"][0][1], 2.0);
        let score = if metric == "dot_product" { 1.0 } else { 0.0 };
        assert_eq!(body["rows"][0][2], score);
    }
}

#[actix_web::test]
async fn nullable_vector_filters_and_default_projections_keep_their_api_shape() {
    let database = Database::new();
    database
        .execute(
            "CREATE TABLE documents (id INTEGER PRIMARY KEY, embedding VECTOR(3));
         INSERT INTO documents VALUES (1, NULL), (2, ARRAY[1, 0, 0]);
         CREATE TABLE vectors_only (embedding VECTOR(3));
         INSERT INTO vectors_only VALUES (ARRAY[1, 0, 0]);",
        )
        .unwrap();
    for (operator, expected_id, expected_score) in [("eq", 1, Value::Null), ("ne", 2, json!(0.0))] {
        let mut request = query("documents");
        request.as_object_mut().unwrap().remove("select");
        request["filters"] = json!([{"column": "embedding", "operator": operator, "value": null}]);
        let (status, body) = post(&database, "/v1/embeddings/search", request).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["columns"], json!(["id", "distance"]));
        assert_eq!(body["rows"], json!([[expected_id, expected_score]]));
    }
    let mut request = query("vectors_only");
    request["select"] = json!([]);
    let (status, body) = post(&database, "/v1/vector/search", request).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["columns"], json!(["embedding", "distance"]));
    assert_eq!(body["rows"], json!([[[1.0, 0.0, 0.0], 0.0]]));
}

#[actix_web::test]
async fn mixed_case_ingestion_upserts_and_quoted_filter_values_are_literal() {
    let database = Database::new();
    database
        .execute(
            "CREATE TABLE documents (id INTEGER PRIMARY KEY, title TEXT, embedding VECTOR(3));
         CREATE INDEX title_idx ON documents USING HASH (title);",
        )
        .unwrap();
    let literal = "O'Reilly'; DROP TABLE documents; --";
    let (status, body) = post(
        &database,
        "/v1/tables/DOCUMENTS/rows",
        json!({
            "rows": [{"ID": 1, "TiTlE": "original", "Embedding": [0, 1, 0]}]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = post(
        &database,
        "/v1/tables/DoCuMeNtS/rows",
        json!({
            "on_conflict": "do_update", "conflict_target": "ID",
            "update_columns": ["TITLE", "Embedding"], "normalize_vectors": true,
            "rows": [{"Id": 1, "title": literal, "EMBEDDING": [3, 0, 0]}]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["results"][0]["rows_affected"], 1);
    let (status, body) = post(
        &database,
        "/v1/vector/search",
        json!({
            "table": "DOCUMENTS", "vector_column": "EMBEDDING", "query": [1, 0, 0],
            "select": ["ID", "TITLE", "Embedding"], "limit": 1,
            "filters": [{"column": "TiTlE", "operator": "eq", "value": literal}]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["columns"],
        json!(["id", "title", "embedding", "distance"])
    );
    assert_eq!(body["rows"], json!([[1, literal, [1.0, 0.0, 0.0], 0.0]]));
    assert_eq!(body["rows_examined"], 1);
}

#[actix_web::test]
async fn rejected_bulk_batches_leave_rows_indexes_vectors_and_revision_unchanged() {
    let database = Database::new();
    database.execute(
        "CREATE TABLE documents (id INTEGER PRIMARY KEY, title TEXT NOT NULL, tag TEXT UNIQUE, embedding VECTOR(3));
         CREATE INDEX title_idx ON documents USING HASH (title);
         INSERT INTO documents VALUES (1, 'original', 'taken', ARRAY[1, 0, 0]);"
    ).unwrap();
    let before = stored_rows(&database, "documents");
    let revision = database.revision().unwrap();
    let valid = json!({"id": 2, "title": "valid first row", "tag": "new", "embedding": [0, 1, 0]});
    let invalid_rows = [
        json!({"id": 3, "title": "wrong dimensions", "embedding": [1, 0]}),
        json!({"id": 3, "title": "wrong type", "embedding": [1, "bad", 0]}),
        json!({"id": 3, "title": "float overflow", "embedding": [1e100, 0, 0]}),
        json!({"id": 3, "title": "unknown field", "unexpected": true}),
        json!({"id": 3, "ID": 4, "title": "duplicate field"}),
        json!({"id": 1, "title": "duplicate id", "embedding": [0, 0, 1]}),
        json!({"id": 3, "title": null, "embedding": [0, 0, 1]}),
        json!({"id": 3, "title": "duplicate tag", "tag": "taken"}),
    ];
    for invalid in invalid_rows {
        let (status, body) = post(
            &database,
            "/v1/tables/documents/rows",
            json!({
                "rows": [valid, invalid]
            }),
        )
        .await;
        assert!(status.is_client_error(), "{body}");
        assert_eq!(stored_rows(&database, "documents"), before, "{body}");
        assert_eq!(database.revision().unwrap(), revision, "{body}");
    }
    // A later unique violation must also roll back an earlier upsert, including
    // its scalar index entry and dense vector replacement.
    let (status, body) = post(
        &database,
        "/v1/tables/documents/rows",
        json!({
            "on_conflict": "do_update", "conflict_target": "id",
            "update_columns": ["title", "embedding"],
            "rows": [
                {"id": 1, "title": "changed", "tag": "taken", "embedding": [0, 1, 0]},
                {"id": 2, "title": "conflicts", "tag": "taken", "embedding": [0, 0, 1]}
            ]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(stored_rows(&database, "documents"), before);
    assert_eq!(database.revision().unwrap(), revision);
    let mut request = query("documents");
    request["filters"] = json!([{"column": "title", "operator": "eq", "value": "original"}]);
    let (status, body) = post(&database, "/v1/vector/search", request).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["rows"], json!([[1, 0.0]]));
    assert_eq!(body["rows_examined"], 1);
}

#[actix_web::test]
async fn configured_response_cap_applies_to_search_and_is_advertised_by_settings() {
    let database = Database::new();
    database.execute(
        "CREATE TABLE documents (id INTEGER PRIMARY KEY, embedding VECTOR(3));
         INSERT INTO documents VALUES (1, ARRAY[1, 0, 0]), (2, ARRAY[0, 1, 0]), (3, ARRAY[0, 0, 1]);"
    ).unwrap();
    let limits = api::RequestLimits::new(4_096, 10, 2).unwrap();
    for limit in [Some(0), Some(3), Some(1_001), None] {
        let mut request = query("documents");
        if let Some(limit) = limit {
            request["limit"] = json!(limit);
        } else {
            request.as_object_mut().unwrap().remove("limit");
        }
        let (status, body) =
            post_with_limits(&database, "/v1/vector/search", request, limits.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"]["code"], "invalid_limit");
    }
    let mut request = query("documents");
    request["limit"] = json!(2);
    let (status, body) =
        post_with_limits(&database, "/v1/embeddings/search", request, limits.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["row_count"], 2);
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(database))
            .configure(move |services| api::configure_with_limits(services, limits)),
    )
    .await;
    let settings: Value = test::call_and_read_body_json(
        &app,
        test::TestRequest::get()
            .uri("/v1/settings/server")
            .to_request(),
    )
    .await;
    assert_eq!(settings["limits"]["max_search_limit"], 2);
}

#[actix_web::test]
async fn invalid_search_types_and_dimensions_fail_even_on_an_empty_table() {
    let database = Database::new();
    database
        .execute("CREATE TABLE documents (id INTEGER, embedding VECTOR(3), active BOOLEAN)")
        .unwrap();
    let cases = [
        ("query", json!([])),
        ("query", json!([1, 0])),
        ("query", json!([1, 0, 0, 0])),
        ("query", json!([1e100, 0, 0])),
        ("vector_column", json!("id")),
        ("select", json!(["missing"])),
        (
            "filters",
            json!([{"column": "active", "operator": "eq", "value": "true"}]),
        ),
        (
            "filters",
            json!([{"column": "id", "operator": "gt", "value": null}]),
        ),
        (
            "filters",
            json!([{"column": "embedding", "operator": "eq", "value": [1, 0, 0]}]),
        ),
        (
            "filters",
            json!([{"column": "id", "operator": "eq", "value": 1, "sql": "OR TRUE"}]),
        ),
    ];
    for (field, value) in cases {
        let mut request = query("documents");
        request[field] = value;
        let (status, body) = post(&database, "/v1/vector/search", request).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{field}: {body}");
    }
    let (status, body) = post(&database, "/v1/vector/search", query("documents")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["rows"], json!([]));
    assert_eq!(body["row_count"], 0);
}

#[actix_web::test]
async fn repeated_selected_columns_cannot_amplify_search_responses() {
    let database = Database::new();
    database
        .execute(
            "CREATE TABLE documents (id INTEGER, embedding VECTOR(3));
             INSERT INTO documents VALUES (1, ARRAY[1, 0, 0]);",
        )
        .unwrap();
    let revision = database.revision().unwrap();
    for select in [
        json!(["id", "ID"]),
        json!(["embedding", "embedding"]),
        json!(vec!["id"; 1_024]),
    ] {
        let mut request = query("documents");
        request["select"] = select;
        let (status, body) = post(&database, "/v1/vector/search", request).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"]["code"], "duplicate_column");
        assert_eq!(database.revision().unwrap(), revision);
    }
}

#[actix_web::test]
async fn sql_http_response_preserves_mixed_results_and_exact_json_value_types() {
    let database = Database::new();
    database.execute(
        "CREATE TABLE documents (id INTEGER PRIMARY KEY, title TEXT, active BOOLEAN, amount DOUBLE, embedding VECTOR(3))"
    ).unwrap();
    let title = "A \"quoted\" line\n雪";
    let (status, body) = post(
        &database,
        "/v1/tables/documents/rows",
        json!({"rows": [
            {"id": i64::MIN, "title": title, "active": true, "amount": 1.25, "embedding": [0.1, -0.0, f64::from(f32::MAX)]},
            {"id": i64::MAX, "title": null, "active": false, "amount": null, "embedding": null}
        ]}),
    ).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(database))
            .configure(api::configure),
    )
    .await;
    let response = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/v1/sql")
            .set_json(json!({"sql": "UPDATE documents SET active = FALSE; SELECT * FROM documents ORDER BY id"}))
            .to_request(),
    ).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/json"
    );
    let body: Value = test::read_body_json(response).await;
    assert_eq!(body["results"].as_array().unwrap().len(), 2);
    assert_eq!(
        body["results"][0],
        json!({"type": "command", "tag": "UPDATE", "rows_affected": 2})
    );
    let result = &body["results"][1];
    assert_eq!(result["type"], "query");
    assert_eq!(
        result["schema"],
        json!([
            {"name": "id", "data_type": "INTEGER"},
            {"name": "title", "data_type": "TEXT"},
            {"name": "active", "data_type": "BOOLEAN"},
            {"name": "amount", "data_type": "DOUBLE"},
            {"name": "embedding", "data_type": "VECTOR(3)"}
        ])
    );
    let returned_max = result["rows"][0][4][2].as_f64().unwrap();
    let expected_max = f64::from(f32::MAX);
    assert!((returned_max - expected_max).abs() <= f64::EPSILON * expected_max);
    assert_eq!(returned_max as f32, f32::MAX);
    // Compare the parsed golden JSON number, allowing serde_json's default
    // decimal parser to round it in the same way as an HTTP consumer.
    let max_json: Value = serde_json::from_str("3.4028234663852886e+38").unwrap();
    assert_eq!(
        result["rows"],
        json!([
            [
                i64::MIN,
                title,
                false,
                1.25,
                [f64::from(0.1_f32), -0.0, max_json]
            ],
            [i64::MAX, null, false, null, null]
        ])
    );
    assert_eq!(result["row_count"], 2);
    assert_eq!(result["rows_examined"], 2);
}
