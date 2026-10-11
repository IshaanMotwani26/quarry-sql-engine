//! TPC-H schemas and the benchmark queries Quarry supports so far.
//! DECIMAL columns are DOUBLE and CHAR/VARCHAR columns are VARCHAR (see types.rs).

use crate::catalog::{Catalog, Schema, Table};
use crate::types::Type::{self, Date, Float64 as Decimal, Int64 as Int, Utf8 as Text};
use crate::types::{parse_date, Value};

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

// ---------------------------------------------------------------------------
// Data generation
// ---------------------------------------------------------------------------
//
// A simplified, deterministic version of the official `dbgen` tool. It keeps
// the parts of the spec that the queries depend on:
//
// - Table sizes scale with `scale` as in the spec (SF 1 = 6M lineitem rows).
// - Keys and foreign keys are consistent: every order belongs to a customer,
//   every lineitem's (partkey, suppkey) pair exists in partsupp.
// - Only customers whose key isn't divisible by 3 place orders, so a third
//   of customers have none (Q13's zero bucket, Q22's NOT EXISTS).
// - Real nation and region names, the spec's part-type syllables, phone
//   numbers prefixed with nation key + 10, and dates and flags derived the
//   way the spec derives them.
//
// Text fields (names, addresses, comments) are shorter and less varied than
// dbgen's, and orderkeys are dense rather than sparse. Results therefore
// won't match the official answer set; Phase 8 loads real dbgen output.

const REGIONS: [&str; 5] = ["AFRICA", "AMERICA", "ASIA", "EUROPE", "MIDDLE EAST"];

const NATIONS: [(&str, i64); 25] = [
    ("ALGERIA", 0),
    ("ARGENTINA", 1),
    ("BRAZIL", 1),
    ("CANADA", 1),
    ("EGYPT", 4),
    ("ETHIOPIA", 0),
    ("FRANCE", 3),
    ("GERMANY", 3),
    ("INDIA", 2),
    ("INDONESIA", 2),
    ("IRAN", 4),
    ("IRAQ", 4),
    ("JAPAN", 2),
    ("JORDAN", 4),
    ("KENYA", 0),
    ("MOROCCO", 0),
    ("MOZAMBIQUE", 0),
    ("PERU", 1),
    ("CHINA", 2),
    ("ROMANIA", 3),
    ("SAUDI ARABIA", 4),
    ("VIETNAM", 2),
    ("RUSSIA", 3),
    ("UNITED KINGDOM", 3),
    ("UNITED STATES", 1),
];

const TYPE_1: [&str; 6] = ["STANDARD", "SMALL", "MEDIUM", "LARGE", "ECONOMY", "PROMO"];
const TYPE_2: [&str; 5] = ["ANODIZED", "BURNISHED", "PLATED", "POLISHED", "BRUSHED"];
const TYPE_3: [&str; 5] = ["TIN", "NICKEL", "BRASS", "STEEL", "COPPER"];
const CONTAINER_1: [&str; 5] = ["SM", "LG", "MED", "JUMBO", "WRAP"];
const CONTAINER_2: [&str; 8] = ["CASE", "BOX", "BAG", "JAR", "PKG", "PACK", "CAN", "DRUM"];
const SEGMENTS: [&str; 5] = [
    "AUTOMOBILE",
    "BUILDING",
    "FURNITURE",
    "MACHINERY",
    "HOUSEHOLD",
];
const PRIORITIES: [&str; 5] = ["1-URGENT", "2-HIGH", "3-MEDIUM", "4-NOT SPECIFIED", "5-LOW"];
const INSTRUCTIONS: [&str; 4] = [
    "DELIVER IN PERSON",
    "COLLECT COD",
    "NONE",
    "TAKE BACK RETURN",
];
const MODES: [&str; 7] = ["REG AIR", "AIR", "RAIL", "SHIP", "TRUCK", "MAIL", "FOB"];
const COLORS: [&str; 12] = [
    "almond", "azure", "blush", "chiffon", "coral", "forest", "ivory", "lavender", "linen",
    "olive", "peach", "smoke",
];
const WORDS: [&str; 16] = [
    "carefully",
    "final",
    "deposits",
    "furiously",
    "regular",
    "accounts",
    "quickly",
    "ironic",
    "packages",
    "blithely",
    "pending",
    "theodolites",
    "bold",
    "foxes",
    "slyly",
    "express",
];

