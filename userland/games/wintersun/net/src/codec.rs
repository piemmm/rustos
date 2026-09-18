//! The bounded, total cursor every frame is read and written through, and
//! the fixed-width sequence the repeated parts of a message use.
//!
//! Scalars are little-endian. A read is checked against what remains, a write
//! against what is left of the caller's buffer, and neither ever panics: the
//! decoder's whole contract is that arbitrary bytes yield a typed refusal.
//!
//! [`WireSeq`] is the one definition of "a bounded run of fixed-width items",
//! shared by entity states, departed ids, world edits, and play events rather
//! than written out four times. It holds either the validated bytes it was
//! read from or the producer's own slice, so a realm encodes a snapshot
//! straight out of its own storage and a client iterates one straight out of
//! the receive buffer — neither pays a copy to reach the other's shape.

use core::fmt;
use core::marker::PhantomData;

use crate::error::WireError;

/// A fixed-width item that can appear in a [`WireSeq`].
///
/// Fixed width is what makes a run of items bounded by its count alone: a
/// variant that needs fewer bytes than the widest one is zero-padded, and the
/// padding is checked on decode so there is exactly one encoding of any value.
pub trait WireItem: Copy + Sized {
    /// Encoded length of every item, whatever its variant.
    const WIRE_LEN: usize;

    /// Read one item.
    ///
    /// # Errors
    ///
    /// A typed [`WireError`] for a short read, an unknown discriminant,
    /// non-zero padding, or a field outside its range.
    fn read(r: &mut Reader<'_>) -> Result<Self, WireError>;

    /// Write one item.
    ///
    /// # Errors
    ///
    /// [`WireError::BufferTooSmall`] when the output is full.
    fn write(&self, w: &mut Writer<'_>) -> Result<(), WireError>;
}

/// A bounds-checked cursor over borrowed input.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    input: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    /// Start reading `input` from its first byte.
    #[must_use]
    pub const fn new(input: &'a [u8]) -> Self {
        Self { input, at: 0 }
    }

    /// Bytes not yet consumed.
    #[must_use]
    pub const fn remaining(&self) -> usize {
        self.input.len() - self.at
    }

    /// Refuse the frame unless every byte was consumed.
    ///
    /// Decoders call this last: a frame with bytes left over is a shape no
    /// encoder here produces, so it is refused rather than silently accepted.
    ///
    /// # Errors
    ///
    /// [`WireError::TrailingBytes`] when input remains.
    pub const fn finish(&self) -> Result<(), WireError> {
        if self.at == self.input.len() {
            Ok(())
        } else {
            Err(WireError::TrailingBytes)
        }
    }

    /// Consume `len` raw bytes.
    ///
    /// # Errors
    ///
    /// [`WireError::Truncated`] when fewer than `len` bytes remain.
    pub fn take(&mut self, len: usize) -> Result<&'a [u8], WireError> {
        let end = self.at.checked_add(len).ok_or(WireError::Truncated)?;
        let slice = self.input.get(self.at..end).ok_or(WireError::Truncated)?;
        self.at = end;
        Ok(slice)
    }

    /// Consume `len` bytes and refuse the frame unless every one is zero.
    ///
    /// # Errors
    ///
    /// [`WireError::NonCanonicalPadding`] when any padding byte is set.
    pub fn padding(&mut self, len: usize) -> Result<(), WireError> {
        if self.take(len)?.iter().any(|b| *b != 0) {
            return Err(WireError::NonCanonicalPadding);
        }
        Ok(())
    }

    /// Consume one byte.
    ///
    /// # Errors
    ///
    /// [`WireError::Truncated`] when the input is exhausted.
    pub fn u8(&mut self) -> Result<u8, WireError> {
        Ok(self.take(1)?[0])
    }

