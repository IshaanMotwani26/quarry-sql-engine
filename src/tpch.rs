//! TPC-H schemas and the benchmark queries Quarry supports so far.
//! DECIMAL columns are DOUBLE and CHAR/VARCHAR columns are VARCHAR (see types.rs).

use crate::catalog::{Catalog, Schema, Table};
use crate::types::Type::{self, Date, Float64 as Decimal, Int64 as Int, Utf8 as Text};

type Columns = &'static [(&'static str, Type)];

pub const TABLES: &[(&str, Columns)] = &[
    (
        "part",
        &[
            ("p_partkey", Int),
            ("p_name", Text),
            ("p_mfgr", Text),
            ("p_brand", Text),
            ("p_type", Text),
            ("p_size", Int),
            ("p_container", Text),
            ("p_retailprice", Decimal),
            ("p_comment", Text),
        ],
    ),
    (
        "supplier",
        &[
            ("s_suppkey", Int),
            ("s_name", Text),
            ("s_address", Text),
            ("s_nationkey", Int),
            ("s_phone", Text),
            ("s_acctbal", Decimal),
            ("s_comment", Text),
        ],
    ),
    (
        "partsupp",
        &[
            ("ps_partkey", Int),
            ("ps_suppkey", Int),
            ("ps_availqty", Int),
            ("ps_supplycost", Decimal),
            ("ps_comment", Text),
        ],
    ),
    (
        "customer",
        &[
            ("c_custkey", Int),
            ("c_name", Text),
            ("c_address", Text),
            ("c_nationkey", Int),
            ("c_phone", Text),
            ("c_acctbal", Decimal),
            ("c_mktsegment", Text),
            ("c_comment", Text),
        ],
    ),
    (
        "orders",
        &[
            ("o_orderkey", Int),
            ("o_custkey", Int),
            ("o_orderstatus", Text),
            ("o_totalprice", Decimal),
            ("o_orderdate", Date),
            ("o_orderpriority", Text),
            ("o_clerk", Text),
            ("o_shippriority", Int),
            ("o_comment", Text),
        ],
    ),
    (
        "lineitem",
        &[
            ("l_orderkey", Int),
            ("l_partkey", Int),
            ("l_suppkey", Int),
            ("l_linenumber", Int),
            ("l_quantity", Decimal),
            ("l_extendedprice", Decimal),
            ("l_discount", Decimal),
            ("l_tax", Decimal),
            ("l_returnflag", Text),
            ("l_linestatus", Text),
            ("l_shipdate", Date),
            ("l_commitdate", Date),
            ("l_receiptdate", Date),
            ("l_shipinstruct", Text),
            ("l_shipmode", Text),
            ("l_comment", Text),
        ],
    ),
    (
        "nation",
        &[
            ("n_nationkey", Int),
            ("n_name", Text),
            ("n_regionkey", Int),
            ("n_comment", Text),
        ],
    ),
    (
        "region",
        &[("r_regionkey", Int), ("r_name", Text), ("r_comment", Text)],
    ),
];

pub fn schema(table: &str) -> Option<Schema> {
    TABLES
        .iter()
        .find(|(name, _)| *name == table)
        .map(|(_, cols)| Schema::from_pairs(cols))
}

/// Registers all eight TPC-H tables with no rows (replacing any existing ones).
pub fn register_schemas(catalog: &mut Catalog) {
    for (name, cols) in TABLES {
        let table =
            Table::new(*name, Schema::from_pairs(cols)).expect("TPC-H types are all storable");
        catalog.register_or_replace(table);
    }
}

pub const Q1: &str = "
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
order by l_returnflag, l_linestatus";

pub const Q3: &str = "
select l_orderkey, sum(l_extendedprice * (1 - l_discount)) as revenue, o_orderdate, o_shippriority
from customer, orders, lineitem
where c_mktsegment = 'BUILDING'
  and c_custkey = o_custkey
  and l_orderkey = o_orderkey
  and o_orderdate < date '1995-03-15'
  and l_shipdate > date '1995-03-15'
group by l_orderkey, o_orderdate, o_shippriority
order by revenue desc, o_orderdate
limit 10";

pub const Q6: &str = "
select sum(l_extendedprice * l_discount) as revenue
from lineitem
where l_shipdate >= date '1994-01-01'
  and l_shipdate < date '1994-01-01' + interval '1' year
  and l_discount between 0.06 - 0.01 and 0.06 + 0.01
  and l_quantity < 24";

pub const Q8: &str = "
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
order by o_year";

pub const Q13: &str = "
select c_count, count(*) as custdist
from (
  select c_custkey, count(o_orderkey)
  from customer left outer join orders
    on c_custkey = o_custkey and o_comment not like '%special%requests%'
  group by c_custkey
) as c_orders (c_custkey, c_count)
group by c_count
order by custdist desc, c_count desc";

pub const Q22: &str = "
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
order by cntrycode";

pub const QUERIES: &[(u32, &str)] = &[(1, Q1), (3, Q3), (6, Q6), (8, Q8), (13, Q13), (22, Q22)];
