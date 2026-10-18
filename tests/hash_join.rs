//! Hash join semantics. Every query here runs twice, once with hash joins and
//! once with nested-loop joins only, and the two results must be identical.

use quarry::catalog::{Catalog, Schema, Table};
use quarry::csv::{parse_csv, CsvOptions};
use quarry::types::{Type, Value};
use quarry::{bind, execute_plan, execute_with, parse_query, plan_query, ExecOptions, Plan};

fn catalog(tables: &[(&str, &str)]) -> Catalog {
    let mut c = Catalog::new();
    for (name, csv) in tables {
        c.register(parse_csv(name, csv, &CsvOptions::default(), None).unwrap())
            .unwrap();
    }
    c
}

/// Runs `sql` with both join strategies, asserts they agree, and returns the
/// rows as strings.
fn rows(c: &Catalog, sql: &str) -> Vec<Vec<String>> {
    let q = parse_query(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let b = bind(c, &q).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let hashed = execute_with(
        c,
        &b,
        ExecOptions {
            hash_join: true,
            ..Default::default()
        },
    )
    .unwrap()
    .rows;
    let looped = execute_with(
        c,
        &b,
        ExecOptions {
            hash_join: false,
            ..Default::default()
        },
    )
    .unwrap()
    .rows;
    assert_eq!(
        hashed, looped,
        "hash and nested-loop joins disagree on {sql}"
    );
    hashed
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

const A: &str = "id,k,s\n1,10,x\n2,20,y\n3,,z\n4,10,w\n";
const B: &str = "id,k,s\n7,10,x\n8,30,y\n9,,z\n10,10,q\n";

#[test]
fn null_keys_never_match() {
    let c = catalog(&[("a", A), ("b", B)]);
    // a.3 and b.9 both have NULL k; they must not join to each other.
    assert_eq!(
        rows(
            &c,
            "SELECT a.id, b.id FROM a JOIN b ON a.k = b.k ORDER BY a.id, b.id"
        ),
        [["1", "7"], ["1", "10"], ["4", "7"], ["4", "10"]]
    );
}

#[test]
fn outer_joins_keep_null_key_rows() {
    let c = catalog(&[("a", A), ("b", B)]);
    let left = rows(&c, "SELECT a.id, b.id FROM a LEFT JOIN b ON a.k = b.k");
    assert!(left.contains(&vec!["3".into(), "NULL".into()]));
    assert!(left.contains(&vec!["2".into(), "NULL".into()]));
    assert_eq!(left.len(), 6);

    let right = rows(&c, "SELECT a.id, b.id FROM a RIGHT JOIN b ON a.k = b.k");
    assert!(
        right.contains(&vec!["NULL".into(), "9".into()]),
        "NULL-key build rows are emitted unmatched"
    );
    assert!(right.contains(&vec!["NULL".into(), "8".into()]));
    assert_eq!(right.len(), 6);

    assert_eq!(
        rows(&c, "SELECT a.id, b.id FROM a FULL JOIN b ON a.k = b.k").len(),
        8
    );
}

#[test]
fn multiple_keys() {
    let c = catalog(&[("a", A), ("b", B)]);
    assert_eq!(
        rows(
            &c,
            "SELECT a.id, b.id FROM a JOIN b ON a.k = b.k AND a.s = b.s"
        ),
        [["1", "7"]]
    );
}

#[test]
fn residual_conditions_decide_outer_join_matches() {
    let c = catalog(&[("a", A), ("b", B)]);
    // b.id > 8 rules out b.7, so a.1 and a.4 match only b.10.
    assert_eq!(
        rows(
            &c,
            "SELECT a.id, b.id FROM a LEFT JOIN b ON a.k = b.k AND b.id > 8 ORDER BY a.id"
        ),
        [["1", "10"], ["2", "NULL"], ["3", "NULL"], ["4", "10"]]
    );
    // A residual that is never true: every left row comes back unmatched.
    assert_eq!(
        rows(
            &c,
            "SELECT a.id, b.id FROM a LEFT JOIN b ON a.k = b.k AND b.id < 0"
        )
        .len(),
        4
    );
}

#[test]
fn keys_can_be_expressions() {
    let c = catalog(&[("a", A), ("b", B)]);
    assert_eq!(
        rows(
            &c,
            "SELECT a.id, b.id FROM a JOIN b ON a.k + 10 = b.k ORDER BY 1, 2"
        ),
        [["2", "8"]]
    );
    assert_eq!(
        rows(
            &c,
            "SELECT a.id, b.id FROM a JOIN b ON upper(a.s) = upper(b.s) ORDER BY 1"
        ),
        [["1", "7"], ["2", "8"], ["3", "9"]]
    );
}

#[test]
fn integer_and_float_keys_match_after_coercion() {
    let c = catalog(&[("a", A), ("f", "v\n10.0\n20.5\n")]);
    // The binder casts a.k to DOUBLE, so 10 matches 10.0.
    assert_eq!(
        rows(&c, "SELECT a.id FROM a JOIN f ON a.k = f.v ORDER BY 1"),
        [["1"], ["4"]]
    );
}

#[test]
fn where_equalities_become_hash_joins_for_comma_joins() {
    let c = catalog(&[("a", A), ("b", B)]);
    assert_eq!(
        rows(
            &c,
            "SELECT a.id, b.id FROM a, b WHERE a.k = b.k AND b.id > 8 ORDER BY 1"
        ),
        [["1", "10"], ["4", "10"]]
    );
}

#[test]
fn correlated_subquery_with_a_hash_join_inside() {
    let c = catalog(&[("a", A), ("b", B)]);
    // The join inside the subquery has an outer reference (o.s) in its
    // residual; it must re-run with each outer row in scope.
    assert_eq!(
        rows(&c, "SELECT o.id FROM a o WHERE EXISTS (SELECT 1 FROM a x JOIN b y ON x.k = y.k AND y.s = o.s) ORDER BY 1"),
        [["1"]]
    );
}

#[test]
fn duplicate_keys_on_both_sides_multiply() {
    let c = catalog(&[("l", "k\n1\n1\n1\n"), ("r", "k\n1\n1\n")]);
    assert_eq!(
        rows(&c, "SELECT count(*) FROM l JOIN r ON l.k = r.k"),
        [["6"]]
    );
}

// ---------------------------------------------------------------------------
// Randomized: multi-key joins with residuals, every join kind, NULLs and
// duplicates, compared against the nested-loop join.
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }

    fn maybe_null(&mut self, n: u64) -> Value {
        if self.below(6) == 0 {
            Value::Null
        } else {
            Value::Int64(self.below(n) as i64)
        }
    }
}

