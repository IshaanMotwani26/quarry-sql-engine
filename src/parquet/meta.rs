//! Parquet file metadata: the footer that describes the schema and where
//! each column's data lives.
//!
//! A Parquet file is `PAR1`, the column data, the footer (a Thrift-encoded
//! `FileMetaData`), the footer's length as a 4-byte little-endian integer,
//! and `PAR1` again. Field numbers below follow `parquet.thrift`.

use super::thrift::{Reader, Struct};
use crate::catalog::{Field, Schema};
use crate::types::Type;

const MAGIC: &[u8; 4] = b"PAR1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Physical {
    Boolean,
    Int32,
    Int64,
    Int96,
    Float,
    Double,
    ByteArray,
    FixedLenByteArray(usize),
}

/// What the stored bytes mean, from the schema's logical or legacy
/// "converted" type annotation.
#[derive(Debug, Clone, PartialEq)]
pub enum Logical {
    None,
    String,
    Date,
    Decimal {
        scale: u32,
    },
    Integer {
        bits: u8,
        signed: bool,
    },
    /// The column holds only NULLs.
    Null,
    /// Anything Quarry can't represent: timestamps, UUIDs, and so on.
    Unsupported(&'static str),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnSchema {
    pub name: String,
    pub physical: Physical,
    pub logical: Logical,
    /// OPTIONAL columns may hold NULLs; REQUIRED ones never do.
    pub optional: bool,
}

impl ColumnSchema {
    /// The Quarry type this column loads as. DECIMAL becomes DOUBLE, the
    /// same approximation the rest of Quarry makes.
    pub fn quarry_type(&self) -> Result<Type, String> {
        use Logical as L;
        use Physical as P;
        let ty = match (&self.physical, &self.logical) {
            (_, L::Unsupported(what)) => return Err(self.unsupported(what)),
            (_, L::Null) => Type::Utf8,
            (_, L::Decimal { .. }) => Type::Float64,
            (P::Int32, L::Date) => Type::Date,
            (P::Int64, L::Integer { signed: false, .. }) => {
                return Err(self.unsupported("unsigned 64-bit integers"))
            }
            (P::Boolean, L::None) => Type::Boolean,
            (P::Int32 | P::Int64, L::None | L::Integer { .. }) => Type::Int64,
            (P::Float | P::Double, L::None) => Type::Float64,
            (P::ByteArray, L::None | L::String) => Type::Utf8,
            (P::Int96, _) => return Err(self.unsupported("INT96 timestamps")),
            (P::FixedLenByteArray(_), L::None) => {
                return Err(self.unsupported("fixed-length binary"))
            }
            (p, l) => {
                return Err(format!(
                    "column {}: invalid annotation {l:?} on {p:?}",
                    self.name
                ))
            }
        };
        Ok(ty)
    }

