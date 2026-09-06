//! The statistical battery: NIST SP 800-22 tests, each reducing a sequence
//! to one p-value.
//!
//! No statistical test can distinguish a good PRNG from true randomness, so
//! none of these proves a generator sound. What they do is reject the
//! *structure* a broken one leaves behind, and [`ALL`] is chosen for the
//! kinds of structure a generator can plausibly acquire: a bias
//! ([`frequency`], [`block_frequency`], [`cumulative_sums_forward`]),
//! short-range correlation ([`runs`], [`longest_run_of_ones`]), linear
//! dependence over GF(2) — the signature of an LFSR-class generator —
//! ([`binary_matrix_rank`]), and compressibility ([`approximate_entropy`],
//! [`maurer_universal`]). Which is why every one of them is held to a
//! negative control it must reject; a battery that passes everything is
//! proving nothing.
//!
//! The spectral (DFT) test is deliberately absent: it needs an FFT for
//! detection power the rank and entropy tests already have.
//!
//! Parameters are fixed at [`SEQUENCE_BITS`] rather than chosen per run, so
//! a p-value means the same thing in every run and the block counts stay
//! inside each test's validity conditions.

// Counts, lengths, and block indices become `f64` throughout: these are
// statistics over a sequence of at most a few million bits, so every value is
// exactly representable in a double and the conversion is the arithmetic
// rather than a loss of it.
#![allow(clippy::cast_precision_loss)]

use crate::bits::BitSeq;
use crate::special::{chi_square_q, erfc, gamma_q, normal_cdf};

/// Bits in one tested sequence: 64 KiB.
///
/// Chosen as the smallest power of two that satisfies every test's own
/// minimum at once — the binding one is [`maurer_universal`], which needs
/// upwards of 387 840 bits at `L = 6`.
pub const SEQUENCE_BITS: usize = 1 << 19;

/// Bytes a generator produces for one tested sequence.
pub const SEQUENCE_BYTES: usize = SEQUENCE_BITS / 8;

