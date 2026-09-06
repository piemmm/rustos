//! Turning a stream of p-values into a verdict.
//!
//! A single p-value decides nothing: at a 1% significance level a *sound*
//! generator produces one below the threshold in one sequence out of a
//! hundred, so a harness that failed on the first low p-value would be a
//! flaky test rather than a defect detector. SP 800-22's answer is a
//! two-level rule over many sequences, and this is that rule:
//!
//! 1. **Pass proportion.** The fraction of sequences whose p-value clears
//!    [`ALPHA`] must sit inside a band around `1 - ALPHA`.
//! 2. **Uniformity.** The p-values themselves must be uniform on `[0, 1)`,
//!    checked by a chi-square goodness-of-fit over ten bins. This is what
//!    catches a generator that is *too* regular — an m-sequence whose run
//!    statistics are ideal to a fault clusters its p-values near 1 and
//!    passes the proportion test while failing here.
//!
//! # Why the band is six sigma and not three
//!
//! SP 800-22 suggests a three-sigma band, which gives each decision a
//! roughly 1% chance of firing on a sound generator. Across nine statistics
//! and two generators that is a coin flip per run — unacceptable in a gate
//! that must never be flaky, and unacceptable in a soak whose verdict would
//! otherwise be noise. Six sigma and a `1e-6` uniformity floor put the whole
//! battery's false-alarm probability in the region of `1e-4`, while costing
//! nothing in detection power: a structural defect does not sit marginally
//! outside the band, it pins p-values at zero and lands hundreds of sigma
//! out. The negative controls are what keep that claim honest.
//!
//! # Why the verdict is reached once, at the end
//!
//! Accumulating across every pass and deciding once is both the more
//! powerful and the less flaky arrangement: the band narrows as the sequence
//! count grows, so a long soak becomes strictly more sensitive, while
//! re-deciding after every pass would multiply the false-alarm rate by the
//! number of looks.

// Counts become `f64` for the statistics: every one is far below the exact
// integer range of a double, so the conversion is the arithmetic rather than
// a loss of it.
#![allow(clippy::cast_precision_loss)]

use std::fmt::Write as _;

use crate::special::chi_square_q;
use crate::statistics::ALL;

/// Significance level for a single sequence's p-value.
pub const ALPHA: f64 = 0.01;

/// Width of the accepted pass-proportion band, in standard deviations.
pub const BAND_SIGMA: f64 = 6.0;

/// Smallest uniformity p-value that is not treated as a rejection.
pub const UNIFORMITY_FLOOR: f64 = 1e-6;

/// Bins the p-value uniformity check divides `[0, 1)` into.
const UNIFORMITY_BINS: usize = 10;

/// Sequences a statistic needs before its verdict means anything.
///
/// Below this the pass-proportion band is wider than the whole `[0, 1]`
/// interval, so a verdict would be vacuous rather than lenient.
pub const MINIMUM_SEQUENCES: u64 = 64;

/// Running tallies for one statistic.
#[derive(Clone, Copy, Debug, Default)]
struct Tally {
    sequences: u64,
    failures: u64,
    bins: [u64; UNIFORMITY_BINS],
}

/// What a statistic's accumulated p-values say about the generator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Both levels of the rule are satisfied.
    Accepted,
    /// Too many (or implausibly few) sequences failed at [`ALPHA`].
    ProportionOutOfBand,
    /// The p-values are not uniform.
    NotUniform,
    /// Fewer than [`MINIMUM_SEQUENCES`] were tested.
    TooFewSequences,
}

impl Verdict {
    /// Whether this verdict is the battery *rejecting* a generator, as
    /// against having reached no conclusion.
    ///
    /// The distinction is what keeps the negative controls honest: a control
    /// run on too few sequences would be "not accepted" for every statistic
    /// and would prove nothing about any of them.
    #[must_use]
    pub const fn is_rejection(self) -> bool {
        matches!(self, Self::ProportionOutOfBand | Self::NotUniform)
    }
}

/// Accumulated p-values for every statistic in the battery.
///
/// One accumulator spans a whole run — a single smoke pass or a night's worth
/// of them — because the verdict is reached over all of it at once.
pub struct Accumulator {
    tallies: Vec<Tally>,
}

