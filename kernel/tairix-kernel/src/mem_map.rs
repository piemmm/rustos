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
//! dead code. The single-window [`build_memory_map`]
//! (aarch64), the map-carve [`carve_guard_arena_from_map`] (x86_64 +
//! riscv64), and the identity-window sizing and page-directory carve
//! [`identity_window_gib`] / [`carve_frames_from_map`] (x86_64) are each
//! gated to the port(s) that use them; the arena-sizing policy is shared.

use tairix_kernel_mem::{BootMemoryMap, PhysAddr, RegionKind};
// `MemoryRegion` and `PAGE_SIZE` are named only by the aarch64 single-window
// `build_memory_map` and the host tests; the x86_64 carve reads regions
// through `BootMemoryMap` without naming the element type, so the import is
// gated to those configurations to stay free of an unused-import warning.
#[cfg(any(all(freestanding, kernel_isa = "aarch64"), test))]
use tairix_kernel_mem::{MemoryRegion, PAGE_SIZE};

/// Alignment of the kthread-stack guard arena: one L2 block (2 MiB).
///
/// Laying the arena out on a 2 MiB boundary means each of its guard pages
/// becomes its own L3 leaf after the boot path re-expresses the covering
/// block at 4 KiB granularity
/// ([`tairix_arch_aarch64::paging::AddressSpace::prepare_guard_arena`]), so
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
/// A cap on the "fraction of discovered RAM" policy so a very large
/// server does not reserve an unbounded slab up front for kthread stacks it
/// will never all use at once. 64 MiB holds well over a thousand guarded
/// stacks (a workable headroom for both desktop and
/// server without waste). Growth past this on genuine exhaustion is the
/// staged follow-on (the growable/chained arena, `plans/PI.md`/PLAN L3b).
/// `pub(crate)`: the aarch64 boot pool sizes itself for the worst-case
/// re-expression of exactly this ceiling
/// (`tairix_arch_aarch64::paging::guard_arena_pool_capacity`).
pub(crate) const STACK_ARENA_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Headroom policy: reserve roughly 1/64 of the discovered RAM window for
/// kthread kernel stacks (a capacity derived from
/// discovered hardware, never a hand-picked literal that caps a large
/// machine or wastes a small one).
const STACK_ARENA_RAM_SHIFT: u32 = 6;

