//! `NTPv4` / SNTP **client** protocol engine (RFC 5905, RFC 4330), with the
//! polling discipline RFC 8633 requires of a client.
//!
//! This is the wire half of `plans/TIMESYNC.md`: the 48-byte header codec,
//! every response-validation rule, the sample computation, the Kiss-o'-Death
//! decode, and the transaction/retry/rotation state machine. It sets no clock
//! and owns no socket — `lib/timesync` decides *whether* to sync and performs
//! the I/O, so this module is pure and host-testable like its `dhcp` and `dns`
//! siblings.
//!
//! # No server, no peering
//!
//! Unicast client mode only. An NTP server, broadcast/multicast/manycast
//! modes, symmetric peering, Autokey, NTS, and the full
//! clock-filter/selection/clustering algorithm are deliberately absent; a
//! single validated sample disciplines the clock.
//!
//! # The response is hostile
//!
//! NTP is unauthenticated UDP, so an off-path attacker can inject replies. The
//! defence is the RFC 5905 §8 on-wire check: the request's transmit timestamp
//! is a **CSPRNG 64-bit nonce**, and a reply is only ever considered ours if
//! its origin timestamp equals that nonce exactly. A reply failing that check
//! is [`Reply::Unsolicited`] and leaves the outstanding transaction alone —
//! discarding it must not cancel the transaction, or a flood of wrong-nonce
//! packets would be a denial of service against the real answer.
//!
//! Using a random nonce rather than the real time also means the request
//! leaks nothing about the local clock, and it is why the round trip is
//! measured on the caller's **monotonic** clock rather than derived from the
//! packet: the local send/receive legs never enter the wire at all.
//!
//! # Politeness is the engine's job
//!
//! A client that hammers a public server is a defect, so the cadence controls
//! live here rather than in a caller that might get them wrong: a hard
//! [`MIN_POLL`] floor, one request in flight at a time, rotation across the
//! configured servers, exponential backoff with caller-supplied jitter, and
//! Kiss-o'-Death obeyed ([`KissCode::Rate`] widens this server's interval,
//! [`KissCode::Deny`] / [`KissCode::Restrict`] retire it).

use tairix_abi::time::{Duration64, Time64, NANOS_PER_SEC};
use tairix_abi::{is_plausible_wall_time, RELEASE_EPOCH_SECS};

use crate::timeutil::{from_nanos, nanos, NEVER};

/// The UDP port NTP is served on (RFC 5905 §7.2).
pub const PORT: u16 = 123;

/// Encoded length of the NTP header every client request and server reply
/// begins with (RFC 5905 §7.3).
pub const PACKET_LEN: usize = 48;

/// Seconds from the NTP epoch (1900-01-01) to the Unix epoch (1970-01-01).
const NTP_UNIX_DELTA_SECS: i64 = 2_208_988_800;

/// Span of one NTP era: the 32-bit seconds field wraps every 2^32 seconds
/// (about 136 years), so an era must be chosen to place a timestamp.
const NTP_ERA_SECS: i64 = 1 << 32;

/// Largest whole-second span a 32.32 fixed-point value is decoded from.
///
/// Every span this engine converts (a server's own processing delay, a root
/// delay or dispersion) is sub-second in practice; the bound keeps the
/// nanosecond widening far from overflow and rejects a nonsense field before
/// it is arithmetic. A fixed validation bound, not a capacity.
const MAX_FIXED_SECS: u64 = 1_000_000;

/// Floor on the **steady-state** cadence between successful queries
/// (RFC 8633 §3.2, RFC 4330 §10).
///
/// Configuration cannot lower it: a client polling a responsive server faster
/// than this is abusing it, whatever its operator asked for. It bounds the
/// steady state rather than every individual packet — a failed transaction may
/// be retried sooner under the bounded [`backoff`], which is what both RFCs
/// permit for start-up and recovery, and that retry still rotates to the next
/// server and still grows exponentially.
pub const MIN_POLL: Duration64 = Duration64::from_secs(64);

/// How long a request waits for its reply before the transaction fails.
pub const RESPONSE_TIMEOUT: Duration64 = Duration64::from_secs(5);

/// First step of the failure backoff; each further consecutive failure
/// doubles it up to [`BACKOFF_CAP`].
pub const BACKOFF_BASE: Duration64 = Duration64::from_secs(8);

