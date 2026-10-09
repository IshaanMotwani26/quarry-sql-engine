//! Aggregate accumulators and hashable group keys.
//!
//! Each aggregate call in a group owns one `AggState`. The executor feeds it
//! one value per input row (`update`) and reads the result once at the end
//! (`finish`). Semantics follow Postgres:
//!
//! - `count(*)` counts rows; every other aggregate ignores NULL inputs.
//! - Over zero non-NULL inputs, `count` is 0 and every other aggregate is NULL.
//! - `sum(bigint)` errors on overflow instead of wrapping.
//! - `DISTINCT` aggregates see each distinct non-NULL value once.
//!
//! Floating-point `sum` and `avg` use Neumaier's compensated summation. Plain
//! `+=` loses low-order bits as the running total grows: summing 0.1 ten times
//! gives 0.9999999999999999. Neumaier tracks the lost bits in a second
//! accumulator and adds them back at the end, which gives exactly 1.0. The
//! cost is one extra add per value.

use std::collections::HashSet;

use crate::bound::{AggFunc, AggregateCall};
use crate::eval::{compare, ExecError, Result};
use crate::types::{Type, Value};

/// A hashable stand-in for `Value`, used for GROUP BY keys, DISTINCT, and
/// (in Phase 4b) hash-join keys. Floats hash by bit pattern after
/// normalizing -0.0 to 0.0 and every NaN to one NaN, so values that compare
/// equal also hash equal. NULLs hash equal to each other, which is what
/// GROUP BY and DISTINCT need (all NULLs form one group).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum KeyValue {
    Null,
    Boolean(bool),
    Int64(i64),
    Float64(u64),
    Utf8(String),
    Date(i32),
    Interval(i32, i32),
}

impl KeyValue {
    pub fn of(v: &Value) -> KeyValue {
        match v {
            Value::Null => KeyValue::Null,
            Value::Boolean(b) => KeyValue::Boolean(*b),
            Value::Int64(x) => KeyValue::Int64(*x),
            Value::Float64(x) => {
                let x = if *x == 0.0 {
                    0.0
                } else if x.is_nan() {
                    f64::NAN
                } else {
                    *x
                };
                KeyValue::Float64(x.to_bits())
            }
            Value::Utf8(s) => KeyValue::Utf8(s.clone()),
            Value::Date(d) => KeyValue::Date(*d),
            Value::Interval { months, days } => KeyValue::Interval(*months, *days),
        }
    }
}

/// Neumaier compensated sum.
#[derive(Debug, Clone, Copy, Default)]
struct CompensatedSum {
    sum: f64,
    compensation: f64,
}

impl CompensatedSum {
    fn add(&mut self, x: f64) {
        let t = self.sum + x;
        if self.sum.abs() >= x.abs() {
            self.compensation += (self.sum - t) + x;
        } else {
            self.compensation += (x - t) + self.sum;
        }
        self.sum = t;
    }

    fn value(&self) -> f64 {
        // Once the sum is infinite or NaN the compensation is meaningless
        // (inf - inf = NaN), so report the plain sum.
        if self.sum.is_finite() {
            self.sum + self.compensation
        } else {
            self.sum
        }
    }
}

#[derive(Debug, Clone)]
enum Accumulator {
    CountStar(i64),
    Count(i64),
    SumInt(Option<i64>),
    SumFloat { sum: CompensatedSum, seen: bool },
    Avg { sum: CompensatedSum, count: i64 },
    Min(Option<Value>),
    Max(Option<Value>),
}

#[derive(Debug, Clone)]
pub struct AggState {
    acc: Accumulator,
    /// Values already seen, for DISTINCT aggregates.
    seen: Option<HashSet<KeyValue>>,
}

impl AggState {
    pub fn new(call: &AggregateCall) -> AggState {
        let acc = match call.func {
            AggFunc::CountStar => Accumulator::CountStar(0),
            AggFunc::Count => Accumulator::Count(0),
            AggFunc::Sum if call.ty == Type::Float64 => Accumulator::SumFloat {
                sum: CompensatedSum::default(),
                seen: false,
            },
            AggFunc::Sum => Accumulator::SumInt(None),
            AggFunc::Avg => Accumulator::Avg {
                sum: CompensatedSum::default(),
                count: 0,
            },
            AggFunc::Min => Accumulator::Min(None),
            AggFunc::Max => Accumulator::Max(None),
        };
        AggState {
            acc,
            seen: call.distinct.then(HashSet::new),
        }
    }

