//! Unit tests for the NTP client engine.

use super::{
    backoff, client_request, evaluate, jitter, Header, KissCode, LeapIndicator, Mode, NtpClient,
    NtpTimestamp, Outcome, RejectReason, Reply, Transaction, BACKOFF_BASE, BACKOFF_CAP,
    MAX_ROOT_DISTANCE, MAX_ROUND_TRIP, MAX_SERVERS, MIN_POLL, PACKET_LEN, RESPONSE_TIMEOUT,
};
use tairix_abi::time::{Duration64, Time64};
use tairix_abi::{is_plausible_wall_time, RELEASE_EPOCH_SECS};

/// Seconds from the NTP epoch to the Unix epoch, restated here so a sign
/// error in the engine cannot be masked by reusing its own constant.
const NTP_UNIX_DELTA: i64 = 2_208_988_800;

/// Build the NTP seconds field denoting `unix_secs`, wrapping into whatever
/// era that lands in, exactly as a server on the wire would.
fn ntp_secs_for(unix_secs: i64) -> u32 {
    let ntp = unix_secs + NTP_UNIX_DELTA;
    u32::try_from(ntp.rem_euclid(1 << 32)).expect("reduced modulo 2^32")
}

fn ts(unix_secs: i64) -> NtpTimestamp {
    NtpTimestamp::from_raw(u64::from(ntp_secs_for(unix_secs)) << 32)
}

/// A wall instant comfortably inside the plausibility window.
fn plausible_secs() -> i64 {
    RELEASE_EPOCH_SECS + 86_400
}

/// Encode a server reply. Every field a validation rule reads is a parameter
/// so each rule gets its own focused case.
struct ReplyBuilder {
    leap: u8,
    version: u8,
    mode: u8,
    stratum: u8,
    root_delay: u32,
    root_dispersion: u32,
    reference_id: [u8; 4],
    origin: NtpTimestamp,
    receive: NtpTimestamp,
    transmit: NtpTimestamp,
}

impl ReplyBuilder {
    /// A well-formed stratum-2 reply echoing `nonce`, whose receive and
    /// transmit instants are one second apart inside the plausible window.
    fn good(nonce: NtpTimestamp) -> Self {
        Self {
            leap: 0,
            version: 4,
            mode: 4,
            stratum: 2,
            root_delay: 0,
            root_dispersion: 0,
            reference_id: *b"GPS\0",
            origin: nonce,
            receive: ts(plausible_secs()),
            transmit: ts(plausible_secs()),
        }
    }

    fn encode(&self) -> [u8; PACKET_LEN] {
        let mut p = [0u8; PACKET_LEN];
        p[0] = (self.leap << 6) | (self.version << 3) | self.mode;
        p[1] = self.stratum;
        p[4..8].copy_from_slice(&self.root_delay.to_be_bytes());
        p[8..12].copy_from_slice(&self.root_dispersion.to_be_bytes());
        p[12..16].copy_from_slice(&self.reference_id);
        p[24..32].copy_from_slice(&self.origin.raw().to_be_bytes());
        p[32..40].copy_from_slice(&self.receive.raw().to_be_bytes());
        p[40..48].copy_from_slice(&self.transmit.raw().to_be_bytes());
        p
    }
}

const NONCE: u64 = 0x0123_4567_89AB_CDEF;

