use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

use rayon::prelude::*;
use sqlparser::ast::{
    Assignment, BinaryOperator, ColumnDef, ColumnOption, ConflictTarget, DoUpdate, Expr, Function,
    FunctionArg, FunctionArgExpr, Ident, ObjectName, ObjectType, OnConflictAction, OnInsert,
    OrderByExpr, Query, Select, SelectItem, SetExpr, Statement, TableConstraint, TableFactor,
    TableWithJoins, UnaryOperator, Value as SqlValue,
};
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;

use crate::durable::{PersistentStorage, WalOperation};
use crate::{storage, Error, Result, Vector, MAX_VECTOR_DIMENSIONS};

/// Logical types supported by the in-memory storage engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DataType {
    Integer,
    Float,
    Text,
    Boolean,
    Vector(usize),
}

impl fmt::Display for DataType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Integer => formatter.write_str("INTEGER"),
            Self::Float => formatter.write_str("DOUBLE"),
            Self::Text => formatter.write_str("TEXT"),
            Self::Boolean => formatter.write_str("BOOLEAN"),
            Self::Vector(dimensions) => write!(formatter, "VECTOR({dimensions})"),
        }
    }
}

/// A stored SQL value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Integer(i64),
    Float(f64),
    Text(String),
    Boolean(bool),
    Vector(Vector),
}

impl Value {
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Null => "NULL",
            Self::Integer(_) => "INTEGER",
            Self::Float(_) => "FLOAT",
            Self::Text(_) => "TEXT",
            Self::Boolean(_) => "BOOLEAN",
            Self::Vector(_) => "VECTOR",
        }
    }

    fn as_bool(&self) -> Result<Option<bool>> {
        match self {
            Self::Boolean(value) => Ok(Some(*value)),
            Self::Null => Ok(None),
            value => Err(type_mismatch("BOOLEAN", value)),
        }
    }

    fn as_f64(&self) -> Result<Option<f64>> {
        match self {
            Self::Integer(value) => Ok(Some(*value as f64)),
            Self::Float(value) => Ok(Some(*value)),
            Self::Null => Ok(None),
            value => Err(type_mismatch("numeric value", value)),
        }
    }

    fn as_vector(&self) -> Result<Option<&Vector>> {
        match self {
            Self::Vector(value) => Ok(Some(value)),
            Self::Null => Ok(None),
            value => Err(type_mismatch("VECTOR", value)),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => formatter.write_str("NULL"),
            Self::Integer(value) => write!(formatter, "{value}"),
            Self::Float(value) => write!(formatter, "{value}"),
            Self::Text(value) => formatter.write_str(value),
            Self::Boolean(value) => write!(formatter, "{value}"),
            Self::Vector(value) => write!(formatter, "{value}"),
        }
    }
}

/// A column in a table schema.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub data_type: DataType,
    pub nullable: bool,
    pub unique: bool,
}

/// Rows and column labels produced by `SELECT`.
#[derive(Clone, Debug, PartialEq)]
pub struct QueryResult {
    pub columns: Vec<String>,
    /// Declared type of each output column. `None` is used only when SQL does
    /// not provide enough information to type an expression, such as `NULL`.
    pub column_types: Vec<Option<DataType>>,
    pub rows: Vec<Vec<Value>>,
    /// Number of source rows evaluated after index pruning.
    pub rows_examined: usize,
}

impl QueryResult {
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }
}

/// Semantic role assigned to a selected output column by [`Database::query_intent`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryColumnRole {
    Identifier,
    Content,
    Attribute,
    Embedding,
    SimilarityScore,
    Aggregate,
    Computed,
}

impl fmt::Display for QueryColumnRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Identifier => "identifier",
            Self::Content => "content",
            Self::Attribute => "attribute",
            Self::Embedding => "embedding",
            Self::SimilarityScore => "similarity_score",
            Self::Aggregate => "aggregate",
            Self::Computed => "computed",
        })
    }
}

/// Schema-aware description of one `SELECT` output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryIntentColumn {
    pub output_name: String,
    pub source_column: Option<String>,
    pub data_type: Option<DataType>,
    pub role: QueryColumnRole,
}

/// Vector-ranking behavior recognized in a `SELECT` order expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VectorQueryIntent {
    pub metric: String,
    pub column: String,
    pub dimensions: usize,
    pub descending: bool,
    pub optimized: bool,
}

/// Validated, schema-aware intent extracted from one read-only SQL query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryIntent {
    pub table: Option<String>,
    pub columns: Vec<QueryIntentColumn>,
    pub distinct: bool,
    pub aggregation: bool,
    pub filter: Option<String>,
    pub group_by: Vec<String>,
    pub having: Option<String>,
    pub order_by: Vec<String>,
    pub limit: Option<usize>,
    pub offset: usize,
    pub vector_search: Option<VectorQueryIntent>,
    pub summary: String,
}

/// Read-only metadata for a scalar hash index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexInfo {
    pub name: String,
    pub column: String,
}

/// Read-only summary of a table and its in-memory footprint shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableInfo {
    pub name: String,
    pub row_count: usize,
    pub column_count: usize,
    pub index_count: usize,
}

/// Result of one SQL statement.
#[derive(Clone, Debug, PartialEq)]
pub enum ExecutionResult {
    Query(QueryResult),
    Command {
        tag: &'static str,
        rows_affected: usize,
    },
}

