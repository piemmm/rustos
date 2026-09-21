//! `<text>` and `<tspan>`: turning a run of characters into the same
//! flattened contours every other shape in this crate becomes.
//!
//! A glyph reaches this crate as geometry (`crate::font`), so text is drawn
//! by the path the rest of the decoder already has: scale the face's outline
//! into user space, place it where the layout resolved, flatten its
//! quadratics at the tolerance the *placement* resolves, and fill the run's
//! contours together under the non-zero rule. A glyph is therefore
//! subdivided exactly as a `<path>` of the same shape, and text is filled,
//! stroked, clipped, masked and transformed like any other shape.
//!
//! # What the layout is
//!
//! SVG addresses *characters*, not glyphs: a `<text>` and each `<tspan>`
//! carry their own `x`/`y`/`dx`/`dy`/`rotate` lists, and the *i*-th
//! addressable character of an element takes that element's *i*-th value —
//! including characters its descendants contributed. An absolute `x` or `y`
//! begins a new **chunk**, which is the unit `text-anchor` shifts. White
//! space is collapsed across the whole `<text>` before any of that, because
//! `<text>a <tspan> b</tspan></text>` renders `a b` and no per-element pass
//! can see the space that spans the boundary.
//!
//! # Two passes, because the cascade and the provider cannot be borrowed at
//! once
//!
//! [`collect`] walks the subtree resolving styles through the decoder's own
//! cascade and asks the provider for nothing. [`lay_out`] then resolves one
//! face per *distinct* style, fetches advances, and places every glyph. The
//! split is what lets the decoder lend its cascade and then its provider in
//! turn, and it keeps the layout arithmetic a pure function of what was
//! collected.

use alloc::vec;
use alloc::vec::Vec;

use tairix_raster::Affine;
use tairix_util::mathf::sqrt;

use crate::error::SvgError;
use crate::font::{
    FaceMetrics, FaceRequest, FontProvider, FontUnavailable, GlyphOutline, OutlineSegment,
};
use crate::geom::{Point, SubPath};
use crate::number::parse_length;
use crate::pathdata::flatten_quadratic;
use crate::style::{parse_length_in, Lengths, Style, TextAnchor};
use crate::xml::{Content, Element};

/// The deepest `<tspan>` nesting accepted inside one `<text>`.
///
/// A fixed containment bound: real lettering nests a level or two to change
/// a fill or shift a position, and the limit is what stops a document of a
/// thousand opened spans from growing the walk without end.
pub(crate) const MAX_TSPAN_DEPTH: usize = 16;

/// The most characters one `<text>` element may hold.
///
/// A fixed containment bound, charged as the characters are collected, so a
/// hostile document cannot make one element lay out a megabyte of lettering.
pub(crate) const MAX_TEXT_LENGTH: usize = 4096;

/// The most glyphs one document may draw, across every `<text>` in it.
///
/// The outline equivalent of the layer budget: it is what stops a document
/// of a thousand short labels from costing what one bounded label promises
/// not to.
pub(crate) const MAX_DOCUMENT_GLYPHS: usize = 16_384;

/// The most glyph runs one document may emit.
///
/// A run becomes a layer, so this also holds the layer budget: without it a
/// document alternating fills character by character would turn one label
/// into thousands of layers.
pub(crate) const MAX_TEXT_RUNS: usize = 1024;

/// The most requests one document may make of the font provider.
///
/// A provider may be a live service across an IPC boundary, so this bounds
/// *round trips* rather than output: without it a hostile document could
/// turn one decode into thousands of calls on a service serving the whole
/// desktop. A fixed containment bound, not a capacity.
pub(crate) const MAX_PROVIDER_REQUESTS: usize = 256;

/// How many scalars one provider request asks for, matched to the run a
/// glyph protocol answers in one round trip.
const PROVIDER_RUN: usize = 32;

/// The most values one position list may carry: a list longer than the text
/// it addresses can only be padding, and the text is itself bounded.
const MAX_POSITION_VALUES: usize = MAX_TEXT_LENGTH;

/// What text is allowed to spend across one whole document.
#[derive(Copy, Clone, Debug)]
pub(crate) struct TextBudget {
    glyphs: usize,
    runs: usize,
    requests: usize,
}

