use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use vectors::{
    ComputeConfig, ComputeDevice, Database, Error, ExecutionResult, GraphChunkInput,
    GraphCollectionConfig, GraphDocumentInput, GraphEmbeddingProfile, GraphIngestRequest,
    InsertConflict, Value, Vector,
};

fn profile() -> GraphEmbeddingProfile {
    GraphEmbeddingProfile {
        provider: "openai".into(),
        model: "text-embedding-3-small".into(),
        dimensions: 3,
        context_format_version: 1,
    }
}

fn setup(db: &Database, semantic_neighbors: usize) {
    db.graph_create_collection(GraphCollectionConfig {
        name: "append".into(),
        profile: profile(),
        semantic_neighbors,
        semantic_threshold: 0.79,
    })
    .unwrap();
}

fn database() -> Database {
    Database::new_with_compute(ComputeConfig {
        device: ComputeDevice::Cpu,
        ..ComputeConfig::default()
    })
}

fn document(id: &str, embeddings: &[[f32; 3]]) -> GraphDocumentInput {
    let mut text = String::new();
    let chunks = embeddings
        .iter()
        .enumerate()
        .map(|(ordinal, embedding)| {
            let content = format!("Passage {ordinal}: source 'quoted' \\ Żółć.\n");
            let start_byte = text.len();
            text.push_str(&content);
            GraphChunkInput {
                start_byte,
                end_byte: text.len(),
                embedding_text: format!("Title: {id}\n\n{content}"),
                text: content,
                embedding: Vector::new(embedding.to_vec()).unwrap(),
            }
        })
        .collect();
    GraphDocumentInput {
        id: id.into(),
        title: format!("Title {id}"),
        source: format!("manual/{id}.md"),
        text,
        metadata: json!({"tenant":"blue","quoted":"'\\"}),
        chunking: json!({"fixture":true}),
        chunks,
    }
}

fn ingest(
    db: &Database,
    document: GraphDocumentInput,
) -> vectors::Result<vectors::GraphIngestResult> {
    db.graph_ingest_document(GraphIngestRequest {
        collection: "append".into(),
        expected_revision: db.revision().unwrap(),
        expected_profile: profile(),
        document,
    })
}

fn rows(db: &Database, table: &str) -> Vec<Vec<Value>> {
    let key = match table {
        "graph_append_documents" => "document_id",
        "graph_append_chunks" => "chunk_id",
        "graph_append_edges" => "edge_id",
        _ => panic!("unexpected table"),
    };
    match db
        .execute(&format!("SELECT * FROM {table} ORDER BY {key}"))
        .unwrap()
        .pop()
        .unwrap()
    {
        ExecutionResult::Query(result) => result.rows,
        _ => panic!("expected query rows"),
    }
}

fn snapshot(db: &Database) -> Vec<Vec<Vec<Value>>> {
    [
        "graph_append_documents",
        "graph_append_chunks",
        "graph_append_edges",
    ]
    .into_iter()
    .map(|name| rows(db, name))
    .collect()
}

struct Directory(PathBuf);

