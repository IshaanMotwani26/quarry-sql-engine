//! Decodes a Parquet file's column chunks into a Quarry table.
//!
//! Each column chunk is a sequence of pages, each with a Thrift-encoded
//! header: an optional dictionary page, then data pages. A data page holds
//! definition levels (which rows are NULL, for OPTIONAL columns) followed by
//! the non-NULL values, either PLAIN or as indices into the dictionary.

use std::borrow::Cow;
use std::path::Path;

use super::encoding::{bit_width, decode_hybrid, Plain};
use super::meta::{read_metadata, ColumnChunk, ColumnSchema, Logical, Physical};
use super::snappy;
use super::thrift::{Reader, Struct};
use crate::catalog::{Column, Table};
use crate::types::Value;

// Page types.
const DATA_PAGE: i64 = 0;
const DICTIONARY_PAGE: i64 = 2;
const DATA_PAGE_V2: i64 = 3;

// Encodings.
const PLAIN: i64 = 0;
const PLAIN_DICTIONARY: i64 = 2;
const RLE: i64 = 3;
const RLE_DICTIONARY: i64 = 8;

pub fn read_parquet_file(name: &str, path: &Path) -> Result<Table, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    read_parquet(name, &bytes)
}

/// Reads a whole Parquet file into a table named `name`.
pub fn read_parquet(name: &str, file: &[u8]) -> Result<Table, String> {
    let meta = read_metadata(file)?;
    let mut table = Table::new(name, meta.schema()?)?;
    for (i, schema) in meta.columns.iter().enumerate() {
        let column = &mut table.columns[i];
        for rg in &meta.row_groups {
            let before = column.len();
            read_chunk(file, schema, &rg.columns[i], column)
                .map_err(|e| format!("column {}: {e}", schema.name))?;
            if (column.len() - before) as i64 != rg.num_rows {
                return Err(format!(
                    "column {}: row group has {} rows but the column has {}",
                    schema.name,
                    rg.num_rows,
                    column.len() - before
                ));
            }
        }
    }
    Ok(table)
}

fn read_chunk(
    file: &[u8],
    schema: &ColumnSchema,
    chunk: &ColumnChunk,
    out: &mut Column,
) -> Result<(), String> {
    let range = chunk.byte_range(file.len())?;
    let data = &file[range];
    let mut pos = 0;
    let mut dictionary: Option<Vec<Value>> = None;
    let mut seen: i64 = 0;
    while seen < chunk.num_values {
        let mut reader = Reader::new(data.get(pos..).unwrap_or(&[]));
        let header = reader.read_struct()?;
        pos += reader.pos();
        let page_type = header.req_int(1, "page type")?;
        let uncompressed = size(header.req_int(2, "page size")?)?;
        let compressed = size(header.req_int(3, "compressed page size")?)?;
        let body = pos
            .checked_add(compressed)
            .and_then(|end| data.get(pos..end))
            .ok_or("corrupt Parquet file: page runs past the column chunk")?;
        pos += compressed;

        match page_type {
            DICTIONARY_PAGE => {
                let h = sub(&header, 7, "dictionary page header")?;
                let n = size(h.req_int(1, "dictionary size")?)?;
                let encoding = h.int(2).unwrap_or(PLAIN);
                if encoding != PLAIN && encoding != PLAIN_DICTIONARY {
                    return Err(format!("unsupported dictionary encoding {encoding}"));
                }
                let page = decompress(chunk.codec, body, uncompressed)?;
                let mut plain = Plain::new(&page);
                let mut values = Vec::with_capacity(n.min(page.len()));
                for _ in 0..n {
                    values.push(plain_value(&mut plain, schema)?);
                }
                dictionary = Some(values);
            }
            DATA_PAGE => {
                let h = sub(&header, 5, "data page header")?;
                let n = page_values(h, chunk.num_values - seen)?;
                let encoding = h.req_int(2, "page encoding")?;
                let page = decompress(chunk.codec, body, uncompressed)?;
                let (defined, used) = if schema.optional {
                    if h.int(3).unwrap_or(RLE) != RLE {
                        return Err("unsupported definition level encoding".into());
                    }
                    // Version 1 pages prefix the levels with their length.
                    let len = page
                        .get(..4)
                        .map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize)
                        .ok_or("corrupt Parquet page: missing definition levels")?;
                    let levels = page
                        .get(4..4 + len)
                        .ok_or("corrupt Parquet page: definition levels run past the page")?;
                    (Some(decode_levels(levels, n)?), 4 + len)
                } else {
                    (None, 0)
                };
                decode_values(
                    &page[used..],
                    encoding,
                    n,
                    defined,
                    &dictionary,
                    schema,
                    out,
                )?;
                seen += n as i64;
            }
            DATA_PAGE_V2 => {
                let h = sub(&header, 8, "data page v2 header")?;
                let n = page_values(h, chunk.num_values - seen)?;
                let encoding = h.req_int(4, "page encoding")?;
                let def_len = size(h.req_int(5, "definition levels length")?)?;
                let rep_len = size(h.req_int(6, "repetition levels length")?)?;
                if rep_len != 0 {
                    return Err("repeated values are not supported".into());
                }
                // Version 2 pages keep the levels uncompressed, ahead of the
                // (possibly compressed) values.
                let levels = body
                    .get(..def_len)
                    .ok_or("corrupt Parquet page: definition levels run past the page")?;
                let defined = if schema.optional {
                    Some(decode_levels(levels, n)?)
                } else {
                    None
                };
                let values = &body[def_len..];
                let values = if h.bool(7).unwrap_or(true) {
                    let len = uncompressed
                        .checked_sub(def_len)
                        .ok_or("corrupt Parquet page: bad page size")?;
                    decompress(chunk.codec, values, len)?
                } else {
                    Cow::Borrowed(values)
                };
                decode_values(&values, encoding, n, defined, &dictionary, schema, out)?;
                seen += n as i64;
            }
            _ => {} // index pages and anything newer: skip
        }
    }
    Ok(())
}

