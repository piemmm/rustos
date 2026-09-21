//! Supplying glyph geometry to a decoder that has no authority to fetch it.
//!
//! An SVG document is decoded inside the parser sandbox, which holds two
//! pipe ends and **nothing else** — no capabilities, so no way to call the
//! font endpoint. `lib/svg` resolves text through an injected provider, and
//! the provider a sandboxed decode gets is this one: it answers from a table
//! the host already sent, and *records* whatever the table does not hold.
//!
//! # Two rounds, and provably no more
//!
//! A decode that reaches text the table cannot answer completes anyway,
//! against placeholder geometry, so the recording is of the *whole*
//! document rather than of the first run that missed. The worker then
//! reports what it wanted instead of pixels; the host — a GUI process that
//! does hold a font client — fetches exactly that and sends it back; the
//! worker decodes again against the full table.
//!
//! That terminates in two rounds by construction: glyph geometry cannot
//! change which elements the walk visits or which scalars the text holds,
//! so the second decode asks for precisely what the first recorded. A
//! document with no text records nothing and costs one round, unchanged.
//!
//! The placeholder geometry is never drawn: a decode that recorded anything
//! is discarded whole and repeated.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;

use tairix_svg::font::{
    FaceId, FaceMetrics, FaceRequest, FontProvider, FontStyle, FontUnavailable, GlyphOutline,
    OutlineContour, OutlineSegment, MAX_GLYPH_POINTS,
};

use crate::wire::{Reader, WireError, Writer};

/// The most faces one document may ask for.
///
/// A fixed containment bound on what a hostile document can make the host
/// fetch on its behalf, checked on both sides of the pipe.
pub const MAX_FACES: usize = 32;

/// The most distinct scalars one document may ask for, across every face.
///
/// A fixed containment bound: the decoder's own glyph budget is larger, and
/// this is what bounds the *table* the host must build and send.
pub const MAX_SCALARS: usize = 4096;

/// Longest family name this exchange will carry.
pub const MAX_FAMILY_NAME: usize = 64;

/// The em a placeholder face reports.
///
/// Nothing is drawn from a placeholder — a decode that used one is thrown
/// away — so the value only has to be one a layout can divide by.
const PLACEHOLDER_EM: f64 = 1000.0;

/// One face a document asked for.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct FaceWant {
    /// The family name, exactly as the document spelled it.
    pub family: String,
    /// The `wght` coordinate.
    pub weight: u16,
    /// The posture, as its wire discriminant.
    pub style: u8,
    /// The `wdth` coordinate, in hundredths of a percent.
    pub stretch: u16,
    /// The distinct scalars this face was asked for, in ascending order.
    pub scalars: Vec<char>,
}

impl FaceWant {
    /// Whether `req` names this face.
    fn matches(&self, req: &FaceRequest<'_>) -> bool {
        self.family == req.family
            && self.weight == req.weight
            && self.style == style_wire(req.style)
            && self.stretch == req.stretch
    }
}

/// What the worker could not answer, and the host must fetch.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FontWants {
    /// The faces, in the order the document first asked for them.
    pub faces: Vec<FaceWant>,
}

