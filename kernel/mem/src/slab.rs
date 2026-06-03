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

use rustos_arch_api::{next_free_tag, MemTag, MemoryTagging, TAG_COUNT};

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
    /// The handle's memory tag did not match the slot's current tag — a
    /// use-after-free was detected (`AGENTS.md` §19.10). The slot was
    /// freed and reallocated since this handle was issued, so the handle
    /// is dangling. On a hardware-tagged port (Arm MTE) the access would
    /// have faulted; this is the architecture-neutral software check.
    TagMismatch,
    /// A slot about to be handed out was not clean: it still held
    /// non-zero bytes (`AGENTS.md` §3.3 of the security charter,
    /// CWE-908/CWE-200). [`Slab::free`] wipes every byte of a slot, and a
    /// fresh slab starts zeroed, so a free slot is **always** all-zero.
    /// A non-zero free slot means the zero-on-free invariant was skipped
    /// or corrupted; rather than leak the previous occupant's contents to
    /// the next caller, [`Slab::alloc`] fails closed with this error.
    DirtySlot,
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
            Self::TagMismatch => f.write_str("slab use-after-free: handle tag mismatch"),
            Self::DirtySlot => f.write_str("slab reuse: freed slot was not zeroed"),
        }
    }
}

/// Opaque handle returned by [`Slab::alloc`].
///
/// Pairs an index into the slab's slot table with the *memory tag*
/// (`AGENTS.md` §19.10) the slot carried when the handle was issued. We
/// hand out indices instead of raw pointers so the host test double can
/// revoke them on drop and detect double-frees without unsafe
/// gymnastics; the tag is the software analogue of an Arm-MTE pointer
/// tag, so a handle that outlives its allocation (a use-after-free)
/// mismatches the slot's rotated tag and is rejected — exactly what the
/// hardware would fault on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SlabHandle {
    slot: usize,
    tag: MemTag,
}

impl SlabHandle {
    /// The memory tag this handle carries (the tag the slot held when the
    /// handle was issued).
    #[must_use]
    pub fn tag(self) -> MemTag {
        self.tag
    }
}

/// Whether the slab runs the architecture-neutral *software*
/// use-after-free tag check (`AGENTS.md` §19.10).
///
/// The software check costs a tag rotation on every allocation and a
/// tag comparison on every free and slot access. On a port whose
/// silicon already enforces use-after-free in hardware — Arm MTE with
/// *both* `tag_storage` and `tag_check_faults` supported and enabled
/// ([`TaggingProfile::enforces_uaf_in_hardware`]) — that work is pure
/// duplicated CPU overhead: the hardware tag checker already faults on
/// a dangling access. There the slab stands the software check down.
///
/// Everywhere else — every port that does **not** enforce UAF in
/// hardware, which today is every Tier-1 target — the software check is
/// on. That is the default ([`Slab::new`]); only an explicit
/// hardware-tagging port flips it off via [`Slab::with_tag_check`].
///
/// [`TaggingProfile::enforces_uaf_in_hardware`]:
///     rustos_arch_api::TaggingProfile::enforces_uaf_in_hardware
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SoftwareTagCheck {
    /// Rotate the slot tag on every allocation and reject a handle whose
    /// tag no longer matches its slot. The default, used on every port
    /// that does not enforce UAF in hardware.
    #[default]
    Enabled,
    /// Skip the software tag rotation and comparison entirely: the
    /// port's hardware tag checker already faults on a use-after-free,
    /// so the software check would only duplicate the cost.
    Disabled,
}

impl SoftwareTagCheck {
    /// Choose the software-tag-check policy for a slab on a port that
    /// exposes `tagging`.
    ///
    /// Returns [`SoftwareTagCheck::Disabled`] exactly when the port
    /// enforces use-after-free in hardware
    /// ([`TaggingProfile::enforces_uaf_in_hardware`]), and
    /// [`SoftwareTagCheck::Enabled`] otherwise. This keeps the software
    /// check on by default (`AGENTS.md` §19.10) yet steps aside — for
    /// performance — precisely when redundant hardware tagging is
    /// available and enabled.
    ///
    /// [`TaggingProfile::enforces_uaf_in_hardware`]:
    ///     rustos_arch_api::TaggingProfile::enforces_uaf_in_hardware
    #[must_use]
    pub fn for_tagging(tagging: &dyn MemoryTagging) -> Self {
        if tagging.profile().enforces_uaf_in_hardware() {
            Self::Disabled
        } else {
            Self::Enabled
        }
    }

