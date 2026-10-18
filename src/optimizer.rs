//! Rule-based optimizer: rewrites a logical plan into an equivalent, cheaper
//! one. Four passes run in order:
//!
//! 1. **Constant folding** evaluates expressions that read no columns, once,
//!    at plan time: `DATE '1998-12-01' - INTERVAL '90' DAY` becomes
//!    `DATE '1998-09-02'`, and `0.06 - 0.01` becomes `0.05`. It also
//!    simplifies boolean logic (`x AND TRUE` is `x`; `x AND FALSE` is FALSE)
//!    and drops filters that are always TRUE.
//! 2. **Filter pushdown** moves each filter condition as far down the plan as
//!    it can legally go, so rows are discarded before they're joined,
//!    grouped, or sorted. A condition on one table lands directly above that
//!    table's scan; a condition spanning both sides of an inner join becomes
//!    part of that join (where it can become a hash-join key).
//! 3. **Join ordering** (`join_order.rs`) picks the order of each chain of
//!    inner joins using row-count estimates, and builds each hash table on
//!    the smaller input.
//! 4. **Projection pruning** works top-down to find which columns each node's
//!    parent actually reads, and removes the rest. Scans then copy only the
//!    columns the query uses out of storage.
//!
//! Every rule preserves results (row order aside, when there's no ORDER BY);
//! the test suite checks this by running every query with and without the
//! optimizer.

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;

use crate::ast::{BinaryOp, JoinKind};
use crate::bound::{BoundExpr, ColumnId, ExprKind};
use crate::catalog::Catalog;
use crate::eval::{eval, Layout, NoSubqueries, Scope};
use crate::join_order::reorder_joins;
use crate::plan::{columns_of, conjoin, contains_subquery, split_conjuncts, Plan};
use crate::types::Value;

/// Runs all optimizer passes. The catalog supplies table statistics.
pub fn optimize(plan: Plan, catalog: &Catalog) -> Plan {
    let plan = map_exprs(plan, &fold);
    let plan = push_down(plan, Vec::new());
    let plan = reorder_joins(plan, catalog);
    // Pushdown substitutes expressions into new places; fold again.
    let plan = map_exprs(plan, &fold);
    let required: HashSet<ColumnId> = plan.output().into_iter().collect();
    prune(plan, &required)
}

// ---------------------------------------------------------------------------
// Expression helpers
// ---------------------------------------------------------------------------

fn infallible<T>(r: Result<T, Infallible>) -> T {
    match r {
        Ok(v) => v,
        Err(never) => match never {},
    }
}

fn bool_literal(e: &BoundExpr) -> Option<bool> {
    match e.kind {
        ExprKind::Literal(Value::Boolean(b)) => Some(b),
        _ => None,
    }
}

fn is_true(e: &BoundExpr) -> bool {
    bool_literal(e) == Some(true)
}

/// True when the expression reads no columns and runs no subqueries, so its
/// value is the same for every row.
fn is_constant(e: &BoundExpr) -> bool {
    let mut constant = true;
    e.walk(&mut |x| {
        constant &= !matches!(x.kind, ExprKind::Column(_) | ExprKind::Subquery { .. });
    });
    constant
}

/// Constant folding, bottom-up. An expression that fails to evaluate (such as
/// `1 / 0`) is left alone, so the error still happens at run time, and only if
/// that expression is actually reached.
pub fn fold(e: BoundExpr) -> BoundExpr {
    let e = infallible(e.try_map_children(&mut |c| Ok(fold(c))));
    match &e.kind {
        ExprKind::Literal(_) | ExprKind::Column(_) | ExprKind::Subquery { .. } => return e,
        ExprKind::Binary {
            left,
            op: op @ (BinaryOp::And | BinaryOp::Or),
            right,
        } => {
            // FALSE AND x = FALSE, TRUE AND x = x; TRUE OR x = TRUE, FALSE OR x = x.
            // These hold in three-valued logic even when x is NULL.
            let absorbing = *op == BinaryOp::Or;
            match (bool_literal(left), bool_literal(right)) {
                (Some(b), _) | (_, Some(b)) if b == absorbing => {
                    return BoundExpr::literal(Value::Boolean(absorbing));
                }
                (Some(_), _) => return (**right).clone(),
                (_, Some(_)) => return (**left).clone(),
                _ => {}
            }
        }
        _ => {}
    }
    if is_constant(&e) {
        let layout = Layout::default();
        if let Ok(v) = eval(&e, &Scope::new(&layout, &[], None), &NoSubqueries) {
            // Keep the expression's type: a folded NULL is still, say, a BIGINT.
            return BoundExpr::new(ExprKind::Literal(v), e.ty);
        }
    }
    e
}

