//! Unit tests for the sandboxed NTP response evaluation.
//!
//! The whole parent path runs over the in-process loopback worker, so the
//! containment discipline — the nonce gate, the bounded copy, and the
//! re-validation of a returned sample — is covered without processes.

use super::{
    encode_reply, evaluate_datagram, TimeSyncFailure, TimeSyncService, MAX_STRATUM_EXCLUSIVE,
    ORIGIN_TS_AT, REPLY_SAMPLE,
};
use crate::host::{Launcher, ParserSandbox};
use crate::loopback::LoopbackLauncher;
use crate::proto::Channel;
use crate::wire::Writer;
use crate::worker::Service;
use alloc::vec;
use alloc::vec::Vec;
use tairix_abi::time::{Duration64, Time64};
use tairix_abi::{Errno, RELEASE_EPOCH_SECS};
use tairix_log::{Event, Sink};
use tairix_net::ntp::{
    KissCode, NtpTimestamp, RejectReason, Reply, Sample, Transaction, MAX_ROUND_TRIP, PACKET_LEN,
};

/// Discards every event: these cases exercise healthy workers unless they
/// script a failure themselves.
struct SilentSink;

impl Sink for SilentSink {
    fn write_event(&self, _event: &Event<'_>) {}
}

type TestSandbox = ParserSandbox<LoopbackLauncher<fn() -> TimeSyncService>, SilentSink>;

fn sandbox() -> TestSandbox {
    ParserSandbox::new(
        LoopbackLauncher::new(TimeSyncService::default as fn() -> TimeSyncService),
        SilentSink,
    )
}

const NONCE: u64 = 0x0123_4567_89AB_CDEF;

/// Seconds from the NTP epoch (1900) to the Unix epoch (1970).
const NTP_UNIX_DELTA: i64 = 2_208_988_800;

/// A wall instant comfortably inside the plausibility window.
fn plausible_secs() -> i64 {
    RELEASE_EPOCH_SECS + 86_400
}

fn ntp_ts(unix_secs: i64) -> NtpTimestamp {
    let field = u32::try_from((unix_secs + NTP_UNIX_DELTA).rem_euclid(1 << 32))
        .expect("reduced modulo 2^32");
    NtpTimestamp::from_raw(u64::from(field) << 32)
}

fn txn(nonce: u64) -> Transaction {
    Transaction {
        server: 3,
        nonce: NtpTimestamp::from_raw(nonce),
        sent_at: Duration64::ZERO,
    }
}

/// A well-formed stratum-2 server reply echoing `nonce` and reporting
/// `unix_secs`.
fn server_reply(nonce: u64, unix_secs: i64) -> [u8; PACKET_LEN] {
    let ts = ntp_ts(unix_secs);
    let mut p = [0u8; PACKET_LEN];
    p[0] = (4 << 3) | 4; // version 4, mode 4 (server)
    p[1] = 2; // stratum
    p[24..32].copy_from_slice(&nonce.to_be_bytes());
    p[32..40].copy_from_slice(&ts.raw().to_be_bytes());
    p[40..48].copy_from_slice(&ts.raw().to_be_bytes());
    p
}

/// A receive instant 20 ms after the request went out — a realistic round
/// trip, well inside the engine's ceiling.
fn received() -> Duration64 {
    Duration64::from_nanos(20_000_000)
}

#[test]
fn a_well_formed_reply_yields_the_sample_through_the_worker() {
    let mut sb = sandbox();
    let verdict = evaluate_datagram(
        &mut sb,
        &txn(NONCE),
        received(),
        &server_reply(NONCE, plausible_secs()),
    )
    .expect("the containment path succeeds");
    let Reply::Sample(sample) = verdict else {
        panic!("a well-formed reply must evaluate to a sample, got {verdict:?}");
    };
    assert_eq!(sample.stratum, 2);
    assert_eq!(sample.true_time.secs(), plausible_secs());
    assert!(sample.round_trip <= MAX_ROUND_TRIP);
}

#[test]
fn a_wrong_nonce_reply_never_reaches_the_worker() {
    // The parent's own gate: an injected flood must be dropped without a
    // round trip, or it becomes a denial of service against the real reply.
    let mut sb = sandbox();
    let mut spoof = server_reply(NONCE ^ 1, plausible_secs());
    assert_eq!(
        evaluate_datagram(&mut sb, &txn(NONCE), received(), &spoof),
        Ok(Reply::Unsolicited)
    );
    // A zeroed origin field (the shape a blind injector sends) is refused
    // just the same.
    spoof[ORIGIN_TS_AT..ORIGIN_TS_AT + 8].fill(0);
    assert_eq!(
        evaluate_datagram(&mut sb, &txn(NONCE), received(), &spoof),
        Ok(Reply::Unsolicited)
    );
}

#[test]
fn a_datagram_too_short_to_hold_a_header_is_unsolicited() {
    let mut sb = sandbox();
    let good = server_reply(NONCE, plausible_secs());
    for len in [0usize, 1, PACKET_LEN - 1] {
        assert_eq!(
            evaluate_datagram(&mut sb, &txn(NONCE), received(), &good[..len]),
            Ok(Reply::Unsolicited),
            "a {len}-byte datagram cannot be a reply"
        );
    }
}

#[test]
fn only_the_fixed_header_crosses_the_boundary() {
    // A longer datagram's tail (an extension field or MAC) is what the codec
    // ignores anyway, so the copy stays bounded at the header length and the
    // verdict is unchanged.
    let mut sb = sandbox();
    let mut long = server_reply(NONCE, plausible_secs()).to_vec();
    long.extend_from_slice(&[0xAA; 4096]);
    let verdict = evaluate_datagram(&mut sb, &txn(NONCE), received(), &long)
        .expect("the containment path succeeds");
    assert!(matches!(verdict, Reply::Sample(_)));
}

#[test]
fn every_engine_verdict_survives_the_round_trip_intact() {
    // The reply grammar must carry each variant losslessly, or the engine's
    // retry, rotation, and Kiss-o'-Death discipline would see the wrong thing.
    let mut sb = sandbox();

    // A stratum-0 `RATE` kiss.
    let mut kiss = server_reply(NONCE, plausible_secs());
    kiss[1] = 0;
    kiss[12..16].copy_from_slice(b"RATE");
    assert_eq!(
        evaluate_datagram(&mut sb, &txn(NONCE), received(), &kiss),
        Ok(Reply::Kiss(KissCode::Rate))
    );

    // An unrecognised kiss keeps its four raw reference-id octets, which is
    // what a reader needs to diagnose the server.
    kiss[12..16].copy_from_slice(b"INIT");
    assert_eq!(
        evaluate_datagram(&mut sb, &txn(NONCE), received(), &kiss),
        Ok(Reply::Kiss(KissCode::Other(*b"INIT")))
    );

    // A rejection reason.
    let mut wrong_mode = server_reply(NONCE, plausible_secs());
    wrong_mode[0] = (4 << 3) | 5;
    assert_eq!(
        evaluate_datagram(&mut sb, &txn(NONCE), received(), &wrong_mode),
        Ok(Reply::Rejected(RejectReason::NotServerMode))
    );
}

#[test]
fn a_malformed_request_is_a_typed_error_reply_not_a_dead_worker() {
    // The service is total: a request it cannot decode still answers, so the
    // parent's containment path is never entered for a caller-side slip.
    let mut service = TimeSyncService;
    for request in [vec![], vec![0xFF], vec![super::OP_EVALUATE, 1, 2, 3]] {
        let reply = service.handle(&request);
        assert_eq!(reply, vec![super::REPLY_ERROR]);
    }
}

/// A launcher whose workers reply with `payload` to every request, so a
/// hostile or broken worker can be scripted exactly.
struct ScriptedLauncher {
    payload: Vec<u8>,
}

/// A channel that answers every framed request with the scripted payload.
struct ScriptedChannel {
    payload: Vec<u8>,
    pending: Vec<u8>,
    at: usize,
    armed: bool,
}

impl Channel for ScriptedChannel {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Errno> {
        if self.at == self.pending.len() {
            if !self.armed {
                return Ok(0);
            }
            self.armed = false;
            let len = u32::try_from(self.payload.len()).unwrap_or(0);
            self.pending = len.to_le_bytes().to_vec();
            self.pending.extend_from_slice(&self.payload);
            self.at = 0;
        }
        let take = buf.len().min(self.pending.len() - self.at);
        buf[..take].copy_from_slice(&self.pending[self.at..self.at + take]);
        self.at += take;
        Ok(take)
    }

