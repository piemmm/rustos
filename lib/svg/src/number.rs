//! SVG's number, length, and coordinate-list grammar.
//!
//! Everything numeric in an SVG document — a coordinate in a `d` attribute, a
//! `viewBox`, a `stroke-width`, a transform argument, a gradient offset —
//! is spelled in one of a few closely related forms: a signed decimal with an
//! optional exponent, optionally followed by a unit, and often run together
//! with its neighbours by nothing more than a minus sign (`M0-4L2-6`).
//!
//! The digits themselves are scanned by the shared C-locale scanner
//! ([`tairix_util::cnum::scan_double`]), so this module owns only the parts
//! that are SVG's own: separator handling, the unit table, percentages, and
//! the single-character flags an elliptical arc uses.
//!
//! Every entry point is total and fails closed: a value that is not a finite
//! number in the accepted grammar is [`SvgError::InvalidNumber`], never a
//! guess and never a `NaN` handed downstream.

use tairix_util::cnum::scan_double;

use crate::error::SvgError;

/// User units per CSS inch, the ratio the other absolute units are defined
/// against.
const UNITS_PER_INCH: f64 = 96.0;

/// A cursor over a run of numbers separated by whitespace, commas, or — where
/// the sign is unambiguous — by nothing at all.
///
/// This is the grammar shared by `points`, `viewBox`, `transform` arguments,
/// `stroke-dasharray`, and path data, so those consumers do not each carry
/// their own separator handling.
#[derive(Clone, Debug)]
pub struct Numbers<'a> {
    rest: &'a str,
}

impl<'a> Numbers<'a> {
    /// A cursor over `text`.
    #[must_use]
    pub fn new(text: &'a str) -> Self {
        Self { rest: text }
    }

    /// The unconsumed remainder, with any leading separators removed.
    #[must_use]
    pub fn remainder(&self) -> &'a str {
        skip_separators(self.rest)
    }

    /// Whether everything that remains is separators.
    #[must_use]
    pub fn is_exhausted(&self) -> bool {
        self.remainder().is_empty()
    }

    /// Take the next number, or `None` at the end of the run.
    ///
    /// # Errors
    /// Returns [`SvgError::InvalidNumber`] when what remains is not a number:
    /// a partially valid list is refused rather than half-applied.
    pub fn take(&mut self) -> Result<Option<f64>, SvgError> {
        let text = skip_separators(self.rest);
        if text.is_empty() {
            self.rest = text;
            return Ok(None);
        }
        let Some((value, consumed)) = scan(text) else {
            return Err(SvgError::InvalidNumber);
        };
        self.rest = &text[consumed..];
        Ok(Some(value))
    }

    /// Take the next number, refusing the end of the run.
    ///
    /// # Errors
    /// Returns [`SvgError::InvalidNumber`] when the run is exhausted or the
    /// next token is not a number.
    pub fn required(&mut self) -> Result<f64, SvgError> {
        self.take()?.ok_or(SvgError::InvalidNumber)
    }

    /// Take the next character if it is a letter, which in path data is a
    /// command and never part of a number.
    pub fn take_letter(&mut self) -> Option<char> {
        let text = skip_separators(self.rest);
        let mut chars = text.chars();
        let first = chars.next()?;
        if !first.is_ascii_alphabetic() {
            return None;
        }
        self.rest = chars.as_str();
        Some(first)
    }

    /// Take an elliptical arc's flag.
    ///
    /// A flag is a single `0` or `1` digit that may be run straight into the
    /// value after it (`a1 1 0 011 5`), so it cannot be scanned as a number.
    ///
    /// # Errors
    /// Returns [`SvgError::InvalidNumber`] for anything but a single `0`/`1`.
    pub fn required_flag(&mut self) -> Result<bool, SvgError> {
        let text = skip_separators(self.rest);
        let mut chars = text.chars();
        match chars.next() {
            Some('0') => {
                self.rest = chars.as_str();
                Ok(false)
            }
            Some('1') => {
                self.rest = chars.as_str();
                Ok(true)
            }
            _ => Err(SvgError::InvalidNumber),
        }
    }
}

