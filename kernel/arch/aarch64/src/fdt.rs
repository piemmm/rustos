//! aarch64 device-tree access.
//!
//! The flattened-device-tree parser itself is architecture-neutral and
//! lives once in [`tairix_fdt`] (no duplication); this
//! module re-exports it and layers the aarch64-specific *queries* the boot
//! path needs from the `virt` board's device tree:
//!
//! * the first `/memory` region (delegated to
//!   [`tairix_fdt::Fdt::first_memory_region`]);
//! * the `/psci` `method` — the conduit (`hvc`/`smc`) the kernel uses to
//!   call PSCI firmware for secondary-core bring-up (the prerequisite for
//!   aarch64 SMP, `plans/WIRING.md` Stage W6);
//! * the optional generic-timer `clock-frequency` counter-rate override
//!   the `/timer` node may carry (`plans/PI.md` P4).
//!
//! Devices reach [`tairix_abi::hwtree`] through the *generic* walk in
//! [`crate::platform`] — no per-device query is needed for a node to be
//! discovered and matched.
//!
//! It also hosts the one bus-aware `reg` decoder the port's tree readers
//! share: per-depth [`crate::fdt::BusLevel`]
//! cell/`ranges` tracking, the ancestor-bus [`crate::fdt::translate`]
//! step, the per-entry [`crate::fdt::translated_reg`] reader, and the
//! early-returning [`crate::fdt::scan_translated`] walk the boot-path
//! console/GIC discovery and the
//! [`crate::platform`] hardware-tree walk are built on. Real boards (the
//! Pi 4's `/soc`) put peripherals behind buses with their own
//! `#address-cells`/`#size-cells` and `ranges`, so a raw `reg` read is a
//! *bus* address — never the CPU-physical base a driver may touch.

use tairix_fdt::{read_cells, Node};

pub use tairix_fdt::{Fdt, FdtError};

/// Deepest device-tree nesting the shared walks track per-level state
/// for.
///
/// A validation bound on hostile input, not a device
/// capacity: real boards nest three or four levels deep, so sixteen is
/// generous, and a deeper tree ends the walk rather than reading state
/// the walker cannot track.
pub const MAX_WALK_DEPTH: usize = 16;

/// Per-depth walk state: what a node's children need to know about their
/// ancestor buses to decode and translate their own `reg`.
#[derive(Copy, Clone)]
pub struct BusLevel<'a> {
    /// `#address-cells` governing this node's children's `reg`.
    pub addr_cells: u32,
    /// `#size-cells` governing this node's children's `reg`.
    pub size_cells: u32,
    /// This node's raw `ranges` value, mapping its children's address
    /// space into its parent's (`None` when absent — untranslatable).
    pub ranges: Option<&'a [u8]>,
}

impl BusLevel<'_> {
    /// State before any node is visited: the devicetree-spec default cell
    /// counts (2 address, 1 size) with no `ranges`.
    pub const DEFAULT: Self = BusLevel {
        addr_cells: 2,
        size_cells: 1,
        ranges: None,
    };
}

/// Read `node`'s own bus-level facts — the cell counts and `ranges` that
/// govern its *children* — applying the devicetree-spec defaults where a
/// property is absent.
#[must_use]
pub fn bus_level<'a>(node: &Node<'a>) -> BusLevel<'a> {
    BusLevel {
        addr_cells: cells_property(node, "#address-cells").unwrap_or(2),
        size_cells: cells_property(node, "#size-cells").unwrap_or(1),
        ranges: node.property("ranges").map(|p| p.value()),
    }
}

/// Read a `#address-cells` / `#size-cells` style single-cell property.
fn cells_property(node: &Node<'_>, name: &str) -> Option<u32> {
    node.property(name)?.read_be_u32(0).ok()
}

