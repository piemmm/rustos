//! Paint servers: the document's gradients, and resolving a `url(#id)` fill
//! into the paint the rasteriser samples.
//!
//! A gradient is defined once, anywhere in the document, and referenced by
//! any number of shapes — and what it looks like depends on the shape using
//! it, because `objectBoundingBox` units are fractions of *that shape's*
//! bounds. Resolution therefore happens per use, here, and produces a
//! [`Paint`] carrying the map from design-grid coordinates back into the
//! gradient's own canonical space, which is what lets the rasteriser sample a
//! rotated or skewed gradient exactly rather than approximating it.

use alloc::vec::Vec;

use tairix_raster::{Affine, Color, Gradient, GradientKind, GradientStop, Paint, SpreadMethod};
use tairix_util::mathf::sqrt;

use crate::color::{parse_color, ColorSpec};
use crate::error::SvgError;
use crate::geom::Point;
use crate::number::{parse_length, parse_number, parse_opacity};
use crate::style::scale_alpha;
use crate::transform::parse_transform;
use crate::xml::Node;

/// The most colour stops accepted in one gradient.
///
/// A fixed security bound: artwork uses a handful, and the cap keeps a
/// hostile document from making every sampled pixel walk an unbounded list.
const MAX_STOPS: usize = 64;

/// How far a chain of `href`-inheriting gradients is followed.
///
/// A fixed security bound; it is also what makes a cycle terminate.
const MAX_HREF_DEPTH: usize = 8;

/// Every paint server the document defines, indexed by fragment id.
#[derive(Clone, Debug, Default)]
pub struct PaintServers<'a> {
    entries: Vec<(&'a str, &'a Node<'a>)>,
}

impl<'a> PaintServers<'a> {
    /// Index every gradient in the tree.
    ///
    /// The whole tree is walked, not just `<defs>`: SVG lets a paint server
    /// be defined anywhere, and a shape may reference one that appears after
    /// it.
    #[must_use]
    pub fn collect(root: &'a Node<'a>) -> Self {
        let mut servers = Self::default();
        servers.walk(root);
        servers
    }

    /// Record `node` if it is a paint server, then its children.
    fn walk(&mut self, node: &'a Node<'a>) {
        if matches!(node.name, "linearGradient" | "radialGradient") {
            if let Some(id) = node.attr("id") {
                self.entries.push((id, node));
            }
        }
        for child in &node.children {
            self.walk(child);
        }
    }

    /// The element with fragment id `id`, if the document defines one.
    fn find(&self, id: &str) -> Option<&'a Node<'a>> {
        self.entries
            .iter()
            .find(|(name, _)| *name == id)
            .map(|(_, node)| *node)
    }

