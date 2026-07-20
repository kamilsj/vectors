use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use vectors::{DataType, Database, Error, ExecutionResult, Value, Vector};

static SNAPSHOT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn snapshot_path(label: &str) -> PathBuf {
    let sequence = SNAPSHOT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "vectors-test-{}-{sequence}-{label}.vdb",
        std::process::id()
    ))
}

fn query(database: &Database, sql: &str) -> vectors::QueryResult {
    let results = database.execute(sql).expect("query should succeed");
    assert_eq!(results.len(), 1);
    match results.into_iter().next().unwrap() {
        ExecutionResult::Query(result) => result,
        result => panic!("expected query result, found {result:?}"),
    }
}

fn seeded_database() -> Database {
    let database = Database::new();
    database
        .execute(
            "CREATE TABLE documents (
                id BIGINT PRIMARY KEY,
                title TEXT NOT NULL,
                category TEXT,
                rating DOUBLE,
                active BOOLEAN,
                embedding VECTOR(3)
            );
            INSERT INTO documents VALUES
                (1, 'Rust guide',   'tech', 9.2, TRUE,  ARRAY[1, 0, 0]),
                (2, 'Cooking',      'food', 8.0, TRUE,  ARRAY[0, 1, 0]),
                (3, 'Rust vectors', 'tech', 7.5, FALSE, ARRAY[0.8, 0.2, 0]),
                (4, 'Null island',  NULL,   NULL, FALSE, ARRAY[0, 0, 1]);",
        )
        .expect("seed statements should succeed");
    database
}

#[test]
fn executes_hybrid_similarity_query() {
    let database = seeded_database();
    let result = query(
        &database,
        "SELECT id, title, cosine_distance(embedding, ARRAY[1, 0, 0]) AS distance
         FROM documents
         WHERE category = 'tech' AND rating >= 7
         ORDER BY distance ASC
         LIMIT 2",
    );

    assert_eq!(result.columns, ["id", "title", "distance"]);
    assert_eq!(result.row_count(), 2);
    assert_eq!(result.rows[0][0], Value::Integer(1));
    assert_eq!(result.rows[1][0], Value::Integer(3));
    assert_eq!(result.rows[0][2], Value::Float(0.0));
}

#[test]
fn supports_scalar_predicates_distinct_offset_and_wildcards() {
    let database = seeded_database();

    let result = query(
        &database,
        "SELECT id, title FROM documents
         WHERE (title ILIKE '%rust%' OR category IN ('food')) AND id BETWEEN 1 AND 3
         ORDER BY id DESC LIMIT 2 OFFSET 1",
    );
    assert_eq!(result.rows.len(), 2);
    assert_eq!(result.rows[0][0], Value::Integer(2));
    assert_eq!(result.rows[1][0], Value::Integer(1));

    let distinct = query(
        &database,
        "SELECT DISTINCT active FROM documents ORDER BY active",
    );
    assert_eq!(distinct.rows.len(), 2);

    let all = query(&database, "SELECT * FROM documents WHERE id = 1");
    assert_eq!(all.columns.len(), 6);
    assert_eq!(all.rows.len(), 1);
}

#[test]
fn vector_functions_cover_supported_metrics() {
    let database = seeded_database();
    let result = query(
        &database,
        "SELECT
            l2_distance(embedding, VECTOR(0, 1, 0)) AS l2,
            squared_l2_distance(embedding, ARRAY[0, 1, 0]) AS l2_squared,
            dot_product(embedding, ARRAY[0, 1, 0]) AS dot,
            vector_dims(embedding) AS dims,
            vector_norm(embedding) AS norm,
            normalize(ARRAY[3, 4, 0]) AS normalized
         FROM documents WHERE id = 2",
    );

    assert_eq!(
        result.rows[0],
        [
            Value::Float(0.0),
            Value::Float(0.0),
            Value::Float(1.0),
            Value::Integer(3),
            Value::Float(1.0),
            Value::Vector(Vector::new(vec![0.6, 0.8, 0.0]).unwrap()),
        ]
    );
}

