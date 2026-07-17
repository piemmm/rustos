//! C-locale `strtod(3)` scanning.
//!
//! TAIRiX is Rust-only, so the coreutils-compatible tools that promise C
//! number parsing (`seq`'s operand scan, `printf`'s numeric arguments)
//! cannot call a C library — they scan through this one engine instead,
//! so C's grammar (optional whitespace and sign, decimal floats,
//! hexadecimal floats, `inf`/`infinity`/`nan`) and its exact one-step
//! rounding exist in exactly one place.
//!
//! [`scan_double`] has `strtod`'s longest-prefix (`endptr`) shape: it
//! reads the longest leading subject sequence that forms a number and
//! reports how many bytes it consumed, so a whole-token caller (`seq`)
//! demands full consumption while a diagnose-and-continue caller
//! (`printf`) inspects the remainder. Everything is total and panic-free.

/// C's `isspace` in the C locale.
#[must_use]
pub fn c_isspace(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c')
}

/// Scan the longest leading `strtod` subject sequence of `text` — leading
/// C-locale whitespace, an optional sign, then a decimal float, a
/// hexadecimal float (`0x…[.…][p±…]`), `inf`/`infinity`, or
/// `nan`/`nan(n-char-seq)`, all case-insensitive — returning the value
/// and the number of bytes consumed (whitespace and sign included, C's
/// `endptr` measure). `None` when no subject sequence starts the text.
#[must_use]
pub fn scan_double(text: &str) -> Option<(f64, usize)> {
    let bytes = text.as_bytes();
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
    let (magnitude, body_len) = scan_magnitude(&bytes[i..], &text[i..])?;
    Some((if negative { -magnitude } else { magnitude }, i + body_len))
}

/// Scan the unsigned number body at the start of `bytes` (`body` is the
/// same tail as `&str`), returning the value and the bytes consumed.
fn scan_magnitude(bytes: &[u8], body: &str) -> Option<(f64, usize)> {
    if starts_with_ignore_case(bytes, b"infinity") {
        return Some((f64::INFINITY, 8));
    }
    if starts_with_ignore_case(bytes, b"inf") {
        return Some((f64::INFINITY, 3));
    }
    if starts_with_ignore_case(bytes, b"nan") {
        // `nan(n-char-seq)`: alphanumerics and underscores up to a `)`;
        // anything else keeps the plain three-byte `nan`.
        if bytes.get(3) == Some(&b'(') {
            let mut j = 4;
            while let Some(&b) = bytes.get(j) {
                if b == b')' {
                    return Some((f64::NAN, j + 1));
                }
                if !(b.is_ascii_alphanumeric() || b == b'_') {
                    break;
                }
                j += 1;
            }
        }
        return Some((f64::NAN, 3));
    }
    if (starts_with_ignore_case(bytes, b"0x")) && bytes.get(2).is_some() {
        if let Some(parsed) = scan_hex_float(&bytes[2..]) {
            let (value, len) = parsed;
            return Some((value, 2 + len));
        }
        // `0x` with no hex digits: the subject is the `0` alone.
        return Some((0.0, 1));
    }
    scan_decimal(bytes, body)
}

/// True when `bytes` starts with `pattern`, ASCII case-insensitively.
fn starts_with_ignore_case(bytes: &[u8], pattern: &[u8]) -> bool {
    bytes.len() >= pattern.len()
        && bytes
            .iter()
            .zip(pattern)
            .all(|(b, p)| b.eq_ignore_ascii_case(p))
}

