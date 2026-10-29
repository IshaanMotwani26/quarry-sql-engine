//! One client connection's state, and how each query message is answered.

use std::io;
use std::sync::Arc;

use super::protocol::{
    command_complete, empty_query_response, error_response, ready_for_query, Body, Message, Out,
    PgError,
};
use super::sql::{self, control_command, split_statements, Control};
use crate::catalog::{Catalog, Field};
use crate::error::ParseError;
use crate::eval::Row;
use crate::types::{format_date, Type, Value};
use crate::{bind, execute, parse_statements};

/// What running one statement produced.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Rows {
        columns: Vec<Field>,
        rows: Vec<Row>,
    },
    /// A command with no result rows, and its tag (such as `BEGIN`).
    Command(String),
}

pub struct Session {
    catalog: Arc<Catalog>,
    /// Transaction status as the protocol reports it: b'I' idle, b'T' in a
    /// transaction block, b'E' in a block that hit an error. Quarry has no
    /// real transactions, but clients like psycopg wrap queries in BEGIN and
    /// COMMIT and check this status.
    tx: u8,
}

impl Session {
    pub fn new(catalog: Arc<Catalog>) -> Self {
        Session { catalog, tx: b'I' }
    }

    /// Answers one message. Returns false when the client asks to close the
    /// connection.
    pub fn handle(&mut self, msg: &Message, out: &mut Out) -> io::Result<bool> {
        match msg.tag {
            b'Q' => {
                let sql = Body::new(&msg.body).cstr()?;
                self.simple_query(&sql, out);
            }
            b'X' => return Ok(false),
            tag => {
                let e = PgError::new(
                    "0A000",
                    format!("unsupported message type '{}'", tag as char),
                );
                error_response(out, &e);
                ready_for_query(out, self.tx);
            }
        }
        Ok(true)
    }

    /// The simple query protocol: a string of one or more statements, each
    /// answered in turn, stopping at the first error.
    fn simple_query(&mut self, sql: &str, out: &mut Out) {
        let statements = split_statements(sql);
        if statements.is_empty() {
            empty_query_response(out);
        }
        for (offset, stmt) in statements {
            match self.run(stmt) {
                Ok(outcome) => send_outcome(out, &outcome),
                Err(mut e) => {
                    if let Some(p) = &mut e.position {
                        *p += sql[..offset].chars().count();
                    }
                    error_response(out, &e);
                    break;
                }
            }
        }
        ready_for_query(out, self.tx);
    }

    /// Runs one statement, tracking the transaction status.
    pub fn run(&mut self, stmt: &str) -> Result<Outcome, PgError> {
        let result = match control_command(stmt) {
            Some(c) => self.control(c),
            None if self.tx == b'E' => Err(aborted()),
            None => run_query(&self.catalog, stmt),
        };
        if result.is_err() && self.tx == b'T' {
            self.tx = b'E';
        }
        result
    }

    fn control(&mut self, c: Control) -> Result<Outcome, PgError> {
        let tag = match c {
            Control::Begin if self.tx == b'E' => return Err(aborted()),
            Control::Begin => {
                self.tx = b'T';
                "BEGIN"
            }
            Control::Commit => {
                // Committing a failed block rolls it back, as in Postgres.
                let tag = if self.tx == b'E' {
                    "ROLLBACK"
                } else {
                    "COMMIT"
                };
                self.tx = b'I';
                tag
            }
            Control::Rollback => {
                self.tx = b'I';
                "ROLLBACK"
            }
            Control::Ignored(_) if self.tx == b'E' => return Err(aborted()),
            Control::Ignored(tag) => tag,
        };
        Ok(Outcome::Command(tag.into()))
    }
}

fn aborted() -> PgError {
    PgError::new(
        "25P02",
        "current transaction is aborted, commands ignored until end of transaction block",
    )
}

/// Parses, binds, and executes one SQL query.
pub fn run_query(catalog: &Catalog, sql: &str) -> Result<Outcome, PgError> {
    let statements = parse_statements(sql).map_err(|e| syntax_error(sql, &e))?;
    let [crate::ast::Statement::Query(query)] = statements.as_slice() else {
        return Err(PgError::new("42601", "expected exactly one statement"));
    };
    let bound = bind(catalog, query).map_err(|e| bind_error(e.0))?;
    let result = execute(catalog, &bound).map_err(|e| exec_error(e.0))?;
    Ok(Outcome::Rows {
        columns: result.columns,
        rows: result.rows,
    })
}

fn syntax_error(sql: &str, e: &ParseError) -> PgError {
    // Convert the 1-based line and column to a 1-based character position.
    let before: usize = sql
        .split('\n')
        .take(e.span.line.saturating_sub(1))
        .map(|line| line.chars().count() + 1)
        .sum();
    PgError {
        code: "42601",
        message: e.message.clone(),
        position: Some(before + e.span.col),
    }
}

