//! A premultiplied-alpha pixel buffer.
//!
//! A [`Surface`] is a rendered CPU pixel buffer: the content of one
//! window for the compositor, or the painted body of the taskbar. It is
//! a dense row-major array of [`Pixel`]s with no padding; a consumer
//! places it on screen at an origin and blends it through [`Pixel::over`].
//!
//! Painting is confined by a clip window ([`Surface::with_clip`]): a view
//! bounds what it draws to the area it owns — an item grid confines its tiles
//! to its item area — by stating that bound once, rather than every drawing
//! routine trimming its own geometry to an edge.

use core::mem::size_of;
use core::num::NonZeroU64;
use core::ops::Range;

use alloc::vec;
use alloc::vec::Vec;

use tairix_reclaim::CachedBytes;

use crate::color::{Color, Pixel};
use crate::round::round_rect_coverage;

/// The half-open pixel window a paint is confined to: `[x0, x1) × [y0, y1)`.
///
/// It is always already intersected with the surface bounds, so `x1 <= width`
/// and `y1 <= height` hold by construction and a write path can enforce the
/// window and the bounds in one test. An empty window (`x0 == x1` or
/// `y0 == y1`) admits nothing.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct ClipRect {
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
}

impl ClipRect {
    /// The window admitting a whole `width`×`height` surface.
    const fn whole(width: u32, height: u32) -> Self {
        Self {
            x0: 0,
            y0: 0,
            x1: width,
            y1: height,
        }
    }

    /// This window narrowed to `[x, x+w) × [y, y+h)`.
    ///
    /// An intersection can only shrink, so a nested clip can never widen what
    /// its parent admitted and a caller cannot escape an enclosing view's area
    /// by asking for a larger one. A window that intersects to nothing is
    /// normalised to empty rather than inverted.
    fn narrowed(self, x: u32, y: u32, w: u32, h: u32) -> Self {
        let x0 = self.x0.max(x);
        let y0 = self.y0.max(y);
        let x1 = self.x1.min(x.saturating_add(w));
        let y1 = self.y1.min(y.saturating_add(h));
        Self {
            x0,
            y0,
            x1: x1.max(x0),
            y1: y1.max(y0),
        }
    }

    /// The rows of `[y, y+h)` this window admits.
    fn rows(self, y: u32, h: u32) -> Range<u32> {
        let start = y.max(self.y0);
        let end = y.saturating_add(h).min(self.y1);
        start..end.max(start)
    }

    /// The columns of `[x, x+w)` this window admits, or `None` when none of
    /// them survive.
    fn columns(self, x: u32, w: u32) -> Option<Range<u32>> {
        let start = x.max(self.x0);
        let end = x.saturating_add(w).min(self.x1);
        (start < end).then_some(start..end)
    }
}

