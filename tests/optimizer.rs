//! Optimizer tests: plan shapes for each rule, and result equivalence. Every
//! query that runs here runs twice, with and without the optimizer, and the
//! results must match: the same rows (in the same order, if the query has an
//! ORDER BY), with floats equal to within rounding.

use quarry::bound::Bound;
use quarry::cardinality::estimate_rows;
use quarry::catalog::{Catalog, Schema, Table};
use quarry::csv::{parse_csv, CsvOptions};
use quarry::optimizer::optimize;
use quarry::types::{Type, Value};
use quarry::Plan;
use quarry::{bind, execute_with, parse_query, plan_query, tpch, ExecOptions};

const A: &str = "id,k,v,s\n1,10,5,x\n2,20,,y\n3,,7,z\n4,10,1,w\n5,30,9,x\n";
const B: &str = "id,k,v,s\n7,10,2,x\n8,30,8,y\n9,,4,z\n10,10,6,q\n";

fn demo() -> Catalog {
    let mut c = Catalog::new();
    for (name, csv) in [("a", A), ("b", B)] {
        c.register(parse_csv(name, csv, &CsvOptions::default(), None).unwrap())
            .unwrap();
    }
    c
}

fn generated(scale: f64) -> Catalog {
    let mut c = Catalog::new();
    tpch::generate(&mut c, scale);
    c
}

fn bound(c: &Catalog, sql: &str) -> Bound {
    let q = parse_query(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    bind(c, &q).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// The optimized plan, rendered.
fn explain(c: &Catalog, sql: &str) -> String {
    let b = bound(c, sql);
    optimize(plan_query(&b.query), c).explain_with(&b.columns, Some(c))
}

/// Two values are the same result if equal, or floats within 1e-9 relative
/// error: summing in a different order can change the last bits.
fn same_value(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Float64(x), Value::Float64(y)) => {
            x == y || (x - y).abs() <= 1e-9 * x.abs().max(y.abs())
        }
        _ => a == b,
    }
}

/// Sort key for comparing unordered results; floats are rounded so values
/// that differ only in the last bits sort together.
fn sort_key(row: &[Value]) -> Vec<String> {
    row.iter()
        .map(|v| match v {
            Value::Float64(x) => format!("{x:.6e}"),
            other => format!("{other:?}"),
        })
        .collect()
}

/// Compares two results. SQL defines row order only under ORDER BY, and
/// the optimizer may change it otherwise (for example by reordering joins),
/// so unordered results are compared as multisets.
fn same_rows(a: &[Vec<Value>], b: &[Vec<Value>], ordered: bool) -> bool {
    let (mut a, mut b) = (a.to_vec(), b.to_vec());
    if !ordered {
        a.sort_by_cached_key(|r| sort_key(r));
        b.sort_by_cached_key(|r| sort_key(r));
    }
    a.len() == b.len()
        && a.iter()
            .zip(&b)
            .all(|(x, y)| x.len() == y.len() && x.iter().zip(y).all(|(u, v)| same_value(u, v)))
}

/// Runs with and without the optimizer, asserts the results match, and
/// returns the optimized result as strings.
fn rows(c: &Catalog, sql: &str) -> Vec<Vec<String>> {
    let b = bound(c, sql);
    let run = |optimize| {
        execute_with(
            c,
            &b,
            ExecOptions {
                optimize,
                ..Default::default()
            },
        )
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .rows
    };
    let (optimized, plain) = (run(true), run(false));
    let ordered = !b.query.order_by.is_empty();
    assert!(
        same_rows(&optimized, &plain, ordered),
        "optimizer changed the result of {sql}\noptimized: {optimized:?}\nplain:     {plain:?}"
    );
    optimized
        .iter()
        .map(|r| {
            r.iter()
                .map(|v| {
                    if v.is_null() {
                        "NULL".into()
                    } else {
                        v.to_string()
                    }
                })
                .collect()
        })
        .collect()
}