impl Default for TextBudget {
    fn default() -> Self {
        Self {
            glyphs: MAX_DOCUMENT_GLYPHS,
            runs: MAX_TEXT_RUNS,
            requests: MAX_PROVIDER_REQUESTS,
        }
    }
}

impl TextBudget {
    /// Charge `count` glyphs.
    fn glyphs(&mut self, count: usize) -> Result<(), SvgError> {
        self.glyphs = self.glyphs.checked_sub(count).ok_or(SvgError::TooComplex)?;
        Ok(())
    }

    /// Charge one emitted run.
    pub(crate) fn run(&mut self) -> Result<(), SvgError> {
        self.runs = self.runs.checked_sub(1).ok_or(SvgError::TooComplex)?;
        Ok(())
    }

    /// Charge one provider call.
    fn request(&mut self) -> Result<(), SvgError> {
        self.requests = self.requests.checked_sub(1).ok_or(SvgError::TooComplex)?;
        Ok(())
    }
}

/// How the decoder's own property cascade is lent to the text walk.
///
/// The walk needs a resolved [`Style`] per element, which only the decoder
/// can produce — it holds the stylesheet and the selector path. Lending it
/// as a trait keeps the cascade in one place and this module free of it.
pub(crate) trait TextCascade<'a> {
    /// Put `element` on the selector path and resolve its style.
    ///
    /// # Errors
    /// Whatever the cascade raises for a malformed property value.
    fn enter(&mut self, element: &'a Element<'a>, inherited: &Style) -> Result<Style, SvgError>;

    /// Take the last entered element off the selector path.
    fn leave(&mut self);
}

/// How one element's white space is treated.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
enum Space {
    /// `xml:space="default"`: newlines dropped, tabs become spaces, leading
    /// and trailing space stripped, runs of space collapsed to one.
    #[default]
    Collapse,
    /// `xml:space="preserve"`: newlines and tabs become spaces, everything
    /// else kept exactly.
    Preserve,
}

impl Space {
    /// The treatment `element` states, or the one it inherits.
    fn of(element: &Element<'_>, inherited: Self) -> Self {
        match element.attr("xml:space") {
            Some("preserve") => Self::Preserve,
            Some("default") => Self::Collapse,
            _ => inherited,
        }
    }
}

/// One collected character and everything that decides where it goes.
#[derive(Clone, Debug)]
struct Placed {
    scalar: char,
    /// Which resolved style it is set in.
    run: usize,
    /// Whether the element it came from preserves white space.
    preserve: bool,
    /// An absolute position it is moved to, per axis.
    absolute: (Option<f64>, Option<f64>),
    /// A relative shift applied after any absolute position.
    shift: (f64, f64),
    /// How far the glyph is turned, in degrees.
    rotate: f64,
}

/// A `textLength` an element states over the characters it contributed.
#[derive(Copy, Clone, Debug)]
struct LengthSpan {
    target: f64,
    /// Whether the glyphs stretch with the spacing.
    glyphs: bool,
    from: usize,
    to: usize,
}

/// One `<text>` subtree flattened into characters and the styles they are
/// set in, before any face is resolved.
pub(crate) struct Collected {
    chars: Vec<Placed>,
    styles: Vec<Style>,
    spans: Vec<LengthSpan>,
}

/// One glyph placed in user space.
#[derive(Clone, Debug)]
pub(crate) struct PlacedGlyph {
    /// Which style run fills it.
    pub(crate) run: usize,
    /// Font units to user space: the em scale and y flip, the glyph's own
    /// rotation, any synthetic oblique shear, and the pen position.
    pub(crate) to_user: Affine,
    /// The synthetic bold stroke this glyph's face could not furnish, as a
    /// fraction of the em.
    pub(crate) synthetic_bold: f64,
}

/// A laid-out `<text>`: the glyphs in document order, the style each is
/// painted with, and the outline each was measured from.
pub(crate) struct Laid {
    pub(crate) glyphs: Vec<PlacedGlyph>,
    pub(crate) styles: Vec<Style>,
    pub(crate) outlines: Vec<GlyphOutline>,
}