/// Translate `addr` from the address space of the node at `depth` into a
/// CPU-physical address by applying each ancestor bus's `ranges`, child
/// to root.
///
/// An ancestor with an *empty* `ranges` is an identity mapping; an
/// ancestor with *no* `ranges` cannot translate (the devicetree spec
/// forbids crossing it), and an address no range entry covers is
/// likewise refused — `None`, never a guess. Nodes
/// directly under the root need no translation.
#[must_use]
pub fn translate(levels: &[BusLevel<'_>], depth: usize, addr: u64) -> Option<u64> {
    let mut translated = addr;
    for bus in (1..depth).rev() {
        let level = levels.get(bus)?;
        let ranges = level.ranges?;
        if ranges.is_empty() {
            continue;
        }
        translated = apply_ranges(
            ranges,
            RangeCells {
                child_address: level.addr_cells,
                parent_address: levels.get(bus - 1)?.addr_cells,
                child_size: level.size_cells,
            },
            translated,
        )?;
    }
    Some(translated)
}

/// The three cell counts decoding one `ranges` entry: the child bus's
/// address cells, the parent bus's address cells, and the child bus's
/// size cells (Devicetree Spec v0.4 §2.3.8).
#[derive(Copy, Clone)]
struct RangeCells {
    child_address: u32,
    parent_address: u32,
    child_size: u32,
}

/// Map `addr` through one `ranges` value: find the `(child, parent,
/// size)` entry containing it and rebase it into the parent space.
fn apply_ranges(ranges: &[u8], cells: RangeCells, addr: u64) -> Option<u64> {
    let RangeCells {
        child_address,
        parent_address,
        child_size,
    } = cells;
    if child_address == 0
        || child_address > 2
        || parent_address == 0
        || parent_address > 2
        || child_size == 0
        || child_size > 2
    {
        return None;
    }
    let entry = ((child_address + parent_address + child_size) * 4) as usize;
    if !ranges.len().is_multiple_of(entry) {
        return None;
    }
    let mut off = 0;
    while off + entry <= ranges.len() {
        let child_base = read_cells(ranges, off, child_address)?;
        let parent_base = read_cells(ranges, off + (child_address as usize) * 4, parent_address)?;
        let size = read_cells(
            ranges,
            off + ((child_address + parent_address) as usize) * 4,
            child_size,
        )?;
        if let Some(delta) = addr.checked_sub(child_base) {
            if delta < size {
                return parent_base.checked_add(delta);
            }
        }
        off += entry;
    }
    None
}

/// Decode a PCI host bridge's `dma-ranges` into the inbound DMA aperture
/// it grants devices behind it (Devicetree Spec v0.4 §2.3.9): the
/// CPU-physical window `[base, base + len)` a device on the bus may DMA
/// through, returned as `(top, len)` where `top = base + len` is the
/// *exclusive* upper bound — the same form [`crate::platform`] hands
/// `HwResource::dma` for the `VideoCore` mailbox carve, so a consumer
/// reads `top` as "every DMA address must lie below this".
///
/// `child_address` is the bridge's own `#address-cells` — `3` for a PCI
/// bus, the `phys.hi`/`phys.mid`/`phys.lo` triple. Only its *width* is
/// used to step over the child PCI address; that address is not part of
/// the CPU-physical aperture and is never read into a `u64` (so the
/// 3-cell width [`read_cells`] would reject is fine here). `parent_address`
/// is the parent bus's `#address-cells` and `child_size` the bridge's own
/// `#size-cells`; both must decode to a `u64` (`1..=2`).
///
/// Returns `(top, len, bus_base)` — never an invented aperture — where `top` is the *exclusive* upper bound of the
/// CPU-physical window a device behind the bridge may reach, `len` its
/// extent, and `bus_base` the bus/PCIe-space address the lowest entry's
/// viewport starts at (the inbound translation, the counterpart of
/// [`outbound_mmio_window`]'s `pcie_base`). For a PCI bus
/// (`child_address == 3`) `bus_base` is the low 64 bits
/// (`phys.mid`/`phys.lo`) of the lowest entry's child PCI triple;
/// otherwise it is `0`. Returns [`None`] when the node carries no
/// `dma-ranges`, a cell count is out of range, the value is not a whole
/// number of entries, or a `base + len` overflows. With multiple entries
/// the aperture spans the lowest base to the highest top.
#[must_use]
pub fn dma_ranges_aperture(
    node: &Node<'_>,
    child_address: u32,
    parent_address: u32,
    child_size: u32,
) -> Option<(u64, u64, u64)> {
    if child_address == 0
        || child_address > 3
        || parent_address == 0
        || parent_address > 2
        || child_size == 0
        || child_size > 2
    {
        return None;
    }
    let value = node.property("dma-ranges")?.value();
    let entry = ((child_address + parent_address + child_size) * 4) as usize;
    if value.is_empty() || value.len() % entry != 0 {
        return None;
    }
    let mut min_base: Option<u64> = None;
    let mut max_top: u64 = 0;
    // Bus/PCIe-space base of the entry with the lowest CPU base — the
    // inbound viewport's far-side start. A 3-cell PCI child carries it in
    // `phys.mid`/`phys.lo` (the two cells after `phys.hi`); a non-PCI
    // child has no translation, so it stays `0`.
    let mut bus_base_at_min: u64 = 0;
    let mut off = 0;
    while off + entry <= value.len() {
        let parent_base = read_cells(value, off + (child_address as usize) * 4, parent_address)?;
        let size = read_cells(
            value,
            off + ((child_address + parent_address) as usize) * 4,
            child_size,
        )?;
        let top = parent_base.checked_add(size)?;
        if min_base.is_none_or(|b| parent_base < b) {
            bus_base_at_min = if child_address == 3 {
                read_cells(value, off + 4, 2)?
            } else {
                0
            };
        }
        min_base = Some(min_base.map_or(parent_base, |b| b.min(parent_base)));
        max_top = max_top.max(top);
        off += entry;
    }
    let base = min_base?;
    let len = max_top.checked_sub(base)?;
    Some((max_top, len, bus_base_at_min))
}

/// Decode a PCI host bridge's outbound `ranges` memory window
/// (Devicetree Spec v0.4 §2.3.8): the CPU-physical aperture the bridge
/// forwards to PCIe memory space, returned as
/// `(cpu_base, pcie_base, size)` — the three values the VL805 wiring's
/// `PcieWindows` outbound fields need, and the
/// `(base, len, translated_base)` an `HwResource::bus_window` carries.
///
/// A PCI `ranges` entry is `<child-PCI-address> <parent-address> <size>`.
/// The child address is the 3-cell PCI triple `phys.hi`/`phys.mid`/
/// `phys.lo`: `phys.hi` bit 24..=25 is the space code (`0b10` = 32-bit
/// memory, `0b11` = 64-bit memory), and `phys.mid`/`phys.lo` are the
/// 64-bit PCIe-space base. The parent address is the CPU-physical base
/// (`parent_address` cells) and `size` the window length (`child_size`
/// cells). The first memory-space entry is returned; an I/O-space entry
/// (space code `0b01`) is skipped.
///
/// `child_address` must be `3` (a PCI bus); `parent_address` and
/// `child_size` must each decode to a `u64` (`1..=2`). Returns `None` —
/// never an invented window — when the node carries no
/// `ranges`, a cell count is out of range, the value is not a whole
/// number of entries, or no entry describes a memory window.
#[must_use]
pub fn outbound_mmio_window(
    node: &Node<'_>,
    child_address: u32,
    parent_address: u32,
    child_size: u32,
) -> Option<(u64, u64, u64)> {
    if child_address != 3
        || parent_address == 0
        || parent_address > 2
        || child_size == 0
        || child_size > 2
    {
        return None;
    }
    let value = node.property("ranges")?.value();
    let entry = ((child_address + parent_address + child_size) * 4) as usize;
    if value.is_empty() || value.len() % entry != 0 {
        return None;
    }
    let mut off = 0;
    while off + entry <= value.len() {
        // `phys.hi` is the first cell; its bits 24..=25 are the space
        // code. Only memory-space windows (`0b10`/`0b11`) are outbound
        // MMIO apertures; an I/O-space window is not what the xHCI BAR
        // lives in, so it is skipped rather than mis-mapped.
        let phys_hi = read_cells(value, off, 1)?;
        let space = (phys_hi >> 24) & 0b11;
        if space == 0b10 || space == 0b11 {
            // PCIe-space base is the low 64 bits of the PCI triple
            // (`phys.mid`/`phys.lo`), the two cells after `phys.hi`.
            let pcie_base = read_cells(value, off + 4, 2)?;
            let cpu_base = read_cells(value, off + (child_address as usize) * 4, parent_address)?;
            let size = read_cells(
                value,
                off + ((child_address + parent_address) as usize) * 4,
                child_size,
            )?;
            if size == 0 {
                return None;
            }
            return Some((cpu_base, pcie_base, size));
        }
        off += entry;
    }
    None
}

/// Decode `reg` entry `index` of the node at `depth` with its parent
/// bus's cell counts and translate the base through the ancestor buses'
/// `ranges`, yielding the CPU-physical `(base, length)` window.
///
/// Returns `None` — never an invented or untranslated window — when the node carries no `reg`, the parent's
/// cell counts are outside the decodable `1..=2` range, the value is not
/// a whole number of entries, `index` is past the last entry, or an
/// ancestor bus cannot translate the base.
#[must_use]
pub fn translated_reg(
    node: &Node<'_>,
    depth: usize,
    levels: &[BusLevel<'_>],
    index: usize,
) -> Option<(u64, u64)> {
    let reg = node.property("reg")?;
    let parent = levels.get(depth.checked_sub(1)?)?;
    let (ac, sc) = (parent.addr_cells, parent.size_cells);
    if ac == 0 || ac > 2 || sc == 0 || sc > 2 {
        return None;
    }
    let value = reg.value();
    let entry = ((ac + sc) * 4) as usize;
    if value.is_empty() || value.len() % entry != 0 {
        return None;
    }
    let off = index.checked_mul(entry)?;
    if off + entry > value.len() {
        return None;
    }
    let base = read_cells(value, off, ac)?;
    let len = read_cells(value, off + (ac as usize) * 4, sc)?;
    let base = translate(levels, depth, base)?;
    Some((base, len))
}

/// Number of whole `reg` entries the node at `depth` carries under its
/// parent bus's cell counts, so a caller can iterate [`translated_reg`]
/// over every window while still *skipping* the entries an ancestor
/// cannot translate (a skipped entry is dropped, never invented ).
///
/// Returns `None` when the node carries no `reg`, the parent's cell
/// counts are outside the decodable `1..=2` range, or the value is not
/// a whole number of entries.
#[must_use]
pub fn reg_entry_count(node: &Node<'_>, depth: usize, levels: &[BusLevel<'_>]) -> Option<usize> {
    let reg = node.property("reg")?;
    let parent = levels.get(depth.checked_sub(1)?)?;
    let (ac, sc) = (parent.addr_cells, parent.size_cells);
    if ac == 0 || ac > 2 || sc == 0 || sc > 2 {
        return None;
    }
    let value = reg.value();
    let entry = ((ac + sc) * 4) as usize;
    if value.is_empty() || value.len() % entry != 0 {
        return None;
    }
    Some(value.len() / entry)
}

/// Walk `fdt` in document order, tracking each ancestor bus's cell
/// counts and `ranges`, handing every non-root node to `visit` together
/// with the per-depth [`BusLevel`] state and the node's depth (so
/// `visit` can decode windows via [`translated_reg`]). The walk returns
/// at the first `Some` `visit` yields.
///
/// Like the [`psci_method`] / [`timer_clock_frequency`] queries, this is
/// an early-returning `Fdt::nodes` traversal that reads only the visited
/// nodes' own properties, so a caller that returns at its matched node
/// stays safe with the MMU off (`plans/PI.md` P4/P5 watch-out — a
/// whole-tree scan faults there once the compiler widens the byte
/// reads). A malformed token or a tree nested beyond [`MAX_WALK_DEPTH`]
/// ends the walk — fail closed, never a guess.
pub fn scan_translated<'a, T>(
    fdt: &Fdt<'a>,
    mut visit: impl FnMut(&Node<'a>, &[BusLevel<'a>], usize) -> Option<T>,
) -> Option<T> {
    let mut levels = [BusLevel::DEFAULT; MAX_WALK_DEPTH];
    for node in fdt.nodes() {
        let Ok(node) = node else { break };
        let depth = node.depth() as usize;
        if depth >= MAX_WALK_DEPTH {
            break;
        }
        // This node's own cell counts and `ranges` govern its *children*;
        // record them after the visit so the visited node itself decodes
        // against its ancestors only.
        let mut level = bus_level(&node);
        if depth == 0 {
            // The root has no parent bus to translate into.
            level.ranges = None;
            levels[0] = level;
            continue;
        }
        if let Some(found) = visit(&node, &levels, depth) {
            return Some(found);
        }
        levels[depth] = level;
    }
    None
}

