//! XDR (RFC 4506) primitives used by the generated protocol code.
//!
//! Every item occupies a multiple of four bytes, big-endian. Variable-length
//! data is prefixed with its length and padded with zeros to four bytes.

use thiserror::Error;

#[derive(Debug, Error, PartialEq)]
pub enum Error {
    #[error("message truncated")]
    Truncated,
    #[error("length {0} exceeds the maximum of {1}")]
    TooLong(usize, usize),
    #[error("fixed-size array has {0} elements, expected {1}")]
    Length(usize, i64),
    #[error("invalid boolean {0}")]
    Bool(u32),
    #[error("unknown union discriminant {0}")]
    Discriminant(i32),
    #[error("{0} trailing bytes after message body")]
    Trailing(usize),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A value with an XDR representation.
pub trait Xdr: Sized {
    fn encode(&self, w: &mut Vec<u8>) -> Result<()>;
    fn decode(r: &mut Reader<'_>) -> Result<Self>;
}

/// Calls without arguments and replies without a body.
impl Xdr for () {
    fn encode(&self, _: &mut Vec<u8>) -> Result<()> {
        Ok(())
    }

    fn decode(_: &mut Reader<'_>) -> Result<Self> {
        Ok(())
    }
}

/// Decodes a complete message body, rejecting leftover bytes.
pub fn from_bytes<T: Xdr>(bytes: &[u8]) -> Result<T> {
    let mut r = Reader::new(bytes);
    let v = T::decode(&mut r)?;
    match r.remaining() {
        0 => Ok(v),
        n => Err(Error::Trailing(n)),
    }
}

pub fn to_bytes<T: Xdr>(v: &T) -> Result<Vec<u8>> {
    let mut w = Vec::new();
    v.encode(&mut w)?;
    Ok(w)
}

pub struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len()
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.buf.len() < n {
            return Err(Error::Truncated);
        }
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Ok(head)
    }
}

fn pad(n: usize) -> usize {
    (4 - n % 4) % 4
}

pub fn put_u32(w: &mut Vec<u8>, v: u32) {
    w.extend_from_slice(&v.to_be_bytes());
}

pub fn put_i32(w: &mut Vec<u8>, v: i32) {
    w.extend_from_slice(&v.to_be_bytes());
}

pub fn put_u64(w: &mut Vec<u8>, v: u64) {
    w.extend_from_slice(&v.to_be_bytes());
}

pub fn put_i64(w: &mut Vec<u8>, v: i64) {
    w.extend_from_slice(&v.to_be_bytes());
}

pub fn put_f32(w: &mut Vec<u8>, v: f32) {
    w.extend_from_slice(&v.to_be_bytes());
}

pub fn put_f64(w: &mut Vec<u8>, v: f64) {
    w.extend_from_slice(&v.to_be_bytes());
}

pub fn put_bool(w: &mut Vec<u8>, v: bool) {
    put_u32(w, u32::from(v));
}

pub fn put_len(w: &mut Vec<u8>, len: usize, max: usize) -> Result<()> {
    if len > max || len > u32::MAX as usize {
        return Err(Error::TooLong(len, max));
    }
    put_u32(w, len as u32);
    Ok(())
}

pub fn put_opaque_fixed(w: &mut Vec<u8>, v: &[u8]) {
    w.extend_from_slice(v);
    w.resize(w.len() + pad(v.len()), 0);
}

pub fn put_opaque(w: &mut Vec<u8>, v: &[u8], max: usize) -> Result<()> {
    put_len(w, v.len(), max)?;
    put_opaque_fixed(w, v);
    Ok(())
}

pub fn put_string(w: &mut Vec<u8>, v: &str, max: usize) -> Result<()> {
    put_opaque(w, v.as_bytes(), max)
}

pub fn get_u32(r: &mut Reader<'_>) -> Result<u32> {
    Ok(u32::from_be_bytes(r.take(4)?.try_into().expect("4 bytes")))
}

pub fn get_i32(r: &mut Reader<'_>) -> Result<i32> {
    Ok(i32::from_be_bytes(r.take(4)?.try_into().expect("4 bytes")))
}

