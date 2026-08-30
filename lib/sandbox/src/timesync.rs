//! The NTP response-evaluation service: the decode a clock-setting service
//! must not run in its own address space.
//!
//! `timed` holds `CAP_TIME_SET`. Setting the machine clock arbitrarily
//! invalidates certificate lifetimes, reorders audit reasoning, and moves
//! capability expiry, so the process holding that capability never parses an
//! attacker-controlled packet: [`TimeSyncService`] runs
//! [`tairix_net::ntp::evaluate`] *inside* the sandboxed worker and only its
//! verdict crosses back (`plans/TIMESYNC.md` §4).
//!
//! [`evaluate_datagram`] is the caller's side, and it trusts the worker with
//! nothing:
//!
//! * The **nonce echo is the parent's own gate**, checked before the worker
//!   is involved at all. The origin timestamp lives at a fixed offset in a
//!   fixed-length header, so reading it is a field extraction, not a parse —
//!   and a spoofed flood is then dropped without a round trip, which is what
//!   keeps it from becoming a denial of service against the real reply.
//! * Only the first [`tairix_net::ntp::PACKET_LEN`] bytes are copied in. A
//!   longer datagram's tail is what the codec ignores anyway, and copying a
//!   fixed-length buffer is not parsing.
//! * A returned sample is **re-validated** against the plausibility window,
//!   the round-trip ceiling, and the usable stratum range. The worker is
//!   hostile the moment it has touched a byte, so a verdict that would move
//!   the clock somewhere the engine's own rules forbid is refused here.

use alloc::vec::Vec;

use tairix_abi::is_plausible_wall_time;
use tairix_abi::time::{Duration64, Time64};
use tairix_net::ntp::{
    evaluate, KissCode, NtpTimestamp, RejectReason, Reply, Sample, Transaction, MAX_ROUND_TRIP,
    PACKET_LEN,
};

use crate::host::{Launcher, ParserSandbox, SandboxError};
use crate::wire::{Reader, WireError, Writer};
use crate::worker::Service;

/// Why an evaluation could not be obtained.
///
/// Distinct from [`Reply`]: these are failures of the *containment path*,
/// never a judgement about the datagram. A caller treats any of them as
/// "this datagram produced no verdict" and lets the engine's own response
/// timeout run, rather than inventing a sample.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TimeSyncFailure {
    /// The sandbox itself failed: the worker crashed, could not be launched,
    /// or the request exceeded the frame bound.
    Sandbox(SandboxError),
    /// The worker's reply violated the reply grammar, so it cannot be
    /// believed at all (fail closed).
    ReplyMalformed,
    /// The worker returned a sample the caller's own re-validation refused —
    /// an implausible instant, an over-long round trip, or an unusable
    /// stratum. The worker is compromised or broken; nothing is applied.
    ReplyRefused,
}

/// Request opcode.
const OP_EVALUATE: u8 = 1;

/// Reply tags.
const REPLY_ERROR: u8 = 0;
const REPLY_SAMPLE: u8 = 1;
const REPLY_KISS: u8 = 2;
const REPLY_REJECTED: u8 = 3;
const REPLY_UNSOLICITED: u8 = 4;

/// Lowest stratum that is not a Kiss-o'-Death, and the first unusable one
/// (RFC 5905 §7.3): a sample outside `1..16` claims no reference clock.
const MIN_STRATUM: u8 = 1;

/// First stratum value meaning "unsynchronised" (RFC 5905 §7.3).
const MAX_STRATUM_EXCLUSIVE: u8 = 16;

/// Offset of a reply's origin timestamp within the fixed 48-byte header
/// (RFC 5905 §7.3) — the field a reply must echo the request's nonce in.
const ORIGIN_TS_AT: usize = 24;

/// Width of the nonce, which is the whole 64-bit NTP timestamp.
const NONCE_LEN: usize = 8;

/// The service the sandboxed worker runs: the RFC 5905 response evaluation
/// and nothing else.
///
/// Total by construction — a request it cannot decode is a typed error
/// reply, never a failed loop.
#[derive(Debug, Default)]
pub struct TimeSyncService;

impl Service for TimeSyncService {
    fn handle(&mut self, request: &[u8]) -> Vec<u8> {
        let mut w = Writer::new();
        match decode_request(request) {
            Ok((txn, received_at, datagram)) => {
                encode_reply(&mut w, evaluate(&datagram, &txn, received_at));
            }
            Err(()) => w.u8(REPLY_ERROR),
        }
        w.finish()
    }
}