    /// `true` if the software tag check is active.
    #[must_use]
    pub fn is_enabled(self) -> bool {
        matches!(self, Self::Enabled)
    }
}

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
    /// Per-slot current memory tag (`AGENTS.md` §19.10). Rotated on every
    /// allocation so a reused slot never carries the tag a previously
    /// issued (now dangling) handle still holds.
    tags: Vec<MemTag>,
    /// Whether the software use-after-free tag check runs, or is stood
    /// down because the port enforces UAF in hardware.
    tag_check: SoftwareTagCheck,
}

impl Slab {
    /// Construct a new slab with the software use-after-free tag check
    /// **enabled** (`AGENTS.md` §19.10) — the default on every port that
    /// does not enforce UAF in hardware.
    ///
    /// A port whose silicon enforces UAF in hardware constructs the slab
    /// through [`Slab::with_tag_check`] instead, passing
    /// [`SoftwareTagCheck::for_tagging`] so the redundant software check
    /// is stood down.
    ///
    /// # Errors
    ///
    /// - [`AllocError::ZeroSize`] if `object_size == 0` or `slot_count == 0`.
    /// - [`AllocError::SizeUnsupported`] if the storage would overflow
    ///   `usize` (i.e. `object_size * slot_count + 2 * GUARD_BYTES`).
    pub fn new(object_size: usize, slot_count: usize) -> Result<Self, AllocError> {
        Self::with_tag_check(object_size, slot_count, SoftwareTagCheck::Enabled)
    }

