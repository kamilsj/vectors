//! Typed document eligibility shared by provider preflight and RAG snapshots.

use super::*;

fn predicate(documents: &Table, filters: &[VectorSearchFilter]) -> Result<Option<Expr>> {
    if filters.len() > 32 {
        return Err(invalid("RAG accepts at most 32 document filters"));
    }
    let mut selection = None;
    for filter in filters {
        if let Value::Text(value) = &filter.value {
            if value.len() > 65536 || value.contains('\0') {
                return Err(invalid(
                    "document filter text must contain at most 65536 bytes and no NUL",
                ));
            }
        }
        let expression = typed_search_predicate(&documents.columns, filter.clone())?;
        selection = Some(match selection {
            None => expression,
            Some(left) => Expr::BinaryOp {
                left: Box::new(left),
                op: BinaryOperator::And,
                right: Box::new(expression),
            },
        });
    }
    Ok(selection)
}

impl Database {
    /// Return the collection profile after schema/type-checking predicates,
    /// before provider work. Retrieval validates again under its own snapshot
    /// lock after embeddings return.
    pub fn graph_validate_document_filters(
        &self,
        collection_name: &str,
        filters: &[VectorSearchFilter],
    ) -> Result<GraphCollection> {
        let catalog = self.catalog.read().map_err(|_| Error::LockPoisoned)?;
        let info = collection(&catalog, collection_name)?;
        predicate(table(&catalog, &info.tables.documents)?, filters)?;
        Ok(info)
    }
}

pub(super) fn eligible_chunks(
    documents: &Table,
    chunks: &Table,
    filters: &[VectorSearchFilter],
) -> Result<Option<Vec<bool>>> {
    let Some(selection) = predicate(documents, filters)? else {
        return Ok(None);
    };
    let indexed = indexed_candidate_rows(documents, &selection);
    let mut allowed = HashSet::new();
    let mut consider = |index: usize| -> Result<()> {
        let row = &documents.rows[index];
        if evaluate(&selection, &EvalContext::new(&documents.columns, row))?.as_bool()?
            == Some(true)
        {
            allowed.insert(text_at(row, 0)?);
        }
        Ok(())
    };
    if let Some(candidates) = indexed {
        for row in candidates.rows {
            consider(row)?;
        }
    } else {
        for row in 0..documents.rows.len() {
            consider(row)?;
        }
    }
    // An indexed miss needs no chunk ID reads or lexical/vector work. Keep a
    // mask of the snapshot's shape so the caller can use the same empty path
    // for misses found by indexes and by residual predicate evaluation.
    if allowed.is_empty() {
        return Ok(Some(vec![false; chunks.rows.len()]));
    }
    chunks
        .rows
        .iter()
        .map(|row| Ok(allowed.contains(text_at(row, 1)?)))
        .collect::<Result<Vec<_>>>()
        .map(Some)
}
