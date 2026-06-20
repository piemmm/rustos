//! Reactive match-and-load over the read-only `/System` driver store
//! (Design D D2b-2c — `.junie/next-pi-prompt.md`).
//!
//! The device manager owns *policy* (`AGENTS.md` §4): it resolves each
//! discovered hardware-tree node against the kernel-decoded driver
//! catalogue with the shared [`rustos_devmatch`] policy (`AGENTS.md` §18.3),
//! and — for each winning node — asks the kernel to load the matched bundle
//! for that node ([`crate::load_driver`]). The kernel keeps the *mechanism*
//! (signature verification, bundle bytes, grant minting, spawn) in its
//! trusted base; this module supplies no bytes and no grants.
//!
//! A driver matched by several nodes is loaded **once** (keyed by its opaque
//! `bundle_id`) and serves them all; an unmatched node is left unbound and
//! logged — never an error (`AGENTS.md` §18.4); a load refusal fails only
//! that node, closed, and the walk continues (`AGENTS.md` §5.4). Every
//! outcome is audited through [`rustos_log`] with the stable
//! [`crate::events`] identifiers, so this is the IPC-loader sibling of the
//! kernel-side `DeviceManager::autoload` walk over the same `resolve`
//! definition (`AGENTS.md` §2.2).

use alloc::collections::BTreeMap;

use rustos_abi::HwNode;
use rustos_devmatch::{resolve, DriverCandidate, MatchResolution};
use rustos_log::{log as log_event, Event, EventId, Field, Level, Sink};
use rustos_util::fmt::{format_hex_u64, format_i32};

use crate::events;
use crate::store::{load_driver, CatalogueDriver, DriverStoreCall};

/// The set of bundles loaded so far, keyed by opaque `bundle_id` → the
/// loaded driver's handle, so a bundle matched by several nodes is loaded
/// once and the cached handle reported for the rest (`AGENTS.md` §18.3).
pub type LoadedBundles = BTreeMap<u32, u64>;

/// Match every node of `nodes` against `catalogue` and load each winner's
/// bundle through `store`, recording loaded bundles in `loaded` and
/// auditing every outcome through `sink`.
///
/// `candidates` is the borrowed [`DriverCandidate`] view of `catalogue`
/// (built once by the caller); `reply_buf` is the caller-owned buffer each
/// [`load_driver`] reply is received into. Idempotent across calls: a node
/// whose winning bundle is already in `loaded` is reported bound without a
/// second load (hotplug re-match, `AGENTS.md` §18.4).
pub fn match_and_load<C: DriverStoreCall + ?Sized>(
    nodes: &[HwNode],
    catalogue: &[CatalogueDriver],
    candidates: &[DriverCandidate<'_>],
    store: &mut C,
    reply_buf: &mut [u8],
    loaded: &mut LoadedBundles,
    sink: &dyn Sink,
) {
    for node in nodes {
        if node.is_root() {
            continue;
        }
        match resolve(node.match_keys(), candidates) {
            MatchResolution::Unmatched => {
                audit_node(sink, events::NODE_UNBOUND, Level::Info, node.id(), &[]);
            }
            MatchResolution::Tie { priority } => {
                let mut pbuf = [0u8; 12];
                let priority_str = format_i32(i32::from(priority), &mut pbuf);
                audit_node(
                    sink,
                    events::NODE_TIE_REJECTED,
                    Level::Warn,
                    node.id(),
                    &[Field {
                        key: "priority",
                        value: priority_str,
                    }],
                );
            }
            MatchResolution::Winner { candidate, .. } => {
                let bundle_id = catalogue[candidate].bundle_id;
                let handle = match loaded.get(&bundle_id) {
                    Some(handle) => *handle,
                    None => match load_driver(store, bundle_id, node.id(), reply_buf) {
                        Ok(handle) => {
                            loaded.insert(bundle_id, handle);
                            handle
                        }
                        Err(errno) => {
                            let mut ebuf = [0u8; 12];
                            let errno_str = format_i32(errno.as_i32(), &mut ebuf);
                            audit_node(
                                sink,
                                events::NODE_LOAD_FAILED,
                                Level::Warn,
                                node.id(),
                                &[Field {
                                    key: "errno",
                                    value: errno_str,
                                }],
                            );
                            continue;
                        }
                    },
                };
                let mut hbuf = [0u8; 16];
                let handle_str = format_hex_u64(handle, &mut hbuf);
                audit_node(
                    sink,
                    events::NODE_BOUND,
                    Level::Info,
                    node.id(),
                    &[Field {
                        key: "handle",
                        value: handle_str,
                    }],
                );
            }
        }
    }
}

/// Log one node decision under `id`, stamping the node id plus up to one
/// event-specific field (`AGENTS.md` §18.3 / §19.4).
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
