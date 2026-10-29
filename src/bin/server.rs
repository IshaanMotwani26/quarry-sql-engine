//! quarry-server: serves Quarry over the PostgreSQL wire protocol.
//!
//!   cargo run --release --bin quarry-server -- --tpch 0.01
//!   psql -h localhost -p 5433

use std::net::TcpListener;
use std::path::Path;
use std::process::exit;
use std::sync::Arc;

use quarry::catalog::Catalog;
use quarry::csv::{read_csv_file, CsvOptions};
use quarry::parquet::read_parquet_file;
use quarry::{pgwire, tpch};

const USAGE: &str = "\
usage: quarry-server [--port N] [--tpch SCALE] [--load NAME=FILE]...

  --port N          port to listen on, on localhost only (default 5433)
  --tpch SCALE      generate TPC-H data, e.g. 0.01
  --load NAME=FILE  load a CSV (with a header row) or .parquet file as table NAME";

fn main() {
    let mut port: u16 = 5433;
    let mut catalog = Catalog::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || {
            args.next()
                .unwrap_or_else(|| fail(&format!("{arg} needs a value")))
        };
        match arg.as_str() {
            "--port" => port = value().parse().unwrap_or_else(|_| fail("invalid port")),
            "--tpch" => {
                let scale: f64 = value().parse().unwrap_or_else(|_| fail("invalid scale"));
                if !(scale > 0.0 && scale <= 1.0) {
                    fail("scale must be in (0, 1]");
                }
                tpch::generate(&mut catalog, scale);
            }
            "--load" => {
                let spec = value();
                let (name, path) = spec
                    .split_once('=')
                    .unwrap_or_else(|| fail("--load expects NAME=FILE"));
                let name = name.to_ascii_lowercase();
                let path = Path::new(path);
                let table = if path
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("parquet"))
                {
                    read_parquet_file(&name, path)
                } else {
                    read_csv_file(&name, path, &CsvOptions::default(), None)
                };
                let table = table.unwrap_or_else(|e| fail(&e));
                eprintln!("loaded {} rows into {}", table.row_count(), table.name);
                catalog.register_or_replace(table);
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            other => fail(&format!("unknown argument {other}")),
        }
    }

    // Localhost only: the server has no authentication.
    let listener = TcpListener::bind(("127.0.0.1", port))
        .unwrap_or_else(|e| fail(&format!("cannot listen on port {port}: {e}")));
    eprintln!("listening on 127.0.0.1:{port}; connect with: psql -h localhost -p {port}");
    if let Err(e) = pgwire::serve(listener, Arc::new(catalog)) {
        fail(&e.to_string());
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("error: {msg}\n\n{USAGE}");
    exit(2);
}
