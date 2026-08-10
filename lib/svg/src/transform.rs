//! Coordinate systems: the `transform` attribute, and fitting a `viewBox`
//! into the box that shows it.
//!
//! Both are the same arithmetic — an affine map — so both live here and
//! produce the one [`Affine`] the rest of the decoder composes. A shape's
//! points are transformed once, by the accumulated map of every ancestor,
//! just before they are placed on the design grid.

use tairix_raster::Affine;

use crate::error::SvgError;
use crate::number::{parse_number, Numbers};

/// Parse a `transform` attribute: a list of transform functions applied
/// right-to-left, as SVG defines them.
///
/// `transform="translate(10 0) rotate(45)"` rotates first and then
/// translates, so the returned map is the product in that order.
///
/// # Errors
/// Returns [`SvgError::InvalidNumber`] for an unknown function, a wrong
/// argument count, or a malformed number: a transform that cannot be read is
/// refused rather than silently dropped, which would draw the shape in the
/// wrong place.
pub fn parse_transform(value: &str) -> Result<Affine, SvgError> {
    let mut total = Affine::IDENTITY;
    let mut rest = value.trim();
    while !rest.is_empty() {
        let Some(open) = rest.find('(') else {
            return Err(SvgError::InvalidNumber);
        };
        let Some(close) = rest.find(')') else {
            return Err(SvgError::InvalidNumber);
        };
        if close < open {
            return Err(SvgError::InvalidNumber);
        }
        let name = rest[..open].trim().trim_start_matches(',').trim();
        let step = parse_function(name, &rest[open + 1..close])?;
        // The list reads left to right but applies right to left, so each
        // function is applied *before* everything already accumulated.
        total = step.then(total);
        rest = rest[close + 1..].trim_start_matches([',', ' ', '\t', '\r', '\n']);
    }
    Ok(total)
}

/// Build one transform function from its name and argument list.
fn parse_function(name: &str, args: &str) -> Result<Affine, SvgError> {
    let mut numbers = Numbers::new(args);
    let mut values = [0.0_f64; 6];
    let mut count = 0;
    while let Some(value) = numbers.take()? {
        if count == values.len() {
            return Err(SvgError::InvalidNumber);
        }
        values[count] = value;
        count += 1;
    }
    match (name, count) {
        ("matrix", 6) => Ok(Affine {
            a: values[0],
            b: values[1],
            c: values[2],
            d: values[3],
            e: values[4],
            f: values[5],
        }),
        ("translate", 1) => Ok(Affine::translate(values[0], 0.0)),
        ("translate", 2) => Ok(Affine::translate(values[0], values[1])),
        ("scale", 1) => Ok(Affine::scale(values[0], values[0])),
        ("scale", 2) => Ok(Affine::scale(values[0], values[1])),
        ("rotate", 1) => Ok(Affine::rotate_degrees(values[0])),
        ("rotate", 3) => Ok(Affine::rotate_degrees_about(
            values[0], values[1], values[2],
        )),
        ("skewX", 1) => Ok(Affine::skew_x_degrees(values[0])),
        ("skewY", 1) => Ok(Affine::skew_y_degrees(values[0])),
        _ => Err(SvgError::InvalidNumber),
    }
}

/// The user-space rectangle an element's `viewBox` maps from.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ViewBox {
    /// The rectangle's top-left corner.
    pub min: (f64, f64),
    /// The rectangle's width and height, both strictly positive.
    pub size: (f64, f64),
}

/// Parse a `viewBox` attribute.
///
/// # Errors
/// Returns [`SvgError::InvalidViewBox`] for anything but four numbers with a
/// positive width and height; a zero or negative extent disables rendering in
/// SVG, which for an asset the desktop must draw is a refusal.
pub fn parse_view_box(value: &str) -> Result<ViewBox, SvgError> {
    let mut numbers = Numbers::new(value);
    let mut values = [0.0_f64; 4];
    for slot in &mut values {
        *slot = numbers.required().map_err(|_| SvgError::InvalidViewBox)?;
    }
    if !numbers.is_exhausted() || values[2] <= 0.0 || values[3] <= 0.0 {
        return Err(SvgError::InvalidViewBox);
    }
    Ok(ViewBox {
        min: (values[0], values[1]),
        size: (values[2], values[3]),
    })
}

/// Where a `viewBox` sits inside its viewport when the two have different
/// shapes.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum Align {
    /// Stretch to fill, ignoring the aspect ratio (`preserveAspectRatio="none"`).
    None,
    /// Keep the aspect ratio, anchoring at the given fraction of the spare
    /// space on each axis: `0.0` is min, `0.5` mid, `1.0` max.
    #[default]
    Mid,
    /// Anchor at the minimum edge of both axes.
    Min,
    /// Anchor at the maximum edge of both axes.
    Max,
    /// Anchor at the minimum of x and the maximum of y, and the three other
    /// mixed spellings SVG allows.
    Mixed(MixedAlign),
}

