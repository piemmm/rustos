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
use crate::store::{load_driver, CatalogueDriver, DriverStoreCall};

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

/// The state the reactive match-and-load loop carries across re-evaluations:
/// the loaded-bundle cache ([`LoadedBundles`]) and the per-node decision
/// memory ([`ReportedNodes`]). Bundling them keeps [`match_and_load`] /
/// [`crate::service::run`] to a single state argument (no
/// argument sprawl) while giving each its own clear role.
#[derive(Default)]
pub struct AutoloadState {
    /// Bundles loaded so far, so a bundle matched by several nodes loads once.
    pub loaded: LoadedBundles,
    /// Each node's last reported decision, so an unchanged one is not
    /// re-logged on re-evaluation (see [`NodeReport`]).
    pub reported: ReportedNodes,
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
                            value: priority_str,
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
                                        value: errno_str,
                                    }],
                                );
                            }
                            continue;
                        }
                    }
                };
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
                            value: handle_str,
                        }],
                    );
                }
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

/// Log one node decision under `id`, stamping the node id plus up to one
/// event-specific field.
fn audit_node(sink: &dyn Sink, id: EventId, level: Level, node: u32, extra: &[Field<'_>]) {
    let mut nbuf = [0u8; 16];
    let node_str = format_hex_u64(u64::from(node), &mut nbuf);
    let mut fields = [Field {
        key: "node",
        value: node_str,
    }; 2];
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
        _ => "devmgr event",
    }
}
