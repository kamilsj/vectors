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
    #[error("query returns at least {found_at_least} rows; maximum is {max}")]
    ResultLimitExceeded { found_at_least: usize, max: usize },
    #[error("table would contain {found} rows; maximum is {max}")]
    TableRowLimit { found: usize, max: usize },
    #[error("database lock was poisoned")]
    LockPoisoned,
    #[error("GPU compute is unavailable: {0}")]
    GpuUnavailable(String),
    #[error("storage I/O error: {0}")]
    StorageIo(String),
    #[error("data directory '{0}' is already open by another process")]
    StorageBusy(String),
    #[error("corrupt snapshot: {0}")]
    CorruptSnapshot(String),
    #[error("corrupt write-ahead log: {0}")]
    CorruptWal(String),
}

pub type Result<T> = std::result::Result<T, Error>;
