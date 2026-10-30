//! The extended query protocol, which drivers like psycopg use for every
//! query: Parse names a statement with `$n` placeholders, Bind supplies the
//! parameter values to make a portal, Describe reports result columns, and
//! Execute runs the portal. Sync ends the sequence.
//!
//! After an error the server discards messages until the next Sync, so the
//! client can pipeline a whole sequence without waiting for replies.

use std::collections::HashMap;
use std::io;

use super::protocol::{
    command_complete, empty_query_response, error_response, ready_for_query, Body, Out, PgError,
};
use super::session::{data_row, row_description, syntax_error, Outcome, Session};
use super::sql::{self, control_command, param_count, split_statements, substitute_params, Param};
use crate::catalog::Field;
use crate::types::format_date;
use crate::{bind, parse_statements};

/// A parsed statement, waiting for parameter values.
#[derive(Debug, Clone)]
pub struct Prepared {
    sql: String,
    /// One type OID per parameter; 0 means the client left it unspecified.
    param_types: Vec<i32>,
}

/// A statement with its parameters bound. It runs when first described or
/// executed, and its rows are handed out across Execute calls.
#[derive(Debug, Clone)]
pub struct Portal {
    sql: String,
    outcome: Option<Outcome>,
    sent: usize,
}

#[derive(Debug, Default)]
pub struct Extended {
    statements: HashMap<String, Prepared>,
    portals: HashMap<String, Portal>,
    /// Set after an error: messages are ignored until Sync.
    skipping: bool,
}

fn protocol_error(e: io::Error) -> PgError {
    PgError::new("08P01", e.to_string())
}

impl Session {
    /// Handles one extended-protocol message (P, B, D, E, C, H or S).
    pub(super) fn extended(&mut self, tag: u8, body: &[u8], out: &mut Out) {
        if tag == b'S' {
            self.ext.skipping = false;
            if self.tx == b'I' {
                // Outside a transaction block, portals last until Sync.
                self.ext.portals.clear();
            }
            ready_for_query(out, self.tx);
            return;
        }
        if self.ext.skipping {
            return;
        }
        let mut b = Body::new(body);
        let result = match tag {
            b'P' => self.parse(&mut b, out),
            b'B' => self.bind_portal(&mut b, out),
            b'D' => self.describe(&mut b, out),
            b'E' => self.execute_portal(&mut b, out),
            b'C' => self.close(&mut b, out),
            _ => Ok(()), // Flush: replies are already flushed after every message.
        };
        if let Err(e) = result {
            error_response(out, &e);
            self.ext.skipping = true;
            if self.tx == b'T' {
                self.tx = b'E';
            }
        }
    }

    fn parse(&mut self, b: &mut Body, out: &mut Out) -> Result<(), PgError> {
        let name = b.cstr().map_err(protocol_error)?;
        let query = b.cstr().map_err(protocol_error)?;
        let n = b.i16().map_err(protocol_error)?;
        let mut param_types = Vec::new();
        for _ in 0..n {
            param_types.push(b.i32().map_err(protocol_error)?);
        }
        if !name.is_empty() && self.ext.statements.contains_key(&name) {
            return Err(PgError::new(
                "42P05",
                format!("prepared statement \"{name}\" already exists"),
            ));
        }
        let statements = split_statements(&query);
        if statements.len() > 1 {
            return Err(PgError::new(
                "42601",
                "cannot insert multiple commands into a prepared statement",
            ));
        }
        // Check the syntax now, as Postgres does, with NULL for every
        // parameter. Positions are only exact when there are no parameters.
        if let Some(&(_, stmt)) = statements.first() {
            if control_command(stmt).is_none() {
                let nulls = null_params(param_count(stmt).max(param_types.len()));
                let text = substitute_params(stmt, &nulls).map_err(|m| PgError::new("42P02", m))?;
                if let Err(e) = parse_statements(&text) {
                    let mut e = syntax_error(&text, &e);
                    if !nulls.is_empty() {
                        e.position = None;
                    }
                    return Err(e);
                }
            }
        }
        let sql = statements.first().map_or("", |&(_, s)| s).to_string();
        self.ext
            .statements
            .insert(name, Prepared { sql, param_types });
        out.msg(b'1', |_| {}); // ParseComplete
        Ok(())
    }

