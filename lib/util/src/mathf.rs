//! Bounded `f64` maths for `no_std` geometry.
//!
//! `floor`, `sqrt`, `sin` and friends live in `std`, where they call the
//! platform libm, so a `no_std` crate cannot reach them. One first-party copy
//! here keeps an external libm out of the trusted computing base and makes
//! every consumer — the glyph rasteriser, the SVG decoder, the figure engine,
//! the world generator — round and rotate identically on every target.
//!
//! That is a cross-target contract: the same source yields the same bits on
//! `x86_64`, `aarch64`, `riscv64` and `wasm32`. So everything here is built
//! from what IEEE 754 defines exactly — the basic arithmetic, plus the square
//! root and integer rounding every conforming implementation must round
//! correctly. [`sqrt`], [`floor`] and [`ceil`] therefore take the toolchain's
//! own forms: an instruction where the target has one, and on the soft-float
//! `x86_64` target the correctly rounded routines of the compiler runtime that
//! already performs its every addition — part of the toolchain, not a libm.
//! Those forms are reachable from `core` only as `core::f64::math`, behind
//! `core_float_math`; once they are stable as inherent methods the calls
//! become `x.sqrt()`, `x.floor()` and `x.ceil()` and the gate goes.
//!
//! The transcendentals have no exact form anywhere, so each is evaluated in
//! one fixed order of basic operations, with no fused multiply-add: fdlibm's
//! range reductions and minimax polynomials, within an ulp of the true value.
//!
//! Every function is total: it returns a finite answer for every finite input
//! and a defined one for the degenerate cases (a negative square root, a
//! vertical `atan2`, an out-of-domain `acos`, an infinite or `NaN` angle), so
//! no caller has to guard against a `NaN` it cannot render.

use core::f64::consts::{FRAC_2_PI, FRAC_PI_2, FRAC_PI_4, LOG2_E, PI};
use core::f64::math;

/// The largest integer not greater than `x`, and `0.0` for `NaN`.
#[must_use]
pub fn floor(x: f64) -> f64 {
    if x.is_nan() {
        0.0
    } else {
        math::floor(x)
    }
}

/// The smallest integer not less than `x`, and `0.0` for `NaN`.
#[must_use]
pub fn ceil(x: f64) -> f64 {
    if x.is_nan() {
        0.0
    } else {
        math::ceil(x)
    }
}

/// The magnitude of `x`.
#[must_use]
pub fn fabs(x: f64) -> f64 {
    if x < 0.0 {
        -x
    } else {
        x
    }
}

/// Round `x` to the nearest integer, halves toward positive infinity, and
/// `0.0` for `NaN`.
///
/// Decided on `x - floor(x)`, which is exact wherever it falls below a half,
/// so the choice never errs; `floor(x + 0.5)` rounds the sum first and so takes
/// `0.49999999999999994`, and every odd integer past 2^52, up by one.
#[must_use]
pub fn round(x: f64) -> f64 {
    let down = floor(x);
    if x - down >= 0.5 {
        down + 1.0
    } else {
        down
    }
}

/// The greater of `a` and `b`. Inputs are always non-`NaN`.
#[must_use]
pub fn fmax(a: f64, b: f64) -> f64 {
    if a > b {
        a
    } else {
        b
    }
}

/// The lesser of `a` and `b`. Inputs are always non-`NaN`.
#[must_use]
pub fn fmin(a: f64, b: f64) -> f64 {
    if a < b {
        a
    } else {
        b
    }
}

/// `x` clamped to `lo..=hi` (with `lo <= hi`).
#[must_use]
pub fn clamp(x: f64, lo: f64, hi: f64) -> f64 {
    fmin(fmax(x, lo), hi)
}

/// [`round`], returned as an `i32`.
///
/// Saturating rather than wrapping: a coordinate a hostile document drove far
/// out of range clamps to the extreme instead of wrapping to the opposite
/// side of the canvas.
#[allow(
    clippy::cast_possible_truncation,
    reason = "the value is clamped into i32's range on the line above, so the \
              truncation the lint warns about cannot occur"
)]
#[must_use]
pub fn round_i32(x: f64) -> i32 {
    clamp(round(x), f64::from(i32::MIN), f64::from(i32::MAX)) as i32
}

