//! The decoded SVG document and the top-level [`decode`] entry point.
//!
//! [`SvgImage`] is the shared, fast-draw vector form a decoded asset becomes:
//! a square design grid plus the `tairix_raster` artwork tree `lib/cursor`'s
//! `VectorCursor`, `lib/icon`'s `VectorIcon`, and the document viewer all
//! draw. SVG is *converted once* into that form and never re-parsed on the
//! hot compositing path.
//!
//! # How a document becomes artwork
//!
//! The tree is walked once, depth first, in document order — which is SVG's
//! painting order. Each element inherits its parent's resolved [`Style`] and
//! accumulated transform; a shape becomes up to two layers, its fill and then
//! its stroke, because that is the order SVG paints them in. Curves, arcs,
//! and stroke outlines have all become polygons by then, so what leaves this
//! module is pure filled geometry.
//!
//! An element whose opacity, `clip-path`, or `mask` makes its subtree a
//! composited unit becomes a [`Group`] around that geometry. An element that
//! needs none of the three adds no group at all, so a flat asset decodes to a
//! flat list. Which of the three is the outer group does not matter: each is
//! a per-pixel scalar on the composited subtree, and those commute — only the
//! isolation itself changes the picture.
//!
//! # The design grid
//!
//! Every asset is placed on the same [`DESIGN_GRID`] whatever its own
//! `viewBox` says, so a consumer never rescales between assets and curve
//! flattening has a single, known accuracy target. [`Viewport`] chooses how
//! a drawing is fitted to it: letter-boxed under `preserveAspectRatio` into
//! the square slot the desktop draws icons and cursors in, or normalised
//! across both axes for a consumer that will rasterise into the drawing's
//! own shape.

use alloc::vec::Vec;

use tairix_raster::{Affine, Color, FillRule, Group, Layer, Mask, MaskKind, Node, Paint};
use tairix_util::mathf::{round_i32, sqrt};

use crate::css::{Declaration, Stylesheet};
use crate::error::SvgError;
use crate::geom::{bounds, Point, SubPath};
use crate::number::{opacity_to_alpha, parse_length, parse_number};
use crate::paint::PaintServers;
use crate::shape::{is_shape, shape_subpaths};
use crate::stroke::stroke_outline;
use crate::style::{scale_alpha, Overflow, PaintOrder, PaintSpec, Style};
use crate::transform::{
    parse_aspect_ratio, parse_transform, parse_view_box, viewport_transform, Align, AspectRatio,
    ViewBox,
};
use crate::xml::{self, Element};

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

/// How deeply composited groups may nest before the document is refused.
///
/// The renderer's own bound, taken from it rather than restated, so a
/// document this decoder admits is one the renderer can draw. It is also what
/// makes a cycle of elements clipping one another terminate.
const MAX_NESTING: usize = tairix_raster::MAX_GROUP_DEPTH;

/// What a clip's own shapes are filled with: any opaque colour does, since
/// only the alpha is read back.
const CLIP_INK: Color = Color::rgb(255, 255, 255);

/// A `<mask>`'s region when it states none, as a fraction of the object
/// bounding box: a tenth past the box on every side.
const MASK_REGION: (f64, f64, f64, f64) = (-0.1, -0.1, 1.2, 1.2);

/// The shape a document's drawing is fitted to when it is decoded.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum Viewport {
    /// The square slot the desktop draws an icon or a cursor in.
    ///
    /// A drawing that is not square is letter-boxed into it under the
    /// document's own `preserveAspectRatio`, so the artwork keeps its shape
    /// and every slot keeps its size.
    #[default]
    Square,
    /// The document's own shape.
    ///
    /// The drawing is normalised across the whole grid, and
    /// [`SvgImage::source_extent`] carries the proportions it was authored
    /// in — so a consumer that rasterises into a surface of that shape gets
    /// the picture undistorted, at the grid's full precision on *both* axes,
    /// and with no letter-box bands to find and crop. A viewer showing a
    /// picture wants this; a slot to fill wants [`Square`](Self::Square).
    ///
    /// `preserveAspectRatio` states how to fit a drawing into a viewport of
    /// a *different* shape, so it has nothing to say here — though a
    /// malformed one still refuses the document, so no file is well formed
    /// under one viewport and malformed under the other.
    ///
    /// Filling the grid on both axes does mean a curve is flattened to the
    /// tolerance of the *larger* scale, so a drawing already close to the
    /// total-vertex bound can pass it here and be refused as too complex
    /// when the letter-boxed fit would have admitted it. The bound is a
    /// containment bound and is not relaxed to suit a shape.
    Natural,
}

/// A decoded SVG asset: a square design grid and the artwork drawn on it,
/// plus an optional pointer hotspot for cursor assets.
#[derive(Clone, Debug, PartialEq)]
pub struct SvgImage {
    design: u32,
    source: (f64, f64),
    nodes: Vec<Node>,
    hotspot: Option<(i32, i32)>,
}

impl SvgImage {
    /// The side length of the square design grid, in design units.
    #[must_use]
    pub const fn design(&self) -> u32 {
        self.design
    }

    /// The artwork, bottom first.
    #[must_use]
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
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
    /// The artwork itself is already on the square grid, so this is only of
    /// interest to a caller that has something to say about the *shape* an
    /// asset was drawn in — the build gate that requires an icon master to be
    /// authored square rather than letter-boxed into every slot.
    #[must_use]
    pub const fn source_extent(&self) -> (f64, f64) {
        self.source
    }
}

