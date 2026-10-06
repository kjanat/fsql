use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use sqlparser::ast::{
    DuplicateTreatment, Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments, Ident,
    Query, Value as Literal,
};

use crate::column::Table;
use crate::error::{Error, Result};
use crate::eval::{Evaluator, Row, is_aggregate, render};
use crate::output::ResultSet;
use crate::plan::{
    Cte, JoinKind, JoinSpec, OrderKey, Planner, Projection, QueryPlan, Relation, SelectPlan,
    SetBody, SetOp, Source,
};
use crate::value::Value;
use crate::walk::Walker;

pub struct MapRow(pub HashMap<String, Value>);

impl Row for MapRow {
    fn column(&self, name: &str) -> Result<Value> {
        self.0
            .get(name)
            .cloned()
            .ok_or_else(|| Error::UnknownColumn(name.to_owned()))
    }
}

pub struct EmptyRow;

impl Row for EmptyRow {
    fn column(&self, name: &str) -> Result<Value> {
        Err(Error::UnknownColumn(name.to_owned()))
    }
}

pub struct Aliased<'a> {
    pub alias: &'a str,
    pub row: &'a dyn Row,
}

impl Row for Aliased<'_> {
    fn column(&self, name: &str) -> Result<Value> {
        match name.split_once('.') {
            Some((qualifier, column)) if qualifier == self.alias => self.row.column(column),
            Some(_) => Err(Error::UnknownColumn(name.to_owned())),
            None => self.row.column(name),
        }
    }
}

struct NullRow {
    names: HashSet<String>,
}

impl Row for NullRow {
    fn column(&self, name: &str) -> Result<Value> {
        if self.names.contains(name) {
            Ok(Value::Null)
        } else {
            Err(Error::UnknownColumn(name.to_owned()))
        }
    }
}

pub struct JoinRow<'a> {
    parts: Vec<(String, Rc<dyn Row + 'a>)>,
    merged: Vec<String>,
    outer: Option<&'a dyn Row>,
}

impl<'a> JoinRow<'a> {
    fn single(alias: &str, row: Box<dyn Row + 'a>, outer: Option<&'a dyn Row>) -> Self {
        Self {
            parts: vec![(alias.to_owned(), Rc::from(row))],
            merged: Vec::new(),
            outer,
        }
    }

    fn extend(&self, alias: &str, row: Rc<dyn Row + 'a>, merged: &[String]) -> Self {
        let mut parts = self.parts.clone();
        parts.push((alias.to_owned(), row));
        let mut all = self.merged.clone();
        all.extend(merged.iter().cloned());
        Self {
            parts,
            merged: all,
            outer: self.outer,
        }
    }
}

impl Row for JoinRow<'_> {
    fn column(&self, name: &str) -> Result<Value> {
        let local = match name.split_once('.') {
            Some((qualifier, column)) => {
                match self.parts.iter().find(|(alias, _)| alias == qualifier) {
                    Some((_, row)) => row.column(column),
                    None => Err(Error::UnknownColumn(name.to_owned())),
                }
            }
            None => {
                let mut found: Vec<Value> = Vec::new();
                let mut failure: Option<Error> = None;
                for (_, row) in &self.parts {
                    match row.column(name) {
                        Ok(value) => {
                            if self.merged.iter().any(|m| m == name) {
                                if !value.is_null() {
                                    return Ok(value);
                                }
                                if found.is_empty() {
                                    found.push(value);
                                }
                            } else {
                                found.push(value);
                            }
                        }
                        Err(Error::UnknownColumn(_)) => {}
                        Err(error) => {
                            failure = Some(error);
                            break;
                        }
                    }
                }
                if let Some(error) = failure {
                    Err(error)
                } else {
                    match found.len() {
                        0 => Err(Error::UnknownColumn(name.to_owned())),
                        1 => Ok(found.remove(0)),
                        _ => Err(Error::Plan(format!("column `{name}` is ambiguous"))),
                    }
                }
            }
        };
        match (local, self.outer) {
            (Err(Error::UnknownColumn(_)), Some(outer)) => outer.column(name),
            (result, _) => result,
        }
    }
}

pub type RowStream<'a> = Box<dyn Iterator<Item = Result<Box<dyn Row + 'a>>> + 'a>;

pub struct Context {
    pub planner: Planner,
    pub sink: RefCell<Vec<Error>>,
}

type Scope = HashMap<String, Rc<ResultSet>>;

pub fn base_rows<'a>(source: &Source) -> Result<RowStream<'a>> {
    match source.table {
        Table::Files => {
            let walker = Walker::new(&source.root, source.options.clone())?;
            Ok(Box::new(walker.map(|entry| {
                entry.map(|entry| Box::new(entry) as Box<dyn Row + 'a>)
            })))
        }
        Table::Mounts => crate::mounts::rows(),
        Table::Xattrs => crate::xattr::rows(source),
        Table::Acls => crate::xattr::acl_rows(source),
    }
}

pub fn run(
    plan: &QueryPlan,
    planner: &Planner,
    errors: &mut dyn FnMut(Error),
) -> Result<ResultSet> {
    let ctx = Rc::new(Context {
        planner: planner.clone(),
        sink: RefCell::new(Vec::new()),
    });
    let result = run_query(plan, &ctx, &Scope::new(), None);
    drain(&ctx, errors);
    result
}

fn report(ctx: &Context, error: Error) {
    ctx.sink.borrow_mut().push(error);
}

fn set_rows(set: &ResultSet) -> Vec<Box<dyn Row + 'static>> {
    set.rows
        .iter()
        .map(|values| {
            let map: HashMap<String, Value> = set
                .headers
                .iter()
                .cloned()
                .zip(values.iter().cloned())
                .collect();
            Box::new(MapRow(map)) as Box<dyn Row>
        })
        .collect()
}

fn mentions(body: &SetBody, name: &str) -> bool {
    match body {
        SetBody::Values(_) => false,
        SetBody::Op { left, right, .. } => mentions(left, name) || mentions(right, name),
        SetBody::Select(select) => select.relations().iter().any(|relation| match relation {
            Relation::Cte { name: cte, .. } => cte == name,
            Relation::Derived { query, .. } => mentions(&query.body, name),
            Relation::Table { .. } => false,
        }),
    }
}

fn rename_columns(mut set: ResultSet, columns: &[String]) -> Result<ResultSet> {
    if columns.is_empty() {
        return Ok(set);
    }
    if columns.len() != set.headers.len() {
        return Err(Error::Plan(format!(
            "CTE names {} columns but its query yields {}",
            columns.len(),
            set.headers.len()
        )));
    }
    set.headers = columns.to_vec();
    Ok(set)
}

