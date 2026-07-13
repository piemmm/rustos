//! The shared refresh-delay option grammar of the full-screen viewers.
//!
//! GNU `top`'s `-d, --delay secs.tenths` spelling — seconds with an optional
//! fraction, of which only the first fractional digit (tenths) is kept — is
//! implemented by every RustOS full-screen monitor (`top`, `sysmon`), and
//! its parsed value directly parameterises the viewer's
//! [`Screen`](crate::Screen) input timeout. The grammar therefore lives here
//! once; each tool keeps its own usage banner and error enum.
//!
//! GNU `top` accepts a zero delay and spins as fast as it can; RustOS never
//! busy-loops, so a parsed zero is clamped up to [`MIN_DELAY_TENTHS`] — a
//! deliberate, documented divergence in each tool's Help.

/// The smallest accepted refresh delay, in tenths of a second.
pub const MIN_DELAY_TENTHS: u32 = 1;

/// Parse a `secs[.tenths…]` delay value into whole tenths of a second,
/// keeping only the first fractional digit (GNU `top` keeps tenths) and
/// clamping a zero up to [`MIN_DELAY_TENTHS`] so a viewer never busy-loops.
///
/// Fails closed: an empty value, a non-digit, more than one decimal point,
/// or a tenths counter that overflows is `None`, never a guessed interval.
#[must_use]
pub fn parse_delay_tenths(value: &str) -> Option<u32> {
    let (whole, fraction) = match value.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (value, ""),
    };
    if whole.is_empty() && fraction.is_empty() {
        return None;
    }
    if !whole.bytes().all(|b| b.is_ascii_digit()) || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let seconds: u32 = if whole.is_empty() {
        0
    } else {
        whole.parse().ok()?
    };
    let tenth = fraction
        .bytes()
        .next()
        .map_or(0, |digit| u32::from(digit - b'0'));
    let tenths = seconds.checked_mul(10)?.checked_add(tenth)?;
    Some(tenths.max(MIN_DELAY_TENTHS))
}

#[cfg(test)]
mod tests {
    use super::{parse_delay_tenths, MIN_DELAY_TENTHS};

    #[test]
    fn whole_seconds_and_tenths_parse() {
        assert_eq!(parse_delay_tenths("3"), Some(30));
        assert_eq!(parse_delay_tenths("0.5"), Some(5));
        assert_eq!(parse_delay_tenths("2.75"), Some(27)); // Only tenths kept.
        assert_eq!(parse_delay_tenths(".5"), Some(5));
        assert_eq!(parse_delay_tenths("1."), Some(10));
    }

    #[test]
    fn zero_clamps_to_the_minimum() {
        assert_eq!(parse_delay_tenths("0"), Some(MIN_DELAY_TENTHS));
        assert_eq!(parse_delay_tenths("0.0"), Some(MIN_DELAY_TENTHS));
    }

    #[test]
    fn malformed_values_fail_closed() {
        assert_eq!(parse_delay_tenths(""), None);
        assert_eq!(parse_delay_tenths("."), None);
        assert_eq!(parse_delay_tenths("abc"), None);
        assert_eq!(parse_delay_tenths("1.2.3"), None);
        assert_eq!(parse_delay_tenths("-1"), None);
        assert_eq!(parse_delay_tenths("999999999999"), None);
    }
}