    /// Consume a byte that must be `0` or `1`, as a presence flag.
    ///
    /// # Errors
    ///
    /// [`WireError::FieldOutOfRange`] for any other value: an optional field
    /// is present or it is not, and a third encoding of that is a forgery.
    pub fn flag(&mut self) -> Result<bool, WireError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(WireError::FieldOutOfRange),
        }
    }

    /// Consume a little-endian `u16`.
    ///
    /// # Errors
    ///
    /// [`WireError::Truncated`] when fewer than two bytes remain.
    pub fn u16(&mut self) -> Result<u16, WireError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    /// Consume a little-endian `i16`.
    ///
    /// # Errors
    ///
    /// [`WireError::Truncated`] when fewer than two bytes remain.
    pub fn i16(&mut self) -> Result<i16, WireError> {
        Ok(self.u16()?.cast_signed())
    }

    /// Consume a little-endian `u32`.
    ///
    /// # Errors
    ///
    /// [`WireError::Truncated`] when fewer than four bytes remain.
    pub fn u32(&mut self) -> Result<u32, WireError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Consume a little-endian `i32`.
    ///
    /// # Errors
    ///
    /// [`WireError::Truncated`] when fewer than four bytes remain.
    pub fn i32(&mut self) -> Result<i32, WireError> {
        Ok(self.u32()?.cast_signed())
    }

    /// Consume a little-endian `u64`.
    ///
    /// # Errors
    ///
    /// [`WireError::Truncated`] when fewer than eight bytes remain.
    pub fn u64(&mut self) -> Result<u64, WireError> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    /// Consume a fixed-size byte array.
    ///
    /// # Errors
    ///
    /// [`WireError::Truncated`] when fewer than `N` bytes remain.
    pub fn array<const N: usize>(&mut self) -> Result<[u8; N], WireError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    /// Consume a `u8`-length-prefixed text field.
    ///
    /// `min` and `max` bound the byte length; the bytes must be UTF-8 and
    /// free of control characters, because the text is rendered as data by
    /// terminals and consoles that would otherwise act on an escape sequence.
    ///
    /// # Errors
    ///
    /// [`WireError::BoundExceeded`] outside the length bounds,
    /// [`WireError::BadText`] for non-UTF-8 or a control character.
    pub fn text8(&mut self, min: usize, max: usize) -> Result<&'a str, WireError> {
        let len = usize::from(self.u8()?);
        self.text(len, min, max, false)
    }

    /// Consume a `u8`-length-prefixed text field that may be absent, where a
    /// zero length means absent rather than an empty string.
    ///
    /// # Errors
    ///
    /// As [`Self::text8`].
    pub fn optional_text8(&mut self, max: usize) -> Result<Option<&'a str>, WireError> {
        let len = usize::from(self.u8()?);
        if len == 0 {
            return Ok(None);
        }
        self.text(len, 1, max, false).map(Some)
    }

    /// Consume a `u16`-length-prefixed text field, as [`Self::text8`].
    ///
    /// `multiline` admits `\n` and `\t`, which a console reply legitimately
    /// carries; every other control byte stays refused.
    ///
    /// # Errors
    ///
    /// As [`Self::text8`].
    pub fn text16(
        &mut self,
        min: usize,
        max: usize,
        multiline: bool,
    ) -> Result<&'a str, WireError> {
        let len = usize::from(self.u16()?);
        self.text(len, min, max, multiline)
    }

    fn text(
        &mut self,
        len: usize,
        min: usize,
        max: usize,
        multiline: bool,
    ) -> Result<&'a str, WireError> {
        if len < min || len > max {
            return Err(WireError::BoundExceeded);
        }
        let bytes = self.take(len)?;
        let text = core::str::from_utf8(bytes).map_err(|_| WireError::BadText)?;
        check_text(text, multiline)?;
        Ok(text)
    }
}

/// Refuse text carrying a control character.
///
/// `multiline` admits `\n` and `\t` and nothing else. The check is over
/// `char`s, so a control codepoint spelled as multi-byte UTF-8 is caught too.
///
/// # Errors
///
/// [`WireError::BadText`] when a refused character is present.
pub fn check_text(text: &str, multiline: bool) -> Result<(), WireError> {
    let refused = text.chars().any(|c| {
        if multiline && (c == '\n' || c == '\t') {
            return false;
        }
        c.is_control()
    });
    if refused {
        Err(WireError::BadText)
    } else {
        Ok(())
    }
}

/// An append-only cursor over a caller-supplied output buffer.
#[derive(Debug)]
pub struct Writer<'a> {
    out: &'a mut [u8],
    at: usize,
}

impl<'a> Writer<'a> {
    /// Start writing at the front of `out`.
    #[must_use]
    pub fn new(out: &'a mut [u8]) -> Self {
        Self { out, at: 0 }
    }

