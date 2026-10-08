//! Expression evaluation: computes a `BoundExpr` against one row.
//!
//! Semantics follow Postgres:
//! - Three-valued logic: comparisons with NULL yield NULL; `AND`/`OR` use
//!   the SQL truth tables (`FALSE AND NULL` is FALSE, `TRUE OR NULL` is TRUE).
//! - Integer overflow and division by zero are errors, not wraparound.
//! - Integer division truncates toward zero.
//! - Adding months to a date clamps to the end of the month (Jan 31 + 1 month
//!   = Feb 28/29).
//!
//! Column references are resolved through a chain of `Scope`s: the current
//! row first, then each enclosing query's row. That chain is what makes
//! correlated subqueries work. Running a subquery is delegated to a
//! `SubqueryRunner`, which the executor provides.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;

use crate::ast::{BinaryOp, UnaryOp};
use crate::bound::{BoundExpr, BoundQuery, ColumnId, ExprKind, ScalarFunc, SubqueryKind};
use crate::types::{civil_from_days, days_from_civil, days_in_month, parse_date, Type, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecError(pub String);

impl fmt::Display for ExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ExecError {}

pub type Result<T> = std::result::Result<T, ExecError>;

fn err<T>(msg: impl Into<String>) -> Result<T> {
    Err(ExecError(msg.into()))
}

pub type Row = Vec<Value>;

/// Maps column ids to positions within a row.
#[derive(Debug, Clone, Default)]
pub struct Layout {
    index: HashMap<ColumnId, usize>,
}

impl Layout {
    pub fn new(ids: &[ColumnId]) -> Self {
        Layout {
            index: ids.iter().enumerate().map(|(i, &id)| (id, i)).collect(),
        }
    }

    pub fn get(&self, id: ColumnId) -> Option<usize> {
        self.index.get(&id).copied()
    }
}

/// The row an expression is evaluated against, plus the rows of enclosing
/// queries (for correlated references).
#[derive(Clone, Copy)]
pub struct Scope<'a> {
    pub layout: &'a Layout,
    pub row: &'a [Value],
    pub outer: Option<&'a Scope<'a>>,
}

impl<'a> Scope<'a> {
    pub fn new(layout: &'a Layout, row: &'a [Value], outer: Option<&'a Scope<'a>>) -> Self {
        Scope { layout, row, outer }
    }

    pub fn lookup(&self, id: ColumnId) -> Option<&Value> {
        match self.layout.get(id) {
            Some(i) => self.row.get(i),
            None => self.outer.and_then(|o| o.lookup(id)),
        }
    }
}