/// The correctly rounded non-negative square root of `x`: `0.0` for a
/// negative, zero or `NaN` input, and `+∞` for `+∞`.
#[must_use]
pub fn sqrt(x: f64) -> f64 {
    if x.is_nan() || x <= 0.0 {
        return 0.0;
    }
    math::sqrt(x)
}

/// The length of the vector `(x, y)`, computed without squaring a magnitude
/// that would overflow.
#[must_use]
pub fn hypot(x: f64, y: f64) -> f64 {
    let (ax, ay) = (fabs(x), fabs(y));
    let big = fmax(ax, ay);
    if big == 0.0 {
        return 0.0;
    }
    let small = fmin(ax, ay) / big;
    big * sqrt(1.0 + small * small)
}

/// Added and subtracted again, rounds a double below 2^51 to the nearest
/// integer without leaving the basic operations.
const TO_INTEGER: f64 = 1.5 / f64::EPSILON;

// `PI/2` as three 33-bit parts, each with the tail the one before it
// leaves, so a whole number of quarter turns below 2^20 times any part is
// exact: fdlibm's `__rem_pio2` constants.
const PIO2_1: f64 = 1.570_796_326_734_125_6;
const PIO2_1T: f64 = 6.077_100_506_506_192e-11;
const PIO2_2: f64 = 6.077_100_506_303_966e-11;
const PIO2_2T: f64 = 2.022_266_248_795_950_6e-21;
const PIO2_3: f64 = 2.022_266_248_711_166_5e-21;
const PIO2_3T: f64 = 8.478_427_660_368_9e-32;

/// The largest angle the parts of `PI/2` still reduce exactly; past it
/// [`reduce_large`] takes over.
const REDUCIBLE: f64 = 1_048_576.0 * FRAC_PI_2;

/// The first 1216 bits of `2/PI` after the binary point, most significant
/// first, behind a zero word standing for its integer part: as far as the
/// largest double's reduction reaches.
const TWO_OVER_PI: [u64; 20] = [
    0,
    0xa2f9_836e_4e44_1529,
    0xfc27_57d1_f534_ddc0,
    0xdb62_9599_3c43_9041,
    0xfe51_63ab_debb_c561,
    0xb724_6e3a_424d_d2e0,
    0x0649_2eea_09d1_921c,
    0xfe1d_eb1c_b129_a73e,
    0xe882_35f5_2ebb_4484,
    0xe99c_7026_b45f_7e41,
    0x3991_d639_8353_39f4,
    0x9c84_5f8b_bdf9_283b,
    0x1ff8_97ff_de05_980f,
    0xef2f_118b_5a0a_6d1f,
    0x6d36_7ecf_27cb_09b7,
    0x4f46_3f66_9e5f_ea2d,
    0x7527_bac7_ebe5_f17b,
    0x3d07_39f7_8a52_92ea,
    0x6bfb_5fb1_1f8d_5d08,
    0x5603_3046_fc7b_6bab,
];

/// `PI/2` in fixed point with 126 fraction bits, rounded to nearest.
const PIO2_FIXED: u128 = 0x6487_ed51_10b4_611a_6263_3145_c06e_0e69;

/// `2^-128`, the weight of a fixed-point remainder's lowest bit.
const REMAINDER_ULP: f64 = f64::from_bits((1023 - 128) << 52);

/// The low 64 bits of a `u128`.
const LOW_WORD: u128 = (1 << 64) - 1;

/// The biased exponent of `x`: its magnitude, counted in bits.
#[allow(
    clippy::cast_possible_truncation,
    reason = "an eleven-bit field, which a u16 holds exactly"
)]
fn exponent(x: f64) -> u16 {
    ((x.to_bits() >> 52) & 0x7ff) as u16
}

/// `rest` less `turns` more parts of `PI/2`: the new remainder, the tail its
/// rounding lost, and their sum.
fn subtract_part(rest: f64, turns: f64, part: f64, part_tail: f64) -> (f64, f64, f64) {
    let taken = turns * part;
    let next = rest - taken;
    let lost = turns * part_tail - ((rest - next) - taken);
    (next, lost, next - lost)
}