    /// Bytes written so far.
    #[must_use]
    pub const fn written(&self) -> usize {
        self.at
    }

    /// Append raw bytes.
    ///
    /// # Errors
    ///
    /// [`WireError::BufferTooSmall`] when they do not fit.
    pub fn bytes(&mut self, value: &[u8]) -> Result<(), WireError> {
        let end = self
            .at
            .checked_add(value.len())
            .ok_or(WireError::BufferTooSmall)?;
        let room = self
            .out
            .get_mut(self.at..end)
            .ok_or(WireError::BufferTooSmall)?;
        room.copy_from_slice(value);
        self.at = end;
        Ok(())
    }

    /// Append `len` zero bytes as a fixed-width variant's padding.
    ///
    /// # Errors
    ///
    /// [`WireError::BufferTooSmall`] when they do not fit.
    pub fn padding(&mut self, len: usize) -> Result<(), WireError> {
        let end = self.at.checked_add(len).ok_or(WireError::BufferTooSmall)?;
        let room = self
            .out
            .get_mut(self.at..end)
            .ok_or(WireError::BufferTooSmall)?;
        room.fill(0);
        self.at = end;
        Ok(())
    }

    /// Append one byte.
    ///
    /// # Errors
    ///
    /// [`WireError::BufferTooSmall`] when the buffer is full.
    pub fn u8(&mut self, value: u8) -> Result<(), WireError> {
        self.bytes(&[value])
    }

    /// Append a presence flag.
    ///
    /// # Errors
    ///
    /// [`WireError::BufferTooSmall`] when the buffer is full.
    pub fn flag(&mut self, value: bool) -> Result<(), WireError> {
        self.u8(u8::from(value))
    }

    /// Append a little-endian `u16`.
    ///
    /// # Errors
    ///
    /// [`WireError::BufferTooSmall`] when it does not fit.
    pub fn u16(&mut self, value: u16) -> Result<(), WireError> {
        self.bytes(&value.to_le_bytes())
    }

    /// Append a little-endian `i16`.
    ///
    /// # Errors
    ///
    /// [`WireError::BufferTooSmall`] when it does not fit.
    pub fn i16(&mut self, value: i16) -> Result<(), WireError> {
        self.bytes(&value.to_le_bytes())
    }

    /// Append a little-endian `u32`.
    ///
    /// # Errors
    ///
    /// [`WireError::BufferTooSmall`] when it does not fit.
    pub fn u32(&mut self, value: u32) -> Result<(), WireError> {
        self.bytes(&value.to_le_bytes())
    }

    /// Append a little-endian `i32`.
    ///
    /// # Errors
    ///
    /// [`WireError::BufferTooSmall`] when it does not fit.
    pub fn i32(&mut self, value: i32) -> Result<(), WireError> {
        self.bytes(&value.to_le_bytes())
    }

    /// Append a little-endian `u64`.
    ///
    /// # Errors
    ///
    /// [`WireError::BufferTooSmall`] when it does not fit.
    pub fn u64(&mut self, value: u64) -> Result<(), WireError> {
        self.bytes(&value.to_le_bytes())
    }

    /// Append a `u8`-length-prefixed text field, checking its bounds and
    /// content exactly as the reader does, so an encoder cannot emit a frame
    /// its own decoder would refuse.
    ///
    /// # Errors
    ///
    /// [`WireError::BoundExceeded`] outside the length bounds,
    /// [`WireError::BadText`] for a control character, or
    /// [`WireError::BufferTooSmall`].
    pub fn text8(&mut self, value: &str, min: usize, max: usize) -> Result<(), WireError> {
        let len = value.len();
        if len < min || len > max || len > usize::from(u8::MAX) {
            return Err(WireError::BoundExceeded);
        }
        check_text(value, false)?;
        let Ok(prefix) = u8::try_from(len) else {
            return Err(WireError::BoundExceeded);
        };
        self.u8(prefix)?;
        self.bytes(value.as_bytes())
    }

