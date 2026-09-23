//! The RFC 4251 §5 data types every SSH message is built from.
//!
//! Decoding is total and fails closed. Every length is checked against what
//! remains before anything is read, an `mpint` must be in the one canonical
//! form the RFC permits, and a `name-list` must hold only names RFC 4250 §4.6.1
//! allows — so a value that decodes re-encodes to exactly the bytes it came
//! from, and a parse never depends on an attacker's choice between equivalent
//! spellings.

use alloc::vec::Vec;
use core::fmt;

/// The longest algorithm or method name RFC 4250 §4.6.1 permits.
pub const MAX_NAME_LEN: usize = 64;

/// Why a value could not be read or written.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum WireError {
    /// The input ended inside a value.
    Truncated,
    /// An `mpint` carried a leading byte its sign did not need, or spelled
    /// zero as anything but the empty string.
    NonCanonicalMpint,
    /// A `name-list` held a name that was empty, over-long, not printable
    /// US-ASCII, or carried more than one `@` or one at either end.
    InvalidName,
    /// A `string` required to be UTF-8 was not.
    InvalidUtf8,
    /// Bytes remained after the last field of a message.
    TrailingData,
    /// A value too long for its `uint32` length prefix.
    TooLong,
    /// The allocator refused the room a value needed.
    OutOfMemory,
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Truncated => "input ended inside a value",
            Self::NonCanonicalMpint => "mpint is not in canonical form",
            Self::InvalidName => "name-list holds a name RFC 4250 does not allow",
            Self::InvalidUtf8 => "string is not UTF-8",
            Self::TrailingData => "bytes follow the last field",
            Self::TooLong => "value too long for its length prefix",
            Self::OutOfMemory => "out of memory",
        })
    }
}

/// Reads RFC 4251 values from the front of a byte slice.
///
/// Every value borrows from the input; nothing is copied or allocated.
#[derive(Clone, Debug)]
pub struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    /// A reader over `bytes`.
    #[must_use]
    pub const fn new(bytes: &'a [u8]) -> Self {
        Self { rest: bytes }
    }

    /// Bytes not yet read.
    #[must_use]
    pub const fn remaining(&self) -> usize {
        self.rest.len()
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        if n > self.rest.len() {
            return Err(WireError::Truncated);
        }
        let (head, tail) = self.rest.split_at(n);
        self.rest = tail;
        Ok(head)
    }

    /// A `byte`.
    ///
    /// # Errors
    ///
    /// [`WireError::Truncated`] at the end of the input.
    pub fn byte(&mut self) -> Result<u8, WireError> {
        Ok(self.fixed::<1>()?[0])
    }

    /// A `byte[N]`: a fixed-length run whose length the message defines.
    ///
    /// # Errors
    ///
    /// [`WireError::Truncated`] when fewer than `N` bytes remain.
    pub fn fixed<const N: usize>(&mut self) -> Result<&'a [u8; N], WireError> {
        self.take(N)?.try_into().map_err(|_| WireError::Truncated)
    }

    /// A `boolean`. Any non-zero byte is true, as RFC 4251 requires of a
    /// reader.
    ///
    /// # Errors
    ///
    /// [`WireError::Truncated`] at the end of the input.
    pub fn boolean(&mut self) -> Result<bool, WireError> {
        Ok(self.byte()? != 0)
    }

    /// A `uint32`.
    ///
    /// # Errors
    ///
    /// [`WireError::Truncated`] when fewer than four bytes remain.
    pub fn uint32(&mut self) -> Result<u32, WireError> {
        Ok(u32::from_be_bytes(*self.fixed()?))
    }

    /// A `uint64`.
    ///
    /// # Errors
    ///
    /// [`WireError::Truncated`] when fewer than eight bytes remain.
    pub fn uint64(&mut self) -> Result<u64, WireError> {
        Ok(u64::from_be_bytes(*self.fixed()?))
    }

    /// A `string`: arbitrary bytes behind a `uint32` length.
    ///
    /// # Errors
    ///
    /// [`WireError::Truncated`] when the declared length runs past the input.
    pub fn string(&mut self) -> Result<&'a [u8], WireError> {
        let len = self.uint32()?;
        self.take(usize::try_from(len).map_err(|_| WireError::Truncated)?)
    }

    /// A `string` that must be UTF-8.
    ///
    /// # Errors
    ///
    /// As [`Self::string`], or [`WireError::InvalidUtf8`].
    pub fn utf8(&mut self) -> Result<&'a str, WireError> {
        core::str::from_utf8(self.string()?).map_err(|_| WireError::InvalidUtf8)
    }

    /// An `mpint`.
    ///
    /// # Errors
    ///
    /// As [`Self::string`], or [`WireError::NonCanonicalMpint`].
    pub fn mpint(&mut self) -> Result<Mpint<'a>, WireError> {
        Mpint::from_twos_complement(self.string()?)
    }

    /// A `name-list`.
    ///
    /// # Errors
    ///
    /// As [`Self::string`], or [`WireError::InvalidName`].
    pub fn name_list(&mut self) -> Result<NameList<'a>, WireError> {
        NameList::parse(self.string()?)
    }

    /// Require that nothing follows the last field.
    ///
    /// # Errors
    ///
    /// [`WireError::TrailingData`] when bytes remain.
    pub fn finish(self) -> Result<(), WireError> {
        if self.rest.is_empty() {
            Ok(())
        } else {
            Err(WireError::TrailingData)
        }
    }
}

