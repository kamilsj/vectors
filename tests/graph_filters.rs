use serde_json::json;
use vectors::{
    Column, ComputeConfig, ComputeDevice, DataType, Database, ExecutionResult, GraphChunkInput,
    GraphCollectionConfig, GraphDocumentInput, GraphEmbeddingProfile, GraphIngestRequest,
    GraphNeighborhoodDirection, GraphRagRequest, GraphRagSelection, GraphRagTraversal,
    GraphRelationshipRequest, Value, Vector, VectorFilterOperator, VectorSearchFilter,
};

// Keep cache-hit checks isolated from this binary's other collection fixtures.
static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn profile() -> GraphEmbeddingProfile {
    GraphEmbeddingProfile {
        provider: "openai".into(),
        model: "test".into(),
        dimensions: 2,
        context_format_version: 1,
    }
}

fn setup(db: &Database) {
    db.graph_create_collection_with_columns(
        GraphCollectionConfig {
            name: "manuals".into(),
            profile: profile(),
            semantic_neighbors: 0,
            semantic_threshold: 0.9,
        },
        vec![
            Column {
                name: "tenant".into(),
                data_type: DataType::Text,
                nullable: false,
                unique: false,
            },
            Column {
                name: "product_id".into(),
                data_type: DataType::Integer,
                nullable: true,
                unique: false,
            },
            Column {
                name: "published".into(),
                data_type: DataType::Boolean,
                nullable: false,
                unique: false,
            },
            Column {
                name: "rating".into(),
                data_type: DataType::Float,
                nullable: true,
                unique: false,
            },
        ],
    )
    .unwrap();
}

fn database() -> Database {
    let db = Database::new_with_compute(ComputeConfig {
        device: ComputeDevice::Cpu,
        ..ComputeConfig::default()
    });
    setup(&db);
    db
}

fn ingest(
    db: &Database,
    id: &str,
    tenant: &str,
    product: Option<i64>,
    text: &str,
    embedding: [f32; 2],
) {
    db.graph_ingest_document(GraphIngestRequest {
        collection: "manuals".into(),
        expected_revision: db.revision().unwrap(),
        expected_profile: profile(),
        document: GraphDocumentInput {
            id: id.into(),
            title: id.into(),
            source: format!("{id}.md"),
            text: text.into(),
            metadata: json!({"tenant":tenant,"product_id":product,"published":true,"rating":product.map(|value| value as f64 / 2.0)}),
            chunking: json!({}),
            chunks: vec![GraphChunkInput {
                start_byte: 0,
                end_byte: text.len(),
                text: text.into(),
                embedding_text: text.into(),
                embedding: Vector::new(embedding.to_vec()).unwrap(),
            }],
        },
    })
    .unwrap();
}

fn request() -> GraphRagRequest {
    GraphRagRequest {
        collection: "manuals".into(),
        expected_profile: profile(),
        query: Vector::new(vec![1.0, 0.0]).unwrap(),
        query_text: "needle".into(),
        candidate_limit: 10,
        seed_limit: 1,
        max_hops: 0,
        neighbor_limit: 4,
        vector_weight: 1.0,
        lexical_weight: 1.0,
    }
}

fn filter(column: &str, operator: VectorFilterOperator, value: Value) -> VectorSearchFilter {
    VectorSearchFilter {
        column: column.into(),
        operator,
        value,
    }
}

fn tenant(value: &str) -> Vec<VectorSearchFilter> {
    vec![filter(
        "tenant",
        VectorFilterOperator::Eq,
        Value::Text(value.into()),
    )]
}

fn selection() -> GraphRagSelection {
    GraphRagSelection {
        limit: 10,
        diversity: 0.0,
        max_context_bytes: 24_000,
        max_per_document: 10,
    }
}

