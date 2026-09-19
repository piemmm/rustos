//! Sample-format conversion: the saturating, total map between every encoding
//! in the vocabulary and the `f32` pivot the engine mixes in.
//!
//! # Why a pivot rather than thirty-six hand-written pairs
//!
//! The mixer accumulates in `f32`, so every stream is already decoded to that
//! pivot before anything is summed. Writing a direct converter per (source,
//! destination) pair beside that would be the same arithmetic spelled twice,
//! and the two spellings would eventually disagree. [`convert`] is therefore
//! total over all thirty-six pairs, implemented as decode-then-encode, with
//! a straight copy when the two formats are equal.
//!
//! # The pivot is exact for twenty-four bits and honest above them
//!
//! Every scale factor is a power of two, so an integer sample of twenty-four
//! bits or fewer divides into `f32` and multiplies back with no rounding at
//! all: [`decode`] followed by [`encode`] is the identity, which is what makes
//! the stack's bit-exactness claim a property rather than an aspiration.
//! [`SampleFormat::S32`] is the exception — `f32` carries twenty-four bits of
//! mantissa, so its low eight bits do not survive the pivot. That is stated
//! rather than hidden: no consumer format produces meaningful thirty-two-bit
//! integer audio, and a wider accumulator would cost every other path to
//! rescue a case nobody can hear.
//!
//! # Hostile samples are bounded before they reach a shared mix
//!
//! A client writes its own samples into a ring the mixer reads. A `NaN` would
//! turn every other stream sharing that sink into a `NaN`, and a float far
//! outside full scale would swamp them, so an out-of-spec sample is clamped
//! to the encoding's own full scale — and a non-finite one is silence —
//! before it can reach the accumulator. One tenant's numbers never bound
//! another's.
//!
//! # Dither narrows, and only narrows
//!
//! Quantising to fewer bits than the source carried correlates the truncation
//! error with the signal, which is audible as distortion rather than as hiss.
//! [`Dither::TriangularPdf`] decorrelates it. It is applied **only** where the
//! destination is genuinely narrower than the material: adding noise to a path
//! that would otherwise be bit-exact would destroy the property, so the
//! narrowing test is the mixer's to make and the answer is carried in the
//! [`Dither`] it passes.

use tairix_abi::driver::audio::SampleFormat;
use tairix_abi::Errno;
use tairix_cpuops::{
    Candidate, CoreKey, CpuFeatureSet, Decision, Family, FamilyId, Selection, Selector,
};
use tairix_rng::{NonCryptoRng, RandU64};
use tairix_sync::OnceCell;
use tairix_util::mathf;

/// The dispatch family's stable id — the log and operator-pin key.
pub const FAMILY_ID: FamilyId = FamilyId("audio-convert");

/// Name of the portable kernel, which is the mandatory baseline.
pub const BASELINE_NAME: &str = "audio-convert-portable";

/// Decode interleaved `format` samples from `src` into the `f32` pivot.
pub type DecodeFn = fn(SampleFormat, &[u8], &mut [f32]) -> usize;

/// Encode pivot samples from `src` into interleaved `format` bytes.
pub type EncodeFn = fn(SampleFormat, &[f32], &mut [u8]) -> usize;

/// One implementation of the conversion kernel.
///
/// The two halves travel together because they are one op: a candidate that
/// decoded faster but encoded differently would not be the same converter, and
/// the self-verify compares a decode-and-re-encode round trip against the
/// portable pair so neither half can drift alone.
#[derive(Copy, Clone, Debug)]
pub struct ConvertKernel {
    /// Format bytes to pivot samples.
    pub decode: DecodeFn,
    /// Pivot samples to format bytes.
    pub encode: EncodeFn,
}

/// The portable kernel: correct on every target, and the reference every
/// accelerated candidate is verified against.
pub const PORTABLE: ConvertKernel = ConvertKernel {
    decode: decode_portable,
    encode: encode_portable,
};

/// The kernel resolved for this process, or unset before [`resolve`] runs.
static RESOLVED: OnceCell<ConvertKernel> = OnceCell::new();

