use serde_json::{json, Value as JsonValue};
use std::sync::Mutex;
use vectors::{
    Column, ComputeConfig, ComputeDevice, DataType, Database, Error, ExecutionResult,
    GraphBrowseRequest, GraphChunkInput, GraphCollectionConfig, GraphDocumentInput,
    GraphDocumentPreview, GraphEmbeddingProfile, GraphIngestRequest, GraphNeighborhoodDirection,
    GraphNeighborhoodRequest, GraphRagRequest, GraphSearchRequest, QueryResult, Value, Vector,
};

static RETRIEVAL_LOCK: Mutex<()> = Mutex::new(());
fn profile() -> GraphEmbeddingProfile {
    GraphEmbeddingProfile {
        provider: "openai".into(),
        model: "text-embedding-3-small".into(),
        dimensions: 3,
        context_format_version: 1,
    }
}
fn config() -> GraphCollectionConfig {
    GraphCollectionConfig {
        name: "fields".into(),
        profile: profile(),
        semantic_neighbors: 1,
        semantic_threshold: 0.8,
    }
}
fn column(name: &str, data_type: DataType, nullable: bool, unique: bool) -> Column {
    Column {
        name: name.into(),
        data_type,
        nullable,
        unique,
    }
}
fn columns() -> Vec<Column> {
    vec![
        column("category", DataType::Text, false, false),
        column("external_id", DataType::Integer, true, true),
        column("weight", DataType::Float, true, false),
        column("published", DataType::Boolean, true, false),
    ]
}
fn database() -> Database {
    let db = Database::new_with_compute(ComputeConfig {
        device: ComputeDevice::Cpu,
        ..ComputeConfig::default()
    });
    db.graph_create_collection_with_columns(config(), columns())
        .unwrap();
    db
}
fn document(id: &str, metadata: JsonValue) -> GraphDocumentInput {
    GraphDocumentInput {
        id: id.into(),
        title: "Title".into(),
        source: "source.txt".into(),
        text: "storage recovery".into(),
        metadata,
        chunking: json!({}),
        chunks: vec![GraphChunkInput {
            start_byte: 0,
            end_byte: 16,
            text: "storage recovery".into(),
            embedding_text: "Title: Title\n\nstorage recovery".into(),
            embedding: Vector::new(vec![1.0, 0.0, 0.0]).unwrap(),
        }],
    }
}
fn ingest(
    db: &Database,
    document: GraphDocumentInput,
) -> vectors::Result<vectors::GraphIngestResult> {
    db.graph_ingest_document(GraphIngestRequest {
        collection: "fields".into(),
        expected_revision: db.revision().unwrap(),
        expected_profile: profile(),
        document,
    })
}
fn query(db: &Database, sql: &str) -> QueryResult {
    match db.execute(sql).unwrap().pop().unwrap() {
        ExecutionResult::Query(rows) => rows,
        _ => panic!("query expected"),
    }
}
fn rag() -> GraphRagRequest {
    GraphRagRequest {
        collection: "fields".into(),
        expected_profile: profile(),
        query: Vector::new(vec![1.0, 0.0, 0.0]).unwrap(),
        query_text: "storage recovery".into(),
        candidate_limit: 10,
        seed_limit: 1,
        max_hops: 0,
        neighbor_limit: 4,
        vector_weight: 1.0,
        lexical_weight: 1.0,
    }
}

