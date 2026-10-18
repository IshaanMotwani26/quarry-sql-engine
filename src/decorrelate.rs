//! Subquery decorrelation: turns `EXISTS`, `NOT EXISTS`, and `IN` subqueries
//! in WHERE (and HAVING) into semi and anti joins.
//!
//! Run as written, a correlated subquery executes once per outer row:
//!
//! ```sql
//! SELECT c_name FROM customer
//! WHERE NOT EXISTS (SELECT * FROM orders WHERE o_custkey = c_custkey)
//! ```
//!
//! scans `orders` once for every customer, which is quadratic. The same
//! question can be answered with one anti join: build a hash table of
//! `orders` on `o_custkey` once, then keep each customer whose key isn't in
//! it. The subquery's correlated conditions (those reading outer columns)
//! become the join condition, and its other conditions filter its input.
//!
//! The rewrite applies only when it's exactly equivalent:
//!
//! - The subquery is a top-level conjunct of the filter, so a FALSE and a
//!   NULL result both drop the row (true of EXISTS and IN in WHERE).
//! - The subquery has no aggregation, LIMIT, or OFFSET, which would change
//!   meaning if its rows were joined instead of computed per outer row.
//! - Outer columns appear only in the subquery's WHERE conditions and, for
//!   IN, its select expression, not in its FROM clause. Those conditions
//!   become the join condition, which is evaluated with both the outer row
//!   and the subquery's row in scope, so even a deeper subquery inside them
//!   can still read the outer row. The FROM clause becomes the join's right
//!   input, which runs once, without any outer row.
//!
//! `NOT IN` is not rewritten: when the subquery produces a NULL, `NOT IN` is
//! never TRUE, which an anti join doesn't model.

use std::collections::HashSet;

use crate::ast::BinaryOp;
use crate::bound::{BoundExpr, BoundQuery, BoundTableRef, ColumnId, ExprKind, SubqueryKind};
use crate::plan::{columns_of, conjoin, plan_table_ref, split_conjuncts, Plan};
use crate::types::Type;

/// Rewrites every eligible subquery condition in `plan` into a semi or anti join.
pub fn decorrelate(plan: Plan) -> Plan {
    let rec = |p: Box<Plan>| Box::new(decorrelate(*p));
    match plan {
        Plan::Filter { input, predicate } => {
            let mut plan = decorrelate(*input);
            let outer: HashSet<ColumnId> = plan.output().into_iter().collect();
            let mut keep = Vec::new();
            for c in split_conjuncts(predicate) {
                match semi_join_for(&c, &outer) {
                    Some((right, on, anti)) => {
                        let right = Box::new(decorrelate(right));
                        plan = Plan::SemiJoin {
                            left: Box::new(plan),
                            right,
                            on,
                            anti,
                        };
                    }
                    None => keep.push(c),
                }
            }
            match conjoin(keep) {
                Some(predicate) => Plan::Filter {
                    input: Box::new(plan),
                    predicate,
                },
                None => plan,
            }
        }
        Plan::Project { input, exprs } => Plan::Project {
            input: rec(input),
            exprs,
        },
        Plan::Join {
            left,
            right,
            kind,
            on,
        } => Plan::Join {
            left: rec(left),
            right: rec(right),
            kind,
            on,
        },
        Plan::SemiJoin {
            left,
            right,
            on,
            anti,
        } => Plan::SemiJoin {
            left: rec(left),
            right: rec(right),
            on,
            anti,
        },
        Plan::Aggregate {
            input,
            group_by,
            aggregates,
        } => Plan::Aggregate {
            input: rec(input),
            group_by,
            aggregates,
        },
        Plan::Distinct { input } => Plan::Distinct { input: rec(input) },
        Plan::Sort { input, keys } => Plan::Sort {
            input: rec(input),
            keys,
        },
        Plan::Limit {
            input,
            limit,
            offset,
        } => Plan::Limit {
            input: rec(input),
            limit,
            offset,
        },
        p @ (Plan::Scan { .. } | Plan::SingleRow) => p,
    }
}

/// True when `e` reads any column in `outer`, directly or from inside a
/// nested subquery.
fn reads(e: &BoundExpr, outer: &HashSet<ColumnId>) -> bool {
    columns_of(e).iter().any(|c| outer.contains(c)) || nested_subquery_reads(e, outer)
}

fn nested_subquery_reads(e: &BoundExpr, outer: &HashSet<ColumnId>) -> bool {
    let mut found = false;
    e.walk(&mut |x| {
        if let ExprKind::Subquery { query, .. } = &x.kind {
            found |= query.correlated.iter().any(|c| outer.contains(c));
        }
    });
    found
}

/// True when a FROM clause reads outer columns (in a join condition or a
/// derived table).
fn from_reads(t: &BoundTableRef, outer: &HashSet<ColumnId>) -> bool {
    match t {
        BoundTableRef::Base { .. } => false,
        BoundTableRef::Derived { query, .. } => query.correlated.iter().any(|c| outer.contains(c)),
        BoundTableRef::Join {
            left, right, on, ..
        } => {
            from_reads(left, outer)
                || from_reads(right, outer)
                || on.as_ref().is_some_and(|e| reads(e, outer))
        }
    }
}

/// If `c` is an eligible `[NOT] EXISTS` or `IN` subquery over the columns
/// `outer`, returns the semi join's right input, condition, and `anti` flag.
fn semi_join_for(
    c: &BoundExpr,
    outer: &HashSet<ColumnId>,
) -> Option<(Plan, Option<BoundExpr>, bool)> {
    let ExprKind::Subquery { query, kind } = &c.kind else {
        return None;
    };
    let (anti, in_expr) = match kind {
        SubqueryKind::Exists { negated } => (*negated, None),
        SubqueryKind::In {
            expr,
            negated: false,
        } => (false, Some(expr)),
        _ => return None,
    };
    let q: &BoundQuery = query;
    if q.is_aggregate() || q.limit.is_some() || q.offset.is_some_and(|o| o > 0) {
        return None;
    }
    let from = q.from.as_ref()?;
    if from_reads(from, outer) {
        return None;
    }

    // Split the subquery's WHERE: conditions reading outer columns become the
    // join condition; the rest filter the subquery's own rows.
    let (mut on, mut local) = (Vec::new(), Vec::new());
    for part in q.filter.clone().map(split_conjuncts).unwrap_or_default() {
        if reads(&part, outer) {
            on.push(part);
        } else {
            local.push(part);
        }
    }
    if let Some(expr) = in_expr {
        // x IN (SELECT y ...) matches when x = y.
        let output = &q.outputs().first()?.expr;
        let eq = ExprKind::Binary {
            left: expr.clone(),
            op: BinaryOp::Eq,
            right: Box::new(output.clone()),
        };
        on.push(BoundExpr::new(eq, Type::Boolean));
    }

    let mut right = plan_table_ref(from);
    if let Some(predicate) = conjoin(local) {
        right = Plan::Filter {
            input: Box::new(right),
            predicate,
        };
    }
    Some((right, conjoin(on), anti))
}