/// Accelerated candidates available on this build.
///
/// Empty today: the portable kernel is a per-sample shift and multiply that
/// the compiler already vectorises, and a hand-written candidate would be
/// architecture-specific code with no measurement behind it. The seam exists
/// so one can be added under the framework's capability gate and mandatory
/// self-verify rather than beside them.
const CANDIDATES: &[Candidate<ConvertKernel>] = &[];

/// Samples one self-verify vector carries.
const VERIFY_SAMPLES: usize = 8;

/// Bytes one self-verify digest carries: the decoded pivot block, then the
/// re-encoded destination bytes.
const VERIFY_DIGEST_LEN: usize = VERIFY_SAMPLES * 4 + VERIFY_SAMPLES * 4;

/// One self-verify input: a byte image in a named encoding.
struct Vector {
    format: SampleFormat,
    bytes: &'static [u8],
}

/// Both halves of a round trip, so a candidate cannot hide a decode error
/// behind a compensating encode error.
type Digest = [u8; VERIFY_DIGEST_LEN];

/// The self-verify vectors: for every encoding, silence, both full-scale
/// extremes, and an alternating pattern that a mis-sized stride would smear.
const VECTORS: &[Vector] = &[
    Vector {
        format: SampleFormat::U8,
        bytes: &[0x80, 0x00, 0xFF, 0x01, 0xFE, 0x40, 0xC0, 0x7F],
    },
    Vector {
        format: SampleFormat::S16,
        bytes: &[
            0x00, 0x00, 0x00, 0x80, 0xFF, 0x7F, 0x01, 0x80, 0xFF, 0xFF, 0x00, 0x40, 0x00, 0xC0,
            0x34, 0x12,
        ],
    },
    Vector {
        format: SampleFormat::S24,
        bytes: &[
            0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0xFF, 0xFF, 0x7F, 0x01, 0x00, 0x80, 0xFF, 0xFF,
            0xFF, 0x00, 0x00, 0x40, 0x00, 0x00, 0xC0, 0x56, 0x34, 0x12,
        ],
    },
    Vector {
        format: SampleFormat::S24In32,
        bytes: &[
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0xFF, 0xFF, 0xFF, 0x7F, 0x00, 0x01, 0x00,
            0x80, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0xC0, 0xFF,
            0x56, 0x34, 0x12, 0x00,
        ],
    },
    Vector {
        format: SampleFormat::S32,
        bytes: &[
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0xFF, 0xFF, 0xFF, 0x7F, 0x01, 0x00,
            0x00, 0x80, 0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0xC0,
            0x78, 0x56, 0x34, 0x12,
        ],
    },
    Vector {
        format: SampleFormat::F32,
        bytes: &[
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0xBF, 0x00, 0x00, 0x80, 0x3F, 0x00, 0x00,
            0x00, 0x3F, 0x00, 0x00, 0x00, 0xBF, 0x00, 0x00, 0x80, 0x7F, 0x00, 0x00, 0xC0, 0x7F,
            0x00, 0x00, 0x00, 0x80,
        ],
    },
];

/// Run one candidate over one vector, producing the comparable digest.
fn run(kernel: ConvertKernel, input: &Vector) -> Digest {
    let mut pivot = [0.0f32; VERIFY_SAMPLES];
    let decoded = (kernel.decode)(input.format, input.bytes, &mut pivot);
    let mut digest = [0u8; VERIFY_DIGEST_LEN];
    for (slot, sample) in pivot.iter().enumerate().take(decoded) {
        digest[slot * 4..slot * 4 + 4].copy_from_slice(&sample.to_le_bytes());
    }
    let mut encoded = [0u8; VERIFY_SAMPLES * 4];
    let bytes = (kernel.encode)(input.format, &pivot[..decoded], &mut encoded);
    digest[VERIFY_SAMPLES * 4..VERIFY_SAMPLES * 4 + bytes].copy_from_slice(&encoded[..bytes]);
    digest
}

/// The portable reference the self-verify compares against.
fn reference(input: &Vector) -> Digest {
    run(PORTABLE, input)
}

