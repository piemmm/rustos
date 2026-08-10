//! SVG's basic shapes, flattened into the one contour currency.
//!
//! A rectangle, circle, ellipse, line, polyline, polygon, and path all end up
//! as [`SubPath`]s in user space, so everything downstream — stroking,
//! transforming, filling — sees a single kind of geometry and no shape needs
//! its own special case again.

use alloc::vec::Vec;

use core::f64::consts::{FRAC_PI_2, PI, TAU};

use crate::error::SvgError;
use crate::geom::{Point, SubPath};
use crate::number::{parse_length, Numbers};
use crate::pathdata::{flatten_ellipse_arc, parse_path_data};
use crate::xml::Node;

/// Whether `name` is one of the elements this module draws.
#[must_use]
pub fn is_shape(name: &str) -> bool {
    matches!(
        name,
        "path" | "rect" | "circle" | "ellipse" | "line" | "polyline" | "polygon"
    )
}

/// Flatten one basic shape into sub-paths in its own user space.
///
/// A shape that encloses nothing — a zero extent, a zero radius — yields an
/// empty list rather than an error, because SVG defines those as simply not
/// rendering. A *negative* extent is an error, because it is not geometry the
/// author can have meant.
///
/// # Errors
/// Returns the parse error of a malformed attribute, or
/// [`SvgError::TooComplex`] once the shape would exceed `max_points`.
pub fn shape_subpaths(
    node: &Node<'_>,
    viewport: (f64, f64),
    tolerance: f64,
    max_points: usize,
) -> Result<Vec<SubPath>, SvgError> {
    match node.name {
        "path" => match node.attr("d") {
            Some(data) => parse_path_data(data, tolerance, max_points),
            None => Ok(Vec::new()),
        },
        "rect" => rect(node, viewport, tolerance),
        "circle" => circle(node, viewport, tolerance),
        "ellipse" => ellipse(node, viewport, tolerance),
        "line" => line(node, viewport),
        "polyline" => points(node, false, max_points),
        "polygon" => points(node, true, max_points),
        _ => Ok(Vec::new()),
    }
}

/// One length attribute, resolved against `basis`, defaulting to zero.
fn length(node: &Node<'_>, name: &str, basis: f64) -> Result<f64, SvgError> {
    match node.attr(name) {
        Some(text) => parse_length(text, basis),
        None => Ok(0.0),
    }
}

/// One radius attribute, which may also be the keyword `auto` meaning "use
/// the other axis".
fn radius(node: &Node<'_>, name: &str, basis: f64) -> Result<Option<f64>, SvgError> {
    match node.attr(name) {
        None | Some("auto") => Ok(None),
        Some(text) => parse_length(text, basis).map(Some),
    }
}

/// The length a radius with no axis of its own resolves a percentage against.
fn diagonal(viewport: (f64, f64)) -> f64 {
    tairix_util::mathf::sqrt(f64::midpoint(
        viewport.0 * viewport.0,
        viewport.1 * viewport.1,
    ))
}

/// `<rect>`, with SVG's rounded-corner rules.
fn rect(node: &Node<'_>, viewport: (f64, f64), tolerance: f64) -> Result<Vec<SubPath>, SvgError> {
    let x = length(node, "x", viewport.0)?;
    let y = length(node, "y", viewport.1)?;
    let w = length(node, "width", viewport.0)?;
    let h = length(node, "height", viewport.1)?;
    if w < 0.0 || h < 0.0 {
        return Err(SvgError::InvalidNumber);
    }
    if w == 0.0 || h == 0.0 {
        return Ok(Vec::new());
    }
    // An absent or `auto` radius takes the other axis's, and neither may
    // exceed half its side — SVG clamps rather than letting the corners meet
    // and invert.
    let (rx, ry) = match (
        radius(node, "rx", viewport.0)?,
        radius(node, "ry", viewport.1)?,
    ) {
        (None, None) => (0.0, 0.0),
        (Some(rx), None) => (rx, rx),
        (None, Some(ry)) => (ry, ry),
        (Some(rx), Some(ry)) => (rx, ry),
    };
    if rx < 0.0 || ry < 0.0 {
        return Err(SvgError::InvalidNumber);
    }
    let rx = rx.min(w / 2.0);
    let ry = ry.min(h / 2.0);

    if rx == 0.0 || ry == 0.0 {
        return Ok(alloc::vec![SubPath::closed(alloc::vec![
            (x, y),
            (x + w, y),
            (x + w, y + h),
            (x, y + h),
        ])]);
    }

    let mut outline = Vec::new();
    outline.push((x + rx, y));
    outline.push((x + w - rx, y));
    let corners = [
        ((x + w - rx, y + ry), -FRAC_PI_2),
        ((x + w - rx, y + h - ry), 0.0),
        ((x + rx, y + h - ry), FRAC_PI_2),
        ((x + rx, y + ry), PI),
    ];
    for (index, (centre, start)) in corners.into_iter().enumerate() {
        flatten_ellipse_arc(
            centre,
            (rx, ry),
            0.0,
            start,
            FRAC_PI_2,
            tolerance,
            &mut outline,
        );
        // The straight side that follows each corner but the last, which the
        // closing of the contour supplies.
        match index {
            0 => outline.push((x + w, y + h - ry)),
            1 => outline.push((x + rx, y + h)),
            2 => outline.push((x, y + ry)),
            _ => {}
        }
    }
    Ok(alloc::vec![SubPath::closed(outline)])
}

