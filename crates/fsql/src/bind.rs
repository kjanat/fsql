//! Bind names and validate the supported SQL subset without reading filesystem rows.
use std::collections::HashMap;

use sqlparser::ast::{
    Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments, Value as Literal,
};

use crate::error::{Error, Result};
use crate::eval::is_aggregate;
use crate::exec::visit;
use crate::plan::{
    InsertRows, Plan, Planner, Projection, QueryPlan, Relation, SelectPlan, SetBody,
};

type Scope = HashMap<String, Vec<String>>;

#[derive(Default, Clone)]
struct Schema {
    tables: Vec<(String, Vec<String>)>,
    merged: Vec<String>,
    outer: Option<Box<Schema>>,
}

impl Schema {
    fn column(&self, name: &str) -> Result<String> {
        let name = name.to_ascii_lowercase();
        let mut matches = Vec::new();
        if let Some((alias, column)) = name.split_once('.') {
            if let Some((_, columns)) = self.tables.iter().find(|(a, _)| a == alias) {
                return if columns.iter().any(|c| c.eq_ignore_ascii_case(column)) {
                    Ok(name)
                } else {
                    Err(Error::UnknownColumn(name))
                };
            }
        } else {
            for (alias, columns) in &self.tables {
                if columns.iter().any(|c| c.eq_ignore_ascii_case(&name)) {
                    matches.push(format!("{alias}.{name}"));
                }
            }
        }
        match matches.len() {
            0 => match &self.outer {
                Some(outer) => outer.column(&name),
                None => Err(Error::UnknownColumn(name)),
            },
            1 => Ok(matches.remove(0)),
            _ if self.merged.contains(&name) => Ok(name),
            _ => Err(Error::Plan(format!("column `{name}` is ambiguous"))),
        }
    }
}

pub(crate) fn plan(plan: &Plan, planner: &Planner) -> Result<()> {
    match plan {
        Plan::Select(query) => {
            query_schema(query, planner, &Scope::new(), &Schema::default())?;
        }
        Plan::Delete(delete) => {
            let schema = Schema {
                tables: vec![(
                    delete.alias.clone(),
                    delete
                        .source
                        .table
                        .columns()
                        .into_iter()
                        .map(str::to_owned)
                        .collect(),
                )],
                ..Schema::default()
            };
            expression(&delete.filter, &schema, planner, &Scope::new(), false)?;
            for key in &delete.order_by {
                expression(&key.expr, &schema, planner, &Scope::new(), false)?;
            }
        }
        Plan::Update(update) => {
            let schema = Schema {
                tables: vec![(
                    update.alias.clone(),
                    update
                        .source
                        .table
                        .columns()
                        .into_iter()
                        .map(str::to_owned)
                        .collect(),
                )],
                ..Schema::default()
            };
            expression(&update.filter, &schema, planner, &Scope::new(), false)?;
            for set in &update.assignments {
                expression(&set.value, &schema, planner, &Scope::new(), false)?;
            }
            for key in &update.order_by {
                expression(&key.expr, &schema, planner, &Scope::new(), false)?;
            }
        }
        Plan::Insert(insert) => match &insert.rows {
            InsertRows::Query(query) => {
                query_schema(query, planner, &Scope::new(), &Schema::default())?;
            }
            InsertRows::Values(rows) => {
                for row in rows {
                    for expr in row {
                        expression(expr, &Schema::default(), planner, &Scope::new(), false)?;
                    }
                }
            }
        },
    }
    Ok(())
}

pub(crate) fn query(plan: &QueryPlan, planner: &Planner) -> Result<Vec<String>> {
    query_schema(plan, planner, &Scope::new(), &Schema::default())
}

