//! The decoded SVG document and the top-level [`decode`] entry point.
//!
//! [`SvgImage`] is the shared, fast-draw vector form a decoded asset becomes:
//! a square design grid plus an ordered stack of filled [`SvgLayer`]s (bottom
//! layer first), exactly the shape `lib/cursor`'s `VectorCursor` and
//! `lib/icon`'s `VectorIcon` consume. SVG is *converted once* into this form
//! and never re-parsed on the hot compositing path.
//!
//! # How a document becomes layers
//!
//! The tree is walked once, depth first, in document order — which is SVG's
//! painting order. Each element inherits its parent's resolved [`Style`] and
//! accumulated transform; a shape becomes up to two layers, its fill and then
//! its stroke, because that is the order SVG paints them in. Curves, arcs,
//! and stroke outlines have all become polygons by then, so what leaves this
//! module is pure filled geometry.
//!
//! # The design grid
//!
//! Every asset is placed on the same square [`DESIGN_GRID`], whatever its own
//! `viewBox` says, with `preserveAspectRatio` honoured — so a non-square
//! drawing is letter-boxed into the square slot the desktop draws icons and
//! cursors in rather than being stretched or refused. Working in one grid
//! also means curve flattening has a single, known accuracy target.

use alloc::vec::Vec;

use tairix_raster::{Affine, FillRule, Paint};
use tairix_util::mathf::{round_i32, sqrt};

use crate::error::SvgError;
use crate::geom::{bounds, Point, SubPath};
use crate::number::{parse_length, parse_number};
use crate::paint::PaintServers;
use crate::shape::{is_shape, shape_subpaths};
use crate::stroke::stroke_outline;
use crate::style::{scale_alpha, PaintSpec, Style};
use crate::transform::{
    parse_aspect_ratio, parse_transform, parse_view_box, viewport_transform, AspectRatio, ViewBox,
};
use crate::xml::{self, Node};

/// The square design grid every decoded asset is placed on, in design units
/// per side.
///
/// Fine enough that rounding a coordinate onto it is far below one pixel at
/// any size the desktop draws an icon or cursor at, and small enough that the
/// integer coordinates stay comfortable to reason about.
pub const DESIGN_GRID: u32 = 2048;

/// How far a flattened curve may deviate from the true one, in design units.
///
/// A fixed accuracy target rather than a segment count, so a large arc is
/// subdivided more than a small one and neither is over-tessellated.
const FLATTEN_TOLERANCE: f64 = 0.4;

/// The largest number of filled layers a single document may contribute.
///
/// A fixed security bound, not a capacity: it caps what a hostile asset can
/// make the compositor draw per frame.
const MAX_LAYERS: usize = 1024;

/// The largest number of vertices summed across every layer of a document.
///
/// A fixed security bound on the memory and per-frame work one asset can
/// demand.
const MAX_TOTAL_VERTICES: usize = 65_536;

/// How deeply `<use>` references are followed before the document is refused.
///
/// A fixed security bound; it is also what makes a reference cycle terminate.
const MAX_USE_DEPTH: usize = 8;

/// One filled layer of a decoded SVG: a paint, a fill rule, and the contours
/// it applies to, in design-grid coordinates.
///
/// A layer holds *several* contours rather than one ring because a single SVG
/// shape often is several: a path with a hole, a multi-part sub-path, and any
/// stroke outline at all (which is the union of a piece per segment, cap, and
/// join). They are filled together, under one rule, so the pieces merge or
/// cancel as the rule says instead of being composited over one another.
#[derive(Clone, Debug, PartialEq)]
pub struct SvgLayer {
    /// What the layer is painted with.
    pub paint: Paint,
    /// Which points the contours enclose.
    pub rule: FillRule,
    /// The contours, each a ring of `(x, y)` design-grid coordinate pairs.
    pub contours: Vec<Vec<(i32, i32)>>,
}

/// A decoded SVG asset: a square design grid and an ordered stack of filled
/// [`SvgLayer`]s, plus an optional pointer hotspot for cursor assets.
#[derive(Clone, Debug, PartialEq)]
pub struct SvgImage {
    design: u32,
    source: (f64, f64),
    layers: Vec<SvgLayer>,
    hotspot: Option<(i32, i32)>,
}

impl SvgImage {
    /// The side length of the square design grid, in design units.
    #[must_use]
    pub const fn design(&self) -> u32 {
        self.design
    }

    /// The filled layers, bottom layer first.
    #[must_use]
    pub fn layers(&self) -> &[SvgLayer] {
        &self.layers
    }

