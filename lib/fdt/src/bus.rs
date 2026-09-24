//! Bus-aware `reg` decoding: the ancestor-bus state every device-tree
//! consumer needs to turn a node's raw `reg` into a CPU-physical window.
//!
//! Real boards put peripherals behind buses carrying their own
//! `#address-cells`/`#size-cells` and `ranges` (the Pi 4's `/soc`, a PCIe
//! root complex), so a raw `reg` read is a *bus* address, never the address
//! a driver may map. Decoding it needs per-depth state the plain node
//! iterator does not carry, which is what this module adds: [`BusLevel`]
//! tracking, the ancestor-chain [`translate`] step, the per-entry
//! [`translated_reg`] reader, the `dma-ranges` and PCI outbound-window
//! decoders, and the early-returning [`scan_translated`] walk.
//!
//! It is pure Devicetree spec v0.4 §2.3 address translation, so it lives
//! beside the parser rather than in any one architecture port: every
//! FDT-based port decodes `reg` the same way, and only the interrupt
//! specifier differs.

use crate::{read_cells, Fdt, Node};

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
    /// This node's raw `dma-ranges` value: the window a device *on this bus*
    /// may reach by DMA (`None` when absent — the bus declares no reach).
    /// Devicetree Spec v0.4 §2.3.9 puts the property on the bus, not on the
    /// mastering device, so a node's DMA constraint is read from its parent
    /// level rather than from itself.
    pub dma_ranges: Option<&'a [u8]>,
}

impl BusLevel<'_> {
    /// State before any node is visited: the devicetree-spec default cell
    /// counts (2 address, 1 size) with no `ranges`.
    pub const DEFAULT: Self = BusLevel {
        addr_cells: 2,
        size_cells: 1,
        ranges: None,
        dma_ranges: None,
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
        dma_ranges: node.property("dma-ranges").map(|p| p.value()),
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
    translate_through(levels, depth, addr)
}

fn translate_through(levels: &[BusLevel<'_>], depth: usize, addr: u64) -> Option<u64> {
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

/// One `dma-ranges` entry (Devicetree Spec v0.4 §2.3.9): `size` bytes of the
/// child bus from `child`, reaching the parent bus at `parent`.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DmaRange {
    /// Child-bus base. A three-cell PCI address contributes its low 64 bits
    /// (`phys.mid`/`phys.lo`); its `phys.hi` cell carries flags, not address.
    pub child: u64,
    /// Parent-bus base.
    pub parent: u64,
    /// Window size in bytes.
    pub size: u64,
}

/// The entries of a `dma-ranges` value, in order; built by [`dma_ranges`].
#[derive(Clone)]
pub struct DmaRanges<'a> {
    value: &'a [u8],
    cells: RangeCells,
    off: usize,
}

impl DmaRanges<'_> {
    fn entry_len(&self) -> usize {
        ((self.cells.child_address + self.cells.parent_address + self.cells.child_size) * 4)
            as usize
    }
}

impl Iterator for DmaRanges<'_> {
    type Item = DmaRange;

    fn next(&mut self) -> Option<DmaRange> {
        let RangeCells {
            child_address,
            parent_address,
            child_size,
        } = self.cells;
        let off = self.off;
        if off + self.entry_len() > self.value.len() {
            return None;
        }
        self.off += self.entry_len();
        // A three-cell child is a PCI address: its low two cells are the base.
        let child = if child_address == 3 {
            read_cells(self.value, off + 4, 2)?
        } else {
            read_cells(self.value, off, child_address)?
        };
        let parent_at = off + (child_address as usize) * 4;
        Some(DmaRange {
            child,
            parent: read_cells(self.value, parent_at, parent_address)?,
            size: read_cells(
                self.value,
                parent_at + (parent_address as usize) * 4,
                child_size,
            )?,
        })
    }
}

