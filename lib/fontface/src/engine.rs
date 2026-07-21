//! The TrueType parser, outline decoder, and coverage rasteriser.

use alloc::vec;
use alloc::vec::Vec;

use crate::mathf;
use crate::FontError;

/// Coverage sample rows per pixel row (vertical supersampling).
const SAMPLE_ROWS: u32 = 4;

/// Line segments each quadratic Bézier is flattened into.
///
/// Eight chords keep the flattening error well under a tenth of a pixel at the
/// atlas's native size, invisible at 16 coverage levels.
const QUAD_SEGMENTS: u32 = 8;

const fn err(what: &'static str) -> FontError {
    FontError::new(what)
}

/// Big-endian field reads over the raw font bytes, each bounds-checked.
struct Reader<'a> {
    data: &'a [u8],
}

impl Reader<'_> {
    fn u8(&self, at: usize) -> Result<u8, FontError> {
        self.data.get(at).copied().ok_or(err("truncated u8"))
    }

    fn i8(&self, at: usize) -> Result<i8, FontError> {
        Ok(i8::from_be_bytes([self.u8(at)?]))
    }

    fn u16(&self, at: usize) -> Result<u16, FontError> {
        let b = self.data.get(at..at + 2).ok_or(err("truncated u16"))?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn i16(&self, at: usize) -> Result<i16, FontError> {
        let b = self.data.get(at..at + 2).ok_or(err("truncated i16"))?;
        Ok(i16::from_be_bytes([b[0], b[1]]))
    }

    fn u32(&self, at: usize) -> Result<u32, FontError> {
        let b = self.data.get(at..at + 4).ok_or(err("truncated u32"))?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
}

/// Offsets of the tables the engine needs, from the table directory.
struct Tables {
    head: usize,
    maxp: usize,
    cmap: usize,
    hhea: usize,
    hmtx: usize,
    loca: usize,
    glyf: usize,
}

impl Tables {
    fn locate(r: &Reader<'_>) -> Result<Self, FontError> {
        let count = r.u16(4)? as usize;
        let mut head = None;
        let mut maxp = None;
        let mut cmap = None;
        let mut hhea = None;
        let mut hmtx = None;
        let mut loca = None;
        let mut glyf = None;
        for i in 0..count {
            let entry = 12 + 16 * i;
            let tag = [
                r.u8(entry)?,
                r.u8(entry + 1)?,
                r.u8(entry + 2)?,
                r.u8(entry + 3)?,
            ];
            let offset = r.u32(entry + 8)? as usize;
            match &tag {
                b"head" => head = Some(offset),
                b"maxp" => maxp = Some(offset),
                b"cmap" => cmap = Some(offset),
                b"hhea" => hhea = Some(offset),
                b"hmtx" => hmtx = Some(offset),
                b"loca" => loca = Some(offset),
                b"glyf" => glyf = Some(offset),
                _ => {}
            }
        }
        Ok(Self {
            head: head.ok_or(err("missing head table"))?,
            maxp: maxp.ok_or(err("missing maxp table"))?,
            cmap: cmap.ok_or(err("missing cmap table"))?,
            hhea: hhea.ok_or(err("missing hhea table"))?,
            hmtx: hmtx.ok_or(err("missing hmtx table"))?,
            loca: loca.ok_or(err("missing loca table"))?,
            glyf: glyf.ok_or(err("missing glyf table"))?,
        })
    }
}

/// A parsed TrueType face: the tables and metrics the rasteriser needs, plus
/// the sorted `(codepoint, glyph)` pairs from its `cmap`.
pub struct Face<'a> {
    r: Reader<'a>,
    tables: Tables,
    units_per_em: i32,
    ascent: i32,
    descent: i32,
    glyph_count: u16,
    advance_count: u16,
    long_loca: bool,
    mapped: Vec<(u32, u16)>,
}