/// A row-major, premultiplied-alpha pixel buffer.
///
/// Two surfaces are equal when they carry the same pixels *and* the same clip
/// window. [`Surface::with_clip`] restores the previous window before it
/// returns, so a surface observed outside a clipped paint always carries the
/// whole-surface window and equality is decided by its pixels alone.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Surface {
    width: u32,
    height: u32,
    clip: ClipRect,
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
            clip: ClipRect::whole(width, height),
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
            clip: ClipRect::whole(width, height),
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
    /// Coordinates outside the surface or the active clip window are ignored.
    pub fn set(&mut self, x: u32, y: u32, pixel: Pixel) {
        if let Some((_, span)) = self.row_span_mut(y, x, 1) {
            if let Some(dst) = span.first_mut() {
                *dst = pixel;
            }
        }
    }

    /// Fill the surface with `color` (premultiplied on the way in), within the
    /// active clip window.
    pub fn fill(&mut self, color: Color) {
        self.fill_rect(0, 0, self.width, self.height, color);
    }

    /// Fill the half-open rectangle `[x, x+w) × [y, y+h)` with `color`,
    /// clipped to the surface bounds and the active clip window.
    ///
    /// The admitted row range is computed once and each row is written with a
    /// single slice fill, so the cost is proportional to the clipped
    /// rectangle's area, never the whole surface.
    pub fn fill_rect(&mut self, x: u32, y: u32, w: u32, h: u32, color: Color) {
        let pixel = color.premultiply();
        for row in self.clip.rows(y, h) {
            if let Some((_, span)) = self.row_span_mut(row, x, w) {
                span.fill(pixel);
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
    ///
    /// Only the four `radius`×`radius` corner squares can be partially
    /// covered, so the fill is split into those and the fully-covered
    /// remainder: an interior row, and the middle span of a corner row, take
    /// the same whole-span path [`fill_rect`](Self::fill_rect) uses (a single
    /// slice fill when `color` is opaque), and only a corner pixel evaluates
    /// [`round_rect_coverage`]. A panel rounded by a few pixels therefore
    /// costs a rectangle fill plus its corners rather than a coverage
    /// evaluation per pixel, with the row range computed once per row.
    ///
    /// Coverage is evaluated in the rectangle's own coordinates, so a
    /// rectangle the surface bounds or the clip window cut short keeps the
    /// corner arcs of the whole shape rather than re-rounding what survives.
    pub fn fill_round_rect(&mut self, x: u32, y: u32, w: u32, h: u32, radius: u32, color: Color) {
        if w == 0 || h == 0 {
            return;
        }
        let source = color.premultiply();
        // The clamp `round_rect_coverage` applies internally, applied here
        // too so the bands below name exactly the pixels it does not answer
        // 255 for. Being at most half the shorter side, the radius never
        // exceeds `w`, so neither subtraction can wrap.
        let radius = radius.min(w / 2).min(h / 2);
        let right_band = w - radius;
        for row in self.clip.rows(y, h) {
            let local_y = row - y;
            let Some((first, span)) = self.row_span_mut(row, x, w) else {
                continue;
            };
            if !in_corner_band(local_y, h, radius) {
                composite_span(span, source);
                continue;
            }
            // The drawn columns as the rectangle sees them: `lead` is the
            // first, and a span never reaches past the rectangle's width, so
            // `lead + drawn <= w` and the band arithmetic cannot wrap.
            let lead = first - x;
            let Ok(drawn) = u32::try_from(span.len()) else {
                continue;
            };
            let left_end = radius.saturating_sub(lead).min(drawn);
            let right_start = right_band.saturating_sub(lead).min(drawn).max(left_end);
            let (left, rest) = span.split_at_mut(left_end as usize);
            let (middle, right) = rest.split_at_mut((right_start - left_end) as usize);
            composite_coverage_span(left, lead..lead + left_end, local_y, w, h, radius, source);
            composite_span(middle, source);
            composite_coverage_span(
                right,
                lead + right_start..lead + drawn,
                local_y,
                w,
                h,
                radius,
                source,
            );
        }
    }

    /// Composite a vertical linear gradient over `[x, x+w) × [y, y+h)`,
    /// ramping from `top` on the rectangle's first row to `bottom` on its
    /// last.
    ///
    /// This is the shared gradient wash: the legibility gradient a
    /// full-screen surface lays over a wallpaper so its text survives a
    /// bright picture, and the soft shading a large plate carries. Both the
    /// colour and the alpha are interpolated in straight-alpha form and
    /// premultiplied per row, so a ramp that fades out keeps its hue all the
    /// way down instead of darkening as it goes.
    ///
    /// The ramp is evaluated in the rectangle's own coordinates, so a
    /// rectangle the surface bounds or the clip window cut short shows the
    /// part of the ramp that survives rather than a re-scaled one. Each row
    /// is one span fill or one blend pass, so the cost is the clipped area.
    /// A zero-size rectangle draws nothing, and a one-row rectangle is `top`.
    pub fn fill_vertical_gradient(
        &mut self,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        top: Color,
        bottom: Color,
    ) {
        if w == 0 || h == 0 {
            return;
        }
        let last = h - 1;
        for row in self.clip.rows(y, h) {
            let source = lerp_color(top, bottom, row - y, last).premultiply();
            // A premultiplied pixel of zero alpha is all zeroes, so `over`
            // would leave every destination pixel exactly as it found it.
            if source.a == 0 {
                continue;
            }
            if let Some((_, span)) = self.row_span_mut(row, x, w) {
                composite_span(span, source);
            }
        }
    }

    /// Confine the surface to the rounded rectangle `[x, x+w) × [y, y+h)`
    /// with corner `radius`: every pixel outside it becomes fully
    /// transparent, and one straddling a corner arc keeps the fraction of
    /// its alpha the arc covers.
    ///
    /// This is how *already-painted* content takes a rounded shape — the
    /// compositor's window-corner mask, or a control assembled from parts
    /// that must end up inside one rounded silhouette. Filling a rounded
    /// rectangle in a background colour over the same content cannot do it:
    /// that leaves an opaque frame where a mask leaves a transparent one.
    /// The edge comes from the same [`round_rect_coverage`] a fill uses, so a
    /// masked shape and a filled one round identically.
    ///
    /// An over-large radius is clamped to half the shorter side, so a radius
    /// of half the height yields a stadium and one of half of both yields a
    /// circle. A zero-size rectangle clears the surface, which is what
    /// confining content to nothing means.
    pub fn mask_to_round_rect(&mut self, x: u32, y: u32, w: u32, h: u32, radius: u32) {
        let radius = radius.min(w / 2).min(h / 2);
        let (width, height) = (self.width, self.height);
        let right = x.saturating_add(w).min(width);
        let bottom = y.saturating_add(h);
        for row in self.clip.rows(0, height) {
            if row < y || row >= bottom {
                self.clear_span(row, 0, width);
                continue;
            }
            self.clear_span(row, 0, x.min(width));
            self.clear_span(row, right, width - right);

            let local_y = row - y;
            if !in_corner_band(local_y, h, radius) {
                continue;
            }
            let Some((first, span)) = self.row_span_mut(row, x, w) else {
                continue;
            };
            // As in `fill_round_rect`: the drawn columns as the rectangle
            // sees them, so a clipped span still keeps the whole shape's arcs.
            let lead = first - x;
            let Ok(drawn) = u32::try_from(span.len()) else {
                continue;
            };
            let left_end = radius.saturating_sub(lead).min(drawn);
            let right_start = (w - radius).saturating_sub(lead).min(drawn).max(left_end);
            let (left, rest) = span.split_at_mut(left_end as usize);
            let (_, right_band) = rest.split_at_mut((right_start - left_end) as usize);
            mask_coverage_span(left, lead..lead + left_end, local_y, w, h, radius);
            mask_coverage_span(
                right_band,
                lead + right_start..lead + drawn,
                local_y,
                w,
                h,
                radius,
            );
        }
    }

    /// Make `[x, x+w)` of row `y` fully transparent, within the surface
    /// bounds and the active clip window.
    fn clear_span(&mut self, y: u32, x: u32, w: u32) {
        if let Some((_, span)) = self.row_span_mut(y, x, w) {
            span.fill(Pixel::TRANSPARENT);
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
        self.fill_sampled(&scaled, color);
    }

    /// Fill an anti-aliased polygon whose vertices are already in *device*
    /// sub-pixel units — [`SUBPIXEL`] per pixel, measured from this surface's
    /// own origin — instead of on a design grid stretched across the surface.
    ///
    /// This is how chrome that must stay sharp at a small pixel size is drawn.
    /// A mark only a few pixels across has no crisp rendering if its geometry
    /// works out fractional: area coverage spreads a 1.4-pixel stroke over two
    /// columns at partial alpha and it reads as a grey smear rather than a
    /// line. A caller that has grid-fitted its shape to whole pixels multiplies
    /// by [`SUBPIXEL`], and every axis-aligned edge then falls exactly on a
    /// pixel boundary — fully inside or fully outside every sample, so no
    /// fringe is produced at all — while a diagonal keeps sub-pixel placement
    /// and stays smooth.
    ///
    /// Unlike [`fill_polygon`](Self::fill_polygon) the shape is *placed*, not
    /// stretched: it is drawn where its coordinates say, so a glyph needs no
    /// square scratch surface and blit to be positioned.
    pub fn fill_polygon_subpixel(&mut self, polygon: &[(i32, i32)], color: Color) {
        if polygon.len() < 3 {
            return;
        }
        let scaled: Vec<(i64, i64)> = polygon
            .iter()
            .map(|&(x, y)| (i64::from(x), i64::from(y)))
            .collect();
        self.fill_sampled(&scaled, color);
    }

    /// Stroke the open polyline through `points` — vertices in device
    /// [`SUBPIXEL`] units — `weight` sub-pixel units wide.
    ///
    /// This is the one stroked-line path the desktop shares: a furniture
    /// glyph's diagonal and a history graph's trace are the same primitive at
    /// different scales, so neither carries its own stroke geometry.
    ///
    /// Each segment is filled as a quad offset by half the weight along *that
    /// segment's own* perpendicular, so a rising and a falling segment both
    /// draw and every segment keeps its full width whatever its slope — a fixed
    /// vertical offset would thin a steep segment away to nothing. Consecutive
    /// quads overlap at the vertex they share, which is what joins them:
    /// compositing an opaque source twice yields the same pixel, so a joint
    /// neither seams nor darkens.
    ///
    /// Fewer than two points is not a line, and a zero or negative weight is
    /// not a stroke; both draw nothing rather than guessing.
    pub fn stroke_polyline(&mut self, points: &[(i32, i32)], weight: i32, color: Color) {
        if points.len() < 2 || weight <= 0 {
            return;
        }
        let half = weight / 2;
        for pair in points.windows(2) {
            let (ax, ay) = pair[0];
            let (bx, by) = pair[1];
            let dx = bx.saturating_sub(ax);
            let dy = by.saturating_sub(ay);
            // Widened before squaring: the sum of two squared `i32`
            // components cannot overflow a `u64`, so a segment spanning a
            // large surface keeps its true length instead of saturating to a
            // shorter one and over-widening its own stroke.
            let (mx, my) = (u64::from(dx.unsigned_abs()), u64::from(dy.unsigned_abs()));
            // Coincident points are no segment, and a zero length has no
            // perpendicular to offset along.
            let Some(len) = NonZeroU64::new((mx * mx + my * my).isqrt()) else {
                continue;
            };
            // Perpendicular to (dx, dy) is (-dy, dx), scaled to the half
            // weight. Rounding, not truncating, is what keeps a hairline at its
            // full width instead of fading it toward nothing.
            let ox = -perpendicular(dy, half, len);
            let oy = perpendicular(dx, half, len);
            let quad = [
                (ax + ox, ay + oy),
                (ax - ox, ay - oy),
                (bx - ox, by - oy),
                (bx + ox, by + oy),
            ];
            self.fill_polygon_subpixel(&quad, color);
        }
    }

    /// Scan-convert `polygon`, whose vertices are in sample sub-units, and
    /// composite `color` scaled by each pixel's coverage.
    ///
    /// The one scan converter both polygon entry points share: design-grid
    /// artwork and grid-fitted device-space chrome differ only in how their
    /// vertices reach these units.
    fn fill_sampled(&mut self, scaled: &[(i64, i64)], color: Color) {
        let Some((x_start, x_end, y_start, y_end)) =
            polygon_pixel_bounds(scaled, self.width, self.height)
        else {
            return;
        };

        let source = color.premultiply();
        let samples = SUPERSAMPLE * SUPERSAMPLE;
        let span_w = x_end - x_start;
        for py in self.clip.rows(y_start, y_end - y_start) {
            let Some((first, row)) = self.row_span_mut(py, x_start, span_w) else {
                continue;
            };
            for (px, dst) in (first..).zip(row.iter_mut()) {
                let coverage = coverage_at(scaled, px, py);
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
    /// `(x, y)`, clipped to the bounds and the active clip window.
    ///
    /// Every non-transparent source pixel is blended through the
    /// premultiplied-alpha [`Pixel::over`] path, so a transparent-background
    /// sprite (a rasterised cursor or icon) lays onto the destination
    /// without a rectangular halo. A negative origin or an over-large source
    /// simply clips the off-surface part rather than panicking.
    ///
    /// The overlapping row and column ranges are resolved once, outside the
    /// row loop, and each row is then copied through paired slice iteration
    /// rather than a per-pixel bounds check and index recomputation, so the
    /// cost is the drawn overlap — not the whole source, and not the whole
    /// surface. A sprite mostly outside a narrow clip window therefore costs
    /// only the sliver that survives it.
    ///
    /// [`Pixel::over`]: crate::color::Pixel::over
    pub fn blit(&mut self, x: i32, y: i32, src: &Surface) {
        // Which of the source's columns and rows land somewhere this blit is
        // allowed to write. Resolving both once, rather than per pixel, is what
        // turns the inner loop below into a plain paired-slice walk.
        let clip = self.clip;
        let (Some(columns), Some(rows)) = (
            source_overlap(x, src.width, clip.x0, clip.x1),
            source_overlap(y, src.height, clip.y0, clip.y1),
        ) else {
            return;
        };
        let Some(destination_column) = add_offset(x, columns.start) else {
            return;
        };
        let row_len = columns.end - columns.start;

        for source_row in rows {
            let Some(destination_row) = add_offset(y, source_row) else {
                continue;
            };
            let Some(row_start) = src.row_start(source_row) else {
                continue;
            };
            let Some((first, destination)) =
                self.row_span_mut(destination_row, destination_column, row_len)
            else {
                continue;
            };
            // Pair the destination span with the source columns it actually
            // covers, so the two can never slide out of step.
            let Some(from) = columns.start.checked_add(first - destination_column) else {
                continue;
            };
            let Some(lo) = row_start.checked_add(from as usize) else {
                continue;
            };
            let Some(hi) = lo.checked_add(destination.len()) else {
                continue;
            };
            let Some(source) = src.pixels.get(lo..hi) else {
                continue;
            };
            for (pixel, dst) in source.iter().zip(destination.iter_mut()) {
                if pixel.a != 0 {
                    *dst = pixel.over(*dst);
                }
            }
        }
    }

    /// Confine every write `paint` makes to `[x, x+w) × [y, y+h)`, restoring
    /// the enclosing window before returning.
    ///
    /// This is how a view bounds what it draws to the area it owns: an item
    /// grid confines its tiles to its item area, so nothing a tile draws can
    /// mark the chrome or gutter beside it. No drawing routine has to trim its
    /// own geometry to an edge, and none can spill onto a neighbour's pixels;
    /// only the writes are withheld, so a shape that straddles the edge keeps
    /// the arcs and metrics of the whole shape.
    ///
    /// The window is *intersected* with the one already in force, so a nested
    /// paint can only ever narrow it: a control handed a clipped surface
    /// cannot widen its way back out to the area its host withheld.
    pub fn with_clip(&mut self, x: u32, y: u32, w: u32, h: u32, paint: impl FnOnce(&mut Self)) {
        let enclosing = self.clip;
        self.clip = enclosing.narrowed(x, y, w, h);
        paint(self);
        self.clip = enclosing;
    }

    /// Borrow the writable pixels of row `y` from column `x`, for at most `w`
    /// columns, with the column the returned span actually starts at. `None`
    /// when the row, or every one of those columns, lies outside the surface
    /// or the active clip window.
    ///
    /// This is the one place a write is confined: the fills, the polygon
    /// rasteriser, [`blit`](Self::blit), [`set`](Self::set), and a consumer
    /// compositing through a mask of its own — the glyph blitter in `lib/font`
    /// scaling a text colour by an 8-bit coverage bitmap — all reach pixels
    /// through here, so no primitive can honour the clip while another forgets
    /// it. A caller pays one bounds check and one index computation per row
    /// rather than per pixel.
    ///
    /// The returned start exceeds `x` when the window cut the span's leading
    /// columns; a caller pairing the span with source data of its own advances
    /// that source by the difference. The pixels stay premultiplied: this is
    /// [`set`](Self::set)'s contract at row granularity.
    #[must_use]
    pub fn row_span_mut(&mut self, y: u32, x: u32, w: u32) -> Option<(u32, &mut [Pixel])> {
        if y < self.clip.y0 || y >= self.clip.y1 {
            return None;
        }
        let columns = self.clip.columns(x, w)?;
        let start = self.row_start(y)?;
        let lo = start.checked_add(columns.start as usize)?;
        let hi = start.checked_add(columns.end as usize)?;
        Some((columns.start, self.pixels.get_mut(lo..hi)?))
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

/// Sub-pixel units per pixel in a device-space polygon
/// ([`Surface::fill_polygon_subpixel`]): the finest placement the scan
/// converter can actually resolve.
///
/// A vertex at a whole multiple of this is a pixel *boundary*. Every sample
/// centre sits at an odd sub-unit offset, so such an edge is either inside or
/// outside all of them and produces no anti-aliased fringe: a shape grid-fitted
/// to whole pixels draws exactly as sharply as a plain span fill, while
/// anything between the boundaries still resolves to eighth-pixel accuracy.
pub const SUBPIXEL: i32 = 8;

/// Sub-pixel samples per axis for anti-aliased polygon fills: half of
/// [`SUBPIXEL`], so a sample centre lands between the sub-units rather than on
/// one. A 4×4 grid gives 17 distinct coverage levels per pixel, enough for
/// smooth edges without the cost of a larger kernel.
pub const SUPERSAMPLE: u32 = SUBPIXEL.unsigned_abs() / 2;

/// One component of a stroke's half-width offset: `component * half / len`,
/// rounded to the nearest sub-unit and keeping its sign.
///
/// `len` is the segment's own length, so it is never shorter than either
/// component and the quotient never exceeds `half`.
fn perpendicular(component: i32, half: i32, len: NonZeroU64) -> i32 {
    let scaled = i64::from(component) * i64::from(half);
    let rounded = (scaled.unsigned_abs() + len.get() / 2) / len.get();
    let rounded = i32::try_from(rounded).unwrap_or(i32::MAX);
    if scaled < 0 {
        -rounded
    } else {
        rounded
    }
}

/// The source indices along one axis whose destination index `origin + i` falls
/// inside the admitted window `[lo, hi)`, intersected with the source's own
/// `extent`. `None` when nothing survives.
///
/// A blit's columns and rows are the same question asked twice, so it is
/// answered once here rather than per axis. The arithmetic is done in `i64`, so
/// a wildly negative origin or an over-large source clips instead of wrapping.
fn source_overlap(origin: i32, extent: u32, lo: u32, hi: u32) -> Option<Range<u32>> {
    let origin = i64::from(origin);
    let start = (i64::from(lo) - origin).max(0);
    let end = (i64::from(hi) - origin).min(i64::from(extent));
    if start >= end {
        return None;
    }
    Some(u32::try_from(start).ok()?..u32::try_from(end).ok()?)
}

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

/// `from` at `step` zero and `to` at `step` `last`, interpolated per channel
/// in straight-alpha form. A `last` of zero is a one-step ramp: `from`.
fn lerp_color(from: Color, to: Color, step: u32, last: u32) -> Color {
    if last == 0 {
        return from;
    }
    let lerp = |a: u8, b: u8| {
        let weighted = u32::from(a) * (last - step) + u32::from(b) * step;
        u8::try_from(weighted / last).unwrap_or(u8::MAX)
    };
    Color::rgba(
        lerp(from.r, to.r),
        lerp(from.g, to.g),
        lerp(from.b, to.b),
        lerp(from.a, to.a),
    )
}

/// Scale one corner span's alpha by each pixel's anti-aliased rounded-rect
/// coverage, so what was painted there survives only as far as the arc
/// reaches.
///
/// `columns` are the span pixels' x coordinates local to the rectangle,
/// paired one for one with `span`, and `local_y` is its row.
fn mask_coverage_span(
    span: &mut [Pixel],
    columns: Range<u32>,
    local_y: u32,
    w: u32,
    h: u32,
    radius: u32,
) {
    for (local_x, dst) in columns.zip(span.iter_mut()) {
        let coverage = round_rect_coverage(local_x, local_y, w, h, radius);
        if coverage == 255 {
            continue;
        }
        *dst = dst.scale_alpha(coverage);
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
