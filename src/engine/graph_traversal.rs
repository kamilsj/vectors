//! Query-aware, bounded beam expansion of RAG candidates.

use super::*;

// Source row, discovered depth, and fused retrieval evidence.
type AdmittedCandidates = Vec<(usize, usize, CandidateScore)>;

#[derive(Clone, Copy)]
struct Evidence {
    row: usize,
    depth: usize,
    score: CandidateScore,
    path: f64,
}

pub(super) struct GraphTraversal<'a> {
    pub request: &'a GraphRagRequest,
    pub chunks: &'a Table,
    pub edges: &'a Table,
    pub lookup: &'a HashMap<&'a str, usize>,
    pub scores: &'a HashMap<usize, CandidateScore>,
    pub lexical_scores: &'a [f64],
    pub similarities: Vec<Option<f64>>,
}

impl GraphTraversal<'_> {
    pub fn expand(mut self, ranked: &[usize]) -> Result<(AdmittedCandidates, bool)> {
        let request = self.request;
        let base_limit = if request.max_hops == 0 {
            request.candidate_limit
        } else {
            request.seed_limit.max(
                request
                    .candidate_limit
                    .saturating_sub((request.candidate_limit / 4).max(1)),
            )
        };
        let mut admitted = ranked
            .iter()
            .take(base_limit)
            .map(|&row| (row, 0, self.scores[&row]))
            .collect::<Vec<_>>();
        let direct = admitted
            .iter()
            .enumerate()
            .map(|(position, (row, _, _))| (*row, position))
            .collect::<HashMap<_, _>>();
        let mut frontier = admitted
            .iter()
            .take(request.seed_limit)
            .map(|&(row, depth, score)| Evidence {
                row,
                depth,
                score,
                path: score.fusion,
            })
            .collect::<Vec<_>>();
        let mut best_path = vec![0.0f64; self.chunks.rows.len()];
        for candidate in &frontier {
            best_path[candidate.row] = candidate.path;
        }
        let mut context: HashMap<usize, Evidence> = HashMap::new();
        let lexical_max = self.lexical_scores.iter().copied().fold(0.0, f64::max);
        let edge_index = self
            .edges
            .indexes
            .values()
            .find(|index| index.column == 1)
            .ok_or_else(|| invalid("graph edge source index is missing"))?;
        let mut truncated = false;

        for depth in 0..request.max_hops {
            let mut proposals: HashMap<usize, Evidence> = HashMap::new();
            for source in &frontier {
                let source_id = text_at(&self.chunks.rows[source.row], 0)?;
                let mut targets: HashMap<usize, f64> = HashMap::new();
                for index in edge_index
                    .buckets
                    .get(&UniqueKey::from(&Value::Text(source_id.into())))
                    .into_iter()
                    .flatten()
                {
                    let edge = read_edge(&self.edges.rows[*index])?;
                    let target = *self
                        .lookup
                        .get(edge.to_chunk.as_str())
                        .ok_or_else(|| invalid("graph edge references a missing chunk"))?;
                    // A zero-weight link conveys no retrieval evidence. It is
                    // still visible in the provider-free graph explorer.
                    if edge.weight == 0.0 {
                        continue;
                    }
                    targets
                        .entry(target)
                        .and_modify(|weight| {
                            *weight = weight.max(edge.weight);
                        })
                        .or_insert(edge.weight);
                }
                let mut neighbors = Vec::with_capacity(targets.len());
                for (row, weight) in targets {
                    let original = self.scores.get(&row).copied().unwrap_or(CandidateScore {
                        lexical: self.lexical_scores[row],
                        fusion: 0.0,
                    });
                    // Keep structural path strength separate from query fit:
                    // a weakly matching bridge may lead to a useful passage.
                    let path = (source.path * 0.5 * weight).max(original.fusion);
                    if path <= best_path[row] {
                        continue;
                    }
                    let vector_fit = if request.vector_weight > 0.0 {
                        if let Some(similarity) = self.similarities[row] {
                            similarity.max(0.0)
                        } else {
                            let Some(Value::Vector(vector)) = self.chunks.rows[row].get(8) else {
                                return Err(invalid("graph chunk embedding is missing"));
                            };
                            let similarity = (1.0
                                - f64::from(request.query.cosine_distance(vector)?))
                            .clamp(-1.0, 1.0);
                            self.similarities[row] = Some(similarity);
                            similarity.max(0.0)
                        }
                    } else {
                        0.0
                    };
                    let lexical_fit = if request.lexical_weight > 0.0 && lexical_max > 0.0 {
                        (self.lexical_scores[row] / lexical_max).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    // A soft floor preserves contextual bridges. The query
                    // signal ranks new context; stable chunk IDs break exact ties.
                    let query_fit = 0.2 + 0.8 * vector_fit.max(lexical_fit);
                    let score = CandidateScore {
                        lexical: original.lexical,
                        fusion: original.fusion.max(path * query_fit),
                    };
                    neighbors.push(Evidence {
                        row,
                        depth: depth + 1,
                        score,
                        path,
                    });
                }
                truncated |= neighbors.len() > request.neighbor_limit;
                top_evidence(&mut neighbors, request.neighbor_limit, self.chunks);
                for proposal in neighbors {
                    proposals
                        .entry(proposal.row)
                        .and_modify(|old| {
                            if proposal.path > old.path {
                                *old = proposal;
                            }
                        })
                        .or_insert(proposal);
                }
            }
            // Merge every seed's proposals BEFORE spending the beam budget.
            // An early seed cannot monopolize the available context slots.
            let mut next = proposals.into_values().collect::<Vec<_>>();
            truncated |= next.len() > request.candidate_limit;
            top_evidence(&mut next, request.candidate_limit, self.chunks);
            for proposal in &next {
                best_path[proposal.row] = proposal.path;
                if let Some(&position) = direct.get(&proposal.row) {
                    admitted[position].2.fusion =
                        admitted[position].2.fusion.max(proposal.score.fusion);
                } else {
                    context
                        .entry(proposal.row)
                        .and_modify(|old| {
                            old.depth = old.depth.min(proposal.depth);
                            old.path = old.path.max(proposal.path);
                            old.score.fusion = old.score.fusion.max(proposal.score.fusion);
                        })
                        .or_insert(*proposal);
                }
            }
            frontier = next;
            if frontier.is_empty() {
                break;
            }
        }

        let remaining = request.candidate_limit.saturating_sub(admitted.len());
        let mut context = context.into_values().collect::<Vec<_>>();
        truncated |= context.len() > remaining;
        top_evidence(&mut context, remaining, self.chunks);
        admitted.extend(
            context
                .into_iter()
                .map(|item| (item.row, item.depth, item.score)),
        );
        let mut selected = admitted
            .iter()
            .map(|(row, _, _)| *row)
            .collect::<HashSet<_>>();
        for &row in ranked {
            if selected.contains(&row) {
                continue;
            }
            if admitted.len() == request.candidate_limit {
                truncated = true;
                break;
            }
            selected.insert(row);
            admitted.push((row, 0, self.scores[&row]));
        }
        Ok((admitted, truncated))
    }
}

fn top_evidence(candidates: &mut Vec<Evidence>, limit: usize, chunks: &Table) {
    let compare = |left: &Evidence, right: &Evidence| {
        right
            .score
            .fusion
            .total_cmp(&left.score.fusion)
            .then_with(|| right.path.total_cmp(&left.path))
            .then_with(|| {
                compare_sort_values(&chunks.rows[left.row][0], &chunks.rows[right.row][0])
            })
    };
    if candidates.len() > limit {
        candidates.select_nth_unstable_by(limit, compare);
        candidates.truncate(limit);
    }
    candidates.sort_by(compare);
}
