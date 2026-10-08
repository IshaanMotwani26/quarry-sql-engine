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
fn comma_joins_become_left_deep_cross_joins() {
    let b = bound(tpch::Q8);
    let mut node = plan_query(&b.query);
    while !matches!(node, Plan::Join { .. }) {
        node = node.children()[0].clone();
    }
    let mut depth = 0;
    while let Plan::Join { left, kind, .. } = node {
        assert_eq!(kind, JoinKind::Cross);
        depth += 1;
        node = *left;
    }
    assert_eq!(depth, 7, "8 tables in FROM need 7 joins");
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