impl Accumulator {
    /// An empty accumulator, one tally per statistic.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tallies: vec![Tally::default(); ALL.len()],
        }
    }

    /// Run every statistic over `sequence` and record its p-value.
    pub fn record(&mut self, sequence: &[u8]) {
        let seq = crate::bits::BitSeq::new(sequence);
        for (statistic, tally) in ALL.iter().zip(self.tallies.iter_mut()) {
            let p = (statistic.p_value)(seq);
            tally.sequences += 1;
            if p < ALPHA {
                tally.failures += 1;
            }
            // A p-value of exactly 1.0 belongs in the top bin rather than
            // one past the end.
            let scaled = p * (UNIFORMITY_BINS as f64);
            let bin = (1..=UNIFORMITY_BINS)
                .position(|edge| scaled < (edge as f64))
                .unwrap_or(UNIFORMITY_BINS - 1);
            tally.bins[bin] += 1;
        }
    }

    /// Sequences recorded so far.
    #[must_use]
    pub fn sequences(&self) -> u64 {
        self.tallies.first().map_or(0, |t| t.sequences)
    }

    /// The verdict for each statistic, in [`ALL`] order.
    #[must_use]
    pub fn verdicts(&self) -> Vec<(&'static str, Verdict)> {
        ALL.iter()
            .zip(&self.tallies)
            .map(|(statistic, tally)| (statistic.name, verdict(tally)))
            .collect()
    }

    /// The statistics that did not accept the generator, rejection and
    /// inconclusive alike — an inconclusive verdict is not a pass.
    #[must_use]
    pub fn rejected(&self) -> Vec<&'static str> {
        self.verdicts()
            .into_iter()
            .filter(|(_, v)| *v != Verdict::Accepted)
            .map(|(name, _)| name)
            .collect()
    }

    /// A one-line-per-statistic report, for a soak log or a failure message.
    #[must_use]
    pub fn report(&self) -> String {
        let mut out = String::new();
        for (statistic, tally) in ALL.iter().zip(&self.tallies) {
            let uniformity = uniformity_p(tally);
            let _ = writeln!(
                out,
                "  {:<20} sequences {:>7}  failures {:>6}  uniformity {:>9.3e}  {:?}",
                statistic.name,
                tally.sequences,
                tally.failures,
                uniformity,
                verdict(tally)
            );
        }
        out
    }
}

impl Default for Accumulator {
    fn default() -> Self {
        Self::new()
    }
}

/// The two-level decision for one statistic's tally.
fn verdict(tally: &Tally) -> Verdict {
    if tally.sequences < MINIMUM_SEQUENCES {
        return Verdict::TooFewSequences;
    }
    let m = tally.sequences as f64;
    let expected_pass = 1.0 - ALPHA;
    let sigma = (expected_pass * ALPHA / m).sqrt();
    let proportion = (m - tally.failures as f64) / m;
    if (proportion - expected_pass).abs() > BAND_SIGMA * sigma {
        return Verdict::ProportionOutOfBand;
    }
    if uniformity_p(tally) < UNIFORMITY_FLOOR {
        return Verdict::NotUniform;
    }
    Verdict::Accepted
}

/// Chi-square goodness-of-fit p-value for the binned p-values against a
/// uniform distribution.
fn uniformity_p(tally: &Tally) -> f64 {
    if tally.sequences == 0 {
        return 1.0;
    }
    let expected = tally.sequences as f64 / UNIFORMITY_BINS as f64;
    let chi_square: f64 = tally
        .bins
        .iter()
        .map(|count| {
            let deviation = *count as f64 - expected;
            deviation * deviation / expected
        })
        .sum();
    chi_square_q(chi_square, (UNIFORMITY_BINS - 1) as f64)
}

#[cfg(test)]
mod tests {
    use super::{
        uniformity_p, verdict, Accumulator, Tally, Verdict, ALPHA, BAND_SIGMA, MINIMUM_SEQUENCES,
        UNIFORMITY_BINS, UNIFORMITY_FLOOR,
    };

    /// A tally with `failures` out of `sequences` and p-values spread evenly
    /// across the bins, so only the proportion arm can fire.
    fn tally(sequences: u64, failures: u64) -> Tally {
        let per_bin = sequences / UNIFORMITY_BINS as u64;
        let mut bins = [per_bin; UNIFORMITY_BINS];
        bins[0] += sequences - per_bin * UNIFORMITY_BINS as u64;
        Tally {
            sequences,
            failures,
            bins,
        }
    }

    #[test]
    fn a_typical_failure_rate_is_accepted() {
        // Exactly the rate ALPHA predicts, at two very different counts: a
        // generator is not suspect for being ordinary.
        assert_eq!(verdict(&tally(4_000, 40)), Verdict::Accepted);
        assert_eq!(verdict(&tally(200_000, 2_000)), Verdict::Accepted);
    }

    /// The band is two-sided on purpose. A statistic that *never* rejects has
    /// stopped discriminating — a tail collapsed to 1, a constant returned
    /// instead of a p-value — and that must fail rather than look like the
    /// cleanest possible result.
    #[test]
    fn implausibly_few_failures_are_rejected_too() {
        // The lower edge only exists once six sigma is narrower than ALPHA
        // itself, which is above roughly 3 500 sequences.
        assert_eq!(verdict(&tally(200_000, 0)), Verdict::ProportionOutOfBand);
        // Below that the band's upper edge passes 1.0, so a short run is not
        // failed for a shortage of failures it had no chance to accumulate.
        assert_eq!(verdict(&tally(512, 0)), Verdict::Accepted);
    }