#[test]
fn eligibility_precedes_each_ranker_and_preserves_the_unfiltered_path() {
    let _guard = TEST_LOCK.lock().unwrap();
    let db = database();
    ingest(
        &db,
        "forbidden",
        "other",
        Some(1),
        "needle needle needle",
        [1.0, 0.0],
    );
    ingest(
        &db,
        "allowed",
        "acme",
        Some(7),
        "needle with some extra context",
        [0.8, 0.2],
    );
    for (vector_weight, lexical_weight) in [(1.0, 0.0), (0.0, 1.0), (1.0, 1.0)] {
        let mut request = request();
        request.vector_weight = vector_weight;
        request.lexical_weight = lexical_weight;
        request.candidate_limit = 1;
        let result = db
            .graph_rag_candidates_filtered(request, GraphRagTraversal::default(), tenant("acme"))
            .unwrap();
        assert_eq!(result.candidates.len(), 1);
        assert_eq!(result.candidates[0].hit.document_id, "allowed");
    }
    let ordinary = db
        .graph_rag_candidates(request())
        .unwrap()
        .finalize(selection(), None)
        .unwrap();
    let no_filter = db
        .graph_rag_candidates_filtered(request(), GraphRagTraversal::default(), vec![])
        .unwrap()
        .finalize(selection(), None)
        .unwrap();
    assert_eq!(ordinary.hits, no_filter.hits);
    // An all-eligible scalar filter retains contiguous vector scan ordering.
    let all = db
        .graph_rag_candidates_filtered(
            request(),
            GraphRagTraversal::default(),
            vec![filter(
                "published",
                VectorFilterOperator::Eq,
                Value::Boolean(true),
            )],
        )
        .unwrap()
        .finalize(selection(), None)
        .unwrap();
    assert_eq!(ordinary.hits, all.hits);
    let none = db
        .graph_rag_candidates_filtered(request(), GraphRagTraversal::default(), tenant("absent"))
        .unwrap();
    assert!(none.candidates.is_empty());
    assert!(none.edges.is_empty());
}

#[test]
fn excluded_documents_cannot_be_bridges_or_consume_neighbor_slots() {
    let _guard = TEST_LOCK.lock().unwrap();
    let db = database();
    ingest(&db, "seed", "acme", Some(7), "needle", [1.0, 0.0]);
    ingest(&db, "bridge", "other", Some(8), "bridge", [1.0, 0.0]);
    ingest(&db, "beyond", "acme", Some(7), "remote context", [0.9, 0.1]);
    ingest(
        &db,
        "neighbor",
        "acme",
        Some(7),
        "direct context",
        [0.8, 0.2],
    );
    let rows = match db
        .execute("SELECT document_id, chunk_id FROM graph_manuals_chunks")
        .unwrap()
        .pop()
        .unwrap()
    {
        ExecutionResult::Query(result) => result.rows,
        _ => panic!("query expected"),
    };
    let id = |document: &str| -> String {
        match &rows
            .iter()
            .find(|row| row[0] == Value::Text(document.into()))
            .unwrap()[1]
        {
            Value::Text(value) => value.clone(),
            _ => panic!("text expected"),
        }
    };
    for (from, to, weight) in [
        ("seed", "bridge", 1.0),
        ("bridge", "beyond", 1.0),
        ("seed", "neighbor", 0.8),
        ("bridge", "seed", 1.0),
        ("beyond", "bridge", 1.0),
        ("neighbor", "seed", 0.8),
    ] {
        db.graph_upsert_relationship(GraphRelationshipRequest {
            collection: "manuals".into(),
            expected_revision: db.revision().unwrap(),
            from_chunk: id(from),
            to_chunk: id(to),
            kind: "supports".into(),
            weight,
        })
        .unwrap();
    }
    for direction in [
        GraphNeighborhoodDirection::Outgoing,
        GraphNeighborhoodDirection::Incoming,
        GraphNeighborhoodDirection::Both,
    ] {
        let mut request = request();
        request.vector_weight = 0.0;
        request.max_hops = 2;
        request.neighbor_limit = 1;
        let result = db
            .graph_rag_candidates_filtered(
                request,
                GraphRagTraversal {
                    direction,
                    kind: Some("supports".into()),
                    min_weight: 0.0,
                },
                tenant("acme"),
            )
            .unwrap();
        let mut documents = result
            .candidates
            .iter()
            .map(|candidate| candidate.hit.document_id.as_str())
            .collect::<Vec<_>>();
        documents.sort_unstable();
        assert_eq!(documents, ["neighbor", "seed"]);
        assert!(result
            .edges
            .iter()
            .all(|edge| edge.from_chunk != id("bridge") && edge.to_chunk != id("bridge")));
        for candidate in result.candidates {
            assert_eq!(candidate.hit.metadata["tenant"], "acme");
            if let Some(path) = candidate.retrieval_path {
                assert!(path
                    .edges
                    .iter()
                    .all(|edge| edge.from_chunk != id("bridge") && edge.to_chunk != id("bridge")));
            }
        }
    }
}

