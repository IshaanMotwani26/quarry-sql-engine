//! The PostgreSQL frontend/backend protocol, version 3: message framing and
//! the encodings of the messages Quarry sends and receives.
//!
//! After the startup packet, every message is a one-byte type tag, a 4-byte
//! big-endian length (counting itself but not the tag), and a body. Strings
//! are NUL-terminated.

use std::io::{self, Read, Write};

/// Messages larger than this are rejected, so a bad length can't make the
/// server allocate gigabytes.
const MAX_MESSAGE: usize = 64 << 20;

pub const PROTOCOL_3_0: i32 = 196_608;
pub const SSL_REQUEST: i32 = 80_877_103;
pub const GSSENC_REQUEST: i32 = 80_877_104;
pub const CANCEL_REQUEST: i32 = 80_877_102;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub tag: u8,
    pub body: Vec<u8>,
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

fn read_body(r: &mut impl Read, len: i32) -> io::Result<Vec<u8>> {
    let len = usize::try_from(len)
        .ok()
        .and_then(|n| n.checked_sub(4))
        .filter(|&n| n <= MAX_MESSAGE)
        .ok_or_else(|| invalid("invalid message length"))?;
    let mut body = vec![0; len];
    r.read_exact(&mut body)?;
    Ok(body)
}

/// Reads one tagged message. Returns `None` if the client closed the
/// connection cleanly between messages.
pub fn read_message(r: &mut impl Read) -> io::Result<Option<Message>> {
    let mut tag = [0u8; 1];
    if r.read(&mut tag)? == 0 {
        return Ok(None);
    }
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let body = read_body(r, i32::from_be_bytes(len))?;
    Ok(Some(Message { tag: tag[0], body }))
}

/// Reads the untagged startup packet: a length, then a protocol code, then
/// the rest. Returns the code and the rest, or `None` on a closed connection.
pub fn read_startup(r: &mut impl Read) -> io::Result<Option<(i32, Vec<u8>)>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        other => other?,
    }
    let body = read_body(r, i32::from_be_bytes(len))?;
    if body.len() < 4 {
        return Err(invalid("startup packet is too short"));
    }
    let code = i32::from_be_bytes(body[..4].try_into().unwrap());
    Ok(Some((code, body[4..].to_vec())))
}

/// A cursor over a message body.
pub struct Body<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Body<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Body { data, pos: 0 }
    }

    pub fn is_empty(&self) -> bool {
        self.pos >= self.data.len()
    }

    pub fn bytes(&mut self, n: usize) -> io::Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&e| e <= self.data.len())
            .ok_or_else(|| invalid("message is shorter than its contents"))?;
        let out = &self.data[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    pub fn u8(&mut self) -> io::Result<u8> {
        Ok(self.bytes(1)?[0])
    }

    pub fn i16(&mut self) -> io::Result<i16> {
        Ok(i16::from_be_bytes(self.bytes(2)?.try_into().unwrap()))
    }

    pub fn i32(&mut self) -> io::Result<i32> {
        Ok(i32::from_be_bytes(self.bytes(4)?.try_into().unwrap()))
    }

    /// A NUL-terminated string.
    pub fn cstr(&mut self) -> io::Result<String> {
        let rest = &self.data[self.pos.min(self.data.len())..];
        let n = rest
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| invalid("string is missing its terminator"))?;
        let s = std::str::from_utf8(&rest[..n]).map_err(|_| invalid("string is not UTF-8"))?;
        self.pos += n + 1;
        Ok(s.to_string())
    }
}

/// Parses the startup packet's parameters: name/value string pairs ending
/// with an empty name.
pub fn startup_params(body: &[u8]) -> io::Result<Vec<(String, String)>> {
    let mut b = Body::new(body);
    let mut params = Vec::new();
    loop {
        let name = b.cstr()?;
        if name.is_empty() {
            return Ok(params);
        }
        params.push((name, b.cstr()?));
    }
}

/// Buffers outgoing messages so a whole response goes out in one write.
#[derive(Default)]
pub struct Out {
    buf: Vec<u8>,
}

impl Out {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a message whose body is written by `f`; the length is filled
    /// in afterwards.
    pub fn msg(&mut self, tag: u8, f: impl FnOnce(&mut Writer)) {
        self.buf.push(tag);
        let start = self.buf.len();
        self.buf.extend([0; 4]);
        f(&mut Writer(&mut self.buf));
        let len = (self.buf.len() - start) as i32;
        self.buf[start..start + 4].copy_from_slice(&len.to_be_bytes());
    }

    /// A raw byte with no framing, the reply to an SSL request.
    pub fn raw(&mut self, b: u8) {
        self.buf.push(b);
    }

