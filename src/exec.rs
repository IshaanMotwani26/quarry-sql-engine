//! Volcano-style executor: every operator exposes `next()`, which pulls rows
//! from its children one at a time and returns its next output row.
//!
//! ```text
//!   Limit.next() -> Sort.next() -> Project.next() -> Filter.next() -> Scan.next()
//! ```
//!
//! Each operator maps column ids to row positions once, when it is built
//! (`Layout`), so evaluating `orders.o_custkey` at runtime is a hash lookup
//! instead of a name search. Sort and HashAggregate are blocking (they must
//! see every input row before emitting one); a join materializes its right
//! input.
//!
//! Subqueries run through the same machinery. An uncorrelated subquery runs
//! once and its rows are cached. A correlated one re-runs for each outer row
//! with that row in scope. Decorrelating them into joins is the optimizer's
//! job (Phase 5).

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crate::agg::{AggState, KeyValue};
use crate::ast::JoinKind;
use crate::bound::{AggregateCall, Bound, BoundExpr, BoundQuery, ColumnId, GroupKey};
use crate::catalog::{Catalog, Field, Table};
use crate::eval::{
    compare, eval, eval_predicate, ExecError, Layout, Result, Row, Scope, SubqueryRunner,
};
use crate::plan::{plan_query, Plan};
use crate::types::Value;

/// A finished query's output.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    pub columns: Vec<Field>,
    pub rows: Vec<Row>,
}

/// Plans and runs a bound query to completion.
pub fn execute(catalog: &Catalog, bound: &Bound) -> Result<QueryResult> {
    let exec = Executor::new(catalog);
    let rows = exec.run_query(&bound.query, None)?;
    let columns = bound
        .query
        .outputs()
        .iter()
        .map(|o| Field {
            name: o.name.clone(),
            ty: o.ty(),
        })
        .collect();
    Ok(QueryResult {
        columns,
        rows: Rc::try_unwrap(rows).unwrap_or_else(|rc| (*rc).clone()),
    })
}

/// Holds per-execution state: the catalog and caches for subquery plans and
/// uncorrelated subquery results.
///
/// Both caches are keyed by the address of a `BoundQuery`. Those addresses
/// are stable because every subquery lives either inside the top-level bound
/// query (borrowed for the whole execution) or inside a cached plan (owned by
/// `plans`, which is never cleared while the executor exists).
struct Executor<'c> {
    catalog: &'c Catalog,
    plans: RefCell<HashMap<*const BoundQuery, Rc<Plan>>>,
    results: RefCell<HashMap<*const BoundQuery, Rc<Vec<Row>>>>,
}

impl<'c> Executor<'c> {
    fn new(catalog: &'c Catalog) -> Self {
        Executor {
            catalog,
            plans: RefCell::default(),
            results: RefCell::default(),
        }
    }

    fn plan_for(&self, q: &BoundQuery) -> Rc<Plan> {
        let key = q as *const BoundQuery;
        if let Some(plan) = self.plans.borrow().get(&key) {
            return Rc::clone(plan);
        }
        let plan = Rc::new(plan_query(q));
        self.plans.borrow_mut().insert(key, Rc::clone(&plan));
        plan
    }

    fn run_query(&self, q: &BoundQuery, outer: Option<&Scope<'_>>) -> Result<Rc<Vec<Row>>> {
        let plan = self.plan_for(q);
        let mut op = self.build(&plan)?;
        let mut rows = Vec::new();
        while let Some(row) = op.next(self, outer)? {
            rows.push(row);
        }
        Ok(Rc::new(rows))
    }

