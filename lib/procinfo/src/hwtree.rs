//! Shared hardware-tree decode and render-order helpers.
//!
//! The device-inventory listing tools (`lspci`, `lsusb`) both fetch the
//! hardware tree through the `sysinfo-v1` `HARDWARE_TREE` query and walk
//! it the same way: decode the reply fail-closed as whole
//! [`HwNode`] records, visit the nodes in stable bus order, label the
//! non-selected context nodes in a `-t` topology view, and keep a
//! selected node's ancestor chain visible. Sibling userland crates may
//! not depend on one another, so that shared walk lives here, in one
//! place, rather than being copied per tool.

use alloc::vec::Vec;

use rustos_abi::hwtree::{HwDeviceClass, HwNode};
use rustos_abi::Errno;

/// Decode a `HARDWARE_TREE` reply as whole [`HwNode`] records.
///
/// The reply is untrusted input to the consuming tool: a length that is
/// not a whole number of records, or a record that does not decode,
/// fails the listing closed rather than rendering a partial inventory.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] for a partial trailing record, or the
/// decode error of the offending record.
pub fn decode_tree(reply: &[u8]) -> Result<Vec<HwNode>, Errno> {
    if reply.len() % HwNode::WIRE_LEN != 0 {
        return Err(Errno::BufferTooSmall);
    }
    reply
        .chunks_exact(HwNode::WIRE_LEN)
        .map(HwNode::from_bytes)
        .collect()
}

/// The indices of `nodes` in stable bus order: a depth-first walk from
/// the roots, children visited in ascending node-id order. A node whose
/// parent is absent from the reply is treated as a root rather than
/// dropped, so a truncated view still lists every device it carries.
#[must_use]
pub fn bus_order(nodes: &[HwNode]) -> Vec<usize> {
    let ids: Vec<u32> = nodes.iter().map(HwNode::id).collect();
    let mut by_id: Vec<usize> = (0..nodes.len()).collect();
    by_id.sort_by_key(|&i| ids[i]);

    let mut order = Vec::with_capacity(nodes.len());
    let mut visited = alloc::vec![false; nodes.len()];
    // An explicit stack; children are pushed in reverse id order so they
    // pop in ascending order.
    let mut stack: Vec<usize> = Vec::new();
    for &root in by_id
        .iter()
        .filter(|&&i| !ids.contains(&nodes[i].parent()) || nodes[i].parent() == nodes[i].id())
    {
        stack.push(root);
        while let Some(index) = stack.pop() {
            if visited[index] {
                continue;
            }
            visited[index] = true;
            order.push(index);
            for &child in by_id.iter().rev() {
                if child != index && nodes[child].parent() == ids[index] {
                    stack.push(child);
                }
            }
        }
    }
    order
}

/// `node`'s depth below its outermost present ancestor, bounded by the
/// node count so a cyclic parent link cannot loop.
#[must_use]
pub fn depth_of(nodes: &[HwNode], node: &HwNode) -> usize {
    let mut depth = 0;
    let mut parent = node.parent();
    while depth < nodes.len() {
        let Some(pos) = nodes.iter().position(|n| n.id() == parent) else {
            break;
        };
        if nodes[pos].id() == node.id() {
            break;
        }
        depth += 1;
        parent = nodes[pos].parent();
    }
    depth
}

/// Which nodes are one of `selected_ids` or an ancestor of one — the set
/// a `-t` topology view renders, so a selected device always appears
/// under its full parent chain. Bounded by the node count so a cyclic
/// parent link cannot loop.
#[must_use]
pub fn keep_with_ancestors(nodes: &[HwNode], selected_ids: &[u32]) -> Vec<bool> {
    let mut keep = alloc::vec![false; nodes.len()];
    for (index, node) in nodes.iter().enumerate() {
        if !selected_ids.contains(&node.id()) {
            continue;
        }
        keep[index] = true;
        // Mark the ancestor chain, stopping on a cycle or a missing parent.
        let mut parent = node.parent();
        let mut steps = 0;
        while steps <= nodes.len() {
            let Some(pos) = nodes.iter().position(|n| n.id() == parent) else {
                break;
            };
            if keep[pos] {
                break;
            }
            keep[pos] = true;
            parent = nodes[pos].parent();
            steps += 1;
        }
    }
    keep
}

