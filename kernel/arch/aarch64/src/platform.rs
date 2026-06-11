//! aarch64 early-boot platform discovery (`AGENTS.md` §17.2 / §18.2).
//!
//! Implements the Arch HAL
//! [`PlatformDiscovery`](rustos_arch_api::PlatformDiscovery) slice by
//! normalising the
//! flattened device tree the `virt` board hands the kernel into
//! [`rustos_abi::hwtree`] nodes, using the aarch64 [`crate::fdt`] queries.
//! The emitted tree is intentionally shallow — exactly what the reader can
//! see today: a root, the first `/memory` region, the generic-timer
//! device (carrying its per-CPU PPI as a capability-gated IRQ resource),
//! the GICv2/GIC-400 interrupt controller (its discovered GICD/GICC
//! windows, `plans/PI.md` P3), the console UART (P2), and — on a board
//! that carries one — the `VideoCore` firmware mailbox with its DMA
//! property-buffer carve request (P7). Richer enumeration (CPU nodes
//! from `/cpus`, virtio-mmio children) lands behind this same trait as
//! the reader grows.
//!
//! The PSCI conduit the tree also carries ([`crate::fdt::psci_method`]) is
//! the prerequisite the aarch64 SMP bring-up (Stage W6) consumes; it is a
//! firmware-call property rather than a device node, so it is exposed
//! through the reader, not as a tree node.

