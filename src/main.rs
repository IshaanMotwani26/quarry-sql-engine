//! Quarry REPL: parses, binds, plans, and executes SQL against an in-memory
//! catalog.
//!
//!   quarry> \demo
//!   quarry> select e.name, d.name from emp e join dept d on e.dept_id = d.id;

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;
use std::time::Instant;

use quarry::ast::Statement;
use quarry::catalog::Catalog;
use quarry::csv::{parse_csv, read_csv_file, CsvOptions};
use quarry::exec::{execute, QueryResult};
use quarry::types::{Type, Value};
use quarry::{bind, parse_statements, plan_query, tpch, ParseError};

const HELP: &str = "\
  \\load <table> <file> [delim]   load a CSV with a header row (types are inferred)
                                 delim: one character, or \\t for tab
  \\demo                          load small sample tables: emp, dept
  \\tpch                          register the 8 TPC-H tables (schemas only, no rows)
  \\d [table]                     list tables, or describe one
  \\ast                           toggle printing the parsed syntax tree
  \\plan                          toggle printing the bound query
  \\explain                       toggle printing the logical plan
  \\timing                        toggle printing execution time
  \\q                             quit";

const DEMO_DEPT: &str = "\
id,name,budget
1,Engineering,500000
2,Sales,200000
3,Research,350000
4,Legal,
";

const DEMO_EMP: &str = "\
id,name,dept_id,salary,hired,manager_id
1,Ada,1,185000,2019-03-04,
2,Grace,1,172000,2020-07-15,1
3,Linus,1,140000,2022-01-10,1
4,Ken,2,98000,2018-11-01,
5,Barbara,2,105000,2021-05-20,4
6,Edsger,3,160000,2017-09-12,
7,Margaret,,120000,2023-02-28,
";

struct Session {
    catalog: Catalog,
    show_ast: bool,
    show_plan: bool,
    show_explain: bool,
    timing: bool,
}

fn main() {
    let interactive = io::stdin().is_terminal();
    let mut session = Session {
        catalog: Catalog::new(),
        show_ast: false,
        show_plan: false,
        show_explain: false,
        timing: interactive,
    };
    let mut buffer = String::new();

    if interactive {
        println!(
            "quarry v{} | end statements with `;` | \\? for help | \\demo for sample data",
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

fn on_off(b: bool) -> &'static str {
    if b {
        "on"
    } else {
        "off"
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
                println!("syntax tree output {}", on_off(self.show_ast));
            }
            ["\\plan"] => {
                self.show_plan = !self.show_plan;
                println!("bound query output {}", on_off(self.show_plan));
            }
            ["\\explain"] => {
                self.show_explain = !self.show_explain;
                println!("logical plan output {}", on_off(self.show_explain));
            }
            ["\\timing"] => {
                self.timing = !self.timing;
                println!("timing {}", on_off(self.timing));
            }
            ["\\demo"] => {
                let opts = CsvOptions::default();
                for (name, csv) in [("dept", DEMO_DEPT), ("emp", DEMO_EMP)] {
                    let table = parse_csv(name, csv, &opts, None).expect("demo data is valid");
                    println!(
                        "loaded {} rows into {} {}",
                        table.row_count(),
                        table.name,
                        table.schema
                    );
                    self.catalog.register_or_replace(table);
                }
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
                    println!("no tables; try \\demo, \\load, or \\tpch");
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
            let bound = match bind(&self.catalog, query) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("error: {e}");
                    continue;
                }
            };
            if self.show_plan {
                print!("{}", bound.explain());
            }
            if self.show_explain {
                print!("{}", plan_query(&bound.query).explain(&bound.columns));
            }
            let start = Instant::now();
            match execute(&self.catalog, &bound) {
                Ok(result) => {
                    print!("{}", format_table(&result));
                    if self.timing {
                        println!("Time: {:.3} ms", start.elapsed().as_secs_f64() * 1000.0);
                    }
                }
                Err(e) => eprintln!("error: {e}"),
            }
        }
    }
}

fn display(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        other => other.to_string(),
    }
}

/// Renders a result in psql's aligned format: numbers right-aligned, text left.
fn format_table(result: &QueryResult) -> String {
    let cells: Vec<Vec<String>> = result
        .rows
        .iter()
        .map(|r| r.iter().map(display).collect())
        .collect();
    let mut widths: Vec<usize> = result
        .columns
        .iter()
        .map(|c| c.name.chars().count())
        .collect();
    for row in &cells {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let right_align: Vec<bool> = result
        .columns
        .iter()
        .map(|c| matches!(c.ty, Type::Int64 | Type::Float64))
        .collect();

    let mut out = String::new();
    let header: Vec<String> = result
        .columns
        .iter()
        .zip(&widths)
        .map(|(c, &w)| {
            let pad = w - c.name.chars().count();
            format!(
                " {}{}{} ",
                " ".repeat(pad / 2),
                c.name,
                " ".repeat(pad - pad / 2)
            )
        })
        .collect();
    out += &header.join("|");
    out += "\n";
    let rule: Vec<String> = widths.iter().map(|&w| "-".repeat(w + 2)).collect();
    out += &rule.join("+");
    out += "\n";
    for row in &cells {
        let line: Vec<String> = row
            .iter()
            .zip(&widths)
            .zip(&right_align)
            .map(|((cell, &w), &right)| {
                if right {
                    format!(" {cell:>w$} ")
                } else {
                    format!(" {cell:<w$} ")
                }
            })
            .collect();
        out += line.join("|").trim_end();
        out += "\n";
    }
    let n = result.rows.len();
    out += &format!("({n} row{})\n", if n == 1 { "" } else { "s" });
    out
}

/// Prints the offending source line with a caret under the error column.
fn print_parse_error(sql: &str, e: &ParseError) {
    eprintln!("error: {e}");
    if let Some(line) = sql.lines().nth(e.span.line.saturating_sub(1)) {
        eprintln!("  | {line}");
        eprintln!("  | {}^", " ".repeat(e.span.col.saturating_sub(1)));
    }
}
