//! Prepare bounded typed upserts without copying the existing table. Changing
//! the conflict key itself retains the general SQL-compatible replacement path.

use super::*;

pub(super) struct PreparedUpsert {
    updates: Vec<(usize, Vec<Value>)>,
    appended: Vec<Vec<Value>>,
}

pub(super) fn prepare(
    table: &Table,
    pending: Vec<Vec<Value>>,
    conflict_column: usize,
    update_columns: &[usize],
) -> Result<PreparedUpsert> {
    debug_assert!(!update_columns.contains(&conflict_column));
    let mut updates = Vec::new();
    let mut appended = Vec::new();
    let mut touched = HashSet::new();
    let mut inserted_keys = HashSet::new();
    for excluded in pending {
        let value = &excluded[conflict_column];
        let conflict = if matches!(value, Value::Null)
            || matches!(value, Value::Float(number) if number.is_nan())
        {
            None
        } else {
            let key = UniqueKey::from(value);
            if inserted_keys.contains(&key) {
                return Err(repeated_row());
            }
            let existing = table.unique_keys.get(&conflict_column).map_or_else(
                || {
                    table
                        .rows
                        .iter()
                        .position(|row| row[conflict_column] == *value)
                },
                |keys| keys.get(&key).copied(),
            );
            if existing.is_none() {
                inserted_keys.insert(key);
            }
            existing
        };
        if let Some(row_index) = conflict {
            if !touched.insert(row_index) {
                return Err(repeated_row());
            }
            let mut replacement = table.rows[row_index].clone();
            for &column in update_columns {
                if !same_value(&replacement[column], &excluded[column]) {
                    replacement[column] = excluded[column].clone();
                }
            }
            validate_row(&table.columns, &replacement)?;
            updates.push((row_index, replacement));
        } else {
            appended.push(excluded);
        }
    }
    // Validate the final state, including swaps of secondary unique keys. None
    // of the live rows, cached indexes or WAL have changed if validation fails.
    for (column, definition) in table.columns.iter().enumerate() {
        if !definition.unique {
            continue;
        }
        let mut staged_keys = HashSet::new();
        for row in updates.iter().map(|(_, row)| row).chain(&appended) {
            if matches!(row[column], Value::Null) {
                continue;
            }
            let key = UniqueKey::from(&row[column]);
            let conflicts = table.unique_keys.get(&column).map_or_else(
                || {
                    table.rows.iter().enumerate().any(|(index, existing)| {
                        !touched.contains(&index) && existing[column] == row[column]
                    })
                },
                |keys| keys.get(&key).is_some_and(|index| !touched.contains(index)),
            );
            if conflicts || !staged_keys.insert(key) {
                return Err(Error::UniqueViolation(definition.name.clone()));
            }
        }
    }
    ensure_table_row_capacity(table.rows.len(), appended.len())?;
    updates.sort_unstable_by_key(|(index, _)| *index);
    Ok(PreparedUpsert { updates, appended })
}

fn repeated_row() -> Error {
    Error::InvalidQuery("ON CONFLICT DO UPDATE cannot affect the same row twice".into())
}

// Equality for storage reuse is stricter than SQL equality: replacing +0 with
// -0 must retain the incoming representation in both row and dense storage.
fn same_value(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Float(left), Value::Float(right)) => left.to_bits() == right.to_bits(),
        (Value::Vector(left), Value::Vector(right)) => {
            left.dimensions() == right.dimensions()
                && (left.as_slice().as_ptr() == right.as_slice().as_ptr()
                    || left
                        .as_slice()
                        .iter()
                        .zip(right.as_slice())
                        .all(|(left, right)| left.to_bits() == right.to_bits()))
        }
        _ => left == right,
    }
}

impl PreparedUpsert {
    pub(super) fn rows_affected(&self) -> usize {
        self.updates.len() + self.appended.len()
    }

