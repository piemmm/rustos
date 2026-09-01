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

/// A service was started: its binary was handed to the
/// [`Spawner`](crate::Spawner) as its service account, and the kernel derived
/// and granted its capabilities from the signed bundle at load time.
pub const SERVICE_STARTED: EventId = EventId(9_001);
/// A service could not be started: the kernel's load gate refused to launch
/// it (a bad manifest, a capability beyond the account's ceiling, or another
/// load failure). Its dependents are skipped.
///
/// `9_003` is retired: the manager no longer decides capability grants (the
/// kernel is the single authority and records its own denial), so there is
/// no separate init-side capability-escalation event. The value is left a
/// gap rather than reused.
pub const SERVICE_START_FAILED: EventId = EventId(9_002);
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
/// An on-demand service was **activated**: a client connected to its
/// endpoint while it was down, and the manager started it (as its sandboxed
/// service account) to satisfy the connection.
pub const SERVICE_ACTIVATED: EventId = EventId(9_012);
/// A connecting client was **queued**: the service it connected to is not
/// yet ready, so the client is parked and will be woken when the service
/// announces readiness (never busy-polled).
pub const ACTIVATION_QUEUED: EventId = EventId(9_013);
/// An endpoint-activation connect was **refused**: the named service is not
/// registered, the client lacks the capability the endpoint requires, the
/// service cannot be activated in its current state, or its pending-connection
/// queue is full. A security-relevant denial, audited with its reason and
/// failing closed (the client is granted nothing).
pub const ACTIVATION_DENIED: EventId = EventId(9_014);
/// An idle-linger timer was **armed**: the last client of an on-demand
/// service disconnected, so the manager armed a single one-shot timer after
/// which the service is idle-stopped unless a new client connects first.
pub const SERVICE_LINGER_ARMED: EventId = EventId(9_015);
/// A graceful **stop** was initiated: the manager asked the service to exit
/// (for example after its idle-linger expired) and is awaiting its grace
/// period before forcing it down.
pub const SERVICE_STOPPING: EventId = EventId(9_016);
/// A service was **force-terminated**: it did not exit within its graceful
/// stop grace period, so the manager forced the process down.
pub const SERVICE_FORCE_TERMINATED: EventId = EventId(9_017);
/// A crashed service is **scheduled to restart**: its
/// [`RestartPolicy`](tairix_abi::RestartPolicy) asked for a restart after
/// this exit and the crash-loop budget was not spent, so the manager armed
/// a one-shot backoff deadline after which it is relaunched.
pub const SERVICE_RESTART_SCHEDULED: EventId = EventId(9_018);
/// A crashed service was **not** restarted because its crash-loop budget is
/// spent: it kept dying before it ran stably, so the manager gave up rather
/// than relaunch it forever (fail closed — never an unbounded retry loop).
pub const SERVICE_RESTART_EXHAUSTED: EventId = EventId(9_019);
/// A service was **not** registered because its service account is outside
/// the manager's [`AuthorityScope`](crate::AuthorityScope): a per-user
/// manager tried to manage a service running as a system account or another
/// user's uid. A security-relevant refusal, audited and failing closed — no
/// per-user manager can raise a service to authority it does not itself hold.
pub const SERVICE_SCOPE_REJECTED: EventId = EventId(9_020);
/// A control-surface `start` request brought a service up (or found it
/// already up): a control tool, through the capability-gated control
/// endpoint, asked the manager to start a specific registered service now.
pub const SERVICE_CONTROL_STARTED: EventId = EventId(9_021);
/// A control-surface `stop` request tore a service — and, in
/// reverse-dependency order, its dependents — down gracefully.
pub const SERVICE_CONTROL_STOPPED: EventId = EventId(9_022);
/// A control-surface request was refused: it named an unknown or
/// policy-invalid service, or the service could not be started in its
/// current state or with a required readiness condition unmet. A
/// security-relevant refusal, audited and failing closed — the request
/// changes nothing.
pub const SERVICE_CONTROL_DENIED: EventId = EventId(9_023);
/// A running service's **liveness watchdog was armed**: a service that opted
/// into the watchdog (a non-zero
/// [`ServiceSpec::watchdog`](crate::ServiceSpec::watchdog) interval) reached a
/// running state, so the manager armed a single one-shot deadline by which it
/// must renew its heartbeat.
pub const SERVICE_WATCHDOG_ARMED: EventId = EventId(9_024);
/// A service's **liveness watchdog elapsed**: it did not renew its heartbeat
/// within its watchdog interval, so the manager concluded its process had
/// wedged and force-terminated it. Classified as an abnormal failure, so the
/// service's [`RestartPolicy`](tairix_abi::RestartPolicy) then applies (an
/// `on-failure`/`always` service is relaunched with the crash-loop budget; a
/// `never` service is left down). A security- and reliability-relevant event.
pub const SERVICE_WATCHDOG_TIMEOUT: EventId = EventId(9_025);
/// A service's **persistent enrolment was changed** by an enrolment-surface
/// request: an administrator, through the capability-gated enrolment
/// endpoint, enabled or disabled a service, so the manager recorded the
/// decision and will obey it on the next boot as well as now.
pub const SERVICE_ENROLMENT_CHANGED: EventId = EventId(9_026);
/// An enrolment-surface request was refused: it named an unknown or
/// policy-invalid service, or one running under an account outside this
/// manager's [`AuthorityScope`](crate::AuthorityScope). A security-relevant
/// refusal, audited and failing closed — the request records nothing.
pub const SERVICE_ENROLMENT_DENIED: EventId = EventId(9_027);
/// A service was **stopped because the administrator's enrolment override
/// became readable** and disables it. Pre-unlock the manager obeys the
/// image's enrolment layer alone, so a service the administrator disabled runs
/// until the override document on the encrypted root can be read; this records
/// the moment that narrowing is applied.
pub const SERVICE_ENROLMENT_REVOKED: EventId = EventId(9_028);

