use quarry::catalog::{Catalog, Schema, Table};
use quarry::csv::{parse_csv, CsvOptions};
use quarry::types::{format_date, parse_date, Type, Value};
use quarry::{bind, execute, parse_query, tpch};

const DEPT: &str = "\
id,name,budget
1,Engineering,500000
2,Sales,200000
3,Research,350000
4,Legal,
";

const EMP: &str = "\
id,name,dept_id,salary,hired,manager_id
1,Ada,1,185000,2019-03-04,
2,Grace,1,172000,2020-07-15,1
3,Linus,1,140000,2022-01-10,1
4,Ken,2,98000,2018-11-01,
5,Barbara,2,105000,2021-05-20,4
6,Edsger,3,160000,2017-09-12,
7,Margaret,,120000,2023-02-28,
";

fn demo() -> Catalog {
    let mut c = Catalog::new();
    for (name, csv) in [("dept", DEPT), ("emp", EMP)] {
        c.register(parse_csv(name, csv, &CsvOptions::default(), None).unwrap())
            .unwrap();
    }
    c
}

fn try_query(catalog: &Catalog, sql: &str) -> Result<Vec<Vec<Value>>, String> {
    let q = parse_query(sql).unwrap_or_else(|e| panic!("parse failed for {sql:?}: {e}"));
    let b = bind(catalog, &q).unwrap_or_else(|e| panic!("bind failed for {sql:?}: {e}"));
    execute(catalog, &b).map(|r| r.rows).map_err(|e| e.0)
}

