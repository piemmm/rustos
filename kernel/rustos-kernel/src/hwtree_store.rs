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
//! The reactive surface — [`HwTreeStore::seed`], [`HwTreeStore::append`],
//! [`HwTreeStore::snapshot`], and the monotonic
//! [`HwTreeStore::generation`] / [`HwTreeStore::snapshot_with_generation`]
//! the `hw_tree_read` / `hw_tree_wait` syscalls serve (Design D D2b) —
//! lands with its first consumer, never ahead of one (`AGENTS.md` §2.3 /
//! §2.4). Node removal for hotplug-out lands in Design D D4 with the
//! user-space `bus_usb` port-watcher that drives it.
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

use rustos_abi::{Errno, HwNode, HwTreeHeader};
use rustos_kernel_core::HwTreeSource;
use rustos_sync::SpinLock;

/// The lock-guarded inventory and its change counter, mutated together so
/// a snapshot and the generation it was taken at are always consistent.
struct Inner {
    /// The discovered nodes in discovery order (root first, bus-enumerated
    /// children appended as they are found).
    nodes: Vec<HwNode>,
    /// Monotonic count of mutations (`seed` / `append`). Starts at `0` on
    /// an empty store and only ever increases, so a `hw_tree_wait` caller
    /// comparing against a previously observed value detects every change
    /// without a lost wake-up (`AGENTS.md` §18.4).
    generation: u64,
}

/// The authoritative discovered-hardware inventory.
///
/// A node is **never** silently dropped on append — the backing store is a
/// growable [`Vec`] with no fixed-capacity ceiling (`AGENTS.md` §24.1).
pub struct HwTreeStore {
    inner: SpinLock<Inner>,
}

impl HwTreeStore {
    /// An empty store.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            inner: SpinLock::new(Inner {
                nodes: Vec::new(),
                generation: 0,
            }),
        }
    }

    /// Replace the entire inventory with `tree` (the boot-discovered tree)
    /// and bump the generation.
    ///
    /// Called once by the boot path at the post-MMU init seam; a re-seed is
    /// permitted (it simply supersedes the prior contents).
    pub fn seed(&self, tree: &[HwNode]) {
        let mut inner = self.inner.lock();
        inner.nodes.clear();
        inner.nodes.extend_from_slice(tree);
        inner.generation += 1;
    }

    /// Append one discovered child `node` to the inventory and bump the
    /// generation. The node is always added, never dropped, and the store
    /// grows on demand (`AGENTS.md` §24.1).
    pub fn append(&self, node: &HwNode) {
        let mut inner = self.inner.lock();
        inner.nodes.push(*node);
        inner.generation += 1;
    }

    /// An owned snapshot of the current inventory.
    ///
    /// Returns a `Vec` so the caller owns a stable view that cannot change
    /// under it while a later mutation appends to the live store.
    #[must_use]
    pub fn snapshot(&self) -> Vec<HwNode> {
        self.inner.lock().nodes.clone()
    }

    /// The current mutation generation (`AGENTS.md` §18.4).
    ///
    /// A `hw_tree_wait` caller blocks while this equals the value it last
    /// observed and wakes when it differs; because it only ever increases,
    /// a change occurring between a caller's read and its next poll is
    /// never missed.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.inner.lock().generation
    }

    /// An owned snapshot paired with the generation it was taken at, read
    /// under one lock so the two cannot disagree.
    ///
    /// This is what `hw_tree_read` serves: the caller learns both the tree
    /// and the exact generation to pass to a subsequent `hw_tree_wait`,
    /// with no window in which the tree changed but the reported generation
    /// did not (`AGENTS.md` §18.4).
    #[must_use]
    pub fn snapshot_with_generation(&self) -> (u64, Vec<HwNode>) {
        let inner = self.inner.lock();
        (inner.generation, inner.nodes.clone())
    }
}

impl Default for HwTreeStore {
    fn default() -> Self {
        Self::new()
    }
}

impl HwTreeStore {
    /// A wire-encoded snapshot: a [`HwTreeHeader`] (the generation it was
    /// taken at and the node count) followed by that many [`HwNode`]
    /// records, all little-endian — the exact bytes `hw_tree_read` copies
    /// out (`AGENTS.md` §18.4).
    ///
    /// The header generation and the node bytes come from one
    /// [`Self::snapshot_with_generation`] read, so a reader's header always
    /// matches the nodes it received. Defined here, beside the store it
    /// serialises, so the wire layout has exactly one encoder
    /// (`AGENTS.md` §2.2).
    #[must_use]
    pub fn encode_snapshot(&self) -> Vec<u8> {
        let (generation, nodes) = self.snapshot_with_generation();
        // The node count is bounded by the discovered hardware; it always
        // fits a `u64` on every supported target.
        let header = HwTreeHeader::new(generation, nodes.len() as u64);
        let mut blob = Vec::with_capacity(HwTreeHeader::WIRE_LEN + nodes.len() * HwNode::WIRE_LEN);
        blob.extend_from_slice(&header.to_le_bytes());
        for node in &nodes {
            blob.extend_from_slice(&node.to_le_bytes());
        }
        blob
    }
}

