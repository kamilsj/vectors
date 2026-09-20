use super::*;
use crate::embedding::{
    tests::{configured, mock_provider},
    Provider,
};
use actix_web::{http::Method, test, App};
use serde_json::json;

fn fixture() -> Database {
    let db = Database::new();
    let profile = GraphEmbeddingProfile {
        provider: "openai".into(),
        model: "text-embedding-3-small".into(),
        dimensions: 2,
        context_format_version: 1,
    };
    db.graph_create_collection(GraphCollectionConfig {
        name: "notes".into(),
        profile: profile.clone(),
        semantic_neighbors: 2,
        semantic_threshold: 0.5,
    })
    .unwrap();
    for (id, title, text, vector) in [
        (
            "one",
            "Storage",
            "Database storage and recovery.",
            vec![1.0, 0.0],
        ),
        (
            "two",
            "Error codes",
            "ZX419 means the journal is full.",
            vec![0.6, 0.8],
        ),
        (
            "three",
            "Guidance",
            "Rotate the journal before retrying writes.",
            vec![0.9, 0.1],
        ),
    ] {
        db.graph_ingest_document(GraphIngestRequest {
            collection: "notes".into(),
            expected_revision: db.revision().unwrap(),
            expected_profile: profile.clone(),
            document: GraphDocumentInput {
                id: id.into(),
                title: title.into(),
                source: format!("manual/{id}.md"),
                text: text.into(),
                metadata: json!({"topic":"database"}),
                chunking: json!({}),
                chunks: vec![GraphChunkInput {
                    start_byte: 0,
                    end_byte: text.len(),
                    text: text.into(),
                    embedding_text: format!("Title: {title}\n\n{text}"),
                    embedding: Vector::new(vector).unwrap().normalized().unwrap(),
                }],
            },
        })
        .unwrap();
    }
    db
}

async fn call(
    db: &Database,
    embeddings: &EmbeddingService,
    reranking: Option<&RerankingService>,
    method: Method,
    path: &str,
    body: JsonValue,
) -> (StatusCode, JsonValue) {
    let mut app = App::new()
        .app_data(web::Data::new(db.clone()))
        .app_data(web::Data::new(embeddings.clone()));
    if let Some(service) = reranking {
        app = app.app_data(web::Data::new(service.clone()));
    }
    let app = test::init_service(app.configure(crate::api::configure)).await;
    let response = test::call_service(
        &app,
        test::TestRequest::default()
            .method(method)
            .uri(path)
            .set_json(body)
            .to_request(),
    )
    .await;
    (response.status(), test::read_body_json(response).await)
}

fn embedding_response() -> JsonValue {
    json!({"data":[{"index":0,"embedding":[1.0,0.0]}],"usage":{"total_tokens":4}})
}

fn query() -> JsonValue {
    json!({"text":"ZX419 journal", "candidate_limit":10,"seed_limit":3,
        "max_results":3,"max_hops":1,"diversity":0.0})
}

fn traversal_query() -> JsonValue {
    json!({"text":"ZX419", "candidate_limit":4, "seed_limit":1,
        "max_results":4, "max_hops":1, "neighbor_limit":1,
        "vector_weight":0, "diversity":0})
}

fn add_traversal_link(db: &Database, from: &str, to: &str, kind: &str, weight: f64) {
    db.graph_upsert_relationship(GraphRelationshipRequest {
        collection: "notes".into(),
        expected_revision: db.revision().unwrap(),
        from_chunk: from.into(),
        to_chunk: to.into(),
        kind: kind.into(),
        weight,
    })
    .unwrap();
}

