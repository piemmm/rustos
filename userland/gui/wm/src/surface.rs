//! A premultiplied-alpha pixel buffer.
//!
//! A [`Surface`] is the rendered content of one window (or of the
//! compositor's back buffer). It is a dense row-major array of
//! [`Pixel`]s with no padding; the compositor places it on screen at a
//! window's origin and blends it through [`Pixel::over`].

use alloc::vec;
use alloc::vec::Vec;

use crate::color::{Color, Pixel};

/// A row-major, premultiplied-alpha pixel buffer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Surface {
    width: u32,
    height: u32,
    pixels: Vec<Pixel>,
}

impl Surface {
    /// Allocate a `width`×`height` surface cleared to fully transparent.
    ///
    /// Returns `None` if the pixel count overflows `usize` (a surface
    /// that could never be allocated), so the caller fails closed rather
    /// than panicking (§2.9).
    #[must_use]
    pub fn new(width: u32, height: u32) -> Option<Self> {
        Self::filled(width, height, Pixel::TRANSPARENT)
    }

    /// Allocate a `width`×`height` surface with every pixel set to
    /// `fill` (a premultiplied [`Pixel`]).
    #[must_use]
    pub fn filled(width: u32, height: u32, fill: Pixel) -> Option<Self> {
        let count = pixel_count(width, height)?;
        Some(Self {
            width,
            height,
            pixels: vec![fill; count],
        })
    }

    /// Surface width in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Surface height in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// Borrow the pixels in row-major order.
    #[must_use]
    pub fn pixels(&self) -> &[Pixel] {
        &self.pixels
    }

    /// The premultiplied pixel at `(x, y)`, or `None` if out of bounds.
    #[must_use]
    pub fn get(&self, x: u32, y: u32) -> Option<Pixel> {
        self.index(x, y).map(|i| self.pixels[i])
    }

    /// Overwrite the pixel at `(x, y)` with a premultiplied `pixel`.
    /// Out-of-bounds coordinates are ignored.
    pub fn set(&mut self, x: u32, y: u32, pixel: Pixel) {
        if let Some(i) = self.index(x, y) {
            self.pixels[i] = pixel;
        }
    }

    /// Fill the whole surface with `color` (premultiplied on the way in).
    pub fn fill(&mut self, color: Color) {
        let pixel = color.premultiply();
        self.pixels.iter_mut().for_each(|p| *p = pixel);
    }

    /// Fill the half-open rectangle `[x, x+w) × [y, y+h)` with `color`,
    /// clipped to the surface bounds.
    pub fn fill_rect(&mut self, x: u32, y: u32, w: u32, h: u32, color: Color) {
        let pixel = color.premultiply();
        let x_end = x.saturating_add(w).min(self.width);
        let y_end = y.saturating_add(h).min(self.height);
        for row in y..y_end {
            for col in x..x_end {
                if let Some(i) = self.index(col, row) {
                    self.pixels[i] = pixel;
                }
            }
        }
    }

    /// Row-major index of `(x, y)`, or `None` if out of bounds.
    fn index(&self, x: u32, y: u32) -> Option<usize> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let offset = u64::from(y) * u64::from(self.width) + u64::from(x);
        usize::try_from(offset).ok()
    }
}

/// `width * height` as a `usize`, or `None` on overflow.
fn pixel_count(width: u32, height: u32) -> Option<usize> {
    let count = u64::from(width).checked_mul(u64::from(height))?;
    usize::try_from(count).ok()
}
