//! Reactive match-and-load over the read-only `/System` driver store
//! (Design D D2b-2c — `.junie/next-pi-prompt.md`).
//!
//! The device manager owns *policy*: it resolves each
//! discovered hardware-tree node against the kernel-decoded driver
//! catalogue with the shared [`rustos_devmatch`] policy,
//! and — for each winning node — asks the kernel to load the matched bundle
//! for that node ([`crate::load_driver`]). The kernel keeps the *mechanism*
//! (signature verification, bundle bytes, grant minting, spawn) in its
//! trusted base; this module supplies no bytes and no grants.
//!
//! A driver matched by several nodes is loaded **once** (keyed by its opaque
//! `bundle_id`) and serves them all; an unmatched node is left unbound and
//! logged — never an error; a load refusal fails only
//! that node, closed, and the walk continues. Every
//! outcome is audited through [`rustos_log`] with the stable
//! [`crate::events`] identifiers, so this is the IPC-loader sibling of the
//! kernel-side `DeviceManager::autoload` walk over the same `resolve`
//! definition.

use alloc::collections::BTreeMap;

use rustos_abi::HwNode;
use rustos_devmatch::{resolve, DriverCandidate, MatchResolution};
use rustos_log::{log as log_event, Event, EventId, Field, Level, Sink};
use rustos_util::fmt::{format_hex_u64, format_i32};

use crate::events;
use crate::store::{load_driver, unload_driver, CatalogueDriver, DriverStoreCall};

/// The set of bundles loaded so far, keyed by opaque `bundle_id` → the
/// loaded driver's handle, so a bundle matched by several nodes is loaded
/// once and the cached handle reported for the rest.
pub type LoadedBundles = BTreeMap<u32, u64>;

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
/// arrives).: an unbound node is logged, not re-logged.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeReport {
    /// The node's winning driver is loaded ([`events::NODE_BOUND`]).
    Bound,
    /// The node matched no driver bind table ([`events::NODE_UNBOUND`]).
    Unbound,
    /// Two drivers tied at the highest priority ([`events::NODE_TIE_REJECTED`]).
    TieRejected,
    /// The winning driver was refused by the load gate
    /// ([`events::NODE_LOAD_FAILED`]).
    LoadFailed,
}

/// The decision last reported per node id, the dedup memory the reactive
/// loop carries across re-evaluations (see [`NodeReport`]).
pub type ReportedNodes = BTreeMap<u32, NodeReport>;

/// One bound node's driver: the opaque `bundle_id` it matched and the
/// `handle` the kernel returned for the loaded driver.
///
/// A bundle matched by several nodes loads once and serves them all, so
/// several bindings can share one `handle`; the unload-on-removal diff tears
/// the driver down only when its **last** bound node has vanished
/// ([`unload_vanished`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NodeDriver {
    /// The opaque `bundle_id` the node matched (the key into
    /// [`LoadedBundles`]).
    pub bundle_id: u32,
    /// The loaded driver's handle the kernel returned.
    pub handle: u64,
}

/// The driver bound to each node id, the hot-removal memory the reactive loop
/// carries across re-evaluations: when a generation bump drops a node from
/// the tree, its binding here names the driver to unload ([`unload_vanished`]).
pub type NodeBindings = BTreeMap<u32, NodeDriver>;

/// The state the reactive match-and-load loop carries across re-evaluations:
/// the loaded-bundle cache ([`LoadedBundles`]), the per-node decision
/// memory ([`ReportedNodes`]), and the per-node driver bindings
/// ([`NodeBindings`]). Bundling them keeps [`match_and_load`] /
/// [`crate::service::run`] to a single state argument (no
/// argument sprawl) while giving each its own clear role.
#[derive(Default)]
pub struct AutoloadState {
    /// Bundles loaded so far, so a bundle matched by several nodes loads once.
    pub loaded: LoadedBundles,
    /// Each node's last reported decision, so an unchanged one is not
    /// re-logged on re-evaluation (see [`NodeReport`]).
    pub reported: ReportedNodes,
    /// The driver bound to each node id, so the hot-removal diff can find the
    /// driver to unload when a bound node vanishes (see [`NodeBindings`]).
    pub bindings: NodeBindings,
}

