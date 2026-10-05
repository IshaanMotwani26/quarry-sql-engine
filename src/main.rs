//! Quarry REPL. For now it parses SQL and echoes the canonical form back;
//! later phases plug the planner and executor in here.
//!
//!   quarry> SELECT a+b*2 FROM t WHERE x BETWEEN 1 AND 5;
//!   SELECT a + b * 2 FROM t WHERE x BETWEEN 1 AND 5
//!
//! Meta-commands: \ast (toggle AST dump), \q (quit)

use std::io::{self, BufRead, IsTerminal, Write};

use quarry::{parse_statements, ParseError};

fn main() {
    let interactive = io::stdin().is_terminal();
    let mut show_ast = false;
    let mut buffer = String::new();

    if interactive {
        println!(
            "quarry v{} | statements end with `;` | \\ast toggles AST | \\q quits",
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
            match line.trim() {
                "\\q" => break,
                "\\ast" => {
                    show_ast = !show_ast;
                    println!("AST output {}", if show_ast { "on" } else { "off" });
                    continue;
                }
                "" => continue,
                _ => {}
            }
        }

        buffer.push_str(&line);
        buffer.push('\n');
        if buffer.trim_end().ends_with(';') {
            run(&buffer, show_ast);
            buffer.clear();
        }
    }

    // Piped input without a trailing `;`.
    if !buffer.trim().is_empty() {
        run(&buffer, show_ast);
    }
}

fn run(sql: &str, show_ast: bool) {
    match parse_statements(sql) {
        Ok(stmts) => {
            for stmt in stmts {
                println!("{stmt}");
                if show_ast {
                    println!("{stmt:#?}");
                }
            }
        }
        Err(e) => print_error(sql, &e),
    }
}

/// Prints the offending source line with a caret under the error column.
fn print_error(sql: &str, e: &ParseError) {
    eprintln!("error: {e}");
    if let Some(line) = sql.lines().nth(e.span.line.saturating_sub(1)) {
        eprintln!("  | {line}");
        eprintln!("  | {}^", " ".repeat(e.span.col.saturating_sub(1)));
    }
}