/// One of the mixed `preserveAspectRatio` anchorings.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct MixedAlign {
    /// The fraction of the spare horizontal space left of the content, in
    /// tenths so the type stays comparable: 0, 5, or 10.
    pub x: u8,
    /// The same for the vertical axis.
    pub y: u8,
}

/// How a `viewBox` is fitted to its viewport.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct AspectRatio {
    /// Where the content sits in the spare space.
    pub align: Align,
    /// Whether the content covers the viewport and overflows (`slice`) rather
    /// than fitting inside it (`meet`, the default).
    pub slice: bool,
}

/// Parse a `preserveAspectRatio` attribute.
///
/// # Errors
/// Returns [`SvgError::InvalidViewBox`] for an unrecognised alignment or
/// meet-or-slice keyword.
pub fn parse_aspect_ratio(value: &str) -> Result<AspectRatio, SvgError> {
    let mut words = value.split_whitespace();
    // The `defer` prefix only applies to a referenced image's own ratio,
    // which this decoder never has, so it is accepted and ignored.
    let mut align_word = words.next().unwrap_or("xMidYMid");
    if align_word == "defer" {
        align_word = words.next().unwrap_or("xMidYMid");
    }
    let align = match align_word {
        "none" => Align::None,
        "xMinYMin" => Align::Min,
        "xMidYMid" => Align::Mid,
        "xMaxYMax" => Align::Max,
        "xMinYMid" => Align::Mixed(MixedAlign { x: 0, y: 5 }),
        "xMinYMax" => Align::Mixed(MixedAlign { x: 0, y: 10 }),
        "xMidYMin" => Align::Mixed(MixedAlign { x: 5, y: 0 }),
        "xMidYMax" => Align::Mixed(MixedAlign { x: 5, y: 10 }),
        "xMaxYMin" => Align::Mixed(MixedAlign { x: 10, y: 0 }),
        "xMaxYMid" => Align::Mixed(MixedAlign { x: 10, y: 5 }),
        _ => return Err(SvgError::InvalidViewBox),
    };
    let slice = match words.next() {
        None | Some("meet") => false,
        Some("slice") => true,
        Some(_) => return Err(SvgError::InvalidViewBox),
    };
    if words.next().is_some() {
        return Err(SvgError::InvalidViewBox);
    }
    Ok(AspectRatio { align, slice })
}

impl Align {
    /// The fraction of the spare space that goes before the content on each
    /// axis.
    fn anchor(self) -> (f64, f64) {
        match self {
            Self::None | Self::Min => (0.0, 0.0),
            Self::Mid => (0.5, 0.5),
            Self::Max => (1.0, 1.0),
            Self::Mixed(mixed) => (f64::from(mixed.x) / 10.0, f64::from(mixed.y) / 10.0),
        }
    }
}

/// The map that places `view_box` inside a `viewport`-sized box whose origin
/// is `(0, 0)`, honouring `ratio`.
#[must_use]
pub fn viewport_transform(view_box: ViewBox, viewport: (f64, f64), ratio: AspectRatio) -> Affine {
    let sx = viewport.0 / view_box.size.0;
    let sy = viewport.1 / view_box.size.1;
    let (sx, sy) = if ratio.align == Align::None {
        (sx, sy)
    } else if ratio.slice {
        let s = sx.max(sy);
        (s, s)
    } else {
        let s = sx.min(sy);
        (s, s)
    };
    let (ax, ay) = ratio.align.anchor();
    let tx = (viewport.0 - view_box.size.0 * sx) * ax;
    let ty = (viewport.1 - view_box.size.1 * sy) * ay;
    Affine::translate(-view_box.min.0, -view_box.min.1)
        .then(Affine::scale(sx, sy))
        .then(Affine::translate(tx, ty))
}

/// Parse an element's own `width` or `height` when it establishes a viewport.
///
/// An absent value means "all of the available space", which SVG spells as
/// `100%`.
///
/// # Errors
/// Returns [`SvgError::InvalidNumber`] for a malformed length.
pub fn parse_viewport_extent(value: Option<&str>, available: f64) -> Result<f64, SvgError> {
    match value {
        None => Ok(available),
        Some(text) => crate::number::parse_length(text, available),
    }
}

/// Parse a plain optional coordinate, defaulting to zero.
///
/// # Errors
/// Returns [`SvgError::InvalidNumber`] for a malformed number.
pub fn parse_optional_number(value: Option<&str>) -> Result<f64, SvgError> {
    value.map_or(Ok(0.0), parse_number)
}

#[cfg(test)]
#[path = "transform_tests.rs"]
mod tests;
