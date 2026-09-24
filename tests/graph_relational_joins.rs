use vectors::{Database, Error, ExecutionResult, InsertConflict, QueryResult, Value, Vector};

fn query(db: &Database, sql: &str) -> QueryResult {
    match db.execute(sql).unwrap().remove(0) {
        ExecutionResult::Query(result) => result,
        _ => panic!("expected query"),
    }
}

fn fixture(indexed: bool) -> Database {
    let db = Database::new();
    db.execute(
        "CREATE TABLE chunks (id INTEGER PRIMARY KEY, document_id INTEGER, embedding VECTOR(2));
         CREATE TABLE documents (id INTEGER PRIMARY KEY, account_id INTEGER, published BOOLEAN);
         CREATE TABLE accounts (id INTEGER, label TEXT, enabled BOOLEAN);
         INSERT INTO chunks VALUES (1,10,ARRAY[1,0]),(2,20,ARRAY[0.8,0.2]),(3,30,ARRAY[0,1]),(4,40,ARRAY[-1,0]),(5,NULL,ARRAY[1,0]),(6,60,ARRAY[1,0]);
         INSERT INTO documents VALUES (10,100,true),(20,100,true),(30,200,true),(40,100,false),(60,300,true);
         INSERT INTO accounts VALUES (100,'main',true),(100,'duplicate',true),(200,'disabled',false),(NULL,'null key',true);",
    ).unwrap();
    if indexed {
        db.execute("CREATE INDEX account_lookup ON accounts USING HASH(id)")
            .unwrap();
    }
    db
}

#[test]
fn vector_chunks_join_documents_and_business_tables_with_typed_parameters() {
    for indexed in [false, true] {
        let db = fixture(indexed);
        let result = db
            .execute_with_parameters(
                "SELECT c.id,a.label,c.embedding <=> $1 AS distance FROM chunks c
             JOIN documents d ON c.document_id=d.id
             JOIN accounts a ON d.account_id=a.id
             WHERE d.published=$2 AND a.enabled=true
             ORDER BY distance,c.id,a.label LIMIT $3 OFFSET 1",
                &[
                    Value::Vector(Vector::new(vec![1.0, 0.0]).unwrap()),
                    Value::Boolean(true),
                    Value::Integer(2),
                ],
            )
            .unwrap()
            .remove(0);
        let ExecutionResult::Query(result) = result else {
            panic!("expected query")
        };
        let distance = f64::from(
            Vector::new(vec![0.8, 0.2])
                .unwrap()
                .cosine_distance(&Vector::new(vec![1.0, 0.0]).unwrap())
                .unwrap(),
        );
        assert_eq!(
            result.rows,
            vec![
                vec![
                    Value::Integer(1),
                    Value::Text("main".into()),
                    Value::Float(0.0)
                ],
                vec![
                    Value::Integer(2),
                    Value::Text("duplicate".into()),
                    Value::Float(distance)
                ],
            ]
        );
        assert_eq!(result.rows_examined, 12);
        let wildcard = query(&db, "SELECT a.* FROM chunks c JOIN documents d ON c.document_id=d.id JOIN accounts a ON a.id=d.account_id ORDER BY c.id,a.label LIMIT 1");
        assert_eq!(wildcard.columns, ["id", "label", "enabled"]);
        assert_eq!(
            wildcard.rows,
            vec![vec![
                Value::Integer(100),
                Value::Text("duplicate".into()),
                Value::Boolean(true)
            ]]
        );
        assert_eq!(query(&db, "SELECT DISTINCT d.account_id FROM chunks c JOIN documents d ON c.document_id=d.id JOIN accounts a ON a.id=d.account_id ORDER BY d.account_id").rows,
            vec![vec![Value::Integer(100)],vec![Value::Integer(200)]]);
    }
}

#[test]
fn chained_left_joins_null_extend_each_stage_and_preserve_on_where_semantics() {
    let db = fixture(true);
    let base = "SELECT c.id,d.id,a.label FROM chunks c LEFT JOIN documents d ON c.document_id=d.id AND d.published=true LEFT JOIN accounts a ON d.account_id=a.id AND a.enabled=true";
    assert_eq!(
        query(&db, &format!("{base} WHERE a.id IS NULL ORDER BY c.id")).rows,
        vec![
            vec![Value::Integer(3), Value::Integer(30), Value::Null],
            vec![Value::Integer(4), Value::Null, Value::Null],
            vec![Value::Integer(5), Value::Null, Value::Null],
            vec![Value::Integer(6), Value::Integer(60), Value::Null],
        ]
    );
    assert_eq!(
        query(
            &db,
            &format!("{base} WHERE a.enabled=true ORDER BY c.id,a.label")
        )
        .rows
        .len(),
        4
    );
}