/// Conflict behavior for typed bulk insertion through [`Database::insert_rows`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum InsertConflict {
    /// Reject the entire batch when any unique constraint is violated.
    #[default]
    Fail,
    /// Skip rows that conflict with the target, or any unique column when the
    /// target is `None`.
    DoNothing { target: Option<String> },
    /// Replace the listed columns from the incoming row when `target` conflicts.
    DoUpdate {
        target: String,
        update_columns: Vec<String>,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct Table {
    pub(crate) columns: Vec<Column>,
    pub(crate) rows: Vec<Vec<Value>>,
    pub(crate) indexes: HashMap<String, HashIndex>,
    unique_keys: HashMap<usize, HashMap<UniqueKey, usize>>,
}

impl Table {
    pub(crate) fn new(
        columns: Vec<Column>,
        rows: Vec<Vec<Value>>,
        indexes: HashMap<String, HashIndex>,
    ) -> Self {
        Self {
            columns,
            rows,
            indexes,
            unique_keys: HashMap::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct HashIndex {
    pub(crate) column: usize,
    buckets: HashMap<UniqueKey, Vec<usize>>,
}

#[derive(Clone, Default, Debug)]
pub(crate) struct Catalog {
    pub(crate) tables: HashMap<String, Table>,
    pub(crate) revision: u64,
    pub(crate) durable_sequence: u64,
}

const PARSE_CACHE_MAX_ENTRIES: usize = 64;
const PARSE_CACHE_MAX_SQL_BYTES: usize = 1024 * 1024;
const PARSE_CACHE_MAX_ENTRY_BYTES: usize = 64 * 1024;

#[derive(Debug)]
struct CachedSql {
    sql: String,
    statements: Vec<Statement>,
}

#[derive(Debug, Default)]
struct ParseCache {
    entries: VecDeque<CachedSql>,
    sql_bytes: usize,
}

impl ParseCache {
    fn get(&mut self, sql: &str) -> Option<Vec<Statement>> {
        let position = self.entries.iter().position(|entry| entry.sql == sql)?;
        let entry = self.entries.remove(position)?;
        let statements = entry.statements.clone();
        self.entries.push_front(entry);
        Some(statements)
    }

    fn insert(&mut self, sql: &str, statements: &[Statement]) {
        if sql.len() > PARSE_CACHE_MAX_ENTRY_BYTES {
            return;
        }
        if let Some(position) = self.entries.iter().position(|entry| entry.sql == sql) {
            if let Some(entry) = self.entries.remove(position) {
                self.sql_bytes -= entry.sql.len();
            }
        }
        while self.entries.len() >= PARSE_CACHE_MAX_ENTRIES
            || self.sql_bytes + sql.len() > PARSE_CACHE_MAX_SQL_BYTES
        {
            let Some(entry) = self.entries.pop_back() else {
                break;
            };
            self.sql_bytes -= entry.sql.len();
        }
        self.sql_bytes += sql.len();
        self.entries.push_front(CachedSql {
            sql: sql.into(),
            statements: statements.to_vec(),
        });
    }
}

impl Catalog {
    fn mark_changed(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }
}

/// A cloneable database handle. Clones share one thread-safe catalog.
#[derive(Clone, Default, Debug)]
pub struct Database {
    catalog: Arc<RwLock<Catalog>>,
    snapshot_lock: Arc<Mutex<()>>,
    parse_cache: Arc<Mutex<ParseCache>>,
    persistent: Option<Arc<PersistentStorage>>,
}

impl Database {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a database from a versioned binary snapshot.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self {
            catalog: Arc::new(RwLock::new(storage::load(path.as_ref())?)),
            snapshot_lock: Arc::new(Mutex::new(())),
            parse_cache: Arc::new(Mutex::new(ParseCache::default())),
            persistent: None,
        })
    }

    /// Open or create a durable database in `directory`.
    ///
    /// Queries execute against the memory-resident catalog. Successful writes
    /// are synchronized to a checksummed write-ahead log before becoming
    /// visible, and the log is periodically compacted into `vectors.vdb`.
    /// Opening the same directory from another process fails while this handle
    /// or any of its clones remains alive.
    pub fn open_persistent(directory: impl AsRef<Path>) -> Result<Self> {
        let (persistent, catalog, records) = PersistentStorage::open(directory.as_ref())?;
        let database = Self {
            catalog: Arc::new(RwLock::new(catalog)),
            snapshot_lock: Arc::new(Mutex::new(())),
            parse_cache: Arc::new(Mutex::new(ParseCache::default())),
            persistent: None,
        };
        database.recover(records)?;
        {
            let mut catalog = database.catalog.write().map_err(|_| Error::LockPoisoned)?;
            catalog.revision = 0;
        }
        Ok(Self {
            persistent: Some(persistent),
            ..database
        })
    }

    /// Save a coherent database snapshot using temporary-file replacement.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        self.save_snapshot(path.as_ref(), None).map(|_| ())
    }

    /// Save a snapshot only when the catalog changed after `last_revision`.
    ///
    /// The returned revision identifies the catalog copy written to disk. A
    /// `None` result means no disk I/O was needed.
    pub fn save_if_changed(
        &self,
        path: impl AsRef<Path>,
        last_revision: u64,
    ) -> Result<Option<u64>> {
        self.save_snapshot(path.as_ref(), Some(last_revision))
    }

    /// Return the current in-process catalog revision.
    pub fn revision(&self) -> Result<u64> {
        let catalog = self.catalog.read().map_err(|_| Error::LockPoisoned)?;
        Ok(catalog.revision)
    }

    /// Analyze one read-only `SELECT` without executing it.
    ///
    /// Wildcards are expanded using the current schema. Returned column roles
    /// distinguish identifiers, content, scalar attributes, embeddings,
    /// similarity scores, and other computed expressions.
    pub fn query_intent(&self, sql: &str) -> Result<QueryIntent> {
        let mut statements = self.parse_sql(sql)?;
        if statements.len() != 1 {
            return Err(Error::InvalidQuery(
                "query intent requires exactly one SELECT statement".into(),
            ));
        }
        let Statement::Query(query) = statements.remove(0) else {
            return Err(Error::InvalidQuery(
                "query intent only accepts a read-only SELECT statement".into(),
            ));
        };
        let catalog = self.catalog.read().map_err(|_| Error::LockPoisoned)?;
        analyze_query_intent(&catalog, &query)
    }

    /// Return the persistent data directory, or `None` for an in-memory or
    /// snapshot-opened database.
    pub fn data_directory(&self) -> Option<&Path> {
        self.persistent.as_deref().map(PersistentStorage::directory)
    }

    /// Compact a persistent database's WAL into a synchronized checkpoint.
    pub fn checkpoint(&self) -> Result<()> {
        let persistent = self.persistent.as_ref().ok_or_else(|| {
            Error::StorageIo("checkpoint requires a persistent data directory".into())
        })?;
        let catalog = self.catalog.write().map_err(|_| Error::LockPoisoned)?;
        persistent.checkpoint(&catalog)
    }

    fn save_snapshot(&self, path: &Path, last_revision: Option<u64>) -> Result<Option<u64>> {
        // Serialize checkpoints from cloned handles, then release the catalog
        // lock as soon as a coherent copy has been captured. Disk I/O must not
        // stall writers.
        let _snapshot_guard = self.snapshot_lock.lock().map_err(|_| Error::LockPoisoned)?;
        let (catalog, revision) = {
            let catalog = self.catalog.read().map_err(|_| Error::LockPoisoned)?;
            if last_revision == Some(catalog.revision) {
                return Ok(None);
            }
            (catalog.clone(), catalog.revision)
        };
        storage::save(&catalog, path)?;
        Ok(Some(revision))
    }

    /// Parse and execute one or more semicolon-separated SQL statements.
    ///
    /// A request containing a write is applied to a private catalog snapshot
    /// first and committed under one write lock. If any statement or durable
    /// WAL append fails, none of the writes in that request become visible.
    pub fn execute(&self, sql: &str) -> Result<Vec<ExecutionResult>> {
        let statements = self.parse_sql(sql)?;
        if (self.persistent.is_none() && statements.len() <= 1)
            || statements
                .iter()
                .all(|statement| matches!(statement, Statement::Query(_)))
        {
            return statements
                .into_iter()
                .map(|statement| self.execute_statement(statement))
                .collect();
        }

        let mut catalog = self.catalog.write().map_err(|_| Error::LockPoisoned)?;
        let staging = Self {
            catalog: Arc::new(RwLock::new(catalog.clone())),
            snapshot_lock: self.snapshot_lock.clone(),
            parse_cache: self.parse_cache.clone(),
            persistent: None,
        };
        let results = statements
            .into_iter()
            .map(|statement| staging.execute_statement(statement))
            .collect::<Result<Vec<_>>>()?;
        let mut committed = staging
            .catalog
            .read()
            .map_err(|_| Error::LockPoisoned)?
            .clone();
        let changed = committed.revision != catalog.revision;
        let checkpoint_needed = if changed {
            if let Some(persistent) = &self.persistent {
                let sequence = next_durable_sequence(catalog.durable_sequence)?;
                let operation = PersistentStorage::prepare_sql(sql)?;
                let checkpoint_needed = persistent.append(sequence, operation)?;
                committed.durable_sequence = sequence;
                checkpoint_needed
            } else {
                false
            }
        } else {
            false
        };
        *catalog = committed;
        drop(catalog);
        if checkpoint_needed {
            // The WAL commit is already durable and visible. Checkpointing is
            // maintenance, so its failure cannot retroactively fail the write;
            // callers can use `checkpoint` to observe and retry it explicitly.
            let _ = self.checkpoint();
        }
        Ok(results)
    }

    fn recover(&self, records: Vec<crate::durable::RecoveryRecord>) -> Result<()> {
        let checkpoint_sequence = self
            .catalog
            .read()
            .map_err(|_| Error::LockPoisoned)?
            .durable_sequence;
        let mut expected = checkpoint_sequence
            .checked_add(1)
            .ok_or_else(|| Error::CorruptWal("checkpoint sequence overflow".into()))?;
        for record in records {
            if record.sequence <= checkpoint_sequence {
                continue;
            }
            if record.sequence != expected {
                return Err(Error::CorruptWal(format!(
                    "expected record sequence {expected}, found {}",
                    record.sequence
                )));
            }
            let before = self.revision()?;
            let result = match record.operation {
                WalOperation::Sql(sql) => self.execute(&sql).map(|_| ()),
                WalOperation::InsertRows {
                    table,
                    rows,
                    conflict,
                } => self.insert_rows(&table, rows, conflict).map(|_| ()),
            };
            result.map_err(|error| {
                Error::CorruptWal(format!(
                    "record {} cannot be replayed: {error}",
                    record.sequence
                ))
            })?;
            if self.revision()? == before {
                return Err(Error::CorruptWal(format!(
                    "record {} did not change the catalog",
                    record.sequence
                )));
            }
            let mut catalog = self.catalog.write().map_err(|_| Error::LockPoisoned)?;
            catalog.durable_sequence = record.sequence;
            expected = expected
                .checked_add(1)
                .ok_or_else(|| Error::CorruptWal("record sequence overflow".into()))?;
        }
        Ok(())
    }

    fn parse_sql(&self, sql: &str) -> Result<Vec<Statement>> {
        // A poisoned or contended cache must never make SQL unavailable. Cache
        // access is short and parsing happens after the lock has been released.
        if let Ok(mut cache) = self.parse_cache.lock() {
            if let Some(statements) = cache.get(sql) {
                return Ok(statements);
            }
        }
        let statements = Parser::parse_sql(&GenericDialect {}, sql)
            .map_err(|error| Error::Parse(error.to_string()))?;
        if let Ok(mut cache) = self.parse_cache.lock() {
            cache.insert(sql, &statements);
        }
        Ok(statements)
    }

    /// Return a copy of a table schema for inspection by an embedding application.
    pub fn schema(&self, table_name: &str) -> Result<Vec<Column>> {
        let catalog = self.catalog.read().map_err(|_| Error::LockPoisoned)?;
        let table_name = normalize_name(table_name);
        catalog
            .tables
            .get(&table_name)
            .map(|table| table.columns.clone())
            .ok_or(Error::TableNotFound(table_name))
    }

    /// Return table names in deterministic order.
    pub fn tables(&self) -> Result<Vec<String>> {
        let catalog = self.catalog.read().map_err(|_| Error::LockPoisoned)?;
        let mut tables = catalog.tables.keys().cloned().collect::<Vec<_>>();
        tables.sort_unstable();
        Ok(tables)
    }

    /// Return table summaries in deterministic order.
    pub fn table_info(&self) -> Result<Vec<TableInfo>> {
        let catalog = self.catalog.read().map_err(|_| Error::LockPoisoned)?;
        let mut tables = catalog
            .tables
            .iter()
            .map(|(name, table)| TableInfo {
                name: name.clone(),
                row_count: table.rows.len(),
                column_count: table.columns.len(),
                index_count: table.indexes.len(),
            })
            .collect::<Vec<_>>();
        tables.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        Ok(tables)
    }

    /// Return scalar index metadata for a table.
    pub fn indexes(&self, table_name: &str) -> Result<Vec<IndexInfo>> {
        let catalog = self.catalog.read().map_err(|_| Error::LockPoisoned)?;
        let table_name = normalize_name(table_name);
        let table = catalog
            .tables
            .get(&table_name)
            .ok_or_else(|| Error::TableNotFound(table_name.clone()))?;
        let mut indexes = table
            .indexes
            .iter()
            .map(|(name, index)| IndexInfo {
                name: name.clone(),
                column: table.columns[index.column].name.clone(),
            })
            .collect::<Vec<_>>();
        indexes.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        Ok(indexes)
    }

    /// Insert fully typed rows without serializing them through SQL text.
    ///
    /// Values are supplied in schema order. The complete batch is validated
    /// before it becomes visible, and conflict handling follows the same core
    /// path as SQL `INSERT`.
    pub fn insert_rows(
        &self,
        table_name: &str,
        rows: Vec<Vec<Value>>,
        conflict: InsertConflict,
    ) -> Result<usize> {
        if self.persistent.is_none() {
            return self.insert_rows_in_memory(table_name, rows, conflict);
        }
        let table_name = normalize_name(table_name);
        let mut catalog = self.catalog.write().map_err(|_| Error::LockPoisoned)?;
        let table = catalog
            .tables
            .get(&table_name)
            .ok_or_else(|| Error::TableNotFound(table_name.clone()))?;
        let conflict_plan = resolve_typed_conflict_plan(table, &conflict)?;
        let pending = prepare_typed_rows(table, rows)?;
        let wal_operation =
            PersistentStorage::prepare_insert_rows(&table_name, &pending, &conflict)?;
        let mutation = prepare_durable_insert(table, pending, conflict_plan)?;
        let rows_affected = mutation.rows_affected();
        let checkpoint_needed = if rows_affected > 0 {
            let sequence = next_durable_sequence(catalog.durable_sequence)?;
            let checkpoint_needed = self
                .persistent
                .as_ref()
                .expect("persistent storage checked above")
                .append(sequence, wal_operation)?;
            mutation.apply(
                catalog
                    .tables
                    .get_mut(&table_name)
                    .expect("table exists while write lock is held"),
            );
            catalog.mark_changed();
            catalog.durable_sequence = sequence;
            checkpoint_needed
        } else {
            false
        };
        drop(catalog);
        if checkpoint_needed {
            // See the SQL path above: a compaction failure must not turn an
            // already synchronized WAL commit into a reported transaction
            // failure.
            let _ = self.checkpoint();
        }
        Ok(rows_affected)
    }

    fn insert_rows_in_memory(
        &self,
        table_name: &str,
        rows: Vec<Vec<Value>>,
        conflict: InsertConflict,
    ) -> Result<usize> {
        let table_name = normalize_name(table_name);
        let mut catalog = self.catalog.write().map_err(|_| Error::LockPoisoned)?;
        let table = catalog
            .tables
            .get_mut(&table_name)
            .ok_or_else(|| Error::TableNotFound(table_name.clone()))?;
        let conflict_plan = resolve_typed_conflict_plan(table, &conflict)?;
        let pending = prepare_typed_rows(table, rows)?;
        let rows_affected = apply_insert_plan(table, pending, conflict_plan)?;
        if rows_affected > 0 {
            catalog.mark_changed();
        }
        Ok(rows_affected)
    }

    fn execute_statement(&self, statement: Statement) -> Result<ExecutionResult> {
        match statement {
            Statement::CreateTable {
                name,
                columns,
                constraints,
                if_not_exists,
                query,
                ..
            } => {
                if query.is_some() {
                    return Err(Error::Unsupported(
                        "CREATE TABLE AS is not supported".into(),
                    ));
                }
                self.create_table(name, columns, constraints, if_not_exists)
            }
            Statement::Insert {
                table_name,
                columns,
                source,
                on,
                returning,
                ..
            } => {
                if returning.is_some() {
                    return Err(Error::Unsupported("INSERT ... RETURNING".into()));
                }
                self.insert(table_name, columns, source, on)
            }
            Statement::Query(query) => {
                let catalog = self.catalog.read().map_err(|_| Error::LockPoisoned)?;
                Ok(ExecutionResult::Query(run_query(&catalog, &query)?))
            }
            Statement::Explain {
                analyze,
                verbose,
                statement,
                format,
                ..
            } => {
                if analyze || format.is_some() {
                    return Err(Error::Unsupported(
                        "EXPLAIN ANALYZE and formatted EXPLAIN output".into(),
                    ));
                }
                let Statement::Query(query) = statement.as_ref() else {
                    return Err(Error::Unsupported(
                        "EXPLAIN for statements other than SELECT".into(),
                    ));
                };
                let catalog = self.catalog.read().map_err(|_| Error::LockPoisoned)?;
                Ok(ExecutionResult::Query(explain_query(
                    &catalog, query, verbose,
                )?))
            }
            Statement::CreateIndex {
                name,
                table_name,
                using,
                columns,
                unique,
                concurrently,
                if_not_exists,
                include,
                nulls_distinct,
                predicate,
            } => {
                if unique
                    || concurrently
                    || !include.is_empty()
                    || nulls_distinct.is_some()
                    || predicate.is_some()
                {
                    return Err(Error::Unsupported(
                        "unique, concurrent, covering, partial, or NULL-configured indexes".into(),
                    ));
                }
                if let Some(method) = &using {
                    if ident_name(method) != "hash" {
                        return Err(Error::Unsupported(format!("index method {}", method.value)));
                    }
                }
                self.create_index(name, table_name, columns, if_not_exists)
            }
            Statement::Update {
                table,
                assignments,
                from,
                selection,
                returning,
            } => {
                if from.is_some() || returning.is_some() {
                    return Err(Error::Unsupported("UPDATE ... FROM or RETURNING".into()));
                }
                self.update(table, assignments, selection)
            }
            Statement::Delete {
                tables,
                from,
                using,
                selection,
                returning,
                order_by,
                limit,
            } => {
                if !tables.is_empty()
                    || using.is_some()
                    || returning.is_some()
                    || !order_by.is_empty()
                    || limit.is_some()
                {
                    return Err(Error::Unsupported(
                        "multi-table DELETE, USING, RETURNING, ORDER BY, and LIMIT".into(),
                    ));
                }
                self.delete(from, selection)
            }
            Statement::Drop {
                object_type,
                if_exists,
                names,
                cascade,
                restrict,
                purge,
                temporary,
            } => {
                if cascade || restrict || purge || temporary {
                    return Err(Error::Unsupported(
                        "DROP CASCADE, RESTRICT, PURGE, or TEMPORARY".into(),
                    ));
                }
                match object_type {
                    ObjectType::Table => self.drop_tables(names, if_exists),
                    ObjectType::Index => self.drop_indexes(names, if_exists),
                    _ => Err(Error::Unsupported(format!("DROP {object_type}"))),
                }
            }
            other => Err(Error::Unsupported(other.to_string())),
        }
    }

    fn create_table(
        &self,
        name: ObjectName,
        definitions: Vec<ColumnDef>,
        constraints: Vec<TableConstraint>,
        if_not_exists: bool,
    ) -> Result<ExecutionResult> {
        let name = object_name(&name);
        if definitions.is_empty() {
            return Err(Error::InvalidQuery(
                "a table must contain at least one column".into(),
            ));
        }

        let mut seen = HashSet::new();
        let mut columns = Vec::with_capacity(definitions.len());
        for definition in definitions {
            let column_name = ident_name(&definition.name);
            if !seen.insert(column_name.clone()) {
                return Err(Error::DuplicateColumn(column_name));
            }
            let data_type = parse_data_type(&definition.data_type)?;
            let mut nullable = true;
            let mut unique = false;
            for option in definition.options {
                match option.option {
                    ColumnOption::Null => nullable = true,
                    ColumnOption::NotNull => nullable = false,
                    ColumnOption::Unique { is_primary, .. } => {
                        unique = true;
                        if is_primary {
                            nullable = false;
                        }
                    }
                    ColumnOption::Comment(_) => {}
                    unsupported => {
                        return Err(Error::Unsupported(format!("column option {unsupported}")))
                    }
                }
            }
            columns.push(Column {
                name: column_name,
                data_type,
                nullable,
                unique,
            });
        }

        for constraint in constraints {
            match constraint {
                TableConstraint::Unique {
                    columns: constrained,
                    is_primary,
                    ..
                } if constrained.len() == 1 => {
                    let index = find_column(&columns, &ident_name(&constrained[0]))?;
                    columns[index].unique = true;
                    if is_primary {
                        columns[index].nullable = false;
                    }
                }
                TableConstraint::Unique { .. } => {
                    return Err(Error::Unsupported(
                        "composite UNIQUE and PRIMARY KEY constraints".into(),
                    ))
                }
                other => return Err(Error::Unsupported(format!("table constraint {other}"))),
            }
        }

        let mut catalog = self.catalog.write().map_err(|_| Error::LockPoisoned)?;
        if catalog.tables.contains_key(&name) {
            if if_not_exists {
                return Ok(ExecutionResult::Command {
                    tag: "CREATE TABLE",
                    rows_affected: 0,
                });
            }
            return Err(Error::TableAlreadyExists(name));
        }
        let mut table = Table::new(columns, Vec::new(), HashMap::new());
        rebuild_indexes(&mut table);
        catalog.tables.insert(name, table);
        catalog.mark_changed();
        Ok(ExecutionResult::Command {
            tag: "CREATE TABLE",
            rows_affected: 0,
        })
    }

    fn insert(
        &self,
        table_name: ObjectName,
        insert_columns: Vec<Ident>,
        source: Option<Box<Query>>,
        on_insert: Option<OnInsert>,
    ) -> Result<ExecutionResult> {
        let table_name = object_name(&table_name);
        let source = source.ok_or_else(|| Error::InvalidQuery("INSERT has no source".into()))?;
        let rows = match source.body.as_ref() {
            SetExpr::Values(values) => &values.rows,
            _ => return Err(Error::Unsupported("INSERT ... SELECT".into())),
        };

        let mut catalog = self.catalog.write().map_err(|_| Error::LockPoisoned)?;
        let table = catalog
            .tables
            .get_mut(&table_name)
            .ok_or_else(|| Error::TableNotFound(table_name.clone()))?;
        let conflict_plan = resolve_conflict_plan(table, on_insert)?;

        let target_indexes = if insert_columns.is_empty() {
            (0..table.columns.len()).collect::<Vec<_>>()
        } else {
            let mut seen = HashSet::new();
            insert_columns
                .iter()
                .map(|name| {
                    let name = ident_name(name);
                    if !seen.insert(name.clone()) {
                        return Err(Error::DuplicateColumn(name));
                    }
                    find_column(&table.columns, &name)
                })
                .collect::<Result<Vec<_>>>()?
        };

        let empty = EvalContext::empty();
        let mut pending = Vec::with_capacity(rows.len());
        for expressions in rows {
            if expressions.len() != target_indexes.len() {
                return Err(Error::InvalidQuery(format!(
                    "INSERT row has {} value(s), expected {}",
                    expressions.len(),
                    target_indexes.len()
                )));
            }
            let mut row = vec![Value::Null; table.columns.len()];
            for (expression, column_index) in expressions.iter().zip(&target_indexes) {
                let value = evaluate(expression, &empty)?;
                row[*column_index] = coerce(value, &table.columns[*column_index].data_type)?;
            }
            validate_row(&table.columns, &row)?;
            pending.push(row);
        }
        let rows_affected = apply_insert_plan(table, pending, conflict_plan)?;
        if rows_affected > 0 {
            catalog.mark_changed();
        }
        Ok(ExecutionResult::Command {
            tag: "INSERT",
            rows_affected,
        })
    }

    fn delete(
        &self,
        from: Vec<TableWithJoins>,
        selection: Option<Expr>,
    ) -> Result<ExecutionResult> {
        if from.len() != 1 || !from[0].joins.is_empty() {
            return Err(Error::Unsupported(
                "DELETE with joins or multiple tables".into(),
            ));
        }
        let table_name = table_factor_name(&from[0].relation)?;
        let mut catalog = self.catalog.write().map_err(|_| Error::LockPoisoned)?;
        let table = catalog
            .tables
            .get_mut(&table_name)
            .ok_or_else(|| Error::TableNotFound(table_name.clone()))?;

        // Evaluate the entire predicate before mutating storage so a row-level
        // error cannot leave a partially applied DELETE.
        let mut should_delete = Vec::with_capacity(table.rows.len());
        for row in &table.rows {
            let context = EvalContext::new(&table.columns, row);
            let delete = match &selection {
                Some(expression) => evaluate(expression, &context)?.as_bool()?.unwrap_or(false),
                None => true,
            };
            should_delete.push(delete);
        }
        let rows_affected = should_delete.iter().filter(|delete| **delete).count();
        let mut index = 0;
        table.rows.retain(|_| {
            let retain = !should_delete[index];
            index += 1;
            retain
        });
        rebuild_indexes(table);
        if rows_affected > 0 {
            catalog.mark_changed();
        }
        Ok(ExecutionResult::Command {
            tag: "DELETE",
            rows_affected,
        })
    }

    fn update(
        &self,
        source: TableWithJoins,
        assignments: Vec<Assignment>,
        selection: Option<Expr>,
    ) -> Result<ExecutionResult> {
        if !source.joins.is_empty() {
            return Err(Error::Unsupported("UPDATE with joins".into()));
        }
        if assignments.is_empty() {
            return Err(Error::InvalidQuery(
                "UPDATE requires at least one assignment".into(),
            ));
        }
        let table_name = table_factor_name(&source.relation)?;
        let mut catalog = self.catalog.write().map_err(|_| Error::LockPoisoned)?;
        let table = catalog
            .tables
            .get_mut(&table_name)
            .ok_or_else(|| Error::TableNotFound(table_name.clone()))?;

        let mut seen = HashSet::new();
        let assignment_indexes = assignments
            .iter()
            .map(|assignment| {
                let identifier = assignment
                    .id
                    .last()
                    .ok_or_else(|| Error::InvalidQuery("empty UPDATE assignment".into()))?;
                let name = ident_name(identifier);
                if !seen.insert(name.clone()) {
                    return Err(Error::DuplicateColumn(name));
                }
                find_column(&table.columns, &name)
            })
            .collect::<Result<Vec<_>>>()?;

        // SQL assignments are simultaneous: every right-hand expression sees
        // the original row. Build and validate all replacement rows first.
        let mut replacement_rows = Vec::with_capacity(table.rows.len());
        let mut rows_affected = 0;
        for row in &table.rows {
            let context = EvalContext::new(&table.columns, row);
            let matches = match &selection {
                Some(expression) => evaluate(expression, &context)?.as_bool()?.unwrap_or(false),
                None => true,
            };
            let mut replacement = row.clone();
            if matches {
                for (assignment, index) in assignments.iter().zip(&assignment_indexes) {
                    let value = evaluate(&assignment.value, &context)?;
                    replacement[*index] = coerce(value, &table.columns[*index].data_type)?;
                }
                validate_row(&table.columns, &replacement)?;
                rows_affected += 1;
            }
            replacement_rows.push(replacement);
        }

        let empty_table = Table::new(table.columns.clone(), Vec::new(), HashMap::new());
        validate_unique(&empty_table, &replacement_rows)?;
        table.rows = replacement_rows;
        rebuild_indexes(table);
        if rows_affected > 0 {
            catalog.mark_changed();
        }
        Ok(ExecutionResult::Command {
            tag: "UPDATE",
            rows_affected,
        })
    }

    fn drop_tables(&self, names: Vec<ObjectName>, if_exists: bool) -> Result<ExecutionResult> {
        let names = names.iter().map(object_name).collect::<Vec<_>>();
        let mut catalog = self.catalog.write().map_err(|_| Error::LockPoisoned)?;
        if !if_exists {
            if let Some(missing) = names
                .iter()
                .find(|name| !catalog.tables.contains_key(*name))
            {
                return Err(Error::TableNotFound(missing.clone()));
            }
        }
        let mut rows_affected = 0;
        for name in names {
            if catalog.tables.remove(&name).is_some() {
                rows_affected += 1;
            }
        }
        if rows_affected > 0 {
            catalog.mark_changed();
        }
        Ok(ExecutionResult::Command {
            tag: "DROP TABLE",
            rows_affected,
        })
    }

    fn create_index(
        &self,
        name: Option<ObjectName>,
        table_name: ObjectName,
        columns: Vec<OrderByExpr>,
        if_not_exists: bool,
    ) -> Result<ExecutionResult> {
        let name = name
            .as_ref()
            .map(object_name)
            .ok_or_else(|| Error::InvalidQuery("CREATE INDEX requires a name".into()))?;
        if columns.len() != 1 {
            return Err(Error::Unsupported(
                "multi-column and expression indexes".into(),
            ));
        }
        let column_name = match &columns[0].expr {
            Expr::Identifier(identifier) => ident_name(identifier),
            _ => return Err(Error::Unsupported("expression indexes".into())),
        };
        let table_name = object_name(&table_name);
        let mut catalog = self.catalog.write().map_err(|_| Error::LockPoisoned)?;
        let exists = catalog
            .tables
            .values()
            .any(|table| table.indexes.contains_key(&name));
        if exists {
            if if_not_exists {
                return Ok(ExecutionResult::Command {
                    tag: "CREATE INDEX",
                    rows_affected: 0,
                });
            }
            return Err(Error::IndexAlreadyExists(name));
        }

        let table = catalog
            .tables
            .get_mut(&table_name)
            .ok_or_else(|| Error::TableNotFound(table_name.clone()))?;
        let column = find_column(&table.columns, &column_name)?;
        if matches!(table.columns[column].data_type, DataType::Vector(_)) {
            return Err(Error::Unsupported(
                "hash indexes on VECTOR columns; index a scalar filter column".into(),
            ));
        }
        let mut index = HashIndex::new(column);
        index.rebuild(&table.rows);
        table.indexes.insert(name, index);
        catalog.mark_changed();
        Ok(ExecutionResult::Command {
            tag: "CREATE INDEX",
            rows_affected: 0,
        })
    }

    fn drop_indexes(&self, names: Vec<ObjectName>, if_exists: bool) -> Result<ExecutionResult> {
        let names = names.iter().map(object_name).collect::<Vec<_>>();
        let mut catalog = self.catalog.write().map_err(|_| Error::LockPoisoned)?;
        if !if_exists {
            for name in &names {
                let exists = catalog
                    .tables
                    .values()
                    .any(|table| table.indexes.contains_key(name));
                if !exists {
                    return Err(Error::IndexNotFound(name.clone()));
                }
            }
        }
        let mut rows_affected = 0;
        for name in names {
            for table in catalog.tables.values_mut() {
                if table.indexes.remove(&name).is_some() {
                    rows_affected += 1;
                    break;
                }
            }
        }
        if rows_affected > 0 {
            catalog.mark_changed();
        }
        Ok(ExecutionResult::Command {
            tag: "DROP INDEX",
            rows_affected,
        })
    }
}

fn next_durable_sequence(sequence: u64) -> Result<u64> {
    sequence
        .checked_add(1)
        .ok_or_else(|| Error::StorageIo("durable sequence exhausted".into()))
}

fn explain_query(catalog: &Catalog, query: &Query, verbose: bool) -> Result<QueryResult> {
    let select = match query.body.as_ref() {
        SetExpr::Select(select) => select,
        _ => return Err(Error::Unsupported("EXPLAIN for set operations".into())),
    };
    validate_select(select)?;
    let mut plan = Vec::new();
    let (table, columns, index_covers_filter) = match select.from.as_slice() {
        [] => {
            plan.push("Source: single row".to_string());
            (None, &[][..], false)
        }
        [from] if from.joins.is_empty() => {
            let name = table_factor_name(&from.relation)?;
            let table = catalog
                .tables
                .get(&name)
                .ok_or_else(|| Error::TableNotFound(name.clone()))?;
            let indexed = select
                .selection
                .as_ref()
                .and_then(|selection| indexed_candidate_rows(table, selection));
            let index_covers_filter = indexed.as_ref().is_some_and(|candidates| candidates.exact);
            if let Some(indexes) = indexed {
                plan.push(format!(
                    "Scan: scalar hash index on {name} ({} of {} row(s))",
                    indexes.rows.len(),
                    table.rows.len()
                ));
            } else {
                plan.push(format!(
                    "Scan: sequential on {name} ({} row(s))",
                    table.rows.len()
                ));
            }
            (Some(table), table.columns.as_slice(), index_covers_filter)
        }
        _ => return Err(Error::Unsupported("EXPLAIN for joins".into())),
    };
    if let Some(selection) = &select.selection {
        validate_expression_columns(selection, columns)?;
        if index_covers_filter {
            plan.push(format!(
                "Filter: {selection} (covered by scalar hash index)"
            ));
        } else {
            plan.push(format!("Filter: {selection}"));
        }
    }

    let projection = build_projection(&select.projection, columns)?;
    let group_by = match &select.group_by {
        sqlparser::ast::GroupByExpr::All => return Err(Error::Unsupported("GROUP BY ALL".into())),
        sqlparser::ast::GroupByExpr::Expressions(expressions) => expressions.as_slice(),
    };
    validate_aggregate_placement(select, group_by)?;
    let aggregate = !group_by.is_empty()
        || projection
            .iter()
            .any(|item| expression_contains_aggregate(&item.expression))
        || select
            .having
            .as_ref()
            .is_some_and(expression_contains_aggregate);
    if aggregate {
        if group_by.is_empty() {
            plan.push("Aggregate: global".into());
        } else {
            plan.push(format!(
                "Aggregate: group by {}",
                group_by
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if let Some(having) = &select.having {
            plan.push(format!("Having: {having}"));
        }
    }
    plan.push(format!(
        "Projection: {}",
        projection
            .iter()
            .map(|item| item.label.clone())
            .collect::<Vec<_>>()
            .join(", ")
    ));

    let empty = EvalContext::empty();
    let offset = query
        .offset
        .as_ref()
        .map(|offset| usize_expression(&offset.value, &empty, "OFFSET"))
        .transpose()?
        .unwrap_or(0);
    let limit = query
        .limit
        .as_ref()
        .map(|limit| usize_expression(limit, &empty, "LIMIT"))
        .transpose()?;
    let result_columns = projection
        .iter()
        .map(|item| item.label.clone())
        .collect::<Vec<_>>();
    let fast_vector_plan = if !aggregate {
        match limit {
            Some(limit) => FastVectorTopKPlan::build(
                select,
                query,
                columns,
                &projection,
                &result_columns,
                offset,
                limit,
            )?,
            None => None,
        }
    } else {
        None
    };
    if !query.order_by.is_empty() {
        let order = query
            .order_by
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        if let Some(limit) = limit {
            let retained = offset
                .checked_add(limit)
                .ok_or_else(|| Error::InvalidQuery("OFFSET plus LIMIT is too large".into()))?;
            if let Some(vector_plan) = &fast_vector_plan {
                plan.push(format!(
                    "VectorTopK: {order} (direct scoring on {}; deferred projection; retain {retained} row(s))",
                    columns[vector_plan.vector_column].name
                ));
            } else {
                plan.push(format!("TopK: {order} (retain {retained} row(s))"));
            }
        } else {
            plan.push(format!("Sort: {order}"));
        }
    }
    if offset != 0 {
        plan.push(format!("Offset: {offset}"));
    }
    if let Some(limit) = limit {
        plan.push(format!("Limit: {limit}"));
    }
    if verbose {
        plan.push(format!(
            "Catalog: {} table(s), source present: {}",
            catalog.tables.len(),
            table.is_some()
        ));
    }

    Ok(QueryResult {
        columns: vec!["plan".into()],
        column_types: vec![Some(DataType::Text)],
        rows: plan
            .into_iter()
            .map(|step| vec![Value::Text(step)])
            .collect(),
        rows_examined: 0,
    })
}

fn analyze_query_intent(catalog: &Catalog, query: &Query) -> Result<QueryIntent> {
    if query.with.is_some()
        || !query.limit_by.is_empty()
        || query.fetch.is_some()
        || !query.locks.is_empty()
        || query.for_clause.is_some()
    {
        return Err(Error::Unsupported(
            "CTEs, LIMIT BY, FETCH, row locks, and FOR clauses".into(),
        ));
    }
    let select = match query.body.as_ref() {
        SetExpr::Select(select) => select,
        _ => {
            return Err(Error::Unsupported(
                "query intent for set operations and nested queries".into(),
            ))
        }
    };
    validate_select(select)?;
    let (table_name, schema) = match select.from.as_slice() {
        [] => (None, &[][..]),
        [from] if from.joins.is_empty() => {
            let name = table_factor_name(&from.relation)?;
            let table = catalog
                .tables
                .get(&name)
                .ok_or_else(|| Error::TableNotFound(name.clone()))?;
            (Some(name), table.columns.as_slice())
        }
        _ => {
            return Err(Error::Unsupported(
                "query intent for joins and multiple FROM items".into(),
            ))
        }
    };
    let projection = build_projection(&select.projection, schema)?;
    for item in &projection {
        validate_expression_columns(&item.expression, schema)?;
    }
    if let Some(selection) = &select.selection {
        validate_expression_columns(selection, schema)?;
        let selection_type = expression_data_type(selection, schema)?;
        ensure_boolean_type(&selection_type)?;
    }

    let result_columns = projection
        .iter()
        .map(|item| item.label.clone())
        .collect::<Vec<_>>();
    for order in &query.order_by {
        let expression = resolve_order_expression(order, &projection, &result_columns);
        validate_expression_columns(expression, schema)?;
        let order_type = expression_data_type(expression, schema)?;
        ensure_sortable_scalar_type(&order_type)?;
    }
    let empty = EvalContext::empty();
    let offset = query
        .offset
        .as_ref()
        .map(|offset| usize_expression(&offset.value, &empty, "OFFSET"))
        .transpose()?
        .unwrap_or(0);
    let limit = query
        .limit
        .as_ref()
        .map(|limit| usize_expression(limit, &empty, "LIMIT"))
        .transpose()?;

    let group_by = match &select.group_by {
        sqlparser::ast::GroupByExpr::All => return Err(Error::Unsupported("GROUP BY ALL".into())),
        sqlparser::ast::GroupByExpr::Expressions(expressions) => expressions.as_slice(),
    };
    validate_aggregate_placement(select, group_by)?;
    let aggregate = !group_by.is_empty()
        || projection
            .iter()
            .any(|item| expression_contains_aggregate(&item.expression))
        || select
            .having
            .as_ref()
            .is_some_and(expression_contains_aggregate);
    if aggregate {
        for expression in group_by {
            validate_expression_columns(expression, schema)?;
            expression_data_type(expression, schema)?;
        }
        for item in &projection {
            validate_group_expression(&item.expression, group_by, schema)?;
        }
        if let Some(having) = &select.having {
            validate_group_expression(having, group_by, schema)?;
            let having_type = expression_data_type(having, schema)?;
            ensure_boolean_type(&having_type)?;
        }
        for order in &query.order_by {
            let expression = resolve_order_expression(order, &projection, &result_columns);
            validate_group_expression(expression, group_by, schema)?;
        }
    } else if select.having.is_some() {
        return Err(Error::InvalidQuery(
            "HAVING requires GROUP BY or an aggregate expression".into(),
        ));
    }
    let optimized = match (aggregate, limit) {
        (false, Some(limit)) => FastVectorTopKPlan::build(
            select,
            query,
            schema,
            &projection,
            &result_columns,
            offset,
            limit,
        )?
        .is_some(),
        _ => false,
    };
    let vector_search = query
        .order_by
        .iter()
        .find_map(|order| {
            let expression = resolve_order_expression(order, &projection, &result_columns);
            match vector_expression(expression, schema) {
                Ok(Some((column, dimensions, metric))) => Some(Ok(VectorQueryIntent {
                    metric: metric.intent_name().into(),
                    column: schema[column].name.clone(),
                    dimensions,
                    descending: order.asc == Some(false),
                    optimized,
                })),
                Ok(None) => None,
                Err(error) => Some(Err(error)),
            }
        })
        .transpose()?;

    let columns = projection
        .iter()
        .map(|item| {
            if let Some(index) = simple_column_expression(&item.expression, schema) {
                let column = &schema[index];
                return Ok(QueryIntentColumn {
                    output_name: item.label.clone(),
                    source_column: Some(column.name.clone()),
                    data_type: Some(column.data_type.clone()),
                    role: column_role(column),
                });
            }
            if vector_expression(&item.expression, schema)?.is_some() {
                return Ok(QueryIntentColumn {
                    output_name: item.label.clone(),
                    source_column: None,
                    data_type: Some(DataType::Float),
                    role: QueryColumnRole::SimilarityScore,
                });
            }
            let data_type = expression_data_type(&item.expression, schema)?;
            Ok(QueryIntentColumn {
                output_name: item.label.clone(),
                source_column: None,
                data_type,
                role: if expression_contains_aggregate(&item.expression) {
                    QueryColumnRole::Aggregate
                } else {
                    QueryColumnRole::Computed
                },
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let distinct = select.distinct.is_some();
    let filter = select.selection.as_ref().map(ToString::to_string);
    let group_by = group_by.iter().map(ToString::to_string).collect::<Vec<_>>();
    let having = select.having.as_ref().map(ToString::to_string);
    let order_by = query.order_by.iter().map(ToString::to_string).collect();
    let summary = intent_summary(IntentSummary {
        table: table_name.as_deref(),
        column_count: columns.len(),
        distinct,
        aggregate,
        filtered: filter.is_some(),
        group_by: &group_by,
        having: having.is_some(),
        limit,
        vector_search: vector_search.as_ref(),
    });
    Ok(QueryIntent {
        table: table_name,
        columns,
        distinct,
        aggregation: aggregate,
        filter,
        group_by,
        having,
        order_by,
        limit,
        offset,
        vector_search,
        summary,
    })
}

fn resolve_order_expression<'a>(
    order: &'a OrderByExpr,
    projection: &'a [Projection],
    result_columns: &[String],
) -> &'a Expr {
    if let Expr::Identifier(identifier) = &order.expr {
        let name = ident_name(identifier);
        if let Some(index) = result_columns
            .iter()
            .position(|label| normalize_name(label) == name)
        {
            return &projection[index].expression;
        }
    }
    &order.expr
}

fn vector_expression(
    expression: &Expr,
    columns: &[Column],
) -> Result<Option<(usize, usize, FastVectorMetric)>> {
    let Some((column, query, metric)) = parse_fast_vector_distance(expression, columns)? else {
        return Ok(None);
    };
    let DataType::Vector(dimensions) = &columns[column].data_type else {
        return Ok(None);
    };
    if query.dimensions() != *dimensions {
        return Err(Error::DimensionMismatch {
            left: *dimensions,
            right: query.dimensions(),
        });
    }
    Ok(Some((column, *dimensions, metric)))
}

fn column_role(column: &Column) -> QueryColumnRole {
    match &column.data_type {
        DataType::Vector(_) => QueryColumnRole::Embedding,
        _ if column.unique => QueryColumnRole::Identifier,
        DataType::Text if content_column_name(&column.name) => QueryColumnRole::Content,
        _ => QueryColumnRole::Attribute,
    }
}

fn content_column_name(name: &str) -> bool {
    const CONTENT_NAMES: &[&str] = &[
        "answer",
        "body",
        "chunk",
        "content",
        "description",
        "document",
        "label",
        "name",
        "question",
        "summary",
        "text",
        "title",
    ];
    let normalized = normalize_name(name);
    CONTENT_NAMES.contains(&normalized.as_str())
        || normalized
            .split('_')
            .any(|part| CONTENT_NAMES.contains(&part))
}

struct IntentSummary<'a> {
    table: Option<&'a str>,
    column_count: usize,
    distinct: bool,
    aggregate: bool,
    filtered: bool,
    group_by: &'a [String],
    having: bool,
    limit: Option<usize>,
    vector_search: Option<&'a VectorQueryIntent>,
}

fn intent_summary(intent: IntentSummary<'_>) -> String {
    let IntentSummary {
        table,
        column_count,
        distinct,
        aggregate,
        filtered,
        group_by,
        having,
        limit,
        vector_search,
    } = intent;
    let columns = if column_count == 1 {
        "1 selected column".into()
    } else {
        format!("{column_count} selected columns")
    };
    let limit = limit.map_or_else(String::new, |limit| {
        let rows = if limit == 1 { "row" } else { "rows" };
        format!(", limited to {limit} {rows}")
    });
    let filter = if filtered {
        " after applying a relational filter"
    } else {
        ""
    };
    let distinct = if distinct { " distinct" } else { "" };
    let grouped = if group_by.is_empty() {
        String::new()
    } else {
        format!(", grouped by {}", group_by.join(", "))
    };
    let having = if having {
        " and filter the resulting groups"
    } else {
        ""
    };
    match (table, vector_search) {
        (Some(table), Some(vector)) => format!(
            "Rank rows from '{table}' by {} on '{}'{filter}; return {columns}{limit}",
            vector.metric, vector.column,
        ),
        (Some(table), None) if aggregate => format!(
            "Aggregate rows from '{table}'{filter}{grouped}{having}; return {columns}{limit}"
        ),
        (Some(table), None) => {
            format!("Read{distinct} {columns} from '{table}'{filter}{limit}")
        }
        (None, _) => format!("Compute {columns}{limit}"),
    }
}

fn run_query(catalog: &Catalog, query: &Query) -> Result<QueryResult> {
    if query.with.is_some()
        || !query.limit_by.is_empty()
        || query.fetch.is_some()
        || !query.locks.is_empty()
        || query.for_clause.is_some()
    {
        return Err(Error::Unsupported(
            "CTEs, LIMIT BY, FETCH, row locks, and FOR clauses".into(),
        ));
    }
    let select = match query.body.as_ref() {
        SetExpr::Select(select) => select,
        _ => {
            return Err(Error::Unsupported(
                "set operations and nested queries".into(),
            ))
        }
    };
    validate_select(select)?;

    let table = match select.from.as_slice() {
        [] => None,
        [from] if from.joins.is_empty() => {
            let name = table_factor_name(&from.relation)?;
            Some(
                catalog
                    .tables
                    .get(&name)
                    .ok_or_else(|| Error::TableNotFound(name.clone()))?,
            )
        }
        _ => return Err(Error::Unsupported("joins and multiple FROM items".into())),
    };
    let columns = table.map_or(&[][..], |table| table.columns.as_slice());
    let indexed_rows = table.and_then(|table| {
        select
            .selection
            .as_ref()
            .and_then(|selection| indexed_candidate_rows(table, selection))
    });
    let residual_selection = match &indexed_rows {
        Some(candidates) if candidates.exact => None,
        _ => select.selection.as_ref(),
    };
    let rows_examined = table.map_or(1, |table| {
        indexed_rows
            .as_ref()
            .map_or(table.rows.len(), |candidates| candidates.rows.len())
    });
    let singleton = Vec::new();
    let source_rows: Box<dyn Iterator<Item = &[Value]> + '_> = match (table, &indexed_rows) {
        (None, _) => Box::new(std::iter::once(singleton.as_slice())),
        (Some(table), Some(candidates)) => Box::new(
            candidates
                .rows
                .iter()
                .map(|index| table.rows[*index].as_slice()),
        ),
        (Some(table), None) => Box::new(table.rows.iter().map(Vec::as_slice)),
    };

    let projection = build_projection(&select.projection, columns)?;
    let result_columns = projection
        .iter()
        .map(|item| item.label.clone())
        .collect::<Vec<_>>();
    let result_types = projection
        .iter()
        .map(|item| expression_data_type(&item.expression, columns))
        .collect::<Result<Vec<_>>>()?;
    for item in &projection {
        validate_expression_columns(&item.expression, columns)?;
    }
    if let Some(selection) = &select.selection {
        validate_expression_columns(selection, columns)?;
        let selection_type = expression_data_type(selection, columns)?;
        ensure_boolean_type(&selection_type)?;
    }
    for order in &query.order_by {
        let is_alias = match &order.expr {
            Expr::Identifier(identifier) => {
                let name = ident_name(identifier);
                result_columns
                    .iter()
                    .any(|label| normalize_name(label) == name)
            }
            _ => false,
        };
        if !is_alias {
            validate_expression_columns(&order.expr, columns)?;
        }
        let expression = resolve_order_expression(order, &projection, &result_columns);
        let order_type = expression_data_type(expression, columns)?;
        ensure_sortable_scalar_type(&order_type)?;
    }
    let empty = EvalContext::empty();
    let offset = match &query.offset {
        Some(offset) => usize_expression(&offset.value, &empty, "OFFSET")?,
        None => 0,
    };
    let limit = match &query.limit {
        Some(expression) => Some(usize_expression(expression, &empty, "LIMIT")?),
        None => None,
    };
    let group_by = match &select.group_by {
        sqlparser::ast::GroupByExpr::All => return Err(Error::Unsupported("GROUP BY ALL".into())),
        sqlparser::ast::GroupByExpr::Expressions(expressions) => expressions.as_slice(),
    };
    validate_aggregate_placement(select, group_by)?;
    for expression in group_by {
        expression_data_type(expression, columns)?;
    }
    if let Some(having) = &select.having {
        let having_type = expression_data_type(having, columns)?;
        ensure_boolean_type(&having_type)?;
    }
    let aggregate_mode = !group_by.is_empty()
        || projection
            .iter()
            .any(|item| expression_contains_aggregate(&item.expression))
        || select
            .having
            .as_ref()
            .is_some_and(expression_contains_aggregate);
    if aggregate_mode {
        return run_aggregate_query(
            select,
            query,
            columns,
            source_rows,
            residual_selection,
            &projection,
            result_columns,
            result_types,
            group_by,
            offset,
            limit,
            rows_examined,
        );
    }
    if select.having.is_some() {
        return Err(Error::InvalidQuery(
            "HAVING requires GROUP BY or an aggregate expression".into(),
        ));
    }
    if let (Some(table), Some(limit)) = (table, limit) {
        if let Some(plan) = FastVectorTopKPlan::build(
            select,
            query,
            columns,
            &projection,
            &result_columns,
            offset,
            limit,
        )? {
            return run_fast_vector_top_k(
                table,
                indexed_rows
                    .as_ref()
                    .map(|candidates| candidates.rows.as_slice()),
                residual_selection,
                plan,
                result_columns,
                result_types,
                rows_examined,
            );
        }
    }
    let mut candidates =
        CandidateSink::new(&query.order_by, offset, limit, select.distinct.is_some())?;
    for row in source_rows {
        let context = EvalContext::new(columns, row);
        if let Some(selection) = residual_selection {
            if !evaluate(selection, &context)?.as_bool()?.unwrap_or(false) {
                continue;
            }
        }
        let values = projection
            .iter()
            .map(|item| evaluate(&item.expression, &context))
            .collect::<Result<Vec<_>>>()?;
        let order = query
            .order_by
            .iter()
            .map(|item| evaluate_order(item, &context, &result_columns, &values))
            .collect::<Result<Vec<_>>>()?;
        candidates.push(Candidate { values, order });
    }

    finish_candidates(
        candidates.into_candidates(),
        (result_columns, result_types),
        false,
        &query.order_by,
        offset,
        limit,
        rows_examined,
    )
}

fn finish_candidates(
    mut candidates: Vec<Candidate>,
    result_schema: (Vec<String>, Vec<Option<DataType>>),
    distinct_results: bool,
    order_by: &[OrderByExpr],
    offset: usize,
    limit: Option<usize>,
    rows_examined: usize,
) -> Result<QueryResult> {
    let (result_columns, result_types) = result_schema;
    if distinct_results {
        let mut distinct = Vec::with_capacity(candidates.len());
        let mut seen = HashSet::with_capacity(candidates.len());
        for candidate in candidates {
            let key = candidate
                .values
                .iter()
                .map(UniqueKey::from)
                .collect::<Vec<_>>();
            if seen.insert(key) {
                distinct.push(candidate);
            }
        }
        candidates = distinct;
    }

    if !order_by.is_empty() {
        validate_order_values(&candidates, order_by.len())?;
        if let Some(limit) = limit {
            let keep = offset
                .checked_add(limit)
                .ok_or_else(|| Error::InvalidQuery("OFFSET plus LIMIT is too large".into()))?;
            if keep == 0 {
                candidates.clear();
            } else if keep < candidates.len() {
                candidates.select_nth_unstable_by(keep, |left, right| {
                    compare_order(left, right, order_by)
                });
                candidates.truncate(keep);
            }
        }
        candidates.sort_by(|left, right| compare_order(left, right, order_by));
    }
    let rows = candidates
        .into_iter()
        .skip(offset)
        .take(limit.unwrap_or(usize::MAX))
        .map(|candidate| candidate.values)
        .collect();

    Ok(QueryResult {
        columns: result_columns,
        column_types: result_types,
        rows,
        rows_examined,
    })
}

#[derive(Clone, Copy, Debug)]
enum AggregateKind {
    Count,
    Sum,
    Average,
    Minimum,
    Maximum,
}

#[derive(Clone, Copy)]
struct AggregateSpec<'a> {
    kind: AggregateKind,
    argument: Option<&'a Expr>,
    distinct: bool,
}

struct AggregateGroup<'a> {
    values: Vec<Value>,
    rows: Vec<&'a [Value]>,
}

#[allow(clippy::too_many_arguments)]
fn run_aggregate_query<'a>(
    select: &Select,
    query: &Query,
    columns: &[Column],
    source_rows: Box<dyn Iterator<Item = &'a [Value]> + 'a>,
    selection: Option<&Expr>,
    projection: &[Projection],
    result_columns: Vec<String>,
    result_types: Vec<Option<DataType>>,
    group_by: &[Expr],
    offset: usize,
    limit: Option<usize>,
    rows_examined: usize,
) -> Result<QueryResult> {
    for expression in group_by {
        validate_expression_columns(expression, columns)?;
    }
    for item in projection {
        validate_group_expression(&item.expression, group_by, columns)?;
    }
    if let Some(having) = &select.having {
        validate_group_expression(having, group_by, columns)?;
    }
    for order in &query.order_by {
        let is_alias = match &order.expr {
            Expr::Identifier(identifier) => {
                let name = ident_name(identifier);
                result_columns
                    .iter()
                    .any(|label| normalize_name(label) == name)
            }
            _ => false,
        };
        if !is_alias {
            validate_group_expression(&order.expr, group_by, columns)?;
        }
    }

    let mut groups = if group_by.is_empty() {
        vec![AggregateGroup {
            values: Vec::new(),
            rows: Vec::new(),
        }]
    } else {
        Vec::new()
    };
    let mut group_positions = HashMap::<Vec<UniqueKey>, usize>::new();
    for row in source_rows {
        let context = EvalContext::new(columns, row);
        if let Some(selection) = selection {
            if !evaluate(selection, &context)?.as_bool()?.unwrap_or(false) {
                continue;
            }
        }
        if group_by.is_empty() {
            groups[0].rows.push(row);
            continue;
        }
        let values = group_by
            .iter()
            .map(|expression| evaluate(expression, &context))
            .collect::<Result<Vec<_>>>()?;
        let key = values.iter().map(UniqueKey::from).collect::<Vec<_>>();
        let position = match group_positions.get(&key) {
            Some(position) => *position,
            None => {
                let position = groups.len();
                groups.push(AggregateGroup {
                    values,
                    rows: Vec::new(),
                });
                group_positions.insert(key, position);
                position
            }
        };
        groups[position].rows.push(row);
    }

    let mut candidates = Vec::with_capacity(groups.len());
    for group in &groups {
        if let Some(having) = &select.having {
            let matches = evaluate_group_expression(having, group_by, group, columns)?
                .as_bool()?
                .unwrap_or(false);
            if !matches {
                continue;
            }
        }
        let values = projection
            .iter()
            .map(|item| evaluate_group_expression(&item.expression, group_by, group, columns))
            .collect::<Result<Vec<_>>>()?;
        let order = query
            .order_by
            .iter()
            .map(|order| {
                aggregate_order_value(
                    &order.expr,
                    projection,
                    &result_columns,
                    &values,
                    group_by,
                    group,
                    columns,
                )
            })
            .collect::<Result<Vec<_>>>()?;
        candidates.push(Candidate { values, order });
    }

    finish_candidates(
        candidates,
        (result_columns, result_types),
        select.distinct.is_some(),
        &query.order_by,
        offset,
        limit,
        rows_examined,
    )
}

#[allow(clippy::too_many_arguments)]
fn aggregate_order_value(
    expression: &Expr,
    projection: &[Projection],
    labels: &[String],
    values: &[Value],
    group_by: &[Expr],
    group: &AggregateGroup<'_>,
    columns: &[Column],
) -> Result<Value> {
    if let Expr::Identifier(identifier) = expression {
        let name = ident_name(identifier);
        if let Some(index) = labels
            .iter()
            .position(|label| normalize_name(label) == name)
        {
            return Ok(values[index].clone());
        }
    }
    if let Some(index) = projection
        .iter()
        .position(|item| item.expression == *expression)
    {
        return Ok(values[index].clone());
    }
    evaluate_group_expression(expression, group_by, group, columns)
}

fn aggregate_function(expression: &Expr) -> Option<&Function> {
    let Expr::Function(function) = expression else {
        return None;
    };
    matches!(
        object_name(&function.name).as_str(),
        "count" | "sum" | "avg" | "min" | "max"
    )
    .then_some(function)
}

fn expression_contains_aggregate(expression: &Expr) -> bool {
    if aggregate_function(expression).is_some() {
        return true;
    }
    match expression {
        Expr::Array(array) => array.elem.iter().any(expression_contains_aggregate),
        Expr::Function(function) => function.args.iter().any(|argument| {
            matches!(
                argument,
                FunctionArg::Unnamed(FunctionArgExpr::Expr(expression))
                    if expression_contains_aggregate(expression)
            )
        }),
        Expr::BinaryOp { left, right, .. } => {
            expression_contains_aggregate(left) || expression_contains_aggregate(right)
        }
        Expr::UnaryOp { expr, .. }
        | Expr::Nested(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr)
        | Expr::IsTrue(expr)
        | Expr::IsFalse(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsNotFalse(expr)
        | Expr::Cast { expr, .. } => expression_contains_aggregate(expr),
        Expr::Between {
            expr, low, high, ..
        } => {
            expression_contains_aggregate(expr)
                || expression_contains_aggregate(low)
                || expression_contains_aggregate(high)
        }
        Expr::InList { expr, list, .. } => {
            expression_contains_aggregate(expr) || list.iter().any(expression_contains_aggregate)
        }
        Expr::Like { expr, pattern, .. } | Expr::ILike { expr, pattern, .. } => {
            expression_contains_aggregate(expr) || expression_contains_aggregate(pattern)
        }
        _ => false,
    }
}

fn validate_aggregate_placement(select: &Select, group_by: &[Expr]) -> Result<()> {
    if select
        .selection
        .as_ref()
        .is_some_and(expression_contains_aggregate)
    {
        return Err(Error::InvalidQuery(
            "aggregate functions are not allowed in WHERE".into(),
        ));
    }
    if group_by.iter().any(expression_contains_aggregate) {
        return Err(Error::InvalidQuery(
            "aggregate functions are not allowed in GROUP BY".into(),
        ));
    }
    Ok(())
}

fn parse_aggregate(function: &Function) -> Result<AggregateSpec<'_>> {
    if function.filter.is_some()
        || function.over.is_some()
        || !function.order_by.is_empty()
        || function.special
    {
        return Err(Error::Unsupported(format!(
            "aggregate modifiers on {}",
            function.name
        )));
    }
    let name = object_name(&function.name);
    let kind = match name.as_str() {
        "count" => AggregateKind::Count,
        "sum" => AggregateKind::Sum,
        "avg" => AggregateKind::Average,
        "min" => AggregateKind::Minimum,
        "max" => AggregateKind::Maximum,
        _ => return Err(Error::Unsupported(format!("aggregate {name}"))),
    };
    if function.args.len() != 1 {
        return Err(Error::InvalidQuery(format!(
            "{name} expects one argument, received {}",
            function.args.len()
        )));
    }
    let argument = match &function.args[0] {
        FunctionArg::Unnamed(FunctionArgExpr::Expr(expression)) => Some(expression),
        FunctionArg::Unnamed(FunctionArgExpr::Wildcard)
        | FunctionArg::Unnamed(FunctionArgExpr::QualifiedWildcard(_))
            if matches!(kind, AggregateKind::Count) && !function.distinct =>
        {
            None
        }
        FunctionArg::Unnamed(FunctionArgExpr::Wildcard)
        | FunctionArg::Unnamed(FunctionArgExpr::QualifiedWildcard(_)) => {
            return Err(Error::InvalidQuery(format!(
                "{name}(DISTINCT *) is not supported"
            )))
        }
        FunctionArg::Named { .. } => {
            return Err(Error::Unsupported("named aggregate arguments".into()))
        }
    };
    if argument.is_some_and(expression_contains_aggregate) {
        return Err(Error::InvalidQuery(
            "nested aggregate functions are not supported".into(),
        ));
    }
    Ok(AggregateSpec {
        kind,
        argument,
        distinct: function.distinct,
    })
}

fn evaluate_aggregate(
    spec: AggregateSpec<'_>,
    rows: &[&[Value]],
    columns: &[Column],
) -> Result<Value> {
    if spec.argument.is_none() {
        let count = i64::try_from(rows.len())
            .map_err(|_| Error::InvalidQuery("COUNT result is too large".into()))?;
        return Ok(Value::Integer(count));
    }
    let argument = spec
        .argument
        .ok_or_else(|| Error::InvalidQuery("aggregate argument is missing".into()))?;
    validate_expression_columns(argument, columns)?;
    let mut values = Vec::with_capacity(rows.len());
    let mut seen = HashSet::new();
    for row in rows {
        let value = evaluate(argument, &EvalContext::new(columns, row))?;
        if matches!(value, Value::Null) {
            continue;
        }
        if spec.distinct && !seen.insert(UniqueKey::from(&value)) {
            continue;
        }
        values.push(value);
    }

    match spec.kind {
        AggregateKind::Count => i64::try_from(values.len())
            .map(Value::Integer)
            .map_err(|_| Error::InvalidQuery("COUNT result is too large".into())),
        AggregateKind::Sum => aggregate_sum(&values),
        AggregateKind::Average => aggregate_average(&values),
        AggregateKind::Minimum => aggregate_extreme(values, BinaryOperator::Lt),
        AggregateKind::Maximum => aggregate_extreme(values, BinaryOperator::Gt),
    }
}

fn aggregate_sum(values: &[Value]) -> Result<Value> {
    if values.is_empty() {
        return Ok(Value::Null);
    }
    let mut integer_sum = 0_i64;
    let mut float_sum = 0.0_f64;
    let mut saw_float = false;
    for value in values {
        match value {
            Value::Integer(value) if saw_float => float_sum += *value as f64,
            Value::Integer(value) => {
                integer_sum = integer_sum
                    .checked_add(*value)
                    .ok_or_else(|| Error::InvalidQuery("SUM integer overflow".into()))?;
            }
            Value::Float(value) => {
                if !saw_float {
                    float_sum = integer_sum as f64;
                    saw_float = true;
                }
                float_sum += value;
            }
            value => return Err(type_mismatch("numeric aggregate argument", value)),
        }
    }
    if saw_float {
        if !float_sum.is_finite() {
            return Err(Error::InvalidQuery("non-finite SUM result".into()));
        }
        Ok(Value::Float(float_sum))
    } else {
        Ok(Value::Integer(integer_sum))
    }
}

fn aggregate_average(values: &[Value]) -> Result<Value> {
    if values.is_empty() {
        return Ok(Value::Null);
    }
    let mut sum = 0.0_f64;
    for value in values {
        sum += value
            .as_f64()?
            .ok_or_else(|| Error::InvalidQuery("AVG argument cannot be NULL".into()))?;
    }
    let result = sum / values.len() as f64;
    if !result.is_finite() {
        return Err(Error::InvalidQuery("non-finite AVG result".into()));
    }
    Ok(Value::Float(result))
}

fn aggregate_extreme(mut values: Vec<Value>, operator: BinaryOperator) -> Result<Value> {
    let Some(mut extreme) = values.pop() else {
        return Ok(Value::Null);
    };
    for value in values {
        if compare_values(&value, &extreme, operator.clone())? == Value::Boolean(true) {
            extreme = value;
        }
    }
    if matches!(extreme, Value::Vector(_)) {
        return Err(type_mismatch(
            "sortable scalar aggregate argument",
            &extreme,
        ));
    }
    Ok(extreme)
}

fn validate_group_expression(
    expression: &Expr,
    group_by: &[Expr],
    columns: &[Column],
) -> Result<()> {
    if group_by.contains(expression) {
        return validate_expression_columns(expression, columns);
    }
    if let Some(function) = aggregate_function(expression) {
        let spec = parse_aggregate(function)?;
        if let Some(argument) = spec.argument {
            validate_expression_columns(argument, columns)?;
        }
        return Ok(());
    }
    match expression {
        Expr::Value(_) => Ok(()),
        Expr::Array(array) => {
            for element in &array.elem {
                validate_group_expression(element, group_by, columns)?;
            }
            Ok(())
        }
        Expr::BinaryOp { left, right, .. } => {
            validate_group_expression(left, group_by, columns)?;
            validate_group_expression(right, group_by, columns)
        }
        Expr::UnaryOp { expr, .. }
        | Expr::Nested(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr)
        | Expr::IsTrue(expr)
        | Expr::IsFalse(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsNotFalse(expr)
        | Expr::Cast { expr, .. } => validate_group_expression(expr, group_by, columns),
        Expr::Between {
            expr, low, high, ..
        } => {
            validate_group_expression(expr, group_by, columns)?;
            validate_group_expression(low, group_by, columns)?;
            validate_group_expression(high, group_by, columns)
        }
        Expr::InList { expr, list, .. } => {
            validate_group_expression(expr, group_by, columns)?;
            for item in list {
                validate_group_expression(item, group_by, columns)?;
            }
            Ok(())
        }
        Expr::Like { expr, pattern, .. } | Expr::ILike { expr, pattern, .. } => {
            validate_group_expression(expr, group_by, columns)?;
            validate_group_expression(pattern, group_by, columns)
        }
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) => Err(Error::InvalidQuery(format!(
            "expression '{expression}' must appear in GROUP BY or be aggregated"
        ))),
        Expr::Function(_) => Err(Error::Unsupported(format!(
            "scalar function '{expression}' in aggregate expression"
        ))),
        other => Err(Error::Unsupported(format!(
            "expression '{other}' in aggregate query"
        ))),
    }
}

fn evaluate_group_expression(
    expression: &Expr,
    group_by: &[Expr],
    group: &AggregateGroup<'_>,
    columns: &[Column],
) -> Result<Value> {
    if let Some(index) = group_by
        .iter()
        .position(|group_expression| group_expression == expression)
    {
        return Ok(group.values[index].clone());
    }
    if let Some(function) = aggregate_function(expression) {
        return evaluate_aggregate(parse_aggregate(function)?, &group.rows, columns);
    }
    match expression {
        Expr::Value(value) => sql_literal(value),
        Expr::Nested(expression) => evaluate_group_expression(expression, group_by, group, columns),
        Expr::BinaryOp { left, op, right } => {
            let left = evaluate_group_expression(left, group_by, group, columns)?;
            if matches!((op, left.as_bool()), (BinaryOperator::And, Ok(Some(false)))) {
                return Ok(Value::Boolean(false));
            }
            if matches!((op, left.as_bool()), (BinaryOperator::Or, Ok(Some(true)))) {
                return Ok(Value::Boolean(true));
            }
            let right = evaluate_group_expression(right, group_by, group, columns)?;
            match op {
                BinaryOperator::And => sql_and(left, right),
                BinaryOperator::Or => sql_or(left, right),
                BinaryOperator::Eq
                | BinaryOperator::NotEq
                | BinaryOperator::Gt
                | BinaryOperator::GtEq
                | BinaryOperator::Lt
                | BinaryOperator::LtEq => compare_values(&left, &right, op.clone()),
                BinaryOperator::Plus
                | BinaryOperator::Minus
                | BinaryOperator::Multiply
                | BinaryOperator::Divide
                | BinaryOperator::Modulo => numeric_binary(&left, &right, op),
                BinaryOperator::Custom(operator) => vector_operator(&left, &right, operator),
                _ => Err(Error::Unsupported(format!(
                    "operator {op} in HAVING expression"
                ))),
            }
        }
        Expr::UnaryOp { op, expr } => {
            let value = evaluate_group_expression(expr, group_by, group, columns)?;
            match op {
                UnaryOperator::Not => match value.as_bool()? {
                    Some(value) => Ok(Value::Boolean(!value)),
                    None => Ok(Value::Null),
                },
                UnaryOperator::Plus => match value {
                    Value::Integer(_) | Value::Float(_) | Value::Null => Ok(value),
                    value => Err(type_mismatch("numeric value", &value)),
                },
                UnaryOperator::Minus => match value {
                    Value::Integer(value) => value
                        .checked_neg()
                        .map(Value::Integer)
                        .ok_or_else(|| Error::InvalidQuery("integer overflow".into())),
                    Value::Float(value) => Ok(Value::Float(-value)),
                    Value::Null => Ok(Value::Null),
                    value => Err(type_mismatch("numeric value", &value)),
                },
                _ => Err(Error::Unsupported(format!(
                    "operator {op} in HAVING expression"
                ))),
            }
        }
        Expr::IsNull(expression) => Ok(Value::Boolean(matches!(
            evaluate_group_expression(expression, group_by, group, columns)?,
            Value::Null
        ))),
        Expr::IsNotNull(expression) => Ok(Value::Boolean(!matches!(
            evaluate_group_expression(expression, group_by, group, columns)?,
            Value::Null
        ))),
        Expr::IsTrue(expression) => Ok(Value::Boolean(
            evaluate_group_expression(expression, group_by, group, columns)?.as_bool()?
                == Some(true),
        )),
        Expr::IsFalse(expression) => Ok(Value::Boolean(
            evaluate_group_expression(expression, group_by, group, columns)?.as_bool()?
                == Some(false),
        )),
        Expr::IsNotTrue(expression) => Ok(Value::Boolean(
            evaluate_group_expression(expression, group_by, group, columns)?.as_bool()?
                != Some(true),
        )),
        Expr::IsNotFalse(expression) => Ok(Value::Boolean(
            evaluate_group_expression(expression, group_by, group, columns)?.as_bool()?
                != Some(false),
        )),
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => {
            let value = evaluate_group_expression(expr, group_by, group, columns)?;
            let low = evaluate_group_expression(low, group_by, group, columns)?;
            let high = evaluate_group_expression(high, group_by, group, columns)?;
            let lower = compare_values(&value, &low, BinaryOperator::GtEq)?;
            let upper = compare_values(&value, &high, BinaryOperator::LtEq)?;
            boolean_not_if(sql_and(lower, upper)?, *negated)
        }
        Expr::InList {
            expr,
            list,
            negated,
        } => {
            let value = evaluate_group_expression(expr, group_by, group, columns)?;
            let mut found = false;
            let mut saw_null = matches!(value, Value::Null);
            for item in list {
                let item = evaluate_group_expression(item, group_by, group, columns)?;
                match compare_values(&value, &item, BinaryOperator::Eq)? {
                    Value::Boolean(true) => found = true,
                    Value::Null => saw_null = true,
                    _ => {}
                }
            }
            let result = if found {
                Value::Boolean(true)
            } else if saw_null {
                Value::Null
            } else {
                Value::Boolean(false)
            };
            boolean_not_if(result, *negated)
        }
        Expr::Like {
            negated,
            expr,
            pattern,
            escape_char,
        }
        | Expr::ILike {
            negated,
            expr,
            pattern,
            escape_char,
        } => {
            if escape_char.is_some() {
                return Err(Error::Unsupported("LIKE ... ESCAPE in HAVING".into()));
            }
            let case_insensitive = matches!(expression, Expr::ILike { .. });
            let value = evaluate_group_expression(expr, group_by, group, columns)?;
            let pattern = evaluate_group_expression(pattern, group_by, group, columns)?;
            match (value, pattern) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Text(mut value), Value::Text(mut pattern)) => {
                    if case_insensitive {
                        value = value.to_lowercase();
                        pattern = pattern.to_lowercase();
                    }
                    boolean_not_if(Value::Boolean(like_matches(&value, &pattern)), *negated)
                }
                (left, right) => Err(Error::TypeMismatch {
                    expected: "TEXT LIKE TEXT".into(),
                    found: format!("{} LIKE {}", left.type_name(), right.type_name()),
                }),
            }
        }
        Expr::Cast {
            expr, data_type, ..
        } => coerce(
            evaluate_group_expression(expr, group_by, group, columns)?,
            &parse_data_type(data_type)?,
        ),
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) => Err(Error::InvalidQuery(format!(
            "expression '{expression}' must appear in GROUP BY or be aggregated"
        ))),
        other => Err(Error::Unsupported(format!(
            "expression '{other}' in HAVING"
        ))),
    }
}