impl Directory {
    fn new() -> Self {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        Self(std::env::temp_dir().join(format!(
            "vectors-append-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        )))
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn append_and_staged_replacement_produce_identical_rows_and_semantic_edges() {
    let fresh = database();
    let staged = database();
    for db in [&fresh, &staged] {
        setup(db, 2);
        ingest(db, document("old", &[[1.0, 0.0, 0.0], [0.0, 1.0, 0.0]])).unwrap();
    }
    ingest(&staged, document("new", &[[0.0, 0.0, 1.0]])).unwrap();
    let appended = ingest(&fresh, document("new", &[[1.0, 0.0, 0.0], [0.8, 0.6, 0.0]])).unwrap();
    let replaced = ingest(
        &staged,
        document("new", &[[1.0, 0.0, 0.0], [0.8, 0.6, 0.0]]),
    )
    .unwrap();
    assert!(!appended.replaced);
    assert!(replaced.replaced);
    assert_eq!(appended.edges_created, 6); // two adjacent and four cross-document semantic edges
    assert_eq!(appended.edges_created, replaced.edges_created);
    assert_eq!(snapshot(&fresh), snapshot(&staged));
    assert_eq!(
        fresh.graph_document("append", "new").unwrap(),
        staged.graph_document("append", "new").unwrap()
    );
}

#[test]
fn late_edge_and_chunk_conflicts_leave_all_tables_revision_and_wal_unchanged() {
    let directory = Directory::new();
    let db = Database::open_persistent(&directory.0).unwrap();
    setup(&db, 0);
    ingest(&db, document("old", &[[1.0, 0.0, 0.0], [0.0, 1.0, 0.0]])).unwrap();
    // The collision is in a later table than the new document and chunks. Its
    // endpoints are unrelated, so no replacement/repair fallback is involved.
    db.execute("UPDATE graph_append_edges SET edge_id = 'adjacent:7:3:new:0:3:new:1' WHERE from_chunk = '3:old:0'").unwrap();
    let before = snapshot(&db);
    let revision = db.revision().unwrap();
    let wal = std::fs::read(directory.0.join("vectors.wal")).unwrap();
    assert!(
        matches!(ingest(&db, document("new", &[[1.0, 0.0, 0.0], [0.0, 1.0, 0.0]])), Err(Error::UniqueViolation(column)) if column == "edge_id")
    );
    assert_eq!(snapshot(&db), before);
    assert_eq!(db.revision().unwrap(), revision);
    assert_eq!(std::fs::read(directory.0.join("vectors.wal")).unwrap(), wal);

    db.execute("UPDATE graph_append_edges SET edge_id = 'unrelated' WHERE from_chunk = '3:old:0'; UPDATE graph_append_chunks SET chunk_id = '3:new:0' WHERE chunk_id = '3:old:0'").unwrap();
    let before = snapshot(&db);
    let revision = db.revision().unwrap();
    let wal = std::fs::read(directory.0.join("vectors.wal")).unwrap();
    assert!(
        matches!(ingest(&db, document("new", &[[1.0, 0.0, 0.0]])), Err(Error::UniqueViolation(column)) if column == "chunk_id")
    );
    assert_eq!(snapshot(&db), before);
    assert_eq!(db.revision().unwrap(), revision);
    assert_eq!(std::fs::read(directory.0.join("vectors.wal")).unwrap(), wal);
    drop(db);
    let reopened = Database::open_persistent(&directory.0).unwrap();
    assert_eq!(snapshot(&reopened), before);
    assert_eq!(reopened.revision().unwrap(), revision);
}

#[test]
fn absent_document_with_orphan_chunks_or_edges_still_repairs_its_namespace() {
    for keep_chunk in [false, true] {
        let db = database();
        setup(&db, 0);
        ingest(&db, document("new", &[[1.0, 0.0, 0.0], [0.0, 1.0, 0.0]])).unwrap();
        db.execute("DELETE FROM graph_append_documents WHERE document_id = 'new'")
            .unwrap();
        if !keep_chunk {
            db.execute("DELETE FROM graph_append_chunks WHERE document_id = 'new'")
                .unwrap();
        }
        let result = ingest(&db, document("new", &[[0.0, 0.0, 1.0]])).unwrap();
        assert!(!result.replaced); // no stored document row, preserving the existing contract
        assert_eq!(result.edges_created, 0);
        let info = db.graph_collection("append").unwrap();
        assert_eq!(
            (info.document_count, info.chunk_count, info.edge_count),
            (1, 1, 0)
        );
        assert_eq!(
            db.graph_document("append", "new")
                .unwrap()
                .unwrap()
                .chunk_count,
            1
        );
    }
}

#[test]
fn appended_documents_commit_once_and_survive_wal_and_checkpoint_recovery() {
    let directory = Directory::new();
    let db = Database::open_persistent(&directory.0).unwrap();
    setup(&db, 2);
    let revision = db.revision().unwrap();
    let first = ingest(&db, document("first", &[[1.0, 0.0, 0.0]])).unwrap();
    let second = ingest(&db, document("second", &[[1.0, 0.0, 0.0], [0.8, 0.6, 0.0]])).unwrap();
    assert_eq!(first.revision, revision + 1);
    assert_eq!(second.revision, revision + 2);
    let before = snapshot(&db);
    let info = db.graph_collection("append").unwrap();
    let document = db.graph_document("append", "second").unwrap();
    drop(db);
    let db = Database::open_persistent(&directory.0).unwrap();
    assert_eq!(snapshot(&db), before);
    assert_eq!(db.graph_collection("append").unwrap(), info);
    assert_eq!(db.graph_document("append", "second").unwrap(), document);
    db.checkpoint().unwrap();
    drop(db);
    let db = Database::open_persistent(&directory.0).unwrap();
    assert_eq!(snapshot(&db), before);
    assert_eq!(db.revision().unwrap(), second.revision);
}

#[test]
fn append_respects_existing_chunk_capacity_without_partial_document() {
    let db = database();
    setup(&db, 0);
    let profile_key = serde_json::to_string(&profile()).unwrap();
    let embedding = Vector::new(vec![1.0, 0.0, 0.0]).unwrap();
    db.insert_rows(
        "graph_append_chunks",
        (0..10_000)
            .map(|ordinal| {
                vec![
                    Value::Text(format!("8:existing:{ordinal}")),
                    Value::Text("existing".into()),
                    Value::Integer(ordinal),
                    Value::Integer(0),
                    Value::Integer(1),
                    Value::Text("x".into()),
                    Value::Text("x".into()),
                    Value::Text(profile_key.clone()),
                    Value::Vector(embedding.clone()),
                ]
            })
            .collect(),
        InsertConflict::Fail,
    )
    .unwrap();
    let revision = db.revision().unwrap();
    assert!(
        matches!(ingest(&db, document("new", &[[1.0, 0.0, 0.0]])), Err(Error::InvalidQuery(message)) if message.contains("10000 chunks"))
    );
    assert_eq!(db.revision().unwrap(), revision);
    let info = db.graph_collection("append").unwrap();
    assert_eq!(
        (info.document_count, info.chunk_count, info.edge_count),
        (0, 10_000, 0)
    );
}

#[test]
fn successful_append_keeps_profile_validation_correct_after_raw_sql_edit() {
    let db = database();
    setup(&db, 0);
    ingest(&db, document("old", &[[1.0, 0.0, 0.0]])).unwrap();
    db.graph_collection("append").unwrap();
    ingest(&db, document("new", &[[0.0, 1.0, 0.0]])).unwrap();
    assert_eq!(db.graph_collection("append").unwrap().chunk_count, 2);
    db.execute(
        "UPDATE graph_append_chunks SET embedding_profile = 'mismatched' WHERE document_id = 'new'",
    )
    .unwrap();
    assert!(db.graph_collection("append").is_err());
    let before = snapshot(&db);
    let revision = db.revision().unwrap();
    assert!(ingest(&db, document("third", &[[0.0, 0.0, 1.0]])).is_err());
    assert_eq!(snapshot(&db), before);
    assert_eq!(db.revision().unwrap(), revision);
}
