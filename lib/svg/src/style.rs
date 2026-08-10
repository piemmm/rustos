//! The presentation properties a shape is drawn with, and how they inherit.
//!
//! SVG spells the same property three ways — a presentation attribute
//! (`fill="red"`), a declaration in the element's `style` attribute
//! (`style="fill:red"`), and whatever the element inherited from its parent —
//! with the `style` attribute winning over the attribute, and the attribute
//! over the inherited value. That precedence lives here, once, so no shape or
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

use tairix_raster::{Color, FillRule};

use crate::color::{parse_color, ColorSpec};
use crate::error::SvgError;
use crate::geom::{LineCap, LineJoin, StrokeStyle};
use crate::number::{opacity_to_alpha, parse_length, parse_number, parse_opacity};
use crate::xml::Node;

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
    /// The group opacity, `0..=1`, applied to this element and its subtree.
    ///
    /// Unlike the fill and stroke opacities this one does *not* inherit: it
    /// is applied where it is written and reset for the children, because SVG
    /// composites the subtree as a unit.
    pub opacity: f64,
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
            opacity: 1.0,
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
            display: true,
            ..self.clone()
        }
    }

    /// This style with `node`'s own presentation attributes and `style`
    /// declarations applied, in SVG's precedence order.
    ///
    /// `viewport` is the diagonal length percentages resolve against, which
    /// is what SVG defines a percentage length with no axis to be measured
    /// along.
    ///
    /// # Errors
    /// Returns the parse error of the first property that is understood but
    /// malformed, so a bad colour or width refuses the document rather than
    /// drawing something the author did not write.
    pub fn apply(&self, node: &Node<'_>, viewport: f64) -> Result<Self, SvgError> {
        let mut style = self.clone();
        for (name, value) in &node.attrs {
            style.set(name, value.as_ref(), viewport)?;
        }
        if let Some(inline) = node.attr("style") {
            for declaration in inline.split(';') {
                let Some((name, value)) = declaration.split_once(':') else {
                    continue;
                };
                style.set(name.trim(), value.trim(), viewport)?;
            }
        }
        Ok(style)
    }

    /// Apply one property. An unknown name is ignored; a known name with an
    /// unparsable value is an error.
    fn set(&mut self, name: &str, value: &str, viewport: f64) -> Result<(), SvgError> {
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

    /// The colour a fill is drawn in, with both opacities folded in, or
    /// `None` when nothing is painted.
    ///
    /// A reference to a paint server has no colour of its own; the caller
    /// resolves it and applies [`Style::alpha`] itself.
    #[must_use]
    pub fn fill_color(&self) -> Option<Color> {
        self.paint_color(&self.fill, self.fill_opacity)
    }

    /// The colour a stroke is drawn in, with both opacities folded in.
    #[must_use]
    pub fn stroke_color(&self) -> Option<Color> {
        self.paint_color(&self.stroke, self.stroke_opacity)
    }

    /// The alpha multiplier one of this element's paints carries: its own
    /// opacity times the group opacity.
    #[must_use]
    pub fn alpha(&self, paint_opacity: f64) -> f64 {
        (paint_opacity * self.opacity).clamp(0.0, 1.0)
    }

    /// `paint`'s colour scaled by `paint_opacity` and the group opacity, or
    /// `None` when it paints nothing.
    fn paint_color(&self, paint: &PaintSpec, paint_opacity: f64) -> Option<Color> {
        let base = match paint {
            PaintSpec::None => return None,
            PaintSpec::Color(color) => *color,
            PaintSpec::Current => self.color,
            PaintSpec::Reference(_, fallback) => (*fallback)?,
        };
        scale_alpha(base, self.alpha(paint_opacity))
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