/// Size the reserved kthread-stack guard arena from the discovered RAM
/// window `ram_size`, per the default policy.
///
/// The target is a fixed fraction of RAM ([`STACK_ARENA_RAM_SHIFT`]),
/// clamped to `[STACK_ARENA_MIN_BYTES, STACK_ARENA_MAX_BYTES]` and rounded
/// **down** to a whole [`GUARD_ARENA_ALIGN`] (2 MiB) block so every guard
/// page in the arena still lands on its own L3 leaf after
/// [`tairix_arch_aarch64::paging::AddressSpace::prepare_guard_arena`]. The
/// result is therefore always a non-zero multiple of 2 MiB. This is a
/// *policy* (a function of discovered hardware), not a frozen scalar, so a
/// 64 MiB embedded board and a 256 GiB server each get a workable arena from
/// the same code. Whether that arena actually fits the
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
/// Each variant is a fail-closed refusal: the boot
/// path records the cause in its audit line and parks rather than
/// handing the allocator a map it cannot trust.
///
/// Produced only by the aarch64 single-window [`build_memory_map`]; the
/// x86_64 path carves its arena out of an already-built firmware map
/// ([`carve_guard_arena_from_map`]) and never reaches these.
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
/// The aarch64 [`build_memory_map`] returns both so the boot path hands the
/// allocator the [`map`](Self::map) and fine-maps the [`arena`](Self::arena)
/// (when one fits) through the page-table block-split. The x86_64 path keeps
/// its map and arena as separate values, so this pairing is aarch64-only.
#[cfg(any(all(freestanding, kernel_isa = "aarch64"), test))]
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
/// `arena_bytes` is the policy size from [`stack_arena_bytes`] (a
/// whole multiple of [`GUARD_ARENA_ALIGN`]). The arena is placed at the
/// first 2 MiB boundary at or after `usable_start` (above the kernel image,
/// so it never overlaps the running code or boot stack). Returns `None` if
/// the whole `arena_bytes` block does not fit before `ram_end`, so a tiny
/// RAM window degrades to no arena rather than a wrapped or overlapping
/// region.
///
/// This is the aarch64 single-window carve consumed by [`build_memory_map`];
/// the x86_64 boot path carves out of a firmware region list instead
/// ([`carve_guard_arena_from_map`]).
#[cfg(any(all(freestanding, kernel_isa = "aarch64"), test))]
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
/// The returned [`MemoryLayout`] pairs the allocator map with the carved
/// [`GuardArena`] (when one fits the kernel's window). The kernel window's
/// regions, in physical order, are: the [`RegionKind::Reserved`] kernel
/// image, an optional [`RegionKind::Usable`] head below the arena, the
/// [`RegionKind::Reserved`] guard arena, and the [`RegionKind::Usable`]
/// remainder; every other window contributes one whole
/// [`RegionKind::Usable`] region (a machine like the Pi 4 describes RAM
/// above its MMIO hole and above 4 GiB as further windows). Zero-length
/// windows and usable spans are omitted so no degenerate region reaches
/// the allocator. The arena policy is sized from the *total* discovered
/// RAM. The arena's frames are
/// reserved so the allocator never hands them out; the boot path
/// re-expresses the arena at 4 KiB granularity so a guard page in it can
/// later be unmapped (`plans/PI.md` stage G2/G3).
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
) -> Result<MemoryLayout, MemoryMapError> {
    let usable_start =
        align_up(kernel_end, PAGE_SIZE as u64).ok_or(MemoryMapError::AddressOverflow)?;

    // Total discovered RAM sizes the arena policy; overflow of the sum is
    // a malformed discovery and fails closed.
    let mut total_ram: u64 = 0;
    for &(base, size) in windows {
        base.checked_add(size)
            .ok_or(MemoryMapError::AddressOverflow)?;
        total_ram = total_ram
            .checked_add(size)
            .ok_or(MemoryMapError::AddressOverflow)?;
    }

    // The kernel image must sit strictly inside exactly one window; that
    // window carries the reserve + arena layout.
    let kernel_window = windows
        .iter()
        .copied()
        .find(|&(base, size)| usable_start >= base && usable_start < base + size)
        .ok_or(MemoryMapError::UsableRegionEmpty)?;
    let (ram_base, ram_size) = kernel_window;
    let ram_end = ram_base + ram_size;

    let arena = carve_guard_arena(usable_start, ram_end, stack_arena_bytes(total_ram));

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
            if usable_start < ram_end {
                map.push(MemoryRegion {
                    kind: RegionKind::Usable,
                    start: PhysAddr::new(usable_start),
                    length: ram_end - usable_start,
                });
            }
        }
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

    Ok(MemoryLayout { map, arena })
}

/// Total bytes the map covers of each [`RegionKind`], in `(usable,
/// reserved)` order. Used by the aarch64 boot path to record the discovered
/// split in its audit line (and by the host tests); the x86_64 boot path
/// logs its guard-arena decision directly, so this is gated to the
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
/// ([`carve_guard_arena_from_map`]) — the same reserve-then-hand-back
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

/// Carve a 2 MiB-aligned kthread-stack guard arena out of an
/// already-built firmware [`BootMemoryMap`] and reserve it.
///
/// This is the x86_64 + riscv64 counterpart of the aarch64 single-window
/// [`build_memory_map`]: that port discovers one `/memory` window and lays
/// the whole map out itself, whereas x86_64 receives a multi-region
/// firmware map (`x86_64::boot::build_memory_map`) — and riscv64 builds its own
/// two-region map (`riscv64::boot::build_boot_memory_map`) — and each only
/// needs to carve the arena out of it. The arena is sized by the same
/// policy ([`stack_arena_bytes`], a whole multiple of [`GUARD_ARENA_ALIGN`])
/// from `ram_bytes` — the discovered RAM the caller passes (both boot paths
/// sum the `Usable` regions, *not* the highest address, since a PC firmware
/// map spans the reserved MMIO hole to 4 GiB and beyond) — so a 256 MiB box
/// and a 256 GiB server each get a workable arena from one code path.
///
/// The arena is placed at the first 2 MiB boundary inside the first
/// [`RegionKind::Usable`] region that can host the whole policy-sized block
/// at or below `max_addr` (exclusive). `max_addr` is the spawn seams'
/// identity-window limit: a stack outside it could not be reached — nor its
/// guard page faulted — under the task's own root, so the
/// carve refuses to place the arena there. On success the range is removed
/// from the usable map ([`BootMemoryMap::reserve_range`]) so the frame
/// allocator never hands those frames out, and the [`GuardArena`] is
/// returned for [`crate::stack_arena::StackArena::install`].
///
/// Returns `None` (the boot path then leaves the kthread-stack guard in its
/// software-canary form — fail closed, never fatal) when no usable region can host a whole 2 MiB-aligned arena below
/// `max_addr`.
#[cfg(any(
    all(freestanding, any(kernel_isa = "x86_64", kernel_isa = "riscv64")),
    test
))]
pub(crate) fn carve_guard_arena_from_map(
    map: &mut BootMemoryMap,
    ram_bytes: u64,
    max_addr: u64,
) -> Option<GuardArena> {
    let arena_bytes = stack_arena_bytes(ram_bytes);
    let mut chosen: Option<u64> = None;
    for region in map.regions() {
        if region.kind != RegionKind::Usable {
            continue;
        }
        let region_start = region.start.as_u64();
        let Some(region_end) = region_start.checked_add(region.length) else {
            continue;
        };
        let Some(base) = align_up(region_start, GUARD_ARENA_ALIGN) else {
            continue;
        };
        let Some(end) = base.checked_add(arena_bytes) else {
            continue;
        };
        // The whole arena must fit inside this usable region *and* below the
        // identity-window limit, or the stack it hosts could not be reached
        // (its guard page faulted) under the task's own root.
        if end <= region_end && end <= max_addr {
            chosen = Some(base);
            break;
        }
    }
    let base = chosen?;
    map.reserve_range(PhysAddr::new(base), PhysAddr::new(base + arena_bytes));
    Some(GuardArena {
        base,
        len: arena_bytes,
    })
}