/// `x` less its nearest whole number of quarter turns, as a remainder within
/// about `PI/4` of zero — carried as a head and the tail its rounding lost —
/// and that number of turns modulo four. An infinite or `NaN` angle has no
/// direction and reduces as zero.
///
/// fdlibm's `__rem_pio2` for moderate angles: Cody and Waite's subtraction of
/// `PI/2` in parts, taking a further part wherever the last cancelled so many
/// bits that too few are left, which near a multiple of `PI/2` is what keeps
/// the tiny remainder accurate to its own last bit.
fn reduce(x: f64) -> (f64, f64, u8) {
    if !x.is_finite() {
        return (0.0, 0.0, 0);
    }
    if fabs(x) > REDUCIBLE {
        return reduce_large(x);
    }
    let turns = (x * FRAC_2_PI + TO_INTEGER) - TO_INTEGER;
    let first = x - turns * PIO2_1;
    let first_lost = turns * PIO2_1T;
    let (mut rest, mut lost, mut head) = (first, first_lost, first - first_lost);
    if exponent(x).saturating_sub(exponent(head)) > 16 {
        (rest, lost, head) = subtract_part(rest, turns, PIO2_2, PIO2_2T);
        if exponent(x).saturating_sub(exponent(head)) > 49 {
            (rest, lost, head) = subtract_part(rest, turns, PIO2_3, PIO2_3T);
        }
    }
    let tail = (rest - head) - lost;
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "turns is a whole number no larger than 2^20, far inside \
                  i64, and only its two low bits are kept"
    )]
    let quadrant = ((turns as i64) & 3) as u8;
    (head, tail, quadrant)
}

/// [`reduce`] for a finite angle past [`REDUCIBLE`]: Payne and Hanek's, in
/// integers, so it is exact whatever the angle's size.
///
/// With `x = mantissa * 2^scale`, a bit of `2/PI` weighing `2^-i` adds
/// `mantissa * 2^(scale - i)` quarter turns, a multiple of four once
/// `i <= scale - 2`. So four words of `2/PI` from the one holding bit
/// `scale - 1` on give the quadrant and a 128-bit fraction of a turn, short of
/// the true one by under 2^-138 — against a double's nearest approach to a
/// quarter turn, about 2^-62.
fn reduce_large(x: f64) -> (f64, f64, u8) {
    let mantissa = u128::from((x.to_bits() & ((1 << 52) - 1)) | (1 << 52));
    // How far into the table bit `scale - 1` of `2/PI` sits.
    let offset = exponent(x).saturating_sub(1013);
    let first = usize::from(offset / 64);
    let shift = 126 - offset % 64;
    let mut limbs = [0_u128; 4];
    let mut carry = 0;
    for (limb, &word) in limbs
        .iter_mut()
        .zip(TWO_OVER_PI[first..first + 4].iter().rev())
    {
        let sum = mantissa * u128::from(word) + carry;
        *limb = sum & LOW_WORD;
        carry = sum >> 64;
    }
    let (low, high) = (limbs[0] | limbs[1] << 64, limbs[2] | limbs[3] << 64);
    let fraction = (low >> shift) | (high << (128 - shift));
    #[allow(
        clippy::cast_possible_truncation,
        reason = "only the two low bits, the quarter turns modulo four, are kept"
    )]
    let turns = ((high >> shift) as u8) & 3;
    let past_half = fraction >> 127 == 1;
    let (distance, turns) = if past_half {
        (fraction.wrapping_neg(), turns + 1)
    } else {
        (fraction, turns)
    };
    let (product_high, product_low) = widening_mul(distance, PIO2_FIXED);
    let (head, tail) = fixed_to_pair((product_high << 2) | (product_low >> 126));
    let (head, tail) = if past_half {
        (-head, -tail)
    } else {
        (head, tail)
    };
    if x.is_sign_negative() {
        (-head, -tail, turns.wrapping_neg() & 3)
    } else {
        (head, tail, turns & 3)
    }
}

