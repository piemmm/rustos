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
use tairix_abi::hwtree::bus_child_endpoint;
use tairix_abi::{HwDeviceClass, HwMatchKey, HwNode, HwResource, HW_NODE_ROOT, HW_NODE_ROOT_ID};
use tairix_fdt::{
    bus_level, name_stem, read_cells, reg_entry_count, translated_reg, BusLevel, Fdt, Node,
    NodeIter, MAX_WALK_DEPTH,
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
        // facts: the hardware-tree id of the nearest *emitted* ancestor,
        // which is the parent a child at depth + 1 names, and the
        // bus-child bookkeeping of the node at that depth — how many
        // duties its resource list took, and how many of its addressed
        // children have been reached. A tree nested beyond the tracked
        // depth is refused as malformed rather than silently
        // under-enumerated.
        let mut levels = [BusLevel::DEFAULT; MAX_WALK_DEPTH];
        let mut ancestors = [0u32; MAX_WALK_DEPTH];
        let mut duties = [0usize; MAX_WALK_DEPTH];
        let mut children_seen = [0usize; MAX_WALK_DEPTH];

        let mut nodes = self.fdt.nodes();
        while let Some(node) = nodes.next() {
            // Cloned before anything else so the look-ahead below starts
            // exactly where this node's subtree does.
            let subtree = nodes.clone();
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
                duties[0] = 0;
                children_seen[0] = 0;
                continue;
            }

            let mut ancestor = ancestors[depth - 1];
            let mut accepted = 0;
            if let Some(mut emitted) =
                build_node(&self.platform, &node, depth, &levels, ancestor, next_id)
            {
                // A child of an addressed, non-enumerable bus carries the
                // *authority* half of its existence: an endpoint grant
                // naming the id its bus driver will serve it on. The index
                // counts only *emitted* children, exactly as the duty
                // look-ahead did, and a child past what the bus node could
                // hold gets none — so the two halves can never disagree.
                if is_bus_child(&node, depth, &levels) {
                    let index = children_seen[depth - 1];
                    children_seen[depth - 1] = index + 1;
                    if index < duties[depth - 1] {
                        let _ = emitted
                            .push_resource(HwResource::endpoint(bus_child_endpoint(next_id)));
                    }
                }
                // The *duty* half: this node's own children, if it is such
                // a bus. Their ids are the ones this walk is about to
                // assign, so they are read ahead here — the emitted parent
                // cannot be amended once the sink has it.
                if declares_addressed_bus(&level) {
                    accepted = push_bus_child_duties(subtree, depth, next_id, &mut emitted);
                }
                sink.emit(emitted)?;
                ancestor = next_id;
                next_id = next_id
                    .checked_add(1)
                    .ok_or(DiscoveryError::MalformedSource)?;
            }
            levels[depth] = level;
            ancestors[depth] = ancestor;
            duties[depth] = accepted;
            children_seen[depth] = 0;
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
    if !is_emitted(node) {
        return None;
    }
    let mut hw = HwNode::new(id, parent, classify(node));

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
        }
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

/// Whether the walk emits a hardware-tree node for this device-tree node.
///
/// A node with no representable match key and no memory `device_type` is one
/// the matcher could never bind, so it is spliced out. The single definition
/// of that rule: [`build_node`] applies it, and the bus-child look-ahead
/// replays it to predict the ids the walk is about to assign — a second
/// spelling would let the two disagree and mis-pair a chip with its endpoint.
fn is_emitted(node: &Node<'_>) -> bool {
    if classify(node) == HwDeviceClass::Memory {
        return true;
    }
    node.property("compatible").is_some_and(|compat| {
        compat
            .iter_strings()
            .any(|s| HwMatchKey::compatible(s).is_ok())
    })
}

/// Whether a node's own cell counts declare it an **addressed,
/// non-enumerable bus**: one address cell and no size cells, so a child's
/// single `reg` cell is its address *on this bus* rather than a window in
/// the parent's address space (Devicetree spec v0.4 §2.3.5).
///
/// I²C, SPI chip-select, and 1-Wire all spell themselves this way, so the
/// walk recognises the convention rather than any board or bus name.
fn declares_addressed_bus(level: &BusLevel<'_>) -> bool {
    level.addr_cells == 1 && level.size_cells == 0
}

