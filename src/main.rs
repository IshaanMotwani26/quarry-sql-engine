//! Quarry REPL. Parses, binds, and plans SQL against an in-memory catalog;
//! execution arrives in Phase 3b.
//!
//!   quarry> \tpch
//!   quarry> select l_returnflag, count(*) from lineitem group by l_returnflag;
//!   columns: l_returnflag VARCHAR, count BIGINT

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;

use quarry::ast::Statement;
use quarry::catalog::Catalog;
use quarry::csv::{read_csv_file, CsvOptions};
use quarry::{bind, parse_statements, plan_query, tpch, ParseError};

const HELP: &str = "\
  \\load <table> <file> [delim]   load a CSV with a header row (types are inferred)
                                 delim: one character, or \\t for tab
  \\tpch                          register the 8 TPC-H tables (schemas only, no rows)
  \\d [table]                     list tables, or describe one
  \\ast                           toggle printing the parsed syntax tree
  \\plan                          toggle printing the bound query
  \\explain                       toggle printing the logical plan
  \\q                             quit";

struct Session {
    catalog: Catalog,
    show_ast: bool,
    show_plan: bool,
    show_explain: bool,
}

fn main() {
    let interactive = io::stdin().is_terminal();
    let mut session = Session {
        catalog: Catalog::new(),
        show_ast: false,
        show_plan: false,
        show_explain: false,
    };
    let mut buffer = String::new();

    if interactive {
        println!(
            "quarry v{} | end statements with `;` | \\? for help",
            env!("CARGO_PKG_VERSION")
        );
    }

    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    loop {
        if interactive {
            print!(
                "{}",
                if buffer.is_empty() {
                    "quarry> "
                } else {
                    "     -> "
                }
            );
            io::stdout().flush().ok();
        }
        let Some(Ok(line)) = lines.next() else { break };

        if buffer.is_empty() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed.starts_with('\\') {
                if !session.meta(trimmed) {
                    break;
                }
                continue;
            }
        }

        buffer.push_str(&line);
        buffer.push('\n');
        if buffer.trim_end().ends_with(';') {
            session.run(&buffer);
            buffer.clear();
        }
    }

    // Piped input without a trailing `;`.
    if !buffer.trim().is_empty() {
        session.run(&buffer);
    }
}

impl Session {
    /// Handles a backslash command. Returns false to quit.
    fn meta(&mut self, line: &str) -> bool {
        let parts: Vec<&str> = line.split_whitespace().collect();
        match parts.as_slice() {
            ["\\q"] => return false,
            ["\\?"] | ["\\help"] => println!("{HELP}"),
            ["\\ast"] => {
                self.show_ast = !self.show_ast;
                println!(
                    "syntax tree output {}",
                    if self.show_ast { "on" } else { "off" }
                );
            }
            ["\\plan"] => {
                self.show_plan = !self.show_plan;
                println!(
                    "bound query output {}",
                    if self.show_plan { "on" } else { "off" }
                );
            }
            ["\\explain"] => {
                self.show_explain = !self.show_explain;
                println!(
                    "logical plan output {}",
                    if self.show_explain { "on" } else { "off" }
                );
            }
            ["\\tpch"] => {
                tpch::register_schemas(&mut self.catalog);
                println!("registered {} TPC-H tables (no rows)", tpch::TABLES.len());
            }
            ["\\d"] => {
                let mut any = false;
                for t in self.catalog.tables() {
                    any = true;
                    println!(
                        "  {:<12} {:>4} columns {:>10} rows",
                        t.name,
                        t.schema.len(),
                        t.row_count()
                    );
                }
                if !any {
                    println!("no tables; try \\load or \\tpch");
                }
            }
            ["\\d", name] => match self.catalog.get(&name.to_ascii_lowercase()) {
                Some(t) => {
                    println!("table {} ({} rows)", t.name, t.row_count());
                    for f in &t.schema.fields {
                        println!("  {:<20} {}", f.name, f.ty);
                    }
                }
                None => eprintln!("error: table \"{name}\" does not exist"),
            },
            ["\\load", name, path, rest @ ..] => {
                let delimiter = match rest {
                    [] => ',',
                    ["\\t"] => '\t',
                    [d] if d.chars().count() == 1 => d.chars().next().unwrap(),
                    _ => {
                        eprintln!("error: delimiter must be a single character");
                        return true;
                    }
                };
                let opts = CsvOptions {
                    delimiter,
                    has_header: true,
                };
                match read_csv_file(&name.to_ascii_lowercase(), Path::new(path), &opts, None) {
                    Ok(table) => {
                        println!(
                            "loaded {} rows into {} {}",
                            table.row_count(),
                            table.name,
                            table.schema
                        );
                        self.catalog.register_or_replace(table);
                    }
                    Err(e) => eprintln!("error: {e}"),
                }
            }
            _ => eprintln!("unknown command `{line}`; try \\?"),
        }
        true
    }

    fn run(&self, sql: &str) {
        let stmts = match parse_statements(sql) {
            Ok(s) => s,
            Err(e) => return print_parse_error(sql, &e),
        };
        for stmt in stmts {
            let Statement::Query(query) = &stmt;
            if self.show_ast {
                println!("{stmt}\n{stmt:#?}");
            }
            match bind(&self.catalog, query) {
                Ok(bound) => {
                    let cols: Vec<String> = bound
                        .query
                        .outputs()
                        .iter()
                        .map(|o| format!("{} {}", o.name, o.ty()))
                        .collect();
                    println!("columns: {}", cols.join(", "));
                    if self.show_plan {
                        print!("{}", bound.explain());
                    }
                    if self.show_explain {
                        print!("{}", plan_query(&bound.query).explain(&bound.columns));
                    }
                }
                Err(e) => eprintln!("error: {e}"),
            }
        }
    }
}

/// Prints the offending source line with a caret under the error column.
fn print_parse_error(sql: &str, e: &ParseError) {
    eprintln!("error: {e}");
    if let Some(line) = sql.lines().nth(e.span.line.saturating_sub(1)) {
        eprintln!("  | {line}");
        eprintln!("  | {}^", " ".repeat(e.span.col.saturating_sub(1)));
    }
}
