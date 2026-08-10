//! CSS/SVG colour values, resolved to a straight-alpha [`Color`].
//!
//! Every colour-valued SVG property — `fill`, `stroke`, `stop-color`,
//! `flood-color` — is spelled in the one CSS colour grammar, so it is parsed
//! in one place: hex (`#rgb` / `#rgba` / `#rrggbb` / `#rrggbbaa`), the
//! `rgb()` / `rgba()` and `hsl()` / `hsla()` functions in both their
//! comma-separated (CSS Color 3) and space-with-slash (CSS Color 4)
//! spellings, the 148 CSS named colours, `transparent`, `none`, and
//! `currentColor`.
//!
//! Assets are untrusted, so every entry point is total: no input panics, and
//! anything outside the grammar is [`SvgError::InvalidColor`] rather than a
//! guessed colour.

use core::cmp::Ordering;
use core::f64::consts::PI;

use tairix_raster::Color;
use tairix_util::mathf::{clamp, floor, fmax, fmin, round};

use crate::error::SvgError;
use crate::number::{opacity_to_alpha, parse_number, parse_opacity};

/// What a colour-valued property resolved to.
///
/// `none` and `currentColor` are colour *values* in CSS, not errors, but
/// neither denotes a paintable colour on its own: the first paints nothing,
/// and the second stands for the inherited `color` property, which only the
/// style resolver knows.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ColorSpec {
    /// The `none` or `transparent` keyword: nothing is painted.
    None,
    /// The `currentColor` keyword, to be substituted by the inherited
    /// `color` property.
    Current,
    /// A resolved colour.
    Value(Color),
}

/// Parse any CSS colour value.
///
/// Leading and trailing whitespace is ignored, and every keyword, function
/// name, unit, and hex digit is matched case-insensitively, as CSS is.
/// Out-of-range channels, alphas, and hues are clamped or wrapped the way CSS
/// clamps them; only a value outside the grammar is refused.
///
/// # Errors
/// Returns [`SvgError::InvalidColor`] for anything that is not a colour in
/// the supported grammar, including a malformed function argument list.
pub fn parse_color(text: &str) -> Result<ColorSpec, SvgError> {
    let text = text.trim();
    if let Some(hex) = text.strip_prefix('#') {
        return parse_hex(hex).map(ColorSpec::Value);
    }
    if let Some((name, arguments)) = text.split_once('(') {
        let body = arguments
            .strip_suffix(')')
            .ok_or(SvgError::InvalidColor)?
            .trim();
        return parse_function(name, body).map(ColorSpec::Value);
    }
    if text.eq_ignore_ascii_case("none") || text.eq_ignore_ascii_case("transparent") {
        return Ok(ColorSpec::None);
    }
    if text.eq_ignore_ascii_case("currentcolor") {
        return Ok(ColorSpec::Current);
    }
    named_color(text).map(ColorSpec::Value)
}

/// Parse a `fill` value, optionally scaled by a `fill-opacity` value.
///
/// Returns `Ok(None)` when the fill paints nothing — the `none` /
/// `transparent` keywords, or any fully transparent result — and
/// `Ok(Some(colour))` for a colour that paints. `currentColor` is refused
/// here rather than guessed: this entry point has no inherited `color` to
/// substitute.
///
/// The opacity scales the colour's own alpha rather than replacing it, so a
/// half-opaque colour at half opacity is a quarter opaque.
///
/// # Errors
/// Returns [`SvgError::InvalidColor`] for a fill outside the colour grammar,
/// and [`SvgError::InvalidNumber`] for a `fill-opacity` that is not a number
/// or percentage.
pub fn parse_fill(fill: &str, opacity: Option<&str>) -> Result<Option<Color>, SvgError> {
    let base = match parse_color(fill)? {
        ColorSpec::None => return Ok(None),
        ColorSpec::Current => return Err(SvgError::InvalidColor),
        ColorSpec::Value(color) => color,
    };
    let alpha = match opacity {
        Some(raw) => opacity_to_alpha(f64::from(base.a) / 255.0 * parse_opacity(raw)?),
        None => base.a,
    };
    if alpha == 0 {
        return Ok(None);
    }
    Ok(Some(Color::rgba(base.r, base.g, base.b, alpha)))
}

