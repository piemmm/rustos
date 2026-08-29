//! The rounding bias that keeps a translucent field from contouring.
//!
//! Compositing over an 8-bit destination costs tonal resolution that the
//! destination cannot give back: a source of alpha `a` admits only `256 - a`
//! of the 256 levels the picture beneath it held. With one fixed rounding
//! rule every input that lands in the same output bucket comes out *exactly*
//! equal, so a slow gradient — a dark sky behind a login screen, a wallpaper
//! under a translucent window — resolves into wide flat plateaus with a hard
//! step between them. That is banding, and no amount of arithmetic precision
//! removes it, because the levels to say it with are not there.
//!
//! What does remove it is spending the missing precision spatially: round at
//! a different fraction of a level in each pixel, so a value between two
//! output levels lands on the lower one in some pixels and the higher one in
//! others, and the *area* mean carries the fraction. The plateau becomes a
//! fine mix of two adjacent levels and the step disappears.
//!
//! The bias is an ordered (Bayer) dither matrix — the classic recursive 8×8
//! threshold pattern from Bayer, "An optimum method for two-level rendition
//! of continuous-tone pictures" (1973). It is a pure function of the pixel's
//! surface coordinates, so a frame is reproducible, two passes over the same
//! pixel agree, a re-composited rectangle matches the frame it replaces, and
//! nothing has to be allocated, seeded, or carried between frames. A caller
//! resolves one [`DitherRow`] per row and one tile of eight biases per span,
//! and then pays nothing per pixel beyond the bias it composites with.

use crate::color::ROUND_NEAREST;

/// The 8×8 ordered-dither matrix, in `0..64`.
///
/// Every value appears exactly once and neighbours are far apart in rank,
/// which is what makes the pattern break a contour instead of drawing a
/// visible ramp of its own.
const MATRIX: [[u8; 8]; 8] = [
    [0, 32, 8, 40, 2, 34, 10, 42],
    [48, 16, 56, 24, 50, 18, 58, 26],
    [12, 44, 4, 36, 14, 46, 6, 38],
    [60, 28, 52, 20, 62, 30, 54, 22],
    [3, 35, 11, 43, 1, 33, 9, 41],
    [51, 19, 59, 27, 49, 17, 57, 25],
    [15, 47, 7, 39, 13, 45, 5, 37],
    [63, 31, 55, 23, 61, 29, 53, 21],
];

/// The rounding biases one surface row dithers with, resolved once per row.
///
/// The pattern tiles the *surface's* own coordinates rather than any shape's,
/// so two spans that meet cannot show a seam where their phases disagree, and
/// a rectangle recomposited on its own lands exactly where the whole-screen
/// pass would have put it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DitherRow {
    biases: [u32; 8],
}

impl DitherRow {
    /// Columns after which the pattern repeats, and therefore the width of a
    /// [`tile_at`](Self::tile_at) run.
    pub(crate) const PERIOD: usize = 8;

    /// The row that rounds every pixel to nearest — no dither.
    ///
    /// What a caller composites with when it is not laying a field over a
    /// picture, so one code path serves both and the plain operator stays the
    /// same arithmetic at the same rounding point.
    pub const NEAREST: Self = Self {
        biases: [ROUND_NEAREST; 8],
    };

    /// The biases surface row `y` rounds with.
    #[must_use]
    pub const fn at(y: u32) -> Self {
        let cells = MATRIX[(y & 7) as usize];
        let mut biases = [0u32; 8];
        let mut x = 0;
        while x < 8 {
            // The 64 ranks spread evenly over a level: rank `k` rounds up
            // `(4k + 1)/255` early, so the tile's mean bias is exactly
            // `ROUND_NEAREST` and a dithered paint neither lightens nor
            // darkens what it covers.
            biases[x] = cells[x] as u32 * 4 + 1;
            x += 1;
        }
        Self { biases }
    }

    /// The bias surface column `x` of this row rounds with, in `0..255`.
    ///
    /// Never a whole level, so the bias only chooses *where* inside a level
    /// the value rounds up and can never shift it onto the next one.
    #[must_use]
    #[inline]
    pub const fn bias(&self, x: u32) -> u32 {
        self.biases[(x & 7) as usize]
    }

    /// The biases the eight columns from surface column `first_x` round with,
    /// in that order.
    ///
    /// What a span composite resolves once and then reads by each pixel's
    /// position *within the span*. The pattern's period is [`Self::PERIOD`],
    /// so a run walked eight pixels at a time takes a constant bias per lane
    /// and derives no surface column at all — which is what keeps the
    /// overflow-checked counter, and the panic branch it carries, out of the
    /// inner loop where it stops the loop vectorising.
    #[must_use]
    pub(crate) const fn tile_at(&self, first_x: u32) -> [u32; Self::PERIOD] {
        let phase = (first_x & 7) as usize;
        let mut tile = [0u32; Self::PERIOD];
        let mut lane = 0;
        while lane < Self::PERIOD {
            tile[lane] = self.biases[(phase + lane) & 7];
            lane += 1;
        }
        tile
    }
}

#[cfg(test)]
#[path = "dither_tests.rs"]
mod tests;
