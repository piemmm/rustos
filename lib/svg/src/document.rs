//! The decoded SVG document and the top-level [`decode`] entry point.
//!
//! [`SvgImage`] is the shared, fast-draw vector form a decoded asset becomes:
//! a square design grid plus an ordered stack of filled polygon [`SvgLayer`]s
//! (bottom layer first), exactly the shape `lib/cursor`'s `VectorCursor` and
//! `lib/icon`'s `VectorIcon` consume. SVG is *converted once* into this form
//! and never re-parsed on the hot compositing path.

use alloc::vec::Vec;

use tairix_raster::Color;

use crate::color::parse_fill;
use crate::error::SvgError;
use crate::path::{parse_path, parse_points, rect_polygon};
use crate::xml::{self, Element};

/// The largest design-grid side a document may declare.
///
/// The desktop authors assets on grids of tens of units; a far larger grid is
/// almost certainly a hostile or corrupt document and is refused rather than
/// trusted.
const MAX_DESIGN: i32 = 4096;

/// The largest number of filled layers a single document may contribute.
const MAX_LAYERS: usize = 1024;

/// The largest number of vertices summed across every layer of a document.
const MAX_TOTAL_VERTICES: usize = 65_536;

/// One filled layer of a decoded SVG: a fill colour and a polygon ring in
/// design-grid coordinates.
///
/// This mirrors `lib/cursor`'s `Shape` and `lib/icon`'s `IconLayer`; those
/// crates map an [`SvgImage`] straight onto their own vector forms, so there
/// is one rasterisation path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SvgLayer {
    /// The straight-alpha fill colour of this layer.
    pub fill: Color,
    /// The polygon outline, as `(x, y)` design-grid coordinate pairs.
    pub polygon: Vec<(i32, i32)>,
}

/// A decoded SVG asset: a square design grid and an ordered stack of filled
/// [`SvgLayer`]s, plus an optional pointer hotspot for cursor assets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SvgImage {
    design: u32,
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
}

/// Decode an SVG byte string into an [`SvgImage`].
///
/// The decoder is total: it returns `Ok` for a document in the supported
/// subset and a precise [`SvgError`] for everything else, and it never panics
/// for any input. It is the single image-decoding
/// entry point the desktop's SVG-first asset pipeline runs untrusted on-disk
/// assets through.
///
/// # Errors
/// See [`SvgError`] for the closed set of rejection reasons.
pub fn decode(bytes: &[u8]) -> Result<SvgImage, SvgError> {
    let text = core::str::from_utf8(bytes).map_err(|_| SvgError::NotUtf8)?;
    let elements = xml::scan(text)?;

    let root = elements
        .iter()
        .find(|el| el.name == "svg")
        .ok_or(SvgError::MissingRoot)?;
    let design = parse_design(root)?;
    let hotspot = parse_hotspot(root)?;

    let mut layers = Vec::new();
    let mut total_vertices = 0usize;
    for el in &elements {
        let Some(polygon) = shape_polygon(el)? else {
            continue;
        };
        if polygon.len() < 3 {
            continue;
        }
        let Some(fill) = shape_fill(el)? else {
            continue;
        };
        total_vertices = total_vertices.saturating_add(polygon.len());
        if layers.len() >= MAX_LAYERS || total_vertices > MAX_TOTAL_VERTICES {
            return Err(SvgError::TooComplex);
        }
        layers.push(SvgLayer { fill, polygon });
    }

    Ok(SvgImage {
        design,
        layers,
        hotspot,
    })
}

/// Resolve the square design-grid side from the `<svg>` element.
///
/// Prefers `viewBox="0 0 D D"`; falls back to equal `width`/`height`.
fn parse_design(svg: &Element<'_>) -> Result<u32, SvgError> {
    if let Some(view_box) = svg.attr("viewBox") {
        return design_from_view_box(view_box);
    }
    match (svg.attr("width"), svg.attr("height")) {
        (Some(w), Some(h)) => {
            let width = parse_length(w)?;
            let height = parse_length(h)?;
            square_design(width, height)
        }
        _ => Err(SvgError::MissingViewBox),
    }
}

/// Parse `viewBox="minx miny width height"`, requiring a zero origin and a
/// square, positive extent.
fn design_from_view_box(view_box: &str) -> Result<u32, SvgError> {
    let values: Vec<i32> = view_box
        .split(|c: char| c == ',' || c.is_ascii_whitespace())
        .filter(|t| !t.is_empty())
        .map(crate::path::parse_int)
        .collect::<Result<_, _>>()
        .map_err(|_| SvgError::InvalidViewBox)?;
    let [min_x, min_y, width, height] = values.as_slice() else {
        return Err(SvgError::InvalidViewBox);
    };
    if *min_x != 0 || *min_y != 0 {
        return Err(SvgError::InvalidViewBox);
    }
    square_design(*width, *height)
}

/// Validate a `width`/`height` pair as a square, positive, in-range design
/// grid.
fn square_design(width: i32, height: i32) -> Result<u32, SvgError> {
    if width != height {
        return Err(SvgError::NonSquareViewBox);
    }
    if width <= 0 || width > MAX_DESIGN {
        return Err(SvgError::InvalidViewBox);
    }
    u32::try_from(width).map_err(|_| SvgError::InvalidViewBox)
}

/// Parse a length that may carry a trailing `px` unit.
fn parse_length(value: &str) -> Result<i32, SvgError> {
    let trimmed = value.trim();
    let number = trimmed.strip_suffix("px").unwrap_or(trimmed);
    crate::path::parse_int(number).map_err(|_| SvgError::InvalidViewBox)
}

/// Read an optional pointer hotspot from the `<svg>` element.
///
/// Both coordinates must be present together; one without the other is a
/// malformed asset rather than a silent default.
fn parse_hotspot(svg: &Element<'_>) -> Result<Option<(i32, i32)>, SvgError> {
    match (svg.attr("data-hotspot-x"), svg.attr("data-hotspot-y")) {
        (Some(x), Some(y)) => {
            let hx = crate::path::parse_int(x)?;
            let hy = crate::path::parse_int(y)?;
            Ok(Some((hx, hy)))
        }
        (None, None) => Ok(None),
        _ => Err(SvgError::InvalidNumber),
    }
}

/// Extract the polygon ring for a shape element, or `None` if the element is
/// not a supported shape.
fn shape_polygon(el: &Element<'_>) -> Result<Option<Vec<(i32, i32)>>, SvgError> {
    match el.name {
        "polygon" | "polyline" => {
            let points = el.attr("points").ok_or(SvgError::Malformed)?;
            Ok(Some(parse_points(points)?))
        }
        "path" => {
            let d = el.attr("d").ok_or(SvgError::Malformed)?;
            Ok(Some(parse_path(d)?))
        }
        "rect" => {
            let width = el.attr("width").ok_or(SvgError::Malformed)?;
            let height = el.attr("height").ok_or(SvgError::Malformed)?;
            Ok(Some(rect_polygon(
                el.attr("x"),
                el.attr("y"),
                width,
                height,
            )?))
        }
        _ => Ok(None),
    }
}

/// Resolve a shape's fill, defaulting to opaque black when unspecified (the
/// SVG default), and returning `None` for an explicit `none`/transparent fill.
fn shape_fill(el: &Element<'_>) -> Result<Option<Color>, SvgError> {
    let fill = el.attr("fill").unwrap_or("black");
    parse_fill(fill, el.attr("fill-opacity"))
}