/// One test: a name for reporting and the statistic itself.
pub struct Statistic {
    /// Stable identifier, used in the accumulator and in failure messages.
    pub name: &'static str,
    /// Reduce a sequence to its p-value.
    pub p_value: fn(BitSeq<'_>) -> f64,
}

/// The battery, in reporting order.
///
/// Cumulative sums appears twice because SP 800-22 defines it as one test
/// yielding two p-values (a forward and a backward walk). Keeping them
/// separate matters: each is uniform under the null hypothesis on its own,
/// where any combination of the two would not be.
pub const ALL: &[Statistic] = &[
    Statistic {
        name: "frequency",
        p_value: frequency,
    },
    Statistic {
        name: "block-frequency",
        p_value: block_frequency,
    },
    Statistic {
        name: "runs",
        p_value: runs,
    },
    Statistic {
        name: "longest-run",
        p_value: longest_run_of_ones,
    },
    Statistic {
        name: "matrix-rank",
        p_value: binary_matrix_rank,
    },
    Statistic {
        name: "approximate-entropy",
        p_value: approximate_entropy,
    },
    Statistic {
        name: "cusum-forward",
        p_value: cumulative_sums_forward,
    },
    Statistic {
        name: "cusum-backward",
        p_value: cumulative_sums_backward,
    },
    Statistic {
        name: "maurer-universal",
        p_value: maurer_universal,
    },
];

/// Frequency (monobit): are there as many ones as zeros?
///
/// The most basic bias check, and the one every other test assumes has
/// passed.
#[must_use]
pub fn frequency(seq: BitSeq<'_>) -> f64 {
    let n = seq.len();
    if n == 0 {
        return 1.0;
    }
    let excess = 2.0 * seq.ones() as f64 - n as f64;
    erfc((excess / (n as f64).sqrt()).abs() / core::f64::consts::SQRT_2)
}

/// Block length for [`block_frequency`]: above SP 800-22's `M > 0.01n`
/// floor, and leaving `N = 64` blocks, inside its `N < 100` ceiling.
const BLOCK_FREQUENCY_BITS: usize = 8192;

/// Frequency within a block: is each region of the sequence balanced, not
/// merely the whole of it?
///
/// Catches a generator that drifts, or that is balanced only because two
/// opposite biases cancel.
#[must_use]
pub fn block_frequency(seq: BitSeq<'_>) -> f64 {
    let m = BLOCK_FREQUENCY_BITS;
    let blocks = seq.len() / m;
    if blocks == 0 {
        return 1.0;
    }
    let mut sum = 0.0;
    for block in 0..blocks {
        let ones = seq.ones_in(block * m, m);
        let deviation = ones as f64 / m as f64 - 0.5;
        sum += deviation * deviation;
    }
    chi_square_q(4.0 * m as f64 * sum, blocks as f64)
}

/// Runs: does the sequence alternate as often as chance says it should?
///
/// The complement of [`frequency`]: a balanced sequence can still be far too
/// sticky (long blocks of one value) or far too jumpy (near-perfect
/// alternation).
#[must_use]
pub fn runs(seq: BitSeq<'_>) -> f64 {
    let n = seq.len();
    if n < 2 {
        return 1.0;
    }
    let n_f = n as f64;
    let pi = seq.ones() as f64 / n_f;
    // The test statistic is only meaningful about a balanced sequence, so a
    // sequence that already fails the monobit prerequisite is rejected here
    // rather than fed to a formula that does not describe it.
    if (pi - 0.5).abs() >= 2.0 / n_f.sqrt() {
        return 0.0;
    }
    let transitions = (0..n - 1).filter(|&k| seq.bit(k) != seq.bit(k + 1)).count();
    let observed = transitions as f64 + 1.0;
    let expected = 2.0 * n_f * pi * (1.0 - pi);
    let scale = 2.0 * (2.0 * n_f).sqrt() * pi * (1.0 - pi);
    erfc((observed - expected).abs() / scale)
}

/// Block length for [`longest_run_of_ones`], with SP 800-22's class
/// probabilities for that length.
const LONGEST_RUN_BLOCK_BITS: usize = 128;

/// Probability of each longest-run class in a 128-bit block: `<= 4`, `5`,
/// `6`, `7`, `8`, `>= 9`.
const LONGEST_RUN_CLASS_P: [f64; 6] = [0.1174, 0.2430, 0.2493, 0.1752, 0.1027, 0.1124];

/// Longest run of ones in a block: is the *extreme* of the run-length
/// distribution right, not just its mean?
///
/// [`runs`] counts runs; this one asks how long the longest gets, which is
/// where a generator with a short internal period or a stuck bit shows up.
#[must_use]
pub fn longest_run_of_ones(seq: BitSeq<'_>) -> f64 {
    let m = LONGEST_RUN_BLOCK_BITS;
    let blocks = seq.len() / m;
    if blocks == 0 {
        return 1.0;
    }
    let mut observed = [0u64; LONGEST_RUN_CLASS_P.len()];
    for block in 0..blocks {
        let base = block * m;
        let (mut longest, mut current) = (0usize, 0usize);
        for i in 0..m {
            if seq.bit(base + i) == 1 {
                current += 1;
                longest = longest.max(current);
            } else {
                current = 0;
            }
        }
        // Classes group everything at or below 4 and at or above 9.
        observed[longest.clamp(4, 9) - 4] += 1;
    }
    let blocks_f = blocks as f64;
    let mut chi_square = 0.0;
    for (count, p) in observed.iter().zip(LONGEST_RUN_CLASS_P) {
        let expected = blocks_f * p;
        let deviation = *count as f64 - expected;
        chi_square += deviation * deviation / expected;
    }
    // Six classes, one linear constraint: five degrees of freedom.
    chi_square_q(chi_square, (LONGEST_RUN_CLASS_P.len() - 1) as f64)
}

/// Side length of the matrices [`binary_matrix_rank`] builds.
const RANK_MATRIX_SIDE: usize = 32;

/// Probabilities that a random 32x32 GF(2) matrix has full rank, rank one
/// short, or less (SP 800-22 §2.5).
const RANK_FULL_P: f64 = 0.2888;
const RANK_ONE_SHORT_P: f64 = 0.5776;
const RANK_LOWER_P: f64 = 0.1336;

/// Binary matrix rank: are consecutive stretches of the sequence linearly
/// independent over GF(2)?
///
/// The interesting one. Every LFSR-class generator — and xoshiro's state
/// transition is exactly that — produces output whose bits are linear
/// functions of a fixed-width state, so matrices built from it are rank
/// deficient far more often than chance allows. No amount of statistical
/// polish on the output word hides a linear recurrence from this test if the
/// state is narrower than the matrix span.
#[must_use]
pub fn binary_matrix_rank(seq: BitSeq<'_>) -> f64 {
    let side = RANK_MATRIX_SIDE;
    let bits_per_matrix = side * side;
    let matrices = seq.len() / bits_per_matrix;
    if matrices == 0 {
        return 1.0;
    }
    let (mut full, mut one_short) = (0u64, 0u64);
    let mut rows = [0u32; RANK_MATRIX_SIDE];
    for matrix in 0..matrices {
        let base = matrix * bits_per_matrix;
        for (row, slot) in rows.iter_mut().enumerate() {
            *slot = seq.chunk(base + row * side, side);
        }
        match gf2_rank(&mut rows) {
            r if r == side => full += 1,
            r if r + 1 == side => one_short += 1,
            _ => {}
        }
    }
    let matrices_f = matrices as f64;
    let lower = matrices_f - full as f64 - one_short as f64;
    let term = |observed: f64, p: f64| {
        let expected = matrices_f * p;
        let deviation = observed - expected;
        deviation * deviation / expected
    };
    let chi_square = term(full as f64, RANK_FULL_P)
        + term(one_short as f64, RANK_ONE_SHORT_P)
        + term(lower, RANK_LOWER_P);
    // Three classes, one constraint: two degrees of freedom, whose tail is
    // just `exp(-chi²/2)`.
    chi_square_q(chi_square, 2.0)
}

/// Rank of a square GF(2) matrix held as one bit-packed row per element,
/// most significant bit leftmost. Destroys `rows`.
fn gf2_rank(rows: &mut [u32]) -> usize {
    let side = rows.len();
    let mut rank = 0;
    for column in 0..side {
        let mask = 1u32 << (31 - column);
        let Some(pivot) = (rank..side).find(|&r| rows[r] & mask != 0) else {
            continue;
        };
        rows.swap(rank, pivot);
        for r in 0..side {
            if r != rank && rows[r] & mask != 0 {
                rows[r] ^= rows[rank];
            }
        }
        rank += 1;
    }
    rank
}

/// Pattern length for [`approximate_entropy`]. Inside SP 800-22's
/// `m < floor(log2 n) - 5` condition at [`SEQUENCE_BITS`].
const APPROXIMATE_ENTROPY_PATTERN_BITS: usize = 10;

/// Approximate entropy: how much does knowing ten bits tell you about the
/// eleventh?
///
/// A compressibility measure. It rejects a generator whose output carries
/// repeated structure even when every marginal frequency is right.
#[must_use]
pub fn approximate_entropy(seq: BitSeq<'_>) -> f64 {
    let m = APPROXIMATE_ENTROPY_PATTERN_BITS;
    let n = seq.len();
    if n <= m + 1 {
        return 1.0;
    }
    let phi_m = pattern_entropy(seq, m);
    let phi_m1 = pattern_entropy(seq, m + 1);
    let chi_square = 2.0 * n as f64 * (core::f64::consts::LN_2 - (phi_m - phi_m1));
    gamma_q(f64::from(1u32 << (m - 1)), chi_square / 2.0)
}

/// `Σ (count/n) ln(count/n)` over every `width`-bit pattern, counted across
/// all `n` overlapping windows of the circularly extended sequence.
fn pattern_entropy(seq: BitSeq<'_>, width: usize) -> f64 {
    let n = seq.len();
    let mask = (1u32 << width) - 1;
    let mut counts = vec![0u32; 1usize << width];
    let mut window = seq.chunk(0, width);
    counts[window as usize] += 1;
    for start in 1..n {
        // The sequence wraps, so the last windows are completed from its
        // head; that is what makes every window length see exactly `n` of
        // them and the two entropies comparable.
        let entering = seq.bit((start + width - 1) % n);
        window = ((window << 1) | u32::from(entering)) & mask;
        counts[window as usize] += 1;
    }
    let n_f = n as f64;
    counts
        .iter()
        .filter(|count| **count > 0)
        .map(|count| {
            let p = f64::from(*count) / n_f;
            p * p.ln()
        })
        .sum()
}

/// Cumulative sums, forward: how far does the +/-1 random walk over the
/// sequence stray from zero?
///
/// Sensitive to a bias that is too small for [`frequency`] to see at this
/// length, because a walk accumulates it.
#[must_use]
pub fn cumulative_sums_forward(seq: BitSeq<'_>) -> f64 {
    cumulative_sums(seq, false)
}

/// Cumulative sums, backward: the same walk taken from the end of the
/// sequence, which is where structure the forward walk averages away shows
/// up.
#[must_use]
pub fn cumulative_sums_backward(seq: BitSeq<'_>) -> f64 {
    cumulative_sums(seq, true)
}

fn cumulative_sums(seq: BitSeq<'_>, backward: bool) -> f64 {
    let n = seq.len();
    if n == 0 {
        return 1.0;
    }
    let mut partial = 0i64;
    let mut excursion = 0i64;
    for step in 0..n {
        let index = if backward { n - 1 - step } else { step };
        partial += if seq.bit(index) == 1 { 1 } else { -1 };
        excursion = excursion.max(partial.abs());
    }
    if excursion == 0 {
        return 1.0;
    }
    // A sequence too long to count in a signed integer says nothing; no
    // caller can reach that, since it would not fit memory.
    let Ok(n_i) = i64::try_from(n) else {
        return 1.0;
    };
    // Truncating division throughout, matching the reference formulation's
    // integer limits; Rust and C truncate toward zero alike.
    let span = n_i / excursion;
    let upper = (span - 1) / 4;
    let z = excursion as f64;
    let sqrt_n = (n as f64).sqrt();
    let mut inner = 0.0;
    for k in (-span + 1) / 4..=upper {
        let k = k as f64;
        inner +=
            normal_cdf((4.0 * k + 1.0) * z / sqrt_n) - normal_cdf((4.0 * k - 1.0) * z / sqrt_n);
    }
    let mut outer = 0.0;
    for k in (-span - 3) / 4..=upper {
        let k = k as f64;
        outer +=
            normal_cdf((4.0 * k + 3.0) * z / sqrt_n) - normal_cdf((4.0 * k + 1.0) * z / sqrt_n);
    }
    (1.0 - inner + outer).clamp(0.0, 1.0)
}

/// Block length for [`maurer_universal`]. SP 800-22's table gives `L = 6`
/// for sequences from 387 840 bits, which [`SEQUENCE_BITS`] clears.
const MAURER_BLOCK_BITS: usize = 6;

/// Expected value and variance of the per-block statistic at
/// [`MAURER_BLOCK_BITS`] (SP 800-22 §2.9's table).
const MAURER_EXPECTED: f64 = 5.217_705_2;
const MAURER_VARIANCE: f64 = 2.954;

/// Maurer's universal statistical test: how far apart are repeats of each
/// six-bit block?
///
/// An estimator of the sequence's per-bit entropy, and the battery's
/// strongest compressibility check: a generator with any exploitable
/// redundancy repeats blocks sooner than chance allows.
#[must_use]
pub fn maurer_universal(seq: BitSeq<'_>) -> f64 {
    let l = MAURER_BLOCK_BITS;
    let blocks = seq.len() / l;
    // The initialisation segment primes the last-seen table so the measured
    // segment never sees an unvisited block.
    let init_blocks = 10 * (1usize << l);
    if blocks <= init_blocks {
        return 1.0;
    }
    let measured = blocks - init_blocks;
    let mut last_seen = vec![0usize; 1usize << l];
    for block in 1..=init_blocks {
        last_seen[seq.chunk((block - 1) * l, l) as usize] = block;
    }
    let mut sum = 0.0;
    for block in init_blocks + 1..=blocks {
        let pattern = seq.chunk((block - 1) * l, l) as usize;
        sum += ((block - last_seen[pattern]) as f64).log2();
        last_seen[pattern] = block;
    }
    let measured_f = measured as f64;
    let statistic = sum / measured_f;
    let l_f = l as f64;
    let c = 0.7 - 0.8 / l_f + (4.0 + 32.0 / l_f) * measured_f.powf(-3.0 / l_f) / 15.0;
    let sigma = c * (MAURER_VARIANCE / measured_f).sqrt();
    erfc(((statistic - MAURER_EXPECTED) / sigma).abs() / core::f64::consts::SQRT_2)
}

#[cfg(test)]
mod tests {
    use super::{
        gf2_rank, ALL, APPROXIMATE_ENTROPY_PATTERN_BITS, BLOCK_FREQUENCY_BITS, MAURER_BLOCK_BITS,
        RANK_MATRIX_SIDE, SEQUENCE_BITS, SEQUENCE_BYTES,
    };
    use crate::bits::BitSeq;