/// Ceiling on the failure backoff, so a long outage settles at a steady slow
/// retry rather than growing without bound.
pub const BACKOFF_CAP: Duration64 = Duration64::from_secs(1024);

/// Widest round trip a sample may have measured and still be usable: beyond
/// this the half-round-trip estimate of the server's transmit instant is too
/// coarse to be worth applying.
pub const MAX_ROUND_TRIP: Duration64 = Duration64::from_secs(3);

/// Ceiling on a server's advertised root delay plus root dispersion — its own
/// claimed distance from a reference clock (RFC 5905 §11.2.1 `MAXDIST`).
pub const MAX_ROOT_DISTANCE: Duration64 = Duration64::from_secs(1);

/// Most servers a client may be configured with.
///
/// A fixed validation bound on configuration input, not a capacity that
/// should scale with the machine: a client needs a handful of servers to be
/// robust, and a longer list would only spread queries thinner while enlarging
/// the state a hostile server set can occupy.
pub const MAX_SERVERS: usize = 8;

/// Leap-second warning a server advertises (RFC 5905 §7.3).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum LeapIndicator {
    /// No warning.
    NoWarning,
    /// The last minute of the day has 61 seconds.
    InsertSecond,
    /// The last minute of the day has 59 seconds.
    DeleteSecond,
    /// The server's own clock is not synchronised, so its time is unusable.
    Unsynchronised,
}

impl LeapIndicator {
    /// Decode the two-bit field. Total: every bit pattern is a variant.
    #[must_use]
    const fn from_bits(bits: u8) -> Self {
        match bits & 0b11 {
            0 => Self::NoWarning,
            1 => Self::InsertSecond,
            2 => Self::DeleteSecond,
            _ => Self::Unsynchronised,
        }
    }
}

/// The association mode a packet claims (RFC 5905 §7.3).
///
/// Only [`Mode::Server`] is accepted on a reply: a client that honoured
/// `Broadcast` would take its time from anyone on the link.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Mode {
    /// Mode 3 — a client request.
    Client,
    /// Mode 4 — a server reply, the only mode this engine accepts.
    Server,
    /// Any other mode: reserved, symmetric active/passive, broadcast, or
    /// private. All are refused on a reply.
    Other(u8),
}

impl Mode {
    #[must_use]
    const fn from_bits(bits: u8) -> Self {
        match bits & 0b111 {
            3 => Self::Client,
            4 => Self::Server,
            other => Self::Other(other),
        }
    }

    #[must_use]
    const fn as_bits(self) -> u8 {
        match self {
            Self::Client => 3,
            Self::Server => 4,
            Self::Other(bits) => bits & 0b111,
        }
    }
}

/// A Kiss-o'-Death code: the four-octet reference id of a stratum-0 reply, by
/// which a server tells a client to back off or go away (RFC 5905 §7.4).
///
/// Obeying these is what makes the client a good citizen; ignoring them is the
/// abuse the polling discipline exists to prevent.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum KissCode {
    /// `RATE` — polling too fast. Widen this server's interval.
    Rate,
    /// `DENY` — do not query this server again.
    Deny,
    /// `RSTR` — access denied by policy. Do not query this server again.
    Restrict,
    /// Any other kiss code, including the `INIT`/`STEP` startup codes. The
    /// sample is unusable but the server is not retired.
    Other([u8; 4]),
}

impl KissCode {
    /// Decode a stratum-0 reply's reference id.
    #[must_use]
    const fn from_reference_id(id: [u8; 4]) -> Self {
        match &id {
            b"RATE" => Self::Rate,
            b"DENY" => Self::Deny,
            b"RSTR" => Self::Restrict,
            _ => Self::Other(id),
        }
    }

    /// Whether this code means the server must not be queried again.
    #[must_use]
    pub const fn retires_server(self) -> bool {
        matches!(self, Self::Deny | Self::Restrict)
    }
}

/// An NTP timestamp: 32 bits of seconds since 1900 and 32 bits of fraction
/// (RFC 5905 §6).
///
/// Held as the two fields the wire format actually defines rather than as one
/// 64-bit word, so every conversion is a widening and nothing is ever
/// truncated back down.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct NtpTimestamp {
    secs: u32,
    frac: u32,
}

impl NtpTimestamp {
    /// The all-zero timestamp, which the protocol uses to mean "unspecified"
    /// and which is therefore never a usable reading.
    pub const ZERO: Self = Self { secs: 0, frac: 0 };

