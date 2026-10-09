//! Synthetic competing-path probes, shared by regressions and the quality report.
use serde_json::json;
use vectors::{
    Database, GraphChunkInput, GraphCollectionConfig, GraphDocumentInput, GraphEmbeddingProfile,
    GraphIngestRequest, GraphNeighborhoodDirection, GraphRagRequest, GraphRagTraversal,
    GraphRelationshipRequest, Vector,
};

pub fn profile() -> GraphEmbeddingProfile {
    GraphEmbeddingProfile {
        provider: "openai".into(),
        model: "test".into(),
        dimensions: 2,
        context_format_version: 1,
    }
}

pub fn chunk(id: &str) -> String {
    format!("{}:{id}:0", id.len())
}

pub fn link(db: &Database, from: &str, to: &str) {
    db.graph_upsert_relationship(GraphRelationshipRequest {
        collection: "quality".into(),
        expected_revision: db.revision().unwrap(),
        from_chunk: chunk(from),
        to_chunk: chunk(to),
        kind: "references".into(),
        weight: 1.0,
    })
    .unwrap();
}

pub fn fixture(semantic_fit: f32, hops: usize, incoming: bool) -> Database {
    let db = Database::new();
    db.graph_create_collection(GraphCollectionConfig {
        name: "quality".into(),
        profile: profile(),
        semantic_neighbors: 0,
        semantic_threshold: 0.8,
    })
    .unwrap();
    // Three hops need space for two bridge passages as well as the answer.
    let mut passages = vec![
        ("lexical", "ZX429 procedures", 0.1),
        ("semantic", "generic recovery guidance", semantic_fit),
    ];
    for id in ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l"]
        .into_iter()
        .take(candidate_limit(hops))
    {
        passages.push((id, "ZX429 ZX429 ZX429 ZX429", 1.0));
    }
    let bridges = ["bridge1", "bridge2"];
    for &id in bridges.iter().take(hops - 1) {
        passages.push((id, "see the linked manual", 0.0));
    }
    for (id, text, fit) in passages {
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
                    embedding: Vector::new(vec![fit, (1.0 - fit * fit).sqrt()]).unwrap(),
                }],
            },
        })
        .unwrap();
    }
    let connect = |from, to| {
        if incoming {
            link(&db, to, from);
        } else {
            link(&db, from, to);
        }
    };
    let mut previous = "a";
    for &bridge in bridges.iter().take(hops - 1) {
        connect(previous, bridge);
        previous = bridge;
    }
    connect(previous, "lexical");
    connect(previous, "semantic");
    db
}

pub fn request(vector_weight: f64, lexical_weight: f64, hops: usize) -> GraphRagRequest {
    GraphRagRequest {
        collection: "quality".into(),
        expected_profile: profile(),
        query: Vector::new(vec![1.0, 0.0]).unwrap(),
        query_text: "ZX429".into(),
        candidate_limit: candidate_limit(hops),
        seed_limit: 1,
        max_hops: hops,
        neighbor_limit: 1,
        vector_weight,
        lexical_weight,
    }
}

pub fn candidate_limit(hops: usize) -> usize {
    if hops == 3 {
        12
    } else {
        4
    }
}

pub fn policy(direction: GraphNeighborhoodDirection) -> GraphRagTraversal {
    GraphRagTraversal {
        direction,
        kind: Some("references".into()),
        min_weight: 0.5,
    }
}

pub const WEIGHTS: [(&str, f64, f64); 5] = [
    ("keyword_priority", 1.0, 10.0),
    ("semantic_priority", 10.0, 1.0),
    ("balanced", 1.0, 1.0),
    ("keyword_only", 0.0, 1.0),
    ("semantic_only", 1.0, 0.0),
];

pub fn expected(name: &str, semantic_fit: f32) -> &'static str {
    match name {
        "keyword_priority" | "keyword_only" => "lexical",
        "semantic_priority" | "semantic_only" => "semantic",
        "balanced" if semantic_fit > 0.9 => "semantic",
        "balanced" => "lexical",
        _ => panic!("unknown probe"),
    }
}