pub fn get_u64(r: &mut Reader<'_>) -> Result<u64> {
    Ok(u64::from_be_bytes(r.take(8)?.try_into().expect("8 bytes")))
}

pub fn get_i64(r: &mut Reader<'_>) -> Result<i64> {
    Ok(i64::from_be_bytes(r.take(8)?.try_into().expect("8 bytes")))
}

pub fn get_f32(r: &mut Reader<'_>) -> Result<f32> {
    Ok(f32::from_be_bytes(r.take(4)?.try_into().expect("4 bytes")))
}

pub fn get_f64(r: &mut Reader<'_>) -> Result<f64> {
    Ok(f64::from_be_bytes(r.take(8)?.try_into().expect("8 bytes")))
}

pub fn get_bool(r: &mut Reader<'_>) -> Result<bool> {
    match get_u32(r)? {
        0 => Ok(false),
        1 => Ok(true),
        v => Err(Error::Bool(v)),
    }
}

pub fn get_len(r: &mut Reader<'_>, max: usize) -> Result<usize> {
    let n = get_u32(r)? as usize;
    if n > max {
        return Err(Error::TooLong(n, max));
    }
    Ok(n)
}

pub fn get_opaque_fixed<const N: usize>(r: &mut Reader<'_>) -> Result<[u8; N]> {
    let v = r.take(N)?.try_into().expect("N bytes");
    r.take(pad(N))?;
    Ok(v)
}

pub fn get_opaque(r: &mut Reader<'_>, max: usize) -> Result<Vec<u8>> {
    let n = get_len(r, max)?;
    let v = r.take(n)?.to_vec();
    r.take(pad(n))?;
    Ok(v)
}

/// Strings are C strings in libvirt and nearly always UTF-8; anything else is
/// converted lossily rather than failing the whole message.
pub fn get_string(r: &mut Reader<'_>, max: usize) -> Result<String> {
    let bytes = get_opaque(r, max)?;
    Ok(String::from_utf8(bytes)
        .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_are_big_endian_words() {
        let mut w = vec![];
        put_i32(&mut w, -2);
        put_u64(&mut w, 0x0102_0304_0506_0708);
        put_bool(&mut w, true);
        assert_eq!(
            w,
            [0xff, 0xff, 0xff, 0xfe, 1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 1]
        );
        let mut r = Reader::new(&w);
        assert_eq!(get_i32(&mut r), Ok(-2));
        assert_eq!(get_u64(&mut r), Ok(0x0102_0304_0506_0708));
        assert_eq!(get_bool(&mut r), Ok(true));
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn variable_data_is_length_prefixed_and_padded() {
        let mut w = vec![];
        put_string(&mut w, "abcde", 16).unwrap();
        assert_eq!(w, [0, 0, 0, 5, b'a', b'b', b'c', b'd', b'e', 0, 0, 0]);
        let mut r = Reader::new(&w);
        assert_eq!(get_string(&mut r, 16).unwrap(), "abcde");
        assert_eq!(r.remaining(), 0);

        let mut w = vec![];
        put_opaque_fixed(&mut w, &[9; 6]);
        assert_eq!(w.len(), 8);
        assert_eq!(get_opaque_fixed::<6>(&mut Reader::new(&w)).unwrap(), [9; 6]);
    }

    #[test]
    fn limits_and_truncation_are_errors() {
        assert_eq!(put_string(&mut vec![], "abc", 2), Err(Error::TooLong(3, 2)));
        let w = [0, 0, 0, 9, 1, 2];
        assert_eq!(
            get_opaque(&mut Reader::new(&w), 4),
            Err(Error::TooLong(9, 4))
        );
        assert_eq!(get_opaque(&mut Reader::new(&w), 64), Err(Error::Truncated));
        assert_eq!(
            get_bool(&mut Reader::new(&[0, 0, 0, 2])),
            Err(Error::Bool(2))
        );
        assert_eq!(from_bytes::<()>(&[0]), Err(Error::Trailing(1)));
    }
}