    /// Wrap a raw 64-bit value — a CSPRNG nonce for a request's transmit
    /// field, or a field decoded from the wire.
    #[must_use]
    pub fn from_raw(raw: u64) -> Self {
        Self::from_bytes(raw.to_be_bytes())
    }

    /// The raw 64-bit value, for the exact-equality nonce check.
    #[must_use]
    pub fn raw(self) -> u64 {
        (u64::from(self.secs) << 32) | u64::from(self.frac)
    }

    /// Whether this is the unspecified timestamp.
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.secs == 0 && self.frac == 0
    }

    /// The instant this timestamp denotes, placing it in the NTP era nearest
    /// [`RELEASE_EPOCH_SECS`].
    ///
    /// The era must be chosen from outside the protocol, which carries only
    /// 32 bits of seconds. Anchoring on the release epoch rather than on the
    /// local clock is deliberate: the local clock may be the wildly-wrong
    /// thing being corrected, so using it to place the era could pick an era
    /// 136 years out. The true time always lies within the plausibility
    /// window above the release epoch, which is far narrower than one era, so
    /// the nearest-era choice is unambiguous. Callers still check the result
    /// against that window — era selection is arithmetic, not validation.
    #[must_use]
    pub fn to_time64(self) -> Time64 {
        let era0_secs = i64::from(self.secs) - NTP_UNIX_DELTA_SECS;
        let eras = div_round_nearest(RELEASE_EPOCH_SECS - era0_secs, NTP_ERA_SECS);
        let secs = era0_secs.saturating_add(eras.saturating_mul(NTP_ERA_SECS));
        Time64::from_secs(secs).saturating_add(Duration64::from_nanos(self.fraction_nanos()))
    }

    /// The span from `earlier` to `self`, or `None` if `earlier` is later or
    /// the span is implausibly large.
    ///
    /// Both operands come from the *same* reply and are within microseconds of
    /// each other, so they share an era and the raw subtraction is correct
    /// without placing either instant.
    #[must_use]
    pub fn duration_since(self, earlier: Self) -> Option<Duration64> {
        fixed_to_duration(self.raw().checked_sub(earlier.raw())?)
    }

    /// Fraction field widened to nanoseconds, always below one second.
    #[must_use]
    fn fraction_nanos(self) -> u64 {
        (u64::from(self.frac) * u64::from(NANOS_PER_SEC)) >> 32
    }

    #[must_use]
    fn from_bytes(bytes: [u8; 8]) -> Self {
        let (secs, frac) = bytes.split_at(4);
        Self {
            secs: u32::from_be_bytes(secs.try_into().unwrap_or([0; 4])),
            frac: u32::from_be_bytes(frac.try_into().unwrap_or([0; 4])),
        }
    }
}

/// Divide `n` by the positive `d`, rounding halves away from zero.
///
/// Written out because the era choice must round symmetrically about zero;
/// truncating division would bias a timestamp just below the anchor into the
/// wrong era.
const fn div_round_nearest(n: i64, d: i64) -> i64 {
    let half = d / 2;
    if n >= 0 {
        (n + half) / d
    } else {
        -((half - n) / d)
    }
}

/// Convert a 32.32 fixed-point span to a [`Duration64`], refusing a value
/// whose whole-second part exceeds [`MAX_FIXED_SECS`].
fn fixed_to_duration(fixed: u64) -> Option<Duration64> {
    let secs = fixed >> 32;
    if secs > MAX_FIXED_SECS {
        return None;
    }
    let frac_nanos = ((fixed & 0xFFFF_FFFF) * u64::from(NANOS_PER_SEC)) >> 32;
    Some(Duration64::from_nanos(
        secs * u64::from(NANOS_PER_SEC) + frac_nanos,
    ))
}

/// Convert a 16.16 fixed-point "short format" span (RFC 5905 §6) — the root
/// delay and root dispersion fields — to a [`Duration64`].
fn short_to_duration(short: u32) -> Duration64 {
    let secs = u64::from(short >> 16);
    let frac_nanos = (u64::from(short & 0xFFFF) * u64::from(NANOS_PER_SEC)) >> 16;
    Duration64::from_nanos(secs * u64::from(NANOS_PER_SEC) + frac_nanos)
}

