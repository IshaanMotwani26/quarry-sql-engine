use quarry::bound::*;
use quarry::catalog::{Catalog, Schema, Table};
use quarry::types::{parse_date, Type, Value};
use quarry::{bind, parse_query, tpch};

fn catalog() -> Catalog {
    let mut c = Catalog::new();
    tpch::register_schemas(&mut c);
    let t = Table::new(
        "t",
        Schema::from_pairs(&[
            ("a", Type::Int64),
            ("b", Type::Float64),
            ("s", Type::Utf8),
            ("d", Type::Date),
        ]),
    )
    .unwrap();
    c.register(t).unwrap();
    let u = Table::new(
        "u",
        Schema::from_pairs(&[("a", Type::Int64), ("c", Type::Boolean)]),
    )
    .unwrap();
    c.register(u).unwrap();
    c
}

fn bind_ok(sql: &str) -> Bound {
    let q = parse_query(sql).unwrap_or_else(|e| panic!("parse failed for {sql:?}: {e}"));
    bind(&catalog(), &q).unwrap_or_else(|e| panic!("bind failed for {sql:?}: {e}"))
}

fn bind_err(sql: &str) -> String {
    let q = parse_query(sql).unwrap_or_else(|e| panic!("parse failed for {sql:?}: {e}"));
    match bind(&catalog(), &q) {
        Ok(_) => panic!("expected a bind error for {sql:?}"),
        Err(e) => e.0,
    }
}

fn output_types(b: &Bound) -> Vec<(String, Type)> {
    b.query
        .outputs()
        .iter()
        .map(|o| (o.name.clone(), o.ty()))
        .collect()
}

fn names_and_types(pairs: &[(&str, Type)]) -> Vec<(String, Type)> {
    pairs.iter().map(|(n, t)| (n.to_string(), *t)).collect()
}

// ---------------------------------------------------------------------------
// TPC-H: every supported query binds with the expected output schema
// ---------------------------------------------------------------------------

#[test]
fn all_tpch_queries_bind() {
    for (n, sql) in tpch::QUERIES {
        let q = parse_query(sql).unwrap();
        if let Err(e) = bind(&catalog(), &q) {
            panic!("TPC-H Q{n} failed to bind: {e}");
        }
    }
}

#[test]
fn tpch_q1_output_schema() {
    use Type::*;
    let b = bind_ok(tpch::Q1);
    assert_eq!(
        output_types(&b),
        names_and_types(&[
            ("l_returnflag", Utf8),
            ("l_linestatus", Utf8),
            ("sum_qty", Float64),
            ("sum_base_price", Float64),
            ("sum_disc_price", Float64),
            ("sum_charge", Float64),
            ("avg_qty", Float64),
            ("avg_price", Float64),
            ("avg_disc", Float64),
            ("count_order", Int64),
        ])
    );
    assert_eq!(b.query.group_by.len(), 2);
    assert_eq!(b.query.aggregates.len(), 8);
    assert_eq!(b.query.order_by.len(), 2);
}

#[test]
fn tpch_q13_derived_column_aliases() {
    let b = bind_ok(tpch::Q13);
    let Some(BoundTableRef::Derived { columns, .. }) = &b.query.from else {
        panic!("expected derived table")
    };
    let names: Vec<&str> = columns
        .iter()
        .map(|&id| b.column(id).name.as_str())
        .collect();
    assert_eq!(names, ["c_custkey", "c_count"]);
    assert_eq!(
        output_types(&b),
        names_and_types(&[("c_count", Type::Int64), ("custdist", Type::Int64)])
    );
}

#[test]
fn tpch_q22_not_exists_is_correlated() {
    let b = bind_ok(tpch::Q22);
    let Some(BoundTableRef::Derived { query: inner, .. }) = &b.query.from else {
        panic!("expected derived table")
    };
    // Find NOT EXISTS (...) inside the derived table's WHERE clause.
    let mut correlated = None;
    inner.filter.as_ref().unwrap().walk(&mut |e| {
        if let ExprKind::Subquery {
            query,
            kind: SubqueryKind::Exists { negated: true },
        } = &e.kind
        {
            correlated = Some(query.correlated.clone());
        }
    });
    let correlated = correlated.expect("NOT EXISTS subquery");
    assert_eq!(correlated.len(), 1);
    assert_eq!(b.column(correlated[0]).display_name(), "customer.c_custkey");
}

