//! quarry-bench: runs the TPC-H queries on generated data, in Quarry and in
//! SQLite, and compares both the timings and the answers.
//!
//!   cargo run --release --bin quarry-bench              # scales 0.01 and 0.05
//!   cargo run --release --bin quarry-bench -- 0.1 --runs 5
//!   cargo run --release --bin quarry-bench -- --no-sqlite
//!
//! Each query is parsed and bound once, then executed `--runs` times; the
//! fastest run is reported, which filters out noise from the rest of the
//! machine. Data generation and loading are not included.
//!
//! The SQLite comparison uses the `sqlite3` command-line tool (Quarry itself
//! has no dependencies) and is skipped if it isn't installed. The generated
//! tables are exported as CSV and loaded into an in-memory SQLite database
//! (so both engines work from memory), with `ANALYZE` run. SQLite is timed in
//! two setups: with each table's primary key only, and with indexes on the
//! foreign keys as well. Quarry has no indexes at all, so the second setup
//! shows what B-tree index lookups buy SQLite. The queries are SQLite-dialect versions of the same SQL
//! (SQLite has no INTERVAL, EXTRACT, or derived-table column lists). Every
//! result is compared row by row, which makes SQLite an independent check
//! that Quarry's answers are right.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::time::Instant;

use quarry::catalog::Catalog;
use quarry::csv::{write_csv, CsvOptions};
use quarry::types::{Type, Value};
use quarry::{bind, execute, parse_query, tpch};

struct Options {
    scales: Vec<f64>,
    runs: usize,
    sqlite: bool,
}

fn parse_args() -> Result<Options, String> {
    let mut opts = Options {
        scales: Vec::new(),
        runs: 3,
        sqlite: true,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--runs" => {
                let n = args.next().ok_or("--runs needs a number")?;
                opts.runs = n
                    .parse()
                    .ok()
                    .filter(|&r| r > 0)
                    .ok_or(format!("invalid --runs value `{n}`"))?;
            }
            "--no-sqlite" => opts.sqlite = false,
            "-h" | "--help" => {
                return Err("usage: quarry-bench [SCALE ...] [--runs N] [--no-sqlite]".into())
            }
            scale => {
                let s: f64 = scale
                    .parse()
                    .map_err(|_| format!("invalid scale `{scale}`"))?;
                if !(s > 0.0 && s <= 1.0) {
                    return Err(format!("scale must be in (0, 1], got {s}"));
                }
                opts.scales.push(s);
            }
        }
    }
    if opts.scales.is_empty() {
        opts.scales = vec![0.01, 0.05];
    }
    Ok(opts)
}

/// One query's measurements.
struct Measurement {
    query: u32,
    best_ms: f64,
    rows: Vec<Vec<Value>>,
}