/// The high and low halves of the 256-bit product `a * b`.
fn widening_mul(a: u128, b: u128) -> (u128, u128) {
    let (a_high, a_low) = (a >> 64, a & LOW_WORD);
    let (b_high, b_low) = (b >> 64, b & LOW_WORD);
    let (middle, middle_carry) = (a_high * b_low).overflowing_add(a_low * b_high);
    let (low, low_carry) = (a_low * b_low).overflowing_add(middle << 64);
    let high =
        a_high * b_high + (middle >> 64) + (u128::from(middle_carry) << 64) + u128::from(low_carry);
    (high, low)
}

/// `fixed * 2^-128` as the nearest double and the part of it that rounding
/// dropped.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    reason = "the head is `fixed` rounded to the nearest double, an integer \
              below 2^128 that converts back exactly; the two differ by under \
              2^75, which the wrapping difference read as an `i128` holds \
              exactly, and whose own rounding is far below the head's last bit"
)]
fn fixed_to_pair(fixed: u128) -> (f64, f64) {
    let head = fixed as f64;
    let tail = fixed.wrapping_sub(head as u128) as i128 as f64;
    (head * REMAINDER_ULP, tail * REMAINDER_ULP)
}

// fdlibm's `__kernel_sin`: `sin(x) ~ x + S1 x^3 + … + S6 x^13` over
// `-PI/4..=PI/4`.
const S1: f64 = -0.166_666_666_666_666_32;
const S2: f64 = 0.008_333_333_333_322_49;
const S3: f64 = -0.000_198_412_698_298_579_5;
const S4: f64 = 2.755_731_370_707_006_8e-6;
const S5: f64 = -2.505_076_025_340_686_3e-8;
const S6: f64 = 1.589_690_995_211_55e-10;

// fdlibm's `__kernel_cos`: `cos(x) ~ 1 - x^2/2 + C1 x^4 + … + C6 x^14` over
// `-PI/4..=PI/4`.
const C1: f64 = 0.041_666_666_666_666_6;
const C2: f64 = -0.001_388_888_888_887_411;
const C3: f64 = 2.480_158_728_947_673e-5;
const C4: f64 = -2.755_731_435_139_066_3e-7;
const C5: f64 = 2.087_572_321_298_175e-9;
const C6: f64 = -1.135_964_755_778_819_5e-11;

/// `sin(head + tail)` for a remainder from [`reduce`].
fn sin_kernel(head: f64, tail: f64) -> f64 {
    let z = head * head;
    let w = z * z;
    let r = S2 + z * (S3 + z * S4) + z * w * (S5 + z * S6);
    let v = z * head;
    head - ((z * (0.5 * tail - v * r) - tail) - v * S1)
}

/// `cos(head + tail)` for a remainder from [`reduce`].
fn cos_kernel(head: f64, tail: f64) -> f64 {
    let z = head * head;
    let w = z * z;
    let r = z * (C1 + z * (C2 + z * C3)) + w * w * (C4 + z * (C5 + z * C6));
    let half = 0.5 * z;
    let near = 1.0 - half;
    near + (((1.0 - near) - half) + (z * r - head * tail))
}

/// The sine of a reduced angle `quadrant` quarter turns on from its remainder.
fn sin_of(head: f64, tail: f64, quadrant: u8) -> f64 {
    match quadrant & 3 {
        0 => sin_kernel(head, tail),
        1 => cos_kernel(head, tail),
        2 => -sin_kernel(head, tail),
        _ => -cos_kernel(head, tail),
    }
}

/// The sine of `x` radians.
#[must_use]
pub fn sin(x: f64) -> f64 {
    let (head, tail, quadrant) = reduce(x);
    sin_of(head, tail, quadrant)
}

/// The cosine of `x` radians.
#[must_use]
pub fn cos(x: f64) -> f64 {
    let (head, tail, quadrant) = reduce(x);
    sin_of(head, tail, quadrant + 1)
}