#[test]
fn self_join_gets_distinct_column_ids() {
    let b = bind_ok("SELECT n1.n_name, n2.n_name FROM nation n1, nation n2 WHERE n1.n_regionkey = n2.n_regionkey");
    let ids: Vec<ColumnId> = b
        .query
        .outputs()
        .iter()
        .map(|o| match o.expr.kind {
            ExprKind::Column(id) => id,
            _ => panic!(),
        })
        .collect();
    assert_ne!(ids[0], ids[1]);
    assert_eq!(b.column(ids[0]).display_name(), "n1.n_name");
    assert_eq!(b.column(ids[1]).display_name(), "n2.n_name");
}

// ---------------------------------------------------------------------------
// Types and coercion
// ---------------------------------------------------------------------------

#[test]
fn integer_literals_widen_to_double() {
    let b = bind_ok("SELECT a + b, a + 1, b * 2 FROM t");
    let tys: Vec<Type> = b.query.outputs().iter().map(|o| o.ty()).collect();
    assert_eq!(tys, [Type::Float64, Type::Int64, Type::Float64]);
    // `b * 2`: the literal 2 is folded to 2.0 instead of wrapped in a CAST.
    let ExprKind::Binary { right, .. } = &b.query.outputs()[2].expr.kind else {
        panic!()
    };
    assert_eq!(right.kind, ExprKind::Literal(Value::Float64(2.0)));
}

#[test]
fn string_literal_compared_with_date_becomes_a_date() {
    let b = bind_ok("SELECT a FROM t WHERE d < '1995-03-15'");
    let ExprKind::Binary { right, .. } = &b.query.filter.as_ref().unwrap().kind else {
        panic!()
    };
    assert_eq!(
        right.kind,
        ExprKind::Literal(Value::Date(parse_date("1995-03-15").unwrap()))
    );
    assert!(bind_err("SELECT a FROM t WHERE d < '1995-02-30'")
        .contains("invalid input syntax for type date"));
}

#[test]
fn date_arithmetic() {
    let b = bind_ok("SELECT d + INTERVAL '1' YEAR, d - d, d + 7, -INTERVAL '3' DAY FROM t");
    let tys: Vec<Type> = b.query.outputs().iter().map(|o| o.ty()).collect();
    assert_eq!(tys, [Type::Date, Type::Int64, Type::Date, Type::Interval]);
}

#[test]
fn case_and_coalesce_unify_types() {
    let b = bind_ok("SELECT CASE WHEN a > 0 THEN b ELSE 0 END, coalesce(NULL, a, 1), CASE a WHEN 1 THEN 'x' END FROM t");
    let tys: Vec<Type> = b.query.outputs().iter().map(|o| o.ty()).collect();
    assert_eq!(tys, [Type::Float64, Type::Int64, Type::Utf8]);
}

#[test]
fn type_errors() {
    assert!(bind_err("SELECT s + 1 FROM t").contains("operator does not exist: VARCHAR + BIGINT"));
    assert!(bind_err("SELECT a FROM t WHERE a").contains("argument of WHERE must be type BOOLEAN"));
    assert!(
        bind_err("SELECT a FROM t WHERE s LIKE 5").contains("LIKE pattern must be type VARCHAR")
    );
    assert!(bind_err("SELECT sum(s) FROM t").contains("function sum(VARCHAR) does not exist"));
    assert!(
        bind_err("SELECT CAST(d AS INTEGER) FROM t").contains("cannot cast type DATE to BIGINT")
    );
    assert!(bind_err("SELECT CASE WHEN a > 0 THEN s ELSE d END FROM t").contains("CASE types"));
    assert!(bind_err("SELECT a IN (1, 'x') FROM t").contains("IN types"));
    assert!(bind_err("SELECT upper(1, 2) FROM t").contains("expects 1 argument"));
    assert!(bind_err("SELECT nope(a) FROM t").contains("function nope does not exist"));
}

// ---------------------------------------------------------------------------
// Name resolution
// ---------------------------------------------------------------------------

#[test]
fn name_resolution_errors() {
    assert_eq!(bind_err("SELECT x FROM t"), "column \"x\" does not exist");
    assert_eq!(
        bind_err("SELECT a FROM nope"),
        "table \"nope\" does not exist"
    );
    assert_eq!(
        bind_err("SELECT a FROM t, u"),
        "column reference \"a\" is ambiguous"
    );
    assert_eq!(
        bind_err("SELECT z.a FROM t"),
        "missing FROM-clause entry for table \"z\""
    );
    assert_eq!(bind_err("SELECT t.zz FROM t"), "column t.zz does not exist");
    assert_eq!(
        bind_err("SELECT 1 FROM t, t"),
        "table name \"t\" specified more than once"
    );
    // An alias hides the table name.
    assert_eq!(
        bind_err("SELECT t.a FROM t AS x"),
        "missing FROM-clause entry for table \"t\""
    );
}

