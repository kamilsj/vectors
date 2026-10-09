use vectors::{Database, ExecutionResult, InsertConflict, QueryResult, Value, Vector};

fn query(db: &Database, sql: &str) -> QueryResult {
    let ExecutionResult::Query(result) = db.execute(sql).unwrap().remove(0) else {
        panic!("query expected")
    };
    result
}

fn fixture() -> Database {
    let db = Database::new();
    db.execute("CREATE TABLE indexed(id INTEGER PRIMARY KEY,code TEXT UNIQUE,chat INTEGER,profile TEXT,embedding VECTOR(3)); CREATE TABLE reference(id INTEGER,code TEXT,chat INTEGER,profile TEXT,embedding VECTOR(3)); CREATE INDEX chats ON indexed(chat); CREATE INDEX profiles ON indexed(profile)").unwrap();
    // Reverse insertion order makes source-order tie breaking observable.
    let rows = (0..128)
        .rev()
        .map(|id| {
            vec![
                Value::Integer(id),
                if id % 17 == 0 {
                    Value::Null
                } else {
                    Value::Text(format!("code-{id}"))
                },
                if id % 11 == 0 {
                    Value::Null
                } else {
                    Value::Integer(id % 8)
                },
                Value::Text(if id % 9 == 0 { "old" } else { "active" }.into()),
                Value::Vector(Vector::new(vec![1., (id % 7) as f32, 0.]).unwrap()),
            ]
        })
        .collect::<Vec<_>>();
    db.insert_rows("indexed", rows.clone(), InsertConflict::Fail)
        .unwrap();
    db.insert_rows("reference", rows, InsertConflict::Fail)
        .unwrap();
    db
}

#[test]
fn primary_and_unique_lookups_prune_without_explicit_secondary_indexes() {
    let db = fixture();
    for (predicate, count) in [
        ("id=7", 1),
        ("7=(id)", 1),
        ("id IN (7,1,7,NULL,999)", 2),
        ("code IN ('code-7','code-34',NULL,'missing')", 1),
        ("id=999", 0),
        ("code=NULL", 0),
    ] {
        let actual = query(&db, &format!("SELECT id FROM indexed WHERE {predicate}"));
        let expected = query(&db, &format!("SELECT id FROM reference WHERE {predicate}"));
        assert_eq!(actual.rows, expected.rows, "{predicate}");
        assert_eq!(actual.rows_examined, count, "{predicate}");
    }
}

#[test]
fn reordered_boolean_plans_and_residuals_match_full_scan_and_exact_ranking() {
    let db = fixture();
    let terms = [
        "id IN (1,7,18,33,100)",
        "chat IN (0,1,2,3,NULL,3)",
        "profile='active'",
        "code IN ('code-7','code-18')",
        "id>12",
        "chat NOT IN (4,NULL)",
    ];
    for a in terms {
        for b in terms {
            for c in [
                "profile='old'",
                "id IN (7,8,100)",
                "chat=1",
                "id<70",
                "NULL",
            ] {
                for predicate in [
                    format!("({a} AND {b}) AND {c}"),
                    format!("{c} AND ({b} AND {a})"),
                    format!("({a} OR {b}) AND {c}"),
                    format!("({a} AND {c}) OR ({b} AND {c})"),
                ] {
                    let sql=format!("SELECT id,cosine_distance(embedding,[1,0,0]) AS distance FROM indexed WHERE {predicate} ORDER BY distance LIMIT 8");
                    let reference = sql
                        .replace("indexed", "reference")
                        .replace(
                            "cosine_distance(embedding,[1,0,0])",
                            "cosine_distance(embedding,[1,0,0])+0.0",
                        )
                        .replace(" LIMIT 8", "");
                    let mut expected = query(&db, &reference).rows;
                    expected.truncate(8);
                    assert_eq!(query(&db, &sql).rows, expected, "{predicate}");
                }
            }
        }
    }
}

#[test]
fn unique_lookup_keeps_numeric_coercion_null_and_expression_errors() {
    let db = Database::new();
    db.execute("CREATE TABLE indexed(i INTEGER UNIQUE,f DOUBLE UNIQUE,b BOOLEAN UNIQUE); CREATE TABLE reference(i INTEGER,f DOUBLE,b BOOLEAN); INSERT INTO indexed VALUES(9007199254740993,2,TRUE),(2,-0.0,FALSE),(NULL,NULL,NULL); INSERT INTO reference VALUES(9007199254740993,2,TRUE),(2,-0.0,FALSE),(NULL,NULL,NULL)").unwrap();
    for predicate in [
        "i=9007199254740992.0",
        "i IN (9007199254740992.0,2)",
        "f IN (0,-0.0,2)",
        "b IN (TRUE,FALSE,NULL)",
        "i IN (2,'bad')",
        "i IN (2,1/0)",
        "i IN (i,2)",
        "i NOT IN (2,NULL)",
        "f=NULL",
        "i IS NULL",
    ] {
        let actual = db.execute(&format!("SELECT * FROM indexed WHERE {predicate}"));
        let expected = db.execute(&format!("SELECT * FROM reference WHERE {predicate}"));
        match (actual, expected) {
            (Ok(mut a), Ok(mut b)) => {
                let (ExecutionResult::Query(a), ExecutionResult::Query(b)) =
                    (a.remove(0), b.remove(0))
                else {
                    panic!("query expected")
                };
                assert_eq!(a.rows, b.rows, "{predicate}");
            }
            (Err(a), Err(b)) => assert_eq!(a, b, "{predicate}"),
            (a, b) => panic!("{predicate}: {a:?} != {b:?}"),
        }
    }
}

#[test]
fn unique_candidates_follow_upserts_deletes_and_durable_recovery() {
    let path = std::env::temp_dir().join(format!(
        "vectors-adaptive-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let db = Database::open_persistent(&path).unwrap();
    db.execute("CREATE TABLE units(id INTEGER PRIMARY KEY,code TEXT UNIQUE,chat TEXT); INSERT INTO units VALUES(1,'one','a'),(2,'two','b'),(3,NULL,'a'); CREATE INDEX chats ON units(chat)").unwrap();
    db.checkpoint().unwrap();
    db.execute("INSERT INTO units VALUES(2,'updated','a') ON CONFLICT(id) DO UPDATE SET code=excluded.code,chat=excluded.chat; DELETE FROM units WHERE id=1").unwrap();
    let sql = "SELECT id FROM units WHERE code IN ('updated','one','two') AND chat='a'";
    let expected = query(&db, sql);
    assert_eq!(expected.rows, vec![vec![Value::Integer(2)]]);
    assert_eq!(expected.rows_examined, 1);
    let revision = db.revision().unwrap();
    assert!(db
        .execute("DELETE FROM units WHERE id/0>1 AND id=999")
        .is_err());
    assert_eq!(db.revision().unwrap(), revision);
    drop(db);
    let db = Database::open_persistent(&path).unwrap();
    assert_eq!(query(&db, sql), expected);
    db.execute("INSERT INTO units VALUES(1,'one','a')").unwrap();
    assert_eq!(
        query(&db, "SELECT id FROM units WHERE code='one'").rows,
        vec![vec![Value::Integer(1)]]
    );
    drop(db);
    std::fs::remove_dir_all(path).unwrap();
}