/// Every event id this crate emits, in numeric order.
///
/// One list, so the uniqueness/range checks and the message table's coverage
/// check cannot disagree about which ids exist — an id missing from here would
/// otherwise render as the generic fallback with nothing to catch it.
pub const ALL: [EventId; 27] = [
    SERVICE_STARTED,
    SERVICE_START_FAILED,
    SERVICE_SKIPPED,
    SERVICE_EXITED,
    ORPHAN_REAPED,
    GRAPH_REJECTED,
    SERVICE_READY,
    CONDITION_SATISFIED,
    NOTIFY_REJECTED,
    SERVICE_NOT_ENROLLED,
    SERVICE_ACTIVATED,
    ACTIVATION_QUEUED,
    ACTIVATION_DENIED,
    SERVICE_LINGER_ARMED,
    SERVICE_STOPPING,
    SERVICE_FORCE_TERMINATED,
    SERVICE_RESTART_SCHEDULED,
    SERVICE_RESTART_EXHAUSTED,
    SERVICE_SCOPE_REJECTED,
    SERVICE_CONTROL_STARTED,
    SERVICE_CONTROL_STOPPED,
    SERVICE_CONTROL_DENIED,
    SERVICE_WATCHDOG_ARMED,
    SERVICE_WATCHDOG_TIMEOUT,
    SERVICE_ENROLMENT_CHANGED,
    SERVICE_ENROLMENT_DENIED,
    SERVICE_ENROLMENT_REVOKED,
];

#[cfg(test)]
mod tests {
    use super::{ALL, INIT_RANGE_END, INIT_RANGE_START};
    use alloc::collections::BTreeSet;

    #[test]
    fn ids_are_inside_reserved_range() {
        for id in ALL {
            assert!((INIT_RANGE_START..INIT_RANGE_END).contains(&id.0));
        }
    }

    #[test]
    fn ids_are_unique() {
        let set: BTreeSet<u32> = ALL.iter().map(|id| id.0).collect();
        assert_eq!(set.len(), ALL.len());
    }
}
