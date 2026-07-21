//! Compare typed bulk insertion with equivalent SQL value parsing.
//!
//! Run with `cargo run --release --example benchmark_ingestion`.

use std::env;
use std::fmt::Write as _;
use std::hint::black_box;
use std::time::{Duration, Instant};

use vectors::{Database, ExecutionResult, InsertConflict, Value, Vector};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let row_count = environment_usize("VECTORS_BENCH_ROWS", 1_000);
    let dimensions = environment_usize("VECTORS_BENCH_DIMENSIONS", 64);
    let iterations = environment_usize("VECTORS_BENCH_ITERATIONS", 10);
    let typed_rows = generate_typed_rows(row_count, dimensions)?;
    let sql = generate_insert_sql(&typed_rows);

    // Prepare databases and owned inputs before starting either timer. This
    // isolates insertion, validation, and SQL parsing from data generation.
    let typed_databases = prepare_databases(iterations, dimensions)?;
    let sql_databases = prepare_databases(iterations, dimensions)?;
    let typed_inputs = (0..iterations)
        .map(|_| typed_rows.clone())
        .collect::<Vec<_>>();

    let typed_time = benchmark_typed(typed_databases, typed_inputs, row_count)?;
    let sql_time = benchmark_sql(sql_databases, &sql, row_count)?;
    println!("rows per batch:          {row_count}");
    println!("vector dimensions:       {dimensions}");
    println!(
        "typed insert average:    {:?}",
        typed_time / iterations as u32
    );
    println!(
        "SQL insert average:      {:?}",
        sql_time / iterations as u32
    );
    println!(
        "typed ingestion speedup: {:.2}x",
        sql_time.as_secs_f64() / typed_time.as_secs_f64()
    );
    Ok(())
}

fn prepare_databases(count: usize, dimensions: usize) -> vectors::Result<Vec<Database>> {
    (0..count)
        .map(|_| {
            let database = Database::new();
            database.execute(&format!(
                "CREATE TABLE ingestion (
                    id INTEGER PRIMARY KEY,
                    label TEXT NOT NULL,
                    category TEXT,
                    embedding VECTOR({dimensions})
                )"
            ))?;
            Ok(database)
        })
        .collect()
}

fn generate_typed_rows(
    row_count: usize,
    dimensions: usize,
) -> Result<Vec<Vec<Value>>, Box<dyn std::error::Error>> {
    (0..row_count)
        .map(|row| {
            let vector = (0..dimensions)
                .map(|dimension| ((row * 31 + dimension * 17 + 1) % 997) as f32 / 997.0)
                .collect::<Vec<_>>();
            Ok(vec![
                Value::Integer(row as i64),
                Value::Text(format!("ingestion-row-{row:08}")),
                Value::Text(if row % 2 == 0 { "even" } else { "odd" }.into()),
                Value::Vector(Vector::new(vector)?),
            ])
        })
        .collect()
}

fn generate_insert_sql(rows: &[Vec<Value>]) -> String {
    let mut sql = String::from("INSERT INTO ingestion VALUES ");
    for (row_index, row) in rows.iter().enumerate() {
        if row_index != 0 {
            sql.push_str(", ");
        }
        let [Value::Integer(id), Value::Text(label), Value::Text(category), Value::Vector(vector)] =
            row.as_slice()
        else {
            unreachable!("benchmark rows have a fixed shape");
        };
        write!(sql, "({id}, '{label}', '{category}', ARRAY[")
            .expect("writing to a String cannot fail");
        for (dimension, value) in vector.as_slice().iter().enumerate() {
            if dimension != 0 {
                sql.push(',');
            }
            write!(sql, "{value}").expect("writing to a String cannot fail");
        }
        sql.push_str("])");
    }
    sql
}

fn benchmark_typed(
    databases: Vec<Database>,
    inputs: Vec<Vec<Vec<Value>>>,
    expected_rows: usize,
) -> vectors::Result<Duration> {
    let started = Instant::now();
    for (database, rows) in databases.into_iter().zip(inputs) {
        let affected = database.insert_rows("ingestion", rows, InsertConflict::Fail)?;
        assert_eq!(affected, expected_rows, "typed insert lost rows");
        black_box(affected);
    }
    Ok(started.elapsed())
}

fn benchmark_sql(
    databases: Vec<Database>,
    sql: &str,
    expected_rows: usize,
) -> vectors::Result<Duration> {
    let started = Instant::now();
    for database in databases {
        let results = database.execute(sql)?;
        assert!(matches!(
            results.as_slice(),
            [ExecutionResult::Command {
                tag: "INSERT",
                rows_affected,
            }] if *rows_affected == expected_rows
        ));
        black_box(results);
    }
    Ok(started.elapsed())
}

fn environment_usize(name: &str, default: usize) -> usize {
    env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}
