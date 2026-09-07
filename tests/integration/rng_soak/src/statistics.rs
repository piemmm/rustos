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

use std::sync::OnceLock;

use crate::battery::UNIFORMITY_BINS;
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

/// A statistic's null p-value distribution across the uniformity bins.
pub type UniformityNull = fn() -> [f64; UNIFORMITY_BINS];

/// One test: a name for reporting and the statistic itself.
pub struct Statistic {
    /// Stable identifier, used in the accumulator and in failure messages.
    pub name: &'static str,
    /// Reduce a sequence to its p-value.
    pub p_value: fn(BitSeq<'_>) -> f64,
    /// How this statistic's p-values are distributed under the null, or
    /// `None` where that has not been derived.
    ///
    /// A p-value is exactly uniform only for a test whose reference
    /// distribution is exact and whose statistic is continuous. None of
    /// these is both: each reduces a finite sequence to a discrete count and
    /// reads an asymptotic tail off it. The deviation is small but *fixed*,
    /// so a uniformity check's power to detect it grows with the sequence
    /// count until it rejects every generator — measurably so by 144 000
    /// sequences, on `ChaCha12` and an HMAC-DRBG alike. Testing against the
    /// statistic's real null instead is what keeps the arm a test of the
    /// generator; where that null is not yet derived the arm is not applied,
    /// and the report says so rather than asserting something false.
    pub uniformity_null: Option<UniformityNull>,
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
        uniformity_null: None,
    },
    Statistic {
        name: "block-frequency",
        p_value: block_frequency,
        uniformity_null: None,
    },
    Statistic {
        name: "runs",
        p_value: runs,
        uniformity_null: None,
    },
    Statistic {
        name: "longest-run",
        p_value: longest_run_of_ones,
        uniformity_null: None,
    },
    Statistic {
        name: "matrix-rank",
        p_value: binary_matrix_rank,
        uniformity_null: Some(rank_uniformity_null),
    },
    Statistic {
        name: "approximate-entropy",
        p_value: approximate_entropy,
        uniformity_null: None,
    },
    Statistic {
        name: "cusum-forward",
        p_value: cumulative_sums_forward,
        uniformity_null: None,
    },
    Statistic {
        name: "cusum-backward",
        p_value: cumulative_sums_backward,
        uniformity_null: None,
    },
    Statistic {
        name: "maurer-universal",
        p_value: maurer_universal,
        uniformity_null: None,
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
const LONGEST_RUN_CLASS_P: [f64; 6] = [
    0.117_403_578_8,
    0.242_955_959_3,
    0.249_363_483_2,
    0.175_177_060_3,
    0.102_701_071_3,
    0.112_398_847_1,
];

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
const RANK_FULL_P: f64 = 0.288_788_095_154;
const RANK_ONE_SHORT_P: f64 = 0.577_576_190_173;
const RANK_LOWER_P: f64 = 0.133_635_714_673;

/// Exact null distribution of [`binary_matrix_rank`]'s p-value.
///
/// The statistic sorts a fixed number of matrices into three rank classes,
/// so its chi-square takes finitely many values and its p-value is a
/// discrete distribution — never uniform on `[0, 1)`, however large the run.
/// Measured on `ChaCha12` the resulting lumpiness is unmistakable: bin
/// deviations oscillate by up to 13% and a uniformity check against a flat
/// reference reaches chi-square 91 on nine degrees of freedom, rejecting a
/// sound generator.
///
/// The class counts are multinomial, so the exact distribution follows from
/// enumerating every reachable `(full, one-short)` pair and binning its
/// p-value with the multinomial weight. It reads the p-value through the
/// same tail function [`binary_matrix_rank`] uses, so this is the null of
/// the implementation rather than of an idealisation of it.
fn rank_uniformity_null() -> [f64; UNIFORMITY_BINS] {
    static NULL: OnceLock<[f64; UNIFORMITY_BINS]> = OnceLock::new();
    *NULL.get_or_init(|| {
        let matrices = SEQUENCE_BITS / (RANK_MATRIX_SIDE * RANK_MATRIX_SIDE);
        let matrices_f = matrices as f64;

        let mut log_factorial = vec![0.0f64; matrices + 1];
        for k in 1..=matrices {
            log_factorial[k] = log_factorial[k - 1] + (k as f64).ln();
        }
        let log_p = [RANK_FULL_P.ln(), RANK_ONE_SHORT_P.ln(), RANK_LOWER_P.ln()];

        let mut null = [0.0f64; UNIFORMITY_BINS];
        for full in 0..=matrices {
            for one_short in 0..=matrices - full {
                let lower = matrices - full - one_short;
                let counts = [full, one_short, lower];
                let mut chi_square = 0.0;
                for (count, p) in counts
                    .iter()
                    .zip([RANK_FULL_P, RANK_ONE_SHORT_P, RANK_LOWER_P])
                {
                    let expected = matrices_f * p;
                    let deviation = *count as f64 - expected;
                    chi_square += deviation * deviation / expected;
                }
                let mut log_weight = log_factorial[matrices];
                for (count, log_p) in counts.iter().zip(log_p) {
                    log_weight += *count as f64 * log_p - log_factorial[*count];
                }
                let p_value = chi_square_q(chi_square, 2.0);
                let bin = bin_of(p_value);
                null[bin] += log_weight.exp();
            }
        }
        null
    })
}

/// Which uniformity bin a p-value falls in.
///
/// A p-value of exactly `1.0` belongs to the last bin rather than off the
/// end, so this is the one place the mapping is spelled.
pub(crate) fn bin_of(p_value: f64) -> usize {
    let scaled = p_value * UNIFORMITY_BINS as f64;
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a p-value is in [0, 1], so the scaled index is in range and non-negative"
    )]
    let bin = scaled as usize;
    bin.min(UNIFORMITY_BINS - 1)
}

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

