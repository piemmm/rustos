//! riscv64 memory tagging.
//!
//! Implements the Arch HAL
//! [`MemoryTagging`](rustos_arch_api::MemoryTagging) surface for riscv64.
//!
//! The RISC-V cores RustOS targets — the QEMU `virt` board and the
//! SiFive U-series (U54 / U74) — implement neither a ratified
//! memory-tagging extension nor pointer masking. The relevant proposals
//! (the pointer-masking extension `Zjpm` and the tagging work that builds
//! on it) are not present on this silicon, so there is no store-tag
//! instruction to emit and no tag-check fault to take.
//!
//! Both features are therefore an honest [`Tagging::Unsupported`](rustos_arch_api::Tagging::Unsupported).
//! Use-after-free on riscv64 is hardened by the architecture-neutral
//! *software* tag check in `kernel/mem` (sharing the
//! [`rustos_arch_api::next_free_tag`] rotation this HAL defines) plus the
//! slab guard pages and W^X. Were RustOS to add a RISC-V core that
//! implements ratified memory tagging, this profile must be revisited and
//! the store-tag / fault path wired, exactly as the side-channel
//! profile is revisited for an out-of-order core.

use rustos_arch_api::{MemoryTagging, Tagging, TaggingProfile};

/// riscv64 implementation of the Arch HAL memory-tagging surface.
///
/// Zero-sized: an untagged port carries no per-instance state.
#[derive(Debug, Default, Clone, Copy)]
pub struct MemoryTags;

impl MemoryTags {
    /// Construct the riscv64 memory-tagging handle.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// The honest declaration for riscv64 (see the module docs).
    #[must_use]
    pub const fn declared_profile() -> TaggingProfile {
        TaggingProfile {
            tag_storage: Tagging::Unsupported(
                "the RISC-V cores RustOS targets (QEMU virt, SiFive U54/U74) implement no \
                 ratified memory-tagging extension, so there is no store-tag instruction",
            ),
            tag_check_faults: Tagging::Unsupported(
                "those cores raise no tag-check fault; UAF is hardened by the software tag \
                 check in kernel/mem plus slab guard pages and W^X",
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