impl FontWants {
    /// Whether anything at all is wanted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.faces.is_empty()
    }

    /// Encode the wants for the host.
    pub fn encode(&self, w: &mut Writer) {
        // Bounded by `MAX_FACES`/`MAX_SCALARS` before it is ever built, so
        // the counts fit their fields by construction; a count that somehow
        // did not would be refused by the decoder's own bound rather than
        // read short.
        w.u32(count(self.faces.len()));
        for face in &self.faces {
            w.str(&face.family);
            w.u32(u32::from(face.weight));
            w.u8(face.style);
            w.u32(u32::from(face.stretch));
            w.u32(count(face.scalars.len()));
            for scalar in &face.scalars {
                w.u32(*scalar as u32);
            }
        }
    }

    /// Decode wants a worker reported, refusing anything past the bounds.
    ///
    /// # Errors
    ///
    /// [`WireError::Malformed`] past a bound, for a scalar that is not a
    /// Unicode scalar value, or for a weight or width outside its field.
    pub fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        let count = r.u32()? as usize;
        if count > MAX_FACES {
            return Err(WireError::Malformed);
        }
        let mut faces = Vec::with_capacity(count);
        let mut total = 0_usize;
        for _ in 0..count {
            let family = r.string(MAX_FAMILY_NAME)?;
            let weight = u16::try_from(r.u32()?).map_err(|_| WireError::Malformed)?;
            let style = r.u8()?;
            let stretch = u16::try_from(r.u32()?).map_err(|_| WireError::Malformed)?;
            let scalars = read_scalars(r, MAX_SCALARS - total)?;
            total += scalars.len();
            faces.push(FaceWant {
                family,
                weight,
                style,
                stretch,
                scalars,
            });
        }
        Ok(Self { faces })
    }
}

/// Read a bounded, ascending run of scalars.
fn read_scalars(r: &mut Reader<'_>, room: usize) -> Result<Vec<char>, WireError> {
    let count = r.u32()? as usize;
    if count > room {
        return Err(WireError::Malformed);
    }
    let mut scalars = Vec::with_capacity(count);
    for _ in 0..count {
        let scalar = char::from_u32(r.u32()?).ok_or(WireError::Malformed)?;
        scalars.push(scalar);
    }
    Ok(scalars)
}

/// The glyph geometry the host fetched, keyed the way the decoder asks for
/// it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FontTable {
    faces: Vec<TableFace>,
}

/// One face in a supplied table.
#[derive(Clone, Debug, PartialEq)]
struct TableFace {
    want: FaceWant,
    metrics: FaceMetrics,
    glyphs: BTreeMap<char, GlyphOutline>,
}

impl FontTable {
    /// An empty table: the state a worker begins in, which answers nothing
    /// and records everything.
    #[must_use]
    pub const fn new() -> Self {
        Self { faces: Vec::new() }
    }

    /// Add one face's metrics and glyphs.
    ///
    /// # Errors
    ///
    /// [`WireError::Malformed`] past [`MAX_FACES`], or for metrics or a
    /// glyph the decoder could not use — a table is untrusted input to the
    /// worker exactly as a document is.
    pub fn push(
        &mut self,
        want: FaceWant,
        metrics: FaceMetrics,
        glyphs: Vec<(char, GlyphOutline)>,
    ) -> Result<(), WireError> {
        if self.faces.len() >= MAX_FACES || !metrics.is_usable() {
            return Err(WireError::Malformed);
        }
        let mut held = BTreeMap::new();
        for (scalar, glyph) in glyphs {
            if !glyph.is_drawable() {
                return Err(WireError::Malformed);
            }
            held.insert(scalar, glyph);
        }
        self.faces.push(TableFace {
            want,
            metrics,
            glyphs: held,
        });
        Ok(())
    }

    /// Encode the table for the worker.
    pub fn encode(&self, w: &mut Writer) {
        // Bounded by `push`, so the counts fit their fields.
        w.u32(count(self.faces.len()));
        for face in &self.faces {
            w.str(&face.want.family);
            w.u32(u32::from(face.want.weight));
            w.u8(face.want.style);
            w.u32(u32::from(face.want.stretch));
            w.i32(units(face.metrics.units_per_em));
            w.i32(units(face.metrics.ascent));
            w.i32(units(face.metrics.descent));
            w.i32(units(face.metrics.line_gap));
            w.u32(count(face.glyphs.len()));
            for (scalar, glyph) in &face.glyphs {
                w.u32(*scalar as u32);
                encode_glyph(w, glyph);
            }
        }
    }

