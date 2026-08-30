//! aarch64 early-boot platform discovery.
//!
//! Implements the Arch HAL
//! [`PlatformDiscovery`](tairix_arch_api::PlatformDiscovery) slice over the
//! shared device-tree walk ([`tairix_arch_api::fdtwalk`]), which normalises
//! the flattened device tree the firmware hands the kernel into
//! [`tairix_abi::hwtree`] nodes generically — no per-device list to grow
//! (`PLAN.md` Stage 4.HW).
//!
//! What is genuinely this port's, and so lives here:
//!
//! * **the GIC interrupt specifier.** Both supported boards (QEMU `virt`,
//!   the Pi 4) describe interrupts with the three-cell `<type, number,
//!   flags>` form whose `number` is relative to its type, so the shared
//!   walk is handed the cell width and the type-relative INTID mapping
//!   ([`crate::fdt::gic_intid_from_cells`]);
//! * **the board augmentation** the shared walk cannot know: the
//!   `VideoCore` mailbox's DMA property-buffer carve, the GENET MAC's DMA
//!   reach read from its parent bus, and the BCM2711 PCIe host bridge's
//!   inbound aperture and outbound window;
//! * **`pcie_bringup`**, the pre-MMU read of those PCIe windows the
//!   in-kernel USB bring-up needs before any hardware tree exists.
//!
//! The PSCI conduit the tree also carries ([`crate::fdt::psci_method`]) is
//! the prerequisite the aarch64 SMP bring-up (Stage W6) consumes; it is a
//! firmware-call property rather than a device node, so it is exposed
//! through the reader, not as a tree node.

use crate::fdt::{gic_intid_from_cells, Fdt};
use tairix_abi::{HwNode, HwResource};
use tairix_arch_api::fdtwalk::FdtPlatform;
use tairix_fdt::{
    bus_level, dma_ranges_aperture, dma_ranges_aperture_of, outbound_mmio_window, read_cells,
    scan_translated, translated_reg, BusLevel, Node,
};
// The single source of the mailbox `compatible` match identity lives in the
// device's own client crate (`lib/vcmailbox`) so the discovery key here and
// the `vcmailbox` service driver's `BIND_KEYS` can never diverge.
use tairix_vcmailbox::MAILBOX_COMPATIBLE;

/// Exclusive upper bound of the 30-bit `VideoCore` SDRAM aperture: the
/// highest ARM-physical address (plus one) the BCM2711 firmware can DMA
/// the mailbox property buffer through. Declared on the mailbox node as
/// the DMA resource's address limit so the host carves the buffer below
/// it (a capability-grant request, never an ambient
/// handle).
const VIDEOCORE_APERTURE_LIMIT: u64 = 0x4000_0000;

/// Length of the DMA-visible property-buffer carve the mailbox node
/// requests: one page — the mapping granularity — which comfortably
/// holds the 128-byte, 16-byte-aligned property message the firmware
/// protocol exchanges (`drivers/display/rpi_hvs::mailbox`).
const MAILBOX_DMA_BUFFER_LEN: u64 = 4096;

/// `compatible` string of the BCM2711 `PCIe` root complex — the host
/// bridge the Pi 4's USB-A ports sit behind (the VL805 xHCI controller,
/// `plans/PI.md` P10). Its emission is augmented with the **inbound-DMA
/// aperture** the bridge grants devices behind it, so a matched bus
/// driver knows the CPU-physical window its DMA carves must lie within
/// (a capability-grant request, never an ambient
/// handle); the aperture is read from the node's `dma-ranges`, never a
/// board constant.
///
/// Public because it is the **discovery-contract identity** of the bridge
/// node: the in-kernel driver-candidate catalogue
/// (`kernel/tairix-kernel::driver_catalog`, PLAN Stage 4.HW item 5)
/// reconstructs the discovered node's `compatible` match key from this same
/// constant — the string the device tree advertised and [`pcie_bringup`]
/// verified — so the USB bring-up is gated by a match against the discovered
/// identity, never a fabricated key.
pub const PCIE_COMPATIBLE: &[u8] = b"brcm,bcm2711-pcie";

