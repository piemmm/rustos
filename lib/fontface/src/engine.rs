//! The TrueType parser, outline decoder, and coverage rasteriser.

use alloc::vec;
use alloc::vec::Vec;

use crate::gridfit::{self, AlignZones, Axes, Zone};
use crate::mathf;
use crate::variations::{self, Axis, AxisSetting, Gvar, VarTables};
use crate::FontError;

/// Coverage sample rows per pixel row (vertical supersampling).
const SAMPLE_ROWS: u32 = 4;

/// Line segments each quadratic Bézier is flattened into.
///
/// Eight chords keep the flattening error well under a tenth of a pixel at the
/// atlas's native size, invisible at 16 coverage levels.
const QUAD_SEGMENTS: u32 = 8;

/// Widest proportional glyph, in pixels, the engine will lay out.
///
/// A tight proportional bitmap is sized from a glyph's own ink, so a corrupt
/// outline with runaway coordinates could otherwise demand an unbounded
/// allocation; a width past this bound fails closed. A validation bound, not a
/// capacity — legitimate glyphs are a small multiple of the em.
const MAX_PROPORTIONAL_WIDTH: u32 = 1 << 14;

pub(crate) const fn err(what: &'static str) -> FontError {
    FontError::new(what)
}

/// Big-endian field reads over the raw font bytes, each bounds-checked.
pub(crate) struct Reader<'a> {
    data: &'a [u8],
}

impl Reader<'_> {
    pub(crate) fn u8(&self, at: usize) -> Result<u8, FontError> {
        self.data.get(at).copied().ok_or(err("truncated u8"))
    }

    pub(crate) fn i8(&self, at: usize) -> Result<i8, FontError> {
        Ok(i8::from_be_bytes([self.u8(at)?]))
    }

    pub(crate) fn u16(&self, at: usize) -> Result<u16, FontError> {
        let b = self.data.get(at..at + 2).ok_or(err("truncated u16"))?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    pub(crate) fn i16(&self, at: usize) -> Result<i16, FontError> {
        let b = self.data.get(at..at + 2).ok_or(err("truncated i16"))?;
        Ok(i16::from_be_bytes([b[0], b[1]]))
    }

    pub(crate) fn u32(&self, at: usize) -> Result<u32, FontError> {
        let b = self.data.get(at..at + 4).ok_or(err("truncated u32"))?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub(crate) fn i32(&self, at: usize) -> Result<i32, FontError> {
        let b = self.data.get(at..at + 4).ok_or(err("truncated i32"))?;
        Ok(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
}

/// Offsets of the tables the engine needs, from the table directory. The
/// variation tables are optional: a static face declares none.
struct Tables {
    head: usize,
    maxp: usize,
    cmap: usize,
    hhea: usize,
    hmtx: usize,
    loca: usize,
    glyf: usize,
    fvar: Option<usize>,
    avar: Option<usize>,
    gvar: Option<usize>,
    hvar: Option<usize>,
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
        let mut fvar = None;
        let mut avar = None;
        let mut gvar = None;
        let mut hvar = None;
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
                b"fvar" => fvar = Some(offset),
                b"avar" => avar = Some(offset),
                b"gvar" => gvar = Some(offset),
                b"HVAR" => hvar = Some(offset),
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
            fvar,
            avar,
            gvar,
            hvar,
        })
    }
}

/// A parsed TrueType face: the tables and metrics the rasteriser needs, plus
/// the sorted `(codepoint, glyph)` pairs from its `cmap`.
///
/// A variable face is parsed *instanced*: the requested axis settings are
/// resolved once into normalised `coords`, and every outline and advance is
/// varied against them on demand. A static face carries no axes and no
/// `coords`, so it behaves exactly as an unvaried TrueType face.
pub struct Face<'a> {
    r: Reader<'a>,
    tables: Tables,
    units_per_em: i32,
    ascent: i32,
    descent: i32,
    line_gap: i32,
    glyph_count: u16,
    advance_count: u16,
    long_loca: bool,
    mapped: Vec<(u32, u16)>,
    axes: Vec<Axis>,
    coords: Vec<f32>,
    var: Option<VarTables>,
    /// The one advance every mapped spacing glyph shares, when the face is
    /// strictly monospace. Read once here because the cell grid asks for it
    /// per glyph.
    uniform: Option<u16>,
    /// The rows every glyph of the face aligns to, read off its own outlines.
    zones: AlignZones,
}

