//! Paint servers: the document's gradients and patterns, and what a
//! `url(#id)` fill resolves to.
//!
//! A paint server is defined once, anywhere in the document, and referenced
//! by any number of shapes — and what it looks like depends on the shape
//! using it, because `objectBoundingBox` units are fractions of *that
//! shape's* bounds. Resolution therefore happens per use, here.
//!
//! A gradient resolves all the way to a [`Paint`] carrying the map from
//! design-grid coordinates back into the gradient's own canonical space,
//! which is what lets the rasteriser sample a rotated or skewed gradient
//! exactly rather than approximating it. A pattern resolves to a
//! [`PatternTile`] — where its tile sits and where its content sits in the
//! tile — because the content is a subtree the document walk has to draw,
//! and that walk is not this module's.
//!
//! A reference that names nothing is not the same as one that names an empty
//! server: the first takes the fallback colour written beside it, the second
//! paints nothing at all. [`Resolved`] is what keeps the two apart.

use alloc::vec::Vec;

use tairix_raster::{Affine, Color, Gradient, GradientKind, GradientStop, Paint, SpreadMethod};
use tairix_util::mathf::sqrt;

use crate::color::{parse_color, ColorSpec};
use crate::error::SvgError;
use crate::geom::Point;
use crate::number::{parse_length, parse_number, parse_opacity};
use crate::style::scale_alpha;
use crate::transform::{
    parse_aspect_ratio, parse_transform, parse_view_box, viewport_transform, AspectRatio, ViewBox,
};
use crate::xml::Element;

/// The most colour stops accepted in one gradient.
///
/// A fixed security bound: artwork uses a handful, and the cap keeps a
/// hostile document from making every sampled pixel walk an unbounded list.
const MAX_STOPS: usize = 64;

/// How far a chain of `href`-inheriting gradients is followed.
///
/// A fixed security bound; it is also what makes a cycle terminate.
const MAX_HREF_DEPTH: usize = 8;

/// What a `url(#id)` paint reference came to.
///
/// The three answers are genuinely different: a name the document does not
/// define falls back to the colour written beside the reference, a server
/// that paints nothing is `none` and takes no fallback, and anything else is
/// a paint.
#[derive(Clone, Debug, PartialEq)]
pub enum Resolved {
    /// The document defines no paint server of that name.
    Unresolved,
    /// A valid paint server with nothing to paint.
    Nothing,
    /// What to fill with.
    Paint(Paint),
}

/// Where a `<pattern>`'s tile sits for one shape, and where the tile's own
/// content sits in it.
#[derive(Clone, Debug)]
pub struct PatternTile<'a> {
    /// Maps a design-grid point into tile space, where one tile is the unit
    /// square.
    pub to_tile: Affine,
    /// Maps the content's own coordinates onto the design grid, of which the
    /// whole is one tile.
    pub content_to_design: Affine,
    /// The viewport the content's percentages resolve against: a tile
    /// establishes one of its own, measured in whichever units the content
    /// is stated in.
    pub content_viewport: (f64, f64),
    /// The element whose children are the tile's content.
    pub content: &'a Element<'a>,
}

/// Every paint server the document defines, indexed by fragment id.
#[derive(Clone, Debug, Default)]
pub struct PaintServers<'a> {
    entries: Vec<(&'a str, &'a Element<'a>)>,
}

impl<'a> PaintServers<'a> {
    /// Index every paint server in the tree.
    ///
    /// The whole tree is walked, not just `<defs>`: SVG lets a paint server
    /// be defined anywhere, and a shape may reference one that appears after
    /// it.
    #[must_use]
    pub fn collect(root: &'a Element<'a>) -> Self {
        let mut servers = Self::default();
        servers.walk(root);
        servers
    }

    /// Record `node` if it is a paint server, then its children.
    fn walk(&mut self, node: &'a Element<'a>) {
        if matches!(node.name, "linearGradient" | "radialGradient" | "pattern") {
            if let Some(id) = node.attr("id") {
                self.entries.push((id, node));
            }
        }
        for child in node.children() {
            self.walk(child);
        }
    }

