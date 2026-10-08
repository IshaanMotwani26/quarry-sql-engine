//! Expression semantics, checked against what Postgres returns for the same SQL.

use quarry::catalog::Catalog;
use quarry::eval::{eval, Layout, NoSubqueries, Scope};
use quarry::types::{parse_date, Value};
use quarry::{bind, parse_query};

/// Evaluates `SELECT <expr>` (no FROM) and returns the value or error text.
fn run(expr: &str) -> Result<Value, String> {
    let sql = format!("SELECT {expr}");
    let q = parse_query(&sql).unwrap_or_else(|e| panic!("parse failed for {sql:?}: {e}"));
    let b = bind(&Catalog::new(), &q).unwrap_or_else(|e| panic!("bind failed for {sql:?}: {e}"));
    let layout = Layout::default();
    let scope = Scope::new(&layout, &[], None);
    eval(&b.query.outputs()[0].expr, &scope, &NoSubqueries).map_err(|e| e.0)
}

fn val(expr: &str) -> Value {
    run(expr).unwrap_or_else(|e| panic!("{expr}: {e}"))
}

fn int(n: i64) -> Value {
    Value::Int64(n)
}

fn float(x: f64) -> Value {
    Value::Float64(x)
}

fn text(s: &str) -> Value {
    Value::Utf8(s.into())
}

fn boolean(b: bool) -> Value {
    Value::Boolean(b)
}

fn date(s: &str) -> Value {
    Value::Date(parse_date(s).unwrap())
}

#[test]
fn arithmetic() {
    assert_eq!(val("1 + 2 * 3"), int(7));
    assert_eq!(val("7 / 2"), int(3), "integer division truncates");
    assert_eq!(val("-7 / 2"), int(-3), "toward zero");
    assert_eq!(val("-7 % 3"), int(-1));
    assert_eq!(val("7 / 2.0"), float(3.5));
    assert_eq!(val("1 - 0.05"), float(0.95));
    assert_eq!(val("-(2 + 3)"), int(-5));
}

#[test]
fn arithmetic_errors() {
    assert_eq!(run("1 / 0").unwrap_err(), "division by zero");
    assert_eq!(run("1.5 / 0").unwrap_err(), "division by zero");
    assert_eq!(run("5 % 0").unwrap_err(), "division by zero");
    assert_eq!(
        run("9223372036854775807 + 1").unwrap_err(),
        "bigint out of range"
    );
    assert_eq!(
        run("-9223372036854775807 - 2").unwrap_err(),
        "bigint out of range"
    );
    assert_eq!(
        run("4611686018427387904 * 2").unwrap_err(),
        "bigint out of range"
    );
}

#[test]
fn null_propagation() {
    assert_eq!(val("NULL + 1"), Value::Null);
    assert_eq!(val("1 = NULL"), Value::Null);
    assert_eq!(val("NULL = NULL"), Value::Null);
    assert_eq!(val("upper(NULL)"), Value::Null);
    assert_eq!(val("NULL IS NULL"), boolean(true));
    assert_eq!(val("1 IS NOT NULL"), boolean(true));
    assert_eq!(val("coalesce(NULL, NULL, 3, 4)"), int(3));
}

#[test]
fn three_valued_logic_truth_tables() {
    let t = "TRUE";
    let f = "FALSE";
    let n = "NULL";
    let expect = |e: &str| match e {
        "T" => boolean(true),
        "F" => boolean(false),
        _ => Value::Null,
    };
    let and = [
        (t, t, "T"),
        (t, f, "F"),
        (t, n, "N"),
        (f, f, "F"),
        (f, n, "F"),
        (n, n, "N"),
    ];
    for (a, b, r) in and {
        assert_eq!(val(&format!("{a} AND {b}")), expect(r), "{a} AND {b}");
        assert_eq!(val(&format!("{b} AND {a}")), expect(r), "{b} AND {a}");
    }
    let or = [
        (t, t, "T"),
        (t, f, "T"),
        (t, n, "T"),
        (f, f, "F"),
        (f, n, "N"),
        (n, n, "N"),
    ];
    for (a, b, r) in or {
        assert_eq!(val(&format!("{a} OR {b}")), expect(r), "{a} OR {b}");
        assert_eq!(val(&format!("{b} OR {a}")), expect(r), "{b} OR {a}");
    }
    assert_eq!(val("NOT NULL"), Value::Null);
}

#[test]
fn and_short_circuits_before_errors() {
    assert_eq!(val("FALSE AND 1 / 0 = 1"), boolean(false));
    assert_eq!(val("TRUE OR 1 / 0 = 1"), boolean(true));
}

#[test]
fn in_list_with_nulls() {
    assert_eq!(val("2 IN (1, 2, 3)"), boolean(true));
    assert_eq!(val("5 IN (1, 2, 3)"), boolean(false));
    assert_eq!(
        val("5 IN (1, NULL)"),
        Value::Null,
        "no match + NULL item = NULL"
    );
    assert_eq!(val("1 IN (1, NULL)"), boolean(true));
    assert_eq!(
        val("5 NOT IN (1, NULL)"),
        Value::Null,
        "the classic NOT IN trap"
    );
    assert_eq!(val("NULL IN (1, 2)"), Value::Null);
    assert_eq!(
        val("2 IN (1.5, 2.0)"),
        boolean(true),
        "BIGINT is widened to DOUBLE"
    );
}