    /// Feeds one input row's value. `None` means the aggregate has no
    /// argument (`count(*)`).
    pub fn update(&mut self, value: Option<Value>) -> Result<()> {
        let Some(v) = value else {
            if let Accumulator::CountStar(n) = &mut self.acc {
                *n += 1;
            }
            return Ok(());
        };
        if v.is_null() {
            return Ok(());
        }
        if let Some(seen) = &mut self.seen {
            if !seen.insert(KeyValue::of(&v)) {
                return Ok(());
            }
        }
        match (&mut self.acc, v) {
            (Accumulator::CountStar(n) | Accumulator::Count(n), _) => *n += 1,
            (Accumulator::SumInt(total), Value::Int64(x)) => {
                let next = total.unwrap_or(0).checked_add(x);
                *total = Some(next.ok_or_else(|| ExecError("bigint out of range".into()))?);
            }
            (Accumulator::SumFloat { sum, seen }, Value::Float64(x)) => {
                sum.add(x);
                *seen = true;
            }
            (Accumulator::Avg { sum, count }, Value::Float64(x)) => {
                sum.add(x);
                *count += 1;
            }
            (Accumulator::Min(best), v) => {
                if best.as_ref().is_none_or(|b| compare(&v, b).is_lt()) {
                    *best = Some(v);
                }
            }
            (Accumulator::Max(best), v) => {
                if best.as_ref().is_none_or(|b| compare(&v, b).is_gt()) {
                    *best = Some(v);
                }
            }
            (acc, v) => {
                return Err(ExecError(format!(
                    "internal: aggregate {acc:?} received a {} value",
                    v.ty()
                )));
            }
        }
        Ok(())
    }

    pub fn finish(self) -> Value {
        match self.acc {
            Accumulator::CountStar(n) | Accumulator::Count(n) => Value::Int64(n),
            Accumulator::SumInt(total) => total.map_or(Value::Null, Value::Int64),
            Accumulator::SumFloat { sum, seen } => {
                if seen {
                    Value::Float64(sum.value())
                } else {
                    Value::Null
                }
            }
            Accumulator::Avg { sum, count } => {
                if count == 0 {
                    Value::Null
                } else {
                    Value::Float64(sum.value() / count as f64)
                }
            }
            Accumulator::Min(v) | Accumulator::Max(v) => v.unwrap_or(Value::Null),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compensated_sum_is_exact_where_naive_sum_drifts() {
        let mut naive = 0.0;
        let mut comp = CompensatedSum::default();
        for _ in 0..10 {
            naive += 0.1;
            comp.add(0.1);
        }
        assert_ne!(naive, 1.0);
        assert_eq!(comp.value(), 1.0);
    }

    #[test]
    fn compensated_sum_survives_catastrophic_cancellation() {
        // Naive summation returns 0 here: 1.0 is lost when added to 1e100.
        let mut comp = CompensatedSum::default();
        for x in [1.0, 1e100, 1.0, -1e100] {
            comp.add(x);
        }
        assert_eq!(comp.value(), 2.0);
    }

    #[test]
    fn compensated_sum_handles_infinity() {
        let mut comp = CompensatedSum::default();
        comp.add(f64::INFINITY);
        comp.add(1.0);
        assert_eq!(comp.value(), f64::INFINITY);
    }

    #[test]
    fn key_values_normalize_floats() {
        assert_eq!(
            KeyValue::of(&Value::Float64(0.0)),
            KeyValue::of(&Value::Float64(-0.0))
        );
        assert_eq!(
            KeyValue::of(&Value::Float64(f64::NAN)),
            KeyValue::of(&Value::Float64(-f64::NAN))
        );
        assert_ne!(
            KeyValue::of(&Value::Int64(1)),
            KeyValue::of(&Value::Float64(1.0))
        );
    }
}
