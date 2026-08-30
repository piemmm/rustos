//! The generic flattened-device-tree → hardware-tree walk every FDT-based
//! port shares.
//!
//! Turning a device tree into [`HwNode`]s is the same job on every such
//! port: emit the root, walk the tree in document order tracking each
//! depth's bus cells and `ranges` ([`tairix_fdt::bus`]), classify each node,
//! decode its `compatible` list into match keys, translate its `reg`
//! windows, and splice out the nodes no driver could ever bind. Only two
//! things genuinely differ, and [`FdtPlatform`] is exactly those two:
//!
//! * **the interrupt specifier** — its cell width and the mapping from its
//!   cells to the line number a granted driver binds (the GIC's type-relative
//!   SPI/PPI offsets, the PLIC's bare source number);
//! * **board augmentation** — extra resources only the platform's own tree
//!   can describe (a firmware mailbox's DMA carve, a root complex's
//!   windows). Ports with none leave the default empty.
//!
//! A port therefore contributes an [`FdtPlatform`] impl and spells its
//! discovery type as [`FdtDiscovery`] over it; the walk itself has one
//! definition. The impl is a *value* built from the tree once
//! ([`FdtPlatform::from_tree`]), so a port whose interrupt mapping depends
//! on a tree-wide fact — the RISC-V PLIC's `riscv,ndev` source count — reads
//! it before the walk rather than per node.

use tairix_abi::driver::net::MAC_ADDRESS_LEN;
use tairix_abi::{HwDeviceClass, HwMatchKey, HwNode, HwResource, HW_NODE_ROOT, HW_NODE_ROOT_ID};
use tairix_fdt::{
    bus_level, name_stem, reg_entry_count, translated_reg, BusLevel, Fdt, Node, MAX_WALK_DEPTH,
};

use crate::platform::{DiscoveryError, HwNodeSink, PlatformDiscovery};

/// Bytes in one device-tree cell.
const CELL_BYTES: usize = 4;

/// The per-port half of the device-tree walk.
pub trait FdtPlatform {
    /// Cells in one `interrupts` specifier on this platform's interrupt
    /// parent (three for a GIC, one for a PLIC).
    const INTERRUPT_CELLS: usize;

    /// Read whatever tree-wide facts the interrupt mapping needs, once,
    /// before the walk starts.
    fn from_tree(fdt: &Fdt<'_>) -> Self;

    /// Map one whole specifier — exactly `INTERRUPT_CELLS` cells — to the
    /// line number a granted driver binds, or `None` for a specifier this
    /// port cannot represent or its controller cannot raise.
    ///
    /// A `None` drops that specifier and leaves the rest of the list; the
    /// walk never guesses a line.
    fn interrupt_line(&self, specifier: &[u8]) -> Option<u32>;

    /// Push any resource only this platform's tree can describe onto a node
    /// the walk has already built. Ports with no board augmentation leave
    /// the default.
    fn augment(&self, _node: &Node<'_>, _depth: usize, _levels: &[BusLevel<'_>], _hw: &mut HwNode) {
    }
}

/// The [`PlatformDiscovery`] implementation over a validated device tree,
/// parameterised by the port's [`FdtPlatform`].
pub struct FdtDiscovery<'a, P> {
    fdt: Fdt<'a>,
    platform: P,
}

impl<'a, P: FdtPlatform> FdtDiscovery<'a, P> {
    /// Wrap an already-validated [`Fdt`] reader, reading the port's
    /// tree-wide facts from it once.
    #[must_use]
    pub fn new(fdt: Fdt<'a>) -> Self {
        let platform = P::from_tree(&fdt);
        Self { fdt, platform }
    }
}