    /// Decode a table the host supplied, refusing anything past a bound or
    /// anything the decoder could not draw.
    ///
    /// # Errors
    ///
    /// [`WireError::Malformed`] for a count past a bound, a scalar that is
    /// not a Unicode scalar value, an unusable face, or an undrawable
    /// glyph.
    pub fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        let count = r.u32()? as usize;
        if count > MAX_FACES {
            return Err(WireError::Malformed);
        }
        let mut table = Self::new();
        let mut total = 0_usize;
        for index in 0..count {
            let family = r.string(MAX_FAMILY_NAME)?;
            let weight = u16::try_from(r.u32()?).map_err(|_| WireError::Malformed)?;
            let style = r.u8()?;
            let stretch = u16::try_from(r.u32()?).map_err(|_| WireError::Malformed)?;
            let metrics = FaceMetrics {
                // The id is this provider's own index, not the host's, so a
                // supplied table can never name a face it did not send.
                id: FaceId::new(u32::try_from(index).map_err(|_| WireError::Malformed)?),
                units_per_em: f64::from(r.i32()?),
                ascent: f64::from(r.i32()?),
                descent: f64::from(r.i32()?),
                line_gap: f64::from(r.i32()?),
            };
            let glyph_count = r.u32()? as usize;
            if glyph_count > MAX_SCALARS - total {
                return Err(WireError::Malformed);
            }
            total += glyph_count;
            let mut glyphs = Vec::with_capacity(glyph_count);
            let mut scalars = Vec::with_capacity(glyph_count);
            for _ in 0..glyph_count {
                let scalar = char::from_u32(r.u32()?).ok_or(WireError::Malformed)?;
                scalars.push(scalar);
                glyphs.push((scalar, decode_glyph(r)?));
            }
            table.push(
                FaceWant {
                    family,
                    weight,
                    style,
                    stretch,
                    scalars,
                },
                metrics,
                glyphs,
            )?;
        }
        Ok(table)
    }

    /// The face `req` names, if the table holds it.
    fn face_of(&self, req: &FaceRequest<'_>) -> Option<(FaceId, &TableFace)> {
        self.faces.iter().enumerate().find_map(|(index, face)| {
            face.want
                .matches(req)
                .then(|| (FaceId::new(u32::try_from(index).unwrap_or(u32::MAX)), face))
        })
    }
}

/// A count on the wire.
///
/// Every count here is bounded well below `u32::MAX` before it is encoded;
/// one that somehow was not saturates, which the decoder's own bound then
/// refuses rather than reading a short record.
fn count(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}

/// A font measurement in font units, rounded to the whole units every face
/// states them in.
fn units(value: f64) -> i32 {
    if !value.is_finite() {
        return 0;
    }
    let rounded = if value < 0.0 {
        value - 0.5
    } else {
        value + 0.5
    };
    if rounded <= f64::from(i32::MIN) || rounded >= f64::from(i32::MAX) {
        return 0;
    }
    #[allow(
        clippy::cast_possible_truncation,
        reason = "held strictly inside the i32 range on the line above, so the \
                  truncation the lint warns about cannot occur"
    )]
    let raw = rounded as i32;
    raw
}

/// Fractional bits every outline coordinate crosses this pipe in, matching
/// the font protocol's own 26.6 form so nothing is re-rounded on the way.
const FRACTION_BITS: u32 = 6;

/// Raw steps per font unit.
const ONE: f64 = (1 << FRACTION_BITS) as f64;

/// A coordinate in the fixed-point form the pipe carries.
fn fixed(value: f64) -> i32 {
    units(value * ONE)
}

/// A coordinate back from the fixed-point form.
fn unfixed(raw: i32) -> f64 {
    f64::from(raw) / ONE
}

/// Wire discriminant of a straight segment.
const SEGMENT_LINE: u8 = 1;
/// Wire discriminant of a quadratic segment.
const SEGMENT_QUADRATIC: u8 = 2;

