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
//!   windows, a vendor spelling of a generic property). Ports with none
//!   leave the defaults.
//!
//! A port therefore contributes an [`FdtPlatform`] impl and spells its
//! discovery type as [`FdtDiscovery`] over it; the walk itself has one
//! definition. The impl is a *value* built from the tree once
//! ([`FdtPlatform::from_tree`]), so a port whose interrupt mapping depends
//! on a tree-wide fact — the RISC-V PLIC's `riscv,ndev` source count — reads
//! it before the walk rather than per node.
//!
//! The generic DMA binding is read here for every port: a node with
//! `#dma-cells` is a controller carrying a [`DmaControllerDuty`] and its
//! bus's DMA windows, and each `dmas` entry of a consumer becomes a
//! [`DmaRequestLine`] naming the controller's endpoint (`plans/SOUND.md`
//! SND5).

use tairix_abi::driver::dmaengine::{
    DmaControllerDuty, DmaRequestLine, DMA_CONTROLLER_ENDPOINTS, DMA_REQUEST_NAME_MAX,
    DMA_SPECIFIER_MAX_CELLS,
};
use tairix_abi::driver::net::MAC_ADDRESS_LEN;
use tairix_abi::hwtree::BUS_CHILD_ENDPOINTS;
use tairix_abi::{HwDeviceClass, HwMatchKey, HwNode, HwResource, HW_NODE_ROOT, HW_NODE_ROOT_ID};
use tairix_fdt::{
    bus_level, dma_reach, name_stem, phandle_ref, read_cells, reg_entry_count, translated_reg,
    BusLevel, Fdt, Node, NodeIter, MAX_WALK_DEPTH,
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

    /// The phandle of the controller [`Self::interrupt_line`] decodes
    /// specifiers for, read from the tree once.
    ///
    /// A node whose effective interrupt parent is any other controller keeps
    /// none of its specifiers: decoding them as this controller's would grant
    /// its driver another device's line.
    fn root_interrupt_controller(&self) -> Option<u32>;

    /// The channels DMA controller `node` leaves to this system, numbered
    /// from the node's own first channel, when the tree states them.
    ///
    /// The default reads the generic [`dma_channel_mask`]; a port whose
    /// vendor binding states the mask another way overrides it.
    fn dma_channel_mask(
        &self,
        node: &Node<'_>,
        _depth: usize,
        _levels: &[BusLevel<'_>],
    ) -> Option<u64> {
        dma_channel_mask(node)
    }

    /// Push any resource only this platform's tree can describe onto a node
    /// the walk has already built. Ports with no board augmentation leave
    /// the default.
    fn augment(&self, _node: &Node<'_>, _depth: usize, _levels: &[BusLevel<'_>], _hw: &mut HwNode) {
    }
}

/// A DMA controller's generic `dma-channel-mask`: bit `n` for its channel
/// `n`, one cell per thirty-two channels, lowest channels first. `None` when
/// the property is absent or not one or two whole cells.
#[must_use]
pub fn dma_channel_mask(node: &Node<'_>) -> Option<u64> {
    let property = node.property("dma-channel-mask")?;
    let low = u64::from(property.read_be_u32(0).ok()?);
    match property.value().len() {
        4 => Some(low),
        8 => Some(low | (u64::from(property.read_be_u32(4).ok()?) << 32)),
        _ => None,
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
        // The interrupt parent the children of the node at each depth inherit.
        let mut interrupt_parents = [None; MAX_WALK_DEPTH];

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
                interrupt_parents[0] =
                    children_interrupt_parent(&node, own_interrupt_parent(&node, None));
                continue;
            }

            let interrupt_parent = own_interrupt_parent(&node, interrupt_parents[depth - 1]);
            let mut ancestor = ancestors[depth - 1];
            let mut accepted = 0;
            if let Some(mut emitted) =
                self.build_node(&node, depth, &levels, ancestor, next_id, interrupt_parent)
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
                        let _ = emitted.push_resource(HwResource::endpoint(
                            BUS_CHILD_ENDPOINTS.endpoint(next_id),
                        ));
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
            interrupt_parents[depth] = children_interrupt_parent(&node, interrupt_parent);
        }

        Ok(())
    }
}