    fn bind_portal(&mut self, b: &mut Body, out: &mut Out) -> Result<(), PgError> {
        let portal = b.cstr().map_err(protocol_error)?;
        let name = b.cstr().map_err(protocol_error)?;
        let stmt = self.ext.statements.get(&name).ok_or_else(|| {
            PgError::new(
                "26000",
                format!("prepared statement \"{name}\" does not exist"),
            )
        })?;

        // Format codes: none means all text, one applies to every parameter,
        // otherwise one per parameter. 0 is text, 1 is binary.
        let n_formats = b.i16().map_err(protocol_error)?;
        let mut formats = Vec::new();
        for _ in 0..n_formats {
            formats.push(b.i16().map_err(protocol_error)?);
        }
        let n_params = b.i16().map_err(protocol_error)? as usize;
        let mut params = Vec::with_capacity(n_params);
        for i in 0..n_params {
            let type_oid = stmt.param_types.get(i).copied().unwrap_or(0);
            let binary = match formats.as_slice() {
                [] => false,
                [f] => *f == 1,
                fs => fs.get(i).copied() == Some(1),
            };
            let value = match b.i32().map_err(protocol_error)? {
                -1 => None,
                len => {
                    let bytes = b.bytes(len.max(0) as usize).map_err(protocol_error)?;
                    Some(if binary {
                        binary_param(type_oid, bytes)?
                    } else {
                        std::str::from_utf8(bytes)
                            .map_err(|_| PgError::new("22021", "parameter is not valid UTF-8"))?
                            .to_string()
                    })
                }
            };
            params.push(Param { type_oid, value });
        }
        let n_results = b.i16().map_err(protocol_error)?;
        for _ in 0..n_results {
            if b.i16().map_err(protocol_error)? != 0 {
                return Err(PgError::new("0A000", "binary results are not supported"));
            }
        }

        let needed = param_count(&stmt.sql);
        if params.len() < needed {
            return Err(PgError::new(
                "08P01",
                format!(
                    "bind message supplies {} parameters, but the statement requires {needed}",
                    params.len()
                ),
            ));
        }
        let sql = substitute_params(&stmt.sql, &params).map_err(|m| PgError::new("22P02", m))?;
        self.ext.portals.insert(
            portal,
            Portal {
                sql,
                outcome: None,
                sent: 0,
            },
        );
        out.msg(b'2', |_| {}); // BindComplete
        Ok(())
    }

    /// Runs a portal's statement if it hasn't run yet.
    fn run_portal(&mut self, name: &str) -> Result<&mut Portal, PgError> {
        let sql = match self.ext.portals.get(name) {
            None => {
                return Err(PgError::new(
                    "34000",
                    format!("portal \"{name}\" does not exist"),
                ))
            }
            Some(p) if p.outcome.is_some() => None,
            Some(p) => Some(p.sql.clone()),
        };
        if let Some(sql) = sql {
            let outcome = if split_statements(&sql).is_empty() {
                Outcome::Command(String::new())
            } else {
                self.run(&sql)?
            };
            self.ext.portals.get_mut(name).unwrap().outcome = Some(outcome);
        }
        Ok(self.ext.portals.get_mut(name).unwrap())
    }

    fn describe(&mut self, b: &mut Body, out: &mut Out) -> Result<(), PgError> {
        let kind = b.u8().map_err(protocol_error)?;
        let name = b.cstr().map_err(protocol_error)?;
        if kind == b'P' {
            let portal = self.run_portal(&name)?;
            match &portal.outcome {
                Some(Outcome::Rows { columns, .. }) => row_description(out, columns),
                _ => out.msg(b'n', |_| {}), // NoData
            }
            return Ok(());
        }
        let stmt = self.ext.statements.get(&name).ok_or_else(|| {
            PgError::new(
                "26000",
                format!("prepared statement \"{name}\" does not exist"),
            )
        })?;
        let n = param_count(&stmt.sql).max(stmt.param_types.len());
        out.msg(b't', |w| {
            // ParameterDescription; unspecified types are reported as text.
            w.i16(n as i16);
            for i in 0..n {
                let oid = stmt.param_types.get(i).copied().unwrap_or(0);
                w.i32(if oid == 0 { sql::TEXT } else { oid });
            }
        });
        match statement_columns(self, &stmt.sql, n) {
            Some(columns) => row_description(out, &columns),
            None => out.msg(b'n', |_| {}),
        }
        Ok(())
    }