#[test]
fn qualified_names_disambiguate() {
    let b = bind_ok("SELECT t.a, u.a, * FROM t JOIN u ON t.a = u.a");
    assert_eq!(b.query.outputs().len(), 2 + 4 + 2);
}

#[test]
fn derived_tables_cannot_see_sibling_from_items() {
    // There's no LATERAL: the derived table can't reference `t`.
    assert_eq!(
        bind_err("SELECT 1 FROM t, (SELECT t.a) AS x"),
        "missing FROM-clause entry for table \"t\""
    );
}

#[test]
fn subqueries_resolve_inner_scope_first() {
    // `a` inside the subquery resolves to u.a, so it is not correlated.
    let b = bind_ok("SELECT a FROM t WHERE a IN (SELECT a FROM u)");
    let ExprKind::Subquery { query, .. } = &b.query.filter.as_ref().unwrap().kind else {
        panic!()
    };
    assert!(query.correlated.is_empty());
}

#[test]
fn subquery_shape_errors() {
    assert_eq!(
        bind_err("SELECT (SELECT a, c FROM u) FROM t"),
        "subquery must return only one column"
    );
    assert!(bind_err("SELECT a FROM t WHERE s IN (SELECT a FROM u)")
        .contains("operator does not exist"));
}

// ---------------------------------------------------------------------------
// Aggregation rules
// ---------------------------------------------------------------------------

#[test]
fn grouped_expressions_may_be_reused_in_select() {
    bind_ok("SELECT a + 1, count(*) FROM t GROUP BY a + 1");
    bind_ok("SELECT a * 2, sum(b) / count(*) FROM t GROUP BY a");
    bind_ok("SELECT t.a FROM t GROUP BY a"); // `t.a` and `a` are the same column
}

#[test]
fn aggregation_errors() {
    assert!(bind_err("SELECT a, b FROM t GROUP BY a")
        .contains("\"t.b\" must appear in the GROUP BY clause"));
    assert!(bind_err("SELECT a, count(*) FROM t").contains("must appear in the GROUP BY clause"));
    assert_eq!(
        bind_err("SELECT a FROM t WHERE sum(a) > 1"),
        "aggregate functions are not allowed in WHERE"
    );
    assert_eq!(
        bind_err("SELECT a FROM t GROUP BY sum(a)"),
        "aggregate functions are not allowed in GROUP BY"
    );
    assert_eq!(
        bind_err("SELECT sum(count(*)) FROM t"),
        "aggregate function calls cannot be nested"
    );
    assert_eq!(bind_err("SELECT sum(*) FROM t"), "sum(*) is not valid");
    assert!(bind_err("SELECT upper(DISTINCT s) FROM t").contains("not an aggregate function"));
}

#[test]
fn identical_aggregates_are_computed_once() {
    let b = bind_ok("SELECT sum(b), sum(b) * 2, avg(b) FROM t HAVING sum(b) > 0");
    assert_eq!(b.query.aggregates.len(), 2);
}

#[test]
fn having_without_group_by_is_an_aggregate_query() {
    let b = bind_ok("SELECT count(*) FROM t HAVING count(*) > 1");
    assert!(b.query.is_aggregate());
    assert!(b.query.group_by.is_empty());
}

// ---------------------------------------------------------------------------
// ORDER BY and LIMIT
// ---------------------------------------------------------------------------

#[test]
fn order_by_alias_position_and_hidden_columns() {
    let b = bind_ok("SELECT a AS x, s FROM t ORDER BY x DESC, 2, b + 1");
    let q = &b.query;
    assert_eq!(q.visible, 2);
    assert_eq!(q.projection.len(), 3, "b + 1 becomes a hidden output");
    assert_eq!(q.order_by[0].column, q.projection[0].id);
    assert!(
        !q.order_by[0].asc && q.order_by[0].nulls_first,
        "DESC defaults to NULLS FIRST"
    );
    assert_eq!(q.order_by[1].column, q.projection[1].id);
    assert_eq!(q.order_by[2].column, q.projection[2].id);
}

#[test]
fn order_by_errors() {
    assert_eq!(
        bind_err("SELECT a FROM t ORDER BY 2"),
        "ORDER BY position 2 is not in select list"
    );
    assert!(bind_err("SELECT DISTINCT a FROM t ORDER BY b").contains("must appear in select list"));
    assert!(bind_err("SELECT a FROM t LIMIT a").contains("LIMIT must be a non-negative integer"));
}

#[test]
fn explain_runs_on_every_tpch_query() {
    for (_, sql) in tpch::QUERIES {
        let text = bind_ok(sql).explain();
        assert!(text.starts_with("Query"));
    }
}
