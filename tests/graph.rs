use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;
use vectors::{
    ComputeConfig, ComputeDevice, Database, Error, ExecutionResult, GraphChunkInput,
    GraphCollectionConfig, GraphDocumentInput, GraphEmbeddingProfile, GraphIngestRequest,
    GraphSearchRequest, Value, Vector,
};

fn database() -> Database {
    Database::new_with_compute(ComputeConfig {
        device: ComputeDevice::Cpu,
        ..ComputeConfig::default()
    })
}

fn profile() -> GraphEmbeddingProfile {
    GraphEmbeddingProfile {
        provider: "openai".into(),
        model: "text-embedding-3-small".into(),
        dimensions: 3,
        context_format_version: 1,
    }
}

fn config(name: &str) -> GraphCollectionConfig {
    GraphCollectionConfig {
        name: name.into(),
        profile: profile(),
        semantic_neighbors: 2,
        semantic_threshold: 0.8,
    }
}

fn document(id: &str, parts: &[(&str, [f32; 3])]) -> GraphDocumentInput {
    let mut text = String::new();
    let mut chunks = Vec::new();
    for (part, vector) in parts {
        let start_byte = text.len();
        text.push_str(part);
        chunks.push(GraphChunkInput {
            start_byte,
            end_byte: text.len(),
            text: (*part).into(),
            embedding_text: format!("Title: {id}\n\n{part}"),
            embedding: Vector::new(vector.to_vec()).unwrap(),
        });
    }
    GraphDocumentInput {
        id: id.into(),
        title: format!("Title {id}"),
        source: format!("https://example.test/{id}"),
        text,
        metadata: json!({"language":"en","tag":"quote' and \\ slash"}),
        chunking: json!({"version":1,"target_tokens":64}),
        chunks,
    }
}

fn ingest(db: &Database, name: &str, document: GraphDocumentInput) -> vectors::GraphIngestResult {
    db.graph_ingest_document(GraphIngestRequest {
        collection: name.into(),
        expected_revision: db.revision().unwrap(),
        expected_profile: profile(),
        document,
    })
    .unwrap()
}

fn query_request(name: &str) -> GraphSearchRequest {
    GraphSearchRequest {
        collection: name.into(),
        expected_profile: profile(),
        query: Vector::new(vec![1.0, 0.0, 0.0]).unwrap(),
        seed_limit: 1,
        max_hops: 1,
        neighbor_limit: 4,
        max_results: 10,
    }
}

fn sql_rows(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    match db.execute(sql).unwrap().pop().unwrap() {
        ExecutionResult::Query(result) => result.rows,
        _ => panic!("expected rows"),
    }
}

fn populate(db: &Database) {
    db.graph_create_collection(config("Knowledge")).unwrap();
    ingest(
        db,
        "knowledge",
        document(
            "cats",
            &[
                ("Cats nap. ", [1.0, 0.0, 0.0]),
                ("Cats purr.", [0.9, 0.1, 0.0]),
            ],
        ),
    );
    ingest(
        db,
        "knowledge",
        document("felines", &[("Felines sleep.", [1.0, 0.0, 0.0])]),
    );
    ingest(
        db,
        "knowledge",
        document("plants", &[("Plants grow.", [0.0, 0.0, 1.0])]),
    );
}