/// Walk `element`'s subtree, resolving each `<tspan>`'s style through
/// `cascade`, and flatten it into characters.
///
/// `inherited` is the `<text>` element's *own* resolved style: the caller
/// has already entered it.
///
/// Asks the provider for nothing: the face a run is set in is resolved in
/// [`lay_out`], once per distinct style.
///
/// # Errors
///
/// [`SvgError::TooComplex`] past the nesting or length bound, and whatever
/// the cascade or a malformed position list raises.
pub(crate) fn collect<'a>(
    element: &'a Element<'a>,
    inherited: &Style,
    viewport: (f64, f64),
    cascade: &mut dyn TextCascade<'a>,
) -> Result<Collected, SvgError> {
    let mut out = Collected {
        chars: Vec::new(),
        styles: Vec::new(),
        spans: Vec::new(),
    };
    // The caller already has the `<text>` element on its selector path and
    // has resolved its style, so the root is not entered a second time; only
    // its `<tspan>` descendants are.
    gather_content(
        element,
        inherited,
        Space::Collapse,
        0,
        viewport,
        cascade,
        &mut out,
    )?;
    collapse(&mut out);
    Ok(out)
}

/// Collect one `<text>` or `<tspan>` and its content.
fn gather<'a>(
    element: &'a Element<'a>,
    inherited: &Style,
    space: Space,
    depth: usize,
    viewport: (f64, f64),
    cascade: &mut dyn TextCascade<'a>,
    out: &mut Collected,
) -> Result<(), SvgError> {
    if depth > MAX_TSPAN_DEPTH {
        return Err(SvgError::TooComplex);
    }
    let style = cascade.enter(element, inherited)?;
    let gathered = gather_content(element, &style, space, depth, viewport, cascade, out);
    cascade.leave();
    gathered
}

/// The body of [`gather`], with `element` already on the selector path.
fn gather_content<'a>(
    element: &'a Element<'a>,
    style: &Style,
    space: Space,
    depth: usize,
    viewport: (f64, f64),
    cascade: &mut dyn TextCascade<'a>,
    out: &mut Collected,
) -> Result<(), SvgError> {
    let space = Space::of(element, space);
    let preserve = space == Space::Preserve;
    let run = out.styles.len();
    out.styles.push(style.clone());
    let positions = Positions::read(element, viewport)?;
    let from = out.chars.len();
    let mut taken = 0_usize;
    let inherited = style.inherit();
    for node in &element.content {
        match node {
            Content::Text(text) => {
                for scalar in text.chars() {
                    if out.chars.len() >= MAX_TEXT_LENGTH {
                        return Err(SvgError::TooComplex);
                    }
                    out.chars.push(Placed {
                        scalar,
                        run,
                        preserve,
                        absolute: positions.absolute(taken),
                        shift: positions.shift(taken),
                        rotate: positions.rotate(taken),
                    });
                    taken += 1;
                }
            }
            // Only `<tspan>` contributes characters; anything else inside a
            // `<text>` is content this decoder cannot draw and is skipped,
            // exactly as it is anywhere else in the tree.
            Content::Element(child) if child.name == "tspan" => {
                let before = out.chars.len();
                gather(child, &inherited, space, depth + 1, viewport, cascade, out)?;
                // A descendant's characters are addressable by *this*
                // element's lists too, so they advance its cursor and take
                // whichever of its values they line up with — but never over
                // a value the descendant stated for itself.
                for index in before..out.chars.len() {
                    adopt(&mut out.chars[index], &positions, taken);
                    taken += 1;
                }
            }
            Content::Element(_) => {}
        }
    }
    record_length(element, style, viewport, from, out)
}

/// Apply an ancestor's position lists to a character a descendant
/// contributed.
fn adopt(placed: &mut Placed, positions: &Positions, at: usize) {
    let (x, y) = positions.absolute(at);
    placed.absolute.0 = placed.absolute.0.or(x);
    placed.absolute.1 = placed.absolute.1.or(y);
    let (dx, dy) = positions.shift(at);
    placed.shift.0 += dx;
    placed.shift.1 += dy;
}

/// Record the `textLength` an element states over the characters it
/// contributed.
fn record_length(
    element: &Element<'_>,
    style: &Style,
    viewport: (f64, f64),
    from: usize,
    out: &mut Collected,
) -> Result<(), SvgError> {
    let Some(text) = element.attr("textLength") else {
        return Ok(());
    };
    let lengths = Lengths {
        viewport: viewport_diagonal(viewport),
        font_size: style.font_size,
    };
    let target = parse_length_in(text, lengths)?;
    if !target.is_finite() || target < 0.0 {
        return Err(SvgError::InvalidNumber);
    }
    let glyphs = match element.attr("lengthAdjust") {
        Some("spacingAndGlyphs") => true,
        Some("spacing") | None => false,
        Some(_) => return Err(SvgError::InvalidNumber),
    };
    out.spans.push(LengthSpan {
        target,
        glyphs,
        from,
        to: out.chars.len(),
    });
    Ok(())
}