/// Encode one glyph's geometry.
fn encode_glyph(w: &mut Writer, glyph: &GlyphOutline) {
    w.i32(units(glyph.units_per_em));
    w.i32(fixed(glyph.advance));
    w.i32(fixed(glyph.synthetic_bold));
    w.i32(fixed(glyph.synthetic_shear));
    // Bounded by `MAX_GLYPH_POINTS`, which `is_drawable` enforced before a
    // glyph ever reached the table.
    w.u32(count(glyph.contours.len()));
    for contour in &glyph.contours {
        w.i32(fixed(contour.start.0));
        w.i32(fixed(contour.start.1));
        w.u32(count(contour.segments.len()));
        for segment in &contour.segments {
            match *segment {
                OutlineSegment::Line { to } => {
                    w.u8(SEGMENT_LINE);
                    w.i32(fixed(to.0));
                    w.i32(fixed(to.1));
                }
                OutlineSegment::Quadratic { control, to } => {
                    w.u8(SEGMENT_QUADRATIC);
                    w.i32(fixed(to.0));
                    w.i32(fixed(to.1));
                    w.i32(fixed(control.0));
                    w.i32(fixed(control.1));
                }
            }
        }
    }
}

/// Decode one glyph's geometry, refusing anything past the point bound.
fn decode_glyph(r: &mut Reader<'_>) -> Result<GlyphOutline, WireError> {
    let units_per_em = f64::from(r.i32()?);
    let advance = unfixed(r.i32()?);
    let synthetic_bold = unfixed(r.i32()?);
    let synthetic_shear = unfixed(r.i32()?);
    let count = r.u32()? as usize;
    if count > MAX_GLYPH_POINTS {
        return Err(WireError::Malformed);
    }
    let mut contours = Vec::with_capacity(count);
    let mut points = count;
    for _ in 0..count {
        let start = (unfixed(r.i32()?), unfixed(r.i32()?));
        let segments = r.u32()? as usize;
        points = points
            .checked_add(segments)
            .filter(|total| *total <= MAX_GLYPH_POINTS)
            .ok_or(WireError::Malformed)?;
        let mut run = Vec::with_capacity(segments);
        for _ in 0..segments {
            run.push(match r.u8()? {
                SEGMENT_LINE => OutlineSegment::Line {
                    to: (unfixed(r.i32()?), unfixed(r.i32()?)),
                },
                SEGMENT_QUADRATIC => {
                    let to = (unfixed(r.i32()?), unfixed(r.i32()?));
                    OutlineSegment::Quadratic {
                        control: (unfixed(r.i32()?), unfixed(r.i32()?)),
                        to,
                    }
                }
                _ => return Err(WireError::Malformed),
            });
        }
        contours.push(OutlineContour {
            start,
            segments: run,
        });
    }
    Ok(GlyphOutline {
        units_per_em,
        advance,
        synthetic_bold,
        synthetic_shear,
        contours,
    })
}

/// The wire discriminant of a posture.
const fn style_wire(style: FontStyle) -> u8 {
    match style {
        FontStyle::Normal => 1,
        FontStyle::Italic => 2,
        FontStyle::Oblique => 3,
    }
}

/// The posture a wire discriminant names.
#[must_use]
pub const fn style_of_wire(wire: u8) -> Option<FontStyle> {
    match wire {
        1 => Some(FontStyle::Normal),
        2 => Some(FontStyle::Italic),
        3 => Some(FontStyle::Oblique),
        _ => None,
    }
}

/// The provider a sandboxed decode runs against: the supplied table, and a
/// record of everything the table could not answer.
pub struct TableFonts<'t> {
    table: &'t FontTable,
    /// One face the table could not answer, and the scalars wanted from it.
    ///
    /// A set rather than a sorted vector: a `<text>` may want thousands of
    /// scalars, and inserting each into a sorted vector moves the tail
    /// every time. The set is drained in order once, when the record is
    /// handed over.
    recorded: Vec<(FaceWant, BTreeSet<char>)>,
    /// How many scalars have been recorded across every face, kept as a
    /// running total so the bound costs nothing to check.
    wanted: usize,
    /// Faces this decode resolved, so an id it issued names the same face
    /// on the way back.
    issued: Vec<Issued>,
}

/// One face this provider answered for.
#[derive(Clone, Debug)]
enum Issued {
    /// Served from the table, at that face's index.
    Held(usize),
    /// Recorded, at that index in [`FontWants::faces`].
    Wanted(usize),
}

