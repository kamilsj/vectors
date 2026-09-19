use serde_json::json;
use vectors::{
    ComputeConfig, ComputeDevice, Database, Error, GraphBrowseRequest, GraphChunkInput,
    GraphCollectionConfig, GraphDocumentInput, GraphEmbeddingProfile, GraphIngestRequest,
    GraphNeighborhoodDirection, GraphNeighborhoodRequest, GraphNeighborhoodResult,
    GraphRelationshipRequest, Vector,
};

fn database(ids: &[&str]) -> Database {
    let db = Database::new_with_compute(ComputeConfig {
        device: ComputeDevice::Cpu,
        ..ComputeConfig::default()
    });
    let profile = GraphEmbeddingProfile {
        provider: "openai".into(),
        model: "text-embedding-3-small".into(),
        dimensions: 3,
        context_format_version: 1,
    };
    db.graph_create_collection(GraphCollectionConfig {
        name: "focus".into(),
        profile: profile.clone(),
        semantic_neighbors: 0,
        semantic_threshold: 0.8,
    })
    .unwrap();
    for id in ids {
        let text = format!("Żółć: source {id}.");
        db.graph_ingest_document(GraphIngestRequest {
            collection: "focus".into(),
            expected_revision: db.revision().unwrap(),
            expected_profile: profile.clone(),
            document: GraphDocumentInput {
                id: (*id).into(),
                title: format!("Title {id}"),
                source: format!("https://example.test/{id}"),
                metadata: json!({"id":id}),
                chunking: json!({"version":1}),
                chunks: vec![GraphChunkInput {
                    start_byte: 0,
                    end_byte: text.len(),
                    text: text.clone(),
                    embedding_text: text.clone(),
                    embedding: Vector::new(vec![1.0, 0.0, 0.0]).unwrap(),
                }],
                text,
            },
        })
        .unwrap();
    }
    db
}
fn chunk(id: &str) -> String {
    format!("{}:{id}:0", id.len())
}
fn relation(db: &Database, from: &str, to: &str, kind: &str, weight: f64) {
    db.graph_upsert_relationship(GraphRelationshipRequest {
        collection: "focus".into(),
        expected_revision: db.revision().unwrap(),
        from_chunk: chunk(from),
        to_chunk: chunk(to),
        kind: kind.into(),
        weight,
    })
    .unwrap();
}
fn request(root: &str) -> GraphNeighborhoodRequest {
    GraphNeighborhoodRequest {
        collection: "focus".into(),
        chunk_id: chunk(root),
        max_hops: 3,
        neighbor_limit: 32,
        max_nodes: 200,
        max_edges: 2000,
        direction: GraphNeighborhoodDirection::Outgoing,
        kind: None,
        min_weight: 0.0,
    }
}
fn ids(result: &GraphNeighborhoodResult) -> Vec<&str> {
    result
        .nodes
        .iter()
        .map(|node| node.node.document_id.as_str())
        .collect()
}

#[test]
fn exploration_crosses_browse_pages_and_preserves_exact_source_citations() {
    let db = database(&["a", "b", "z"]);
    relation(&db, "a", "z", "references", 0.9);
    let page = db
        .graph_browse(GraphBrowseRequest {
            collection: "focus".into(),
            document_id: None,
            offset: 0,
            limit: 2,
            max_edges: 100,
        })
        .unwrap();
    assert!(page.nodes.iter().all(|node| node.document_id != "z"));
    let result = db.graph_neighborhood(request("a")).unwrap();
    assert_eq!(ids(&result), ["a", "z"]);
    assert_eq!(result.root_chunk, chunk("a"));
    assert_eq!(result.nodes[0].depth, 0);
    assert_eq!(result.nodes[1].depth, 1);
    assert!(!result.truncated);
    for node in &result.nodes {
        let source = db
            .graph_document("focus", &node.node.document_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            source.text.get(node.node.start_byte..node.node.end_byte),
            Some(node.node.text.as_str())
        );
        assert_eq!(node.node.metadata, json!({"id":node.node.document_id}));
    }
    let json = serde_json::to_value(result).unwrap();
    assert!(json["nodes"][0].get("node").is_none());
    assert_eq!(json["nodes"][1]["chunk_id"], chunk("z"));
    assert!(json["nodes"][0].get("similarity").is_none());
}

