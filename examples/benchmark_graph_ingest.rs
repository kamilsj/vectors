//! Provider-free new-document ingestion benchmark with unrelated stored payload.
//! Run: cargo run --release --example benchmark_graph_ingest -- 10000 128 20 32 2 /tmp/graph.json
//! Arguments: final chunks, dimensions, timed documents, unrelated MiB, semantic
//! neighbors, optional canonical graph output. Each timed document adds five
//! chunks. Setup, warm-up, result serialization and HTTP are excluded; this uses
//! an in-memory CPU database, so timings do not include disk synchronization.

use serde_json::{json, Value as JsonValue};
use std::time::Instant;
use vectors::{
    ComputeConfig, ComputeDevice, Database, ExecutionResult, GraphChunkInput,
    GraphCollectionConfig, GraphDocumentInput, GraphEmbeddingProfile, GraphIngestRequest,
    InsertConflict, Value, Vector,
};

fn document(id: &str, start: usize, count: usize, dimensions: usize) -> GraphDocumentInput {
    let mut text = String::new();
    let chunks = (start..start + count)
        .map(|seed| {
            let content = format!(
                "Passage {seed}: journal recovery preserves committed transactions. \
                 Check durable records before restarting the storage service.\n"
            );
            let start_byte = text.len();
            text.push_str(&content);
            GraphChunkInput {
                start_byte,
                end_byte: text.len(),
                text: content.clone(),
                embedding_text: content,
                embedding: Vector::new(
                    (0..dimensions)
                        .map(|index| (((seed * 31 + index * 17) % 101) as f32 - 50.0) / 50.0)
                        .collect(),
                )
                .unwrap()
                .normalized()
                .unwrap(),
            }
        })
        .collect();
    GraphDocumentInput {
        id: id.into(),
        title: format!("Manual {id}"),
        source: format!("manual/{id}.md"),
        text,
        metadata: json!({"tenant":"benchmark"}),
        chunking: json!({"fixture":true}),
        chunks,
    }
}

fn ingest(
    db: &Database,
    profile: &GraphEmbeddingProfile,
    document: GraphDocumentInput,
) -> vectors::GraphIngestResult {
    db.graph_ingest_document(GraphIngestRequest {
        collection: "bench".into(),
        expected_revision: db.revision().unwrap(),
        expected_profile: profile.clone(),
        document,
    })
    .unwrap()
}

fn rows(db: &Database, table: &str, key: &str) -> JsonValue {
    let query = db
        .execute(&format!("SELECT * FROM {table} ORDER BY {key}"))
        .unwrap();
    let ExecutionResult::Query(query) = &query[0] else {
        panic!("expected rows")
    };
    JsonValue::Array(
        query
            .rows
            .iter()
            .map(|row| {
                JsonValue::Array(
                    row.iter()
                        .map(|value| match value {
                            Value::Null => JsonValue::Null,
                            Value::Integer(value) => json!(value),
                            Value::Float(value) => json!(value),
                            Value::Text(value) => json!(value),
                            Value::Boolean(value) => json!(value),
                            Value::Vector(value) => json!(value.as_slice()),
                        })
                        .collect(),
                )
            })
            .collect(),
    )
}

fn percentile(samples: &[f64], fraction: f64) -> f64 {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[((sorted.len() - 1) as f64 * fraction).ceil() as usize]
}

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    assert!(args.len() <= 6, "expected final_chunks dimensions repetitions unrelated_mib semantic_neighbors [canonical_path]");
    let argument = |index: usize, default: usize| {
        args.get(index)
            .map_or(default, |value| value.parse::<usize>().unwrap())
    };
    let chunks = argument(0, 1000);
    let dimensions = argument(1, 128);
    let repetitions = argument(2, 20);
    let unrelated_mib = argument(3, 32);
    let semantic_neighbors = argument(4, 2);
    let added_chunks = 5;
    assert!(repetitions > 0 && chunks > (repetitions + 1) * added_chunks && chunks <= 10_000);
    assert!((1..=3072).contains(&dimensions) && unrelated_mib <= 256 && semantic_neighbors <= 16);
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
    let initial_chunks = chunks - (repetitions + 1) * added_chunks;
    for start in (0..initial_chunks).step_by(100) {
        ingest(
            &db,
            &profile,
            document(
                &format!("seed-{start}"),
                start,
                100.min(initial_chunks - start),
                dimensions,
            ),
        );
    }
    db.execute(&format!(
        "UPDATE graph_bench_config SET semantic_neighbors = {semantic_neighbors}"
    ))
    .unwrap();
    db.execute("CREATE TABLE unrelated (id INTEGER UNIQUE, payload TEXT)")
        .unwrap();
    if unrelated_mib > 0 {
        db.insert_rows(
            "unrelated",
            (0..unrelated_mib * 32)
                .map(|index| {
                    vec![
                        Value::Integer(index as i64),
                        Value::Text("x".repeat(32 * 1024)),
                    ]
                })
                .collect(),
            InsertConflict::Fail,
        )
        .unwrap();
    }
    ingest(
        &db,
        &profile,
        document("warmup", initial_chunks, added_chunks, dimensions),
    );
    let mut samples = Vec::new();
    let mut results = Vec::new();
    for repetition in 0..repetitions {
        let document = document(
            &format!("new-{repetition}"),
            initial_chunks + (repetition + 1) * added_chunks,
            added_chunks,
            dimensions,
        );
        let start = Instant::now();
        let result = ingest(&db, &profile, document);
        samples.push(start.elapsed().as_secs_f64() * 1_000_000.0);
        results.push(result);
    }
    let info = db.graph_collection("bench").unwrap();
    assert_eq!(info.chunk_count, chunks);
    let canonical = json!({
        "collection":info,
        "results":results,
        "documents":rows(&db,"graph_bench_documents","document_id"),
        "chunks":rows(&db,"graph_bench_chunks","chunk_id"),
        "edges":rows(&db,"graph_bench_edges","edge_id"),
    });
    let canonical = serde_json::to_vec(&canonical).unwrap();
    if let Some(path) = args.get(5) {
        std::fs::write(path, &canonical).unwrap();
    }
    let hash = canonical.iter().fold(0xcbf29ce484222325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    });
    println!(
        "{}",
        json!({
            "initial_chunks": chunks - repetitions * added_chunks,
            "final_chunks": chunks,
            "dimensions":dimensions,
            "documents": repetitions,
            "chunks_per_document": added_chunks,
            "unrelated_mib":unrelated_mib,
            "semantic_neighbors":semantic_neighbors,
            "median_us":percentile(&samples,0.5),
            "p95_us":percentile(&samples,0.95),
            "total_us":samples.iter().sum::<f64>(),
            "samples_us":samples,
            "canonical_fnv64":format!("{hash:016x}"),
            "canonical_bytes":canonical.len(),
        })
    );
}