impl<'t> TableFonts<'t> {
    /// A provider over `table`, recording whatever it cannot answer.
    #[must_use]
    pub const fn new(table: &'t FontTable) -> Self {
        Self {
            table,
            recorded: Vec::new(),
            wanted: 0,
            issued: Vec::new(),
        }
    }

    /// What the decode asked for that the table did not hold.
    #[must_use]
    pub fn into_wants(self) -> FontWants {
        FontWants {
            faces: self
                .recorded
                .into_iter()
                .map(|(want, scalars)| FaceWant {
                    scalars: scalars.into_iter().collect(),
                    ..want
                })
                .collect(),
        }
    }

    /// Record `scalar` against the recorded face at `index`, keeping the
    /// list sorted and free of repeats so the host fetches each glyph once.
    fn want(&mut self, index: usize, scalar: char) -> Result<(), FontUnavailable> {
        if self.wanted >= MAX_SCALARS {
            return Err(FontUnavailable);
        }
        let (_, scalars) = self.recorded.get_mut(index).ok_or(FontUnavailable)?;
        if scalars.insert(scalar) {
            self.wanted += 1;
        }
        Ok(())
    }
}

impl FontProvider for TableFonts<'_> {
    fn select(&mut self, req: &FaceRequest<'_>) -> Result<FaceMetrics, FontUnavailable> {
        if let Some((id, face)) = self.table.face_of(req) {
            let metrics = FaceMetrics { id, ..face.metrics };
            let handle =
                FaceId::new(u32::try_from(self.issued.len()).map_err(|_| FontUnavailable)?);
            let index = usize::try_from(id.get()).map_err(|_| FontUnavailable)?;
            self.issued.push(Issued::Held(index));
            return Ok(FaceMetrics {
                id: handle,
                ..metrics
            });
        }
        // Not held: record it and answer placeholder metrics, so the walk
        // reaches every remaining `<text>` and the record is of the whole
        // document rather than of the first run that missed.
        let held = self.recorded.iter().position(|(face, _)| face.matches(req));
        let index = if let Some(index) = held {
            index
        } else {
            if self.recorded.len() >= MAX_FACES {
                return Err(FontUnavailable);
            }
            self.recorded.push((
                FaceWant {
                    family: String::from(req.family),
                    weight: req.weight,
                    style: style_wire(req.style),
                    stretch: req.stretch,
                    scalars: Vec::new(),
                },
                BTreeSet::new(),
            ));
            self.recorded.len() - 1
        };
        let handle = FaceId::new(u32::try_from(self.issued.len()).map_err(|_| FontUnavailable)?);
        self.issued.push(Issued::Wanted(index));
        Ok(FaceMetrics {
            id: handle,
            units_per_em: PLACEHOLDER_EM,
            ascent: PLACEHOLDER_EM,
            descent: 0.0,
            line_gap: 0.0,
        })
    }

    fn outlines(
        &mut self,
        face: FaceId,
        run: &[char],
        out: &mut Vec<GlyphOutline>,
    ) -> Result<(), FontUnavailable> {
        let at = usize::try_from(face.get()).map_err(|_| FontUnavailable)?;
        let issued = self.issued.get(at).cloned().ok_or(FontUnavailable)?;
        match issued {
            Issued::Held(index) => {
                let held = self.table.faces.get(index).ok_or(FontUnavailable)?;
                // A table missing a scalar of a run it was supplied for is a
                // table that cannot serve this document; recording it here
                // would ask for a third round, so it is refused instead.
                for scalar in run {
                    out.push(held.glyphs.get(scalar).ok_or(FontUnavailable)?.clone());
                }
                Ok(())
            }
            Issued::Wanted(index) => {
                for scalar in run {
                    self.want(index, *scalar)?;
                    out.push(GlyphOutline {
                        units_per_em: PLACEHOLDER_EM,
                        ..GlyphOutline::default()
                    });
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
#[path = "svgfonts_tests.rs"]
mod tests;
