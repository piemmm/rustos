//! Operand scanning: the GNU `seq` `scan_arg`, faithfully.
//!
//! An operand is parsed the way C's `strtold` reads it in the C locale —
//! through the one shared scanner (`rustos_util::cnum`, also consumed by
//! `printf`) — and the whole token must be consumed. Alongside the value,
//! the scan records the *print width* and *decimal precision* of the
//! input spelling, which the default output format and `-w` are computed
//! from; that layout derivation is `seq`'s own and lives here.

use alloc::string::String;

use rustos_util::cnum::{c_isspace, scan_double};

use crate::error::SeqError;

/// The sentinel precision meaning "not easily expressed as a fixed-point
/// decimal" (the GNU `INT_MAX` sentinel).
pub const NOT_FIXED_POINT: i64 = i32::MAX as i64;

/// A scanned command-line operand: its value and the layout of its input
/// spelling (GNU `struct operand`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Operand {
    /// The numeric value.
    pub value: f64,
    /// Its print width, were it printed in a form similar to its input
    /// form (`-.1` counts as `-0.1`; `1.` counts as `1`).
    pub width: i64,
    /// Digits after the decimal point, or [`NOT_FIXED_POINT`].
    pub precision: i64,
}

impl Operand {
    /// The implicit `1` used when FIRST or INCREMENT is omitted.
    pub const ONE: Self = Self {
        value: 1.0,
        width: 1,
        precision: 0,
    };
}

/// Parse the full token `text` as C's `strtold` would in the C locale,
/// returning `None` when the token is not entirely a number.
#[must_use]
pub fn parse_number(text: &str) -> Option<f64> {
    scan_double(text).and_then(|(value, len)| (len == text.len()).then_some(value))
}

