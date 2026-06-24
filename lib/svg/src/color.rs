//! Parsing the SVG `fill` colour subset into a straight-alpha [`Color`].
//!
//! WM/desktop SVG assets fill their shapes with a small, closed colour
//! vocabulary: hex (`#rgb` / `#rrggbb`), a handful of named colours, and the
//! `none` / `transparent` keywords that mean "draw no fill". The optional
//! `fill-opacity` attribute scales the alpha. Anything outside this subset is
//! rejected so a malformed asset fails closed to its caller's fallback rather
//! than guessing a colour.

use rustos_raster::Color;

use crate::error::SvgError;

/// Parse a `fill` value, optionally scaled by a `fill-opacity` value.
///
/// Returns `Ok(None)` when the fill is `none`/`transparent` (the shape draws
/// nothing and contributes no layer), `Ok(Some(colour))` for a resolved
/// colour, and `Err` for any value outside the supported subset.
///
/// # Errors
/// Returns [`SvgError::InvalidColor`] for an unrecognised colour keyword or
/// malformed hex, and [`SvgError::InvalidNumber`] for a `fill-opacity` that is
/// not a real number in `0..=1`.
pub fn parse_fill(fill: &str, opacity: Option<&str>) -> Result<Option<Color>, SvgError> {
    let Some(base) = parse_color(fill.trim())? else {
        return Ok(None);
    };
    let Some(raw) = opacity else {
        return Ok(Some(base));
    };
    let permille = parse_permille(raw.trim())?;
    let scaled = scale_permille(base.a, permille);
    if scaled == 0 {
        return Ok(None);
    }
    Ok(Some(Color::rgba(base.r, base.g, base.b, scaled)))
}

/// Parse a bare colour keyword or hex literal.
///
/// Returns `Ok(None)` for `none`/`transparent`.
fn parse_color(value: &str) -> Result<Option<Color>, SvgError> {
    if value.is_empty() {
        return Err(SvgError::InvalidColor);
    }
    if let Some(hex) = value.strip_prefix('#') {
        return parse_hex(hex).map(Some);
    }
    match value {
        "none" | "transparent" => Ok(None),
        "black" => Ok(Some(Color::rgb(0, 0, 0))),
        "white" => Ok(Some(Color::rgb(255, 255, 255))),
        "red" => Ok(Some(Color::rgb(255, 0, 0))),
        "green" => Ok(Some(Color::rgb(0, 128, 0))),
        "blue" => Ok(Some(Color::rgb(0, 0, 255))),
        "gray" | "grey" => Ok(Some(Color::rgb(128, 128, 128))),
        "yellow" => Ok(Some(Color::rgb(255, 255, 0))),
        _ => Err(SvgError::InvalidColor),
    }
}

/// Parse a `#rgb`, `#rgba`, `#rrggbb`, or `#rrggbbaa` hex literal.
fn parse_hex(hex: &str) -> Result<Color, SvgError> {
    match hex.len() {
        3 => {
            let r = nibble_pair(hex, 0)?;
            let g = nibble_pair(hex, 1)?;
            let b = nibble_pair(hex, 2)?;
            Ok(Color::rgb(r, g, b))
        }
        4 => {
            let r = nibble_pair(hex, 0)?;
            let g = nibble_pair(hex, 1)?;
            let b = nibble_pair(hex, 2)?;
            let a = nibble_pair(hex, 3)?;
            Ok(Color::rgba(r, g, b, a))
        }
        6 => {
            let r = byte_pair(hex, 0)?;
            let g = byte_pair(hex, 1)?;
            let b = byte_pair(hex, 2)?;
            Ok(Color::rgb(r, g, b))
        }
        8 => {
            let r = byte_pair(hex, 0)?;
            let g = byte_pair(hex, 1)?;
            let b = byte_pair(hex, 2)?;
            let a = byte_pair(hex, 3)?;
            Ok(Color::rgba(r, g, b, a))
        }
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
    let value = hex_digit(digit)?;
    Ok(value * 17)
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

/// Parse a `fill-opacity` decimal in `0..=1` into permille (`0..=1000`).
///
/// The desktop's geometry and opacities are exact integers, so the value is parsed with first-party fixed-point arithmetic
/// rather than a float, keeping the decoder allocation- and float-free. Up to
/// three fractional digits are honoured; further digits must be zero so no
/// precision is silently dropped.
fn parse_permille(value: &str) -> Result<u32, SvgError> {
    let (integer, fraction) = match value.split_once('.') {
        Some((int, frac)) => (int, frac),
        None => (value, ""),
    };
    let whole: u32 = match integer {
        "" => 0,
        digits => digits.parse().map_err(|_| SvgError::InvalidNumber)?,
    };
    if whole > 1 {
        return Err(SvgError::InvalidNumber);
    }
    let mut thousandths = 0u32;
    let mut scale = 100u32;
    for (index, byte) in fraction.bytes().enumerate() {
        if !byte.is_ascii_digit() {
            return Err(SvgError::InvalidNumber);
        }
        let digit = u32::from(byte - b'0');
        if index < 3 {
            thousandths += digit * scale;
            scale /= 10;
        } else if digit != 0 {
            return Err(SvgError::InvalidNumber);
        }
    }
    let permille = whole * 1000 + thousandths;
    if permille > 1000 {
        return Err(SvgError::InvalidNumber);
    }
    Ok(permille)
}

/// Scale a `u8` channel by a `0..=1000` permille factor, rounding to nearest.
fn scale_permille(value: u8, permille: u32) -> u8 {
    let scaled = (u32::from(value) * permille + 500) / 1000;
    u8::try_from(scaled.min(255)).unwrap_or(u8::MAX)
}
