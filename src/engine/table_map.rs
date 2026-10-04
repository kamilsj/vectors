//! Catalog snapshots share unchanged tables. Mutable access detaches only the
//! requested table, preserving transaction rollback and snapshot isolation.

use std::collections::HashMap;
use std::ops::Index;
use std::sync::Arc;

use super::Table;

#[derive(Clone, Default, Debug)]
pub(crate) struct TableMap {
    tables: HashMap<String, Arc<Table>>,
}

impl TableMap {
    pub(crate) fn len(&self) -> usize {
        self.tables.len()
    }

    pub(crate) fn contains_key(&self, name: &str) -> bool {
        self.tables.contains_key(name)
    }

    pub(crate) fn get(&self, name: &str) -> Option<&Table> {
        self.tables.get(name).map(Arc::as_ref)
    }

    pub(crate) fn get_mut(&mut self, name: &str) -> Option<&mut Table> {
        self.tables.get_mut(name).map(Arc::make_mut)
    }

    pub(crate) fn insert(&mut self, name: String, table: Table) -> Option<Arc<Table>> {
        self.tables.insert(name, Arc::new(table))
    }

    pub(crate) fn remove(&mut self, name: &str) -> Option<Arc<Table>> {
        self.tables.remove(name)
    }

    pub(crate) fn keys(&self) -> impl ExactSizeIterator<Item = &String> {
        self.tables.keys()
    }

    pub(crate) fn values(&self) -> impl ExactSizeIterator<Item = &Table> {
        self.tables.values().map(Arc::as_ref)
    }

    pub(crate) fn iter(&self) -> impl ExactSizeIterator<Item = (&String, &Table)> {
        self.tables
            .iter()
            .map(|(name, table)| (name, table.as_ref()))
    }
}

impl Index<&str> for TableMap {
    type Output = Table;

    fn index(&self, name: &str) -> &Self::Output {
        &self.tables[name]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Catalog, Column, DataType, Database, ExecutionResult, Value};

    fn fixture() -> Database {
        let database = Database::new();
        database
            .execute(
                "CREATE TABLE items (id INTEGER PRIMARY KEY, title TEXT, embedding VECTOR(2));
                 CREATE INDEX items_title ON items (title);
                 INSERT INTO items VALUES (1, 'before', [1, 0]), (2, 'second', [0, 1]);
                 CREATE TABLE unrelated (id INTEGER PRIMARY KEY, title TEXT);
                 INSERT INTO unrelated VALUES (7, 'untouched')",
            )
            .unwrap();
        database
    }

    #[test]
    fn cloned_catalog_shares_tables_until_first_mutable_access() {
        let database = fixture();
        let original = database.catalog.read().unwrap().clone();
        let mut staged = original.clone();
        for name in original.tables.keys() {
            assert!(std::ptr::eq(
                original.tables.get(name).unwrap(),
                staged.tables.get(name).unwrap()
            ));
        }

        staged.tables.get_mut("items").unwrap().rows[0][1] = Value::Text("staged".into());
        let detached = std::ptr::from_ref(&staged.tables["items"]);
        assert!(!std::ptr::eq(&original.tables["items"], detached));
        assert!(std::ptr::eq(
            &original.tables["unrelated"],
            &staged.tables["unrelated"]
        ));
        assert_eq!(
            original.tables["items"].rows[0][1],
            Value::Text("before".into())
        );
        // Once detached, another mutable access must not copy the table again.
        assert!(std::ptr::eq(
            staged.tables.get_mut("items").unwrap(),
            detached
        ));
        assert!(staged.tables.get_mut("missing").is_none());
    }

    #[test]
    fn successful_transaction_detaches_only_changed_tables_and_indexes() {
        let database = fixture();
        let before = database.catalog.read().unwrap().clone();
        database
            .execute(
                "UPDATE items SET id = 3, title = 'after', embedding = [2, 3] WHERE id = 1;
                 DROP INDEX items_title; CREATE INDEX items_id ON items (id)",
            )
            .unwrap();
        let after = database.catalog.read().unwrap();
        assert!(std::ptr::eq(
            &before.tables["unrelated"],
            &after.tables["unrelated"]
        ));
        assert!(!std::ptr::eq(
            &before.tables["items"],
            &after.tables["items"]
        ));
        assert_eq!(before.tables["items"].rows[0][0], Value::Integer(1));
        assert_eq!(after.tables["items"].rows[0][0], Value::Integer(3));
        assert!(before.tables["items"].indexes.contains_key("items_title"));
        assert!(!before.tables["items"].indexes.contains_key("items_id"));
        assert!(!after.tables["items"].indexes.contains_key("items_title"));
        assert!(after.tables["items"].indexes.contains_key("items_id"));
        assert_eq!(
            before.tables["items"].vector_columns[&2]
                .get(0)
                .unwrap()
                .values,
            [1.0, 0.0]
        );
        assert_eq!(
            after.tables["items"].vector_columns[&2]
                .get(0)
                .unwrap()
                .values,
            [2.0, 3.0]
        );
    }