/// The `compatible` string identifying a per-PE external-debug component
/// in the device tree (the Linux `arm,coresight-cpu-debug` binding).
///
/// A node bearing it exposes that CPU's memory-mapped external-debug
/// registers (`EDPCSR`, …) at its `reg` window and names the CPU it
/// belongs to through its `cpu` phandle — the two facts the cross-core PC
/// sampler ([`crate::coresight`]) needs to read a wedged core's PC.
pub const CORESIGHT_CPU_DEBUG_COMPATIBLE: &str = "arm,coresight-cpu-debug";

/// Discover each CPU's CoreSight external-debug component base from the
/// device tree, writing the CPU-physical base of the debug node bound to
/// dense CPU `i` into `out[i]`.
///
/// `cpu_affinities[i]` is the MPIDR affinity of dense `CpuId` `i` (the boot
/// path's ordered `/cpus` map); `out` must be the same length and is left
/// untouched — the caller's "no component" sentinel — for a CPU with no
/// described debug node. For every `arm,coresight-cpu-debug` node the walk
/// translates its `reg` base through the ancestor buses' `ranges`
/// ([`translated_reg`]) and resolves its `cpu` phandle to that CPU's
/// affinity (`cpu_affinity_for_phandle`), recording the base under the
/// matching dense id. A node whose base cannot be translated, whose phandle
/// names no known CPU, or whose CPU is not in `cpu_affinities` is skipped
/// (fail closed — never a fabricated base).
///
/// Alloc-free: it writes directly into `out` and resolves phandles with
/// nested tree lookups, so it runs in the freestanding kernel with no heap.
/// It walks the whole tree, so it must be called with the MMU on (the
/// early-returning discovery queries are the MMU-off ones).
pub fn debug_component_bases(fdt: &Fdt<'_>, cpu_affinities: &[u64], out: &mut [usize]) {
    let _: Option<()> = scan_translated(fdt, |node, levels, depth| {
        if node.is_compatible(CORESIGHT_CPU_DEBUG_COMPATIBLE) {
            if let Some((idx, phys)) =
                debug_node_dense_base(fdt, node, levels, depth, cpu_affinities)
            {
                if let Some(slot) = out.get_mut(idx) {
                    *slot = phys;
                }
            }
        }
        // Never early-return: visit every node so *every* CPU's component
        // is found, not just the first.
        None
    });
}