fn sub<'a>(header: &'a Struct, id: i16, what: &str) -> Result<&'a Struct, String> {
    header
        .strukt(id)
        .ok_or_else(|| format!("corrupt Parquet file: missing {what}"))
}

/// A data page's value count, which can't exceed what the chunk has left.
fn page_values(h: &Struct, left: i64) -> Result<usize, String> {
    let n = h.req_int(1, "page value count")?;
    if n > left {
        return Err("corrupt Parquet file: page has more values than its column chunk".into());
    }
    size(n)
}

fn size(n: i64) -> Result<usize, String> {
    usize::try_from(n).map_err(|_| format!("corrupt Parquet file: negative size {n}"))
}

/// Definition levels for a flat OPTIONAL column: 1 if the row has a value,
/// 0 if it is NULL.
fn decode_levels(data: &[u8], n: usize) -> Result<Vec<bool>, String> {
    let (levels, _) = decode_hybrid(data, bit_width(1), n)?;
    Ok(levels.into_iter().map(|l| l == 1).collect())
}

fn decompress<'a>(codec: i64, data: &'a [u8], size: usize) -> Result<Cow<'a, [u8]>, String> {
    let name = match codec {
        0 => return Ok(Cow::Borrowed(data)),
        1 => {
            let out = snappy::decompress(data)?;
            if out.len() != size {
                return Err("corrupt Parquet page: wrong decompressed size".into());
            }
            return Ok(Cow::Owned(out));
        }
        2 => "GZIP",
        3 => "LZO",
        4 => "BROTLI",
        5 => "LZ4",
        6 => "ZSTD",
        7 => "LZ4_RAW",
        _ => "unknown",
    };
    Err(format!(
        "{name} compression is not supported (only Snappy and uncompressed files are)"
    ))
}

