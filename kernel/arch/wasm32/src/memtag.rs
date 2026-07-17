//! wasm32 memory tagging.
//!
//! Implements the Arch HAL
//! [`MemoryTagging`](tairix_arch_api::MemoryTagging) surface for the
//! browser sandbox (`wasm32-unknown-unknown`).
//!
//! WebAssembly exposes no per-granule memory-tagging primitive: linear
//! memory is a flat byte array and there is no host call to stamp or
//! check a tag. Spatial and temporal safety in the browser is provided
//! by the sandbox itself — each module instance owns a single bounds-
//! checked linear memory, and TAIRiX runs one such memory per Web Worker
//! (`kernel/arch/wasm32::isolation`), so a stray access traps at the
//! WASM bounds check rather than escaping the instance.
//!
//! Both features are therefore an honest
//! [`Tagging::Unsupported`](tairix_arch_api::Tagging::Unsupported)
//! — the host owns the relevant protection. Use-after-free *within* a
//! linear memory is still hardened by the architecture-neutral *software*
//! tag check in `kernel/mem`, which shares the
//! [`tairix_arch_api::next_free_tag`] rotation this HAL defines.

use tairix_arch_api::{MemoryTagging, Tagging, TaggingProfile};

/// wasm32 implementation of the Arch HAL memory-tagging surface.
///
/// Zero-sized: an untagged port carries no per-instance state.
#[derive(Debug, Default, Clone, Copy)]
pub struct MemoryTags;

impl MemoryTags {
    /// Construct the wasm32 memory-tagging handle.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// The honest declaration for wasm32 (see the module docs).
    #[must_use]
    pub const fn declared_profile() -> TaggingProfile {
        TaggingProfile {
            tag_storage: Tagging::Unsupported(
                "WebAssembly linear memory is a flat byte array with no host primitive to stamp \
                 a per-granule tag",
            ),
            tag_check_faults: Tagging::Unsupported(
                "there is no WASM tag-check fault; spatial safety is the host sandbox's \
                 per-worker bounds-checked linear memory, and UAF within it is hardened by the \
                 software tag check in kernel/mem",
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
    use tairix_arch_api::memtag::conformance;

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