#[test]
fn filters_rebind_after_sql_updates_without_poisoning_the_lexical_cache_or_snapshots() {
    let _guard = TEST_LOCK.lock().unwrap();
    let db = database();
    ingest(&db, "a", "first", Some(7), "needle", [1.0, 0.0]);
    ingest(&db, "b", "second", None, "needle", [0.8, 0.2]);
    let first = db
        .graph_rag_candidates_filtered(request(), GraphRagTraversal::default(), tenant("first"))
        .unwrap();
    assert_eq!(first.candidates.len(), 1);
    let second = db
        .graph_rag_candidates_filtered(request(), GraphRagTraversal::default(), tenant("second"))
        .unwrap();
    assert!(second.lexical_cache_hit);
    assert_eq!(second.candidates[0].hit.document_id, "b");
    db.execute("UPDATE graph_manuals_documents SET tenant='second' WHERE document_id='a'")
        .unwrap();
    let changed = db
        .graph_rag_candidates_filtered(request(), GraphRagTraversal::default(), tenant("second"))
        .unwrap();
    assert!(changed.lexical_cache_hit);
    assert_eq!(changed.candidates.len(), 2);
    assert!(changed.revision > first.revision);
    let old = first.finalize(selection(), None).unwrap();
    assert_eq!(old.hits[0].hit.metadata["tenant"], "first");
    let nulls = db
        .graph_rag_candidates_filtered(
            request(),
            GraphRagTraversal::default(),
            vec![filter("product_id", VectorFilterOperator::Eq, Value::Null)],
        )
        .unwrap();
    assert_eq!(nulls.candidates.len(), 1);
    assert_eq!(nulls.candidates[0].hit.document_id, "b");
    let mut predicates = tenant("second");
    predicates.push(filter(
        "product_id",
        VectorFilterOperator::Gte,
        Value::Integer(7),
    ));
    predicates.push(filter(
        "published",
        VectorFilterOperator::Eq,
        Value::Boolean(true),
    ));
    let conjunction = db
        .graph_rag_candidates_filtered(request(), GraphRagTraversal::default(), predicates)
        .unwrap();
    assert_eq!(conjunction.candidates.len(), 1);
    assert_eq!(conjunction.candidates[0].hit.document_id, "a");
}

