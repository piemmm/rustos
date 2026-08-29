//! TAIRiX time-synchronisation client: **when** to set the clock, and what to
//! record when it is set (`plans/TIMESYNC.md`).
//!
//! The NTP protocol itself lives in [`tairix_net::ntp`] — this crate owns no
//! wire format and no retry policy. What it owns is the clock policy a
//! Raspberry Pi forces on us: a machine with no RTC boots knowing nothing and
//! must correct itself at once, while a machine with a working RTC must *not*
//! query a public server on every reboot just because it can.
//!
//! # The decision, not a schedule
//!
//! [`decide`] answers "should this machine sync now?" from three inputs: the
//! wall clock's current reading, the persisted [`SyncRecord`] of when time was
//! last seen, and the configured refresh cadence. It syncs immediately only
//! when there is a *reason* to distrust the clock — it is unset, it reads
//! outside the plausibility window, it has gone backwards, or the persisted
//! last-seen instant is further behind than [`STALE_BOOT_GAP`]. Otherwise the
//! clock is believed and the next query waits for the refresh cadence to
//! elapse *in uptime*, so a machine that reboots ten times an hour still
//! queries once a day.
//!
//! # Provenance describes the source, never the size of the change
//!
//! A validated sample always records
//! [`WallTimeState::Trusted`]: the new
//! reading comes wholly from the network time source, whether it established
//! an unset clock, replaced an RTC's, or refreshed an earlier sync.
//! [`WallTimeState::Adjusted`] is
//! deliberately never used here — the ABI defines it as a previously-set time
//! corrected after the fact, which describes a manual step, not a source
//! replacing its own value.
//!
//! A correction wider than [`STEP_THRESHOLD`] is still reported as a *step*
//! rather than a refinement, because a large jump can move certificate
//! validity and change how a reader interprets the log. That is an audit
//! distinction the caller records; it does not change the provenance.
//!
//! # No I/O, no clock, no randomness
//!
//! Everything here is pure and host-tested: monotonic time, the wall-clock
//! reading, and every CSPRNG word are supplied by the caller, exactly as the
//! DHCP, DNS, and TCP engines take theirs.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

use tairix_abi::time::{Duration64, Time64};
use tairix_abi::{is_plausible_wall_time, WallClockReading, WallTimeState};
use tairix_net::ntp::{jitter, KissCode, NtpClient, Outcome, Query, RejectReason, Sample};

/// How far the clock's reading may lag the persisted last-seen instant before
/// the machine is assumed to have been off long enough that its clock has
/// drifted or been lost.
///
/// Five days: comfortably longer than a weekend powered down, short enough
/// that a dead RTC battery is caught on the next boot rather than weeks later.
pub const STALE_BOOT_GAP: Duration64 = Duration64::from_secs(5 * 86_400);

/// Default cadence for the steady-state refresh, measured in **uptime**.
///
/// One day. Gated on uptime rather than wall time so a machine that reboots
/// frequently does not re-query on every boot — the reason a trusted RTC
/// reading is left alone.
pub const DEFAULT_REFRESH: Duration64 = Duration64::from_secs(86_400);

/// Widest randomised delay before the first query of a boot.
///
/// Short, because a machine whose clock is unset is barely usable until it has
/// one; randomised, because a fleet restored from a single image and powered
/// on together would otherwise all query at the same instant.
pub const INITIAL_DELAY_SPAN: Duration64 = Duration64::from_secs(8);

/// Correction magnitude above which an applied sample is reported as a step
/// rather than a refinement.
///
/// An audit classification only — it never changes the recorded provenance.
pub const STEP_THRESHOLD: Duration64 = Duration64::from_secs(1);

/// Why the clock is not to be believed and must be synchronised at once.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SyncReason {
    /// No wall time has been established this boot.
    ClockUnset,
    /// The reading falls outside the plausibility window — before this
    /// release existed, or absurdly far in the future.
    Implausible,
    /// The reading is *earlier* than the last instant time was seen at. Real
    /// time does not run backwards, so the clock has lost its state (the
    /// classic dead-RTC-battery reset, which can land on a date that is
    /// plausible in itself).
    WentBackwards,
    /// More than [`STALE_BOOT_GAP`] separates the reading from the last
    /// instant time was seen at.
    StaleBoot,
}