impl<'a> Face<'a> {
    /// Parse `data` (a TrueType `glyf`-outline face) into its tables and
    /// codepoint map, failing closed on any malformed or unsupported table.
    ///
    /// # Errors
    ///
    /// Returns a [`FontError`] when a required table is missing, a field is
    /// truncated, the vertical metrics are non-positive, or the `cmap` has no
    /// format-4 subtable.
    pub fn parse(data: &'a [u8]) -> Result<Self, FontError> {
        let r = Reader { data };
        let tables = Tables::locate(&r)?;
        let units_per_em = i32::from(r.u16(tables.head + 18)?);
        if units_per_em == 0 {
            return Err(err("unitsPerEm is zero"));
        }
        let long_loca = match r.i16(tables.head + 50)? {
            0 => false,
            1 => true,
            _ => return Err(err("unknown indexToLocFormat")),
        };
        let ascent = i32::from(r.i16(tables.hhea + 4)?);
        let descent = -i32::from(r.i16(tables.hhea + 6)?);
        if ascent <= 0 || descent < 0 {
            return Err(err("non-positive vertical metrics"));
        }
        let advance_count = r.u16(tables.hhea + 34)?;
        if advance_count == 0 {
            return Err(err("numberOfHMetrics is zero"));
        }
        let glyph_count = r.u16(tables.maxp + 4)?;
        let mapped = parse_cmap_format4(&r, tables.cmap)?;
        Ok(Self {
            r,
            tables,
            units_per_em,
            ascent,
            descent,
            glyph_count,
            advance_count,
            long_loca,
            mapped,
        })
    }

    /// Font units per em.
    #[must_use]
    pub fn units_per_em(&self) -> i32 {
        self.units_per_em
    }

    /// Ascender in font units.
    #[must_use]
    pub fn ascent(&self) -> i32 {
        self.ascent
    }

    /// Descender magnitude in font units (positive).
    #[must_use]
    pub fn descent(&self) -> i32 {
        self.descent
    }

    /// The sorted `(codepoint, glyph)` pairs the face's `cmap` maps.
    #[must_use]
    pub fn mapped(&self) -> &[(u32, u16)] {
        &self.mapped
    }

    /// The glyph the face maps `code` to, or `None` when it maps no such
    /// spacing glyph.
    #[must_use]
    pub fn glyph_for(&self, code: u32) -> Option<u16> {
        let index = self.mapped.binary_search_by(|&(c, _)| c.cmp(&code)).ok()?;
        Some(self.mapped[index].1)
    }

    /// The advance width of `glyph` in font units.
    fn advance(&self, glyph: u16) -> Result<u16, FontError> {
        let index = glyph.min(self.advance_count - 1);
        self.r.u16(self.tables.hmtx + 4 * usize::from(index))
    }

    /// The one advance width every mapped spacing glyph shares, in font units.
    ///
    /// The cell grid only works over a strictly monospace face: every mapped
    /// glyph must advance by this uniform width or by zero (a combining mark).
    /// A face mixing two spacing widths fails closed here.
    ///
    /// # Errors
    ///
    /// Returns a [`FontError`] when the face maps no spacing glyph or is not
    /// strictly monospace.
    pub fn uniform_advance(&self) -> Result<u16, FontError> {
        let mut uniform = None;
        for &(_, glyph) in &self.mapped {
            let advance = self.advance(glyph)?;
            if advance == 0 {
                continue;
            }
            match uniform {
                None => uniform = Some(advance),
                Some(seen) if seen == advance => {}
                Some(_) => return Err(err("face is not strictly monospace")),
            }
        }
        uniform.ok_or(err("face maps no spacing glyphs"))
    }

    /// The `glyf` byte range of `glyph`, or `None` for an empty outline.
    fn glyf_range(&self, glyph: u16) -> Result<Option<(usize, usize)>, FontError> {
        if glyph >= self.glyph_count {
            return Err(err("glyph id out of range"));
        }
        let i = usize::from(glyph);
        let (start, end) = if self.long_loca {
            (
                self.r.u32(self.tables.loca + 4 * i)? as usize,
                self.r.u32(self.tables.loca + 4 * i + 4)? as usize,
            )
        } else {
            (
                usize::from(self.r.u16(self.tables.loca + 2 * i)?) * 2,
                usize::from(self.r.u16(self.tables.loca + 2 * i + 2)?) * 2,
            )
        };
        if end < start {
            return Err(err("loca offsets not monotonic"));
        }
        Ok((start != end).then_some((self.tables.glyf + start, self.tables.glyf + end)))
    }