#[test]
fn later_join_can_reach_an_earlier_table_after_null_extension() {
    let db = fixture(true);
    // A later join may match the original table after an earlier LEFT JOIN
    // failed. NULL extension must not erase any earlier source's values.
    let rows = query(&db, "SELECT c.id,d.id,e.id FROM chunks c LEFT JOIN documents d ON c.document_id=d.id AND d.id<0 JOIN chunks e ON e.id=c.id ORDER BY c.id");
    assert_eq!(
        rows.rows,
        (1..=6)
            .map(|id| vec![Value::Integer(id), Value::Null, Value::Integer(id)])
            .collect::<Vec<_>>()
    );
    // An ON match must not become an unmatched LEFT row when the following
    // INNER JOIN or the final WHERE rejects that match.
    assert!(query(&db, "SELECT c.id FROM chunks c LEFT JOIN documents d ON c.document_id=d.id JOIN accounts a ON a.id=d.account_id WHERE d.id IS NULL").rows.is_empty());
}

#[test]
fn every_join_stage_handles_numeric_coercion_and_duplicate_matches() {
    let db = Database::new();
    db.execute("CREATE TABLE integers (k INTEGER); CREATE TABLE floats (k DOUBLE); CREATE TABLE tail (k INTEGER); INSERT INTO integers VALUES (0),(1),(9007199254740993),(NULL); INSERT INTO floats VALUES (-0.0),(1.0),(9007199254740992.0),(NULL); INSERT INTO tail VALUES (0),(1),(1),(9007199254740993),(NULL); CREATE INDEX float_key ON floats USING HASH(k); CREATE INDEX tail_key ON tail USING HASH(k)").unwrap();
    let rows = query(&db, "SELECT a.k,b.k,c.k FROM integers a JOIN floats b ON a.k=b.k JOIN tail c ON b.k=c.k ORDER BY a.k,c.k").rows;
    assert_eq!(rows.len(), 4);
    assert_eq!(
        rows[0],
        vec![Value::Integer(0), Value::Float(-0.0), Value::Integer(0)]
    );
    assert_eq!(rows[1], rows[2]);
    assert_eq!(
        rows[3],
        vec![
            Value::Integer(9007199254740993),
            Value::Float(9007199254740992.0),
            Value::Integer(9007199254740993)
        ]
    );
}

#[test]
fn join_prefix_binding_rejects_forward_references_and_ambiguity_even_when_empty() {
    let db = fixture(false);
    for empty in [false, true] {
        if empty {
            db.execute("DELETE FROM chunks").unwrap();
        }
        for sql in [
            "SELECT id FROM chunks c JOIN documents d ON c.document_id=d.id JOIN accounts a ON d.account_id=a.id",
            "SELECT c.id FROM chunks c JOIN documents d ON c.document_id=d.id JOIN accounts a ON account_id=id",
            "SELECT c.id FROM chunks c JOIN documents d ON c.document_id=a.id JOIN accounts a ON d.account_id=a.id",
            "SELECT c.id FROM chunks c JOIN documents d ON c.document_id=d.id AND a.enabled=true JOIN accounts a ON d.account_id=a.id",
            "SELECT c.id FROM chunks c JOIN documents d ON c.document_id=d.id JOIN accounts c ON d.account_id=c.id",
            "SELECT c.id,d.id,a.id FROM chunks c JOIN documents d ON c.document_id=d.id JOIN accounts a ON d.account_id=a.id ORDER BY id",
        ] { assert!(db.execute(sql).is_err(), "{sql}; empty={empty}"); }
        for sql in [
            "SELECT c.id FROM chunks c JOIN documents d ON c.document_id=d.id JOIN accounts a ON d.id=c.id",
            "SELECT c.id FROM chunks c JOIN documents d ON c.document_id=d.id FULL JOIN accounts a ON d.account_id=a.id",
            "SELECT COUNT(*) FROM chunks c JOIN documents d ON c.document_id=d.id JOIN accounts a ON d.account_id=a.id",
            "SELECT c.id FROM chunks c JOIN documents d ON c.document_id=d.id JOIN chunks a ON c.embedding=a.embedding",
            "SELECT c.id FROM chunks c JOIN documents d ON c.document_id=d.id JOIN accounts a ON d.account_id=a.id+1",
        ] { assert!(matches!(db.execute(sql), Err(Error::Unsupported(_))), "{sql}; empty={empty}"); }
    }
}

#[test]
fn bounded_depth_and_zero_limits_validate_without_probing() {
    let db = Database::new();
    db.execute("CREATE TABLE nodes (id INTEGER PRIMARY KEY); INSERT INTO nodes VALUES (1)")
        .unwrap();
    let mut sql = "SELECT n0.id FROM nodes n0".to_owned();
    for index in 1..16 {
        sql.push_str(&format!(
            " LEFT JOIN nodes n{index} ON n{}.id=n{index}.id",
            index - 1
        ));
    }
    assert_eq!(query(&db, &sql).rows, vec![vec![Value::Integer(1)]]);
    let zero = query(&db, &format!("{sql} ORDER BY n15.id LIMIT 0"));
    assert!(zero.rows.is_empty());
    assert_eq!(zero.rows_examined, 0);
    sql.push_str(" JOIN nodes n16 ON n15.id=n16.id LIMIT 0");
    assert!(
        matches!(db.execute(&sql), Err(Error::Unsupported(message)) if message.contains("16 tables"))
    );
}