/// Ask the sandboxed worker to evaluate `datagram` as the reply to `txn`,
/// received at monotonic `received_at`.
///
/// A datagram that cannot be this transaction's reply — too short to hold a
/// header, or an origin timestamp that does not echo the nonce — is
/// [`Reply::Unsolicited`] with no worker round trip at all.
///
/// # Errors
///
/// [`TimeSyncFailure`] when the containment path itself failed or the
/// worker's verdict could not be believed. Never a partial verdict.
pub fn evaluate_datagram<L: Launcher, S: tairix_log::Sink>(
    sandbox: &mut ParserSandbox<L, S>,
    txn: &Transaction,
    received_at: Duration64,
    datagram: &[u8],
) -> Result<Reply, TimeSyncFailure> {
    let Some(header) = datagram.get(..PACKET_LEN) else {
        return Ok(Reply::Unsolicited);
    };
    // The parent's own anti-spoof gate: a fixed-offset slice of a
    // fixed-length buffer, so this reads a field rather than parsing one.
    let mut echoed = [0u8; NONCE_LEN];
    echoed.copy_from_slice(&header[ORIGIN_TS_AT..ORIGIN_TS_AT + NONCE_LEN]);
    if u64::from_be_bytes(echoed) != txn.nonce.raw() {
        return Ok(Reply::Unsolicited);
    }

    let mut w = Writer::new();
    w.u8(OP_EVALUATE);
    w.u64(txn.nonce.raw());
    w.bytes(&txn.sent_at.to_le_bytes());
    w.bytes(&received_at.to_le_bytes());
    w.bytes(header);
    let reply = sandbox
        .request(&w.finish())
        .map_err(TimeSyncFailure::Sandbox)?;
    let verdict = decode_reply(&reply)?;
    if let Reply::Sample(sample) = verdict {
        if !believable(&sample) {
            return Err(TimeSyncFailure::ReplyRefused);
        }
    }
    Ok(verdict)
}

/// Whether a worker-returned sample survives the caller's own re-validation.
///
/// The engine applies these rules too; repeating them here is what makes the
/// worker's compromise unable to move the clock.
fn believable(sample: &Sample) -> bool {
    is_plausible_wall_time(sample.true_time)
        && sample.round_trip <= MAX_ROUND_TRIP
        && (MIN_STRATUM..MAX_STRATUM_EXCLUSIVE).contains(&sample.stratum)
}

/// Decode a request payload into the transaction, receive instant, and the
/// datagram bytes to evaluate.
fn decode_request(request: &[u8]) -> Result<(Transaction, Duration64, Vec<u8>), ()> {
    let mut r = Reader::new(request);
    let decode = |_: WireError| ();
    if r.u8().map_err(decode)? != OP_EVALUATE {
        return Err(());
    }
    let nonce = r.u64().map_err(decode)?;
    let sent_at = duration(&mut r)?;
    let received_at = duration(&mut r)?;
    let datagram = r.bytes(PACKET_LEN).map_err(decode)?.to_vec();
    if !r.is_exhausted() {
        return Err(());
    }
    Ok((
        Transaction {
            // The evaluation never reads the server index, so it is not sent:
            // the worker learns nothing about the caller's server table.
            server: 0,
            nonce: NtpTimestamp::from_raw(nonce),
            sent_at,
        },
        received_at,
        datagram,
    ))
}

/// Consume a [`Duration64`] encoded through its own ABI codec.
fn duration(r: &mut Reader<'_>) -> Result<Duration64, ()> {
    let bytes = r.bytes(Duration64::WIRE_LEN).map_err(|_| ())?;
    Duration64::from_bytes(bytes).map_err(|_| ())
}

/// Encode a verdict onto the reply.
fn encode_reply(w: &mut Writer, reply: Reply) {
    match reply {
        Reply::Sample(sample) => {
            w.u8(REPLY_SAMPLE);
            w.bytes(&sample.true_time.to_le_bytes());
            w.bytes(&sample.round_trip.to_le_bytes());
            w.u8(sample.stratum);
        }
        Reply::Kiss(code) => {
            w.u8(REPLY_KISS);
            encode_kiss(w, code);
        }
        Reply::Rejected(reason) => {
            w.u8(REPLY_REJECTED);
            w.u8(reason_to_wire(reason));
        }
        Reply::Unsolicited => w.u8(REPLY_UNSOLICITED),
    }
}

/// Decode a worker reply fail-closed.
fn decode_reply(reply: &[u8]) -> Result<Reply, TimeSyncFailure> {
    let mut r = Reader::new(reply);
    let malformed = |_: WireError| TimeSyncFailure::ReplyMalformed;
    let verdict = match r.u8().map_err(malformed)? {
        REPLY_SAMPLE => {
            let true_time = Time64::from_bytes(r.bytes(Time64::WIRE_LEN).map_err(malformed)?)
                .map_err(|_| TimeSyncFailure::ReplyMalformed)?;
            let round_trip =
                Duration64::from_bytes(r.bytes(Duration64::WIRE_LEN).map_err(malformed)?)
                    .map_err(|_| TimeSyncFailure::ReplyMalformed)?;
            Reply::Sample(Sample {
                true_time,
                round_trip,
                stratum: r.u8().map_err(malformed)?,
            })
        }
        REPLY_KISS => Reply::Kiss(decode_kiss(&mut r)?),
        REPLY_REJECTED => Reply::Rejected(
            reason_from_wire(r.u8().map_err(malformed)?).ok_or(TimeSyncFailure::ReplyMalformed)?,
        ),
        REPLY_UNSOLICITED => Reply::Unsolicited,
        // The error tag and anything unrecognised alike: the worker produced
        // no usable verdict.
        _ => return Err(TimeSyncFailure::ReplyMalformed),
    };
    if !r.is_exhausted() {
        return Err(TimeSyncFailure::ReplyMalformed);
    }
    Ok(verdict)
}