#[test]
fn enforces_schema_and_keeps_failed_batch_atomic() {
    let database = seeded_database();
    let error = database
        .execute(
            "INSERT INTO documents (id, title, embedding) VALUES
                (10, 'valid until batch fails', ARRAY[1, 2, 3]),
                (1, 'duplicate', ARRAY[1, 2, 3])",
        )
        .unwrap_err();
    assert_eq!(error, Error::UniqueViolation("id".into()));
    assert!(query(&database, "SELECT id FROM documents WHERE id = 10")
        .rows
        .is_empty());

    let error = database
        .execute(
            "INSERT INTO documents (id, title, embedding) VALUES
                (10, 'wrong dimensions', ARRAY[1, 2])",
        )
        .unwrap_err();
    assert_eq!(error, Error::DimensionMismatch { left: 3, right: 2 });

    let error = database
        .execute("INSERT INTO documents (id, embedding) VALUES (10, ARRAY[1, 2, 3])")
        .unwrap_err();
    assert_eq!(error, Error::NullViolation("title".into()));
}

#[test]
fn delete_is_filtered_and_atomic_on_expression_errors() {
    let database = seeded_database();
    let result = database
        .execute("DELETE FROM documents WHERE category = 'food'")
        .unwrap();
    assert_eq!(
        result[0],
        ExecutionResult::Command {
            tag: "DELETE",
            rows_affected: 1
        }
    );

    let error = database
        .execute("DELETE FROM documents WHERE id / 0 > 1")
        .unwrap_err();
    assert_eq!(error, Error::InvalidQuery("division by zero".into()));
    assert_eq!(query(&database, "SELECT id FROM documents").rows.len(), 3);
}

#[test]
fn update_is_simultaneous_constraint_checked_and_atomic() {
    let database = seeded_database();
    let result = database
        .execute(
            "UPDATE documents
             SET title = 'Updated', rating = rating + 0.5, embedding = ARRAY[1, 0, 0]
             WHERE id = 3",
        )
        .unwrap();
    assert_eq!(
        result[0],
        ExecutionResult::Command {
            tag: "UPDATE",
            rows_affected: 1
        }
    );
    let updated = query(
        &database,
        "SELECT title, rating, l2_distance(embedding, ARRAY[1, 0, 0]) AS distance
         FROM documents WHERE id = 3",
    );
    assert_eq!(
        updated.rows[0],
        [
            Value::Text("Updated".into()),
            Value::Float(8.0),
            Value::Float(0.0),
        ]
    );

    let error = database
        .execute("UPDATE documents SET id = 1, title = 'must roll back' WHERE id = 2")
        .unwrap_err();
    assert_eq!(error, Error::UniqueViolation("id".into()));
    let unchanged = query(&database, "SELECT id, title FROM documents WHERE id = 2");
    assert_eq!(
        unchanged.rows[0],
        [Value::Integer(2), Value::Text("Cooking".into())]
    );
}

#[test]
fn multi_statement_writes_commit_or_roll_back_as_one_unit() {
    let database = seeded_database();
    let error = database
        .execute(
            "INSERT INTO documents (id, title, embedding)
                 VALUES (10, 'would otherwise commit', ARRAY[1, 0, 0]);
             UPDATE documents SET id = 1 WHERE id = 2;",
        )
        .unwrap_err();
    assert_eq!(error, Error::UniqueViolation("id".into()));
    assert!(query(&database, "SELECT id FROM documents WHERE id = 10")
        .rows
        .is_empty());
    assert_eq!(
        query(&database, "SELECT id FROM documents WHERE id = 2").rows[0][0],
        Value::Integer(2)
    );

    database
        .execute(
            "INSERT INTO documents (id, title, embedding)
                 VALUES (10, 'committed', ARRAY[1, 0, 0]);
             UPDATE documents SET active = TRUE WHERE id = 3;",
        )
        .unwrap();
    assert_eq!(
        query(&database, "SELECT active FROM documents WHERE id = 3").rows[0][0],
        Value::Boolean(true)
    );
}