    /// Rasterise `glyph` into `bitmap_width × geometry.height` bytes of 4-bit
    /// (`0..=15`) coverage, row-major, one value per byte.
    ///
    /// `px_per_em` is the pixels-per-em the outline is scaled by (font units
    /// scale by `px_per_em / units_per_em`); `geometry.baseline` positions the
    /// baseline within the cell. An empty outline yields all-transparent
    /// coverage.
    ///
    /// # Errors
    ///
    /// Returns a [`FontError`] on a malformed outline (bad indices, cyclic
    /// composite, unsupported composite flags).
    pub fn rasterise_glyph(
        &self,
        glyph: u16,
        geometry: &CellGeometry,
        px_per_em: f64,
        bitmap_width: u32,
    ) -> Result<Vec<u8>, FontError> {
        let mut sink = OutlineSink {
            segments: Vec::new(),
            scale: px_per_em / f64::from(self.units_per_em),
            origin_x: 0.0,
            baseline_y: f64::from(geometry.baseline),
            transform: Affine::IDENTITY,
        };
        outline_glyph(self, glyph, &mut sink, 0)?;
        Ok(rasterise(&sink.segments, geometry, bitmap_width))
    }
}

/// Decode the first format-4 (BMP segment-mapped) `cmap` subtable into a
/// sorted `(codepoint, glyph)` list.
fn parse_cmap_format4(r: &Reader<'_>, cmap: usize) -> Result<Vec<(u32, u16)>, FontError> {
    let subtables = r.u16(cmap + 2)?;
    let mut format4 = None;
    for i in 0..usize::from(subtables) {
        let offset = r.u32(cmap + 8 + 8 * i)? as usize;
        if r.u16(cmap + offset)? == 4 {
            format4 = Some(cmap + offset);
            break;
        }
    }
    let sub = format4.ok_or(err("no format-4 cmap subtable"))?;
    let seg_count = usize::from(r.u16(sub + 6)?) / 2;
    if seg_count == 0 {
        return Err(err("empty cmap"));
    }
    let ends = sub + 14;
    let starts = ends + 2 * seg_count + 2;
    let deltas = starts + 2 * seg_count;
    let range_offsets = deltas + 2 * seg_count;
    let mut mapped = Vec::new();
    for s in 0..seg_count {
        let end = u32::from(r.u16(ends + 2 * s)?);
        let start = u32::from(r.u16(starts + 2 * s)?);
        let delta = r.u16(deltas + 2 * s)?;
        let range_offset = r.u16(range_offsets + 2 * s)?;
        if start > end {
            return Err(err("cmap segment start exceeds end"));
        }
        for code in start..=end {
            if code == 0xFFFF {
                continue;
            }
            let glyph = if range_offset == 0 {
                let code = u16::try_from(code).map_err(|_| err("cmap code beyond BMP"))?;
                code.wrapping_add(delta)
            } else {
                let at =
                    range_offsets + 2 * s + usize::from(range_offset) + 2 * (code - start) as usize;
                let indirect = r.u16(at)?;
                if indirect == 0 {
                    continue;
                }
                indirect.wrapping_add(delta)
            };
            if glyph != 0 {
                mapped.push((code, glyph));
            }
        }
    }
    mapped.sort_unstable();
    Ok(mapped)
}

/// One straight outline segment in pixel space (y grows downward).
#[derive(Copy, Clone, Debug)]
struct Segment {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
}

/// A font-unit affine transform: maps `(x, y)` to
/// `(a·x + c·y + dx, b·x + d·y + dy)` — the composite-component transform
/// shape the `glyf` spec defines.
#[derive(Copy, Clone)]
struct Affine {
    a: f64,
    b: f64,
    c: f64,
    d: f64,
    dx: f64,
    dy: f64,
}

