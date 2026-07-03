//! Deterministic fuzz-style integration test for the record-ingress admission
//! path ([`rustos_log::Ingress`]).
//!
//! Ingress is where an untrusted caller's *requests* (a stream, a source, a
//! level) meet an attested [`Origin`] and become an authoritative record.
//! Every one of those requests is attacker-influenced, so the admission
//! decision must never panic and must never let a caller escalate: a
//! user-domain origin can never obtain a privileged stream or a reserved
//! source name, and the per-stream append sequence it hands out must be
//! strictly monotonic. This harness drives [`Ingress::admit`] on pseudo-random
//! (origin, subsystem, requested-stream, requested-source, level) tuples and
//! asserts those invariants, then encodes and decodes the built record so the
//! whole ingress→on-disk path is exercised, not just the decision.
//!
//! Seed selection, the start-of-test seed log, and the smoke / soak loop are
//! the shared `rustos_fuzzseed` seam (one definition).

use rustos_abi::{
    CapabilitySummary, Origin, ProcId, Time64, TrustDomain, WallClockReading, WallTimeState,
    ORIGIN_CONSOLE_NONE, PROC_ID_LEN,
};
use rustos_log::{
    reserved_source_prefix, CallerContent, DictionaryBuilder, DictionaryView, Ingress, Level,
    Stream, STREAM_COUNT,
};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 50_000;

/// Derive a bounded index in `0..n` from a PRNG word.
fn pick(word: u64, n: usize) -> usize {
    // `word % n` is in `0..n`, so the `usize` conversion cannot truncate.
    usize::try_from(word % n as u64).unwrap_or(0)
}

/// Map a PRNG word onto a `Stream`, or `None` for "no request".
fn maybe_stream(word: u64) -> Option<Stream> {
    match word % 7 {
        0 => None,
        1 => Some(Stream::Boot),
        2 => Some(Stream::Runtime),
        3 => Some(Stream::Debug),
        4 => Some(Stream::Security),
        5 => Some(Stream::Audit),
        _ => Some(Stream::Journal),
    }
}

/// Map a PRNG word onto a caller level, or `None`.
fn maybe_level(word: u64) -> Option<Level> {
    match word % 7 {
        0 => None,
        other => Level::from_u8(u8::try_from(other - 1).unwrap_or(0)),
    }
}

/// A small pool of requested-source strings: reserved-namespace spoofs, a
/// plausible legitimate name, and the empty string.
const REQUESTED_SOURCES: [&str; 8] = [
    "kernel.mem",
    "audit.login",
    "security.mac",
    "service.devmgr",
    "system.time",
    "driver.net",
    "dhcp",
    "",
];

/// A small pool of subsystem labels: valid, malformed, and empty.
const SUBSYSTEMS: [&str; 5] = ["mem", "net", "Bad.Label", "", "a b"];

fn exercise(word0: u64, word1: u64, ingress: &mut Ingress) {
    let kernel = word0 & 1 == 0;
    let origin = if kernel {
        Origin::new(
            TrustDomain::Kernel,
            0,
            0,
            1,
            ProcId::KERNEL,
            CapabilitySummary::EMPTY,
            ORIGIN_CONSOLE_NONE,
        )
    } else {
        let uid = u32::try_from((word0 >> 1) & 0xFFFF_FFFF).unwrap_or(0);
        Origin::new(
            TrustDomain::User,
            uid,
            uid,
            42,
            ProcId::from_raw([(word0 >> 8).to_le_bytes()[0]; PROC_ID_LEN]),
            CapabilitySummary::EMPTY,
            ORIGIN_CONSOLE_NONE,
        )
    };

    let subsystem = if pick(word0 >> 2, 3) == 0 {
        None
    } else {
        Some(SUBSYSTEMS[pick(word0 >> 4, SUBSYSTEMS.len())])
    };
    let requested_stream = maybe_stream(word1);
    let requested_source = if pick(word1 >> 4, 3) == 0 {
        None
    } else {
        Some(REQUESTED_SOURCES[pick(word1 >> 8, REQUESTED_SOURCES.len())])
    };
    let caller_level = maybe_level(word1 >> 16);

    let adm = ingress.admit(
        &origin,
        subsystem,
        requested_stream,
        requested_source,
        caller_level,
    );

    // Invariant 1: a user origin can never obtain a privileged stream.
    if !kernel {
        assert!(
            !adm.stream().requires_trusted_emitter(),
            "user origin escalated to a privileged stream"
        );
        // Invariant 2: a user origin's authoritative source never lands in a
        // reserved namespace, whatever it requested.
        assert!(
            reserved_source_prefix(adm.source().as_str()).is_none(),
            "user origin obtained a reserved source: {}",
            adm.source().as_str()
        );
    }

    // Invariant 3: the requested-source spoof flag matches the screen exactly.
    let expect_source_spoof = requested_source.is_some_and(|s| reserved_source_prefix(s).is_some());
    assert_eq!(adm.source_spoofed(), expect_source_spoof);

    // Invariant 4: after admit, the resolved stream's next-seq is exactly
    // one past the seq handed out.
    assert_eq!(ingress.next_seq(adm.stream()), adm.seq().saturating_add(1));

    // Invariant 5: the built record encodes and decodes to the same attested
    // facts, so ingress never produces an unencodable body.
    let caller = CallerContent {
        level: caller_level,
        component: Some("c"),
        tag: None,
        event_id: None,
        requested_source,
        requested_stream,
        message: "m",
    };
    let record = adm.build_record(
        word1,
        WallClockReading::new(Time64::from_secs(1_700_000_000), WallTimeState::Trusted),
        caller,
        &[],
    );
    let mut buf = [0u8; 512];
    let n = record
        .encode(&mut buf, &mut DictionaryBuilder::new())
        .expect("admitted record always encodes");
    let mut view = DictionaryView::new();
    let decoded = rustos_log::decode_record(&buf[..n], &mut view).expect("round-trips");
    assert_eq!(decoded.effective_level(), adm.effective_level());
    assert_eq!(decoded.source_name(), adm.source().as_str());
    assert_eq!(decoded.cpu_seq(), word1);
}

#[test]
fn ingress_admission_never_panics_and_holds_invariants() {
    let mut rng = rustos_fuzzseed::Lcg::new(rustos_fuzzseed::start(
        "ingress_admission_never_panics_and_holds_invariants",
        rustos_fuzzseed::FUZZ_SEED_ENV,
    ));

    // One long-lived ingress so the per-stream append sequences advance across
    // iterations; a fresh one occasionally, to also exercise genesis.
    let mut ingress = Ingress::new();
    let deadline = rustos_fuzzseed::budget_deadline(rustos_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for i in 0..SMOKE_ITERATIONS {
            if i % 4096 == 0 {
                // Occasionally resume from arbitrary seeds to cover `resume`
                // and non-zero starting sequences.
                let mut seeds = [0u64; STREAM_COUNT];
                for slot in &mut seeds {
                    *slot = rng.next_u64();
                }
                ingress = Ingress::resume(seeds);
            }
            exercise(rng.next_u64(), rng.next_u64(), &mut ingress);
        }
        if !rustos_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}