    fn execute_portal(&mut self, b: &mut Body, out: &mut Out) -> Result<(), PgError> {
        let name = b.cstr().map_err(protocol_error)?;
        let max_rows = b.i32().map_err(protocol_error)?;
        let portal = self.run_portal(&name)?;
        match portal.outcome.as_ref().unwrap() {
            Outcome::Rows { rows, .. } => {
                let left = rows.len() - portal.sent;
                let n = if max_rows > 0 {
                    left.min(max_rows as usize)
                } else {
                    left
                };
                for row in &rows[portal.sent..portal.sent + n] {
                    data_row(out, row);
                }
                portal.sent += n;
                if portal.sent < rows.len() {
                    out.msg(b's', |_| {}); // PortalSuspended: more rows remain
                } else {
                    command_complete(out, &format!("SELECT {}", rows.len()));
                }
            }
            Outcome::Command(tag) if tag.is_empty() => empty_query_response(out),
            Outcome::Command(tag) => command_complete(out, tag),
        }
        Ok(())
    }

    fn close(&mut self, b: &mut Body, out: &mut Out) -> Result<(), PgError> {
        let kind = b.u8().map_err(protocol_error)?;
        let name = b.cstr().map_err(protocol_error)?;
        if kind == b'S' {
            self.ext.statements.remove(&name);
        } else {
            self.ext.portals.remove(&name);
        }
        out.msg(b'3', |_| {}); // CloseComplete
        Ok(())
    }
}

/// Converts a parameter sent in binary format to its text form. Drivers
/// such as psycopg send numbers, booleans, and dates this way.
fn binary_param(type_oid: i32, b: &[u8]) -> Result<String, PgError> {
    let bad = || PgError::new("22P03", format!("invalid binary value for type {type_oid}"));
    let int = |n: usize| -> Result<i64, PgError> {
        if b.len() != n {
            return Err(bad());
        }
        Ok(b.iter().fold(i64::from(b[0] as i8 >> 7), |acc, &x| {
            (acc << 8) | i64::from(x)
        }))
    };
    Ok(match type_oid {
        sql::BOOL => match b {
            [0] => "f".into(),
            [1] => "t".into(),
            _ => return Err(bad()),
        },
        sql::INT2 => int(2)?.to_string(),
        sql::INT4 => int(4)?.to_string(),
        sql::INT8 => int(8)?.to_string(),
        sql::FLOAT4 => f32::from_bits(int(4)? as u32).to_string(),
        sql::FLOAT8 => f64::from_bits(int(8)? as u64).to_string(),
        // Days since 2000-01-01, Postgres's epoch.
        sql::DATE => format_date(i32::try_from(int(4)? + 10_957).map_err(|_| bad())?),
        sql::NUMERIC => numeric(b).ok_or_else(bad)?,
        sql::TEXT | sql::VARCHAR | 0 => std::str::from_utf8(b)
            .map_err(|_| PgError::new("22021", "parameter is not valid UTF-8"))?
            .to_string(),
        other => {
            return Err(PgError::new(
                "0A000",
                format!("binary parameters of type {other} are not supported"),
            ))
        }
    })
}