/// A decoded NTP header.
///
/// Decoding is total and infallible over a long-enough buffer: every field is
/// a fixed-offset integer and every enum decode covers all bit patterns, so
/// there is nothing to fail on beyond the length. Whether the header is
/// *acceptable* is [`evaluate`]'s job, kept separate so the codec cannot be
/// blamed for policy.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Header {
    /// Leap-second warning.
    pub leap: LeapIndicator,
    /// Protocol version (3 or 4 in practice).
    pub version: u8,
    /// Association mode.
    pub mode: Mode,
    /// Distance from a reference clock: 0 is a Kiss-o'-Death, 1 a reference
    /// clock, 2..=15 a secondary server, 16 and above unsynchronised.
    pub stratum: u8,
    /// The server's own log2 poll interval.
    pub poll: i8,
    /// The server's log2 clock precision.
    pub precision: i8,
    /// Total round-trip delay to the reference clock.
    pub root_delay: Duration64,
    /// Maximum error relative to the reference clock.
    pub root_dispersion: Duration64,
    /// Reference identifier, or the kiss code when `stratum` is 0.
    pub reference_id: [u8; 4],
    /// When the server last set its own clock.
    pub reference_ts: NtpTimestamp,
    /// The request's transmit timestamp, echoed. This is the nonce check.
    pub origin_ts: NtpTimestamp,
    /// When the server received the request.
    pub receive_ts: NtpTimestamp,
    /// When the server sent the reply.
    pub transmit_ts: NtpTimestamp,
}

impl Header {
    /// Decode a header from the first [`PACKET_LEN`] bytes of `bytes`.
    ///
    /// A longer buffer is accepted and its tail ignored: a reply may legally
    /// carry extension fields or a MAC, which this client neither uses nor
    /// needs to reject.
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let head: &[u8; PACKET_LEN] = bytes.get(..PACKET_LEN)?.try_into().ok()?;
        Some(Self {
            leap: LeapIndicator::from_bits(head[0] >> 6),
            version: (head[0] >> 3) & 0b111,
            mode: Mode::from_bits(head[0]),
            stratum: head[1],
            poll: i8::from_be_bytes([head[2]]),
            precision: i8::from_be_bytes([head[3]]),
            root_delay: short_to_duration(u32::from_be_bytes([head[4], head[5], head[6], head[7]])),
            root_dispersion: short_to_duration(u32::from_be_bytes([
                head[8], head[9], head[10], head[11],
            ])),
            reference_id: [head[12], head[13], head[14], head[15]],
            reference_ts: NtpTimestamp::from_bytes([
                head[16], head[17], head[18], head[19], head[20], head[21], head[22], head[23],
            ]),
            origin_ts: NtpTimestamp::from_bytes([
                head[24], head[25], head[26], head[27], head[28], head[29], head[30], head[31],
            ]),
            receive_ts: NtpTimestamp::from_bytes([
                head[32], head[33], head[34], head[35], head[36], head[37], head[38], head[39],
            ]),
            transmit_ts: NtpTimestamp::from_bytes([
                head[40], head[41], head[42], head[43], head[44], head[45], head[46], head[47],
            ]),
        })
    }

    /// The kiss code this reply carries, or `None` if it is not a
    /// Kiss-o'-Death.
    #[must_use]
    pub const fn kiss(&self) -> Option<KissCode> {
        if self.stratum == 0 {
            Some(KissCode::from_reference_id(self.reference_id))
        } else {
            None
        }
    }
}

/// Encode a client request whose transmit timestamp is `nonce`.
///
/// Every other field is zero: a client has nothing truthful to say about
/// stratum or root distance, and leaving the timestamps unspecified is both
/// what RFC 4330 §5 prescribes for a simple client and what keeps the local
/// clock off the wire.
#[must_use]
pub fn client_request(nonce: NtpTimestamp) -> [u8; PACKET_LEN] {
    let mut packet = [0u8; PACKET_LEN];
    // Leap 0 (no warning), version 4, mode 3 (client).
    packet[0] = (4u8 << 3) | Mode::Client.as_bits();
    packet[40..48].copy_from_slice(&nonce.raw().to_be_bytes());
    packet
}

