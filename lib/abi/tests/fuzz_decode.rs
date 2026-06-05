//! Deterministic fuzz-style integration test for the ABI decoders.
//!
//! Every decoder in `lib/abi` accepts an arbitrary byte slice from a
//! possibly hostile peer; per `AGENTS.md` §10 the right way to drive it is
//! a fuzz harness. This file is the smoke harness that runs in
//! `cargo test`: a deterministic 64-bit LCG generates 100 000 short
//! pseudo-random inputs and asserts the decoders refuse them cleanly
//! without panicking and without ever producing an `Ok` result that
//! disagrees with the round-trip encoder.
//!
//! The same set of decoder functions is the entry point the `cargo xtask
//! fuzz` orchestrator drives for ≥ 60 s per PR (`AGENTS.md` §19.6); the
//! helper [`exercise`] keeps the contract centralised so the two cannot
//! drift.
//!
//! ## Wall-clock budget (`AGENTS.md` §19.6)
//!
//! A plain `cargo test` runs the fixed [`SMOKE_ITERATIONS`] sweep so the
//! suite stays fast and deterministic. When `cargo xtask fuzz` sets
//! `RUSTOS_FUZZ_BUDGET_SECS`, [`budget`] returns a deadline and the
//! PRNG-driven harness keeps drawing fresh inputs from the *same
//! continuing* stream until it elapses — the §19.6 "run each harness for
//! ≥ 60 s" contract. The seed is fixed, so a crash at draw N stays
//! reproducible regardless of how far a given machine got. The
//! bit-flip harness is an exhaustive boundary sweep, not a random one,
//! so it runs once regardless of the budget.

use rustos_abi::input::{KeyInput, PointerInput};
use rustos_abi::process::{ProcessStart, ProcessStartHeader, StringSlot};
use rustos_abi::sysinfo::{
    KernelMemoryStats, MountListRequest, MountRecord, ProcessListRequest, ProcessRecord,
    SysinfoRequestHeader, SystemIdentity, Uptime,
};
use rustos_abi::time::{Duration64, Time64};
use rustos_abi::{
    AppInfoHeader, IpcMessageHeader, LoadImage, ManifestHeader, NeededLibrary, PortName,
    SYSCALL_TABLE_HASH_LEN,
};

/// Fixed CFI tag fed to [`LoadImage::parse`] in the harness. A random input
/// is overwhelmingly unlikely to match it, so the loader fails closed long
/// before mapping anything; the point is that no input panics.
const FUZZ_CFI_TAG: [u8; SYSCALL_TABLE_HASH_LEN] = [0u8; SYSCALL_TABLE_HASH_LEN];

/// Fixed-iteration sweep run by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 100_000;

/// Deadline for the current run, or `None` for the fixed smoke sweep.
///
/// `cargo xtask fuzz` exports `RUSTOS_FUZZ_BUDGET_SECS`; a positive value
/// turns the PRNG-driven harness into a wall-clock loop. An unset,
/// empty, zero, or unparsable value preserves the deterministic smoke
/// behaviour.
fn budget() -> Option<std::time::Instant> {
    let secs: u64 = std::env::var("RUSTOS_FUZZ_BUDGET_SECS")
        .ok()?
        .parse()
        .ok()?;
    if secs == 0 {
        return None;
    }
    Some(std::time::Instant::now() + std::time::Duration::from_secs(secs))
}

/// `true` while the wall-clock budget has time left; always `false` for
/// the fixed smoke sweep so the loop body runs exactly once.
fn within_budget(deadline: Option<std::time::Instant>) -> bool {
    matches!(deadline, Some(end) if std::time::Instant::now() < end)
}

