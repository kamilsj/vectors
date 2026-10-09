//! Plan scalar candidates using posting counts before allocating row lists.

use super::*;

pub(super) struct IndexedCandidates {
    pub rows: Vec<usize>,
    // Only indexed terms proven TRUE by these candidates may be omitted.
    pub residual: Option<Expr>,
}

struct Selection<'a> {
    rows: RowPlan<'a>,
    residual: Option<Expr>,
}

struct RowPlan<'a> {
    upper_bound: usize,
    kind: PlanKind<'a>,
}

enum PlanKind<'a> {
    Lookup {
        column: usize,
        keys: HashSet<UniqueKey>,
        postings: Vec<&'a [usize]>,
    },
    // The smaller upper bound always drives an intersection, independently
    // of SQL term order or parenthesization. Other plans filter it in place.
    And(Box<RowPlan<'a>>, Box<RowPlan<'a>>),
    Or(Box<RowPlan<'a>>, Box<RowPlan<'a>>),
}

pub(super) fn indexed_candidate_rows(
    table: &Table,
    expression: &Expr,
) -> Option<IndexedCandidates> {
    let selection = plan(table, expression)?;
    Some(IndexedCandidates {
        rows: selection.rows.materialize(table),
        residual: selection.residual,
    })
}

fn plan<'a>(table: &'a Table, expression: &Expr) -> Option<Selection<'a>> {
    match expression {
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => match (plan(table, left), plan(table, right)) {
            (Some(left), Some(right)) => Some(Selection {
                rows: RowPlan::and(left.rows, right.rows),
                residual: and_residual(left.residual, right.residual),
            }),
            (Some(mut indexed), None) => {
                indexed.residual = and_residual(indexed.residual, Some(*right.clone()));
                Some(indexed)
            }
            (None, Some(mut indexed)) => {
                indexed.residual = and_residual(Some(*left.clone()), indexed.residual);
                Some(indexed)
            }
            (None, None) => None,
        },
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Or,
            right,
        } => {
            let (left, right) = (plan(table, left)?, plan(table, right)?);
            Some(Selection {
                rows: RowPlan {
                    upper_bound: left
                        .rows
                        .upper_bound
                        .saturating_add(right.rows.upper_bound)
                        .min(table.rows.len()),
                    kind: PlanKind::Or(Box::new(left.rows), Box::new(right.rows)),
                },
                // A union does not prove which branch matched each row.
                residual: (left.residual.is_some() || right.residual.is_some())
                    .then(|| expression.clone()),
            })
        }
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Eq,
            right,
        } => lookup(table, left, std::slice::from_ref(right.as_ref()))
            .or_else(|| lookup(table, right, std::slice::from_ref(left.as_ref()))),
        Expr::InList {
            expr,
            list,
            negated: false,
        } => lookup(table, expr, list),
        Expr::Nested(inner) => plan(table, inner),
        _ => None,
    }
}

fn and_residual(left: Option<Expr>, right: Option<Expr>) -> Option<Expr> {
    match (left, right) {
        (Some(left), Some(right)) => Some(Expr::BinaryOp {
            left: Box::new(left),
            op: BinaryOperator::And,
            right: Box::new(right),
        }),
        (remaining, None) | (None, remaining) => remaining,
    }
}

fn lookup<'a>(table: &'a Table, expression: &Expr, values: &[Expr]) -> Option<Selection<'a>> {
    let column = simple_column_expression(expression, &table.columns)?;
    let unique = table.unique_keys.get(&column);
    let index = table.indexes.values().find(|index| index.column == column);
    if unique.is_none() && index.is_none() {
        return None;
    }
    let mut keys = HashSet::with_capacity(values.len());
    for expression in values {
        // Resolve every list member before pruning. Mixed types, invalid or
        // row-dependent expressions must retain the general SQL evaluator.
        let value = evaluate(expression, &EvalContext::empty()).ok()?;
        let value = coerce(value, &table.columns[column].data_type).ok()?;
        match value {
            Value::Null => continue,
            Value::Vector(_) => return None,
            _ => {
                keys.insert(UniqueKey::from(&value));
            }
        }
    }
    let postings = keys
        .iter()
        .filter_map(|key| {
            if let Some(unique) = unique {
                unique.get(key).map(std::slice::from_ref)
            } else {
                index?.buckets.get(key).map(Vec::as_slice)
            }
        })
        .collect::<Vec<_>>();
    Some(Selection {
        rows: RowPlan {
            upper_bound: postings.iter().map(|rows| rows.len()).sum(),
            kind: PlanKind::Lookup {
                column,
                keys,
                postings,
            },
        },
        residual: None,
    })
}

impl<'a> RowPlan<'a> {
    fn and(mut left: Self, mut right: Self) -> Self {
        if right.upper_bound < left.upper_bound {
            std::mem::swap(&mut left, &mut right);
        }
        Self {
            upper_bound: left.upper_bound,
            kind: PlanKind::And(Box::new(left), Box::new(right)),
        }
    }