    /// Construct a new slab with an explicit software-tag-check policy.
    ///
    /// Pass [`SoftwareTagCheck::for_tagging`] with the port's
    /// [`MemoryTagging`] handle so the software check disables itself
    /// only where hardware tagging already enforces use-after-free.
    ///
    /// # Errors
    ///
    /// As [`Slab::new`].
    pub fn with_tag_check(
        object_size: usize,
        slot_count: usize,
        tag_check: SoftwareTagCheck,
    ) -> Result<Self, AllocError> {
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
            tags: vec![MemTag::INITIAL; slot_count],
            tag_check,
        })
    }

    /// The software-tag-check policy this slab was constructed with.
    #[must_use]
    pub fn tag_check(&self) -> SoftwareTagCheck {
        self.tag_check
    }

    /// Reserve one slot and return its handle.
    ///
    /// # Errors
    ///
    /// - [`SlabError::Alloc`]`(`[`AllocError::OutOfMemory`]`)` when every
    ///   slot is taken.
    /// - [`SlabError::GuardViolation`] if a guard region was tampered with.
    /// - [`SlabError::DirtySlot`] if the free slot it would hand out is not
    ///   zeroed (the zero-on-free invariant was skipped or corrupted;
    ///   `AGENTS.md` §3.3, CWE-908/200).
    pub fn alloc(&mut self) -> Result<SlabHandle, SlabError> {
        // Pre-flight: a slab is never expected to grow without a
        // detected over-run. If guards have been clobbered the alloc
        // must fail closed (`AGENTS.md` §5.4).
        self.verify_guards_internal()?;
        for i in 0..self.slot_count {
            if !self.in_use[i] {
                // The zero-on-free invariant means a free slot must be
                // all-zero (`free` wipes it; a fresh slab starts zeroed).
                // Verify it before reuse so a slot whose zero-on-free was
                // skipped or corrupted cannot leak its previous occupant's
                // bytes to this caller (`AGENTS.md` §3.3, CWE-908/200).
                // Fail closed (§5.4): leave the slot free and reject.
                if !self.slot_is_clean(i) {
                    return Err(SlabError::DirtySlot);
                }
                self.in_use[i] = true;
                // Rotate the slot's tag so any handle still holding the
                // previous tag (a dangling pointer into this slot) will
                // mismatch and be rejected (`AGENTS.md` §19.10). When the
                // port enforces UAF in hardware the software rotation is
                // redundant overhead, so it is skipped and the slot keeps
                // its resting tag.
                let tag = if self.tag_check.is_enabled() {
                    let rotated = next_free_tag(self.tags[i], TAG_COUNT);
                    self.tags[i] = rotated;
                    rotated
                } else {
                    self.tags[i]
                };
                return Ok(SlabHandle { slot: i, tag });
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
    /// - [`SlabError::TagMismatch`] if the handle's tag no longer matches
    ///   the slot's current tag (a use-after-free; `AGENTS.md` §19.10).
    /// - [`SlabError::GuardViolation`] if either guard region was
    ///   tampered with while the slot was live.
    pub fn free(&mut self, h: SlabHandle) -> Result<(), SlabError> {
        if h.slot >= self.slot_count {
            return Err(SlabError::UnknownHandle);
        }
        if !self.in_use[h.slot] {
            return Err(SlabError::DoubleFree);
        }
        if self.tag_check.is_enabled() && h.tag != self.tags[h.slot] {
            return Err(SlabError::TagMismatch);
        }
        self.verify_guards_internal()?;
        // Zero the slot's bytes — keeps the slab clean between uses
        // and means freed slots don't leak their previous contents to
        // the *next* allocator caller (cheap defence-in-depth, not
        // the same thing as `sensitive` zero-on-free).
        let off = GUARD_BYTES + h.slot * self.object_size;
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
        self.in_use[h.slot] = false;
        Ok(())
    }

    /// Borrow the byte slice backing a live slot.
    ///
    /// # Errors
    ///
    /// - [`SlabError::UnknownHandle`] / [`SlabError::DoubleFree`] for
    ///   stale handles.
    /// - [`SlabError::TagMismatch`] if the handle outlived its allocation
    ///   and the slot has since been reused (a use-after-free;
    ///   `AGENTS.md` §19.10).
    pub fn slot_mut(&mut self, h: SlabHandle) -> Result<&mut [u8], SlabError> {
        if h.slot >= self.slot_count {
            return Err(SlabError::UnknownHandle);
        }
        if !self.in_use[h.slot] {
            return Err(SlabError::DoubleFree);
        }
        if self.tag_check.is_enabled() && h.tag != self.tags[h.slot] {
            return Err(SlabError::TagMismatch);
        }
        let off = GUARD_BYTES + h.slot * self.object_size;
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

    /// Test-only override of slot `slot`'s stored memory tag.
    ///
    /// Models direct tampering with the slab's `tags[]` metadata — a
    /// freelist/metadata-corruption primitive (`AGENTS.md` §19.10),
    /// distinct from the natural rotation [`Slab::alloc`] performs. A
    /// later [`Slab::slot_mut`]/[`Slab::free`] presented with the
    /// *original* handle must then be rejected as a
    /// [`SlabError::TagMismatch`]. Production code never writes `tags[]`
    /// outside `alloc`. Returns `None` if `slot` is out of range.
    #[cfg(test)]
    pub(crate) fn poke_tag_for_test(&mut self, slot: usize, tag: MemTag) -> Option<()> {
        *self.tags.get_mut(slot)? = tag;
        Some(())
    }

    /// Test-only override of slot `slot`'s `in_use` bit.
    ///
    /// Models direct tampering with the slab's allocation bitmap — a
    /// freelist-corruption primitive. Flipping a freed slot's bit to
    /// `true` must never let the allocator hand the same live slot out
    /// twice (no aliasing). Production code never writes `in_use[]`
    /// outside `alloc`/`free`. Returns `None` if `slot` is out of range.
    #[cfg(test)]
    pub(crate) fn poke_in_use_for_test(&mut self, slot: usize, in_use: bool) -> Option<()> {
        *self.in_use.get_mut(slot)? = in_use;
        Some(())
    }

    /// `true` if slot `slot`'s data bytes are all zero.
    ///
    /// The zero-on-free invariant ([`Slab::free`] wipes the slot, a fresh
    /// slab starts zeroed) means every *free* slot is all-zero, so this is
    /// the check [`Slab::alloc`] runs before reuse to catch a slot whose
    /// zero-on-free was skipped or corrupted (`AGENTS.md` §3.3).
    fn slot_is_clean(&self, slot: usize) -> bool {
        let off = GUARD_BYTES + slot * self.object_size;
        self.storage[off..off + self.object_size]
            .iter()
            .all(|&b| b == 0)
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
    use rustos_arch_api::{MemoryTagging, Tagging, TaggingProfile};

    /// Minimal [`MemoryTagging`] stub for exercising
    /// [`SoftwareTagCheck::for_tagging`]: it reports a fixed profile and
    /// the Arm-MTE-like 16-byte / 16-value geometry.
    struct StubTagging {
        profile: TaggingProfile,
    }

    impl MemoryTagging for StubTagging {
        fn profile(&self) -> TaggingProfile {
            self.profile
        }
        fn granule_bytes(&self) -> usize {
            16
        }
        fn tag_count(&self) -> u8 {
            TAG_COUNT
        }
    }

    fn hardware_enforcing() -> StubTagging {
        StubTagging {
            profile: TaggingProfile {
                tag_storage: Tagging::Supported,
                tag_check_faults: Tagging::Supported,
            },
        }
    }

    fn pending_port() -> StubTagging {
        StubTagging {
            profile: TaggingProfile {
                tag_storage: Tagging::Supported,
                tag_check_faults: Tagging::Pending("Tagged page attribute lands in Stage 6"),
            },
        }
    }

    fn untagged_port() -> StubTagging {
        StubTagging {
            profile: TaggingProfile {
                tag_storage: Tagging::Unsupported("no memory-tagging silicon"),
                tag_check_faults: Tagging::Unsupported("no memory-tagging silicon"),
            },
        }
    }

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
        let bogus = SlabHandle {
            slot: 99,
            tag: MemTag::INITIAL,
        };
        assert!(matches!(s.free(bogus), Err(SlabError::UnknownHandle)));
    }

    #[test]
    fn use_after_free_then_realloc_is_a_tag_mismatch() {
        // One slot, so a free + alloc reuses it. The stale handle keeps
        // the tag the slot held before it was freed; the realloc rotates
        // the slot's tag, so the stale handle now mismatches and any
        // access through it is rejected as a use-after-free.
        let mut s = Slab::new(16, 1).unwrap();
        let stale = s.alloc().unwrap();
        s.free(stale).unwrap();
        let fresh = s.alloc().unwrap();
        assert_ne!(stale.tag(), fresh.tag());
        assert!(matches!(s.slot_mut(stale), Err(SlabError::TagMismatch)));
        assert!(matches!(s.free(stale), Err(SlabError::TagMismatch)));
        // The live handle for the reused slot still works.
        s.slot_mut(fresh).unwrap();
    }

    #[test]
    fn each_reallocation_rotates_the_tag() {
        // Repeated alloc/free of the same slot must keep changing the tag
        // so no two consecutive lifetimes share one (`AGENTS.md` §19.10).
        let mut s = Slab::new(8, 1).unwrap();
        let mut previous = None;
        for _ in 0..TAG_COUNT {
            let h = s.alloc().unwrap();
            if let Some(p) = previous {
                assert_ne!(p, h.tag());
            }
            previous = Some(h.tag());
            s.free(h).unwrap();
        }
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
    fn new_defaults_to_software_tag_check_enabled() {
        // The software UAF check is on by default on every port that
        // does not enforce UAF in hardware (`AGENTS.md` §19.10).
        let s = Slab::new(16, 1).unwrap();
        assert_eq!(s.tag_check(), SoftwareTagCheck::Enabled);
        assert!(s.tag_check().is_enabled());
        assert_eq!(SoftwareTagCheck::default(), SoftwareTagCheck::Enabled);
    }

    #[test]
    fn for_tagging_disables_only_under_hardware_enforcement() {
        // Hardware that both stores tags and faults on a mismatch makes
        // the software check redundant overhead: stand it down.
        assert_eq!(
            SoftwareTagCheck::for_tagging(&hardware_enforcing()),
            SoftwareTagCheck::Disabled
        );
        // A port that can store tags but does not yet fault (Pending) is
        // not enforcing UAF in hardware, so the software check stays on.
        assert_eq!(
            SoftwareTagCheck::for_tagging(&pending_port()),
            SoftwareTagCheck::Enabled
        );
        // No tagging silicon at all: software check stays on.
        assert_eq!(
            SoftwareTagCheck::for_tagging(&untagged_port()),
            SoftwareTagCheck::Enabled
        );
    }

    #[test]
    fn disabled_check_skips_rotation_and_tag_mismatch() {
        // With the software check stood down (hardware enforces UAF) the
        // slab does no tag rotation and never reports TagMismatch — the
        // hardware tag checker is the line of defence, and the redundant
        // software work is skipped for performance.
        let mut s =
            Slab::with_tag_check(16, 1, SoftwareTagCheck::for_tagging(&hardware_enforcing()))
                .unwrap();
        assert_eq!(s.tag_check(), SoftwareTagCheck::Disabled);

        let stale = s.alloc().unwrap();
        s.free(stale).unwrap();
        let fresh = s.alloc().unwrap();
        // No rotation happened: the slot's resting tag is unchanged.
        assert_eq!(stale.tag(), fresh.tag());
        // The software check does not fire; the (stale) handle is
        // accepted because tag comparison is skipped.
        assert!(s.slot_mut(stale).is_ok());
        s.free(fresh).unwrap();
    }

    #[test]
    fn disabled_check_still_detects_double_free_and_unknown_handle() {
        // Standing down the *tag* check must not weaken the other slab
        // invariants (`AGENTS.md` §5.4 fail closed).
        let mut s = Slab::with_tag_check(16, 1, SoftwareTagCheck::Disabled).unwrap();
        let h = s.alloc().unwrap();
        s.free(h).unwrap();
        assert!(matches!(s.free(h), Err(SlabError::DoubleFree)));
        let bogus = SlabHandle {
            slot: 99,
            tag: MemTag::INITIAL,
        };
        assert!(matches!(s.free(bogus), Err(SlabError::UnknownHandle)));
    }

    #[test]
    fn display_messages_present() {
        extern crate std;
        use std::format;
        assert!(format!("{}", SlabError::DoubleFree).contains("double"));
        assert!(format!("{}", SlabError::UnknownHandle).contains("handle"));
        assert!(format!("{}", SlabError::TagMismatch).contains("use-after-free"));
        assert!(format!("{}", SlabError::GuardViolation).contains("guard"));
        assert!(format!("{}", SlabError::Alloc(AllocError::OutOfMemory)).contains("memory"));
        assert!(format!("{}", SlabError::DirtySlot).contains("zeroed"));
    }

    // -- §3.2 deliberate metadata-corruption tests ------------------------
    //
    // These drive the *detector*: corrupt the slab's `tags[]` / `in_use[]`
    // metadata through the sanctioned `#[cfg(test)]` trapdoors and assert
    // the next operation fails closed (`AGENTS.md` §5.4) rather than
    // handing back a live aliased object or a stale slot.

    #[test]
    fn tampering_with_a_slots_tag_is_a_tag_mismatch() {
        let mut s = Slab::new(32, 2).unwrap();
        let h = s.alloc().unwrap();
        // Corrupt the slot's recorded tag directly — metadata tampering,
        // not the natural rotation an alloc/free would perform.
        let forged = next_free_tag(h.tag(), TAG_COUNT);
        assert_ne!(forged, h.tag());
        s.poke_tag_for_test(h.slot, forged).unwrap();
        // The original handle now disagrees with the slot's tag: every
        // access path must reject it (`AGENTS.md` §19.10).
        assert!(matches!(s.slot_mut(h), Err(SlabError::TagMismatch)));
        assert!(matches!(s.free(h), Err(SlabError::TagMismatch)));
    }

    #[test]
    fn tampering_with_the_in_use_bitmap_never_aliases_a_live_slot() {
        let mut s = Slab::new(32, 2).unwrap();
        let live = s.alloc().unwrap();
        // Forge the *free* slot's bitmap bit to "in use" behind the
        // allocator's back (a freelist-corruption primitive). Now every
        // slot looks busy, so the allocator must fail closed rather than
        // alias a live slot.
        s.poke_in_use_for_test(1, true).unwrap();
        assert!(matches!(
            s.alloc(),
            Err(SlabError::Alloc(AllocError::OutOfMemory))
        ));
        // The genuinely-live handle is unaffected and still serviceable.
        assert!(s.slot_mut(live).is_ok());
    }

    #[test]
    fn clearing_a_live_slots_in_use_bit_is_caught_as_a_double_free() {
        let mut s = Slab::new(32, 1).unwrap();
        let live = s.alloc().unwrap();
        // Forge the live slot's bit to "free": a subsequent free of the
        // genuine handle must be rejected, never freeing twice.
        s.poke_in_use_for_test(0, false).unwrap();
        assert!(matches!(s.free(live), Err(SlabError::DoubleFree)));
    }

    #[test]
    fn guard_violation_detected_at_the_data_guard_boundaries() {
        // The exact off-by-one under-/over-run bytes: last byte of the
        // leading guard, and the first byte of the trailing guard.
        let mut head = Slab::new(16, 2).unwrap();
        let _h = head.alloc().unwrap();
        // SAFETY: test-only trapdoor; the byte is owned and we observe
        // the resulting GuardViolation immediately. No alias is created.
        unsafe { head.poke_for_test(GUARD_BYTES - 1, 0).unwrap() };
        assert!(matches!(
            head.check_guards(),
            Err(SlabError::GuardViolation)
        ));

        let data_bytes = 16 * 2;
        let mut tail = Slab::new(16, 2).unwrap();
        let _h = tail.alloc().unwrap();
        // SAFETY: as above.
        unsafe { tail.poke_for_test(GUARD_BYTES + data_bytes, 0).unwrap() };
        assert!(matches!(
            tail.check_guards(),
            Err(SlabError::GuardViolation)
        ));
    }

    // -- §3.3 stale-data / dirty-slot reuse tests -------------------------
    //
    // These prove zero-on-free is an *enforced* invariant, not incidental
    // (`AGENTS.md` §3.3 / §4 of the charter, CWE-908/200): if a freed
    // slot's wipe is skipped or corrupted, the reuse path must refuse the
    // slot rather than leak its previous occupant's bytes.

    #[test]
    fn reusing_a_slot_whose_zero_on_free_was_skipped_is_rejected() {
        let mut s = Slab::new(32, 1).unwrap();
        let h = s.alloc().unwrap();
        // Write a recognisable "credential" into the slot, then free it —
        // `free` is supposed to wipe every byte.
        for b in s.slot_mut(h).unwrap().iter_mut() {
            *b = 0xA5;
        }
        s.free(h).unwrap();
        // Simulate the zero-on-free being skipped/corrupted: scribble a
        // leftover credential byte back into the freed slot's storage.
        // SAFETY: test-only trapdoor over owned storage; no alias, and we
        // observe the resulting `DirtySlot` immediately.
        unsafe { s.poke_for_test(GUARD_BYTES, 0xA5).unwrap() };
        // Reuse must fail closed rather than hand back the dirty bytes.
        assert!(matches!(s.alloc(), Err(SlabError::DirtySlot)));
    }

    #[test]
    fn a_clean_freed_slot_reallocs_without_a_dirty_slot_error() {
        // The honest negative: an untampered free/realloc round-trip never
        // trips the dirty-slot detector, so the check does not regress the
        // normal path.
        let mut s = Slab::new(32, 1).unwrap();
        let h = s.alloc().unwrap();
        for b in s.slot_mut(h).unwrap().iter_mut() {
            *b = 0xA5;
        }
        s.free(h).unwrap();
        let h2 = s.alloc().expect("clean slot reallocs");
        assert!(s.slot_mut(h2).unwrap().iter().all(|&b| b == 0));
    }

    use alloc::format;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

        /// §3.7 / §4 (CWE-787): a single-byte corruption at *any* storage
        /// offset is either detected (guard) or lands in legal slot bytes,
        /// and the next operations stay **total** — they return `Ok` or a
        /// typed `Err`, never UB, never a panic, and never a live aliased
        /// object (the returned slice is always the requested slot's
        /// length).
        ///
        /// This validates the *detector*, not the impossibility of
        /// corruption (`AGENTS.md` §2.6, §6 of the charter).
        #[test]
        fn single_byte_storage_corruption_is_total_and_never_aliases(
            object_size in 1usize..64,
            slot_count in 1usize..8,
            offset in 0usize..(GUARD_BYTES * 2 + 64 * 8),
            byte in any::<u8>(),
        ) {
            let mut s = Slab::new(object_size, slot_count).unwrap();
            let h = s.alloc().unwrap();
            // Scribble one arbitrary byte anywhere; an out-of-range
            // offset is a harmless `None`.
            // SAFETY: test-only trapdoor over owned storage, no alias.
            unsafe {
                let _ = s.poke_for_test(offset, byte);
            }
            match s.check_guards() {
                Ok(()) | Err(SlabError::GuardViolation) => {}
                Err(other) => prop_assert!(false, "unexpected guard error {other:?}"),
            }
            match s.slot_mut(h) {
                Ok(slice) => prop_assert_eq!(slice.len(), object_size),
                Err(SlabError::GuardViolation) => {}
                Err(other) => prop_assert!(false, "unexpected access error {other:?}"),
            }
        }
    }
}
