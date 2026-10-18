//! Logical plan: a tree of relational operators built from a `BoundQuery`.
//!
//! Every node produces rows whose columns are identified by `ColumnId`s, in
//! the order given by `Plan::output`. Expressions inside the plan refer to
//! columns only by id, so the executor maps ids to row positions once when it
//! builds each operator, and the optimizer (Phase 5) can move expressions
//! between nodes freely.
//!
//! The planner follows SQL's logical evaluation order:
//!
//! ```text
//! FROM -> WHERE -> GROUP BY/aggregates -> HAVING -> SELECT -> DISTINCT
//!      -> ORDER BY -> LIMIT/OFFSET -> drop hidden ORDER BY columns
//! ```
//!
//! One rewrite happens during planning: a WHERE condition that compares
//! columns from both sides of an inner or comma join moves into that join's
//! condition. `FROM a, b WHERE a.x = b.y` otherwise plans as a cross product
//! followed by a filter, and the executor can't use a hash join for it.
//! General predicate pushdown (filters into scans, through projections and
//! outer joins) is the optimizer's job in Phase 5.

use std::collections::HashSet;

use crate::ast::{BinaryOp, JoinKind};
use crate::bound::{
    fmt_expr, AggregateCall, BoundExpr, BoundOrderBy, BoundQuery, BoundTableRef, ColumnId,
    ColumnInfo, ExprKind, GroupKey,
};
use crate::catalog::Catalog;
use crate::types::Type;

#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    /// One row with no columns: the input to a SELECT with no FROM clause.
    SingleRow,
    /// Reads a base table. Produces the table columns at positions
    /// `indices`, under the ids `columns` (same length). Unpruned, `indices`
    /// is `0..width`; projection pruning removes columns nothing reads.
    Scan {
        table: String,
        columns: Vec<ColumnId>,
        indices: Vec<usize>,
        /// The table's total column count, for `\explain`.
        width: usize,
    },
    /// Keeps rows where `predicate` is TRUE (NULL and FALSE are dropped).
    Filter {
        input: Box<Plan>,
        predicate: BoundExpr,
    },
    /// Computes one output column per expression.
    Project {
        input: Box<Plan>,
        exprs: Vec<(ColumnId, BoundExpr)>,
    },
    /// Output is the left columns followed by the right columns.
    Join {
        left: Box<Plan>,
        right: Box<Plan>,
        kind: JoinKind,
        on: Option<BoundExpr>,
    },
    /// Output is the group keys followed by the aggregate results.
    Aggregate {
        input: Box<Plan>,
        group_by: Vec<GroupKey>,
        aggregates: Vec<AggregateCall>,
    },
    /// Removes duplicate rows (NULLs compare equal here, as in SQL DISTINCT).
    Distinct { input: Box<Plan> },
    Sort {
        input: Box<Plan>,
        keys: Vec<BoundOrderBy>,
    },
    Limit {
        input: Box<Plan>,
        limit: Option<u64>,
        offset: u64,
    },
    /// Keeps the left rows that have at least one match on the right (or,
    /// with `anti`, no match). Output is the left columns only. This is how
    /// `EXISTS`, `NOT EXISTS`, and `IN` subqueries run once decorrelated.
    SemiJoin {
        left: Box<Plan>,
        right: Box<Plan>,
        on: Option<BoundExpr>,
        anti: bool,
    },
}

