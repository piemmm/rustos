//! Rasterising a [`VectorCursor`] onto a `lib/raster` [`Surface`] at any
//! pixel side.
//!
//! Scaling is what makes the vector representation worthwhile: a cursor
//! authored once on its design grid is rendered at whatever pixel size a
//! display's DPI and the user's chosen pointer size call for. The artwork
//! goes through `lib/raster`'s single [`Surface::draw_artwork`] path — every
//! pixel taking the exact area a shape covers of it — and the stack through
//! its one [`Surface::layered`] composition, so the cursor library owns no
//! scan converter or colour arithmetic of its own. A degenerate side or
//! cursor fails closed with `None` rather than panicking.
//!
//! The side is asked for in **pixels**, not as a factor of the asset's own
//! design grid, because the grid is an authoring detail that differs
//! between a built-in cursor and a decoded SVG one: a caller naming a
//! factor would get a different pointer size from each, so swapping cursor
//! sets would resize the pointer. The caller names the size it wants and
//! every set honours it.

use tairix_geometry::Point;
use tairix_raster::{layer_count, Surface};
use tairix_reclaim::CachedBytes;

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

impl CachedBytes for CursorImage {
    /// The image's only heap allocation is its `Surface`'s pixel buffer;
    /// the hotspot is a plain `Copy` coordinate pair with no heap part.
    fn payload_bytes(&self) -> usize {
        self.surface.payload_bytes()
    }

    /// Delegate to the surface's own wipe: the hotspot carries no
    /// rendered data worth clearing.
    fn wipe(&mut self) {
        self.surface.wipe();
    }
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
    /// Rasterise this cursor into a `side`x`side` pixel image.
    ///
    /// Returns `None` for a zero `side`, a cursor whose design grid is
    /// degenerate, a pixel buffer that cannot be allocated, or a group whose
    /// isolation buffer cannot be — the caller falls back to a smaller side
    /// or a different cursor rather than showing a half-composited one.
    /// The artwork is drawn through the shared [`Surface::draw_artwork`] path
    /// in stack order, so a dark outline beneath a light body stays legible,
    /// and the stack goes through [`Surface::layered`] so the body meets that
    /// outline without the pale seam that compositing already-anti-aliased
    /// shapes leaves.
    #[must_use]
    pub fn rasterise(&self, side: u32) -> Option<CursorImage> {
        let design = self.design_size();
        if side == 0 || design == 0 {
            return None;
        }

        let mut drawn = false;
        let surface = Surface::layered(side, side, layer_count(self.nodes()), |surface| {
            drawn = surface.draw_artwork(self.nodes(), design);
        })?;

        drawn.then(|| CursorImage {
            surface,
            hotspot: self.scaled_hotspot(side, design),
        })
    }

    /// The hotspot mapped from the design grid onto a `side`-pixel image and
    /// clamped inside it.
    ///
    /// `design` is non-zero by the caller's own check, so the division is
    /// sound and the pointer can never be handed a hotspot outside the
    /// artwork it belongs to.
    fn scaled_hotspot(&self, side: u32, design: u32) -> Point {
        let onto = |coordinate: i32| -> i32 {
            let value = i64::from(coordinate) * i64::from(side) / i64::from(design);
            let clamped = value.clamp(0, i64::from(side.saturating_sub(1)));
            i32::try_from(clamped).unwrap_or(0)
        };
        Point::new(onto(self.hotspot_x()), onto(self.hotspot_y()))
    }
}