fn random_table(rng: &mut Rng, name: &str) -> Table {
    let schema = Schema::from_pairs(&[
        ("id", Type::Int64),
        ("k1", Type::Int64),
        ("k2", Type::Int64),
        ("v", Type::Int64),
    ]);
    let mut t = Table::new(name, schema).unwrap();
    for id in 0..rng.below(12) as i64 {
        t.push_row(vec![
            Value::Int64(id),
            rng.maybe_null(3),
            rng.maybe_null(2),
            rng.maybe_null(10),
        ])
        .unwrap();
    }
    t
}

#[test]
fn random_joins_agree_with_nested_loop() {
    let mut rng = Rng(0xA5A5_0B0B_1234_9876);
    let conditions = [
        "a.k1 = b.k1",
        "a.k1 = b.k1 AND a.k2 = b.k2",
        "a.k1 = b.k1 AND a.v < b.v",
        "b.k2 = a.k2 AND (a.v IS NULL OR b.v > 3)",
        "a.k1 + b.k2 = 2 AND a.k1 = b.k1",
        "a.k1 = b.k1 AND a.k1 = 1",
    ];
    for _ in 0..200 {
        let mut c = Catalog::new();
        c.register(random_table(&mut rng, "a")).unwrap();
        c.register(random_table(&mut rng, "b")).unwrap();
        for on in conditions {
            for kind in ["INNER", "LEFT", "RIGHT", "FULL"] {
                rows(
                    &c,
                    &format!("SELECT a.id, b.id FROM a {kind} JOIN b ON {on}"),
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Semi and anti joins. Built by hand here (the optimizer doesn't produce them
// yet) and checked against the same query written with EXISTS / NOT EXISTS,
// which runs as a correlated subquery.
// ---------------------------------------------------------------------------

/// Plans `SELECT a.id FROM a JOIN b ON <cond>`, then turns the join into a
/// semi (or anti) join: a's rows with (or without) a match in b.
fn semi_join_plan(c: &Catalog, cond: &str, anti: bool) -> (Plan, quarry::bound::Bound) {
    let q = parse_query(&format!("SELECT a.id FROM a JOIN b ON {cond}")).unwrap();
    let b = bind(c, &q).unwrap();
    let Plan::Project { input, exprs } = plan_query(&b.query) else {
        panic!("expected a Project")
    };
    let Plan::Join {
        left, right, on, ..
    } = *input
    else {
        panic!("expected a Join")
    };
    (
        Plan::Project {
            input: Box::new(Plan::SemiJoin {
                left,
                right,
                on,
                anti,
            }),
            exprs,
        },
        b,
    )
}

/// Checks a semi/anti join against [NOT] EXISTS, with and without hashing.
fn check_semi_join(c: &Catalog, cond: &str, anti: bool) -> Vec<Value> {
    let (plan, _) = semi_join_plan(c, cond, anti);
    let not = if anti { "NOT " } else { "" };
    let sql = format!("SELECT a.id FROM a WHERE {not}EXISTS (SELECT 1 FROM b WHERE {cond})");
    let q = parse_query(&sql).unwrap();
    let b = bind(c, &q).unwrap();
    let expected = execute_with(c, &b, ExecOptions::default()).unwrap().rows;
    for hash_join in [true, false] {
        let got = execute_plan(
            c,
            &plan,
            ExecOptions {
                hash_join,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(got, expected, "{sql} (hash_join = {hash_join})");
    }
    expected.into_iter().map(|mut r| r.remove(0)).collect()
}

#[test]
fn semi_join_keeps_each_matching_row_once() {
    let c = catalog(&[("a", A), ("b", B)]);
    // a.1 and a.4 (k = 10) each match two rows of b but appear once.
    assert_eq!(
        check_semi_join(&c, "a.k = b.k", false),
        [Value::Int64(1), Value::Int64(4)]
    );
}

#[test]
fn anti_join_keeps_rows_without_a_match_including_null_keys() {
    let c = catalog(&[("a", A), ("b", B)]);
    // a.3 has a NULL key: it matches nothing, so NOT EXISTS keeps it.
    assert_eq!(
        check_semi_join(&c, "a.k = b.k", true),
        [Value::Int64(2), Value::Int64(3)]
    );
}

#[test]
fn semi_joins_with_residuals_and_without_keys() {
    let c = catalog(&[("a", A), ("b", B)]);
    // The residual a.id > 2 rules out a.1, which matches b on the key alone.
    let int = Value::Int64;
    assert_eq!(
        check_semi_join(&c, "a.k = b.k AND a.id > 2", false),
        [int(4)]
    );
    assert_eq!(
        check_semi_join(&c, "a.k = b.k AND a.id > 2", true),
        [int(1), int(2), int(3)]
    );
    // No equality: falls back to trying every right row.
    check_semi_join(&c, "a.id + 6 < b.id", false);
    check_semi_join(&c, "a.id + 6 < b.id", true);
}

#[test]
fn semi_join_explain_text() {
    let c = catalog(&[("a", A), ("b", B)]);
    let (plan, b) = semi_join_plan(&c, "a.k = b.k AND b.id > 8", true);
    assert!(plan
        .explain(&b.columns)
        .contains("HashAntiJoin keys=[a.k = b.k] residual=[(b.id > 8)]"));
    let (plan, b) = semi_join_plan(&c, "a.id < b.id", false);
    assert!(plan
        .explain(&b.columns)
        .contains("NestedLoopSemiJoin on (a.id < b.id)"));
}

#[test]
fn random_semi_and_anti_joins_agree_with_exists() {
    let mut rng = Rng(0x5E31_A471_0000_0001);
    let conditions = [
        "a.k1 = b.k1",
        "a.k1 = b.k1 AND a.k2 = b.k2",
        "a.k1 = b.k1 AND a.v < b.v",
        "a.v > b.v",
    ];
    for _ in 0..200 {
        let mut c = Catalog::new();
        c.register(random_table(&mut rng, "a")).unwrap();
        c.register(random_table(&mut rng, "b")).unwrap();
        for cond in conditions {
            check_semi_join(&c, cond, false);
            check_semi_join(&c, cond, true);
        }
    }
}
