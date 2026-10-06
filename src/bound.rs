//! The binder's output: a query where every name is resolved and every
//! expression is typed.
//!
//! Every column the query can see gets a unique `ColumnId`: table columns
//! (once per occurrence, so `nation n1, nation n2` get different ids),
//! derived-table columns, GROUP BY keys, aggregate results, and outputs.
//! Expressions refer to columns only by id, so later phases can move
//! expressions around the plan without re-resolving names.

use crate::ast::{BinaryOp, JoinKind, UnaryOp};
use crate::types::{Type, Value};

pub type ColumnId = usize;

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnInfo {
    pub id: ColumnId,
    pub qualifier: Option<String>,
    pub name: String,
    pub ty: Type,
}

impl ColumnInfo {
    pub fn display_name(&self) -> String {
        match &self.qualifier {
            Some(q) => format!("{q}.{}", self.name),
            None => self.name.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundExpr {
    pub kind: ExprKind,
    pub ty: Type,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExprKind {
    Column(ColumnId),
    Literal(Value),
    Unary {
        op: UnaryOp,
        expr: Box<BoundExpr>,
    },
    Binary {
        left: Box<BoundExpr>,
        op: BinaryOp,
        right: Box<BoundExpr>,
    },
    IsNull {
        expr: Box<BoundExpr>,
        negated: bool,
    },
    Like {
        expr: Box<BoundExpr>,
        pattern: Box<BoundExpr>,
        negated: bool,
    },
    InList {
        expr: Box<BoundExpr>,
        list: Vec<BoundExpr>,
        negated: bool,
    },
    Case {
        operand: Option<Box<BoundExpr>>,
        branches: Vec<(BoundExpr, BoundExpr)>,
        else_result: Option<Box<BoundExpr>>,
    },
    /// Converts `expr` to this node's `ty`.
    Cast {
        expr: Box<BoundExpr>,
    },
    Function {
        func: ScalarFunc,
        args: Vec<BoundExpr>,
    },
    Subquery {
        query: Box<BoundQuery>,
        kind: SubqueryKind,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum SubqueryKind {
    Scalar,
    Exists { negated: bool },
    In { expr: Box<BoundExpr>, negated: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarFunc {
    Substring,
    Upper,
    Lower,
    Length,
    Abs,
    Round,
    Coalesce,
    ExtractYear,
    ExtractMonth,
    ExtractDay,
}

impl ScalarFunc {
    pub fn name(self) -> &'static str {
        match self {
            ScalarFunc::Substring => "substring",
            ScalarFunc::Upper => "upper",
            ScalarFunc::Lower => "lower",
            ScalarFunc::Length => "length",
            ScalarFunc::Abs => "abs",
            ScalarFunc::Round => "round",
            ScalarFunc::Coalesce => "coalesce",
            ScalarFunc::ExtractYear => "extract_year",
            ScalarFunc::ExtractMonth => "extract_month",
            ScalarFunc::ExtractDay => "extract_day",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggFunc {
    CountStar,
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

impl AggFunc {
    pub fn name(self) -> &'static str {
        match self {
            AggFunc::CountStar | AggFunc::Count => "count",
            AggFunc::Sum => "sum",
            AggFunc::Avg => "avg",
            AggFunc::Min => "min",
            AggFunc::Max => "max",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AggregateCall {
    /// Column id under which the aggregate's result is visible.
    pub id: ColumnId,
    pub func: AggFunc,
    /// `None` only for `count(*)`.
    pub arg: Option<BoundExpr>,
    pub distinct: bool,
    pub ty: Type,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GroupKey {
    pub id: ColumnId,
    pub expr: BoundExpr,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BoundTableRef {
    Base {
        table: String,
        columns: Vec<ColumnId>,
    },
    /// `columns[i]` is the outer name for the subquery's i-th output.
    Derived {
        query: Box<BoundQuery>,
        columns: Vec<ColumnId>,
    },
    Join {
        left: Box<BoundTableRef>,
        right: Box<BoundTableRef>,
        kind: JoinKind,
        on: Option<BoundExpr>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutputColumn {
    pub id: ColumnId,
    pub name: String,
    pub expr: BoundExpr,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundOrderBy {
    /// Always one of the query's projection ids.
    pub column: ColumnId,
    pub asc: bool,
    pub nulls_first: bool,
}

/// Evaluation order: from -> filter -> group/aggregate -> having ->
/// projection -> distinct -> order by -> limit/offset.
///
/// When the query aggregates, `having` and `projection` refer only to group
/// key ids, aggregate ids, and outer (correlated) columns.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BoundQuery {
    pub from: Option<BoundTableRef>,
    pub filter: Option<BoundExpr>,
    pub group_by: Vec<GroupKey>,
    pub aggregates: Vec<AggregateCall>,
    pub having: Option<BoundExpr>,
    /// Visible outputs first, then hidden columns needed only by ORDER BY.
    pub projection: Vec<OutputColumn>,
    pub visible: usize,
    pub distinct: bool,
    pub order_by: Vec<BoundOrderBy>,
    pub limit: Option<u64>,
    pub offset: Option<u64>,
    /// Columns from enclosing queries that this query references.
    pub correlated: Vec<ColumnId>,
}

impl BoundQuery {
    pub fn is_aggregate(&self) -> bool {
        !self.group_by.is_empty() || !self.aggregates.is_empty() || self.having.is_some()
    }

    pub fn outputs(&self) -> &[OutputColumn] {
        &self.projection[..self.visible]
    }
}

type MapFn<'f, E> = dyn FnMut(BoundExpr) -> Result<BoundExpr, E> + 'f;

impl BoundExpr {
    pub fn new(kind: ExprKind, ty: Type) -> Self {
        BoundExpr { kind, ty }
    }

    pub fn literal(v: Value) -> Self {
        let ty = v.ty();
        BoundExpr {
            kind: ExprKind::Literal(v),
            ty,
        }
    }

    pub fn column(id: ColumnId, ty: Type) -> Self {
        BoundExpr {
            kind: ExprKind::Column(id),
            ty,
        }
    }

    /// Rebuilds this node with `f` applied to each direct child expression.
    /// Subquery bodies are not entered (they have their own scope).
    pub fn try_map_children<E>(self, f: &mut MapFn<'_, E>) -> Result<BoundExpr, E> {
        let boxed = |e: Box<BoundExpr>, f: &mut MapFn<'_, E>| f(*e).map(Box::new);
        let kind = match self.kind {
            k @ (ExprKind::Column(_) | ExprKind::Literal(_)) => k,
            ExprKind::Unary { op, expr } => ExprKind::Unary {
                op,
                expr: boxed(expr, f)?,
            },
            ExprKind::Binary { left, op, right } => {
                let left = boxed(left, f)?;
                ExprKind::Binary {
                    left,
                    op,
                    right: boxed(right, f)?,
                }
            }
            ExprKind::IsNull { expr, negated } => ExprKind::IsNull {
                expr: boxed(expr, f)?,
                negated,
            },
            ExprKind::Like {
                expr,
                pattern,
                negated,
            } => {
                let expr = boxed(expr, f)?;
                ExprKind::Like {
                    expr,
                    pattern: boxed(pattern, f)?,
                    negated,
                }
            }
            ExprKind::InList {
                expr,
                list,
                negated,
            } => {
                let expr = boxed(expr, f)?;
                let list = list.into_iter().map(&mut *f).collect::<Result<_, E>>()?;
                ExprKind::InList {
                    expr,
                    list,
                    negated,
                }
            }
            ExprKind::Case {
                operand,
                branches,
                else_result,
            } => {
                let operand = match operand {
                    Some(o) => Some(boxed(o, f)?),
                    None => None,
                };
                let mut new_branches = Vec::with_capacity(branches.len());
                for (w, t) in branches {
                    let w = f(w)?;
                    new_branches.push((w, f(t)?));
                }
                let else_result = match else_result {
                    Some(e) => Some(boxed(e, f)?),
                    None => None,
                };
                ExprKind::Case {
                    operand,
                    branches: new_branches,
                    else_result,
                }
            }
            ExprKind::Cast { expr } => ExprKind::Cast {
                expr: boxed(expr, f)?,
            },
            ExprKind::Function { func, args } => ExprKind::Function {
                func,
                args: args.into_iter().map(&mut *f).collect::<Result<_, E>>()?,
            },
            ExprKind::Subquery {
                query,
                kind: SubqueryKind::In { expr, negated },
            } => ExprKind::Subquery {
                query,
                kind: SubqueryKind::In {
                    expr: boxed(expr, f)?,
                    negated,
                },
            },
            k @ ExprKind::Subquery { .. } => k,
        };
        Ok(BoundExpr { kind, ty: self.ty })
    }

    /// Pre-order traversal over this expression (not entering subquery bodies).
    pub fn walk(&self, f: &mut dyn FnMut(&BoundExpr)) {
        f(self);
        match &self.kind {
            ExprKind::Column(_) | ExprKind::Literal(_) => {}
            ExprKind::Unary { expr, .. }
            | ExprKind::IsNull { expr, .. }
            | ExprKind::Cast { expr } => expr.walk(f),
            ExprKind::Binary { left, right, .. } => {
                left.walk(f);
                right.walk(f);
            }
            ExprKind::Like { expr, pattern, .. } => {
                expr.walk(f);
                pattern.walk(f);
            }
            ExprKind::InList { expr, list, .. } => {
                expr.walk(f);
                list.iter().for_each(|e| e.walk(f));
            }
            ExprKind::Case {
                operand,
                branches,
                else_result,
            } => {
                if let Some(o) = operand {
                    o.walk(f);
                }
                for (w, t) in branches {
                    w.walk(f);
                    t.walk(f);
                }
                if let Some(e) = else_result {
                    e.walk(f);
                }
            }
            ExprKind::Function { args, .. } => args.iter().for_each(|e| e.walk(f)),
            ExprKind::Subquery {
                kind: SubqueryKind::In { expr, .. },
                ..
            } => expr.walk(f),
            ExprKind::Subquery { .. } => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Explain: a readable outline of a bound query, used by the REPL's \plan.
// ---------------------------------------------------------------------------

/// A bound query plus the registry describing every `ColumnId`.
#[derive(Debug, Clone)]
pub struct Bound {
    pub query: BoundQuery,
    pub columns: Vec<ColumnInfo>,
}

impl Bound {
    pub fn column(&self, id: ColumnId) -> &ColumnInfo {
        &self.columns[id]
    }

    pub fn explain(&self) -> String {
        let mut ex = Explainer {
            cols: &self.columns,
            out: String::new(),
        };
        ex.query(&self.query, 0);
        ex.out
    }
}

struct Explainer<'a> {
    cols: &'a [ColumnInfo],
    out: String,
}

impl<'a> Explainer<'a> {
    fn line(&mut self, indent: usize, text: &str) {
        self.out.push_str(&"  ".repeat(indent));
        self.out.push_str(text);
        self.out.push('\n');
    }

    fn name(&self, id: ColumnId) -> String {
        self.cols[id].display_name()
    }

    fn query(&mut self, q: &'a BoundQuery, indent: usize) {
        if q.correlated.is_empty() {
            self.line(indent, "Query");
        } else {
            let names: Vec<String> = q.correlated.iter().map(|&id| self.name(id)).collect();
            self.line(
                indent,
                &format!("Query (correlated on {})", names.join(", ")),
            );
        }
        let i = indent + 1;
        if let Some(from) = &q.from {
            self.table_ref(from, i);
        }
        if let Some(f) = &q.filter {
            self.expr_line(i, "filter:", f);
        }
        for k in &q.group_by {
            let label = format!("group {} =", self.name(k.id));
            self.expr_line(i, &label, &k.expr);
        }
        for a in &q.aggregates {
            let mut subs = Vec::new();
            let arg = match &a.arg {
                Some(e) => self.expr(e, &mut subs),
                None => "*".into(),
            };
            let distinct = if a.distinct { "DISTINCT " } else { "" };
            let text = format!(
                "agg {}: {} = {}({distinct}{arg})",
                self.name(a.id),
                a.ty,
                a.func.name()
            );
            self.line(i, &text);
            self.subqueries(subs, i);
        }
        if let Some(h) = &q.having {
            self.expr_line(i, "having:", h);
        }
        if q.distinct {
            self.line(i, "distinct");
        }
        for (n, o) in q.projection.iter().enumerate() {
            let kind = if n < q.visible { "output" } else { "hidden" };
            let label = format!("{kind} {}: {} =", o.name, o.ty());
            self.expr_line(i, &label, &o.expr);
        }
        if !q.order_by.is_empty() {
            let keys: Vec<String> = q
                .order_by
                .iter()
                .map(|o| {
                    let dir = if o.asc { "ASC" } else { "DESC" };
                    let nulls = if o.nulls_first {
                        "NULLS FIRST"
                    } else {
                        "NULLS LAST"
                    };
                    format!("{} {dir} {nulls}", self.name(o.column))
                })
                .collect();
            self.line(i, &format!("order by: {}", keys.join(", ")));
        }
        if let Some(l) = q.limit {
            self.line(i, &format!("limit: {l}"));
        }
        if let Some(o) = q.offset {
            self.line(i, &format!("offset: {o}"));
        }
    }

    fn table_ref(&mut self, t: &'a BoundTableRef, indent: usize) {
        match t {
            BoundTableRef::Base { table, columns } => {
                let alias = columns
                    .first()
                    .and_then(|&id| self.cols[id].qualifier.clone());
                match alias {
                    Some(a) if a != *table => self.line(indent, &format!("scan {table} AS {a}")),
                    _ => self.line(indent, &format!("scan {table}")),
                }
            }
            BoundTableRef::Derived { query, columns } => {
                let alias = columns
                    .first()
                    .and_then(|&id| self.cols[id].qualifier.clone());
                let names: Vec<&str> = columns
                    .iter()
                    .map(|&id| self.cols[id].name.as_str())
                    .collect();
                let alias = alias.unwrap_or_else(|| "<unnamed>".into());
                self.line(indent, &format!("derived {alias} ({})", names.join(", ")));
                self.query(query, indent + 1);
            }
            BoundTableRef::Join {
                left,
                right,
                kind,
                on,
            } => {
                let kind = format!("{kind:?}").to_ascii_lowercase();
                match on {
                    Some(on) => self.expr_line(indent, &format!("{kind} join on"), on),
                    None => self.line(indent, &format!("{kind} join")),
                }
                self.table_ref(left, indent + 1);
                self.table_ref(right, indent + 1);
            }
        }
    }

    fn expr_line(&mut self, indent: usize, label: &str, e: &'a BoundExpr) {
        let mut subs = Vec::new();
        let text = self.expr(e, &mut subs);
        self.line(indent, &format!("{label} {text}"));
        self.subqueries(subs, indent);
    }

    fn subqueries(&mut self, subs: Vec<&'a BoundQuery>, indent: usize) {
        for (n, s) in subs.into_iter().enumerate() {
            self.line(indent + 1, &format!("$sub{n}:"));
            self.query(s, indent + 2);
        }
    }

    fn expr(&self, e: &'a BoundExpr, subs: &mut Vec<&'a BoundQuery>) -> String {
        let not = |n: bool| if n { "NOT " } else { "" };
        match &e.kind {
            ExprKind::Column(id) => self.name(*id),
            ExprKind::Literal(v) => match v {
                Value::Utf8(s) => format!("'{}'", s.replace('\'', "''")),
                Value::Date(_) => format!("DATE '{v}'"),
                Value::Interval { .. } => format!("INTERVAL '{v}'"),
                Value::Null => format!("NULL::{}", e.ty),
                _ => v.to_string(),
            },
            ExprKind::Unary {
                op: UnaryOp::Not,
                expr,
            } => format!("NOT {}", self.expr(expr, subs)),
            ExprKind::Unary { expr, .. } => format!("-{}", self.expr(expr, subs)),
            ExprKind::Binary { left, op, right } => {
                format!(
                    "({} {op} {})",
                    self.expr(left, subs),
                    self.expr(right, subs)
                )
            }
            ExprKind::IsNull { expr, negated } => {
                format!("{} IS {}NULL", self.expr(expr, subs), not(*negated))
            }
            ExprKind::Like {
                expr,
                pattern,
                negated,
            } => {
                format!(
                    "{} {}LIKE {}",
                    self.expr(expr, subs),
                    not(*negated),
                    self.expr(pattern, subs)
                )
            }
            ExprKind::InList {
                expr,
                list,
                negated,
            } => {
                let items: Vec<String> = list.iter().map(|x| self.expr(x, subs)).collect();
                format!(
                    "{} {}IN ({})",
                    self.expr(expr, subs),
                    not(*negated),
                    items.join(", ")
                )
            }
            ExprKind::Case {
                operand,
                branches,
                else_result,
            } => {
                let mut s = String::from("CASE");
                if let Some(o) = operand {
                    s += &format!(" {}", self.expr(o, subs));
                }
                for (w, t) in branches {
                    s += &format!(" WHEN {} THEN {}", self.expr(w, subs), self.expr(t, subs));
                }
                if let Some(x) = else_result {
                    s += &format!(" ELSE {}", self.expr(x, subs));
                }
                s + " END"
            }
            ExprKind::Cast { expr } => format!("CAST({} AS {})", self.expr(expr, subs), e.ty),
            ExprKind::Function { func, args } => {
                let args: Vec<String> = args.iter().map(|a| self.expr(a, subs)).collect();
                format!("{}({})", func.name(), args.join(", "))
            }
            ExprKind::Subquery { query, kind } => {
                let tag = format!("$sub{}", subs.len());
                subs.push(query);
                match kind {
                    SubqueryKind::Scalar => tag,
                    SubqueryKind::Exists { negated } => format!("{}EXISTS {tag}", not(*negated)),
                    SubqueryKind::In { expr, negated } => {
                        format!("{} {}IN {tag}", self.expr(expr, subs), not(*negated))
                    }
                }
            }
        }
    }
}

impl OutputColumn {
    pub fn ty(&self) -> Type {
        self.expr.ty
    }
}
