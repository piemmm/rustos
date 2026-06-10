//! Translate the firmware-discovered `/memory` window into the canonical
//! [`BootMemoryMap`] the live allocator hand-off consumes
//! (`plans/PI.md` P6c-1).
//!
//! The aarch64 boot path (`boot_aarch64`) discovers the board's RAM
//! window from the device tree — never a fabricated static list
//! (`AGENTS.md` §18.2). This module turns that `(base, size)` pair and
//! the linker-provided end of the kernel image into the two-region
//! physical map the frame allocator needs (`plans/PI.md` P6c-2): the span
//! from the RAM base through the kernel image + boot heap is
//! [`RegionKind::Reserved`], and the remainder is [`RegionKind::Usable`].
//! This is the aarch64 analogue of the riscv64 boot pipeline's
//! `build_memory_map`, kept as its own pure routine rather than copied
//! (`AGENTS.md` §2.2 carve-out: each port owns its discovery, but the
//! arithmetic here is self-contained and host-tested).
//!
//! The arithmetic is deliberately free of the aarch64 architecture crate
//! so it is exercised by host unit tests under `cargo test`
//! (`AGENTS.md` §7): the `boot_aarch64` module that calls it links the
//! bare-metal-only port and cannot be host-compiled, so the
//! correctness-critical bounds checks would otherwise never run on the
//! CI host. The module compiles on the aarch64 production build (where
//! `boot_aarch64` consumes it) and on any host `cargo test` build (where
//! the tests below consume it), and on no other configuration, so it is
//! never dead code (`AGENTS.md` §2.3).

use rustos_kernel_mem::{BootMemoryMap, MemoryRegion, PhysAddr, RegionKind, PAGE_SIZE};

/// Alignment of the kthread-stack guard arena: one L2 block (2 MiB).
///
/// Laying the arena out on a 2 MiB boundary means each of its guard pages
/// becomes its own L3 leaf after the boot path re-expresses the covering
/// block at 4 KiB granularity
/// ([`rustos_arch_aarch64::paging::AddressSpace::prepare_guard_arena`]), so
/// a guard page can be unmapped without shattering the 2 MiB block the
/// running CPU executes on (`plans/PI.md` guard-page fault-form, stage G2).
const GUARD_ARENA_ALIGN: u64 = 2 * 1024 * 1024;

/// Smallest reserved kthread-stack guard arena: one 2 MiB block.
///
/// One block still holds tens of guarded kthread kernel stacks (each a few
/// pages of stack plus a one-page guard), so even the tiniest discovered RAM
/// window that can spare a block gets a working arena. A window too small to
/// carve even this floor degrades to no arena and the software-canary
/// `BoxStack` fallback ([`carve_guard_arena`]).
const STACK_ARENA_MIN_BYTES: u64 = GUARD_ARENA_ALIGN;

/// Largest reserved kthread-stack guard arena: 64 MiB.
///
/// A cap on the §24.2 "fraction of discovered RAM" policy so a very large
/// server does not reserve an unbounded slab up front for kthread stacks it
/// will never all use at once. 64 MiB holds well over a thousand guarded
/// stacks (`AGENTS.md` §24.2 — a workable headroom for both desktop and
/// server without waste). Growth past this on genuine exhaustion is the
/// staged follow-on (the growable/chained arena, `plans/PI.md`/PLAN §24 L3b).
const STACK_ARENA_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Headroom policy: reserve roughly 1/64 of the discovered RAM window for
/// kthread kernel stacks (`AGENTS.md` §24.1 — a capacity derived from
/// discovered hardware, never a hand-picked literal that caps a large
/// machine or wastes a small one).
const STACK_ARENA_RAM_SHIFT: u32 = 6;

