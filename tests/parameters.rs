use vectors::{bind_parameters, Database, ExecutionResult, Value, Vector};

fn rows(db: &Database, sql: &str, values: &[Value]) -> Vec<Vec<Value>> {
    match db.execute_with_parameters(sql, values).unwrap().remove(0) {
        ExecutionResult::Query(result) => result.rows,
        other => panic!("expected query: {other:?}"),
    }
}

#[test]
fn binds_values_without_interpreting_text_as_sql() {
    let db = Database::new();
    db.execute("CREATE TABLE docs (id INTEGER PRIMARY KEY, text TEXT, embedding VECTOR(2))")
        .unwrap();
    let attack = "Żółć '); DROP TABLE docs; -- $1 \\ \n";
    db.execute_with_parameters(
        "INSERT INTO docs VALUES ($1, $2, $3)",
        &[
            Value::Integer(1),
            Value::Text(attack.into()),
            Value::Vector(Vector::new(vec![1.0, 0.0]).unwrap()),
        ],
    )
    .unwrap();
    assert_eq!(
        rows(
            &db,
            "SELECT text FROM docs WHERE id = $1",
            &[Value::Integer(1)]
        ),
        vec![vec![Value::Text(attack.into())]]
    );
    assert_eq!(
        rows(
            &db,
            "SELECT id FROM docs ORDER BY embedding <=> $1 LIMIT $2",
            &[
                Value::Vector(Vector::new(vec![1.0, 0.0]).unwrap()),
                Value::Integer(1),
            ]
        ),
        vec![vec![Value::Integer(1)]]
    );
}

#[test]
fn preserves_quotes_comments_unicode_locations_and_negative_values() {
    let sql = "SELECT '$1''雪', $2, \"$3\", $$ $4 $$,\r\n\t$1-- $7\n/* $9 */,$2";
    assert_eq!(
        bind_parameters(sql, &[Value::Integer(-2), Value::Text("a'b".into())]).unwrap(),
        "SELECT '$1''雪', ('a''b'), \"$3\", $$ $4 $$,\r\n\t(-2)-- $7\n/* $9 */,('a''b')"
    );
    assert_eq!(
        rows(
            &Database::new(),
            "SELECT 1-$1, $2, $3, $4",
            &[
                Value::Integer(-2),
                Value::Float(1.0),
                Value::Boolean(true),
                Value::Null,
            ]
        ),
        vec![vec![
            Value::Integer(3),
            Value::Float(1.0),
            Value::Boolean(true),
            Value::Null
        ]]
    );
}

#[test]
fn invalid_bindings_and_failed_batches_never_commit_partial_writes() {
    let db = Database::new();
    db.execute("CREATE TABLE docs (id INTEGER PRIMARY KEY)")
        .unwrap();
    for sql in [
        "INSERT INTO docs VALUES (1); SELECT $0",
        "SELECT $2",
        "SELECT ?",
        "SELECT $name",
        "SELECT 1",
        "SELECT $999999999999999999999999999",
    ] {
        assert!(
            db.execute_with_parameters(sql, &[Value::Integer(1)])
                .is_err(),
            "{sql}"
        );
    }
    assert!(db
        .execute_with_parameters(
            "INSERT INTO docs VALUES ($1); INSERT INTO docs VALUES ($1)",
            &[Value::Integer(1)]
        )
        .is_err());
    assert!(rows(&db, "SELECT * FROM docs", &[]).is_empty());
    assert!(bind_parameters("SELECT $1", &[Value::Float(f64::NAN)]).is_err());
    assert!(bind_parameters("SELECT $1", &[]).is_err());
}

#[test]
fn parameters_remain_values_and_expansion_is_bounded() {
    let db = Database::new();
    let extremes = Value::Vector(Vector::new(vec![f32::MAX, f32::MIN, f32::MIN_POSITIVE]).unwrap());
    assert_eq!(
        rows(&db, "SELECT $1", std::slice::from_ref(&extremes)),
        vec![vec![extremes]]
    );
    assert_eq!(
        rows(
            &db,
            "SELECT $1, $2",
            &[Value::Integer(i64::MIN), Value::Integer(i64::MAX)]
        ),
        vec![vec![Value::Integer(i64::MIN), Value::Integer(i64::MAX)]]
    );
    assert!(db
        .execute_with_parameters(
            "CREATE TABLE $1 (id INTEGER)",
            &[Value::Text("docs".into())]
        )
        .is_err());
    let text = Value::Text("x".repeat(1024 * 1024));
    let sql = std::iter::repeat_n("$1", 33).collect::<Vec<_>>().join(",");
    assert!(bind_parameters(&format!("SELECT {sql}"), &[text]).is_err());
}

#[test]
fn parameterized_writes_recover_from_wal_and_checkpoint() {
    let directory = std::env::temp_dir().join(format!(
        "vectors-parameters-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let text = Value::Text("雪'); DROP TABLE docs; --".into());
    {
        let db = Database::open_persistent(&directory).unwrap();
        db.execute_with_parameters("CREATE TABLE docs (id INTEGER PRIMARY KEY, text TEXT); INSERT INTO docs VALUES ($1, $2)",
            &[Value::Integer(1), text.clone()]).unwrap();
    }
    {
        let db = Database::open_persistent(&directory).unwrap();
        assert_eq!(
            rows(
                &db,
                "SELECT text FROM docs WHERE id = $1",
                &[Value::Integer(1)]
            ),
            vec![vec![text.clone()]]
        );
        db.execute_with_parameters(
            "INSERT INTO docs VALUES ($1, $2)",
            &[Value::Integer(2), text.clone()],
        )
        .unwrap();
        db.checkpoint().unwrap();
    }
    {
        let db = Database::open_persistent(&directory).unwrap();
        assert_eq!(
            rows(&db, "SELECT text FROM docs ORDER BY id", &[]),
            vec![vec![text.clone()], vec![text]]
        );
    }
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn cosine_operator_matches_function_in_fast_general_and_aggregate_queries() {
    let db = Database::new();
    db.execute("CREATE TABLE docs (id INTEGER, embedding VECTOR(2)); INSERT INTO docs VALUES (1, ARRAY[1,0]), (2, ARRAY[0,1]), (3, NULL)").unwrap();
    for suffix in [
        "ORDER BY score LIMIT 2",
        "ORDER BY id",
        "WHERE (embedding <=> ARRAY[1,0]) < 0.5",
    ] {
        let operator = rows(
            &db,
            &format!("SELECT id, embedding <=> ARRAY[1,0] AS score FROM docs {suffix}"),
            &[],
        );
        let function = rows(
            &db,
            &format!(
                "SELECT id, cosine_distance(embedding, ARRAY[1,0]) AS score FROM docs {suffix}"
            ),
            &[],
        );
        assert_eq!(operator, function);
    }
    assert_eq!(
        rows(
            &db,
            "SELECT COUNT(*) FROM docs GROUP BY embedding HAVING embedding <=> embedding = 0",
            &[]
        ),
        vec![vec![Value::Integer(1)], vec![Value::Integer(1)]]
    );
    assert!(db.execute("SELECT 1 <=> 2").is_err());
}
