//! Operand scanning: the GNU `seq` `scan_arg`, faithfully.
//!
//! An operand is parsed the way C's `strtold` reads it in the C locale —
//! optional leading whitespace, an optional sign, then a decimal number
//! (with optional fraction and `e`/`E` exponent), a hexadecimal float
//! (`0x…[.…][p±…]`), or `inf`/`infinity`/`nan` — and the whole token must
//! be consumed. Alongside the value, the scan records the *print width*
//! and *decimal precision* of the input spelling, which the default
//! output format and `-w` are computed from.

use alloc::string::String;

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
pub fn parse_number(text: &str) -> Option<f64> {
    let rest = text.trim_start_matches(c_isspace);
    let (negative, rest) = match rest.as_bytes().first() {
        Some(b'+') => (false, &rest[1..]),
        Some(b'-') => (true, &rest[1..]),
        _ => (false, rest),
    };
    let magnitude = parse_unsigned(rest)?;
    Some(if negative { -magnitude } else { magnitude })
}

/// C's `isspace` in the C locale.
fn c_isspace(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c')
}

/// Parse an unsigned number body: hex float, `inf`/`infinity`, `nan`, or
/// a decimal float, consuming the whole string.
fn parse_unsigned(rest: &str) -> Option<f64> {
    // The one permitted sign was already split off; a second (`--5`,
    // `+-5`) is not a number.
    if matches!(rest.as_bytes().first(), Some(b'+' | b'-')) {
        return None;
    }
    let lower = rest.to_ascii_lowercase();
    if lower == "inf" || lower == "infinity" {
        return Some(f64::INFINITY);
    }
    // `nan` and `nan(…)` parse to a NaN; the caller rejects it with its
    // own diagnostic, exactly as GNU seq does.
    if lower == "nan" || (lower.starts_with("nan(") && lower.ends_with(')')) {
        return Some(f64::NAN);
    }
    if let Some(hex) = lower.strip_prefix("0x") {
        return parse_hex_float(hex);
    }
    // The decimal grammar `strtold` accepts is what Rust's own f64 parser
    // accepts, minus the spellings already handled above; Rust would also
    // accept `inf`/`nan` with signs, but the sign was already split off.
    if lower.is_empty()
        || !lower
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'.' | b'e' | b'+' | b'-'))
    {
        return None;
    }
    rest.parse::<f64>().ok()
}

/// The number of significant hex digits collected before rounding; see
/// the sticky-bit note in [`parse_hex_float`].
const WINDOW: usize = 28;

/// Parse the body of a hexadecimal float (after `0x`, already lowercased):
/// `hexdigits[.hexdigits][p[±]decimal]`, needing at least one hex digit.
fn parse_hex_float(body: &str) -> Option<f64> {
    let (mantissa, exponent) = match body.split_once('p') {
        Some((mantissa, exp)) => (mantissa, Some(exp)),
        None => (body, None),
    };
    let (int_part, frac_part) = match mantissa.split_once('.') {
        Some((int_part, frac_part)) => (int_part, frac_part),
        None => (mantissa, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    if !int_part.bytes().all(|b| b.is_ascii_hexdigit())
        || !frac_part.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }

    let exponent: i64 = match exponent {
        Some(exp) => {
            let exp = exp.strip_prefix('+').unwrap_or(exp);
            let negative = exp.starts_with('-');
            let digits = exp.strip_prefix('-').unwrap_or(exp);
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            // Saturate an over-long exponent: the value is certainly 0 or
            // infinity either way.
            let magnitude: i64 = digits.parse().unwrap_or(i64::MAX / 2);
            if negative {
                -magnitude
            } else {
                magnitude
            }
        }
        None => 0,
    };

    // Collect the significant hex digits into one integer so the binary
    // value is rounded exactly once. 28 hex digits (112 bits) comfortably
    // exceed an f64's 53-bit significand; digits beyond that window only
    // matter as a sticky "something non-zero follows" bit.
    let mut mantissa: u128 = 0;
    let mut taken = 0_usize;
    let mut frac_digits_taken = 0_i64;
    let mut sticky = false;
    let mut leading = true;
    for (from_fraction, b) in int_part
        .bytes()
        .map(|b| (false, b))
        .chain(frac_part.bytes().map(|b| (true, b)))
    {
        if leading && b == b'0' {
            // Skip leading zeros; fractional ones still shift the scale.
            if from_fraction {
                frac_digits_taken += 1;
            }
            continue;
        }
        leading = false;
        if taken < WINDOW {
            mantissa = mantissa * 16 + u128::from(hex_digit(b));
            taken += 1;
            if from_fraction {
                frac_digits_taken += 1;
            }
        } else {
            sticky = sticky || b != b'0';
            if !from_fraction {
                frac_digits_taken -= 1; // an untaken integer digit scales up
            }
        }
    }
    if sticky {
        // Make the mantissa "just above" its truncation so a half-way
        // rounding decision cannot resolve to even incorrectly. The window
        // guarantees bit 0 is far below the f64 rounding point.
        mantissa |= 1;
    }
    Some(compose_f64(
        mantissa,
        exponent.saturating_sub(4 * frac_digits_taken),
    ))
}

/// The value of one hexadecimal digit.
fn hex_digit(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        _ => b - b'A' + 10,
    }
}

