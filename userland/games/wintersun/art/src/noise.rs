//! The one lattice under every grain, warp and frayed edge.
//!
//! Three things in this crate need a smooth pseudo-random field: a
//! material's grain, the warp that stops a tile repeating, and the
//! perturbation that frays a road's edge into the grass. They are one
//! field with three tags, not three fields.
//!
//! # Why it is integer, and what that buys
//!
//! The world generator works in `f64` over `lib/util::mathf` because its
//! stages genuinely need transcendentals. Nothing here does: a value
//! noise is a hash, an interpolation and a sum. Keeping it in integers
//! makes the output bit-identical on every Tier-1 target *by the
//! language's own arithmetic rules* rather than by a test, and makes the
//! per-texel cost a handful of multiplies in the pass with the largest
//! share of the frame budget.
//!
//! # Wrapping, and why it is not optional
//!
//! A material tile is drawn end to end across a hillside, so its noise
//! must be periodic at the tile's side or the seam is visible on every
//! repeat. Every lattice read therefore masks its coordinates to a period
//! the caller states, and [`Tiled`] is the handle that carries it — a
//! sampler that could not wrap would produce tiles that cannot tile.

use tairix_hash::FastHash;

/// What a lattice value belongs to.
///
/// Domain separation, so two fields sampling the same lattice point never
/// see the same number and adding a field cannot shift an existing one.
/// The discriminants are frozen: a new field takes a new value.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[repr(u64)]
pub enum Field {
    /// A material's colour grain.
    Grain = 1,
    /// A material's height relief.
    Relief = 2,
    /// The eastward component of the anti-repetition warp.
    WarpX = 3,
    /// The southward component of the same warp.
    WarpY = 4,
    /// The perturbation that frays a decal's edge.
    Fray = 5,
    /// Per-particle spawn variation.
    Spawn = 6,
}

/// A keyed noise field, wrapping at a stated period.
///
/// The period is a power of two in lattice cells. A sampler built for one
/// period cannot be asked for another, which is what keeps a tile's noise
/// and the tile's own side from drifting apart.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Tiled {
    key: u64,
    mask: i32,
}

/// The coarsest lattice cell a sample will use, as a log2 of the
/// coordinate unit.
///
/// A cell this size already spans the whole coordinate space, so a caller
/// asking for more gets the same field rather than a shift the machine
/// cannot perform.
pub const MAX_CELL_LOG2: u32 = 30;

/// The widest period a sampler will wrap at.
///
/// A validation bound on the shift a caller may ask for, not a capacity:
/// beyond this the mask would not fit the signed lattice coordinate the
/// hash is fed.
pub const MAX_PERIOD_LOG2: u32 = 24;

impl Tiled {
    /// A sampler keyed on `key`, wrapping every `1 << period_log2` lattice
    /// cells.
    ///
    /// A period above [`MAX_PERIOD_LOG2`] is refused rather than clamped:
    /// a silently narrowed period would tile at a size the caller did not
    /// ask for, which is the seam this type exists to prevent.
    #[must_use]
    pub const fn new(key: u64, period_log2: u32) -> Option<Self> {
        if period_log2 == 0 || period_log2 > MAX_PERIOD_LOG2 {
            return None;
        }
        Some(Self {
            key,
            mask: (1i32 << period_log2) - 1,
        })
    }

    /// A sampler that does not wrap, for a field sampled over open world
    /// coordinates rather than into a tile.
    #[must_use]
    pub const fn unbounded(key: u64) -> Self {
        Self { key, mask: -1 }
    }

    /// The raw word this field assigns to the lattice point `(x, y)`.
    #[must_use]
    pub fn lattice(&self, field: Field, x: i32, y: i32) -> u64 {
        let (x, y) = (x & self.mask, y & self.mask);
        let mut message = [0u8; 16];
        message[0..8].copy_from_slice(&(field as u64).to_le_bytes());
        message[8..12].copy_from_slice(&x.to_le_bytes());
        message[12..16].copy_from_slice(&y.to_le_bytes());
        FastHash::hash_bytes(self.key, &message)
    }