/// Resolve one `arm,coresight-cpu-debug` `node` to
/// `(dense_cpu_id, cpu_physical_base)`, or `None` when any step fails
/// closed (untranslatable base, missing/unknown `cpu` phandle, or a CPU
/// not present in `cpu_affinities`).
fn debug_node_dense_base(
    fdt: &Fdt<'_>,
    node: &Node<'_>,
    levels: &[BusLevel<'_>],
    depth: usize,
    cpu_affinities: &[u64],
) -> Option<(usize, usize)> {
    let (base, _len) = translated_reg(node, depth, levels, 0)?;
    let phandle = node.property("cpu")?.read_be_u32(0).ok()?;
    let affinity = cpu_affinity_for_phandle(fdt, phandle)?;
    let idx = cpu_affinities.iter().position(|&a| a == affinity)?;
    Some((idx, usize::try_from(base).ok()?))
}

/// Resolve a CPU-node phandle to that CPU's MPIDR affinity (its `reg`), or
/// `None` when no node carries the phandle or it has no readable `reg`.
///
/// Phandles are unique within a device tree, so the first node whose
/// `phandle` (or the legacy `linux,phandle`) matches is the referenced CPU;
/// its `reg` is the affinity, read in the `/cpus` `#address-cells` width
/// (one cell on the common binding, two where the upper affinity bits are
/// used — size cells are zero, so there is no window to translate).
fn cpu_affinity_for_phandle(fdt: &Fdt<'_>, phandle: u32) -> Option<u64> {
    fdt.nodes().flatten().find_map(|node| {
        let node_phandle = node
            .property("phandle")
            .or_else(|| node.property("linux,phandle"))?
            .read_be_u32(0)
            .ok()?;
        if node_phandle != phandle {
            return None;
        }
        let reg = node.property("reg")?.value();
        let cells = if reg.len() >= 8 { 2 } else { 1 };
        read_cells(reg, 0, cells)
    })
}

/// The PSCI conduit a platform uses to call firmware (
/// `plans/WIRING.md` W6).
///
/// Discovered from the `/psci` node's `method` property; selects the
/// instruction (`hvc` at EL2-hosted, `smc` at EL3-hosted) the secondary
/// bring-up path issues.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum PsciMethod {
    /// Hypervisor call — the firmware lives at EL2 (QEMU `virt` default).
    Hvc,
    /// Secure-monitor call — the firmware lives at EL3.
    Smc,
}

impl PsciMethod {
    /// Parse a `/psci` `method` property value (a NUL-terminated string).
    #[must_use]
    pub fn from_property(value: &[u8]) -> Option<Self> {
        // Trim a trailing NUL the device tree stores on string properties.
        let s = match value.iter().position(|&b| b == 0) {
            Some(nul) => &value[..nul],
            None => value,
        };
        match s {
            b"hvc" => Some(Self::Hvc),
            b"smc" => Some(Self::Smc),
            _ => None,
        }
    }
}

/// Read the PSCI conduit from the `/psci` node, or `None` if the tree
/// declares no PSCI node or an unrecognised method.
///
/// Matches the `/psci` node through the shared `Fdt::nodes` early-return
/// walk — the same byte-safe traversal [`crate::gic::configure_from_fdt`]
/// and [`timer_clock_frequency`] use — and reads
/// `method` from that node's own properties. It stops at the first
/// matching node and never scans the whole tree, so it stays safe with
/// the MMU off (the secondary-bring-up boot path discovers the conduit
/// before the page tables are live; a whole-tree `Fdt::property`/`walk`
/// scan faults there once the compiler widens the byte reads —
/// `plans/PI.md` P4/P5 watch-out).
#[must_use]
pub fn psci_method(fdt: &Fdt<'_>) -> Option<PsciMethod> {
    let node = fdt.nodes().flatten().find(node_is_psci)?;
    PsciMethod::from_property(node.property("method")?.value())
}

/// `true` iff `node` is the standard `/psci` node — matched by an
/// `arm,psci` `compatible` prefix (covering `arm,psci`, `arm,psci-0.2`,
/// `arm,psci-1.0`, …) so the conduit is read from the firmware's PSCI
/// node regardless of the exact revision it advertises.
fn node_is_psci(node: &tairix_fdt::Node<'_>) -> bool {
    node.property("compatible").is_some_and(|compatible| {
        compatible
            .iter_strings()
            .any(|s| s.starts_with(b"arm,psci"))
    })
}

