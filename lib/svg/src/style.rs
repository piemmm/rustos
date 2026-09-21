//! The presentation properties a shape is drawn with, and how they inherit.
//!
//! SVG spells the same property four ways — a presentation attribute
//! (`fill="red"`), a rule in one of the document's `<style>` sheets
//! (`.cls { fill: red }`), a declaration in the element's `style` attribute
//! (`style="fill:red"`), and whatever the element inherited from its parent.
//! The later three win in that order, and an `!important` declaration wins
//! over every normal one. That precedence lives here, once, so no shape or
//! container re-derives it.
//!
//! A [`Style`] is therefore a *resolved* value set: every property already
//! holds the value this element draws with. Descending into a child is
//! [`Style::inherit`], which copies the inheritable properties and resets the
//! ones SVG does not inherit.
//!
//! Unlike geometry, a property this decoder does not understand is *ignored*
//! rather than refused: a document is full of editor metadata and text
//! properties that have no bearing on the shapes drawn, and refusing them
//! would reject nearly every real asset. A property it *does* understand but
//! cannot parse is an error, so a malformed colour or width still fails
//! closed.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_raster::{Color, FillRule, MaskKind};

use crate::color::{parse_color, ColorSpec};
use crate::css::{self, Declaration};
use crate::error::SvgError;
use crate::font::FontStyle;
use crate::geom::{LineCap, LineJoin, StrokeStyle};
use crate::number::{opacity_to_alpha, parse_length, parse_number, parse_opacity};
use crate::xml::Element;

/// Where one declaration in the cascade came from.
///
/// Only the `marker` shorthand reads it: SVG publishes that property to CSS
/// alone, so a presentation attribute spelling it is not one.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Source {
    /// A presentation attribute on the element.
    Attribute,
    /// A stylesheet rule or a `style` declaration.
    Declaration,
}

/// The lengths one property value may be written against: the viewport a
/// percentage measures, and the font size an `em` does.
///
/// A presentation property is resolved *after* the element's font size is,
/// so `stroke-width: 0.1em` means what it says. A geometry attribute is
/// parsed before any style exists and keeps the viewport alone, which is
/// where this crate's remaining relative-unit gap sits.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Lengths {
    /// The length a percentage with no axis of its own resolves against.
    pub viewport: f64,
    /// The computed font size, which one `em` measures.
    pub font_size: f64,
}

/// SVG's initial font size in user units, which is CSS's `medium`.
pub const INITIAL_FONT_SIZE: f64 = 16.0;

/// The x-height an `ex` is taken as, as a fraction of the em.
///
/// CSS's own fallback for a face whose x-height is unknown — and at cascade
/// time it genuinely is, since the face is not resolved until the text is
/// laid out.
const EX_PER_EM: f64 = 0.5;

/// Parse a length that may be written in font-relative units.
///
/// # Errors
/// [`SvgError::InvalidNumber`] for a value outside the grammar, exactly as
/// [`parse_length`] refuses one.
pub fn parse_length_in(text: &str, lengths: Lengths) -> Result<f64, SvgError> {
    let trimmed = text.trim();
    let relative = |suffix: &str, per_em: f64| {
        trimmed
            .strip_suffix(suffix)
            .map(|number| parse_number(number).map(|value| value * lengths.font_size * per_em))
    };
    match relative("em", 1.0).or_else(|| relative("ex", EX_PER_EM)) {
        Some(scaled) => {
            let scaled = scaled?;
            if scaled.is_finite() {
                Ok(scaled)
            } else {
                Err(SvgError::InvalidNumber)
            }
        }
        None => parse_length(trimmed, lengths.viewport),
    }
}

/// Where a chunk of text sits relative to the position that placed it.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum TextAnchor {
    /// The chunk begins at the position.
    #[default]
    Start,
    /// The chunk is centred on it.
    Middle,
    /// The chunk ends at it.
    End,
}

/// The most dash lengths accepted in one pattern.
///
/// A fixed security bound: a dash pattern is a handful of lengths in every
/// real asset, and the cap is what stops a hostile one from making the
/// stroker walk an unbounded pattern.
const MAX_DASHES: usize = 64;

