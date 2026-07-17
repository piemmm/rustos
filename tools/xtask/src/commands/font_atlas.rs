//! `cargo xtask font-atlas` implementation.
//!
//! The system text family combines Inconsolata EX (Latin, Greek, Cyrillic),
//! M PLUS 1 Code Regular (Japanese), and Noto Sans Hebrew `ExtraCondensed`, all
//! under SIL OFL 1.1 and committed as TrueType sources under `lib/font/assets/`.
//! Faces have precedence in that order and each companion fills only codepoints
//! earlier faces do not map. The renderers never parse TrueType at runtime:
//! this command rasterises the merged repertoire into a fixed-cell,
//! 4-bit-coverage bitmap atlas and emits it as
//! generated data (`lib/font/src/atlas.rs` + `lib/font/src/atlas_coverage.bin`)
//! that `lib/font` compiles in. With no arguments the command verifies the
//! committed atlas matches a fresh generation (the `ci` drift guard, exactly
//! like `c-header`); `--write` regenerates it.
//!
//! The generator is deliberately first-party and deterministic: a minimal
//! TrueType reader (`head`/`maxp`/`cmap` format 4/`hhea`/`hmtx`/`loca`/`glyf`)
//! and a scanline rasteriser (quadratic outlines flattened to segments,
//! non-zero winding, 4 sample rows per pixel row with exact horizontal span
//! coverage). Identical input bytes produce identical output bytes on every
//! host, so the drift guard is meaningful.
//!
//! Pixel geometry: the em square is [`EM_PX`] pixels tall. Inconsolata is
//! strictly monospace: every spacing glyph advances by one uniform width the
//! generator reads from the face itself, and that advance defines the terminal
//! cell. Every bitmap slot is two cells wide so Japanese full-width outlines
//! can cover the continuation cell reserved by `lib/vt`; narrow outlines leave
//! the second cell transparent. Cell height and baseline derive from the
//! primary face. Zero-advance combining marks rasterise like
//! any other glyph — the face draws their outlines inside the advance-wide
//! cell (GPOS anchor repositioning is a shaping concern the cell grid
//! deliberately does not have), so each mark lands in its own cell: the same
//! deliberate one-scalar-per-cell model `lib/vt` / `lib/curses` document.

use std::fmt::Write as _;
use std::path::Path;

/// Workspace-relative path of the committed TrueType source.
pub const DEFAULT_TTF_PATH: &str = "lib/font/assets/Inconsolata-EX.ttf";
/// Workspace-relative path of the committed Japanese companion face.
pub const JAPANESE_TTF_PATH: &str = "lib/font/assets/MPLUS1Code-Regular.ttf";
/// Workspace-relative path of the committed Hebrew companion face.
pub const HEBREW_TTF_PATH: &str = "lib/font/assets/NotoSansHebrew-ExtraCondensed.ttf";
/// Workspace-relative path of the generated Rust atlas view.
pub const DEFAULT_ATLAS_RS_PATH: &str = "lib/font/src/atlas.rs";
/// Workspace-relative path of the generated coverage payload.
pub const DEFAULT_ATLAS_BIN_PATH: &str = "lib/font/src/atlas_coverage.bin";

/// Pixels per em square: the one size the atlas is rasterised at.
///
/// 25 px/em puts Inconsolata EX's uniform 613/1024-em advance at 14.97 px,
/// so the rounded 15-pixel cell distorts the glyph grid by only 0.2% (a
/// 24 px em would round 14.37 px down to 14, squeezing every glyph by 2.6%).
/// The resulting 15×28 cell (ascent 23, descent 5) is large enough that
/// unhinted anti-aliased strokes stay crisp, small enough that a 1080p
/// console keeps 38 rows at scale 1.
const EM_PX: u32 = 25;

/// Maximum terminal cells occupied by one generated glyph bitmap.
const MAX_GLYPH_CELLS: u32 = 2;

/// Coverage sample rows per pixel row (vertical supersampling).
const SAMPLE_ROWS: u32 = 4;

/// Line segments each quadratic Bézier is flattened into.
///
/// At a 25 px em the largest curve spans roughly half the cell; 8 chords keep
/// the flattening error well under a tenth of a pixel, invisible at 16
/// coverage levels.
const QUAD_SEGMENTS: u32 = 8;

/// A parse failure in the committed TrueType file.
///
/// The committed font is trusted repository data, but the reader still fails
/// closed on anything malformed or unsupported rather than emitting a wrong
/// atlas.
fn parse_err(what: &str) -> String {
    format!("font-atlas: malformed or unsupported TrueType data: {what}")
}