    #[test]
    fn a_grossly_inflated_failure_rate_is_rejected() {
        // Ten times the expected failures is a defect, not luck.
        assert_eq!(verdict(&tally(4_000, 400)), Verdict::ProportionOutOfBand);
    }

    /// The band must be exactly `BAND_SIGMA` wide — the property the
    /// false-alarm argument rests on. Checked across the whole edge rather
    /// than at one point, so a wrong width shows up wherever it is wrong.
    #[test]
    fn the_band_edge_is_where_the_sigma_width_puts_it() {
        let sequences = 10_000u64;
        let m = sequences as f64;
        let sigma = ((1.0 - ALPHA) * ALPHA / m).sqrt();
        for failures in 0..400u64 {
            let inside = ((failures as f64) / m - ALPHA).abs() <= BAND_SIGMA * sigma;
            let expected = if inside {
                Verdict::Accepted
            } else {
                Verdict::ProportionOutOfBand
            };
            assert_eq!(
                verdict(&tally(sequences, failures)),
                expected,
                "{failures} failures of {sequences}"
            );
        }
    }

    #[test]
    fn too_few_sequences_is_not_a_pass_and_not_a_rejection() {
        let inconclusive = verdict(&tally(MINIMUM_SEQUENCES - 1, 0));
        assert_eq!(inconclusive, Verdict::TooFewSequences);
        assert_eq!(verdict(&tally(MINIMUM_SEQUENCES, 0)), Verdict::Accepted);
        // A generator is failed for an inconclusive verdict — fail closed —
        // but a *control* must not be credited as rejected by one, or a
        // too-small control run would satisfy every statistic vacuously.
        assert!(!inconclusive.is_rejection());
        assert!(Verdict::ProportionOutOfBand.is_rejection());
        assert!(Verdict::NotUniform.is_rejection());
        assert!(!Verdict::Accepted.is_rejection());
    }

    /// The arm that catches a generator that is too regular rather than too
    /// irregular: every p-value in one bin passes the proportion test and
    /// must still be rejected.
    #[test]
    fn p_values_piled_into_one_bin_are_rejected_as_non_uniform() {
        // A count below the band's lower edge, so only the uniformity arm can
        // be what fires.
        let mut bins = [0u64; UNIFORMITY_BINS];
        bins[9] = 2_000;
        let piled = Tally {
            sequences: 2_000,
            failures: 0,
            bins,
        };
        assert!(uniformity_p(&piled) < UNIFORMITY_FLOOR);
        assert_eq!(verdict(&piled), Verdict::NotUniform);
    }

    #[test]
    fn evenly_spread_p_values_are_uniform() {
        assert!(uniformity_p(&tally(4_000, 40)) > 0.5);
    }

    /// A mild lean must not fire: the uniformity floor exists to catch
    /// structure, not sampling noise.
    #[test]
    fn a_mild_bin_imbalance_is_accepted() {
        let mut bins = [400u64; UNIFORMITY_BINS];
        bins[0] += 30;
        bins[9] -= 30;
        let leaning = Tally {
            sequences: 4_000,
            failures: 40,
            bins,
        };
        assert_eq!(verdict(&leaning), Verdict::Accepted);
    }

    /// A recorded sequence must land in every statistic's tally, and a
    /// p-value of exactly 1 must not fall off the end of the bins.
    #[test]
    fn recording_a_sequence_tallies_every_statistic() {
        let mut acc = Accumulator::new();
        // All-zero: many statistics give exactly 0, one or two exactly 1.
        acc.record(&vec![0u8; crate::statistics::SEQUENCE_BYTES]);
        assert_eq!(acc.sequences(), 1);
        for (name, v) in acc.verdicts() {
            assert_eq!(v, Verdict::TooFewSequences, "{name}");
        }
        let total: u64 = acc.tallies.iter().map(|t| t.bins.iter().sum::<u64>()).sum();
        assert_eq!(
            total,
            crate::statistics::ALL.len() as u64,
            "every p-value must land in a bin"
        );
    }

    #[test]
    fn the_report_names_every_statistic() {
        let mut acc = Accumulator::new();
        acc.record(&vec![0xa5u8; crate::statistics::SEQUENCE_BYTES]);
        let report = acc.report();
        for statistic in crate::statistics::ALL {
            assert!(
                report.contains(statistic.name),
                "{} missing",
                statistic.name
            );
        }
    }
}
