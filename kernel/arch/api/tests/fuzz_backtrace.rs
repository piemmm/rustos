//! Deterministic fuzz harness for the arch-neutral frame-pointer
//! unwinder [`tairix_arch_api::backtrace::walk`].
//!
//! The unwinder runs inside the kernel panic handler, over a frame-pointer
//! chain that — precisely because the kernel is panicking — may be
//! arbitrarily corrupt: a wild frame pointer, a cycle, an unaligned value,
//! a chain that never terminates. A naive `*(fp)` walk over such a chain is
//! a fault inside the fault handler — a triple fault. The walk therefore
//! validates every candidate frame pointer (non-null, 8-aligned, strictly
//! monotonic, both read words wholly within the supplied stack bounds)
//! before dereferencing, is depth-capped, and reads memory only through a
//! [`StackReader`]. Per ("every parser of untrusted input ... has a fuzz
//! target") that walk is driven here against adversarial chains, with two
//! invariants that a violation aborts the run (the abort *is* the failure):
//!
//! * **Never reads out of the bounds it was given.** The mock reader
//!   asserts every `read_word` address lies within the backing store and
//!   inside the walk's declared bounds. A read outside is the
//!   fault-in-fault-handler bug this harness exists to catch.
//! * **Always terminates**, emitting at most
//!   [`tairix_arch_api::backtrace::MAX_FRAMES`] frames, whatever the fp,
//!   layout, bounds, or memory contents.
//!
//! The reader is [fallible](StackReader::read_word): a user-fault
//! backtrace walks the crashing task's *untrusted* stack, where a
//! structurally valid address can still be unmapped and the copy-in read
//! fails. The harness therefore also, on a per-iteration random cut,
//! returns [`None`] from a point in the address space onward, so the
//! `None`-terminated walk path is fuzzed for the same two invariants: a
//! failed read ends the walk cleanly and never provokes an out-of-bounds
//! read.
//!
//! No external fuzz runner: a per-run-seeded LCG (seed drawn and logged by
//! `tairix_fuzzseed`) fills a backing "stack" with random words and drives
//! the walk from random start frame pointers with random frame layouts and
//! random (possibly degenerate) bounds. A plain `cargo test` runs the fixed
//! smoke sweep; `cargo xtask fuzz` extends the loop to a wall-clock budget.

use tairix_arch_api::backtrace::{walk, FrameLayout, StackBounds, StackReader, MAX_FRAMES};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 200_000;

/// Number of 64-bit words in the backing "stack" the walker reads.
const WORDS: usize = 512;

/// [`WORDS`] as a `u64`, without a fallible cast in the hot loop.
const fn words_u64() -> u64 {
    WORDS as u64
}

/// A backing store the walker reads through. Every `read_word` address is
/// asserted to lie within the store *and* within the bounds the walk was
/// given — either failure is the out-of-bounds read the walk must never
/// perform.
struct FuzzStack {
    base: u64,
    words: [u64; WORDS],
    bounds: StackBounds,
    /// Addresses at or above this are "unmapped": [`read_word`] returns
    /// [`None`] for them, modelling an untrusted user stack whose page is
    /// not resident. `u64::MAX` means "every in-bounds word is readable".
    unreadable_from: u64,
}

impl StackReader for FuzzStack {
    fn read_word(&self, addr: u64) -> Option<u64> {
        assert!(
            self.bounds.contains_word(addr),
            "walk read {addr:#x} outside the bounds it was given"
        );
        assert_eq!(addr % 8, 0, "walk read a non-8-aligned address {addr:#x}");
        let end = self.base + words_u64() * 8;
        assert!(
            addr >= self.base && addr + 8 <= end,
            "walk read {addr:#x} outside the backing store [{:#x},{:#x})",
            self.base,
            end
        );
        // Model an unmapped user page: the bounds/alignment asserts above
        // still fire (the walk must honour bounds even for a word it will
        // then fail to read), but the value is unavailable.
        if addr >= self.unreadable_from {
            return None;
        }
        let idx = word_index(addr, self.base);
        Some(self.words[idx])
    }
}

/// `x` reduced into `0..=max`, without a narrowing `as` cast.
fn bounded(x: u64, max: u64) -> u64 {
    x % (max.saturating_add(1))
}

/// Low 16 bits of `x` as an `i16`, without a narrowing `as` cast (the
/// workspace denies `cast_possible_truncation`).
fn i16_of(x: u64) -> i16 {
    let [b0, b1, ..] = x.to_le_bytes();
    i16::from_le_bytes([b0, b1])
}

/// Word index for an in-store address, without a narrowing `as` cast.
fn word_index(addr: u64, base: u64) -> usize {
    usize::try_from((addr - base) / 8).unwrap_or(0)
}

#[test]
fn walking_any_frame_chain_terminates_and_stays_in_bounds() {
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    let mut state: u64 = tairix_fuzzseed::start(
        "walking_any_frame_chain_terminates_and_stays_in_bounds",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    );
    let mut next = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        state
    };

    // A fixed, aligned base for the backing store's address space. Using a
    // non-zero base exercises the offset arithmetic (a low base would make
    // an underflowing negative offset look like a wrap rejection rather
    // than a genuine bounds miss).
    let base: u64 = 0x1_0000_0000;

    let mut iteration: u64 = 0;
    loop {
        // Random backing memory: every word is a potential caller-fp or
        // return address, so the chain the walk follows is arbitrary.
        let mut words = [0u64; WORDS];
        for w in &mut words {
            // Bias some words toward plausible in-store frame pointers so
            // the walk sometimes makes progress rather than always failing
            // the first check; others stay fully random.
            if next() & 1 == 0 {
                let widx = bounded(next(), words_u64() - 1);
                *w = base + widx * 8;
            } else {
                *w = next();
            }
        }

        // Random, possibly degenerate, bounds — always a sub-range of the
        // backing store so the reader's store assertion is a true "did the
        // walk honour the bounds" check, never a harness lie.
        let lo_idx = bounded(next(), words_u64() - 1);
        let hi_idx = bounded(next(), words_u64());
        let low = base + lo_idx * 8;
        let high = base + hi_idx * 8;
        let bounds = StackBounds::new(low, high);

        // Random layout: both signs, arbitrary magnitudes (including ones
        // that will wrap and be rejected).
        let layout = FrameLayout {
            saved_fp_offset: i16_of(next()),
            return_addr_offset: i16_of(next() >> 16),
        };

        // Random start fp: sometimes a valid in-store aligned pointer,
        // sometimes wild / unaligned / null.
        let start_fp = match next() % 4 {
            0 => base + bounded(next(), words_u64() - 1) * 8, // aligned in-store
            1 => next(),                                      // fully wild
            2 => base + bounded(next(), words_u64() - 1) * 8 + 1, // unaligned
            _ => 0,                                           // null
        };

        // On roughly half of iterations, make the address space unreadable
        // from a random word onward so the fallible (None-terminated) walk
        // path is exercised; otherwise every in-bounds word is readable.
        let unreadable_from = if next() & 1 == 0 {
            base + bounded(next(), words_u64()) * 8
        } else {
            u64::MAX
        };

        let stack = FuzzStack {
            base,
            words,
            bounds,
            unreadable_from,
        };

        let mut emitted = 0usize;
        let n = walk(&stack, start_fp, layout, bounds, |_ra| {
            emitted += 1;
            assert!(
                emitted <= MAX_FRAMES,
                "walk emitted more than MAX_FRAMES frames"
            );
        });
        assert!(n <= MAX_FRAMES, "walk returned a count above MAX_FRAMES");
        assert_eq!(n, emitted, "returned count disagrees with emit calls");

        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
