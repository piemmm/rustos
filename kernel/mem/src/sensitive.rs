//! Sensitive-region API: zero-on-free for credentials, keys, and
//! capability tokens.
//!
//! the charter mandates:
//!
//! > Zero-on-free for any allocation that ever held credentials, keys,
//! > or capability tokens.
//!
//! This module supplies the only blessed API for those allocations:
//! [`alloc_sensitive`] hands back a [`SensitiveBuffer`] that wipes
//! itself on drop. Zeroing is delegated to the audited `zeroize` crate
//! ("audited crypto. No hand-rolled primitives.").
//!
//! Sensitive buffers are deliberately *not* `Clone`: every copy of a
//! secret would need its own zero-on-free dance, and accidentally
//! producing one is the most common bug in this area. If you need a
//! second copy, allocate a second buffer and copy explicitly.

use alloc::boxed::Box;
use core::fmt;
use core::ops::{Deref, DerefMut};

use zeroize::Zeroize;

use crate::error::AllocError;

/// A heap-allocated byte buffer that is zeroed on drop.
///
/// The buffer is fixed-size: it is exactly `len` bytes long, where
/// `len` is the argument to [`alloc_sensitive`]. Resizing is not
/// permitted (that would risk a re-allocation that left a copy of the
/// secret in the old slab).
///
/// # Why `Box<[u8]>` and not `Vec<u8>`?
///
/// A `Vec<u8>` may grow, which would silently leak the old contents.
/// `Box<[u8]>` is a fixed-length owned slice — exactly the right shape
/// for a sensitive region.
pub struct SensitiveBuffer {
    buf: Box<[u8]>,
}

impl SensitiveBuffer {
    /// Length of the buffer in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// `true` if the buffer is zero bytes long. Reserved here because
    /// `len() == 0` cannot actually occur — [`alloc_sensitive`] rejects
    /// zero-sized requests — but the lint exists nonetheless.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Read-only access to the bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    /// Mutable access to the bytes.
    #[must_use]
    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        &mut self.buf
    }
}

impl Deref for SensitiveBuffer {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.buf
    }
}

impl DerefMut for SensitiveBuffer {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.buf
    }
}

impl fmt::Debug for SensitiveBuffer {
    /// Debug-formats the buffer **without** revealing its contents.
    ///
    /// Secrets must never appear in logs, panics, or crash reports.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SensitiveBuffer")
            .field("len", &self.buf.len())
            .field("bytes", &"<redacted>")
            .finish()
    }
}

impl Drop for SensitiveBuffer {
    fn drop(&mut self) {
        // SAFETY-INVARIANT: `Zeroize::zeroize` uses `core::ptr::write_volatile`
        // on every byte, which the compiler is forbidden to elide; this is
        // the entire point of taking a dependency on `zeroize`.
        self.buf.zeroize();
    }
}

/// Allocate a `len`-byte sensitive buffer, initialised to zero.
///
/// # Errors
///
/// - [`AllocError::ZeroSize`] for `len == 0` (see the module note).
/// - [`AllocError::OutOfMemory`] if the underlying heap is exhausted.
///   Detected through `Box::try_new_uninit_slice`-style logic via a
///   `try_reserve` on a `Vec`, then `into_boxed_slice` — keeping the
///   contract `Result`-only.
pub fn alloc_sensitive(len: usize) -> Result<SensitiveBuffer, AllocError> {
    if len == 0 {
        return Err(AllocError::ZeroSize);
    }
    let mut v: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    v.try_reserve_exact(len)
        .map_err(|_| AllocError::OutOfMemory)?;
    v.resize(len, 0);
    Ok(SensitiveBuffer {
        buf: v.into_boxed_slice(),
    })
}

/// Free a sensitive buffer, ensuring its bytes are zeroed first.
///
/// Equivalent to `drop(buf)`, named for symmetry with
/// [`alloc_sensitive`] in places where the lifecycle is documented
/// step by step. The dropped buffer's bytes are zeroed by
/// [`SensitiveBuffer::drop`].
pub fn free_sensitive(buf: SensitiveBuffer) {
    drop(buf);
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    extern crate std;
    use std::format;

    #[test]
    fn alloc_zero_rejected() {
        assert_eq!(alloc_sensitive(0).err(), Some(AllocError::ZeroSize));
    }

    #[test]
    fn alloc_basic_starts_zeroed() {
        let buf = alloc_sensitive(64).unwrap();
        assert_eq!(buf.len(), 64);
        assert!(buf.as_bytes().iter().all(|b| *b == 0));
    }

    #[test]
    fn write_then_read() {
        let mut buf = alloc_sensitive(8).unwrap();
        buf.as_bytes_mut().copy_from_slice(b"deadbeef");
        assert_eq!(buf.as_bytes(), b"deadbeef");
    }

    #[test]
    fn debug_does_not_leak_contents() {
        let mut buf = alloc_sensitive(8).unwrap();
        buf.as_bytes_mut().copy_from_slice(b"S3CR3T!!");
        let s = format!("{buf:?}");
        assert!(s.contains("<redacted>"));
        assert!(!s.contains("S3CR3T"));
    }

    /// Demonstrate that drop zeroes the bytes.
    ///
    /// We cannot inspect freed memory in safe Rust, but we *can* assert
    /// that `zeroize` was called by exercising the `Zeroize` impl
    /// directly on the slice the buffer owns — i.e. observing that the
    /// in-place zeroing is the *same* operation drop performs.
    #[test]
    fn manual_zeroize_clears_payload() {
        let mut buf = alloc_sensitive(16).unwrap();
        buf.as_bytes_mut().copy_from_slice(&[0xAB; 16]);
        // Calling the same trait method drop will call:
        buf.as_bytes_mut().zeroize();
        assert!(buf.as_bytes().iter().all(|b| *b == 0));
    }

    /// Smoke test that `free_sensitive` is a drop-equivalent helper —
    /// drops compile, the value moves, no panic.
    #[test]
    fn free_sensitive_drops() {
        let buf = alloc_sensitive(4).unwrap();
        free_sensitive(buf);
    }

    #[test]
    fn deref_exposes_slice() {
        let mut buf = alloc_sensitive(4).unwrap();
        buf[0] = 1;
        buf[1] = 2;
        let total: u32 = buf.iter().map(|b| u32::from(*b)).sum();
        assert_eq!(total, 3);
    }

    #[test]
    fn is_empty_is_always_false_for_alloced_buffers() {
        let buf = alloc_sensitive(1).unwrap();
        assert!(!buf.is_empty());
    }
}