    /// The paint server with fragment id `id`, if the document defines one.
    ///
    /// A caller needs it to resolve what the server's own place in the
    /// document gives it — the `color` a `currentColor` stop stands for.
    #[must_use]
    pub fn node(&self, id: &str) -> Option<&'a Element<'a>> {
        self.find(id)
    }

    /// The element with fragment id `id`, if the document defines one.
    fn find(&self, id: &str) -> Option<&'a Element<'a>> {
        self.entries
            .iter()
            .find(|(name, _)| *name == id)
            .map(|(_, node)| *node)
    }

    /// The paint one shape fills with through a reference to the gradient
    /// `node`.
    ///
    /// `bounds` is the shape's object bounding box in user space, `to_design`
    /// the map from that user space onto the design grid, `viewport` the size
    /// percentages in user-space coordinates resolve against, `alpha` the
    /// multiplier the element's fill or stroke opacity contributes, and
    /// `current_color` what a stop's `currentColor` stands for.
    ///
    /// # Errors
    /// Returns the parse error of a malformed gradient attribute or stop.
    pub fn gradient(
        &self,
        node: &'a Element<'a>,
        bounds: (Point, Point),
        to_design: Affine,
        viewport: (f64, f64),
        alpha: f64,
        current_color: Color,
    ) -> Result<Resolved, SvgError> {
        let chain = self.chain(node);
        let stops = Self::stops(&chain, alpha, current_color)?;
        if stops.is_empty() {
            // A gradient with no stops is a valid server that paints nothing,
            // which SVG spells as `none` — not as the reference's fallback,
            // which is for a reference that names nothing at all.
            return Ok(Resolved::Nothing);
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
            return Ok(Resolved::Paint(Paint::Solid(last)));
        };

        let to_screen = canonical
            .then(to_user)
            .then(gradient_transform)
            .then(to_design);
        let Some(to_gradient) = to_screen.invert() else {
            return Ok(Resolved::Paint(Paint::Solid(last)));
        };
        Ok(Resolved::Paint(Paint::Gradient(Gradient {
            kind,
            stops,
            spread,
            to_gradient,
        })))
    }

    /// Where the tile of the pattern `node` sits for one shape, and where its
    /// own content sits in that tile.
    ///
    /// The tile is the unit square of [`to_tile`](PatternTile::to_tile), and
    /// the content is placed on the whole of a `grid`-unit design grid, so
    /// the renderer draws one repeat exactly as it draws any other drawing.
    /// A tile establishes a viewport of its own, which is what the content's
    /// percentages resolve against.
    ///
    /// `None` for a pattern that paints nothing: a tile with no area (SVG
    /// disables a pattern whose `width` or `height` is zero) or a placement
    /// that collapses, which has no repeat to render.
    ///
    /// # Errors
    /// Returns the parse error of a malformed pattern attribute.
    pub fn pattern(
        &self,
        node: &'a Element<'a>,
        bounds: (Point, Point),
        to_design: Affine,
        viewport: (f64, f64),
        grid: f64,
    ) -> Result<Option<PatternTile<'a>>, SvgError> {
        let chain = self.chain(node);
        let object_units = !matches!(attribute(&chain, "patternUnits"), Some("userSpaceOnUse"));
        let basis = if object_units {
            Basis::UNIT
        } else {
            Basis::of(viewport)
        };
        // Every one of the four is zero when unstated, which is SVG's way of
        // saying a pattern must state its own tile.
        let x = coordinate(&chain, "x", basis.x, 0.0)?;
        let y = coordinate(&chain, "y", basis.y, 0.0)?;
        let width = coordinate(&chain, "width", basis.x, 0.0)?;
        let height = coordinate(&chain, "height", basis.y, 0.0)?;
        let (min, max) = bounds;
        let (box_width, box_height) = (max.0 - min.0, max.1 - min.1);
        let tile = if object_units {
            (
                min.0 + x * box_width,
                min.1 + y * box_height,
                width * box_width,
                height * box_height,
            )
        } else {
            (x, y, width, height)
        };
        let (tile_x, tile_y, tile_width, tile_height) = tile;
        if tile_width <= 0.0 || tile_height <= 0.0 {
            return Ok(None);
        }