/// A terse label for a non-selected context node in a `-t` topology view.
#[must_use]
pub fn class_label(class: Option<HwDeviceClass>) -> &'static str {
    match class {
        Some(HwDeviceClass::Root) => "root",
        Some(HwDeviceClass::Bus) => "bus",
        Some(HwDeviceClass::Cpu) => "cpu",
        Some(HwDeviceClass::Memory) => "memory",
        Some(HwDeviceClass::Display) => "display",
        Some(HwDeviceClass::Input) => "input",
        Some(HwDeviceClass::Network) => "network",
        Some(HwDeviceClass::Storage) => "storage",
        Some(HwDeviceClass::Timer) => "timer",
        Some(HwDeviceClass::InterruptController) => "interrupt-controller",
        Some(HwDeviceClass::Serial) => "serial",
        Some(HwDeviceClass::Other) | None => "device",
    }
}

#[cfg(test)]
mod tests {
    use rustos_abi::hwtree::{HwMatchKey, HW_NODE_ROOT};

    use super::*;

    /// A tree with two roots and out-of-order ids: root #1 carrying #2
    /// (which carries #4) and #3, plus an unrelated root #9.
    fn nodes() -> Vec<HwNode> {
        let mut out = Vec::new();
        for (id, parent, class) in [
            (3, 1, HwDeviceClass::Network),
            (1, HW_NODE_ROOT, HwDeviceClass::Bus),
            (9, HW_NODE_ROOT, HwDeviceClass::Timer),
            (4, 2, HwDeviceClass::Input),
            (2, 1, HwDeviceClass::Bus),
        ] {
            let mut node = HwNode::new(id, parent, class);
            node.push_match_key(HwMatchKey::compatible(b"fixture,node").expect("fits"))
                .expect("key fits");
            out.push(node);
        }
        out
    }

    fn wire(nodes: &[HwNode]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for node in nodes {
            bytes.extend_from_slice(&node.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn decode_round_trips_whole_records() {
        let nodes = nodes();
        let decoded = decode_tree(&wire(&nodes)).expect("decodes");
        assert_eq!(decoded.len(), nodes.len());
        assert_eq!(decoded[0].id(), 3);
    }

    #[test]
    fn decode_fails_closed_on_a_partial_record() {
        let mut bytes = wire(&nodes());
        bytes.push(0);
        assert_eq!(decode_tree(&bytes), Err(Errno::BufferTooSmall));
    }

    #[test]
    fn bus_order_is_a_stable_depth_first_walk() {
        let nodes = nodes();
        let order: Vec<u32> = bus_order(&nodes).iter().map(|&i| nodes[i].id()).collect();
        assert_eq!(order, [1, 2, 4, 3, 9]);
    }

    #[test]
    fn an_orphan_is_treated_as_a_root_not_dropped() {
        let mut nodes = nodes();
        // Point #3 at a parent absent from the reply.
        nodes[0] = HwNode::new(3, 77, HwDeviceClass::Network);
        let order: Vec<u32> = bus_order(&nodes).iter().map(|&i| nodes[i].id()).collect();
        assert_eq!(order.len(), 5, "every node is listed");
        assert!(order.contains(&3));
    }

    #[test]
    fn depth_follows_the_present_ancestor_chain() {
        let nodes = nodes();
        let by_id = |id: u32| nodes.iter().find(|n| n.id() == id).expect("present");
        assert_eq!(depth_of(&nodes, by_id(1)), 0);
        assert_eq!(depth_of(&nodes, by_id(2)), 1);
        assert_eq!(depth_of(&nodes, by_id(4)), 2);
    }

    #[test]
    fn depth_is_bounded_on_a_cyclic_parent_link() {
        let cyclic = alloc::vec![
            HwNode::new(1, 2, HwDeviceClass::Bus),
            HwNode::new(2, 1, HwDeviceClass::Bus),
        ];
        assert!(depth_of(&cyclic, &cyclic[0]) <= cyclic.len());
    }

    #[test]
    fn keep_marks_the_selection_and_its_ancestors_only() {
        let nodes = nodes();
        let keep = keep_with_ancestors(&nodes, &[4]);
        let kept: Vec<u32> = nodes
            .iter()
            .zip(&keep)
            .filter(|(_, &k)| k)
            .map(|(n, _)| n.id())
            .collect();
        assert_eq!(kept, [1, 4, 2]);
    }

    #[test]
    fn every_class_gets_a_label() {
        assert_eq!(class_label(Some(HwDeviceClass::Bus)), "bus");
        assert_eq!(class_label(None), "device");
    }
}