    fn build<'p>(&self, plan: &'p Plan) -> Result<Box<dyn Operator + 'p>>
    where
        'c: 'p,
    {
        Ok(match plan {
            Plan::SingleRow => Box::new(SingleRow { done: false }),
            Plan::Scan { table, .. } => {
                let catalog: &'c Catalog = self.catalog;
                let table = catalog
                    .get(table)
                    .ok_or_else(|| ExecError(format!("table \"{table}\" does not exist")))?;
                Box::new(Scan { table, pos: 0 })
            }
            Plan::Filter { input, predicate } => Box::new(Filter {
                layout: Layout::new(&input.output()),
                input: self.build(input)?,
                predicate,
            }),
            Plan::Project { input, exprs } => Box::new(Project {
                layout: Layout::new(&input.output()),
                input: self.build(input)?,
                exprs,
            }),
            Plan::Join {
                left,
                right,
                kind,
                on,
            } => {
                let (lw, rw) = (left.output().len(), right.output().len());
                Box::new(NestedLoopJoin {
                    layout: Layout::new(&plan.output()),
                    left: self.build(left)?,
                    right: Some(self.build(right)?),
                    right_rows: Vec::new(),
                    right_matched: Vec::new(),
                    kind: *kind,
                    on: on.as_ref(),
                    left_width: lw,
                    right_width: rw,
                    current: None,
                    idx: 0,
                    matched: false,
                    left_done: false,
                    tail: 0,
                })
            }
            Plan::Aggregate {
                input,
                group_by,
                aggregates,
            } => Box::new(HashAggregate {
                layout: Layout::new(&input.output()),
                input: Some(self.build(input)?),
                group_by,
                aggregates,
                output: Vec::new().into_iter(),
            }),
            Plan::Distinct { input } => Box::new(Distinct {
                input: self.build(input)?,
                seen: HashSet::new(),
            }),
            Plan::Sort { input, keys } => {
                let layout = Layout::new(&input.output());
                let keys = keys
                    .iter()
                    .map(|k| Ok((position(&layout, k.column)?, k.asc, k.nulls_first)))
                    .collect::<Result<Vec<_>>>()?;
                Box::new(Sort {
                    input: Some(self.build(input)?),
                    keys,
                    sorted: Vec::new().into_iter(),
                })
            }
            Plan::Limit {
                input,
                limit,
                offset,
            } => Box::new(Limit {
                input: self.build(input)?,
                skip: *offset,
                remaining: *limit,
            }),
        })
    }
}

impl SubqueryRunner for Executor<'_> {
    fn run(&self, query: &BoundQuery, outer: &Scope<'_>) -> Result<Rc<Vec<Row>>> {
        if !query.correlated.is_empty() {
            return self.run_query(query, Some(outer));
        }
        let key = query as *const BoundQuery;
        if let Some(rows) = self.results.borrow().get(&key) {
            return Ok(Rc::clone(rows));
        }
        let rows = self.run_query(query, None)?;
        self.results.borrow_mut().insert(key, Rc::clone(&rows));
        Ok(rows)
    }
}

fn position(layout: &Layout, id: ColumnId) -> Result<usize> {
    layout.get(id).ok_or_else(|| {
        ExecError(format!(
            "internal: column #{id} missing from operator input"
        ))
    })
}

// ---------------------------------------------------------------------------
// Operators
// ---------------------------------------------------------------------------

trait Operator {
    /// Returns the next row, or `None` when the operator is exhausted.
    /// `outer` is the enclosing query's row when running a correlated subquery.
    fn next(&mut self, exec: &Executor<'_>, outer: Option<&Scope<'_>>) -> Result<Option<Row>>;
}

struct SingleRow {
    done: bool,
}

impl Operator for SingleRow {
    fn next(&mut self, _: &Executor<'_>, _: Option<&Scope<'_>>) -> Result<Option<Row>> {
        if self.done {
            return Ok(None);
        }
        self.done = true;
        Ok(Some(Vec::new()))
    }
}

struct Scan<'p> {
    table: &'p Table,
    pos: usize,
}

impl Operator for Scan<'_> {
    fn next(&mut self, _: &Executor<'_>, _: Option<&Scope<'_>>) -> Result<Option<Row>> {
        if self.pos >= self.table.row_count() {
            return Ok(None);
        }
        let row = self.table.row(self.pos);
        self.pos += 1;
        Ok(Some(row))
    }
}

struct Filter<'p> {
    input: Box<dyn Operator + 'p>,
    layout: Layout,
    predicate: &'p BoundExpr,
}

impl Operator for Filter<'_> {
    fn next(&mut self, exec: &Executor<'_>, outer: Option<&Scope<'_>>) -> Result<Option<Row>> {
        while let Some(row) = self.input.next(exec, outer)? {
            let scope = Scope::new(&self.layout, &row, outer);
            if eval_predicate(self.predicate, &scope, exec)? {
                return Ok(Some(row));
            }
        }
        Ok(None)
    }
}

struct Project<'p> {
    input: Box<dyn Operator + 'p>,
    layout: Layout,
    exprs: &'p [(ColumnId, BoundExpr)],
}

