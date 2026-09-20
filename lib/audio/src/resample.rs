//! The one resampler: a polyphase Kaiser-windowed-sinc interpolator whose
//! ratio is an exact rational.
//!
//! There is exactly one of these in TAIRiX. Drivers never resample and clients
//! never need to, so a second implementation anywhere is a review blocker.
//!
//! # The ratio never drifts
//!
//! The output position is carried as an integer input frame plus an integer
//! remainder over the reduced denominator — 48 kHz into 44.1 kHz is 160/147,
//! and the step is exactly that. No floating-point accumulator is advanced per
//! sample, so an hour of playback ends on precisely the frame arithmetic says
//! it should rather than a few samples either side of it.
//!
//! # One mechanism, two cases
//!
//! The phase bank holds one row per denominator step where the denominator is
//! small enough ([`MAX_PHASES`]), which covers every pair in the standard rate
//! family: the phase lookup is then an index and the interpolation weight
//! between rows is exactly zero. A ratio whose denominator is larger — a
//! rate pair with no common factor, or the drifting ratio a linked clock
//! domain corrects against — shares the same stepping and reaches its phase by
//! interpolating between the two nearest rows, which is the fractional-delay
//! case. The code has one path; the exact case simply never engages the
//! interpolation.
//!
//! # A unity ratio is a copy
//!
//! Equal rates bypass the filter entirely. A windowed sinc at unity is very
//! nearly the identity, and "very nearly" would destroy the stack's
//! bit-exactness property, so the bypass is explicit rather than incidental.
//!
//! # The figures are measured, not asserted
//!
//! [`DESIGN_ATTENUATION_DB`], [`PASSBAND_EDGE`] and [`STOPBAND_EDGE`] describe
//! the prototype the bank is built from, and the crate's own tests evaluate
//! the built bank's frequency response and hold it to them. A change to the
//! tap count or the window that quietly degraded the filter would fail the
//! suite rather than leave the documentation lying.

use alloc::vec::Vec;

use tairix_abi::driver::audio::{Rate, MAX_CHANNELS};
use tairix_abi::Errno;
use tairix_util::fallible;
use tairix_util::mathf;

/// Input frames one output sample is computed from when the output rate is
/// at least the input's.
///
/// A filter-design parameter, not a capacity: with the transition width below
/// it is what buys [`DESIGN_ATTENUATION_DB`] of stopband rejection, and
/// changing it changes the filter rather than a limit.
pub const BASE_TAPS: usize = 96;

/// The most taps any ratio's filter uses.
///
/// A decimating filter must reach the stopband by the *output's* Nyquist, so
/// its transition is narrower in input-rate terms by exactly the decimation
/// factor and it needs proportionally more taps. This caps that growth: it
/// covers decimation to a fifth, which is past every pair in the standard
/// rate family, and a steeper one widens the transition rather than
/// unbounding the work one output sample costs. It bounds the bank too — at
/// most [`MAX_PHASES`] rows of this length — so no rate pair can be asked
/// for an unbounded coefficient table.
pub const MAX_TAPS: usize = 512;

/// Rows the phase bank holds when the ratio's denominator is larger.
///
/// Chosen so the rate pairs that actually occur get an exact bank: the
/// 44.1-into-48 family reduces to denominators of 147 and 160, and the
/// telephony multiples to single digits. A larger denominator interpolates
/// between rows instead, which is the fractional-delay case.
pub const MAX_PHASES: u32 = 160;

/// Where the passband ends, in cycles per sample of the **lower** of the two
/// rates. At 44.1 kHz that is 19.0 kHz.
pub const PASSBAND_EDGE: f64 = 0.43;

/// Where the stopband begins, in cycles per sample of the lower rate: Nyquist,
/// so nothing aliases back into the band at all.
pub const STOPBAND_EDGE: f64 = 0.5;

/// Stopband rejection the window is designed for, in decibels.
///
/// Kaiser's own tap estimate, `(A - 8) / (2.285 * transition in radians)`,
/// puts a hundred decibels across this transition at exactly
/// [`BASE_TAPS`] taps, which is where that figure comes from.
pub const DESIGN_ATTENUATION_DB: f64 = 100.0;

/// The kernel's half-amplitude point: the centre of the transition, not its
/// start.
///
/// A windowed sinc is six decibels down at its own cutoff, so cutting the
/// sinc at the passband edge would put the transition's whole first half
/// inside the band the filter is supposed to pass.
const CUTOFF: f64 = f64::midpoint(PASSBAND_EDGE, STOPBAND_EDGE);