    #[test]
    fn failed_transaction_keeps_original_table_identity_data_and_revision() {
        let database = fixture();
        let before = database.catalog.read().unwrap().clone();
        assert!(database
            .execute(
                "UPDATE items SET title = 'discard', embedding = [9, 9] WHERE id = 1;
                 DROP INDEX items_title;
                 DROP TABLE unrelated;
                 CREATE TABLE transient (id INTEGER);
                 INSERT INTO items VALUES (2, 'duplicate', [5, 5])",
            )
            .is_err());
        let after = database.catalog.read().unwrap();
        assert_eq!(before.revision, after.revision);
        assert_eq!(before.durable_sequence, after.durable_sequence);
        assert_eq!(before.tables.len(), after.tables.len());
        for name in before.tables.keys() {
            assert!(std::ptr::eq(
                before.tables.get(name).unwrap(),
                after.tables.get(name).unwrap()
            ));
        }
        assert!(!after.tables.contains_key("transient"));
    }

    #[test]
    fn frozen_snapshot_remains_serializable_after_live_mutations() {
        let database = fixture();
        let before = database.catalog.read().unwrap().clone();
        database
            .execute(
                "DELETE FROM items WHERE id = 1;
                 INSERT INTO items VALUES (3, 'new', [3, 4]);
                 DROP INDEX items_title;
                 DROP TABLE unrelated",
            )
            .unwrap();
        let path = std::env::temp_dir().join(format!(
            "vectors-cow-snapshot-{}-{}.vdb",
            std::process::id(),
            crate::engine::next_vector_storage_id()
        ));
        crate::storage::save(&before, &path).unwrap();
        let restored = Database::open(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(restored.tables().unwrap(), ["items", "unrelated"]);
        let ExecutionResult::Query(result) = restored
            .execute("SELECT id, embedding FROM items WHERE title = 'before'")
            .unwrap()
            .remove(0)
        else {
            panic!("expected query result");
        };
        assert_eq!(
            result.rows,
            vec![vec![
                Value::Integer(1),
                before.tables["items"].rows[0][2].clone()
            ]]
        );
        assert_eq!(restored.indexes("items").unwrap()[0].name, "items_title");
        assert!(restored
            .execute("INSERT INTO items VALUES (1, 'duplicate', [0, 0])")
            .is_err());
    }

    #[test]
    #[ignore = "manual catalog clone microbenchmark; run with --release --ignored --nocapture"]
    fn catalog_clone_benchmark() {
        let mut catalog = Catalog::default();
        for table_index in 0..16 {
            let columns = vec![Column {
                name: "text".into(),
                data_type: DataType::Text,
                nullable: false,
                unique: false,
            }];
            let rows = (0..2_000)
                .map(|row| {
                    vec![Value::Text(format!(
                        "{table_index}:{row}:{}",
                        "x".repeat(256)
                    ))]
                })
                .collect();
            catalog.tables.insert(
                format!("table_{table_index}"),
                Table::new(columns, rows, HashMap::new()),
            );
        }
        let legacy_started = std::time::Instant::now();
        for _ in 0..30 {
            let deep_copy = catalog
                .tables
                .iter()
                .map(|(name, table)| (name.clone(), table.clone()))
                .collect::<HashMap<_, _>>();
            std::hint::black_box(deep_copy);
        }
        let legacy_us = legacy_started.elapsed().as_secs_f64() * 1_000_000.0 / 30.0;
        let shared_started = std::time::Instant::now();
        for _ in 0..30_000 {
            std::hint::black_box(catalog.clone());
        }
        let shared_us = shared_started.elapsed().as_secs_f64() * 1_000_000.0 / 30_000.0;
        println!("catalog clone: 16 tables, 32000 text rows; deep clone {legacy_us:.2} us; shared clone {shared_us:.2} us");
    }
}
