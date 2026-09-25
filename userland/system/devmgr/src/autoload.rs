//! Reactive match-and-load over the read-only `/System` driver store
//! (Design D D2b-2c — `plans/PI.md`).
//!
//! The device manager owns *policy*: it resolves each
//! discovered hardware-tree node against the kernel-decoded driver
//! catalogue with the shared [`tairix_devmatch`] policy,
//! and — for each winning node — asks the kernel to load the matched bundle
//! for that node ([`crate::load_driver`]). The kernel keeps the *mechanism*
//! (signature verification, bundle bytes, grant minting, spawn) in its
//! trusted base; this module supplies no bytes and no grants.
//!
//! Every matched node gets its **own** loaded driver instance: the kernel
//! mints a fresh process per load with exactly that node's resource grants,
//! so one loaded process can never see a sibling node's registers — two
//! identical devices (e.g. a virtio keyboard and a virtio mouse, both
//! device id 18) each need their own instance or the second is bound in
//! name only and never driven. An unmatched node is left unbound and
//! logged — never an error; a load refusal fails only
//! that node, closed, and the walk continues. Every
//! outcome is audited through [`tairix_log`] with the stable
//! [`crate::events`] identifiers, so this is the IPC-loader sibling of the
//! kernel-side `DeviceManager::autoload` walk over the same `resolve`
//! definition.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use tairix_abi::{Errno, HwNode};
use tairix_devmatch::{resolve, DriverCandidate, MatchResolution};
use tairix_log::{log as log_event, Event, EventId, Field, Level, Sink};
use tairix_util::fmt::{format_hex_u64, format_i32};

use crate::events;
use crate::store::{load_driver, unload_driver, CatalogueDriver, DriverStoreCall};

/// The last match decision reported for a node, so an unchanged decision is
/// **not** re-logged when the reactive loop re-evaluates the tree.
///
/// The device manager re-runs [`match_and_load`] over the whole snapshot on
/// every hardware-tree generation advance. Without this
/// memory each re-evaluation would re-emit the same `NODE_UNBOUND` /
/// `NODE_BOUND` audit line for every node, flooding the (slow, serial)
/// diagnostic log with identical records and starving the boot — a
/// progress-spam / redundant-work defect. A node is logged only the
/// first time it reaches a decision and again only when that decision
/// *changes* (e.g. `Unbound` → `Bound` once the late-bound catalogue
/// arrives).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeReport {
    /// The node's winning driver is loaded ([`events::NODE_BOUND`]).
    Bound,
    /// The node matched no driver bind table ([`events::NODE_UNBOUND`]).
    Unbound,
    /// Two drivers tied at the highest priority ([`events::NODE_TIE_REJECTED`]).
    TieRejected,
    /// The winning driver did not load: the gate refused it
    /// ([`events::NODE_LOAD_FAILED`]), the node left the tree first
    /// ([`events::NODE_LOAD_RACED_REMOVAL`]), or another live driver holds it
    /// ([`events::NODE_ALREADY_DRIVEN`]). Never re-attempted.
    LoadFailed,
}

/// The decision last reported per node id, the dedup memory the reactive
/// loop carries across re-evaluations (see [`NodeReport`]).
pub type ReportedNodes = BTreeMap<u32, NodeReport>;

/// One bound node's driver: the opaque `bundle_id` it matched and the
/// `handle` the kernel returned for the loaded driver instance.
///
/// Every load spawns a fresh driver process holding exactly its node's
/// grants, so each binding names its own instance and `handle`; the
/// unload-on-removal diff tears an instance down when its node vanishes
/// ([`unload_vanished`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NodeDriver {
    /// The opaque `bundle_id` the node matched.
    pub bundle_id: u32,
    /// The loaded driver's handle the kernel returned.
    pub handle: u64,
}

