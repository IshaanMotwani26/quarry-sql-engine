use quarry::ast::*;
use quarry::{parse_query, parse_statements};

fn parse(sql: &str) -> Query {
    parse_query(sql).unwrap_or_else(|e| panic!("failed to parse {sql:?}: {e}"))
}

/// The WHERE clause of `SELECT * FROM t WHERE <expr>`.
fn expr(e: &str) -> Expr {
    parse(&format!("SELECT * FROM t WHERE {e}"))
        .selection
        .unwrap()
}

/// parse -> print -> parse must give back the identical AST.
fn assert_round_trip(sql: &str) {
    let first = parse(sql);
    let printed = first.to_string();
    let second = parse_query(&printed)
        .unwrap_or_else(|e| panic!("printed SQL did not reparse: {e}\n{printed}"));
    assert_eq!(
        first, second,
        "round trip changed the AST.\nprinted: {printed}"
    );
}

// ---------------------------------------------------------------------------
// Precedence and associativity
// ---------------------------------------------------------------------------

#[test]
fn multiplication_binds_tighter_than_addition() {
    use BinaryOp::*;
    assert_eq!(
        expr("1 + 2 * 3"),
        Expr::binary(
            Expr::num("1"),
            Plus,
            Expr::binary(Expr::num("2"), Multiply, Expr::num("3"))
        )
    );
}

#[test]
fn subtraction_is_left_associative() {
    use BinaryOp::*;
    assert_eq!(
        expr("a - b - c"),
        Expr::binary(
            Expr::binary(Expr::col("a"), Minus, Expr::col("b")),
            Minus,
            Expr::col("c")
        )
    );
}

#[test]
fn and_binds_tighter_than_or() {
    use BinaryOp::*;
    assert_eq!(
        expr("a OR b AND c"),
        Expr::binary(
            Expr::col("a"),
            Or,
            Expr::binary(Expr::col("b"), And, Expr::col("c"))
        )
    );
}

#[test]
fn not_applies_to_whole_comparison() {
    assert_eq!(
        expr("NOT a = 1"),
        Expr::Unary {
            op: UnaryOp::Not,
            expr: Box::new(Expr::binary(Expr::col("a"), BinaryOp::Eq, Expr::num("1"))),
        }
    );
}

#[test]
fn between_and_is_not_confused_with_logical_and() {
    let e = expr("x BETWEEN 1 AND 5 AND y = 2");
    let Expr::Binary {
        left,
        op: BinaryOp::And,
        ..
    } = e
    else {
        panic!("expected AND at top: {e:?}")
    };
    assert!(matches!(*left, Expr::Between { negated: false, .. }));
}

#[test]
fn parentheses_override_precedence() {
    use BinaryOp::*;
    assert_eq!(
        expr("(1 + 2) * 3"),
        Expr::binary(
            Expr::binary(Expr::num("1"), Plus, Expr::num("2")),
            Multiply,
            Expr::num("3")
        )
    );
}

// ---------------------------------------------------------------------------
// Predicates and special forms
// ---------------------------------------------------------------------------

#[test]
fn negated_predicates() {
    assert!(matches!(
        expr("a NOT LIKE 'x%'"),
        Expr::Like { negated: true, .. }
    ));
    assert!(matches!(
        expr("a NOT IN (1, 2)"),
        Expr::InList { negated: true, .. }
    ));
    assert!(matches!(
        expr("a IS NOT NULL"),
        Expr::IsNull { negated: true, .. }
    ));
    assert!(matches!(
        expr("NOT EXISTS (SELECT 1)"),
        Expr::Exists { negated: true, .. }
    ));
    assert!(matches!(
        expr("a IN (SELECT b FROM u)"),
        Expr::InSubquery { negated: false, .. }
    ));
}

