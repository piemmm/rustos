//! What a fill is painted with: a flat colour, a gradient, or a tile.
//!
//! A [`Paint`] answers one question — what colour goes at a point — so the
//! scan converter fills a shape without knowing which kind it holds, and a
//! new kind never touches the fill's own walk.
//!
//! A [`Gradient`] is defined in its own *canonical* space and carries the
//! [`Affine`] that maps a shape's coordinates into it
//! ([`Gradient::to_gradient`]). Canonical space is deliberately trivial: a
//! linear gradient runs along the x axis from `0` to `1`, so its parameter is
//! simply the mapped `x`, and a radial gradient is the unit circle at the
//! origin, so its parameter is the distance from the origin. Every ellipse,
//! rotation, and `gradientUnits` convention a document can express is then one
//! matrix rather than a special case in the sampler.
//!
//! A [`Pattern`] is the third kind, and the one whose colour at a point is
//! *pixels*: a tile of artwork repeated across the shape. It carries the tile
//! as artwork rather than as a rendered image, because the resolution the
//! tile must be rendered at is the one the drawing is being rasterised at,
//! which the producer of a resolution-independent drawing cannot know. The
//! renderer sizes it from the fill ([`Pattern::tile_extent`]), draws one
//! period of it — once per replica its [`TileFold`] folds in — and reads it
//! back at the repeated position this module resolves
//! ([`Pattern::tile_position`]).
//!
//! Sampling is total: no input produces a `NaN`, a division by zero, or a
//! panic. A gradient with no stops paints nothing, one with a single stop
//! paints that stop everywhere, and a parameter outside `0..=1` is brought
//! back inside it by the [`SpreadMethod`].

use alloc::vec::Vec;

use tairix_util::mathf;

use crate::affine::Affine;
use crate::artwork::Node;
use crate::color::Color;

/// How far from the centre of the unit circle a radial gradient's focal point
/// may sit.
///
/// SVG requires a focal point outside the circle to be moved just inside its
/// edge. Keeping it strictly inside is also what makes the focal-ray formula
/// total: on the edge the ray through a point on that same edge has zero
/// length, and the parameter would divide by zero.
const FOCAL_LIMIT: f64 = 0.99;

/// The largest a pattern tile is rendered, in pixels along each axis.
///
/// A fixed containment bound, not a capacity: a tile is rendered at the
/// resolution the drawing is being rasterised at, so a document is free to
/// ask for one the size of the whole picture. Clamping the *render* costs
/// only sharpness, because a tile's period is its geometry and not its
/// resolution, so an absurd tile blurs rather than allocating.
///
/// A megabyte at the limit, against the full-extent isolation buffer a group
/// already costs — and a repeat larger than this on screen is barely a
/// repeat, so the blur is confined to the case where the tiling itself has
/// stopped meaning much.
pub const MAX_TILE_EXTENT: u32 = 512;

/// What a fill is painted with.
#[derive(Clone, Debug, PartialEq)]
pub enum Paint {
    /// One colour everywhere.
    Solid(Color),
    /// A colour that varies with position.
    Gradient(Gradient),
    /// A tile of artwork repeated across the shape.
    Pattern(Pattern),
}

/// How many whole periods a tile's content may reach past its own tile, on
/// any one side.
///
/// A fixed containment bound, not a capacity. Drawing the spill exactly means
/// drawing the content once per replica that can reach the period being
/// rendered, so this multiplies the one-off tile render by up to `(2k+1)²` —
/// nine draws here against one for a confined tile.
///
/// One period is where an honest pattern ends: content reaching further
/// overlaps its neighbour's neighbour, so the repeat has stopped being a
/// repeat and the picture is artwork, more cheaply authored as artwork. A
/// tile whose content is up to three periods across still folds.
pub const MAX_TILE_FOLD: u32 = 1;

