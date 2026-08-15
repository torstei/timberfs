//! A minimal protobuf wire codec: varints, length-delimited fields and
//! the fixed widths — no schema, no code generation, no build step.
//!
//! It exists because OTLP's log schema is small, fixed and stable (a
//! dozen messages, none of them changing shape), while a generated
//! binding would pull a code-generation toolchain into a crate that has
//! deliberately stayed at a handful of dependencies. What the wire format
//! demands is exactly this file: read a key, act on the field number you
//! know, skip by wire type the ones you do not.
//!
//! Skipping unknown fields by wire type is the whole of forward
//! compatibility here: a newer sender's extra fields cost nothing and
//! break nothing, which is the same property `.bark` and the record
//! stream already rely on.

use anyhow::{bail, Result};

pub const WIRE_VARINT: u8 = 0;
pub const WIRE_64BIT: u8 = 1;
pub const WIRE_LEN: u8 = 2;
pub const WIRE_32BIT: u8 = 5;

pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Reader<'a> {
        Reader { buf, pos: 0 }
    }

    pub fn done(&self) -> bool {
        self.pos >= self.buf.len()
    }

    pub fn varint(&mut self) -> Result<u64> {
        let mut out: u64 = 0;
        let mut shift = 0;
        loop {
            if self.pos >= self.buf.len() {
                bail!("truncated varint");
            }
            let b = self.buf[self.pos];
            self.pos += 1;
            out |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(out);
            }
            shift += 7;
            if shift >= 64 {
                bail!("varint overflows 64 bits");
            }
        }
    }

    /// Field number and wire type of the next field.
    pub fn key(&mut self) -> Result<(u32, u8)> {
        let k = self.varint()?;
        let field = (k >> 3) as u32;
        if field == 0 {
            bail!("field number 0 is not valid");
        }
        Ok((field, (k & 7) as u8))
    }

    pub fn bytes(&mut self) -> Result<&'a [u8]> {
        let len = self.varint()? as usize;
        let end = self
            .pos
            .checked_add(len)
            .filter(|e| *e <= self.buf.len())
            .ok_or_else(|| anyhow::anyhow!("length-delimited field runs past the message"))?;
        let out = &self.buf[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    pub fn string(&mut self) -> Result<String> {
        Ok(String::from_utf8_lossy(self.bytes()?).into_owned())
    }

    pub fn fixed64(&mut self) -> Result<u64> {
        if self.pos + 8 > self.buf.len() {
            bail!("truncated 64-bit field");
        }
        let v = u64::from_le_bytes(self.buf[self.pos..self.pos + 8].try_into().unwrap());
        self.pos += 8;
        Ok(v)
    }

    pub fn fixed32(&mut self) -> Result<u32> {
        if self.pos + 4 > self.buf.len() {
            bail!("truncated 32-bit field");
        }
        let v = u32::from_le_bytes(self.buf[self.pos..self.pos + 4].try_into().unwrap());
        self.pos += 4;
        Ok(v)
    }

    /// Step over a field this decoder does not know. Anything else would
    /// make a newer sender's message unreadable.
    pub fn skip(&mut self, wire: u8) -> Result<()> {
        match wire {
            WIRE_VARINT => {
                self.varint()?;
            }
            WIRE_64BIT => {
                self.fixed64()?;
            }
            WIRE_LEN => {
                self.bytes()?;
            }
            WIRE_32BIT => {
                self.fixed32()?;
            }
            other => bail!("unsupported wire type {other}"),
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Writer {
        Writer { buf: Vec::new() }
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    fn put_varint(&mut self, mut v: u64) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                self.buf.push(b);
                return;
            }
            self.buf.push(b | 0x80);
        }
    }

    fn key(&mut self, field: u32, wire: u8) {
        self.put_varint(((field as u64) << 3) | wire as u64);
    }

    pub fn varint_field(&mut self, field: u32, v: u64) {
        self.key(field, WIRE_VARINT);
        self.put_varint(v);
    }

    pub fn fixed64_field(&mut self, field: u32, v: u64) {
        self.key(field, WIRE_64BIT);
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn bytes_field(&mut self, field: u32, b: &[u8]) {
        self.key(field, WIRE_LEN);
        self.put_varint(b.len() as u64);
        self.buf.extend_from_slice(b);
    }

    pub fn string_field(&mut self, field: u32, s: &str) {
        self.bytes_field(field, s.as_bytes());
    }

    /// A nested message: built into its own buffer, since a
    /// length-delimited field has to know its length before its content.
    pub fn message_field(&mut self, field: u32, build: impl FnOnce(&mut Writer)) {
        let mut inner = Writer::new();
        build(&mut inner);
        self.bytes_field(field, &inner.buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_roundtrip_across_the_boundaries() {
        for v in [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX] {
            let mut w = Writer::new();
            w.varint_field(1, v);
            let bytes = w.into_bytes();
            let mut r = Reader::new(&bytes);
            let (field, wire) = r.key().unwrap();
            assert_eq!((field, wire), (1, WIRE_VARINT));
            assert_eq!(r.varint().unwrap(), v);
            assert!(r.done());
        }
    }

    #[test]
    fn every_field_shape_roundtrips() {
        let mut w = Writer::new();
        w.string_field(1, "hello");
        w.fixed64_field(2, 1_700_000_000_123_000_000);
        w.varint_field(3, 17);
        w.bytes_field(4, &[0xde, 0xad]);
        w.message_field(5, |m| {
            m.string_field(1, "nested");
            m.varint_field(2, 1);
        });
        let bytes = w.into_bytes();

        let mut r = Reader::new(&bytes);
        assert_eq!(r.key().unwrap(), (1, WIRE_LEN));
        assert_eq!(r.string().unwrap(), "hello");
        assert_eq!(r.key().unwrap(), (2, WIRE_64BIT));
        assert_eq!(r.fixed64().unwrap(), 1_700_000_000_123_000_000);
        assert_eq!(r.key().unwrap(), (3, WIRE_VARINT));
        assert_eq!(r.varint().unwrap(), 17);
        assert_eq!(r.key().unwrap(), (4, WIRE_LEN));
        assert_eq!(r.bytes().unwrap(), &[0xde, 0xad]);
        assert_eq!(r.key().unwrap(), (5, WIRE_LEN));
        let nested = r.bytes().unwrap();
        assert!(r.done());
        let mut n = Reader::new(nested);
        assert_eq!(n.key().unwrap(), (1, WIRE_LEN));
        assert_eq!(n.string().unwrap(), "nested");
    }

    #[test]
    fn unknown_fields_are_stepped_over_by_wire_type() {
        // What a newer sender's message looks like to this decoder: it
        // must reach field 9 without knowing 6, 7 or 8.
        let mut w = Writer::new();
        w.varint_field(6, 99);
        w.fixed64_field(7, 1);
        w.string_field(8, "unknown to us");
        w.string_field(9, "the one we want");
        let bytes = w.into_bytes();

        let mut r = Reader::new(&bytes);
        let mut found = None;
        while !r.done() {
            let (field, wire) = r.key().unwrap();
            if field == 9 {
                found = Some(r.string().unwrap());
            } else {
                r.skip(wire).unwrap();
            }
        }
        assert_eq!(found.as_deref(), Some("the one we want"));
    }

    #[test]
    fn truncation_is_an_error_not_a_short_read() {
        let mut w = Writer::new();
        w.string_field(1, "hello");
        let bytes = w.into_bytes();
        let mut r = Reader::new(&bytes[..bytes.len() - 2]);
        assert_eq!(r.key().unwrap(), (1, WIRE_LEN));
        assert!(r.bytes().is_err());
        // A varint that never terminates.
        let mut r = Reader::new(&[0x80, 0x80]);
        assert!(r.varint().is_err());
    }
}
