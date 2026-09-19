//! Measure SQL column lookup overhead in scalar and residual-filtered vector queries.
//!
//! `cargo run --release --locked --example benchmark_column_lookup`
//! Controls: VECTORS_BENCH_ROWS, VECTORS_BENCH_COLUMNS, VECTORS_BENCH_DIMENSIONS,
//! VECTORS_BENCH_ITERATIONS, and RAYON_NUM_THREADS. Reports CPU-only warm queries.

use std::env;
use std::hint::black_box;
use std::io;
use std::time::Instant;

use vectors::{
    ComputeConfig, ComputeDevice, Database, ExecutionResult, InsertConflict, QueryResult, Value,
    Vector, MAX_VECTOR_DIMENSIONS,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rows = setting("VECTORS_BENCH_ROWS", 16_384)?;
    let columns = setting("VECTORS_BENCH_COLUMNS", 32)?;
    let dimensions = setting("VECTORS_BENCH_DIMENSIONS", 64)?;
    let iterations = setting("VECTORS_BENCH_ITERATIONS", 40)?;
    if columns > 256 || dimensions > MAX_VECTOR_DIMENSIONS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "columns must be at most 256 and dimensions at most 65535",
        )
        .into());
    }
    let database = Database::new_with_compute(ComputeConfig {
        device: ComputeDevice::Cpu,
        ..ComputeConfig::default()
    });
    let scalar_columns = (0..columns)
        .map(|column| format!("c{column} INTEGER"))
        .collect::<Vec<_>>()
        .join(", ");
    database.execute(&format!("CREATE TABLE points (id INTEGER PRIMARY KEY, {scalar_columns}, embedding VECTOR({dimensions}))"))?;
    for start in (0..rows).step_by(1024) {
        let batch = (start..(start + 1024).min(rows))
            .map(|row| {
                let mut values = vec![Value::Integer(row as i64)];
                values.extend(
                    (0..columns)
                        .map(|column| Value::Integer(((row * 37 + column * 11) % 100_003) as i64)),
                );
                let vector = (0..dimensions)
                    .map(|dimension| (((row * 17 + dimension * 31 + 1) % 997) as f32 / 997.0) - 0.5)
                    .collect();
                values.push(Value::Vector(Vector::new(vector)?));
                Ok(values)
            })
            .collect::<vectors::Result<Vec<_>>>()?;
        database.insert_rows("points", batch, InsertConflict::Fail)?;
    }
    let last = columns - 1;
    let predicate = format!("c{last} % 2 = 0");
    let scalar = format!("SELECT id, c{last} FROM points WHERE {predicate} ORDER BY c{last} DESC, id ASC LIMIT 20 OFFSET 3");
    let scalar_reference =
        format!("SELECT id, c{last} FROM points WHERE {predicate} ORDER BY c{last} DESC, id ASC");
    let mut expected = query(&database, &scalar_reference)?.rows;
    expected = expected.into_iter().skip(3).take(20).collect();
    assert_eq!(query(&database, &scalar)?.rows, expected);

    let query_vector = (0..dimensions)
        .map(|dimension| (((dimension * 13 + 3) % 101) as f32 / 101.0).to_string())
        .collect::<Vec<_>>()
        .join(",");
    let score = format!("cosine_distance(embedding, ARRAY[{query_vector}])");
    let vector = format!("SELECT id, {score} AS score FROM points WHERE {predicate} ORDER BY score ASC LIMIT 20 OFFSET 3");
    let vector_reference = format!("SELECT id, {score} AS score, id + 0 AS general FROM points WHERE {predicate} ORDER BY score ASC, id ASC LIMIT 20 OFFSET 3");
    let mut expected = query(&database, &vector_reference)?;
    for row in &mut expected.rows {
        row.pop();
    }
    assert_eq!(query(&database, &vector)?.rows, expected.rows);
    assert!(
        database
            .query_intent(&vector)?
            .vector_search
            .unwrap()
            .optimized
    );
    println!("rows={rows} metadata_columns={columns} dimensions={dimensions} threads={} iterations={iterations} compute=cpu ingestion_batch=1024 limit=20 offset=3 filter=unindexed_modulo_50_percent", rayon::current_num_threads());
    println!("workload,rows_examined,p50_ms,p95_ms,mean_ms");
    for (label, sql) in [("scalar_top_k", scalar), ("cosine_residual_filter", vector)] {
        for _ in 0..5 {
            black_box(database.execute(&sql)?);
        }
        let mut times = Vec::with_capacity(iterations);
        for _ in 0..iterations {
            let started = Instant::now();
            black_box(database.execute(&sql)?);
            times.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        times.sort_by(f64::total_cmp);
        println!(
            "{label},{},{:.6},{:.6},{:.6}",
            rows,
            times[(iterations - 1) / 2],
            times[(iterations * 95).div_ceil(100) - 1],
            times.iter().sum::<f64>() / iterations as f64
        );
    }
    Ok(())
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

fn query(database: &Database, sql: &str) -> vectors::Result<QueryResult> {
    match database.execute(sql)?.pop() {
        Some(ExecutionResult::Query(result)) => Ok(result),
        _ => unreachable!("benchmark query must return rows"),
    }
}