impl Operator for Project<'_> {
    fn next(&mut self, exec: &Executor<'_>, outer: Option<&Scope<'_>>) -> Result<Option<Row>> {
        let Some(row) = self.input.next(exec, outer)? else {
            return Ok(None);
        };
        let scope = Scope::new(&self.layout, &row, outer);
        let mut out = Vec::with_capacity(self.exprs.len());
        for (_, e) in self.exprs {
            out.push(eval(e, &scope, exec)?);
        }
        Ok(Some(out))
    }
}

/// Nested-loop join supporting every join kind. The right input is read into
/// memory on the first call; each left row is then compared against every
/// right row. O(left x right): Phase 4 adds a hash join for equality joins.
///
/// - LEFT/FULL: a left row with no match is emitted once, padded with NULLs.
/// - RIGHT/FULL: right rows that never matched are emitted at the end.
struct NestedLoopJoin<'p> {
    left: Box<dyn Operator + 'p>,
    right: Option<Box<dyn Operator + 'p>>,
    right_rows: Vec<Row>,
    right_matched: Vec<bool>,
    kind: JoinKind,
    on: Option<&'p BoundExpr>,
    layout: Layout,
    left_width: usize,
    right_width: usize,
    /// The current left row followed by space for a right row.
    current: Option<Row>,
    idx: usize,
    matched: bool,
    left_done: bool,
    tail: usize,
}

impl Operator for NestedLoopJoin<'_> {
    fn next(&mut self, exec: &Executor<'_>, outer: Option<&Scope<'_>>) -> Result<Option<Row>> {
        if let Some(mut right) = self.right.take() {
            while let Some(row) = right.next(exec, outer)? {
                self.right_rows.push(row);
            }
            self.right_matched = vec![false; self.right_rows.len()];
        }
        let lw = self.left_width;
        loop {
            if self.left_done {
                if matches!(self.kind, JoinKind::Right | JoinKind::Full) {
                    while self.tail < self.right_rows.len() {
                        let i = self.tail;
                        self.tail += 1;
                        if !self.right_matched[i] {
                            let mut row = vec![Value::Null; lw];
                            row.extend(self.right_rows[i].iter().cloned());
                            return Ok(Some(row));
                        }
                    }
                }
                return Ok(None);
            }

            if self.current.is_none() {
                match self.left.next(exec, outer)? {
                    Some(mut row) => {
                        row.resize(lw + self.right_width, Value::Null);
                        self.current = Some(row);
                        self.idx = 0;
                        self.matched = false;
                    }
                    None => {
                        self.left_done = true;
                        continue;
                    }
                }
            }

            let buf = self.current.as_mut().expect("set above");
            while self.idx < self.right_rows.len() {
                let i = self.idx;
                self.idx += 1;
                buf[lw..].clone_from_slice(&self.right_rows[i]);
                let hit = match self.on {
                    None => true,
                    Some(p) => eval_predicate(p, &Scope::new(&self.layout, buf, outer), exec)?,
                };
                if hit {
                    self.matched = true;
                    self.right_matched[i] = true;
                    return Ok(Some(buf.clone()));
                }
            }

            // Every right row has been tried against this left row.
            let mut row = self.current.take().expect("set above");
            if !self.matched && matches!(self.kind, JoinKind::Left | JoinKind::Full) {
                row[lw..].fill(Value::Null);
                return Ok(Some(row));
            }
        }
    }
}

/// Hash aggregation. Reads its whole input, assigning each row to a group
/// by its GROUP BY key values, then emits one row per group: the key values
/// followed by each aggregate's result.
///
/// Groups are emitted in the order their first row arrived, so output is
/// deterministic without an ORDER BY. With no GROUP BY there is exactly one
/// group, even over zero input rows (`SELECT count(*) FROM empty` is 0, not
/// no rows); with GROUP BY, empty input produces no groups.
struct HashAggregate<'p> {
    input: Option<Box<dyn Operator + 'p>>,
    layout: Layout,
    group_by: &'p [GroupKey],
    aggregates: &'p [AggregateCall],
    output: std::vec::IntoIter<Row>,
}

