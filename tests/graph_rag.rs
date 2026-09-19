use serde_json::json;
use std::sync::Mutex;
use vectors::{
    ComputeConfig, ComputeDevice, Database, Error, GraphBrowseRequest, GraphChunkInput,
    GraphCollectionConfig, GraphDocumentInput, GraphEmbeddingProfile, GraphIngestRequest,
    GraphRagRequest, GraphRagSelection, GraphRelationshipDeleteRequest, GraphRelationshipRequest,
    Vector,
};

// The cache deliberately has a small global eviction budget. Keep cache-hit
// assertions isolated from this test binary's other collection workloads.
static TEST_LOCK: Mutex<()> = Mutex::new(());

fn profile() -> GraphEmbeddingProfile {
    GraphEmbeddingProfile {
        provider: "openai".into(),
        model: "text-embedding-3-small".into(),
        dimensions: 3,
        context_format_version: 1,
    }
}
fn database() -> Database {
    let db = Database::new_with_compute(ComputeConfig {
        device: ComputeDevice::Cpu,
        ..ComputeConfig::default()
    });
    collection(&db);
    db
}
fn collection(db: &Database) {
    db.graph_create_collection(GraphCollectionConfig {
        name: "rag".into(),
        profile: profile(),
        semantic_neighbors: 0,
        semantic_threshold: 0.8,
    })
    .unwrap();
}
fn doc(id: &str, parts: &[(&str, [f32; 3])]) -> GraphDocumentInput {
    let mut text = String::new();
    let chunks = parts
        .iter()
        .map(|(part, values)| {
            let start_byte = text.len();
            text.push_str(part);
            GraphChunkInput {
                start_byte,
                end_byte: text.len(),
                text: (*part).into(),
                embedding_text: (*part).into(),
                embedding: Vector::new(values.to_vec()).unwrap(),
            }
        })
        .collect();
    GraphDocumentInput {
        id: id.into(),
        title: format!("Title {id}"),
        source: format!("https://example.test/{id}"),
        text,
        metadata: json!({"id":id}),
        chunking: json!({"version":1}),
        chunks,
    }
}
fn ingest(db: &Database, document: GraphDocumentInput) {
    db.graph_ingest_document(GraphIngestRequest {
        collection: "rag".into(),
        expected_revision: db.revision().unwrap(),
        expected_profile: profile(),
        document,
    })
    .unwrap();
}
fn request() -> GraphRagRequest {
    GraphRagRequest {
        collection: "rag".into(),
        expected_profile: profile(),
        query: Vector::new(vec![1.0, 0.0, 0.0]).unwrap(),
        query_text: "database wal".into(),
        candidate_limit: 10,
        seed_limit: 2,
        max_hops: 0,
        neighbor_limit: 4,
        vector_weight: 1.0,
        lexical_weight: 1.0,
    }
}
fn selection() -> GraphRagSelection {
    GraphRagSelection {
        limit: 10,
        diversity: 0.0,
        max_context_bytes: 24_000,
        max_per_document: 3,
    }
}
fn browse() -> GraphBrowseRequest {
    GraphBrowseRequest {
        collection: "rag".into(),
        document_id: None,
        offset: 0,
        limit: 100,
        max_edges: 100,
    }
}
fn relationship(
    db: &Database,
    from: &str,
    to: &str,
    kind: &str,
    weight: f64,
) -> vectors::GraphRelationshipResult {
    db.graph_upsert_relationship(GraphRelationshipRequest {
        collection: "rag".into(),
        expected_revision: db.revision().unwrap(),
        from_chunk: from.into(),
        to_chunk: to.into(),
        kind: kind.into(),
        weight,
    })
    .unwrap()
}

