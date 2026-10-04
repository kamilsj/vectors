//! Warm retrieval benchmark for maintained chunk-ID lookups.
//! Run: cargo run --release --example benchmark_rag_lookups -- 1000 128 50
//! Optional fourth argument writes complete canonical results for version parity.
//! Corpus setup, warm-up, result serialization, providers, and HTTP are not timed.

use serde_json::{json, Value};
use std::time::Instant;
use vectors::{
    ComputeConfig, ComputeDevice, Database, GraphChunkInput, GraphCollectionConfig,
    GraphDocumentInput, GraphEmbeddingProfile, GraphIngestRequest, GraphRagOptions,
    GraphRagRequest, GraphRagSelection, Vector, VectorFilterOperator, VectorSearchFilter,
};

fn vector(seed: usize, dimensions: usize) -> Vector {
    Vector::new(
        (0..dimensions)
            .map(|i| (((seed * 31 + i * 17) % 101) as f32 - 50.0) / 50.0)
            .collect(),
    )
    .unwrap()
    .normalized()
    .unwrap()
}

fn percentile(samples: &[f64], percentile: f64) -> f64 {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[((sorted.len() - 1) as f64 * percentile).ceil() as usize]
}

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    assert!(
        args.len() <= 4,
        "expected chunks dimensions repetitions [canonical-path]"
    );
    let argument = |index: usize, default: usize| {
        args.get(index)
            .map_or(default, |value| value.parse().expect("positive integer"))
    };
    let chunks = argument(0, 1000);
    let dimensions = argument(1, 128);
    let repetitions = argument(2, 50);
    assert!((10..=10_000).contains(&chunks) && chunks % 10 == 0);
    assert!((1..=3072).contains(&dimensions) && repetitions > 0);
    let documents = chunks / 10;
    let db = Database::new_with_compute(ComputeConfig {
        device: ComputeDevice::Cpu,
        ..ComputeConfig::default()
    });
    let profile = GraphEmbeddingProfile {
        provider: "openai".into(),
        model: "text-embedding-3-large".into(),
        dimensions,
        context_format_version: 1,
    };
    db.graph_create_collection(GraphCollectionConfig {
        name: "bench".into(),
        profile: profile.clone(),
        semantic_neighbors: 0,
        semantic_threshold: 0.8,
    })
    .unwrap();
    for doc in 0..documents {
        let mut text = String::new();
        let mut parts = Vec::new();
        for chunk in 0..10 {
            let content = format!(
                "Document {doc}, passage {chunk}: incident ZX{} requires journal rotation. \
                 Verify committed transactions and recover the durable snapshot before restarting.\n",
                (doc + chunk) % 17
            );
            let start_byte = text.len();
            text.push_str(&content);
            parts.push(GraphChunkInput {
                start_byte,
                end_byte: text.len(),
                text: content.clone(),
                embedding_text: content,
                embedding: vector(doc * 10 + chunk, dimensions),
            });
        }
        db.graph_ingest_document(GraphIngestRequest {
            collection: "bench".into(),
            expected_revision: db.revision().unwrap(),
            expected_profile: profile.clone(),
            document: GraphDocumentInput {
                id: format!("document-{doc}"),
                title: format!("Incident manual {doc}"),
                source: format!("manual/{doc}.md"),
                text,
                metadata: json!({}),
                chunking: json!({"fixture":true}),
                chunks: parts,
            },
        })
        .unwrap();
    }
    let mut report = serde_json::Map::new();
    let mut canonical = serde_json::Map::new();
    for (name, vector_weight, max_hops, filtered) in [
        ("keyword", 0.0, 0, false),
        ("filtered_keyword_graph", 0.0, 1, true),
        ("hybrid_graph", 1.0, 1, false),
    ] {
        let mut samples = Vec::new();
        let mut results = Vec::new();
        for iteration in 0..repetitions + 5 {
            let query_index = iteration.saturating_sub(5);
            let request = GraphRagRequest {
                collection: "bench".into(),
                expected_profile: profile.clone(),
                query: if vector_weight == 0.0 {
                    Vector::new(vec![0.0; dimensions]).unwrap()
                } else {
                    vector(query_index % chunks, dimensions)
                },
                query_text: format!("ZX{} journal recovery", query_index % 17),
                candidate_limit: 40,
                seed_limit: 12,
                max_hops,
                neighbor_limit: 8,
                vector_weight,
                lexical_weight: 1.0,
            };
            let options = GraphRagOptions {
                document_filters: if filtered {
                    vec![VectorSearchFilter {
                        column: "document_id".into(),
                        operator: VectorFilterOperator::Eq,
                        value: vectors::Value::Text(format!(
                            "document-{}",
                            query_index * 17 % documents
                        )),
                    }]
                } else {
                    Vec::new()
                },
                ..GraphRagOptions::default()
            };
            let selection = GraphRagSelection {
                limit: 10,
                diversity: 0.3,
                max_context_bytes: 24_000,
                max_per_document: 3,
            };
            let start = Instant::now();
            let result = db
                .graph_rag_candidates_with_options(request, options)
                .unwrap()
                .finalize(selection, None)
                .unwrap();
            let elapsed = start.elapsed().as_secs_f64() * 1_000_000.0;
            if iteration < 5 {
                continue;
            }
            assert!(result.lexical_cache_hit);
            samples.push(elapsed);
            let mut value = serde_json::to_value(&result).unwrap();
            value.as_object_mut().unwrap().remove("lexical_cache_hit");
            results.push(value);
        }
        report.insert(
            name.into(),
            json!({
                "median_us":percentile(&samples, 0.5),
                "p95_us":percentile(&samples, 0.95), "samples_us":samples,
            }),
        );
        canonical.insert(name.into(), Value::Array(results));
    }
    if let Some(path) = args.get(3) {
        std::fs::write(path, serde_json::to_vec_pretty(&canonical).unwrap()).unwrap();
    }
    println!("{}", serde_json::to_string_pretty(&json!({
        "workload":{"documents":documents,"chunks":chunks,"dimensions":dimensions,
            "repetitions":repetitions,"candidates":40,"seeds":12,"neighbors":8,
            "results":10,"diversity":0.3,"compute":"cpu","profile":"release",
            "semantic_neighbors":0,"warmup_queries_per_workload":5},
        "timings":report,"provider_calls":0,
        "scope":"Warm in-process retrieval and MMR on deterministic synthetic data with adjacent graph links; excludes corpus setup, warm-up, providers, HTTP, serialization and concurrent load."
    })).unwrap());
}
