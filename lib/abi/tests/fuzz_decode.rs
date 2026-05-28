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
//! The same set of decoder functions is the entry point a `libfuzzer`-style
//! harness would call once Stage 2 wires `cargo fuzz` up; the helper
//! [`exercise`] keeps the contract centralised so the two cannot drift.

use rustos_abi::{IpcMessageHeader, ManifestHeader};

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
    for _ in 0..100_000 {
        // Random size in [0, buf.len()].
        // Mask to a width that fits any usize then range-reduce. The
        // bitmask makes the cast lossless without depending on
        // target-pointer width.
        let size = ((rng.next_u64() & 0xFFFF) as usize) % (buf.len() + 1);
        rng.fill(&mut buf[..size]);
        exercise(&buf[..size]);
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
