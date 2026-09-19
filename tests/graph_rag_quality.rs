use serde_json::json;
use vectors::{
    Database, GraphChunkInput, GraphCollectionConfig, GraphDocumentInput, GraphEmbeddingProfile,
    GraphIngestRequest, GraphRagRequest, GraphRelationshipRequest, Vector,
};

fn profile() -> GraphEmbeddingProfile {
    GraphEmbeddingProfile {
        provider: "openai".into(),
        model: "text-embedding-3-small".into(),
        dimensions: 3,
        context_format_version: 1,
    }
}
fn fixture() -> Database {
    let db = Database::new();
    db.graph_create_collection(GraphCollectionConfig {
        name: "quality".into(),
        profile: profile(),
        semantic_neighbors: 0,
        semantic_threshold: 0.8,
    })
    .unwrap();
    for (id, text, values) in [
        ("a", "needle alpha", [1.0, 0.0, 0.0]),
        ("b", "needle beta", [0.9, 0.1, 0.0]),
        ("c", "needle gamma", [0.8, 0.2, 0.0]),
        ("answer", "specific relevant answer", [0.7, 0.3, 0.0]),
        ("bridge", "other context", [0.0, 1.0, 0.0]),
    ] {
        db.graph_ingest_document(GraphIngestRequest {
            collection: "quality".into(),
            expected_revision: db.revision().unwrap(),
            expected_profile: profile(),
            document: GraphDocumentInput {
                id: id.into(),
                title: id.into(),
                source: format!("{id}.md"),
                text: text.into(),
                metadata: json!({}),
                chunking: json!({}),
                chunks: vec![GraphChunkInput {
                    start_byte: 0,
                    end_byte: text.len(),
                    text: text.into(),
                    embedding_text: text.into(),
                    embedding: Vector::new(values.to_vec()).unwrap(),
                }],
            },
        })
        .unwrap();
    }
    db
}
fn link(db: &Database, from: &str, to: &str, kind: &str, weight: f64) {
    db.graph_upsert_relationship(GraphRelationshipRequest {
        collection: "quality".into(),
        expected_revision: db.revision().unwrap(),
        from_chunk: format!("{}:{from}:0", from.len()),
        to_chunk: format!("{}:{to}:0", to.len()),
        kind: kind.into(),
        weight,
    })
    .unwrap();
}
fn request() -> GraphRagRequest {
    GraphRagRequest {
        collection: "quality".into(),
        expected_profile: profile(),
        query: Vector::new(vec![1.0, 0.0, 0.0]).unwrap(),
        query_text: "needle".into(),
        candidate_limit: 4,
        seed_limit: 2,
        max_hops: 1,
        neighbor_limit: 1,
        vector_weight: 1.0,
        lexical_weight: 10.0,
    }
}

#[test]
fn later_seeds_relevant_context_beats_early_seeds_irrelevant_neighbor() {
    let db = fixture();
    link(&db, "a", "bridge", "adjacent", 1.0);
    link(&db, "b", "answer", "supports", 1.0);
    let result = db.graph_rag_candidates(request()).unwrap();
    assert_eq!(result.candidates.len(), 4);
    assert!(result
        .candidates
        .iter()
        .any(|item| item.hit.document_id == "answer" && item.hit.depth == 1));
    assert!(!result
        .candidates
        .iter()
        .any(|item| item.hit.document_id == "bridge"));
    assert!(result.truncated);
}

#[test]
fn query_evidence_beats_unrelated_adjacency_with_one_neighbor_budget() {
    let db = fixture();
    link(&db, "a", "bridge", "adjacent", 1.0);
    link(&db, "a", "answer", "semantic", 0.9);
    let mut query = request();
    query.seed_limit = 1;
    let result = db.graph_rag_candidates(query).unwrap();
    assert!(result
        .candidates
        .iter()
        .any(|item| item.hit.document_id == "answer" && item.hit.depth == 1));
    assert!(!result
        .candidates
        .iter()
        .any(|item| item.hit.document_id == "bridge"));
}

#[test]
fn useful_second_hop_can_replace_its_weak_bridge_in_final_context_budget() {
    let db = fixture();
    link(&db, "a", "bridge", "references", 1.0);
    link(&db, "bridge", "answer", "explains", 1.0);
    let mut query = request();
    query.seed_limit = 1;
    let one_hop = db.graph_rag_candidates(query.clone()).unwrap();
    assert!(one_hop
        .candidates
        .iter()
        .any(|item| item.hit.document_id == "bridge"));
    query.max_hops = 2;
    let two_hops = db.graph_rag_candidates(query.clone()).unwrap();
    assert!(two_hops
        .candidates
        .iter()
        .any(|item| item.hit.document_id == "answer" && item.hit.depth == 2));
    assert!(!two_hops
        .candidates
        .iter()
        .any(|item| item.hit.document_id == "bridge"));
    assert_eq!(two_hops.candidates.len(), 4);
    // A cycle must not change the best returned evidence or make work unbounded.
    link(&db, "answer", "a", "references", 1.0);
    query.max_hops = 3;
    let cycled = db.graph_rag_candidates(query).unwrap();
    assert_eq!(
        two_hops
            .candidates
            .iter()
            .map(|item| &item.hit.chunk_id)
            .collect::<Vec<_>>(),
        cycled
            .candidates
            .iter()
            .map(|item| &item.hit.chunk_id)
            .collect::<Vec<_>>()
    );
}

#[test]
fn zero_weight_edges_do_not_consume_the_graph_neighbor_budget() {
    let db = fixture();
    link(&db, "a", "bridge", "adjacent", 0.0);
    link(&db, "a", "answer", "references", 1.0);
    let result = db.graph_rag_candidates(request()).unwrap();
    assert!(result
        .candidates
        .iter()
        .any(|item| item.hit.document_id == "answer" && item.hit.depth == 1));
    assert!(!result
        .candidates
        .iter()
        .any(|item| item.hit.document_id == "bridge"));
}