fn validate_expression_columns(expression: &Expr, columns: &[Column]) -> Result<()> {
    match expression {
        Expr::Identifier(identifier) => {
            find_column(columns, &ident_name(identifier))?;
        }
        Expr::CompoundIdentifier(identifiers) => {
            let identifier = identifiers
                .last()
                .ok_or_else(|| Error::InvalidQuery("empty identifier".into()))?;
            find_column(columns, &ident_name(identifier))?;
        }
        Expr::Array(array) => {
            for element in &array.elem {
                validate_expression_columns(element, columns)?;
            }
        }
        Expr::Function(function) => {
            for argument in &function.args {
                if let FunctionArg::Unnamed(FunctionArgExpr::Expr(expression)) = argument {
                    validate_expression_columns(expression, columns)?;
                }
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            validate_expression_columns(left, columns)?;
            validate_expression_columns(right, columns)?;
        }
        Expr::UnaryOp { expr, .. }
        | Expr::Nested(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr)
        | Expr::IsTrue(expr)
        | Expr::IsFalse(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsNotFalse(expr) => validate_expression_columns(expr, columns)?,
        Expr::Between {
            expr, low, high, ..
        } => {
            validate_expression_columns(expr, columns)?;
            validate_expression_columns(low, columns)?;
            validate_expression_columns(high, columns)?;
        }
        Expr::InList { expr, list, .. } => {
            validate_expression_columns(expr, columns)?;
            for item in list {
                validate_expression_columns(item, columns)?;
            }
        }
        Expr::Like { expr, pattern, .. } | Expr::ILike { expr, pattern, .. } => {
            validate_expression_columns(expr, columns)?;
            validate_expression_columns(pattern, columns)?;
        }
        Expr::Cast { expr, .. } => validate_expression_columns(expr, columns)?,
        _ => {}
    }
    Ok(())
}

struct IndexedCandidates {
    rows: Vec<usize>,
    exact: bool,
}

fn indexed_candidate_rows(table: &Table, expression: &Expr) -> Option<IndexedCandidates> {
    match expression {
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => match (
            indexed_candidate_rows(table, left),
            indexed_candidate_rows(table, right),
        ) {
            (Some(left), Some(right)) => Some(IndexedCandidates {
                rows: intersect_sorted(&left.rows, &right.rows),
                exact: left.exact && right.exact,
            }),
            (Some(mut candidates), None) | (None, Some(mut candidates)) => {
                candidates.exact = false;
                Some(candidates)
            }
            (None, None) => None,
        },
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Or,
            right,
        } => match (
            indexed_candidate_rows(table, left),
            indexed_candidate_rows(table, right),
        ) {
            (Some(left), Some(right)) => Some(IndexedCandidates {
                rows: union_sorted(&left.rows, &right.rows),
                exact: left.exact && right.exact,
            }),
            _ => None,
        },
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Eq,
            right,
        } => equality_index_lookup(table, left, right)
            .or_else(|| equality_index_lookup(table, right, left))
            .map(|rows| IndexedCandidates { rows, exact: true }),
        Expr::Nested(expression) => indexed_candidate_rows(table, expression),
        _ => None,
    }
}