/// Record the boot path's kthread guard-arena decision on every boot so the
/// guard posture is audited, not silently trusted.
///
/// A carved+installed arena logs at Info with its base/len; a fall-back to
/// the software canary logs at Warn so a machine that could not host an
/// arena is visible in the boot record. `id` is the per-port boot-audit
/// event id (each boot module's `KERNEL_BOOT_GUARD_ARENA`); the body is
/// shared so the x86_64 and riscv64 boot paths emit one record shape.
#[cfg(all(freestanding, any(kernel_isa = "x86_64", kernel_isa = "riscv64")))]
pub(crate) fn log_guard_arena(
    sink: &(dyn tairix_log::Sink + Sync),
    id: tairix_log::EventId,
    arena: Option<(u64, u64)>,
) {
    use tairix_log::{Event, Field, Level};
    use tairix_util::fmt::format_hex_u64;

    let mut base_buf = [0u8; 16];
    let mut len_buf = [0u8; 16];
    let (base, len) = arena.unwrap_or((0, 0));
    let base_hex = format_hex_u64(base, &mut base_buf);
    let len_hex = format_hex_u64(len, &mut len_buf);
    let (level, message) = if arena.is_some() {
        (Level::Info, "kthread guard arena installed")
    } else {
        (
            Level::Warn,
            "no kthread guard arena; software-canary stacks used",
        )
    };
    tairix_log::log(
        sink,
        &Event {
            level,
            id,
            message,
            fields: &[
                Field {
                    key: "installed",
                    value: tairix_log::FieldValue::Str(if arena.is_some() {
                        "true"
                    } else {
                        "false"
                    }),
                },
                Field {
                    key: "base",
                    value: tairix_log::FieldValue::Str(base_hex),
                },
                Field {
                    key: "len",
                    value: tairix_log::FieldValue::Str(len_hex),
                },
            ],
        },
    );
}

#[cfg(test)]
mod tests {
    use super::{
        build_memory_map, carve_frames_from_map, identity_window_gib, region_byte_totals,
        stack_arena_bytes, MemoryMapError, GUARD_ARENA_ALIGN, STACK_ARENA_MAX_BYTES,
        STACK_ARENA_MIN_BYTES,
    };
    use tairix_kernel_mem::{RegionKind, PAGE_SIZE};

    /// The QEMU `virt` board's RAM base (GiB 1).
    const VIRT_RAM_BASE: u64 = 0x4000_0000;
    /// 2 MiB block alignment, mirrored from the module.
    const TWO_MIB: u64 = 0x20_0000;