/// What a shape is painted with, before a paint server is resolved.
///
/// A reference is kept as the fragment name it points at; resolving it needs
/// the document's definitions, which is the paint module's job, not the
/// cascade's.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum PaintSpec {
    /// Nothing is painted.
    #[default]
    None,
    /// A plain colour.
    Color(Color),
    /// The cascade's own `color` property, resolved where the paint is used
    /// rather than where it is written: CSS resolves `currentColor` against
    /// the element's final `color`, which an attribute later in the same tag
    /// may still change.
    Current,
    /// A `url(#id)` reference to a paint server, with the fallback that
    /// follows it if there is one.
    Reference(String, Option<Color>),
}

/// One of the three things a shape paints.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PaintSlot {
    /// The interior.
    Fill,
    /// The outline.
    Stroke,
    /// The markers at the shape's vertices.
    Markers,
}

/// The order a shape paints its fill, its stroke, and its markers in.
///
/// Always a permutation of the three: the property can only reorder what a
/// shape draws, never drop part of it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct PaintOrder([PaintSlot; 3]);

impl Default for PaintOrder {
    /// SVG's initial order: the fill, the stroke over it, the markers over
    /// both.
    fn default() -> Self {
        Self([PaintSlot::Fill, PaintSlot::Stroke, PaintSlot::Markers])
    }
}

impl PaintOrder {
    /// The three slots, bottom first.
    #[must_use]
    pub const fn slots(self) -> [PaintSlot; 3] {
        self.0
    }
}

/// Whether a viewport-establishing element confines its content.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum Overflow {
    /// Content outside the viewport is clipped away: SVG's initial value for
    /// a `<symbol>` and a nested `<svg>`.
    #[default]
    Hidden,
    /// Content is drawn wherever it falls.
    Visible,
}

/// Every property that decides how one element is drawn, already resolved.
#[derive(Clone, Debug, PartialEq)]
pub struct Style {
    /// What the interior is painted with.
    pub fill: PaintSpec,
    /// Which points count as interior.
    pub fill_rule: FillRule,
    /// The fill's own opacity, `0..=1`.
    pub fill_opacity: f64,
    /// What the outline is painted with.
    pub stroke: PaintSpec,
    /// The stroke's own opacity, `0..=1`.
    pub stroke_opacity: f64,
    /// The geometry of the stroke.
    pub stroke_style: StrokeStyle,
    /// The value `currentColor` stands for.
    pub color: Color,
    /// Which points a `<clipPath>` child of this element encloses.
    pub clip_rule: FillRule,
    /// Which of a shape's fill, stroke, and markers is painted first.
    pub paint_order: PaintOrder,
    /// Whether the stroke is outlined in the host space, so neither its width
    /// nor its dash lengths scale with the element's transform.
    ///
    /// Non-inheriting, as SVG defines it.
    pub non_scaling_stroke: bool,
    /// The group opacity, `0..=1`, applied to this element and its subtree.
    ///
    /// Unlike the fill and stroke opacities this one does *not* inherit: it
    /// is applied where it is written and reset for the children, because SVG
    /// composites the subtree as a unit.
    pub opacity: f64,
    /// The fragment name of the `<clipPath>` this element is clipped to.
    ///
    /// Non-inheriting, like the other compositing properties: a clip applies
    /// to the subtree as a unit rather than to each descendant again.
    pub clip_path: Option<String>,
    /// The fragment name of the `<mask>` this element is masked by.
    pub mask: Option<String>,
    /// The fragment name of the `<marker>` drawn at the shape's first vertex.
    ///
    /// The three marker properties inherit, unlike the compositing ones: a
    /// marker is drawn once per vertex of each descendant shape, not once
    /// around the subtree.
    pub marker_start: Option<String>,
    /// The `<marker>` drawn at every vertex but the first and the last.
    pub marker_mid: Option<String>,
    /// The `<marker>` drawn at the shape's last vertex.
    pub marker_end: Option<String>,
    /// Whether a viewport-establishing element confines its content to the
    /// viewport.
    pub overflow: Overflow,
    /// Which channel of a `<mask>` element's own content is its factor.
    ///
    /// Only a `<mask>` reads it, so like the other compositing properties it
    /// does not inherit.
    pub mask_kind: MaskKind,
    /// Whether the element and its subtree are drawn at all.
    pub display: bool,
    /// Whether the element itself is drawn (its children may still be).
    pub visible: bool,
    /// The `font-family` list as written, resolved where the text is laid
    /// out rather than here: which name answers depends on the store, which
    /// the cascade has no way to ask.
    pub font_family: Option<String>,
    /// The font size in user units.
    pub font_size: f64,
    /// The `wght` design-axis coordinate, `1..=1000`.
    pub font_weight: u16,
    /// The posture.
    pub font_style: FontStyle,
    /// The `wdth` design-axis coordinate, in hundredths of a percent.
    pub font_stretch: u16,
    /// Extra space added after every glyph, in user units.
    pub letter_spacing: f64,
    /// Extra space added after every space character, in user units.
    pub word_spacing: f64,
    /// Where a chunk sits relative to the position that placed it.
    pub text_anchor: TextAnchor,
}

