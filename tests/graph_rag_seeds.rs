use serde_json::json;
use vectors::{
    Column, ComputeConfig, ComputeDevice, DataType, Database, GraphChunkInput,
    GraphCollectionConfig, GraphDocumentInput, GraphEmbeddingProfile, GraphIngestRequest,
    GraphRagOptions, GraphRagRequest, GraphRagResult, GraphRagSelection, GraphRagTraversal,
    GraphRelationshipRequest, Value, Vector, VectorFilterOperator, VectorSearchFilter,
};

fn profile() -> GraphEmbeddingProfile {
    GraphEmbeddingProfile {
        provider: "openai".into(),
        model: "seed-test".into(),
        dimensions: 2,
        context_format_version: 1,
    }
}

fn ingest(db: &Database, id: &str, tenant: &str, parts: &[(&str, [f32; 2])]) {
    let mut text = String::new();
    let chunks = parts
        .iter()
        .map(|(part, embedding)| {
            if !text.is_empty() {
                text.push('\n');
            }
            let start_byte = text.len();
            text.push_str(part);
            GraphChunkInput {
                start_byte,
                end_byte: text.len(),
                text: (*part).into(),
                embedding_text: (*part).into(),
                embedding: Vector::new(embedding.to_vec()).unwrap(),
            }
        })
        .collect();
    db.graph_ingest_document(GraphIngestRequest {
        collection: "seeds".into(),
        expected_revision: db.revision().unwrap(),
        expected_profile: profile(),
        document: GraphDocumentInput {
            id: id.into(),
            title: format!("Title {id}"),
            source: format!("{id}.md"),
            text,
            metadata: json!({"tenant": tenant}),
            chunking: json!({}),
            chunks,
        },
    })
    .unwrap();
}

fn link(db: &Database, from: &str, to: &str) {
    db.graph_upsert_relationship(GraphRelationshipRequest {
        collection: "seeds".into(),
        expected_revision: db.revision().unwrap(),
        from_chunk: format!("{}:{from}:0", from.len()),
        to_chunk: format!("{}:{to}:0", to.len()),
        kind: "references".into(),
        weight: 1.0,
    })
    .unwrap();
}

fn fixture(reverse: bool, indirect: bool, second_tenant: &str) -> Database {
    let db = Database::new_with_compute(ComputeConfig {
        device: ComputeDevice::Cpu,
        ..ComputeConfig::default()
    });
    db.graph_create_collection_with_columns(
        GraphCollectionConfig {
            name: "seeds".into(),
            profile: profile(),
            semantic_neighbors: 0,
            semantic_threshold: 0.9,
        },
        vec![Column {
            name: "tenant".into(),
            data_type: DataType::Text,
            nullable: false,
            unique: false,
        }],
    )
    .unwrap();
    let mut ids = vec!["a", "b", "d", "answer", "bridge"];
    if reverse {
        ids.reverse();
    }
    for id in ids {
        match id {
            "a" => ingest(
                &db,
                "a",
                "public",
                &[
                    ("Dominant first passage", [1.0, 0.0]),
                    ("Dominant second passage", [0.99, 0.1]),
                    ("Dominant third passage", [0.98, 0.2]),
                ],
            ),
            "b" => ingest(
                &db,
                "b",
                second_tenant,
                &[("Independent evidence source", [0.9, 0.3])],
            ),
            "d" => ingest(&db, "d", "public", &[("Another direct result", [0.8, 0.4])]),
            "answer" => ingest(
                &db,
                "answer",
                "public",
                &[("Żółć: the supporting answer", [0.7, 0.7])],
            ),
            "bridge" => ingest(
                &db,
                "bridge",
                "public",
                &[("Intermediate passage", [0.0, 1.0])],
            ),
            _ => unreachable!(),
        }
    }
    let mut edges = if indirect {
        vec![("b", "bridge"), ("bridge", "answer"), ("answer", "b")]
    } else {
        vec![("b", "answer")]
    };
    if reverse {
        edges.reverse();
    }
    for (from, to) in edges {
        link(&db, from, to);
    }
    db
}

fn request() -> GraphRagRequest {
    GraphRagRequest {
        collection: "seeds".into(),
        expected_profile: profile(),
        query: Vector::new(vec![1.0, 0.0]).unwrap(),
        query_text: "supporting evidence".into(),
        candidate_limit: 4,
        seed_limit: 2,
        max_hops: 1,
        neighbor_limit: 4,
        vector_weight: 1.0,
        lexical_weight: 0.0,
    }
}

fn options(cap: Option<usize>) -> GraphRagOptions {
    GraphRagOptions {
        traversal: GraphRagTraversal {
            kind: Some("references".into()),
            ..GraphRagTraversal::default()
        },
        max_seeds_per_document: cap,
        ..GraphRagOptions::default()
    }
}