#[test]
fn literals() {
    assert_eq!(
        expr("d = DATE '1998-12-01'"),
        Expr::binary(
            Expr::col("d"),
            BinaryOp::Eq,
            Expr::Literal(Literal::Date("1998-12-01".into())),
        )
    );
    assert!(matches!(
        expr("x < INTERVAL '90' DAY (3)"),
        Expr::Binary { right, .. } if *right == Expr::Literal(Literal::Interval {
            value: "90".into(), unit: DateTimeField::Day
        })
    ));
    assert_eq!(
        expr("s = 'it''s'"),
        Expr::binary(Expr::col("s"), BinaryOp::Eq, Expr::str("it's"))
    );
}

#[test]
fn date_is_still_usable_as_a_column_name() {
    assert_eq!(
        expr("date > 5"),
        Expr::binary(Expr::col("date"), BinaryOp::Gt, Expr::num("5"))
    );
}

#[test]
fn functions_case_cast_extract() {
    let q = parse(
        "SELECT count(*), count(DISTINCT x), CAST(y AS DECIMAL(15, 2)), \
         EXTRACT(YEAR FROM d), CASE WHEN a > 0 THEN 'pos' ELSE 'neg' END FROM t",
    );
    assert_eq!(q.projection.len(), 5);
    let SelectItem::Expr {
        expr: Expr::Function {
            args: FunctionArgs::Star,
            ..
        },
        ..
    } = &q.projection[0]
    else {
        panic!()
    };
    let SelectItem::Expr {
        expr: Expr::Function { distinct: true, .. },
        ..
    } = &q.projection[1]
    else {
        panic!()
    };
}

#[test]
fn substring_from_for_normalizes_to_function_args() {
    assert_eq!(
        expr("substring(p FROM 1 FOR 2) = '13'"),
        Expr::binary(
            Expr::Function {
                name: "substring".into(),
                args: FunctionArgs::List(vec![Expr::col("p"), Expr::num("1"), Expr::num("2")]),
                distinct: false,
            },
            BinaryOp::Eq,
            Expr::str("13"),
        )
    );
}

// ---------------------------------------------------------------------------
// Clauses
// ---------------------------------------------------------------------------

#[test]
fn full_select_shape() {
    let q = parse(
        "SELECT DISTINCT t.a AS x, b y, u.* FROM t \
         LEFT OUTER JOIN u ON t.id = u.id CROSS JOIN v \
         WHERE a > 1 GROUP BY a, b HAVING count(*) > 2 \
         ORDER BY x DESC NULLS LAST, y LIMIT 10 OFFSET 5",
    );
    assert!(q.distinct);
    assert!(matches!(&q.projection[1], SelectItem::Expr { alias: Some(a), .. } if a == "y"));
    assert_eq!(q.projection[2], SelectItem::QualifiedWildcard("u".into()));
    let joins = &q.from[0].joins;
    assert_eq!(joins.len(), 2);
    assert_eq!(joins[0].kind, JoinKind::Left);
    assert_eq!(joins[1].kind, JoinKind::Cross);
    assert!(joins[1].on.is_none());
    assert_eq!(q.group_by.len(), 2);
    assert!(!q.order_by[0].asc);
    assert_eq!(q.order_by[0].nulls_first, Some(false));
    assert_eq!(q.limit, Some(Expr::num("10")));
    assert_eq!(q.offset, Some(Expr::num("5")));
}

#[test]
fn identifiers_are_case_folded_unless_quoted() {
    let q = parse("SELECT Foo, \"Bar\" FROM T");
    assert_eq!(
        q.projection[0],
        SelectItem::Expr {
            expr: Expr::col("foo"),
            alias: None
        }
    );
    assert_eq!(
        q.projection[1],
        SelectItem::Expr {
            expr: Expr::col("Bar"),
            alias: None
        }
    );
}

#[test]
fn multiple_statements() {
    assert_eq!(parse_statements("SELECT 1; SELECT 2;;").unwrap().len(), 2);
}

// ---------------------------------------------------------------------------
// Errors carry useful positions
// ---------------------------------------------------------------------------