/// Read the generic-timer counter frequency (Hz) the `/timer` node
/// declares through its `clock-frequency` property, if any.
///
/// `clock-frequency` is the standard `arm,armv?-timer` override the
/// device tree carries when firmware leaves `CNTFRQ_EL0` mis-programmed
/// (the Linux binding honours it ahead of the register). The canonical
/// source is still the `CNTFRQ_EL0` register (`kernel_arch::read_cntfrq`);
/// this surfaces only the optional tree override so the boot path can
/// prefer it. Returns `None` when the node omits the property or the
/// value is not a single big-endian `u32`.
#[must_use]
pub fn timer_clock_frequency(fdt: &Fdt<'_>) -> Option<u32> {
    // Match the generic-timer node by its `compatible` string and read
    // `clock-frequency` from that node's own properties, via the shared
    // `Fdt::nodes` walk — the same early-returning traversal
    // [`crate::gic::configure_from_fdt`] uses. This
    // stops at the first matching node and only ever reads that node's
    // properties, never scanning the whole tree.
    let node = fdt
        .nodes()
        .flatten()
        .find(|node| node.is_compatible("arm,armv8-timer"))?;
    let value = node.property("clock-frequency")?.value();
    let bytes = value.get(0..4)?;
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// Select the generic-timer frequency (Hz) to drive preemption with,
/// preferring the device-tree `clock-frequency` override
/// ([`timer_clock_frequency`]) when present and non-zero, otherwise the
/// `CNTFRQ_EL0` register value `cntfrq`.
///
/// A zero `clock-frequency` is treated as absent (a board that declares
/// `0` has not really told us a rate), so the register value is used
/// rather than a divide-by-zero timer interval (fail
/// closed). Pure and host-testable: the register read is the caller's
/// responsibility (`kernel_arch::timer_frequency_hz` composes the two on
/// the freestanding target).
#[must_use]
pub const fn effective_timer_hz(tree_clock_frequency: Option<u32>, cntfrq: u64) -> u64 {
    match tree_clock_frequency {
        Some(hz) if hz != 0 => hz as u64,
        _ => cntfrq,
    }
}

/// First cell of a GIC `interrupts` specifier: the interrupt *type*.
///
/// The Arm GIC device-tree binding (Linux
/// `Documentation/devicetree/bindings/interrupt-controller/arm,gic.yaml`)
/// encodes each interrupt as a three-cell tuple `<type number flags>`.
/// `type` is [`GIC_TYPE_SPI`] (shared peripheral) or [`GIC_TYPE_PPI`]
/// (private peripheral); any other value is not a GICv2 interrupt this
/// port understands and is refused (fail closed).
pub const GIC_TYPE_SPI: u32 = 0;

/// First cell of a GIC `interrupts` specifier naming a private-peripheral
/// interrupt (PPI). See [`GIC_TYPE_SPI`].
pub const GIC_TYPE_PPI: u32 = 1;

/// Lowest GICv2 INTID a device-tree PPI maps to.
///
/// PPIs occupy INTIDs `16..32` (the SGIs below them are software-only and
/// have no device-tree `interrupts` form), so a binding's PPI `number` is
/// offset by this base. The SPI base is [`crate::gic::MIN_SPI_INTID`] —
/// the one definition both this decoder and the GICv2 routing share.
pub const GIC_PPI_INTID_BASE: u32 = 16;

/// Map a GIC `interrupts` specifier `(kind, number)` pair to its global
/// GICv2 INTID.
///
/// `kind` is the first `interrupts` cell ([`GIC_TYPE_SPI`] /
/// [`GIC_TYPE_PPI`]) and `number` the second. An SPI is offset by
/// [`crate::gic::MIN_SPI_INTID`] (32) and a PPI by [`GIC_PPI_INTID_BASE`]
/// (16), matching the kernel Arm GIC binding. Returns `None` — never a
/// guessed line — when:
///
/// * `kind` is neither SPI nor PPI (e.g. a GICv3-only extended-SPI
///   binding this GICv2 port does not drive),
/// * the offset addition overflows, or
/// * the resulting INTID exceeds [`crate::gic::MAX_INTID`] (an SPI
///   `number` the controller cannot address).
#[must_use]
pub fn gic_intid_from_cells(kind: u32, number: u32) -> Option<u32> {
    let intid = match kind {
        GIC_TYPE_SPI => number.checked_add(crate::gic::MIN_SPI_INTID)?,
        GIC_TYPE_PPI => number.checked_add(GIC_PPI_INTID_BASE)?,
        _ => return None,
    };
    (intid <= crate::gic::MAX_INTID).then_some(intid)
}

/// Decode the first GIC interrupt the device `node`'s `interrupts`
/// property names into its global GICv2 INTID.
///
/// Reads the `<type number flags>` triple through the `lib/fdt` cell
/// reader ([`Node::property`] + `read_be_u32`) — never a raw byte poke
/// (: discovery uses the enumerable source only) — and
/// maps it through [`gic_intid_from_cells`]. Returns `None` when the node
/// has no `interrupts` property, the property is shorter than the two
/// cells the decode needs, or the specifier is not a GICv2 SPI/PPI this
/// port can route. The caller (the device-IRQ bring-up in
/// [`crate::platform`] / the kernel binary) then leaves the device
/// unbound rather than guessing a line.
///
/// Only the first interrupt of a multi-interrupt device is decoded; a
/// device that raises several lines is served as further specifiers are
/// needed (no speculative multi-line surface).
#[must_use]
pub fn gic_device_intid(node: &Node<'_>) -> Option<u32> {
    let interrupts = node.property("interrupts")?;
    let kind = interrupts.read_be_u32(0).ok()?;
    let number = interrupts.read_be_u32(4).ok()?;
    gic_intid_from_cells(kind, number)
}

#[cfg(test)]
mod tests {
    use super::{
        debug_component_bases, dma_ranges_aperture, effective_timer_hz, gic_device_intid,
        gic_intid_from_cells, outbound_mmio_window, psci_method, timer_clock_frequency, Fdt,
        PsciMethod, GIC_TYPE_PPI, GIC_TYPE_SPI,
    };
    use crate::gic::{MAX_INTID, MIN_SPI_INTID};
    use tairix_fdt::fixture::{raspi_like_arm, virt_like_arm, DtbBuilder};

    /// Build a tree with two CPUs and, optionally, a per-CPU
    /// `arm,coresight-cpu-debug` node under a `/soc` bus, so
    /// [`debug_component_bases`] discovery is exercised end to end. Each
    /// entry of `debug` is `(cpu_phandle, base)`; a `soc` bus (address/size
    /// cells 2, identity `ranges`) holds the debug nodes so their `reg` is
    /// translated like a real board's peripheral. CPU affinities are `0` and
    /// `1` with phandles `1` and `2`.
    fn tree_with_debug_nodes(debug: &[(u32, u64)]) -> std::vec::Vec<u8> {
        let mut b = DtbBuilder::new();
        b.begin_node("");
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 2);

        b.begin_node("cpus");
        b.prop_u32("#address-cells", 1);
        b.prop_u32("#size-cells", 0);
        for (i, phandle) in [1u32, 2u32].into_iter().enumerate() {
            b.begin_node(if i == 0 { "cpu@0" } else { "cpu@1" });
            b.prop_str("device_type", "cpu");
            b.prop("reg", &u32::try_from(i).unwrap().to_be_bytes());
            b.prop_u32("phandle", phandle);
            b.end_node();
        }
        b.end_node(); // cpus

        b.begin_node("soc");
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 2);
        b.prop("ranges", &[]); // identity: child == parent address space
        for (n, &(cpu_phandle, base)) in debug.iter().enumerate() {
            b.begin_node(if n == 0 { "debug@a" } else { "debug@b" });
            b.prop_str("compatible", "arm,coresight-cpu-debug");
            let mut reg = std::vec::Vec::new();
            reg.extend_from_slice(&base.to_be_bytes()); // 2 address cells
            reg.extend_from_slice(&0x1000u64.to_be_bytes()); // 2 size cells
            b.prop("reg", &reg);
            b.prop_u32("cpu", cpu_phandle);
            b.end_node();
        }
        b.end_node(); // soc

        b.end_node(); // root
        b.build()
    }

    #[test]
    fn discovers_a_per_cpu_debug_base_bound_to_its_cpu() {
        // cpu0 (phandle 1) → debug base A, cpu1 (phandle 2) → debug base B.
        let blob = tree_with_debug_nodes(&[(1, 0xf651_0000), (2, 0xf653_0000)]);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let mut out = [0usize; 2];
        debug_component_bases(&fdt, &[0, 1], &mut out);
        assert_eq!(out, [0xf651_0000, 0xf653_0000]);
    }

    #[test]
    fn a_cpu_without_a_debug_node_keeps_its_sentinel() {
        // Only cpu1 (phandle 2) has a debug node; cpu0's slot is left as the
        // caller's "no component" sentinel (0), never fabricated.
        let blob = tree_with_debug_nodes(&[(2, 0xf653_0000)]);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let mut out = [0usize; 2];
        debug_component_bases(&fdt, &[0, 1], &mut out);
        assert_eq!(out, [0, 0xf653_0000]);
    }

    #[test]
    fn a_debug_node_for_an_unknown_cpu_is_skipped() {
        // A debug node whose `cpu` phandle names no known CPU (3) writes
        // nothing — fail closed, never a wrong-CPU attribution.
        let blob = tree_with_debug_nodes(&[(3, 0xf651_0000)]);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let mut out = [0usize; 2];
        debug_component_bases(&fdt, &[0, 1], &mut out);
        assert_eq!(out, [0, 0]);
    }

    #[test]
    fn a_tree_with_no_debug_nodes_installs_nothing() {
        // The QEMU `virt` / stock Pi 4 shape: no debug components described,
        // so discovery leaves every slot at the sentinel (the sampler then
        // reports Unsupported and the buddy detector runs unchanged).
        let blob = tree_with_debug_nodes(&[]);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let mut out = [0usize; 2];
        debug_component_bases(&fdt, &[0, 1], &mut out);
        assert_eq!(out, [0, 0]);
    }

    /// Build a single-node tree whose `pcie` node carries `dma-ranges`,
    /// then hand that node to `f`. The `PCIe` binding's cells are fixed:
    /// `#address-cells = 3` (the `phys.hi`/`phys.mid`/`phys.lo` triple),
    /// `#size-cells = 2`, with the parent root at `2`/`2`.
    fn with_pcie_dma_ranges(dma_ranges: &[u8], f: impl FnOnce(Option<(u64, u64, u64)>)) {
        let mut b = DtbBuilder::new();
        b.begin_node("");
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 2);
        b.begin_node("pcie@7d500000");
        b.prop_str("compatible", "brcm,bcm2711-pcie");
        b.prop_u32("#address-cells", 3);
        b.prop_u32("#size-cells", 2);
        if !dma_ranges.is_empty() {
            b.prop("dma-ranges", dma_ranges);
        }
        b.end_node();
        b.end_node();
        let blob = b.build();
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let pcie = fdt
            .nodes()
            .filter_map(Result::ok)
            .find(|n| n.name() == b"pcie@7d500000")
            .expect("pcie node");
        f(dma_ranges_aperture(&pcie, 3, 2, 2));
    }

    /// One BCM2711-shaped `dma-ranges` entry: 3-cell child PCI address,
    /// 2-cell parent CPU base, 2-cell size. The child PCI base
    /// (`phys.mid`/`phys.lo`, the inbound viewport's far side) is `0`.
    fn dma_ranges_entry(pci_hi: u32, parent_base: u64, size: u64) -> std::vec::Vec<u8> {
        dma_ranges_entry_at(pci_hi, 0, parent_base, size)
    }

    /// As [`dma_ranges_entry`] but with an explicit child PCI base
    /// `pci_base` in `phys.mid`/`phys.lo`.
    fn dma_ranges_entry_at(
        pci_hi: u32,
        pci_base: u64,
        parent_base: u64,
        size: u64,
    ) -> std::vec::Vec<u8> {
        let mut v = std::vec::Vec::new();
        v.extend_from_slice(&pci_hi.to_be_bytes()); // phys.hi
        v.extend_from_slice(&pci_base.to_be_bytes()); // phys.mid:phys.lo
        v.extend_from_slice(&parent_base.to_be_bytes());
        v.extend_from_slice(&size.to_be_bytes());
        v
    }

    #[test]
    fn reads_the_bcm2711_pcie_inbound_aperture() {
        // The real Pi 4 tree: PCI 0x0 → CPU 0x0 for 3 GiB, so devices
        // behind the bridge DMA only the low 3 GiB of SDRAM. The top is
        // the exclusive upper bound, the len the aperture extent.
        let dr = dma_ranges_entry(0x0200_0000, 0x0, 0xc000_0000);
        with_pcie_dma_ranges(&dr, |aperture| {
            // (top, len, inbound PCIe base) — the Pi views memory at
            // PCIe address 0.
            assert_eq!(aperture, Some((0xc000_0000, 0xc000_0000, 0)));
        });
    }

    #[test]
    fn pcie_inbound_viewport_carries_a_nonzero_pcie_base() {
        // A viewport not anchored at PCIe address 0: the child PCI base
        // (`phys.mid`/`phys.lo`) is captured as the inbound translation,
        // distinct from the CPU base.
        let dr = dma_ranges_entry_at(0x0200_0000, 0x4000_0000, 0x0, 0xc000_0000);
        with_pcie_dma_ranges(&dr, |aperture| {
            assert_eq!(aperture, Some((0xc000_0000, 0xc000_0000, 0x4000_0000)));
        });
    }

    #[test]
    fn pcie_aperture_spans_multiple_entries() {
        // Two windows: their union runs from the lowest base to the
        // highest top.
        let mut dr = dma_ranges_entry_at(0x0200_0000, 0x1000, 0x1_0000_0000, 0x4000_0000);
        dr.extend_from_slice(&dma_ranges_entry_at(
            0x0200_0000,
            0x2000,
            0x8000_0000,
            0x4000_0000,
        ));
        with_pcie_dma_ranges(&dr, |aperture| {
            // base = min(0x8000_0000, 0x1_0000_0000), top = max tops, and
            // the inbound PCIe base is the lowest-CPU-base entry's
            // (`0x8000_0000` → `0x2000`).
            assert_eq!(aperture, Some((0x1_4000_0000, 0xc000_0000, 0x2000)));
        });
    }

    #[test]
    fn pcie_aperture_absent_without_dma_ranges() {
        with_pcie_dma_ranges(&[], |aperture| assert_eq!(aperture, None));
    }

    #[test]
    fn pcie_aperture_rejects_a_partial_entry() {
        // A `dma-ranges` value that is not a whole number of 7-cell
        // entries is refused, never read past its end.
        let mut dr = dma_ranges_entry(0x0200_0000, 0x0, 0xc000_0000);
        dr.truncate(dr.len() - 4);
        with_pcie_dma_ranges(&dr, |aperture| assert_eq!(aperture, None));
    }

    #[test]
    fn pcie_aperture_rejects_out_of_range_cells() {
        let dr = dma_ranges_entry(0x0200_0000, 0x0, 0xc000_0000);
        let blob = {
            let mut b = DtbBuilder::new();
            b.begin_node("");
            b.prop_u32("#address-cells", 2);
            b.prop_u32("#size-cells", 2);
            b.begin_node("pcie@7d500000");
            b.prop_str("compatible", "brcm,bcm2711-pcie");
            b.prop("dma-ranges", &dr);
            b.end_node();
            b.end_node();
            b.build()
        };
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let pcie = fdt
            .nodes()
            .filter_map(Result::ok)
            .find(|n| n.name() == b"pcie@7d500000")
            .expect("pcie node");
        // A zero or over-wide cell count fails closed.
        assert_eq!(dma_ranges_aperture(&pcie, 0, 2, 2), None);
        assert_eq!(dma_ranges_aperture(&pcie, 4, 2, 2), None);
        assert_eq!(dma_ranges_aperture(&pcie, 3, 3, 2), None);
        assert_eq!(dma_ranges_aperture(&pcie, 3, 2, 0), None);
    }

    /// Build a single-node tree whose `pcie` node carries an outbound
    /// `ranges`, then hand that node to `f`. Same cells as the
    /// `dma-ranges` helper: child `3`, parent `2`, size `2`.
    fn with_pcie_ranges(ranges: &[u8], f: impl FnOnce(Option<(u64, u64, u64)>)) {
        let mut b = DtbBuilder::new();
        b.begin_node("");
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 2);
        b.begin_node("pcie@7d500000");
        b.prop_str("compatible", "brcm,bcm2711-pcie");
        b.prop_u32("#address-cells", 3);
        b.prop_u32("#size-cells", 2);
        if !ranges.is_empty() {
            b.prop("ranges", ranges);
        }
        b.end_node();
        b.end_node();
        let blob = b.build();
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let pcie = fdt
            .nodes()
            .filter_map(Result::ok)
            .find(|n| n.name() == b"pcie@7d500000")
            .expect("pcie node");
        f(outbound_mmio_window(&pcie, 3, 2, 2));
    }

    /// One PCI `ranges` entry: 3-cell child (phys.hi + 64-bit PCIe base),
    /// 2-cell parent CPU base, 2-cell size.
    fn ranges_entry(pci_hi: u32, pcie_base: u64, cpu_base: u64, size: u64) -> std::vec::Vec<u8> {
        let mut v = std::vec::Vec::new();
        v.extend_from_slice(&pci_hi.to_be_bytes()); // phys.hi (space code)
        v.extend_from_slice(&pcie_base.to_be_bytes()); // phys.mid + phys.lo
        v.extend_from_slice(&cpu_base.to_be_bytes());
        v.extend_from_slice(&size.to_be_bytes());
        v
    }

    #[test]
    fn reads_the_bcm2711_pcie_outbound_window() {
        // The real Pi 4 tree: 32-bit memory space (phys.hi 0x02…),
        // CPU 0x6_0000_0000 -> PCIe 0xc000_0000, 1 GiB.
        let r = ranges_entry(0x0200_0000, 0xc000_0000, 0x6_0000_0000, 0x4000_0000);
        with_pcie_ranges(&r, |win| {
            assert_eq!(win, Some((0x6_0000_0000, 0xc000_0000, 0x4000_0000)));
        });
    }

    #[test]
    fn outbound_window_skips_an_io_space_entry() {
        // An I/O-space entry (space code 0b01) precedes the memory
        // window; the decoder skips it and returns the memory aperture.
        let mut r = ranges_entry(0x0100_0000, 0x0, 0x6_f000_0000, 0x1000);
        r.extend_from_slice(&ranges_entry(
            0x0200_0000,
            0xc000_0000,
            0x6_0000_0000,
            0x4000_0000,
        ));
        with_pcie_ranges(&r, |win| {
            assert_eq!(win, Some((0x6_0000_0000, 0xc000_0000, 0x4000_0000)));
        });
    }

    #[test]
    fn outbound_window_absent_without_ranges() {
        with_pcie_ranges(&[], |win| assert_eq!(win, None));
    }

    #[test]
    fn outbound_window_rejects_a_partial_entry() {
        // Not a whole number of 7-cell entries: refused, never read past
        // its end.
        let mut r = ranges_entry(0x0200_0000, 0xc000_0000, 0x6_0000_0000, 0x4000_0000);
        r.truncate(r.len() - 4);
        with_pcie_ranges(&r, |win| assert_eq!(win, None));
    }

    #[test]
    fn outbound_window_none_when_only_io_space() {
        // An all-I/O-space `ranges` has no memory window to return.
        let r = ranges_entry(0x0100_0000, 0x0, 0x6_f000_0000, 0x1000);
        with_pcie_ranges(&r, |win| assert_eq!(win, None));
    }

    #[test]
    fn outbound_window_rejects_out_of_range_cells() {
        let r = ranges_entry(0x0200_0000, 0xc000_0000, 0x6_0000_0000, 0x4000_0000);
        let blob = {
            let mut b = DtbBuilder::new();
            b.begin_node("");
            b.prop_u32("#address-cells", 2);
            b.prop_u32("#size-cells", 2);
            b.begin_node("pcie@7d500000");
            b.prop_str("compatible", "brcm,bcm2711-pcie");
            b.prop("ranges", &r);
            b.end_node();
            b.end_node();
            b.build()
        };
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let pcie = fdt
            .nodes()
            .filter_map(Result::ok)
            .find(|n| n.name() == b"pcie@7d500000")
            .expect("pcie node");
        // A non-PCI child width, or an over-wide parent/size, fails closed.
        assert_eq!(outbound_mmio_window(&pcie, 2, 2, 2), None);
        assert_eq!(outbound_mmio_window(&pcie, 3, 3, 2), None);
        assert_eq!(outbound_mmio_window(&pcie, 3, 2, 0), None);
    }

    /// Build a minimal tree carrying a `/timer` node with the given
    /// `clock-frequency` (a single big-endian `u32`), used to exercise
    /// the P4 counter-rate override reader.
    fn tree_with_timer_clock(hz: u32) -> std::vec::Vec<u8> {
        let mut b = DtbBuilder::new();
        b.begin_node("");
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 2);
        b.begin_node("timer");
        b.prop_str("compatible", "arm,armv8-timer");
        b.prop_u32("clock-frequency", hz);
        b.end_node();
        b.end_node();
        b.build()
    }

    #[test]
    fn reads_psci_method_hvc_and_smc() {
        let hvc = virt_like_arm(0x4000_0000, 0x2000_0000, "hvc", 14);
        let fdt = Fdt::new(&hvc).expect("valid fdt");
        assert_eq!(psci_method(&fdt), Some(PsciMethod::Hvc));

        let smc = virt_like_arm(0x4000_0000, 0x2000_0000, "smc", 14);
        let fdt = Fdt::new(&smc).expect("valid fdt");
        assert_eq!(psci_method(&fdt), Some(PsciMethod::Smc));
    }

    #[test]
    fn reads_psci_method_from_a_raspi_shaped_tree() {
        // The Raspberry-Pi-shaped fixture carries a `/psci` node with the
        // `smc` conduit an EL3-firmware platform uses (`armstub8.bin`).
        let blob = raspi_like_arm(0x7e20_1000, 0x7e21_5040);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        assert_eq!(psci_method(&fdt), Some(PsciMethod::Smc));
    }

    #[test]
    fn psci_method_absent_on_a_tree_without_the_node() {
        // A tree with no `/psci` node yields `None`, so the bring-up path
        // fails closed rather than assuming a conduit.
        let mut b = DtbBuilder::new();
        b.begin_node("");
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 2);
        b.begin_node("timer");
        b.prop_str("compatible", "arm,armv8-timer");
        b.end_node();
        b.end_node();
        let blob = b.build();
        let fdt = Fdt::new(&blob).expect("valid fdt");
        assert_eq!(psci_method(&fdt), None);
    }

    #[test]
    fn unknown_psci_method_is_rejected() {
        assert_eq!(PsciMethod::from_property(b"nonsense\0"), None);
        assert_eq!(PsciMethod::from_property(b"hvc\0"), Some(PsciMethod::Hvc));
        assert_eq!(PsciMethod::from_property(b"smc"), Some(PsciMethod::Smc));
    }

    #[test]
    fn reads_timer_clock_frequency_when_the_node_declares_it() {
        // The Pi 4's 54 MHz crystal is the evocative value here.
        let blob = tree_with_timer_clock(54_000_000);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        assert_eq!(timer_clock_frequency(&fdt), Some(54_000_000));
    }

    #[test]
    fn timer_clock_frequency_absent_on_a_tree_without_the_property() {
        // The `virt`-shaped fixture carries `/timer` `interrupts` but no
        // `clock-frequency`, so the override is absent and the boot path
        // falls back to `CNTFRQ_EL0`.
        let blob = virt_like_arm(0x4000_0000, 0x2000_0000, "hvc", 30);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        assert_eq!(timer_clock_frequency(&fdt), None);
    }

    #[test]
    fn effective_timer_hz_prefers_a_nonzero_tree_override() {
        // A present, non-zero tree value wins over the register reading.
        assert_eq!(effective_timer_hz(Some(54_000_000), 62_500_000), 54_000_000);
    }

    #[test]
    fn effective_timer_hz_falls_back_to_the_register() {
        // Absent override → the `CNTFRQ_EL0` value is used.
        assert_eq!(effective_timer_hz(None, 62_500_000), 62_500_000);
        // A zero override is treated as absent (never a 0 Hz timer).
        assert_eq!(effective_timer_hz(Some(0), 62_500_000), 62_500_000);
    }

    /// Build a minimal tree with one device node carrying an `interrupts`
    /// triple `<type number flags>`, then hand that node to `f`.
    fn with_device_interrupts(triple: &[u8], f: impl FnOnce(&super::Node<'_>)) {
        let mut b = DtbBuilder::new();
        b.begin_node("");
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 2);
        b.begin_node("virtio_mmio@a000000");
        b.prop_str("compatible", "virtio,mmio");
        if !triple.is_empty() {
            b.prop("interrupts", triple);
        }
        b.end_node();
        b.end_node();
        let blob = b.build();
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let node = fdt
            .nodes()
            .filter_map(Result::ok)
            .find(|n| n.name() == b"virtio_mmio@a000000")
            .expect("device node");
        f(&node);
    }

    /// Encode a GIC `<type number flags>` interrupt triple as three
    /// big-endian cells, the way a device tree stores it.
    fn interrupts_triple(kind: u32, number: u32, flags: u32) -> std::vec::Vec<u8> {
        let mut v = std::vec::Vec::new();
        v.extend_from_slice(&kind.to_be_bytes());
        v.extend_from_slice(&number.to_be_bytes());
        v.extend_from_slice(&flags.to_be_bytes());
        v
    }

    #[test]
    fn spi_cells_offset_by_the_shared_min_spi_base() {
        // An SPI `number` maps to INTID `number + MIN_SPI_INTID`. SPI 2 is
        // the `virt` board's PL031 RTC (INTID 34), the device the IRQ
        // vertical arms — proving the decoder agrees with the routing.
        assert_eq!(gic_intid_from_cells(GIC_TYPE_SPI, 0), Some(MIN_SPI_INTID));
        assert_eq!(
            gic_intid_from_cells(GIC_TYPE_SPI, 2),
            Some(MIN_SPI_INTID + 2)
        );
    }

    #[test]
    fn ppi_cells_offset_by_the_ppi_base() {
        // A PPI `number` maps to INTID `number + 16`; the generic-timer
        // PPI on `virt` is PPI 14 → INTID 30.
        assert_eq!(gic_intid_from_cells(GIC_TYPE_PPI, 0), Some(16));
        assert_eq!(gic_intid_from_cells(GIC_TYPE_PPI, 14), Some(30));
    }

    #[test]
    fn unknown_interrupt_type_is_refused() {
        // A type cell that is neither SPI (0) nor PPI (1) — e.g. a
        // GICv3-only extended-SPI binding — yields no INTID (fail closed).
        assert_eq!(gic_intid_from_cells(2, 5), None);
        assert_eq!(gic_intid_from_cells(0xFFFF_FFFF, 0), None);
    }

    #[test]
    fn spi_number_above_the_controller_ceiling_is_refused() {
        // An SPI `number` whose INTID would exceed `MAX_INTID` is rejected
        // rather than silently routed to an unaddressable line.
        let over = MAX_INTID - MIN_SPI_INTID + 1;
        assert_eq!(gic_intid_from_cells(GIC_TYPE_SPI, over), None);
        // The exact ceiling is accepted.
        assert_eq!(
            gic_intid_from_cells(GIC_TYPE_SPI, MAX_INTID - MIN_SPI_INTID),
            Some(MAX_INTID)
        );
    }

    #[test]
    fn cell_offset_addition_cannot_overflow() {
        // A hostile `number` near `u32::MAX` must not wrap the addition
        // (a validation bound, fail closed).
        assert_eq!(gic_intid_from_cells(GIC_TYPE_SPI, u32::MAX), None);
        assert_eq!(gic_intid_from_cells(GIC_TYPE_PPI, u32::MAX), None);
    }

    #[test]
    fn device_node_spi_is_decoded_from_its_interrupts_property() {
        // SPI 2, level-high (flags 4) — the `virt` RTC specifier shape.
        let triple = interrupts_triple(GIC_TYPE_SPI, 2, 4);
        with_device_interrupts(&triple, |node| {
            assert_eq!(gic_device_intid(node), Some(MIN_SPI_INTID + 2));
        });
    }

    #[test]
    fn device_node_without_interrupts_yields_none() {
        with_device_interrupts(&[], |node| {
            assert_eq!(gic_device_intid(node), None);
        });
    }

    #[test]
    fn device_node_with_a_truncated_interrupts_property_yields_none() {
        // Only the first cell present: the decode needs two cells, so a
        // short property is refused, never read past its end.
        let mut short = std::vec::Vec::new();
        short.extend_from_slice(&GIC_TYPE_SPI.to_be_bytes());
        with_device_interrupts(&short, |node| {
            assert_eq!(gic_device_intid(node), None);
        });
    }
}