/// Replaces references to the columns in `map` with their defining expressions.
fn substitute(e: BoundExpr, map: &HashMap<ColumnId, &BoundExpr>) -> BoundExpr {
    if let ExprKind::Column(id) = e.kind {
        return map.get(&id).map_or(e, |&def| def.clone());
    }
    infallible(e.try_map_children(&mut |c| Ok(substitute(c, map))))
}

/// Every column an expression needs from its input. Unlike `columns_of`, this
/// includes columns referenced inside subqueries (their correlated columns),
/// which pruning must not remove.
fn required_columns(e: &BoundExpr) -> HashSet<ColumnId> {
    let mut cols = HashSet::new();
    e.walk(&mut |x| match &x.kind {
        ExprKind::Column(id) => {
            cols.insert(*id);
        }
        ExprKind::Subquery { query, .. } => cols.extend(query.correlated.iter().copied()),
        _ => {}
    });
    cols
}

/// Applies `f` to every expression stored in the plan.
fn map_exprs(plan: Plan, f: &dyn Fn(BoundExpr) -> BoundExpr) -> Plan {
    let rec = |p: Box<Plan>| Box::new(map_exprs(*p, f));
    match plan {
        p @ (Plan::SingleRow | Plan::Scan { .. }) => p,
        Plan::Filter { input, predicate } => {
            let predicate = f(predicate);
            let input = rec(input);
            if is_true(&predicate) {
                *input
            } else {
                Plan::Filter { input, predicate }
            }
        }
        Plan::Project { input, exprs } => Plan::Project {
            input: rec(input),
            exprs: exprs.into_iter().map(|(id, e)| (id, f(e))).collect(),
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
            on: on.map(f).filter(|e| !is_true(e)),
        },
        Plan::Aggregate {
            input,
            mut group_by,
            mut aggregates,
        } => {
            for k in &mut group_by {
                k.expr = f(k.expr.clone());
            }
            for a in &mut aggregates {
                a.arg = a.arg.take().map(f);
            }
            Plan::Aggregate {
                input: rec(input),
                group_by,
                aggregates,
            }
        }
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
        Plan::SemiJoin {
            left,
            right,
            on,
            anti,
        } => Plan::SemiJoin {
            left: rec(left),
            right: rec(right),
            on: on.map(f).filter(|e| !is_true(e)),
            anti,
        },
    }
}

// ---------------------------------------------------------------------------
// Filter pushdown
// ---------------------------------------------------------------------------

/// Puts `preds` in a Filter above `plan` (or returns `plan` if there are none).
fn wrap(plan: Plan, preds: Vec<BoundExpr>) -> Plan {
    match conjoin(preds.into_iter().filter(|p| !is_true(p)).collect()) {
        Some(predicate) => Plan::Filter {
            input: Box::new(plan),
            predicate,
        },
        None => plan,
    }
}

/// Which side of a join a condition reads from. Columns from neither side
/// are outer (correlated) references and act as constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Neither,
    Left,
    Right,
    Both,
    /// Contains a subquery, whose column references aren't visible here;
    /// never moved.
    Opaque,
}

fn side(e: &BoundExpr, left: &HashSet<ColumnId>, right: &HashSet<ColumnId>) -> Side {
    if contains_subquery(e) {
        return Side::Opaque;
    }
    let cols = columns_of(e);
    match (
        cols.iter().any(|c| left.contains(c)),
        cols.iter().any(|c| right.contains(c)),
    ) {
        (false, false) => Side::Neither,
        (true, false) => Side::Left,
        (false, true) => Side::Right,
        (true, true) => Side::Both,
    }
}