#[test]
fn graph_is_sql_visible_with_only_cross_document_semantics_and_adjacent_context() {
    let db = database();
    populate(&db);
    let info = db.graph_collection("KNOWLEDGE").unwrap();
    assert_eq!(
        (info.document_count, info.chunk_count, info.edge_count),
        (3, 4, 6)
    );
    assert_eq!(
        db.graph_collections().unwrap().as_slice(),
        std::slice::from_ref(&info)
    );
    let edges = sql_rows(
        &db,
        &format!(
            "SELECT from_chunk,to_chunk,kind,weight FROM {}",
            info.tables.edges
        ),
    );
    assert_eq!(
        edges
            .iter()
            .filter(|row| row[2] == Value::Text("adjacent".into()))
            .count(),
        2
    );
    assert_eq!(
        edges
            .iter()
            .filter(|row| row[2] == Value::Text("semantic".into()))
            .count(),
        4
    );
    assert!(edges.iter().all(
        |row| !row[0].to_string().contains("plants") && !row[1].to_string().contains("plants")
    ));
    let result = db.graph_search(query_request("knowledge")).unwrap();
    assert_eq!(result.hits.len(), 3);
    assert!(result.hits[0].seed);
    assert_eq!(result.hits[0].depth, 0);
    assert_eq!(result.hits[0].similarity, 1.0);
    assert!(result.hits[1..]
        .iter()
        .all(|hit| !hit.seed && hit.depth == 1));
    assert_eq!(result.hits[1].document_id, "cats");
    assert_eq!(result.hits[2].document_id, "felines");
    assert_eq!(result.edges.len(), 6);
    for hit in result.hits {
        let document = db
            .graph_document("knowledge", &hit.document_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            document.text.get(hit.start_byte..hit.end_byte),
            Some(hit.text.as_str())
        );
        assert_eq!(hit.metadata, document.metadata);
    }
}

#[test]
fn replacement_and_deletion_remove_all_incident_links_without_orphans() {
    let db = database();
    populate(&db);
    let replacement = ingest(
        &db,
        "knowledge",
        document("cats", &[("New unrelated topic.", [0.0, 1.0, 0.0])]),
    );
    assert!(replacement.replaced);
    assert_eq!(replacement.chunks, 1);
    assert_eq!(db.graph_collection("knowledge").unwrap().edge_count, 0);
    let removed = db
        .graph_delete_document("knowledge", "cats", replacement.revision)
        .unwrap();
    assert_eq!((removed.chunks_removed, removed.edges_removed), (1, 0));
    assert!(db.graph_document("knowledge", "cats").unwrap().is_none());
    assert_eq!(db.graph_collection("knowledge").unwrap().chunk_count, 2);
    assert!(db
        .graph_search(query_request("knowledge"))
        .unwrap()
        .hits
        .iter()
        .all(|hit| hit.document_id != "cats"));
}

#[test]
fn stale_ingest_and_delete_revisions_preserve_existing_graph() {
    let db = database();
    populate(&db);
    let expected = db.revision().unwrap();
    db.execute("CREATE TABLE unrelated (id INTEGER)").unwrap();
    let current = db.graph_collection("knowledge").unwrap();
    assert!(matches!(
        db.graph_ingest_document(GraphIngestRequest {
            collection: "knowledge".into(),
            expected_revision: expected,
            expected_profile: profile(),
            document: document("cats", &[("replace", [1.0, 0.0, 0.0])])
        }),
        Err(Error::RevisionConflict { .. })
    ));
    assert!(matches!(
        db.graph_delete_document("knowledge", "cats", expected),
        Err(Error::RevisionConflict { .. })
    ));
    assert_eq!(db.graph_collection("knowledge").unwrap(), current);
    assert_eq!(
        db.graph_document("knowledge", "cats")
            .unwrap()
            .unwrap()
            .text,
        "Cats nap. Cats purr."
    );
}

#[test]
fn invalid_embeddings_offsets_and_metadata_never_partially_replace_a_document() {
    let db = database();
    populate(&db);
    let before = db.graph_collection("knowledge").unwrap();
    for case in 0..8 {
        let mut bad = document("cats", &[("Żółć and text", [1.0, 0.0, 0.0])]);
        match case {
            0 => bad.chunks[0].start_byte = 1,
            1 => bad.chunks[0].end_byte -= 1,
            2 => bad.chunks[0].text = "wrong citation".into(),
            3 => bad.chunks[0].embedding = Vector::new(vec![1.0, 0.0]).unwrap(),
            4 => bad.chunks[0].embedding = Vector::new(vec![0.0; 3]).unwrap(),
            5 => bad.chunks[0].embedding_text.clear(),
            6 => bad.metadata = json!(["not an object"]),
            7 => bad.chunking = json!(null),
            _ => unreachable!(),
        }
        assert!(
            db.graph_ingest_document(GraphIngestRequest {
                collection: "knowledge".into(),
                expected_revision: before.revision,
                expected_profile: profile(),
                document: bad
            })
            .is_err(),
            "case {case}"
        );
        assert_eq!(db.graph_collection("knowledge").unwrap(), before);
    }
}