/// How much deeper than the guaranteed figure the window is designed.
///
/// Kaiser's shape and tap formulas are estimates good to about a decibel, so
/// designing for exactly the guaranteed figure lands either side of it.
/// Designing a few decibels deeper makes the *measured* response clear the
/// figure the crate publishes, which is the one the tests hold it to.
const KAISER_MARGIN_DB: f64 = 4.0;

/// The Kaiser shape parameter, from Kaiser's own `0.1102 * (A - 8.7)` for
/// attenuations past 50 dB.
const KAISER_BETA: f64 = 0.1102 * (DESIGN_ATTENUATION_DB + KAISER_MARGIN_DB - 8.7);

/// An exact, reduced resampling ratio: input frames per output frame.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Ratio {
    input: u32,
    output: u32,
}

impl Ratio {
    /// The ratio carrying `from` to `to`, reduced to lowest terms.
    #[must_use]
    pub fn between(from: Rate, to: Rate) -> Self {
        let divisor = gcd(from.hz(), to.hz());
        Self {
            input: from.hz() / divisor,
            output: to.hz() / divisor,
        }
    }

    /// The ratio `input`/`output`, reduced, or [`None`] when either term is
    /// zero — a ratio with a zero term describes no stepping at all.
    #[must_use]
    pub fn new(input: u32, output: u32) -> Option<Self> {
        if input == 0 || output == 0 {
            return None;
        }
        let divisor = gcd(input, output);
        Some(Self {
            input: input / divisor,
            output: output / divisor,
        })
    }

    /// Input frames consumed per `output()` frames produced.
    #[must_use]
    pub const fn input(self) -> u32 {
        self.input
    }

    /// The denominator the position's remainder is carried over.
    #[must_use]
    pub const fn output(self) -> u32 {
        self.output
    }

    /// Whether input and output advance together, so no filtering is wanted.
    #[must_use]
    pub const fn is_unity(self) -> bool {
        self.input == self.output
    }
}

/// Taps the filter for `ratio` needs.
///
/// An interpolating ratio keeps the base count; a decimating one scales it by
/// the decimation factor, because the transition it must fit inside is
/// narrower by exactly that much in input-rate terms. Rounded up to an even
/// count so the span is symmetric about an output position, and capped.
fn taps_for(ratio: Ratio) -> usize {
    if ratio.output() >= ratio.input() {
        return BASE_TAPS;
    }
    let denominator = usize::try_from(ratio.output()).unwrap_or(1).max(1);
    let scaled = BASE_TAPS
        .saturating_mul(usize::try_from(ratio.input()).unwrap_or(1))
        .saturating_add(denominator - 1)
        / denominator;
    scaled.next_multiple_of(2).min(MAX_TAPS)
}

/// The greatest common divisor, by Euclid.
const fn gcd(a: u32, b: u32) -> u32 {
    let (mut a, mut b) = (a, b);
    while b != 0 {
        let next = a % b;
        a = b;
        b = next;
    }
    a
}

/// The modified Bessel function of the first kind, order zero, by its power
/// series — which converges inside the double's last bit well before the
/// window's shape parameter.
fn bessel_i0(x: f64) -> f64 {
    let quarter_square = x * x / 4.0;
    let mut term = 1.0;
    let mut sum = 1.0;
    for k in 1..40_u32 {
        term *= quarter_square / (f64::from(k) * f64::from(k));
        sum += term;
        if term < sum * 1e-18 {
            break;
        }
    }
    sum
}

/// The normalised sine cardinal, `sin(pi*x) / (pi*x)`, with its removable
/// singularity filled in.
fn sinc(x: f64) -> f64 {
    if mathf::fabs(x) < 1e-12 {
        return 1.0;
    }
    let scaled = core::f64::consts::PI * x;
    mathf::sin(scaled) / scaled
}

/// `count` as a real number.
fn as_real(count: usize) -> f64 {
    f64::from(u16::try_from(count).unwrap_or(u16::MAX))
}

/// The Kaiser-windowed lowpass kernel at `offset` input frames from its
/// centre, over a `taps`-frame span, with its half-amplitude point at
/// `cutoff` cycles per input frame.
fn kernel(offset: f64, taps: usize, cutoff: f64, i0_beta: f64) -> f64 {
    let normalised = 2.0 * offset / as_real(taps);
    if mathf::fabs(normalised) >= 1.0 {
        return 0.0;
    }
    let window = bessel_i0(KAISER_BETA * mathf::sqrt(1.0 - normalised * normalised)) / i0_beta;
    2.0 * cutoff * sinc(2.0 * cutoff * offset) * window
}