/// Pushes the conjuncts in `preds` (conditions that must hold on this node's
/// output) as far down into `plan` as is legal, returning the rewritten plan.
fn push_down(plan: Plan, preds: Vec<BoundExpr>) -> Plan {
    match plan {
        Plan::Filter { input, predicate } => {
            let mut all = split_conjuncts(predicate);
            all.extend(preds);
            push_down(*input, all)
        }
        p @ (Plan::Scan { .. } | Plan::SingleRow) => wrap(p, preds),
        // Filtering commutes with sorting and with DISTINCT.
        Plan::Sort { input, keys } => Plan::Sort {
            input: Box::new(push_down(*input, preds)),
            keys,
        },
        Plan::Distinct { input } => Plan::Distinct {
            input: Box::new(push_down(*input, preds)),
        },
        // ...but not with LIMIT: filtering first would change which rows are kept.
        Plan::Limit {
            input,
            limit,
            offset,
        } => wrap(
            Plan::Limit {
                input: Box::new(push_down(*input, Vec::new())),
                limit,
                offset,
            },
            preds,
        ),
        Plan::Project { input, exprs } => {
            // Rewrite each condition in terms of the projection's input by
            // inlining the expressions it reads. Skip it if that would inline
            // a subquery (duplicating its work) or if it contains one.
            let defs: HashMap<ColumnId, &BoundExpr> =
                exprs.iter().map(|(id, e)| (*id, e)).collect();
            let (mut down, mut stay) = (Vec::new(), Vec::new());
            for p in preds {
                let inlines_subquery = columns_of(&p)
                    .iter()
                    .any(|c| defs.get(c).is_some_and(|d| contains_subquery(d)));
                if contains_subquery(&p) || inlines_subquery {
                    stay.push(p);
                } else {
                    down.push(substitute(p, &defs));
                }
            }
            let input = Box::new(push_down(*input, std::mem::take(&mut down)));
            wrap(Plan::Project { input, exprs }, std::mem::take(&mut stay))
        }
        Plan::Aggregate {
            input,
            group_by,
            aggregates,
        } => {
            // A condition on group keys only (not aggregates) filters whole
            // groups, so it can filter rows before grouping instead. Not with
            // zero group keys: that aggregate returns a row even when its
            // input is empty, so filtering below it would change the result.
            let agg_ids: HashSet<ColumnId> = aggregates.iter().map(|a| a.id).collect();
            let keys: HashMap<ColumnId, &BoundExpr> =
                group_by.iter().map(|k| (k.id, &k.expr)).collect();
            let (mut down, mut stay) = (Vec::new(), Vec::new());
            for p in preds {
                let reads_aggregate = columns_of(&p).iter().any(|c| agg_ids.contains(c));
                if group_by.is_empty() || reads_aggregate || contains_subquery(&p) {
                    stay.push(p);
                } else {
                    down.push(substitute(p, &keys));
                }
            }
            let input = Box::new(push_down(*input, std::mem::take(&mut down)));
            wrap(
                Plan::Aggregate {
                    input,
                    group_by,
                    aggregates,
                },
                std::mem::take(&mut stay),
            )
        }
        Plan::Join {
            left,
            right,
            kind,
            on,
        } => push_into_join(*left, *right, kind, on, preds),
        Plan::SemiJoin {
            left,
            right,
            on,
            anti,
        } => push_into_semi_join(*left, *right, on, anti, preds),
    }
}

