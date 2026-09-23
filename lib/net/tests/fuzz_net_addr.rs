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
//! --soak` exports `TAIRIX_FUZZ_BUDGET_SECS`, the PRNG-driven harness
//! keeps drawing fresh inputs from the *same continuing* stream until
//! the deadline elapses. The seed is logged at the start, so a
//! fresh-seed crash stays reproducible via `TAIRIX_FUZZ_SEED`.

use core::num::NonZeroU32;

use tairix_fuzzseed::Prng;
use tairix_net::addr::{Ipv6Scope, ScopedIpv6Addr};
use tairix_net::checksum::Checksum;
use tairix_net::{internet_checksum, Ipv4Addr, Ipv6Addr};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 100_000;

fn exercise_scope(rng: &mut Prng) {
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
    let zone = NonZeroU32::new(rng.next_u32());
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

fn exercise_checksum(rng: &mut Prng, buf: &mut [u8]) {
    let len = rng.at_most(buf.len());
    let data = &mut buf[..len];
    rng.fill(data);
    let expected = internet_checksum(data);

    // Any two split points must fold identically to the one-shot.
    let a = rng.at_most(len);
    let b = a + rng.at_most(len - a);
    let mut sum = Checksum::new();
    sum.push(&data[..a]);
    sum.push(&data[a..b]);
    sum.push(&data[b..]);
    assert_eq!(sum.finish(), expected);

    // The pseudo-header seeds must equal the contiguous equivalent.
    let upper_len16 = u16::try_from(len).expect("buffer is shorter than u16::MAX");
    let upper_len32 = u32::from(upper_len16);
    let src = Ipv4Addr::from(rng.next_u32().to_be_bytes());
    let dst = Ipv4Addr::from(rng.next_u32().to_be_bytes());
    let protocol = rng.next_u8();
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
    let next_header = rng.next_u8();
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
    let mut rng = Prng::new(tairix_fuzzseed::start(
        "random_inputs_uphold_address_and_checksum_invariants",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let mut buf = [0u8; 128];
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            exercise_scope(&mut rng);
            exercise_checksum(&mut rng, &mut buf);
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}
