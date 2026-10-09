use quarry::catalog::{Catalog, Schema, Table};
use quarry::csv::{parse_csv, CsvOptions};
use quarry::types::{parse_date, Type, Value};
use quarry::{bind, execute, parse_query};

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

/// Results rendered as strings ("NULL" for NULL) for compact assertions.
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

fn col(sql: &str) -> Vec<String> {
    rows(sql).into_iter().map(|mut r| r.remove(0)).collect()
}

// ---------------------------------------------------------------------------
// Basics
// ---------------------------------------------------------------------------

#[test]
fn scan_filter_project() {
    assert_eq!(
        col("SELECT name FROM emp WHERE salary > 150000 ORDER BY id"),
        ["Ada", "Grace", "Edsger"]
    );
    assert_eq!(
        rows("SELECT id * 10, upper(name) FROM dept WHERE id = 2"),
        [["20", "SALES"]]
    );
    assert_eq!(
        rows("SELECT * FROM dept WHERE budget IS NULL"),
        [["4", "Legal", "NULL"]]
    );
}

#[test]
fn where_null_is_not_true() {
    // Margaret's dept_id is NULL: neither `= 1` nor `<> 1` holds.
    assert_eq!(
        col("SELECT name FROM emp WHERE dept_id <> 1"),
        ["Ken", "Barbara", "Edsger"]
    );
}

#[test]
fn select_without_from() {
    assert_eq!(rows("SELECT 1 + 1, 'a' || 'b'"), [["2", "ab"]]);
}

#[test]
fn empty_table_and_empty_result() {
    let mut c = demo();
    c.register(Table::new("empty", Schema::from_pairs(&[("x", Type::Int64)])).unwrap())
        .unwrap();
    assert!(query(&c, "SELECT x FROM empty").is_empty());
    assert!(query(&c, "SELECT name FROM emp WHERE 1 = 0").is_empty());
    assert!(query(&c, "SELECT e.name FROM emp e CROSS JOIN empty").is_empty());
}

// ---------------------------------------------------------------------------
// Joins
// ---------------------------------------------------------------------------

#[test]
fn inner_join() {
    assert_eq!(
        rows("SELECT e.name, d.name FROM emp e JOIN dept d ON e.dept_id = d.id WHERE d.id = 2 ORDER BY e.id"),
        [["Ken", "Sales"], ["Barbara", "Sales"]]
    );
}

#[test]
fn left_join_pads_unmatched_left_rows() {
    let r =
        rows("SELECT e.name, d.name FROM emp e LEFT JOIN dept d ON e.dept_id = d.id ORDER BY e.id");
    assert_eq!(r.len(), 7);
    assert_eq!(r[6], ["Margaret", "NULL"]);
}

#[test]
fn right_join_emits_unmatched_right_rows() {
    let r = rows("SELECT d.name, e.name FROM emp e RIGHT JOIN dept d ON e.dept_id = d.id ORDER BY d.id, e.id");
    assert_eq!(r.len(), 7);
    assert_eq!(r[6], ["Legal", "NULL"]);
}

#[test]
fn full_join_keeps_both_sides() {
    let r = rows("SELECT e.name, d.name FROM emp e FULL JOIN dept d ON e.dept_id = d.id");
    assert_eq!(r.len(), 8);
    assert!(r.contains(&vec!["Margaret".into(), "NULL".into()]));
    assert!(r.contains(&vec!["NULL".into(), "Legal".into()]));
}

#[test]
fn left_join_condition_is_not_a_filter() {
    // Putting the predicate in ON keeps every employee; putting it in WHERE would not.
    let r = rows("SELECT e.name, d.name FROM emp e LEFT JOIN dept d ON e.dept_id = d.id AND d.name = 'Sales'");
    assert_eq!(r.len(), 7);
    assert_eq!(r.iter().filter(|row| row[1] == "Sales").count(), 2);
}

#[test]
fn self_join_and_comma_join() {
    assert_eq!(
        rows("SELECT e.name, m.name FROM emp e, emp m WHERE e.manager_id = m.id ORDER BY e.id"),
        [["Grace", "Ada"], ["Linus", "Ada"], ["Barbara", "Ken"]]
    );
    assert_eq!(query(&demo(), "SELECT 1 FROM emp, dept").len(), 28);
}

// ---------------------------------------------------------------------------
// Sorting, DISTINCT, LIMIT
// ---------------------------------------------------------------------------

