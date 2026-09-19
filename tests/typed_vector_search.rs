use std::sync::atomic::{AtomicU64, Ordering};

use vectors::{
    ComputeConfig, ComputeDevice, Database, Error, ExecutionResult, InsertConflict, QueryResult,
    Value, Vector, VectorFilterOperator as Op, VectorSearch, VectorSearchFilter,
    VectorSearchMetric,
};

fn database() -> Database {
    Database::new_with_compute(ComputeConfig {
        device: ComputeDevice::Cpu,
        ..ComputeConfig::default()
    })
}

fn request(metric: VectorSearchMetric) -> VectorSearch {
    VectorSearch {
        table: "PoInTs".into(),
        vector_column: "EMBEDDING".into(),
        query: Vector::new(vec![0.6, -0.25, 1.5]).unwrap(),
        metric,
        select: vec!["ID".into(), "title".into()],
        filters: Vec::new(),
        limit: 4,
    }
}

fn filter(column: &str, operator: Op, value: Value) -> VectorSearchFilter {
    VectorSearchFilter {
        column: column.into(),
        operator,
        value,
    }
}

fn sql_query(database: &Database, sql: &str) -> QueryResult {
    match database.execute(sql).unwrap().pop().unwrap() {
        ExecutionResult::Query(result) => result,
        other => panic!("expected query, found {other:?}"),
    }
}

fn fixture() -> Database {
    let db = database();
    db.execute("CREATE TABLE points (id INTEGER PRIMARY KEY, title TEXT, category TEXT, weight DOUBLE, active BOOLEAN, embedding VECTOR(3))").unwrap();
    db.insert_rows(
        "points",
        (0..24)
            .map(|id| {
                vec![
                    Value::Integer(id),
                    if id % 5 == 0 {
                        Value::Null
                    } else {
                        Value::Text(format!("item '{id}"))
                    },
                    Value::Text(if id % 3 == 0 { "keep" } else { "skip" }.into()),
                    if id % 7 == 0 {
                        Value::Null
                    } else {
                        Value::Float((id as f64 - 10.0) / 2.0)
                    },
                    Value::Boolean(id % 2 == 0),
                    if id % 8 == 0 {
                        Value::Null
                    } else {
                        Value::Vector(Vector::new(vec![id as f32 / 13.0, -0.25, 1.0]).unwrap())
                    },
                ]
            })
            .collect(),
        InsertConflict::Fail,
    )
    .unwrap();
    db
}

#[test]
fn typed_search_matches_sql_for_all_metrics_filters_and_index_paths() {
    let db = fixture();
    let cases = [
        (vec![], ""),
        (
            vec![filter("CaTeGoRy", Op::Eq, Value::Text("keep".into()))],
            "WHERE category = 'keep'",
        ),
        (
            vec![
                filter("category", Op::Eq, Value::Text("keep".into())),
                filter("weight", Op::Gte, Value::Integer(1)),
            ],
            "WHERE category = 'keep' AND weight >= 1.0",
        ),
        (
            vec![
                filter("id", Op::Gt, Value::Integer(2)),
                filter("id", Op::Lte, Value::Integer(19)),
                filter("active", Op::Ne, Value::Boolean(false)),
            ],
            "WHERE id > 2 AND id <= 19 AND active != FALSE",
        ),
        (
            vec![filter("weight", Op::Lt, Value::Float(1.5))],
            "WHERE weight < 1.5",
        ),
        (
            vec![filter("title", Op::Eq, Value::Null)],
            "WHERE title IS NULL",
        ),
        (
            vec![filter("embedding", Op::Ne, Value::Null)],
            "WHERE embedding IS NOT NULL",
        ),
        (
            vec![filter("embedding", Op::Eq, Value::Null)],
            "WHERE embedding IS NULL",
        ),
        (
            vec![filter("title", Op::Eq, Value::Text("item '13".into()))],
            "WHERE title = 'item ''13'",
        ),
        (
            vec![filter("category", Op::Eq, Value::Text("absent".into()))],
            "WHERE category = 'absent'",
        ),
    ];
    for indexed in [false, true] {
        if indexed {
            db.execute("CREATE INDEX category_idx ON points USING HASH (category)")
                .unwrap();
        }
        for (metric, function, direction) in [
            (VectorSearchMetric::Cosine, "cosine_distance", "ASC"),
            (VectorSearchMetric::L2, "l2_distance", "ASC"),
            (VectorSearchMetric::SquaredL2, "squared_l2_distance", "ASC"),
            (VectorSearchMetric::DotProduct, "dot_product", "DESC"),
        ] {
            for (filters, predicate) in &cases {
                let mut search = request(metric);
                search.filters = filters.clone();
                let sql = format!("SELECT id, title, {function}(embedding, ARRAY[0.6, -0.25, 1.5]) AS distance FROM points {predicate} ORDER BY distance {direction} LIMIT 4");
                let expected = sql_query(&db, &sql);
                assert_eq!(
                    db.search_vectors(search).unwrap(),
                    expected,
                    "indexed={indexed}, {sql}"
                );
            }
        }
    }
}