    /// The paint a `url(#id)` reference resolves to for one shape.
    ///
    /// `bounds` is the shape's object bounding box in user space, `to_design`
    /// the map from that user space onto the design grid, `viewport` the size
    /// percentages in user-space coordinates resolve against, `alpha` the
    /// multiplier the element's fill or stroke opacity contributes, and
    /// `current_color` what a stop's `currentColor` stands for.
    ///
    /// Returns `Ok(None)` when nothing of that name is defined, so the caller
    /// can apply the reference's fallback colour or leave the shape unpainted
    /// rather than inventing a colour.
    ///
    /// # Errors
    /// Returns the parse error of a malformed gradient attribute or stop.
    pub fn resolve(
        &self,
        id: &str,
        bounds: (Point, Point),
        to_design: Affine,
        viewport: (f64, f64),
        alpha: f64,
        current_color: Color,
    ) -> Result<Option<Paint>, SvgError> {
        let Some(node) = self.find(id) else {
            return Ok(None);
        };
        let chain = self.chain(node);
        let stops = Self::stops(&chain, alpha, current_color)?;
        if stops.is_empty() {
            // A gradient with no stops paints nothing at all, which SVG
            // spells as `none` rather than as black.
            return Ok(None);
        }
        let last = stops[stops.len() - 1].color;

        let object_units = !matches!(attribute(&chain, "gradientUnits"), Some("userSpaceOnUse"));
        // In bounding-box units every coordinate is already a fraction of
        // one, so a percentage resolves against 1; in user space it resolves
        // against the viewport, per axis, with the diagonal rule for a radius
        // that has no axis of its own.
        let basis = if object_units {
            Basis::UNIT
        } else {
            Basis::of(viewport)
        };
        let to_user = if object_units {
            let (min, max) = bounds;
            Affine::scale(max.0 - min.0, max.1 - min.1).then(Affine::translate(min.0, min.1))
        } else {
            Affine::IDENTITY
        };
        let gradient_transform = match attribute(&chain, "gradientTransform") {
            Some(text) => parse_transform(text)?,
            None => Affine::IDENTITY,
        };
        let spread = match attribute(&chain, "spreadMethod") {
            None | Some("pad") => SpreadMethod::Pad,
            Some("reflect") => SpreadMethod::Reflect,
            Some("repeat") => SpreadMethod::Repeat,
            Some(_) => return Err(SvgError::InvalidNumber),
        };

        let (kind, canonical) = if node.name == "radialGradient" {
            radial_placement(&chain, &basis)?
        } else {
            linear_placement(&chain, &basis)?
        };
        // A gradient with no extent has no direction to run along; SVG paints
        // the last stop's colour over the whole shape.
        let Some(canonical) = canonical else {
            return Ok(Some(Paint::Solid(last)));
        };

        let to_screen = canonical
            .then(to_user)
            .then(gradient_transform)
            .then(to_design);
        let Some(to_gradient) = to_screen.invert() else {
            return Ok(Some(Paint::Solid(last)));
        };
        Ok(Some(Paint::Gradient(Gradient {
            kind,
            stops,
            spread,
            to_gradient,
        })))
    }

    /// `node` followed by the gradients it inherits from, nearest first.
    fn chain(&self, node: &'a Node<'a>) -> Vec<&'a Node<'a>> {
        let mut chain = alloc::vec![node];
        let mut current = node;
        while chain.len() < MAX_HREF_DEPTH {
            let Some(href) = current.href().and_then(|link| link.strip_prefix('#')) else {
                break;
            };
            let Some(next) = self.find(href) else {
                break;
            };
            // A cycle would otherwise walk to the depth bound every time.
            if chain.iter().any(|seen| core::ptr::eq(*seen, next)) {
                break;
            }
            chain.push(next);
            current = next;
        }
        chain
    }

    /// The colour stops of the nearest gradient in `chain` that defines any.
    fn stops(
        chain: &[&'a Node<'a>],
        alpha: f64,
        current_color: Color,
    ) -> Result<Vec<GradientStop>, SvgError> {
        for node in chain {
            let mut stops: Vec<GradientStop> = Vec::new();
            for child in node.children.iter().filter(|child| child.name == "stop") {
                if stops.len() == MAX_STOPS {
                    return Err(SvgError::TooComplex);
                }
                let stop = parse_stop(child, alpha, current_color)?;
                // Offsets must not go backwards; SVG pulls a smaller one up
                // to its predecessor rather than reordering the list.
                let offset = match stops.last() {
                    Some(previous) => stop.offset.max(previous.offset),
                    None => stop.offset,
                };
                stops.push(GradientStop { offset, ..stop });
            }
            if !stops.is_empty() {
                return Ok(stops);
            }
        }
        Ok(Vec::new())
    }
}

/// The value of `name` on the nearest gradient in the chain that carries it.
fn attribute<'a>(chain: &[&'a Node<'a>], name: &str) -> Option<&'a str> {
    chain.iter().find_map(|node| node.attr(name))
}

/// What a percentage in a gradient's coordinates is a percentage *of*.
struct Basis {
    x: f64,
    y: f64,
    radius: f64,
}

impl Basis {
    /// Bounding-box units: every coordinate is a fraction of one.
    const UNIT: Self = Self {
        x: 1.0,
        y: 1.0,
        radius: 1.0,
    };

    /// User-space units: percentages resolve against the viewport, and a
    /// radius against its diagonal rule.
    fn of(viewport: (f64, f64)) -> Self {
        Self {
            x: viewport.0,
            y: viewport.1,
            radius: sqrt(f64::midpoint(
                viewport.0 * viewport.0,
                viewport.1 * viewport.1,
            )),
        }
    }
}

