//! Cardinality estimation: how many rows each part of a plan will produce.
//!
//! The optimizer uses these estimates to compare alternative plans (see
//! `join_order.rs`). They come from simple, standard heuristics: table row
//! counts and per-column distinct-value counts from the catalog, fixed
//! selectivities for filters (`col = literal` keeps 1/distinct of the rows, a
//! range keeps 30%), and `|L| * |R| / max(distinct(L.key), distinct(R.key))`
//! for an equi-join. They only need to rank plans correctly, not predict exact
//! row counts.

use std::collections::{HashMap, HashSet};

use crate::ast::{BinaryOp, JoinKind};
use crate::bound::{BoundExpr, ColumnId, ExprKind};
use crate::catalog::Catalog;
use crate::plan::{columns_of, split_conjuncts, Plan};

/// Estimated number of rows `plan` produces.
pub fn estimate_rows(plan: &Plan, catalog: &Catalog) -> f64 {
    Estimator::new(plan, catalog).rows(plan)
}

pub(crate) struct Estimator<'a> {
    catalog: &'a Catalog,
    /// Which base-table column each scanned column id reads.
    origins: HashMap<ColumnId, (String, usize)>,
}

impl<'a> Estimator<'a> {
    /// An estimator for `plan` and any plan built from its parts.
    pub(crate) fn new(plan: &Plan, catalog: &'a Catalog) -> Self {
        let mut est = Estimator {
            catalog,
            origins: HashMap::new(),
        };
        est.collect_origins(plan);
        est
    }

    fn collect_origins(&mut self, plan: &Plan) {
        if let Plan::Scan {
            table,
            columns,
            indices,
            ..
        } = plan
        {
            for (&id, &i) in columns.iter().zip(indices) {
                self.origins.insert(id, (table.clone(), i));
            }
        }
        for child in plan.children() {
            self.collect_origins(child);
        }
    }

    /// Distinct values of a column, if it comes straight from a table.
    fn distinct(&self, e: &BoundExpr) -> Option<f64> {
        let ExprKind::Column(id) = e.kind else {
            return None;
        };
        let (table, i) = self.origins.get(&id)?;
        let stats = self.catalog.column_stats(table, *i)?;
        Some((stats.distinct as f64).max(1.0))
    }

    /// Fraction of rows a condition keeps.
    fn selectivity(&self, e: &BoundExpr) -> f64 {
        match &e.kind {
            ExprKind::Binary {
                left,
                op: BinaryOp::Eq,
                right,
            } => {
                let is_literal = |x: &BoundExpr| matches!(x.kind, ExprKind::Literal(_));
                match (is_literal(left), is_literal(right)) {
                    (false, true) => self.distinct(left).map_or(0.1, |d| 1.0 / d),
                    (true, false) => self.distinct(right).map_or(0.1, |d| 1.0 / d),
                    _ => 0.1,
                }
            }
            ExprKind::Binary {
                op: BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq,
                ..
            } => 0.3,
            ExprKind::Binary {
                op: BinaryOp::NotEq,
                ..
            } => 0.9,
            ExprKind::Binary {
                left,
                op: BinaryOp::And,
                right,
            } => self.selectivity(left) * self.selectivity(right),
            ExprKind::Binary {
                left,
                op: BinaryOp::Or,
                right,
            } => {
                let (a, b) = (self.selectivity(left), self.selectivity(right));
                a + b - a * b
            }
            ExprKind::InList {
                list,
                negated: false,
                ..
            } => (0.1 * list.len() as f64).min(0.9),
            ExprKind::IsNull { negated: false, .. } => 0.05,
            ExprKind::Like { negated: false, .. } => 0.1,
            _ => 0.5,
        }
    }

    pub(crate) fn rows(&self, plan: &Plan) -> f64 {
        let rows = match plan {
            Plan::SingleRow => 1.0,
            Plan::Scan { table, .. } => self
                .catalog
                .get(table)
                .map_or(1000.0, |t| t.row_count() as f64),
            Plan::Filter { input, predicate } => self.rows(input) * self.selectivity(predicate),
            Plan::Project { input, .. } | Plan::Sort { input, .. } => self.rows(input),
            Plan::Distinct { input } => self.rows(input) * 0.5,
            Plan::Limit {
                input,
                limit,
                offset,
            } => {
                let after_offset = (self.rows(input) - *offset as f64).max(0.0);
                limit.map_or(after_offset, |l| after_offset.min(l as f64))
            }
            Plan::Aggregate {
                input, group_by, ..
            } => {
                let input = self.rows(input);
                if group_by.is_empty() {
                    1.0
                } else {
                    let groups: Option<f64> =
                        group_by.iter().map(|k| self.distinct(&k.expr)).product();
                    groups.unwrap_or(input / 10.0).min(input)
                }
            }
            Plan::Join {
                left,
                right,
                kind,
                on,
            } => {
                let (l, r) = (self.rows(left), self.rows(right));
                let (lc, rc) = (column_set(left), column_set(right));
                let inner = self.join_rows(
                    l,
                    r,
                    &lc,
                    &rc,
                    on.iter().flat_map(|e| split_conjuncts(e.clone())),
                );
                match kind {
                    JoinKind::Inner | JoinKind::Cross => inner,
                    JoinKind::Left => inner.max(l),
                    JoinKind::Right => inner.max(r),
                    JoinKind::Full => inner.max(l + r),
                }
            }
        };
        rows.max(1.0)
    }

    /// Estimated size of an inner join between inputs of `l` and `r` rows
    /// (with columns `lc` and `rc`) on `conds`. Uses the most selective
    /// equality key; each other condition halves the result.
    pub(crate) fn join_rows(
        &self,
        l: f64,
        r: f64,
        lc: &HashSet<ColumnId>,
        rc: &HashSet<ColumnId>,
        conds: impl Iterator<Item = BoundExpr>,
    ) -> f64 {
        let mut key_selectivity: Option<f64> = None;
        let mut other = 1.0;
        for c in conds {
            if let ExprKind::Binary {
                left: a,
                op: BinaryOp::Eq,
                right: b,
            } = &c.kind
            {
                let (a_cols, b_cols) = (columns_of(a), columns_of(b));
                let crosses = (is_within(&a_cols, lc) && is_within(&b_cols, rc))
                    || (is_within(&a_cols, rc) && is_within(&b_cols, lc));
                if crosses && !a_cols.is_empty() && !b_cols.is_empty() {
                    // Without statistics, assume the key is unique on its side.
                    let side_rows =
                        |e: &BoundExpr| if is_within(&columns_of(e), lc) { l } else { r };
                    let da = self.distinct(a).unwrap_or(side_rows(a)).min(side_rows(a));
                    let db = self.distinct(b).unwrap_or(side_rows(b)).min(side_rows(b));
                    let sel = 1.0 / da.max(db).max(1.0);
                    key_selectivity = Some(key_selectivity.map_or(sel, |k: f64| k.min(sel)));
                    continue;
                }
            }
            other *= 0.5;
        }
        (l * r * key_selectivity.unwrap_or(1.0) * other).max(1.0)
    }
}

pub(crate) fn column_set(plan: &Plan) -> HashSet<ColumnId> {
    plan.output().into_iter().collect()
}

fn is_within(cols: &HashSet<ColumnId>, set: &HashSet<ColumnId>) -> bool {
    cols.iter().all(|c| set.contains(c))
}
