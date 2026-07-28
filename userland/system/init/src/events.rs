//! Stable [`tairix_log::EventId`] constants emitted by `init`.
//!
//! Per `lib/log` convention every subsystem owns a
//! 1 000-wide reserved range. PID 1 occupies `9000..10000` (adjacent to the
//! System Information service's `8000..9000`). Once shipped the numeric
//! values must never be re-used or re-numbered — external audit-log
//! consumers rely on them.

use tairix_log::EventId;

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
/// skipped.
pub const SERVICE_START_FAILED: EventId = EventId(9_002);
/// A service was refused because its manifest requests a capability the
/// system authority does not hold — a denial is a security-relevant
/// decision in its own right.
pub const SERVICE_DENIED: EventId = EventId(9_003);
/// A service was skipped because a dependency failed to start; it is never
/// brought up against a missing prerequisite.
pub const SERVICE_SKIPPED: EventId = EventId(9_004);
/// A registered service's process exited and was reaped.
pub const SERVICE_EXITED: EventId = EventId(9_005);
/// An inherited orphan (a process PID 1 did not itself start) was reaped.
pub const ORPHAN_REAPED: EventId = EventId(9_006);
/// The registered service graph was rejected before any service started:
/// a dependency names an unregistered service, or the graph contains a
/// cycle. The whole bring-up fails closed.
pub const GRAPH_REJECTED: EventId = EventId(9_007);
/// A service reached readiness: an `immediate` service whose spawn
/// succeeded, or a `notify` service that announced itself up. This is the
/// point the manager releases the service's dependents.
pub const SERVICE_READY: EventId = EventId(9_008);
/// A named readiness condition became satisfied (a providing service reached
/// readiness, or the manager/kernel signalled it), releasing any services
/// that were waiting on it.
pub const CONDITION_SATISFIED: EventId = EventId(9_009);
/// A readiness notification was rejected: it named no known service, or it
/// arrived for a service that was not in the `starting` state (a protocol
/// violation). Fails closed — the notice is ignored, never trusted.
pub const NOTIFY_REJECTED: EventId = EventId(9_010);
/// A discovered `/System/Services` bundle was **not** registered for
/// bring-up because it is not enrolled in the registration store: presence
/// on disk never implies eligibility, so an unenrolled bundle is skipped
/// (fail closed) rather than started. The security-relevant decision is
/// audited so an operator can see a present-but-inactive service.
pub const SERVICE_NOT_ENROLLED: EventId = EventId(9_011);

#[cfg(test)]
mod tests {
    use super::{
        CONDITION_SATISFIED, GRAPH_REJECTED, INIT_RANGE_END, INIT_RANGE_START, NOTIFY_REJECTED,
        ORPHAN_REAPED, SERVICE_DENIED, SERVICE_EXITED, SERVICE_NOT_ENROLLED, SERVICE_READY,
        SERVICE_SKIPPED, SERVICE_STARTED, SERVICE_START_FAILED,
    };

    const ALL: [u32; 11] = [
        SERVICE_STARTED.0,
        SERVICE_START_FAILED.0,
        SERVICE_DENIED.0,
        SERVICE_SKIPPED.0,
        SERVICE_EXITED.0,
        ORPHAN_REAPED.0,
        GRAPH_REJECTED.0,
        SERVICE_READY.0,
        CONDITION_SATISFIED.0,
        NOTIFY_REJECTED.0,
        SERVICE_NOT_ENROLLED.0,
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