/// A command-line token's length as an `i64` (a token cannot approach
/// `i64::MAX` bytes; the clamp keeps the conversion total).
fn len_i64(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// Scan one command-line operand (GNU `scan_arg`): parse its value and
/// derive the width/precision of its spelling.
///
/// # Errors
///
/// [`SeqError::InvalidNumber`] when the token is not a number, and
/// [`SeqError::NotANumber`] when it parses to a NaN.
pub fn scan_arg(arg: &str) -> Result<Operand, SeqError> {
    let Some(value) = parse_number(arg) else {
        return Err(SeqError::InvalidNumber(String::from(arg)));
    };
    if value.is_nan() {
        return Err(SeqError::NotANumber(String::from(arg)));
    }

    // Spaces and `+` are not output, so they do not count toward width.
    let arg = arg.trim_start_matches(|c: char| c_isspace(c) || c == '+');

    let mut width: i64 = 0;
    let mut precision: i64 = NOT_FIXED_POINT;

    // Integers (no decimal point, not a hex float) print with no
    // fractional digits. GNU checks only a lowercase `p`, so an uppercase
    // hex-float exponent (`0X1P3`) counts as an integer there; mirrored.
    let decimal_point = arg.find('.');
    if decimal_point.is_none() && !arg.contains('p') {
        precision = 0;
    }

    // Width and precision are only derived for decimal spellings of
    // finite values.
    if !arg.contains(['x', 'X']) && value.is_finite() {
        let mut fraction_len: i64 = 0;
        width = len_i64(arg.len());

        if let Some(point) = decimal_point {
            let after = &arg[point + 1..];
            fraction_len = len_i64(after.find(['e', 'E']).unwrap_or(after.len()));
            precision = fraction_len;
            width += if fraction_len == 0 {
                -1 // `#.` prints as `#`
            } else {
                let leading_digit = point > 0 && arg.as_bytes()[point - 1].is_ascii_digit();
                i64::from(!leading_digit) // `.#`/`-.#` print as `0.#`/`-0.#`
            };
        }

        if let Some(e) = arg.find(['e', 'E']) {
            let exp_text = &arg[e + 1..];
            // Saturate an over-long exponent the way `strtol` clamps to
            // LONG_MAX; the value is infinite or zero either way, and only
            // the sign of the adjustment matters.
            let mut exponent: i64 = exp_text.parse().unwrap_or(if exp_text.starts_with('-') {
                i64::MIN / 2
            } else {
                i64::MAX / 2
            });
            precision += if exponent < 0 {
                -exponent
            } else {
                -precision.min(exponent)
            };
            // The `e…` text is not output, so it leaves the width.
            width -= len_i64(arg.len() - e);
            if exponent < 0 {
                if decimal_point.is_some() {
                    if e > 0 && decimal_point == Some(e - 1) {
                        width += 1; // undo the `#.` -> `#` adjustment above
                    }
                } else {
                    width += 1;
                }
                exponent = -exponent;
            } else {
                if decimal_point.is_some() && precision == 0 && fraction_len != 0 {
                    width -= 1; // no space needed for the `.`
                }
                exponent -= fraction_len.min(exponent);
            }
            width += exponent;
        }
    }

    Ok(Operand {
        value,
        width,
        precision,
    })
}

#[cfg(test)]
mod tests {
    use super::{parse_number, scan_arg, Operand, NOT_FIXED_POINT};
    use crate::error::SeqError;
    use alloc::string::String;

    fn layout(arg: &str) -> (i64, i64) {
        let operand = scan_arg(arg).expect("scans");
        (operand.width, operand.precision)
    }

    #[test]
    fn decimal_spellings_parse() {
        assert_eq!(parse_number("1"), Some(1.0));
        assert_eq!(parse_number("-2.5"), Some(-2.5));
        assert_eq!(parse_number("+2.5"), Some(2.5));
        assert_eq!(parse_number(".5"), Some(0.5));
        assert_eq!(parse_number("5."), Some(5.0));
        assert_eq!(parse_number("1e3"), Some(1000.0));
        assert_eq!(parse_number("1E3"), Some(1000.0));
        assert_eq!(parse_number("1.5e-2"), Some(0.015));
        assert_eq!(parse_number("  7"), Some(7.0), "leading C whitespace");
    }

    #[test]
    fn non_numbers_are_rejected() {
        for text in [
            "", " ", "abc", "1x", "1 ", "--5", "1e", "0x", "0xp3", "nanx", "1_000",
        ] {
            assert_eq!(parse_number(text), None, "{text:?}");
        }
    }

    #[test]
    fn infinities_and_nans_parse() {
        assert_eq!(parse_number("inf"), Some(f64::INFINITY));
        assert_eq!(parse_number("INF"), Some(f64::INFINITY));
        assert_eq!(parse_number("Infinity"), Some(f64::INFINITY));
        assert_eq!(parse_number("-inf"), Some(f64::NEG_INFINITY));
        assert!(parse_number("nan").is_some_and(f64::is_nan));
        assert!(parse_number("NaN").is_some_and(f64::is_nan));
        assert!(parse_number("nan(0x1)").is_some_and(f64::is_nan));
    }

    #[test]
    fn hex_floats_parse_exactly() {
        assert_eq!(parse_number("0x8"), Some(8.0));
        assert_eq!(parse_number("0X1P3"), Some(8.0));
        assert_eq!(parse_number("0x1.8p1"), Some(3.0));
        assert_eq!(parse_number("0x.8p1"), Some(1.0));
        assert_eq!(parse_number("0x8."), Some(8.0));
        assert_eq!(parse_number("-0x10p-1"), Some(-8.0));
        assert_eq!(parse_number("0x0p0"), Some(0.0));
        assert_eq!(
            parse_number("0x1.fffffffffffffp1023"),
            Some(f64::MAX),
            "largest finite double"
        );
    }

    #[test]
    fn hex_float_rounding_is_single_step() {
        // Ties round to even: 53rd bit set, nothing after.
        assert_eq!(
            parse_number("0x1.00000000000008p0"),
            Some(1.0),
            "half-ulp tie rounds to even"
        );
        // Anything beyond the tie rounds up.
        assert_eq!(
            parse_number("0x1.000000000000080000000000000001p0"),
            Some(f64::from_bits(0x3FF0_0000_0000_0001)),
            "sticky digits break the tie upward"
        );
        // Subnormal boundaries.
        assert_eq!(parse_number("0x1p-1074"), Some(f64::from_bits(1)));
        assert_eq!(
            parse_number("0x1p-1075"),
            Some(0.0),
            "half-ulp of least subnormal ties to zero"
        );
        assert_eq!(
            parse_number("0x1.1p-1075"),
            Some(f64::from_bits(1)),
            "just above the half-ulp rounds up"
        );
        assert_eq!(
            parse_number("0x1.8p-1074"),
            Some(f64::from_bits(2)),
            "subnormal tie rounds to even"
        );
        // The largest subnormal plus half its ulp promotes to the least
        // normal through the rounding carry.
        assert_eq!(
            parse_number("0x0.fffffffffffff8p-1022"),
            Some(f64::from_bits(1 << 52)),
            "carry promotes to the least normal"
        );
        // Overflow past the largest finite double.
        assert_eq!(parse_number("0x1p1024"), Some(f64::INFINITY));
        assert_eq!(
            parse_number("0x1.fffffffffffff8p1023"),
            Some(f64::INFINITY),
            "tie above MAX renormalises to infinity"
        );
    }

    #[test]
    fn integer_layouts() {
        assert_eq!(layout("1"), (1, 0));
        assert_eq!(layout("10"), (2, 0));
        assert_eq!(layout("-4"), (2, 0));
        assert_eq!(layout("+4"), (1, 0), "the sign is not output");
        assert_eq!(layout("007"), (3, 0), "leading zeros keep their width");
    }

    #[test]
    fn fixed_point_layouts() {
        assert_eq!(layout("1.5"), (3, 1));
        assert_eq!(layout("-.1"), (4, 1), "-.1 prints as -0.1");
        assert_eq!(layout(".1"), (3, 1), ".1 prints as 0.1");
        assert_eq!(layout("1."), (1, 0), "1. prints as 1");
        assert_eq!(layout("2.00"), (4, 2));
    }

    #[test]
    fn exponent_layouts() {
        // Hand-derived from the GNU scan_arg width algebra.
        assert_eq!(layout("1e3"), (4, 0), "1000");
        assert_eq!(layout("1.5e2"), (3, 0), "150");
        assert_eq!(layout("1e-2"), (4, 2), "0.01");
        assert_eq!(layout("5.e-3"), (5, 3), "0.005");
        assert_eq!(layout("1.55e2"), (3, 0), "155");
    }

    #[test]
    fn hex_and_inf_layouts() {
        // Hex spellings derive no width; only the integer/precision rule
        // applies (and GNU checks only a lowercase `p`).
        assert_eq!(layout("0x8"), (0, 0));
        assert_eq!(layout("0x1.8p1"), (0, NOT_FIXED_POINT));
        assert_eq!(layout("0x8p2"), (0, NOT_FIXED_POINT));
        assert_eq!(layout("0X1P3"), (0, 0), "the GNU lowercase-p quirk");
        assert_eq!(layout("inf"), (0, 0));
    }

    #[test]
    fn scan_errors_mirror_gnu() {
        assert_eq!(
            scan_arg("abc"),
            Err(SeqError::InvalidNumber(String::from("abc")))
        );
        assert_eq!(
            scan_arg("nan"),
            Err(SeqError::NotANumber(String::from("nan")))
        );
        assert_eq!(scan_arg(""), Err(SeqError::InvalidNumber(String::new())));
    }

    #[test]
    fn implicit_one_layout() {
        assert_eq!(Operand::ONE.value.to_bits(), 1.0_f64.to_bits());
        assert_eq!((Operand::ONE.width, Operand::ONE.precision), (1, 0));
    }
}
