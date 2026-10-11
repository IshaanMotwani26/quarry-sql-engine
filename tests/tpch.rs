//! TPC-H data generator integrity, and end-to-end query checks on generated data.

use std::collections::{BTreeMap, HashSet};

use quarry::catalog::{Catalog, Table};
use quarry::types::{parse_date, Value};
use quarry::{bind, execute_with, parse_query, tpch, ExecOptions};

fn generated(scale: f64) -> Catalog {
    let mut c = Catalog::new();
    tpch::generate(&mut c, scale);
    c
}

fn run(catalog: &Catalog, sql: &str, hash_join: bool) -> Vec<Vec<Value>> {
    let q = parse_query(sql).unwrap();
    let b = bind(catalog, &q).unwrap();
    execute_with(catalog, &b, ExecOptions { hash_join })
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .rows
}

/// Reads one column of a table by name.
fn column<'t>(t: &'t Table, name: &str) -> impl Iterator<Item = Value> + 't {
    let i = t
        .schema
        .index_of(name)
        .unwrap_or_else(|| panic!("no column {name}"));
    (0..t.row_count()).map(move |r| t.columns[i].get(r))
}

fn int(v: Value) -> i64 {
    match v {
        Value::Int64(x) => x,
        other => panic!("expected BIGINT, got {other:?}"),
    }
}