fn equality_index_lookup(table: &Table, column: &Expr, value: &Expr) -> Option<Vec<usize>> {
    let column_name = match column {
        Expr::Identifier(identifier) => ident_name(identifier),
        Expr::CompoundIdentifier(identifiers) => ident_name(identifiers.last()?),
        _ => return None,
    };
    let column = find_column(&table.columns, &column_name).ok()?;
    let index = table
        .indexes
        .values()
        .find(|index| index.column == column)?;
    let value = evaluate(value, &EvalContext::empty()).ok()?;
    let value = coerce(value, &table.columns[column].data_type).ok()?;
    if matches!(value, Value::Null) {
        return Some(Vec::new());
    }
    if matches!(value, Value::Vector(_)) {
        return None;
    }
    Some(
        index
            .buckets
            .get(&UniqueKey::from(&value))
            .cloned()
            .unwrap_or_default(),
    )
}

fn intersect_sorted(left: &[usize], right: &[usize]) -> Vec<usize> {
    let mut output = Vec::with_capacity(left.len().min(right.len()));
    let (mut left_index, mut right_index) = (0, 0);
    while left_index < left.len() && right_index < right.len() {
        match left[left_index].cmp(&right[right_index]) {
            Ordering::Less => left_index += 1,
            Ordering::Greater => right_index += 1,
            Ordering::Equal => {
                output.push(left[left_index]);
                left_index += 1;
                right_index += 1;
            }
        }
    }
    output
}

