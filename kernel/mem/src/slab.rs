//! Kernel slab allocator with guard pages on both sides.
//!
//! `AGENTS.md` §4 mandates *"Guard pages around kernel slabs."* In a
//! real RustOS deployment the guard pages are unmapped virtual pages
//! immediately above and below each slab; a buffer-overflow write past
//! the slab faults loudly instead of silently corrupting the next slab.
//!
//! On a developer workstation we obviously cannot rely on an MMU. The
//! host-testable implementation here emulates guard pages with a
//! known-byte pattern (`0xCC`, x86's `int3`) placed at both ends of the
//! slab's backing buffer. The allocator validates that pattern on every
//! deallocation (and on demand via [`Slab::check_guards`]).
//!
//! Either way, a [`SlabError::GuardViolation`] is the *only* possible
//! outcome of an overrun: the page-fault on real hardware and the
//! pattern-mismatch on a host both surface through the same error
//! channel, so callers can be written once and tested everywhere.

use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use crate::error::AllocError;
use crate::frame::PAGE_SIZE;
#[cfg(test)]
use crate::ptr::offset_within;
use crate::ptr::slice_within;

/// Byte pattern used to fill the guard pages on the host.
///
/// `0xCC` is x86's `int3` (breakpoint). Picking an "obviously wrong"
/// byte that is unlikely to be a valid value in kernel object types
/// makes accidental matches improbable. Real hardware never touches
/// this byte — the guard pages there are simply unmapped.
const GUARD_BYTE: u8 = 0xCC;

/// Width of the guard region in bytes.
///
/// Picked equal to one page so the host model matches the on-hardware
/// model exactly (the on-hardware guard *is* one unmapped page on each
/// side).
const GUARD_BYTES: usize = PAGE_SIZE;

/// Slab-allocator errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SlabError {
    /// Allocation failed (no free slot or zero-sized request).
    Alloc(AllocError),
    /// The handle passed to `free` did not belong to this slab.
    UnknownHandle,
    /// The handle was already freed.
    DoubleFree,
    /// One or both guard regions were modified — a slab over-run was
    /// detected. Real hardware would have faulted; the host check
    /// surfaces it through this variant.
    GuardViolation,
}

impl From<AllocError> for SlabError {
    fn from(e: AllocError) -> Self {
        Self::Alloc(e)
    }
}

impl fmt::Display for SlabError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Alloc(e) => write!(f, "slab alloc: {e}"),
            Self::UnknownHandle => f.write_str("slab handle not from this slab"),
            Self::DoubleFree => f.write_str("slab double free"),
            Self::GuardViolation => f.write_str("slab guard-page violation"),
        }
    }
}

/// Opaque handle returned by [`Slab::alloc`].
///
/// Internally an index into the slab's slot table. We hand out indices
/// instead of raw pointers so the host test double can revoke them on
/// drop and detect double-frees without unsafe gymnastics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SlabHandle(usize);

/// Fixed-size object slab.
///
/// One slab manages objects of a single `object_size` and yields up to
/// `slot_count` of them. Slabs of *different* sizes are independent
/// instances of this type.
pub struct Slab {
    object_size: usize,
    slot_count: usize,
    /// Backing store laid out as:
    ///   `[ GUARD_BYTES | object_size * slot_count | GUARD_BYTES ]`
    storage: Vec<u8>,
    /// Per-slot allocation state.
    in_use: Vec<bool>,
}

impl Slab {
    /// Construct a new slab.
    ///
    /// # Errors
    ///
    /// - [`AllocError::ZeroSize`] if `object_size == 0` or `slot_count == 0`.
    /// - [`AllocError::SizeUnsupported`] if the storage would overflow
    ///   `usize` (i.e. `object_size * slot_count + 2 * GUARD_BYTES`).
    pub fn new(object_size: usize, slot_count: usize) -> Result<Self, AllocError> {
        if object_size == 0 || slot_count == 0 {
            return Err(AllocError::ZeroSize);
        }
        let data_bytes = object_size
            .checked_mul(slot_count)
            .ok_or(AllocError::SizeUnsupported)?;
        let total = data_bytes
            .checked_add(GUARD_BYTES)
            .and_then(|x| x.checked_add(GUARD_BYTES))
            .ok_or(AllocError::SizeUnsupported)?;

        let mut storage = vec![0u8; total];
        // Paint the guard regions.
        for b in &mut storage[..GUARD_BYTES] {
            *b = GUARD_BYTE;
        }
        for b in &mut storage[GUARD_BYTES + data_bytes..] {
            *b = GUARD_BYTE;
        }

        Ok(Self {
            object_size,
            slot_count,
            storage,
            in_use: vec![false; slot_count],
        })
    }