/// The dispatch family: candidates, the mandatory portable baseline, and the
/// reference plus vectors every survivor must reproduce.
fn family() -> Family<'static, ConvertKernel, Vector, Digest> {
    Family {
        id: FAMILY_ID,
        // Every candidate is bit-identical by construction — the self-verify
        // refuses one that is not — and whether a vector kernel beats the
        // scalar one at a period's block size depends on the machine, so the
        // choice is a measurement rather than a declared order.
        selection: Selection::ByBenchmark,
        candidates: CANDIDATES,
        baseline: Candidate {
            name: BASELINE_NAME,
            requires: &[],
            impl_: PORTABLE,
        },
        reference,
        run,
        vectors: VECTORS,
    }
}

/// Resolve the conversion kernel for this process from the delivered
/// `features`, installing the winner and returning the typed decision.
///
/// Idempotent: the first call wins the installed kernel, and a later one
/// re-selects for the record without disturbing it. Never panics, and falls
/// closed to [`PORTABLE`].
#[must_use = "record the Decision through the audit log, or bind it to `_`"]
pub fn resolve(features: CpuFeatureSet) -> Decision {
    let selected = Selector::new().select(&family(), features, CoreKey(features.bits()), None);
    let _ = RESOLVED.set(selected.impl_);
    selected.decision
}

/// The resolved kernel, or [`PORTABLE`] before [`resolve`] has run.
#[must_use]
pub fn kernel() -> ConvertKernel {
    match RESOLVED.get() {
        Ok(Some(resolved)) => *resolved,
        _ => PORTABLE,
    }
}

/// Decode as many whole `format` samples from `src` as `out` holds, returning
/// how many were written.
///
/// A non-finite or out-of-scale sample lands as silence or full scale rather
/// than reaching a shared accumulator.
pub fn decode(format: SampleFormat, src: &[u8], out: &mut [f32]) -> usize {
    (kernel().decode)(format, src, out)
}

/// Encode as many pivot samples from `src` as `out` holds bytes for, returning
/// how many bytes were written. Saturating: a sample past full scale lands at
/// full scale, never wrapped to the opposite deflection.
pub fn encode(format: SampleFormat, src: &[f32], out: &mut [u8]) -> usize {
    (kernel().encode)(format, src, out)
}

/// Convert whole samples from one encoding to another through the pivot,
/// returning the bytes written to `out`.
///
/// `scratch` is the caller's reusable pivot buffer, so the conversion
/// allocates nothing; its length bounds how many samples one call converts.
/// Equal encodings need no pivot and are copied whole, bounded by `out`.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] when `scratch` is empty, or when `out` cannot
/// hold what `src` offers — a short destination is refused rather than
/// half-filled, since a partial conversion would leave the caller guessing
/// which samples arrived.
pub fn convert(
    from: SampleFormat,
    to: SampleFormat,
    src: &[u8],
    out: &mut [u8],
    scratch: &mut [f32],
) -> Result<usize, Errno> {
    let offered = src.len() / from.bytes_per_sample();
    if from == to {
        // Equal encodings have nothing to round-trip, and a copy is the only
        // spelling that is exact for every format including `S32`.
        let bytes = offered * from.bytes_per_sample();
        let Some(slot) = out.get_mut(..bytes) else {
            return Err(Errno::BufferTooSmall);
        };
        slot.copy_from_slice(&src[..bytes]);
        return Ok(bytes);
    }
    if scratch.is_empty() {
        return Err(Errno::BufferTooSmall);
    }
    let samples = offered.min(scratch.len());
    if out.len() < samples * to.bytes_per_sample() {
        return Err(Errno::BufferTooSmall);
    }
    let decoded = decode(from, src, &mut scratch[..samples]);
    Ok(encode(to, &scratch[..decoded], out))
}

/// Fill `out` with `format`'s silent byte.
///
/// Zero for every signed and floating encoding, mid-scale for the unsigned
/// one — where a zero byte would be full negative deflection and a gap filled
/// with it would click.
pub fn silence(format: SampleFormat, out: &mut [u8]) {
    out.fill(format.silence_byte());
}

