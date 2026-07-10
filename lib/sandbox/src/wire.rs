//! Bounded, fail-closed cursor encoding for the sandbox protocol payloads.
//!
//! Every payload that crosses the sandbox boundary is encoded and decoded
//! through these two cursors. The reader treats its input as hostile — a
//! reply comes from a worker that has parsed attacker-controlled bytes and
//! may itself be compromised — so every read is bounds-checked against the
//! remaining input and every variable-length field is checked against the
//! caller's stated cap *before* any bytes are copied. A short, oversize,
//! or malformed field is the typed [`WireError`], never a panic and never
//! a partial trust of later bytes.
//!
//! Scalars are little-endian, like every RustOS wire format.

use alloc::string::String;
use alloc::vec::Vec;

/// Typed decode failure: the input ended early, or a variable-length field
/// exceeded its fixed cap. The two cases are deliberately distinguished so
/// a test can assert *why* a hostile payload was refused.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum WireError {
    /// Fewer bytes remained than the field requires.
    Truncated,
    /// A length prefix exceeded the field's fixed cap, or a string field
    /// held invalid UTF-8.
    Malformed,
}

/// Append-only payload encoder over a growable byte vector.
#[derive(Debug, Default)]
pub struct Writer {
    out: Vec<u8>,
}

impl Writer {
    /// Start an empty payload.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Finish, yielding the encoded payload.
    #[must_use]
    pub fn finish(self) -> Vec<u8> {
        self.out
    }

    /// Append one byte.
    pub fn u8(&mut self, value: u8) {
        self.out.push(value);
    }

    /// Append a little-endian `u32`.
    pub fn u32(&mut self, value: u32) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    /// Append a little-endian `u64`.
    pub fn u64(&mut self, value: u64) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    /// Append a length-prefixed byte string (`u32` length, then the bytes).
    pub fn bytes(&mut self, value: &[u8]) {
        // Payload fields are bounded well below `u32::MAX` by the frame
        // cap; a longer slice cannot reach here through the public
        // encoders, so saturating keeps the encoder total without an
        // unchecked cast.
        let len = u32::try_from(value.len()).unwrap_or(u32::MAX);
        self.u32(len);
        self.out.extend_from_slice(value);
    }

    /// Append a length-prefixed UTF-8 string.
    pub fn str(&mut self, value: &str) {
        self.bytes(value.as_bytes());
    }
}

/// Bounds-checked payload decoder over a borrowed byte slice.
#[derive(Debug)]
pub struct Reader<'a> {
    input: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    /// Start reading `input` from its first byte.
    #[must_use]
    pub fn new(input: &'a [u8]) -> Self {
        Self { input, at: 0 }
    }

    /// Whether every input byte has been consumed.
    ///
    /// Decoders check this last, so a payload with trailing bytes —
    /// a shape no honest encoder produces — is refused rather than
    /// silently accepted.
    #[must_use]
    pub fn is_exhausted(&self) -> bool {
        self.at == self.input.len()
    }

    /// Consume `len` raw bytes.
    ///
    /// # Errors
    ///
    /// [`WireError::Truncated`] when fewer than `len` bytes remain.
    pub fn take(&mut self, len: usize) -> Result<&'a [u8], WireError> {
        let end = self.at.checked_add(len).ok_or(WireError::Truncated)?;
        if end > self.input.len() {
            return Err(WireError::Truncated);
        }
        let slice = &self.input[self.at..end];
        self.at = end;
        Ok(slice)
    }

    /// Consume one byte.
    ///
    /// # Errors
    ///
    /// [`WireError::Truncated`] when the input is exhausted.
    pub fn u8(&mut self) -> Result<u8, WireError> {
        Ok(self.take(1)?[0])
    }

    /// Consume a little-endian `u32`.
    ///
    /// # Errors
    ///
    /// [`WireError::Truncated`] when fewer than four bytes remain.
    pub fn u32(&mut self) -> Result<u32, WireError> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// Consume a little-endian `u64`.
    ///
    /// # Errors
    ///
    /// [`WireError::Truncated`] when fewer than eight bytes remain.
    pub fn u64(&mut self) -> Result<u64, WireError> {
        let bytes = self.take(8)?;
        Ok(u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    /// Consume a length-prefixed byte string of at most `cap` bytes.
    ///
    /// # Errors
    ///
    /// [`WireError::Malformed`] when the length prefix exceeds `cap`;
    /// [`WireError::Truncated`] when the declared bytes are not all
    /// present. The cap is checked before any bytes are consumed, so a
    /// hostile length can never drive a large read or allocation.
    pub fn bytes(&mut self, cap: usize) -> Result<&'a [u8], WireError> {
        let len = self.u32()? as usize;
        if len > cap {
            return Err(WireError::Malformed);
        }
        self.take(len)
    }

    /// Consume a length-prefixed UTF-8 string of at most `cap` bytes.
    ///
    /// # Errors
    ///
    /// As [`Reader::bytes`], plus [`WireError::Malformed`] for invalid
    /// UTF-8.
    pub fn string(&mut self, cap: usize) -> Result<String, WireError> {
        let bytes = self.bytes(cap)?;
        core::str::from_utf8(bytes)
            .map(String::from)
            .map_err(|_| WireError::Malformed)
    }
}

