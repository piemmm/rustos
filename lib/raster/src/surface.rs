//! A premultiplied-alpha pixel buffer.
//!
//! A [`Surface`] is a rendered CPU pixel buffer: the content of one
//! window for the compositor, or the painted body of the taskbar. It is
//! a dense row-major array of [`Pixel`]s with no padding; a consumer
//! places it on screen at an origin and blends it through [`Pixel::over`].

use alloc::vec;
use alloc::vec::Vec;

use crate::color::{Color, Pixel};
use crate::round::round_rect_coverage;

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
    /// than panicking.
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

    /// Build a surface from row-major, **straight**-alpha RGBA8 bytes (4
    /// bytes per pixel — the shape a decoded raster image, e.g.
    /// `tairix_image::RasterImage`, carries), premultiplying each pixel
    /// through the crate's one conversion path ([`Color::premultiply`])
    /// rather than duplicating that arithmetic here.
    ///
    /// Returns `None` if `rgba.len()` is not exactly `width * height * 4`
    /// (checked throughout, so an absurd `width`/`height` fails closed
    /// rather than panicking), the same failure contract [`Surface::new`]
    /// gives for a pixel count that could never be allocated.
    #[must_use]
    pub fn from_rgba8(width: u32, height: u32, rgba: &[u8]) -> Option<Self> {
        let count = pixel_count(width, height)?;
        let expected_len = count.checked_mul(4)?;
        if rgba.len() != expected_len {
            return None;
        }
        let (quads, _remainder) = rgba.as_chunks::<4>();
        let pixels = quads
            .iter()
            .map(|&[r, g, b, a]| Color::rgba(r, g, b, a).premultiply())
            .collect();
        Some(Self {
            width,
            height,
            pixels,
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

    /// Fill the rounded rectangle `[x, x+w) × [y, y+h)` with corner `radius`,
    /// compositing `color` over the existing pixels at each pixel's
    /// anti-aliased rounded-rectangle coverage.
    ///
    /// This is the single rounded-rectangle fill the desktop shares: a
    /// Reactive Alloy control plate rounds through here over the same
    /// [`round_rect_coverage`] the
    /// compositor rounds a window with, so there is never a second rounding
    /// definition. A `radius` of `0` is a square fill (like
    /// [`fill_rect`](Self::fill_rect) but through the compositing path); an
    /// over-large radius is clamped to half the shorter side. The rectangle is
    /// clipped to the surface bounds and a zero-size rectangle draws nothing.
    pub fn fill_round_rect(&mut self, x: u32, y: u32, w: u32, h: u32, radius: u32, color: Color) {
        if w == 0 || h == 0 {
            return;
        }
        let source = color.premultiply();
        let x_end = x.saturating_add(w).min(self.width);
        let y_end = y.saturating_add(h).min(self.height);
        for row in y..y_end {
            let local_y = row - y;
            for col in x..x_end {
                let local_x = col - x;
                let coverage = round_rect_coverage(local_x, local_y, w, h, radius);
                if coverage == 0 {
                    continue;
                }
                let src = source.scale_alpha(coverage);
                if let Some(dst) = self.get(col, row) {
                    self.set(col, row, src.over(dst));
                }
            }
        }
    }

    /// Fill an anti-aliased polygon onto this surface, compositing `color`
    /// over the existing pixels through the premultiplied-alpha
    /// [`Pixel::over`] path.
    ///
    /// The polygon's vertices are authored on a square `design`×`design`
    /// grid and mapped across the whole surface, so one piece of vector
    /// artwork fills a surface of any size crisply. This is the single
    /// supersampled polygon-fill path the desktop's vector assets share —
    /// pointer cursors (`lib/cursor`) and desktop icons (`lib/icon`)
    /// rasterise through here rather than each carrying its own scan
    /// converter.
    ///
    /// Each output pixel is probed on a fixed [`SUPERSAMPLE`]×[`SUPERSAMPLE`]
    /// sub-pixel grid and the fraction of samples inside the polygon becomes
    /// its coverage, applied to `color` before compositing. The single ring
    /// is filled with the even-odd rule. A polygon with fewer than three
    /// vertices covers no area and leaves the surface untouched; a
    /// degenerate `design` of zero is treated as `1`, so the call is total
    /// and never panics.
    ///
    /// [`Pixel::over`]: crate::color::Pixel::over
    pub fn fill_polygon(&mut self, polygon: &[(i32, i32)], design: u32, color: Color) {
        if polygon.len() < 3 {
            return;
        }
        let (Some(denom_x), Some(denom_y)) = (sample_span(self.width), sample_span(self.height))
        else {
            return;
        };
        let design = i64::from(design.max(1));
        let scaled: Vec<(i64, i64)> = polygon
            .iter()
            .map(|&(x, y)| {
                (
                    i64::from(x) * denom_x / design,
                    i64::from(y) * denom_y / design,
                )
            })
            .collect();

        let source = color.premultiply();
        let samples = SUPERSAMPLE * SUPERSAMPLE;
        for py in 0..self.height {
            for px in 0..self.width {
                let coverage = coverage_at(&scaled, px, py);
                if coverage == 0 {
                    continue;
                }
                let factor = coverage_to_alpha(coverage, samples);
                let src = source.scale_alpha(factor);
                if let Some(dst) = self.get(px, py) {
                    self.set(px, py, src.over(dst));
                }
            }
        }
    }

    /// Composite `src` over this surface with its top-left corner at
    /// `(x, y)`, clipped to the bounds.
    ///
    /// Every non-transparent source pixel is blended through the
    /// premultiplied-alpha [`Pixel::over`] path, so a transparent-background
    /// sprite (a rasterised cursor or icon) lays onto the destination
    /// without a rectangular halo. A negative origin or an over-large source
    /// simply clips the off-surface part rather than panicking.
    ///
    /// [`Pixel::over`]: crate::color::Pixel::over
    pub fn blit(&mut self, x: i32, y: i32, src: &Surface) {
        for sy in 0..src.height {
            for sx in 0..src.width {
                let Some(pixel) = src.get(sx, sy) else {
                    continue;
                };
                if pixel.a == 0 {
                    continue;
                }
                let (Some(dx), Some(dy)) = (add_offset(x, sx), add_offset(y, sy)) else {
                    continue;
                };
                if let Some(dst) = self.get(dx, dy) {
                    self.set(dx, dy, pixel.over(dst));
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

/// Sub-pixel samples per axis for anti-aliased polygon fills. A 4×4 grid
/// gives 17 distinct coverage levels per pixel, enough for smooth edges
/// without the cost of a larger kernel.
pub const SUPERSAMPLE: u32 = 4;

/// Add an unsigned source offset to a signed destination origin, returning
/// the destination coordinate only when it is non-negative and in `u32`
/// range (an off-surface coordinate clips rather than wrapping).
fn add_offset(origin: i32, offset: u32) -> Option<u32> {
    let sum = i64::from(origin) + i64::from(offset);
    if sum < 0 {
        return None;
    }
    u32::try_from(sum).ok()
}

/// `width * height` as a `usize`, or `None` on overflow.
fn pixel_count(width: u32, height: u32) -> Option<usize> {
    let count = u64::from(width).checked_mul(u64::from(height))?;
    usize::try_from(count).ok()
}

/// The number of sample sub-units spanned by `pixels` pixels: one pixel is
/// `2 * SUPERSAMPLE` sub-units wide, so sample centres land on odd offsets
/// and never on an exact polygon edge. `None` if the span overflows.
fn sample_span(pixels: u32) -> Option<i64> {
    let span = u64::from(pixels)
        .checked_mul(2)?
        .checked_mul(u64::from(SUPERSAMPLE))?;
    i64::try_from(span).ok()
}

/// The fixed-point coordinate of sub-sample `sub` within output pixel
/// `pixel`, in the same sample sub-units as a scaled polygon. The pixel
/// spans `[pixel*2*SS, (pixel+1)*2*SS)`; the `sub`-th sample centre sits at
/// `pixel*2*SS + 2*sub + 1`.
fn sample_coordinate(pixel: u32, sub: u32) -> i64 {
    let base = i64::from(pixel) * 2 * i64::from(SUPERSAMPLE);
    base + 2 * i64::from(sub) + 1
}

/// The number of sub-samples of pixel `(px, py)` that fall inside `polygon`.
fn coverage_at(polygon: &[(i64, i64)], px: u32, py: u32) -> u32 {
    let mut hits = 0;
    for sy in 0..SUPERSAMPLE {
        let sample_y = sample_coordinate(py, sy);
        for sx in 0..SUPERSAMPLE {
            let sample_x = sample_coordinate(px, sx);
            if point_in_polygon(polygon, sample_x, sample_y) {
                hits += 1;
            }
        }
    }
    hits
}

/// Even-odd point-in-polygon test in integer sample space.
///
/// A horizontal ray is cast in `+x`; each edge that straddles `py` flips the
/// inside flag when its crossing lies to the right of `px`. The comparison
/// is cross-multiplied (with the edge's vertical direction accounted for) so
/// no division is needed and the result stays exact.
fn point_in_polygon(polygon: &[(i64, i64)], px: i64, py: i64) -> bool {
    let mut inside = false;
    let n = polygon.len();
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = polygon[i];
        let (xj, yj) = polygon[j];
        if (yi > py) != (yj > py) {
            let lhs = (px - xi) * (yj - yi);
            let rhs = (xj - xi) * (py - yi);
            if yj - yi > 0 {
                if lhs < rhs {
                    inside = !inside;
                }
            } else if lhs > rhs {
                inside = !inside;
            }
        }
        j = i;
    }
    inside
}

/// Map a sample hit count to an alpha factor in `0..=255`.
fn coverage_to_alpha(hits: u32, samples: u32) -> u8 {
    if samples == 0 {
        return 0;
    }
    let scaled = u32::from(u8::MAX) * hits / samples;
    u8::try_from(scaled.min(u32::from(u8::MAX))).unwrap_or(u8::MAX)
}
