//! SQL-visible chunk graphs with atomic document replacement and exact vector seeds.

use super::*;
use serde::Serialize;
use serde_json::Value as JsonValue;

#[path = "graph_neighborhood.rs"]
mod neighborhood;
pub use neighborhood::{
    GraphNeighborhoodDirection, GraphNeighborhoodNode, GraphNeighborhoodRequest,
    GraphNeighborhoodResult,
};

#[path = "graph_rag.rs"]
mod rag;
pub use rag::{
    GraphBrowseRequest, GraphBrowseResult, GraphNode, GraphRagCandidate, GraphRagHit, GraphRagPath,
    GraphRagRequest, GraphRagResult, GraphRagSelection, GraphRagSnapshot, GraphRagTraversal,
    GraphRelationshipDeleteRequest, GraphRelationshipDeleteResult, GraphRelationshipRequest,
    GraphRelationshipResult,
};

const MAX_CHUNKS: usize = 10_000;
const MAX_DOCUMENT_CHUNKS: usize = 256;
const MAX_EDGES: usize = MAX_CHUNKS * 34;
const MAX_DOCUMENT_BYTES: usize = 1024 * 1024;
const MAX_VECTOR_ELEMENTS: usize = 32 * 1024 * 1024;
const MAX_COLLECTION_TEXT_BYTES: usize = 64 * 1024 * 1024;
const DOCUMENT_BASE_COLUMNS: usize = 7;
const MAX_DOCUMENT_COLUMNS: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GraphEmbeddingProfile {
    pub provider: String,
    pub model: String,
    pub dimensions: usize,
    pub context_format_version: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GraphCollectionConfig {
    pub name: String,
    pub profile: GraphEmbeddingProfile,
    pub semantic_neighbors: usize,
    pub semantic_threshold: f64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GraphTables {
    pub config: String,
    pub documents: String,
    pub chunks: String,
    pub edges: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GraphCollection {
    pub config: GraphCollectionConfig,
    pub revision: u64,
    pub document_count: usize,
    pub chunk_count: usize,
    pub edge_count: usize,
    pub tables: GraphTables,
    pub document_columns: Vec<GraphDocumentColumn>,
}

/// One canonical SQL-backed field supplied through document metadata.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GraphDocumentColumn {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub unique: bool,
}

#[derive(Clone, Debug)]
pub struct GraphChunkInput {
    pub start_byte: usize,
    pub end_byte: usize,
    pub text: String,
    pub embedding_text: String,
    pub embedding: Vector,
}

#[derive(Clone, Debug)]
pub struct GraphDocumentInput {
    pub id: String,
    pub title: String,
    pub source: String,
    pub text: String,
    pub metadata: JsonValue,
    pub chunking: JsonValue,
    pub chunks: Vec<GraphChunkInput>,
}

/// Borrowed, unembedded chunk input used for validation before a provider call.
#[derive(Clone, Debug)]
pub struct GraphChunkPreview<'a> {
    pub start_byte: usize,
    pub end_byte: usize,
    pub text: &'a str,
    pub embedding_text: &'a str,
}

/// Validate document structure and conservatively reserve graph storage without
/// embedding or allocating placeholder vectors.
#[derive(Clone, Debug)]
pub struct GraphDocumentPreview<'a> {
    pub id: &'a str,
    pub title: &'a str,
    pub source: &'a str,
    pub text: &'a str,
    pub metadata: &'a JsonValue,
    pub chunking: &'a JsonValue,
    pub chunks: Vec<GraphChunkPreview<'a>>,
}

