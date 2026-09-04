//! Deterministic fuzz harness for `lib/hash`.
//!
//! Both hashers take attacker-controlled bytes and lengths — a filename off a
//! foreign volume, a DNS name, a 5-tuple — so the invariants that matter for
//! any input at all are:
//!
//! 1. Neither hasher panics, whatever the input bytes or length.
//! 2. The digest is independent of how the caller chunked its writes: an
//!    arbitrary split into any number of pieces agrees with the one-shot hash
//!    over the concatenation. A container whose lookups fed the key in a
//!    different shape from its inserts would miss its own entries.
//! 3. The digest depends on the whole input: appending a byte changes it, and
//!    so does changing the key.
//!
//! Runs the fixed smoke sweep under plain `cargo test`; keeps drawing from
//! the same seeded stream until `TAIRIX_FUZZ_BUDGET_SECS` elapses under
//! `cargo xtask fuzz`.

use core::hash::Hasher;

use tairix_fuzzseed::Lcg;
use tairix_hash::{FastHash, HashSeed, SipHash13};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 20_000;

/// Longest input drawn. Comfortably past the 32-byte stripe and the 8-byte
/// word so every buffering boundary is exercised.
const MAX_LEN: usize = 300;

/// Draw an input of a random length, biased towards the short lengths where
/// the tail-handling branches live.
fn draw_input(rng: &mut Lcg, buf: &mut Vec<u8>) {
    let len = match rng.below(4) {
        0 => rng.below(9),
        1 => rng.below(40),
        _ => rng.below(MAX_LEN),
    };
    buf.resize(len, 0);
    rng.fill(buf);
}

/// Hash `input` by splitting it at random boundaries, so a chunking that
/// disagrees with the one-shot is caught.
fn hash_in_pieces(seed: HashSeed, fast_seed: u64, input: &[u8], rng: &mut Lcg) -> (u64, u64) {
    let mut sip = SipHash13::new(seed);
    let mut fast = FastHash::with_seed(fast_seed);
    let mut rest = input;
    while !rest.is_empty() {
        let take = 1 + rng.below(rest.len());
        let (piece, tail) = rest.split_at(take);
        sip.write(piece);
        fast.write(piece);
        rest = tail;
    }
    (sip.finish(), fast.finish())
}

fn check(seed: HashSeed, fast_seed: u64, input: &[u8], rng: &mut Lcg) {
    let sip_once = SipHash13::hash_bytes(seed, input);
    let fast_once = FastHash::hash_bytes(fast_seed, input);

    let (sip_pieces, fast_pieces) = hash_in_pieces(seed, fast_seed, input, rng);
    assert_eq!(sip_once, sip_pieces, "SipHash-1-3 chunking disagrees");
    assert_eq!(fast_once, fast_pieces, "XXH64 chunking disagrees");

    // Appending a byte must move the digest: a hash that ignored its tail
    // would collide every key sharing a prefix.
    let mut longer = input.to_vec();
    longer.push(0x5a);
    assert_ne!(
        sip_once,
        SipHash13::hash_bytes(seed, &longer),
        "SipHash-1-3 ignored an appended byte"
    );
    assert_ne!(
        fast_once,
        FastHash::hash_bytes(fast_seed, &longer),
        "XXH64 ignored an appended byte"
    );

    // The key must matter, or the collision-flooding defence is not there.
    let (k0, k1) = seed.words();
    let other = HashSeed::from_words(k0 ^ 1, k1);
    assert_ne!(
        sip_once,
        SipHash13::hash_bytes(other, input),
        "SipHash-1-3 ignored its key"
    );
}

#[test]
fn arbitrary_input_hashes_consistently() {
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "arbitrary_input_hashes_consistently",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    let mut buf = Vec::new();
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            let seed = HashSeed::from_words(rng.next_u64(), rng.next_u64());
            let fast_seed = rng.next_u64();
            draw_input(&mut rng, &mut buf);
            check(seed, fast_seed, &buf, &mut rng);
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}
