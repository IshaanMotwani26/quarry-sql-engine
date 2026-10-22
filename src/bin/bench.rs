//! quarry-bench: runs the TPC-H queries on generated data and reports timings.
//!
//!   cargo run --release --bin quarry-bench              # scales 0.01 and 0.05
//!   cargo run --release --bin quarry-bench -- 0.1 --runs 5
//!
//! Each query is parsed and bound once, then executed `--runs` times; the
//! fastest run is reported, which filters out noise from the rest of the
//! machine. Data generation time is not included.

use std::process::ExitCode;
use std::time::Instant;

use quarry::catalog::Catalog;
use quarry::types::Value;
use quarry::{bind, execute, parse_query, tpch};

struct Options {
    scales: Vec<f64>,
    runs: usize,
}

fn parse_args() -> Result<Options, String> {
    let mut opts = Options {
        scales: Vec::new(),
        runs: 3,
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
            "-h" | "--help" => return Err("usage: quarry-bench [SCALE ...] [--runs N]".into()),
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
        println!("| Query | Result rows | Quarry (best of {}) |", opts.runs);
        println!("|---|---|---|");
        for m in &results {
            println!("| Q{} | {} | {:.1} ms |", m.query, m.rows.len(), m.best_ms);
        }
    }
    ExitCode::SUCCESS
}