/// The replicas of a tile drawn into one period, so content spilling past its
/// own tile still appears in its neighbours.
///
/// A pattern is periodic, so the infinitely many replicas restricted to one
/// period sum to the finitely many whose content reaches into it, folded back
/// by whole periods. The result is one periodic tile again, so the wrap
/// sampler and the per-pixel cost stay a confined tile's and only the tile's
/// own render pays.
///
/// The overhangs cross over: content reaching past the tile's *right* edge is
/// what a replica to its *left* spills back in, so a right overhang is
/// counted in [`before`](Self::before).
///
/// Replicas draw in raster order — rows top to bottom, each row left to right
/// — which is what blitting whole tiles across the plane produces, and is
/// observable wherever two of them overlap.
///
/// The default draws the tile's own replica alone, which is the
/// `overflow: hidden` a pattern is otherwise drawn under.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct TileFold {
    /// Replicas drawn before the tile's own, on the x and y axes.
    pub before: (u32, u32),
    /// Replicas drawn after it.
    pub after: (u32, u32),
}

impl TileFold {
    /// The fold that draws content spanning `min..=max`, on a grid of which
    /// `design` units square is one tile.
    ///
    /// `None` when the content reaches more than [`MAX_TILE_FOLD`] whole
    /// periods past the tile on any side, which the caller refuses rather
    /// than folding.
    #[must_use]
    pub fn reaching(min: (i32, i32), max: (i32, i32), design: u32) -> Option<Self> {
        let period = i64::from(design.max(1));
        let before = |far: i32| periods(i64::from(far) - period, period);
        let after = |near: i32| periods(-i64::from(near), period);
        Some(Self {
            before: (before(max.0)?, before(max.1)?),
            after: (after(min.0)?, after(min.1)?),
        })
    }

    /// The replica grid this fold draws — `before + 1 + after` on each axis —
    /// or `None` past [`MAX_TILE_FOLD`].
    ///
    /// A renderer asks before allocating, so a fold assembled by hand rather
    /// than by [`reaching`](Self::reaching) fails closed instead of tiling
    /// the machine to a halt.
    #[must_use]
    pub fn grid(self) -> Option<(u32, u32)> {
        let axis = |before: u32, after: u32| {
            (before <= MAX_TILE_FOLD && after <= MAX_TILE_FOLD).then_some(before + after + 1)
        };
        Some((
            axis(self.before.0, self.after.0)?,
            axis(self.before.1, self.after.1)?,
        ))
    }
}

/// `overhang` design units as whole `period`-unit periods, rounded up, or
/// `None` past [`MAX_TILE_FOLD`].
fn periods(overhang: i64, period: i64) -> Option<u32> {
    if overhang <= 0 {
        return Some(0);
    }
    let whole = (overhang - 1) / period + 1;
    u32::try_from(whole)
        .ok()
        .filter(|count| *count <= MAX_TILE_FOLD)
}

/// A tile of artwork repeated across a fill.
///
/// The tile is held as *artwork* rather than as an image because the
/// resolution it wants is the one the drawing is being rasterised at, which a
/// resolution-independent producer does not know. The renderer sizes the tile
/// from the fill it is drawing ([`tile_extent`](Self::tile_extent)), draws the
/// content into a buffer of that size, and reads it back with the repeat
/// [`to_tile`](Self::to_tile) implies.
#[derive(Clone, Debug, PartialEq)]
pub struct Pattern {
    /// The tile's own artwork, on the drawing's design grid: the whole grid
    /// is one tile.
    pub content: Vec<Node>,
    /// Maps a point in the filled geometry's own coordinates into tile space,
    /// where one tile is the unit square.
    pub to_tile: Affine,
    /// The replicas folded into that one tile for content that spills past
    /// it.
    pub fold: TileFold,
    /// What the assembled tile is composited at.
    ///
    /// The fill's own opacity, weakening the tile once it is built rather
    /// than layer by layer or replica by replica: SVG weakens the fill
    /// operation as a whole, so two of the tile's layers would otherwise show
    /// through one another and two overlapping replicas would each pay it.
    pub opacity: u8,
}