#[test]
fn incoming_and_bidirectional_traversal_preserve_original_arrows() {
    let db = database(&["a", "b", "c", "d", "e"]);
    relation(&db, "a", "b", "references", 0.8);
    relation(&db, "b", "c", "supports", 0.9);
    relation(&db, "d", "b", "supports", 0.7);
    relation(&db, "e", "a", "references", 0.6);
    let outgoing = db.graph_neighborhood(request("b")).unwrap();
    assert_eq!(ids(&outgoing), ["b", "c"]);
    let mut query = request("b");
    query.direction = GraphNeighborhoodDirection::Incoming;
    let incoming = db.graph_neighborhood(query.clone()).unwrap();
    assert_eq!(ids(&incoming), ["b", "a", "d", "e"]);
    assert_eq!(incoming.nodes[3].depth, 2);
    assert!(incoming
        .edges
        .iter()
        .any(|edge| edge.from_chunk == chunk("a") && edge.to_chunk == chunk("b")));
    assert!(!incoming
        .edges
        .iter()
        .any(|edge| edge.from_chunk == chunk("b") && edge.to_chunk == chunk("a")));
    query.direction = GraphNeighborhoodDirection::Both;
    let both = db.graph_neighborhood(query).unwrap();
    assert_eq!(ids(&both), ["b", "c", "a", "d", "e"]);
    assert_eq!(both.edges.len(), 4);
    assert_eq!(
        serde_json::to_string(&GraphNeighborhoodDirection::Incoming).unwrap(),
        "\"incoming\""
    );
}

#[test]
fn kind_and_inclusive_weight_filters_apply_to_paths_and_returned_edges() {
    let db = database(&["a", "b", "c", "d"]);
    relation(&db, "a", "b", "references", 0.4);
    relation(&db, "a", "c", "supports", 0.9);
    relation(&db, "c", "d", "supports", 0.8);
    relation(&db, "a", "d", "references", 0.7);
    let mut query = request("a");
    query.kind = Some("supports".into());
    query.min_weight = 0.8;
    let result = db.graph_neighborhood(query.clone()).unwrap();
    assert_eq!(ids(&result), ["a", "c", "d"]);
    assert_eq!(result.nodes[2].depth, 2);
    assert!(result
        .edges
        .iter()
        .all(|edge| edge.kind == "supports" && edge.weight >= 0.8));
    assert!(!result.truncated);
    query.min_weight = 0.85;
    let result = db.graph_neighborhood(query).unwrap();
    assert_eq!(ids(&result), ["a", "c"]);
    assert!(!result.truncated);
    let mut query = request("a");
    query.kind = Some("references".into());
    query.min_weight = 0.5;
    assert_eq!(ids(&db.graph_neighborhood(query).unwrap()), ["a", "d"]);
}

#[test]
fn layers_prioritize_strongest_edges_and_all_limits_report_truncation() {
    let db = database(&["a", "b", "c", "d", "e", "z"]);
    relation(&db, "a", "c", "references", 0.7);
    relation(&db, "a", "b", "references", 0.7);
    relation(&db, "b", "d", "references", 0.2);
    relation(&db, "c", "e", "references", 0.9);
    let mut query = request("a");
    query.max_nodes = 4;
    let result = db.graph_neighborhood(query.clone()).unwrap();
    assert_eq!(ids(&result), ["a", "b", "c", "e"]);
    assert!(result.truncated);
    assert_eq!(result, db.graph_neighborhood(query).unwrap());
    let mut query = request("a");
    query.neighbor_limit = 1;
    let result = db.graph_neighborhood(query).unwrap();
    assert_eq!(ids(&result), ["a", "b", "d"]);
    assert!(result.truncated);
    let mut query = request("a");
    query.max_hops = 0;
    let result = db.graph_neighborhood(query).unwrap();
    assert_eq!(ids(&result), ["a"]);
    assert!(result.truncated);
    let mut query = request("a");
    query.max_nodes = 1;
    let result = db.graph_neighborhood(query).unwrap();
    assert_eq!(ids(&result), ["a"]);
    assert!(result.truncated);
    let mut query = request("a");
    query.max_edges = 0;
    let result = db.graph_neighborhood(query).unwrap();
    assert_eq!(result.nodes.len(), 5);
    assert!(result.edges.is_empty());
    assert!(result.truncated);
    let isolated = db.graph_neighborhood(request("z")).unwrap();
    assert_eq!(ids(&isolated), ["z"]);
    assert!(!isolated.truncated);
}