/// The kernel-wide authoritative hardware inventory (`AGENTS.md` §18.1).
///
/// Seeded by the boot path, appended to by the floor bus bring-up, and
/// snapshotted by the autoload reader — the one store all three share
/// (`AGENTS.md` §2.2).
pub static HW_TREE: HwTreeStore = HwTreeStore::new();

/// The [`HwTreeSource`] the boot path installs into the syscall dispatch
/// hook (`BootInfo::with_hw_tree`), backing the `hw_tree_read` /
/// `hw_tree_wait` syscalls with the authoritative [`HW_TREE`]
/// (`AGENTS.md` §18.1 / §18.4).
///
/// A zero-sized adapter: it owns nothing and simply forwards to the one
/// global store, so the single inventory all of the kernel shares is also
/// the one user space observes (`AGENTS.md` §2.2).
pub struct HwTreeStoreSource;

impl HwTreeSource for HwTreeStoreSource {
    fn generation(&self) -> Result<u64, Errno> {
        Ok(HW_TREE.generation())
    }

    fn snapshot(&self) -> Result<Vec<u8>, Errno> {
        Ok(HW_TREE.encode_snapshot())
    }
}

/// The shared [`HwTreeStoreSource`] the boot path installs through
/// `BootInfo::with_hw_tree` (`AGENTS.md` §18.1).
pub static HW_TREE_SOURCE: HwTreeStoreSource = HwTreeStoreSource;

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

    #[test]
    fn generation_starts_at_zero_and_increases_monotonically_on_every_mutation() {
        let store = HwTreeStore::new();
        assert_eq!(store.generation(), 0, "a fresh store is generation 0");

        store.seed(&seed_tree());
        assert_eq!(store.generation(), 1, "seed bumps the generation");

        store.append(&hid_child());
        assert_eq!(store.generation(), 2, "append bumps the generation");

        // A re-seed is a mutation too — it bumps the generation so a waiter
        // observing the prior value wakes.
        store.seed(&seed_tree());
        assert_eq!(store.generation(), 3, "re-seed bumps the generation");
    }

    #[test]
    fn snapshot_with_generation_agrees_with_the_separate_accessors() {
        let store = HwTreeStore::new();
        store.seed(&seed_tree());
        store.append(&hid_child());

        let (gen, snap) = store.snapshot_with_generation();
        assert_eq!(gen, store.generation(), "paired generation matches");
        assert_eq!(snap, store.snapshot(), "paired snapshot matches");
        assert_eq!(gen, 2);
        assert_eq!(snap.len(), 3);
    }

    #[test]
    fn encode_snapshot_round_trips_header_and_nodes() {
        let store = HwTreeStore::new();
        store.seed(&seed_tree());
        store.append(&hid_child());

        let blob = store.encode_snapshot();
        // The header reports the current generation and node count, and
        // decodes back to exactly the stored nodes.
        let header = HwTreeHeader::from_bytes(&blob).expect("header decodes");
        assert_eq!(header.generation(), store.generation());
        assert_eq!(header.node_count(), 3);

        let mut off = HwTreeHeader::WIRE_LEN;
        let mut decoded = Vec::new();
        for _ in 0..header.node_count() {
            let node = HwNode::from_bytes(&blob[off..]).expect("node decodes");
            decoded.push(node);
            off += HwNode::WIRE_LEN;
        }
        assert_eq!(off, blob.len(), "no trailing bytes");
        assert_eq!(decoded, store.snapshot(), "nodes round-trip exactly");
    }

    #[test]
    fn the_static_source_forwards_to_the_global_store() {
        // The adapter is a pure forwarder: its generation and snapshot are
        // whatever the global `HW_TREE` currently holds (`AGENTS.md` §2.2).
        assert_eq!(HW_TREE_SOURCE.generation(), Ok(HW_TREE.generation()));
        assert_eq!(HW_TREE_SOURCE.snapshot(), Ok(HW_TREE.encode_snapshot()));
    }
}