/// `compatible` string of the BCM2711's GENET v5 Ethernet MAC — the Pi 4's
/// on-board gigabit NIC.
///
/// Its emission is augmented with the **DMA aperture** its parent bus grants
/// (a capability-grant request, never an ambient handle), because only the
/// platform's tree knows that window and the MAC's own node declares none.
/// The autoloaded `drivers/network/genet` driver states the same identity in
/// its bind table; this is the discovery side of that contract.
pub const GENET_COMPATIBLE: &[u8] = b"brcm,bcm2711-genet-v5";

/// This port's half of the shared device-tree walk: the GIC interrupt
/// specifier and the BCM2711 board augmentation.
pub struct Aarch64Fdt;

impl FdtPlatform for Aarch64Fdt {
    /// The three-cell `<type, number, flags>` GIC binding both supported
    /// boards describe interrupts with.
    const INTERRUPT_CELLS: usize = 3;

    /// The GIC mapping is a pure function of the specifier's own cells, so
    /// nothing tree-wide is read.
    fn from_tree(_fdt: &Fdt<'_>) -> Self {
        Self
    }

    /// Map a GIC specifier to the global INTID `irq_bind` and the GIC
    /// routing use.
    ///
    /// The `number` cell is relative to its `type`, so emitting it raw would
    /// hand an autoloaded driver a line 32 below the one its device raises.
    /// The mapping is the one shared decoder the boot path's own
    /// `gic_device_intid` reads, and a type this GICv2 port cannot represent
    /// (a GICv3-only extended-SPI binding) maps to nothing rather than a
    /// guessed line.
    fn interrupt_line(&self, specifier: &[u8]) -> Option<u32> {
        let kind = u32::try_from(read_cells(specifier, 0, 1)?).ok()?;
        let number = u32::try_from(read_cells(specifier, 4, 1)?).ok()?;
        gic_intid_from_cells(kind, number)
    }

    fn augment(&self, node: &Node<'_>, depth: usize, levels: &[BusLevel<'_>], hw: &mut HwNode) {
        let Some(compatible) = node.property("compatible") else {
            return;
        };
        let names_compatible = |wanted: &[u8]| compatible.iter_strings().any(|s| s == wanted);

        // The VideoCore firmware mailbox additionally *requests* a DMA
        // property-buffer carve inside the 30-bit aperture (`plans/PI.md`
        // P7) — a capability-grant request the driver host satisfies, declared here because only the platform
        // knows the firmware's aperture.
        if names_compatible(MAILBOX_COMPATIBLE) {
            let dma = HwResource::dma(VIDEOCORE_APERTURE_LIMIT, MAILBOX_DMA_BUFFER_LEN);
            // A node with no room left simply carries no carve request; the
            // capacity bound is the ABI's, never a panic.
            let _ = hw.push_resource(dma);
        }

        // A BCM2711 GENET MAC masters DMA to ordinary system memory but its own
        // node declares no reach: Devicetree Spec v0.4 §2.3.9 puts `dma-ranges`
        // on the *bus*, so the constraint is read from the parent bus level (the
        // Pi 4's `/scb`) — a discovered value, never a board constant. Declared
        // here because only the platform's tree knows the aperture.
        if names_compatible(GENET_COMPATIBLE) {
            if let Some((aperture_top, aperture_len)) = parent_dma_aperture(depth, levels) {
                // No room left simply carries no carve request; the capacity
                // bound is the ABI's, never a panic.
                let _ = hw.push_resource(HwResource::dma(aperture_top, aperture_len));
            }
        }
        // The BCM2711 PCIe host bridge additionally *requests* the
        // inbound-DMA aperture it grants devices behind it (`plans/PI.md`
        // P10): the CPU-physical window read from its `dma-ranges`, with the
        // node's own `#address`/`#size`-cells decoding the child PCI triple
        // and its parent bus's `#address-cells` the CPU base. Declared here
        // because only the platform's tree knows the aperture; the VL805 wiring carves its xHCI DMA region below the top.
        if names_compatible(PCIE_COMPATIBLE) {
            let level = bus_level(node);
            let parent_addr_cells = depth
                .checked_sub(1)
                .and_then(|i| levels.get(i))
                .map_or(2, |l| l.addr_cells);
            if let Some((aperture_top, aperture_len, inbound_pcie_base)) =
                dma_ranges_aperture(node, level.addr_cells, parent_addr_cells, level.size_cells)
            {
                // No room left simply carries no aperture request; the
                // capacity bound is the ABI's, never a panic. An
                // unreadable `dma-ranges` likewise omits it rather than
                // inventing a window. The far-side PCIe base the inbound
                // viewport starts at rides the resource's translation field
                // so the VL805 wiring can program the inbound BAR from the
                // tree, never a board constant.
                let _ = hw.push_resource(HwResource::dma_translated(
                    aperture_top,
                    aperture_len,
                    inbound_pcie_base,
                ));
            }
            // …and the outbound memory window from its `ranges`: the
            // CPU-physical aperture the bridge forwards to PCIe memory space
            // and the PCIe-space base it maps to, so the VL805 wiring can
            // both program the root complex's outbound window and translate
            // the enumerated BAR back to a CPU-physical address. Carried as a
            // single `BusWindow` (CPU base, size, far-side PCIe base) rather
            // than conflated with the controller's own `reg` MMIO windows. An unreadable `ranges` omits it rather
            // than inventing a window.
            if let Some((cpu_base, pcie_base, size)) =
                outbound_mmio_window(node, level.addr_cells, parent_addr_cells, level.size_cells)
            {
                let _ = hw.push_resource(HwResource::bus_window(cpu_base, size, pcie_base));
            }
        }
    }
}