impl Default for Style {
    /// SVG's initial property values: a black fill, no stroke, fully opaque.
    fn default() -> Self {
        Self {
            fill: PaintSpec::Color(Color::rgb(0, 0, 0)),
            fill_rule: FillRule::NonZero,
            fill_opacity: 1.0,
            stroke: PaintSpec::None,
            stroke_opacity: 1.0,
            stroke_style: StrokeStyle::default(),
            color: Color::rgb(0, 0, 0),
            clip_rule: FillRule::NonZero,
            paint_order: PaintOrder::default(),
            non_scaling_stroke: false,
            opacity: 1.0,
            clip_path: None,
            mask: None,
            marker_start: None,
            marker_mid: None,
            marker_end: None,
            overflow: Overflow::default(),
            mask_kind: MaskKind::Luminance,
            display: true,
            visible: true,
            font_family: None,
            font_size: INITIAL_FONT_SIZE,
            font_weight: NORMAL_WEIGHT,
            font_style: FontStyle::Normal,
            font_stretch: NORMAL_STRETCH,
            letter_spacing: 0.0,
            word_spacing: 0.0,
            text_anchor: TextAnchor::Start,
        }
    }
}

impl Style {
    /// The style a child starts from: everything inheritable kept, and the
    /// properties SVG does not inherit back at their initial values.
    #[must_use]
    pub fn inherit(&self) -> Self {
        Self {
            opacity: 1.0,
            clip_path: None,
            mask: None,
            overflow: Overflow::default(),
            mask_kind: MaskKind::Luminance,
            non_scaling_stroke: false,
            display: true,
            ..self.clone()
        }
    }