#[test]
fn empty_inner_stage_short_circuits_only_after_complete_validation() {
    let db = fixture(false);
    db.execute("DELETE FROM accounts").unwrap();
    let sql = "SELECT c.id,a.label FROM chunks c LEFT JOIN documents d ON c.document_id=d.id JOIN accounts a ON d.account_id=a.id";
    let result = query(&db, sql);
    assert!(result.rows.is_empty());
    assert_eq!(result.rows_examined, 0);
    assert!(matches!(
        db.execute(&sql.replace("a.label", "a.missing")),
        Err(Error::ColumnNotFound(_))
    ));
    // Empty LEFT sources must still preserve the preceding rows.
    assert_eq!(
        query(&db, &sql.replace("JOIN accounts", "LEFT JOIN accounts"))
            .rows
            .len(),
        6
    );
}

#[test]
fn dense_join_chains_stream_limits_and_enforce_atomic_response_budgets() {
    let db = Database::new();
    db.execute("CREATE TABLE nodes (id INTEGER,k INTEGER)")
        .unwrap();
    db.insert_rows(
        "nodes",
        (0..2000)
            .map(|id| vec![Value::Integer(id), Value::Integer(1)])
            .collect(),
        InsertConflict::Fail,
    )
    .unwrap();
    let sql = "SELECT a.id,b.id,c.id FROM nodes a JOIN nodes b ON a.k=b.k JOIN nodes c ON b.k=c.k";
    let result = query(&db, &format!("{sql} LIMIT 3"));
    assert_eq!(result.rows_examined, 4);
    assert_eq!(
        result.rows,
        (0..3)
            .map(|id| vec![Value::Integer(0), Value::Integer(0), Value::Integer(id)])
            .collect::<Vec<_>>()
    );
    assert!(matches!(
        db.execute_with_row_limit(sql, 2),
        Err(Error::ResultLimitExceeded {
            max: 2,
            found_at_least: 3
        })
    ));
    let revision = db.revision().unwrap();
    assert!(matches!(
        db.execute_with_row_limit(&format!("INSERT INTO nodes VALUES (9000,2); {sql}"), 2),
        Err(Error::ResultLimitExceeded { .. })
    ));
    assert_eq!(db.revision().unwrap(), revision);
    assert!(query(&db, "SELECT id FROM nodes WHERE id=9000")
        .rows
        .is_empty());
}

#[test]
fn four_table_join_shapes_match_an_independent_nested_loop_reference() {
    let db = Database::new();
    db.execute("CREATE TABLE nodes (id INTEGER PRIMARY KEY,k INTEGER,next_key INTEGER,enabled BOOLEAN); INSERT INTO nodes VALUES (1,1,2,true),(2,1,NULL,true),(3,2,1,false),(4,NULL,1,true),(5,3,9,true),(6,2,3,true)").unwrap();
    let rows = query(&db, "SELECT * FROM nodes ORDER BY id").rows;
    for indexed in [false, true] {
        if indexed {
            db.execute("CREATE INDEX node_key ON nodes USING HASH(k)")
                .unwrap();
        }
        // All eight INNER/LEFT combinations, with duplicate and NULL keys,
        // residual ON filtering and a final WHERE using two different stages.
        for shape in 0..8 {
            let mut sql = "SELECT a.id,b.id,c.id,d.id FROM nodes a".to_owned();
            let mut combinations: Vec<Vec<Option<&Vec<Value>>>> =
                rows.iter().map(|row| vec![Some(row)]).collect();
            for stage in 0..3 {
                let outer = shape & (1 << stage) != 0;
                let left = ['a', 'b', 'c'][stage];
                let right = ['b', 'c', 'd'][stage];
                sql.push_str(&format!(
                    " {} JOIN nodes {right} ON {left}.next_key={right}.k AND {right}.enabled=true",
                    if outer { "LEFT" } else { "INNER" }
                ));
                combinations = combinations
                    .into_iter()
                    .flat_map(|previous| {
                        let mut next = Vec::new();
                        for row in &rows {
                            if previous[stage]
                                .is_some_and(|left| left[2] != Value::Null && left[2] == row[1])
                                && row[3] == Value::Boolean(true)
                            {
                                let mut combination = previous.clone();
                                combination.push(Some(row));
                                next.push(combination);
                            }
                        }
                        if outer && next.is_empty() {
                            let mut combination = previous;
                            combination.push(None);
                            next.push(combination);
                        }
                        next
                    })
                    .collect();
            }
            sql.push_str(" WHERE d.enabled=true OR b.id IS NULL");
            let expected = combinations
                .into_iter()
                .filter(|combination| {
                    combination[3].is_some_and(|row| row[3] == Value::Boolean(true))
                        || combination[1].is_none()
                })
                .map(|combination| {
                    combination
                        .into_iter()
                        .map(|row| row.map_or(Value::Null, |row| row[0].clone()))
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();
            assert_eq!(
                query(&db, &sql).rows,
                expected,
                "shape={shape}, indexed={indexed}"
            );
        }
    }
}