/// The [`PlatformDiscovery`](tairix_arch_api::PlatformDiscovery)
/// implementation the boot path constructs: the shared walk over this
/// port's [`Aarch64Fdt`].
pub type FdtDiscovery<'a> = tairix_arch_api::fdtwalk::FdtDiscovery<'a, Aarch64Fdt>;

/// The BCM2711 PCIe root-complex address windows the in-kernel USB
/// keyboard bring-up needs (`plans/PI.md` P10), read from the
/// `brcm,bcm2711-pcie` node — never compiled-in.
///
/// The same three windows the [`FdtDiscovery`] walk emits on the bridge's
/// hardware-tree node (the controller `reg`, the inbound `dma-ranges`
/// aperture, the outbound `ranges` window), but resolved by a single
/// early-returning [`scan_translated`] walk so the boot path can read them
/// **before the MMU is enabled** — early enough to fold the controller and
/// outbound-window gigapages into the identity map's Device mask
/// (`plans/PI.md` P4/P5 MMU-off watch-out; a whole-tree scan faults there).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct PcieDiscovery {
    /// CPU-physical base of the PCIe controller register block (the
    /// translated `reg[0]`).
    pub regs_phys: u64,
    /// Byte length of the controller register block.
    pub regs_len: u64,
    /// Exclusive upper bound of the CPU-physical window devices behind the
    /// bridge may DMA through (the inbound aperture's top): a DMA carve
    /// must lie wholly below it.
    pub dma_aperture_top: u64,
    /// Byte length of the inbound aperture.
    pub inbound_size: u64,
    /// PCIe-space base the inbound viewport is programmed at.
    pub inbound_pcie_base: u64,
    /// CPU-physical base of the outbound MMIO window (the bridge forwards
    /// it to PCIe memory space — the enumerated BARs live here).
    pub outbound_cpu_base: u64,
    /// PCIe-space base the outbound window maps to.
    pub outbound_pcie_base: u64,
    /// Byte length of the outbound MMIO window.
    pub outbound_size: u64,
}