/// Runs every TPC-H query `runs` times against `catalog`.
fn run_quarry(catalog: &Catalog, runs: usize) -> Result<Vec<Measurement>, String> {
    let mut out = Vec::new();
    for &(query, sql) in tpch::QUERIES {
        let parsed = parse_query(sql).map_err(|e| format!("Q{query}: {e}"))?;
        let bound = bind(catalog, &parsed).map_err(|e| format!("Q{query}: {e}"))?;
        let mut best = f64::INFINITY;
        let mut rows = Vec::new();
        for _ in 0..runs {
            let start = Instant::now();
            let result = execute(catalog, &bound).map_err(|e| format!("Q{query}: {e}"))?;
            best = best.min(start.elapsed().as_secs_f64() * 1000.0);
            rows = result.rows;
        }
        out.push(Measurement {
            query,
            best_ms: best,
            rows,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// SQLite
// ---------------------------------------------------------------------------

/// The same queries in SQLite's dialect.
const SQLITE_QUERIES: &[(u32, &str)] = &[
    (
        1,
        "select l_returnflag, l_linestatus, sum(l_quantity) as sum_qty, sum(l_extendedprice) as sum_base_price,
           sum(l_extendedprice * (1 - l_discount)) as sum_disc_price,
           sum(l_extendedprice * (1 - l_discount) * (1 + l_tax)) as sum_charge,
           avg(l_quantity) as avg_qty, avg(l_extendedprice) as avg_price, avg(l_discount) as avg_disc,
           count(*) as count_order
         from lineitem
         where l_shipdate <= date('1998-12-01', '-90 days')
         group by l_returnflag, l_linestatus
         order by l_returnflag, l_linestatus",
    ),
    (
        3,
        "select l_orderkey, sum(l_extendedprice * (1 - l_discount)) as revenue, o_orderdate, o_shippriority
         from customer, orders, lineitem
         where c_mktsegment = 'BUILDING' and c_custkey = o_custkey and l_orderkey = o_orderkey
           and o_orderdate < '1995-03-15' and l_shipdate > '1995-03-15'
         group by l_orderkey, o_orderdate, o_shippriority
         order by revenue desc, o_orderdate
         limit 10",
    ),
    (
        6,
        "select sum(l_extendedprice * l_discount) as revenue
         from lineitem
         where l_shipdate >= '1994-01-01' and l_shipdate < date('1994-01-01', '+1 year')
           and l_discount between 0.06 - 0.01 and 0.06 + 0.01 and l_quantity < 24",
    ),
    (
        8,
        "select o_year, sum(case when nation = 'BRAZIL' then volume else 0 end) / sum(volume) as mkt_share
         from (
           select cast(strftime('%Y', o_orderdate) as integer) as o_year,
             l_extendedprice * (1 - l_discount) as volume, n2.n_name as nation
           from part, supplier, lineitem, orders, customer, nation n1, nation n2, region
           where p_partkey = l_partkey and s_suppkey = l_suppkey and l_orderkey = o_orderkey
             and o_custkey = c_custkey and c_nationkey = n1.n_nationkey
             and n1.n_regionkey = r_regionkey and r_name = 'AMERICA'
             and s_nationkey = n2.n_nationkey
             and o_orderdate between '1995-01-01' and '1996-12-31'
             and p_type = 'ECONOMY ANODIZED STEEL'
         ) as all_nations
         group by o_year
         order by o_year",
    ),
    (
        13,
        "select c_count, count(*) as custdist
         from (
           select c_custkey, count(o_orderkey) as c_count
           from customer left outer join orders
             on c_custkey = o_custkey and o_comment not like '%special%requests%'
           group by c_custkey
         ) as c_orders
         group by c_count
         order by custdist desc, c_count desc",
    ),
    (
        22,
        "select cntrycode, count(*) as numcust, sum(c_acctbal) as totacctbal
         from (
           select substr(c_phone, 1, 2) as cntrycode, c_acctbal
           from customer
           where substr(c_phone, 1, 2) in ('13', '31', '23', '29', '30', '18', '17')
             and c_acctbal > (
               select avg(c_acctbal) from customer
               where c_acctbal > 0.00
                 and substr(c_phone, 1, 2) in ('13', '31', '23', '29', '30', '18', '17')
             )
             and not exists (select * from orders where o_custkey = c_custkey)
         ) as custsale
         group by cntrycode
         order by cntrycode",
    ),
];

/// Each table's primary key, as in the TPC-H spec.
const PRIMARY_KEYS: &[(&str, &str)] = &[
    ("region", "r_regionkey"),
    ("nation", "n_nationkey"),
    ("supplier", "s_suppkey"),
    ("part", "p_partkey"),
    ("partsupp", "ps_partkey, ps_suppkey"),
    ("customer", "c_custkey"),
    ("orders", "o_orderkey"),
    ("lineitem", "l_orderkey, l_linenumber"),
];

/// Indexes on the foreign-key columns the queries join on.
const FOREIGN_KEY_INDEXES: &[(&str, &str)] = &[
    ("orders", "o_custkey"),
    ("lineitem", "l_partkey"),
    ("lineitem", "l_suppkey"),
    ("customer", "c_nationkey"),
    ("supplier", "s_nationkey"),
    ("nation", "n_regionkey"),
];

fn sqlite_available() -> bool {
    Command::new("sqlite3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// CREATE TABLE statements derived from Quarry's TPC-H schemas.
fn sqlite_schema() -> String {
    let mut ddl = String::new();
    for (table, columns) in tpch::TABLES {
        let cols: Vec<String> = columns
            .iter()
            .map(|(name, ty)| {
                let sql_type = match ty {
                    Type::Int64 => "INTEGER",
                    Type::Float64 => "REAL",
                    _ => "TEXT", // dates are ISO text, which sorts and compares correctly
                };
                format!("{name} {sql_type}")
            })
            .collect();
        let key = PRIMARY_KEYS
            .iter()
            .find(|(t, _)| t == table)
            .map_or("", |(_, k)| k);
        ddl += &format!(
            "CREATE TABLE {table} ({}, PRIMARY KEY ({key}));\n",
            cols.join(", ")
        );
    }
    ddl
}

/// One query's SQLite measurements.
struct SqliteResult {
    query: u32,
    best_ms: f64,
    /// Output cells as printed by sqlite3, from the first run.
    rows: Vec<Vec<String>>,
}

/// Times each query in SQLite, optionally with foreign-key indexes.
fn run_sqlite(
    catalog: &Catalog,
    runs: usize,
    dir: &Path,
    fk_indexes: bool,
) -> Result<Vec<SqliteResult>, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut script = String::from(".bail on\n");
    script += &sqlite_schema();
    script += ".mode csv\n";
    let opts = CsvOptions {
        delimiter: ',',
        has_header: false,
    };
    for (table, _) in tpch::TABLES {
        let path = dir.join(format!("{table}.csv"));
        let t = catalog.get(table).ok_or(format!("table {table} missing"))?;
        let mut out =
            BufWriter::new(File::create(&path).map_err(|e| format!("{}: {e}", path.display()))?);
        write_csv(t, &mut out, &opts)
            .and_then(|_| out.flush())
            .map_err(|e| e.to_string())?;
        // Forward slashes work on Windows too, and single quotes stop the
        // shell from treating backslashes as escapes.
        script += &format!(
            ".import '{}' {table}\n",
            path.display().to_string().replace('\\', "/")
        );
    }
    if fk_indexes {
        for (table, column) in FOREIGN_KEY_INDEXES {
            script += &format!("CREATE INDEX {table}_{column} ON {table}({column});\n");
        }
    }
    script += "ANALYZE;\n.mode list\n.separator |\n.nullvalue NULL\n.timer on\n";
    for (query, sql) in SQLITE_QUERIES {
        script += &format!(".print ===Q{query}===\n");
        for _ in 0..runs {
            script += &format!("{sql};\n");
        }
    }

    let mut child = Command::new("sqlite3")
        .arg(":memory:")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not start sqlite3: {e}"))?;
    child
        .stdin
        .take()
        .expect("piped")
        .write_all(script.as_bytes())
        .map_err(|e| e.to_string())?;
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "sqlite3 failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    // Output: a marker line per query, then each run's rows followed by a
    // "Run Time: real <seconds> ..." line. Rows are kept from the first run.
    let mut results: Vec<SqliteResult> = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(q) = line
            .strip_prefix("===Q")
            .and_then(|l| l.strip_suffix("==="))
        {
            results.push(SqliteResult {
                query: q.parse().map_err(|_| format!("bad marker {line}"))?,
                best_ms: f64::INFINITY,
                rows: Vec::new(),
            });
        } else if let Some(rest) = line.strip_prefix("Run Time: real ") {
            let secs: f64 = rest
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or(format!("bad timer line {line}"))?;
            if let Some(last) = results.last_mut() {
                last.best_ms = last.best_ms.min(secs * 1000.0);
            }
        } else if let Some(last) = results.last_mut() {
            // Only the first run's rows: before any timing line is recorded.
            if last.best_ms.is_infinite() {
                last.rows
                    .push(line.split('|').map(str::to_string).collect());
            }
        }
    }
    let _ = std::fs::remove_dir_all(dir);
    Ok(results)
}

/// Whether a Quarry value and a SQLite output cell are the same answer.
/// Floats may differ in the last bits (sums can run in a different order).
fn same_cell(quarry: &Value, sqlite: &str) -> bool {
    match quarry {
        Value::Null => sqlite == "NULL",
        Value::Int64(x) => sqlite.parse::<f64>().is_ok_and(|y| y == *x as f64),
        Value::Float64(x) => sqlite
            .parse::<f64>()
            .is_ok_and(|y| (x - y).abs() <= 1e-9 * x.abs().max(y.abs()).max(1.0)),
        other => other.to_string() == sqlite,
    }
}

fn same_result(quarry: &[Vec<Value>], sqlite: &[Vec<String>]) -> bool {
    quarry.len() == sqlite.len()
        && quarry
            .iter()
            .zip(sqlite)
            .all(|(q, s)| q.len() == s.len() && q.iter().zip(s).all(|(a, b)| same_cell(a, b)))
}

fn ratio(quarry_ms: f64, sqlite_ms: f64) -> String {
    if sqlite_ms < 1.0 {
        return "SQLite under 1 ms".into();
    }
    let r = sqlite_ms / quarry_ms;
    if r >= 1.0 {
        format!("Quarry {r:.1}x faster")
    } else {
        format!("SQLite {:.1}x faster", 1.0 / r)
    }
}

fn main() -> ExitCode {
    let opts = match parse_args() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    if cfg!(debug_assertions) {
        eprintln!("warning: debug build; timings are ~10x slower. Use `cargo run --release --bin quarry-bench`.");
    }
    for &scale in &opts.scales {
        let start = Instant::now();
        let mut catalog = Catalog::new();
        tpch::generate(&mut catalog, scale);
        let lineitems = catalog.get("lineitem").map_or(0, |t| t.row_count());
        println!(
            "\n## TPC-H scale {scale} ({lineitems} lineitem rows, generated in {:.1} s)\n",
            start.elapsed().as_secs_f64()
        );
        let results = match run_quarry(&catalog, opts.runs) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        };
        let sqlite = if !opts.sqlite {
            None
        } else if !sqlite_available() {
            eprintln!("note: sqlite3 not found on PATH; skipping the SQLite comparison");
            None
        } else {
            let dir = std::env::temp_dir().join(format!("quarry-bench-{}", std::process::id()));
            let keys_only = run_sqlite(&catalog, opts.runs, &dir, false);
            let indexed = run_sqlite(&catalog, opts.runs, &dir, true);
            match keys_only.and_then(|k| indexed.map(|i| (k, i))) {
                Ok(both) => Some(both),
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
            }
        };
        match &sqlite {
            None => {
                println!("| Query | Result rows | Quarry (best of {}) |", opts.runs);
                println!("|---|---|---|");
                for m in &results {
                    println!("| Q{} | {} | {:.1} ms |", m.query, m.rows.len(), m.best_ms);
                }
            }
            Some((keys_only, indexed)) => {
                println!("| Query | Quarry | SQLite, primary keys | | SQLite, + foreign-key indexes | | Same answer |");
                println!("|---|---|---|---|---|---|---|");
                let mut all_match = true;
                for m in &results {
                    let find = |set: &[SqliteResult]| {
                        set.iter()
                            .find(|r| r.query == m.query)
                            .map(|r| (r.best_ms, r.rows.clone()))
                    };
                    let (Some((keys_ms, keys_rows)), Some((idx_ms, idx_rows))) =
                        (find(keys_only), find(indexed))
                    else {
                        continue;
                    };
                    let same = same_result(&m.rows, &keys_rows) && same_result(&m.rows, &idx_rows);
                    all_match &= same;
                    println!(
                        "| Q{} | {:.1} ms | {:.1} ms | {} | {:.1} ms | {} | {} |",
                        m.query,
                        m.best_ms,
                        keys_ms,
                        ratio(m.best_ms, keys_ms),
                        idx_ms,
                        ratio(m.best_ms, idx_ms),
                        if same { "yes" } else { "**NO**" }
                    );
                }
                if !all_match {
                    eprintln!("error: Quarry and SQLite disagree on at least one query");
                    return ExitCode::FAILURE;
                }
            }
        }
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_compare_by_value() {
        assert!(same_cell(&Value::Int64(1995), "1995"));
        assert!(same_cell(&Value::Float64(0.0), "0.0"));
        assert!(same_cell(&Value::Float64(0.1 + 0.2), "0.3"));
        assert!(!same_cell(&Value::Float64(0.3), "0.31"));
        assert!(same_cell(&Value::Utf8("13".into()), "13"));
        assert!(same_cell(&Value::Null, "NULL"));
        assert!(!same_cell(&Value::Int64(1), "x"));
    }

    #[test]
    fn schema_declares_every_table_with_its_primary_key() {
        let ddl = sqlite_schema();
        assert_eq!(ddl.matches("CREATE TABLE").count(), tpch::TABLES.len());
        assert!(ddl.contains("l_shipdate TEXT"));
        assert!(ddl.contains("l_discount REAL"));
        assert!(ddl.contains("PRIMARY KEY (l_orderkey, l_linenumber)"));
    }

    #[test]
    fn every_quarry_query_has_a_sqlite_version() {
        let quarry: Vec<u32> = tpch::QUERIES.iter().map(|(q, _)| *q).collect();
        let sqlite: Vec<u32> = SQLITE_QUERIES.iter().map(|(q, _)| *q).collect();
        assert_eq!(quarry, sqlite);
    }
}
