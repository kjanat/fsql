use std::path::{Path, PathBuf};

use sqlparser::ast::{
    Assignment, AssignmentTarget, BinaryOperator, Delete, Distinct, Expr, FromTable, FunctionArg,
    FunctionArgExpr, GroupByExpr, Insert, JoinConstraint, JoinOperator, LimitClause, OrderByKind,
    OrderBySort, Query, Select, SelectItem, SetExpr, SetOperator, SetQuantifier, Statement,
    TableFactor, TableObject, TableWithJoins, Update, Value as Literal,
};
use sqlparser::parser::Parser;

use crate::column::{Column, Table};
use crate::dialect::FsqlDialect;
use crate::error::{Error, Result};
use crate::eval::{Evaluator, Row};
use crate::value::Value;
use crate::walk::WalkOptions;

#[derive(Debug, Clone)]
pub struct Source {
    pub table: Table,
    pub root: PathBuf,
    pub options: WalkOptions,
}

#[derive(Debug, Clone)]
pub enum Relation {
    Table {
        source: Source,
        alias: String,
    },
    Cte {
        name: String,
        alias: String,
    },
    Derived {
        query: Box<QueryPlan>,
        alias: String,
    },
}

impl Relation {
    pub fn alias(&self) -> &str {
        match self {
            Self::Table { alias, .. } | Self::Cte { alias, .. } | Self::Derived { alias, .. } => {
                alias
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
    Cross,
}

#[derive(Debug, Clone)]
pub struct JoinSpec {
    pub relation: Relation,
    pub kind: JoinKind,
    pub on: Option<Expr>,
    pub using: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct FromClause {
    pub relation: Relation,
    pub joins: Vec<JoinSpec>,
}

#[derive(Debug, Clone)]
pub enum Projection {
    Wildcard(Option<String>),
    Expr { expr: Box<Expr>, name: String },
}

#[derive(Debug, Clone)]
pub struct OrderKey {
    pub expr: Expr,
    pub descending: bool,
    pub nulls_first: bool,
}

#[derive(Debug, Clone)]
pub struct SelectPlan {
    pub from: Vec<FromClause>,
    pub projection: Vec<Projection>,
    pub filter: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
    pub distinct: bool,
}

impl SelectPlan {
    pub fn single_table(&self) -> Option<&Source> {
        match self.from.as_slice() {
            [
                FromClause {
                    relation: Relation::Table { source, .. },
                    joins,
                },
            ] if joins.is_empty() => Some(source),
            _ => None,
        }
    }

    pub fn relations(&self) -> Vec<&Relation> {
        let mut out = Vec::new();
        for clause in &self.from {
            out.push(&clause.relation);
            for join in &clause.joins {
                out.push(&join.relation);
            }
        }
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOp {
    Union,
    Intersect,
    Except,
}

#[derive(Debug, Clone)]
pub enum SetBody {
    Select(Box<SelectPlan>),
    Values(Vec<Vec<Expr>>),
    Op {
        left: Box<SetBody>,
        op: SetOp,
        all: bool,
        right: Box<SetBody>,
    },
}

#[derive(Debug, Clone)]
pub struct Cte {
    pub name: String,
    pub columns: Vec<String>,
    pub query: QueryPlan,
}

#[derive(Debug, Clone)]
pub struct QueryPlan {
    pub ctes: Vec<Cte>,
    pub recursive: bool,
    pub body: SetBody,
    pub order_by: Vec<OrderKey>,
    pub limit: Option<usize>,
    pub offset: usize,
}

#[derive(Debug, Clone)]
pub struct DeletePlan {
    pub source: Source,
    pub alias: String,
    pub filter: Expr,
    pub order_by: Vec<OrderKey>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct Set {
    pub column: Column,
    pub value: Expr,
}

#[derive(Debug, Clone)]
pub struct UpdatePlan {
    pub source: Source,
    pub alias: String,
    pub assignments: Vec<Set>,
    pub filter: Expr,
    pub order_by: Vec<OrderKey>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertColumn {
    Column(Column),
    Source,
    Content,
}

impl InsertColumn {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "source" => Some(Self::Source),
            "content" => Some(Self::Content),
            other => Column::parse(other).map(Self::Column),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Column(column) => column.name(),
            Self::Source => "source",
            Self::Content => "content",
        }
    }
}

#[derive(Debug, Clone)]
pub enum InsertRows {
    Values(Vec<Vec<Expr>>),
    Query(Box<QueryPlan>),
}

#[derive(Debug, Clone)]
pub struct InsertPlan {
    pub table: Table,
    pub columns: Vec<InsertColumn>,
    pub rows: InsertRows,
}

#[derive(Debug, Clone)]
pub enum Plan {
    Select(Box<QueryPlan>),
    Delete(DeletePlan),
    Update(UpdatePlan),
    Insert(InsertPlan),
}

impl Plan {
    pub fn is_mutation(&self) -> bool {
        !matches!(self, Self::Select(_))
    }

    pub fn verb(&self) -> &'static str {
        match self {
            Self::Select(_) => "SELECT",
            Self::Delete(_) => "DELETE",
            Self::Update(_) => "UPDATE",
            Self::Insert(_) => "INSERT",
        }
    }
}

pub const ASSIGNABLE: [Column; 11] = [
    Column::Path,
    Column::Name,
    Column::Parent,
    Column::Mode,
    Column::Uid,
    Column::Gid,
    Column::User,
    Column::Group,
    Column::Atime,
    Column::Mtime,
    Column::Target,
];

pub const INSERTABLE: [InsertColumn; 12] = [
    InsertColumn::Column(Column::Path),
    InsertColumn::Column(Column::Kind),
    InsertColumn::Column(Column::Mode),
    InsertColumn::Column(Column::Target),
    InsertColumn::Column(Column::Uid),
    InsertColumn::Column(Column::Gid),
    InsertColumn::Column(Column::User),
    InsertColumn::Column(Column::Group),
    InsertColumn::Column(Column::Atime),
    InsertColumn::Column(Column::Mtime),
    InsertColumn::Source,
    InsertColumn::Content,
];

struct NoRow;

impl Row for NoRow {
    fn column(&self, name: &str) -> Result<Value> {
        Err(Error::Plan(format!(
            "column `{name}` cannot be used in a constant expression"
        )))
    }
}

pub fn constant(expr: &Expr) -> Result<Value> {
    Evaluator::default().eval(expr, &NoRow)
}

pub fn is_tautology(expr: &Expr) -> bool {
    matches!(constant(expr), Ok(value) if value.truth() == Some(true))
}

#[derive(Debug, Clone)]
pub struct Planner {
    pub default_root: PathBuf,
    pub options: WalkOptions,
}

impl Planner {
    pub fn new(default_root: impl Into<PathBuf>, options: WalkOptions) -> Self {
        Self {
            default_root: default_root.into(),
            options,
        }
    }

    pub fn plan(&self, sql: &str) -> Result<Vec<Plan>> {
        let statements = Parser::parse_sql(&FsqlDialect, sql)?;
        let plans = statements
            .iter()
            .map(|statement| self.statement(statement))
            .collect::<Result<Vec<_>>>()?;
        for plan in &plans {
            crate::bind::plan(plan, self)?;
        }
        Ok(plans)
    }

    fn statement(&self, statement: &Statement) -> Result<Plan> {
        match statement {
            Statement::Query(query) => Ok(Plan::Select(Box::new(self.query(query)?))),
            Statement::Delete(delete) => self.delete(delete),
            Statement::Update(update) => self.update(update),
            Statement::Insert(insert) => self.insert(insert),
            other => Err(Error::Unsupported(format!("statement `{other}`"))),
        }
    }

    pub fn query(&self, query: &Query) -> Result<QueryPlan> {
        self.query_scoped(query, &[])
    }

    pub fn query_scoped(&self, query: &Query, scope: &[String]) -> Result<QueryPlan> {
        if !query.pipe_operators.is_empty() {
            return Err(Error::Unsupported("pipe operators".to_owned()));
        }
        if query.fetch.is_some() {
            return Err(Error::Unsupported("FETCH".to_owned()));
        }
        let mut scope: Vec<String> = scope.to_vec();
        let mut ctes = Vec::new();
        let mut recursive = false;
        if let Some(with) = &query.with {
            recursive = with.recursive;
            for cte in &with.cte_tables {
                if cte.from.is_some() {
                    return Err(Error::Unsupported("CTE FROM".to_owned()));
                }
                let name = cte.alias.name.value.to_ascii_lowercase();
                if recursive {
                    scope.push(name.clone());
                }
                let planned = self.query_scoped(&cte.query, &scope)?;
                if !recursive {
                    scope.push(name.clone());
                }
                ctes.push(Cte {
                    name,
                    columns: cte
                        .alias
                        .columns
                        .iter()
                        .map(|c| c.name.value.to_ascii_lowercase())
                        .collect(),
                    query: planned,
                });
            }
        }
        let body = self.body(&query.body, &scope)?;
        let order_by = match &query.order_by {
            None => Vec::new(),
            Some(order_by) => {
                if order_by.interpolate.is_some() {
                    return Err(Error::Unsupported("INTERPOLATE".to_owned()));
                }
                match &order_by.kind {
                    OrderByKind::All(_) => {
                        return Err(Error::Unsupported("ORDER BY ALL".to_owned()));
                    }
                    OrderByKind::Expressions(exprs) => exprs
                        .iter()
                        .map(|item| {
                            if item.with_fill.is_some() {
                                return Err(Error::Unsupported("WITH FILL".to_owned()));
                            }
                            order_key(&item.expr, &item.options)
                        })
                        .collect::<Result<Vec<_>>>()?,
                }
            }
        };
        let (limit, offset) = limits(query.limit_clause.as_ref())?;
        Ok(QueryPlan {
            ctes,
            recursive,
            body,
            order_by,
            limit,
            offset,
        })
    }

    fn body(&self, body: &SetExpr, scope: &[String]) -> Result<SetBody> {
        match body {
            SetExpr::Select(select) => Ok(SetBody::Select(Box::new(self.select(select, scope)?))),
            SetExpr::Query(inner) => {
                let query = self.query_scoped(inner, scope)?;
                Ok(SetBody::Select(Box::new(SelectPlan {
                    from: vec![FromClause {
                        relation: Relation::Derived {
                            query: Box::new(query),
                            alias: "subquery".to_owned(),
                        },
                        joins: Vec::new(),
                    }],
                    projection: vec![Projection::Wildcard(None)],
                    filter: None,
                    group_by: Vec::new(),
                    having: None,
                    distinct: false,
                })))
            }
            SetExpr::Values(values) => Ok(SetBody::Values(
                values.rows.iter().map(|row| row.content.clone()).collect(),
            )),
            SetExpr::SetOperation {
                left,
                op,
                set_quantifier,
                right,
            } => {
                let op = match op {
                    SetOperator::Union => SetOp::Union,
                    SetOperator::Intersect => SetOp::Intersect,
                    SetOperator::Except | SetOperator::Minus => SetOp::Except,
                };
                let all = match set_quantifier {
                    SetQuantifier::All => true,
                    SetQuantifier::Distinct | SetQuantifier::None => false,
                    other => return Err(Error::Unsupported(format!("{other}"))),
                };
                Ok(SetBody::Op {
                    left: Box::new(self.body(left, scope)?),
                    op,
                    all,
                    right: Box::new(self.body(right, scope)?),
                })
            }
            other => Err(Error::Unsupported(format!("query `{other}`"))),
        }
    }

    fn select(&self, select: &Select, scope: &[String]) -> Result<SelectPlan> {
        if select.top.is_some() {
            return Err(Error::Unsupported("TOP".to_owned()));
        }
        if select.into.is_some() {
            return Err(Error::Unsupported("SELECT INTO".to_owned()));
        }
        if select.prewhere.is_some() || select.qualify.is_some() {
            return Err(Error::Unsupported("PREWHERE / QUALIFY".to_owned()));
        }
        if !select.lateral_views.is_empty() || !select.named_window.is_empty() {
            return Err(Error::Unsupported("lateral views / windows".to_owned()));
        }
        if select.exclude.is_some() {
            return Err(Error::Unsupported("EXCLUDE".to_owned()));
        }
        let distinct = match &select.distinct {
            None | Some(Distinct::All) => false,
            Some(Distinct::Distinct) => true,
            Some(Distinct::On(_)) => return Err(Error::Unsupported("DISTINCT ON".to_owned())),
        };
        let mut from = select
            .from
            .iter()
            .map(|item| self.clause(item, scope))
            .collect::<Result<Vec<_>>>()?;
        if let [clause] = from.as_mut_slice()
            && clause.joins.is_empty()
            && let Relation::Table { source, .. } = &mut clause.relation
            && source.table == Table::Files
            && !source.options.one_filesystem
            && let Some(filter) = &select.selection
            && let Some((root, depth)) = narrow(&source.root, filter)
        {
            source.root = root;
            source.options.initial_depth = depth;
        }
        let projection = select
            .projection
            .iter()
            .map(|item| match item {
                SelectItem::UnnamedExpr(expr) => Ok(Projection::Expr {
                    expr: Box::new(expr.clone()),
                    name: projection_name(expr),
                }),
                SelectItem::ExprWithAlias { expr, alias } => Ok(Projection::Expr {
                    expr: Box::new(expr.clone()),
                    name: alias.value.clone(),
                }),
                SelectItem::Wildcard(_) => Ok(Projection::Wildcard(None)),
                SelectItem::QualifiedWildcard(kind, _) => match kind {
                    sqlparser::ast::SelectItemQualifiedWildcardKind::ObjectName(name) => Ok(
                        Projection::Wildcard(Some(name.to_string().to_ascii_lowercase())),
                    ),
                    other => Err(Error::Unsupported(format!("{other}.*"))),
                },
                SelectItem::ExprWithAliases { .. } => {
                    Err(Error::Unsupported("multiple aliases".to_owned()))
                }
            })
            .collect::<Result<Vec<_>>>()?;
        let group_by = match &select.group_by {
            GroupByExpr::All(_) => return Err(Error::Unsupported("GROUP BY ALL".to_owned())),
            GroupByExpr::Expressions(exprs, modifiers) => {
                if !modifiers.is_empty() {
                    return Err(Error::Unsupported("GROUP BY modifiers".to_owned()));
                }
                exprs.clone()
            }
        };
        Ok(SelectPlan {
            from,
            projection,
            filter: select.selection.clone(),
            group_by,
            having: select.having.clone(),
            distinct,
        })
    }

    fn clause(&self, item: &TableWithJoins, scope: &[String]) -> Result<FromClause> {
        let relation = self.relation(&item.relation, scope)?;
        let joins = item
            .joins
            .iter()
            .map(|join| {
                if join.global {
                    return Err(Error::Unsupported("GLOBAL JOIN".to_owned()));
                }
                let relation = self.relation(&join.relation, scope)?;
                let (kind, constraint) = match &join.join_operator {
                    JoinOperator::Join(c) | JoinOperator::Inner(c) => (JoinKind::Inner, c),
                    JoinOperator::Left(c) | JoinOperator::LeftOuter(c) => (JoinKind::Left, c),
                    JoinOperator::Right(c) | JoinOperator::RightOuter(c) => (JoinKind::Right, c),
                    JoinOperator::FullOuter(c) => (JoinKind::Full, c),
                    JoinOperator::CrossJoin(c) => (JoinKind::Cross, c),
                    other => return Err(Error::Unsupported(format!("join `{other:?}`"))),
                };
                let (on, using) = match constraint {
                    JoinConstraint::On(expr) => (Some(expr.clone()), Vec::new()),
                    JoinConstraint::Using(columns) => (
                        None,
                        columns
                            .iter()
                            .map(|c| c.to_string().to_ascii_lowercase())
                            .collect(),
                    ),
                    JoinConstraint::Natural => {
                        return Err(Error::Unsupported("NATURAL JOIN".to_owned()));
                    }
                    JoinConstraint::None => (None, Vec::new()),
                };
                if kind != JoinKind::Cross && on.is_none() && using.is_empty() {
                    return Err(Error::Plan(format!(
                        "JOIN {} needs ON or USING",
                        relation.alias()
                    )));
                }
                Ok(JoinSpec {
                    relation,
                    kind,
                    on,
                    using,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(FromClause { relation, joins })
    }

    fn relation(&self, factor: &TableFactor, scope: &[String]) -> Result<Relation> {
        match factor {
            TableFactor::Table {
                name, alias, args, ..
            } => {
                let table_name = name.to_string().to_ascii_lowercase();
                let alias = alias
                    .as_ref()
                    .map(|a| a.name.value.to_ascii_lowercase())
                    .unwrap_or_else(|| table_name.clone());
                if scope.contains(&table_name) {
                    if args.is_some() {
                        return Err(Error::Plan(format!(
                            "CTE `{table_name}` takes no arguments"
                        )));
                    }
                    return Ok(Relation::Cte {
                        name: table_name,
                        alias,
                    });
                }
                let table = Table::parse(&table_name).ok_or(Error::UnknownTable(table_name))?;
                let mut root = self.default_root.clone();
                if let Some(args) = args {
                    if args.settings.is_some() {
                        return Err(Error::Unsupported("table settings".to_owned()));
                    }
                    match args.args.as_slice() {
                        [] => {}
                        [FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))] => {
                            root = PathBuf::from(literal_text(expr)?);
                        }
                        _ => return Err(Error::Unsupported("table arguments".to_owned())),
                    }
                }
                Ok(Relation::Table {
                    source: Source {
                        table,
                        root,
                        options: self.options.clone(),
                    },
                    alias,
                })
            }
            TableFactor::Derived {
                lateral,
                subquery,
                alias,
                ..
            } => {
                if *lateral {
                    return Err(Error::Unsupported("LATERAL".to_owned()));
                }
                Ok(Relation::Derived {
                    query: Box::new(self.query_scoped(subquery, scope)?),
                    alias: alias
                        .as_ref()
                        .map(|a| a.name.value.to_ascii_lowercase())
                        .unwrap_or_else(|| "subquery".to_owned()),
                })
            }
            other => Err(Error::Unsupported(format!("table `{other}`"))),
        }
    }

    fn mutation_source(&self, item: &TableWithJoins, filter: &Expr) -> Result<(Source, String)> {
        if !item.joins.is_empty() {
            return Err(Error::Unsupported("JOIN in a mutation".to_owned()));
        }
        let Relation::Table { mut source, alias } = self.relation(&item.relation, &[])? else {
            return Err(Error::Unsupported("subquery as mutation target".to_owned()));
        };
        if source.table != Table::Files {
            return Err(Error::Plan(format!(
                "`{}` is read only",
                source.table.name()
            )));
        }
        if !source.options.one_filesystem
            && let Some((root, depth)) = narrow(&source.root, filter)
        {
            source.root = root;
            source.options.initial_depth = depth;
        }
        Ok((source, alias))
    }

    fn delete(&self, delete: &Delete) -> Result<Plan> {
        if !delete.tables.is_empty() {
            return Err(Error::Unsupported("multi-table DELETE".to_owned()));
        }
        if delete.using.is_some() || delete.returning.is_some() || delete.output.is_some() {
            return Err(Error::Unsupported("USING / RETURNING".to_owned()));
        }
        let (FromTable::WithFromKeyword(from) | FromTable::WithoutKeyword(from)) = &delete.from;
        let [item] = from.as_slice() else {
            return Err(Error::Unsupported("DELETE from multiple tables".to_owned()));
        };
        let filter = required_filter(delete.selection.as_ref(), "DELETE")?;
        let (source, alias) = self.mutation_source(item, &filter)?;
        let order_by = delete
            .order_by
            .iter()
            .map(|item| order_key(&item.expr, &item.options))
            .collect::<Result<Vec<_>>>()?;
        let limit = delete.limit.as_ref().map(count).transpose()?;
        Ok(Plan::Delete(DeletePlan {
            source,
            alias,
            filter,
            order_by,
            limit,
        }))
    }

    fn update(&self, update: &Update) -> Result<Plan> {
        if update.from.is_some() || update.returning.is_some() || update.output.is_some() {
            return Err(Error::Unsupported("UPDATE FROM / RETURNING".to_owned()));
        }
        if update.or.is_some() {
            return Err(Error::Unsupported("UPDATE OR".to_owned()));
        }
        let filter = required_filter(update.selection.as_ref(), "UPDATE")?;
        let (source, alias) = self.mutation_source(&update.table, &filter)?;
        let assignments = update
            .assignments
            .iter()
            .map(set)
            .collect::<Result<Vec<_>>>()?;
        if assignments.is_empty() {
            return Err(Error::Plan("UPDATE has no SET clause".to_owned()));
        }
        let order_by = update
            .order_by
            .iter()
            .map(|item| order_key(&item.expr, &item.options))
            .collect::<Result<Vec<_>>>()?;
        let limit = update.limit.as_ref().map(count).transpose()?;
        Ok(Plan::Update(UpdatePlan {
            source,
            alias,
            assignments,
            filter,
            order_by,
            limit,
        }))
    }

    fn insert(&self, insert: &Insert) -> Result<Plan> {
        if insert.on.is_some() || insert.or.is_some() || insert.replace_into || insert.ignore {
            return Err(Error::Unsupported("INSERT conflict handling".to_owned()));
        }
        if insert.returning.is_some() || insert.output.is_some() || insert.overwrite {
            return Err(Error::Unsupported(
                "INSERT RETURNING / OVERWRITE".to_owned(),
            ));
        }
        if !insert.multi_table_into_clauses.is_empty() || insert.partitioned.is_some() {
            return Err(Error::Unsupported("multi-table INSERT".to_owned()));
        }
        let TableObject::TableName(name) = &insert.table else {
            return Err(Error::Unsupported(format!(
                "INSERT into `{}`",
                insert.table
            )));
        };
        let table_name = name.to_string().to_ascii_lowercase();
        let table = Table::parse(&table_name).ok_or(Error::UnknownTable(table_name))?;
        if table != Table::Files {
            return Err(Error::Plan(format!("`{}` is read only", table.name())));
        }
        let (columns, rows) = if !insert.assignments.is_empty() {
            let mut columns = Vec::new();
            let mut row = Vec::new();
            for assignment in &insert.assignments {
                let AssignmentTarget::ColumnName(name) = &assignment.target else {
                    return Err(Error::Unsupported("tuple assignment".to_owned()));
                };
                let text = name.to_string().to_ascii_lowercase();
                columns.push(InsertColumn::parse(&text).ok_or(Error::UnknownColumn(text))?);
                row.push(assignment.value.clone());
            }
            (columns, InsertRows::Values(vec![row]))
        } else {
            let columns = insert
                .columns
                .iter()
                .map(|name| {
                    let text = name.to_string().to_ascii_lowercase();
                    InsertColumn::parse(&text).ok_or(Error::UnknownColumn(text))
                })
                .collect::<Result<Vec<_>>>()?;
            let Some(source) = &insert.source else {
                return Err(Error::Plan("INSERT has no VALUES or SELECT".to_owned()));
            };
            let rows = match source.body.as_ref() {
                SetExpr::Values(values) if source.with.is_none() => {
                    InsertRows::Values(values.rows.iter().map(|row| row.content.clone()).collect())
                }
                _ => InsertRows::Query(Box::new(self.query(source)?)),
            };
            (columns, rows)
        };
        if columns.is_empty() {
            return Err(Error::Plan(
                "INSERT needs an explicit column list".to_owned(),
            ));
        }
        for column in &columns {
            if !INSERTABLE.contains(column) {
                return Err(Error::Plan(format!(
                    "column `{}` cannot be set by INSERT",
                    column.name()
                )));
            }
        }
        if !columns.contains(&InsertColumn::Column(Column::Path)) {
            return Err(Error::Plan("INSERT must set `path`".to_owned()));
        }
        if let InsertRows::Values(rows) = &rows {
            for row in rows {
                if row.len() != columns.len() {
                    return Err(Error::Plan(format!(
                        "INSERT row has {} values for {} columns",
                        row.len(),
                        columns.len()
                    )));
                }
            }
        }
        Ok(Plan::Insert(InsertPlan {
            table,
            columns,
            rows,
        }))
    }
}

fn required_filter(filter: Option<&Expr>, verb: &str) -> Result<Expr> {
    let Some(filter) = filter else {
        return Err(Error::Plan(format!("{verb} has no WHERE clause")));
    };
    if is_tautology(filter) {
        return Err(Error::Plan(format!(
            "{verb} WHERE clause `{filter}` is always true"
        )));
    }
    Ok(filter.clone())
}

fn set(assignment: &Assignment) -> Result<Set> {
    let AssignmentTarget::ColumnName(name) = &assignment.target else {
        return Err(Error::Unsupported("tuple assignment".to_owned()));
    };
    let text = name.to_string().to_ascii_lowercase();
    let text = text.rsplit('.').next().unwrap_or(&text).to_owned();
    let column = Column::parse(&text).ok_or(Error::UnknownColumn(text))?;
    if !ASSIGNABLE.contains(&column) {
        return Err(Error::Plan(format!(
            "column `{}` cannot be assigned",
            column.name()
        )));
    }
    Ok(Set {
        column,
        value: assignment.value.clone(),
    })
}

fn order_key(expr: &Expr, options: &sqlparser::ast::OrderByOptions) -> Result<OrderKey> {
    let descending = match &options.sort {
        None | Some(OrderBySort::Asc) => false,
        Some(OrderBySort::Desc) => true,
        Some(OrderBySort::Using(_)) => return Err(Error::Unsupported("ORDER BY USING".to_owned())),
    };
    Ok(OrderKey {
        expr: expr.clone(),
        descending,
        nulls_first: options.nulls_first.unwrap_or(descending),
    })
}

fn limits(clause: Option<&LimitClause>) -> Result<(Option<usize>, usize)> {
    match clause {
        None => Ok((None, 0)),
        Some(LimitClause::LimitOffset {
            limit,
            offset,
            limit_by,
        }) => {
            if !limit_by.is_empty() {
                return Err(Error::Unsupported("LIMIT BY".to_owned()));
            }
            let limit = limit.as_ref().map(count).transpose()?;
            let offset = offset.as_ref().map(|o| count(&o.value)).transpose()?;
            Ok((limit, offset.unwrap_or(0)))
        }
        Some(LimitClause::OffsetCommaLimit { offset, limit }) => {
            Ok((Some(count(limit)?), count(offset)?))
        }
    }
}

fn count(expr: &Expr) -> Result<usize> {
    match constant(expr)? {
        Value::Int(n) if n >= 0 => {
            usize::try_from(n).map_err(|_| Error::Overflow("LIMIT".to_owned()))
        }
        other => Err(Error::Plan(format!(
            "expected a non-negative integer, got `{}`",
            crate::eval::render(&other)
        ))),
    }
}

fn literal_text(expr: &Expr) -> Result<String> {
    match expr {
        Expr::Value(literal) => match &literal.value {
            Literal::SingleQuotedString(s) | Literal::DoubleQuotedString(s) => Ok(s.clone()),
            other => Err(Error::Plan(format!(
                "expected a path string, got `{other}`"
            ))),
        },
        Expr::Identifier(ident) => Ok(ident.value.clone()),
        other => Err(Error::Plan(format!(
            "expected a path string, got `{other}`"
        ))),
    }
}

fn projection_name(expr: &Expr) -> String {
    match expr {
        Expr::Identifier(ident) => ident.value.clone(),
        Expr::CompoundIdentifier(parts) => {
            parts.last().map(|p| p.value.clone()).unwrap_or_default()
        }
        other => other.to_string(),
    }
}

fn conjuncts<'a>(expr: &'a Expr, out: &mut Vec<&'a Expr>) {
    match expr {
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            conjuncts(left, out);
            conjuncts(right, out);
        }
        Expr::Nested(inner) => conjuncts(inner, out),
        other => out.push(other),
    }
}

fn is_column(expr: &Expr, column: Column) -> bool {
    match expr {
        Expr::Identifier(ident) => ident.value.eq_ignore_ascii_case(column.name()),
        Expr::CompoundIdentifier(parts) => parts
            .last()
            .is_some_and(|p| p.value.eq_ignore_ascii_case(column.name())),
        Expr::Nested(inner) => is_column(inner, column),
        _ => false,
    }
}

fn string_literal(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Value(literal) => match &literal.value {
            Literal::SingleQuotedString(s) | Literal::DoubleQuotedString(s) => Some(s),
            _ => None,
        },
        Expr::Nested(inner) => string_literal(inner),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Hint {
    Prefix(String),
    Exact(String),
    Parent(String),
}

fn hint(term: &Expr) -> Option<Hint> {
    match term {
        Expr::Like {
            negated: false,
            expr,
            pattern,
            escape_char: None,
            ..
        } if is_column(expr, Column::Path) => {
            let pattern = string_literal(pattern)?;
            let end = pattern.find(['%', '_']).unwrap_or(pattern.len());
            Some(Hint::Prefix(pattern[..end].to_owned()))
        }
        Expr::BinaryOp { left, op, right } => match op {
            BinaryOperator::Glob if is_column(left, Column::Path) => {
                let pattern = string_literal(right)?;
                let end = pattern
                    .find(['*', '?', '[', '{', '\\'])
                    .unwrap_or(pattern.len());
                Some(Hint::Prefix(pattern[..end].to_owned()))
            }
            BinaryOperator::Eq if is_column(left, Column::Path) => {
                Some(Hint::Exact(string_literal(right)?.to_owned()))
            }
            BinaryOperator::Eq if is_column(right, Column::Path) => {
                Some(Hint::Exact(string_literal(left)?.to_owned()))
            }
            BinaryOperator::Eq if is_column(left, Column::Parent) => {
                Some(Hint::Parent(string_literal(right)?.to_owned()))
            }
            BinaryOperator::Eq if is_column(right, Column::Parent) => {
                Some(Hint::Parent(string_literal(left)?.to_owned()))
            }
            _ => None,
        },
        Expr::Function(function) => {
            if !function
                .name
                .to_string()
                .eq_ignore_ascii_case("starts_with")
            {
                return None;
            }
            let sqlparser::ast::FunctionArguments::List(list) = &function.args else {
                return None;
            };
            let [
                FunctionArg::Unnamed(FunctionArgExpr::Expr(subject)),
                FunctionArg::Unnamed(FunctionArgExpr::Expr(prefix)),
            ] = list.args.as_slice()
            else {
                return None;
            };
            if !is_column(subject, Column::Path) {
                return None;
            }
            Some(Hint::Prefix(string_literal(prefix)?.to_owned()))
        }
        Expr::Nested(inner) => hint(inner),
        _ => None,
    }
}

fn hint_directory(hint: &Hint) -> Option<PathBuf> {
    match hint {
        Hint::Prefix(prefix) => {
            if !prefix.starts_with('/') {
                return None;
            }
            if let Some(dir) = prefix.strip_suffix('/') {
                Some(PathBuf::from(if dir.is_empty() { "/" } else { dir }))
            } else {
                Path::new(prefix).parent().map(Path::to_path_buf)
            }
        }
        Hint::Exact(path) => {
            if !path.starts_with('/') {
                return None;
            }
            Path::new(path).parent().map(Path::to_path_buf)
        }
        Hint::Parent(dir) => {
            if !dir.starts_with('/') {
                return None;
            }
            Some(PathBuf::from(dir))
        }
    }
}

pub fn narrow(root: &Path, filter: &Expr) -> Option<(PathBuf, u32)> {
    let root = std::fs::canonicalize(root).ok()?;
    let mut terms = Vec::new();
    conjuncts(filter, &mut terms);
    let mut best: Option<PathBuf> = None;
    for term in terms {
        let Some(hint) = hint(term) else { continue };
        let Some(dir) = hint_directory(&hint) else {
            continue;
        };
        let Ok(dir) = std::fs::canonicalize(&dir) else {
            continue;
        };
        if !dir.starts_with(&root) {
            continue;
        }
        let deeper = best
            .as_ref()
            .is_none_or(|current| dir.components().count() > current.components().count());
        if deeper {
            best = Some(dir);
        }
    }
    let dir = best?;
    if dir == root {
        return None;
    }
    let depth = dir.strip_prefix(&root).ok()?.components().count();
    Some((dir, u32::try_from(depth).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn planner() -> Planner {
        Planner::new("/", WalkOptions::default())
    }

    fn one(sql: &str) -> Result<Plan> {
        let mut plans = planner().plan(sql)?;
        assert_eq!(plans.len(), 1);
        Ok(plans.remove(0))
    }

    fn select_of(plan: &QueryPlan) -> &SelectPlan {
        match &plan.body {
            SetBody::Select(select) => select,
            other => panic!("expected select, got {other:?}"),
        }
    }

    #[test]
    fn select_plan_carries_every_clause() {
        let Plan::Select(plan) = one(
            "select ext, count(*) as n, sum(size) from files('/tmp') where size > 1k group by ext having count(*) > 1 order by n desc, ext limit 10 offset 5",
        )
        .expect("plan") else {
            panic!("expected select");
        };
        let select = select_of(&plan);
        let source = select.single_table().expect("source");
        assert_eq!(source.table, Table::Files);
        assert_eq!(source.root, PathBuf::from("/tmp"));
        assert_eq!(select.projection.len(), 3);
        assert!(matches!(&select.projection[1], Projection::Expr { name, .. } if name == "n"));
        assert!(select.filter.is_some());
        assert_eq!(select.group_by.len(), 1);
        assert!(select.having.is_some());
        assert_eq!(plan.order_by.len(), 2);
        assert!(plan.order_by[0].descending);
        assert!(plan.order_by[0].nulls_first);
        assert!(!plan.order_by[1].descending);
        assert!(!plan.order_by[1].nulls_first);
        assert_eq!(plan.limit, Some(10));
        assert_eq!(plan.offset, 5);
    }

    #[test]
    fn select_without_from_has_no_relations() {
        let Plan::Select(plan) = one("select 1 + 1 as two").expect("plan") else {
            panic!("expected select");
        };
        assert!(select_of(&plan).from.is_empty());
    }

    #[test]
    fn joins_ctes_derived_tables_and_set_ops_plan() {
        let Plan::Select(plan) = one(
            "with big as (select path, dev from files where size > 1g) select b.path, m.fstype from big b join mounts m on b.dev = m.dev left join files f using (path) union all select path, null from files",
        )
        .expect("plan") else {
            panic!("expected select");
        };
        assert_eq!(plan.ctes.len(), 1);
        assert_eq!(plan.ctes[0].name, "big");
        let SetBody::Op { left, op, all, .. } = &plan.body else {
            panic!("expected set op");
        };
        assert_eq!(*op, SetOp::Union);
        assert!(all);
        let SetBody::Select(select) = left.as_ref() else {
            panic!("expected select");
        };
        assert!(
            matches!(&select.from[0].relation, Relation::Cte { name, alias } if name == "big" && alias == "b")
        );
        assert_eq!(select.from[0].joins.len(), 2);
        assert_eq!(select.from[0].joins[0].kind, JoinKind::Inner);
        assert!(select.from[0].joins[0].on.is_some());
        assert_eq!(select.from[0].joins[1].kind, JoinKind::Left);
        assert_eq!(select.from[0].joins[1].using, vec!["path".to_owned()]);
        let Plan::Select(plan) =
            one("select * from (select name from files) sub where name = 'x'").expect("plan")
        else {
            panic!("expected select");
        };
        assert!(
            matches!(&select_of(&plan).from[0].relation, Relation::Derived { alias, .. } if alias == "sub")
        );
        assert!(matches!(
            one("select 1 from files a join files b on a.path = b.path natural join mounts"),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            one("select 1 from files a join files b"),
            Err(Error::Plan(_))
        ));
    }

    #[test]
    fn delete_requires_a_where_clause() {
        assert!(
            matches!(one("delete from files"), Err(Error::Plan(reason)) if reason.contains("WHERE"))
        );
    }

    #[test]
    fn delete_rejects_tautologies() {
        for sql in [
            "delete from files where 1 = 1",
            "delete from files where true",
            "delete from files where 'a' like '%'",
        ] {
            assert!(
                matches!(one(sql), Err(Error::Plan(reason)) if reason.contains("always true")),
                "{sql}"
            );
        }
    }

    #[test]
    fn delete_with_a_real_predicate_plans() {
        let Plan::Delete(plan) =
            one("delete from files f where f.ext = 'tmp' order by size desc limit 5")
                .expect("plan")
        else {
            panic!("expected delete");
        };
        assert_eq!(plan.limit, Some(5));
        assert_eq!(plan.order_by.len(), 1);
        assert_eq!(plan.alias, "f");
    }

    #[test]
    fn delete_from_a_read_only_table_is_refused() {
        assert!(matches!(
            one("delete from mounts where dev = 1"),
            Err(Error::Plan(_))
        ));
    }

    #[test]
    fn update_only_assigns_assignable_columns() {
        assert!(one("update files set mode = 0o644 where ext = 'sh'").is_ok());
        assert!(matches!(
            one("update files set size = 0 where ext = 'sh'"),
            Err(Error::Plan(_))
        ));
        assert!(matches!(
            one("update files set mode = 0o644"),
            Err(Error::Plan(_))
        ));
    }

    #[test]
    fn insert_needs_path_and_known_columns() {
        let Plan::Insert(plan) = one(
            "insert into files (path, kind, source) values ('/tmp/x', 'file', '/etc/hostname')",
        )
        .expect("plan") else {
            panic!("expected insert");
        };
        assert_eq!(
            plan.columns,
            vec![
                InsertColumn::Column(Column::Path),
                InsertColumn::Column(Column::Kind),
                InsertColumn::Source
            ]
        );
        assert!(matches!(
            one("insert into files (kind) values ('dir')"),
            Err(Error::Plan(_))
        ));
        assert!(matches!(
            one("insert into files (path, size) values ('/x', 1)"),
            Err(Error::Plan(_))
        ));
        assert!(matches!(
            one("insert into files (path, kind) values ('/x')"),
            Err(Error::Plan(_))
        ));
    }

    #[test]
    fn insert_select_is_a_copy_plan() {
        let Plan::Insert(plan) = one("insert into files (path, kind, source) select path || '.bak', kind, path from files where ext = 'txt'").expect("plan") else {
            panic!("expected insert");
        };
        assert!(matches!(plan.rows, InsertRows::Query(_)));
    }

    #[test]
    fn unknown_tables_and_columns_are_named() {
        assert!(
            matches!(one("select 1 from dirs"), Err(Error::UnknownTable(name)) if name == "dirs")
        );
        assert!(
            matches!(one("update files set sizee = 1 where 1 = 2"), Err(Error::UnknownColumn(name)) if name == "sizee")
        );
    }

    fn fixture(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fsql-plan-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("a/b/c")).expect("dirs");
        std::fs::canonicalize(dir).expect("canonical")
    }

    fn parse_filter(sql: &str) -> Expr {
        Parser::new(&FsqlDialect)
            .try_with_sql(sql)
            .expect("tokenize")
            .parse_expr()
            .expect("parse")
    }

    #[test]
    fn path_prefixes_narrow_the_walk_root() {
        let dir = fixture("narrow");
        let display = dir.display();
        let cases = [
            (format!("path like '{display}/a/b/%'"), Some(("a/b", 2))),
            (format!("path like '{display}/a/b%'"), Some(("a", 1))),
            (
                format!("path glob '{display}/a/**/*.rs' and size > 1"),
                Some(("a", 1)),
            ),
            (
                format!("starts_with(path, '{display}/a/b/c/')"),
                Some(("a/b/c", 3)),
            ),
            (format!("path = '{display}/a/b/c/file'"), Some(("a/b/c", 3))),
            (format!("parent = '{display}/a'"), Some(("a", 1))),
            (format!("path like '{display}/%'"), None),
            (format!("path like '{display}/a/%' or ext = 'x'"), None),
            (format!("path not like '{display}/a/%'"), None),
            ("path like '/nonexistent/%'".to_owned(), None),
            ("ext = 'tmp'".to_owned(), None),
        ];
        for (sql, expected) in cases {
            let result = narrow(&dir, &parse_filter(&sql));
            let expected = expected.map(|(suffix, depth)| (dir.join(suffix), depth));
            assert_eq!(result, expected, "{sql}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn narrowing_never_escapes_the_declared_root() {
        let dir = fixture("escape");
        let inner = dir.join("a");
        assert_eq!(
            narrow(
                &inner,
                &parse_filter(&format!("path like '{}/%'", dir.display()))
            ),
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
