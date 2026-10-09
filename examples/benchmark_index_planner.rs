//! Measure selective scalar planning and exact chat retrieval, with output parity.
//! RAYON_NUM_THREADS=4 cargo run --release --example benchmark_index_planner -- 100000 1024 50

use serde_json::json;
use std::{env, time::Instant};
use vectors::{Database, ExecutionResult, InsertConflict, Value, Vector};

fn query(db: &Database, sql: &str, parameters: &[Value]) -> vectors::QueryResult {
    let ExecutionResult::Query(result) = db
        .execute_with_parameters(sql, parameters)
        .unwrap()
        .remove(0)
    else {
        panic!("query expected")
    };
    result
}

fn main() {
    let args = env::args()
        .skip(1)
        .map(|value| value.parse::<usize>().unwrap())
        .collect::<Vec<_>>();
    let count = args.first().copied().unwrap_or(100_000);
    let dimensions = args.get(1).copied().unwrap_or(1024);
    let iterations = args.get(2).copied().unwrap_or(50);
    assert!(count >= 10_000 && dimensions >= 2 && iterations >= 5);
    let db = Database::new();
    db.execute(&format!("CREATE TABLE units(id INTEGER PRIMARY KEY,unit_key TEXT UNIQUE,chat_id TEXT,turn_id INTEGER,profile TEXT,embedding VECTOR({dimensions})); CREATE INDEX chats ON units(chat_id); CREATE INDEX turns ON units(turn_id); CREATE INDEX profiles ON units(profile)")).unwrap();
    for start in (0..count).step_by(512) {
        db.insert_rows(
            "units",
            (start..(start + 512).min(count))
                .map(|id| {
                    vec![
                        Value::Integer(id as i64),
                        Value::Text(format!("unit-{id}")),
                        Value::Text(format!("chat-{}", id / 8 % 1000)),
                        Value::Integer((id / 8) as i64),
                        Value::Text(if id % 11 == 0 { "old" } else { "active" }.into()),
                        Value::Vector(
                            Vector::new(
                                (0..dimensions)
                                    .map(|d| ((id * 31 + d * 17) % 997 + 1) as f32 / 997.)
                                    .collect(),
                            )
                            .unwrap(),
                        ),
                    ]
                })
                .collect(),
            InsertConflict::Fail,
        )
        .unwrap();
    }
    let scopes = (0..500)
        .map(|id| format!("'chat-{id}'"))
        .collect::<Vec<_>>()
        .join(",");
    let cases=[
        ("primary_key", "SELECT id FROM units WHERE id=9876".into(),false),
        ("unique_membership", "SELECT id FROM units WHERE unit_key IN ('unit-123','unit-999','missing','unit-123',NULL)".into(),false),
        ("skewed_equality", "SELECT id FROM units WHERE profile='active' AND chat_id='chat-17'".into(),false),
        ("skewed_membership", format!("SELECT id FROM units WHERE chat_id IN ({scopes}) AND turn_id=1234"),false),
        ("empty_conjunction", "SELECT id FROM units WHERE profile='active' AND turn_id=-1".into(),false),
        ("broad_intersection", format!("SELECT COUNT(*) FROM units WHERE profile='active' AND chat_id IN ({scopes})"),false),
        ("chat_scope_only", "SELECT id,cosine_distance(embedding,$1) AS distance FROM units WHERE chat_id IN ('chat-17','chat-42') AND CAST(profile AS TEXT)='active' ORDER BY distance LIMIT 20".into(),true),
        ("scoped_vector", format!("SELECT id,cosine_distance(embedding,$1) AS distance FROM units WHERE chat_id IN ({scopes}) AND turn_id=1234 AND profile='active' ORDER BY distance LIMIT 20"),true),
    ];
    let parameters = [Value::Vector(Vector::new(vec![1.; dimensions]).unwrap())];
    let mut measurements = Vec::new();
    let mut digest = 0xcbf29ce484222325_u64;
    for (name, sql, vector) in cases {
        let parameters = if vector { &parameters[..] } else { &[] };
        let mut reference = sql.clone();
        // CAST disables index lookup while preserving these scalar types.
        for (column, ty) in [
            ("unit_key", "TEXT"),
            ("chat_id", "TEXT"),
            ("turn_id", "INTEGER"),
            ("profile", "TEXT"),
        ] {
            reference = reference.replace(column, &format!("CAST({column} AS {ty})"));
        }
        reference = reference.replace("WHERE id=", "WHERE CAST(id AS INTEGER)=");
        if vector {
            reference = reference
                .replace(
                    "cosine_distance(embedding,$1)",
                    "cosine_distance(embedding,$1)+0.0",
                )
                .replace(" LIMIT 20", "");
        }
        let mut expected = query(&db, &reference, parameters).rows;
        if vector {
            expected.truncate(20);
        }
        let mut samples = Vec::new();
        let mut rows_examined = 0;
        for iteration in 0..iterations + 5 {
            let started = Instant::now();
            let actual = query(&db, &sql, parameters);
            let elapsed = started.elapsed().as_secs_f64() * 1000.;
            assert_eq!(actual.rows, expected, "{name}");
            rows_examined = actual.rows_examined;
            if iteration >= 5 {
                samples.push(elapsed);
            }
        }
        for byte in format!("{name}{expected:?}").bytes() {
            digest = (digest ^ u64::from(byte)).wrapping_mul(0x100000001b3);
        }
        let mut sorted = samples.clone();
        sorted.sort_by(f64::total_cmp);
        measurements.push(json!({"operation":name,"median_ms":sorted[sorted.len()/2],"p95_ms":sorted[(sorted.len()*95).div_ceil(100)-1],"samples_ms":samples,"rows_examined":rows_examined,"result_count":expected.len()}));
    }
    println!("{}",serde_json::to_string_pretty(&json!({"existing_rows":count,"dimensions":dimensions,"iterations":iterations,"warmups":5,"measurements":measurements,"result_digest":format!("{digest:016x}"),"correctness":"Every result matches full-scan/full-sort reference, including exact vector scores and order","provider_calls":0})).unwrap());
}