#[test]
fn order_by_directions_and_nulls() {
    assert_eq!(
        col("SELECT dept_id FROM emp ORDER BY dept_id"),
        ["1", "1", "1", "2", "2", "3", "NULL"]
    );
    assert_eq!(
        col("SELECT dept_id FROM emp ORDER BY dept_id DESC"),
        ["NULL", "3", "2", "2", "1", "1", "1"]
    );
    assert_eq!(
        col("SELECT dept_id FROM emp ORDER BY dept_id DESC NULLS LAST")[6],
        "NULL"
    );
    assert_eq!(
        col("SELECT dept_id FROM emp ORDER BY dept_id NULLS FIRST")[0],
        "NULL"
    );
}

#[test]
fn order_by_multiple_keys_alias_position_and_hidden_column() {
    assert_eq!(
        col("SELECT name FROM emp ORDER BY dept_id DESC NULLS LAST, salary"),
        ["Edsger", "Ken", "Barbara", "Linus", "Grace", "Ada", "Margaret"]
    );
    assert_eq!(
        col("SELECT name AS n FROM dept ORDER BY n"),
        ["Engineering", "Legal", "Research", "Sales"]
    );
    assert_eq!(
        col("SELECT name, id FROM dept ORDER BY 2 DESC"),
        ["Legal", "Research", "Sales", "Engineering"]
    );
    // Sorted by a column that isn't in the output.
    assert_eq!(
        col("SELECT name FROM emp ORDER BY hired LIMIT 2"),
        ["Edsger", "Ken"]
    );
    assert_eq!(
        rows("SELECT name FROM emp ORDER BY hired LIMIT 1")[0].len(),
        1
    );
}

#[test]
fn sort_is_stable_for_ties() {
    assert_eq!(
        col("SELECT name FROM emp WHERE dept_id = 1 ORDER BY dept_id"),
        ["Ada", "Grace", "Linus"]
    );
}

#[test]
fn distinct() {
    assert_eq!(
        col("SELECT DISTINCT dept_id FROM emp ORDER BY dept_id"),
        ["1", "2", "3", "NULL"]
    );
    assert_eq!(
        rows("SELECT DISTINCT dept_id, manager_id FROM emp WHERE dept_id = 1").len(),
        2
    );
}

#[test]
fn limit_and_offset() {
    let all = col("SELECT id FROM emp ORDER BY id");
    assert_eq!(col("SELECT id FROM emp ORDER BY id LIMIT 3"), all[..3]);
    assert_eq!(
        col("SELECT id FROM emp ORDER BY id LIMIT 3 OFFSET 2"),
        all[2..5]
    );
    assert_eq!(col("SELECT id FROM emp ORDER BY id OFFSET 5"), all[5..]);
    assert!(col("SELECT id FROM emp ORDER BY id OFFSET 100").is_empty());
    assert!(col("SELECT id FROM emp LIMIT 0").is_empty());
}

// ---------------------------------------------------------------------------
// Subqueries
// ---------------------------------------------------------------------------

#[test]
fn derived_tables() {
    assert_eq!(
        col("SELECT big FROM (SELECT name AS big, salary FROM emp WHERE salary > 150000) AS x ORDER BY salary"),
        ["Edsger", "Grace", "Ada"]
    );
    assert_eq!(
        col("SELECT b FROM (SELECT id, name FROM dept) AS t (a, b) WHERE a = 3"),
        ["Research"]
    );
}

#[test]
fn correlated_exists_and_not_exists() {
    assert_eq!(
        col("SELECT name FROM dept d WHERE EXISTS (SELECT 1 FROM emp e WHERE e.dept_id = d.id) ORDER BY id"),
        ["Engineering", "Sales", "Research"]
    );
    assert_eq!(
        col(
            "SELECT name FROM dept d WHERE NOT EXISTS (SELECT 1 FROM emp e WHERE e.dept_id = d.id)"
        ),
        ["Legal"]
    );
}

#[test]
fn in_and_not_in_subqueries() {
    assert_eq!(
        col("SELECT name FROM emp WHERE dept_id IN (SELECT id FROM dept WHERE budget > 300000) ORDER BY id"),
        ["Ada", "Grace", "Linus", "Edsger"]
    );
    // The NOT IN trap: one NULL in the subquery makes NOT IN return no rows.
    assert!(col("SELECT name FROM dept WHERE id NOT IN (SELECT dept_id FROM emp)").is_empty());
    assert_eq!(
        col("SELECT name FROM dept WHERE id NOT IN (SELECT dept_id FROM emp WHERE dept_id IS NOT NULL)"),
        ["Legal"]
    );
}

