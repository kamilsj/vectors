//! Compare structured vector search with the legacy schema/SQL conversion path.
//!
//! `cargo run --release --locked --example benchmark_typed_search`
//! Controls: VECTORS_BENCH_ROWS, VECTORS_BENCH_DIMENSIONS,
//! VECTORS_BENCH_ITERATIONS, and RAYON_NUM_THREADS. CPU only, LIMIT 10.
//! Query vectors change on every iteration; SQL formatting and parsing are
//! timed, while common request construction and ingestion are not. Both paths
//! use the same exact top-k engine. Every measured result is compared.

use std::env;
use std::hint::black_box;
use std::io;
use std::time::Instant;

use vectors::{
    ComputeConfig, ComputeDevice, Database, ExecutionResult, InsertConflict, QueryResult, Value,
    Vector, VectorFilterOperator, VectorSearch, VectorSearchFilter, VectorSearchMetric,
    MAX_VECTOR_DIMENSIONS,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rows = setting("VECTORS_BENCH_ROWS", 4096)?;
    let dimensions = setting("VECTORS_BENCH_DIMENSIONS", 768)?;
    let iterations = setting("VECTORS_BENCH_ITERATIONS", 100)?;
    if dimensions > MAX_VECTOR_DIMENSIONS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "vector dimensions exceed the engine limit",
        )
        .into());
    }
    let db = Database::new_with_compute(ComputeConfig {
        device: ComputeDevice::Cpu,
        ..ComputeConfig::default()
    });
    db.execute(&format!("CREATE TABLE points (id INTEGER PRIMARY KEY, category INTEGER, embedding VECTOR({dimensions})); CREATE INDEX categories ON points USING HASH(category)"))?;
    for first in (0..rows).step_by(1024) {
        let batch = (first..rows.min(first + 1024))
            .map(|row| {
                let vector = (0..dimensions)
                    .map(|dimension| ((row * 17 + dimension * 31 + 1) % 997) as f32 / 997.0 - 0.5)
                    .collect();
                Ok(vec![
                    Value::Integer(row as i64),
                    Value::Integer((row % 128) as i64),
                    Value::Vector(Vector::new(vector)?),
                ])
            })
            .collect::<vectors::Result<Vec<_>>>()?;
        db.insert_rows("points", batch, InsertConflict::Fail)?;
    }
    println!("rows={rows} dimensions={dimensions} threads={} iterations={iterations} compute=cpu ingestion_batch=1024 metric=cosine limit=10 queries=varying order=alternating profile={}", rayon::current_num_threads(), if cfg!(debug_assertions) { "debug" } else { "release" });
    println!("workload,path,iteration,rows_examined,milliseconds");
    for workload in ["full_scan", "indexed_1_of_128", "indexed_with_residual"] {
        let request = |iteration| make_request(dimensions, rows, iteration, workload);
        for warmup in 0..5 {
            let search = request(iterations + warmup + 1)?;
            assert_eq!(legacy_search(&db, &search)?, db.search_vectors(search)?);
        }
        for iteration in 0..iterations {
            let search = request(iteration)?;
            let typed = search.clone();
            let (legacy_time, legacy, typed_time, actual) = if iteration % 2 == 0 {
                let (legacy_time, legacy) = timed(|| legacy_search(&db, &search))?;
                let (typed_time, actual) = timed(|| db.search_vectors(typed))?;
                (legacy_time, legacy, typed_time, actual)
            } else {
                let (typed_time, actual) = timed(|| db.search_vectors(typed))?;
                let (legacy_time, legacy) = timed(|| legacy_search(&db, &search))?;
                (legacy_time, legacy, typed_time, actual)
            };
            assert_eq!(actual, legacy, "{workload} iteration {iteration}");
            println!(
                "{workload},sql_conversion,{iteration},{},{legacy_time:.9}",
                legacy.rows_examined
            );
            println!(
                "{workload},typed,{iteration},{},{typed_time:.9}",
                actual.rows_examined
            );
        }
    }
    Ok(())
}

fn make_request(
    dimensions: usize,
    rows: usize,
    iteration: usize,
    workload: &str,
) -> vectors::Result<VectorSearch> {
    let mut query = (0..dimensions)
        .map(|dimension| ((dimension * 13 + 3) % 101) as f32 / 101.0)
        .collect::<Vec<_>>();
    query[0] = (iteration as f32 + 1.0) / 10007.0;
    let mut filters = Vec::new();
    if workload != "full_scan" {
        filters.push(VectorSearchFilter {
            column: "category".into(),
            operator: VectorFilterOperator::Eq,
            value: Value::Integer(0),
        });
    }
    if workload == "indexed_with_residual" {
        filters.push(VectorSearchFilter {
            column: "id".into(),
            operator: VectorFilterOperator::Gte,
            value: Value::Integer((rows / 2) as i64),
        });
    }
    Ok(VectorSearch {
        table: "points".into(),
        vector_column: "embedding".into(),
        query: Vector::new(query)?,
        metric: VectorSearchMetric::Cosine,
        select: vec!["id".into(), "category".into()],
        filters,
        limit: 10,
    })
}

// Reproduce the former API conversion for this benchmark's known integer
// predicates: inspect schema, resolve/project names, format the vector, and
// execute SQL. JSON decoding and HTTP serialization are excluded from both.
fn legacy_search(db: &Database, request: &VectorSearch) -> vectors::Result<QueryResult> {
    let schema = db.schema(&request.table)?;
    let resolve = |name: &str| {
        schema
            .iter()
            .find(|column| column.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| vectors::Error::ColumnNotFound(name.into()))
    };
    let column = resolve(&request.vector_column)?;
    let mut projection = request
        .select
        .iter()
        .map(|name| resolve(name).map(|column| quote(&column.name)))
        .collect::<vectors::Result<Vec<_>>>()?;
    let vector = request
        .query
        .as_slice()
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    projection.push(format!(
        "cosine_distance({}, ARRAY[{vector}]) AS distance",
        quote(&column.name)
    ));
    let filters = request
        .filters
        .iter()
        .map(|filter| {
            let operator = match filter.operator {
                VectorFilterOperator::Eq => "=",
                VectorFilterOperator::Gte => ">=",
                _ => unreachable!("benchmark only generates integer equality/range filters"),
            };
            let Value::Integer(value) = filter.value else {
                unreachable!("benchmark only generates integer filters")
            };
            Ok(format!(
                "{} {operator} {value}",
                quote(&resolve(&filter.column)?.name)
            ))
        })
        .collect::<vectors::Result<Vec<_>>>()?;
    let selection = if filters.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", filters.join(" AND "))
    };
    let sql = format!(
        "SELECT {} FROM {}{selection} ORDER BY distance ASC LIMIT {}",
        projection.join(", "),
        quote(&request.table),
        request.limit
    );
    match db.execute(&sql)?.pop() {
        Some(ExecutionResult::Query(result)) => Ok(result),
        _ => unreachable!("benchmark executes a single SELECT"),
    }
}

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn timed(
    operation: impl FnOnce() -> vectors::Result<QueryResult>,
) -> vectors::Result<(f64, QueryResult)> {
    let started = Instant::now();
    let result = black_box(operation()?);
    Ok((started.elapsed().as_secs_f64() * 1000.0, result))
}

fn setting(name: &str, default: usize) -> io::Result<usize> {
    match env::var(name) {
        Ok(value) => value
            .parse()
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{name} must be a positive integer"),
                )
            }),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(io::Error::new(io::ErrorKind::InvalidInput, error)),
    }
}