impl<P: FdtPlatform> PlatformDiscovery for FdtDiscovery<'_, P> {
    fn discover(&self, sink: &mut dyn HwNodeSink) -> Result<(), DiscoveryError> {
        // Root first so every later node's parent is already emitted. Its
        // id is the shared `HW_NODE_ROOT_ID`; its parent is the
        // `HW_NODE_ROOT` sentinel, so it alone is `is_root`.
        sink.emit(HwNode::new(
            HW_NODE_ROOT_ID,
            HW_NODE_ROOT,
            HwDeviceClass::Root,
        ))?;
        let mut next_id: u32 = 1;
        // The shared per-depth bus state plus this walk's own per-depth
        // fact: the hardware-tree id of the nearest *emitted* ancestor,
        // which is the parent a child at depth + 1 names. A tree nested
        // beyond the tracked depth is refused as malformed rather than
        // silently under-enumerated.
        let mut levels = [BusLevel::DEFAULT; MAX_WALK_DEPTH];
        let mut ancestors = [0u32; MAX_WALK_DEPTH];

        for node in self.fdt.nodes() {
            let node = node.map_err(|_| DiscoveryError::MalformedSource)?;
            let depth = node.depth() as usize;
            if depth >= MAX_WALK_DEPTH {
                return Err(DiscoveryError::MalformedSource);
            }

            // This node's own cell counts and `ranges` govern its
            // *children*; record them whether or not the node is emitted.
            let mut level = bus_level(&node);
            if depth == 0 {
                level.ranges = None;
                levels[0] = level;
                ancestors[0] = HW_NODE_ROOT_ID;
                continue;
            }

            let mut ancestor = ancestors[depth - 1];
            if let Some(emitted) =
                build_node(&self.platform, &node, depth, &levels, ancestor, next_id)
            {
                sink.emit(emitted)?;
                ancestor = next_id;
                next_id = next_id
                    .checked_add(1)
                    .ok_or(DiscoveryError::MalformedSource)?;
            }
            levels[depth] = level;
            ancestors[depth] = ancestor;
        }

        Ok(())
    }
}

/// Build the hardware-tree node for one device-tree node, or `None` when
/// the node describes nothing the tree can carry (no representable match
/// key and not a memory node — the matcher could never bind it).
fn build_node<P: FdtPlatform>(
    platform: &P,
    node: &Node<'_>,
    depth: usize,
    levels: &[BusLevel<'_>],
    parent: u32,
    id: u32,
) -> Option<HwNode> {
    let class = classify(node);
    let mut hw = HwNode::new(id, parent, class);

    let mut keys = 0usize;
    if let Some(compat) = node.property("compatible") {
        for s in compat.iter_strings() {
            // A string longer than the ABI's bound is rejected on *both*
            // sides — a driver bind key could never carry it either — so
            // skipping it provably loses no match. Keys past the node
            // capacity are dropped most-specific-first preserved (the
            // devicetree list order).
            let Ok(key) = HwMatchKey::compatible(s) else {
                continue;
            };
            if hw.push_match_key(key).is_err() {
                break;
            }
            keys += 1;
        }
    }
    if keys == 0 && class != HwDeviceClass::Memory {
        return None;
    }

    push_mmio_resources(node, depth, levels, &mut hw);
    push_irq_resources(platform, node, &mut hw);

    // A NIC's own hardware address, where its node carries one: the
    // standard ethernet-controller binding, so it is read for every node
    // rather than gated on a board.
    if let Some(octets) = local_mac_address(node) {
        let _ = hw.push_resource(HwResource::link_address(octets));
    }

    platform.augment(node, depth, levels, &mut hw);

    Some(hw)
}

/// Decode each `reg` entry with the parent's cell counts, translate it
/// through the ancestor buses' `ranges`, and push it as an MMIO resource.
///
/// Entries that cannot be decoded (out-of-range cell counts, a length that
/// is not a whole number of entries) or translated (an ancestor bus without
/// usable `ranges`) are dropped — the tree never carries an invented or
/// untranslated window. Entries past the node's resource capacity are
/// dropped likewise (an ABI bound).
fn push_mmio_resources(node: &Node<'_>, depth: usize, levels: &[BusLevel<'_>], hw: &mut HwNode) {
    let Some(entries) = reg_entry_count(node, depth, levels) else {
        return;
    };
    for index in 0..entries {
        if let Some((base, len)) = translated_reg(node, depth, levels, index) {
            if hw.push_resource(HwResource::mmio(base, len)).is_err() {
                return;
            }
        }
    }
}

