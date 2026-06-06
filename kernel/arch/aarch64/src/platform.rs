//! aarch64 early-boot platform discovery (`AGENTS.md` §17.2 / §18.2).
//!
//! Implements the Arch HAL
//! [`PlatformDiscovery`](rustos_arch_api::PlatformDiscovery) slice by
//! normalising the
//! flattened device tree the `virt` board hands the kernel into
//! [`rustos_abi::hwtree`] nodes, using the aarch64 [`crate::fdt`] queries.
//! The emitted tree is intentionally shallow — exactly what the reader can
//! see today: a root, the first `/memory` region, and the generic-timer
//! device (carrying its per-CPU PPI as a capability-gated IRQ resource).
//! Richer enumeration (CPU nodes from `/cpus`, the GIC, virtio-mmio
//! children) lands behind this same trait as the reader grows.
//!
//! The PSCI conduit the tree also carries ([`crate::fdt::psci_method`]) is
//! the prerequisite the aarch64 SMP bring-up (Stage W6) consumes; it is a
//! firmware-call property rather than a device node, so it is exposed
//! through the reader, not as a tree node.

use crate::console::find_console;
use crate::fdt::{timer_ppi, Fdt};
use rustos_abi::{HwDeviceClass, HwMatchKey, HwNode, HwResource, HW_NODE_ROOT};
use rustos_arch_api::{DiscoveryError, HwNodeSink, PlatformDiscovery};

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
        assert_eq!(sink.total, 3, "root + memory + timer");
        assert_eq!(sink.memory, 1);
        assert_eq!(sink.timer, 1);
        assert_eq!(sink.timer_irq, 30, "timer node carries the PPI as an IRQ");
        // The `virt` fixture carries no UART node, so no serial node is
        // emitted (the console keeps its discovered-or-default base).
        assert_eq!(sink.serial, 0);
    }

    #[test]
    fn emits_serial_node_with_discovered_base_from_raspi_tree() {
        // The Pi-shaped fixture carries a PL011 + a mini-UART and no
        // `/timer`; discovery emits root + memory + the (preferred) PL011
        // serial node, with its MMIO base read from the tree.
        let blob = raspi_like_arm(0x3f20_1000, 0x3f21_5040);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let disco = FdtDiscovery::new(fdt);
        let mut sink = CountingSink::default();
        disco.discover(&mut sink).expect("discovery succeeds");
        assert_eq!(sink.total, 3, "root + memory + serial");
        assert_eq!(sink.memory, 1);
        assert_eq!(sink.timer, 0);
        assert_eq!(sink.serial, 1);
        assert_eq!(sink.serial_base, 0x3f20_1000, "serial base discovered");
        assert_eq!(
            sink.serial_compatible,
            ConsoleModel::Pl011.compatible(),
            "serial node carries the model's compatible bind key"
        );
    }
}
