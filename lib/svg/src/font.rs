//! The injected font seam: how this crate obtains a face and a glyph's
//! geometry without holding any font data or any authority of its own.
//!
//! SVG text is filled, stroked, gradient-painted, clipped, masked and
//! transformed exactly as a `<path>` is, and the picture this crate produces
//! has no resolution. So a glyph must arrive as **contours**, join the one
//! geometry currency, and be flattened by the single step every other curve
//! takes. A rasterised cell would fix a size at decode time and put text on a
//! second rasterisation path.
//!
//! This crate is `no_std` and has no file access: the faces live behind the
//! OS font service. [`FontProvider`] is therefore injected by the caller and
//! carries whatever authority *that* caller already holds — modelled on the
//! help engine's read seam. A caller with none supplies [`NoFonts`] and
//! decodes documents without text, rather than the decoder acquiring ambient
//! authority to draw them.
//!
//! # A provider is not trusted
//!
//! A provider may be a live IPC client, a sandboxed exchange, or a test
//! double, so what it hands back is checked like any other input: an em of
//! zero, a non-finite coordinate, or a glyph past [`MAX_GLYPH_POINTS`]
//! refuses the glyph rather than reaching the geometry.

use alloc::vec::Vec;

/// The most outline points one glyph may contribute.
///
/// A fixed containment bound, not a capacity: it matches the bound the font
/// engine itself decodes a glyph under, so a provider cannot hand back more
/// geometry for one character than a face could ever hold. The points are
/// additionally charged against the document's total vertex budget, so a
/// page of text cannot outspend a page of paths.
pub const MAX_GLYPH_POINTS: usize = 8192;

/// A face the provider has resolved and will answer outline requests for.
///
/// Opaque: what it names is the provider's business, and a decoder holding
/// one has no way to reach a face the provider did not give it.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct FaceId(u32);

impl FaceId {
    /// The face a provider names `id`.
    #[must_use]
    pub const fn new(id: u32) -> Self {
        Self(id)
    }

    /// The provider's own name for this face.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// The posture a run of text is set in.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum FontStyle {
    /// Upright.
    #[default]
    Normal,
    /// The designer's own cursive letterforms.
    Italic,
    /// Upright letterforms slanted, with no change of shape.
    Oblique,
}

/// One face the document asks for: a single family name and the design-axis
/// coordinates to set it at.
///
/// A single family, because `font-family` is a *list* and walking it is the
/// cascade's job — the decoder asks for each name in turn and falls through
/// to the generic when none answers. The provider's job is to answer for one
/// name or say it cannot.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct FaceRequest<'a> {
    /// The family name, which may be one of CSS's generic names.
    pub family: &'a str,
    /// The `wght` axis coordinate, `1..=1000`.
    pub weight: u16,
    /// The posture.
    pub style: FontStyle,
    /// The `wdth` axis coordinate, in hundredths of a percent of normal.
    pub stretch: u16,
}

/// What a resolved face measures, in its own font units.
///
/// Everything a run's layout needs before a single glyph is fetched, so the
/// baseline and the line box are settled once for the whole run rather than
/// drifting with whichever face each scalar resolved to.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct FaceMetrics {
    /// The face this request resolved to.
    pub id: FaceId,
    /// Font units per em. A caller scales by `font-size / units_per_em`.
    pub units_per_em: f64,
    /// The ascender, in font units.
    pub ascent: f64,
    /// The descender magnitude, in font units (positive).
    pub descent: f64,
    /// Extra leading between lines, in font units.
    pub line_gap: f64,
}

impl FaceMetrics {
    /// Whether these metrics can be laid text out with.
    ///
    /// An em of zero or a non-finite measurement would divide the layout by
    /// nothing, so a provider answering one is treated as answering nothing.
    #[must_use]
    pub fn is_usable(&self) -> bool {
        self.units_per_em.is_finite()
            && self.units_per_em > 0.0
            && self.ascent.is_finite()
            && self.descent.is_finite()
            && self.line_gap.is_finite()
    }
}

/// One segment of a glyph contour, continuing from the previous point.
///
/// Quadratics come through whole: how finely a curve must be flattened
/// depends on the size it is finally drawn at, which only the placement
/// knows, so a glyph subdivides exactly as a `<path>` of the same shape.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum OutlineSegment {
    /// A straight line.
    Line {
        /// Where the segment ends.
        to: (f64, f64),
    },
    /// A quadratic Bézier.
    Quadratic {
        /// The off-curve control point.
        control: (f64, f64),
        /// Where the segment ends.
        to: (f64, f64),
    },
}

