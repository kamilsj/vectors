//! Streaming scalar hash-join chains using the ordinary SQL evaluator.
use super::*;
use sqlparser::ast::{JoinConstraint, JoinOperator};

const MAX_JOIN_TABLES: usize = 16;

struct JoinSource<'a> {
    table: &'a Table,
    qualifier: String,
    start: usize,
}

struct JoinSchema<'a> {
    left: &'a Table,
    sources: Vec<JoinSource<'a>>,
    columns: Vec<Column>,
}

impl<'a> JoinSchema<'a> {
    fn source(catalog: &'a Catalog, factor: &TableFactor) -> Result<(&'a Table, String)> {
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

    fn new(catalog: &'a Catalog, left: &TableFactor, right: &TableFactor) -> Result<Self> {
        let (left, left_name) = Self::source(catalog, left)?;
        let mut schema = Self {
            left,
            sources: vec![JoinSource {
                table: left,
                qualifier: left_name,
                start: 0,
            }],
            columns: left
                .columns
                .iter()
                .enumerate()
                .map(|(index, column)| Column {
                    name: format!("__join_{index}"),
                    ..column.clone()
                })
                .collect(),
        };
        schema.append(catalog, right)?;
        Ok(schema)
    }

    fn append(&mut self, catalog: &'a Catalog, factor: &TableFactor) -> Result<()> {
        let (table, qualifier) = Self::source(catalog, factor)?;
        if self
            .sources
            .iter()
            .any(|source| source.qualifier.eq_ignore_ascii_case(&qualifier))
        {
            return Err(Error::InvalidQuery(
                "JOIN requires distinct table names or aliases".into(),
            ));
        }
        let start = self.columns.len();
        self.columns.extend(
            table
                .columns
                .iter()
                .enumerate()
                .map(|(index, column)| Column {
                    name: format!("__join_{}", start + index),
                    ..column.clone()
                }),
        );
        self.sources.push(JoinSource {
            table,
            qualifier,
            start,
        });
        Ok(())
    }

    fn range(&self, qualifier: &str) -> Result<std::ops::Range<usize>> {
        if let Some(source) = self
            .sources
            .iter()
            .find(|source| source.qualifier.eq_ignore_ascii_case(qualifier))
        {
            Ok(source.start..source.start + source.table.columns.len())
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
        let source = self
            .sources
            .iter()
            .rev()
            .find(|source| source.start <= index)
            .expect("bound JOIN column");
        &source.table.columns[index - source.start]
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

    fn equijoin_key(&self, expression: &Expr, boundary: usize) -> Result<Option<(usize, usize)>> {
        match expression {
            Expr::Nested(expr) => self.equijoin_key(expr, boundary),
            Expr::BinaryOp {
                left,
                op: BinaryOperator::And,
                right,
            } => Ok(self
                .equijoin_key(left, boundary)?
                .or(self.equijoin_key(right, boundary)?)),
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

enum JoinLookup<'a> {
    Buckets(std::borrow::Cow<'a, HashMap<UniqueKey, Vec<usize>>>),
    Unique(&'a HashMap<UniqueKey, usize>),
}

impl JoinLookup<'_> {
    fn get(&self, key: &UniqueKey) -> Option<&[usize]> {
        match self {
            Self::Buckets(buckets) => buckets.get(key).map(Vec::as_slice),
            Self::Unique(keys) => keys.get(key).map(std::slice::from_ref),
        }
    }
}

struct JoinStage<'a> {
    table: &'a Table,
    boundary: usize,
    left_key: usize,
    right_key: usize,
    numeric_coercion: bool,
    outer: bool,
    on: Expr,
    lookup: JoinLookup<'a>,
}

impl<'a> JoinStage<'a> {
    fn new(schema: &JoinSchema<'a>, operator: &JoinOperator) -> Result<Self> {
        let (outer, on) = match operator {
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
        let source = schema.sources.last().expect("JOIN has a right source");
        let boundary = source.start;
        let (left_key, right_key) = schema.equijoin_key(on, boundary)?.ok_or_else(|| {
            Error::Unsupported(
                "JOIN ON requires scalar column equality between an earlier table and the newly joined table".into(),
            )
        })?;
        let left_type = &schema.columns[left_key].data_type;
        let right_type = &source.table.columns[right_key].data_type;
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
        Ok(Self {
            table: source.table,
            boundary,
            left_key,
            right_key,
            numeric_coercion,
            outer,
            on,
            lookup: JoinLookup::Buckets(std::borrow::Cow::Owned(HashMap::new())),
        })
    }

    fn build_index(&mut self) {
        // Primary/unique keys already have a maintained one-row lookup. Only
        // identical types can reuse it: numeric coercion may collapse distinct
        // integers onto one f64 key and must retain every matching row.
        if !self.numeric_coercion {
            if let Some(keys) = self.table.unique_keys.get(&self.right_key) {
                self.lookup = JoinLookup::Unique(keys);
                return;
            }
        }
        let maintained = (!self.numeric_coercion)
            .then(|| {
                self.table
                    .indexes
                    .values()
                    .find(|index| index.column == self.right_key)
            })
            .flatten();
        self.lookup = JoinLookup::Buckets(if let Some(index) = maintained {
            std::borrow::Cow::Borrowed(&index.buckets)
        } else {
            let mut index: HashMap<UniqueKey, Vec<usize>> = HashMap::new();
            for (row, values) in self.table.rows.iter().enumerate() {
                if let Some(key) = hash_key(&values[self.right_key], self.numeric_coercion) {
                    index.entry(key).or_default().push(row);
                }
            }
            std::borrow::Cow::Owned(index)
        });
    }
}

// Each stage changes only its own references in a single scratch row. No
// intermediate match sets or vector buffers are copied, even for dense joins.
fn visit_join_chain<'a>(
    stages: &[JoinStage<'a>],
    columns: &[Column],
    values: &mut [Option<&'a Value>],
    rows_examined: &mut usize,
    emit: &mut impl FnMut(EvalContext<'_>) -> Result<bool>,
) -> Result<bool> {
    let Some((stage, rest)) = stages.split_first() else {
        return emit(EvalContext::joined_many(columns, values));
    };
    let matches = values[stage.left_key]
        .and_then(|value| hash_key(value, stage.numeric_coercion))
        .and_then(|key| stage.lookup.get(&key));
    let end = stage.boundary + stage.table.columns.len();
    let mut matched = false;
    for &index in matches.into_iter().flatten() {
        for (slot, value) in values[stage.boundary..end]
            .iter_mut()
            .zip(&stage.table.rows[index])
        {
            *slot = Some(value);
        }
        *rows_examined = rows_examined.saturating_add(1);
        if evaluate(&stage.on, &EvalContext::joined_many(columns, values))?
            .as_bool()?
            .unwrap_or(false)
        {
            matched = true;
            if visit_join_chain(rest, columns, values, rows_examined, emit)? {
                return Ok(true);
            }
        }
    }
    if stage.outer && !matched {
        values[stage.boundary..end].fill(None);
        *rows_examined = rows_examined.saturating_add(1);
        return visit_join_chain(rest, columns, values, rows_examined, emit);
    }
    Ok(false)
}

pub(super) fn run_join_query(
    catalog: &Catalog,
    select: &Select,
    query: &Query,
    result_row_limit: Option<usize>,
) -> Result<QueryResult> {
    let from = &select.from[0];
    validate_aggregate_placement(select, &[])?;
    if from.joins.len() >= MAX_JOIN_TABLES {
        return Err(Error::Unsupported(format!(
            "JOIN queries support at most {MAX_JOIN_TABLES} tables"
        )));
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
    let mut schema = JoinSchema::new(catalog, &from.relation, &from.joins[0].relation)?;
    let mut stages = Vec::with_capacity(from.joins.len());
    for (index, join) in from.joins.iter().enumerate() {
        if index != 0 {
            schema.append(catalog, &join.relation)?;
        }
        // Bind each ON against its visible prefix, before later tables exist.
        stages.push(JoinStage::new(&schema, &join.join_operator)?);
    }
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
    // Finish binding/type validation before returning a provably empty result.
    // An empty INNER source makes the entire chain empty, including after
    // earlier LEFT stages; avoid building unrelated right-hand hash tables.
    if limit == Some(0)
        || schema.left.rows.is_empty()
        || stages
            .iter()
            .any(|stage| !stage.outer && stage.table.rows.is_empty())
    {
        return Ok(QueryResult {
            columns: result_columns,
            column_types: result_types,
            rows: Vec::new(),
            rows_examined: 0,
        });
    }
    for stage in &mut stages {
        stage.build_index();
    }
    let mut rows_examined = 0usize;
    let mut emit = |context: EvalContext<'_>| -> Result<bool> {
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
    // Preserve the two-table fast path without any scratch row/reference copy.
    if stages.len() == 1 {
        let stage = &stages[0];
        'source: for left in &schema.left.rows {
            let matches = hash_key(&left[stage.left_key], stage.numeric_coercion)
                .and_then(|key| stage.lookup.get(&key));
            let mut matched = false;
            for &index in matches.into_iter().flatten() {
                let right = &stage.table.rows[index];
                rows_examined = rows_examined.saturating_add(1);
                let context = EvalContext::joined(&schema.columns, left, Some(right));
                if evaluate(&stage.on, &context)?.as_bool()?.unwrap_or(false) {
                    matched = true;
                    if emit(context)? {
                        break 'source;
                    }
                }
            }
            if stage.outer && !matched {
                rows_examined = rows_examined.saturating_add(1);
                if emit(EvalContext::joined(&schema.columns, left, None))? {
                    break 'source;
                }
            }
        }
    } else {
        let mut values = vec![None; schema.columns.len()];
        for left in &schema.left.rows {
            for (slot, value) in values.iter_mut().zip(left) {
                *slot = Some(value);
            }
            if visit_join_chain(
                &stages,
                &schema.columns,
                &mut values,
                &mut rows_examined,
                &mut emit,
            )? {
                break;
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