/// Asserts `needle` appears in the plan, ignoring indentation, so tests check
/// which node sits directly above which without counting spaces.
fn assert_contains(text: &str, needle: &str) {
    let flat = |s: &str| {
        s.lines()
            .map(str::trim_start)
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(
        flat(text).contains(&flat(needle)),
        "expected {needle:?} in plan:\n{text}"
    );
}

// ---------------------------------------------------------------------------
// Constant folding
// ---------------------------------------------------------------------------

#[test]
fn constants_are_folded() {
    let c = demo();
    let plan = explain(
        &c,
        "SELECT id FROM a WHERE v > 2 + 3 AND s = upper('x') || ''",
    );
    assert_contains(&plan, "Filter ((a.v > 5) AND (a.s = 'X'))");
    let mut c = Catalog::new();
    tpch::register_schemas(&mut c);
    assert_contains(
        &explain(&c, tpch::Q1),
        "Filter (lineitem.l_shipdate <= DATE '1998-09-02')",
    );
}

#[test]
fn always_true_conditions_disappear() {
    let c = demo();
    let plan = explain(&c, "SELECT id FROM a WHERE 1 = 1 AND (v > 1 OR TRUE)");
    assert!(!plan.contains("Filter"), "{plan}");
    assert_eq!(
        rows(&c, "SELECT id FROM a WHERE 1 = 1 AND (v > 1 OR TRUE)").len(),
        5
    );
}

#[test]
fn always_false_conditions_still_return_correct_results() {
    let c = demo();
    assert!(rows(&c, "SELECT id FROM a WHERE v > 1 AND 1 = 0").is_empty());
    assert_eq!(rows(&c, "SELECT count(*) FROM a WHERE 1 = 0"), [["0"]]);
}

#[test]
fn failing_constants_are_not_folded() {
    // 1 / 0 must only fail if it's actually evaluated.
    let c = demo();
    assert_eq!(
        rows(
            &c,
            "SELECT CASE WHEN v > 100 THEN 1 / 0 ELSE 1 END FROM a WHERE id = 1"
        ),
        [["1"]]
    );
    assert_contains(&explain(&c, "SELECT 1 / 0 FROM a"), "(1 / 0)");
}

// ---------------------------------------------------------------------------
// Filter pushdown
// ---------------------------------------------------------------------------

#[test]
fn filters_move_to_their_scans() {
    let c = demo();
    let plan = explain(
        &c,
        "SELECT a.id FROM a, b WHERE a.k = b.k AND a.v > 2 AND b.s <> 'q'",
    );
    // Either input may end up on the build side; only placement matters here.
    assert!(
        plan.contains("HashJoin Inner keys=[a.k = b.k]")
            || plan.contains("HashJoin Inner keys=[b.k = a.k]"),
        "{plan}"
    );
    assert_contains(&plan, "Filter (a.v > 2)\nScan a");
    assert_contains(&plan, "Filter (b.s <> 'q')\nScan b");
}

#[test]
fn inner_join_on_conditions_move_like_where_conditions() {
    let c = demo();
    let plan = explain(&c, "SELECT a.id FROM a JOIN b ON a.k = b.k AND a.v > 2");
    assert_contains(&plan, "Filter (a.v > 2)\nScan a");
    assert!(
        !plan.contains("residual"),
        "a.v > 2 should leave the join condition:\n{plan}"
    );
}

#[test]
fn where_on_the_null_padded_side_of_an_outer_join_stays_put() {
    let c = demo();
    // The anti-join pattern: rows of a with no match in b.
    let sql = "SELECT a.id FROM a LEFT JOIN b ON a.k = b.k WHERE b.id IS NULL ORDER BY 1";
    assert_contains(
        &explain(&c, sql),
        "Filter b.id IS NULL\n      HashJoin Left",
    );
    assert_eq!(rows(&c, sql), [["2"], ["3"]]);
    // A WHERE condition on the preserved side can move down.
    assert_contains(
        &explain(
            &c,
            "SELECT a.id FROM a LEFT JOIN b ON a.k = b.k WHERE a.v > 2",
        ),
        "Filter (a.v > 2)\n      Scan a",
    );
}

#[test]
fn outer_join_on_conditions_move_only_into_the_optional_side() {
    let c = demo();
    // ON b.v > 3 filters b's rows before the join...
    let sql = "SELECT a.id, b.id FROM a LEFT JOIN b ON a.k = b.k AND b.v > 3 ORDER BY 1, 2";
    assert_contains(&explain(&c, sql), "Filter (b.v > 3)\n      Scan b");
    rows(&c, sql);
    // ...but ON a.v > 3 must stay: rows of a failing it still appear, padded with NULLs.
    let sql = "SELECT a.id, b.id FROM a LEFT JOIN b ON a.k = b.k AND a.v > 3 ORDER BY 1, 2";
    assert_contains(&explain(&c, sql), "residual=[(a.v > 3)]");
    assert_eq!(rows(&c, sql).len(), 6);
    // Mirror image for RIGHT JOIN, and nothing moves for FULL JOIN.
    assert_contains(
        &explain(&c, "SELECT 1 FROM a RIGHT JOIN b ON a.k = b.k AND a.v > 3"),
        "Filter (a.v > 3)\n      Scan a",
    );
    assert_contains(
        &explain(&c, "SELECT 1 FROM a FULL JOIN b ON a.k = b.k AND a.v > 3"),
        "residual=[(a.v > 3)]",
    );
}

#[test]
fn filters_on_group_keys_move_below_the_aggregate() {
    let c = demo();
    let sql =
        "SELECT k, n FROM (SELECT k, count(*) AS n FROM a GROUP BY k) AS g WHERE k > 10 AND n > 0";
    let plan = explain(&c, sql);
    // k > 10 filters rows before grouping; n > 0 reads an aggregate, so it stays above.
    assert_contains(&plan, "Filter (a.k > 10)\n            Scan a");
    assert_contains(&plan, "Filter (count(*) > 0)");
    assert_eq!(rows(&c, sql), [["20", "1"], ["30", "1"]]);
}

#[test]
fn filters_do_not_move_below_a_scalar_aggregate() {
    // count(*) over zero rows is still one row, so a condition that reads no
    // group key must be checked after aggregating, never pushed below it.
    let c = demo();
    // A constant FALSE condition: zero rows, not one row holding count 0.
    assert!(rows(
        &c,
        "SELECT n FROM (SELECT count(*) AS n FROM a) AS t WHERE 1 = 0"
    )
    .is_empty());
    // An outer reference behaves like a constant inside the subquery.
    assert_eq!(
        rows(&c, "SELECT a.id FROM a WHERE EXISTS (SELECT 1 FROM (SELECT count(*) AS n FROM b) AS t WHERE a.v > 6) ORDER BY 1"),
        [["3"], ["5"]]
    );
    // A condition on the aggregate itself.
    assert_eq!(
        rows(
            &c,
            "SELECT n FROM (SELECT count(*) AS n FROM a WHERE v > 100) AS t WHERE n = 0"
        ),
        [["0"]]
    );
}

#[test]
fn filters_do_not_move_below_limit() {
    let c = demo();
    let sql = "SELECT id FROM (SELECT id, v FROM a ORDER BY id LIMIT 3) AS t WHERE v > 4";
    assert_contains(&explain(&c, sql), "Limit 3");
    assert_eq!(rows(&c, sql), [["1"], ["3"]]);
}

#[test]
fn filters_move_through_derived_tables() {
    let c = demo();
    let sql = "SELECT x FROM (SELECT v * 2 AS x FROM a) AS t WHERE x > 10";
    assert_contains(&explain(&c, sql), "Filter ((a.v * 2) > 10)\n        Scan a");
    assert_eq!(rows(&c, sql), [["14"], ["18"]]);
}

#[test]
fn filters_inside_a_correlated_subquery_reach_its_scan() {
    // Not decorrelatable (the subquery aggregates), so it still runs per row,
    // but its own filters, including the outer reference, still move down.
    let c = demo();
    let sql = "SELECT id FROM a WHERE (SELECT count(*) FROM b WHERE b.k = a.k AND b.v > 3) > 0 ORDER BY 1";
    assert_contains(
        &explain(&c, sql),
        "Filter ((b.k = a.k) AND (b.v > 3))\nScan b",
    );
    assert_eq!(rows(&c, sql), [["1"], ["4"], ["5"]]);
}

// ---------------------------------------------------------------------------
// Subquery decorrelation
// ---------------------------------------------------------------------------

#[test]
fn exists_becomes_a_semi_join() {
    let c = demo();
    let sql =
        "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.k AND b.v > 3) ORDER BY 1";
    let plan = explain(&c, sql);
    assert_contains(&plan, "HashSemiJoin keys=[a.k = b.k]");
    // The subquery's own condition filters b before the join.
    assert_contains(&plan, "Filter (b.v > 3)\nScan b");
    assert!(!plan.contains("$sub"), "{plan}");
    assert_eq!(rows(&c, sql), [["1"], ["4"], ["5"]]);
}

#[test]
fn not_exists_becomes_an_anti_join() {
    let c = generated(0.002);
    let plan = explain(&c, tpch::Q22);
    assert_contains(
        &plan,
        "HashAntiJoin keys=[customer.c_custkey = orders.o_custkey]",
    );
    assert!(!plan.contains("(correlated)"), "{plan}");
    // The uncorrelated scalar subquery (the average balance) stays a subquery.
    assert!(plan.contains("$sub0:"), "{plan}");
}

#[test]
fn in_subqueries_become_semi_joins() {
    let c = demo();
    let sql = "SELECT id FROM a WHERE k IN (SELECT k FROM b WHERE v > 3) ORDER BY 1";
    assert_contains(&explain(&c, sql), "HashSemiJoin keys=[a.k = b.k]");
    assert_eq!(rows(&c, sql), [["1"], ["4"], ["5"]]);
    // The select expression may itself read the outer row.
    let sql = "SELECT id FROM a WHERE v IN (SELECT b.v + a.k - 10 FROM b) ORDER BY 1";
    assert!(explain(&c, sql).contains("SemiJoin"));
    rows(&c, sql);
}

#[test]
fn not_in_is_left_as_a_subquery() {
    // With a NULL in b.k, NOT IN is never TRUE; an anti join would return rows.
    let c = demo();
    let sql = "SELECT id FROM a WHERE k NOT IN (SELECT k FROM b)";
    assert!(explain(&c, sql).contains("NOT IN $sub0"));
    assert!(rows(&c, sql).is_empty());
}

#[test]
fn subqueries_that_aggregate_or_limit_stay_correlated() {
    let c = demo();
    for sql in [
        "SELECT id FROM a WHERE EXISTS (SELECT count(*) FROM b WHERE b.k = a.k)",
        "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.k LIMIT 1)",
    ] {
        assert!(explain(&c, sql).contains("(correlated)"), "{sql}");
        rows(&c, sql);
    }
}

#[test]
fn subqueries_whose_from_clause_reads_the_outer_row_stay_correlated() {
    // The derived table x needs a's row, but a join's right input runs once,
    // with no outer row, so this subquery must keep running per row of a.
    let c = demo();
    let sql = "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM (SELECT b.id FROM b WHERE b.k = a.k) AS x) ORDER BY 1";
    assert!(explain(&c, sql).contains("(correlated)"));
    assert_eq!(rows(&c, sql), [["1"], ["4"], ["5"]]);
}

#[test]
fn subqueries_whose_inner_subquery_reads_the_outer_row_are_still_joined() {
    let c = demo();
    // The innermost EXISTS reads a.v, two levels out. Its condition becomes
    // part of the semi join's condition, which sees both a's and b's rows, so
    // the middle subquery can still be decorrelated; the innermost one runs
    // per candidate pair instead of per (a row, b row).
    let sql = "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.k AND EXISTS \
               (SELECT 1 FROM b AS c WHERE c.v > a.v AND c.id = b.id)) ORDER BY 1";
    let plan = explain(&c, sql);
    assert_contains(
        &plan,
        "HashSemiJoin keys=[a.k = b.k] residual=[EXISTS $sub0]",
    );
    assert_eq!(rows(&c, sql), [["1"], ["4"]]);
}

// ---------------------------------------------------------------------------
// Projection pruning
// ---------------------------------------------------------------------------

#[test]
fn scans_read_only_needed_columns() {
    let mut c = Catalog::new();
    tpch::register_schemas(&mut c);
    assert_contains(
        &explain(&c, tpch::Q6),
        "Scan lineitem [4/16 columns: l_quantity, l_extendedprice, l_discount, l_shipdate]",
    );
}

#[test]
fn columns_used_only_by_correlated_subqueries_are_kept() {
    let mut c = Catalog::new();
    tpch::register_schemas(&mut c);
    // c_custkey appears only inside Q22's NOT EXISTS subquery.
    assert_contains(
        &explain(&c, tpch::Q22),
        "Scan customer [3/8 columns: c_custkey, c_phone, c_acctbal]",
    );
}

#[test]
fn count_star_reads_no_columns() {
    let c = demo();
    assert_contains(
        &explain(&c, "SELECT count(*) FROM a"),
        "Scan a [0/4 columns: ]",
    );
    assert_eq!(rows(&c, "SELECT count(*) FROM a"), [["5"]]);
    assert_eq!(
        rows(&c, "SELECT count(*) FROM (SELECT k FROM a) AS t"),
        [["5"]]
    );
}

#[test]
fn distinct_keeps_every_column_it_compares() {
    let c = demo();
    assert_eq!(
        rows(
            &c,
            "SELECT count(*) FROM (SELECT DISTINCT k, s FROM a) AS t"
        ),
        [["5"]]
    );
    assert_eq!(
        rows(&c, "SELECT count(*) FROM (SELECT DISTINCT k FROM a) AS t"),
        [["4"]]
    );
}

#[test]
fn unused_aggregates_are_dropped() {
    let c = demo();
    let plan = explain(
        &c,
        "SELECT k FROM (SELECT k, sum(v) AS s, max(v) AS m FROM a GROUP BY k) AS t WHERE m > 1",
    );
    assert_contains(&plan, "aggs=[max(a.v)]");
}

// ---------------------------------------------------------------------------
// Cardinality estimation
// ---------------------------------------------------------------------------

#[test]
fn row_estimates_use_table_statistics() {
    let c = generated(0.002);
    let sizes = tpch::Sizes::at(0.002);
    let est = |sql: &str| estimate_rows(&plan_query(&bound(&c, sql).query), &c);
    assert_eq!(est("SELECT * FROM customer"), sizes.customer as f64);
    // c_mktsegment has 5 distinct values, so equality keeps about a fifth.
    let segment = est("SELECT * FROM customer WHERE c_mktsegment = 'BUILDING'");
    assert!(
        (segment - sizes.customer as f64 / 5.0).abs() < 1.0,
        "{segment}"
    );
    // A foreign-key join produces about as many rows as the referencing side.
    let joined = est("SELECT * FROM orders, customer WHERE o_custkey = c_custkey");
    let ratio = joined / sizes.orders as f64;
    assert!(
        (0.9..=1.6).contains(&ratio),
        "estimated {joined} rows for {} orders",
        sizes.orders
    );
}

// ---------------------------------------------------------------------------
// Join ordering
// ---------------------------------------------------------------------------

/// Every join in the plan, as (left input, right input).
fn joins(plan: &Plan) -> Vec<(&Plan, &Plan)> {
    let mut out = Vec::new();
    if let Plan::Join { left, right, .. } = plan {
        out.push((&**left, &**right));
    }
    for child in plan.children() {
        out.extend(joins(child));
    }
    out
}

#[test]
fn q8_no_longer_builds_a_cross_product() {
    let c = generated(0.002);
    let plan = explain(&c, tpch::Q8);
    assert!(!plan.contains("Cross"), "{plan}");
    assert_eq!(plan.matches("HashJoin Inner").count(), 7, "{plan}");
}

#[test]
fn a_bad_written_order_is_fixed() {
    // Written order joins a and b first, with no condition between them.
    let c = demo();
    let sql = "SELECT a.id, b.id, x.id FROM a, b, a AS x WHERE a.k = x.k AND x.v = b.v";
    let plan = explain(&c, sql);
    assert!(!plan.contains("Cross"), "{plan}");
    rows(&c, sql);
}

#[test]
fn hash_tables_are_built_on_the_smaller_input() {
    let c = generated(0.002);
    for (n, sql) in tpch::QUERIES {
        let b = bound(&c, sql);
        let plan = optimize(plan_query(&b.query), &c);
        for (left, right) in joins(&plan) {
            let (l, r) = (estimate_rows(left, &c), estimate_rows(right, &c));
            assert!(
                r <= l,
                "TPC-H Q{n}: build side estimated at {r} rows, probe side at {l}"
            );
        }
    }
}

#[test]
fn reordering_stops_at_outer_joins() {
    let c = demo();
    let sql = "SELECT a.id, x.id FROM a LEFT JOIN b ON a.k = b.k, a AS x WHERE x.k = a.k";
    let plan = explain(&c, sql);
    // The LEFT JOIN stays one unit (a with b); only its position in the chain can change.
    assert!(plan.contains("HashJoin Left keys=[a.k = b.k]"), "{plan}");
    rows(&c, sql);
}

// ---------------------------------------------------------------------------
// End to end
// ---------------------------------------------------------------------------

#[test]
fn result_comparison_ignores_order_only_without_order_by() {
    let (one, two) = (vec![Value::Int64(1)], vec![Value::Int64(2)]);
    assert!(same_rows(
        &[one.clone(), two.clone()],
        &[two.clone(), one.clone()],
        false
    ));
    assert!(!same_rows(
        &[one.clone(), two.clone()],
        &[two.clone(), one.clone()],
        true
    ));
    assert!(
        !same_rows(
            &[one.clone(), one.clone()],
            &[one.clone(), two.clone()],
            false
        ),
        "multisets, not sets"
    );
    assert!(same_rows(
        &[vec![Value::Float64(0.1 + 0.2)]],
        &[vec![Value::Float64(0.3)]],
        true
    ));
    assert!(!same_rows(
        &[vec![Value::Float64(0.3)]],
        &[vec![Value::Float64(0.31)]],
        true
    ));
}

#[test]
fn every_tpch_query_gives_identical_results_with_and_without_the_optimizer() {
    let mut c = Catalog::new();
    tpch::generate(&mut c, 0.002);
    for (n, sql) in tpch::QUERIES {
        let result = rows(&c, sql);
        assert!(!result.is_empty(), "TPC-H Q{n} returned no rows");
    }
}

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len())]
    }
    fn value(&mut self, n: usize) -> Value {
        if self.below(6) == 0 {
            Value::Null
        } else {
            Value::Int64(self.below(n) as i64)
        }
    }
}