impl Pattern {
    /// The pixel size to render one tile at, given how many of the filled
    /// geometry's units one device pixel spans on each axis.
    ///
    /// One tile edge in device pixels, so the tile is drawn at the density it
    /// is read back at and neither blurs nor aliases. `None` when the tiling
    /// collapses: a degenerate or non-finite map has no tile to draw, and the
    /// fill fails closed rather than inventing one.
    #[must_use]
    pub fn tile_extent(&self, contour_per_pixel: (f64, f64)) -> Option<(u32, u32)> {
        let from_tile = self.to_tile.invert()?;
        let per_x = mathf::fabs(contour_per_pixel.0);
        let per_y = mathf::fabs(contour_per_pixel.1);
        if per_x <= 0.0 || per_y <= 0.0 || !per_x.is_finite() || !per_y.is_finite() {
            return None;
        }
        let side = |dx: f64, dy: f64| {
            let pixels = mathf::ceil(mathf::hypot(dx / per_x, dy / per_y));
            let whole = if pixels.is_finite() {
                mathf::round_i32(pixels)
            } else {
                i32::MAX
            };
            u32::try_from(whole).unwrap_or(1).clamp(1, MAX_TILE_EXTENT)
        };
        Some((
            side(from_tile.a, from_tile.b),
            side(from_tile.c, from_tile.d),
        ))
    }

    /// Where in the unit tile `point` falls, both axes brought into `0..=1`
    /// by the repeat.
    ///
    /// `None` for a point the map sends nowhere finite, which paints nothing
    /// rather than indexing the tile with a `NaN`.
    #[must_use]
    pub fn tile_position(&self, point: (f64, f64)) -> Option<(f64, f64)> {
        let (u, v) = self.to_tile.apply(point);
        if !u.is_finite() || !v.is_finite() {
            return None;
        }
        Some((u - mathf::floor(u), v - mathf::floor(v)))
    }
}

/// A colour ramp: its geometry, its stops, and how it behaves outside them.
#[derive(Clone, Debug, PartialEq)]
pub struct Gradient {
    /// Whether the ramp runs along a line or out from a centre.
    pub kind: GradientKind,
    /// The colour ramp, ordered by ascending `offset` in `0..=1`.
    pub stops: Vec<GradientStop>,
    /// What happens outside `0..=1`.
    pub spread: SpreadMethod,
    /// Maps a point in the filled geometry's own coordinates into canonical
    /// gradient space.
    pub to_gradient: Affine,
}

impl Gradient {
    /// The straight-alpha colour at `point`, in the coordinate space the
    /// filled geometry is authored in.
    ///
    /// With no stops there is no colour to paint, so the answer is fully
    /// transparent; with one stop the ramp is that colour everywhere.
    #[must_use]
    pub fn sample(&self, point: (f64, f64)) -> Color {
        let (Some(first), Some(last)) = (self.stops.first(), self.stops.last()) else {
            return Color::TRANSPARENT;
        };
        let parameter = self
            .spread
            .wrap(self.kind.parameter(self.to_gradient.apply(point)));
        if parameter <= first.offset {
            return first.color;
        }
        if parameter >= last.offset {
            return last.color;
        }
        let index = self.stops.partition_point(|stop| stop.offset <= parameter);
        let (Some(below), Some(above)) = (
            index.checked_sub(1).and_then(|at| self.stops.get(at)),
            self.stops.get(index),
        ) else {
            return last.color;
        };
        let span = above.offset - below.offset;
        if span <= 0.0 {
            // Coincident offsets are a hard stop: the later colour wins.
            return above.color;
        }
        mix(below.color, above.color, (parameter - below.offset) / span)
    }
}

/// The geometry a gradient's parameter is measured along.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum GradientKind {
    /// The parameter is the x coordinate: the ramp runs from `x = 0` to
    /// `x = 1`.
    Linear,
    /// The parameter is the fraction of the way from `focal` to the unit
    /// circle, along the ray through the sampled point.
    Radial {
        /// Where the ramp starts, inside the unit circle. The origin is the
        /// plain concentric case.
        focal: (f64, f64),
    },
}

impl GradientKind {
    /// The ramp parameter at `point`, already in canonical gradient space and
    /// before the spread method brings it into `0..=1`.
    fn parameter(self, point: (f64, f64)) -> f64 {
        match self {
            Self::Linear => point.0,
            Self::Radial { focal } => radial_parameter(point, focal),
        }
    }
}

