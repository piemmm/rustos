//! Arch-neutral boot-time hardware-tree collection.
//!
//! The growable [`HwNodeSink`] the boot pipelines collect a discovered
//! [`HwNode`] tree into before publishing it to
//! [`crate::hwtree_store::HW_TREE`]. Every architecture whose boot path
//! builds a hardware tree (aarch64, riscv64) collects into the *same* sink
//! rather than each carrying its own copy of the trivial
//! collect-into-`Vec` logic, so a change to how the boot tree is buffered
//! cannot silently diverge between siblings.
//!
//! It is pure `alloc`/`lib/abi` glue over the frozen
//! [`PlatformDiscovery`](tairix_arch_api::PlatformDiscovery) seam, so it is
//! host-tested on the CI host and names no architecture.

use alloc::boxed::Box;
use alloc::vec::Vec;
use tairix_abi::HwNode;
use tairix_arch_api::{DiscoveryError, HwNodeSink};

/// A growable [`HwNodeSink`] that collects emitted nodes into a `Vec`.
///
/// The buffer is a growable `Vec`, never a fixed-capacity array, so a
/// larger machine's richer tree is never silently truncated (there is no
/// fixed-size tree buffer to outgrow); a node always fits, so
/// [`HwNodeSink::emit`] never fails.
pub struct CollectingHwNodeSink {
    nodes: Vec<HwNode>,
}

impl CollectingHwNodeSink {
    /// A fresh, empty sink.
    #[must_use]
    pub const fn new() -> Self {
        Self { nodes: Vec::new() }
    }

    /// Consume the collected nodes and leak them to `'static` for the life
    /// of the running kernel.
    ///
    /// This is a one-shot boot publish (like the leaked `KernelState`),
    /// never a mutable global: the buffered tree outlives the boot frame
    /// and lives for the kernel's lifetime so the hardware-inventory
    /// readers can borrow it.
    #[must_use]
    pub fn leak(self) -> &'static [HwNode] {
        Box::leak(self.nodes.into_boxed_slice())
    }
}

impl Default for CollectingHwNodeSink {
    fn default() -> Self {
        Self::new()
    }
}

impl HwNodeSink for CollectingHwNodeSink {
    fn emit(&mut self, node: HwNode) -> Result<(), DiscoveryError> {
        self.nodes.push(node);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::CollectingHwNodeSink;
    use tairix_abi::{HwDeviceClass, HwNode, HW_NODE_ROOT, HW_NODE_ROOT_ID};
    use tairix_arch_api::HwNodeSink;

    #[test]
    fn collects_in_emit_order_and_leaks_the_tree() {
        let mut sink = CollectingHwNodeSink::new();
        sink.emit(HwNode::new(
            HW_NODE_ROOT_ID,
            HW_NODE_ROOT,
            HwDeviceClass::Root,
        ))
        .expect("emit never fails for a growable sink");
        sink.emit(HwNode::new(1, HW_NODE_ROOT_ID, HwDeviceClass::Memory))
            .expect("emit never fails for a growable sink");
        sink.emit(HwNode::new(2, HW_NODE_ROOT_ID, HwDeviceClass::Timer))
            .expect("emit never fails for a growable sink");

        let tree: &[HwNode] = sink.leak();
        assert_eq!(tree.len(), 3);
        assert_eq!(tree[0].class(), Some(HwDeviceClass::Root));
        assert_eq!(tree[1].class(), Some(HwDeviceClass::Memory));
        assert_eq!(tree[2].class(), Some(HwDeviceClass::Timer));
    }

    #[test]
    fn a_fresh_sink_leaks_an_empty_tree() {
        let sink = CollectingHwNodeSink::new();
        assert!(sink.leak().is_empty());
    }
}
