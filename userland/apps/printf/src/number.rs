//! Numeric argument conversion: GNU `printf`'s reading of an ARGUMENT
//! for a numeric `%` conversion.
//!
//! The rules are C's, with the POSIX `printf` additions, exactly as the
//! GNU tool applies them:
//!
//! * A leading `'` or `"` converts the following character's code point
//!   (a *character constant*); further characters are ignored with a
//!   warning.
//! * Integers read as `strtoimax`/`strtoumax` in base 0: optional
//!   C-locale whitespace and sign, then `0x` hex, `0` octal, or decimal.
//!   An out-of-range value saturates (negatives wrap for the unsigned
//!   conversions, as in C).
//! * Floats read as `strtod` through the one shared scanner
//!   (`rustos_util::cnum`, also `seq`'s).
//! * An empty argument converts to zero silently; an argument with no
//!   number at all, a partially converted one, and an out-of-range one
//!   carry the GNU diagnostic as a typed [`Note`] the client reports —
//!   the conversion still yields its best value and the run continues.

use alloc::string::String;

use rustos_util::cnum::{c_isspace, scan_double};

/// What a conversion wants to say about its argument, mirroring the GNU
/// diagnostics. The first three are errors (the run exits `1`); the
/// trailing-characters note is a warning (the exit status is untouched).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Note {
    /// `'X': expected a numeric value` — no number starts the argument.
    ExpectedNumeric,
    /// `'X': value not completely converted` — text follows the number.
    NotCompletelyConverted,
    /// `'X': Numerical result out of range` — the value saturated.
    OutOfRange,
    /// `warning: X: character(s) following character constant have been
    /// ignored` — carries the ignored tail.
    TrailingCharacters(String),
}

/// A converted numeric argument: the best value and the note, if any.
#[derive(Clone, Debug, PartialEq)]
pub struct Converted<T> {
    /// The value the conversion renders.
    pub value: T,
    /// The diagnostic the client reports, if the argument earned one.
    pub note: Option<Note>,
}

impl<T> Converted<T> {
    fn clean(value: T) -> Self {
        Self { value, note: None }
    }

    fn noted(value: T, note: Note) -> Self {
        Self {
            value,
            note: Some(note),
        }
    }

    /// The same note around a converted value.
    fn map<U>(self, convert: impl FnOnce(T) -> U) -> Converted<U> {
        Converted {
            value: convert(self.value),
            note: self.note,
        }
    }
}

/// Read `arg` for a signed integer conversion (`%d`/`%i`).
#[must_use]
pub fn to_signed(arg: &str) -> Converted<i64> {
    if let Some(constant) = character_constant(arg) {
        return constant.map(i64::from);
    }
    let scan = scan_integer(arg);
    let (value, saturated) = if scan.negative {
        match i64::try_from(scan.magnitude) {
            Ok(magnitude) => (-magnitude, false),
            Err(_) if scan.magnitude == min_magnitude() => (i64::MIN, false),
            Err(_) => (i64::MIN, true),
        }
    } else {
        match i64::try_from(scan.magnitude) {
            Ok(value) => (value, false),
            Err(_) => (i64::MAX, true),
        }
    };
    classify(arg, &scan, saturated, value)
}

/// Read `arg` for an unsigned integer conversion (`%u`/`%o`/`%x`/`%X`).
/// A negative spelling wraps, exactly as C's `strtoumax` wraps it.
#[must_use]
pub fn to_unsigned(arg: &str) -> Converted<u64> {
    if let Some(constant) = character_constant(arg) {
        return constant.map(u64::from);
    }
    let scan = scan_integer(arg);
    let (value, saturated) = match u64::try_from(scan.magnitude) {
        Ok(magnitude) if scan.negative => (magnitude.wrapping_neg(), false),
        Ok(magnitude) => (magnitude, false),
        Err(_) => (u64::MAX, true),
    };
    classify(arg, &scan, saturated, value)
}

/// Read `arg` for a floating-point conversion (`%e`/`%f`/`%g`/`%a` and
/// their uppercase forms).
///
/// The value is an IEEE 754 `double`; a finite spelling beyond its range
/// renders as an infinity with no range diagnostic (GNU computes in
/// `long double` — the documented platform divergence, as in `seq`).
#[must_use]
pub fn to_float(arg: &str) -> Converted<f64> {
    if let Some(constant) = character_constant(arg) {
        return constant.map(f64::from);
    }
    match scan_double(arg) {
        None => {
            if arg.is_empty() {
                Converted::clean(0.0)
            } else {
                Converted::noted(0.0, Note::ExpectedNumeric)
            }
        }
        Some((value, consumed)) if consumed == arg.len() => Converted::clean(value),
        Some((value, _)) => Converted::noted(value, Note::NotCompletelyConverted),
    }
}