fn recursive_cte(
    cte: &Cte,
    ctx: &Rc<Context>,
    scope: &Scope,
    outer: Option<&dyn Row>,
) -> Result<ResultSet> {
    let SetBody::Op {
        left,
        op: SetOp::Union,
        all,
        right,
    } = &cte.query.body
    else {
        return Err(Error::Plan(format!(
            "recursive CTE `{}` must be `base UNION [ALL] step`",
            cte.name
        )));
    };
    let mut result = rename_columns(body_rows(left, ctx, scope, outer)?, &cte.columns)?;
    let mut seen: HashSet<String> = result.rows.iter().map(|r| format!("{r:?}")).collect();
    let mut working = result.clone_rows();
    let mut scope = scope.clone();
    for _ in 0..100_000 {
        if working.rows.is_empty() {
            return Ok(result);
        }
        scope.insert(cte.name.clone(), Rc::new(working));
        let step = rename_columns(body_rows(right, ctx, &scope, outer)?, &cte.columns)?;
        if step.headers.len() != result.headers.len() {
            return Err(Error::Plan(format!(
                "recursive CTE `{}` step yields {} columns, base yields {}",
                cte.name,
                step.headers.len(),
                result.headers.len()
            )));
        }
        let fresh: Vec<Vec<Value>> = step
            .rows
            .into_iter()
            .filter(|row| *all || seen.insert(format!("{row:?}")))
            .collect();
        result.rows.extend(fresh.iter().cloned());
        working = ResultSet {
            headers: result.headers.clone(),
            rows: fresh,
        };
    }
    Err(Error::Plan(format!(
        "recursive CTE `{}` did not terminate",
        cte.name
    )))
}

impl ResultSet {
    fn clone_rows(&self) -> ResultSet {
        ResultSet {
            headers: self.headers.clone(),
            rows: self.rows.clone(),
        }
    }
}

pub fn run_query(
    plan: &QueryPlan,
    ctx: &Rc<Context>,
    scope: &Scope,
    outer: Option<&dyn Row>,
) -> Result<ResultSet> {
    let mut scope = scope.clone();
    for cte in &plan.ctes {
        let set = if plan.recursive && mentions(&cte.query.body, &cte.name) {
            recursive_cte(cte, ctx, &scope, outer)?
        } else {
            rename_columns(run_query(&cte.query, ctx, &scope, outer)?, &cte.columns)?
        };
        scope.insert(cte.name.clone(), Rc::new(set));
    }
    match &plan.body {
        SetBody::Select(select) => select_rows(
            select,
            &plan.order_by,
            plan.limit,
            plan.offset,
            ctx,
            &scope,
            outer,
        ),
        body => {
            let set = body_rows(body, ctx, &scope, outer)?;
            order_set(set, &plan.order_by, plan.limit, plan.offset)
        }
    }
}

fn body_rows(
    body: &SetBody,
    ctx: &Rc<Context>,
    scope: &Scope,
    outer: Option<&dyn Row>,
) -> Result<ResultSet> {
    match body {
        SetBody::Select(select) => select_rows(select, &[], None, 0, ctx, scope, outer),
        SetBody::Values(rows) => {
            let mut evaluator = evaluator(ctx, scope);
            let width = rows.first().map(Vec::len).unwrap_or(0);
            let headers = (1..=width).map(|i| format!("column{i}")).collect();
            let rows = rows
                .iter()
                .map(|row| {
                    if row.len() != width {
                        return Err(Error::Plan("VALUES rows differ in width".to_owned()));
                    }
                    row.iter()
                        .map(|expr| evaluator.eval(expr, outer.unwrap_or(&EmptyRow)))
                        .collect()
                })
                .collect::<Result<Vec<Vec<Value>>>>()?;
            Ok(ResultSet { headers, rows })
        }
        SetBody::Op {
            left,
            op,
            all,
            right,
        } => {
            let left = body_rows(left, ctx, scope, outer)?;
            let right = body_rows(right, ctx, scope, outer)?;
            if left.headers.len() != right.headers.len() {
                return Err(Error::Plan(format!(
                    "set operation sides yield {} and {} columns",
                    left.headers.len(),
                    right.headers.len()
                )));
            }
            let key = |row: &Vec<Value>| format!("{row:?}");
            let rows = match op {
                SetOp::Union => {
                    let mut rows = left.rows;
                    rows.extend(right.rows);
                    if *all {
                        rows
                    } else {
                        let mut seen = HashSet::new();
                        rows.into_iter().filter(|r| seen.insert(key(r))).collect()
                    }
                }
                SetOp::Intersect => {
                    let mut counts: HashMap<String, usize> = HashMap::new();
                    for row in &right.rows {
                        *counts.entry(key(row)).or_default() += 1;
                    }
                    let mut emitted = HashSet::new();
                    left.rows
                        .into_iter()
                        .filter(|row| {
                            let k = key(row);
                            match counts.get_mut(&k) {
                                Some(n) if *n > 0 => {
                                    if *all {
                                        *n -= 1;
                                        true
                                    } else {
                                        emitted.insert(k)
                                    }
                                }
                                _ => false,
                            }
                        })
                        .collect()
                }
                SetOp::Except => {
                    let mut counts: HashMap<String, usize> = HashMap::new();
                    for row in &right.rows {
                        *counts.entry(key(row)).or_default() += 1;
                    }
                    let mut emitted = HashSet::new();
                    left.rows
                        .into_iter()
                        .filter(|row| {
                            let k = key(row);
                            match counts.get_mut(&k) {
                                Some(n) if *n > 0 => {
                                    if *all {
                                        *n -= 1;
                                    }
                                    false
                                }
                                _ => *all || emitted.insert(k),
                            }
                        })
                        .collect()
                }
            };
            Ok(ResultSet {
                headers: left.headers,
                rows,
            })
        }
    }
}

fn order_set(
    set: ResultSet,
    order_by: &[OrderKey],
    limit: Option<usize>,
    offset: usize,
) -> Result<ResultSet> {
    let sources: Vec<usize> = order_by
        .iter()
        .map(|key| match &key.expr {
            Expr::Identifier(ident) => set
                .headers
                .iter()
                .position(|h| h.eq_ignore_ascii_case(&ident.value))
                .ok_or_else(|| Error::UnknownColumn(ident.value.clone())),
            Expr::Value(literal) => match &literal.value {
                Literal::Number(text, _) => {
                    let position: usize = text.parse().map_err(|_| {
                        Error::Plan(format!("ORDER BY position `{text}` is not a number"))
                    })?;
                    if position == 0 || position > set.headers.len() {
                        return Err(Error::Plan(format!(
                            "ORDER BY position {position} is out of range"
                        )));
                    }
                    Ok(position - 1)
                }
                other => Err(Error::Plan(format!(
                    "ORDER BY `{other}` on a set operation must name an output column"
                ))),
            },
            other => Err(Error::Plan(format!(
                "ORDER BY `{other}` on a set operation must name an output column"
            ))),
        })
        .collect::<Result<Vec<_>>>()?;
    let mut rows = set.rows;
    if !order_by.is_empty() {
        rows.sort_by(|a, b| {
            let ka: Vec<Value> = sources.iter().map(|i| a[*i].clone()).collect();
            let kb: Vec<Value> = sources.iter().map(|i| b[*i].clone()).collect();
            compare_keys(&ka, &kb, order_by)
        });
    }
    let rows = rows
        .into_iter()
        .skip(offset)
        .take(limit.unwrap_or(usize::MAX))
        .collect();
    Ok(ResultSet {
        headers: set.headers,
        rows,
    })
}

