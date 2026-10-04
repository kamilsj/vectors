//! Provider-backed behavior is exercised with synthetic loopback services only.

use super::*;
use crate::embedding::{
    tests::{configured, mock_provider, test_service},
    Provider,
};
use crate::{
    GraphChunkInput, GraphCollectionConfig, GraphDocumentInput, GraphEmbeddingProfile,
    GraphIngestRequest,
};
use actix_web::{test as awtest, App};
use serde_json::json;

fn fixture(provider: Provider, populated: bool) -> Database {
    let dimensions = if provider == Provider::Openai { 2 } else { 256 };
    let profile = GraphEmbeddingProfile {
        provider: provider.as_str().into(),
        model: if provider == Provider::Openai {
            "text-embedding-3-small"
        } else {
            "voyage-4"
        }
        .into(),
        dimensions,
        context_format_version: 1,
    };
    let db = Database::new();
    db.graph_create_collection_with_columns(
        GraphCollectionConfig {
            name: "notes".into(),
            profile: profile.clone(),
            semantic_neighbors: 0,
            semantic_threshold: 0.8,
        },
        vec![Column {
            name: "tenant_id".into(),
            data_type: DataType::Integer,
            nullable: false,
            unique: false,
        }],
    )
    .unwrap();
    if populated {
        for (id, text, tenant) in [
            ("one", "Private unrelated document.", 1),
            ("two", "ZX419 means the journal is full.", 2),
        ] {
            let mut values = vec![0.0; dimensions];
            values[0] = 1.0;
            db.graph_ingest_document(GraphIngestRequest {
                collection: "notes".into(),
                expected_revision: db.revision().unwrap(),
                expected_profile: profile.clone(),
                document: GraphDocumentInput {
                    id: id.into(),
                    title: format!("Guide {id}"),
                    source: format!("manual/{id}.md"),
                    text: text.into(),
                    metadata: json!({"tenant_id": tenant}),
                    chunking: json!({}),
                    chunks: vec![GraphChunkInput {
                        start_byte: 0,
                        end_byte: text.len(),
                        text: text.into(),
                        embedding_text: text.into(),
                        embedding: Vector::new(values).unwrap(),
                    }],
                },
            })
            .unwrap();
        }
    }
    db
}

fn embedding_response(dimensions: usize) -> JsonValue {
    let mut values = vec![0.0; dimensions];
    values[0] = 1.0;
    json!({"data":[{"index":0,"embedding":values}],"usage":{"total_tokens":4}})
}

fn generation_response(answer: &str) -> JsonValue {
    json!({
        "status":"completed", "model":"gpt-4.1-mini", "error":null,
        "output":[
            {"type":"reasoning", "summary":[{"type":"summary_text", "text":"hidden-reasoning-must-not-leak"}]},
            {"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":answer}]}
        ],
        "usage":{"input_tokens":45,"output_tokens":12}
    })
}

fn question() -> JsonValue {
    json!({"text":"What does ZX419 mean?", "retrieval":{
        "candidate_limit":2,"seed_limit":1,"max_results":2,"max_hops":0,"diversity":0,
        "document_filters":[{"column":"tenant_id","operator":"eq","value":2}]
    }})
}

async fn call(
    db: &Database,
    embeddings: Option<&EmbeddingService>,
    endpoint: Option<&str>,
    body: JsonValue,
) -> (StatusCode, JsonValue) {
    call_with_counter(
        db,
        embeddings,
        endpoint,
        body,
        Arc::new(AtomicUsize::new(0)),
    )
    .await
}

async fn call_with_counter(
    db: &Database,
    embeddings: Option<&EmbeddingService>,
    endpoint: Option<&str>,
    body: JsonValue,
    counter: Arc<AtomicUsize>,
) -> (StatusCode, JsonValue) {
    let mut app = App::new()
        .app_data(web::Data::new(db.clone()))
        .app_data(web::Data::new(TestGenerationCounter(counter)));
    if let Some(service) = embeddings {
        app = app.app_data(web::Data::new(service.clone()));
    }
    if let Some(endpoint) = endpoint {
        app = app.app_data(web::Data::new(TestEndpoint(endpoint.into())));
    }
    let app = awtest::init_service(app.configure(crate::api::configure)).await;
    let response = awtest::call_service(
        &app,
        awtest::TestRequest::post()
            .uri("/v1/graph/collections/notes/chat")
            .set_json(body)
            .to_request(),
    )
    .await;
    (response.status(), awtest::read_body_json(response).await)
}