/// Discover the BCM2711 PCIe root complex's three address windows from the
/// firmware device tree at `fdt`, for the in-kernel USB-keyboard bring-up
/// (`plans/PI.md` P10).
///
/// Uses the early-returning [`scan_translated`] walk, reading only the
/// matched `brcm,bcm2711-pcie` node's own `reg`/`ranges`/`dma-ranges`
/// against its ancestors' cell counts, so it is safe to call **with the
/// MMU still off** (`plans/PI.md` P4/P5 watch-out — a whole-tree scan
/// widens the byte loads and faults there).
///
/// Returns `None` when the tree describes no `brcm,bcm2711-pcie` node, or
/// when any of the three windows is absent or undecodable — fail closed, never a board constant. The QEMU `virt`
/// tree carries no such node, so it simply yields `None` and the keyboard
/// bring-up is skipped.
#[must_use]
pub fn pcie_bringup(fdt: &Fdt<'_>) -> Option<PcieDiscovery> {
    scan_translated(fdt, |node, levels, depth| {
        let compatible = node.property("compatible")?;
        if !compatible.iter_strings().any(|s| s == PCIE_COMPATIBLE) {
            // Not the PCIe bridge: keep walking.
            return None;
        }
        let level = bus_level(node);
        let parent_addr_cells = depth
            .checked_sub(1)
            .and_then(|i| levels.get(i))
            .map_or(2, |l| l.addr_cells);
        // The controller register block (first `reg` entry), translated
        // through the ancestor buses' `ranges` (untranslatable → `None`,
        // never read raw — the real Pi tree is bus-nested under `/scb`).
        let (regs_phys, regs_len) = translated_reg(node, depth, levels, 0)?;
        // The inbound-DMA aperture from `dma-ranges`.
        let (dma_aperture_top, inbound_size, inbound_pcie_base) =
            dma_ranges_aperture(node, level.addr_cells, parent_addr_cells, level.size_cells)?;
        // The outbound MMIO window from `ranges`.
        let (outbound_cpu_base, outbound_pcie_base, outbound_size) =
            outbound_mmio_window(node, level.addr_cells, parent_addr_cells, level.size_cells)?;
        Some(PcieDiscovery {
            regs_phys,
            regs_len,
            dma_aperture_top,
            inbound_size,
            inbound_pcie_base,
            outbound_cpu_base,
            outbound_pcie_base,
            outbound_size,
        })
    })
}