/// The driver bound to each node id, the hot-removal memory the reactive loop
/// carries across re-evaluations: when a generation bump drops a node from
/// the tree, its binding here names the driver to unload ([`unload_vanished`]).
pub type NodeBindings = BTreeMap<u32, NodeDriver>;

/// The state the reactive match-and-load loop carries across re-evaluations:
/// the per-node decision memory ([`ReportedNodes`]), the per-node driver
/// bindings ([`NodeBindings`]), and the node ids the last pass ran over.
#[derive(Default)]
pub struct AutoloadState {
    /// Each node's last reported decision, so an unchanged one is not
    /// re-logged on re-evaluation (see [`NodeReport`]).
    pub reported: ReportedNodes,
    /// The driver bound to each node id, so the hot-removal diff can find the
    /// driver to unload when a bound node vanishes (see [`NodeBindings`]).
    pub bindings: NodeBindings,
    /// The ascending ids of the snapshot the last pass ran over, so a
    /// reaction that added or removed no node can be told apart.
    pub observed: Vec<u32>,
}

/// Match every node of `nodes` against `catalogue` and load each winner's
/// bundle through `store` — **one instance per matched node** — recording
/// the bindings in `state.bindings` and auditing every outcome through
/// `sink`.
///
/// `candidates` is the borrowed [`DriverCandidate`] view of `catalogue`
/// (built once by the caller); `reply_buf` is the caller-owned buffer each
/// [`load_driver`] reply is received into. Idempotent across calls: a node
/// already bound in `state.bindings` keeps its instance and is reported
/// bound without a second load (hotplug re-match). A bundle matched by
/// several nodes loads once **per node**: the kernel spawns each load into
/// its own process holding exactly that node's resource grants, so a shared
/// instance would leave every node after the first granted to no one and
/// silently dead (two identical virtio-input devices — a keyboard and a
/// mouse — are the canonical case).
///
/// `state.reported` is the per-node dedup memory ([`ReportedNodes`]): each
/// node's decision is logged only the first time it is reached and again only
/// when it *changes*, so re-evaluating a settled tree (the common case after
/// each generation advance) emits no audit line at all — never re-flooding
/// the diagnostic log with identical records. A node already recorded
/// [`NodeReport::LoadFailed`] is **not** re-attempted: the static store's gate
/// would refuse it identically, a node that left the tree is gone, and a held
/// node stays held while its driver runs.
pub fn match_and_load<C: DriverStoreCall + ?Sized>(
    nodes: &[HwNode],
    catalogue: &[CatalogueDriver],
    candidates: &[DriverCandidate<'_>],
    store: &mut C,
    reply_buf: &mut [u8],
    state: &mut AutoloadState,
    sink: &dyn Sink,
) {
    for node in nodes {
        if node.is_root() {
            continue;
        }
        let id = node.id();
        match resolve(node.match_keys(), candidates) {
            MatchResolution::Unmatched => {
                // An unmatched node is the routine, high-volume case: on a
                // real device tree most nodes (clocks, pinctrl, thermal, …)
                // have no driver, so emitting one record per unbound node at
                // `Info` floods the slow diagnostic UART and starves boot (a
                // progress-spam / defect — it once delayed the Pi's
                // `Root passphrase:` prompt by tens of seconds). It is `Debug`
                // instead, so the default `Info` filter drops it in O(1)
                // before any `log_emit` syscall. A *binding* (`NODE_BOUND`), a
                // packaging tie, or a load refusal stays visible — those are
                // the actionable outcomes. Mirrors `events::NODE_OBSERVED`.
                if changed(&mut state.reported, id, NodeReport::Unbound) {
                    audit_node(sink, events::NODE_UNBOUND, Level::Debug, id, &[]);
                }
            }
            MatchResolution::Tie { priority } => {
                if changed(&mut state.reported, id, NodeReport::TieRejected) {
                    let mut pbuf = [0u8; 12];
                    let priority_str = format_i32(i32::from(priority), &mut pbuf);
                    audit_node(
                        sink,
                        events::NODE_TIE_REJECTED,
                        Level::Warn,
                        id,
                        &[Field {
                            key: "priority",
                            value: tairix_log::FieldValue::Str(priority_str),
                        }],
                    );
                }
            }
            MatchResolution::Winner { candidate, .. } => {
                let bundle_id = catalogue[candidate].bundle_id;
                let handle = if let Some(existing) = state.bindings.get(&id) {
                    existing.handle
                } else {
                    // A changed device is republished under a new id, so a
                    // failed load is never worth repeating for this one.
                    if state.reported.get(&id) == Some(&NodeReport::LoadFailed) {
                        continue;
                    }
                    match load_driver(store, bundle_id, id, reply_buf) {
                        Ok(handle) => handle,
                        Err(errno) => {
                            if changed(&mut state.reported, id, NodeReport::LoadFailed) {
                                let (event, level) = load_refusal(errno);
                                let mut ebuf = [0u8; 12];
                                let errno_str = format_i32(errno.as_i32(), &mut ebuf);
                                audit_node(
                                    sink,
                                    event,
                                    level,
                                    id,
                                    &[Field {
                                        key: "errno",
                                        value: tairix_log::FieldValue::Str(errno_str),
                                    }],
                                );
                            }
                            continue;
                        }
                    }
                };
                // Record (or refresh) which driver this node is bound to, so
                // the hot-removal diff can tear it down if the node later
                // vanishes (see `unload_vanished`). A node re-matched on a
                // re-evaluation re-records the same binding; the entry is
                // dropped only when the node disappears or its driver unloads.
                state.bindings.insert(id, NodeDriver { bundle_id, handle });
                if changed(&mut state.reported, id, NodeReport::Bound) {
                    let mut hbuf = [0u8; 16];
                    let handle_str = format_hex_u64(handle, &mut hbuf);
                    audit_node(
                        sink,
                        events::NODE_BOUND,
                        Level::Info,
                        id,
                        &[Field {
                            key: "handle",
                            value: tairix_log::FieldValue::Str(handle_str),
                        }],
                    );
                }
            }
        }
    }
}

/// Unload every driver whose bound hardware-tree node has **vanished** from
/// the live tree (hot-removal), the symmetric partner of [`match_and_load`].
///
/// `present` answers whether a node id is in the snapshot just observed.
/// Every previously-bound node ([`AutoloadState::bindings`]) absent from it
/// is gone: its binding is dropped and the kernel is asked to tear its
/// driver instance down through [`unload_driver`] (each node owns its own
/// instance, so no other binding can share the handle). Every decision
/// recorded for an absent node is dropped too, bound or not: a node id is
/// never reissued, so a device re-attached later arrives as a fresh node and
/// the memory would otherwise grow with every re-plug.
///
/// A vanished node is gone for good, whatever its fault-domain owner is
/// doing: no bus driver drops its children across its own reset (the xHCI
/// controller keeps every child whose device comes back, `plans/FIX-IO.md`
/// IO4), and a node that does leave the tree never returns under its id,
/// so its driver has nothing left to serve.
///
/// Idempotent and fail-soft: an unload that the kernel reports already gone
/// ([`Errno::NotFound`]) still drops the local binding; a transport failure
/// is logged and the binding dropped so the stale driver is never
/// re-derived. Every unload is audited ([`events::NODE_UNLOADED`]).
pub fn unload_vanished<C: DriverStoreCall + ?Sized>(
    present: &dyn Fn(u32) -> bool,
    store: &mut C,
    reply_buf: &mut [u8],
    state: &mut AutoloadState,
    sink: &dyn Sink,
) {
    state.reported.retain(|&node_id, _| present(node_id));
    state.bindings.retain(|&node_id, driver| {
        if present(node_id) {
            return true;
        }
        let outcome = unload_driver(store, driver.handle, reply_buf);
        let mut hbuf = [0u8; 16];
        let handle_str = format_hex_u64(driver.handle, &mut hbuf);
        match outcome {
            Ok(()) => audit_node(
                sink,
                events::NODE_UNLOADED,
                Level::Info,
                node_id,
                &[Field {
                    key: "handle",
                    value: tairix_log::FieldValue::Str(handle_str),
                }],
            ),
            Err(errno) => {
                // Already gone, or the transport failed: the binding is
                // dropped either way so the stale driver is never re-derived.
                let mut ebuf = [0u8; 12];
                let errno_str = format_i32(errno.as_i32(), &mut ebuf);
                audit_node(
                    sink,
                    events::NODE_UNLOADED,
                    Level::Warn,
                    node_id,
                    &[
                        Field {
                            key: "handle",
                            value: tairix_log::FieldValue::Str(handle_str),
                        },
                        Field {
                            key: "errno",
                            value: tairix_log::FieldValue::Str(errno_str),
                        },
                    ],
                );
            }
        }
        false
    });
}

/// The record a failed load is reported under: a node that left the tree
/// before its driver was admitted, or that another live driver already
/// holds, is the tree moving under the load rather than the gate refusing
/// the image.
fn load_refusal(errno: Errno) -> (EventId, Level) {
    match errno {
        Errno::DeviceOffline => (events::NODE_LOAD_RACED_REMOVAL, Level::Info),
        Errno::Busy => (events::NODE_ALREADY_DRIVEN, Level::Info),
        _ => (events::NODE_LOAD_FAILED, Level::Warn),
    }
}

/// Record `kind` as node `id`'s latest decision, returning `true` when it
/// differs from the previously reported one (or none was) — the signal that
/// the decision is worth logging (log a change, not every
/// re-evaluation).
fn changed(reported: &mut ReportedNodes, id: u32, kind: NodeReport) -> bool {
    reported.insert(id, kind) != Some(kind)
}

/// Log one node decision under `id`, stamping the node id plus up to two
/// event-specific fields (sized for the largest emitter — the fail-soft
/// unload record carries both a `handle` and an `errno`).
fn audit_node(sink: &dyn Sink, id: EventId, level: Level, node: u32, extra: &[Field<'_>]) {
    let mut nbuf = [0u8; 16];
    let node_str = format_hex_u64(u64::from(node), &mut nbuf);
    let mut fields = [Field {
        key: "node",
        value: tairix_log::FieldValue::Str(node_str),
    }; 3];
    let mut len = 1;
    for field in extra {
        fields[len] = *field;
        len += 1;
    }
    log_event(
        sink,
        &Event {
            level,
            id,
            message: event_message(id),
            fields: &fields[..len],
        },
    );
}

fn event_message(id: EventId) -> &'static str {
    match id {
        x if x == events::NODE_BOUND => "node bound to driver",
        x if x == events::NODE_UNBOUND => "node left unbound: no matching driver",
        x if x == events::NODE_TIE_REJECTED => "node refused: unbroken bind-priority tie",
        x if x == events::NODE_LOAD_FAILED => "node load failed: driver-store gate refused",
        x if x == events::NODE_LOAD_RACED_REMOVAL => "node left the tree before its driver loaded",
        x if x == events::NODE_ALREADY_DRIVEN => "node already held by a live driver",
        x if x == events::NODE_UNLOADED => "driver unloaded: bound node vanished",
        _ => "devmgr event",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsink::RecordingSink;

    use alloc::vec;
    use core::cell::RefCell;

    use tairix_abi::driver_store::{encode_error_reply, encode_unload_reply, StoreRequest};
    use tairix_abi::hwtree::{HwDeviceClass, HwMatchKey};
    use tairix_abi::DriverBindKey;

    /// A driver-store seam that records every `Unload { handle }` and frames
    /// a success reply — so the diff's teardown decisions are observable.
    struct UnloadRecorder {
        unloads: RefCell<Vec<u64>>,
    }

    impl UnloadRecorder {
        fn new() -> Self {
            Self {
                unloads: RefCell::new(Vec::new()),
            }
        }
    }

    impl DriverStoreCall for UnloadRecorder {
        fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
            // The diff only ever issues `Unload`; any other opcode is a test
            // bug surfaced fail-closed rather than silently ignored.
            match StoreRequest::decode(request)? {
                StoreRequest::Unload { handle } => {
                    self.unloads.borrow_mut().push(handle);
                    encode_unload_reply(reply)
                }
                _ => Err(Errno::NotImplemented),
            }
        }
    }

    /// A sink that drops every record — these tests assert on the store's
    /// recorded unloads and the state, not the audit log.
    struct NullSink;
    impl Sink for NullSink {
        fn write_event(&self, _event: &Event<'_>) {}
    }

    fn bound(state: &mut AutoloadState, node: u32, bundle_id: u32, handle: u64) {
        state
            .bindings
            .insert(node, NodeDriver { bundle_id, handle });
        state.reported.insert(node, NodeReport::Bound);
    }

    #[test]
    fn a_vanished_bound_node_unloads_its_driver_instance() {
        let mut state = AutoloadState::default();
        bound(&mut state, 2, 7, 0x1007);
        let mut store = UnloadRecorder::new();
        let mut reply = [0u8; 64];

        // Node 2 is no longer present: tear its driver down.
        unload_vanished(&|_id| false, &mut store, &mut reply, &mut state, &NullSink);

        assert_eq!(store.unloads.borrow().as_slice(), &[0x1007]);
        assert!(state.bindings.is_empty(), "the binding is dropped");
        assert!(
            !state.reported.contains_key(&2),
            "the decision memory goes with the node"
        );
    }

    #[test]
    fn a_vanished_node_leaves_no_decision_behind_bound_or_not() {
        // Node ids are never reissued, so an entry kept for a node that left
        // grows the memory by one with every re-plug, for the life of the
        // service.
        let mut state = AutoloadState::default();
        state.reported.insert(3, NodeReport::Unbound);
        state.reported.insert(4, NodeReport::TieRejected);
        state.reported.insert(5, NodeReport::LoadFailed);
        state.reported.insert(6, NodeReport::Unbound);
        let mut store = UnloadRecorder::new();
        let mut reply = [0u8; 64];

        unload_vanished(&|id| id == 6, &mut store, &mut reply, &mut state, &NullSink);

        assert_eq!(
            state.reported.keys().copied().collect::<Vec<_>>(),
            [6],
            "only the node still present keeps its decision"
        );
        assert!(
            store.unloads.borrow().is_empty(),
            "nothing was bound, so nothing is unloaded"
        );
    }

    #[test]
    fn a_still_present_bound_node_is_not_unloaded() {
        let mut state = AutoloadState::default();
        bound(&mut state, 2, 7, 0x1007);
        let mut store = UnloadRecorder::new();
        let mut reply = [0u8; 64];

        // Node 2 is still present: nothing is torn down.
        unload_vanished(&|id| id == 2, &mut store, &mut reply, &mut state, &NullSink);

        assert!(store.unloads.borrow().is_empty());
        assert_eq!(state.bindings.len(), 1);
    }

    #[test]
    fn a_vanished_node_unloads_only_its_own_instance() {
        // One bundle, two nodes, two instances (each load spawns its own
        // process holding its node's grants). Losing one node tears down
        // only that node's instance; the sibling's keeps running.
        let mut state = AutoloadState::default();
        bound(&mut state, 2, 7, 0x1007);
        bound(&mut state, 3, 7, 0x2007);
        let mut store = UnloadRecorder::new();
        let mut reply = [0u8; 64];

        // Node 2 vanishes, node 3 stays: only node 2's instance is torn down.
        unload_vanished(&|id| id == 3, &mut store, &mut reply, &mut state, &NullSink);
        assert_eq!(
            store.unloads.borrow().as_slice(),
            &[0x1007],
            "only the vanished node's instance is torn down"
        );
        assert_eq!(state.bindings.len(), 1);

        // Now node 3 vanishes too — its own instance is unloaded.
        unload_vanished(&|_id| false, &mut store, &mut reply, &mut state, &NullSink);
        assert_eq!(store.unloads.borrow().as_slice(), &[0x1007, 0x2007]);
        assert!(state.bindings.is_empty());
    }

    #[test]
    fn an_already_gone_handle_still_drops_the_binding_fail_soft() {
        // The kernel reports the driver already gone; the diff still drops the
        // local binding so the stale driver is never re-derived (fail-soft).
        struct AlreadyGone;
        impl DriverStoreCall for AlreadyGone {
            fn call(&mut self, _request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
                tairix_abi::driver_store::encode_error_reply(reply, Errno::NotFound)
            }
        }

        let mut state = AutoloadState::default();
        bound(&mut state, 2, 7, 0x1007);
        let mut store = AlreadyGone;
        let mut reply = [0u8; 64];

        unload_vanished(&|_id| false, &mut store, &mut reply, &mut state, &NullSink);

        assert!(state.bindings.is_empty());
    }

    /// A store that refuses every load in band with the errno scripted for
    /// its node.
    struct RefusingStore(Vec<(u32, Errno)>);

    impl DriverStoreCall for RefusingStore {
        fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
            let StoreRequest::Load { node_id, .. } = StoreRequest::decode(request)? else {
                return Err(Errno::NotImplemented);
            };
            let errno = self
                .0
                .iter()
                .find(|(node, _)| *node == node_id)
                .map_or(Errno::NotFound, |(_, errno)| *errno);
            encode_error_reply(reply, errno)
        }
    }

    #[test]
    fn a_load_the_tree_overtook_is_not_reported_as_a_gate_refusal() {
        let key = HwMatchKey::virtio(0x1234);
        let input = |id| {
            let mut node = HwNode::new(id, 1, HwDeviceClass::Input);
            node.push_match_key(key).expect("key fits");
            node
        };
        let nodes = [input(2), input(3), input(4)];
        let catalogue = [CatalogueDriver {
            bundle_id: 7,
            bind_keys: vec![DriverBindKey::new(5, key)],
        }];
        let candidates: Vec<_> = catalogue.iter().map(CatalogueDriver::candidate).collect();
        let mut store = RefusingStore(vec![
            (2, Errno::DeviceOffline),
            (3, Errno::Busy),
            (4, Errno::PermissionDenied),
        ]);
        let sink = RecordingSink::new();
        let mut state = AutoloadState::default();
        let mut reply = [0u8; 64];

        match_and_load(
            &nodes,
            &catalogue,
            &candidates,
            &mut store,
            &mut reply,
            &mut state,
            &sink,
        );

        assert_eq!(
            sink.ids(),
            [
                events::NODE_LOAD_RACED_REMOVAL.0,
                events::NODE_ALREADY_DRIVEN.0,
                events::NODE_LOAD_FAILED.0,
            ]
        );
        assert_eq!(
            sink.level_of(events::NODE_LOAD_RACED_REMOVAL.0),
            Some(Level::Info)
        );
        assert_eq!(
            sink.level_of(events::NODE_ALREADY_DRIVEN.0),
            Some(Level::Info)
        );
        assert_eq!(sink.level_of(events::NODE_LOAD_FAILED.0), Some(Level::Warn));
        assert_eq!(
            sink.field_of(events::NODE_ALREADY_DRIVEN.0, "errno")
                .as_deref(),
            Some("41"),
            "the cause travels with the record"
        );
        assert!(
            state
                .reported
                .values()
                .all(|report| *report == NodeReport::LoadFailed),
            "none of the three is re-attempted"
        );
    }
}