#[test]
fn between_and_comparisons() {
    assert_eq!(
        val("0.06 BETWEEN 0.06 - 0.01 AND 0.06 + 0.01"),
        boolean(true)
    );
    assert_eq!(val("5 NOT BETWEEN 1 AND 3"), boolean(true));
    assert_eq!(val("'apple' < 'banana'"), boolean(true));
    assert_eq!(val("2 = 2.0"), boolean(true));
}

#[test]
fn like() {
    assert_eq!(
        val("'special requests' LIKE '%special%requests%'"),
        boolean(true)
    );
    assert_eq!(val("'abc' NOT LIKE 'a_c'"), boolean(false));
    assert_eq!(val("NULL LIKE 'a%'"), Value::Null);
}

#[test]
fn case_expressions() {
    assert_eq!(
        val("CASE WHEN 1 > 2 THEN 'a' WHEN 2 > 1 THEN 'b' ELSE 'c' END"),
        text("b")
    );
    assert_eq!(
        val("CASE WHEN NULL THEN 1 ELSE 2 END"),
        int(2),
        "NULL condition is not true"
    );
    assert_eq!(
        val("CASE 2 WHEN 1 THEN 'one' WHEN 2 THEN 'two' END"),
        text("two")
    );
    assert_eq!(
        val("CASE NULL WHEN NULL THEN 'match' ELSE 'none' END"),
        text("none")
    );
    assert_eq!(val("CASE WHEN FALSE THEN 1 END"), Value::Null);
    // Only the chosen branch is evaluated.
    assert_eq!(val("CASE WHEN TRUE THEN 1 ELSE 1 / 0 END"), int(1));
}

#[test]
fn casts() {
    assert_eq!(val("CAST('42' AS INTEGER)"), int(42));
    assert_eq!(val("CAST(' 2.5 ' AS DOUBLE)"), float(2.5));
    assert_eq!(
        val("CAST(2.5 AS INTEGER)"),
        int(3),
        "rounds half away from zero"
    );
    assert_eq!(val("CAST(-2.5 AS INTEGER)"), int(-3));
    assert_eq!(val("CAST(42 AS VARCHAR)"), text("42"));
    assert_eq!(
        val("CAST(DATE '1995-03-15' AS VARCHAR)"),
        text("1995-03-15")
    );
    assert_eq!(val("CAST('1995-03-15' AS DATE)"), date("1995-03-15"));
    assert_eq!(val("CAST('yes' AS BOOLEAN)"), boolean(true));
    assert_eq!(
        run("CAST('abc' AS INTEGER)").unwrap_err(),
        "invalid input syntax for type bigint: \"abc\""
    );
    assert_eq!(
        run("CAST('1995-02-30' AS DATE)").unwrap_err(),
        "invalid input syntax for type date: \"1995-02-30\""
    );
    assert_eq!(
        run("CAST(1e300 AS INTEGER)").unwrap_err(),
        "bigint out of range"
    );
}

#[test]
fn dates() {
    // TPC-H Q1's cutoff date.
    assert_eq!(
        val("DATE '1998-12-01' - INTERVAL '90' DAY"),
        date("1998-09-02")
    );
    // TPC-H Q6's range end.
    assert_eq!(
        val("DATE '1994-01-01' + INTERVAL '1' YEAR"),
        date("1995-01-01")
    );
    assert_eq!(
        val("DATE '2024-01-31' + INTERVAL '1' MONTH"),
        date("2024-02-29")
    );
    assert_eq!(val("DATE '2024-03-01' - DATE '2024-02-01'"), int(29));
    assert_eq!(val("DATE '2024-02-28' + 2"), date("2024-03-01"));
    assert_eq!(val("EXTRACT(YEAR FROM DATE '1995-06-17')"), int(1995));
    assert_eq!(val("EXTRACT(MONTH FROM DATE '1995-06-17')"), int(6));
    assert_eq!(val("EXTRACT(DAY FROM DATE '1995-06-17')"), int(17));
    assert_eq!(val("DATE '1995-03-15' < '1995-03-16'"), boolean(true));
}

#[test]
fn string_functions() {
    assert_eq!(val("substring('13-345-678' FROM 1 FOR 2)"), text("13"));
    assert_eq!(val("substring('hello', 2)"), text("ello"));
    assert_eq!(val("upper('abc') || lower('DEF')"), text("ABCdef"));
    assert_eq!(
        val("length('héllo')"),
        int(5),
        "counts characters, not bytes"
    );
    assert_eq!(
        run("substring('abc', 1, -1)").unwrap_err(),
        "negative substring length not allowed"
    );
}

#[test]
fn numeric_functions() {
    assert_eq!(val("abs(-5)"), int(5));
    assert_eq!(val("abs(-2.5)"), float(2.5));
    assert_eq!(val("round(2.5)"), float(3.0));
    assert_eq!(val("round(2.71828, 2)"), float(2.72));
    assert_eq!(val("round(1234.5, -2)"), float(1200.0));
}

#[test]
fn subqueries_need_a_runner() {
    assert!(run("(SELECT 1)")
        .unwrap_err()
        .contains("subqueries cannot be evaluated"));
}