/// The precomputed polyphase coefficients for one resampling ratio.
///
/// Built once per rate pair and shared by every stream that needs it: the
/// coefficients depend on the ratio alone, so one bank per (source rate, sink
/// rate) on a device serves all of that device's streams rather than each
/// carrying its own tens of kibibytes.
#[derive(Clone, Debug)]
pub struct FilterBank {
    ratio: Ratio,
    /// Rows are `phases + 1`: the extra row is the position exactly one input
    /// frame along, which the interpolation between rows needs as its upper
    /// end and which the exact case reaches when the remainder is at its top.
    phases: u32,
    taps: usize,
    coefficients: Vec<f32>,
}

impl FilterBank {
    /// Build the bank carrying `from` to `to`.
    ///
    /// Equal rates need no coefficients at all, so the bank is empty and every
    /// resampler over it copies.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfMemory`] when the coefficient table cannot be allocated.
    pub fn new(from: Rate, to: Rate) -> Result<Self, Errno> {
        Self::for_ratio(Ratio::between(from, to))
    }

    /// Build the bank for an already-reduced `ratio` — the form a drifting
    /// clock domain's correction is expressed in.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfMemory`] when the coefficient table cannot be allocated.
    pub fn for_ratio(ratio: Ratio) -> Result<Self, Errno> {
        if ratio.is_unity() {
            return Ok(Self {
                ratio,
                phases: 0,
                taps: 0,
                coefficients: Vec::new(),
            });
        }
        let phases = ratio.output().min(MAX_PHASES);
        let taps = taps_for(ratio);
        let rows = usize::try_from(phases).map_err(|_| Errno::OutOfRange)? + 1;
        let mut coefficients = fallible::filled(rows * taps, 0.0f32).ok_or(Errno::OutOfMemory)?;
        // Downsampling moves the anti-alias cutoff down with the output rate;
        // upsampling leaves it at the input's own band edge, since there is
        // nothing above it to fold back.
        let lower = f64::from(ratio.output().min(ratio.input()));
        let cutoff = CUTOFF * lower / f64::from(ratio.input());
        let i0_beta = bessel_i0(KAISER_BETA);
        for row in 0..rows {
            let fraction = f64::from(u32::try_from(row).unwrap_or(u32::MAX)) / f64::from(phases);
            let mut sum = 0.0;
            for tap in 0..taps {
                let offset = fraction + as_real(taps) / 2.0 - 1.0 - as_real(tap);
                let value = kernel(offset, taps, cutoff, i0_beta);
                coefficients[row * taps + tap] = as_f32(value);
                sum += value;
            }
            // Normalise each row to unit gain at DC. The windowed kernel's own
            // sum is within a part in ten thousand of one, and the residue
            // would otherwise show up as a level that wobbles with the phase.
            if sum > 0.0 {
                let scale = as_f32(1.0 / sum);
                for tap in &mut coefficients[row * taps..row * taps + taps] {
                    *tap *= scale;
                }
            }
        }
        Ok(Self {
            ratio,
            phases,
            taps,
            coefficients,
        })
    }

    /// The ratio this bank was built for.
    #[must_use]
    pub const fn ratio(&self) -> Ratio {
        self.ratio
    }

    /// Whether the bank is the copying one.
    #[must_use]
    pub const fn is_unity(&self) -> bool {
        self.ratio.is_unity()
    }

    /// Rows the bank holds below the extra end row.
    ///
    /// Equal to the ratio's denominator when that fits, which is the exact
    /// case; [`MAX_PHASES`] otherwise, which is the interpolated one.
    #[must_use]
    pub const fn phases(&self) -> u32 {
        self.phases
    }

    /// Whether every phase the ratio can produce lands on a row exactly, so no
    /// interpolation between rows ever happens.
    #[must_use]
    pub const fn is_exact(&self) -> bool {
        self.ratio.is_unity() || self.phases == self.ratio.output()
    }

    /// Input frames one output sample is computed from, which a decimating
    /// ratio scales up from [`BASE_TAPS`].
    #[must_use]
    pub const fn taps(&self) -> usize {
        self.taps
    }

    /// One phase row's taps, oldest input frame first.
    pub(crate) fn row(&self, phase: u32) -> &[f32] {
        let start = usize::try_from(phase).unwrap_or(0) * self.taps;
        self.coefficients
            .get(start..start + self.taps)
            .unwrap_or(&[])
    }
}

/// `value` as a coefficient.
#[allow(
    clippy::cast_possible_truncation,
    reason = "a filter coefficient is a small number near unity, and the pivot \
              the taps are applied in is f32 anyway, so carrying the design \
              precision past this point would buy nothing"
)]
fn as_f32(value: f64) -> f32 {
    value as f32
}