/// Why a reply that *was* ours could not be used.
///
/// Distinct from [`Reply::Unsolicited`]: these all end the transaction,
/// because the server we asked did answer — badly.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RejectReason {
    /// Not mode 4. A client never takes time from a broadcast or a peer.
    NotServerMode,
    /// Protocol version below 3.
    UnsupportedVersion,
    /// The server says its own clock is not synchronised.
    ServerUnsynchronised,
    /// Stratum 16 or above: unsynchronised.
    StratumUnusable,
    /// A timestamp the computation needs was left unspecified.
    UnspecifiedTimestamp,
    /// The server's claimed distance from a reference clock exceeds
    /// [`MAX_ROOT_DISTANCE`].
    RootDistanceTooLarge,
    /// The timestamps contradict each other — the reply was sent before it was
    /// received, or the server's own processing outlasted the whole round
    /// trip.
    InconsistentTimestamps,
    /// The measured round trip exceeds [`MAX_ROUND_TRIP`].
    RoundTripTooLong,
    /// The resulting instant falls outside the plausibility window.
    ImplausibleTime,
    /// A Kiss-o'-Death whose code is neither a rate limit nor a refusal (the
    /// `INIT` / `STEP` startup codes and anything unrecognised): no sample,
    /// but no reason to treat the server differently either.
    UnusableKiss,
}

/// A usable time sample derived from a validated reply.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Sample {
    /// The server's estimate of the current instant, advanced by half the
    /// measured round trip to account for the reply's flight time.
    pub true_time: Time64,
    /// The round trip measured on the caller's monotonic clock, with the
    /// server's own processing time removed.
    pub round_trip: Duration64,
    /// The server's stratum, carried for audit.
    pub stratum: u8,
}

/// What arrived on the socket.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Reply {
    /// A validated sample.
    Sample(Sample),
    /// A Kiss-o'-Death. No sample, and the server may need retiring.
    Kiss(KissCode),
    /// Ours, but unusable.
    Rejected(RejectReason),
    /// Not ours: too short to decode, or its origin timestamp does not echo
    /// the nonce. The outstanding transaction is untouched, so an injected
    /// flood cannot cancel the real reply.
    Unsolicited,
}

/// One outstanding request: which server it went to, the nonce that
/// authenticates its reply, and when it was sent on the monotonic clock.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Transaction {
    /// Index into the caller's server table.
    pub server: u8,
    /// The CSPRNG nonce placed in the request's transmit field.
    pub nonce: NtpTimestamp,
    /// Monotonic instant the request was sent.
    pub sent_at: Duration64,
}

/// Validate `bytes` as the reply to `txn`, received at monotonic
/// `received_at`, and derive a [`Sample`].
///
/// The nonce check runs before any other judgement, so a packet that is not
/// ours is classified [`Reply::Unsolicited`] and can never be mistaken for the
/// asked server answering badly.
#[must_use]
pub fn evaluate(bytes: &[u8], txn: &Transaction, received_at: Duration64) -> Reply {
    let Some(header) = Header::decode(bytes) else {
        return Reply::Unsolicited;
    };
    if header.origin_ts.raw() != txn.nonce.raw() {
        return Reply::Unsolicited;
    }
    evaluate_authenticated(&header, txn, received_at)
}

/// The judgement applied once a reply is known to be ours.
fn evaluate_authenticated(header: &Header, txn: &Transaction, received_at: Duration64) -> Reply {
    if header.mode != Mode::Server {
        return Reply::Rejected(RejectReason::NotServerMode);
    }
    if header.version < 3 {
        return Reply::Rejected(RejectReason::UnsupportedVersion);
    }
    if let Some(kiss) = header.kiss() {
        return Reply::Kiss(kiss);
    }
    if header.leap == LeapIndicator::Unsynchronised {
        return Reply::Rejected(RejectReason::ServerUnsynchronised);
    }
    if header.stratum >= 16 {
        return Reply::Rejected(RejectReason::StratumUnusable);
    }
    if header.transmit_ts.is_zero() || header.receive_ts.is_zero() {
        return Reply::Rejected(RejectReason::UnspecifiedTimestamp);
    }
    let root_distance = from_nanos(nanos(header.root_delay) + nanos(header.root_dispersion));
    if root_distance > MAX_ROOT_DISTANCE {
        return Reply::Rejected(RejectReason::RootDistanceTooLarge);
    }

    // The local legs come from the monotonic clock, the server's own
    // processing from the reply: both are spans, so no epoch is involved.
    let Some(local_elapsed) = checked_span(txn.sent_at, received_at) else {
        return Reply::Rejected(RejectReason::InconsistentTimestamps);
    };
    let Some(server_delay) = header.transmit_ts.duration_since(header.receive_ts) else {
        return Reply::Rejected(RejectReason::InconsistentTimestamps);
    };
    if server_delay > local_elapsed {
        return Reply::Rejected(RejectReason::InconsistentTimestamps);
    }
    let round_trip = from_nanos(nanos(local_elapsed) - nanos(server_delay));
    if round_trip > MAX_ROUND_TRIP {
        return Reply::Rejected(RejectReason::RoundTripTooLong);
    }

    let true_time = header
        .transmit_ts
        .to_time64()
        .saturating_add(from_nanos(nanos(round_trip) / 2));
    if !is_plausible_wall_time(true_time) {
        return Reply::Rejected(RejectReason::ImplausibleTime);
    }
    Reply::Sample(Sample {
        true_time,
        round_trip,
        stratum: header.stratum,
    })
}