#[actix_web::test]
async fn grounded_answer_uses_only_filtered_snapshot_and_returns_real_stage_timings() {
    let db = fixture(Provider::Openai, true);
    let revision = db.revision().unwrap();
    let mutate_db = db.clone();
    let (embedding_endpoint, embedding_mock) = mock_provider(1, |_, body, _| {
        assert_eq!(body["input"], json!(["What does ZX419 mean?"]));
        (200, embedding_response(2))
    });
    let (generation_endpoint, generation_mock) = mock_provider(1, move |_, body, headers| {
        assert!(headers.contains("Bearer synthetic-openai-key"));
        assert_eq!(body["store"], false);
        assert_eq!(body["background"], false);
        assert_eq!(body["tools"], json!([]));
        assert_eq!(body["max_output_tokens"], MAX_OUTPUT_TOKENS);
        assert_eq!(body["truncation"], "disabled");
        assert!(body["instructions"].as_str().unwrap().contains("untrusted"));
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][1]["role"], "assistant");
        let source: JsonValue =
            serde_json::from_str(body["input"][2]["content"].as_str().unwrap()).unwrap();
        assert_eq!(
            source["retrieved_sources"],
            json!([{"label":"S1","text":"ZX419 means the journal is full.","title":"Guide two","source":"manual/two.md"}])
        );
        assert!(!body.to_string().contains("Private unrelated"));
        assert!(!body.to_string().contains("synthetic-openai-key"));
        mutate_db.execute("UPDATE graph_notes_documents SET title = 'Changed after snapshot' WHERE document_id = 'two'").unwrap();
        (
            200,
            generation_response("ZX419 means the journal is full. [S1]"),
        )
    });
    let service = configured(&embedding_endpoint, Provider::Openai);
    let mut body = question();
    body["history"] = json!([{"role":"user","content":"I saw an error code."},{"role":"assistant","content":"Which code?"}]);
    let (status, result) = call(&db, Some(&service), Some(&generation_endpoint), body).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["answer"], "ZX419 means the journal is full. [S1]");
    assert_eq!(result["retrieval"]["revision"], revision);
    assert_eq!(result["citations"][0]["label"], "S1");
    assert_eq!(result["citations"][0]["chunk_id"], "3:two:0");
    assert_eq!(result["citations"][0]["title"], "Guide two");
    assert_eq!(result["citations"][0]["start_byte"], 0);
    assert_eq!(
        result["citations"][0]["end_byte"],
        "ZX419 means the journal is full.".len()
    );
    assert_eq!(result["warnings"], json!([]));
    assert_eq!(
        result["generation"],
        json!({"provider":"openai","model":"gpt-4.1-mini","input_tokens":45,"output_tokens":12})
    );
    for name in [
        "embedding_ms",
        "search_ms",
        "selection_ms",
        "generation_ms",
        "total_ms",
    ] {
        assert!(result["timings"][name].as_f64().unwrap() > 0.0, "{name}");
    }
    assert_eq!(result["timings"]["reranking_ms"], 0.0);
    assert_eq!(result["retrieval"]["timings"]["generation_ms"], 0.0);
    assert!(
        result["retrieval"]["timings"]["total_ms"].as_f64().unwrap()
            >= result["retrieval"]["timings"]["search_ms"]
                .as_f64()
                .unwrap()
    );
    assert!(!result.to_string().contains("hidden-reasoning"));
    assert!(db.revision().unwrap() > revision);
    embedding_mock.join().unwrap();
    generation_mock.join().unwrap();
}

