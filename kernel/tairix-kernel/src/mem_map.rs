//! Translate the firmware-discovered `/memory` window into the canonical
//! [`BootMemoryMap`] the live allocator hand-off consumes
//! (`plans/PI.md` P6c-1).
//!
//! The aarch64 boot path (`aarch64::boot`) discovers the board's RAM
//! window from the device tree — never a fabricated static list. This module turns that `(base, size)` pair and
//! the linker-provided end of the kernel image into the two-region
//! physical map the frame allocator needs (`plans/PI.md` P6c-2): the span
//! from the RAM base through the kernel image + boot heap is
//! [`RegionKind::Reserved`], and the remainder is [`RegionKind::Usable`].
//! This is the aarch64 analogue of the riscv64 boot pipeline's
//! `build_memory_map`, kept as its own pure routine rather than copied
//! (carve-out: each port owns its discovery, but the
//! arithmetic here is self-contained and host-tested).
//!
//! The arithmetic is deliberately free of the architecture crates so it is
//! exercised by host unit tests under `cargo test`: the
//! `aarch64::boot` / `x86_64::boot` modules that call it link the bare-metal-only
//! ports and cannot be host-compiled, so the correctness-critical bounds
//! checks would otherwise never run on the CI host. The module compiles on
//! the bare-metal production builds (where `aarch64::boot` / `x86_64::boot` /
//! `riscv64::boot` consume it) and on any host `cargo test` build (where the
//! tests below consume it), and on no other configuration, so it is never
//! dead code. The single-window [`build_memory_map`] (aarch64) and the
//! identity-window sizing and page-directory carve [`identity_window_gib`] /
//! [`carve_frames_from_map`] (x86_64) are each gated to the port(s) that use
//! them.

use tairix_kernel_mem::{BootMemoryMap, PhysAddr};
// Only the aarch64 map builder and the x86_64 window sizing/carve classify
// regions; the riscv64 build reaches neither.
#[cfg(any(
    all(freestanding, any(kernel_isa = "aarch64", kernel_isa = "x86_64")),
    test
))]
use tairix_kernel_mem::RegionKind;
// `MemoryRegion` and `PAGE_SIZE` are named only by the aarch64 single-window
// `build_memory_map` and the host tests; the x86_64 carve reads regions
// through `BootMemoryMap` without naming the element type, so the import is
// gated to those configurations to stay free of an unused-import warning.
#[cfg(any(all(freestanding, kernel_isa = "aarch64"), test))]
use tairix_kernel_mem::{MemoryRegion, PAGE_SIZE};

/// Why the discovered RAM window could not be turned into a usable map.
///
/// Each variant is a fail-closed refusal: the boot
/// path records the cause in its audit line and parks rather than
/// handing the allocator a map it cannot trust.
///
/// Produced only by the aarch64 single-window [`build_memory_map`].
#[cfg(any(all(freestanding, kernel_isa = "aarch64"), test))]
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

#[cfg(any(all(freestanding, kernel_isa = "aarch64"), test))]
impl MemoryMapError {
    /// Stable cause string for the boot audit line.
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

/// Reserve out of `map` every whole physical frame the firmware blob at
/// `[base, base + len)` occupies (rounded outward to a page boundary).
///
/// A firmware structure the kernel keeps reading *after* the allocator
/// hand-off — the flattened device tree above all — is placed by firmware
/// wherever it likes, and on both the aarch64 `virt`/Pi and riscv64 `virt`
/// boards that landing spot is inside the RAM window the map otherwise marks
/// usable. Left usable, the blob is fair game for the frame allocator *and*
/// is overwritten by the early-boot RAM self-test that zeroes every usable
/// byte (`tairix_kernel_mem::ramtest`) — destroying the tree every later
/// consumer (device discovery, root-storage bind, the QEMU scenarios) still
/// reads. Reserving its frames keeps the blob live for the life of the
/// kernel, exactly as the kernel image is reserved.
///
/// The blob is the one shared definition both DTB-bearing ports call, so the
/// reservation cannot drift between them. A zero-length blob, or a `base +
/// len` / rounding that overflows `u64`, is a no-op (fail closed: a malformed
/// span reserves nothing rather than wrapping into an unrelated region).
#[cfg(any(
    all(freestanding, any(kernel_isa = "aarch64", kernel_isa = "riscv64")),
    test
))]
pub(crate) fn reserve_blob_frames(map: &mut BootMemoryMap, base: u64, len: u64) {
    if len == 0 {
        return;
    }
    let page = tairix_kernel_mem::PAGE_SIZE as u64;
    let frame_start = base & !(page - 1);
    let Some(frame_end) = base.checked_add(len).and_then(|end| align_up(end, page)) else {
        return;
    };
    map.reserve_range(PhysAddr::new(frame_start), PhysAddr::new(frame_end));
}