/// Executes subqueries on the evaluator's behalf. `outer` is the scope the
/// subquery appears in; correlated references resolve against it. Results
/// are shared (`Rc`) so a runner can cache an uncorrelated subquery's rows
/// and hand them out once per outer row without copying.
pub trait SubqueryRunner {
    fn run(&self, query: &BoundQuery, outer: &Scope<'_>) -> Result<Rc<Vec<Row>>>;
}

/// A runner for contexts with no executor (tests, constant folding).
pub struct NoSubqueries;

impl SubqueryRunner for NoSubqueries {
    fn run(&self, _: &BoundQuery, _: &Scope<'_>) -> Result<Rc<Vec<Row>>> {
        err("subqueries cannot be evaluated in this context")
    }
}

/// Evaluates a predicate; NULL counts as false, as in WHERE and ON.
pub fn eval_predicate(
    e: &BoundExpr,
    scope: &Scope<'_>,
    runner: &dyn SubqueryRunner,
) -> Result<bool> {
    match eval(e, scope, runner)? {
        Value::Boolean(b) => Ok(b),
        Value::Null => Ok(false),
        other => err(format!("internal: predicate produced {}", other.ty())),
    }
}

pub fn eval(e: &BoundExpr, scope: &Scope<'_>, runner: &dyn SubqueryRunner) -> Result<Value> {
    let ev = |x: &BoundExpr| eval(x, scope, runner);
    match &e.kind {
        ExprKind::Column(id) => scope
            .lookup(*id)
            .cloned()
            .ok_or_else(|| ExecError(format!("internal: column #{id} is not available here"))),
        ExprKind::Literal(v) => Ok(v.clone()),
        ExprKind::Unary {
            op: UnaryOp::Not,
            expr,
        } => Ok(not3(ev(expr)?)),
        ExprKind::Unary {
            op: UnaryOp::Plus,
            expr,
        } => ev(expr),
        ExprKind::Unary {
            op: UnaryOp::Minus,
            expr,
        } => negate(ev(expr)?),
        ExprKind::Binary {
            left,
            op: BinaryOp::And,
            right,
        } => {
            let l = ev(left)?;
            if l == Value::Boolean(false) {
                return Ok(l); // short-circuit
            }
            Ok(and3(l, ev(right)?))
        }
        ExprKind::Binary {
            left,
            op: BinaryOp::Or,
            right,
        } => {
            let l = ev(left)?;
            if l == Value::Boolean(true) {
                return Ok(l);
            }
            Ok(or3(l, ev(right)?))
        }
        ExprKind::Binary { left, op, right } => binary(*op, ev(left)?, ev(right)?),
        ExprKind::IsNull { expr, negated } => Ok(Value::Boolean(ev(expr)?.is_null() != *negated)),
        ExprKind::Like {
            expr,
            pattern,
            negated,
        } => {
            let (Value::Utf8(s), Value::Utf8(p)) = (ev(expr)?, ev(pattern)?) else {
                return Ok(Value::Null);
            };
            Ok(Value::Boolean(like(&s, &p)? != *negated))
        }
        ExprKind::InList {
            expr,
            list,
            negated,
        } => {
            let x = ev(expr)?;
            let mut items = Vec::with_capacity(list.len());
            for item in list {
                items.push(ev(item)?);
            }
            Ok(in_values(&x, items.iter(), *negated))
        }
        ExprKind::Case {
            operand,
            branches,
            else_result,
        } => {
            let subject = operand.as_ref().map(|o| ev(o)).transpose()?;
            for (when, then) in branches {
                let hit = match &subject {
                    // `CASE x WHEN v`: a NULL x or v never matches.
                    Some(x) => sql_eq(x, &ev(when)?) == Some(true),
                    None => eval_predicate(when, scope, runner)?,
                };
                if hit {
                    return ev(then);
                }
            }
            match else_result {
                Some(e) => ev(e),
                None => Ok(Value::Null),
            }
        }
        ExprKind::Cast { expr } => cast_value(ev(expr)?, e.ty),
        ExprKind::Function {
            func: ScalarFunc::Coalesce,
            args,
        } => {
            for a in args {
                let v = ev(a)?;
                if !v.is_null() {
                    return Ok(v);
                }
            }
            Ok(Value::Null)
        }
        ExprKind::Function { func, args } => {
            let mut values = Vec::with_capacity(args.len());
            for a in args {
                let v = ev(a)?;
                // Every other function is strict: any NULL argument gives NULL.
                if v.is_null() {
                    return Ok(Value::Null);
                }
                values.push(v);
            }
            call(*func, &values)
        }
        ExprKind::Subquery { query, kind } => match kind {
            SubqueryKind::Scalar => {
                let rows = runner.run(query, scope)?;
                match rows.as_slice() {
                    [] => Ok(Value::Null),
                    [row] => Ok(row.first().cloned().unwrap_or(Value::Null)),
                    _ => err("more than one row returned by a subquery used as an expression"),
                }
            }
            SubqueryKind::Exists { negated } => {
                let rows = runner.run(query, scope)?;
                Ok(Value::Boolean(rows.is_empty() == *negated))
            }
            SubqueryKind::In { expr, negated } => {
                let x = ev(expr)?;
                let rows = runner.run(query, scope)?;
                Ok(in_values(&x, rows.iter().map(|r| &r[0]), *negated))
            }
        },
    }
}

// ---------------------------------------------------------------------------
// Three-valued logic
// ---------------------------------------------------------------------------

fn not3(v: Value) -> Value {
    match v {
        Value::Boolean(b) => Value::Boolean(!b),
        _ => Value::Null,
    }
}

fn and3(l: Value, r: Value) -> Value {
    match (l, r) {
        (Value::Boolean(false), _) | (_, Value::Boolean(false)) => Value::Boolean(false),
        (Value::Boolean(true), Value::Boolean(true)) => Value::Boolean(true),
        _ => Value::Null,
    }
}

fn or3(l: Value, r: Value) -> Value {
    match (l, r) {
        (Value::Boolean(true), _) | (_, Value::Boolean(true)) => Value::Boolean(true),
        (Value::Boolean(false), Value::Boolean(false)) => Value::Boolean(false),
        _ => Value::Null,
    }
}

/// SQL equality: `None` when either side is NULL.
fn sql_eq(a: &Value, b: &Value) -> Option<bool> {
    if a.is_null() || b.is_null() {
        None
    } else {
        Some(compare(a, b) == Ordering::Equal)
    }
}

/// `x IN (items)`: TRUE on a match; otherwise NULL if x or any item is NULL;
/// otherwise FALSE. NOT IN negates that three-valued result.
fn in_values<'v>(x: &Value, items: impl Iterator<Item = &'v Value>, negated: bool) -> Value {
    let result = if x.is_null() {
        Value::Null
    } else {
        let mut saw_null = false;
        let mut found = false;
        for item in items {
            match sql_eq(x, item) {
                Some(true) => {
                    found = true;
                    break;
                }
                Some(false) => {}
                None => saw_null = true,
            }
        }
        if found {
            Value::Boolean(true)
        } else if saw_null {
            Value::Null
        } else {
            Value::Boolean(false)
        }
    };
    if negated {
        not3(result)
    } else {
        result
    }
}