#[test]
fn whitespace_gaps_and_overlap_keep_exact_utf8_citations() {
    let db = database();
    db.graph_create_collection(config("text")).unwrap();
    let text = "  Żółć. \n\nDrugi akapit.  ";
    let first_start = text.find('Ż').unwrap();
    let second_start = text.find("Drugi").unwrap();
    let mut doc = document("utf8", &[(text, [1.0, 0.0, 0.0])]);
    doc.chunks = vec![
        GraphChunkInput {
            start_byte: first_start,
            end_byte: second_start - 3,
            text: text[first_start..second_start - 3].into(),
            embedding_text: "Żółć.".into(),
            embedding: Vector::new(vec![1.0, 0.0, 0.0]).unwrap(),
        },
        GraphChunkInput {
            start_byte: second_start,
            end_byte: text.len() - 2,
            text: text[second_start..text.len() - 2].into(),
            embedding_text: "Drugi akapit.".into(),
            embedding: Vector::new(vec![0.9, 0.1, 0.0]).unwrap(),
        },
    ];
    ingest(&db, "text", doc);
    let result = db.graph_search(query_request("text")).unwrap();
    assert_eq!(result.hits.len(), 2);
    for hit in result.hits {
        assert_eq!(&text[hit.start_byte..hit.end_byte], hit.text);
    }
}

#[test]
fn profile_schema_and_citation_tampering_return_errors_instead_of_mixed_results() {
    let db = database();
    populate(&db);
    let mut request = query_request("knowledge");
    request.expected_profile.model = "another-model".into();
    assert!(db.graph_search(request).is_err());
    db.execute("UPDATE graph_knowledge_config SET model='another-model'")
        .unwrap();
    assert!(db.graph_collection("knowledge").is_err());
    db.execute("UPDATE graph_knowledge_config SET model='text-embedding-3-small'; UPDATE graph_knowledge_documents SET text='tampered' WHERE document_id='cats'").unwrap();
    assert!(db.graph_search(query_request("knowledge")).is_err());
    db.execute("DROP INDEX graph_knowledge_edges_from_chunk_idx")
        .unwrap();
    assert!(db.graph_collection("knowledge").is_err());
}

#[test]
fn search_expansion_caps_and_zero_hops_are_enforced() {
    let db = database();
    populate(&db);
    let mut request = query_request("knowledge");
    request.max_results = 2;
    let result = db.graph_search(request).unwrap();
    assert_eq!(result.hits.len(), 2);
    assert!(result.truncated);
    let mut request = query_request("knowledge");
    request.max_hops = 0;
    let result = db.graph_search(request).unwrap();
    assert_eq!(result.hits.len(), 1);
    assert!(result.edges.is_empty());
    for case in 0..6 {
        let mut request = query_request("knowledge");
        match case {
            0 => request.seed_limit = 0,
            1 => request.max_hops = 4,
            2 => request.neighbor_limit = 33,
            3 => request.max_results = 101,
            4 => request.max_results = 0,
            5 => request.query = Vector::new(vec![0.0; 3]).unwrap(),
            _ => unreachable!(),
        };
        assert!(db.graph_search(request).is_err());
    }
}

#[test]
fn induced_graph_keeps_links_between_seeds_and_sql_defined_relationships() {
    let db = database();
    populate(&db);
    let mut request = query_request("knowledge");
    request.seed_limit = 3;
    request.max_hops = 0;
    let result = db.graph_search(request).unwrap();
    assert_eq!(result.hits.len(), 3);
    assert_eq!(result.edges.len(), 6);
    assert!(result.hits.iter().all(|hit| hit.seed));
    db.execute("INSERT INTO graph_knowledge_edges VALUES ('manual-link','4:cats:0','6:plants:0','references',0.7)").unwrap();
    let result = db.graph_search(query_request("knowledge")).unwrap();
    assert!(result.hits.iter().any(|hit| hit.document_id == "plants"));
    assert!(result.edges.iter().any(|edge| edge.kind == "references"));
}