#[test]
fn error_messages_and_positions() {
    let e = parse_query("SELECT a FROM").unwrap_err();
    assert!(e.message.contains("expected identifier"), "{e}");

    let e = parse_query("SELECT a\nFROM t WHERE").unwrap_err();
    assert_eq!((e.span.line, e.span.col), (2, 13));
    assert!(e.message.contains("expected expression"), "{e}");

    let e = parse_query("SELECT (a + b FROM t").unwrap_err();
    assert!(e.message.contains("expected `)`"), "{e}");

    assert!(parse_query("SELECT a FROM t t2 t3").is_err());
    assert!(parse_query("SELECT CASE END").is_err());
    assert!(parse_query("SELECT CAST(a AS blob)").is_err());
}

// ---------------------------------------------------------------------------
// Round trips: printer output must reparse to the same AST
// ---------------------------------------------------------------------------

#[test]
fn round_trip_tricky_expressions() {
    for sql in [
        "SELECT a - (b - c), (a - b) - c, a / (b * c)",
        "SELECT -(-a), - -a, -(a + b), NOT (a OR b), NOT NOT a",
        "SELECT (a = b) = c, a = (b = c)",
        "SELECT a || b || c, (a + 1) || 'x'",
        "SELECT x BETWEEN (a AND b) AND c FROM t",
        "SELECT (a OR b) IS NULL, a LIKE (b || '%')",
        "SELECT \"Weird Name\", \"select\", \"a\"\"b\" FROM \"From\"",
        "SELECT CASE x WHEN 1 THEN 'a' WHEN 2 THEN 'b' END",
        "SELECT * FROM (SELECT 1) AS s (one) JOIN t ON TRUE ORDER BY 1 NULLS FIRST",
    ] {
        assert_round_trip(sql);
    }
}

const TPCH_Q1: &str = "
select l_returnflag, l_linestatus,
  sum(l_quantity) as sum_qty,
  sum(l_extendedprice) as sum_base_price,
  sum(l_extendedprice * (1 - l_discount)) as sum_disc_price,
  sum(l_extendedprice * (1 - l_discount) * (1 + l_tax)) as sum_charge,
  avg(l_quantity) as avg_qty,
  avg(l_extendedprice) as avg_price,
  avg(l_discount) as avg_disc,
  count(*) as count_order
from lineitem
where l_shipdate <= date '1998-12-01' - interval '90' day (3)
group by l_returnflag, l_linestatus
order by l_returnflag, l_linestatus;";

const TPCH_Q3: &str = "
select l_orderkey, sum(l_extendedprice * (1 - l_discount)) as revenue, o_orderdate, o_shippriority
from customer, orders, lineitem
where c_mktsegment = 'BUILDING'
  and c_custkey = o_custkey
  and l_orderkey = o_orderkey
  and o_orderdate < date '1995-03-15'
  and l_shipdate > date '1995-03-15'
group by l_orderkey, o_orderdate, o_shippriority
order by revenue desc, o_orderdate
limit 10;";

const TPCH_Q6: &str = "
select sum(l_extendedprice * l_discount) as revenue
from lineitem
where l_shipdate >= date '1994-01-01'
  and l_shipdate < date '1994-01-01' + interval '1' year
  and l_discount between 0.06 - 0.01 and 0.06 + 0.01
  and l_quantity < 24;";

const TPCH_Q8: &str = "
select o_year,
  sum(case when nation = 'BRAZIL' then volume else 0 end) / sum(volume) as mkt_share
from (
  select extract(year from o_orderdate) as o_year,
    l_extendedprice * (1 - l_discount) as volume,
    n2.n_name as nation
  from part, supplier, lineitem, orders, customer, nation n1, nation n2, region
  where p_partkey = l_partkey and s_suppkey = l_suppkey and l_orderkey = o_orderkey
    and o_custkey = c_custkey and c_nationkey = n1.n_nationkey
    and n1.n_regionkey = r_regionkey and r_name = 'AMERICA'
    and s_nationkey = n2.n_nationkey
    and o_orderdate between date '1995-01-01' and date '1996-12-31'
    and p_type = 'ECONOMY ANODIZED STEEL'
) as all_nations
group by o_year
order by o_year;";

