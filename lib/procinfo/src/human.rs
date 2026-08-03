//! Human-readable figure rendering shared by the full-screen viewers.
//!
//! `top` and `sysmon` render the same `sysinfo-v1` figures — byte counts,
//! tenths-of-a-percent shares, uptimes, load averages — in the same
//! GNU-`top`-familiar spellings, so the formatting lives here once. Each
//! viewer keeps its own layout; this module owns only the figure → text
//! conversions they would otherwise copy.

use alloc::format;
use alloc::string::String;

use tairix_abi::sysinfo::LoadAverage;

/// Render a tenths-of-a-percent figure as `W.T`, saturating at `999.9` so
/// a column never widens.
#[must_use]
pub fn format_tenths(tenths: u32) -> String {
    let tenths = tenths.min(9_999);
    format!("{}.{}", tenths / 10, tenths % 10)
}

/// The widest string [`format_size`] can return, in columns.
///
/// A byte-count column is laid out at least this wide. A figure cannot be
/// elided the way an over-long name can — an elided number is a wrong
/// number — so the formatter is bounded instead, and this is the bound the
/// viewers budget for.
pub const SIZE_WIDTH: usize = 7;

/// Render a byte count for a `SIZE`-style column: the exact count below a
/// kibibyte (`742`), otherwise one decimal place in the largest binary band
/// the count is under 1024 of, with a one-letter suffix — `K`, `M`, `G`,
/// `T`, `P`, `E` (`1.5K`, `986.2M`, `2.0G`, `15.9E`). Zero renders as `0`.
///
/// The ladder runs all the way to exbibytes so that no `u64` can overrun
/// [`SIZE_WIDTH`]: the widest result is `1023.9X` at the top of a band, and
/// `u64::MAX` lands in the exbibyte band as `15.9E`. A ladder that stops
/// lower has no band left to promote a huge figure into and grows the digits
/// instead, which is what pushed a multi-terabyte figure past its column.
///
/// The tenths are computed in `u128`, so scaling `u64::MAX` cannot wrap.
#[must_use]
pub fn format_size(bytes: u64) -> String {
    /// One band is this many times the one below it.
    const BAND: u128 = 1024;
    /// The band letters, indexed by how many times the count was divided by
    /// [`BAND`]: `0` → kibibytes … `5` → exbibytes, which no `u64` exceeds.
    const LETTERS: [char; 6] = ['K', 'M', 'G', 'T', 'P', 'E'];

    let bytes = u128::from(bytes);
    if bytes < BAND {
        return format!("{bytes}");
    }
    let mut letter = 0usize;
    let mut divisor = BAND;
    while bytes / divisor >= BAND && letter + 1 < LETTERS.len() {
        divisor *= BAND;
        letter += 1;
    }
    let tenths = bytes * 10 / divisor;
    format!("{}.{}{}", tenths / 10, tenths % 10, LETTERS[letter])
}

/// Render a plain event count for a narrow column: the exact number
/// below 100 000 (`12345`), then tenths of the next SI-style unit —
/// `k` thousands, `M` millions, `G` billions, `T` trillions — so a
/// counter that grows without bound (cache hits, interrupt counts)
/// never widens its column past six characters. Zero renders as `0`.
///
/// Unlike [`format_size`] this is a *count* of things, not bytes, so it
/// scales in decimal thousands (`k`/`M`/…), not binary KiB/MiB.
#[must_use]
pub fn format_count(count: u64) -> String {
    const K: u64 = 1_000;
    const M: u64 = 1_000 * K;
    const G: u64 = 1_000 * M;
    const T: u64 = 1_000 * G;
    if count < 100_000 {
        return format!("{count}");
    }
    let (unit, suffix) = if count < M {
        (K, 'k')
    } else if count < G {
        (M, 'M')
    } else if count < T {
        (G, 'G')
    } else {
        (T, 'T')
    };
    let tenths = count * 10 / unit;
    format!("{}.{}{}", tenths / 10, tenths % 10, suffix)
}

/// Render a byte count as tenths of MiB (`986.2`), the unit of the GNU
/// `top` memory summary line.
#[must_use]
pub fn format_mib(bytes: u64) -> String {
    let tenths = bytes * 10 / (1024 * 1024);
    format!("{}.{}", tenths / 10, tenths % 10)
}

/// Render a monotonic-nanosecond uptime as `D days, H:MM` / `H:MM`.
#[must_use]
pub fn format_uptime(ns: u64) -> String {
    let minutes = ns / 60_000_000_000;
    let hours = minutes / 60;
    let days = hours / 24;
    if days > 0 {
        format!(
            "{} day{}, {}:{:02}",
            days,
            if days == 1 { "" } else { "s" },
            hours % 24,
            minutes % 60
        )
    } else {
        format!("{}:{:02}", hours, minutes % 60)
    }
}

