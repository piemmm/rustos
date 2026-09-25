//! Where the player is looking, and what that makes a pixel.
//!
//! The projection is orthographic and axis-aligned — the world is
//! top-down and the camera does not rotate — so it is a scale and a
//! translate, and both are integer. A pixel spans a power-of-two number
//! of world sub-units, which is what lets the terrain pass walk a span
//! by shifting rather than dividing and keeps a frame bit-identical on
//! every target.
//!
//! The camera is clamped to the realm, not to the player: a player at the
//! map's edge walks toward the edge of the screen rather than the view
//! sliding off the generated world into nothing.
//!
//! The clamp is applied where the view is *projected*, not where the
//! camera is told what to look at, and the camera carries the realm's
//! extent to do it with. A camera that clamped only on being aimed would
//! be correct until the window grew: the wider view it then projected
//! would reach past an edge it had already settled against, and the
//! frame would draw ground the realm does not have. Clamping at
//! projection makes "the view is inside the realm" true of every extent
//! rather than of the last one the camera happened to be told about.

use tairix_wintersun_art::decal::Bounds;
use tairix_wintersun_net::value::WorldPoint;
use tairix_wintersun_world::geom::{CELL_SUB_UNITS, CHUNK_CELLS};
use tairix_wintersun_world::params::RealmParams;

use crate::view::Viewport;

/// How many world sub-units one pixel spans, as a power of two.
///
/// A power of two rather than a free number because the terrain pass steps
/// a span by adding a constant and reads a material mip chosen by
/// [`Mip::for_density`], both of which want a shift. It is also the whole
/// of the zoom control: there are five stops, and a stop is a doubling.
///
/// [`Mip::for_density`]: tairix_wintersun_art::material::Mip::for_density
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct Zoom(u32);

/// Closest the camera comes: one cell across 128 pixels.
const MIN_LEVEL: u32 = 3;
/// Furthest the camera goes: one cell across 8 pixels.
const MAX_LEVEL: u32 = 7;
/// One cell across 32 pixels, which is the view the game is authored at.
const DEFAULT_LEVEL: u32 = 5;

impl Zoom {
    /// The view the game is authored at: one world cell across 32 pixels.
    pub const DEFAULT: Self = Self(DEFAULT_LEVEL);
    /// Closest in.
    pub const NEAREST: Self = Self(MIN_LEVEL);
    /// Furthest out.
    pub const FURTHEST: Self = Self(MAX_LEVEL);

    /// The stop `level` doublings from one sub-unit per pixel, clamped to
    /// the range the renderer draws well at.
    #[must_use]
    pub const fn new(level: u32) -> Self {
        if level < MIN_LEVEL {
            Self(MIN_LEVEL)
        } else if level > MAX_LEVEL {
            Self(MAX_LEVEL)
        } else {
            Self(level)
        }
    }

    /// One stop closer, or `None` at the nearest.
    #[must_use]
    pub const fn nearer(self) -> Option<Self> {
        if self.0 > MIN_LEVEL {
            Some(Self(self.0 - 1))
        } else {
            None
        }
    }

    /// One stop further out, or `None` at the furthest.
    #[must_use]
    pub const fn further(self) -> Option<Self> {
        if self.0 < MAX_LEVEL {
            Some(Self(self.0 + 1))
        } else {
            None
        }
    }

    /// Log2 of the sub-units one pixel spans.
    #[must_use]
    pub const fn level(self) -> u32 {
        self.0
    }

    /// The sub-units one pixel spans.
    #[must_use]
    pub const fn sub_units_per_pixel(self) -> i32 {
        1 << self.0
    }

    /// The pixels one world cell spans.
    #[must_use]
    pub const fn pixels_per_cell(self) -> u32 {
        (CELL_SUB_UNITS as u32) >> self.0
    }
}

impl Default for Zoom {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The extent of a realm in world sub-units.
///
/// Derived from the realm's own chunk extent rather than stated
/// separately, so a realm of a different size needs no second number.
#[must_use]
pub fn realm_bounds(params: RealmParams) -> Bounds {
    let cell = i64::from(CELL_SUB_UNITS);
    let cells = i64::from(CHUNK_CELLS);
    let min = i64::from(params.min_chunk()) * cells * cell;
    let max = (i64::from(params.max_chunk()) + 1) * cells * cell;
    let clamp = |v: i64| i32::try_from(v).unwrap_or(if v < 0 { i32::MIN } else { i32::MAX });
    Bounds {
        min_x: clamp(min),
        min_y: clamp(min),
        max_x: clamp(max),
        max_y: clamp(max),
    }
}

/// What the player is looking at.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Camera {
    target: WorldPoint,
    zoom: Zoom,
    bounds: Bounds,
}

impl Camera {
    /// A camera aimed at `target`, at `zoom`, inside `bounds`.
    #[must_use]
    pub const fn new(target: WorldPoint, zoom: Zoom, bounds: Bounds) -> Self {
        Self {
            target,
            zoom,
            bounds,
        }
    }

    /// What it has been aimed at, which is not necessarily what it can
    /// show: a target outside the realm is settled onto the nearest view
    /// that is inside it.
    #[must_use]
    pub const fn target(&self) -> WorldPoint {
        self.target
    }

