//! Runtime hardware-inventory store (Design D, D1 — `plans/PI.md`).
//!
//! The single, authoritative record of the discovered hardware tree: one
//! growable node list the boot path seeds once, user-space bus drivers
//! publish discovered children into (`hw_emit_node`), and the autoload reader
//! snapshots — the one inventory every reader shares (no parallel device
//! lists).
//!
//! A node id names one device for the whole boot. The boot seed carries the
//! discovery ids; every node published later is issued an id above every id
//! the store has held, so no id is reissued and every node's id is above its
//! parent's. The list stays in ascending id order, which is what lets every
//! lookup by id binary-search it and a removal find a subtree in one pass.
//!
//! # Concurrency / boot-ordering
//!
//! Every access happens **after** the MMU is enabled: the boot path seeds
//! the store at the post-MMU init seam and the unlock kthread snapshots it
//! once the run queue is live. The [`SpinLock`]'s atomic read-modify-write is
//! UNPREDICTABLE on the MMU-off Device memory the boot CPU first runs on, so
//! callers must not touch the store before the MMU is on.

use alloc::vec::Vec;

use tairix_abi::blkio::FaultDomainState;
use tairix_abi::hwtree::{HwResource, HwResourceKind, HW_NODE_ROOT, HW_NODE_ROOT_ID};
use tairix_abi::{Errno, HwNode, HwTreeHeader};
use tairix_kernel_core::{HwNodeLiveness, HwTreeSource};
use tairix_sync::SpinLock;

/// The first id an unseeded store issues: the root's is never handed out.
const FIRST_ISSUED_ID: u32 = HW_NODE_ROOT_ID + 1;

/// The lock-guarded inventory and its change counter, mutated together so
/// a snapshot and the generation it was taken at are always consistent.
struct Inner {
    /// The live nodes in ascending id order: the seed is sorted, a published
    /// node carries the highest id yet, and a removal keeps the order.
    nodes: Vec<HwNode>,
    /// Monotonic count of mutations. Starts at `0` on an empty store and only
    /// ever increases, so a `hw_tree_wait` caller comparing against a
    /// previously observed value detects every change without a lost wake-up.
    generation: u64,
    /// The id the next published node is given: above every id the store has
    /// ever held, so no id is reissued within a boot.
    next_id: u32,
}

impl Inner {
    /// Where the live node `id` sits in [`Self::nodes`].
    fn position(&self, id: u32) -> Option<usize> {
        self.nodes.binary_search_by_key(&id, HwNode::id).ok()
    }

    /// The live node `id`.
    fn node(&self, id: u32) -> Option<&HwNode> {
        self.nodes.get(self.position(id)?)
    }

    /// The live node `id`, mutably.
    fn node_mut(&mut self, id: u32) -> Option<&mut HwNode> {
        let at = self.position(id)?;
        self.nodes.get_mut(at)
    }

    /// The store has never held a node, so seeding it can neither drop one
    /// nor reissue an id.
    fn is_fresh(&self) -> bool {
        self.nodes.is_empty() && self.next_id == FIRST_ISSUED_ID
    }
}