// ---------------------------------------------------------------------------
// Comparison
// ---------------------------------------------------------------------------

fn type_rank(v: &Value) -> u8 {
    match v {
        Value::Null => 0,
        Value::Boolean(_) => 1,
        Value::Int64(_) | Value::Float64(_) => 2,
        Value::Utf8(_) => 3,
        Value::Date(_) => 4,
        Value::Interval { .. } => 5,
    }
}

fn cmp_f64(a: f64, b: f64) -> Ordering {
    // NaN sorts after every other number (Postgres semantics); 0.0 == -0.0.
    a.partial_cmp(&b)
        .unwrap_or_else(|| a.is_nan().cmp(&b.is_nan()))
}

/// A total order over non-NULL values of compatible types. The binder makes
/// both sides of every comparison the same type, so the cross-type arms are
/// only a safety net that keeps sorting from panicking.
pub fn compare(a: &Value, b: &Value) -> Ordering {
    use Value::*;
    match (a, b) {
        (Boolean(x), Boolean(y)) => x.cmp(y),
        (Int64(x), Int64(y)) => x.cmp(y),
        (Float64(x), Float64(y)) => cmp_f64(*x, *y),
        (Int64(x), Float64(y)) => cmp_f64(*x as f64, *y),
        (Float64(x), Int64(y)) => cmp_f64(*x, *y as f64),
        (Utf8(x), Utf8(y)) => x.cmp(y),
        (Date(x), Date(y)) => x.cmp(y),
        (
            Interval {
                months: m1,
                days: d1,
            },
            Interval {
                months: m2,
                days: d2,
            },
        ) => {
            // Postgres compares intervals assuming 30-day months.
            (*m1 as i64 * 30 + *d1 as i64).cmp(&(*m2 as i64 * 30 + *d2 as i64))
        }
        _ => type_rank(a).cmp(&type_rank(b)),
    }
}

// ---------------------------------------------------------------------------
// Arithmetic
// ---------------------------------------------------------------------------

fn overflow<T>(what: &str) -> Result<T> {
    err(format!("{what} out of range"))
}

fn negate(v: Value) -> Result<Value> {
    Ok(match v {
        Value::Null => Value::Null,
        Value::Int64(x) => Value::Int64(x.checked_neg().map_or_else(|| overflow("bigint"), Ok)?),
        Value::Float64(x) => Value::Float64(-x),
        Value::Interval { months, days } => Value::Interval {
            months: -months,
            days: -days,
        },
        other => return err(format!("internal: cannot negate {}", other.ty())),
    })
}

fn date_value(days: i64) -> Result<Value> {
    i32::try_from(days)
        .map(Value::Date)
        .or_else(|_| overflow("date"))
}