/// Whether quantising material of `source_bits` into `destination` throws away
/// resolution, which is the only condition under which dither belongs.
#[must_use]
pub fn narrows(source_bits: u32, destination: SampleFormat) -> bool {
    !destination.is_float() && destination.valid_bits() < source_bits
}

/// Whether a quantisation step is dithered.
///
/// Deliberately has no [`Default`]: the engine's default is the triangular
/// one ([`crate::Mixer::new`] sets it), and a type-level default of the other
/// value would read as contradicting that.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Dither {
    /// Quantise straight. The only correct setting where the destination is at
    /// least as wide as the material, because dither there would add noise to
    /// a path that was exact.
    None,
    /// Add triangular-probability noise of one destination step, peak to peak,
    /// before quantising. The standard choice: it decorrelates the error from
    /// the signal at the cost of a little more noise power than a rectangular
    /// distribution, and leaves no modulation of the noise floor by the signal.
    TriangularPdf,
}

/// The noise source a dithered quantisation draws from.
///
/// Predictable by construction: dither wants decorrelation, not
/// unpredictability, and the tree's non-cryptographic generator is the one
/// place that algorithm lives. A seed fixes the sequence, so a test can assert
/// a dithered result exactly.
#[derive(Debug)]
pub struct DitherSource {
    rng: NonCryptoRng,
}

impl DitherSource {
    /// A source whose sequence is fixed by `seed`.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self {
            rng: NonCryptoRng::seed_from_u64(seed),
        }
    }

    /// One triangular-distributed sample in `-1.0..1.0`, the difference of two
    /// independent uniforms drawn from one word.
    fn next_triangular(&mut self) -> f32 {
        let word = self.rng.next_u64();
        uniform_24(word) - uniform_24(word >> 32)
    }

    /// Add one step of triangular noise to every sample of `block`, scaled to
    /// `destination`'s own quantisation step.
    ///
    /// A float destination has no step to dither to, so it is left alone.
    pub fn apply(&mut self, destination: SampleFormat, block: &mut [f32]) {
        if destination.is_float() {
            return;
        }
        let step = 1.0 / full_scale_f32(destination);
        for sample in block {
            *sample += self.next_triangular() * step;
        }
    }
}

/// The low twenty-four bits of `word` as a uniform in `0.0..1.0`.
///
/// Twenty-four bits over two to the twenty-fourth: every value lands exactly
/// on the pivot's grid, so the distribution has no gaps or clumps.
fn uniform_24(word: u64) -> f32 {
    let bits = i32::try_from(word & 0x00FF_FFFF).unwrap_or(0);
    to_pivot(bits) / 16_777_216.0
}

/// The magnitude one unit of `format` occupies in the pivot: the scale an
/// integer sample is divided by, and one for a float.
fn full_scale_f32(format: SampleFormat) -> f32 {
    match format {
        SampleFormat::U8 => 128.0,
        SampleFormat::S16 => 32_768.0,
        SampleFormat::S24 | SampleFormat::S24In32 => 8_388_608.0,
        SampleFormat::S32 => 2_147_483_648.0,
        SampleFormat::F32 => 1.0,
    }
}

/// The scale and the integer range one quantisation step of `format` spans.
///
/// The range is the encoding's own, which is asymmetric for every
/// two's-complement width: one more step of negative deflection than positive.
fn integer_domain(format: SampleFormat) -> Option<(f64, i32, i32)> {
    match format {
        SampleFormat::U8 => Some((128.0, -128, 127)),
        SampleFormat::S16 => Some((32_768.0, -32_768, 32_767)),
        SampleFormat::S24 | SampleFormat::S24In32 => Some((8_388_608.0, -8_388_608, 8_388_607)),
        SampleFormat::S32 => Some((2_147_483_648.0, i32::MIN, i32::MAX)),
        SampleFormat::F32 => None,
    }
}

/// `value` as a pivot sample.
#[allow(
    clippy::cast_precision_loss,
    reason = "every caller's value is inside the twenty-four-bit range f32 \
              represents exactly, bar the S32 sample whose low eight bits the \
              pivot documents that it cannot carry"
)]
fn to_pivot(value: i32) -> f32 {
    value as f32
}