fn float(v: &Value) -> f64 {
    match v {
        Value::Float64(x) => *x,
        Value::Int64(x) => *x as f64,
        other => panic!("expected a number, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Generator
// ---------------------------------------------------------------------------

#[test]
fn table_sizes_follow_the_scale_factor() {
    let c = generated(0.002);
    let count = |t: &str| c.get(t).unwrap().row_count() as i64;
    let sizes = tpch::Sizes::at(0.002);
    assert_eq!((count("region"), count("nation")), (5, 25));
    assert_eq!(count("supplier"), sizes.supplier);
    assert_eq!(count("part"), sizes.part);
    assert_eq!(
        count("partsupp"),
        sizes.part * tpch::suppliers_per_part(sizes.supplier)
    );
    assert_eq!(count("customer"), sizes.customer);
    assert_eq!(count("orders"), sizes.orders);
    let lines = count("lineitem");
    assert!((sizes.orders..=sizes.orders * 7).contains(&lines));
    // 1 to 7 lines per order averages 4.
    assert!(
        (3.5..4.5).contains(&(lines as f64 / sizes.orders as f64)),
        "{lines} lines"
    );
}

#[test]
fn foreign_keys_are_consistent() {
    let c = generated(0.002);
    let keys =
        |t: &str, col: &str| -> HashSet<i64> { column(c.get(t).unwrap(), col).map(int).collect() };
    let customers = keys("customer", "c_custkey");
    let orders = keys("orders", "o_orderkey");
    let order_customers = keys("orders", "o_custkey");
    assert!(order_customers.is_subset(&customers));
    // Customers whose key is divisible by 3 never order.
    assert!(order_customers.iter().all(|k| k % 3 != 0));
    assert!(keys("lineitem", "l_orderkey").is_subset(&orders));
    assert!(keys("supplier", "s_nationkey").is_subset(&keys("nation", "n_nationkey")));

    let pairs = |t: &str, a: &str, b: &str| -> HashSet<(i64, i64)> {
        let t = c.get(t).unwrap();
        column(t, a).map(int).zip(column(t, b).map(int)).collect()
    };
    let partsupp = pairs("partsupp", "ps_partkey", "ps_suppkey");
    assert_eq!(
        partsupp.len(),
        c.get("partsupp").unwrap().row_count(),
        "partsupp keys are unique"
    );
    assert!(pairs("lineitem", "l_partkey", "l_suppkey").is_subset(&partsupp));
}

#[test]
fn derived_fields_follow_the_spec() {
    let c = generated(0.002);
    let li = c.get("lineitem").unwrap();
    let current = Value::Date(parse_date("1995-06-17").unwrap());
    for ((ship, receipt), (flag, status)) in column(li, "l_shipdate")
        .zip(column(li, "l_receiptdate"))
        .zip(column(li, "l_returnflag").zip(column(li, "l_linestatus")))
    {
        let flag = flag.to_string();
        let status = status.to_string();
        assert_eq!(
            status == "O",
            quarry::eval::compare(&ship, &current).is_gt()
        );
        if quarry::eval::compare(&receipt, &current).is_le() {
            assert!(flag == "R" || flag == "A");
        } else {
            assert_eq!(flag, "N");
        }
    }
    // Phone numbers start with nation key + 10 (Q22 relies on this).
    let cust = c.get("customer").unwrap();
    for (phone, nation) in column(cust, "c_phone").zip(column(cust, "c_nationkey")) {
        assert!(phone
            .to_string()
            .starts_with(&format!("{}-", int(nation) + 10)));
    }
}

#[test]
fn partsupp_keys_are_unique_at_every_scale() {
    // Includes scales where the spec's supplier formula would repeat suppliers.
    for scale in [0.0001, 0.0005, 0.002, 0.01, 0.03] {
        let c = generated(scale);
        let t = c.get("partsupp").unwrap();
        let pairs: HashSet<(i64, i64)> = column(t, "ps_partkey")
            .map(int)
            .zip(column(t, "ps_suppkey").map(int))
            .collect();
        assert_eq!(
            pairs.len(),
            t.row_count(),
            "duplicate partsupp keys at scale {scale}"
        );
    }
}

#[test]
fn generation_is_deterministic() {
    let a = generated(0.001);
    let b = generated(0.001);
    for (name, _) in tpch::TABLES {
        assert_eq!(
            a.get(name),
            b.get(name),
            "table {name} differs between runs"
        );
    }
}

// ---------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------

#[test]
fn every_query_gives_identical_results_with_hash_and_nested_loop_joins() {
    // Small enough that nested-loop joins finish quickly in debug builds.
    let c = generated(0.0005);
    for (n, sql) in tpch::QUERIES {
        let hashed = run(&c, sql, true);
        let looped = run(&c, sql, false);
        // Exact equality, including float bits and row order: both joins
        // emit matches in the same order, so every sum adds in the same order.
        assert_eq!(hashed, looped, "TPC-H Q{n}");
        assert!(!hashed.is_empty(), "TPC-H Q{n} returned no rows");
    }
}

#[test]
fn q13_counts_customers_without_orders() {
    let c = generated(0.002);
    let rows = run(&c, tpch::Q13, true);
    let zero = rows
        .iter()
        .find(|r| r[0] == Value::Int64(0))
        .expect("a c_count = 0 bucket");
    // Every customer whose key is divisible by 3 has no orders; a few
    // others may also have drawn none.
    assert!(int(zero[1].clone()) >= tpch::Sizes::at(0.002).customer / 3);
    let total: i64 = rows.iter().map(|r| int(r[1].clone())).sum();
    assert_eq!(
        total,
        tpch::Sizes::at(0.002).customer,
        "every customer is in exactly one bucket"
    );
}

#[test]
fn q3_matches_reference() {
    let c = generated(0.002);
    let cutoff = Value::Date(parse_date("1995-03-15").unwrap());
    let cmp = quarry::eval::compare;

    let cust = c.get("customer").unwrap();
    let building: HashSet<i64> = column(cust, "c_custkey")
        .zip(column(cust, "c_mktsegment"))
        .filter(|(_, seg)| seg.to_string() == "BUILDING")
        .map(|(k, _)| int(k))
        .collect();

    let ord = c.get("orders").unwrap();
    let mut orders: BTreeMap<i64, (Value, i64)> = BTreeMap::new();
    for ((key, cust_key), (date, prio)) in column(ord, "o_orderkey")
        .zip(column(ord, "o_custkey"))
        .zip(column(ord, "o_orderdate").zip(column(ord, "o_shippriority")))
    {
        if building.contains(&int(cust_key)) && cmp(&date, &cutoff).is_lt() {
            orders.insert(int(key), (date, int(prio)));
        }
    }

    let li = c.get("lineitem").unwrap();
    let mut revenue: BTreeMap<i64, f64> = BTreeMap::new();
    for ((key, ship), (price, disc)) in column(li, "l_orderkey")
        .zip(column(li, "l_shipdate"))
        .zip(column(li, "l_extendedprice").zip(column(li, "l_discount")))
    {
        let key = int(key);
        if orders.contains_key(&key) && cmp(&ship, &cutoff).is_gt() {
            *revenue.entry(key).or_default() += float(&price) * (1.0 - float(&disc));
        }
    }

    let mut want: Vec<(i64, f64, Value, i64)> = revenue
        .into_iter()
        .map(|(key, rev)| {
            let (date, prio) = orders[&key].clone();
            (key, rev, date, prio)
        })
        .collect();
    want.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| cmp(&a.2, &b.2)));
    want.truncate(10);

    let got = run(&c, tpch::Q3, true);
    assert_eq!(got.len(), want.len());
    assert!(
        got.len() == 10,
        "generated data should produce a full top 10"
    );
    for (row, (key, rev, date, prio)) in got.iter().zip(&want) {
        assert_eq!(row[0], Value::Int64(*key));
        assert!(
            (float(&row[1]) - rev).abs() <= 1e-6 * rev.abs(),
            "revenue for order {key}"
        );
        assert_eq!(&row[2], date);
        assert_eq!(row[3], Value::Int64(*prio));
    }
}