fn selection() -> GraphRagSelection {
    GraphRagSelection {
        limit: 4,
        diversity: 0.0,
        max_context_bytes: 4096,
        max_per_document: 4,
    }
}

fn retrieve(db: &Database, request: GraphRagRequest, options: GraphRagOptions) -> GraphRagResult {
    db.graph_rag_candidates_with_options(request, options)
        .unwrap()
        .finalize(selection(), None)
        .unwrap()
}

#[test]
fn diverse_seeds_recover_evidence_from_below_the_initial_direct_pool() {
    let db = fixture(false, false, "public");
    let original = retrieve(&db, request(), options(None));
    assert_eq!(original.candidate_count, 4);
    assert!(!original
        .hits
        .iter()
        .any(|hit| hit.hit.document_id == "answer"));
    assert_eq!(original.hits[3].hit.document_id, "b");

    let diverse = retrieve(&db, request(), options(Some(1)));
    assert_eq!(diverse.candidate_count, 4);
    assert_eq!(diverse.hits.len(), 4);
    let second_seed = diverse
        .hits
        .iter()
        .find(|hit| hit.hit.document_id == "b")
        .unwrap();
    assert_eq!(second_seed.hit.depth, 0);
    assert!(second_seed.hit.seed);
    assert!(second_seed.retrieval_path.is_none());
    let answer = diverse
        .hits
        .iter()
        .find(|hit| hit.hit.document_id == "answer")
        .unwrap();
    assert_eq!(answer.hit.depth, 1);
    let path = answer.retrieval_path.as_ref().unwrap();
    assert_eq!(path.seed_chunk_id, "1:b:0");
    assert_eq!(path.edges.len(), 1);
    assert_eq!(path.edges[0].from_chunk, "1:b:0");
    assert_eq!(path.edges[0].to_chunk, "6:answer:0");
    // The seed cap must not turn into a per-document result cap.
    assert_eq!(
        diverse
            .hits
            .iter()
            .filter(|hit| hit.hit.document_id == "a")
            .count(),
        2
    );
    assert!(diverse.truncated);
}

#[test]
fn fewer_distinct_seeds_leave_graph_capacity_when_seed_and_candidate_limits_match() {
    let db = fixture(false, false, "public");
    let mut query = request();
    query.seed_limit = query.candidate_limit;
    let uncapped = retrieve(&db, query.clone(), options(None));
    assert!(!uncapped
        .hits
        .iter()
        .any(|hit| hit.hit.document_id == "answer"));

    let capped = retrieve(&db, query, options(Some(1)));
    assert_eq!(capped.candidate_count, 4);
    let answer = capped
        .hits
        .iter()
        .find(|hit| hit.hit.document_id == "answer")
        .unwrap();
    assert_eq!(answer.hit.depth, 1);
    assert_eq!(
        answer.retrieval_path.as_ref().unwrap().seed_chunk_id,
        "1:b:0"
    );
}

#[test]
fn lexical_only_retrieval_diversifies_seeds_without_changing_source_citations() {
    let db = fixture(false, false, "public");
    db.execute(
        "UPDATE graph_seeds_chunks SET embedding_text='needle needle needle' WHERE document_id='a';
         UPDATE graph_seeds_chunks SET embedding_text='needle filler filler filler' WHERE document_id='b';",
    )
    .unwrap();
    let mut query = request();
    query.query_text = "needle".into();
    query.vector_weight = 0.0;
    query.lexical_weight = 1.0;
    let uncapped = retrieve(&db, query.clone(), options(None));
    assert_eq!(uncapped.hits[3].hit.document_id, "b");
    assert!(!uncapped
        .hits
        .iter()
        .any(|hit| hit.hit.document_id == "answer"));

    let capped = retrieve(&db, query, options(Some(1)));
    assert_eq!(capped.candidate_count, 4);
    let answer = capped
        .hits
        .iter()
        .find(|hit| hit.hit.document_id == "answer")
        .unwrap();
    assert_eq!(answer.hit.depth, 1);
    assert_eq!(answer.lexical_score, 0.0);
    assert_eq!(
        answer.retrieval_path.as_ref().unwrap().seed_chunk_id,
        "1:b:0"
    );
    let document = db.graph_document("seeds", "answer").unwrap().unwrap();
    assert_eq!(
        &document.text[answer.hit.start_byte..answer.hit.end_byte],
        answer.hit.text
    );
}

#[test]
fn default_options_preserve_existing_retrieval_and_disabled_traversal_ignores_seed_cap() {
    let db = fixture(false, false, "public");
    let original = db
        .graph_rag_candidates(request())
        .unwrap()
        .finalize(selection(), None)
        .unwrap();
    assert_eq!(
        original,
        retrieve(&db, request(), GraphRagOptions::default())
    );
    assert_eq!(
        retrieve(&db, request(), options(None)),
        retrieve(&db, request(), options(Some(2)))
    );

    let mut query = request();
    query.max_hops = 0;
    assert_eq!(
        retrieve(&db, query.clone(), options(None)),
        retrieve(&db, query, options(Some(1)))
    );
}

