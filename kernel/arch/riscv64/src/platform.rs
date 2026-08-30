//! riscv64 early-boot platform discovery.
//!
//! Implements the Arch HAL
//! [`PlatformDiscovery`](tairix_arch_api::PlatformDiscovery) slice over the
//! shared device-tree walk ([`tairix_arch_api::fdtwalk`]), which normalises
//! the flattened device tree the `virt` board hands the kernel into
//! [`tairix_abi::hwtree`] nodes generically: every node carrying a
//! `compatible` becomes a hardware-tree node whose match keys are that
//! property's strings, `reg` entries become capability-gated MMIO resources
//! translated through each ancestor bus's `ranges`, and `/memory` nodes
//! become `Memory` nodes.
//!
//! What is genuinely this port's, and so lives here, is the **PLIC
//! interrupt specifier**: the QEMU `virt` board's PLIC declares
//! `#interrupt-cells = <1>`, so the single cell *is* the source number
//! rather than a type-relative offset, and source `0` is the reserved
//! "no interrupt" sentinel. The controller's discovered `riscv,ndev` source
//! count bounds it, read once before the walk, so a device is never bound to
//! a line the controller cannot raise.

use crate::fdt::{plic_ndev, plic_source_in_range, Fdt, PLIC_SOURCE_NONE};
use tairix_arch_api::fdtwalk::FdtPlatform;
use tairix_fdt::read_cells;

/// This port's half of the shared device-tree walk: the PLIC interrupt
/// specifier, carrying the controller's discovered source count.
pub struct Riscv64Fdt {
    /// The PLIC's `riscv,ndev` source count, or `None` when the tree
    /// describes no PLIC or its node carries no readable count.
    ndev: Option<u32>,
}

impl FdtPlatform for Riscv64Fdt {
    /// The single-cell PLIC binding the `virt` board describes interrupts
    /// with.
    const INTERRUPT_CELLS: usize = 1;

    fn from_tree(fdt: &Fdt<'_>) -> Self {
        Self {
            ndev: plic_ndev(fdt),
        }
    }

    /// The cell is the PLIC source number itself. The reserved `0` sentinel
    /// routes to no line and is refused; a source above the controller's
    /// discovered count is refused too, so the tree never carries a line
    /// the PLIC cannot raise. With no discovered count the upper bound
    /// falls to the controller's own arm-time range guard, which is where
    /// the boot path's `plic_device_source` leaves it as well.
    fn interrupt_line(&self, specifier: &[u8]) -> Option<u32> {
        let source = u32::try_from(read_cells(specifier, 0, 1)?).ok()?;
        match self.ndev {
            Some(ndev) => plic_source_in_range(source, ndev).then_some(source),
            None => (source != PLIC_SOURCE_NONE).then_some(source),
        }
    }
}

/// The [`PlatformDiscovery`](tairix_arch_api::PlatformDiscovery)
/// implementation the boot path constructs: the shared walk over this
/// port's [`Riscv64Fdt`].
pub type FdtDiscovery<'a> = tairix_arch_api::fdtwalk::FdtDiscovery<'a, Riscv64Fdt>;

#[cfg(test)]
mod tests {
    use super::FdtDiscovery;
    use crate::fdt::tests::virt_like;
    use crate::fdt::Fdt;
    use tairix_abi::{HwDeviceClass, HwNode, HwResourceKind};
    use tairix_arch_api::platform::{conformance, DiscoveryError, HwNodeSink, PlatformDiscovery};

    /// The `virt_like` fixture's PLIC source for its one virtio-mmio slot.
    const SLOT_PLIC_IRQ: u32 = 1;

    #[test]
    fn passes_platform_discovery_conformance() {
        let blob = virt_like(0x8000_0000, 0x1000_0000, 10_000_000);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let disco = FdtDiscovery::new(fdt);
        conformance::run(&disco);
    }

    /// Collects the emitted tree so a test can assert the exact nodes the
    /// blob yields.
    #[derive(Default)]
    struct CollectingSink {
        nodes: std::vec::Vec<HwNode>,
    }