fn query_schema(
    plan: &QueryPlan,
    planner: &Planner,
    scope: &Scope,
    outer: &Schema,
) -> Result<Vec<String>> {
    let mut scope = scope.clone();
    for cte in &plan.ctes {
        if plan.recursive && crate::exec::mentions(&cte.query.body, &cte.name) {
            let columns = if !cte.columns.is_empty() {
                cte.columns.clone()
            } else {
                let SetBody::Op { left, .. } = &cte.query.body else {
                    return Err(Error::Plan("recursive CTE needs a UNION".into()));
                };
                body(left, planner, &scope, outer)?.0
            };
            scope.insert(cte.name.clone(), columns);
        }
        let columns = query_schema(&cte.query, planner, &scope, outer)?;
        if !cte.columns.is_empty() && cte.columns.len() != columns.len() {
            return Err(Error::Plan(
                "CTE column count differs from its query".into(),
            ));
        }
        scope.insert(
            cte.name.clone(),
            if cte.columns.is_empty() {
                columns
            } else {
                cte.columns.clone()
            },
        );
    }
    let (headers, schema) = body(&plan.body, planner, &scope, outer)?;
    for key in &plan.order_by {
        match &key.expr {
            Expr::Identifier(i) if headers.iter().any(|h| h.eq_ignore_ascii_case(&i.value)) => {}
            Expr::Value(v) if matches!(v.value, Literal::Number(..)) => {
                let Literal::Number(n, _) = &v.value else {
                    unreachable!()
                };
                let n = n
                    .parse::<usize>()
                    .map_err(|_| Error::Plan("invalid ORDER BY position".into()))?;
                if n == 0 || n > headers.len() {
                    return Err(Error::Plan("ORDER BY position out of range".into()));
                }
            }
            expr => expression(expr, &schema, planner, &scope, true)?,
        }
    }
    if let SetBody::Select(select) = &plan.body {
        let grouped = !select.group_by.is_empty()
            || select
                .projection
                .iter()
                .any(|p| matches!(p, Projection::Expr { expr, .. } if has_aggregate(expr)))
            || plan.order_by.iter().any(|k| has_aggregate(&k.expr))
            || select.having.as_ref().is_some_and(has_aggregate);
        if grouped {
            for projection in &select.projection {
                match projection {
                    Projection::Expr { expr, .. } => {
                        grouped_expression(expr, &select.group_by, &schema, planner, &scope)?
                    }
                    Projection::Wildcard(_) => {
                        return Err(Error::Plan(
                            "wildcards in grouped queries must be expanded explicitly".into(),
                        ));
                    }
                }
            }
            if let Some(having) = &select.having {
                grouped_expression(having, &select.group_by, &schema, planner, &scope)?;
            }
            for key in &plan.order_by {
                if matches!(&key.expr, Expr::Identifier(i) if headers.iter().any(|h| h.eq_ignore_ascii_case(&i.value)))
                    || matches!(&key.expr, Expr::Value(_))
                {
                    continue;
                }
                grouped_expression(&key.expr, &select.group_by, &schema, planner, &scope)?;
            }
        }
    }
    Ok(headers)
}

fn relation(
    relation: &Relation,
    planner: &Planner,
    scope: &Scope,
    outer: &Schema,
) -> Result<(String, Vec<String>)> {
    Ok((
        relation.alias().to_owned(),
        match relation {
            Relation::Table { source, .. } => source
                .table
                .columns()
                .into_iter()
                .map(str::to_owned)
                .collect(),
            Relation::Cte { name, .. } => scope
                .get(name)
                .cloned()
                .ok_or_else(|| Error::UnknownTable(name.clone()))?,
            Relation::Derived { query, .. } => query_schema(query, planner, scope, outer)?,
        },
    ))
}

fn body(
    body: &SetBody,
    planner: &Planner,
    scope: &Scope,
    outer: &Schema,
) -> Result<(Vec<String>, Schema)> {
    match body {
        SetBody::Select(select) => select_schema(select, planner, scope, outer),
        SetBody::Values(rows) => {
            let width = rows.first().map_or(0, Vec::len);
            for row in rows {
                if row.len() != width {
                    return Err(Error::Plan("VALUES rows differ in width".into()));
                }
                for expr in row {
                    expression(expr, outer, planner, scope, false)?;
                }
            }
            Ok((
                (1..=width).map(|n| format!("column{n}")).collect(),
                outer.clone(),
            ))
        }
        SetBody::Op { left, right, .. } => {
            let (headers, _) = self::body(left, planner, scope, outer)?;
            let (right, _) = self::body(right, planner, scope, outer)?;
            if headers.len() != right.len() {
                return Err(Error::Plan("set operation widths differ".into()));
            }
            Ok((
                headers.clone(),
                Schema {
                    tables: vec![(String::new(), headers)],
                    ..Schema::default()
                },
            ))
        }
    }
}