/// Whether `node` is an addressed child of such a bus.
///
/// The `/cpus` container uses the same cell convention for CPU numbering
/// (Devicetree spec v0.4 §3.7), so a CPU node is excluded: its `reg` is a
/// hart/MPIDR identifier, not a device address on a transfer bus.
fn is_bus_child(node: &Node<'_>, depth: usize, levels: &[BusLevel<'_>]) -> bool {
    depth
        .checked_sub(1)
        .and_then(|parent| levels.get(parent))
        .is_some_and(declares_addressed_bus)
        && classify(node) != HwDeviceClass::Cpu
        && bus_child_address(node).is_some()
}

/// The address an addressed bus child answers to: the first cell of its
/// `reg`, which its parent's `#address-cells = <1>` declares to be exactly
/// one cell wide.
fn bus_child_address(node: &Node<'_>) -> Option<u64> {
    read_cells(node.property("reg")?.value(), 0, 1)
}

/// Push one [`HwResource::bus_child`] duty per addressed child of the bus
/// node being built, and report how many its resource list could hold.
///
/// `subtree` is the walk's own iterator cloned just after the bus node, and
/// `bus_id` the id the walk is assigning that node — so replaying
/// [`is_emitted`] over the subtree in document order yields exactly the ids
/// the walk will assign, and a child's duty here names the same endpoint its
/// own node will later claim.
///
/// A child past the node's resource capacity is left without a duty (and so,
/// by the caller's matching bound, without an endpoint): a bus whose tree
/// declares more children than one node can carry serves the ones it can and
/// leaves the rest unbound, rather than handing a chip driver authority no
/// bus driver was told to serve.
fn push_bus_child_duties(
    subtree: NodeIter<'_>,
    bus_depth: usize,
    bus_id: u32,
    hw: &mut HwNode,
) -> usize {
    let mut accepted = 0;
    let mut next_id = bus_id;
    for child in subtree {
        let Ok(child) = child else { return accepted };
        let depth = child.depth() as usize;
        if depth <= bus_depth {
            return accepted;
        }
        if !is_emitted(&child) {
            continue;
        }
        let Some(id) = next_id.checked_add(1) else {
            return accepted;
        };
        next_id = id;
        if depth != bus_depth + 1 || classify(&child) == HwDeviceClass::Cpu {
            continue;
        }
        let Some(address) = bus_child_address(&child) else {
            continue;
        };
        if hw
            .push_resource(HwResource::bus_child(bus_child_endpoint(id), address))
            .is_err()
        {
            return accepted;
        }
        accepted += 1;
    }
    accepted
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

#[cfg(test)]
mod tests {
    use super::{FdtDiscovery, FdtPlatform};
    use crate::platform::{DiscoveryError, HwNodeSink, PlatformDiscovery};
    use std::vec::Vec;
    use tairix_abi::hwtree::bus_child_endpoint;
    use tairix_abi::{HwNode, HwResource, HwResourceKind, HW_NODE_MAX_RESOURCES};
    use tairix_fdt::fixture::DtbBuilder;
    use tairix_fdt::{BusLevel, Fdt, Node};

    /// The smallest honest port: one interrupt cell mapped straight through,
    /// no board augmentation. Enough to exercise the shared walk.
    struct BarePlatform;

    impl FdtPlatform for BarePlatform {
        const INTERRUPT_CELLS: usize = 1;

        fn from_tree(_fdt: &Fdt<'_>) -> Self {
            Self
        }

        fn interrupt_line(&self, specifier: &[u8]) -> Option<u32> {
            let bytes: [u8; 4] = specifier.try_into().ok()?;
            Some(u32::from_be_bytes(bytes))
        }
    }

    #[derive(Default)]
    struct CollectingSink {
        nodes: Vec<HwNode>,
    }

    impl HwNodeSink for CollectingSink {
        fn emit(&mut self, node: HwNode) -> Result<(), DiscoveryError> {
            self.nodes.push(node);
            Ok(())
        }
    }

    fn discover(blob: &[u8]) -> Vec<HwNode> {
        let fdt = Fdt::new(blob).expect("valid fdt");
        let mut sink = CollectingSink::default();
        FdtDiscovery::<BarePlatform>::new(fdt)
            .discover(&mut sink)
            .expect("discovery succeeds");
        sink.nodes
    }

    fn by_key<'a>(nodes: &'a [HwNode], compatible: &[u8]) -> &'a HwNode {
        nodes
            .iter()
            .find(|n| {
                n.match_keys()
                    .iter()
                    .any(|k| k.compatible_bytes() == compatible)
            })
            .unwrap_or_else(|| panic!("a node matching {compatible:?}"))
    }

    fn duties(node: &HwNode) -> Vec<(u64, u64)> {
        node.resources()
            .iter()
            .filter_map(HwResource::bus_child_pair)
            .collect()
    }

    fn endpoints(node: &HwNode) -> Vec<u64> {
        node.resources()
            .iter()
            .filter(|r| r.kind() == Some(HwResourceKind::Endpoint))
            .map(HwResource::base)
            .collect()
    }

    /// A tree with one memory-mapped I²C controller carrying two addressed
    /// children plus, optionally, `extra` further children at successive
    /// addresses (to overrun the node's resource capacity).
    fn i2c_tree(extra: u32) -> Vec<u8> {
        let mut b = DtbBuilder::new();
        b.begin_node("");
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 2);
        b.begin_node("memory@40000000");
        b.prop_str("device_type", "memory");
        b.prop(
            "reg",
            &[
                &0x4000_0000u64.to_be_bytes()[..],
                &0x1000_0000u64.to_be_bytes()[..],
            ]
            .concat(),
        );
        b.end_node();
        b.begin_node("i2c@fe804000");
        b.prop_str("compatible", "brcm,bcm2835-i2c");
        b.prop(
            "reg",
            &[
                &0xFE80_4000u64.to_be_bytes()[..],
                &0x200u64.to_be_bytes()[..],
            ]
            .concat(),
        );
        b.prop("interrupts", &53u32.to_be_bytes());
        b.prop_u32("#address-cells", 1);
        b.prop_u32("#size-cells", 0);
        b.begin_node("rtc@68");
        b.prop_str("compatible", "maxim,ds3231");
        b.prop("reg", &0x68u32.to_be_bytes());
        b.end_node();
        b.begin_node("rtc@51");
        b.prop_str("compatible", "nxp,pcf85063a");
        b.prop("reg", &0x51u32.to_be_bytes());
        b.end_node();
        for i in 0..extra {
            let address = 0x10 + i;
            b.begin_node("sensor");
            b.prop_str("compatible", "vendor,sensor");
            b.prop("reg", &address.to_be_bytes());
            b.end_node();
        }
        b.end_node();
        b.end_node();
        b.build()
    }

    #[test]
    fn a_bus_child_gets_the_endpoint_its_parent_was_given_the_duty_for() {
        let nodes = discover(&i2c_tree(0));
        let bus = by_key(&nodes, b"brcm,bcm2835-i2c");
        let ds3231 = by_key(&nodes, b"maxim,ds3231");
        let pcf = by_key(&nodes, b"nxp,pcf85063a");

        // The duty half names each child's endpoint *and* its bus address.
        assert_eq!(
            duties(bus),
            std::vec![
                (bus_child_endpoint(ds3231.id()), 0x68),
                (bus_child_endpoint(pcf.id()), 0x51),
            ]
        );
        // The authority half names only the endpoint: a chip driver never
        // learns a bus address, so it cannot address a neighbour.
        assert_eq!(
            endpoints(ds3231),
            std::vec![bus_child_endpoint(ds3231.id())]
        );
        assert_eq!(endpoints(pcf), std::vec![bus_child_endpoint(pcf.id())]);
        assert!(duties(ds3231).is_empty());
        // The two halves agree, and the two children never share an id.
        assert_ne!(ds3231.id(), pcf.id());
    }

    #[test]
    fn a_bus_child_gets_no_memory_window_from_its_bus_address() {
        let nodes = discover(&i2c_tree(0));
        for compatible in [&b"maxim,ds3231"[..], b"nxp,pcf85063a"] {
            let child = by_key(&nodes, compatible);
            assert!(
                child
                    .resources()
                    .iter()
                    .all(|r| r.kind() == Some(HwResourceKind::Endpoint)),
                "{compatible:?} must carry nothing but its endpoint"
            );
        }
        // The bus itself still gets its own window and line.
        let bus = by_key(&nodes, b"brcm,bcm2835-i2c");
        assert!(bus
            .resources()
            .iter()
            .any(|r| r.kind() == Some(HwResourceKind::Mmio) && r.base() == 0xFE80_4000));
        assert!(bus
            .resources()
            .iter()
            .any(|r| r.kind() == Some(HwResourceKind::Irq) && r.base() == 53));
    }

    #[test]
    fn a_child_whose_duty_did_not_fit_is_left_without_authority() {
        // The bus already spends two resource slots on its window and line,
        // so past that the node cannot hold every duty.
        let extra = u32::try_from(HW_NODE_MAX_RESOURCES).expect("small");
        let nodes = discover(&i2c_tree(extra));
        let bus = by_key(&nodes, b"brcm,bcm2835-i2c");
        let granted = duties(bus);
        assert_eq!(granted.len(), HW_NODE_MAX_RESOURCES - 2);

        // Every child the bus was told to serve holds exactly the matching
        // endpoint, and every child past the bound holds none — the halves
        // never disagree.
        let served: Vec<u64> = granted.iter().map(|(endpoint, _)| *endpoint).collect();
        let mut children: Vec<&HwNode> = nodes.iter().filter(|n| n.id() > bus.id()).collect();
        children.sort_by_key(|n| n.id());
        assert!(children.len() > served.len());
        for (index, child) in children.iter().enumerate() {
            let expected: Vec<u64> = if index < served.len() {
                std::vec![bus_child_endpoint(child.id())]
            } else {
                Vec::new()
            };
            assert_eq!(endpoints(child), expected, "child {index}");
        }
        for (endpoint, child) in served.iter().zip(children.iter()) {
            assert_eq!(*endpoint, bus_child_endpoint(child.id()));
        }
    }

    #[test]
    fn a_cpu_node_is_not_a_bus_child() {
        // `/cpus` spells itself with the same cell counts an addressed bus
        // does, so a CPU must not be handed a transfer endpoint.
        let mut b = DtbBuilder::new();
        b.begin_node("");
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 2);
        b.begin_node("cpus");
        b.prop_u32("#address-cells", 1);
        b.prop_u32("#size-cells", 0);
        b.begin_node("cpu@0");
        b.prop_str("device_type", "cpu");
        b.prop_str("compatible", "arm,cortex-a72");
        b.prop("reg", &0u32.to_be_bytes());
        b.end_node();
        b.end_node();
        b.end_node();
        let nodes = discover(&b.build());
        let cpu = by_key(&nodes, b"arm,cortex-a72");
        assert!(endpoints(cpu).is_empty());
        assert!(nodes.iter().all(|n| duties(n).is_empty()));
    }

    #[test]
    fn a_bus_whose_own_node_is_unbindable_hands_out_no_authority() {
        // No `compatible` on the bus, so no driver could ever serve it and
        // the walk splices it out; its children must not be left calling an
        // endpoint nothing will bind.
        let mut b = DtbBuilder::new();
        b.begin_node("");
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 2);
        b.begin_node("i2c@fe804000");
        b.prop_u32("#address-cells", 1);
        b.prop_u32("#size-cells", 0);
        b.begin_node("rtc@68");
        b.prop_str("compatible", "maxim,ds3231");
        b.prop("reg", &0x68u32.to_be_bytes());
        b.end_node();
        b.end_node();
        b.end_node();
        let nodes = discover(&b.build());
        assert!(endpoints(by_key(&nodes, b"maxim,ds3231")).is_empty());
    }

    #[test]
    fn an_unbindable_sibling_does_not_shift_the_ids_the_duties_name() {
        // The look-ahead must splice out exactly what the walk does: a
        // child with no representable match key consumes no id, and one
        // nested deeper consumes one without being a duty of this bus.
        let mut b = DtbBuilder::new();
        b.begin_node("");
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 2);
        b.begin_node("i2c@fe804000");
        b.prop_str("compatible", "brcm,bcm2835-i2c");
        b.prop_u32("#address-cells", 1);
        b.prop_u32("#size-cells", 0);
        b.begin_node("unbindable@10");
        b.prop("reg", &0x10u32.to_be_bytes());
        b.end_node();
        b.begin_node("mux@70");
        b.prop_str("compatible", "vendor,mux");
        b.prop("reg", &0x70u32.to_be_bytes());
        b.begin_node("nested");
        b.prop_str("compatible", "vendor,nested");
        b.end_node();
        b.end_node();
        b.begin_node("rtc@68");
        b.prop_str("compatible", "maxim,ds3231");
        b.prop("reg", &0x68u32.to_be_bytes());
        b.end_node();
        b.end_node();
        b.end_node();
        let nodes = discover(&b.build());
        let bus = by_key(&nodes, b"brcm,bcm2835-i2c");
        let mux = by_key(&nodes, b"vendor,mux");
        let ds3231 = by_key(&nodes, b"maxim,ds3231");
        assert_eq!(
            duties(bus),
            std::vec![
                (bus_child_endpoint(mux.id()), 0x70),
                (bus_child_endpoint(ds3231.id()), 0x68),
            ]
        );
        assert_eq!(endpoints(mux), std::vec![bus_child_endpoint(mux.id())]);
        assert_eq!(
            endpoints(ds3231),
            std::vec![bus_child_endpoint(ds3231.id())]
        );
        // The nested grandchild is not this bus's child and gets nothing.
        assert!(endpoints(by_key(&nodes, b"vendor,nested")).is_empty());
    }

    #[test]
    fn an_ordinary_memory_mapped_child_is_untouched() {
        // A `#size-cells = <1>` bus is a memory-mapped one: its children
        // keep their windows and gain no endpoint.
        let mut b = DtbBuilder::new();
        b.begin_node("");
        b.prop_u32("#address-cells", 1);
        b.prop_u32("#size-cells", 1);
        b.begin_node("serial@9000000");
        b.prop_str("compatible", "arm,pl011");
        b.prop(
            "reg",
            &[
                &0x0900_0000u32.to_be_bytes()[..],
                &0x1000u32.to_be_bytes()[..],
            ]
            .concat(),
        );
        b.end_node();
        b.end_node();
        let nodes = discover(&b.build());
        let uart = by_key(&nodes, b"arm,pl011");
        assert!(endpoints(uart).is_empty());
        assert!(uart
            .resources()
            .iter()
            .any(|r| r.kind() == Some(HwResourceKind::Mmio) && r.base() == 0x0900_0000));
    }

    #[test]
    fn the_addressed_bus_convention_is_read_from_the_cell_counts_alone() {
        let addressed = BusLevel {
            addr_cells: 1,
            size_cells: 0,
            ranges: None,
            dma_ranges: None,
        };
        assert!(super::declares_addressed_bus(&addressed));
        assert!(!super::declares_addressed_bus(&BusLevel::DEFAULT));
        assert!(!super::declares_addressed_bus(&BusLevel {
            size_cells: 1,
            ..addressed
        }));
        assert!(!super::declares_addressed_bus(&BusLevel {
            addr_cells: 2,
            ..addressed
        }));
    }

    /// Silence the unused-import warning `Node` would otherwise raise while
    /// still proving the address decode reads exactly one cell.
    #[test]
    fn a_bus_child_address_is_one_cell_wide() {
        let mut b = DtbBuilder::new();
        b.begin_node("");
        b.prop("reg", &[0x00, 0x00, 0x00, 0x68, 0xDE, 0xAD, 0xBE, 0xEF]);
        b.end_node();
        let blob = b.build();
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let root: Node<'_> = fdt.nodes().next().expect("a node").expect("well formed");
        assert_eq!(super::bus_child_address(&root), Some(0x68));
    }
}
