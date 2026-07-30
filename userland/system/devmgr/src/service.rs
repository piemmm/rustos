//! The device manager's reactive match-and-load loop.
//!
//! The `Run` service fetches the kernel-decoded driver catalogue once (the
//! read-only `/System` store is static for the life of the system), then reads the discovered hardware tree, loads a
//! driver for every node that matches a catalogue bundle, and **blocks**
//! until the tree changes and re-reads it — the reactive discovery the
//! hotplug model requires. Both halves are pure with respect to the kernel:
//! the loop reads/waits through the [`HwTreeService`] seam and fetches/loads
//! through the [`DriverStoreCall`] seam, so its logic — fetch once, then
//! match-and-load on every generation advance — is exercised on the host
//! against scripted doubles, independently of the freestanding
//! `hw_tree_read` / `hw_tree_wait` / `ipc_call` syscalls it binds in
//! production.
//!
//! The loop never busy-spins: [`HwTreeService::wait_for_change`]
//! blocks until the store's generation advances. A failure in a tree-seam
//! operation ends the loop fail-closed with the reported [`Errno`]; a catalogue that cannot be fetched is fail-soft —
//! the service loads nothing but keeps observing.

use alloc::vec::Vec;

use tairix_abi::{Errno, HwNode, HwTreeHeader};
use tairix_devmatch::DriverCandidate;
use tairix_log::{log as log_event, Event, Level, Sink};

use crate::autoload::{match_and_load, unload_vanished, AutoloadState};
use crate::events;
use crate::netbind::{bind_new_channels, NetBindState, NetstackBind};
use crate::netcfg::{
    deliver_interface_configs, deliver_network_settings, NetConfigState, NetIfConfigState,
    NetworkConfigSource, NetworkInterfaceConfigSource,
};
use crate::observe::for_each_node;
use crate::store::{fetch_catalogue, CatalogueDriver, DriverStoreCall};

/// The kernel-facing hardware-tree operations the reactive loop performs,
/// abstracted so the loop is host-testable against a scripted double.
///
/// The production implementation (the freestanding `devmgr` `Run` binary)
/// backs these with the `hw_tree_read` / `hw_tree_wait` `abi-v1` syscalls
/// and writes node reports to its inherited diagnostic stream (fd 2).
pub trait HwTreeService {
    /// Read the current hardware-tree snapshot into `buf`, returning the
    /// number of bytes written (a [`HwTreeHeader`] followed by its node
    /// records). Fails closed with the reported [`Errno`] — an undersized
    /// buffer is [`Errno::BufferTooSmall`], never a truncated read.
    fn read_tree(&mut self, buf: &mut [u8]) -> Result<usize, Errno>;

    /// Block until the store's generation advances past `last_generation`
    /// (reactive re-match and hotplug). Returns once
    /// the tree has changed, or fails closed with the reported [`Errno`].
    fn wait_for_change(&mut self, last_generation: u64) -> Result<(), Errno>;

    /// Report the decoded snapshot header (its generation and node count)
    /// after a read.
    fn on_header(&mut self, header: &HwTreeHeader);

    /// Report one decoded node of the snapshot, in wire order.
    fn on_node(&mut self, node: &HwNode);
}

/// Initial size of the growable hardware-tree snapshot buffer.
///
/// This is a *starting capacity*, never a ceiling: [`read_tree_growing`] doubles the buffer and retries whenever the
/// kernel reports the discovered tree does not fit, so a machine whose
/// device tree is larger than this — a real board's full firmware tree has
/// far more nodes than QEMU `virt`'s handful — is read in full rather than
/// failing. It is sized as a generous one-read fit for a typical discovered
/// tree (a `HwNode` is `HwNode::WIRE_LEN` bytes), so the common case takes a
/// single read and only an unusually large tree pays for a grow.
const INITIAL_TREE_SNAPSHOT_BYTES: usize = 64 * 1024;

/// Read the current hardware-tree snapshot into `buf`, **growing** `buf`
/// until the whole snapshot fits.
///
/// `hw_tree_read` returns the entire snapshot or [`Errno::BufferTooSmall`]
/// — it never truncates and does not report the size it
/// needs — so a buffer too small for the discovered tree is doubled and the
/// read retried until it fits. The hardware tree is a *discovered capacity*,
/// not a fixed ceiling: the device manager grows before it fails, so a board with a larger tree than
/// [`INITIAL_TREE_SNAPSHOT_BYTES`] is read in full rather than aborting the
/// service. Genuine exhaustion still fails closed — the underlying
/// allocation failure surfaces as the runtime's OOM, and an arithmetic
/// overflow of the doubling is [`Errno::OutOfRange`].
///
/// # Errors
///
/// Any [`Errno`] other than [`Errno::BufferTooSmall`] from
/// [`HwTreeService::read_tree`] is propagated fail-closed; only
/// `BufferTooSmall` triggers a grow-and-retry.
fn read_tree_growing<T: HwTreeService>(tree: &mut T, buf: &mut Vec<u8>) -> Result<usize, Errno> {
    loop {
        if buf.is_empty() {
            buf.resize(INITIAL_TREE_SNAPSHOT_BYTES, 0);
        }
        match tree.read_tree(buf.as_mut_slice()) {
            Ok(len) => return Ok(len),
            Err(Errno::BufferTooSmall) => {
                // Double and retry. `buf` is non-empty here (resized above),
                // so the new length is strictly larger; an overflow of the
                // doubling fails closed rather than wrapping.
                let grown = buf.len().checked_mul(2).ok_or(Errno::OutOfRange)?;
                buf.resize(grown, 0);
            }
            Err(err) => return Err(err),
        }
    }
}

