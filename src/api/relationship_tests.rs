use super::*;
use actix_web::{http::Method, test as actix_test, App};
use serde_json::json;

fn database() -> Database {
    let db = Database::new();
    db.execute("CREATE TABLE products (id INTEGER PRIMARY KEY, name TEXT, embedding VECTOR(3)); CREATE TABLE articles (id INTEGER PRIMARY KEY, product_id INTEGER, body TEXT); INSERT INTO products VALUES (7,'Widget',ARRAY[1,0,0]); INSERT INTO articles VALUES (1,7,'Manual')").unwrap();
    db
}

fn input(revision: u64) -> CreateRelationship {
    CreateRelationship {
        name: "about_product".into(),
        source_table: "articles".into(),
        source_column: "product_id".into(),
        target_table: "products".into(),
        target_column: "id".into(),
        expected_revision: revision,
    }
}

#[test]
fn relationships_reuse_sql_columns_indexes_and_revision_checks() {
    let db = database();
    let before = db.revision().unwrap();
    let created = create_relationship(&db, input(before)).unwrap();
    assert_eq!(created.revision, db.revision().unwrap());
    assert_eq!(created.relationships[0].data_type, "INTEGER");
    assert!(db
        .indexes("articles")
        .unwrap()
        .iter()
        .any(|index| index.column == "product_id"));
    assert!(db
        .indexes("products")
        .unwrap()
        .iter()
        .any(|index| index.column == "id"));
    let listed = list_relationships(&db).unwrap();
    assert_eq!(listed.revision, created.revision);
    assert_eq!(listed.relationships.len(), 1);
    assert!(listed.relationships[0].valid);
    assert!(create_relationship(&db, input(before)).is_err());
    let current = db.revision().unwrap();
    assert!(create_relationship(&db, input(current)).is_err());
    let mut duplicate = input(current);
    duplicate.name = "same_pair".into();
    assert!(create_relationship(&db, duplicate).is_err());
    assert_eq!(db.revision().unwrap(), current);
    db.execute("DROP TABLE products").unwrap();
    let listed = list_relationships(&db).unwrap();
    assert!(!listed.relationships[0].valid);
    assert!(listed.relationships[0]
        .error
        .as_ref()
        .unwrap()
        .contains("products"));
}

#[test]
fn relationships_resolve_mixed_case_columns_and_reuse_existing_indexes() {
    let db = Database::new();
    db.execute("CREATE TABLE products (\"ProductID\" INTEGER); CREATE TABLE articles (\"ProductID\" INTEGER); CREATE INDEX product_lookup ON products (\"ProductID\"); INSERT INTO products VALUES (7); INSERT INTO articles VALUES (7)").unwrap();
    let mut definition = input(db.revision().unwrap());
    definition.source_column = "ProductID".into();
    definition.target_column = "ProductID".into();
    let created = create_relationship(&db, definition).unwrap();
    assert_eq!(created.revision, db.revision().unwrap());
    assert_eq!(db.indexes("products").unwrap().len(), 1);
    assert!(list_relationships(&db).unwrap().relationships[0].valid);
    assert!(db
        .execute("SELECT a.productid FROM articles a JOIN products p ON a.productid=p.productid")
        .is_ok());
    let mut duplicate = input(db.revision().unwrap());
    duplicate.name = "duplicate_link".into();
    duplicate.source_column = "PRODUCTID".into();
    duplicate.target_column = "productid".into();
    assert!(create_relationship(&db, duplicate).is_err());
}

#[test]
fn invalid_relationships_are_atomic_and_do_not_create_a_catalog() {
    for (target_column, name) in [
        ("name", "wrong_type"),
        ("embedding", "vector_link"),
        ("missing", "missing_field"),
        ("id", "bad name"),
    ] {
        let db = database();
        let revision = db.revision().unwrap();
        let mut definition = input(revision);
        definition.target_column = target_column.into();
        definition.name = name.into();
        assert!(create_relationship(&db, definition).is_err());
        assert_eq!(db.revision().unwrap(), revision);
        assert!(db.schema(RELATIONSHIPS_TABLE).is_err());
    }
    let db = database();
    db.execute("CREATE TABLE _vectors_relationships (name TEXT)")
        .unwrap();
    let revision = db.revision().unwrap();
    assert!(create_relationship(&db, input(revision)).is_err());
    assert_eq!(db.revision().unwrap(), revision);
}