#[test]
fn hybrid_rrf_recovers_rare_lexical_match_and_is_deterministic() {
    let _guard = TEST_LOCK.lock().unwrap();
    let db = database();
    ingest(
        &db,
        doc("a", &[("general unrelated prose", [1.0, 0.0, 0.0])]),
    );
    ingest(&db, doc("b", &[("database WAL recovery", [0.0, 1.0, 0.0])]));
    let first = db.graph_rag_candidates(request()).unwrap();
    assert!(!first.lexical_cache_hit);
    assert_eq!(first.candidates[0].hit.document_id, "b");
    assert!(first.candidates[0].lexical_score > 0.0);
    let expected_rrf = 1.0 / 62.0 + 1.0 / 61.0;
    assert!((first.candidates[0].fusion_score - expected_rrf).abs() < 1e-12);
    let second = db.graph_rag_candidates(request()).unwrap();
    assert!(second.lexical_cache_hit);
    let mut first = first.finalize(selection(), None).unwrap();
    let second = second.finalize(selection(), None).unwrap();
    first.lexical_cache_hit = true;
    assert_eq!(first, second);
    assert!(second.hits.iter().all(|hit| hit.rerank_score.is_none()));
    let mut vector_only = request();
    vector_only.lexical_weight = 0.0;
    assert_eq!(
        db.graph_rag_candidates(vector_only).unwrap().candidates[0]
            .hit
            .document_id,
        "a"
    );
    let mut lexical_only = request();
    lexical_only.vector_weight = 0.0;
    let result = db.graph_rag_candidates(lexical_only).unwrap();
    assert_eq!(result.candidates.len(), 1);
    assert_eq!(result.candidates[0].hit.document_id, "b");
}

#[test]
fn lexical_cache_invalidates_after_sql_graph_writes_and_snapshot_restore() {
    let _guard = TEST_LOCK.lock().unwrap();
    let db = database();
    ingest(&db, doc("a", &[("database wal", [1.0, 0.0, 0.0])]));
    ingest(&db, doc("b", &[("other subject", [0.0, 1.0, 0.0])]));
    assert!(
        !db.graph_rag_candidates(request())
            .unwrap()
            .lexical_cache_hit
    );
    assert!(
        db.graph_rag_candidates(request())
            .unwrap()
            .lexical_cache_hit
    );
    db.execute("UPDATE graph_rag_chunks SET embedding_text='other subject' WHERE document_id='a'; UPDATE graph_rag_chunks SET embedding_text='database wal' WHERE document_id='b';").unwrap();
    let after = db.graph_rag_candidates(request()).unwrap();
    assert!(!after.lexical_cache_hit);
    assert!(
        after
            .candidates
            .iter()
            .find(|candidate| candidate.hit.document_id == "a")
            .unwrap()
            .lexical_score
            == 0.0
    );
    assert!(
        after
            .candidates
            .iter()
            .find(|candidate| candidate.hit.document_id == "b")
            .unwrap()
            .lexical_score
            > 0.0
    );
    assert!(
        db.graph_rag_candidates(request())
            .unwrap()
            .lexical_cache_hit
    );
    relationship(&db, "1:a:0", "1:b:0", "supports", 0.7);
    assert!(
        db.graph_rag_candidates(request())
            .unwrap()
            .lexical_cache_hit
    );
    assert!(
        db.graph_rag_candidates(request())
            .unwrap()
            .lexical_cache_hit
    );
    let path =
        std::env::temp_dir().join(format!("vectors-rag-snapshot-{}.vdb", std::process::id()));
    db.save(&path).unwrap();
    let reopened = Database::open(&path).unwrap();
    assert!(
        !reopened
            .graph_rag_candidates(request())
            .unwrap()
            .lexical_cache_hit
    );
    std::fs::remove_file(path).unwrap();
}

