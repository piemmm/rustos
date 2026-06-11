//! aarch64 early-boot platform discovery (`AGENTS.md` §17.2 / §18.2).
//!
//! Implements the Arch HAL
//! [`PlatformDiscovery`](rustos_arch_api::PlatformDiscovery) slice by
//! normalising the flattened device tree the firmware hands the kernel
//! into [`rustos_abi::hwtree`] nodes — **generically**. The walk emits a
//! node for every device the tree describes, with no per-device list to
//! grow (`PLAN.md` Stage 4.HW):
//!
//! * every node carrying a `compatible` property becomes a hardware-tree
//!   node whose match keys are that property's strings, in order (the
//!   devicetree most-specific-first order the `devmgr` bind resolution
//!   relies on, `AGENTS.md` §18.3);
//! * `/memory` nodes (matched by `device_type`/name — they carry no
//!   `compatible`) become `Memory` nodes;
//! * `reg` entries become capability-gated MMIO resources, decoded with
//!   the parent's `#address-cells`/`#size-cells` and translated through
//!   each ancestor bus's `ranges` into CPU-physical addresses — an
//!   untranslatable entry is dropped, never emitted untranslated
//!   (`AGENTS.md` §2.9);
//! * `interrupts` entries become IRQ resources (the three-cell GIC
//!   specifier both supported boards use; the second cell is the
//!   interrupt number);
//! * the device class is derived from the node's data — `device_type`
//!   (`memory`/`cpu`), the `interrupt-controller` marker, or the
//!   spec-recommended generic node-name stem — defaulting to `Other`.
//!
//! Bus interior nodes (e.g. a `simple-bus` `/soc`) are emitted before
//! their children, so the flat stream reconstructs the tree shape
//! (`kernel/arch/api` emission-order contract). Nodes describing no
//! bindable device — no usable match key and not a memory node — are not
//! emitted; the §18.3 matcher could never bind them.
//!
//! The PSCI conduit the tree also carries ([`crate::fdt::psci_method`]) is
//! the prerequisite the aarch64 SMP bring-up (Stage W6) consumes; it is a
//! firmware-call property rather than a device node, so it is exposed
//! through the reader, not as a tree node.

use crate::fdt::{bus_level, reg_entry_count, translated_reg, BusLevel, Fdt, MAX_WALK_DEPTH};
use rustos_abi::{HwDeviceClass, HwMatchKey, HwNode, HwResource, HW_NODE_ROOT};
use rustos_arch_api::{DiscoveryError, HwNodeSink, PlatformDiscovery};
use rustos_fdt::{name_stem, read_cells, Node};

/// Exclusive upper bound of the 30-bit `VideoCore` SDRAM aperture: the
/// highest ARM-physical address (plus one) the BCM2711 firmware can DMA
/// the mailbox property buffer through. Declared on the mailbox node as
/// the DMA resource's address limit so the host carves the buffer below
/// it (`AGENTS.md` §18.1 — a capability-grant request, never an ambient
/// handle).
const VIDEOCORE_APERTURE_LIMIT: u64 = 0x4000_0000;

/// Length of the DMA-visible property-buffer carve the mailbox node
/// requests: one page — the mapping granularity — which comfortably
/// holds the 128-byte, 16-byte-aligned property message the firmware
/// protocol exchanges (`drivers/display/rpi_hvs::mailbox`).
const MAILBOX_DMA_BUFFER_LEN: u64 = 4096;

/// `compatible` string of the BCM283x/BCM2711 `VideoCore` firmware
/// mailbox (the Pi 4 device tree names the BCM2711 doorbell block with
/// the original BCM2835 binding) — the one node whose emission is
/// augmented with the DMA property-buffer carve request above.
const MAILBOX_COMPATIBLE: &[u8] = b"brcm,bcm2835-mbox";

/// Builds the hardware tree from a borrowed flattened device tree.
pub struct FdtDiscovery<'a> {
    fdt: Fdt<'a>,
}

impl<'a> FdtDiscovery<'a> {
    /// Wrap an already-validated [`Fdt`] reader.
    #[must_use]
    pub fn new(fdt: Fdt<'a>) -> Self {
        Self { fdt }
    }
}