/// `sample` clamped into the pivot's nominal range, with a non-finite value
/// answering silence.
///
/// Negative zero survives, because a float source that wrote it is entitled to
/// read it back unchanged.
fn clamp_unit(sample: f32) -> f32 {
    if !sample.is_finite() {
        return 0.0;
    }
    if sample > 1.0 {
        return 1.0;
    }
    if sample < -1.0 {
        return -1.0;
    }
    sample
}

/// Quantise one pivot sample onto `format`'s integer grid, saturating at both
/// ends and rounding halves away from zero so the two deflections round alike.
fn quantise(sample: f32, scale: f64, min: i32, max: i32) -> i32 {
    let scaled = f64::from(clamp_unit(sample)) * scale;
    let nearest = if scaled >= 0.0 {
        mathf::floor(scaled + 0.5)
    } else {
        mathf::ceil(scaled - 0.5)
    };
    mathf::round_i32(mathf::clamp(nearest, f64::from(min), f64::from(max)))
}

/// Sign-extend the three packed bytes of a 24-bit sample into an `i32`.
fn read_s24(bytes: &[u8]) -> i32 {
    let sign = if bytes[2] & 0x80 == 0 { 0x00 } else { 0xFF };
    i32::from_le_bytes([bytes[0], bytes[1], bytes[2], sign])
}

/// The portable decode: interleaved `format` bytes to pivot samples.
fn decode_portable(format: SampleFormat, src: &[u8], out: &mut [f32]) -> usize {
    let width = format.bytes_per_sample();
    let count = (src.len() / width).min(out.len());
    let scale = full_scale_f32(format);
    for (slot, sample) in out.iter_mut().enumerate().take(count) {
        let raw = &src[slot * width..slot * width + width];
        *sample = match format {
            SampleFormat::U8 => (f32::from(raw[0]) - 128.0) / scale,
            SampleFormat::S16 => f32::from(i16::from_le_bytes([raw[0], raw[1]])) / scale,
            SampleFormat::S24 => to_pivot(read_s24(raw)) / scale,
            // A value outside the twenty-four bits this encoding carries is
            // not a sample of it, so it lands at the encoding's own full scale
            // rather than at up to two hundred and fifty-six times it.
            SampleFormat::S24In32 => {
                let word = i32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
                to_pivot(word.clamp(-8_388_608, 8_388_607)) / scale
            }
            SampleFormat::S32 => {
                to_pivot(i32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]])) / scale
            }
            SampleFormat::F32 => clamp_unit(f32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]])),
        };
    }
    count
}

/// The portable encode: pivot samples to interleaved `format` bytes.
fn encode_portable(format: SampleFormat, src: &[f32], out: &mut [u8]) -> usize {
    let width = format.bytes_per_sample();
    let count = src.len().min(out.len() / width);
    for (slot, sample) in src.iter().enumerate().take(count) {
        let raw = &mut out[slot * width..slot * width + width];
        match integer_domain(format) {
            None => raw.copy_from_slice(&clamp_unit(*sample).to_le_bytes()),
            Some((scale, min, max)) => {
                let value = quantise(*sample, scale, min, max);
                match format {
                    SampleFormat::U8 => raw[0] = u8::try_from(value + 128).unwrap_or(0x80),
                    SampleFormat::S16 => {
                        raw.copy_from_slice(&i16::try_from(value).unwrap_or(0).to_le_bytes());
                    }
                    SampleFormat::S24 => raw.copy_from_slice(&value.to_le_bytes()[..3]),
                    SampleFormat::S24In32 | SampleFormat::S32 => {
                        raw.copy_from_slice(&value.to_le_bytes());
                    }
                    // The float encoding has no integer domain, so this arm is
                    // unreachable; answering silence keeps the function total.
                    SampleFormat::F32 => raw.fill(0),
                }
            }
        }
    }
    count * width
}

#[cfg(test)]
#[path = "convert_tests.rs"]
mod tests;