/// Size the reserved kthread-stack guard arena from the discovered RAM
/// window `ram_size`, per the §24.2 default policy.
///
/// The target is a fixed fraction of RAM ([`STACK_ARENA_RAM_SHIFT`]),
/// clamped to `[STACK_ARENA_MIN_BYTES, STACK_ARENA_MAX_BYTES]` and rounded
/// **down** to a whole [`GUARD_ARENA_ALIGN`] (2 MiB) block so every guard
/// page in the arena still lands on its own L3 leaf after
/// [`rustos_arch_aarch64::paging::AddressSpace::prepare_guard_arena`]. The
/// result is therefore always a non-zero multiple of 2 MiB. This is a
/// *policy* (a function of discovered hardware), not a frozen scalar, so a
/// 64 MiB embedded board and a 256 GiB server each get a workable arena from
/// the same code (`AGENTS.md` §24.2). Whether that arena actually fits the
/// usable remainder is decided by [`carve_guard_arena`], which fails closed
/// to no arena when it does not.
fn stack_arena_bytes(ram_size: u64) -> u64 {
    let target = ram_size >> STACK_ARENA_RAM_SHIFT;
    let clamped = target.clamp(STACK_ARENA_MIN_BYTES, STACK_ARENA_MAX_BYTES);
    // Round down to a whole 2 MiB block; the minimum is already a multiple of
    // `GUARD_ARENA_ALIGN`, so the floored result stays >= one block.
    clamped & !(GUARD_ARENA_ALIGN - 1)
}

/// Why the discovered RAM window could not be turned into a usable map.
///
/// Each variant is a fail-closed refusal (`AGENTS.md` §2.9): the boot
/// path records the cause in its audit line and parks rather than
/// handing the allocator a map it cannot trust.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum MemoryMapError {
    /// `ram_base + ram_size`, or the page-aligned kernel-image end,
    /// overflowed the 64-bit physical address space.
    AddressOverflow,
    /// The page-aligned end of the kernel image does not fall strictly
    /// inside the discovered RAM window, so no whole usable frame
    /// remains to hand the allocator.
    UsableRegionEmpty,
}

impl MemoryMapError {
    /// Stable cause string for the boot audit line (`AGENTS.md` §5.4.4).
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::AddressOverflow => "address_overflow",
            Self::UsableRegionEmpty => "usable_region_empty",
        }
    }
}

/// Round `value` up to the next multiple of `align` (a power of two),
/// returning `None` if the rounding would overflow `u64`.
fn align_up(value: u64, align: u64) -> Option<u64> {
    let mask = align - 1;
    value.checked_add(mask).map(|sum| sum & !mask)
}

/// The reserved, 2 MiB-aligned kthread-stack guard arena `(base, len)`
/// the boot path fine-maps and the allocator must not touch.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) struct GuardArena {
    /// First physical byte of the arena (2 MiB-aligned).
    pub(crate) base: u64,
    /// Arena length in bytes — a whole multiple of [`GUARD_ARENA_ALIGN`]
    /// sized from the discovered RAM window by [`stack_arena_bytes`].
    pub(crate) len: u64,
}

/// The physical-memory map plus the reserved guard arena carved from it.
///
/// [`build_memory_map`] returns both so the boot path hands the allocator
/// the [`map`](Self::map) and fine-maps the [`arena`](Self::arena) (when
/// one fits) through the page-table block-split.
#[derive(Clone, Debug)]
pub(crate) struct MemoryLayout {
    /// The physical-memory map the frame allocator consumes.
    pub(crate) map: BootMemoryMap,
    /// The reserved guard arena, or `None` when the usable window is too
    /// small to carve a 2 MiB-aligned block out of (fail-closed: the boot
    /// path simply leaves the kthread-stack guard in its software-canary
    /// form, `plans/PI.md` stage G2 watch-out).
    pub(crate) arena: Option<GuardArena>,
}