    /// The lattice value at `(x, y)`, as a `0..=65535` fraction.
    #[must_use]
    pub fn corner(&self, field: Field, x: i32, y: i32) -> u16 {
        // The top bits, which are the best mixed.
        #[allow(
            clippy::cast_possible_truncation,
            reason = "the shift leaves exactly sixteen bits"
        )]
        {
            (self.lattice(field, x, y) >> 48) as u16
        }
    }

    /// The smoothly interpolated field at `(x, y)`, where the coordinates
    /// are in units of `1 << cell_log2` per lattice cell.
    ///
    /// Bilinear between the four surrounding corners, with each axis run
    /// through a smoothstep first — so the field is continuous in value
    /// and in slope, and a grain has no lattice grid in it.
    #[must_use]
    pub fn value(&self, field: Field, x: i32, y: i32, cell_log2: u32) -> u16 {
        let cell_log2 = cell_log2.clamp(1, MAX_CELL_LOG2);
        let (x0, y0) = (x >> cell_log2, y >> cell_log2);
        let fx = smoothstep(fraction(x, cell_log2));
        let fy = smoothstep(fraction(y, cell_log2));

        let top = mix(
            self.corner(field, x0, y0),
            self.corner(field, x0 + 1, y0),
            fx,
        );
        let bottom = mix(
            self.corner(field, x0, y0 + 1),
            self.corner(field, x0 + 1, y0 + 1),
            fx,
        );
        mix(top, bottom, fy)
    }

    /// Fractional Brownian motion: `octaves` doublings of frequency, each
    /// contributing half the amplitude of the one before it.
    ///
    /// Returns `0..=65535`. Zero octaves is a flat half — a caller that
    /// has shed all its detail gets the material's mid tone, never a
    /// division by nothing.
    #[must_use]
    pub fn fbm(&self, field: Field, x: i32, y: i32, cell_log2: u32, octaves: u32) -> u16 {
        if octaves == 0 {
            return HALF;
        }
        // Widened: the first octave alone reaches `u16::MAX * SCALE`,
        // which is most of a `u32`, and the second would carry it over.
        let mut sum: u64 = 0;
        let mut weight: u64 = 0;
        for octave in 0..octaves {
            // Stop before the lattice cell collapses to a single unit, at
            // which point further octaves are white noise rather than
            // detail.
            let Some(cell) = cell_log2.checked_sub(octave) else {
                break;
            };
            if cell == 0 {
                break;
            }
            let amplitude = SCALE >> octave;
            if amplitude == 0 {
                break;
            }
            sum += u64::from(self.value(field, x, y, cell)) * amplitude;
            weight += amplitude;
        }
        if weight == 0 {
            return HALF;
        }
        #[allow(
            clippy::cast_possible_truncation,
            reason = "the quotient is a weighted mean of u16 values"
        )]
        {
            (sum / weight) as u16
        }
    }
}

/// Mid-scale of a `u16` field.
const HALF: u16 = 0x8000;

/// The first octave's weight, halved per octave after it.
const SCALE: u64 = 1 << 16;

/// Where `x` sits inside its lattice cell, as a `0..=65535` fraction.
///
/// Uses the arithmetic-shift remainder rather than `%`, so a negative
/// coordinate lands in `0..span` like a positive one and the field is
/// continuous across the origin. Scaled in 64 bits, because a coarse cell
/// times the full `u16` range leaves 32 behind.
fn fraction(x: i32, cell_log2: u32) -> u16 {
    let span = 1i64 << cell_log2;
    let within = i64::from(x) - ((i64::from(x) >> cell_log2) << cell_log2);
    debug_assert!(within >= 0 && within < span);
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the remainder is in `0..span`, so the quotient is in the u16 range"
    )]
    {
        ((within * i64::from(u16::MAX)) / span) as u16
    }
}

/// `3t² - 2t³` over a `0..=65535` fraction.
///
/// The classic smoothstep, in fixed point: zero slope at both ends, so two
/// adjacent lattice cells meet without a crease.
fn smoothstep(t: u16) -> u16 {
    let t = u64::from(t);
    let total = u64::from(u16::MAX);
    let square = t * t / total;
    let cube = square * t / total;
    #[allow(
        clippy::cast_possible_truncation,
        reason = "3t^2 - 2t^3 over 0..=1 stays in 0..=1"
    )]
    {
        (3 * square - 2 * cube).min(total) as u16
    }
}

/// `a` toward `b` by `t`/65535, rounded to nearest.
fn mix(a: u16, b: u16, t: u16) -> u16 {
    let total = u32::from(u16::MAX);
    let (a, b, t) = (u32::from(a), u32::from(b), u32::from(t));
    #[allow(
        clippy::cast_possible_truncation,
        reason = "a weighted mean of two u16 values is a u16"
    )]
    {
        ((a * (total - t) + b * t + total / 2) / total) as u16
    }
}

/// `value` rescaled from `0..=65535` into `0..=255`, rounded to nearest.
#[must_use]
pub fn to_byte(value: u16) -> u8 {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "a u16 scaled down by 257 is a u8"
    )]
    {
        ((u32::from(value) + 128) / 257) as u8
    }
}

/// `value` as a signed offset in `-half..=half`.
///
/// The field is unsigned because a hash is; a warp and a fray both want it
/// centred on zero, and doing that once here keeps the two from each
/// choosing their own rounding.
#[must_use]
pub fn centred(value: u16, half: i32) -> i32 {
    let span = i64::from(half) * 2;
    let scaled = i64::from(value) * span / i64::from(u16::MAX);
    #[allow(
        clippy::cast_possible_truncation,
        reason = "the product is bounded by `span`, itself an i32 doubled into an i64"
    )]
    {
        (scaled - i64::from(half)) as i32
    }
}

#[cfg(test)]
mod tests;