#[test]
fn typed_search_preserves_default_vector_and_quoted_projections_and_rejects_duplicates() {
    let db = fixture();
    let mut search = request(VectorSearchMetric::L2);
    search.select.clear();
    let expected = sql_query(&db, "SELECT id, title, category, weight, active, l2_distance(embedding, ARRAY[0.6, -0.25, 1.5]) AS distance FROM points ORDER BY distance LIMIT 4");
    assert_eq!(db.search_vectors(search.clone()).unwrap(), expected);
    search.select = vec!["embedding".into(), "ID".into()];
    let expected = sql_query(&db, "SELECT embedding, id, l2_distance(embedding, ARRAY[0.6, -0.25, 1.5]) AS distance FROM points ORDER BY distance LIMIT 4");
    assert_eq!(db.search_vectors(search.clone()).unwrap(), expected);
    search.select.push("id".into());
    assert_eq!(
        db.search_vectors(search),
        Err(Error::DuplicateColumn("id".into()))
    );

    db.execute("CREATE TABLE \"odd table\" (\"other vector\" VECTOR(3), \"em\"\"bed\" VECTOR(3)); INSERT INTO \"odd table\" VALUES (ARRAY[1,0,0], ARRAY[0,1,0])").unwrap();
    let mut search = request(VectorSearchMetric::SquaredL2);
    search.table = "odd table".into();
    search.vector_column = "em\"bed".into();
    search.select.clear();
    let actual = db.search_vectors(search).unwrap();
    let expected = sql_query(&db, "SELECT \"em\"\"bed\", squared_l2_distance(\"em\"\"bed\", ARRAY[0.6,-0.25,1.5]) AS distance FROM \"odd table\" ORDER BY distance LIMIT 4");
    assert_eq!(actual, expected);
}

#[test]
fn selected_distance_column_cannot_shadow_the_computed_rank() {
    let db = database();
    db.execute("CREATE TABLE points (id INTEGER, distance DOUBLE, embedding VECTOR(3)); INSERT INTO points VALUES (1, 100.0, ARRAY[0.6,-0.25,1.5]), (2, -100.0, ARRAY[1,2,3])").unwrap();
    let mut search = request(VectorSearchMetric::SquaredL2);
    search.select = vec!["id".into(), "distance".into()];
    let result = db.search_vectors(search).unwrap();
    assert_eq!(result.columns, ["id", "distance", "distance"]);
    assert_eq!(
        result.rows[0],
        [Value::Integer(1), Value::Float(100.0), Value::Float(0.0)]
    );
    let expected = sql_query(&db, "SELECT id, distance, squared_l2_distance(embedding, ARRAY[0.6,-0.25,1.5]) AS score FROM points ORDER BY score LIMIT 4");
    assert_eq!(result.rows, expected.rows);
}