impl<'a> Face<'a> {
    /// Parse `data` (a TrueType `glyf`-outline face) at its default instance.
    ///
    /// Equivalent to [`parse_instance`](Self::parse_instance) with no settings,
    /// so a variable face renders at every axis's default and a static face is
    /// unchanged.
    ///
    /// # Errors
    ///
    /// Returns a [`FontError`] when a required table is missing, a field is
    /// truncated, the vertical metrics are non-positive, or the `cmap` has no
    /// format-4 subtable.
    pub fn parse(data: &'a [u8]) -> Result<Self, FontError> {
        Self::parse_instance(data, &[])
    }

    /// Parse `data` and instance it at the given axis `settings`.
    ///
    /// Each setting is resolved against the face's `fvar` axes: its value is
    /// clamped into the axis range, normalised to `-1..0..+1`, and remapped
    /// through `avar` when present. Axes no setting names stay at their
    /// default, and a setting for an axis the face does not declare is ignored.
    /// A face without `fvar` accepts any settings and applies no variation, so
    /// its output is byte-identical to an unvaried parse.
    ///
    /// # Errors
    ///
    /// Returns a [`FontError`] when a required table is missing, a field is
    /// truncated, the vertical metrics are non-positive, the `cmap` has no
    /// format-4 subtable, or a variation table (`fvar`/`avar`/`gvar`) is
    /// malformed.
    pub fn parse_instance(data: &'a [u8], settings: &[AxisSetting]) -> Result<Self, FontError> {
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
        let line_gap = i32::from(r.i16(tables.hhea + 8)?);
        let advance_count = r.u16(tables.hhea + 34)?;
        if advance_count == 0 {
            return Err(err("numberOfHMetrics is zero"));
        }
        let glyph_count = r.u16(tables.maxp + 4)?;
        let mapped = parse_cmap_format4(&r, tables.cmap)?;
        let axes = match tables.fvar {
            Some(fvar) => variations::parse_fvar(&r, fvar)?,
            None => Vec::new(),
        };
        let coords = resolve_coords(&r, &axes, tables.avar, settings)?;
        let var = match (tables.fvar, tables.gvar) {
            (Some(_), Some(gvar)) => Some(VarTables {
                gvar: Gvar::parse(&r, gvar, axes.len())?,
                hvar: tables.hvar,
            }),
            _ => None,
        };
        let mut face = Self {
            r,
            tables,
            units_per_em,
            ascent,
            descent,
            line_gap,
            glyph_count,
            advance_count,
            long_loca,
            mapped,
            axes,
            coords,
            var,
            uniform: None,
            zones: AlignZones::default(),
        };
        face.uniform = face.read_uniform_advance();
        face.zones = face.read_align_zones();
        Ok(face)
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

    /// The variation axes the face declares, empty for a static face.
    #[must_use]
    pub fn axes(&self) -> &[Axis] {
        &self.axes
    }

    /// Whether the face is variable — it declares both an `fvar` axis set and
    /// `gvar` outline deltas, so instancing it changes its glyphs.
    #[must_use]
    pub fn is_variable(&self) -> bool {
        self.var.is_some()
    }

    /// The `hhea` line gap in font units — the extra leading between lines on
    /// top of ascent plus descent.
    #[must_use]
    pub fn line_gap(&self) -> i32 {
        self.line_gap
    }

    /// The unvaried advance width of `glyph` in font units, straight from
    /// `hmtx`.
    fn base_advance(&self, glyph: u16) -> Result<u16, FontError> {
        let index = glyph.min(self.advance_count - 1);
        self.r.u16(self.tables.hmtx + 4 * usize::from(index))
    }

    /// The advance width of `glyph` in font units at the instanced coordinate.
    ///
    /// For a variable face the `hmtx` base is corrected by the `HVAR` advance
    /// delta when present, else derived from the glyph's varied phantom points;
    /// for a static face it is the `hmtx` value unchanged.
    ///
    /// # Errors
    ///
    /// Returns a [`FontError`] when `hmtx` is truncated or a variation store is
    /// malformed.
    pub fn advance(&self, glyph: u16) -> Result<i32, FontError> {
        let base = i32::from(self.base_advance(glyph)?);
        let Some(var) = &self.var else {
            return Ok(base);
        };
        let delta = if let Some(hvar) = var.hvar {
            variations::hvar_advance_delta(&self.r, hvar, &self.coords, glyph)?
        } else {
            let n = self.point_count(glyph)?;
            let mut deltas = vec![(0.0, 0.0); n + 4];
            var.gvar
                .deltas(&self.r, &self.coords, glyph, n, None, &mut deltas)?;
            mathf::round_i32(deltas[n + 1].0 - deltas[n].0)
        };
        Ok(base + delta)
    }

    /// The rows glyphs of this face align to, for the grid fitter.
    ///
    /// Read from the face's own reference glyphs: the flat-sided ones fix
    /// where a zone sits, the round ones how far past it they overshoot. A
    /// face that maps none of a zone's references simply has no such zone,
    /// and its glyphs are fitted on their strokes alone.
    fn read_align_zones(&self) -> AlignZones {
        /// The reference glyphs each zone is read from: the flat-sided ones,
        /// the round ones that overshoot past them, and whether the zone is
        /// the top or the bottom of those glyphs. The baseline is not listed
        /// — it is zero by definition, and only its overshoot is read.
        const SOURCES: [(&[char], &[char], bool); 4] = [
            (&['x', 'z', 'v'], &['o', 'e', 'c'], true),
            (&['H', 'E', 'Z'], &['O', 'C', 'G'], true),
            (&['b', 'd', 'h', 'k', 'l'], &[], true),
            (&['p', 'q'], &['g', 'j', 'y'], false),
        ];

        let round_bottom = self.reference_extreme(&['o', 'e', 'c'], false);
        let mut zones = vec![Zone::new(0.0, round_bottom.unwrap_or(0.0))];
        for &(flat_from, round_from, top) in &SOURCES {
            let Some(flat) = self.reference_extreme(flat_from, top) else {
                continue;
            };
            let over = self.reference_extreme(round_from, top).unwrap_or(flat);
            zones.push(Zone::new(flat, over));
        }
        AlignZones::new(zones)
    }

    /// How far the first of `chars` the face draws reaches, in font units
    /// below the baseline (negative above): its top when `top`, else its
    /// bottom. `None` when the face draws none of them.
    fn reference_extreme(&self, chars: &[char], top: bool) -> Option<f64> {
        for &ch in chars {
            let Some(glyph) = self.glyph_for(u32::from(ch)) else {
                continue;
            };
            let mut sink = OutlineSink::uniform(1.0, 0.0);
            if outline_glyph(self, glyph, &mut sink, 0).is_err() || sink.segments.is_empty() {
                continue;
            }
            let reach = sink.segments.iter().fold(None, |reach: Option<f64>, seg| {
                let end = if top {
                    mathf::fmin(seg.y0, seg.y1)
                } else {
                    mathf::fmax(seg.y0, seg.y1)
                };
                Some(match reach {
                    Some(seen) if top => mathf::fmin(seen, end),
                    Some(seen) => mathf::fmax(seen, end),
                    None => end,
                })
            });
            if reach.is_some() {
                return reach;
            }
        }
        None
    }

    /// The one advance width every mapped spacing glyph shares, in font units.
    ///
    /// The cell grid only works over a strictly monospace face: every mapped
    /// glyph must advance by this uniform width or by zero (a combining mark).
    /// A face mixing two spacing widths fails closed here. This is the
    /// unvaried `hmtx` width — the monospace faces the cell grid serves are
    /// static, so instancing does not enter into it.
    ///
    /// # Errors
    ///
    /// Returns a [`FontError`] when the face maps no spacing glyph or is not
    /// strictly monospace.
    pub fn uniform_advance(&self) -> Result<u16, FontError> {
        self.uniform.ok_or(err("face is not strictly monospace"))
    }

    /// The uniform advance, or `None` when the face mixes spacing widths or
    /// maps no spacing glyph at all.
    fn read_uniform_advance(&self) -> Option<u16> {
        let mut uniform = None;
        for &(_, glyph) in &self.mapped {
            let advance = self.base_advance(glyph).ok()?;
            if advance == 0 {
                continue;
            }
            match uniform {
                None => uniform = Some(advance),
                Some(seen) if seen == advance => {}
                Some(_) => return None,
            }
        }
        uniform
    }

    /// The number of outline points `glyph` contributes to a `gvar` tuple: the
    /// point count of a simple glyph, the component count of a composite, or
    /// zero for an empty glyph (which still carries four phantom points).
    fn point_count(&self, glyph: u16) -> Result<usize, FontError> {
        let Some((start, _)) = self.glyf_range(glyph)? else {
            return Ok(0);
        };
        let contour_count = self.r.i16(start)?;
        if contour_count < 0 {
            return Ok(decode_components(&self.r, start)?.len());
        }
        let contour_count = usize::from(contour_count.unsigned_abs());
        if contour_count == 0 {
            return Ok(0);
        }
        Ok(usize::from(self.r.u16(start + 10 + 2 * (contour_count - 1))?) + 1)
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
        let scale = px_per_em / f64::from(self.units_per_em);
        let sink = OutlineSink {
            scale_x: self.cell_scale(geometry.width).unwrap_or(scale),
            ..OutlineSink::uniform(scale, f64::from(geometry.baseline))
        };
        let segments = self.fitted_outline(glyph, sink, Axes::RowsAndColumns)?;
        Ok(rasterise(&segments, geometry, bitmap_width))
    }

    /// [`rasterise_glyph`](Self::rasterise_glyph) with the grid fitting left
    /// out, so a test can measure what the fitting did.
    #[cfg(test)]
    pub(crate) fn rasterise_unfitted(
        &self,
        glyph: u16,
        geometry: &CellGeometry,
        px_per_em: f64,
        bitmap_width: u32,
    ) -> Result<Vec<u8>, FontError> {
        let scale = px_per_em / f64::from(self.units_per_em);
        let mut sink = OutlineSink {
            scale_x: self.cell_scale(geometry.width).unwrap_or(scale),
            ..OutlineSink::uniform(scale, f64::from(geometry.baseline))
        };
        outline_glyph(self, glyph, &mut sink, 0)?;
        Ok(rasterise(&sink.segments, geometry, bitmap_width))
    }

    /// Decode `glyph` into `sink`, snap it to the pixel grid along `axes`, and
    /// hand back the segments that can fill.
    ///
    /// A segment with no height crosses no sample row and so contributes
    /// nothing to the winding fill; it is carried this far only because the
    /// fitter reads an outline's flat edges off exactly those segments.
    fn fitted_outline(
        &self,
        glyph: u16,
        mut sink: OutlineSink,
        axes: Axes,
    ) -> Result<Vec<Segment>, FontError> {
        outline_glyph(self, glyph, &mut sink, 0)?;
        gridfit::fit(
            &mut sink.segments,
            axes,
            &self.zones,
            sink.baseline_y,
            sink.scale_y,
        );
        sink.segments
            .retain(|segment| segment.y0.total_cmp(&segment.y1) != core::cmp::Ordering::Equal);
        Ok(sink.segments)
    }

    /// Pixels per font unit across that put this face's uniform advance
    /// exactly on a `width`-pixel cell, or `None` when the face has no one
    /// advance to place there.
    ///
    /// A cell grid's whole premise is that the advance *is* the cell, and a
    /// face's advance rounds to it rather than landing on it: the console
    /// face's 613/1024-em advance is 8.38 pixels in the 8-pixel cell it
    /// defines. Drawn at its own width a glyph overhangs its cell and sits a
    /// twentieth of a cell further right for each column across, which is
    /// ink in the neighbouring cell and stems that no longer line up. Fitting
    /// the advance to the cell costs a 5% narrowing no reader can see and
    /// buys a grid that holds.
    fn cell_scale(&self, width: u32) -> Option<f64> {
        let advance = self.uniform.filter(|&advance| advance > 0)?;
        Some(f64::from(width) / f64::from(advance))
    }

    /// Rasterise `glyph` proportionally: a bitmap tight to the glyph's own ink
    /// in x, positioned by its left bearing rather than inside a fixed cell.
    ///
    /// The outline is scaled by `px_per_em / units_per_em` and laid out with
    /// the baseline `baseline` pixel rows below the top of a `height`-row box.
    /// The returned [`GlyphRaster`] covers only the inked columns: `left` is
    /// the integer pixel column of the leftmost ink relative to the pen origin
    /// (it may be negative), `width` reaches the rightmost ink, and ink above
    /// or below the box is clipped to the `0..height` rows. A glyph with no
    /// ink (a space) yields `width == 0`, `left == 0`, and empty coverage.
    /// Coverage keeps the 4-bit (`0..=15`) row-major convention
    /// [`rasterise_glyph`](Self::rasterise_glyph) returns.
    ///
    /// # Errors
    ///
    /// Returns a [`FontError`] on a malformed outline, or when the glyph's ink
    /// is implausibly wide (a corrupt outline), which fails closed rather than
    /// demanding an unbounded bitmap.
    pub fn rasterise_proportional(
        &self,
        glyph: u16,
        px_per_em: f64,
        baseline: u32,
        height: u32,
    ) -> Result<GlyphRaster, FontError> {
        let scale = px_per_em / f64::from(self.units_per_em);
        let sink = OutlineSink::uniform(scale, f64::from(baseline));
        let mut segments = self.fitted_outline(glyph, sink, Axes::Rows)?;
        let Some(left_edge) = ink_left_edge(&segments) else {
            return Ok(GlyphRaster {
                width: 0,
                height,
                left: 0,
                coverage: Vec::new(),
            });
        };
        let (left, width) = ink_extent(&segments, left_edge)?;
        if width == 0 {
            return Ok(GlyphRaster {
                width: 0,
                height,
                left: 0,
                coverage: Vec::new(),
            });
        }
        for segment in &mut segments {
            segment.x0 -= left_edge;
            segment.x1 -= left_edge;
        }
        let geometry = CellGeometry {
            width,
            height,
            baseline,
        };
        let coverage = rasterise(&segments, &geometry, width);
        Ok(GlyphRaster {
            width,
            height,
            left,
            coverage,
        })
    }
}

/// A glyph rasterised proportionally: a coverage bitmap tight to the ink in x,
/// plus where that ink sits relative to the pen origin.
///
/// `coverage` is `width × height` bytes of 4-bit (`0..=15`) coverage,
/// row-major. `left` is the pixel column of the leftmost inked pixel relative
/// to the pen origin and may be negative; a caller blits the bitmap at
/// `pen_x + left`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GlyphRaster {
    /// Bitmap width in pixels (the inked horizontal extent; `0` when blank).
    pub width: u32,
    /// Bitmap height in pixels (the requested box height).
    pub height: u32,
    /// The leftmost inked column relative to the pen origin, possibly negative.
    pub left: i32,
    /// `width × height` bytes of 4-bit coverage, row-major.
    pub coverage: Vec<u8>,
}

