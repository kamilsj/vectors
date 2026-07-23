//! An embeddable SQL engine with first-class vector values.
//!
//! Relational predicates and vector distance functions can be combined in one
//! query. Queries run against a memory-resident catalog; an optional persistent
//! data directory adds a synchronized write-ahead log and compact checkpoints.

pub mod api;
mod durable;
mod engine;
mod error;
mod storage;
mod vector;

pub use engine::{
    Column, DataType, Database, ExecutionResult, IndexInfo, InsertConflict, QueryResult, TableInfo,
    Value,
};
pub use error::{Error, Result};
pub use vector::{Vector, MAX_VECTOR_DIMENSIONS};