    /// Append a `u16`-length-prefixed text field, as [`Self::text8`].
    ///
    /// # Errors
    ///
    /// As [`Self::text8`].
    pub fn text16(
        &mut self,
        value: &str,
        min: usize,
        max: usize,
        multiline: bool,
    ) -> Result<(), WireError> {
        let len = value.len();
        if len < min || len > max || len > usize::from(u16::MAX) {
            return Err(WireError::BoundExceeded);
        }
        check_text(value, multiline)?;
        let Ok(prefix) = u16::try_from(len) else {
            return Err(WireError::BoundExceeded);
        };
        self.u16(prefix)?;
        self.bytes(value.as_bytes())
    }
}

/// A bounded run of fixed-width items: the bytes it decoded from, or the
/// producer's own slice.
///
/// Both forms iterate identically, so a realm encodes from its live entity
/// storage and a client reads from its receive buffer with no intermediate
/// copy on either side. Every item is validated when the run is read, so
/// iterating a decoded run cannot fail.
#[derive(Clone, Copy)]
pub struct WireSeq<'a, T: WireItem> {
    repr: Repr<'a, T>,
}

#[derive(Clone, Copy)]
enum Repr<'a, T: WireItem> {
    /// `bytes.len()` is an exact multiple of `T::WIRE_LEN`, and every item in
    /// it decoded when the run was read.
    Encoded(&'a [u8], PhantomData<T>),
    Items(&'a [T]),
}

impl<'a, T: WireItem> WireSeq<'a, T> {
    /// An empty run.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            repr: Repr::Items(&[]),
        }
    }

    /// A run over the producer's own items.
    #[must_use]
    pub const fn from_items(items: &'a [T]) -> Self {
        Self {
            repr: Repr::Items(items),
        }
    }

    /// How many items the run holds.
    #[must_use]
    pub fn len(&self) -> usize {
        match &self.repr {
            Repr::Encoded(bytes, _) => bytes.len() / T::WIRE_LEN,
            Repr::Items(items) => items.len(),
        }
    }

    /// Whether the run is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Item `index`, or `None` when the index is past the end.
    ///
    /// Decoding cannot fail here: every item was validated when the run was
    /// read.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<T> {
        match &self.repr {
            Repr::Encoded(bytes, _) => {
                let start = index.checked_mul(T::WIRE_LEN)?;
                let end = start.checked_add(T::WIRE_LEN)?;
                let chunk = bytes.get(start..end)?;
                T::read(&mut Reader::new(chunk)).ok()
            }
            Repr::Items(items) => items.get(index).copied(),
        }
    }

    /// Iterate the run.
    pub fn iter(&self) -> impl Iterator<Item = T> + '_ {
        (0..self.len()).map_while(move |i| self.get(i))
    }

    /// Read `count` items, validating each.
    ///
    /// # Errors
    ///
    /// [`WireError::Truncated`] when the run is short, or whatever a
    /// malformed item reports.
    pub fn read(r: &mut Reader<'a>, count: usize) -> Result<Self, WireError> {
        let span = count.checked_mul(T::WIRE_LEN).ok_or(WireError::Truncated)?;
        let bytes = r.take(span)?;
        let mut check = Reader::new(bytes);
        for _ in 0..count {
            T::read(&mut check)?;
        }
        Ok(Self {
            repr: Repr::Encoded(bytes, PhantomData),
        })
    }

    /// Write the run, without its count.
    ///
    /// A decoded run is copied through: its encoding is canonical, so
    /// re-encoding item by item would produce the same bytes at more cost.
    ///
    /// # Errors
    ///
    /// [`WireError::BufferTooSmall`] when the output is full.
    pub fn write(&self, w: &mut Writer<'_>) -> Result<(), WireError> {
        match &self.repr {
            Repr::Encoded(bytes, _) => w.bytes(bytes),
            Repr::Items(items) => {
                for item in *items {
                    item.write(w)?;
                }
                Ok(())
            }
        }
    }
}

impl<'a, T: WireItem> From<&'a [T]> for WireSeq<'a, T> {
    fn from(items: &'a [T]) -> Self {
        Self::from_items(items)
    }
}

impl<T: WireItem + PartialEq> PartialEq for WireSeq<'_, T> {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().eq(other.iter())
    }
}

impl<T: WireItem + PartialEq + Eq> Eq for WireSeq<'_, T> {}

impl<T: WireItem + fmt::Debug> fmt::Debug for WireSeq<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

#[cfg(test)]
mod tests;
