use quarry::ast::JoinKind;
use quarry::bound::Bound;
use quarry::catalog::{Catalog, Schema, Table};
use quarry::types::Type;
use quarry::{bind, parse_query, plan_query, tpch, Plan};

fn catalog() -> Catalog {
    let mut c = Catalog::new();
    tpch::register_schemas(&mut c);
    let t = Table::new(
        "t",
        Schema::from_pairs(&[("a", Type::Int64), ("b", Type::Float64), ("s", Type::Utf8)]),
    )
    .unwrap();
    c.register(t).unwrap();
    c
}

fn bound(sql: &str) -> Bound {
    let q = parse_query(sql).unwrap_or_else(|e| panic!("parse failed for {sql:?}: {e}"));
    bind(&catalog(), &q).unwrap_or_else(|e| panic!("bind failed for {sql:?}: {e}"))
}

/// The plan's node names from the root down, following the first child.
fn spine(plan: &Plan) -> Vec<&'static str> {
    let mut out = Vec::new();
    let mut node = plan;
    loop {
        out.push(match node {
            Plan::SingleRow => "SingleRow",
            Plan::Scan { .. } => "Scan",
            Plan::Filter { .. } => "Filter",
            Plan::Project { .. } => "Project",
            Plan::Join { .. } => "Join",
            Plan::Aggregate { .. } => "Aggregate",
            Plan::Distinct { .. } => "Distinct",
            Plan::Sort { .. } => "Sort",
            Plan::Limit { .. } => "Limit",
        });
        match node.children().first() {
            Some(child) => node = child,
            None => return out,
        }
    }
}

#[test]
fn plan_outputs_match_query_outputs_for_every_tpch_query() {
    for (n, sql) in tpch::QUERIES {
        let b = bound(sql);
        let plan = plan_query(&b.query);
        let expected: Vec<_> = b.query.outputs().iter().map(|o| o.id).collect();
        assert_eq!(plan.output(), expected, "TPC-H Q{n}");
        assert!(!plan.explain(&b.columns).is_empty());
    }
}

#[test]
fn clause_order_follows_sql_semantics() {
    let b = bound(tpch::Q3);
    assert_eq!(
        spine(&plan_query(&b.query)),
        [
            "Limit",
            "Sort",
            "Project",
            "Aggregate",
            "Filter",
            "Join",
            "Join",
            "Scan"
        ]
    );
    let b = bound("SELECT DISTINCT a FROM t WHERE a > 1 ORDER BY a LIMIT 5 OFFSET 2");
    let plan = plan_query(&b.query);
    assert_eq!(
        spine(&plan),
        ["Limit", "Sort", "Distinct", "Project", "Filter", "Scan"]
    );
    let Plan::Limit { limit, offset, .. } = plan else {
        panic!()
    };
    assert_eq!((limit, offset), (Some(5), 2));
}

#[test]
fn having_filters_above_the_aggregate() {
    let b = bound("SELECT a, count(*) FROM t GROUP BY a HAVING count(*) > 1");
    assert_eq!(
        spine(&plan_query(&b.query)),
        ["Project", "Filter", "Aggregate", "Scan"]
    );
}

#[test]
fn hidden_order_by_columns_are_dropped_after_sorting() {
    let b = bound("SELECT a FROM t ORDER BY b");
    let plan = plan_query(&b.query);
    assert_eq!(spine(&plan), ["Project", "Sort", "Project", "Scan"]);
    assert_eq!(plan.output().len(), 1);
    // The inner projection carries the hidden sort key.
    let Plan::Project { input, .. } = &plan else {
        panic!()
    };
    assert_eq!(input.output().len(), 2);
}

#[test]
fn no_from_clause_reads_a_single_row() {
    let b = bound("SELECT 1 + 1");
    assert_eq!(spine(&plan_query(&b.query)), ["Project", "SingleRow"]);
}

#[test]
fn derived_tables_are_renamed_to_outer_ids() {
    let b = bound(tpch::Q13);
    let plan = plan_query(&b.query);
    // Sort -> Project -> Aggregate -> Project (rename) -> Project (inner) -> Aggregate -> Join
    assert_eq!(
        spine(&plan),
        [
            "Sort",
            "Project",
            "Aggregate",
            "Project",
            "Project",
            "Aggregate",
            "Join",
            "Scan"
        ]
    );
    let mut join = &plan;
    while !matches!(join, Plan::Join { .. }) {
        join = join.children()[0];
    }
    let Plan::Join { kind, on, .. } = join else {
        unreachable!()
    };
    assert_eq!(*kind, JoinKind::Left);
    assert!(on.is_some());
}

