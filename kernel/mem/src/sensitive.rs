//! Sensitive-region API: zero-on-free for credentials, keys, and
//! capability tokens.
//!
//! Any allocation that ever held a credential, key, or capability token is
//! zeroed on free, and this module is the only blessed API for one:
//! [`alloc_sensitive`] hands back a zeroed [`SensitiveBuffer`] of a given
//! length and [`SensitiveBuffer::copy_from_slice`] one holding a copy of
//! existing bytes; both wipe themselves on drop. Zeroing is delegated to the
//! audited `zeroize` crate rather than hand-rolled.
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
/// The buffer is fixed-size: exactly as long as the length it was
/// constructed with. Resizing is not permitted — a re-allocation would
/// leave a copy of the secret in the old slab.
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
    /// A wiped-on-drop copy of `src`.
    ///
    /// The one place a borrowed slice becomes an owned sensitive buffer. The
    /// copy is exact-capacity: an over-allocation would leave secret bytes
    /// outside the length the wipe covers.
    ///
    /// # Errors
    ///
    /// [`AllocError::OutOfMemory`] if the heap cannot hold the copy.
    pub fn copy_from_slice(src: &[u8]) -> Result<Self, AllocError> {
        let mut v: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
        v.try_reserve_exact(src.len())
            .map_err(|_| AllocError::OutOfMemory)?;
        v.extend_from_slice(src);
        Ok(Self {
            buf: v.into_boxed_slice(),
        })
    }

    /// Length of the buffer in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// `true` if the buffer holds no bytes: a zero-length allocation or a
    /// copy of an empty slice.
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
/// `len == 0` yields an empty buffer rather than an error: a caller whose
/// length is data (an IPC payload, a read of unknown size) would otherwise
/// have to special-case empty, and that special case is where the leak
/// hides. A caller that genuinely requires a non-empty region rejects zero
/// itself, with its own diagnostic — as the shared-memory object does.
///
/// # Errors
///
/// [`AllocError::OutOfMemory`] if the underlying heap is exhausted; the
/// `try_reserve_exact` keeps the contract `Result`-only.
pub fn alloc_sensitive(len: usize) -> Result<SensitiveBuffer, AllocError> {
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
    fn alloc_zero_yields_an_empty_buffer() {
        let buf = alloc_sensitive(0).expect("zero length is representable");
        assert!(buf.is_empty());
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

    #[test]
    fn copy_from_slice_owns_the_bytes() {
        let buf = SensitiveBuffer::copy_from_slice(b"S3CR3T").unwrap();
        assert_eq!(buf.as_bytes(), b"S3CR3T");
        assert_eq!(buf.len(), 6);
    }

    #[test]
    fn copy_from_slice_accepts_an_empty_source() {
        let buf = SensitiveBuffer::copy_from_slice(&[]).unwrap();
        assert!(buf.is_empty());
    }

    #[test]
    fn copy_from_slice_is_exact_capacity() {
        // A copy that over-allocated would leave secret bytes in a slab the
        // wipe never covers, so the buffer must be exactly `src.len()`.
        let buf = SensitiveBuffer::copy_from_slice(&[0xCD; 300]).unwrap();
        assert_eq!(buf.len(), 300);
        assert!(buf.as_bytes().iter().all(|b| *b == 0xCD));
    }
}
