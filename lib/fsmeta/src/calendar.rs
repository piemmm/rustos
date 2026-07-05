//! Proleptic-Gregorian civil-date arithmetic, integer-only and `no_std`.
//!
//! Foreign timestamp formats such as GEMDOS/FAT store a *calendar* date
//! (year/month/day, hour/minute/second) rather than a count of seconds, so
//! converting them to and from a `Time64` needs a calendar. These are Howard
//! Hinnant's well-known `days_from_civil` / `civil_from_days` algorithms,
//! exact for every date in the `i64` range and with no dependency.

/// Days from the Unix epoch (1970-01-01) to the given proleptic-Gregorian
/// civil date. `month` is `1..=12`, `day` is `1..=31`; the caller validates
/// the ranges. Negative for dates before the epoch.
#[must_use]
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let m = i64::from(month);
    let d = i64::from(day);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The proleptic-Gregorian civil date `(year, month, day)` for a count of days
/// from the Unix epoch (the inverse of [`days_from_civil`]).
#[must_use]
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    // `mp` is `0..=11`, so `m` is `1..=12` and `d` is `1..=31`: both fit `u32`.
    (year, u32_from(m), u32_from(d))
}

/// Narrow a known-small, non-negative `i64` day/month component to `u32`.
fn u32_from(value: i64) -> u32 {
    u32::try_from(value).unwrap_or(0)
}
