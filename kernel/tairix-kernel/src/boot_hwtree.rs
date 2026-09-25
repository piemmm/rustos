//! Arch-neutral boot-time hardware-tree collection.
//!
//! The growable [`HwNodeSink`] the boot pipelines collect a discovered
//! [`HwNode`] tree into before the boot record moves it into
//! [`crate::hwtree_store::HW_TREE`]. Every architecture whose boot path
//! builds a hardware tree collects into the *same* sink rather than each
//! carrying its own copy of the trivial collect-into-`Vec` logic, so a change
//! to how the boot tree is buffered cannot silently diverge between siblings.
//!
//! It is pure `alloc`/`lib/abi` glue over the frozen
//! [`PlatformDiscovery`](tairix_arch_api::PlatformDiscovery) seam, so it is
//! host-tested on the CI host and names no architecture.

use alloc::vec::Vec;
use tairix_abi::HwNode;
use tairix_arch_api::{DiscoveryError, HwNodeSink};

/// A growable [`HwNodeSink`] that collects emitted nodes into a `Vec`.
///
/// The buffer grows with the discovered tree, so a larger machine's richer
/// tree is never truncated; [`HwNodeSink::emit`] fails only when the heap
/// cannot grow it.
pub struct CollectingHwNodeSink {
    nodes: Vec<HwNode>,
}

impl CollectingHwNodeSink {
    /// A fresh, empty sink.
    #[must_use]
    pub const fn new() -> Self {
        Self { nodes: Vec::new() }
    }

    /// The collected nodes, in emit order, handed over without a copy.
    #[must_use]
    pub fn into_vec(self) -> Vec<HwNode> {
        self.nodes
    }
}

impl Default for CollectingHwNodeSink {
    fn default() -> Self {
        Self::new()
    }
}

impl HwNodeSink for CollectingHwNodeSink {
    fn emit(&mut self, node: HwNode) -> Result<(), DiscoveryError> {
        self.nodes
            .try_reserve(1)
            .map_err(|_| DiscoveryError::SinkFull)?;
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
    fn collects_in_emit_order_and_hands_the_tree_over() {
        let mut sink = CollectingHwNodeSink::new();
        sink.emit(HwNode::new(
            HW_NODE_ROOT_ID,
            HW_NODE_ROOT,
            HwDeviceClass::Root,
        ))
        .expect("the heap can grow the sink");
        sink.emit(HwNode::new(1, HW_NODE_ROOT_ID, HwDeviceClass::Memory))
            .expect("the heap can grow the sink");
        sink.emit(HwNode::new(2, HW_NODE_ROOT_ID, HwDeviceClass::Timer))
            .expect("the heap can grow the sink");

        let tree = sink.into_vec();
        assert_eq!(tree.len(), 3);
        assert_eq!(tree[0].class(), Some(HwDeviceClass::Root));
        assert_eq!(tree[1].class(), Some(HwDeviceClass::Memory));
        assert_eq!(tree[2].class(), Some(HwDeviceClass::Timer));
    }

    #[test]
    fn a_fresh_sink_hands_over_an_empty_tree() {
        assert!(CollectingHwNodeSink::new().into_vec().is_empty());
    }
}