impl Affine {
    const IDENTITY: Self = Self {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        dx: 0.0,
        dy: 0.0,
    };

    fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        (
            self.a * x + self.c * y + self.dx,
            self.b * x + self.d * y + self.dy,
        )
    }

    /// The transform applying `inner` first, then `self`.
    fn then(&self, inner: &Self) -> Self {
        Self {
            a: self.a * inner.a + self.c * inner.b,
            b: self.b * inner.a + self.d * inner.b,
            c: self.a * inner.c + self.c * inner.d,
            d: self.b * inner.c + self.d * inner.d,
            dx: self.a * inner.dx + self.c * inner.dy + self.dx,
            dy: self.b * inner.dx + self.d * inner.dy + self.dy,
        }
    }
}

/// Collects an outline as segments, flattening quadratics and applying the
/// font-unit → pixel transform (including the composite component transform).
struct OutlineSink {
    segments: Vec<Segment>,
    scale: f64,
    origin_x: f64,
    baseline_y: f64,
    transform: Affine,
}

impl OutlineSink {
    fn to_px(&self, x: f64, y: f64) -> (f64, f64) {
        let (fx, fy) = self.transform.apply(x, y);
        (
            self.origin_x + fx * self.scale,
            self.baseline_y - fy * self.scale,
        )
    }

    fn line(&mut self, x0: f64, y0: f64, x1: f64, y1: f64) {
        let (px0, py0) = self.to_px(x0, y0);
        let (px1, py1) = self.to_px(x1, y1);
        // An exactly horizontal segment can never cross a sample row, so it
        // contributes nothing to the winding fill and is dropped.
        if py0.total_cmp(&py1) != core::cmp::Ordering::Equal {
            self.segments.push(Segment {
                x0: px0,
                y0: py0,
                x1: px1,
                y1: py1,
            });
        }
    }

    /// Flatten the quadratic Bézier into [`QUAD_SEGMENTS`] chords.
    fn quad(&mut self, x0: f64, y0: f64, cx: f64, cy: f64, x1: f64, y1: f64) {
        let mut px = x0;
        let mut py = y0;
        for i in 1..=QUAD_SEGMENTS {
            let t = f64::from(i) / f64::from(QUAD_SEGMENTS);
            let u = 1.0 - t;
            let nx = u * u * x0 + 2.0 * u * t * cx + t * t * x1;
            let ny = u * u * y0 + 2.0 * u * t * cy + t * t * y1;
            self.line(px, py, nx, ny);
            px = nx;
            py = ny;
        }
    }
}

/// One decoded point of a simple glyph outline.
#[derive(Copy, Clone)]
struct Point {
    x: f64,
    y: f64,
    on_curve: bool,
}

/// Append `glyph`'s outline segments to `sink`, recursing through composite
/// glyphs. `depth` bounds the recursion so a malformed cyclic composite fails
/// closed instead of overflowing the stack.
fn outline_glyph(
    face: &Face<'_>,
    glyph: u16,
    sink: &mut OutlineSink,
    depth: u32,
) -> Result<(), FontError> {
    if depth > 4 {
        return Err(err("composite glyph nesting deeper than 4"));
    }
    let Some((start, end)) = face.glyf_range(glyph)? else {
        return Ok(());
    };
    let r = &face.r;
    let contour_count = r.i16(start)?;
    if contour_count >= 0 {
        outline_simple(
            r,
            start,
            end,
            usize::from(contour_count.unsigned_abs()),
            sink,
        )
    } else {
        outline_composite(face, start, sink, depth)
    }
}

