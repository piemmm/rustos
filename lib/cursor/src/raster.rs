//! Rasterising a [`VectorCursor`] onto a `lib/raster` [`Surface`] at any
//! scale.
//!
//! Scaling is what makes the vector representation worthwhile: a cursor
//! authored once on its design grid is rendered at whatever pixel size a
//! display's DPI calls for. The fill is anti-aliased and composited through
//! `lib/raster`'s single supersampled [`Surface::fill_polygon`] path, so the
//! cursor library owns no scan converter or colour arithmetic of its own. Out-of-range scales and degenerate cursors fail
//! closed with `None` rather than panicking.

use alloc::vec::Vec;

use tairix_geometry::Point;
use tairix_raster::Surface;

use crate::vector::VectorCursor;

/// A rasterised cursor: an opaque-where-drawn pixel image plus the hotspot
/// expressed in that image's own pixel coordinates.
///
/// The window manager blits [`surface`](Self::surface) so that
/// [`hotspot`](Self::hotspot) lands on the pointer position; the surface is
/// transparent everywhere the cursor does not draw, so it composites over
/// the desktop correctly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorImage {
    surface: Surface,
    hotspot: Point,
}

impl CursorImage {
    /// The rendered pixels, transparent outside the cursor artwork.
    #[must_use]
    pub fn surface(&self) -> &Surface {
        &self.surface
    }

    /// The hotspot in this image's pixel coordinates.
    #[must_use]
    pub const fn hotspot(&self) -> Point {
        self.hotspot
    }

    /// The image width in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.surface.width()
    }

    /// The image height in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.surface.height()
    }
}

impl VectorCursor {
    /// The square pixel side this cursor rasterises to at `scale_percent`,
    /// or `None` if the cursor or the scale is degenerate.
    ///
    /// `scale_percent` is relative to the design grid: `100` renders one
    /// pixel per design unit, `200` doubles it, `50` halves it. A side of
    /// zero (an empty design grid or a scale that rounds the artwork away to
    /// nothing) is not renderable and reported as `None`.
    #[must_use]
    pub fn footprint(&self, scale_percent: u32) -> Option<u32> {
        let side = u64::from(self.design_size()).checked_mul(u64::from(scale_percent))? / 100;
        let side = u32::try_from(side).ok()?;
        (side > 0).then_some(side)
    }

    /// Rasterise this cursor at `scale_percent` (see [`footprint`]).
    ///
    /// Returns `None` for a degenerate cursor or scale, or if the resulting
    /// pixel buffer cannot be allocated — the caller falls back to a smaller
    /// scale or a different cursor rather than crashing.
    /// Each shape is filled through the shared [`Surface::fill_polygon`] path
    /// in stack order, so a dark outline beneath a light body stays legible.
    ///
    /// [`footprint`]: Self::footprint
    #[must_use]
    pub fn rasterise(&self, scale_percent: u32) -> Option<CursorImage> {
        let side = self.footprint(scale_percent)?;
        let mut surface = Surface::new(side, side)?;
        let design = self.design_size();

        for shape in self.shapes() {
            let polygon: Vec<(i32, i32)> = shape.polygon.iter().map(|v| (v.x, v.y)).collect();
            surface.fill_polygon(&polygon, design, shape.fill);
        }

        let hotspot = self.scaled_hotspot(scale_percent, side);
        Some(CursorImage { surface, hotspot })
    }

    /// The hotspot scaled into output pixels and clamped to the image.
    fn scaled_hotspot(&self, scale_percent: u32, side: u32) -> Point {
        let scale = |design: i32| -> i32 {
            let value = i64::from(design) * i64::from(scale_percent) / 100;
            let max = i64::from(side.saturating_sub(1));
            let clamped = value.clamp(0, max);
            i32::try_from(clamped).unwrap_or(0)
        };
        Point::new(scale(self.hotspot_x()), scale(self.hotspot_y()))
    }
}