/// One validation case: its name, the defect to introduce, and the refusal it
/// must produce.
type Case = (&'static str, fn(&mut ReplyBuilder), RejectReason);

fn txn() -> Transaction {
    Transaction {
        server: 0,
        nonce: NtpTimestamp::from_raw(NONCE),
        sent_at: Duration64::from_secs(100),
    }
}

/// Received 20 ms after the request went out.
fn received_at() -> Duration64 {
    Duration64::from_nanos(100 * 1_000_000_000 + 20_000_000)
}

// --- Timestamp conversion and the NTP era ----------------------------------

#[test]
fn timestamp_round_trips_through_the_era_nearest_the_release_epoch() {
    for unix in [
        RELEASE_EPOCH_SECS,
        RELEASE_EPOCH_SECS + 1,
        plausible_secs(),
        // Well past 2038: the 32-bit-time boundary is not a boundary here.
        RELEASE_EPOCH_SECS + 20 * 365 * 86_400,
    ] {
        assert_eq!(
            ts(unix).to_time64().secs(),
            unix,
            "unix {unix} must survive the era placement"
        );
    }
}

#[test]
fn timestamp_places_an_era_1_reading_correctly() {
    // Unix 2_208_988_800 is about 2040-01-01 — past the 2036 NTP era-0
    // rollover, so its seconds field wraps to a small value with the high bit
    // clear, which a naive decoder would read as the 1900s.
    let unix_2040: i64 = 2_208_988_800;
    let field = ntp_secs_for(unix_2040);
    assert!(
        field < 0x8000_0000,
        "an era-1 seconds field has its high bit clear, got {field:#x}"
    );
    assert!(
        i64::from(field) + NTP_UNIX_DELTA != unix_2040,
        "the field must genuinely have wrapped, or this proves nothing"
    );
    assert_eq!(ts(unix_2040).to_time64().secs(), unix_2040);
}

#[test]
fn timestamp_carries_its_fraction_as_nanoseconds() {
    // Fraction 0x8000_0000 is exactly half a second.
    let half = NtpTimestamp::from_raw((u64::from(ntp_secs_for(plausible_secs())) << 32) | 1 << 31);
    let t = half.to_time64();
    assert_eq!(t.secs(), plausible_secs());
    assert_eq!(t.subsec_nanos(), 500_000_000);
}

#[test]
fn timestamp_zero_is_recognised_and_never_a_reading() {
    assert!(NtpTimestamp::ZERO.is_zero());
    assert!(NtpTimestamp::from_raw(0).is_zero());
    assert!(!NtpTimestamp::from_raw(1).is_zero());
}

#[test]
fn timestamp_raw_round_trips_so_the_nonce_check_is_exact() {
    for raw in [0, 1, u64::MAX, NONCE, 0x8000_0000_0000_0000] {
        assert_eq!(NtpTimestamp::from_raw(raw).raw(), raw);
    }
}

#[test]
fn duration_since_refuses_a_backwards_or_absurd_span() {
    let later = NtpTimestamp::from_raw(10 << 32);
    let earlier = NtpTimestamp::from_raw(4 << 32);
    assert_eq!(
        later.duration_since(earlier),
        Some(Duration64::from_secs(6))
    );
    assert_eq!(earlier.duration_since(later), None);
    // Beyond the fixed-point span bound.
    let absurd = NtpTimestamp::from_raw(2_000_000 << 32);
    assert_eq!(absurd.duration_since(NtpTimestamp::ZERO), None);
}

// --- The codec -------------------------------------------------------------

#[test]
fn client_request_is_a_v4_client_packet_carrying_only_the_nonce() {
    let nonce = NtpTimestamp::from_raw(NONCE);
    let p = client_request(nonce);
    let h = Header::decode(&p).expect("48 bytes decode");
    assert_eq!(h.mode, Mode::Client);
    assert_eq!(h.version, 4);
    assert_eq!(h.leap, LeapIndicator::NoWarning);
    assert_eq!(h.stratum, 0);
    assert_eq!(h.transmit_ts, nonce);
    // The local clock never reaches the wire.
    assert!(h.reference_ts.is_zero());
    assert!(h.origin_ts.is_zero());
    assert!(h.receive_ts.is_zero());
}

#[test]
fn decode_refuses_a_short_buffer_and_ignores_a_long_tail() {
    let p = ReplyBuilder::good(NtpTimestamp::from_raw(NONCE)).encode();
    for short in 0..PACKET_LEN {
        assert!(Header::decode(&p[..short]).is_none(), "{short} bytes");
    }
    // A reply may legally carry extension fields or a MAC.
    let mut long = [0u8; PACKET_LEN + 20];
    long[..PACKET_LEN].copy_from_slice(&p);
    assert_eq!(Header::decode(&long), Header::decode(&p));
}

#[test]
fn decode_is_total_over_every_leap_and_mode_bit_pattern() {
    for byte in 0u8..=255 {
        let mut p = [0u8; PACKET_LEN];
        p[0] = byte;
        let h = Header::decode(&p).expect("length is all that can fail");
        // Every bit pattern maps to a variant; nothing panics or is lost.
        assert_eq!(h.version, (byte >> 3) & 0b111);
        let _ = h.leap;
        let _ = h.mode;
    }
}

#[test]
fn kiss_codes_decode_and_only_deny_and_restrict_retire() {
    for (id, code, retires) in [
        (b"RATE", KissCode::Rate, false),
        (b"DENY", KissCode::Deny, true),
        (b"RSTR", KissCode::Restrict, true),
        (b"INIT", KissCode::Other(*b"INIT"), false),
    ] {
        let mut b = ReplyBuilder::good(NtpTimestamp::from_raw(NONCE));
        b.stratum = 0;
        b.reference_id = *id;
        let h = Header::decode(&b.encode()).expect("decodes");
        assert_eq!(h.kiss(), Some(code));
        assert_eq!(code.retires_server(), retires);
    }
    // A non-zero stratum is never a kiss.
    let h = Header::decode(&ReplyBuilder::good(NtpTimestamp::from_raw(NONCE)).encode())
        .expect("decodes");
    assert_eq!(h.kiss(), None);
}

// --- Validation: the anti-spoof gate --------------------------------------

#[test]
fn a_reply_whose_origin_does_not_echo_the_nonce_is_unsolicited() {
    let mut b = ReplyBuilder::good(NtpTimestamp::from_raw(NONCE));
    b.origin = NtpTimestamp::from_raw(NONCE ^ 1);
    assert_eq!(
        evaluate(&b.encode(), &txn(), received_at()),
        Reply::Unsolicited,
        "a one-bit nonce difference must not be accepted"
    );
    // Zero origin (a server that ignored the field) is equally not ours.
    b.origin = NtpTimestamp::ZERO;
    assert_eq!(
        evaluate(&b.encode(), &txn(), received_at()),
        Reply::Unsolicited
    );
}

#[test]
fn a_short_packet_is_unsolicited_rather_than_a_rejection() {
    // Unsolicited is the load-bearing distinction: a rejection ends the
    // transaction, so an injected runt must not be able to cancel it.
    assert_eq!(evaluate(&[], &txn(), received_at()), Reply::Unsolicited);
    assert_eq!(
        evaluate(&[0u8; PACKET_LEN - 1], &txn(), received_at()),
        Reply::Unsolicited
    );
}

#[test]
fn a_good_reply_yields_a_sample_at_the_servers_transmit_instant() {
    let b = ReplyBuilder::good(NtpTimestamp::from_raw(NONCE));
    let Reply::Sample(sample) = evaluate(&b.encode(), &txn(), received_at()) else {
        panic!("a well-formed reply must produce a sample");
    };
    assert_eq!(sample.stratum, 2);
    // Receive and transmit are equal here, so the whole 20 ms is round trip.
    assert_eq!(sample.round_trip, Duration64::from_nanos(20_000_000));
    // The instant is the server's transmit plus half the round trip.
    assert_eq!(sample.true_time.secs(), plausible_secs());
    assert_eq!(sample.true_time.subsec_nanos(), 10_000_000);
    assert!(is_plausible_wall_time(sample.true_time));
}

#[test]
fn the_round_trip_excludes_the_servers_own_processing_time() {
    let mut b = ReplyBuilder::good(NtpTimestamp::from_raw(NONCE));
    // Server held the request for 8 ms of the 20 ms round trip.
    b.receive = NtpTimestamp::from_raw(
        (u64::from(ntp_secs_for(plausible_secs())) << 32) | ((1u64 << 32) / 125),
    );
    b.transmit = NtpTimestamp::from_raw(
        (u64::from(ntp_secs_for(plausible_secs())) << 32)
            | ((1u64 << 32) / 125 + (1u64 << 32) * 8 / 1000),
    );
    let Reply::Sample(sample) = evaluate(&b.encode(), &txn(), received_at()) else {
        panic!("expected a sample");
    };
    // 20 ms observed minus ~8 ms of server time, to fixed-point precision.
    let rt = sample.round_trip.saturating_total_nanos();
    assert!(
        (11_990_000..=12_010_000).contains(&rt),
        "round trip {rt} ns should be about 12 ms"
    );
}

#[test]
fn validation_refuses_each_defect_in_turn() {
    let nonce = NtpTimestamp::from_raw(NONCE);
    let cases: [Case; 7] = [
        (
            "broadcast mode",
            |b| b.mode = 5,
            RejectReason::NotServerMode,
        ),
        (
            "version 2",
            |b| b.version = 2,
            RejectReason::UnsupportedVersion,
        ),
        (
            "server says it is unsynchronised",
            |b| b.leap = 3,
            RejectReason::ServerUnsynchronised,
        ),
        (
            "stratum 16",
            |b| b.stratum = 16,
            RejectReason::StratumUnusable,
        ),
        (
            "unspecified transmit timestamp",
            |b| b.transmit = NtpTimestamp::ZERO,
            RejectReason::UnspecifiedTimestamp,
        ),
        (
            "root distance beyond the ceiling",
            |b| b.root_delay = 2 << 16,
            RejectReason::RootDistanceTooLarge,
        ),
        (
            "reply sent before it was received",
            |b| {
                b.receive = ts(plausible_secs() + 1);
                b.transmit = ts(plausible_secs());
            },
            RejectReason::InconsistentTimestamps,
        ),
    ];
    for (name, mutate, expected) in cases {
        let mut b = ReplyBuilder::good(nonce);
        mutate(&mut b);
        assert_eq!(
            evaluate(&b.encode(), &txn(), received_at()),
            Reply::Rejected(expected),
            "{name} must be refused"
        );
    }
}

#[test]
fn root_distance_is_the_sum_of_delay_and_dispersion() {
    let nonce = NtpTimestamp::from_raw(NONCE);
    // Each half is under the ceiling; together they exceed it.
    let mut b = ReplyBuilder::good(nonce);
    b.root_delay = 3 << 14;
    b.root_dispersion = 3 << 14;
    assert_eq!(
        evaluate(&b.encode(), &txn(), received_at()),
        Reply::Rejected(RejectReason::RootDistanceTooLarge)
    );
    // Exactly at the ceiling is admitted.
    let mut ok = ReplyBuilder::good(nonce);
    ok.root_delay = 1 << 16;
    ok.root_dispersion = 0;
    assert_eq!(
        MAX_ROOT_DISTANCE,
        Duration64::from_secs(1),
        "the case below assumes a one-second ceiling"
    );
    assert!(matches!(
        evaluate(&ok.encode(), &txn(), received_at()),
        Reply::Sample(_)
    ));
}

#[test]
fn a_round_trip_beyond_the_ceiling_is_refused() {
    let b = ReplyBuilder::good(NtpTimestamp::from_raw(NONCE));
    let late = Duration64::from_secs(100 + MAX_ROUND_TRIP.secs() + 1);
    assert_eq!(
        evaluate(&b.encode(), &txn(), late),
        Reply::Rejected(RejectReason::RoundTripTooLong)
    );
}

#[test]
fn a_reply_received_before_it_was_sent_is_inconsistent() {
    let b = ReplyBuilder::good(NtpTimestamp::from_raw(NONCE));
    assert_eq!(
        evaluate(&b.encode(), &txn(), Duration64::from_secs(99)),
        Reply::Rejected(RejectReason::InconsistentTimestamps)
    );
}

#[test]
fn an_implausible_instant_is_refused_however_well_formed_the_reply() {
    let nonce = NtpTimestamp::from_raw(NONCE);
    // A server insisting on a time before this release exists.
    for unix in [0, RELEASE_EPOCH_SECS - 1, -1] {
        let mut b = ReplyBuilder::good(nonce);
        b.receive = ts(unix);
        b.transmit = ts(unix);
        assert_eq!(
            evaluate(&b.encode(), &txn(), received_at()),
            Reply::Rejected(RejectReason::ImplausibleTime),
            "unix {unix} must be refused"
        );
    }
}

// --- Politeness ------------------------------------------------------------

#[test]
fn backoff_doubles_from_the_base_and_clamps_at_the_cap() {
    assert_eq!(backoff(0), BACKOFF_BASE);
    assert_eq!(backoff(1), Duration64::from_secs(BACKOFF_BASE.secs() * 2));
    assert_eq!(backoff(2), Duration64::from_secs(BACKOFF_BASE.secs() * 4));
    // Monotonic non-decreasing, and never past the cap, for every input.
    let mut prev = Duration64::ZERO;
    for failures in 0..200u32 {
        let d = backoff(failures);
        assert!(d >= prev, "backoff must not decrease at {failures}");
        assert!(d <= BACKOFF_CAP, "backoff must not exceed the cap");
        prev = d;
    }
    assert_eq!(backoff(u32::MAX), BACKOFF_CAP);
}

#[test]
fn jitter_stays_within_a_quarter_either_side() {
    let span = Duration64::from_secs(100);
    let total = span.saturating_total_nanos();
    for entropy in [0, 1, u64::MAX / 2, u64::MAX - 1, u64::MAX] {
        let j = jitter(span, entropy).saturating_total_nanos();
        assert!(
            j >= total - total / 4 && j <= total + total / 4,
            "jittered {j} must stay within +/-25% of {total}"
        );
    }
    // The extremes reach the bounds, so the whole spread is usable rather
    // than the top of it being unreachable.
    assert_eq!(jitter(span, 0).saturating_total_nanos(), total - total / 4);
    assert_eq!(
        jitter(span, u64::MAX).saturating_total_nanos(),
        total + total / 4
    );
    // And the middle of the entropy range lands near the middle of the span,
    // so low values are not favoured.
    let mid = jitter(span, u64::MAX / 2).saturating_total_nanos();
    assert!(
        mid.abs_diff(total) < total / 100,
        "mid-range entropy {mid} should land near {total}"
    );
}

#[test]
fn jitter_never_overflows_on_an_absurd_span() {
    // Overflow checks are on in every profile, so an unclamped fixed-point
    // multiply here would panic rather than misbehave. No configuration can
    // produce such a span, but the helper is public.
    for span in [
        Duration64::from_secs(i64::MAX),
        Duration64::from_secs(i64::MAX / 2),
        Duration64::from_nanos(u64::MAX),
    ] {
        for entropy in [0, u64::MAX / 3, u64::MAX] {
            let j = jitter(span, entropy);
            assert!(j >= Duration64::ZERO, "span {span:?} entropy {entropy}");
        }
    }
}

#[test]
fn jitter_of_a_tiny_span_is_the_span() {
    // No spread to distribute, and no division by zero.
    assert_eq!(jitter(Duration64::ZERO, u64::MAX), Duration64::ZERO);
    assert_eq!(
        jitter(Duration64::from_nanos(3), u64::MAX),
        Duration64::from_nanos(3)
    );
}

#[test]
fn the_poll_floor_cannot_be_lowered_by_configuration() {
    let client = NtpClient::new(2, Duration64::from_secs(1), Duration64::ZERO);
    assert_eq!(client.poll_interval(), MIN_POLL);
    // A longer interval than the floor is honoured as asked.
    let slow = NtpClient::new(2, Duration64::from_secs(3600), Duration64::ZERO);
    assert_eq!(slow.poll_interval(), Duration64::from_secs(3600));
}

// --- The transaction state machine ---------------------------------------

/// Drive a client to its first in-flight request and return the query.
fn first_query(client: &mut NtpClient) -> super::Query {
    client
        .poll(Duration64::ZERO, NONCE)
        .expect("the first query is due at zero")
}

#[test]
fn the_first_poll_sends_one_request_carrying_the_supplied_nonce() {
    let mut client = NtpClient::new(2, MIN_POLL, Duration64::ZERO);
    let q = first_query(&mut client);
    assert_eq!(q.server, 0);
    let h = Header::decode(&q.packet).expect("decodes");
    assert_eq!(h.transmit_ts.raw(), NONCE);
    let outstanding = client.outstanding().expect("a transaction is in flight");
    assert_eq!(outstanding.nonce.raw(), NONCE);
    // Only one request in flight: a second poll at the same instant is silent.
    assert!(client.poll(Duration64::ZERO, NONCE + 1).is_none());
    // And the deadline is the response timeout.
    assert_eq!(client.next_deadline(), Some(RESPONSE_TIMEOUT));
}

#[test]
fn a_good_reply_schedules_the_next_query_a_poll_interval_later() {
    let mut client = NtpClient::new(1, MIN_POLL, Duration64::ZERO);
    let _ = first_query(&mut client);
    let reply = ReplyBuilder::good(NtpTimestamp::from_raw(NONCE)).encode();
    let at = Duration64::from_nanos(20_000_000);
    let Outcome::Sample(sample) = client.on_datagram(at, &reply) else {
        panic!("expected a sample");
    };
    assert!(is_plausible_wall_time(sample.true_time));
    assert_eq!(client.outstanding(), None);
    let expected =
        Duration64::from_nanos(at.saturating_total_nanos() + MIN_POLL.saturating_total_nanos());
    assert_eq!(client.next_deadline(), Some(expected));
}

#[test]
fn a_wrong_nonce_flood_never_cancels_the_outstanding_transaction() {
    let mut client = NtpClient::new(1, MIN_POLL, Duration64::ZERO);
    let _ = first_query(&mut client);
    let before = client.outstanding().expect("in flight");
    for spoof in 0..64u64 {
        let mut b = ReplyBuilder::good(NtpTimestamp::from_raw(NONCE));
        b.origin = NtpTimestamp::from_raw(NONCE ^ (spoof | 1));
        assert_eq!(
            client.on_datagram(Duration64::from_nanos(1_000_000), &b.encode()),
            Outcome::Unsolicited
        );
    }
    assert_eq!(
        client.outstanding(),
        Some(before),
        "the real answer must still be awaited"
    );
    // The genuine reply is still accepted afterwards.
    let good = ReplyBuilder::good(NtpTimestamp::from_raw(NONCE)).encode();
    assert!(matches!(
        client.on_datagram(Duration64::from_nanos(20_000_000), &good),
        Outcome::Sample(_)
    ));
}

#[test]
fn a_datagram_with_nothing_outstanding_changes_no_state() {
    let mut client = NtpClient::new(1, MIN_POLL, Duration64::from_secs(10));
    let before = client.next_deadline();
    let good = ReplyBuilder::good(NtpTimestamp::from_raw(NONCE)).encode();
    assert_eq!(
        client.on_datagram(Duration64::ZERO, &good),
        Outcome::Unsolicited
    );
    assert_eq!(client.next_deadline(), before);
}

#[test]
fn a_response_timeout_rotates_the_server_and_backs_off() {
    let mut client = NtpClient::new(2, MIN_POLL, Duration64::ZERO);
    let q0 = first_query(&mut client);
    assert_eq!(q0.server, 0);
    // At the timeout the transaction ends; no query is emitted yet.
    let timeout_at = RESPONSE_TIMEOUT;
    assert!(client.poll(timeout_at, NONCE).is_none());
    assert_eq!(client.outstanding(), None);
    let deadline = client.next_deadline().expect("a retry is scheduled");
    assert_eq!(
        deadline,
        Duration64::from_nanos(
            timeout_at.saturating_total_nanos() + BACKOFF_BASE.saturating_total_nanos()
        )
    );
    // The retry goes to the *other* server.
    let q1 = client.poll(deadline, NONCE + 1).expect("retry is due");
    assert_eq!(q1.server, 1, "a failing server must not absorb every retry");
}

#[test]
fn consecutive_failures_lengthen_the_backoff() {
    let mut client = NtpClient::new(1, MIN_POLL, Duration64::ZERO);
    let mut now = Duration64::ZERO;
    let mut previous = Duration64::ZERO;
    for round in 0..4 {
        let _ = client.poll(now, NONCE + round).expect("a query is due");
        now = Duration64::from_nanos(
            now.saturating_total_nanos() + RESPONSE_TIMEOUT.saturating_total_nanos(),
        );
        assert!(client.poll(now, NONCE).is_none(), "the request times out");
        let next = client.next_deadline().expect("scheduled");
        let wait =
            Duration64::from_nanos(next.saturating_total_nanos() - now.saturating_total_nanos());
        if round > 0 {
            assert!(wait > previous, "round {round} must wait longer");
        }
        previous = wait;
        now = next;
    }
}

#[test]
fn a_good_reply_resets_the_failure_backoff() {
    let mut client = NtpClient::new(1, MIN_POLL, Duration64::ZERO);
    let _ = first_query(&mut client);
    assert!(client.poll(RESPONSE_TIMEOUT, NONCE).is_none());
    let retry_at = client.next_deadline().expect("scheduled");
    let _ = client.poll(retry_at, NONCE).expect("retry");
    let good = ReplyBuilder::good(NtpTimestamp::from_raw(NONCE)).encode();
    let at = Duration64::from_nanos(retry_at.saturating_total_nanos() + 1_000_000);
    assert!(matches!(client.on_datagram(at, &good), Outcome::Sample(_)));
    // Next failure starts from the base again, not from where it left off.
    let due = client.next_deadline().expect("scheduled");
    let _ = client.poll(due, NONCE).expect("scheduled query");
    let timeout = Duration64::from_nanos(
        due.saturating_total_nanos() + RESPONSE_TIMEOUT.saturating_total_nanos(),
    );
    assert!(client.poll(timeout, NONCE).is_none());
    let next = client.next_deadline().expect("scheduled");
    assert_eq!(
        Duration64::from_nanos(next.saturating_total_nanos() - timeout.saturating_total_nanos()),
        BACKOFF_BASE
    );
}

// --- Kiss-o'-Death discipline --------------------------------------------

fn kiss_reply(nonce: u64, code: [u8; 4]) -> [u8; PACKET_LEN] {
    let mut b = ReplyBuilder::good(NtpTimestamp::from_raw(nonce));
    b.stratum = 0;
    b.reference_id = code;
    b.encode()
}

#[test]
fn a_deny_kiss_retires_that_server_and_the_client_uses_the_other() {
    let mut client = NtpClient::new(2, MIN_POLL, Duration64::ZERO);
    let q = first_query(&mut client);
    assert_eq!(q.server, 0);
    assert_eq!(
        client.on_datagram(
            Duration64::from_nanos(1_000_000),
            &kiss_reply(NONCE, *b"DENY")
        ),
        Outcome::ServerRetired {
            server: 0,
            code: KissCode::Deny,
        }
    );
    let due = client.next_deadline().expect("scheduled");
    let next = client.poll(due, NONCE + 1).expect("a query is due");
    assert_eq!(next.server, 1, "the retired server must not be queried");
}

#[test]
fn retiring_every_server_exhausts_the_client_rather_than_looping() {
    let mut client = NtpClient::new(2, MIN_POLL, Duration64::ZERO);
    let mut now = Duration64::ZERO;
    for round in 0..2u64 {
        let q = client.poll(now, NONCE + round).expect("a query is due");
        assert!(!client.is_exhausted());
        let outcome = client.on_datagram(
            Duration64::from_nanos(now.saturating_total_nanos() + 1_000_000),
            &kiss_reply(NONCE + round, *b"RSTR"),
        );
        assert_eq!(
            outcome,
            Outcome::ServerRetired {
                server: q.server,
                code: KissCode::Restrict,
            }
        );
        if let Some(next) = client.next_deadline() {
            now = next;
        }
    }
    assert!(client.is_exhausted(), "no server is left to ask");
    assert_eq!(client.next_deadline(), None, "nothing left to wait for");
    assert!(client
        .poll(Duration64::from_secs(1_000_000), NONCE)
        .is_none());
}

#[test]
fn a_rate_kiss_holds_that_server_off_and_widens_on_repeat() {
    let mut client = NtpClient::new(1, MIN_POLL, Duration64::ZERO);
    let _ = first_query(&mut client);
    let first_kiss_at = Duration64::from_nanos(1_000_000);
    assert_eq!(
        client.on_datagram(first_kiss_at, &kiss_reply(NONCE, *b"RATE")),
        Outcome::RateLimited { server: 0 }
    );

    // The hold, not the much shorter failure backoff, decides the next query:
    // obeying RATE means waiting at least the poll floor for that server.
    let first_hold = client.next_deadline().expect("scheduled");
    assert_eq!(
        first_hold.saturating_total_nanos() - first_kiss_at.saturating_total_nanos(),
        MIN_POLL.saturating_total_nanos(),
        "the first RATE must hold the server off by the poll floor"
    );
    // And that is one sleep, not a wake that finds the server still held.
    let q = client.poll(first_hold, NONCE + 1).expect("query is due");
    assert_eq!(q.server, 0);

    // A second RATE doubles the hold.
    let second_kiss_at = Duration64::from_nanos(first_hold.saturating_total_nanos() + 1_000_000);
    assert_eq!(
        client.on_datagram(second_kiss_at, &kiss_reply(NONCE + 1, *b"RATE")),
        Outcome::RateLimited { server: 0 }
    );
    let second_hold = client.next_deadline().expect("scheduled");
    assert_eq!(
        second_hold.saturating_total_nanos() - second_kiss_at.saturating_total_nanos(),
        2 * MIN_POLL.saturating_total_nanos(),
        "a repeated RATE must widen the hold"
    );
}

#[test]
fn an_unrecognised_kiss_is_unusable_but_keeps_the_server() {
    let mut client = NtpClient::new(1, MIN_POLL, Duration64::ZERO);
    let _ = first_query(&mut client);
    assert_eq!(
        client.on_datagram(
            Duration64::from_nanos(1_000_000),
            &kiss_reply(NONCE, *b"INIT")
        ),
        Outcome::Rejected(RejectReason::UnusableKiss)
    );
    assert!(!client.is_exhausted());
    let due = client.next_deadline().expect("scheduled");
    assert_eq!(
        client.poll(due, NONCE + 1).map(|q| q.server),
        Some(0),
        "the server is still usable"
    );
}

// --- Configuration edges -------------------------------------------------

#[test]
fn an_empty_server_set_is_exhausted_and_never_sends() {
    let mut client = NtpClient::new(0, MIN_POLL, Duration64::ZERO);
    assert!(client.is_exhausted());
    assert_eq!(client.next_deadline(), None);
    assert!(client.poll(Duration64::from_secs(1), NONCE).is_none());
}

#[test]
fn a_server_count_beyond_the_bound_is_clamped_not_trusted() {
    let mut client = NtpClient::new(u8::MAX, MIN_POLL, Duration64::ZERO);
    // Rotate further than the bound and confirm every index stays in range.
    let mut now = Duration64::ZERO;
    for round in 0..(MAX_SERVERS as u64 + 4) {
        if let Some(q) = client.poll(now, NONCE + round) {
            assert!(
                usize::from(q.server) < MAX_SERVERS,
                "server index {} must stay inside the bound",
                q.server
            );
        }
        now = Duration64::from_nanos(
            now.saturating_total_nanos() + RESPONSE_TIMEOUT.saturating_total_nanos(),
        );
        let _ = client.poll(now, NONCE);
        if let Some(next) = client.next_deadline() {
            now = next;
        }
    }
}

#[test]
fn the_first_query_can_be_deferred_past_zero() {
    let mut client = NtpClient::new(1, MIN_POLL, Duration64::from_secs(3600));
    assert_eq!(client.next_deadline(), Some(Duration64::from_secs(3600)));
    assert!(client.poll(Duration64::from_secs(3599), NONCE).is_none());
    assert!(client.poll(Duration64::from_secs(3600), NONCE).is_some());
}

#[test]
fn a_sample_never_yields_an_implausible_instant() {
    // The window check is inside the engine, so no caller can be handed one.
    let mut client = NtpClient::new(1, MIN_POLL, Duration64::ZERO);
    let _ = first_query(&mut client);
    let mut b = ReplyBuilder::good(NtpTimestamp::from_raw(NONCE));
    b.receive = ts(0);
    b.transmit = ts(0);
    assert_eq!(
        client.on_datagram(Duration64::from_nanos(1_000_000), &b.encode()),
        Outcome::Rejected(RejectReason::ImplausibleTime)
    );
}

#[test]
fn time64_conversion_survives_the_extremes_of_the_seconds_field() {
    // Total over every seconds field: no panic, no overflow.
    for field in [0u32, 1, 0x7FFF_FFFF, 0x8000_0000, u32::MAX] {
        let t = NtpTimestamp::from_raw(u64::from(field) << 32).to_time64();
        // Placed within one era of the anchor either way.
        let delta = t.secs().saturating_sub(RELEASE_EPOCH_SECS).abs();
        assert!(
            delta <= 1 << 32,
            "field {field} placed {delta}s from the anchor"
        );
    }
    assert_eq!(Time64::from_secs(0).secs(), 0);
}
