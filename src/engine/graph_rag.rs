//! Bounded graph browsing, relationship edits, and snapshot-based hybrid RAG.

use super::*;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{OnceLock, Weak};

#[path = "graph_traversal.rs"]
mod traversal;

#[derive(Clone, Debug)]
pub struct GraphBrowseRequest {
    pub collection: String,
    pub document_id: Option<String>,
    pub offset: usize,
    pub limit: usize,
    pub max_edges: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GraphNode {
    pub chunk_id: String,
    pub document_id: String,
    pub title: String,
    pub source: String,
    pub text: String,
    pub metadata: JsonValue,
    pub start_byte: usize,
    pub end_byte: usize,
    pub ordinal: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GraphBrowseResult {
    pub collection: String,
    pub revision: u64,
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    pub total_nodes: usize,
    pub total_edges: usize,
    pub offset: usize,
    pub limit: usize,
    pub truncated: bool,
}

#[derive(Clone, Debug)]
pub struct GraphRelationshipRequest {
    pub collection: String,
    pub expected_revision: u64,
    pub from_chunk: String,
    pub to_chunk: String,
    pub kind: String,
    pub weight: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GraphRelationshipResult {
    pub collection: String,
    pub revision: u64,
    pub edge: GraphEdge,
    pub created: bool,
}

#[derive(Clone, Debug)]
pub struct GraphRelationshipDeleteRequest {
    pub collection: String,
    pub expected_revision: u64,
    pub from_chunk: String,
    pub to_chunk: String,
    pub kind: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GraphRelationshipDeleteResult {
    pub collection: String,
    pub revision: u64,
    pub edges_removed: usize,
}

#[derive(Clone, Debug)]
pub struct GraphRagRequest {
    pub collection: String,
    pub expected_profile: GraphEmbeddingProfile,
    pub query: Vector,
    pub query_text: String,
    pub candidate_limit: usize,
    pub seed_limit: usize,
    pub max_hops: usize,
    pub neighbor_limit: usize,
    pub vector_weight: f64,
    pub lexical_weight: f64,
}

/// Optional relationship policy for graph expansion. Direct hybrid matches
/// remain eligible independently of this policy.
#[derive(Clone, Debug, serde::Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct GraphRagTraversal {
    pub direction: GraphNeighborhoodDirection,
    pub kind: Option<String>,
    pub min_weight: f64,
}

impl Default for GraphRagTraversal {
    fn default() -> Self {
        Self {
            direction: GraphNeighborhoodDirection::Outgoing,
            kind: None,
            min_weight: 0.0,
        }
    }
}

impl GraphRagTraversal {
    /// Validate before provider work so malformed filters cannot incur usage.
    pub fn validate(&self) -> Result<()> {
        if !self.min_weight.is_finite() || !(0.0..=1.0).contains(&self.min_weight) {
            return Err(invalid(
                "RAG minimum relationship weight must be finite and in 0..1",
            ));
        }
        if let Some(kind) = &self.kind {
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

/// One retained discovery route from a hybrid seed to a graph-context hit.
/// Edges are ordered along the walk but retain their original stored arrows;
/// incoming exploration walks an edge from its target back to its source.
/// Intermediate passages need not be included in the final context budget.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GraphRagPath {
    pub seed_chunk_id: String,
    pub edges: Vec<GraphEdge>,
}

/// Internal reranking material. Deliberately not serializable: vectors stay on
/// the server while external rerankers receive only the bounded text inputs.
#[derive(Clone, Debug)]
pub struct GraphRagCandidate {
    pub hit: GraphHit,
    pub lexical_score: f64,
    pub fusion_score: f64,
    pub rerank_text: String,
    pub vector: Vector,
    pub retrieval_path: Option<GraphRagPath>,
}

#[derive(Clone, Debug)]
pub struct GraphRagSnapshot {
    pub collection: String,
    pub revision: u64,
    pub candidates: Vec<GraphRagCandidate>,
    pub edges: Vec<GraphEdge>,
    pub lexical_cache_hit: bool,
    pub truncated: bool,
}

#[derive(Clone, Debug)]
pub struct GraphRagSelection {
    pub limit: usize,
    /// Zero ranks by relevance; one maximizes diversity after the first hit.
    pub diversity: f64,
    pub max_context_bytes: usize,
    pub max_per_document: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GraphRagHit {
    #[serde(flatten)]
    pub hit: GraphHit,
    pub lexical_score: f64,
    pub fusion_score: f64,
    pub rerank_score: Option<f64>,
    pub selection_score: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retrieval_path: Option<GraphRagPath>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GraphRagResult {
    pub collection: String,
    pub revision: u64,
    pub hits: Vec<GraphRagHit>,
    pub edges: Vec<GraphEdge>,
    pub lexical_cache_hit: bool,
    pub truncated: bool,
    pub context_bytes: usize,
    pub candidate_count: usize,
}

impl Database {
    /// Browse a deterministic citation-bearing page without an embedding call.
    pub fn graph_browse(&self, request: GraphBrowseRequest) -> Result<GraphBrowseResult> {
        if !(1..=200).contains(&request.limit) || request.max_edges > 2000 {
            return Err(invalid(
                "graph browse requires limit 1..200 and max_edges 0..2000",
            ));
        }
        let catalog = self.catalog.read().map_err(|_| Error::LockPoisoned)?;
        let info = collection(&catalog, &request.collection)?;
        let chunks = table(&catalog, &info.tables.chunks)?;
        let documents = table(&catalog, &info.tables.documents)?;
        let document_lookup = documents
            .rows
            .iter()
            .map(|row| text_at(row, 0).map(|id| (id, row)))
            .collect::<Result<HashMap<_, _>>>()?;
        let mut selected = chunks
            .rows
            .iter()
            .filter(|row| {
                request
                    .document_id
                    .as_deref()
                    .is_none_or(|id| matches!(row.get(1),Some(Value::Text(value)) if value == id))
            })
            .collect::<Vec<_>>();
        selected.sort_by(|left, right| {
            compare_sort_values(&left[1], &right[1])
                .then_with(|| compare_sort_values(&left[2], &right[2]))
                .then_with(|| compare_sort_values(&left[0], &right[0]))
        });
        let total_nodes = selected.len();
        let nodes = selected
            .into_iter()
            .skip(request.offset)
            .take(request.limit)
            .map(|chunk| node(chunk, &document_lookup))
            .collect::<Result<Vec<_>>>()?;
        let selected_ids = nodes
            .iter()
            .map(|node| node.chunk_id.as_str())
            .collect::<HashSet<_>>();
        let (edges, edges_truncated) = induced_edges(
            table(&catalog, &info.tables.edges)?,
            chunks,
            &selected_ids,
            request.max_edges,
            None,
        )?;
        Ok(GraphBrowseResult {
            collection: info.config.name,
            revision: catalog.revision,
            nodes,
            edges,
            total_nodes,
            total_edges: info.edge_count,
            offset: request.offset,
            limit: request.limit,
            truncated: request.offset.saturating_add(request.limit) < total_nodes
                || edges_truncated,
        })
    }

    /// Upsert one directed relationship under revision CAS. Existing duplicate
    /// triples from raw SQL are coalesced into a single canonical edge record.
    pub fn graph_upsert_relationship(
        &self,
        request: GraphRelationshipRequest,
    ) -> Result<GraphRelationshipResult> {
        let relation = edge(
            request.from_chunk,
            request.to_chunk,
            &request.kind,
            request.weight,
        );
        read_edge(&edge_row(&relation))?;
        let (mut result, revision) =
            self.graph_transaction(Some(request.expected_revision), |catalog, wal| {
                let info = collection(catalog, &request.collection)?;
                validate_endpoints(catalog, &info.tables, &relation)?;
                let removed = remove_relationship(catalog, &info.tables, &relation, wal)?;
                if table(catalog, &info.tables.edges)?.rows.len() >= MAX_EDGES {
                    return Err(invalid("graph edge capacity exceeded"));
                }
                insert(catalog, &info.tables.edges, vec![edge_row(&relation)], wal)?;
                validate_text_capacity(catalog, &info.tables)?;
                Ok(GraphRelationshipResult {
                    collection: info.config.name,
                    revision: 0,
                    edge: relation.clone(),
                    created: removed == 0,
                })
            })?;
        result.revision = revision;
        Ok(result)
    }

    pub fn graph_delete_relationship(
        &self,
        request: GraphRelationshipDeleteRequest,
    ) -> Result<GraphRelationshipDeleteResult> {
        let relation = edge(request.from_chunk, request.to_chunk, &request.kind, 1.0);
        read_edge(&edge_row(&relation))?;
        let (mut result, revision) =
            self.graph_transaction(Some(request.expected_revision), |catalog, wal| {
                let info = collection(catalog, &request.collection)?;
                validate_endpoints(catalog, &info.tables, &relation)?;
                let removed = remove_relationship(catalog, &info.tables, &relation, wal)?;
                if removed == 0 {
                    return Err(invalid("graph relationship does not exist"));
                }
                Ok(GraphRelationshipDeleteResult {
                    collection: info.config.name,
                    revision: 0,
                    edges_removed: removed,
                })
            })?;
        result.revision = revision;
        Ok(result)
    }
}

fn validate_endpoints(catalog: &Catalog, tables: &GraphTables, edge: &GraphEdge) -> Result<()> {
    let chunks = table(catalog, &tables.chunks)?;
    for id in [&edge.from_chunk, &edge.to_chunk] {
        if !chunks
            .rows
            .iter()
            .any(|row| matches!(row.first(),Some(Value::Text(value)) if value==id))
        {
            return Err(invalid(
                "relationship endpoints must be existing chunks in this collection",
            ));
        }
    }
    Ok(())
}

fn remove_relationship(
    catalog: &mut Catalog,
    tables: &GraphTables,
    edge: &GraphEdge,
    wal: &mut Option<String>,
) -> Result<usize> {
    let table = catalog
        .tables
        .get_mut(&tables.edges)
        .ok_or_else(|| Error::TableNotFound(tables.edges.clone()))?;
    let before = table.rows.len();
    table.rows.retain(|row| !matches!((row.get(1),row.get(2),row.get(3)),(Some(Value::Text(from)),Some(Value::Text(to)),Some(Value::Text(kind))) if from==&edge.from_chunk && to==&edge.to_chunk && kind==&edge.kind));
    let removed = before - table.rows.len();
    if removed > 0 {
        rebuild_indexes(table);
        if let Some(wal) = wal {
            wal.push_str(&format!(
                "DELETE FROM {} WHERE from_chunk={} AND to_chunk={} AND kind={};",
                quote(&tables.edges),
                literal(&edge.from_chunk),
                literal(&edge.to_chunk),
                literal(&edge.kind)
            ));
        }
    }
    Ok(removed)
}

fn node(chunk: &[Value], documents: &HashMap<&str, &Vec<Value>>) -> Result<GraphNode> {
    let document_id = text_at(chunk, 1)?;
    let document = *documents
        .get(document_id)
        .ok_or_else(|| invalid("graph chunk references a missing document"))?;
    let start_byte = usize_at(chunk, 3)?;
    let end_byte = usize_at(chunk, 4)?;
    let text = text_at(chunk, 5)?;
    if text_at(document, 3)?.get(start_byte..end_byte) != Some(text) {
        return Err(invalid(
            "graph citation no longer matches its source document",
        ));
    }
    Ok(GraphNode {
        chunk_id: text_at(chunk, 0)?.into(),
        document_id: document_id.into(),
        title: text_at(document, 1)?.into(),
        source: text_at(document, 2)?.into(),
        text: text.into(),
        metadata: json_at(document, 4)?,
        start_byte,
        end_byte,
        ordinal: usize_at(chunk, 2)?,
    })
}

fn induced_edges(
    edge_table: &Table,
    chunks: &Table,
    selected: &HashSet<&str>,
    limit: usize,
    traversal: Option<&GraphRagTraversal>,
) -> Result<(Vec<GraphEdge>, bool)> {
    let known = chunks
        .rows
        .iter()
        .map(|row| text_at(row, 0))
        .collect::<Result<HashSet<_>>>()?;
    let index = edge_table
        .indexes
        .values()
        .find(|index| index.column == 1)
        .ok_or_else(|| invalid("graph edge source index is missing"))?;
    let mut edges = BTreeMap::new();
    let mut truncated = false;
    let mut sources = selected.iter().copied().collect::<Vec<_>>();
    sources.sort_unstable();
    for source in sources {
        for row_index in index
            .buckets
            .get(&UniqueKey::from(&Value::Text(source.into())))
            .into_iter()
            .flatten()
        {
            let edge = read_edge(&edge_table.rows[*row_index])?;
            if traversal.is_some_and(|policy| !policy.includes(&edge)) {
                continue;
            }
            if !known.contains(edge.to_chunk.as_str()) {
                return Err(invalid("graph edge references a missing chunk"));
            }
            if selected.contains(edge.to_chunk.as_str()) {
                let key = (
                    edge.from_chunk.clone(),
                    edge.to_chunk.clone(),
                    edge.kind.clone(),
                );
                edges
                    .entry(key)
                    .and_modify(|stored: &mut GraphEdge| {
                        stored.weight = stored.weight.max(edge.weight);
                    })
                    .or_insert(edge);
                if edges.len() > limit {
                    edges.pop_last();
                    truncated = true;
                }
            }
        }
    }
    Ok((edges.into_values().collect(), truncated))
}

const LEXICAL_INDEX_BYTES: usize = 16 * 1024 * 1024;
const LEXICAL_CACHE_ENTRIES: usize = 3;
const RRF_OFFSET: f64 = 60.0;

struct LexicalIndex {
    length_factors: Vec<f64>,
    postings: HashMap<String, Vec<(usize, usize)>>,
}
struct LexicalCacheEntry {
    owner: Weak<RwLock<Catalog>>,
    collection: String,
    storage_id: u64,
    row_count: usize,
    index: Arc<LexicalIndex>,
}
static LEXICAL_CACHE: OnceLock<Mutex<VecDeque<LexicalCacheEntry>>> = OnceLock::new();

fn token_words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
}

fn tokens(text: &str) -> impl Iterator<Item = String> + '_ {
    token_words(text).map(str::to_lowercase)
}

fn index_tokens(text: &str) -> impl Iterator<Item = std::borrow::Cow<'_, str>> {
    token_words(text).map(|word| {
        // Most source words already are lowercase ASCII. Borrow those until
        // their frequency is known instead of allocating once per occurrence.
        // Non-ASCII must retain str::to_lowercase's complete Unicode behavior,
        // including titlecase letters and multi-character case mappings.
        if word
            .bytes()
            .all(|byte| byte.is_ascii() && !byte.is_ascii_uppercase())
        {
            std::borrow::Cow::Borrowed(word)
        } else {
            std::borrow::Cow::Owned(word.to_lowercase())
        }
    })
}

impl LexicalIndex {
    // Account conservatively for hash entries, string capacity and posting
    // allocations. An oversized vocabulary falls back to exact query-only
    // indexing rather than silently dropping words from BM25.
    fn build(chunks: &Table, query_terms: Option<&BTreeSet<String>>) -> Result<Option<Self>> {
        let mut postings: HashMap<String, Vec<(usize, usize)>> = HashMap::new();
        let mut lengths = Vec::with_capacity(chunks.rows.len());
        let mut bytes = chunks.rows.len() * std::mem::size_of::<usize>();
        for (row_index, row) in chunks.rows.iter().enumerate() {
            let mut counts = HashMap::new();
            let mut length = 0;
            let text = text_at(row, 6)?;
            bounded_text(text, 32 * 1024, false, "stored embedding text")?;
            for term in index_tokens(text) {
                length += 1;
                if query_terms.is_none_or(|terms| terms.contains(term.as_ref())) {
                    *counts.entry(term).or_insert(0usize) += 1;
                }
            }
            lengths.push(length);
            for (term, frequency) in counts {
                bytes = bytes.saturating_add(32);
                if let Some(existing) = postings.get_mut(term.as_ref()) {
                    if query_terms.is_none() && bytes > LEXICAL_INDEX_BYTES {
                        return Ok(None);
                    }
                    existing.push((row_index, frequency));
                } else {
                    bytes = bytes.saturating_add(128 + term.len() * 2);
                    if query_terms.is_none() && bytes > LEXICAL_INDEX_BYTES {
                        return Ok(None);
                    }
                    // Own a vocabulary key only once, when it first appears
                    // anywhere in the collection (or query-only fallback).
                    postings.insert(term.into_owned(), vec![(row_index, frequency)]);
                }
            }
        }
        let average_length =
            (lengths.iter().sum::<usize>() as f64 / lengths.len().max(1) as f64).max(f64::EPSILON);
        // Evaluate exactly the same BM25 length expression once per row instead
        // of repeating its division for every matched query term.
        let length_factors = lengths
            .into_iter()
            .map(|length| 1.2 * (0.25 + 0.75 * length as f64 / average_length))
            .collect::<Vec<_>>();
        let index = Self {
            length_factors,
            postings,
        };
        // Include actual vector/string capacities, plus conservative hash-table
        // slot/control overhead. The incremental estimate above stops oversized
        // builds early; this final check bounds what the cache actually retains.
        if query_terms.is_none() && index.retained_bytes() > LEXICAL_INDEX_BYTES {
            return Ok(None);
        }
        Ok(Some(index))
    }

    fn retained_bytes(&self) -> usize {
        let hash_slot_bytes = std::mem::size_of::<(String, Vec<(usize, usize)>)>() + 32;
        self.postings.iter().fold(
            std::mem::size_of::<Self>()
                .saturating_add(self.length_factors.capacity() * std::mem::size_of::<f64>())
                .saturating_add(self.postings.capacity().saturating_mul(hash_slot_bytes)),
            |bytes, (term, postings)| {
                bytes.saturating_add(term.capacity()).saturating_add(
                    postings
                        .capacity()
                        .saturating_mul(std::mem::size_of::<(usize, usize)>()),
                )
            },
        )
    }

    fn score(
        &self,
        terms: &BTreeSet<String>,
        chunks: &Table,
        limit: usize,
    ) -> (Vec<f64>, Vec<(usize, f64)>) {
        let mut scores = vec![0.0; self.length_factors.len()];
        let count = self.length_factors.len() as f64;
        // Sorted query terms ensure stable floating-point accumulation.
        for term in terms {
            let Some(postings) = self.postings.get(term) else {
                continue;
            };
            let frequency = postings.len() as f64;
            let idf = (1.0 + (count - frequency + 0.5) / (frequency + 0.5)).ln();
            for &(row, frequency) in postings {
                let tf = frequency as f64;
                let denominator = tf + self.length_factors[row];
                scores[row] += idf * tf * 2.2 / denominator;
            }
        }
        let mut ranked = scores
            .iter()
            .copied()
            .enumerate()
            .filter(|(_, score)| *score > 0.0)
            .collect::<Vec<_>>();
        let compare = |(left, a): &(usize, f64), (right, b): &(usize, f64)| {
            b.total_cmp(a)
                .then_with(|| compare_sort_values(&chunks.rows[*left][0], &chunks.rows[*right][0]))
        };
        if ranked.len() > limit {
            ranked.select_nth_unstable_by(limit, compare);
            ranked.truncate(limit);
        }
        ranked.sort_by(compare);
        (scores, ranked)
    }
}

fn lexical_index(
    owner: &Arc<RwLock<Catalog>>,
    name: &str,
    chunks: &Table,
    terms: &BTreeSet<String>,
) -> Result<(Arc<LexicalIndex>, bool)> {
    // This ID is a chunk-table content generation: append changes it and every
    // UPDATE/DELETE (including text-only edits) rebuilds dense storage. Cloning a
    // table for an unrelated transaction preserves it; restore/recreate allocate
    // fresh IDs. Keep this invariant if mutation paths gain selective rebuilds.
    let storage = chunks
        .vector_columns
        .get(&8)
        .filter(|storage| storage.row_count == chunks.rows.len())
        .ok_or_else(|| invalid("graph chunk storage metadata is inconsistent"))?;
    let storage_id = storage.storage_id;
    let row_count = chunks.rows.len();
    let cache = LEXICAL_CACHE.get_or_init(|| Mutex::new(VecDeque::new()));
    {
        let mut entries = cache.lock().map_err(|_| Error::LockPoisoned)?;
        entries.retain(|entry| entry.owner.strong_count() > 0);
        if let Some(position) = entries.iter().position(|entry| {
            entry.owner.ptr_eq(&Arc::downgrade(owner))
                && entry.collection == name
                && entry.storage_id == storage_id
                && entry.row_count == row_count
        }) {
            let entry = entries.remove(position).expect("cache position exists");
            let index = entry.index.clone();
            entries.push_back(entry);
            return Ok((index, true));
        }
        // Do not retain an obsolete full index when the new vocabulary needs
        // the query-only fallback, which is intentionally never cached.
        entries.retain(|entry| {
            !(entry.owner.ptr_eq(&Arc::downgrade(owner)) && entry.collection == name)
        });
    }
    let Some(index) = LexicalIndex::build(chunks, None)? else {
        // Query-only postings keep exact BM25 semantics for large vocabularies
        // while bounding the retained cache. At most 256 query terms are used.
        return Ok((
            Arc::new(
                LexicalIndex::build(chunks, Some(terms))?
                    .ok_or_else(|| invalid("could not build lexical query index"))?,
            ),
            false,
        ));
    };
    let index = Arc::new(index);
    let mut entries = cache.lock().map_err(|_| Error::LockPoisoned)?;
    // Builders never hold the global lock while tokenizing. If another reader
    // published this generation meanwhile, reuse it instead of replacing it.
    if let Some(position) = entries.iter().position(|entry| {
        entry.owner.ptr_eq(&Arc::downgrade(owner))
            && entry.collection == name
            && entry.storage_id == storage_id
            && entry.row_count == row_count
    }) {
        let entry = entries.remove(position).expect("cache position exists");
        let index = entry.index.clone();
        entries.push_back(entry);
        return Ok((index, true));
    }
    entries
        .retain(|entry| !(entry.owner.ptr_eq(&Arc::downgrade(owner)) && entry.collection == name));
    while entries.len() >= LEXICAL_CACHE_ENTRIES {
        entries.pop_front();
    }
    entries.push_back(LexicalCacheEntry {
        owner: Arc::downgrade(owner),
        collection: name.into(),
        storage_id,
        row_count,
        index: index.clone(),
    });
    Ok((index, false))
}

#[derive(Clone, Copy, Default)]
struct CandidateScore {
    lexical: f64,
    fusion: f64,
}

impl GraphRagRequest {
    /// Validate query size and lexical term count before requesting embeddings.
    pub fn validate_query_text(text: &str) -> Result<()> {
        bounded_text(text, 8191, false, "RAG query")?;
        if tokens(text).collect::<BTreeSet<_>>().len() > 256 {
            return Err(invalid(
                "RAG query may contain at most 256 distinct lexical terms",
            ));
        }
        Ok(())
    }
}

impl Database {
    /// Materialize a coherent, bounded hybrid retrieval snapshot. No database
    /// access is needed while an external reranker is running or during final
    /// diversity selection; all candidate vectors and citations are owned.
    pub fn graph_rag_candidates(&self, request: GraphRagRequest) -> Result<GraphRagSnapshot> {
        self.graph_rag_candidates_with_traversal(request, GraphRagTraversal::default())
    }

    /// Retrieve with an explicit direction and relationship filter. The
    /// default policy preserves the outgoing traversal of `graph_rag_candidates`.
    pub fn graph_rag_candidates_with_traversal(
        &self,
        request: GraphRagRequest,
        policy: GraphRagTraversal,
    ) -> Result<GraphRagSnapshot> {
        policy.validate()?;
        if !(1..=100).contains(&request.candidate_limit)
            || !(1..=20).contains(&request.seed_limit)
            || request.seed_limit > request.candidate_limit
            || request.max_hops > 3
            || !(1..=32).contains(&request.neighbor_limit)
            || !request.vector_weight.is_finite()
            || !(0.0..=10.0).contains(&request.vector_weight)
            || !request.lexical_weight.is_finite()
            || !(0.0..=10.0).contains(&request.lexical_weight)
            || request.vector_weight + request.lexical_weight == 0.0
        {
            return Err(invalid("invalid RAG candidate limits or retrieval weights"));
        }
        GraphRagRequest::validate_query_text(&request.query_text)?;
        let terms = tokens(&request.query_text).collect::<BTreeSet<_>>();
        let catalog = self.catalog.read().map_err(|_| Error::LockPoisoned)?;
        let info = collection(&catalog, &request.collection)?;
        check_profile(&info.config.profile, &request.expected_profile)?;
        if request.query.dimensions() != info.config.profile.dimensions {
            return Err(Error::DimensionMismatch {
                left: request.query.dimensions(),
                right: info.config.profile.dimensions,
            });
        }
        if request.query.norm() == 0.0 {
            return Err(Error::ZeroNorm);
        }
        let chunks = table(&catalog, &info.tables.chunks)?;
        let lookup = chunks
            .rows
            .iter()
            .enumerate()
            .map(|(index, row)| text_at(row, 0).map(|id| (id, index)))
            .collect::<Result<HashMap<_, _>>>()?;
        let documents = table(&catalog, &info.tables.documents)?
            .rows
            .iter()
            .map(|row| text_at(row, 0).map(|id| (id, row)))
            .collect::<Result<HashMap<_, _>>>()?;
        let mut scores: HashMap<usize, CandidateScore> = HashMap::new();
        let mut query_similarities = vec![None; chunks.rows.len()];
        if request.vector_weight > 0.0 {
            let result = run_typed_vector_search(
                chunks,
                VectorSearch {
                    table: info.tables.chunks.clone(),
                    vector_column: "embedding".into(),
                    query: request.query.clone(),
                    metric: VectorSearchMetric::Cosine,
                    select: vec!["chunk_id".into()],
                    filters: Vec::new(),
                    limit: request.candidate_limit,
                },
                &self.compute,
            )?;
            for (rank, row) in result.rows.iter().enumerate() {
                let index = *lookup
                    .get(text_at(row, 0)?)
                    .ok_or_else(|| invalid("vector search returned an unknown chunk"))?;
                query_similarities[index] = Some((1.0 - number_at(row, 1)?).clamp(-1.0, 1.0));
                scores.entry(index).or_default().fusion +=
                    request.vector_weight / (RRF_OFFSET + rank as f64 + 1.0);
            }
        }
        let mut lexical_cache_hit = false;
        let mut lexical_scores = vec![0.0; chunks.rows.len()];
        if request.lexical_weight > 0.0 && !terms.is_empty() {
            let (index, cache_hit) =
                lexical_index(&self.catalog, &info.config.name, chunks, &terms)?;
            lexical_cache_hit = cache_hit;
            let (all_scores, lexical_ranks) = index.score(&terms, chunks, request.candidate_limit);
            lexical_scores = all_scores;
            for (rank, (index, _)) in lexical_ranks.into_iter().enumerate() {
                scores.entry(index).or_default().fusion +=
                    request.lexical_weight / (RRF_OFFSET + rank as f64 + 1.0);
            }
            for (index, score) in &mut scores {
                score.lexical = lexical_scores[*index];
            }
        }
        let mut ranked = scores.keys().copied().collect::<Vec<_>>();
        ranked.sort_by(|left, right| {
            scores[right]
                .fusion
                .total_cmp(&scores[left].fusion)
                .then_with(|| compare_sort_values(&chunks.rows[*left][0], &chunks.rows[*right][0]))
        });
        let edge_table = table(&catalog, &info.tables.edges)?;
        let (admitted, truncated) = traversal::GraphTraversal {
            request: &request,
            policy: &policy,
            chunks,
            edges: edge_table,
            lookup: &lookup,
            scores: &scores,
            lexical_scores: &lexical_scores,
            similarities: query_similarities,
        }
        .expand(&ranked)?;
        let mut candidates = Vec::with_capacity(admitted.len());
        for admission in admitted {
            let row = &chunks.rows[admission.row];
            let Some(Value::Vector(vector)) = row.get(8) else {
                return Err(invalid("graph chunk embedding is missing"));
            };
            candidates.push(GraphRagCandidate {
                hit: hit(row, &documents, &request.query, admission.depth)?,
                lexical_score: admission.score.lexical,
                fusion_score: admission.score.fusion,
                rerank_text: text_at(row, 6)?.into(),
                vector: vector.clone(),
                retrieval_path: admission.retrieval_path,
            });
        }
        candidates.sort_by(|left, right| {
            right
                .fusion_score
                .total_cmp(&left.fusion_score)
                .then_with(|| left.hit.chunk_id.cmp(&right.hit.chunk_id))
        });
        let selected = candidates
            .iter()
            .map(|candidate| candidate.hit.chunk_id.as_str())
            .collect::<HashSet<_>>();
        let (edges, edges_truncated) = induced_edges(
            edge_table,
            chunks,
            &selected,
            request.candidate_limit * request.neighbor_limit,
            Some(&policy),
        )?;
        Ok(GraphRagSnapshot {
            collection: info.config.name,
            revision: catalog.revision,
            candidates,
            edges,
            lexical_cache_hit,
            truncated: truncated || edges_truncated,
        })
    }
}

impl GraphRagSnapshot {
    /// Apply optional external scores and local MMR without any fresh reads.
    /// Context budgets count UTF-8 bytes of whole source chunks, never a cut
    /// substring; citations therefore always remain exact source intervals.
    pub fn finalize(
        self,
        options: GraphRagSelection,
        rerank_scores: Option<&[f64]>,
    ) -> Result<GraphRagResult> {
        if !(1..=100).contains(&options.limit)
            || !options.diversity.is_finite()
            || !(0.0..=1.0).contains(&options.diversity)
            || !(1..=1024 * 1024).contains(&options.max_context_bytes)
            || !(1..=100).contains(&options.max_per_document)
        {
            return Err(invalid(
                "invalid RAG diversity, citation budget or result limits",
            ));
        }
        if rerank_scores.is_some_and(|scores| {
            scores.len() != self.candidates.len() || scores.iter().any(|score| !score.is_finite())
        }) {
            return Err(invalid(
                "reranker must return one finite score per candidate",
            ));
        }
        let candidate_count = self.candidates.len();
        if candidate_count > 100 {
            return Err(invalid("RAG snapshots may contain at most 100 candidates"));
        }
        let relevance = self
            .candidates
            .iter()
            .enumerate()
            .map(|(index, candidate)| {
                rerank_scores.map_or(candidate.fusion_score, |scores| scores[index])
            })
            .collect::<Vec<_>>();
        if relevance.iter().any(|score| !score.is_finite()) {
            return Err(invalid("RAG candidate scores must be finite"));
        }
        let minimum = relevance.iter().copied().fold(f64::INFINITY, f64::min);
        let maximum = relevance.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        // Avoid overflow with even extreme caller-supplied finite scores.
        let scale = minimum.abs().max(maximum.abs()).max(1.0);
        let low = minimum / scale;
        let high = maximum / scale;
        let relevance = relevance
            .iter()
            .map(|score| {
                if high > low {
                    (*score / scale - low) / (high - low)
                } else {
                    1.0
                }
            })
            .collect::<Vec<_>>();
        let normalized_texts = if options.diversity > 0.0 {
            self.candidates
                .iter()
                .map(|candidate| {
                    candidate
                        .hit
                        .text
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                        .to_lowercase()
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let mut selected = Vec::new();
        let mut available = vec![true; candidate_count];
        let mut redundancy = vec![0.0f64; candidate_count];
        let mut counts: HashMap<&str, usize> = HashMap::new();
        let mut context_bytes = 0;
        let mut truncated = self.truncated;
        while selected.len() < options.limit {
            let mut best: Option<(usize, f64)> = None;
            for (index, candidate) in self.candidates.iter().enumerate() {
                if !available[index] {
                    continue;
                }
                if candidate.hit.text.len() > options.max_context_bytes - context_bytes
                    || counts
                        .get(candidate.hit.document_id.as_str())
                        .copied()
                        .unwrap_or(0)
                        >= options.max_per_document
                {
                    available[index] = false;
                    truncated = true;
                    continue;
                }
                let score = if selected.is_empty() {
                    relevance[index]
                } else {
                    (1.0 - options.diversity) * relevance[index]
                        - options.diversity * redundancy[index]
                };
                if best.is_none_or(|(previous, previous_score)| {
                    score.total_cmp(&previous_score).is_gt()
                        || (score.total_cmp(&previous_score).is_eq()
                            && candidate.hit.chunk_id < self.candidates[previous].hit.chunk_id)
                }) {
                    best = Some((index, score));
                }
            }
            let Some((index, score)) = best else {
                break;
            };
            available[index] = false;
            selected.push((index, score));
            let chosen = &self.candidates[index];
            context_bytes += chosen.hit.text.len();
            *counts.entry(chosen.hit.document_id.as_str()).or_default() += 1;
            if options.diversity > 0.0 {
                for (other, candidate) in self.candidates.iter().enumerate() {
                    if !available[other] {
                        continue;
                    }
                    let overlap = source_overlap(&chosen.hit, &candidate.hit);
                    if normalized_texts[index] == normalized_texts[other] || overlap >= 0.8 {
                        available[other] = false;
                        truncated = true;
                        continue;
                    }
                    let cosine = (1.0
                        - f64::from(chosen.vector.cosine_distance(&candidate.vector)?))
                    .clamp(0.0, 1.0);
                    redundancy[other] = redundancy[other].max(cosine.max(overlap));
                }
            }
        }
        truncated |= selected.len() < candidate_count;
        let selected_ids = selected
            .iter()
            .map(|(index, _)| self.candidates[*index].hit.chunk_id.as_str())
            .collect::<HashSet<_>>();
        let edges = self
            .edges
            .into_iter()
            .filter(|edge| {
                selected_ids.contains(edge.from_chunk.as_str())
                    && selected_ids.contains(edge.to_chunk.as_str())
            })
            .collect();
        drop(counts);
        let mut candidates = self.candidates.into_iter().map(Some).collect::<Vec<_>>();
        let hits = selected
            .into_iter()
            .map(|(index, selection_score)| {
                let candidate = candidates[index]
                    .take()
                    .expect("selected candidate occurs once");
                GraphRagHit {
                    hit: candidate.hit,
                    lexical_score: candidate.lexical_score,
                    fusion_score: candidate.fusion_score,
                    rerank_score: rerank_scores.map(|scores| scores[index]),
                    selection_score,
                    retrieval_path: candidate.retrieval_path,
                }
            })
            .collect();
        Ok(GraphRagResult {
            collection: self.collection,
            revision: self.revision,
            hits,
            edges,
            lexical_cache_hit: self.lexical_cache_hit,
            truncated,
            context_bytes,
            candidate_count,
        })
    }
}

fn source_overlap(left: &GraphHit, right: &GraphHit) -> f64 {
    if left.document_id != right.document_id {
        return 0.0;
    }
    let overlap = left
        .end_byte
        .min(right.end_byte)
        .saturating_sub(left.start_byte.max(right.start_byte));
    let smaller = left
        .end_byte
        .saturating_sub(left.start_byte)
        .min(right.end_byte.saturating_sub(right.start_byte));
    if smaller == 0 {
        0.0
    } else {
        overlap as f64 / smaller as f64
    }
}