    fn materialize(&self, table: &Table) -> Vec<usize> {
        if self.upper_bound == 0 {
            return Vec::new();
        }
        match &self.kind {
            PlanKind::Lookup { postings, .. } => {
                let mut rows = Vec::with_capacity(self.upper_bound);
                for posting in postings {
                    rows.extend_from_slice(posting);
                }
                if postings.len() > 1 {
                    rows.sort_unstable();
                }
                rows
            }
            PlanKind::And(small, other) => {
                let mut rows = small.materialize(table);
                other.retain(&mut rows, table);
                rows
            }
            PlanKind::Or(left, right) => {
                let (left, right) = (left.materialize(table), right.materialize(table));
                let mut rows = Vec::with_capacity(self.upper_bound);
                let (mut a, mut b) = (0, 0);
                while a < left.len() && b < right.len() {
                    match left[a].cmp(&right[b]) {
                        Ordering::Less => {
                            rows.push(left[a]);
                            a += 1;
                        }
                        Ordering::Greater => {
                            rows.push(right[b]);
                            b += 1;
                        }
                        Ordering::Equal => {
                            rows.push(left[a]);
                            a += 1;
                            b += 1;
                        }
                    }
                }
                rows.extend_from_slice(&left[a..]);
                rows.extend_from_slice(&right[b..]);
                rows
            }
        }
    }

    fn retain(&self, rows: &mut Vec<usize>, table: &Table) {
        if rows.is_empty() {
            return;
        }
        if self.upper_bound == 0 {
            rows.clear();
            return;
        }
        match &self.kind {
            PlanKind::And(small, other) => {
                small.retain(rows, table);
                other.retain(rows, table);
            }
            PlanKind::Lookup { postings, .. } if postings.len() == 1 => {
                let posting = postings[0];
                // Probe a long posting for sparse candidates; merge when both
                // sides are broad. Neither path copies the other posting.
                let probes = rows
                    .len()
                    .saturating_mul(posting.len().ilog2() as usize + 1);
                if probes < posting.len() {
                    rows.retain(|row| posting.binary_search(row).is_ok());
                } else {
                    let mut cursor = 0;
                    rows.retain(|row| {
                        while cursor < posting.len() && posting[cursor] < *row {
                            cursor += 1;
                        }
                        posting.get(cursor) == Some(row)
                    });
                }
            }
            _ => rows.retain(|row| self.contains(*row, table)),
        }
    }

    fn contains(&self, row: usize, table: &Table) -> bool {
        if self.upper_bound == 0 {
            return false;
        }
        match &self.kind {
            PlanKind::Lookup {
                column,
                keys,
                postings,
            } => {
                if postings.len() == 1 {
                    postings[0].binary_search(&row).is_ok()
                } else {
                    keys.contains(&UniqueKey::from(&table.rows[row][*column]))
                }
            }
            PlanKind::And(left, right) => left.contains(row, table) && right.contains(row, table),
            PlanKind::Or(left, right) => left.contains(row, table) || right.contains(row, table),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selective_conjunction_materializes_only_the_smallest_posting() {
        let db = Database::new();
        db.execute("CREATE TABLE items(id INTEGER PRIMARY KEY,scope INTEGER,active BOOLEAN); CREATE INDEX scopes ON items(scope); CREATE INDEX activity ON items(active)").unwrap();
        db.insert_rows(
            "items",
            (0..10_000)
                .map(|id| {
                    vec![
                        Value::Integer(id),
                        Value::Integer(id / 5),
                        Value::Boolean(true),
                    ]
                })
                .collect(),
            InsertConflict::Fail,
        )
        .unwrap();
        let catalog = db.catalog.read().unwrap();
        let table = &catalog.tables["items"];
        for predicate in [
            "active=TRUE AND scope=4",
            "scope=4 AND active=TRUE",
            "(active=TRUE OR scope=8) AND scope=4",
            "active=TRUE AND (scope IN (4,5,6) AND id=21)",
            "scope=9999 AND active=TRUE",
        ] {
            let statement = Parser::parse_sql(
                &GenericDialect {},
                &format!("DELETE FROM items WHERE {predicate}"),
            )
            .unwrap()
            .remove(0);
            let Statement::Delete {
                selection: Some(expression),
                ..
            } = statement
            else {
                panic!("predicate expected")
            };
            let selection = plan(table, &expression).unwrap();
            let rows = selection.rows.materialize(table);
            assert!(
                rows.capacity() <= 5,
                "broad posting was materialized for {predicate}"
            );
            let expected = table
                .rows
                .iter()
                .enumerate()
                .filter(|(_, row)| {
                    evaluate(&expression, &EvalContext::new(&table.columns, row)).unwrap()
                        == Value::Boolean(true)
                })
                .map(|(id, _)| id)
                .collect::<Vec<_>>();
            assert_eq!(rows, expected);
        }
    }
}
