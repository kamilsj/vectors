//! Saywit-shaped, parameterized exact search with indexed chat membership.
//! Run before and after planner changes with identical arguments:
//! cargo run --release --example benchmark_scoped_search -- 20000 1024 7
//! Timings include parameter binding, SQL parsing, filtering and exact top-k.
//! Synthetic vectors only; no provider calls or user messages.
use serde_json::json;
use std::time::Instant;
use vectors::{
    ComputeConfig, ComputeDevice, Database, ExecutionResult, InsertConflict, Value, Vector,
};

fn query(db: &Database, sql: &str, parameters: &[Value]) -> vectors::QueryResult {
    let ExecutionResult::Query(result) = db
        .execute_with_parameters(sql, parameters)
        .unwrap()
        .remove(0)
    else {
        panic!("expected query");
    };
    result
}

fn percentile(samples: &[f64], fraction: f64) -> f64 {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[((sorted.len() - 1) as f64 * fraction).ceil() as usize]
}

fn main() {
    let args = std::env::args()
        .skip(1)
        .map(|arg| arg.parse::<usize>().unwrap())
        .collect::<Vec<_>>();
    let rows = args.first().copied().unwrap_or(20_000);
    let dimensions = args.get(1).copied().unwrap_or(1024);
    let repetitions = args.get(2).copied().unwrap_or(7);
    assert!(rows >= 1000 && (1..=3072).contains(&dimensions) && repetitions > 0);
    let db = Database::new_with_compute(ComputeConfig {
        device: ComputeDevice::Cpu,
        ..ComputeConfig::default()
    });
    db.execute(&format!("CREATE TABLE units (id TEXT PRIMARY KEY, chat_id TEXT, turn_id TEXT, source_id INTEGER, profile TEXT, embedding VECTOR({dimensions})); CREATE INDEX chats ON units(chat_id)")).unwrap();
    for start in (0..rows).step_by(500) {
        let batch = (start..rows.min(start + 500))
            .map(|row| {
                vec![
                    Value::Text(format!("unit-{row}")),
                    Value::Text(format!("chat-{}", row % 1000)),
                    Value::Text(format!("turn-{}", row / 4)),
                    Value::Integer(row as i64),
                    Value::Text(if row % 17 == 0 { "old" } else { "active" }.into()),
                    Value::Vector(
                        Vector::new(
                            (0..dimensions)
                                .map(|d| ((row * 31 + d * 17 + 7) % 997) as f32 / 997.0 - 0.5)
                                .collect(),
                        )
                        .unwrap(),
                    ),
                ]
            })
            .collect();
        db.insert_rows("units", batch, InsertConflict::Fail)
            .unwrap();
    }
    let mut workloads = Vec::new();
    for scope in [1, 8, 64, 500] {
        let placeholders = (3..3 + scope)
            .map(|n| format!("${n}"))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("SELECT id,turn_id,source_id,cosine_distance(embedding,$1) AS distance FROM units WHERE profile=$2 AND chat_id IN ({placeholders}) AND turn_id<>${} ORDER BY distance LIMIT 100", scope + 3);
        let mut samples = Vec::new();
        let mut examined = 0;
        let mut signature = Vec::new();
        for iteration in 0..repetitions + 1 {
            let mut parameters = vec![
                Value::Vector(
                    Vector::new(
                        (0..dimensions)
                            .map(|d| ((d * 13 + iteration * 7 + 1) % 101) as f32 / 101.0 - 0.5)
                            .collect(),
                    )
                    .unwrap(),
                ),
                Value::Text("active".into()),
            ];
            parameters.extend((0..scope).map(|n| Value::Text(format!("chat-{n}"))));
            parameters.push(Value::Text("turn-0".into()));
            let start = Instant::now();
            let result = query(&db, &sql, &parameters);
            let elapsed = start.elapsed().as_secs_f64() * 1000.0;
            // Force a non-indexed predicate and general vector expression as an
            // independent full-scan/full-sort reference, outside measured time.
            let reference = sql
                .replace("chat_id IN", "CAST(chat_id AS TEXT) IN")
                .replace(
                    "cosine_distance(embedding,$1)",
                    "cosine_distance(embedding,$1) + 0.0",
                )
                .replace("ORDER BY distance", "ORDER BY distance,source_id");
            let expected = query(&db, &reference, &parameters);
            assert_eq!(
                result.rows, expected.rows,
                "scope={scope}, iteration={iteration}"
            );
            assert_eq!(result.columns, expected.columns);
            examined = result.rows_examined;
            if iteration > 0 {
                samples.push(elapsed);
            }
            signature.push(
                result
                    .rows
                    .iter()
                    .map(|row| row[0].to_string())
                    .collect::<Vec<_>>(),
            );
        }
        workloads.push(json!({"allowed_chats":scope,"rows_examined":examined,"median_ms":percentile(&samples,0.5),"p95_ms":percentile(&samples,0.95),"samples_ms":samples,"result_ids":signature}));
    }
    println!("{}", serde_json::to_string_pretty(&json!({"rows":rows,"dimensions":dimensions,"chats":1000,"repetitions":repetitions,"threads":rayon::current_num_threads(),"compute":"cpu","profile":"release","exact_reference_parity":true,"provider_calls":0,"workloads":workloads})).unwrap());
}
