//! riscv64 early-boot platform discovery.
//!
//! Implements the Arch HAL
//! [`PlatformDiscovery`](rustos_arch_api::PlatformDiscovery) slice by
//! normalising the
//! flattened device tree the `virt` board hands the kernel (FDT → hardware tree) into [`rustos_abi::hwtree`] nodes. This is
//! a tracked *move* of the facts the [`crate::fdt`] reader already extracts
//! behind the common HAL trait, not a new parser: the
//! boot path used to consume the `/memory` region and the
//! `timebase-frequency` directly; it now reaches them as a
//! [`rustos_abi::HwNode`] tree
//! through the same trait every port implements.
//!
//! The emitted tree is intentionally shallow — exactly what the current
//! reader can see: a root, the first `/memory` region, and (when present) a
//! timer device. Richer enumeration (CPU nodes, the PLIC, virtio-mmio
//! children) lands as the reader and the bus drivers grow, behind this same
//! trait.

use crate::fdt::Fdt;
use rustos_abi::{HwDeviceClass, HwNode, HwResource, HW_NODE_ROOT, HW_NODE_ROOT_ID};
use rustos_arch_api::{DiscoveryError, HwNodeSink, PlatformDiscovery};

/// Builds the hardware tree from a borrowed flattened device tree.
///
/// Holds the [`Fdt`] reader so the same instance is host-testable against a
/// hand-built blob and usable at boot over the firmware-provided pointer.
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
        // Root first so every later node's parent is already emitted. Its
        // id is the shared [`HW_NODE_ROOT_ID`]; its parent is the
        // `HW_NODE_ROOT` sentinel (so it alone is `is_root`).
        sink.emit(HwNode::new(
            HW_NODE_ROOT_ID,
            HW_NODE_ROOT,
            HwDeviceClass::Root,
        ))?;
        let mut next_id: u32 = 1;

        if let Some((base, size)) = self.fdt.first_memory_region() {
            let mut node = HwNode::new(next_id, HW_NODE_ROOT_ID, HwDeviceClass::Memory);
            // A single resource can never exceed the node's bound; a
            // failure would be a logic error in the ABI, so surface it as
            // a malformed source rather than panicking.
            node.push_resource(HwResource::mmio(base, size))
                .map_err(|_| DiscoveryError::MalformedSource)?;
            sink.emit(node)?;
            next_id += 1;
        }

        if self.fdt.timebase_frequency().is_some() {
            sink.emit(HwNode::new(next_id, HW_NODE_ROOT_ID, HwDeviceClass::Timer))?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::FdtDiscovery;
    use crate::fdt::tests::virt_like;
    use crate::fdt::Fdt;
    use rustos_abi::{HwDeviceClass, HwNode};
    use rustos_arch_api::platform::{conformance, DiscoveryError, HwNodeSink, PlatformDiscovery};

    #[test]
    fn passes_platform_discovery_conformance() {
        let blob = virt_like(0x8000_0000, 0x1000_0000, 10_000_000);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let disco = FdtDiscovery::new(fdt);
        conformance::run(&disco);
    }

    /// A counting sink so the test can assert the exact tree the `virt`
    /// blob yields: root + memory + timer.
    #[derive(Default)]
    struct CountingSink {
        memory: usize,
        timer: usize,
        total: usize,
    }

    impl HwNodeSink for CountingSink {
        fn emit(&mut self, node: HwNode) -> Result<(), DiscoveryError> {
            self.total += 1;
            match node.class() {
                Some(HwDeviceClass::Memory) => self.memory += 1,
                Some(HwDeviceClass::Timer) => self.timer += 1,
                _ => {}
            }
            Ok(())
        }
    }

    #[test]
    fn emits_memory_and_timer_from_virt_tree() {
        let blob = virt_like(0x8000_0000, 0x1000_0000, 10_000_000);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let disco = FdtDiscovery::new(fdt);
        let mut sink = CountingSink::default();
        disco.discover(&mut sink).expect("discovery succeeds");
        assert_eq!(sink.total, 3, "root + memory + timer");
        assert_eq!(sink.memory, 1);
        assert_eq!(sink.timer, 1);
    }
}
