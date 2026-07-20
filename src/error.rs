use thiserror::Error;

/// Errors returned by the parser, planner, storage layer, or expression evaluator.
#[derive(Debug, Error, Clone, PartialEq)]
pub enum Error {
    #[error("SQL parse error: {0}")]
    Parse(String),
    #[error("unsupported SQL: {0}")]
    Unsupported(String),
    #[error("table '{0}' already exists")]
    TableAlreadyExists(String),
    #[error("table '{0}' does not exist")]
    TableNotFound(String),
    #[error("index '{0}' already exists")]
    IndexAlreadyExists(String),
    #[error("index '{0}' does not exist")]
    IndexNotFound(String),
    #[error("column '{0}' does not exist")]
    ColumnNotFound(String),
    #[error("column '{0}' appears more than once")]
    DuplicateColumn(String),
    #[error("expected {expected}, found {found}")]
    TypeMismatch { expected: String, found: String },
    #[error("vector dimensions differ: left has {left}, right has {right}")]
    DimensionMismatch { left: usize, right: usize },
    #[error("vector dimension must be greater than zero")]
    InvalidVectorDimension,
    #[error("vector has {found} dimensions; the maximum is {max}")]
    VectorDimensionLimit { found: usize, max: usize },
    #[error("vector element at index {index} is not finite")]
    NonFiniteVectorElement { index: usize },
    #[error("cosine distance is undefined for a zero vector")]
    ZeroNorm,
    #[error("column '{0}' cannot be null")]
    NullViolation(String),
    #[error("duplicate value violates unique constraint on column '{0}'")]
    UniqueViolation(String),
    #[error("invalid query: {0}")]
    InvalidQuery(String),
    #[error("database lock was poisoned")]
    LockPoisoned,
    #[error("storage I/O error: {0}")]
    StorageIo(String),
    #[error("corrupt snapshot: {0}")]
    CorruptSnapshot(String),
}

pub type Result<T> = std::result::Result<T, Error>;