/// Parse a `#rgb`, `#rgba`, `#rrggbb`, or `#rrggbbaa` literal.
fn parse_hex(hex: &str) -> Result<Color, SvgError> {
    match hex.len() {
        3 => Ok(Color::rgb(
            nibble_pair(hex, 0)?,
            nibble_pair(hex, 1)?,
            nibble_pair(hex, 2)?,
        )),
        4 => Ok(Color::rgba(
            nibble_pair(hex, 0)?,
            nibble_pair(hex, 1)?,
            nibble_pair(hex, 2)?,
            nibble_pair(hex, 3)?,
        )),
        6 => Ok(Color::rgb(
            byte_pair(hex, 0)?,
            byte_pair(hex, 1)?,
            byte_pair(hex, 2)?,
        )),
        8 => Ok(Color::rgba(
            byte_pair(hex, 0)?,
            byte_pair(hex, 1)?,
            byte_pair(hex, 2)?,
            byte_pair(hex, 3)?,
        )),
        _ => Err(SvgError::InvalidColor),
    }
}

/// Expand the `nth` single hex nibble of a `#rgb`/`#rgba` literal to a byte
/// (`f` → `0xff`), matching the CSS shorthand rule.
fn nibble_pair(hex: &str, nth: usize) -> Result<u8, SvgError> {
    let digit = hex
        .as_bytes()
        .get(nth)
        .copied()
        .ok_or(SvgError::InvalidColor)?;
    Ok(hex_digit(digit)? * 17)
}

/// Parse the `nth` byte (two hex digits) of a `#rrggbb`/`#rrggbbaa` literal.
fn byte_pair(hex: &str, nth: usize) -> Result<u8, SvgError> {
    let bytes = hex.as_bytes();
    let hi = bytes.get(nth * 2).copied().ok_or(SvgError::InvalidColor)?;
    let lo = bytes
        .get(nth * 2 + 1)
        .copied()
        .ok_or(SvgError::InvalidColor)?;
    Ok(hex_digit(hi)? * 16 + hex_digit(lo)?)
}

/// Map one ASCII hex digit to its `0..=15` value.
fn hex_digit(byte: u8) -> Result<u8, SvgError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(SvgError::InvalidColor),
    }
}

/// Resolve one colour function from its name and its argument list (the text
/// between the parentheses).
fn parse_function(name: &str, body: &str) -> Result<Color, SvgError> {
    if name.eq_ignore_ascii_case("rgb") || name.eq_ignore_ascii_case("rgba") {
        return Arguments::split(body)?.rgb();
    }
    if name.eq_ignore_ascii_case("hsl") || name.eq_ignore_ascii_case("hsla") {
        return Arguments::split(body)?.hsl();
    }
    Err(SvgError::InvalidColor)
}

/// The three channel arguments of a colour function and its optional alpha.
struct Arguments<'a> {
    channels: [&'a str; 3],
    alpha: Option<&'a str>,
}

impl<'a> Arguments<'a> {
    /// Split a colour function's argument list in either CSS spelling.
    ///
    /// The two spellings are exclusive: a comma list carries its alpha as a
    /// fourth comma-separated argument, a whitespace list carries it after a
    /// `/`. Mixing them, or any other argument count, is malformed.
    fn split(body: &'a str) -> Result<Self, SvgError> {
        if body.contains(',') {
            let mut parts = body.split(',');
            let channels = [
                argument(parts.next())?,
                argument(parts.next())?,
                argument(parts.next())?,
            ];
            let alpha = match parts.next() {
                Some(text) => Some(argument(Some(text))?),
                None => None,
            };
            if parts.next().is_some() {
                return Err(SvgError::InvalidColor);
            }
            return Ok(Self { channels, alpha });
        }
        let (values, alpha) = match body.split_once('/') {
            Some((values, alpha)) => (values, Some(argument(Some(alpha))?)),
            None => (body, None),
        };
        let mut words = values.split_ascii_whitespace();
        let channels = [
            argument(words.next())?,
            argument(words.next())?,
            argument(words.next())?,
        ];
        if words.next().is_some() {
            return Err(SvgError::InvalidColor);
        }
        Ok(Self { channels, alpha })
    }

    /// Resolve `rgb()` / `rgba()` arguments.
    ///
    /// CSS requires the three channels to agree on their type, so a mixed
    /// `rgb(255, 50%, 0)` is malformed rather than half-interpreted.
    fn rgb(&self) -> Result<Color, SvgError> {
        let percentages = self.channels[0].ends_with('%');
        let mut bytes = [0u8; 3];
        for (byte, text) in bytes.iter_mut().zip(self.channels) {
            if text.ends_with('%') != percentages {
                return Err(SvgError::InvalidColor);
            }
            *byte = if percentages {
                to_byte(parse_percentage(text)? * 255.0 / 100.0)
            } else {
                to_byte(number(text)?)
            };
        }
        Ok(Color::rgba(bytes[0], bytes[1], bytes[2], self.to_alpha()?))
    }