/// Split the discovered RAM `windows` into the subranges that lie in
/// gigapages the identity map types Normal — i.e. drop every byte that
/// falls in a gigapage `device_mask` claims (bit `g` of
/// `device_mask[g / 64]` set means gigapage `g` is Device-typed).
///
/// The aarch64 identity map types memory at 1 GiB granularity and Device
/// wins over RAM for a shared gigapage (MMIO must never be cached or
/// speculated), so RAM that shares a gigapage with a discovered MMIO block
/// — the Pi 4's window below 4 GiB ends inside the gigapage holding its
/// UART/GIC/PCIe — would be mapped Device: atomics on it are unpredictable
/// and the allocator must never hand it out. Such bytes are clipped here,
/// fail closed; reclaiming them needs 2 MiB-granular identity typing (the
/// staged follow-up in `plans/APPS.md` I4). A window past the 512 GiB
/// identity window is likewise clipped (no representable slot).
#[cfg(any(all(freestanding, kernel_isa = "aarch64"), test))]
pub(crate) fn clip_windows_to_normal_ram(
    windows: &[(u64, u64)],
    device_mask: &[u64],
) -> alloc::vec::Vec<(u64, u64)> {
    const GIB: u64 = 1 << 30;
    let gigapage_is_device = |gigapage: u64| -> bool {
        usize::try_from(gigapage / 64)
            .ok()
            .and_then(|word| device_mask.get(word))
            .is_some_and(|w| w & (1 << (gigapage % 64)) != 0)
    };
    let mut out = alloc::vec::Vec::new();
    for &(base, size) in windows {
        let Some(end) = base.checked_add(size) else {
            continue;
        };
        // Walk the window gigapage by gigapage, accumulating maximal
        // Normal-typed runs.
        let mut run_start: Option<u64> = None;
        let mut cursor = base;
        while cursor < end {
            let gigapage = cursor / GIB;
            let gigapage_end = (gigapage + 1).saturating_mul(GIB).min(end);
            if gigapage_is_device(gigapage) {
                if let Some(start) = run_start.take() {
                    out.push((start, cursor - start));
                }
            } else if run_start.is_none() {
                run_start = Some(cursor);
            }
            cursor = gigapage_end;
        }
        if let Some(start) = run_start {
            out.push((start, end - start));
        }
    }
    out
}