impl OutlineSegment {
    /// Whether every coordinate is finite, which is what keeps a provider's
    /// answer from reaching the geometry as a `NaN`.
    fn is_finite(&self) -> bool {
        let finite = |p: (f64, f64)| p.0.is_finite() && p.1.is_finite();
        match *self {
            Self::Line { to } => finite(to),
            Self::Quadratic { control, to } => finite(control) && finite(to),
        }
    }
}

/// One closed contour of a glyph outline, in the face's font units with y up.
///
/// Contours fill by **non-zero winding**: a counter is a contour wound
/// against the one enclosing it rather than a shape in its own right, so a
/// glyph's contours are filled *together* under that rule.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OutlineContour {
    /// Where the contour begins, and where its last segment returns to.
    pub start: (f64, f64),
    /// The segments, in order.
    pub segments: Vec<OutlineSegment>,
}

/// One glyph as the provider hands it over.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GlyphOutline {
    /// The **resolved** face's em, which a per-scalar fallback may differ in
    /// from the family's primary.
    pub units_per_em: f64,
    /// The pen advance in that face's font units.
    pub advance: f64,
    /// A synthetic bold stroke the face could not furnish, as a fraction of
    /// the em. Zero where the face rendered the weight itself.
    pub synthetic_bold: f64,
    /// A synthetic oblique shear the face could not furnish — the horizontal
    /// offset per unit of height. Zero where the face rendered the posture
    /// itself.
    pub synthetic_shear: f64,
    /// The glyph's closed contours. A glyph with no ink — a space — has
    /// none, and is drawn by advancing the pen.
    pub contours: Vec<OutlineContour>,
}

impl GlyphOutline {
    /// How many points the glyph states, contours and segments together.
    #[must_use]
    pub fn points(&self) -> usize {
        self.contours
            .iter()
            .map(|contour| 1 + contour.segments.len())
            .sum()
    }

    /// Whether the glyph can be drawn: an em to scale by, finite metrics and
    /// coordinates throughout, and no more geometry than one character may
    /// contribute.
    #[must_use]
    pub fn is_drawable(&self) -> bool {
        self.units_per_em.is_finite()
            && self.units_per_em > 0.0
            && self.advance.is_finite()
            && self.synthetic_bold.is_finite()
            && self.synthetic_bold >= 0.0
            && self.synthetic_shear.is_finite()
            && self.points() <= MAX_GLYPH_POINTS
            && self.contours.iter().all(|contour| {
                contour.start.0.is_finite()
                    && contour.start.1.is_finite()
                    && contour.segments.iter().all(OutlineSegment::is_finite)
            })
    }
}

/// The provider could not furnish what was asked for.
///
/// One outcome rather than a reason code: a decoder's only response is to
/// try the next family and finally to refuse the document, and a reason it
/// cannot act on would be noise in the error the caller sees.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct FontUnavailable;

/// The injected, capability-scoped seam a document's text is resolved
/// through.
///
/// Two operations and no ambient authority: name a face, then ask that face
/// for a run of glyphs. An implementation may be a live font-service client,
/// a two-phase exchange across a sandbox boundary, or a table already
/// gathered — the decoder cannot tell and does not care.
pub trait FontProvider {
    /// Resolve `req` to a face, or report that this provider cannot furnish
    /// it.
    ///
    /// # Errors
    ///
    /// [`FontUnavailable`] when no face answers the request.
    fn select(&mut self, req: &FaceRequest<'_>) -> Result<FaceMetrics, FontUnavailable>;

    /// Append `run`'s glyphs, in order, to `out`.
    ///
    /// Exactly `run.len()` outlines are appended on success; a provider that
    /// appends any other number has answered a run the decoder did not ask
    /// for, and the decoder refuses it.
    ///
    /// # Errors
    ///
    /// [`FontUnavailable`] when the face cannot furnish the run.
    fn outlines(
        &mut self,
        face: FaceId,
        run: &[char],
        out: &mut Vec<GlyphOutline>,
    ) -> Result<(), FontUnavailable>;
}

/// The provider a caller with no font authority supplies.
///
/// It furnishes nothing, so a document carrying text is refused rather than
/// drawn without its lettering: absent lettering is a wrong picture, not a
/// missing decoration. A document with no text decodes exactly as before.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct NoFonts;

impl FontProvider for NoFonts {
    fn select(&mut self, _req: &FaceRequest<'_>) -> Result<FaceMetrics, FontUnavailable> {
        Err(FontUnavailable)
    }

    fn outlines(
        &mut self,
        _face: FaceId,
        _run: &[char],
        _out: &mut Vec<GlyphOutline>,
    ) -> Result<(), FontUnavailable> {
        Err(FontUnavailable)
    }
}

#[cfg(test)]
#[path = "font_tests.rs"]
mod tests;
