//! TAIRiX time-synchronisation service (`timed`) — the orchestration half.
//!
//! `timed` is the only holder of `CAP_TIME_SET` on a running machine. It reads
//! the clock, decides whether to believe it (`tairix_timesync`), queries the
//! network time servers that crate's source policy selected
//! (`tairix_net::ntp`), and applies a validated sample. This crate is that orchestration over **injected seams**,
//! so all of it is host-tested; `src/run.rs` wires the real clock, sockets,
//! resolver, and files.
//!
//! # The authority split
//!
//! The process holding `CAP_TIME_SET` never parses a packet. Every received
//! datagram is evaluated inside a capability-less sandbox worker
//! ([`tairix_sandbox::timesync`]), which gates the nonce echo caller-side
//! before the worker is involved and re-validates any returned sample against
//! the plausibility, round-trip, and stratum bounds. Only the *verdict*
//! reaches the engine here.
//!
//! # Nothing here polls
//!
//! [`Timed::next_deadline`] folds the engine's single monotonic wake instant.
//! The reactor arms one timer, calls [`Timed::poll`] when it lapses and
//! [`Timed::on_datagram`] when a datagram arrives, and otherwise gives the CPU
//! up. A send that fails is not retried on the spot: the engine's own response
//! timeout ends the transaction and its bounded backoff schedules the next
//! attempt, so a machine whose network is not up yet costs nothing and no
//! packet is sent faster than the politeness policy allows.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::vec::Vec;

use tairix_abi::rtc_ipc::RtcReading;
use tairix_abi::time::{Duration64, Time64};
use tairix_abi::{Errno, FieldValue, WallClockReading, WallTimeState};
use tairix_log::{Event, Field, Level, Sink};
use tairix_net::ntp::Query;
use tairix_sandbox::host::{Launcher, ParserSandbox};
use tairix_sandbox::timesync::evaluate_datagram;
use tairix_timesync::{
    events, ClockUpdate, Event as SyncEvent, ServerSelection, ServerSource, SyncReason, SyncRecord,
    TimeServer, TimeSync,
};
use tairix_util::retry::RetryLadder;

/// The wall and monotonic clocks the service reads and sets.
///
/// Setting the wall clock is the whole authority this service holds, so the
/// seam is deliberately three operations wide and nothing more.
pub trait Clock {
    /// The monotonic uptime reading the engine schedules against.
    fn monotonic(&self) -> Duration64;

    /// The wall clock's current instant and provenance.
    ///
    /// # Errors
    ///
    /// The kernel's typed refusal. A clock that cannot be read is treated as
    /// unset, so the service corrects it rather than trusting a reading it
    /// never got.
    fn wall(&self) -> Result<WallClockReading, Errno>;

    /// Set the wall clock to `time` with provenance `state`.
    ///
    /// # Errors
    ///
    /// The kernel's typed refusal (a missing capability, a non-settable
    /// state, or a clock that is not wired).
    fn set_wall(&self, time: Time64, state: WallTimeState) -> Result<(), Errno>;
}

/// The document the persisted [`SyncRecord`] lives in.
pub trait RecordStore {
    /// Read the whole document, or `None` when none exists yet.
    ///
    /// # Errors
    ///
    /// The backing's typed refusal. An unreadable record resolves to
    /// [`SyncRecord::EMPTY`], so the stale-boot and went-backwards rules
    /// simply do not fire rather than firing on a fiction.
    fn read(&self) -> Result<Option<Vec<u8>>, Errno>;

    /// Replace the whole document with `bytes`.
    ///
    /// # Errors
    ///
    /// The backing's typed refusal.
    fn write(&self, bytes: &[u8]) -> Result<(), Errno>;
}

/// The board's real-time clock, as the service reaches it.
///
/// The chip belongs to an autoloaded driver holding no clock authority; this
/// service is the only holder of `CAP_TIME_SET` and is what turns a chip
/// reading into a wall time. Keeping the split here is what makes the
/// provenance ladder enforceable: the driver reports a reading, and this
/// service — not the driver — says it is `Firmware`.
pub trait RtcSource {
    /// Read the chip: the instant it can vouch for, if any, and its status.
    ///
    /// # Errors
    ///
    /// The typed refusal. A board with no clock chip has no driver serving
    /// the endpoint, which reads as `NotFound` — an ordinary state, not a
    /// failure.
    fn read(&mut self) -> Result<RtcReading, Errno>;

