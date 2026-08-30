//! The shared minute-granular spelling of a civil date.
//!
//! The calendar arithmetic itself lives with [`tairix_abi::time::Time64`] in
//! that crate ([`CivilTime`], `days_from_civil`, `civil_from_days`),
//! because every consumer of an instant already depends on that crate. This
//! module holds only the one rendered spelling more than one consumer shares,
//! which needs `alloc` and so cannot live there.

use alloc::string::String;
use core::fmt::Write as _;

use tairix_abi::time::CivilTime;

/// Render `civil` as `YYYY-MM-DD HH:MM` (UTC, minute granularity): the shared
/// long-ISO clock/stamp spelling every minute-granular consumer uses, so the
/// format lives in one place. The year is zero-padded to at least four digits.
#[must_use]
pub fn iso_minute(civil: &CivilTime) -> String {
    let mut out = String::new();
    // Writing into a `String` never fails; the `Result` is discarded
    // deliberately rather than unwrapped.
    let _ = write!(
        out,
        "{:04}-{:02}-{:02} {:02}:{:02}",
        civil.year, civil.month, civil.day, civil.hour, civil.minute
    );
    out
}

#[cfg(test)]
mod tests {
    use super::iso_minute;
    use tairix_abi::time::CivilTime;

    #[test]
    fn renders_a_known_instant_to_minute_granularity() {
        // 2024-02-29T13:46:07Z — a leap-day, past-2038 anchor.
        let civil = CivilTime::from_unix_secs(1_709_214_367);
        assert_eq!(iso_minute(&civil), "2024-02-29 13:46");
    }

    #[test]
    fn pads_every_component_to_a_fixed_width() {
        // 0001-02-03T04:05:06Z exercises the zero padding on every field.
        let civil = CivilTime {
            year: 1,
            month: 2,
            day: 3,
            hour: 4,
            minute: 5,
            second: 6,
        };
        assert_eq!(iso_minute(&civil), "0001-02-03 04:05");
    }
}