#[test]
fn empty_filtered_results_do_not_build_or_evict_the_lexical_cache() {
    let _guard = TEST_LOCK.lock().unwrap();
    let db = database();
    ingest(&db, "a", "acme", Some(7), "needle", [1.0, 0.0]);
    let revision = db.revision().unwrap();
    // Cover an indexed equality miss and a range predicate requiring a scan.
    for predicates in [
        tenant("absent"),
        vec![filter(
            "product_id",
            VectorFilterOperator::Lt,
            Value::Integer(0),
        )],
    ] {
        let empty = db
            .graph_rag_candidates_filtered(request(), GraphRagTraversal::default(), predicates)
            .unwrap();
        assert_eq!(empty.revision, revision);
        assert!(empty.candidates.is_empty() && empty.edges.is_empty());
        assert!(!empty.lexical_cache_hit && !empty.truncated);
        let finalized = empty.finalize(selection(), None).unwrap();
        assert!(finalized.hits.is_empty() && finalized.edges.is_empty());
        assert_eq!(finalized.context_bytes, 0);
    }
    let first = db.graph_rag_candidates(request()).unwrap();
    assert!(!first.lexical_cache_hit);
    db.graph_rag_candidates_filtered(request(), GraphRagTraversal::default(), tenant("absent"))
        .unwrap();
    let warm = db.graph_rag_candidates(request()).unwrap();
    assert!(warm.lexical_cache_hit);
    assert_eq!(first.candidates.len(), warm.candidates.len());
    // An empty selection must not hide an invalid query vector or predicate.
    let mut invalid = request();
    invalid.query = Vector::new(vec![1.0]).unwrap();
    assert!(db
        .graph_rag_candidates_filtered(invalid, GraphRagTraversal::default(), tenant("absent"))
        .is_err());
    let mut predicates = tenant("absent");
    predicates.push(filter("missing", VectorFilterOperator::Eq, Value::Null));
    assert!(db
        .graph_rag_candidates_filtered(request(), GraphRagTraversal::default(), predicates)
        .is_err());
}

#[test]
fn scalar_filter_operators_match_sql_null_and_comparison_semantics() {
    let _guard = TEST_LOCK.lock().unwrap();
    let db = database();
    for (id, product) in [
        ("negative", Some(-2)),
        ("seven", Some(7)),
        ("missing", None),
    ] {
        ingest(&db, id, "acme", product, "needle", [1.0, 0.0]);
    }
    db.execute("UPDATE graph_manuals_documents SET published=false WHERE document_id='negative'")
        .unwrap();
    for (column, operator, value, sql_predicate) in [
        (
            "product_id",
            VectorFilterOperator::Eq,
            Value::Integer(7),
            "product_id = 7",
        ),
        (
            "product_id",
            VectorFilterOperator::Ne,
            Value::Integer(7),
            "product_id <> 7",
        ),
        (
            "product_id",
            VectorFilterOperator::Gt,
            Value::Integer(-2),
            "product_id > -2",
        ),
        (
            "product_id",
            VectorFilterOperator::Gte,
            Value::Integer(7),
            "product_id >= 7",
        ),
        (
            "product_id",
            VectorFilterOperator::Lt,
            Value::Integer(7),
            "product_id < 7",
        ),
        (
            "product_id",
            VectorFilterOperator::Lte,
            Value::Integer(-2),
            "product_id <= -2",
        ),
        (
            "product_id",
            VectorFilterOperator::Eq,
            Value::Null,
            "product_id IS NULL",
        ),
        (
            "product_id",
            VectorFilterOperator::Ne,
            Value::Null,
            "product_id IS NOT NULL",
        ),
        (
            "published",
            VectorFilterOperator::Eq,
            Value::Boolean(false),
            "published = false",
        ),
        (
            "rating",
            VectorFilterOperator::Eq,
            Value::Float(3.5),
            "rating = 3.5",
        ),
        (
            "rating",
            VectorFilterOperator::Eq,
            Value::Integer(-1),
            "rating = -1",
        ),
        (
            "rating",
            VectorFilterOperator::Lte,
            Value::Float(-0.0),
            "rating <= -0.0",
        ),
        (
            "document_id",
            VectorFilterOperator::Lt,
            Value::Text("seven".into()),
            "document_id < 'seven'",
        ),
    ] {
        let mut rows = db.execute(&format!("SELECT document_id FROM graph_manuals_documents WHERE {sql_predicate} ORDER BY document_id")).unwrap();
        let ExecutionResult::Query(expected) = rows.remove(0) else {
            panic!("query expected")
        };
        let filtered = db
            .graph_rag_candidates_filtered(
                request(),
                GraphRagTraversal::default(),
                vec![filter(column, operator, value)],
            )
            .unwrap();
        let mut actual = filtered
            .candidates
            .into_iter()
            .map(|candidate| vec![Value::Text(candidate.hit.document_id)])
            .collect::<Vec<_>>();
        actual.sort_by(|left, right| match (&left[0], &right[0]) {
            (Value::Text(left), Value::Text(right)) => left.cmp(right),
            _ => unreachable!(),
        });
        assert_eq!(actual, expected.rows, "{sql_predicate}");
    }
}