#[test]
fn collection_creation_validates_names_configuration_and_rolls_back_collisions() {
    let db = database();
    for name in ["", "a-b", "1a", "x;DROP TABLE x", "żółć"] {
        assert!(db.graph_create_collection(config(name)).is_err());
    }
    for threshold in [-0.1, 1.1, f64::NAN, f64::INFINITY] {
        let mut bad = config("bad");
        bad.semantic_threshold = threshold;
        assert!(db.graph_create_collection(bad).is_err());
    }
    assert!(db.tables().unwrap().is_empty());
    db.execute("CREATE TABLE graph_collision_chunks (id INTEGER)")
        .unwrap();
    let revision = db.revision().unwrap();
    assert!(db.graph_create_collection(config("collision")).is_err());
    assert_eq!(db.revision().unwrap(), revision);
    assert_eq!(db.tables().unwrap(), ["graph_collision_chunks"]);
}

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        Self(std::env::temp_dir().join(format!(
                "vectors-graph-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
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
fn complete_graph_transactions_survive_wal_recovery_replacement_and_checkpoints() {
    let directory = Directory::new();
    let db = Database::open_persistent(&directory.0).unwrap();
    populate(&db);
    let before = db.graph_search(query_request("knowledge")).unwrap();
    let stored = db.graph_document("knowledge", "cats").unwrap();
    let info = db.graph_collection("knowledge").unwrap();
    drop(db);
    let db = Database::open_persistent(&directory.0).unwrap();
    assert_eq!(db.graph_collection("knowledge").unwrap(), info);
    assert_eq!(db.graph_document("knowledge", "cats").unwrap(), stored);
    assert_eq!(db.graph_search(query_request("knowledge")).unwrap(), before);
    let result = ingest(
        &db,
        "knowledge",
        document(
            "cats",
            &[("Changed 'quoted' \\ text\nwith Żółć", [0.0, 1.0, 0.0])],
        ),
    );
    let expected = db.graph_document("knowledge", "cats").unwrap();
    drop(db);
    let db = Database::open_persistent(&directory.0).unwrap();
    assert_eq!(db.graph_document("knowledge", "cats").unwrap(), expected);
    assert_eq!(db.revision().unwrap(), result.revision);
    db.graph_delete_document("knowledge", "cats", result.revision)
        .unwrap();
    db.checkpoint().unwrap();
    let revision = db.revision().unwrap();
    drop(db);
    let db = Database::open_persistent(&directory.0).unwrap();
    assert_eq!(db.revision().unwrap(), revision);
    assert!(db.graph_document("knowledge", "cats").unwrap().is_none());
    assert_eq!(db.graph_collection("knowledge").unwrap().edge_count, 0);
}

#[test]
fn graph_transactions_with_same_revision_allow_only_one_concurrent_writer() {
    let db = database();
    db.graph_create_collection(config("race")).unwrap();
    let revision = db.revision().unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let tasks = (0..2)
        .map(|index| {
            let db = db.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                db.graph_ingest_document(GraphIngestRequest {
                    collection: "race".into(),
                    expected_revision: revision,
                    expected_profile: profile(),
                    document: document(&format!("doc{index}"), &[("One chunk", [1.0, 0.0, 0.0])]),
                })
            })
        })
        .collect::<Vec<_>>();
    let results = tasks
        .into_iter()
        .map(|task| task.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(Error::RevisionConflict { .. })))
            .count(),
        1
    );
    assert_eq!(db.graph_collection("race").unwrap().document_count, 1);
}