    /// Resolve `hsl()` / `hsla()` arguments.
    fn hsl(&self) -> Result<Color, SvgError> {
        let hue = parse_hue(self.channels[0])?;
        let saturation = clamp(parse_percentage(self.channels[1])? / 100.0, 0.0, 1.0);
        let lightness = clamp(parse_percentage(self.channels[2])? / 100.0, 0.0, 1.0);
        let alpha = self.to_alpha()?;
        Ok(hsl_to_rgb(hue, saturation, lightness, alpha))
    }

    /// The alpha argument as an 8-bit value, defaulting to fully opaque.
    fn to_alpha(&self) -> Result<u8, SvgError> {
        match self.alpha {
            Some(text) => Ok(opacity_to_alpha(
                parse_opacity(text).map_err(|_| SvgError::InvalidColor)?,
            )),
            None => Ok(u8::MAX),
        }
    }
}

/// Trim one argument, refusing a missing or empty one.
fn argument(text: Option<&str>) -> Result<&str, SvgError> {
    let text = text.ok_or(SvgError::InvalidColor)?.trim();
    if text.is_empty() {
        return Err(SvgError::InvalidColor);
    }
    Ok(text)
}

/// Parse a plain number argument.
fn number(text: &str) -> Result<f64, SvgError> {
    parse_number(text).map_err(|_| SvgError::InvalidColor)
}

/// Parse a `<percentage>` argument, returning its numeric part.
fn parse_percentage(text: &str) -> Result<f64, SvgError> {
    let value = text.strip_suffix('%').ok_or(SvgError::InvalidColor)?;
    number(value)
}

/// Parse a hue in degrees, accepting the CSS angle units.
fn parse_hue(text: &str) -> Result<f64, SvgError> {
    /// Degrees per unit. `grad` is tested before `rad`, of which it is a
    /// suffix, so `1grad` is not read as `1g` radians.
    const UNITS: [(&str, f64); 4] = [
        ("deg", 1.0),
        ("grad", 360.0 / 400.0),
        ("rad", 180.0 / PI),
        ("turn", 360.0),
    ];
    for (unit, degrees) in UNITS {
        if let Some(value) = strip_unit(text, unit) {
            return Ok(number(value)? * degrees);
        }
    }
    number(text)
}

/// `text` without a trailing `unit`, matched case-insensitively.
fn strip_unit<'a>(text: &'a str, unit: &str) -> Option<&'a str> {
    let split = text.len().checked_sub(unit.len())?;
    let value = text.get(..split)?;
    text.get(split..)?
        .eq_ignore_ascii_case(unit)
        .then_some(value)
}

/// Convert CSS HSL to RGB.
///
/// The reference conversion from CSS Color 4: one helper sampled at three
/// points around the hue circle, which needs no per-sector branching.
fn hsl_to_rgb(hue: f64, saturation: f64, lightness: f64, alpha: u8) -> Color {
    let reach = saturation * fmin(lightness, 1.0 - lightness);
    let channel = |n: f64| {
        let k = wrap(n + hue / 30.0, 12.0);
        let sector = fmax(-1.0, fmin(fmin(k - 3.0, 9.0 - k), 1.0));
        to_byte((lightness - reach * sector) * 255.0)
    };
    Color::rgba(channel(0.0), channel(8.0), channel(4.0), alpha)
}

/// `value` reduced into `0..period`, so a hue wraps rather than failing.
///
/// Spelled with [`floor`] rather than `%`, which on `f64` would call out to
/// an external `fmod`.
fn wrap(value: f64, period: f64) -> f64 {
    value - floor(value / period) * period
}

/// Round a channel to the nearest `0..=255` byte, clamping as CSS does.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the value is rounded and clamped into 0..=255 before the cast, \
              so neither the truncation nor the sign loss can occur"
)]
fn to_byte(value: f64) -> u8 {
    clamp(round(value), 0.0, 255.0) as u8
}

