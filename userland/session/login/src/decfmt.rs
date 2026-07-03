//! Fixed-capacity decimal formatting for audit fields.
//!
//! Login renders numeric audit-record fields (uids, exit codes, attempt
//! counts) without an allocator; [`DecBuf`] is the one formatter the
//! [`Login`](crate::Login) state machine and the elevation broker
//! ([`crate::elevate`]) share, so neither carries a private copy. It
//! mirrors the helper in `init`.

use core::fmt::{self, Write as _};

/// Fixed-capacity decimal formatter for an `i128`.
///
/// 40 bytes hold the widest `i128` (39 digits plus a sign), so
/// [`format`](Self::format) can never overflow its buffer.
pub(crate) struct DecBuf {
    bytes: [u8; Self::CAP],
    len: usize,
}

impl DecBuf {
    const CAP: usize = 40;

    /// An empty formatter.
    pub(crate) fn new() -> Self {
        Self {
            bytes: [0; Self::CAP],
            len: 0,
        }
    }

    /// Render `value` in decimal, returning the borrowed text.
    pub(crate) fn format(&mut self, value: i128) -> &str {
        self.len = 0;
        let _ = write!(DecWriter(self), "{value}");
        core::str::from_utf8(&self.bytes[..self.len]).unwrap_or("?")
    }
}

struct DecWriter<'a>(&'a mut DecBuf);

impl fmt::Write for DecWriter<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let bytes = s.as_bytes();
        let end = self.0.len.checked_add(bytes.len()).ok_or(fmt::Error)?;
        if end > DecBuf::CAP {
            return Err(fmt::Error);
        }
        self.0.bytes[self.0.len..end].copy_from_slice(bytes);
        self.0.len = end;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::DecBuf;

    #[test]
    fn formats_extremes_and_zero() {
        let mut buf = DecBuf::new();
        assert_eq!(buf.format(0), "0");
        assert_eq!(buf.format(-1), "-1");
        assert_eq!(
            buf.format(i128::MAX),
            "170141183460469231731687303715884105727"
        );
        assert_eq!(
            buf.format(i128::MIN),
            "-170141183460469231731687303715884105728"
        );
    }
}