#[cfg(test)]
mod tests {
    use super::{Reader, WireError, Writer};
    use alloc::string::String;
    use alloc::vec;

    #[test]
    fn round_trips_every_scalar_and_field_shape() {
        let mut w = Writer::new();
        w.u8(0xAB);
        w.u32(0xDEAD_BEEF);
        w.u64(0x0123_4567_89AB_CDEF);
        w.bytes(b"raw");
        w.str("text");
        let payload = w.finish();

        let mut r = Reader::new(&payload);
        assert_eq!(r.u8(), Ok(0xAB));
        assert_eq!(r.u32(), Ok(0xDEAD_BEEF));
        assert_eq!(r.u64(), Ok(0x0123_4567_89AB_CDEF));
        assert_eq!(r.bytes(16), Ok(&b"raw"[..]));
        assert_eq!(r.string(16), Ok(String::from("text")));
        assert!(r.is_exhausted());
    }

    #[test]
    fn every_truncation_point_is_refused() {
        let mut w = Writer::new();
        w.u32(7);
        w.str("seven");
        let payload = w.finish();
        // Every proper prefix fails closed; only the full payload decodes.
        for cut in 0..payload.len() {
            let mut r = Reader::new(&payload[..cut]);
            let outcome = r.u32().and_then(|_| r.string(16).map(|_| ()));
            assert!(outcome.is_err(), "prefix {cut} must be refused");
        }
    }

    #[test]
    fn a_length_prefix_over_the_cap_is_malformed_before_any_read() {
        let mut w = Writer::new();
        w.bytes(&[0u8; 32]);
        let payload = w.finish();
        let mut r = Reader::new(&payload);
        assert_eq!(r.bytes(31), Err(WireError::Malformed));
    }

    #[test]
    fn a_hostile_length_prefix_cannot_demand_a_huge_read() {
        // A four-byte payload claiming u32::MAX following bytes: the cap
        // check refuses it before any allocation or read.
        let payload = u32::MAX.to_le_bytes();
        let mut r = Reader::new(&payload);
        assert_eq!(r.bytes(64), Err(WireError::Malformed));
    }

    #[test]
    fn invalid_utf8_in_a_string_field_is_malformed() {
        let mut w = Writer::new();
        w.bytes(&[0xFF, 0xFE]);
        let payload = w.finish();
        let mut r = Reader::new(&payload);
        assert_eq!(r.string(16), Err(WireError::Malformed));
    }

    #[test]
    fn trailing_bytes_are_visible_to_the_exhaustion_check() {
        let payload = vec![0u8; 3];
        let mut r = Reader::new(&payload);
        assert_eq!(r.u8(), Ok(0));
        assert!(!r.is_exhausted());
    }
}
