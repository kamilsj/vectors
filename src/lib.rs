//! An embeddable SQL engine with first-class vector values.
//!
//! Relational predicates and vector distance functions can be combined in one
//! query. The catalog is held in memory and can be saved to a versioned snapshot.

pub mod api;
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
