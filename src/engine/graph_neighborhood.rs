//! Provider-free focused exploration of the stored, directed chunk graph.

use super::*;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GraphNeighborhoodDirection {
    Outgoing,
    Incoming,
    #[default]
    Both,
}

#[derive(Clone, Debug)]
pub struct GraphNeighborhoodRequest {
    pub collection: String,
    pub chunk_id: String,
    pub max_hops: usize,
    pub neighbor_limit: usize,
    pub max_nodes: usize,
    pub max_edges: usize,
    pub direction: GraphNeighborhoodDirection,
    pub kind: Option<String>,
    pub min_weight: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GraphNeighborhoodNode {
    #[serde(flatten)]
    pub node: GraphNode,
    pub depth: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GraphNeighborhoodResult {
    pub collection: String,
    pub revision: u64,
    pub root_chunk: String,
    pub nodes: Vec<GraphNeighborhoodNode>,
    pub edges: Vec<GraphEdge>,
    pub truncated: bool,
}

impl GraphNeighborhoodRequest {
    fn validate(&self) -> Result<()> {
        if self.max_hops > 3
            || !(1..=32).contains(&self.neighbor_limit)
            || !(1..=200).contains(&self.max_nodes)
            || self.max_edges > 2000
            || !self.min_weight.is_finite()
            || !(0.0..=1.0).contains(&self.min_weight)
        {
            return Err(invalid("graph neighborhood requires hops 0..3, neighbors 1..32, nodes 1..200, edges 0..2000, and finite minimum weight 0..1"));
        }
        bounded_text(&self.chunk_id, 1024, false, "root chunk id")?;
        if let Some(kind) = &self.kind {
            // Use the same relationship-label validation as stored edges.
            read_edge(&edge_row(&edge(
                "source".into(),
                "target".into(),
                kind,
                1.0,
            )))?;
        }
        Ok(())
    }

    fn includes(&self, edge: &GraphEdge) -> bool {
        self.kind.as_ref().is_none_or(|kind| kind == &edge.kind) && edge.weight >= self.min_weight
    }
}

impl Database {
    /// Explore stored relationships around a chunk using one catalog snapshot.
    /// Depth is the shortest discovered path under the requested filters and
    /// traversal bounds, not a relevance score or a claim about the source text.
    pub fn graph_neighborhood(
        &self,
        request: GraphNeighborhoodRequest,
    ) -> Result<GraphNeighborhoodResult> {
        request.validate()?;
        let catalog = self.catalog.read().map_err(|_| Error::LockPoisoned)?;
        let info = collection(&catalog, &request.collection)?;
        let chunks = table(&catalog, &info.tables.chunks)?;
        let lookup = id_index(chunks)?;
        let root = *lookup
            .get(&UniqueKey::from(&Value::Text(request.chunk_id.clone())))
            .ok_or_else(|| invalid("graph neighborhood root chunk does not exist"))?;
        let edge_table = table(&catalog, &info.tables.edges)?;
        let outgoing = edge_table
            .indexes
            .values()
            .find(|index| index.column == 1)
            .ok_or_else(|| invalid("graph edge source index is missing"))?;
        let incoming = edge_table
            .indexes
            .values()
            .find(|index| index.column == 2)
            .ok_or_else(|| invalid("graph edge target index is missing"))?;
        let indexes: &[(&HashIndex, usize)] = match request.direction {
            GraphNeighborhoodDirection::Outgoing => &[(outgoing, 2)],
            GraphNeighborhoodDirection::Incoming => &[(incoming, 1)],
            GraphNeighborhoodDirection::Both => &[(outgoing, 2), (incoming, 1)],
        };
        let mut selected = vec![(root, 0)];
        let mut visited = HashSet::from([root]);
        let mut frontier = vec![root];
        let mut truncated = false;
        for depth in 0..=request.max_hops {
            // Merge each breadth-first layer before enforcing the global node
            // cap, so a later parent can contribute a stronger relationship.
            let mut next: HashMap<usize, f64> = HashMap::new();
            for current in frontier {
                let key = UniqueKey::from(&Value::Text(text_at(&chunks.rows[current], 0)?.into()));
                let mut neighbors: HashMap<usize, f64> = HashMap::new();
                for (index, endpoint) in indexes {
                    for row in index.buckets.get(&key).into_iter().flatten() {
                        let row = &edge_table.rows[*row];
                        let edge = read_edge(row)?;
                        if !request.includes(&edge) {
                            continue;
                        }
                        let neighbor = *lookup
                            .get(&UniqueKey::from(&Value::Text(
                                text_at(row, *endpoint)?.into(),
                            )))
                            .ok_or_else(|| invalid("graph edge references a missing chunk"))?;
                        if visited.contains(&neighbor) {
                            continue;
                        }
                        neighbors
                            .entry(neighbor)
                            .and_modify(|weight| *weight = weight.max(edge.weight))
                            .or_insert(edge.weight);
                    }
                }
                let mut neighbors = neighbors.into_iter().collect::<Vec<_>>();
                rank_neighbors(&mut neighbors, chunks);
                truncated |= neighbors.len() > request.neighbor_limit;
                for (neighbor, weight) in neighbors.into_iter().take(request.neighbor_limit) {
                    next.entry(neighbor)
                        .and_modify(|previous| *previous = previous.max(weight))
                        .or_insert(weight);
                }
            }
            if depth == request.max_hops {
                truncated |= !next.is_empty();
                break;
            }
            let mut next = next.into_iter().collect::<Vec<_>>();
            rank_neighbors(&mut next, chunks);
            let capacity = request.max_nodes - selected.len();
            truncated |= next.len() > capacity;
            next.truncate(capacity);
            frontier = next.into_iter().map(|(index, _)| index).collect();
            for &index in &frontier {
                visited.insert(index);
                selected.push((index, depth + 1));
            }
            if frontier.is_empty() {
                break;
            }
        }
        let documents = table(&catalog, &info.tables.documents)?;
        let document_ids = id_index(documents)?;
        let nodes = selected
            .iter()
            .map(|(index, depth)| {
                citation_node(&chunks.rows[*index], documents, document_ids, *depth)
            })
            .collect::<Result<Vec<_>>>()?;
        let mut edges = BoundedEdges::new(request.max_edges);
        // Return the induced eligible directed graph, including cycles and
        // links between already-visited nodes. Direction only affects traversal.
        for (index, _) in selected {
            let key = UniqueKey::from(&Value::Text(text_at(&chunks.rows[index], 0)?.into()));
            for row in outgoing.buckets.get(&key).into_iter().flatten() {
                let edge = read_edge(&edge_table.rows[*row])?;
                if !request.includes(&edge) {
                    continue;
                }
                let target = *lookup
                    .get(&UniqueKey::from(&Value::Text(edge.to_chunk.clone())))
                    .ok_or_else(|| invalid("graph edge references a missing chunk"))?;
                if visited.contains(&target) {
                    edges.insert(edge);
                }
            }
        }
        truncated |= edges.truncated;
        Ok(GraphNeighborhoodResult {
            collection: info.config.name,
            revision: catalog.revision,
            root_chunk: request.chunk_id,
            nodes,
            edges: edges.ordered.into_iter().map(|edge| edge.0).collect(),
            truncated,
        })
    }
}

fn rank_neighbors(neighbors: &mut [(usize, f64)], chunks: &Table) {
    neighbors.sort_by(|(left, a), (right, b)| {
        b.total_cmp(a)
            .then_with(|| compare_sort_values(&chunks.rows[*left][0], &chunks.rows[*right][0]))
    });
}

fn citation_node(
    chunk: &[Value],
    documents: &Table,
    document_ids: &HashMap<UniqueKey, usize>,
    depth: usize,
) -> Result<GraphNeighborhoodNode> {
    let document_id = text_at(chunk, 1)?;
    let index = document_ids
        .get(&UniqueKey::from(&Value::Text(document_id.into())))
        .ok_or_else(|| invalid("graph chunk references a missing document"))?;
    let document = &documents.rows[*index];
    let start_byte = usize_at(chunk, 3)?;
    let end_byte = usize_at(chunk, 4)?;
    let text = text_at(chunk, 5)?;
    if text_at(document, 3)?.get(start_byte..end_byte) != Some(text) {
        return Err(invalid(
            "graph citation no longer matches its source document",
        ));
    }
    Ok(GraphNeighborhoodNode {
        node: GraphNode {
            chunk_id: text_at(chunk, 0)?.into(),
            document_id: document_id.into(),
            title: text_at(document, 1)?.into(),
            source: text_at(document, 2)?.into(),
            text: text.into(),
            metadata: document_metadata(document, &documents.columns)?,
            start_byte,
            end_byte,
            ordinal: usize_at(chunk, 2)?,
        },
        depth,
    })
}

// These maps are maintained atomically by all SQL/typed writes and rebuilt on
// recovery. Borrow them under the same catalog lock as the graph traversal.
fn id_index(table: &Table) -> Result<&HashMap<UniqueKey, usize>> {
    table
        .unique_keys
        .get(&0)
        .ok_or_else(|| invalid("graph unique id index is missing"))
}

#[derive(Clone, Debug)]
struct RankedEdge(GraphEdge);
impl PartialEq for RankedEdge {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}
impl Eq for RankedEdge {}
impl PartialOrd for RankedEdge {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for RankedEdge {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .0
            .weight
            .total_cmp(&self.0.weight)
            .then_with(|| self.0.from_chunk.cmp(&other.0.from_chunk))
            .then_with(|| self.0.to_chunk.cmp(&other.0.to_chunk))
            .then_with(|| self.0.kind.cmp(&other.0.kind))
    }
}

/// Keep only the strongest bounded set while coalescing raw SQL duplicates.
/// Memory scales with the requested edge limit, never the stored edge count.
struct BoundedEdges {
    limit: usize,
    weights: BTreeMap<(String, String, String), f64>,
    ordered: BTreeSet<RankedEdge>,
    truncated: bool,
}
impl BoundedEdges {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            weights: BTreeMap::new(),
            ordered: BTreeSet::new(),
            truncated: false,
        }
    }
    fn insert(&mut self, edge: GraphEdge) {
        let key = (
            edge.from_chunk.clone(),
            edge.to_chunk.clone(),
            edge.kind.clone(),
        );
        if let Some(&weight) = self.weights.get(&key) {
            if weight >= edge.weight {
                return;
            }
            let mut previous = edge.clone();
            previous.weight = weight;
            self.ordered.remove(&RankedEdge(previous));
        }
        self.weights.insert(key, edge.weight);
        self.ordered.insert(RankedEdge(edge));
        if self.ordered.len() > self.limit {
            if let Some(RankedEdge(edge)) = self.ordered.pop_last() {
                self.weights
                    .remove(&(edge.from_chunk, edge.to_chunk, edge.kind));
            }
            self.truncated = true;
        }
    }
}
