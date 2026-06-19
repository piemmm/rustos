//! Runtime hardware-inventory store (Design D, D1 — `.junie/next-pi-prompt.md`).
//!
//! The single, authoritative record of the discovered hardware tree
//! (`AGENTS.md` §18.1): one growable node list the boot path seeds, the
//! floor bus bring-up appends discovered children to, and the autoload
//! reader snapshots. It replaces the earlier "leak a fresh
//! `&'static [HwNode]` slice on every change" stash in
//! [`crate::unlock_service`], so there is exactly one inventory all three
//! share (`AGENTS.md` §2.2 — no parallel device lists).
//!
//! This D1 surface is deliberately minimal — [`HwTreeStore::seed`],
//! [`HwTreeStore::append`], and [`HwTreeStore::snapshot`], each with a
//! present-day in-kernel consumer (`AGENTS.md` §2.3 / §15.5). The reactive
//! additions the steady-state design needs — a generation counter and a
//! `hw_tree_wait` block for re-match/hotplug, and node removal for
//! hotplug-out — land in Design D D2/D4 **with** their first user-space
//! consumers, never ahead of one (`AGENTS.md` §2.3 / §2.4).
//!
//! # Concurrency / boot-ordering
//!
//! Every access happens **after** the MMU is enabled: the boot path seeds
//! the store at the post-MMU init seam, the floor bring-up appends during
//! PID 1 spawn, and the unlock kthread snapshots it once the run queue is
//! live. The [`SpinLock`]'s atomic read-modify-write is UNPREDICTABLE on the
//! MMU-off Device memory the boot CPU first runs on, so — exactly as the old
//! [`crate::unlock_service`] stash documented — callers must not touch the
//! store before the MMU is on.

use alloc::vec::Vec;

use rustos_abi::HwNode;
use rustos_sync::SpinLock;

/// The authoritative discovered-hardware inventory.
///
/// A node is **never** silently dropped on append — the backing store is a
/// growable [`Vec`] with no fixed-capacity ceiling (`AGENTS.md` §24.1).
pub struct HwTreeStore {
    /// The discovered nodes in discovery order (root first, bus-enumerated
    /// children appended as they are found).
    nodes: SpinLock<Vec<HwNode>>,
}

impl HwTreeStore {
    /// An empty store.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            nodes: SpinLock::new(Vec::new()),
        }
    }

    /// Replace the entire inventory with `tree` (the boot-discovered tree).
    ///
    /// Called once by the boot path at the post-MMU init seam; a re-seed is
    /// permitted (it simply supersedes the prior contents).
    pub fn seed(&self, tree: &[HwNode]) {
        let mut nodes = self.nodes.lock();
        nodes.clear();
        nodes.extend_from_slice(tree);
    }

    /// Append one discovered child `node` to the inventory. The node is
    /// always added, never dropped, and the store grows on demand
    /// (`AGENTS.md` §24.1).
    pub fn append(&self, node: &HwNode) {
        self.nodes.lock().push(*node);
    }

    /// An owned snapshot of the current inventory.
    ///
    /// Returns a `Vec` so the caller owns a stable view that cannot change
    /// under it while a later mutation appends to the live store.
    #[must_use]
    pub fn snapshot(&self) -> Vec<HwNode> {
        self.nodes.lock().clone()
    }
}

impl Default for HwTreeStore {
    fn default() -> Self {
        Self::new()
    }
}

/// The kernel-wide authoritative hardware inventory (`AGENTS.md` §18.1).
///
/// Seeded by the boot path, appended to by the floor bus bring-up, and
/// snapshotted by the autoload reader — the one store all three share
/// (`AGENTS.md` §2.2).
pub static HW_TREE: HwTreeStore = HwTreeStore::new();

#[cfg(test)]
mod tests {
    use super::*;

    use rustos_abi::hwtree::{HwDeviceClass, HwMatchKey, HwNode, HW_NODE_ROOT};

    /// A minimal discovered tree (a root + a discovered bus), as the floor
    /// leaves it before the USB bring-up enumerates a child.
    fn seed_tree() -> [HwNode; 2] {
        [
            HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root),
            HwNode::new(2, 1, HwDeviceClass::Bus),
        ]
    }

    /// The bus-enumerated HID child (`AGENTS.md` §18.2), keyed by the USB
    /// interface-class match key the bring-up reads (never fabricated,
    /// §18.5).
    fn hid_child() -> HwNode {
        let mut hid = HwNode::new(3, 2, HwDeviceClass::Input);
        hid.push_match_key(HwMatchKey::usb(0x1234, 0x5678, 0x03_01_01))
            .expect("match key fits");
        hid
    }

    #[test]
    fn a_fresh_store_snapshots_empty() {
        let store = HwTreeStore::new();
        assert!(store.snapshot().is_empty());
    }

    #[test]
    fn seeding_replaces_the_contents() {
        let store = HwTreeStore::new();
        store.seed(&seed_tree());
        assert_eq!(store.snapshot().len(), 2);

        // A re-seed supersedes the prior contents.
        store.seed(&[HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root)]);
        assert_eq!(store.snapshot().len(), 1);
    }

    #[test]
    fn appending_adds_the_child_last_in_order() {
        let store = HwTreeStore::new();
        let seed = seed_tree();
        store.seed(&seed);
        let hid = hid_child();
        store.append(&hid);

        let snap = store.snapshot();
        assert_eq!(snap.len(), 3, "the child is appended, nothing dropped");
        assert_eq!(snap[0], seed[0], "existing nodes keep their order");
        assert_eq!(snap[1], seed[1]);
        assert_eq!(snap[2], hid, "the enumerated child lands last");
    }

    #[test]
    fn a_snapshot_is_stable_across_a_later_mutation() {
        let store = HwTreeStore::new();
        store.seed(&seed_tree());
        let snapshot = store.snapshot();
        // A mutation after the snapshot does not change the owned view.
        store.append(&hid_child());
        assert_eq!(snapshot.len(), 2, "the earlier snapshot is unaffected");
        assert_eq!(store.snapshot().len(), 3, "the live store grew");
    }
}
