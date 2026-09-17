//! Compare exact CPU scan latency across ingestion layouts and thread counts.
//!
//! Run with `cargo run --release --locked --example benchmark_scan_layout`.
//! Set `RAYON_NUM_THREADS` to compare CPU scaling in separate processes.

use std::env;
use std::hint::black_box;
use std::io;
use std::time::Instant;

use vectors::{
    ComputeConfig, ComputeDevice, Database, ExecutionResult, InsertConflict, QueryResult, Value,
    Vector, MAX_VECTOR_DIMENSIONS,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rows = setting("VECTORS_BENCH_ROWS", 32_768)?;
    let dimensions = setting("VECTORS_BENCH_DIMENSIONS", 384)?;
    let iterations = setting("VECTORS_BENCH_ITERATIONS", 60)?;
    let batch_rows = setting("VECTORS_BENCH_BATCH_ROWS", rows)?;
    if dimensions > MAX_VECTOR_DIMENSIONS {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "too many dimensions").into());
    }
    let database = Database::new_with_compute(ComputeConfig {
        device: ComputeDevice::Cpu,
        ..ComputeConfig::default()
    });
    database.execute(&format!(
        "CREATE TABLE points (id INTEGER PRIMARY KEY, category INTEGER, embedding VECTOR({dimensions}));
         CREATE INDEX points_category ON points (category)"
    ))?;
    for start in (0..rows).step_by(batch_rows) {
        let batch = (start..start.saturating_add(batch_rows).min(rows))
            .map(|row| {
                let mut state = (row as u64 + 1).wrapping_mul(0x9e3779b97f4a7c15);
                let values = (0..dimensions)
                    .map(|_| {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        ((state >> 40) as f32 / 8_388_608.0) - 1.0
                    })
                    .collect();
                Ok(vec![
                    Value::Integer(row as i64),
                    Value::Integer((row % 2) as i64),
                    Value::Vector(Vector::new(values)?),
                ])
            })
            .collect::<vectors::Result<Vec<_>>>()?;
        database.insert_rows("points", batch, InsertConflict::Fail)?;
    }
    let query_vector = (0..dimensions)
        .map(|dimension| (((dimension * 17 + 3) % 101) as f32 / 101.0).to_string())
        .collect::<Vec<_>>()
        .join(",");
    println!(
        "rows={rows} dimensions={dimensions} batch_rows={batch_rows} threads={} iterations={iterations} compute=cpu",
        rayon::current_num_threads()
    );
    println!("metric,filter,candidates,p50_ms,p95_ms,mean_ms");
    for (metric, direction) in [
        ("cosine_distance", "ASC"),
        ("squared_l2_distance", "ASC"),
        ("dot_product", "DESC"),
    ] {
        for (filter_name, predicate) in [("all", ""), ("half", "WHERE category = 0")] {
            let score = format!("{metric}(embedding, ARRAY[{query_vector}])");
            let tail =
                format!("FROM points {predicate} ORDER BY score {direction} LIMIT 20 OFFSET 3");
            let optimized = format!("SELECT id, {score} AS score {tail}");
            let generic = format!(
                "SELECT id, {score} AS score, id + 0 AS generic FROM points {predicate} \
                 ORDER BY score {direction}, id ASC LIMIT 20 OFFSET 3"
            );
            let actual = query(&database, &optimized)?;
            let mut expected = query(&database, &generic)?;
            for row in &mut expected.rows {
                row.pop();
            }
            assert_eq!(actual.rows, expected.rows, "{metric}, {filter_name}");
            let intent = database.query_intent(&optimized)?;
            assert!(intent.vector_search.is_some_and(|search| search.optimized));

            // Warm query parsing, the Rayon pool, and vector pages before timing.
            for _ in 0..5 {
                black_box(database.execute(&optimized)?);
            }
            let mut samples = Vec::with_capacity(iterations);
            for _ in 0..iterations {
                let start = Instant::now();
                black_box(database.execute(&optimized)?);
                samples.push(start.elapsed().as_secs_f64() * 1_000.0);
            }
            samples.sort_by(f64::total_cmp);
            let mean = samples.iter().sum::<f64>() / iterations as f64;
            let p50 = samples[(iterations - 1) / 2];
            let p95 = samples[(iterations * 95).div_ceil(100) - 1];
            println!(
                "{metric},{filter_name},{},{p50:.6},{p95:.6},{mean:.6}",
                actual.rows_examined
            );
        }
    }
    Ok(())
}

fn setting(name: &str, default: usize) -> Result<usize, io::Error> {
    match env::var(name) {
        Ok(value) => value
            .parse::<usize>()
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