/// The tangent of `x` radians.
///
/// A pole (where the cosine vanishes) yields a large finite value rather than
/// an infinity, so a skew transform built from it still produces a drawable —
/// if extreme — shape instead of a `NaN` that would erase it.
#[must_use]
pub fn tan(x: f64) -> f64 {
    let (head, tail, quadrant) = reduce(x);
    let (s, c) = (
        sin_of(head, tail, quadrant),
        sin_of(head, tail, quadrant + 1),
    );
    if fabs(c) < 1e-12 {
        return if s < 0.0 { -1e12 } else { 1e12 };
    }
    s / c
}

// fdlibm's `atan`: `atan(t) ~ t - t (AT0 t^2 + … + AT10 t^22)` for
// `|t| < 7/16`.
const AT0: f64 = 0.333_333_333_333_329_3;
const AT1: f64 = -0.199_999_999_998_764_83;
const AT2: f64 = 0.142_857_142_725_034_66;
const AT3: f64 = -0.111_111_104_054_623_56;
const AT4: f64 = 0.090_908_871_334_365_07;
const AT5: f64 = -0.076_918_762_050_448_3;
const AT6: f64 = 0.066_610_731_373_875_31;
const AT7: f64 = -0.058_335_701_337_905_735;
const AT8: f64 = 0.049_768_779_946_159_324;
const AT9: f64 = -0.036_531_572_744_216_916;
const AT10: f64 = 0.016_285_820_115_365_782;

// `atan` of the breakpoints `1/2`, `1`, `3/2` and infinity, each as a head
// and the tail its rounding lost.
const ATAN_HALF: (f64, f64) = (0.463_647_609_000_806_1, 2.269_877_745_296_168_7e-17);
const ATAN_ONE: (f64, f64) = (FRAC_PI_4, 3.061_616_997_868_383e-17);
const ATAN_THREE_HALVES: (f64, f64) = (0.982_793_723_247_329, 1.390_331_103_123_099_8e-17);
const ATAN_INFINITY: (f64, f64) = (FRAC_PI_2, 6.123_233_995_736_766e-17);

/// 2^66, past which `atan` is `PI/2` to the last bit.
const ATAN_SATURATES: f64 = 7.378_697_629_483_821e19;

/// The arctangent of `x` radians, in `-PI/2..=PI/2`, and `0.0` for `NaN`.
///
/// fdlibm's reduction: each of four intervals is mapped onto `|t| < 7/16` and
/// offset by the arctangent of its breakpoint.
#[must_use]
pub fn atan(x: f64) -> f64 {
    if x.is_nan() {
        return 0.0;
    }
    let magnitude = fabs(x);
    let angle = if magnitude >= ATAN_SATURATES {
        FRAC_PI_2
    } else {
        let (t, offset) = if magnitude < 0.4375 {
            (magnitude, None)
        } else if magnitude < 0.6875 {
            ((2.0 * magnitude - 1.0) / (2.0 + magnitude), Some(ATAN_HALF))
        } else if magnitude < 1.1875 {
            ((magnitude - 1.0) / (magnitude + 1.0), Some(ATAN_ONE))
        } else if magnitude < 2.4375 {
            (
                (magnitude - 1.5) / (1.0 + 1.5 * magnitude),
                Some(ATAN_THREE_HALVES),
            )
        } else {
            (-1.0 / magnitude, Some(ATAN_INFINITY))
        };
        let z = t * t;
        let w = z * z;
        let odd = z * (AT0 + w * (AT2 + w * (AT4 + w * (AT6 + w * (AT8 + w * AT10)))));
        let even = w * (AT1 + w * (AT3 + w * (AT5 + w * (AT7 + w * AT9))));
        match offset {
            None => t - t * (odd + even),
            Some((hi, lo)) => hi - ((t * (odd + even) - lo) - t),
        }
    };
    if x.is_sign_negative() {
        -angle
    } else {
        angle
    }
}

/// The angle of the vector `(x, y)` in `-PI..=PI`, measured from the positive
/// x axis.
///
/// The origin has no angle; it answers `0.0` rather than a `NaN`, so a
/// degenerate segment still yields a drawable direction.
#[must_use]
pub fn atan2(y: f64, x: f64) -> f64 {
    if x > 0.0 {
        return atan(y / x);
    }
    if x < 0.0 {
        return if y >= 0.0 {
            atan(y / x) + PI
        } else {
            atan(y / x) - PI
        };
    }
    if y > 0.0 {
        FRAC_PI_2
    } else if y < 0.0 {
        -FRAC_PI_2
    } else {
        0.0
    }
}

