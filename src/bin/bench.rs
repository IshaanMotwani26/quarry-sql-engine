//! quarry-bench: runs the TPC-H queries on generated data, in Quarry, SQLite,
//! and DuckDB, and compares both the timings and the answers.
//!
//!   cargo run --release --bin quarry-bench              # scales 0.01 and 0.05
//!   cargo run --release --bin quarry-bench -- 1 --runs 3
//!   cargo run --release --bin quarry-bench -- --no-sqlite --no-duckdb
//!
//! Each query is parsed and bound once, then executed `--runs` times; the
//! fastest run is reported, which filters out noise from the rest of the
//! machine. Data generation and loading are not included.
//!
//! The comparisons use the `sqlite3` and `duckdb` command-line tools (Quarry
//! itself has no dependencies); each is skipped if it isn't installed. The
//! generated tables are exported as CSV once and loaded into in-memory
//! databases, so every engine works from memory.
//!
//! - SQLite runs with `ANALYZE`, in two setups: each table's primary key
//!   only, and with indexes on the foreign keys as well. Quarry has no
//!   indexes, so the second setup shows what B-tree lookups buy SQLite. The
//!   queries are SQLite-dialect versions of the same SQL (SQLite has no
//!   INTERVAL, EXTRACT, or derived-table column lists).
//! - DuckDB, a vectorized columnar engine, runs Quarry's own SQL with one
//!   thread (like Quarry and SQLite) and with all cores.
//!
//! Every result is compared row by row, so each engine is an independent
//! check that Quarry's answers are right.

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
    duckdb: bool,
}

