//! Deterministic fuzz harness for the shared ICMP/ICMPv6 machinery.
//!
//! Invariants, for any input bits a peer crafts, in both families:
//!
//! 1. Message, echo, and error parsing never panic.
//! 2. An accepted decode round-trips through its matching encoder.
//! 3. The rate limiter never allows more than its budget.
//!
//! Runs the fixed smoke sweep under plain `cargo test`; keeps drawing
//! from the same seeded stream until `RUSTOS_FUZZ_BUDGET_SECS` elapses
//! under `cargo xtask fuzz`.

use rustos_abi::time::Duration64;
use rustos_net::icmp::{ErrorRateLimiter, IcmpContext, IcmpEcho, IcmpError, IcmpMessage};
use rustos_net::Ipv6Addr;

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 20_000;

fn contexts() -> [IcmpContext; 2] {
    [
        IcmpContext::V4,
        IcmpContext::V6 {
            source: Ipv6Addr::new(0xFE80, 0, 0, 0, 0, 0, 0, 1),
            destination: Ipv6Addr::new(0xFE80, 0, 0, 0, 0, 0, 0, 2),
        },
    ]
}

fn exercise(bytes: &[u8]) {
    for context in contexts() {
        if let Some(message) = IcmpMessage::parse(context, bytes) {
            let mut out = vec![0u8; bytes.len()];
            let len = message
                .write(context, &mut out)
                .expect("a parsed message must re-encode");
            let redecoded = IcmpMessage::parse(context, &out[..len])
                .expect("round-trip of an accepted message must parse");
            assert_eq!(message, redecoded);
        }
        if let Some(echo) = IcmpEcho::parse(context, bytes) {
            let mut out = vec![0u8; echo.wire_len()];
            let len = echo
                .write(context, &mut out)
                .expect("a parsed echo must re-encode");
            let redecoded = IcmpEcho::parse(context, &out[..len])
                .expect("round-trip of an accepted echo must parse");
            assert_eq!(echo, redecoded);
        }
        if let Some(error) = IcmpError::parse(context, bytes) {
            let mut out = vec![0u8; error.wire_len()];
            if let Some(len) = error.write(context, &mut out) {
                let redecoded = IcmpError::parse(context, &out[..len])
                    .expect("round-trip of an accepted error must parse");
                assert_eq!(error.kind, redecoded.kind);
                assert_eq!(error.invoking, redecoded.invoking);
            }
        }
    }
}

/// Lehmer-style LCG — deterministic, no allocator. Identical to the
/// generator in the sibling harnesses so failures reproduce one way.
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
    let mut rng = Lcg::new(rustos_fuzzseed::start(
        "random_inputs_never_panic",
        rustos_fuzzseed::FUZZ_SEED_ENV,
    ));
    let mut buf = [0u8; 256];
    let deadline = rustos_fuzzseed::budget_deadline(rustos_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            let size = ((rng.next_u64() & 0x1FF) as usize) % (buf.len() + 1);
            rng.fill(&mut buf[..size]);
            exercise(&buf[..size]);
        }
        if !rustos_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}

#[test]
fn rate_limiter_budget_holds_under_random_time_steps() {
    let mut rng = Lcg::new(rustos_fuzzseed::start(
        "rate_limiter_budget_holds_under_random_time_steps",
        rustos_fuzzseed::FUZZ_SEED_ENV,
    ));
    let deadline = rustos_fuzzseed::budget_deadline(rustos_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        // Over any window, allowed count never exceeds burst + rate ×
        // elapsed seconds (+1 for the fractional carry).
        let burst = (rng.next_u64() % 8) as u32;
        let rate = (rng.next_u64() % 8) as u32;
        let mut limiter = ErrorRateLimiter::new(burst, rate);
        let mut now_ms: u64 = 0;
        let mut allowed: u64 = 0;
        for _ in 0..1_000 {
            now_ms += rng.next_u64() % 100;
            let now = Duration64::from_nanos(now_ms * 1_000_000);
            if limiter.allow(now) {
                allowed += 1;
            }
        }
        let ceiling = u64::from(burst) + u64::from(rate) * (now_ms / 1_000 + 1);
        assert!(
            allowed <= ceiling,
            "allowed {allowed} exceeds ceiling {ceiling} (burst {burst}, rate {rate})"
        );
        if !rustos_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}

#[test]
fn structured_inputs_with_corrupted_fields_never_panic() {
    for context in contexts() {
        let mut message = vec![0u8; 64];
        let echo = IcmpEcho {
            kind: rustos_net::icmp::EchoKind::Request,
            identifier: 0x1234,
            sequence: 7,
            payload: &[0xAB; 24],
        };
        let len = echo.write(context, &mut message).expect("seed writes");
        message.truncate(len);
        IcmpEcho::parse(context, &message).expect("seed parses");
        for byte in 0..message.len() {
            for bit in 0..8u32 {
                message[byte] ^= 1 << bit;
                exercise(&message);
                message[byte] ^= 1 << bit;
            }
        }
    }
}