/// The arccosine of `x` in `0..=PI`, with the domain clamped to `-1..=1` so an
/// out-of-range input (a rounding overshoot in an arc conversion) answers an
/// endpoint rather than a `NaN`.
#[must_use]
pub fn acos(x: f64) -> f64 {
    let c = clamp(x, -1.0, 1.0);
    atan2(sqrt(1.0 - c * c), c)
}

/// The arcsine of `x` in `-PI/2..=PI/2`, with the domain clamped to `-1..=1`
/// so an out-of-range input answers an endpoint rather than a `NaN`.
#[must_use]
pub fn asin(x: f64) -> f64 {
    let c = clamp(x, -1.0, 1.0);
    atan2(c, sqrt(1.0 - c * c))
}

/// Largest argument [`exp`] evaluates, the greatest double below
/// `ln(f64::MAX)`; above it the true result exceeds a double and the answer
/// saturates.
const EXP_MAX_ARG: f64 = 709.782_712_893_384;

/// Smallest argument [`exp`] evaluates, the least double above
/// `ln(f64::MIN_POSITIVE)`; below it the true result is under the smallest
/// normal double and the answer is zero.
const EXP_MIN_ARG: f64 = -708.396_418_532_264_1;

/// `ln(2)`'s leading bits, chosen with a zero tail so `k * LN_2_HI` is exact.
const LN_2_HI: f64 = 6.931_471_803_691_238e-1;

/// The remainder of `ln(2)` past [`LN_2_HI`], subtracted separately so the
/// range reduction keeps its low bits.
const LN_2_LO: f64 = 1.908_214_929_270_587_7e-10;

// fdlibm's `exp`: `r (e^r + 1) / (e^r - 1) ~ 2 + P1 r^2 + … + P5 r^10` over
// `|r| <= ln(2)/2`.
const P1: f64 = 0.166_666_666_666_666_02;
const P2: f64 = -0.002_777_777_777_701_559_3;
const P3: f64 = 6.613_756_321_437_934e-5;
const P4: f64 = -1.653_390_220_546_525_2e-6;
const P5: f64 = 4.138_136_797_057_238_5e-8;

/// `e` raised to `x`.
///
/// Range-reduced to `x = k*ln(2) + r` with `|r| <= ln(2)/2`, where fdlibm's
/// rational approximation holds, then scaled by `2^k` through the exponent
/// field.
///
/// Total, like the rest of this module, and saturating rather than infinite:
/// an argument past the double's range answers [`f64::MAX`] or zero, and a
/// `NaN` answers zero. A consumer converting a decibel gain therefore gets
/// silence from a corrupt input rather than a `NaN` that would spread through
/// everything it is multiplied into.
#[must_use]
pub fn exp(x: f64) -> f64 {
    if x.is_nan() || x < EXP_MIN_ARG {
        return 0.0;
    }
    if x > EXP_MAX_ARG {
        return f64::MAX;
    }
    let k = round(x * LOG2_E);
    let hi = x - k * LN_2_HI;
    let lo = k * LN_2_LO;
    let r = hi - lo;
    let rr = r * r;
    let c = r - rr * (P1 + rr * (P2 + rr * (P3 + rr * (P4 + rr * P5))));
    let scaled = 1.0 + ((r * c / (2.0 - c) - lo) + hi);
    // `k` runs from -1022 to 1024 over the domain, and 2^1024 is past the
    // largest power a double holds, so the scale is applied in two halves.
    let k = round_i32(k);
    scaled * power_of_two(k / 2) * power_of_two(k - k / 2)
}

/// `2^n`, exact for `n` in a double's normal exponents, `-1022..=1023`.
#[allow(
    clippy::cast_sign_loss,
    reason = "the clamped exponent plus its bias is at least 1"
)]
fn power_of_two(n: i32) -> f64 {
    f64::from_bits(((n.clamp(-1022, 1023) + 1023) as u64) << 52)
}

#[cfg(test)]
#[path = "mathf_tests.rs"]
mod tests;