/// Decodes Postgres's binary NUMERIC: a digit count, the weight (power of
/// 10000) of the first digit, a sign, the display scale, then base-10000
/// digits.
fn numeric(b: &[u8]) -> Option<String> {
    let word = |i: usize| -> Option<i32> {
        Some(i32::from(i16::from_be_bytes(
            b.get(2 * i..2 * i + 2)?.try_into().ok()?,
        )))
    };
    let (ndigits, weight, sign, dscale) = (word(0)?, word(1)?, word(2)? as u16, word(3)?);
    if ndigits < 0 || b.len() != 8 + 2 * ndigits as usize || dscale < 0 {
        return None;
    }
    if sign == 0xC000 {
        return Some("NaN".into());
    }
    let digits: Vec<i32> = (0..ndigits as usize)
        .map(|i| word(4 + i))
        .collect::<Option<_>>()?;
    let digit = |pos: i32| -> i32 {
        // The base-10000 digit with weight `pos`, or 0 outside the stored ones.
        let i = weight - pos;
        if i >= 0 && (i as usize) < digits.len() {
            digits[i as usize]
        } else {
            0
        }
    };
    let mut s = String::new();
    if sign == 0x4000 {
        s.push('-');
    }
    let mut int = String::new();
    for pos in (0..=weight.max(0)).rev() {
        int.push_str(&format!("{:04}", digit(pos)));
    }
    let int = int.trim_start_matches('0');
    s.push_str(if int.is_empty() { "0" } else { int });
    if dscale > 0 {
        let mut frac = String::new();
        let groups = (dscale + 3) / 4;
        for k in 1..=groups {
            frac.push_str(&format!("{:04}", digit(-k)));
        }
        s.push('.');
        s.push_str(&frac[..dscale as usize]);
    }
    Some(s)
}

fn null_params(n: usize) -> Vec<Param> {
    vec![
        Param {
            type_oid: 0,
            value: None,
        };
        n
    ]
}

/// The result columns of a statement, found by binding it with NULL for
/// every parameter. `None` for commands, and for queries that only bind once
/// real values are known.
fn statement_columns(session: &Session, sql: &str, n_params: usize) -> Option<Vec<Field>> {
    if split_statements(sql).is_empty() || control_command(sql).is_some() {
        return None;
    }
    let text = substitute_params(sql, &null_params(n_params)).ok()?;
    let statements = parse_statements(&text).ok()?;
    let [crate::ast::Statement::Query(query)] = statements.as_slice() else {
        return None;
    };
    let bound = bind(session.catalog(), query).ok()?;
    Some(
        bound
            .query
            .outputs()
            .iter()
            .map(|o| Field {
                name: o.name.clone(),
                ty: o.ty(),
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numeric_bytes(words: &[i16]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_be_bytes()).collect()
    }

    #[test]
    fn binary_numbers_and_dates() {
        assert_eq!(
            binary_param(sql::INT4, &(-7i32).to_be_bytes()).unwrap(),
            "-7"
        );
        assert_eq!(
            binary_param(sql::INT8, &(1i64 << 40).to_be_bytes()).unwrap(),
            "1099511627776"
        );
        assert_eq!(
            binary_param(sql::FLOAT8, &2.5f64.to_be_bytes()).unwrap(),
            "2.5"
        );
        assert_eq!(binary_param(sql::BOOL, &[1]).unwrap(), "t");
        assert_eq!(
            binary_param(sql::DATE, &0i32.to_be_bytes()).unwrap(),
            "2000-01-01"
        );
        assert!(binary_param(sql::INT4, &[0, 1]).is_err());
    }

    #[test]
    fn binary_numeric() {
        // 12345.678: digits 1, 2345, 6780 with weight 1 and scale 3.
        let b = numeric_bytes(&[3, 1, 0, 3, 1, 2345, 6780]);
        assert_eq!(numeric(&b).unwrap(), "12345.678");
        // -0.05: one digit 500 at weight -1, scale 2.
        let b = numeric_bytes(&[1, -1, 0x4000, 2, 500]);
        assert_eq!(numeric(&b).unwrap(), "-0.05");
        // 20000: one digit 2 at weight 1.
        assert_eq!(numeric(&numeric_bytes(&[1, 1, 0, 0, 2])).unwrap(), "20000");
        assert_eq!(numeric(&numeric_bytes(&[0, 0, 0, 0])).unwrap(), "0");
    }
}