/// One stream's resampling state, driven over a shared [`FilterBank`].
///
/// The state is per stream and the coefficients are per rate pair, so the
/// bank is *passed to* [`process`](Self::process) rather than held here: a
/// service holding both in one stream record would otherwise need a
/// self-reference, and building a resampler per period would reset the
/// filter memory every period — an audible discontinuity.
///
/// Holds only the per-channel history the filter spans, so a stream costs a
/// few kibibytes rather than a copy of the coefficients.
#[derive(Debug)]
pub struct Resampler {
    /// The ratio the history was sized and stepped for. A [`FilterBank`] is
    /// determined entirely by its ratio, so this is what tells a bank that
    /// belongs to this resampler from one that does not.
    ratio: Ratio,
    channels: usize,
    /// The last [`FilterBank::taps`] input frames, interleaved, indexed by
    /// frame number modulo the tap count. Zero-initialised, which is the
    /// filter's ramp-in.
    history: Vec<f32>,
    /// One output position's blended tap vector, reused every frame so the
    /// per-period path allocates nothing.
    taps: Vec<f32>,
    /// Input frames pushed since the last reset.
    pushed: u64,
    /// The next output's integer input position.
    whole: u64,
    /// Its remainder over the ratio's denominator.
    remainder: u32,
}

impl Resampler {
    /// A resampler for `channels` interleaved channels, sized for `bank`.
    ///
    /// Every later [`process`](Self::process) must be handed a bank of the
    /// same ratio; the buffers are sized here and reused, so the per-period
    /// path allocates nothing.
    ///
    /// # Errors
    ///
    /// * [`Errno::OutOfRange`] — `channels` is zero or past the vocabulary's
    ///   maximum.
    /// * [`Errno::OutOfMemory`] — the history could not be allocated.
    pub fn new(bank: &FilterBank, channels: usize) -> Result<Self, Errno> {
        if channels == 0 || channels > MAX_CHANNELS {
            return Err(Errno::OutOfRange);
        }
        let (history, taps) = if bank.is_unity() {
            (Vec::new(), Vec::new())
        } else {
            (
                fallible::filled(bank.taps() * channels, 0.0f32).ok_or(Errno::OutOfMemory)?,
                fallible::filled(bank.taps(), 0.0f32).ok_or(Errno::OutOfMemory)?,
            )
        };
        Ok(Self {
            ratio: bank.ratio(),
            channels,
            history,
            taps,
            pushed: 0,
            whole: 0,
            remainder: 0,
        })
    }

    /// The ratio this resampler steps at, and therefore the only
    /// [`FilterBank`] it may be driven over.
    #[must_use]
    pub const fn ratio(&self) -> Ratio {
        self.ratio
    }

    /// Discard the filter's memory and return to the start of the stream.
    ///
    /// What a flush is made of: the frames that were in flight are gone, so
    /// the history they would have coloured is gone with them.
    pub fn reset(&mut self) {
        self.history.fill(0.0);
        self.pushed = 0;
        self.whole = 0;
        self.remainder = 0;
    }

    /// The filter's group delay, in input frames.
    ///
    /// Zero for the copying bank; half the tap span otherwise, which is the
    /// latency the resampling stage contributes to a stream's grant.
    #[must_use]
    pub fn latency_frames(&self) -> usize {
        self.taps.len() / 2
    }

    /// An upper bound on the output frames `input_frames` themselves produce,
    /// so a caller sizes its destination once rather than guessing.
    ///
    /// A destination that filled on the previous call left frames behind, and
    /// those are emitted before these — which is why [`Self::process`] reports
    /// what it actually moved rather than promising to move everything.
    #[must_use]
    pub fn max_output_frames(&self, input_frames: usize) -> usize {
        if self.ratio.is_unity() {
            return input_frames;
        }
        let produced =
            u64::try_from(input_frames).unwrap_or(u64::MAX) * u64::from(self.ratio.output());
        usize::try_from(produced / u64::from(self.ratio.input())).unwrap_or(usize::MAX) + 1
    }

