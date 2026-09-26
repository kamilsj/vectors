//! Query-aware, bounded beam expansion with snapshot-owned path evidence.

use super::*;

pub(super) struct Admission {
    pub row: usize,
    pub depth: usize,
    pub score: CandidateScore,
    pub retrieval_path: Option<GraphRagPath>,
}

// Keep row references in the beam; only admitted paths copy edge strings.
#[derive(Clone, Copy)]
struct Trace {
    seed: usize,
    edges: [usize; 3],
    len: usize,
}

#[derive(Clone, Copy)]
struct Evidence {
    row: usize,
    score: CandidateScore,
    strength: f64,
    trace: Trace,
}

pub(super) struct GraphTraversal<'a> {
    pub request: &'a GraphRagRequest,
    pub policy: &'a GraphRagTraversal,
    pub chunks: &'a Table,
    pub edges: &'a Table,
    pub lookup: &'a HashMap<&'a str, usize>,
    pub scores: &'a HashMap<usize, CandidateScore>,
    pub lexical_scores: &'a [f64],
    pub similarities: Vec<Option<f64>>,
    pub eligible: Option<&'a [bool]>,
    pub max_seeds_per_document: Option<usize>,
}

impl GraphTraversal<'_> {
    pub fn expand(mut self, ranked: &[usize]) -> Result<(Vec<Admission>, bool)> {
        let request = self.request;
        let seeds = self.select_seeds(ranked)?;
        let base_limit = if request.max_hops == 0 {
            request.candidate_limit
        } else {
            seeds.len().max(
                request
                    .candidate_limit
                    .saturating_sub((request.candidate_limit / 4).max(1)),
            )
        };
        // Reserve places for every chosen seed, including those below the
        // ordinary direct-match cutoff, then fill remaining places by rank.
        // This keeps both the direct pool and the graph frontier bounded.
        let mut direct_rows = seeds.clone();
        let mut included = seeds.iter().copied().collect::<HashSet<_>>();
        for &row in ranked {
            if direct_rows.len() == base_limit {
                break;
            }
            if included.insert(row) {
                direct_rows.push(row);
            }
        }
        let mut admitted = direct_rows
            .into_iter()
            .map(|row| Admission {
                row,
                depth: 0,
                score: self.scores[&row],
                retrieval_path: None,
            })
            .collect::<Vec<_>>();
        let direct = admitted
            .iter()
            .enumerate()
            .map(|(position, item)| (item.row, position))
            .collect::<HashMap<_, _>>();
        let mut frontier = seeds
            .into_iter()
            .map(|row| Evidence {
                row,
                score: self.scores[&row],
                strength: self.scores[&row].fusion,
                trace: Trace {
                    seed: row,
                    edges: [0; 3],
                    len: 0,
                },
            })
            .collect::<Vec<_>>();
        let mut best_path = vec![0.0f64; self.chunks.rows.len()];
        for candidate in &frontier {
            best_path[candidate.row] = candidate.strength;
        }
        let mut context: HashMap<usize, Evidence> = HashMap::new();
        let lexical_max = self.lexical_scores.iter().copied().fold(0.0, f64::max);
        let outgoing = self
            .edges
            .indexes
            .values()
            .find(|index| index.column == 1)
            .ok_or_else(|| invalid("graph edge source index is missing"))?;
        let incoming = self
            .edges
            .indexes
            .values()
            .find(|index| index.column == 2)
            .ok_or_else(|| invalid("graph edge target index is missing"))?;
        let indexes: &[(&HashIndex, usize)] = match self.policy.direction {
            GraphNeighborhoodDirection::Outgoing => &[(outgoing, 2)],
            GraphNeighborhoodDirection::Incoming => &[(incoming, 1)],
            GraphNeighborhoodDirection::Both => &[(outgoing, 2), (incoming, 1)],
        };
        let mut truncated = false;
        for _ in 0..request.max_hops {
            let mut proposals: HashMap<usize, Evidence> = HashMap::new();
            for source in &frontier {
                let source_id = text_at(&self.chunks.rows[source.row], 0)?;
                let key = UniqueKey::from(&Value::Text(source_id.into()));
                let mut targets: HashMap<usize, (f64, usize)> = HashMap::new();
                for (index, endpoint) in indexes {
                    for &edge_index in index.buckets.get(&key).into_iter().flatten() {
                        let row = &self.edges.rows[edge_index];
                        let edge = read_edge(row)?;
                        // Filter before coalescing, scoring, or consuming slots.
                        if !self.policy.includes(&edge) || edge.weight == 0.0 {
                            continue;
                        }
                        let target = *self
                            .lookup
                            .get(text_at(row, *endpoint)?)
                            .ok_or_else(|| invalid("graph edge references a missing chunk"))?;
                        if self.eligible.is_some_and(|rows| !rows[target]) {
                            continue;
                        }
                        targets
                            .entry(target)
                            .and_modify(|(weight, previous)| {
                                if edge.weight > *weight
                                    || (edge.weight == *weight
                                        && edge_order(row, &self.edges.rows[*previous]).is_lt())
                                {
                                    *weight = edge.weight;
                                    *previous = edge_index;
                                }
                            })
                            .or_insert((edge.weight, edge_index));
                    }
                }
                let mut neighbors = Vec::with_capacity(targets.len());
                for (row, (weight, edge_index)) in targets {
                    let original = self.scores.get(&row).copied().unwrap_or(CandidateScore {
                        lexical: self.lexical_scores[row],
                        fusion: 0.0,
                    });
                    // Separate structural strength from query fit so a weakly
                    // matching bridge can still lead to useful evidence.
                    let strength = (source.strength * 0.5 * weight).max(original.fusion);
                    if strength <= best_path[row] {
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
                    let query_fit = 0.2 + 0.8 * vector_fit.max(lexical_fit);
                    let score = CandidateScore {
                        lexical: original.lexical,
                        fusion: original.fusion.max(strength * query_fit),
                    };
                    let mut trace = source.trace;
                    trace.edges[trace.len] = edge_index;
                    trace.len += 1;
                    neighbors.push(Evidence {
                        row,
                        score,
                        strength,
                        trace,
                    });
                }
                truncated |= neighbors.len() > request.neighbor_limit;
                top_evidence(&mut neighbors, request.neighbor_limit, self.chunks);
                for proposal in neighbors {
                    proposals
                        .entry(proposal.row)
                        .and_modify(|old| {
                            if self.preferred(&proposal, old) {
                                *old = proposal;
                            }
                        })
                        .or_insert(proposal);
                }
            }
            // Compare all seeds' proposals before enforcing the global beam cap.
            let mut next = proposals.into_values().collect::<Vec<_>>();
            truncated |= next.len() > request.candidate_limit;
            top_evidence(&mut next, request.candidate_limit, self.chunks);
            for proposal in &next {
                best_path[proposal.row] = proposal.strength;
                if let Some(&position) = direct.get(&proposal.row) {
                    admitted[position].score.fusion =
                        admitted[position].score.fusion.max(proposal.score.fusion);
                } else {
                    context
                        .entry(proposal.row)
                        .and_modify(|old| {
                            // Score, depth and route must describe the same evidence.
                            if self.preferred(proposal, old) {
                                *old = *proposal;
                            }
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
        for item in context {
            let path = GraphRagPath {
                seed_chunk_id: text_at(&self.chunks.rows[item.trace.seed], 0)?.into(),
                edges: item.trace.edges[..item.trace.len]
                    .iter()
                    .map(|&index| read_edge(&self.edges.rows[index]))
                    .collect::<Result<Vec<_>>>()?,
            };
            admitted.push(Admission {
                row: item.row,
                depth: item.trace.len,
                score: item.score,
                retrieval_path: Some(path),
            });
        }
        let mut selected = admitted.iter().map(|item| item.row).collect::<HashSet<_>>();
        for &row in ranked {
            if selected.contains(&row) {
                continue;
            }
            if admitted.len() == request.candidate_limit {
                truncated = true;
                break;
            }
            selected.insert(row);
            admitted.push(Admission {
                row,
                depth: 0,
                score: self.scores[&row],
                retrieval_path: None,
            });
        }
        Ok((admitted, truncated))
    }

    fn select_seeds(&self, ranked: &[usize]) -> Result<Vec<usize>> {
        let Some(limit) = self
            .max_seeds_per_document
            .filter(|_| self.request.max_hops > 0)
        else {
            return Ok(ranked
                .iter()
                .copied()
                .take(self.request.seed_limit)
                .collect());
        };
        let mut counts: HashMap<&str, usize> = HashMap::new();
        let mut seeds = Vec::with_capacity(self.request.seed_limit);
        for &row in ranked {
            let count = counts
                .entry(text_at(&self.chunks.rows[row], 1)?)
                .or_default();
            if *count >= limit {
                continue;
            }
            *count += 1;
            seeds.push(row);
            if seeds.len() == self.request.seed_limit {
                break;
            }
        }
        Ok(seeds)
    }

    fn preferred(&self, next: &Evidence, previous: &Evidence) -> bool {
        next.strength
            .total_cmp(&previous.strength)
            .then_with(|| previous.trace.len.cmp(&next.trace.len))
            .then_with(|| {
                compare_sort_values(
                    &self.chunks.rows[previous.trace.seed][0],
                    &self.chunks.rows[next.trace.seed][0],
                )
            })
            .then_with(|| {
                previous.trace.edges[..previous.trace.len]
                    .iter()
                    .zip(&next.trace.edges[..next.trace.len])
                    .map(|(&left, &right)| {
                        edge_order(&self.edges.rows[left], &self.edges.rows[right])
                    })
                    .find(|order| !order.is_eq())
                    .unwrap_or(Ordering::Equal)
            })
            .is_gt()
    }
}

fn edge_order(left: &[Value], right: &[Value]) -> Ordering {
    [1, 2, 3]
        .into_iter()
        .map(|column| compare_sort_values(&left[column], &right[column]))
        .find(|order| !order.is_eq())
        .unwrap_or(Ordering::Equal)
}

fn top_evidence(candidates: &mut Vec<Evidence>, limit: usize, chunks: &Table) {
    let compare = |left: &Evidence, right: &Evidence| {
        right
            .score
            .fusion
            .total_cmp(&left.score.fusion)
            .then_with(|| right.strength.total_cmp(&left.strength))
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
