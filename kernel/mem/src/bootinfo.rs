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

    /// Carve the physical range `[start, end)` out of every
    /// [`RegionKind::Usable`] region so the frame allocator can never hand
    /// out a frame overlapping it.
    ///
    /// Each usable region that overlaps the range is split into the (up to
    /// two) usable sub-ranges that fall *outside* `[start, end)`; the
    /// overlapping middle is dropped, becoming an implicit reserved gap (a
    /// gap in the map is treated as [`RegionKind::Reserved`], §85). Reserved
    /// regions pass through untouched. The no-overlap invariant
    /// [`crate::FrameAllocator::new`] enforces is preserved, since this only
    /// shrinks or splits existing usable regions — it never introduces a new
    /// overlapping one.
    ///
    /// This is how the boot path reserves the running kernel image (and its
    /// bump heap) out of firmware-usable RAM on platforms whose firmware
    /// memory map reports the loader-placed kernel as conventional/usable
    /// memory (e.g. the UEFI `EfiLoaderData`/`EfiConventionalMemory` the
    /// x86_64 boot path sees). A zero-width or inverted range is a no-op.
    pub fn reserve_range(&mut self, start: PhysAddr, end: PhysAddr) {
        let (cs, ce) = (start.as_u64(), end.as_u64());
        if ce <= cs {
            return;
        }
        let mut out: Vec<MemoryRegion> = Vec::with_capacity(self.regions.len() + 1);
        for r in self.regions.drain(..) {
            if r.kind != RegionKind::Usable {
                out.push(r);
                continue;
            }
            let Some(r_end) = r.end() else {
                // A region whose end overflows is left untouched; the
                // allocator constructor rejects it on its own merits.
                out.push(r);
                continue;
            };
            let (rs, re) = (r.start.as_u64(), r_end.as_u64());
            if re <= cs || rs >= ce {
                // Disjoint from the carved range.
                out.push(r);
                continue;
            }
            // Left remainder `[rs, cs)` stays usable.
            if rs < cs {
                out.push(MemoryRegion {
                    start: PhysAddr::new(rs),
                    length: cs - rs,
                    kind: RegionKind::Usable,
                });
            }
            // Right remainder `[ce, re)` stays usable.
            if ce < re {
                out.push(MemoryRegion {
                    start: PhysAddr::new(ce),
                    length: re - ce,
                    kind: RegionKind::Usable,
                });
            }
            // The overlapping middle is dropped (becomes a reserved gap).
        }
        self.regions = out;
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

    /// `reserve_range` splits a usable region straddling the carved range
    /// into the two outside remainders and drops the overlapping middle.
    /// This is the exact shape the x86_64 boot path relies on to reserve the
    /// kernel image out of one big `EfiConventionalMemory` run (`plans/PI.md`
    /// X4 follow-on — the frame-allocator-vs-kernel-image fix).
    #[test]
    fn reserve_range_splits_straddling_usable_region() {
        let mut m = BootMemoryMap::new();
        m.push(MemoryRegion {
            start: PhysAddr::new(0),
            length: 0x100_0000,
            kind: RegionKind::Usable,
        });
        // Carve out [0x10_0000, 0x44_0000) — the "kernel image".
        m.reserve_range(PhysAddr::new(0x10_0000), PhysAddr::new(0x44_0000));
        let regions = m.regions();
        assert_eq!(regions.len(), 2);
        assert_eq!(regions[0].start, PhysAddr::new(0));
        assert_eq!(regions[0].length, 0x10_0000);
        assert_eq!(regions[0].kind, RegionKind::Usable);
        assert_eq!(regions[1].start, PhysAddr::new(0x44_0000));
        assert_eq!(regions[1].length, 0x100_0000 - 0x44_0000);
        assert_eq!(regions[1].kind, RegionKind::Usable);
        // The carved frames are no longer in any usable region, so the frame
        // allocator (gap == reserved) can never hand them out.
        for r in regions {
            let end = r.end().unwrap().as_u64();
            assert!(
                end <= 0x10_0000 || r.start.as_u64() >= 0x44_0000,
                "carved range still usable: {r:?}"
            );
        }
    }

    /// `reserve_range` leaves `Reserved` regions and disjoint usable regions
    /// untouched, and truncates a usable region overlapped only at one end.
    #[test]
    fn reserve_range_truncates_and_skips() {
        let mut m = BootMemoryMap::new();
        // Reserved region overlapping the carve: must pass through unchanged.
        m.push(MemoryRegion {
            start: PhysAddr::new(0x2000),
            length: 0x1000,
            kind: RegionKind::Reserved,
        });
        // Usable region overlapped only at its low end by the carve.
        m.push(MemoryRegion {
            start: PhysAddr::new(0x10_0000),
            length: 0x10_0000,
            kind: RegionKind::Usable,
        });
        // Disjoint usable region far above the carve.
        m.push(MemoryRegion {
            start: PhysAddr::new(0x80_0000),
            length: 0x1000,
            kind: RegionKind::Usable,
        });
        // Carve [0, 0x10_8000): clips the low end of the middle region,
        // overlaps the reserved one (left untouched), misses the high one.
        m.reserve_range(PhysAddr::new(0), PhysAddr::new(0x10_8000));
        let regions = m.regions();
        assert_eq!(regions.len(), 3);
        assert!(regions.contains(&MemoryRegion {
            start: PhysAddr::new(0x2000),
            length: 0x1000,
            kind: RegionKind::Reserved,
        }));
        assert!(regions.contains(&MemoryRegion {
            start: PhysAddr::new(0x10_8000),
            length: 0x10_0000 - 0x8000,
            kind: RegionKind::Usable,
        }));
        assert!(regions.contains(&MemoryRegion {
            start: PhysAddr::new(0x80_0000),
            length: 0x1000,
            kind: RegionKind::Usable,
        }));
    }

    /// A zero-width or inverted carve range is a no-op.
    #[test]
    fn reserve_range_zero_width_is_noop() {
        let mut m = BootMemoryMap::new();
        m.push(MemoryRegion {
            start: PhysAddr::new(0),
            length: 0x10_0000,
            kind: RegionKind::Usable,
        });
        m.reserve_range(PhysAddr::new(0x4000), PhysAddr::new(0x4000));
        m.reserve_range(PhysAddr::new(0x8000), PhysAddr::new(0x4000));
        assert_eq!(m.regions().len(), 1);
        assert_eq!(m.regions()[0].length, 0x10_0000);
    }

    /// The carved range becomes an implicit reserved gap, so a frame
    /// allocator built from the clipped map never marks those frames usable.
    /// Locks the `reserve_range` ↔ `FrameAllocator` contract (`AGENTS.md`
    /// §2.2 — one reservation mechanism).
    #[test]
    fn reserve_range_frames_are_not_allocatable() {
        use crate::frame::{FrameAllocator, PAGE_SIZE};
        let mut m = BootMemoryMap::new();
        m.push(MemoryRegion {
            start: PhysAddr::new(0),
            length: 0x100_0000,
            kind: RegionKind::Usable,
        });
        // Reserve [0x10_0000, 0x44_0000) — the simulated kernel image.
        let (kstart, kend) = (0x10_0000u64, 0x44_0000u64);
        m.reserve_range(PhysAddr::new(kstart), PhysAddr::new(kend));
        let alloc = FrameAllocator::new(&m).expect("allocator builds");
        // Every frame the allocator hands out must lie outside the carve.
        for _ in 0..2048 {
            let Ok(frame) = alloc.alloc() else { break };
            let pa = frame.0 as u64 * PAGE_SIZE as u64;
            assert!(
                pa < kstart || pa >= kend,
                "allocator handed out a reserved kernel-image frame at {pa:#x}"
            );
        }
    }
}