    #[test]
    fn every_statistic_has_a_unique_name() {
        for (i, a) in ALL.iter().enumerate() {
            for b in &ALL[i + 1..] {
                assert_ne!(a.name, b.name, "duplicate statistic name");
            }
        }
    }

    /// Each test's parameters have their own validity conditions, and a
    /// sequence length that violated one would make its p-value meaningless
    /// rather than merely weak.
    #[test]
    fn the_sequence_length_satisfies_every_tests_conditions() {
        assert_eq!(SEQUENCE_BYTES * 8, SEQUENCE_BITS);
        // Block frequency: M > 0.01n, and fewer than 100 blocks.
        assert!(BLOCK_FREQUENCY_BITS as f64 > 0.01 * SEQUENCE_BITS as f64);
        const {
            assert!(SEQUENCE_BITS / BLOCK_FREQUENCY_BITS < 100);
        }
        // Binary matrix rank: at least 38 matrices.
        const {
            assert!(SEQUENCE_BITS / (RANK_MATRIX_SIDE * RANK_MATRIX_SIDE) >= 38);
        }
        // Approximate entropy: m < floor(log2 n) - 5.
        assert!(APPROXIMATE_ENTROPY_PATTERN_BITS < SEQUENCE_BITS.ilog2() as usize - 5);
        // Maurer: n >= (Q + K) * L with Q = 10 * 2^L and K >= 1000 * 2^L.
        let q = 10 * (1usize << MAURER_BLOCK_BITS);
        let k = 1000 * (1usize << MAURER_BLOCK_BITS);
        assert!(SEQUENCE_BITS >= (q + k) * MAURER_BLOCK_BITS);
    }