/// The span from `start` to `end` on the monotonic clock, or `None` if `end`
/// precedes `start` — which monotonic time never produces, and which would
/// mean the caller mixed clocks.
fn checked_span(start: Duration64, end: Duration64) -> Option<Duration64> {
    let (start, end) = (nanos(start), nanos(end));
    end.checked_sub(start).map(from_nanos)
}

/// The backoff after `failures` consecutive failed transactions: [`BACKOFF_BASE`]
/// doubled per failure, clamped to [`BACKOFF_CAP`].
#[must_use]
pub fn backoff(failures: u32) -> Duration64 {
    let base = nanos(BACKOFF_BASE);
    let cap = nanos(BACKOFF_CAP);
    let shift = failures.min(u32::BITS - 1);
    let scaled = base.checked_shl(shift).unwrap_or(cap);
    from_nanos(scaled.min(cap))
}

/// Widest span the jitter scaling accepts, in nanoseconds (about 36 years).
///
/// No scheduling interval can legitimately exceed this, and clamping to it
/// keeps the fixed-point multiply below the `u128` range — arithmetic that
/// would otherwise overflow, and so panic, because overflow checks are on in
/// every profile.
const MAX_JITTER_SPAN_NANOS: u128 = 1 << 60;

/// Spread `span` by up to ±25% using a caller-supplied CSPRNG word.
///
/// Jitter is load-bearing rather than cosmetic: a fleet of machines restored
/// from one image and powered on together would otherwise query the same
/// server at the same instant forever.
#[must_use]
pub fn jitter(span: Duration64, entropy: u64) -> Duration64 {
    let total = nanos(span).min(MAX_JITTER_SPAN_NANOS);
    let quarter = total / 4;
    let spread = quarter * 2;
    if spread == 0 {
        return from_nanos(total);
    }
    // Scale by multiply-shift rather than a modulo: `%` would leave the top of
    // the range unreachable and skew the low values, so the spread would be
    // both narrower and lopsided.
    let offset = (u128::from(entropy) * (spread + 1)) >> u64::BITS;
    from_nanos(total - quarter + offset)
}

/// Per-server state: whether it is retired, when it may next be queried, and
/// how far its Kiss-o'-Death rate limit has been widened.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct ServerState {
    retired: bool,
    not_before: u128,
    rate_limit: u128,
}

impl ServerState {
    const fn new() -> Self {
        Self {
            retired: false,
            not_before: 0,
            rate_limit: 0,
        }
    }
}

/// What the engine is currently doing.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Phase {
    /// Nothing outstanding; the next query is due at this monotonic instant.
    Scheduled(u128),
    /// A request is outstanding until its reply or this deadline.
    Awaiting { txn: Transaction, deadline: u128 },
    /// Every configured server has been retired by a Kiss-o'-Death. Nothing
    /// further will be attempted this boot.
    Exhausted,
}

/// A request the caller should send.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Query {
    /// Index into the caller's server table.
    pub server: u8,
    /// The encoded request.
    pub packet: [u8; PACKET_LEN],
}

/// What [`NtpClient::on_datagram`] concluded.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Outcome {
    /// A usable sample. The caller decides how to apply it.
    Sample(Sample),
    /// Ours but unusable; the transaction has ended and a retry is scheduled.
    Rejected(RejectReason),
    /// The server asked not to be queried again and has been retired.
    ServerRetired {
        /// Index of the retired server.
        server: u8,
        /// The code it sent.
        code: KissCode,
    },
    /// The server asked for a slower rate; its interval has been widened.
    RateLimited {
        /// Index of the rate-limited server.
        server: u8,
    },
    /// Not ours, or nothing was outstanding. No state changed.
    Unsolicited,
}