    /// This style with `element`'s own properties applied, in the cascade's
    /// precedence order.
    ///
    /// `cascade` is the declarations the document's stylesheets match on this
    /// element, already ordered by specificity and source order with the
    /// `!important` ones last ([`Stylesheet::cascade`](crate::css::Stylesheet::cascade)).
    ///
    /// `viewport` is the diagonal length percentages resolve against, which
    /// is what SVG defines a percentage length with no axis to be measured
    /// along.
    ///
    /// # Errors
    /// Returns the parse error of the first property that is understood but
    /// malformed, so a bad colour or width refuses the document rather than
    /// drawing something the author did not write.
    pub fn apply(
        &self,
        element: &Element<'_>,
        viewport: f64,
        cascade: &[Declaration<'_>],
    ) -> Result<Self, SvgError> {
        let mut style = self.clone();
        // The font size is computed first, in its own pass over the same
        // cascade, because every other length may be written in `em` — which
        // is what CSS does, and the only way `stroke-width: 0.1em` can mean
        // the size this element ends up set in rather than whichever size
        // happened to be applied by then. A percentage measures the
        // *inherited* size, so the basis is the one this element started
        // from.
        let inherited = Lengths {
            viewport,
            font_size: self.font_size,
        };
        // The extra pass is paid only where a font size is actually
        // declared, which almost no element in a drawing does: without one
        // the inherited size already *is* this element's, so the single
        // pass reads the same `em` the two would have.
        let pass = if sets_font_size(element, cascade) {
            style.walk_cascade(element, cascade, inherited, FontSizePass::Only)?;
            FontSizePass::Rest
        } else {
            FontSizePass::Every
        };
        let lengths = Lengths {
            viewport,
            font_size: style.font_size,
        };
        style.walk_cascade(element, cascade, lengths, pass)?;
        Ok(style)
    }

    /// Apply one pass of the cascade in precedence order.
    fn walk_cascade(
        &mut self,
        element: &Element<'_>,
        cascade: &[Declaration<'_>],
        lengths: Lengths,
        pass: FontSizePass,
    ) -> Result<(), SvgError> {
        for (name, value) in &element.attrs {
            if pass.covers(name) {
                self.set(name, value.as_ref(), lengths, Source::Attribute)?;
            }
        }
        let inline = element.attr("style").unwrap_or("");
        for important in [false, true] {
            for rule in cascade
                .iter()
                .filter(|declaration| declaration.important == important)
            {
                if pass.covers(rule.name) {
                    self.set(rule.name, rule.value, lengths, Source::Declaration)?;
                }
            }
            for own in css::declarations(inline).filter(|own| own.important == important) {
                if pass.covers(own.name) {
                    self.set(own.name, own.value, lengths, Source::Declaration)?;
                }
            }
        }
        Ok(())
    }

    /// Apply one property. An unknown name is ignored; a known name with an
    /// unparsable value is an error.
    fn set(
        &mut self,
        name: &str,
        value: &str,
        lengths: Lengths,
        source: Source,
    ) -> Result<(), SvgError> {
        let value = value.trim();
        match name {
            "fill" => self.fill = parse_paint(value)?,
            "fill-rule" => self.fill_rule = parse_fill_rule(value)?,
            "fill-opacity" => self.fill_opacity = parse_opacity(value)?,
            "stroke" => self.stroke = parse_paint(value)?,
            "stroke-opacity" => self.stroke_opacity = parse_opacity(value)?,
            "stroke-width" => self.stroke_style.width = parse_length_in(value, lengths)?,
            "stroke-linecap" => self.stroke_style.cap = parse_cap(value)?,
            "stroke-linejoin" => self.stroke_style.join = parse_join(value)?,
            "stroke-miterlimit" => self.stroke_style.miter_limit = parse_miter_limit(value)?,
            "stroke-dasharray" => self.stroke_style.dashes = parse_dashes(value, lengths)?,
            "stroke-dashoffset" => self.stroke_style.dash_offset = parse_length_in(value, lengths)?,
            "opacity" => self.opacity = parse_opacity(value)?,
            "clip-rule" => self.clip_rule = parse_fill_rule(value)?,
            // An invalid `paint-order` is a dropped declaration, so the
            // element keeps whatever it inherited rather than snapping back
            // to the initial order.
            "paint-order" => {
                if let Some(order) = parse_paint_order(value) {
                    self.paint_order = order;
                }
            }
            // Dropped when invalid, as CSS drops a value it cannot use: the
            // initial `none` it leaves is an ordinary stroke, which hides
            // nothing.
            "vector-effect" => {
                if let Some(non_scaling) = parse_vector_effect(value) {
                    self.non_scaling_stroke = non_scaling;
                }
            }
            "clip-path" => self.clip_path = parse_funciri(value)?,
            "mask" => self.mask = parse_funciri(value)?,
            "marker-start" => self.marker_start = parse_funciri(value)?,
            "marker-mid" => self.marker_mid = parse_funciri(value)?,
            "marker-end" => self.marker_end = parse_funciri(value)?,
            // SVG defines no `marker` presentation attribute, only the CSS
            // shorthand, so an attribute of that name sets nothing — honouring
            // it would draw markers no other renderer does.
            "marker" if source == Source::Declaration => {
                let shared = parse_funciri(value)?;
                self.marker_start.clone_from(&shared);
                self.marker_mid.clone_from(&shared);
                self.marker_end = shared;
            }
            "mask-type" => {
                self.mask_kind = match value {
                    "alpha" => MaskKind::Alpha,
                    _ => MaskKind::Luminance,
                }
            }
            "overflow" => {
                self.overflow = match value {
                    "visible" | "auto" => Overflow::Visible,
                    _ => Overflow::Hidden,
                }
            }
            "color" => {
                if let ColorSpec::Value(color) = parse_color(value)? {
                    self.color = color;
                }
            }
            "display" => self.display = value != "none",
            "visibility" => self.visible = !matches!(value, "hidden" | "collapse"),
            // The list is kept as written: which of its names answers is the
            // font store's question, not the cascade's.
            "font-family" => self.font_family = Some(value.to_string()),
            "font-size" => self.font_size = parse_font_size(value, lengths)?,
            "font-weight" => self.font_weight = parse_font_weight(value, self.font_weight)?,
            "font-style" => self.font_style = parse_font_style(value)?,
            "font-stretch" => self.font_stretch = parse_font_stretch(value)?,
            "letter-spacing" => self.letter_spacing = parse_spacing(value, lengths)?,
            "word-spacing" => self.word_spacing = parse_spacing(value, lengths)?,
            "text-anchor" => self.text_anchor = parse_text_anchor(value)?,
            _ => {}
        }
        Ok(())
    }
}

/// `color` with its alpha multiplied by `factor`, or `None` once nothing of
/// it would be visible.
#[must_use]
pub fn scale_alpha(color: Color, factor: f64) -> Option<Color> {
    let alpha = opacity_to_alpha(f64::from(color.a) / 255.0 * factor);
    (alpha != 0).then(|| Color::rgba(color.r, color.g, color.b, alpha))
}

/// Parse a `fill` or `stroke` value.
fn parse_paint(value: &str) -> Result<PaintSpec, SvgError> {
    if let Some(rest) = value.strip_prefix("url(") {
        let (reference, after) = rest.split_once(')').ok_or(SvgError::InvalidColor)?;
        let name = reference.trim().trim_matches(['"', '\'']);
        let id = name.strip_prefix('#').ok_or(SvgError::InvalidColor)?;
        let after = after.trim();
        let fallback = if after.is_empty() {
            None
        } else {
            match parse_color(after)? {
                ColorSpec::Value(color) => Some(color),
                // `currentColor` as a paint-server fallback would need the
                // cascade's own colour, which the caller resolves; treat it
                // as no fallback rather than guessing black.
                ColorSpec::None | ColorSpec::Current => None,
            }
        };
        return Ok(PaintSpec::Reference(id.to_string(), fallback));
    }
    Ok(match parse_color(value)? {
        ColorSpec::None => PaintSpec::None,
        ColorSpec::Value(color) => PaintSpec::Color(color),
        ColorSpec::Current => PaintSpec::Current,
    })
}

/// Parse a `clip-path` or `mask` value into the fragment it names.
///
/// `none` applies nothing. Anything else must be a local `url(#id)`: this
/// crate resolves no external reference, and a CSS basic shape is a composite
/// it cannot build — so both are refused rather than quietly drawn without
/// the clip the author asked for.
fn parse_funciri(value: &str) -> Result<Option<String>, SvgError> {
    if value == "none" {
        return Ok(None);
    }
    let rest = value
        .strip_prefix("url(")
        .ok_or(SvgError::InvalidReference)?;
    let (reference, _) = rest.split_once(')').ok_or(SvgError::InvalidReference)?;
    let name = reference.trim().trim_matches(['"', '\'']);
    let id = name.strip_prefix('#').ok_or(SvgError::InvalidReference)?;
    Ok(Some(id.to_string()))
}

/// Parse a `vector-effect`, answering whether it asks for a non-scaling
/// stroke, or `None` for a value CSS calls invalid.
///
/// The value is `none`, or one or more effect keywords optionally followed by
/// the host space to measure them in. Only `non-scaling-stroke` is drawn, so
/// a value naming it alongside others still asks for one and a value naming
/// only others asks for nothing — but both parse, because an effect this
/// decoder does not apply is still a value CSS accepts.
fn parse_vector_effect(value: &str) -> Option<bool> {
    let mut words = value.split_ascii_whitespace().peekable();
    if words.peek() == Some(&"none") {
        words.next();
        return words.next().is_none().then_some(false);
    }
    let mut non_scaling = false;
    let mut effects = 0;
    while let Some(word) = words.peek() {
        match *word {
            "non-scaling-stroke" => non_scaling = true,
            "non-scaling-size" | "non-rotation" | "fixed-position" => {}
            _ => break,
        }
        effects += 1;
        words.next();
    }
    if effects == 0 {
        return None;
    }
    // The trailing keyword names which space the effects are measured in.
    // This decoder has one, so it selects nothing; a value is still refused
    // for spelling it anywhere but last.
    if matches!(words.peek(), Some(&"viewport" | &"screen")) {
        words.next();
    }
    words.next().is_none().then_some(non_scaling)
}

/// Parse a `paint-order`, or `None` for a value CSS calls invalid.
///
/// `normal` is the initial order. Otherwise the words name some of `fill`,
/// `stroke`, and `markers`: those come first in the order they are written,
/// and whichever are left follow in the initial order. An empty value, an
/// unknown word, or a repeat is invalid, which CSS drops — it does not refuse
/// the document and does not reset the property.
fn parse_paint_order(value: &str) -> Option<PaintOrder> {
    if value == "normal" {
        return Some(PaintOrder::default());
    }
    let mut order = PaintOrder::default().0;
    let mut named = 0;
    for word in value.split_ascii_whitespace() {
        let slot = match word {
            "fill" => PaintSlot::Fill,
            "stroke" => PaintSlot::Stroke,
            "markers" => PaintSlot::Markers,
            _ => return None,
        };
        // A fourth word can only repeat one of the three, so the index stays
        // inside the array.
        if order[..named].contains(&slot) {
            return None;
        }
        order[named] = slot;
        named += 1;
    }
    if named == 0 {
        return None;
    }
    for slot in PaintOrder::default().0 {
        if !order[..named].contains(&slot) {
            order[named] = slot;
            named += 1;
        }
    }
    Some(PaintOrder(order))
}

/// Parse a `fill-rule` keyword.
fn parse_fill_rule(value: &str) -> Result<FillRule, SvgError> {
    match value {
        "nonzero" => Ok(FillRule::NonZero),
        "evenodd" => Ok(FillRule::EvenOdd),
        _ => Err(SvgError::InvalidNumber),
    }
}

/// Parse a `stroke-linecap` keyword.
fn parse_cap(value: &str) -> Result<LineCap, SvgError> {
    match value {
        "butt" => Ok(LineCap::Butt),
        "round" => Ok(LineCap::Round),
        "square" => Ok(LineCap::Square),
        _ => Err(SvgError::InvalidNumber),
    }
}

/// Parse a `stroke-linejoin` keyword.
fn parse_join(value: &str) -> Result<LineJoin, SvgError> {
    match value {
        "miter" | "miter-clip" => Ok(LineJoin::Miter),
        "round" => Ok(LineJoin::Round),
        "bevel" | "arcs" => Ok(LineJoin::Bevel),
        _ => Err(SvgError::InvalidNumber),
    }
}

/// Parse a `stroke-miterlimit`, which SVG floors at 1.
fn parse_miter_limit(value: &str) -> Result<f64, SvgError> {
    Ok(parse_number(value)?.max(1.0))
}

/// Parse a `stroke-dasharray`.
///
/// `none` and a pattern whose lengths sum to zero both mean a solid stroke; a
/// negative length makes the whole pattern invalid, which SVG also draws
/// solid.
fn parse_dashes(value: &str, lengths: Lengths) -> Result<Vec<f64>, SvgError> {
    if value == "none" {
        return Ok(Vec::new());
    }
    let mut dashes = Vec::new();
    // Split before parsing rather than scanning a number run: a dash length
    // may carry a unit or a percentage, which a bare number scan would leave
    // behind as trailing junk.
    for token in value.split([',', ' ', '\t', '\r', '\n']) {
        if token.is_empty() {
            continue;
        }
        if dashes.len() == MAX_DASHES {
            return Err(SvgError::TooComplex);
        }
        dashes.push(parse_length_in(token, lengths)?);
    }
    if dashes.iter().any(|length| *length < 0.0) || dashes.iter().sum::<f64>() <= 0.0 {
        dashes.clear();
    }
    Ok(dashes)
}

/// Whether anything in this element's cascade sets `font-size`.
///
/// A name comparison over the declarations the element already carries, so
/// the check costs a fraction of the pass it decides against.
fn sets_font_size(element: &Element<'_>, cascade: &[Declaration<'_>]) -> bool {
    element.attrs.iter().any(|(name, _)| *name == FONT_SIZE)
        || cascade.iter().any(|rule| rule.name == FONT_SIZE)
        || css::declarations(element.attr("style").unwrap_or("")).any(|own| own.name == FONT_SIZE)
}

/// The one property whose value every other length may be written against.
const FONT_SIZE: &str = "font-size";

/// Which properties a pass of the cascade applies.
///
/// `font-size` runs first and alone — so every other length can be written
/// against the size this element actually ends up set in — but only where
/// one is declared at all.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum FontSizePass {
    /// `font-size` and nothing else.
    Only,
    /// Everything but `font-size`, which an earlier pass applied.
    Rest,
    /// Everything, because no `font-size` is declared here.
    Every,
}