/// `<circle>`.
fn circle(node: &Node<'_>, viewport: (f64, f64), tolerance: f64) -> Result<Vec<SubPath>, SvgError> {
    let cx = length(node, "cx", viewport.0)?;
    let cy = length(node, "cy", viewport.1)?;
    let r = length(node, "r", diagonal(viewport))?;
    if r < 0.0 {
        return Err(SvgError::InvalidNumber);
    }
    Ok(full_ellipse((cx, cy), (r, r), tolerance))
}

/// `<ellipse>`, whose radii may each be `auto` (meaning the other's value).
fn ellipse(
    node: &Node<'_>,
    viewport: (f64, f64),
    tolerance: f64,
) -> Result<Vec<SubPath>, SvgError> {
    let cx = length(node, "cx", viewport.0)?;
    let cy = length(node, "cy", viewport.1)?;
    let (rx, ry) = match (
        radius(node, "rx", viewport.0)?,
        radius(node, "ry", viewport.1)?,
    ) {
        (None, None) => (0.0, 0.0),
        (Some(rx), None) => (rx, rx),
        (None, Some(ry)) => (ry, ry),
        (Some(rx), Some(ry)) => (rx, ry),
    };
    if rx < 0.0 || ry < 0.0 {
        return Err(SvgError::InvalidNumber);
    }
    Ok(full_ellipse((cx, cy), (rx, ry), tolerance))
}

/// A whole ellipse as one closed contour, or nothing when it has no area.
fn full_ellipse(centre: Point, radii: Point, tolerance: f64) -> Vec<SubPath> {
    if radii.0 == 0.0 || radii.1 == 0.0 {
        return Vec::new();
    }
    let mut outline = alloc::vec![(centre.0 + radii.0, centre.1)];
    flatten_ellipse_arc(centre, radii, 0.0, 0.0, TAU, tolerance, &mut outline);
    // The sweep closes on its own start point, which the contour's implicit
    // closure already supplies.
    if outline.len() > 1 {
        outline.pop();
    }
    alloc::vec![SubPath::closed(outline)]
}

/// `<line>`, which has no area and so only ever shows as a stroke.
fn line(node: &Node<'_>, viewport: (f64, f64)) -> Result<Vec<SubPath>, SvgError> {
    let x1 = length(node, "x1", viewport.0)?;
    let y1 = length(node, "y1", viewport.1)?;
    let x2 = length(node, "x2", viewport.0)?;
    let y2 = length(node, "y2", viewport.1)?;
    Ok(alloc::vec![SubPath::open(alloc::vec![(x1, y1), (x2, y2)])])
}

/// `<polyline>` and `<polygon>`, which differ only in closure.
fn points(node: &Node<'_>, closed: bool, max_points: usize) -> Result<Vec<SubPath>, SvgError> {
    let Some(text) = node.attr("points") else {
        return Ok(Vec::new());
    };
    let mut numbers = Numbers::new(text);
    let mut list = Vec::new();
    while let Some(x) = numbers.take()? {
        if list.len() == max_points {
            return Err(SvgError::TooComplex);
        }
        // An odd trailing coordinate ends the list in SVG rather than
        // invalidating it, but a malformed one is still refused.
        let Some(y) = numbers.take()? else {
            break;
        };
        list.push((x, y));
    }
    if list.is_empty() {
        return Ok(Vec::new());
    }
    Ok(alloc::vec![SubPath {
        points: list,
        closed,
    }])
}

#[cfg(test)]
#[path = "shape_tests.rs"]
mod tests;