fn union_sorted(left: &[usize], right: &[usize]) -> Vec<usize> {
    let mut output = Vec::with_capacity(left.len() + right.len());
    let (mut left_index, mut right_index) = (0, 0);
    while left_index < left.len() || right_index < right.len() {
        let value = match (left.get(left_index), right.get(right_index)) {
            (Some(left), Some(right)) => match left.cmp(right) {
                Ordering::Less => {
                    left_index += 1;
                    *left
                }
                Ordering::Greater => {
                    right_index += 1;
                    *right
                }
                Ordering::Equal => {
                    left_index += 1;
                    right_index += 1;
                    *left
                }
            },
            (Some(left), None) => {
                left_index += 1;
                *left
            }
            (None, Some(right)) => {
                right_index += 1;
                *right
            }
            (None, None) => break,
        };
        output.push(value);
    }
    output
}

fn validate_order_values(candidates: &[Candidate], order_count: usize) -> Result<()> {
    for index in 0..order_count {
        let mut expected = None;
        for candidate in candidates {
            let category = match &candidate.order[index] {
                Value::Null => continue,
                Value::Integer(_) | Value::Float(_) => "numeric",
                Value::Text(_) => "text",
                Value::Boolean(_) => "boolean",
                Value::Vector(_) => {
                    return Err(Error::TypeMismatch {
                        expected: "sortable scalar value".into(),
                        found: "VECTOR".into(),
                    })
                }
            };
            if let Some(expected) = expected {
                if expected != category {
                    return Err(Error::TypeMismatch {
                        expected: format!("{expected} ORDER BY expression"),
                        found: category.into(),
                    });
                }
            } else {
                expected = Some(category);
            }
        }
    }
    Ok(())
}

fn validate_select(select: &Select) -> Result<()> {
    if select.top.is_some()
        || select.into.is_some()
        || !select.lateral_views.is_empty()
        || !select.cluster_by.is_empty()
        || !select.distribute_by.is_empty()
        || !select.sort_by.is_empty()
        || !select.named_window.is_empty()
        || select.qualify.is_some()
    {
        return Err(Error::Unsupported(
            "TOP, SELECT INTO, and advanced SELECT clauses".into(),
        ));
    }
    Ok(())
}

#[derive(Debug)]
struct Projection {
    expression: Expr,
    label: String,
}

fn build_projection(items: &[SelectItem], columns: &[Column]) -> Result<Vec<Projection>> {
    let mut output = Vec::new();
    for item in items {
        match item {
            SelectItem::UnnamedExpr(expression) => output.push(Projection {
                expression: expression.clone(),
                label: expression_label(expression),
            }),
            SelectItem::ExprWithAlias { expr, alias } => output.push(Projection {
                expression: expr.clone(),
                label: ident_name(alias),
            }),
            SelectItem::Wildcard(options) if options.to_string().is_empty() => {
                output.extend(columns.iter().map(|column| Projection {
                    expression: Expr::Identifier(Ident::new(&column.name)),
                    label: column.name.clone(),
                }));
            }
            SelectItem::QualifiedWildcard(_, options) if options.to_string().is_empty() => {
                output.extend(columns.iter().map(|column| Projection {
                    expression: Expr::Identifier(Ident::new(&column.name)),
                    label: column.name.clone(),
                }));
            }
            wildcard => {
                return Err(Error::Unsupported(format!(
                    "wildcard projection {wildcard}"
                )))
            }
        }
    }
    Ok(output)
}

const PARALLEL_VECTOR_SCAN_THRESHOLD: usize = 4_096;

#[derive(Clone, Copy, Debug)]
enum FastVectorMetric {
    L2,
    SquaredL2,
    Cosine,
    DotProduct,
    NegativeDotProduct,
}

impl FastVectorMetric {
    fn intent_name(self) -> &'static str {
        match self {
            Self::L2 => "l2_distance",
            Self::SquaredL2 => "squared_l2_distance",
            Self::Cosine => "cosine_distance",
            Self::DotProduct | Self::NegativeDotProduct => "dot_product",
        }
    }

    fn score(self, vector: &Vector, query: &Vector) -> Result<f64> {
        let score = match self {
            Self::L2 => vector.l2_distance(query)?,
            Self::SquaredL2 => vector.squared_l2_distance(query)?,
            Self::Cosine => vector.cosine_distance(query)?,
            Self::DotProduct => vector.dot_product(query)?,
            Self::NegativeDotProduct => -vector.dot_product(query)?,
        };
        Ok(f64::from(score))
    }
}

#[derive(Clone, Copy, Debug)]
enum FastProjection {
    Column(usize),
    Score,
}

#[derive(Clone, Copy, Debug)]
struct FastSortOrder {
    descending: bool,
    nulls_first: bool,
}

impl FastSortOrder {
    fn new(order: &OrderByExpr) -> Self {
        Self {
            descending: order.asc == Some(false),
            nulls_first: order.nulls_first.unwrap_or(order.asc == Some(false)),
        }
    }

    fn compare(self, left: &Value, right: &Value) -> Ordering {
        match (left, right) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Null, _) => {
                if self.nulls_first {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (_, Value::Null) => {
                if self.nulls_first {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            (left, right) => {
                let ordering = compare_sort_values(left, right);
                if self.descending {
                    ordering.reverse()
                } else {
                    ordering
                }
            }
        }
    }
}

struct FastVectorTopKPlan {
    vector_column: usize,
    query: Vector,
    metric: FastVectorMetric,
    projections: Vec<FastProjection>,
    order: FastSortOrder,
    offset: usize,
    limit: usize,
    capacity: usize,
}

impl FastVectorTopKPlan {
    #[allow(clippy::too_many_arguments)]
    fn build(
        select: &Select,
        query: &Query,
        columns: &[Column],
        projection: &[Projection],
        result_columns: &[String],
        offset: usize,
        limit: usize,
    ) -> Result<Option<Self>> {
        if select.distinct.is_some() || query.order_by.len() != 1 {
            return Ok(None);
        }
        let order = &query.order_by[0];
        let order_expression = match &order.expr {
            Expr::Identifier(identifier) => {
                let name = ident_name(identifier);
                match result_columns
                    .iter()
                    .position(|label| normalize_name(label) == name)
                {
                    Some(index) => &projection[index].expression,
                    None => &order.expr,
                }
            }
            _ => &order.expr,
        };
        let Some((vector_column, query_vector, metric)) =
            parse_fast_vector_distance(order_expression, columns)?
        else {
            return Ok(None);
        };
        let expected_dimensions = match columns[vector_column].data_type {
            DataType::Vector(dimensions) => dimensions,
            _ => return Ok(None),
        };
        if query_vector.dimensions() != expected_dimensions {
            return Err(Error::DimensionMismatch {
                left: expected_dimensions,
                right: query_vector.dimensions(),
            });
        }

        let mut projections = Vec::with_capacity(projection.len());
        for item in projection {
            if item.expression == *order_expression {
                projections.push(FastProjection::Score);
            } else if let Some(column) = simple_column_expression(&item.expression, columns) {
                projections.push(FastProjection::Column(column));
            } else {
                return Ok(None);
            }
        }
        let capacity = offset
            .checked_add(limit)
            .ok_or_else(|| Error::InvalidQuery("OFFSET plus LIMIT is too large".into()))?;
        Ok(Some(Self {
            vector_column,
            query: query_vector,
            metric,
            projections,
            order: FastSortOrder::new(order),
            offset,
            limit,
            capacity,
        }))
    }

    fn score_row<'a>(
        &self,
        columns: &[Column],
        row: &'a [Value],
        selection: Option<&Expr>,
    ) -> Result<Option<FastVectorCandidate<'a>>> {
        let context = EvalContext::new(columns, row);
        if let Some(selection) = selection {
            if !evaluate(selection, &context)?.as_bool()?.unwrap_or(false) {
                return Ok(None);
            }
        }
        let score = match &row[self.vector_column] {
            Value::Null => Value::Null,
            Value::Vector(vector) => Value::Float(self.metric.score(vector, &self.query)?),
            value => return Err(type_mismatch("VECTOR", value)),
        };
        Ok(Some(FastVectorCandidate {
            row,
            score,
            order: self.order,
        }))
    }

    fn materialize(&self, candidate: FastVectorCandidate<'_>) -> Vec<Value> {
        self.projections
            .iter()
            .map(|projection| match projection {
                FastProjection::Column(index) => candidate.row[*index].clone(),
                FastProjection::Score => candidate.score.clone(),
            })
            .collect()
    }
}

fn simple_column_expression(expression: &Expr, columns: &[Column]) -> Option<usize> {
    let identifier = match expression {
        Expr::Identifier(identifier) => identifier,
        Expr::CompoundIdentifier(identifiers) => identifiers.last()?,
        Expr::Nested(expression) => return simple_column_expression(expression, columns),
        _ => return None,
    };
    find_column(columns, &ident_name(identifier)).ok()
}

fn expression_data_type(expression: &Expr, columns: &[Column]) -> Result<Option<DataType>> {
    match expression {
        Expr::Value(value) => Ok(value_data_type(&sql_literal(value)?)),
        Expr::Identifier(identifier) => Ok(Some(
            columns[find_column(columns, &ident_name(identifier))?]
                .data_type
                .clone(),
        )),
        Expr::CompoundIdentifier(identifiers) => {
            let identifier = identifiers
                .last()
                .ok_or_else(|| Error::InvalidQuery("empty identifier".into()))?;
            Ok(Some(
                columns[find_column(columns, &ident_name(identifier))?]
                    .data_type
                    .clone(),
            ))
        }
        Expr::Nested(expression) => expression_data_type(expression, columns),
        Expr::Array(array) => {
            for element in &array.elem {
                let data_type = expression_data_type(element, columns)?;
                ensure_numeric_type(&data_type)?;
            }
            if array.elem.is_empty() {
                return Err(Error::InvalidVectorDimension);
            }
            if array.elem.len() > MAX_VECTOR_DIMENSIONS {
                return Err(Error::VectorDimensionLimit {
                    found: array.elem.len(),
                    max: MAX_VECTOR_DIMENSIONS,
                });
            }
            Ok(Some(DataType::Vector(array.elem.len())))
        }
        Expr::Function(function) => function_data_type(function, columns),
        Expr::BinaryOp { left, op, right } => {
            let left = expression_data_type(left, columns)?;
            let right = expression_data_type(right, columns)?;
            match op {
                BinaryOperator::Eq
                | BinaryOperator::NotEq
                | BinaryOperator::Gt
                | BinaryOperator::GtEq
                | BinaryOperator::Lt
                | BinaryOperator::LtEq => {
                    ensure_comparable_types(&left, &right, op)?;
                    Ok(Some(DataType::Boolean))
                }
                BinaryOperator::And | BinaryOperator::Or => {
                    ensure_boolean_type(&left)?;
                    ensure_boolean_type(&right)?;
                    Ok(Some(DataType::Boolean))
                }
                BinaryOperator::Plus
                | BinaryOperator::Minus
                | BinaryOperator::Multiply
                | BinaryOperator::Divide
                | BinaryOperator::Modulo => numeric_result_type(left, right),
                BinaryOperator::Custom(operator)
                    if matches!(operator.as_str(), "<->" | "<#>" | "<=>") =>
                {
                    ensure_vector_pair(&left, &right)?;
                    Ok(Some(DataType::Float))
                }
                BinaryOperator::Custom(operator) => {
                    Err(Error::Unsupported(format!("custom operator {operator}")))
                }
                operator => Err(Error::Unsupported(format!("binary operator {operator}"))),
            }
        }
        Expr::UnaryOp { op, expr } => {
            let data_type = expression_data_type(expr, columns)?;
            match op {
                UnaryOperator::Not => {
                    ensure_boolean_type(&data_type)?;
                    Ok(Some(DataType::Boolean))
                }
                UnaryOperator::Plus | UnaryOperator::Minus => {
                    ensure_numeric_type(&data_type)?;
                    Ok(data_type)
                }
                operator => Err(Error::Unsupported(format!("unary operator {operator}"))),
            }
        }
        Expr::IsNull(expression) | Expr::IsNotNull(expression) => {
            expression_data_type(expression, columns)?;
            Ok(Some(DataType::Boolean))
        }
        Expr::IsTrue(expression)
        | Expr::IsFalse(expression)
        | Expr::IsNotTrue(expression)
        | Expr::IsNotFalse(expression) => {
            let data_type = expression_data_type(expression, columns)?;
            ensure_boolean_type(&data_type)?;
            Ok(Some(DataType::Boolean))
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            let value = expression_data_type(expr, columns)?;
            let low = expression_data_type(low, columns)?;
            let high = expression_data_type(high, columns)?;
            ensure_comparable_types(&value, &low, &BinaryOperator::GtEq)?;
            ensure_comparable_types(&value, &high, &BinaryOperator::LtEq)?;
            Ok(Some(DataType::Boolean))
        }
        Expr::InList { expr, list, .. } => {
            let value = expression_data_type(expr, columns)?;
            for item in list {
                let item = expression_data_type(item, columns)?;
                ensure_comparable_types(&value, &item, &BinaryOperator::Eq)?;
            }
            Ok(Some(DataType::Boolean))
        }
        Expr::Like { expr, pattern, .. } | Expr::ILike { expr, pattern, .. } => {
            let value = expression_data_type(expr, columns)?;
            let pattern = expression_data_type(pattern, columns)?;
            ensure_text_type(&value)?;
            ensure_text_type(&pattern)?;
            Ok(Some(DataType::Boolean))
        }
        Expr::Cast {
            expr, data_type, ..
        } => {
            expression_data_type(expr, columns)?;
            Ok(Some(parse_data_type(data_type)?))
        }
        other => Err(Error::Unsupported(format!("expression {other}"))),
    }
}