impl FontSizePass {
    /// Whether this pass applies the property `name`.
    fn covers(self, name: &str) -> bool {
        match self {
            Self::Only => name == FONT_SIZE,
            Self::Rest => name != FONT_SIZE,
            Self::Every => true,
        }
    }
}

/// The `wght` axis coordinate `normal` names.
const NORMAL_WEIGHT: u16 = 400;

/// The `wght` axis coordinate `bold` names.
const BOLD_WEIGHT: u16 = 700;

/// Lightest and heaviest coordinate the `wght` axis is defined over.
const WEIGHT_RANGE: (u16, u16) = (1, 1000);

/// The step `bolder` and `lighter` move by.
///
/// CSS 2.1 defines them against a face's own weight ladder, which needs the
/// resolved face; one named step of the numeric axis is the approximation
/// every renderer makes, and it keeps the property a pure function of the
/// cascade.
const WEIGHT_STEP: u16 = 300;

/// The `wdth` axis coordinate `normal` names, in hundredths of a percent.
const NORMAL_STRETCH: u16 = 100 * STRETCH_SCALE;

/// Hundredths of a percent per `font-stretch` step.
const STRETCH_SCALE: u16 = 100;

/// Narrowest and widest coordinate the `wdth` axis is defined over.
const STRETCH_RANGE: (u16, u16) = (50 * STRETCH_SCALE, 200 * STRETCH_SCALE);