/// Render a fixed-point load-average value as the conventional `W.CC`.
#[must_use]
pub fn format_load(fixed: u32) -> String {
    format!(
        "{}.{:02}",
        LoadAverage::whole(fixed),
        LoadAverage::centis(fixed)
    )
}

#[cfg(test)]
mod tests {
    use super::{
        format_count, format_load, format_mib, format_size, format_tenths, format_uptime,
        SIZE_WIDTH,
    };

    #[test]
    fn counts_render_exact_then_compact_units() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(99_999), "99999");
        // At and above 100 000 the compact SI-style units keep the
        // column narrow; counts scale in decimal thousands, not KiB.
        assert_eq!(format_count(100_000), "100.0k");
        assert_eq!(format_count(1_500_000), "1.5M");
        assert_eq!(format_count(2_000_000_000), "2.0G");
        assert_eq!(format_count(3_400_000_000_000), "3.4T");
    }

    #[test]
    fn tenths_render_with_one_decimal_and_saturate() {
        assert_eq!(format_tenths(0), "0.0");
        assert_eq!(format_tenths(123), "12.3");
        assert_eq!(format_tenths(1000), "100.0");
        assert_eq!(format_tenths(u32::MAX), "999.9");
    }

    #[test]
    fn sizes_climb_the_binary_ladder_to_exbibytes() {
        assert_eq!(format_size(0), "0");
        assert_eq!(format_size(742), "742");
        assert_eq!(format_size(1023), "1023");
        assert_eq!(format_size(1024), "1.0K");
        assert_eq!(format_size(1536), "1.5K");
        assert_eq!(format_size(8 * 1024 * 1024), "8.0M");
        assert_eq!(format_size(512 * 1024 * 1024), "512.0M");
        assert_eq!(format_size(20 * 1024 * 1024 * 1024), "20.0G");
        assert_eq!(format_size(1024u64.pow(4)), "1.0T");
        assert_eq!(format_size(3 * 1024u64.pow(5)), "3.0P");
        assert_eq!(format_size(1024u64.pow(6)), "1.0E");
        // The exbibyte band is the last one a `u64` can reach into.
        assert_eq!(format_size(u64::MAX), "15.9E");
    }

    #[test]
    fn no_byte_count_overflows_the_size_column() {
        // A column overflows at the *top* of a band, where the digits are
        // widest and the next band has not been entered yet, so every band
        // boundary is probed either side.
        let mut probes = alloc::vec::Vec::new();
        let mut band = 1u64;
        loop {
            probes.push(band);
            match band.checked_mul(1024) {
                Some(next) => band = next,
                None => break,
            }
        }
        probes.push(u64::MAX);
        for boundary in probes {
            for bytes in [
                boundary.saturating_sub(1),
                boundary,
                boundary.saturating_add(1),
            ] {
                let text = format_size(bytes);
                let width = text.chars().count();
                assert!(
                    width <= SIZE_WIDTH,
                    "format_size({bytes}) = {text:?} wants {width} columns, budget {SIZE_WIDTH}"
                );
            }
        }
        // The budget is tight, not slack: the top of a band really does need
        // all seven columns, so it must not be narrowed.
        assert_eq!(format_size(1024 * 1024 - 1), "1023.9K");
        assert_eq!(format_size(1024 * 1024 - 1).chars().count(), SIZE_WIDTH);
    }

    #[test]
    fn mib_renders_tenths() {
        assert_eq!(format_mib(0), "0.0");
        assert_eq!(format_mib(1024 * 1024), "1.0");
        assert_eq!(format_mib(1024 * 1024 + 512 * 1024), "1.5");
    }

    #[test]
    fn uptime_renders_days_hours_minutes() {
        assert_eq!(format_uptime(0), "0:00");
        assert_eq!(format_uptime(3_600_000_000_000), "1:00");
        assert_eq!(format_uptime(90_060_000_000_000), "1 day, 1:01");
        assert_eq!(format_uptime(2 * 86_400_000_000_000), "2 days, 0:00");
    }

    #[test]
    fn load_renders_two_centis() {
        use tairix_abi::sysinfo::LOAD_FIXED_SHIFT;
        assert_eq!(format_load(0), "0.00");
        assert_eq!(format_load(1 << LOAD_FIXED_SHIFT), "1.00");
    }
}