    pub(super) fn apply(self, table: &mut Table) {
        let updated_content = self.updates.iter().any(|(index, replacement)| {
            table.rows[*index]
                .iter()
                .zip(replacement)
                .any(|(old, new)| !same_value(old, new))
        });
        // Remove old unique keys first, so a valid swap cannot remove a key
        // which another updated row has just installed.
        for (column, keys) in &mut table.unique_keys {
            for (index, replacement) in &self.updates {
                let old = &table.rows[*index][*column];
                if old != &replacement[*column] && !matches!(old, Value::Null) {
                    keys.remove(&UniqueKey::from(old));
                }
            }
            for (index, replacement) in &self.updates {
                let value = &replacement[*column];
                if table.rows[*index][*column] != *value && !matches!(value, Value::Null) {
                    keys.insert(UniqueKey::from(value), *index);
                }
            }
        }
        for index in table.indexes.values_mut() {
            for (row_index, replacement) in &self.updates {
                let old = &table.rows[*row_index][index.column];
                let new = &replacement[index.column];
                if old == new {
                    continue;
                }
                if !matches!(old, Value::Null) {
                    let key = UniqueKey::from(old);
                    if let Some(bucket) = index.buckets.get_mut(&key) {
                        if let Ok(position) = bucket.binary_search(row_index) {
                            bucket.remove(position);
                        }
                        if bucket.is_empty() {
                            index.buckets.remove(&key);
                        }
                    }
                }
                if !matches!(new, Value::Null) {
                    let bucket = index.buckets.entry(UniqueKey::from(new)).or_default();
                    let position = bucket.partition_point(|index| index < row_index);
                    bucket.insert(position, *row_index);
                }
            }
        }
        // Repack each changed vector slab once, keeping untouched slabs shared
        // and preserving source-row order for exact ranking and tie breaking.
        let changed_chunks = table
            .vector_columns
            .iter()
            .map(|(&column, dense)| {
                let mut chunks = Vec::new();
                for (index, replacement) in &self.updates {
                    if !same_value(&table.rows[*index][column], &replacement[column]) {
                        let chunk = dense
                            .chunk_index(*index)
                            .expect("stored vector row has a slab");
                        if chunks.last() != Some(&chunk) {
                            chunks.push(chunk);
                        }
                    }
                }
                (column, chunks)
            })
            .collect::<Vec<_>>();
        for (index, replacement) in self.updates {
            table.rows[index] = replacement;
        }
        for (column, chunks) in changed_chunks {
            let dense = table.vector_columns.get_mut(&column).unwrap();
            for chunk_index in chunks {
                let chunk = &dense.chunks[chunk_index];
                let mut replacement = DenseVectorColumn::empty(dense.dimensions);
                replacement.row_count = chunk.first_row;
                replacement.append_rows(
                    column,
                    chunk.first_row,
                    &mut table.rows[chunk.first_row..chunk.first_row + chunk.row_count],
                );
                debug_assert_eq!(replacement.chunks.len(), 1);
                dense.chunks[chunk_index] = replacement.chunks.pop().unwrap();
            }
        }
        // GraphRAG's lexical/profile caches use the dense generation for the
        // whole row, including text and profile. Invalidate on scalar edits too,
        // without copying unchanged vector slabs.
        if updated_content {
            for dense in table.vector_columns.values_mut() {
                dense.storage_id = next_vector_storage_id();
            }
        }
        let first_new_row = table.rows.len();
        table.rows.extend(self.appended);
        extend_indexes(table, first_new_row);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updates_repack_only_changed_slabs_preserve_snapshots_and_skip_identical_vectors() {
        let db = Database::new();
        db.execute(
            "CREATE TABLE units (id INTEGER PRIMARY KEY, embedding VECTOR(1024), other VECTOR(2))",
        )
        .unwrap();
        let vector = Value::Vector(Vector::new(vec![1.; 1024]).unwrap());
        let other = Value::Vector(Vector::new(vec![1., 0.]).unwrap());
        db.insert_rows(
            "units",
            (0..5000)
                .map(|id| vec![Value::Integer(id), vector.clone(), other.clone()])
                .collect(),
            InsertConflict::Fail,
        )
        .unwrap();
        let snapshot = db.catalog.read().unwrap().clone();
        assert_eq!(snapshot.tables["units"].vector_columns[&1].chunks.len(), 3);
        let changed = Value::Vector(Vector::new(vec![2.; 1024]).unwrap());
        let conflict = InsertConflict::DoUpdate {
            target: "id".into(),
            update_columns: vec!["embedding".into(), "other".into()],
        };
        db.insert_rows(
            "units",
            vec![
                vec![Value::Integer(4097), changed.clone(), other.clone()],
                vec![Value::Integer(2), Value::Null, other.clone()],
                vec![Value::Integer(1), changed.clone(), other.clone()],
            ],
            conflict.clone(),
        )
        .unwrap();
        {
            let current = db.catalog.read().unwrap();
            let before = &snapshot.tables["units"].vector_columns[&1];
            let after = &current.tables["units"].vector_columns[&1];
            assert_ne!(before.storage_id, after.storage_id);
            for index in 0..3 {
                assert_eq!(
                    Arc::ptr_eq(&before.chunks[index].values, &after.chunks[index].values),
                    index == 1
                );
            }
            assert_eq!(after.get(1).unwrap().values, vec![2.; 1024]);
            assert!(after.get(2).is_none());
            assert_eq!(before.get(2).unwrap().values, vec![1.; 1024]);
            assert!(Arc::ptr_eq(
                &snapshot.tables["units"].vector_columns[&2].chunks[0].values,
                &current.tables["units"].vector_columns[&2].chunks[0].values
            ));
        }
        let before = db.catalog.read().unwrap().clone();
        // Independently allocated equal vectors must keep the packed row/slab
        // storage, rather than accumulate a second copy on every retry.
        db.insert_rows(
            "units",
            vec![vec![
                Value::Integer(1),
                Value::Vector(Vector::new(vec![2.; 1024]).unwrap()),
                other.clone(),
            ]],
            conflict.clone(),
        )
        .unwrap();
        {
            let after = db.catalog.read().unwrap();
            assert_eq!(
                before.tables["units"].vector_columns[&1].storage_id,
                after.tables["units"].vector_columns[&1].storage_id
            );
            let (Value::Vector(old), Value::Vector(new)) = (
                &before.tables["units"].rows[1][1],
                &after.tables["units"].rows[1][1],
            ) else {
                panic!("vectors")
            };
            assert_eq!(old.as_slice().as_ptr(), new.as_slice().as_ptr());
        }
        db.insert_rows(
            "units",
            vec![vec![Value::Integer(2), changed, Value::Null]],
            conflict,
        )
        .unwrap();
        let after = db.catalog.read().unwrap();
        assert!(after.tables["units"].vector_columns[&1].get(2).is_some());
        assert!(after.tables["units"].vector_columns[&2].get(2).is_none());
    }
}