/// Distributes a join's own conditions and the conditions above it.
///
/// For an inner join, ON and WHERE are interchangeable, so a condition on one
/// side moves into that side and a condition spanning both stays on the join.
/// Outer joins are where care is needed:
///
/// - Above `a LEFT JOIN b`, a WHERE condition on `a` alone can move into `a`
///   (it removes the same rows either way). One on `b` cannot: it must also
///   see the NULL-padded rows the join creates.
/// - In `a LEFT JOIN b ON ...`, an ON condition on `b` alone can move into
///   `b` (b rows failing it could never match). One on `a` alone cannot: a
///   rows failing it must still appear, padded with NULLs.
///
/// RIGHT JOIN is the mirror image, and nothing moves across a FULL JOIN.
fn push_into_join(
    left: Plan,
    right: Plan,
    kind: JoinKind,
    on: Option<BoundExpr>,
    preds: Vec<BoundExpr>,
) -> Plan {
    let l: HashSet<ColumnId> = left.output().into_iter().collect();
    let r: HashSet<ColumnId> = right.output().into_iter().collect();
    let inner = matches!(kind, JoinKind::Inner | JoinKind::Cross);
    let (mut to_left, mut to_right, mut keep_on, mut stay) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());

    for c in on.map(split_conjuncts).unwrap_or_default() {
        match (kind, side(&c, &l, &r)) {
            (_, Side::Left | Side::Neither) if inner => to_left.push(c),
            (_, Side::Right) if inner => to_right.push(c),
            (JoinKind::Left, Side::Right) => to_right.push(c),
            (JoinKind::Right, Side::Left) => to_left.push(c),
            _ => keep_on.push(c),
        }
    }
    for p in preds {
        match (kind, side(&p, &l, &r)) {
            (_, Side::Left | Side::Neither) if inner => to_left.push(p),
            (_, Side::Right) if inner => to_right.push(p),
            (_, Side::Both) if inner => keep_on.push(p),
            (JoinKind::Left, Side::Left | Side::Neither) => to_left.push(p),
            (JoinKind::Right, Side::Right | Side::Neither) => to_right.push(p),
            _ => stay.push(p),
        }
    }

    let on = conjoin(keep_on);
    let kind = if kind == JoinKind::Cross && on.is_some() {
        JoinKind::Inner
    } else {
        kind
    };
    let join = Plan::Join {
        left: Box::new(push_down(left, to_left)),
        right: Box::new(push_down(right, to_right)),
        kind,
        on,
    };
    wrap(join, stay)
}

/// A semi or anti join outputs only left rows, unchanged, so every condition
/// above it can move into the left input. Of its own conditions, one on the
/// right input alone can move into the right input (right rows failing it
/// can never match). One on the left alone can move into the left input only
/// for a semi join: an anti join must still emit the left rows that fail it.
fn push_into_semi_join(
    left: Plan,
    right: Plan,
    on: Option<BoundExpr>,
    anti: bool,
    preds: Vec<BoundExpr>,
) -> Plan {
    let l: HashSet<ColumnId> = left.output().into_iter().collect();
    let r: HashSet<ColumnId> = right.output().into_iter().collect();
    let (mut to_left, mut to_right, mut keep_on) = (preds, Vec::new(), Vec::new());
    for c in on.map(split_conjuncts).unwrap_or_default() {
        match side(&c, &l, &r) {
            Side::Right => to_right.push(c),
            Side::Left | Side::Neither if !anti => to_left.push(c),
            _ => keep_on.push(c),
        }
    }
    Plan::SemiJoin {
        left: Box::new(push_down(left, to_left)),
        right: Box::new(push_down(right, to_right)),
        on: conjoin(keep_on),
        anti,
    }
}

// ---------------------------------------------------------------------------
// Projection pruning
// ---------------------------------------------------------------------------

fn union(a: &HashSet<ColumnId>, b: HashSet<ColumnId>) -> HashSet<ColumnId> {
    let mut out = a.clone();
    out.extend(b);
    out
}