/// The entries of a `dma-ranges` `value`, decoded with the child bus's own
/// `#address-cells` and `#size-cells` and its parent's `#address-cells`.
///
/// Returns `None` — so every entry yielded is whole — when a cell count is
/// outside `1..=2` (`1..=3` for the child address, the PCI triple) or the
/// value is not a whole, non-empty number of entries.
#[must_use]
pub fn dma_ranges(
    value: &[u8],
    child_address: u32,
    parent_address: u32,
    child_size: u32,
) -> Option<DmaRanges<'_>> {
    if child_address == 0
        || child_address > 3
        || parent_address == 0
        || parent_address > 2
        || child_size == 0
        || child_size > 2
    {
        return None;
    }
    let ranges = DmaRanges {
        value,
        cells: RangeCells {
            child_address,
            parent_address,
            child_size,
        },
        off: 0,
    };
    if value.is_empty() || !value.len().is_multiple_of(ranges.entry_len()) {
        return None;
    }
    Some(ranges)
}

/// The most windows [`dma_reach`] composes. A bound on untrusted input, since
/// each bus's entries multiply the windows of the buses below it.
pub const MAX_DMA_WINDOWS: usize = 16;

/// One window a device reaches memory through: `size` bytes from bus address
/// `bus` on the device's own bus, landing at CPU-physical address `cpu`.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DmaWindow {
    /// Where the window starts on the device's own bus.
    pub bus: u64,
    /// Where it lands in CPU-physical memory.
    pub cpu: u64,
    /// Its length in bytes.
    pub size: u64,
}

/// At most [`MAX_DMA_WINDOWS`] composed windows.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct DmaWindows {
    windows: [DmaWindow; MAX_DMA_WINDOWS],
    len: usize,
}

impl DmaWindows {
    const EMPTY: Self = Self {
        windows: [DmaWindow {
            bus: 0,
            cpu: 0,
            size: 0,
        }; MAX_DMA_WINDOWS],
        len: 0,
    };

    fn as_slice(&self) -> &[DmaWindow] {
        &self.windows[..self.len]
    }

    /// Keep `window`, reporting whether there was room for it.
    fn push(&mut self, window: DmaWindow) -> bool {
        let Some(slot) = self.windows.get_mut(self.len) else {
            return false;
        };
        *slot = window;
        self.len += 1;
        true
    }

    /// The first bus's entries, each a window of its own.
    fn of(entries: DmaRanges<'_>) -> Self {
        let mut out = Self::EMPTY;
        for entry in entries.filter(|entry| entry.size != 0) {
            let window = DmaWindow {
                bus: entry.child,
                cpu: entry.parent,
                size: entry.size,
            };
            if !out.push(window) {
                break;
            }
        }
        out
    }

    /// These windows carried through one more bus: each clipped to that bus's
    /// entries, split where an entry boundary changes the offset, and rebased
    /// into its parent's space. A part no entry covers is dropped.
    fn through(&self, entries: &DmaRanges<'_>) -> Self {
        let mut out = Self::EMPTY;
        for window in self.as_slice() {
            let (start, end) = span(window.cpu, window.size);
            for entry in entries.clone() {
                let (entry_start, entry_end) = span(entry.child, entry.size);
                let (lo, hi) = (start.max(entry_start), end.min(entry_end));
                if lo >= hi {
                    continue;
                }
                let into_window = lo - start;
                let into_entry = lo - entry_start;
                let (Ok(into_window), Ok(into_entry), Ok(size)) = (
                    u64::try_from(into_window),
                    u64::try_from(into_entry),
                    u64::try_from(hi - lo),
                ) else {
                    continue;
                };
                let (Some(bus), Some(cpu)) = (
                    window.bus.checked_add(into_window),
                    entry.parent.checked_add(into_entry),
                ) else {
                    continue;
                };
                if !out.push(DmaWindow { bus, cpu, size }) {
                    return out;
                }
            }
        }
        out
    }
}

/// `[start, start + size)` in a width that cannot overflow.
fn span(start: u64, size: u64) -> (u128, u128) {
    (u128::from(start), u128::from(start) + u128::from(size))
}