/// The DMA aperture a node's **parent bus** grants the devices on it, as
/// `(exclusive top, extent)`, or [`None`] when no ancestor level declares
/// one.
///
/// A mastering device's reach is a property of the bus it sits on
/// (Devicetree Spec v0.4 §2.3.9), so the window is decoded from the parent
/// [`BusLevel`]'s `dma-ranges` against that bus's own cell counts and its
/// parent's address width — exactly as [`dma_ranges_aperture`] decodes a
/// bridge's own property.
fn parent_dma_aperture(depth: usize, levels: &[BusLevel<'_>]) -> Option<(u64, u64)> {
    let parent = levels.get(depth.checked_sub(1)?)?;
    let grandparent_addr_cells = depth
        .checked_sub(2)
        .and_then(|i| levels.get(i))
        .map_or(2, |l| l.addr_cells);
    let (top, len, _) = dma_ranges_aperture_of(
        parent.dma_ranges?,
        parent.addr_cells,
        grandparent_addr_cells,
        parent.size_cells,
    )?;
    Some((top, len))
}

#[cfg(test)]
mod tests {
    use super::{pcie_bringup, FdtDiscovery};
    use crate::fdt::{Fdt, GIC_PPI_INTID_BASE};
    use crate::gic::MIN_SPI_INTID;
    use std::vec::Vec;
    use tairix_abi::{HwDeviceClass, HwNode, HwResource, HwResourceKind};
    use tairix_arch_api::platform::{conformance, DiscoveryError, HwNodeSink, PlatformDiscovery};
    use tairix_fdt::fixture;
    use tairix_fdt::fixture::{arm_with_cpus, raspi_like_arm, virt_like_arm, DtbBuilder};

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
        // generic walk still surfaces it (unbound is fine).
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
        assert_eq!(
            irq.base(),
            u64::from(GIC_PPI_INTID_BASE + 30),
            "the timer's PPI is emitted as its global GICv2 INTID"
        );

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
        // root + psci + soc + gic + mailbox + gpio + pl011 + mini-uart +
        // scb + ethernet + mdio + memory: the generic walk emits *both*
        // UARTs — preferring one is console policy (`crate::console`), not
        // tree shape — and each simple-bus is itself a bindable bus node.
        assert_eq!(nodes.len(), 12);

        // Every `/soc` child names the emitted bus node as its parent. Both
        // `/soc` and `/scb` are `simple-bus`; the walk emits `/soc` first.
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

        // The BCM2711 GPIO controller (the pin-mux block
        // `uart_init::find_gpio` binds) carries its translated window
        // with the tree-declared length.
        let gpio = by_key(&nodes, b"brcm,bcm2711-gpio");
        assert_eq!(gpio.parent(), soc.id());
        assert_eq!(mmio_windows(gpio), [(0xfe20_0000, 0xb4)]);

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

    #[test]
    fn the_genet_node_carries_its_window_intids_dma_reach_and_board_mac() {
        let nodes = discover_all(&raspi_like_arm(0x7e20_1000, 0x7e21_5040));
        let genet = by_key(&nodes, b"brcm,bcm2711-genet-v5");
        // A network device, at the CPU-physical base its `/scb` bus `ranges`
        // translates its `reg` to.
        assert_eq!(genet.class(), Some(HwDeviceClass::Network));
        assert_eq!(
            mmio_windows(genet),
            [(0xfd58_0000, u64::from(fixture::GENET_REGS_LEN))]
        );

        // Both `interrupts` entries are emitted as the *global* GICv2 INTIDs
        // a driver binds, not the type-relative device-tree cells.
        let irqs: Vec<u64> = genet
            .resources()
            .iter()
            .filter(|r| r.kind() == Some(HwResourceKind::Irq))
            .map(HwResource::base)
            .collect();
        assert_eq!(
            irqs,
            [
                u64::from(MIN_SPI_INTID + fixture::GENET_SPI_A),
                u64::from(MIN_SPI_INTID + fixture::GENET_SPI_B)
            ]
        );

        // The MAC masters DMA but declares no reach of its own: the window
        // comes from its `/scb` bus's `dma-ranges`, a discovered value.
        let dma = genet
            .resources()
            .iter()
            .find(|r| r.kind() == Some(HwResourceKind::Dma))
            .expect("dma constraint request");
        assert_eq!(dma.base(), u64::from(fixture::SCB_DMA_APERTURE_TOP));
        assert_eq!(dma.length(), u64::from(fixture::SCB_DMA_APERTURE_TOP));
        // Untranslated: a GENET buffer is reached at its CPU-physical base.
        assert_eq!(dma.translated_base(), 0);

        // The firmware-published board MAC rides on the node as a fact that
        // confers no authority, so the driver programs the address printed
        // on the board rather than inventing one.
        let mac = genet
            .resources()
            .iter()
            .find_map(HwResource::link_address_octets)
            .expect("link address");
        assert_eq!(mac, fixture::GENET_BOARD_MAC);

        // Its MDIO child is emitted too, and left unbound: the GENET driver
        // reaches the PHY through its own register window.
        let mdio = by_key(&nodes, b"brcm,genet-mdio-v5");
        assert_eq!(mdio.parent(), genet.id());
    }

    #[test]
    fn a_node_on_a_bus_without_dma_ranges_requests_no_dma() {
        // The QEMU `virt` tree declares no `dma-ranges` anywhere, so nothing
        // discovered from it carries a DMA constraint: a driver receives no
        // reach it was not granted.
        let nodes = discover_all(&virt_like_arm(0x4000_0000, 0x2000_0000, "hvc", 30));
        assert!(nodes
            .iter()
            .flat_map(HwNode::resources)
            .all(|r| r.kind() != Some(HwResourceKind::Dma)));
    }

    #[test]
    fn an_unrepresentable_interrupt_specifier_is_dropped_not_guessed() {
        // A GICv3-only extended-SPI binding (type 2) is not a line this
        // GICv2 port can raise: the node is still emitted, carrying no IRQ
        // resource, rather than a line 32 below something real.
        let mut b = DtbBuilder::new();
        b.begin_node("");
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 2);
        b.begin_node("widget");
        b.prop_str("compatible", "vendor,widget");
        let mut interrupts = Vec::new();
        for cell in [2u32, 7, 0x04] {
            interrupts.extend_from_slice(&cell.to_be_bytes());
        }
        b.prop("interrupts", &interrupts);
        b.end_node();
        b.end_node();
        let nodes = discover_all(&b.build());
        let widget = by_key(&nodes, b"vendor,widget");
        assert!(widget
            .resources()
            .iter()
            .all(|r| r.kind() != Some(HwResourceKind::Irq)));
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

    /// A Pi-4-shaped tree with a `PCIe` host bridge under `/scb`, carrying
    /// the real BCM2711 `reg`, `ranges`, and `dma-ranges` shapes.
    fn scb_pcie_tree() -> Vec<u8> {
        let mut b = DtbBuilder::new();
        b.begin_node("");
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 2);
        b.begin_node("scb");
        b.prop_str("compatible", "simple-bus");
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 2);
        // <child=0x7c000000 parent=0xfc000000 size=0x03800000>: so
        // 0x7d500000 translates to 0xfd500000.
        let mut ranges = Vec::new();
        ranges.extend_from_slice(&0x7c00_0000u64.to_be_bytes());
        ranges.extend_from_slice(&0xfc00_0000u64.to_be_bytes());
        ranges.extend_from_slice(&0x0380_0000u64.to_be_bytes());
        b.prop("ranges", &ranges);
        b.begin_node("pcie@7d500000");
        b.prop_str("compatible", "brcm,bcm2711-pcie");
        b.prop_str("device_type", "pci");
        b.prop_u32("#address-cells", 3);
        b.prop_u32("#size-cells", 2);
        let mut reg = Vec::new();
        reg.extend_from_slice(&0x7d50_0000u64.to_be_bytes());
        reg.extend_from_slice(&0x9310u64.to_be_bytes());
        b.prop("reg", &reg);
        // Outbound `ranges`: <pci.hi=0x02000000 pci.mid=0 pci.lo=0xc0000000
        // cpu=0x6_00000000 size=0x40000000> — the Pi 4's high MMIO
        // aperture, CPU 0x6_0000_0000 mapped to PCIe 0xc000_0000, 1 GiB.
        let mut ranges = Vec::new();
        ranges.extend_from_slice(&0x0200_0000u32.to_be_bytes());
        ranges.extend_from_slice(&0u32.to_be_bytes());
        ranges.extend_from_slice(&0xc000_0000u32.to_be_bytes());
        ranges.extend_from_slice(&0x6_0000_0000u64.to_be_bytes());
        ranges.extend_from_slice(&0x4000_0000u64.to_be_bytes());
        b.prop("ranges", &ranges);
        // <pci.hi=0x02000000 pci.mid=0 pci.lo=0  cpu=0x0  size=0xc0000000>
        let mut dma_ranges = Vec::new();
        dma_ranges.extend_from_slice(&0x0200_0000u32.to_be_bytes());
        dma_ranges.extend_from_slice(&0u32.to_be_bytes());
        dma_ranges.extend_from_slice(&0u32.to_be_bytes());
        dma_ranges.extend_from_slice(&0u64.to_be_bytes());
        dma_ranges.extend_from_slice(&0xc000_0000u64.to_be_bytes());
        b.prop("dma-ranges", &dma_ranges);
        b.end_node();
        b.end_node();
        b.end_node();
        b.build()
    }

    fn dma_resources(node: &HwNode) -> Vec<(u64, u64, u64)> {
        node.resources()
            .iter()
            .filter(|r| r.kind() == Some(HwResourceKind::Dma))
            .map(|r| (r.base(), r.length(), r.translated_base()))
            .collect()
    }

    fn bus_windows(node: &HwNode) -> Vec<(u64, u64, u64)> {
        node.resources()
            .iter()
            .filter(|r| r.kind() == Some(HwResourceKind::BusWindow))
            .map(|r| (r.base(), r.length(), r.translated_base()))
            .collect()
    }

    #[test]
    fn pcie_bringup_reads_all_three_windows_off_the_bridge() {
        // The pre-MMU bring-up discovery resolves the same three windows
        // the full `discover` walk emits on the bridge node, by a single
        // early-returning `scan_translated` pass (`plans/PI.md` P10).
        let blob = scb_pcie_tree();
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let pcie = pcie_bringup(&fdt).expect("the pcie bridge is discovered");
        assert_eq!(pcie.regs_phys, 0xfd50_0000);
        assert_eq!(pcie.regs_len, 0x9310);
        assert_eq!(pcie.dma_aperture_top, 0xc000_0000);
        assert_eq!(pcie.inbound_size, 0xc000_0000);
        assert_eq!(pcie.inbound_pcie_base, 0);
        assert_eq!(pcie.outbound_cpu_base, 0x6_0000_0000);
        assert_eq!(pcie.outbound_pcie_base, 0xc000_0000);
        assert_eq!(pcie.outbound_size, 0x4000_0000);
    }

    #[test]
    fn pcie_bringup_is_none_when_no_bridge_is_present() {
        // The QEMU `virt`-shaped tree (no `brcm,bcm2711-pcie` node) yields
        // no bring-up, so the keyboard service is skipped.
        let blob = virt_like_arm(0x4000_0000, 0x2000_0000, "hvc", 30);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        assert_eq!(pcie_bringup(&fdt), None);
    }

    #[test]
    fn emits_the_pcie_bridge_with_its_outbound_window() {
        let nodes = discover_all(&scb_pcie_tree());
        let pcie = by_key(&nodes, b"brcm,bcm2711-pcie");

        // The outbound `ranges` memory window: CPU 0x6_0000_0000 -> PCIe
        // 0xc000_0000, 1 GiB, carried as one `BusWindow` (CPU base, size,
        // far-side PCIe base) distinct from the controller `reg` MMIO.
        assert_eq!(
            bus_windows(pcie),
            [(0x6_0000_0000, 0x4000_0000, 0xc000_0000)]
        );
        let win = pcie
            .resources()
            .iter()
            .find(|r| r.kind() == Some(HwResourceKind::BusWindow))
            .expect("outbound window request");
        assert_eq!(
            win.required_capability(),
            Ok(Some(tairix_abi::CapabilityId::MMIO_MAP))
        );
    }

    #[test]
    fn pcie_bridge_without_ranges_carries_no_outbound_window() {
        // The without-`dma-ranges` fixture also carries no `ranges`: no
        // outbound window is invented.
        let mut b = DtbBuilder::new();
        b.begin_node("");
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 2);
        b.begin_node("pcie@7d500000");
        b.prop_str("compatible", "brcm,bcm2711-pcie");
        b.prop_u32("#address-cells", 3);
        b.prop_u32("#size-cells", 2);
        b.end_node();
        b.end_node();
        let nodes = discover_all(&b.build());
        let pcie = by_key(&nodes, b"brcm,bcm2711-pcie");
        assert!(bus_windows(pcie).is_empty());
    }

    #[test]
    fn emits_the_pcie_bridge_with_its_inbound_dma_aperture() {
        let nodes = discover_all(&scb_pcie_tree());
        let scb = by_key(&nodes, b"simple-bus");

        // The PCIe root complex is a bus node, parented under `/scb`, its
        // controller `reg` translated through the bus `ranges` (the
        // ECAM/config-access window the VL805 wiring maps under
        // `CAP_MMIO_MAP`) — no per-device code on this path.
        let pcie = by_key(&nodes, b"brcm,bcm2711-pcie");
        assert_eq!(pcie.class(), Some(HwDeviceClass::Bus));
        assert_eq!(pcie.parent(), scb.id());
        assert_eq!(mmio_windows(pcie), [(0xfd50_0000, 0x9310)]);

        // The augmentation: the inbound-DMA aperture from `dma-ranges` —
        // the low 3 GiB of SDRAM devices behind the bridge may reach.
        // base is the exclusive top, and the far-side PCIe base (0 on the
        // Pi: memory is viewed at PCIe address 0) rides the translation
        // field.
        assert_eq!(dma_resources(pcie), [(0xc000_0000, 0xc000_0000, 0)]);
        let dma = pcie
            .resources()
            .iter()
            .find(|r| r.kind() == Some(HwResourceKind::Dma))
            .expect("aperture request");
        assert_eq!(
            dma.required_capability(),
            Ok(Some(tairix_abi::CapabilityId::MEM_DMA))
        );
    }

    #[test]
    fn pcie_bridge_without_dma_ranges_carries_no_aperture() {
        // Strip the `dma-ranges`: the bridge is still emitted with its
        // translated window, but no aperture is invented.
        let mut b = DtbBuilder::new();
        b.begin_node("");
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 2);
        b.begin_node("pcie@7d500000");
        b.prop_str("compatible", "brcm,bcm2711-pcie");
        b.prop_u32("#address-cells", 3);
        b.prop_u32("#size-cells", 2);
        b.end_node();
        b.end_node();
        let nodes = discover_all(&b.build());
        let pcie = by_key(&nodes, b"brcm,bcm2711-pcie");
        assert!(dma_resources(pcie).is_empty());
    }

    #[test]
    fn drops_a_window_an_ancestor_cannot_translate() {
        // Without `ranges` the bus address cannot reach the CPU's space;
        // the node is still emitted (bindable) but carries no invented
        // window.
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
            .map(tairix_abi::HwMatchKey::compatible_bytes)
            .collect();
        assert_eq!(
            keys,
            [b"brcm,bcm2711-emmc2".as_slice(), b"arasan,sdhci-5.1"]
        );
    }

    #[test]
    fn refuses_a_tree_nested_beyond_the_walk_bound() {
        // 16 nested nodes exceed `MAX_WALK_DEPTH`; the walk fails closed
        // rather than silently under-enumerating.
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