    /// Reserve one slot and return its handle.
    ///
    /// # Errors
    ///
    /// [`SlabError::Alloc`]`(`[`AllocError::OutOfMemory`]`)` when every
    /// slot is taken.
    pub fn alloc(&mut self) -> Result<SlabHandle, SlabError> {
        // Pre-flight: a slab is never expected to grow without a
        // detected over-run. If guards have been clobbered the alloc
        // must fail closed (`AGENTS.md` §5.4).
        self.verify_guards_internal()?;
        for (i, used) in self.in_use.iter_mut().enumerate() {
            if !*used {
                *used = true;
                return Ok(SlabHandle(i));
            }
        }
        Err(SlabError::Alloc(AllocError::OutOfMemory))
    }

    /// Return the slot to the free pool.
    ///
    /// # Errors
    ///
    /// - [`SlabError::UnknownHandle`] if the handle is out of range.
    /// - [`SlabError::DoubleFree`] if the slot is not currently in use.
    /// - [`SlabError::GuardViolation`] if either guard region was
    ///   tampered with while the slot was live.
    pub fn free(&mut self, h: SlabHandle) -> Result<(), SlabError> {
        if h.0 >= self.slot_count {
            return Err(SlabError::UnknownHandle);
        }
        if !self.in_use[h.0] {
            return Err(SlabError::DoubleFree);
        }
        self.verify_guards_internal()?;
        // Zero the slot's bytes — keeps the slab clean between uses
        // and means freed slots don't leak their previous contents to
        // the *next* allocator caller (cheap defence-in-depth, not
        // the same thing as `sensitive` zero-on-free).
        let off = GUARD_BYTES + h.0 * self.object_size;
        let base = self.storage.as_mut_ptr();
        let total = self.storage.len();
        // SAFETY: `base..base+total` is the live `storage` Vec; we hold
        // exclusive `&mut self` to it for the duration of this call and
        // `slice_within` keeps the access inside the data region.
        // `ptr.rs` is the only module allowed to call raw `*::add`.
        let slot = unsafe { slice_within(base, total, off, self.object_size) }
            .ok_or(SlabError::Alloc(AllocError::OutOfRange))?;
        for b in slot.iter_mut() {
            *b = 0;
        }
        self.in_use[h.0] = false;
        Ok(())
    }

    /// Borrow the byte slice backing a live slot.
    ///
    /// # Errors
    ///
    /// - [`SlabError::UnknownHandle`] / [`SlabError::DoubleFree`] for
    ///   stale handles.
    pub fn slot_mut(&mut self, h: SlabHandle) -> Result<&mut [u8], SlabError> {
        if h.0 >= self.slot_count {
            return Err(SlabError::UnknownHandle);
        }
        if !self.in_use[h.0] {
            return Err(SlabError::DoubleFree);
        }
        let off = GUARD_BYTES + h.0 * self.object_size;
        let base = self.storage.as_mut_ptr();
        let total = self.storage.len();
        // SAFETY: as in `free` — the slice stays inside `storage`, no
        // alias is created (we take `&mut self`), and `slice_within`
        // bounds-checks the request.
        let s = unsafe { slice_within(base, total, off, self.object_size) }
            .ok_or(SlabError::Alloc(AllocError::OutOfRange))?;
        Ok(s)
    }

    /// Explicit guard-page check.
    ///
    /// Equivalent to the implicit check performed by every alloc/free,
    /// but exposed for callers that want to verify the slab between
    /// operations.
    ///
    /// # Errors
    ///
    /// [`SlabError::GuardViolation`] if either guard region has been
    /// modified.
    pub fn check_guards(&self) -> Result<(), SlabError> {
        self.verify_guards_internal()
    }

    /// Number of currently-live slots.
    #[must_use]
    pub fn live(&self) -> usize {
        self.in_use.iter().filter(|x| **x).count()
    }