/// Builds the logical plan for a bound query. Its output columns are exactly
/// the query's visible outputs, in order.
pub fn plan_query(q: &BoundQuery) -> Plan {
    let mut plan = match &q.from {
        Some(from) => plan_table_ref(from),
        None => Plan::SingleRow,
    };
    if let Some(filter) = &q.filter {
        let mut slots: Vec<Option<BoundExpr>> = split_conjuncts(filter.clone())
            .into_iter()
            .map(Some)
            .collect();
        let from_columns: HashSet<ColumnId> = plan.output().into_iter().collect();
        plan = place_join_conditions(plan, &mut slots, &from_columns);
        if let Some(predicate) = conjoin(slots.into_iter().flatten().collect()) {
            plan = Plan::Filter {
                input: Box::new(plan),
                predicate,
            };
        }
    }
    if q.is_aggregate() {
        plan = Plan::Aggregate {
            input: Box::new(plan),
            group_by: q.group_by.clone(),
            aggregates: q.aggregates.clone(),
        };
    }
    if let Some(predicate) = &q.having {
        plan = Plan::Filter {
            input: Box::new(plan),
            predicate: predicate.clone(),
        };
    }
    // Hidden ORDER BY columns are computed here alongside the visible ones...
    let exprs = q
        .projection
        .iter()
        .map(|o| (o.id, o.expr.clone()))
        .collect();
    plan = Plan::Project {
        input: Box::new(plan),
        exprs,
    };
    if q.distinct {
        plan = Plan::Distinct {
            input: Box::new(plan),
        };
    }
    if !q.order_by.is_empty() {
        plan = Plan::Sort {
            input: Box::new(plan),
            keys: q.order_by.clone(),
        };
    }
    if q.limit.is_some() || q.offset.is_some_and(|o| o > 0) {
        plan = Plan::Limit {
            input: Box::new(plan),
            limit: q.limit,
            offset: q.offset.unwrap_or(0),
        };
    }
    // ...and dropped after sorting.
    if q.visible < q.projection.len() {
        let exprs = q
            .outputs()
            .iter()
            .map(|o| (o.id, BoundExpr::column(o.id, o.ty())))
            .collect();
        plan = Plan::Project {
            input: Box::new(plan),
            exprs,
        };
    }
    plan
}

fn plan_table_ref(t: &BoundTableRef) -> Plan {
    match t {
        BoundTableRef::Base { table, columns } => Plan::Scan {
            table: table.clone(),
            columns: columns.clone(),
            indices: (0..columns.len()).collect(),
            width: columns.len(),
        },
        BoundTableRef::Derived { query, columns } => {
            // A derived table is its own plan, renamed to the ids the outer
            // query knows it by.
            let inner = plan_query(query);
            let exprs = columns
                .iter()
                .zip(query.outputs())
                .map(|(&outer_id, out)| (outer_id, BoundExpr::column(out.id, out.ty())))
                .collect();
            Plan::Project {
                input: Box::new(inner),
                exprs,
            }
        }
        BoundTableRef::Join {
            left,
            right,
            kind,
            on,
        } => Plan::Join {
            left: Box::new(plan_table_ref(left)),
            right: Box::new(plan_table_ref(right)),
            kind: *kind,
            on: on.clone(),
        },
    }
}

/// Moves each WHERE conjunct that references both sides of an inner or cross
/// join into the lowest such join's condition. A cross join that gains a
/// condition becomes an inner join; the two are equivalent for any condition.
///
/// Outer joins are not entered: moving a condition below a LEFT JOIN changes
/// its result. Conjuncts containing subqueries stay in WHERE, because their
/// correlated column references aren't visible to this analysis.
/// `from_columns` is every column the FROM clause produces; columns outside
/// it are outer (correlated) references and act as constants here.
fn place_join_conditions(
    plan: Plan,
    slots: &mut [Option<BoundExpr>],
    from_columns: &HashSet<ColumnId>,
) -> Plan {
    let Plan::Join {
        left,
        right,
        kind,
        on,
    } = plan
    else {
        return plan;
    };
    if !matches!(kind, JoinKind::Inner | JoinKind::Cross) {
        return Plan::Join {
            left,
            right,
            kind,
            on,
        };
    }
    // Children first, so each conjunct lands at the lowest join that covers it.
    let left = place_join_conditions(*left, slots, from_columns);
    let right = place_join_conditions(*right, slots, from_columns);
    let (l, r): (HashSet<ColumnId>, HashSet<ColumnId>) = (
        left.output().into_iter().collect(),
        right.output().into_iter().collect(),
    );
    let mut parts = on.map(split_conjuncts).unwrap_or_default();
    for slot in slots.iter_mut() {
        let belongs_here = slot.as_ref().is_some_and(|c| {
            if contains_subquery(c) {
                return false;
            }
            let local: Vec<ColumnId> = columns_of(c)
                .into_iter()
                .filter(|id| from_columns.contains(id))
                .collect();
            local.iter().all(|id| l.contains(id) || r.contains(id))
                && local.iter().any(|id| l.contains(id))
                && local.iter().any(|id| r.contains(id))
        });
        if belongs_here {
            parts.extend(slot.take());
        }
    }
    let kind = if kind == JoinKind::Cross && !parts.is_empty() {
        JoinKind::Inner
    } else {
        kind
    };
    Plan::Join {
        left: Box::new(left),
        right: Box::new(right),
        kind,
        on: conjoin(parts),
    }
}

