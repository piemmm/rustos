//! Runtime hardware-inventory store (Design D, D1 — `.junie/next-pi-prompt.md`).
//!
//! The single, authoritative record of the discovered hardware tree: one growable node list the boot path seeds, the
//! floor bus bring-up appends discovered children to, and the autoload
//! reader snapshots. It replaces the earlier "leak a fresh
//! `&'static [HwNode]` slice on every change" stash in
//! [`crate::unlock_service`], so there is exactly one inventory all three
//! share (no parallel device lists).
//!
//! The reactive surface — [`HwTreeStore::seed`], [`HwTreeStore::append`],
//! [`HwTreeStore::snapshot`], and the monotonic
//! [`HwTreeStore::generation`] / [`HwTreeStore::snapshot_with_generation`]
//! the `hw_tree_read` / `hw_tree_wait` syscalls serve (Design D D2b) —
//! lands with its first consumer, never ahead of one. Node removal for hotplug-out lands in Design D D4 with the
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

use tairix_abi::hwtree::{HwResource, HW_NODE_ROOT};
use tairix_abi::{Errno, HwNode, HwTreeHeader};
use tairix_kernel_core::HwTreeSource;
use tairix_sync::SpinLock;

/// The lock-guarded inventory and its change counter, mutated together so
/// a snapshot and the generation it was taken at are always consistent.
struct Inner {
    /// The discovered nodes in discovery order (root first, bus-enumerated
    /// children appended as they are found).
    nodes: Vec<HwNode>,
    /// Monotonic count of mutations (`seed` / `append`). Starts at `0` on
    /// an empty store and only ever increases, so a `hw_tree_wait` caller
    /// comparing against a previously observed value detects every change
    /// without a lost wake-up.
    generation: u64,
}