/// One element's `x`/`y`/`dx`/`dy`/`rotate` lists.
#[derive(Default)]
struct Positions {
    x: Vec<f64>,
    y: Vec<f64>,
    dx: Vec<f64>,
    dy: Vec<f64>,
    rotate: Vec<f64>,
}

impl Positions {
    /// Read the lists `element` states.
    fn read(element: &Element<'_>, viewport: (f64, f64)) -> Result<Self, SvgError> {
        let axis = |name: &str, basis: f64| match element.attr(name) {
            Some(text) => length_list(text, basis),
            None => Ok(Vec::new()),
        };
        Ok(Self {
            x: axis("x", viewport.0)?,
            y: axis("y", viewport.1)?,
            dx: axis("dx", viewport.0)?,
            dy: axis("dy", viewport.1)?,
            // An angle carries no unit and resolves against no length.
            rotate: axis("rotate", 0.0)?,
        })
    }

    /// The absolute position the `at`-th character takes.
    fn absolute(&self, at: usize) -> (Option<f64>, Option<f64>) {
        (self.x.get(at).copied(), self.y.get(at).copied())
    }

    /// The relative shift the `at`-th character takes.
    fn shift(&self, at: usize) -> (f64, f64) {
        (
            self.dx.get(at).copied().unwrap_or(0.0),
            self.dy.get(at).copied().unwrap_or(0.0),
        )
    }

    /// The rotation the `at`-th character takes. The last value persists for
    /// every character after it, which is what SVG says of `rotate`.
    fn rotate(&self, at: usize) -> f64 {
        self.rotate
            .get(at)
            .or_else(|| self.rotate.last())
            .copied()
            .unwrap_or(0.0)
    }
}

/// Parse a whitespace- or comma-separated list of lengths.
fn length_list(text: &str, basis: f64) -> Result<Vec<f64>, SvgError> {
    let mut out = Vec::new();
    for token in text.split([',', ' ', '\t', '\r', '\n']) {
        if token.is_empty() {
            continue;
        }
        if out.len() == MAX_POSITION_VALUES {
            return Err(SvgError::TooComplex);
        }
        let value = parse_length(token, basis)?;
        if !value.is_finite() {
            return Err(SvgError::InvalidNumber);
        }
        out.push(value);
    }
    Ok(out)
}

/// Apply SVG's white-space rules across the whole element's characters.
///
/// Collapsing spans element boundaries, so it happens once over the flat
/// list rather than per `<tspan>`.
fn collapse(collected: &mut Collected) {
    let mut out: Vec<Placed> = Vec::with_capacity(collected.chars.len());
    for placed in collected.chars.drain(..) {
        let scalar = match placed.scalar {
            '\n' | '\r' if !placed.preserve => continue,
            '\n' | '\r' | '\t' => ' ',
            other => other,
        };
        // A run of spaces is one space, and a leading one is none.
        if scalar == ' ' && !placed.preserve && out.last().is_none_or(|prev| prev.scalar == ' ') {
            continue;
        }
        out.push(Placed { scalar, ..placed });
    }
    while out
        .last()
        .is_some_and(|last| last.scalar == ' ' && !last.preserve)
    {
        out.pop();
    }
    for span in &mut collected.spans {
        span.from = span.from.min(out.len());
        span.to = span.to.min(out.len());
    }
    collected.chars = out;
}

/// Resolve faces, fetch glyphs, and place every character.
///
/// `Ok(None)` when the element resolved to no drawable character at all — an
/// empty `<text>`, or one holding nothing but collapsed white space.
///
/// # Errors
///
/// * [`SvgError::FontUnavailable`] when no family a run names, nor the
///   generic it falls through to, can be furnished, or the provider answers
///   a run the decoder did not ask for.
/// * [`SvgError::TooComplex`] once a bound is passed.
pub(crate) fn lay_out(
    collected: Collected,
    provider: &mut dyn FontProvider,
    budget: &mut TextBudget,
) -> Result<Option<Laid>, SvgError> {
    if collected.chars.is_empty() {
        return Ok(None);
    }
    let faces = resolve_faces(&collected.styles, provider, budget)?;
    let outlines = fetch_outlines(&collected, &faces, provider, budget)?;
    Ok(Some(place(collected, outlines)))
}