/// `mantissa * 2^exp2`, correctly rounded to nearest-even in one step —
/// including into the subnormal range — so a hex-float parse never
/// double-rounds.
fn compose_f64(mantissa: u128, exp2: i64) -> f64 {
    if mantissa == 0 {
        return 0.0;
    }
    // Normalise: value = m * 2^e with the top bit of m at position 127.
    let shift = mantissa.leading_zeros();
    let m = mantissa << shift;
    let e = exp2 + i64::from(127 - shift); // value = (m / 2^127) * 2^(e+... )

    // value = m * 2^(e - 127); the binary exponent of the value is e.
    if e > 1023 {
        return f64::INFINITY;
    }
    // How many of m's top bits become the significand: 53 for normals,
    // fewer as the value sinks below the smallest normal exponent -1022.
    let significand_bits: i64 = if e >= -1022 {
        53
    } else {
        // e == -1023 keeps 52 bits, and so on; below that, zero.
        53 - (-1022 - e)
    };
    if significand_bits <= 0 {
        // Too small even for the least subnormal; the largest such value
        // is half its ulp, which rounds to zero (ties-to-even at exactly
        // half only when the kept part is zero — still zero).
        if significand_bits == 0 {
            // Exactly the half-ulp boundary of the least subnormal: round
            // to even (zero) unless anything follows the lead bit.
            let rest = m << 1;
            return if rest != 0 { f64::from_bits(1) } else { 0.0 };
        }
        return 0.0;
    }
    // significand_bits is 1..=53 here (the <= 0 cases returned above).
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let keep = significand_bits as u32;
    // At most the top 53 bits of m survive the shift, so u64 holds them.
    #[allow(clippy::cast_possible_truncation)]
    let kept = (m >> (128 - keep)) as u64;
    let round_bit = (m >> (127 - keep)) & 1;
    let rest = m << (keep + 1);
    let mut significand = kept;
    if round_bit == 1 && (rest != 0 || kept & 1 == 1) {
        significand += 1;
    }
    if keep == 53 {
        // Normal range. A carry out of the 53 bits renormalises upward,
        // which can overflow to infinity.
        let mut exponent = e;
        if significand == 1 << 53 {
            significand = 1 << 52;
            exponent += 1;
            if exponent > 1023 {
                return f64::INFINITY;
            }
        }
        // exponent is -1022..=1023 here, so the biased field is 1..=2046.
        #[allow(clippy::cast_sign_loss)]
        let bits = ((exponent + 1023) as u64) << 52 | (significand & ((1 << 52) - 1));
        f64::from_bits(bits)
    } else {
        // Subnormal: exponent field 0, the significand stored as-is. A
        // carry up to `1 << keep` (at most `1 << 52`) lands on the exact
        // bit pattern of the next-larger value — including the promotion
        // from the largest subnormal to the least normal — by design of
        // the IEEE 754 encoding.
        f64::from_bits(significand)
    }
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
