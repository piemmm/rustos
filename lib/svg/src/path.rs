//! Turning SVG geometry attributes into a single filled polygon ring.
//!
//! The desktop's rasteriser fills polygons (`lib/raster`'s single
//! [`fill_polygon`] path); richer artwork is built by *stacking* filled layers,
//! not by curves or multi-contour rings. So the supported
//! geometry is exactly what maps to one ring of integer vertices:
//!
//! * `<polygon>` / `<polyline>` `points`;
//! * `<rect>` (`x`, `y`, `width`, `height`); and
//! * `<path>` `d` restricted to the straight-line commands `M`/`L`/`H`/`V`/`Z`
//!   (absolute and relative).
//!
//! Curves, arcs, and a second sub-path are rejected with
//! [`SvgError::UnsupportedPath`] so a richer asset fails closed to its caller's
//! fallback rather than rasterising wrongly. Coordinates are
//! integers in the design grid; a fractional or exponent literal is an
//! [`SvgError::InvalidNumber`] rejection.
//!
//! [`fill_polygon`]: tairix_raster::Surface::fill_polygon

use alloc::vec::Vec;

use crate::error::SvgError;

/// Parse a `points` list (`"x0,y0 x1,y1 …"`) into vertices.
///
/// # Errors
/// [`SvgError::InvalidNumber`] for a non-integer coordinate or an odd number
/// of coordinates.
pub fn parse_points(points: &str) -> Result<Vec<(i32, i32)>, SvgError> {
    let mut coords = Vec::new();
    for token in points
        .split(|c: char| c == ',' || c.is_ascii_whitespace())
        .filter(|t| !t.is_empty())
    {
        coords.push(parse_int(token)?);
    }
    if coords.len() % 2 != 0 {
        return Err(SvgError::InvalidNumber);
    }
    Ok(coords.chunks_exact(2).map(|p| (p[0], p[1])).collect())
}

/// Build the four corners of an axis-aligned `<rect>`.
///
/// # Errors
/// [`SvgError::InvalidNumber`] for a non-integer or negative extent.
pub fn rect_polygon(
    x: Option<&str>,
    y: Option<&str>,
    width: &str,
    height: &str,
) -> Result<Vec<(i32, i32)>, SvgError> {
    let x = x.map(parse_int).transpose()?.unwrap_or(0);
    let y = y.map(parse_int).transpose()?.unwrap_or(0);
    let w = parse_int(width)?;
    let h = parse_int(height)?;
    if w < 0 || h < 0 {
        return Err(SvgError::InvalidNumber);
    }
    let (x1, y1) = (x.saturating_add(w), y.saturating_add(h));
    Ok(alloc::vec![(x, y), (x1, y), (x1, y1), (x, y1)])
}

/// Parse a `<path>` `d` attribute restricted to straight-line commands.
///
/// # Errors
/// [`SvgError::UnsupportedPath`] for a curve, arc, or second sub-path;
/// [`SvgError::InvalidNumber`] for a non-integer coordinate; and
/// [`SvgError::Malformed`] for a command missing its operands.
pub fn parse_path(d: &str) -> Result<Vec<(i32, i32)>, SvgError> {
    let tokens = tokenize(d)?;
    let mut pts: Vec<(i32, i32)> = Vec::new();
    let mut cur = (0i32, 0i32);
    let mut current_cmd = 0u8;
    let mut have_move = false;
    let mut i = 0;
    while i < tokens.len() {
        let cmd = match tokens[i] {
            Token::Command(c) => {
                i += 1;
                c
            }
            Token::Number(_) => {
                if current_cmd == 0 {
                    return Err(SvgError::UnsupportedPath);
                }
                current_cmd
            }
        };
        let absolute = cmd.is_ascii_uppercase();
        match cmd {
            b'M' | b'm' | b'L' | b'l' => {
                let x = take_number(&tokens, &mut i)?;
                let y = take_number(&tokens, &mut i)?;
                cur = if absolute {
                    (x, y)
                } else {
                    (cur.0.saturating_add(x), cur.1.saturating_add(y))
                };
                if cmd == b'M' || cmd == b'm' {
                    if have_move {
                        return Err(SvgError::UnsupportedPath);
                    }
                    have_move = true;
                    current_cmd = if absolute { b'L' } else { b'l' };
                } else {
                    current_cmd = cmd;
                }
                pts.push(cur);
            }
            b'H' | b'h' => {
                let x = take_number(&tokens, &mut i)?;
                cur.0 = if absolute { x } else { cur.0.saturating_add(x) };
                current_cmd = cmd;
                pts.push(cur);
            }
            b'V' | b'v' => {
                let y = take_number(&tokens, &mut i)?;
                cur.1 = if absolute { y } else { cur.1.saturating_add(y) };
                current_cmd = cmd;
                pts.push(cur);
            }
            b'Z' | b'z' => {
                // Close path: there is no current command for any operands that
                // follow, so a stray number after `Z` is out of subset rather
                // than an implicit repeat (which would not consume it).
                current_cmd = 0;
            }
            _ => return Err(SvgError::UnsupportedPath),
        }
    }
    Ok(pts)
}

/// A `<path>` token: a command letter or an integer operand.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Token {
    Command(u8),
    Number(i32),
}

/// Split a `d` attribute into command/number tokens.
fn tokenize(d: &str) -> Result<Vec<Token>, SvgError> {
    let bytes = d.as_bytes();
    let n = bytes.len();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < n {
        let b = bytes[i];
        if b == b',' || b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if b.is_ascii_alphabetic() {
            tokens.push(Token::Command(b));
            i += 1;
            continue;
        }
        let start = i;
        if b == b'+' || b == b'-' {
            i += 1;
        }
        let digits_start = i;
        while i < n && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == digits_start {
            return Err(SvgError::InvalidNumber);
        }
        if i < n && (bytes[i] == b'.' || bytes[i] == b'e' || bytes[i] == b'E') {
            return Err(SvgError::InvalidNumber);
        }
        tokens.push(Token::Number(parse_int(&d[start..i])?));
    }
    Ok(tokens)
}

/// Take the next operand, erroring if the next token is not a number.
fn take_number(tokens: &[Token], i: &mut usize) -> Result<i32, SvgError> {
    match tokens.get(*i) {
        Some(Token::Number(v)) => {
            *i += 1;
            Ok(*v)
        }
        _ => Err(SvgError::Malformed),
    }
}

/// Parse an integer coordinate or length, tolerating a leading `+`.
pub(crate) fn parse_int(token: &str) -> Result<i32, SvgError> {
    token.trim().parse().map_err(|_| SvgError::InvalidNumber)
}