/// Splits `a AND b AND c` into `[a, b, c]`.
pub fn split_conjuncts(e: BoundExpr) -> Vec<BoundExpr> {
    let mut out = Vec::new();
    let mut stack = vec![e];
    while let Some(e) = stack.pop() {
        match e.kind {
            ExprKind::Binary {
                left,
                op: BinaryOp::And,
                right,
            } => {
                stack.push(*right);
                stack.push(*left);
            }
            kind => out.push(BoundExpr::new(kind, e.ty)),
        }
    }
    out
}

/// The inverse of `split_conjuncts`: `[a, b, c]` becomes `(a AND b) AND c`.
pub fn conjoin(parts: Vec<BoundExpr>) -> Option<BoundExpr> {
    parts.into_iter().reduce(|acc, e| {
        BoundExpr::new(
            ExprKind::Binary {
                left: Box::new(acc),
                op: BinaryOp::And,
                right: Box::new(e),
            },
            Type::Boolean,
        )
    })
}

pub(crate) fn columns_of(e: &BoundExpr) -> HashSet<ColumnId> {
    let mut cols = HashSet::new();
    e.walk(&mut |x| {
        if let ExprKind::Column(id) = x.kind {
            cols.insert(id);
        }
    });
    cols
}

pub(crate) fn contains_subquery(e: &BoundExpr) -> bool {
    let mut found = false;
    e.walk(&mut |x| found |= matches!(x.kind, ExprKind::Subquery { .. }));
    found
}

/// A join condition split into equality keys and everything else. Borrows
/// from the plan, so the executor can hold it for the life of the operator.
#[derive(Debug, Clone, PartialEq)]
pub struct JoinKeys<'a> {
    /// `(left expression, right expression)` pairs that must be equal.
    pub keys: Vec<(&'a BoundExpr, &'a BoundExpr)>,
    /// The remaining conjuncts; a key match counts only if all are TRUE.
    pub residual: Vec<&'a BoundExpr>,
}

/// Borrowing version of `split_conjuncts`.
fn conjuncts_ref<'a>(e: &'a BoundExpr, out: &mut Vec<&'a BoundExpr>) {
    match &e.kind {
        ExprKind::Binary {
            left,
            op: BinaryOp::And,
            right,
        } => {
            conjuncts_ref(left, out);
            conjuncts_ref(right, out);
        }
        _ => out.push(e),
    }
}

#[derive(PartialEq)]
enum Side {
    None,
    Left,
    Right,
    Both,
}

/// Splits a join condition into hash-join keys and a residual. A conjunct
/// `x = y` is a key when `x` reads only left columns and `y` only right
/// columns (or the reverse, which is swapped). Columns from neither side are
/// outer references and count as constants.
pub fn join_keys<'a>(on: &'a BoundExpr, left: &[ColumnId], right: &[ColumnId]) -> JoinKeys<'a> {
    let side = |e: &BoundExpr| {
        if contains_subquery(e) {
            return Side::Both; // never a key
        }
        let cols = columns_of(e);
        let in_left = cols.iter().any(|id| left.contains(id));
        let in_right = cols.iter().any(|id| right.contains(id));
        match (in_left, in_right) {
            (false, false) => Side::None,
            (true, false) => Side::Left,
            (false, true) => Side::Right,
            (true, true) => Side::Both,
        }
    };
    let mut parts = Vec::new();
    conjuncts_ref(on, &mut parts);
    let mut split = JoinKeys {
        keys: Vec::new(),
        residual: Vec::new(),
    };
    for conjunct in parts {
        if let ExprKind::Binary {
            left: a,
            op: BinaryOp::Eq,
            right: b,
        } = &conjunct.kind
        {
            match (side(a), side(b)) {
                (Side::Left, Side::Right) => {
                    split.keys.push((&**a, &**b));
                    continue;
                }
                (Side::Right, Side::Left) => {
                    split.keys.push((&**b, &**a));
                    continue;
                }
                _ => {}
            }
        }
        split.residual.push(conjunct);
    }
    split
}