    /// The pointer hotspot in design units, if the asset declared one
    /// (`data-hotspot-x` / `data-hotspot-y` on the `<svg>` element).
    #[must_use]
    pub const fn hotspot(&self) -> Option<(i32, i32)> {
        self.hotspot
    }

    /// The width and height of the user-space box the document was authored
    /// in, before it was fitted to the design grid.
    ///
    /// The layers themselves are already on the square grid, so this is only
    /// of interest to a caller that has something to say about the *shape* an
    /// asset was drawn in — the build gate that requires an icon master to be
    /// authored square rather than letter-boxed into every slot.
    #[must_use]
    pub const fn source_extent(&self) -> (f64, f64) {
        self.source
    }
}

/// Decode an SVG byte string into an [`SvgImage`].
///
/// The decoder is total: it returns `Ok` for a document it can draw and a
/// precise [`SvgError`] for everything else, and it never panics for any
/// input. It is the single image-decoding entry point the desktop's SVG-first
/// asset pipeline runs untrusted on-disk assets through.
///
/// # Errors
/// See [`SvgError`] for the closed set of rejection reasons.
pub fn decode(bytes: &[u8]) -> Result<SvgImage, SvgError> {
    let text = core::str::from_utf8(bytes).map_err(|_| SvgError::NotUtf8)?;
    let root = xml::parse(text)?;
    if root.name != "svg" {
        return Err(SvgError::MissingRoot);
    }

    let view_box = root_view_box(&root)?;
    let ratio = match root.attr("preserveAspectRatio") {
        Some(text) => parse_aspect_ratio(text)?,
        None => AspectRatio::default(),
    };
    let grid = f64::from(DESIGN_GRID);
    let to_design = viewport_transform(view_box, (grid, grid), ratio);

    let mut decoder = Decoder {
        ids: Vec::new(),
        servers: PaintServers::collect(&root),
        viewport: view_box.size,
        tolerance: FLATTEN_TOLERANCE / to_design.max_scale().max(f64::MIN_POSITIVE),
        layers: Vec::new(),
        vertices_left: MAX_TOTAL_VERTICES,
    };
    decoder.index(&root);
    // The root's own viewport map is already in `to_design`, so its children
    // are walked directly rather than letting the nested-viewport arm apply
    // it a second time.
    let style = Style::default().apply(&root, viewport_diagonal(view_box.size))?;
    if style.display {
        decoder.walk_children(&root, &style, to_design, 0)?;
    }

    Ok(SvgImage {
        design: DESIGN_GRID,
        source: view_box.size,
        layers: decoder.layers,
        hotspot: hotspot(&root, to_design)?,
    })
}

/// The user-space rectangle the root element draws in.
///
/// A `viewBox` states it outright; without one, an absolute `width`/`height`
/// pair does. A document with neither has no coordinate system to draw in and
/// is refused rather than guessed at.
fn root_view_box(root: &Node<'_>) -> Result<ViewBox, SvgError> {
    if let Some(text) = root.attr("viewBox") {
        return parse_view_box(text);
    }
    let (Some(width), Some(height)) = (root.attr("width"), root.attr("height")) else {
        return Err(SvgError::MissingViewBox);
    };
    let w = parse_length(width, 0.0).map_err(|_| SvgError::InvalidViewBox)?;
    let h = parse_length(height, 0.0).map_err(|_| SvgError::InvalidViewBox)?;
    if w <= 0.0 || h <= 0.0 {
        return Err(SvgError::InvalidViewBox);
    }
    Ok(ViewBox {
        min: (0.0, 0.0),
        size: (w, h),
    })
}

/// Read an optional pointer hotspot from the `<svg>` element, in design
/// units.
///
/// Both coordinates must be present together; one without the other is a
/// malformed asset rather than a silent default.
fn hotspot(root: &Node<'_>, to_design: Affine) -> Result<Option<(i32, i32)>, SvgError> {
    match (root.attr("data-hotspot-x"), root.attr("data-hotspot-y")) {
        (Some(x), Some(y)) => {
            let point = to_design.apply((parse_number(x)?, parse_number(y)?));
            Ok(Some((round_i32(point.0), round_i32(point.1))))
        }
        (None, None) => Ok(None),
        _ => Err(SvgError::InvalidNumber),
    }
}

/// The walk's state: what has been drawn so far, and what it needs to draw
/// the rest.
struct Decoder<'a> {
    ids: Vec<(&'a str, &'a Node<'a>)>,
    servers: PaintServers<'a>,
    viewport: (f64, f64),
    tolerance: f64,
    layers: Vec<SvgLayer>,
    vertices_left: usize,
}

