//! Small reproducible benchmark for snapshot loading and SQL vector top-k.
//!
//! Run with `cargo run --release --example benchmark_vector_search`.
//! Add `--features gpu -- --compute auto` to exercise automatic GPU selection.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::hint::black_box;
use std::io;
use std::time::{Duration, Instant};

use vectors::{ComputeConfig, ComputeDevice, Database, ExecutionResult, QueryResult, Value};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let row_count = environment_usize("VECTORS_BENCH_ROWS", 20_000);
    let dimensions = environment_usize("VECTORS_BENCH_DIMENSIONS", 64);
    let iterations = environment_usize("VECTORS_BENCH_ITERATIONS", 8);
    let mut compute = ComputeConfig::default();
    compute.device = compute_device()?;
    compute.gpu_min_elements =
        environment_usize("VECTORS_GPU_MIN_ELEMENTS", compute.gpu_min_elements);
    compute.gpu_cache_bytes = environment_usize("VECTORS_GPU_CACHE_BYTES", compute.gpu_cache_bytes);
    println!(
        "compute policy:           {} (GPU crossover: {} elements, cache: {:.0} MiB)",
        compute.device,
        compute.gpu_min_elements,
        compute.gpu_cache_bytes as f64 / 1_048_576.0
    );
    let database = Database::new_with_compute(compute);
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

    let optimized_ids = neighbor_ids(query(&database, &optimized)?)?;
    let generic_ids = neighbor_ids(query(&database, &generic)?)?;
    // GPU and CPU accumulators need not produce byte-identical floating-point
    // scores. The primary key is the stable correctness boundary, and sorting
    // also avoids assigning meaning to SQL rows tied on distance.
    assert_eq!(
        optimized_ids, generic_ids,
        "optimized and generic plans returned different neighbors"
    );
    println!("verified neighbors:       {} ids", optimized_ids.len());

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

fn compute_device() -> Result<ComputeDevice, Box<dyn std::error::Error>> {
    let mut requested = env::var("VECTORS_COMPUTE_DEVICE").ok();
    let mut arguments = env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--compute" => {
                requested = Some(arguments.next().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "--compute requires a value")
                })?);
            }
            "-h" | "--help" => {
                println!(
                    "Usage: benchmark_vector_search [--compute cpu|auto|gpu]\n\n\
                     VECTORS_COMPUTE_DEVICE provides the same setting when the option is omitted."
                );
                std::process::exit(0);
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown argument '{argument}'"),
                )
                .into());
            }
        }
    }
    let requested = requested.unwrap_or_else(|| "cpu".into());
    ComputeDevice::parse(&requested).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid compute device '{requested}'; use cpu, auto, or gpu"),
        )
        .into()
    })
}

fn neighbor_ids(result: QueryResult) -> Result<Vec<i64>, Box<dyn std::error::Error>> {
    let mut ids = result
        .rows
        .into_iter()
        .map(|row| match row.first() {
            Some(Value::Integer(id)) => Ok(*id),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "benchmark query did not return an integer id",
            )),
        })
        .collect::<Result<Vec<_>, _>>()?;
    ids.sort_unstable();
    Ok(ids)
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
