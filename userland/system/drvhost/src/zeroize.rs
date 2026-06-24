//! Volatile-clear primitive for sensitive buffers.
//!
//! Per ("Zero-on-free for any allocation that ever held
//! credentials, keys, or capability tokens") the driver host must wipe
//! every buffer that held a manifest signature or a capability bitmap
//! before the allocation is returned. A plain `slice.fill(0)` is **not**
//! sufficient: the compiler is permitted to elide writes to a region the
//! program never reads again, leaving the secret bytes in place. The
//! primitive below uses [`core::ptr::write_volatile`] paired with a
//! sequentially-consistent [`compiler_fence`] so the writes cannot be
//! removed and cannot be re-ordered with subsequent freeing of the
//! containing allocation.
//!
//! This is the **only** `unsafe` block in the `drvhost` crate. Its
//! invariant ("the slice is a valid mutable region for `slice.len()`
//! bytes") is upheld by the [`&mut [u8]`] borrow checker; the unit
//! test below exercises the wipe on a heap allocation that escapes the
//! optimiser via `core::hint::black_box`.

use core::sync::atomic::{compiler_fence, Ordering};

/// Volatile-clear every byte of `slice`.
///
/// On return all bytes of `slice` are observably zero, and the writes
/// will not be removed by the optimiser nor reordered past the
/// containing scope's `Drop`.
///
/// This is a no-op for an empty slice.
///
/// # Capabilities
///
/// None.
pub fn secure_clear(slice: &mut [u8]) {
    for byte in slice.iter_mut() {
        // SAFETY: `byte` is a `&mut u8` produced by the iterator over a
        // `&mut [u8]`. The borrow checker guarantees the pointer is
        // non-null, properly aligned (alignment of `u8` is 1), valid
        // for writes for the duration of this call, and not aliased.
        // `write_volatile` requires exactly those invariants. Writing
        // `0` to a byte is a defined operation for every initialised
        // (and uninitialised) `u8` representation, so no UB can arise
        // from the value itself.
        unsafe {
            core::ptr::write_volatile(byte as *mut u8, 0);
        }
    }
    // Pair the writes with a sequentially-consistent compiler fence so
    // a subsequent `drop(buf)` cannot be reordered above the wipe.
    compiler_fence(Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::secure_clear;
    extern crate alloc;
    use alloc::vec;

    #[test]
    fn clears_every_byte() {
        let mut buf = vec![0xA5u8; 128];
        secure_clear(&mut buf);
        for &b in &buf {
            assert_eq!(b, 0);
        }
    }

    #[test]
    fn empty_slice_is_a_noop() {
        let mut buf: [u8; 0] = [];
        secure_clear(&mut buf);
        assert_eq!(buf.len(), 0);
    }

    #[test]
    fn clears_unaligned_subslice() {
        // Exercise the in-loop volatile write against a subslice whose
        // start address is not aligned to a machine word: this catches
        // a hypothetical future rewrite that swaps the byte loop for
        // an aligned word-store.
        let mut buf = [0xFFu8; 33];
        secure_clear(&mut buf[1..]);
        assert_eq!(buf[0], 0xFF, "byte 0 was outside the clear range");
        for &b in &buf[1..] {
            assert_eq!(b, 0);
        }
    }

    #[test]
    fn writes_survive_optimisation_observer() {
        // Black-box the secret so a sufficiently smart future compiler
        // cannot decide the buffer is dead and skip the wipe.
        let mut buf = vec![0x77u8; 64];
        let observed = core::hint::black_box(&mut buf);
        secure_clear(observed);
        for &b in &buf {
            assert_eq!(b, 0);
        }
    }
}