#[test]
fn capped_seed_selection_is_deterministic_through_a_cycle_and_retains_owned_citations() {
    let first_db = fixture(false, true, "public");
    let second_db = fixture(true, true, "public");
    let mut query = request();
    query.max_hops = 3;
    assert_eq!(
        retrieve(&first_db, query.clone(), options(Some(1))),
        retrieve(&second_db, query.clone(), options(Some(1)))
    );

    let snapshot = first_db
        .graph_rag_candidates_with_options(query, options(Some(1)))
        .unwrap();
    let revision = snapshot.revision;
    assert!(!snapshot
        .candidates
        .iter()
        .any(|item| item.hit.document_id == "bridge"));
    let rerank_scores = snapshot
        .candidates
        .iter()
        .map(|item| {
            if item.hit.document_id == "answer" {
                1.0
            } else {
                0.0
            }
        })
        .collect::<Vec<_>>();
    first_db
        .graph_delete_document("seeds", "b", first_db.revision().unwrap())
        .unwrap();
    first_db
        .graph_delete_document("seeds", "bridge", first_db.revision().unwrap())
        .unwrap();
    let result = snapshot
        .finalize(
            GraphRagSelection {
                limit: 1,
                ..selection()
            },
            Some(&rerank_scores),
        )
        .unwrap();
    assert_eq!(result.revision, revision);
    assert_eq!(result.hits.len(), 1);
    assert!(result.edges.is_empty());
    let answer = &result.hits[0];
    assert_eq!(answer.hit.document_id, "answer");
    assert_eq!(answer.hit.depth, 2);
    assert_eq!(result.context_bytes, answer.hit.text.len());
    let path = answer.retrieval_path.as_ref().unwrap();
    assert_eq!(path.seed_chunk_id, "1:b:0");
    assert_eq!(path.edges.len(), 2);
    assert_eq!(path.edges[0].to_chunk, "6:bridge:0");
    assert_eq!(path.edges[1].from_chunk, "6:bridge:0");
    assert_eq!(path.edges[1].to_chunk, "6:answer:0");
    let document = first_db.graph_document("seeds", "answer").unwrap().unwrap();
    assert_eq!(
        &document.text[answer.hit.start_byte..answer.hit.end_byte],
        answer.hit.text
    );
}

#[test]
fn excluded_documents_cannot_be_promoted_to_seeds_or_used_as_graph_bridges() {
    let db = fixture(false, true, "private");
    let mut query = request();
    query.max_hops = 3;
    let mut filtered = options(Some(1));
    filtered.document_filters = vec![VectorSearchFilter {
        column: "tenant".into(),
        operator: VectorFilterOperator::Eq,
        value: Value::Text("public".into()),
    }];
    let result = retrieve(&db, query, filtered);
    assert_eq!(result.candidate_count, 4);
    assert!(result
        .hits
        .iter()
        .all(|hit| { hit.hit.document_id == "a" || hit.hit.document_id == "d" }));
    assert!(result.hits.iter().all(|hit| hit.retrieval_path.is_none()));
    assert!(result.edges.is_empty());
}

#[test]
fn seed_cap_is_bounded_and_does_not_refill_from_a_capped_document() {
    let db = fixture(false, false, "public");
    for cap in [0, 21, usize::MAX] {
        assert!(db
            .graph_rag_candidates_with_options(request(), options(Some(cap)))
            .is_err());
    }
    assert!(db
        .graph_rag_candidates_with_options(request(), options(Some(20)))
        .is_ok());

    db.graph_upsert_relationship(GraphRelationshipRequest {
        collection: "seeds".into(),
        expected_revision: db.revision().unwrap(),
        from_chunk: "1:a:1".into(),
        to_chunk: "6:answer:0".into(),
        kind: "references".into(),
        weight: 1.0,
    })
    .unwrap();
    // All bounded hybrid matches belong to A. Only its second chunk links to
    // the answer, and the cap must leave that chunk out of the graph frontier.
    let mut query = request();
    query.candidate_limit = 3;
    let uncapped = retrieve(&db, query.clone(), options(None));
    assert!(uncapped
        .hits
        .iter()
        .any(|hit| hit.hit.document_id == "answer"));
    let result = retrieve(&db, query, options(Some(1)));
    assert_eq!(result.candidate_count, 3);
    assert_eq!(result.hits.len(), 3);
    assert!(result
        .hits
        .iter()
        .all(|hit| hit.hit.document_id == "a" && hit.hit.depth == 0));
}
