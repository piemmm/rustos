//! Bounded `f64` maths for `no_std` geometry.
//!
//! `floor`, `sqrt`, `sin`, `atan2` and friends live in `std`, where they call
//! the platform libm, so a `no_std` crate cannot reach them. Rolling these
//! ourselves keeps an external libm out of the trusted computing base, and
//! keeping them here — rather than one private copy per crate — means the
//! glyph rasteriser (`lib/fontface`) and the SVG decoder (`lib/svg`) round and
//! rotate identically.
//!
//! Every function is total: it returns a finite answer for every finite input
//! and a defined one for the degenerate cases (a negative square root, a
//! vertical `atan2`, an out-of-domain `acos`), so no caller has to guard
//! against a `NaN` it cannot render. Callers pass pixel coordinates, design-
//! grid units, and angles in radians — small, finite values far inside `i64`.
//!
//! Accuracy is about 1e-9 relative for the transcendental functions, which is
//! several orders finer than the sub-pixel grid any consumer rasterises onto.

use core::f64::consts::{FRAC_PI_2, FRAC_PI_4, PI};

/// Two pi, the period of [`sin`] and [`cos`].
const TAU: f64 = 2.0 * PI;

/// Truncate `x` toward zero. Callers only pass finite values within `i64`.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "callers only pass finite pixel coordinates, design-grid units \
              and angles: small integers far within both i64 and f64's 52-bit \
              mantissa, so neither the truncation to i64 nor the widening back \
              to f64 loses any value"
)]
#[must_use]
pub fn trunc(x: f64) -> f64 {
    x as i64 as f64
}

/// The largest integer not greater than `x`.
#[must_use]
pub fn floor(x: f64) -> f64 {
    let t = trunc(x);
    if t > x {
        t - 1.0
    } else {
        t
    }
}

/// The smallest integer not less than `x`.
#[must_use]
pub fn ceil(x: f64) -> f64 {
    let t = trunc(x);
    if t < x {
        t + 1.0
    } else {
        t
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

/// Round `x` to the nearest integer, halves toward positive infinity.
#[must_use]
pub fn round(x: f64) -> f64 {
    floor(x + 0.5)
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

/// Round `x` to the nearest integer, halves toward positive infinity (i.e.
/// `floor(x + 0.5)`), returned as an `i32`.
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

/// The non-negative square root of `x`, and `0.0` for a negative or zero
/// input.
///
/// Newton-Raphson from a bit-halved initial guess: halving the biased
/// exponent lands within a factor of two of the root, from which four
/// iterations converge to the last bit or two for every finite input.
#[must_use]
pub fn sqrt(x: f64) -> f64 {
    if x.is_nan() || x <= 0.0 {
        return 0.0;
    }
    if x.is_infinite() {
        return x;
    }
    let mut guess = f64::from_bits((x.to_bits() >> 1) + (1023_u64 << 51));
    for _ in 0..5 {
        guess = f64::midpoint(guess, x / guess);
    }
    guess
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

/// `x` reduced into `-PI..=PI`, the interval the polynomial kernels below are
/// accurate over.
fn wrap_angle(x: f64) -> f64 {
    let wrapped = x - TAU * round(x / TAU);
    clamp(wrapped, -PI, PI)
}

/// `sin(x)` for `x` already in `-PI/4..=PI/4`.
///
/// The odd minimax polynomial of the Taylor form, truncated where the next
/// term is below the double's last bit over this interval.
fn sin_kernel(x: f64) -> f64 {
    let x2 = x * x;
    x * (1.0
        + x2 * (-1.0 / 6.0
            + x2 * (1.0 / 120.0
                + x2 * (-1.0 / 5040.0 + x2 * (1.0 / 362_880.0 - x2 / 39_916_800.0)))))
}

/// `cos(x)` for `x` already in `-PI/4..=PI/4`.
fn cos_kernel(x: f64) -> f64 {
    let x2 = x * x;
    1.0 + x2
        * (-0.5 + x2 * (1.0 / 24.0 + x2 * (-1.0 / 720.0 + x2 * (1.0 / 40320.0 - x2 / 3_628_800.0))))
}

/// The sine of `x` radians.
#[must_use]
pub fn sin(x: f64) -> f64 {
    let a = wrap_angle(x);
    if a < 0.0 {
        -sin_half_turn(-a)
    } else {
        sin_half_turn(a)
    }
}

/// `sin(a)` for `a` in `0..=PI`.
///
/// Folded twice onto the quarter turn the kernels cover: sine is symmetric
/// about `PI/2`, and above `PI/4` the cosine kernel of the complement is the
/// accurate half of the pair.
fn sin_half_turn(a: f64) -> f64 {
    let folded = if a > FRAC_PI_2 { PI - a } else { a };
    if folded > FRAC_PI_4 {
        cos_kernel(FRAC_PI_2 - folded)
    } else {
        sin_kernel(folded)
    }
}

/// The cosine of `x` radians.
#[must_use]
pub fn cos(x: f64) -> f64 {
    sin(x + FRAC_PI_2)
}

/// The tangent of `x` radians.
///
/// A pole (where the cosine vanishes) yields a large finite value rather than
/// an infinity, so a skew transform built from it still produces a drawable —
/// if extreme — shape instead of a `NaN` that would erase it.
#[must_use]
pub fn tan(x: f64) -> f64 {
    let c = cos(x);
    if fabs(c) < 1e-12 {
        return if sin(x) < 0.0 { -1e12 } else { 1e12 };
    }
    sin(x) / c
}

/// `sqrt(3)`, the constant of the arctangent's sixth-turn reduction.
const SQRT_3: f64 = 1.732_050_807_568_877_2;

/// `tan(PI/12)` = `2 - sqrt(3)`, the largest argument the series below is
/// asked to converge over.
const TAN_PI_12: f64 = 0.267_949_192_431_122_7;

/// `atan(t)` by its alternating power series, for `|t| <= tan(PI/12)`.
///
/// The argument is reduced onto that interval first, where `t^2 <= 0.072` and
/// the series is well inside the double's last bit after this many terms.
fn atan_series(t: f64) -> f64 {
    let t2 = t * t;
    let mut term = t;
    let mut sum = t;
    for k in 1..14_u32 {
        term *= -t2;
        sum += term / f64::from(2 * k + 1);
    }
    sum
}

/// The arctangent of `x` radians, in `-PI/2..=PI/2`.
#[must_use]
pub fn atan(x: f64) -> f64 {
    if x < 0.0 {
        return -atan(-x);
    }
    if x > 1.0 {
        return FRAC_PI_2 - atan(1.0 / x);
    }
    if x > TAN_PI_12 {
        // atan(x) = PI/6 + atan((sqrt(3)x - 1) / (sqrt(3) + x)), which brings
        // the whole of 0..=1 inside the series' interval.
        return PI / 6.0 + atan_series((SQRT_3 * x - 1.0) / (SQRT_3 + x));
    }
    atan_series(x)
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

#[cfg(test)]
#[path = "mathf_tests.rs"]
mod tests;