/// One resolved face per style, sharing the resolution between styles whose
/// font properties agree — which is every `<tspan>` that changed only a
/// fill, so a label costs one round trip rather than one per span.
fn resolve_faces(
    styles: &[Style],
    provider: &mut dyn FontProvider,
    budget: &mut TextBudget,
) -> Result<Vec<FaceMetrics>, SvgError> {
    let mut faces: Vec<FaceMetrics> = Vec::with_capacity(styles.len());
    // Compared against the *distinct* faces resolved so far rather than
    // against every earlier style: a document's spans run to thousands
    // while the faces they ask for are a handful, so this is linear in the
    // spans where the pairwise scan was quadratic.
    let mut distinct: Vec<(usize, FaceMetrics)> = Vec::new();
    for style in styles {
        let shared = distinct
            .iter()
            .find(|(rep, _)| styles.get(*rep).is_some_and(|held| same_face(held, style)))
            .map(|(_, face)| *face);
        let face = if let Some(face) = shared {
            face
        } else {
            let resolved = select(style, provider, budget)?;
            distinct.push((faces.len(), resolved));
            resolved
        };
        faces.push(face);
    }
    Ok(faces)
}

/// Whether two styles ask for the same face.
fn same_face(a: &Style, b: &Style) -> bool {
    a.font_family == b.font_family
        && a.font_weight == b.font_weight
        && a.font_style == b.font_style
        && a.font_stretch == b.font_stretch
}

/// Walk one style's `font-family` list and fall through to the generic every
/// store is expected to answer.
fn select(
    style: &Style,
    provider: &mut dyn FontProvider,
    budget: &mut TextBudget,
) -> Result<FaceMetrics, SvgError> {
    let list = style.font_family.as_deref().unwrap_or("");
    for family in family_list(list) {
        if let Some(face) = try_face(style, family, provider, budget)? {
            return Ok(face);
        }
    }
    // SVG's initial `font-family` is the UA's own, and the generic a desktop
    // always has is the last rung of the ladder.
    match try_face(style, GENERIC_FALLBACK, provider, budget)? {
        Some(face) => Ok(face),
        None => Err(SvgError::FontUnavailable),
    }
}

/// The generic family the ladder ends at.
const GENERIC_FALLBACK: &str = "sans-serif";

/// Ask the provider for one family, charging the call.
fn try_face(
    style: &Style,
    family: &str,
    provider: &mut dyn FontProvider,
    budget: &mut TextBudget,
) -> Result<Option<FaceMetrics>, SvgError> {
    budget.request()?;
    let request = FaceRequest {
        family,
        weight: style.font_weight,
        style: style.font_style,
        stretch: style.font_stretch,
    };
    match provider.select(&request) {
        // Metrics nothing can be laid out with are no answer at all, so the
        // ladder moves on rather than dividing the layout by zero.
        Ok(face) if face.is_usable() => Ok(Some(face)),
        Ok(_) | Err(FontUnavailable) => Ok(None),
    }
}

/// The families a `font-family` list names, in order, unquoted.
fn family_list(list: &str) -> impl Iterator<Item = &str> {
    list.split(',')
        .map(|name| name.trim().trim_matches(['"', '\'']).trim())
        .filter(|name| !name.is_empty())
}

/// The length a percentage with no axis of its own resolves against.
fn viewport_diagonal(viewport: (f64, f64)) -> f64 {
    sqrt(f64::midpoint(
        viewport.0 * viewport.0,
        viewport.1 * viewport.1,
    ))
}