#[test]
fn invalid_typed_search_is_rejected_even_when_no_rows_are_scanned() {
    let db = database();
    db.execute("CREATE TABLE points (id INTEGER, title TEXT, embedding VECTOR(3))")
        .unwrap();
    let valid = request(VectorSearchMetric::Cosine);
    let mut invalid = valid.clone();
    invalid.query = Vector::new(vec![1.0]).unwrap();
    assert!(matches!(
        db.search_vectors(invalid),
        Err(Error::DimensionMismatch { left: 3, right: 1 })
    ));
    let mut invalid = valid.clone();
    invalid.vector_column = "id".into();
    assert!(matches!(
        db.search_vectors(invalid),
        Err(Error::TypeMismatch { .. })
    ));
    for (column, operator, value) in [
        ("id", Op::Eq, Value::Float(1.0)),
        ("title", Op::Eq, Value::Boolean(true)),
        ("id", Op::Gt, Value::Null),
        (
            "embedding",
            Op::Eq,
            Value::Vector(Vector::new(vec![1.0; 3]).unwrap()),
        ),
        ("absent", Op::Eq, Value::Null),
    ] {
        let mut invalid = valid.clone();
        invalid.filters.push(filter(column, operator, value));
        assert!(db.search_vectors(invalid).is_err());
    }
    let mut invalid = valid.clone();
    invalid.select.push("absent".into());
    assert!(matches!(
        db.search_vectors(invalid),
        Err(Error::ColumnNotFound(_))
    ));
    let mut invalid = valid;
    invalid.limit = 0;
    assert!(matches!(
        db.search_vectors(invalid),
        Err(Error::InvalidQuery(_))
    ));
}

#[test]
fn numeric_filters_preserve_i64_boundaries_large_floats_and_finite_validation() {
    let db = database();
    db.execute("CREATE TABLE points (id INTEGER, title TEXT, weight DOUBLE, embedding VECTOR(3)); CREATE INDEX weights ON points USING HASH(weight)").unwrap();
    db.insert_rows(
        "points",
        vec![vec![
            Value::Integer(i64::MIN),
            Value::Text("edge".into()),
            Value::Float(1e100),
            Value::Vector(Vector::new(vec![1.0; 3]).unwrap()),
        ]],
        InsertConflict::Fail,
    )
    .unwrap();
    for predicate in [
        filter("id", Op::Eq, Value::Integer(i64::MIN)),
        filter("weight", Op::Eq, Value::Float(1e100)),
    ] {
        let mut search = request(VectorSearchMetric::L2);
        search.filters = vec![predicate];
        assert_eq!(
            db.search_vectors(search).unwrap().rows[0][0],
            Value::Integer(i64::MIN)
        );
    }
    for value in [f64::INFINITY, f64::NEG_INFINITY, f64::NAN] {
        let mut search = request(VectorSearchMetric::Cosine);
        search
            .filters
            .push(filter("weight", Op::Eq, Value::Float(value)));
        assert!(matches!(
            db.search_vectors(search),
            Err(Error::InvalidQuery(_))
        ));
    }
}

#[test]
fn cosine_zero_norm_matches_sql_errors_and_empty_candidate_behavior() {
    let db = fixture();
    let mut search = request(VectorSearchMetric::Cosine);
    search.query = Vector::new(vec![0.0; 3]).unwrap();
    assert_eq!(db.search_vectors(search.clone()), Err(Error::ZeroNorm));
    assert_eq!(db.execute("SELECT id, cosine_distance(embedding, ARRAY[0,0,0]) AS distance FROM points ORDER BY distance LIMIT 4"), Err(Error::ZeroNorm));
    search
        .filters
        .push(filter("id", Op::Eq, Value::Integer(-1)));
    assert!(db.search_vectors(search).unwrap().rows.is_empty());
}

#[test]
fn large_typed_filter_conjunction_uses_bounded_stack_depth() {
    let db = fixture();
    let mut search = request(VectorSearchMetric::L2);
    search.filters = vec![filter("id", Op::Gte, Value::Integer(0)); 4096];
    let actual = db.search_vectors(search).unwrap();
    assert_eq!(
        actual,
        db.search_vectors(request(VectorSearchMetric::L2)).unwrap()
    );
}

