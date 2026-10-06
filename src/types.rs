//! Runtime types and values.
//!
//! The engine has a small, closed type system. TPC-H's DECIMAL columns are
//! stored as Float64 for now; exact decimals are a later optimization.

use std::fmt;

use crate::ast;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Type {
    /// Type of an untyped NULL literal; coerces to anything.
    Null,
    Boolean,
    Int64,
    Float64,
    Utf8,
    /// Days since 1970-01-01.
    Date,
    /// Only appears in expressions (`date - INTERVAL '90' DAY`), never in tables.
    Interval,
}

impl Type {
    pub fn is_numeric(self) -> bool {
        matches!(self, Type::Int64 | Type::Float64)
    }

    pub fn from_ast(dt: &ast::DataType) -> Type {
        match dt {
            ast::DataType::Integer | ast::DataType::BigInt => Type::Int64,
            ast::DataType::Double | ast::DataType::Decimal { .. } => Type::Float64,
            ast::DataType::Varchar(_) => Type::Utf8,
            ast::DataType::Boolean => Type::Boolean,
            ast::DataType::Date => Type::Date,
        }
    }

    /// Implicit conversions the binder may insert without an explicit CAST.
    pub fn can_coerce_to(self, to: Type) -> bool {
        self == to || self == Type::Null || (self == Type::Int64 && to == Type::Float64)
    }

    /// The type both sides of an operator should be converted to, if any.
    pub fn common(a: Type, b: Type) -> Option<Type> {
        if a.can_coerce_to(b) {
            Some(b)
        } else if b.can_coerce_to(a) {
            Some(a)
        } else {
            None
        }
    }
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Type::Null => "NULL",
            Type::Boolean => "BOOLEAN",
            Type::Int64 => "BIGINT",
            Type::Float64 => "DOUBLE",
            Type::Utf8 => "VARCHAR",
            Type::Date => "DATE",
            Type::Interval => "INTERVAL",
        };
        write!(f, "{s}")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Boolean(bool),
    Int64(i64),
    Float64(f64),
    Utf8(String),
    Date(i32),
    Interval { months: i32, days: i32 },
}

impl Value {
    pub fn ty(&self) -> Type {
        match self {
            Value::Null => Type::Null,
            Value::Boolean(_) => Type::Boolean,
            Value::Int64(_) => Type::Int64,
            Value::Float64(_) => Type::Float64,
            Value::Utf8(_) => Type::Utf8,
            Value::Date(_) => Type::Date,
            Value::Interval { .. } => Type::Interval,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "NULL"),
            Value::Boolean(b) => write!(f, "{b}"),
            Value::Int64(v) => write!(f, "{v}"),
            Value::Float64(v) => write!(f, "{v}"),
            Value::Utf8(s) => write!(f, "{s}"),
            Value::Date(d) => write!(f, "{}", format_date(*d)),
            Value::Interval { months, days } => write!(f, "{months} mons {days} days"),
        }
    }
}

// ---------------------------------------------------------------------------
// Dates: proleptic Gregorian calendar, stored as days since 1970-01-01.
// Conversions use Howard Hinnant's civil-date algorithms (no dependencies).
// ---------------------------------------------------------------------------

fn is_leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(y) => 29,
        2 => 28,
        _ => 0,
    }
}

pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let (m, d) = (m as i64, d as i64);
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// Parses a strict `YYYY-MM-DD` date, validating month lengths and leap years.
pub fn parse_date(s: &str) -> Option<i32> {
    let mut parts = s.trim().split('-');
    let (y, m, d) = (parts.next()?, parts.next()?, parts.next()?);
    let digits = |p: &str| p.bytes().all(|b| b.is_ascii_digit());
    if parts.next().is_some()
        || (y.len(), m.len(), d.len()) != (4, 2, 2)
        || !(digits(y) && digits(m) && digits(d))
    {
        return None;
    }
    let (y, m, d): (i64, u32, u32) = (y.parse().ok()?, m.parse().ok()?, d.parse().ok()?);
    if !(1..=12).contains(&m) || d == 0 || d > days_in_month(y, m) {
        return None;
    }
    i32::try_from(days_from_civil(y, m, d)).ok()
}

pub fn format_date(days: i32) -> String {
    let (y, m, d) = civil_from_days(days as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_dates() {
        assert_eq!(parse_date("1970-01-01"), Some(0));
        assert_eq!(parse_date("1998-12-01"), Some(10_561));
        assert_eq!(parse_date("2000-02-29"), Some(11_016));
        assert_eq!(format_date(-1), "1969-12-31");
    }

    #[test]
    fn rejects_invalid_dates() {
        for s in [
            "1999-02-29",
            "1900-02-29",
            "2024-13-01",
            "2024-04-31",
            "24-01-01",
            "2024-1-01",
            "abcd-ef-gh",
            "",
        ] {
            assert_eq!(parse_date(s), None, "{s}");
        }
    }

    #[test]
    fn round_trip_every_day_for_four_centuries() {
        let start = days_from_civil(1800, 1, 1);
        let end = days_from_civil(2200, 1, 1);
        for day in start..end {
            let text = format_date(day as i32);
            assert_eq!(parse_date(&text), Some(day as i32), "{text}");
        }
    }
}