/// Scan a decimal float body — digits, an optional point and more digits
/// (at least one digit overall), and an optional `e`/`E` exponent that is
/// dropped again unless at least one digit follows its sign — and parse
/// the consumed slice.
fn scan_decimal(bytes: &[u8], body: &str) -> Option<(f64, usize)> {
    let mut i = 0;
    let mut digits = 0_usize;
    while bytes.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
        digits += 1;
    }
    if bytes.get(i) == Some(&b'.') {
        i += 1;
        while bytes.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return None;
    }
    if matches!(bytes.get(i), Some(b'e' | b'E')) {
        let mut j = i + 1;
        if matches!(bytes.get(j), Some(b'+' | b'-')) {
            j += 1;
        }
        if bytes.get(j).is_some_and(u8::is_ascii_digit) {
            while bytes.get(j).is_some_and(u8::is_ascii_digit) {
                j += 1;
            }
            i = j;
        }
    }
    // The consumed slice is exactly Rust's f64 grammar (digits, point,
    // exponent), which parses with correct rounding; fail closed on the
    // unreachable mismatch rather than guessing.
    body.get(..i)?.parse::<f64>().ok().map(|value| (value, i))
}

/// The number of significant hex digits collected before rounding; see
/// the sticky-bit note in [`scan_hex_float`].
const WINDOW: usize = 28;