/// Terms kept in each geometric sum over block distances. The weights carry
/// `(1 - 2^-L)^n`, which falls under a double's epsilon by `n = 2483` at
/// `L = 6`, so the neglected tail is below the rounding of the terms kept.
const MAURER_DISTANCE_TERMS: usize = 3072;

/// Lags kept in the autocovariance sums, which decay at the same ratio.
const MAURER_LAG_TERMS: usize = 3072;

/// Null-distribution moments of [`maurer_universal`]'s per-block statistic.
struct MaurerNull {
    /// `E[log2 A]` for one block distance.
    mean: f64,
    /// `Var[log2 A]` for one block distance.
    variance: f64,
    /// `sum over k >= 1` of `Cov(log2 A_n, log2 A_(n+k))`.
    covariance: f64,
    /// `sum over k >= 1` of `k Cov(log2 A_n, log2 A_(n+k))`.
    lag_weighted_covariance: f64,
}

impl MaurerNull {
    /// Variance of the mean of `measured` block statistics.
    ///
    /// Both terms are positive — the distances are negatively correlated, so
    /// the covariance sums are negative — so this never yields a zero or
    /// imaginary standard deviation to divide by.
    fn statistic_variance(&self, measured: f64) -> f64 {
        (self.variance + 2.0 * self.covariance) / measured
            - 2.0 * self.lag_weighted_covariance / (measured * measured)
    }
}

/// The null moments at [`MAURER_BLOCK_BITS`], derived once per process.
fn maurer_null() -> &'static MaurerNull {
    static NULL: OnceLock<MaurerNull> = OnceLock::new();
    NULL.get_or_init(|| maurer_null_moments(MAURER_DISTANCE_TERMS, MAURER_LAG_TERMS))
}