#[test]
fn lexical_cache_survives_unrelated_writes_and_failed_chunk_batches() {
    let _guard = TEST_LOCK.lock().unwrap();
    let db = database();
    ingest(&db, doc("a", &[("database wal", [1.0, 0.0, 0.0])]));
    assert!(
        !db.graph_rag_candidates(request())
            .unwrap()
            .lexical_cache_hit
    );
    db.execute("CREATE TABLE unrelated (id INTEGER UNIQUE); INSERT INTO unrelated VALUES (1); UPDATE unrelated SET id=2; DELETE FROM unrelated;").unwrap();
    assert!(
        db.graph_rag_candidates(request())
            .unwrap()
            .lexical_cache_hit
    );
    db.execute("UPDATE graph_rag_documents SET title='New citation title' WHERE document_id='a'")
        .unwrap();
    let changed_title = db.graph_rag_candidates(request()).unwrap();
    assert!(changed_title.lexical_cache_hit);
    assert_eq!(changed_title.candidates[0].hit.title, "New citation title");
    let before = db.revision().unwrap();
    assert!(db.execute("UPDATE graph_rag_chunks SET embedding_text='uncommitted replacement'; INSERT INTO unrelated VALUES (3), (3);").is_err());
    assert_eq!(db.revision().unwrap(), before);
    let unchanged = db.graph_rag_candidates(request()).unwrap();
    assert!(unchanged.lexical_cache_hit);
    assert!(unchanged.candidates[0].lexical_score > 0.0);
    assert!(db.execute("TRUNCATE TABLE graph_rag_chunks").is_err());
    assert!(
        db.graph_rag_candidates(request())
            .unwrap()
            .lexical_cache_hit
    );
}

#[test]
fn lexical_cache_invalidates_typed_append_and_text_only_upsert_but_not_failed_insert() {
    let _guard = TEST_LOCK.lock().unwrap();
    let db = database();
    ingest(&db, doc("a", &[("database wal", [1.0, 0.0, 0.0])]));
    assert!(
        !db.graph_rag_candidates(request())
            .unwrap()
            .lexical_cache_hit
    );
    let result = db.execute("SELECT * FROM graph_rag_chunks").unwrap();
    let vectors::ExecutionResult::Query(result) = &result[0] else {
        panic!("expected rows")
    };
    let original = result.rows[0].clone();
    let mut appended = original.clone();
    appended[0] = vectors::Value::Text("typed-extra".into());
    db.insert_rows(
        "graph_rag_chunks",
        vec![appended.clone()],
        vectors::InsertConflict::Fail,
    )
    .unwrap();
    let after_append = db.graph_rag_candidates(request()).unwrap();
    assert!(!after_append.lexical_cache_hit);
    assert_eq!(after_append.candidates.len(), 2);
    appended[6] = vectors::Value::Text("different terminology".into());
    db.insert_rows(
        "graph_rag_chunks",
        vec![appended],
        vectors::InsertConflict::DoUpdate {
            target: "chunk_id".into(),
            update_columns: vec!["embedding_text".into()],
        },
    )
    .unwrap();
    let after_update = db.graph_rag_candidates(request()).unwrap();
    assert!(!after_update.lexical_cache_hit);
    assert_eq!(
        after_update
            .candidates
            .iter()
            .find(|candidate| candidate.hit.chunk_id == "typed-extra")
            .unwrap()
            .lexical_score,
        0.0
    );
    assert!(db
        .insert_rows(
            "graph_rag_chunks",
            vec![original.clone(), original],
            vectors::InsertConflict::Fail
        )
        .is_err());
    assert!(
        db.graph_rag_candidates(request())
            .unwrap()
            .lexical_cache_hit
    );
}

#[test]
fn lexical_cache_invalidates_delete_all_graph_replacement_and_drop_recreate() {
    let _guard = TEST_LOCK.lock().unwrap();
    let db = database();
    ingest(&db, doc("a", &[("database wal", [1.0, 0.0, 0.0])]));
    assert!(
        !db.graph_rag_candidates(request())
            .unwrap()
            .lexical_cache_hit
    );
    ingest(&db, doc("a", &[("different subject", [1.0, 0.0, 0.0])]));
    let replaced = db.graph_rag_candidates(request()).unwrap();
    assert!(!replaced.lexical_cache_hit);
    assert_eq!(replaced.candidates[0].lexical_score, 0.0);
    db.graph_delete_document("rag", "a", db.revision().unwrap())
        .unwrap();
    let deleted = db.graph_rag_candidates(request()).unwrap();
    assert!(!deleted.lexical_cache_hit);
    assert!(deleted.candidates.is_empty());
    ingest(&db, doc("b", &[("database wal", [1.0, 0.0, 0.0])]));
    assert!(
        !db.graph_rag_candidates(request())
            .unwrap()
            .lexical_cache_hit
    );
    db.execute("DELETE FROM graph_rag_chunks").unwrap();
    let empty = db.graph_rag_candidates(request()).unwrap();
    assert!(!empty.lexical_cache_hit);
    assert!(empty.candidates.is_empty());
    assert!(
        db.graph_rag_candidates(request())
            .unwrap()
            .lexical_cache_hit
    );
    db.execute(
        "DROP TABLE graph_rag_edges, graph_rag_chunks, graph_rag_documents, graph_rag_config",
    )
    .unwrap();
    collection(&db);
    ingest(&db, doc("c", &[("database wal restored", [1.0, 0.0, 0.0])]));
    let recreated = db.graph_rag_candidates(request()).unwrap();
    assert!(!recreated.lexical_cache_hit);
    assert_eq!(recreated.candidates.len(), 1);
    assert_eq!(recreated.candidates[0].hit.document_id, "c");
}