    #[test]
    fn kernel_then_head_then_reserved_arena_then_usable() {
        let ram_size = 0x4000_0000; // 1 GiB
        let kernel_end = VIRT_RAM_BASE + 0x10_0000; // 1 MiB image, already aligned
        let layout = build_memory_map(&[(VIRT_RAM_BASE, ram_size)], kernel_end)
            .expect("window is well-formed");
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
        let layout =
            build_memory_map(&[(VIRT_RAM_BASE, ram_size)], kernel_end).expect("well-formed");
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
        let layout =
            build_memory_map(&[(VIRT_RAM_BASE, ram_size)], kernel_end).expect("well-formed");

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
        let layout =
            build_memory_map(&[(VIRT_RAM_BASE, ram_size)], kernel_end).expect("well-formed");
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
        let layout =
            build_memory_map(&[(VIRT_RAM_BASE, ram_size)], kernel_end).expect("well-formed");
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
    fn further_windows_are_wholly_usable_and_size_the_arena_policy() {
        // A Pi 4 (8 GiB) shape: the kernel window below the MMIO hole plus
        // two further windows (1 GiB..~4 GiB and above 4 GiB). Every
        // non-kernel window arrives as one whole usable region and the
        // arena policy is sized from the *total* RAM, so an 8 GiB machine
        // reserves the full 64 MiB arena even though its kernel window is
        // under 1 GiB.
        let windows = [
            (0x0u64, 0x3B40_0000u64),
            (0x4000_0000, 0x8000_0000),
            (0x1_0000_0000, 0x1_0000_0000),
        ];
        let kernel_end = 0x40_0000; // 4 MiB image in window 0
        let layout = build_memory_map(&windows, kernel_end).expect("well-formed");

        let total: u64 = windows.iter().map(|&(_, size)| size).sum();
        let arena = layout.arena.expect("an arena fits window 0");
        assert_eq!(arena.len, stack_arena_bytes(total));
        assert!(
            arena.base + arena.len <= 0x3B40_0000,
            "arena stays in the kernel window"
        );

        // The further windows are present, whole, and usable.
        let regions = layout.map.regions();
        assert!(regions.iter().any(|r| r.kind == RegionKind::Usable
            && r.start.as_u64() == 0x4000_0000
            && r.length == 0x8000_0000));
        assert!(regions.iter().any(|r| r.kind == RegionKind::Usable
            && r.start.as_u64() == 0x1_0000_0000
            && r.length == 0x1_0000_0000));

        let (usable, reserved) = region_byte_totals(&layout.map);
        assert_eq!(usable + reserved, total, "no byte of any window is lost");
    }

    #[test]
    fn a_zero_length_window_contributes_no_region() {
        let windows = [(VIRT_RAM_BASE, 0x4000_0000u64), (0x2_0000_0000, 0)];
        let kernel_end = VIRT_RAM_BASE + TWO_MIB;
        let layout = build_memory_map(&windows, kernel_end).expect("well-formed");
        assert!(layout
            .map
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
        let mut layout =
            build_memory_map(&[(VIRT_RAM_BASE, ram_size)], kernel_end).expect("well-formed");
        let (usable_before, _) = region_byte_totals(&layout.map);

        let dtb_base = VIRT_RAM_BASE + 0x2000_0123; // mid-page
        let dtb_len = 0x1_500u64; // spills across a page once rounded outward
        super::reserve_blob_frames(&mut layout.map, dtb_base, dtb_len);

        let page = PAGE_SIZE as u64;
        let frame_start = dtb_base & !(page - 1);
        let frame_end = (dtb_base + dtb_len + page - 1) & !(page - 1);
        let reserved = frame_end - frame_start;

        let (usable_after, _) = region_byte_totals(&layout.map);
        assert_eq!(
            usable_before - usable_after,
            reserved,
            "exactly the DTB's whole frames leave the usable total"
        );
        // No usable region overlaps the reserved DTB frames.
        for r in layout.map.regions() {
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
        let mut layout = build_memory_map(&[(VIRT_RAM_BASE, 0x4000_0000)], VIRT_RAM_BASE + TWO_MIB)
            .expect("well-formed");
        let before = layout.map.regions().len();
        // A zero-length blob reserves nothing.
        super::reserve_blob_frames(&mut layout.map, VIRT_RAM_BASE + 0x1000_0000, 0);
        // A `base + len` that overflows `u64` reserves nothing (fail closed),
        // never wrapping into an unrelated region.
        super::reserve_blob_frames(&mut layout.map, u64::MAX - 8, 64);
        assert_eq!(layout.map.regions().len(), before);
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

    #[test]
    fn arena_policy_floors_at_one_block_on_a_tiny_window() {
        // 64 MiB / 64 = 1 MiB, below the one-block floor, so a tiny machine
        // still gets a working arena rather than nothing (default
        // policy) — and never wastes more than a single 2 MiB block.
        assert_eq!(stack_arena_bytes(64 * 1024 * 1024), STACK_ARENA_MIN_BYTES);
        // Even a degenerate, sub-block window floors to one block (the
        // separate fit check in `carve_guard_arena` is what refuses it).
        assert_eq!(stack_arena_bytes(0), STACK_ARENA_MIN_BYTES);
    }

    #[test]
    fn arena_policy_scales_with_ram() {
        // 1 GiB / 64 = 16 MiB: a desktop gets many more stacks than the old
        // single fixed 2 MiB block, derived from discovered RAM.
        let one_gib = 0x4000_0000;
        assert_eq!(stack_arena_bytes(one_gib), 16 * 1024 * 1024);
        assert!(stack_arena_bytes(one_gib) > STACK_ARENA_MIN_BYTES);
    }

    #[test]
    fn arena_policy_caps_a_huge_window() {
        // 256 GiB / 64 = 4 GiB, well past the cap: a big server reserves the
        // bounded headroom, not an unbounded slab up front. Growth
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
        let layout =
            build_memory_map(&[(VIRT_RAM_BASE, ram_size)], kernel_end).expect("well-formed");
        let arena = layout.arena.expect("a large window carves an arena");
        assert_eq!(arena.len, STACK_ARENA_MAX_BYTES);
        assert!(
            arena.len > GUARD_ARENA_ALIGN,
            "more than one block reserved"
        );
        assert_regions_tile_window(&layout, VIRT_RAM_BASE, ram_size);
    }

    // --- `carve_guard_arena_from_map` (the x86_64 firmware-map carve) ----

    use super::carve_guard_arena_from_map;
    use tairix_kernel_mem::{BootMemoryMap, MemoryRegion, PhysAddr};

    /// A single usable region spanning `[start, start + len)`.
    fn usable(start: u64, len: u64) -> MemoryRegion {
        MemoryRegion {
            start: PhysAddr::new(start),
            length: len,
            kind: RegionKind::Usable,
        }
    }

    /// The total usable bytes the map still describes.
    fn usable_bytes(map: &BootMemoryMap) -> u64 {
        region_byte_totals(map).0
    }

    #[test]
    fn firmware_carve_reserves_a_policy_sized_block_out_of_usable_ram() {
        // A 256 MiB usable region (the QEMU `-m 256M` box): 256 MiB / 64 =
        // 4 MiB, a two-block arena, carved out and reserved.
        let ram = 256u64 * 1024 * 1024;
        let mut map = BootMemoryMap::new();
        map.push(usable(0, ram));
        let before = usable_bytes(&map);

        let arena =
            carve_guard_arena_from_map(&mut map, ram, 4 << 30).expect("256 MiB fits an arena");
        assert_eq!(arena.len, stack_arena_bytes(ram));
        assert_eq!(arena.base % GUARD_ARENA_ALIGN, 0, "arena is 2 MiB-aligned");

        // The arena's frames are gone from the usable map, and no usable
        // frame overlaps the reserved range.
        assert_eq!(usable_bytes(&map), before - arena.len);
        for region in map.regions() {
            if region.kind != RegionKind::Usable {
                continue;
            }
            let rs = region.start.as_u64();
            let re = rs + region.length;
            assert!(
                re <= arena.base || rs >= arena.base + arena.len,
                "no usable region overlaps the reserved arena",
            );
        }
    }

    #[test]
    fn firmware_carve_aligns_base_up_inside_an_unaligned_region() {
        // A usable region starting 1 MiB in: the arena base is rounded up to
        // the next 2 MiB boundary, never below the region start.
        let ram = 64u64 * 1024 * 1024;
        let mut map = BootMemoryMap::new();
        map.push(usable(0x10_0000, ram));
        let arena = carve_guard_arena_from_map(&mut map, ram, 4 << 30).expect("fits");
        assert_eq!(arena.base % GUARD_ARENA_ALIGN, 0);
        assert!(arena.base >= 0x10_0000, "base stays inside the region");
    }

    #[test]
    fn firmware_carve_skips_a_region_too_small_and_uses_a_later_one() {
        // The first usable region cannot host even one 2 MiB-aligned block;
        // the carve walks on to the ample second region rather than failing.
        let ram = 64u64 * 1024 * 1024;
        let mut map = BootMemoryMap::new();
        map.push(usable(0x1000, 0x1000)); // a single 4 KiB sliver
        map.push(usable(0x40_0000, ram));
        let arena = carve_guard_arena_from_map(&mut map, ram, 4 << 30).expect("second region fits");
        assert!(arena.base >= 0x40_0000, "arena landed in the ample region");
    }

    #[test]
    fn firmware_carve_skips_reserved_regions() {
        // A reserved region big enough for an arena is never carved into;
        // only usable RAM hosts the arena.
        let ram = 64u64 * 1024 * 1024;
        let mut map = BootMemoryMap::new();
        map.push(MemoryRegion {
            start: PhysAddr::new(0),
            length: ram,
            kind: RegionKind::Reserved,
        });
        map.push(usable(0x400_0000, ram));
        let arena = carve_guard_arena_from_map(&mut map, ram, 8 << 30).expect("usable region fits");
        assert!(
            arena.base >= 0x400_0000,
            "arena avoided the reserved region"
        );
    }

    #[test]
    fn firmware_carve_refuses_a_region_above_the_identity_limit() {
        // The only usable region sits entirely above the identity window, so
        // a stack there could not be reached under the task's own root: fail
        // closed to no arena (the seam falls back to the software canary).
        let ram = 64u64 * 1024 * 1024;
        let mut map = BootMemoryMap::new();
        map.push(usable(8u64 << 30, ram));
        assert!(
            carve_guard_arena_from_map(&mut map, ram, 4 << 30).is_none(),
            "no arena above the 4 GiB identity limit",
        );
        // The map is left untouched on the fail-closed path.
        assert_eq!(usable_bytes(&map), ram);
    }

    #[test]
    fn firmware_carve_clamps_the_arena_end_to_the_identity_limit() {
        // A region that straddles the identity limit only yields an arena if
        // the *whole* block fits below it. Here the limit cuts through the
        // policy-sized block, so the carve refuses.
        let ram = 8u64 * 1024 * 1024 * 1024; // policy size = 64 MiB cap
        let limit = (4u64 << 30) + 0x10_0000; // 4 GiB + 1 MiB
        let mut map = BootMemoryMap::new();
        // One usable region from just below the limit; a 64 MiB arena would
        // run well past `limit`.
        map.push(usable(4u64 << 30, ram));
        assert!(
            carve_guard_arena_from_map(&mut map, ram, limit).is_none(),
            "the whole arena must fit below the identity limit",
        );
    }

    #[test]
    fn firmware_carve_serves_the_riscv64_virt_window() {
        // The riscv64 boot path's two-region map (`riscv64::boot`): the
        // kernel image reserved at the `virt` board's RAM base
        // (0x8000_0000, GiB 2) and the remainder usable. The carve places
        // the policy-sized arena just above the kernel image, wholly below
        // the spawn seams' 4 GiB identity window, and reserves it
        // (`plans/PI.md` riscv64 G3b-2).
        let ram_base = 0x8000_0000u64; // QEMU `virt` RAM base (GiB 2)
        let ram = 256u64 * 1024 * 1024; // the `-m 256M` vertical box
        let kernel_end = ram_base + 0x40_0000; // 4 MiB image + boot heap
        let mut map = BootMemoryMap::new();
        map.push(MemoryRegion {
            start: PhysAddr::new(ram_base),
            length: kernel_end - ram_base,
            kind: RegionKind::Reserved,
        });
        map.push(usable(kernel_end, ram_base + ram - kernel_end));
        let usable_before = usable_bytes(&map);

        let arena = carve_guard_arena_from_map(&mut map, usable_before, 4 << 30)
            .expect("a 256 MiB virt window hosts an arena");
        assert_eq!(arena.len, stack_arena_bytes(usable_before));
        assert_eq!(arena.base % GUARD_ARENA_ALIGN, 0, "arena is 2 MiB-aligned");
        assert!(arena.base >= kernel_end, "arena sits above the kernel");
        assert!(
            arena.base + arena.len <= 4 << 30,
            "arena fits the seams' 4 GiB identity window"
        );
        assert_eq!(usable_bytes(&map), usable_before - arena.len);
    }

    #[test]
    fn firmware_carve_none_leaves_the_map_unchanged() {
        // No usable region can host a whole 2 MiB-aligned arena: the map is
        // returned untouched and the boot path keeps the software canary.
        let ram = 1024u64 * 1024; // 1 MiB, below one 2 MiB block
        let mut map = BootMemoryMap::new();
        map.push(usable(0, ram));
        let before = map.regions().len();
        assert!(carve_guard_arena_from_map(&mut map, ram, 4 << 30).is_none());
        assert_eq!(map.regions().len(), before, "map untouched on no-arena");
        assert_eq!(usable_bytes(&map), ram);
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