impl PlatformDiscovery for FdtDiscovery<'_> {
    fn discover(&self, sink: &mut dyn HwNodeSink) -> Result<(), DiscoveryError> {
        // Root first so every later node's parent is already emitted.
        sink.emit(HwNode::new(0, HW_NODE_ROOT, HwDeviceClass::Root))?;
        let mut next_id: u32 = 1;
        // The shared per-depth bus state (`crate::fdt`, `AGENTS.md` §2.2)
        // plus this walk's own per-depth fact: the hardware-tree id of
        // the nearest *emitted* ancestor — the parent id a child emitted
        // at depth + 1 names. A tree nested beyond `MAX_WALK_DEPTH` is
        // refused as malformed rather than silently under-enumerated
        // (`AGENTS.md` §2.9 / §24.4).
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
                ancestors[0] = 0;
                continue;
            }

            let mut ancestor = ancestors[depth - 1];
            if let Some(emitted) = build_node(&node, depth, &levels, ancestor, next_id) {
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
/// key and not a memory node — the §18.3 matcher could never bind it).
fn build_node(
    node: &Node<'_>,
    depth: usize,
    levels: &[BusLevel<'_>],
    parent: u32,
    id: u32,
) -> Option<HwNode> {
    let class = classify(node);
    let compatible = node.property("compatible");
    let mut hw = HwNode::new(id, parent, class);

    let mut keys = 0usize;
    if let Some(compat) = compatible {
        for s in compat.iter_strings() {
            // A string longer than `HW_COMPATIBLE_MAX` is rejected by the
            // ABI on *both* sides — a driver bind key could never carry
            // it either — so skipping it provably loses no match. Keys
            // past the node capacity are dropped most-specific-first
            // preserved (the devicetree list order).
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
    push_irq_resources(node, &mut hw);

    // The VideoCore firmware mailbox additionally *requests* a DMA
    // property-buffer carve inside the 30-bit aperture (`plans/PI.md`
    // P7) — a capability-grant request the driver host satisfies
    // (`AGENTS.md` §18.1), declared here because only the platform
    // knows the firmware's aperture.
    if compatible.is_some_and(|c| c.iter_strings().any(|s| s == MAILBOX_COMPATIBLE)) {
        let dma = HwResource::dma(VIDEOCORE_APERTURE_LIMIT, MAILBOX_DMA_BUFFER_LEN);
        // A node with no room left simply carries no carve request; the
        // capacity bound is the ABI's, never a panic (`AGENTS.md` §2.9).
        let _ = hw.push_resource(dma);
    }

    Some(hw)
}

/// Decode each `reg` entry with the parent's cell counts, translate it
/// through the ancestor buses' `ranges` (the shared
/// [`crate::fdt::translated_reg`] decoder, `AGENTS.md` §2.2), and push
/// it as an MMIO resource.
///
/// Entries that cannot be decoded (out-of-range cell counts, a length
/// that is not a whole number of entries) or translated (an ancestor bus
/// without usable `ranges`) are dropped — the tree never carries an
/// invented or untranslated window (`AGENTS.md` §2.9). Entries past the
/// node's resource capacity are dropped likewise (a §24.4 ABI bound).
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

/// Push one IRQ resource per `interrupts` specifier.
///
/// Both supported boards (QEMU `virt`, the Pi 4) describe interrupts
/// with the three-cell GIC specifier `<type, number, flags>`; the second
/// cell is the interrupt number the kernel and drivers bind. A value
/// that is not a whole number of specifiers is dropped — never a guessed
/// line (`AGENTS.md` §2.9).
fn push_irq_resources(node: &Node<'_>, hw: &mut HwNode) {
    /// Byte length of one three-cell GIC interrupt specifier.
    const GIC_SPECIFIER_LEN: usize = 12;
    let Some(interrupts) = node.property("interrupts") else {
        return;
    };
    let value = interrupts.value();
    if value.is_empty() || value.len() % GIC_SPECIFIER_LEN != 0 {
        return;
    }
    let mut off = 0;
    while off + GIC_SPECIFIER_LEN <= value.len() {
        let Some(number) = read_cells(value, off + 4, 1) else {
            return;
        };
        if hw.push_resource(HwResource::irq(number, 1)).is_err() {
            return;
        }
        off += GIC_SPECIFIER_LEN;
    }
}

/// Derive the device class from the node's own data, most authoritative
/// source first: `device_type` (the spec keeps it for `memory` and
/// `cpu`), the `interrupt-controller` marker property, then the
/// spec-recommended generic node-name stem. Anything else is honestly
/// [`HwDeviceClass::Other`] — the class is advisory; binding is by match
/// key (`AGENTS.md` §18.3).
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
        b"interrupt-controller" | b"intc" | b"gic" => HwDeviceClass::InterruptController,
        b"serial" | b"uart" => HwDeviceClass::Serial,
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
    use super::FdtDiscovery;
    use crate::fdt::Fdt;
    use rustos_abi::{HwDeviceClass, HwNode, HwResourceKind};
    use rustos_arch_api::platform::{conformance, DiscoveryError, HwNodeSink, PlatformDiscovery};
    use rustos_fdt::fixture::{arm_with_cpus, raspi_like_arm, virt_like_arm, DtbBuilder};
    use std::vec::Vec;

    #[test]
    fn passes_platform_discovery_conformance() {
        let blob = virt_like_arm(0x4000_0000, 0x2000_0000, "hvc", 14);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let disco = FdtDiscovery::new(fdt);
        conformance::run(&disco);
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

    fn discover_all(blob: &[u8]) -> Vec<HwNode> {
        let fdt = Fdt::new(blob).expect("valid fdt");
        let mut sink = CollectingSink::default();
        FdtDiscovery::new(fdt)
            .discover(&mut sink)
            .expect("discovery succeeds");
        sink.nodes
    }

    /// The node carrying `compatible` among its match keys.
    fn by_key<'a>(nodes: &'a [HwNode], compatible: &[u8]) -> &'a HwNode {
        nodes
            .iter()
            .find(|n| {
                n.match_keys()
                    .iter()
                    .any(|k| k.compatible_bytes() == compatible)
            })
            .expect("node with the key is emitted")
    }

    fn mmio_windows(node: &HwNode) -> Vec<(u64, u64)> {
        node.resources()
            .iter()
            .filter(|r| r.kind() == Some(HwResourceKind::Mmio))
            .map(|r| (r.base(), r.length()))
            .collect()
    }

    #[test]
    fn emits_every_described_device_from_a_virt_tree() {
        let nodes = discover_all(&virt_like_arm(0x4000_0000, 0x2000_0000, "hvc", 30));
        // root + psci + intc + timer + memory — every `compatible`-
        // carrying node plus the memory node, nothing invented.
        assert_eq!(nodes.len(), 5);
        assert!(nodes[0].is_root());

        // `/psci` is firmware, not hardware the port special-cases: the
        // generic walk still surfaces it (unbound is fine, §18.4).
        let psci = by_key(&nodes, b"arm,psci-1.0");
        assert_eq!(psci.class(), Some(HwDeviceClass::Other));
        assert!(psci.resources().is_empty());

        // The GICv2 (`intc@…` — the `virt` board's name) carries both
        // register windows with their *tree-declared* lengths.
        let gic = by_key(&nodes, b"arm,cortex-a15-gic");
        assert_eq!(gic.class(), Some(HwDeviceClass::InterruptController));
        assert_eq!(
            mmio_windows(gic),
            [(0x0800_0000, 0x1_0000), (0x0801_0000, 0x1_0000)]
        );

        // The generic timer carries its PPI from `interrupts` and its
        // own `compatible` as the bind key.
        let timer = by_key(&nodes, b"arm,armv8-timer");
        assert_eq!(timer.class(), Some(HwDeviceClass::Timer));
        let irq = timer.resources().first().expect("timer irq resource");
        assert_eq!(irq.kind(), Some(HwResourceKind::Irq));
        assert_eq!(irq.base(), 30, "timer node carries the PPI as an IRQ");

        // `/memory` has no `compatible`; `device_type` classifies it.
        let memory = nodes
            .iter()
            .find(|n| n.class() == Some(HwDeviceClass::Memory))
            .expect("memory node emitted");
        assert!(memory.match_keys().is_empty());
        assert_eq!(mmio_windows(memory), [(0x4000_0000, 0x2000_0000)]);

        // Every device sits directly under the root.
        assert!(nodes[1..].iter().all(|n| n.parent() == 0));
    }

    #[test]
    fn emits_every_described_device_from_a_raspi_tree() {
        let nodes = discover_all(&raspi_like_arm(0x7e20_1000, 0x7e21_5040));
        // root + psci + soc + gic + mailbox + pl011 + mini-uart + memory:
        // the generic walk emits *both* UARTs — preferring one is console
        // policy (`crate::console`), not tree shape — and the `/soc`
        // simple-bus is itself a bindable bus node.
        assert_eq!(nodes.len(), 8);

        // Every `/soc` child names the emitted bus node as its parent.
        let soc = by_key(&nodes, b"simple-bus");
        assert_eq!(soc.class(), Some(HwDeviceClass::Bus));

        // The Pi 4's GIC-400 `reg` carries the real tree's four one-cell
        // bus regions (GICD/GICC/GICH/GICV), each translated through the
        // `/soc` `ranges` to its BCM2711 CPU-physical window.
        let gic = by_key(&nodes, b"arm,gic-400");
        assert_eq!(gic.class(), Some(HwDeviceClass::InterruptController));
        assert_eq!(gic.parent(), soc.id());
        assert_eq!(
            mmio_windows(gic),
            [
                (0xff84_1000, 0x1000),
                (0xff84_2000, 0x2000),
                (0xff84_4000, 0x2000),
                (0xff84_6000, 0x2000)
            ]
        );

        // The VideoCore mailbox carries the discovered (translated)
        // doorbell window plus the DMA property-buffer carve request
        // bounded by the 30-bit aperture (`plans/PI.md` P7).
        let mailbox = by_key(&nodes, b"brcm,bcm2835-mbox");
        assert_eq!(mailbox.class(), Some(HwDeviceClass::Other));
        assert_eq!(mmio_windows(mailbox), [(0xfe00_b880, 0x40)]);
        let dma = mailbox.resources().get(1).expect("dma carve request");
        assert_eq!(dma.kind(), Some(HwResourceKind::Dma));
        assert_eq!(dma.base(), 0x4000_0000, "VC aperture limit");
        assert_eq!(dma.length(), 4096, "one-page property carve");

        // Both UARTs are serial-class nodes with their bus `reg` values
        // translated to the CPU-physical bases.
        let pl011 = by_key(&nodes, b"arm,pl011");
        assert_eq!(pl011.class(), Some(HwDeviceClass::Serial));
        assert_eq!(mmio_windows(pl011), [(0xfe20_1000, 0x200)]);
        let mini = by_key(&nodes, b"brcm,bcm2835-aux-uart");
        assert_eq!(mini.class(), Some(HwDeviceClass::Serial));
        assert_eq!(mmio_windows(mini), [(0xfe21_5040, 0x40)]);

        let memory = nodes
            .iter()
            .find(|n| n.class() == Some(HwDeviceClass::Memory))
            .expect("memory node emitted");
        assert_eq!(mmio_windows(memory), [(0, 0x4000_0000)]);
    }

    /// A Pi-4-shaped nested tree: devices under a `simple-bus` `/soc`
    /// whose `ranges` maps the legacy bus addresses (`0x7e……`) to the
    /// BCM2711 ARM-physical window (`0xfe……`) — the real Pi DTB shape.
    fn nested_soc_tree(with_ranges: bool) -> Vec<u8> {
        let mut b = DtbBuilder::new();
        b.begin_node("");
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 2);
        b.begin_node("soc");
        b.prop_str("compatible", "simple-bus");
        b.prop_u32("#address-cells", 1);
        b.prop_u32("#size-cells", 1);
        if with_ranges {
            // <child=0x7e000000 parent=0x0_fe000000 size=0x01800000>
            let mut ranges = Vec::new();
            ranges.extend_from_slice(&0x7e00_0000u32.to_be_bytes());
            ranges.extend_from_slice(&0xfe00_0000u64.to_be_bytes());
            ranges.extend_from_slice(&0x0180_0000u32.to_be_bytes());
            b.prop("ranges", &ranges);
        }
        b.begin_node("emmc2@7e340000");
        b.prop_str("compatible", "brcm,bcm2711-emmc2");
        let mut reg = Vec::new();
        reg.extend_from_slice(&0x7e34_0000u32.to_be_bytes());
        reg.extend_from_slice(&0x100u32.to_be_bytes());
        b.prop("reg", &reg);
        b.end_node();
        b.end_node();
        b.end_node();
        b.build()
    }

    #[test]
    fn translates_nested_reg_through_the_bus_ranges() {
        let nodes = discover_all(&nested_soc_tree(true));
        assert_eq!(nodes.len(), 3, "root + soc + emmc2");

        let soc = by_key(&nodes, b"simple-bus");
        assert_eq!(soc.class(), Some(HwDeviceClass::Bus));
        assert_eq!(soc.parent(), 0);

        // The EMMC2 node — the P8 SD host — is emitted with no per-device
        // code, parented under the bus, its window translated into the
        // ARM-physical space.
        let emmc2 = by_key(&nodes, b"brcm,bcm2711-emmc2");
        assert_eq!(emmc2.class(), Some(HwDeviceClass::Storage));
        assert_eq!(emmc2.parent(), soc.id());
        assert_eq!(mmio_windows(emmc2), [(0xfe34_0000, 0x100)]);
    }

    #[test]
    fn drops_a_window_an_ancestor_cannot_translate() {
        // Without `ranges` the bus address cannot reach the CPU's space;
        // the node is still emitted (bindable) but carries no invented
        // window (`AGENTS.md` §2.9).
        let nodes = discover_all(&nested_soc_tree(false));
        let emmc2 = by_key(&nodes, b"brcm,bcm2711-emmc2");
        assert!(emmc2.resources().is_empty());
    }

    #[test]
    fn skips_nodes_no_driver_could_ever_bind() {
        // `/cpus` and its `cpu@*` children carry no `compatible` in this
        // fixture, and are not memory nodes: nothing can bind them, so
        // they are not emitted. Only root + memory remain.
        let blob = arm_with_cpus(0x4000_0000, 0x2000_0000, &[(0, Some(1024)), (1, None)]);
        let nodes = discover_all(&blob);
        assert_eq!(nodes.len(), 2, "root + memory");
        assert_eq!(nodes[1].class(), Some(HwDeviceClass::Memory));
    }

    #[test]
    fn skips_an_overlong_compatible_and_keeps_list_order() {
        let overlong = "x".repeat(65);
        let mut b = DtbBuilder::new();
        b.begin_node("");
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 2);
        // A node whose only compatible cannot be represented is skipped
        // outright (no driver bind key could carry it either).
        b.begin_node("bogus");
        b.prop_str("compatible", &overlong);
        b.end_node();
        // A multi-string list is emitted in devicetree order (most
        // specific first), the unrepresentable entry dropped.
        b.begin_node("sdhci@0");
        let mut list = Vec::new();
        for s in ["brcm,bcm2711-emmc2", overlong.as_str(), "arasan,sdhci-5.1"] {
            list.extend_from_slice(s.as_bytes());
            list.push(0);
        }
        b.prop("compatible", &list);
        b.end_node();
        b.end_node();
        let nodes = discover_all(&b.build());

        assert_eq!(nodes.len(), 2, "root + the sdhci node");
        let sdhci = &nodes[1];
        assert_eq!(sdhci.class(), Some(HwDeviceClass::Storage));
        let keys: Vec<&[u8]> = sdhci
            .match_keys()
            .iter()
            .map(rustos_abi::HwMatchKey::compatible_bytes)
            .collect();
        assert_eq!(
            keys,
            [b"brcm,bcm2711-emmc2".as_slice(), b"arasan,sdhci-5.1"]
        );
    }

    #[test]
    fn refuses_a_tree_nested_beyond_the_walk_bound() {
        // 16 nested nodes exceed `MAX_WALK_DEPTH`; the walk fails closed
        // rather than silently under-enumerating (`AGENTS.md` §2.9).
        let mut b = DtbBuilder::new();
        b.begin_node("");
        for _ in 0..16 {
            b.begin_node("nest");
        }
        for _ in 0..16 {
            b.end_node();
        }
        b.end_node();
        let blob = b.build();
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let mut sink = CollectingSink::default();
        assert_eq!(
            FdtDiscovery::new(fdt).discover(&mut sink),
            Err(DiscoveryError::MalformedSource)
        );
    }
}