pub fn standalone(planner: &Planner) -> (Evaluator, Rc<Context>) {
    let ctx = Rc::new(Context {
        planner: planner.clone(),
        sink: RefCell::new(Vec::new()),
    });
    let evaluator = evaluator(&ctx, &Scope::new());
    (evaluator, ctx)
}

pub fn drain(ctx: &Context, errors: &mut dyn FnMut(Error)) {
    for error in ctx.sink.borrow_mut().drain(..) {
        errors(error);
    }
}

fn evaluator(ctx: &Rc<Context>, scope: &Scope) -> Evaluator {
    let ctx = ctx.clone();
    let scope = scope.clone();
    let names: Vec<String> = scope.keys().cloned().collect();
    let mut evaluator = Evaluator::default();
    evaluator.subqueries = Some(Rc::new(move |query: &Query, outer: Option<&dyn Row>| {
        let plan = ctx.planner.query_scoped(query, &names)?;
        run_query(&plan, &ctx, &scope, outer)
    }));
    evaluator
}

pub fn visit<'a>(expr: &'a Expr, f: &mut dyn FnMut(&'a Expr) -> bool) {
    if !f(expr) {
        return;
    }
    match expr {
        Expr::BinaryOp { left, right, .. } => {
            visit(left, f);
            visit(right, f);
        }
        Expr::UnaryOp { expr, .. }
        | Expr::Nested(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr)
        | Expr::IsTrue(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsFalse(expr)
        | Expr::IsNotFalse(expr)
        | Expr::IsUnknown(expr)
        | Expr::IsNotUnknown(expr)
        | Expr::Cast { expr, .. }
        | Expr::Extract { expr, .. } => visit(expr, f),
        Expr::InList { expr, list, .. } => {
            visit(expr, f);
            for item in list {
                visit(item, f);
            }
        }
        Expr::InSubquery { expr, .. } => visit(expr, f),
        Expr::AnyOp { left, right, .. } | Expr::AllOp { left, right, .. } => {
            visit(left, f);
            visit(right, f);
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            visit(expr, f);
            visit(low, f);
            visit(high, f);
        }
        Expr::Like { expr, pattern, .. } | Expr::ILike { expr, pattern, .. } => {
            visit(expr, f);
            visit(pattern, f);
        }
        Expr::Function(function) => {
            if let FunctionArguments::List(list) = &function.args {
                for arg in &list.args {
                    if let FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) = arg {
                        visit(expr, f);
                    }
                }
            }
        }
        Expr::Substring {
            expr,
            substring_from,
            substring_for,
            ..
        } => {
            visit(expr, f);
            if let Some(from) = substring_from {
                visit(from, f);
            }
            if let Some(length) = substring_for {
                visit(length, f);
            }
        }
        Expr::Trim {
            expr, trim_what, ..
        } => {
            visit(expr, f);
            if let Some(what) = trim_what {
                visit(what, f);
            }
        }
        Expr::Interval(interval) => visit(&interval.value, f),
        _ => {}
    }
}

fn aggregate_function(expr: &Expr) -> Option<&Function> {
    match expr {
        Expr::Function(function) if is_aggregate(&function.name.to_string()) => Some(function),
        _ => None,
    }
}

pub fn referenced_columns(exprs: &[&Expr]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for expr in exprs {
        visit(expr, &mut |node| {
            if aggregate_function(node).is_some() {
                return false;
            }
            let name = match node {
                Expr::Identifier(ident) => Some(ident.value.to_ascii_lowercase()),
                Expr::CompoundIdentifier(parts) => Some(
                    parts
                        .iter()
                        .map(|p| p.value.to_ascii_lowercase())
                        .collect::<Vec<_>>()
                        .join("."),
                ),
                _ => None,
            };
            if let Some(name) = name
                && !names.contains(&name)
            {
                names.push(name);
            }
            true
        });
    }
    names
}

#[derive(Debug, Clone)]
struct AggregateCall {
    key: String,
    name: String,
    distinct: bool,
    arg: Option<Expr>,
    separator: Option<Expr>,
}

fn aggregate_calls(exprs: &[&Expr]) -> Result<Vec<AggregateCall>> {
    let mut calls: Vec<AggregateCall> = Vec::new();
    let mut error = None;
    for expr in exprs {
        visit(expr, &mut |node| {
            let Some(function) = aggregate_function(node) else {
                return true;
            };
            let key = function.to_string();
            if calls.iter().any(|c| c.key == key) {
                return false;
            }
            match parse_aggregate(function) {
                Ok(call) => calls.push(call),
                Err(e) => {
                    if error.is_none() {
                        error = Some(e);
                    }
                }
            }
            false
        });
    }
    match error {
        Some(error) => Err(error),
        None => Ok(calls),
    }
}

fn parse_aggregate(function: &Function) -> Result<AggregateCall> {
    let name = function.name.to_string().to_ascii_lowercase();
    let FunctionArguments::List(list) = &function.args else {
        return Err(Error::Arity {
            function: name,
            expected: 1,
            got: 0,
        });
    };
    let distinct = matches!(list.duplicate_treatment, Some(DuplicateTreatment::Distinct));
    let mut args = list.args.iter();
    let arg = match args.next() {
        Some(FunctionArg::Unnamed(FunctionArgExpr::Wildcard)) if name == "count" => None,
        Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))) => Some(expr.clone()),
        _ => {
            return Err(Error::Arity {
                function: name,
                expected: 1,
                got: list.args.len(),
            });
        }
    };
    let separator = match args.next() {
        None => None,
        Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)))
            if name == "group_concat" || name == "string_agg" =>
        {
            Some(expr.clone())
        }
        Some(_) => {
            return Err(Error::Arity {
                function: name,
                expected: 1,
                got: list.args.len(),
            });
        }
    };
    let mut nested = false;
    if let Some(arg) = &arg {
        visit(arg, &mut |node| {
            if aggregate_function(node).is_some() {
                nested = true;
            }
            true
        });
    }
    if nested {
        return Err(Error::Plan(format!("nested aggregate in `{function}`")));
    }
    Ok(AggregateCall {
        key: function.to_string(),
        name,
        distinct,
        arg,
        separator,
    })
}

enum Acc {
    Count(i64),
    Sum(Option<Value>),
    Avg {
        sum: f64,
        count: i64,
    },
    Min(Option<Value>),
    Max(Option<Value>),
    Concat {
        parts: Vec<String>,
        separator: Option<String>,
    },
}

struct Accumulator {
    acc: Acc,
    seen: Option<HashSet<String>>,
}