/// What to do about the clock at start-up.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Decision {
    /// Query as soon as the network allows, for this reason.
    SyncNow(SyncReason),
    /// The clock is believed. Wait this long in uptime before refreshing.
    RefreshAfter(Duration64),
}

/// The persisted record of when this machine last knew the time.
///
/// Written on each successful sync and read back at start-up. It is the only
/// input that can distinguish "powered off for an hour" from "powered off for
/// a month", which no clock reading alone can tell.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct SyncRecord {
    /// When time was last successfully synchronised, if ever.
    pub last_sync: Option<Time64>,
    /// The latest instant this machine is known to have observed. Never
    /// earlier than `last_sync`.
    pub last_seen: Option<Time64>,
}

impl SyncRecord {
    /// The empty record: nothing has ever been observed.
    ///
    /// A missing or unreadable store resolves to this rather than to a
    /// guessed instant, so the stale-boot and went-backwards rules simply do
    /// not fire instead of firing on a fiction.
    pub const EMPTY: Self = Self {
        last_sync: None,
        last_seen: None,
    };

    /// The record after observing `now` as a synchronised instant.
    #[must_use]
    pub fn synced_at(self, now: Time64) -> Self {
        Self {
            last_sync: Some(now),
            last_seen: Some(match self.last_seen {
                Some(seen) if seen > now => seen,
                _ => now,
            }),
        }
    }
}

/// Decide whether to synchronise now, given the clock's `reading`, the
/// persisted `record`, and the configured `refresh` cadence.
///
/// The order of the tests is the policy: each urgent case is a distinct reason
/// to distrust the clock, and only a clock that survives all of them is
/// believed.
#[must_use]
pub fn decide(reading: WallClockReading, record: SyncRecord, refresh: Duration64) -> Decision {
    if !reading.state().is_set() {
        return Decision::SyncNow(SyncReason::ClockUnset);
    }
    let now = reading.time();
    if !is_plausible_wall_time(now) {
        return Decision::SyncNow(SyncReason::Implausible);
    }
    if let Some(seen) = record.last_seen {
        if now < seen {
            return Decision::SyncNow(SyncReason::WentBackwards);
        }
        if now.saturating_duration_since(seen) > STALE_BOOT_GAP {
            return Decision::SyncNow(SyncReason::StaleBoot);
        }
    }
    Decision::RefreshAfter(refresh)
}

/// A clock change to apply, and what to say about it in the audit log.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ClockUpdate {
    /// The instant to set the clock to.
    pub wall: Time64,
    /// The provenance to record. Always
    /// [`WallTimeState::Trusted`] — see
    /// the module documentation.
    pub state: WallTimeState,
    /// Magnitude of the change from the previous reading, or `None` when
    /// there was no reading to correct from and the clock is being
    /// established for the first time this boot.
    pub correction: Option<Duration64>,
    /// Whether this is a step rather than a refinement: an establishment, or
    /// a correction exceeding [`STEP_THRESHOLD`]. An audit classification,
    /// not a provenance one.
    pub stepped: bool,
    /// The round trip the sample was measured over, for audit.
    pub round_trip: Duration64,
    /// The server's stratum, for audit.
    pub stratum: u8,
}

/// What the client concluded about a received datagram.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Event {
    /// Apply this change to the clock.
    Apply(ClockUpdate),
    /// The reply was ours but unusable. Audit and move on.
    Refused(RejectReason),
    /// The server asked never to be queried again and has been dropped.
    ServerRetired {
        /// Index into the caller's server table.
        server: u8,
        /// The code it sent.
        code: KissCode,
    },
    /// The server asked for a slower rate; its interval has been widened.
    RateLimited {
        /// Index into the caller's server table.
        server: u8,
    },
    /// Not ours, or nothing was outstanding. No state changed.
    Ignored,
}