/// xorshift64: small, fast, deterministic.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// Uniform integer in `lo..=hi`.
    fn int(&mut self, lo: i64, hi: i64) -> i64 {
        lo + (self.next() % (hi - lo + 1) as u64) as i64
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.int(0, items.len() as i64 - 1) as usize]
    }

    /// A decimal with two places in `lo..=hi`, e.g. money(-999.99, 9999.99).
    fn money(&mut self, lo: f64, hi: f64) -> f64 {
        self.int((lo * 100.0).round() as i64, (hi * 100.0).round() as i64) as f64 / 100.0
    }

    fn comment(&mut self, words: usize) -> String {
        (0..words)
            .map(|_| self.pick(&WORDS))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn phone(&mut self, nation: i64) -> String {
        format!(
            "{}-{}-{}-{}",
            nation + 10,
            self.int(100, 999),
            self.int(100, 999),
            self.int(1000, 9999)
        )
    }
}

fn text(s: impl Into<String>) -> Value {
    Value::Utf8(s.into())
}

/// Row counts for a scale factor (every table gets at least one row).
/// partsupp has `suppliers_per_part` (normally 4) rows per part; lineitem
/// has 1 to 7 per order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sizes {
    pub supplier: i64,
    pub part: i64,
    pub customer: i64,
    pub orders: i64,
}

impl Sizes {
    pub fn at(scale: f64) -> Sizes {
        let n = |base: f64| ((base * scale).round() as i64).max(1);
        Sizes {
            supplier: n(10_000.0),
            part: n(200_000.0),
            customer: n(150_000.0),
            orders: n(1_500_000.0),
        }
    }
}

/// Suppliers per part: 4 by the spec, fewer only when there are fewer than
/// 4 suppliers in total (scale < 0.0004).
pub fn suppliers_per_part(suppliers: i64) -> i64 {
    suppliers.min(4)
}

/// The i-th supplier of a part (`i < suppliers_per_part`), distinct for each i.
///
/// The spec's formula steps through suppliers in strides of
/// `suppliers / 4 + (partkey - 1) / suppliers`. That stride can wrap around and
/// repeat a supplier unless `3 * stride < suppliers`, which only holds above
/// roughly 228 suppliers (scale 0.023). Below that, consecutive suppliers are
/// used instead, so (partkey, suppkey) stays a valid primary key at any scale.
fn partsupp_supplier(partkey: i64, i: i64, suppliers: i64) -> i64 {
    let stride = suppliers / 4 + (partkey - 1) / suppliers;
    if 3 * stride < suppliers {
        (partkey + i * stride) % suppliers + 1
    } else {
        (partkey + i) % suppliers + 1
    }
}

/// The spec's retail price formula, which makes prices depend on the key.
fn retail_price(partkey: i64) -> f64 {
    (90_000 + (partkey / 10) % 20_001 + 100 * (partkey % 1000)) as f64 / 100.0
}

fn new_table(name: &str) -> Table {
    Table::new(name, schema(name).expect("known TPC-H table")).expect("TPC-H types are storable")
}

fn push(table: &mut Table, row: Vec<Value>) {
    table
        .push_row(row)
        .expect("generated rows match the schema");
}

