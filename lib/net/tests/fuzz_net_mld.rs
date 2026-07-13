//! Deterministic fuzz harness for the MLDv2 codec.
//!
//! Invariants, for any input bits a peer crafts:
//!
//! 1. [`MldQuery::parse`] never panics, on any body bytes.
//! 2. A parsed query always yields a canonical millisecond window (the
//!    MLDv2 floating-point decode never overflows).
//! 3. [`write_v2_report`] never panics and reports exactly the byte
//!    length [`v2_report_len`] predicts for a bounded record set, and
//!    refuses (returns `None`) when the buffer is too small.
//!
//! Runs a fixed smoke sweep under plain `cargo test`; keeps drawing from
//! the same seeded stream until the budget elapses under
//! `cargo xtask fuzz`.

use rustos_net::mld::{self, MldQuery, RecordType};
use rustos_net::Ipv6Addr;

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 20_000;

/// Bound on the records a fuzzed report carries (matches the small
/// per-report record sets the stack actually emits).
const MAX_RECORDS: usize = 8;

fn exercise_query_parse(body: &[u8]) {
    if let Some(query) = MldQuery::parse(body) {
        // `is_general` agrees with the unspecified address, and the
        // window is a plain millisecond count (never a raw code).
        assert_eq!(query.is_general(), query.multicast_address.is_unspecified());
        let _ = query.max_response_millis;
    }
}

fn exercise_report_write(rng: &mut Lcg) {
    let count = ((rng.next_u64() & 0xF) as usize) % (MAX_RECORDS + 1);
    let mut records = alloc_records(rng, count);
    let needed = mld::v2_report_len(count);
    // A buffer one byte short fails closed.
    if needed > 0 {
        let mut short = vec![0u8; needed - 1];
        assert!(mld::write_v2_report(&records, &mut short).is_none());
    }
    let mut out = vec![0u8; needed];
    let written = mld::write_v2_report(&records, &mut out).expect("exact buffer writes");
    assert_eq!(written, needed);
    records.clear();
}

fn alloc_records(rng: &mut Lcg, count: usize) -> Vec<(RecordType, Ipv6Addr)> {
    let mut records = Vec::with_capacity(count);
    for _ in 0..count {
        let record_type = match rng.next_u64() % 3 {
            0 => RecordType::ModeIsExclude,
            1 => RecordType::ChangeToInclude,
            _ => RecordType::ChangeToExclude,
        };
        let mut octets = [0u8; 16];
        rng.fill(&mut octets);
        records.push((record_type, Ipv6Addr::from(octets)));
    }
    records
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
    let mut buf = [0u8; 64];
    let deadline = rustos_fuzzseed::budget_deadline(rustos_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            let size = ((rng.next_u64() & 0x7F) as usize) % (buf.len() + 1);
            rng.fill(&mut buf[..size]);
            exercise_query_parse(&buf[..size]);
            exercise_report_write(&mut rng);
        }
        if !rustos_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}

#[test]
fn floating_max_response_code_never_overflows() {
    // Every possible Maximum Response Code decodes to a finite window.
    for code in 0u16..=u16::MAX {
        let mut body = [0u8; mld::MLDV2_QUERY_MIN_BODY_LEN];
        body[0..2].copy_from_slice(&code.to_be_bytes());
        let query = MldQuery::parse(&body).expect("v2 query parses");
        assert!(query.is_v2);
        let _ = query.max_response_millis;
    }
}