fn bind_error(message: String) -> PgError {
    let code = if message.starts_with("table") && message.ends_with("does not exist") {
        "42P01" // undefined_table
    } else if message.starts_with("column") && message.ends_with("does not exist") {
        "42703" // undefined_column
    } else if message.starts_with("operator does not exist") {
        "42883" // undefined_function
    } else if message.contains("GROUP BY") {
        "42803" // grouping_error
    } else {
        "42804" // datatype_mismatch, the most common remaining cause
    };
    PgError::new(code, message)
}

fn exec_error(message: String) -> PgError {
    let code = match message.as_str() {
        "division by zero" => "22012",
        m if m.contains("overflow") || m.contains("out of range") => "22003",
        _ => "XX000", // internal_error
    };
    PgError::new(code, message)
}

/// The Postgres type OID and size that each Quarry type is reported as.
pub fn pg_type(ty: Type) -> (i32, i16) {
    match ty {
        Type::Boolean => (sql::BOOL, 1),
        Type::Int64 => (sql::INT8, 8),
        Type::Float64 => (sql::FLOAT8, 8),
        Type::Date => (sql::DATE, 4),
        Type::Interval => (sql::INTERVAL, 16),
        Type::Utf8 | Type::Null => (sql::TEXT, -1),
    }
}

/// A value in Postgres's text format, or `None` for NULL.
pub fn text_value(v: &Value) -> Option<String> {
    Some(match v {
        Value::Null => return None,
        Value::Boolean(b) => (if *b { "t" } else { "f" }).into(),
        Value::Int64(n) => n.to_string(),
        Value::Float64(x) if x.is_nan() => "NaN".into(),
        Value::Float64(x) if x.is_infinite() => {
            (if *x > 0.0 { "Infinity" } else { "-Infinity" }).into()
        }
        Value::Float64(x) => x.to_string(),
        Value::Utf8(s) => s.clone(),
        Value::Date(d) => format_date(*d),
        Value::Interval { months, days } => format!("{months} mons {days} days"),
    })
}

pub fn row_description(out: &mut Out, columns: &[Field]) {
    out.msg(b'T', |w| {
        w.i16(columns.len() as i16);
        for c in columns {
            let (oid, size) = pg_type(c.ty);
            w.cstr(&c.name);
            w.i32(0); // table OID: not a table column
            w.i16(0); // column number within that table
            w.i32(oid);
            w.i16(size);
            w.i32(-1); // type modifier
            w.i16(0); // text format
        }
    });
}

pub fn data_row(out: &mut Out, row: &Row) {
    out.msg(b'D', |w| {
        w.i16(row.len() as i16);
        for v in row {
            match text_value(v) {
                None => w.i32(-1),
                Some(s) => {
                    w.i32(s.len() as i32);
                    w.bytes(s.as_bytes());
                }
            }
        }
    });
}

fn send_outcome(out: &mut Out, outcome: &Outcome) {
    match outcome {
        Outcome::Rows { columns, rows } => {
            row_description(out, columns);
            for row in rows {
                data_row(out, row);
            }
            command_complete(out, &format!("SELECT {}", rows.len()));
        }
        Outcome::Command(tag) => command_complete(out, tag),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_format() {
        assert_eq!(text_value(&Value::Boolean(true)).unwrap(), "t");
        assert_eq!(text_value(&Value::Float64(0.1)).unwrap(), "0.1");
        assert_eq!(
            text_value(&Value::Float64(f64::NEG_INFINITY)).unwrap(),
            "-Infinity"
        );
        assert_eq!(text_value(&Value::Date(0)).unwrap(), "1970-01-01");
        assert_eq!(text_value(&Value::Null), None);
    }

    #[test]
    fn syntax_error_positions_count_characters() {
        let catalog = Catalog::new();
        let e = run_query(&catalog, "select 'é',\n  from").unwrap_err();
        assert_eq!(e.code, "42601");
        assert_eq!(e.position, Some(15), "{e:?}");
    }

    #[test]
    fn transaction_status() {
        let mut s = Session::new(Arc::new(Catalog::new()));
        s.run("begin").unwrap();
        assert_eq!(s.tx, b'T');
        assert_eq!(s.run("select * from nope").unwrap_err().code, "42P01");
        assert_eq!(s.tx, b'E');
        assert_eq!(s.run("select 1").unwrap_err().code, "25P02");
        assert_eq!(
            s.run("commit").unwrap(),
            Outcome::Command("ROLLBACK".into())
        );
        assert_eq!(s.tx, b'I');
        assert!(s.run("select 1").is_ok());
    }
}
