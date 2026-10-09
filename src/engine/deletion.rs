//! Plan deletions before any mutation, then retain unchanged vector slabs.

use super::*;

pub(super) fn parse(statement: Statement) -> Result<(Vec<TableWithJoins>, Option<Expr>)> {
    let Statement::Delete {
        tables,
        from,
        using,
        selection,
        returning,
        order_by,
        limit,
    } = statement
    else {
        unreachable!("DELETE dispatch checks the statement")
    };
    if !tables.is_empty()
        || using.is_some()
        || returning.is_some()
        || !order_by.is_empty()
        || limit.is_some()
    {
        return Err(Error::Unsupported(
            "multi-table DELETE, USING, RETURNING, ORDER BY, and LIMIT".into(),
        ));
    }
    Ok((from, selection))
}

pub(super) fn target(from: &[TableWithJoins]) -> Result<String> {
    if from.len() != 1 || !from[0].joins.is_empty() {
        return Err(Error::Unsupported(
            "DELETE with joins or multiple tables".into(),
        ));
    }
    table_factor_name(&from[0].relation)
}

pub(super) enum PreparedDelete {
    All(usize),
    Rows(Vec<usize>),
}

pub(super) fn prepare(table: &Table, selection: Option<&Expr>) -> Result<PreparedDelete> {
    let Some(selection) = selection else {
        return Ok(PreparedDelete::All(table.rows.len()));
    };
    // Preserve the existing empty-table expression behavior.
    if table.rows.is_empty() {
        return Ok(PreparedDelete::All(0));
    }
    if let Some(candidates) = indexed_candidate_rows(table, selection) {
        if candidates.residual.is_none() {
            return Ok(plan(table.rows.len(), candidates.rows));
        }
    }
    // Only fully index-proven predicates skip evaluation. A residual expression
    // could fail even on an excluded row; retain DELETE's atomic error semantics.
    let mut rows = Vec::new();
    for (index, row) in table.rows.iter().enumerate() {
        if evaluate(selection, &EvalContext::new(&table.columns, row))?
            .as_bool()?
            .unwrap_or(false)
        {
            rows.push(index);
        }
    }
    Ok(plan(table.rows.len(), rows))
}

fn plan(count: usize, rows: Vec<usize>) -> PreparedDelete {
    if rows.len() == count {
        PreparedDelete::All(count)
    } else {
        PreparedDelete::Rows(rows)
    }
}

impl PreparedDelete {
    pub(super) fn rows_affected(&self) -> usize {
        match self {
            Self::All(count) => *count,
            Self::Rows(rows) => rows.len(),
        }
    }

    pub(super) fn apply(self, table: &mut Table) {
        let Self::Rows(deleted) = self else {
            table.rows.clear();
            rebuild_indexes(table);
            return;
        };
        if deleted.is_empty() {
            return;
        }
        debug_assert!(deleted.windows(2).all(|pair| pair[0] < pair[1]));
        let mut deleted_rows = deleted.iter().copied().peekable();
        let mut original_index = 0;
        table.rows.retain(|_| {
            let remove = deleted_rows.peek() == Some(&original_index);
            if remove {
                deleted_rows.next();
            }
            original_index += 1;
            !remove
        });
        for (&column, dense) in &mut table.vector_columns {
            let chunks = std::mem::take(&mut dense.chunks);
            dense.chunk_lookup.clear();
            dense.row_count = 0;
            let mut cursor = 0;
            for mut chunk in chunks {
                let before = cursor;
                let end = chunk.first_row + chunk.row_count;
                while cursor < deleted.len() && deleted[cursor] < end {
                    cursor += 1;
                }
                let removed = cursor - before;
                let retained = chunk.row_count - removed;
                if retained == 0 {
                    continue;
                }
                let first_row = dense.row_count;
                if removed == 0 {
                    // Dense values use offsets within their slab, not global
                    // row IDs, so an unchanged slab can move without copying.
                    chunk.first_row = first_row;
                    dense.push_chunk(chunk);
                } else {
                    dense.append_rows(
                        column,
                        first_row,
                        &mut table.rows[first_row..first_row + retained],
                    );
                }
            }
            debug_assert_eq!(dense.row_count, table.rows.len());
        }
        // Only row positions changed. Keep existing scalar keys and allocations
        // instead of hashing and allocating every surviving value again.
        for index in table.indexes.values_mut() {
            index.buckets.retain(|_, rows| {
                rows.retain_mut(|row| remap_row(row, &deleted));
                !rows.is_empty()
            });
        }
        for keys in table.unique_keys.values_mut() {
            keys.retain(|_, row| remap_row(row, &deleted));
        }
    }
}