/// Fetch every character's outline, a bounded run at a time.
fn fetch_outlines(
    collected: &Collected,
    faces: &[FaceMetrics],
    provider: &mut dyn FontProvider,
    budget: &mut TextBudget,
) -> Result<Vec<GlyphOutline>, SvgError> {
    budget.glyphs(collected.chars.len())?;
    let mut outlines = Vec::with_capacity(collected.chars.len());
    let mut at = 0;
    while at < collected.chars.len() {
        let run = collected.chars[at].run;
        // Capped before the scan, not after: a whole `<text>` of one style
        // would otherwise have every batch walk the entire remainder only
        // to discard all but the first `PROVIDER_RUN` of it.
        let limit = collected.chars.len().min(at + PROVIDER_RUN);
        let end = collected.chars[at..limit]
            .iter()
            .position(|placed| placed.run != run)
            .map_or(limit, |offset| at + offset);
        let scalars: Vec<char> = collected.chars[at..end]
            .iter()
            .map(|placed| placed.scalar)
            .collect();
        let face = faces.get(run).ok_or(SvgError::Malformed)?;
        budget.request()?;
        let before = outlines.len();
        provider
            .outlines(face.id, &scalars, &mut outlines)
            .map_err(|FontUnavailable| SvgError::FontUnavailable)?;
        // A provider that answers a different run than the one asked for has
        // not answered it: the decoder refuses rather than pairing glyphs
        // with characters it cannot know line up.
        if outlines.len() != before + scalars.len()
            || !outlines[before..].iter().all(GlyphOutline::is_drawable)
        {
            return Err(SvgError::FontUnavailable);
        }
        at = end;
    }
    Ok(outlines)
}

/// Resolve every pen position, anchor each chunk, apply any `textLength`,
/// and build each glyph's transform.
fn place(collected: Collected, outlines: Vec<GlyphOutline>) -> Laid {
    let Collected {
        chars,
        styles,
        spans,
    } = collected;
    let mut advances: Vec<f64> = chars
        .iter()
        .zip(&outlines)
        .map(|(placed, outline)| advance_of(placed, &styles, outline))
        .collect();
    let mut stretch = vec![1.0_f64; chars.len()];
    apply_length_spans(&spans, &mut advances, &mut stretch);

    let (origins, chunks) = pen_positions(&chars, &advances);
    let mut origins = origins;
    anchor(&chunks, &chars, &styles, &advances, &mut origins);

    let mut glyphs = Vec::with_capacity(chars.len());
    for (index, placed) in chars.iter().enumerate() {
        let Some(outline) = outlines.get(index) else {
            continue;
        };
        let Some(style) = styles.get(placed.run) else {
            continue;
        };
        let em = style.font_size / outline.units_per_em;
        let origin = origins.get(index).copied().unwrap_or((0.0, 0.0));
        let widened = stretch.get(index).copied().unwrap_or(1.0);
        // Font units are y up and user space y down, so the flip is part of
        // the same scale. The shear leans the letterforms about the
        // baseline, which is the pen origin, so a synthetic oblique costs one
        // more matrix compose and nothing per contour.
        let to_user = Affine::scale(em * widened, -em)
            .then(oblique(outline.synthetic_shear))
            .then(Affine::rotate_degrees(placed.rotate))
            .then(Affine::translate(origin.0, origin.1));
        glyphs.push(PlacedGlyph {
            run: placed.run,
            to_user,
            synthetic_bold: outline.synthetic_bold,
        });
    }
    Laid {
        glyphs,
        styles,
        outlines,
    }
}

/// One character's user-space advance, spacing included.
fn advance_of(placed: &Placed, styles: &[Style], outline: &GlyphOutline) -> f64 {
    let Some(style) = styles.get(placed.run) else {
        return 0.0;
    };
    let scale = style.font_size / outline.units_per_em;
    let spacing = style.letter_spacing
        + if placed.scalar == ' ' {
            style.word_spacing
        } else {
            0.0
        };
    outline.advance * scale + spacing
}

/// Where every character's pen sits, and the spans of them one anchor
/// shifts together.
type PenPositions = (Vec<(f64, f64)>, Vec<(usize, usize)>);

/// Resolve the pen position of every character, and the chunks the anchor
/// shifts.
fn pen_positions(chars: &[Placed], advances: &[f64]) -> PenPositions {
    let mut origins: Vec<(f64, f64)> = Vec::with_capacity(chars.len());
    let mut chunks: Vec<(usize, usize)> = Vec::new();
    let mut pen = (0.0_f64, 0.0_f64);
    for (index, placed) in chars.iter().enumerate() {
        if placed.absolute.0.is_some() || placed.absolute.1.is_some() || index == 0 {
            if let Some(last) = chunks.last_mut() {
                last.1 = index;
            }
            chunks.push((index, chars.len()));
        }
        if let Some(x) = placed.absolute.0 {
            pen.0 = x;
        }
        if let Some(y) = placed.absolute.1 {
            pen.1 = y;
        }
        pen.0 += placed.shift.0;
        pen.1 += placed.shift.1;
        origins.push(pen);
        pen.0 += advances.get(index).copied().unwrap_or(0.0);
    }
    (origins, chunks)
}