/// Push one IRQ resource per `interrupts` specifier, carrying the line
/// number the port's [`FdtPlatform::interrupt_line`] mapped it to.
///
/// A property whose length is not a whole number of specifiers is refused
/// entire — a partial list is a malformed one, and guessing where it ends
/// would invent a line. A single specifier the port cannot represent is
/// skipped and the rest still emitted.
fn push_irq_resources<P: FdtPlatform>(platform: &P, node: &Node<'_>, hw: &mut HwNode) {
    let specifier_len = P::INTERRUPT_CELLS * CELL_BYTES;
    let Some(interrupts) = node.property("interrupts") else {
        return;
    };
    let value = interrupts.value();
    if specifier_len == 0 || value.is_empty() || value.len() % specifier_len != 0 {
        return;
    }
    for specifier in value.chunks_exact(specifier_len) {
        if let Some(line) = platform.interrupt_line(specifier) {
            if hw
                .push_resource(HwResource::irq(u64::from(line), 1))
                .is_err()
            {
                return;
            }
        }
    }
}

/// A node's own hardware address from the standard ethernet-controller
/// binding.
///
/// `mac-address` (the current address) takes precedence over
/// `local-mac-address` (the address programmed at manufacture), matching the
/// binding's own precedence. A property that is not exactly one address is
/// ignored rather than truncated or padded, and an all-zero address is
/// refused (it is neither a valid unicast nor the broadcast address, so it
/// carries no identity) — fail closed, never a guessed MAC.
fn local_mac_address(node: &Node<'_>) -> Option<[u8; MAC_ADDRESS_LEN]> {
    for name in ["mac-address", "local-mac-address"] {
        let Some(property) = node.property(name) else {
            continue;
        };
        let value = property.value();
        if value.len() != MAC_ADDRESS_LEN {
            continue;
        }
        let mut octets = [0u8; MAC_ADDRESS_LEN];
        octets.copy_from_slice(value);
        if octets != [0u8; MAC_ADDRESS_LEN] {
            return Some(octets);
        }
    }
    None
}

/// Derive the device class from the node's own data, most authoritative
/// source first: `device_type` (the spec keeps it for `memory` and `cpu`),
/// the `interrupt-controller` marker property, then the spec-recommended
/// generic node-name stem. Anything else is honestly
/// [`HwDeviceClass::Other`] — the class is advisory; binding is by match
/// key.
fn classify(node: &Node<'_>) -> HwDeviceClass {
    if let Some(device_type) = node.property("device_type") {
        match device_type.iter_strings().next() {
            Some(b"memory") => return HwDeviceClass::Memory,
            Some(b"cpu") => return HwDeviceClass::Cpu,
            _ => {}
        }
    }
    if node.property("interrupt-controller").is_some() {
        return HwDeviceClass::InterruptController;
    }
    match name_stem(node.name()) {
        b"memory" => HwDeviceClass::Memory,
        b"cpu" => HwDeviceClass::Cpu,
        b"timer" => HwDeviceClass::Timer,
        b"interrupt-controller" | b"intc" | b"gic" | b"plic" => HwDeviceClass::InterruptController,
        b"serial" | b"uart" => HwDeviceClass::Serial,
        b"rtc" => HwDeviceClass::Rtc,
        b"ethernet" => HwDeviceClass::Network,
        b"mmc" | b"sdhci" | b"emmc2" => HwDeviceClass::Storage,
        b"keyboard" | b"mouse" | b"touchscreen" => HwDeviceClass::Input,
        b"display" | b"gpu" | b"hdmi" | b"framebuffer" => HwDeviceClass::Display,
        b"soc" | b"bus" | b"pci" | b"pcie" | b"usb" | b"axi" => HwDeviceClass::Bus,
        _ => HwDeviceClass::Other,
    }
}