    /// Write `time` back to the chip so the next boot starts from it.
    ///
    /// # Errors
    ///
    /// The typed refusal. A failure costs the next boot its head start and
    /// nothing else, so a caller reports it and carries on.
    fn set(&mut self, time: Time64) -> Result<(), Errno>;
}

/// The datagram transport to the servers in use.
///
/// `index` addresses the selected server list positionally, exactly as the
/// engine's rotation cursor does, so the engine never holds an address and
/// name resolution stays out of the pure path.
pub trait Transport {
    /// Send `packet` to the server at `index`.
    ///
    /// # Errors
    ///
    /// The typed reason no packet left the machine — no address for that
    /// server, or a refused socket send.
    fn send(&mut self, index: u8, packet: &[u8]) -> Result<(), Errno>;
}

/// What one engine step concluded, for the reactor and the tests.
///
/// Every variant is already audited by the time it is returned; the value is
/// what lets a caller (and a QEMU vertical's witness) observe the step
/// without re-deriving it from the log.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Step {
    /// A request was sent to the server at this index.
    Queried(u8),
    /// A request was due but could not be sent, for this reason.
    NotSent(Errno),
    /// The clock was set from a validated sample.
    ClockSet(ClockUpdate),
    /// A validated sample could not be applied to the clock.
    ClockRefused(Errno),
    /// The datagram produced no usable sample; it was audited and dropped.
    NoSample,
    /// Nothing was due and nothing changed.
    Idle,
}

/// The time-synchronisation service: the clock policy, the NTP engine, the
/// sandboxed evaluation, and the persisted record, over injected seams.
pub struct Timed<C: Clock, R: RecordStore, T: Transport, L: Launcher, S: Sink, K: RtcSource> {
    clock: C,
    store: R,
    transport: T,
    sandbox: ParserSandbox<L, S>,
    sink: S,
    rtc: K,
    sync: TimeSync,
    record: SyncRecord,
    /// The ladder for re-reading the RTC while its driver is not up yet, or
    /// `None` once the chip answered or the ladder is spent.
    rtc_retry: Option<RetryLadder>,
    /// The servers in use, held only so an audit record can name the one a
    /// decision was about; the engine itself addresses them by index.
    servers: Vec<TimeServer>,
    /// Which tier those servers came from, for the audit trail.
    source: ServerSource,
    /// Whether the "every server has refused" state has already been
    /// reported. Exhaustion is a *state* the engine can sit in for the rest
    /// of the boot, so the record is edge-triggered: announcing it on every
    /// poll would repeat the same warning indefinitely. Cleared when a
    /// server becomes usable again, so a later exhaustion is reported afresh.
    exhaustion_reported: bool,
}

/// Everything [`Timed::new`] needs, so the constructor does not grow a
/// six-argument signature.
pub struct TimedConfig<C: Clock, R: RecordStore, T: Transport, L: Launcher, S: Sink, K: RtcSource> {
    /// The wall and monotonic clocks.
    pub clock: C,
    /// The persisted last-seen record's backing document.
    pub store: R,
    /// The datagram transport to the servers in use.
    pub transport: T,
    /// The sandbox the response evaluation runs in.
    pub sandbox: ParserSandbox<L, S>,
    /// The audit sink.
    pub sink: S,
    /// The board's real-time clock.
    pub rtc: K,
    /// The servers to query and the tier they came from.
    pub selection: ServerSelection,
    /// The steady-state refresh cadence, measured in uptime.
    pub refresh: Duration64,
    /// A CSPRNG word, spreading the first query so a fleet booting from one
    /// image does not converge on a single instant.
    pub entropy: u64,
}