impl<'a> From<&'a GraphDocumentInput> for GraphDocumentPreview<'a> {
    fn from(document: &'a GraphDocumentInput) -> Self {
        Self {
            id: &document.id,
            title: &document.title,
            source: &document.source,
            text: &document.text,
            metadata: &document.metadata,
            chunking: &document.chunking,
            chunks: document
                .chunks
                .iter()
                .map(|chunk| GraphChunkPreview {
                    start_byte: chunk.start_byte,
                    end_byte: chunk.end_byte,
                    text: &chunk.text,
                    embedding_text: &chunk.embedding_text,
                })
                .collect(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GraphDocument {
    pub id: String,
    pub title: String,
    pub source: String,
    pub text: String,
    pub metadata: JsonValue,
    pub chunking: JsonValue,
    pub chunk_count: usize,
    /// Detects direct SQL edits to generated chunks; not an authenticity proof.
    pub chunks_intact: bool,
}

#[derive(Clone, Debug)]
pub struct GraphIngestRequest {
    pub collection: String,
    pub expected_revision: u64,
    pub expected_profile: GraphEmbeddingProfile,
    pub document: GraphDocumentInput,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GraphIngestResult {
    pub collection: String,
    pub document_id: String,
    pub revision: u64,
    pub chunks: usize,
    pub edges_created: usize,
    pub replaced: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GraphDeleteResult {
    pub document_id: String,
    pub revision: u64,
    pub chunks_removed: usize,
    pub edges_removed: usize,
}

#[derive(Clone, Debug)]
pub struct GraphSearchRequest {
    pub collection: String,
    pub expected_profile: GraphEmbeddingProfile,
    pub query: Vector,
    pub seed_limit: usize,
    pub max_hops: usize,
    pub neighbor_limit: usize,
    pub max_results: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GraphEdge {
    pub from_chunk: String,
    pub to_chunk: String,
    pub kind: String,
    pub weight: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GraphHit {
    pub chunk_id: String,
    pub document_id: String,
    pub title: String,
    pub source: String,
    pub text: String,
    pub metadata: JsonValue,
    pub start_byte: usize,
    pub end_byte: usize,
    pub similarity: f64,
    pub depth: usize,
    pub seed: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GraphSearchResult {
    pub collection: String,
    pub revision: u64,
    pub hits: Vec<GraphHit>,
    pub edges: Vec<GraphEdge>,
    pub truncated: bool,
}

impl Database {
    /// Create four ordinary SQL tables with an immutable embedding profile.
    pub fn graph_create_collection(
        &self,
        config: GraphCollectionConfig,
    ) -> Result<GraphCollection> {
        self.graph_create_collection_with_columns(config, Vec::new())
    }

    /// Append at most 32 indexed scalar columns to the existing document table.
    /// Their canonical values are supplied and returned through metadata.
    pub fn graph_create_collection_with_columns(
        &self,
        mut config: GraphCollectionConfig,
        document_columns: Vec<Column>,
    ) -> Result<GraphCollection> {
        config.name = collection_name(&config.name)?;
        validate_config(&config)?;
        validate_document_columns(&document_columns)?;
        let (mut info, revision) = self.graph_transaction(None, |catalog, wal| {
            let tables = table_names(&config.name);
            for (name, columns, indexes) in
                schemas_with_columns(&tables, config.profile.dimensions, &document_columns)
            {
                if catalog.tables.contains_key(&name) {
                    return Err(Error::TableAlreadyExists(name));
                }
                if let Some(wal) = wal {
                    let definitions = columns
                        .iter()
                        .map(|column| {
                            format!(
                                "{} {}{}{}",
                                quote(&column.name),
                                column.data_type,
                                if column.nullable { "" } else { " NOT NULL" },
                                if column.unique { " UNIQUE" } else { "" }
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(",");
                    wal.push_str(&format!("CREATE TABLE {} ({definitions});", quote(&name)));
                    for column in &indexes {
                        wal.push_str(&format!(
                            "CREATE INDEX {} ON {} USING HASH ({});",
                            quote(&index_name(&name, &columns[*column].name)),
                            quote(&name),
                            quote(&columns[*column].name)
                        ));
                    }
                }
                let hash_indexes = indexes
                    .into_iter()
                    .map(|column| {
                        (
                            index_name(&name, &columns[column].name),
                            HashIndex {
                                column,
                                buckets: HashMap::new(),
                            },
                        )
                    })
                    .collect();
                let mut table = Table::new(columns, Vec::new(), hash_indexes);
                rebuild_indexes(&mut table);
                catalog.tables.insert(name, table);
            }
            insert(
                catalog,
                &tables.config,
                vec![vec![
                    Value::Integer(1),
                    Value::Text(config.profile.provider.clone()),
                    Value::Text(config.profile.model.clone()),
                    Value::Integer(config.profile.dimensions as i64),
                    Value::Integer(i64::from(config.profile.context_format_version)),
                    Value::Integer(config.semantic_neighbors as i64),
                    Value::Float(config.semantic_threshold),
                ]],
                wal,
            )?;
            collection(catalog, &config.name)
        })?;
        info.revision = revision;
        Ok(info)
    }

    pub fn graph_collection(&self, name: &str) -> Result<GraphCollection> {
        let catalog = self.catalog.read().map_err(|_| Error::LockPoisoned)?;
        collection(&catalog, name)
    }

    pub fn graph_collections(&self) -> Result<Vec<GraphCollection>> {
        let catalog = self.catalog.read().map_err(|_| Error::LockPoisoned)?;
        let mut names = catalog
            .tables
            .keys()
            .filter_map(|name| name.strip_prefix("graph_")?.strip_suffix("_config"))
            .collect::<Vec<_>>();
        names.sort_unstable();
        names
            .into_iter()
            .map(|name| collection(&catalog, name))
            .collect()
    }

    /// Inspect stored source and chunking settings for idempotency before embedding.
    pub fn graph_document(&self, name: &str, document_id: &str) -> Result<Option<GraphDocument>> {
        let catalog = self.catalog.read().map_err(|_| Error::LockPoisoned)?;
        let info = collection(&catalog, name)?;
        read_document(&catalog, &info.tables, document_id)
    }

    /// Check replacement capacity before paying for embeddings. Text/edge space
    /// reserves the maximum configured semantic fanout, even when the actual
    /// similarity threshold later produces fewer edges. Commit rechecks it.
    pub fn graph_check_ingest_capacity(
        &self,
        name: &str,
        document: &GraphDocumentPreview<'_>,
    ) -> Result<u64> {
        validate_preview(document)?;
        let catalog = self.catalog.read().map_err(|_| Error::LockPoisoned)?;
        let info = collection(&catalog, name)?;
        check_ingest_capacity(&catalog, &info, document)?;
        Ok(catalog.revision)
    }

    /// Replace metadata without changing chunks, vectors, or relationships.
    /// This uses the same revision guard and durable transaction as ingestion.
    pub fn graph_update_document_metadata(
        &self,
        collection_name: &str,
        id: &str,
        metadata: JsonValue,
        expected_revision: u64,
    ) -> Result<GraphIngestResult> {
        bounded_text(id, 512, false, "document id")?;
        validate_metadata(&metadata)?;
        let (mut result, revision) =
            self.graph_transaction(Some(expected_revision), |catalog, wal| {
                let info = collection(catalog, collection_name)?;
                let document = read_document(catalog, &info.tables, id)?
                    .ok_or_else(|| invalid("graph document does not exist"))?;
                if !document.chunks_intact {
                    return Err(invalid(
                        "stored document chunks changed; reingest before updating metadata",
                    ));
                }
                let documents = table(catalog, &info.tables.documents)?;
                let (free, fields) = document_fields(documents, id, &metadata)?;
                let position = documents
                    .rows
                    .iter()
                    .position(|row| matches!(row.first(), Some(Value::Text(value)) if value == id))
                    .ok_or_else(|| invalid("graph document does not exist"))?;
                let mut row = documents.rows[position].clone();
                row[4] = Value::Text(free.to_string());
                row.truncate(DOCUMENT_BASE_COLUMNS);
                row.extend(fields);
                let mut chunks = table(catalog, &info.tables.chunks)?
                    .rows
                    .iter()
                    .filter(|chunk| matches!(chunk.get(1), Some(Value::Text(value)) if value == id))
                    .collect::<Vec<_>>();
                chunks.sort_by_key(|chunk| match chunk.get(2) {
                    Some(Value::Integer(ordinal)) => *ordinal,
                    _ => -1,
                });
                row[6] = Value::Text(chunk_fingerprint(&row[..6], &chunks));
                if let Some(wal) = wal {
                    let assignments = std::iter::once(4)
                        .chain(std::iter::once(6))
                        .chain(DOCUMENT_BASE_COLUMNS..row.len())
                        .map(|index| {
                            format!(
                                "{}={}",
                                quote(&documents.columns[index].name),
                                sql_value(&row[index])
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(",");
                    wal.push_str(&format!(
                        "UPDATE {} SET {assignments} WHERE document_id={};",
                        quote(&info.tables.documents),
                        literal(id)
                    ));
                }
                let documents = catalog
                    .tables
                    .get_mut(&info.tables.documents)
                    .ok_or_else(|| Error::TableNotFound(info.tables.documents.clone()))?;
                validate_row(&documents.columns, &row)?;
                documents.rows[position] = row;
                rebuild_relational_indexes(documents);
                validate_text_capacity(catalog, &info.tables)?;
                Ok(GraphIngestResult {
                    collection: info.config.name,
                    document_id: id.into(),
                    revision: 0,
                    chunks: document.chunk_count,
                    edges_created: 0,
                    replaced: true,
                })
            })?;
        result.revision = revision;
        Ok(result)
    }

    /// Replace one document and every incident edge in a single durable commit.
    /// Provider work must finish before this call; a stale revision fails before
    /// graph generation or mutation. Similarity edges express cosine proximity,
    /// not extracted or verified factual relationships.
    pub fn graph_ingest_document(&self, request: GraphIngestRequest) -> Result<GraphIngestResult> {
        validate_document(&request.document, &request.expected_profile)?;
        let document_id = request.document.id.clone();
        let (mut result, revision) =
            self.graph_transaction(Some(request.expected_revision), |catalog, wal| {
                let info = collection(catalog, &request.collection)?;
                check_profile(&info.config.profile, &request.expected_profile)?;
                check_ingest_capacity(
                    catalog,
                    &info,
                    &GraphDocumentPreview::from(&request.document),
                )?;
                let replaced = read_document(catalog, &info.tables, &document_id)?.is_some();
                remove_document(catalog, &info.tables, &document_id, wal)?;
                if table(catalog, &info.tables.chunks)?
                    .rows
                    .len()
                    .saturating_add(request.document.chunks.len())
                    > MAX_CHUNKS
                {
                    return Err(invalid(
                        "a graph collection can contain at most 10000 chunks",
                    ));
                }
                if table(catalog, &info.tables.chunks)?
                    .rows
                    .len()
                    .saturating_add(request.document.chunks.len())
                    .saturating_mul(info.config.profile.dimensions)
                    > MAX_VECTOR_ELEMENTS
                {
                    return Err(invalid("graph vector storage exceeds 33554432 elements"));
                }
                let document = &request.document;
                let profile_key = profile_key(&info.config.profile)?;
                let rows: Vec<Vec<Value>> = document
                    .chunks
                    .iter()
                    .enumerate()
                    .map(|(ordinal, chunk)| {
                        vec![
                            Value::Text(chunk_id(&document.id, ordinal)),
                            Value::Text(document.id.clone()),
                            Value::Integer(ordinal as i64),
                            Value::Integer(chunk.start_byte as i64),
                            Value::Integer(chunk.end_byte as i64),
                            Value::Text(chunk.text.clone()),
                            Value::Text(chunk.embedding_text.clone()),
                            Value::Text(profile_key.clone()),
                            Value::Vector(chunk.embedding.clone()),
                        ]
                    })
                    .collect();
                let (metadata, field_values) = document_fields(
                    table(catalog, &info.tables.documents)?,
                    &document.id,
                    &document.metadata,
                )?;
                let mut document_row = vec![
                    Value::Text(document.id.clone()),
                    Value::Text(document.title.clone()),
                    Value::Text(document.source.clone()),
                    Value::Text(document.text.clone()),
                    Value::Text(metadata.to_string()),
                    Value::Text(document.chunking.to_string()),
                ];
                let fingerprint =
                    chunk_fingerprint(&document_row, &rows.iter().collect::<Vec<_>>());
                document_row.push(Value::Text(fingerprint));
                document_row.extend(field_values);
                insert(catalog, &info.tables.documents, vec![document_row], wal)?;
                let mut edges = Vec::new();
                for ordinal in 1..document.chunks.len() {
                    let previous = chunk_id(&document.id, ordinal - 1);
                    let current = chunk_id(&document.id, ordinal);
                    edges.push(edge(previous.clone(), current.clone(), "adjacent", 1.0));
                    edges.push(edge(current, previous, "adjacent", 1.0));
                }
                // At most one exact bounded search per new chunk, never an all-pairs
                // rebuild of the existing graph. Search before inserting the
                // replacement chunks: the old document is already absent, so
                // the dense/GPU path needs no residual exclusion predicate.
                if info.config.semantic_neighbors > 0 {
                    let chunks = table(catalog, &info.tables.chunks)?;
                    for (ordinal, chunk) in document.chunks.iter().enumerate() {
                        let neighbors = run_typed_vector_search(
                            chunks,
                            VectorSearch {
                                table: info.tables.chunks.clone(),
                                vector_column: "embedding".into(),
                                query: chunk.embedding.clone(),
                                metric: VectorSearchMetric::Cosine,
                                select: vec!["chunk_id".into()],
                                filters: Vec::new(),
                                limit: info.config.semantic_neighbors,
                            },
                            &self.compute,
                        )?;
                        for row in neighbors.rows {
                            let similarity = (1.0 - number_at(&row, 1)?).clamp(-1.0, 1.0);
                            if similarity < info.config.semantic_threshold {
                                continue;
                            }
                            let from = chunk_id(&document.id, ordinal);
                            let to = text_at(&row, 0)?.to_owned();
                            edges.push(edge(from.clone(), to.clone(), "semantic", similarity));
                            edges.push(edge(to, from, "semantic", similarity));
                        }
                    }
                }
                insert(catalog, &info.tables.chunks, rows, wal)?;
                let count = edges.len();
                if table(catalog, &info.tables.edges)?
                    .rows
                    .len()
                    .saturating_add(count)
                    > MAX_EDGES
                {
                    return Err(invalid("graph edge capacity exceeded"));
                }
                insert(
                    catalog,
                    &info.tables.edges,
                    edges.iter().map(edge_row).collect(),
                    wal,
                )?;
                validate_text_capacity(catalog, &info.tables)?;
                Ok(GraphIngestResult {
                    collection: info.config.name,
                    document_id: document_id.clone(),
                    revision: 0,
                    chunks: document.chunks.len(),
                    edges_created: count,
                    replaced,
                })
            })?;
        result.revision = revision;
        Ok(result)
    }

    pub fn graph_delete_document(
        &self,
        name: &str,
        document_id: &str,
        expected_revision: u64,
    ) -> Result<GraphDeleteResult> {
        bounded_text(document_id, 512, false, "document id")?;
        let (mut result, revision) =
            self.graph_transaction(Some(expected_revision), |catalog, wal| {
                let info = collection(catalog, name)?;
                if read_document(catalog, &info.tables, document_id)?.is_none() {
                    return Err(invalid("graph document does not exist"));
                }
                let (chunks_removed, edges_removed) =
                    remove_document(catalog, &info.tables, document_id, wal)?;
                Ok(GraphDeleteResult {
                    document_id: document_id.into(),
                    revision: 0,
                    chunks_removed,
                    edges_removed,
                })
            })?;
        result.revision = revision;
        Ok(result)
    }

    /// Rank exact vector seeds, then expand a bounded breadth-first neighborhood.
    /// Seeds remain first; graph context has explicit depth and its own actual
    /// query cosine similarity. Schema, ranking and citations share one read lock.
    pub fn graph_search(&self, request: GraphSearchRequest) -> Result<GraphSearchResult> {
        if !(1..=20).contains(&request.seed_limit)
            || request.max_hops > 3
            || !(1..=32).contains(&request.neighbor_limit)
            || !(1..=100).contains(&request.max_results)
            || request.seed_limit > request.max_results
        {
            return Err(invalid("graph search requires seed_limit 1..20, max_hops 0..3, neighbor_limit 1..32, max_results 1..100, and seeds <= results"));
        }
        let catalog = self.catalog.read().map_err(|_| Error::LockPoisoned)?;
        let info = collection(&catalog, &request.collection)?;
        check_profile(&info.config.profile, &request.expected_profile)?;
        if request.query.norm() == 0.0 {
            return Err(Error::ZeroNorm);
        }
        let chunks = table(&catalog, &info.tables.chunks)?;
        let seeds = run_typed_vector_search(
            chunks,
            VectorSearch {
                table: info.tables.chunks.clone(),
                vector_column: "embedding".into(),
                query: request.query.clone(),
                metric: VectorSearchMetric::Cosine,
                select: vec!["chunk_id".into()],
                filters: Vec::new(),
                limit: request.seed_limit,
            },
            &self.compute,
        )?;
        let chunk_lookup = chunks
            .rows
            .iter()
            .enumerate()
            .map(|(index, row)| text_at(row, 0).map(|id| (id, index)))
            .collect::<Result<HashMap<_, _>>>()?;
        let documents = table(&catalog, &info.tables.documents)?;
        let document_lookup = documents
            .rows
            .iter()
            .map(|row| text_at(row, 0).map(|id| (id, row)))
            .collect::<Result<HashMap<_, _>>>()?;
        let edge_table = table(&catalog, &info.tables.edges)?;
        let edge_index = edge_table
            .indexes
            .values()
            .find(|index| index.column == 1)
            .ok_or_else(|| invalid("graph edge source index is missing"))?;
        let mut result = GraphSearchResult {
            collection: info.config.name,
            revision: catalog.revision,
            hits: Vec::new(),
            edges: Vec::new(),
            truncated: false,
        };
        let mut visited = HashSet::new();
        let mut pending = VecDeque::new();
        for row in &seeds.rows {
            let id = text_at(row, 0)?.to_owned();
            visited.insert(id.clone());
            pending.push_back((id, 0));
        }
        while let Some((id, depth)) = pending.pop_front() {
            let chunk = &chunks.rows[*chunk_lookup
                .get(id.as_str())
                .ok_or_else(|| invalid("graph edge references a missing chunk"))?];
            result.hits.push(hit(
                chunk,
                &document_lookup,
                &documents.columns,
                &request.query,
                depth,
            )?);
            if depth >= request.max_hops {
                continue;
            }
            let mut neighbors = edge_index
                .buckets
                .get(&UniqueKey::from(&Value::Text(id)))
                .into_iter()
                .flatten()
                .map(|index| read_edge(&edge_table.rows[*index]))
                .collect::<Result<Vec<_>>>()?;
            neighbors.sort_by(|left, right| {
                (left.kind != "adjacent")
                    .cmp(&(right.kind != "adjacent"))
                    .then_with(|| right.weight.total_cmp(&left.weight))
                    .then_with(|| left.to_chunk.cmp(&right.to_chunk))
            });
            let mut admitted = 0;
            for neighbor in neighbors {
                if !chunk_lookup.contains_key(neighbor.to_chunk.as_str()) {
                    return Err(invalid("graph edge references a missing chunk"));
                }
                if visited.contains(&neighbor.to_chunk) {
                    continue;
                }
                if admitted == request.neighbor_limit || visited.len() == request.max_results {
                    result.truncated = true;
                    continue;
                }
                admitted += 1;
                visited.insert(neighbor.to_chunk.clone());
                pending.push_back((neighbor.to_chunk.clone(), depth + 1));
                result.edges.push(neighbor);
            }
        }
        // Return relationships among all selected chunks, including seed-to-
        // seed links and cycles. Keep discovery links first so the bounded
        // induced graph still explains how expanded context was reached.
        let discovery_edges = std::mem::take(&mut result.edges);
        let discovery = discovery_edges
            .iter()
            .map(|edge| {
                (
                    edge.from_chunk.as_str(),
                    edge.to_chunk.as_str(),
                    edge.kind.as_str(),
                )
            })
            .collect::<HashSet<_>>();
        let mut returned = HashSet::new();
        for hit in &result.hits {
            let mut edges = edge_index
                .buckets
                .get(&UniqueKey::from(&Value::Text(hit.chunk_id.clone())))
                .into_iter()
                .flatten()
                .map(|index| read_edge(&edge_table.rows[*index]))
                .collect::<Result<Vec<_>>>()?;
            if edges
                .iter()
                .any(|edge| !chunk_lookup.contains_key(edge.to_chunk.as_str()))
            {
                return Err(invalid("graph edge references a missing chunk"));
            }
            edges.retain(|edge| visited.contains(&edge.to_chunk));
            edges.sort_by(|left, right| {
                let left_key = (
                    left.from_chunk.as_str(),
                    left.to_chunk.as_str(),
                    left.kind.as_str(),
                );
                let right_key = (
                    right.from_chunk.as_str(),
                    right.to_chunk.as_str(),
                    right.kind.as_str(),
                );
                (!discovery.contains(&left_key))
                    .cmp(&(!discovery.contains(&right_key)))
                    .then_with(|| (left.kind != "adjacent").cmp(&(right.kind != "adjacent")))
                    .then_with(|| right.weight.total_cmp(&left.weight))
                    .then_with(|| left.to_chunk.cmp(&right.to_chunk))
                    .then_with(|| left.kind.cmp(&right.kind))
            });
            let mut count = 0;
            for edge in edges {
                let key = (
                    edge.from_chunk.clone(),
                    edge.to_chunk.clone(),
                    edge.kind.clone(),
                );
                if !returned.insert(key) {
                    continue;
                }
                if count == request.neighbor_limit {
                    result.truncated = true;
                    continue;
                }
                count += 1;
                result.edges.push(edge);
            }
        }
        Ok(result)
    }

    fn graph_transaction<T>(
        &self,
        expected_revision: Option<u64>,
        mutate: impl FnOnce(&mut Catalog, &mut Option<String>) -> Result<T>,
    ) -> Result<(T, u64)> {
        let mut catalog = self.catalog.write().map_err(|_| Error::LockPoisoned)?;
        if let Some(expected) = expected_revision {
            if catalog.revision != expected {
                return Err(Error::RevisionConflict {
                    expected,
                    actual: catalog.revision,
                });
            }
        }
        let mut staged = catalog.clone();
        let mut wal = self.persistent.as_ref().map(|_| String::new());
        let result = mutate(&mut staged, &mut wal)?;
        let checkpoint_needed = if let (Some(persistent), Some(sql)) = (&self.persistent, wal) {
            let sequence = next_durable_sequence(catalog.durable_sequence)?;
            let checkpoint = persistent.append(sequence, PersistentStorage::prepare_sql(&sql)?)?;
            staged.durable_sequence = sequence;
            staged.revision = sequence;
            checkpoint
        } else {
            staged.mark_changed();
            false
        };
        let revision = staged.revision;
        *catalog = staged;
        drop(catalog);
        if checkpoint_needed {
            let _ = self.checkpoint();
        }
        Ok((result, revision))
    }
}

fn invalid(message: &str) -> Error {
    Error::InvalidQuery(message.into())
}
fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}
fn literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}
fn bounded_text(value: &str, max: usize, empty: bool, label: &str) -> Result<()> {
    if value.len() > max || (!empty && value.trim().is_empty()) || value.contains('\0') {
        return Err(invalid(&format!(
            "{label} is empty, contains NUL, or exceeds {max} UTF-8 bytes"
        )));
    }
    Ok(())
}
fn collection_name(name: &str) -> Result<String> {
    let name = name.to_ascii_lowercase();
    if name.is_empty()
        || name.len() > 48
        || !name.as_bytes()[0].is_ascii_lowercase()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(invalid("collection name must start with an ASCII letter and contain at most 48 ASCII letters, digits, or underscores"));
    }
    Ok(name)
}
fn validate_config(config: &GraphCollectionConfig) -> Result<()> {
    bounded_text(&config.profile.provider, 64, false, "embedding provider")?;
    bounded_text(&config.profile.model, 128, false, "embedding model")?;
    if config.profile.dimensions == 0
        || config.profile.dimensions > MAX_VECTOR_DIMENSIONS
        || config.profile.context_format_version != 1
    {
        return Err(invalid(
            "graph embedding dimensions or context format version are invalid",
        ));
    }
    if config.semantic_neighbors > 16
        || !config.semantic_threshold.is_finite()
        || !(0.0..=1.0).contains(&config.semantic_threshold)
    {
        return Err(invalid(
            "semantic_neighbors must be 0..16 and semantic_threshold must be finite in 0..1",
        ));
    }
    Ok(())
}
fn profile_key(profile: &GraphEmbeddingProfile) -> Result<String> {
    serde_json::to_string(profile).map_err(|_| invalid("invalid embedding profile"))
}
fn check_profile(actual: &GraphEmbeddingProfile, expected: &GraphEmbeddingProfile) -> Result<()> {
    if actual != expected {
        return Err(invalid("graph embedding profile changed; use the collection's provider, model, dimensions, and context format"));
    }
    Ok(())
}
fn table_names(name: &str) -> GraphTables {
    GraphTables {
        config: format!("graph_{name}_config"),
        documents: format!("graph_{name}_documents"),
        chunks: format!("graph_{name}_chunks"),
        edges: format!("graph_{name}_edges"),
    }
}
fn index_name(table: &str, column: &str) -> String {
    format!("{table}_{column}_idx")
}
fn columns(fields: &[(&str, DataType, bool)]) -> Vec<Column> {
    fields
        .iter()
        .map(|(name, data_type, unique)| Column {
            name: (*name).into(),
            data_type: data_type.clone(),
            nullable: false,
            unique: *unique,
        })
        .collect()
}
fn schemas(names: &GraphTables, dimensions: usize) -> Vec<(String, Vec<Column>, Vec<usize>)> {
    use DataType::{Float as F, Integer as I, Text as T};
    vec![
        (
            names.config.clone(),
            columns(&[
                ("id", I, true),
                ("provider", T, false),
                ("model", T, false),
                ("dimensions", I, false),
                ("context_format_version", I, false),
                ("semantic_neighbors", I, false),
                ("semantic_threshold", F, false),
            ]),
            vec![],
        ),
        (
            names.documents.clone(),
            columns(&[
                ("document_id", T, true),
                ("title", T, false),
                ("source", T, false),
                ("text", T, false),
                ("metadata", T, false),
                ("chunking", T, false),
                ("chunk_fingerprint", T, false),
            ]),
            vec![0],
        ),
        (
            names.chunks.clone(),
            columns(&[
                ("chunk_id", T, true),
                ("document_id", T, false),
                ("ordinal", I, false),
                ("start_byte", I, false),
                ("end_byte", I, false),
                ("text", T, false),
                ("embedding_text", T, false),
                ("embedding_profile", T, false),
                ("embedding", DataType::Vector(dimensions), false),
            ]),
            vec![0, 1],
        ),
        (
            names.edges.clone(),
            columns(&[
                ("edge_id", T, true),
                ("from_chunk", T, false),
                ("to_chunk", T, false),
                ("kind", T, false),
                ("weight", F, false),
            ]),
            vec![1, 2],
        ),
    ]
}
fn schemas_with_columns(
    names: &GraphTables,
    dimensions: usize,
    document_columns: &[Column],
) -> Vec<(String, Vec<Column>, Vec<usize>)> {
    let mut schema = schemas(names, dimensions);
    schema[1]
        .2
        .extend(DOCUMENT_BASE_COLUMNS..DOCUMENT_BASE_COLUMNS + document_columns.len());
    schema[1].1.extend_from_slice(document_columns);
    schema
}

fn validate_document_columns(columns: &[Column]) -> Result<()> {
    if columns.len() > MAX_DOCUMENT_COLUMNS {
        return Err(invalid(
            "a graph collection supports at most 32 custom document columns",
        ));
    }
    let mut names = HashSet::from([
        "document_id",
        "title",
        "source",
        "text",
        "metadata",
        "chunking",
        "chunk_fingerprint",
    ]);
    for column in columns {
        if column.name.is_empty()
            || column.name.len() > 48
            || !column.name.as_bytes()[0].is_ascii_lowercase()
            || !column
                .name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(invalid("document column names must start with a lowercase ASCII letter and contain at most 48 lowercase ASCII letters, digits, or underscores"));
        }
        if !names.insert(&column.name) {
            return Err(Error::DuplicateColumn(column.name.clone()));
        }
        if matches!(column.data_type, DataType::Vector(_)) {
            return Err(invalid(
                "custom document columns support TEXT, INTEGER, DOUBLE, and BOOLEAN",
            ));
        }
    }
    Ok(())
}

fn validate_metadata(metadata: &JsonValue) -> Result<()> {
    if !metadata.is_object() || metadata.to_string().len() > 65536 {
        return Err(invalid(
            "metadata must be a JSON object of at most 65536 bytes",
        ));
    }
    Ok(())
}

// Strip declared keys from the free-form JSON. SQL columns are the sole stored
// source for these values; nullable missing fields deliberately become NULL.
fn document_fields(
    table: &Table,
    id: &str,
    metadata: &JsonValue,
) -> Result<(JsonValue, Vec<Value>)> {
    validate_metadata(metadata)?;
    let mut free = metadata.as_object().unwrap().clone();
    let mut values = Vec::with_capacity(table.columns.len().saturating_sub(DOCUMENT_BASE_COLUMNS));
    for (index, column) in table.columns.iter().enumerate().skip(DOCUMENT_BASE_COLUMNS) {
        let input = free.remove(&column.name).unwrap_or(JsonValue::Null);
        let value = if input.is_null() {
            if !column.nullable {
                return Err(Error::NullViolation(column.name.clone()));
            }
            Value::Null
        } else {
            match (&column.data_type, &input) {
                (DataType::Text, JsonValue::String(value)) if !value.contains('\0') => {
                    Value::Text(value.clone())
                }
                (DataType::Integer, JsonValue::Number(value)) if value.as_i64().is_some() => {
                    Value::Integer(value.as_i64().unwrap())
                }
                (DataType::Float, JsonValue::Number(value))
                    if value.as_f64().is_some_and(f64::is_finite) =>
                {
                    Value::Float(value.as_f64().unwrap())
                }
                (DataType::Boolean, JsonValue::Bool(value)) => Value::Boolean(*value),
                _ => {
                    return Err(invalid(&format!(
                        "metadata field '{}' must match {}",
                        column.name, column.data_type
                    )))
                }
            }
        };
        if column.unique && !matches!(value, Value::Null) {
            let key = UniqueKey::from(&value);
            let existing = table
                .unique_keys
                .get(&index)
                .and_then(|keys| keys.get(&key));
            if existing.is_some_and(|row| !matches!(table.rows[*row].first(), Some(Value::Text(existing_id)) if existing_id == id)) {
                return Err(Error::UniqueViolation(column.name.clone()));
            }
        }
        values.push(value);
    }
    Ok((JsonValue::Object(free), values))
}

fn document_metadata(row: &[Value], columns: &[Column]) -> Result<JsonValue> {
    let mut metadata = json_at(row, 4)?;
    let object = metadata.as_object_mut().unwrap();
    for (index, column) in columns.iter().enumerate().skip(DOCUMENT_BASE_COLUMNS) {
        let value = match (row.get(index), &column.data_type) {
            (Some(Value::Null), _) if column.nullable => JsonValue::Null,
            (Some(Value::Text(value)), DataType::Text) => JsonValue::String(value.clone()),
            (Some(Value::Integer(value)), DataType::Integer) => JsonValue::from(*value),
            (Some(Value::Float(value)), DataType::Float) if value.is_finite() => {
                JsonValue::from(*value)
            }
            (Some(Value::Boolean(value)), DataType::Boolean) => JsonValue::from(*value),
            _ => return Err(invalid("graph document contains an invalid custom field")),
        };
        object.insert(column.name.clone(), value);
    }
    Ok(metadata)
}

fn table<'a>(catalog: &'a Catalog, name: &str) -> Result<&'a Table> {
    catalog
        .tables
        .get(name)
        .ok_or_else(|| Error::TableNotFound(name.into()))
}

fn validate_text_capacity(catalog: &Catalog, tables: &GraphTables) -> Result<()> {
    let mut bytes = 0_usize;
    for name in [&tables.documents, &tables.chunks, &tables.edges] {
        for row in &table(catalog, name)?.rows {
            for value in row {
                if let Value::Text(value) = value {
                    bytes = bytes.saturating_add(value.len());
                    if bytes > MAX_COLLECTION_TEXT_BYTES {
                        return Err(invalid("graph text storage exceeds 64 MiB"));
                    }
                }
            }
        }
    }
    Ok(())
}

fn row_text_bytes(row: &[Value]) -> usize {
    row.iter().fold(0_usize, |bytes, value| match value {
        Value::Text(value) => bytes.saturating_add(value.len()),
        _ => bytes,
    })
}

fn check_ingest_capacity(
    catalog: &Catalog,
    info: &GraphCollection,
    document: &GraphDocumentPreview<'_>,
) -> Result<()> {
    let (metadata, fields) = document_fields(
        table(catalog, &info.tables.documents)?,
        document.id,
        document.metadata,
    )?;
    let chunks = table(catalog, &info.tables.chunks)?;
    let (replaced_ids, replaced_count) = document_chunk_ids(catalog, &info.tables, document.id)?;
    let remaining_chunks = chunks.rows.len().saturating_sub(replaced_count);
    let next_chunks = remaining_chunks.saturating_add(document.chunks.len());
    if next_chunks > MAX_CHUNKS {
        return Err(invalid(
            "a graph collection can contain at most 10000 chunks",
        ));
    }
    if next_chunks.saturating_mul(info.config.profile.dimensions) > MAX_VECTOR_ELEMENTS {
        return Err(invalid("graph vector storage exceeds 33554432 elements"));
    }
    let mut bytes = 0_usize;
    let mut remaining_edges = 0_usize;
    let mut max_existing_id = 0;
    for row in &table(catalog, &info.tables.documents)?.rows {
        if text_at(row, 0)? != document.id {
            bytes = bytes.saturating_add(row_text_bytes(row));
        }
    }
    for row in &chunks.rows {
        if !replaced_ids.contains(text_at(row, 0)?) {
            bytes = bytes.saturating_add(row_text_bytes(row));
            max_existing_id = max_existing_id.max(text_at(row, 0)?.len());
        }
    }
    for row in &table(catalog, &info.tables.edges)?.rows {
        if !replaced_ids.contains(text_at(row, 1)?) && !replaced_ids.contains(text_at(row, 2)?) {
            bytes = bytes.saturating_add(row_text_bytes(row));
            remaining_edges += 1;
        }
    }
    for value in [document.id, document.title, document.source, document.text] {
        bytes = bytes.saturating_add(value.len());
    }
    bytes = bytes
        .saturating_add(metadata.to_string().len())
        .saturating_add(row_text_bytes(&fields))
        .saturating_add(document.chunking.to_string().len())
        .saturating_add(16);
    let profile_bytes = profile_key(&info.config.profile)?.len();
    let mut max_new_id = 0;
    for (ordinal, chunk) in document.chunks.iter().enumerate() {
        let id_bytes = chunk_id(document.id, ordinal).len();
        max_new_id = max_new_id.max(id_bytes);
        bytes = bytes
            .saturating_add(id_bytes)
            .saturating_add(document.id.len())
            .saturating_add(chunk.text.len())
            .saturating_add(chunk.embedding_text.len())
            .saturating_add(profile_bytes);
    }
    let adjacency_count = document.chunks.len().saturating_sub(1).saturating_mul(2);
    let semantic_count = document
        .chunks
        .len()
        .saturating_mul(info.config.semantic_neighbors.min(remaining_chunks))
        .saturating_mul(2);
    if remaining_edges
        .saturating_add(adjacency_count)
        .saturating_add(semantic_count)
        > MAX_EDGES
    {
        return Err(invalid(
            "graph edge capacity would be exceeded by the configured fanout",
        ));
    }
    // Each edge stores its generated ID, both endpoints, and kind. The small
    // reserve also covers ID delimiters and the decimal endpoint-length field.
    let adjacent_bytes = max_new_id.saturating_mul(4).saturating_add(36);
    let semantic_bytes = max_new_id
        .saturating_add(max_existing_id)
        .saturating_mul(2)
        .saturating_add(36);
    bytes = bytes
        .saturating_add(adjacency_count.saturating_mul(adjacent_bytes))
        .saturating_add(semantic_count.saturating_mul(semantic_bytes));
    if bytes > MAX_COLLECTION_TEXT_BYTES {
        return Err(invalid(
            "graph text capacity would exceed 64 MiB with the configured fanout",
        ));
    }
    Ok(())
}
fn text_at(row: &[Value], index: usize) -> Result<&str> {
    match row.get(index) {
        Some(Value::Text(value)) => Ok(value),
        _ => Err(invalid("graph table contains an invalid text field")),
    }
}
fn usize_at(row: &[Value], index: usize) -> Result<usize> {
    match row.get(index) {
        Some(Value::Integer(value)) => usize::try_from(*value)
            .map_err(|_| invalid("graph table contains an invalid integer field")),
        _ => Err(invalid("graph table contains an invalid integer field")),
    }
}
fn number_at(row: &[Value], index: usize) -> Result<f64> {
    match row.get(index) {
        Some(Value::Float(value)) if value.is_finite() => Ok(*value),
        _ => Err(invalid("graph table contains an invalid finite score")),
    }
}
fn json_at(row: &[Value], index: usize) -> Result<JsonValue> {
    let value: JsonValue = serde_json::from_str(text_at(row, index)?)
        .map_err(|_| invalid("graph table contains invalid JSON metadata"))?;
    if !value.is_object() {
        return Err(invalid("graph metadata and chunking must be JSON objects"));
    }
    Ok(value)
}
fn collection(catalog: &Catalog, name: &str) -> Result<GraphCollection> {
    let name = collection_name(name)?;
    let tables = table_names(&name);
    let config_table = table(catalog, &tables.config)?;
    if config_table.columns != schemas(&tables, 1)[0].1 || config_table.rows.len() != 1 {
        return Err(invalid(
            "graph configuration schema or singleton record changed",
        ));
    }
    let row = &config_table.rows[0];
    if usize_at(row, 0)? != 1 {
        return Err(invalid("graph configuration id must be one"));
    }
    let config = GraphCollectionConfig {
        name,
        profile: GraphEmbeddingProfile {
            provider: text_at(row, 1)?.into(),
            model: text_at(row, 2)?.into(),
            dimensions: usize_at(row, 3)?,
            context_format_version: u32::try_from(usize_at(row, 4)?)
                .map_err(|_| invalid("invalid context version"))?,
        },
        semantic_neighbors: usize_at(row, 5)?,
        semantic_threshold: number_at(row, 6)?,
    };
    validate_config(&config)?;
    let document_table = table(catalog, &tables.documents)?;
    let document_columns = document_table
        .columns
        .get(DOCUMENT_BASE_COLUMNS..)
        .ok_or_else(|| invalid("graph document schema is missing required columns"))?;
    validate_document_columns(document_columns)?;
    for (name, columns, indexes) in
        schemas_with_columns(&tables, config.profile.dimensions, document_columns)
    {
        let table = table(catalog, &name)?;
        if table.columns != columns
            || indexes
                .iter()
                .any(|column| !table.indexes.values().any(|index| index.column == *column))
        {
            return Err(invalid(
                "graph managed table schema or required scalar index changed",
            ));
        }
    }
    let chunks = table(catalog, &tables.chunks)?;
    let key = profile_key(&config.profile)?;
    if chunks.rows.len() > MAX_CHUNKS || table(catalog, &tables.edges)?.rows.len() > MAX_EDGES {
        return Err(invalid("graph collection exceeds its supported capacity"));
    }
    for row in &chunks.rows {
        if text_at(row, 7)? != key {
            return Err(invalid(
                "stored chunks do not match the collection embedding profile",
            ));
        }
    }
    Ok(GraphCollection {
        config,
        revision: catalog.revision,
        document_count: table(catalog, &tables.documents)?.rows.len(),
        chunk_count: chunks.rows.len(),
        edge_count: table(catalog, &tables.edges)?.rows.len(),
        document_columns: document_columns
            .iter()
            .map(|column| GraphDocumentColumn {
                name: column.name.clone(),
                data_type: column.data_type.to_string(),
                nullable: column.nullable,
                unique: column.unique,
            })
            .collect(),
        tables,
    })
}
fn read_document(
    catalog: &Catalog,
    tables: &GraphTables,
    id: &str,
) -> Result<Option<GraphDocument>> {
    let document_table = table(catalog, &tables.documents)?;
    let document = document_table
        .rows
        .iter()
        .find(|row| matches!(row.first(), Some(Value::Text(value)) if value == id));
    document
        .map(|row| {
            let mut chunks = table(catalog, &tables.chunks)?
                .rows
                .iter()
                .filter(|row| matches!(row.get(1), Some(Value::Text(value)) if value == id))
                .collect::<Vec<_>>();
            chunks.sort_by_key(|row| match row.get(2) {
                Some(Value::Integer(ordinal)) => *ordinal,
                _ => -1,
            });
            Ok(GraphDocument {
                id: text_at(row, 0)?.into(),
                title: text_at(row, 1)?.into(),
                source: text_at(row, 2)?.into(),
                text: text_at(row, 3)?.into(),
                metadata: document_metadata(row, &document_table.columns)?,
                chunking: json_at(row, 5)?,
                chunk_count: chunks.len(),
                chunks_intact: !chunks.is_empty()
                    && text_at(row, 6)? == chunk_fingerprint(&row[..6], &chunks)
                    && chunks.iter().all(|chunk| {
                        let (Ok(start), Ok(end), Ok(text), Ok(source)) = (
                            usize_at(chunk, 3),
                            usize_at(chunk, 4),
                            text_at(chunk, 5),
                            text_at(row, 3),
                        ) else {
                            return false;
                        };
                        source.get(start..end) == Some(text)
                    }),
            })
        })
        .transpose()
}

// Stable FNV-1a corruption fingerprint; it deliberately makes no authenticity
// guarantee against someone who can also rewrite managed SQL metadata.
fn chunk_fingerprint(document: &[Value], rows: &[&Vec<Value>]) -> String {
    fn add(hash: &mut u64, bytes: &[u8]) {
        for byte in bytes {
            *hash = (*hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3);
        }
    }
    let mut hash = 0xcbf29ce484222325;
    add(&mut hash, &(rows.len() as u64 + 1).to_le_bytes());
    for row in std::iter::once(document).chain(rows.iter().map(|row| row.as_slice())) {
        add(&mut hash, &(row.len() as u64).to_le_bytes());
        for value in row.iter() {
            match value {
                Value::Null => add(&mut hash, b"n"),
                Value::Integer(value) => {
                    add(&mut hash, b"i");
                    add(&mut hash, &value.to_le_bytes());
                }
                Value::Float(value) => {
                    add(&mut hash, b"f");
                    add(&mut hash, &value.to_bits().to_le_bytes());
                }
                Value::Boolean(value) => add(&mut hash, if *value { b"t" } else { b"b" }),
                Value::Text(value) => {
                    add(&mut hash, b"s");
                    add(&mut hash, &(value.len() as u64).to_le_bytes());
                    add(&mut hash, value.as_bytes());
                }
                Value::Vector(value) => {
                    add(&mut hash, b"v");
                    add(&mut hash, &(value.dimensions() as u64).to_le_bytes());
                    for value in value.as_slice() {
                        add(&mut hash, &value.to_bits().to_le_bytes());
                    }
                }
            }
        }
    }
    format!("{hash:016x}")
}
fn validate_document(document: &GraphDocumentInput, profile: &GraphEmbeddingProfile) -> Result<()> {
    validate_preview(&GraphDocumentPreview::from(document))?;
    for chunk in &document.chunks {
        if chunk.embedding.dimensions() != profile.dimensions {
            return Err(Error::DimensionMismatch {
                left: profile.dimensions,
                right: chunk.embedding.dimensions(),
            });
        }
        if chunk.embedding.norm() == 0.0 {
            return Err(Error::ZeroNorm);
        }
    }
    Ok(())
}
fn validate_preview(document: &GraphDocumentPreview<'_>) -> Result<()> {
    bounded_text(document.id, 512, false, "document id")?;
    bounded_text(document.title, 1024, true, "document title")?;
    bounded_text(document.source, 4096, true, "document source")?;
    bounded_text(document.text, MAX_DOCUMENT_BYTES, false, "document text")?;
    for (value, label) in [
        (&document.metadata, "metadata"),
        (&document.chunking, "chunking"),
    ] {
        if !value.is_object() || value.to_string().len() > 65536 {
            return Err(invalid(&format!(
                "{label} must be a JSON object of at most 65536 bytes"
            )));
        }
    }
    if document.chunks.is_empty() || document.chunks.len() > MAX_DOCUMENT_CHUNKS {
        return Err(invalid("document must contain 1..256 chunks"));
    }
    let mut previous_start = 0;
    let mut covered = 0;
    for (index, chunk) in document.chunks.iter().enumerate() {
        if chunk.start_byte >= chunk.end_byte
            || document.text.get(chunk.start_byte..chunk.end_byte) != Some(chunk.text)
            || (chunk.start_byte > covered
                && document
                    .text
                    .get(covered..chunk.start_byte)
                    .is_none_or(|gap| !gap.trim().is_empty()))
            || (index > 0 && (chunk.start_byte <= previous_start || chunk.end_byte <= covered))
        {
            return Err(invalid(
                "chunk byte offsets must match UTF-8 source slices and cover the document in order",
            ));
        }
        bounded_text(chunk.embedding_text, 32768, false, "embedding text")?;
        previous_start = chunk.start_byte;
        covered = chunk.end_byte;
    }
    if !document.text[covered..].trim().is_empty() {
        return Err(invalid("chunks must cover all non-whitespace source text"));
    }
    Ok(())
}
fn chunk_id(document: &str, ordinal: usize) -> String {
    format!("{}:{document}:{ordinal}", document.len())
}
fn edge(from_chunk: String, to_chunk: String, kind: &str, weight: f64) -> GraphEdge {
    GraphEdge {
        from_chunk,
        to_chunk,
        kind: kind.into(),
        weight,
    }
}
fn edge_row(edge: &GraphEdge) -> Vec<Value> {
    vec![
        Value::Text(format!(
            "{}:{}:{}:{}",
            edge.kind,
            edge.from_chunk.len(),
            edge.from_chunk,
            edge.to_chunk
        )),
        Value::Text(edge.from_chunk.clone()),
        Value::Text(edge.to_chunk.clone()),
        Value::Text(edge.kind.clone()),
        Value::Float(edge.weight),
    ]
}
fn read_edge(row: &[Value]) -> Result<GraphEdge> {
    let edge = edge(
        text_at(row, 1)?.into(),
        text_at(row, 2)?.into(),
        text_at(row, 3)?,
        number_at(row, 4)?,
    );
    if edge.kind.is_empty()
        || edge.kind.len() > 64
        || !edge.kind.as_bytes()[0].is_ascii_lowercase()
        || !edge
            .kind
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        || !(0.0..=1.0).contains(&edge.weight)
        || edge.from_chunk == edge.to_chunk
    {
        return Err(invalid("graph edge kind, weight, or endpoints are invalid"));
    }
    bounded_text(&edge.from_chunk, 1024, false, "relationship source")?;
    bounded_text(&edge.to_chunk, 1024, false, "relationship target")?;
    Ok(edge)
}
fn insert(
    catalog: &mut Catalog,
    name: &str,
    rows: Vec<Vec<Value>>,
    wal: &mut Option<String>,
) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    if let Some(wal) = wal {
        wal.push_str(&format!("INSERT INTO {} VALUES ", quote(name)));
        for (index, row) in rows.iter().enumerate() {
            if index > 0 {
                wal.push(',');
            }
            wal.push('(');
            for (index, value) in row.iter().enumerate() {
                if index > 0 {
                    wal.push(',');
                }
                wal.push_str(&sql_value(value));
            }
            wal.push(')');
        }
        wal.push(';');
    }
    let table = catalog
        .tables
        .get_mut(name)
        .ok_or_else(|| Error::TableNotFound(name.into()))?;
    let rows = prepare_typed_rows(table, rows)?;
    apply_insert_plan(table, rows, InsertConflictPlan::Fail)?;
    Ok(())
}
fn sql_value(value: &Value) -> String {
    match value {
        Value::Null => "NULL".into(),
        Value::Integer(value) => value.to_string(),
        Value::Float(value) => format!("{value:?}"),
        Value::Text(value) => literal(value),
        Value::Boolean(value) => value.to_string(),
        Value::Vector(value) => format!(
            "ARRAY[{}]",
            value
                .as_slice()
                .iter()
                .map(|value| format!("{value:?}"))
                .collect::<Vec<_>>()
                .join(",")
        ),
    }
}
fn remove_document(
    catalog: &mut Catalog,
    tables: &GraphTables,
    id: &str,
    wal: &mut Option<String>,
) -> Result<(usize, usize)> {
    let (ids, chunk_count) = document_chunk_ids(catalog, tables, id)?;
    let mut edges_removed = 0;
    for (name, kind) in [
        (&tables.edges, 0),
        (&tables.chunks, 1),
        (&tables.documents, 2),
    ] {
        let table = catalog
            .tables
            .get_mut(name)
            .ok_or_else(|| Error::TableNotFound(name.clone()))?;
        let before = table.rows.len();
        table.rows.retain(|row| match kind { 0=>!matches!((row.get(1),row.get(2)),(Some(Value::Text(from)),Some(Value::Text(to))) if ids.contains(from)||ids.contains(to)),1=>!matches!(row.get(1),Some(Value::Text(value)) if value==id),_=>!matches!(row.first(),Some(Value::Text(value)) if value==id) });
        if kind == 0 {
            edges_removed = before - table.rows.len();
        }
        if before != table.rows.len() {
            rebuild_indexes(table);
        }
    }
    if let Some(wal) = wal {
        if !ids.is_empty() {
            let mut ids = ids.iter().map(|id| literal(id)).collect::<Vec<_>>();
            ids.sort();
            let ids = ids.join(",");
            wal.push_str(&format!(
                "DELETE FROM {} WHERE from_chunk IN ({ids}) OR to_chunk IN ({ids});",
                quote(&tables.edges)
            ));
        }
        wal.push_str(&format!(
            "DELETE FROM {} WHERE document_id={};DELETE FROM {} WHERE document_id={};",
            quote(&tables.chunks),
            literal(id),
            quote(&tables.documents),
            literal(id)
        ));
    }
    Ok((chunk_count, edges_removed))
}

fn document_chunk_ids(
    catalog: &Catalog,
    tables: &GraphTables,
    id: &str,
) -> Result<(HashSet<String>, usize)> {
    let mut ids = table(catalog, &tables.chunks)?
        .rows
        .iter()
        .filter(|row| matches!(row.get(1), Some(Value::Text(value)) if value == id))
        .map(|row| text_at(row, 0).map(str::to_owned))
        .collect::<Result<HashSet<_>>>()?;
    let count = ids.len();
    // A raw SQL deletion may have removed a chunk but left incident edges.
    // Generated IDs retain the exact length-prefixed document namespace.
    let prefix = format!("{}:{id}:", id.len());
    for row in &table(catalog, &tables.edges)?.rows {
        for index in [1, 2] {
            let endpoint = text_at(row, index)?;
            if endpoint.strip_prefix(&prefix).is_some_and(|ordinal| {
                !ordinal.is_empty() && ordinal.bytes().all(|byte| byte.is_ascii_digit())
            }) {
                ids.insert(endpoint.into());
            }
        }
    }
    Ok((ids, count))
}
fn hit(
    chunk: &[Value],
    documents: &HashMap<&str, &Vec<Value>>,
    document_columns: &[Column],
    query: &Vector,
    depth: usize,
) -> Result<GraphHit> {
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
    let Some(Value::Vector(embedding)) = chunk.get(8) else {
        return Err(invalid("graph chunk embedding is missing"));
    };
    let similarity = (1.0 - f64::from(query.cosine_distance(embedding)?)).clamp(-1.0, 1.0);
    Ok(GraphHit {
        chunk_id: text_at(chunk, 0)?.into(),
        document_id: document_id.into(),
        title: text_at(document, 1)?.into(),
        source: text_at(document, 2)?.into(),
        text: text.into(),
        metadata: document_metadata(document, document_columns)?,
        start_byte,
        end_byte,
        similarity,
        depth,
        seed: depth == 0,
    })
}