impl Accumulator {
    fn new(call: &AggregateCall) -> Self {
        let acc = match call.name.as_str() {
            "count" => Acc::Count(0),
            "sum" => Acc::Sum(None),
            "avg" => Acc::Avg { sum: 0.0, count: 0 },
            "min" => Acc::Min(None),
            "max" => Acc::Max(None),
            _ => Acc::Concat {
                parts: Vec::new(),
                separator: None,
            },
        };
        Self {
            acc,
            seen: call.distinct.then(HashSet::new),
        }
    }

    fn push(&mut self, value: Option<Value>, separator: Option<Value>) -> Result<()> {
        if let Some(seen) = &mut self.seen
            && let Some(value) = &value
            && !seen.insert(format!("{value:?}"))
        {
            return Ok(());
        }
        match (&mut self.acc, value) {
            (Acc::Count(n), None) => *n += 1,
            (Acc::Count(n), Some(value)) => {
                if !value.is_null() {
                    *n += 1;
                }
            }
            (_, None) => {}
            (_, Some(Value::Null)) => {}
            (Acc::Sum(total), Some(value)) => {
                *total = Some(match (total.take(), value) {
                    (None, Value::Int(n)) => Value::Int(n),
                    (None, Value::Float(f)) => Value::Float(f),
                    (Some(Value::Int(a)), Value::Int(b)) => match a.checked_add(b) {
                        Some(sum) => Value::Int(sum),
                        None => Value::Float(a as f64 + b as f64),
                    },
                    (Some(Value::Int(a)), Value::Float(b)) => Value::Float(a as f64 + b),
                    (Some(Value::Float(a)), Value::Int(b)) => Value::Float(a + b as f64),
                    (Some(Value::Float(a)), Value::Float(b)) => Value::Float(a + b),
                    (_, other) => {
                        return Err(Error::TypeMismatch {
                            operation: "sum".to_owned(),
                            left: "number".to_owned(),
                            right: type_name(&other),
                        });
                    }
                });
            }
            (Acc::Avg { sum, count }, Some(value)) => {
                *sum += match value {
                    Value::Int(n) => n as f64,
                    Value::Float(f) => f,
                    other => {
                        return Err(Error::TypeMismatch {
                            operation: "avg".to_owned(),
                            left: "number".to_owned(),
                            right: type_name(&other),
                        });
                    }
                };
                *count += 1;
            }
            (Acc::Min(current), Some(value)) => {
                let replace = match current {
                    None => true,
                    Some(existing) => value.compare(existing) == Some(Ordering::Less),
                };
                if replace {
                    *current = Some(value);
                }
            }
            (Acc::Max(current), Some(value)) => {
                let replace = match current {
                    None => true,
                    Some(existing) => value.compare(existing) == Some(Ordering::Greater),
                };
                if replace {
                    *current = Some(value);
                }
            }
            (
                Acc::Concat {
                    parts,
                    separator: sep,
                },
                Some(value),
            ) => {
                if sep.is_none() {
                    *sep = separator.map(|s| render(&s));
                }
                parts.push(render(&value));
            }
        }
        Ok(())
    }

    fn finish(self) -> Value {
        match self.acc {
            Acc::Count(n) => Value::Int(n),
            Acc::Sum(total) => total.unwrap_or(Value::Null),
            Acc::Avg { sum, count } => {
                if count == 0 {
                    Value::Null
                } else {
                    Value::Float(sum / count as f64)
                }
            }
            Acc::Min(value) | Acc::Max(value) => value.unwrap_or(Value::Null),
            Acc::Concat { parts, separator } => {
                if parts.is_empty() {
                    Value::Null
                } else {
                    Value::Text(parts.join(separator.as_deref().unwrap_or(",")))
                }
            }
        }
    }
}

fn type_name(value: &Value) -> String {
    match value.type_of() {
        None => "null".to_owned(),
        Some(t) => format!("{t:?}").to_ascii_lowercase(),
    }
}

struct Bound<'a> {
    alias: String,
    columns: Vec<String>,
    rows: RowStream<'a>,
}

fn bind<'a>(
    relation: &'a Relation,
    ctx: &Rc<Context>,
    scope: &Scope,
    outer: Option<&'a dyn Row>,
) -> Result<Bound<'a>> {
    match relation {
        Relation::Table { source, alias } => Ok(Bound {
            alias: alias.clone(),
            columns: source
                .table
                .columns()
                .iter()
                .map(|c| (*c).to_owned())
                .collect(),
            rows: base_rows(source)?,
        }),
        Relation::Cte { name, alias } => {
            let set = scope
                .get(name)
                .cloned()
                .ok_or_else(|| Error::UnknownTable(name.clone()))?;
            Ok(Bound {
                alias: alias.clone(),
                columns: set.headers.clone(),
                rows: Box::new(set_rows(&set).into_iter().map(Ok)),
            })
        }
        Relation::Derived { query, alias } => {
            let set = run_query(query, ctx, scope, outer)?;
            Ok(Bound {
                alias: alias.clone(),
                columns: set.headers.clone(),
                rows: Box::new(set_rows(&set).into_iter().map(Ok)),
            })
        }
    }
}

struct Prepared {
    headers: Vec<String>,
    exprs: Vec<Expr>,
}

fn prepare(plan: &SelectPlan, relations: &[(String, Vec<String>)]) -> Result<Prepared> {
    let mut headers = Vec::new();
    let mut exprs = Vec::new();
    for projection in &plan.projection {
        match projection {
            Projection::Wildcard(qualifier) => {
                if relations.is_empty() {
                    return Err(Error::Plan("SELECT * needs a FROM clause".to_owned()));
                }
                let mut matched = false;
                for (alias, columns) in relations {
                    if qualifier.as_ref().is_some_and(|q| q != alias) {
                        continue;
                    }
                    matched = true;
                    for name in columns {
                        headers.push(name.clone());
                        exprs.push(Expr::CompoundIdentifier(vec![
                            Ident::new(alias.clone()),
                            Ident::new(name.clone()),
                        ]));
                    }
                }
                if !matched {
                    return Err(Error::UnknownTable(qualifier.clone().unwrap_or_default()));
                }
            }
            Projection::Expr { expr, name } => {
                headers.push(name.clone());
                exprs.push((**expr).clone());
            }
        }
    }
    Ok(Prepared { headers, exprs })
}

fn skippable(error: &Error) -> bool {
    matches!(error, Error::Io { .. })
}

fn passes(evaluator: &mut Evaluator, filter: Option<&Expr>, row: &dyn Row) -> Result<bool> {
    match filter {
        None => Ok(true),
        Some(filter) => Ok(evaluator.eval(filter, row)?.truth() == Some(true)),
    }
}

enum OrderSource {
    Projected(usize),
    Expr(Box<Expr>),
}

