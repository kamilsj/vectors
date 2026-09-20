//! Measure complete vector-ranked joins with and without a maintained scalar index.
//! Run: cargo run --release --example benchmark_sql_join -- 50000 128 20
//! Fixture writes, index creation and output serialization are excluded.
use serde_json::json;
use std::time::Instant;
use vectors::{
    ComputeConfig, ComputeDevice, Database, ExecutionResult, InsertConflict, QueryResult, Value,
    Vector,
};

fn percentile(values: &[f64], percentile: f64) -> f64 {
    let mut values = values.to_vec();
    values.sort_by(f64::total_cmp);
    values[((values.len() - 1) as f64 * percentile).ceil() as usize]
}
fn vector(seed: usize, dimensions: usize) -> Vector {
    Vector::new(
        (0..dimensions)
            .map(|i| (((seed * 31 + i * 17) % 101) as f32 - 50.0) / 50.0)
            .collect(),
    )
    .unwrap()
    .normalized()
    .unwrap()
}
fn query(db: &Database, sql: &str) -> (QueryResult, f64) {
    let start = Instant::now();
    let mut output = db.execute(sql).unwrap();
    let elapsed = start.elapsed().as_secs_f64() * 1_000_000.0;
    let ExecutionResult::Query(result) = output.remove(0) else {
        panic!("query result expected")
    };
    (result, elapsed)
}
fn canonical(result: &QueryResult) -> serde_json::Value {
    let rows = result
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|value| match value {
                    Value::Integer(value) => json!(value),
                    Value::Float(value) => json!(value),
                    Value::Text(value) => json!(value),
                    Value::Boolean(value) => json!(value),
                    Value::Null => serde_json::Value::Null,
                    Value::Vector(value) => json!(value.as_slice()),
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    json!({"columns":result.columns,"column_types":result.column_types.iter().map(|kind|kind.as_ref().map(ToString::to_string)).collect::<Vec<_>>(),"rows":rows,"rows_examined":result.rows_examined})
}
fn run(
    rows: usize,
    dimensions: usize,
    repetitions: usize,
    groups: usize,
    phase: &str,
) -> serde_json::Value {
    let db = Database::new_with_compute(ComputeConfig {
        device: ComputeDevice::Cpu,
        ..ComputeConfig::default()
    });
    db.execute(&format!("CREATE TABLE probes (topic INTEGER PRIMARY KEY); CREATE TABLE indexed_rows (id INTEGER PRIMARY KEY,topic INTEGER,published BOOLEAN,embedding VECTOR({dimensions})); CREATE TABLE ephemeral_rows (id INTEGER PRIMARY KEY,topic INTEGER,published BOOLEAN,embedding VECTOR({dimensions}));")).unwrap();
    db.insert_rows(
        "probes",
        (0..64).map(|id| vec![Value::Integer(id)]).collect(),
        InsertConflict::Fail,
    )
    .unwrap();
    for start in (0..rows).step_by(1000) {
        let batch = (start..rows.min(start + 1000))
            .map(|id| {
                vec![
                    Value::Integer(id as i64),
                    Value::Integer((id % groups) as i64),
                    Value::Boolean(!(id / groups).is_multiple_of(4) || groups == rows),
                    Value::Vector(vector(id, dimensions)),
                ]
            })
            .collect::<Vec<_>>();
        db.insert_rows("indexed_rows", batch.clone(), InsertConflict::Fail)
            .unwrap();
        db.insert_rows("ephemeral_rows", batch, InsertConflict::Fail)
            .unwrap();
    }
    db.execute("CREATE INDEX corpus_topic ON indexed_rows USING HASH (topic)")
        .unwrap();
    let make_sql = |table: &str, iteration: usize| {
        format!("SELECT p.topic,r.id,cosine_distance(r.embedding,ARRAY[{}]) AS distance FROM probes p JOIN {table} r ON p.topic=r.topic WHERE r.published=true ORDER BY distance,r.id LIMIT 10",vector(iteration,dimensions).as_slice().iter().map(|v|format!("{v:?}")).collect::<Vec<_>>().join(","))
    };
    // Prime both exact queries and SQL parse caches before measurement.
    let queries = (0..repetitions)
        .map(|iteration| {
            [
                make_sql("indexed_rows", iteration),
                make_sql("ephemeral_rows", iteration),
            ]
        })
        .collect::<Vec<_>>();
    for pair in &queries {
        assert_eq!(query(&db, &pair[0]).0, query(&db, &pair[1]).0);
    }
    let mut samples = [Vec::new(), Vec::new()];
    let mut results = Vec::new();
    for (iteration, pair) in queries.iter().enumerate() {
        let order = if iteration % 2 == 0 { [0, 1] } else { [1, 0] };
        let mut output = [None, None];
        for variant in order {
            let (result, time) = query(&db, &pair[variant]);
            samples[variant].push(time);
            output[variant] = Some(result);
        }
        assert_eq!(output[0], output[1]);
        results.push(canonical(output[0].as_ref().unwrap()));
    }
    let summary = |variant: usize| json!({"median_us":percentile(&samples[variant],0.5),"p95_us":percentile(&samples[variant],0.95),"samples_us":samples[variant]});
    json!({"workload":phase,"right_rows":rows,"probe_keys":64,"key_groups":groups,"dimensions":dimensions,"repetitions":repetitions,"result_limit":10,"metric":"cosine_distance","indexed":summary(0),"ephemeral":summary(1),"all_results_identical":true,"canonical_results":results})
}
fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let arg = |index: usize, default: usize| {
        args.get(index).map_or(default, |value| {
            value.parse().expect("positive integer argument")
        })
    };
    let rows = arg(0, 50_000);
    let dimensions = arg(1, 128);
    let repetitions = arg(2, 20);
    assert!(
        args.len() <= 3
            && rows >= 500
            && (2..=65535).contains(&dimensions)
            && repetitions > 0
            && repetitions <= 32
    );
    println!("{}",serde_json::to_string_pretty(&json!({"compute":"cpu","profile":"release","scope":"Complete in-process SELECT JOIN with residual predicate, exact cosine ranking, deterministic tie breaker and LIMIT10. SQL parse caches warmed. Excludes fixture writes/index construction/result serialization; compares maintained right HASH index with per-query temporary hash construction in the same executor.","phases":[run(rows,dimensions,repetitions,rows,"sparse_unique_keys"),run(rows,dimensions,repetitions,500,"one_to_many_keys")]})).unwrap());
}
