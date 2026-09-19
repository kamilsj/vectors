//! An embeddable SQL engine with first-class vector values.
//!
//! Relational predicates and vector distance functions can be combined in one
//! query. Queries run against a memory-resident catalog; an optional persistent
//! data directory adds a synchronized write-ahead log and compact checkpoints.

pub mod api;
pub mod chunking;
mod compute;
mod durable;
mod embedding;
mod engine;
mod error;
#[cfg(feature = "gpu")]
mod gpu;
mod reranking;
mod storage;
mod vector;

pub use compute::{ComputeConfig, ComputeDevice};
pub use embedding::EmbeddingService;
pub use engine::{
    Column, DataType, Database, ExecutionResult, IndexInfo, InsertConflict, QueryColumnRole,
    QueryIntent, QueryIntentColumn, QueryResult, TableInfo, Value, VectorFilterOperator,
    VectorQueryIntent, VectorSearch, VectorSearchFilter, VectorSearchMetric,
};
pub use engine::{
    GraphBrowseRequest, GraphBrowseResult, GraphChunkInput, GraphChunkPreview, GraphCollection,
    GraphCollectionConfig, GraphDeleteResult, GraphDocument, GraphDocumentInput,
    GraphDocumentPreview, GraphEdge, GraphEmbeddingProfile, GraphHit, GraphIngestRequest,
    GraphIngestResult, GraphNeighborhoodDirection, GraphNeighborhoodNode, GraphNeighborhoodRequest,
    GraphNeighborhoodResult, GraphNode, GraphRagCandidate, GraphRagHit, GraphRagRequest,
    GraphRagResult, GraphRagSelection, GraphRagSnapshot, GraphRelationshipDeleteRequest,
    GraphRelationshipDeleteResult, GraphRelationshipRequest, GraphRelationshipResult,
    GraphSearchRequest, GraphSearchResult, GraphTables,
};
pub use error::{Error, Result};
pub use reranking::RerankingService;
pub use vector::{Vector, MAX_VECTOR_DIMENSIONS};
