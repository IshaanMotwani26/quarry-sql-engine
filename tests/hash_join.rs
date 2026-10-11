//! Hash join semantics. Every query here runs twice, once with hash joins and
//! once with nested-loop joins only, and the two results must be identical.

use quarry::catalog::{Catalog, Schema, Table};
use quarry::csv::{parse_csv, CsvOptions};
use quarry::types::{Type, Value};
use quarry::{bind, execute_with, parse_query, ExecOptions};

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
    let hashed = execute_with(c, &b, ExecOptions { hash_join: true })
        .unwrap()
        .rows;
    let looped = execute_with(c, &b, ExecOptions { hash_join: false })
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