fn select_schema(
    select: &SelectPlan,
    planner: &Planner,
    scope: &Scope,
    outer: &Schema,
) -> Result<(Vec<String>, Schema)> {
    let mut schema = Schema {
        outer: Some(Box::new(outer.clone())),
        ..Schema::default()
    };
    for clause in &select.from {
        schema
            .tables
            .push(relation(&clause.relation, planner, scope, outer)?);
        for join in &clause.joins {
            let right = relation(&join.relation, planner, scope, outer)?;
            for column in &join.using {
                schema.column(column)?;
                if !right.1.contains(column) {
                    return Err(Error::UnknownColumn(column.clone()));
                }
            }
            schema.tables.push(right);
            schema.merged.extend(join.using.iter().cloned());
            if let Some(on) = &join.on {
                expression(on, &schema, planner, scope, false)?;
            }
        }
    }
    for (index, (alias, _)) in schema.tables.iter().enumerate() {
        if schema.tables[..index].iter().any(|(a, _)| a == alias) {
            return Err(Error::Plan(format!("duplicate table alias `{alias}`")));
        }
    }
    if let Some(filter) = &select.filter {
        expression(filter, &schema, planner, scope, false)?;
    }
    for group in &select.group_by {
        expression(group, &schema, planner, scope, false)?;
    }
    if let Some(having) = &select.having {
        expression(having, &schema, planner, scope, true)?;
    }
    let mut headers = Vec::new();
    for projection in &select.projection {
        match projection {
            Projection::Expr { expr, name } => {
                expression(expr, &schema, planner, scope, true)?;
                headers.push(name.clone());
            }
            Projection::Wildcard(qualifier) => {
                let mut found = false;
                for (alias, columns) in &schema.tables {
                    if qualifier.as_ref().is_none_or(|q| q == alias) {
                        headers.extend(columns.iter().cloned());
                        found = true;
                    }
                }
                if !found {
                    return Err(if qualifier.is_none() {
                        Error::Plan("wildcard needs a source".into())
                    } else {
                        Error::UnknownTable(qualifier.clone().unwrap_or_default())
                    });
                }
            }
        }
    }
    // Set-operation operands also need grouping validation; they are not
    // necessarily wrapped in their own QueryPlan.
    if !select.group_by.is_empty()
        || select
            .projection
            .iter()
            .any(|p| matches!(p, Projection::Expr { expr, .. } if has_aggregate(expr)))
        || select.having.as_ref().is_some_and(has_aggregate)
    {
        for projection in &select.projection {
            match projection {
                Projection::Expr { expr, .. } => {
                    grouped_expression(expr, &select.group_by, &schema, planner, scope)?
                }
                Projection::Wildcard(_) => {
                    return Err(Error::Plan(
                        "wildcards in grouped queries must be expanded explicitly".into(),
                    ));
                }
            }
        }
        if let Some(having) = &select.having {
            grouped_expression(having, &select.group_by, &schema, planner, scope)?;
        }
    }
    Ok((headers, schema))
}

fn has_aggregate(expr: &Expr) -> bool {
    let mut found = false;
    visit(expr, &mut |node| {
        found |= matches!(node, Expr::Function(f) if is_aggregate(&f.name.to_string()));
        true
    });
    found
}

fn grouped_expression(
    expr: &Expr,
    groups: &[Expr],
    schema: &Schema,
    planner: &Planner,
    scope: &Scope,
) -> Result<()> {
    let mut result = Ok(());
    visit(expr, &mut |node| {
        if result.is_err()
            || groups
                .iter()
                .any(|group| group.to_string() == node.to_string())
            || matches!(node, Expr::Function(f) if is_aggregate(&f.name.to_string()))
        {
            return false;
        }
        if let Expr::Subquery(query)
        | Expr::Exists {
            subquery: query, ..
        }
        | Expr::InSubquery {
            subquery: query, ..
        } = node
        {
            // Subqueries retain their own namespaces, but may only correlate
            // against columns that identify this group. Do not let an empty
            // result hide an ungrouped outer reference.
            let mut grouped = schema.clone();
            let allowed = groups
                .iter()
                .filter_map(column_name)
                .filter_map(|name| schema.column(&name).ok())
                .collect::<Vec<_>>();
            for (alias, columns) in &mut grouped.tables {
                columns.retain(|column| {
                    allowed.contains(&format!("{alias}.{column}"))
                        || (schema.merged.contains(column) && allowed.contains(column))
                });
            }
            let names = scope.keys().cloned().collect::<Vec<_>>();
            result = planner
                .query_scoped(query, &names)
                .and_then(|plan| query_schema(&plan, planner, scope, &grouped))
                .map(|_| ());
        }
        if let Some(name) = column_name(node) {
            let bound = schema.column(&name);
            let grouped = groups
                .iter()
                .filter_map(column_name)
                .any(|g| schema.column(&g).ok() == bound.as_ref().ok().cloned());
            if !grouped {
                result = Err(Error::Plan(format!(
                    "column `{name}` must be grouped or aggregated"
                )));
            }
        }
        true
    });
    result
}

fn column_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Identifier(i) => Some(i.value.clone()),
        Expr::CompoundIdentifier(parts) => Some(
            parts
                .iter()
                .map(|i| i.value.as_str())
                .collect::<Vec<_>>()
                .join("."),
        ),
        _ => None,
    }
}

