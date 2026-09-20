//! Fixed-size graph exploration as unrelated document/chunk counts grow.
//! cargo run --release --example benchmark_graph_neighborhood -- 10000 100 [result.json]
//! Fixture setup, vector search, providers and JSON encoding are outside timing.
use std::time::Instant;
use vectors::{
    ComputeConfig, ComputeDevice, Database, GraphCollectionConfig, GraphEmbeddingProfile,
    GraphNeighborhoodDirection, GraphNeighborhoodRequest, InsertConflict, Value, Vector,
};

fn text(value: impl ToString) -> Value {
    Value::Text(value.to_string())
}

fn main() {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    assert!(
        arguments.len() <= 3,
        "expected rows repetitions [result.json]"
    );
    let count: usize = arguments
        .first()
        .map_or(10_000, |value| value.parse().unwrap());
    let repetitions: usize = arguments.get(1).map_or(100, |value| value.parse().unwrap());
    assert!((4..=10_000).contains(&count) && repetitions > 0);
    let database = Database::new_with_compute(ComputeConfig {
        device: ComputeDevice::Cpu,
        ..ComputeConfig::default()
    });
    let profile = GraphEmbeddingProfile {
        provider: "openai".into(),
        model: "text-embedding-3-small".into(),
        dimensions: 3,
        context_format_version: 1,
    };
    let collection = database
        .graph_create_collection(GraphCollectionConfig {
            name: "benchmark".into(),
            profile: profile.clone(),
            semantic_neighbors: 0,
            semantic_threshold: 0.8,
        })
        .unwrap();
    let key = serde_json::to_string(&profile).unwrap();
    let vector = Vector::new(vec![1.0, 0.0, 0.0]).unwrap();
    let mut documents = Vec::with_capacity(count);
    let mut chunks = Vec::with_capacity(count);
    for index in 0..count {
        let content = format!("Source passage {index}: committed writes recover from WAL.");
        documents.push(vec![
            text(index),
            text("Guide"),
            text("source.md"),
            text(&content),
            text("{}"),
            text("{}"),
            text(""),
        ]);
        chunks.push(vec![
            text(index),
            text(index),
            Value::Integer(0),
            Value::Integer(0),
            Value::Integer(content.len() as i64),
            text(&content),
            text(&content),
            text(&key),
            Value::Vector(vector.clone()),
        ]);
    }
    database
        .insert_rows(
            &collection.tables.documents,
            documents,
            InsertConflict::Fail,
        )
        .unwrap();
    database
        .insert_rows(&collection.tables.chunks, chunks, InsertConflict::Fail)
        .unwrap();
    database
        .insert_rows(
            &collection.tables.edges,
            (0..3)
                .map(|index| {
                    vec![
                        text(index),
                        text(index),
                        text(index + 1),
                        text("supports"),
                        Value::Float(0.9),
                    ]
                })
                .collect(),
            InsertConflict::Fail,
        )
        .unwrap();
    let request = GraphNeighborhoodRequest {
        collection: "benchmark".into(),
        chunk_id: "0".into(),
        max_hops: 3,
        neighbor_limit: 8,
        max_nodes: 20,
        max_edges: 20,
        direction: GraphNeighborhoodDirection::Both,
        kind: Some("supports".into()),
        min_weight: 0.5,
    };
    let expected = database.graph_neighborhood(request.clone()).unwrap();
    assert_eq!(
        expected
            .nodes
            .iter()
            .map(|node| node.node.chunk_id.as_str())
            .collect::<Vec<_>>(),
        ["0", "1", "2", "3"]
    );
    assert_eq!(expected.edges.len(), 3);
    assert!(!expected.truncated);
    let mut timings = Vec::with_capacity(repetitions);
    for _ in 0..repetitions {
        let started = Instant::now();
        let result = database.graph_neighborhood(request.clone()).unwrap();
        timings.push(started.elapsed().as_secs_f64() * 1_000_000.0);
        assert_eq!(result, expected);
    }
    timings.sort_by(f64::total_cmp);
    if let Some(path) = arguments.get(2) {
        std::fs::write(path, serde_json::to_vec_pretty(&expected).unwrap()).unwrap();
    }
    println!(
        "{}",
        serde_json::json!({
            "documents": count, "chunks": count, "dimensions": 3, "stored_edges": 3,
            "returned_nodes": expected.nodes.len(), "returned_edges": expected.edges.len(),
            "max_hops": 3, "max_nodes": 20, "max_edges": 20, "neighbor_limit": 8,
            "direction": "both", "kind": "supports", "min_weight": 0.5, "vector_search": false,
            "repetitions": repetitions, "median_us": timings[repetitions / 2],
            "p95_us": timings[((repetitions - 1) as f64 * 0.95).ceil() as usize],
            "correctness_checked": true,
        })
    );
}
