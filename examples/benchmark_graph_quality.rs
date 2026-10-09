//! Controlled graph-neighbor admission probes, not a real-corpus accuracy claim.
//! cargo run --release --example benchmark_graph_quality -- 20
#[path = "../tests/fixtures/graph_rag_weight_cases.rs"]
mod cases;

use serde_json::json;
use std::time::Instant;
use vectors::GraphNeighborhoodDirection;

fn percentile(samples: &mut [f64], fraction: f64) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[((samples.len() - 1) as f64 * fraction).ceil() as usize]
}

fn main() {
    let repetitions = std::env::args().nth(1).map_or(20, |value| {
        value.parse::<usize>().expect("integer repetitions")
    });
    assert!((1..=1000).contains(&repetitions));
    let mut probes = Vec::new();
    let mut passed = 0;
    let mut timings = Vec::new();
    for direction in [
        GraphNeighborhoodDirection::Outgoing,
        GraphNeighborhoodDirection::Incoming,
        GraphNeighborhoodDirection::Both,
    ] {
        for hops in 1..=3 {
            for fit in [0.4, 0.95] {
                let db = cases::fixture(
                    fit,
                    hops,
                    matches!(direction, GraphNeighborhoodDirection::Incoming),
                );
                for (name, vector, lexical) in cases::WEIGHTS {
                    let request = cases::request(vector, lexical, hops);
                    let policy = cases::policy(direction);
                    let snapshot = db
                        .graph_rag_candidates_with_traversal(request.clone(), policy.clone())
                        .unwrap();
                    let expected = cases::expected(name, fit);
                    let retained = snapshot
                        .candidates
                        .iter()
                        .any(|candidate| candidate.hit.document_id == expected);
                    passed += usize::from(retained);
                    for _ in 0..repetitions {
                        let start = Instant::now();
                        let warm = db
                            .graph_rag_candidates_with_traversal(request.clone(), policy.clone())
                            .unwrap();
                        timings.push(start.elapsed().as_secs_f64() * 1_000_000.0);
                        assert_eq!(
                            snapshot
                                .candidates
                                .iter()
                                .map(|c| &c.hit)
                                .collect::<Vec<_>>(),
                            warm.candidates.iter().map(|c| &c.hit).collect::<Vec<_>>()
                        );
                    }
                    probes.push(json!({
                        "policy":name,"vector_weight":vector,"lexical_weight":lexical,
                        "semantic_fit":fit,"hops":hops,"direction":direction,
                        "candidate_limit":request.candidate_limit,"neighbor_limit":request.neighbor_limit,
                        "expected_document":expected,"retained":retained,
                        "graph_documents":snapshot.candidates.iter().filter(|c|c.hit.depth>0)
                            .map(|c|c.hit.document_id.as_str()).collect::<Vec<_>>()
                    }));
                }
            }
        }
    }
    println!("{}", serde_json::to_string_pretty(&json!({
        "suite":"competing graph paths with configured retrieval weights",
        "cases":probes.len(),"passed":passed,"repetitions":repetitions,
        "warm_search_us":{"median":percentile(&mut timings,0.5),"p95":percentile(&mut timings,0.95)},
        "provider_calls":0,"synthetic_embeddings":true,
        "scope":"Controlled candidate-admission regression probes. Excludes embeddings, external reranking and generation; does not estimate production answer accuracy or large-corpus throughput.",
        "probes":probes
    })).unwrap());
}
