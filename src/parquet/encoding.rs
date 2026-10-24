//! Parquet's value encodings.
//!
//! - The RLE / bit-packing hybrid stores small integers (definition levels,
//!   dictionary indices, booleans) as a mix of runs of one repeated value and
//!   groups of 8 values packed at a fixed bit width.
//! - PLAIN stores values back to back: little-endian numbers, bit-packed
//!   booleans, and length-prefixed byte arrays.

/// Decodes `count` values of `bit_width` bits from the RLE / bit-packing
/// hybrid encoding. Returns the values and the number of bytes consumed.
pub fn decode_hybrid(
    data: &[u8],
    bit_width: u32,
    count: usize,
) -> Result<(Vec<u32>, usize), String> {
    if bit_width > 32 {
        return Err(format!("corrupt Parquet page: bit width {bit_width}"));
    }
    let truncated = || "corrupt Parquet page: RLE data ends early".to_string();
    let value_bytes = bit_width.div_ceil(8) as usize;
    let mut out = Vec::with_capacity(count.min(data.len() * 8 + 8));
    let mut pos = 0;
    while out.len() < count {
        let (header, n) = varint(&data[pos..]).ok_or_else(truncated)?;
        pos += n;
        if header & 1 == 0 {
            // RLE run: one value repeated `header >> 1` times.
            let run = (header >> 1) as usize;
            let bytes = data.get(pos..pos + value_bytes).ok_or_else(truncated)?;
            pos += value_bytes;
            let mut v = 0u32;
            for (i, b) in bytes.iter().enumerate() {
                v |= u32::from(*b) << (8 * i);
            }
            if run == 0 {
                return Err("corrupt Parquet page: empty RLE run".into());
            }
            let take = run.min(count - out.len());
            out.extend(std::iter::repeat_n(v, take));
        } else {
            // Bit-packed run: `header >> 1` groups of 8 values, least
            // significant bit first.
            let groups = (header >> 1) as usize;
            let len = groups
                .checked_mul(bit_width as usize)
                .ok_or_else(truncated)?;
            let end = pos.checked_add(len).ok_or_else(truncated)?;
            let packed = data.get(pos..end).ok_or_else(truncated)?;
            pos = end;
            let take = (groups * 8).min(count - out.len());
            unpack(packed, bit_width, take, &mut out);
        }
    }
    Ok((out, pos))
}

fn unpack(packed: &[u8], bit_width: u32, count: usize, out: &mut Vec<u32>) {
    if bit_width == 0 {
        out.extend(std::iter::repeat_n(0, count));
        return;
    }
    let mask = (1u64 << bit_width) - 1;
    let mut acc = 0u64;
    let mut bits = 0;
    let mut bytes = packed.iter();
    for _ in 0..count {
        while bits < bit_width {
            acc |= u64::from(*bytes.next().unwrap_or(&0)) << bits;
            bits += 8;
        }
        out.push((acc & mask) as u32);
        acc >>= bit_width;
        bits -= bit_width;
    }
}

/// An unsigned LEB128 varint and its length in bytes.
fn varint(data: &[u8]) -> Option<(u64, usize)> {
    let mut out = 0u64;
    for (i, b) in data.iter().take(10).enumerate() {
        out |= u64::from(b & 0x7f) << (7 * i);
        if b & 0x80 == 0 {
            return Some((out, i + 1));
        }
    }
    None
}

/// Number of bits needed to store values up to `max`.
pub fn bit_width(max: u32) -> u32 {
    32 - max.leading_zeros()
}

/// A cursor over PLAIN-encoded values.
pub struct Plain<'a> {
    data: &'a [u8],
    pos: usize,
    /// Bit offset into the current byte, for booleans.
    bit: u32,
}

impl<'a> Plain<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Plain {
            data,
            pos: 0,
            bit: 0,
        }
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N], String> {
        let bytes = self.bytes(N)?;
        Ok(bytes.try_into().unwrap())
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&e| e <= self.data.len())
            .ok_or("corrupt Parquet page: values end early")?;
        let out = &self.data[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    pub fn boolean(&mut self) -> Result<bool, String> {
        let byte = *self
            .data
            .get(self.pos)
            .ok_or("corrupt Parquet page: values end early")?;
        let v = byte >> self.bit & 1 == 1;
        self.bit += 1;
        if self.bit == 8 {
            self.bit = 0;
            self.pos += 1;
        }
        Ok(v)
    }

    pub fn i32(&mut self) -> Result<i32, String> {
        Ok(i32::from_le_bytes(self.take()?))
    }

    pub fn i64(&mut self) -> Result<i64, String> {
        Ok(i64::from_le_bytes(self.take()?))
    }

    pub fn f32(&mut self) -> Result<f32, String> {
        Ok(f32::from_le_bytes(self.take()?))
    }

    pub fn f64(&mut self) -> Result<f64, String> {
        Ok(f64::from_le_bytes(self.take()?))
    }

    /// A BYTE_ARRAY value: a 4-byte little-endian length, then the bytes.
    pub fn byte_array(&mut self) -> Result<&'a [u8], String> {
        let len = u32::from_le_bytes(self.take()?) as usize;
        self.bytes(len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hybrid_rle_and_bit_packed_runs() {
        // RLE run of five 3s, then one bit-packed group of 0..8 at width 3.
        let data = [
            0x0a, 0x03, // header 5 << 1, value 3
            0x03, 0x88, 0xc6, 0xfa, // header (1 << 1) | 1, packed 0..8
        ];
        let (values, used) = decode_hybrid(&data, 3, 13).unwrap();
        assert_eq!(values, [3, 3, 3, 3, 3, 0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(used, data.len());
        // Asking for fewer values stops partway through a run.
        assert_eq!(decode_hybrid(&data, 3, 2).unwrap().0, [3, 3]);
        assert!(decode_hybrid(&data, 3, 20).is_err());
    }

    #[test]
    fn hybrid_zero_bit_width_is_all_zeros() {
        let (values, _) = decode_hybrid(&[0x08], 0, 4).unwrap();
        assert_eq!(values, [0, 0, 0, 0]);
    }

    #[test]
    fn bit_widths() {
        assert_eq!(bit_width(0), 0);
        assert_eq!(bit_width(1), 1);
        assert_eq!(bit_width(7), 3);
        assert_eq!(bit_width(8), 4);
    }

    #[test]
    fn plain_values() {
        let mut data = vec![0b0000_0101];
        data.extend(7i32.to_le_bytes());
        data.extend(3u32.to_le_bytes());
        data.extend(b"abc");
        let mut p = Plain::new(&data);
        assert!(p.boolean().unwrap());
        assert!(!p.boolean().unwrap());
        assert!(p.boolean().unwrap());
        let mut p = Plain::new(&data[1..]);
        assert_eq!(p.i32().unwrap(), 7);
        assert_eq!(p.byte_array().unwrap(), b"abc");
        assert!(p.i32().is_err());
    }
}
