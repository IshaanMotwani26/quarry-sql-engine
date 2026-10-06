//! Catalog and in-memory storage.
//!
//! Tables are stored column-by-column (one typed vector per column) rather
//! than row-by-row. Phase 3 reads them a row at a time, but Phase 6's
//! vectorized executor can scan these vectors directly.

use std::collections::BTreeMap;
use std::fmt;

use crate::types::{Type, Value};

#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub name: String,
    pub ty: Type,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Schema {
    pub fields: Vec<Field>,
}

impl Schema {
    pub fn new(fields: Vec<Field>) -> Self {
        Schema { fields }
    }

    pub fn from_pairs(pairs: &[(&str, Type)]) -> Self {
        Schema::new(
            pairs
                .iter()
                .map(|(name, ty)| Field {
                    name: name.to_string(),
                    ty: *ty,
                })
                .collect(),
        )
    }

    pub fn len(&self) -> usize {
        self.fields.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.fields.iter().position(|f| f.name == name)
    }
}

impl fmt::Display for Schema {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let cols: Vec<String> = self
            .fields
            .iter()
            .map(|c| format!("{} {}", c.name, c.ty))
            .collect();
        write!(f, "({})", cols.join(", "))
    }
}

/// One column of data. `None` is SQL NULL.
#[derive(Debug, Clone, PartialEq)]
pub enum Column {
    Boolean(Vec<Option<bool>>),
    Int64(Vec<Option<i64>>),
    Float64(Vec<Option<f64>>),
    Utf8(Vec<Option<String>>),
    Date(Vec<Option<i32>>),
}

impl Column {
    /// Returns `None` for types that cannot be stored in a table.
    pub fn new(ty: Type) -> Option<Column> {
        Some(match ty {
            Type::Boolean => Column::Boolean(Vec::new()),
            Type::Int64 => Column::Int64(Vec::new()),
            Type::Float64 => Column::Float64(Vec::new()),
            Type::Utf8 => Column::Utf8(Vec::new()),
            Type::Date => Column::Date(Vec::new()),
            Type::Null | Type::Interval => return None,
        })
    }

    pub fn ty(&self) -> Type {
        match self {
            Column::Boolean(_) => Type::Boolean,
            Column::Int64(_) => Type::Int64,
            Column::Float64(_) => Type::Float64,
            Column::Utf8(_) => Type::Utf8,
            Column::Date(_) => Type::Date,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Column::Boolean(v) => v.len(),
            Column::Int64(v) => v.len(),
            Column::Float64(v) => v.len(),
            Column::Utf8(v) => v.len(),
            Column::Date(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, i: usize) -> Value {
        let v = match self {
            Column::Boolean(v) => v[i].map(Value::Boolean),
            Column::Int64(v) => v[i].map(Value::Int64),
            Column::Float64(v) => v[i].map(Value::Float64),
            Column::Utf8(v) => v[i].clone().map(Value::Utf8),
            Column::Date(v) => v[i].map(Value::Date),
        };
        v.unwrap_or(Value::Null)
    }

    /// Whether `push(v)` would succeed. Int64 values widen into Float64 columns.
    pub fn accepts(&self, v: &Value) -> bool {
        matches!(
            (self, v),
            (_, Value::Null)
                | (Column::Boolean(_), Value::Boolean(_))
                | (Column::Int64(_), Value::Int64(_))
                | (Column::Float64(_), Value::Float64(_) | Value::Int64(_))
                | (Column::Utf8(_), Value::Utf8(_))
                | (Column::Date(_), Value::Date(_))
        )
    }

    pub fn push(&mut self, v: Value) -> Result<(), String> {
        match (self, v) {
            (Column::Boolean(c), Value::Null) => c.push(None),
            (Column::Int64(c), Value::Null) => c.push(None),
            (Column::Float64(c), Value::Null) => c.push(None),
            (Column::Utf8(c), Value::Null) => c.push(None),
            (Column::Date(c), Value::Null) => c.push(None),
            (Column::Boolean(c), Value::Boolean(b)) => c.push(Some(b)),
            (Column::Int64(c), Value::Int64(x)) => c.push(Some(x)),
            (Column::Float64(c), Value::Float64(x)) => c.push(Some(x)),
            (Column::Float64(c), Value::Int64(x)) => c.push(Some(x as f64)),
            (Column::Utf8(c), Value::Utf8(s)) => c.push(Some(s)),
            (Column::Date(c), Value::Date(d)) => c.push(Some(d)),
            (col, v) => {
                return Err(format!(
                    "cannot store a {} value in a {} column",
                    v.ty(),
                    col.ty()
                ))
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    pub name: String,
    pub schema: Schema,
    pub columns: Vec<Column>,
}

impl Table {
    pub fn new(name: impl Into<String>, schema: Schema) -> Result<Table, String> {
        let name = name.into();
        let mut columns = Vec::with_capacity(schema.len());
        for field in &schema.fields {
            let col = Column::new(field.ty).ok_or_else(|| {
                format!(
                    "column {}.{} cannot have type {}",
                    name, field.name, field.ty
                )
            })?;
            columns.push(col);
        }
        Ok(Table {
            name,
            schema,
            columns,
        })
    }

    pub fn row_count(&self) -> usize {
        self.columns.first().map_or(0, Column::len)
    }

    /// Appends a row atomically: on error, no column is modified.
    pub fn push_row(&mut self, row: Vec<Value>) -> Result<(), String> {
        if row.len() != self.columns.len() {
            return Err(format!(
                "expected {} values, got {}",
                self.columns.len(),
                row.len()
            ));
        }
        for ((col, field), v) in self.columns.iter().zip(&self.schema.fields).zip(&row) {
            if !col.accepts(v) {
                return Err(format!(
                    "column {}: cannot store a {} value in a {} column",
                    field.name,
                    v.ty(),
                    field.ty
                ));
            }
        }
        for (col, v) in self.columns.iter_mut().zip(row) {
            col.push(v)?;
        }
        Ok(())
    }

    pub fn row(&self, i: usize) -> Vec<Value> {
        self.columns.iter().map(|c| c.get(i)).collect()
    }
}

#[derive(Debug, Default)]
pub struct Catalog {
    tables: BTreeMap<String, Table>,
}

impl Catalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, table: Table) -> Result<(), String> {
        if self.tables.contains_key(&table.name) {
            return Err(format!("table \"{}\" already exists", table.name));
        }
        self.tables.insert(table.name.clone(), table);
        Ok(())
    }

    pub fn register_or_replace(&mut self, table: Table) {
        self.tables.insert(table.name.clone(), table);
    }

    pub fn get(&self, name: &str) -> Option<&Table> {
        self.tables.get(name)
    }

    pub fn tables(&self) -> impl Iterator<Item = &Table> {
        self.tables.values()
    }
}