/// Derives the null moments from the joint law of two block distances.
///
/// SP 800-22 tabulates `E[log2 A]` and `Var[log2 A]`, then — because the
/// distances are not independent — scales the standard deviation of their
/// mean by the heuristic `c = 0.7 - 0.8/L + (4 + 32/L) K^(-3/L) / 15`. That
/// heuristic is 3.8% low at `L = 6` and this crate's block count, which
/// inflates every z-score by as much and rejects a sound generator at 1.32%
/// against a nominal 1%; Coron and Naccache, "An Accurate Evaluation of
/// Maurer's Universal Test" (Selected Areas in Cryptography 1998), identify
/// it as the test's weak point. So the dependence is summed exactly here.
///
/// The joint law is the unbiased-source case of Miyazaki, Nuida and
/// Shikata, "The reference distributions of Maurer's universal statistical
/// test and its improved tests" (arXiv:2103.10660) eqs. 20-44. Writing
/// `p = 2^-L`, `u = 1 - p`, `v = 1 - 2p`, for distances `i` at block `n` and
/// `j` at block `n + k`:
///
/// | case | `Pr[A_n = i, A_(n+k) = j]` |
/// |---|---|
/// | `1 <= j <= k-1` | `p u^(i-1) . p u^(j-1)`, independent |
/// | `j = k` | `p^2 u^(i+k-2)` |
/// | `k+1 <= j <= k+i-1` | `p^2 u^(i-j+2k-1) v^(j-k-1)` |
/// | `j = k+i` | `0`, the two distances cannot meet |
/// | `j >= k+i+1` | `p^2 u^(j-i-1) v^(i-1)` |
fn maurer_null_moments(distance_terms: usize, lag_terms: usize) -> MaurerNull {
    let p = 1.0 / (1usize << MAURER_BLOCK_BITS) as f64;
    let u = 1.0 - p;
    let v = 1.0 - 2.0 * p;
    let overlap_ratio = v / u;

    // Every weight below carries a power of `u`, `v`, or `v/u` that advances
    // by one factor per term, so each is a running product rather than a
    // fresh exponentiation.
    let (mut mean, mut second_moment) = (0.0, 0.0);
    let mut decay = 1.0;
    for i in 1..=distance_terms {
        let value = (i as f64).log2();
        mean += p * decay * value;
        second_moment += p * decay * value * value;
        decay *= u;
    }
    let variance = second_moment - mean * mean;

    // far[x] = sum over t >= 0 of log2(x + t) u^t, the tail a case-5 pairing
    // sums over, by backward recursion so each is one multiply-add. Indices
    // reach lag_terms + distance_terms + 1; the rest is the headroom the
    // truncated recursion needs for those entries to have converged.
    let far_len = lag_terms + 2 * distance_terms + 2;
    let mut far = vec![0.0; far_len + 2];
    for x in (1..=far_len).rev() {
        far[x] = (x as f64).log2() + u * far[x + 1];
    }

    // before[k] = sum over j < k of log2(j) p u^(j-1), the case-1 pairing.
    let mut before = vec![0.0; lag_terms + 2];
    let mut decay = 1.0;
    for k in 2..=lag_terms + 1 {
        before[k] = before[k - 1] + ((k - 1) as f64).log2() * p * decay;
        decay *= u;
    }

    let (mut covariance, mut lag_weighted_covariance) = (0.0, 0.0);
    let mut within = vec![0.0; distance_terms + 1];
    let mut lag_weight = u;
    for k in 1..=lag_terms {
        let mut joint = mean * before[k] + (k as f64).log2() * p * (lag_weight / u) * mean;

        // within[s] accumulates the case-3 pairings for j = k + 1 + s.
        let mut running = 0.0;
        let mut power = 1.0;
        for (s, slot) in within.iter_mut().enumerate() {
            running += ((k + 1 + s) as f64).log2() * power;
            *slot = running;
            power *= overlap_ratio;
        }
        let mut nested = 0.0;
        let mut reach = u * u;
        for i in 2..=distance_terms {
            nested += (i as f64).log2() * reach * within[i - 2];
            reach *= u;
        }
        joint += nested * p * p * lag_weight / (u * u);

        let mut straddling = 0.0;
        let mut skew = 1.0;
        for i in 1..=distance_terms {
            straddling += (i as f64).log2() * skew * far[k + i + 1];
            skew *= v;
        }
        joint += straddling * p * p * lag_weight;

        let lag_covariance = joint - mean * mean;
        covariance += lag_covariance;
        lag_weighted_covariance += k as f64 * lag_covariance;
        lag_weight *= u;
    }

    MaurerNull {
        mean,
        variance,
        covariance,
        lag_weighted_covariance,
    }
}

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
    let null = maurer_null();
    let sigma = null.statistic_variance(measured_f).sqrt();
    erfc(((statistic - null.mean) / sigma).abs() / core::f64::consts::SQRT_2)
}

#[cfg(test)]
mod tests {
    use super::{
        bin_of, gf2_rank, maurer_null, maurer_null_moments, rank_uniformity_null, ALL,
        APPROXIMATE_ENTROPY_PATTERN_BITS, BLOCK_FREQUENCY_BITS, MAURER_BLOCK_BITS,
        MAURER_DISTANCE_TERMS, MAURER_LAG_TERMS, RANK_MATRIX_SIDE, SEQUENCE_BITS, SEQUENCE_BYTES,
        UNIFORMITY_BINS,
    };
    use crate::bits::BitSeq;

    /// Blocks the Maurer statistic averages over one [`SEQUENCE_BITS`] run.
    fn maurer_measured_blocks() -> f64 {
        (SEQUENCE_BITS / MAURER_BLOCK_BITS - 10 * (1usize << MAURER_BLOCK_BITS)) as f64
    }