/// Big-endian field reads over the raw font bytes, each bounds-checked.
struct Reader<'a> {
    data: &'a [u8],
}

impl Reader<'_> {
    fn u8(&self, at: usize) -> Result<u8, String> {
        self.data
            .get(at)
            .copied()
            .ok_or_else(|| parse_err("truncated u8"))
    }

    fn i8(&self, at: usize) -> Result<i8, String> {
        Ok(i8::from_be_bytes([self.u8(at)?]))
    }

    fn u16(&self, at: usize) -> Result<u16, String> {
        let b = self
            .data
            .get(at..at + 2)
            .ok_or_else(|| parse_err("truncated u16"))?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn i16(&self, at: usize) -> Result<i16, String> {
        let b = self
            .data
            .get(at..at + 2)
            .ok_or_else(|| parse_err("truncated i16"))?;
        Ok(i16::from_be_bytes([b[0], b[1]]))
    }

    fn u32(&self, at: usize) -> Result<u32, String> {
        let b = self
            .data
            .get(at..at + 4)
            .ok_or_else(|| parse_err("truncated u32"))?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
}

/// Offsets of the tables the generator needs, from the table directory.
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
    fn locate(r: &Reader<'_>) -> Result<Self, String> {
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
            head: head.ok_or_else(|| parse_err("missing head table"))?,
            maxp: maxp.ok_or_else(|| parse_err("missing maxp table"))?,
            cmap: cmap.ok_or_else(|| parse_err("missing cmap table"))?,
            hhea: hhea.ok_or_else(|| parse_err("missing hhea table"))?,
            hmtx: hmtx.ok_or_else(|| parse_err("missing hmtx table"))?,
            loca: loca.ok_or_else(|| parse_err("missing loca table"))?,
            glyf: glyf.ok_or_else(|| parse_err("missing glyf table"))?,
        })
    }
}

/// The parsed face: everything the rasteriser needs, in font units.
struct Face<'a> {
    r: Reader<'a>,
    tables: Tables,
    /// Font units per em.
    units_per_em: i32,
    /// Ascender in font units (from `hhea`).
    ascent: i32,
    /// Descender magnitude in font units (positive; `hhea` stores it negative).
    descent: i32,
    /// Glyph count (from `maxp`).
    glyph_count: u16,
    /// Advance-width count (from `hhea`); trailing glyphs repeat the last one.
    advance_count: u16,
    /// Whether `loca` holds 32-bit offsets (`head.indexToLocFormat == 1`).
    long_loca: bool,
    /// Sorted `(codepoint, glyph)` pairs from the `cmap`.
    mapped: Vec<(u32, u16)>,
}