#[actix_web::test]
async fn retrieval_incoming_control_discovers_reverse_links_and_preserves_default_direction() {
    let db = fixture();
    add_traversal_link(&db, "3:one:0", "3:two:0", "supports", 0.8);
    let (endpoint, mock) = mock_provider(3, |_, _, _| (200, embedding_response()));
    let embeddings = configured(&endpoint, Provider::Openai);
    for direction in [None, Some("incoming"), Some("both")] {
        let mut request = traversal_query();
        request["kind"] = json!("supports");
        if let Some(direction) = direction {
            request["direction"] = json!(direction);
        }
        let (status, result) = call(
            &db,
            &embeddings,
            None,
            Method::POST,
            "/v1/graph/collections/notes/retrieve",
            request,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{result}");
        let hits = result["hits"].as_array().unwrap();
        let seed = hits.iter().find(|hit| hit["document_id"] == "two").unwrap();
        assert!(seed.get("retrieval_path").is_none());
        if direction.is_none() {
            assert_eq!(hits.len(), 1);
            assert_eq!(result["edges"], json!([]));
        } else {
            assert_eq!(hits.len(), 2);
            let context = hits.iter().find(|hit| hit["document_id"] == "one").unwrap();
            assert_eq!(context["depth"], 1);
            assert_eq!(
                context["retrieval_path"],
                json!({
                    "seed_chunk_id":"3:two:0", "edges":[{
                        "from_chunk":"3:one:0", "to_chunk":"3:two:0",
                        "kind":"supports", "weight":0.8
                    }]
                })
            );
            assert_eq!(result["edges"], context["retrieval_path"]["edges"]);
        }
    }
    mock.join().unwrap();
}

#[actix_web::test]
async fn retrieval_filters_relationships_before_neighbor_budget_and_returned_edges() {
    let db = fixture();
    add_traversal_link(&db, "3:two:0", "3:one:0", "unwanted", 1.0);
    add_traversal_link(&db, "3:two:0", "3:one:0", "supports", 0.2);
    add_traversal_link(&db, "3:two:0", "5:three:0", "supports", 0.8);
    add_traversal_link(&db, "5:three:0", "3:two:0", "supports", 0.1);
    let (endpoint, mock) = mock_provider(1, |_, _, _| (200, embedding_response()));
    let embeddings = configured(&endpoint, Provider::Openai);
    let mut request = traversal_query();
    request["kind"] = json!("supports");
    request["min_weight"] = json!(0.8);
    let (status, result) = call(
        &db,
        &embeddings,
        None,
        Method::POST,
        "/v1/graph/collections/notes/retrieve",
        request,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let hits = result["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 2);
    assert!(hits.iter().any(|hit| hit["document_id"] == "two"));
    assert!(hits.iter().any(|hit| hit["document_id"] == "three"));
    assert!(!hits.iter().any(|hit| hit["document_id"] == "one"));
    assert_eq!(
        result["edges"],
        json!([{
            "from_chunk":"3:two:0", "to_chunk":"5:three:0",
            "kind":"supports", "weight":0.8
        }])
    );
    mock.join().unwrap();
}

#[actix_web::test]
async fn invalid_retrieval_relationship_controls_fail_before_provider_work() {
    let db = fixture();
    let embeddings = configured("http://127.0.0.1:1", Provider::Openai);
    for invalid in [
        json!({"direction":"sideways"}),
        json!({"min_weight":-0.1}),
        json!({"min_weight":1.1}),
        json!({"kind":""}),
        json!({"kind":"not-a-label"}),
        json!({"kind":"UPPER"}),
        json!({"kind":"a".repeat(65)}),
    ] {
        let mut request = traversal_query();
        for (field, value) in invalid.as_object().unwrap() {
            request[field] = value.clone();
        }
        let (status, result) = call(
            &db,
            &embeddings,
            None,
            Method::POST,
            "/v1/graph/collections/notes/retrieve",
            request,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{invalid}: {result}");
        assert!(!result["error"]["code"]
            .as_str()
            .unwrap()
            .starts_with("embedding_"));
    }
}

#[actix_web::test]
async fn browse_and_relationship_edits_are_provider_free_and_revision_guarded() {
    let db = fixture();
    let embeddings = configured("http://127.0.0.1:1", Provider::Openai);
    let (status, page) = call(
        &db,
        &embeddings,
        None,
        Method::GET,
        "/v1/graph/collections/notes/graph?limit=2&max_edges=10",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(page["total_nodes"], 3);
    assert_eq!(page["truncated"], true);
    let before = db.revision().unwrap();
    let link = json!({"expected_revision":before,"from_chunk":"3:one:0",
        "to_chunk":"3:two:0","kind":"explains","weight":0.9});
    let path = "/v1/graph/collections/notes/relationships";
    let (status, created) = call(&db, &embeddings, None, Method::POST, path, link.clone()).await;
    assert_eq!(status, StatusCode::OK, "{created}");
    assert_eq!(created["edge"]["kind"], "explains");
    assert_eq!(created["created"], true);
    let (status, conflict) = call(&db, &embeddings, None, Method::POST, path, link).await;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");
    assert_eq!(conflict["error"]["code"], "stale_revision");
    let (status, removed) = call(
        &db,
        &embeddings,
        None,
        Method::DELETE,
        path,
        json!({"expected_revision":created["revision"],"from_chunk":"3:one:0",
            "to_chunk":"3:two:0","kind":"explains"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{removed}");
    assert_eq!(removed["edges_removed"], 1);
    let sql = db
        .execute("SELECT kind FROM graph_notes_edges WHERE kind = 'explains'")
        .unwrap();
    let ExecutionResult::Query(rows) = &sql[0] else {
        panic!("expected query result")
    };
    assert!(rows.rows.is_empty());
}

#[actix_web::test]
async fn local_hybrid_retrieval_recovers_keywords_with_consistent_repeated_results() {
    let db = fixture();
    let (endpoint, mock) = mock_provider(2, |_, _, _| (200, embedding_response()));
    let embeddings = configured(&endpoint, Provider::Openai);
    let mut request = query();
    request["text"] = json!("ZX419");
    request["vector_weight"] = json!(0);
    request["lexical_weight"] = json!(1);
    request["max_hops"] = json!(0);
    let mut first = None;
    for _ in 0..2 {
        let (status, result) = call(
            &db,
            &embeddings,
            None,
            Method::POST,
            "/v1/graph/collections/notes/retrieve",
            request.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{result}");
        assert_eq!(result["hits"][0]["document_id"], "two");
        assert!(result["hits"][0]["lexical_score"].as_f64().unwrap() > 0.0);
        assert_eq!(result["reranking"]["method"], "local");
        assert!(result["hits"][0]["rerank_score"].is_null());
        assert_eq!(result["embedding_usage"]["total_tokens"], 4);
        // Other parallel API tests may evict this bounded, process-wide cache.
        // Isolated engine tests and the benchmark assert cold/warm cache hits.
        assert!(result["lexical_cache_hit"].is_boolean());
        if let Some(first) = &first {
            assert_eq!(&result["hits"], first);
        } else {
            first = Some(result["hits"].clone());
        }
    }
    mock.join().unwrap();
}

#[actix_web::test]
async fn voyage_reranks_one_snapshot_even_when_database_changes_during_provider_call() {
    let db = fixture();
    let initial_revision = db.revision().unwrap();
    let write_db = db.clone();
    let (endpoint, embedding_mock) = mock_provider(1, |_, _, _| (200, embedding_response()));
    let embeddings = configured(&endpoint, Provider::Openai);
    let (endpoint, reranking_mock) = crate::reranking::tests::mock_provider(
        1,
        move |_, body, _| {
            assert_eq!(body["truncation"], false);
            let documents = body["documents"].as_array().unwrap();
            let data = documents.iter().enumerate().rev().map(|(index, document)| {
            json!({"index":index,"relevance_score":if document.as_str().unwrap().contains("Rotate") {0.99} else {0.1}})
        }).collect::<Vec<_>>();
            write_db.execute("UPDATE graph_notes_documents SET title = 'Changed after retrieval' WHERE document_id = 'three'").unwrap();
            (200, json!({"data":data,"usage":{"total_tokens":50}}))
        },
    );
    let reranker = crate::reranking::tests::configured(&endpoint);
    let mut request = query();
    request["reranker"] = json!("voyage");
    let (status, result) = call(
        &db,
        &embeddings,
        Some(&reranker),
        Method::POST,
        "/v1/graph/collections/notes/retrieve",
        request,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["hits"][0]["document_id"], "three");
    assert_eq!(result["hits"][0]["title"], "Guidance");
    assert_eq!(result["hits"][0]["rerank_score"], 0.99);
    assert_eq!(result["revision"], initial_revision);
    assert!(db.revision().unwrap() > initial_revision);
    assert_eq!(result["reranking"]["model"], "rerank-2.5");
    assert_eq!(result["reranking"]["total_tokens"], 50);
    embedding_mock.join().unwrap();
    reranking_mock.join().unwrap();
}

#[actix_web::test]
async fn invalid_requests_and_unconfigured_reranker_fail_before_query_embedding() {
    let db = fixture();
    let embeddings = configured("http://127.0.0.1:1", Provider::Openai);
    for bad in [
        json!({"candidate_limit":0}),
        json!({"max_context_bytes":0}),
        json!({"vector_weight":0,"lexical_weight":0}),
        json!({"diversity":1.1}),
    ] {
        let mut request = query();
        for (key, value) in bad.as_object().unwrap() {
            request[key] = value.clone();
        }
        let (status, result) = call(
            &db,
            &embeddings,
            None,
            Method::POST,
            "/v1/graph/collections/notes/retrieve",
            request,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{result}");
        assert_eq!(result["error"]["code"], "invalid_rag_request");
    }
    let mut request = query();
    request["reranker"] = json!("voyage");
    let (status, result) = call(
        &db,
        &embeddings,
        None,
        Method::POST,
        "/v1/graph/collections/notes/retrieve",
        request,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{result}");
    assert_eq!(result["error"]["code"], "reranking_unavailable");
    let mut request = query();
    request["text"] = json!((0..257)
        .map(|index| format!("term{index}"))
        .collect::<Vec<_>>()
        .join(" "));
    let (status, result) = call(
        &db,
        &embeddings,
        None,
        Method::POST,
        "/v1/graph/collections/notes/retrieve",
        request,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{result}");
    assert!(result["error"]["message"].as_str().unwrap().contains("256"));
}

#[actix_web::test]
async fn empty_rag_collection_needs_no_provider_and_has_zero_usage() {
    let db = fixture();
    let embeddings = configured("http://127.0.0.1:1", Provider::Openai);
    let mut config = db.graph_collection("notes").unwrap().config;
    config.name = "empty".into();
    db.graph_create_collection(config).unwrap();
    let mut request = query();
    request["reranker"] = json!("voyage");
    let (status, result) = call(
        &db,
        &embeddings,
        None,
        Method::POST,
        "/v1/graph/collections/empty/retrieve",
        request,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["candidate_count"], 0);
    assert_eq!(result["hits"], json!([]));
    assert_eq!(result["embedding_usage"]["total_tokens"], 0);
    assert_eq!(result["reranking"]["total_tokens"], 0);
}

#[actix_web::test]
async fn provider_failure_is_explicit_without_silent_local_fallback() {
    let db = fixture();
    let revision = db.revision().unwrap();
    let (endpoint, embedding_mock) = mock_provider(1, |_, _, _| (200, embedding_response()));
    let embeddings = configured(&endpoint, Provider::Openai);
    let (endpoint, reranking_mock) = crate::reranking::tests::mock_provider(1, |_, _, _| {
        (401, json!({"message":"private-upstream-details"}))
    });
    let reranker = crate::reranking::tests::configured(&endpoint);
    let mut request = query();
    request["reranker"] = json!("voyage");
    let (status, result) = call(
        &db,
        &embeddings,
        Some(&reranker),
        Method::POST,
        "/v1/graph/collections/notes/retrieve",
        request,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{result}");
    assert_eq!(result["error"]["code"], "reranking_auth_failed");
    assert!(!result.to_string().contains("private-upstream-details"));
    assert_eq!(db.revision().unwrap(), revision);
    embedding_mock.join().unwrap();
    reranking_mock.join().unwrap();
}

#[actix_web::test]
async fn new_graph_routes_and_reranking_settings_require_authentication() {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(fixture()))
            .app_data(web::Data::new(ApiSecurity::bearer_token("test-admin")))
            .configure(crate::api::configure),
    )
    .await;
    for (method, path, payload) in [
        (Method::GET, "/v1/graph/collections/notes/graph", json!({})),
        (
            Method::GET,
            "/v1/graph/collections/notes/neighborhood?chunk_id=3%3Aone%3A0",
            json!({}),
        ),
        (
            Method::POST,
            "/v1/graph/collections/notes/retrieve",
            query(),
        ),
        (
            Method::POST,
            "/v1/graph/collections/notes/relationships",
            json!({"expected_revision":0,"from_chunk":"a","to_chunk":"b","kind":"ref","weight":1}),
        ),
        (
            Method::DELETE,
            "/v1/graph/collections/notes/relationships",
            json!({"expected_revision":0,"from_chunk":"a","to_chunk":"b","kind":"ref"}),
        ),
        (Method::GET, "/v1/settings/reranking", json!({})),
        (
            Method::PUT,
            "/v1/settings/reranking",
            json!({"api_key":"synthetic-secret"}),
        ),
    ] {
        let response = test::call_service(
            &app,
            test::TestRequest::default()
                .method(method)
                .uri(path)
                .set_json(payload)
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
    }
}

#[actix_web::test]
async fn reranking_settings_never_return_credentials() {
    let service = crate::reranking::tests::test_service(None);
    let db = fixture();
    let embeddings = configured("http://127.0.0.1:1", Provider::Openai);
    let (status, settings) = call(
        &db,
        &embeddings,
        Some(&service),
        Method::PUT,
        "/v1/settings/reranking",
        json!({"api_key":"synthetic-secret","model":"rerank-2.5-lite"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{settings}");
    assert_eq!(settings["configured"], true);
    assert_eq!(settings["model"], "rerank-2.5-lite");
    assert!(!settings.to_string().contains("synthetic-secret"));
    let (status, settings) = call(
        &db,
        &embeddings,
        Some(&service),
        Method::GET,
        "/v1/settings/reranking",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(settings.get("api_key").is_none());
}

#[actix_web::test]
async fn retrieval_bounds_relationships_by_the_configured_response_limit() {
    let db = fixture();
    for kind in ["cites", "explains", "supports"] {
        db.graph_upsert_relationship(GraphRelationshipRequest {
            collection: "notes".into(),
            expected_revision: db.revision().unwrap(),
            from_chunk: "3:one:0".into(),
            to_chunk: "5:three:0".into(),
            kind: kind.into(),
            weight: 1.0,
        })
        .unwrap();
    }
    let (endpoint, mock) = mock_provider(1, |_, _, _| (200, embedding_response()));
    let limits = RequestLimits::new(4_096, 10, 2).unwrap();
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(db))
            .app_data(web::Data::new(configured(&endpoint, Provider::Openai)))
            .configure(move |services| crate::api::configure_with_limits(services, limits)),
    )
    .await;
    // Both candidates fit every other budget. Only the edge cap can make this
    // snapshot truncated: multiple relationship kinds connect the same pair.
    let response = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/v1/graph/collections/notes/retrieve")
            .set_json(json!({
                "text":"storage", "candidate_limit":2, "seed_limit":2,
                "max_results":2, "max_hops":0, "neighbor_limit":32,
                "lexical_weight":0, "diversity":0
            }))
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let result: JsonValue = test::read_body_json(response).await;
    assert_eq!(result["candidate_count"], 2);
    assert_eq!(result["hits"].as_array().unwrap().len(), 2);
    assert_eq!(result["edges"].as_array().unwrap().len(), 2);
    assert_eq!(result["truncated"], true);
    let selected = result["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["chunk_id"].as_str().unwrap())
        .collect::<Vec<_>>();
    for edge in result["edges"].as_array().unwrap() {
        assert!(selected.contains(&edge["from_chunk"].as_str().unwrap()));
        assert!(selected.contains(&edge["to_chunk"].as_str().unwrap()));
    }
    mock.join().unwrap();
}

#[actix_web::test]
async fn browse_clamps_default_page_sizes_and_reports_the_effective_limit() {
    let limits = RequestLimits::new(4_096, 10, 2).unwrap();
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(fixture()))
            .configure(move |services| crate::api::configure_with_limits(services, limits)),
    )
    .await;
    let response = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/v1/graph/collections/notes/graph?limit=100")
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let first: JsonValue = test::read_body_json(response).await;
    assert_eq!(first["limit"], 2);
    assert_eq!(first["offset"], 0);
    assert_eq!(first["total_nodes"], 3);
    assert_eq!(first["nodes"].as_array().unwrap().len(), 2);
    assert!(first["edges"].as_array().unwrap().len() <= 2);
    assert_eq!(first["truncated"], true);

    let next_offset = first["offset"].as_u64().unwrap() + first["limit"].as_u64().unwrap();
    let response = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!(
                "/v1/graph/collections/notes/graph?limit=100&offset={next_offset}"
            ))
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let second: JsonValue = test::read_body_json(response).await;
    assert_eq!(second["limit"], 2);
    assert_eq!(second["offset"], 2);
    assert_eq!(second["nodes"].as_array().unwrap().len(), 1);
    assert!(!first["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|node| { node["chunk_id"] == second["nodes"][0]["chunk_id"] }));

    for limit in [0, 201] {
        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/v1/graph/collections/notes/graph?limit={limit}"))
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "limit={limit}");
    }
}

#[actix_web::test]
async fn focused_neighborhood_is_provider_free_filtered_and_bounded_by_server_limits() {
    let db = fixture();
    db.graph_upsert_relationship(GraphRelationshipRequest {
        collection: "notes".into(),
        expected_revision: db.revision().unwrap(),
        from_chunk: "3:one:0".into(),
        to_chunk: "3:two:0".into(),
        kind: "explains".into(),
        weight: 1.0,
    })
    .unwrap();
    let revision = db.revision().unwrap();
    let limits = RequestLimits::new(4096, 10, 2).unwrap();
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(db.clone()))
            .configure(move |services| crate::api::configure_with_limits(services, limits)),
    )
    .await;
    let response = test::call_service(&app, test::TestRequest::get()
        .uri("/v1/graph/collections/notes/neighborhood?chunk_id=3%3Aone%3A0&direction=outgoing&kind=explains&min_weight=1&max_nodes=100")
        .to_request()).await;
    assert_eq!(response.status(), StatusCode::OK);
    let result: JsonValue = test::read_body_json(response).await;
    assert_eq!(result["root_chunk"], "3:one:0");
    assert_eq!(result["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(result["nodes"][0]["chunk_id"], "3:one:0");
    assert_eq!(result["nodes"][0]["depth"], 0);
    assert_eq!(result["nodes"][1]["chunk_id"], "3:two:0");
    assert_eq!(result["nodes"][1]["depth"], 1);
    assert_eq!(result["edges"].as_array().unwrap().len(), 1);
    assert_eq!(result["edges"][0]["kind"], "explains");
    assert_eq!(result["revision"], revision);
    assert_eq!(db.revision().unwrap(), revision);
    for suffix in [
        "max_nodes=201",
        "max_nodes=0",
        "max_hops=4",
        "min_weight=1.1",
        "neighbor_limit=0",
    ] {
        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!(
                    "/v1/graph/collections/notes/neighborhood?chunk_id=3%3Aone%3A0&{suffix}"
                ))
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{suffix}");
    }
}