    /// Resample interleaved frames from `input` into `output` over `bank`,
    /// returning the input frames consumed and the output frames produced.
    ///
    /// Consumption stops when `output` is full, so a caller that under-sized
    /// its destination loses nothing: the unconsumed input is offered again.
    ///
    /// # Errors
    ///
    /// * [`Errno::NotSupported`] — `bank` is not this resampler's: its ratio
    ///   differs from the one the history was sized and stepped for, so
    ///   filtering over it would silently produce the wrong audio.
    /// * [`Errno::LengthOutOfRange`] — either side is not a whole number of
    ///   frames; a partial frame would rotate every later channel.
    pub fn process(
        &mut self,
        bank: &FilterBank,
        input: &[f32],
        output: &mut [f32],
    ) -> Result<(usize, usize), Errno> {
        if bank.ratio() != self.ratio {
            return Err(Errno::NotSupported);
        }
        if !input.len().is_multiple_of(self.channels) || !output.len().is_multiple_of(self.channels)
        {
            return Err(Errno::LengthOutOfRange);
        }
        let in_frames = input.len() / self.channels;
        let out_frames = output.len() / self.channels;
        if self.ratio.is_unity() {
            let moved = in_frames.min(out_frames);
            output[..moved * self.channels].copy_from_slice(&input[..moved * self.channels]);
            self.pushed += u64::try_from(moved).unwrap_or(0);
            self.whole = self.pushed;
            return Ok((moved, moved));
        }
        let mut consumed = 0;
        let mut produced = 0;
        while consumed < in_frames {
            if produced >= out_frames {
                break;
            }
            self.push(&input[consumed * self.channels..(consumed + 1) * self.channels]);
            consumed += 1;
            while produced < out_frames && self.producible() {
                let slot = &mut output[produced * self.channels..(produced + 1) * self.channels];
                self.emit(bank, slot);
                produced += 1;
            }
        }
        Ok((consumed, produced))
    }

    /// Take one input frame into the history.
    fn push(&mut self, frame: &[f32]) {
        let slot = usize::try_from(self.pushed % self.modulus()).unwrap_or(0);
        self.history[slot * self.channels..slot * self.channels + self.channels]
            .copy_from_slice(frame);
        self.pushed += 1;
    }

    /// The tap count as the modulus the history ring is indexed by.
    fn modulus(&self) -> u64 {
        u64::try_from(self.taps.len()).unwrap_or(1).max(1)
    }

    /// Whether the next output's whole tap window is in the history.
    fn producible(&self) -> bool {
        self.window_base() <= self.pushed
    }

    /// One past the newest input frame the next output reads.
    fn window_base(&self) -> u64 {
        self.whole + u64::try_from(self.taps.len() / 2).unwrap_or(0) + 1
    }

    /// The channels the resampler was built for.
    #[must_use]
    pub const fn channels(&self) -> usize {
        self.channels
    }

    /// Produce one output frame and advance the position by the exact ratio.
    fn emit(&mut self, bank: &FilterBank, out: &mut [f32]) {
        let ratio = self.ratio;
        let denominator = u64::from(ratio.output());
        let scaled = u64::from(self.remainder) * u64::from(bank.phases());
        let phase = u32::try_from(scaled / denominator).unwrap_or(0);
        // Zero whenever the bank has a row per denominator step, which is what
        // makes the exact case exact: there is nothing between rows to blend.
        let blend = as_f32(
            f64::from(u32::try_from(scaled % denominator).unwrap_or(0)) / f64::from(ratio.output()),
        );
        self.blend_taps(bank, phase, blend);
        let base = self.window_base();
        let modulus = self.modulus();
        for (channel, slot) in out.iter_mut().enumerate().take(self.channels) {
            let mut sum = 0.0f32;
            for (tap, coefficient) in self.taps.iter().enumerate() {
                let index = usize::try_from((base + u64::try_from(tap).unwrap_or(0)) % modulus)
                    .unwrap_or(0);
                sum += self.history[index * self.channels + channel] * coefficient;
            }
            *slot = sum;
        }
        let advanced = u64::from(self.remainder) + u64::from(ratio.input());
        self.whole += advanced / denominator;
        self.remainder = u32::try_from(advanced % denominator).unwrap_or(0);
    }

    /// Load the tap vector for one output position into the reused scratch:
    /// the phase row, blended toward the next where the ratio lands between
    /// two.
    ///
    /// Built once per output frame rather than once per channel, so a
    /// multi-channel stream pays the blend once.
    fn blend_taps(&mut self, bank: &FilterBank, phase: u32, blend: f32) {
        let lower = bank.row(phase);
        if blend == 0.0 {
            for (slot, value) in self.taps.iter_mut().zip(lower) {
                *slot = *value;
            }
            return;
        }
        let upper = bank.row(phase + 1);
        for (slot, (low, high)) in self.taps.iter_mut().zip(lower.iter().zip(upper)) {
            *slot = low + (high - low) * blend;
        }
    }
}

#[cfg(test)]
#[path = "resample_tests.rs"]
mod tests;