/// Match every node of `nodes` against `catalogue` and load each winner's
/// bundle through `store`, recording loaded bundles in `state.loaded` and
/// auditing every outcome through `sink`.
///
/// `candidates` is the borrowed [`DriverCandidate`] view of `catalogue`
/// (built once by the caller); `reply_buf` is the caller-owned buffer each
/// [`load_driver`] reply is received into. Idempotent across calls: a node
/// whose winning bundle is already in `state.loaded` is reported bound
/// without a second load (hotplug re-match).
///
/// `state.reported` is the per-node dedup memory ([`ReportedNodes`]): each
/// node's decision is logged only the first time it is reached and again only
/// when it *changes*, so re-evaluating a settled tree (the common case after
/// each generation advance) emits no audit line at all — never re-flooding
/// the diagnostic log with identical records. A node already recorded [`NodeReport::LoadFailed`] is **not**
/// re-attempted against the static driver store (the gate would refuse it
/// identically), so a refusal costs the kernel load gate nothing on later
/// reactions.
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
                // `Root passphrase:` prompt by tens of seconds). It is logged
                // at `Debug` instead — still logged with its stable id when
                // diagnostics are enabled, but dropped in
                // O(1) by the default `Info` filter before any `log_emit`
                // syscall. A *binding* (`NODE_BOUND`), a packaging tie, or a
                // load refusal stays visible — those are the actionable
                // outcomes. Mirrors `events::NODE_OBSERVED`.
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
                            value: rustos_log::FieldValue::Str(priority_str),
                        }],
                    );
                }
            }
            MatchResolution::Winner { candidate, .. } => {
                let bundle_id = catalogue[candidate].bundle_id;
                let handle = if let Some(handle) = state.loaded.get(&bundle_id) {
                    *handle
                } else {
                    // A node already recorded load-failed fails the static
                    // load gate identically, so do not re-run the gate (nor
                    // re-log) on every reaction. A genuine
                    // change re-emits the node, which resets its record below.
                    if state.reported.get(&id) == Some(&NodeReport::LoadFailed) {
                        continue;
                    }
                    match load_driver(store, bundle_id, id, reply_buf) {
                        Ok(handle) => {
                            state.loaded.insert(bundle_id, handle);
                            handle
                        }
                        Err(errno) => {
                            if changed(&mut state.reported, id, NodeReport::LoadFailed) {
                                let mut ebuf = [0u8; 12];
                                let errno_str = format_i32(errno.as_i32(), &mut ebuf);
                                audit_node(
                                    sink,
                                    events::NODE_LOAD_FAILED,
                                    Level::Warn,
                                    id,
                                    &[Field {
                                        key: "errno",
                                        value: rustos_log::FieldValue::Str(errno_str),
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
                            value: rustos_log::FieldValue::Str(handle_str),
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
/// `present` is the set of node ids in the snapshot just observed. Every
/// previously-bound node ([`AutoloadState::bindings`]) absent from `present`
/// is gone: its binding is dropped, and — when no *other* still-present bound
/// node shares the same driver `handle` (a bundle matched by several nodes is
/// loaded once) — the kernel is asked to tear that driver down through
/// [`unload_driver`]. The driver's `bundle_id` is then purged from
/// `state.loaded` and its `reported` decision cleared, so if the device is
/// re-attached the driver is loaded afresh (re-plug works with no reboot).
///
/// Idempotent and fail-soft: an unload that the kernel reports already gone
/// ([`Errno::NotFound`](rustos_abi::Errno::NotFound)) still drops the local
/// binding; a transport failure is logged and the binding dropped so the
/// stale driver is never re-derived. Every unload is audited
/// ([`events::NODE_UNLOADED`]).
pub fn unload_vanished<C: DriverStoreCall + ?Sized>(
    present: &dyn Fn(u32) -> bool,
    store: &mut C,
    reply_buf: &mut [u8],
    state: &mut AutoloadState,
    sink: &dyn Sink,
) {
    // Collect the vanished bound node ids first: the borrow of `bindings`
    // ends before the unload pass mutates `state`.
    let vanished: alloc::vec::Vec<(u32, NodeDriver)> = state
        .bindings
        .iter()
        .filter(|(id, _)| !present(**id))
        .map(|(id, driver)| (*id, *driver))
        .collect();

    for (node_id, driver) in vanished {
        // Drop this node's binding and its dedup memory so a re-attach
        // re-binds and re-logs from scratch.
        state.bindings.remove(&node_id);
        state.reported.remove(&node_id);

        // A shared driver (one bundle, several nodes) is torn down only when
        // its *last* bound node is gone: if another live binding still names
        // the same handle, leave the driver running.
        if state
            .bindings
            .values()
            .any(|other| other.handle == driver.handle)
        {
            continue;
        }

        // This was the last node the driver served: ask the kernel to tear it
        // down and purge the loaded-bundle cache so a re-attach reloads it.
        let outcome = unload_driver(store, driver.handle, reply_buf);
        state.loaded.remove(&driver.bundle_id);
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
                    value: rustos_log::FieldValue::Str(handle_str),
                }],
            ),
            Err(errno) => {
                // Fail-soft: the kernel reported the driver already gone, or
                // the transport failed. Either way the local binding is
                // dropped so the stale driver is never re-derived; the
                // outcome is logged with its errno for audit.
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
                            value: rustos_log::FieldValue::Str(handle_str),
                        },
                        Field {
                            key: "errno",
                            value: rustos_log::FieldValue::Str(errno_str),
                        },
                    ],
                );
            }
        }
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
        value: rustos_log::FieldValue::Str(node_str),
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
        x if x == events::NODE_UNLOADED => "driver unloaded: bound node vanished",
        _ => "devmgr event",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloc::vec::Vec;
    use core::cell::RefCell;

    use rustos_abi::driver_store::encode_unload_reply;
    use rustos_abi::Errno;

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
            match rustos_abi::driver_store::StoreRequest::decode(request)? {
                rustos_abi::driver_store::StoreRequest::Unload { handle } => {
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
        state.loaded.insert(bundle_id, handle);
        state.reported.insert(node, NodeReport::Bound);
    }

    #[test]
    fn a_vanished_bound_node_unloads_its_driver_and_purges_the_cache() {
        let mut state = AutoloadState::default();
        bound(&mut state, 2, 7, 0x1007);
        let mut store = UnloadRecorder::new();
        let mut reply = [0u8; 64];

        // Node 2 is no longer present: tear its driver down.
        unload_vanished(&|_id| false, &mut store, &mut reply, &mut state, &NullSink);

        assert_eq!(store.unloads.borrow().as_slice(), &[0x1007]);
        assert!(state.bindings.is_empty(), "the binding is dropped");
        assert!(
            !state.loaded.contains_key(&7),
            "the loaded-bundle cache is purged so a re-attach reloads"
        );
        assert!(!state.reported.contains_key(&2));
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
        assert!(state.loaded.contains_key(&7));
    }

    #[test]
    fn a_shared_driver_is_unloaded_only_when_its_last_node_vanishes() {
        // One bundle (and one handle) serves two nodes. Losing one node must
        // not tear the still-serving driver down; only when the *last* bound
        // node vanishes is the driver unloaded.
        let mut state = AutoloadState::default();
        bound(&mut state, 2, 7, 0x1007);
        bound(&mut state, 3, 7, 0x1007);
        let mut store = UnloadRecorder::new();
        let mut reply = [0u8; 64];

        // Node 2 vanishes, node 3 stays: the shared driver keeps running.
        unload_vanished(&|id| id == 3, &mut store, &mut reply, &mut state, &NullSink);
        assert!(
            store.unloads.borrow().is_empty(),
            "a driver still serving a live node is not torn down"
        );
        assert!(state.loaded.contains_key(&7));
        assert_eq!(state.bindings.len(), 1);

        // Now node 3 vanishes too — its last node is gone, so unload once.
        unload_vanished(&|_id| false, &mut store, &mut reply, &mut state, &NullSink);
        assert_eq!(store.unloads.borrow().as_slice(), &[0x1007]);
        assert!(state.bindings.is_empty());
        assert!(!state.loaded.contains_key(&7));
    }

    #[test]
    fn an_already_gone_handle_still_drops_the_binding_fail_soft() {
        // The kernel reports the driver already gone; the diff still drops the
        // local binding so the stale driver is never re-derived (fail-soft).
        struct AlreadyGone;
        impl DriverStoreCall for AlreadyGone {
            fn call(&mut self, _request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
                rustos_abi::driver_store::encode_error_reply(reply, Errno::NotFound)
            }
        }

        let mut state = AutoloadState::default();
        bound(&mut state, 2, 7, 0x1007);
        let mut store = AlreadyGone;
        let mut reply = [0u8; 64];

        unload_vanished(&|_id| false, &mut store, &mut reply, &mut state, &NullSink);

        assert!(state.bindings.is_empty());
        assert!(!state.loaded.contains_key(&7));
    }
}
