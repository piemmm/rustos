//! Shared hardware-tree fetch and render-order helpers.
//!
//! The device-inventory listing tools (`lspci`, `lsusb`) and `sysinfo`
//! all fetch the hardware tree through the paged `sysinfo-v1`
//! `HARDWARE_TREE` query and walk it the same way: page the snapshot in
//! with [`fetch_tree`], visit the nodes in stable bus order, label the
//! non-selected context nodes in a `-t` topology view, and keep a
//! selected node's ancestor chain visible. Sibling userland crates may
//! not depend on one another, so that shared walk lives here, in one
//! place, rather than being copied per tool.

use alloc::vec::Vec;

use tairix_abi::hwtree::{HwDeviceClass, HwNode, HwTreeHeader};
use tairix_abi::sysinfo::{
    HardwareTreeRequest, SysinfoQueryId, SYSINFO_MAX_REPLY, SYSINFO_REPLY_STATUS_LEN,
};
use tairix_abi::Errno;

use crate::request::{call, CallError};
use crate::transport::Transport;

/// The most whole [`HwNode`] records one framed reply can carry after
/// its status word and the repeated [`HwTreeHeader`].
const HW_TREE_PAGE_RECORDS: usize =
    (SYSINFO_MAX_REPLY - SYSINFO_REPLY_STATUS_LEN - HwTreeHeader::WIRE_LEN) / HwNode::WIRE_LEN;

// A page carries at least one record (or the walk could never progress)
// and fits the request's u16 `limit` field, so the narrowing cast below
// cannot truncate.
const _: () = assert!(HW_TREE_PAGE_RECORDS > 0 && HW_TREE_PAGE_RECORDS <= u16::MAX as usize);

/// Number of [`HwNode`] records requested per `HARDWARE_TREE` page.
/// Derived from the endpoint's contract rather than hand-picked, so a
/// wider reply window automatically widens the page.
#[allow(clippy::cast_possible_truncation)] // compile-time checked above
pub const HW_TREE_PAGE: u16 = HW_TREE_PAGE_RECORDS as u16;

/// How many snapshots [`fetch_tree`] attempts before concluding the tree
/// is changing faster than it can be walked (hotplug churn) and giving
/// up rather than looping.
const FETCH_ATTEMPTS: usize = 3;

/// Fetch the whole hardware tree, paging the `HARDWARE_TREE` query until
/// the snapshot's total node count is held.
///
/// Every page repeats the snapshot's [`HwTreeHeader`]; the walk checks
/// that the generation stayed constant across pages and restarts on a
/// fresh snapshot when the tree changed under it, at most
/// `FETCH_ATTEMPTS` times.
///
/// The reply is untrusted input to the consuming tool and the walk
/// **fails closed**: a page that is not a whole number of records, a
/// record that does not decode, or a snapshot that ends short of (or
/// serves past) its own declared total is refused rather than rendered
/// as a partial inventory.
///
/// # Errors
///
/// * [`CallError::PermissionDenied`] — the caller lacks `CAP_SYSINFO_HW`.
/// * [`CallError::Service`]`(`[`Errno::BadMagic`]`)` — a structurally
///   invalid page or an inconsistent snapshot.
/// * [`CallError::Service`]`(`[`Errno::TimedOut`]`)` — the tree kept
///   changing across `FETCH_ATTEMPTS` snapshots.
/// * Any other transport failure, propagated verbatim.
pub fn fetch_tree(transport: &dyn Transport) -> Result<Vec<HwNode>, CallError> {
    for _ in 0..FETCH_ATTEMPTS {
        if let Some(nodes) = fetch_snapshot(transport)? {
            return Ok(nodes);
        }
    }
    Err(CallError::Service(Errno::TimedOut))
}