/// Decode an SVG byte string into an [`SvgImage`] fitted to `viewport`.
///
/// The decoder is total: it returns `Ok` for a document it can draw and a
/// precise [`SvgError`] for everything else, and it never panics for any
/// input. It is the single image-decoding entry point the desktop's SVG-first
/// asset pipeline runs untrusted on-disk assets through.
///
/// `viewport` chooses the *shape* the drawing is fitted to. It never changes
/// what counts as a well-formed document; see [`Viewport::Natural`] for the
/// one thing it does change.
///
/// # Errors
/// See [`SvgError`] for the closed set of rejection reasons.
pub fn decode(bytes: &[u8], viewport: Viewport) -> Result<SvgImage, SvgError> {
    let text = core::str::from_utf8(bytes).map_err(|_| SvgError::NotUtf8)?;
    let root = xml::parse(text)?;
    if root.name != "svg" {
        return Err(SvgError::MissingRoot);
    }

    let view_box = root_view_box(&root)?;
    // Parsed whichever viewport is asked for, so a malformed attribute
    // refuses the document either way, and read only where it means
    // something.
    let declared = match root.attr("preserveAspectRatio") {
        Some(text) => parse_aspect_ratio(text)?,
        None => AspectRatio::default(),
    };
    let ratio = match viewport {
        Viewport::Square => declared,
        Viewport::Natural => AspectRatio {
            align: Align::None,
            slice: false,
        },
    };
    let grid = f64::from(DESIGN_GRID);
    let to_design = viewport_transform(view_box, (grid, grid), ratio);

    let mut decoder = Decoder {
        root: &root,
        ids: Vec::new(),
        chains: Vec::new(),
        colors: Vec::new(),
        servers: PaintServers::collect(&root),
        sheet: Stylesheet::collect(&root)?,
        cascade: Vec::new(),
        path: Vec::new(),
        extents: Vec::new(),
        viewport: view_box.size,
        tolerance: FLATTEN_TOLERANCE / to_design.max_scale().max(f64::MIN_POSITIVE),
        vertices_left: MAX_TOTAL_VERTICES,
        layers_left: MAX_LAYERS,
        nesting: 0,
    };
    decoder.index(&root);

    let mut nodes = Vec::new();
    decoder.path.push(&root);
    // The root's own viewport map is already in `to_design`, so its children
    // are walked directly rather than letting the nested-viewport arm apply
    // it a second time.
    let style = decoder.style_of(&Style::default(), &root)?;
    if style.display {
        decoder.walk_children(&root, &style, to_design, 0, &mut nodes)?;
    }
    decoder.path.pop();

    Ok(SvgImage {
        design: DESIGN_GRID,
        source: view_box.size,
        nodes,
        hotspot: hotspot(&root, to_design)?,
    })
}

