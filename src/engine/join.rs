//! Streaming two-table scalar hash joins using the ordinary SQL evaluator.
use super::*;
use sqlparser::ast::{JoinConstraint, JoinOperator};

struct JoinSchema<'a> {
    left: &'a Table,
    right: &'a Table,
    left_name: String,
    right_name: String,
    columns: Vec<Column>,
}

impl<'a> JoinSchema<'a> {
    fn new(catalog: &'a Catalog, left: &TableFactor, right: &TableFactor) -> Result<Self> {
        fn source<'a>(catalog: &'a Catalog, factor: &TableFactor) -> Result<(&'a Table, String)> {
            if !matches!(factor, TableFactor::Table { args: None, with_hints, version: None, partitions, .. } if with_hints.is_empty() && partitions.is_empty())
            {
                return Err(Error::Unsupported(format!("JOIN table source {factor}")));
            }
            let name = table_factor_name(factor)?;
            let qualifier = match factor {
                TableFactor::Table {
                    alias: Some(alias), ..
                } if alias.columns.is_empty() => ident_name(&alias.name),
                TableFactor::Table { alias: None, .. } => name.clone(),
                _ => {
                    return Err(Error::Unsupported(
                        "JOIN table aliases with column lists".into(),
                    ))
                }
            };
            Ok((
                catalog
                    .tables
                    .get(&name)
                    .ok_or_else(|| Error::TableNotFound(name.clone()))?,
                qualifier,
            ))
        }
        let (left, left_name) = source(catalog, left)?;
        let (right, right_name) = source(catalog, right)?;
        if left_name.eq_ignore_ascii_case(&right_name) {
            return Err(Error::InvalidQuery(
                "JOIN requires distinct table names or aliases".into(),
            ));
        }
        let columns = left
            .columns
            .iter()
            .chain(&right.columns)
            .enumerate()
            .map(|(index, column)| Column {
                name: format!("__join_{index}"),
                ..column.clone()
            })
            .collect();
        Ok(Self {
            left,
            right,
            left_name,
            right_name,
            columns,
        })
    }

    fn range(&self, qualifier: &str) -> Result<std::ops::Range<usize>> {
        if self.left_name.eq_ignore_ascii_case(qualifier) {
            Ok(0..self.left.columns.len())
        } else if self.right_name.eq_ignore_ascii_case(qualifier) {
            Ok(self.left.columns.len()..self.columns.len())
        } else {
            Err(Error::InvalidQuery(format!(
                "unknown JOIN table qualifier '{qualifier}'"
            )))
        }
    }

    fn resolve(&self, expression: &Expr) -> Result<usize> {
        let (name, range) = match expression {
            Expr::Identifier(name) => (ident_name(name), 0..self.columns.len()),
            Expr::CompoundIdentifier(parts) if parts.len() == 2 => {
                (ident_name(&parts[1]), self.range(&ident_name(&parts[0]))?)
            }
            _ => {
                return Err(Error::Unsupported(
                    "JOIN keys must be scalar column references".into(),
                ))
            }
        };
        let mut matches = range.filter(|index| {
            self.original_column(*index)
                .name
                .eq_ignore_ascii_case(&name)
        });
        let index = matches
            .next()
            .ok_or_else(|| Error::ColumnNotFound(name.clone()))?;
        if matches.next().is_some() {
            return Err(Error::InvalidQuery(format!(
                "ambiguous JOIN column '{name}'; qualify it with a table alias"
            )));
        }
        Ok(index)
    }

    fn original_column(&self, index: usize) -> &Column {
        if index < self.left.columns.len() {
            &self.left.columns[index]
        } else {
            &self.right.columns[index - self.left.columns.len()]
        }
    }

    fn identifier(&self, index: usize) -> Expr {
        // Keep bound source references qualified so ORDER BY cannot mistake an
        // internal name for a user-defined projection alias with the same text.
        Expr::CompoundIdentifier(vec![
            Ident::new("__joined"),
            Ident::new(&self.columns[index].name),
        ])
    }

    // Resolve references once; all operators/functions remain the existing
    // evaluator's responsibility, including typing, NULL and vector behavior.
    fn normalize(&self, expression: &mut Expr) -> Result<()> {
        match expression {
            Expr::Identifier(_) | Expr::CompoundIdentifier(_) => {
                *expression = self.identifier(self.resolve(expression)?)
            }
            Expr::Array(array) => {
                for item in &mut array.elem {
                    self.normalize(item)?;
                }
            }
            Expr::Function(function) => {
                if function.null_treatment.is_some() {
                    return Err(Error::Unsupported("function NULL treatment in JOIN".into()));
                }
                for argument in &mut function.args {
                    match argument {
                        FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => {
                            self.normalize(expr)?
                        }
                        _ => {
                            return Err(Error::Unsupported(
                                "JOIN function wildcard or named arguments".into(),
                            ))
                        }
                    }
                }
            }
            Expr::BinaryOp { left, right, .. } => {
                self.normalize(left)?;
                self.normalize(right)?;
            }
            Expr::UnaryOp { expr, .. }
            | Expr::Nested(expr)
            | Expr::IsNull(expr)
            | Expr::IsNotNull(expr)
            | Expr::IsTrue(expr)
            | Expr::IsFalse(expr)
            | Expr::IsNotTrue(expr)
            | Expr::IsNotFalse(expr) => self.normalize(expr)?,
            Expr::Cast { expr, format, .. } => {
                if format.is_some() {
                    return Err(Error::Unsupported("CAST ... FORMAT in JOIN".into()));
                }
                self.normalize(expr)?;
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                self.normalize(expr)?;
                self.normalize(low)?;
                self.normalize(high)?;
            }
            Expr::InList { expr, list, .. } => {
                self.normalize(expr)?;
                for item in list {
                    self.normalize(item)?;
                }
            }
            Expr::Like {
                expr,
                pattern,
                escape_char,
                ..
            }
            | Expr::ILike {
                expr,
                pattern,
                escape_char,
                ..
            } => {
                if escape_char.is_some() {
                    return Err(Error::Unsupported("LIKE ... ESCAPE".into()));
                }
                self.normalize(expr)?;
                self.normalize(pattern)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn projection(&self, items: &[SelectItem]) -> Result<Vec<Projection>> {
        let mut normalized = Vec::new();
        for item in items {
            let range = match item {
                SelectItem::Wildcard(options) if options.to_string().is_empty() => {
                    Some(0..self.columns.len())
                }
                SelectItem::QualifiedWildcard(name, options)
                    if options.to_string().is_empty() && name.0.len() == 1 =>
                {
                    Some(self.range(&ident_name(&name.0[0]))?)
                }
                _ => None,
            };
            if let Some(range) = range {
                normalized.extend(range.map(|index| SelectItem::ExprWithAlias {
                    expr: self.identifier(index),
                    alias: Ident::with_quote('"', &self.original_column(index).name),
                }));
                continue;
            }
            let (mut expr, alias) = match item {
                SelectItem::UnnamedExpr(expr) => {
                    (expr.clone(), Ident::with_quote('"', expression_label(expr)))
                }
                SelectItem::ExprWithAlias { expr, alias } => (expr.clone(), alias.clone()),
                _ => {
                    return Err(Error::Unsupported(format!(
                        "JOIN wildcard projection {item}"
                    )))
                }
            };
            self.normalize(&mut expr)?;
            normalized.push(SelectItem::ExprWithAlias { expr, alias });
        }
        build_projection(&normalized, &self.columns)
    }

    fn equijoin_key(&self, expression: &Expr) -> Result<Option<(usize, usize)>> {
        match expression {
            Expr::Nested(expr) => self.equijoin_key(expr),
            Expr::BinaryOp {
                left,
                op: BinaryOperator::And,
                right,
            } => Ok(self.equijoin_key(left)?.or(self.equijoin_key(right)?)),
            Expr::BinaryOp {
                left,
                op: BinaryOperator::Eq,
                right,
            } if matches!(
                left.as_ref(),
                Expr::Identifier(_) | Expr::CompoundIdentifier(_)
            ) && matches!(
                right.as_ref(),
                Expr::Identifier(_) | Expr::CompoundIdentifier(_)
            ) =>
            {
                let a = self.resolve(left)?;
                let b = self.resolve(right)?;
                let boundary = self.left.columns.len();
                match (a < boundary, b < boundary) {
                    (true, false) => Ok(Some((a, b - boundary))),
                    (false, true) => Ok(Some((b, a - boundary))),
                    _ => Ok(None),
                }
            }
            _ => Ok(None),
        }
    }
}

fn hash_key(value: &Value, numeric_coercion: bool) -> Option<UniqueKey> {
    match value {
        Value::Null => None,
        Value::Integer(value) if numeric_coercion => {
            Some(UniqueKey::from(&Value::Float(*value as f64)))
        }
        value => Some(UniqueKey::from(value)),
    }
}

pub(super) fn run_join_query(
    catalog: &Catalog,
    select: &Select,
    query: &Query,
    result_row_limit: Option<usize>,
) -> Result<QueryResult> {
    let from = &select.from[0];
    validate_aggregate_placement(select, &[])?;
    if from.joins.len() != 1 {
        return Err(Error::Unsupported(
            "exactly one INNER or LEFT equijoin between two tables is supported".into(),
        ));
    }
    if !matches!(&select.group_by, sqlparser::ast::GroupByExpr::Expressions(items) if items.is_empty())
        || select.having.is_some()
        || select.projection.iter().any(|item| match item {
            SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                expression_contains_aggregate(expr)
            }
            _ => false,
        })
        || query
            .order_by
            .iter()
            .any(|item| expression_contains_aggregate(&item.expr))
    {
        return Err(Error::Unsupported(
            "aggregate expressions, GROUP BY, and HAVING in JOIN queries".into(),
        ));
    }
    if matches!(select.distinct, Some(sqlparser::ast::Distinct::On(_))) {
        return Err(Error::Unsupported("DISTINCT ON in JOIN queries".into()));
    }
    let join = &from.joins[0];
    let (outer, on) = match &join.join_operator {
        JoinOperator::Inner(JoinConstraint::On(on)) => (false, on),
        JoinOperator::LeftOuter(JoinConstraint::On(on)) => (true, on),
        _ => {
            return Err(Error::Unsupported(
                "JOIN requires INNER or LEFT JOIN with an ON scalar equality".into(),
            ))
        }
    };
    if expression_contains_aggregate(on) {
        return Err(Error::Unsupported(
            "aggregate expressions in JOIN ON".into(),
        ));
    }
    let schema = JoinSchema::new(catalog, &from.relation, &join.relation)?;
    let (left_key, right_key) = schema.equijoin_key(on)?.ok_or_else(|| {
        Error::Unsupported(
            "JOIN ON requires equality between scalar columns from opposite tables".into(),
        )
    })?;
    let left_type = &schema.left.columns[left_key].data_type;
    let right_type = &schema.right.columns[right_key].data_type;
    if matches!(left_type, DataType::Vector(_)) || matches!(right_type, DataType::Vector(_)) {
        return Err(Error::Unsupported(
            "JOIN keys must be scalar columns".into(),
        ));
    }
    ensure_comparable_types(
        &Some(left_type.clone()),
        &Some(right_type.clone()),
        &BinaryOperator::Eq,
    )?;
    let numeric_coercion = left_type != right_type;
    let mut on = on.clone();
    schema.normalize(&mut on)?;
    ensure_boolean_type(&expression_data_type(&on, &schema.columns)?)?;
    let mut selection = select.selection.clone();
    if let Some(selection) = &mut selection {
        schema.normalize(selection)?;
        ensure_boolean_type(&expression_data_type(selection, &schema.columns)?)?;
    }
    let projection = schema.projection(&select.projection)?;
    let result_columns = projection
        .iter()
        .map(|item| item.label.clone())
        .collect::<Vec<_>>();
    let result_types = projection
        .iter()
        .map(|item| expression_data_type(&item.expression, &schema.columns))
        .collect::<Result<Vec<_>>>()?;
    let mut order_by = query.order_by.clone();
    for item in &mut order_by {
        let aliases = match &item.expr {
            Expr::Identifier(name) => result_columns
                .iter()
                .filter(|label| label.eq_ignore_ascii_case(&ident_name(name)))
                .count(),
            _ => 0,
        };
        if aliases > 1 {
            return Err(Error::InvalidQuery(format!(
                "ambiguous JOIN ORDER BY alias '{}'",
                item.expr
            )));
        }
        if aliases == 0 {
            schema.normalize(&mut item.expr)?;
        } else if let Expr::Identifier(name) = &mut item.expr {
            // The shared ORDER BY evaluator normalizes projection labels;
            // normalize quoted aliases too after case-insensitive binding.
            *name = Ident::new(normalize_name(&name.value));
        }
        ensure_sortable_scalar_type(&expression_data_type(
            resolve_order_expression(item, &projection, &result_columns),
            &schema.columns,
        )?)?;
    }
    let empty = EvalContext::empty();
    let offset = query
        .offset
        .as_ref()
        .map(|offset| usize_expression(&offset.value, &empty, "OFFSET"))
        .transpose()?
        .unwrap_or(0);
    let sql_limit = query
        .limit
        .as_ref()
        .map(|limit| usize_expression(limit, &empty, "LIMIT"))
        .transpose()?;
    let limit = match (sql_limit, result_row_limit) {
        (Some(limit), Some(max)) => Some(limit.min(max.saturating_add(1))),
        (None, Some(max)) => Some(max.saturating_add(1)),
        (limit, None) => limit,
    };
    let mut sink = CandidateSink::new(&order_by, offset, limit, select.distinct.is_some())?;
    if limit == Some(0) {
        return Ok(QueryResult {
            columns: result_columns,
            column_types: result_types,
            rows: Vec::new(),
            rows_examined: 0,
        });
    }
    let maintained = (!numeric_coercion)
        .then(|| {
            schema
                .right
                .indexes
                .values()
                .find(|index| index.column == right_key)
        })
        .flatten();
    let built;
    let buckets = if let Some(index) = maintained {
        &index.buckets
    } else {
        let mut index: HashMap<UniqueKey, Vec<usize>> = HashMap::new();
        for (row, values) in schema.right.rows.iter().enumerate() {
            if let Some(key) = hash_key(&values[right_key], numeric_coercion) {
                index.entry(key).or_default().push(row);
            }
        }
        built = index;
        &built
    };
    let mut rows_examined = 0usize;
    let mut emit = |left: &[Value], right: Option<&[Value]>| -> Result<bool> {
        let context = EvalContext::joined(&schema.columns, left, right);
        if let Some(selection) = &selection {
            if !evaluate(selection, &context)?.as_bool()?.unwrap_or(false) {
                return Ok(false);
            }
        }
        let values = projection
            .iter()
            .map(|item| evaluate(&item.expression, &context))
            .collect::<Result<Vec<_>>>()?;
        let order = order_by
            .iter()
            .map(|item| evaluate_order(item, &context, &result_columns, &values))
            .collect::<Result<Vec<_>>>()?;
        sink.push(Candidate { values, order });
        Ok(sink.is_full())
    };
    // Probe in source order; each bucket preserves right-table source order.
    // Only one borrowed pair exists at a time; vector buffers are never copied.
    'source: for left in &schema.left.rows {
        let matches = hash_key(&left[left_key], numeric_coercion).and_then(|key| buckets.get(&key));
        let mut matched = false;
        for &index in matches.into_iter().flatten() {
            let right = &schema.right.rows[index];
            rows_examined = rows_examined.saturating_add(1);
            if evaluate(
                &on,
                &EvalContext::joined(&schema.columns, left, Some(right)),
            )?
            .as_bool()?
            .unwrap_or(false)
            {
                matched = true;
                if emit(left, Some(right))? {
                    break 'source;
                }
            }
        }
        if outer && !matched {
            rows_examined = rows_examined.saturating_add(1);
            if emit(left, None)? {
                break 'source;
            }
        }
    }
    let result = finish_candidates(
        sink.into_candidates(),
        (result_columns, result_types),
        false,
        &order_by,
        offset,
        limit,
        rows_examined,
    )?;
    enforce_query_row_limit(result, result_row_limit)
}