    /// Capacity (total slots).
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.slot_count
    }

    /// Bytes per object.
    #[must_use]
    pub fn object_size(&self) -> usize {
        self.object_size
    }

    /// Test-only access to byte `offset` in the storage backing.
    ///
    /// This is the trapdoor the unit tests use to *simulate* an
    /// overrun (writing past the end of a slot into the trailing
    /// guard). Production code must not call it; production callers
    /// reach object bytes through [`Slab::slot_mut`].
    ///
    /// # Safety
    ///
    /// The caller must not cause a data race; `&mut self` guarantees
    /// exclusive access. The byte at `offset` is reachable iff
    /// `offset < storage.len()`.
    #[cfg(test)]
    pub(crate) unsafe fn poke_for_test(&mut self, offset: usize, byte: u8) -> Option<()> {
        let base = self.storage.as_mut_ptr();
        let total = self.storage.len();
        // SAFETY: invariant established by `offset_within`: the
        // returned pointer is inside `storage`'s allocation. `&mut self`
        // gives us exclusive access; we write exactly one byte.
        let p = offset_within(base, total, offset)?;
        unsafe { core::ptr::write(p, byte) };
        Some(())
    }

    fn verify_guards_internal(&self) -> Result<(), SlabError> {
        let data_bytes = self.object_size * self.slot_count;
        let head = &self.storage[..GUARD_BYTES];
        let tail = &self.storage[GUARD_BYTES + data_bytes..];
        if head.iter().any(|&b| b != GUARD_BYTE) || tail.iter().any(|&b| b != GUARD_BYTE) {
            return Err(SlabError::GuardViolation);
        }
        Ok(())
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn new_rejects_zero_size() {
        assert_eq!(Slab::new(0, 4).err(), Some(AllocError::ZeroSize));
        assert_eq!(Slab::new(64, 0).err(), Some(AllocError::ZeroSize));
    }

    #[test]
    fn new_rejects_size_overflow() {
        assert_eq!(
            Slab::new(usize::MAX, 2).err(),
            Some(AllocError::SizeUnsupported)
        );
    }

    #[test]
    fn alloc_and_free_round_trip() {
        let mut s = Slab::new(32, 4).unwrap();
        let h = s.alloc().unwrap();
        assert_eq!(s.live(), 1);
        s.free(h).unwrap();
        assert_eq!(s.live(), 0);
    }

    #[test]
    fn exhausts_and_then_oom() {
        let mut s = Slab::new(16, 3).unwrap();
        let _a = s.alloc().unwrap();
        let _b = s.alloc().unwrap();
        let _c = s.alloc().unwrap();
        assert!(matches!(
            s.alloc(),
            Err(SlabError::Alloc(AllocError::OutOfMemory))
        ));
    }

    #[test]
    fn double_free_detected() {
        let mut s = Slab::new(16, 2).unwrap();
        let h = s.alloc().unwrap();
        s.free(h).unwrap();
        assert!(matches!(s.free(h), Err(SlabError::DoubleFree)));
    }

    #[test]
    fn unknown_handle_rejected() {
        let mut s = Slab::new(16, 2).unwrap();
        assert!(matches!(
            s.free(SlabHandle(99)),
            Err(SlabError::UnknownHandle)
        ));
    }

    #[test]
    fn slot_mut_writes_dont_clobber_guards() {
        let mut s = Slab::new(16, 2).unwrap();
        let h = s.alloc().unwrap();
        let slot = s.slot_mut(h).unwrap();
        slot.copy_from_slice(&[7u8; 16]);
        s.check_guards().unwrap();
        s.free(h).unwrap();
    }

    #[test]
    fn slot_zeroed_on_free() {
        let mut s = Slab::new(16, 1).unwrap();
        let h = s.alloc().unwrap();
        s.slot_mut(h).unwrap().copy_from_slice(&[0xAB; 16]);
        s.free(h).unwrap();
        // Re-alloc the same slot and confirm it reads as zeroes.
        let h2 = s.alloc().unwrap();
        assert!(s.slot_mut(h2).unwrap().iter().all(|b| *b == 0));
    }

    #[test]
    fn guard_violation_detected_at_check() {
        let mut s = Slab::new(16, 2).unwrap();
        let _h = s.alloc().unwrap();
        // Simulate an overrun by writing into the trailing guard.
        let overrun_offset = GUARD_BYTES + 16 * 2; // first byte of trailing guard
                                                   // SAFETY: test-only trapdoor; we modify a byte we own and
                                                   // immediately observe the resulting GuardViolation. No alias.
        unsafe { s.poke_for_test(overrun_offset, 0).unwrap() };
        assert!(matches!(s.check_guards(), Err(SlabError::GuardViolation)));
    }

    #[test]
    fn guard_violation_detected_at_alloc() {
        let mut s = Slab::new(16, 4).unwrap();
        // Clobber the leading guard.
        // SAFETY: as above.
        unsafe { s.poke_for_test(0, 0).unwrap() };
        assert!(matches!(s.alloc(), Err(SlabError::GuardViolation)));
    }

    #[test]
    fn guard_violation_detected_at_free() {
        let mut s = Slab::new(16, 4).unwrap();
        let h = s.alloc().unwrap();
        // Clobber leading guard while slot is live.
        // SAFETY: test-only trapdoor; we own the buffer.
        unsafe { s.poke_for_test(0, 0).unwrap() };
        assert!(matches!(s.free(h), Err(SlabError::GuardViolation)));
    }

    #[test]
    fn capacity_and_object_size_reported() {
        let s = Slab::new(64, 5).unwrap();
        assert_eq!(s.capacity(), 5);
        assert_eq!(s.object_size(), 64);
    }

    #[test]
    fn display_messages_present() {
        extern crate std;
        use std::format;
        assert!(format!("{}", SlabError::DoubleFree).contains("double"));
        assert!(format!("{}", SlabError::UnknownHandle).contains("handle"));
        assert!(format!("{}", SlabError::GuardViolation).contains("guard"));
        assert!(format!("{}", SlabError::Alloc(AllocError::OutOfMemory)).contains("memory"));
    }
}