#[test]
fn drops_tables_and_rejects_vector_sort_keys() {
    let database = seeded_database();
    let error = database
        .execute("SELECT id FROM documents ORDER BY embedding")
        .unwrap_err();
    assert_eq!(
        error,
        Error::TypeMismatch {
            expected: "sortable scalar value".into(),
            found: "VECTOR".into()
        }
    );

    let ignored = database.execute("DROP TABLE IF EXISTS absent").unwrap();
    assert_eq!(
        ignored[0],
        ExecutionResult::Command {
            tag: "DROP TABLE",
            rows_affected: 0
        }
    );
    database.execute("DROP TABLE documents").unwrap();
    assert_eq!(
        database.schema("documents").unwrap_err(),
        Error::TableNotFound("documents".into())
    );
}

#[test]
fn supports_single_column_table_constraints_and_schema_inspection() {
    let database = Database::new();
    database
        .execute(
            "CREATE TABLE examples (
                id INTEGER,
                label VARCHAR(100),
                embedding VECTOR(2),
                PRIMARY KEY (id),
                UNIQUE (label)
            )",
        )
        .unwrap();
    let schema = database.schema("examples").unwrap();
    assert_eq!(schema[0].data_type, DataType::Integer);
    assert!(!schema[0].nullable);
    assert!(schema[0].unique);
    assert!(schema[1].unique);
    assert_eq!(schema[2].data_type, DataType::Vector(2));
}

#[test]
fn cloned_handles_are_safe_for_concurrent_writers() {
    let database = Arc::new(Database::new());
    database
        .execute("CREATE TABLE points (id INTEGER PRIMARY KEY, embedding VECTOR(2))")
        .unwrap();

    let handles = (0..8)
        .map(|id| {
            let database = Arc::clone(&database);
            thread::spawn(move || {
                database
                    .execute(&format!(
                        "INSERT INTO points VALUES ({id}, ARRAY[{id}, {id}])"
                    ))
                    .unwrap();
            })
        })
        .collect::<Vec<_>>();
    for handle in handles {
        handle.join().unwrap();
    }

    let result = query(&database, "SELECT id FROM points ORDER BY id");
    assert_eq!(result.rows.len(), 8);
    for (id, row) in result.rows.iter().enumerate() {
        assert_eq!(row[0], Value::Integer(id as i64));
    }
}

#[test]
fn vector_math_rejects_invalid_inputs() {
    let left = Vector::new(vec![1.0, 2.0, 3.0, 4.0, 5.0]).unwrap();
    let right = Vector::new(vec![1.0, 1.0, 1.0, 1.0, 1.0]).unwrap();
    assert_eq!(left.dot_product(&right).unwrap(), 15.0);
    assert_eq!(left.squared_l2_distance(&right).unwrap(), 30.0);
    assert!((left.norm() - 55.0_f64.sqrt()).abs() < f64::EPSILON);
    assert!((left.normalized().unwrap().norm() - 1.0).abs() < 1.0e-7);

    let short = Vector::new(vec![1.0]).unwrap();
    assert_eq!(
        left.l2_distance(&short).unwrap_err(),
        Error::DimensionMismatch { left: 5, right: 1 }
    );
    let zero = Vector::new(vec![0.0, 0.0, 0.0, 0.0, 0.0]).unwrap();
    assert_eq!(left.cosine_distance(&zero).unwrap_err(), Error::ZeroNorm);
    assert_eq!(
        Vector::new(Vec::new()).unwrap_err(),
        Error::InvalidVectorDimension
    );
    assert_eq!(
        Vector::new(vec![1.0, f32::NAN]).unwrap_err(),
        Error::NonFiniteVectorElement { index: 1 }
    );
}

#[test]
fn snapshots_round_trip_deterministically_and_preserve_constraints() {
    let database = seeded_database();
    database
        .execute("UPDATE documents SET rating = 9.75 WHERE id = 3")
        .unwrap();
    let path = snapshot_path("round-trip");

    database.save(&path).unwrap();
    let first_snapshot = fs::read(&path).unwrap();
    // Exercise replacement of an existing file and deterministic encoding.
    database.save(&path).unwrap();
    assert_eq!(fs::read(&path).unwrap(), first_snapshot);

    let restored = Database::open(&path).unwrap();
    let result = query(
        &restored,
        "SELECT id, rating, cosine_distance(embedding, ARRAY[1, 0, 0]) AS distance
         FROM documents WHERE category = 'tech' ORDER BY distance",
    );
    assert_eq!(result.rows.len(), 2);
    assert_eq!(result.rows[0][0], Value::Integer(1));
    assert_eq!(result.rows[1][1], Value::Float(9.75));
    assert_eq!(
        restored
            .execute(
                "INSERT INTO documents (id, title, embedding)
                 VALUES (1, 'duplicate', ARRAY[1, 0, 0])"
            )
            .unwrap_err(),
        Error::UniqueViolation("id".into())
    );

    fs::remove_file(path).unwrap();
}