#[test]
fn lexical_cache_is_cold_after_wal_and_checkpoint_reopen() {
    let _guard = TEST_LOCK.lock().unwrap();
    let directory = std::env::temp_dir().join(format!(
        "vectors-rag-cache-reopen-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    {
        let db = Database::open_persistent(&directory).unwrap();
        collection(&db);
        ingest(&db, doc("a", &[("database wal", [1.0, 0.0, 0.0])]));
        assert!(
            !db.graph_rag_candidates(request())
                .unwrap()
                .lexical_cache_hit
        );
        assert!(
            db.graph_rag_candidates(request())
                .unwrap()
                .lexical_cache_hit
        );
    }
    for _ in 0..2 {
        let db = Database::open_persistent(&directory).unwrap();
        let reopened = db.graph_rag_candidates(request()).unwrap();
        assert!(!reopened.lexical_cache_hit);
        assert_eq!(reopened.candidates[0].hit.document_id, "a");
        assert!(reopened.candidates[0].lexical_score > 0.0);
        assert!(
            db.graph_rag_candidates(request())
                .unwrap()
                .lexical_cache_hit
        );
        db.checkpoint().unwrap();
    }
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn lexical_cache_evicts_the_least_recent_of_more_than_three_live_indexes() {
    let _guard = TEST_LOCK.lock().unwrap();
    let databases = (0..4)
        .map(|_| {
            let db = database();
            ingest(&db, doc("a", &[("database wal", [1.0, 0.0, 0.0])]));
            db
        })
        .collect::<Vec<_>>();
    for db in &databases[..3] {
        assert!(
            !db.graph_rag_candidates(request())
                .unwrap()
                .lexical_cache_hit
        );
    }
    assert!(
        databases[0]
            .graph_rag_candidates(request())
            .unwrap()
            .lexical_cache_hit
    );
    assert!(
        !databases[3]
            .graph_rag_candidates(request())
            .unwrap()
            .lexical_cache_hit
    );
    assert!(
        databases[0]
            .graph_rag_candidates(request())
            .unwrap()
            .lexical_cache_hit
    );
    assert!(
        databases[2]
            .graph_rag_candidates(request())
            .unwrap()
            .lexical_cache_hit
    );
    assert!(
        !databases[1]
            .graph_rag_candidates(request())
            .unwrap()
            .lexical_cache_hit
    );
}

#[test]
fn lexical_cache_oversized_vocabulary_uses_exact_uncached_query_scoring() {
    let _guard = TEST_LOCK.lock().unwrap();
    let db = database();
    let texts = (0..100)
        .map(|chunk| {
            (0..1_000)
                .map(|term| format!("t{}", chunk * 1_000 + term))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>();
    let parts = texts
        .iter()
        .map(|text| (text.as_str(), [1.0, 0.0, 0.0]))
        .collect::<Vec<_>>();
    ingest(&db, doc("large", &parts));
    let mut query = request();
    query.query_text = "t42".into();
    query.vector_weight = 0.0;
    let expected = (1.0_f64 + (100.0 - 1.0 + 0.5) / (1.0 + 0.5)).ln();
    let mut score = None;
    for _ in 0..2 {
        let result = db.graph_rag_candidates(query.clone()).unwrap();
        assert!(!result.lexical_cache_hit);
        assert_eq!(result.candidates.len(), 1);
        assert_eq!(result.candidates[0].hit.chunk_id, "5:large:0");
        let current = result.candidates[0].lexical_score;
        assert!((current - expected).abs() < 1e-12);
        if let Some(previous) = score {
            assert_eq!(current.to_bits(), previous);
        }
        score = Some(current.to_bits());
    }
}

#[test]
fn lexical_cache_precomputed_length_factors_preserve_exact_bm25_scores() {
    let _guard = TEST_LOCK.lock().unwrap();
    let db = database();
    let corpus = [
        ("!!!", 0, [0, 0]),
        ("database", 1, [1, 0]),
        ("wal wal", 2, [0, 2]),
        ("database wal filler filler filler", 5, [1, 1]),
        ("DATABASE DATABASE database wal wal x x x x", 9, [3, 2]),
    ];
    for (index, (text, _, _)) in corpus.iter().enumerate() {
        ingest(&db, doc(&format!("d{index}"), &[(text, [1.0, 0.0, 0.0])]));
    }
    let mut query = request();
    query.vector_weight = 0.0;
    let average_length = 17.0 / 5.0;
    let idf = (1.0_f64 + (5.0 - 3.0 + 0.5) / (3.0 + 0.5)).ln();
    for warm in [false, true] {
        let result = db.graph_rag_candidates(query.clone()).unwrap();
        assert_eq!(result.lexical_cache_hit, warm);
        assert_eq!(result.candidates.len(), 4);
        for (index, (_, length, frequencies)) in corpus.iter().enumerate().skip(1) {
            let mut expected = 0.0;
            for frequency in frequencies {
                if *frequency == 0 {
                    continue;
                }
                let tf = *frequency as f64;
                let denominator = tf + 1.2 * (0.25 + 0.75 * *length as f64 / average_length);
                expected += idf * tf * 2.2 / denominator;
            }
            let candidate = result
                .candidates
                .iter()
                .find(|candidate| candidate.hit.document_id == format!("d{index}"))
                .unwrap();
            assert_eq!(candidate.lexical_score.to_bits(), expected.to_bits());
        }
    }
}

#[test]
fn graph_context_traverses_direct_hybrid_bridge_and_edges_join_selected_hits() {
    let _guard = TEST_LOCK.lock().unwrap();
    let db = database();
    for (id, vector) in [
        ("a", [1.0, 0.0, 0.0]),
        ("b", [0.9, 0.1, 0.0]),
        ("c", [0.8, 0.2, 0.0]),
        ("d", [0.0, 1.0, 0.0]),
    ] {
        ingest(&db, doc(id, &[(&format!("unique material {id}"), vector)]));
    }
    relationship(&db, "1:a:0", "1:b:0", "references", 1.0);
    relationship(&db, "1:b:0", "1:d:0", "supports", 0.9);
    let mut query = request();
    query.candidate_limit = 3;
    query.seed_limit = 1;
    query.max_hops = 2;
    query.lexical_weight = 0.0;
    let snapshot = db.graph_rag_candidates(query).unwrap();
    assert_eq!(snapshot.candidates.len(), 3);
    assert_eq!(
        snapshot
            .candidates
            .iter()
            .find(|candidate| candidate.hit.document_id == "b")
            .unwrap()
            .hit
            .depth,
        0
    );
    let context = snapshot
        .candidates
        .iter()
        .find(|candidate| candidate.hit.document_id == "d")
        .unwrap();
    assert_eq!(context.hit.depth, 2);
    assert!(!context.hit.seed);
    assert!(!snapshot
        .candidates
        .iter()
        .any(|candidate| candidate.hit.document_id == "c"));
    assert_eq!(snapshot.edges.len(), 2);
    let result = snapshot.finalize(selection(), None).unwrap();
    assert_eq!(result.edges.len(), 2);
    for edge in &result.edges {
        assert!(result
            .hits
            .iter()
            .any(|hit| hit.hit.chunk_id == edge.from_chunk));
        assert!(result
            .hits
            .iter()
            .any(|hit| hit.hit.chunk_id == edge.to_chunk));
    }
}

#[test]
fn mmr_uses_vector_diversity_and_suppresses_copied_text() {
    let _guard = TEST_LOCK.lock().unwrap();
    let db = database();
    ingest(&db, doc("a", &[("database alpha", [1.0, 0.0, 0.0])]));
    ingest(&db, doc("b", &[("database beta", [0.99, 0.1, 0.0])]));
    ingest(&db, doc("c", &[("database gamma", [0.0, 1.0, 0.0])]));
    let mut query = request();
    query.lexical_weight = 0.0;
    let snapshot = db.graph_rag_candidates(query.clone()).unwrap();
    let mut options = selection();
    options.limit = 2;
    let direct = snapshot.clone().finalize(options.clone(), None).unwrap();
    assert_eq!(direct.hits[1].hit.document_id, "b");
    options.diversity = 1.0;
    let diverse = snapshot.finalize(options.clone(), None).unwrap();
    assert_eq!(diverse.hits[0].hit.document_id, "a");
    assert_eq!(diverse.hits[1].hit.document_id, "c");
    ingest(&db, doc("b", &[("  DATABASE   alpha  ", [0.99, 0.1, 0.0])]));
    options.diversity = 0.3;
    let deduplicated = db
        .graph_rag_candidates(query)
        .unwrap()
        .finalize(options, None)
        .unwrap();
    assert_eq!(deduplicated.hits.len(), 2);
    assert_eq!(deduplicated.hits[1].hit.document_id, "c");
}

#[test]
fn overlap_budget_and_per_document_caps_keep_whole_utf8_citations() {
    let _guard = TEST_LOCK.lock().unwrap();
    let db = database();
    let mut overlap = doc("a", &[("abcdefghijXYZ", [1.0, 0.0, 0.0])]);
    overlap.chunks = vec![
        GraphChunkInput {
            start_byte: 0,
            end_byte: 10,
            text: "abcdefghij".into(),
            embedding_text: "abcdefghij".into(),
            embedding: Vector::new(vec![1.0, 0.0, 0.0]).unwrap(),
        },
        GraphChunkInput {
            start_byte: 2,
            end_byte: 13,
            text: "cdefghijXYZ".into(),
            embedding_text: "cdefghijXYZ".into(),
            embedding: Vector::new(vec![0.99, 0.1, 0.0]).unwrap(),
        },
    ];
    ingest(&db, overlap);
    ingest(&db, doc("b", &[("Żółć", [0.0, 1.0, 0.0])]));
    let mut query = request();
    query.lexical_weight = 0.0;
    let snapshot = db.graph_rag_candidates(query).unwrap();
    let mut options = selection();
    options.diversity = 0.3;
    let result = snapshot.clone().finalize(options.clone(), None).unwrap();
    assert_eq!(result.hits.len(), 2);
    assert_eq!(
        result
            .hits
            .iter()
            .filter(|hit| hit.hit.document_id == "a")
            .count(),
        1
    );
    options.diversity = 0.0;
    options.max_per_document = 1;
    assert_eq!(
        snapshot
            .clone()
            .finalize(options.clone(), None)
            .unwrap()
            .hits
            .len(),
        2
    );
    options.max_context_bytes = "Żółć".len();
    let result = snapshot.finalize(options, None).unwrap();
    assert_eq!(result.hits.len(), 1);
    assert_eq!(result.hits[0].hit.text, "Żółć");
    assert_eq!(result.context_bytes, "Żółć".len());
    assert!(result.truncated);
    let source = db.graph_document("rag", "b").unwrap().unwrap();
    let hit = &result.hits[0].hit;
    assert_eq!(&source.text[hit.start_byte..hit.end_byte], hit.text);
}

#[test]
fn external_reranking_uses_owned_snapshot_after_document_deletion() {
    let _guard = TEST_LOCK.lock().unwrap();
    let db = database();
    ingest(&db, doc("a", &[("database one", [1.0, 0.0, 0.0])]));
    ingest(&db, doc("b", &[("database two", [0.0, 1.0, 0.0])]));
    let snapshot = db.graph_rag_candidates(request()).unwrap();
    let revision = snapshot.revision;
    let scores = snapshot
        .candidates
        .iter()
        .map(|candidate| {
            if candidate.hit.document_id == "b" {
                0.9
            } else {
                0.1
            }
        })
        .collect::<Vec<_>>();
    db.graph_delete_document("rag", "b", revision).unwrap();
    let mut options = selection();
    options.limit = 1;
    let result = snapshot.finalize(options, Some(&scores)).unwrap();
    assert_eq!(result.revision, revision);
    assert!(db.revision().unwrap() > revision);
    assert_eq!(result.hits[0].hit.document_id, "b");
    assert_eq!(result.hits[0].rerank_score, Some(0.9));
    assert_eq!(result.candidate_count, 2);
    assert_eq!(result.hits[0].hit.text, "database two");
    let snapshot = db.graph_rag_candidates(request()).unwrap();
    assert!(snapshot.clone().finalize(selection(), Some(&[])).is_err());
    assert!(snapshot.finalize(selection(), Some(&[f64::NAN])).is_err());
}

#[test]
fn browsing_pages_are_deterministic_and_relationship_edits_are_cas_atomic() {
    let _guard = TEST_LOCK.lock().unwrap();
    let db = database();
    ingest(&db, doc("z", &[("Żółć", [1.0, 0.0, 0.0])]));
    ingest(
        &db,
        doc(
            "a",
            &[("First. ", [1.0, 0.0, 0.0]), ("Second.", [0.0, 1.0, 0.0])],
        ),
    );
    let first = db.graph_browse(browse()).unwrap();
    assert_eq!(
        first
            .nodes
            .iter()
            .map(|node| node.document_id.as_str())
            .collect::<Vec<_>>(),
        vec!["a", "a", "z"]
    );
    assert_eq!(first.edges.len(), 2);
    let mut page = browse();
    page.document_id = Some("a".into());
    page.offset = 1;
    page.limit = 1;
    let page = db.graph_browse(page).unwrap();
    assert_eq!(page.nodes[0].ordinal, 1);
    assert_eq!(page.total_nodes, 2);
    assert!(page.edges.is_empty());
    let before = db.revision().unwrap();
    let created = relationship(&db, "1:a:0", "1:z:0", "supports", 0.8);
    assert!(created.created);
    let failed = db.graph_upsert_relationship(GraphRelationshipRequest {
        collection: "rag".into(),
        expected_revision: before,
        from_chunk: "1:a:0".into(),
        to_chunk: "1:z:0".into(),
        kind: "supports".into(),
        weight: 0.1,
    });
    assert!(matches!(failed, Err(Error::RevisionConflict { .. })));
    let updated = relationship(&db, "1:a:0", "1:z:0", "supports", 0.4);
    assert!(!updated.created);
    let edges = db.graph_browse(browse()).unwrap().edges;
    assert_eq!(
        edges.iter().filter(|edge| edge.kind == "supports").count(),
        1
    );
    assert_eq!(
        edges
            .iter()
            .find(|edge| edge.kind == "supports")
            .unwrap()
            .weight,
        0.4
    );
    let before = db.revision().unwrap();
    let failed = db.graph_upsert_relationship(GraphRelationshipRequest {
        collection: "rag".into(),
        expected_revision: before,
        from_chunk: "missing".into(),
        to_chunk: "1:z:0".into(),
        kind: "supports".into(),
        weight: 0.1,
    });
    assert!(failed.is_err());
    assert_eq!(db.revision().unwrap(), before);
    let deleted = db
        .graph_delete_relationship(GraphRelationshipDeleteRequest {
            collection: "rag".into(),
            expected_revision: before,
            from_chunk: "1:a:0".into(),
            to_chunk: "1:z:0".into(),
            kind: "supports".into(),
        })
        .unwrap();
    assert_eq!(deleted.edges_removed, 1);
    let mut bounded = browse();
    bounded.max_edges = 1;
    let page = db.graph_browse(bounded).unwrap();
    assert_eq!(page.edges.len(), 1);
    assert!(page.truncated);
}

#[test]
fn relationship_mutations_survive_wal_recovery_and_checkpoint() {
    let _guard = TEST_LOCK.lock().unwrap();
    let directory =
        std::env::temp_dir().join(format!("vectors-rag-relations-{}", std::process::id()));
    let db = Database::open_persistent(&directory).unwrap();
    collection(&db);
    ingest(&db, doc("a", &[("source", [1.0, 0.0, 0.0])]));
    ingest(&db, doc("b", &[("target", [0.0, 1.0, 0.0])]));
    relationship(&db, "1:a:0", "1:b:0", "supports", 0.4);
    let expected = db.graph_browse(browse()).unwrap();
    drop(db);
    let db = Database::open_persistent(&directory).unwrap();
    assert_eq!(db.graph_browse(browse()).unwrap(), expected);
    let changed = relationship(&db, "1:a:0", "1:b:0", "supports", 0.9);
    assert!(!changed.created);
    db.checkpoint().unwrap();
    drop(db);
    let db = Database::open_persistent(&directory).unwrap();
    assert_eq!(db.graph_browse(browse()).unwrap().edges[0].weight, 0.9);
    db.graph_delete_relationship(GraphRelationshipDeleteRequest {
        collection: "rag".into(),
        expected_revision: db.revision().unwrap(),
        from_chunk: "1:a:0".into(),
        to_chunk: "1:b:0".into(),
        kind: "supports".into(),
    })
    .unwrap();
    drop(db);
    let db = Database::open_persistent(&directory).unwrap();
    assert!(db.graph_browse(browse()).unwrap().edges.is_empty());
    drop(db);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn invalid_rag_limits_and_profiles_fail_before_retrieval() {
    let _guard = TEST_LOCK.lock().unwrap();
    let db = database();
    assert!(GraphRagRequest::validate_query_text(" ").is_err());
    assert!(GraphRagRequest::validate_query_text(
        &(0..257)
            .map(|index| format!("word{index}"))
            .collect::<Vec<_>>()
            .join(" ")
    )
    .is_err());
    let mut query = request();
    query.vector_weight = f64::NAN;
    assert!(db.graph_rag_candidates(query).is_err());
    let mut query = request();
    query.candidate_limit = 101;
    assert!(db.graph_rag_candidates(query).is_err());
    let mut query = request();
    query.expected_profile.model = "different".into();
    assert!(db.graph_rag_candidates(query).is_err());
    let mut query = request();
    query.query = Vector::new(vec![1.0, 0.0]).unwrap();
    assert!(matches!(
        db.graph_rag_candidates(query),
        Err(Error::DimensionMismatch { .. })
    ));
    let empty = db
        .graph_rag_candidates(request())
        .unwrap()
        .finalize(selection(), None)
        .unwrap();
    assert!(empty.hits.is_empty());
    assert_eq!(empty.context_bytes, 0);
}

#[test]
fn graph_context_keeps_stronger_later_path_and_cycles_remain_bounded() {
    let _guard = TEST_LOCK.lock().unwrap();
    let db = database();
    for (id, vector) in [
        ("a", [1.0, 0.0, 0.0]),
        ("b", [0.9, 0.1, 0.0]),
        ("c", [0.8, 0.2, 0.0]),
        ("d", [0.7, 0.3, 0.0]),
        ("z", [0.0, 1.0, 0.0]),
    ] {
        ingest(&db, doc(id, &[(&format!("unique material {id}"), vector)]));
    }
    relationship(&db, "1:a:0", "1:z:0", "references", 0.01);
    relationship(&db, "1:b:0", "1:z:0", "supports", 1.0);
    relationship(&db, "1:z:0", "1:a:0", "references", 1.0);
    let mut query = request();
    query.candidate_limit = 4;
    query.seed_limit = 2;
    query.max_hops = 3;
    query.lexical_weight = 0.0;
    let result = db.graph_rag_candidates(query).unwrap();
    assert_eq!(result.candidates.len(), 4);
    let context = result
        .candidates
        .iter()
        .find(|candidate| candidate.hit.document_id == "z")
        .unwrap();
    assert_eq!(context.hit.depth, 1);
    // The stronger path comes from the second seed (rank 2). This
    // orthogonal passage receives the query-affinity floor of 0.2.
    assert!((context.fusion_score - 0.2 * 0.5 / 62.0).abs() < 1e-12);
}