const TPCH_Q13: &str = "
select c_count, count(*) as custdist
from (
  select c_custkey, count(o_orderkey)
  from customer left outer join orders
    on c_custkey = o_custkey and o_comment not like '%special%requests%'
  group by c_custkey
) as c_orders (c_custkey, c_count)
group by c_count
order by custdist desc, c_count desc;";

const TPCH_Q22: &str = "
select cntrycode, count(*) as numcust, sum(c_acctbal) as totacctbal
from (
  select substring(c_phone from 1 for 2) as cntrycode, c_acctbal
  from customer
  where substring(c_phone from 1 for 2) in ('13', '31', '23', '29', '30', '18', '17')
    and c_acctbal > (
      select avg(c_acctbal) from customer
      where c_acctbal > 0.00
        and substring(c_phone from 1 for 2) in ('13', '31', '23', '29', '30', '18', '17')
    )
    and not exists (select * from orders where o_custkey = c_custkey)
) as custsale
group by cntrycode
order by cntrycode;";

#[test]
fn tpch_queries_parse_and_round_trip() {
    for sql in [TPCH_Q1, TPCH_Q3, TPCH_Q6, TPCH_Q8, TPCH_Q13, TPCH_Q22] {
        assert_round_trip(sql);
    }
}

// ---------------------------------------------------------------------------
// Robustness: random token soup must produce Ok or Err, never a panic
// ---------------------------------------------------------------------------