fn decode_values(
    data: &[u8],
    encoding: i64,
    n: usize,
    defined: Option<Vec<bool>>,
    dictionary: &Option<Vec<Value>>,
    schema: &ColumnSchema,
    out: &mut Column,
) -> Result<(), String> {
    let present = defined
        .as_ref()
        .map_or(n, |d| d.iter().filter(|&&x| x).count());
    let values: Vec<Value> = match encoding {
        PLAIN => {
            let mut plain = Plain::new(data);
            (0..present)
                .map(|_| plain_value(&mut plain, schema))
                .collect::<Result<_, _>>()?
        }
        PLAIN_DICTIONARY | RLE_DICTIONARY => {
            let dict = dictionary
                .as_ref()
                .ok_or("corrupt Parquet file: dictionary-encoded page without a dictionary")?;
            let width = u32::from(*data.first().ok_or("corrupt Parquet page: empty")?);
            let (indices, _) = decode_hybrid(&data[1..], width, present)?;
            indices
                .into_iter()
                .map(|i| {
                    dict.get(i as usize)
                        .cloned()
                        .ok_or_else(|| "corrupt Parquet page: dictionary index out of range".into())
                })
                .collect::<Result<_, String>>()?
        }
        RLE if schema.physical == Physical::Boolean => {
            let len = data
                .get(..4)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize)
                .ok_or("corrupt Parquet page: missing RLE length")?;
            let rle = data
                .get(4..4 + len)
                .ok_or("corrupt Parquet page: RLE data runs past the page")?;
            let (bits, _) = decode_hybrid(rle, 1, present)?;
            bits.into_iter().map(|b| Value::Boolean(b == 1)).collect()
        }
        other => return Err(format!("unsupported encoding {}", encoding_name(other))),
    };

    let mut values = values.into_iter();
    for i in 0..n {
        let v = match &defined {
            Some(d) if !d[i] => Value::Null,
            _ => values.next().expect("counted above"),
        };
        out.push(v)?;
    }
    Ok(())
}

fn encoding_name(e: i64) -> String {
    match e {
        4 => "BIT_PACKED".into(),
        5 => "DELTA_BINARY_PACKED".into(),
        6 => "DELTA_LENGTH_BYTE_ARRAY".into(),
        7 => "DELTA_BYTE_ARRAY".into(),
        9 => "BYTE_STREAM_SPLIT".into(),
        _ => e.to_string(),
    }
}

/// Reads one PLAIN value and converts it to the column's Quarry type.
fn plain_value(p: &mut Plain, schema: &ColumnSchema) -> Result<Value, String> {
    use Logical as L;
    use Physical as P;
    Ok(match (&schema.physical, &schema.logical) {
        (P::Boolean, _) => Value::Boolean(p.boolean()?),
        (P::Int32, L::Date) => Value::Date(p.i32()?),
        (P::Int32, L::Decimal { scale }) => decimal(i128::from(p.i32()?), *scale),
        (P::Int32, L::Integer { signed: false, .. }) => Value::Int64(i64::from(p.i32()? as u32)),
        (P::Int32, _) => Value::Int64(i64::from(p.i32()?)),
        (P::Int64, L::Decimal { scale }) => decimal(i128::from(p.i64()?), *scale),
        (P::Int64, _) => Value::Int64(p.i64()?),
        (P::Float, _) => Value::Float64(f64::from(p.f32()?)),
        (P::Double, _) => Value::Float64(p.f64()?),
        (P::ByteArray, L::Decimal { scale }) => decimal(be_int(p.byte_array()?)?, *scale),
        (P::FixedLenByteArray(n), L::Decimal { scale }) => decimal(be_int(p.bytes(*n)?)?, *scale),
        (P::ByteArray, _) => {
            let bytes = p.byte_array()?;
            let s = std::str::from_utf8(bytes).map_err(|_| "string is not valid UTF-8")?;
            Value::Utf8(s.to_string())
        }
        (P::Int96 | P::FixedLenByteArray(_), _) => {
            unreachable!("rejected when the schema was read")
        }
    })
}

fn decimal(unscaled: i128, scale: u32) -> Value {
    // Dividing by an exact power of ten rounds the same way as parsing the
    // decimal text, so 0.05 becomes the same double as the literal 0.05.
    Value::Float64(unscaled as f64 / 10f64.powi(scale as i32))
}

/// A big-endian two's complement integer, as DECIMAL byte arrays store it.
fn be_int(bytes: &[u8]) -> Result<i128, String> {
    if bytes.is_empty() || bytes.len() > 16 {
        return Err(format!(
            "unsupported DECIMAL width of {} bytes",
            bytes.len()
        ));
    }
    let fill = if bytes[0] & 0x80 != 0 { 0xff } else { 0 };
    let mut buf = [fill; 16];
    buf[16 - bytes.len()..].copy_from_slice(bytes);
    Ok(i128::from_be_bytes(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn big_endian_decimals() {
        assert_eq!(be_int(&[0x04, 0xd2]).unwrap(), 1234);
        assert_eq!(be_int(&[0xff, 0xfb]).unwrap(), -5);
        assert_eq!(decimal(-5, 2), Value::Float64(-0.05));
        assert!(be_int(&[]).is_err());
    }
}