/// Parse a `font-size`.
///
/// A percentage measures the inherited size rather than the viewport, which
/// is what CSS means by a relative font size — so the basis is swapped for
/// this one property.
fn parse_font_size(value: &str, lengths: Lengths) -> Result<f64, SvgError> {
    let against_parent = Lengths {
        viewport: lengths.font_size,
        ..lengths
    };
    let size = parse_length_in(value, against_parent)?;
    if size.is_finite() && size >= 0.0 {
        Ok(size)
    } else {
        Err(SvgError::InvalidNumber)
    }
}

/// Parse a `font-weight`, given the weight this element inherited.
fn parse_font_weight(value: &str, inherited: u16) -> Result<u16, SvgError> {
    let axis = match value {
        "normal" => NORMAL_WEIGHT,
        "bold" => BOLD_WEIGHT,
        "bolder" => inherited.saturating_add(WEIGHT_STEP),
        "lighter" => inherited.saturating_sub(WEIGHT_STEP),
        number => {
            let parsed = parse_number(number)?;
            if !parsed.is_finite() {
                return Err(SvgError::InvalidNumber);
            }
            // The axis admits whole coordinates only, and the range check
            // below refuses anything the field could not hold.
            round_axis(parsed)?
        }
    };
    if axis < WEIGHT_RANGE.0 || axis > WEIGHT_RANGE.1 {
        // A keyword step that walked off the end lands on the end, as CSS
        // says; a *written* weight outside the axis is a value no face has.
        return match value {
            "bolder" | "lighter" => Ok(axis.clamp(WEIGHT_RANGE.0, WEIGHT_RANGE.1)),
            _ => Err(SvgError::InvalidNumber),
        };
    }
    Ok(axis)
}