/// Carve a 2 MiB-aligned guard arena of `arena_bytes` out of the usable
/// window `[usable_start, ram_end)`.
///
/// `arena_bytes` is the §24.2 policy size from [`stack_arena_bytes`] (a
/// whole multiple of [`GUARD_ARENA_ALIGN`]). The arena is placed at the
/// first 2 MiB boundary at or after `usable_start` (above the kernel image,
/// so it never overlaps the running code or boot stack). Returns `None` if
/// the whole `arena_bytes` block does not fit before `ram_end`, so a tiny
/// RAM window degrades to no arena rather than a wrapped or overlapping
/// region (`AGENTS.md` §2.9).
fn carve_guard_arena(usable_start: u64, ram_end: u64, arena_bytes: u64) -> Option<GuardArena> {
    let base = align_up(usable_start, GUARD_ARENA_ALIGN)?;
    let end = base.checked_add(arena_bytes)?;
    if end > ram_end {
        return None;
    }
    Some(GuardArena {
        base,
        len: arena_bytes,
    })
}

/// Build the physical-memory map for the discovered RAM window
/// `[ram_base, ram_base + ram_size)`, reserving everything up to the
/// page-aligned `kernel_end`, carving a 2 MiB-aligned kthread-stack guard
/// arena out of the usable remainder, and marking what is left usable.
///
/// `kernel_end` is the linker-provided one-past-the-end address of the
/// kernel image including the boot heap (`__kernel_end`). It is rounded
/// up to a whole [`PAGE_SIZE`] frame so the usable region the allocator
/// receives starts on a frame boundary.
///
/// The returned [`MemoryLayout`] pairs the allocator map with the carved
/// [`GuardArena`] (when one fits). The map's regions, in physical order,
/// are: the [`RegionKind::Reserved`] kernel image, an optional
/// [`RegionKind::Usable`] head below the arena, the
/// [`RegionKind::Reserved`] guard arena, and the [`RegionKind::Usable`]
/// remainder. Zero-length usable spans are omitted so no degenerate
/// region reaches the allocator. The arena's frames are reserved so the
/// allocator never hands them out (`AGENTS.md` §4); the boot path
/// re-expresses the arena at 4 KiB granularity so a guard page in it can
/// later be unmapped (`plans/PI.md` stage G2/G3).
///
/// # Errors
///
/// Returns [`MemoryMapError::AddressOverflow`] if the RAM window or the
/// page-aligned kernel end overflows `u64`, or
/// [`MemoryMapError::UsableRegionEmpty`] if the page-aligned kernel end
/// is not strictly inside the RAM window (no usable span remains).
pub(crate) fn build_memory_map(
    ram_base: u64,
    ram_size: u64,
    kernel_end: u64,
) -> Result<MemoryLayout, MemoryMapError> {
    let ram_end = ram_base
        .checked_add(ram_size)
        .ok_or(MemoryMapError::AddressOverflow)?;
    let usable_start =
        align_up(kernel_end, PAGE_SIZE as u64).ok_or(MemoryMapError::AddressOverflow)?;
    if usable_start < ram_base || usable_start >= ram_end {
        return Err(MemoryMapError::UsableRegionEmpty);
    }

    let arena = carve_guard_arena(usable_start, ram_end, stack_arena_bytes(ram_size));

    let mut map = BootMemoryMap::new();
    // The kernel image + boot heap: always reserved, from the RAM base
    // through the first usable frame.
    map.push(MemoryRegion {
        kind: RegionKind::Reserved,
        start: PhysAddr::new(ram_base),
        length: usable_start - ram_base,
    });

    match arena {
        Some(GuardArena { base, len }) => {
            let arena_end = base + len;
            // Usable head between the kernel image and the 2 MiB-aligned
            // arena (omitted when the arena starts exactly at the first
            // usable frame).
            if base > usable_start {
                map.push(MemoryRegion {
                    kind: RegionKind::Usable,
                    start: PhysAddr::new(usable_start),
                    length: base - usable_start,
                });
            }
            // The reserved guard arena itself.
            map.push(MemoryRegion {
                kind: RegionKind::Reserved,
                start: PhysAddr::new(base),
                length: len,
            });
            // The usable remainder above the arena (omitted when the arena
            // ends exactly at the RAM window).
            if arena_end < ram_end {
                map.push(MemoryRegion {
                    kind: RegionKind::Usable,
                    start: PhysAddr::new(arena_end),
                    length: ram_end - arena_end,
                });
            }
        }
        None => {
            map.push(MemoryRegion {
                kind: RegionKind::Usable,
                start: PhysAddr::new(usable_start),
                length: ram_end - usable_start,
            });
        }
    }

    Ok(MemoryLayout { map, arena })
}