/// The time-synchronisation client: the start-up decision, the NTP engine it
/// drives, and the clock update it produces.
///
/// A non-blocking state machine, so a service can fold its one deadline in
/// with whatever else its reactor serves and never poll.
#[derive(Clone, Debug)]
pub struct TimeSync {
    ntp: NtpClient,
    urgency: Option<SyncReason>,
    refresh: Duration64,
}

impl TimeSync {
    /// Build a client for `servers` configured servers with the `refresh`
    /// cadence, deciding from `reading` and `record` when its first query is
    /// due relative to `now`.
    ///
    /// `entropy` must be a CSPRNG word; it spreads the first query so a fleet
    /// booting together does not converge on one instant. The refresh cadence
    /// is floored at [`MIN_POLL`](tairix_net::ntp::MIN_POLL) by the engine, so a
    /// misconfigured value
    /// cannot make this client impolite.
    #[must_use]
    pub fn new(
        servers: u8,
        refresh: Duration64,
        reading: WallClockReading,
        record: SyncRecord,
        now: Duration64,
        entropy: u64,
    ) -> Self {
        let (urgency, delay) = match decide(reading, record, refresh) {
            Decision::SyncNow(reason) => (Some(reason), jitter(INITIAL_DELAY_SPAN, entropy)),
            Decision::RefreshAfter(after) => (None, jitter(after, entropy)),
        };
        let first = Duration64::from_nanos(
            now.saturating_total_nanos()
                .saturating_add(delay.saturating_total_nanos()),
        );
        Self {
            ntp: NtpClient::new(servers, refresh, first),
            urgency,
            refresh,
        }
    }

    /// Why the first query of this boot is urgent, or `None` when the clock
    /// was believed and this is an ordinary refresh.
    #[must_use]
    pub const fn urgency(&self) -> Option<SyncReason> {
        self.urgency
    }

    /// The configured refresh cadence, before the engine's floor.
    #[must_use]
    pub const fn refresh(&self) -> Duration64 {
        self.refresh
    }

    /// Whether every configured server has refused further queries.
    #[must_use]
    pub const fn is_exhausted(&self) -> bool {
        self.ntp.is_exhausted()
    }

    /// The single monotonic instant to wake at, or `None` when there is
    /// nothing left to wait for.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Duration64> {
        self.ntp.next_deadline()
    }

    /// Advance the engine, returning a request to send if one is due.
    ///
    /// `entropy` must be a **fresh** CSPRNG word on every call: it becomes the
    /// nonce that authenticates the reply.
    pub fn poll(&mut self, now: Duration64, entropy: u64) -> Option<Query> {
        self.ntp.poll(now, entropy)
    }

    /// Feed a received datagram to the engine and decide what it means for the
    /// clock, whose current reading is `reading`.
    pub fn on_datagram(
        &mut self,
        now: Duration64,
        reading: WallClockReading,
        bytes: &[u8],
    ) -> Event {
        match self.ntp.on_datagram(now, bytes) {
            Outcome::Sample(sample) => {
                self.urgency = None;
                Event::Apply(update_for(sample, reading))
            }
            Outcome::Rejected(reason) => Event::Refused(reason),
            Outcome::ServerRetired { server, code } => Event::ServerRetired { server, code },
            Outcome::RateLimited { server } => Event::RateLimited { server },
            Outcome::Unsolicited => Event::Ignored,
        }
    }
}

/// Turn a validated sample into the clock update to apply, measuring the
/// correction against `reading`.
fn update_for(sample: Sample, reading: WallClockReading) -> ClockUpdate {
    // An unset clock has no reading to measure against, so there is no
    // correction to report — reporting the whole instant as one would be a
    // fabricated magnitude. Establishing the clock always counts as a step.
    let (correction, stepped) = if reading.state().is_set() {
        let previous = reading.time();
        let delta = if sample.true_time >= previous {
            sample.true_time.saturating_duration_since(previous)
        } else {
            previous.saturating_duration_since(sample.true_time)
        };
        (Some(delta), delta > STEP_THRESHOLD)
    } else {
        (None, true)
    };
    ClockUpdate {
        wall: sample.true_time,
        state: WallTimeState::Trusted,
        correction,
        stepped,
        round_trip: sample.round_trip,
        stratum: sample.stratum,
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
