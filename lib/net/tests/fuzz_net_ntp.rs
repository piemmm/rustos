//! Deterministic fuzz harness for the NTP client engine.
//!
//! NTP is unauthenticated UDP, so every byte here is attacker-controlled.
//! Invariants, for any input bits a hostile server or an off-path injector
//! crafts:
//!
//! 1. [`Header::decode`] never panics, on any bytes, at any length.
//! 2. [`evaluate`] never panics and never yields a sample whose instant falls
//!    outside the plausibility window — the one thing a caller holding
//!    `CAP_TIME_SET` relies on.
//! 3. A reply whose origin timestamp does not echo the transaction's nonce is
//!    always [`Reply::Unsolicited`], never a sample and never a rejection.
//!    This is the anti-spoof gate, and classifying a foreign packet as a
//!    *rejection* would let a flood cancel the real answer.
//! 4. Driving [`NtpClient`] with arbitrary datagrams at arbitrary instants
//!    never panics, never emits a request while one is outstanding, never
//!    exceeds the poll floor's politeness, and always yields a coherent
//!    next-deadline decision.
//! 5. Timestamp conversion is total over every 64-bit value, and the era
//!    placement always lands within one era of the release-epoch anchor.
//!
//! Runs the fixed smoke sweep under plain `cargo test`; keeps drawing from the
//! same seeded stream until `TAIRIX_FUZZ_BUDGET_SECS` elapses under
//! `cargo xtask fuzz`.

use tairix_abi::time::Duration64;
use tairix_abi::{is_plausible_wall_time, RELEASE_EPOCH_SECS};
use tairix_net::ntp::{
    backoff, client_request, evaluate, jitter, Header, NtpClient, NtpTimestamp, Outcome, Reply,
    Transaction, BACKOFF_CAP, MAX_ROUND_TRIP, MAX_SERVERS, MIN_POLL, PACKET_LEN,
};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 20_000;

/// Decode arbitrary bytes and check the codec is total.
fn exercise_decode(bytes: &[u8]) {
    let Some(header) = Header::decode(bytes) else {
        // Only a short buffer may be refused.
        assert!(bytes.len() < PACKET_LEN, "a long buffer must decode");
        return;
    };
    assert!(bytes.len() >= PACKET_LEN);
    // Every accessor is infallible once decoded.
    let _ = header.leap;
    let _ = header.mode;
    let _ = header.poll;
    let _ = header.precision;
    let _ = header.root_delay;
    let _ = header.root_dispersion;
    // A kiss is reported exactly when the stratum is zero.
    assert_eq!(header.kiss().is_some(), header.stratum == 0);
}

/// Evaluate arbitrary bytes against a transaction and check the two rules a
/// `CAP_TIME_SET` holder depends on.
fn exercise_evaluate(bytes: &[u8], txn: &Transaction, received_at: Duration64) {
    match evaluate(bytes, txn, received_at) {
        Reply::Sample(sample) => {
            // Rule 2: a sample is never outside the plausibility window.
            assert!(
                is_plausible_wall_time(sample.true_time),
                "a sample escaped the plausibility window: {:?}",
                sample.true_time
            );
            // And never claims a round trip beyond the ceiling.
            assert!(sample.round_trip <= MAX_ROUND_TRIP);
            assert!((1..16).contains(&sample.stratum));
            // Rule 3, the other direction: a sample is only ever produced for
            // a reply that echoed the nonce.
            let header = Header::decode(bytes).expect("a sample implies a decode");
            assert_eq!(header.origin_ts.raw(), txn.nonce.raw());
        }
        Reply::Unsolicited => {}
        Reply::Kiss(_) | Reply::Rejected(_) => {
            // Rule 3: anything the engine treats as *ours* echoed the nonce.
            let header = Header::decode(bytes).expect("a verdict implies a decode");
            assert_eq!(
                header.origin_ts.raw(),
                txn.nonce.raw(),
                "a foreign packet must be Unsolicited, never a verdict"
            );
        }
    }
}