/// Total bytes the map covers of each [`RegionKind`], in `(usable,
/// reserved)` order. Used by the boot path to record the discovered
/// split in its audit line.
pub(crate) fn region_byte_totals(map: &BootMemoryMap) -> (u64, u64) {
    let mut usable = 0u64;
    let mut reserved = 0u64;
    for region in map.regions() {
        match region.kind {
            RegionKind::Usable => usable = usable.saturating_add(region.length),
            RegionKind::Reserved => reserved = reserved.saturating_add(region.length),
        }
    }
    (usable, reserved)
}

#[cfg(test)]
mod tests {
    use super::{
        build_memory_map, region_byte_totals, stack_arena_bytes, MemoryMapError, GUARD_ARENA_ALIGN,
        STACK_ARENA_MAX_BYTES, STACK_ARENA_MIN_BYTES,
    };
    use rustos_kernel_mem::{RegionKind, PAGE_SIZE};

    /// The QEMU `virt` board's RAM base (GiB 1).
    const VIRT_RAM_BASE: u64 = 0x4000_0000;
    /// 2 MiB block alignment, mirrored from the module.
    const TWO_MIB: u64 = 0x20_0000;

    #[test]
    fn kernel_then_head_then_reserved_arena_then_usable() {
        let ram_size = 0x4000_0000; // 1 GiB
        let kernel_end = VIRT_RAM_BASE + 0x10_0000; // 1 MiB image, already aligned
        let layout =
            build_memory_map(VIRT_RAM_BASE, ram_size, kernel_end).expect("window is well-formed");
        let regions = layout.map.regions();
        // Reserved kernel, usable head (kernel end is not 2 MiB-aligned),
        // reserved arena, usable remainder.
        assert_eq!(regions.len(), 4);

        assert_eq!(regions[0].kind, RegionKind::Reserved);
        assert_eq!(regions[0].start.as_u64(), VIRT_RAM_BASE);
        assert_eq!(regions[0].length, 0x10_0000);

        assert_eq!(regions[1].kind, RegionKind::Usable);
        assert_eq!(regions[1].start.as_u64(), kernel_end);

        let arena = layout.arena.expect("an arena fits in a 1 GiB window");
        assert_eq!(arena.len, stack_arena_bytes(ram_size));
        assert_eq!(arena.base % TWO_MIB, 0, "arena is 2 MiB-aligned");
        assert!(
            arena.base >= kernel_end,
            "arena sits above the kernel image"
        );
        assert_eq!(regions[2].kind, RegionKind::Reserved);
        assert_eq!(regions[2].start.as_u64(), arena.base);
        assert_eq!(regions[2].length, arena.len);

        assert_eq!(regions[3].kind, RegionKind::Usable);
        assert_eq!(regions[3].start.as_u64(), arena.base + arena.len);

        // The regions are contiguous and cover the whole window exactly.
        assert_regions_tile_window(&layout, VIRT_RAM_BASE, ram_size);
    }

    #[test]
    fn arena_aligned_kernel_end_omits_the_usable_head() {
        // A 2 MiB-aligned kernel end leaves the arena starting exactly at
        // the first usable frame, so there is no head-usable region.
        let ram_size = 0x4000_0000;
        let kernel_end = VIRT_RAM_BASE + TWO_MIB;
        let layout = build_memory_map(VIRT_RAM_BASE, ram_size, kernel_end).expect("well-formed");
        let regions = layout.map.regions();
        assert_eq!(regions.len(), 3, "no usable head when the arena is aligned");
        let arena = layout.arena.expect("arena fits");
        assert_eq!(arena.base, kernel_end);
        assert_eq!(regions[1].kind, RegionKind::Reserved);
        assert_eq!(regions[1].start.as_u64(), arena.base);
        assert_regions_tile_window(&layout, VIRT_RAM_BASE, ram_size);
    }