/// Resolve the requested axis `settings` into normalised coordinates, one per
/// declared axis, remapped through `avar` when the face carries it. A static
/// face (no axes) resolves to no coordinates.
fn resolve_coords(
    r: &Reader<'_>,
    axes: &[Axis],
    avar: Option<usize>,
    settings: &[AxisSetting],
) -> Result<Vec<f32>, FontError> {
    if axes.is_empty() {
        return Ok(Vec::new());
    }
    let mut coords: Vec<f32> = axes
        .iter()
        .map(|axis| variations::normalise_axis(axis, settings))
        .collect();
    if let Some(avar) = avar {
        variations::apply_avar(r, avar, &mut coords)?;
    }
    Ok(coords)
}

/// The floored leftmost-ink x of a set of segments in pixel space, or `None`
/// when there are no segments (no ink).
fn ink_left_edge(segments: &[Segment]) -> Option<f64> {
    if segments.is_empty() {
        return None;
    }
    let mut min_x = f64::INFINITY;
    for segment in segments {
        min_x = mathf::fmin(min_x, mathf::fmin(segment.x0, segment.x1));
    }
    Some(mathf::floor(min_x))
}

/// The `(left, width)` of the tight ink box: `left` the floored leftmost ink
/// column, `width` the pixels out to the ceiled rightmost ink.
///
/// # Errors
///
/// A [`FontError`] when the width exceeds [`MAX_PROPORTIONAL_WIDTH`].
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the width is a non-negative integer-valued f64 (ceil minus \
              floor) already checked against MAX_PROPORTIONAL_WIDTH, so the \
              cast to u32 cannot truncate or lose a sign"
)]
fn ink_extent(segments: &[Segment], left_edge: f64) -> Result<(i32, u32), FontError> {
    let mut max_x = f64::NEG_INFINITY;
    for segment in segments {
        max_x = mathf::fmax(max_x, mathf::fmax(segment.x0, segment.x1));
    }
    let width = mathf::ceil(max_x) - left_edge;
    if width > f64::from(MAX_PROPORTIONAL_WIDTH) {
        return Err(err("proportional glyph ink is implausibly wide"));
    }
    Ok((mathf::round_i32(left_edge), width as u32))
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
pub(crate) struct Segment {
    pub(crate) x0: f64,
    pub(crate) y0: f64,
    pub(crate) x1: f64,
    pub(crate) y1: f64,
}

/// Where `segment` crosses the horizontal line `row`, and which way it runs
/// (`+1` downward, `-1` upward), or `None` when it does not reach it.
///
/// The span is half-open in the row, so a vertex two segments share is
/// counted once. This is the whole of the fill rule: the rasteriser sums
/// these crossings into spans, and the grid fitter sums them to the left of a
/// point to ask whether that point is inside the outline — one definition, so
/// the fitter can never disagree with the fill about where the ink is.
pub(crate) fn crossing(segment: &Segment, row: f64) -> Option<(f64, i32)> {
    let (top, bottom, at_top, at_bottom, direction) = if segment.y0 < segment.y1 {
        (segment.y0, segment.y1, segment.x0, segment.x1, 1)
    } else {
        (segment.y1, segment.y0, segment.x1, segment.x0, -1)
    };
    if row < top || row >= bottom {
        return None;
    }
    let t = (row - top) / (bottom - top);
    Some((at_top + t * (at_bottom - at_top), direction))
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
    /// Pixels per font unit across. A cell grid narrows this so the face's
    /// advance lands exactly on the cell; everything else matches `scale_y`.
    scale_x: f64,
    /// Pixels per font unit down.
    scale_y: f64,
    origin_x: f64,
    baseline_y: f64,
    transform: Affine,
}

impl OutlineSink {
    /// A sink scaling both axes alike, for an outline drawn at its own
    /// proportions.
    fn uniform(scale: f64, baseline_y: f64) -> Self {
        Self {
            segments: Vec::new(),
            scale_x: scale,
            scale_y: scale,
            origin_x: 0.0,
            baseline_y,
            transform: Affine::IDENTITY,
        }
    }

    fn to_px(&self, x: f64, y: f64) -> (f64, f64) {
        let (fx, fy) = self.transform.apply(x, y);
        (
            self.origin_x + fx * self.scale_x,
            self.baseline_y - fy * self.scale_y,
        )
    }

    fn line(&mut self, x0: f64, y0: f64, x1: f64, y1: f64) {
        let (px0, py0) = self.to_px(x0, y0);
        let (px1, py1) = self.to_px(x1, y1);
        self.segments.push(Segment {
            x0: px0,
            y0: py0,
            x1: px1,
            y1: py1,
        });
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
    let contour_count = face.r.i16(start)?;
    if contour_count >= 0 {
        outline_simple(
            face,
            glyph,
            start,
            end,
            usize::from(contour_count.unsigned_abs()),
            sink,
        )
    } else {
        outline_composite(face, glyph, start, sink, depth)
    }
}

/// Decode a simple glyph's contours into `sink`, applying its `gvar` deltas
/// (with IUP for untouched points) when the face is instanced.
fn outline_simple(
    face: &Face<'_>,
    glyph: u16,
    start: usize,
    end: usize,
    contour_count: usize,
    sink: &mut OutlineSink,
) -> Result<(), FontError> {
    let r = &face.r;
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

    let mut coords: Vec<(f64, f64)> = (0..point_count)
        .map(|i| (f64::from(xs[i]), f64::from(ys[i])))
        .collect();
    if let Some(var) = &face.var {
        let base = coords.clone();
        let mut deltas = vec![(0.0, 0.0); point_count + 4];
        let applied = var.gvar.deltas(
            r,
            &face.coords,
            glyph,
            point_count,
            Some((&contour_ends, &base)),
            &mut deltas,
        )?;
        if applied {
            for (point, delta) in coords.iter_mut().zip(&deltas) {
                point.0 += delta.0;
                point.1 += delta.1;
            }
        }
    }

    let mut first = 0usize;
    for &contour_end in &contour_ends {
        let last = usize::from(contour_end);
        if last < first || last >= point_count {
            return Err(err("contour end indices not monotonic"));
        }
        let points: Vec<Point> = (first..=last)
            .map(|i| Point {
                x: coords[i].0,
                y: coords[i].1,
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
            x: f64::midpoint(a.x, b.x),
            y: f64::midpoint(a.y, b.y),
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
                    x: f64::midpoint(c.x, p.x),
                    y: f64::midpoint(c.y, p.y),
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

/// One decoded component of a composite glyph: which glyph it places, its
/// placement offset in font units, and its 2×2 affine scale terms.
#[derive(Copy, Clone)]
struct Component {
    glyph: u16,
    dx: f64,
    dy: f64,
    a: f64,
    b: f64,
    c: f64,
    d: f64,
}

/// Decode a composite glyph's component records.
///
/// Only plain xy offsets are supported; point-matching args and Apple's
/// scaled-component-offset interpretation fail closed. The count is bounded so
/// a malformed record with the more-components flag stuck set cannot loop
/// without limit.
fn decode_components(r: &Reader<'_>, start: usize) -> Result<Vec<Component>, FontError> {
    let mut at = start + 10;
    let mut components = Vec::new();
    loop {
        let flags = r.u16(at)?;
        let glyph = r.u16(at + 2)?;
        at += 4;
        if flags & 0x0002 == 0 {
            return Err(err("composite point-matching args unsupported"));
        }
        // SCALED_COMPONENT_OFFSET (the Apple interpretation) would rescale the
        // offset by the component transform; the accepted faces use plain
        // font-unit offsets, so anything else is a wrong glyph waiting to
        // happen.
        if flags & 0x0800 != 0 {
            return Err(err("composite scaled component offset unsupported"));
        }
        let (dx, dy) = if flags & 0x0001 != 0 {
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
        components.push(Component {
            glyph,
            dx,
            dy,
            a: x_scale,
            b: scale01,
            c: scale10,
            d: y_scale,
        });
        if flags & 0x0020 == 0 {
            break;
        }
        if components.len() > usize::from(u16::MAX) {
            return Err(err("composite has too many components"));
        }
    }
    Ok(components)
}

/// Decode a composite glyph: recurse into each component with its affine
/// transform composed onto the sink. When the face is instanced, the `gvar`
/// deltas for the composite shift each component's placement offset before it
/// recurses (and the component itself is varied by its own `gvar` data).
fn outline_composite(
    face: &Face<'_>,
    glyph: u16,
    start: usize,
    sink: &mut OutlineSink,
    depth: u32,
) -> Result<(), FontError> {
    let components = decode_components(&face.r, start)?;
    let mut offsets: Vec<(f64, f64)> = components.iter().map(|c| (c.dx, c.dy)).collect();
    if let Some(var) = &face.var {
        let n = components.len();
        let mut deltas = vec![(0.0, 0.0); n + 4];
        if var
            .gvar
            .deltas(&face.r, &face.coords, glyph, n, None, &mut deltas)?
        {
            for (offset, delta) in offsets.iter_mut().zip(&deltas) {
                offset.0 += delta.0;
                offset.1 += delta.1;
            }
        }
    }
    for (component, &(dx, dy)) in components.iter().zip(&offsets) {
        let saved = sink.transform;
        sink.transform = saved.then(&Affine {
            a: component.a,
            b: component.b,
            c: component.c,
            d: component.d,
            dx,
            dy,
        });
        outline_glyph(face, component.glyph, sink, depth + 1)?;
        sink.transform = saved;
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
            crossings.extend(segments.iter().filter_map(|seg| crossing(seg, y)));
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
