use serde_json::json;
use vectors::{
    Database, ExecutionResult, GraphChunkInput, GraphCollectionConfig, GraphDocumentInput,
    GraphEmbeddingProfile, GraphIngestRequest, InsertConflict, Value, Vector,
};

fn populated() -> Database {
    let db = Database::new();
    let profile = GraphEmbeddingProfile {
        provider: "openai".into(),
        model: "text-embedding-3-small".into(),
        dimensions: 3,
        context_format_version: 1,
    };
    db.graph_create_collection(GraphCollectionConfig {
        name: "profiles".into(),
        profile: profile.clone(),
        semantic_neighbors: 0,
        semantic_threshold: 0.8,
    })
    .unwrap();
    db.graph_ingest_document(GraphIngestRequest {
        collection: "profiles".into(),
        expected_revision: db.revision().unwrap(),
        expected_profile: profile,
        document: GraphDocumentInput {
            id: "doc".into(),
            title: "Document".into(),
            source: "manual.pdf#page=1".into(),
            text: "text".into(),
            metadata: json!({}),
            chunking: json!({}),
            chunks: vec![GraphChunkInput {
                start_byte: 0,
                end_byte: 4,
                text: "text".into(),
                embedding_text: "text".into(),
                embedding: Vector::new(vec![1.0, 0.0, 0.0]).unwrap(),
            }],
        },
    })
    .unwrap();
    // Prime successful validation before changing the underlying SQL tables.
    db.graph_collection("profiles").unwrap();
    db
}

fn invalid_profile(db: &Database) {
    assert!(db
        .graph_collection("profiles")
        .unwrap_err()
        .to_string()
        .contains("stored chunks do not match"));
}

#[test]
fn warm_profile_checks_revalidate_scalar_edits_insertions_deletions_and_config() {
    let db = populated();
    let info = db.graph_collection("profiles").unwrap();
    let chunks = &info.tables.chunks;
    db.execute(&format!(
        "INSERT INTO \"{chunks}\" VALUES ('bad','doc',1,0,4,'text','text','wrong',ARRAY[1.0,0.0,0.0])"
    ))
    .unwrap();
    invalid_profile(&db);
    db.execute(&format!("DELETE FROM \"{chunks}\" WHERE chunk_id='bad'"))
        .unwrap();
    assert_eq!(db.graph_collection("profiles").unwrap().chunk_count, 1);

    db.execute(&format!(
        "UPDATE \"{chunks}\" SET embedding_profile='wrong'"
    ))
    .unwrap();
    invalid_profile(&db);
    invalid_profile(&db); // A failed check must never be cached as successful.
    db.execute_with_parameters(
        &format!("UPDATE \"{chunks}\" SET embedding_profile=$1"),
        &[Value::Text(
            serde_json::to_string(&info.config.profile).unwrap(),
        )],
    )
    .unwrap();
    db.graph_collection("profiles").unwrap();

    db.execute(&format!(
        "UPDATE \"{}\" SET model='another-model'",
        info.tables.config
    ))
    .unwrap();
    invalid_profile(&db);
    db.execute(&format!("DELETE FROM \"{chunks}\"")).unwrap();
    assert_eq!(db.graph_collection("profiles").unwrap().chunk_count, 0);
}

#[test]
fn cached_success_is_not_shared_with_another_database_or_reopened_snapshot() {
    let valid = populated();
    let invalid = populated();
    let chunks = invalid.graph_collection("profiles").unwrap().tables.chunks;
    invalid
        .execute(&format!(
            "UPDATE \"{chunks}\" SET embedding_profile='wrong'"
        ))
        .unwrap();
    valid.graph_collection("profiles").unwrap();
    invalid_profile(&invalid);
    let path = std::env::temp_dir().join(format!(
        "vectors-profile-generations-{}-{}.snapshot",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    invalid.save(&path).unwrap();
    invalid_profile(&Database::open(&path).unwrap());
    valid.save(&path).unwrap();
    Database::open(&path)
        .unwrap()
        .graph_collection("profiles")
        .unwrap();
    std::fs::remove_file(path).unwrap();
}

#[test]
fn typed_profile_only_upsert_invalidates_cached_validation() {
    let db = populated();
    let chunks = db.graph_collection("profiles").unwrap().tables.chunks;
    let ExecutionResult::Query(mut stored) = db
        .execute(&format!("SELECT * FROM \"{chunks}\""))
        .unwrap()
        .remove(0)
    else {
        panic!("query")
    };
    let original = stored.rows[0].clone();
    stored.rows[0][7] = Value::Text("wrong".into());
    let conflict = InsertConflict::DoUpdate {
        target: "chunk_id".into(),
        update_columns: vec!["embedding_profile".into()],
    };
    db.insert_rows(&chunks, stored.rows, conflict.clone())
        .unwrap();
    invalid_profile(&db);
    db.insert_rows(&chunks, vec![original], conflict).unwrap();
    db.graph_collection("profiles").unwrap();
}
