//! Bounded chat upserts into a growing index, with exact retrieval checks.
//! Run: cargo run --release --example benchmark_chat_indexing -- 100000 1024 7

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
    assert!(count >= 1024 && dimensions >= 2 && repeats > 0);
    let root = env::temp_dir().join(format!(
        "vectors-chat-indexing-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    fs::create_dir_all(&root)?;
    let mut measurements = Vec::new();
    for durable in [false, true] {
        let db = if durable {
            Database::open_persistent(&root)?
        } else {
            Database::new()
        };
        db.execute(&format!("CREATE TABLE units (id TEXT PRIMARY KEY, chat_id TEXT NOT NULL, turn_id TEXT NOT NULL, profile TEXT NOT NULL, embedding VECTOR({dimensions})); CREATE INDEX chats ON units(chat_id); CREATE INDEX turns ON units(turn_id)"))?;
        for start in (0..count).step_by(512) {
            let rows = (start..(start + 512).min(count))
                .map(|id| row(id, 0, dimensions, "original"))
                .collect::<vectors::Result<Vec<_>>>()?;
            db.insert_rows("units", rows, InsertConflict::Fail)?;
        }
        if durable {
            db.checkpoint()?;
        }
        let mut next_id = count;
        for batch in [8, 128] {
            for operation in ["append", "edit", "replay"] {
                let mut samples = Vec::new();
                for iteration in 0..=repeats {
                    let start = if operation == "append" {
                        let start = next_id;
                        next_id += batch;
                        start
                    } else {
                        count / 2
                    };
                    let phase = if operation == "replay" {
                        repeats
                    } else {
                        iteration
                    };
                    let rows = (start..start + batch)
                        .map(|id| row(id, phase, dimensions, "updated"))
                        .collect::<vectors::Result<Vec<_>>>()?;
                    let started = Instant::now();
                    let affected = db.insert_rows(
                        "units",
                        rows,
                        InsertConflict::DoUpdate {
                            target: "id".into(),
                            update_columns: vec![
                                "chat_id".into(),
                                "turn_id".into(),
                                "profile".into(),
                                "embedding".into(),
                            ],
                        },
                    )?;
                    let elapsed = started.elapsed().as_secs_f64() * 1000.;
                    assert_eq!(affected, batch);
                    if iteration > 0 {
                        samples.push(elapsed);
                    }
                }
                let mut ordered = samples.clone();
                ordered.sort_by(f64::total_cmp);
                measurements.push(json!({"storage":if durable {"durable"}else{"memory"},"operation":operation,"batch":batch,
                    "median_ms":ordered[ordered.len()/2],"p95_ms":ordered[((ordered.len() as f64*0.95).ceil() as usize-1).min(ordered.len()-1)],"samples_ms":samples}));
            }
        }
        let expected = verify(&db, dimensions, next_id)?;
        if durable {
            drop(db);
            let recovered = Database::open_persistent(&root)?;
            assert_eq!(verify(&recovered, dimensions, next_id)?, expected);
        }
    }
    fs::remove_dir_all(root)?;
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"existing_rows":count,"dimensions":dimensions,"iterations":repeats,"warmups":1,
        "measurements":measurements,"correctness":"exact scoped ranking and scores match full scan; WAL recovery matches","provider_calls":0})
        )?
    );
    Ok(())
}

fn row(id: usize, phase: usize, dimensions: usize, chat: &str) -> vectors::Result<Vec<Value>> {
    let values = (0..dimensions)
        .map(|d| ((id * 31 + d * 17 + phase * 13) % 997 + 1) as f32 / 997.)
        .collect();
    Ok(vec![
        Value::Text(format!("unit-{id:09}")),
        Value::Text(chat.into()),
        Value::Text(format!("turn-{}", id / 8)),
        Value::Text("active".into()),
        Value::Vector(Vector::new(values)?),
    ])
}

fn verify(db: &Database, dimensions: usize, expected: usize) -> vectors::Result<Vec<Vec<Value>>> {
    let ExecutionResult::Query(count) = db.execute("SELECT COUNT(*) FROM units")?.remove(0) else {
        panic!("count")
    };
    assert_eq!(count.rows[0][0], Value::Integer(expected as i64));
    let params = [Value::Vector(Vector::new(vec![1.; dimensions])?)];
    let sql="SELECT id,cosine_distance(embedding,$1) AS distance FROM units WHERE chat_id='updated' AND profile='active' ORDER BY distance LIMIT 20";
    let reference = sql
        .replace("chat_id=", "CAST(chat_id AS TEXT)=")
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
    Ok(actual.rows)
}
