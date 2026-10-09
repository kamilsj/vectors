//! Compare cold and warm CPU/GPU/automatic searches on identical synthetic data.
use std::time::Instant;
use vectors::{
    ComputeConfig, ComputeDevice, Database, ExecutionResult, InsertConflict, Value, Vector,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let policy = ComputeDevice::parse(args.first().map(String::as_str).unwrap_or("cpu"))
        .ok_or("expected cpu, gpu or auto")?;
    let rows: usize = args.get(1).map(String::as_str).unwrap_or("20000").parse()?;
    let dimensions: usize = args.get(2).map(String::as_str).unwrap_or("1024").parse()?;
    let iterations: usize = args.get(3).map(String::as_str).unwrap_or("40").parse()?;
    if rows < 100 || dimensions < 2 || iterations == 0 {
        return Err("invalid benchmark sizes".into());
    }
    let threshold = std::env::var("VECTORS_GPU_MIN_ELEMENTS")
        .ok()
        .map(|s| s.parse())
        .transpose()?
        .unwrap_or(8 * 1024 * 1024);
    let database = Database::new_with_compute(ComputeConfig {
        device: policy,
        gpu_min_elements: threshold,
        ..ComputeConfig::default()
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
    let query = std::iter::once("1")
        .chain(std::iter::repeat_n("0", dimensions - 1))
        .collect::<Vec<_>>()
        .join(",");
    let mut measurements = Vec::new();
    for (name, predicate) in [
        ("broad_residual", "profile='current' AND id<>7"),
        ("scoped_residual", "scope=3 AND profile='current' AND id<>7"),
    ] {
        let projection = format!("id,cosine_distance(embedding,ARRAY[{query}]) AS distance");
        let suffix = format!(" FROM units WHERE {predicate} ORDER BY distance LIMIT 20");
        let sql = format!("SELECT {projection}{suffix}");
        let reference_sql = format!("SELECT {projection},id+0 AS force_cpu{suffix}");
        let run = |sql: &str| -> Result<vectors::QueryResult, Box<dyn std::error::Error>> {
            match database.execute(sql)?.remove(0) {
                ExecutionResult::Query(result) => Ok(result),
                _ => Err("expected query".into()),
            }
        };
        let expected = run(&reference_sql)?;
        let cold = Instant::now();
        let actual = run(&sql)?;
        let cold_ms = cold.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(
            actual.rows.iter().map(|r| &r[0]).collect::<Vec<_>>(),
            expected.rows.iter().map(|r| &r[0]).collect::<Vec<_>>()
        );
        for (a, b) in actual.rows.iter().zip(&expected.rows) {
            let (Value::Float(a), Value::Float(b)) = (&a[1], &b[1]) else {
                panic!("expected score")
            };
            assert!((a - b).abs() <= 1e-5, "GPU score mismatch");
        }
        for _ in 0..5 {
            std::hint::black_box(run(&sql)?);
        }
        let mut samples = Vec::new();
        for _ in 0..iterations {
            let started = Instant::now();
            std::hint::black_box(run(&sql)?);
            samples.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        let mut sorted = samples.clone();
        sorted.sort_by(f64::total_cmp);
        measurements.push(serde_json::json!({"name":name,"rows_examined":actual.rows_examined,"cold_ms":cold_ms,"median_ms":sorted[iterations/2],"p95_ms":sorted[(iterations*95).div_ceil(100)-1],"samples_ms":samples,"correctness":"same ordered IDs and scores within 1e-5 of generic CPU query"}));
    }
    println!(
        "{}",
        serde_json::json!({"policy":policy.to_string(),"rows":rows,"dimensions":dimensions,"iterations":iterations,"gpu_min_elements":threshold,"measurements":measurements})
    );
    Ok(())
}
