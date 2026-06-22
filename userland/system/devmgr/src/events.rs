//! Stable [`rustos_log::EventId`] constants emitted by the device
//! manager.
//!
//! Per `lib/log` convention (`AGENTS.md` §2.5) every subsystem owns a
//! 1 000-wide reserved range. The device manager occupies
//! `13000..14000`. Once shipped the numeric values must never be
//! re-used or re-numbered — external audit-log consumers rely on them.

use rustos_log::EventId;

/// Range start (inclusive) reserved for `devmgr` event identifiers.
///
/// Exposed so audit consumers can filter by subsystem in O(1) instead of
/// matching on individual event identifiers.
pub const DEVMGR_RANGE_START: u32 = 13_000;
/// Range end (exclusive) reserved for `devmgr` event identifiers.
pub const DEVMGR_RANGE_END: u32 = 14_000;

/// A hardware-tree node was bound: its winning driver is loaded.
pub const NODE_BOUND: EventId = EventId(13_001);
/// A hardware-tree node matched no driver bind table and was left
/// unbound — never an error (`AGENTS.md` §18.4).
pub const NODE_UNBOUND: EventId = EventId(13_002);
/// Two or more drivers matched a node at the same highest priority; the
/// unbroken tie is a packaging defect, so the node is refused a binding
/// (`AGENTS.md` §18.3).
pub const NODE_TIE_REJECTED: EventId = EventId(13_003);
/// A node's winning driver failed to load through the driver-host load
/// gate; the node stays unbound (fail closed, `AGENTS.md` §5.4).
pub const NODE_LOAD_FAILED: EventId = EventId(13_004);
/// The read-only `/System` driver-store catalogue could not be fetched (the
/// store endpoint is unbound or the store is unreadable). The device
/// manager loads nothing but keeps observing the hardware tree — never an
/// error (fail-soft, `AGENTS.md` §18.4 / §2.9).
pub const DRIVER_STORE_UNAVAILABLE: EventId = EventId(13_005);
/// A hardware-tree snapshot was read: its generation and node count. Emitted
/// at `Debug` (verbose boot/hotplug diagnostics, filtered out by default).
pub const TREE_OBSERVED: EventId = EventId(13_006);
/// One node of the observed hardware tree: its id, parent, class, and match-
/// key count. Emitted at `Debug` (one record per node, filtered out by
/// default — lower the level to trace what reached the device manager).
pub const NODE_OBSERVED: EventId = EventId(13_007);

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [EventId; 7] = [
        NODE_BOUND,
        NODE_UNBOUND,
        NODE_TIE_REJECTED,
        NODE_LOAD_FAILED,
        DRIVER_STORE_UNAVAILABLE,
        TREE_OBSERVED,
        NODE_OBSERVED,
    ];

    #[test]
    fn ids_are_inside_reserved_range() {
        for id in ALL {
            assert!(id.0 >= DEVMGR_RANGE_START && id.0 < DEVMGR_RANGE_END);
        }
    }

    #[test]
    fn ids_are_unique() {
        let mut ids = ALL.map(|id| id.0);
        ids.sort_unstable();
        for w in ids.windows(2) {
            assert_ne!(w[0], w[1], "duplicate devmgr EventId");
        }
    }
}