#[test]
fn invalid_predicates_are_checked_even_without_any_rows() {
    let _guard = TEST_LOCK.lock().unwrap();
    let db = database();
    let bad = [
        vec![filter(
            "unknown",
            VectorFilterOperator::Eq,
            Value::Integer(1),
        )],
        vec![filter(
            "product_id",
            VectorFilterOperator::Eq,
            Value::Text("7".into()),
        )],
        vec![filter("product_id", VectorFilterOperator::Gt, Value::Null)],
        vec![filter(
            "tenant",
            VectorFilterOperator::Eq,
            Value::Text("bad\0value".into()),
        )],
        vec![filter(
            "tenant",
            VectorFilterOperator::Eq,
            Value::Text("x".repeat(65537)),
        )],
        vec![filter(
            "product_id",
            VectorFilterOperator::Eq,
            Value::Float(f64::NAN),
        )],
        vec![filter(
            "rating",
            VectorFilterOperator::Eq,
            Value::Float(f64::INFINITY),
        )],
        vec![filter("published", VectorFilterOperator::Eq, Value::Boolean(true)); 33],
    ];
    for predicates in bad {
        assert!(db
            .graph_validate_document_filters("manuals", &predicates)
            .is_err());
        assert!(db
            .graph_rag_candidates_filtered(request(), GraphRagTraversal::default(), predicates)
            .is_err());
    }
}

#[test]
fn linked_vector_queries_and_document_filters_survive_wal_checkpoint_and_snapshot() {
    let _guard = TEST_LOCK.lock().unwrap();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "vectors-filtered-relational-{}-{nonce}",
        std::process::id()
    ));
    let snapshot = directory.with_extension("vdb");
    let check = |db: &Database| {
        let filtered = db
            .graph_rag_candidates_filtered(request(), GraphRagTraversal::default(), tenant("acme"))
            .unwrap();
        assert_eq!(filtered.candidates.len(), 1);
        assert_eq!(filtered.candidates[0].hit.document_id, "manual");
        let result = db.execute("SELECT c.text, p.name, c.embedding <=> [1,0] AS distance FROM graph_manuals_chunks c JOIN graph_manuals_documents d ON c.document_id=d.document_id JOIN products p ON d.product_id=p.id WHERE d.tenant='acme' ORDER BY distance LIMIT 3").unwrap();
        let ExecutionResult::Query(rows) = &result[0] else {
            panic!("query expected")
        };
        assert_eq!(
            rows.rows,
            vec![vec![
                Value::Text("needle manual".into()),
                Value::Text("Widget".into()),
                Value::Float(0.0)
            ]]
        );
    };
    {
        let db = Database::open_persistent(&directory).unwrap();
        setup(&db);
        db.execute("CREATE TABLE products (id INTEGER PRIMARY KEY, name TEXT); INSERT INTO products VALUES (7, 'Widget'); CREATE INDEX products_id ON products USING HASH (id)").unwrap();
        ingest(&db, "manual", "acme", Some(7), "needle manual", [1.0, 0.0]);
        ingest(
            &db,
            "other",
            "private",
            Some(7),
            "needle private",
            [1.0, 0.0],
        );
        check(&db);
    }
    {
        let db = Database::open_persistent(&directory).unwrap();
        check(&db);
        db.checkpoint().unwrap();
        db.save(&snapshot).unwrap();
    }
    for db in [
        Database::open_persistent(&directory).unwrap(),
        Database::open(&snapshot).unwrap(),
    ] {
        check(&db);
    }
    std::fs::remove_file(snapshot).unwrap();
    std::fs::remove_dir_all(directory).unwrap();
}
