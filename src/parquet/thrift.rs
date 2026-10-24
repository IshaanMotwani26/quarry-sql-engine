//! Decoder for the Thrift compact protocol, the binary encoding Parquet uses
//! for its file footer and page headers.
//!
//! Rather than generating code from Parquet's `.thrift` IDL, this decodes any
//! struct into a generic tree keyed by field id; `meta.rs` then picks out the
//! fields it needs. Unknown fields are decoded and ignored, so files written
//! by newer Parquet versions still load.

use std::collections::BTreeMap;

/// Nesting deeper than this is rejected so corrupt input can't overflow the
/// stack. Parquet metadata nests about five levels deep.
const MAX_DEPTH: usize = 32;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bool(bool),
    /// i8, i16, i32 and i64 all decode to this.
    Int(i64),
    Double(f64),
    Binary(Vec<u8>),
    List(Vec<Value>),
    Map(Vec<(Value, Value)>),
    Struct(Struct),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Struct {
    fields: BTreeMap<i16, Value>,
}

impl Struct {
    pub fn get(&self, id: i16) -> Option<&Value> {
        self.fields.get(&id)
    }

    pub fn int(&self, id: i16) -> Option<i64> {
        match self.get(id) {
            Some(Value::Int(n)) => Some(*n),
            _ => None,
        }
    }

    /// A required integer field; `what` names it in the error message.
    pub fn req_int(&self, id: i16, what: &str) -> Result<i64, String> {
        self.int(id)
            .ok_or_else(|| format!("corrupt Parquet metadata: missing {what}"))
    }

    pub fn bool(&self, id: i16) -> Option<bool> {
        match self.get(id) {
            Some(Value::Bool(b)) => Some(*b),
            _ => None,
        }
    }

    pub fn string(&self, id: i16) -> Option<String> {
        match self.get(id) {
            Some(Value::Binary(b)) => Some(String::from_utf8_lossy(b).into_owned()),
            _ => None,
        }
    }

    /// A list field, or an empty slice if it is absent.
    pub fn list(&self, id: i16) -> &[Value] {
        match self.get(id) {
            Some(Value::List(items)) => items,
            _ => &[],
        }
    }

    pub fn strukt(&self, id: i16) -> Option<&Struct> {
        match self.get(id) {
            Some(Value::Struct(s)) => Some(s),
            _ => None,
        }
    }
}

// Compact protocol type codes.
const BOOL_TRUE: u8 = 1;
const BOOL_FALSE: u8 = 2;
const BYTE: u8 = 3;
const I16: u8 = 4;
const I32: u8 = 5;
const I64: u8 = 6;
const DOUBLE: u8 = 7;
const BINARY: u8 = 8;
const LIST: u8 = 9;
const SET: u8 = 10;
const MAP: u8 = 11;
const STRUCT: u8 = 12;

pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    /// Bytes consumed so far.
    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn read_struct(&mut self) -> Result<Struct, String> {
        self.struct_at(0)
    }

    fn byte(&mut self) -> Result<u8, String> {
        let b = *self
            .buf
            .get(self.pos)
            .ok_or("corrupt Parquet metadata: unexpected end of data")?;
        self.pos += 1;
        Ok(b)
    }

    fn bytes(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&end| end <= self.buf.len())
            .ok_or("corrupt Parquet metadata: unexpected end of data")?;
        let out = &self.buf[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    fn varint(&mut self) -> Result<u64, String> {
        let mut out = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.byte()?;
            out |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(out);
            }
        }
        Err("corrupt Parquet metadata: varint is too long".into())
    }

    fn zigzag(&mut self) -> Result<i64, String> {
        let n = self.varint()?;
        Ok((n >> 1) as i64 ^ -((n & 1) as i64))
    }

    /// A collection size, checked against the bytes left so a corrupt length
    /// can't trigger a huge allocation. Every element takes at least a byte.
    fn size(&mut self, n: u64) -> Result<usize, String> {
        let left = (self.buf.len() - self.pos) as u64;
        if n > left {
            return Err("corrupt Parquet metadata: collection size exceeds the data".into());
        }
        Ok(n as usize)
    }

    fn struct_at(&mut self, depth: usize) -> Result<Struct, String> {
        if depth > MAX_DEPTH {
            return Err("corrupt Parquet metadata: nesting is too deep".into());
        }
        let mut fields = BTreeMap::new();
        let mut last: i16 = 0;
        loop {
            let header = self.byte()?;
            if header == 0 {
                return Ok(Struct { fields });
            }
            let ty = header & 0x0f;
            let delta = header >> 4;
            let id = if delta == 0 {
                i16::try_from(self.zigzag()?)
                    .map_err(|_| "corrupt Parquet metadata: bad field id".to_string())?
            } else {
                last.wrapping_add(i16::from(delta))
            };
            last = id;
            let value = match ty {
                // In a struct field the boolean is the type code itself.
                BOOL_TRUE => Value::Bool(true),
                BOOL_FALSE => Value::Bool(false),
                _ => self.value(ty, depth)?,
            };
            fields.insert(id, value);
        }
    }

    fn value(&mut self, ty: u8, depth: usize) -> Result<Value, String> {
        Ok(match ty {
            // Inside a list or map, a boolean takes a byte of its own.
            BOOL_TRUE | BOOL_FALSE => Value::Bool(self.byte()? == BOOL_TRUE),
            BYTE => Value::Int(i64::from(self.byte()? as i8)),
            I16 | I32 | I64 => Value::Int(self.zigzag()?),
            DOUBLE => {
                let b = self.bytes(8)?;
                Value::Double(f64::from_le_bytes(b.try_into().unwrap()))
            }
            BINARY => {
                let n = self.varint()?;
                let n = self.size(n)?;
                Value::Binary(self.bytes(n)?.to_vec())
            }
            LIST | SET => {
                let header = self.byte()?;
                let elem = header & 0x0f;
                let n = match header >> 4 {
                    15 => self.varint()?,
                    n => u64::from(n),
                };
                let n = self.size(n)?;
                let mut items = Vec::with_capacity(n);
                for _ in 0..n {
                    items.push(self.value(elem, depth + 1)?);
                }
                Value::List(items)
            }
            MAP => {
                let n = self.varint()?;
                let n = self.size(n)?;
                let mut entries = Vec::with_capacity(n);
                if n > 0 {
                    let types = self.byte()?;
                    let (k, v) = (types >> 4, types & 0x0f);
                    for _ in 0..n {
                        let key = self.value(k, depth + 1)?;
                        let val = self.value(v, depth + 1)?;
                        entries.push((key, val));
                    }
                }
                Value::Map(entries)
            }
            STRUCT => Value::Struct(self.struct_at(depth + 1)?),
            _ => return Err(format!("corrupt Parquet metadata: unknown type code {ty}")),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_fields_of_every_kind() {
        let bytes = [
            0x15, 0x04, // field 1, i32, zigzag(2) = 4
            0x11, // field 2, bool true
            0x16, 0x03, // field 3, i64 -2
            0x18, 0x02, b'h', b'i', // field 4, binary "hi"
            0x19, 0x25, 0x02, 0x03, // field 5, list<i32> [1, -2]
            0x1c, 0x15, 0x0e, 0x00, // field 6, struct { 1: 7 }
            0x05, 0x40, 0x01, // field id 32 (long form), i32 -1
            0x00,
        ];
        let mut r = Reader::new(&bytes);
        let s = r.read_struct().unwrap();
        assert_eq!(r.pos(), bytes.len());
        assert_eq!(s.int(1), Some(2));
        assert_eq!(s.bool(2), Some(true));
        assert_eq!(s.int(3), Some(-2));
        assert_eq!(s.string(4).as_deref(), Some("hi"));
        assert_eq!(s.list(5), &[Value::Int(1), Value::Int(-2)]);
        assert_eq!(s.strukt(6).unwrap().int(1), Some(7));
        assert_eq!(s.int(32), Some(-1));
        assert!(s.list(99).is_empty());
    }

    #[test]
    fn booleans_in_lists_take_a_byte() {
        let bytes = [0x19, 0x11, 0x01, 0x00]; // field 1, list<bool> [true]
        let s = Reader::new(&bytes).read_struct().unwrap();
        assert_eq!(s.list(1), &[Value::Bool(true)]);
    }

    #[test]
    fn rejects_truncated_and_oversized_input() {
        assert!(Reader::new(&[0x15]).read_struct().is_err());
        // A list claiming 200 elements with two bytes left.
        let err = Reader::new(&[0x19, 0xf5, 0xc8, 0x01, 0x00, 0x00])
            .read_struct()
            .unwrap_err();
        assert!(err.contains("exceeds"), "{err}");
        // Deeply nested structs.
        let mut deep = vec![0x1c; 100];
        deep.push(0x00);
        assert!(Reader::new(&deep)
            .read_struct()
            .unwrap_err()
            .contains("deep"));
    }
}