/// Look up one of the CSS named colours.
fn named_color(name: &str) -> Result<Color, SvgError> {
    NAMED_COLORS
        .binary_search_by(|(candidate, _)| compare_ascii_lowercase(candidate, name))
        .ok()
        .and_then(|index| NAMED_COLORS.get(index))
        .map(|(_, color)| *color)
        .ok_or(SvgError::InvalidColor)
}

/// Order a lowercase table entry against a probe of any case.
fn compare_ascii_lowercase(candidate: &str, probe: &str) -> Ordering {
    for (entry, wanted) in candidate.bytes().zip(probe.bytes()) {
        let wanted = wanted.to_ascii_lowercase();
        if entry != wanted {
            return entry.cmp(&wanted);
        }
    }
    candidate.len().cmp(&probe.len())
}

/// The CSS named colours, sorted by name so [`named_color`] can binary-search
/// them.
///
/// A sorted table rather than a 148-arm `match`: the names are data, so the
/// lookup is a logarithmic binary search instead of a linear chain of string
/// comparisons, and the ordering it depends on is checked by a unit test
/// rather than trusted to the eye.
static NAMED_COLORS: &[(&str, Color)] = &[
    ("aliceblue", Color::rgb(0xf0, 0xf8, 0xff)),
    ("antiquewhite", Color::rgb(0xfa, 0xeb, 0xd7)),
    ("aqua", Color::rgb(0x00, 0xff, 0xff)),
    ("aquamarine", Color::rgb(0x7f, 0xff, 0xd4)),
    ("azure", Color::rgb(0xf0, 0xff, 0xff)),
    ("beige", Color::rgb(0xf5, 0xf5, 0xdc)),
    ("bisque", Color::rgb(0xff, 0xe4, 0xc4)),
    ("black", Color::rgb(0x00, 0x00, 0x00)),
    ("blanchedalmond", Color::rgb(0xff, 0xeb, 0xcd)),
    ("blue", Color::rgb(0x00, 0x00, 0xff)),
    ("blueviolet", Color::rgb(0x8a, 0x2b, 0xe2)),
    ("brown", Color::rgb(0xa5, 0x2a, 0x2a)),
    ("burlywood", Color::rgb(0xde, 0xb8, 0x87)),
    ("cadetblue", Color::rgb(0x5f, 0x9e, 0xa0)),
    ("chartreuse", Color::rgb(0x7f, 0xff, 0x00)),
    ("chocolate", Color::rgb(0xd2, 0x69, 0x1e)),
    ("coral", Color::rgb(0xff, 0x7f, 0x50)),
    ("cornflowerblue", Color::rgb(0x64, 0x95, 0xed)),
    ("cornsilk", Color::rgb(0xff, 0xf8, 0xdc)),
    ("crimson", Color::rgb(0xdc, 0x14, 0x3c)),
    ("cyan", Color::rgb(0x00, 0xff, 0xff)),
    ("darkblue", Color::rgb(0x00, 0x00, 0x8b)),
    ("darkcyan", Color::rgb(0x00, 0x8b, 0x8b)),
    ("darkgoldenrod", Color::rgb(0xb8, 0x86, 0x0b)),
    ("darkgray", Color::rgb(0xa9, 0xa9, 0xa9)),
    ("darkgreen", Color::rgb(0x00, 0x64, 0x00)),
    ("darkgrey", Color::rgb(0xa9, 0xa9, 0xa9)),
    ("darkkhaki", Color::rgb(0xbd, 0xb7, 0x6b)),
    ("darkmagenta", Color::rgb(0x8b, 0x00, 0x8b)),
    ("darkolivegreen", Color::rgb(0x55, 0x6b, 0x2f)),
    ("darkorange", Color::rgb(0xff, 0x8c, 0x00)),
    ("darkorchid", Color::rgb(0x99, 0x32, 0xcc)),
    ("darkred", Color::rgb(0x8b, 0x00, 0x00)),
    ("darksalmon", Color::rgb(0xe9, 0x96, 0x7a)),
    ("darkseagreen", Color::rgb(0x8f, 0xbc, 0x8f)),
    ("darkslateblue", Color::rgb(0x48, 0x3d, 0x8b)),
    ("darkslategray", Color::rgb(0x2f, 0x4f, 0x4f)),
    ("darkslategrey", Color::rgb(0x2f, 0x4f, 0x4f)),
    ("darkturquoise", Color::rgb(0x00, 0xce, 0xd1)),
    ("darkviolet", Color::rgb(0x94, 0x00, 0xd3)),
    ("deeppink", Color::rgb(0xff, 0x14, 0x93)),
    ("deepskyblue", Color::rgb(0x00, 0xbf, 0xff)),
    ("dimgray", Color::rgb(0x69, 0x69, 0x69)),
    ("dimgrey", Color::rgb(0x69, 0x69, 0x69)),
    ("dodgerblue", Color::rgb(0x1e, 0x90, 0xff)),
    ("firebrick", Color::rgb(0xb2, 0x22, 0x22)),
    ("floralwhite", Color::rgb(0xff, 0xfa, 0xf0)),
    ("forestgreen", Color::rgb(0x22, 0x8b, 0x22)),
    ("fuchsia", Color::rgb(0xff, 0x00, 0xff)),
    ("gainsboro", Color::rgb(0xdc, 0xdc, 0xdc)),
    ("ghostwhite", Color::rgb(0xf8, 0xf8, 0xff)),
    ("gold", Color::rgb(0xff, 0xd7, 0x00)),
    ("goldenrod", Color::rgb(0xda, 0xa5, 0x20)),
    ("gray", Color::rgb(0x80, 0x80, 0x80)),
    ("green", Color::rgb(0x00, 0x80, 0x00)),
    ("greenyellow", Color::rgb(0xad, 0xff, 0x2f)),
    ("grey", Color::rgb(0x80, 0x80, 0x80)),
    ("honeydew", Color::rgb(0xf0, 0xff, 0xf0)),
    ("hotpink", Color::rgb(0xff, 0x69, 0xb4)),
    ("indianred", Color::rgb(0xcd, 0x5c, 0x5c)),
    ("indigo", Color::rgb(0x4b, 0x00, 0x82)),
    ("ivory", Color::rgb(0xff, 0xff, 0xf0)),
    ("khaki", Color::rgb(0xf0, 0xe6, 0x8c)),
    ("lavender", Color::rgb(0xe6, 0xe6, 0xfa)),
    ("lavenderblush", Color::rgb(0xff, 0xf0, 0xf5)),
    ("lawngreen", Color::rgb(0x7c, 0xfc, 0x00)),
    ("lemonchiffon", Color::rgb(0xff, 0xfa, 0xcd)),
    ("lightblue", Color::rgb(0xad, 0xd8, 0xe6)),
    ("lightcoral", Color::rgb(0xf0, 0x80, 0x80)),
    ("lightcyan", Color::rgb(0xe0, 0xff, 0xff)),
    ("lightgoldenrodyellow", Color::rgb(0xfa, 0xfa, 0xd2)),
    ("lightgray", Color::rgb(0xd3, 0xd3, 0xd3)),
    ("lightgreen", Color::rgb(0x90, 0xee, 0x90)),
    ("lightgrey", Color::rgb(0xd3, 0xd3, 0xd3)),
    ("lightpink", Color::rgb(0xff, 0xb6, 0xc1)),
    ("lightsalmon", Color::rgb(0xff, 0xa0, 0x7a)),
    ("lightseagreen", Color::rgb(0x20, 0xb2, 0xaa)),
    ("lightskyblue", Color::rgb(0x87, 0xce, 0xfa)),
    ("lightslategray", Color::rgb(0x77, 0x88, 0x99)),
    ("lightslategrey", Color::rgb(0x77, 0x88, 0x99)),
    ("lightsteelblue", Color::rgb(0xb0, 0xc4, 0xde)),
    ("lightyellow", Color::rgb(0xff, 0xff, 0xe0)),
    ("lime", Color::rgb(0x00, 0xff, 0x00)),
    ("limegreen", Color::rgb(0x32, 0xcd, 0x32)),
    ("linen", Color::rgb(0xfa, 0xf0, 0xe6)),
    ("magenta", Color::rgb(0xff, 0x00, 0xff)),
    ("maroon", Color::rgb(0x80, 0x00, 0x00)),
    ("mediumaquamarine", Color::rgb(0x66, 0xcd, 0xaa)),
    ("mediumblue", Color::rgb(0x00, 0x00, 0xcd)),
    ("mediumorchid", Color::rgb(0xba, 0x55, 0xd3)),
    ("mediumpurple", Color::rgb(0x93, 0x70, 0xdb)),
    ("mediumseagreen", Color::rgb(0x3c, 0xb3, 0x71)),
    ("mediumslateblue", Color::rgb(0x7b, 0x68, 0xee)),
    ("mediumspringgreen", Color::rgb(0x00, 0xfa, 0x9a)),
    ("mediumturquoise", Color::rgb(0x48, 0xd1, 0xcc)),
    ("mediumvioletred", Color::rgb(0xc7, 0x15, 0x85)),
    ("midnightblue", Color::rgb(0x19, 0x19, 0x70)),
    ("mintcream", Color::rgb(0xf5, 0xff, 0xfa)),
    ("mistyrose", Color::rgb(0xff, 0xe4, 0xe1)),
    ("moccasin", Color::rgb(0xff, 0xe4, 0xb5)),
    ("navajowhite", Color::rgb(0xff, 0xde, 0xad)),
    ("navy", Color::rgb(0x00, 0x00, 0x80)),
    ("oldlace", Color::rgb(0xfd, 0xf5, 0xe6)),
    ("olive", Color::rgb(0x80, 0x80, 0x00)),
    ("olivedrab", Color::rgb(0x6b, 0x8e, 0x23)),
    ("orange", Color::rgb(0xff, 0xa5, 0x00)),
    ("orangered", Color::rgb(0xff, 0x45, 0x00)),
    ("orchid", Color::rgb(0xda, 0x70, 0xd6)),
    ("palegoldenrod", Color::rgb(0xee, 0xe8, 0xaa)),
    ("palegreen", Color::rgb(0x98, 0xfb, 0x98)),
    ("paleturquoise", Color::rgb(0xaf, 0xee, 0xee)),
    ("palevioletred", Color::rgb(0xdb, 0x70, 0x93)),
    ("papayawhip", Color::rgb(0xff, 0xef, 0xd5)),
    ("peachpuff", Color::rgb(0xff, 0xda, 0xb9)),
    ("peru", Color::rgb(0xcd, 0x85, 0x3f)),
    ("pink", Color::rgb(0xff, 0xc0, 0xcb)),
    ("plum", Color::rgb(0xdd, 0xa0, 0xdd)),
    ("powderblue", Color::rgb(0xb0, 0xe0, 0xe6)),
    ("purple", Color::rgb(0x80, 0x00, 0x80)),
    ("rebeccapurple", Color::rgb(0x66, 0x33, 0x99)),
    ("red", Color::rgb(0xff, 0x00, 0x00)),
    ("rosybrown", Color::rgb(0xbc, 0x8f, 0x8f)),
    ("royalblue", Color::rgb(0x41, 0x69, 0xe1)),
    ("saddlebrown", Color::rgb(0x8b, 0x45, 0x13)),
    ("salmon", Color::rgb(0xfa, 0x80, 0x72)),
    ("sandybrown", Color::rgb(0xf4, 0xa4, 0x60)),
    ("seagreen", Color::rgb(0x2e, 0x8b, 0x57)),
    ("seashell", Color::rgb(0xff, 0xf5, 0xee)),
    ("sienna", Color::rgb(0xa0, 0x52, 0x2d)),
    ("silver", Color::rgb(0xc0, 0xc0, 0xc0)),
    ("skyblue", Color::rgb(0x87, 0xce, 0xeb)),
    ("slateblue", Color::rgb(0x6a, 0x5a, 0xcd)),
    ("slategray", Color::rgb(0x70, 0x80, 0x90)),
    ("slategrey", Color::rgb(0x70, 0x80, 0x90)),
    ("snow", Color::rgb(0xff, 0xfa, 0xfa)),
    ("springgreen", Color::rgb(0x00, 0xff, 0x7f)),
    ("steelblue", Color::rgb(0x46, 0x82, 0xb4)),
    ("tan", Color::rgb(0xd2, 0xb4, 0x8c)),
    ("teal", Color::rgb(0x00, 0x80, 0x80)),
    ("thistle", Color::rgb(0xd8, 0xbf, 0xd8)),
    ("tomato", Color::rgb(0xff, 0x63, 0x47)),
    ("turquoise", Color::rgb(0x40, 0xe0, 0xd0)),
    ("violet", Color::rgb(0xee, 0x82, 0xee)),
    ("wheat", Color::rgb(0xf5, 0xde, 0xb3)),
    ("white", Color::rgb(0xff, 0xff, 0xff)),
    ("whitesmoke", Color::rgb(0xf5, 0xf5, 0xf5)),
    ("yellow", Color::rgb(0xff, 0xff, 0x00)),
    ("yellowgreen", Color::rgb(0x9a, 0xcd, 0x32)),
];

#[cfg(test)]
#[path = "color_tests.rs"]
mod tests;
