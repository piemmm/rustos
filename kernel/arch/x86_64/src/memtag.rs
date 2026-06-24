//! x86_64 memory tagging.
//!
//! Implements the Arch HAL
//! [`MemoryTagging`](rustos_arch_api::MemoryTagging) surface for x86_64.
//!
//! Mainstream x86_64 silicon has **no per-granule memory tagging** of the
//! Arm-MTE / SPARC-ADI kind: there is no architectural store-tag
//! instruction and no pointer/granule tag-check fault. Intel LAM (Linear
//! Address Masking) and AMD UAI (Upper Address Ignore) mask high pointer
//! bits so software can stash metadata there, but the CPU does **not**
//! store a matching tag per memory granule and does **not** fault on a
//! mismatch — they are address-masking features, not memory tagging.
//!
//! So both features are an honest [`Tagging::Unsupported`](rustos_arch_api::Tagging::Unsupported). On
//! x86_64 use-after-free is hardened by the architecture-neutral
//! *software* tag check in `kernel/mem` (which uses the same
//! [`rustos_arch_api::next_free_tag`] rotation this HAL defines) layered
//! on the slab guard pages and W^X. The granule
//! is therefore the trivial one byte and the tag space collapses to the
//! single tag `0`.

use rustos_arch_api::{MemoryTagging, Tagging, TaggingProfile};

/// x86_64 implementation of the Arch HAL memory-tagging surface.
///
/// Zero-sized: an untagged port carries no per-instance state.
#[derive(Debug, Default, Clone, Copy)]
pub struct MemoryTags;

impl MemoryTags {
    /// Construct the x86_64 memory-tagging handle.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// The honest declaration for x86_64 (see the module docs).
    #[must_use]
    pub const fn declared_profile() -> TaggingProfile {
        TaggingProfile {
            tag_storage: Tagging::Unsupported(
                "mainstream x86_64 has no per-granule store-tag instruction; Intel LAM / AMD UAI \
                 mask high pointer bits but do not store a tag per memory granule",
            ),
            tag_check_faults: Tagging::Unsupported(
                "x86_64 raises no pointer/granule tag-check fault; UAF is hardened by the \
                 software tag check in kernel/mem plus slab guard pages and W^X",
            ),
        }
    }
}

impl MemoryTagging for MemoryTags {
    fn profile(&self) -> TaggingProfile {
        Self::declared_profile()
    }

    fn granule_bytes(&self) -> usize {
        1
    }

    fn tag_count(&self) -> u8 {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_arch_api::memtag::conformance;

    #[test]
    fn passes_memtag_conformance() {
        conformance::run_all(&MemoryTags::new());
    }

    #[test]
    fn declared_profile_is_honest_and_release_ready() {
        let profile = MemoryTags::new().profile();
        assert_eq!(profile.validate(), Ok(()));
        // x86_64 genuinely lacks memory-tagging silicon: both slots are
        // justified Unsupported, so the port has no outstanding tagging
        // gap and is release-ready.
        assert!(matches!(profile.tag_storage, Tagging::Unsupported(_)));
        assert!(matches!(profile.tag_check_faults, Tagging::Unsupported(_)));
        assert!(profile.is_release_ready());
        assert!(!profile.enforces_uaf_in_hardware());
    }

    #[test]
    fn untagged_geometry() {
        let m = MemoryTags::new();
        assert_eq!(m.granule_bytes(), 1);
        assert_eq!(m.tag_count(), 1);
    }
}
