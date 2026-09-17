//! The soft-edged fur on Cinder's coat, and the shadow he stands on.
//!
//! A blob is a radial splat whose coverage falls off towards its rim, with a
//! small deterministic angular ripple, so a cheek ruff or a tail plume reads
//! as layered guard hairs rather than as a clean disc. Coverage comes from one
//! shared table indexed by squared radius, so the pixel loop is integer work
//! and no square root or transcendental is evaluated per pixel.
//!
//! This is the *soft* half of the creature. His silhouette — the skull, the
//! limbs, the ears, the garment — is outlines rather than splats, and lives in
//! [`crate::shape`]; a blob cannot state a cat.

use tairix_raster::{Color, Surface};
use tairix_util::mathf;

/// The shared falloff table's step count as a float, converted losslessly
/// from the one definition so the two cannot drift apart.
///
/// A constant expression, so the conversion is folded and costs the pixel
/// loop nothing.
#[allow(clippy::cast_precision_loss)] // A compile-time constant far below 2^53.
const fn steps_f64() -> f64 {
    FALLOFF_STEPS as f64
}

/// [`RIPPLE_SECTORS`] as a float, on the same terms.
#[allow(clippy::cast_precision_loss)] // A compile-time constant far below 2^53.
const fn sectors_f64() -> f64 {
    RIPPLE_SECTORS as f64
}

/// How many steps the shared falloff table holds.
///
/// Indexed by *squared* distance from the centre as a fraction of the squared
/// radius, so the samples cluster where the eye looks — the outer rim, where
/// the edge softness lives — without the table needing to be large.
pub const FALLOFF_STEPS: usize = 64;

/// Coverage from the blob's centre to its rim, as a fraction of full.
///
/// A smoothstep rather than a linear ramp: a linear edge reads as a hard
/// polygon at small sizes, and a Gaussian never quite reaches zero so every
/// blob would tint its whole bounding box.
#[must_use]
pub fn falloff_table() -> [u8; FALLOFF_STEPS] {
    let mut table = [0u8; FALLOFF_STEPS];
    let mut index = 0;
    while index < FALLOFF_STEPS {
        // `t` is the squared-radius fraction, so the geometric distance is its
        // root; taking the root here is what keeps it out of the pixel loop.
        let t = (f64::from(u32::try_from(index).unwrap_or(0)) + 0.5) / steps_f64();
        let r = mathf::sqrt(t);
        let edge = 1.0 - r;
        // Smoothstep over the outer third, solid within it.
        let coverage = if edge >= SOLID_CORE {
            1.0
        } else {
            let u = mathf::clamp(edge / SOLID_CORE, 0.0, 1.0);
            u * u * (3.0 - 2.0 * u)
        };
        // The smoothstep is bounded to `0.0..=1.0`, so this scales into a byte
        // exactly.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            table[index] = (coverage * 255.0) as u8;
        }
        index += 1;
    }
    table
}

/// The fraction of the radius that is fully covered before the edge softens.
const SOLID_CORE: f64 = 0.34;

/// How many sectors the per-blob rim ripple is sampled in.
///
/// A ripple is what makes a disc read as a clump of guard hairs. Few enough
/// sectors that the wobble is legible at thirty pixels across, and a power of
/// two so the sector of an angle is a mask rather than a division.
pub const RIPPLE_SECTORS: usize = 16;

/// How far the rim ripples, as a fraction of the radius.
const RIPPLE_DEPTH: f64 = 0.16;

/// One soft disc of fur.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Blob {
    /// Screen column of the centre.
    pub x: f64,
    /// Screen row of the centre.
    pub y: f64,
    /// Radius in pixels before the ripple.
    pub radius: f64,
    /// Straight-alpha colour; its alpha scales the whole splat.
    pub color: Color,
    /// Which ripple pattern this blob wears.
    ///
    /// Deterministic per blob rather than random per frame: fur that
    /// re-rippled every frame would boil, which reads as noise rather than as
    /// a coat.
    pub seed: u16,
}

/// The rim radii of `seed`'s ripple, one per sector, as fractions of the
/// radius.
///
/// Derived from the seed by a cheap integer mix rather than drawn from the
/// generator: the pattern must be the same every frame for a given blob, and
/// tying it to the blob's identity is what guarantees that without storing it.
#[must_use]
pub fn ripple(seed: u16) -> [f64; RIPPLE_SECTORS] {
    let mut out = [1.0; RIPPLE_SECTORS];
    let mut state = u32::from(seed).wrapping_mul(0x9E37_79B9) | 1;
    for slot in &mut out {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        // The top bits are the best mixed; fold them into `-1.0..=1.0`.
        let unit = f64::from(state >> 16) / f64::from(u16::MAX) * 2.0 - 1.0;
        *slot = 1.0 + unit * RIPPLE_DEPTH;
    }
    out
}

