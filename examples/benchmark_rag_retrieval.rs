//! Repeatable cold/warm lexical-cache comparison with identical RAG results.
//! Run: cargo run --release --example benchmark_rag_retrieval -- 64 16 128 20
use serde_json::json;
use std::time::Instant;
use vectors::{
    ComputeConfig, ComputeDevice, Database, GraphChunkInput, GraphCollectionConfig,
    GraphDocumentInput, GraphEmbeddingProfile, GraphIngestRequest, GraphRagRequest,
    GraphRagSelection, Vector,
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
    let args = std::env::args()
        .skip(1)
        .map(|arg| arg.parse::<usize>().expect("positive integer argument"))
        .collect::<Vec<_>>();
    let docs = args.first().copied().unwrap_or(64);
    let per_doc = args.get(1).copied().unwrap_or(16);
    let dimensions = args.get(2).copied().unwrap_or(128);
    let repetitions = args.get(3).copied().unwrap_or(20);
    assert!(docs > 0 && (1..=256).contains(&per_doc) && docs * per_doc <= 10_000);
    assert!((1..=3072).contains(&dimensions) && repetitions > 0);
    let database = Database::new_with_compute(ComputeConfig {
        device: ComputeDevice::Cpu,
        ..ComputeConfig::default()
    });
    let profile = GraphEmbeddingProfile {
        provider: "openai".into(),
        model: "text-embedding-3-large".into(),
        dimensions,
        context_format_version: 1,
    };
    database
        .graph_create_collection(GraphCollectionConfig {
            name: "bench".into(),
            profile: profile.clone(),
            semantic_neighbors: 2,
            semantic_threshold: 0.8,
        })
        .unwrap();
    database.execute("CREATE TABLE benchmark_revision_marker (id INTEGER PRIMARY KEY, value INTEGER); INSERT INTO benchmark_revision_marker VALUES (1, 0)").unwrap();
    for doc in 0..docs {
        let mut text = String::new();
        let mut chunks = Vec::new();
        for chunk in 0..per_doc {
            let content = format!("Document {doc} section {chunk}. Incident ZX{} requires journal rotation before recovery. {}\n", (doc + chunk) % 17,
                "The storage service validates checksums and replays committed transactions during recovery. Operators inspect the journal, retain source citations, and restart the writer after available disk space has been confirmed. ".repeat(4));
            let start_byte = text.len();
            text.push_str(&content);
            chunks.push(GraphChunkInput {
                start_byte,
                end_byte: text.len(),
                text: content.clone(),
                embedding_text: content,
                embedding: vector(doc * per_doc + chunk, dimensions),
            });
        }
        database
            .graph_ingest_document(GraphIngestRequest {
                collection: "bench".into(),
                expected_revision: database.revision().unwrap(),
                expected_profile: profile.clone(),
                document: GraphDocumentInput {
                    id: format!("document-{doc}"),
                    title: format!("Storage incident {doc}"),
                    source: format!("manual/{doc}.md"),
                    text,
                    metadata: json!({"group":doc%8}),
                    chunking: json!({"fixture":true}),
                    chunks,
                },
            })
            .unwrap();
    }
    let mut cold_us = Vec::new();
    let mut warm_us = Vec::new();
    for iteration in 0..repetitions {
        // Rebuild chunk storage without altering text, invalidating its lexical index.
        // The write and its catalog-copy cost are deliberately outside timing.
        database
            .execute("UPDATE graph_bench_chunks SET embedding_text = embedding_text WHERE document_id = 'document-0'")
            .unwrap();
        let request = GraphRagRequest {
            collection: "bench".into(),
            expected_profile: profile.clone(),
            query: vector(iteration % (docs * per_doc), dimensions),
            query_text: format!("ZX{} journal recovery", iteration % 17),
            candidate_limit: 40,
            seed_limit: 12,
            max_hops: 1,
            neighbor_limit: 8,
            vector_weight: 1.0,
            lexical_weight: 1.0,
        };
        let selection = GraphRagSelection {
            limit: 10,
            diversity: 0.3,
            max_context_bytes: 24_000,
            max_per_document: 3,
        };
        let start = Instant::now();
        let cold = database
            .graph_rag_candidates(request.clone())
            .unwrap()
            .finalize(selection.clone(), None)
            .unwrap();
        cold_us.push(start.elapsed().as_secs_f64() * 1_000_000.0);
        let start = Instant::now();
        let warm = database
            .graph_rag_candidates(request)
            .unwrap()
            .finalize(selection, None)
            .unwrap();
        warm_us.push(start.elapsed().as_secs_f64() * 1_000_000.0);
        assert!(!cold.lexical_cache_hit && warm.lexical_cache_hit);
        assert_eq!(cold.hits, warm.hits);
        assert_eq!(cold.edges, warm.edges);
        assert_eq!(cold.context_bytes, warm.context_bytes);
    }
    println!("{}", serde_json::to_string_pretty(&json!({
        "workload":{"documents":docs,"chunks_per_document":per_doc,"chunks":docs*per_doc,"dimensions":dimensions,"repetitions":repetitions,"candidates":40,"seeds":12,"results":10,"hops":1,"neighbors":8,"diversity":0.3,"compute":"cpu","profile":"release"},
        "cold_us":{"median":percentile(&cold_us,0.5),"p95":percentile(&cold_us,0.95)},
        "warm_us":{"median":percentile(&warm_us,0.5),"p95":percentile(&warm_us,0.95)},
        "median_speedup":percentile(&cold_us,0.5)/percentile(&warm_us,0.5),
        "identical_results":true,"provider_calls":0,
        "scope":"Local RAG retrieval plus context selection; excludes provider latency, ingestion, and invalidation writes."
    })).unwrap());
}
