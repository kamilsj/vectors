use serde_json::json;
use std::collections::HashMap;
use vectors::{
    Column, ComputeConfig, ComputeDevice, DataType, Database, GraphChunkInput,
    GraphCollectionConfig, GraphDocumentInput, GraphEmbeddingProfile, GraphIngestRequest,
    GraphRagOptions, GraphRagRequest, GraphRagResult, GraphRagSelection, GraphRelationshipRequest,
    Value, Vector, VectorFilterOperator, VectorSearchFilter,
};

fn profile() -> GraphEmbeddingProfile {
    GraphEmbeddingProfile {
        provider: "openai".into(),
        model: "sparse-test".into(),
        dimensions: 2,
        context_format_version: 1,
    }
}

fn database() -> Database {
    let db = Database::new_with_compute(ComputeConfig {
        device: ComputeDevice::Cpu,
        ..ComputeConfig::default()
    });
    db.graph_create_collection_with_columns(
        GraphCollectionConfig {
            name: "sparse".into(),
            profile: profile(),
            semantic_neighbors: 0,
            semantic_threshold: 0.8,
        },
        vec![Column {
            name: "tenant".into(),
            data_type: DataType::Text,
            nullable: false,
            unique: false,
        }],
    )
    .unwrap();
    db
}

fn ingest(db: &Database, id: &str, tenant: &str, parts: &[&str]) {
    let mut text = String::new();
    let chunks = parts
        .iter()
        .enumerate()
        .map(|(ordinal, part)| {
            let start_byte = text.len();
            text.push_str(part);
            GraphChunkInput {
                start_byte,
                end_byte: text.len(),
                text: (*part).into(),
                embedding_text: (*part).into(),
                embedding: Vector::new(vec![1.0, ordinal as f32 * 0.1]).unwrap(),
            }
        })
        .collect();
    db.graph_ingest_document(GraphIngestRequest {
        collection: "sparse".into(),
        expected_revision: db.revision().unwrap(),
        expected_profile: profile(),
        document: GraphDocumentInput {
            id: id.into(),
            title: format!("Title {id}"),
            source: format!("{id}.md"),
            text,
            metadata: json!({"tenant":tenant}),
            chunking: json!({}),
            chunks,
        },
    })
    .unwrap();
}

fn link(db: &Database, from: &str, to: &str) {
    db.graph_upsert_relationship(GraphRelationshipRequest {
        collection: "sparse".into(),
        expected_revision: db.revision().unwrap(),
        from_chunk: format!("{}:{from}:0", from.len()),
        to_chunk: format!("{}:{to}:0", to.len()),
        kind: "references".into(),
        weight: 0.8,
    })
    .unwrap();
}

fn request(text: &str) -> GraphRagRequest {
    GraphRagRequest {
        collection: "sparse".into(),
        expected_profile: profile(),
        query: Vector::new(vec![1.0, 0.0]).unwrap(),
        query_text: text.into(),
        candidate_limit: 100,
        seed_limit: 1,
        max_hops: 0,
        neighbor_limit: 8,
        vector_weight: 0.0,
        lexical_weight: 1.0,
    }
}

fn tenant(value: &str) -> GraphRagOptions {
    GraphRagOptions {
        document_filters: vec![VectorSearchFilter {
            column: "tenant".into(),
            operator: VectorFilterOperator::Eq,
            value: Value::Text(value.into()),
        }],
        ..GraphRagOptions::default()
    }
}

fn retrieve(db: &Database, request: GraphRagRequest, options: GraphRagOptions) -> GraphRagResult {
    let mut result = db
        .graph_rag_candidates_with_options(request, options)
        .unwrap()
        .finalize(
            GraphRagSelection {
                limit: 100,
                diversity: 0.0,
                max_context_bytes: 65536,
                max_per_document: 100,
            },
            None,
        )
        .unwrap();
    result.lexical_cache_hit = false;
    result
}

#[test]
fn selective_filters_preserve_global_bm25_and_precede_both_rankers() {
    let db = database();
    for index in 0..80 {
        let text = if index % 13 == 0 {
            "common rare rare extended passage"
        } else {
            "common passage"
        };
        ingest(
            &db,
            &format!("doc-{index:02}"),
            if index % 17 == 0 { "keep" } else { "other" },
            &[text],
        );
    }
    // Broad unfiltered scoring and a rare-term query exercise both storage
    // strategies. Filtering must not change corpus-level IDF or average length.
    for text in ["common rare", "rare", "missing"] {
        let full = retrieve(&db, request(text), GraphRagOptions::default());
        let scores = full
            .hits
            .iter()
            .map(|hit| (hit.hit.chunk_id.as_str(), hit.lexical_score))
            .collect::<HashMap<_, _>>();
        let filtered = retrieve(&db, request(text), tenant("keep"));
        assert_eq!(
            filtered.hits.len(),
            full.hits
                .iter()
                .filter(|hit| hit.hit.metadata["tenant"] == "keep")
                .count()
        );
        for hit in filtered.hits {
            assert_eq!(hit.hit.metadata["tenant"], "keep");
            assert_eq!(hit.lexical_score, scores[hit.hit.chunk_id.as_str()]);
        }
    }
    for (vector_weight, lexical_weight) in [(1.0, 0.0), (1.0, 1.0), (0.0, 1.0)] {
        let mut query = request("common rare");
        query.candidate_limit = 2;
        query.vector_weight = vector_weight;
        query.lexical_weight = lexical_weight;
        let filtered = retrieve(&db, query.clone(), tenant("keep"));
        assert_eq!(filtered.hits.len(), 2);
        assert!(filtered
            .hits
            .iter()
            .all(|hit| hit.hit.metadata["tenant"] == "keep"));
        let missing = retrieve(&db, query, tenant("absent"));
        assert!(missing.hits.is_empty() && missing.edges.is_empty());
        assert!(!missing.truncated);
    }
}