fn random_table(rng: &mut Rng, name: &str) -> Table {
    let schema = Schema::from_pairs(&[("id", Type::Int64), ("k", Type::Int64), ("v", Type::Int64)]);
    let mut t = Table::new(name, schema).unwrap();
    for id in 0..rng.below(10) as i64 {
        t.push_row(vec![Value::Int64(id), rng.value(3), rng.value(10)])
            .unwrap();
    }
    t
}

/// Random queries over two random tables, built from templates that each
/// stress a different rule: outer joins, derived tables, grouping, LIMIT,
/// DISTINCT, and correlated subqueries.
#[test]
fn random_queries_give_identical_results_with_and_without_the_optimizer() {
    let mut rng = Rng(0x0F7_1A5E_2026_1012);
    let preds = [
        "a.v > 4",
        "b.v < 5",
        "a.v > b.v",
        "a.k = b.k",
        "b.id IS NULL",
        "a.v IS NOT NULL",
        "a.v + b.v = 10",
        "1 = 1",
        "1 = 0",
        "(a.v > 2 OR b.v > 2)",
        "coalesce(b.v, 0) < 3",
    ];
    for _ in 0..150 {
        let mut c = Catalog::new();
        c.register(random_table(&mut rng, "a")).unwrap();
        c.register(random_table(&mut rng, "b")).unwrap();
        for _ in 0..8 {
            let kind = rng.pick(&["INNER", "LEFT", "RIGHT", "FULL"]);
            let (on, w1, w2) = (rng.pick(&preds), rng.pick(&preds), rng.pick(&preds));
            let sql = match rng.below(10) {
                0 => format!("SELECT a.id, b.id FROM a {kind} JOIN b ON {on} WHERE {w1} AND {w2}"),
                1 => format!("SELECT a.id, b.id FROM a, b WHERE {w1} AND {w2}"),
                2 => format!(
                    "SELECT t.k, count(*), sum(t.v) FROM (SELECT a.k, b.v FROM a {kind} JOIN b ON {on}) AS t \
                     WHERE t.k > 0 OR t.v > 3 GROUP BY t.k HAVING count(*) > 0"
                ),
                3 => format!(
                    "SELECT * FROM (SELECT a.id, a.v FROM a ORDER BY a.v, a.id LIMIT 4) AS t, b WHERE t.v > b.v AND {}",
                    rng.pick(&["b.v > 1", "1 = 1", "b.k IS NOT NULL"])
                ),
                4 => format!("SELECT DISTINCT a.k, b.k FROM a {kind} JOIN b ON {on} WHERE {w1}"),
                6 => format!(
                    "SELECT a.id, b.id, x.id FROM a, b, a AS x WHERE a.k = x.k AND {} AND {w1}",
                    rng.pick(&["x.v = b.v", "b.k = x.k", "b.v < x.v", "1 = 1"])
                ),
                5 => format!(
                    "SELECT a.id FROM a WHERE EXISTS (SELECT 1 FROM (SELECT count(*) AS n, sum(b.v) AS s FROM b) AS t WHERE {})",
                    rng.pick(&["a.v > 4", "1 = 0", "t.n > 2", "a.v > t.s", "a.k IS NULL"])
                ),
                7 => format!(
                    "SELECT a.id FROM a WHERE a.{} {} (SELECT b.{} FROM b WHERE {}) AND {}",
                    rng.pick(&["k", "v"]),
                    rng.pick(&["IN", "NOT IN"]),
                    rng.pick(&["k", "v"]),
                    rng.pick(&["b.v > 3", "b.v < a.v", "b.k = a.k", "1 = 1"]),
                    rng.pick(&["a.v > 2", "1 = 1"])
                ),
                _ => format!(
                    "SELECT a.id FROM a WHERE {} (SELECT 1 FROM b WHERE {} AND {}) AND {}",
                    rng.pick(&["EXISTS", "NOT EXISTS"]),
                    rng.pick(&["b.k = a.k", "b.v = a.v", "b.v > a.v", "b.k = a.k + 0"]),
                    rng.pick(&["b.v > 3", "b.v < a.v", "1 = 1", "b.k IS NULL"]),
                    rng.pick(&["a.v > 2", "1 = 1", "a.k IS NOT NULL"])
                ),
            };
            rows(&c, &sql);
        }
    }
}