/// Appends RFC 4251 values to a buffer.
///
/// A failed write latches: every later write is ignored and [`Self::finish`]
/// reports the first failure, so a message is built with plain calls and
/// checked once. Whatever was appended before the failure stays in the
/// buffer; a holder that must not keep half a message truncates it.
#[derive(Debug)]
pub struct Writer<'a> {
    out: &'a mut Vec<u8>,
    failed: Option<WireError>,
}

impl<'a> Writer<'a> {
    /// A writer appending to `out`.
    pub fn new(out: &'a mut Vec<u8>) -> Self {
        Self { out, failed: None }
    }

    fn put(&mut self, bytes: &[u8]) {
        if self.failed.is_some() {
            return;
        }
        if self.out.try_reserve(bytes.len()).is_err() {
            self.failed = Some(WireError::OutOfMemory);
            return;
        }
        self.out.extend_from_slice(bytes);
    }

    fn length(&mut self, len: usize) {
        match u32::try_from(len) {
            Ok(len) => self.uint32(len),
            Err(_) => {
                if self.failed.is_none() {
                    self.failed = Some(WireError::TooLong);
                }
            }
        }
    }

    /// A `byte`.
    pub fn byte(&mut self, value: u8) {
        self.put(&[value]);
    }

    /// A `byte[n]` run, written as it stands with no length.
    pub fn fixed(&mut self, bytes: &[u8]) {
        self.put(bytes);
    }

    /// A `boolean`, always as 0 or 1.
    pub fn boolean(&mut self, value: bool) {
        self.byte(u8::from(value));
    }

    /// A `uint32`.
    pub fn uint32(&mut self, value: u32) {
        self.put(&value.to_be_bytes());
    }

    /// A `uint64`.
    pub fn uint64(&mut self, value: u64) {
        self.put(&value.to_be_bytes());
    }

    /// A `string`.
    pub fn string(&mut self, value: &[u8]) {
        self.length(value.len());
        self.put(value);
    }

    /// An `mpint` already in canonical form.
    pub fn mpint(&mut self, value: Mpint<'_>) {
        self.string(value.as_twos_complement());
    }

    /// The `mpint` of the non-negative integer whose big-endian magnitude is
    /// `magnitude`: leading zeros dropped, and one zero byte added where the
    /// top bit would otherwise read as a sign.
    pub fn mpint_unsigned(&mut self, magnitude: &[u8]) {
        let start = magnitude
            .iter()
            .position(|&byte| byte != 0)
            .unwrap_or(magnitude.len());
        let digits = &magnitude[start..];
        let sign_pad = digits.first().is_some_and(|&top| top & 0x80 != 0);
        self.length(digits.len() + usize::from(sign_pad));
        if sign_pad {
            self.byte(0);
        }
        self.put(digits);
    }

    /// A `name-list`.
    pub fn name_list(&mut self, list: NameList<'_>) {
        self.string(list.as_str().as_bytes());
    }

    /// Report the first failure, if any write failed.
    ///
    /// # Errors
    ///
    /// [`WireError::TooLong`] or [`WireError::OutOfMemory`].
    pub fn finish(self) -> Result<(), WireError> {
        self.failed.map_or(Ok(()), Err)
    }
}

/// A multiple-precision integer in RFC 4251's canonical two's-complement
/// form: the data bytes of the `string`, most significant first, with no
/// leading byte its sign does not need and zero as the empty string.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct Mpint<'a>(&'a [u8]);