#[test]
fn correlated_scalar_subquery() {
    assert_eq!(
        rows("SELECT name, (SELECT name FROM dept WHERE id = emp.dept_id) FROM emp WHERE id IN (1, 7) ORDER BY id"),
        [["Ada", "Engineering"], ["Margaret", "NULL"]]
    );
    let e = try_query(&demo(), "SELECT (SELECT name FROM emp) FROM dept").unwrap_err();
    assert_eq!(
        e,
        "more than one row returned by a subquery used as an expression"
    );
}

#[test]
fn nested_correlation_reaches_two_levels_out() {
    // The innermost query references `d`, two scopes up.
    let sql = "SELECT d.name FROM dept d WHERE EXISTS (
                 SELECT 1 FROM emp e WHERE e.dept_id = d.id AND EXISTS (
                   SELECT 1 FROM emp m WHERE m.id = e.manager_id AND m.dept_id = d.id))
               ORDER BY d.id";
    assert_eq!(col(sql), ["Engineering", "Sales"]);
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[test]
fn runtime_errors_surface() {
    let c = demo();
    assert_eq!(
        try_query(&c, "SELECT salary / (id - 1) FROM emp").unwrap_err(),
        "division by zero"
    );
    assert_eq!(
        try_query(&c, "SELECT (SELECT name FROM dept)").unwrap_err(),
        "more than one row returned by a subquery used as an expression"
    );
}

#[test]
fn dates_round_trip_through_execution() {
    let r = query(
        &demo(),
        "SELECT hired + INTERVAL '1' YEAR FROM emp WHERE id = 1",
    );
    assert_eq!(r, [[Value::Date(parse_date("2020-03-04").unwrap())]]);
}

// ---------------------------------------------------------------------------
// Property test: every join kind matches a brute-force reference on random
// tables with duplicate and NULL keys.
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// A table of (id, k) rows where k is NULL about 1 time in 5.
fn random_table(rng: &mut Rng, name: &str) -> Table {
    let mut t = Table::new(
        name,
        Schema::from_pairs(&[("id", Type::Int64), ("k", Type::Int64)]),
    )
    .unwrap();
    for id in 0..rng.below(8) as i64 {
        let k = if rng.below(5) == 0 {
            Value::Null
        } else {
            Value::Int64(rng.below(4) as i64)
        };
        t.push_row(vec![Value::Int64(id), k]).unwrap();
    }
    t
}

type Pair = (Option<i64>, Option<i64>);

fn reference_join(a: &Table, b: &Table, kind: &str) -> Vec<Pair> {
    let id = |t: &Table, i: usize| match t.row(i)[0] {
        Value::Int64(x) => x,
        _ => unreachable!(),
    };
    let key = |t: &Table, i: usize| t.row(i)[1].clone();
    let matches = |i: usize, j: usize| !key(a, i).is_null() && key(a, i) == key(b, j);
    let mut out = Vec::new();
    let mut b_matched = vec![false; b.row_count()];
    for i in 0..a.row_count() {
        let mut matched = false;
        for (j, seen) in b_matched.iter_mut().enumerate() {
            if kind == "CROSS" || matches(i, j) {
                out.push((Some(id(a, i)), Some(id(b, j))));
                matched = true;
                *seen = true;
            }
        }
        if !matched && matches!(kind, "LEFT" | "FULL") {
            out.push((Some(id(a, i)), None));
        }
    }
    if matches!(kind, "RIGHT" | "FULL") {
        for (j, seen) in b_matched.iter().enumerate() {
            if !seen {
                out.push((None, Some(id(b, j))));
            }
        }
    }
    out.sort();
    out
}

#[test]
fn random_joins_match_reference() {
    let mut rng = Rng(0x5EED_1234_ABCD_0001);
    for _ in 0..300 {
        let mut c = Catalog::new();
        let a = random_table(&mut rng, "a");
        let b = random_table(&mut rng, "b");
        c.register(a.clone()).unwrap();
        c.register(b.clone()).unwrap();
        for kind in ["INNER", "LEFT", "RIGHT", "FULL", "CROSS"] {
            let sql = if kind == "CROSS" {
                "SELECT a.id, b.id FROM a CROSS JOIN b".to_string()
            } else {
                format!("SELECT a.id, b.id FROM a {kind} JOIN b ON a.k = b.k")
            };
            let mut got: Vec<Pair> = query(&c, &sql)
                .into_iter()
                .map(|r| {
                    let f = |v: &Value| match v {
                        Value::Int64(x) => Some(*x),
                        _ => None,
                    };
                    (f(&r[0]), f(&r[1]))
                })
                .collect();
            got.sort();
            assert_eq!(
                got,
                reference_join(&a, &b, kind),
                "{sql}\na = {a:?}\nb = {b:?}"
            );
        }
    }
}