/// The client transaction engine: one request in flight, rotation across
/// servers, backoff on failure, and the Kiss-o'-Death discipline.
///
/// It is a non-blocking state machine in the shape of
/// [`DhcpClient`](crate::dhcp::DhcpClient) rather than a blocking driver like
/// [`dns::resolve`](crate::dns::resolve), because a time client is a
/// long-lived engine in a service's reactor, not a bounded one-shot call.
/// Each entry point yields at most one action, so nothing is allocated.
#[derive(Clone, Debug)]
pub struct NtpClient {
    servers: [ServerState; MAX_SERVERS],
    server_count: u8,
    cursor: u8,
    phase: Phase,
    failures: u32,
    poll_interval: u128,
}

impl NtpClient {
    /// Create a client for `server_count` servers whose steady-state cadence
    /// is `poll_interval`, with the first query due at `first_query_at`.
    ///
    /// `server_count` is clamped to [`MAX_SERVERS`] and `poll_interval` raised
    /// to [`MIN_POLL`] if the caller asked for less — the floor is not
    /// negotiable. A zero `server_count` yields an exhausted client
    /// ([`Self::is_exhausted`]) that never sends, which is the honest state
    /// for an empty configuration rather than an error the caller must
    /// handle.
    #[must_use]
    pub fn new(server_count: u8, poll_interval: Duration64, first_query_at: Duration64) -> Self {
        let server_count = server_count.min(u8::try_from(MAX_SERVERS).unwrap_or(u8::MAX));
        let phase = if server_count == 0 {
            Phase::Exhausted
        } else {
            Phase::Scheduled(nanos(first_query_at))
        };
        Self {
            servers: [ServerState::new(); MAX_SERVERS],
            server_count,
            cursor: 0,
            phase,
            failures: 0,
            poll_interval: nanos(poll_interval).max(nanos(MIN_POLL)),
        }
    }

    /// The steady-state cadence, after the [`MIN_POLL`] floor was applied.
    #[must_use]
    pub fn poll_interval(&self) -> Duration64 {
        from_nanos(self.poll_interval)
    }

    /// Whether every configured server has been retired.
    #[must_use]
    pub const fn is_exhausted(&self) -> bool {
        matches!(self.phase, Phase::Exhausted)
    }

    /// The transaction currently outstanding, if any.
    #[must_use]
    pub const fn outstanding(&self) -> Option<Transaction> {
        match self.phase {
            Phase::Awaiting { txn, .. } => Some(txn),
            _ => None,
        }
    }