/// Parse a `font-style`.
fn parse_font_style(value: &str) -> Result<FontStyle, SvgError> {
    match value {
        "normal" => Ok(FontStyle::Normal),
        "italic" => Ok(FontStyle::Italic),
        // SVG 1.1 admits an angle after `oblique`; the angle is the S25
        // property cascade's, and the posture alone is what this resolves.
        value if value == "oblique" || value.starts_with("oblique ") => Ok(FontStyle::Oblique),
        _ => Err(SvgError::InvalidNumber),
    }
}

/// Parse a `font-stretch` into hundredths of a percent.
fn parse_font_stretch(value: &str) -> Result<u16, SvgError> {
    let percent = match value {
        "ultra-condensed" => 50 * STRETCH_SCALE,
        "extra-condensed" => 625 * (STRETCH_SCALE / 10),
        "condensed" => 75 * STRETCH_SCALE,
        "semi-condensed" => 875 * (STRETCH_SCALE / 10),
        "semi-expanded" => 1125 * (STRETCH_SCALE / 10),
        "expanded" => 125 * STRETCH_SCALE,
        "extra-expanded" => 150 * STRETCH_SCALE,
        "ultra-expanded" => 200 * STRETCH_SCALE,
        // `wider` and `narrower` are defined against the face's own width
        // ladder, which the cascade cannot see; they resolve to the
        // unstretched width like any other value this subset cannot follow.
        "normal" | "wider" | "narrower" => NORMAL_STRETCH,
        percent => {
            let number = percent
                .strip_suffix('%')
                .ok_or(SvgError::InvalidNumber)
                .and_then(parse_number)?;
            let rounded = round_axis(number * f64::from(STRETCH_SCALE))?;
            if rounded < STRETCH_RANGE.0 || rounded > STRETCH_RANGE.1 {
                return Err(SvgError::InvalidNumber);
            }
            rounded
        }
    };
    Ok(percent)
}

/// One design-axis coordinate as the nearest whole step.
///
/// # Errors
/// [`SvgError::InvalidNumber`] for a non-finite value or one no axis field
/// could hold.
fn round_axis(value: f64) -> Result<u16, SvgError> {
    if !value.is_finite() || value < 0.0 || value > f64::from(u16::MAX) {
        return Err(SvgError::InvalidNumber);
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "held non-negative and within the u16 range on the line above, so \
                  neither the truncation nor the sign loss the lints warn about can occur"
    )]
    Ok((value + 0.5) as u16)
}

/// Parse a `letter-spacing` or `word-spacing`.
fn parse_spacing(value: &str, lengths: Lengths) -> Result<f64, SvgError> {
    if value == "normal" {
        return Ok(0.0);
    }
    parse_length_in(value, lengths)
}

/// Parse a `text-anchor`.
fn parse_text_anchor(value: &str) -> Result<TextAnchor, SvgError> {
    match value {
        "start" => Ok(TextAnchor::Start),
        "middle" => Ok(TextAnchor::Middle),
        "end" => Ok(TextAnchor::End),
        _ => Err(SvgError::InvalidNumber),
    }
}

#[cfg(test)]
#[path = "style_tests.rs"]
mod tests;