impl<C: Clock, R: RecordStore, T: Transport, L: Launcher, S: Sink + Clone, K: RtcSource>
    Timed<C, R, T, L, S, K>
{
    /// Build the service, deciding from the clock's reading and the persisted
    /// record when the first query of this boot is due.
    ///
    /// A clock that cannot be read is treated as unset: correcting a clock we
    /// could not see beats believing a reading we never got.
    #[must_use]
    pub fn new(config: TimedConfig<C, R, T, L, S, K>) -> Self {
        let TimedConfig {
            clock,
            store,
            transport,
            sandbox,
            sink,
            mut rtc,
            selection,
            refresh,
            entropy,
        } = config;
        let record = read_record(&store);
        // The RTC first, so a board that has one enters the decision matrix
        // with a `Firmware` clock rather than an unset one — which is the
        // whole reason a machine carries a clock chip.
        let rtc_retry = RetryLadder::arm(
            clock.monotonic().saturating_total_nanos(),
            RTC_RETRY_BASE_NANOS,
            RTC_RETRY_ATTEMPTS,
            seed_clock_from_rtc(&mut rtc, &clock, &sink),
        );
        let reading = clock
            .wall()
            .unwrap_or_else(|_| WallClockReading::new(Time64::from_secs(0), WallTimeState::Unset));
        let ServerSelection { source, servers } = selection;
        let count = u8::try_from(servers.len()).unwrap_or(u8::MAX);
        let sync = TimeSync::new(count, refresh, reading, record, clock.monotonic(), entropy);
        let service = Self {
            clock,
            store,
            transport,
            sandbox,
            sink,
            rtc,
            sync,
            record,
            rtc_retry,
            servers,
            source,
            exhaustion_reported: false,
        };
        service.audit_startup();
        service
    }

    /// The single monotonic instant the reactor should wake at, or `None` when
    /// there is nothing left to wait for.
    ///
    /// Folds the RTC ladder in, so a service with no server configured still
    /// wakes to re-read a clock chip whose driver was not up yet.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Duration64> {
        let rtc_at = self.rtc_retry.map(|rung| Duration64::from_nanos(rung.at));
        match (self.sync.next_deadline(), rtc_at) {
            (Some(engine), Some(rtc)) => Some(engine.min(rtc)),
            (only, None) | (None, only) => only,
        }
    }

    /// Whether every server in use has refused further queries.
    #[must_use]
    pub fn is_exhausted(&self) -> bool {
        self.sync.is_exhausted()
    }

    /// The persisted record as it now stands.
    #[must_use]
    pub const fn record(&self) -> SyncRecord {
        self.record
    }

    /// Advance the engine at monotonic `now`, sending a request if one is due.
    ///
    /// `entropy` must be a **fresh** CSPRNG word on every call: it becomes the
    /// nonce that authenticates the reply.
    pub fn poll(&mut self, now: Duration64, entropy: u64) -> Step {
        self.poll_rtc(now);
        let Some(Query { server, packet }) = self.sync.poll(now, entropy) else {
            if self.sync.is_exhausted() {
                if !self.exhaustion_reported {
                    self.exhaustion_reported = true;
                    self.record_event(
                        events::SERVERS_EXHAUSTED,
                        Level::Warn,
                        "timed: every time server in use has refused further queries",
                        &[],
                    );
                }
            } else {
                self.exhaustion_reported = false;
            }
            return Step::Idle;
        };
        // A query is due, so the engine is not exhausted: a later exhaustion
        // is a fresh transition and is reported again.
        self.exhaustion_reported = false;
        match self.transport.send(server, &packet) {
            Ok(()) => Step::Queried(server),
            Err(err) => {
                // No packet left the machine. The engine's response timeout
                // ends the transaction and its backoff schedules the retry, so
                // a network that is not up yet costs one silent attempt per
                // backoff step rather than a spin.
                self.record_event(
                    events::QUERY_NOT_SENT,
                    Level::Info,
                    "timed: no request could be sent to the time server",
                    &[
                        self.server_field(server),
                        Field {
                            key: "error",
                            value: FieldValue::Error(err),
                        },
                    ],
                );
                Step::NotSent(err)
            }
        }
    }

    /// Feed a received datagram to the engine at monotonic `now`.
    ///
    /// The evaluation runs in the sandbox worker; only its verdict is applied
    /// here. A datagram that is not the outstanding transaction's reply leaves
    /// every bit of state alone, so an injected flood cannot cancel the real
    /// answer.
    pub fn on_datagram(&mut self, now: Duration64, bytes: &[u8]) -> Step {
        let Some(txn) = self.sync.outstanding() else {
            return Step::Idle;
        };
        let verdict = match evaluate_datagram(&mut self.sandbox, &txn, now, bytes) {
            Ok(verdict) => verdict,
            Err(failure) => {
                self.record_event(
                    events::EVALUATION_FAILED,
                    Level::Warn,
                    "timed: the sandboxed response evaluation produced no usable verdict",
                    &[Field {
                        key: "failure",
                        value: FieldValue::Str(failure_name(failure)),
                    }],
                );
                return Step::NoSample;
            }
        };
        let reading = self
            .clock
            .wall()
            .unwrap_or_else(|_| WallClockReading::new(Time64::from_secs(0), WallTimeState::Unset));
        match self.sync.on_reply(now, reading, verdict) {
            SyncEvent::Apply(update) => self.apply(update),
            SyncEvent::Refused(reason) => {
                self.record_event(
                    events::SAMPLE_REFUSED,
                    Level::Info,
                    "timed: the server answered with a sample that could not be used",
                    &[Field {
                        key: "reason",
                        value: FieldValue::Str(reject_name(reason)),
                    }],
                );
                Step::NoSample
            }
            SyncEvent::ServerRetired { server, code } => {
                self.record_event(
                    events::SERVER_RETIRED,
                    Level::Warn,
                    "timed: the time server refused further queries and has been retired",
                    &[
                        self.server_field(server),
                        Field {
                            key: "kiss",
                            value: FieldValue::Str(kiss_name(code)),
                        },
                    ],
                );
                Step::NoSample
            }
            SyncEvent::RateLimited { server } => {
                self.record_event(
                    events::SERVER_RATE_LIMITED,
                    Level::Info,
                    "timed: the time server asked for a slower rate; its interval is widened",
                    &[self.server_field(server)],
                );
                Step::NoSample
            }
            SyncEvent::Ignored => Step::Idle,
        }
    }

    /// Apply a validated clock update and persist the record.
    fn apply(&mut self, update: ClockUpdate) -> Step {
        if let Err(err) = self.clock.set_wall(update.wall, update.state) {
            self.record_event(
                events::CLOCK_SET_REFUSED,
                Level::Error,
                "timed: the kernel refused to set the clock from a validated sample",
                &[Field {
                    key: "error",
                    value: FieldValue::Error(err),
                }],
            );
            return Step::ClockRefused(err);
        }
        self.record_event(
            events::CLOCK_SET,
            Level::Info,
            "timed: the clock was set from a validated network sample",
            &[
                Field {
                    key: "wall_secs",
                    value: FieldValue::SignedInt(update.wall.secs()),
                },
                Field {
                    key: "change",
                    value: FieldValue::Str(if update.stepped { "step" } else { "refinement" }),
                },
                Field {
                    key: "correction_secs",
                    value: match update.correction {
                        Some(delta) => FieldValue::SignedInt(delta.secs()),
                        None => FieldValue::Null,
                    },
                },
                Field {
                    key: "round_trip_nanos",
                    value: FieldValue::UnsignedInt(update.round_trip.saturating_total_nanos()),
                },
                Field {
                    key: "stratum",
                    value: FieldValue::UnsignedInt(u64::from(update.stratum)),
                },
            ],
        );
        // The record is what lets a future boot tell a short power-off from a
        // long one. Losing it costs that distinction, never the clock.
        self.record = self.record.synced_at(update.wall);
        if let Err(err) = self.store.write(&self.record.to_bytes()) {
            self.record_event(
                events::RECORD_NOT_WRITTEN,
                Level::Warn,
                "timed: the clock was set but the last-seen record could not be written",
                &[Field {
                    key: "error",
                    value: FieldValue::Error(err),
                }],
            );
        }
        // Write the validated instant back to the board's clock chip, so a
        // machine that syncs once then boots offline still starts from a good
        // time. A refusal costs the next boot its head start and nothing
        // else, so it is reported and the sync stands.
        match self.rtc.set(update.wall) {
            Ok(()) => self.record_event(
                events::RTC_WRITEBACK,
                Level::Info,
                "timed: the validated instant was written back to the real-time clock",
                &[Field {
                    key: "wall_secs",
                    value: FieldValue::SignedInt(update.wall.secs()),
                }],
            ),
            Err(err) => self.record_event(
                events::RTC_WRITEBACK_REFUSED,
                Level::Info,
                "timed: the real-time clock did not accept the validated instant",
                &[Field {
                    key: "error",
                    value: FieldValue::Error(err),
                }],
            ),
        }
        Step::ClockSet(update)
    }

    /// Climb the RTC ladder if a rung is due, and stop climbing once the chip
    /// has answered or the ladder is spent.
    ///
    /// A chip that answers *late* improves the clock but does not re-derive
    /// the sync decision: the engine's rotation and backoff are already in
    /// flight, and rebuilding them would forget which servers had refused.
    /// The cost is at most one early query on a machine whose RTC bound after
    /// the service started.
    fn poll_rtc(&mut self, now: Duration64) {
        let Some(rung) = self.rtc_retry.as_mut() else {
            return;
        };
        if now.saturating_total_nanos() < rung.at {
            return;
        }
        if seed_clock_from_rtc(&mut self.rtc, &self.clock, &self.sink) {
            self.rtc_retry = None;
            return;
        }
        if !self
            .rtc_retry
            .as_mut()
            .is_some_and(|rung| rung.advance(now.saturating_total_nanos()))
        {
            emit(
                &self.sink,
                events::RTC_UNAVAILABLE,
                Level::Info,
                "timed: no real-time clock answered within the start-up window",
                &[],
            );
            self.rtc_retry = None;
        }
    }

    /// Record what the service decided about the clock at startup, and where
    /// the servers it will ask came from.
    ///
    /// The set is never empty — the selection's lowest tier is the built-in
    /// fallback — so this is the one start-up record, and `source` is what
    /// tells an operator whether the machine is asking their server, their
    /// network's, or the public pool.
    fn audit_startup(&self) {
        self.record_event(
            events::SERVICE_READY,
            Level::Info,
            "timed: running",
            &[
                Field {
                    key: "urgency",
                    value: match self.sync.urgency() {
                        Some(reason) => FieldValue::Str(reason_name(reason)),
                        None => FieldValue::Str("refresh"),
                    },
                },
                Field {
                    key: "source",
                    value: FieldValue::Str(self.source.as_str()),
                },
                Field {
                    key: "servers",
                    value: FieldValue::UnsignedInt(u64::try_from(self.servers.len()).unwrap_or(0)),
                },
            ],
        );
    }

    /// The audit field naming a server by index.
    ///
    /// An index the list does not hold cannot come from the engine (its
    /// rotation is bounded by the count it was built with), so the field
    /// states the index rather than inventing a name.
    fn server_field(&self, index: u8) -> Field<'_> {
        Field {
            key: "server",
            value: match self.servers.get(usize::from(index)) {
                Some(server) => FieldValue::Str(&server.name),
                None => FieldValue::UnsignedInt(u64::from(index)),
            },
        }
    }

    /// Emit one audit record.
    fn record_event(
        &self,
        id: tairix_log::EventId,
        level: Level,
        message: &str,
        fields: &[Field<'_>],
    ) {
        emit(&self.sink, id, level, message, fields);
    }
}

