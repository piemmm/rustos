//! Deterministic fuzz harness for the address vocabulary and checksum.
//!
//! Two property families over per-run-seeded pseudo-random inputs:
//!
//! - **Addresses**: [`Ipv6Scope::of`] and [`ScopedIpv6Addr::new`] are
//!   total over arbitrary 128-bit addresses and zone indices, never
//!   panic, and uphold the scope/zone invariants (a constructed scoped
//!   address carries a zone exactly when its scope is not global).
//! - **Checksum**: any split of a byte stream across incremental
//!   [`Checksum::push`] calls folds identically to the one-shot
//!   [`internet_checksum`], and the pseudo-header seeds fold identically
//!   to the equivalent contiguous buffer.
//!
//! ## Wall-clock budget
//!
//! A plain `cargo test` runs the [`SMOKE_ITERATIONS`] sweep once from a
//! fresh, logged seed so the suite stays fast. When `cargo xtask fuzz
//! --soak` exports `RUSTOS_FUZZ_BUDGET_SECS`, the PRNG-driven harness
//! keeps drawing fresh inputs from the *same continuing* stream until
//! the deadline elapses. The seed is logged at the start, so a
//! fresh-seed crash stays reproducible via `RUSTOS_FUZZ_SEED`.

use core::num::NonZeroU32;

use rustos_net::addr::{Ipv6Scope, ScopedIpv6Addr};
use rustos_net::checksum::Checksum;
use rustos_net::{internet_checksum, Ipv4Addr, Ipv6Addr};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 100_000;

/// Lehmer-style LCG — deterministic, no allocator. Identical to the
/// generator in `lib/abi/tests/fuzz_decode.rs` so the two harnesses share
/// one reproducible-failure story.
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

fn exercise_scope(rng: &mut Lcg) {
    let mut octets = [0u8; 16];
    rng.fill(&mut octets);
    // Bias half the draws into the interesting prefixes so multicast and
    // link-local paths are hit constantly, not once in 2^8 draws.
    match rng.next_u64() & 0x3 {
        0 => octets[0] = 0xFF,                      // multicast
        1 => (octets[0], octets[1]) = (0xFE, 0x80), // link-local
        _ => {}
    }
    let addr = Ipv6Addr::from(octets);
    let scope = Ipv6Scope::of(&addr);
    let zone = NonZeroU32::new((rng.next_u64() & 0xFFFF_FFFF) as u32);
    // A refusal is the fail-closed path; reaching past the call at all
    // proves "no panic", so only the accepted case has invariants.
    if let Ok(scoped) = ScopedIpv6Addr::new(addr, zone) {
        let scope = scope.expect("a constructed address must have a scope");
        assert_eq!(scoped.scope(), scope);
        // The zone is present exactly when the scope is not global.
        assert_eq!(scoped.zone().is_some(), scope != Ipv6Scope::Global);
        assert_eq!(scoped.addr(), addr);
    }
}

fn exercise_checksum(rng: &mut Lcg, buf: &mut [u8]) {
    let len = ((rng.next_u64() & 0xFF) as usize) % (buf.len() + 1);
    let data = &mut buf[..len];
    rng.fill(data);
    let expected = internet_checksum(data);

    // Any two split points must fold identically to the one-shot.
    let a = ((rng.next_u64() & 0xFF) as usize) % (len + 1);
    let b = a + ((rng.next_u64() & 0xFF) as usize) % (len - a + 1);
    let mut sum = Checksum::new();
    sum.push(&data[..a]);
    sum.push(&data[a..b]);
    sum.push(&data[b..]);
    assert_eq!(sum.finish(), expected);

    // The pseudo-header seeds must equal the contiguous equivalent.
    let upper_len16 = u16::try_from(len).expect("buffer is shorter than u16::MAX");
    let upper_len32 = u32::from(upper_len16);
    let src = Ipv4Addr::from(((rng.next_u64() & 0xFFFF_FFFF) as u32).to_be_bytes());
    let dst = Ipv4Addr::from(((rng.next_u64() & 0xFFFF_FFFF) as u32).to_be_bytes());
    let protocol = (rng.next_u64() & 0xFF) as u8;
    let mut seeded = Checksum::ipv4_pseudo(src, dst, protocol, upper_len16);
    seeded.push(data);
    let mut contiguous = Vec::new();
    contiguous.extend_from_slice(&src.octets());
    contiguous.extend_from_slice(&dst.octets());
    contiguous.extend_from_slice(&[0, protocol]);
    contiguous.extend_from_slice(&upper_len16.to_be_bytes());
    contiguous.extend_from_slice(data);
    assert_eq!(seeded.finish(), internet_checksum(&contiguous));

    let mut v6 = [0u8; 16];
    rng.fill(&mut v6);
    let src6 = Ipv6Addr::from(v6);
    rng.fill(&mut v6);
    let dst6 = Ipv6Addr::from(v6);
    let next_header = (rng.next_u64() & 0xFF) as u8;
    let mut seeded = Checksum::ipv6_pseudo(src6, dst6, next_header, upper_len32);
    seeded.push(data);
    let mut contiguous = Vec::new();
    contiguous.extend_from_slice(&src6.octets());
    contiguous.extend_from_slice(&dst6.octets());
    contiguous.extend_from_slice(&upper_len32.to_be_bytes());
    contiguous.extend_from_slice(&[0, 0, 0, next_header]);
    contiguous.extend_from_slice(data);
    assert_eq!(seeded.finish(), internet_checksum(&contiguous));
}

#[test]
fn random_inputs_uphold_address_and_checksum_invariants() {
    let mut rng = Lcg::new(rustos_fuzzseed::start(
        "random_inputs_uphold_address_and_checksum_invariants",
        rustos_fuzzseed::FUZZ_SEED_ENV,
    ));
    let mut buf = [0u8; 128];
    let deadline = rustos_fuzzseed::budget_deadline(rustos_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            exercise_scope(&mut rng);
            exercise_checksum(&mut rng, &mut buf);
        }
        if !rustos_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}
