//! Durable PDF-like page ingestion and retrieval at realistic vector dimensions.
//! No provider calls or PDF extraction: measures the local storage/retrieval stage.
//! cargo run --release --example benchmark_pdf_rag -- 1000 1536
use serde_json::json;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use vectors::{
    Database, GraphChunkInput, GraphCollectionConfig, GraphDocumentInput, GraphEmbeddingProfile,
    GraphIngestRequest, GraphRagOptions, GraphRagRequest, GraphRagSelection, Vector,
};

fn vector(seed: usize, dimensions: usize) -> Vector {
    let mut state = seed as u64 + 1;
    Vector::new(
        (0..dimensions)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state % 2001) as f32 / 1000.0 - 1.0
            })
            .collect(),
    )
    .unwrap()
    .normalized()
    .unwrap()
}

fn percentile(samples: &[f64], quantile: f64) -> f64 {
    let mut samples = samples.to_vec();
    samples.sort_by(f64::total_cmp);
    samples[(samples.len() as f64 * quantile).ceil() as usize - 1]
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let pages: usize = args.first().map_or(Ok(1000), |s| s.parse())?;
    let dimensions: usize = args.get(1).map_or(Ok(1536), |s| s.parse())?;
    assert!((1..=10_000).contains(&pages) && (1..=3072).contains(&dimensions));
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let directory =
        std::env::temp_dir().join(format!("vectors-pdf-rag-{}-{unique}", std::process::id()));
    let database = Database::open_persistent(&directory)?;
    let profile = GraphEmbeddingProfile {
        provider: "openai".into(),
        model: "text-embedding-3-small".into(),
        dimensions,
        context_format_version: 1,
    };
    database.graph_create_collection(GraphCollectionConfig {
        name: "pdfs".into(),
        profile: profile.clone(),
        semantic_neighbors: 0,
        semantic_threshold: 0.8,
    })?;
    let mut ingest_ms = Vec::new();
    let mut query_ms = Vec::new();
    let started = Instant::now();
    for page in 0..pages {
        let text = format!("Project ATLAS{page:05} uses recovery code ORBIT{:05}. {}",
            (page * 7919) % 99991,
            "Verify the recovery code, inspect the journal, and restore the latest snapshot before replaying committed entries. ".repeat(5));
        let document = GraphDocumentInput {
            id: format!("pdf-{page:05}"),
            title: format!("ATLAS{page:05} manual page 1"),
            source: format!("manual-{page:05}.pdf#page=1"),
            text: text.clone(),
            metadata: json!({"page":1}),
            chunking: json!({"benchmark":true}),
            chunks: vec![GraphChunkInput {
                start_byte: 0,
                end_byte: text.len(),
                text: text.clone(),
                embedding_text: text,
                embedding: vector(page, dimensions),
            }],
        };
        let now = Instant::now();
        database.graph_ingest_document(GraphIngestRequest {
            collection: "pdfs".into(),
            expected_revision: database.revision()?,
            expected_profile: profile.clone(),
            document,
        })?;
        ingest_ms.push(now.elapsed().as_secs_f64() * 1000.0);
    }
    let total_ingest_ms = started.elapsed().as_secs_f64() * 1000.0;
    let wal_bytes = std::fs::metadata(directory.join("vectors.wal"))?.len();
    let snapshot_bytes =
        std::fs::metadata(directory.join("vectors.vdb")).map_or(0, |metadata| metadata.len());
    drop(database);
    let now = Instant::now();
    let database = Database::open_persistent(&directory)?;
    let reopen_ms = now.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(database.graph_collection("pdfs")?.chunk_count, pages);
    for run in 0..20 {
        let page = run * (pages - 1) / 19;
        let now = Instant::now();
        let result = database
            .graph_rag_candidates_with_options(
                GraphRagRequest {
                    collection: "pdfs".into(),
                    expected_profile: profile.clone(),
                    query: vector(page, dimensions),
                    query_text: format!("ATLAS{page:05} recovery code"),
                    candidate_limit: 40,
                    seed_limit: 12,
                    max_hops: 1,
                    neighbor_limit: 8,
                    vector_weight: 1.0,
                    lexical_weight: 1.0,
                },
                GraphRagOptions {
                    max_seeds_per_document: Some(2),
                    ..Default::default()
                },
            )?
            .finalize(
                GraphRagSelection {
                    limit: 10,
                    diversity: 0.3,
                    max_context_bytes: 24_000,
                    max_per_document: 3,
                },
                None,
            )?;
        query_ms.push(now.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(result.hits[0].hit.document_id, format!("pdf-{page:05}"));
        assert!(result.hits[0]
            .hit
            .text
            .contains(&format!("ORBIT{:05}", (page * 7919) % 99991)));
    }
    drop(database);
    std::fs::remove_dir_all(&directory)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "workload":{"pages":pages,"chunks":pages,"dimensions":dimensions,"semantic_neighbors":0,"durable":true,"queries":20},
            "ingestion":{"total_ms":total_ingest_ms,"pages_per_second":pages as f64 / total_ingest_ms * 1000.0,"p50_ms":percentile(&ingest_ms,0.5),"p95_ms":percentile(&ingest_ms,0.95)},
            "retrieval_ms":{"p50":percentile(&query_ms,0.5),"p95":percentile(&query_ms,0.95)},
            "reopen_ms":reopen_ms,"wal_bytes":wal_bytes,"snapshot_bytes":snapshot_bytes,"expected_first_hit_checks":20,"provider_calls":0,
            "scope":"Synthetic one-chunk pages, deterministic normalized vectors; sequential durable ingestion and exact hybrid retrieval after reopen (one cold, 19 warm queries). Excludes PDF extraction, providers, network, semantic linking, and concurrent query contention."
        }))?
    );
    Ok(())
}