fn order_sources(order_by: &[OrderKey], prepared: &Prepared) -> Result<Vec<OrderSource>> {
    order_by
        .iter()
        .map(|key| {
            if let Expr::Identifier(ident) = &key.expr
                && let Some(index) = prepared
                    .headers
                    .iter()
                    .position(|h| h.eq_ignore_ascii_case(&ident.value))
            {
                return Ok(OrderSource::Projected(index));
            }
            if let Expr::Value(literal) = &key.expr
                && let Literal::Number(text, _) = &literal.value
            {
                let position: usize = text.parse().map_err(|_| {
                    Error::Plan(format!("ORDER BY position `{text}` is not a number"))
                })?;
                if position == 0 || position > prepared.headers.len() {
                    return Err(Error::Plan(format!(
                        "ORDER BY position {position} is out of range"
                    )));
                }
                return Ok(OrderSource::Projected(position - 1));
            }
            Ok(OrderSource::Expr(Box::new(key.expr.clone())))
        })
        .collect()
}

fn order_values(
    evaluator: &mut Evaluator,
    sources: &[OrderSource],
    projected: &[Value],
    row: &dyn Row,
) -> Result<Vec<Value>> {
    sources
        .iter()
        .map(|source| match source {
            OrderSource::Projected(index) => Ok(projected[*index].clone()),
            OrderSource::Expr(expr) => evaluator.eval(expr, row),
        })
        .collect()
}

