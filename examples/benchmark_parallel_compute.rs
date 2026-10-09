//! Concurrent exact searches; compare the same harness on both revisions.
use std::sync::Barrier;
use std::time::Instant;
use vectors::{
    ComputeConfig, ComputeDevice, Database, ExecutionResult, InsertConflict, Value, Vector,
};

fn query(database: &Database, sql: &str) -> vectors::QueryResult {
    match database.execute(sql).unwrap().remove(0) {
        ExecutionResult::Query(result) => result,
        _ => panic!("expected query"),
    }
}

fn verify(actual: &vectors::QueryResult, expected: &vectors::QueryResult) {
    assert_eq!(actual.rows_examined, expected.rows_examined);
    assert_eq!(actual.rows.len(), expected.rows.len());
    for (actual, expected) in actual.rows.iter().zip(&expected.rows) {
        assert_eq!(actual[0], expected[0], "ordered IDs changed");
        let (Value::Float(a), Value::Float(b)) = (&actual[1], &expected[1]) else {
            panic!("expected score")
        };
        assert!((a - b).abs() <= 1e-5, "score differs from CPU reference");
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let policy = ComputeDevice::parse(args.first().map(String::as_str).unwrap_or("auto"))
        .ok_or("expected cpu, auto or gpu")?;
    let rows: usize = args
        .get(1)
        .map(String::as_str)
        .unwrap_or("100000")
        .parse()?;
    let dimensions: usize = args.get(2).map(String::as_str).unwrap_or("1024").parse()?;
    let concurrency: usize = args.get(3).map(String::as_str).unwrap_or("4").parse()?;
    let iterations: usize = args.get(4).map(String::as_str).unwrap_or("100").parse()?;
    let cache_mib: usize = args.get(5).map(String::as_str).unwrap_or("4096").parse()?;
    if rows < 100 || dimensions < 2 || concurrency == 0 || iterations == 0 {
        return Err("invalid benchmark sizes".into());
    }
    let database = Database::new_with_compute(ComputeConfig {
        device: policy,
        gpu_min_elements: 32 * 1024 * 1024,
        gpu_cache_bytes: cache_mib.checked_mul(1024 * 1024).ok_or("cache overflow")?,
    });
    database.execute(&format!("CREATE TABLE units(id INTEGER PRIMARY KEY, scope INTEGER, profile TEXT, embedding VECTOR({dimensions})); CREATE INDEX unit_scope ON units(scope)"))?;
    for start in (0..rows).step_by(512) {
        database.insert_rows(
            "units",
            (start..(start + 512).min(rows))
                .map(|id| {
                    let mut vector = vec![0.001 + id as f32 / rows as f32 * 0.1; dimensions];
                    vector[0] = 1.0;
                    vec![
                        Value::Integer(id as i64),
                        Value::Integer((id % 32) as i64),
                        Value::Text("current".into()),
                        Value::Vector(Vector::new(vector).unwrap()),
                    ]
                })
                .collect(),
            InsertConflict::Fail,
        )?;
    }
    let vector = std::iter::once("1")
        .chain(std::iter::repeat_n("0", dimensions - 1))
        .collect::<Vec<_>>()
        .join(",");
    let projection = format!("id,cosine_distance(embedding,ARRAY[{vector}]) AS distance");
    let suffix = " FROM units WHERE profile='current' AND id<>7 ORDER BY distance LIMIT 20";
    let sql = format!("SELECT {projection}{suffix}");
    let expected = query(
        &database,
        &format!("SELECT {projection},id+0 AS force_cpu{suffix}"),
    );
    let cold = Instant::now();
    verify(&query(&database, &sql), &expected);
    let cold_ms = cold.elapsed().as_secs_f64() * 1000.0;
    for _ in 0..5 {
        verify(&query(&database, &sql), &expected);
    }
    let barrier = Barrier::new(concurrency + 1);
    let (wall_seconds, mut samples) = std::thread::scope(|scope| {
        let handles = (0..concurrency)
            .map(|_| {
                let (database, sql, expected, barrier) = (&database, &sql, &expected, &barrier);
                scope.spawn(move || {
                    let mut samples = Vec::with_capacity(iterations);
                    barrier.wait();
                    for _ in 0..iterations {
                        let start = Instant::now();
                        let actual = query(database, sql);
                        samples.push(start.elapsed().as_secs_f64() * 1000.0);
                        verify(&actual, expected);
                    }
                    samples
                })
            })
            .collect::<Vec<_>>();
        let start = Instant::now();
        barrier.wait();
        let samples = handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect::<Vec<_>>();
        (start.elapsed().as_secs_f64(), samples)
    });
    samples.sort_by(f64::total_cmp);
    let count = samples.len();
    println!(
        "{}",
        serde_json::json!({"policy":policy.to_string(),"rows":rows,
        "dimensions":dimensions,"concurrency":concurrency,"queries":count,
        "cache_mib":cache_mib,"cold_ms":cold_ms,"wall_seconds":wall_seconds,
        "queries_per_second":count as f64/wall_seconds,"median_ms":samples[count/2],
        "p95_ms":samples[(count*95).div_ceil(100)-1],"samples_ms":samples,
        "correctness":"Every query matches ordered CPU reference IDs; scores within 1e-5"})
    );
    Ok(())
}