impl<P: FdtPlatform> FdtDiscovery<'_, P> {
    /// Build the hardware-tree node for one device-tree node, or `None` when
    /// the node describes nothing the tree can carry (no representable match
    /// key and not a memory node — the matcher could never bind it).
    fn build_node(
        &self,
        node: &Node<'_>,
        depth: usize,
        levels: &[BusLevel<'_>],
        parent: u32,
        id: u32,
        interrupt_parent: Option<u32>,
    ) -> Option<HwNode> {
        if !is_emitted(node) {
            return None;
        }
        let class = classify(node);
        let mut hw = HwNode::new(id, parent, class);

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
        if interrupt_parent.is_some()
            && interrupt_parent == self.platform.root_interrupt_controller()
        {
            push_irq_resources(&self.platform, node, &mut hw);
        }

        // A NIC's own hardware address, where its node carries one: the
        // standard ethernet-controller binding, so it is read for every node
        // rather than gated on a board.
        if let Some(octets) = local_mac_address(node) {
            let _ = hw.push_resource(HwResource::link_address(octets));
        }

        if class == HwDeviceClass::Dma {
            let channels = self.platform.dma_channel_mask(node, depth, levels);
            if let Ok(duty) =
                DmaControllerDuty::new(DMA_CONTROLLER_ENDPOINTS.endpoint(id), channels)
            {
                if hw.push_resource(HwResource::dma_controller(&duty)).is_ok() {
                    push_dma_windows(depth, levels, &mut hw);
                }
            }
        }
        push_dma_requests(&self.fdt, node, &mut hw);

        self.platform.augment(node, depth, levels, &mut hw);

        Some(hw)
    }
}

/// A node's effective interrupt parent (Devicetree Spec v0.4 §2.4.1): its own
/// `interrupt-parent`, else the one its tree parent hands down. A present but
/// malformed property names no parent rather than inheriting one.
fn own_interrupt_parent(node: &Node<'_>, inherited: Option<u32>) -> Option<u32> {
    match node.property("interrupt-parent") {
        None => inherited,
        Some(property) if property.value().len() == 4 => {
            property.read_be_u32(0).ok().and_then(phandle_ref)
        }
        Some(_) => None,
    }
}

/// The interrupt parent `node`'s children inherit: `node` itself when it is
/// an interrupt controller or nexus, which `#interrupt-cells` marks, else
/// `node`'s own. A node's `#interrupt-cells` never makes it its *own* parent:
/// a nexus's own interrupts go to the controller above it.
fn children_interrupt_parent(node: &Node<'_>, own: Option<u32>) -> Option<u32> {
    if node.property("#interrupt-cells").is_some() {
        node.phandle()
    } else {
        own
    }
}

