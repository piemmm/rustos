//! Deterministic fuzz harness for the telnet client's receive path.
//!
//! Every byte a telnet server sends is attacker-controlled, and the client
//! answers some of them, so the invariants for any input bits a hostile server
//! crafts are:
//!
//! 1. [`Parser::feed`] never panics, on any bytes, however they are chunked.
//! 2. The parser's held subnegotiation never exceeds
//!    [`MAX_SUBNEG_LEN`](tairix_telnet::nvt::MAX_SUBNEG_LEN) — a peer cannot
//!    make the client hold an attacker-sized buffer.
//! 3. Driving a live [`Session`] with arbitrary bytes never panics, and the
//!    reply it draws is bounded by the input rather than amplifying it.
//! 4. Every byte the session *emits* re-parses as well-formed telnet, so a
//!    hostile exchange can never make the client desynchronise its own peer.
//! 5. The RFC 1184 SLC and `MODE` folds answer an arbitrary triplet stream
//!    with at most one triplet per triplet received.
//!
//! Runs the fixed smoke sweep under plain `cargo test`; keeps drawing from the
//! same seeded stream until `TAIRIX_FUZZ_BUDGET_SECS` elapses under
//! `cargo xtask fuzz`.

use tairix_telnet::command::Config;
use tairix_telnet::linemode::{Linemode, SlcTable, SLC_MAX};
use tairix_telnet::nvt::{NvtEvent, Parser, MAX_SUBNEG_LEN};
use tairix_telnet::session::Session;

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 4_000;

/// Longest chunk the sweep hands the parser in one call.
const MAX_CHUNK: usize = 96;

/// How many chunks one parser or session sees before it is replaced, so both a
/// single hostile burst and a long hostile conversation are explored.
const CHUNKS_PER_ROUND: usize = 8;

/// Feed arbitrary bytes through the parser in arbitrary chunks and assert the
/// bounded, total invariants.
fn exercise_parser(rng: &mut Lcg, buf: &mut [u8; MAX_CHUNK]) {
    let mut parser = Parser::new();
    for _ in 0..CHUNKS_PER_ROUND {
        let size = rng.index(MAX_CHUNK + 1);
        rng.fill(&mut buf[..size]);
        parser.feed(&buf[..size], |event| {
            if let NvtEvent::Subnegotiation { params, .. } = event {
                assert!(
                    params.len() <= MAX_SUBNEG_LEN,
                    "a peer sized the client's buffer: {}",
                    params.len()
                );
            }
        });
    }
}

/// Drive a live session with arbitrary network bytes, asserting it never
/// panics, never amplifies, and never emits anything but well-formed telnet.
fn exercise_session(rng: &mut Lcg, buf: &mut [u8; MAX_CHUNK]) {
    let mut session = Session::new(&Config::default(), "TAIRIX", 38_400);
    session.begin(&Config::default());
    let _ = session.take_wire();
    for _ in 0..CHUNKS_PER_ROUND {
        let size = rng.index(MAX_CHUNK + 1);
        rng.fill(&mut buf[..size]);
        let _ = session.on_network(&buf[..size]);
        let wire = session.take_wire();
        // The reply is bounded by the input: one answer per request, plus the
        // fixed-size subnegotiations a request can draw. A generous ceiling
        // still catches amplification, which is the property under test.
        assert!(
            wire.len() <= 16 * (size + 1) + MAX_SUBNEG_LEN,
            "{size} bytes drew {} bytes of reply",
            wire.len()
        );
        reparses(&wire);
        let _ = session.take_screen();
        let _ = session.take_trace();

        // Arbitrary keystrokes on the same session, so both directions are
        // driven against whatever state the hostile stream left behind.
        let typed = rng.index(MAX_CHUNK + 1);
        rng.fill(&mut buf[..typed]);
        let _ = session.on_keyboard(&buf[..typed]);
        reparses(&session.take_wire());
        let _ = session.take_screen();
        let _ = session.take_trace();
    }
}

/// Assert `bytes` is a complete, well-formed telnet stream: it re-parses, and
/// it leaves the parser back in the data state rather than mid-command.
fn reparses(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let mut parser = Parser::new();
    parser.feed(bytes, |event| {
        assert!(
            !matches!(event, NvtEvent::SubnegotiationRefused { .. }),
            "the client emitted a subnegotiation its own parser refuses"
        );
    });
    // A trailing sentinel byte must come back as data, which is only true if
    // the emitted stream ended on a command boundary.
    let mut tail_seen = false;
    parser.feed(b"\x01", |event| {
        if matches!(event, NvtEvent::Data(data) if data == b"\x01") {
            tail_seen = true;
        }
    });
    assert!(tail_seen, "the client left its own stream mid-command");
}

/// Drive the RFC 1184 folds with arbitrary payloads, asserting the reply is at
/// most one triplet per triplet received.
fn exercise_linemode(rng: &mut Lcg, buf: &mut [u8; MAX_CHUNK]) {
    let mut lm = Linemode::new();
    let mut table = SlcTable::new();
    for _ in 0..CHUNKS_PER_ROUND {
        let size = rng.index(MAX_CHUNK + 1);
        rng.fill(&mut buf[..size]);
        let outcome = lm.fold(&buf[..size]);
        assert!(outcome.reply.len() <= 5 + size * 3);
        let reply = table.fold(&buf[..size]);
        assert_eq!(
            reply.len() % 3,
            0,
            "an SLC reply is always whole triplets: {reply:?}"
        );
        assert!(
            reply.len() <= (size / 3) * 3,
            "{size} bytes drew {}",
            reply.len()
        );
        // Whatever the peer said, every entry is still a defined entry.
        for function in 1..=SLC_MAX {
            assert!(table.get(function).is_some(), "function {function}");
        }
        assert_eq!(table.get(0), None);
        assert_eq!(table.get(SLC_MAX + 1), None);
    }
}

/// Lehmer-style LCG — deterministic, no allocator. Identical to the generator
/// in the sibling harnesses so failures reproduce one way.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
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
        (self.next_u64() & 0xFFFF) as usize % modulus
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
    let mut buf = [0u8; MAX_CHUNK];
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            exercise_parser(&mut rng, &mut buf);
            exercise_session(&mut rng, &mut buf);
            exercise_linemode(&mut rng, &mut buf);
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}
