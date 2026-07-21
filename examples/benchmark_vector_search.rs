//! Small reproducible benchmark for snapshot loading and SQL vector top-k.
//!
//! Run with `cargo run --release --example benchmark_vector_search`.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::hint::black_box;
use std::time::{Duration, Instant};

use vectors::{Database, ExecutionResult, QueryResult};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let row_count = environment_usize("VECTORS_BENCH_ROWS", 20_000);
    let dimensions = environment_usize("VECTORS_BENCH_DIMENSIONS", 64);
    let iterations = environment_usize("VECTORS_BENCH_ITERATIONS", 8);
    let database = Database::new();
    database.execute(&format!(
        "CREATE TABLE benchmark (
            id INTEGER PRIMARY KEY,
            label TEXT NOT NULL,
            category TEXT,
            embedding VECTOR({dimensions})
        );
        CREATE INDEX benchmark_category_idx
            ON benchmark USING HASH (category);"
    ))?;

    let setup_started = Instant::now();
    for batch_start in (0..row_count).step_by(500) {
        let batch_end = (batch_start + 500).min(row_count);
        let mut sql = String::from("INSERT INTO benchmark VALUES ");
        for row in batch_start..batch_end {
            if row != batch_start {
                sql.push_str(", ");
            }
            write!(
                sql,
                "({row}, 'benchmark-row-{row:08}', '{}', ARRAY[",
                if row % 2 == 0 { "even" } else { "odd" }
            )
            .expect("writing to a String cannot fail");
            for dimension in 0..dimensions {
                if dimension != 0 {
                    sql.push(',');
                }
                let value = ((row * 31 + dimension * 17 + 1) % 997) as f32 / 997.0;
                write!(sql, "{value}").expect("writing to a String cannot fail");
            }
            sql.push_str("]) ");
        }
        database.execute(&sql)?;
    }
    println!(
        "loaded {row_count} x {dimensions} vectors through SQL in {:?}",
        setup_started.elapsed()
    );

    let query_vector = (0..dimensions)
        .map(|dimension| if dimension == 0 { "1" } else { "0" })
        .collect::<Vec<_>>()
        .join(",");
    let optimized = format!(
        "SELECT id, label,
                cosine_distance(embedding, ARRAY[{query_vector}]) AS distance
         FROM benchmark
         WHERE category = 'even'
         ORDER BY distance
         LIMIT 20"
    );
    // The additional arithmetic projection deliberately selects the generic
    // evaluator, providing a correctness and overhead comparison.
    let generic = format!(
        "SELECT id, label,
                cosine_distance(embedding, ARRAY[{query_vector}]) AS distance,
                id + 0 AS generic_projection
         FROM benchmark
         WHERE category = 'even'
         ORDER BY distance
         LIMIT 20"
    );

    let optimized_rows = query(&database, &optimized)?;
    let generic_rows = query(&database, &generic)?;
    let mut optimized_comparison = optimized_rows.rows;
    let mut generic_comparison = generic_rows
        .rows
        .into_iter()
        .map(|mut row| {
            row.pop();
            row
        })
        .collect::<Vec<_>>();
    // SQL does not define ordering within equal distance values. Sort the two
    // result sets by their unique id before comparing correctness.
    optimized_comparison.sort_by(|left, right| left[0].to_string().cmp(&right[0].to_string()));
    generic_comparison.sort_by(|left, right| left[0].to_string().cmp(&right[0].to_string()));
    assert_eq!(
        optimized_comparison, generic_comparison,
        "optimized and generic plans returned different neighbors"
    );

    let optimized_time = benchmark(iterations, || database.execute(&optimized));
    let uncached_queries = (0..iterations)
        .map(|iteration| format!("{optimized}\n-- parse-cache-miss-{iteration}"))
        .collect::<Vec<_>>();
    let mut uncached_iteration = 0;
    let uncached_time = benchmark(iterations, || {
        let result = database.execute(&uncached_queries[uncached_iteration]);
        uncached_iteration += 1;
        result
    });
    let generic_time = benchmark(iterations, || database.execute(&generic));
    println!(
        "cached top-k average:    {:?}",
        optimized_time / iterations as u32
    );
    println!(
        "uncached top-k average:  {:?}",
        uncached_time / iterations as u32
    );
    println!(
        "parse-cache speedup:     {:.2}x",
        uncached_time.as_secs_f64() / optimized_time.as_secs_f64()
    );
    println!(
        "generic SQL average:     {:?}",
        generic_time / iterations as u32
    );
    println!(
        "top-k speedup:           {:.2}x",
        generic_time.as_secs_f64() / optimized_time.as_secs_f64()
    );

    let snapshot = env::temp_dir().join(format!(
        "vectors-benchmark-{}-{row_count}x{dimensions}.vdb",
        std::process::id()
    ));
    let save_started = Instant::now();
    database.save(&snapshot)?;
    let save_time = save_started.elapsed();
    let snapshot_bytes = fs::metadata(&snapshot)?.len();
    let load_started = Instant::now();
    let restored = Database::open(&snapshot)?;
    let load_time = load_started.elapsed();
    black_box(restored);
    let _ = fs::remove_file(snapshot);
    println!(
        "snapshot size:           {:.2} MiB",
        snapshot_bytes as f64 / 1_048_576.0
    );
    println!("snapshot save:           {save_time:?}");
    println!("snapshot load:           {load_time:?}");
    Ok(())
}

fn environment_usize(name: &str, default: usize) -> usize {
    env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn query(database: &Database, sql: &str) -> vectors::Result<QueryResult> {
    let mut results = database.execute(sql)?;
    match results.pop() {
        Some(ExecutionResult::Query(result)) => Ok(result),
        _ => unreachable!("benchmark query must produce rows"),
    }
}

fn benchmark(
    iterations: usize,
    mut operation: impl FnMut() -> vectors::Result<Vec<ExecutionResult>>,
) -> Duration {
    let started = Instant::now();
    for _ in 0..iterations {
        black_box(operation().expect("benchmark query should succeed"));
    }
    started.elapsed()
}