/// Drive every ABI decoder on `bytes`.
///
/// Returns silently. The contract is "must not panic for any input"; a
/// successful decode is additionally required to round-trip through its
/// matching encoder.
fn exercise(bytes: &[u8]) {
    if let Ok(header) = IpcMessageHeader::from_bytes(bytes) {
        let encoded = header.to_le_bytes();
        let redecoded = IpcMessageHeader::from_bytes(&encoded)
            .expect("round-trip of an accepted header must succeed");
        assert_eq!(header, redecoded);
    }
    if let Ok(header) = ManifestHeader::from_bytes(bytes) {
        let encoded = header.to_le_bytes();
        let redecoded = ManifestHeader::from_bytes(&encoded)
            .expect("round-trip of an accepted header must succeed");
        assert_eq!(header, redecoded);
    }
    if let Ok(header) = AppInfoHeader::from_bytes(bytes) {
        let redecoded = AppInfoHeader::from_bytes(&header.to_le_bytes())
            .expect("round-trip of an accepted header must succeed");
        assert_eq!(header, redecoded);
    }
    if let Ok(header) = SysinfoRequestHeader::from_bytes(bytes) {
        let redecoded = SysinfoRequestHeader::from_bytes(&header.to_le_bytes())
            .expect("round-trip of an accepted header must succeed");
        assert_eq!(header, redecoded);
    }
    if let Ok(req) = ProcessListRequest::from_bytes(bytes) {
        let redecoded = ProcessListRequest::from_bytes(&req.to_le_bytes())
            .expect("round-trip of an accepted request must succeed");
        assert_eq!(req, redecoded);
    }
    if let Ok(req) = MountListRequest::from_bytes(bytes) {
        let redecoded = MountListRequest::from_bytes(&req.to_le_bytes())
            .expect("round-trip of an accepted request must succeed");
        assert_eq!(req, redecoded);
    }
    if let Ok(rec) = MountRecord::from_bytes(bytes) {
        let redecoded = MountRecord::from_bytes(&rec.to_le_bytes())
            .expect("round-trip of an accepted record must succeed");
        assert_eq!(rec, redecoded);
    }
    if let Ok(rec) = ProcessRecord::from_bytes(bytes) {
        let redecoded = ProcessRecord::from_bytes(&rec.to_le_bytes())
            .expect("round-trip of an accepted record must succeed");
        assert_eq!(rec, redecoded);
    }
    if let Ok(stats) = KernelMemoryStats::from_bytes(bytes) {
        let redecoded = KernelMemoryStats::from_bytes(&stats.to_le_bytes())
            .expect("round-trip of accepted stats must succeed");
        assert_eq!(stats, redecoded);
    }
    if let Ok(up) = Uptime::from_bytes(bytes) {
        let redecoded = Uptime::from_bytes(&up.to_le_bytes())
            .expect("round-trip of an accepted uptime must succeed");
        assert_eq!(up, redecoded);
    }
    if let Ok(id) = SystemIdentity::from_bytes(bytes) {
        let redecoded = SystemIdentity::from_bytes(&id.to_le_bytes())
            .expect("round-trip of an accepted identity must succeed");
        assert_eq!(id, redecoded);
    }
    if let Ok(time) = Time64::from_bytes(bytes) {
        let redecoded = Time64::from_bytes(&time.to_le_bytes())
            .expect("round-trip of an accepted instant must succeed");
        assert_eq!(time, redecoded);
    }
    if let Ok(duration) = Duration64::from_bytes(bytes) {
        let redecoded = Duration64::from_bytes(&duration.to_le_bytes())
            .expect("round-trip of an accepted duration must succeed");
        assert_eq!(duration, redecoded);
    }
    if let Ok(event) = PointerInput::from_bytes(bytes) {
        let redecoded = PointerInput::from_bytes(&event.to_le_bytes())
            .expect("round-trip of an accepted pointer event must succeed");
        assert_eq!(event, redecoded);
    }
    if let Ok(event) = KeyInput::from_bytes(bytes) {
        let redecoded = KeyInput::from_bytes(&event.to_le_bytes())
            .expect("round-trip of an accepted key event must succeed");
        assert_eq!(event, redecoded);
    }
    if let Ok(name) = PortName::from_bytes(bytes) {
        let redecoded = PortName::from_bytes(&name.to_le_bytes())
            .expect("round-trip of an accepted port name must succeed");
        assert_eq!(name, redecoded);
    }
    if let Ok(lib) = NeededLibrary::decode(bytes) {
        let redecoded = NeededLibrary::decode(&lib.to_le_bytes())
            .expect("round-trip of an accepted needed-library record must succeed");
        assert_eq!(lib, redecoded);
    }
    // The whole-image loader has no single round-trip encoder (the builder is
    // test-only), so the contract here is the §19.6 "must not panic for any
    // input"; an accepted image must additionally re-parse deterministically
    // and yield resolvable needed-library references.
    if let Ok(image) = LoadImage::parse(bytes, &FUZZ_CFI_TAG) {
        let reparsed = LoadImage::parse(bytes, &FUZZ_CFI_TAG)
            .expect("re-parse of an accepted load image must succeed");
        assert_eq!(image, reparsed);
        for name in image.needed_libraries() {
            assert!(!name.is_empty());
        }
    }
    exercise_process(bytes);
}