impl Plan {
    /// Ids of the columns this node produces, in row order.
    pub fn output(&self) -> Vec<ColumnId> {
        match self {
            Plan::SingleRow => Vec::new(),
            Plan::Scan { columns, .. } => columns.clone(),
            Plan::Filter { input, .. }
            | Plan::Distinct { input }
            | Plan::Sort { input, .. }
            | Plan::Limit { input, .. }
            | Plan::SemiJoin { left: input, .. } => input.output(),
            Plan::Project { exprs, .. } => exprs.iter().map(|(id, _)| *id).collect(),
            Plan::Join { left, right, .. } => {
                let mut out = left.output();
                out.extend(right.output());
                out
            }
            Plan::Aggregate {
                group_by,
                aggregates,
                ..
            } => group_by
                .iter()
                .map(|k| k.id)
                .chain(aggregates.iter().map(|a| a.id))
                .collect(),
        }
    }

    pub fn children(&self) -> Vec<&Plan> {
        match self {
            Plan::SingleRow | Plan::Scan { .. } => Vec::new(),
            Plan::Filter { input, .. }
            | Plan::Project { input, .. }
            | Plan::Aggregate { input, .. }
            | Plan::Distinct { input }
            | Plan::Sort { input, .. }
            | Plan::Limit { input, .. } => vec![input],
            Plan::Join { left, right, .. } | Plan::SemiJoin { left, right, .. } => {
                vec![left, right]
            }
        }
    }

    /// An indented, human-readable rendering of the plan (the REPL's `\explain`).
    /// Subqueries inside expressions appear as `$subN` with their own plans
    /// printed underneath the node that uses them.
    pub fn explain(&self, cols: &[ColumnInfo]) -> String {
        self.explain_with(cols, None)
    }

    /// Like `explain`. Pass the catalog when `self` came from the optimizer,
    /// so subquery plans are shown optimized too.
    pub fn explain_with(&self, cols: &[ColumnInfo], optimized: Option<&Catalog>) -> String {
        let mut out = String::new();
        explain_into(self, cols, 0, optimized, &mut out);
        out
    }
}

fn line(out: &mut String, indent: usize, text: &str) {
    out.push_str(&"  ".repeat(indent));
    out.push_str(text);
    out.push('\n');
}

/// Explain text for a join: its hash keys and residual when it has
/// equality keys, otherwise the nested-loop condition.
fn describe_join<'a>(
    hash_name: &str,
    loop_name: &str,
    on: Option<&'a BoundExpr>,
    left: &Plan,
    right: &Plan,
    cols: &[ColumnInfo],
    subs: &mut Vec<&'a BoundQuery>,
) -> String {
    match on.map(|on| join_keys(on, &left.output(), &right.output())) {
        Some(JoinKeys { keys, residual }) if !keys.is_empty() => {
            let keys: Vec<String> = keys
                .iter()
                .map(|(l, r)| format!("{} = {}", fmt_expr(cols, l, subs), fmt_expr(cols, r, subs)))
                .collect();
            let mut text = format!("{hash_name} keys=[{}]", keys.join(", "));
            if !residual.is_empty() {
                let parts: Vec<String> = residual.iter().map(|r| fmt_expr(cols, r, subs)).collect();
                text += &format!(" residual=[{}]", parts.join(", "));
            }
            text
        }
        _ => match on {
            Some(on) => format!("{loop_name} on {}", fmt_expr(cols, on, subs)),
            None => loop_name.to_string(),
        },
    }
}

