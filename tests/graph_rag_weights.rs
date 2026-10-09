#[path = "fixtures/graph_rag_weight_cases.rs"]
mod cases;

use vectors::{
    GraphNeighborhoodDirection, GraphRagSelection, Value, VectorFilterOperator, VectorSearchFilter,
};

#[test]
fn graph_expansion_respects_weighted_query_intent_at_each_hop() {
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
                    let snapshot = db
                        .graph_rag_candidates_with_traversal(
                            cases::request(vector, lexical, hops),
                            cases::policy(direction),
                        )
                        .unwrap();
                    let expected = cases::expected(name, fit);
                    let evidence = snapshot
                        .candidates
                        .iter()
                        .find(|candidate| candidate.hit.document_id == expected)
                        .unwrap_or_else(|| {
                            panic!(
                                "missing {expected}: {name}, fit={fit}, hops={hops}, {direction:?}"
                            )
                        });
                    assert_eq!(evidence.hit.depth, hops);
                    assert_eq!(evidence.retrieval_path.as_ref().unwrap().edges.len(), hops);
                    assert_eq!(snapshot.candidates.len(), cases::candidate_limit(hops));
                    assert_eq!(snapshot.traversal_seed_ids, [cases::chunk("a")]);
                    // Final local selection must preserve the retrieved evidence
                    // when its result and byte budgets allow the full candidate set.
                    let result = snapshot
                        .finalize(
                            GraphRagSelection {
                                limit: cases::candidate_limit(hops),
                                diversity: 0.0,
                                max_context_bytes: 1024,
                                max_per_document: 1,
                            },
                            None,
                        )
                        .unwrap();
                    assert!(result
                        .hits
                        .iter()
                        .any(|hit| hit.hit.document_id == expected));
                }
            }
        }
    }
}

#[test]
fn common_weight_rescaling_preserves_paths_and_candidate_order() {
    let db = cases::fixture(0.95, 2, false);
    for (_, vector, lexical) in cases::WEIGHTS {
        let retrieve = |scale| {
            db.graph_rag_candidates(cases::request(vector * scale, lexical * scale, 2))
                .unwrap()
                .candidates
                .into_iter()
                .map(|candidate| (candidate.hit, candidate.retrieval_path))
                .collect::<Vec<_>>()
        };
        assert_eq!(retrieve(1.0), retrieve(0.25));
    }
}

#[test]
fn weighted_expansion_never_traverses_an_excluded_bridge() {
    let db = cases::fixture(0.95, 2, false);
    for (_, vector, lexical) in cases::WEIGHTS {
        let snapshot = db
            .graph_rag_candidates_filtered(
                cases::request(vector, lexical, 2),
                cases::policy(GraphNeighborhoodDirection::Outgoing),
                vec![VectorSearchFilter {
                    column: "document_id".into(),
                    operator: VectorFilterOperator::Ne,
                    value: Value::Text("bridge1".into()),
                }],
            )
            .unwrap();
        assert_eq!(snapshot.candidates.len(), 4);
        assert!(snapshot
            .candidates
            .iter()
            .all(|candidate| candidate.hit.depth == 0 && candidate.retrieval_path.is_none()));
    }
}

#[test]
fn zero_fit_bridges_do_not_displace_a_relevant_third_hop_with_one_context_slot() {
    let db = cases::fixture(0.4, 3, false);
    let mut request = cases::request(1.0, 0.0, 3);
    request.candidate_limit = 4;
    let snapshot = db.graph_rag_candidates(request).unwrap();
    let context = snapshot
        .candidates
        .iter()
        .filter(|candidate| candidate.hit.depth > 0)
        .collect::<Vec<_>>();
    assert_eq!(context.len(), 1);
    assert_eq!(context[0].hit.document_id, "semantic");
    assert_eq!(context[0].hit.depth, 3);
    let path = context[0].retrieval_path.as_ref().unwrap();
    assert_eq!(path.edges.len(), 3);
    assert_eq!(path.edges[0].to_chunk, cases::chunk("bridge1"));
    assert_eq!(path.edges[1].to_chunk, cases::chunk("bridge2"));
}