#[test]
fn matching_all_rows_preserves_order_scores_paths_and_zero_hop_truncation() {
    let db = database();
    ingest(&db, "z", "keep", &["common rare", "common other"]);
    ingest(&db, "a", "keep", &["common rare", "common other"]);
    ingest(&db, "b", "keep", &["common", "common tail"]);
    link(&db, "z", "a");
    link(&db, "a", "b");
    link(&db, "b", "z");
    for max_hops in [0, 1, 3] {
        for (vector_weight, lexical_weight) in [(1.0, 0.0), (1.0, 1.0), (0.0, 1.0)] {
            let mut query = request("common rare");
            query.candidate_limit = 4;
            query.seed_limit = 2;
            query.max_hops = max_hops;
            query.vector_weight = vector_weight;
            query.lexical_weight = lexical_weight;
            let mut all = tenant("keep");
            all.max_seeds_per_document = Some(1);
            let unfiltered = GraphRagOptions {
                document_filters: vec![],
                ..all.clone()
            };
            let expected = retrieve(&db, query.clone(), unfiltered);
            let actual = retrieve(&db, query, all);
            assert_eq!(actual, expected);
            assert_eq!(actual.hits.len(), 4);
            if max_hops == 0 {
                assert!(actual.traversal_seed_ids.is_empty());
                assert!(actual.hits.iter().all(|hit| hit.retrieval_path.is_none()));
            }
        }
    }
}

#[test]
fn sparse_scope_tracks_sql_updates_compaction_and_reopen_without_excluded_bridges() {
    let db = database();
    for index in 0..60 {
        ingest(
            &db,
            &format!("noise-{index}"),
            "other",
            &["unrelated filler"],
        );
    }
    ingest(&db, "seed", "keep", &["needle source"]);
    ingest(&db, "bridge", "other", &["connector"]);
    ingest(&db, "answer", "keep", &["verified fact"]);
    ingest(&db, "loop", "keep", &["cycle context"]);
    link(&db, "seed", "bridge");
    link(&db, "bridge", "answer");
    link(&db, "seed", "loop");
    link(&db, "loop", "seed");
    let mut query = request("needle");
    query.max_hops = 3;
    query.candidate_limit = 8;
    let before = retrieve(&db, query.clone(), tenant("keep"));
    assert_eq!(before.hits.len(), 2);
    assert!(before
        .hits
        .iter()
        .all(|hit| !["bridge", "answer"].contains(&hit.hit.document_id.as_str())));
    db.execute("UPDATE graph_sparse_documents SET tenant = 'keep' WHERE document_id = 'bridge'")
        .unwrap();
    db.graph_delete_document("sparse", "noise-0", db.revision().unwrap())
        .unwrap();
    let expected = retrieve(&db, query.clone(), tenant("keep"));
    assert_eq!(expected.hits.len(), 4);
    let answer = expected
        .hits
        .iter()
        .find(|hit| hit.hit.document_id == "answer")
        .unwrap();
    assert_eq!(answer.hit.text, "verified fact");
    assert_eq!(answer.hit.depth, 2);
    assert_eq!(answer.retrieval_path.as_ref().unwrap().edges.len(), 2);
    let path = std::env::temp_dir().join(format!(
        "vectors-sparse-retrieval-{}.db",
        std::process::id()
    ));
    db.save(&path).unwrap();
    let reopened = Database::open_with_compute(
        &path,
        ComputeConfig {
            device: ComputeDevice::Cpu,
            ..ComputeConfig::default()
        },
    )
    .unwrap();
    // A standalone snapshot opens a fresh revision epoch; all retrieval data
    // and scores must survive unchanged.
    let mut restored = retrieve(&reopened, query, tenant("keep"));
    assert_eq!(restored.revision, reopened.revision().unwrap());
    restored.revision = expected.revision;
    assert_eq!(restored, expected);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn sparse_lookup_keeps_missing_document_and_citation_errors() {
    let db = database();
    ingest(&db, "seed", "keep", &["needle source"]);
    db.execute("UPDATE graph_sparse_documents SET text = 'changed' WHERE document_id = 'seed'")
        .unwrap();
    let citation = db
        .graph_rag_candidates_with_options(request("needle"), tenant("keep"))
        .unwrap_err();
    assert!(citation.to_string().contains("citation no longer matches"));
    db.execute("DELETE FROM graph_sparse_documents WHERE document_id = 'seed'")
        .unwrap();
    let missing = db.graph_rag_candidates(request("needle")).unwrap_err();
    assert!(missing.to_string().contains("missing document"));
}
