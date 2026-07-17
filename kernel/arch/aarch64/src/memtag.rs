//! aarch64 memory tagging — Arm MTE.
//!
//! Implements the Arch HAL
//! [`MemoryTagging`](tairix_arch_api::MemoryTagging) surface for aarch64
//! using the **Memory Tagging Extension** (`FEAT_MTE` / `FEAT_MTE2`,
//! ARMv8.5-A). MTE is the canonical hardware use-after-free defence: it
//! stores a 4-bit *allocation tag* for every 16-byte granule, carries a
//! matching 4-bit *logical tag* in pointer bits `[59:56]`, and — when the
//! region is mapped `Normal Tagged` and tag checking is enabled — faults
//! on any access whose pointer tag does not match the granule tag.
//! Rotating the tag on free (so a dangling pointer keeps the stale tag)
//! is what turns a use-after-free into a deterministic fault.
//!
//! # Tag geometry
//!
//! MTE's granule is **16 bytes** and its tag is **4 bits** (16 values),
//! which is exactly the architecture-neutral [`tairix_arch_api::TAG_COUNT`]
//! the HAL defines, so [`MemoryTagging::rotate_tag`](tairix_arch_api::MemoryTagging::rotate_tag) yields a real MTE
//! tag with no narrowing.
//!
//! # What is implemented
//!
//! * [`set_region_tag`](tairix_arch_api::MemoryTagging::set_region_tag) emits the MTE `stg` (Store
//!   Allocation Tag) sequence over each granule of the region — the real
//!   store-tag path, ready for the allocator to stamp a freshly-rotated
//!   tag onto a region.
//!
//! # Why both slots are `Pending`
//!
//! The `stg` store is only architecturally defined when MTE is *enabled*
//! (`SCTLR_EL1.ATA`/`TCF`) and the target memory is mapped `Normal
//! Tagged` via its stage-1 attributes; on a core without `FEAT_MTE` `stg`
//! is `UNDEFINED`. Enabling MTE therefore needs (a) a runtime `FEAT_MTE`
//! probe (`ID_AA64PFR1_EL1.MTE`) and (b) the `Normal Tagged` page
//! attribute + the synchronous tag-check-fault decode, both of which land
//! with the Stage 6 user/kernel boundary and the real page-table
//! attribute work (`PLAN.md` §19 burn-down). Until then the store path is
//! gated behind a per-handle `mte_enabled` flag that defaults **off**, so
//! the sequence is compiled and reviewed but never executed on
//! possibly-MTE-less silicon. The profile is honestly [`Tagging::Pending`](tairix_arch_api::Tagging::Pending)
//! on both slots — not release-ready — exactly as the side-channel
//! KPTI / Spectre-v2 slots are `Pending` on this port. Use-after-free is
//! hardened *today* by the architecture-neutral software tag check in
//! `kernel/mem`, which shares this HAL's tag rotation.

use tairix_arch_api::{MemTag, MemoryTagging, Tagging, TaggingProfile};

/// MTE allocation-tag granule, in bytes (Arm Architecture Reference
/// Manual: a tag covers a 16-byte granule).
const GRANULE_BYTES: usize = 16;

/// aarch64 implementation of the Arch HAL memory-tagging surface.
///
/// Carries the `mte_enabled` gate (see the module docs): only a handle
/// constructed after a successful `FEAT_MTE` probe and `Normal Tagged`
/// mapping (Stage 6) may emit the `stg` store sequence. [`Self::new`]
/// builds the default, gated-off handle.
#[derive(Debug, Default, Clone, Copy)]
pub struct MemoryTags {
    mte_enabled: bool,
}

impl MemoryTags {
    /// Construct the aarch64 memory-tagging handle with MTE stores gated
    /// **off** (the only safe default until Stage 6 probes `FEAT_MTE` and
    /// maps `Normal Tagged` memory).
    #[must_use]
    pub const fn new() -> Self {
        Self { mte_enabled: false }
    }

    /// Construct a handle with the MTE `stg` store path **enabled**.
    ///
    /// Stage 6 calls this only after confirming `ID_AA64PFR1_EL1.MTE`
    /// reports `FEAT_MTE` and after enabling tag checking; passing `true`
    /// on a core without MTE, or before mapping the target memory
    /// `Normal Tagged`, would make [`Self::set_region_tag`] execute an
    /// `UNDEFINED` instruction.
    #[must_use]
    pub const fn with_mte_enabled() -> Self {
        Self { mte_enabled: true }
    }

    /// `true` if this handle will emit the MTE store-tag sequence.
    #[must_use]
    pub const fn mte_enabled(self) -> bool {
        self.mte_enabled
    }

    /// The honest declaration for aarch64 (see the module docs).
    #[must_use]
    pub const fn declared_profile() -> TaggingProfile {
        TaggingProfile {
            tag_storage: Tagging::Pending(
                "the MTE `stg` store is only defined when FEAT_MTE is enabled (SCTLR_EL1.ATA) \
                 and the region is mapped Normal Tagged; the ID_AA64PFR1_EL1.MTE probe and the \
                 Tagged page attribute land with the Stage 6 user/kernel boundary (PLAN.md §19)",
            ),
            tag_check_faults: Tagging::Pending(
                "the synchronous tag-check fault needs the Normal Tagged stage-1 attribute and \
                 the abort decode, delivered by the Stage 6 page-table work (PLAN.md §19)",
            ),
        }
    }
}

