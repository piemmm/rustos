//! Constant-time comparison of secret byte strings.
//!
//! Comparing a secret — a MAC tag, a capability-token signature, a key
//! fingerprint — against an attacker-supplied value with the `==` operator
//! leaks, through early-exit timing, how many leading bytes matched. That is
//! enough to forge the value one byte at a time. The function here compares
//! in time that depends only on the *lengths* of the inputs (which are public)
//! and never on their contents.
//!
//! This is the one sanctioned home for secret comparison in RustOS: callers
//! must never reintroduce `==`, `Ord`, or a short-circuiting loop over secret
//! material.

/// Compare two byte slices for equality in constant time with respect to
/// their contents.
///
/// Returns `true` iff `a` and `b` have the same length and the same bytes.
/// The work performed depends only on `a.len()` and `b.len()` — never on
/// *where* (or whether) the contents differ: every overlapping byte pair is
/// folded into a single difference accumulator with no data-dependent branch
/// and no early return.
///
/// The length check is deliberately *not* constant-time in the lengths
/// themselves; input lengths are public (a tag or digest has a fixed, known
/// size), and a mismatched length is an immediate, content-independent
/// rejection.
#[must_use]
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && fold_difference(a.iter().copied().zip(b.iter().copied())) == 0
}

/// Fold a sequence of byte pairs into a single difference accumulator that is
/// zero iff every pair was equal.
///
/// This is the constant-time core of [`ct_eq`], kept as a separate, generic
/// function for two reasons. First, it makes the no-early-exit property
/// explicit: a [`Iterator::fold`] consumes the *entire* sequence — there is
/// no `break` or `?` that could short-circuit on the first differing byte.
/// Second, it lets the unit tests verify that property directly by driving
/// the fold with an instrumented iterator that counts how many pairs it
/// yields, independent of the public length pre-check in [`ct_eq`].
fn fold_difference<I: Iterator<Item = (u8, u8)>>(pairs: I) -> u8 {
    pairs.fold(0u8, |acc, (x, y)| acc | (x ^ y))
}

#[cfg(test)]
mod tests {
    use super::{ct_eq, fold_difference};

    /// `splitmix64` finaliser — a self-contained deterministic PRNG so the
    /// randomised cases need no external crate and never flake.
    fn next(state: &mut u64) -> u64 {
        *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// An iterator wrapper that records, in a borrowed counter, how many
    /// items the consumer actually pulled. Driving [`fold_difference`] with
    /// it turns "did the fold visit every byte regardless of content?" into
    /// a deterministic assertion — the non-flaky stand-in for a timing
    /// measurement (the charter forbids wall-clock timing tests).
    struct Counting<'a, I> {
        inner: I,
        pulled: &'a mut usize,
    }

    impl<I: Iterator> Iterator for Counting<'_, I> {
        type Item = I::Item;

        fn next(&mut self) -> Option<Self::Item> {
            let item = self.inner.next();
            if item.is_some() {
                *self.pulled += 1;
            }
            item
        }
    }

    fn count_pairs(a: &[u8], b: &[u8]) -> usize {
        let mut pulled = 0usize;
        let pairs = Counting {
            inner: a.iter().copied().zip(b.iter().copied()),
            pulled: &mut pulled,
        };
        let _ = fold_difference(pairs);
        pulled
    }

    #[test]
    fn matches_slice_equality_for_equal_inputs() {
        let a = [0x11u8, 0x22, 0x33, 0x44];
        assert!(ct_eq(&a, &a));
        assert!(ct_eq(&[], &[]));
    }

    #[test]
    fn rejects_different_lengths_without_panicking() {
        assert!(!ct_eq(&[1, 2, 3], &[1, 2, 3, 4]));
        assert!(!ct_eq(&[], &[0]));
    }

    #[test]
    fn detects_a_difference_at_every_position() {
        let base = [0xA0u8, 0xB1, 0xC2, 0xD3, 0xE4, 0xF5, 0x06, 0x17];
        for i in 0..base.len() {
            let mut other = base;
            other[i] ^= 0x01;
            assert!(
                !ct_eq(&base, &other),
                "byte {i} flipped must compare unequal"
            );
        }
    }

    /// The constant-time invariant: the comparison visits every overlapping
    /// byte pair, whatever the contents — equal inputs, a difference in the
    /// first byte, a difference in the last byte, or an all-bytes difference
    /// all pull exactly `len` pairs. An early exit on the first mismatch
    /// (the timing leak we are defending against) would make the first-byte
    /// case pull just one pair, so this assertion would fail.
    #[test]
    fn traversal_is_independent_of_content() {
        const LEN: usize = 32;
        let equal_a = [0x5Au8; LEN];
        let equal_b = [0x5Au8; LEN];
        assert_eq!(count_pairs(&equal_a, &equal_b), LEN);

        let mut differ_first = equal_b;
        differ_first[0] ^= 0xFF;
        assert_eq!(count_pairs(&equal_a, &differ_first), LEN);

        let mut differ_last = equal_b;
        differ_last[LEN - 1] ^= 0xFF;
        assert_eq!(count_pairs(&equal_a, &differ_last), LEN);

        let all_differ = [0xA5u8; LEN];
        assert_eq!(count_pairs(&equal_a, &all_differ), LEN);
    }

    /// Randomised differential check against the reference `==`: for many
    /// fixed-seed random pairs (sometimes forced equal), `ct_eq` must agree
    /// with slice equality. Deterministic, so it can never flake.
    #[test]
    fn agrees_with_reference_equality() {
        let mut state = 0x0123_4567_89AB_CDEFu64;
        for _ in 0..4096 {
            let len = (next(&mut state) % 48) as usize;
            let mut a = [0u8; 48];
            let mut b = [0u8; 48];
            for byte in a.iter_mut().take(len) {
                *byte = (next(&mut state) & 0xFF) as u8;
            }
            // Half the time compare against an independent buffer; the other
            // half against a copy that is then perturbed with low
            // probability, so the suite exercises both equal and near-equal
            // inputs.
            if next(&mut state) & 1 == 0 {
                for byte in b.iter_mut().take(len) {
                    *byte = (next(&mut state) & 0xFF) as u8;
                }
            } else {
                b[..len].copy_from_slice(&a[..len]);
                if len > 0 && next(&mut state) % 4 == 0 {
                    let pos = usize::try_from(next(&mut state) % len as u64).expect("pos < len");
                    b[pos] ^= 1 + (next(&mut state) & 0x7F) as u8;
                }
            }
            assert_eq!(ct_eq(&a[..len], &b[..len]), a[..len] == b[..len]);
        }
    }
}