/// One colour in a ramp.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct GradientStop {
    /// Where along the ramp this colour sits, in `0..=1`.
    pub offset: f64,
    /// The colour itself, in straight alpha.
    pub color: Color,
}

/// What a gradient paints outside `0..=1`.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SpreadMethod {
    /// The end colours continue outwards.
    Pad,
    /// The ramp repeats, mirrored each time, so the seams match.
    Reflect,
    /// The ramp repeats from the start, with a visible seam.
    Repeat,
}

impl SpreadMethod {
    /// `parameter` brought into `0..=1`.
    ///
    /// The final clamp is what makes an extreme or non-finite parameter — a
    /// point mapped through a near-degenerate transform, or one far outside a
    /// tiny radial gradient — resolve to an end colour rather than escaping
    /// as a `NaN`.
    fn wrap(self, parameter: f64) -> f64 {
        let wrapped = match self {
            Self::Pad => parameter,
            Self::Repeat => parameter - mathf::floor(parameter),
            Self::Reflect => {
                let doubled = parameter - 2.0 * mathf::floor(0.5 * parameter);
                if doubled > 1.0 {
                    2.0 - doubled
                } else {
                    doubled
                }
            }
        };
        mathf::clamp(wrapped, 0.0, 1.0)
    }
}

/// The focal-ray parameter of `point`: how far it lies from `focal` as a
/// fraction of the distance from `focal` to the unit circle along the same
/// ray.
///
/// Solving `|focal + s * (point - focal)| = 1` for the positive root gives the
/// `s` that reaches the circle, and the answer is `1/s`. The root exists and
/// is strictly positive for every focal point inside the circle, which is what
/// [`clamp_focal`] guarantees.
fn radial_parameter(point: (f64, f64), focal: (f64, f64)) -> f64 {
    let (focal_x, focal_y) = clamp_focal(focal);
    let (dx, dy) = (point.0 - focal_x, point.1 - focal_y);
    let squared = dx * dx + dy * dy;
    if !squared.is_finite() || squared <= 0.0 {
        // The focal point itself is where the ramp starts.
        return 0.0;
    }
    let along = focal_x * dx + focal_y * dy;
    let gap = 1.0 - (focal_x * focal_x + focal_y * focal_y);
    let reach = (mathf::sqrt(along * along + squared * gap) - along) / squared;
    if reach > 0.0 {
        1.0 / reach
    } else {
        // Only reachable if the subtraction above cancels to zero, which puts
        // the point at the circle's edge.
        1.0
    }
}

/// `focal` moved just inside the unit circle if it is not already there.
fn clamp_focal(focal: (f64, f64)) -> (f64, f64) {
    let (focal_x, focal_y) = focal;
    if !focal_x.is_finite() || !focal_y.is_finite() {
        // An unusable focal point degrades to the concentric case rather than
        // poisoning the ray arithmetic.
        return (0.0, 0.0);
    }
    let distance = mathf::hypot(focal_x, focal_y);
    if distance <= FOCAL_LIMIT {
        return (focal_x, focal_y);
    }
    let scale = FOCAL_LIMIT / distance;
    (focal_x * scale, focal_y * scale)
}

/// `from` at `fraction` zero and `to` at one, interpolated per channel in
/// straight alpha so a ramp that fades out keeps its hue instead of darkening
/// toward black.
fn mix(from: Color, to: Color, fraction: f64) -> Color {
    Color::rgba(
        mix_channel(from.r, to.r, fraction),
        mix_channel(from.g, to.g, fraction),
        mix_channel(from.b, to.b, fraction),
        mix_channel(from.a, to.a, fraction),
    )
}

/// One channel of [`mix`], rounded to the nearest level and kept in range for
/// any `fraction`.
fn mix_channel(from: u8, to: u8, fraction: f64) -> u8 {
    let start = f64::from(from);
    let value = start + (f64::from(to) - start) * fraction;
    u8::try_from(mathf::round_i32(mathf::clamp(value, 0.0, 255.0))).unwrap_or(u8::MAX)
}

#[cfg(test)]
#[path = "paint_tests.rs"]
mod tests;