/// The authoritative discovered-hardware inventory.
///
/// A node is **never** silently dropped on append — the backing store is a
/// growable [`Vec`] with no fixed-capacity ceiling.
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
        {
            let mut inner = self.inner.lock();
            inner.nodes.clear();
            inner.nodes.extend_from_slice(tree);
            inner.generation += 1;
        }
        // The generation advanced: wake every parked `hw_tree_wait` caller
        // so it re-reads and re-matches. Done after the
        // inner lock is dropped so the scheduler's `unpark` locks are never
        // taken under ours; a fail-safe no-op before the
        // wait-queue arch hook is installed (early boot).
        tairix_kernel_core::hw_tree_wake();
    }

    /// Append one discovered child `node` to the inventory and bump the
    /// generation. The node is always added, never dropped, and the store
    /// grows on demand.
    pub fn append(&self, node: &HwNode) {
        {
            let mut inner = self.inner.lock();
            inner.nodes.push(*node);
            inner.generation += 1;
        }
        // Wake parked `hw_tree_wait` callers on the change (see [`Self::seed`]).
        tairix_kernel_core::hw_tree_wake();
    }

    /// Publish a user-space-emitted child `node` under parent `parent_id`,
    /// assigning it a fresh, collision-free [`HwNode::id`] and recording the
    /// kernel-resolved parent, then appending it and bumping the generation.
    /// Returns the assigned id.
    ///
    /// This is the store side of the `hw_emit_node` syscall (the
    /// [`HwTreeSource::publish`] implementation). The kernel **owns
    /// identity**: the emitter supplies a node's
    /// class, match keys, and resource requests, but never its id or parent.
    /// The id is `max(existing non-root id) + 1`, so it can never collide
    /// with a seeded or previously published node — load-bearing, because the
    /// driver-store load path resolves a matched node by its id (a collision
    /// would mint the wrong driver's grants). `parent_id` is the emitter's
    /// own matched node (resolved kernel-side from the caller's task id), so a
    /// driver cannot forge its position in the tree.
    ///
    /// The id scan is `O(n)` over the current node set, but a publish is a
    /// rare discovery event (a handful per boot), never a hot path; the scan and the append happen under one lock so
    /// the assigned id cannot race a concurrent publish.
    pub fn publish_child(&self, parent_id: u32, mut node: HwNode) -> u32 {
        let id = {
            let mut inner = self.inner.lock();
            // The next free id is one past the largest live non-root id; the
            // root sentinel (`HW_NODE_ROOT`, `u32::MAX`) never participates.
            // An empty tree starts emitted ids at 1.
            let max_id = inner
                .nodes
                .iter()
                .map(HwNode::id)
                .filter(|&id| id != HW_NODE_ROOT)
                .max();
            let id = max_id.map_or(1, |m| m.saturating_add(1));
            node.set_identity(id, parent_id);
            inner.nodes.push(node);
            inner.generation += 1;
            id
        };
        // Wake parked `hw_tree_wait` callers on the change (see [`Self::seed`]);
        // done after the inner lock is dropped.
        tairix_kernel_core::hw_tree_wake();
        id
    }

    /// Remove the child `node_id` — and its whole subtree — from the
    /// inventory, but **only** when its parent is exactly `parent_id`, then
    /// bump the generation. Returns the ids of every removed node (the
    /// named child plus all its transitive descendants) so the caller can
    /// retire per-node kernel state precisely, or [`Errno::NotFound`]
    /// fail-closed if no live node has that id, or if it exists but its
    /// parent is not `parent_id`.
    ///
    /// This is the store side of the `hw_remove_node` syscall and the exact
    /// counterpart of [`Self::publish_child`]. The `parent_id` check is the
    /// ownership gate: the `hw_remove_node` handler resolves `parent_id` to
    /// the caller's *own* matched node kernel-side, so requiring the removed
    /// node's parent to equal it means a driver can retire **only** a child it
    /// itself published, never an arbitrary node (no ambient
    /// authority; — the same caller-trusted identity `publish_child`
    /// uses). A node the caller does not own and an absent node are
    /// indistinguishable in the reply (both [`Errno::NotFound`]), so the
    /// failure leaks nothing about the rest of the tree.
    ///
    /// The whole subtree rooted at `node_id` is removed — every transitive
    /// descendant, found by walking the parent links — so a grandchild a
    /// bus-child driver published can never outlive the parent device that is
    /// gone. The root sentinel can never be a removal
    /// target (an emitter's `parent_id` is its own non-root node, and the
    /// root's parent is itself), so the inventory's root is structurally
    /// safe.
    ///
    /// The descendant walk is `O(n·depth)` over the current node set, but a
    /// removal is a rare discovery event (a handful per hotplug), never a hot
    /// path; the find, the subtree collection, and the
    /// retain all happen under one lock so the set cannot race a concurrent
    /// mutation.
    ///
    /// # Errors
    ///
    /// [`Errno::NotFound`] if no live node has id `node_id`, or its parent is
    /// not `parent_id` (fail closed).
    pub fn remove_child(&self, parent_id: u32, node_id: u32) -> Result<Vec<u32>, Errno> {
        let doomed = {
            let mut inner = self.inner.lock();
            // Ownership gate: the target must exist *and* be a direct child of
            // the caller's own node. The root sentinel is never a valid
            // target (a non-root `parent_id` can never equal a root id), so
            // the inventory root is structurally protected.
            let owned = inner
                .nodes
                .iter()
                .any(|node| node.id() == node_id && node.parent() == parent_id);
            if !owned {
                return Err(Errno::NotFound);
            }

            // Collect the subtree: the target plus every transitive
            // descendant. Iterate to a fixed point so any depth is covered
            // without recursion (no stack growth); the node
            // count bounds the passes, so it always terminates.
            let mut doomed: Vec<u32> = alloc::vec![node_id];
            loop {
                let before = doomed.len();
                for node in &inner.nodes {
                    let child = node.id();
                    if doomed.contains(&node.parent()) && !doomed.contains(&child) {
                        doomed.push(child);
                    }
                }
                if doomed.len() == before {
                    break;
                }
            }

            inner.nodes.retain(|node| !doomed.contains(&node.id()));
            inner.generation += 1;
            doomed
        };
        // Wake parked `hw_tree_wait` callers on the change (see [`Self::seed`]);
        // done after the inner lock is dropped.
        tairix_kernel_core::hw_tree_wake();
        Ok(doomed)
    }

    /// Bump the generation **without** changing the node set, waking every
    /// parked `hw_tree_wait` caller so it re-reads and re-evaluates.
    ///
    /// This is the "re-evaluate now" signal for a reactive observer that
    /// depends on system state the node set does not itself carry — in
    /// particular the user-space `devmgr`, which must re-attempt its
    /// driver-store catalogue fetch once the kernel driver-store service has
    /// bound its endpoint (which happens after the boot tree settles, so no
    /// `seed`/`append` would otherwise wake the parked manager). The node
    /// set is unchanged, so a re-read is idempotent (the manager's load
    /// dedup makes a re-match a no-op); only the generation advances so the
    /// wait observes a change.
    pub fn bump(&self) {
        {
            let mut inner = self.inner.lock();
            inner.generation += 1;
        }
        // Wake parked `hw_tree_wait` callers (see [`Self::seed`]); done after
        // the inner lock is dropped.
        tairix_kernel_core::hw_tree_wake();
    }

    /// An owned snapshot of the current inventory.
    ///
    /// Returns a `Vec` so the caller owns a stable view that cannot change
    /// under it while a later mutation appends to the live store.
    #[must_use]
    pub fn snapshot(&self) -> Vec<HwNode> {
        self.inner.lock().nodes.clone()
    }

    /// The current mutation generation.
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
    /// did not.
    #[must_use]
    pub fn snapshot_with_generation(&self) -> (u64, Vec<HwNode>) {
        let inner = self.inner.lock();
        (inner.generation, inner.nodes.clone())
    }

    /// The resource grants of the live non-root node `node_id`, or `None`
    /// when no live non-root node has that id (fail closed).
    ///
    /// This is what the driver-store load gate resolves a matched node's
    /// grants against, read from the **live** inventory under the same lock
    /// every other reader uses — so a node a user-space bus driver published
    /// at runtime through `hw_emit_node` ([`Self::publish_child`]) is
    /// resolvable the instant it appears, not only the boot-seeded nodes a
    /// one-shot snapshot froze. The root sentinel
    /// is never a load target (a driver is bound to a discovered device, never
    /// the tree root), so it is excluded here.
    #[must_use]
    pub fn resolve_resources(&self, node_id: u32) -> Option<Vec<HwResource>> {
        let inner = self.inner.lock();
        inner
            .nodes
            .iter()
            .find(|node| !node.is_root() && node.id() == node_id)
            .map(|node| node.resources().to_vec())
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
    /// out.
    ///
    /// The header generation and the node bytes come from one
    /// [`Self::snapshot_with_generation`] read, so a reader's header always
    /// matches the nodes it received. Defined here, beside the store it
    /// serialises, so the wire layout has exactly one encoder.
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

/// The kernel-wide authoritative hardware inventory.
///
/// Seeded by the boot path, appended to by the floor bus bring-up, and
/// snapshotted by the autoload reader — the one store all three share.
pub static HW_TREE: HwTreeStore = HwTreeStore::new();

/// The [`HwTreeSource`] the boot path installs into the syscall dispatch
/// hook (`BootInfo::with_hw_tree`), backing the `hw_tree_read` /
/// `hw_tree_wait` syscalls with the authoritative [`HW_TREE`].
///
/// A zero-sized adapter: it owns nothing and simply forwards to the one
/// global store, so the single inventory all of the kernel shares is also
/// the one user space observes.
pub struct HwTreeStoreSource;

impl HwTreeSource for HwTreeStoreSource {
    fn generation(&self) -> Result<u64, Errno> {
        Ok(HW_TREE.generation())
    }

    fn snapshot(&self) -> Result<Vec<u8>, Errno> {
        Ok(HW_TREE.encode_snapshot())
    }

    fn publish(&self, parent_id: u32, node: HwNode) -> Result<u32, Errno> {
        // Publish the user-space-emitted child under `parent_id` into the one
        // authoritative inventory; `publish_child` assigns it a fresh,
        // collision-free id, sets its parent to the emitter's own node, bumps
        // the generation, and wakes every parked `hw_tree_wait` caller, so the
        // device manager re-reads and autoloads the matching driver. The `hw_emit_node` handler has already
        // verified the caller's `CAP_HW_EMIT`, resolved `parent_id` to the
        // caller's own matched node, and checked that every requested resource
        // is covered by one of its grants, so the store only
        // assigns identity and records it. The assigned id flows back to the
        // emitter so it can later retract this child by id.
        Ok(HW_TREE.publish_child(parent_id, node))
    }

    fn remove(&self, parent_id: u32, node_id: u32) -> Result<Vec<u32>, Errno> {
        // Remove the child `node_id` (and its subtree) from the one
        // authoritative inventory, but only when its parent is `parent_id` —
        // the caller's own matched node, resolved kernel-side by the
        // `hw_remove_node` handler (no ambient authority).
        // `remove_child` enforces that ownership gate, removes the whole
        // subtree, bumps the generation, and wakes every parked
        // `hw_tree_wait` caller so the device manager re-reads and unloads the
        // driver bound to the vanished node. It fails
        // closed `NotFound` for an unknown id or a node the caller does not
        // own; the store only mutates the inventory. The removed ids flow
        // back so the handler can retire per-node kernel state (a vanished
        // display node's seat).
        HW_TREE.remove_child(parent_id, node_id)
    }
}

/// The shared [`HwTreeStoreSource`] the boot path installs through
/// `BootInfo::with_hw_tree`.
pub static HW_TREE_SOURCE: HwTreeStoreSource = HwTreeStoreSource;

#[cfg(test)]
mod tests {
    use super::*;

    use tairix_abi::hwtree::{HwDeviceClass, HwMatchKey, HwNode, HW_NODE_ROOT};

    /// A minimal discovered tree (a root + a discovered bus), as the floor
    /// leaves it before the USB bring-up enumerates a child.
    fn seed_tree() -> [HwNode; 2] {
        [
            HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root),
            HwNode::new(2, 1, HwDeviceClass::Bus),
        ]
    }

    /// The bus-enumerated HID child, keyed by the USB
    /// interface-class match key the bring-up reads (never fabricated).
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
    fn publish_child_assigns_a_fresh_id_and_the_resolved_parent() {
        let store = HwTreeStore::new();
        store.seed(&seed_tree()); // ids 1 (root) and 2 (bus)

        // The emitter supplies a node whose id/parent are placeholders; the
        // store owns identity and overwrites both.
        let mut emitted = HwNode::new(0, HW_NODE_ROOT, HwDeviceClass::Input);
        emitted
            .push_match_key(HwMatchKey::usb(0x1234, 0x5678, 0x03_01_01))
            .expect("match key fits");
        let id = store.publish_child(2, emitted);
        // `max(non-root id) + 1` = 3, parented under the resolved node 2.
        assert_eq!(
            id, 3,
            "the first emitted id is one past the largest live id"
        );
        let snap = store.snapshot();
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[2].id(), 3, "the store assigned the id");
        assert_eq!(snap[2].parent(), 2, "parented under the resolved parent");
        assert_eq!(snap[2].match_keys().len(), 1, "the emitter's data is kept");

        // A second publish never reuses an id, even under the same parent.
        let id2 = store.publish_child(2, HwNode::new(0, HW_NODE_ROOT, HwDeviceClass::Input));
        assert_eq!(id2, 4, "ids are monotonic and collision-free");
    }

    #[test]
    fn remove_child_drops_the_node_and_its_subtree() {
        let store = HwTreeStore::new();
        store.seed(&seed_tree()); // ids 1 (root) and 2 (bus)
                                  // bus 2 publishes child 3; child 3 publishes grandchild 4.
        let child = store.publish_child(2, HwNode::new(0, HW_NODE_ROOT, HwDeviceClass::Bus));
        assert_eq!(child, 3);
        let grandchild = store.publish_child(3, HwNode::new(0, HW_NODE_ROOT, HwDeviceClass::Input));
        assert_eq!(grandchild, 4);
        assert_eq!(store.snapshot().len(), 4);

        // Removing child 3 (owned by bus 2) takes grandchild 4 with it, so a
        // stale descendant never outlives its parent — and both removed ids
        // are reported so per-node kernel state can be retired.
        assert_eq!(store.remove_child(2, 3), Ok(alloc::vec![3, 4]));
        let snap = store.snapshot();
        let ids: Vec<u32> = snap.iter().map(HwNode::id).collect();
        assert_eq!(ids, alloc::vec![1, 2], "only root and bus remain");
    }

    #[test]
    fn remove_child_fails_closed_for_an_unowned_or_absent_node() {
        let store = HwTreeStore::new();
        store.seed(&seed_tree()); // ids 1 (root) and 2 (bus)
        let child = store.publish_child(2, HwNode::new(0, HW_NODE_ROOT, HwDeviceClass::Input));
        assert_eq!(child, 3);

        // A node that exists but whose parent is not the claimed one: the
        // caller does not own it, so removal fails closed.
        assert_eq!(store.remove_child(99, 3), Err(Errno::NotFound));
        // An absent id fails closed identically — the two are
        // indistinguishable to the caller.
        assert_eq!(store.remove_child(2, 4242), Err(Errno::NotFound));
        // The failed removals left the inventory untouched.
        assert_eq!(store.snapshot().len(), 3);
    }

    #[test]
    fn remove_child_bumps_the_generation_and_a_failure_does_not() {
        let store = HwTreeStore::new();
        store.seed(&seed_tree());
        let child = store.publish_child(2, HwNode::new(0, HW_NODE_ROOT, HwDeviceClass::Input));
        assert_eq!(child, 3);
        let before = store.generation();
        // A successful removal advances the generation so a parked
        // `hw_tree_wait` caller wakes.
        assert_eq!(store.remove_child(2, 3), Ok(alloc::vec![3]));
        assert_eq!(store.generation(), before + 1);
        // A fail-closed removal changes nothing, including the generation.
        assert_eq!(store.remove_child(2, 3), Err(Errno::NotFound));
        assert_eq!(store.generation(), before + 1);
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
        // whatever the global `HW_TREE` currently holds.
        assert_eq!(HW_TREE_SOURCE.generation(), Ok(HW_TREE.generation()));
        assert_eq!(HW_TREE_SOURCE.snapshot(), Ok(HW_TREE.encode_snapshot()));
    }
}