impl<'a> Decoder<'a> {
    /// Record every element that carries an `id`, so a `<use>` can find it
    /// wherever in the document it appears.
    fn index(&mut self, node: &'a Node<'a>) {
        if let Some(id) = node.attr("id") {
            self.ids.push((id, node));
        }
        for child in &node.children {
            self.index(child);
        }
    }

    /// Draw `node` and its subtree.
    fn walk(
        &mut self,
        node: &'a Node<'a>,
        inherited: &Style,
        transform: Affine,
        depth: usize,
    ) -> Result<(), SvgError> {
        let diagonal = viewport_diagonal(self.viewport);
        let style = inherited.apply(node, diagonal)?;
        if !style.display {
            return Ok(());
        }
        let transform = match node.attr("transform") {
            Some(text) => parse_transform(text)?.then(transform),
            None => transform,
        };

        if is_shape(node.name) {
            return self.draw(node, &style, transform);
        }
        match node.name {
            "use" => self.expand_use(node, &style, transform, depth),
            "switch" => self.walk_switch(node, &style, transform, depth),
            "svg" => self.walk_viewport(node, &style, transform, depth),
            "g" | "a" => self.walk_children(node, &style, transform, depth),
            // Everything else is either a definition rendered only where it
            // is referenced, or metadata. Both are skipped whole: descending
            // into a `<defs>` would paint its contents twice.
            _ => Ok(()),
        }
    }

    /// Draw every child of `node`.
    fn walk_children(
        &mut self,
        node: &'a Node<'a>,
        style: &Style,
        transform: Affine,
        depth: usize,
    ) -> Result<(), SvgError> {
        let inherited = style.inherit();
        for child in &node.children {
            self.walk(child, &inherited, transform, depth)?;
        }
        Ok(())
    }

    /// Draw a nested `<svg>`, which establishes a viewport of its own that
    /// its children's percentages and `viewBox` resolve against.
    fn walk_viewport(
        &mut self,
        node: &'a Node<'a>,
        style: &Style,
        transform: Affine,
        depth: usize,
    ) -> Result<(), SvgError> {
        let x = optional_length(node, "x", self.viewport.0)?;
        let y = optional_length(node, "y", self.viewport.1)?;
        let width = match node.attr("width") {
            Some(text) => parse_length(text, self.viewport.0)?,
            None => self.viewport.0,
        };
        let height = match node.attr("height") {
            Some(text) => parse_length(text, self.viewport.1)?,
            None => self.viewport.1,
        };
        if width <= 0.0 || height <= 0.0 {
            return Ok(());
        }
        let placed = Affine::translate(x, y).then(transform);
        let (inner, viewport) = match node.attr("viewBox") {
            Some(text) => {
                let view_box = parse_view_box(text)?;
                let ratio = match node.attr("preserveAspectRatio") {
                    Some(spec) => parse_aspect_ratio(spec)?,
                    None => AspectRatio::default(),
                };
                (
                    viewport_transform(view_box, (width, height), ratio).then(placed),
                    view_box.size,
                )
            }
            None => (placed, (width, height)),
        };
        let outer = core::mem::replace(&mut self.viewport, viewport);
        let result = self.walk_children(node, style, inner, depth);
        self.viewport = outer;
        result
    }

    /// Draw the first child of a `<switch>` whose conditions this decoder
    /// meets, and no others.
    fn walk_switch(
        &mut self,
        node: &'a Node<'a>,
        style: &Style,
        transform: Affine,
        depth: usize,
    ) -> Result<(), SvgError> {
        let inherited = style.inherit();
        for child in &node.children {
            if child.attr("requiredExtensions").is_some()
                || child.attr("requiredFeatures").is_some()
            {
                continue;
            }
            return self.walk(child, &inherited, transform, depth);
        }
        Ok(())
    }

    /// Draw what a `<use>` references, at the offset it asks for.
    fn expand_use(
        &mut self,
        node: &'a Node<'a>,
        style: &Style,
        transform: Affine,
        depth: usize,
    ) -> Result<(), SvgError> {
        if depth >= MAX_USE_DEPTH {
            return Err(SvgError::TooComplex);
        }
        let Some(target) = node
            .href()
            .and_then(|link| link.strip_prefix('#'))
            .and_then(|id| self.find(id))
        else {
            // A reference to nothing draws nothing, which is what SVG does
            // with a dangling one.
            return Ok(());
        };
        let x = optional_length(node, "x", self.viewport.0)?;
        let y = optional_length(node, "y", self.viewport.1)?;
        let placed = Affine::translate(x, y).then(transform);
        let inherited = style.inherit();
        // A referenced `symbol` is a container that is only ever drawn
        // through a `use`; anything else is drawn as itself.
        if target.name == "symbol" {
            return self.walk_children(target, &inherited, placed, depth + 1);
        }
        self.walk(target, &inherited, placed, depth + 1)
    }