/// Emit one audit record through `sink`.
///
/// A free function because the RTC seeding runs before the service value
/// exists, and one emitter beats two.
fn emit<S: Sink>(
    sink: &S,
    id: tairix_log::EventId,
    level: Level,
    message: &str,
    fields: &[Field<'_>],
) {
    tairix_log::log(
        sink,
        &Event {
            level,
            id,
            message,
            fields,
        },
    );
}

/// Read the board's clock chip and, if it vouched for an instant, set the
/// wall clock from it as `Firmware`.
///
/// Returns whether the question is **settled** — the chip answered, whether
/// or not it had a time to give — so a caller knows whether to keep climbing
/// its ladder. The kernel enforces the provenance ladder, so a `Firmware`
/// write that arrives after a network sync is refused there rather than
/// trusted to be polite here; that refusal is reported, not retried.
fn seed_clock_from_rtc<C: Clock, S: Sink, K: RtcSource>(rtc: &mut K, clock: &C, sink: &S) -> bool {
    // No driver serves the endpoint yet, or the read was refused. Both look
    // the same from here, so the ladder — not a guess — bounds it.
    let Ok(reading) = rtc.read() else {
        return false;
    };
    let Some(time) = reading.time else {
        emit(
            sink,
            events::RTC_NO_READING,
            Level::Info,
            "timed: the real-time clock has no instant it can vouch for",
            &[Field {
                key: "oscillator_stopped",
                value: FieldValue::Bool(reading.status.oscillator_stopped),
            }],
        );
        // The chip answered; asking it again would get the same answer.
        return true;
    };
    match clock.set_wall(time, WallTimeState::Firmware) {
        Ok(()) => emit(
            sink,
            events::RTC_CLOCK_SET,
            Level::Info,
            "timed: the clock was set from the board's real-time clock",
            &[
                Field {
                    key: "wall_secs",
                    value: FieldValue::SignedInt(time.secs()),
                },
                Field {
                    key: "battery_backed",
                    value: FieldValue::Bool(reading.status.battery_backed),
                },
            ],
        ),
        Err(err) => emit(
            sink,
            events::RTC_CLOCK_SET,
            Level::Info,
            "timed: the kernel did not accept the real-time clock's reading",
            &[Field {
                key: "error",
                value: FieldValue::Error(err),
            }],
        ),
    }
    true
}

/// Read the persisted record, resolving every failure to
/// [`SyncRecord::EMPTY`].
fn read_record<R: RecordStore>(store: &R) -> SyncRecord {
    match store.read() {
        Ok(Some(bytes)) => SyncRecord::from_bytes(&bytes),
        _ => SyncRecord::EMPTY,
    }
}

/// The stable audit spelling of a start-up urgency.
const fn reason_name(reason: SyncReason) -> &'static str {
    match reason {
        SyncReason::ClockUnset => "clock_unset",
        SyncReason::Implausible => "implausible",
        SyncReason::WentBackwards => "went_backwards",
        SyncReason::StaleBoot => "stale_boot",
    }
}