impl<'a> Face<'a> {
    fn parse(data: &'a [u8]) -> Result<Self, String> {
        let r = Reader { data };
        let tables = Tables::locate(&r)?;
        let units_per_em = i32::from(r.u16(tables.head + 18)?);
        if units_per_em == 0 {
            return Err(parse_err("unitsPerEm is zero"));
        }
        let long_loca = match r.i16(tables.head + 50)? {
            0 => false,
            1 => true,
            _ => return Err(parse_err("unknown indexToLocFormat")),
        };
        let ascent = i32::from(r.i16(tables.hhea + 4)?);
        let descent = -i32::from(r.i16(tables.hhea + 6)?);
        if ascent <= 0 || descent < 0 {
            return Err(parse_err("non-positive vertical metrics"));
        }
        let advance_count = r.u16(tables.hhea + 34)?;
        if advance_count == 0 {
            return Err(parse_err("numberOfHMetrics is zero"));
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

    /// The advance width of `glyph` in font units.
    fn advance(&self, glyph: u16) -> Result<u16, String> {
        let index = glyph.min(self.advance_count - 1);
        self.r.u16(self.tables.hmtx + 4 * usize::from(index))
    }

    /// The one advance width every mapped spacing glyph shares, in font
    /// units.
    ///
    /// The cell grid only works over a strictly monospace face: every mapped
    /// glyph must advance by this uniform width or by zero (a combining
    /// mark). A face mixing two spacing widths would silently break the
    /// grid, so it fails closed here instead.
    fn uniform_advance(&self) -> Result<u16, String> {
        let mut uniform = None;
        for &(_, glyph) in &self.mapped {
            let advance = self.advance(glyph)?;
            if advance == 0 {
                continue;
            }
            match uniform {
                None => uniform = Some(advance),
                Some(seen) if seen == advance => {}
                Some(_) => return Err(parse_err("face is not strictly monospace")),
            }
        }
        uniform.ok_or_else(|| parse_err("face maps no spacing glyphs"))
    }

    /// The `glyf` byte range of `glyph`, or `None` for an empty outline.
    fn glyf_range(&self, glyph: u16) -> Result<Option<(usize, usize)>, String> {
        if glyph >= self.glyph_count {
            return Err(parse_err("glyph id out of range"));
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
            return Err(parse_err("loca offsets not monotonic"));
        }
        Ok((start != end).then_some((self.tables.glyf + start, self.tables.glyf + end)))
    }
}

/// Decode the first format-4 (BMP segment-mapped) `cmap` subtable into a
/// sorted `(codepoint, glyph)` list.
fn parse_cmap_format4(r: &Reader<'_>, cmap: usize) -> Result<Vec<(u32, u16)>, String> {
    let subtables = r.u16(cmap + 2)?;
    let mut format4 = None;
    for i in 0..usize::from(subtables) {
        let offset = r.u32(cmap + 8 + 8 * i)? as usize;
        if r.u16(cmap + offset)? == 4 {
            format4 = Some(cmap + offset);
            break;
        }
    }
    let sub = format4.ok_or_else(|| parse_err("no format-4 cmap subtable"))?;
    let seg_count = usize::from(r.u16(sub + 6)?) / 2;
    if seg_count == 0 {
        return Err(parse_err("empty cmap"));
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
            return Err(parse_err("cmap segment start exceeds end"));
        }
        for code in start..=end {
            if code == 0xFFFF {
                continue;
            }
            let glyph = if range_offset == 0 {
                let code = u16::try_from(code).map_err(|_| parse_err("cmap code beyond BMP"))?;
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

    /// The transform applying `inner` first, then `self` — how a nested
    /// composite component composes under its parent.
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
/// font-unit → pixel transform (including the composite component
/// transform).
struct OutlineSink {
    segments: Vec<Segment>,
    /// Pixels per font unit.
    scale: f64,
    /// Pixel x of font-unit x = 0.
    origin_x: f64,
    /// Pixel y of the baseline (font-unit y = 0); font y grows up, pixel y
    /// grows down.
    baseline_y: f64,
    /// The composite-glyph component transform, in font units.
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

    /// Flatten the quadratic Bézier `(x0, y0) — (cx, cy) — (x1, y1)` into
    /// [`QUAD_SEGMENTS`] chords.
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
) -> Result<(), String> {
    if depth > 4 {
        return Err(parse_err("composite glyph nesting deeper than 4"));
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
) -> Result<(), String> {
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
                    return Err(parse_err("flag repeat overruns point count"));
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
        return Err(parse_err("simple glyph overruns its loca range"));
    }

    let mut first = 0usize;
    for &contour_end in &contour_ends {
        let last = usize::from(contour_end);
        if last < first || last >= point_count {
            return Err(parse_err("contour end indices not monotonic"));
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
    // Establish the starting on-curve point per the TrueType rules: the
    // first on-curve point of the contour, or — when every point is
    // off-curve — the implied midpoint between the last and first points.
    // `offset` indexes the chosen start so the walk below begins after it.
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
    // Close the contour back to the start, through a trailing control point
    // if one is still pending.
    if let Some(c) = pending_control {
        sink.quad(current.x, current.y, c.x, c.y, start.x, start.y);
    } else {
        // Close the contour unless the walk already returned to the exact
        // start point (an exact comparison, hence `total_cmp`: a
        // nearly-closed contour still needs its closing segment).
        let closed = current.x.total_cmp(&start.x) == core::cmp::Ordering::Equal
            && current.y.total_cmp(&start.y) == core::cmp::Ordering::Equal;
        if !closed {
            sink.line(current.x, current.y, start.x, start.y);
        }
    }
}

/// Decode a composite glyph: recurse into each component with its affine
/// transform (XY offset, plain/xy scale, or 2×2 matrix) composed onto the
/// sink. Point-matching args and scaled component offsets fail closed.
fn outline_composite(
    face: &Face<'_>,
    start: usize,
    sink: &mut OutlineSink,
    depth: u32,
) -> Result<(), String> {
    let r = &face.r;
    let mut at = start + 10;
    loop {
        let flags = r.u16(at)?;
        let component = r.u16(at + 2)?;
        at += 4;
        let args_are_words = flags & 0x0001 != 0;
        let args_are_xy = flags & 0x0002 != 0;
        if !args_are_xy {
            return Err(parse_err("composite point-matching args unsupported"));
        }
        // SCALED_COMPONENT_OFFSET would rescale the offset by the component
        // transform (the Apple interpretation); the faces this generator
        // accepts use plain font-unit offsets, so anything else is a wrong
        // atlas waiting to happen.
        if flags & 0x0800 != 0 {
            return Err(parse_err("composite scaled component offset unsupported"));
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
        let f2dot14 = |at_ref: &mut usize| -> Result<f64, String> {
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

/// The pixel geometry the atlas is rasterised at, derived from the face.
struct CellGeometry {
    /// Cell width in pixels: the face's uniform advance, rounded to whole
    /// pixels.
    width: u32,
    /// Cell height in pixels: ascent rows plus descent rows.
    height: u32,
    /// Baseline row: pixel rows above the baseline.
    baseline: u32,
}

impl CellGeometry {
    fn derive(face: &Face<'_>, advance: u16) -> Result<Self, String> {
        let units = i64::from(face.units_per_em);
        // Round the uniform advance to the nearest whole pixel: the cell grid
        // needs an integral width, and half-up rounding keeps the distortion
        // under half a pixel per glyph.
        let width_px = (i64::from(advance) * i64::from(EM_PX) + units / 2).div_euclid(units);
        let width =
            u32::try_from(width_px).map_err(|_| parse_err("advance overflows the cell width"))?;
        // Integer ceiling division keeps the pixel metrics exact.
        let ceil_px = |value: i32| -> Result<u32, String> {
            let px = (i64::from(value) * i64::from(EM_PX) + units - 1).div_euclid(units);
            u32::try_from(px).map_err(|_| parse_err("negative vertical metric"))
        };
        let baseline = ceil_px(face.ascent)?;
        let descent_rows = ceil_px(face.descent)?;
        let height = baseline + descent_rows;
        if width == 0 || height == 0 || height > 64 {
            return Err(parse_err("implausible cell geometry"));
        }
        Ok(Self {
            width,
            height,
            baseline,
        })
    }
}

/// Rasterise `segments` into one cell of 4-bit coverage values (`0..=15`),
/// row-major, one nibble value per byte (packing happens at emission).
///
/// Non-zero winding fill: each of the [`SAMPLE_ROWS`] sample rows per pixel
/// row contributes `1 / SAMPLE_ROWS` of a pixel's coverage, distributed over
/// the pixels its filled spans cross with exact fractional x extents.
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
                // Half-open [top, bottom) so a vertex shared by two segments
                // is counted exactly once.
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
    ((coverage * 15.0) + 0.5).floor().clamp(0.0, 15.0) as u8
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
    let x0 = x0.max(0.0);
    let x1 = x1.min(width);
    if x0 >= x1 {
        return;
    }
    let first = x0.floor() as usize;
    let last = (x1.ceil() as usize).min(row.len());
    for (i, cell) in row.iter_mut().enumerate().take(last).skip(first) {
        let left = (i as f64).max(x0);
        let right = ((i + 1) as f64).min(x1);
        if right > left {
            *cell += weight * (right - left);
        }
    }
}

/// A contiguous run of mapped codepoints, indexing consecutive glyph cells.
struct AtlasRange {
    first: u32,
    len: u32,
    base: u32,
}

/// The fully built atlas, ready for emission.
struct Atlas {
    geometry: CellGeometry,
    ranges: Vec<AtlasRange>,
    /// One two-cell-capable bitmap of unpacked 4-bit coverage values per
    /// mapped codepoint, in codepoint order.
    cells: Vec<Vec<u8>>,
    /// Index of the U+FFFD replacement-character cell.
    fallback: u32,
}

/// Rasterise an ordered family of faces into an [`Atlas`].
///
/// For an overlapping codepoint, the earliest face supplies the outline.
fn build_atlas(face_data: &[&[u8]]) -> Result<Atlas, String> {
    let faces = face_data
        .iter()
        .map(|data| Face::parse(data))
        .collect::<Result<Vec<_>, _>>()?;
    let primary = faces
        .first()
        .ok_or_else(|| parse_err("font family has no primary face"))?;
    let advance = primary.uniform_advance()?;
    let geometry = CellGeometry::derive(primary, advance)?;
    let glyph_width = geometry.width * MAX_GLYPH_CELLS;
    let mut ranges: Vec<AtlasRange> = Vec::new();
    let mut cells = Vec::with_capacity(faces.iter().map(|face| face.mapped.len()).sum());
    let mut fallback = None;
    let mut face_indices = vec![0usize; faces.len()];
    while let Some((face_index, code, glyph)) = faces
        .iter()
        .enumerate()
        .filter_map(|(face_index, face)| {
            face.mapped
                .get(face_indices[face_index])
                .map(|&(code, glyph)| (face_index, code, glyph))
        })
        .min_by_key(|&(face_index, code, _)| (code, face_index))
    {
        let face = &faces[face_index];
        for (other_face, index) in faces.iter().zip(face_indices.iter_mut()) {
            if other_face
                .mapped
                .get(*index)
                .is_some_and(|&(other_code, _)| other_code == code)
            {
                *index += 1;
            }
        }
        // A zero-advance combining mark rasterises like any other glyph: the
        // face draws mark outlines inside the advance-wide cell, so each
        // mark lands in its own cell — the one-scalar-per-cell model the
        // terminal stack documents.
        let mut sink = OutlineSink {
            segments: Vec::new(),
            scale: f64::from(EM_PX) / f64::from(face.units_per_em),
            origin_x: 0.0,
            baseline_y: f64::from(geometry.baseline),
            transform: Affine::IDENTITY,
        };
        outline_glyph(face, glyph, &mut sink, 0)?;
        let cell = rasterise(&sink.segments, &geometry, glyph_width);
        let index = u32::try_from(cells.len())
            .map_err(|_| parse_err("more mapped codepoints than fit a u32 index"))?;
        if code == u32::from(char::REPLACEMENT_CHARACTER) {
            fallback = Some(index);
        }
        match ranges.last_mut() {
            Some(range) if range.first + range.len == code => range.len += 1,
            _ => ranges.push(AtlasRange {
                first: code,
                len: 1,
                base: index,
            }),
        }
        cells.push(cell);
    }
    let fallback = fallback
        .ok_or_else(|| parse_err("face does not map U+FFFD, so there is no fallback glyph"))?;
    Ok(Atlas {
        geometry,
        ranges,
        cells,
        fallback,
    })
}

impl Atlas {
    /// Packed bytes per glyph bitmap: two 4-bit pixels per byte, rows padded
    /// to whole bytes.
    fn bytes_per_glyph(&self) -> usize {
        (self.geometry.width as usize * MAX_GLYPH_CELLS as usize).div_ceil(2)
            * self.geometry.height as usize
    }

    /// The packed coverage payload: every glyph bitmap concatenated in codepoint
    /// order, each row packed two pixels per byte, left pixel in the high
    /// nibble.
    fn coverage_bytes(&self) -> Vec<u8> {
        let width = self.geometry.width as usize * MAX_GLYPH_CELLS as usize;
        let mut bytes = Vec::with_capacity(self.cells.len() * self.bytes_per_glyph());
        for cell in &self.cells {
            for row in cell.chunks(width) {
                for pair in row.chunks(2) {
                    let high = pair[0] << 4;
                    let low = pair.get(1).copied().unwrap_or(0);
                    bytes.push(high | low);
                }
            }
        }
        bytes
    }

    /// Render the generated Rust view (`lib/font/src/atlas.rs`).
    fn render_rust(&self) -> String {
        let mut out = String::new();
        out.push_str(
            "// GENERATED FILE — DO NOT EDIT.\n\
             //\n\
             // Emitted by `cargo xtask font-atlas --write` from the committed faces\n\
             // under `lib/font/assets/` (SIL OFL 1.1; see\n\
             // `lib/font/assets/OFL.txt` and `NotoSansHebrew-OFL.txt`).\n\
             // `cargo xtask font-atlas` (run by `ci`)\n\
             // fails closed if this file drifts from a fresh generation\n\
             // (AGENTS.md §2.2: generated views are never hand-maintained).\n\n",
        );
        out.push_str(
            "//! The generated Inconsolata EX + M PLUS 1 Code + Noto Sans Hebrew\n\
             //! glyph atlas: fixed-cell 4-bit coverage bitmaps for the merged\n\
             //! repertoire, plus the codepoint → glyph-index range table. Pure data;\n\
             //! lookup and blitting live in\n\
             //! the hand-written modules of this crate.\n\n",
        );
        let _ = writeln!(
            out,
            "/// Glyph cell width in pixels (the face's uniform advance, rounded to\n\
             /// whole pixels).\n\
             pub const CELL_WIDTH: u32 = {};\n",
            self.geometry.width
        );
        let _ = writeln!(
            out,
            "/// Maximum glyph bitmap width in pixels. Wide glyphs may cover both\n\
             /// terminal cells; narrow glyphs leave the second cell transparent.\n\
             pub const GLYPH_WIDTH: u32 = {};\n",
            self.geometry.width * MAX_GLYPH_CELLS
        );
        let _ = writeln!(
            out,
            "/// Glyph cell height in pixels (ascent rows plus descent rows).\n\
             pub const CELL_HEIGHT: u32 = {};\n",
            self.geometry.height
        );
        let _ = writeln!(
            out,
            "/// Baseline row: pixel rows above the baseline within a cell.\n\
             pub const BASELINE: u32 = {};\n",
            self.geometry.baseline
        );
        let _ = writeln!(
            out,
            "/// Packed bytes per glyph cell (two 4-bit pixels per byte, rows padded\n\
             /// to whole bytes).\n\
             pub const BYTES_PER_CELL: usize = {};\n",
            (self.geometry.width as usize).div_ceil(2) * self.geometry.height as usize
        );
        let _ = writeln!(
            out,
            "/// Packed bytes per two-cell-capable glyph bitmap.\n\
             pub const BYTES_PER_GLYPH: usize = {};\n",
            self.bytes_per_glyph()
        );
        let _ = writeln!(
            out,
            "/// Cell index of the U+FFFD replacement character: the fallback for a\n\
             /// codepoint the face does not map.\n\
             pub const FALLBACK_INDEX: u32 = {};\n",
            self.fallback
        );
        let _ = writeln!(
            out,
            "/// Total glyph bitmaps in [`COVERAGE`].\n\
             pub const CELL_COUNT: u32 = {};\n",
            self.cells.len()
        );
        out.push_str(
            "/// The sorted, non-overlapping codepoint runs the atlas covers:\n\
             /// `(first, len, base)` maps codepoints `first..first + len` to the\n\
             /// consecutive cells starting at index `base`.\n\
             pub const RANGES: &[(u32, u32, u32)] = &[\n",
        );
        for range in &self.ranges {
            let _ = writeln!(
                out,
                "    (0x{:04X}, {}, {}),",
                range.first, range.len, range.base
            );
        }
        out.push_str("];\n\n");
        out.push_str(
            "/// The packed coverage payload: [`CELL_COUNT`] glyph bitmaps of\n\
             /// [`BYTES_PER_GLYPH`] bytes each, in range order. Within a bitmap: row\n\
             /// major, two 4-bit pixels per byte, left pixel in the high nibble,\n\
             /// `0` transparent through `15` fully covered.\n\
             pub static COVERAGE: &[u8] = include_bytes!(\"atlas_coverage.bin\");\n",
        );
        out
    }
}

/// Generate the atlas from the committed face, returning the two artefacts
/// as `(rust_view, coverage_payload)`.
fn generate(workspace_root: &Path) -> Result<(String, Vec<u8>), String> {
    let paths = [DEFAULT_TTF_PATH, JAPANESE_TTF_PATH, HEBREW_TTF_PATH];
    let face_data = paths
        .map(|path| {
            let path = workspace_root.join(path);
            std::fs::read(&path)
                .map_err(|e| format!("font-atlas: cannot read {}: {e}", path.display()))
        })
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;
    let faces = face_data.iter().map(Vec::as_slice).collect::<Vec<_>>();
    let atlas = build_atlas(&faces)?;
    Ok((atlas.render_rust(), atlas.coverage_bytes()))
}

/// Regenerate the committed atlas artefacts in place (`--write`).
pub fn write(workspace_root: &Path) -> Result<(), String> {
    let (rust_view, coverage) = generate(workspace_root)?;
    let rs_path = workspace_root.join(DEFAULT_ATLAS_RS_PATH);
    let bin_path = workspace_root.join(DEFAULT_ATLAS_BIN_PATH);
    std::fs::write(&rs_path, rust_view)
        .map_err(|e| format!("font-atlas: cannot write {}: {e}", rs_path.display()))?;
    std::fs::write(&bin_path, coverage)
        .map_err(|e| format!("font-atlas: cannot write {}: {e}", bin_path.display()))?;
    Ok(())
}

/// Verify the committed atlas artefacts match a fresh generation (the `ci`
/// drift guard). Fails closed with the regeneration command on any mismatch.
pub fn check_sync(workspace_root: &Path) -> Result<(), String> {
    let (rust_view, coverage) = generate(workspace_root)?;
    let rs_path = workspace_root.join(DEFAULT_ATLAS_RS_PATH);
    let bin_path = workspace_root.join(DEFAULT_ATLAS_BIN_PATH);
    let drifted = |path: &Path| {
        format!(
            "font-atlas: `{}` is out of sync with `{}`; \
             run `cargo xtask font-atlas --write` and commit the result.",
            path.display(),
            "the committed font sources",
        )
    };
    let committed_rs = std::fs::read(&rs_path).map_err(|_| drifted(&rs_path))?;
    if committed_rs != rust_view.as_bytes() {
        return Err(drifted(&rs_path));
    }
    let committed_bin = std::fs::read(&bin_path).map_err(|_| drifted(&bin_path))?;
    if committed_bin != coverage {
        return Err(drifted(&bin_path));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("workspace root")
    }

    fn committed_face() -> Vec<u8> {
        std::fs::read(workspace_root().join(DEFAULT_TTF_PATH)).expect("committed face")
    }

    fn committed_fallback_face() -> Vec<u8> {
        std::fs::read(workspace_root().join(JAPANESE_TTF_PATH)).expect("committed fallback face")
    }

    fn committed_hebrew_face() -> Vec<u8> {
        std::fs::read(workspace_root().join(HEBREW_TTF_PATH)).expect("committed Hebrew face")
    }

    fn committed_atlas() -> Atlas {
        let primary = committed_face();
        let japanese = committed_fallback_face();
        let hebrew = committed_hebrew_face();
        build_atlas(&[&primary, &japanese, &hebrew]).expect("atlas builds")
    }

    /// The coverage cell for `code`, unpacked to one nibble value per pixel.
    fn cell_of(atlas: &Atlas, code: u32) -> &[u8] {
        let range = atlas
            .ranges
            .iter()
            .find(|r| (r.first..r.first + r.len).contains(&code))
            .expect("code is mapped");
        let index = (range.base + (code - range.first)) as usize;
        &atlas.cells[index]
    }

    #[test]
    fn geometry_matches_the_face_metrics() {
        let atlas = committed_atlas();
        // Inconsolata EX: 1024 upm, ascent 939, descent 198, advance 613,
        // at a 25 px em.
        assert_eq!(atlas.geometry.width, 15);
        assert_eq!(atlas.geometry.height, 28);
        assert_eq!(atlas.geometry.baseline, 23);
        assert_eq!(atlas.bytes_per_glyph(), 15 * 28);
    }

    #[test]
    fn ranges_are_sorted_dense_and_non_overlapping() {
        let atlas = committed_atlas();
        let mut previous_end = 0u32;
        let mut expected_base = 0u32;
        for range in &atlas.ranges {
            assert!(range.len > 0);
            assert!(
                range.first >= previous_end,
                "ranges overlap or are unsorted"
            );
            assert_eq!(range.base, expected_base, "cells are not in range order");
            previous_end = range.first + range.len;
            expected_base += range.len;
        }
        assert_eq!(expected_base as usize, atlas.cells.len());
    }

    #[test]
    fn printable_ascii_is_covered_and_space_is_blank() {
        let atlas = committed_atlas();
        for code in 0x20..=0x7Eu32 {
            assert!(
                atlas
                    .ranges
                    .iter()
                    .any(|r| (r.first..r.first + r.len).contains(&code)),
                "U+{code:04X} is not covered"
            );
        }
        assert!(
            cell_of(&atlas, 0x20).iter().all(|&c| c == 0),
            "space is not blank"
        );
        assert!(
            cell_of(&atlas, u32::from('A')).contains(&15),
            "'A' has no fully covered pixel"
        );
    }

    #[test]
    fn box_drawing_full_block_is_solid() {
        let atlas = committed_atlas();
        // U+2588 FULL BLOCK: a monospace face draws it edge to edge, so the
        // whole cell interior must be fully covered.
        let cell = cell_of(&atlas, 0x2588);
        let solid = cell
            .iter()
            .fold(0usize, |count, &c| count + usize::from(c == 15));
        assert!(
            solid * 10 >= (cell.len() / 2) * 9,
            "full block is only {solid}/{} solid",
            cell.len()
        );
    }

    #[test]
    fn combining_mark_is_rendered_as_a_spacing_glyph() {
        let atlas = committed_atlas();
        // U+0301 COMBINING ACUTE ACCENT: zero advance, but the face draws
        // its outline inside the advance-wide cell, so it must have visible
        // ink.
        assert!(
            cell_of(&atlas, 0x0301).iter().any(|&c| c > 0),
            "combining acute has no ink"
        );
    }

    #[test]
    fn cyrillic_is_covered_with_ink() {
        let atlas = committed_atlas();
        // The whole Cyrillic block the face maps must rasterise with real
        // ink, not fall back to U+FFFD — the Ukrainian console regression:
        // і ї є ґ plus the base alphabet.
        for ch in [
            'і', 'ї', 'є', 'ґ', 'І', 'Ї', 'Є', 'Ґ', 'а', 'я', 'А', 'Я', 'Щ', 'ь',
        ] {
            let cell = cell_of(&atlas, u32::from(ch));
            assert!(cell.iter().any(|&c| c > 0), "{ch:?} has no ink");
        }
    }

    #[test]
    fn japanese_is_covered_with_two_cell_glyphs() {
        let atlas = committed_atlas();
        let width = atlas.geometry.width as usize * MAX_GLYPH_CELLS as usize;
        for ch in ['あ', 'ア', '漢', '字', '日', '本', '語', '。', '「', '」'] {
            let glyph = cell_of(&atlas, u32::from(ch));
            assert!(
                glyph.iter().any(|&coverage| coverage > 0),
                "{ch:?} has no ink"
            );
        }
        for ch in ['あ', 'ア', '漢', '字', '日', '本', '語'] {
            let glyph = cell_of(&atlas, u32::from(ch));
            assert!(
                glyph
                    .chunks(width)
                    .any(|row| row[atlas.geometry.width as usize..].iter().any(|&c| c > 0)),
                "{ch:?} never reaches its continuation cell"
            );
        }
    }

    #[test]
    fn hebrew_is_covered_with_single_cell_glyphs() {
        let atlas = committed_atlas();
        let width = atlas.geometry.width as usize * MAX_GLYPH_CELLS as usize;
        for ch in [
            'א', 'ב', 'ה', 'ו', 'ל', 'ם', 'ש', 'ך', 'ן', 'ף', 'ץ', '־', '׳', '״', '\u{05B0}',
            '\u{05B8}', '\u{05BC}',
        ] {
            let glyph = cell_of(&atlas, u32::from(ch));
            assert!(
                glyph.iter().any(|&coverage| coverage > 0),
                "{ch:?} has no ink"
            );
            assert!(
                glyph
                    .chunks(width)
                    .all(|row| row[atlas.geometry.width as usize..].iter().all(|&c| c == 0)),
                "{ch:?} reaches beyond its terminal cell"
            );
        }
    }

    #[test]
    fn fallback_is_the_replacement_character() {
        let atlas = committed_atlas();
        let cell = cell_of(&atlas, 0xFFFD);
        assert!(cell.iter().any(|&c| c > 0), "U+FFFD has no ink");
        let range = atlas
            .ranges
            .iter()
            .find(|r| (r.first..r.first + r.len).contains(&0xFFFD))
            .expect("U+FFFD mapped");
        assert_eq!(range.base + (0xFFFD - range.first), atlas.fallback);
    }

    #[test]
    fn generation_is_deterministic() {
        let face = committed_face();
        let fallback_face = committed_fallback_face();
        let hebrew_face = committed_hebrew_face();
        let faces = [&face[..], &fallback_face[..], &hebrew_face[..]];
        let a = build_atlas(&faces).expect("atlas builds");
        let b = build_atlas(&faces).expect("atlas builds");
        assert_eq!(a.render_rust(), b.render_rust());
        assert_eq!(a.coverage_bytes(), b.coverage_bytes());
    }

    #[test]
    fn truncated_face_fails_closed() {
        let face = committed_face();
        let fallback_face = committed_fallback_face();
        let hebrew_face = committed_hebrew_face();
        assert!(build_atlas(&[]).is_err());
        assert!(build_atlas(&[&face[..64], &fallback_face, &hebrew_face]).is_err());
        assert!(build_atlas(&[&face[..face.len() / 2], &fallback_face, &hebrew_face]).is_err());
        assert!(build_atlas(&[&face, &fallback_face[..64], &hebrew_face]).is_err());
        assert!(build_atlas(&[
            &face,
            &fallback_face[..fallback_face.len() / 2],
            &hebrew_face,
        ])
        .is_err());
        assert!(build_atlas(&[&face, &fallback_face, &hebrew_face[..64]]).is_err());
        assert!(
            build_atlas(&[&face, &fallback_face, &hebrew_face[..hebrew_face.len() / 2],]).is_err()
        );
    }
}
