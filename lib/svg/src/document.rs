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

use tairix_raster::{
    Affine, Color, FillRule, Group, Layer, Mask, MaskKind, Node, Paint, Pattern, TileFold,
};
use tairix_util::mathf::{round_i32, sqrt};

use crate::css::{self, Declaration, Stylesheet};
use crate::error::SvgError;
use crate::font::FontProvider;
use crate::geom::{bounds, LineCap, LineJoin, Point, StrokeStyle, SubPath, Vertex, Vertices};
use crate::marker::{Marker, Position};
use crate::number::{opacity_to_alpha, parse_length, parse_number};
use crate::paint::{PaintServers, PatternTile, Resolved};
use crate::shape::{is_shape, shape_subpaths, takes_markers};
use crate::stroke::stroke_outline;
use crate::style::{scale_alpha, Overflow, PaintOrder, PaintSlot, PaintSpec, Style};
use crate::text::{self, TextBudget, TextCascade};
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
/// subdivided more than a small one and neither is over-tessellated. It is
/// resolved against the placement each shape is actually drawn under
/// ([`flatten_tolerance`]), not against the document's own: a subtree inside
/// `scale(10)` reaches the grid ten times larger, and flattening it to the
/// root's tolerance would facet it by ten times the error.
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

/// How many elements one document may make this walk visit.
///
/// A `<use>`, a `<clipPath>`, a pattern tile, and a marker are each drawn once
/// per *reference*, so a document can make the walk visit far more elements
/// than it holds — and a marker multiplies hardest of all, because one `<path>`
/// element places an instance at every vertex of its `d`. None of the other
/// budgets catches that on its own: content that resolves to no paint charges
/// no layer and no vertex, so an asset whose markers draw nothing could spin
/// the walk for as long as it liked.
///
/// A fixed security bound on decode work, not a capacity: eight times
/// [`xml::MAX_ELEMENTS`], so a document may be walked
/// eight times over through reuse, which is far past any drawing and far short
/// of the seconds an unbounded fan-out measured.
const MAX_ELEMENT_VISITS: usize = 8 * xml::MAX_ELEMENTS;

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
pub fn decode(
    bytes: &[u8],
    viewport: Viewport,
    provider: &mut dyn FontProvider,
) -> Result<SvgImage, SvgError> {
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

    // Gathered before the decoder so the rules may borrow a sheet that had
    // to be joined from several runs.
    let sheets = css::sheet_texts(&root);
    let mut decoder = Decoder {
        root: &root,
        provider,
        text: TextBudget::default(),
        ids: Vec::new(),
        chains: Vec::new(),
        styles: Vec::new(),
        servers: PaintServers::collect(&root),
        sheet: Stylesheet::collect(&sheets)?,
        cascade: Vec::new(),
        path: Vec::new(),
        extents: Vec::new(),
        viewport: view_box.size,
        // A root map with no inverse has flattened the drawing onto a line,
        // where every placement collapses and nothing is drawn at all.
        host: to_design.invert().map(|from_design| Host {
            from_design,
            to_design,
            tolerance: flatten_tolerance(to_design),
        }),
        vertices_left: MAX_TOTAL_VERTICES,
        layers_left: MAX_LAYERS,
        visits_left: MAX_ELEMENT_VISITS,
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
struct Decoder<'a, 'p> {
    root: &'a Element<'a>,
    /// The injected seam a `<text>`'s faces and glyphs come from. Held
    /// rather than passed because every walk arm can reach text, and no arm
    /// but the text one touches it.
    provider: &'p mut dyn FontProvider,
    /// What text is allowed to spend across the whole document.
    text: TextBudget,
    ids: Vec<(&'a str, &'a Element<'a>)>,
    chains: Vec<(&'a Element<'a>, Vec<&'a Element<'a>>)>,
    styles: Vec<(&'a Element<'a>, (f64, f64), Style)>,
    servers: PaintServers<'a>,
    sheet: Stylesheet<'a>,
    cascade: Vec<Declaration<'a>>,
    path: Vec<&'a Element<'a>>,
    extents: Vec<Extent>,
    viewport: (f64, f64),
    host: Option<Host>,
    vertices_left: usize,
    layers_left: usize,
    visits_left: usize,
    nesting: usize,
}

/// The document's own root user space: where a non-scaling stroke is
/// outlined, and how that outline reaches the design grid.
///
/// SVG calculates such a stroke in the *host* space, which the specification
/// equates to the screen's. A decoded asset has no screen, so the root user
/// space stands in: it is where the document's own lengths are written, and
/// its map to the device is a uniform scale under either [`Viewport`], so a
/// round pen stays round. The effect therefore cancels the element's
/// transform chain, not the scale the asset is rasterised at.
#[derive(Copy, Clone)]
struct Host {
    /// The design grid back onto the host space.
    from_design: Affine,
    /// The host space onto the design grid.
    to_design: Affine,
    /// What a round join or cap is flattened to, in host units.
    tolerance: f64,
}

impl Host {
    /// The map from the space `transform` places onto the host space.
    fn to_host(self, transform: Affine) -> Affine {
        transform.then(self.from_design)
    }
}

/// What every instance of one shape's markers shares.
#[derive(Copy, Clone)]
struct Instancing {
    /// The referencing element's stroke width, which the default
    /// `markerUnits` measures the marker in.
    stroke_width: f64,
    /// The shape's own placement onto the design grid.
    transform: Affine,
    /// Where the shape's stroke is outlined, when it is a non-scaling one.
    ///
    /// A marker measured in stroke widths is sized by the width *after* the
    /// transforms affecting it, so it follows the stroke into that space and
    /// stops scaling too. One measured in user units does not.
    host: Option<Host>,
    /// How deep in `<use>` expansions the shape sits.
    depth: usize,
}

impl<'a> TextCascade<'a> for Decoder<'a, '_> {
    fn enter(&mut self, element: &'a Element<'a>, inherited: &Style) -> Result<Style, SvgError> {
        // The text walk visits the same elements the tree walk would, so it
        // is charged the same visit — a `<text>` of a thousand spans cannot
        // cost less than a `<g>` of a thousand children.
        self.visit()?;
        // The selector path is the decoder's, and a `<tspan>` matches a
        // stylesheet rule exactly as any other element does.
        self.path.push(element);
        self.style_of(inherited, element)
    }

    fn leave(&mut self) {
        self.path.pop();
    }
}

impl<'a> Decoder<'a, '_> {
    /// Record every element that carries an `id`, so a reference can find it
    /// wherever in the document it appears.
    fn index(&mut self, element: &'a Element<'a>) {
        if let Some(id) = element.attr("id") {
            self.ids.push((id, element));
        }
        for child in element.children() {
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
    /// A `<clipPath>`, a `<mask>`, a pattern tile, or a marker is reached by
    /// reference, so what it inherits is its own place in the document — not
    /// the style of whatever element happened to point at it. The caller has
    /// already set the path aside and restores it afterwards.
    ///
    /// The style is memoised, because the number of times a definition is
    /// reached is not bounded by the document's size: a marker is resolved
    /// once per vertex it is placed at, and re-walking its ancestor chain
    /// each time would multiply the decode by the depth the author happened
    /// to nest it at.
    ///
    /// The viewport is part of the key, not just the node: a percentage
    /// length anywhere on the chain resolves against whichever viewport the
    /// *referencing* element sits in, so a definition reached from two of
    /// them genuinely has two answers.
    fn enter_definition(&mut self, node: &'a Element<'a>) -> Result<Style, SvgError> {
        let chain = self.chain_of(node);
        if let Some(style) = self
            .styles
            .iter()
            .find(|(key, viewport, _)| core::ptr::eq(*key, node) && *viewport == self.viewport)
            .map(|(_, _, style)| style.clone())
        {
            self.path.extend_from_slice(&chain);
            return Ok(style);
        }
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
        self.styles.push((node, self.viewport, style.clone()));
        Ok(style)
    }

    /// The `color` a referenced definition's own ancestry gives it.
    fn definition_color(&mut self, node: &'a Element<'a>) -> Result<Color, SvgError> {
        let outer = core::mem::take(&mut self.path);
        let resolved = self.enter_definition(node);
        self.path = outer;
        Ok(resolved?.color)
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

    /// Take one element visit from the document's budget.
    ///
    /// Charged wherever the walk reaches an element, however it got there, so
    /// that a document which draws one subtree many times is bounded by the
    /// work it asks for rather than by the elements it holds.
    fn visit(&mut self) -> Result<(), SvgError> {
        if self.visits_left == 0 {
            return Err(SvgError::TooComplex);
        }
        self.visits_left -= 1;
        Ok(())
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
        self.visit()?;
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
            self.draw(element, style, transform, depth, opacity, out)?;
            return Ok(true);
        }
        match element.name {
            "use" => self.expand_use(element, style, transform, depth, out)?,
            "switch" => self.walk_switch(element, style, transform, depth, out)?,
            "svg" => self.walk_viewport(element, style, transform, depth, None, out)?,
            "g" | "a" => self.walk_children(element, style, transform, depth, out)?,
            "text" => return self.draw_text(element, style, transform, depth, opacity, out),
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
        for child in element.children() {
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
        for child in element.children() {
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
        depth: usize,
        opacity: u8,
        out: &mut Vec<Node>,
    ) -> Result<(), SvgError> {
        let tolerance = flatten_tolerance(transform);
        // Only a shape that carries markers pays for the vertex list, and it
        // cannot outrun the visits its instances would each cost anyway.
        let marked = style.visible && takes_markers(element.name) && self.names_marker(style);
        let mut vertices = marked.then(|| Vertices::new(self.visits_left));
        let subpaths = shape_subpaths(
            element,
            self.viewport,
            tolerance,
            self.vertices_left,
            vertices.as_mut(),
        )?;
        if subpaths.is_empty() {
            return Ok(());
        }
        self.measure(&subpaths, transform);
        if !style.visible {
            return Ok(());
        }
        let box_of = bounds(&subpaths);
        let stroked = style.stroke_style.width > 0.0 && !matches!(style.stroke, PaintSpec::None);
        let host = style.non_scaling_stroke.then_some(self.host).flatten();

        // Two layers of one element overlap, so folding the element's opacity
        // into each would show the fill through its own stroke. Those are
        // composited as a unit instead, at their own opacities. One layer
        // needs no buffer: painting it at the product is the same pixels.
        // Markers always take the buffer, because a marker is a subtree that
        // may overlap both the shape and the next instance of itself.
        //
        // Decided from the style rather than from the resolved paints, so
        // each is resolved exactly once — resolving twice to learn whether to
        // isolate would walk a pattern's whole tile twice against one vertex
        // budget. A paint that turns out to resolve to nothing then costs an
        // isolation buffer it did not need, which is the same picture.
        let isolate = opacity != u8::MAX
            && (marked
                || (paints(&style.fill, style.fill_opacity)
                    && stroked
                    && style.stroke_opacity > 0.0));
        if isolate {
            self.fits_group()?;
        }
        let group = if isolate {
            1.0
        } else {
            f64::from(opacity) / 255.0
        };

        let painted = self.paint_of(
            &style.fill,
            style,
            style.fill_opacity * group,
            box_of,
            transform,
            depth,
        )?;
        let outlined = if stroked {
            self.paint_of(
                &style.stroke,
                style,
                style.stroke_opacity * group,
                box_of,
                transform,
                depth,
            )?
        } else {
            None
        };

        let fill =
            painted.map(|paint| Layer::filled(paint, style.fill_rule, place(&subpaths, transform)));
        let stroke = match outlined {
            Some(paint) => {
                // A non-scaling stroke inverts the usual order: the geometry
                // is carried into the host space and the pen applied there,
                // so the transform is spent on the path and not on the width.
                // Every length the stroker reads follows it across; only the
                // tolerance is restated, being resolved against the placement
                // rather than authored.
                let moved = host.map(|host| {
                    let to_host = host.to_host(transform);
                    let carried: Vec<SubPath> =
                        subpaths.iter().map(|sub| sub.mapped(to_host)).collect();
                    (carried, host)
                });
                let (geometry, tolerance, onto) = match &moved {
                    Some((geometry, host)) => (geometry.as_slice(), host.tolerance, host.to_design),
                    None => (subpaths.as_slice(), tolerance, transform),
                };
                let outline =
                    stroke_outline(geometry, &style.stroke_style, tolerance, self.vertices_left)?;
                // A stroke outline is a union of overlapping pieces, so only
                // the non-zero rule merges them; even-odd would punch holes
                // where two pieces meet.
                Some(Layer::filled(
                    paint,
                    FillRule::NonZero,
                    place(&outline, onto),
                ))
            }
            None => None,
        };

        let markers = match vertices {
            Some(vertices) => self.markers(
                style,
                &vertices.finish(),
                Instancing {
                    stroke_width: style.stroke_style.width,
                    transform,
                    host,
                    depth,
                },
            )?,
            None => Vec::new(),
        };

        let layers = self.ordered(style.paint_order, fill, stroke, markers)?;
        let mut drawn = if isolate {
            grouped(opacity, None, layers)
        } else {
            layers
        };
        out.append(&mut drawn);
        Ok(())
    }

    /// Draw one `<text>` element: lay its characters out, flatten every
    /// glyph, and fill each run's contours together.
    ///
    /// Answers whether the element's own opacity is already accounted for,
    /// exactly as a shape does.
    fn draw_text(
        &mut self,
        element: &'a Element<'a>,
        style: &Style,
        transform: Affine,
        depth: usize,
        opacity: u8,
        out: &mut Vec<Node>,
    ) -> Result<bool, SvgError> {
        let viewport = self.viewport;
        let collected = text::collect(element, style, viewport, self)?;
        let Decoder {
            provider,
            text: budget,
            ..
        } = self;
        let Some(laid) = text::lay_out(collected, *provider, budget)? else {
            return Ok(true);
        };
        let runs = glyph_runs(&laid);
        // Two layers of one element overlap, so folding the element's
        // opacity into each would show the fill through its own stroke, and
        // two runs of one `<text>` can overlap wherever the author moved
        // them. Either case is composited as a unit instead.
        let isolate = opacity != u8::MAX
            && (runs.len() > 1
                || runs.first().is_some_and(|&(from, _)| {
                    laid.glyphs
                        .get(from)
                        .and_then(|glyph| laid.styles.get(glyph.run))
                        .is_some_and(paints_both)
                }));
        if isolate {
            self.fits_group()?;
        }
        let group = if isolate {
            1.0
        } else {
            f64::from(opacity) / 255.0
        };
        let mut drawn = Vec::new();
        for span in runs {
            self.draw_text_run(&laid, span, (transform, depth, group), &mut drawn)?;
        }
        let mut wrapped = if isolate {
            grouped(opacity, None, drawn)
        } else {
            drawn
        };
        out.append(&mut wrapped);
        Ok(true)
    }

    /// Fill (and stroke) one run of glyphs, all its contours together.
    ///
    /// The contours are built in **user space**, exactly as a shape's are,
    /// so the stroke's width and dashes are read in the space the author
    /// wrote them in and the placement onto the grid happens once at the
    /// end. Only the flattening tolerance is resolved against the full
    /// font-units-to-grid map, which is what makes a glyph subdivide like a
    /// `<path>` of the same shape at the same size.
    fn draw_text_run(
        &mut self,
        laid: &text::Laid,
        span: (usize, usize),
        placement: (Affine, usize, f64),
        out: &mut Vec<Node>,
    ) -> Result<(), SvgError> {
        let (from, to) = span;
        let (transform, depth, group) = placement;
        let run = laid.glyphs.get(from).ok_or(SvgError::Malformed)?.run;
        let style = laid.styles.get(run).ok_or(SvgError::Malformed)?;
        self.text.run()?;
        let mut subpaths: Vec<SubPath> = Vec::new();
        for index in from..to {
            let (Some(glyph), Some(outline)) = (laid.glyphs.get(index), laid.outlines.get(index))
            else {
                continue;
            };
            let tolerance = flatten_tolerance(glyph.to_user.then(transform));
            let mut drawn = text::glyph_subpaths(outline, glyph.to_user, tolerance);
            // A synthetic bold the face could not furnish is this crate's own
            // stroker run over the glyph's contours and unioned into the same
            // non-zero fill: there is no second thickening implementation.
            // The stroke is a fraction of the em, and the em in user units is
            // the font size, whichever face the glyph resolved to.
            let bold = glyph.synthetic_bold * style.font_size;
            if bold > 0.0 && !drawn.is_empty() {
                let pen = StrokeStyle {
                    width: bold,
                    cap: LineCap::Round,
                    join: LineJoin::Round,
                    ..StrokeStyle::default()
                };
                let mut thickened = stroke_outline(
                    &drawn,
                    &pen,
                    flatten_tolerance(transform),
                    self.vertices_left,
                )?;
                drawn.append(&mut thickened);
            }
            subpaths.append(&mut drawn);
        }
        if subpaths.is_empty() {
            return Ok(());
        }
        self.measure(&subpaths, transform);
        if !style.visible {
            return Ok(());
        }
        let box_of = bounds(&subpaths);
        let painted = self.paint_of(
            &style.fill,
            style,
            style.fill_opacity * group,
            box_of,
            transform,
            depth,
        )?;
        let stroked = style.stroke_style.width > 0.0 && !matches!(style.stroke, PaintSpec::None);
        let outlined = if stroked {
            self.paint_of(
                &style.stroke,
                style,
                style.stroke_opacity * group,
                box_of,
                transform,
                depth,
            )?
        } else {
            None
        };
        let fill = painted
            .map(|paint| Layer::filled(paint, FillRule::NonZero, place(&subpaths, transform)));
        let stroke = match outlined {
            Some(paint) => {
                let outline = stroke_outline(
                    &subpaths,
                    &style.stroke_style,
                    flatten_tolerance(transform),
                    self.vertices_left,
                )?;
                Some(Layer::filled(
                    paint,
                    FillRule::NonZero,
                    place(&outline, transform),
                ))
            }
            None => None,
        };
        let mut layers = self.ordered(style.paint_order, fill, stroke, Vec::new())?;
        out.append(&mut layers);
        Ok(())
    }

    /// Assemble what one shape draws, in the order it paints them.
    fn ordered(
        &mut self,
        order: PaintOrder,
        mut fill: Option<Layer>,
        mut stroke: Option<Layer>,
        mut markers: Vec<Node>,
    ) -> Result<Vec<Node>, SvgError> {
        let mut out = Vec::new();
        for slot in order.slots() {
            match slot {
                PaintSlot::Fill => {
                    if let Some(layer) = fill.take() {
                        self.push(layer, &mut out)?;
                    }
                }
                PaintSlot::Stroke => {
                    if let Some(layer) = stroke.take() {
                        self.push(layer, &mut out)?;
                    }
                }
                PaintSlot::Markers => out.append(&mut markers),
            }
        }
        Ok(out)
    }

    /// Whether `style` names a marker this document actually defines.
    fn names_marker(&self, style: &Style) -> bool {
        [&style.marker_start, &style.marker_mid, &style.marker_end]
            .into_iter()
            .flatten()
            .any(|id| self.marker(id).is_some())
    }

    /// The `<marker>` element with fragment id `id`.
    fn marker(&self, id: &str) -> Option<&'a Element<'a>> {
        self.find(id).filter(|node| node.name == "marker")
    }

    /// Draw this shape's markers, bottom first.
    ///
    /// SVG paints the start marker, then the mid markers in the order they
    /// occur along the path, then the end marker — which is the order the
    /// vertex walk meets them in, so one pass produces it. A shape of a single
    /// vertex is both the first and the last, and carries both of those
    /// markers on the one point.
    ///
    /// Each `<marker>` is read once for the whole shape; only its placement is
    /// per vertex.
    fn markers(
        &mut self,
        style: &Style,
        vertices: &[Vertex],
        shared: Instancing,
    ) -> Result<Vec<Node>, SvgError> {
        let ends = [
            (
                Position::Start,
                self.resolve_marker(style.marker_start.as_deref())?,
            ),
            (
                Position::Mid,
                self.resolve_marker(style.marker_mid.as_deref())?,
            ),
            (
                Position::End,
                self.resolve_marker(style.marker_end.as_deref())?,
            ),
        ];
        let last = vertices.len().saturating_sub(1);
        let mut out = Vec::new();
        for (index, vertex) in vertices.iter().enumerate() {
            for (position, resolved) in &ends {
                let here = match position {
                    Position::Start => index == 0,
                    Position::Mid => index != 0 && index != last,
                    Position::End => index == last,
                };
                let Some((node, marker)) = resolved.filter(|_| here) else {
                    continue;
                };
                self.instance(node, &marker, vertex, *position, shared, &mut out)?;
            }
        }
        Ok(out)
    }

    /// The `<marker>` a property names, read once for the whole shape.
    ///
    /// A reference to a marker the document does not define draws nothing and
    /// leaves the shape alone: unlike a missing clip, a missing decoration
    /// cannot show more than the author asked for, so refusing the element
    /// would lose a picture that is merely undecorated.
    fn resolve_marker(
        &mut self,
        id: Option<&str>,
    ) -> Result<Option<(&'a Element<'a>, Marker)>, SvgError> {
        let Some(node) = id.and_then(|id| self.marker(id)) else {
            return Ok(None);
        };
        Ok(Marker::read(node, self.viewport)?.map(|marker| (node, marker)))
    }

    /// Draw one instance of `node` at `vertex`.
    ///
    /// The content takes its style from the marker's own place in the
    /// document, exactly as a clip's or a pattern tile's does — SVG 1.1 has no
    /// way for a marker to be tinted by the shape that placed it. Its geometry
    /// is no part of that shape's bounding box either, and its percentages
    /// resolve against the viewport the marker establishes.
    fn instance(
        &mut self,
        node: &'a Element<'a>,
        marker: &Marker,
        vertex: &Vertex,
        position: Position,
        shared: Instancing,
        out: &mut Vec<Node>,
    ) -> Result<(), SvgError> {
        // One instance is one visit of the `<marker>` element, which is what
        // bounds an empty marker placed at every vertex of a long path — and
        // what makes a marker whose content places the same marker terminate.
        self.visit()?;
        // Placed at the vertex as the host space sees it, and turned by the
        // direction the path runs there.
        let (base, at) = match shared.host.filter(|_| marker.scales_with_stroke()) {
            Some(host) => (
                host.to_design,
                vertex.mapped(host.to_host(shared.transform)),
            ),
            None => (shared.transform, *vertex),
        };
        let Some(placement) = marker.place(&at, position, shared.stroke_width) else {
            return Ok(());
        };
        let viewport = placement.viewport.then(base);
        let content = placement.content.then(base);

        let outer_path = core::mem::take(&mut self.path);
        let built = self.enter_definition(node).and_then(|own| {
            // A marker viewport clips like any other unless the author says
            // otherwise; `display: none` on the element does not apply, since
            // that is how the marker avoids being drawn where it is defined.
            let levels = usize::from(own.overflow == Overflow::Hidden);
            self.enter(levels)?;
            let measuring = core::mem::take(&mut self.extents);
            let outer_viewport = core::mem::replace(&mut self.viewport, marker.content_viewport);
            let mut drawn = Vec::new();
            let walked = self.walk_children(node, &own, content, shared.depth, &mut drawn);
            self.viewport = outer_viewport;
            self.extents = measuring;
            walked?;
            self.leave(levels);
            Ok((own.overflow, drawn))
        });
        self.path = outer_path;

        let (overflow, drawn) = built?;
        let mut placed = if overflow == Overflow::Hidden {
            self.clipped_to(
                (0.0, 0.0, marker.viewport.0, marker.viewport.1),
                viewport,
                drawn,
            )?
        } else {
            drawn
        };
        out.append(&mut placed);
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
        depth: usize,
    ) -> Result<Option<Paint>, SvgError> {
        let alpha = alpha.clamp(0.0, 1.0);
        if let PaintSpec::Reference(id, fallback) = spec {
            match self.server_paint(id, alpha, box_of, transform, depth)? {
                Resolved::Paint(paint) => return Ok(Some(paint)),
                Resolved::Nothing => return Ok(None),
                // A reference that names nothing falls back to the colour
                // written beside it, and to nothing at all when there is
                // none.
                Resolved::Unresolved => {
                    return Ok(fallback
                        .and_then(|color| scale_alpha(color, alpha))
                        .map(Paint::Solid))
                }
            }
        }
        let color = match spec {
            PaintSpec::Color(color) => *color,
            PaintSpec::Current => style.color,
            PaintSpec::None | PaintSpec::Reference(_, _) => return Ok(None),
        };
        Ok(scale_alpha(color, alpha).map(Paint::Solid))
    }

    /// What the paint server named `id` comes to for this shape.
    ///
    /// A shape with no bounding box has no bounding-box units to resolve a
    /// server in, so the reference is treated as unresolved and its fallback
    /// applies.
    fn server_paint(
        &mut self,
        id: &str,
        alpha: f64,
        box_of: Option<(Point, Point)>,
        transform: Affine,
        depth: usize,
    ) -> Result<Resolved, SvgError> {
        let (Some(extent), Some(node)) = (box_of, self.servers.node(id)) else {
            return Ok(Resolved::Unresolved);
        };
        if node.name == "pattern" {
            return self.pattern_paint(node, extent, transform, alpha, depth);
        }
        // A `currentColor` stop stands for the `color` the gradient's own
        // ancestry gives it, exactly as a clip or a mask takes its style from
        // where it sits rather than from its user.
        let current = self.definition_color(node)?;
        self.servers
            .gradient(node, extent, transform, self.viewport, alpha, current)
    }

    /// The paint a `<pattern>` reference resolves to for one shape.
    ///
    /// The tile's content becomes artwork of its own, on a design grid of
    /// which the whole is one tile, so the renderer draws a repeat exactly as
    /// it draws any other drawing. A tile is a buffer in flight just as a
    /// group is, so it costs a level of the same bound — which is what keeps
    /// what this decoder admits to what the renderer will draw.
    fn pattern_paint(
        &mut self,
        node: &'a Element<'a>,
        box_of: (Point, Point),
        transform: Affine,
        alpha: f64,
        depth: usize,
    ) -> Result<Resolved, SvgError> {
        let grid = f64::from(DESIGN_GRID);
        let Some(tile) = self
            .servers
            .pattern(node, box_of, transform, self.viewport, grid)?
        else {
            return Ok(Resolved::Nothing);
        };
        self.enter(1)?;
        let outer = core::mem::take(&mut self.path);
        let built = self.tile_paint(node, &tile, alpha, depth);
        self.path = outer;
        self.leave(1);
        built
    }

    /// Walk a pattern's tile content and wrap it as the paint a shape fills
    /// with.
    ///
    /// The caller has set the selector path aside and charged the tile's
    /// nesting level. Like every referenced definition the content inherits
    /// from its own place in the document; only the geometry comes from the
    /// shape being filled.
    fn tile_paint(
        &mut self,
        node: &'a Element<'a>,
        tile: &PatternTile<'a>,
        alpha: f64,
        depth: usize,
    ) -> Result<Resolved, SvgError> {
        let own = self.enter_definition(node)?;
        let overflow = own.overflow;
        let inherited = if core::ptr::eq(tile.content, node) {
            own
        } else {
            // `href` inherits content as well as attributes, and content
            // taken from another pattern inherits where *that* one sits.
            self.path.clear();
            self.enter_definition(tile.content)?
        };
        // A tile is a drawing in its own space: its geometry is no part of
        // the bounding box of whatever element is being filled, and its
        // percentages resolve against the viewport the tile establishes
        // rather than the document's.
        let measuring = core::mem::take(&mut self.extents);
        let outer_viewport = core::mem::replace(&mut self.viewport, tile.content_viewport);
        let mut content = Vec::new();
        let walked = self.walk_children(
            tile.content,
            &inherited,
            tile.content_to_design,
            depth,
            &mut content,
        );
        self.viewport = outer_viewport;
        self.extents = measuring;
        walked?;
        if content.is_empty() {
            return Ok(Resolved::Nothing);
        }
        let opacity = opacity_to_alpha(alpha);
        if opacity == 0 {
            return Ok(Resolved::Nothing);
        }
        // The tile buffer confines the content to the tile, which is the
        // `overflow: hidden` a pattern is drawn under. Content an author let
        // spill into the neighbouring repeats is drawn by folding those
        // neighbours back into the one period — exact, because a pattern is
        // periodic — and a spill past the fold's bound is a budget overrun
        // like any other.
        let fold = match (overflow, design_bounds(&content)) {
            (Overflow::Visible, Some((min, max))) => {
                TileFold::reaching(min, max, DESIGN_GRID).ok_or(SvgError::TooComplex)?
            }
            _ => TileFold::default(),
        };
        Ok(Resolved::Paint(Paint::Pattern(Pattern {
            content,
            to_tile: tile.to_tile,
            fold,
            opacity,
        })))
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
        for child in node.children() {
            self.visit()?;
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
            let subpaths = shape_subpaths(
                shape,
                self.viewport,
                flatten_tolerance(placed),
                self.vertices_left,
                None,
            )?;
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
    for child in element.children() {
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

/// Whether a paint spec can contribute a layer at all, before anything it
/// references is resolved.
///
/// Deliberately generous: a reference is counted even though it may resolve
/// to nothing, because the one caller wants an answer that never *under*
/// states what an element draws.
fn paints(spec: &PaintSpec, opacity: f64) -> bool {
    !matches!(spec, PaintSpec::None) && opacity > 0.0
}

/// [`FLATTEN_TOLERANCE`] expressed in the local units `transform` places on
/// the design grid.
///
/// A placement that collapses draws a point however finely a curve on it is
/// subdivided, so it takes the coarsest tolerance there is rather than an
/// infinite one, which the flattener would read as the finest.
fn flatten_tolerance(transform: Affine) -> f64 {
    let local = FLATTEN_TOLERANCE / transform.max_scale();
    if local.is_finite() {
        local
    } else {
        f64::MAX
    }
}

/// Whether a style paints both a fill and a stroke, so its two layers
/// overlap and a group opacity cannot be folded into either.
fn paints_both(style: &Style) -> bool {
    paints(&style.fill, style.fill_opacity)
        && style.stroke_style.width > 0.0
        && !matches!(style.stroke, PaintSpec::None)
        && style.stroke_opacity > 0.0
}

/// The maximal stretches of consecutive glyphs that share a style run, in
/// paint order.
fn glyph_runs(laid: &text::Laid) -> Vec<(usize, usize)> {
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for (index, glyph) in laid.glyphs.iter().enumerate() {
        match runs.last_mut() {
            Some(last) if laid.glyphs[last.0].run == glyph.run && last.1 == index => last.1 += 1,
            _ => runs.push((index, index + 1)),
        }
    }
    runs
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