    /// The single monotonic instant the caller should wake at, or `None` when
    /// there is nothing to wait for.
    ///
    /// One folded deadline keeps the service tickless: it arms one timer and
    /// never polls.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Duration64> {
        match self.phase {
            Phase::Scheduled(at) => Some(from_nanos(at)),
            Phase::Awaiting { deadline, .. } => Some(from_nanos(deadline)),
            Phase::Exhausted => None,
        }
    }

    /// Advance the engine at monotonic `now`, returning a request to send if
    /// one is due.
    ///
    /// `entropy` must be a **fresh CSPRNG word** on every call: it becomes the
    /// nonce that authenticates the reply, so a counter, a clock reading, or a
    /// repeated value would hand an off-path attacker a predictable target.
    /// It is supplied rather than drawn here so this engine stays pure and
    /// host-testable, exactly as the DHCP and TCP engines take their random
    /// inputs.
    pub fn poll(&mut self, now: Duration64, entropy: u64) -> Option<Query> {
        let now_ns = nanos(now);
        if let Phase::Awaiting { deadline, .. } = self.phase {
            if now_ns < deadline {
                return None;
            }
            self.fail_transaction(now_ns);
        }
        let Phase::Scheduled(at) = self.phase else {
            return None;
        };
        if now_ns < at {
            return None;
        }
        let server = self.claim_server(now_ns)?;
        let nonce = NtpTimestamp::from_raw(entropy);
        self.phase = Phase::Awaiting {
            txn: Transaction {
                server,
                nonce,
                sent_at: now,
            },
            deadline: now_ns.saturating_add(nanos(RESPONSE_TIMEOUT)),
        };
        Some(Query {
            server,
            packet: client_request(nonce),
        })
    }

    /// Feed a received datagram to the engine at monotonic `now`.
    ///
    /// A datagram that is not the outstanding transaction's reply leaves every
    /// bit of state alone.
    pub fn on_datagram(&mut self, now: Duration64, bytes: &[u8]) -> Outcome {
        let Phase::Awaiting { txn, .. } = self.phase else {
            return Outcome::Unsolicited;
        };
        let now_ns = nanos(now);
        match evaluate(bytes, &txn, now) {
            Reply::Unsolicited => Outcome::Unsolicited,
            Reply::Sample(sample) => {
                self.failures = 0;
                self.schedule_next(now_ns, self.poll_interval);
                Outcome::Sample(sample)
            }
            Reply::Rejected(reason) => {
                self.fail_transaction(now_ns);
                Outcome::Rejected(reason)
            }
            Reply::Kiss(code) => {
                let server = usize::from(txn.server);
                if code.retires_server() {
                    if let Some(state) = self.servers.get_mut(server) {
                        state.retired = true;
                    }
                    self.fail_transaction(now_ns);
                    Outcome::ServerRetired {
                        server: txn.server,
                        code,
                    }
                } else if code == KissCode::Rate {
                    self.widen_rate_limit(server, now_ns);
                    self.fail_transaction(now_ns);
                    Outcome::RateLimited { server: txn.server }
                } else {
                    self.fail_transaction(now_ns);
                    Outcome::Rejected(RejectReason::UnusableKiss)
                }
            }
        }
    }

    /// End the outstanding transaction as a failure and schedule the backoff.
    ///
    /// The rotation cursor is not touched here: claiming a server already
    /// moved it past the one just used, and advancing again would skip a
    /// server per failure — with two configured servers that lands back on the
    /// one that just failed.
    fn fail_transaction(&mut self, now_ns: u128) {
        self.failures = self.failures.saturating_add(1);
        let wait = nanos(backoff(self.failures.saturating_sub(1)));
        self.schedule_next(now_ns, wait);
    }

    /// Widen a server's Kiss-o'-Death rate limit and hold it off accordingly.
    fn widen_rate_limit(&mut self, server: usize, now_ns: u128) {
        let floor = nanos(MIN_POLL);
        let cap = nanos(BACKOFF_CAP);
        if let Some(state) = self.servers.get_mut(server) {
            let widened = if state.rate_limit == 0 {
                floor
            } else {
                state.rate_limit.saturating_mul(2).min(cap)
            };
            state.rate_limit = widened;
            state.not_before = now_ns.saturating_add(widened);
        }
    }

    /// Schedule the next query `wait` from `now_ns`, or give up if every
    /// server is retired.
    ///
    /// The instant is held back to when a server actually becomes available,
    /// so a Kiss-o'-Death hold is waited out in one sleep instead of waking
    /// only to discover every server is still held off.
    fn schedule_next(&mut self, now_ns: u128, wait: u128) {
        let Some(available_at) = self.earliest_available() else {
            self.phase = Phase::Exhausted;
            return;
        };
        self.phase = Phase::Scheduled(now_ns.saturating_add(wait).max(available_at));
    }

    /// The earliest instant any non-retired server may be queried, or `None`
    /// when every server has been retired.
    fn earliest_available(&self) -> Option<u128> {
        self.servers
            .iter()
            .take(usize::from(self.server_count))
            .filter(|state| !state.retired)
            .map(|state| state.not_before)
            .min()
    }

    /// Take the next server that is neither retired nor held off, advancing
    /// the rotation cursor past it.
    ///
    /// When every server is merely held off, the phase is rescheduled to the
    /// earliest instant one becomes available, so a rate limit is waited out
    /// rather than spun on.
    fn claim_server(&mut self, now_ns: u128) -> Option<u8> {
        let count = usize::from(self.server_count);
        let mut earliest = NEVER;
        for step in 0..count {
            let index = (usize::from(self.cursor) + step) % count;
            // `index < server_count <= MAX_SERVERS`, so the slot always
            // exists; breaking rather than returning keeps the fall-through
            // that re-arms the phase, so no past deadline can escape and spin
            // a caller's reactor.
            let Some(state) = self.servers.get(index) else {
                break;
            };
            if state.retired {
                continue;
            }
            if state.not_before > now_ns {
                earliest = earliest.min(state.not_before);
                continue;
            }
            self.cursor = u8::try_from((index + 1) % count).unwrap_or(0);
            return u8::try_from(index).ok();
        }
        if earliest == NEVER {
            self.phase = Phase::Exhausted;
        } else {
            self.phase = Phase::Scheduled(earliest);
        }
        None
    }
}

#[cfg(test)]
#[path = "ntp_tests.rs"]
mod tests;