fn expression(
    expr: &Expr,
    schema: &Schema,
    planner: &Planner,
    scope: &Scope,
    aggregates: bool,
) -> Result<()> {
    let mut result = Ok(());
    visit(expr, &mut |node| {
        if result.is_err() {
            return false;
        }
        result = (|| -> Result<()> {
            if let Some(name) = column_name(node) {
                schema.column(&name)?;
            }
            match node {
                Expr::Function(function) => {
                    function_shape(function)?;
                    if is_aggregate(&function.name.to_string()) {
                        if !aggregates {
                            return Err(Error::Plan(
                                "aggregate is not allowed in this clause".into(),
                            ));
                        }
                        if let FunctionArguments::List(list) = &function.args {
                            for arg in &list.args {
                                if let FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) = arg
                                    && has_aggregate(expr)
                                {
                                    return Err(Error::Plan("nested aggregate".into()));
                                }
                            }
                        }
                    }
                }
                Expr::Subquery(query)
                | Expr::Exists {
                    subquery: query, ..
                }
                | Expr::InSubquery {
                    subquery: query, ..
                } => {
                    let names = scope.keys().cloned().collect::<Vec<_>>();
                    let plan = planner.query_scoped(query, &names)?;
                    let headers = query_schema(&plan, planner, scope, schema)?;
                    if !matches!(node, Expr::Exists { .. }) && headers.len() != 1 {
                        return Err(Error::Plan("subquery must yield one column".into()));
                    }
                }
                Expr::Identifier(_)
                | Expr::CompoundIdentifier(_)
                | Expr::Value(_)
                | Expr::Nested(_)
                | Expr::UnaryOp { .. }
                | Expr::BinaryOp { .. }
                | Expr::IsNull(_)
                | Expr::IsNotNull(_)
                | Expr::IsTrue(_)
                | Expr::IsNotTrue(_)
                | Expr::IsFalse(_)
                | Expr::IsNotFalse(_)
                | Expr::IsUnknown(_)
                | Expr::IsNotUnknown(_)
                | Expr::InList { .. }
                | Expr::Between { .. }
                | Expr::Like { .. }
                | Expr::ILike { .. }
                | Expr::Interval(_)
                | Expr::Cast { .. }
                | Expr::Substring { .. }
                | Expr::Trim { .. }
                | Expr::Extract { .. }
                | Expr::AnyOp { .. }
                | Expr::AllOp { .. } => {}
                _ => return Err(Error::Unsupported(format!("expression `{node}`"))),
            }
            Ok(())
        })();
        true
    });
    result
}

pub(crate) fn function_shape(function: &Function) -> Result<()> {
    if function.over.is_some()
        || function.filter.is_some()
        || function.null_treatment.is_some()
        || !function.within_group.is_empty()
        || !matches!(function.parameters, FunctionArguments::None)
    {
        return Err(Error::Unsupported(format!(
            "function modifiers in `{function}`"
        )));
    }
    let name = function.name.to_string().to_ascii_lowercase();
    let count = match &function.args {
        FunctionArguments::None => 0,
        FunctionArguments::List(list) => {
            if !list.clauses.is_empty()
                || (!is_aggregate(&name) && list.duplicate_treatment.is_some())
            {
                return Err(Error::Unsupported(format!(
                    "function arguments in `{function}`"
                )));
            }
            for arg in &list.args {
                match arg {
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(_)) => {}
                    FunctionArg::Unnamed(FunctionArgExpr::Wildcard)
                        if name == "count" && list.duplicate_treatment.is_none() => {}
                    _ => return Err(Error::Unsupported(format!("argument `{arg}`"))),
                }
            }
            list.args.len()
        }
        _ => return Err(Error::Unsupported("function subquery arguments".into())),
    };
    let (min, max) = match name.as_str() {
        "now" | "current_timestamp" => (0, 0),
        "lower" | "upper" | "length" | "abs" | "basename" | "dirname" | "extension" | "human"
        | "format_size" | "oct" | "typeof" | "count" | "sum" | "min" | "max" | "avg" => (1, 1),
        "ifnull" | "nullif" | "starts_with" | "ends_with" | "contains" => (2, 2),
        "replace" => (3, 3),
        "substr" | "substring" => (2, 3),
        "trim" | "ltrim" | "rtrim" | "group_concat" | "string_agg" => (1, 2),
        "coalesce" => (1, usize::MAX),
        _ => return Err(Error::UnknownFunction(name)),
    };
    if count < min || count > max {
        return Err(Error::Arity {
            function: name,
            expected: min,
            got: count,
        });
    }
    Ok(())
}
