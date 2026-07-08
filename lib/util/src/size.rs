//! GNU coreutils-style size scaling: block-size parsing and human-readable
//! formatting.
//!
//! The disk-usage tools (`du`, `df`) both speak the GNU size vocabulary — a
//! `-B`/`--block-size` argument (`512`, `1K`, `1MiB`, `1GB`, `human-readable`,
//! `si`), `-k`, and the `-h`/`--si` human-readable renderings — so the
//! grammar and the rounding rules live here once, per the shared-code rule.
//! The behaviour follows GNU `human.c`/`xstrtoumax`: sizes are scaled with
//! **ceiling** rounding (a partially used block is a used block; usage is
//! never under-reported), and the human form shows one decimal below ten
//! units, an integer otherwise.
//!
//! Everything is `no_std`, allocation-free, total, and panic-free: parsers
//! return `None` on any malformed or overflowing input (fail closed), and
//! formatters write into a caller-supplied stack buffer.

/// How a size should be rendered, as selected by the GNU block-size options.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SizeScale {
    /// Whole blocks of the given byte size, ceiling-rounded (`-B N`, `-k`).
    Blocks(u64),
    /// Human-readable, powers of 1024 (`-h` / `--block-size=human-readable`).
    HumanBinary,
    /// Human-readable, powers of 1000 (`--si` / `--block-size=si`).
    HumanDecimal,
}

/// Maximum bytes a rendered size occupies: 39 digits for a full `u128` plus
/// a decimal point, a tenths digit, and a unit letter fit comfortably.
pub const SIZE_TEXT_MAX: usize = 42;

/// Parse a GNU `--block-size` argument into a [`SizeScale`].
///
/// Accepts the GNU grammar: an optional decimal count followed by an
/// optional suffix, or one of the two rendering words:
///
/// * words — `human-readable` (powers of 1024), `si` (powers of 1000);
/// * byte suffixes — `c` (1), `w` (2), `b` (512);
/// * powers of 1024 — `K`/`k`, `M`, `G`, `T`, `P`, `E` (also with `iB`:
///   `KiB`, `MiB`, …);
/// * powers of 1000 — the same letters with `B`: `KB`/`kB`, `MB`, ….
///
/// A bare count (`512`) is that many bytes. Returns `None` — fail closed,
/// never a guessed unit — for an empty string, a zero size, an unknown
/// suffix, or a count that overflows `u64`.
#[must_use]
pub fn parse_block_size(text: &str) -> Option<SizeScale> {
    match text {
        "human-readable" => return Some(SizeScale::HumanBinary),
        "si" => return Some(SizeScale::HumanDecimal),
        _ => {}
    }
    let bytes = text.as_bytes();
    let digits_len = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
    let (digits, suffix) = text.split_at(digits_len);
    let count: u64 = if digits.is_empty() {
        1
    } else {
        digits.parse().ok()?
    };
    let unit = match suffix {
        "" => {
            if digits.is_empty() {
                return None;
            }
            1
        }
        "c" => 1,
        "w" => 2,
        "b" => 512,
        _ => parse_unit_suffix(suffix)?,
    };
    let size = count.checked_mul(unit)?;
    if size == 0 {
        return None;
    }
    Some(SizeScale::Blocks(size))
}

/// The multiplier for a `K`/`KiB`/`KB`-style unit suffix, or `None` for an
/// unknown spelling.
fn parse_unit_suffix(suffix: &str) -> Option<u64> {
    let (letter, rest) = {
        let mut chars = suffix.chars();
        (chars.next()?, chars.as_str())
    };
    let base: u64 = match rest {
        // `K` — powers of 1024; `KiB` — the same, spelled out.
        "" | "iB" => 1024,
        // `KB` — powers of 1000.
        "B" => 1000,
        _ => return None,
    };
    let exponent: u32 = match letter.to_ascii_uppercase() {
        'K' => 1,
        'M' => 2,
        'G' => 3,
        'T' => 4,
        'P' => 5,
        'E' => 6,
        _ => return None,
    };
    base.checked_pow(exponent)
}

/// Scale `bytes` into whole `block_size`-byte blocks, rounding **up** — a
/// partially used block is a used block, so usage is never under-reported.
///
/// A zero `block_size` cannot name a unit and yields `None` (fail closed);
/// callers obtain block sizes through [`parse_block_size`], which already
/// refuses zero.
#[must_use]
pub fn blocks_ceil(bytes: u128, block_size: u64) -> Option<u128> {
    if block_size == 0 {
        return None;
    }
    Some(bytes.div_ceil(u128::from(block_size)))
}

/// Unit letters for the powers of 1024 (`human-readable`); index 0 is the
/// bare-byte tier and carries no letter.
const BINARY_UNITS: [char; 8] = ['K', 'M', 'G', 'T', 'P', 'E', 'Z', 'Y'];

/// Unit letters for the powers of 1000 (`si`). SI spells kilo lowercase.
const DECIMAL_UNITS: [char; 8] = ['k', 'M', 'G', 'T', 'P', 'E', 'Z', 'Y'];

