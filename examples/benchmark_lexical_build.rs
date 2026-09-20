//! Measure keyword-index construction within complete local RAG retrieval.
//! Run: cargo run --release --example benchmark_lexical_build -- 64 16 128 20
//! Optionally append a path for canonical results to compare across revisions.
//! Writes, fixture setup, provider calls and result serialization are not timed.
use serde_json::{json, Value};
use std::time::Instant;
use vectors::{
    ComputeConfig, ComputeDevice, Database, GraphChunkInput, GraphCollectionConfig,
    GraphDocumentInput, GraphEmbeddingProfile, GraphIngestRequest, GraphRagRequest, GraphRagResult,
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

#[derive(Default)]
struct Phase {
    elapsed_us: Vec<f64>,
    cache_hits: usize,
    fingerprints: Vec<String>,
}
impl Phase {
    fn record(&mut self, elapsed_us: f64, result: &GraphRagResult, canonical: &Value) {
        self.elapsed_us.push(elapsed_us);
        self.cache_hits += usize::from(result.lexical_cache_hit);
        // Stable noncryptographic fingerprint; the optional canonical artifact
        // supports a complete byte comparison rather than relying on this hash.
        let fingerprint = serde_json::to_vec(canonical)
            .unwrap()
            .iter()
            .fold(0xcbf29ce484222325_u64, |hash, byte| {
                (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
            });
        self.fingerprints.push(format!("{fingerprint:016x}"));
    }
    fn report(&self) -> Value {
        json!({
            "median_us": percentile(&self.elapsed_us, 0.5),
            "p95_us": percentile(&self.elapsed_us, 0.95),
            "samples_us": self.elapsed_us,
            "cache_hits": self.cache_hits,
            "cache_misses": self.elapsed_us.len() - self.cache_hits,
            "result_fingerprints_fnv1a64": self.fingerprints,
        })
    }
}

fn canonical(result: &GraphRagResult) -> Value {
    // Exclude revision and cache-hit telemetry, which legitimately change
    // between phases; retain every hit, score, citation and returned edge.
    json!({
        "collection": result.collection, "hits": result.hits, "edges": result.edges,
        "truncated": result.truncated, "context_bytes": result.context_bytes,
        "candidate_count": result.candidate_count,
    })
}

fn retrieve(database: &Database, request: &GraphRagRequest) -> (GraphRagResult, f64) {
    let request = request.clone();
    let selection = GraphRagSelection {
        limit: 10,
        diversity: 0.3,
        max_context_bytes: 24_000,
        max_per_document: 3,
    };
    let started = Instant::now();
    let result = database
        .graph_rag_candidates(request)
        .unwrap()
        .finalize(selection, None)
        .unwrap();
    (result, started.elapsed().as_secs_f64() * 1_000_000.0)
}

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    assert!(
        args.len() <= 5,
        "expected documents chunks dimensions repetitions [canonical-results-path]"
    );
    let argument = |index: usize, default: usize| {
        args.get(index).map_or(default, |value| {
            value.parse().expect("positive integer argument")
        })
    };
    let docs = argument(0, 64);
    let per_doc = argument(1, 16);
    let dimensions = argument(2, 128);
    let repetitions = argument(3, 20);
    assert!(
        docs > 0
            && (1..=256).contains(&per_doc)
            && (2..=10_000).contains(&docs.saturating_mul(per_doc))
    );
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
    let request = |iteration| GraphRagRequest {
        collection: "bench".into(),
        expected_profile: profile.clone(),
        query: vector(iteration % (docs * per_doc), dimensions),
        query_text: format!("ZX{} journal recovery", iteration % 17),
        candidate_limit: 40,
        seed_limit: 12,
        max_hops: 0,
        neighbor_limit: 8,
        vector_weight: 1.0,
        lexical_weight: 1.0,
    };
    // Prime execution/runtime caches once; each measured cold phase then forces
    // a chunk-storage generation change and rebuilds the lexical index.
    retrieve(&database, &request(0));
    let mut phases = [Phase::default(), Phase::default()];
    let mut results = Vec::with_capacity(repetitions);
    for iteration in 0..repetitions {
        let query = request(iteration);
        database.execute("UPDATE graph_bench_chunks SET embedding_text = embedding_text WHERE document_id = 'document-0'").unwrap();
        let (cold, elapsed) = retrieve(&database, &query);
        assert!(
            !cold.lexical_cache_hit,
            "the chunk write must invalidate the lexical index"
        );
        let expected = canonical(&cold);
        phases[0].record(elapsed, &cold, &expected);
        let (warm, elapsed) = retrieve(&database, &query);
        assert!(
            warm.lexical_cache_hit,
            "the repeated query must reuse its lexical index"
        );
        let actual = canonical(&warm);
        assert_eq!(expected, actual);
        phases[1].record(elapsed, &warm, &actual);
        results.push(expected);
    }
    if let Some(path) = args.get(4) {
        std::fs::write(path, serde_json::to_vec(&results).unwrap())
            .expect("write canonical result artifact");
    }
    let info = database.graph_collection("bench").unwrap();
    println!("{}", serde_json::to_string_pretty(&json!({
        "workload": {"documents": docs, "chunks_per_document": per_doc, "chunks": docs * per_doc, "edges": info.edge_count,
            "dimensions": dimensions, "repetitions": repetitions, "candidates": 40, "seeds": 12, "results": 10,
            "hops": 0, "neighbors": 8, "diversity": 0.3, "max_per_document": 3, "max_context_bytes": 24_000,
            "compute": "cpu", "profile": "release", "vector_weight": 1.0, "lexical_weight": 1.0},
        "phases": {"cold_after_chunk_update": phases[0].report(), "warm_repeat": phases[1].report()},
        "identical_results_across_phases": true, "provider_calls": 0,
        "percentile_method": "Sorted samples, index ceil((n - 1) * p), zero-based.",
        "scope": "Local hybrid RAG candidate retrieval and final MMR/context selection. Excludes all writes, fixture setup, provider/network time, HTTP handling and result serialization. Zero graph-expansion hops isolate keyword-index construction and unchanged ranking. The cold phase rebuilds the lexical index; the warm phase reuses it."
    })).unwrap());
}