#[test]
fn random_inputs_never_panic() {
    const VOCAB: &[&str] = &[
        "SELECT",
        "FROM",
        "WHERE",
        "AND",
        "OR",
        "NOT",
        "IN",
        "BETWEEN",
        "LIKE",
        "IS",
        "NULL",
        "CASE",
        "WHEN",
        "THEN",
        "ELSE",
        "END",
        "JOIN",
        "LEFT",
        "ON",
        "GROUP",
        "BY",
        "ORDER",
        "EXISTS",
        "CAST",
        "AS",
        "EXTRACT",
        "date",
        "interval",
        "'x'",
        "1",
        "2.5",
        "a",
        "t",
        "(",
        ")",
        ",",
        ".",
        "*",
        "+",
        "-",
        "/",
        "=",
        "<",
        "||",
        ";",
        "\"q\"",
        "count",
        "substring",
        "year",
        "LIMIT",
        "DESC",
        "nulls",
    ];
    // Small xorshift PRNG: deterministic, no dependencies.
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut rand = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..20_000 {
        let len = (rand() % 25) as usize;
        let sql: Vec<&str> = (0..len)
            .map(|_| VOCAB[(rand() as usize) % VOCAB.len()])
            .collect();
        let sql = sql.join(" ");
        // Anything that parses must also survive a round trip.
        if let Ok(stmts) = parse_statements(&sql) {
            for s in stmts {
                let printed = s.to_string();
                let again = parse_statements(&printed).unwrap_or_else(|e| {
                    panic!("{sql:?} printed as {printed:?}, which failed: {e}")
                });
                assert_eq!(again, vec![s], "round trip mismatch for {sql:?}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Property test: random ASTs -> print -> parse must be the identity.
// Token soup above rarely forms valid SQL; generating trees directly
// stress-tests the printer's parenthesization against the parser's precedence.
// ---------------------------------------------------------------------------

struct Gen(u64);

impl Gen {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }

    fn leaf(&mut self) -> Expr {
        match self.below(7) {
            0 => Expr::num(self.pick::<&str>(&["0", "1", "42", "3.14", "1e10"])),
            1 => Expr::str(self.pick::<&str>(&["", "x", "it's", "%a%"])),
            2 => Expr::Literal(Literal::Null),
            3 => Expr::Literal(Literal::Boolean(self.below(2) == 0)),
            4 => Expr::Literal(Literal::Date("1995-03-15".into())),
            5 => Expr::Column {
                table: Some("t".into()),
                name: "Mixed Case".into(),
            },
            _ => Expr::col(self.pick::<&str>(&["a", "b", "date", "select", "c_1"])),
        }
    }

    fn expr(&mut self, depth: u32) -> Expr {
        if depth == 0 {
            return self.leaf();
        }
        let d = depth - 1;
        let b = |e: Expr| Box::new(e);
        match self.below(12) {
            0..=2 => {
                use BinaryOp::*;
                let op = *self.pick(&[
                    Or, And, Eq, NotEq, Lt, LtEq, Gt, GtEq, Concat, Plus, Minus, Multiply, Divide,
                    Modulo,
                ]);
                Expr::Binary {
                    left: b(self.expr(d)),
                    op,
                    right: b(self.expr(d)),
                }
            }
            3 => {
                let op = *self.pick(&[UnaryOp::Not, UnaryOp::Minus, UnaryOp::Plus]);
                let mut inner = self.expr(d);
                // `NOT EXISTS` canonically parses as Exists { negated: true }.
                if op == UnaryOp::Not && matches!(inner, Expr::Exists { .. }) {
                    inner = self.leaf();
                }
                Expr::Unary { op, expr: b(inner) }
            }
            4 => Expr::IsNull {
                expr: b(self.expr(d)),
                negated: self.below(2) == 0,
            },
            5 => Expr::Like {
                expr: b(self.expr(d)),
                pattern: b(self.expr(d)),
                negated: self.below(2) == 0,
            },
            6 => Expr::Between {
                expr: b(self.expr(d)),
                low: b(self.expr(d)),
                high: b(self.expr(d)),
                negated: self.below(2) == 0,
            },
            7 => Expr::InList {
                expr: b(self.expr(d)),
                list: (0..1 + self.below(3)).map(|_| self.expr(d)).collect(),
                negated: self.below(2) == 0,
            },
            8 => Expr::Function {
                name: self.pick(&["sum", "f", "Upper"]).to_string(),
                args: FunctionArgs::List((0..self.below(3)).map(|_| self.expr(d)).collect()),
                distinct: false,
            },
            9 => Expr::Case {
                operand: if self.below(2) == 0 {
                    Some(b(self.expr(d)))
                } else {
                    None
                },
                branches: vec![(self.expr(d), self.expr(d))],
                else_result: if self.below(2) == 0 {
                    Some(b(self.expr(d)))
                } else {
                    None
                },
            },
            10 => Expr::Cast {
                expr: b(self.expr(d)),
                data_type: DataType::Decimal {
                    precision: Some(15),
                    scale: Some(2),
                },
            },
            _ => {
                let mut q = parse("SELECT a FROM t");
                q.selection = Some(self.expr(d));
                if self.below(2) == 0 {
                    Expr::Exists {
                        subquery: Box::new(q),
                        negated: self.below(2) == 0,
                    }
                } else {
                    Expr::Subquery(Box::new(q))
                }
            }
        }
    }
}

#[test]
fn random_ast_round_trip() {
    let mut g = Gen(0xDEAD_BEEF_CAFE_F00D);
    for _ in 0..20_000 {
        let depth = 1 + g.below(5) as u32;
        let mut q = parse("SELECT a FROM t");
        q.projection = vec![SelectItem::Expr {
            expr: g.expr(depth),
            alias: None,
        }];
        q.selection = Some(g.expr(depth));
        let printed = q.to_string();
        let reparsed = parse_query(&printed)
            .unwrap_or_else(|e| panic!("generated SQL failed to parse: {e}\n{printed}"));
        assert_eq!(q, reparsed, "round trip mismatch:\n{printed}");
    }
}