/// (Re)fetch the catalogue while it has not yet been obtained, then read the
/// current tree through `tree` (reporting and collecting its nodes) and
/// match-and-load each node through `store`, returning the generation the
/// snapshot was taken at.
///
/// The catalogue is retried while `catalogue` is [`None`]: the kernel store
/// service binds its endpoint *after* the boot tree settles, so a fetch
/// issued before the bind fails (the endpoint is unbound) — the kernel then
/// bumps the tree generation when it binds, waking this loop to retry. Until the catalogue is obtained, matching runs
/// against an empty candidate set, so every node is observed and left
/// unbound, then loaded on the re-evaluation once the store is reachable.
///
/// # Errors
///
/// Propagates the [`Errno`] from [`HwTreeService::read_tree`] or from the
/// fail-closed [`for_each_node`] decode; on any error no header is reported
/// and no node is loaded. A catalogue-fetch failure is
/// **not** propagated — it is fail-soft (logged, retried).
#[allow(clippy::too_many_arguments)]
fn react_once<T: HwTreeService, C: DriverStoreCall>(
    tree: &mut T,
    store: &mut C,
    netstack: &mut dyn NetstackBind,
    netcfg: &mut dyn NetworkConfigSource,
    netifcfg: &mut dyn NetworkInterfaceConfigSource,
    netbind: &mut NetBindState,
    netconfig: &mut NetConfigState,
    netifconfig: &mut NetIfConfigState,
    catalogue: &mut Option<Vec<CatalogueDriver>>,
    state: &mut AutoloadState,
    tree_buf: &mut Vec<u8>,
    reply_buf: &mut [u8],
    sink: &dyn Sink,
) -> Result<u64, Errno> {
    if catalogue.is_none() {
        match fetch_catalogue(store, reply_buf) {
            Ok(fetched) => *catalogue = Some(fetched),
            Err(_) => {
                // Fail-soft: no store served yet (unbound endpoint) or an
                // unreadable store loads nothing this cycle, but the service
                // keeps observing and retries on the next generation bump.
                log_event(
                    sink,
                    &Event {
                        level: Level::Warn,
                        id: events::DRIVER_STORE_UNAVAILABLE,
                        message: "driver-store catalogue unavailable; retrying on re-evaluation",
                        fields: &[],
                    },
                );
            }
        }
    }
    let len = read_tree_growing(tree, tree_buf)?;
    // Decode the snapshot once: report each node and collect it (`HwNode`
    // is `Copy`), so the immutable borrow of `tree_buf` ends before the
    // match-and-load pass writes into the disjoint `reply_buf`.
    let mut nodes: Vec<HwNode> = Vec::new();
    let header = for_each_node(&tree_buf[..len], |node| {
        tree.on_node(node);
        nodes.push(*node);
    })?;
    tree.on_header(&header);
    // Match against the obtained catalogue, or an empty set while it is not
    // yet available (every node observed and left unbound until the store
    // binds).
    let drivers: &[CatalogueDriver] = catalogue.as_deref().unwrap_or(&[]);
    let candidates: Vec<DriverCandidate<'_>> =
        drivers.iter().map(CatalogueDriver::candidate).collect();
    match_and_load(&nodes, drivers, &candidates, store, reply_buf, state, sink);
    // Deliver the stack-wide `net.*` policy to the network stack once,
    // before binding any channel, so a freshly-bound interface adopts it at
    // construction. Fail-soft: an unreadable store (pre-unlock) or a stack
    // not yet up is retried on the next generation bump.
    deliver_network_settings(netcfg, netconfig, netstack, sink);
    // Hand each newly-discovered NIC device channel (a `netchan` node a bound
    // NIC driver emitted) to the network stack, idempotently (each endpoint
    // once) and fail-soft (a stack not yet up is retried next bump).
    bind_new_channels(&nodes, netbind, netstack, sink);
    // Deliver each managed interface's `network.conf` configuration to the
    // network stack. Runs *after* `bind_new_channels` so an interface that
    // just bound this cycle can be matched (by MAC) and configured in the
    // same reaction; an interface not yet bound is retried on the next bump.
    deliver_interface_configs(netifcfg, netifconfig, netstack, sink);
    // Hot-removal reaction: a bound node missing from this snapshot means its
    // device is gone, so tear its driver down. The same generation-bump path
    // that loads a newly-appeared node's driver unloads a vanished one's
    // (re-plug then re-loads it), so connect and disconnect are symmetric.
    let present: alloc::collections::BTreeSet<u32> = nodes.iter().map(HwNode::id).collect();
    // The interior nodes whose fault domain is *recovering* this snapshot: a
    // hub/controller mid-reset transiently drops its children, so a vanished
    // child under a recovering owner is held (one recovery episode across the
    // subtree, not N spurious teardowns — `plans/FIX-IO.md` IO4). A node
    // reports its own domain health; a leaf device is always Healthy.
    let recovering: alloc::collections::BTreeSet<u32> = nodes
        .iter()
        .filter(|node| node.fault_health() == tairix_abi::blkio::FaultDomainState::Recovering)
        .map(HwNode::id)
        .collect();
    unload_vanished(
        &|id| present.contains(&id),
        &|owner| recovering.contains(&owner),
        store,
        reply_buf,
        state,
        sink,
    );
    Ok(header.generation())
}