    /// A p-value is a probability, whatever it is handed — including the
    /// degenerate inputs a mis-sized parameter would produce.
    #[test]
    fn every_statistic_returns_a_probability_for_degenerate_input() {
        let zeros = vec![0u8; SEQUENCE_BYTES];
        let ones = vec![0xffu8; SEQUENCE_BYTES];
        let alternating = vec![0b1010_1010u8; SEQUENCE_BYTES];
        let empty: Vec<u8> = Vec::new();
        for input in [&zeros, &ones, &alternating, &empty] {
            for statistic in ALL {
                let p = (statistic.p_value)(BitSeq::new(input));
                assert!(
                    (0.0..=1.0).contains(&p) && p.is_finite(),
                    "{} gave {p}",
                    statistic.name
                );
            }
        }
    }

    /// A degenerate sequence has to be *rejected*, not merely survived: if
    /// all-zeros passed the battery, the battery would be measuring nothing.
    #[test]
    fn a_constant_sequence_is_rejected_by_most_of_the_battery() {
        let zeros = vec![0u8; SEQUENCE_BYTES];
        let rejected = ALL
            .iter()
            .filter(|s| (s.p_value)(BitSeq::new(&zeros)) < 0.01)
            .count();
        assert!(
            rejected >= 7,
            "only {rejected} statistics rejected an all-zero sequence"
        );
    }

    #[test]
    fn gf2_rank_matches_hand_computed_ranks() {
        // Identity: full rank.
        let mut identity: Vec<u32> = (0..32).map(|i| 1u32 << (31 - i)).collect();
        assert_eq!(gf2_rank(&mut identity), 32);
        // All-zero: rank 0.
        let mut zero = vec![0u32; 32];
        assert_eq!(gf2_rank(&mut zero), 0);
        // One duplicated row: one short of full.
        let mut duplicate: Vec<u32> = (0..32).map(|i| 1u32 << (31 - i)).collect();
        duplicate[31] = duplicate[30];
        assert_eq!(gf2_rank(&mut duplicate), 31);
        // Every row equal: rank 1.
        let mut equal = vec![0x1234_5678u32; 32];
        assert_eq!(gf2_rank(&mut equal), 1);
        // A row that is the GF(2) sum of two others is dependent.
        let mut dependent: Vec<u32> = (0..32).map(|i| 1u32 << (31 - i)).collect();
        dependent[5] = dependent[1] ^ dependent[2];
        assert_eq!(gf2_rank(&mut dependent), 31);
    }
}
