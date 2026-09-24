use vectors::{Database, Error, ExecutionResult, InsertConflict, QueryResult, Value, Vector};

fn query(db: &Database, sql: &str) -> QueryResult {
    match db.execute(sql).unwrap().pop().unwrap() {
        ExecutionResult::Query(result) => result,
        _ => panic!("expected query"),
    }
}
fn fixture(indexed: bool) -> Database {
    let db = Database::new();
    db.execute("CREATE TABLE lhs (id INTEGER PRIMARY KEY, link INTEGER, title TEXT, embedding VECTOR(2)); CREATE TABLE rhs (id INTEGER PRIMARY KEY, link INTEGER, title TEXT, enabled BOOLEAN);
        INSERT INTO lhs VALUES (1,7,'alpha',ARRAY[1,0]),(2,7,'beta',ARRAY[0.8,0.2]),(3,8,'gamma',ARRAY[-1,0]),(4,NULL,'null key',ARRAY[0,1]),(5,9,'unmatched',ARRAY[0.6,0.4]);
        INSERT INTO rhs VALUES (10,7,'first',true),(11,7,'second',false),(12,8,'third',NULL),(13,NULL,'null must not match',true),(14,8,'last',true);").unwrap();
    if indexed {
        db.execute("CREATE INDEX rhs_link ON rhs USING HASH (link)")
            .unwrap();
    }
    db
}

#[test]
fn indexed_and_temporary_hash_joins_match_nested_reference_with_duplicates_and_nulls() {
    for indexed in [false, true] {
        let db = fixture(indexed);
        let left = query(&db, "SELECT id,link FROM lhs ORDER BY id").rows;
        let right = query(&db, "SELECT id,link FROM rhs ORDER BY id").rows;
        let expected = left
            .iter()
            .flat_map(|left| {
                right
                    .iter()
                    .filter(move |right| left[1] != Value::Null && left[1] == right[1])
                    .map(move |right| vec![left[0].clone(), right[0].clone()])
            })
            .collect::<Vec<_>>();
        let result=query(&db,"SELECT a.id AS left_id,b.id AS right_id FROM lhs a INNER JOIN rhs b ON a.link=b.link ORDER BY a.id,b.id");
        assert_eq!(result.rows, expected);
        assert_eq!(result.rows_examined, 6);
        assert_eq!(
            query(
                &db,
                "SELECT a.id,b.id FROM lhs a JOIN rhs b ON b.link=a.link ORDER BY a.id,b.id"
            )
            .rows,
            result.rows
        );
        assert_eq!(
            query(
                &db,
                "SELECT DISTINCT a.link FROM lhs a JOIN rhs b ON a.link=b.link ORDER BY a.link"
            )
            .rows,
            vec![vec![Value::Integer(7)], vec![Value::Integer(8)]]
        );
    }
}

#[test]
fn left_join_residual_on_and_where_have_distinct_sql_null_semantics() {
    let db = fixture(true);
    let result=query(&db,"SELECT a.id,b.id FROM lhs a LEFT JOIN rhs b ON a.link=b.link AND b.enabled=true ORDER BY a.id,b.id");
    assert_eq!(
        result.rows,
        vec![
            vec![Value::Integer(1), Value::Integer(10)],
            vec![Value::Integer(2), Value::Integer(10)],
            vec![Value::Integer(3), Value::Integer(14)],
            vec![Value::Integer(4), Value::Null],
            vec![Value::Integer(5), Value::Null]
        ]
    );
    let result=query(&db,"SELECT a.id,b.id FROM lhs a LEFT JOIN rhs b ON a.link=b.link WHERE b.enabled=false ORDER BY a.id");
    assert_eq!(
        result.rows,
        vec![
            vec![Value::Integer(1), Value::Integer(11)],
            vec![Value::Integer(2), Value::Integer(11)]
        ]
    );
    let result=query(&db,"SELECT a.id FROM lhs a LEFT JOIN rhs b ON a.link=b.link AND b.id<0 WHERE b.id IS NULL ORDER BY a.id");
    assert_eq!(
        result.rows,
        (1..=5)
            .map(|id| vec![Value::Integer(id)])
            .collect::<Vec<_>>()
    );
    assert_eq!(query(&db,"SELECT a.id FROM lhs a LEFT JOIN rhs b ON a.link=b.link WHERE b.id IS NULL ORDER BY a.id").rows,vec![vec![Value::Integer(4)],vec![Value::Integer(5)]]);
}

#[test]
fn qualified_wildcards_aliases_self_joins_and_unqualified_ambiguity_are_correct() {
    let db = fixture(true);
    let left = query(
        &db,
        "SELECT a.* FROM lhs a JOIN rhs b ON a.link=b.link ORDER BY a.id,b.id LIMIT 1",
    );
    assert_eq!(left.columns, vec!["id", "link", "title", "embedding"]);
    assert_eq!(
        left.rows[0],
        query(&db, "SELECT * FROM lhs WHERE id=1").rows[0]
    );
    let both = query(
        &db,
        "SELECT * FROM lhs a JOIN rhs b ON a.link=b.link ORDER BY a.id,b.id LIMIT 1",
    );
    assert_eq!(
        both.columns,
        vec![
            "id",
            "link",
            "title",
            "embedding",
            "id",
            "link",
            "title",
            "enabled"
        ]
    );
    assert_eq!(
        query(
            &db,
            "SELECT embedding,b.enabled FROM lhs a JOIN rhs b ON a.link=b.link LIMIT 1"
        )
        .rows[0]
            .len(),
        2
    );
    for sql in [
        "SELECT id FROM lhs a JOIN rhs b ON a.link=b.link",
        "SELECT a.id FROM lhs a JOIN rhs b ON link=b.link",
        "SELECT a.id FROM lhs a JOIN rhs b ON a.link=b.link WHERE title='x'",
        "SELECT a.id,b.id FROM lhs a JOIN rhs b ON a.link=b.link ORDER BY id",
        "SELECT bad.title FROM lhs a JOIN rhs b ON a.link=b.link",
        "SELECT bad.* FROM lhs a JOIN rhs b ON a.link=b.link",
        "SELECT lhs.id FROM lhs a JOIN rhs b ON a.link=b.link",
    ] {
        assert!(db.execute(sql).is_err(), "{sql}");
    }
    db.execute("CREATE TABLE employee (id INTEGER PRIMARY KEY,parent INTEGER); INSERT INTO employee VALUES (1,NULL),(2,1),(3,2)").unwrap();
    assert_eq!(query(&db,"SELECT c.id,p.id AS parent FROM employee c LEFT JOIN employee p ON c.parent=p.id ORDER BY c.id").rows,vec![vec![Value::Integer(1),Value::Null],vec![Value::Integer(2),Value::Integer(1)],vec![Value::Integer(3),Value::Integer(2)]]);
    assert!(db
        .execute("SELECT * FROM employee JOIN employee ON employee.parent=employee.id")
        .is_err());
}

#[test]
fn vector_ranking_reuses_exact_scores_top_k_aliases_and_offsets() {
    let db = fixture(true);
    let base="SELECT a.id AS left_id,b.id AS right_id,cosine_distance(a.embedding,ARRAY[1,0]) AS distance FROM lhs a JOIN rhs b ON a.link=b.link WHERE a.title NOT LIKE 'skip%'";
    let mut expected = Vec::new();
    for (id, vector, right_ids) in [
        (1, vec![1.0, 0.0], vec![10, 11]),
        (2, vec![0.8, 0.2], vec![10, 11]),
        (3, vec![-1.0, 0.0], vec![12, 14]),
    ] {
        let distance = f64::from(
            Vector::new(vector)
                .unwrap()
                .cosine_distance(&Vector::new(vec![1.0, 0.0]).unwrap())
                .unwrap(),
        );
        for right in right_ids {
            expected.push(vec![
                Value::Integer(id),
                Value::Integer(right),
                Value::Float(distance),
            ]);
        }
    }
    assert_eq!(
        query(
            &db,
            &format!("{base} ORDER BY distance,a.id,b.id LIMIT 3 OFFSET 1")
        )
        .rows,
        expected[1..4]
    );
    assert_eq!(query(&db,&format!("{base} ORDER BY cosine_distance(a.embedding,ARRAY[1,0]),a.id,b.id LIMIT 3 OFFSET 1")).rows,expected[1..4]);
    let nulls=query(&db,"SELECT b.id,cosine_distance(a.embedding,ARRAY[1,0]) AS distance FROM rhs b LEFT JOIN lhs a ON b.link=a.link ORDER BY distance DESC NULLS FIRST,b.id LIMIT 1");
    assert_eq!(nulls.rows, vec![vec![Value::Integer(13), Value::Null]]);
    assert!(query(&db, &format!("{base} ORDER BY distance LIMIT 0"))
        .rows
        .is_empty());
    assert_eq!(query(&db,"SELECT b.id AS __join_0,a.id AS real_id FROM lhs a JOIN rhs b ON a.link=b.link ORDER BY a.id DESC,b.id LIMIT 1").rows,vec![vec![Value::Integer(12),Value::Integer(3)]]);
    let quoted = base.replace("AS distance", "AS \"Score\"");
    for alias in ["\"Score\"", "score", "\"SCORE\""] {
        let result = query(
            &db,
            &format!("{quoted} ORDER BY {alias},a.id,b.id LIMIT 3 OFFSET 1"),
        );
        assert_eq!(result.rows, expected[1..4]);
        assert_eq!(result.columns[2], "Score");
    }
}

#[test]
fn integer_double_equality_uses_existing_sql_numeric_comparison() {
    let db = Database::new();
    db.execute("CREATE TABLE integers (k INTEGER); CREATE TABLE floats (k DOUBLE); INSERT INTO integers VALUES (0),(1),(9007199254740993),(NULL); INSERT INTO floats VALUES (-0.0),(1.0),(9007199254740992.0),(NULL); CREATE INDEX floats_key ON floats USING HASH (k)").unwrap();
    let result = query(
        &db,
        "SELECT a.k,b.k FROM integers a JOIN floats b ON a.k=b.k ORDER BY a.k",
    );
    assert_eq!(result.rows.len(), 3);
    assert_eq!(result.rows[0], vec![Value::Integer(0), Value::Float(-0.0)]);
    assert_eq!(
        result.rows[2],
        vec![
            Value::Integer(9007199254740993),
            Value::Float(9007199254740992.0)
        ]
    );
    assert_eq!(
        query(
            &db,
            "SELECT b.k,a.k FROM floats a JOIN integers b ON a.k=b.k ORDER BY b.k"
        )
        .rows,
        result.rows
    );
}

#[test]
fn unique_join_lookups_preserve_nulls_mutations_and_numeric_collisions() {
    let db = Database::new();
    db.execute("CREATE TABLE probes (id INTEGER); CREATE TABLE targets (id INTEGER UNIQUE,label TEXT); INSERT INTO probes VALUES (0),(1),(2),(NULL); INSERT INTO targets VALUES (0,'zero'),(1,'one'),(NULL,'first null'),(NULL,'second null')").unwrap();
    let sql = "SELECT p.id,t.label FROM probes p LEFT JOIN targets t ON p.id=t.id ORDER BY p.id NULLS FIRST";
    assert_eq!(
        query(&db, sql).rows,
        vec![
            vec![Value::Null, Value::Null],
            vec![Value::Integer(0), Value::Text("zero".into())],
            vec![Value::Integer(1), Value::Text("one".into())],
            vec![Value::Integer(2), Value::Null],
        ]
    );
    db.execute("UPDATE targets SET id=2 WHERE id=1; DELETE FROM targets WHERE id=0; INSERT INTO targets VALUES (1,'replacement')").unwrap();
    assert_eq!(
        query(&db, sql).rows,
        vec![
            vec![Value::Null, Value::Null],
            vec![Value::Integer(0), Value::Null],
            vec![Value::Integer(1), Value::Text("replacement".into())],
            vec![Value::Integer(2), Value::Text("one".into())],
        ]
    );
    // Distinct, unique i64 keys can compare equal after the evaluator's f64
    // coercion. A unique lookup cannot discard either matching target row.
    db.execute("CREATE TABLE float_probes (k DOUBLE); CREATE TABLE integer_targets (k INTEGER PRIMARY KEY); INSERT INTO float_probes VALUES (9007199254740992.0); INSERT INTO integer_targets VALUES (9007199254740992),(9007199254740993)").unwrap();
    assert_eq!(
        query(
            &db,
            "SELECT t.k FROM float_probes p JOIN integer_targets t ON p.k=t.k ORDER BY t.k"
        )
        .rows,
        vec![
            vec![Value::Integer(9007199254740992)],
            vec![Value::Integer(9007199254740993)]
        ]
    );
}

#[test]
fn maintained_indexes_follow_mutations_and_unordered_limit_stops_early() {
    let db = fixture(true);
    assert_eq!(
        query(
            &db,
            "SELECT a.id,b.id FROM lhs a JOIN rhs b ON a.link=b.link LIMIT 1"
        )
        .rows_examined,
        1
    );
    db.execute("UPDATE rhs SET link=9 WHERE id=10; DELETE FROM rhs WHERE id=11; INSERT INTO rhs VALUES (15,7,'new',true)").unwrap();
    assert_eq!(
        query(
            &db,
            "SELECT a.id,b.id FROM lhs a JOIN rhs b ON a.link=b.link ORDER BY a.id,b.id"
        )
        .rows,
        vec![
            vec![Value::Integer(1), Value::Integer(15)],
            vec![Value::Integer(2), Value::Integer(15)],
            vec![Value::Integer(3), Value::Integer(12)],
            vec![Value::Integer(3), Value::Integer(14)],
            vec![Value::Integer(5), Value::Integer(10)]
        ]
    );
    db.execute("DROP INDEX rhs_link").unwrap();
    assert_eq!(
        query(
            &db,
            "SELECT a.id,b.id FROM lhs a JOIN rhs b ON a.link=b.link LIMIT 1"
        )
        .rows_examined,
        1
    );
}

#[test]
fn api_row_limits_and_failed_join_batches_remain_atomic() {
    let db = fixture(true);
    let sql = "SELECT a.id,b.id FROM lhs a JOIN rhs b ON a.link=b.link ORDER BY a.id,b.id";
    assert!(matches!(
        db.execute_with_row_limit(sql, 2),
        Err(Error::ResultLimitExceeded {
            max: 2,
            found_at_least: 3
        })
    ));
    assert!(db
        .execute_with_row_limit(&format!("{sql} LIMIT 2"), 2)
        .is_ok());
    let revision = db.revision().unwrap();
    assert!(matches!(
        db.execute_with_row_limit(&format!("UPDATE lhs SET title='rollback'; {sql}"), 2),
        Err(Error::ResultLimitExceeded { .. })
    ));
    assert_eq!(db.revision().unwrap(), revision);
    assert_eq!(
        query(&db, "SELECT title FROM lhs WHERE id=1").rows,
        vec![vec![Value::Text("alpha".into())]]
    );
    assert!(db.execute("UPDATE lhs SET title='rollback'; SELECT missing FROM lhs a JOIN rhs b ON a.link=b.link").is_err());
    assert_eq!(db.revision().unwrap(), revision);
    assert!(matches!(
        db.execute_with_row_limit(&format!("{sql} LIMIT 1; {sql} LIMIT 2"), 2),
        Err(Error::ResultLimitExceeded { max: 2, .. })
    ));
}

#[test]
fn unsupported_join_shapes_are_explicit_even_for_empty_inputs() {
    let db = fixture(false);
    for sql in [
        "SELECT COUNT(*) FROM lhs a JOIN rhs b ON a.link=b.link",
        "SELECT a.id FROM lhs a JOIN rhs b ON a.link=b.link GROUP BY a.id",
        "SELECT a.id FROM lhs a CROSS JOIN rhs b",
        "SELECT a.id FROM lhs a RIGHT JOIN rhs b ON a.link=b.link",
        "SELECT a.id FROM lhs a JOIN rhs b ON a.id>b.id",
        "SELECT * FROM lhs a JOIN rhs b USING(link)",
        "SELECT a.id FROM lhs a WITH (NOLOCK) JOIN rhs b ON a.link=b.link",
        "SELECT a.id FROM lhs a JOIN rhs b WITH (NOLOCK) ON a.link=b.link",
        "SELECT a.id FROM lhs PARTITION (p0) a JOIN rhs b ON a.link=b.link",
        "SELECT a.id FROM lhs a JOIN rhs PARTITION (p0) b ON a.link=b.link",
    ] {
        assert!(
            matches!(db.execute(sql), Err(Error::Unsupported(_))),
            "{sql}"
        );
    }
    assert!(matches!(
        db.query_intent("SELECT a.id FROM lhs a JOIN rhs b ON a.link=b.link"),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        db.execute("EXPLAIN SELECT a.id FROM lhs a JOIN rhs b ON a.link=b.link"),
        Err(Error::Unsupported(_))
    ));
    db.execute("DELETE FROM lhs; DELETE FROM rhs").unwrap();
    assert!(matches!(
        db.execute("SELECT unknown FROM lhs a JOIN rhs b ON a.link=b.link"),
        Err(Error::ColumnNotFound(_))
    ));
    assert!(db
        .execute("SELECT a.id FROM lhs a JOIN rhs b ON a.title=b.id")
        .is_err());
}

#[test]
fn dense_equal_keys_do_not_require_materializing_the_cartesian_match_set() {
    let db = Database::new();
    db.execute(
        "CREATE TABLE lefts (id INTEGER,k INTEGER); CREATE TABLE rights (id INTEGER,k INTEGER)",
    )
    .unwrap();
    for name in ["lefts", "rights"] {
        db.insert_rows(
            name,
            (0..2000)
                .map(|id| vec![Value::Integer(id), Value::Integer(1)])
                .collect(),
            InsertConflict::Fail,
        )
        .unwrap();
    }
    let result = query(
        &db,
        "SELECT a.id,b.id FROM lefts a JOIN rights b ON a.k=b.k LIMIT 3",
    );
    assert_eq!(result.rows_examined, 3);
    assert_eq!(
        result.rows,
        vec![
            vec![Value::Integer(0), Value::Integer(0)],
            vec![Value::Integer(0), Value::Integer(1)],
            vec![Value::Integer(0), Value::Integer(2)]
        ]
    );
}

#[test]
fn zero_limit_validates_query_without_building_or_probing_join() {
    for indexed in [false, true] {
        let db = fixture(indexed);
        for suffix in [
            "LIMIT 0",
            "ORDER BY a.id LIMIT 0",
            "ORDER BY a.id LIMIT 0 OFFSET 2",
        ] {
            let result = query(
                &db,
                &format!("SELECT a.id FROM lhs a JOIN rhs b ON a.link=b.link {suffix}"),
            );
            assert!(result.rows.is_empty());
            assert_eq!(result.rows_examined, 0);
            assert_eq!(result.columns, ["id"]);
        }
        assert!(matches!(
            db.execute("SELECT missing FROM lhs a JOIN rhs b ON a.link=b.link LIMIT 0"),
            Err(Error::ColumnNotFound(_))
        ));
    }
}

#[test]
fn unsupported_expression_modifiers_fail_for_populated_and_empty_joins() {
    let db = fixture(false);
    for empty in [false, true] {
        if empty {
            db.execute("DELETE FROM lhs; DELETE FROM rhs").unwrap();
        }
        for sql in [
            "SELECT a.title LIKE 'x!_%' ESCAPE '!' FROM lhs a JOIN rhs b ON a.link=b.link",
            "SELECT a.id FROM lhs a JOIN rhs b ON a.link=b.link AND a.title LIKE 'x!_%' ESCAPE '!'",
            "SELECT a.id FROM lhs a JOIN rhs b ON a.link=b.link WHERE a.title ILIKE 'x!_%' ESCAPE '!'",
            "SELECT vector_dims(a.embedding) IGNORE NULLS FROM lhs a JOIN rhs b ON a.link=b.link",
            "SELECT vector_dims(a.embedding) RESPECT NULLS FROM lhs a JOIN rhs b ON a.link=b.link",
            "SELECT CAST(a.id AS TEXT FORMAT 'HEX') FROM lhs a JOIN rhs b ON a.link=b.link",
        ] {
            assert!(matches!(db.execute(sql), Err(Error::Unsupported(_))), "{sql}; empty={empty}");
        }
    }
}
