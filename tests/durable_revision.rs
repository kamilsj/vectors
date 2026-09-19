use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use vectors::{Database, ExecutionResult, InsertConflict, Value};

static DIRECTORY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "vectors-durable-revision-{}-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(),
            DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        )))
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn revisions_survive_wal_and_checkpoint_recovery_across_all_write_paths() {
    let directory = TestDirectory::new();
    let database = Database::open_persistent(&directory.0).unwrap();
    assert_eq!(database.revision().unwrap(), 0);
    // Several statements form one durable commit, including schema and row edits.
    database.execute("CREATE TABLE items (id INTEGER PRIMARY KEY, value TEXT); INSERT INTO items VALUES (1, 'initial'); UPDATE items SET value = 'committed' WHERE id = 1;").unwrap();
    let first_revision = database.revision().unwrap();
    assert_eq!(first_revision, 1);
    drop(database);

    let database = Database::open_persistent(&directory.0).unwrap();
    assert_eq!(database.revision().unwrap(), first_revision);
    database
        .insert_rows(
            "items",
            vec![vec![Value::Integer(2), Value::Text("typed".into())]],
            InsertConflict::Fail,
        )
        .unwrap();
    let typed_revision = database.revision().unwrap();
    assert!(typed_revision > first_revision);
    database.checkpoint().unwrap();
    assert_eq!(database.revision().unwrap(), typed_revision);
    drop(database);

    let database = Database::open_persistent(&directory.0).unwrap();
    assert_eq!(database.revision().unwrap(), typed_revision);
    database
        .execute("INSERT INTO items VALUES (3, 'sql fast path')")
        .unwrap();
    let sql_revision = database.revision().unwrap();
    assert!(sql_revision > typed_revision);
    drop(database);

    let database = Database::open_persistent(&directory.0).unwrap();
    assert_eq!(database.revision().unwrap(), sql_revision);
    let results = database
        .execute("SELECT id, value FROM items ORDER BY id")
        .unwrap();
    let [ExecutionResult::Query(result)] = results.as_slice() else {
        panic!("expected query result");
    };
    assert_eq!(result.rows.len(), 3);
    assert_eq!(result.rows[0][1], Value::Text("committed".into()));
}

#[test]
fn failed_and_noop_writes_do_not_advance_a_durable_revision() {
    let directory = TestDirectory::new();
    let database = Database::open_persistent(&directory.0).unwrap();
    database.execute("CREATE TABLE items (id INTEGER PRIMARY KEY, value TEXT); INSERT INTO items VALUES (1, 'original');").unwrap();
    let revision = database.revision().unwrap();
    database
        .execute("INSERT INTO items VALUES (1, 'ignored') ON CONFLICT (id) DO NOTHING;")
        .unwrap();
    assert_eq!(database.revision().unwrap(), revision);
    assert_eq!(
        database
            .insert_rows(
                "items",
                vec![vec![Value::Integer(1), Value::Text("also ignored".into())]],
                InsertConflict::DoNothing {
                    target: Some("id".into())
                },
            )
            .unwrap(),
        0
    );
    assert_eq!(database.revision().unwrap(), revision);
    assert!(database
        .execute(
            "UPDATE items SET value = 'must roll back'; INSERT INTO items VALUES (1, 'duplicate');"
        )
        .is_err());
    assert_eq!(database.revision().unwrap(), revision);
    drop(database);

    let database = Database::open_persistent(&directory.0).unwrap();
    assert_eq!(database.revision().unwrap(), revision);
    let results = database.execute("SELECT value FROM items").unwrap();
    let [ExecutionResult::Query(result)] = results.as_slice() else {
        panic!("expected query result");
    };
    assert_eq!(result.rows, vec![vec![Value::Text("original".into())]]);
}