impl<'a> Mpint<'a> {
    /// Zero.
    pub const ZERO: Mpint<'static> = Mpint(&[]);

    /// Accept `bytes` as an `mpint`'s data if they are canonical.
    ///
    /// # Errors
    ///
    /// [`WireError::NonCanonicalMpint`] for a leading `0x00` that does not
    /// protect a set top bit, a leading `0xFF` that does not protect a clear
    /// one, or zero spelled as anything but the empty string.
    pub fn from_twos_complement(bytes: &'a [u8]) -> Result<Self, WireError> {
        match bytes {
            [0x00] => Err(WireError::NonCanonicalMpint),
            [0x00, next, ..] if next & 0x80 == 0 => Err(WireError::NonCanonicalMpint),
            [0xFF, next, ..] if next & 0x80 != 0 => Err(WireError::NonCanonicalMpint),
            _ => Ok(Self(bytes)),
        }
    }

    /// The data bytes, as they appear on the wire.
    #[must_use]
    pub const fn as_twos_complement(self) -> &'a [u8] {
        self.0
    }

    /// Whether the value is zero.
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.0.is_empty()
    }

    /// Whether the value is below zero.
    #[must_use]
    pub fn is_negative(self) -> bool {
        self.0.first().is_some_and(|&top| top & 0x80 != 0)
    }

    /// The magnitude of a non-negative value, big-endian with no leading
    /// zero — the form a key-agreement or signature primitive takes — or
    /// `None` for a negative one.
    #[must_use]
    pub fn magnitude(self) -> Option<&'a [u8]> {
        if self.is_negative() {
            return None;
        }
        // Canonical form keeps a leading zero only to shield a set top bit.
        Some(self.0.strip_prefix(&[0]).unwrap_or(self.0))
    }
}

/// A comma-separated `name-list` whose every name RFC 4250 §4.6.1 permits:
/// 1 to [`MAX_NAME_LEN`] printable US-ASCII bytes with no comma, and at most
/// one `@`, which neither opens nor closes the name.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct NameList<'a>(&'a str);

impl<'a> NameList<'a> {
    /// The list with no names.
    pub const EMPTY: NameList<'static> = NameList("");

    /// Accept `text` as a name-list. `const`, so a list an engine offers is
    /// checked where it is written.
    ///
    /// # Errors
    ///
    /// [`WireError::InvalidName`] for any name RFC 4250 does not allow.
    pub const fn new(text: &'a str) -> Result<Self, WireError> {
        match validate(text.as_bytes()) {
            Ok(()) => Ok(Self(text)),
            Err(err) => Err(err),
        }
    }

    /// Accept received `bytes` as a name-list.
    ///
    /// # Errors
    ///
    /// [`WireError::InvalidName`] for any name RFC 4250 does not allow.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, WireError> {
        validate(bytes)?;
        // Printable US-ASCII is UTF-8, so this cannot fail once validated.
        core::str::from_utf8(bytes)
            .map(Self)
            .map_err(|_| WireError::InvalidName)
    }

    /// The list as it appears on the wire.
    #[must_use]
    pub const fn as_str(self) -> &'a str {
        self.0
    }

    /// Whether the list holds no names.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0.is_empty()
    }

    /// The names, in the order the list gives them.
    pub fn iter(self) -> impl Iterator<Item = &'a str> {
        self.0.split(',').filter(|name| !name.is_empty())
    }

    /// Whether `name` is one of the names.
    #[must_use]
    pub fn contains(self, name: &str) -> bool {
        self.iter().any(|listed| listed == name)
    }
}

/// Whether `bytes` spell a name-list whose every name RFC 4250 permits.
const fn validate(bytes: &[u8]) -> Result<(), WireError> {
    if bytes.is_empty() {
        return Ok(());
    }
    let mut start = 0;
    let mut ats = 0;
    let mut at = 0;
    while at <= bytes.len() {
        if at == bytes.len() || bytes[at] == b',' {
            let len = at - start;
            if len == 0
                || len > MAX_NAME_LEN
                || ats > 1
                || bytes[start] == b'@'
                || bytes[at - 1] == b'@'
            {
                return Err(WireError::InvalidName);
            }
            start = at + 1;
            ats = 0;
        } else {
            let byte = bytes[at];
            if byte <= b' ' || byte > b'~' {
                return Err(WireError::InvalidName);
            }
            if byte == b'@' {
                ats += 1;
            }
        }
        at += 1;
    }
    Ok(())
}

#[cfg(test)]
#[path = "wire_tests.rs"]
mod tests;