/// Drive the `process` startup-vector decoders on `bytes`.
///
/// Split out of [`exercise`] so each helper stays a single, readable unit;
/// the contract is identical (must not panic; an accepted decode round-trips
/// or re-parses deterministically).
fn exercise_process(bytes: &[u8]) {
    if let Ok(header) = ProcessStartHeader::from_bytes(bytes) {
        let redecoded = ProcessStartHeader::from_bytes(&header.to_le_bytes())
            .expect("round-trip of an accepted start header must succeed");
        assert_eq!(header, redecoded);
    }
    if let Ok(slot) = StringSlot::from_bytes(bytes) {
        let redecoded = StringSlot::from_bytes(&slot.to_le_bytes())
            .expect("round-trip of an accepted string slot must succeed");
        assert_eq!(slot, redecoded);
    }
    if let Ok(view) = ProcessStart::parse(bytes) {
        // The view borrows `bytes`; re-parsing the same bytes must be
        // deterministic, and every accepted string must resolve.
        let reparsed = ProcessStart::parse(bytes)
            .expect("re-parse of an accepted startup vector must succeed");
        assert_eq!(view, reparsed);
        for i in 0..view.arg_count() {
            assert!(view.arg(i).is_some());
        }
        for i in 0..view.env_count() {
            assert!(view.env(i).is_some());
        }
    }
    exercise_process_builder(bytes);
}

/// Drive the production startup-vector *builder* on `bytes` (§19.6).
///
/// The fuzz bytes are split on `0xFF` into argument/environment strings and
/// fed to [`rustos_abi::process::write_into`]; an accepted build must parse
/// back to exactly those strings, and a rejected build (e.g. an embedded NUL)
/// must fail closed rather than panic.
fn exercise_process_builder(bytes: &[u8]) {
    let mut parts: Vec<&[u8]> = bytes.split(|&b| b == 0xFF).collect();
    // Keep the builder cheap and comfortably within the abi-v1 limits.
    parts.truncate(8);
    let split = parts.len() / 2;
    let (args, env) = parts.split_at(split);

    let mut seed = [0u8; 8];
    let take = core::cmp::min(8, bytes.len());
    seed[..take].copy_from_slice(&bytes[..take]);
    let canary = u64::from_le_bytes(seed);

    let Ok(len) = rustos_abi::process::encoded_len(args, env) else {
        return;
    };
    let mut buf = vec![0u8; len];
    let Ok(written) = rustos_abi::process::write_into(&mut buf, args, env, canary) else {
        // A rejected build (an embedded NUL, say) is a fail-closed outcome.
        return;
    };
    assert_eq!(written, len);
    let view = ProcessStart::parse(&buf).expect("a freshly built block must parse");
    assert_eq!(view.arg_count() as usize, args.len());
    assert_eq!(view.env_count() as usize, env.len());
    assert_eq!(view.canary(), canary);
    let mut idx: u32 = 0;
    for a in args {
        assert_eq!(view.arg(idx), Some(*a));
        idx += 1;
    }
    idx = 0;
    for e in env {
        assert_eq!(view.env(idx), Some(*e));
        idx += 1;
    }
}

/// Lehmer LCG (Park–Miller) — deterministic, no_std, no allocator.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        // Seed must not collapse the multiplicative recurrence to zero.
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    fn next_u64(&mut self) -> u64 {
        // Multiplier from Steele/Vigna's PCG paper; full-period.
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
fn random_short_inputs_never_panic() {
    let mut rng = Lcg::new(0xCAFE_F00D_DEAD_BEEF);
    let mut buf = [0u8; 256];
    let deadline = budget();
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            // Random size in [0, buf.len()].
            // Mask to a width that fits any usize then range-reduce. The
            // bitmask makes the cast lossless without depending on
            // target-pointer width.
            let size = ((rng.next_u64() & 0xFFFF) as usize) % (buf.len() + 1);
            rng.fill(&mut buf[..size]);
            exercise(&buf[..size]);
        }
        if !within_budget(deadline) {
            break;
        }
    }
}

#[test]
fn structured_inputs_with_corrupted_fields_never_panic() {
    // Start from a well-formed IPC header, then bit-flip individual bytes
    // to walk the boundary between accepted and rejected.
    let mut base = IpcMessageHeader {
        magic: rustos_abi::IPC_MESSAGE_HEADER_MAGIC,
        version: 1,
        flags: 0,
        endpoint: 0xDEAD_BEEF_CAFE_F00D,
        sender: 0,
        payload_len: 16,
        reserved: 0,
    }
    .to_le_bytes();
    for byte in 0..base.len() {
        for bit in 0..8u32 {
            base[byte] ^= 1 << bit;
            exercise(&base);
            base[byte] ^= 1 << bit;
        }
    }
}
