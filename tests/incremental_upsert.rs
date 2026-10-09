use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use vectors::{Database, ExecutionResult, InsertConflict, QueryResult, Value, Vector};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("vectors-upsert-{}-{}",
            std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos())))
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

fn setup(db: &Database) {
    db.execute("CREATE TABLE units (id INTEGER PRIMARY KEY, chat TEXT, slug TEXT UNIQUE, embedding VECTOR(2));
        CREATE INDEX chats ON units(chat); CREATE INDEX slugs ON units(slug);
        INSERT INTO units VALUES (1,'a','one',[1,0]),(2,'b','two',[0,1]),(3,'a','three',[1,1])").unwrap();
}

fn row(id: i64, chat: Option<&str>, slug: Option<&str>, embedding: Option<[f32; 2]>) -> Vec<Value> {
    vec![
        Value::Integer(id),
        chat.map_or(Value::Null, |x| Value::Text(x.into())),
        slug.map_or(Value::Null, |x| Value::Text(x.into())),
        embedding.map_or(Value::Null, |x| {
            Value::Vector(Vector::new(x.to_vec()).unwrap())
        }),
    ]
}

fn upsert(db: &Database, rows: Vec<Vec<Value>>) -> vectors::Result<usize> {
    db.insert_rows(
        "units",
        rows,
        InsertConflict::DoUpdate {
            target: "id".into(),
            update_columns: vec!["chat".into(), "slug".into(), "embedding".into()],
        },
    )
}

fn verify_search(db: &Database) {
    for scope in ["'a'", "'b'", "'a','b'", "'missing'"] {
        let sql = format!("SELECT id,cosine_distance(embedding,[1,0]) AS distance FROM units WHERE chat IN ({scope}) ORDER BY distance LIMIT 100");
        let reference = sql
            .replace("chat IN", "CAST(chat AS TEXT) IN")
            .replace(
                "cosine_distance(embedding,[1,0])",
                "cosine_distance(embedding,[1,0])+0.0",
            )
            .replace(" LIMIT 100", "");
        assert_eq!(query(db, &sql).rows, query(db, &reference).rows);
    }
}

#[test]
fn mixed_updates_append_swap_unique_keys_and_recover_through_wal_and_checkpoint() {
    let dir = Directory::new();
    let db = Database::open_persistent(&dir.0).unwrap();
    setup(&db);
    db.checkpoint().unwrap();
    let revision = db.revision().unwrap();
    let rows = vec![
        row(2, Some("a"), Some("one"), Some([1., 0.])),
        row(1, Some("b"), Some("two"), None),
        row(4, Some("a"), None, Some([1., 0.])),
    ];
    assert_eq!(upsert(&db, rows.clone()).unwrap(), 3);
    assert_eq!(
        query(&db, "SELECT id FROM units WHERE chat='a'").rows,
        vec![
            vec![Value::Integer(2)],
            vec![Value::Integer(3)],
            vec![Value::Integer(4)]
        ]
    );
    assert_eq!(
        query(&db, "SELECT id FROM units WHERE slug='one'").rows,
        vec![vec![Value::Integer(2)]]
    );
    assert_eq!(
        query(&db, "SELECT id FROM units WHERE slug='two'").rows,
        vec![vec![Value::Integer(1)]]
    );
    // A retry preserves identities/results but retains the established affected-row/revision contract.
    assert_eq!(upsert(&db, rows).unwrap(), 3);
    assert_eq!(db.revision().unwrap(), revision + 2);
    verify_search(&db);
    let expected = query(&db, "SELECT * FROM units ORDER BY id").rows;
    let revision = db.revision().unwrap();
    drop(db);
    for checkpoint in [true, false] {
        let db = Database::open_persistent(&dir.0).unwrap();
        assert_eq!(db.revision().unwrap(), revision);
        assert_eq!(query(&db, "SELECT * FROM units ORDER BY id").rows, expected);
        verify_search(&db);
        if checkpoint {
            db.checkpoint().unwrap();
        }
    }
}

#[test]
fn rejected_batches_preserve_rows_indexes_vectors_revision_and_wal() {
    let dir = Directory::new();
    let db = Database::open_persistent(&dir.0).unwrap();
    setup(&db);
    let original = query(&db, "SELECT * FROM units ORDER BY id").rows;
    let revision = db.revision().unwrap();
    let wal = fs::read(dir.0.join("vectors.wal")).unwrap();
    for bad_rows in [
        vec![
            row(1, Some("b"), Some("three"), Some([0., 1.])),
            row(4, Some("b"), Some("four"), None),
        ],
        vec![
            row(1, None, None, None),
            row(1, Some("b"), None, Some([0., 1.])),
        ],
        vec![
            row(4, None, None, None),
            row(4, Some("b"), None, Some([0., 1.])),
        ],
        vec![
            row(1, None, Some("same"), None),
            row(2, None, Some("same"), None),
        ],
        vec![row(1, None, None, None), vec![Value::Integer(7)]],
        vec![
            row(1, None, None, None),
            vec![
                Value::Integer(7),
                Value::Null,
                Value::Null,
                Value::Vector(Vector::new(vec![1., 2., 3.]).unwrap()),
            ],
        ],
    ] {
        assert!(upsert(&db, bad_rows).is_err());
        assert_eq!(query(&db, "SELECT * FROM units ORDER BY id").rows, original);
        assert_eq!(db.revision().unwrap(), revision);
        assert_eq!(fs::read(dir.0.join("vectors.wal")).unwrap(), wal);
        verify_search(&db);
    }
}

#[test]
fn typed_batches_match_sql_reference_across_retries_nulls_moves_and_failures() {
    let typed = Database::new();
    let reference = Database::new();
    setup(&typed);
    setup(&reference);
    // Deterministic generated batches exercise conflicts with both existing and
    // just-appended rows, secondary constraints, NULLs and multiple index buckets.
    let mut state = 7919_u64;
    for round in 0..120 {
        let mut rows = Vec::new();
        let mut sql_rows = Vec::new();
        for _ in 0..(round % 7 + 1) {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let id = (state % 30 + 1) as i64;
            let chat = match (state >> 8) % 3 {
                0 => None,
                1 => Some("a"),
                _ => Some("b"),
            };
            let slug = if state.is_multiple_of(5) {
                None
            } else {
                Some(format!("key-{}", (state >> 16) % 40))
            };
            let vector = if state.is_multiple_of(7) {
                None
            } else {
                Some([((state >> 24) % 5 + 1) as f32, 1.])
            };
            let values = row(id, chat, slug.as_deref(), vector);
            sql_rows.push(format!(
                "({id},{},{},{})",
                chat.map_or("NULL".into(), |x| format!("'{x}'")),
                slug.as_ref().map_or("NULL".into(), |x| format!("'{x}'")),
                vector.map_or("NULL".into(), |v| format!("[{},{}]", v[0], v[1]))
            ));
            rows.push(values);
        }
        let actual = upsert(&typed, rows);
        let expected=reference.execute(&format!("INSERT INTO units VALUES {} ON CONFLICT(id) DO UPDATE SET chat=excluded.chat,slug=excluded.slug,embedding=excluded.embedding",sql_rows.join(",")));
        match (actual, expected) {
            (Ok(count), Ok(results)) => assert!(
                matches!(results[0],ExecutionResult::Command {rows_affected,..} if rows_affected==count)
            ),
            (Err(actual), Err(expected)) => assert_eq!(actual, expected, "round {round}"),
            (actual, expected) => panic!("round {round}: {actual:?} != {expected:?}"),
        }
        assert_eq!(
            query(&typed, "SELECT * FROM units ORDER BY id").rows,
            query(&reference, "SELECT * FROM units ORDER BY id").rows
        );
        verify_search(&typed);
    }
}

#[test]
fn nullable_conflict_targets_do_not_merge_null_rows_and_key_updates_keep_fallback() {
    let db = Database::new();
    db.execute("CREATE TABLE nullable (id INTEGER, lookup DOUBLE UNIQUE, embedding VECTOR(2)); INSERT INTO nullable VALUES (1,0.0,[1,0])").unwrap();
    let make = |id, key| vec![Value::Integer(id), key, Value::Null];
    let conflict = InsertConflict::DoUpdate {
        target: "lookup".into(),
        update_columns: vec!["id".into(), "embedding".into()],
    };
    assert_eq!(
        db.insert_rows(
            "nullable",
            vec![
                make(2, Value::Float(-0.0)),
                make(3, Value::Null),
                make(4, Value::Null)
            ],
            conflict.clone()
        )
        .unwrap(),
        3
    );
    assert_eq!(
        query(&db, "SELECT id FROM nullable ORDER BY id").rows,
        vec![
            vec![Value::Integer(2)],
            vec![Value::Integer(3)],
            vec![Value::Integer(4)]
        ]
    );
    assert_eq!(
        db.insert_rows(
            "nullable",
            vec![make(5, Value::Float(0.0))],
            InsertConflict::DoUpdate {
                target: "lookup".into(),
                update_columns: vec!["id".into(), "lookup".into()]
            }
        )
        .unwrap(),
        1
    );
    assert_eq!(
        query(&db, "SELECT id FROM nullable WHERE lookup=0.0").rows,
        vec![vec![Value::Integer(5)]]
    );
    assert_eq!(db.insert_rows("nullable", vec![], conflict).unwrap(), 0);
}

#[test]
fn equal_values_preserve_incoming_float_bits_and_nan_conflicts_keep_existing_semantics() {
    let db = Database::new();
    db.execute("CREATE TABLE numbers (id INTEGER PRIMARY KEY, value DOUBLE, embedding VECTOR(2)); INSERT INTO numbers VALUES (1,0.0,[0.0,1.0])").unwrap();
    db.insert_rows(
        "numbers",
        vec![vec![
            Value::Integer(1),
            Value::Float(-0.0),
            Value::Vector(Vector::new(vec![-0.0, 1.0]).unwrap()),
        ]],
        InsertConflict::DoUpdate {
            target: "id".into(),
            update_columns: vec!["value".into(), "embedding".into()],
        },
    )
    .unwrap();
    let result = query(&db, "SELECT value,embedding FROM numbers");
    let [Value::Float(number), Value::Vector(vector)] = result.rows[0].as_slice() else {
        panic!("numeric row")
    };
    assert_eq!(number.to_bits(), (-0.0_f64).to_bits());
    assert_eq!(vector.as_slice()[0].to_bits(), (-0.0_f32).to_bits());
    db.execute("CREATE TABLE nans (lookup DOUBLE UNIQUE, value INTEGER)")
        .unwrap();
    let rows = vec![vec![Value::Float(f64::NAN), Value::Integer(1)]];
    db.insert_rows("nans", rows.clone(), InsertConflict::Fail)
        .unwrap();
    assert!(matches!(
        db.insert_rows(
            "nans",
            rows,
            InsertConflict::DoUpdate {
                target: "lookup".into(),
                update_columns: vec!["value".into()]
            }
        ),
        Err(vectors::Error::UniqueViolation(_))
    ));
}