/// Adds an interval to a date. Months are applied first, clamping the day to
/// the target month's length; then days are added.
pub fn add_interval(date: i32, months: i32, days: i32) -> Result<Value> {
    let (y, m, d) = civil_from_days(date as i64);
    let total = y * 12 + (m as i64 - 1) + months as i64;
    let (ny, nm) = (total.div_euclid(12), (total.rem_euclid(12) + 1) as u32);
    let nd = d.min(days_in_month(ny, nm));
    date_value(days_from_civil(ny, nm, nd) + days as i64)
}

fn binary(op: BinaryOp, l: Value, r: Value) -> Result<Value> {
    use BinaryOp::*;
    use Value::*;
    if l.is_null() || r.is_null() {
        return Ok(Null);
    }
    match op {
        Eq | NotEq | Lt | LtEq | Gt | GtEq => {
            let ord = compare(&l, &r);
            let b = match op {
                Eq => ord == Ordering::Equal,
                NotEq => ord != Ordering::Equal,
                Lt => ord == Ordering::Less,
                LtEq => ord != Ordering::Greater,
                Gt => ord == Ordering::Greater,
                _ => ord != Ordering::Less,
            };
            return Ok(Boolean(b));
        }
        Concat => {
            let (Utf8(a), Utf8(b)) = (l, r) else {
                return err("internal: || on non-text values");
            };
            return Ok(Utf8(a + &b));
        }
        _ => {}
    }
    let result = match (op, l, r) {
        (Plus, Int64(a), Int64(b)) => {
            Int64(a.checked_add(b).map_or_else(|| overflow("bigint"), Ok)?)
        }
        (Minus, Int64(a), Int64(b)) => {
            Int64(a.checked_sub(b).map_or_else(|| overflow("bigint"), Ok)?)
        }
        (Multiply, Int64(a), Int64(b)) => {
            Int64(a.checked_mul(b).map_or_else(|| overflow("bigint"), Ok)?)
        }
        (Divide | Modulo, Int64(_), Int64(0)) => return err("division by zero"),
        // checked_* also catches i64::MIN / -1.
        (Divide, Int64(a), Int64(b)) => {
            Int64(a.checked_div(b).map_or_else(|| overflow("bigint"), Ok)?)
        }
        (Modulo, Int64(a), Int64(b)) => Int64(a.checked_rem(b).unwrap_or(0)),
        (Plus, Float64(a), Float64(b)) => Float64(a + b),
        (Minus, Float64(a), Float64(b)) => Float64(a - b),
        (Multiply, Float64(a), Float64(b)) => Float64(a * b),
        (Divide | Modulo, Float64(_), Float64(0.0)) => return err("division by zero"),
        (Divide, Float64(a), Float64(b)) => Float64(a / b),
        (Modulo, Float64(a), Float64(b)) => Float64(a % b),
        (Plus, Date(d), Interval { months, days }) | (Plus, Interval { months, days }, Date(d)) => {
            add_interval(d, months, days)?
        }
        (Minus, Date(d), Interval { months, days }) => add_interval(d, -months, -days)?,
        (Plus, Date(d), Int64(n)) | (Plus, Int64(n), Date(d)) => date_value(d as i64 + n)?,
        (Minus, Date(d), Int64(n)) => date_value(d as i64 - n)?,
        (Minus, Date(a), Date(b)) => Int64(a as i64 - b as i64),
        (
            Plus,
            Interval {
                months: m1,
                days: d1,
            },
            Interval {
                months: m2,
                days: d2,
            },
        ) => Interval {
            months: m1
                .checked_add(m2)
                .map_or_else(|| overflow("interval"), Ok)?,
            days: d1
                .checked_add(d2)
                .map_or_else(|| overflow("interval"), Ok)?,
        },
        (
            Minus,
            Interval {
                months: m1,
                days: d1,
            },
            Interval {
                months: m2,
                days: d2,
            },
        ) => Interval {
            months: m1
                .checked_sub(m2)
                .map_or_else(|| overflow("interval"), Ok)?,
            days: d1
                .checked_sub(d2)
                .map_or_else(|| overflow("interval"), Ok)?,
        },
        (op, l, r) => {
            return err(format!(
                "internal: no implementation for {} {op} {}",
                l.ty(),
                r.ty()
            ))
        }
    };
    Ok(result)
}

// ---------------------------------------------------------------------------
// LIKE
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
enum Pat {
    Lit(char),
    One,
    Any,
}