/// The stable audit spelling of a rejected reply's reason.
const fn reject_name(reason: tairix_net::ntp::RejectReason) -> &'static str {
    use tairix_net::ntp::RejectReason as R;
    match reason {
        R::NotServerMode => "not_server_mode",
        R::UnsupportedVersion => "unsupported_version",
        R::ServerUnsynchronised => "server_unsynchronised",
        R::StratumUnusable => "stratum_unusable",
        R::UnspecifiedTimestamp => "unspecified_timestamp",
        R::RootDistanceTooLarge => "root_distance_too_large",
        R::InconsistentTimestamps => "inconsistent_timestamps",
        R::RoundTripTooLong => "round_trip_too_long",
        R::ImplausibleTime => "implausible_time",
        R::UnusableKiss => "unusable_kiss",
    }
}

/// The stable audit spelling of a Kiss-o'-Death code.
const fn kiss_name(code: tairix_net::ntp::KissCode) -> &'static str {
    use tairix_net::ntp::KissCode as K;
    match code {
        K::Rate => "rate",
        K::Deny => "deny",
        K::Restrict => "restrict",
        K::Other(_) => "other",
    }
}

/// The stable audit spelling of a containment-path failure.
const fn failure_name(failure: tairix_sandbox::timesync::TimeSyncFailure) -> &'static str {
    use tairix_sandbox::timesync::TimeSyncFailure as F;
    match failure {
        F::Sandbox(_) => "sandbox",
        F::ReplyMalformed => "reply_malformed",
        F::ReplyRefused => "reply_refused",
    }
}

