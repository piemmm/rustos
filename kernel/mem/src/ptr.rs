//! Bounds-checked pointer arithmetic helpers.
//!
//! the charter forbids "raw pointer arithmetic without bounds-checked
//! wrappers." This module is the single place in `kernel/mem` allowed to
//! call `<*mut _>::add` / `<*const _>::add`. Every other module routes
//! pointer offsets through [`offset_within`] / [`slice_within`].
//!
//! Each helper enforces two invariants:
//!
//! 1. The requested byte offset *and* the requested length fit inside the
//!    `[base, base + region_len)` window.
//! 2. The resulting `usize` arithmetic does not overflow.
//!
//! Either failure yields `None`. Callers that need an
//! [`crate::AllocError`] for their public API translate `None` →
//! [`crate::AllocError::OutOfRange`].

/// Compute `base + offset` as a `*mut u8`, returning `None` if the
/// resulting address would fall outside the `region_len`-byte region
/// starting at `base` or if any intermediate arithmetic would overflow.
///
/// `offset == region_len` is rejected: it is a one-past-the-end pointer
/// and dereferencing it is undefined behaviour. Callers asking for
/// "the end" should request `region_len - 1` and add one explicitly via
/// [`end_within`].
#[must_use]
pub fn offset_within(base: *mut u8, region_len: usize, offset: usize) -> Option<*mut u8> {
    if offset >= region_len {
        return None;
    }
    // `base as usize + offset` cannot overflow because both fit in `usize`
    // *and* `offset < region_len <= isize::MAX as usize` (enforced by
    // every safe allocator that produced `base`). We still check it.
    let addr = (base as usize).checked_add(offset)?;
    // SAFETY: `offset < region_len` is the contract supplied by every
    // caller (the region is owned by the allocator). The checked
    // `as usize` arithmetic above rules out overflow, and the result is
    // therefore inside the `[base, base + region_len)` allocation, which
    // is the only place pointer arithmetic on `base` is defined.
    Some(addr as *mut u8)
}

/// Compute `base + region_len` — the one-past-the-end pointer for an
/// allocation of `region_len` bytes starting at `base`.
///
/// Returns `None` if the address would overflow `usize`. The result is
/// *not* dereferenceable; it is only valid for pointer comparison and
/// for constructing exclusive end markers.
#[must_use]
pub fn end_within(base: *mut u8, region_len: usize) -> Option<*mut u8> {
    let addr = (base as usize).checked_add(region_len)?;
    Some(addr as *mut u8)
}

/// Construct a `&mut [u8]` of `len` bytes starting at `base + offset`,
/// returning `None` if `[offset, offset + len)` is not entirely inside
/// `[0, region_len)`.
///
/// # Safety
///
/// - `base` must point at an allocation of at least `region_len` bytes
///   that the caller currently owns exclusively (a `&mut` re-borrow
///   would otherwise alias).
/// - The bytes in the returned slice must be properly initialised, or
///   the caller must only write to them before reading.
#[must_use]
pub unsafe fn slice_within<'a>(
    base: *mut u8,
    region_len: usize,
    offset: usize,
    len: usize,
) -> Option<&'a mut [u8]> {
    let end = offset.checked_add(len)?;
    if end > region_len {
        return None;
    }
    if len == 0 {
        // An empty slice with a non-null base is well-defined and avoids
        // calling `from_raw_parts_mut` with a possibly-undefined offset.
        // SAFETY: zero-length slice is always valid; the lifetime is the
        // caller's responsibility per the function-level Safety contract.
        return Some(unsafe { core::slice::from_raw_parts_mut(base, 0) });
    }
    let start = offset_within(base, region_len, offset)?;
    // SAFETY: `offset_within` guarantees `start` is in-bounds, and the
    // explicit check above guarantees `start + len` is also in-bounds.
    // The function-level Safety contract guarantees exclusive ownership
    // and initialisation, so the resulting `&mut [u8]` does not alias
    // and points at valid memory for `len` bytes.
    Some(unsafe { core::slice::from_raw_parts_mut(start, len) })
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    extern crate std;
    use std::vec;

    #[test]
    fn offset_within_accepts_in_bounds() {
        let mut buf = vec![0u8; 16];
        let base = buf.as_mut_ptr();
        let p = offset_within(base, 16, 0).unwrap();
        assert_eq!(p, base);
        let p = offset_within(base, 16, 15).unwrap();
        assert_eq!(p as usize - base as usize, 15);
    }

    #[test]
    fn offset_within_rejects_one_past_end() {
        let mut buf = vec![0u8; 16];
        let base = buf.as_mut_ptr();
        assert!(offset_within(base, 16, 16).is_none());
        assert!(offset_within(base, 16, 17).is_none());
    }

    #[test]
    fn offset_within_rejects_overflow() {
        // Use a synthetic `base` near usize::MAX. We never dereference it,
        // we only check that arithmetic refuses to wrap.
        let base = (usize::MAX - 4) as *mut u8;
        assert!(offset_within(base, 16, 8).is_none());
    }

    #[test]
    fn end_within_returns_one_past_end() {
        let mut buf = vec![0u8; 16];
        let base = buf.as_mut_ptr();
        let end = end_within(base, 16).unwrap();
        assert_eq!(end as usize - base as usize, 16);
    }

    #[test]
    fn end_within_rejects_overflow() {
        let base = (usize::MAX - 4) as *mut u8;
        assert!(end_within(base, 8).is_none());
    }

    #[test]
    fn slice_within_accepts_in_bounds_window() {
        let mut buf = vec![0u8; 32];
        let base = buf.as_mut_ptr();
        // SAFETY: `buf` lives long enough; the slice is contained in it
        // and we never alias `buf` while the returned slice is alive.
        let s = unsafe { slice_within(base, 32, 4, 8).unwrap() };
        s.copy_from_slice(&[7u8; 8]);
        // The `&mut [u8]` returned by `slice_within` ends its borrow here.
        let _ = s;
        assert_eq!(&buf[..4], &[0; 4]);
        assert_eq!(&buf[4..12], &[7; 8]);
        assert_eq!(&buf[12..], &[0; 20]);
    }

    #[test]
    fn slice_within_rejects_out_of_bounds() {
        let mut buf = vec![0u8; 16];
        let base = buf.as_mut_ptr();
        // SAFETY: bounds check is the very thing under test; the unsafe
        // call should observe `None` and never touch memory.
        let s = unsafe { slice_within(base, 16, 10, 8) };
        assert!(s.is_none());
    }

    #[test]
    fn slice_within_zero_len_is_ok() {
        let mut buf = vec![0u8; 1];
        let base = buf.as_mut_ptr();
        // SAFETY: zero-length read at any in-bounds offset is safe.
        let s = unsafe { slice_within(base, 1, 0, 0).unwrap() };
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn slice_within_rejects_len_overflow() {
        let mut buf = vec![0u8; 16];
        let base = buf.as_mut_ptr();
        // SAFETY: the function must refuse the request; no memory access.
        let s = unsafe { slice_within(base, 16, usize::MAX, 1) };
        assert!(s.is_none());
    }
}
