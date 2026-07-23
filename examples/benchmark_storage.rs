use std::env;
use std::fs;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use vectors::{Database, ExecutionResult, InsertConflict, Value, Vector};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rows_per_batch = setting("VECTORS_BENCH_ROWS", 1_000);
    let dimensions = setting("VECTORS_BENCH_DIMENSIONS", 64);
    let batches = setting("VECTORS_BENCH_ITERATIONS", 10);
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let directory = env::temp_dir().join(format!(
        "vectors-storage-benchmark-{}-{unique}",
        std::process::id()
    ));

    let database = Database::open_persistent(&directory)?;
    database.execute(&format!(
        "CREATE TABLE embeddings (
            id INTEGER PRIMARY KEY,
            category TEXT,
            embedding VECTOR({dimensions})
        )"
    ))?;

    let started = Instant::now();
    for batch in 0..batches {
        let rows = (0..rows_per_batch)
            .map(|row| {
                let id = batch * rows_per_batch + row;
                let values = (0..dimensions)
                    .map(|dimension| ((id + dimension) % 997) as f32 / 997.0)
                    .collect::<Vec<_>>();
                Ok(vec![
                    Value::Integer(id as i64),
                    Value::Text(format!("group-{}", id % 8)),
                    Value::Vector(Vector::new(values)?),
                ])
            })
            .collect::<vectors::Result<Vec<_>>>()?;
        database.insert_rows("embeddings", rows, InsertConflict::Fail)?;
    }
    let ingestion = started.elapsed();
    let total_rows = rows_per_batch * batches;
    let wal_bytes = fs::metadata(directory.join("vectors.wal"))?.len();
    drop(database);

    let started = Instant::now();
    let recovered = Database::open_persistent(&directory)?;
    let recovery = started.elapsed();
    let results = recovered.execute("SELECT COUNT(*) FROM embeddings")?;
    let count = match &results[0] {
        ExecutionResult::Query(result) => &result.rows[0][0],
        _ => unreachable!(),
    };
    assert_eq!(count, &Value::Integer(total_rows as i64));

    let started = Instant::now();
    recovered.checkpoint()?;
    let checkpoint = started.elapsed();
    let snapshot_bytes = fs::metadata(directory.join("vectors.vdb"))?.len();
    drop(recovered);
    fs::remove_dir_all(&directory)?;

    println!("durable storage benchmark");
    println!("rows: {total_rows}; dimensions: {dimensions}; batches: {batches}");
    println!(
        "fsynced ingestion: {:.2} ms total; {:.0} rows/s",
        ingestion.as_secs_f64() * 1_000.0,
        total_rows as f64 / ingestion.as_secs_f64()
    );
    println!(
        "WAL: {:.2} MiB; recovery: {:.2} ms",
        wal_bytes as f64 / (1024.0 * 1024.0),
        recovery.as_secs_f64() * 1_000.0
    );
    println!(
        "checkpoint: {:.2} MiB; {:.2} ms",
        snapshot_bytes as f64 / (1024.0 * 1024.0),
        checkpoint.as_secs_f64() * 1_000.0
    );
    Ok(())
}

fn setting(name: &str, default: usize) -> usize {
    env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}