/// Removes columns that nothing above reads. `required` is the set of column
/// ids the parent needs from this node; the node may still produce others
/// (Filter, Join, and Sort pass through whatever their inputs produce).
fn prune(plan: Plan, required: &HashSet<ColumnId>) -> Plan {
    match plan {
        Plan::SingleRow => Plan::SingleRow,
        Plan::Scan {
            table,
            columns,
            indices,
            width,
        } => {
            let (columns, indices) = columns
                .into_iter()
                .zip(indices)
                .filter(|(id, _)| required.contains(id))
                .unzip();
            Plan::Scan {
                table,
                columns,
                indices,
                width,
            }
        }
        Plan::Filter { input, predicate } => {
            let need = union(required, required_columns(&predicate));
            Plan::Filter {
                input: Box::new(prune(*input, &need)),
                predicate,
            }
        }
        Plan::Project { input, exprs } => {
            let exprs: Vec<(ColumnId, BoundExpr)> = exprs
                .into_iter()
                .filter(|(id, _)| required.contains(id))
                .collect();
            let need = exprs
                .iter()
                .flat_map(|(_, e)| required_columns(e))
                .collect();
            Plan::Project {
                input: Box::new(prune(*input, &need)),
                exprs,
            }
        }
        Plan::Join {
            left,
            right,
            kind,
            on,
        } => {
            let need = union(
                required,
                on.as_ref().map(required_columns).unwrap_or_default(),
            );
            Plan::Join {
                left: Box::new(prune(*left, &need)),
                right: Box::new(prune(*right, &need)),
                kind,
                on,
            }
        }
        Plan::Aggregate {
            input,
            group_by,
            aggregates,
        } => {
            // Group keys always stay: they define the groups. Unused
            // aggregates are dropped.
            let aggregates: Vec<_> = aggregates
                .into_iter()
                .filter(|a| required.contains(&a.id))
                .collect();
            let need = group_by
                .iter()
                .flat_map(|k| required_columns(&k.expr))
                .chain(
                    aggregates
                        .iter()
                        .filter_map(|a| a.arg.as_ref())
                        .flat_map(required_columns),
                )
                .collect();
            Plan::Aggregate {
                input: Box::new(prune(*input, &need)),
                group_by,
                aggregates,
            }
        }
        Plan::Distinct { input } => {
            // Distinctness is over whole rows, so every input column matters.
            let need = input.output().into_iter().collect();
            Plan::Distinct {
                input: Box::new(prune(*input, &need)),
            }
        }
        Plan::Sort { input, keys } => {
            let need = union(required, keys.iter().map(|k| k.column).collect());
            Plan::Sort {
                input: Box::new(prune(*input, &need)),
                keys,
            }
        }
        Plan::Limit {
            input,
            limit,
            offset,
        } => Plan::Limit {
            input: Box::new(prune(*input, required)),
            limit,
            offset,
        },
        Plan::SemiJoin {
            left,
            right,
            on,
            anti,
        } => {
            let need = union(
                required,
                on.as_ref().map(required_columns).unwrap_or_default(),
            );
            Plan::SemiJoin {
                left: Box::new(prune(*left, &need)),
                right: Box::new(prune(*right, &need)),
                on,
                anti,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Type;

    fn lit(v: Value) -> BoundExpr {
        BoundExpr::literal(v)
    }

    fn and(a: BoundExpr, b: BoundExpr) -> BoundExpr {
        BoundExpr::new(
            ExprKind::Binary {
                left: Box::new(a),
                op: BinaryOp::And,
                right: Box::new(b),
            },
            Type::Boolean,
        )
    }

    fn or(a: BoundExpr, b: BoundExpr) -> BoundExpr {
        BoundExpr::new(
            ExprKind::Binary {
                left: Box::new(a),
                op: BinaryOp::Or,
                right: Box::new(b),
            },
            Type::Boolean,
        )
    }

    fn col() -> BoundExpr {
        BoundExpr::column(0, Type::Boolean)
    }

    #[test]
    fn boolean_simplification() {
        let t = || lit(Value::Boolean(true));
        let f = || lit(Value::Boolean(false));
        assert_eq!(fold(and(col(), t())), col());
        assert_eq!(fold(and(t(), col())), col());
        assert_eq!(fold(and(col(), f())), f());
        assert_eq!(fold(or(col(), t())), t());
        assert_eq!(fold(or(f(), col())), col());
    }

    #[test]
    fn errors_are_not_folded() {
        let div = BoundExpr::new(
            ExprKind::Binary {
                left: Box::new(lit(Value::Int64(1))),
                op: BinaryOp::Divide,
                right: Box::new(lit(Value::Int64(0))),
            },
            Type::Int64,
        );
        assert_eq!(fold(div.clone()), div);
    }

    #[test]
    fn folded_nulls_keep_their_type() {
        let e = BoundExpr::new(
            ExprKind::Binary {
                left: Box::new(BoundExpr::new(ExprKind::Literal(Value::Null), Type::Int64)),
                op: BinaryOp::Plus,
                right: Box::new(lit(Value::Int64(1))),
            },
            Type::Int64,
        );
        assert_eq!(
            fold(e),
            BoundExpr::new(ExprKind::Literal(Value::Null), Type::Int64)
        );
    }
}
