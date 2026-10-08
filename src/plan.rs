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

use crate::ast::JoinKind;
use crate::bound::{
    fmt_expr, AggregateCall, BoundExpr, BoundOrderBy, BoundQuery, BoundTableRef, ColumnId,
    ColumnInfo, ExprKind, GroupKey,
};

#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    /// One row with no columns: the input to a SELECT with no FROM clause.
    SingleRow,
    /// Reads a base table. `columns[i]` is the id of the table's i-th column.
    Scan {
        table: String,
        columns: Vec<ColumnId>,
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
}

/// Builds the logical plan for a bound query. Its output columns are exactly
/// the query's visible outputs, in order.
pub fn plan_query(q: &BoundQuery) -> Plan {
    let mut plan = match &q.from {
        Some(from) => plan_table_ref(from),
        None => Plan::SingleRow,
    };
    if let Some(predicate) = &q.filter {
        plan = Plan::Filter {
            input: Box::new(plan),
            predicate: predicate.clone(),
        };
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

impl Plan {
    /// Ids of the columns this node produces, in row order.
    pub fn output(&self) -> Vec<ColumnId> {
        match self {
            Plan::SingleRow => Vec::new(),
            Plan::Scan { columns, .. } => columns.clone(),
            Plan::Filter { input, .. }
            | Plan::Distinct { input }
            | Plan::Sort { input, .. }
            | Plan::Limit { input, .. } => input.output(),
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
            Plan::Join { left, right, .. } => vec![left, right],
        }
    }

    /// An indented, human-readable rendering of the plan (the REPL's `\explain`).
    /// Subqueries inside expressions appear as `$subN` with their own plans
    /// printed underneath the node that uses them.
    pub fn explain(&self, cols: &[ColumnInfo]) -> String {
        let mut out = String::new();
        explain_into(self, cols, 0, &mut out);
        out
    }
}

fn line(out: &mut String, indent: usize, text: &str) {
    out.push_str(&"  ".repeat(indent));
    out.push_str(text);
    out.push('\n');
}

fn explain_into(plan: &Plan, cols: &[ColumnInfo], indent: usize, out: &mut String) {
    let name = |id: ColumnId| cols[id].display_name();
    let mut subs: Vec<&BoundQuery> = Vec::new();
    let text = match plan {
        Plan::SingleRow => "SingleRow".to_string(),
        Plan::Scan { table, columns } => match columns
            .first()
            .and_then(|&id| cols[id].qualifier.as_deref())
        {
            Some(alias) if alias != table => format!("Scan {table} AS {alias}"),
            _ => format!("Scan {table}"),
        },
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
        Plan::Join { kind, on, .. } => {
            let kind = match kind {
                JoinKind::Inner => "InnerJoin",
                JoinKind::Left => "LeftJoin",
                JoinKind::Right => "RightJoin",
                JoinKind::Full => "FullJoin",
                JoinKind::Cross => "CrossJoin",
            };
            match on {
                Some(on) => format!("{kind} on {}", fmt_expr(cols, on, &mut subs)),
                None => kind.to_string(),
            }
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
        explain_into(&plan_query(sub), cols, indent + 3, out);
    }
    for child in plan.children() {
        explain_into(child, cols, indent + 1, out);
    }
}