/// Build the physical-memory map for the discovered RAM `windows`,
/// reserving everything up to the page-aligned `kernel_end` inside the
/// window that holds the kernel image, carving a 2 MiB-aligned
/// kthread-stack guard arena out of that window's usable remainder, and
/// marking everything else — including every further window — usable.
///
/// `kernel_end` is the linker-provided one-past-the-end address of the
/// kernel image including the boot heap (`__kernel_end`). It is rounded
/// up to a whole [`PAGE_SIZE`] frame so the usable region the allocator
/// receives starts on a frame boundary.
///
/// The kernel window's regions, in physical order, are the
/// [`RegionKind::Reserved`] kernel image and the [`RegionKind::Usable`]
/// remainder; every other window contributes one whole
/// [`RegionKind::Usable`] region (a machine like the Pi 4 describes RAM
/// above its MMIO hole and above 4 GiB as further windows). Zero-length
/// windows and usable spans are omitted so no degenerate region reaches
/// the allocator.
///
/// # Errors
///
/// Returns [`MemoryMapError::AddressOverflow`] if a window or the
/// page-aligned kernel end overflows `u64`, or
/// [`MemoryMapError::UsableRegionEmpty`] if the page-aligned kernel end
/// is not strictly inside any window (no usable span could bound the
/// image — including an empty window list).
#[cfg(any(all(freestanding, kernel_isa = "aarch64"), test))]
pub(crate) fn build_memory_map(
    windows: &[(u64, u64)],
    kernel_end: u64,
) -> Result<BootMemoryMap, MemoryMapError> {
    let usable_start =
        align_up(kernel_end, PAGE_SIZE as u64).ok_or(MemoryMapError::AddressOverflow)?;

    // A window whose extent overflows is a malformed discovery and fails
    // closed.
    for &(base, size) in windows {
        base.checked_add(size)
            .ok_or(MemoryMapError::AddressOverflow)?;
    }

    // The kernel image must sit strictly inside exactly one window; that
    // window carries the reserve.
    let kernel_window = windows
        .iter()
        .copied()
        .find(|&(base, size)| usable_start >= base && usable_start < base + size)
        .ok_or(MemoryMapError::UsableRegionEmpty)?;
    let (ram_base, ram_size) = kernel_window;
    let ram_end = ram_base + ram_size;

    let mut map = BootMemoryMap::new();
    // The kernel image + boot heap: always reserved, from the RAM base
    // through the first usable frame.
    map.push(MemoryRegion {
        kind: RegionKind::Reserved,
        start: PhysAddr::new(ram_base),
        length: usable_start - ram_base,
    });

    if usable_start < ram_end {
        map.push(MemoryRegion {
            kind: RegionKind::Usable,
            start: PhysAddr::new(usable_start),
            length: ram_end - usable_start,
        });
    }

    // Every other discovered window is wholly usable RAM.
    for &(base, size) in windows {
        if (base, size) == kernel_window || size == 0 {
            continue;
        }
        map.push(MemoryRegion {
            kind: RegionKind::Usable,
            start: PhysAddr::new(base),
            length: size,
        });
    }

    Ok(map)
}

/// Total bytes the map covers of each [`RegionKind`], in `(usable,
/// reserved)` order. Used by the aarch64 boot path to record the discovered
/// split in its audit line (and by the host tests), so this is gated to the
/// configurations that name it to stay free of dead code.
#[cfg(any(all(freestanding, kernel_isa = "aarch64"), test))]
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

/// One gigabyte, the granularity the x86_64 identity window is sized in.
#[cfg(any(all(freestanding, kernel_isa = "x86_64"), test))]
const GIB: u64 = 1 << 30;

/// Size the identity/direct-map window, in whole gigabytes, from the RAM
/// `map` actually reports.
///
/// The window has to cover every frame the allocator can hand out, because
/// the kernel reaches a frame's bytes through it — a process image write, a
/// shared-region scrub, a page table, a slab page. It is therefore the top
/// of the highest **usable** region rounded up to a gigabyte, never the
/// map's highest address: a PC firmware map spans the reserved MMIO hole
/// well past the installed RAM, and sizing from that would map gigabytes of
/// holes.
///
/// `floor_gib` keeps the architectural MMIO frames the boot trampoline
/// already covers inside the window on a machine with less RAM than that.
/// `cap_gib` is the first gigabyte the window may not reach — the user
/// virtual base, since the identity map shares each process root's low half
/// with the child image. RAM above the cap is unreachable by pointer and
/// every consumer of it fails closed.
#[cfg(any(all(freestanding, kernel_isa = "x86_64"), test))]
pub(crate) fn identity_window_gib(map: &BootMemoryMap, floor_gib: usize, cap_gib: usize) -> usize {
    let top = map
        .regions()
        .iter()
        .filter(|region| region.kind == RegionKind::Usable)
        .filter_map(tairix_kernel_mem::MemoryRegion::end)
        .fold(0u64, |acc, end| acc.max(end.as_u64()));
    let gib = usize::try_from(top.div_ceil(GIB)).unwrap_or(cap_gib);
    gib.clamp(floor_gib, cap_gib)
}