#[test]
fn scalar_columns_are_canonical_indexed_and_sql_queryable() {
    let db = database();
    let info = db.graph_collection("fields").unwrap();
    assert_eq!(
        serde_json::to_value(&info).unwrap()["document_columns"],
        json!([
            {"name":"category","data_type":"TEXT","nullable":false,"unique":false},
            {"name":"external_id","data_type":"INTEGER","nullable":true,"unique":true},
            {"name":"weight","data_type":"DOUBLE","nullable":true,"unique":false},
            {"name":"published","data_type":"BOOLEAN","nullable":true,"unique":false}
        ])
    );
    assert_eq!(&db.schema(&info.tables.documents).unwrap()[7..], columns());
    assert_eq!(db.indexes(&info.tables.documents).unwrap().len(), 5);
    ingest(&db, document("a", json!({"category":"manual","external_id":i64::MAX,"weight":2,"published":true,"free":{"tag":"kept"}}))).unwrap();
    ingest(&db, document("b", json!({"category":"news"}))).unwrap();
    let stored = query(&db, "SELECT metadata, category, external_id, weight, published FROM graph_fields_documents WHERE category = 'manual'");
    assert_eq!(stored.rows_examined, 1);
    assert_eq!(
        stored.rows,
        vec![vec![
            Value::Text(r#"{"free":{"tag":"kept"}}"#.into()),
            Value::Text("manual".into()),
            Value::Integer(i64::MAX),
            Value::Float(2.0),
            Value::Boolean(true)
        ]]
    );
    let nullable = db.graph_document("fields", "b").unwrap().unwrap();
    assert_eq!(
        nullable.metadata,
        json!({"category":"news","external_id":null,"weight":null,"published":null})
    );
    assert!(nullable.chunks_intact);
    let joined = query(&db, "SELECT d.category, c.text FROM graph_fields_documents d JOIN graph_fields_chunks c ON d.document_id = c.document_id WHERE d.external_id = 9223372036854775807");
    assert_eq!(
        joined.rows,
        vec![vec![
            Value::Text("manual".into()),
            Value::Text("storage recovery".into())
        ]]
    );
}

#[test]
fn preflight_and_atomic_ingest_reject_invalid_required_and_unique_values() {
    let db = database();
    ingest(
        &db,
        document("a", json!({"category":"one","external_id":7})),
    )
    .unwrap();
    let before = db.revision().unwrap();
    for metadata in [
        json!({}),
        json!({"category":null}),
        json!({"category":3}),
        json!({"category":"ok","external_id":1.0}),
        json!({"category":"ok","external_id":u64::MAX}),
        json!({"category":"ok","external_id":7}),
        json!({"category":"ok","weight":"3"}),
        json!({"category":"ok","published":1}),
        json!({"category":"a\u{0000}b"}),
        json!({"category":"x".repeat(65537)}),
    ] {
        let incoming = document("b", metadata);
        assert!(db
            .graph_check_ingest_capacity("fields", &GraphDocumentPreview::from(&incoming))
            .is_err());
        assert!(ingest(&db, incoming).is_err());
        assert_eq!(db.revision().unwrap(), before);
        assert_eq!(db.graph_collection("fields").unwrap().document_count, 1);
    }
    let replacement = document("a", json!({"category":"replacement","external_id":7}));
    assert!(db
        .graph_check_ingest_capacity("fields", &GraphDocumentPreview::from(&replacement))
        .is_ok());
    ingest(&db, replacement).unwrap();
    ingest(&db, document("b", json!({"category":"nullable"}))).unwrap();
    ingest(&db, document("c", json!({"category":"also nullable"}))).unwrap();
    assert!(matches!(
        db.execute("UPDATE graph_fields_documents SET external_id=7 WHERE document_id='b'"),
        Err(Error::UniqueViolation(_))
    ));
}

#[test]
fn invalid_column_declarations_leave_no_partial_collection() {
    let db = Database::new();
    for bad in [
        vec![column("title", DataType::Text, true, false)],
        vec![column("Category", DataType::Text, true, false)],
        vec![column("has space", DataType::Text, true, false)],
        vec![column("1bad", DataType::Text, true, false)],
        vec![column("bad", DataType::Vector(3), true, false)],
        vec![
            column("same", DataType::Text, true, false),
            column("same", DataType::Integer, true, false),
        ],
        (0..33)
            .map(|i| column(&format!("field{i}"), DataType::Text, true, false))
            .collect(),
    ] {
        assert!(db
            .graph_create_collection_with_columns(config(), bad)
            .is_err());
        assert!(db.tables().unwrap().is_empty());
        assert_eq!(db.revision().unwrap(), 0);
    }
    db.graph_create_collection_with_columns(
        config(),
        (0..32)
            .map(|i| column(&format!("field{i}"), DataType::Text, true, false))
            .collect(),
    )
    .unwrap();
    assert_eq!(
        db.graph_collection("fields")
            .unwrap()
            .document_columns
            .len(),
        32
    );
}

#[test]
fn direct_sql_edits_are_visible_in_every_graph_metadata_view() {
    let _guard = RETRIEVAL_LOCK.lock().unwrap();
    let db = database();
    ingest(
        &db,
        document("a", json!({"category":"old","external_id":3})),
    )
    .unwrap();
    db.execute("UPDATE graph_fields_documents SET category='sql', weight=0.75, published=true WHERE document_id='a'").unwrap();
    let expected = json!({"category":"sql","external_id":3,"weight":0.75,"published":true});
    let doc = db.graph_document("fields", "a").unwrap().unwrap();
    assert_eq!(doc.metadata, expected);
    assert!(
        doc.chunks_intact,
        "typed metadata does not alter embedding context"
    );
    let search = db
        .graph_search(GraphSearchRequest {
            collection: "fields".into(),
            expected_profile: profile(),
            query: Vector::new(vec![1.0, 0.0, 0.0]).unwrap(),
            seed_limit: 1,
            max_hops: 0,
            neighbor_limit: 4,
            max_results: 10,
        })
        .unwrap();
    assert_eq!(search.hits[0].metadata, expected);
    let browse = db
        .graph_browse(GraphBrowseRequest {
            collection: "fields".into(),
            document_id: None,
            offset: 0,
            limit: 10,
            max_edges: 10,
        })
        .unwrap();
    assert_eq!(browse.nodes[0].metadata, expected);
    let neighborhood = db
        .graph_neighborhood(GraphNeighborhoodRequest {
            collection: "fields".into(),
            chunk_id: "1:a:0".into(),
            max_hops: 0,
            neighbor_limit: 4,
            max_nodes: 10,
            max_edges: 10,
            direction: GraphNeighborhoodDirection::Both,
            kind: None,
            min_weight: 0.0,
        })
        .unwrap();
    assert_eq!(neighborhood.nodes[0].node.metadata, expected);
    assert_eq!(
        db.graph_rag_candidates(rag()).unwrap().candidates[0]
            .hit
            .metadata,
        expected
    );
    db.execute(r#"UPDATE graph_fields_documents SET metadata='{"category":"shadow","free":true}' WHERE document_id='a'"#).unwrap();
    assert_eq!(
        db.graph_document("fields", "a").unwrap().unwrap().metadata,
        json!({"category":"sql","external_id":3,"weight":0.75,"published":true,"free":true})
    );
}

#[test]
fn metadata_update_keeps_chunks_edges_and_lexical_index_and_enforces_cas() {
    let _guard = RETRIEVAL_LOCK.lock().unwrap();
    let db = database();
    ingest(
        &db,
        document("a", json!({"category":"old","external_id":1})),
    )
    .unwrap();
    ingest(
        &db,
        document("b", json!({"category":"other","external_id":2})),
    )
    .unwrap();
    let chunks = query(&db, "SELECT * FROM graph_fields_chunks ORDER BY chunk_id");
    let edges = query(&db, "SELECT * FROM graph_fields_edges ORDER BY edge_id");
    assert!(!edges.rows.is_empty());
    db.graph_rag_candidates(rag()).unwrap();
    let revision = db.revision().unwrap();
    let result = db
        .graph_update_document_metadata(
            "fields",
            "a",
            json!({"category":"new","external_id":1,"note":"free change"}),
            revision,
        )
        .unwrap();
    assert_eq!(result.chunks, 1);
    assert_eq!(result.edges_created, 0);
    assert!(result.replaced && result.revision > revision);
    assert_eq!(
        query(&db, "SELECT * FROM graph_fields_chunks ORDER BY chunk_id"),
        chunks
    );
    assert_eq!(
        query(&db, "SELECT * FROM graph_fields_edges ORDER BY edge_id"),
        edges
    );
    assert!(db.graph_rag_candidates(rag()).unwrap().lexical_cache_hit);
    assert!(
        db.graph_document("fields", "a")
            .unwrap()
            .unwrap()
            .chunks_intact
    );
    assert_eq!(
        query(
            &db,
            "SELECT document_id FROM graph_fields_documents WHERE category='new'"
        )
        .rows,
        vec![vec![Value::Text("a".into())]]
    );
    assert!(matches!(
        db.graph_update_document_metadata("fields", "a", json!({"category":"stale"}), revision),
        Err(Error::RevisionConflict { .. })
    ));
    let revision = db.revision().unwrap();
    assert!(matches!(
        db.graph_update_document_metadata(
            "fields",
            "a",
            json!({"category":"bad","external_id":2}),
            revision
        ),
        Err(Error::UniqueViolation(_))
    ));
    assert_eq!(db.revision().unwrap(), revision);
    assert_eq!(
        db.graph_document("fields", "a").unwrap().unwrap().metadata["category"],
        "new"
    );
    db.execute("UPDATE graph_fields_chunks SET embedding_text='corrupt' WHERE document_id='a'")
        .unwrap();
    let revision = db.revision().unwrap();
    assert!(db
        .graph_update_document_metadata("fields", "a", json!({"category":"cannot bless"}), revision)
        .is_err());
    assert_eq!(db.revision().unwrap(), revision);
}

#[test]
fn custom_columns_and_metadata_only_updates_survive_wal_checkpoint_and_snapshot() {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "vectors-document-fields-{}-{nonce}",
        std::process::id()
    ));
    let snapshot = path.with_extension("vdb");
    let expected = json!({"category":"persisted","external_id":null,"weight":1.25,"published":false,"free":"kept"});
    {
        let db = Database::open_persistent(&path).unwrap();
        db.graph_create_collection_with_columns(config(), columns())
            .unwrap();
        ingest(&db, document("a", json!({"category":"initial"}))).unwrap();
        db.graph_update_document_metadata("fields", "a", expected.clone(), db.revision().unwrap())
            .unwrap();
    }
    {
        let db = Database::open_persistent(&path).unwrap();
        assert_eq!(
            db.graph_document("fields", "a").unwrap().unwrap().metadata,
            expected
        );
        assert!(
            db.graph_document("fields", "a")
                .unwrap()
                .unwrap()
                .chunks_intact
        );
        assert_eq!(
            db.graph_collection("fields")
                .unwrap()
                .document_columns
                .len(),
            4
        );
        assert!(db.schema("graph_fields_documents").unwrap()[8].nullable);
        assert_eq!(db.indexes("graph_fields_documents").unwrap().len(), 5);
        db.checkpoint().unwrap();
        db.save(&snapshot).unwrap();
    }
    for db in [
        Database::open_persistent(&path).unwrap(),
        Database::open(&snapshot).unwrap(),
    ] {
        assert_eq!(
            db.graph_document("fields", "a").unwrap().unwrap().metadata,
            expected
        );
        assert_eq!(
            query(
                &db,
                "SELECT document_id FROM graph_fields_documents WHERE category='persisted'"
            )
            .rows
            .len(),
            1
        );
    }
    std::fs::remove_file(snapshot).unwrap();
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn legacy_collection_and_metadata_updates_need_no_schema_migration() {
    let db = Database::new();
    db.graph_create_collection(config()).unwrap();
    assert!(db
        .graph_collection("fields")
        .unwrap()
        .document_columns
        .is_empty());
    ingest(&db, document("a", json!({"category":"still free JSON"}))).unwrap();
    db.graph_update_document_metadata(
        "fields",
        "a",
        json!({"category":"changed","object":{"a":1}}),
        db.revision().unwrap(),
    )
    .unwrap();
    assert_eq!(db.schema("graph_fields_documents").unwrap().len(), 7);
    let document = db.graph_document("fields", "a").unwrap().unwrap();
    assert_eq!(
        document.metadata,
        json!({"category":"changed","object":{"a":1}})
    );
    assert!(document.chunks_intact);
}