/// Decode a simple glyph's contours into `sink`.
fn outline_simple(
    r: &Reader<'_>,
    start: usize,
    end: usize,
    contour_count: usize,
    sink: &mut OutlineSink,
) -> Result<(), FontError> {
    let mut at = start + 10;
    let mut contour_ends = Vec::with_capacity(contour_count);
    for _ in 0..contour_count {
        contour_ends.push(r.u16(at)?);
        at += 2;
    }
    let point_count = match contour_ends.last() {
        Some(&last) => usize::from(last) + 1,
        None => return Ok(()),
    };
    let instruction_len = usize::from(r.u16(at)?);
    at += 2 + instruction_len;

    // Flags, with the repeat shorthand expanded.
    let mut flags = Vec::with_capacity(point_count);
    while flags.len() < point_count {
        let flag = r.u8(at)?;
        at += 1;
        flags.push(flag);
        if flag & 0x08 != 0 {
            let repeats = r.u8(at)?;
            at += 1;
            for _ in 0..repeats {
                if flags.len() == point_count {
                    return Err(err("flag repeat overruns point count"));
                }
                flags.push(flag);
            }
        }
    }

    // X deltas, then Y deltas, each cumulative.
    let mut xs = Vec::with_capacity(point_count);
    let mut x = 0i32;
    for &flag in &flags {
        if flag & 0x02 != 0 {
            let d = i32::from(r.u8(at)?);
            at += 1;
            x += if flag & 0x10 != 0 { d } else { -d };
        } else if flag & 0x10 == 0 {
            x += i32::from(r.i16(at)?);
            at += 2;
        }
        xs.push(x);
    }
    let mut ys = Vec::with_capacity(point_count);
    let mut y = 0i32;
    for &flag in &flags {
        if flag & 0x04 != 0 {
            let d = i32::from(r.u8(at)?);
            at += 1;
            y += if flag & 0x20 != 0 { d } else { -d };
        } else if flag & 0x20 == 0 {
            y += i32::from(r.i16(at)?);
            at += 2;
        }
        ys.push(y);
    }
    if at > end {
        return Err(err("simple glyph overruns its loca range"));
    }

    let mut first = 0usize;
    for &contour_end in &contour_ends {
        let last = usize::from(contour_end);
        if last < first || last >= point_count {
            return Err(err("contour end indices not monotonic"));
        }
        let points: Vec<Point> = (first..=last)
            .map(|i| Point {
                x: f64::from(xs[i]),
                y: f64::from(ys[i]),
                on_curve: flags[i] & 0x01 != 0,
            })
            .collect();
        emit_contour(&points, sink);
        first = last + 1;
    }
    Ok(())
}

/// Emit one closed contour of on/off-curve points as lines and quadratics,
/// synthesising the implied on-curve midpoints between consecutive off-curve
/// points per the TrueType outline rules.
fn emit_contour(points: &[Point], sink: &mut OutlineSink) {
    if points.is_empty() {
        return;
    }
    // Establish the starting on-curve point per the TrueType rules: the first
    // on-curve point, or — when every point is off-curve — the implied
    // midpoint between the last and first points.
    let (start, offset) = if let Some(i) = points.iter().position(|p| p.on_curve) {
        (points[i], i)
    } else {
        let a = points[points.len() - 1];
        let b = points[0];
        let mid = Point {
            x: (a.x + b.x) / 2.0,
            y: (a.y + b.y) / 2.0,
            on_curve: true,
        };
        (mid, points.len() - 1)
    };
    let mut current = start;
    let mut pending_control: Option<Point> = None;
    for k in 1..=points.len() {
        let p = points[(offset + k) % points.len()];
        match (p.on_curve, pending_control) {
            (true, None) => {
                sink.line(current.x, current.y, p.x, p.y);
                current = p;
            }
            (true, Some(c)) => {
                sink.quad(current.x, current.y, c.x, c.y, p.x, p.y);
                current = p;
                pending_control = None;
            }
            (false, None) => pending_control = Some(p),
            (false, Some(c)) => {
                let mid = Point {
                    x: (c.x + p.x) / 2.0,
                    y: (c.y + p.y) / 2.0,
                    on_curve: true,
                };
                sink.quad(current.x, current.y, c.x, c.y, mid.x, mid.y);
                current = mid;
                pending_control = Some(p);
            }
        }
    }
    // Close the contour, through a trailing control point if one is pending.
    if let Some(c) = pending_control {
        sink.quad(current.x, current.y, c.x, c.y, start.x, start.y);
    } else {
        let closed = current.x.total_cmp(&start.x) == core::cmp::Ordering::Equal
            && current.y.total_cmp(&start.y) == core::cmp::Ordering::Equal;
        if !closed {
            sink.line(current.x, current.y, start.x, start.y);
        }
    }
}

