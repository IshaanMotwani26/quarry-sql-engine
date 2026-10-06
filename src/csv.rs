//! CSV loading (RFC 4180 quoting) with optional schema inference.
//!
//! - `"a ""quoted"" field"` and newlines inside quotes are supported.
//! - An empty *unquoted* field is NULL; `""` is an empty string.
//! - A trailing delimiter (TPC-H `.tbl` style: `1|foo|`) is ignored.
//! - Header names are lowercased, matching SQL's case folding.

use std::path::Path;

use crate::catalog::{Field, Schema, Table};
use crate::types::{parse_date, Type, Value};

#[derive(Debug, Clone)]
pub struct CsvOptions {
    pub delimiter: char,
    pub has_header: bool,
}

impl Default for CsvOptions {
    fn default() -> Self {
        CsvOptions {
            delimiter: ',',
            has_header: true,
        }
    }
}

#[derive(Debug, Clone)]
struct RawField {
    text: String,
    quoted: bool,
}

impl RawField {
    fn is_null(&self) -> bool {
        self.text.is_empty() && !self.quoted
    }
}

struct Record {
    line: usize,
    fields: Vec<RawField>,
}

pub fn read_csv_file(
    name: &str,
    path: &Path,
    opts: &CsvOptions,
    schema: Option<&Schema>,
) -> Result<Table, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse_csv(name, &text, opts, schema)
}

/// Parses CSV text into a table. Without `schema`, column types are inferred
/// by trying BIGINT, DOUBLE, DATE, BOOLEAN, then VARCHAR.
pub fn parse_csv(
    name: &str,
    text: &str,
    opts: &CsvOptions,
    schema: Option<&Schema>,
) -> Result<Table, String> {
    let mut records = split_records(text, opts.delimiter)?;
    let header = if opts.has_header && !records.is_empty() {
        Some(records.remove(0))
    } else {
        None
    };

    let mut header_names = match &header {
        Some(h) => h
            .fields
            .iter()
            .map(|f| f.text.trim().to_ascii_lowercase())
            .collect(),
        None => Vec::new(),
    };
    if header_names.last().is_some_and(|n: &String| n.is_empty()) && header_names.len() > 1 {
        header_names.pop();
    }

    let width = match (schema, &header) {
        (Some(s), _) => s.len(),
        (None, Some(_)) => header_names.len(),
        (None, None) => records.first().map_or(0, |r| r.fields.len()),
    };

    for rec in &mut records {
        if rec.fields.len() == width + 1 && rec.fields.last().is_some_and(RawField::is_null) {
            rec.fields.pop();
        }
        if rec.fields.len() != width {
            return Err(format!(
                "line {}: expected {width} fields, found {}",
                rec.line,
                rec.fields.len()
            ));
        }
    }

    let schema = match schema {
        Some(s) => s.clone(),
        None => infer_schema(&header_names, width, &records)?,
    };

    let mut table = Table::new(name, schema)?;
    for rec in records {
        let mut row = Vec::with_capacity(width);
        for (raw, field) in rec.fields.iter().zip(&table.schema.fields) {
            let v = parse_field(raw, field.ty)
                .map_err(|e| format!("line {}, column {}: {e}", rec.line, field.name))?;
            row.push(v);
        }
        table
            .push_row(row)
            .map_err(|e| format!("line {}: {e}", rec.line))?;
    }
    Ok(table)
}

fn split_records(text: &str, delim: char) -> Result<Vec<Record>, String> {
    let mut records = Vec::new();
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut quoted = false; // current field started with a quote
    let mut in_quotes = false; // currently inside the quoted section
    let mut line = 1;
    let mut record_line = 1;

    let mut finish_record = |fields: &mut Vec<RawField>, record_line: usize| {
        let blank = fields.len() == 1 && fields[0].is_null();
        let taken = std::mem::take(fields);
        if !blank {
            records.push(Record {
                line: record_line,
                fields: taken,
            });
        }
    };

    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                if c == '\n' {
                    line += 1;
                }
                field.push(c);
            }
            continue;
        }
        match c {
            '"' if field.is_empty() && !quoted => {
                in_quotes = true;
                quoted = true;
            }
            '\r' => {}
            '\n' => {
                fields.push(RawField {
                    text: std::mem::take(&mut field),
                    quoted,
                });
                quoted = false;
                finish_record(&mut fields, record_line);
                line += 1;
                record_line = line;
            }
            c if c == delim => {
                fields.push(RawField {
                    text: std::mem::take(&mut field),
                    quoted,
                });
                quoted = false;
            }
            c => {
                if quoted {
                    return Err(format!(
                        "line {line}: unexpected character `{c}` after closing quote"
                    ));
                }
                field.push(c);
            }
        }
    }
    if in_quotes {
        return Err(format!("line {record_line}: unterminated quoted field"));
    }
    if !field.is_empty() || quoted || !fields.is_empty() {
        fields.push(RawField {
            text: field,
            quoted,
        });
        finish_record(&mut fields, record_line);
    }
    Ok(records)
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "true" | "t" => Some(true),
        "false" | "f" => Some(false),
        _ => None,
    }
}

fn parse_field(raw: &RawField, ty: Type) -> Result<Value, String> {
    if raw.is_null() {
        return Ok(Value::Null);
    }
    let s = raw.text.as_str();
    let bad = |what: &str| format!("invalid {what} `{s}`");
    match ty {
        Type::Utf8 => Ok(Value::Utf8(raw.text.clone())),
        Type::Int64 => s
            .trim()
            .parse()
            .map(Value::Int64)
            .map_err(|_| bad("integer")),
        Type::Float64 => s
            .trim()
            .parse()
            .map(Value::Float64)
            .map_err(|_| bad("number")),
        Type::Date => parse_date(s)
            .map(Value::Date)
            .ok_or_else(|| bad("date (expected YYYY-MM-DD)")),
        Type::Boolean => parse_bool(s)
            .map(Value::Boolean)
            .ok_or_else(|| bad("boolean")),
        Type::Null | Type::Interval => Err(format!("type {ty} cannot be loaded from CSV")),
    }
}

fn infer_schema(header: &[String], width: usize, records: &[Record]) -> Result<Schema, String> {
    const CANDIDATES: [Type; 4] = [Type::Int64, Type::Float64, Type::Date, Type::Boolean];
    let mut fields: Vec<Field> = Vec::with_capacity(width);
    for i in 0..width {
        let name = match header.get(i) {
            Some(h) if !h.is_empty() => h.clone(),
            _ => format!("column{}", i + 1),
        };
        if fields.iter().any(|f| f.name == name) {
            return Err(format!("duplicate column name \"{name}\" in header"));
        }
        let values = records
            .iter()
            .map(|r| &r.fields[i])
            .filter(|f| !f.is_null());
        let ty = CANDIDATES
            .into_iter()
            .find(|&ty| {
                values
                    .clone()
                    .all(|f| !f.quoted && parse_field(f, ty).is_ok())
            })
            .unwrap_or(Type::Utf8);
        // A column with no non-null values gives no evidence; default to VARCHAR.
        let all_null = records.iter().all(|r| r.fields[i].is_null());
        fields.push(Field {
            name,
            ty: if all_null { Type::Utf8 } else { ty },
        });
    }
    Ok(Schema::new(fields))
}
