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

/// Build the two-region physical-memory map for the discovered RAM
/// window `[ram_base, ram_base + ram_size)`, reserving everything up to
/// the page-aligned `kernel_end` and marking the remainder usable.
///
/// `kernel_end` is the linker-provided one-past-the-end address of the
/// kernel image including the boot heap (`__kernel_end`). It is rounded
/// up to a whole [`PAGE_SIZE`] frame so the usable region the allocator
/// receives starts on a frame boundary.
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
) -> Result<BootMemoryMap, MemoryMapError> {
    let ram_end = ram_base
        .checked_add(ram_size)
        .ok_or(MemoryMapError::AddressOverflow)?;
    let usable_start =
        align_up(kernel_end, PAGE_SIZE as u64).ok_or(MemoryMapError::AddressOverflow)?;
    if usable_start < ram_base || usable_start >= ram_end {
        return Err(MemoryMapError::UsableRegionEmpty);
    }

    let mut map = BootMemoryMap::new();
    map.push(MemoryRegion {
        kind: RegionKind::Reserved,
        start: PhysAddr::new(ram_base),
        length: usable_start - ram_base,
    });
    map.push(MemoryRegion {
        kind: RegionKind::Usable,
        start: PhysAddr::new(usable_start),
        length: ram_end - usable_start,
    });
    Ok(map)
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
    use super::{build_memory_map, region_byte_totals, MemoryMapError};
    use rustos_kernel_mem::{RegionKind, PAGE_SIZE};

    /// The QEMU `virt` board's RAM base (GiB 1).
    const VIRT_RAM_BASE: u64 = 0x4000_0000;

    #[test]
    fn page_aligned_kernel_end_yields_reserved_then_usable() {
        let ram_size = 0x4000_0000; // 1 GiB
        let kernel_end = VIRT_RAM_BASE + 0x10_0000; // 1 MiB image, already aligned
        let map =
            build_memory_map(VIRT_RAM_BASE, ram_size, kernel_end).expect("window is well-formed");

        let regions = map.regions();
        assert_eq!(regions.len(), 2);

        assert_eq!(regions[0].kind, RegionKind::Reserved);
        assert_eq!(regions[0].start.as_u64(), VIRT_RAM_BASE);
        assert_eq!(regions[0].length, 0x10_0000);

        assert_eq!(regions[1].kind, RegionKind::Usable);
        assert_eq!(regions[1].start.as_u64(), kernel_end);
        assert_eq!(regions[1].length, ram_size - 0x10_0000);

        // The two regions are contiguous and cover the whole window.
        assert_eq!(
            regions[1].start.as_u64() + regions[1].length,
            VIRT_RAM_BASE + ram_size,
        );
    }

    #[test]
    fn unaligned_kernel_end_rounds_up_to_a_whole_frame() {
        let ram_size = 0x4000_0000;
        let kernel_end = VIRT_RAM_BASE + 0x10_0123; // mid-page
        let map = build_memory_map(VIRT_RAM_BASE, ram_size, kernel_end).expect("well-formed");

        let usable_start = map.regions()[1].start.as_u64();
        assert_eq!(usable_start % PAGE_SIZE as u64, 0);
        assert_eq!(usable_start, VIRT_RAM_BASE + 0x10_1000);
        // No byte of memory is lost: reserved end meets usable start.
        let reserved = map.regions()[0];
        assert_eq!(reserved.start.as_u64() + reserved.length, usable_start);
    }

    #[test]
    fn byte_totals_split_usable_and_reserved() {
        let ram_size = 0x4000_0000;
        let kernel_end = VIRT_RAM_BASE + 0x20_0000; // 2 MiB
        let map = build_memory_map(VIRT_RAM_BASE, ram_size, kernel_end).expect("well-formed");
        let (usable, reserved) = region_byte_totals(&map);
        assert_eq!(reserved, 0x20_0000);
        assert_eq!(usable, ram_size - 0x20_0000);
        assert_eq!(usable + reserved, ram_size);
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
}