/// Render `bytes` in the GNU human-readable form into `buf`, returning the
/// populated prefix.
///
/// Matches GNU `human_ceiling` output: a size below one unit prints as a
/// bare integer; a scaled size prints with one decimal below ten units
/// (`1.5K`, `9.9M`) and as an integer otherwise (`23M`, `999G`), always
/// rounding **up** so usage is never under-reported. A tier that rounds up
/// to the next unit moves to it (`1023.99K` → `1.0M`).
#[must_use]
pub fn format_human(bytes: u128, scale_base: u64, buf: &mut [u8; SIZE_TEXT_MAX]) -> &str {
    let (base, units) = if scale_base == 1000 {
        (1000u128, &DECIMAL_UNITS)
    } else {
        (1024u128, &BINARY_UNITS)
    };
    if bytes < base {
        return format_u128(bytes, buf);
    }
    // The largest tier whose unit does not exceed the value. `u128` holds
    // at most base^13, so the highest table entry is always reachable.
    let mut tier = 0usize;
    let mut unit_value = base;
    while tier + 1 < units.len() && bytes >= unit_value * base {
        tier += 1;
        unit_value *= base;
    }
    // A value whose ceiling reaches a full `base` of this tier's units
    // (1023.44K → 1024K) presents in the next tier instead (1.0M), exactly
    // as GNU re-tiers a rounded-up amount.
    if bytes.div_ceil(unit_value) >= base && tier + 1 < units.len() {
        tier += 1;
        unit_value *= base;
    }
    // Tenths of a unit decide the form: strictly below ten units (tenths
    // ≤ 99) prints one decimal place (`1.5K` … `9.9K`); anything else is a
    // whole number, still rounded up. The ×10 only happens when `bytes <
    // 10 × unit_value ≤ 10 × 1024⁸`, so it cannot overflow a `u128`.
    let tenths = if bytes < 10 * unit_value {
        (bytes * 10).div_ceil(unit_value)
    } else {
        100
    };
    let written = if tenths < 100 {
        let mut len = encode_u128(tenths / 10, buf);
        buf[len] = b'.';
        buf[len + 1] = b'0' + u8::try_from(tenths % 10).unwrap_or(0);
        len += 2;
        len
    } else {
        encode_u128(bytes.div_ceil(unit_value), buf)
    };
    buf[written] = unit_char(units[tier]);
    // The buffer holds only the ASCII bytes just written.
    core::str::from_utf8(&buf[..=written]).unwrap_or("")
}

/// Render `value` as decimal text into `buf`, returning the populated prefix.
#[must_use]
pub fn format_u128(value: u128, buf: &mut [u8; SIZE_TEXT_MAX]) -> &str {
    let len = encode_u128(value, buf);
    core::str::from_utf8(&buf[..len]).unwrap_or("")
}

/// Write `value` as decimal ASCII into the front of `buf`, returning the
/// byte count. A `u128` has at most 39 digits, which always fits.
fn encode_u128(mut value: u128, buf: &mut [u8; SIZE_TEXT_MAX]) -> usize {
    let mut tmp = [0u8; 39];
    let mut pos = tmp.len();
    if value == 0 {
        pos -= 1;
        tmp[pos] = b'0';
    }
    while value > 0 {
        pos -= 1;
        // `value % 10` is in `0..=9`; the cast is lossless.
        #[allow(clippy::cast_possible_truncation)]
        {
            tmp[pos] = b'0' + (value % 10) as u8;
        }
        value /= 10;
    }
    let len = tmp.len() - pos;
    buf[..len].copy_from_slice(&tmp[pos..]);
    len
}

/// A unit letter as its single ASCII byte (the tables hold only ASCII).
fn unit_char(unit: char) -> u8 {
    let mut encoded = [0u8; 4];
    unit.encode_utf8(&mut encoded);
    encoded[0]
}

#[cfg(test)]
mod tests {
    use super::{blocks_ceil, format_human, parse_block_size, SizeScale, SIZE_TEXT_MAX};

    #[test]
    fn parses_the_rendering_words() {
        assert_eq!(
            parse_block_size("human-readable"),
            Some(SizeScale::HumanBinary)
        );
        assert_eq!(parse_block_size("si"), Some(SizeScale::HumanDecimal));
    }

    #[test]
    fn parses_bare_counts_and_byte_suffixes() {
        assert_eq!(parse_block_size("512"), Some(SizeScale::Blocks(512)));
        assert_eq!(parse_block_size("c"), Some(SizeScale::Blocks(1)));
        assert_eq!(parse_block_size("w"), Some(SizeScale::Blocks(2)));
        assert_eq!(parse_block_size("b"), Some(SizeScale::Blocks(512)));
        assert_eq!(parse_block_size("2b"), Some(SizeScale::Blocks(1024)));
    }