#[test]
fn concurrent_snapshot_requests_from_cloned_handles_are_serialized() {
    let database = seeded_database();
    let path = snapshot_path("concurrent-saves");
    let barrier = Arc::new(Barrier::new(8));
    let handles = (0..8)
        .map(|_| {
            let database = database.clone();
            let path = path.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                database.save(path)
            })
        })
        .collect::<Vec<_>>();

    for handle in handles {
        handle.join().unwrap().unwrap();
    }
    let restored = Database::open(&path).unwrap();
    assert_eq!(
        query(&restored, "SELECT COUNT(*) FROM documents").rows[0][0],
        Value::Integer(4)
    );
    fs::remove_file(path).unwrap();
}

#[test]
fn catalog_revisions_track_committed_changes_and_skip_redundant_saves() {
    let database = Database::new();
    let initial = database.revision().unwrap();
    database.execute("SELECT 1").unwrap();
    assert_eq!(database.revision().unwrap(), initial);

    database
        .execute("CREATE TABLE entries (id INTEGER PRIMARY KEY, value TEXT)")
        .unwrap();
    let after_create = database.revision().unwrap();
    assert_ne!(after_create, initial);
    database
        .execute("CREATE TABLE IF NOT EXISTS entries (id INTEGER)")
        .unwrap();
    assert_eq!(database.revision().unwrap(), after_create);

    database
        .execute("INSERT INTO entries VALUES (1, 'one')")
        .unwrap();
    let after_insert = database.revision().unwrap();
    assert_ne!(after_insert, after_create);
    assert_eq!(
        database
            .execute("INSERT INTO entries VALUES (1, 'duplicate')")
            .unwrap_err(),
        Error::UniqueViolation("id".into())
    );
    assert_eq!(database.revision().unwrap(), after_insert);
    database
        .execute(
            "INSERT INTO entries VALUES (1, 'ignored')
             ON CONFLICT DO NOTHING",
        )
        .unwrap();
    database
        .execute("UPDATE entries SET value = 'unused' WHERE id = 99")
        .unwrap();
    assert_eq!(database.revision().unwrap(), after_insert);

    assert!(database
        .execute(
            "INSERT INTO entries VALUES (2, 'two');
             INSERT INTO entries VALUES (1, 'duplicate')"
        )
        .is_err());
    assert_eq!(database.revision().unwrap(), after_insert);

    let path = snapshot_path("revision");
    database.save(&path).unwrap();
    assert_eq!(database.save_if_changed(&path, after_insert).unwrap(), None);
    database
        .execute("UPDATE entries SET value = 'updated' WHERE id = 1")
        .unwrap();
    let after_update = database.revision().unwrap();
    assert_eq!(
        database.save_if_changed(&path, after_insert).unwrap(),
        Some(after_update)
    );
    assert_eq!(database.save_if_changed(&path, after_update).unwrap(), None);
    let restored = Database::open(&path).unwrap();
    assert_eq!(
        query(&restored, "SELECT value FROM entries WHERE id = 1").rows[0][0],
        Value::Text("updated".into())
    );
    assert_eq!(restored.revision().unwrap(), 0);
    fs::remove_file(path).unwrap();
}

#[test]
fn snapshots_detect_corruption_and_truncation() {
    let database = seeded_database();
    let corrupt_path = snapshot_path("corrupt");
    let truncated_path = snapshot_path("truncated");
    database.save(&corrupt_path).unwrap();

    let mut bytes = fs::read(&corrupt_path).unwrap();
    let content_index = bytes.len() - 9;
    bytes[content_index] ^= 0x01;
    fs::write(&corrupt_path, &bytes).unwrap();
    assert_eq!(
        Database::open(&corrupt_path).unwrap_err(),
        Error::CorruptSnapshot("snapshot checksum does not match".into())
    );

    bytes.truncate(bytes.len() - 5);
    fs::write(&truncated_path, bytes).unwrap();
    assert!(matches!(
        Database::open(&truncated_path),
        Err(Error::CorruptSnapshot(_))
    ));

    fs::remove_file(corrupt_path).unwrap();
    fs::remove_file(truncated_path).unwrap();
}