/// Generates all eight TPC-H tables at `scale` (e.g. 0.01 is 60k lineitem
/// rows) and registers them, replacing existing tables with the same names.
pub fn generate(catalog: &mut Catalog, scale: f64) {
    let mut rng = Rng(0x7C9_2026_1009_ABCD ^ scale.to_bits());
    let Sizes {
        supplier: suppliers,
        part: parts,
        customer: customers,
        orders,
    } = Sizes::at(scale);

    let mut region = new_table("region");
    for (key, name) in REGIONS.iter().enumerate() {
        push(
            &mut region,
            vec![Value::Int64(key as i64), text(*name), text(rng.comment(6))],
        );
    }

    let mut nation = new_table("nation");
    for (key, (name, region_key)) in NATIONS.iter().enumerate() {
        push(
            &mut nation,
            vec![
                Value::Int64(key as i64),
                text(*name),
                Value::Int64(*region_key),
                text(rng.comment(8)),
            ],
        );
    }

    let mut supplier = new_table("supplier");
    for key in 1..=suppliers {
        let nation_key = rng.int(0, 24);
        push(
            &mut supplier,
            vec![
                Value::Int64(key),
                text(format!("Supplier#{key:09}")),
                text(rng.comment(2)),
                Value::Int64(nation_key),
                text(rng.phone(nation_key)),
                Value::Float64(rng.money(-999.99, 9999.99)),
                text(rng.comment(6)),
            ],
        );
    }

    let mut part = new_table("part");
    let mut partsupp = new_table("partsupp");
    for key in 1..=parts {
        let m = rng.int(1, 5);
        let name: Vec<&str> = (0..5).map(|_| rng.pick(&COLORS)).collect();
        push(
            &mut part,
            vec![
                Value::Int64(key),
                text(name.join(" ")),
                text(format!("Manufacturer#{m}")),
                text(format!("Brand#{m}{}", rng.int(1, 5))),
                text(format!(
                    "{} {} {}",
                    rng.pick(&TYPE_1),
                    rng.pick(&TYPE_2),
                    rng.pick(&TYPE_3)
                )),
                Value::Int64(rng.int(1, 50)),
                text(format!(
                    "{} {}",
                    rng.pick(&CONTAINER_1),
                    rng.pick(&CONTAINER_2)
                )),
                Value::Float64(retail_price(key)),
                text(rng.comment(3)),
            ],
        );
        for i in 0..suppliers_per_part(suppliers) {
            push(
                &mut partsupp,
                vec![
                    Value::Int64(key),
                    Value::Int64(partsupp_supplier(key, i, suppliers)),
                    Value::Int64(rng.int(1, 9999)),
                    Value::Float64(rng.money(1.0, 1000.0)),
                    text(rng.comment(8)),
                ],
            );
        }
    }

    let mut customer = new_table("customer");
    for key in 1..=customers {
        let nation_key = rng.int(0, 24);
        push(
            &mut customer,
            vec![
                Value::Int64(key),
                text(format!("Customer#{key:09}")),
                text(rng.comment(2)),
                Value::Int64(nation_key),
                text(rng.phone(nation_key)),
                Value::Float64(rng.money(-999.99, 9999.99)),
                text(rng.pick(&SEGMENTS)),
                text(rng.comment(8)),
            ],
        );
    }

    let mut order_table = new_table("orders");
    let mut lineitem = new_table("lineitem");
    let start = parse_date("1992-01-01").expect("valid date");
    let end = parse_date("1998-08-02").expect("valid date"); // 1998-12-31 minus 151 days
    let current = parse_date("1995-06-17").expect("valid date");
    // Ordering customers have keys not divisible by 3: map 0, 1, 2, 3, ... to 1, 2, 4, 5, ...
    let ordering_customers = (customers - customers / 3).max(1);
    for order_key in 1..=orders {
        let k = rng.int(0, ordering_customers - 1);
        let cust_key = (k / 2 * 3 + k % 2 + 1).min(customers);
        let order_date = rng.int(start as i64, end as i64) as i32;
        let mut total = 0.0;
        let (mut all_f, mut all_o) = (true, true);
        for line in 1..=rng.int(1, 7) {
            let part_key = rng.int(1, parts);
            let supp_key = partsupp_supplier(
                part_key,
                rng.int(0, suppliers_per_part(suppliers) - 1),
                suppliers,
            );
            let quantity = rng.int(1, 50) as f64;
            let price = quantity * retail_price(part_key);
            let discount = rng.int(0, 10) as f64 / 100.0;
            let tax = rng.int(0, 8) as f64 / 100.0;
            let ship = order_date + rng.int(1, 121) as i32;
            let commit = order_date + rng.int(30, 90) as i32;
            let receipt = ship + rng.int(1, 30) as i32;
            let returnflag = if receipt <= current {
                rng.pick(&["R", "A"])
            } else {
                "N"
            };
            let linestatus = if ship > current { "O" } else { "F" };
            all_f &= linestatus == "F";
            all_o &= linestatus == "O";
            total += price * (1.0 + tax) * (1.0 - discount);
            push(
                &mut lineitem,
                vec![
                    Value::Int64(order_key),
                    Value::Int64(part_key),
                    Value::Int64(supp_key),
                    Value::Int64(line),
                    Value::Float64(quantity),
                    Value::Float64(price),
                    Value::Float64(discount),
                    Value::Float64(tax),
                    text(returnflag),
                    text(linestatus),
                    Value::Date(ship),
                    Value::Date(commit),
                    Value::Date(receipt),
                    text(rng.pick(&INSTRUCTIONS)),
                    text(rng.pick(&MODES)),
                    text(rng.comment(4)),
                ],
            );
        }
        let status = if all_f {
            "F"
        } else if all_o {
            "O"
        } else {
            "P"
        };
        // About 1 in 50 order comments mention special requests (Q13 excludes them).
        let comment = if rng.int(0, 49) == 0 {
            format!("{} special {} requests", rng.pick(&WORDS), rng.pick(&WORDS))
        } else {
            rng.comment(5)
        };
        push(
            &mut order_table,
            vec![
                Value::Int64(order_key),
                Value::Int64(cust_key),
                text(status),
                Value::Float64((total * 100.0).round() / 100.0),
                Value::Date(order_date),
                text(rng.pick(&PRIORITIES)),
                text(format!(
                    "Clerk#{:09}",
                    rng.int(1, (1000.0 * scale).ceil() as i64)
                )),
                Value::Int64(0),
                text(comment),
            ],
        );
    }

    for table in [
        region,
        nation,
        supplier,
        part,
        partsupp,
        customer,
        order_table,
        lineitem,
    ] {
        catalog.register_or_replace(table);
    }
}
