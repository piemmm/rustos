//! The watchdog fixture's shared vocabulary: the audit records its `Run`
//! binary emits and the renewal count it emits them for
//! (`plans/NEW-SERVICEMANAGER.md` SVC-8, `plans/WATCHDOG.md`).
//!
//! Defined here rather than in the program because the consuming vertical's
//! witness sink keys on them: the sink and the program are two consumers of
//! one definition, so they cannot drift into each other's silence.

#![no_std]
#![deny(missing_docs)]

use tairix_log::EventId;

/// Emitted once per accepted liveness renewal, before the fixture wedges.
///
/// The vertical requires [`RENEWALS_BEFORE_WEDGE`] of these *before* the
/// manager's timeout, which is what makes the healthy half a counted
/// positive rather than an absence: a fixture that never renewed would time
/// out just the same, so only the count distinguishes "renewal works" from
/// "renewal is a no-op".
pub const RENEWED: EventId = EventId(4540);

/// Emitted once when the fixture stops renewing and parks for good.
///
/// Marks the instant the wedge begins, so a reader of the transcript can
/// see that the timeout which follows is the one the fixture provoked and
/// not an earlier accident.
pub const WEDGED: EventId = EventId(4541);

/// Emitted instead of [`RENEWED`] when the manager reports no watchdog.
///
/// An honest, distinguishable outcome: the fixture is running under a
/// manager that is not watching it, so there is nothing to renew and
/// nothing the vertical could prove. Never silent.
pub const UNWATCHED: EventId = EventId(4542);

/// The service this fixture stands in for, as PID 1's floor description
/// names it.
///
/// Must equal the `name` in the fixture's `AppInfo.toml`, which is what
/// decides the bundle directory the disk plants it at. The two cannot drift
/// silently: a name the base store does not contain fails the image build
/// (the substitution would otherwise become an addition), and a name that
/// is some *other* service leaves the vertical waiting for a timeout that
/// never comes.
pub const SUBSTITUTES: &str = "netstack";

/// The audit-record field the manager names a service in, so the witness
/// reads the manager's own attribution rather than guessing from ordering.
pub const SERVICE_FIELD: &str = "service";

/// How many renewals the fixture makes before it wedges.
///
/// The runtime renews at half the manager's interval, so three renewals
/// carry the service past one whole interval — the point at which an
/// un-renewed watchdog would already have fired. Proving survival past that
/// line is the only thing that distinguishes a working renewal from a
/// discarded one, so the count cannot be lower; making it higher would only
/// spend guest seconds.
pub const RENEWALS_BEFORE_WEDGE: u32 = 3;
