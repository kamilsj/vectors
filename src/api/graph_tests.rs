use super::*;
use actix_web::{http::Method, test, App};
use serde_json::json;
use std::sync::Mutex;

use crate::embedding::tests::{configured, mock_provider};
use crate::embedding::Provider;

async fn call(
    database: &Database,
    embeddings: &EmbeddingService,
    method: Method,
    path: &str,
    body: JsonValue,
) -> (StatusCode, JsonValue) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(database.clone()))
            .app_data(web::Data::new(embeddings.clone()))
            .configure(crate::api::configure),
    )
    .await;
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

async fn create(database: &Database, embeddings: &EmbeddingService) -> JsonValue {
    let (status, body) = call(
        database,
        embeddings,
        Method::POST,
        "/v1/graph/collections",
        json!({"name":"notes", "semantic_neighbors":2, "semantic_threshold":0.8}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

fn provider_response(input: &JsonValue, dimensions: usize) -> JsonValue {
    let data = input
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .rev()
        .map(|(index, _)| {
            let mut embedding = vec![0.0; dimensions];
            embedding[0] = 3.0;
            embedding[1] = 4.0;
            json!({"index":index,"embedding":embedding})
        })
        .collect::<Vec<_>>();
    json!({"data":data,"usage":{"total_tokens":input.as_array().unwrap().len()}})
}

fn snapshot(database: &Database) -> Vec<ExecutionResult> {
    database.execute(
        "SELECT * FROM graph_notes_config; SELECT * FROM graph_notes_documents ORDER BY document_id; \
         SELECT * FROM graph_notes_chunks ORDER BY chunk_id; SELECT * FROM graph_notes_edges ORDER BY edge_id"
    ).unwrap()
}

fn first_document() -> JsonValue {
    json!({
        "id":"one", "title":"Field notes", "source":"https://example.test/one",
        "text":"\n\n# Alpha\nVectors in Łódź retain their source.\n\n# Beta\nGraphs connect useful passages.",
        "metadata":{"category":"integration"}
    })
}

#[actix_web::test]
async fn preview_ingest_sql_search_replay_and_delete_form_one_consistent_workflow() {
    let database = Database::new();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let seen = calls.clone();
    let (endpoint, mock) = mock_provider(3, move |index, body, _| {
        assert_eq!(body["model"], "voyage-4");
        assert_eq!(body["truncation"], false);
        assert_eq!(
            body["input_type"],
            if index == 2 { "query" } else { "document" }
        );
        assert_eq!(
            body["input"].as_array().unwrap().len(),
            if index == 0 { 2 } else { 1 }
        );
        seen.lock().unwrap().push(body.clone());
        (200, provider_response(&body["input"], 256))
    });
    let embeddings = configured(&endpoint, Provider::Voyage);
    let document = first_document();
    let (status, preview) = call(
        &database,
        &embeddings,
        Method::POST,
        "/v1/graph/chunk",
        json!({"text":document["text"],"title":document["title"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(preview["chunks"].as_array().unwrap().len(), 2);
    let source = document["text"].as_str().unwrap();
    for (ordinal, chunk) in preview["chunks"].as_array().unwrap().iter().enumerate() {
        let start = chunk["byte_start"].as_u64().unwrap() as usize;
        let end = chunk["byte_end"].as_u64().unwrap() as usize;
        assert_eq!(chunk["ordinal"], ordinal);
        assert_eq!(&source[start..end], chunk["text"].as_str().unwrap());
        assert!(chunk["embedding_text"]
            .as_str()
            .unwrap()
            .starts_with("Title: Field notes\nSection: "));
        assert!(chunk["embedding_text"].as_str().unwrap().len() <= 8191);
    }
    assert!(calls.lock().unwrap().is_empty());
    let created = create(&database, &embeddings).await;
    assert_eq!(
        created["config"]["profile"],
        json!({"provider":"voyage","model":"voyage-4","dimensions":256,"context_format_version":1})
    );
    assert_eq!(created["tables"]["chunks"], "graph_notes_chunks");
    assert!(calls.lock().unwrap().is_empty());

    let (status, inserted) = call(
        &database,
        &embeddings,
        Method::POST,
        "/v1/graph/collections/notes/documents",
        document.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{inserted}");
    assert_eq!(inserted["chunks"], 2);
    assert_eq!(inserted["edges_created"], 2);
    assert_eq!(inserted["unchanged"], false);
    assert_eq!(inserted["embedding_usage"]["total_tokens"], 2);
    let expected_input = preview["chunks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|chunk| chunk["embedding_text"].clone())
        .collect::<Vec<_>>();
    assert_eq!(calls.lock().unwrap()[0]["input"], json!(expected_input));

    let (status, second) = call(&database, &embeddings, Method::POST, "/v1/graph/collections/notes/documents", json!({
        "id":"two", "title":"Related notes", "source":"https://example.test/two", "text":"Another document about useful vector search."
    })).await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(second["chunks"], 1);
    assert_eq!(second["edges_created"], 4);

    let (status, sql) = call(&database, &embeddings, Method::POST, "/v1/sql", json!({"sql":
        "SELECT document_id, ordinal, start_byte, end_byte, text, embedding_text, embedding_profile, embedding FROM graph_notes_chunks WHERE document_id = 'one' ORDER BY ordinal; SELECT kind FROM graph_notes_edges ORDER BY edge_id"
    })).await;
    assert_eq!(status, StatusCode::OK, "{sql}");
    let rows = sql["results"][0]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    for (index, row) in rows.iter().enumerate() {
        let chunk = &preview["chunks"][index];
        assert_eq!(row[0], "one");
        assert_eq!(row[1], index);
        assert_eq!(row[2], chunk["byte_start"]);
        assert_eq!(row[3], chunk["byte_end"]);
        assert_eq!(row[4], chunk["text"]);
        assert_eq!(row[5], chunk["embedding_text"]);
        assert_eq!(
            serde_json::from_str::<JsonValue>(row[6].as_str().unwrap()).unwrap(),
            created["config"]["profile"]
        );
        let vector = row[7].as_array().unwrap();
        assert_eq!(vector.len(), 256);
        let norm = vector
            .iter()
            .map(|value| value.as_f64().unwrap().powi(2))
            .sum::<f64>()
            .sqrt();
        assert!((norm - 1.0).abs() < 1e-6);
    }
    let edges = sql["results"][1]["rows"].as_array().unwrap();
    assert_eq!(edges.len(), 6);
    assert_eq!(edges.iter().filter(|row| row[0] == "semantic").count(), 4);

    let (status, found) = call(&database, &embeddings, Method::POST, "/v1/graph/collections/notes/search", json!({
        "text":"find related graph context", "seed_limit":1,"max_hops":1,"neighbor_limit":8,"max_results":3
    })).await;
    assert_eq!(status, StatusCode::OK, "{found}");
    assert_eq!(found["hits"].as_array().unwrap().len(), 3);
    assert_eq!(found["hits"][0]["seed"], true);
    assert_eq!(found["hits"][0]["depth"], 0);
    assert_eq!(
        found["hits"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|hit| hit["seed"] == false)
            .count(),
        2
    );
    for hit in found["hits"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|hit| hit["document_id"] == "one")
    {
        let start = hit["start_byte"].as_u64().unwrap() as usize;
        let end = hit["end_byte"].as_u64().unwrap() as usize;
        assert_eq!(hit["text"], &source[start..end]);
        assert_eq!(hit["source"], document["source"]);
        assert_eq!(hit["metadata"], document["metadata"]);
    }
    assert_eq!(
        calls.lock().unwrap()[2]["input"],
        json!(["find related graph context"])
    );
    mock.join().unwrap();

    // The mock is closed: even an attempted provider call makes replay fail.
    let before = snapshot(&database);
    let revision = database.revision().unwrap();
    let (status, replay) = call(
        &database,
        &embeddings,
        Method::POST,
        "/v1/graph/collections/notes/documents",
        document.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay["unchanged"], true);
    assert_eq!(replay["embedding_usage"]["total_tokens"], 0);
    assert_eq!(database.revision().unwrap(), revision);
    assert_eq!(snapshot(&database), before);
    assert_eq!(calls.lock().unwrap().len(), 3);

    let (status, stored) = call(
        &database,
        &embeddings,
        Method::GET,
        "/v1/graph/collections/notes/documents/one",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{stored}");
    assert_eq!(stored["text"], document["text"]);
    assert_eq!(stored["chunk_count"], 2);
    let (status, deleted) = call(
        &database,
        &embeddings,
        Method::DELETE,
        "/v1/graph/collections/notes/documents/one",
        json!({"expected_revision":revision}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    assert_eq!(deleted["chunks_removed"], 2);
    assert_eq!(deleted["edges_removed"], 6);
    let remaining = database.graph_collection("notes").unwrap();
    assert_eq!(
        (
            remaining.document_count,
            remaining.chunk_count,
            remaining.edge_count
        ),
        (1, 1, 0)
    );
    let after = snapshot(&database);
    let ExecutionResult::Query(chunks) = &after[2] else {
        panic!("expected chunks")
    };
    assert_eq!(chunks.rows.len(), 1);
    assert_eq!(chunks.rows[0][1], Value::Text("two".into()));
    let ExecutionResult::Query(edges) = &after[3] else {
        panic!("expected edges")
    };
    assert!(edges.rows.is_empty());
    let (status, _) = call(
        &database,
        &embeddings,
        Method::GET,
        "/v1/graph/collections/notes/documents/one",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(calls.lock().unwrap().len(), 3);
}

#[actix_web::test]
async fn collection_profile_mismatch_rejects_ingest_and_query_before_provider_calls() {
    let database = Database::new();
    let (endpoint, mock) =
        mock_provider(1, |_, body, _| (200, provider_response(&body["input"], 2)));
    let embeddings = configured(&endpoint, Provider::Openai);
    create(&database, &embeddings).await;
    let (status, body) = call(
        &database,
        &embeddings,
        Method::POST,
        "/v1/graph/collections/notes/documents",
        first_document(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    mock.join().unwrap();
    let (status, body) = call(
        &database,
        &embeddings,
        Method::PUT,
        "/v1/settings/embeddings",
        json!({"dimensions":3}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let before = snapshot(&database);
    let revision = database.revision().unwrap();
    for (path, body) in [
        ("/v1/graph/collections/notes/documents", {
            let mut revised = first_document();
            revised["title"] = json!("Revised document");
            revised
        }),
        (
            "/v1/graph/collections/notes/search",
            json!({"text":"query"}),
        ),
    ] {
        let (status, body) = call(&database, &embeddings, Method::POST, path, body).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"]["code"], "embedding_settings_changed");
        assert_eq!(snapshot(&database), before);
        assert_eq!(database.revision().unwrap(), revision);
    }
}

#[actix_web::test]
async fn failed_provider_replacement_leaves_documents_chunks_edges_and_revision_unchanged() {
    let database = Database::new();
    let (endpoint, mock) = mock_provider(2, |index, body, _| {
        if index == 0 {
            (200, provider_response(&body["input"], 2))
        } else {
            (503, json!({"error":"synthetic-provider-secret"}))
        }
    });
    let embeddings = configured(&endpoint, Provider::Openai);
    create(&database, &embeddings).await;
    let (status, body) = call(
        &database,
        &embeddings,
        Method::POST,
        "/v1/graph/collections/notes/documents",
        first_document(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let before = snapshot(&database);
    let revision = database.revision().unwrap();
    let mut changed = first_document();
    changed["title"] = json!("Revised source title");
    let (status, body) = call(
        &database,
        &embeddings,
        Method::POST,
        "/v1/graph/collections/notes/documents",
        changed,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(body["error"]["code"], "embedding_unavailable");
    assert!(!body.to_string().contains("synthetic-provider-secret"));
    assert_eq!(snapshot(&database), before);
    assert_eq!(database.revision().unwrap(), revision);
    mock.join().unwrap();
}

#[actix_web::test]
async fn unchanged_upload_repairs_sql_edits_to_chunks_and_document_title_context() {
    let database = Database::new();
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let calls = provider_calls.clone();
    let (endpoint, mock) = mock_provider(4, move |index, body, _| {
        calls.fetch_add(1, Ordering::Relaxed);
        for text in body["input"].as_array().unwrap() {
            assert!(!text.as_str().unwrap().contains("corrupted generated chunk"));
            if index == 3 {
                assert!(text
                    .as_str()
                    .unwrap()
                    .starts_with("Title: Revised through SQL\n"));
            }
        }
        (200, provider_response(&body["input"], 2))
    });
    let embeddings = configured(&endpoint, Provider::Openai);
    create(&database, &embeddings).await;
    let mut document = first_document();
    let (status, body) = call(
        &database,
        &embeddings,
        Method::POST,
        "/v1/graph/collections/notes/documents",
        document.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let pristine = snapshot(&database);
    assert!(
        database
            .graph_document("notes", "one")
            .unwrap()
            .unwrap()
            .chunks_intact
    );

    for (index, sql) in [
        "UPDATE graph_notes_chunks SET text = 'corrupted generated chunk' WHERE document_id = 'one' AND ordinal = 0",
        "UPDATE graph_notes_chunks SET embedding = ARRAY[1,0] WHERE document_id = 'one' AND ordinal = 0",
    ].iter().enumerate() {
        database.execute(sql).unwrap();
        let changed_revision = database.revision().unwrap();
        let stored = database.graph_document("notes", "one").unwrap().unwrap();
        assert_eq!(stored.text, document["text"].as_str().unwrap());
        assert_eq!(stored.chunk_count, 2);
        assert!(!stored.chunks_intact);
        let (status, repaired) = call(
            &database, &embeddings, Method::POST,
            "/v1/graph/collections/notes/documents", document.clone(),
        ).await;
        assert_eq!(status, StatusCode::OK, "{repaired}");
        assert_eq!(repaired["unchanged"], false);
        assert_eq!(repaired["replaced"], true);
        assert_eq!(repaired["embedding_usage"]["total_tokens"], 2);
        assert!(database.revision().unwrap() > changed_revision);
        assert!(database.graph_document("notes", "one").unwrap().unwrap().chunks_intact);
        assert_eq!(snapshot(&database), pristine);
        assert_eq!(provider_calls.load(Ordering::Relaxed), index + 2);
    }
    database.execute("UPDATE graph_notes_documents SET title = 'Revised through SQL' WHERE document_id = 'one'").unwrap();
    let changed_revision = database.revision().unwrap();
    assert!(
        !database
            .graph_document("notes", "one")
            .unwrap()
            .unwrap()
            .chunks_intact
    );
    document["title"] = json!("Revised through SQL");
    let (status, repaired) = call(
        &database,
        &embeddings,
        Method::POST,
        "/v1/graph/collections/notes/documents",
        document.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{repaired}");
    assert_eq!(repaired["unchanged"], false);
    assert_eq!(repaired["replaced"], true);
    assert_eq!(repaired["embedding_usage"]["total_tokens"], 2);
    assert!(database.revision().unwrap() > changed_revision);
    let stored = database.graph_document("notes", "one").unwrap().unwrap();
    assert!(stored.chunks_intact);
    assert_eq!(stored.title, "Revised through SQL");
    let after_title_repair = snapshot(&database);
    let ExecutionResult::Query(chunks) = &after_title_repair[2] else {
        panic!("expected chunks");
    };
    for row in &chunks.rows {
        let Value::Text(context) = &row[6] else {
            panic!("expected context text")
        };
        assert!(context.starts_with("Title: Revised through SQL\n"));
    }
    mock.join().unwrap();
    let revision = database.revision().unwrap();
    let (status, replay) = call(
        &database,
        &embeddings,
        Method::POST,
        "/v1/graph/collections/notes/documents",
        document,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay["unchanged"], true);
    assert_eq!(database.revision().unwrap(), revision);
    assert_eq!(provider_calls.load(Ordering::Relaxed), 4);
    assert_eq!(snapshot(&database), after_title_repair);
}

#[actix_web::test]
async fn zero_vector_provider_results_cannot_replace_documents_or_run_graph_search() {
    let database = Database::new();
    let (endpoint, mock) = mock_provider(3, |index, body, _| {
        if index == 0 {
            (200, provider_response(&body["input"], 2))
        } else {
            let data = body["input"]
                .as_array()
                .unwrap()
                .iter()
                .enumerate()
                .map(|(index, _)| json!({"index":index,"embedding":[0.0, 0.0]}))
                .collect::<Vec<_>>();
            (200, json!({"data":data}))
        }
    });
    let embeddings = configured(&endpoint, Provider::Openai);
    create(&database, &embeddings).await;
    let (status, body) = call(
        &database,
        &embeddings,
        Method::POST,
        "/v1/graph/collections/notes/documents",
        first_document(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let before = snapshot(&database);
    let revision = database.revision().unwrap();
    let mut changed = first_document();
    changed["title"] = json!("Attempted replacement with zero vectors");
    for (path, input) in [
        ("/v1/graph/collections/notes/documents", changed),
        (
            "/v1/graph/collections/notes/search",
            json!({"text":"query"}),
        ),
    ] {
        let (status, body) = call(&database, &embeddings, Method::POST, path, input).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
        assert_eq!(body["error"]["code"], "invalid_embedding_response");
        assert_eq!(snapshot(&database), before);
        assert_eq!(database.revision().unwrap(), revision);
    }
    mock.join().unwrap();
}

#[actix_web::test]
async fn concurrent_write_after_embedding_causes_conflict_without_partial_graph_rows() {
    let database = Database::new();
    let concurrent = database.clone();
    let (endpoint, mock) = mock_provider(1, move |_, body, _| {
        concurrent
            .execute("CREATE TABLE concurrent_write (id INTEGER)")
            .unwrap();
        (200, provider_response(&body["input"], 2))
    });
    let embeddings = configured(&endpoint, Provider::Openai);
    create(&database, &embeddings).await;
    let before = snapshot(&database);
    let revision = database.revision().unwrap();
    let (status, body) = call(
        &database,
        &embeddings,
        Method::POST,
        "/v1/graph/collections/notes/documents",
        first_document(),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "stale_revision");
    assert_eq!(database.revision().unwrap(), revision + 1);
    assert_eq!(snapshot(&database), before);
    let graph = database.graph_collection("notes").unwrap();
    assert_eq!(
        (graph.document_count, graph.chunk_count, graph.edge_count),
        (0, 0, 0)
    );
    mock.join().unwrap();
}

#[actix_web::test]
async fn invalid_documents_and_stale_client_revisions_are_rejected_before_generation() {
    let database = Database::new();
    let embeddings = configured("http://127.0.0.1:1", Provider::Openai);
    create(&database, &embeddings).await;
    let before = snapshot(&database);
    let revision = database.revision().unwrap();
    let (status, empty_search) = call(
        &database,
        &embeddings,
        Method::POST,
        "/v1/graph/collections/notes/search",
        json!({"text":"query without stored chunks"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{empty_search}");
    assert_eq!(empty_search["hits"], json!([]));
    for field in ["id", "title", "source", "text"] {
        let mut input = first_document();
        input[field] = json!("invalid\u{0000}value");
        let (status, body) = call(
            &database,
            &embeddings,
            Method::POST,
            "/v1/graph/collections/notes/documents",
            input,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{field}: {body}");
        assert_eq!(body["error"]["code"], "invalid_document", "{field}: {body}");
    }
    let mut stale = first_document();
    stale["expected_revision"] = json!(revision - 1);
    let (status, body) = call(
        &database,
        &embeddings,
        Method::POST,
        "/v1/graph/collections/notes/documents",
        stale,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "stale_revision");
    for text in ["雪".repeat(3000), " ".repeat(8)] {
        let (status, body) = call(
            &database,
            &embeddings,
            Method::POST,
            "/v1/graph/chunk",
            json!({"text":text,"chunking":{"max_characters":8000}}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }
    for chunking in [
        json!({"max_characters":0}),
        json!({"max_characters":8001}),
        json!({"max_characters":8,"overlap_characters":8}),
        json!({"max_chunks":0}),
        json!({"max_chunks":257}),
        json!({"max_characters":8,"overlap_characters":0,"max_chunks":1}),
    ] {
        let mut input = first_document();
        input["chunking"] = chunking.clone();
        let (status, body) = call(
            &database,
            &embeddings,
            Method::POST,
            "/v1/graph/collections/notes/documents",
            input,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{chunking}: {body}");
        let (status, preview) = call(
            &database,
            &embeddings,
            Method::POST,
            "/v1/graph/chunk",
            json!({"text":first_document()["text"],"chunking":chunking}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{preview}");
        assert!(
            preview.get("chunks").is_none(),
            "rejected documents must not return a truncated preview"
        );
    }
    assert_eq!(snapshot(&database), before);
    assert_eq!(database.revision().unwrap(), revision);
}

#[actix_web::test]
async fn graph_routes_require_authentication_and_chunk_preview_obeys_shared_capacity() {
    let database = Database::new();
    let embeddings = configured("http://127.0.0.1:1", Provider::Openai);
    let limiter = DatabaseTaskLimiter::new(1);
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(database.clone()))
            .app_data(web::Data::new(embeddings))
            .app_data(web::Data::new(ApiSecurity::bearer_token("synthetic-admin")))
            .app_data(web::Data::new(limiter.clone()))
            .configure(crate::api::configure),
    )
    .await;
    for (method, path, body) in [
        (Method::POST, "/v1/graph/chunk", json!({"text":"hello"})),
        (
            Method::POST,
            "/v1/graph/collections",
            json!({"name":"notes"}),
        ),
        (Method::GET, "/v1/graph/collections", json!({})),
        (Method::GET, "/v1/graph/collections/notes", json!({})),
        (
            Method::POST,
            "/v1/graph/collections/notes/documents",
            json!({"id":"one","text":"hello"}),
        ),
        (
            Method::GET,
            "/v1/graph/collections/notes/documents/one",
            json!({}),
        ),
        (
            Method::DELETE,
            "/v1/graph/collections/notes/documents/one",
            json!({"expected_revision":0}),
        ),
        (
            Method::POST,
            "/v1/graph/collections/notes/search",
            json!({"text":"hello"}),
        ),
    ] {
        let response = test::call_service(
            &app,
            test::TestRequest::default()
                .method(method)
                .uri(path)
                .set_json(body)
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
        assert_eq!(
            response.headers().get("www-authenticate").unwrap(),
            "Bearer"
        );
    }
    assert!(database.tables().unwrap().is_empty());
    let held = limiter.acquire().unwrap();
    let response = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/v1/graph/chunk")
            .insert_header(("authorization", "Bearer synthetic-admin"))
            .set_json(json!({"text":"hello"}))
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers().get("retry-after").unwrap(), "1");
    let body: JsonValue = test::read_body_json(response).await;
    assert_eq!(body["error"]["code"], "overloaded");
    drop(held);
    let response = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/v1/graph/chunk")
            .insert_header(("authorization", "Bearer synthetic-admin"))
            .set_json(json!({"text":"hello"}))
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[actix_web::test]
async fn structured_documents_share_sql_columns_and_reuse_embeddings_for_field_edits() {
    let database = Database::new();
    let (endpoint, mock) = mock_provider(1, |_, body, _| {
        (200, provider_response(&body["input"], 256))
    });
    let embeddings = configured(&endpoint, Provider::Voyage);
    let columns = json!([
        {"name":"category","data_type":"TEXT","nullable":false},
        {"name":"priority","data_type":"INTEGER"},
        {"name":"score","data_type":"DOUBLE"},
        {"name":"approved","data_type":"BOOLEAN","nullable":false},
        {"name":"external_id","data_type":"TEXT","unique":true}
    ]);
    let (status, created) = call(
        &database,
        &embeddings,
        Method::POST,
        "/v1/graph/collections",
        json!({"name":"notes","document_columns":columns}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    assert_eq!(created["document_columns"].as_array().unwrap().len(), 5);
    assert_eq!(created["document_columns"][0]["nullable"], false);

    let mut document = first_document();
    // Required values fail before contacting the provider or changing the catalog.
    let before = snapshot(&database);
    let (status, _) = call(
        &database,
        &embeddings,
        Method::POST,
        "/v1/graph/collections/notes/documents",
        document.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(snapshot(&database), before);
    document["metadata"] = json!({"category":"integration","priority":2,"score":1,
        "approved":false,"external_id":"one","extra":{"retained":true}});
    let (status, inserted) = call(
        &database,
        &embeddings,
        Method::POST,
        "/v1/graph/collections/notes/documents",
        document.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{inserted}");
    assert_eq!(inserted["embeddings_reused"], false);
    mock.join().unwrap(); // Further embedding requests would fail.
    let chunks_and_edges = snapshot(&database)[2..].to_vec();

    // Numeric normalization and omitted nullable fields preserve idempotency.
    let revision = database.revision().unwrap();
    let (status, replay) = call(
        &database,
        &embeddings,
        Method::POST,
        "/v1/graph/collections/notes/documents",
        document.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay["unchanged"], true);
    assert_eq!(database.revision().unwrap(), revision);

    document["metadata"]["category"] = json!("engineering");
    document["metadata"]
        .as_object_mut()
        .unwrap()
        .remove("priority");
    document["expected_revision"] = json!(revision);
    let (status, updated) = call(
        &database,
        &embeddings,
        Method::POST,
        "/v1/graph/collections/notes/documents",
        document.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["embeddings_reused"], true);
    assert_eq!(updated["unchanged"], false);
    assert_eq!(updated["embedding_usage"]["total_tokens"], 0);
    assert_eq!(snapshot(&database)[2..], chunks_and_edges);
    let stored = database.graph_document("notes", "one").unwrap().unwrap();
    assert_eq!(stored.metadata["priority"], JsonValue::Null);
    assert_eq!(stored.metadata["score"].as_f64(), Some(1.0));
    assert!(stored.chunks_intact);
    let (status, sql) = call(&database, &embeddings, Method::POST, "/v1/sql", json!({"sql":
        "SELECT d.category, d.approved, c.text, cosine_distance(c.embedding, c.embedding) AS distance FROM graph_notes_documents d JOIN graph_notes_chunks c ON c.document_id = d.document_id WHERE d.category = 'engineering' ORDER BY distance LIMIT 2"
    })).await;
    assert_eq!(status, StatusCode::OK, "{sql}");
    assert_eq!(sql["results"][0]["rows"].as_array().unwrap().len(), 2);
    assert_eq!(sql["results"][0]["rows"][0][0], "engineering");
    assert_eq!(sql["results"][0]["rows"][0][1], false);

    // A SQL field edit is immediately the value returned by graph APIs.
    database
        .execute("UPDATE graph_notes_documents SET category = 'sql' WHERE document_id = 'one'")
        .unwrap();
    let (status, stored) = call(
        &database,
        &embeddings,
        Method::GET,
        "/v1/graph/collections/notes/documents/one",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{stored}");
    assert_eq!(stored["metadata"]["category"], "sql");
    assert!(stored["metadata"]["extra"]["retained"].as_bool().unwrap());
    let before = snapshot(&database);
    let (status, _) = call(
        &database,
        &embeddings,
        Method::POST,
        "/v1/graph/collections/notes/documents",
        document.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(snapshot(&database), before);
    document["expected_revision"] = json!(database.revision().unwrap());
    document["metadata"]["approved"] = json!("false");
    let (status, _) = call(
        &database,
        &embeddings,
        Method::POST,
        "/v1/graph/collections/notes/documents",
        document,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(snapshot(&database), before);
}

#[actix_web::test]
async fn invalid_document_schema_does_not_create_partial_collections() {
    let database = Database::new();
    let (endpoint, mock) = mock_provider(0, |_, _, _| unreachable!());
    let embeddings = configured(&endpoint, Provider::Voyage);
    mock.join().unwrap();
    for columns in [
        json!([{"name":"title","data_type":"TEXT"}]),
        json!([{"name":"tag","data_type":"VECTOR(3)"}]),
        json!([{"name":"a","data_type":"TEXT"},{"name":"a","data_type":"TEXT"}]),
        json!([{"name":"a","data_type":"JSON"}]),
        json!([{"name":"not a field","data_type":"TEXT"}]),
        json!([{"name":"a","data_type":"TEXT","unknown":true}]),
    ] {
        let revision = database.revision().unwrap();
        let (status, body) = call(
            &database,
            &embeddings,
            Method::POST,
            "/v1/graph/collections",
            json!({"name":"notes","document_columns":columns}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(database.graph_collections().unwrap().is_empty());
        assert_eq!(database.revision().unwrap(), revision);
    }
}