/// Walk one snapshot to completion. Returns `None` when the snapshot's
/// generation moved between pages, so the caller takes a fresh one.
fn fetch_snapshot(transport: &dyn Transport) -> Result<Option<Vec<HwNode>>, CallError> {
    let mut nodes: Vec<HwNode> = Vec::new();
    let mut generation: Option<u64> = None;
    loop {
        let offset =
            u32::try_from(nodes.len()).map_err(|_| CallError::Service(Errno::LengthOutOfRange))?;
        let request = HardwareTreeRequest {
            offset,
            limit: HW_TREE_PAGE,
            flags: 0,
        };
        let reply = call(
            transport,
            SysinfoQueryId::HARDWARE_TREE,
            &request.to_le_bytes(),
        )?;
        let header = HwTreeHeader::from_bytes(&reply).map_err(CallError::Service)?;
        let body = &reply[HwTreeHeader::WIRE_LEN..];
        if body.len() % HwNode::WIRE_LEN != 0 {
            return Err(CallError::Service(Errno::BadMagic));
        }
        match generation {
            None => generation = Some(header.generation()),
            Some(seen) if seen != header.generation() => return Ok(None),
            Some(_) => {}
        }
        let total = usize::try_from(header.node_count())
            .map_err(|_| CallError::Service(Errno::LengthOutOfRange))?;
        for chunk in body.as_chunks::<{ HwNode::WIRE_LEN }>().0 {
            nodes.push(HwNode::from_bytes(chunk).map_err(CallError::Service)?);
        }
        if nodes.len() > total {
            // More records than the snapshot's own header promised.
            return Err(CallError::Service(Errno::BadMagic));
        }
        if nodes.len() == total {
            return Ok(Some(nodes));
        }
        if body.is_empty() {
            // Short of the promised total with no forward progress: a
            // truncated snapshot, refused whole.
            return Err(CallError::Service(Errno::BadMagic));
        }
    }
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
    use tairix_abi::hwtree::{HwMatchKey, HW_NODE_ROOT};

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

    /// Serves paged `HARDWARE_TREE` replies the way the real service does:
    /// every page repeats the snapshot header (the generation drawn from
    /// `gens`, the last entry repeating) and carries the whole records of
    /// the requested window. `total_override` lets a test claim a total the
    /// body cannot honour; `ragged` appends a stray byte to every page.
    struct PagedFixture {
        nodes: Vec<HwNode>,
        gens: core::cell::RefCell<Vec<u64>>,
        total_override: Option<u64>,
        ragged: bool,
    }

    impl PagedFixture {
        fn new(nodes: Vec<HwNode>) -> Self {
            Self {
                nodes,
                gens: core::cell::RefCell::new(alloc::vec![7]),
                total_override: None,
                ragged: false,
            }
        }

        fn generation(&self) -> u64 {
            let mut gens = self.gens.borrow_mut();
            if gens.len() > 1 {
                gens.remove(0)
            } else {
                gens[0]
            }
        }
    }

    impl Transport for PagedFixture {
        fn query(&self, request: &[u8]) -> Result<Vec<u8>, Errno> {
            let header = tairix_abi::sysinfo::SysinfoRequestHeader::from_bytes(request)?;
            assert_eq!(header.query, SysinfoQueryId::HARDWARE_TREE);
            let payload = &request[tairix_abi::sysinfo::SysinfoRequestHeader::WIRE_LEN..];
            let req = HardwareTreeRequest::from_bytes(payload)?;
            let total = self.total_override.unwrap_or(self.nodes.len() as u64);
            let mut reply = Vec::new();
            reply.extend_from_slice(&HwTreeHeader::new(self.generation(), total).to_le_bytes());
            let offset = req.offset as usize;
            if offset < self.nodes.len() {
                let take = core::cmp::min(self.nodes.len() - offset, req.limit as usize);
                for node in &self.nodes[offset..offset + take] {
                    reply.extend_from_slice(&node.to_le_bytes());
                }
            }
            if self.ragged {
                reply.push(0);
            }
            Ok(reply)
        }
    }

    /// A flat tree larger than two pages, so a fetch must page.
    fn big_tree() -> Vec<HwNode> {
        (0..2 * u32::from(HW_TREE_PAGE) + 3)
            .map(|id| {
                HwNode::new(
                    id,
                    if id == 0 { HW_NODE_ROOT } else { 0 },
                    HwDeviceClass::Other,
                )
            })
            .collect()
    }

    #[test]
    fn fetch_reassembles_a_multi_page_tree() {
        let nodes = big_tree();
        let fixture = PagedFixture::new(nodes.clone());
        assert_eq!(fetch_tree(&fixture), Ok(nodes));
    }

    #[test]
    fn fetch_restarts_when_the_tree_changes_under_the_walk() {
        // The first walk sees generation 1 then 2 (a hotplug between its
        // pages) and restarts; the second walk sees a stable snapshot.
        let nodes = big_tree();
        let fixture = PagedFixture {
            gens: core::cell::RefCell::new(alloc::vec![1, 2]),
            ..PagedFixture::new(nodes.clone())
        };
        assert_eq!(fetch_tree(&fixture), Ok(nodes));
    }

    #[test]
    fn fetch_gives_up_on_relentless_churn() {
        // Every page arrives from a different snapshot; after the bounded
        // attempts the walk reports the contention rather than looping.
        let gens: Vec<u64> = (0..64).collect();
        let fixture = PagedFixture {
            gens: core::cell::RefCell::new(gens),
            ..PagedFixture::new(big_tree())
        };
        assert_eq!(
            fetch_tree(&fixture),
            Err(CallError::Service(Errno::TimedOut))
        );
    }

    #[test]
    fn fetch_fails_closed_on_a_truncated_snapshot() {
        // The header promises one more record than the body can supply.
        let nodes = big_tree();
        let total = nodes.len() as u64 + 1;
        let fixture = PagedFixture {
            total_override: Some(total),
            ..PagedFixture::new(nodes)
        };
        assert_eq!(
            fetch_tree(&fixture),
            Err(CallError::Service(Errno::BadMagic))
        );
    }

    #[test]
    fn fetch_fails_closed_on_a_partial_record() {
        let fixture = PagedFixture {
            ragged: true,
            ..PagedFixture::new(nodes())
        };
        assert_eq!(
            fetch_tree(&fixture),
            Err(CallError::Service(Errno::BadMagic))
        );
    }

    #[test]
    fn fetch_maps_a_capability_denial() {
        struct Denied;
        impl Transport for Denied {
            fn query(&self, _request: &[u8]) -> Result<Vec<u8>, Errno> {
                Err(Errno::PermissionDenied)
            }
        }
        assert_eq!(fetch_tree(&Denied), Err(CallError::PermissionDenied));
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