/// Decode a composite glyph: recurse into each component with its affine
/// transform composed onto the sink.
fn outline_composite(
    face: &Face<'_>,
    start: usize,
    sink: &mut OutlineSink,
    depth: u32,
) -> Result<(), FontError> {
    let r = &face.r;
    let mut at = start + 10;
    loop {
        let flags = r.u16(at)?;
        let component = r.u16(at + 2)?;
        at += 4;
        let args_are_words = flags & 0x0001 != 0;
        let args_are_xy = flags & 0x0002 != 0;
        if !args_are_xy {
            return Err(err("composite point-matching args unsupported"));
        }
        // SCALED_COMPONENT_OFFSET (the Apple interpretation) would rescale the
        // offset by the component transform; the accepted faces use plain
        // font-unit offsets, so anything else is a wrong glyph waiting to
        // happen.
        if flags & 0x0800 != 0 {
            return Err(err("composite scaled component offset unsupported"));
        }
        let (dx, dy) = if args_are_words {
            let v = (f64::from(r.i16(at)?), f64::from(r.i16(at + 2)?));
            at += 4;
            v
        } else {
            let v = (f64::from(r.i8(at)?), f64::from(r.i8(at + 1)?));
            at += 2;
            v
        };
        // The component's scale terms are F2Dot14 fixed-point values.
        let f2dot14 = |at_ref: &mut usize| -> Result<f64, FontError> {
            let v = f64::from(r.i16(*at_ref)?) / 16384.0;
            *at_ref += 2;
            Ok(v)
        };
        let (x_scale, scale01, scale10, y_scale) = if flags & 0x0008 != 0 {
            let scale = f2dot14(&mut at)?;
            (scale, 0.0, 0.0, scale)
        } else if flags & 0x0040 != 0 {
            let x_scale = f2dot14(&mut at)?;
            let y_scale = f2dot14(&mut at)?;
            (x_scale, 0.0, 0.0, y_scale)
        } else if flags & 0x0080 != 0 {
            let x_scale = f2dot14(&mut at)?;
            let scale01 = f2dot14(&mut at)?;
            let scale10 = f2dot14(&mut at)?;
            let y_scale = f2dot14(&mut at)?;
            (x_scale, scale01, scale10, y_scale)
        } else {
            (1.0, 0.0, 0.0, 1.0)
        };
        let saved = sink.transform;
        sink.transform = saved.then(&Affine {
            a: x_scale,
            b: scale01,
            c: scale10,
            d: y_scale,
            dx,
            dy,
        });
        outline_glyph(face, component, sink, depth + 1)?;
        sink.transform = saved;
        if flags & 0x0020 == 0 {
            break;
        }
    }
    Ok(())
}

/// The pixel geometry a glyph is rasterised into: the cell width in pixels, the
/// cell height in pixels, and the baseline row (pixel rows above the baseline).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CellGeometry {
    /// Cell width in pixels.
    pub width: u32,
    /// Cell height in pixels.
    pub height: u32,
    /// Baseline row: pixel rows above the baseline.
    pub baseline: u32,
}

impl CellGeometry {
    /// Derive the cell geometry a face rasterises at, given its uniform
    /// `advance` (font units) and an integral `em_px` pixels-per-em.
    ///
    /// The width is the advance rounded half-up to a whole pixel; the baseline
    /// and descent are exact integer ceilings, so the metrics are reproducible
    /// bit-for-bit — this is what the generated atlas is built from.
    ///
    /// # Errors
    ///
    /// Returns a [`FontError`] when a metric overflows a `u32` or the derived
    /// cell is implausibly large.
    pub fn derive(face: &Face<'_>, advance: u16, em_px: u32) -> Result<Self, FontError> {
        let units = i64::from(face.units_per_em);
        let width_px = (i64::from(advance) * i64::from(em_px) + units / 2).div_euclid(units);
        let width = u32::try_from(width_px).map_err(|_| err("advance overflows the cell width"))?;
        let ceil_px = |value: i32| -> Result<u32, FontError> {
            let px = (i64::from(value) * i64::from(em_px) + units - 1).div_euclid(units);
            u32::try_from(px).map_err(|_| err("negative vertical metric"))
        };
        let baseline = ceil_px(face.ascent)?;
        let descent_rows = ceil_px(face.descent)?;
        let height = baseline + descent_rows;
        if width == 0 || height == 0 || height > 64 {
            return Err(err("implausible cell geometry"));
        }
        Ok(Self {
            width,
            height,
            baseline,
        })
    }
}