/// Run the reactive match-and-load loop: read the tree, load a driver for
/// every matched node, and block on every generation advance to re-match.
///
/// * `tree` — the hardware-tree read/wait seam.
/// * `store` — the driver-store catalogue/load seam.
/// * `sink` — the audit sink every match/load decision is logged through.
/// * `reply_buf` — the buffer the catalogue and each load reply are received
///   into. The tree snapshot is read into a separate, service-owned
///   buffer that grows to fit the discovered tree (`read_tree_growing`), so the caller never picks a tree-size ceiling and a
///   load (writing `reply_buf`) never clobbers the snapshot mid-decode.
/// * `budget` — bounds the number of *reactions* (re-reads after a change):
///   [`None`] runs for the life of the service (the production device
///   manager waits forever), while [`Some(n)`](Some) returns [`Ok`] after
///   `n` reactions — the bounded form the host tests drive. The initial read
///   is always performed before the first wait.
///
/// The catalogue is fetched lazily and retried while it has not been
/// obtained: the kernel store service binds its endpoint after the boot tree
/// settles, so the first fetch may fail and is retried on the re-evaluation
/// the kernel triggers when it binds (once
/// obtained, the static read-only store is not re-fetched).
///
/// # Errors
///
/// Returns the first [`Errno`] a *tree-seam* operation reports
/// ([`HwTreeService::read_tree`] / [`HwTreeService::wait_for_change`]) or a
/// snapshot decode failure; the loop is fail-closed and
/// never silently continues past such an error. A catalogue-fetch failure is
/// fail-soft, not propagated.
#[allow(clippy::too_many_arguments)]
pub fn run<T: HwTreeService, C: DriverStoreCall>(
    tree: &mut T,
    store: &mut C,
    netstack: &mut dyn NetstackBind,
    netcfg: &mut dyn NetworkConfigSource,
    netifcfg: &mut dyn NetworkInterfaceConfigSource,
    sink: &dyn Sink,
    reply_buf: &mut [u8],
    budget: Option<u32>,
) -> Result<(), Errno> {
    let mut catalogue: Option<Vec<CatalogueDriver>> = None;
    // The memory of whether the stack-wide `net.*` policy has been
    // delivered to the network stack (once, `plans/NETWORK.md` N9b-2).
    let mut netconfig = NetConfigState::new();
    // The memory of which per-interface `network.conf` configurations have
    // been delivered (each when its interface binds, `plans/NETWORK.md`
    // N9b-3-1).
    let mut netifconfig = NetIfConfigState::new();
    // The memory of which NIC device channels have been handed to the
    // network stack: each `netchan` endpoint is bound exactly once across
    // every generation bump.
    let mut netbind = NetBindState::new();
    // The loaded-bundle cache plus the per-node decision memory: a
    // re-evaluation of a settled tree re-emits no audit line. The device manager re-matches the whole snapshot on every
    // generation advance, and without the decision memory each pass would
    // re-log every unbound node, flooding the slow diagnostic serial line and
    // stalling the boot.
    let mut state = AutoloadState::default();
    // The snapshot buffer the service owns for its lifetime: it starts
    // empty and `read_tree_growing` sizes it to the discovered tree on the
    // first read, growing it later only if the tree ever grows past it
    // (no caller-picked ceiling).
    let mut tree_buf: Vec<u8> = Vec::new();

    let mut last_generation = react_once(
        tree,
        store,
        netstack,
        netcfg,
        netifcfg,
        &mut netbind,
        &mut netconfig,
        &mut netifconfig,
        &mut catalogue,
        &mut state,
        &mut tree_buf,
        reply_buf,
        sink,
    )?;
    let mut reactions = 0u32;
    loop {
        if budget.is_some_and(|max| reactions >= max) {
            return Ok(());
        }
        tree.wait_for_change(last_generation)?;
        last_generation = react_once(
            tree,
            store,
            netstack,
            netcfg,
            netifcfg,
            &mut netbind,
            &mut netconfig,
            &mut netifconfig,
            &mut catalogue,
            &mut state,
            &mut tree_buf,
            reply_buf,
            sink,
        )?;
        reactions += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloc::vec;
    use core::cell::RefCell;

    use tairix_abi::driver_store::{
        encode_catalogue_reply, encode_load_reply, encode_unload_reply, StoreRequest,
    };
    use tairix_abi::hwtree::{HwDeviceClass, HwMatchKey, HW_NODE_ROOT};
    use tairix_abi::DriverBindKey;

    /// Encode `[HwTreeHeader][HwNode; n]` exactly as the kernel store does.
    fn encode(generation: u64, nodes: &[HwNode]) -> Vec<u8> {
        let mut blob = Vec::new();
        blob.extend_from_slice(&HwTreeHeader::new(generation, nodes.len() as u64).to_le_bytes());
        for node in nodes {
            blob.extend_from_slice(&node.to_le_bytes());
        }
        blob
    }

    fn input_node(id: u32, key: HwMatchKey) -> HwNode {
        let mut node = HwNode::new(id, 1, HwDeviceClass::Input);
        node.push_match_key(key).expect("key fits");
        node
    }

    /// A scripted hardware-tree seam: hands out a queued snapshot on each
    /// `read_tree`, records the generations it waited past and the node ids
    /// it reported, and fails closed once its script is exhausted.
    struct ScriptedTree {
        snapshots: Vec<Vec<u8>>,
        next: usize,
        waited_on: Vec<u64>,
        reported_nodes: Vec<u32>,
        wait_error: Option<Errno>,
    }

    impl ScriptedTree {
        fn new(snapshots: Vec<Vec<u8>>) -> Self {
            Self {
                snapshots,
                next: 0,
                waited_on: Vec::new(),
                reported_nodes: Vec::new(),
                wait_error: None,
            }
        }
    }

    impl HwTreeService for ScriptedTree {
        fn read_tree(&mut self, buf: &mut [u8]) -> Result<usize, Errno> {
            let snapshot = self.snapshots.get(self.next).ok_or(Errno::NotFound)?;
            self.next += 1;
            if buf.len() < snapshot.len() {
                return Err(Errno::BufferTooSmall);
            }
            buf[..snapshot.len()].copy_from_slice(snapshot);
            Ok(snapshot.len())
        }

        fn wait_for_change(&mut self, last_generation: u64) -> Result<(), Errno> {
            if let Some(err) = self.wait_error {
                return Err(err);
            }
            self.waited_on.push(last_generation);
            Ok(())
        }

        fn on_header(&mut self, _header: &HwTreeHeader) {}

        fn on_node(&mut self, node: &HwNode) {
            self.reported_nodes.push(node.id());
        }
    }

    /// A scripted driver-store seam: frames a fixed catalogue on a
    /// `Catalogue` request, and on a `Load` records `(bundle_id, node_id)`
    /// and frames a per-bundle handle — the in-memory analogue of the kernel
    /// server's `build_reply`, so the client's framing round-trips against a
    /// real wire reply.
    struct ScriptedStore {
        catalogue: Vec<(u32, Vec<DriverBindKey>)>,
        loads: RefCell<Vec<(u32, u32)>>,
        unloads: RefCell<Vec<u64>>,
    }

    impl DriverStoreCall for ScriptedStore {
        fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
            match StoreRequest::decode(request)? {
                StoreRequest::Catalogue => {
                    let entries: Vec<(u32, &[DriverBindKey])> = self
                        .catalogue
                        .iter()
                        .map(|(id, keys)| (*id, keys.as_slice()))
                        .collect();
                    encode_catalogue_reply(reply, &entries)
                }
                StoreRequest::Load { bundle_id, node_id } => {
                    self.loads.borrow_mut().push((bundle_id, node_id));
                    // A distinct, non-zero handle per load: every load spawns
                    // its own instance, so handles are per-instance unique.
                    let seq = self.loads.borrow().len() as u64;
                    encode_load_reply(reply, 0x1000 + seq)
                }
                StoreRequest::Unload { handle } => {
                    self.unloads.borrow_mut().push(handle);
                    encode_unload_reply(reply)
                }
                // The reactive-loop tests drive the catalogue/load/unload
                // path only; a config request against this double carries no
                // file, so it answers the fail-closed `NotFound` the real
                // server sends for an absent file (the loop tests use no-op
                // config sources, so this is never reached in practice).
                StoreRequest::ReadConfig { .. } => {
                    tairix_abi::driver_store::encode_error_reply(reply, Errno::NotFound)
                }
            }
        }
    }

    /// A store whose catalogue fetch fails in band (the kernel framed an
    /// error reply, e.g. an unreadable store).
    struct FailingCatalogue;

    impl DriverStoreCall for FailingCatalogue {
        fn call(&mut self, _request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
            tairix_abi::driver_store::encode_error_reply(reply, Errno::PermissionDenied)
        }
    }

    struct RecordingSink {
        ids: RefCell<Vec<u32>>,
    }

    impl RecordingSink {
        fn new() -> Self {
            Self {
                ids: RefCell::new(Vec::new()),
            }
        }
        fn ids(&self) -> Vec<u32> {
            self.ids.borrow().clone()
        }
    }

    impl Sink for RecordingSink {
        fn write_event(&self, event: &Event<'_>) {
            self.ids.borrow_mut().push(event.id.0);
        }
    }

    /// A no-op [`NetstackBind`] for the loop tests, whose hardware trees
    /// carry no `netchan` node — so `bind_new_channels` never calls it. The
    /// netchan hand-off policy itself is tested directly in `crate::netbind`.
    struct NoNetstack;
    impl NetstackBind for NoNetstack {
        fn bind_driver(
            &mut self,
            _endpoint_id: u64,
            _iface: &[u8; tairix_abi::net_ipc::IF_NAME_LEN],
            _node_location: u64,
        ) -> Result<(), Errno> {
            Ok(())
        }

        fn apply_settings(
            &mut self,
            _settings: tairix_abi::net_ipc::NetworkSettings,
        ) -> Result<(), Errno> {
            Ok(())
        }

        fn apply_interface_config(
            &mut self,
            _config: &tairix_abi::net_ipc::NetInterfaceConfigMsg,
        ) -> Result<(), Errno> {
            Ok(())
        }

        fn apply_bond_config(
            &mut self,
            _config: &tairix_abi::net_ipc::NetBondConfigMsg,
        ) -> Result<(), Errno> {
            Ok(())
        }
    }

    /// A no-op [`NetworkConfigSource`] for the loop tests: it never yields a
    /// policy, so `deliver_network_settings` is a no-op. The delivery policy
    /// itself is tested directly in `crate::netcfg`.
    struct NoConfig;
    impl NetworkConfigSource for NoConfig {
        fn load(&mut self) -> Option<tairix_abi::net_ipc::NetworkSettings> {
            None
        }
    }

    /// A no-op [`NetworkInterfaceConfigSource`] for the loop tests: it never
    /// yields a plan, so `deliver_interface_configs` is a no-op. The delivery
    /// policy itself is tested directly in `crate::netcfg`.
    struct NoIfConfig;
    impl NetworkInterfaceConfigSource for NoIfConfig {
        fn load(&mut self) -> Option<crate::netcfg::InterfaceConfigPlan> {
            None
        }
    }

    fn bind(priority: u16, key: HwMatchKey) -> DriverBindKey {
        DriverBindKey::new(priority, key)
    }

    #[test]
    fn the_first_cycle_loads_a_driver_for_every_matched_node() {
        let kbd = HwMatchKey::virtio(0x1234);
        let snapshot = encode(
            1,
            &[
                HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root),
                input_node(2, kbd),
            ],
        );
        let mut tree = ScriptedTree::new(vec![snapshot]);
        let mut store = ScriptedStore {
            catalogue: vec![(7, vec![bind(5, kbd)])],
            loads: RefCell::new(Vec::new()),
            unloads: RefCell::new(Vec::new()),
        };
        let sink = RecordingSink::new();
        let mut reply_buf = [0u8; 4096];

        run(
            &mut tree,
            &mut store,
            &mut NoNetstack,
            &mut NoConfig,
            &mut NoIfConfig,
            &sink,
            &mut reply_buf,
            Some(0),
        )
        .expect("the initial cycle runs");

        // Node 2 matched bundle 7 and was loaded for that node id.
        assert_eq!(store.loads.borrow().as_slice(), &[(7, 2)]);
        assert!(
            sink.ids().contains(&events::NODE_BOUND.0),
            "{:?}",
            sink.ids()
        );
    }

    #[test]
    fn an_unmatched_node_is_left_unbound_and_never_loaded() {
        // `NODE_UNBOUND` is a `Debug` record (filtered out on a default `Info`
        // boot); lower the threshold so the test observes it.
        tairix_log::set_max_level(tairix_log::Level::Trace);
        let snapshot = encode(
            1,
            &[
                HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root),
                input_node(2, HwMatchKey::virtio(0xFFFF)),
            ],
        );
        let mut tree = ScriptedTree::new(vec![snapshot]);
        let mut store = ScriptedStore {
            catalogue: vec![(7, vec![bind(5, HwMatchKey::virtio(0x1234))])],
            loads: RefCell::new(Vec::new()),
            unloads: RefCell::new(Vec::new()),
        };
        let sink = RecordingSink::new();
        let mut reply_buf = [0u8; 4096];

        run(
            &mut tree,
            &mut store,
            &mut NoNetstack,
            &mut NoConfig,
            &mut NoIfConfig,
            &sink,
            &mut reply_buf,
            Some(0),
        )
        .expect("the initial cycle runs");

        assert!(store.loads.borrow().is_empty(), "no node matched");
        assert!(sink.ids().contains(&events::NODE_UNBOUND.0));
    }

    #[test]
    fn a_bundle_matched_by_two_nodes_loads_one_instance_per_node() {
        let key = HwMatchKey::compatible(b"arm,pl011").expect("compatible fits");
        let snapshot = encode(
            1,
            &[
                HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root),
                {
                    let mut n = HwNode::new(2, 1, HwDeviceClass::Serial);
                    n.push_match_key(key).expect("key fits");
                    n
                },
                {
                    let mut n = HwNode::new(3, 1, HwDeviceClass::Serial);
                    n.push_match_key(key).expect("key fits");
                    n
                },
            ],
        );
        let mut tree = ScriptedTree::new(vec![snapshot]);
        let mut store = ScriptedStore {
            catalogue: vec![(4, vec![bind(2, key)])],
            loads: RefCell::new(Vec::new()),
            unloads: RefCell::new(Vec::new()),
        };
        let sink = RecordingSink::new();
        let mut reply_buf = [0u8; 4096];

        run(
            &mut tree,
            &mut store,
            &mut NoNetstack,
            &mut NoConfig,
            &mut NoIfConfig,
            &sink,
            &mut reply_buf,
            Some(0),
        )
        .expect("the initial cycle runs");

        // The regression for the QEMU virtio keyboard+mouse pair: bundle 4
        // is loaded once per matched node — the kernel grants each spawned
        // instance exactly its own node's resources, so a shared load would
        // leave the second device granted to no one and silently dead.
        assert_eq!(store.loads.borrow().as_slice(), &[(4, 2), (4, 3)]);
        assert_eq!(
            sink.ids()
                .iter()
                .filter(|&&id| id == events::NODE_BOUND.0)
                .count(),
            2
        );
    }

    #[test]
    fn a_reaction_reloads_only_a_newly_appeared_node() {
        let kbd = HwMatchKey::virtio(0x1234);
        let net = HwMatchKey::virtio(0x0001);
        let first = encode(
            1,
            &[
                HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root),
                input_node(2, kbd),
            ],
        );
        let second = encode(
            2,
            &[
                HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root),
                input_node(2, kbd),
                {
                    let mut n = HwNode::new(3, 1, HwDeviceClass::Network);
                    n.push_match_key(net).expect("key fits");
                    n
                },
            ],
        );
        let mut tree = ScriptedTree::new(vec![first, second]);
        let mut store = ScriptedStore {
            catalogue: vec![(7, vec![bind(5, kbd)]), (8, vec![bind(5, net)])],
            loads: RefCell::new(Vec::new()),
            unloads: RefCell::new(Vec::new()),
        };
        let sink = RecordingSink::new();
        let mut reply_buf = [0u8; 4096];

        run(
            &mut tree,
            &mut store,
            &mut NoNetstack,
            &mut NoConfig,
            &mut NoIfConfig,
            &sink,
            &mut reply_buf,
            Some(1),
        )
        .expect("one reaction");

        // The keyboard (bundle 7) is loaded only once across both cycles;
        // the appeared network node loads bundle 8 on the reaction.
        assert_eq!(store.loads.borrow().as_slice(), &[(7, 2), (8, 3)]);
        assert_eq!(tree.waited_on, vec![1]);
    }

    #[test]
    fn a_reaction_unloads_a_driver_whose_bound_node_vanished() {
        // Hot-removal: a node bound on the first cycle disappears on the
        // reaction (the device was unplugged), so the device manager asks the
        // kernel to unload exactly its driver and nothing else.
        let kbd = HwMatchKey::virtio(0x1234);
        let first = encode(
            1,
            &[
                HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root),
                input_node(2, kbd),
            ],
        );
        // The keyboard node is gone at the next generation.
        let second = encode(2, &[HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root)]);
        let mut tree = ScriptedTree::new(vec![first, second]);
        let mut store = ScriptedStore {
            catalogue: vec![(7, vec![bind(5, kbd)])],
            loads: RefCell::new(Vec::new()),
            unloads: RefCell::new(Vec::new()),
        };
        let sink = RecordingSink::new();
        let mut reply_buf = [0u8; 4096];

        run(
            &mut tree,
            &mut store,
            &mut NoNetstack,
            &mut NoConfig,
            &mut NoIfConfig,
            &sink,
            &mut reply_buf,
            Some(1),
        )
        .expect("one reaction");

        // Bundle 7 loaded with the first sequential handle (`0x1001`) on the
        // first cycle, and that exact handle is unloaded when its node
        // vanished.
        assert_eq!(store.loads.borrow().as_slice(), &[(7, 2)]);
        assert_eq!(store.unloads.borrow().as_slice(), &[0x1001]);
        assert!(sink.ids().contains(&events::NODE_UNLOADED.0));
    }

    #[test]
    fn a_reaction_with_no_vanished_node_unloads_nothing() {
        // A generation bump that drops no bound node (here a settled tree
        // re-observed) must unload nothing — only a *vanished* bound node
        // triggers a teardown.
        let kbd = HwMatchKey::virtio(0x1234);
        let snapshot_nodes = [
            HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root),
            input_node(2, kbd),
        ];
        let first = encode(1, &snapshot_nodes);
        let second = encode(2, &snapshot_nodes);
        let mut tree = ScriptedTree::new(vec![first, second]);
        let mut store = ScriptedStore {
            catalogue: vec![(7, vec![bind(5, kbd)])],
            loads: RefCell::new(Vec::new()),
            unloads: RefCell::new(Vec::new()),
        };
        let sink = RecordingSink::new();
        let mut reply_buf = [0u8; 4096];

        run(
            &mut tree,
            &mut store,
            &mut NoNetstack,
            &mut NoConfig,
            &mut NoIfConfig,
            &sink,
            &mut reply_buf,
            Some(1),
        )
        .expect("one reaction");

        // The keyboard stays bound across both cycles; nothing is unloaded.
        assert_eq!(store.loads.borrow().as_slice(), &[(7, 2)]);
        assert!(store.unloads.borrow().is_empty());
        assert!(!sink.ids().contains(&events::NODE_UNLOADED.0));
    }

    #[test]
    fn a_vanished_then_reattached_node_unloads_then_reloads() {
        // Re-plug works with no reboot: a bound node vanishes (unload), then
        // re-appears at a later generation (re-load) — the symmetric
        // connect/disconnect path the same generation-bump loop drives.
        let kbd = HwMatchKey::virtio(0x1234);
        let present = encode(
            1,
            &[
                HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root),
                input_node(2, kbd),
            ],
        );
        let gone = encode(2, &[HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root)]);
        let again = encode(
            3,
            &[
                HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root),
                input_node(2, kbd),
            ],
        );
        let mut tree = ScriptedTree::new(vec![present, gone, again]);
        let mut store = ScriptedStore {
            catalogue: vec![(7, vec![bind(5, kbd)])],
            loads: RefCell::new(Vec::new()),
            unloads: RefCell::new(Vec::new()),
        };
        let sink = RecordingSink::new();
        let mut reply_buf = [0u8; 4096];

        run(
            &mut tree,
            &mut store,
            &mut NoNetstack,
            &mut NoConfig,
            &mut NoIfConfig,
            &sink,
            &mut reply_buf,
            Some(2),
        )
        .expect("two reactions");

        // Loaded on cycle 1, unloaded on cycle 2 (node gone), loaded again on
        // cycle 3 (node re-appeared) — the binding was dropped on the unload
        // so the re-attach loads a fresh instance rather than reporting a
        // stale cached handle.
        assert_eq!(store.loads.borrow().as_slice(), &[(7, 2), (7, 2)]);
        assert_eq!(store.unloads.borrow().as_slice(), &[0x1001]);
    }

    #[test]
    fn a_reaction_does_not_relog_an_unchanged_unbound_node() {
        // `NODE_UNBOUND` is a `Debug` record (filtered out on a default `Info`
        // boot); lower the threshold so the test observes it.
        tairix_log::set_max_level(tairix_log::Level::Trace);
        let unmatched = HwMatchKey::virtio(0xFFFF);
        let first = encode(
            1,
            &[
                HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root),
                input_node(2, unmatched),
            ],
        );
        // The same node set at a later generation: a genuine re-evaluation
        // (the tree generation advanced) that changes nothing about node 2.
        let second = encode(
            2,
            &[
                HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root),
                input_node(2, unmatched),
            ],
        );
        let mut tree = ScriptedTree::new(vec![first, second]);
        let mut store = ScriptedStore {
            catalogue: vec![(7, vec![bind(5, HwMatchKey::virtio(0x1234))])],
            loads: RefCell::new(Vec::new()),
            unloads: RefCell::new(Vec::new()),
        };
        let sink = RecordingSink::new();
        let mut reply_buf = [0u8; 4096];

        run(
            &mut tree,
            &mut store,
            &mut NoNetstack,
            &mut NoConfig,
            &mut NoIfConfig,
            &sink,
            &mut reply_buf,
            Some(1),
        )
        .expect("one reaction");

        // Node 2 is unbound in both evaluations, but the unbound decision is
        // logged exactly once — a re-evaluation of a settled tree must not
        // re-flood the diagnostic log.
        assert_eq!(
            sink.ids()
                .iter()
                .filter(|&&id| id == events::NODE_UNBOUND.0)
                .count(),
            1,
            "an unchanged unbound node must not be re-logged on re-evaluation"
        );
    }

    #[test]
    fn a_failed_catalogue_fetch_is_fail_soft_and_still_observes() {
        // `NODE_UNBOUND` is a `Debug` record (filtered out on a default `Info`
        // boot); lower the threshold so the test observes it.
        tairix_log::set_max_level(tairix_log::Level::Trace);
        let snapshot = encode(
            1,
            &[
                HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root),
                input_node(2, HwMatchKey::virtio(0x1234)),
            ],
        );
        let mut tree = ScriptedTree::new(vec![snapshot]);
        let mut store = FailingCatalogue;
        let sink = RecordingSink::new();
        let mut reply_buf = [0u8; 4096];

        run(
            &mut tree,
            &mut store,
            &mut NoNetstack,
            &mut NoConfig,
            &mut NoIfConfig,
            &sink,
            &mut reply_buf,
            Some(0),
        )
        .expect("a failed catalogue fetch does not abort the loop");

        // The store-unavailable event is logged and the node is observed and
        // (with an empty catalogue) left unbound — never an error.
        assert!(sink.ids().contains(&events::DRIVER_STORE_UNAVAILABLE.0));
        assert!(sink.ids().contains(&events::NODE_UNBOUND.0));
        assert_eq!(tree.reported_nodes, vec![1, 2]);
    }

    #[test]
    fn run_fails_closed_when_the_initial_read_fails() {
        let mut tree = ScriptedTree::new(Vec::new());
        let mut store = ScriptedStore {
            catalogue: Vec::new(),
            loads: RefCell::new(Vec::new()),
            unloads: RefCell::new(Vec::new()),
        };
        let sink = RecordingSink::new();
        let mut reply_buf = [0u8; 4096];
        assert_eq!(
            run(
                &mut tree,
                &mut store,
                &mut NoNetstack,
                &mut NoConfig,
                &mut NoIfConfig,
                &sink,
                &mut reply_buf,
                None
            ),
            Err(Errno::NotFound)
        );
        assert!(tree.waited_on.is_empty());
    }

    #[test]
    fn run_fails_closed_when_the_wait_fails() {
        let snapshot = encode(1, &[HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root)]);
        let mut tree = ScriptedTree::new(vec![snapshot]);
        tree.wait_error = Some(Errno::NotImplemented);
        let mut store = ScriptedStore {
            catalogue: Vec::new(),
            loads: RefCell::new(Vec::new()),
            unloads: RefCell::new(Vec::new()),
        };
        let sink = RecordingSink::new();
        let mut reply_buf = [0u8; 4096];
        assert_eq!(
            run(
                &mut tree,
                &mut store,
                &mut NoNetstack,
                &mut NoConfig,
                &mut NoIfConfig,
                &sink,
                &mut reply_buf,
                None
            ),
            Err(Errno::NotImplemented)
        );
    }

    /// A hardware-tree seam serving one fixed snapshot: it fails closed with
    /// [`Errno::BufferTooSmall`] (without consuming anything) until the
    /// caller's buffer is large enough, then copies the whole snapshot out —
    /// the double for exercising [`read_tree_growing`]'s grow-and-retry. `reads` counts every `read_tree` call so a test
    /// can assert a grow actually happened.
    struct FixedSnapshotTree {
        snapshot: Vec<u8>,
        reads: usize,
    }

    impl HwTreeService for FixedSnapshotTree {
        fn read_tree(&mut self, buf: &mut [u8]) -> Result<usize, Errno> {
            self.reads += 1;
            if buf.len() < self.snapshot.len() {
                return Err(Errno::BufferTooSmall);
            }
            buf[..self.snapshot.len()].copy_from_slice(&self.snapshot);
            Ok(self.snapshot.len())
        }

        fn wait_for_change(&mut self, _last_generation: u64) -> Result<(), Errno> {
            Ok(())
        }

        fn on_header(&mut self, _header: &HwTreeHeader) {}

        fn on_node(&mut self, _node: &HwNode) {}
    }

    /// A hardware-tree seam whose `read_tree` always fails with a non-
    /// `BufferTooSmall` error — to prove [`read_tree_growing`] propagates it
    /// fail-closed rather than looping.
    struct ErroringTree(Errno);

    impl HwTreeService for ErroringTree {
        fn read_tree(&mut self, _buf: &mut [u8]) -> Result<usize, Errno> {
            Err(self.0)
        }

        fn wait_for_change(&mut self, _last_generation: u64) -> Result<(), Errno> {
            Ok(())
        }

        fn on_header(&mut self, _header: &HwTreeHeader) {}

        fn on_node(&mut self, _node: &HwNode) {}
    }

    #[test]
    fn read_tree_growing_grows_a_too_small_buffer_until_the_snapshot_fits() {
        // A snapshot far larger than the buffer we start with, so the read
        // must grow several times before it fits (grow
        // before you fail; the tree is a discovered capacity, not a ceiling).
        let mut nodes = vec![HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root)];
        for id in 2..50u32 {
            nodes.push(input_node(id, HwMatchKey::virtio(id)));
        }
        let snapshot = encode(1, &nodes);
        let mut tree = FixedSnapshotTree {
            snapshot: snapshot.clone(),
            reads: 0,
        };
        let mut buf = vec![0u8; 64];
        let len = read_tree_growing(&mut tree, &mut buf).expect("the read grows to fit");
        assert_eq!(len, snapshot.len());
        assert!(buf.len() >= snapshot.len());
        assert!(tree.reads > 1, "the read had to grow at least once");
    }

    #[test]
    fn read_tree_growing_propagates_a_non_buffer_error_fail_closed() {
        let mut tree = ErroringTree(Errno::NotImplemented);
        let mut buf = Vec::new();
        assert_eq!(
            read_tree_growing(&mut tree, &mut buf),
            Err(Errno::NotImplemented)
        );
    }

    #[test]
    fn run_reads_a_tree_larger_than_the_initial_buffer_and_loads_the_match() {
        // The metal scaling case: a real board's full firmware tree is far
        // larger than QEMU `virt`'s handful of nodes, so the service's own
        // snapshot buffer (which starts empty, sizes to
        // `INITIAL_TREE_SNAPSHOT_BYTES`, then grows) must grow before the
        // discovered tree fits — and still load the matched node, rather than
        // failing closed and being relaunched.
        let target = HwMatchKey::virtio(0x9999);
        let mut nodes = vec![HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root)];
        for id in 2..220u32 {
            nodes.push(input_node(id, HwMatchKey::virtio(0x1_0000 + id)));
        }
        nodes.push(input_node(900, target));
        let snapshot = encode(1, &nodes);
        assert!(
            snapshot.len() > INITIAL_TREE_SNAPSHOT_BYTES,
            "the test tree must exceed the initial buffer to exercise the grow"
        );
        let mut tree = FixedSnapshotTree { snapshot, reads: 0 };
        let mut store = ScriptedStore {
            catalogue: vec![(7, vec![bind(5, target)])],
            loads: RefCell::new(Vec::new()),
            unloads: RefCell::new(Vec::new()),
        };
        let sink = RecordingSink::new();
        // Heap-allocated (not a 64 KiB stack array) so the test matches the
        // production reply-buffer size without a large-stack-array lint.
        let mut reply_buf = vec![0u8; 64 * 1024];

        run(
            &mut tree,
            &mut store,
            &mut NoNetstack,
            &mut NoConfig,
            &mut NoIfConfig,
            &sink,
            &mut reply_buf,
            Some(0),
        )
        .expect("the cycle runs after the buffer grows to fit the tree");

        // The one matching node (900) loaded bundle 7; the grow happened.
        assert_eq!(store.loads.borrow().as_slice(), &[(7, 900)]);
        assert!(
            tree.reads > 1,
            "the snapshot should not have fit the initial buffer"
        );
    }
}
