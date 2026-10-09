//! Measure chat-turn deletion and replacement, with exact ranking and recovery checks.
//! cargo run --release --example benchmark_chat_cleanup -- 100000 1024 7

use serde_json::json;
use std::{
    env, fs,
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use vectors::{Database, ExecutionResult, InsertConflict, Value, Vector};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = env::args()
        .skip(1)
        .map(|x| x.parse::<usize>())
        .collect::<Result<Vec<_>, _>>()?;
    let count = args.first().copied().unwrap_or(100_000);
    let dimensions = args.get(1).copied().unwrap_or(1024);
    let repeats = args.get(2).copied().unwrap_or(7);
    assert!(count >= 1024 && dimensions >= 2 && repeats > 0 && (repeats + 1) * 16 < count);
    let root = env::temp_dir().join(format!(
        "vectors-chat-cleanup-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    let mut measurements = Vec::new();
    let mut results = Vec::new();
    for durable in [false, true] {
        let db = if durable {
            Database::open_persistent(&root)?
        } else {
            Database::new()
        };
        db.execute(&format!("CREATE TABLE units(id TEXT PRIMARY KEY,chat_id TEXT,turn_id TEXT,profile TEXT,embedding VECTOR({dimensions})); CREATE INDEX chats ON units(chat_id); CREATE INDEX turns ON units(turn_id)"))?;
        for start in (0..count).step_by(512) {
            db.insert_rows(
                "units",
                (start..(start + 512).min(count))
                    .map(|id| row(id, dimensions))
                    .collect::<vectors::Result<_>>()?,
                InsertConflict::Fail,
            )?;
        }
        if durable {
            db.checkpoint()?;
        }
        for operation in ["missing_turn", "delete_turn", "replace_turn"] {
            let mut samples = Vec::new();
            for iteration in 0..=repeats {
                let turn = if operation == "missing_turn" {
                    count + iteration
                } else if operation == "delete_turn" {
                    iteration
                } else {
                    count / 16 + iteration
                };
                let parameters = [Value::Text(format!("turn-{turn}"))];
                let rows = if operation == "replace_turn" {
                    (turn * 8..turn * 8 + 8)
                        .map(|id| row(id, dimensions))
                        .collect::<vectors::Result<Vec<_>>>()?
                } else {
                    Vec::new()
                };
                let started = Instant::now();
                let ExecutionResult::Command { rows_affected, .. } = db
                    .execute_with_parameters("DELETE FROM units WHERE turn_id=$1", &parameters)?
                    .remove(0)
                else {
                    panic!("delete")
                };
                if operation == "replace_turn" {
                    assert_eq!(
                        db.insert_rows(
                            "units",
                            rows,
                            InsertConflict::DoUpdate {
                                target: "id".into(),
                                update_columns: vec![
                                    "chat_id".into(),
                                    "turn_id".into(),
                                    "profile".into(),
                                    "embedding".into()
                                ]
                            }
                        )?,
                        8
                    );
                }
                let elapsed = started.elapsed().as_secs_f64() * 1000.;
                assert_eq!(
                    rows_affected,
                    if operation == "missing_turn" { 0 } else { 8 }
                );
                if iteration > 0 {
                    samples.push(elapsed);
                }
            }
            let mut ordered = samples.clone();
            ordered.sort_by(f64::total_cmp);
            measurements.push(json!({"storage":if durable{"durable"}else{"memory"},"operation":operation,
                "median_ms":ordered[ordered.len()/2],"p95_ms":ordered[((ordered.len() as f64*0.95).ceil() as usize-1).min(ordered.len()-1)],"samples_ms":samples}));
        }
        let result = verify(&db, count, dimensions, (repeats + 1) * 8)?;
        if durable {
            drop(db);
            let recovered = Database::open_persistent(&root)?;
            assert_eq!(
                verify(&recovered, count, dimensions, (repeats + 1) * 8)?,
                result
            );
        }
        results.push(result);
    }
    assert_eq!(results[0], results[1]);
    fs::remove_dir_all(root)?;
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"existing_rows":count,"dimensions":dimensions,"iterations":repeats,"warmups":1,
        "measurements":measurements,"result_digest":results[0],"correctness":"every retained ID and exact scoped rank/score checked; durable recovery matches","provider_calls":0})
        )?
    );
    Ok(())
}

fn row(id: usize, dimensions: usize) -> vectors::Result<Vec<Value>> {
    let vector = (0..dimensions)
        .map(|d| ((id * 31 + d * 17) % 997 + 1) as f32 / 997.)
        .collect();
    Ok(vec![
        Value::Text(format!("unit-{id:09}")),
        Value::Text(format!("chat-{}", id / 8 % 1000)),
        Value::Text(format!("turn-{}", id / 8)),
        Value::Text("active".into()),
        Value::Vector(Vector::new(vector)?),
    ])
}
fn verify(
    db: &Database,
    count: usize,
    dimensions: usize,
    removed: usize,
) -> vectors::Result<String> {
    let ExecutionResult::Query(ids) = db.execute("SELECT id FROM units ORDER BY id")?.remove(0)
    else {
        panic!("query")
    };
    assert_eq!(ids.rows.len(), count - removed);
    for (row, id) in ids.rows.iter().zip(removed..count) {
        assert_eq!(row[0], Value::Text(format!("unit-{id:09}")));
    }
    let params = [Value::Vector(Vector::new(vec![1.; dimensions])?)];
    let sql="SELECT id,cosine_distance(embedding,$1) AS distance FROM units WHERE chat_id IN ('chat-20','chat-300','chat-700') AND profile='active' ORDER BY distance LIMIT 20";
    let reference = sql
        .replace("chat_id IN", "CAST(chat_id AS TEXT) IN")
        .replace(
            "cosine_distance(embedding,$1)",
            "cosine_distance(embedding,$1)+0.0",
        )
        .replace(" LIMIT 20", "");
    let ExecutionResult::Query(actual) = db.execute_with_parameters(sql, &params)?.remove(0) else {
        panic!("query")
    };
    let ExecutionResult::Query(mut expected) =
        db.execute_with_parameters(&reference, &params)?.remove(0)
    else {
        panic!("query")
    };
    expected.rows.truncate(20);
    assert_eq!(actual.rows, expected.rows);
    // Stable FNV digest for comparing IDs and exact score representations across builds.
    let mut digest = 0xcbf29ce484222325_u64;
    for byte in format!("{:?}{:?}", ids.rows, actual.rows).bytes() {
        digest = (digest ^ u64::from(byte)).wrapping_mul(0x100000001b3);
    }
    Ok(format!("{digest:016x}"))
}