/// Scan one SVG number from the start of `text`.
///
/// The shared C-locale scanner also accepts C's hexadecimal floats and its
/// `inf`/`nan` words, none of which are SVG numbers; a token carrying any
/// letter but an exponent's `e` is therefore refused rather than silently
/// read as a value the author never wrote.
fn scan(text: &str) -> Option<(f64, usize)> {
    let (value, consumed) = scan_double(text)?;
    let token = text.get(..consumed)?;
    let alien = token
        .bytes()
        .any(|b| b.is_ascii_alphabetic() && !matches!(b, b'e' | b'E'));
    (value.is_finite() && !alien).then_some((value, consumed))
}

/// `text` with any leading whitespace and at most one comma removed.
fn skip_separators(text: &str) -> &str {
    let text = text.trim_start_matches(is_svg_space);
    match text.strip_prefix(',') {
        Some(after) => after.trim_start_matches(is_svg_space),
        None => text,
    }
}

/// Whether `c` is one of SVG's whitespace characters.
fn is_svg_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\r' | '\n' | '\u{c}')
}

/// Parse `text` as exactly one number, with nothing but whitespace after it.
///
/// # Errors
/// Returns [`SvgError::InvalidNumber`] for an empty value, a malformed
/// number, or trailing text.
pub fn parse_number(text: &str) -> Result<f64, SvgError> {
    let text = text.trim();
    let Some((value, consumed)) = scan(text) else {
        return Err(SvgError::InvalidNumber);
    };
    if consumed != text.len() {
        return Err(SvgError::InvalidNumber);
    }
    Ok(value)
}

/// Parse a length: a number with an optional CSS absolute unit, or a
/// percentage of `basis`.
///
/// The absolute units are the CSS ones SVG inherits, all fixed ratios of the
/// 96-unit inch, so a document authored in millimetres lands on the same user
/// space as one authored in pixels.
///
/// # Errors
/// Returns [`SvgError::InvalidNumber`] for a malformed number or a unit
/// outside the absolute set (a font-relative `em`/`ex` has no meaning without
/// text, which this decoder does not render).
pub fn parse_length(text: &str, basis: f64) -> Result<f64, SvgError> {
    let text = text.trim();
    let Some((value, consumed)) = scan(text) else {
        return Err(SvgError::InvalidNumber);
    };
    let unit = text[consumed..].trim();
    let scaled = match unit {
        "" | "px" => value,
        "%" => value * basis / 100.0,
        "pt" => value * UNITS_PER_INCH / 72.0,
        "pc" => value * UNITS_PER_INCH / 6.0,
        "mm" => value * UNITS_PER_INCH / 25.4,
        "cm" => value * UNITS_PER_INCH / 2.54,
        "in" => value * UNITS_PER_INCH,
        "q" | "Q" => value * UNITS_PER_INCH / 101.6,
        _ => return Err(SvgError::InvalidNumber),
    };
    if scaled.is_finite() {
        Ok(scaled)
    } else {
        Err(SvgError::InvalidNumber)
    }
}

/// Parse an opacity: a number or a percentage, clamped to `0..=1`.
///
/// SVG clamps rather than rejects an out-of-range opacity, so `1.5` is fully
/// opaque and `-1` fully transparent; only a value that is not a number at
/// all is refused.
///
/// # Errors
/// Returns [`SvgError::InvalidNumber`] when `text` is not a number.
pub fn parse_opacity(text: &str) -> Result<f64, SvgError> {
    let text = text.trim();
    let value = match text.strip_suffix('%') {
        Some(percent) => parse_number(percent)? / 100.0,
        None => parse_number(text)?,
    };
    Ok(value.clamp(0.0, 1.0))
}

/// An opacity in `0..=1` as an 8-bit alpha, rounded to nearest.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the argument is clamped into 0..=1 before scaling, so the \
              product is a non-negative value no greater than 255"
)]
#[must_use]
pub fn opacity_to_alpha(opacity: f64) -> u8 {
    (opacity.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

#[cfg(test)]
#[path = "number_tests.rs"]
mod tests;