    /// The derived per-distance moments must reproduce SP 800-22 §2.9's
    /// published table, which is the independent check on the joint law.
    #[test]
    fn the_derived_block_moments_match_the_published_table() {
        let null = maurer_null();
        assert!(
            (null.mean - 5.217_705_2).abs() < 5e-7,
            "E[log2 A] = {}, table gives 5.2177052",
            null.mean
        );
        assert!(
            (null.variance - 2.954).abs() < 5e-4,
            "Var[log2 A] = {}, table gives 2.954",
            null.variance
        );
    }

    /// The standard deviation of the statistic must match what the null
    /// distribution actually produces: 3.4509608e-3, measured over 48 000
    /// sequences drawn from the platform CSPRNG. SP 800-22's heuristic gives
    /// 3.3192e-3 — 3.8% low, which is what rejected sound generators at
    /// 1.32% against a nominal 1%.
    #[test]
    fn the_statistic_deviation_matches_the_measured_null_distribution() {
        let sigma = maurer_null()
            .statistic_variance(maurer_measured_blocks())
            .sqrt();
        let measured = 3.450_960_8e-3;
        assert!(
            (sigma / measured - 1.0).abs() < 0.01,
            "sigma = {sigma:e} but the null distribution measures {measured:e}"
        );
    }

    /// The enumerated null must be a probability distribution: the
    /// multinomial weights are summed independently of the binning, so a
    /// missed or double-counted count vector shows up here.
    #[test]
    fn the_derived_rank_null_is_a_distribution() {
        let null = rank_uniformity_null();
        let total: f64 = null.iter().sum();
        assert!(
            (total - 1.0).abs() < 1e-9,
            "the enumerated rank null sums to {total}, not 1"
        );
        assert!(
            null.iter().all(|share| *share > 0.0),
            "every bin must be reachable: {null:?}"
        );
    }

    /// The point of deriving it: this statistic's p-value is *not* uniform,
    /// so testing it against a flat reference rejects a sound generator.
    /// Measured on `ChaCha12` that reference error reaches chi-square 91 on
    /// nine degrees of freedom at 144 000 sequences.
    #[test]
    fn the_derived_rank_null_is_not_uniform() {
        let null = rank_uniformity_null();
        let flat = 1.0 / UNIFORMITY_BINS as f64;
        let worst = null
            .iter()
            .map(|share| (share / flat - 1.0).abs())
            .fold(0.0f64, f64::max);
        assert!(
            worst > 0.01,
            "a flat null would be within {:.3}% of the real one, so this \
             statistic did not need its own: {null:?}",
            worst * 100.0
        );
    }

    /// The derived null must be what the statistic actually produces, not
    /// merely a distribution: these are the bin shares 144 000 `ChaCha12`
    /// sequences landed in, whose sampling error is 0.0008 per share. A
    /// uniformity check against a flat reference scores chi-square 91 on
    /// that histogram and against this one 9.0, which is the whole point.
    #[test]
    fn the_derived_rank_null_matches_the_measured_distribution() {
        const MEASURED: [f64; UNIFORMITY_BINS] = [
            0.0998, 0.0974, 0.1024, 0.1036, 0.0951, 0.0997, 0.1032, 0.1012, 0.0988, 0.0988,
        ];
        let null = rank_uniformity_null();
        for (bin, (derived, measured)) in null.iter().zip(MEASURED).enumerate() {
            assert!(
                (derived - measured).abs() < 0.003,
                "bin {bin}: derived {derived:.4} against a measured {measured:.4}"
            );
        }
    }

    /// A p-value of exactly 1.0 belongs in the last bin, not one past the
    /// end — the bound the shared binning exists to get right.
    #[test]
    fn the_binning_covers_the_closed_unit_interval() {
        assert_eq!(bin_of(0.0), 0);
        assert_eq!(bin_of(1.0), UNIFORMITY_BINS - 1);
        assert_eq!(bin_of(0.999_999), UNIFORMITY_BINS - 1);
        assert_eq!(bin_of(0.1), 1);
    }

    /// The geometric sums are truncated, so the kept terms must be enough
    /// that doubling them does not move the answer.
    #[test]
    fn the_truncated_null_sums_have_converged() {
        let coarse = maurer_null_moments(MAURER_DISTANCE_TERMS, MAURER_LAG_TERMS);
        let fine = maurer_null_moments(2 * MAURER_DISTANCE_TERMS, 2 * MAURER_LAG_TERMS);
        let blocks = maurer_measured_blocks();
        let ratio = coarse.statistic_variance(blocks) / fine.statistic_variance(blocks);
        assert!(
            (ratio - 1.0).abs() < 1e-12,
            "doubling the kept terms moved the variance by a factor {ratio}"
        );
    }

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