/// Scan a hexadecimal-float body (after the `0x`): hex digits with an
/// optional point (at least one digit overall) and an optional `p`/`P`
/// exponent that is dropped again unless at least one decimal digit
/// follows its sign. Returns the value and the bytes consumed after the
/// `0x`.
fn scan_hex_float(bytes: &[u8]) -> Option<(f64, usize)> {
    // Collect the significant hex digits into one integer so the binary
    // value is rounded exactly once. 28 hex digits (112 bits) comfortably
    // exceed an f64's 53-bit significand; digits beyond that window only
    // matter as a sticky "something non-zero follows" bit.
    let mut mantissa: u128 = 0;
    let mut taken = 0_usize;
    let mut frac_digits_taken = 0_i64;
    let mut sticky = false;
    let mut leading = true;
    let mut digits = 0_usize;
    let mut i = 0;
    let mut in_fraction = false;
    loop {
        match bytes.get(i) {
            Some(&b) if b.is_ascii_hexdigit() => {
                digits += 1;
                if leading && b == b'0' {
                    // Skip leading zeros; fractional ones still shift the
                    // scale.
                    if in_fraction {
                        frac_digits_taken += 1;
                    }
                } else {
                    leading = false;
                    if taken < WINDOW {
                        mantissa = mantissa * 16 + u128::from(hex_digit(b));
                        taken += 1;
                        if in_fraction {
                            frac_digits_taken += 1;
                        }
                    } else {
                        sticky = sticky || b != b'0';
                        if !in_fraction {
                            // An untaken integer digit scales up.
                            frac_digits_taken -= 1;
                        }
                    }
                }
                i += 1;
            }
            Some(&b'.') if !in_fraction => {
                in_fraction = true;
                i += 1;
            }
            _ => break,
        }
    }
    if digits == 0 {
        return None;
    }

    let mut exponent: i64 = 0;
    if matches!(bytes.get(i), Some(b'p' | b'P')) {
        let mut j = i + 1;
        let negative = match bytes.get(j) {
            Some(b'+') => {
                j += 1;
                false
            }
            Some(b'-') => {
                j += 1;
                true
            }
            _ => false,
        };
        if bytes.get(j).is_some_and(u8::is_ascii_digit) {
            // Saturate an over-long exponent: the value is certainly 0 or
            // infinity either way.
            let mut magnitude: i64 = 0;
            while let Some(&b) = bytes.get(j) {
                if !b.is_ascii_digit() {
                    break;
                }
                magnitude = magnitude
                    .saturating_mul(10)
                    .saturating_add(i64::from(b - b'0'));
                j += 1;
            }
            exponent = if negative { -magnitude } else { magnitude };
            i = j;
        }
    }

    if sticky {
        // Make the mantissa "just above" its truncation so a half-way
        // rounding decision cannot resolve to even incorrectly. The window
        // guarantees bit 0 is far below the f64 rounding point.
        mantissa |= 1;
    }
    Some((
        compose_f64(mantissa, exponent.saturating_sub(4 * frac_digits_taken)),
        i,
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

#[cfg(test)]
mod tests {
    use super::scan_double;

    /// A scan that must consume the whole text.
    fn whole(text: &str) -> Option<f64> {
        scan_double(text).and_then(|(value, len)| (len == text.len()).then_some(value))
    }

    #[test]
    fn decimal_spellings_scan() {
        assert_eq!(whole("5"), Some(5.0));
        assert_eq!(whole("-0.5"), Some(-0.5));
        assert_eq!(whole("+.25"), Some(0.25));
        assert_eq!(whole("1."), Some(1.0));
        assert_eq!(whole("1e3"), Some(1000.0));
        assert_eq!(whole("1.5E-2"), Some(0.015));
        assert_eq!(whole("  \t7"), Some(7.0));
    }

    #[test]
    fn non_numbers_scan_nothing() {
        assert_eq!(scan_double(""), None);
        assert_eq!(scan_double("x"), None);
        assert_eq!(scan_double("--5"), None);
        assert_eq!(scan_double("."), None);
        assert_eq!(scan_double("+"), None);
        assert_eq!(scan_double("e5"), None);
    }

    #[test]
    fn prefixes_scan_with_endptr_semantics() {
        assert_eq!(scan_double("12abc"), Some((12.0, 2)));
        assert_eq!(scan_double(" 12 "), Some((12.0, 3)));
        assert_eq!(scan_double("1e"), Some((1.0, 1)), "bare exponent dropped");
        assert_eq!(scan_double("1e+"), Some((1.0, 1)));
        assert_eq!(scan_double("0x"), Some((0.0, 1)), "0x alone is the 0");
        assert_eq!(scan_double("0xp3"), Some((0.0, 1)));
        assert_eq!(
            scan_double("0x1p"),
            Some((1.0, 3)),
            "bare binary exponent dropped"
        );
        assert_eq!(scan_double("infx"), Some((f64::INFINITY, 3)));
        assert_eq!(scan_double("-infinity"), Some((f64::NEG_INFINITY, 9)));
        let (nan, len) = scan_double("nan(a b)").expect("nan scans");
        assert!(nan.is_nan());
        assert_eq!(len, 3, "an invalid n-char-seq keeps the bare nan");
        let (nan, len) = scan_double("nan(abc_1)x").expect("nan scans");
        assert!(nan.is_nan());
        assert_eq!(len, 10);
    }

    #[test]
    fn infinities_and_nans_scan() {
        assert_eq!(whole("inf"), Some(f64::INFINITY));
        assert_eq!(whole("-INF"), Some(f64::NEG_INFINITY));
        assert_eq!(whole("Infinity"), Some(f64::INFINITY));
        assert!(whole("nan").is_some_and(f64::is_nan));
        assert!(whole("NaN(0x1)").is_some_and(f64::is_nan));
    }

    #[test]
    fn hex_floats_scan_exactly() {
        assert_eq!(whole("0x1p-1"), Some(0.5));
        assert_eq!(whole("0X1.8P1"), Some(3.0));
        assert_eq!(whole("0x.8"), Some(0.5));
        assert_eq!(whole("0x10"), Some(16.0));
        assert_eq!(whole("-0x1p3"), Some(-8.0));
        assert_eq!(whole("0x1.fffffffffffffp+1023"), Some(f64::MAX));
        assert_eq!(whole("0x1p-1074"), Some(f64::from_bits(1)));
        assert_eq!(whole("0x1p+1024"), Some(f64::INFINITY));
        assert_eq!(whole("0x0p0"), Some(0.0));
    }

    #[test]
    fn hex_float_rounding_is_single_step() {
        // 53 significant bits plus a trailing 1: rounds half-to-even up.
        assert_eq!(
            whole("0x1.00000000000008p0"),
            Some(1.0),
            "half-way ties to the even significand"
        );
        assert_eq!(
            whole("0x1.000000000000080000000000000001p0"),
            Some(f64::from_bits(0x3FF0_0000_0000_0001)),
            "a sticky digit past the window breaks the tie upward"
        );
        assert_eq!(
            whole("0x1.00000000000018p0"),
            Some(f64::from_bits(0x3FF0_0000_0000_0002)),
            "half-way above an odd significand rounds up"
        );
    }
}