    #[test]
    fn parses_binary_and_decimal_unit_suffixes() {
        assert_eq!(parse_block_size("K"), Some(SizeScale::Blocks(1024)));
        assert_eq!(parse_block_size("k"), Some(SizeScale::Blocks(1024)));
        assert_eq!(parse_block_size("1K"), Some(SizeScale::Blocks(1024)));
        assert_eq!(parse_block_size("KiB"), Some(SizeScale::Blocks(1024)));
        assert_eq!(parse_block_size("KB"), Some(SizeScale::Blocks(1000)));
        assert_eq!(parse_block_size("kB"), Some(SizeScale::Blocks(1000)));
        assert_eq!(parse_block_size("M"), Some(SizeScale::Blocks(1024 * 1024)));
        assert_eq!(parse_block_size("MB"), Some(SizeScale::Blocks(1_000_000)));
        assert_eq!(
            parse_block_size("2MiB"),
            Some(SizeScale::Blocks(2 * 1024 * 1024))
        );
        assert_eq!(
            parse_block_size("G"),
            Some(SizeScale::Blocks(1024 * 1024 * 1024))
        );
        assert_eq!(parse_block_size("E"), Some(SizeScale::Blocks(1 << 60)));
    }

    #[test]
    fn rejects_malformed_zero_and_overflowing_sizes() {
        assert_eq!(parse_block_size(""), None);
        assert_eq!(parse_block_size("0"), None);
        assert_eq!(parse_block_size("Q"), None);
        assert_eq!(parse_block_size("1X"), None);
        assert_eq!(parse_block_size("K B"), None);
        assert_eq!(parse_block_size("12.5K"), None);
        assert_eq!(parse_block_size("-1"), None);
        // 2^64 bytes overflows the u64 count.
        assert_eq!(parse_block_size("18446744073709551616"), None);
        assert_eq!(parse_block_size("99999999999999999999E"), None);
    }

    #[test]
    fn blocks_round_up_and_refuse_a_zero_unit() {
        assert_eq!(blocks_ceil(0, 1024), Some(0));
        assert_eq!(blocks_ceil(1, 1024), Some(1));
        assert_eq!(blocks_ceil(1024, 1024), Some(1));
        assert_eq!(blocks_ceil(1025, 1024), Some(2));
        assert_eq!(blocks_ceil(1, 0), None);
    }

    #[test]
    fn human_binary_matches_the_gnu_renderings() {
        let mut buf = [0u8; SIZE_TEXT_MAX];
        assert_eq!(format_human(0, 1024, &mut buf), "0");
        assert_eq!(format_human(1023, 1024, &mut buf), "1023");
        assert_eq!(format_human(1024, 1024, &mut buf), "1.0K");
        assert_eq!(format_human(1025, 1024, &mut buf), "1.1K");
        assert_eq!(format_human(1536, 1024, &mut buf), "1.5K");
        assert_eq!(format_human(10 * 1024, 1024, &mut buf), "10K");
        assert_eq!(format_human(10 * 1024 + 1, 1024, &mut buf), "11K");
        assert_eq!(format_human(1024 * 1024 - 1, 1024, &mut buf), "1.0M");
        assert_eq!(format_human(23 * 1024 * 1024, 1024, &mut buf), "23M");
        assert_eq!(format_human(4 * 1024 * 1024 * 1024, 1024, &mut buf), "4.0G");
    }

    #[test]
    fn human_decimal_uses_si_letters_and_powers() {
        let mut buf = [0u8; SIZE_TEXT_MAX];
        assert_eq!(format_human(999, 1000, &mut buf), "999");
        assert_eq!(format_human(1000, 1000, &mut buf), "1.0k");
        assert_eq!(format_human(1500, 1000, &mut buf), "1.5k");
        assert_eq!(format_human(1_000_000, 1000, &mut buf), "1.0M");
        assert_eq!(format_human(999_000_000, 1000, &mut buf), "999M");
    }

    #[test]
    fn human_carries_a_rounded_tier_into_the_next_unit() {
        let mut buf = [0u8; SIZE_TEXT_MAX];
        // 1023.95K rounds past 1024.0K, so it must present as 1.0M.
        assert_eq!(format_human(1024 * 1024 - 50, 1024, &mut buf), "1.0M");
        // 1023.44K also ceils to 1024K in the integer branch, which must
        // re-tier rather than print a four-digit "1024K".
        assert_eq!(format_human(1023 * 1024 + 450, 1024, &mut buf), "1.0M");
        // The largest value that stays in the K tier.
        assert_eq!(format_human(1023 * 1024, 1024, &mut buf), "1023K");
        // Just under ten units keeps the decimal; ten and over drops it.
        assert_eq!(format_human(9 * 1024 + 512, 1024, &mut buf), "9.5K");
    }

    #[test]
    fn human_reaches_the_top_tier_without_overflow() {
        let mut buf = [0u8; SIZE_TEXT_MAX];
        // 2^80 bytes = 1.0Y at powers of 1024.
        assert_eq!(format_human(1u128 << 80, 1024, &mut buf), "1.0Y");
        assert_eq!(format_human(u128::from(u64::MAX), 1024, &mut buf), "16E");
    }
}