#[test]
fn document_and_chunk_fingerprint_detects_context_edits_and_allows_replacement_repair() {
    let db = database();
    populate(&db);
    for sql in [
        "UPDATE graph_knowledge_chunks SET text='tampered' WHERE chunk_id='4:cats:0'",
        "UPDATE graph_knowledge_chunks SET embedding_text='old context' WHERE chunk_id='4:cats:0'",
        "UPDATE graph_knowledge_chunks SET embedding=ARRAY[0,1,0] WHERE chunk_id='4:cats:0'",
        "UPDATE graph_knowledge_documents SET title='SQL edited title' WHERE document_id='cats'",
        "UPDATE graph_knowledge_documents SET chunking='{}' WHERE document_id='cats'",
    ] {
        assert!(
            db.graph_document("knowledge", "cats")
                .unwrap()
                .unwrap()
                .chunks_intact
        );
        db.execute(sql).unwrap();
        assert!(
            !db.graph_document("knowledge", "cats")
                .unwrap()
                .unwrap()
                .chunks_intact,
            "{sql}"
        );
        ingest(
            &db,
            "knowledge",
            document(
                "cats",
                &[
                    ("Cats nap. ", [1.0, 0.0, 0.0]),
                    ("Cats purr.", [0.9, 0.1, 0.0]),
                ],
            ),
        );
        assert!(
            db.graph_document("knowledge", "cats")
                .unwrap()
                .unwrap()
                .chunks_intact
        );
    }
}

#[test]
fn repair_after_sql_chunk_deletion_removes_dangling_generated_edges() {
    let db = database();
    populate(&db);
    db.execute("DELETE FROM graph_knowledge_chunks WHERE chunk_id='4:cats:0'")
        .unwrap();
    assert!(
        !db.graph_document("knowledge", "cats")
            .unwrap()
            .unwrap()
            .chunks_intact
    );
    ingest(
        &db,
        "knowledge",
        document(
            "cats",
            &[
                ("Cats nap. ", [1.0, 0.0, 0.0]),
                ("Cats purr.", [0.9, 0.1, 0.0]),
            ],
        ),
    );
    assert!(
        db.graph_document("knowledge", "cats")
            .unwrap()
            .unwrap()
            .chunks_intact
    );
    assert_eq!(db.graph_collection("knowledge").unwrap().edge_count, 6);
    assert_eq!(
        db.graph_search(query_request("knowledge"))
            .unwrap()
            .hits
            .len(),
        3
    );
}

#[test]
fn borrowed_preflight_checks_source_and_replacement_capacity_without_mutation() {
    let db = database();
    db.graph_create_collection(config("full")).unwrap();
    let candidate = document("new", &[("new source", [1.0, 0.0, 0.0])]);
    let preview = vectors::GraphDocumentPreview::from(&candidate);
    assert_eq!(
        db.graph_check_ingest_capacity("full", &preview).unwrap(),
        db.revision().unwrap()
    );
    let mut invalid = preview.clone();
    invalid.chunks[0].end_byte -= 1;
    assert!(db.graph_check_ingest_capacity("full", &invalid).is_err());
    // Fill the SQL-visible chunk table directly so the preflight exercises the
    // collection cap without spending time generating 10000 semantic searches.
    let profile_key = serde_json::to_string(&profile()).unwrap();
    let embedding = Vector::new(vec![1.0, 0.0, 0.0]).unwrap();
    let rows = (0..10_000)
        .map(|index| {
            vec![
                Value::Text(format!("8:existing:{index}")),
                Value::Text("existing".into()),
                Value::Integer(index),
                Value::Integer(0),
                Value::Integer(1),
                Value::Text("x".into()),
                Value::Text("x".into()),
                Value::Text(profile_key.clone()),
                Value::Vector(embedding.clone()),
            ]
        })
        .collect();
    db.insert_rows("graph_full_chunks", rows, vectors::InsertConflict::Fail)
        .unwrap();
    let revision = db.revision().unwrap();
    assert!(db.graph_check_ingest_capacity("full", &preview).is_err());
    let mut replacement = preview;
    replacement.id = "existing";
    assert_eq!(
        db.graph_check_ingest_capacity("full", &replacement)
            .unwrap(),
        revision
    );
    assert_eq!(db.revision().unwrap(), revision);
    assert_eq!(db.graph_collection("full").unwrap().chunk_count, 10_000);
}