#[test]
fn induced_edges_keep_cycles_and_strongest_sql_duplicates_without_mutation() {
    let db = database(&["a", "b", "c"]);
    relation(&db, "a", "b", "supports", 0.8);
    relation(&db, "b", "a", "references", 0.9);
    relation(&db, "b", "c", "supports", 0.7);
    relation(&db, "a", "c", "references", 0.6);
    db.execute(
        "INSERT INTO graph_focus_edges VALUES ('duplicate', '1:a:0', '1:b:0', 'supports', 1.0)",
    )
    .unwrap();
    let revision = db.revision().unwrap();
    let mut query = request("a");
    query.max_hops = 1;
    let result = db.graph_neighborhood(query.clone()).unwrap();
    assert_eq!(ids(&result), ["a", "b", "c"]);
    assert_eq!(result.edges.len(), 4);
    assert_eq!(result.edges[0].from_chunk, chunk("a"));
    assert_eq!(result.edges[0].to_chunk, chunk("b"));
    assert_eq!(result.edges[0].weight, 1.0);
    assert!(result
        .edges
        .iter()
        .any(|edge| edge.from_chunk == chunk("b") && edge.to_chunk == chunk("a")));
    assert_eq!(result.revision, revision);
    assert_eq!(db.revision().unwrap(), revision);
    assert_eq!(result, db.graph_neighborhood(query.clone()).unwrap());
    query.max_edges = 1;
    let bounded = db.graph_neighborhood(query).unwrap();
    assert_eq!(bounded.edges, result.edges[..1]);
    assert!(bounded.truncated);
    // A revision read before exploration remains a valid CAS token afterward.
    db.graph_upsert_relationship(GraphRelationshipRequest {
        collection: "focus".into(),
        expected_revision: revision,
        from_chunk: chunk("c"),
        to_chunk: chunk("a"),
        kind: "supports".into(),
        weight: 0.5,
    })
    .unwrap();
}

#[test]
fn invalid_limits_filters_and_missing_roots_fail_without_mutation() {
    let db = database(&["a"]);
    let revision = db.revision().unwrap();
    let mut bad = Vec::new();
    let mut query = request("a");
    query.max_hops = 4;
    bad.push(query);
    let mut query = request("a");
    query.neighbor_limit = 0;
    bad.push(query);
    let mut query = request("a");
    query.neighbor_limit = 33;
    bad.push(query);
    let mut query = request("a");
    query.max_nodes = 0;
    bad.push(query);
    let mut query = request("a");
    query.max_nodes = 201;
    bad.push(query);
    let mut query = request("a");
    query.max_edges = 2001;
    bad.push(query);
    let mut query = request("a");
    query.min_weight = f64::NAN;
    bad.push(query);
    let mut query = request("a");
    query.min_weight = 1.1;
    bad.push(query);
    let mut query = request("a");
    query.kind = Some("Bad label".into());
    bad.push(query);
    let mut query = request("a");
    query.kind = Some("".into());
    bad.push(query);
    bad.push(request("missing"));
    for query in bad {
        assert!(db.graph_neighborhood(query).is_err());
        assert_eq!(db.revision().unwrap(), revision);
    }
}

#[test]
fn dangling_eligible_edges_fail_but_filtered_relationships_are_not_traversed() {
    let db = database(&["a", "b"]);
    relation(&db, "a", "b", "references", 0.9);
    db.execute("DELETE FROM graph_focus_chunks WHERE document_id='b'")
        .unwrap();
    let result = db.graph_neighborhood(request("a"));
    assert!(
        matches!(result, Err(Error::InvalidQuery(message)) if message.contains("missing chunk"))
    );
    let mut query = request("a");
    query.kind = Some("supports".into());
    let result = db.graph_neighborhood(query).unwrap();
    assert_eq!(ids(&result), ["a"]);
    assert!(!result.truncated);
    db.execute("DELETE FROM graph_focus_edges; INSERT INTO graph_focus_edges VALUES ('incoming', 'missing', '1:a:0', 'supports', 0.9)").unwrap();
    let mut query = request("a");
    query.direction = GraphNeighborhoodDirection::Incoming;
    assert!(
        matches!(db.graph_neighborhood(query), Err(Error::InvalidQuery(message)) if message.contains("missing chunk"))
    );
}

#[test]
fn schema_and_citation_corruption_return_errors_instead_of_inconsistent_nodes() {
    let db = database(&["a", "b"]);
    relation(&db, "a", "b", "references", 0.9);
    db.execute("UPDATE graph_focus_chunks SET start_byte=1 WHERE document_id='b'")
        .unwrap();
    assert!(
        matches!(db.graph_neighborhood(request("a")), Err(Error::InvalidQuery(message)) if message.contains("citation"))
    );
    db.execute("UPDATE graph_focus_chunks SET start_byte=0 WHERE document_id='b'; DROP INDEX graph_focus_edges_to_chunk_idx").unwrap();
    let mut query = request("a");
    query.direction = GraphNeighborhoodDirection::Both;
    assert!(
        matches!(db.graph_neighborhood(query), Err(Error::InvalidQuery(message)) if message.contains("schema"))
    );
}