/// Composite `blob` onto `surface` through the shared `table`.
///
/// Premultiplied source-over, one pass, integer inside the loop: the squared
/// distance indexes the falloff table directly, so nothing transcendental is
/// evaluated per pixel.
pub fn splat(surface: &mut Surface, blob: &Blob, table: &[u8; FALLOFF_STEPS]) {
    if blob.radius <= 0.0 || blob.color.a == 0 {
        return;
    }
    let rim = ripple(blob.seed);
    // The ripple can push the rim out, so the bounding box has to allow for it
    // or the outermost fur would be clipped square.
    let reach = blob.radius * (1.0 + RIPPLE_DEPTH);
    let (Some(left), Some(top), Some(right), Some(bottom)) = (
        floor_to_i32(blob.x - reach),
        floor_to_i32(blob.y - reach),
        ceil_to_i32(blob.x + reach),
        ceil_to_i32(blob.y + reach),
    ) else {
        return;
    };
    let width = i64::from(surface.width());
    let height = i64::from(surface.height());
    for py in top..=bottom {
        if i64::from(py) < 0 || i64::from(py) >= height {
            continue;
        }
        let dy = f64::from(py) + 0.5 - blob.y;
        for px in left..=right {
            if i64::from(px) < 0 || i64::from(px) >= width {
                continue;
            }
            let dx = f64::from(px) + 0.5 - blob.x;
            let Some(coverage) = coverage_at(dx, dy, blob.radius, &rim, table) else {
                continue;
            };
            if coverage == 0 {
                continue;
            }
            let alpha = mul255(coverage, blob.color.a);
            if alpha == 0 {
                continue;
            }
            let source = Color {
                a: alpha,
                ..blob.color
            }
            .premultiply();
            // Bounds were checked above, so both reads are in range.
            let (Ok(ux), Ok(uy)) = (u32::try_from(px), u32::try_from(py)) else {
                continue;
            };
            let Some(dst) = surface.get(ux, uy) else {
                continue;
            };
            surface.set(ux, uy, source.over(dst));
        }
    }
}

/// The blob's coverage at body-relative `(dx, dy)`, or `None` outside its rim.
fn coverage_at(
    dx: f64,
    dy: f64,
    radius: f64,
    rim: &[f64; RIPPLE_SECTORS],
    table: &[u8; FALLOFF_STEPS],
) -> Option<u8> {
    let sector_radius = radius * rim[sector_of(dx, dy)];
    if sector_radius <= 0.0 {
        return None;
    }
    let distance_squared = dx * dx + dy * dy;
    let limit = sector_radius * sector_radius;
    if distance_squared >= limit {
        return None;
    }
    let fraction = distance_squared / limit;
    // `fraction` is in `0.0..1.0`, so the scaled index is in range; the min
    // guards the boundary the float could land exactly on.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let index = ((fraction * steps_f64()) as usize).min(FALLOFF_STEPS - 1);
    Some(table[index])
}

/// Which ripple sector the direction `(dx, dy)` falls in.
fn sector_of(dx: f64, dy: f64) -> usize {
    let angle = mathf::atan2(dy, dx) + core::f64::consts::PI;
    let scaled = angle / core::f64::consts::TAU * sectors_f64();
    // `angle` is in `0.0..=TAU`, so the product is in range; the min guards
    // the exact-TAU boundary.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        (scaled as usize).min(RIPPLE_SECTORS - 1)
    }
}

/// `a * b / 255`, rounded, without a division.
fn mul255(a: u8, b: u8) -> u8 {
    let product = u32::from(a) * u32::from(b) + 128;
    // The sum of a byte product and 128 fits comfortably; the shift leaves a
    // byte.
    #[allow(clippy::cast_possible_truncation)]
    {
        ((product + (product >> 8)) >> 8) as u8
    }
}

/// `x` floored to an `i32`, or `None` when it is not representable.
fn floor_to_i32(x: f64) -> Option<i32> {
    let floored = mathf::floor(x);
    if floored < f64::from(i32::MIN) || floored > f64::from(i32::MAX) {
        return None;
    }
    // The range check above is exactly the cast's precondition.
    #[allow(clippy::cast_possible_truncation)]
    Some(floored as i32)
}

/// `x` ceilinged to an `i32`, or `None` when it is not representable.
fn ceil_to_i32(x: f64) -> Option<i32> {
    let ceiled = mathf::ceil(x);
    if ceiled < f64::from(i32::MIN) || ceiled > f64::from(i32::MAX) {
        return None;
    }
    // The range check above is exactly the cast's precondition.
    #[allow(clippy::cast_possible_truncation)]
    Some(ceiled as i32)
}

/// Composite a soft elliptical shadow centred at `(x, y)`.
///
/// Its own path rather than a wide blob, because a shadow is squashed by the
/// camera and a blob is round: reusing the blob would need a per-blob aspect
/// nothing else wants.
pub fn shadow(surface: &mut Surface, x: f64, y: f64, rx: f64, ry: f64, alpha: u8) {
    if rx <= 0.0 || ry <= 0.0 || alpha == 0 {
        return;
    }
    let (Some(left), Some(top), Some(right), Some(bottom)) = (
        floor_to_i32(x - rx),
        floor_to_i32(y - ry),
        ceil_to_i32(x + rx),
        ceil_to_i32(y + ry),
    ) else {
        return;
    };
    let width = i64::from(surface.width());
    let height = i64::from(surface.height());
    for py in top..=bottom {
        if i64::from(py) < 0 || i64::from(py) >= height {
            continue;
        }
        let dy = (f64::from(py) + 0.5 - y) / ry;
        for px in left..=right {
            if i64::from(px) < 0 || i64::from(px) >= width {
                continue;
            }
            let dx = (f64::from(px) + 0.5 - x) / rx;
            let distance_squared = dx * dx + dy * dy;
            if distance_squared >= 1.0 {
                continue;
            }
            // Soft to the rim so the shadow has no visible edge.
            let fade = 1.0 - distance_squared;
            let scaled = f64::from(alpha) * fade * fade;
            // `fade` is in `0.0..=1.0`, so this stays inside the byte.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let a = scaled as u8;
            if a == 0 {
                continue;
            }
            let (Ok(ux), Ok(uy)) = (u32::try_from(px), u32::try_from(py)) else {
                continue;
            };
            let Some(dst) = surface.get(ux, uy) else {
                continue;
            };
            let source = Color {
                r: 0,
                g: 0,
                b: 0,
                a,
            }
            .premultiply();
            surface.set(ux, uy, source.over(dst));
        }
    }
}

#[cfg(test)]
#[path = "fur_tests.rs"]
mod tests;