#[test]
fn vector_dimensions_have_a_resource_safety_limit() {
    assert_eq!(
        Vector::new(vec![0.0; vectors::MAX_VECTOR_DIMENSIONS + 1]).unwrap_err(),
        Error::VectorDimensionLimit {
            found: vectors::MAX_VECTOR_DIMENSIONS + 1,
            max: vectors::MAX_VECTOR_DIMENSIONS,
        }
    );
    let database = Database::new();
    assert_eq!(
        database
            .execute(&format!(
                "CREATE TABLE too_wide (embedding VECTOR({}))",
                vectors::MAX_VECTOR_DIMENSIONS + 1
            ))
            .unwrap_err(),
        Error::VectorDimensionLimit {
            found: vectors::MAX_VECTOR_DIMENSIONS + 1,
            max: vectors::MAX_VECTOR_DIMENSIONS,
        }
    );
}

#[test]
fn hash_indexes_prune_hybrid_searches_and_track_mutations() {
    let database = seeded_database();
    let unindexed = query(
        &database,
        "SELECT id, cosine_distance(embedding, ARRAY[1, 0, 0]) AS distance
         FROM documents WHERE category = 'tech' ORDER BY distance",
    );
    assert_eq!(unindexed.rows_examined, 4);

    database
        .execute("CREATE INDEX documents_category_idx ON documents USING HASH (category)")
        .unwrap();
    assert_eq!(
        database.indexes("documents").unwrap(),
        [vectors::IndexInfo {
            name: "documents_category_idx".into(),
            column: "category".into(),
        }]
    );
    let indexed = query(
        &database,
        "SELECT id, cosine_distance(embedding, ARRAY[1, 0, 0]) AS distance
         FROM documents
         WHERE category = 'tech' AND rating >= 7
         ORDER BY distance",
    );
    assert_eq!(indexed.rows, unindexed.rows);
    assert_eq!(indexed.rows_examined, 2);
    assert_eq!(
        database
            .execute("SELECT missing FROM documents WHERE category = 'absent'")
            .unwrap_err(),
        Error::ColumnNotFound("missing".into())
    );

    database
        .execute(
            "INSERT INTO documents VALUES
                (5, 'Index maintenance', 'tech', 8.5, TRUE, ARRAY[0.9, 0.1, 0]);
             UPDATE documents SET category = 'tech' WHERE id = 2;
             DELETE FROM documents WHERE id = 1;",
        )
        .unwrap();
    let after_mutations = query(
        &database,
        "SELECT id FROM documents WHERE category = 'tech' ORDER BY id",
    );
    assert_eq!(after_mutations.rows_examined, 3);
    assert_eq!(
        after_mutations.rows,
        [
            vec![Value::Integer(2)],
            vec![Value::Integer(3)],
            vec![Value::Integer(5)],
        ]
    );

    database
        .execute("DROP INDEX documents_category_idx")
        .unwrap();
    assert!(database.indexes("documents").unwrap().is_empty());
    assert_eq!(
        query(
            &database,
            "SELECT id FROM documents WHERE category = 'tech'"
        )
        .rows_examined,
        4
    );
}

#[test]
fn indexes_persist_and_participate_in_batch_rollback() {
    let database = seeded_database();
    let error = database
        .execute(
            "CREATE INDEX temporary_idx ON documents (active);
             UPDATE documents SET id = 1 WHERE id = 2;",
        )
        .unwrap_err();
    assert_eq!(error, Error::UniqueViolation("id".into()));
    assert!(database.indexes("documents").unwrap().is_empty());

    database
        .execute("CREATE INDEX documents_active_idx ON documents (active)")
        .unwrap();
    let path = snapshot_path("indexes");
    database.save(&path).unwrap();
    let restored = Database::open(&path).unwrap();
    assert_eq!(
        restored.indexes("documents").unwrap(),
        [vectors::IndexInfo {
            name: "documents_active_idx".into(),
            column: "active".into(),
        }]
    );
    let result = query(&restored, "SELECT id FROM documents WHERE active = TRUE");
    assert_eq!(result.rows_examined, 2);
    assert_eq!(result.rows.len(), 2);

    assert!(matches!(
        restored.execute("CREATE INDEX bad_vector_idx ON documents (embedding)"),
        Err(Error::Unsupported(_))
    ));
    fs::remove_file(path).unwrap();
}

