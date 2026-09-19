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
        for (name, value) in &element.attrs {
            style.set(name, value.as_ref(), viewport, Source::Attribute)?;
        }
        let inline = element.attr("style").unwrap_or("");
        for important in [false, true] {
            for rule in cascade
                .iter()
                .filter(|declaration| declaration.important == important)
            {
                style.set(rule.name, rule.value, viewport, Source::Declaration)?;
            }
            for own in css::declarations(inline).filter(|own| own.important == important) {
                style.set(own.name, own.value, viewport, Source::Declaration)?;
            }
        }
        Ok(style)
    }

    /// Apply one property. An unknown name is ignored; a known name with an
    /// unparsable value is an error.
    fn set(
        &mut self,
        name: &str,
        value: &str,
        viewport: f64,
        source: Source,
    ) -> Result<(), SvgError> {
        let value = value.trim();
        match name {
            "fill" => self.fill = parse_paint(value)?,
            "fill-rule" => self.fill_rule = parse_fill_rule(value)?,
            "fill-opacity" => self.fill_opacity = parse_opacity(value)?,
            "stroke" => self.stroke = parse_paint(value)?,
            "stroke-opacity" => self.stroke_opacity = parse_opacity(value)?,
            "stroke-width" => self.stroke_style.width = parse_length(value, viewport)?,
            "stroke-linecap" => self.stroke_style.cap = parse_cap(value)?,
            "stroke-linejoin" => self.stroke_style.join = parse_join(value)?,
            "stroke-miterlimit" => self.stroke_style.miter_limit = parse_miter_limit(value)?,
            "stroke-dasharray" => self.stroke_style.dashes = parse_dashes(value, viewport)?,
            "stroke-dashoffset" => self.stroke_style.dash_offset = parse_length(value, viewport)?,
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
fn parse_dashes(value: &str, viewport: f64) -> Result<Vec<f64>, SvgError> {
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
        dashes.push(parse_length(token, viewport)?);
    }
    if dashes.iter().any(|length| *length < 0.0) || dashes.iter().sum::<f64>() <= 0.0 {
        dashes.clear();
    }
    Ok(dashes)
}

#[cfg(test)]
#[path = "style_tests.rs"]
mod tests;