/// Reserve `pages` physically contiguous, page-aligned frames below
/// `max_addr` out of `map`, returning their physical base.
///
/// The x86_64 identity widening needs page directories before the frame
/// allocator exists, and they must stay out of its hands afterwards, so
/// they are carved from the firmware map exactly as the guard arena is
/// — the same reserve-then-hand-back
/// shape, so neither can drift from the other. `max_addr` is the window
/// the caller can still write through while it installs the wider one.
///
/// The run is taken from the **highest** usable placement that fits, not
/// the lowest: a PC's first usable region is the legacy sub-640-KiB window
/// that holds the firmware's own structures and the physical address the
/// AP start-up trampoline is copied to, and a small run would otherwise
/// always land there. Nothing the boot path is still reading lives at the
/// top of usable RAM (firmware tables are `Reserved` in the map, so they
/// are never candidates).
///
/// Returns `None`, having changed nothing, when no usable region can host
/// the whole run below `max_addr` (the caller then fails the boot rather
/// than running on a window it did not install).
#[cfg(any(all(freestanding, kernel_isa = "x86_64"), test))]
pub(crate) fn carve_frames_from_map(
    map: &mut BootMemoryMap,
    pages: usize,
    max_addr: u64,
) -> Option<u64> {
    let page = tairix_kernel_mem::PAGE_SIZE as u64;
    let bytes = (pages as u64).checked_mul(page)?;
    if bytes == 0 {
        return None;
    }
    let mut chosen: Option<u64> = None;
    for region in map.regions() {
        if region.kind != RegionKind::Usable {
            continue;
        }
        let region_start = region.start.as_u64();
        let Some(region_end) = region_start.checked_add(region.length) else {
            continue;
        };
        let Some(start) = align_up(region_start, page) else {
            continue;
        };
        // Highest page-aligned base inside this region whose whole run stays
        // below both the region end and the writable window.
        let ceiling = region_end.min(max_addr);
        let Some(base) = ceiling.checked_sub(bytes).map(|top| top & !(page - 1)) else {
            continue;
        };
        if base >= start && base.saturating_add(bytes) <= ceiling {
            chosen = Some(chosen.map_or(base, |best: u64| best.max(base)));
        }
    }
    let base = chosen?;
    map.reserve_range(PhysAddr::new(base), PhysAddr::new(base + bytes));
    Some(base)
}

#[cfg(test)]
mod tests {
    use super::{
        build_memory_map, carve_frames_from_map, identity_window_gib, region_byte_totals,
        MemoryMapError,
    };
    use tairix_kernel_mem::{RegionKind, PAGE_SIZE};

    /// The QEMU `virt` board's RAM base (GiB 1).
    const VIRT_RAM_BASE: u64 = 0x4000_0000;
    /// 2 MiB block alignment, mirrored from the module.
    const TWO_MIB: u64 = 0x20_0000;

    #[test]
    fn unaligned_kernel_end_rounds_up_to_a_whole_frame() {
        let ram_size = 0x4000_0000;
        let kernel_end = VIRT_RAM_BASE + 0x10_0123; // mid-page
        let map = build_memory_map(&[(VIRT_RAM_BASE, ram_size)], kernel_end).expect("well-formed");

        let usable_start = map.regions()[1].start.as_u64();
        assert_eq!(usable_start % PAGE_SIZE as u64, 0);
        assert_eq!(usable_start, VIRT_RAM_BASE + 0x10_1000);
        // No byte of memory is lost: reserved end meets the first usable
        // frame, and every region tiles the window.
        let reserved = map.regions()[0];
        assert_eq!(reserved.start.as_u64() + reserved.length, usable_start);
        assert_regions_tile_window(&map, VIRT_RAM_BASE, ram_size);
    }

