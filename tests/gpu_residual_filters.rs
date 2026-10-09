use vectors::{
    ComputeConfig, ComputeDevice, Database, Error, ExecutionResult, InsertConflict, Value, Vector,
};

fn database() -> Database {
    database_with_policy(ComputeDevice::Gpu, 1)
}

fn database_with_policy(device: ComputeDevice, threshold: usize) -> Database {
    let database = Database::new_with_compute(ComputeConfig {
        device,
        gpu_min_elements: threshold,
        // Deterministically unavailable, even on machines with a GPU.
        gpu_cache_bytes: 0,
    });
    database.execute("CREATE TABLE items(id INTEGER PRIMARY KEY, scope TEXT, embedding VECTOR(2)); CREATE INDEX item_scope ON items(scope); INSERT INTO items VALUES (1,'keep',[0,0]),(2,'keep',[1,0]),(3,'other',[0,1])").unwrap();
    database
}

#[test]
fn automatic_residual_scans_keep_cpu_results_when_gpu_is_unavailable() {
    let cpu = database_with_policy(ComputeDevice::Cpu, 1);
    for threshold in [1, usize::MAX] {
        let automatic = database_with_policy(ComputeDevice::Auto, threshold);
        for predicate in [
            "id<>1",
            "scope='keep' AND id<>1",
            "id<>1 AND id<>2 AND id<>3",
        ] {
            let sql = format!("SELECT id,cosine_distance(embedding,[1,0]) AS score FROM items WHERE {predicate} ORDER BY score LIMIT 2");
            assert_eq!(automatic.execute(&sql).unwrap(), cpu.execute(&sql).unwrap());
        }
    }
}

#[test]
fn parallel_prefilter_fallback_preserves_candidate_order_and_statistics() {
    let cpu = database_with_policy(ComputeDevice::Cpu, 1);
    let automatic = database_with_policy(ComputeDevice::Auto, 1);
    for database in [&cpu, &automatic] {
        database
            .insert_rows(
                "items",
                (10..8202)
                    .map(|id| {
                        vec![
                            Value::Integer(id),
                            Value::Text("keep".into()),
                            if id % 7 == 0 {
                                Value::Null
                            } else {
                                Value::Vector(Vector::new(vec![1.0, 0.0]).unwrap())
                            },
                        ]
                    })
                    .collect(),
                InsertConflict::Fail,
            )
            .unwrap();
    }
    for predicate in [
        "id<>1",
        "scope='keep' AND id<>1",
        "scope='keep' AND id>9000",
    ] {
        let sql = format!("SELECT id,cosine_distance(embedding,[1,0]) AS score FROM items WHERE {predicate} ORDER BY score LIMIT 50");
        assert_eq!(automatic.execute(&sql).unwrap(), cpu.execute(&sql).unwrap());
    }
}

#[test]
fn forced_gpu_residual_filters_do_not_silently_score_on_cpu() {
    let database = database();
    for predicate in ["id<>1", "scope='keep' AND id<>1"] {
        assert!(matches!(
            database.execute(&format!("SELECT id FROM items WHERE {predicate} ORDER BY cosine_distance(embedding,[1,0]) LIMIT 2")),
            Err(Error::GpuUnavailable(_))
        ), "{predicate}: forced GPU must not fall back to CPU");
        if cfg!(feature = "gpu") {
            let results = database.execute(&format!("EXPLAIN SELECT id FROM items WHERE {predicate} ORDER BY cosine_distance(embedding,[1,0]) LIMIT 2")).unwrap();
            assert!(format!("{results:?}")
                .contains("GPU exact scan required after CPU residual filtering"));
        }
    }
}

#[test]
fn forced_gpu_empty_residual_selection_needs_no_adapter() {
    let database = database();
    for predicate in [
        "id<>1 AND id<>2 AND id<>3",
        "scope='keep' AND id<>1 AND id<>2",
        "id=NULL",
    ] {
        let results = database.execute(&format!("SELECT id FROM items WHERE {predicate} ORDER BY cosine_distance(embedding,[1,0]) LIMIT 2")).unwrap();
        let ExecutionResult::Query(result) = &results[0] else {
            panic!("expected query")
        };
        assert!(result.rows.is_empty());
    }
}

#[test]
fn forced_gpu_reports_residual_evaluation_errors_before_device_initialization() {
    let database = database();
    let error = database
        .execute(
            "SELECT id FROM items WHERE 1/(id-id)>0 ORDER BY l2_distance(embedding,[1,0]) LIMIT 2",
        )
        .unwrap_err();
    assert!(!matches!(error, Error::GpuUnavailable(_)));
    assert!(error.to_string().contains("zero"));
}