fn function_data_type(function: &Function, columns: &[Column]) -> Result<Option<DataType>> {
    let name = object_name(&function.name);
    if matches!(name.as_str(), "count" | "sum" | "avg" | "min" | "max") {
        let spec = parse_aggregate(function)?;
        let argument_type = spec
            .argument
            .map(|argument| expression_data_type(argument, columns))
            .transpose()?
            .flatten();
        return Ok(match spec.kind {
            AggregateKind::Count => Some(DataType::Integer),
            AggregateKind::Average => {
                ensure_numeric_type(&argument_type)?;
                Some(DataType::Float)
            }
            AggregateKind::Sum => {
                ensure_numeric_type(&argument_type)?;
                argument_type
            }
            AggregateKind::Minimum | AggregateKind::Maximum => {
                ensure_sortable_scalar_type(&argument_type)?;
                argument_type
            }
        });
    }
    if function.distinct
        || function.filter.is_some()
        || function.over.is_some()
        || !function.order_by.is_empty()
    {
        return Err(Error::Unsupported(format!(
            "function modifiers on {}",
            function.name
        )));
    }
    let arguments = function
        .args
        .iter()
        .map(|argument| match argument {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(expression)) => Ok(expression),
            _ => Err(Error::Unsupported(
                "named or wildcard function arguments".into(),
            )),
        })
        .collect::<Result<Vec<_>>>()?;
    let argument_types = arguments
        .iter()
        .map(|argument| expression_data_type(argument, columns))
        .collect::<Result<Vec<_>>>()?;
    match name.as_str() {
        "vector" => {
            if arguments.is_empty() {
                return Err(Error::InvalidVectorDimension);
            }
            if arguments.len() > MAX_VECTOR_DIMENSIONS {
                return Err(Error::VectorDimensionLimit {
                    found: arguments.len(),
                    max: MAX_VECTOR_DIMENSIONS,
                });
            }
            for data_type in &argument_types {
                ensure_numeric_type(data_type)?;
            }
            Ok(Some(DataType::Vector(arguments.len())))
        }
        "vector_dims" | "dimensions" => {
            require_type_argument_count(&name, arguments.len(), 1)?;
            ensure_vector_type(&argument_types[0])?;
            Ok(Some(DataType::Integer))
        }
        "vector_norm" | "norm" => {
            require_type_argument_count(&name, arguments.len(), 1)?;
            ensure_vector_type(&argument_types[0])?;
            Ok(Some(DataType::Float))
        }
        "normalize" | "normalize_vector" => {
            require_type_argument_count(&name, arguments.len(), 1)?;
            ensure_vector_type(&argument_types[0])?;
            Ok(argument_types[0].clone())
        }
        "l2_distance"
        | "euclidean_distance"
        | "squared_l2_distance"
        | "cosine_distance"
        | "dot_product"
        | "inner_product" => {
            require_type_argument_count(&name, arguments.len(), 2)?;
            ensure_vector_pair(&argument_types[0], &argument_types[1])?;
            Ok(Some(DataType::Float))
        }
        _ => Err(Error::Unsupported(format!("function {name}"))),
    }
}

fn require_type_argument_count(name: &str, found: usize, expected: usize) -> Result<()> {
    if found != expected {
        return Err(Error::InvalidQuery(format!(
            "{name} expects {expected} argument(s), received {found}"
        )));
    }
    Ok(())
}

fn numeric_result_type(
    left: Option<DataType>,
    right: Option<DataType>,
) -> Result<Option<DataType>> {
    ensure_numeric_type(&left)?;
    ensure_numeric_type(&right)?;
    Ok(match (left, right) {
        (Some(DataType::Float), _) | (_, Some(DataType::Float)) => Some(DataType::Float),
        (Some(DataType::Integer), Some(DataType::Integer)) => Some(DataType::Integer),
        (Some(data_type), None) | (None, Some(data_type)) => Some(data_type),
        _ => None,
    })
}

fn ensure_numeric_type(data_type: &Option<DataType>) -> Result<()> {
    match data_type {
        None | Some(DataType::Integer | DataType::Float) => Ok(()),
        Some(found) => Err(declared_type_mismatch("numeric value", found)),
    }
}

fn ensure_boolean_type(data_type: &Option<DataType>) -> Result<()> {
    match data_type {
        None | Some(DataType::Boolean) => Ok(()),
        Some(found) => Err(declared_type_mismatch("BOOLEAN", found)),
    }
}

fn ensure_text_type(data_type: &Option<DataType>) -> Result<()> {
    match data_type {
        None | Some(DataType::Text) => Ok(()),
        Some(found) => Err(declared_type_mismatch("TEXT", found)),
    }
}

fn ensure_vector_type(data_type: &Option<DataType>) -> Result<Option<usize>> {
    match data_type {
        None => Ok(None),
        Some(DataType::Vector(dimensions)) => Ok(Some(*dimensions)),
        Some(found) => Err(declared_type_mismatch("VECTOR", found)),
    }
}

fn ensure_vector_pair(left: &Option<DataType>, right: &Option<DataType>) -> Result<()> {
    let left = ensure_vector_type(left)?;
    let right = ensure_vector_type(right)?;
    if let (Some(left), Some(right)) = (left, right) {
        if left != right {
            return Err(Error::DimensionMismatch { left, right });
        }
    }
    Ok(())
}

fn ensure_sortable_scalar_type(data_type: &Option<DataType>) -> Result<()> {
    match data_type {
        None | Some(DataType::Integer | DataType::Float | DataType::Text | DataType::Boolean) => {
            Ok(())
        }
        Some(found) => Err(declared_type_mismatch("sortable scalar value", found)),
    }
}

fn ensure_comparable_types(
    left: &Option<DataType>,
    right: &Option<DataType>,
    operator: &BinaryOperator,
) -> Result<()> {
    let (Some(left), Some(right)) = (left, right) else {
        return Ok(());
    };
    let comparable = matches!(
        (left, right),
        (
            DataType::Integer | DataType::Float,
            DataType::Integer | DataType::Float
        )
    ) || left == right && !matches!(left, DataType::Vector(_))
        || matches!((left, right), (DataType::Vector(_), DataType::Vector(_)))
            && matches!(operator, BinaryOperator::Eq | BinaryOperator::NotEq);
    if comparable {
        Ok(())
    } else {
        Err(Error::TypeMismatch {
            expected: "comparable values".into(),
            found: format!(
                "{} and {}",
                declared_value_type_name(left),
                declared_value_type_name(right)
            ),
        })
    }
}

fn declared_type_mismatch(expected: &str, found: &DataType) -> Error {
    Error::TypeMismatch {
        expected: expected.into(),
        found: declared_value_type_name(found).into(),
    }
}

fn declared_value_type_name(data_type: &DataType) -> &'static str {
    match data_type {
        DataType::Integer => "INTEGER",
        DataType::Float => "FLOAT",
        DataType::Text => "TEXT",
        DataType::Boolean => "BOOLEAN",
        DataType::Vector(_) => "VECTOR",
    }
}

fn value_data_type(value: &Value) -> Option<DataType> {
    match value {
        Value::Null => None,
        Value::Integer(_) => Some(DataType::Integer),
        Value::Float(_) => Some(DataType::Float),
        Value::Text(_) => Some(DataType::Text),
        Value::Boolean(_) => Some(DataType::Boolean),
        Value::Vector(vector) => Some(DataType::Vector(vector.dimensions())),
    }
}

fn parse_fast_vector_distance(
    expression: &Expr,
    columns: &[Column],
) -> Result<Option<(usize, Vector, FastVectorMetric)>> {
    let expression = match expression {
        Expr::Nested(expression) => expression.as_ref(),
        expression => expression,
    };
    let (left, right, metric) = match expression {
        Expr::Function(function)
            if !function.distinct
                && function.filter.is_none()
                && function.over.is_none()
                && function.order_by.is_empty() =>
        {
            let arguments = function
                .args
                .iter()
                .map(|argument| match argument {
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(expression)) => Some(expression),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>();
            let Some(arguments) = arguments else {
                return Ok(None);
            };
            if arguments.len() != 2 {
                return Ok(None);
            }
            let metric = match object_name(&function.name).as_str() {
                "l2_distance" | "euclidean_distance" => FastVectorMetric::L2,
                "squared_l2_distance" => FastVectorMetric::SquaredL2,
                "cosine_distance" => FastVectorMetric::Cosine,
                "dot_product" | "inner_product" => FastVectorMetric::DotProduct,
                _ => return Ok(None),
            };
            (arguments[0], arguments[1], metric)
        }
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Custom(operator),
            right,
        } => {
            let metric = match operator.as_str() {
                "<->" => FastVectorMetric::L2,
                "<#>" => FastVectorMetric::NegativeDotProduct,
                "<=>" => FastVectorMetric::Cosine,
                _ => return Ok(None),
            };
            (left.as_ref(), right.as_ref(), metric)
        }
        _ => return Ok(None),
    };

    for (column_expression, query_expression) in [(left, right), (right, left)] {
        let Some(column) = simple_column_expression(column_expression, columns) else {
            continue;
        };
        if !matches!(columns[column].data_type, DataType::Vector(_)) {
            continue;
        }
        match evaluate(query_expression, &EvalContext::empty()) {
            Ok(Value::Vector(query)) => return Ok(Some((column, query, metric))),
            Ok(_) | Err(Error::ColumnNotFound(_)) => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

struct FastVectorCandidate<'a> {
    row: &'a [Value],
    score: Value,
    order: FastSortOrder,
}

impl PartialEq for FastVectorCandidate<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for FastVectorCandidate<'_> {}

impl PartialOrd for FastVectorCandidate<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for FastVectorCandidate<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.order.compare(&self.score, &other.score)
    }
}

fn push_fast_vector_candidate<'a>(
    heap: &mut BinaryHeap<FastVectorCandidate<'a>>,
    candidate: FastVectorCandidate<'a>,
    capacity: usize,
) {
    if capacity == 0 {
        return;
    }
    if heap.len() < capacity {
        heap.push(candidate);
    } else if heap.peek().is_some_and(|worst| candidate < *worst) {
        heap.pop();
        heap.push(candidate);
    }
}

fn score_and_push_fast_vector_candidate<'a>(
    heap: &mut BinaryHeap<FastVectorCandidate<'a>>,
    row: &'a [Value],
    columns: &[Column],
    selection: Option<&Expr>,
    plan: &FastVectorTopKPlan,
) -> Result<()> {
    if let Some(candidate) = plan.score_row(columns, row, selection)? {
        push_fast_vector_candidate(heap, candidate, plan.capacity);
    }
    Ok(())
}

fn parallel_fast_vector_heap<'a, I>(
    rows: I,
    columns: &[Column],
    selection: Option<&Expr>,
    plan: &FastVectorTopKPlan,
) -> Result<BinaryHeap<FastVectorCandidate<'a>>>
where
    I: ParallelIterator<Item = &'a [Value]>,
{
    rows.try_fold(BinaryHeap::new, |mut heap, row| {
        if let Some(candidate) = plan.score_row(columns, row, selection)? {
            push_fast_vector_candidate(&mut heap, candidate, plan.capacity);
        }
        Ok(heap)
    })
    .try_reduce(BinaryHeap::new, |mut left, right| {
        for candidate in right {
            push_fast_vector_candidate(&mut left, candidate, plan.capacity);
        }
        Ok(left)
    })
}

fn run_fast_vector_top_k(
    table: &Table,
    indexed_rows: Option<&[usize]>,
    selection: Option<&Expr>,
    plan: FastVectorTopKPlan,
    result_columns: Vec<String>,
    result_types: Vec<Option<DataType>>,
    rows_examined: usize,
) -> Result<QueryResult> {
    let source_count = indexed_rows.map_or(table.rows.len(), <[usize]>::len);
    let mut heap = if source_count >= PARALLEL_VECTOR_SCAN_THRESHOLD {
        match indexed_rows {
            Some(indexes) => parallel_fast_vector_heap(
                indexes
                    .par_iter()
                    .map(|index| table.rows[*index].as_slice()),
                &table.columns,
                selection,
                &plan,
            )?,
            None => parallel_fast_vector_heap(
                table.rows.par_iter().map(Vec::as_slice),
                &table.columns,
                selection,
                &plan,
            )?,
        }
    } else {
        let mut heap = BinaryHeap::new();
        match indexed_rows {
            Some(indexes) => {
                for index in indexes {
                    score_and_push_fast_vector_candidate(
                        &mut heap,
                        &table.rows[*index],
                        &table.columns,
                        selection,
                        &plan,
                    )?;
                }
            }
            None => {
                for row in &table.rows {
                    score_and_push_fast_vector_candidate(
                        &mut heap,
                        row,
                        &table.columns,
                        selection,
                        &plan,
                    )?;
                }
            }
        }
        heap
    };
    let mut candidates = heap.drain().collect::<Vec<_>>();
    candidates.sort();
    let rows = candidates
        .into_iter()
        .skip(plan.offset)
        .take(plan.limit)
        .map(|candidate| plan.materialize(candidate))
        .collect();
    Ok(QueryResult {
        columns: result_columns,
        column_types: result_types,
        rows,
        rows_examined,
    })
}

#[derive(Debug)]
struct Candidate {
    values: Vec<Value>,
    order: Vec<Value>,
}

struct CandidateSink<'a> {
    storage: CandidateStorage<'a>,
    seen: Option<HashSet<Vec<UniqueKey>>>,
}

enum CandidateStorage<'a> {
    All(Vec<Candidate>),
    TopK {
        heap: BinaryHeap<RankedCandidate<'a>>,
        capacity: usize,
        order_by: &'a [OrderByExpr],
    },
}

struct RankedCandidate<'a> {
    candidate: Candidate,
    order_by: &'a [OrderByExpr],
}

impl<'a> CandidateSink<'a> {
    fn new(
        order_by: &'a [OrderByExpr],
        offset: usize,
        limit: Option<usize>,
        distinct: bool,
    ) -> Result<Self> {
        let storage = match (order_by.is_empty(), limit) {
            (false, Some(limit)) => CandidateStorage::TopK {
                heap: BinaryHeap::new(),
                capacity: offset
                    .checked_add(limit)
                    .ok_or_else(|| Error::InvalidQuery("OFFSET plus LIMIT is too large".into()))?,
                order_by,
            },
            _ => CandidateStorage::All(Vec::new()),
        };
        Ok(Self {
            storage,
            seen: distinct.then(HashSet::new),
        })
    }

    fn push(&mut self, candidate: Candidate) {
        if let Some(seen) = &mut self.seen {
            let key = candidate
                .values
                .iter()
                .map(UniqueKey::from)
                .collect::<Vec<_>>();
            if !seen.insert(key) {
                return;
            }
        }
        match &mut self.storage {
            CandidateStorage::All(candidates) => candidates.push(candidate),
            CandidateStorage::TopK {
                heap,
                capacity,
                order_by,
            } => {
                if *capacity == 0 {
                    return;
                }
                let ranked = RankedCandidate {
                    candidate,
                    order_by,
                };
                if heap.len() < *capacity {
                    heap.push(ranked);
                } else if let Some(worst) = heap.peek() {
                    if ranked < *worst {
                        heap.pop();
                        heap.push(ranked);
                    }
                }
            }
        }
    }

    fn into_candidates(self) -> Vec<Candidate> {
        match self.storage {
            CandidateStorage::All(candidates) => candidates,
            CandidateStorage::TopK { heap, .. } => heap
                .into_iter()
                .map(|candidate| candidate.candidate)
                .collect(),
        }
    }
}

impl PartialEq for RankedCandidate<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for RankedCandidate<'_> {}

impl PartialOrd for RankedCandidate<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RankedCandidate<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        compare_order(&self.candidate, &other.candidate, self.order_by)
    }
}

fn evaluate_order(
    order: &OrderByExpr,
    context: &EvalContext<'_>,
    labels: &[String],
    values: &[Value],
) -> Result<Value> {
    if let Expr::Identifier(identifier) = &order.expr {
        let name = ident_name(identifier);
        if let Some(index) = labels
            .iter()
            .position(|label| normalize_name(label) == name)
        {
            return Ok(values[index].clone());
        }
    }
    evaluate(&order.expr, context)
}