/// The `u128` magnitude of `i64::MIN`.
fn min_magnitude() -> u128 {
    u128::from(i64::MAX.unsigned_abs()) + 1
}

/// The character-constant reading: a first byte of `'` or `"` converts
/// the following character's code point. `None` when `arg` is not a
/// character constant.
fn character_constant(arg: &str) -> Option<Converted<u32>> {
    if !matches!(arg.as_bytes().first(), Some(b'\'' | b'"')) {
        return None;
    }
    let mut chars = arg[1..].chars();
    let Some(c) = chars.next() else {
        // A bare quote holds no character: no number at all.
        return Some(Converted::noted(0, Note::ExpectedNumeric));
    };
    let rest = chars.as_str();
    Some(if rest.is_empty() {
        Converted::clean(u32::from(c))
    } else {
        Converted::noted(u32::from(c), Note::TrailingCharacters(String::from(rest)))
    })
}

/// One `strtoimax`-style scan: sign, magnitude (clamped into a `u128`
/// wide enough to detect any `u64`/`i64` overflow), digits consumed, and
/// the byte length consumed.
struct IntScan {
    negative: bool,
    magnitude: u128,
    digits: usize,
    consumed: usize,
}

/// Scan the longest leading base-0 integer of `arg`: C-locale
/// whitespace, an optional sign, then `0x` hex, `0` octal, or decimal
/// digits. The magnitude saturates at `u128::MAX / 2` (far beyond any
/// representable result) so overflow classification stays exact.
fn scan_integer(arg: &str) -> IntScan {
    // Saturation cap: far beyond any representable result, so overflow
    // classification stays exact however many digits follow.
    const CAP: u128 = u128::MAX / 2;

    let bytes = arg.as_bytes();
    let mut i = 0;
    while i < bytes.len() && c_isspace(char::from(bytes[i])) {
        i += 1;
    }
    let negative = match bytes.get(i) {
        Some(b'+') => {
            i += 1;
            false
        }
        Some(b'-') => {
            i += 1;
            true
        }
        _ => false,
    };
    let (base, mut i) = if bytes.get(i) == Some(&b'0')
        && matches!(bytes.get(i + 1), Some(b'x' | b'X'))
        && bytes.get(i + 2).is_some_and(u8::is_ascii_hexdigit)
    {
        (16u128, i + 2)
    } else if bytes.get(i) == Some(&b'0') {
        (8, i)
    } else {
        (10, i)
    };
    let mut magnitude: u128 = 0;
    let mut digits = 0_usize;
    while let Some(&b) = bytes.get(i) {
        let digit = match (base, b) {
            (8, b'0'..=b'7') | (10, b'0'..=b'9') => u128::from(b - b'0'),
            (16, _) if b.is_ascii_hexdigit() => u128::from(hex_value(b)),
            _ => break,
        };
        magnitude = magnitude
            .saturating_mul(base)
            .saturating_add(digit)
            .min(CAP);
        digits += 1;
        i += 1;
    }
    IntScan {
        negative,
        magnitude,
        digits,
        consumed: i,
    }
}

/// The value of one hexadecimal digit byte.
fn hex_value(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        _ => b - b'A' + 10,
    }
}

/// Classify a finished integer scan into the GNU diagnostic order:
/// no digits (empty is silent), then out-of-range (C checks `errno`
/// first), then a partially converted value.
fn classify<T>(arg: &str, scan: &IntScan, saturated: bool, value: T) -> Converted<T> {
    if scan.digits == 0 {
        return if arg.is_empty() {
            Converted::clean(value)
        } else {
            Converted::noted(value, Note::ExpectedNumeric)
        };
    }
    if saturated {
        return Converted::noted(value, Note::OutOfRange);
    }
    if scan.consumed < arg.len() {
        return Converted::noted(value, Note::NotCompletelyConverted);
    }
    Converted::clean(value)
}

#[cfg(test)]
mod tests {
    use super::{to_float, to_signed, to_unsigned, Converted, Note};
    use alloc::string::String;

