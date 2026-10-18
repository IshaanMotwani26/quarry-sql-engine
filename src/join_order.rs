//! Join ordering: picks the order in which a chain of inner joins runs.
//!
//! `FROM part, supplier, lineitem, ...` says nothing about which tables to
//! join first, but the choice can change the work by orders of magnitude:
//! joining two unrelated tables first produces a cross product. Using the
//! row estimates from `cardinality.rs`, this module chooses an order that
//! keeps intermediate results small, and builds each hash table on the
//! smaller input.

use std::collections::HashSet;

use crate::ast::JoinKind;
use crate::bound::{BoundExpr, ColumnId};
use crate::cardinality::{column_set, Estimator};
use crate::catalog::Catalog;
use crate::plan::{columns_of, conjoin, contains_subquery, split_conjuncts, Plan};

/// Reorders every chain of inner/cross joins in `plan`, and puts the smaller
/// input on the build (right) side of every join, outer joins included.
pub fn reorder_joins(plan: Plan, catalog: &Catalog) -> Plan {
    let est = Estimator::new(&plan, catalog);
    reorder(&est, plan)
}

/// One input to a chain of inner joins.
struct Leaf {
    plan: Plan,
    columns: HashSet<ColumnId>,
    rows: f64,
}

fn reorder(est: &Estimator, plan: Plan) -> Plan {
    let rec = |p: Box<Plan>| Box::new(reorder(est, *p));
    match plan {
        Plan::Join {
            kind: JoinKind::Inner | JoinKind::Cross,
            ..
        } => {
            let (mut leaves, mut conds) = (Vec::new(), Vec::new());
            flatten(plan, &mut leaves, &mut conds);
            let leaves: Vec<Leaf> = leaves
                .into_iter()
                .map(|p| {
                    let plan = reorder(est, p);
                    Leaf {
                        columns: column_set(&plan),
                        rows: est.rows(&plan),
                        plan,
                    }
                })
                .collect();
            build_order(est, leaves, conds)
        }
        Plan::Join {
            left,
            right,
            kind,
            on,
        } => {
            // Outer joins keep their inputs, but can still swap sides so the
            // hash table is built on the smaller one: `a LEFT JOIN b` is
            // `b RIGHT JOIN a`. Output columns are identified by id, so the
            // swapped column order doesn't matter to anything above.
            let (left, right) = (reorder(est, *left), reorder(est, *right));
            if est.rows(&right) > est.rows(&left) {
                let kind = match kind {
                    JoinKind::Left => JoinKind::Right,
                    JoinKind::Right => JoinKind::Left,
                    other => other,
                };
                Plan::Join {
                    left: Box::new(right),
                    right: Box::new(left),
                    kind,
                    on,
                }
            } else {
                Plan::Join {
                    left: Box::new(left),
                    right: Box::new(right),
                    kind,
                    on,
                }
            }
        }
        Plan::Filter { input, predicate } => Plan::Filter {
            input: rec(input),
            predicate,
        },
        Plan::Project { input, exprs } => Plan::Project {
            input: rec(input),
            exprs,
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

/// Simulates joining `leaves` in `order`, returning the order's cost (the
/// total estimated size of the intermediate results) and the conditions
/// applied at each step.
fn simulate(
    est: &Estimator,
    leaves: &[Leaf],
    conds: &[BoundExpr],
    order: &[usize],
) -> (f64, Vec<Vec<usize>>) {
    let chain = chain_columns(leaves);
    let mut joined: HashSet<ColumnId> = leaves[order[0]].columns.clone();
    let mut rows = leaves[order[0]].rows;
    let mut used = vec![false; conds.len()];
    let (mut cost, mut steps) = (0.0, Vec::new());
    for &i in &order[1..] {
        let leaf = &leaves[i];
        let applicable: Vec<usize> = (0..conds.len())
            .filter(|&c| !used[c] && applies(&conds[c], &chain, &joined, &leaf.columns))
            .collect();
        rows = est.join_rows(
            rows,
            leaf.rows,
            &joined,
            &leaf.columns,
            applicable.iter().map(|&c| conds[c].clone()),
        );
        for &c in &applicable {
            used[c] = true;
        }
        joined.extend(leaf.columns.iter().copied());
        cost += rows;
        steps.push(applicable);
    }
    (cost, steps)
}

/// Greedy order: start from the smallest input, then repeatedly add the
/// input connected to what's joined so far that gives the smallest result
/// (or the smallest input, if none is connected).
fn greedy_order(est: &Estimator, leaves: &[Leaf], conds: &[BoundExpr]) -> Vec<usize> {
    let smallest = |candidates: &mut dyn Iterator<Item = (usize, f64)>| {
        candidates.fold(None, |best: Option<(usize, f64)>, (i, r)| match best {
            Some((_, b)) if b <= r => best,
            _ => Some((i, r)),
        })
    };
    let chain = chain_columns(leaves);
    let first =
        smallest(&mut leaves.iter().enumerate().map(|(i, l)| (i, l.rows))).map_or(0, |(i, _)| i);
    let mut order = vec![first];
    let mut joined = leaves[first].columns.clone();
    let mut rows = leaves[first].rows;
    while order.len() < leaves.len() {
        let remaining = (0..leaves.len()).filter(|i| !order.contains(i));
        let connected: Vec<(usize, f64)> = remaining
            .clone()
            .filter_map(|i| {
                let leaf = &leaves[i];
                let applicable: Vec<BoundExpr> = conds
                    .iter()
                    .filter(|c| connects(c, &chain, &joined, &leaf.columns))
                    .cloned()
                    .collect();
                (!applicable.is_empty()).then(|| {
                    (
                        i,
                        est.join_rows(
                            rows,
                            leaf.rows,
                            &joined,
                            &leaf.columns,
                            applicable.into_iter(),
                        ),
                    )
                })
            })
            .collect();
        let (next, next_rows) = match smallest(&mut connected.into_iter()) {
            Some(pick) => pick,
            None => {
                let (i, r) =
                    smallest(&mut remaining.map(|i| (i, leaves[i].rows))).expect("leaves remain");
                (i, rows * r)
            }
        };
        order.push(next);
        joined.extend(leaves[next].columns.iter().copied());
        rows = next_rows;
    }
    order
}

/// Chooses between the written order and the greedy order by estimated
/// cost, then builds the left-deep join tree.
fn build_order(est: &Estimator, leaves: Vec<Leaf>, conds: Vec<BoundExpr>) -> Plan {
    let written: Vec<usize> = (0..leaves.len()).collect();
    let greedy = greedy_order(est, &leaves, &conds);
    let (written_cost, _) = simulate(est, &leaves, &conds, &written);
    let (greedy_cost, _) = simulate(est, &leaves, &conds, &greedy);
    let order = if greedy_cost < written_cost {
        greedy
    } else {
        written
    };
    let (_, steps) = simulate(est, &leaves, &conds, &order);

    let mut slots: Vec<Option<Leaf>> = leaves.into_iter().map(Some).collect();
    let mut conds: Vec<Option<BoundExpr>> = conds.into_iter().map(Some).collect();
    let first = slots[order[0]].take().expect("each leaf used once");
    let (mut plan, mut rows, mut columns) = (first.plan, first.rows, first.columns);
    let last = order.len() - 2;
    for (step, (&i, applicable)) in order[1..].iter().zip(steps).enumerate() {
        let leaf = slots[i].take().expect("each leaf used once");
        let mut on: Vec<BoundExpr> = applicable
            .into_iter()
            .filter_map(|c| conds[c].take())
            .collect();
        if step == last {
            // Anything left (conditions with subqueries) applies once every
            // input is joined.
            on.extend(conds.iter_mut().filter_map(Option::take));
        }
        let joined_rows =
            est.join_rows(rows, leaf.rows, &columns, &leaf.columns, on.iter().cloned());
        columns.extend(leaf.columns.iter().copied());
        // The hash join builds its table on the right input: make that the
        // smaller side.
        let (left, right) = if leaf.rows > rows {
            (leaf.plan, plan)
        } else {
            (plan, leaf.plan)
        };
        let on = conjoin(on);
        let kind = if on.is_some() {
            JoinKind::Inner
        } else {
            JoinKind::Cross
        };
        plan = Plan::Join {
            left: Box::new(left),
            right: Box::new(right),
            kind,
            on,
        };
        rows = joined_rows;
    }
    plan
}

fn chain_columns(leaves: &[Leaf]) -> HashSet<ColumnId> {
    leaves
        .iter()
        .flat_map(|l| l.columns.iter().copied())
        .collect()
}

/// Splits a tree of inner/cross joins into its inputs and conditions.
fn flatten(plan: Plan, leaves: &mut Vec<Plan>, conds: &mut Vec<BoundExpr>) {
    match plan {
        Plan::Join {
            left,
            right,
            kind: JoinKind::Inner | JoinKind::Cross,
            on,
        } => {
            flatten(*left, leaves, conds);
            flatten(*right, leaves, conds);
            conds.extend(on.map(split_conjuncts).unwrap_or_default());
        }
        other => leaves.push(other),
    }
}

/// True when `c` can be checked once `leaf` joins `joined`: every column it
/// reads from this join chain is available. (Columns outside the chain are
/// outer references, constant here.) Conditions with subqueries wait for the
/// final join, since their correlated references aren't visible here.
fn applies(
    c: &BoundExpr,
    chain: &HashSet<ColumnId>,
    joined: &HashSet<ColumnId>,
    leaf: &HashSet<ColumnId>,
) -> bool {
    !contains_subquery(c)
        && columns_of(c)
            .iter()
            .filter(|id| chain.contains(id))
            .all(|id| joined.contains(id) || leaf.contains(id))
}

/// True when `c` applies at this step and links `leaf` to what's joined.
fn connects(
    c: &BoundExpr,
    chain: &HashSet<ColumnId>,
    joined: &HashSet<ColumnId>,
    leaf: &HashSet<ColumnId>,
) -> bool {
    applies(c, chain, joined, leaf) && {
        let cols = columns_of(c);
        cols.iter().any(|id| joined.contains(id)) && cols.iter().any(|id| leaf.contains(id))
    }
}