/// Kiss-o'-Death wire tags. `KISS_OTHER` carries the reply's four raw
/// reference-id octets, because the unrecognised codes are exactly the ones a
/// reader needs spelled out to diagnose a server.
const KISS_RATE: u8 = 1;
const KISS_DENY: u8 = 2;
const KISS_RESTRICT: u8 = 3;
const KISS_OTHER: u8 = 4;

/// Number of octets in a stratum-0 reply's reference id (RFC 5905 §7.3).
const REFERENCE_ID_LEN: usize = 4;

fn encode_kiss(w: &mut Writer, code: KissCode) {
    match code {
        KissCode::Rate => w.u8(KISS_RATE),
        KissCode::Deny => w.u8(KISS_DENY),
        KissCode::Restrict => w.u8(KISS_RESTRICT),
        KissCode::Other(id) => {
            w.u8(KISS_OTHER);
            w.bytes(&id);
        }
    }
}

fn decode_kiss(r: &mut Reader<'_>) -> Result<KissCode, TimeSyncFailure> {
    let malformed = |_: WireError| TimeSyncFailure::ReplyMalformed;
    match r.u8().map_err(malformed)? {
        KISS_RATE => Ok(KissCode::Rate),
        KISS_DENY => Ok(KissCode::Deny),
        KISS_RESTRICT => Ok(KissCode::Restrict),
        KISS_OTHER => {
            let mut id = [0u8; REFERENCE_ID_LEN];
            let bytes = r.bytes(REFERENCE_ID_LEN).map_err(malformed)?;
            if bytes.len() != id.len() {
                return Err(TimeSyncFailure::ReplyMalformed);
            }
            id.copy_from_slice(bytes);
            Ok(KissCode::Other(id))
        }
        _ => Err(TimeSyncFailure::ReplyMalformed),
    }
}

/// Rejection-reason wire codes.
const REASON_NOT_SERVER_MODE: u8 = 1;
const REASON_UNSUPPORTED_VERSION: u8 = 2;
const REASON_SERVER_UNSYNCHRONISED: u8 = 3;
const REASON_STRATUM_UNUSABLE: u8 = 4;
const REASON_UNSPECIFIED_TIMESTAMP: u8 = 5;
const REASON_ROOT_DISTANCE: u8 = 6;
const REASON_INCONSISTENT_TIMESTAMPS: u8 = 7;
const REASON_ROUND_TRIP: u8 = 8;
const REASON_IMPLAUSIBLE_TIME: u8 = 9;
const REASON_UNUSABLE_KISS: u8 = 10;

const fn reason_to_wire(reason: RejectReason) -> u8 {
    match reason {
        RejectReason::NotServerMode => REASON_NOT_SERVER_MODE,
        RejectReason::UnsupportedVersion => REASON_UNSUPPORTED_VERSION,
        RejectReason::ServerUnsynchronised => REASON_SERVER_UNSYNCHRONISED,
        RejectReason::StratumUnusable => REASON_STRATUM_UNUSABLE,
        RejectReason::UnspecifiedTimestamp => REASON_UNSPECIFIED_TIMESTAMP,
        RejectReason::RootDistanceTooLarge => REASON_ROOT_DISTANCE,
        RejectReason::InconsistentTimestamps => REASON_INCONSISTENT_TIMESTAMPS,
        RejectReason::RoundTripTooLong => REASON_ROUND_TRIP,
        RejectReason::ImplausibleTime => REASON_IMPLAUSIBLE_TIME,
        RejectReason::UnusableKiss => REASON_UNUSABLE_KISS,
    }
}

const fn reason_from_wire(raw: u8) -> Option<RejectReason> {
    match raw {
        REASON_NOT_SERVER_MODE => Some(RejectReason::NotServerMode),
        REASON_UNSUPPORTED_VERSION => Some(RejectReason::UnsupportedVersion),
        REASON_SERVER_UNSYNCHRONISED => Some(RejectReason::ServerUnsynchronised),
        REASON_STRATUM_UNUSABLE => Some(RejectReason::StratumUnusable),
        REASON_UNSPECIFIED_TIMESTAMP => Some(RejectReason::UnspecifiedTimestamp),
        REASON_ROOT_DISTANCE => Some(RejectReason::RootDistanceTooLarge),
        REASON_INCONSISTENT_TIMESTAMPS => Some(RejectReason::InconsistentTimestamps),
        REASON_ROUND_TRIP => Some(RejectReason::RoundTripTooLong),
        REASON_IMPLAUSIBLE_TIME => Some(RejectReason::ImplausibleTime),
        REASON_UNUSABLE_KISS => Some(RejectReason::UnusableKiss),
        _ => None,
    }
}

#[cfg(test)]
#[path = "timesync_tests.rs"]
mod tests;