#[test]
fn comma_join_conditions_move_from_where_into_the_joins() {
    let b = bound(tpch::Q8);
    let mut node = plan_query(&b.query);
    while !matches!(node, Plan::Join { .. }) {
        node = node.children()[0].clone();
    }
    // Walk the left-deep chain from the top join down.
    let mut kinds = Vec::new();
    while let Plan::Join { left, kind, on, .. } = node {
        kinds.push((kind, on.is_some()));
        node = *left;
    }
    assert_eq!(kinds.len(), 7, "8 tables in FROM need 7 joins");
    // Every join gets a condition except the bottom one: part and supplier
    // share no condition, so they remain a cross product until join
    // reordering (Phase 5b) puts lineitem between them.
    assert_eq!(kinds[6], (JoinKind::Cross, false));
    assert!(
        kinds[..6].iter().all(|&k| k == (JoinKind::Inner, true)),
        "{kinds:?}"
    );
}

#[test]
fn single_table_and_subquery_conditions_stay_in_where() {
    let b = bound(
        "SELECT 1 FROM t AS x, t AS y WHERE x.a = y.a AND x.b > 1 AND y.s IN (SELECT s FROM t)",
    );
    let plan = plan_query(&b.query);
    let Plan::Project { input, .. } = &plan else {
        panic!()
    };
    let Plan::Filter {
        input: join,
        predicate,
    } = &**input
    else {
        panic!("expected a Filter: {plan:?}")
    };
    let text = plan.explain(&b.columns);
    assert!(text.contains("HashJoin Inner keys=[x.a = y.a]"), "{text}");
    // x.b > 1 and the IN subquery remain in the filter above the join.
    assert!(matches!(
        predicate.kind,
        quarry::bound::ExprKind::Binary { .. }
    ));
    assert!(
        text.contains("Filter ((x.b > 1) AND y.s IN $sub0)"),
        "{text}"
    );
    assert!(matches!(
        **join,
        Plan::Join {
            kind: JoinKind::Inner,
            ..
        }
    ));
}

#[test]
fn conditions_are_not_moved_into_outer_joins() {
    // Moving `y.a = 1` into the LEFT JOIN would keep x rows it should drop.
    let b = bound("SELECT 1 FROM t AS x LEFT JOIN t AS y ON x.a = y.a WHERE y.a = 1 OR x.a = y.b");
    let text = plan_query(&b.query).explain(&b.columns);
    assert!(
        text.contains("Filter ((y.a = 1) OR (CAST(x.a AS DOUBLE) = y.b))"),
        "{text}"
    );
    assert!(text.contains("HashJoin Left keys=[x.a = y.a]"), "{text}");
}

#[test]
fn non_equality_joins_use_a_nested_loop() {
    let b = bound("SELECT 1 FROM t AS x JOIN t AS y ON x.a < y.a");
    let text = plan_query(&b.query).explain(&b.columns);
    assert!(
        text.contains("NestedLoopJoin Inner on (x.a < y.a)"),
        "{text}"
    );
}

#[test]
fn equality_with_expressions_and_residuals() {
    let b =
        bound("SELECT 1 FROM t AS x JOIN t AS y ON y.a + 1 = x.a * 2 AND x.s <> y.s AND x.b = 3");
    let text = plan_query(&b.query).explain(&b.columns);
    // The key is swapped so the left input's expression comes first.
    assert!(
        text.contains(
            "HashJoin Inner keys=[(x.a * 2) = (y.a + 1)] residual=[(x.s <> y.s), (x.b = 3)]"
        ),
        "{text}"
    );
}

#[test]
fn explain_shows_correlated_subqueries() {
    let b = bound(tpch::Q22);
    let text = plan_query(&b.query).explain(&b.columns);
    assert!(text.contains("NOT EXISTS $sub1"), "{text}");
    assert!(text.contains("$sub1 (correlated):"), "{text}");
    assert!(
        text.contains("Aggregate keys=[] aggs=[avg(customer.c_acctbal)]"),
        "{text}"
    );
}
