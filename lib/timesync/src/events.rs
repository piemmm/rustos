//! Stable [`tairix_log::EventId`] constants emitted by `timed`.
//!
//! Per `lib/log` convention every subsystem owns a 1 000-wide reserved range.
//! The time service occupies `23000..24000` (following the wallpaper chooser's
//! `22000..23000`). Once shipped the numeric values must never be re-used or
//! re-numbered — external audit-log consumers rely on them.

use tairix_log::EventId;

/// Range start (inclusive) reserved for `timed` event identifiers.
///
/// Exposed so audit consumers can filter by subsystem in O(1) instead of
/// matching on individual event identifiers.
pub const TIMED_RANGE_START: u32 = 23_000;

/// Range end (exclusive) reserved for `timed` event identifiers.
pub const TIMED_RANGE_END: u32 = 24_000;

/// The service came up and states what it decided about the clock: the reason
/// the first query is urgent, or that the clock was believed and only the
/// refresh cadence applies.
pub const SERVICE_READY: EventId = EventId(23_001);

/// The service could not come up — the delivery port could not be bound, the
/// wait-set could not be armed, or no socket could be opened. It exits
/// fail-closed rather than half-running, and PID 1 relaunches it.
pub const SERVICE_UNAVAILABLE: EventId = EventId(23_002);

/// No time servers are configured, so the clock is never set from the
/// network. Recorded once at startup: an operator reading the log must be able
/// to tell "nobody configured a server" from "the servers did not answer".
pub const NO_SERVERS_CONFIGURED: EventId = EventId(23_003);

/// The clock was set from a validated network sample. Carries the applied
/// instant, whether it was a step or a refinement, the correction magnitude
/// where there was a previous reading, the measured round trip, and the
/// server's stratum.
pub const CLOCK_SET: EventId = EventId(23_004);

/// A validated sample could not be applied: the kernel refused
/// `wall_time_set`. The service holds `CAP_TIME_SET`, so this is a defect or
/// a revoked grant rather than an expected outcome.
pub const CLOCK_SET_REFUSED: EventId = EventId(23_005);

/// A reply that *was* ours could not be used, with the engine's typed reason.
/// The transaction ends and the backoff schedules the next attempt.
pub const SAMPLE_REFUSED: EventId = EventId(23_006);

/// A server sent a Kiss-o'-Death refusing further queries and has been
/// retired for this boot.
pub const SERVER_RETIRED: EventId = EventId(23_007);

/// A server asked for a slower rate; its poll interval has been widened.
pub const SERVER_RATE_LIMITED: EventId = EventId(23_008);

/// The response evaluation could not be obtained: the sandbox worker failed,
/// or its verdict violated the reply grammar or the caller's own
/// re-validation. Nothing was applied.
pub const EVALUATION_FAILED: EventId = EventId(23_009);

/// A request could not be sent — no address for the configured server, or the
/// socket refused it. The engine's response timeout schedules the retry; no
/// packet left the machine.
pub const QUERY_NOT_SENT: EventId = EventId(23_010);

/// The persisted last-seen record could not be written, so a future boot will
/// not be able to tell a short power-off from a long one. The clock is still
/// set; only the record is lost.
pub const RECORD_NOT_WRITTEN: EventId = EventId(23_011);

/// Every configured server has refused further queries, so nothing further
/// will be attempted this boot.
pub const SERVERS_EXHAUSTED: EventId = EventId(23_012);