/// Where a device on a bus reaches memory by DMA; built by [`dma_reach`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DmaReach {
    windows: DmaWindows,
    translated: bool,
}

impl DmaReach {
    const IDENTITY: Self = Self {
        windows: DmaWindows::EMPTY,
        translated: false,
    };

    /// The windows the device reaches memory through, in the order the buses
    /// list them, or `None` when no bus between it and the root translates or
    /// bounds an address.
    #[must_use]
    pub fn windows(&self) -> Option<&[DmaWindow]> {
        self.translated.then(|| self.windows.as_slice())
    }
}

/// Compose the `dma-ranges` of every bus between the node at `depth` and the
/// root into the windows it reaches memory through (Devicetree Spec v0.4
/// §2.3.9). An empty property is the identity at its bus and the buses above
/// it still apply; an absent or malformed one anywhere on the way maps
/// nothing, so `None`.
#[must_use]
pub fn dma_reach(levels: &[BusLevel<'_>], depth: usize) -> Option<DmaReach> {
    let mut reach = DmaReach::IDENTITY;
    for bus in (1..depth).rev() {
        let level = levels.get(bus)?;
        let value = level.dma_ranges?;
        if value.is_empty() {
            continue;
        }
        let parent = levels.get(bus - 1)?;
        let entries = dma_ranges(value, level.addr_cells, parent.addr_cells, level.size_cells)?;
        let windows = if reach.translated {
            reach.windows.through(&entries)
        } else {
            DmaWindows::of(entries)
        };
        reach = DmaReach {
            windows,
            translated: true,
        };
    }
    Some(reach)
}

/// Decode a PCI host bridge's `dma-ranges` into the inbound DMA aperture
/// it grants devices behind it (Devicetree Spec v0.4 §2.3.9): the
/// CPU-physical window `[base, base + len)` a device on the bus may DMA
/// through, returned as `(top, len)` where `top = base + len` is the
/// *exclusive* upper bound — the same form a hardware-tree walk hands
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
/// extent, and `bus_base` the bus-space address the lowest entry's viewport
/// starts at (the inbound translation, the counterpart of
/// [`outbound_mmio_window`]'s `pcie_base`; a [`DmaRange::child`]). Returns
/// [`None`] when the node carries no `dma-ranges`, a cell count is out of
/// range, the value is not a whole number of entries, or a `base + len`
/// overflows. With multiple entries the aperture spans the lowest base to the
/// highest top; [`dma_ranges`] yields the entries themselves.
#[must_use]
pub fn dma_ranges_aperture(
    node: &Node<'_>,
    child_address: u32,
    parent_address: u32,
    child_size: u32,
) -> Option<(u64, u64, u64)> {
    dma_ranges_aperture_of(
        node.property("dma-ranges")?.value(),
        child_address,
        parent_address,
        child_size,
    )
}

/// [`dma_ranges_aperture`] over an already-read `dma-ranges` `value`, so a
/// node's own property and an ancestor [`BusLevel`]'s share one decode.
#[must_use]
pub fn dma_ranges_aperture_of(
    value: &[u8],
    child_address: u32,
    parent_address: u32,
    child_size: u32,
) -> Option<(u64, u64, u64)> {
    let mut lowest: Option<DmaRange> = None;
    let mut max_top: u64 = 0;
    for range in dma_ranges(value, child_address, parent_address, child_size)? {
        max_top = max_top.max(range.parent.checked_add(range.size)?);
        if lowest.is_none_or(|l| range.parent < l.parent) {
            lowest = Some(range);
        }
    }
    let lowest = lowest?;
    Some((max_top, max_top.checked_sub(lowest.parent)?, lowest.child))
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
/// Like a port's own early boot-path queries, this is an early-returning
/// `Fdt::nodes` traversal that reads only the visited nodes' own properties, so a caller that returns at its matched node
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

#[cfg(test)]
mod tests {
    use super::{dma_ranges, dma_ranges_aperture, dma_ranges_aperture_of, outbound_mmio_window};
    use super::{dma_reach, BusLevel, DmaRange, DmaReach, DmaWindow, Fdt, MAX_DMA_WINDOWS};
    use crate::fixture::DtbBuilder;
    use alloc::vec::Vec;

    /// A one-cell child, two-cell parent, one-cell size `dma-ranges`, the
    /// shape of an on-chip bus under a 64-bit root.
    fn soc_dma_ranges(entries: &[(u32, u64, u32)]) -> Vec<u8> {
        let mut v = Vec::new();
        for &(child, parent, size) in entries {
            v.extend_from_slice(&child.to_be_bytes());
            v.extend_from_slice(&parent.to_be_bytes());
            v.extend_from_slice(&size.to_be_bytes());
        }
        v
    }

    #[test]
    fn each_dma_ranges_entry_is_decoded_on_its_own() {
        let value = soc_dma_ranges(&[
            (0xc000_0000, 0x0, 0x4000_0000),
            (0x7c00_0000, 0xfc00_0000, 0x0380_0000),
        ]);
        let entries: Vec<DmaRange> = dma_ranges(&value, 1, 2, 1).expect("whole").collect();
        assert_eq!(
            entries,
            [
                DmaRange {
                    child: 0xc000_0000,
                    parent: 0x0,
                    size: 0x4000_0000,
                },
                DmaRange {
                    child: 0x7c00_0000,
                    parent: 0xfc00_0000,
                    size: 0x0380_0000,
                },
            ]
        );
        // The fold spans both windows and reports the lowest one's real child
        // base, where a non-PCI bus once reported none.
        assert_eq!(
            dma_ranges_aperture_of(&value, 1, 2, 1),
            Some((0xff80_0000, 0xff80_0000, 0xc000_0000))
        );
    }

    /// A one-cell child, one-cell parent, one-cell size `dma-ranges`: a
    /// sub-bus inside the soc.
    fn sub_dma_ranges(entries: &[(u32, u32, u32)]) -> Vec<u8> {
        entries
            .iter()
            .flat_map(|&(child, parent, size)| [child, parent, size])
            .flat_map(u32::to_be_bytes)
            .collect()
    }

    /// Root, a Pi-style soc bus, and a sub-bus carrying `sub`, so a device on
    /// the sub-bus sits at depth 3.
    fn pi_levels<'a>(soc: &'a [u8], sub: Option<&'a [u8]>) -> [BusLevel<'a>; 3] {
        let level = |dma_ranges| BusLevel {
            addr_cells: 1,
            size_cells: 1,
            ranges: Some(&[][..]),
            dma_ranges,
        };
        let root = BusLevel {
            addr_cells: 2,
            ..BusLevel::DEFAULT
        };
        [root, level(Some(soc)), level(sub)]
    }

    fn windows(levels: &[BusLevel<'_>], depth: usize) -> Vec<DmaWindow> {
        dma_reach(levels, depth)
            .as_ref()
            .and_then(DmaReach::windows)
            .map(<[DmaWindow]>::to_vec)
            .expect("translated windows")
    }

    #[test]
    fn an_identity_bus_keeps_the_translation_of_the_buses_above_it() {
        let soc = soc_dma_ranges(&[(0xc000_0000, 0x0, 0x4000_0000)]);
        assert_eq!(
            windows(&pi_levels(&soc, Some(&[])), 3),
            [DmaWindow {
                bus: 0xc000_0000,
                cpu: 0x0,
                size: 0x4000_0000,
            }]
        );
    }

    #[test]
    fn an_inner_window_is_clipped_to_the_outer_one_and_rebased_through_it() {
        let soc = soc_dma_ranges(&[(0xc000_0000, 0x0, 0x4000_0000)]);
        // The inner bus reaches 0x3000_0000..0x5000_0000 on the soc; only the
        // lower half lies in the soc's window to memory.
        let sub = sub_dma_ranges(&[(0x0, 0xf000_0000, 0x2000_0000)]);
        assert_eq!(
            windows(&pi_levels(&soc, Some(&sub)), 3),
            [DmaWindow {
                bus: 0x0,
                cpu: 0x3000_0000,
                size: 0x1000_0000,
            }]
        );
    }

    #[test]
    fn a_window_spanning_two_outer_entries_is_split_where_the_offset_changes() {
        let soc = soc_dma_ranges(&[
            (0xc000_0000, 0x0, 0x1000_0000),
            (0xd000_0000, 0x8000_0000, 0x1000_0000),
        ]);
        let sub = sub_dma_ranges(&[(0x0, 0xc800_0000, 0x1000_0000)]);
        assert_eq!(
            windows(&pi_levels(&soc, Some(&sub)), 3),
            [
                DmaWindow {
                    bus: 0x0,
                    cpu: 0x0800_0000,
                    size: 0x0800_0000,
                },
                DmaWindow {
                    bus: 0x0800_0000,
                    cpu: 0x8000_0000,
                    size: 0x0800_0000,
                },
            ]
        );
    }

    #[test]
    fn a_bus_that_maps_nothing_leaves_no_reach_and_all_identity_leaves_no_bound() {
        let soc = soc_dma_ranges(&[(0xc000_0000, 0x0, 0x4000_0000)]);
        assert_eq!(dma_reach(&pi_levels(&soc, None), 3), None);
        // A window wholly outside every outer entry is dropped.
        let sub = sub_dma_ranges(&[(0x0, 0x1000, 0x1000)]);
        assert_eq!(windows(&pi_levels(&soc, Some(&sub)), 3), []);
        let identity = [
            BusLevel::DEFAULT,
            BusLevel {
                dma_ranges: Some(&[]),
                ..BusLevel::DEFAULT
            },
        ];
        for depth in [1, 2] {
            let reach = dma_reach(&identity, depth).expect("an identity reach");
            assert_eq!(reach.windows(), None);
        }
    }

    #[test]
    fn composed_windows_are_bounded() {
        let soc_entries: Vec<(u32, u64, u32)> = (0..8u32)
            .map(|i| {
                (
                    0xc000_0000 + i * 0x0100_0000,
                    u64::from(i) * 0x1000_0000,
                    0x0100_0000,
                )
            })
            .collect();
        let soc = soc_dma_ranges(&soc_entries);
        let sub_entries: Vec<(u32, u32, u32)> = (0..8u32)
            .map(|i| (i * 0x1000_0000, 0xc000_0000, 0x0800_0000))
            .collect();
        let sub = sub_dma_ranges(&sub_entries);
        let composed = windows(&pi_levels(&soc, Some(&sub)), 3);
        assert_eq!(composed.len(), MAX_DMA_WINDOWS);
    }

    #[test]
    fn a_dma_ranges_value_that_is_not_whole_decodes_to_nothing() {
        let value = soc_dma_ranges(&[(0xc000_0000, 0x0, 0x4000_0000)]);
        assert!(dma_ranges(&value[..value.len() - 4], 1, 2, 1).is_none());
        assert!(dma_ranges(&[], 1, 2, 1).is_none());
        for (child, parent, size) in [
            (0, 2, 1),
            (4, 2, 1),
            (1, 0, 1),
            (1, 3, 1),
            (1, 2, 0),
            (1, 2, 3),
        ] {
            assert!(
                dma_ranges(&value, child, parent, size).is_none(),
                "{child}/{parent}/{size}"
            );
        }
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
    fn dma_ranges_entry(pci_hi: u32, parent_base: u64, size: u64) -> Vec<u8> {
        dma_ranges_entry_at(pci_hi, 0, parent_base, size)
    }

    /// As [`dma_ranges_entry`] but with an explicit child PCI base
    /// `pci_base` in `phys.mid`/`phys.lo`.
    fn dma_ranges_entry_at(pci_hi: u32, pci_base: u64, parent_base: u64, size: u64) -> Vec<u8> {
        let mut v = Vec::new();
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
    fn ranges_entry(pci_hi: u32, pcie_base: u64, cpu_base: u64, size: u64) -> Vec<u8> {
        let mut v = Vec::new();
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
}