fn compare_order(left: &Candidate, right: &Candidate, order_by: &[OrderByExpr]) -> Ordering {
    for (index, order) in order_by.iter().enumerate() {
        let nulls_first = order.nulls_first.unwrap_or(order.asc == Some(false));
        let ordering = match (&left.order[index], &right.order[index]) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Null, _) => {
                if nulls_first {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (_, Value::Null) => {
                if nulls_first {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            (left, right) => {
                let ordering = compare_sort_values(left, right);
                if order.asc == Some(false) {
                    ordering.reverse()
                } else {
                    ordering
                }
            }
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

fn compare_sort_values(left: &Value, right: &Value) -> Ordering {
    match (left, right) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,
        (Value::Integer(left), Value::Integer(right)) => left.cmp(right),
        (Value::Integer(left), Value::Float(right)) => {
            (*left as f64).partial_cmp(right).unwrap_or(Ordering::Equal)
        }
        (Value::Float(left), Value::Integer(right)) => left
            .partial_cmp(&(*right as f64))
            .unwrap_or(Ordering::Equal),
        (Value::Float(left), Value::Float(right)) => {
            left.partial_cmp(right).unwrap_or(Ordering::Equal)
        }
        (Value::Text(left), Value::Text(right)) => left.cmp(right),
        (Value::Boolean(left), Value::Boolean(right)) => left.cmp(right),
        // Evaluation/type checking catches most mixed-type cases. Keep sorting total.
        (left, right) => left.type_name().cmp(right.type_name()),
    }
}

#[derive(Clone, Copy)]
struct EvalContext<'a> {
    columns: &'a [Column],
    row: &'a [Value],
    excluded: Option<&'a [Value]>,
}

impl<'a> EvalContext<'a> {
    fn new(columns: &'a [Column], row: &'a [Value]) -> Self {
        Self {
            columns,
            row,
            excluded: None,
        }
    }

    fn upsert(columns: &'a [Column], row: &'a [Value], excluded: &'a [Value]) -> Self {
        Self {
            columns,
            row,
            excluded: Some(excluded),
        }
    }

    fn empty() -> Self {
        Self {
            columns: &[],
            row: &[],
            excluded: None,
        }
    }

    fn column(&self, name: &str) -> Result<Value> {
        let index = find_column(self.columns, name)?;
        Ok(self.row[index].clone())
    }

    fn compound_column(&self, identifiers: &[Ident]) -> Result<Value> {
        let identifier = identifiers
            .last()
            .ok_or_else(|| Error::InvalidQuery("empty identifier".into()))?;
        let index = find_column(self.columns, &ident_name(identifier))?;
        if identifiers.len() == 2
            && identifiers.first().map(ident_name).as_deref() == Some("excluded")
        {
            if let Some(excluded) = self.excluded {
                return Ok(excluded[index].clone());
            }
        }
        Ok(self.row[index].clone())
    }
}

fn evaluate(expression: &Expr, context: &EvalContext<'_>) -> Result<Value> {
    match expression {
        Expr::Value(value) => sql_literal(value),
        Expr::Identifier(identifier) => context.column(&ident_name(identifier)),
        Expr::CompoundIdentifier(identifiers) => context.compound_column(identifiers),
        Expr::Nested(expression) => evaluate(expression, context),
        Expr::Array(array) => {
            let values = array
                .elem
                .iter()
                .map(|element| evaluate(element, context)?.as_f64())
                .collect::<Result<Vec<_>>>()?;
            if values.iter().any(Option::is_none) {
                return Err(Error::InvalidQuery(
                    "vector literals cannot contain NULL".into(),
                ));
            }
            let values = values
                .into_iter()
                .flatten()
                .map(|value| value as f32)
                .collect::<Vec<_>>();
            ensure_finite_f32(&values)?;
            Ok(Value::Vector(Vector::new(values)?))
        }
        Expr::Function(function) => evaluate_function(function, context),
        Expr::BinaryOp { left, op, right } => evaluate_binary(left, op, right, context),
        Expr::UnaryOp { op, expr } => evaluate_unary(op, expr, context),
        Expr::IsNull(expression) => Ok(Value::Boolean(matches!(
            evaluate(expression, context)?,
            Value::Null
        ))),
        Expr::IsNotNull(expression) => Ok(Value::Boolean(!matches!(
            evaluate(expression, context)?,
            Value::Null
        ))),
        Expr::IsTrue(expression) => Ok(Value::Boolean(
            evaluate(expression, context)?.as_bool()? == Some(true),
        )),
        Expr::IsFalse(expression) => Ok(Value::Boolean(
            evaluate(expression, context)?.as_bool()? == Some(false),
        )),
        Expr::IsNotTrue(expression) => Ok(Value::Boolean(
            evaluate(expression, context)?.as_bool()? != Some(true),
        )),
        Expr::IsNotFalse(expression) => Ok(Value::Boolean(
            evaluate(expression, context)?.as_bool()? != Some(false),
        )),
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => {
            let value = evaluate(expr, context)?;
            let lower = compare_values(&value, &evaluate(low, context)?, BinaryOperator::GtEq)?;
            let upper = compare_values(&value, &evaluate(high, context)?, BinaryOperator::LtEq)?;
            boolean_not_if(sql_and(lower, upper)?, *negated)
        }
        Expr::InList {
            expr,
            list,
            negated,
        } => {
            let value = evaluate(expr, context)?;
            let mut found = false;
            let mut saw_null = matches!(value, Value::Null);
            for item in list {
                match compare_values(&value, &evaluate(item, context)?, BinaryOperator::Eq)? {
                    Value::Boolean(true) => found = true,
                    Value::Null => saw_null = true,
                    _ => {}
                }
            }
            let result = if found {
                Value::Boolean(true)
            } else if saw_null {
                Value::Null
            } else {
                Value::Boolean(false)
            };
            boolean_not_if(result, *negated)
        }
        Expr::Like {
            negated,
            expr,
            pattern,
            escape_char,
        }
        | Expr::ILike {
            negated,
            expr,
            pattern,
            escape_char,
        } => {
            if escape_char.is_some() {
                return Err(Error::Unsupported("LIKE ... ESCAPE".into()));
            }
            let case_insensitive = matches!(expression, Expr::ILike { .. });
            let value = evaluate(expr, context)?;
            let pattern = evaluate(pattern, context)?;
            match (value, pattern) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Text(mut value), Value::Text(mut pattern)) => {
                    if case_insensitive {
                        value = value.to_lowercase();
                        pattern = pattern.to_lowercase();
                    }
                    boolean_not_if(Value::Boolean(like_matches(&value, &pattern)), *negated)
                }
                (left, right) => Err(Error::TypeMismatch {
                    expected: "TEXT LIKE TEXT".into(),
                    found: format!("{} LIKE {}", left.type_name(), right.type_name()),
                }),
            }
        }
        Expr::Cast {
            expr, data_type, ..
        } => coerce(evaluate(expr, context)?, &parse_data_type(data_type)?),
        other => Err(Error::Unsupported(format!("expression {other}"))),
    }
}

fn evaluate_binary(
    left: &Expr,
    operator: &BinaryOperator,
    right: &Expr,
    context: &EvalContext<'_>,
) -> Result<Value> {
    if matches!(operator, BinaryOperator::And | BinaryOperator::Or) {
        let left = evaluate(left, context)?;
        // Short-circuit without changing SQL's three-valued boolean behavior.
        if matches!(
            (operator, left.as_bool()?),
            (BinaryOperator::And, Some(false))
        ) {
            return Ok(Value::Boolean(false));
        }
        if matches!(
            (operator, left.as_bool()?),
            (BinaryOperator::Or, Some(true))
        ) {
            return Ok(Value::Boolean(true));
        }
        let right = evaluate(right, context)?;
        return if *operator == BinaryOperator::And {
            sql_and(left, right)
        } else {
            sql_or(left, right)
        };
    }

    let left = evaluate(left, context)?;
    let right = evaluate(right, context)?;
    match operator {
        BinaryOperator::Eq
        | BinaryOperator::NotEq
        | BinaryOperator::Gt
        | BinaryOperator::GtEq
        | BinaryOperator::Lt
        | BinaryOperator::LtEq => compare_values(&left, &right, operator.clone()),
        BinaryOperator::Plus
        | BinaryOperator::Minus
        | BinaryOperator::Multiply
        | BinaryOperator::Divide
        | BinaryOperator::Modulo => numeric_binary(&left, &right, operator),
        BinaryOperator::Custom(operator) => vector_operator(&left, &right, operator),
        _ => Err(Error::Unsupported(format!("binary operator {operator}"))),
    }
}

fn vector_operator(left: &Value, right: &Value, operator: &str) -> Result<Value> {
    let (Some(left), Some(right)) = (left.as_vector()?, right.as_vector()?) else {
        return Ok(Value::Null);
    };
    let result = match operator {
        "<->" => left.l2_distance(right)?,
        "<#>" => -left.dot_product(right)?,
        "<=>" => left.cosine_distance(right)?,
        _ => return Err(Error::Unsupported(format!("custom operator {operator}"))),
    };
    Ok(Value::Float(result as f64))
}

fn evaluate_unary(
    operator: &UnaryOperator,
    expression: &Expr,
    context: &EvalContext<'_>,
) -> Result<Value> {
    let value = evaluate(expression, context)?;
    match operator {
        UnaryOperator::Not => match value.as_bool()? {
            Some(value) => Ok(Value::Boolean(!value)),
            None => Ok(Value::Null),
        },
        UnaryOperator::Plus => match value {
            Value::Integer(_) | Value::Float(_) | Value::Null => Ok(value),
            value => Err(type_mismatch("numeric value", &value)),
        },
        UnaryOperator::Minus => match value {
            Value::Integer(value) => value
                .checked_neg()
                .map(Value::Integer)
                .ok_or_else(|| Error::InvalidQuery("integer overflow".into())),
            Value::Float(value) => Ok(Value::Float(-value)),
            Value::Null => Ok(Value::Null),
            value => Err(type_mismatch("numeric value", &value)),
        },
        _ => Err(Error::Unsupported(format!("unary operator {operator}"))),
    }
}

fn evaluate_function(function: &Function, context: &EvalContext<'_>) -> Result<Value> {
    if function.distinct
        || function.filter.is_some()
        || function.over.is_some()
        || !function.order_by.is_empty()
    {
        return Err(Error::Unsupported(format!(
            "function modifiers on {}",
            function.name
        )));
    }
    let name = object_name(&function.name);
    let arguments = function
        .args
        .iter()
        .map(|argument| match argument {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(expression)) => {
                evaluate(expression, context)
            }
            _ => Err(Error::Unsupported(
                "named or wildcard function arguments".into(),
            )),
        })
        .collect::<Result<Vec<_>>>()?;

    match name.as_str() {
        "vector" => {
            let mut values = Vec::with_capacity(arguments.len());
            for argument in arguments {
                let value = argument
                    .as_f64()?
                    .ok_or_else(|| Error::InvalidQuery("VECTOR arguments cannot be NULL".into()))?
                    as f32;
                values.push(value);
            }
            ensure_finite_f32(&values)?;
            Ok(Value::Vector(Vector::new(values)?))
        }
        "vector_dims" | "dimensions" => {
            require_argument_count(&name, &arguments, 1)?;
            match arguments[0].as_vector()? {
                Some(vector) => Ok(Value::Integer(vector.dimensions() as i64)),
                None => Ok(Value::Null),
            }
        }
        "vector_norm" | "norm" => {
            require_argument_count(&name, &arguments, 1)?;
            match arguments[0].as_vector()? {
                Some(vector) => Ok(Value::Float(vector.norm())),
                None => Ok(Value::Null),
            }
        }
        "normalize" | "normalize_vector" => {
            require_argument_count(&name, &arguments, 1)?;
            match arguments[0].as_vector()? {
                Some(vector) => Ok(Value::Vector(vector.normalized()?)),
                None => Ok(Value::Null),
            }
        }
        "l2_distance"
        | "euclidean_distance"
        | "squared_l2_distance"
        | "cosine_distance"
        | "dot_product"
        | "inner_product" => {
            require_argument_count(&name, &arguments, 2)?;
            let (Some(left), Some(right)) = (arguments[0].as_vector()?, arguments[1].as_vector()?)
            else {
                return Ok(Value::Null);
            };
            let result = match name.as_str() {
                "l2_distance" | "euclidean_distance" => left.l2_distance(right)?,
                "squared_l2_distance" => left.squared_l2_distance(right)?,
                "cosine_distance" => left.cosine_distance(right)?,
                "dot_product" | "inner_product" => left.dot_product(right)?,
                _ => unreachable!(),
            };
            Ok(Value::Float(result as f64))
        }
        _ => Err(Error::Unsupported(format!("function {name}"))),
    }
}

fn require_argument_count(name: &str, arguments: &[Value], expected: usize) -> Result<()> {
    if arguments.len() != expected {
        return Err(Error::InvalidQuery(format!(
            "{name} expects {expected} argument(s), received {}",
            arguments.len()
        )));
    }
    Ok(())
}

fn numeric_binary(left: &Value, right: &Value, operator: &BinaryOperator) -> Result<Value> {
    if matches!(left, Value::Null) || matches!(right, Value::Null) {
        return Ok(Value::Null);
    }
    if let (Value::Integer(left), Value::Integer(right)) = (left, right) {
        let value = match operator {
            BinaryOperator::Plus => left.checked_add(*right),
            BinaryOperator::Minus => left.checked_sub(*right),
            BinaryOperator::Multiply => left.checked_mul(*right),
            BinaryOperator::Divide if *right != 0 => left.checked_div(*right),
            BinaryOperator::Modulo if *right != 0 => left.checked_rem(*right),
            BinaryOperator::Divide | BinaryOperator::Modulo => {
                return Err(Error::InvalidQuery("division by zero".into()))
            }
            _ => None,
        };
        return value
            .map(Value::Integer)
            .ok_or_else(|| Error::InvalidQuery("integer overflow".into()));
    }
    let (Some(left), Some(right)) = (left.as_f64()?, right.as_f64()?) else {
        return Ok(Value::Null);
    };
    if right == 0.0 && matches!(operator, BinaryOperator::Divide | BinaryOperator::Modulo) {
        return Err(Error::InvalidQuery("division by zero".into()));
    }
    let value = match operator {
        BinaryOperator::Plus => left + right,
        BinaryOperator::Minus => left - right,
        BinaryOperator::Multiply => left * right,
        BinaryOperator::Divide => left / right,
        BinaryOperator::Modulo => left % right,
        _ => unreachable!(),
    };
    if !value.is_finite() {
        return Err(Error::InvalidQuery("non-finite numeric result".into()));
    }
    Ok(Value::Float(value))
}

fn compare_values(left: &Value, right: &Value, operator: BinaryOperator) -> Result<Value> {
    if matches!(left, Value::Null) || matches!(right, Value::Null) {
        return Ok(Value::Null);
    }
    let ordering = match (left, right) {
        (Value::Integer(left), Value::Integer(right)) => left.cmp(right),
        (Value::Integer(left), Value::Float(right)) => (*left as f64)
            .partial_cmp(right)
            .ok_or_else(|| Error::InvalidQuery("cannot compare NaN".into()))?,
        (Value::Float(left), Value::Integer(right)) => left
            .partial_cmp(&(*right as f64))
            .ok_or_else(|| Error::InvalidQuery("cannot compare NaN".into()))?,
        (Value::Float(left), Value::Float(right)) => left
            .partial_cmp(right)
            .ok_or_else(|| Error::InvalidQuery("cannot compare NaN".into()))?,
        (Value::Text(left), Value::Text(right)) => left.cmp(right),
        (Value::Boolean(left), Value::Boolean(right)) => left.cmp(right),
        (Value::Vector(left), Value::Vector(right)) if operator == BinaryOperator::Eq => {
            return Ok(Value::Boolean(left == right))
        }
        (Value::Vector(left), Value::Vector(right)) if operator == BinaryOperator::NotEq => {
            return Ok(Value::Boolean(left != right))
        }
        _ => {
            return Err(Error::TypeMismatch {
                expected: "comparable values".into(),
                found: format!("{} and {}", left.type_name(), right.type_name()),
            })
        }
    };
    let result = match operator {
        BinaryOperator::Eq => ordering == Ordering::Equal,
        BinaryOperator::NotEq => ordering != Ordering::Equal,
        BinaryOperator::Gt => ordering == Ordering::Greater,
        BinaryOperator::GtEq => ordering != Ordering::Less,
        BinaryOperator::Lt => ordering == Ordering::Less,
        BinaryOperator::LtEq => ordering != Ordering::Greater,
        _ => unreachable!(),
    };
    Ok(Value::Boolean(result))
}

fn sql_and(left: Value, right: Value) -> Result<Value> {
    match (left.as_bool()?, right.as_bool()?) {
        (Some(false), _) | (_, Some(false)) => Ok(Value::Boolean(false)),
        (Some(true), Some(true)) => Ok(Value::Boolean(true)),
        _ => Ok(Value::Null),
    }
}

fn sql_or(left: Value, right: Value) -> Result<Value> {
    match (left.as_bool()?, right.as_bool()?) {
        (Some(true), _) | (_, Some(true)) => Ok(Value::Boolean(true)),
        (Some(false), Some(false)) => Ok(Value::Boolean(false)),
        _ => Ok(Value::Null),
    }
}

fn boolean_not_if(value: Value, negate: bool) -> Result<Value> {
    if !negate {
        return Ok(value);
    }
    match value.as_bool()? {
        Some(value) => Ok(Value::Boolean(!value)),
        None => Ok(Value::Null),
    }
}

fn like_matches(value: &str, pattern: &str) -> bool {
    let value = value.chars().collect::<Vec<_>>();
    let pattern = pattern.chars().collect::<Vec<_>>();
    let mut previous = vec![false; value.len() + 1];
    previous[0] = true;
    for pattern_char in pattern {
        let mut current = vec![false; value.len() + 1];
        if pattern_char == '%' {
            current[0] = previous[0];
        }
        for index in 1..=value.len() {
            current[index] = match pattern_char {
                '%' => current[index - 1] || previous[index],
                '_' => previous[index - 1],
                literal => previous[index - 1] && value[index - 1] == literal,
            };
        }
        previous = current;
    }
    previous[value.len()]
}

fn sql_literal(value: &SqlValue) -> Result<Value> {
    match value {
        SqlValue::Number(value, _) if value.contains(['.', 'e', 'E']) => {
            let value = value
                .parse::<f64>()
                .map_err(|_| Error::InvalidQuery(format!("invalid number {value}")))?;
            if !value.is_finite() {
                return Err(Error::InvalidQuery("numbers must be finite".into()));
            }
            Ok(Value::Float(value))
        }
        SqlValue::Number(value, _) => value
            .parse::<i64>()
            .map(Value::Integer)
            .map_err(|_| Error::InvalidQuery(format!("integer is out of range: {value}"))),
        SqlValue::SingleQuotedString(value)
        | SqlValue::DoubleQuotedString(value)
        | SqlValue::EscapedStringLiteral(value)
        | SqlValue::NationalStringLiteral(value)
        | SqlValue::RawStringLiteral(value) => Ok(Value::Text(value.clone())),
        SqlValue::Boolean(value) => Ok(Value::Boolean(*value)),
        SqlValue::Null => Ok(Value::Null),
        other => Err(Error::Unsupported(format!("literal {other}"))),
    }
}

fn parse_data_type(data_type: &sqlparser::ast::DataType) -> Result<DataType> {
    use sqlparser::ast::DataType as SqlDataType;
    match data_type {
        SqlDataType::TinyInt(_)
        | SqlDataType::Int2(_)
        | SqlDataType::SmallInt(_)
        | SqlDataType::MediumInt(_)
        | SqlDataType::Int(_)
        | SqlDataType::Int4(_)
        | SqlDataType::Int64
        | SqlDataType::Integer(_)
        | SqlDataType::BigInt(_)
        | SqlDataType::Int8(_) => Ok(DataType::Integer),
        SqlDataType::Float(_)
        | SqlDataType::Float4
        | SqlDataType::Float64
        | SqlDataType::Real
        | SqlDataType::Float8
        | SqlDataType::Double
        | SqlDataType::DoublePrecision
        | SqlDataType::Numeric(_)
        | SqlDataType::Decimal(_)
        | SqlDataType::Dec(_) => Ok(DataType::Float),
        SqlDataType::Text
        | SqlDataType::String(_)
        | SqlDataType::Character(_)
        | SqlDataType::Char(_)
        | SqlDataType::CharacterVarying(_)
        | SqlDataType::CharVarying(_)
        | SqlDataType::Varchar(_)
        | SqlDataType::Nvarchar(_) => Ok(DataType::Text),
        SqlDataType::Bool | SqlDataType::Boolean => Ok(DataType::Boolean),
        SqlDataType::Custom(name, modifiers) if object_name(name) == "vector" => {
            if modifiers.len() != 1 {
                return Err(Error::InvalidQuery(
                    "VECTOR type requires exactly one dimension, for example VECTOR(384)".into(),
                ));
            }
            let dimensions = modifiers[0].parse::<usize>().map_err(|_| {
                Error::InvalidQuery(format!("invalid vector dimension {}", modifiers[0]))
            })?;
            if dimensions == 0 {
                return Err(Error::InvalidVectorDimension);
            }
            if dimensions > MAX_VECTOR_DIMENSIONS {
                return Err(Error::VectorDimensionLimit {
                    found: dimensions,
                    max: MAX_VECTOR_DIMENSIONS,
                });
            }
            Ok(DataType::Vector(dimensions))
        }
        _ => Err(Error::Unsupported(format!("data type {data_type}"))),
    }
}

fn coerce(value: Value, data_type: &DataType) -> Result<Value> {
    match (value, data_type) {
        (Value::Null, _) => Ok(Value::Null),
        (value @ Value::Integer(_), DataType::Integer) => Ok(value),
        (Value::Integer(value), DataType::Float) => Ok(Value::Float(value as f64)),
        (value @ Value::Float(_), DataType::Float) => Ok(value),
        (value @ Value::Text(_), DataType::Text) => Ok(value),
        (value @ Value::Boolean(_), DataType::Boolean) => Ok(value),
        (Value::Vector(value), DataType::Vector(dimensions))
            if value.dimensions() == *dimensions =>
        {
            Ok(Value::Vector(value))
        }
        (Value::Vector(value), DataType::Vector(dimensions)) => Err(Error::DimensionMismatch {
            left: *dimensions,
            right: value.dimensions(),
        }),
        (value, expected) => Err(type_mismatch(&expected.to_string(), &value)),
    }
}