/// SQL LIKE: `%` matches any run of characters, `_` exactly one, and `\`
/// escapes the next character. Linear-time greedy matching with
/// backtracking to the most recent `%`.
pub fn like(text: &str, pattern: &str) -> Result<bool> {
    let mut pat = Vec::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        pat.push(match c {
            '%' => Pat::Any,
            '_' => Pat::One,
            '\\' => match chars.next() {
                Some(e) => Pat::Lit(e),
                None => return err("LIKE pattern must not end with escape character"),
            },
            c => Pat::Lit(c),
        });
    }
    let text: Vec<char> = text.chars().collect();
    let (mut t, mut p) = (0, 0);
    let mut star: Option<usize> = None;
    let mut mark = 0;
    while t < text.len() {
        let step = p < pat.len()
            && match pat[p] {
                Pat::One => true,
                Pat::Lit(c) => c == text[t],
                Pat::Any => false,
            };
        if step {
            t += 1;
            p += 1;
        } else if p < pat.len() && pat[p] == Pat::Any {
            star = Some(p);
            mark = t;
            p += 1;
        } else if let Some(s) = star {
            // Let the last % absorb one more character and retry.
            p = s + 1;
            mark += 1;
            t = mark;
        } else {
            return Ok(false);
        }
    }
    while p < pat.len() && pat[p] == Pat::Any {
        p += 1;
    }
    Ok(p == pat.len())
}

// ---------------------------------------------------------------------------
// Casts
// ---------------------------------------------------------------------------

fn invalid<T>(ty: Type, s: &str) -> Result<T> {
    err(format!(
        "invalid input syntax for type {}: \"{s}\"",
        ty.to_string().to_ascii_lowercase()
    ))
}

pub fn cast_value(v: Value, to: Type) -> Result<Value> {
    use Value::*;
    if v.is_null() || v.ty() == to {
        return Ok(v);
    }
    Ok(match (v, to) {
        (Int64(x), Type::Float64) => Float64(x as f64),
        (Float64(x), Type::Int64) => {
            // Postgres rounds half away from zero when casting to integer.
            let r = x.round();
            if !r.is_finite() || r < i64::MIN as f64 || r >= i64::MAX as f64 {
                return overflow("bigint");
            }
            Int64(r as i64)
        }
        (Boolean(b), Type::Int64) => Int64(b as i64),
        (Int64(x), Type::Boolean) => Boolean(x != 0),
        (Utf8(s), Type::Int64) => s.trim().parse().map(Int64).or_else(|_| invalid(to, &s))?,
        (Utf8(s), Type::Float64) => s.trim().parse().map(Float64).or_else(|_| invalid(to, &s))?,
        (Utf8(s), Type::Date) => parse_date(&s)
            .map(Date)
            .map_or_else(|| invalid(to, &s), Ok)?,
        (Utf8(s), Type::Boolean) => match s.trim().to_ascii_lowercase().as_str() {
            "t" | "true" | "yes" | "y" | "on" | "1" => Boolean(true),
            "f" | "false" | "no" | "n" | "off" | "0" => Boolean(false),
            _ => return invalid(to, &s),
        },
        (v, Type::Utf8) => Utf8(v.to_string()),
        (v, to) => return err(format!("internal: cannot cast {} to {to}", v.ty())),
    })
}

// ---------------------------------------------------------------------------
// Scalar functions (all arguments are non-NULL here)
// ---------------------------------------------------------------------------

fn substring(s: &str, start: i64, len: Option<i64>) -> Result<Value> {
    // Positions are 1-based and may start before 1: substring('hello', 0, 3) = 'he'.
    let end = match len {
        Some(l) if l < 0 => return err("negative substring length not allowed"),
        Some(l) => start.saturating_add(l),
        None => i64::MAX,
    };
    let from = start.max(1);
    if end <= from {
        return Ok(Value::Utf8(String::new()));
    }
    let skip = (from - 1) as usize;
    let take = usize::try_from(end - from).unwrap_or(usize::MAX);
    Ok(Value::Utf8(s.chars().skip(skip).take(take).collect()))
}

fn round(x: f64, digits: i64) -> f64 {
    let digits = digits.clamp(-300, 300) as i32;
    let scale = 10f64.powi(digits);
    let r = (x * scale).round() / scale;
    if r.is_finite() {
        r
    } else {
        x
    }
}

