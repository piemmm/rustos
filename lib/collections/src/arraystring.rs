//! A UTF-8 string whose capacity is a compile-time bound the caller chooses.

use core::cmp::Ordering;
use core::fmt;
use core::hash::{Hash, Hasher};
use core::ops::Deref;

use crate::CapacityError;

/// The longest prefix of `s` that fits `max` bytes and ends on a character
/// boundary.
fn boundary_prefix(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    // Zero is always a boundary, so `end` names one and the slice holds.
    &s[..end]
}

/// A string of at most `N` bytes held inline, allocating nothing.
///
/// What an ad-hoc `[u8; N]` paired with a length field was standing in for,
/// with the UTF-8 invariant enforced by construction instead of assumed: the
/// bytes below the length are always valid UTF-8, so [`as_str`](Self::as_str)
/// cannot fail and no stored value can make a read panic.
///
/// [`Copy`], unlike [`ArrayVec`](crate::ArrayVec), because there is nothing to
/// drop — which is what lets a record holding one be lifted out from under a
/// lock and rendered afterwards.
///
/// Two pushes are offered and the choice is the caller's: `try_push_str`
/// refuses a string that does not fit, and `push_str_truncating` stores the
/// longest character-boundary prefix that does. A store that must never lose
/// text takes the first; a bounded one-line diagnostic takes the second.
#[derive(Copy, Clone)]
pub struct ArrayString<const N: usize> {
    /// `bytes[..len]` is valid UTF-8; the rest is unread residue.
    bytes: [u8; N],
    len: usize,
}

impl<const N: usize> ArrayString<N> {
    /// An empty string. `const`, so one can back a `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            bytes: [0; N],
            len: 0,
        }
    }

    /// The longest character-boundary prefix of `s` that fits.
    #[must_use]
    pub fn from_str_truncating(s: &str) -> Self {
        let mut out = Self::new();
        out.push_str_truncating(s);
        out
    }

    /// The fixed capacity in bytes, `N`.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        N
    }

    /// Stored length in bytes.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether nothing is stored.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Bytes that can still be stored.
    #[must_use]
    pub const fn remaining_capacity(&self) -> usize {
        N - self.len
    }

    /// The stored text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        // Every mutation stores a whole `&str`, so the prefix is valid UTF-8;
        // the empty fallback keeps a read total rather than asserting it.
        core::str::from_utf8(&self.bytes[..self.len]).unwrap_or("")
    }

    /// The stored text as bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8] {
        self.bytes.split_at(self.len).0
    }

    /// Append the whole of `s`.
    ///
    /// # Errors
    ///
    /// [`CapacityError`] when `s` does not fit; nothing is stored.
    pub fn try_push_str(&mut self, s: &str) -> Result<(), CapacityError> {
        if s.len() > self.remaining_capacity() {
            return Err(CapacityError::new(()));
        }
        self.bytes[self.len..self.len + s.len()].copy_from_slice(s.as_bytes());
        self.len += s.len();
        Ok(())
    }

    /// Append the longest character-boundary prefix of `s` that fits,
    /// returning the remainder that did not.
    ///
    /// A partial character is never stored: the split lands on a boundary, so
    /// the invariant holds however the text was cut.
    pub fn push_str_truncating<'a>(&mut self, s: &'a str) -> &'a str {
        let taken = boundary_prefix(s, self.remaining_capacity());
        self.bytes[self.len..self.len + taken.len()].copy_from_slice(taken.as_bytes());
        self.len += taken.len();
        &s[taken.len()..]
    }

    /// Append one character.
    ///
    /// # Errors
    ///
    /// [`CapacityError`] when its UTF-8 encoding does not fit.
    pub fn try_push(&mut self, c: char) -> Result<(), CapacityError> {
        let mut encoded = [0u8; 4];
        self.try_push_str(c.encode_utf8(&mut encoded))
    }

    /// Discard the stored text.
    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// Shorten to at most `len` bytes, cutting back to a character boundary so
    /// no partial character survives.
    pub fn truncate(&mut self, len: usize) {
        if len >= self.len {
            return;
        }
        self.len = boundary_prefix(self.as_str(), len).len();
    }
}

impl<const N: usize> Default for ArrayString<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Deref for ArrayString<N> {
    type Target = str;

    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl<const N: usize> fmt::Debug for ArrayString<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl<const N: usize> fmt::Display for ArrayString<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A write sink, so `write!` can build a bounded string. Text past the
/// capacity is refused whole rather than half-stored.
impl<const N: usize> fmt::Write for ArrayString<N> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.try_push_str(s).map_err(|_| fmt::Error)
    }
}

impl<const N: usize> PartialEq for ArrayString<N> {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl<const N: usize> Eq for ArrayString<N> {}

impl<const N: usize> PartialEq<str> for ArrayString<N> {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl<const N: usize> PartialOrd for ArrayString<N> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<const N: usize> Ord for ArrayString<N> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl<const N: usize> Hash for ArrayString<N> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl<const N: usize> TryFrom<&str> for ArrayString<N> {
    type Error = CapacityError;

    fn try_from(s: &str) -> Result<Self, CapacityError> {
        let mut out = Self::new();
        out.try_push_str(s)?;
        Ok(out)
    }
}

#[cfg(test)]
#[path = "arraystring_tests.rs"]
mod tests;