        let pattern_transform = match attribute(&chain, "patternTransform") {
            Some(text) => parse_transform(text)?,
            None => Affine::IDENTITY,
        };
        let placed = Affine::scale(tile_width, tile_height)
            .then(Affine::translate(tile_x, tile_y))
            .then(pattern_transform)
            .then(to_design);
        let Some(to_tile) = placed.invert() else {
            return Ok(None);
        };

        // The tile's own square, whatever its proportions, is the whole
        // design grid the content is drawn on.
        let normalise = Affine::scale(grid / tile_width, grid / tile_height);
        let from_tile_corner = Affine::translate(-tile_x, -tile_y);
        let (content_to_design, content_viewport) = match view_box_of(&chain)? {
            // A `viewBox` fits the content to the tile itself, which is what
            // makes `patternContentUnits` have nothing left to say.
            Some((view_box, ratio)) => (
                viewport_transform(view_box, (tile_width, tile_height), ratio).then(normalise),
                view_box.size,
            ),
            None if content_units_are_bounding_box(&chain) => (
                Affine::scale(box_width, box_height)
                    .then(from_tile_corner)
                    .then(normalise),
                // The tile measured in the fractions the content is stated
                // in. A box with no extent has none to measure it by, so the
                // user-space tile stands in and the collapsed content draws
                // nothing either way.
                if box_width > 0.0 && box_height > 0.0 {
                    (tile_width / box_width, tile_height / box_height)
                } else {
                    (tile_width, tile_height)
                },
            ),
            None => (from_tile_corner.then(normalise), (tile_width, tile_height)),
        };

        Ok(Some(PatternTile {
            to_tile,
            content_to_design,
            content_viewport,
            // A pattern with no children of its own draws the children of the
            // one it inherits from, which is the other half of `href`.
            content: chain
                .iter()
                .copied()
                .find(|link| link.has_children())
                .unwrap_or(node),
        }))
    }

    /// `node` followed by the paint servers it inherits from, nearest first.
    fn chain(&self, node: &'a Element<'a>) -> Vec<&'a Element<'a>> {
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
        chain: &[&'a Element<'a>],
        alpha: f64,
        current_color: Color,
    ) -> Result<Vec<GradientStop>, SvgError> {
        for node in chain {
            let mut stops: Vec<GradientStop> = Vec::new();
            for child in node.children().filter(|child| child.name == "stop") {
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

/// The value of `name` on the nearest server in the chain that carries it.
fn attribute<'a>(chain: &[&'a Element<'a>], name: &str) -> Option<&'a str> {
    chain.iter().find_map(|node| node.attr(name))
}

/// The `viewBox` a pattern's content is fitted to, with the
/// `preserveAspectRatio` that fits it, or `None` when the chain states none.
fn view_box_of<'a>(chain: &[&'a Element<'a>]) -> Result<Option<(ViewBox, AspectRatio)>, SvgError> {
    let Some(text) = attribute(chain, "viewBox") else {
        return Ok(None);
    };
    let view_box = parse_view_box(text)?;
    let ratio = match attribute(chain, "preserveAspectRatio") {
        Some(spec) => parse_aspect_ratio(spec)?,
        None => AspectRatio::default(),
    };
    Ok(Some((view_box, ratio)))
}

/// Whether a pattern's content coordinates are fractions of the filled
/// shape's bounding box. They are user-space lengths unless it says so.
fn content_units_are_bounding_box<'a>(chain: &[&'a Element<'a>]) -> bool {
    matches!(
        attribute(chain, "patternContentUnits"),
        Some("objectBoundingBox")
    )
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
    chain: &[&'a Element<'a>],
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
    chain: &[&'a Element<'a>],
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
    chain: &[&'a Element<'a>],
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
fn parse_stop(
    node: &Element<'_>,
    alpha: f64,
    current_color: Color,
) -> Result<GradientStop, SvgError> {
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