fn call(func: ScalarFunc, args: &[Value]) -> Result<Value> {
    use Value::*;
    Ok(match (func, args) {
        (ScalarFunc::Substring, [Utf8(s), Int64(start)]) => substring(s, *start, None)?,
        (ScalarFunc::Substring, [Utf8(s), Int64(start), Int64(len)]) => {
            substring(s, *start, Some(*len))?
        }
        (ScalarFunc::Upper, [Utf8(s)]) => Utf8(s.to_uppercase()),
        (ScalarFunc::Lower, [Utf8(s)]) => Utf8(s.to_lowercase()),
        (ScalarFunc::Length, [Utf8(s)]) => Int64(s.chars().count() as i64),
        (ScalarFunc::Abs, [Int64(x)]) => {
            Int64(x.checked_abs().map_or_else(|| overflow("bigint"), Ok)?)
        }
        (ScalarFunc::Abs, [Float64(x)]) => Float64(x.abs()),
        (ScalarFunc::Round, [Float64(x)]) => Float64(round(*x, 0)),
        (ScalarFunc::Round, [Float64(x), Int64(d)]) => Float64(round(*x, *d)),
        (ScalarFunc::ExtractYear, [Date(d)]) => Int64(civil_from_days(*d as i64).0),
        (ScalarFunc::ExtractMonth, [Date(d)]) => Int64(civil_from_days(*d as i64).1 as i64),
        (ScalarFunc::ExtractDay, [Date(d)]) => Int64(civil_from_days(*d as i64).2 as i64),
        (func, args) => {
            let tys: Vec<String> = args.iter().map(|a| a.ty().to_string()).collect();
            return err(format!(
                "internal: no implementation for {}({})",
                func.name(),
                tys.join(", ")
            ));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_patterns() {
        let cases = [
            ("hello", "h%", true),
            ("hello", "%llo", true),
            ("hello", "h_llo", true),
            ("hello", "h_lo", false),
            ("hello", "%", true),
            ("", "%", true),
            ("", "_", false),
            ("abcabc", "%abc", true),
            ("aaab", "%a%b", true),
            ("100%", "100\\%", true),
            ("100x", "100\\%", false),
            ("a_b", "a\\_b", true),
            ("special requests", "%special%requests%", true),
            ("specialrequest", "%special%requests%", false),
            ("ünïcode", "_n_code", true),
        ];
        for (text, pat, expected) in cases {
            assert_eq!(like(text, pat).unwrap(), expected, "{text:?} LIKE {pat:?}");
        }
        assert!(like("x", "x\\").is_err());
    }

    #[test]
    fn month_arithmetic_clamps() {
        let d = |s: &str| parse_date(s).unwrap();
        assert_eq!(
            add_interval(d("2024-01-31"), 1, 0).unwrap(),
            Value::Date(d("2024-02-29"))
        );
        assert_eq!(
            add_interval(d("2023-01-31"), 1, 0).unwrap(),
            Value::Date(d("2023-02-28"))
        );
        assert_eq!(
            add_interval(d("2024-03-31"), -1, 0).unwrap(),
            Value::Date(d("2024-02-29"))
        );
        assert_eq!(
            add_interval(d("1994-01-01"), 12, 0).unwrap(),
            Value::Date(d("1995-01-01"))
        );
        assert_eq!(
            add_interval(d("1998-12-01"), 0, -90).unwrap(),
            Value::Date(d("1998-09-02"))
        );
        assert!(add_interval(0, i32::MAX, 0).is_err());
    }

    #[test]
    fn substring_edges() {
        let s = |st, len| substring("hello", st, len).unwrap();
        assert_eq!(s(1, Some(2)), Value::Utf8("he".into()));
        assert_eq!(s(0, Some(3)), Value::Utf8("he".into()));
        assert_eq!(s(-5, Some(3)), Value::Utf8(String::new()));
        assert_eq!(s(4, None), Value::Utf8("lo".into()));
        assert_eq!(s(10, Some(2)), Value::Utf8(String::new()));
        assert_eq!(s(i64::MAX, Some(i64::MAX)), Value::Utf8(String::new()));
        assert!(substring("hello", 1, Some(-1)).is_err());
    }
}