/// Stretch or compress each `textLength` span to the length it states.
///
/// `spacing` spreads the difference across the gaps between glyphs, which
/// leaves the letterforms untouched; `spacingAndGlyphs` scales the glyphs by
/// the same ratio, so the text really is drawn at that width rather than
/// merely spaced to it.
fn apply_length_spans(spans: &[LengthSpan], advances: &mut [f64], stretch: &mut [f64]) {
    for span in spans {
        let Some(slice) = advances.get(span.from..span.to) else {
            continue;
        };
        let count = slice.len();
        let natural: f64 = slice.iter().sum();
        if count == 0 || !natural.is_finite() || natural <= 0.0 {
            continue;
        }
        if span.glyphs {
            let ratio = span.target / natural;
            if !ratio.is_finite() || ratio < 0.0 {
                continue;
            }
            for index in span.from..span.to {
                advances[index] *= ratio;
                stretch[index] *= ratio;
            }
        } else {
            // The last glyph's own advance is not a gap, so the difference is
            // shared between the gaps there actually are.
            let gaps = count.saturating_sub(1);
            if gaps == 0 {
                continue;
            }
            // Bounded by the resolved text length, well inside what a
            // double states exactly.
            let per_gap =
                (span.target - natural) / f64::from(u32::try_from(gaps).unwrap_or(u32::MAX));
            if !per_gap.is_finite() {
                continue;
            }
            for advance in advances
                .iter_mut()
                .take(span.to.saturating_sub(1))
                .skip(span.from)
            {
                *advance += per_gap;
            }
        }
    }
}

/// Shift each chunk by what its `text-anchor` says.
fn anchor(
    chunks: &[(usize, usize)],
    chars: &[Placed],
    styles: &[Style],
    advances: &[f64],
    origins: &mut [(f64, f64)],
) {
    for &(from, to) in chunks {
        let anchor = chars
            .get(from)
            .and_then(|first| styles.get(first.run))
            .map_or(TextAnchor::Start, |style| style.text_anchor);
        if anchor == TextAnchor::Start {
            continue;
        }
        let width: f64 = advances
            .get(from..to)
            .map_or(0.0, |slice| slice.iter().sum());
        let shift = match anchor {
            TextAnchor::Start => 0.0,
            TextAnchor::Middle => -width / 2.0,
            TextAnchor::End => -width,
        };
        for origin in origins.iter_mut().take(to).skip(from) {
            origin.0 += shift;
        }
    }
}

/// The shear a synthetic oblique folds into a glyph's transform.
fn oblique(shear: f64) -> Affine {
    if shear == 0.0 {
        return Affine::IDENTITY;
    }
    Affine {
        // Font units are y up, so leaning the tops of the letterforms
        // *forward* subtracts with height rather than adding.
        c: -shear,
        ..Affine::IDENTITY
    }
}

/// Flatten one glyph's contours into user-space sub-paths.
///
/// `tolerance` is in font units, resolved against the placement the glyph is
/// finally drawn under, so a glyph subdivides exactly as a `<path>` of the
/// same shape at the same size would.
pub(crate) fn glyph_subpaths(
    outline: &GlyphOutline,
    to_user: Affine,
    tolerance: f64,
) -> Vec<SubPath> {
    let mut out = Vec::with_capacity(outline.contours.len());
    for contour in &outline.contours {
        let mut points: Vec<Point> = Vec::with_capacity(contour.segments.len() + 1);
        points.push(contour.start);
        for segment in &contour.segments {
            let from = points.last().copied().unwrap_or(contour.start);
            match *segment {
                OutlineSegment::Line { to } => points.push(to),
                OutlineSegment::Quadratic { control, to } => {
                    flatten_quadratic(from, control, to, tolerance, &mut points);
                }
            }
        }
        let mapped: Vec<Point> = points.iter().map(|point| to_user.apply(*point)).collect();
        let sub = SubPath::closed(mapped);
        if !sub.is_degenerate() {
            out.push(sub);
        }
    }
    out
}

#[cfg(test)]
#[path = "text_tests.rs"]
mod tests;