    fn unsupported(&self, what: &str) -> String {
        format!("column {}: {what} are not supported", self.name)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnChunk {
    pub codec: i64,
    pub num_values: i64,
    pub data_page_offset: i64,
    pub dictionary_page_offset: Option<i64>,
    pub total_compressed_size: i64,
}

impl ColumnChunk {
    /// The byte range of this chunk's pages. The dictionary page, if any,
    /// comes first.
    pub fn byte_range(&self, file_len: usize) -> Result<std::ops::Range<usize>, String> {
        let start = match self.dictionary_page_offset {
            Some(d) if d > 0 && d < self.data_page_offset => d,
            _ => self.data_page_offset,
        };
        let end = start.checked_add(self.total_compressed_size);
        match (
            usize::try_from(start),
            end.and_then(|e| usize::try_from(e).ok()),
        ) {
            (Ok(s), Some(e)) if s <= e && e <= file_len => Ok(s..e),
            _ => Err("corrupt Parquet file: column chunk lies outside the file".into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RowGroup {
    pub num_rows: i64,
    /// One chunk per column, in schema order.
    pub columns: Vec<ColumnChunk>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileMetaData {
    pub num_rows: i64,
    pub columns: Vec<ColumnSchema>,
    pub row_groups: Vec<RowGroup>,
    pub created_by: Option<String>,
}

impl FileMetaData {
    pub fn schema(&self) -> Result<Schema, String> {
        let fields = self
            .columns
            .iter()
            .map(|c| {
                Ok(Field {
                    name: c.name.to_ascii_lowercase(),
                    ty: c.quarry_type()?,
                })
            })
            .collect::<Result<_, String>>()?;
        Ok(Schema::new(fields))
    }
}

pub fn read_metadata(file: &[u8]) -> Result<FileMetaData, String> {
    let n = file.len();
    if n < 12 || &file[..4] != MAGIC {
        return Err("not a Parquet file (missing PAR1 header)".into());
    }
    match &file[n - 4..] {
        m if m == MAGIC => {}
        b"PARE" => return Err("encrypted Parquet files are not supported".into()),
        _ => return Err("not a Parquet file (missing PAR1 footer)".into()),
    }
    let len = u32::from_le_bytes(file[n - 8..n - 4].try_into().unwrap()) as usize;
    let start = (n - 8)
        .checked_sub(len)
        .filter(|&s| s >= 4)
        .ok_or("corrupt Parquet file: footer length is wrong")?;
    let meta = Reader::new(&file[start..n - 8]).read_struct()?;
    parse_file_metadata(&meta)
}

fn parse_file_metadata(meta: &Struct) -> Result<FileMetaData, String> {
    let columns = parse_schema(meta.list(2))?;
    let row_groups = meta
        .list(4)
        .iter()
        .map(|rg| parse_row_group(as_struct(rg)?, columns.len()))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(FileMetaData {
        num_rows: meta.req_int(3, "row count")?,
        columns,
        row_groups,
        created_by: meta.string(6),
    })
}

fn as_struct(v: &super::thrift::Value) -> Result<&Struct, String> {
    match v {
        super::thrift::Value::Struct(s) => Ok(s),
        _ => Err("corrupt Parquet metadata: expected a struct".into()),
    }
}

/// The schema is a tree flattened depth-first; element 0 is the root. Quarry
/// reads flat files only, so every other element must be a leaf.
fn parse_schema(elements: &[super::thrift::Value]) -> Result<Vec<ColumnSchema>, String> {
    let (root, leaves) = elements
        .split_first()
        .ok_or("corrupt Parquet metadata: empty schema")?;
    let expected = as_struct(root)?.int(5).unwrap_or(0);
    let mut columns = Vec::with_capacity(leaves.len());
    for el in leaves {
        let el = as_struct(el)?;
        let name = el.string(4).unwrap_or_default();
        if el.int(5).unwrap_or(0) > 0 || el.int(1).is_none() {
            return Err(format!(
                "column {name}: nested columns (structs, lists, maps) are not supported"
            ));
        }
        let optional = match el.int(3).unwrap_or(0) {
            0 => false,
            1 => true,
            _ => return Err(format!("column {name}: repeated columns are not supported")),
        };
        let physical =
            match el.int(1) {
                Some(0) => Physical::Boolean,
                Some(1) => Physical::Int32,
                Some(2) => Physical::Int64,
                Some(3) => Physical::Int96,
                Some(4) => Physical::Float,
                Some(5) => Physical::Double,
                Some(6) => Physical::ByteArray,
                Some(7) => {
                    let len = el.req_int(2, "fixed-length type width")?;
                    Physical::FixedLenByteArray(len.try_into().map_err(|_| {
                        format!("column {name}: invalid fixed-length type width {len}")
                    })?)
                }
                t => return Err(format!("column {name}: unknown physical type {t:?}")),
            };
        let logical = parse_logical(el);
        columns.push(ColumnSchema {
            name,
            physical,
            logical,
            optional,
        });
    }
    if columns.len() as i64 != expected {
        return Err("corrupt Parquet metadata: schema child count mismatch".into());
    }
    Ok(columns)
}

/// Prefers the newer `logicalType` union (field 10) over the legacy
/// `converted_type` (field 6).
fn parse_logical(el: &Struct) -> Logical {
    if let Some(lt) = el.strukt(10) {
        if lt.strukt(1).is_some() || lt.strukt(4).is_some() || lt.strukt(12).is_some() {
            return Logical::String; // STRING, ENUM, JSON
        }
        if let Some(d) = lt.strukt(5) {
            return Logical::Decimal {
                scale: d.int(1).unwrap_or(0).clamp(0, 38) as u32,
            };
        }
        if lt.strukt(6).is_some() {
            return Logical::Date;
        }
        if let Some(i) = lt.strukt(10) {
            return Logical::Integer {
                bits: i.int(1).unwrap_or(64).clamp(0, 64) as u8,
                signed: i.bool(2).unwrap_or(true),
            };
        }
        if lt.strukt(11).is_some() {
            return Logical::Null;
        }
        let what = [
            (7, "TIME values"),
            (8, "TIMESTAMP values"),
            (14, "UUIDs"),
            (15, "16-bit floats"),
        ]
        .into_iter()
        .find(|(id, _)| lt.strukt(*id).is_some())
        .map_or("logical types like this one", |(_, what)| what);
        return Logical::Unsupported(what);
    }
    match el.int(6) {
        None => Logical::None,
        Some(0) | Some(4) | Some(19) => Logical::String, // UTF8, ENUM, JSON
        Some(5) => Logical::Decimal {
            scale: el.int(7).unwrap_or(0).clamp(0, 38) as u32,
        },
        Some(6) => Logical::Date,
        Some(c @ 11..=18) => {
            let bits = [8, 16, 32, 64][((c - 11) % 4) as usize];
            Logical::Integer {
                bits,
                signed: c >= 15,
            }
        }
        Some(7) | Some(8) => Logical::Unsupported("TIME values"),
        Some(9) | Some(10) => Logical::Unsupported("TIMESTAMP values"),
        Some(21) => Logical::Unsupported("INTERVAL values"),
        Some(_) => Logical::Unsupported("logical types like this one"),
    }
}

fn parse_row_group(rg: &Struct, width: usize) -> Result<RowGroup, String> {
    let columns = rg
        .list(1)
        .iter()
        .map(|cc| {
            let cc = as_struct(cc)?;
            if cc.string(1).is_some() {
                return Err("Parquet files that reference other files are not supported".into());
            }
            let md = cc
                .strukt(3)
                .ok_or("corrupt Parquet metadata: column chunk without metadata")?;
            Ok(ColumnChunk {
                codec: md.req_int(4, "compression codec")?,
                num_values: md.req_int(5, "value count")?,
                data_page_offset: md.req_int(9, "data page offset")?,
                dictionary_page_offset: md.int(11),
                total_compressed_size: md.req_int(7, "column chunk size")?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    if columns.len() != width {
        return Err("corrupt Parquet metadata: row group has the wrong number of columns".into());
    }
    Ok(RowGroup {
        num_rows: rg.req_int(3, "row group row count")?,
        columns,
    })
}