    #[test]
    fn integers_read_in_base_zero() {
        assert_eq!(to_signed("12"), Converted::clean(12));
        assert_eq!(to_signed("+12"), Converted::clean(12));
        assert_eq!(to_signed("-12"), Converted::clean(-12));
        assert_eq!(to_signed("0x1A"), Converted::clean(26));
        assert_eq!(to_signed("-0x10"), Converted::clean(-16));
        assert_eq!(to_signed("010"), Converted::clean(8));
        assert_eq!(to_signed(" \t12"), Converted::clean(12));
        assert_eq!(to_signed("0"), Converted::clean(0));
        assert_eq!(to_unsigned("0xff"), Converted::clean(255));
    }

    #[test]
    fn empty_is_zero_and_silent() {
        assert_eq!(to_signed(""), Converted::clean(0));
        assert_eq!(to_unsigned(""), Converted::clean(0));
        assert_eq!(to_float(""), Converted::clean(0.0));
    }

    #[test]
    fn non_numbers_are_diagnosed() {
        assert_eq!(to_signed("abc"), Converted::noted(0, Note::ExpectedNumeric));
        assert_eq!(to_signed(" "), Converted::noted(0, Note::ExpectedNumeric));
        assert_eq!(
            to_float("abc"),
            Converted::noted(0.0, Note::ExpectedNumeric)
        );
    }

    #[test]
    fn partial_conversions_are_diagnosed() {
        assert_eq!(
            to_signed("12abc"),
            Converted::noted(12, Note::NotCompletelyConverted)
        );
        assert_eq!(
            to_signed("12 "),
            Converted::noted(12, Note::NotCompletelyConverted)
        );
        assert_eq!(
            to_signed("08"),
            Converted::noted(0, Note::NotCompletelyConverted),
            "an octal scan stops at 8"
        );
        assert_eq!(
            to_float("1.5x"),
            Converted::noted(1.5, Note::NotCompletelyConverted)
        );
    }

    #[test]
    fn out_of_range_saturates() {
        assert_eq!(
            to_signed("99999999999999999999999"),
            Converted::noted(i64::MAX, Note::OutOfRange)
        );
        assert_eq!(
            to_signed("-99999999999999999999999"),
            Converted::noted(i64::MIN, Note::OutOfRange)
        );
        assert_eq!(
            to_unsigned("99999999999999999999999"),
            Converted::noted(u64::MAX, Note::OutOfRange)
        );
        assert_eq!(
            to_signed("-9223372036854775808"),
            Converted::clean(i64::MIN),
            "the exact minimum is in range"
        );
        assert_eq!(
            to_signed("-9223372036854775809"),
            Converted::noted(i64::MIN, Note::OutOfRange)
        );
    }

    #[test]
    fn negatives_wrap_for_unsigned_conversions() {
        assert_eq!(to_unsigned("-1"), Converted::clean(u64::MAX));
        assert_eq!(to_unsigned("-2"), Converted::clean(u64::MAX - 1));
    }

    #[test]
    fn character_constants_convert_the_next_character() {
        assert_eq!(to_signed("'A"), Converted::clean(65));
        assert_eq!(to_signed("\"A"), Converted::clean(65));
        assert_eq!(to_unsigned("'A"), Converted::clean(65));
        assert_eq!(to_float("'A"), Converted::clean(65.0));
        assert_eq!(to_signed("'é"), Converted::clean(233), "Unicode scalar");
        assert_eq!(
            to_signed("'ABC"),
            Converted::noted(65, Note::TrailingCharacters(String::from("BC")))
        );
        assert_eq!(to_signed("'"), Converted::noted(0, Note::ExpectedNumeric));
    }

    #[test]
    fn floats_read_the_c_grammar() {
        assert_eq!(to_float("1.5"), Converted::clean(1.5));
        assert_eq!(to_float("0x1p-1"), Converted::clean(0.5));
        assert_eq!(to_float("inf"), Converted::clean(f64::INFINITY));
        assert_eq!(to_float("-inf"), Converted::clean(f64::NEG_INFINITY));
        assert!(to_float("nan").value.is_nan());
        assert!(to_float("nan").note.is_none());
        // Beyond double's range: the documented long-double divergence —
        // the value overflows to infinity with no range diagnostic.
        assert_eq!(to_float("1e999"), Converted::clean(f64::INFINITY));
    }
}