#[test]
fn typed_search_keeps_schema_and_row_plan_in_one_catalog_snapshot() {
    let db = database();
    db.execute("CREATE TABLE points (id INTEGER, title TEXT, embedding VECTOR(3)); INSERT INTO points VALUES (1,'first',ARRAY[1,0,0])").unwrap();
    let writer = db.clone();
    let task = std::thread::spawn(move || {
        for iteration in 0..100 {
            let sql = if iteration % 2 == 0 {
                "DROP TABLE points; CREATE TABLE points (embedding VECTOR(3), title TEXT, id INTEGER); INSERT INTO points VALUES (ARRAY[0,1,0],'second',2)"
            } else {
                "DROP TABLE points; CREATE TABLE points (id INTEGER, title TEXT, embedding VECTOR(3)); INSERT INTO points VALUES (1,'first',ARRAY[1,0,0])"
            };
            writer.execute(sql).unwrap();
        }
    });
    for _ in 0..100 {
        let result = db
            .search_vectors(request(VectorSearchMetric::SquaredL2))
            .unwrap();
        assert_eq!(result.rows.len(), 1);
        let row = &result.rows[0];
        assert!(
            matches!((&row[0], &row[1]), (Value::Integer(1), Value::Text(title)) if title == "first")
                || matches!((&row[0], &row[1]), (Value::Integer(2), Value::Text(title)) if title == "second")
        );
    }
    task.join().unwrap();
}

fn check_schema_guard(db: &Database) {
    db.execute("CREATE TABLE guarded (left_value INTEGER, right_value INTEGER)")
        .unwrap();
    let stale = db.schema("guarded").unwrap();
    db.execute(
        "DROP TABLE guarded; CREATE TABLE guarded (right_value INTEGER, left_value INTEGER)",
    )
    .unwrap();
    let revision = db.revision().unwrap();
    for rows in [vec![], vec![vec![Value::Integer(1), Value::Integer(2)]]] {
        assert_eq!(
            db.insert_rows_if_schema("GUARDED", &stale, rows, InsertConflict::Fail),
            Err(Error::SchemaChanged {
                table: "guarded".into()
            })
        );
        assert_eq!(db.revision().unwrap(), revision);
        assert!(sql_query(db, "SELECT * FROM guarded").rows.is_empty());
    }
    let current = db.schema("guarded").unwrap();
    for changed in 0..4 {
        let mut stale = current.clone();
        match changed {
            0 => stale[0].name = "renamed".into(),
            1 => stale[0].data_type = vectors::DataType::Float,
            2 => stale[0].nullable = !stale[0].nullable,
            3 => stale[0].unique = !stale[0].unique,
            _ => unreachable!(),
        }
        assert!(matches!(
            db.insert_rows_if_schema(
                "guarded",
                &stale,
                vec![vec![Value::Integer(1), Value::Integer(2)]],
                InsertConflict::Fail
            ),
            Err(Error::SchemaChanged { .. })
        ));
    }
    assert_eq!(
        db.insert_rows_if_schema(
            "guarded",
            &current,
            vec![vec![Value::Integer(2), Value::Integer(1)]],
            InsertConflict::Fail
        )
        .unwrap(),
        1
    );
}

#[test]
fn schema_guard_prevents_reinterpreted_typed_inserts_in_memory() {
    check_schema_guard(&database());
}

#[test]
fn schema_guard_prevents_reinterpreted_typed_inserts_in_durable_storage() {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let directory = std::env::temp_dir().join(format!(
        "vectors-typed-schema-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let db = Database::open_persistent(&directory).unwrap();
    check_schema_guard(&db);
    let revision = db.revision().unwrap();
    drop(db);
    let reopened = Database::open_persistent(&directory).unwrap();
    assert_eq!(reopened.revision().unwrap(), revision);
    assert_eq!(
        sql_query(&reopened, "SELECT * FROM guarded").rows,
        [vec![Value::Integer(2), Value::Integer(1)]]
    );
    drop(reopened);
    std::fs::remove_dir_all(directory).unwrap();
}