/// One gradient coordinate: the attribute if present, else `fraction` of the
/// basis, which is how SVG spells every one of their initial values.
fn coordinate<'a>(
    chain: &[&'a Node<'a>],
    name: &str,
    basis: f64,
    fraction: f64,
) -> Result<f64, SvgError> {
    match attribute(chain, name) {
        Some(text) => parse_length(text, basis),
        None => Ok(basis * fraction),
    }
}

/// The canonical placement of a linear gradient: the map taking the unit x
/// axis onto the gradient vector, or `None` when the vector has no length.
fn linear_placement<'a>(
    chain: &[&'a Node<'a>],
    basis: &Basis,
) -> Result<(GradientKind, Option<Affine>), SvgError> {
    let x1 = coordinate(chain, "x1", basis.x, 0.0)?;
    let y1 = coordinate(chain, "y1", basis.y, 0.0)?;
    let x2 = coordinate(chain, "x2", basis.x, 1.0)?;
    let y2 = coordinate(chain, "y2", basis.y, 0.0)?;
    let (dx, dy) = (x2 - x1, y2 - y1);
    if dx == 0.0 && dy == 0.0 {
        return Ok((GradientKind::Linear, None));
    }
    // Map the unit x axis onto the gradient vector: the perpendicular column
    // carries the same vector rotated a quarter turn, which keeps the
    // gradient's bands square to its direction.
    let placement = Affine {
        a: dx,
        b: dy,
        c: -dy,
        d: dx,
        e: x1,
        f: y1,
    };
    Ok((GradientKind::Linear, Some(placement)))
}

/// The canonical placement of a radial gradient: the map taking the unit
/// circle onto the gradient's circle, plus its focal point in that unit
/// space.
fn radial_placement<'a>(
    chain: &[&'a Node<'a>],
    basis: &Basis,
) -> Result<(GradientKind, Option<Affine>), SvgError> {
    let cx = coordinate(chain, "cx", basis.x, 0.5)?;
    let cy = coordinate(chain, "cy", basis.y, 0.5)?;
    let r = coordinate(chain, "r", basis.radius, 0.5)?;
    if r <= 0.0 {
        return Ok((GradientKind::Radial { focal: (0.0, 0.0) }, None));
    }
    // The focal point defaults to the centre, which is the one initial value
    // that is not a fraction of the basis.
    let fx = match attribute(chain, "fx") {
        Some(text) => parse_length(text, basis.x)?,
        None => cx,
    };
    let fy = match attribute(chain, "fy") {
        Some(text) => parse_length(text, basis.y)?,
        None => cy,
    };
    let focal = ((fx - cx) / r, (fy - cy) / r);
    let placement = Affine::scale(r, r).then(Affine::translate(cx, cy));
    Ok((GradientKind::Radial { focal }, Some(placement)))
}

/// Parse one `<stop>`.
fn parse_stop(node: &Node<'_>, alpha: f64, current_color: Color) -> Result<GradientStop, SvgError> {
    let mut color = Color::rgb(0, 0, 0);
    let mut opacity = 1.0;
    let mut offset = 0.0;
    let mut apply = |name: &str, value: &str| -> Result<(), SvgError> {
        match name {
            "offset" => {
                offset = match value.trim().strip_suffix('%') {
                    Some(percent) => parse_number(percent)? / 100.0,
                    None => parse_number(value)?,
                }
                .clamp(0.0, 1.0);
            }
            "stop-color" => match parse_color(value)? {
                ColorSpec::Value(value) => color = value,
                ColorSpec::Current => color = current_color,
                ColorSpec::None => color = Color::rgba(0, 0, 0, 0),
            },
            "stop-opacity" => opacity = parse_opacity(value)?,
            _ => {}
        }
        Ok(())
    };
    for (name, value) in &node.attrs {
        apply(name, value.as_ref())?;
    }
    if let Some(inline) = node.attr("style") {
        for declaration in inline.split(';') {
            if let Some((name, value)) = declaration.split_once(':') {
                apply(name.trim(), value.trim())?;
            }
        }
    }
    Ok(GradientStop {
        offset,
        color: scale_alpha(color, opacity * alpha).unwrap_or(Color::rgba(0, 0, 0, 0)),
    })
}

#[cfg(test)]
#[path = "paint_tests.rs"]
mod tests;