    #[test]
    fn unaligned_kernel_end_rounds_up_to_a_whole_frame() {
        let ram_size = 0x4000_0000;
        let kernel_end = VIRT_RAM_BASE + 0x10_0123; // mid-page
        let layout = build_memory_map(VIRT_RAM_BASE, ram_size, kernel_end).expect("well-formed");

        let usable_start = layout.map.regions()[1].start.as_u64();
        assert_eq!(usable_start % PAGE_SIZE as u64, 0);
        assert_eq!(usable_start, VIRT_RAM_BASE + 0x10_1000);
        // No byte of memory is lost: reserved end meets the first usable
        // frame, and every region tiles the window.
        let reserved = layout.map.regions()[0];
        assert_eq!(reserved.start.as_u64() + reserved.length, usable_start);
        assert_regions_tile_window(&layout, VIRT_RAM_BASE, ram_size);
    }

    #[test]
    fn byte_totals_count_kernel_and_arena_as_reserved() {
        let ram_size = 0x4000_0000;
        let kernel_end = VIRT_RAM_BASE + TWO_MIB; // 2 MiB, already aligned
        let layout = build_memory_map(VIRT_RAM_BASE, ram_size, kernel_end).expect("well-formed");
        let (usable, reserved) = region_byte_totals(&layout.map);
        // The kernel image (2 MiB) plus the policy-sized reserved arena.
        assert_eq!(reserved, TWO_MIB + stack_arena_bytes(ram_size));
        assert_eq!(usable, ram_size - reserved);
        assert_eq!(usable + reserved, ram_size);
    }

    #[test]
    fn small_window_too_tight_for_an_arena_yields_none() {
        // 3 MiB total, 1 MiB kernel: the next 2 MiB-aligned arena block
        // would run past the window, so no arena is carved and the map is
        // the plain reserved-then-usable split (fail closed, no overlap).
        let ram_size = 0x30_0000; // 3 MiB
        let kernel_end = VIRT_RAM_BASE + 0x10_0000; // 1 MiB
        let layout = build_memory_map(VIRT_RAM_BASE, ram_size, kernel_end).expect("well-formed");
        assert!(layout.arena.is_none(), "no arena fits a 3 MiB window");
        let regions = layout.map.regions();
        assert_eq!(regions.len(), 2);
        assert_eq!(regions[0].kind, RegionKind::Reserved);
        assert_eq!(regions[1].kind, RegionKind::Usable);
        assert_regions_tile_window(&layout, VIRT_RAM_BASE, ram_size);
    }

    /// Assert the map's regions tile `[ram_base, ram_base + ram_size)`
    /// exactly: contiguous, non-overlapping, and lossless.
    fn assert_regions_tile_window(layout: &super::MemoryLayout, ram_base: u64, ram_size: u64) {
        let regions = layout.map.regions();
        let mut cursor = ram_base;
        for region in regions {
            assert_eq!(region.start.as_u64(), cursor, "regions are contiguous");
            cursor += region.length;
        }
        assert_eq!(
            cursor,
            ram_base + ram_size,
            "regions cover the whole window"
        );
    }

    #[test]
    fn kernel_end_below_ram_base_is_rejected() {
        // A kernel end that precedes the RAM window cannot bound a
        // usable region: fail closed rather than emit a wrapped length.
        assert_eq!(
            build_memory_map(VIRT_RAM_BASE, 0x4000_0000, VIRT_RAM_BASE - 0x1000).unwrap_err(),
            MemoryMapError::UsableRegionEmpty,
        );
    }

    #[test]
    fn kernel_end_at_or_past_ram_end_is_rejected() {
        let ram_size = 0x10_0000; // 1 MiB
                                  // Kernel image fills the whole window: no usable frames remain.
        assert_eq!(
            build_memory_map(VIRT_RAM_BASE, ram_size, VIRT_RAM_BASE + ram_size).unwrap_err(),
            MemoryMapError::UsableRegionEmpty,
        );
        // And strictly past the end is equally refused.
        assert_eq!(
            build_memory_map(VIRT_RAM_BASE, ram_size, VIRT_RAM_BASE + ram_size + 0x4000)
                .unwrap_err(),
            MemoryMapError::UsableRegionEmpty,
        );
    }

