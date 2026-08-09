//! Bounded `f64` rounding helpers.
//!
//! The rasteriser needs `floor`, `ceil`, `min`, `max`, and `clamp` on `f64`,
//! all of which live in `std` (they call the platform libm) and so are
//! unavailable in this `no_std` crate. Every value they see here is a pixel
//! coordinate or a coverage fraction — small and finite, well within `i64` —
//! so `floor`/`ceil` are implemented by truncation toward zero and a one-step
//! correction, and `min`/`max` by a plain comparison. Rolling these tiny,
//! obviously-correct helpers ourselves is preferable to pulling in an external
//! libm crate (`AGENTS.md` §2.12).

/// Truncate `x` toward zero. Callers only pass finite values within `i64`.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "callers only pass finite pixel coordinates and coverage \
              fractions: small integers far within both i64 and f64's 52-bit \
              mantissa, so neither the truncation to i64 nor the widening back \
              to f64 loses any value"
)]
fn trunc(x: f64) -> f64 {
    x as i64 as f64
}

/// The largest integer not greater than `x`.
pub(crate) fn floor(x: f64) -> f64 {
    let t = trunc(x);
    if t > x {
        t - 1.0
    } else {
        t
    }
}

/// The smallest integer not less than `x`.
pub(crate) fn ceil(x: f64) -> f64 {
    let t = trunc(x);
    if t < x {
        t + 1.0
    } else {
        t
    }
}

/// The magnitude of `x`.
pub(crate) fn fabs(x: f64) -> f64 {
    if x < 0.0 {
        -x
    } else {
        x
    }
}

/// Round `x` to the nearest integer, halves toward positive infinity.
pub(crate) fn round(x: f64) -> f64 {
    floor(x + 0.5)
}

/// The greater of `a` and `b`. Inputs are always non-`NaN`.
pub(crate) fn fmax(a: f64, b: f64) -> f64 {
    if a > b {
        a
    } else {
        b
    }
}

/// The lesser of `a` and `b`. Inputs are always non-`NaN`.
pub(crate) fn fmin(a: f64, b: f64) -> f64 {
    if a < b {
        a
    } else {
        b
    }
}

/// `x` clamped to `lo..=hi` (with `lo <= hi`).
pub(crate) fn clamp(x: f64, lo: f64, hi: f64) -> f64 {
    fmin(fmax(x, lo), hi)
}

/// Round `x` to the nearest integer, halves toward positive infinity (i.e.
/// `floor(x + 0.5)`), returned as an `i32`.
///
/// This is the round-half-up rule a reference instancer applies when turning a
/// fractional variation delta into an integer font-unit correction, so
/// computed advances agree with it.
///
/// Callers only pass variation advance/position deltas in font units — a few
/// thousand at most, far inside `i32` — so the round result cannot overflow.
#[allow(
    clippy::cast_possible_truncation,
    reason = "the value is a font-unit advance/position delta bounded to a few \
              thousand, well within i32, so the truncation the lint warns about \
              cannot occur"
)]
pub(crate) fn round_i32(x: f64) -> i32 {
    round(x) as i32
}
