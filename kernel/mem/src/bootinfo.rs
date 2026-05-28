//! Typed boot-loader memory map.
//!
//! The bootloader is the only authority that knows the *actual* layout
//! of physical RAM on the machine: where the kernel image was loaded,
//! which ranges are firmware-reserved, which ranges are usable RAM, and
//! which ranges are MMIO. The frame allocator must respect those
//! reservations exactly — otherwise it will hand out a frame the
//! firmware still owns.
//!
//! This module defines the typed handover format. The boot stubs in
//! `kernel/arch/*` (Stage 3) construct a [`BootMemoryMap`] from whatever
//! protocol the platform exposes (multiboot2, UEFI, DTB, …) and pass it
//! to [`crate::FrameAllocator::new`].

use alloc::vec::Vec;

use crate::frame::{PhysAddr, PAGE_SIZE};

/// Kind of a single contiguous physical-memory region.
///
/// "Reserved" subsumes everything the allocator must keep its hands off:
/// firmware data, MMIO, the kernel image, the boot stack, the device
/// tree, etc. The frame allocator treats every non-[`Usable`] region as
/// off-limits, so we deliberately do *not* distinguish further.
///
/// [`Usable`]: RegionKind::Usable
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RegionKind {
    /// Free RAM that the frame allocator may hand out.
    Usable,
    /// Firmware-reserved, MMIO, kernel image, or other untouchable.
    Reserved,
}

/// One contiguous run of physical addresses with a single [`RegionKind`].
///
/// Ranges may be page-misaligned at the byte level (firmware regions
/// often are). The frame allocator rounds [`RegionKind::Usable`]
/// regions *inward* to whole frames, so partially-page-aligned reserved
/// areas never accidentally hand out partial frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryRegion {
    /// Inclusive start of the region (physical byte address).
    pub start: PhysAddr,
    /// Length of the region in bytes.
    pub length: u64,
    /// What kind of memory this is.
    pub kind: RegionKind,
}

impl MemoryRegion {
    /// One past the last byte of the region.
    ///
    /// Returns `None` if `start + length` would overflow `u64`. Callers
    /// rely on this to refuse to construct a frame allocator from a map
    /// that claims memory past the architectural limit.
    #[must_use]
    pub fn end(&self) -> Option<PhysAddr> {
        self.start
            .as_u64()
            .checked_add(self.length)
            .map(PhysAddr::new)
    }

    /// `true` if this region contains at least one whole, aligned frame.
    ///
    /// A 100-byte usable region contains no whole frames, so the frame
    /// allocator will skip it. This is the rule that prevents partial
    /// frames being handed out at region boundaries.
    #[must_use]
    pub fn has_whole_frame(&self) -> bool {
        let Some(end) = self.end() else {
            return false;
        };
        let first = self.start.as_u64().div_ceil(PAGE_SIZE as u64);
        let last_excl = end.as_u64() / PAGE_SIZE as u64;
        last_excl > first
    }
}

/// An ordered list of physical-memory regions describing the whole
/// machine's physical address space, as observed at boot.
///
/// The map is *additive*: overlapping regions are not allowed, and the
/// allocator's constructor verifies this (`AGENTS.md` §2.10 — fail
/// closed). The list need not cover the full address space; any gap is
/// implicitly treated as [`RegionKind::Reserved`] (i.e. unusable).
#[derive(Debug, Clone, Default)]
pub struct BootMemoryMap {
    regions: Vec<MemoryRegion>,
}

impl BootMemoryMap {
    /// Construct an empty map. Use [`Self::push`] to add regions, then
    /// hand the map to [`crate::FrameAllocator::new`].
    #[must_use]
    pub const fn new() -> Self {
        Self {
            regions: Vec::new(),
        }
    }

    /// Append a region.
    ///
    /// Validation is deferred to [`crate::FrameAllocator::new`], which
    /// rejects overlaps and integer overflows. Keeping this method
    /// infallible lets `arch/*` code build the map in any order and
    /// validate once at the end.
    pub fn push(&mut self, region: MemoryRegion) {
        self.regions.push(region);
    }

    /// View the regions in insertion order.
    #[must_use]
    pub fn regions(&self) -> &[MemoryRegion] {
        &self.regions
    }

    /// `true` if the map contains no regions.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }

    /// Highest physical byte address mentioned by any region (exclusive).
    ///
    /// Returns `0` for an empty map. Returns `None` if any region's end
    /// would overflow `u64`.
    #[must_use]
    pub fn highest_address(&self) -> Option<PhysAddr> {
        let mut hi = 0u64;
        for r in &self.regions {
            let end = r.end()?.as_u64();
            if end > hi {
                hi = end;
            }
        }
        Some(PhysAddr::new(hi))
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn empty_map_is_empty() {
        let m = BootMemoryMap::new();
        assert!(m.is_empty());
        assert_eq!(m.regions().len(), 0);
        assert_eq!(m.highest_address(), Some(PhysAddr::new(0)));
    }

    #[test]
    fn region_end_overflow_yields_none() {
        let r = MemoryRegion {
            start: PhysAddr::new(u64::MAX - 10),
            length: 100,
            kind: RegionKind::Usable,
        };
        assert!(r.end().is_none());
    }

    #[test]
    fn region_end_normal() {
        let r = MemoryRegion {
            start: PhysAddr::new(0x1000),
            length: 0x2000,
            kind: RegionKind::Usable,
        };
        assert_eq!(r.end(), Some(PhysAddr::new(0x3000)));
    }

    #[test]
    fn has_whole_frame_rejects_tiny_region() {
        let r = MemoryRegion {
            start: PhysAddr::new(0x1010),
            length: 100,
            kind: RegionKind::Usable,
        };
        assert!(!r.has_whole_frame());
    }

    #[test]
    fn has_whole_frame_accepts_full_frame() {
        let r = MemoryRegion {
            start: PhysAddr::new(0x1000),
            length: PAGE_SIZE as u64,
            kind: RegionKind::Usable,
        };
        assert!(r.has_whole_frame());
    }

    #[test]
    fn highest_address_picks_max_end() {
        let mut m = BootMemoryMap::new();
        m.push(MemoryRegion {
            start: PhysAddr::new(0),
            length: 0x1000,
            kind: RegionKind::Usable,
        });
        m.push(MemoryRegion {
            start: PhysAddr::new(0x4000),
            length: 0x2000,
            kind: RegionKind::Reserved,
        });
        assert_eq!(m.highest_address(), Some(PhysAddr::new(0x6000)));
    }

    #[test]
    fn highest_address_overflow() {
        let mut m = BootMemoryMap::new();
        m.push(MemoryRegion {
            start: PhysAddr::new(u64::MAX),
            length: 1,
            kind: RegionKind::Usable,
        });
        assert!(m.highest_address().is_none());
    }
}