#[actix_web::test]
async fn empty_chat_needs_no_services_and_has_zero_usage_and_timings() {
    let db = fixture(Provider::Openai, false);
    for mode in ["answer", "retrieve"] {
        let (status, result) = call(&db, None, None, json!({"text":"Question?","mode":mode})).await;
        assert_eq!(status, StatusCode::OK, "{result}");
        assert!(result["answer"].is_null());
        assert_eq!(result["retrieval"]["hits"], json!([]));
        assert_eq!(result["retrieval"]["traversal_seed_ids"], json!([]));
        assert_eq!(result["citations"], json!([]));
        assert_eq!(
            result["generation"],
            json!({"provider":null,"model":null,"input_tokens":0,"output_tokens":0})
        );
        for value in result["timings"].as_object().unwrap().values() {
            assert_eq!(value.as_f64().unwrap(), 0.0);
        }
    }
}

fn without_generation_key(service: &EmbeddingService) {
    service
        .update_settings(
            serde_json::from_value(json!({"provider":"openai","clear_api_key":true})).unwrap(),
        )
        .unwrap();
    service
        .update_settings(
            serde_json::from_value(json!({"provider":"voyage","dimensions":256})).unwrap(),
        )
        .unwrap();
}

#[actix_web::test]
async fn retrieval_only_works_with_voyage_and_no_generation_key() {
    let db = fixture(Provider::Voyage, true);
    let (endpoint, mock) = mock_provider(1, |_, _, headers| {
        assert!(headers.contains("Bearer synthetic-voyage-key"));
        (200, embedding_response(256))
    });
    let service = configured(&endpoint, Provider::Voyage);
    without_generation_key(&service);
    let settings = serde_json::to_value(service.settings().unwrap()).unwrap();
    assert_eq!(settings["generation_configured"], false);
    let mut body = question();
    body["mode"] = json!("retrieve");
    let (status, result) = call(&db, Some(&service), Some("http://127.0.0.1:1"), body).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["retrieval"]["hits"].as_array().unwrap().len(), 1);
    assert_eq!(result["citations"][0]["label"], "S1");
    assert!(result["answer"].is_null());
    assert!(result["generation"]["provider"].is_null());
    assert_eq!(result["timings"]["generation_ms"], 0.0);
    mock.join().unwrap();
}

#[actix_web::test]
async fn answer_uses_openai_key_even_when_voyage_embeds() {
    let db = fixture(Provider::Voyage, true);
    let (embedding_endpoint, embedding_mock) =
        mock_provider(1, |_, _, _| (200, embedding_response(256)));
    let (generation_endpoint, generation_mock) = mock_provider(1, |_, _, headers| {
        assert!(headers.contains("Bearer synthetic-openai-key"));
        assert!(!headers.contains("synthetic-voyage-key"));
        (200, generation_response("The journal is full. [S1]"))
    });
    let service = configured(&embedding_endpoint, Provider::Voyage);
    let settings = serde_json::to_value(service.settings().unwrap()).unwrap();
    assert_eq!(settings["generation_configured"], true);
    assert!(!settings.to_string().contains("synthetic"));
    let (status, result) = call(&db, Some(&service), Some(&generation_endpoint), question()).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["generation"]["provider"], "openai");
    embedding_mock.join().unwrap();
    generation_mock.join().unwrap();
}