fn query(catalog: &Catalog, sql: &str) -> Vec<Vec<Value>> {
    try_query(catalog, sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn rows(sql: &str) -> Vec<Vec<String>> {
    query(&demo(), sql)
        .into_iter()
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

// ---------------------------------------------------------------------------
// Aggregate functions
// ---------------------------------------------------------------------------

#[test]
fn scalar_aggregates() {
    assert_eq!(
        rows("SELECT count(*), sum(salary), avg(salary), min(salary), max(salary) FROM emp"),
        [["7", "980000", "140000", "98000", "185000"]]
    );
}

#[test]
fn count_star_vs_count_column() {
    // count(*) counts rows; count(x) skips NULLs.
    assert_eq!(
        rows("SELECT count(*), count(manager_id), count(dept_id) FROM emp"),
        [["7", "3", "6"]]
    );
}

#[test]
fn min_max_on_text_and_dates() {
    assert_eq!(
        rows("SELECT min(name), max(name), min(hired), max(hired) FROM emp"),
        [["Ada", "Margaret", "2017-09-12", "2023-02-28"]]
    );
}

#[test]
fn distinct_aggregates_ignore_duplicates_and_nulls() {
    assert_eq!(
        rows("SELECT count(DISTINCT dept_id), count(DISTINCT manager_id) FROM emp"),
        [["3", "2"]]
    );
    assert_eq!(
        rows("SELECT sum(DISTINCT dept_id), avg(DISTINCT dept_id) FROM emp"),
        [["6", "2"]]
    );
}

#[test]
fn aggregates_over_empty_input() {
    // No GROUP BY: one row, with count 0 and everything else NULL.
    assert_eq!(
        rows(
            "SELECT count(*), count(id), sum(salary), avg(salary), min(name) FROM emp WHERE 1 = 0"
        ),
        [["0", "0", "NULL", "NULL", "NULL"]]
    );
    // GROUP BY: no groups, so no rows.
    assert!(rows("SELECT dept_id, count(*) FROM emp WHERE 1 = 0 GROUP BY dept_id").is_empty());
}

#[test]
fn aggregates_over_all_null_input() {
    assert_eq!(
        rows("SELECT count(manager_id), sum(manager_id), max(manager_id) FROM emp WHERE id = 1"),
        [["0", "NULL", "NULL"]]
    );
}

// ---------------------------------------------------------------------------
// GROUP BY and HAVING
// ---------------------------------------------------------------------------

#[test]
fn group_by_with_order_by_aggregate() {
    assert_eq!(
        rows("SELECT dept_id, count(*) AS n, sum(salary) FROM emp WHERE dept_id IS NOT NULL GROUP BY dept_id ORDER BY n DESC"),
        [["1", "3", "497000"], ["2", "2", "203000"], ["3", "1", "160000"]]
    );
}

#[test]
fn nulls_form_one_group() {
    let r = rows("SELECT dept_id, count(*) FROM emp GROUP BY dept_id ORDER BY dept_id");
    assert_eq!(r.len(), 4);
    assert_eq!(r[3], ["NULL", "1"]);
}

#[test]
fn groups_appear_in_first_seen_order_without_order_by() {
    let r = rows("SELECT dept_id FROM emp GROUP BY dept_id");
    let keys: Vec<&str> = r.iter().map(|row| row[0].as_str()).collect();
    assert_eq!(keys, ["1", "2", "3", "NULL"]);
}

#[test]
fn group_by_multiple_keys_and_expressions() {
    assert_eq!(
        rows("SELECT extract(year from hired) >= 2020 AS recent, dept_id = 1 AS eng, count(*) FROM emp \
              WHERE dept_id IS NOT NULL GROUP BY extract(year from hired) >= 2020, dept_id = 1 ORDER BY 1, 2"),
        [["false", "false", "2"], ["false", "true", "1"], ["true", "false", "1"], ["true", "true", "2"]]
    );
}

#[test]
fn expressions_over_aggregates() {
    assert_eq!(
        rows("SELECT dept_id, max(salary) - min(salary), sum(salary) / count(*) FROM emp WHERE dept_id = 1 GROUP BY dept_id"),
        [["1", "45000", "165666"]]
    );
}

#[test]
fn having() {
    assert_eq!(
        rows("SELECT dept_id, count(*) FROM emp GROUP BY dept_id HAVING count(*) > 1 ORDER BY dept_id"),
        [["1", "3"], ["2", "2"]]
    );
    // HAVING may use an aggregate that isn't in the select list.
    assert_eq!(
        rows("SELECT dept_id FROM emp GROUP BY dept_id HAVING avg(salary) > 150000 ORDER BY 1"),
        [["1"], ["3"]]
    );
    // HAVING without GROUP BY makes the whole table one group.
    assert_eq!(
        rows("SELECT count(*) FROM emp HAVING count(*) > 5"),
        [["7"]]
    );
    assert!(rows("SELECT count(*) FROM emp HAVING count(*) > 100").is_empty());
}

#[test]
fn distinct_over_grouped_output() {
    assert_eq!(
        rows("SELECT DISTINCT count(*) FROM emp GROUP BY dept_id ORDER BY 1"),
        [["1"], ["2"], ["3"]]
    );
}

// ---------------------------------------------------------------------------
// Aggregates combined with joins and subqueries
// ---------------------------------------------------------------------------

#[test]
fn left_join_then_group_counts_zero_for_empty_groups() {
    assert_eq!(
        rows("SELECT d.name, count(e.id) FROM dept d LEFT JOIN emp e ON e.dept_id = d.id GROUP BY d.name ORDER BY d.name"),
        [["Engineering", "3"], ["Legal", "0"], ["Research", "1"], ["Sales", "2"]]
    );
}

#[test]
fn aggregate_over_an_aggregated_derived_table() {
    // TPC-H Q13's shape: count departments by headcount.
    assert_eq!(
        rows("SELECT headcount, count(*) FROM (SELECT d.id, count(e.id) AS headcount FROM dept d \
              LEFT JOIN emp e ON e.dept_id = d.id GROUP BY d.id) AS x GROUP BY headcount ORDER BY headcount"),
        [["0", "1"], ["1", "1"], ["2", "1"], ["3", "1"]]
    );
}

#[test]
fn uncorrelated_aggregate_subquery() {
    assert_eq!(
        rows("SELECT name FROM emp WHERE salary > (SELECT avg(salary) FROM emp) ORDER BY name"),
        [["Ada"], ["Edsger"], ["Grace"]]
    );
}

#[test]
fn correlated_aggregate_subquery_runs_per_row() {
    // Each employee is compared with their own department's average.
    // Margaret's dept_id is NULL, so her department average is NULL and she's excluded.
    assert_eq!(
        rows("SELECT name FROM emp e WHERE salary > (SELECT avg(salary) FROM emp e2 WHERE e2.dept_id = e.dept_id) ORDER BY name"),
        [["Ada"], ["Barbara"], ["Grace"]]
    );
}

#[test]
fn having_with_subquery() {
    assert_eq!(
        rows(
            "SELECT dept_id FROM emp WHERE dept_id IS NOT NULL GROUP BY dept_id \
              HAVING sum(salary) > (SELECT budget FROM dept WHERE id = 3) ORDER BY 1"
        ),
        [["1"]]
    );
}

// ---------------------------------------------------------------------------
// Numeric behavior
// ---------------------------------------------------------------------------

fn single_column(name: &str, ty: Type, values: Vec<Value>) -> Catalog {
    let mut t = Table::new(name, Schema::from_pairs(&[("x", ty)])).unwrap();
    for v in values {
        t.push_row(vec![v]).unwrap();
    }
    let mut c = Catalog::new();
    c.register(t).unwrap();
    c
}

#[test]
fn float_sum_is_compensated() {
    let c = single_column("t", Type::Float64, vec![Value::Float64(0.1); 10]);
    assert_eq!(
        query(&c, "SELECT sum(x), avg(x) FROM t"),
        [[Value::Float64(1.0), Value::Float64(0.1)]]
    );
}

#[test]
fn integer_sum_overflow_is_an_error() {
    let c = single_column(
        "t",
        Type::Int64,
        vec![Value::Int64(i64::MAX), Value::Int64(1)],
    );
    assert_eq!(
        try_query(&c, "SELECT sum(x) FROM t").unwrap_err(),
        "bigint out of range"
    );
    // avg works in DOUBLE, so it doesn't overflow.
    assert!(try_query(&c, "SELECT avg(x) FROM t").is_ok());
}

// ---------------------------------------------------------------------------
// TPC-H Q1 and Q6 on generated lineitem data, checked against a reference
// implementation written directly in Rust.
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + (self.next() % (hi - lo + 1) as u64) as i64
    }
}

struct Line {
    quantity: f64,
    price: f64,
    discount: f64,
    tax: f64,
    returnflag: &'static str,
    linestatus: &'static str,
    shipdate: i32,
}

fn generate_lineitem(n: usize) -> (Catalog, Vec<Line>) {
    let mut rng = Rng(0x7C9_0001_DEAD_BEEF);
    let mut table = Table::new("lineitem", tpch::schema("lineitem").unwrap()).unwrap();
    let mut lines = Vec::with_capacity(n);
    let start = parse_date("1992-01-02").unwrap() as i64;
    let end = parse_date("1998-12-01").unwrap() as i64;
    for i in 0..n {
        let quantity = rng.range(1, 50) as f64;
        let line = Line {
            quantity,
            price: quantity * rng.range(90_000, 200_000) as f64 / 100.0,
            discount: rng.range(0, 10) as f64 / 100.0,
            tax: rng.range(0, 8) as f64 / 100.0,
            returnflag: ["A", "N", "R"][rng.range(0, 2) as usize],
            linestatus: ["F", "O"][rng.range(0, 1) as usize],
            shipdate: rng.range(start, end) as i32,
        };
        let text = |s: &str| Value::Utf8(s.into());
        table
            .push_row(vec![
                Value::Int64(i as i64 / 4 + 1),
                Value::Int64(rng.range(1, 2000)),
                Value::Int64(rng.range(1, 100)),
                Value::Int64(i as i64 % 4 + 1),
                Value::Float64(line.quantity),
                Value::Float64(line.price),
                Value::Float64(line.discount),
                Value::Float64(line.tax),
                text(line.returnflag),
                text(line.linestatus),
                Value::Date(line.shipdate),
                Value::Date(line.shipdate + 30),
                Value::Date(line.shipdate + 15),
                text("DELIVER IN PERSON"),
                text("TRUCK"),
                text("generated"),
            ])
            .unwrap();
        lines.push(line);
    }
    let mut c = Catalog::new();
    c.register(table).unwrap();
    (c, lines)
}

fn float(v: &Value) -> f64 {
    match v {
        Value::Float64(x) => *x,
        Value::Int64(x) => *x as f64,
        other => panic!("expected a number, got {other:?}"),
    }
}

fn assert_close(got: f64, want: f64, what: &str) {
    let tolerance = 1e-9 * want.abs().max(1.0);
    assert!(
        (got - want).abs() <= tolerance,
        "{what}: got {got}, want {want}"
    );
}

#[test]
fn tpch_q1_matches_reference() {
    let (catalog, lines) = generate_lineitem(5000);
    let cutoff = parse_date("1998-09-02").unwrap(); // 1998-12-01 - 90 days

    // Reference: group by (returnflag, linestatus), accumulate in plain f64.
    #[derive(Default)]
    struct Acc {
        qty: f64,
        base: f64,
        disc_price: f64,
        charge: f64,
        disc: f64,
        count: i64,
    }
    let mut groups: std::collections::BTreeMap<(&str, &str), Acc> = Default::default();
    for l in lines.iter().filter(|l| l.shipdate <= cutoff) {
        let g = groups.entry((l.returnflag, l.linestatus)).or_default();
        g.qty += l.quantity;
        g.base += l.price;
        g.disc_price += l.price * (1.0 - l.discount);
        g.charge += l.price * (1.0 - l.discount) * (1.0 + l.tax);
        g.disc += l.discount;
        g.count += 1;
    }

    let result = query(&catalog, tpch::Q1);
    assert_eq!(result.len(), groups.len());
    for (row, ((flag, status), g)) in result.iter().zip(&groups) {
        assert_eq!(row[0], Value::Utf8(flag.to_string()));
        assert_eq!(row[1], Value::Utf8(status.to_string()));
        let n = g.count as f64;
        for (i, (want, what)) in [
            (g.qty, "sum_qty"),
            (g.base, "sum_base_price"),
            (g.disc_price, "sum_disc_price"),
            (g.charge, "sum_charge"),
            (g.qty / n, "avg_qty"),
            (g.base / n, "avg_price"),
            (g.disc / n, "avg_disc"),
        ]
        .into_iter()
        .enumerate()
        {
            assert_close(float(&row[i + 2]), want, &format!("{flag}/{status} {what}"));
        }
        assert_eq!(row[9], Value::Int64(g.count));
    }
}

#[test]
fn tpch_q6_matches_reference() {
    let (catalog, lines) = generate_lineitem(5000);
    let from = parse_date("1994-01-01").unwrap();
    let to = parse_date("1995-01-01").unwrap();
    // Same float arithmetic as the engine; see the README's note on DECIMAL.
    let (lo, hi) = (0.06 - 0.01, 0.06 + 0.01);
    let want: f64 = lines
        .iter()
        .filter(|l| l.shipdate >= from && l.shipdate < to)
        .filter(|l| l.discount >= lo && l.discount <= hi && l.quantity < 24.0)
        .map(|l| l.price * l.discount)
        .sum();
    let result = query(&catalog, tpch::Q6);
    assert_close(float(&result[0][0]), want, "revenue");
    assert!(
        want > 0.0,
        "generated data should hit Q6's filter (dates {} to {})",
        format_date(from),
        format_date(to)
    );
}