fn explain_into(
    plan: &Plan,
    cols: &[ColumnInfo],
    indent: usize,
    optimized: Option<&Catalog>,
    out: &mut String,
) {
    let name = |id: ColumnId| cols[id].display_name();
    let mut subs: Vec<&BoundQuery> = Vec::new();
    let text = match plan {
        Plan::SingleRow => "SingleRow".to_string(),
        Plan::Scan {
            table,
            columns,
            width,
            ..
        } => {
            let mut text = match columns
                .first()
                .and_then(|&id| cols[id].qualifier.as_deref())
            {
                Some(alias) if alias != table => format!("Scan {table} AS {alias}"),
                _ => format!("Scan {table}"),
            };
            if columns.len() < *width {
                let names: Vec<&str> = columns.iter().map(|&id| cols[id].name.as_str()).collect();
                text += &format!(
                    " [{}/{} columns: {}]",
                    columns.len(),
                    width,
                    names.join(", ")
                );
            }
            text
        }
        Plan::Filter { predicate, .. } => {
            format!("Filter {}", fmt_expr(cols, predicate, &mut subs))
        }
        Plan::Project { exprs, .. } => {
            let items: Vec<String> = exprs
                .iter()
                .map(|(id, e)| {
                    let rendered = fmt_expr(cols, e, &mut subs);
                    // A column passed through under its own name prints once.
                    let same_name =
                        matches!(e.kind, ExprKind::Column(src) if cols[src].name == cols[*id].name);
                    if same_name || rendered == name(*id) {
                        rendered
                    } else {
                        format!("{} := {rendered}", name(*id))
                    }
                })
                .collect();
            format!("Project {}", items.join(", "))
        }
        Plan::Join {
            left,
            right,
            kind,
            on,
        } => {
            let kind = match kind {
                JoinKind::Inner => "Inner",
                JoinKind::Left => "Left",
                JoinKind::Right => "Right",
                JoinKind::Full => "Full",
                JoinKind::Cross => "Cross",
            };
            describe_join(
                &format!("HashJoin {kind}"),
                &format!("NestedLoopJoin {kind}"),
                on.as_ref(),
                left,
                right,
                cols,
                &mut subs,
            )
        }
        Plan::SemiJoin {
            left,
            right,
            on,
            anti,
        } => {
            let kind = if *anti { "Anti" } else { "Semi" };
            describe_join(
                &format!("Hash{kind}Join"),
                &format!("NestedLoop{kind}Join"),
                on.as_ref(),
                left,
                right,
                cols,
                &mut subs,
            )
        }
        Plan::Aggregate {
            group_by,
            aggregates,
            ..
        } => {
            let keys: Vec<String> = group_by
                .iter()
                .map(|k| fmt_expr(cols, &k.expr, &mut subs))
                .collect();
            let aggs: Vec<String> = aggregates
                .iter()
                .map(|a| {
                    let arg = match &a.arg {
                        Some(e) => fmt_expr(cols, e, &mut subs),
                        None => "*".into(),
                    };
                    let distinct = if a.distinct { "DISTINCT " } else { "" };
                    format!("{}({distinct}{arg})", a.func.name())
                })
                .collect();
            format!(
                "Aggregate keys=[{}] aggs=[{}]",
                keys.join(", "),
                aggs.join(", ")
            )
        }
        Plan::Distinct { .. } => "Distinct".to_string(),
        Plan::Sort { keys, .. } => {
            let keys: Vec<String> = keys
                .iter()
                .map(|k| {
                    let dir = if k.asc { "ASC" } else { "DESC" };
                    let nulls = if k.nulls_first {
                        "NULLS FIRST"
                    } else {
                        "NULLS LAST"
                    };
                    format!("{} {dir} {nulls}", name(k.column))
                })
                .collect();
            format!("Sort {}", keys.join(", "))
        }
        Plan::Limit { limit, offset, .. } => match limit {
            Some(l) if *offset > 0 => format!("Limit {l} offset {offset}"),
            Some(l) => format!("Limit {l}"),
            None => format!("Offset {offset}"),
        },
    };
    line(out, indent, &text);
    for (n, sub) in subs.into_iter().enumerate() {
        let corr = if sub.correlated.is_empty() {
            ""
        } else {
            " (correlated)"
        };
        line(out, indent + 2, &format!("$sub{n}{corr}:"));
        let sub_plan = plan_query(sub);
        let sub_plan = match optimized {
            Some(catalog) => crate::optimizer::optimize(sub_plan, catalog),
            None => sub_plan,
        };
        explain_into(&sub_plan, cols, indent + 3, optimized, out);
    }
    for child in plan.children() {
        explain_into(child, cols, indent + 1, optimized, out);
    }
}