#[test]
fn bounded_top_k_ordering_preserves_limit_and_offset() {
    let database = Database::new();
    database
        .execute("CREATE TABLE points (id INTEGER PRIMARY KEY, embedding VECTOR(2))")
        .unwrap();
    let values = (0..100)
        .map(|id| format!("({id}, ARRAY[{id}, 0])"))
        .collect::<Vec<_>>()
        .join(", ");
    database
        .execute(&format!("INSERT INTO points VALUES {values}"))
        .unwrap();

    let ascending = query(
        &database,
        "SELECT id FROM points
         ORDER BY squared_l2_distance(embedding, ARRAY[0, 0])
         LIMIT 5 OFFSET 3",
    );
    assert_eq!(
        ascending.rows,
        (3..8)
            .map(|id| vec![Value::Integer(id)])
            .collect::<Vec<_>>()
    );

    let descending = query(
        &database,
        "SELECT id FROM points
         ORDER BY squared_l2_distance(embedding, ARRAY[0, 0]) DESC
         LIMIT 4 OFFSET 2",
    );
    assert_eq!(
        descending.rows,
        (94..=97)
            .rev()
            .map(|id| vec![Value::Integer(id)])
            .collect::<Vec<_>>()
    );
}

#[test]
fn evaluates_scalar_and_grouped_aggregates() {
    let database = seeded_database();
    let scalar = query(
        &database,
        "SELECT COUNT(*) AS rows,
                COUNT(category) AS categorized,
                COUNT(DISTINCT category) AS categories,
                SUM(id) AS id_sum,
                AVG(rating) AS average_rating,
                MIN(title) AS first_title,
                MAX(title) AS last_title
         FROM documents",
    );
    assert_eq!(scalar.rows.len(), 1);
    assert_eq!(scalar.rows[0][0], Value::Integer(4));
    assert_eq!(scalar.rows[0][1], Value::Integer(3));
    assert_eq!(scalar.rows[0][2], Value::Integer(2));
    assert_eq!(scalar.rows[0][3], Value::Integer(10));
    let Value::Float(average) = scalar.rows[0][4] else {
        panic!("AVG should return a float");
    };
    assert!((average - (24.7 / 3.0)).abs() < 1.0e-12);
    assert_eq!(scalar.rows[0][5], Value::Text("Cooking".into()));
    assert_eq!(scalar.rows[0][6], Value::Text("Rust vectors".into()));

    let grouped = query(
        &database,
        "SELECT category, COUNT(*) AS documents, AVG(rating) AS average_rating,
                MIN(id) AS first_id, MAX(id) AS last_id
         FROM documents
         GROUP BY category
         ORDER BY documents DESC, category ASC",
    );
    assert_eq!(grouped.rows.len(), 3);
    assert_eq!(grouped.rows[0][0], Value::Text("tech".into()));
    assert_eq!(grouped.rows[0][1], Value::Integer(2));
    assert_eq!(grouped.rows[0][3], Value::Integer(1));
    assert_eq!(grouped.rows[0][4], Value::Integer(3));
    assert_eq!(grouped.rows[1][0], Value::Text("food".into()));
    assert_eq!(grouped.rows[2][0], Value::Null);
    assert_eq!(grouped.rows[2][2], Value::Null);

    let having = query(
        &database,
        "SELECT category,
                COUNT(*) AS documents,
                COUNT(*) + 1 AS weighted_documents,
                AVG(rating) * 100 AS rating_percent,
                CAST(COUNT(*) AS FLOAT) AS floating_count
         FROM documents
         GROUP BY category
         HAVING category ILIKE 'TE%'
            AND COUNT(*) > 1
            AND AVG(rating) + 1 BETWEEN 9 AND 10
         ORDER BY COUNT(*) + 1 DESC",
    );
    assert_eq!(having.rows.len(), 1);
    assert_eq!(having.rows[0][0], Value::Text("tech".into()));
    assert_eq!(having.rows[0][1], Value::Integer(2));
    assert_eq!(having.rows[0][2], Value::Integer(3));
    let Value::Float(percent) = having.rows[0][3] else {
        panic!("aggregate arithmetic should return a float");
    };
    assert!((percent - 835.0).abs() < 1.0e-10);
    assert_eq!(having.rows[0][4], Value::Float(2.0));
}

