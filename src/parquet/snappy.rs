//! Snappy decompression (the raw format, which Parquet uses for its pages).
//!
//! A compressed block is the uncompressed length as a varint, then a series
//! of elements. Each element starts with a tag byte whose low two bits say
//! what it is: a literal (bytes copied from the input) or a copy (bytes
//! repeated from earlier in the output, at some offset back).

pub fn decompress(input: &[u8]) -> Result<Vec<u8>, String> {
    let corrupt = |what: &str| format!("corrupt Snappy data: {what}");
    let (len, mut pos) = varint(input).ok_or_else(|| corrupt("bad length"))?;
    // Every input byte expands to at most 64 output bytes (a 3-byte copy of
    // 64), so a larger claimed length means corrupt input.
    if len > input.len() as u64 * 64 {
        return Err(corrupt("length is too large"));
    }
    let len = len as usize;
    let mut out = Vec::with_capacity(len);

    while pos < input.len() {
        let tag = input[pos];
        pos += 1;
        let (length, offset) = match tag & 0b11 {
            0 => {
                // Literal. Lengths up to 60 fit in the tag; longer ones
                // follow in 1 to 4 little-endian bytes.
                let mut n = (tag >> 2) as usize;
                if n >= 60 {
                    let extra = n - 59;
                    let bytes = input
                        .get(pos..pos + extra)
                        .ok_or_else(|| corrupt("literal length runs past the input"))?;
                    pos += extra;
                    n = le(bytes);
                }
                let n = n + 1;
                let lit = pos
                    .checked_add(n)
                    .and_then(|end| input.get(pos..end))
                    .ok_or_else(|| corrupt("literal runs past the input"))?;
                pos += n;
                out.extend_from_slice(lit);
                continue;
            }
            1 => {
                // Copy with an 11-bit offset: length 4..11.
                let b = *input
                    .get(pos)
                    .ok_or_else(|| corrupt("copy runs past the input"))?;
                pos += 1;
                let length = 4 + ((tag >> 2) & 0b111) as usize;
                let offset = ((tag as usize >> 5) << 8) | b as usize;
                (length, offset)
            }
            k => {
                // Copy with a 16-bit (k = 2) or 32-bit (k = 3) offset.
                let width = if k == 2 { 2 } else { 4 };
                let bytes = input
                    .get(pos..pos + width)
                    .ok_or_else(|| corrupt("copy runs past the input"))?;
                pos += width;
                ((tag >> 2) as usize + 1, le(bytes))
            }
        };
        if offset == 0 || offset > out.len() {
            return Err(corrupt("copy offset is out of range"));
        }
        // Byte by byte, since a copy may overlap its own output (offset 1,
        // length 10 repeats one byte ten times).
        let start = out.len() - offset;
        for i in 0..length {
            let b = out[start + i];
            out.push(b);
        }
        if out.len() > len {
            return Err(corrupt("output is longer than its stated length"));
        }
    }
    if out.len() != len {
        return Err(corrupt("output is shorter than its stated length"));
    }
    Ok(out)
}

fn le(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .rev()
        .fold(0, |acc, &b| (acc << 8) | b as usize)
}

fn varint(data: &[u8]) -> Option<(u64, usize)> {
    let mut out = 0u64;
    for (i, b) in data.iter().take(5).enumerate() {
        out |= u64::from(b & 0x7f) << (7 * i);
        if b & 0x80 == 0 {
            return Some((out, i + 1));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_and_copies() {
        // "abcd" as a literal, then a 1-byte-offset copy of length 8 at
        // offset 4: "abcdabcdabcd".
        let data = [12, 0x0c, b'a', b'b', b'c', b'd', 0x11, 0x04];
        assert_eq!(decompress(&data).unwrap(), b"abcdabcdabcd");
    }

    #[test]
    fn overlapping_copy_repeats_bytes() {
        // "a", then a 2-byte-offset copy of length 9 at offset 1.
        let data = [10, 0x00, b'a', 0x22, 0x01, 0x00];
        assert_eq!(decompress(&data).unwrap(), b"aaaaaaaaaa");
    }

    #[test]
    fn long_literal_length() {
        let text = vec![b'x'; 100];
        let mut data = vec![100, 60 << 2, 99];
        data.extend(&text);
        assert_eq!(decompress(&data).unwrap(), text);
    }

    #[test]
    fn rejects_corrupt_input() {
        assert!(decompress(&[5, 0x00, b'a']).is_err()); // too short
        assert!(decompress(&[4, 0x11, 0x09]).is_err()); // copy before any output
        assert!(decompress(&[1, 0x04, b'a']).is_err()); // literal past the input
        assert!(decompress(&[0xff, 0xff, 0xff, 0xff, 0x0f]).is_err()); // huge length
    }
}
