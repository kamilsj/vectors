use vectors::{Database, ExecutionResult, QueryResult, Value, Vector};

fn query(db: &Database, sql: &str, parameters: &[Value]) -> QueryResult {
    let ExecutionResult::Query(result) = db
        .execute_with_parameters(sql, parameters)
        .unwrap()
        .remove(0)
    else {
        panic!("expected query");
    };
    result
}

fn fixture() -> Database {
    let db = Database::new();
    db.execute("CREATE TABLE units (id INTEGER PRIMARY KEY, chat_id TEXT, profile TEXT, turn_id TEXT, embedding VECTOR(3));
        CREATE INDEX chats ON units(chat_id);
        INSERT INTO units VALUES
        (1, 'private', 'active', 'private', ARRAY[1,0,0]),
        (2, 'team-a', 'old', 'old', ARRAY[1,0,0]),
        (3, 'team-a', 'active', 'self', ARRAY[1,0,0]),
        (4, 'team-a', 'active', 'answer-a', ARRAY[0.8,0.2,0]),
        (5, 'team-b', 'active', 'answer-b', ARRAY[0.8,0.2,0]),
        (6, NULL, 'active', 'unscoped', ARRAY[1,0,0]),
        (7, 'team-b', 'active', 'null-vector', NULL)").unwrap();
    db
}

#[test]
fn saywit_parameterized_scope_prunes_before_top_k_and_preserves_exact_scores() {
    let db = fixture();
    let sql = "SELECT id,turn_id,cosine_distance(embedding,$1) AS distance FROM units
        WHERE profile=$2 AND chat_id IN ($3,$4,$5,$6) AND turn_id<>$7 ORDER BY distance LIMIT 2";
    let parameters = [
        Value::Vector(Vector::new(vec![1.0, 0.0, 0.0]).unwrap()),
        Value::Text("active".into()),
        Value::Text("team-b".into()),
        Value::Text("team-a".into()),
        Value::Text("team-a".into()),
        Value::Null,
        Value::Text("self".into()),
    ];
    let actual = query(&db, sql, &parameters);
    let reference = sql
        .replace("chat_id IN", "CAST(chat_id AS TEXT) IN")
        .replace(
            "cosine_distance(embedding,$1)",
            "cosine_distance(embedding,$1) + 0.0",
        )
        .replace("ORDER BY distance", "ORDER BY distance,id");
    let expected = query(&db, &reference, &parameters);
    assert_eq!(actual.rows, expected.rows);
    assert_eq!(
        actual.rows.iter().map(|r| r[0].clone()).collect::<Vec<_>>(),
        vec![Value::Integer(4), Value::Integer(5)]
    );
    assert_eq!(actual.rows_examined, 5);
    assert_eq!(expected.rows_examined, 7);
}

#[test]
fn membership_duplicates_nulls_boolean_composition_and_negation_match_scan() {
    let db = fixture();
    for (predicate, examined) in [
        ("chat_id IN ('team-b','team-a','team-a',NULL)", 5),
        ("chat_id IN ('missing',NULL)", 0),
        ("chat_id IN (NULL)", 0),
        ("(chat_id) IN ('team-a')", 3),
        (
            "units.chat_id IN ('team-b') OR chat_id IN ('team-a','team-b')",
            5,
        ),
        (
            "chat_id IN ('team-a','team-b') AND chat_id IN ('team-b')",
            2,
        ),
        ("chat_id IN ('team-a') AND id>3", 3),
        ("chat_id IN ('team-a') OR profile='active'", 7),
        ("(chat_id IN ('team-a') AND profile='active') OR chat_id IN ('team-b')", 5),
        ("(chat_id IN ('team-a') AND profile='old') OR (chat_id IN ('team-b') AND turn_id='answer-b')", 5),
        ("profile='active' AND (chat_id IN ('team-a','team-b') AND turn_id<>'self')", 5),
        ("(chat_id IN ('team-a','team-b') AND profile='active') AND (chat_id IN ('team-b') AND turn_id<>'null-vector')", 2),
        ("(chat_id IN ('team-a') AND NULL) OR chat_id IN ('team-b')", 5),
        ("(chat_id IN ('team-a') AND NULL) AND chat_id IN ('team-b')", 0),
        ("chat_id NOT IN ('team-a',NULL)", 7),
        ("chat_id NOT IN ('team-a')", 7),
        ("chat_id IN (profile,'team-a')", 7),
    ] {
        let sql = format!("SELECT id FROM units WHERE {predicate} ORDER BY id");
        let actual = query(&db, &sql, &[]);
        let reference = sql
            .replace("units.chat_id", "chat_id")
            .replace("chat_id", "CAST(chat_id AS TEXT)");
        assert_eq!(actual.rows, query(&db, &reference, &[]).rows, "{predicate}");
        assert_eq!(actual.rows_examined, examined, "{predicate}");
    }
    let plan = query(
        &db,
        "EXPLAIN SELECT id FROM units WHERE chat_id IN ('team-a','team-b')",
        &[],
    );
    assert!(plan
        .rows
        .iter()
        .any(|r| r[0].to_string().contains("covered by scalar hash index")));
}

#[test]
fn scalar_types_and_unsafe_constant_lists_preserve_sql_comparison_rules() {
    let db = Database::new();
    for table in ["indexed", "scan"] {
        db.execute(&format!("CREATE TABLE {table} (id INTEGER, number INTEGER, floating DOUBLE, flag BOOLEAN);
            INSERT INTO {table} VALUES (1,1,0.0,TRUE),(2,2,-0.0,FALSE),(3,9007199254740993,2.0,NULL),(4,NULL,NULL,NULL)")).unwrap();
    }
    db.execute("CREATE INDEX numbers ON indexed(number); CREATE INDEX floats ON indexed(floating); CREATE INDEX flags ON indexed(flag)").unwrap();
    for predicate in [
        "number IN (1,1,NULL,2)",
        "number IN (9007199254740992.0)",
        "floating IN (0,-0.0,2)",
        "flag IN (FALSE,TRUE,NULL)",
        "number IN (1,number+1)",
        "number IN (1,'invalid')",
        "number IN (1,1/0)",
    ] {
        let sql = format!("SELECT id FROM indexed WHERE {predicate} ORDER BY id");
        let expected = db.execute(&sql.replace("indexed", "scan"));
        let actual = db.execute(&sql);
        match (actual, expected) {
            (Ok(mut actual), Ok(mut expected)) => {
                let (ExecutionResult::Query(actual), ExecutionResult::Query(expected)) =
                    (actual.remove(0), expected.remove(0))
                else {
                    panic!("query results")
                };
                assert_eq!(actual.rows, expected.rows, "{predicate}");
            }
            (Err(actual), Err(expected)) => assert_eq!(actual, expected, "{predicate}"),
            (actual, expected) => panic!("{predicate}: {actual:?} != {expected:?}"),
        }
    }
}

#[test]
fn membership_tracks_updates_deletes_and_snapshot_reopen() {
    let db = fixture();
    db.execute("UPDATE units SET chat_id='team-b' WHERE id=1; DELETE FROM units WHERE id=4; INSERT INTO units VALUES (8,'team-b','active','new',ARRAY[1,0,0])").unwrap();
    let sql = "SELECT id FROM units WHERE chat_id IN ('team-b','team-b',NULL) ORDER BY id";
    let actual = query(&db, sql, &[]);
    assert_eq!(actual.rows_examined, 4);
    assert_eq!(
        actual.rows,
        vec![
            vec![Value::Integer(1)],
            vec![Value::Integer(5)],
            vec![Value::Integer(7)],
            vec![Value::Integer(8)]
        ]
    );
    let path = std::env::temp_dir().join(format!("vectors-membership-{}.json", std::process::id()));
    db.save(&path).unwrap();
    let restored = Database::open(&path).unwrap();
    std::fs::remove_file(path).unwrap();
    assert_eq!(query(&restored, sql, &[]), actual);
}

#[test]
fn hundreds_of_bound_chat_ids_keep_duplicate_and_injection_values_literal() {
    let db = fixture();
    let mut values = (0..500)
        .map(|n| Value::Text(format!("missing-{n}")))
        .collect::<Vec<_>>();
    values[10] = Value::Text("team-a".into());
    values[400] = Value::Text("team-b".into());
    values[499] = Value::Text("team-a' OR 1=1 --".into());
    let placeholders = (1..=500)
        .map(|n| format!("${n}"))
        .collect::<Vec<_>>()
        .join(",");
    let actual = query(
        &db,
        &format!("SELECT id FROM units WHERE chat_id IN ({placeholders}) ORDER BY id"),
        &values,
    );
    assert_eq!(actual.rows_examined, 5);
    assert_eq!(
        actual.rows,
        query(
            &db,
            "SELECT id FROM units WHERE CAST(chat_id AS TEXT) IN ('team-a','team-b') ORDER BY id",
            &[]
        )
        .rows
    );
}
