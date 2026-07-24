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

/// Render a byte count for a `SIZE`-style column: whole KiB below ten MiB
/// (`8432K`), tenths of MiB below ten GiB (`123.4M`), tenths of GiB above
/// (`12.3G`). Zero renders as `0`.
#[must_use]
pub fn format_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if bytes == 0 {
        return String::from("0");
    }
    if bytes < 10 * MIB {
        return format!("{}K", bytes.div_ceil(KIB));
    }
    if bytes < 10 * GIB {
        let tenths = bytes * 10 / MIB;
        return format!("{}.{}M", tenths / 10, tenths % 10);
    }
    let tenths = bytes * 10 / GIB;
    format!("{}.{}G", tenths / 10, tenths % 10)
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
    use super::{format_count, format_load, format_mib, format_size, format_tenths, format_uptime};

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
    fn sizes_choose_the_gnu_top_unit() {
        assert_eq!(format_size(0), "0");
        assert_eq!(format_size(1), "1K");
        assert_eq!(format_size(8 * 1024 * 1024), "8192K");
        assert_eq!(format_size(512 * 1024 * 1024), "512.0M");
        assert_eq!(format_size(20 * 1024 * 1024 * 1024), "20.0G");
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