pub fn compare_keys(a: &[Value], b: &[Value], keys: &[OrderKey]) -> Ordering {
    for (index, key) in keys.iter().enumerate() {
        let (x, y) = (&a[index], &b[index]);
        let ordering = match (x.is_null(), y.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) => {
                if key.nulls_first {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (false, true) => {
                if key.nulls_first {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            (false, false) => {
                let natural = x.compare(y).unwrap_or(Ordering::Equal);
                if key.descending {
                    natural.reverse()
                } else {
                    natural
                }
            }
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

fn finish(
    order_by: &[OrderKey],
    limit: Option<usize>,
    offset: usize,
    distinct: bool,
    headers: Vec<String>,
    mut collected: Vec<(Vec<Value>, Vec<Value>)>,
) -> ResultSet {
    if !order_by.is_empty() {
        collected.sort_by(|a, b| compare_keys(&a.0, &b.0, order_by));
    }
    let mut rows: Vec<Vec<Value>> = Vec::with_capacity(collected.len());
    let mut seen: HashSet<String> = HashSet::new();
    for (_, projected) in collected {
        if distinct && !seen.insert(format!("{projected:?}")) {
            continue;
        }
        rows.push(projected);
    }
    let rows: Vec<Vec<Value>> = rows
        .into_iter()
        .skip(offset)
        .take(limit.unwrap_or(usize::MAX))
        .collect();
    ResultSet { headers, rows }
}

fn bare_names(names: &[String], alias: &str, columns: &[String]) -> HashSet<String> {
    let mut out: HashSet<String> = columns.iter().cloned().collect();
    for name in names {
        match name.split_once('.') {
            Some((qualifier, column)) if qualifier == alias => {
                out.insert(column.to_owned());
            }
            Some(_) => {}
            None => {
                out.insert(name.clone());
            }
        }
    }
    out
}

fn snapshot<'a>(row: &dyn Row, names: &HashSet<String>) -> Result<Rc<dyn Row + 'a>> {
    let mut map = HashMap::new();
    for name in names {
        match row.column(name) {
            Ok(value) => {
                map.insert(name.clone(), value);
            }
            Err(Error::UnknownColumn(_)) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(Rc::new(MapRow(map)))
}

type Materialized<'a> = (Vec<Rc<dyn Row + 'a>>, HashSet<String>);

fn materialize<'a>(bound: Bound<'a>, names: &[String], ctx: &Context) -> Result<Materialized<'a>> {
    let wanted = bare_names(names, &bound.alias, &bound.columns);
    let mut rows = Vec::new();
    for row in bound.rows {
        match row.and_then(|row| snapshot(&*row, &wanted)) {
            Ok(row) => rows.push(row),
            Err(error) if skippable(&error) => report(ctx, error),
            Err(error) => return Err(error),
        }
    }
    Ok((rows, bound.columns.into_iter().collect()))
}

fn join<'a>(
    left: Vec<JoinRow<'a>>,
    left_aliases: &[(String, HashSet<String>)],
    spec: &JoinSpec,
    right: Vec<Rc<dyn Row + 'a>>,
    right_names: &HashSet<String>,
    evaluator: &mut Evaluator,
) -> Result<Vec<JoinRow<'a>>> {
    let alias = spec.relation.alias();
    let mut out = Vec::new();
    let mut right_matched = vec![false; right.len()];
    for l in &left {
        let mut matched = false;
        for (index, r) in right.iter().enumerate() {
            let candidate = l.extend(alias, r.clone(), &spec.using);
            let ok = if spec.kind == JoinKind::Cross {
                true
            } else if let Some(on) = &spec.on {
                evaluator.eval(on, &candidate)?.truth() == Some(true)
            } else {
                let mut all = true;
                for column in &spec.using {
                    let a = l.column(column)?;
                    let b = r.column(column)?;
                    if a.is_null() || b.is_null() || a.compare(&b) != Some(Ordering::Equal) {
                        all = false;
                        break;
                    }
                }
                all
            };
            if ok {
                out.push(candidate);
                matched = true;
                right_matched[index] = true;
            }
        }
        if !matched && matches!(spec.kind, JoinKind::Left | JoinKind::Full) {
            out.push(l.extend(
                alias,
                Rc::new(NullRow {
                    names: right_names.clone(),
                }),
                &spec.using,
            ));
        }
    }
    if matches!(spec.kind, JoinKind::Right | JoinKind::Full) {
        let outer = left.first().and_then(|l| l.outer);
        for (index, r) in right.iter().enumerate() {
            if right_matched[index] {
                continue;
            }
            let mut row = JoinRow {
                parts: Vec::new(),
                merged: spec.using.clone(),
                outer,
            };
            for (left_alias, names) in left_aliases {
                row.parts.push((
                    left_alias.clone(),
                    Rc::new(NullRow {
                        names: names.clone(),
                    }),
                ));
            }
            row.parts.push((alias.to_owned(), r.clone()));
            out.push(row);
        }
    }
    Ok(out)
}

fn select_rows(
    plan: &SelectPlan,
    order_by: &[OrderKey],
    limit: Option<usize>,
    offset: usize,
    ctx: &Rc<Context>,
    scope: &Scope,
    outer: Option<&dyn Row>,
) -> Result<ResultSet> {
    let mut all_exprs: Vec<&Expr> = Vec::new();
    for projection in &plan.projection {
        if let Projection::Expr { expr, .. } = projection {
            all_exprs.push(expr);
        }
    }
    if let Some(filter) = &plan.filter {
        all_exprs.push(filter);
    }
    all_exprs.extend(plan.group_by.iter());
    if let Some(having) = &plan.having {
        all_exprs.push(having);
    }
    for key in order_by {
        all_exprs.push(&key.expr);
    }
    for clause in &plan.from {
        for join in &clause.joins {
            if let Some(on) = &join.on {
                all_exprs.push(on);
            }
        }
    }
    let names = referenced_columns(&all_exprs);
    let mut evaluator = evaluator(ctx, scope);
    let mut relations: Vec<(String, Vec<String>)> = Vec::new();
    let streaming = plan.from.len() == 1 && plan.from[0].joins.is_empty();
    let rows: Box<dyn Iterator<Item = Result<JoinRow<'_>>> + '_> = if plan.from.is_empty() {
        Box::new(std::iter::once(Ok(JoinRow::single(
            "",
            Box::new(EmptyRow),
            outer,
        ))))
    } else if streaming {
        let bound = bind(&plan.from[0].relation, ctx, scope, outer)?;
        relations.push((bound.alias.clone(), bound.columns.clone()));
        let alias = bound.alias;
        Box::new(
            bound
                .rows
                .map(move |row| row.map(|row| JoinRow::single(&alias, row, outer))),
        )
    } else {
        let mut product: Vec<JoinRow<'_>> = Vec::new();
        let mut first = true;
        for clause in &plan.from {
            let bound = bind(&clause.relation, ctx, scope, outer)?;
            relations.push((bound.alias.clone(), bound.columns.clone()));
            let alias = bound.alias.clone();
            let (base, base_names) = materialize(bound, &names, ctx)?;
            let mut aliases: Vec<(String, HashSet<String>)> = vec![(alias.clone(), base_names)];
            let mut current: Vec<JoinRow<'_>> = base
                .into_iter()
                .map(|row| JoinRow {
                    parts: vec![(alias.clone(), row)],
                    merged: Vec::new(),
                    outer,
                })
                .collect();
            for spec in &clause.joins {
                let bound = bind(&spec.relation, ctx, scope, outer)?;
                relations.push((bound.alias.clone(), bound.columns.clone()));
                let right_alias = bound.alias.clone();
                let (right, right_names) = materialize(bound, &names, ctx)?;
                current = join(current, &aliases, spec, right, &right_names, &mut evaluator)?;
                aliases.push((right_alias, right_names));
            }
            if first {
                product = current;
                first = false;
            } else {
                let mut combined = Vec::new();
                for l in &product {
                    for r in &current {
                        let mut row = JoinRow {
                            parts: l.parts.clone(),
                            merged: l.merged.clone(),
                            outer,
                        };
                        row.parts.extend(r.parts.iter().cloned());
                        row.merged.extend(r.merged.iter().cloned());
                        combined.push(row);
                    }
                }
                product = combined;
            }
        }
        Box::new(product.into_iter().map(Ok))
    };
    let prepared = prepare(plan, &relations)?;
    let mut projected_exprs: Vec<&Expr> = prepared.exprs.iter().collect();
    if let Some(having) = &plan.having {
        projected_exprs.push(having);
    }
    for key in order_by {
        projected_exprs.push(&key.expr);
    }
    let calls = aggregate_calls(&projected_exprs)?;
    if let Some(filter) = &plan.filter
        && !aggregate_calls(&[filter])?.is_empty()
    {
        return Err(Error::Plan(
            "aggregates are not allowed in WHERE".to_owned(),
        ));
    }
    let grouped = !calls.is_empty() || !plan.group_by.is_empty();
    let paging = Paging {
        order_by,
        limit,
        offset,
    };
    if grouped {
        select_grouped(plan, prepared, calls, rows, &paging, &mut evaluator, ctx)
    } else {
        select_simple(plan, prepared, rows, &paging, &mut evaluator, ctx)
    }
}

struct Paging<'a> {
    order_by: &'a [OrderKey],
    limit: Option<usize>,
    offset: usize,
}

fn select_simple<'a>(
    plan: &SelectPlan,
    prepared: Prepared,
    rows: Box<dyn Iterator<Item = Result<JoinRow<'a>>> + 'a>,
    paging: &Paging<'_>,
    evaluator: &mut Evaluator,
    ctx: &Context,
) -> Result<ResultSet> {
    let Paging {
        order_by,
        limit,
        offset,
    } = *paging;
    let order = order_sources(order_by, &prepared)?;
    let can_stop_early = order_by.is_empty() && !plan.distinct;
    let cap = limit.map(|limit| limit.saturating_add(offset));
    let mut collected: Vec<(Vec<Value>, Vec<Value>)> = Vec::new();
    for row in rows {
        let row = match row {
            Ok(row) => row,
            Err(error) if skippable(&error) => {
                report(ctx, error);
                continue;
            }
            Err(error) => return Err(error),
        };
        let step = (|| -> Result<Option<(Vec<Value>, Vec<Value>)>> {
            if !passes(evaluator, plan.filter.as_ref(), &row)? {
                return Ok(None);
            }
            let projected = prepared
                .exprs
                .iter()
                .map(|expr| evaluator.eval(expr, &row))
                .collect::<Result<Vec<Value>>>()?;
            let keys = order_values(evaluator, &order, &projected, &row)?;
            Ok(Some((keys, projected)))
        })();
        match step {
            Ok(Some(item)) => collected.push(item),
            Ok(None) => {}
            Err(error) if skippable(&error) => report(ctx, error),
            Err(error) => return Err(error),
        }
        if can_stop_early && cap.is_some_and(|cap| collected.len() >= cap) {
            break;
        }
    }
    Ok(finish(
        order_by,
        limit,
        offset,
        plan.distinct,
        prepared.headers,
        collected,
    ))
}

struct Group {
    snapshot: HashMap<String, Value>,
    accumulators: Vec<Accumulator>,
}

fn select_grouped<'a>(
    plan: &SelectPlan,
    prepared: Prepared,
    calls: Vec<AggregateCall>,
    rows: Box<dyn Iterator<Item = Result<JoinRow<'a>>> + 'a>,
    paging: &Paging<'_>,
    evaluator: &mut Evaluator,
    ctx: &Context,
) -> Result<ResultSet> {
    let Paging {
        order_by,
        limit,
        offset,
    } = *paging;
    let order = order_sources(order_by, &prepared)?;
    let mut referenced_exprs: Vec<&Expr> = prepared.exprs.iter().collect();
    if let Some(having) = &plan.having {
        referenced_exprs.push(having);
    }
    for key in order_by {
        referenced_exprs.push(&key.expr);
    }
    let referenced = referenced_columns(&referenced_exprs);
    let mut groups: Vec<Group> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for row in rows {
        let row = match row {
            Ok(row) => row,
            Err(error) if skippable(&error) => {
                report(ctx, error);
                continue;
            }
            Err(error) => return Err(error),
        };
        let step = (|| -> Result<()> {
            if !passes(evaluator, plan.filter.as_ref(), &row)? {
                return Ok(());
            }
            let key_values = plan
                .group_by
                .iter()
                .map(|expr| evaluator.eval(expr, &row))
                .collect::<Result<Vec<Value>>>()?;
            let key = format!("{key_values:?}");
            let position = match index.get(&key) {
                Some(position) => *position,
                None => {
                    let mut snapshot = HashMap::new();
                    for name in &referenced {
                        match row.column(name) {
                            Ok(value) => {
                                snapshot.insert(name.clone(), value);
                            }
                            Err(Error::UnknownColumn(_)) => {}
                            Err(error) => return Err(error),
                        }
                    }
                    groups.push(Group {
                        snapshot,
                        accumulators: calls.iter().map(Accumulator::new).collect(),
                    });
                    index.insert(key, groups.len() - 1);
                    groups.len() - 1
                }
            };
            for (call, accumulator) in calls.iter().zip(&mut groups[position].accumulators) {
                let value = match &call.arg {
                    None => None,
                    Some(arg) => Some(evaluator.eval(arg, &row)?),
                };
                let separator = match &call.separator {
                    None => None,
                    Some(sep) => Some(evaluator.eval(sep, &row)?),
                };
                accumulator.push(value, separator)?;
            }
            Ok(())
        })();
        match step {
            Ok(()) => {}
            Err(error) if skippable(&error) => report(ctx, error),
            Err(error) => return Err(error),
        }
    }
    if groups.is_empty() && plan.group_by.is_empty() {
        groups.push(Group {
            snapshot: HashMap::new(),
            accumulators: calls.iter().map(Accumulator::new).collect(),
        });
    }
    let mut collected: Vec<(Vec<Value>, Vec<Value>)> = Vec::new();
    for group in groups {
        let row = MapRow(group.snapshot);
        evaluator.aggregates = calls
            .iter()
            .zip(group.accumulators)
            .map(|(call, accumulator)| (call.key.clone(), accumulator.finish()))
            .collect();
        if !passes(evaluator, plan.having.as_ref(), &row)? {
            continue;
        }
        let projected = prepared
            .exprs
            .iter()
            .map(|expr| evaluator.eval(expr, &row))
            .collect::<Result<Vec<Value>>>()?;
        let keys = order_values(evaluator, &order, &projected, &row)?;
        collected.push((keys, projected));
    }
    Ok(finish(
        order_by,
        limit,
        offset,
        plan.distinct,
        prepared.headers,
        collected,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::Plan;
    use crate::walk::WalkOptions;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fsql-exec-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).expect("dirs");
        std::fs::create_dir_all(dir.join("build")).expect("dirs");
        std::fs::write(dir.join("src/main.rs"), b"fn main() {}").expect("file");
        std::fs::write(dir.join("src/lib.rs"), b"pub fn f() {}\n").expect("file");
        std::fs::write(dir.join("build/out.tmp"), vec![0u8; 3000]).expect("file");
        std::fs::write(dir.join("build/cache.tmp"), vec![0u8; 1000]).expect("file");
        std::fs::write(dir.join("README"), b"hi").expect("file");
        std::fs::canonicalize(dir).expect("canonical")
    }

    fn run_sql(dir: &std::path::Path, sql: &str) -> Result<ResultSet> {
        let planner = Planner::new(dir, WalkOptions::default());
        let mut plans = planner.plan(sql)?;
        let Plan::Select(plan) = plans.remove(0) else {
            panic!("expected select");
        };
        let mut errors = Vec::new();
        let result = run(&plan, &planner, &mut |e| errors.push(e))?;
        assert!(errors.is_empty(), "{errors:?}");
        Ok(result)
    }

    fn texts(set: &ResultSet, column: usize) -> Vec<String> {
        set.rows.iter().map(|r| render(&r[column])).collect()
    }

    #[test]
    fn filters_projects_and_orders() {
        let dir = fixture("basic");
        let set = run_sql(
            &dir,
            "select name, size from files where kind = 'file' order by size desc, name",
        )
        .expect("run");
        assert_eq!(set.headers, ["name", "size"]);
        assert_eq!(
            texts(&set, 0),
            ["out.tmp", "cache.tmp", "lib.rs", "main.rs", "README"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn byte_literals_and_globs_work_end_to_end() {
        let dir = fixture("globs");
        let set = run_sql(
            &dir,
            "select path from files where size > 2k and path glob '*.tmp'",
        )
        .expect("run");
        assert_eq!(
            texts(&set, 0),
            [dir.join("build/out.tmp").to_string_lossy().into_owned()]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn limit_and_offset_apply_after_ordering() {
        let dir = fixture("limit");
        let set = run_sql(
            &dir,
            "select name from files where kind = 'file' order by name limit 2 offset 1",
        )
        .expect("run");
        assert_eq!(texts(&set, 0), ["cache.tmp", "lib.rs"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn group_by_with_aggregates_and_having() {
        let dir = fixture("group");
        let set = run_sql(
            &dir,
            "select ext, count(*) as n, sum(size) as bytes, max(size) from files where kind = 'file' group by ext having count(*) > 1 order by n desc, ext",
        )
        .expect("run");
        assert_eq!(set.headers, ["ext", "n", "bytes", "max(size)"]);
        assert_eq!(set.rows.len(), 2);
        assert_eq!(set.rows[0][0], Value::Text("rs".to_owned()));
        assert_eq!(set.rows[0][1], Value::Int(2));
        assert_eq!(set.rows[0][2], Value::Int(26));
        assert_eq!(set.rows[1][0], Value::Text("tmp".to_owned()));
        assert_eq!(set.rows[1][2], Value::Int(4000));
        assert_eq!(set.rows[1][3], Value::Int(3000));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn aggregate_without_group_by_yields_one_row_even_when_empty() {
        let dir = fixture("empty-agg");
        let set = run_sql(
            &dir,
            "select count(*), sum(size), avg(size) from files where ext = 'nope'",
        )
        .expect("run");
        assert_eq!(
            set.rows,
            vec![vec![Value::Int(0), Value::Null, Value::Null]]
        );
        let set = run_sql(&dir, "select count(*), count(distinct ext), group_concat(distinct ext, '|') from files where kind = 'file'").expect("run");
        assert_eq!(set.rows[0][0], Value::Int(5));
        assert_eq!(set.rows[0][1], Value::Int(2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn distinct_and_wildcard() {
        let dir = fixture("distinct");
        let set = run_sql(&dir, "select distinct kind from files order by kind").expect("run");
        assert_eq!(texts(&set, 0), ["dir", "file"]);
        let set = run_sql(&dir, "select * from files where name = 'README'").expect("run");
        assert_eq!(set.headers.len(), Table::Files.columns().len());
        assert_eq!(set.rows.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn select_without_from_evaluates_constants() {
        let dir = fixture("constants");
        let set = run_sql(&dir, "select 1 + 1 as two, upper('x')").expect("run");
        assert_eq!(
            set.rows,
            vec![vec![Value::Int(2), Value::Text("X".to_owned())]]
        );
        assert!(matches!(run_sql(&dir, "select * "), Err(Error::Plan(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn type_errors_abort_instead_of_skipping() {
        let dir = fixture("abort");
        assert!(matches!(
            run_sql(&dir, "select name from files where size > 'big'"),
            Err(Error::TypeMismatch { .. })
        ));
        assert!(matches!(
            run_sql(&dir, "select name from files where count(*) > 1"),
            Err(Error::Plan(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn order_by_alias_and_position() {
        let dir = fixture("alias");
        let set = run_sql(
            &dir,
            "select name as n, size from files where kind = 'file' order by 2 desc, n limit 1",
        )
        .expect("run");
        assert_eq!(texts(&set, 0), ["out.tmp"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn aliases_qualify_columns() {
        let dir = fixture("qualify");
        let set = run_sql(
            &dir,
            "select f.name from files f where f.ext = 'rs' order by f.name",
        )
        .expect("run");
        assert_eq!(texts(&set, 0), ["lib.rs", "main.rs"]);
        let set = run_sql(&dir, "select f.* from files as f where name = 'README'").expect("run");
        assert_eq!(set.headers.len(), Table::Files.columns().len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn inner_left_right_and_cross_joins() {
        let dir = fixture("joins");
        let set = run_sql(
            &dir,
            "select a.name, b.name from files a join files b on a.parent = b.parent and a.name < b.name where a.kind = 'file' and b.kind = 'file' order by 1, 2",
        )
        .expect("run");
        assert_eq!(texts(&set, 0), ["cache.tmp", "lib.rs"]);
        assert_eq!(texts(&set, 1), ["out.tmp", "main.rs"]);
        let set = run_sql(
            &dir,
            "select d.name, count(f.path) as n from files d left join files f on f.parent = d.path and f.kind = 'file' where d.kind = 'dir' and d.depth > 0 group by d.name order by d.name",
        )
        .expect("run");
        assert_eq!(texts(&set, 0)[0..2], ["build".to_owned(), "src".to_owned()]);
        assert_eq!(set.rows[0][1], Value::Int(2));
        let set = run_sql(
            &dir,
            "select f.name, m.fstype from files f join mounts m on f.dev = m.dev where f.name = 'README'",
        )
        .expect("run");
        assert_eq!(set.rows.len(), 1);
        assert!(matches!(&set.rows[0][1], Value::Text(t) if !t.is_empty()));
        let set = run_sql(&dir, "select count(*) from (select 1 as x union all select 2) a cross join (select 1 as y union all select 2 union all select 3) b").expect("run");
        assert_eq!(set.rows[0][0], Value::Int(6));
        let set = run_sql(
            &dir,
            "select l.x, r.y from (select 1 as x union all select 2) l right join (select 2 as y union all select 3) r on l.x = r.y order by r.y",
        )
        .expect("run");
        assert_eq!(
            set.rows,
            vec![
                vec![Value::Int(2), Value::Int(2)],
                vec![Value::Null, Value::Int(3)]
            ]
        );
        let set = run_sql(
            &dir,
            "select x, y from (select 1 as x union all select 2) l full join (select 2 as y union all select 3) r on x = y order by coalesce(x, y)",
        )
        .expect("run");
        assert_eq!(set.rows.len(), 3);
        let set = run_sql(
            &dir,
            "select name, size from (select name, parent from files where kind = 'file') a join (select name, size from files) b using (name) order by size desc limit 1",
        )
        .expect("run");
        assert_eq!(texts(&set, 0), ["out.tmp"]);
        assert!(matches!(
            run_sql(
                &dir,
                "select name from files a join files b on a.path = b.path"
            ),
            Err(Error::Plan(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn subqueries_scalar_in_exists_any_and_correlated() {
        let dir = fixture("subqueries");
        let set = run_sql(
            &dir,
            "select name from files where size = (select max(size) from files)",
        )
        .expect("run");
        assert_eq!(texts(&set, 0), ["out.tmp"]);
        let set = run_sql(&dir, "select name from files where ext in (select ext from files where size > 2k) order by name").expect("run");
        assert_eq!(texts(&set, 0), ["cache.tmp", "out.tmp"]);
        let set = run_sql(
            &dir,
            "select d.name from files d where d.kind = 'dir' and exists (select 1 from files f where f.parent = d.path and f.ext = 'rs')",
        )
        .expect("run");
        assert_eq!(texts(&set, 0), ["src"]);
        let set = run_sql(&dir, "select name from files where size > all (select size from files where ext = 'rs') and kind = 'file' order by name").expect("run");
        assert_eq!(texts(&set, 0), ["cache.tmp", "out.tmp"]);
        let set = run_sql(
            &dir,
            "select name, (select count(*) from files c where c.parent = p.path) as children from files p where p.kind = 'dir' order by name",
        )
        .expect("run");
        assert_eq!(
            set.rows[0],
            vec![Value::Text("build".to_owned()), Value::Int(2)]
        );
        assert_eq!(set.rows[1][1], Value::Int(3));
        assert!(matches!(
            run_sql(&dir, "select (select size from files) from files limit 1"),
            Err(Error::Plan(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ctes_plain_and_recursive() {
        let dir = fixture("ctes");
        let set = run_sql(
            &dir,
            "with big as (select name, size from files where size > 2k), small as (select name from files where size < 20 and kind = 'file') select name from big union all select name from small order by name",
        )
        .expect("run");
        assert_eq!(texts(&set, 0), ["README", "lib.rs", "main.rs", "out.tmp"]);
        let set = run_sql(
            &dir,
            "with recursive n(x) as (select 1 union all select x + 1 from n where x < 5) select sum(x) from n",
        )
        .expect("run");
        assert_eq!(set.rows[0][0], Value::Int(15));
        let set = run_sql(
            &dir,
            &format!(
                "with recursive up(path, depth) as (select path, 0 from files where name = 'main.rs' union all select dirname(up.path), depth + 1 from up where up.path <> '{}') select max(depth) from up",
                dir.display()
            ),
        )
        .expect("run");
        assert_eq!(set.rows[0][0], Value::Int(2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_operations() {
        let dir = fixture("setops");
        let set = run_sql(
            &dir,
            "select 1 union select 1 union all select 2 order by 1",
        )
        .expect("run");
        assert_eq!(set.rows, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
        let set = run_sql(
            &dir,
            "select 1 union all select 1 union all select 2 order by 1",
        )
        .expect("run");
        assert_eq!(set.rows.len(), 3);
        let set = run_sql(
            &dir,
            "select ext from files where kind = 'file' intersect select 'rs' as e",
        )
        .expect("run");
        assert_eq!(set.rows, vec![vec![Value::Text("rs".to_owned())]]);
        let set = run_sql(
            &dir,
            "select ext from files where kind = 'file' except select 'rs' order by 1",
        )
        .expect("run");
        assert_eq!(
            set.rows,
            vec![vec![Value::Text("tmp".to_owned())], vec![Value::Null]]
        );
        let set = run_sql(
            &dir,
            "select * from (values (1, 'a'), (2, 'b')) v order by column1 desc limit 1",
        )
        .expect("run");
        assert_eq!(
            set.rows,
            vec![vec![Value::Int(2), Value::Text("b".to_owned())]]
        );
        assert!(matches!(
            run_sql(&dir, "select 1 union select 1, 2"),
            Err(Error::Plan(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
