use serde_json::json;
use vectors::{
    Database, GraphChunkInput, GraphCollectionConfig, GraphDocumentInput, GraphEmbeddingProfile,
    GraphIngestRequest, GraphNeighborhoodDirection, GraphRagRequest, GraphRagSelection,
    GraphRagTraversal, GraphRelationshipRequest, Vector,
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

#[test]
fn evidence_keeps_an_omitted_bridge_after_live_document_deletion() {
    let db = fixture();
    link(&db, "a", "bridge", "references", 1.0);
    link(&db, "bridge", "answer", "explains", 1.0);
    let mut query = request();
    query.seed_limit = 1;
    query.max_hops = 2;
    let snapshot = db.graph_rag_candidates(query).unwrap();
    let revision = snapshot.revision;
    assert!(!snapshot
        .candidates
        .iter()
        .any(|item| item.hit.document_id == "bridge"));
    let scores = snapshot
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
    db.graph_delete_document("quality", "bridge", db.revision().unwrap())
        .unwrap();
    let result = snapshot
        .finalize(
            GraphRagSelection {
                limit: 1,
                diversity: 0.0,
                max_context_bytes: 24,
                max_per_document: 1,
            },
            Some(&scores),
        )
        .unwrap();
    assert_eq!(result.revision, revision);
    assert_eq!(result.hits.len(), 1);
    assert!(result.edges.is_empty());
    let hit = &result.hits[0];
    assert_eq!(hit.hit.document_id, "answer");
    assert_eq!(hit.hit.depth, 2);
    assert_eq!(result.context_bytes, hit.hit.text.len());
    let path = hit.retrieval_path.as_ref().unwrap();
    assert_eq!(path.seed_chunk_id, "1:a:0");
    assert_eq!(path.edges.len(), 2);
    assert_eq!(path.edges[0].from_chunk, "1:a:0");
    assert_eq!(path.edges[0].to_chunk, "6:bridge:0");
    assert_eq!(path.edges[1].from_chunk, "6:bridge:0");
    assert_eq!(path.edges[1].to_chunk, "6:answer:0");
}

#[test]
fn bidirectional_route_retains_original_arrows_through_an_omitted_bridge() {
    let db = fixture();
    link(&db, "bridge", "a", "references", 1.0);
    link(&db, "bridge", "answer", "references", 1.0);
    link(&db, "a", "answer", "semantic", 1.0);
    let mut query = request();
    query.seed_limit = 1;
    query.max_hops = 2;
    let result = db
        .graph_rag_candidates_with_traversal(
            query,
            GraphRagTraversal {
                direction: GraphNeighborhoodDirection::Both,
                kind: Some("references".into()),
                min_weight: 1.0,
            },
        )
        .unwrap();
    let answer = result
        .candidates
        .iter()
        .find(|item| item.hit.document_id == "answer")
        .unwrap();
    assert_eq!(answer.hit.depth, 2);
    let path = answer.retrieval_path.as_ref().unwrap();
    assert_eq!(path.seed_chunk_id, "1:a:0");
    assert_eq!(
        path.edges
            .iter()
            .map(|edge| (edge.from_chunk.as_str(), edge.to_chunk.as_str()))
            .collect::<Vec<_>>(),
        vec![("6:bridge:0", "1:a:0"), ("6:bridge:0", "6:answer:0")]
    );
    assert!(path
        .edges
        .iter()
        .all(|edge| edge.kind == "references" && edge.weight == 1.0));
    assert!(!result
        .candidates
        .iter()
        .any(|item| item.hit.document_id == "bridge"));
    assert!(result.edges.iter().all(|edge| edge.kind == "references"));
}

#[test]
fn stronger_later_route_keeps_matching_depth_score_and_edges() {
    let db = fixture();
    link(&db, "a", "bridge", "references", 1.0);
    link(&db, "a", "answer", "weak", 0.01);
    link(&db, "bridge", "answer", "supports", 1.0);
    let mut query = request();
    query.seed_limit = 1;
    query.neighbor_limit = 2;
    query.max_hops = 2;
    let result = db.graph_rag_candidates(query).unwrap();
    let answer = result
        .candidates
        .iter()
        .find(|item| item.hit.document_id == "answer")
        .unwrap();
    assert_eq!(answer.hit.depth, 2);
    let path = answer.retrieval_path.as_ref().unwrap();
    assert_eq!(path.edges.len(), answer.hit.depth);
    assert_eq!(
        path.edges
            .iter()
            .map(|edge| edge.kind.as_str())
            .collect::<Vec<_>>(),
        vec!["references", "supports"]
    );
    assert!(answer.fusion_score > 1.0 / 64.0);
}

#[test]
fn parallel_relationship_ties_choose_the_same_evidence_regardless_of_insertion_order() {
    let mut paths = Vec::new();
    for kinds in [["supports", "references"], ["references", "supports"]] {
        let db = fixture();
        for kind in kinds {
            link(&db, "a", "answer", kind, 1.0);
        }
        let mut query = request();
        query.seed_limit = 1;
        let result = db.graph_rag_candidates(query).unwrap();
        assert!(result
            .candidates
            .iter()
            .filter(|item| item.hit.depth == 0)
            .all(|item| item.retrieval_path.is_none()));
        paths.push(
            result
                .candidates
                .iter()
                .find(|item| item.hit.document_id == "answer")
                .unwrap()
                .retrieval_path
                .clone()
                .unwrap(),
        );
    }
    assert_eq!(paths[0], paths[1]);
    assert_eq!(paths[0].edges[0].kind, "references");
}

#[test]
fn duplicate_sql_relationships_keep_the_same_strongest_weight_in_paths_and_results() {
    let db = fixture();
    link(&db, "a", "answer", "supports", 0.1);
    db.execute("INSERT INTO graph_quality_edges VALUES ('duplicate', '1:a:0', '6:answer:0', 'supports', 1.0)").unwrap();
    let mut query = request();
    query.seed_limit = 1;
    let snapshot = db.graph_rag_candidates(query).unwrap();
    let path = snapshot
        .candidates
        .iter()
        .find(|item| item.hit.document_id == "answer")
        .unwrap()
        .retrieval_path
        .as_ref()
        .unwrap();
    assert_eq!(path.edges[0].weight, 1.0);
    assert_eq!(snapshot.edges.len(), 1);
    assert_eq!(snapshot.edges[0], path.edges[0]);
    let result = snapshot
        .finalize(
            GraphRagSelection {
                limit: 4,
                diversity: 0.0,
                max_context_bytes: 4096,
                max_per_document: 1,
            },
            None,
        )
        .unwrap();
    assert_eq!(result.edges.len(), 1);
    assert_eq!(result.edges[0].weight, 1.0);
}
