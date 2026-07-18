//! GNU coreutils count-with-multiplier-suffix parsing, saturating at
//! [`u64::MAX`].
//!
//! The `head` and `tail` command apps (`plans/APPS.md` §12.1 Stage C) — and
//! the future `split` — accept the same `-c`/`-n` count grammar: decimal
//! digits followed by an optional multiplier suffix from the GNU alphabet
//! (`b` = 512; `k`/`K`/`m`/`M`/`G`/`T`/`P`/`E`/`Z`/`Y`/`R`/`Q` = powers of
//! 1024, or of 1000 with a trailing `B`, or of 1024 with a trailing `iB`).
//! The grammar therefore lives here once rather than being copied into each
//! tool. The tool-specific *sign* handling (`head`'s leading `-` "elide",
//! `tail`'s `+` "from start") stays in each tool; only the unsigned count
//! grammar is shared.
//!
//! A count larger than every possible input is served exactly by
//! [`u64::MAX`], so every multiply saturates rather than wrapping or
//! failing — a genuinely out-of-range spelling is still rejected as
//! [`None`].

/// Parse a non-empty all-ASCII-digit spelling into a count, saturating at
/// [`u64::MAX`]. Returns [`None`] for an empty or non-digit spelling.
#[must_use]
pub fn parse_decimal(digits: &str) -> Option<u64> {
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut value: u64 = 0;
    for digit in digits.bytes() {
        value = value
            .saturating_mul(10)
            .saturating_add(u64::from(digit - b'0'));
    }
    Some(value)
}

/// Parse a decimal count with an optional GNU multiplier suffix, saturating
/// at [`u64::MAX`]. Returns [`None`] when the spelling is not a count (no
/// leading digit, or an unknown suffix).
#[must_use]
pub fn parse_suffixed(text: &str) -> Option<u64> {
    let digits_end = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    if digits_end == 0 {
        return None;
    }
    let value = parse_decimal(&text[..digits_end])?;
    let multiplier = suffix_multiplier(&text[digits_end..])?;
    Some(value.saturating_mul(multiplier))
}

/// The multiplier a suffix spelling names (`""` is 1), or [`None`] for an
/// unknown suffix. Powers beyond what a `u64` holds saturate.
fn suffix_multiplier(suffix: &str) -> Option<u64> {
    if suffix.is_empty() {
        return Some(1);
    }
    if suffix == "b" {
        return Some(512);
    }
    let mut chars = suffix.chars();
    let letter = chars.next()?;
    let power: u32 = match letter {
        'k' | 'K' => 1,
        'm' | 'M' => 2,
        'G' => 3,
        'T' => 4,
        'P' => 5,
        'E' => 6,
        'Z' => 7,
        'Y' => 8,
        'R' => 9,
        'Q' => 10,
        _ => return None,
    };
    let base: u64 = match chars.as_str() {
        "" | "iB" => 1024,
        "B" => 1000,
        _ => return None,
    };
    Some(saturating_pow(base, power))
}

/// `base` raised to `power`, saturating at [`u64::MAX`].
fn saturating_pow(base: u64, power: u32) -> u64 {
    let mut value: u64 = 1;
    for _ in 0..power {
        value = value.saturating_mul(base);
    }
    value
}

#[cfg(test)]
mod tests {
    use super::{parse_decimal, parse_suffixed};

    #[test]
    fn decimal_parses_and_saturates() {
        assert_eq!(parse_decimal("0"), Some(0));
        assert_eq!(parse_decimal("42"), Some(42));
        assert_eq!(parse_decimal("18446744073709551615"), Some(u64::MAX));
        assert_eq!(parse_decimal("99999999999999999999999"), Some(u64::MAX));
        assert_eq!(parse_decimal(""), None);
        assert_eq!(parse_decimal("1a"), None);
        assert_eq!(parse_decimal("-1"), None);
    }

    #[test]
    fn suffixed_matches_the_gnu_multiplier_alphabet() {
        assert_eq!(parse_suffixed("5"), Some(5));
        assert_eq!(parse_suffixed("1b"), Some(512));
        assert_eq!(parse_suffixed("2K"), Some(2048));
        assert_eq!(parse_suffixed("2k"), Some(2048));
        assert_eq!(parse_suffixed("1kB"), Some(1000));
        assert_eq!(parse_suffixed("1MiB"), Some(1024 * 1024));
        assert_eq!(parse_suffixed("3MB"), Some(3_000_000));
        // Beyond u64 saturates: larger than any possible input.
        assert_eq!(parse_suffixed("99Y"), Some(u64::MAX));
    }

    #[test]
    fn suffixed_rejects_non_counts() {
        assert_eq!(parse_suffixed(""), None);
        assert_eq!(parse_suffixed("x"), None);
        assert_eq!(parse_suffixed("5J"), None);
        assert_eq!(parse_suffixed("5kX"), None);
    }
}