#[test]
fn aggregates_preserve_index_pruning_empty_sets_and_sql_validation() {
    let database = seeded_database();
    database
        .execute("CREATE INDEX documents_category_idx ON documents (category)")
        .unwrap();
    let indexed = query(
        &database,
        "SELECT active, COUNT(*) AS documents
         FROM documents
         WHERE category = 'tech'
         GROUP BY active
         ORDER BY COUNT(*) DESC
         LIMIT 1",
    );
    assert_eq!(indexed.rows_examined, 2);
    assert_eq!(indexed.rows.len(), 1);
    assert_eq!(indexed.rows[0][1], Value::Integer(1));

    database.execute("DELETE FROM documents").unwrap();
    let empty = query(
        &database,
        "SELECT COUNT(*) AS rows, SUM(id), AVG(rating), MIN(title), MAX(title)
         FROM documents",
    );
    assert_eq!(
        empty.rows[0],
        [
            Value::Integer(0),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ]
    );

    assert!(matches!(
        database.execute("SELECT title, COUNT(*) FROM documents"),
        Err(Error::InvalidQuery(_))
    ));
    assert!(matches!(
        database.execute("SELECT title, COUNT(*) FROM documents GROUP BY category"),
        Err(Error::InvalidQuery(_))
    ));
    assert_eq!(
        query(&database, "SELECT COUNT(*)").rows[0][0],
        Value::Integer(1)
    );
    assert!(query(
        &database,
        "SELECT COUNT(*) FROM documents HAVING COUNT(*) > 0"
    )
    .rows
    .is_empty());
    assert_eq!(
        query(
            &database,
            "SELECT COUNT(*) FROM documents HAVING COUNT(*) = 0"
        )
        .rows[0][0],
        Value::Integer(0)
    );
    assert!(matches!(
        database.execute("SELECT COUNT(*) FROM documents HAVING title = 'missing'"),
        Err(Error::InvalidQuery(_))
    ));
    assert!(matches!(
        database.execute("SELECT SUM(COUNT(*)) FROM documents"),
        Err(Error::InvalidQuery(_))
    ));
}

