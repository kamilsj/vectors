use serde_json::json;
use vectors::{
    Column, ComputeConfig, ComputeDevice, DataType, Database, ExecutionResult, GraphChunkInput,
    GraphCollectionConfig, GraphDocumentInput, GraphEmbeddingProfile, GraphIngestRequest, Value,
    Vector,
};

fn profile() -> GraphEmbeddingProfile {
    GraphEmbeddingProfile {
        provider: "openai".into(),
        model: "capacity-test".into(),
        dimensions: 3,
        context_format_version: 1,
    }
}

fn database() -> Database {
    let database = Database::new_with_compute(ComputeConfig {
        device: ComputeDevice::Cpu,
        ..ComputeConfig::default()
    });
    database
        .graph_create_collection_with_columns(
            GraphCollectionConfig {
                name: "capacity".into(),
                profile: profile(),
                semantic_neighbors: 0,
                semantic_threshold: 0.8,
            },
            vec![Column {
                name: "tenant".into(),
                data_type: DataType::Text,
                nullable: false,
                unique: false,
            }],
        )
        .unwrap();
    database
}

fn ingest(database: &Database, id: &str, parts: &[&str]) {
    let mut text = String::new();
    let chunks = parts
        .iter()
        .map(|part| {
            let start_byte = text.len();
            text.push_str(part);
            GraphChunkInput {
                start_byte,
                end_byte: text.len(),
                text: (*part).into(),
                embedding_text: format!("Title: Notes\n\n{part}"),
                embedding: Vector::new(vec![1.0, 0.0, 0.0]).unwrap(),
            }
        })
        .collect();
    database
        .graph_ingest_document(GraphIngestRequest {
            collection: "capacity".into(),
            expected_revision: database.revision().unwrap(),
            expected_profile: profile(),
            document: GraphDocumentInput {
                id: id.into(),
                title: "Notes".into(),
                source: format!("{id}.pdf"),
                text,
                metadata: json!({"tenant": "Łódź", "filename": "źródło.pdf"}),
                chunking: json!({"format": 1}),
                chunks,
            },
        })
        .unwrap();
}

fn stored_sizes(database: &Database) -> (usize, usize) {
    let results = database
        .execute(
            "SELECT * FROM graph_capacity_documents; \
             SELECT * FROM graph_capacity_chunks; \
             SELECT * FROM graph_capacity_edges",
        )
        .unwrap();
    let mut text_bytes = 0;
    let mut vector_elements = 0;
    for result in results {
        let ExecutionResult::Query(query) = result else {
            panic!("expected stored rows");
        };
        for value in query.rows.into_iter().flatten() {
            match value {
                Value::Text(text) => text_bytes += text.len(),
                Value::Vector(vector) => vector_elements += vector.dimensions(),
                _ => {}
            }
        }
    }
    (text_bytes, vector_elements)
}

#[test]
fn empty_collection_capacity_reports_enforced_limits_without_writes() {
    let database = database();
    let revision = database.revision().unwrap();
    let capacity = database.graph_collection_capacity("CAPACITY").unwrap();
    assert_eq!(capacity.collection, "capacity");
    assert_eq!(capacity.revision, revision);
    assert_eq!(database.revision().unwrap(), revision);
    assert_eq!(capacity.usage.chunks, 0);
    assert_eq!(capacity.usage.edges, 0);
    assert_eq!(capacity.usage.vector_elements, 0);
    assert_eq!(capacity.usage.text_bytes, 0);
    assert_eq!(capacity.limits.chunks, 10_000);
    assert_eq!(capacity.limits.edges, 340_000);
    assert_eq!(capacity.limits.vector_elements, 32 * 1024 * 1024);
    assert_eq!(capacity.limits.text_bytes, 64 * 1024 * 1024);
    assert_eq!(capacity.limits.document_bytes, 1024 * 1024);
    assert_eq!(capacity.limits.document_chunks, 256);
    assert!(database.graph_collection_capacity("missing").is_err());
}

#[test]
fn capacity_counts_all_utf8_storage_and_tracks_sql_edits_replacement_and_deletion() {
    let database = database();
    ingest(&database, "one", &["Żółć. ", "Drugi fragment."]);
    ingest(&database, "two", &["Ta sama przestrzeń wektorowa."]);
    let initial = database.graph_collection_capacity("capacity").unwrap();
    assert_eq!(initial.usage.chunks, 3);
    // Only within-document adjacency exists when semantic neighbors are zero.
    assert_eq!(initial.usage.edges, 2);
    assert_eq!(initial.usage.vector_elements, 9);
    assert_eq!(
        (initial.usage.text_bytes, initial.usage.vector_elements),
        stored_sizes(&database)
    );

    database
        .execute("UPDATE graph_capacity_documents SET tenant='Zażółć gęślą jaźń' WHERE document_id='one'")
        .unwrap();
    let changed = database.graph_collection_capacity("capacity").unwrap();
    assert_eq!(changed.revision, database.revision().unwrap());
    assert!(changed.revision > initial.revision);
    assert_eq!(
        changed.usage.text_bytes - initial.usage.text_bytes,
        "Zażółć gęślą jaźń".len() - "Łódź".len()
    );
    assert_eq!(
        (changed.usage.text_bytes, changed.usage.vector_elements),
        stored_sizes(&database)
    );

    ingest(&database, "one", &["Replacement."]);
    let replaced = database.graph_collection_capacity("capacity").unwrap();
    assert_eq!(replaced.usage.chunks, 2);
    assert_eq!(replaced.usage.edges, 0);
    assert_eq!(replaced.usage.vector_elements, 6);
    assert_eq!(
        (replaced.usage.text_bytes, replaced.usage.vector_elements),
        stored_sizes(&database)
    );
    for id in ["one", "two"] {
        database
            .graph_delete_document("capacity", id, database.revision().unwrap())
            .unwrap();
    }
    let empty = database.graph_collection_capacity("capacity").unwrap();
    assert_eq!(empty.usage.chunks, 0);
    assert_eq!(empty.usage.edges, 0);
    assert_eq!(empty.usage.vector_elements, 0);
    assert_eq!(empty.usage.text_bytes, 0);
}