#[test]
fn index_name_conflicts_roll_back_the_complete_relationship_transaction() {
    let db = database();
    db.execute("CREATE INDEX _vectors_relation_about_product_source ON products (name)")
        .unwrap();
    let revision = db.revision().unwrap();
    assert!(create_relationship(&db, input(revision)).is_err());
    assert!(db.schema(RELATIONSHIPS_TABLE).is_err());
    assert_eq!(db.revision().unwrap(), revision);
    assert!(db.indexes("articles").unwrap().is_empty());
}

#[test]
fn relationships_survive_wal_reopen_with_committed_revision() {
    let root = std::env::temp_dir().join(format!(
        "vectors-relationships-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let revision;
    {
        let db = Database::open_persistent(&root).unwrap();
        db.execute("CREATE TABLE products (id INTEGER PRIMARY KEY); CREATE TABLE articles (id INTEGER, product_id INTEGER)").unwrap();
        let result = create_relationship(&db, input(db.revision().unwrap())).unwrap();
        revision = result.revision;
        assert_eq!(revision, db.revision().unwrap());
    }
    {
        let db = Database::open_persistent(&root).unwrap();
        let listed = list_relationships(&db).unwrap();
        assert_eq!(listed.revision, revision);
        assert_eq!(listed.relationships.len(), 1);
        assert!(listed.relationships[0].valid);
        assert!(db
            .indexes("articles")
            .unwrap()
            .iter()
            .any(|index| index.column == "product_id"));
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[actix_web::test]
async fn api_relationships_require_auth_and_remove_only_the_definition() {
    let db = database();
    let app = actix_test::init_service(
        App::new()
            .app_data(web::Data::new(db.clone()))
            .app_data(web::Data::new(ApiSecurity::bearer_token(
                "relationship-token",
            )))
            .configure(super::super::configure),
    )
    .await;
    let denied = actix_test::call_service(
        &app,
        actix_test::TestRequest::get()
            .uri("/v1/relationships")
            .to_request(),
    )
    .await;
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    let definition = json!({"name":"about_product","source_table":"articles","source_column":"product_id","target_table":"products","target_column":"id","expected_revision":db.revision().unwrap()});
    let created: JsonValue = actix_test::call_and_read_body_json(
        &app,
        actix_test::TestRequest::post()
            .uri("/v1/relationships")
            .insert_header(("Authorization", "Bearer relationship-token"))
            .set_json(definition)
            .to_request(),
    )
    .await;
    assert_eq!(created["relationship"]["name"], "about_product");
    assert_eq!(created["revision"], db.revision().unwrap());
    // SQL-visible definitions may retain mixed case; deletion resolves the
    // same stored name without changing its data or interpreting it as SQL.
    db.execute("UPDATE _vectors_relationships SET name='About_Product' WHERE name='about_product'")
        .unwrap();
    assert!(list_relationships(&db).unwrap().relationships[0].valid);
    let deleted: JsonValue = actix_test::call_and_read_body_json(
        &app,
        actix_test::TestRequest::default()
            .method(Method::DELETE)
            .uri("/v1/relationships/about_product")
            .insert_header(("Authorization", "Bearer relationship-token"))
            .set_json(json!({"expected_revision":db.revision().unwrap()}))
            .to_request(),
    )
    .await;
    assert_eq!(deleted["name"], "About_Product");
    assert_eq!(deleted["revision"], db.revision().unwrap());
    assert!(list_relationships(&db).unwrap().relationships.is_empty());
    assert!(db.schema("products").is_ok());
    assert!(db.schema("articles").is_ok());
    assert!(!db.indexes("articles").unwrap().is_empty());
}
