use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};
use vectors::{Database, ExecutionResult, InsertConflict, QueryResult, Value, Vector};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self(std::env::temp_dir().join(format!(
                "vectors-delete-{}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )))
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn query(db: &Database, sql: &str) -> QueryResult {
    let ExecutionResult::Query(result) = db.execute(sql).unwrap().remove(0) else {
        panic!("query")
    };
    result
}
fn affected(results: Vec<ExecutionResult>) -> usize {
    let ExecutionResult::Command { tag, rows_affected } = results[0] else {
        panic!("delete")
    };
    assert_eq!(tag, "DELETE");
    rows_affected
}
fn setup(db: &Database, index: bool) {
    db.execute("CREATE TABLE units(id INTEGER PRIMARY KEY, turn_id TEXT, chat_id TEXT, embedding VECTOR(3)); INSERT INTO units VALUES (1,'one','a',[1,0,0]),(2,'one','a',[0,1,0]),(3,'two','b',[1,1,0]),(4,'three',NULL,NULL),(5,NULL,'b',[1,1,0])").unwrap();
    if index {
        db.execute("CREATE INDEX turns ON units(turn_id); CREATE INDEX chats ON units(chat_id)")
            .unwrap();
    }
}
fn verify_search(db: &Database) {
    for scope in ["'a'", "'b'", "'a','b'", "'missing'"] {
        let sql=format!("SELECT id,cosine_distance(embedding,[1,0,0]) AS distance FROM units WHERE chat_id IN ({scope}) ORDER BY distance LIMIT 100");
        let reference = sql
            .replace("chat_id IN", "CAST(chat_id AS TEXT) IN")
            .replace(
                "cosine_distance(embedding,[1,0,0])",
                "cosine_distance(embedding,[1,0,0])+0.0",
            )
            .replace(" LIMIT 100", "");
        let mut expected = query(db, &reference).rows;
        expected.truncate(100);
        assert_eq!(query(db, &sql).rows, expected);
    }
}

#[test]
fn indexed_predicates_match_full_evaluation_including_nulls_and_errors() {
    for predicate in [
        "turn_id='one'",
        "turn_id IN ('one','two','one',NULL)",
        "turn_id='one' OR chat_id='b'",
        "turn_id IN ('one','two') AND chat_id='b'",
        "turn_id NOT IN ('one',NULL)",
        "turn_id IS NULL",
        "turn_id='missing'",
        "turn_id='one' AND id=1",
        "turn_id='missing' AND id/0>1",
        "turn_id='one' OR id/0>1",
        "turn_id IN ('one',1/0)",
        "turn_id IN ('one',id)",
        "turn_id=42",
        "chat_id='b' AND missing=1",
        "turn_id IN (turn_id,'one')",
        "NULL",
        "FALSE",
        "TRUE",
    ] {
        let indexed = Database::new();
        let scan = Database::new();
        setup(&indexed, true);
        setup(&scan, false);
        let sql = format!("DELETE FROM units WHERE {predicate}");
        assert_eq!(indexed.execute(&sql), scan.execute(&sql), "{predicate}");
        assert_eq!(
            query(&indexed, "SELECT * FROM units").rows,
            query(&scan, "SELECT * FROM units").rows,
            "{predicate}"
        );
        verify_search(&indexed);
    }
}

#[test]
fn failed_or_empty_durable_deletes_preserve_wal_revision_and_rows() {
    let dir = Directory::new();
    let db = Database::open_persistent(&dir.0).unwrap();
    setup(&db, true);
    let rows = query(&db, "SELECT * FROM units").rows;
    let revision = db.revision().unwrap();
    let wal = fs::read(dir.0.join("vectors.wal")).unwrap();
    for sql in [
        "DELETE FROM units WHERE turn_id='missing'",
        "DELETE FROM units WHERE turn_id IN (NULL,'missing')",
        "DELETE FROM units WHERE turn_id='missing'; SELECT 1",
    ] {
        assert_eq!(affected(db.execute(sql).unwrap()), 0);
    }
    for sql in ["DELETE FROM units WHERE turn_id='missing' AND id/0>1",
        "DELETE FROM units WHERE turn_id='one'; INSERT INTO units VALUES(3,'duplicate','a',[1,0,0])",
        "DELETE FROM units WHERE turn_id='one' RETURNING id", "DELETE FROM units WHERE id=1 LIMIT 1",
        "DELETE FROM units USING units", "DELETE FROM units WHERE missing=1"] {
        assert!(db.execute(sql).is_err(),"{sql}");
    }
    assert_eq!(db.revision().unwrap(), revision);
    assert_eq!(fs::read(dir.0.join("vectors.wal")).unwrap(), wal);
    assert_eq!(query(&db, "SELECT * FROM units").rows, rows);
    verify_search(&db);
}

#[test]
fn acknowledged_delete_and_reinsert_survive_wal_and_checkpoint_recovery() {
    let dir = Directory::new();
    let db = Database::open_persistent(&dir.0).unwrap();
    setup(&db, true);
    db.checkpoint().unwrap();
    let revision = db.revision().unwrap();
    assert_eq!(
        affected(
            db.execute_with_parameters(
                "DELETE FROM units WHERE turn_id=$1",
                &[Value::Text("one".into())]
            )
            .unwrap()
        ),
        2
    );
    assert_eq!(db.revision().unwrap(), revision + 1);
    let rows = query(&db, "SELECT * FROM units").rows;
    drop(db);
    let db = Database::open_persistent(&dir.0).unwrap();
    assert_eq!(query(&db, "SELECT * FROM units").rows, rows);
    assert!(query(&db, "SELECT id FROM units WHERE turn_id='one'")
        .rows
        .is_empty());
    db.insert_rows(
        "units",
        vec![vec![
            Value::Integer(1),
            Value::Text("one".into()),
            Value::Text("a".into()),
            Value::Vector(Vector::new(vec![1., 0., 0.]).unwrap()),
        ]],
        InsertConflict::Fail,
    )
    .unwrap();
    assert_eq!(
        query(&db, "SELECT id FROM units WHERE turn_id='one'").rows,
        vec![vec![Value::Integer(1)]]
    );
    verify_search(&db);
    db.checkpoint().unwrap();
    let rows = query(&db, "SELECT * FROM units").rows;
    drop(db);
    let db = Database::open_persistent(&dir.0).unwrap();
    assert_eq!(query(&db, "SELECT * FROM units").rows, rows);
    verify_search(&db);
    assert_eq!(affected(db.execute("DELETE FROM units").unwrap()), 4);
    assert_eq!(
        affected(db.execute("DELETE FROM units WHERE missing=1").unwrap()),
        0
    );
    assert_eq!(
        query(&db, "SELECT COUNT(*) FROM units").rows[0][0],
        Value::Integer(0)
    );
}

#[test]
fn varied_slab_boundaries_deletes_and_upserts_match_reconstructed_reference() {
    let db = Database::new();
    db.execute("CREATE TABLE units(id INTEGER PRIMARY KEY, turn_id TEXT, chat_id TEXT, embedding VECTOR(3));CREATE INDEX turns ON units(turn_id);CREATE INDEX chats ON units(chat_id)").unwrap();
    let mut expected = Vec::new();
    for size in [1, 7, 4100, 3, 201] {
        let start = expected.len();
        let rows = (start..start + size)
            .map(|id| {
                vec![
                    Value::Integer(id as i64),
                    Value::Text(format!("t{}", id / 8)),
                    Value::Text(if id.is_multiple_of(2) { "a" } else { "b" }.into()),
                    if id.is_multiple_of(11) {
                        Value::Null
                    } else {
                        Value::Vector(Vector::new(vec![1., (id % 13) as f32, 0.]).unwrap())
                    },
                ]
            })
            .collect::<Vec<_>>();
        expected.extend(rows.clone());
        db.insert_rows("units", rows, InsertConflict::Fail).unwrap();
    }
    for turn in ["t0", "t512", "t1", "t513", "t40", "missing"] {
        let before = expected.len();
        expected.retain(|row| row[1] != Value::Text(turn.into()));
        assert_eq!(
            affected(
                db.execute_with_parameters(
                    "DELETE FROM units WHERE turn_id=$1",
                    &[Value::Text(turn.into())]
                )
                .unwrap()
            ),
            before - expected.len()
        );
        assert_eq!(query(&db, "SELECT * FROM units").rows, expected);
        // No LIMIT on the reference, preserving source order for equal scores.
        let sql="SELECT id,cosine_distance(embedding,[1,0,0]) AS distance FROM units WHERE chat_id='a' ORDER BY distance LIMIT 20";
        let reference = sql
            .replace("chat_id=", "CAST(chat_id AS TEXT)=")
            .replace(
                "cosine_distance(embedding,[1,0,0])",
                "cosine_distance(embedding,[1,0,0])+0.0",
            )
            .replace(" LIMIT 20", "");
        let mut result = query(&db, &reference).rows;
        result.truncate(20);
        assert_eq!(query(&db, sql).rows, result);
    }
    expected[0][3] = Value::Vector(Vector::new(vec![0., 0., 1.]).unwrap());
    db.insert_rows(
        "units",
        vec![expected[0].clone()],
        InsertConflict::DoUpdate {
            target: "id".into(),
            update_columns: vec!["embedding".into()],
        },
    )
    .unwrap();
    assert_eq!(query(&db, "SELECT * FROM units").rows, expected);
    verify_search(&db);
}

#[test]
fn remapped_scalar_and_unique_indexes_match_rebuilt_indexes() {
    let db = Database::new();
    db.execute("CREATE TABLE items(id INTEGER PRIMARY KEY,code TEXT UNIQUE,bucket INTEGER); CREATE INDEX codes ON items(code); CREATE INDEX buckets ON items(bucket); INSERT INTO items VALUES(1,'a',1),(2,'b',2),(3,NULL,1),(4,'d',1),(5,'e',NULL),(6,NULL,2),(7,'g',1)").unwrap();
    db.execute("DELETE FROM items WHERE code IN ('b','d','e')")
        .unwrap();
    let expected = ["bucket=1", "bucket=2", "code='a'", "code='g'", "code='b'"].map(|predicate| {
        let sql = format!("SELECT id FROM items WHERE {predicate}");
        let result = query(&db, &sql);
        (sql, result)
    });
    db.execute("DROP INDEX codes; DROP INDEX buckets; CREATE INDEX codes ON items(code); CREATE INDEX buckets ON items(bucket)").unwrap();
    for (sql, expected) in expected {
        assert_eq!(query(&db, &sql), expected);
    }
    for sql in [
        "INSERT INTO items VALUES(8,'g',1)",
        "INSERT INTO items VALUES(7,'new',1)",
    ] {
        assert!(db.execute(sql).is_err());
    }
    db.execute("INSERT INTO items VALUES(2,'b',2); INSERT INTO items VALUES(7,'updated',2) ON CONFLICT(id) DO UPDATE SET code=excluded.code,bucket=excluded.bucket").unwrap();
    assert_eq!(
        query(&db, "SELECT id,code FROM items WHERE bucket=2").rows,
        vec![
            vec![Value::Integer(6), Value::Null],
            vec![Value::Integer(7), Value::Text("updated".into())],
            vec![Value::Integer(2), Value::Text("b".into())]
        ]
    );
    assert!(query(&db, "SELECT id FROM items WHERE code='g'")
        .rows
        .is_empty());
}