/// Rasterise `segments` into `width × geometry.height` bytes of 4-bit coverage
/// (`0..=15`), row-major, one value per byte.
///
/// Non-zero winding fill: each of the [`SAMPLE_ROWS`] sample rows per pixel row
/// contributes `1 / SAMPLE_ROWS` of a pixel's coverage, distributed over the
/// pixels its filled spans cross with exact fractional x extents.
fn rasterise(segments: &[Segment], geometry: &CellGeometry, width: u32) -> Vec<u8> {
    let width = width as usize;
    let mut coverage = vec![0f64; width * geometry.height as usize];
    let row_weight = 1.0 / f64::from(SAMPLE_ROWS);
    let mut crossings: Vec<(f64, i32)> = Vec::new();
    for row in 0..geometry.height {
        for sub in 0..SAMPLE_ROWS {
            let y = f64::from(row) + (f64::from(sub) + 0.5) / f64::from(SAMPLE_ROWS);
            crossings.clear();
            for seg in segments {
                let (top, bottom, x_at_top, x_at_bottom, winding) = if seg.y0 < seg.y1 {
                    (seg.y0, seg.y1, seg.x0, seg.x1, 1)
                } else {
                    (seg.y1, seg.y0, seg.x1, seg.x0, -1)
                };
                // Half-open [top, bottom) so a vertex shared by two segments is
                // counted exactly once.
                if y >= top && y < bottom {
                    let t = (y - top) / (bottom - top);
                    crossings.push((x_at_top + t * (x_at_bottom - x_at_top), winding));
                }
            }
            crossings.sort_by(|a, b| a.0.total_cmp(&b.0));
            let mut winding = 0;
            let mut span_start = 0.0;
            for &(x, w) in &crossings {
                if winding == 0 && w != 0 {
                    span_start = x;
                }
                let was = winding;
                winding += w;
                if was != 0 && winding == 0 {
                    let start = row as usize * width;
                    add_span(
                        &mut coverage[start..start + width],
                        span_start,
                        x,
                        row_weight,
                    );
                }
            }
        }
    }
    coverage.iter().map(|&c| quantise(c)).collect()
}

/// Quantise an accumulated coverage fraction to a 4-bit level (`0..=15`).
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the clamp bounds the value to 0..=15 before the cast, so the \
              truncation and sign loss the lints warn about cannot occur"
)]
fn quantise(coverage: f64) -> u8 {
    mathf::clamp(mathf::floor(coverage * 15.0 + 0.5), 0.0, 15.0) as u8
}

/// Add `weight × horizontal-overlap` coverage for the span `[x0, x1)` to one
/// pixel row, clipping to the cell.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "the span is clamped to [0, row.len()] (≤ 64 pixels) before the \
              float↔index conversions, so none of them can truncate, go \
              negative, or lose precision"
)]
fn add_span(row: &mut [f64], x0: f64, x1: f64, weight: f64) {
    let width = row.len() as f64;
    let x0 = mathf::fmax(x0, 0.0);
    let x1 = mathf::fmin(x1, width);
    if x0 >= x1 {
        return;
    }
    let first = mathf::floor(x0) as usize;
    let last = (mathf::ceil(x1) as usize).min(row.len());
    for (i, cell) in row.iter_mut().enumerate().take(last).skip(first) {
        let left = mathf::fmax(i as f64, x0);
        let right = mathf::fmin((i + 1) as f64, x1);
        if right > left {
            *cell += weight * (right - left);
        }
    }
}