pub(crate) fn validate_row(columns: &[Column], row: &[Value]) -> Result<()> {
    for (column, value) in columns.iter().zip(row) {
        if !column.nullable && matches!(value, Value::Null) {
            return Err(Error::NullViolation(column.name.clone()));
        }
    }
    Ok(())
}

pub(crate) fn validate_unique(table: &Table, pending: &[Vec<Value>]) -> Result<()> {
    for (column_index, column) in table.columns.iter().enumerate() {
        if !column.unique {
            continue;
        }
        let mut values = HashSet::new();
        if let Some(existing) = table.unique_keys.get(&column_index) {
            for row in pending {
                let value = &row[column_index];
                if matches!(value, Value::Null) {
                    continue;
                }
                let key = UniqueKey::from(value);
                if existing.contains_key(&key) || !values.insert(key) {
                    return Err(Error::UniqueViolation(column.name.clone()));
                }
            }
        } else {
            // Snapshots and prospective replacement tables deliberately start
            // without maps so validation remains independent of cached state.
            for row in table.rows.iter().chain(pending) {
                let value = &row[column_index];
                if matches!(value, Value::Null) {
                    continue;
                }
                if !values.insert(UniqueKey::from(value)) {
                    return Err(Error::UniqueViolation(column.name.clone()));
                }
            }
        }
    }
    Ok(())
}

enum InsertConflictPlan {
    Fail,
    DoNothing(Vec<usize>),
    DoUpdate {
        conflict_column: usize,
        update: DoUpdate,
    },
    ReplaceColumns {
        conflict_column: usize,
        update_columns: Vec<usize>,
    },
}

enum PreparedInsertMutation {
    Append(Vec<Vec<Value>>),
    Replace { table: Table, rows_affected: usize },
}

impl PreparedInsertMutation {
    fn rows_affected(&self) -> usize {
        match self {
            Self::Append(rows) => rows.len(),
            Self::Replace { rows_affected, .. } => *rows_affected,
        }
    }

    fn apply(self, table: &mut Table) {
        match self {
            Self::Append(rows) => {
                let first_new_row = table.rows.len();
                table.rows.extend(rows);
                extend_indexes(table, first_new_row);
            }
            Self::Replace {
                table: replacement, ..
            } => *table = replacement,
        }
    }
}

fn prepare_durable_insert(
    table: &Table,
    pending: Vec<Vec<Value>>,
    conflict_plan: InsertConflictPlan,
) -> Result<PreparedInsertMutation> {
    match conflict_plan {
        InsertConflictPlan::Fail => {
            validate_unique(table, &pending)?;
            Ok(PreparedInsertMutation::Append(pending))
        }
        InsertConflictPlan::DoNothing(conflict_columns) => {
            let mut accepted = Vec::with_capacity(pending.len());
            for row in pending {
                if !row_conflicts(table, &accepted, &row, &conflict_columns) {
                    accepted.push(row);
                }
            }
            validate_unique(table, &accepted)?;
            Ok(PreparedInsertMutation::Append(accepted))
        }
        plan => {
            let mut replacement = table.clone();
            let rows_affected = apply_insert_plan(&mut replacement, pending, plan)?;
            Ok(PreparedInsertMutation::Replace {
                table: replacement,
                rows_affected,
            })
        }
    }
}

fn prepare_typed_rows(table: &Table, rows: Vec<Vec<Value>>) -> Result<Vec<Vec<Value>>> {
    let mut pending = Vec::with_capacity(rows.len());
    for row in rows {
        if row.len() != table.columns.len() {
            return Err(Error::InvalidQuery(format!(
                "typed insert row has {} value(s), expected {}",
                row.len(),
                table.columns.len()
            )));
        }
        let row = row
            .into_iter()
            .zip(&table.columns)
            .map(|(value, column)| coerce(value, &column.data_type))
            .collect::<Result<Vec<_>>>()?;
        validate_row(&table.columns, &row)?;
        pending.push(row);
    }
    Ok(pending)
}

fn resolve_typed_conflict_plan(
    table: &Table,
    conflict: &InsertConflict,
) -> Result<InsertConflictPlan> {
    match conflict {
        InsertConflict::Fail => Ok(InsertConflictPlan::Fail),
        InsertConflict::DoNothing { target } => {
            let columns = match target {
                Some(target) => vec![resolve_unique_column(table, target)?],
                None => table
                    .columns
                    .iter()
                    .enumerate()
                    .filter_map(|(index, column)| column.unique.then_some(index))
                    .collect(),
            };
            Ok(InsertConflictPlan::DoNothing(columns))
        }
        InsertConflict::DoUpdate {
            target,
            update_columns,
        } => {
            if update_columns.is_empty() {
                return Err(Error::InvalidQuery(
                    "typed conflict update requires at least one update column".into(),
                ));
            }
            let mut seen = HashSet::new();
            let update_columns = update_columns
                .iter()
                .map(|name| {
                    let name = normalize_name(name);
                    if !seen.insert(name.clone()) {
                        return Err(Error::DuplicateColumn(name));
                    }
                    find_column(&table.columns, &name)
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(InsertConflictPlan::ReplaceColumns {
                conflict_column: resolve_unique_column(table, target)?,
                update_columns,
            })
        }
    }
}

fn resolve_unique_column(table: &Table, name: &str) -> Result<usize> {
    let index = find_column(&table.columns, name)?;
    if table.columns[index].unique {
        Ok(index)
    } else {
        Err(Error::InvalidQuery(format!(
            "conflict target '{}' is not unique",
            table.columns[index].name
        )))
    }
}

fn apply_insert_plan(
    table: &mut Table,
    pending: Vec<Vec<Value>>,
    conflict_plan: InsertConflictPlan,
) -> Result<usize> {
    match conflict_plan {
        InsertConflictPlan::Fail => {
            validate_unique(table, &pending)?;
            let rows_affected = pending.len();
            let first_new_row = table.rows.len();
            table.rows.extend(pending);
            extend_indexes(table, first_new_row);
            Ok(rows_affected)
        }
        InsertConflictPlan::DoNothing(conflict_columns) => {
            let mut accepted = Vec::with_capacity(pending.len());
            for row in pending {
                if !row_conflicts(table, &accepted, &row, &conflict_columns) {
                    accepted.push(row);
                }
            }
            validate_unique(table, &accepted)?;
            let rows_affected = accepted.len();
            let first_new_row = table.rows.len();
            table.rows.extend(accepted);
            extend_indexes(table, first_new_row);
            Ok(rows_affected)
        }
        InsertConflictPlan::DoUpdate {
            conflict_column,
            update,
        } => {
            let rows_affected = apply_conflict_updates(table, pending, conflict_column, &update)?;
            if rows_affected > 0 {
                rebuild_indexes(table);
            }
            Ok(rows_affected)
        }
        InsertConflictPlan::ReplaceColumns {
            conflict_column,
            update_columns,
        } => {
            let rows_affected =
                apply_conflict_replacements(table, pending, conflict_column, &update_columns)?;
            if rows_affected > 0 {
                rebuild_indexes(table);
            }
            Ok(rows_affected)
        }
    }
}

fn resolve_conflict_plan(table: &Table, on_insert: Option<OnInsert>) -> Result<InsertConflictPlan> {
    let Some(on_insert) = on_insert else {
        return Ok(InsertConflictPlan::Fail);
    };
    let conflict = match on_insert {
        OnInsert::OnConflict(conflict) => conflict,
        OnInsert::DuplicateKeyUpdate(_) => {
            return Err(Error::Unsupported("ON DUPLICATE KEY UPDATE".into()))
        }
        _ => return Err(Error::Unsupported("unknown INSERT conflict clause".into())),
    };
    match conflict.action {
        OnConflictAction::DoNothing => match conflict.conflict_target.as_ref() {
            None => Ok(InsertConflictPlan::DoNothing(
                table
                    .columns
                    .iter()
                    .enumerate()
                    .filter_map(|(index, column)| column.unique.then_some(index))
                    .collect(),
            )),
            Some(target) => Ok(InsertConflictPlan::DoNothing(vec![
                resolve_conflict_target(table, target)?,
            ])),
        },
        OnConflictAction::DoUpdate(update) => {
            let target = conflict.conflict_target.as_ref().ok_or_else(|| {
                Error::InvalidQuery("ON CONFLICT DO UPDATE requires a conflict target".into())
            })?;
            Ok(InsertConflictPlan::DoUpdate {
                conflict_column: resolve_conflict_target(table, target)?,
                update,
            })
        }
    }
}

fn resolve_conflict_target(table: &Table, target: &ConflictTarget) -> Result<usize> {
    match target {
        ConflictTarget::Columns(columns) if columns.len() == 1 => {
            let index = find_column(&table.columns, &ident_name(&columns[0]))?;
            if table.columns[index].unique {
                Ok(index)
            } else {
                Err(Error::InvalidQuery(format!(
                    "ON CONFLICT target '{}' is not unique",
                    table.columns[index].name
                )))
            }
        }
        ConflictTarget::Columns(_) => {
            Err(Error::Unsupported("composite ON CONFLICT targets".into()))
        }
        ConflictTarget::OnConstraint(_) => {
            Err(Error::Unsupported("named ON CONFLICT constraints".into()))
        }
    }
}

fn apply_conflict_updates(
    table: &mut Table,
    pending: Vec<Vec<Value>>,
    conflict_column: usize,
    update: &DoUpdate,
) -> Result<usize> {
    if update.assignments.is_empty() {
        return Err(Error::InvalidQuery(
            "ON CONFLICT DO UPDATE requires at least one assignment".into(),
        ));
    }

    let mut seen_columns = HashSet::new();
    let assignment_indexes = update
        .assignments
        .iter()
        .map(|assignment| {
            let identifier = assignment
                .id
                .last()
                .ok_or_else(|| Error::InvalidQuery("empty ON CONFLICT assignment".into()))?;
            let name = ident_name(identifier);
            if !seen_columns.insert(name.clone()) {
                return Err(Error::DuplicateColumn(name));
            }
            find_column(&table.columns, &name)
        })
        .collect::<Result<Vec<_>>>()?;

    apply_conflict_updates_with(
        table,
        pending,
        conflict_column,
        |columns, existing, excluded| {
            let context = EvalContext::upsert(columns, existing, excluded);
            let should_update = match &update.selection {
                Some(selection) => evaluate(selection, &context)?.as_bool()?.unwrap_or(false),
                None => true,
            };
            if !should_update {
                return Ok(None);
            }

            let mut replacement = existing.to_vec();
            for (assignment, column_index) in update.assignments.iter().zip(&assignment_indexes) {
                let value = evaluate(&assignment.value, &context)?;
                replacement[*column_index] = coerce(value, &columns[*column_index].data_type)?;
            }
            Ok(Some(replacement))
        },
    )
}

fn apply_conflict_replacements(
    table: &mut Table,
    pending: Vec<Vec<Value>>,
    conflict_column: usize,
    update_columns: &[usize],
) -> Result<usize> {
    apply_conflict_updates_with(
        table,
        pending,
        conflict_column,
        |_columns, existing, excluded| {
            let mut replacement = existing.to_vec();
            for column in update_columns {
                replacement[*column] = excluded[*column].clone();
            }
            Ok(Some(replacement))
        },
    )
}

fn apply_conflict_updates_with(
    table: &mut Table,
    pending: Vec<Vec<Value>>,
    conflict_column: usize,
    mut replacement_for: impl FnMut(&[Column], &[Value], &[Value]) -> Result<Option<Vec<Value>>>,
) -> Result<usize> {
    // Work against a private copy so any expression or constraint failure rolls
    // back every insert and update from this statement.
    let mut prospective = table.rows.clone();
    let mut touched_rows = HashSet::new();
    let mut rows_affected = 0;
    for excluded in pending {
        let conflict = (!matches!(excluded[conflict_column], Value::Null))
            .then(|| {
                prospective
                    .iter()
                    .position(|row| row[conflict_column] == excluded[conflict_column])
            })
            .flatten();

        let Some(row_index) = conflict else {
            let row_index = prospective.len();
            prospective.push(excluded);
            touched_rows.insert(row_index);
            rows_affected += 1;
            continue;
        };
        if !touched_rows.insert(row_index) {
            return Err(Error::InvalidQuery(
                "ON CONFLICT DO UPDATE cannot affect the same row twice".into(),
            ));
        }

        let existing = prospective[row_index].clone();
        let Some(replacement) = replacement_for(&table.columns, &existing, &excluded)? else {
            continue;
        };
        validate_row(&table.columns, &replacement)?;
        prospective[row_index] = replacement;
        rows_affected += 1;
    }

    let empty_table = Table::new(table.columns.clone(), Vec::new(), HashMap::new());
    validate_unique(&empty_table, &prospective)?;
    table.rows = prospective;
    Ok(rows_affected)
}

fn row_conflicts(
    table: &Table,
    accepted: &[Vec<Value>],
    candidate: &[Value],
    conflict_columns: &[usize],
) -> bool {
    conflict_columns.iter().any(|column| {
        let value = &candidate[*column];
        if matches!(value, Value::Null) {
            return false;
        }
        let existing_conflict = table
            .unique_keys
            .get(column)
            .map(|keys| keys.contains_key(&UniqueKey::from(value)))
            .unwrap_or_else(|| table.rows.iter().any(|row| row[*column] == *value));
        existing_conflict || accepted.iter().any(|row| row[*column] == *value)
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum UniqueKey {
    Null,
    Integer(i64),
    Float(u64),
    Text(String),
    Boolean(bool),
    Vector(Vec<u32>),
}

impl HashIndex {
    pub(crate) fn new(column: usize) -> Self {
        Self {
            column,
            buckets: HashMap::new(),
        }
    }

    fn rebuild(&mut self, rows: &[Vec<Value>]) {
        self.buckets.clear();
        self.extend(rows, 0);
    }

    fn extend(&mut self, rows: &[Vec<Value>], first_row: usize) {
        for (offset, row) in rows.iter().enumerate() {
            let value = &row[self.column];
            if matches!(value, Value::Null | Value::Vector(_)) {
                continue;
            }
            self.buckets
                .entry(UniqueKey::from(value))
                .or_default()
                .push(first_row + offset);
        }
    }
}

fn extend_indexes(table: &mut Table, first_new_row: usize) {
    let new_rows = &table.rows[first_new_row..];
    for index in table.indexes.values_mut() {
        index.extend(new_rows, first_new_row);
    }
    for (column, keys) in &mut table.unique_keys {
        for (offset, row) in new_rows.iter().enumerate() {
            let value = &row[*column];
            if !matches!(value, Value::Null) {
                keys.insert(UniqueKey::from(value), first_new_row + offset);
            }
        }
    }
}

pub(crate) fn rebuild_indexes(table: &mut Table) {
    let rows = &table.rows;
    for index in table.indexes.values_mut() {
        index.rebuild(rows);
    }
    table.unique_keys.clear();
    for (column_index, column) in table.columns.iter().enumerate() {
        if !column.unique {
            continue;
        }
        let mut keys = HashMap::new();
        for (row_index, row) in rows.iter().enumerate() {
            let value = &row[column_index];
            if !matches!(value, Value::Null) {
                let previous = keys.insert(UniqueKey::from(value), row_index);
                debug_assert!(previous.is_none(), "validated unique value is duplicated");
            }
        }
        table.unique_keys.insert(column_index, keys);
    }
}

impl From<&Value> for UniqueKey {
    fn from(value: &Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Integer(value) => Self::Integer(*value),
            Value::Float(value) => Self::Float(if *value == 0.0 { 0 } else { value.to_bits() }),
            Value::Text(value) => Self::Text(value.clone()),
            Value::Boolean(value) => Self::Boolean(*value),
            Value::Vector(value) => Self::Vector(
                value
                    .as_slice()
                    .iter()
                    .map(|element| {
                        if *element == 0.0 {
                            0
                        } else {
                            element.to_bits()
                        }
                    })
                    .collect(),
            ),
        }
    }
}

fn usize_expression(expression: &Expr, context: &EvalContext<'_>, label: &str) -> Result<usize> {
    match evaluate(expression, context)? {
        Value::Integer(value) if value >= 0 => {
            usize::try_from(value).map_err(|_| Error::InvalidQuery(format!("{label} is too large")))
        }
        value => Err(Error::InvalidQuery(format!(
            "{label} must be a non-negative integer, found {value}"
        ))),
    }
}

fn table_factor_name(factor: &TableFactor) -> Result<String> {
    match factor {
        TableFactor::Table { name, args, .. } if args.is_none() => Ok(object_name(name)),
        other => Err(Error::Unsupported(format!("table source {other}"))),
    }
}

fn find_column(columns: &[Column], name: &str) -> Result<usize> {
    let name = normalize_name(name);
    columns
        .iter()
        .position(|column| normalize_name(&column.name) == name)
        .ok_or(Error::ColumnNotFound(name))
}

fn expression_label(expression: &Expr) -> String {
    match expression {
        Expr::Identifier(identifier) => ident_name(identifier),
        Expr::CompoundIdentifier(identifiers) => identifiers
            .last()
            .map(ident_name)
            .unwrap_or_else(|| expression.to_string()),
        _ => expression.to_string(),
    }
}

fn object_name(name: &ObjectName) -> String {
    name.0.last().map(ident_name).unwrap_or_default()
}

fn ident_name(identifier: &Ident) -> String {
    if identifier.quote_style.is_some() {
        identifier.value.clone()
    } else {
        normalize_name(&identifier.value)
    }
}

fn normalize_name(name: &str) -> String {
    name.to_ascii_lowercase()
}

fn type_mismatch(expected: &str, found: &Value) -> Error {
    Error::TypeMismatch {
        expected: expected.into(),
        found: found.type_name().into(),
    }
}

fn ensure_finite_f32(values: &[f32]) -> Result<()> {
    if values.iter().any(|value| !value.is_finite()) {
        return Err(Error::InvalidQuery(
            "vector elements must be finite numbers".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cache_is_shared_bounded_and_catalog_independent() {
        let database = Database::new();
        let clone = database.clone();
        assert!(Arc::ptr_eq(&database.parse_cache, &clone.parse_cache));

        database
            .execute("CREATE TABLE cached (id INTEGER PRIMARY KEY)")
            .unwrap();
        let query = "SELECT COUNT(*) FROM cached";
        assert_eq!(database.execute(query).unwrap().len(), 1);
        clone.execute("INSERT INTO cached VALUES (1)").unwrap();
        let result = database.execute(query).unwrap();
        let ExecutionResult::Query(result) = &result[0] else {
            panic!("expected a query result");
        };
        assert_eq!(result.rows, [vec![Value::Integer(1)]]);

        for value in 0..(PARSE_CACHE_MAX_ENTRIES + 16) {
            database.execute(&format!("SELECT {value}")).unwrap();
        }
        let oversized = format!("SELECT 1 -- {}", "x".repeat(PARSE_CACHE_MAX_ENTRY_BYTES));
        database.execute(&oversized).unwrap();
        assert!(matches!(database.execute("SELECT ("), Err(Error::Parse(_))));

        let cache = database.parse_cache.lock().unwrap();
        assert_eq!(cache.entries.len(), PARSE_CACHE_MAX_ENTRIES);
        assert!(cache.sql_bytes <= PARSE_CACHE_MAX_SQL_BYTES);
        assert!(!cache.entries.iter().any(|entry| entry.sql == oversized));
        assert!(!cache.entries.iter().any(|entry| entry.sql == "SELECT ("));
        assert_eq!(
            cache.entries.front().map(|entry| entry.sql.as_str()),
            Some("SELECT 79")
        );
    }
}