use crate::console::find_console;
use crate::fdt::{find_mailbox, timer_ppi, Fdt};
use crate::gic::find_gic;
use rustos_abi::{HwDeviceClass, HwMatchKey, HwNode, HwResource, HW_NODE_ROOT};
use rustos_arch_api::{DiscoveryError, HwNodeSink, PlatformDiscovery};

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

        if let Some((base, size)) = self.fdt.first_memory_region() {
            let mut node = HwNode::new(next_id, 0, HwDeviceClass::Memory);
            node.push_resource(HwResource::mmio(base, size))
                .map_err(|_| DiscoveryError::MalformedSource)?;
            sink.emit(node)?;
            next_id += 1;
        }

        if let Some(ppi) = timer_ppi(&self.fdt) {
            let mut node = HwNode::new(next_id, 0, HwDeviceClass::Timer);
            node.push_resource(HwResource::irq(u64::from(ppi), 1))
                .map_err(|_| DiscoveryError::MalformedSource)?;
            sink.emit(node)?;
            next_id += 1;
        }

        // The interrupt controller (`plans/PI.md` P3): GICD/GICC MMIO
        // bases are discovered, not assumed, so the same kernel drives the
        // `virt` GICv2 and the Pi 4's GIC-400. The node carries the GIC's
        // `compatible` bind key and both register windows as
        // capability-gated MMIO resources (`AGENTS.md` §4 / §18.1).
        if let Some(gic) = find_gic(&self.fdt) {
            let mut node = HwNode::new(next_id, 0, HwDeviceClass::InterruptController);
            node.push_match_key(
                HwMatchKey::compatible(gic.compatible)
                    .map_err(|_| DiscoveryError::MalformedSource)?,
            )
            .map_err(|_| DiscoveryError::MalformedSource)?;
            node.push_resource(HwResource::mmio(gic.gicd_base, 0x1000))
                .map_err(|_| DiscoveryError::MalformedSource)?;
            node.push_resource(HwResource::mmio(gic.gicc_base, 0x2000))
                .map_err(|_| DiscoveryError::MalformedSource)?;
            sink.emit(node)?;
            next_id += 1;
        }

        // The VideoCore firmware mailbox (`plans/PI.md` P7): the doorbell
        // block the Pi 4's framebuffer discovery rings. The node carries
        // the `compatible` bind key, the discovered doorbell window as a
        // capability-gated MMIO resource, and a DMA resource *requesting*
        // a property-buffer carve inside the 30-bit VideoCore aperture
        // (`AGENTS.md` §4 / §18.1). Boards without one (QEMU `virt`)
        // simply omit the node — never an error (§18.4).
        if let Some(mailbox) = find_mailbox(&self.fdt) {
            let mut node = HwNode::new(next_id, 0, HwDeviceClass::Other);
            node.push_match_key(
                HwMatchKey::compatible(mailbox.compatible())
                    .map_err(|_| DiscoveryError::MalformedSource)?,
            )
            .map_err(|_| DiscoveryError::MalformedSource)?;
            node.push_resource(HwResource::mmio(mailbox.base, mailbox.len))
                .map_err(|_| DiscoveryError::MalformedSource)?;
            node.push_resource(HwResource::dma(
                VIDEOCORE_APERTURE_LIMIT,
                MAILBOX_DMA_BUFFER_LEN,
            ))
            .map_err(|_| DiscoveryError::MalformedSource)?;
            sink.emit(node)?;
            next_id += 1;
        }

        // The console UART (`plans/PI.md` P2): its MMIO base is discovered,
        // not assumed, so the boot console can be brought up on whatever
        // board the firmware describes. The node carries the model's
        // `compatible` as its bind key and the register window as a
        // capability-gated MMIO resource (`AGENTS.md` §4 / §18.1).
        if let Some(console) = find_console(&self.fdt) {
            let mut node = HwNode::new(next_id, 0, HwDeviceClass::Serial);
            node.push_match_key(
                HwMatchKey::compatible(console.model.compatible())
                    .map_err(|_| DiscoveryError::MalformedSource)?,
            )
            .map_err(|_| DiscoveryError::MalformedSource)?;
            node.push_resource(HwResource::mmio(console.base, console.len))
                .map_err(|_| DiscoveryError::MalformedSource)?;
            sink.emit(node)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::FdtDiscovery;
    use crate::console::ConsoleModel;
    use crate::fdt::Fdt;
    use rustos_abi::{HwDeviceClass, HwNode, HwResourceKind};
    use rustos_arch_api::platform::{conformance, DiscoveryError, HwNodeSink, PlatformDiscovery};
    use rustos_fdt::fixture::{raspi_like_arm, virt_like_arm};

    #[test]
    fn passes_platform_discovery_conformance() {
        let blob = virt_like_arm(0x4000_0000, 0x2000_0000, "hvc", 14);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let disco = FdtDiscovery::new(fdt);
        conformance::run(&disco);
    }

    #[derive(Default)]
    struct CountingSink {
        memory: usize,
        timer: usize,
        timer_irq: u64,
        serial: usize,
        serial_base: u64,
        serial_compatible: std::vec::Vec<u8>,
        intc: usize,
        gicd_base: u64,
        gicc_base: u64,
        intc_compatible: std::vec::Vec<u8>,
        mailbox: usize,
        mailbox_base: u64,
        mailbox_len: u64,
        mailbox_dma_limit: u64,
        mailbox_dma_len: u64,
        mailbox_compatible: std::vec::Vec<u8>,
        total: usize,
    }

    impl HwNodeSink for CountingSink {
        fn emit(&mut self, node: HwNode) -> Result<(), DiscoveryError> {
            self.total += 1;
            match node.class() {
                Some(HwDeviceClass::Memory) => self.memory += 1,
                Some(HwDeviceClass::Timer) => {
                    self.timer += 1;
                    if let Some(res) = node.resources().first() {
                        self.timer_irq = res.base();
                    }
                }
                Some(HwDeviceClass::Serial) => {
                    self.serial += 1;
                    if let Some(res) = node.resources().first() {
                        assert_eq!(res.kind(), Some(HwResourceKind::Mmio));
                        self.serial_base = res.base();
                    }
                    if let Some(key) = node.match_keys().first() {
                        self.serial_compatible = key.compatible_bytes().to_vec();
                    }
                }
                Some(HwDeviceClass::InterruptController) => {
                    self.intc += 1;
                    let res = node.resources();
                    if let Some(gicd) = res.first() {
                        assert_eq!(gicd.kind(), Some(HwResourceKind::Mmio));
                        self.gicd_base = gicd.base();
                    }
                    if let Some(gicc) = res.get(1) {
                        assert_eq!(gicc.kind(), Some(HwResourceKind::Mmio));
                        self.gicc_base = gicc.base();
                    }
                    if let Some(key) = node.match_keys().first() {
                        self.intc_compatible = key.compatible_bytes().to_vec();
                    }
                }
                Some(HwDeviceClass::Other) => {
                    self.mailbox += 1;
                    let res = node.resources();
                    if let Some(regs) = res.first() {
                        assert_eq!(regs.kind(), Some(HwResourceKind::Mmio));
                        self.mailbox_base = regs.base();
                        self.mailbox_len = regs.length();
                    }
                    if let Some(dma) = res.get(1) {
                        assert_eq!(dma.kind(), Some(HwResourceKind::Dma));
                        self.mailbox_dma_limit = dma.base();
                        self.mailbox_dma_len = dma.length();
                    }
                    if let Some(key) = node.match_keys().first() {
                        self.mailbox_compatible = key.compatible_bytes().to_vec();
                    }
                }
                _ => {}
            }
            Ok(())
        }
    }

    #[test]
    fn emits_memory_and_timer_with_ppi_from_virt_tree() {
        let blob = virt_like_arm(0x4000_0000, 0x2000_0000, "hvc", 30);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let disco = FdtDiscovery::new(fdt);
        let mut sink = CountingSink::default();
        disco.discover(&mut sink).expect("discovery succeeds");
        assert_eq!(sink.total, 4, "root + memory + timer + gic");
        assert_eq!(sink.memory, 1);
        assert_eq!(sink.timer, 1);
        assert_eq!(sink.timer_irq, 30, "timer node carries the PPI as an IRQ");
        // The `virt` fixture carries no UART node, so no serial node is
        // emitted (the console keeps its discovered-or-default base).
        assert_eq!(sink.serial, 0);
        // The `virt` board has no VideoCore mailbox; the node is simply
        // not emitted (§18.4).
        assert_eq!(sink.mailbox, 0);
        // The GICv2 node carries both discovered register windows + bind key.
        assert_eq!(sink.intc, 1);
        assert_eq!(sink.gicd_base, 0x0800_0000, "GICD base discovered");
        assert_eq!(sink.gicc_base, 0x0801_0000, "GICC base discovered");
        assert_eq!(sink.intc_compatible, b"arm,cortex-a15-gic");
    }

    #[test]
    fn emits_serial_node_with_discovered_base_from_raspi_tree() {
        // The Pi-shaped fixture carries a GIC-400, a VideoCore mailbox, a
        // PL011 + a mini-UART and no `/timer`; discovery emits root +
        // memory + the GIC + the mailbox + the (preferred) PL011 serial
        // node, each with its MMIO base read from the tree.
        let blob = raspi_like_arm(0x3f20_1000, 0x3f21_5040);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let disco = FdtDiscovery::new(fdt);
        let mut sink = CountingSink::default();
        disco.discover(&mut sink).expect("discovery succeeds");
        assert_eq!(sink.total, 5, "root + memory + gic + mailbox + serial");
        assert_eq!(sink.memory, 1);
        assert_eq!(sink.timer, 0);
        assert_eq!(sink.serial, 1);
        assert_eq!(sink.serial_base, 0x3f20_1000, "serial base discovered");
        assert_eq!(
            sink.serial_compatible,
            ConsoleModel::Pl011.compatible(),
            "serial node carries the model's compatible bind key"
        );
        // The Pi 4's GIC-400 is discovered at the BCM2711 bases.
        assert_eq!(sink.intc, 1);
        assert_eq!(sink.gicd_base, 0xff84_1000, "GICD-400 base discovered");
        assert_eq!(sink.gicc_base, 0xff84_2000, "GICC-400 base discovered");
        assert_eq!(sink.intc_compatible, b"arm,gic-400");
        // The VideoCore mailbox node carries the discovered doorbell
        // window plus the DMA property-buffer carve request bounded by
        // the 30-bit aperture (`plans/PI.md` P7).
        assert_eq!(sink.mailbox, 1);
        assert_eq!(sink.mailbox_base, 0xfe00_b880, "doorbell base discovered");
        assert_eq!(sink.mailbox_len, 0x40, "doorbell window length");
        assert_eq!(sink.mailbox_dma_limit, 0x4000_0000, "VC aperture limit");
        assert_eq!(sink.mailbox_dma_len, 4096, "one-page property carve");
        assert_eq!(sink.mailbox_compatible, b"brcm,bcm2835-mbox");
    }
}