    fn write(&mut self, buf: &[u8]) -> Result<usize, Errno> {
        self.armed = true;
        Ok(buf.len())
    }
}

impl Launcher for ScriptedLauncher {
    type Channel = ScriptedChannel;

    fn launch(&mut self) -> Result<ScriptedChannel, Errno> {
        Ok(ScriptedChannel {
            payload: self.payload.clone(),
            pending: Vec::new(),
            at: 0,
            armed: false,
        })
    }

    fn dispose(&mut self, _channel: ScriptedChannel) -> Option<i32> {
        None
    }
}

fn hostile(payload: Vec<u8>) -> ParserSandbox<ScriptedLauncher, SilentSink> {
    ParserSandbox::new(ScriptedLauncher { payload }, SilentSink)
}

/// The reply a compromised worker would send to claim `sample`.
fn forged_sample(sample: &Sample) -> Vec<u8> {
    let mut w = Writer::new();
    encode_reply(&mut w, Reply::Sample(*sample));
    w.finish()
}

#[test]
fn a_worker_sample_outside_the_engines_own_rules_is_refused() {
    // The worker is hostile the moment it has touched a byte, so a verdict
    // that would move the clock somewhere the engine forbids must not be
    // applied — this is the re-validation the authority split rests on.
    let good = Sample {
        true_time: Time64::from_secs(plausible_secs()),
        round_trip: Duration64::from_secs(1),
        stratum: 2,
    };
    // The honest sample still passes, so the gate is not simply closed.
    let mut sb = hostile(forged_sample(&good));
    assert_eq!(
        evaluate_datagram(&mut sb, &txn(NONCE), received(), &server_reply(NONCE, 0)),
        Ok(Reply::Sample(good))
    );

    let forgeries = [
        // Before this release existed.
        Sample {
            true_time: Time64::from_secs(RELEASE_EPOCH_SECS - 1),
            ..good
        },
        // A round trip beyond the ceiling makes the estimate worthless.
        Sample {
            round_trip: Duration64::from_secs(MAX_ROUND_TRIP.secs() + 1),
            ..good
        },
        // Stratum 0 is a kiss, not a sample; 16 and up is unsynchronised.
        Sample { stratum: 0, ..good },
        Sample {
            stratum: MAX_STRATUM_EXCLUSIVE,
            ..good
        },
    ];
    for forgery in forgeries {
        let mut sb = hostile(forged_sample(&forgery));
        assert_eq!(
            evaluate_datagram(&mut sb, &txn(NONCE), received(), &server_reply(NONCE, 0)),
            Err(TimeSyncFailure::ReplyRefused),
            "the caller must refuse {forgery:?}"
        );
    }
}

#[test]
fn a_reply_violating_the_grammar_yields_nothing() {
    // Fail closed: an unparseable reply produces no verdict at all rather
    // than a partially-trusted one.
    let truncated = {
        let mut w = Writer::new();
        w.u8(REPLY_SAMPLE);
        w.finish()
    };
    let trailing = {
        let mut w = Writer::new();
        encode_reply(
            &mut w,
            Reply::Sample(Sample {
                true_time: Time64::from_secs(plausible_secs()),
                round_trip: Duration64::ZERO,
                stratum: 2,
            }),
        );
        let mut out = w.finish();
        out.push(0);
        out
    };
    for payload in [
        vec![],
        vec![0xFF],
        vec![super::REPLY_ERROR],
        vec![super::REPLY_KISS, 0xFF],
        vec![super::REPLY_REJECTED, 0xFF],
        vec![super::REPLY_UNSOLICITED, 0],
        truncated,
        trailing,
    ] {
        let mut sb = hostile(payload.clone());
        assert_eq!(
            evaluate_datagram(&mut sb, &txn(NONCE), received(), &server_reply(NONCE, 0)),
            Err(TimeSyncFailure::ReplyMalformed),
            "reply {payload:?} must not be believed"
        );
    }
}
