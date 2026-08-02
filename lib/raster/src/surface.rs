//! A premultiplied-alpha pixel buffer.
//!
//! A [`Surface`] is a rendered CPU pixel buffer: the content of one
//! window for the compositor, or the painted body of the taskbar. It is
//! a dense row-major array of [`Pixel`]s with no padding; a consumer
//! places it on screen at an origin and blends it through [`Pixel::over`].

use core::mem::size_of;
use core::ops::Range;

use alloc::vec;
use alloc::vec::Vec;

use tairix_reclaim::CachedBytes;

use crate::color::{Color, Pixel};
use crate::round::round_rect_coverage;

/// A row-major, premultiplied-alpha pixel buffer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Surface {
    width: u32,
    height: u32,
    pixels: Vec<Pixel>,
}

impl CachedBytes for Surface {
    /// The retained heap size of the pixel buffer — the only heap
    /// allocation a `Surface` owns.
    fn payload_bytes(&self) -> usize {
        self.pixels.len() * size_of::<Pixel>()
    }

    /// Overwrite every pixel with fully transparent black, so a reclaimed
    /// surface leaves no rendered user data behind in freed heap memory.
    fn wipe(&mut self) {
        self.pixels.fill(Pixel::TRANSPARENT);
    }
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
    ///
    /// The row range is computed once and each row is written with a single
    /// slice fill, so the cost is proportional to the clipped rectangle's
    /// area, never the whole surface.
    pub fn fill_rect(&mut self, x: u32, y: u32, w: u32, h: u32, color: Color) {
        let pixel = color.premultiply();
        let x_end = x.saturating_add(w).min(self.width);
        let y_end = y.saturating_add(h).min(self.height);
        if x >= x_end || y >= y_end {
            return;
        }
        for row in y..y_end {
            let Some(start) = self.row_start(row) else {
                continue;
            };
            self.pixels[start + x as usize..start + x_end as usize].fill(pixel);
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
    ///
    /// Only the four `radius`×`radius` corner squares can be partially
    /// covered, so the fill is split into those and the fully-covered
    /// remainder: an interior row, and the middle span of a corner row, take
    /// the same whole-span path [`fill_rect`](Self::fill_rect) uses (a single
    /// slice fill when `color` is opaque), and only a corner pixel evaluates
    /// [`round_rect_coverage`]. A panel rounded by a few pixels therefore
    /// costs a rectangle fill plus its corners rather than a coverage
    /// evaluation per pixel, with the row range computed once per row.
    pub fn fill_round_rect(&mut self, x: u32, y: u32, w: u32, h: u32, radius: u32, color: Color) {
        if w == 0 || h == 0 {
            return;
        }
        let source = color.premultiply();
        let x_end = x.saturating_add(w).min(self.width);
        let y_end = y.saturating_add(h).min(self.height);
        if x >= x_end || y >= y_end {
            return;
        }
        // The clamp `round_rect_coverage` applies internally, applied here
        // too so the bands below name exactly the pixels it does not answer
        // 255 for. Being at most half the shorter side, the radius never
        // exceeds `w`, so neither subtraction can wrap.
        let radius = radius.min(w / 2).min(h / 2);
        let visible_w = x_end - x;
        let left_corner_end = radius.min(visible_w);
        let right_corner_start = (w - radius).min(visible_w);
        for row in y..y_end {
            let Some(start) = self.row_start(row) else {
                continue;
            };
            let local_y = row - y;
            let row_pixels = &mut self.pixels[start + x as usize..start + x_end as usize];
            if !in_corner_band(local_y, h, radius) {
                composite_span(row_pixels, source);
                continue;
            }
            let (left, rest) = row_pixels.split_at_mut(left_corner_end as usize);
            let (middle, right) =
                rest.split_at_mut((right_corner_start - left_corner_end) as usize);
            composite_coverage_span(left, 0..left_corner_end, local_y, w, h, radius, source);
            composite_span(middle, source);
            composite_coverage_span(
                right,
                right_corner_start..visible_w,
                local_y,
                w,
                h,
                radius,
                source,
            );
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
    /// Only the polygon's bounding box, clipped to the surface, is scanned:
    /// every sample outside it would test as uncovered anyway, so a small
    /// shape on a large surface (a cursor or an icon glyph) costs its own
    /// area rather than the whole canvas.
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

        let Some((x_start, x_end, y_start, y_end)) =
            polygon_pixel_bounds(&scaled, self.width, self.height)
        else {
            return;
        };

        let source = color.premultiply();
        let samples = SUPERSAMPLE * SUPERSAMPLE;
        for py in y_start..y_end {
            let Some(start) = self.row_start(py) else {
                continue;
            };
            let row = &mut self.pixels[start + x_start as usize..start + x_end as usize];
            for (px, dst) in (x_start..x_end).zip(row.iter_mut()) {
                let coverage = coverage_at(&scaled, px, py);
                if coverage == 0 {
                    continue;
                }
                let factor = coverage_to_alpha(coverage, samples);
                let src = source.scale_alpha(factor);
                *dst = src.over(*dst);
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
    /// The overlapping column range is clipped once, outside the row loop,
    /// and each row is then copied through paired slice iteration rather
    /// than a per-pixel bounds check and index recomputation, so the cost is
    /// the overlap area, not the whole surface.
    ///
    /// [`Pixel::over`]: crate::color::Pixel::over
    pub fn blit(&mut self, x: i32, y: i32, src: &Surface) {
        let dst_width = i64::from(self.width);
        let src_width = i64::from(src.width);
        let x64 = i64::from(x);
        // The source columns that land on a valid destination column: `sx`
        // with `0 <= x + sx < self.width`, intersected with `src`'s own
        // width. Computing this once, rather than per pixel, is what turns
        // the inner loop below into a plain paired-slice walk.
        let sx_lo = (-x64).max(0);
        let sx_hi = (dst_width - x64).min(src_width);
        if sx_lo >= sx_hi {
            return;
        }
        let sx_start = u32::try_from(sx_lo).unwrap_or(0);
        let sx_end = u32::try_from(sx_hi).unwrap_or(src.width);
        let dst_col_start = u32::try_from(x64 + sx_lo).unwrap_or(0);
        let row_len = (sx_end - sx_start) as usize;

        for sy in 0..src.height {
            let Some(dy) = add_offset(y, sy) else {
                continue;
            };
            let (Some(dst_row), Some(src_row)) = (self.row_start(dy), src.row_start(sy)) else {
                continue;
            };
            let src_slice = &src.pixels[src_row + sx_start as usize..src_row + sx_end as usize];
            let dst_start = dst_row + dst_col_start as usize;
            let dst_slice = &mut self.pixels[dst_start..dst_start + row_len];
            for (pixel, dst) in src_slice.iter().zip(dst_slice.iter_mut()) {
                if pixel.a != 0 {
                    *dst = pixel.over(*dst);
                }
            }
        }
    }

    /// Borrow row `y`'s pixels left to right, or `None` if `y` is out of
    /// bounds.
    ///
    /// A consumer that composites through a mask of its own — the glyph
    /// blitter in `lib/font` scaling a text colour by an 8-bit coverage
    /// bitmap — writes a row at a time through this, paying one bounds check
    /// and one index computation per row rather than per pixel. The pixels
    /// stay premultiplied: this is [`set`](Self::set)'s contract at row
    /// granularity.
    #[must_use]
    pub fn row_mut(&mut self, y: u32) -> Option<&mut [Pixel]> {
        let start = self.row_start(y)?;
        let end = start.checked_add(self.width as usize)?;
        self.pixels.get_mut(start..end)
    }

    /// Row-major index of `(x, y)`, or `None` if out of bounds.
    fn index(&self, x: u32, y: u32) -> Option<usize> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let offset = u64::from(y) * u64::from(self.width) + u64::from(x);
        usize::try_from(offset).ok()
    }

    /// Row-major index of the first pixel of row `y`, or `None` if `y` is
    /// out of bounds.
    ///
    /// The row-wise fill and blit paths call this once per row instead of
    /// recomputing `y * width + x` (via [`Self::index`]) for every pixel in
    /// it, then read or write the rest of the row through plain slicing.
    fn row_start(&self, y: u32) -> Option<usize> {
        if y >= self.height {
            return None;
        }
        let offset = u64::from(y) * u64::from(self.width);
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

/// Whether `local` falls in one of the two `radius`-wide bands at the ends of
/// a `size`-long side of a rounded rectangle — the only rows or columns a
/// corner arc can reach. A zero radius has no such band.
///
/// The caller clamps `radius` to half the shorter side, so the subtraction
/// cannot wrap.
fn in_corner_band(local: u32, size: u32, radius: u32) -> bool {
    local < radius || local >= size - radius
}

/// Composite `source` at full coverage over every pixel of `span`.
///
/// Compositing a fully opaque source yields that source unchanged, so an
/// opaque span is one slice fill rather than a per-pixel blend.
fn composite_span(span: &mut [Pixel], source: Pixel) {
    if source.a == 255 {
        span.fill(source);
        return;
    }
    for dst in span.iter_mut() {
        *dst = source.over(*dst);
    }
}

/// Composite `source` over one corner span of a `w`×`h` rounded rectangle of
/// corner `radius`, scaling it by each pixel's anti-aliased coverage.
///
/// `columns` are the span pixels' x coordinates local to the rectangle,
/// paired one for one with `span`, and `local_y` is its row.
fn composite_coverage_span(
    span: &mut [Pixel],
    columns: Range<u32>,
    local_y: u32,
    w: u32,
    h: u32,
    radius: u32,
    source: Pixel,
) {
    for (local_x, dst) in columns.zip(span.iter_mut()) {
        let coverage = round_rect_coverage(local_x, local_y, w, h, radius);
        if coverage == 0 {
            continue;
        }
        *dst = source.scale_alpha(coverage).over(*dst);
    }
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

/// The pixel-space bounding box `[x_start, x_end) × [y_start, y_end)` that
/// could contain any sample of `polygon` — already in the scaled sample
/// units [`coverage_at`] consumes — intersected with a `width`×`height`
/// canvas. `None` when the polygon's extent misses the canvas entirely.
///
/// A sample outside a vertex's extreme coordinate can never be inside the
/// polygon, so no pixel outside this box can have non-zero coverage; a
/// small shape (a cursor or an icon glyph) therefore costs its own bounding
/// box, not the whole canvas, with the output identical to scanning every
/// pixel and discarding the zero-coverage ones.
fn polygon_pixel_bounds(
    polygon: &[(i64, i64)],
    width: u32,
    height: u32,
) -> Option<(u32, u32, u32, u32)> {
    let mut min_x = i64::MAX;
    let mut max_x = i64::MIN;
    let mut min_y = i64::MAX;
    let mut max_y = i64::MIN;
    for &(px, py) in polygon {
        min_x = min_x.min(px);
        max_x = max_x.max(px);
        min_y = min_y.min(py);
        max_y = max_y.max(py);
    }

    // Each pixel `p` spans sample units `[p * scale, (p + 1) * scale)`, so
    // `div_euclid` (the mathematical floor, correct for a negative vertex
    // too) gives the pixel that owns each extreme coordinate.
    let scale = i64::from(2 * SUPERSAMPLE);
    let x_start = min_x.div_euclid(scale).max(0);
    let x_end = (max_x.div_euclid(scale) + 1).min(i64::from(width));
    let y_start = min_y.div_euclid(scale).max(0);
    let y_end = (max_y.div_euclid(scale) + 1).min(i64::from(height));
    if x_start >= x_end || y_start >= y_end {
        return None;
    }
    // Each bound above is clamped into `0..=width` or `0..=height`, so the
    // conversion is always exact; the fallback only guards against a future
    // change to the clamps above changing that invariant.
    Some((
        u32::try_from(x_start).unwrap_or(0),
        u32::try_from(x_end).unwrap_or(width),
        u32::try_from(y_start).unwrap_or(0),
        u32::try_from(y_end).unwrap_or(height),
    ))
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