/// The authoritative discovered-hardware inventory.
///
/// The backing store is a growable [`Vec`] with no fixed-capacity ceiling.
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
                next_id: FIRST_ISSUED_ID,
            }),
        }
    }

    /// Seed the empty store with the boot-discovered `tree` and bump the
    /// generation.
    ///
    /// The seeded ids are the boot-time discovery ids: firmware discovery
    /// numbers its nodes from one, parents before children, and the synthetic
    /// nodes the boot probes emit hang from the root in the disjoint regions
    /// of [`crate::hwtree_node_ids`]. Every node published later is issued an
    /// id above all of them. A store is seeded once, before anything else
    /// populates it, so seeding can neither drop a node a driver was loaded
    /// for nor hand its id to a second device.
    ///
    /// # Errors
    ///
    /// [`Errno::AlreadyExists`] once the store has held a node, or when
    /// `tree` names one id twice; [`Errno::OutOfRange`] for a node whose id
    /// is the root sentinel ([`HW_NODE_ROOT`]) or not above its parent's. A
    /// refusal changes nothing.
    pub fn seed(&self, mut tree: Vec<HwNode>) -> Result<(), Errno> {
        tree.sort_unstable_by_key(HwNode::id);
        if tree.windows(2).any(|pair| pair[0].id() == pair[1].id()) {
            return Err(Errno::AlreadyExists);
        }
        if tree
            .iter()
            .any(|node| !node.is_root() && node.parent() >= node.id())
        {
            return Err(Errno::OutOfRange);
        }
        let next_id = match tree.last().map(HwNode::id) {
            Some(HW_NODE_ROOT) => return Err(Errno::OutOfRange),
            Some(highest) => highest + 1,
            None => FIRST_ISSUED_ID,
        };
        {
            let mut inner = self.inner.lock();
            if !inner.is_fresh() {
                return Err(Errno::AlreadyExists);
            }
            inner.nodes = tree;
            inner.next_id = next_id;
            inner.generation += 1;
        }
        // Woken after the inner lock is dropped so the scheduler's `unpark`
        // locks are never taken under ours; a no-op before the wait-queue
        // arch hook is installed (early boot).
        tairix_kernel_core::hw_tree_wake();
        Ok(())
    }

    /// Publish a user-space-emitted child `node` under parent `parent_id`,
    /// assigning it an [`HwNode::id`] no node has held before in this boot and
    /// recording the kernel-resolved parent, then appending it and bumping the
    /// generation. Returns the assigned id.
    ///
    /// This is the store side of the `hw_emit_node` syscall (the
    /// [`HwTreeSource::publish`] implementation). The kernel **owns
    /// identity**: the emitter supplies a node's class, match keys, and
    /// resource requests, but never its id or parent. An id is never
    /// reissued, so it names one device for the whole boot: the driver-store
    /// load path resolves a matched node by its id, and the DMA quarantine's
    /// reset and removal proofs speak for the device an id named, so an id
    /// reused after its node was removed would let one device's proof free
    /// another's memory. `parent_id` is the emitter's own matched node
    /// (resolved kernel-side from the caller's task id), so a driver cannot
    /// forge its position in the tree.
    ///
    /// # Errors
    ///
    /// [`Errno::NotFound`] when `parent_id` is not in the inventory, decided
    /// under the lock the append takes, so no child ever lands under a node a
    /// removal has already taken out — it could then never be removed, and a
    /// driver loaded for it would outlive its device's quarantine.
    /// [`Errno::NoSpace`] once the id space is spent; the root sentinel
    /// ([`HW_NODE_ROOT`]) is never issued. [`Errno::OutOfMemory`] when the
    /// inventory cannot grow.
    pub fn publish_child(&self, parent_id: u32, mut node: HwNode) -> Result<u32, Errno> {
        let id = {
            let mut inner = self.inner.lock();
            if inner.node(parent_id).is_none() {
                return Err(Errno::NotFound);
            }
            let id = inner.next_id;
            if id == HW_NODE_ROOT {
                return Err(Errno::NoSpace);
            }
            inner.nodes.try_reserve(1).map_err(|_| Errno::OutOfMemory)?;
            inner.next_id = id + 1;
            node.set_identity(id, parent_id);
            inner.nodes.push(node);
            inner.generation += 1;
            id
        };
        // Wake parked `hw_tree_wait` callers on the change (see [`Self::seed`]);
        // done after the inner lock is dropped.
        tairix_kernel_core::hw_tree_wake();
        Ok(id)
    }

    /// Whether the live inventory holds a node with id `node_id`.
    #[must_use]
    pub fn is_live(&self, node_id: u32) -> bool {
        self.inner.lock().node(node_id).is_some()
    }

    /// The live node `node_id`.
    #[must_use]
    pub fn node(&self, node_id: u32) -> Option<HwNode> {
        self.inner.lock().node(node_id).copied()
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
    /// the caller's *own* matched node kernel-side, so a driver can retire
    /// only a direct child of its own node — one it published or one the boot
    /// seed placed there — never an arbitrary node (no ambient authority). A
    /// node the caller does not own and an absent node are indistinguishable
    /// in the reply (both [`Errno::NotFound`]), so the failure leaks nothing
    /// about the rest of the tree.
    ///
    /// The whole subtree rooted at `node_id` is removed, so a grandchild a
    /// bus-child driver published can never outlive the parent device that is
    /// gone. The root sentinel can never be a removal target (an emitter's
    /// `parent_id` is its own non-root node, and the root's parent is the
    /// sentinel), so the inventory's root is structurally safe.
    ///
    /// Every node's id is above its parent's, so one ascending pass from the
    /// target meets each descendant after the node it hangs from: finding a
    /// `d`-node subtree among `n` live nodes costs `O(n log d)`, and so does
    /// the retain that keeps the order. Both run under the one lock, so the
    /// set cannot race a concurrent mutation.
    ///
    /// # Errors
    ///
    /// [`Errno::NotFound`] if no live node has id `node_id`, or its parent is
    /// not `parent_id` (fail closed); [`Errno::OutOfMemory`] when the removed
    /// ids cannot be listed. A refusal changes nothing.
    pub fn remove_child(&self, parent_id: u32, node_id: u32) -> Result<Vec<u32>, Errno> {
        let doomed = {
            let mut inner = self.inner.lock();
            let at = inner
                .position(node_id)
                .filter(|&at| inner.nodes[at].parent() == parent_id)
                .ok_or(Errno::NotFound)?;
            let mut doomed: Vec<u32> = Vec::new();
            for node in &inner.nodes[at..] {
                if node.id() == node_id || doomed.binary_search(&node.parent()).is_ok() {
                    doomed.try_reserve(1).map_err(|_| Errno::OutOfMemory)?;
                    doomed.push(node.id());
                }
            }
            inner
                .nodes
                .retain(|node| doomed.binary_search(&node.id()).is_err());
            inner.generation += 1;
            doomed
        };
        // Wake parked `hw_tree_wait` callers on the change (see [`Self::seed`]);
        // done after the inner lock is dropped.
        tairix_kernel_core::hw_tree_wake();
        Ok(doomed)
    }

    /// Record the fault-domain `health` of the live non-root node `node_id`
    /// and bump the generation, waking every parked `hw_tree_wait` caller so
    /// no reader holds a snapshot the change made stale. Returns
    /// [`Errno::NotFound`] fail-closed if no live non-root node has that id.
    ///
    /// This is the store side of the `hw_node_health` syscall. The handler
    /// has already resolved `node_id` to the caller's *own* matched node, so
    /// a driver only ever sets the health of the interior node it was loaded
    /// for. Only the health byte and the generation change — the node set is
    /// untouched, so this is a *distinct* signal from [`Self::remove_child`]
    /// (a merely-recovering subtree is never torn down). The root sentinel is
    /// never a health target (a driver is loaded for a discovered device,
    /// never the tree root), so it is excluded.
    ///
    /// # Errors
    ///
    /// [`Errno::NotFound`] if no live non-root node has id `node_id`.
    pub fn set_node_health(&self, node_id: u32, health: FaultDomainState) -> Result<(), Errno> {
        {
            let mut inner = self.inner.lock();
            let Some(node) = inner.node_mut(node_id).filter(|node| !node.is_root()) else {
                return Err(Errno::NotFound);
            };
            node.set_fault_health(health);
            inner.generation += 1;
        }
        // Wake parked `hw_tree_wait` callers on the change (see [`Self::seed`]);
        // done after the inner lock is dropped.
        tairix_kernel_core::hw_tree_wake();
        Ok(())
    }

    /// Bump the generation **without** changing the node set, waking every
    /// parked `hw_tree_wait` caller so it re-reads and re-evaluates.
    ///
    /// This is the "re-evaluate now" signal for a reactive observer that
    /// depends on system state the node set does not itself carry — in
    /// particular the user-space `devmgr`, which must re-attempt its
    /// driver-store catalogue fetch once the kernel driver-store service has
    /// bound its endpoint (which happens after the boot tree settles, so no
    /// change to the node set would otherwise wake the parked manager). The node
    /// set is unchanged, so the manager re-matches only when the fetch it
    /// retries succeeds; only the generation advances so the wait observes a
    /// change.
    pub fn bump(&self) {
        {
            let mut inner = self.inner.lock();
            inner.generation += 1;
        }
        // Wake parked `hw_tree_wait` callers (see [`Self::seed`]); done after
        // the inner lock is dropped.
        tairix_kernel_core::hw_tree_wake();
    }

    /// An owned snapshot of the current inventory, in ascending id order, so
    /// the caller holds a stable view that a later mutation cannot change.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfMemory`] when the copy cannot be allocated.
    pub fn snapshot(&self) -> Result<Vec<HwNode>, Errno> {
        let inner = self.inner.lock();
        let mut nodes = Vec::new();
        nodes
            .try_reserve_exact(inner.nodes.len())
            .map_err(|_| Errno::OutOfMemory)?;
        nodes.extend_from_slice(&inner.nodes);
        Ok(nodes)
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

    /// The resource grants of the live non-root node `node_id`, or `None`
    /// when no live non-root node has that id or the grants cannot be copied
    /// (fail closed).
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
        let resources = inner
            .node(node_id)
            .filter(|node| !node.is_root())?
            .resources();
        let mut grants = Vec::new();
        grants.try_reserve_exact(resources.len()).ok()?;
        grants.extend_from_slice(resources);
        Some(grants)
    }

    /// The block-service endpoint resource base ids the live node `node_id`
    /// declares, but **only** when its parent is exactly `parent_id` — a
    /// direct child of the caller's own node. Returns [`Errno::NotFound`]
    /// fail-closed when no live node has that id, or it exists but its parent
    /// is not `parent_id` (the caller does not own it).
    ///
    /// This backs the orderly (stop-if-idle) `hw_remove_node`: the handler
    /// reads a node's declared endpoints to refuse the removal while a volume
    /// is still attached on one of them. The ownership gate is the same one
    /// [`Self::remove_child`] enforces, so a non-owner never learns whether a
    /// node exists or is busy (fail closed). Only
    /// [`HwResourceKind::Endpoint`] resources are returned, by their base id;
    /// a node with no endpoint resource yields an empty vector (it can never
    /// be busy).
    ///
    /// # Errors
    ///
    /// [`Errno::NotFound`] if no live node has id `node_id`, or its parent is
    /// not `parent_id`; [`Errno::OutOfMemory`] when the list cannot be
    /// allocated.
    pub fn node_endpoints(&self, parent_id: u32, node_id: u32) -> Result<Vec<u64>, Errno> {
        let inner = self.inner.lock();
        let node = inner
            .node(node_id)
            .filter(|node| node.parent() == parent_id)
            .ok_or(Errno::NotFound)?;
        let endpoints = || {
            node.resources()
                .iter()
                .filter(|resource| resource.kind() == Some(HwResourceKind::Endpoint))
                .map(HwResource::base)
        };
        let mut bases = Vec::new();
        bases
            .try_reserve_exact(endpoints().count())
            .map_err(|_| Errno::OutOfMemory)?;
        bases.extend(endpoints());
        Ok(bases)
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
    /// Encoded under the store's lock straight into one buffer, so the header
    /// always matches the nodes that follow it and no intermediate copy of
    /// the node list is made. Defined here, beside the store it serialises,
    /// so the wire layout has exactly one encoder.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfMemory`] when the buffer cannot be allocated.
    pub fn encode_snapshot(&self) -> Result<Vec<u8>, Errno> {
        let inner = self.inner.lock();
        let count = inner.nodes.len();
        let len = count
            .checked_mul(HwNode::WIRE_LEN)
            .and_then(|nodes| nodes.checked_add(HwTreeHeader::WIRE_LEN))
            .ok_or(Errno::OutOfMemory)?;
        let mut blob = Vec::new();
        blob.try_reserve_exact(len)
            .map_err(|_| Errno::OutOfMemory)?;
        let header = HwTreeHeader::new(
            inner.generation,
            u64::try_from(count).map_err(|_| Errno::OutOfRange)?,
        );
        blob.extend_from_slice(&header.to_le_bytes());
        for node in &inner.nodes {
            blob.extend_from_slice(&node.to_le_bytes());
        }
        Ok(blob)
    }
}

/// The kernel-wide authoritative hardware inventory.
///
/// Seeded by the boot path, published into by user-space bus drivers, and
/// snapshotted by the autoload reader — the one store all of them share.
pub static HW_TREE: HwTreeStore = HwTreeStore::new();

/// Serialises the host tests that read or mutate [`HW_TREE`].
///
/// The harness runs a crate's tests on several threads, so a test comparing
/// two reads of the shared inventory races a sibling seeding or publishing
/// into it between them. Every test touching `HW_TREE` holds this for its
/// whole body.
#[cfg(test)]
static HW_TREE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Acquire [`HW_TREE_TEST_LOCK`], recovering a poisoned lock so one panicking
/// test cannot wedge every other.
#[cfg(test)]
pub(crate) fn lock_hw_tree_tests() -> std::sync::MutexGuard<'static, ()> {
    HW_TREE_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The [`HwTreeSource`] the boot path installs into the syscall dispatch
/// hook (`BootInfo::with_hw_tree`), backing the `hw_tree_read` /
/// `hw_tree_wait` syscalls with the authoritative [`HW_TREE`].
///
/// A zero-sized adapter: it owns nothing and simply forwards to the one
/// global store, so the single inventory all of the kernel shares is also
/// the one user space observes.
pub struct HwTreeStoreSource;

impl HwNodeLiveness for HwTreeStore {
    fn is_live(&self, node_id: u32) -> bool {
        HwTreeStore::is_live(self, node_id)
    }
}

impl HwNodeLiveness for HwTreeStoreSource {
    fn is_live(&self, node_id: u32) -> bool {
        HW_TREE.is_live(node_id)
    }
}

impl HwTreeSource for HwTreeStoreSource {
    fn generation(&self) -> Result<u64, Errno> {
        Ok(HW_TREE.generation())
    }

    fn snapshot(&self) -> Result<Vec<u8>, Errno> {
        HW_TREE.encode_snapshot()
    }

    fn publish(&self, parent_id: u32, node: HwNode) -> Result<u32, Errno> {
        // Publish the user-space-emitted child under `parent_id` into the one
        // authoritative inventory; `publish_child` refuses a parent the tree no
        // longer holds, assigns the child an id no node has held this boot,
        // sets its parent to the emitter's own node, bumps the generation, and
        // wakes every parked `hw_tree_wait` caller, so the device manager
        // re-reads and autoloads the matching driver. The `hw_emit_node`
        // handler has already verified the caller's `CAP_HW_EMIT`, resolved
        // `parent_id` to the caller's own matched node, and checked that every
        // requested resource is covered by one of its grants. The assigned id
        // flows back to the emitter so it can later retract this child by id.
        HW_TREE.publish_child(parent_id, node)
    }

    fn set_health(&self, node_id: u32, health: FaultDomainState) -> Result<(), Errno> {
        // The handler has already resolved `node_id` to the caller's own
        // matched node and validated `health`; the leaf drivers beneath it
        // read the byte on their recovery path.
        HW_TREE.set_node_health(node_id, health)
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

    fn node(&self, node_id: u32) -> Result<Option<HwNode>, Errno> {
        Ok(HW_TREE.node(node_id))
    }

    fn node_endpoints(&self, parent_id: u32, node_id: u32) -> Result<Vec<u64>, Errno> {
        // Report the node's declared block-service endpoints from the one
        // authoritative inventory, ownership-gated on `parent_id` exactly as
        // `remove` is, so the orderly-removal busy check reasons about the
        // caller's own node and a non-owner learns nothing (fail closed).
        HW_TREE.node_endpoints(parent_id, node_id)
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
    fn seed_tree() -> Vec<HwNode> {
        alloc::vec![
            HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root),
            HwNode::new(2, 1, HwDeviceClass::Bus),
        ]
    }

    /// The bus-enumerated HID child, keyed by the USB interface-class match
    /// key the bring-up reads (never fabricated). Its id and parent are
    /// placeholders the store overwrites.
    fn hid_child() -> HwNode {
        let mut hid = HwNode::new(0, HW_NODE_ROOT, HwDeviceClass::Input);
        hid.push_match_key(HwMatchKey::usb(0x1234, 0x5678, 0x03_01_01))
            .expect("match key fits");
        hid
    }

    /// Publish a placeholder child of `parent` into `store`.
    fn publish(store: &HwTreeStore, parent: u32, class: HwDeviceClass) -> u32 {
        store
            .publish_child(parent, HwNode::new(0, HW_NODE_ROOT, class))
            .expect("an id is free")
    }

    fn snapshot(store: &HwTreeStore) -> Vec<HwNode> {
        store.snapshot().expect("the snapshot fits in memory")
    }

    fn ids(store: &HwTreeStore) -> Vec<u32> {
        snapshot(store).iter().map(HwNode::id).collect()
    }

    #[test]
    fn a_fresh_store_snapshots_empty() {
        let store = HwTreeStore::new();
        assert!(snapshot(&store).is_empty());
    }

    #[test]
    fn a_store_is_seeded_once() {
        // A second seed could drop a node a driver was loaded for, or name a
        // later device by an id an earlier one held.
        let store = HwTreeStore::new();
        store.seed(seed_tree()).expect("a fresh store seeds");
        let before = store.generation();
        assert_eq!(
            store.seed(alloc::vec![HwNode::new(
                1,
                HW_NODE_ROOT,
                HwDeviceClass::Root
            )]),
            Err(Errno::AlreadyExists)
        );
        assert_eq!(ids(&store), [1, 2], "the refused seed dropped nothing");
        assert_eq!(store.generation(), before);
    }

    #[test]
    fn a_seed_naming_one_id_twice_is_refused_whole() {
        let store = HwTreeStore::new();
        assert_eq!(
            store.seed(alloc::vec![
                HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root),
                HwNode::new(5, 1, HwDeviceClass::Storage),
                HwNode::new(5, 1, HwDeviceClass::Network),
            ]),
            Err(Errno::AlreadyExists)
        );
        assert_eq!(
            store.seed(alloc::vec![
                HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root),
                HwNode::new(HW_NODE_ROOT, 1, HwDeviceClass::Bus),
            ]),
            Err(Errno::OutOfRange),
            "the root sentinel names no node"
        );
        assert_eq!(store.generation(), 0, "a refusal changes nothing");
        store
            .seed(seed_tree())
            .expect("a refused seed leaves the store fresh");
    }

    #[test]
    fn a_seed_with_a_node_not_above_its_parent_is_refused_whole() {
        // A removal finds a subtree in one ascending pass, which misses any
        // descendant whose id is below the node it hangs from.
        let store = HwTreeStore::new();
        assert_eq!(
            store.seed(alloc::vec![
                HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root),
                HwNode::new(3, 5, HwDeviceClass::Input),
                HwNode::new(5, 1, HwDeviceClass::Bus),
            ]),
            Err(Errno::OutOfRange)
        );
        assert_eq!(store.generation(), 0, "a refusal changes nothing");
        assert!(snapshot(&store).is_empty());
    }

    #[test]
    fn the_inventory_is_kept_in_ascending_id_order() {
        let store = HwTreeStore::new();
        store
            .seed(alloc::vec![
                HwNode::new(0x8002_0000, 1, HwDeviceClass::Display),
                HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root),
                HwNode::new(0x8000_0000, 1, HwDeviceClass::Storage),
                HwNode::new(2, 1, HwDeviceClass::Bus),
            ])
            .expect("a fresh store seeds");
        assert_eq!(ids(&store), [1, 2, 0x8000_0000, 0x8002_0000]);
        let child = publish(&store, 2, HwDeviceClass::Input);
        assert_eq!(
            ids(&store),
            [1, 2, 0x8000_0000, 0x8002_0000, child],
            "a published node carries the highest id yet"
        );
        assert!(store.is_live(0x8000_0000));
        assert!(!store.is_live(3), "an id between two live ones is not live");
    }

    #[test]
    fn publish_child_assigns_a_fresh_id_and_the_resolved_parent() {
        let store = HwTreeStore::new();
        store.seed(seed_tree()).expect("a fresh store seeds"); // ids 1 (root) and 2 (bus)

        // The emitter supplies a node whose id/parent are placeholders; the
        // store owns identity and overwrites both.
        let id = store.publish_child(2, hid_child()).expect("an id is free");
        assert_eq!(id, 3, "the first emitted id is above every seeded one");
        let snap = snapshot(&store);
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[2].id(), 3, "the store assigned the id");
        assert_eq!(snap[2].parent(), 2, "parented under the resolved parent");
        assert_eq!(snap[2].match_keys().len(), 1, "the emitter's data is kept");

        // A second publish never reuses an id, even under the same parent.
        assert_eq!(publish(&store, 2, HwDeviceClass::Input), 4);
    }

    #[test]
    fn a_child_is_never_published_under_a_node_the_tree_no_longer_holds() {
        // A driver whose node was surprise-removed may still run until the
        // device manager unloads it. A child it published under the absent
        // node could never be removed or retired, yet would get a driver and
        // DMA custody of its own.
        let store = HwTreeStore::new();
        store.seed(seed_tree()).expect("a fresh store seeds");
        let child = publish(&store, 2, HwDeviceClass::Bus);
        assert_eq!(store.remove_child(2, child), Ok(alloc::vec![child]));
        let before = store.generation();
        assert_eq!(
            store.publish_child(child, hid_child()),
            Err(Errno::NotFound)
        );
        assert_eq!(
            store.publish_child(4242, hid_child()),
            Err(Errno::NotFound),
            "nor under one that never existed"
        );
        assert_eq!(ids(&store), [1, 2], "nothing was added");
        assert_eq!(store.generation(), before);
    }

    #[test]
    fn a_removed_nodes_id_is_never_issued_again() {
        // A reissued id would let one device's reset or removal speak for
        // another's memory.
        let store = HwTreeStore::new();
        store.seed(seed_tree()).expect("a fresh store seeds");
        let first = publish(&store, 2, HwDeviceClass::Storage);
        assert_eq!(store.remove_child(2, first), Ok(alloc::vec![first]));
        assert!(!store.is_live(first));
        let second = publish(&store, 2, HwDeviceClass::Storage);
        assert_ne!(second, first, "the next device gets an id of its own");
        assert!(second > first);
    }

    #[test]
    fn a_published_id_stays_above_a_removed_seeded_one() {
        // Removing the highest seeded node does not lower the next id issued.
        let store = HwTreeStore::new();
        store
            .seed(alloc::vec![
                HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root),
                HwNode::new(2, 1, HwDeviceClass::Bus),
                HwNode::new(0x8000_0007, 2, HwDeviceClass::Storage),
            ])
            .expect("a fresh store seeds");
        assert_eq!(
            store.remove_child(2, 0x8000_0007),
            Ok(alloc::vec![0x8000_0007])
        );
        assert_eq!(publish(&store, 2, HwDeviceClass::Input), 0x8000_0008);
    }

    #[test]
    fn publishing_fails_closed_once_the_id_space_is_spent() {
        let store = HwTreeStore::new();
        store
            .seed(alloc::vec![
                HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root),
                HwNode::new(HW_NODE_ROOT - 1, 1, HwDeviceClass::Bus),
            ])
            .expect("a fresh store seeds");
        let before = store.generation();
        assert_eq!(
            store.publish_child(1, HwNode::new(0, HW_NODE_ROOT, HwDeviceClass::Input)),
            Err(Errno::NoSpace),
            "the root sentinel is never issued, nor is any id reused"
        );
        assert_eq!(snapshot(&store).len(), 2, "nothing was added");
        assert_eq!(store.generation(), before);
    }

    #[test]
    fn liveness_follows_the_inventory() {
        let store = HwTreeStore::new();
        store.seed(seed_tree()).expect("a fresh store seeds");
        assert!(store.is_live(2));
        let child = publish(&store, 2, HwDeviceClass::Input);
        assert!(store.is_live(child));
        assert_eq!(store.remove_child(2, child), Ok(alloc::vec![child]));
        assert!(!store.is_live(child));
        assert!(!store.is_live(4242), "an id never issued is not live");
    }

    #[test]
    fn remove_child_drops_the_node_and_its_subtree() {
        let store = HwTreeStore::new();
        store.seed(seed_tree()).expect("a fresh store seeds"); // ids 1 (root) and 2 (bus)

        // bus 2 publishes child 3; child 3 publishes grandchild 4.
        let child = publish(&store, 2, HwDeviceClass::Bus);
        assert_eq!(child, 3);
        let grandchild = publish(&store, 3, HwDeviceClass::Input);
        assert_eq!(grandchild, 4);
        assert_eq!(snapshot(&store).len(), 4);

        // Removing child 3 (owned by bus 2) takes grandchild 4 with it, so a
        // stale descendant never outlives its parent — and both removed ids
        // are reported so per-node kernel state can be retired.
        assert_eq!(store.remove_child(2, 3), Ok(alloc::vec![3, 4]));
        assert_eq!(ids(&store), [1, 2], "only root and bus remain");
    }

    #[test]
    fn a_node_is_found_by_id_while_it_lives() {
        let store = HwTreeStore::new();
        store
            .seed(alloc::vec![
                HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root),
                HwNode::new(2, 1, HwDeviceClass::Bus),
            ])
            .expect("a fresh store seeds");
        let child = publish(&store, 2, HwDeviceClass::Input);
        assert_eq!(
            store.node(child).map(|node| (node.id(), node.parent())),
            Some((child, 2))
        );
        assert_eq!(store.node(child + 1), None, "an id never issued");
        store.remove_child(2, child).expect("removes");
        assert_eq!(store.node(child), None, "a removed node is gone");
    }

    #[test]
    fn remove_child_takes_every_descendant_and_no_sibling() {
        // Descendants and bystanders interleave in id order, as publishes
        // from different buses do.
        let store = HwTreeStore::new();
        store
            .seed(alloc::vec![
                HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root),
                HwNode::new(2, 1, HwDeviceClass::Bus),
                HwNode::new(3, 1, HwDeviceClass::Bus),
            ])
            .expect("a fresh store seeds");
        let child = publish(&store, 2, HwDeviceClass::Bus);
        let grandchild = publish(&store, child, HwDeviceClass::Bus);
        let bystander = publish(&store, 3, HwDeviceClass::Input);
        let great = publish(&store, grandchild, HwDeviceClass::Input);
        let sibling = publish(&store, 2, HwDeviceClass::Input);

        assert_eq!(
            store.remove_child(2, child),
            Ok(alloc::vec![child, grandchild, great])
        );
        assert_eq!(ids(&store), [1, 2, 3, bystander, sibling]);
    }

    #[test]
    fn a_driver_may_retire_a_seeded_child_of_its_node() {
        // Discovery seeds a bus's children under it, so the bus's driver owns
        // them exactly as it owns the ones it publishes.
        let store = HwTreeStore::new();
        store
            .seed(alloc::vec![
                HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root),
                HwNode::new(2, 1, HwDeviceClass::Bus),
                HwNode::new(3, 2, HwDeviceClass::Storage),
                HwNode::new(4, 3, HwDeviceClass::Storage),
            ])
            .expect("a fresh store seeds");
        assert_eq!(store.remove_child(2, 3), Ok(alloc::vec![3, 4]));
        assert_eq!(ids(&store), [1, 2]);
    }

    #[test]
    fn remove_child_fails_closed_for_an_unowned_or_absent_node() {
        let store = HwTreeStore::new();
        store.seed(seed_tree()).expect("a fresh store seeds"); // ids 1 (root) and 2 (bus)
        let child = publish(&store, 2, HwDeviceClass::Input);
        assert_eq!(child, 3);

        // A node that exists but whose parent is not the claimed one: the
        // caller does not own it, so removal fails closed.
        assert_eq!(store.remove_child(99, 3), Err(Errno::NotFound));
        // An absent id fails closed identically — the two are
        // indistinguishable to the caller.
        assert_eq!(store.remove_child(2, 4242), Err(Errno::NotFound));
        // The failed removals left the inventory untouched.
        assert_eq!(snapshot(&store).len(), 3);
    }

    #[test]
    fn remove_child_bumps_the_generation_and_a_failure_does_not() {
        let store = HwTreeStore::new();
        store.seed(seed_tree()).expect("a fresh store seeds");
        let child = publish(&store, 2, HwDeviceClass::Input);
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
    fn set_node_health_records_health_and_bumps_the_generation() {
        let store = HwTreeStore::new();
        store.seed(seed_tree()).expect("a fresh store seeds"); // ids 1 (root) and 2 (bus)
        let before = store.generation();

        // A live non-root node's health is recorded and the generation bumps
        // so a parked `hw_tree_wait` caller (the device manager) wakes.
        assert_eq!(
            store.set_node_health(2, FaultDomainState::Recovering),
            Ok(())
        );
        assert_eq!(store.generation(), before + 1);
        let snap = snapshot(&store);
        assert_eq!(snap[1].id(), 2);
        assert_eq!(snap[1].fault_health(), FaultDomainState::Recovering);
        // The node set is untouched — a health update is *not* a removal.
        assert_eq!(snap.len(), 2);

        // A recovery clears it back to Healthy.
        assert_eq!(store.set_node_health(2, FaultDomainState::Healthy), Ok(()));
        assert_eq!(
            snapshot(&store)[1].fault_health(),
            FaultDomainState::Healthy
        );
    }

    #[test]
    fn set_node_health_fails_closed_for_the_root_or_an_absent_node() {
        let store = HwTreeStore::new();
        store.seed(seed_tree()).expect("a fresh store seeds"); // ids 1 (root) and 2 (bus)
        let before = store.generation();

        // The tree root is never a health target.
        assert_eq!(
            store.set_node_health(1, FaultDomainState::Recovering),
            Err(Errno::NotFound)
        );
        // An absent id fails closed identically.
        assert_eq!(
            store.set_node_health(4242, FaultDomainState::Offline),
            Err(Errno::NotFound)
        );
        // A fail-closed health update changes nothing, including the generation.
        assert_eq!(store.generation(), before);
    }

    #[test]
    fn a_snapshot_is_stable_across_a_later_mutation() {
        let store = HwTreeStore::new();
        store.seed(seed_tree()).expect("a fresh store seeds");
        let before = snapshot(&store);
        // A mutation after the snapshot does not change the owned view.
        store.publish_child(2, hid_child()).expect("an id is free");
        assert_eq!(before.len(), 2, "the earlier snapshot is unaffected");
        assert_eq!(snapshot(&store).len(), 3, "the live store grew");
    }

    #[test]
    fn generation_starts_at_zero_and_increases_monotonically_on_every_mutation() {
        let store = HwTreeStore::new();
        assert_eq!(store.generation(), 0, "a fresh store is generation 0");

        store.seed(seed_tree()).expect("a fresh store seeds");
        assert_eq!(store.generation(), 1, "seed bumps the generation");

        let child = store.publish_child(2, hid_child()).expect("an id is free");
        assert_eq!(store.generation(), 2, "publish bumps the generation");

        store
            .set_node_health(2, FaultDomainState::Recovering)
            .expect("a live node");
        assert_eq!(store.generation(), 3, "a health change bumps it");

        store.remove_child(2, child).expect("an owned child");
        assert_eq!(store.generation(), 4, "a removal bumps it");

        store.bump();
        assert_eq!(store.generation(), 5, "so does a bare re-evaluate signal");

        // A refused mutation changes nothing a waiter could observe.
        assert!(store.seed(seed_tree()).is_err());
        assert!(store.publish_child(child, hid_child()).is_err());
        assert_eq!(store.generation(), 5);
    }

    #[test]
    fn encode_snapshot_round_trips_header_and_nodes() {
        let store = HwTreeStore::new();
        store.seed(seed_tree()).expect("a fresh store seeds");
        store.publish_child(2, hid_child()).expect("an id is free");

        let blob = store
            .encode_snapshot()
            .expect("the snapshot fits in memory");
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
        assert_eq!(decoded, snapshot(&store), "nodes round-trip exactly");
    }

    #[test]
    fn the_static_source_forwards_to_the_global_store() {
        // Each assertion reads the shared inventory twice, so a sibling
        // mutating it in between would fail a forwarder that is correct.
        let _serial = lock_hw_tree_tests();
        // The adapter is a pure forwarder: its generation and snapshot are
        // whatever the global `HW_TREE` currently holds.
        assert_eq!(HW_TREE_SOURCE.generation(), Ok(HW_TREE.generation()));
        assert_eq!(HW_TREE_SOURCE.snapshot(), HW_TREE.encode_snapshot());
    }
}