/// First delay before re-reading the configuration store, in nanoseconds.
///
/// `timed` is a boot-floor service and starts *before* the encrypted root
/// holding the store is mounted, so its first read normally finds nothing.
/// There is no userland event for "the root is mounted", so the store is
/// re-read on this bounded, doubling schedule folded into the reactor's own
/// deadline — the tickless fallback, never a spin, and never a wait on the
/// start-up path, which would hold the boot up behind a service nothing else
/// is waiting for.
pub const CONFIG_RETRY_BASE_NANOS: u64 = 4_000_000_000;

/// How many rungs the configuration ladder climbs before giving up.
///
/// Eight doublings from four seconds span about seventeen minutes: far longer
/// than any unlock takes, and finite, so a machine that genuinely has no
/// server configured stops reading rather than re-reading a file for the rest
/// of the boot. Configuring a server later means restarting the service.
pub const CONFIG_RETRY_ATTEMPTS: u32 = 8;

/// First delay before re-reading the board's real-time clock, in nanoseconds.
///
/// The RTC is served by an autoloaded user-space driver, so on a boot-floor
/// service the endpoint is usually not bound yet at start-up. There is no
/// userland event for "that driver bound", so the read climbs the same
/// bounded ladder the configuration read does — a one-shot timer folded into
/// the reactor's own deadline, never a spin.
pub const RTC_RETRY_BASE_NANOS: u64 = 1_000_000_000;

/// How many rungs the RTC ladder climbs before giving up.
///
/// Six doublings from one second span about a minute. Driver autoload runs as
/// soon as the hardware tree is published, far earlier than the encrypted
/// root is mounted, so this is generous; the point of the bound is that a
/// board with no clock chip at all — a Raspberry Pi 3/4 — stops asking
/// instead of waking for the rest of the boot.
pub const RTC_RETRY_ATTEMPTS: u32 = 6;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