fn parse_args() -> Result<Options, String> {
    let mut opts = Options {
        scales: Vec::new(),
        runs: 3,
        sqlite: true,
        duckdb: true,
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
            "--no-duckdb" => opts.duckdb = false,
            "-h" | "--help" => {
                return Err(
                    "usage: quarry-bench [SCALE ...] [--runs N] [--no-sqlite] [--no-duckdb]".into(),
                )
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

fn available(program: &str) -> bool {
    Command::new(program)
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

/// One query's measurements from an external engine.
struct EngineResult {
    query: u32,
    best_ms: f64,
    /// Output cells as printed by the engine's shell, from the first run.
    rows: Vec<Vec<String>>,
}

/// Writes every TPC-H table to `dir` as a headerless CSV file.
fn export_tables(catalog: &Catalog, dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let opts = CsvOptions {
        delimiter: ',',
        has_header: false,
    };
    for (table, _) in tpch::TABLES {
        let path = csv_path(dir, table);
        let t = catalog.get(table).ok_or(format!("table {table} missing"))?;
        let mut out = BufWriter::new(File::create(&path).map_err(|e| format!("{path}: {e}"))?);
        write_csv(t, &mut out, &opts)
            .and_then(|_| out.flush())
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Forward slashes work on Windows too, and both shells take them inside
/// single quotes without treating backslashes as escapes.
fn csv_path(dir: &Path, table: &str) -> String {
    dir.join(format!("{table}.csv"))
        .display()
        .to_string()
        .replace('\\', "/")
}

/// Appends each query `runs` times, each group after a marker line.
fn append_queries(script: &mut String, queries: &[(u32, String)], runs: usize) {
    for (query, sql) in queries {
        *script += &format!(".print ===Q{query}===\n");
        for _ in 0..runs {
            *script += &format!("{sql};\n");
        }
    }
}

/// The sqlite3 script: schema, import, optional indexes, then the queries.
fn sqlite_script(dir: &Path, runs: usize, fk_indexes: bool) -> String {
    let mut script = String::from(".bail on\n");
    script += &sqlite_schema();
    script += ".mode csv\n";
    for (table, _) in tpch::TABLES {
        script += &format!(".import '{}' {table}\n", csv_path(dir, table));
    }
    if fk_indexes {
        for (table, column) in FOREIGN_KEY_INDEXES {
            script += &format!("CREATE INDEX {table}_{column} ON {table}({column});\n");
        }
    }
    script += "ANALYZE;\n.mode list\n.separator |\n.nullvalue NULL\n.timer on\n";
    let queries: Vec<(u32, String)> = SQLITE_QUERIES
        .iter()
        .map(|(q, sql)| (*q, sql.to_string()))
        .collect();
    append_queries(&mut script, &queries, runs);
    script
}

// ---------------------------------------------------------------------------
// DuckDB
// ---------------------------------------------------------------------------

/// CREATE TABLE statements with DuckDB's types.
fn duckdb_schema() -> String {
    let mut ddl = String::new();
    for (table, columns) in tpch::TABLES {
        let cols: Vec<String> = columns
            .iter()
            .map(|(name, ty)| {
                let sql_type = match ty {
                    Type::Int64 => "BIGINT",
                    Type::Float64 => "DOUBLE",
                    Type::Date => "DATE",
                    _ => "VARCHAR",
                };
                format!("{name} {sql_type}")
            })
            .collect();
        ddl += &format!("CREATE TABLE {table} ({});\n", cols.join(", "));
    }
    ddl
}

/// Quarry's own query text, with two changes for DuckDB:
///
/// - DuckDB doesn't accept the interval precision `day (3)` that the TPC-H
///   spec writes.
/// - DuckDB evaluates `0.06 - 0.01` as an exact DECIMAL (0.05), while Quarry
///   computes it in floating point (0.049999...). The prices and discounts
///   are stored as DOUBLE in both engines, so the literals are made DOUBLE
///   too, or the engines would disagree about which discounts fall inside
///   Q6's range.
fn duckdb_queries() -> Vec<(u32, String)> {
    tpch::QUERIES
        .iter()
        .map(|(q, sql)| {
            let sql = sql
                .replace("day (3)", "day")
                .replace("0.06 - 0.01", "0.06::double - 0.01::double")
                .replace("0.06 + 0.01", "0.06::double + 0.01::double");
            (*q, sql)
        })
        .collect()
}

/// The duckdb script: schema, CSV load, thread count, then the queries.
fn duckdb_script(dir: &Path, runs: usize, threads: usize) -> String {
    let mut script = String::from(".bail on\n");
    script += &duckdb_schema();
    for (table, _) in tpch::TABLES {
        script += &format!(
            "COPY {table} FROM '{}' (FORMAT csv, HEADER false);\n",
            csv_path(dir, table)
        );
    }
    script += &format!("SET threads = {threads};\n");
    script += ".mode list\n.separator |\n.nullvalue NULL\n.headers off\n.timer on\n";
    append_queries(&mut script, &duckdb_queries(), runs);
    script
}

// ---------------------------------------------------------------------------
// Running an engine's shell
// ---------------------------------------------------------------------------

/// Pipes `script` into `program :memory:` and collects each query's rows and
/// best time. `timer` is the prefix of the shell's timing lines: sqlite3
/// prints "Run Time: real 0.012 ...", duckdb "Run Time (s): real 0.012 ...".
fn run_shell(program: &str, script: &str, timer: &str) -> Result<Vec<EngineResult>, String> {
    let mut child = Command::new(program)
        .arg(":memory:")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not start {program}: {e}"))?;
    child
        .stdin
        .take()
        .expect("piped")
        .write_all(script.as_bytes())
        .map_err(|e| e.to_string())?;
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "{program} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    parse_output(&String::from_utf8_lossy(&output.stdout), timer)
}

/// Output: a marker line per query, then each run's rows followed by a
/// timing line. Rows are kept from the first run. Timing lines before the
/// first marker (for loading and setup) are ignored.
fn parse_output(stdout: &str, timer: &str) -> Result<Vec<EngineResult>, String> {
    let mut results: Vec<EngineResult> = Vec::new();
    for line in stdout.lines() {
        if let Some(q) = line
            .strip_prefix("===Q")
            .and_then(|l| l.strip_suffix("==="))
        {
            results.push(EngineResult {
                query: q.parse().map_err(|_| format!("bad marker {line}"))?,
                best_ms: f64::INFINITY,
                rows: Vec::new(),
            });
        } else if let Some(rest) = line.strip_prefix(timer) {
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
    Ok(results)
}

/// Whether a Quarry value and an engine's output cell are the same answer.
/// Floats may differ in the last bits (sums can run in a different order).
fn same_cell(quarry: &Value, other: &str) -> bool {
    match quarry {
        Value::Null => other == "NULL",
        Value::Int64(x) => other.parse::<f64>().is_ok_and(|y| y == *x as f64),
        Value::Float64(x) => other
            .parse::<f64>()
            .is_ok_and(|y| (x - y).abs() <= 1e-9 * x.abs().max(y.abs()).max(1.0)),
        v => v.to_string() == other,
    }
}

fn same_result(quarry: &[Vec<Value>], other: &[Vec<String>]) -> bool {
    quarry.len() == other.len()
        && quarry
            .iter()
            .zip(other)
            .all(|(q, s)| q.len() == s.len() && q.iter().zip(s).all(|(a, b)| same_cell(a, b)))
}

/// Above this scale SQLite runs only with foreign-key indexes: without
/// them its Q22 plan scans `orders` once per customer, which grows with the
/// square of the scale (5 s per run at 0.1, minutes at 1).
const SQLITE_KEYS_ONLY_MAX_SCALE: f64 = 0.1;

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
    let sqlite = opts.sqlite && available("sqlite3");
    if opts.sqlite && !sqlite {
        eprintln!("note: sqlite3 not found on PATH; skipping SQLite");
    }
    let duckdb = opts.duckdb && available("duckdb");
    if opts.duckdb && !duckdb {
        eprintln!("note: duckdb not found on PATH; skipping DuckDB");
    }
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());

    for &scale in &opts.scales {
        let start = Instant::now();
        let mut catalog = Catalog::new();
        tpch::generate(&mut catalog, scale);
        let lineitems = catalog.get("lineitem").map_or(0, |t| t.row_count());
        println!(
            "\n## TPC-H scale {scale} ({lineitems} lineitem rows, generated in {:.1} s)\n",
            start.elapsed().as_secs_f64()
        );
        let quarry = match run_quarry(&catalog, opts.runs) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        };

        // Each external engine: a column label and its results.
        let mut engines: Vec<(String, Vec<EngineResult>)> = Vec::new();
        if sqlite || duckdb {
            let dir = std::env::temp_dir().join(format!("quarry-bench-{}", std::process::id()));
            let mut runs: Vec<(String, &str, String, &str)> = Vec::new();
            if sqlite && scale <= SQLITE_KEYS_ONLY_MAX_SCALE {
                let script = sqlite_script(&dir, opts.runs, false);
                runs.push((
                    "SQLite, primary keys".into(),
                    "sqlite3",
                    script,
                    "Run Time: real ",
                ));
            }
            if sqlite {
                let script = sqlite_script(&dir, opts.runs, true);
                runs.push((
                    "SQLite, + FK indexes".into(),
                    "sqlite3",
                    script,
                    "Run Time: real ",
                ));
            }
            if duckdb {
                for threads in [1, cores] {
                    let label = match threads {
                        1 => "DuckDB, 1 thread".to_string(),
                        n => format!("DuckDB, {n} threads"),
                    };
                    let script = duckdb_script(&dir, opts.runs, threads);
                    runs.push((label, "duckdb", script, "Run Time (s): real "));
                    if cores == 1 {
                        break;
                    }
                }
            }
            let result = export_tables(&catalog, &dir).and_then(|_| {
                runs.into_iter()
                    .map(|(label, program, script, timer)| {
                        eprintln!("running {label}...");
                        run_shell(program, &script, timer).map(|r| (label, r))
                    })
                    .collect::<Result<Vec<_>, String>>()
            });
            let _ = std::fs::remove_dir_all(&dir);
            match result {
                Ok(r) => engines = r,
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
            }
            if sqlite && scale > SQLITE_KEYS_ONLY_MAX_SCALE {
                println!("(SQLite without foreign-key indexes is skipped above scale {SQLITE_KEYS_ONLY_MAX_SCALE}: its Q22 plan is quadratic.)\n");
            }
        }

        let mut header = format!("| Query | Rows | Quarry (best of {})", opts.runs);
        let mut rule = String::from("|---|---|---");
        for (label, _) in &engines {
            header += &format!(" | {label}");
            rule += "|---";
        }
        if !engines.is_empty() {
            header += " | Same answers";
            rule += "|---";
        }
        println!("{header} |\n{rule}|");

        let mut all_match = true;
        for m in &quarry {
            let mut line = format!("| Q{} | {} | {} ", m.query, m.rows.len(), ms(m.best_ms));
            let mut same = true;
            for (_, results) in &engines {
                match results.iter().find(|r| r.query == m.query) {
                    Some(r) => {
                        same &= same_result(&m.rows, &r.rows);
                        line += &format!("| {} ", ms(r.best_ms));
                    }
                    None => {
                        same = false;
                        line += "| missing ";
                    }
                }
            }
            if !engines.is_empty() {
                line += if same { "| yes " } else { "| **NO** " };
            }
            all_match &= same;
            println!("{line}|");
        }
        if !all_match {
            eprintln!("error: the engines disagree on at least one query");
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}

/// Milliseconds with a thousands separator, e.g. "1,234 ms" or "5.2 ms".
fn ms(x: f64) -> String {
    if x < 100.0 {
        return format!("{x:.1} ms");
    }
    let digits = format!("{:.0}", x);
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    format!("{out} ms")
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
    fn parses_shell_output() {
        let out = "Run Time (s): real 0.500 user 0.1 sys 0\n===Q1===\nA|1.5\nB|NULL\n\
                   Run Time (s): real 0.020 user 0 sys 0\nA|1.5\nB|NULL\n\
                   Run Time (s): real 0.010 user 0 sys 0\n===Q6===\n42\n\
                   Run Time (s): real 0.003 user 0 sys 0\n";
        let r = parse_output(out, "Run Time (s): real ").unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!((r[0].query, r[0].best_ms), (1, 10.0));
        assert_eq!(r[0].rows, [["A", "1.5"], ["B", "NULL"]]);
        assert_eq!((r[1].query, r[1].rows.len()), (6, 1));
    }

    #[test]
    fn duckdb_queries_use_double_literals() {
        let q6 = &duckdb_queries()[2].1;
        assert!(q6.contains("0.06::double - 0.01::double"), "{q6}");
        assert!(!duckdb_queries()[0].1.contains("day (3)"));
        assert_eq!(
            duckdb_schema().matches("CREATE TABLE").count(),
            tpch::TABLES.len()
        );
    }

    #[test]
    fn formats_milliseconds() {
        assert_eq!(ms(5.25), "5.2 ms");
        assert_eq!(ms(1234.4), "1,234 ms");
        assert_eq!(ms(123.0), "123 ms");
    }

    #[test]
    fn every_quarry_query_has_a_sqlite_version() {
        let quarry: Vec<u32> = tpch::QUERIES.iter().map(|(q, _)| *q).collect();
        let sqlite: Vec<u32> = SQLITE_QUERIES.iter().map(|(q, _)| *q).collect();
        assert_eq!(quarry, sqlite);
    }
}
