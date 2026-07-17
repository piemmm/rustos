//! Property tests for the frame-allocator invariants.
//!
//! These run in addition to the inline `#[cfg(test)]` proptest in
//! `src/frame.rs`: integration-level coverage that hammers the public
//! API exclusively, with no access to internal helpers.
//!
//! Invariants enforced here:
//!
//! 1. **No double allocation.** Two outstanding allocations never share
//!    a frame.
//! 2. **No leaks.** After the same number of frees as allocs the
//!    allocator's free count returns to its initial value.
//! 3. **Reserved frames untouchable.** A frame inside any reserved
//!    region is never observed in the alloc stream.

#![cfg(not(loom))]

use std::collections::HashSet;

use proptest::prelude::*;
use proptest::test_runner::Config;

use tairix_kernel_mem::{
    BootMemoryMap, Frame, FrameAllocator, MemoryRegion, PhysAddr, RegionKind, PAGE_SIZE,
};

/// Build a memory map with a small reserved hole carved out of an
/// otherwise contiguous usable region.
fn map_with_hole(usable_lo: usize, hole: (usize, usize), usable_hi: usize) -> BootMemoryMap {
    let mut m = BootMemoryMap::new();
    let to_bytes = |frames: usize| (frames * PAGE_SIZE) as u64;
    m.push(MemoryRegion {
        start: PhysAddr::new(0),
        length: to_bytes(usable_lo),
        kind: RegionKind::Usable,
    });
    m.push(MemoryRegion {
        start: PhysAddr::new(to_bytes(usable_lo)),
        length: to_bytes(hole.1 - hole.0),
        kind: RegionKind::Reserved,
    });
    m.push(MemoryRegion {
        start: PhysAddr::new(to_bytes(hole.1)),
        length: to_bytes(usable_hi - hole.1),
        kind: RegionKind::Usable,
    });
    m
}

proptest! {
    #![proptest_config(Config { cases: 96, ..Config::default() })]

    /// Random alloc/free sequences must uphold no-double-alloc and
    /// no-leak invariants on a simple contiguous map.
    #[test]
    fn contiguous_alloc_free_round_trip(ops in proptest::collection::vec(any::<u8>(), 1..256)) {
        let mut m = BootMemoryMap::new();
        m.push(MemoryRegion {
            start: PhysAddr::new(0),
            length: (64 * PAGE_SIZE) as u64,
            kind: RegionKind::Usable,
        });
        let a = FrameAllocator::new(&m).unwrap();
        let initial = a.free_frames();
        let mut held: Vec<Frame> = Vec::new();
        let mut seen: HashSet<usize> = HashSet::new();

        for op in ops {
            if op % 2 == 0 || held.is_empty() {
                if let Ok(f) = a.alloc() {
                    prop_assert!(seen.insert(f.0), "double alloc {}", f.0);
                    held.push(f);
                }
            } else {
                let idx = (op as usize) % held.len();
                let f = held.swap_remove(idx);
                seen.remove(&f.0);
                a.free(f).unwrap();
            }
        }
        for f in held {
            a.free(f).unwrap();
        }
        prop_assert_eq!(a.free_frames(), initial);
    }

    /// Reserved frames in the middle of the usable range must never
    /// appear in the allocation stream.
    #[test]
    fn reserved_window_never_yielded(seed in any::<u8>()) {
        // Frames 0..16 usable, 16..24 reserved, 24..40 usable.
        let map = map_with_hole(16, (16, 24), 40);
        let a = FrameAllocator::new(&map).unwrap();
        let mut handed = Vec::new();
        // Use `seed` to vary the operation count.
        let target = 1 + (seed as usize) % 32;
        for _ in 0..target {
            match a.alloc() {
                Ok(f) => {
                    prop_assert!(
                        !(16..24).contains(&f.0),
                        "reserved frame {} handed out",
                        f.0
                    );
                    handed.push(f);
                }
                Err(_) => break,
            }
        }
        for f in handed {
            a.free(f).unwrap();
        }
    }

    /// Mixed-order alloc/free preserves the free-frame total.
    #[test]
    fn mixed_order_alloc_free(orders in proptest::collection::vec(0u32..4u32, 1..32)) {
        let mut m = BootMemoryMap::new();
        m.push(MemoryRegion {
            start: PhysAddr::new(0),
            length: (64 * PAGE_SIZE) as u64,
            kind: RegionKind::Usable,
        });
        let a = FrameAllocator::new(&m).unwrap();
        let initial = a.free_frames();
        let mut held: Vec<(Frame, u32)> = Vec::new();
        for o in orders {
            if let Ok(f) = a.alloc_order(o) {
                held.push((f, o));
            }
        }
        for (f, o) in held {
            a.free_order(f, o).unwrap();
        }
        prop_assert_eq!(a.free_frames(), initial);
    }
}