/// The user-space rectangle the root element draws in.
///
/// A `viewBox` states it outright; without one, an absolute `width`/`height`
/// pair does. A document with neither has no coordinate system to draw in and
/// is refused rather than guessed at.
fn root_view_box(root: &Element<'_>) -> Result<ViewBox, SvgError> {
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
fn hotspot(root: &Element<'_>, to_design: Affine) -> Result<Option<(i32, i32)>, SvgError> {
    match (root.attr("data-hotspot-x"), root.attr("data-hotspot-y")) {
        (Some(x), Some(y)) => {
            let point = to_design.apply((parse_number(x)?, parse_number(y)?));
            Ok(Some((round_i32(point.0), round_i32(point.1))))
        }
        (None, None) => Ok(None),
        _ => Err(SvgError::InvalidNumber),
    }
}

/// An object bounding box being accumulated, in the user space of the element
/// that asked for one.
///
/// `objectBoundingBox` units are fractions of the *element's* box, and a
/// container's box is the union of its descendants' fill geometry measured in
/// that container's own space. Accumulating as the subtree is drawn is what
/// makes it exact under a rotation, where the box of an already-placed box
/// would not be — and it costs nothing for the elements that never ask.
struct Extent {
    from_design: Affine,
    box_of: Option<(Point, Point)>,
}

/// A `<mask>`'s region rectangle and the map that places it.
struct Region {
    rect: (f64, f64, f64, f64),
    transform: Affine,
}

/// The walk's state: what it has drawn so far, and what it needs to draw the
/// rest.
struct Decoder<'a> {
    root: &'a Element<'a>,
    ids: Vec<(&'a str, &'a Element<'a>)>,
    chains: Vec<(&'a Element<'a>, Vec<&'a Element<'a>>)>,
    colors: Vec<(&'a Element<'a>, Color)>,
    servers: PaintServers<'a>,
    sheet: Stylesheet<'a>,
    cascade: Vec<Declaration<'a>>,
    path: Vec<&'a Element<'a>>,
    extents: Vec<Extent>,
    viewport: (f64, f64),
    tolerance: f64,
    vertices_left: usize,
    layers_left: usize,
    nesting: usize,
}

impl<'a> Decoder<'a> {
    /// Record every element that carries an `id`, so a reference can find it
    /// wherever in the document it appears.
    fn index(&mut self, element: &'a Element<'a>) {
        if let Some(id) = element.attr("id") {
            self.ids.push((id, element));
        }
        for child in &element.children {
            self.index(child);
        }
    }

    /// The element with fragment id `id`.
    fn find(&self, id: &str) -> Option<&'a Element<'a>> {
        self.ids
            .iter()
            .find(|(name, _)| *name == id)
            .map(|(_, element)| *element)
    }

    /// The chain of elements from the document root down to `node`.
    ///
    /// Memoised, because it is looked up once per definition a document
    /// actually references and the search is a walk of the tree.
    fn chain_of(&mut self, node: &'a Element<'a>) -> Vec<&'a Element<'a>> {
        if let Some((_, chain)) = self
            .chains
            .iter()
            .find(|(key, _)| core::ptr::eq(*key, node))
        {
            return chain.clone();
        }
        let mut chain = Vec::new();
        descend(self.root, node, &mut chain);
        self.chains.push((node, chain.clone()));
        chain
    }

    /// Resolve a referenced definition's own style and leave the selector
    /// path at it, ready for its children.
    ///
    /// A `<clipPath>` or a `<mask>` is reached by reference, so what it
    /// inherits is its own place in the document — not the style of whatever
    /// element happened to point at it. The caller has already set the path
    /// aside and restores it afterwards.
    fn enter_definition(&mut self, node: &'a Element<'a>) -> Result<Style, SvgError> {
        let chain = self.chain_of(node);
        let mut style = Style::default();
        for (index, element) in chain.iter().enumerate() {
            self.path.push(element);
            let resolved = self.style_of(&style, element)?;
            style = if index + 1 == chain.len() {
                resolved
            } else {
                resolved.inherit()
            };
        }
        Ok(style)
    }

    /// The `color` a referenced definition's own ancestry gives it.
    ///
    /// Memoised: the answer depends only on where the definition sits, and a
    /// document may paint hundreds of shapes with one gradient.
    fn definition_color(&mut self, node: &'a Element<'a>) -> Result<Color, SvgError> {
        if let Some((_, color)) = self
            .colors
            .iter()
            .find(|(key, _)| core::ptr::eq(*key, node))
        {
            return Ok(*color);
        }
        let outer = core::mem::take(&mut self.path);
        let resolved = self.enter_definition(node);
        self.path = outer;
        let color = resolved?.color;
        self.colors.push((node, color));
        Ok(color)
    }

    /// Note that what follows is built inside `levels` further groups.
    ///
    /// Paired with [`leave`](Self::leave) on the way out. An error abandons
    /// the whole decode, so the count is not unwound on one.
    fn enter(&mut self, levels: usize) -> Result<(), SvgError> {
        self.nesting += levels;
        if self.nesting > MAX_NESTING {
            return Err(SvgError::TooComplex);
        }
        Ok(())
    }

    /// The counterpart of [`enter`](Self::enter).
    fn leave(&mut self, levels: usize) {
        self.nesting -= levels;
    }

    /// Whether one more enclosing group still fits the nesting bound.
    fn fits_group(&self) -> Result<(), SvgError> {
        (self.nesting < MAX_NESTING)
            .then_some(())
            .ok_or(SvgError::TooComplex)
    }

    /// `element`'s resolved style, given what it inherits.
    ///
    /// The element must already be the last of [`Decoder::path`], which is
    /// what the stylesheet matches its selectors against.
    fn style_of(&mut self, inherited: &Style, element: &Element<'_>) -> Result<Style, SvgError> {
        self.cascade.clear();
        if !self.sheet.is_empty() {
            self.sheet.cascade(&self.path, &mut self.cascade);
        }
        inherited.apply(element, viewport_diagonal(self.viewport), &self.cascade)
    }

    /// Draw `element` and its subtree into `out`.
    fn walk(
        &mut self,
        element: &'a Element<'a>,
        inherited: &Style,
        transform: Affine,
        depth: usize,
        out: &mut Vec<Node>,
    ) -> Result<(), SvgError> {
        self.path.push(element);
        let drawn = self.walk_element(element, inherited, transform, depth, out);
        self.path.pop();
        drawn
    }

    /// [`walk`](Self::walk) with `element` already on the selector path.
    fn walk_element(
        &mut self,
        element: &'a Element<'a>,
        inherited: &Style,
        transform: Affine,
        depth: usize,
        out: &mut Vec<Node>,
    ) -> Result<(), SvgError> {
        let style = self.style_of(inherited, element)?;
        if !style.display {
            return Ok(());
        }
        let transform = match element.attr("transform") {
            Some(text) => parse_transform(text)?.then(transform),
            None => transform,
        };
        let opacity = opacity_to_alpha(style.opacity);
        if opacity == 0 {
            return Ok(());
        }

        let wrappers = usize::from(style.clip_path.is_some())
            + usize::from(style.mask.is_some())
            + usize::from(opacity != u8::MAX);
        // A clip or a mask in bounding-box units needs the box this element's
        // own geometry comes to, which only drawing it can say.
        let measured = self.opens_extent(&style);
        if measured {
            self.extents.push(Extent {
                from_design: transform.invert().unwrap_or(Affine::IDENTITY),
                box_of: None,
            });
        }
        self.enter(wrappers)?;
        let mut content = Vec::new();
        let handled = self.content_of(element, &style, transform, depth, opacity, &mut content)?;
        self.leave(wrappers);
        let box_of = measured
            .then(|| self.extents.pop().and_then(|extent| extent.box_of))
            .flatten();

        let mut wrapped = self.composite(&style, transform, box_of, depth, content)?;
        if opacity != u8::MAX && !handled {
            wrapped = grouped(opacity, None, wrapped);
        }
        out.append(&mut wrapped);
        Ok(())
    }

    /// Draw what `element` itself contributes, before any group wraps it,
    /// answering whether the element's own opacity is already accounted for.
    ///
    /// A shape says `true`: it folds the opacity into a lone layer, where the
    /// pixels are identical, and isolates itself only when its fill and its
    /// stroke would otherwise show through one another.
    fn content_of(
        &mut self,
        element: &'a Element<'a>,
        style: &Style,
        transform: Affine,
        depth: usize,
        opacity: u8,
        out: &mut Vec<Node>,
    ) -> Result<bool, SvgError> {
        if is_shape(element.name) {
            self.draw(element, style, transform, opacity, out)?;
            return Ok(true);
        }
        match element.name {
            "use" => self.expand_use(element, style, transform, depth, out)?,
            "switch" => self.walk_switch(element, style, transform, depth, out)?,
            "svg" => self.walk_viewport(element, style, transform, depth, None, out)?,
            "g" | "a" => self.walk_children(element, style, transform, depth, out)?,
            // Everything else is either a definition rendered only where it
            // is referenced, or metadata. Both are skipped whole: descending
            // into a `<defs>` would paint its contents twice.
            _ => {}
        }
        Ok(false)
    }

    /// Wrap `content` in the clip and mask `style` asks for.
    ///
    /// A reference that resolves to nothing means the element is not
    /// rendered: drawing it unclipped or unmasked would be a wrong picture
    /// where an empty one is an honest refusal.
    fn composite(
        &mut self,
        style: &Style,
        transform: Affine,
        box_of: Option<(Point, Point)>,
        depth: usize,
        content: Vec<Node>,
    ) -> Result<Vec<Node>, SvgError> {
        let mut wrapped = content;
        if let Some(id) = style.clip_path.clone() {
            self.enter(1)?;
            let mask = self.clip_mask(&id, transform, box_of, depth)?;
            self.leave(1);
            let Some(mask) = mask else {
                return Ok(Vec::new());
            };
            wrapped = grouped(u8::MAX, Some(mask), wrapped);
        }
        if let Some(id) = style.mask.clone() {
            self.enter(1)?;
            let mask = self.element_mask(&id, transform, box_of, depth)?;
            self.leave(1);
            let Some(mask) = mask else {
                return Ok(Vec::new());
            };
            wrapped = grouped(u8::MAX, Some(mask), wrapped);
        }
        Ok(wrapped)
    }

    /// Whether this element's clip or mask is stated in bounding-box units,
    /// so its own geometry must be measured as it is drawn.
    fn opens_extent(&self, style: &Style) -> bool {
        let clip = style.clip_path.as_deref().is_some_and(|id| {
            self.find(id)
                .is_some_and(|node| units_are_bounding_box(node, "clipPathUnits", false))
        });
        // A `<mask>`'s region is in bounding-box units by default, so a mask
        // almost always needs the box.
        let mask = style.mask.as_deref().is_some_and(|id| {
            self.find(id).is_some_and(|node| {
                units_are_bounding_box(node, "maskUnits", true)
                    || units_are_bounding_box(node, "maskContentUnits", false)
            })
        });
        clip || mask
    }

    /// Fold one shape's fill geometry into every bounding box being measured.
    ///
    /// The stroke is deliberately left out: SVG's object bounding box is the
    /// geometry's, whatever it is drawn with.
    fn measure(&mut self, subpaths: &[SubPath], transform: Affine) {
        for extent in &mut self.extents {
            let to_extent = transform.then(extent.from_design);
            for point in subpaths.iter().flat_map(|sub| sub.points.iter()) {
                let placed = to_extent.apply(*point);
                extent.box_of = Some(match extent.box_of {
                    None => (placed, placed),
                    Some((min, max)) => (
                        (min.0.min(placed.0), min.1.min(placed.1)),
                        (max.0.max(placed.0), max.1.max(placed.1)),
                    ),
                });
            }
        }
    }

    /// Draw every child of `element`.
    fn walk_children(
        &mut self,
        element: &'a Element<'a>,
        style: &Style,
        transform: Affine,
        depth: usize,
        out: &mut Vec<Node>,
    ) -> Result<(), SvgError> {
        let inherited = style.inherit();
        for child in &element.children {
            self.walk(child, &inherited, transform, depth, out)?;
        }
        Ok(())
    }

    /// Draw a viewport-establishing element — a nested `<svg>`, or a
    /// `<symbol>` reached through a `<use>` — whose children's percentages
    /// and `viewBox` resolve against a viewport of its own.
    ///
    /// `slot` is the width and height a `<use>` states for a `<symbol>`,
    /// which override the element's own.
    fn walk_viewport(
        &mut self,
        element: &'a Element<'a>,
        style: &Style,
        transform: Affine,
        depth: usize,
        slot: Option<(Option<f64>, Option<f64>)>,
        out: &mut Vec<Node>,
    ) -> Result<(), SvgError> {
        let x = optional_length(element, "x", self.viewport.0)?;
        let y = optional_length(element, "y", self.viewport.1)?;
        let (slot_width, slot_height) = slot.unwrap_or((None, None));
        let width = match slot_width {
            Some(width) => width,
            None => {
                optional_attr_length(element, "width", self.viewport.0)?.unwrap_or(self.viewport.0)
            }
        };
        let height = match slot_height {
            Some(height) => height,
            None => {
                optional_attr_length(element, "height", self.viewport.1)?.unwrap_or(self.viewport.1)
            }
        };
        if width <= 0.0 || height <= 0.0 {
            return Ok(());
        }
        let placed = Affine::translate(x, y).then(transform);
        let (inner, viewport) = match element.attr("viewBox") {
            Some(text) => {
                let view_box = parse_view_box(text)?;
                let ratio = match element.attr("preserveAspectRatio") {
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

        // A viewport clips its content unless the author says otherwise,
        // which is what keeps a `<symbol>` inside the slot a `<use>` gave it.
        let clips = style.overflow == Overflow::Hidden;
        let levels = usize::from(clips);
        self.enter(levels)?;
        let outer = core::mem::replace(&mut self.viewport, viewport);
        let mut content = Vec::new();
        let drawn = self.walk_children(element, style, inner, depth, &mut content);
        self.viewport = outer;
        drawn?;
        self.leave(levels);

        let mut clipped = if clips {
            self.clipped_to((0.0, 0.0, width, height), placed, content)?
        } else {
            content
        };
        out.append(&mut clipped);
        Ok(())
    }

    /// `content` confined to one rectangle of the drawing.
    ///
    /// Artwork already inside the rectangle composites identically without a
    /// group, so the isolation buffer is allocated only where the rectangle
    /// actually cuts something — which is what keeps a `<symbol>` drawn at
    /// its own size, or a mask region wider than its content, free.
    fn clipped_to(
        &mut self,
        rect: (f64, f64, f64, f64),
        transform: Affine,
        content: Vec<Node>,
    ) -> Result<Vec<Node>, SvgError> {
        if content.is_empty() || rect_contains(rect, transform, &content) {
            return Ok(content);
        }
        let mask = self.rect_mask(rect, transform)?;
        Ok(grouped(u8::MAX, Some(mask), content))
    }

    /// Draw the first child of a `<switch>` whose conditions this decoder
    /// meets, and no others.
    fn walk_switch(
        &mut self,
        element: &'a Element<'a>,
        style: &Style,
        transform: Affine,
        depth: usize,
        out: &mut Vec<Node>,
    ) -> Result<(), SvgError> {
        let inherited = style.inherit();
        for child in &element.children {
            if !is_switchable(child.name) || !conditions_met(child) {
                continue;
            }
            return self.walk(child, &inherited, transform, depth, out);
        }
        Ok(())
    }

    /// Draw what a `<use>` references, at the offset it asks for.
    fn expand_use(
        &mut self,
        element: &'a Element<'a>,
        style: &Style,
        transform: Affine,
        depth: usize,
        out: &mut Vec<Node>,
    ) -> Result<(), SvgError> {
        if depth >= MAX_USE_DEPTH {
            return Err(SvgError::TooComplex);
        }
        let Some(target) = self.referenced(element) else {
            // A reference to nothing draws nothing, which is what SVG does
            // with a dangling one.
            return Ok(());
        };
        let x = optional_length(element, "x", self.viewport.0)?;
        let y = optional_length(element, "y", self.viewport.1)?;
        let placed = Affine::translate(x, y).then(transform);
        let inherited = style.inherit();
        // A `<symbol>` is a container drawn only through a `<use>`, and the
        // extent the `<use>` states is the viewport it is fitted to.
        if target.name == "symbol" {
            let slot = (
                optional_attr_length(element, "width", self.viewport.0)?,
                optional_attr_length(element, "height", self.viewport.1)?,
            );
            self.path.push(target);
            let resolved = self.style_of(&inherited, target);
            let drawn = resolved.and_then(|style| {
                if style.display {
                    self.walk_viewport(target, &style, placed, depth + 1, Some(slot), out)
                } else {
                    Ok(())
                }
            });
            self.path.pop();
            return drawn;
        }
        self.walk(target, &inherited, placed, depth + 1, out)
    }

    /// The element a reference attribute names, if the document defines one.
    fn referenced(&self, element: &Element<'_>) -> Option<&'a Element<'a>> {
        element
            .href()
            .and_then(|link| link.strip_prefix('#'))
            .and_then(|id| self.find(id))
    }

    /// Turn one shape into its fill layer and its stroke layer.
    fn draw(
        &mut self,
        element: &'a Element<'a>,
        style: &Style,
        transform: Affine,
        opacity: u8,
        out: &mut Vec<Node>,
    ) -> Result<(), SvgError> {
        let subpaths = shape_subpaths(element, self.viewport, self.tolerance, self.vertices_left)?;
        if subpaths.is_empty() {
            return Ok(());
        }
        self.measure(&subpaths, transform);
        if !style.visible {
            return Ok(());
        }
        let box_of = bounds(&subpaths);
        let group = f64::from(opacity) / 255.0;
        let stroked = style.stroke_style.width > 0.0 && !matches!(style.stroke, PaintSpec::None);

        let mut fill = self.paint_of(
            &style.fill,
            style,
            style.fill_opacity * group,
            box_of,
            transform,
        )?;
        let mut stroke = if stroked {
            self.paint_of(
                &style.stroke,
                style,
                style.stroke_opacity * group,
                box_of,
                transform,
            )?
        } else {
            None
        };
        // Two layers of one element overlap, so folding the element's opacity
        // into each would show the fill through its own stroke. Those two are
        // composited as a unit instead, at their own opacities. One layer
        // needs no buffer: painting it at the product is the same pixels.
        let isolate = opacity != u8::MAX && fill.is_some() && stroke.is_some();
        if isolate {
            self.fits_group()?;
            fill = self.paint_of(&style.fill, style, style.fill_opacity, box_of, transform)?;
            stroke = self.paint_of(
                &style.stroke,
                style,
                style.stroke_opacity,
                box_of,
                transform,
            )?;
        }

        let fill =
            fill.map(|paint| Layer::filled(paint, style.fill_rule, place(&subpaths, transform)));
        let stroke = match stroke {
            Some(paint) => {
                let outline = stroke_outline(
                    &subpaths,
                    &style.stroke_style,
                    self.tolerance,
                    self.vertices_left,
                )?;
                // A stroke outline is a union of overlapping pieces, so only
                // the non-zero rule merges them; even-odd would punch holes
                // where two pieces meet.
                Some(Layer::filled(
                    paint,
                    FillRule::NonZero,
                    place(&outline, transform),
                ))
            }
            None => None,
        };

        let mut layers = Vec::new();
        let ordered = match style.paint_order {
            PaintOrder::StrokeFirst => [stroke, fill],
            PaintOrder::FillFirst => [fill, stroke],
        };
        for layer in ordered.into_iter().flatten() {
            self.push(layer, &mut layers)?;
        }
        let mut drawn = if isolate {
            grouped(opacity, None, layers)
        } else {
            layers
        };
        out.append(&mut drawn);
        Ok(())
    }

    /// Resolve one of a shape's paints, or `None` when it paints nothing.
    fn paint_of(
        &mut self,
        spec: &PaintSpec,
        style: &Style,
        alpha: f64,
        box_of: Option<(Point, Point)>,
        transform: Affine,
    ) -> Result<Option<Paint>, SvgError> {
        let alpha = alpha.clamp(0.0, 1.0);
        if let PaintSpec::Reference(id, fallback) = spec {
            if let Some(extent) = box_of {
                // A `currentColor` stop stands for the `color` the gradient's
                // own ancestry gives it, exactly as a clip or a mask takes its
                // style from where it sits rather than from its user.
                let current = match self.servers.node(id) {
                    Some(node) => self.definition_color(node)?,
                    None => style.color,
                };
                let resolved =
                    self.servers
                        .resolve(id, extent, transform, self.viewport, alpha, current)?;
                if let Some(paint) = resolved {
                    return Ok(Some(paint));
                }
            }
            // An unresolvable server falls back to the colour written beside
            // it, and to nothing at all when there is none.
            return Ok(fallback
                .and_then(|color| scale_alpha(color, alpha))
                .map(Paint::Solid));
        }
        let color = match spec {
            PaintSpec::Color(color) => *color,
            PaintSpec::Current => style.color,
            PaintSpec::None | PaintSpec::Reference(_, _) => return Ok(None),
        };
        Ok(scale_alpha(color, alpha).map(Paint::Solid))
    }

    /// The mask a `clip-path` reference resolves to, or `None` when the
    /// document defines no such `<clipPath>`.
    fn clip_mask(
        &mut self,
        id: &str,
        transform: Affine,
        box_of: Option<(Point, Point)>,
        depth: usize,
    ) -> Result<Option<Mask>, SvgError> {
        let Some(node) = self.find(id).filter(|node| node.name == "clipPath") else {
            return Ok(None);
        };
        let placed = if units_are_bounding_box(node, "clipPathUnits", false) {
            match bounding_box_units(box_of) {
                Some(units) => units.then(transform),
                None => return Ok(None),
            }
        } else {
            transform
        };

        let outer = core::mem::take(&mut self.path);
        let content = self.enter_definition(node).and_then(|own| {
            let mut content = Vec::new();
            self.clip_shapes(node, &own, placed, depth, &mut content)?;
            // A `<clipPath>` may itself be clipped, which intersects the two.
            self.composite(&own, placed, box_of, depth, content)
        });
        self.path = outer;

        Ok(Some(Mask {
            kind: MaskKind::Alpha,
            content: content?,
        }))
    }

    /// Collect the opaque shapes a `<clipPath>`'s children contribute.
    ///
    /// The clip's own `<use>` indirection is followed, and a child that is
    /// neither a shape nor a reference to one contributes nothing — which is
    /// what SVG says of a `<clipPath>` holding anything else.
    fn clip_shapes(
        &mut self,
        node: &'a Element<'a>,
        inherited: &Style,
        transform: Affine,
        depth: usize,
        out: &mut Vec<Node>,
    ) -> Result<(), SvgError> {
        let inherited = inherited.inherit();
        for child in &node.children {
            self.path.push(child);
            let resolved = self.style_of(&inherited, child);
            self.path.pop();
            let style = resolved?;
            if !style.display || !style.visible {
                continue;
            }
            let placed = match child.attr("transform") {
                Some(text) => parse_transform(text)?.then(transform),
                None => transform,
            };
            let (shape, placed) = match child.name {
                "use" => {
                    let Some(target) = self.referenced(child).filter(|node| is_shape(node.name))
                    else {
                        continue;
                    };
                    let x = optional_length(child, "x", self.viewport.0)?;
                    let y = optional_length(child, "y", self.viewport.1)?;
                    (target, Affine::translate(x, y).then(placed))
                }
                name if is_shape(name) => (child, placed),
                _ => continue,
            };
            let subpaths =
                shape_subpaths(shape, self.viewport, self.tolerance, self.vertices_left)?;
            if subpaths.is_empty() {
                continue;
            }
            let mut piece = Vec::new();
            self.push(
                Layer::filled(
                    Paint::Solid(CLIP_INK),
                    style.clip_rule,
                    place(&subpaths, placed),
                ),
                &mut piece,
            )?;
            // A child of a clip may carry a clip of its own, which narrows
            // only that child's contribution to the union.
            let mut piece = self.composite(&style, placed, bounds(&subpaths), depth, piece)?;
            out.append(&mut piece);
        }
        Ok(())
    }

    /// The mask a `mask` reference resolves to, or `None` when the document
    /// defines no such `<mask>`.
    fn element_mask(
        &mut self,
        id: &str,
        transform: Affine,
        box_of: Option<(Point, Point)>,
        depth: usize,
    ) -> Result<Option<Mask>, SvgError> {
        let Some(node) = self.find(id).filter(|node| node.name == "mask") else {
            return Ok(None);
        };
        let boxed = units_are_bounding_box(node, "maskUnits", true)
            || units_are_bounding_box(node, "maskContentUnits", false);
        let units = if boxed {
            match bounding_box_units(box_of) {
                Some(units) => Some(units.then(transform)),
                None => return Ok(None),
            }
        } else {
            None
        };
        let region = match (units_are_bounding_box(node, "maskUnits", true), units) {
            (true, Some(placed)) => Region {
                rect: mask_region(node, (1.0, 1.0), MASK_REGION)?,
                transform: placed,
            },
            _ => Region {
                rect: mask_region(node, self.viewport, scaled_region(self.viewport))?,
                transform,
            },
        };
        if region.rect.2 <= 0.0 || region.rect.3 <= 0.0 {
            // A region with no area masks everything away, which is a legal
            // way to hide an element.
            return Ok(Some(Mask {
                kind: MaskKind::Alpha,
                content: Vec::new(),
            }));
        }
        let inner = match (
            units_are_bounding_box(node, "maskContentUnits", false),
            units,
        ) {
            (true, Some(placed)) => placed,
            _ => transform,
        };

        let outer = core::mem::take(&mut self.path);
        let built = self.enter_definition(node).and_then(|own| {
            // The region bounds the mask itself, and a `<mask>` may carry a
            // clip or a mask of its own; each narrows what it lets through.
            self.enter(1)?;
            let mut content = Vec::new();
            self.walk_children(node, &own, inner, depth, &mut content)?;
            self.leave(1);
            let bounded = self.clipped_to(region.rect, region.transform, content)?;
            Ok((
                own.mask_kind,
                self.composite(&own, inner, box_of, depth, bounded)?,
            ))
        });
        self.path = outer;

        let (kind, content) = built?;
        Ok(Some(Mask { kind, content }))
    }

    /// An alpha mask that admits one rectangle of the drawing and nothing
    /// else.
    fn rect_mask(
        &mut self,
        rect: (f64, f64, f64, f64),
        transform: Affine,
    ) -> Result<Mask, SvgError> {
        let (x, y, width, height) = rect;
        let corners = SubPath::closed(alloc::vec![
            (x, y),
            (x + width, y),
            (x + width, y + height),
            (x, y + height),
        ]);
        let mut content = Vec::new();
        self.push(
            Layer::filled(
                Paint::Solid(CLIP_INK),
                FillRule::NonZero,
                place(core::slice::from_ref(&corners), transform),
            ),
            &mut content,
        )?;
        Ok(Mask {
            kind: MaskKind::Alpha,
            content,
        })
    }

    /// Add a layer to `out`, charging it against the document's budgets.
    fn push(&mut self, layer: Layer, out: &mut Vec<Node>) -> Result<(), SvgError> {
        let vertices = layer.vertices();
        if vertices == 0 {
            return Ok(());
        }
        if self.layers_left == 0 || vertices > self.vertices_left {
            return Err(SvgError::TooComplex);
        }
        self.vertices_left -= vertices;
        self.layers_left -= 1;
        out.push(Node::Fill(layer));
        Ok(())
    }
}

/// Whether every vertex of `nodes` already lies inside `rect`.
///
/// Only an axis-preserving placement leaves an axis-aligned rectangle to
/// compare against; under a rotation the answer is "not known to be inside",
/// which keeps the clip rather than dropping it.
fn rect_contains(rect: (f64, f64, f64, f64), transform: Affine, nodes: &[Node]) -> bool {
    if transform.b != 0.0 || transform.c != 0.0 {
        return false;
    }
    let (x, y, width, height) = rect;
    let near = transform.apply((x, y));
    let far = transform.apply((x + width, y + height));
    let Some((min, max)) = design_bounds(nodes) else {
        return true;
    };
    f64::from(min.0) >= near.0.min(far.0)
        && f64::from(min.1) >= near.1.min(far.1)
        && f64::from(max.0) <= near.0.max(far.0)
        && f64::from(max.1) <= near.1.max(far.1)
}

/// The design-grid box every vertex of `nodes` falls inside, or `None` when
/// they draw nothing.
///
/// A group's own mask can only narrow what its children cover, so only the
/// children are measured.
fn design_bounds(nodes: &[Node]) -> Option<((i32, i32), (i32, i32))> {
    let mut extent: Option<((i32, i32), (i32, i32))> = None;
    let mut fold = |point: (i32, i32)| {
        extent = Some(match extent {
            None => (point, point),
            Some((min, max)) => (
                (min.0.min(point.0), min.1.min(point.1)),
                (max.0.max(point.0), max.1.max(point.1)),
            ),
        });
    };
    tairix_raster::for_each_fill(nodes, &mut |layer| {
        for point in layer.contours.iter().flatten() {
            fold(*point);
        }
    });
    extent
}

/// Build the root-down chain of elements reaching `target`, or leave `chain`
/// empty when the tree does not hold it.
fn descend<'a>(
    element: &'a Element<'a>,
    target: &Element<'_>,
    chain: &mut Vec<&'a Element<'a>>,
) -> bool {
    chain.push(element);
    if core::ptr::eq(element, target) {
        return true;
    }
    for child in &element.children {
        if descend(child, target, chain) {
            return true;
        }
    }
    chain.pop();
    false
}

/// One group around `children`, or nothing at all when there is nothing to
/// composite.
fn grouped(opacity: u8, mask: Option<Mask>, children: Vec<Node>) -> Vec<Node> {
    if children.is_empty() {
        return children;
    }
    alloc::vec![Node::Group(Group {
        opacity,
        mask,
        children,
    })]
}

/// Whether a `<switch>` may choose this child at all.
///
/// Only a graphics or container element is a candidate; a `<title>` or
/// `<desc>` first child would otherwise swallow the whole switch.
fn is_switchable(name: &str) -> bool {
    is_shape(name) || matches!(name, "g" | "svg" | "a" | "switch" | "use" | "foreignObject")
}

/// Whether this decoder meets a candidate's conditional-processing
/// attributes.
///
/// An empty list is met by definition — it requires nothing. A non-empty one
/// is not: this decoder claims no extension or feature, and it is handed no
/// locale, so a `systemLanguage` is a condition it cannot show is satisfied.
/// Passing over such a child is what reaches the unconditional fallback an
/// author writes for exactly this case.
fn conditions_met(child: &Element<'_>) -> bool {
    ["requiredExtensions", "requiredFeatures", "systemLanguage"]
        .into_iter()
        .all(|name| child.attr(name).is_none_or(|value| value.trim().is_empty()))
}

/// Whether `element`'s `name` units attribute selects `objectBoundingBox`.
fn units_are_bounding_box(element: &Element<'_>, name: &str, default: bool) -> bool {
    match element.attr(name) {
        Some("objectBoundingBox") => true,
        Some(_) => false,
        None => default,
    }
}

/// The map from bounding-box fractions into the element's user space.
///
/// `None` for a box with no area, where every fraction would collapse onto a
/// point — SVG says such an element is not rendered at all.
fn bounding_box_units(box_of: Option<(Point, Point)>) -> Option<Affine> {
    let (min, max) = box_of?;
    let (width, height) = (max.0 - min.0, max.1 - min.1);
    if width <= 0.0 || height <= 0.0 {
        return None;
    }
    Some(Affine::scale(width, height).then(Affine::translate(min.0, min.1)))
}

/// [`MASK_REGION`] as user-space lengths against `viewport`.
fn scaled_region(viewport: (f64, f64)) -> (f64, f64, f64, f64) {
    (
        MASK_REGION.0 * viewport.0,
        MASK_REGION.1 * viewport.1,
        MASK_REGION.2 * viewport.0,
        MASK_REGION.3 * viewport.1,
    )
}

/// A `<mask>`'s region rectangle, in whichever units it is stated.
fn mask_region(
    node: &Element<'_>,
    basis: (f64, f64),
    default: (f64, f64, f64, f64),
) -> Result<(f64, f64, f64, f64), SvgError> {
    let extent = |name: &str, basis: f64, default: f64| match node.attr(name) {
        Some(text) => parse_length(text, basis),
        None => Ok(default),
    };
    Ok((
        extent("x", basis.0, default.0)?,
        extent("y", basis.1, default.1)?,
        extent("width", basis.0, default.2)?,
        extent("height", basis.1, default.3)?,
    ))
}

/// The length a percentage with no axis of its own resolves against.
fn viewport_diagonal(viewport: (f64, f64)) -> f64 {
    sqrt(f64::midpoint(
        viewport.0 * viewport.0,
        viewport.1 * viewport.1,
    ))
}

/// One optional length attribute, defaulting to zero.
fn optional_length(element: &Element<'_>, name: &str, basis: f64) -> Result<f64, SvgError> {
    Ok(optional_attr_length(element, name, basis)?.unwrap_or(0.0))
}

/// One optional length attribute, absent when the element does not state it.
fn optional_attr_length(
    element: &Element<'_>,
    name: &str,
    basis: f64,
) -> Result<Option<f64>, SvgError> {
    match element.attr(name) {
        Some(text) => parse_length(text, basis).map(Some),
        None => Ok(None),
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
