//! Proleptic-Gregorian civil-date arithmetic, integer-only and `no_std`.
//!
//! Foreign timestamp formats such as GEMDOS/FAT store a *calendar* date
//! (year/month/day, hour/minute/second) rather than a count of seconds, so
//! converting them to and from a `Time64` needs a calendar. These are Howard
//! Hinnant's well-known `days_from_civil` / `civil_from_days` algorithms,
//! exact for every date in the `i64` range and with no dependency.
//!
//! [`CivilTime`] is the one place a count of seconds since the Unix epoch is
//! decomposed into broken-down UTC fields (year/month/day, hour/minute/
//! second), so no display consumer re-derives the `div_euclid(86_400)` +
//! [`civil_from_days`] + hour/minute arithmetic itself.

use alloc::string::String;
use core::fmt::Write as _;

use tairix_abi::time::Time64;

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

/// Seconds in one UTC day.
const SECS_PER_DAY: i64 = 86_400;

/// A broken-down UTC civil time: the calendar fields of an absolute instant.
///
/// This is the one decomposition of a count of seconds since the Unix epoch
/// into `(year, month, day, hour, minute, second)`, so a display consumer —
/// `ls`'s date column, the login clock, `fstree`'s stamp column — never
/// re-derives the day/time arithmetic. All fields are UTC; TAIRiX has no
/// timezone offset to apply. Presentation (the exact rendered string) belongs
/// to each consumer, except the one canonical minute-granular spelling
/// [`CivilTime::iso_minute`] that more than one consumer shares.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CivilTime {
    /// Proleptic-Gregorian year (may be negative for dates before year 1).
    pub year: i64,
    /// Month of the year, `1..=12`.
    pub month: u32,
    /// Day of the month, `1..=31`.
    pub day: u32,
    /// Hour of the day, `0..=23`.
    pub hour: u32,
    /// Minute of the hour, `0..=59`.
    pub minute: u32,
    /// Second of the minute, `0..=59` (no leap seconds).
    pub second: u32,
}

impl CivilTime {
    /// Decompose `secs` seconds since the Unix epoch into UTC calendar
    /// fields. Negative `secs` (instants before 1970) are handled exactly
    /// through Euclidean division, so the time-of-day is always in range.
    #[must_use]
    pub fn from_unix_secs(secs: i64) -> Self {
        let days = secs.div_euclid(SECS_PER_DAY);
        let tod = secs.rem_euclid(SECS_PER_DAY);
        let (year, month, day) = civil_from_days(days);
        // `tod` is `0..86_400`, so each component is a known-small value.
        Self {
            year,
            month,
            day,
            hour: u32_from(tod / 3_600),
            minute: u32_from((tod % 3_600) / 60),
            second: u32_from(tod % 60),
        }
    }

    /// Decompose the whole-seconds part of a [`Time64`] instant. The
    /// sub-second field is not part of the calendar breakdown; a consumer
    /// that renders nanoseconds reads [`Time64::subsec_nanos`] itself.
    #[must_use]
    pub fn from_time64(time: Time64) -> Self {
        Self::from_unix_secs(time.secs())
    }

    /// Render as `YYYY-MM-DD HH:MM` (UTC, minute granularity): the shared
    /// long-ISO clock/stamp spelling every minute-granular consumer uses, so
    /// the format lives in one place. The year is zero-padded to at least
    /// four digits.
    #[must_use]
    pub fn iso_minute(&self) -> String {
        let mut out = String::new();
        // Writing into a `String` never fails; the `Result` is discarded
        // deliberately rather than unwrapped.
        let _ = write!(
            out,
            "{:04}-{:02}-{:02} {:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::CivilTime;
    use tairix_abi::time::Time64;

    #[test]
    fn epoch_decomposes_to_the_first_instant_of_1970() {
        let civil = CivilTime::from_unix_secs(0);
        assert_eq!(
            (
                civil.year,
                civil.month,
                civil.day,
                civil.hour,
                civil.minute,
                civil.second
            ),
            (1970, 1, 1, 0, 0, 0)
        );
    }

    #[test]
    fn a_known_instant_decomposes_field_for_field() {
        // 2024-02-29T13:46:07Z — a leap-day, past-2038 sanity anchor.
        let civil = CivilTime::from_unix_secs(1_709_214_367);
        assert_eq!(
            (
                civil.year,
                civil.month,
                civil.day,
                civil.hour,
                civil.minute,
                civil.second
            ),
            (2024, 2, 29, 13, 46, 7)
        );
        assert_eq!(civil.iso_minute(), "2024-02-29 13:46");
    }

    #[test]
    fn a_pre_epoch_instant_keeps_the_time_of_day_in_range() {
        // One second before the epoch is 1969-12-31T23:59:59Z, not a
        // negative or wrapped time-of-day.
        let civil = CivilTime::from_unix_secs(-1);
        assert_eq!(
            (
                civil.year,
                civil.month,
                civil.day,
                civil.hour,
                civil.minute,
                civil.second
            ),
            (1969, 12, 31, 23, 59, 59)
        );
    }

    #[test]
    fn from_time64_ignores_the_sub_second_field() {
        let time = Time64::new(1_709_214_367, 500_000_000).expect("valid nanos");
        assert_eq!(
            CivilTime::from_time64(time),
            CivilTime::from_unix_secs(1_709_214_367)
        );
    }
}