    #[test]
    fn ram_window_overflow_is_rejected() {
        assert_eq!(
            build_memory_map(u64::MAX - 0x10, 0x100, u64::MAX - 0x10).unwrap_err(),
            MemoryMapError::AddressOverflow,
        );
    }

    #[test]
    fn kernel_end_alignment_overflow_is_rejected() {
        // A kernel end within a page of u64::MAX cannot be rounded up to
        // a frame boundary without overflowing.
        assert_eq!(
            build_memory_map(VIRT_RAM_BASE, 0x4000_0000, u64::MAX - 1).unwrap_err(),
            MemoryMapError::AddressOverflow,
        );
    }

    #[test]
    fn cause_strings_are_stable() {
        assert_eq!(MemoryMapError::AddressOverflow.as_str(), "address_overflow");
        assert_eq!(
            MemoryMapError::UsableRegionEmpty.as_str(),
            "usable_region_empty",
        );
    }

    #[test]
    fn arena_policy_floors_at_one_block_on_a_tiny_window() {
        // 64 MiB / 64 = 1 MiB, below the one-block floor, so a tiny machine
        // still gets a working arena rather than nothing (§24.2 default
        // policy) — and never wastes more than a single 2 MiB block.
        assert_eq!(stack_arena_bytes(64 * 1024 * 1024), STACK_ARENA_MIN_BYTES);
        // Even a degenerate, sub-block window floors to one block (the
        // separate fit check in `carve_guard_arena` is what refuses it).
        assert_eq!(stack_arena_bytes(0), STACK_ARENA_MIN_BYTES);
    }

    #[test]
    fn arena_policy_scales_with_ram() {
        // 1 GiB / 64 = 16 MiB: a desktop gets many more stacks than the old
        // single fixed 2 MiB block, derived from discovered RAM (§24.1).
        let one_gib = 0x4000_0000;
        assert_eq!(stack_arena_bytes(one_gib), 16 * 1024 * 1024);
        assert!(stack_arena_bytes(one_gib) > STACK_ARENA_MIN_BYTES);
    }

    #[test]
    fn arena_policy_caps_a_huge_window() {
        // 256 GiB / 64 = 4 GiB, well past the cap: a big server reserves the
        // bounded headroom, not an unbounded slab up front (§24.2). Growth
        // past this is the staged growable-arena follow-on (L3b).
        let huge = 256u64 * 1024 * 1024 * 1024;
        assert_eq!(stack_arena_bytes(huge), STACK_ARENA_MAX_BYTES);
    }

    #[test]
    fn arena_policy_is_always_a_whole_block_in_range() {
        for gib in [0u64, 1, 2, 5, 16, 64, 512] {
            let bytes = stack_arena_bytes(gib * 1024 * 1024 * 1024);
            assert_eq!(bytes % GUARD_ARENA_ALIGN, 0, "arena is whole 2 MiB blocks");
            assert!(bytes >= STACK_ARENA_MIN_BYTES, "never below the floor");
            assert!(bytes <= STACK_ARENA_MAX_BYTES, "never above the cap");
        }
    }

    #[test]
    fn large_window_reserves_more_than_one_block() {
        // An 8 GiB machine reserves a multi-block arena (capped at 64 MiB),
        // proving the carve consumes the policy size, not a fixed 2 MiB.
        let ram_size = 8u64 * 1024 * 1024 * 1024;
        let kernel_end = VIRT_RAM_BASE + TWO_MIB;
        let layout = build_memory_map(VIRT_RAM_BASE, ram_size, kernel_end).expect("well-formed");
        let arena = layout.arena.expect("a large window carves an arena");
        assert_eq!(arena.len, STACK_ARENA_MAX_BYTES);
        assert!(
            arena.len > GUARD_ARENA_ALIGN,
            "more than one block reserved"
        );
        assert_regions_tile_window(&layout, VIRT_RAM_BASE, ram_size);
    }
}