    /// Assert the map's regions tile `[ram_base, ram_base + ram_size)`
    /// exactly: contiguous, non-overlapping, and lossless.
    fn assert_regions_tile_window(map: &BootMemoryMap, ram_base: u64, ram_size: u64) {
        let regions = map.regions();
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
    fn further_windows_are_wholly_usable() {
        // A Pi 4 (8 GiB) shape: the kernel window below the MMIO hole plus
        // two further windows (1 GiB..~4 GiB and above 4 GiB). Every
        // non-kernel window arrives as one whole usable region.
        let windows = [
            (0x0u64, 0x3B40_0000u64),
            (0x4000_0000, 0x8000_0000),
            (0x1_0000_0000, 0x1_0000_0000),
        ];
        let kernel_end = 0x40_0000; // 4 MiB image in window 0
        let map = build_memory_map(&windows, kernel_end).expect("well-formed");

        let total: u64 = windows.iter().map(|&(_, size)| size).sum();

        // The further windows are present, whole, and usable.
        let regions = map.regions();
        assert!(regions.iter().any(|r| r.kind == RegionKind::Usable
            && r.start.as_u64() == 0x4000_0000
            && r.length == 0x8000_0000));
        assert!(regions.iter().any(|r| r.kind == RegionKind::Usable
            && r.start.as_u64() == 0x1_0000_0000
            && r.length == 0x1_0000_0000));

        let (usable, reserved) = region_byte_totals(&map);
        assert_eq!(usable + reserved, total, "no byte of any window is lost");
    }

    #[test]
    fn a_zero_length_window_contributes_no_region() {
        let windows = [(VIRT_RAM_BASE, 0x4000_0000u64), (0x2_0000_0000, 0)];
        let kernel_end = VIRT_RAM_BASE + TWO_MIB;
        let map = build_memory_map(&windows, kernel_end).expect("well-formed");
        assert!(map
            .regions()
            .iter()
            .all(|r| r.start.as_u64() != 0x2_0000_0000));
    }

    #[test]
    fn window_list_without_the_kernel_is_rejected() {
        // The kernel image lies in none of the windows: no window can
        // bound the reserve, so the build fails closed.
        assert_eq!(
            build_memory_map(&[(0x1_0000_0000, 0x4000_0000)], VIRT_RAM_BASE + TWO_MIB).unwrap_err(),
            MemoryMapError::UsableRegionEmpty,
        );
        // As does an empty discovery.
        assert_eq!(
            build_memory_map(&[], VIRT_RAM_BASE + TWO_MIB).unwrap_err(),
            MemoryMapError::UsableRegionEmpty,
        );
    }

    #[test]
    fn reserve_blob_frames_carves_the_dtb_out_of_usable_ram() {
        // A DTB landed high in the usable window (the QEMU `virt` / Pi
        // shape): its whole frames must leave the usable span and become a
        // reserved gap, so neither the allocator nor the RAM self-test can
        // touch the live tree. Unaligned base and length exercise the
        // outward rounding.
        let ram_size = 0x4000_0000u64; // 1 GiB
        let kernel_end = VIRT_RAM_BASE + TWO_MIB;
        let mut map =
            build_memory_map(&[(VIRT_RAM_BASE, ram_size)], kernel_end).expect("well-formed");
        let (usable_before, _) = region_byte_totals(&map);

        let dtb_base = VIRT_RAM_BASE + 0x2000_0123; // mid-page
        let dtb_len = 0x1_500u64; // spills across a page once rounded outward
        super::reserve_blob_frames(&mut map, dtb_base, dtb_len);

        let page = PAGE_SIZE as u64;
        let frame_start = dtb_base & !(page - 1);
        let frame_end = (dtb_base + dtb_len + page - 1) & !(page - 1);
        let reserved = frame_end - frame_start;

        let (usable_after, _) = region_byte_totals(&map);
        assert_eq!(
            usable_before - usable_after,
            reserved,
            "exactly the DTB's whole frames leave the usable total"
        );
        // No usable region overlaps the reserved DTB frames.
        for r in map.regions() {
            if r.kind != RegionKind::Usable {
                continue;
            }
            let (rs, re) = (r.start.as_u64(), r.start.as_u64() + r.length);
            assert!(
                re <= frame_start || rs >= frame_end,
                "a usable region still overlaps the DTB frames"
            );
        }
    }

    #[test]
    fn reserve_blob_frames_is_a_noop_for_zero_len_or_overflow() {
        let mut map = build_memory_map(&[(VIRT_RAM_BASE, 0x4000_0000)], VIRT_RAM_BASE + TWO_MIB)
            .expect("well-formed");
        let before = map.regions().len();
        // A zero-length blob reserves nothing.
        super::reserve_blob_frames(&mut map, VIRT_RAM_BASE + 0x1000_0000, 0);
        // A `base + len` that overflows `u64` reserves nothing (fail closed),
        // never wrapping into an unrelated region.
        super::reserve_blob_frames(&mut map, u64::MAX - 8, 64);
        assert_eq!(map.regions().len(), before);
    }

    #[test]
    fn clip_drops_ram_inside_device_typed_gigapages() {
        // Pi 4 shape: gigapage 3 holds the UART/GIC/PCIe, so the below-hole
        // window's bytes inside it are clipped; the windows outside stay
        // whole. Device mask bit 3 set.
        let device_mask = [1u64 << 3];
        let windows = [
            (0x0u64, 0x3B40_0000u64),       // gigapage 0 only
            (0x4000_0000, 0xBC00_0000),     // gigapages 1..=3
            (0x1_0000_0000, 0x1_0000_0000), // gigapages 4..=7
        ];
        let clipped = super::clip_windows_to_normal_ram(&windows, &device_mask);
        assert_eq!(
            clipped,
            alloc::vec![
                (0x0, 0x3B40_0000),
                // The middle window loses exactly its gigapage-3 tail.
                (0x4000_0000, 0x8000_0000),
                (0x1_0000_0000, 0x1_0000_0000),
            ]
        );
    }

    #[test]
    fn clip_splits_a_window_around_an_interior_device_gigapage() {
        // A window spanning gigapages 0..=2 with gigapage 1 Device-typed
        // splits into its two Normal-typed halves.
        let device_mask = [1u64 << 1];
        let windows = [(0x0u64, 3u64 << 30)];
        let clipped = super::clip_windows_to_normal_ram(&windows, &device_mask);
        assert_eq!(clipped, alloc::vec![(0x0, 1 << 30), (2 << 30, 1 << 30)]);
    }

    #[test]
    fn kernel_end_below_ram_base_is_rejected() {
        // A kernel end that precedes the RAM window cannot bound a
        // usable region: fail closed rather than emit a wrapped length.
        assert_eq!(
            build_memory_map(&[(VIRT_RAM_BASE, 0x4000_0000)], VIRT_RAM_BASE - 0x1000).unwrap_err(),
            MemoryMapError::UsableRegionEmpty,
        );
    }

    #[test]
    fn kernel_end_at_or_past_ram_end_is_rejected() {
        let ram_size = 0x10_0000; // 1 MiB
                                  // Kernel image fills the whole window: no usable frames remain.
        assert_eq!(
            build_memory_map(&[(VIRT_RAM_BASE, ram_size)], VIRT_RAM_BASE + ram_size).unwrap_err(),
            MemoryMapError::UsableRegionEmpty,
        );
        // And strictly past the end is equally refused.
        assert_eq!(
            build_memory_map(
                &[(VIRT_RAM_BASE, ram_size)],
                VIRT_RAM_BASE + ram_size + 0x4000
            )
            .unwrap_err(),
            MemoryMapError::UsableRegionEmpty,
        );
    }

    #[test]
    fn ram_window_overflow_is_rejected() {
        assert_eq!(
            build_memory_map(&[(u64::MAX - 0x10, 0x100)], u64::MAX - 0x10).unwrap_err(),
            MemoryMapError::AddressOverflow,
        );
    }

    #[test]
    fn kernel_end_alignment_overflow_is_rejected() {
        // A kernel end within a page of u64::MAX cannot be rounded up to
        // a frame boundary without overflowing.
        assert_eq!(
            build_memory_map(&[(VIRT_RAM_BASE, 0x4000_0000)], u64::MAX - 1).unwrap_err(),
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

    use tairix_kernel_mem::{BootMemoryMap, MemoryRegion, PhysAddr};

    /// Total bytes of [`RegionKind::Usable`] the map covers.
    fn usable_bytes(map: &BootMemoryMap) -> u64 {
        map.regions()
            .iter()
            .filter(|r| r.kind == RegionKind::Usable)
            .map(|r| r.length)
            .sum()
    }

    /// A single usable region spanning `[start, start + len)`.
    fn usable(start: u64, len: u64) -> MemoryRegion {
        MemoryRegion {
            kind: RegionKind::Usable,
            start: PhysAddr::new(start),
            length: len,
        }
    }

    /// The shape a PC firmware map has once the guest has more RAM than the
    /// boot trampoline's own window: the legacy low window, the main block
    /// below the PCI hole, the reserved hole itself, and the remainder
    /// re-based above 4 GiB.
    fn pc_map_with_high_ram() -> BootMemoryMap {
        let mut map = BootMemoryMap::new();
        map.push(usable(0, 0x9_FC00));
        map.push(usable(0x10_0000, 0xBFF0_0000 - 0x10_0000));
        map.push(MemoryRegion {
            start: PhysAddr::new(0xBFF0_0000),
            length: 0x1_0000_0000 - 0xBFF0_0000,
            kind: RegionKind::Reserved,
        });
        map.push(usable(0x1_0000_0000, 0x4010_0000));
        map
    }

    #[test]
    fn identity_window_covers_the_top_of_usable_ram() {
        // RAM ends at 5 GiB + 1 MiB, so the window must reach the whole 6th
        // gigabyte or the frames at the top of the pool are unreachable.
        assert_eq!(identity_window_gib(&pc_map_with_high_ram(), 4, 64), 6);
    }

    #[test]
    fn identity_window_ignores_reserved_regions_above_ram() {
        // A firmware map spans its reserved MMIO hole past the installed
        // RAM; sizing from the highest *address* would map gigabytes of it.
        let mut map = BootMemoryMap::new();
        map.push(usable(0, 0x2000_0000));
        map.push(MemoryRegion {
            start: PhysAddr::new(0xF000_0000),
            length: 0x1000_0000,
            kind: RegionKind::Reserved,
        });
        assert_eq!(identity_window_gib(&map, 4, 64), 4, "floor, not the hole");
    }

    #[test]
    fn identity_window_clamps_to_the_user_virtual_base() {
        // RAM above the cap shares the window's virtual range with the child
        // image, so the window stops short of it and those frames fail closed.
        let mut map = BootMemoryMap::new();
        map.push(usable(0, 128u64 << 30));
        assert_eq!(identity_window_gib(&map, 4, 64), 64);
    }

    #[test]
    fn frame_carve_takes_the_top_of_usable_ram_below_the_bound() {
        // Bottom-up would land in the legacy sub-640-KiB window, over the
        // firmware structures and the AP start-up trampoline at 0x8000.
        let mut map = pc_map_with_high_ram();
        let before = usable_bytes(&map);
        let base = carve_frames_from_map(&mut map, 6, 4 << 30).expect("6 frames fit");
        assert_eq!(base, 0xBFF0_0000 - 6 * PAGE_SIZE as u64);
        assert_eq!(base % PAGE_SIZE as u64, 0);
        assert_eq!(usable_bytes(&map), before - 6 * PAGE_SIZE as u64);
    }

    #[test]
    fn frame_carve_stays_below_the_writable_window() {
        // Only RAM above the caller's window is free: the carve refuses
        // rather than handing back frames it could not write through.
        let mut map = BootMemoryMap::new();
        map.push(usable(0x1_0000_0000, 0x4000_0000));
        let before = map.regions().len();
        assert!(carve_frames_from_map(&mut map, 4, 4 << 30).is_none());
        assert_eq!(map.regions().len(), before, "map untouched on no carve");
    }

    #[test]
    fn frame_carve_reserves_against_a_second_carve() {
        // The reservation is what keeps the frame allocator — and any later
        // carve — off the live page directories.
        let mut map = BootMemoryMap::new();
        map.push(usable(0, 0x10_0000));
        let first = carve_frames_from_map(&mut map, 2, 4 << 30).expect("2 frames fit");
        let second = carve_frames_from_map(&mut map, 2, 4 << 30).expect("2 more frames fit");
        assert_eq!(first, 0x10_0000 - 2 * PAGE_SIZE as u64);
        assert_eq!(second, first - 2 * PAGE_SIZE as u64);
    }
}