    /// The realm it is confined to.
    #[must_use]
    pub const fn bounds(&self) -> Bounds {
        self.bounds
    }

    /// Aim it. The clamp happens where the view is projected, so this
    /// records a wish and cannot be wrong for an extent it has not seen.
    pub fn look_at(&mut self, target: WorldPoint) {
        self.target = target;
    }

    /// The world sub-units one of `view`'s render pixels spans.
    #[must_use]
    pub fn step(&self, view: &Viewport) -> i32 {
        view.step(self.zoom)
    }

    /// Where `view` is actually centred.
    ///
    /// A realm narrower than the view is centred rather than clamped to
    /// an edge it does not reach, so a small realm is drawn in the middle
    /// of the window instead of pinned to a corner.
    #[must_use]
    pub fn centre(&self, view: &Viewport) -> WorldPoint {
        let (width, height) = view.render();
        let span = i64::from(self.step(view));
        let half_w = i64::from(width) * span / 2;
        let half_h = i64::from(height) * span / 2;
        WorldPoint {
            x: saturate(clamp_axis(
                i64::from(self.target.x),
                i64::from(self.bounds.min_x),
                i64::from(self.bounds.max_x),
                half_w,
            )),
            y: saturate(clamp_axis(
                i64::from(self.target.y),
                i64::from(self.bounds.min_y),
                i64::from(self.bounds.max_y),
                half_h,
            )),
        }
    }

    /// How close in it is.
    #[must_use]
    pub const fn zoom(&self) -> Zoom {
        self.zoom
    }

    /// Change the zoom, keeping the centre.
    pub fn set_zoom(&mut self, zoom: Zoom) {
        self.zoom = zoom;
    }

    /// The world point the top-left pixel of `view` samples.
    ///
    /// Everything else in the projection is this plus a multiple of the
    /// pixel span, so it is computed once per frame and stepped.
    #[must_use]
    pub fn origin(&self, view: &Viewport) -> WorldPoint {
        let (width, height) = view.render();
        let span = i64::from(self.step(view));
        let centre = self.centre(view);
        let x = i64::from(centre.x) - i64::from(width) * span / 2;
        let y = i64::from(centre.y) - i64::from(height) * span / 2;
        WorldPoint {
            x: saturate(x),
            y: saturate(y),
        }
    }

    /// The world extent `view` covers.
    ///
    /// The edges are inclusive of the last pixel's sample, which is what
    /// the chunk and decal bucketing either side of this expects.
    #[must_use]
    pub fn visible(&self, view: &Viewport) -> Bounds {
        let (width, height) = view.render();
        let span = i64::from(self.step(view));
        let origin = self.origin(view);
        Bounds {
            min_x: origin.x,
            min_y: origin.y,
            max_x: saturate(i64::from(origin.x) + i64::from(width.max(1) - 1) * span),
            max_y: saturate(i64::from(origin.y) + i64::from(height.max(1) - 1) * span),
        }
    }

    /// The world point a render pixel of `view` samples.
    #[must_use]
    pub fn world_at(&self, view: &Viewport, px: i32, py: i32) -> WorldPoint {
        let span = i64::from(self.step(view));
        let origin = self.origin(view);
        WorldPoint {
            x: saturate(i64::from(origin.x) + i64::from(px) * span),
            y: saturate(i64::from(origin.y) + i64::from(py) * span),
        }
    }

    /// The render pixel of `view` a world point falls in, whether or not
    /// that pixel is on screen.
    ///
    /// Off-screen answers are the point: a sprite straddling the edge is
    /// drawn clipped, not dropped, so the caller wants the coordinate and
    /// decides the clip itself.
    #[must_use]
    pub fn screen_at(&self, view: &Viewport, at: WorldPoint) -> (i32, i32) {
        pixel_of(self.origin(view), self.step(view), at)
    }
}

/// The render pixel `at` falls in, for a view whose top-left pixel samples
/// `origin` at `step` world sub-units a pixel: floored, so a point west or
/// north of the origin lands in the pixel before it rather than in it.
pub(crate) fn pixel_of(origin: WorldPoint, step: i32, at: WorldPoint) -> (i32, i32) {
    let span = i64::from(step.max(1));
    let sx = (i64::from(at.x) - i64::from(origin.x)).div_euclid(span);
    let sy = (i64::from(at.y) - i64::from(origin.y)).div_euclid(span);
    (saturate(sx), saturate(sy))
}

/// Keep `want` such that `want ± half` stays within `[min, max]`, centring
/// instead when the extent is narrower than the view.
fn clamp_axis(want: i64, min: i64, max: i64, half: i64) -> i64 {
    if max - min <= half * 2 {
        return i64::midpoint(min, max);
    }
    want.clamp(min + half, max - half)
}

/// Bring a 64-bit world coordinate back into the 32-bit one the wire uses.
///
/// Reached only by a camera driven past the realm's own extent, where the
/// edge is the honest answer and wrapping would put the view on the far
/// side of the map.
fn saturate(v: i64) -> i32 {
    i32::try_from(v).unwrap_or(if v < 0 { i32::MIN } else { i32::MAX })
}

#[cfg(test)]
#[path = "camera_tests.rs"]
mod tests;