#[test]
fn explain_reports_index_filter_aggregate_and_top_k_stages() {
    let database = seeded_database();
    database
        .execute("CREATE INDEX documents_category_idx ON documents (category)")
        .unwrap();

    let vector_plan = query(
        &database,
        "EXPLAIN
         SELECT id, cosine_distance(embedding, ARRAY[1, 0, 0]) AS distance
         FROM documents
         WHERE category = 'tech'
         ORDER BY distance
         LIMIT 2",
    );
    assert_eq!(vector_plan.columns, ["plan"]);
    let steps = vector_plan
        .rows
        .iter()
        .map(|row| row[0].to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(steps.contains("scalar hash index on documents (2 of 4 row(s))"));
    assert!(steps.contains("Filter: category = 'tech'"));
    assert!(steps.contains("TopK:"));
    assert!(steps.contains("retain 2 row(s)"));
    assert!(steps.contains("Limit: 2"));

    let aggregate_plan = query(
        &database,
        "EXPLAIN
         SELECT category, COUNT(*) AS documents
         FROM documents
         GROUP BY category
         HAVING COUNT(*) > 1
         ORDER BY documents DESC",
    );
    let steps = aggregate_plan
        .rows
        .iter()
        .map(|row| row[0].to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(steps.contains("Aggregate: group by category"));
    assert!(steps.contains("Having: COUNT(*) > 1"));
    assert!(steps.contains("Sort: documents DESC"));

    assert!(matches!(
        database.execute("EXPLAIN ANALYZE SELECT * FROM documents"),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn insert_on_conflict_supports_idempotent_batches_and_atomic_upserts() {
    let database = Database::new();
    database
        .execute(
            "CREATE TABLE entries (
                id INTEGER PRIMARY KEY,
                slug TEXT UNIQUE,
                version INTEGER NOT NULL,
                embedding VECTOR(2)
            );
            INSERT INTO entries VALUES (1, 'one', 1, ARRAY[1, 0]);",
        )
        .unwrap();

    let results = database
        .execute(
            "INSERT INTO entries VALUES
                (1, 'duplicate-id', 1, ARRAY[1, 0]),
                (2, 'two', 1, ARRAY[0, 1]),
                (2, 'duplicate-batch-id', 1, ARRAY[0, 1])
             ON CONFLICT DO NOTHING",
        )
        .unwrap();
    assert_eq!(
        results[0],
        ExecutionResult::Command {
            tag: "INSERT",
            rows_affected: 1
        }
    );
    assert_eq!(
        query(&database, "SELECT COUNT(*) FROM entries").rows[0][0],
        Value::Integer(2)
    );

    let error = database
        .execute(
            "INSERT INTO entries VALUES (3, 'one', 1, ARRAY[1, 1])
             ON CONFLICT (id) DO NOTHING",
        )
        .unwrap_err();
    assert_eq!(error, Error::UniqueViolation("slug".into()));
    assert!(query(&database, "SELECT id FROM entries WHERE id = 3")
        .rows
        .is_empty());

    assert!(matches!(
        database.execute(
            "INSERT INTO entries VALUES (3, 'three', 1, ARRAY[1, 1])
             ON CONFLICT (embedding) DO NOTHING"
        ),
        Err(Error::InvalidQuery(_))
    ));

    let results = database
        .execute(
            "INSERT INTO entries VALUES (1, 'updated', 99, ARRAY[3, 4])
             ON CONFLICT (id) DO UPDATE SET
                slug = excluded.slug,
                version = version + 1,
                embedding = normalize(excluded.embedding)",
        )
        .unwrap();
    assert_eq!(
        results[0],
        ExecutionResult::Command {
            tag: "INSERT",
            rows_affected: 1
        }
    );
    let row = &query(
        &database,
        "SELECT slug, version, embedding FROM entries WHERE id = 1",
    )
    .rows[0];
    assert_eq!(row[0], Value::Text("updated".into()));
    assert_eq!(row[1], Value::Integer(2));
    assert_eq!(row[2], Value::Vector(Vector::new(vec![0.6, 0.8]).unwrap()));

    let results = database
        .execute(
            "INSERT INTO entries VALUES (1, 'skip', 100, ARRAY[0, 1])
             ON CONFLICT (id) DO UPDATE SET slug = excluded.slug
             WHERE excluded.slug != 'skip'",
        )
        .unwrap();
    assert_eq!(
        results[0],
        ExecutionResult::Command {
            tag: "INSERT",
            rows_affected: 0
        }
    );
    assert_eq!(
        query(&database, "SELECT slug FROM entries WHERE id = 1").rows[0][0],
        Value::Text("updated".into())
    );

    assert!(matches!(
        database.execute(
            "INSERT INTO entries VALUES
                (1, 'first-update', 1, ARRAY[1, 0]),
                (1, 'second-update', 1, ARRAY[0, 1])
             ON CONFLICT (id) DO UPDATE SET slug = excluded.slug"
        ),
        Err(Error::InvalidQuery(_))
    ));
    assert_eq!(
        query(&database, "SELECT slug FROM entries WHERE id = 1").rows[0][0],
        Value::Text("updated".into())
    );

    assert_eq!(
        database
            .execute(
                "INSERT INTO entries VALUES (1, 'two', 1, ARRAY[1, 0])
                 ON CONFLICT (id) DO UPDATE SET slug = excluded.slug"
            )
            .unwrap_err(),
        Error::UniqueViolation("slug".into())
    );
    assert_eq!(
        query(&database, "SELECT slug FROM entries WHERE id = 1").rows[0][0],
        Value::Text("updated".into())
    );

    assert!(matches!(
        database.execute(
            "INSERT INTO entries VALUES (3, 'three', 1, ARRAY[1, 1])
             ON CONFLICT DO UPDATE SET slug = 'changed'"
        ),
        Err(Error::InvalidQuery(_))
    ));
}