    /// The element with fragment id `id`.
    fn find(&self, id: &str) -> Option<&'a Node<'a>> {
        self.ids
            .iter()
            .find(|(name, _)| *name == id)
            .map(|(_, node)| *node)
    }

    /// Turn one shape into its fill layer and its stroke layer.
    fn draw(
        &mut self,
        node: &'a Node<'a>,
        style: &Style,
        transform: Affine,
    ) -> Result<(), SvgError> {
        let subpaths = shape_subpaths(node, self.viewport, self.tolerance, self.vertices_left)?;
        if subpaths.is_empty() || !style.visible {
            return Ok(());
        }
        let box_of = bounds(&subpaths);

        let fill = self.paint_of(&style.fill, style, style.fill_opacity, box_of, transform)?;
        if let Some(paint) = fill {
            let contours = place(&subpaths, transform);
            self.push(SvgLayer {
                paint,
                rule: style.fill_rule,
                contours,
            })?;
        }

        let stroke = &style.stroke_style;
        if stroke.width > 0.0 && !matches!(style.stroke, PaintSpec::None) {
            if let Some(paint) = self.paint_of(
                &style.stroke,
                style,
                style.stroke_opacity,
                box_of,
                transform,
            )? {
                let outline =
                    stroke_outline(&subpaths, stroke, self.tolerance, self.vertices_left)?;
                let contours = place(&outline, transform);
                self.push(SvgLayer {
                    paint,
                    // A stroke outline is a union of overlapping pieces, so
                    // only the non-zero rule merges them; even-odd would
                    // punch holes where two pieces meet.
                    rule: FillRule::NonZero,
                    contours,
                })?;
            }
        }
        Ok(())
    }

    /// Resolve one of a shape's paints, or `None` when it paints nothing.
    fn paint_of(
        &self,
        spec: &PaintSpec,
        style: &Style,
        opacity: f64,
        box_of: Option<(Point, Point)>,
        transform: Affine,
    ) -> Result<Option<Paint>, SvgError> {
        if let PaintSpec::Reference(id, fallback) = spec {
            if let Some(extent) = box_of {
                let alpha = style.alpha(opacity);
                let resolved = self.servers.resolve(
                    id,
                    extent,
                    transform,
                    self.viewport,
                    alpha,
                    style.color,
                )?;
                if let Some(paint) = resolved {
                    return Ok(Some(paint));
                }
            }
            // An unresolvable server falls back to the colour written beside
            // it, and to nothing at all when there is none.
            return Ok(fallback
                .and_then(|color| scale_alpha(color, style.alpha(opacity)))
                .map(Paint::Solid));
        }
        let color = match spec {
            PaintSpec::Color(color) => *color,
            PaintSpec::Current => style.color,
            PaintSpec::None | PaintSpec::Reference(_, _) => return Ok(None),
        };
        Ok(scale_alpha(color, style.alpha(opacity)).map(Paint::Solid))
    }

    /// Add a layer, charging its vertices against the document's budget.
    fn push(&mut self, layer: SvgLayer) -> Result<(), SvgError> {
        let vertices: usize = layer.contours.iter().map(Vec::len).sum();
        if vertices == 0 {
            return Ok(());
        }
        if self.layers.len() >= MAX_LAYERS || vertices > self.vertices_left {
            return Err(SvgError::TooComplex);
        }
        self.vertices_left -= vertices;
        self.layers.push(layer);
        Ok(())
    }
}

/// The length a percentage with no axis of its own resolves against.
fn viewport_diagonal(viewport: (f64, f64)) -> f64 {
    sqrt(f64::midpoint(
        viewport.0 * viewport.0,
        viewport.1 * viewport.1,
    ))
}

/// One optional length attribute, defaulting to zero.
fn optional_length(node: &Node<'_>, name: &str, basis: f64) -> Result<f64, SvgError> {
    match node.attr(name) {
        Some(text) => parse_length(text, basis),
        None => Ok(0.0),
    }
}

/// Map flattened sub-paths onto the design grid, dropping any that enclose no
/// area.
fn place(subpaths: &[SubPath], to_design: Affine) -> Vec<Vec<(i32, i32)>> {
    subpaths
        .iter()
        .filter(|sub| !sub.is_degenerate())
        .map(|sub| {
            sub.points
                .iter()
                .map(|point| {
                    let placed = to_design.apply(*point);
                    (round_i32(placed.0), round_i32(placed.1))
                })
                .collect()
        })
        .collect()
}

#[cfg(test)]
#[path = "document_tests.rs"]
mod tests;