fn remap_row(row: &mut usize, deleted: &[usize]) -> bool {
    match deleted.binary_search(row) {
        Ok(_) => false,
        Err(removed_before) => {
            *row -= removed_before;
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Database {
        let db = Database::new();
        db.execute("CREATE TABLE units(id INTEGER PRIMARY KEY, turn_id INTEGER, embedding VECTOR(1024), other VECTOR(2)); CREATE INDEX turns ON units(turn_id)").unwrap();
        for start in (0..8192).step_by(2048) {
            db.insert_rows(
                "units",
                (start..start + 2048)
                    .map(|id| {
                        vec![
                            Value::Integer(id),
                            Value::Integer(id / 8),
                            if id % 17 == 0 {
                                Value::Null
                            } else {
                                Value::Vector(Vector::new(vec![id as f32 + 1.; 1024]).unwrap())
                            },
                            Value::Vector(Vector::new(vec![id as f32 + 1., 1.]).unwrap()),
                        ]
                    })
                    .collect(),
                InsertConflict::Fail,
            )
            .unwrap();
        }
        db
    }

    #[test]
    fn no_match_delete_preserves_table_and_vector_generations_even_in_a_transaction() {
        let db = fixture();
        let snapshot = db.catalog.read().unwrap().clone();
        for sql in [
            "DELETE FROM units WHERE turn_id=999999",
            "DELETE FROM units WHERE turn_id IN (NULL,999999); SELECT 1",
        ] {
            db.execute(sql).unwrap();
            let catalog = db.catalog.read().unwrap();
            assert_eq!(catalog.revision, snapshot.revision);
            assert!(std::ptr::eq(
                &catalog.tables["units"],
                &snapshot.tables["units"]
            ));
        }
    }

    #[test]
    fn deletes_reuse_unaffected_slabs_and_repack_partial_slabs_across_lookup_boundaries() {
        let db = fixture();
        let snapshot = db.catalog.read().unwrap().clone();
        // One complete slab and one row from a later slab. The remaining slabs
        // move to different global row positions without copying their payloads.
        db.execute("DELETE FROM units WHERE id<2048 OR id=6000")
            .unwrap();
        let catalog = db.catalog.read().unwrap();
        let table = &catalog.tables["units"];
        assert_eq!(table.rows.len(), 6143);
        for column in [2, 3] {
            let before = &snapshot.tables["units"].vector_columns[&column];
            let after = &table.vector_columns[&column];
            assert_ne!(before.storage_id, after.storage_id);
            assert_eq!(after.chunks.len(), 3);
            for (old, new, shared) in [(1, 0, true), (2, 1, false), (3, 2, true)] {
                assert_eq!(
                    Arc::ptr_eq(&before.chunks[old].values, &after.chunks[new].values),
                    shared
                );
            }
            for (row_index, row) in table.rows.iter().enumerate() {
                match &row[column] {
                    Value::Null => assert!(after.get(row_index).is_none()),
                    Value::Vector(value) => {
                        let dense = after.get(row_index).unwrap();
                        assert_eq!(dense.values, value.as_slice());
                        assert_eq!(dense.norm, value.norm());
                    }
                    _ => panic!("vector column"),
                }
            }
        }
        assert_eq!(snapshot.tables["units"].rows.len(), 8192);
        assert_eq!(table.rows[3952][0], Value::Integer(6001));
        let mut rebuilt = table.clone();
        rebuild_relational_indexes(&mut rebuilt);
        assert_eq!(table.unique_keys, rebuilt.unique_keys);
        for (name, index) in &table.indexes {
            assert_eq!(index.buckets, rebuilt.indexes[name].buckets);
        }
    }
}