impl MemoryTagging for MemoryTags {
    fn profile(&self) -> TaggingProfile {
        Self::declared_profile()
    }

    fn granule_bytes(&self) -> usize {
        GRANULE_BYTES
    }

    fn tag_count(&self) -> u8 {
        // MTE's 4-bit tag == the neutral TAG_COUNT (16); no narrowing.
        tairix_arch_api::TAG_COUNT
    }

    unsafe fn set_region_tag(&self, base: *mut u8, len: usize, tag: MemTag) {
        if !self.mte_enabled {
            return;
        }
        // SAFETY: the caller (Stage 6, the only constructor of an enabled
        // handle) guarantees `base .. base+len` is a single live region
        // mapped Normal Tagged with MTE enabled, `base` granule-aligned
        // and `len` a granule multiple. `store_allocation_tags` only
        // reads `tag`'s 4-bit value and writes the granule tags within
        // that region.
        unsafe { store_allocation_tags(base, len, tag) }
    }
}

/// Stamp the MTE allocation tag `tag` onto every 16-byte granule of
/// `base .. base + len` using the `stg` (Store Allocation Tag)
/// instruction.
///
/// # Safety
///
/// Requires `FEAT_MTE` enabled (else `stg` is `UNDEFINED`), the region
/// mapped `Normal Tagged`, `base` aligned to [`GRANULE_BYTES`] and `len`
/// a multiple of it, and the region a single live allocation owned by
/// the caller.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
#[target_feature(enable = "mte")]
unsafe fn store_allocation_tags(base: *mut u8, len: usize, tag: MemTag) {
    // MTE carries the logical tag in pointer bits [59:56].
    let tag_bits = ((tag.value() as u64) & 0xF) << 56;
    let mut addr = base as u64;
    let end = addr.wrapping_add(len as u64);
    while addr < end {
        let tagged = (addr & !(0xF_u64 << 56)) | tag_bits;
        // SAFETY: `stg <x>, [<x>]` stores the 4-bit allocation tag taken
        // from the address operand's bits [59:56] to the 16-byte granule
        // at that address. The caller's contract guarantees MTE is
        // enabled and the granule is within a live Normal Tagged region,
        // so the store cannot fault or alias outside the region.
        unsafe {
            core::arch::asm!(
                "stg {t}, [{t}]",
                t = in(reg) tagged,
                options(nostack, preserves_flags),
            );
        }
        addr = addr.wrapping_add(GRANULE_BYTES as u64);
    }
}

/// Host build: the `stg` instruction does not exist off the bare-metal
/// aarch64 target, so the store is a no-op (the conformance vertical and
/// host tests still exercise the gate and the tag algebra).
#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
unsafe fn store_allocation_tags(_base: *mut u8, _len: usize, _tag: MemTag) {}

#[cfg(test)]
mod tests {
    use super::*;
    use tairix_arch_api::memtag::conformance;
    use tairix_arch_api::{next_free_tag, TAG_COUNT};

    #[test]
    fn passes_memtag_conformance() {
        conformance::run_all(&MemoryTags::new());
        conformance::run_all(&MemoryTags::with_mte_enabled());
    }

    #[test]
    fn declared_profile_is_honest_but_not_release_ready() {
        let profile = MemoryTags::new().profile();
        assert_eq!(profile.validate(), Ok(()));
        // MTE is real silicon support, but enabling it needs Stage 6: both
        // slots are tracked Pending, so the port is not release-ready yet.
        assert!(profile.tag_storage.is_pending());
        assert!(profile.tag_check_faults.is_pending());
        assert!(!profile.is_release_ready());
        assert!(!profile.enforces_uaf_in_hardware());
    }

    #[test]
    fn reports_real_mte_geometry() {
        let m = MemoryTags::new();
        assert_eq!(m.granule_bytes(), GRANULE_BYTES);
        assert_eq!(m.tag_count(), TAG_COUNT);
    }

    #[test]
    fn rotate_tag_uses_the_full_mte_space() {
        let m = MemoryTags::new();
        for raw in 0..TAG_COUNT {
            let prev = MemTag::new(raw);
            assert_eq!(m.rotate_tag(prev), next_free_tag(prev, TAG_COUNT));
            assert_ne!(m.rotate_tag(prev), prev);
        }
    }

    #[test]
    fn store_gate_defaults_off_and_is_a_safe_noop_on_host() {
        let disabled = MemoryTags::new();
        assert!(!disabled.mte_enabled());
        let enabled = MemoryTags::with_mte_enabled();
        assert!(enabled.mte_enabled());

        // Both are no-ops on the host (the `stg` path is cfg-gated out);
        // calling through either handle must not panic.
        let mut buf = [0u8; GRANULE_BYTES];
        // SAFETY: `buf` is a live, granule-sized stack allocation; on the
        // host `set_region_tag` is a no-op for both handles.
        unsafe {
            disabled.set_region_tag(buf.as_mut_ptr(), buf.len(), MemTag::new(1));
            enabled.set_region_tag(buf.as_mut_ptr(), buf.len(), MemTag::new(2));
        }
    }
}