    pub fn flush_to(&mut self, w: &mut impl Write) -> io::Result<()> {
        if !self.buf.is_empty() {
            w.write_all(&self.buf)?;
            w.flush()?;
            self.buf.clear();
        }
        Ok(())
    }

    pub fn bytes(&self) -> &[u8] {
        &self.buf
    }
}

pub struct Writer<'a>(&'a mut Vec<u8>);

impl Writer<'_> {
    pub fn u8(&mut self, b: u8) {
        self.0.push(b);
    }

    pub fn i16(&mut self, n: i16) {
        self.0.extend(n.to_be_bytes());
    }

    pub fn i32(&mut self, n: i32) {
        self.0.extend(n.to_be_bytes());
    }

    pub fn bytes(&mut self, b: &[u8]) {
        self.0.extend_from_slice(b);
    }

    pub fn cstr(&mut self, s: &str) {
        // A NUL inside the string would end it early; drop any.
        self.0.extend(s.bytes().filter(|&b| b != 0));
        self.0.push(0);
    }
}

// Backend messages.

pub fn authentication_ok(out: &mut Out) {
    out.msg(b'R', |w| w.i32(0));
}

pub fn parameter_status(out: &mut Out, name: &str, value: &str) {
    out.msg(b'S', |w| {
        w.cstr(name);
        w.cstr(value);
    });
}

pub fn backend_key_data(out: &mut Out, pid: i32, secret: i32) {
    out.msg(b'K', |w| {
        w.i32(pid);
        w.i32(secret);
    });
}

/// `status` is b'I' (idle), b'T' (in a transaction block), or b'E' (in a
/// failed transaction block).
pub fn ready_for_query(out: &mut Out, status: u8) {
    out.msg(b'Z', |w| w.u8(status));
}

pub fn command_complete(out: &mut Out, tag: &str) {
    out.msg(b'C', |w| w.cstr(tag));
}

pub fn empty_query_response(out: &mut Out) {
    out.msg(b'I', |_| {});
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgError {
    /// The five-character SQLSTATE code, such as 42601 for a syntax error.
    pub code: &'static str,
    pub message: String,
    /// 1-based character position in the query, for syntax errors.
    pub position: Option<usize>,
}

impl PgError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        PgError {
            code,
            message: message.into(),
            position: None,
        }
    }
}

pub fn error_response(out: &mut Out, e: &PgError) {
    out.msg(b'E', |w| {
        for (field, value) in [(b'S', "ERROR"), (b'V', "ERROR"), (b'C', e.code)] {
            w.u8(field);
            w.cstr(value);
        }
        w.u8(b'M');
        w.cstr(&e.message);
        if let Some(p) = e.position {
            w.u8(b'P');
            w.cstr(&p.to_string());
        }
        w.u8(0);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_messages_with_their_length() {
        let mut out = Out::new();
        command_complete(&mut out, "SELECT 1");
        ready_for_query(&mut out, b'I');
        assert_eq!(out.bytes(), b"C\0\0\0\x0dSELECT 1\0Z\0\0\0\x05I");

        let mut r = out.bytes();
        let m = read_message(&mut r).unwrap().unwrap();
        assert_eq!((m.tag, m.body.as_slice()), (b'C', &b"SELECT 1\0"[..]));
        assert_eq!(read_message(&mut r).unwrap().unwrap().body, b"I");
        assert_eq!(read_message(&mut r).unwrap(), None);
    }

    #[test]
    fn reads_startup_packets() {
        let mut packet = Vec::new();
        let params = b"user\0ada\0database\0db\0\0";
        packet.extend((8 + params.len() as i32).to_be_bytes());
        packet.extend(PROTOCOL_3_0.to_be_bytes());
        packet.extend(params);
        let (code, body) = read_startup(&mut packet.as_slice()).unwrap().unwrap();
        assert_eq!(code, PROTOCOL_3_0);
        assert_eq!(
            startup_params(&body).unwrap(),
            [
                ("user".into(), "ada".into()),
                ("database".into(), "db".into())
            ]
        );
    }

    #[test]
    fn rejects_bad_lengths() {
        let huge = [b'Q', 0x7f, 0xff, 0xff, 0xff];
        assert!(read_message(&mut huge.as_slice()).is_err());
        let negative = [b'Q', 0xff, 0xff, 0xff, 0xff];
        assert!(read_message(&mut negative.as_slice()).is_err());
        let truncated = [b'Q', 0, 0, 0, 9, b'a'];
        assert!(read_message(&mut truncated.as_slice()).is_err());
        assert!(Body::new(b"no terminator").cstr().is_err());
    }
}