/// Push the windows a DMA controller at `depth` reaches memory through, each
/// composed through every bus between it and the root and carrying the bus
/// address it starts at. With nothing on the way that translates, it reaches
/// memory untranslated: one unconstrained window. A bus that maps nothing
/// leaves it no window.
fn push_dma_windows(depth: usize, levels: &[BusLevel<'_>], hw: &mut HwNode) {
    if depth == 0 {
        return;
    }
    let Some(reach) = dma_reach(levels, depth) else {
        return;
    };
    match reach.windows() {
        None => {
            let _ = hw.push_resource(HwResource::dma(0, 0));
        }
        Some(windows) => {
            for window in windows {
                let Some(top) = window.cpu.checked_add(window.size) else {
                    continue;
                };
                if hw
                    .push_resource(HwResource::dma_translated(top, window.size, window.bus))
                    .is_err()
                {
                    return;
                }
            }
        }
    }
}

/// A DMA controller a `dmas` entry names: the id the walk gives it and its
/// `#dma-cells`.
#[derive(Copy, Clone)]
struct DmaControllerRef {
    id: u32,
    cells: u32,
}

/// Push one [`DmaRequestLine`] per `dmas` entry, each naming the endpoint of
/// the controller its phandle resolves to and paired with the `dma-names`
/// string in the same position.
///
/// An entry is as wide as its controller's `#dma-cells`, so an entry whose
/// controller cannot be resolved ends the list: nothing after it can be
/// found. An entry wider than a record carries is dropped whole, never
/// truncated, and a name longer than a record holds leaves its line unnamed.
fn push_dma_requests(fdt: &Fdt<'_>, node: &Node<'_>, hw: &mut HwNode) {
    let Some(dmas) = node.property("dmas") else {
        return;
    };
    let value = dmas.value();
    let mut names = node.property("dma-names").map(|p| p.iter_strings());
    // A node's entries usually all name one controller, so the last
    // resolution is kept rather than replayed.
    let mut resolved: Option<(u32, DmaControllerRef)> = None;
    let mut off = 0;
    for index in 0..=u8::MAX {
        if off >= value.len() {
            return;
        }
        let Some(phandle) = be_cell(value, off).and_then(phandle_ref) else {
            return;
        };
        let controller = match resolved {
            Some((known, controller)) if known == phandle => controller,
            _ => {
                let Some(controller) = resolve_dma_controller(fdt, phandle) else {
                    return;
                };
                resolved = Some((phandle, controller));
                controller
            }
        };
        let start = off + CELL_BYTES;
        let Some(end) = usize::try_from(controller.cells)
            .ok()
            .and_then(|cells| cells.checked_mul(CELL_BYTES))
            .and_then(|len| start.checked_add(len))
        else {
            return;
        };
        let Some(specifier_bytes) = value.get(start..end) else {
            return;
        };
        off = end;
        let name = names
            .as_mut()
            .and_then(Iterator::next)
            .filter(|name| name.len() <= DMA_REQUEST_NAME_MAX)
            .unwrap_or_default();
        if specifier_bytes.len() > DMA_SPECIFIER_MAX_CELLS * CELL_BYTES {
            continue;
        }
        let mut specifier = [0u32; DMA_SPECIFIER_MAX_CELLS];
        let (cells, _) = specifier_bytes.as_chunks::<CELL_BYTES>();
        for (slot, cell) in specifier.iter_mut().zip(cells) {
            *slot = u32::from_be_bytes(*cell);
        }
        let cells = cells.len();
        let endpoint = DMA_CONTROLLER_ENDPOINTS.endpoint(controller.id);
        let Ok(line) = DmaRequestLine::new(endpoint, index, &specifier[..cells], name) else {
            continue;
        };
        if hw.push_resource(HwResource::dma_request(&line)).is_err() {
            return;
        }
    }
}

/// The id the walk gives the DMA controller whose phandle is `phandle`, and
/// its `#dma-cells`, found by replaying the walk's own emission rule — so a
/// consumer met before its controller names the id the controller will get.
///
/// One pass over the tree per controller a node names; a controller the walk
/// does not emit has no id, and a node with no `#dma-cells` is no controller.
fn resolve_dma_controller(fdt: &Fdt<'_>, phandle: u32) -> Option<DmaControllerRef> {
    let mut id = HW_NODE_ROOT_ID;
    for node in fdt.nodes() {
        let node = node.ok()?;
        let depth = node.depth() as usize;
        if depth >= MAX_WALK_DEPTH {
            return None;
        }
        if depth == 0 {
            continue;
        }
        let emitted = is_emitted(&node);
        if emitted {
            id = id.checked_add(1)?;
        }
        if node.phandle() == Some(phandle) {
            let cells = node.property("#dma-cells")?;
            if !emitted || cells.value().len() != CELL_BYTES {
                return None;
            }
            return Some(DmaControllerRef {
                id,
                cells: cells.read_be_u32(0).ok()?,
            });
        }
    }
    None
}

/// The big-endian cell at byte `off` of `value`.
fn be_cell(value: &[u8], off: usize) -> Option<u32> {
    let bytes = value.get(off..off.checked_add(CELL_BYTES)?)?;
    Some(u32::from_be_bytes(bytes.try_into().ok()?))
}

/// Whether the walk emits a hardware-tree node for this device-tree node.
///
/// A node with no representable match key and no memory `device_type` is one
/// the matcher could never bind, so it is spliced out. The single definition
/// of that rule: [`FdtDiscovery::build_node`] applies it, and the bus-child
/// look-ahead and [`resolve_dma_controller`] replay it to predict the ids the
/// walk assigns — a second spelling would let them disagree and pair a chip or
/// a DMA consumer with another node's endpoint.
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
            .push_resource(HwResource::bus_child(
                BUS_CHILD_ENDPOINTS.endpoint(id),
                address,
            ))
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
/// the `#dma-cells` a DMA controller's binding requires, the
/// `interrupt-controller` marker property, then the spec-recommended
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
    if node.property("#dma-cells").is_some() {
        return HwDeviceClass::Dma;
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
        // The three generic names the devicetree spec offers for a device
        // that computes rather than moves: a crypto offload, a signal
        // processor, a media decode/encode engine.
        b"crypto" | b"dsp" | b"video-codec" => HwDeviceClass::Accelerator,
        // The devicetree names for a sound device: the controller itself, a
        // codec behind it, and the machine-level graph that binds the two.
        b"sound" | b"audio" | b"codec" | b"i2s" => HwDeviceClass::Audio,
        b"soc" | b"bus" | b"pci" | b"pcie" | b"usb" | b"axi" => HwDeviceClass::Bus,
        _ => HwDeviceClass::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::{FdtDiscovery, FdtPlatform};
    use crate::platform::{DiscoveryError, HwNodeSink, PlatformDiscovery};
    use std::vec::Vec;
    use tairix_abi::driver::dmaengine::{
        DmaControllerDuty, DmaRequestLine, DMA_CONTROLLER_ENDPOINTS,
    };
    use tairix_abi::hwtree::BUS_CHILD_ENDPOINTS;
    use tairix_abi::{HwDeviceClass, HwNode, HwResource, HwResourceKind, HW_NODE_MAX_RESOURCES};
    use tairix_fdt::fixture::DtbBuilder;
    use tairix_fdt::{BusLevel, Fdt, Node};

    /// The phandle every fixture gives its root interrupt controller.
    const ROOT_INTC: u32 = 1;

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

        fn root_interrupt_controller(&self) -> Option<u32> {
            Some(ROOT_INTC)
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
        b.prop_u32("interrupt-parent", ROOT_INTC);
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

    /// The three devicetree generic names for a device that computes rather
    /// than moves are classed [`HwDeviceClass::Accelerator`]. Without the
    /// class an offload engine is discovered as `Other`, so nothing above
    /// discovery can tell it apart from an unmodelled device.
    #[test]
    fn an_offload_engine_node_is_classed_as_an_accelerator() {
        let mut b = DtbBuilder::new();
        b.begin_node("");
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 2);
        for (name, compatible) in [
            ("crypto@ff8b0000", "vendor,crypto"),
            ("dsp@ff8c0000", "vendor,dsp"),
            ("video-codec@ff8d0000", "vendor,vpu"),
            // A name outside the generic set stays honestly unmodelled.
            ("widget@ff8e0000", "vendor,widget"),
        ] {
            b.begin_node(name);
            b.prop_str("compatible", compatible);
            b.prop(
                "reg",
                &[
                    &0xFF8B_0000u64.to_be_bytes()[..],
                    &0x1000u64.to_be_bytes()[..],
                ]
                .concat(),
            );
            b.end_node();
        }
        b.end_node();
        let nodes = discover(&b.build());
        for compatible in [
            &b"vendor,crypto"[..],
            &b"vendor,dsp"[..],
            &b"vendor,vpu"[..],
        ] {
            assert_eq!(
                by_key(&nodes, compatible).class(),
                Some(HwDeviceClass::Accelerator),
                "{}",
                core::str::from_utf8(compatible).unwrap_or("?")
            );
        }
        assert_eq!(
            by_key(&nodes, b"vendor,widget").class(),
            Some(HwDeviceClass::Other)
        );
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
                (BUS_CHILD_ENDPOINTS.endpoint(ds3231.id()), 0x68),
                (BUS_CHILD_ENDPOINTS.endpoint(pcf.id()), 0x51),
            ]
        );
        // The authority half names only the endpoint: a chip driver never
        // learns a bus address, so it cannot address a neighbour.
        assert_eq!(
            endpoints(ds3231),
            std::vec![BUS_CHILD_ENDPOINTS.endpoint(ds3231.id())]
        );
        assert_eq!(
            endpoints(pcf),
            std::vec![BUS_CHILD_ENDPOINTS.endpoint(pcf.id())]
        );
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
                std::vec![BUS_CHILD_ENDPOINTS.endpoint(child.id())]
            } else {
                Vec::new()
            };
            assert_eq!(endpoints(child), expected, "child {index}");
        }
        for (endpoint, child) in served.iter().zip(children.iter()) {
            assert_eq!(*endpoint, BUS_CHILD_ENDPOINTS.endpoint(child.id()));
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
                (BUS_CHILD_ENDPOINTS.endpoint(mux.id()), 0x70),
                (BUS_CHILD_ENDPOINTS.endpoint(ds3231.id()), 0x68),
            ]
        );
        assert_eq!(
            endpoints(mux),
            std::vec![BUS_CHILD_ENDPOINTS.endpoint(mux.id())]
        );
        assert_eq!(
            endpoints(ds3231),
            std::vec![BUS_CHILD_ENDPOINTS.endpoint(ds3231.id())]
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

    fn irqs(node: &HwNode) -> Vec<u64> {
        node.resources()
            .iter()
            .filter(|r| r.kind() == Some(HwResourceKind::Irq))
            .map(HwResource::base)
            .collect()
    }

    /// A device carrying `interrupts = <line>` and, optionally, its own
    /// `interrupt-parent`.
    fn device(b: &mut DtbBuilder, name: &str, compatible: &str, line: u32, parent: Option<u32>) {
        b.begin_node(name);
        b.prop_str("compatible", compatible);
        b.prop("interrupts", &line.to_be_bytes());
        if let Some(parent) = parent {
            b.prop_u32("interrupt-parent", parent);
        }
        b.end_node();
    }

    /// A tree whose root names the root controller, with a nested
    /// one-cell controller (phandle 2) that has devices of its own.
    fn nested_interrupt_tree() -> Vec<u8> {
        let mut b = DtbBuilder::new();
        b.begin_node("");
        b.prop_u32("#address-cells", 1);
        b.prop_u32("#size-cells", 1);
        b.prop_u32("interrupt-parent", ROOT_INTC);
        b.begin_node("intc");
        b.prop_str("compatible", "test,root-intc");
        b.prop("interrupt-controller", &[]);
        b.prop_u32("#interrupt-cells", 1);
        b.prop_u32("phandle", ROOT_INTC);
        b.end_node();
        // A nested controller: its own line goes to the root controller,
        // its children's to it.
        b.begin_node("gpio");
        b.prop_str("compatible", "test,gpio");
        b.prop("interrupts", &40u32.to_be_bytes());
        b.prop("interrupt-controller", &[]);
        b.prop_u32("#interrupt-cells", 1);
        b.prop_u32("phandle", 2);
        device(&mut b, "button", "test,button", 3, None);
        b.end_node();
        device(&mut b, "inherits", "test,inherits", 50, None);
        device(&mut b, "rewired", "test,rewired", 0, Some(2));
        device(&mut b, "named-root", "test,named-root", 51, Some(ROOT_INTC));
        b.begin_node("malformed");
        b.prop_str("compatible", "test,malformed");
        b.prop("interrupts", &52u32.to_be_bytes());
        b.prop("interrupt-parent", &[0, 1]);
        b.end_node();
        b.end_node();
        b.build()
    }

    #[test]
    fn a_specifier_is_mapped_only_under_the_root_interrupt_controller() {
        let nodes = discover(&nested_interrupt_tree());
        assert_eq!(irqs(by_key(&nodes, b"test,inherits")), std::vec![50]);
        assert_eq!(irqs(by_key(&nodes, b"test,named-root")), std::vec![51]);
        // A nested controller's own line is the root controller's.
        assert_eq!(irqs(by_key(&nodes, b"test,gpio")), std::vec![40]);
        // Its children's lines, and a device wired to it by phandle, are
        // numbers in the nested controller's space: decoding them as the
        // root's would grant another device's line.
        assert!(irqs(by_key(&nodes, b"test,button")).is_empty());
        assert!(irqs(by_key(&nodes, b"test,rewired")).is_empty());
        // A malformed reference names no parent rather than inheriting one.
        assert!(irqs(by_key(&nodes, b"test,malformed")).is_empty());
    }

    #[test]
    fn a_tree_that_names_no_interrupt_parent_maps_no_specifier() {
        let mut b = DtbBuilder::new();
        b.begin_node("");
        b.prop_u32("#address-cells", 1);
        b.prop_u32("#size-cells", 1);
        device(&mut b, "orphan", "test,orphan", 9, None);
        b.end_node();
        let nodes = discover(&b.build());
        assert!(irqs(by_key(&nodes, b"test,orphan")).is_empty());
    }

    /// The legacy controller's `/soc` in the shape of the pinned Pi 4 tree:
    /// a one-cell bus under a two-cell root, translating the peripherals and
    /// reaching RAM and the peripherals through two `dma-ranges` windows.
    /// A consumer sits ahead of its controller in document order, so its
    /// requests name an id the walk has not yet assigned.
    fn dma_tree() -> Vec<u8> {
        let cells =
            |values: &[u32]| -> Vec<u8> { values.iter().flat_map(|v| v.to_be_bytes()).collect() };
        let mut b = DtbBuilder::new();
        b.begin_node("");
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 1);
        b.prop_u32("interrupt-parent", ROOT_INTC);
        b.begin_node("soc");
        b.prop_str("compatible", "simple-bus");
        b.prop_u32("#address-cells", 1);
        b.prop_u32("#size-cells", 1);
        b.prop(
            "ranges",
            &cells(&[0x7e00_0000, 0, 0xfe00_0000, 0x0180_0000]),
        );
        b.prop(
            "dma-ranges",
            &cells(&[
                0xc000_0000,
                0,
                0,
                0x4000_0000,
                0x7c00_0000,
                0,
                0xfc00_0000,
                0x0380_0000,
            ]),
        );
        b.begin_node("i2s@7e203000");
        b.prop_str("compatible", "brcm,bcm2835-i2s");
        b.prop("reg", &cells(&[0x7e20_3000, 0x24]));
        b.prop("dmas", &cells(&[0x0c, 2, 0x0c, 3]));
        b.prop("dma-names", b"tx\0rx\0");
        b.end_node();
        b.begin_node("dma-controller@7e007000");
        b.prop_str("compatible", "brcm,bcm2835-dma");
        b.prop("reg", &cells(&[0x7e00_7000, 0xb00]));
        b.prop(
            "interrupts",
            &cells(&[80, 81, 82, 83, 84, 85, 86, 87, 87, 88, 88]),
        );
        b.prop_u32("#dma-cells", 1);
        b.prop_u32("dma-channel-mask", 0x7f5);
        b.prop_u32("phandle", 0x0c);
        b.end_node();
        b.begin_node("mmc@7e202000");
        b.prop_str("compatible", "brcm,bcm2835-sdhost");
        b.prop("reg", &cells(&[0x7e20_2000, 0x100]));
        b.prop("dmas", &cells(&[0x0c, 0x2000_000d]));
        b.prop("dma-names", b"rx-tx-and-more\0");
        b.end_node();
        // A controller whose binding is wider than a record: its entries are
        // dropped, but the entry after one still parses.
        b.begin_node("wide-dma");
        b.prop_str("compatible", "test,wide-dma");
        b.prop_u32("#dma-cells", 3);
        b.prop_u32("phandle", 0x20);
        b.end_node();
        b.begin_node("mixed");
        b.prop_str("compatible", "test,mixed");
        b.prop("dmas", &cells(&[0x20, 1, 2, 3, 0x0c, 6]));
        b.prop("dma-names", b"wide\0narrow\0");
        b.end_node();
        b.begin_node("dangling");
        b.prop_str("compatible", "test,dangling");
        b.prop("dmas", &cells(&[0x0c, 7, 0x99, 1, 0x0c, 8]));
        b.end_node();
        b.end_node();
        b.end_node();
        b.build()
    }

    fn requests(node: &HwNode) -> Vec<DmaRequestLine> {
        node.resources()
            .iter()
            .filter_map(|r| r.dma_request_line().ok())
            .collect()
    }

    #[test]
    fn a_dma_controller_carries_its_duty_and_each_window_it_reaches_memory_through() {
        let nodes = discover(&dma_tree());
        let dma = by_key(&nodes, b"brcm,bcm2835-dma");
        assert_eq!(dma.class(), Some(HwDeviceClass::Dma));
        let duties: Vec<DmaControllerDuty> = dma
            .resources()
            .iter()
            .filter_map(|r| r.dma_controller_duty().ok())
            .collect();
        assert_eq!(
            duties,
            std::vec![DmaControllerDuty::new(
                DMA_CONTROLLER_ENDPOINTS.endpoint(dma.id()),
                Some(0x7f5)
            )
            .expect("valid")]
        );
        let windows: Vec<HwResource> = dma
            .resources()
            .iter()
            .copied()
            .filter(|r| r.kind() == Some(HwResourceKind::Dma))
            .collect();
        assert_eq!(
            windows,
            std::vec![
                HwResource::dma_translated(0x4000_0000, 0x4000_0000, 0xc000_0000),
                HwResource::dma_translated(0xff80_0000, 0x0380_0000, 0x7c00_0000),
            ]
        );
        // Every line fits: the window, eleven lines, the duty and two windows.
        assert_eq!(irqs(dma).len(), 11);
        assert_eq!(dma.resources().len(), 15);
        assert!(dma
            .resources()
            .iter()
            .any(|r| r.kind() == Some(HwResourceKind::Mmio) && r.base() == 0xfe00_7000));
    }

    #[test]
    fn a_controller_below_an_identity_bus_reaches_memory_through_the_soc_above_it() {
        let cells =
            |values: &[u32]| -> Vec<u8> { values.iter().flat_map(|v| v.to_be_bytes()).collect() };
        let mut b = DtbBuilder::new();
        b.begin_node("");
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 1);
        b.begin_node("soc");
        b.prop_str("compatible", "simple-bus");
        b.prop_u32("#address-cells", 1);
        b.prop_u32("#size-cells", 1);
        b.prop("ranges", &[]);
        b.prop("dma-ranges", &cells(&[0xc000_0000, 0, 0, 0x4000_0000]));
        b.begin_node("sub");
        b.prop_str("compatible", "simple-bus");
        b.prop_u32("#address-cells", 1);
        b.prop_u32("#size-cells", 1);
        b.prop("ranges", &[]);
        b.prop("dma-ranges", &[]);
        b.begin_node("dma-controller@7e007000");
        b.prop_str("compatible", "brcm,bcm2835-dma");
        b.prop("reg", &cells(&[0x7e00_7000, 0xb00]));
        b.prop_u32("#dma-cells", 1);
        b.end_node();
        b.end_node();
        b.end_node();
        b.end_node();
        let nodes = discover(&b.build());
        let windows: Vec<HwResource> = by_key(&nodes, b"brcm,bcm2835-dma")
            .resources()
            .iter()
            .copied()
            .filter(|r| r.kind() == Some(HwResourceKind::Dma))
            .collect();
        assert_eq!(
            windows,
            std::vec![HwResource::dma_translated(
                0x4000_0000,
                0x4000_0000,
                0xc000_0000
            )]
        );
    }

    #[test]
    fn a_consumer_names_its_controller_even_before_the_walk_reaches_it() {
        let nodes = discover(&dma_tree());
        let dma = by_key(&nodes, b"brcm,bcm2835-dma");
        let i2s = by_key(&nodes, b"brcm,bcm2835-i2s");
        assert!(i2s.id() < dma.id());
        let endpoint = DMA_CONTROLLER_ENDPOINTS.endpoint(dma.id());
        assert_eq!(
            requests(i2s),
            std::vec![
                DmaRequestLine::new(endpoint, 0, &[2], b"tx").expect("valid"),
                DmaRequestLine::new(endpoint, 1, &[3], b"rx").expect("valid"),
            ]
        );
        // A name longer than a record holds leaves the line unnamed rather
        // than truncated; the specifier's serving bits ride along whole.
        let mmc = by_key(&nodes, b"brcm,bcm2835-sdhost");
        assert_eq!(
            requests(mmc),
            std::vec![DmaRequestLine::new(endpoint, 0, &[0x2000_000d], b"").expect("valid")]
        );
    }

    #[test]
    fn an_entry_wider_than_a_record_is_dropped_and_a_dangling_one_ends_the_list() {
        let nodes = discover(&dma_tree());
        let endpoint = DMA_CONTROLLER_ENDPOINTS.endpoint(by_key(&nodes, b"brcm,bcm2835-dma").id());
        // The three-cell entry is dropped whole; the next keeps its position
        // and its own name.
        assert_eq!(
            requests(by_key(&nodes, b"test,mixed")),
            std::vec![DmaRequestLine::new(endpoint, 1, &[6], b"narrow").expect("valid")]
        );
        // Past an unresolvable phandle nothing can be found, not even the
        // well-formed entry after it.
        assert_eq!(
            requests(by_key(&nodes, b"test,dangling")),
            std::vec![DmaRequestLine::new(endpoint, 0, &[7], b"").expect("valid")]
        );
        // The wide controller is still a controller, with its own duty.
        let wide = by_key(&nodes, b"test,wide-dma");
        assert_eq!(wide.class(), Some(HwDeviceClass::Dma));
    }

    #[test]
    fn a_controller_off_any_translating_bus_reaches_memory_untranslated() {
        let mut b = DtbBuilder::new();
        b.begin_node("");
        b.prop_u32("#address-cells", 1);
        b.prop_u32("#size-cells", 1);
        b.begin_node("top-dma");
        b.prop_str("compatible", "test,top-dma");
        b.prop_u32("#dma-cells", 1);
        b.end_node();
        b.begin_node("identity-bus");
        b.prop_str("compatible", "simple-bus");
        b.prop("ranges", &[]);
        b.prop("dma-ranges", &[]);
        b.begin_node("identity-dma");
        b.prop_str("compatible", "test,identity-dma");
        b.prop_u32("#dma-cells", 1);
        b.end_node();
        b.end_node();
        b.begin_node("opaque-bus");
        b.prop_str("compatible", "simple-bus");
        b.prop("ranges", &[]);
        b.begin_node("opaque-dma");
        b.prop_str("compatible", "test,opaque-dma");
        b.prop_u32("#dma-cells", 1);
        b.end_node();
        b.end_node();
        b.end_node();
        let nodes = discover(&b.build());
        let windows = |compatible: &[u8]| -> Vec<HwResource> {
            by_key(&nodes, compatible)
                .resources()
                .iter()
                .copied()
                .filter(|r| r.kind() == Some(HwResourceKind::Dma))
                .collect()
        };
        assert_eq!(windows(b"test,top-dma"), std::vec![HwResource::dma(0, 0)]);
        assert_eq!(
            windows(b"test,identity-dma"),
            std::vec![HwResource::dma(0, 0)]
        );
        // A bus with no `dma-ranges` maps nothing for its children.
        assert!(windows(b"test,opaque-dma").is_empty());
        // No mask stated, so the duty says so.
        let duty = by_key(&nodes, b"test,top-dma")
            .resources()
            .iter()
            .find_map(|r| r.dma_controller_duty().ok())
            .expect("a duty");
        assert_eq!(duty.channels(), None);
    }

    #[test]
    fn the_generic_channel_mask_reads_one_cell_per_thirty_two_channels() {
        let mut b = DtbBuilder::new();
        b.begin_node("");
        for (name, value) in [
            ("one", &[0u8, 0, 0x07, 0xf5][..]),
            ("two", &[0, 0, 0, 1, 0x80, 0, 0, 0][..]),
            ("ragged", &[0, 0, 1][..]),
        ] {
            b.begin_node(name);
            b.prop("dma-channel-mask", value);
            b.end_node();
        }
        b.begin_node("none");
        b.end_node();
        b.end_node();
        let blob = b.build();
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let masks: Vec<Option<u64>> = fdt
            .nodes()
            .skip(1)
            .map(|n| super::dma_channel_mask(&n.expect("well formed")))
            .collect();
        assert_eq!(
            masks,
            std::vec![Some(0x7f5), Some(0x8000_0000_0000_0001), None, None]
        );
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
