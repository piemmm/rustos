//! Stable [`rustos_log::EventId`] constants emitted by `init`.
//!
//! Per `lib/log` convention (`AGENTS.md` §2.5) every subsystem owns a
//! 1 000-wide reserved range. PID 1 occupies `9000..10000` (adjacent to the
//! System Information service's `8000..9000`). Once shipped the numeric
//! values must never be re-used or re-numbered — external audit-log
//! consumers rely on them (`AGENTS.md` §19.4).

use rustos_log::EventId;

/// Range start (inclusive) reserved for `init` event identifiers.
///
/// Exposed so audit consumers can filter by subsystem in O(1) instead of
/// matching on individual event identifiers.
pub const INIT_RANGE_START: u32 = 9_000;
/// Range end (exclusive) reserved for `init` event identifiers.
pub const INIT_RANGE_END: u32 = 10_000;

/// A service was started: its manifest decoded, its capability ceiling
/// computed and granted, and its binary handed to the [`Spawner`](crate::Spawner).
pub const SERVICE_STARTED: EventId = EventId(9_001);
/// A service could not be started: its manifest failed to decode, or the
/// [`Spawner`](crate::Spawner) refused to launch it. Its dependents are
/// skipped (`AGENTS.md` §5.4.5).
pub const SERVICE_START_FAILED: EventId = EventId(9_002);
/// A service was refused because its manifest requests a capability the
/// system authority does not hold — a denial is a security-relevant
/// decision in its own right (`AGENTS.md` §5.4.4).
pub const SERVICE_DENIED: EventId = EventId(9_003);
/// A service was skipped because a dependency failed to start; it is never
/// brought up against a missing prerequisite (`AGENTS.md` §5.4.5).
pub const SERVICE_SKIPPED: EventId = EventId(9_004);
/// A registered service's process exited and was reaped.
pub const SERVICE_EXITED: EventId = EventId(9_005);
/// An inherited orphan (a process PID 1 did not itself start) was reaped.
pub const ORPHAN_REAPED: EventId = EventId(9_006);
/// The registered service graph was rejected before any service started:
/// a dependency names an unregistered service, or the graph contains a
/// cycle. The whole bring-up fails closed (`AGENTS.md` §5.4.5).
pub const GRAPH_REJECTED: EventId = EventId(9_007);

#[cfg(test)]
mod tests {
    use super::{
        GRAPH_REJECTED, INIT_RANGE_END, INIT_RANGE_START, ORPHAN_REAPED, SERVICE_DENIED,
        SERVICE_EXITED, SERVICE_SKIPPED, SERVICE_STARTED, SERVICE_START_FAILED,
    };

    const ALL: [u32; 7] = [
        SERVICE_STARTED.0,
        SERVICE_START_FAILED.0,
        SERVICE_DENIED.0,
        SERVICE_SKIPPED.0,
        SERVICE_EXITED.0,
        ORPHAN_REAPED.0,
        GRAPH_REJECTED.0,
    ];

    #[test]
    fn ids_are_inside_reserved_range() {
        for id in ALL {
            assert!((INIT_RANGE_START..INIT_RANGE_END).contains(&id));
        }
    }

    #[test]
    fn ids_are_unique() {
        let mut ids = ALL;
        ids.sort_unstable();
        for w in ids.windows(2) {
            assert_ne!(w[0], w[1], "duplicate init EventId");
        }
    }
}
