//! The compositor's pointer-cursor overlay.
//!
//! The cursor is the top-most layer of the scene: it is composited over
//! every window so it is always visible. The cursor *artwork* is not the
//! window manager's concern — it is a scalable, colourful, replaceable
//! [`CursorImage`] produced by `lib/cursor`. This
//! module only places that image on screen and samples it during
//! recomposition.
//!
//! [`CursorImage`]: rustos_cursor::CursorImage

use rustos_cursor::CursorImage;

use crate::color::Pixel;
use crate::geometry::{Point, Rect};

/// A rasterised cursor positioned on screen.
///
/// The stored [`origin`](Self::bounds) is the image's top-left corner,
/// derived from the pointer position by subtracting the image's hotspot, so
/// the hotspot lands exactly on the pointer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorLayer {
    image: CursorImage,
    origin: Point,
}

impl CursorLayer {
    /// Place `image` so its hotspot sits at `pointer`.
    #[must_use]
    pub fn new(image: CursorImage, pointer: Point) -> Self {
        let origin = top_left(&image, pointer);
        Self { image, origin }
    }

    /// Move the cursor so its hotspot sits at `pointer`.
    pub fn set_pointer(&mut self, pointer: Point) {
        self.origin = top_left(&self.image, pointer);
    }

    /// Replace the cursor artwork, keeping the hotspot at `pointer`.
    pub fn set_image(&mut self, image: CursorImage, pointer: Point) {
        self.origin = top_left(&image, pointer);
        self.image = image;
    }

    /// The screen rectangle the cursor currently covers.
    #[must_use]
    pub fn bounds(&self) -> Rect {
        Rect::new(
            self.origin.x,
            self.origin.y,
            self.image.width(),
            self.image.height(),
        )
    }

    /// The premultiplied cursor pixel at screen `(x, y)`, or `None` where
    /// the cursor draws nothing (outside its image, or a transparent pixel
    /// within it).
    #[must_use]
    pub fn sample(&self, x: i32, y: i32) -> Option<Pixel> {
        let local_x = u32::try_from(x.checked_sub(self.origin.x)?).ok()?;
        let local_y = u32::try_from(y.checked_sub(self.origin.y)?).ok()?;
        self.sample_local(local_x, local_y)
    }

    /// The premultiplied cursor pixel at *image-local* `(lx, ly)`, or
    /// `None` outside the image or where it draws nothing. The
    /// hardware-layer present path bakes the cursor into a layer through
    /// this, addressed in the image's own coordinate space.
    #[must_use]
    pub fn sample_local(&self, lx: u32, ly: u32) -> Option<Pixel> {
        let pixel = self.image.surface().get(lx, ly)?;
        (pixel.a > 0).then_some(pixel)
    }
}

/// The image's top-left corner for a hotspot placed at `pointer`.
fn top_left(image: &CursorImage, pointer: Point) -> Point {
    let hotspot = image.hotspot();
    Point::new(
        pointer.x.saturating_sub(hotspot.x),
        pointer.y.saturating_sub(hotspot.y),
    )
}