#[actix_web::test]
async fn populated_answer_checks_generation_credentials_before_embedding() {
    let db = fixture(Provider::Voyage, true);
    let service = configured("http://127.0.0.1:1", Provider::Voyage);
    without_generation_key(&service);
    let (status, result) = call(&db, Some(&service), None, question()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{result}");
    assert_eq!(result["error"]["code"], "generation_not_configured");
}

#[actix_web::test]
async fn fifth_answer_is_rejected_without_spending_an_embedding_call() {
    let calls = Arc::new(AtomicUsize::new(0));
    let provider_calls = calls.clone();
    let (endpoint, mock) = mock_provider(1, move |_, _, _| {
        provider_calls.fetch_add(1, Ordering::SeqCst);
        (200, embedding_response(2))
    });
    let counter = Arc::new(AtomicUsize::new(0));
    let permits = (0..MAX_GENERATION_REQUESTS)
        .map(|_| GenerationPermit::acquire(counter.clone()).unwrap())
        .collect::<Vec<_>>();
    let app = awtest::init_service(
        App::new()
            .app_data(web::Data::new(fixture(Provider::Openai, true)))
            .app_data(web::Data::new(configured(&endpoint, Provider::Openai)))
            .app_data(web::Data::new(TestGenerationCounter(counter.clone())))
            .configure(crate::api::configure),
    )
    .await;
    let response = awtest::call_service(
        &app,
        awtest::TestRequest::post()
            .uri("/v1/graph/collections/notes/chat")
            .set_json(question())
            .to_request(),
    )
    .await;
    let status = response.status();
    let result: JsonValue = awtest::read_body_json(response).await;
    let paid_calls = calls.load(Ordering::SeqCst);
    // Finish the one-shot local mock without leaving a waiting provider thread.
    if paid_calls == 0 {
        reqwest::Client::new()
            .post(&endpoint)
            .json(&json!({}))
            .send()
            .await
            .unwrap();
    }
    mock.join().unwrap();
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{result}");
    assert_eq!(result["error"]["code"], "generation_overloaded");
    assert_eq!(paid_calls, 0);
    drop(permits);
    assert_eq!(counter.load(Ordering::SeqCst), 0);
    assert!(GenerationPermit::acquire(counter).is_ok());
}

#[actix_web::test]
async fn no_matching_hits_skip_generation_even_with_a_broken_generation_endpoint() {
    let db = fixture(Provider::Openai, true);
    let service = configured("http://127.0.0.1:1", Provider::Openai);
    let (status, result) = call(
        &db,
        Some(&service),
        Some("http://127.0.0.1:1"),
        json!({
            "text":"unfindableidentifier", "retrieval":{"vector_weight":0,"max_hops":0}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert!(result["answer"].is_null());
    assert_eq!(result["retrieval"]["hits"], json!([]));
    assert_eq!(result["generation"]["output_tokens"], 0);
    assert_eq!(result["timings"]["generation_ms"], 0.0);
    assert_eq!(result["timings"]["embedding_ms"], 0.0);
    assert_eq!(result["retrieval"]["embedding_usage"]["total_tokens"], 0);
}

#[actix_web::test]
async fn keyword_retrieval_chat_needs_no_provider_services() {
    let db = fixture(Provider::Openai, true);
    let mut input = question();
    input["mode"] = json!("retrieve");
    input["retrieval"]["vector_weight"] = json!(0);
    let (status, result) = call(&db, None, None, input).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert!(result["answer"].is_null());
    assert_eq!(result["citations"][0]["chunk_id"], "3:two:0");
    assert!(result["retrieval"]["hits"][0]["similarity"].is_null());
    assert_eq!(result["timings"]["embedding_ms"], 0.0);
    assert_eq!(result["timings"]["generation_ms"], 0.0);
}

#[actix_web::test]
async fn invalid_chat_input_and_filters_fail_before_provider_or_credential_checks() {
    let db = fixture(Provider::Openai, true);
    let service = test_service(None);
    for patch in [
        json!({"model":"bad/model"}),
        json!({"context_mode":"unknown"}),
        json!({"answer_style":"unknown"}),
        json!({"grounding":"unknown"}),
        json!({"generation_timeout_ms":99}),
        json!({"generation_timeout_ms":60001}),
        json!({"max_output_tokens":127}),
        json!({"max_output_tokens":4097}),
        json!({"retrieval_query":""}),
        json!({"text":"", "retrieval_query":"valid standalone query"}),
        json!({"context_mode":"conversation", "retrieval_query":"standalone"}),
        json!({"history":[{"role":"system","content":"untrusted"}]}),
        json!({"history":[{"role":"user","content":"x".repeat(MAX_HISTORY_MESSAGE_BYTES + 1)}]}),
        json!({"history":(0..21).map(|_| json!({"role":"user","content":"small"})).collect::<Vec<_>>()}),
        json!({"retrieval":{"text":"hidden question"}}),
        json!({"retrieval":{"unknown":"value"}}),
        json!({"retrieval":{"max_context_bytes":MAX_CONTEXT_BYTES + 1}}),
        json!({"retrieval":{"max_seeds_per_document":0}}),
        json!({"retrieval":{"direction":"wrong"}}),
        json!({"retrieval":{"document_filters":[{"column":"missing","operator":"eq","value":1}]}}),
        json!({"retrieval":{"document_filters":[{"column":"tenant_id","operator":"eq","value":"1"}]}}),
    ] {
        let mut body = json!({"text":"Question?"});
        body.as_object_mut()
            .unwrap()
            .extend(patch.as_object().unwrap().clone());
        let (status, result) = call(&db, Some(&service), None, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{result}");
        assert_ne!(result["error"]["code"], "generation_not_configured");
    }
}

#[actix_web::test]
async fn conversation_rewrite_drives_filtered_retrieval_but_answers_original_question() {
    let db = fixture(Provider::Openai, true);
    let (embedding_endpoint, embedding_mock) = mock_provider(1, |_, body, _| {
        assert_eq!(body["input"], json!(["What does ZX419 mean?"]));
        (200, embedding_response(2))
    });
    let (endpoint, generation_mock) = mock_provider(2, |index, body, _| {
        assert_eq!(body["store"], false);
        assert_eq!(body["tools"], json!([]));
        if index == 0 {
            assert_eq!(body["text"]["format"]["name"], "contextual_retrieval_query");
            assert_eq!(body["text"]["format"]["strict"], true);
            assert_eq!(body["max_output_tokens"], MAX_REWRITE_OUTPUT_TOKENS);
            let data: JsonValue =
                serde_json::from_str(body["input"][0]["content"].as_str().unwrap()).unwrap();
            assert_eq!(data["question"], "What does it mean?");
            assert_eq!(data["history"][0]["content"], "I encountered ZX419.");
            assert!(!body.to_string().contains("Private unrelated"));
            (
                200,
                generation_response(r#"{"query":"What does ZX419 mean?"}"#),
            )
        } else {
            assert_eq!(
                body["input"].as_array().unwrap().last().unwrap()["content"],
                "What does it mean?"
            );
            assert!(!body.to_string().contains("Private unrelated"));
            (200, generation_response("The journal is full. [S1]"))
        }
    });
    let mut body = question();
    body["text"] = json!("What does it mean?");
    body["context_mode"] = json!("conversation");
    body["history"] = json!([{"role":"user","content":"I encountered ZX419."}]);
    let (status, result) = call(
        &db,
        Some(&configured(&embedding_endpoint, Provider::Openai)),
        Some(&endpoint),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["retrieval_query"], "What does ZX419 mean?");
    assert_eq!(result["query_context"]["mode"], "conversation");
    assert_eq!(result["query_context"]["rewritten"], true);
    assert_eq!(result["query_context"]["generation"]["input_tokens"], 45);
    assert!(result["query_context"]["duration_ms"].as_f64().unwrap() > 0.0);
    assert_eq!(result["answer_status"], "answered");
    assert_eq!(result["citation_status"], "valid_labels");
    assert_eq!(result["cited_labels"], json!(["S1"]));
    assert_eq!(result["citations"][0]["document_id"], "two");
    assert_eq!(result["generation"]["input_tokens"], 45);
    assert!(result["speech_text"].is_null());
    embedding_mock.join().unwrap();
    generation_mock.join().unwrap();
}

#[actix_web::test]
async fn supplied_query_and_history_free_conversation_need_no_rewrite() {
    let db = fixture(Provider::Openai, true);
    let mut body = question();
    body["text"] = json!("What does it mean?");
    body["retrieval_query"] = json!("ZX419");
    body["mode"] = json!("retrieve");
    body["retrieval"]["vector_weight"] = json!(0);
    let (status, result) = call(&db, None, Some("http://127.0.0.1:1"), body).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["query_context"]["mode"], "provided");
    assert_eq!(result["query_context"]["rewritten"], false);
    assert_eq!(result["retrieval_query"], "ZX419");
    assert_eq!(result["answer_status"], "retrieval_only");
    assert_eq!(result["citations"][0]["document_id"], "two");

    let mut body = question();
    body["context_mode"] = json!("conversation");
    body["mode"] = json!("retrieve");
    body["retrieval"]["vector_weight"] = json!(0);
    let (status, result) = call(&db, None, Some("http://127.0.0.1:1"), body).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["query_context"]["mode"], "conversation");
    assert_eq!(result["query_context"]["rewritten"], false);
    assert_eq!(result["query_context"]["generation"]["input_tokens"], 0);
}

#[actix_web::test]
async fn conversational_keyword_retrieval_reports_only_rewrite_usage() {
    let (endpoint, mock) = mock_provider(1, |_, body, _| {
        assert_eq!(body["text"]["format"]["name"], "contextual_retrieval_query");
        (200, generation_response(r#"{"query":"ZX419 journal"}"#))
    });
    let mut body = question();
    body["text"] = json!("What does it mean?");
    body["mode"] = json!("retrieve");
    body["context_mode"] = json!("conversation");
    body["history"] = json!([{"role":"user","content":"I encountered ZX419."}]);
    body["retrieval"]["vector_weight"] = json!(0);
    let (status, result) = call(
        &fixture(Provider::Openai, true),
        Some(&configured("http://127.0.0.1:1", Provider::Openai)),
        Some(&endpoint),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["retrieval_query"], "ZX419 journal");
    assert_eq!(result["answer_status"], "retrieval_only");
    assert!(result["answer"].is_null());
    assert!(result["generation"]["provider"].is_null());
    assert_eq!(result["query_context"]["generation"]["input_tokens"], 45);
    assert_eq!(result["retrieval"]["embedding_usage"]["total_tokens"], 0);
    assert_eq!(result["citations"][0]["document_id"], "two");
    mock.join().unwrap();
}

#[actix_web::test]
async fn empty_and_filtered_empty_chat_skip_rewrite_embedding_and_generation() {
    for populated in [false, true] {
        for mode in ["answer", "retrieve"] {
            let db = fixture(Provider::Openai, populated);
            let mut body = question();
            body["mode"] = json!(mode);
            body["context_mode"] = json!("conversation");
            body["history"] = json!([{"role":"user","content":"I encountered ZX419."}]);
            body["retrieval"]["document_filters"][0]["value"] = json!(99);
            // No services configured: every provider stage must be skipped.
            let (status, result) = call(&db, None, Some("http://127.0.0.1:1"), body).await;
            assert_eq!(status, StatusCode::OK, "{result}");
            assert_eq!(
                result["answer_status"],
                if mode == "answer" {
                    "no_sources"
                } else {
                    "retrieval_only"
                }
            );
            assert_eq!(result["retrieval"]["hits"], json!([]));
            assert_eq!(result["query_context"]["rewritten"], false);
            assert_eq!(result["query_context"]["generation"]["input_tokens"], 0);
            assert_eq!(result["generation"]["input_tokens"], 0);
            assert_eq!(result["timings"]["total_ms"], 0.0);
        }
    }
}

#[actix_web::test]
async fn malformed_filters_and_unavailable_embedding_configuration_fail_before_rewrite() {
    let db = fixture(Provider::Voyage, true);
    let service = configured("http://127.0.0.1:1", Provider::Voyage);
    service
        .update_settings(serde_json::from_value(json!({"clear_api_key":true})).unwrap())
        .unwrap();
    assert!(ensure_configured(Some(&web::Data::new(service.clone()))).is_ok());
    let mut body = question();
    body["context_mode"] = json!("conversation");
    body["history"] = json!([{"role":"user","content":"I encountered ZX419."}]);
    let (status, result) = call(
        &db,
        Some(&service),
        Some("http://127.0.0.1:1"),
        body.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{result}");
    assert_eq!(result["error"]["code"], "embedding_not_configured");
    body["retrieval"]["document_filters"][0]["column"] = json!("missing");
    let (status, result) = call(&db, Some(&service), Some("http://127.0.0.1:1"), body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{result}");
    assert!(!result.to_string().contains("synthetic"));
}

#[actix_web::test]
async fn rewrite_admission_applies_to_retrieval_only_before_provider_work() {
    let counter = Arc::new(AtomicUsize::new(0));
    let permits = (0..MAX_GENERATION_REQUESTS)
        .map(|_| GenerationPermit::acquire(counter.clone()).unwrap())
        .collect::<Vec<_>>();
    let mut body = question();
    body["mode"] = json!("retrieve");
    body["context_mode"] = json!("conversation");
    body["history"] = json!([{"role":"user","content":"I encountered ZX419."}]);
    let (status, result) = call_with_counter(
        &fixture(Provider::Openai, true),
        None,
        Some("http://127.0.0.1:1"),
        body,
        counter.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{result}");
    assert_eq!(result["error"]["code"], "generation_overloaded");
    drop(permits);
    assert_eq!(counter.load(Ordering::Acquire), 0);
}

#[actix_web::test]
async fn invalid_or_incomplete_rewrite_never_becomes_a_retrieval_query() {
    for (text, status) in [
        (r#"{"query":"ZX419","extra":"private"}"#, "completed"),
        (r#"{"query":"ZX419"}"#, "incomplete"),
        (r#"{"query":""}"#, "completed"),
    ] {
        let (endpoint, mock) = mock_provider(1, move |_, _, _| {
            let mut result = generation_response(text);
            result["status"] = json!(status);
            (200, result)
        });
        let mut body = question();
        body["context_mode"] = json!("conversation");
        body["history"] = json!([{"role":"user","content":"I encountered ZX419."}]);
        let (status, result) = call(
            &fixture(Provider::Openai, true),
            Some(&configured("http://127.0.0.1:1", Provider::Openai)),
            Some(&endpoint),
            body,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{result}");
        assert_eq!(result["error"]["code"], "invalid_query_rewrite");
        assert!(!result.to_string().contains("private"));
        mock.join().unwrap();
    }
}

#[actix_web::test]
async fn generation_timeout_and_cancellation_release_admission_without_retry() {
    for cancel in [false, true] {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (endpoint, mock) = mock_provider(1, move |_, _, _| {
            started_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(5));
            (200, generation_response("The journal is full. [S1]"))
        });
        let counter = Arc::new(AtomicUsize::new(0));
        let observed = counter.clone();
        let mut body = question();
        body["retrieval"]["vector_weight"] = json!(0);
        // Leave CI enough time to establish the loopback connection before
        // exercising the deliberately blocked response timeout.
        body["generation_timeout_ms"] = json!(if cancel { 60_000 } else { 500 });
        let task = actix_web::rt::spawn(async move {
            call_with_counter(
                &fixture(Provider::Openai, true),
                Some(&configured("http://127.0.0.1:1", Provider::Openai)),
                Some(&endpoint),
                body,
                counter,
            )
            .await
        });
        web::block(move || started_rx.recv_timeout(Duration::from_secs(5)))
            .await
            .unwrap()
            .unwrap();
        if cancel {
            assert_eq!(observed.load(Ordering::Acquire), 1);
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        } else {
            let (status, result) = task.await.unwrap();
            assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{result}");
            assert_eq!(result["error"]["code"], "generation_timeout");
        }
        assert_eq!(observed.load(Ordering::Acquire), 0);
        assert!(GenerationPermit::acquire(observed).is_ok());
        release_tx.send(()).unwrap();
        mock.join().unwrap();
    }
}

#[actix_web::test]
async fn strict_voice_answer_returns_only_validated_evidence_and_speech() {
    let (endpoint, mock) = mock_provider(1, |_, body, _| {
        assert_eq!(body["text"]["format"]["name"], "grounded_answer");
        assert_eq!(body["max_output_tokens"], 256);
        assert!(body["instructions"]
            .as_str()
            .unwrap()
            .contains("voice script"));
        (200, generation_response(&json!({"status":"answered","answer":"The journal is full. [S1]","evidence":[{"label":"S1","quote":"ZX419 means the journal is full."}]}).to_string()))
    });
    let mut body = question();
    body["retrieval"]["vector_weight"] = json!(0);
    body["answer_style"] = json!("voice");
    body["grounding"] = json!("strict");
    body["max_output_tokens"] = json!(256);
    let (status, result) = call(
        &fixture(Provider::Openai, true),
        Some(&configured("http://127.0.0.1:1", Provider::Openai)),
        Some(&endpoint),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["answer_status"], "answered");
    assert_eq!(result["citation_status"], "valid_labels");
    assert_eq!(result["speech_text"], "The journal is full.");
    assert_eq!(
        result["evidence"],
        json!([{"label":"S1","quote":"ZX419 means the journal is full."}])
    );
    mock.join().unwrap();
}

#[actix_web::test]
async fn chat_requires_authentication_before_any_provider_call() {
    let app = awtest::init_service(
        App::new()
            .app_data(web::Data::new(fixture(Provider::Openai, true)))
            .app_data(web::Data::new(ApiSecurity::bearer_token("expected-token")))
            .configure(crate::api::configure),
    )
    .await;
    let response = awtest::call_service(
        &app,
        awtest::TestRequest::post()
            .uri("/v1/graph/collections/notes/chat")
            .set_json(question())
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[actix_web::test]
async fn generation_failures_are_redacted_and_not_silently_replaced_with_retrieval() {
    for (upstream, expected, code) in [
        (401, StatusCode::BAD_GATEWAY, "generation_auth_failed"),
        (
            429,
            StatusCode::TOO_MANY_REQUESTS,
            "generation_rate_limited",
        ),
        (503, StatusCode::BAD_GATEWAY, "generation_unavailable"),
    ] {
        let db = fixture(Provider::Openai, true);
        let (embedding_endpoint, embedding_mock) =
            mock_provider(1, |_, _, _| (200, embedding_response(2)));
        let (generation_endpoint, generation_mock) = mock_provider(1, move |_, _, _| {
            (
                upstream,
                json!({"error":"private-provider-message synthetic-openai-key"}),
            )
        });
        let service = configured(&embedding_endpoint, Provider::Openai);
        let (status, result) =
            call(&db, Some(&service), Some(&generation_endpoint), question()).await;
        assert_eq!(status, expected, "{result}");
        assert_eq!(result["error"]["code"], code);
        assert!(!result.to_string().contains("private-provider"));
        assert!(!result.to_string().contains("synthetic"));
        embedding_mock.join().unwrap();
        generation_mock.join().unwrap();
    }
}

#[actix_web::test]
async fn oversized_generation_response_is_rejected() {
    let db = fixture(Provider::Openai, true);
    let (embedding_endpoint, embedding_mock) =
        mock_provider(1, |_, _, _| (200, embedding_response(2)));
    let (generation_endpoint, generation_mock) = mock_provider(1, |_, _, _| {
        (200, json!({"oversized":"x".repeat(MAX_RESPONSE_BYTES)}))
    });
    let service = configured(&embedding_endpoint, Provider::Openai);
    let (status, result) = call(&db, Some(&service), Some(&generation_endpoint), question()).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{result}");
    assert_eq!(result["error"]["code"], "invalid_generation_response");
    embedding_mock.join().unwrap();
    generation_mock.join().unwrap();
}

fn citation() -> Citation {
    Citation {
        label: "S1".into(),
        chunk_id: "chunk".into(),
        document_id: "doc".into(),
        title: "Title".into(),
        source: "source".into(),
        start_byte: 0,
        end_byte: 10,
    }
}

#[test]
fn citation_validation_warns_without_claiming_factual_verification() {
    let citations = [citation()];
    assert!(citation_warnings("Supported [S1]", &citations).is_empty());
    assert_eq!(citation_warnings("Unsupported [S99]", &citations).len(), 2);
    assert_eq!(
        citation_warnings("One valid [S1], one unknown [S2]", &citations).len(),
        1
    );
    assert_eq!(citation_warnings("No citation", &citations).len(), 1);
    assert_eq!(
        citation_warnings("Malformed [S0] and [S01]", &citations).len(),
        2
    );
}

#[test]
fn generation_parser_ignores_reasoning_and_reports_incomplete_answers() {
    let mut value = generation_response("Answer [S1]");
    value["status"] = json!("incomplete");
    let answer = parse_generation(&serde_json::to_vec(&value).unwrap()).unwrap();
    assert_eq!(answer.answer, "Answer [S1]");
    assert_eq!(answer.warnings.len(), 1);
    for patch in [
        json!({"status":"failed"}),
        json!({"output":[]}),
        json!({"usage":{"input_tokens":-1,"output_tokens":1}}),
        json!({"model":"untrusted model with spaces"}),
    ] {
        let mut value = generation_response("Answer [S1]");
        value
            .as_object_mut()
            .unwrap()
            .extend(patch.as_object().unwrap().clone());
        assert!(parse_generation(&serde_json::to_vec(&value).unwrap()).is_err());
    }
    assert!(parse_generation(
        &serde_json::to_vec(&generation_response(&"x".repeat(MAX_ANSWER_BYTES + 1))).unwrap()
    )
    .is_err());
}