impl HashAggregate<'_> {
    fn new_states(&self) -> Vec<AggState> {
        self.aggregates.iter().map(AggState::new).collect()
    }

    fn consume(
        &self,
        mut input: Box<dyn Operator + '_>,
        exec: &Executor<'_>,
        outer: Option<&Scope<'_>>,
    ) -> Result<Vec<Row>> {
        let mut index: HashMap<Vec<KeyValue>, usize> = HashMap::new();
        let mut groups: Vec<(Row, Vec<AggState>)> = Vec::new();
        while let Some(row) = input.next(exec, outer)? {
            let scope = Scope::new(&self.layout, &row, outer);
            let mut key = Vec::with_capacity(self.group_by.len());
            for k in self.group_by {
                key.push(eval(&k.expr, &scope, exec)?);
            }
            let hashed: Vec<KeyValue> = key.iter().map(KeyValue::of).collect();
            let g = match index.get(&hashed) {
                Some(&g) => g,
                None => {
                    groups.push((key, self.new_states()));
                    index.insert(hashed, groups.len() - 1);
                    groups.len() - 1
                }
            };
            for (state, call) in groups[g].1.iter_mut().zip(self.aggregates) {
                let value = match &call.arg {
                    Some(arg) => Some(eval(arg, &scope, exec)?),
                    None => None,
                };
                state.update(value)?;
            }
        }
        if self.group_by.is_empty() && groups.is_empty() {
            groups.push((Vec::new(), self.new_states()));
        }
        Ok(groups
            .into_iter()
            .map(|(mut row, states)| {
                row.extend(states.into_iter().map(AggState::finish));
                row
            })
            .collect())
    }
}

impl Operator for HashAggregate<'_> {
    fn next(&mut self, exec: &Executor<'_>, outer: Option<&Scope<'_>>) -> Result<Option<Row>> {
        if let Some(input) = self.input.take() {
            self.output = self.consume(input, exec, outer)?.into_iter();
        }
        Ok(self.output.next())
    }
}

struct Distinct<'p> {
    input: Box<dyn Operator + 'p>,
    seen: HashSet<Vec<KeyValue>>,
}

impl Operator for Distinct<'_> {
    fn next(&mut self, exec: &Executor<'_>, outer: Option<&Scope<'_>>) -> Result<Option<Row>> {
        while let Some(row) = self.input.next(exec, outer)? {
            if self.seen.insert(row.iter().map(KeyValue::of).collect()) {
                return Ok(Some(row));
            }
        }
        Ok(None)
    }
}

/// Orders two rows by `(position, asc, nulls_first)` keys.
fn compare_rows(a: &Row, b: &Row, keys: &[(usize, bool, bool)]) -> Ordering {
    for &(i, asc, nulls_first) in keys {
        let ord = match (a[i].is_null(), b[i].is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) => {
                if nulls_first {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (false, true) => {
                if nulls_first {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            (false, false) => {
                let o = compare(&a[i], &b[i]);
                if asc {
                    o
                } else {
                    o.reverse()
                }
            }
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

struct Sort<'p> {
    input: Option<Box<dyn Operator + 'p>>,
    keys: Vec<(usize, bool, bool)>,
    sorted: std::vec::IntoIter<Row>,
}

impl Operator for Sort<'_> {
    fn next(&mut self, exec: &Executor<'_>, outer: Option<&Scope<'_>>) -> Result<Option<Row>> {
        if let Some(mut input) = self.input.take() {
            let mut rows = Vec::new();
            while let Some(row) = input.next(exec, outer)? {
                rows.push(row);
            }
            // Stable, so ties keep their input order.
            rows.sort_by(|a, b| compare_rows(a, b, &self.keys));
            self.sorted = rows.into_iter();
        }
        Ok(self.sorted.next())
    }
}

struct Limit<'p> {
    input: Box<dyn Operator + 'p>,
    skip: u64,
    remaining: Option<u64>,
}

impl Operator for Limit<'_> {
    fn next(&mut self, exec: &Executor<'_>, outer: Option<&Scope<'_>>) -> Result<Option<Row>> {
        if self.remaining == Some(0) {
            return Ok(None); // stop pulling from the input entirely
        }
        while self.skip > 0 {
            if self.input.next(exec, outer)?.is_none() {
                return Ok(None);
            }
            self.skip -= 1;
        }
        let row = self.input.next(exec, outer)?;
        if row.is_some() {
            if let Some(r) = &mut self.remaining {
                *r -= 1;
            }
        }
        Ok(row)
    }
}