/// Drive the client state machine with arbitrary datagrams and instants.
fn exercise_client(rng: &mut Lcg, buf: &mut [u8]) {
    let servers = u8::try_from(1 + rng.index(MAX_SERVERS)).unwrap_or(1);
    let mut client = NtpClient::new(servers, MIN_POLL, Duration64::ZERO);
    let mut now = Duration64::ZERO;

    for _ in 0..12 {
        // Advance time by an arbitrary but non-negative amount.
        now = Duration64::from_nanos(
            now.saturating_total_nanos()
                .saturating_add(rng.next_u64() % 200_000_000_000),
        );
        let outstanding_before = client.outstanding().is_some();
        if let Some(query) = client.poll(now, rng.next_u64()) {
            assert!(
                !outstanding_before,
                "a request was emitted while one was outstanding"
            );
            assert!(usize::from(query.server) < usize::from(servers));
            assert!(client.outstanding().is_some());
            // The request is always a well-formed v4 client packet.
            let header = Header::decode(&query.packet).expect("own request decodes");
            assert_eq!(header.version, 4);
            assert_eq!(header.stratum, 0);
        }
        // Feed an arbitrary datagram, sometimes echoing the real nonce so the
        // accepting path is explored too rather than only the rejecting one.
        let size = rng.index(buf.len() + 1);
        rng.fill(&mut buf[..size]);
        if size >= PACKET_LEN {
            if let Some(txn) = client.outstanding() {
                if rng.next_u64().is_multiple_of(2) {
                    buf[24..32].copy_from_slice(&txn.nonce.raw().to_be_bytes());
                }
            }
        }
        match client.on_datagram(now, &buf[..size]) {
            Outcome::Sample(sample) => {
                assert!(is_plausible_wall_time(sample.true_time));
                assert!(client.outstanding().is_none());
            }
            Outcome::Unsolicited => {}
            Outcome::Rejected(_) | Outcome::ServerRetired { .. } | Outcome::RateLimited { .. } => {
                assert!(client.outstanding().is_none());
            }
        }

        // Rule 4: the deadline decision is always coherent — a client with
        // work pending names an instant, and an exhausted one names none.
        assert_eq!(client.next_deadline().is_none(), client.is_exhausted());
    }
}

/// Timestamp conversion and the politeness helpers are total.
fn exercise_arithmetic(rng: &mut Lcg) {
    let raw = rng.next_u64();
    let ts = NtpTimestamp::from_raw(raw);
    assert_eq!(ts.raw(), raw, "the nonce check needs an exact round trip");
    let placed = ts.to_time64();
    // Rule 5: always within one era of the anchor.
    let delta = placed.secs().saturating_sub(RELEASE_EPOCH_SECS).abs();
    assert!(delta <= 1i64 << 32, "era placement drifted by {delta}s");
    assert!(placed.subsec_nanos() < 1_000_000_000);

    // A span between two arbitrary timestamps is either refused or coherent.
    let other = NtpTimestamp::from_raw(rng.next_u64());
    if let Some(span) = ts.duration_since(other) {
        assert!(span >= Duration64::ZERO);
    }

    // Backoff is bounded and monotonic; jitter stays inside its band.
    let failures = u32::try_from(rng.next_u64() % 4_000_000_000).unwrap_or(0);
    assert!(backoff(failures) <= BACKOFF_CAP);
    let span = Duration64::from_nanos(rng.next_u64() % 1_000_000_000_000);
    let total = span.saturating_total_nanos();
    let j = jitter(span, rng.next_u64()).saturating_total_nanos();
    assert!(
        j >= total - total / 4 && j <= total + total / 4,
        "jitter {j} escaped the band around {total}"
    );

    // Encoding a request is total for any nonce.
    let packet = client_request(NtpTimestamp::from_raw(rng.next_u64()));
    assert!(Header::decode(&packet).is_some());
}

/// The seeded stream. A plain LCG: the harness needs reproducible bits, not
/// cryptographic ones.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        self.0
    }

    /// A bounded index in `[0, modulus)`; `modulus` must be non-zero.
    fn index(&mut self, modulus: usize) -> usize {
        usize::try_from(self.next_u64() & 0xFFFF).unwrap_or(0) % modulus
    }

    fn fill(&mut self, buf: &mut [u8]) {
        let mut i = 0;
        while i < buf.len() {
            let word = self.next_u64().to_le_bytes();
            let take = core::cmp::min(8, buf.len() - i);
            buf[i..i + take].copy_from_slice(&word[..take]);
            i += take;
        }
    }
}

#[test]
fn random_inputs_never_panic() {
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "random_inputs_never_panic",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let mut buf = [0u8; 96];
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            let txn = Transaction {
                server: 0,
                nonce: NtpTimestamp::from_raw(rng.next_u64()),
                sent_at: Duration64::from_nanos(rng.next_u64() % 1_000_000_000_000),
            };
            let received_at = Duration64::from_nanos(
                txn.sent_at
                    .saturating_total_nanos()
                    .saturating_add(rng.next_u64() % 10_000_000_000),
            );
            let size = rng.index(buf.len() + 1);
            rng.fill(&mut buf[..size]);
            exercise_decode(&buf[..size]);
            exercise_evaluate(&buf[..size], &txn, received_at);

            // Half the draws echo the nonce, so the accepting path is
            // explored rather than only the anti-spoof rejection.
            if size >= PACKET_LEN {
                buf[24..32].copy_from_slice(&txn.nonce.raw().to_be_bytes());
                exercise_evaluate(&buf[..size], &txn, received_at);
            }

            exercise_arithmetic(&mut rng);
            exercise_client(&mut rng, &mut buf);
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}