    impl HwNodeSink for CollectingSink {
        fn emit(&mut self, node: HwNode) -> Result<(), DiscoveryError> {
            self.nodes.push(node);
            Ok(())
        }
    }

    fn discover_all(blob: &[u8]) -> std::vec::Vec<HwNode> {
        let fdt = Fdt::new(blob).expect("valid fdt");
        let mut sink = CollectingSink::default();
        FdtDiscovery::new(fdt)
            .discover(&mut sink)
            .expect("discovery succeeds");
        sink.nodes
    }

    fn by_key(nodes: &[HwNode], compatible: &str) -> HwNode {
        let wanted = tairix_abi::HwMatchKey::compatible(compatible.as_bytes()).expect("fits");
        *nodes
            .iter()
            .find(|n| n.match_keys().contains(&wanted))
            .unwrap_or_else(|| panic!("a node carries {compatible}"))
    }

    /// The `virt`-shaped tree every emission test reads: `/memory`, the
    /// PLIC declaring one source, and one `virtio,mmio` slot on the source
    /// named by `slot_irq`.
    fn virt_tree(slot_irq: u32) -> std::vec::Vec<u8> {
        crate::fdt::tests::virt_like_with_virtio(
            0x8000_0000,
            0x1000_0000,
            10_000_000,
            1,
            &[(0x1000_1000, slot_irq)],
        )
    }

    #[test]
    fn emits_the_memory_window_and_every_compatible_node() {
        let nodes = discover_all(&virt_tree(SLOT_PLIC_IRQ));

        let memory: std::vec::Vec<&HwNode> = nodes
            .iter()
            .filter(|n| n.class() == Some(HwDeviceClass::Memory))
            .collect();
        assert_eq!(memory.len(), 1, "one described memory window");
        let window = memory[0].resources()[0];
        assert_eq!((window.base(), window.length()), (0x8000_0000, 0x1000_0000));

        // The PLIC is discovered as an interrupt controller with its own
        // register window — the node the boot path reads `riscv,ndev` and
        // `reg` off, now visible in the tree a tool can list.
        let plic = by_key(&nodes, "riscv,plic0");
        assert_eq!(plic.class(), Some(HwDeviceClass::InterruptController));
        assert_eq!(plic.resources()[0].base(), 0x0c00_0000);

        // Every non-root node hangs off the root: the fixture nests no bus.
        assert!(nodes.iter().skip(1).all(|n| n.parent() == 0));
    }

    #[test]
    fn a_virtio_slot_carries_its_plic_source_as_an_irq_resource() {
        // The whole point of moving this port onto the shared walk: a
        // discovered device now carries the line its driver parks on, which
        // the previous shallow emission could not describe at all.
        let nodes = discover_all(&virt_tree(SLOT_PLIC_IRQ));
        let slot = by_key(&nodes, "virtio,mmio");
        let irqs: std::vec::Vec<u64> = slot
            .resources()
            .iter()
            .filter(|r| r.kind() == Some(HwResourceKind::Irq))
            .map(tairix_abi::HwResource::base)
            .collect();
        assert_eq!(irqs, std::vec![u64::from(SLOT_PLIC_IRQ)]);
    }

    #[test]
    fn the_reserved_plic_sentinel_is_dropped_not_guessed() {
        // Source 0 routes to no line, so the node is still emitted (its
        // `compatible` binds a driver) but carries no interrupt.
        let nodes = discover_all(&virt_tree(0));
        let slot = by_key(&nodes, "virtio,mmio");
        assert!(
            slot.resources()
                .iter()
                .all(|r| r.kind() != Some(HwResourceKind::Irq)),
            "the reserved sentinel carries no line"
        );
    }

    #[test]
    fn a_source_above_the_controller_count_is_refused() {
        // The tree declares `riscv,ndev = 1`, so source 2 is a line this
        // PLIC cannot raise.
        let nodes = discover_all(&virt_tree(2));
        let slot = by_key(&nodes, "virtio,mmio");
        assert!(
            slot.resources()
                .iter()
                .all(|r| r.kind() != Some(HwResourceKind::Irq)),
            "an out-of-range source carries no line"
        );
    }
}
