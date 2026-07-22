//! Display formatting for the item view's metadata columns.
//!
//! The list view shows each entry's byte [`size`](crate::Entry::size) and its
//! last-modification [`Time64`] alongside its name. Turning those raw figures
//! into the short strings a column shows is presentation, shared by the file
//! manager and the trusted picker so the two render a listing identically —
//! it lives here once rather than being re-derived per view.
//!
//! This is the *file-listing* convention (binary size units, an ISO calendar
//! date), deliberately distinct from the `top`/`sysinfo` figure spellings in
//! `lib/procinfo` — a browser engine does not depend on the System
//! Information client crate, and a directory listing reads by calendar date,
//! not by an uptime-relative figure. Both are pure and total: every input
//! produces a string, and no path panics.

use alloc::format;
use alloc::string::String;

use tairix_abi::time::Time64;

/// Render a byte count for the item view's size column in binary units:
/// `0 B`, `512 B`, `1.0 KiB`, `1.5 MiB`, `2.3 GiB`, … up to `PiB`.
///
/// Below one kibibyte the exact byte count is shown (`742 B`); at or above it
/// the value is rendered in the largest unit for which it is at least one,
/// with a single decimal place. The tenths are computed in `u128` so even a
/// multi-exbibyte size (the huge-volume floor the charter designs for) scales
/// without overflow rather than wrapping.
#[must_use]
pub fn format_size(bytes: u64) -> String {
    const UNIT: u128 = 1024;
    /// The scaled-unit letters, indexed by how many times `bytes` was divided
    /// by [`UNIT`] beyond the byte: `0` → KiB, `1` → MiB, …
    const LETTERS: [char; 5] = ['K', 'M', 'G', 'T', 'P'];

    if u128::from(bytes) < UNIT {
        return format!("{bytes} B");
    }

    let mut idx = 0usize;
    let mut divisor = UNIT;
    while u128::from(bytes) >= divisor.saturating_mul(UNIT) && idx + 1 < LETTERS.len() {
        divisor = divisor.saturating_mul(UNIT);
        idx += 1;
    }
    let tenths = u128::from(bytes).saturating_mul(10) / divisor;
    format!("{}.{} {}iB", tenths / 10, tenths % 10, LETTERS[idx])
}

/// Render an entry's modification instant as an ISO calendar date
/// (`YYYY-MM-DD`), or the empty string when the instant is the epoch.
///
/// A backing that keeps no per-node stamp reports [`Time64::UNIX_EPOCH`]
/// (see [`Entry::modified`](crate::Entry::modified)); showing `1970-01-01`
/// for every such file would be a fabricated wall time, so the column is left
/// blank instead — honest about the absence rather than inventing a date.
///
/// Dates before 1970 and far after 2038 render correctly: the conversion is
/// the standard proleptic-Gregorian days-to-civil algorithm over the signed
/// 64-bit seconds, with floor division so a pre-epoch instant maps to the
/// civil day that contains it.
#[must_use]
pub fn format_date(modified: Time64) -> String {
    if modified.secs() == Time64::UNIX_EPOCH.secs()
        && modified.subsec_nanos() == Time64::UNIX_EPOCH.subsec_nanos()
    {
        return String::new();
    }
    let (year, month, day) = civil_from_secs(modified.secs());
    format!("{year:04}-{month:02}-{day:02}")
}

/// Convert signed seconds since the Unix epoch into a proleptic-Gregorian
/// `(year, month, day)`, flooring to the civil day that contains the instant.
///
/// The days-to-civil arithmetic is Howard Hinnant's well-known algorithm,
/// exact for the full range the seconds can express.
fn civil_from_secs(secs: i64) -> (i64, i64, i64) {
    const SECS_PER_DAY: i64 = 86_400;
    let days = secs.div_euclid(SECS_PER_DAY);

    // Shift the epoch to 0000-03-01 so leap days fall at the end of the cycle.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::{civil_from_secs, format_date, format_size};
    use tairix_abi::time::Time64;

    #[test]
    fn sizes_below_a_kibibyte_show_exact_bytes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1), "1 B");
        assert_eq!(format_size(742), "742 B");
        assert_eq!(format_size(1023), "1023 B");
    }

    #[test]
    fn sizes_scale_into_binary_units_with_one_decimal() {
        assert_eq!(format_size(1024), "1.0 KiB");
        assert_eq!(format_size(1536), "1.5 KiB");
        assert_eq!(format_size(1024 * 1024), "1.0 MiB");
        assert_eq!(format_size(1024 * 1024 * 1024), "1.0 GiB");
        assert_eq!(format_size(1024u64.pow(4)), "1.0 TiB");
        assert_eq!(format_size(1024u64.pow(5)), "1.0 PiB");
    }

    #[test]
    fn huge_sizes_stay_in_pib_without_overflow() {
        // Beyond a pebibyte the value stays in PiB rather than wrapping.
        assert_eq!(format_size(u64::MAX), "16383.9 PiB");
    }

    #[test]
    fn the_epoch_renders_blank_never_a_fabricated_date() {
        assert_eq!(format_date(Time64::UNIX_EPOCH), "");
    }

    #[test]
    fn dates_render_iso_after_and_before_the_epoch() {
        // 2021-01-01T00:00:00Z.
        assert_eq!(format_date(Time64::from_secs(1_609_459_200)), "2021-01-01");
        // A stamp late in a day still resolves to that civil day.
        assert_eq!(
            format_date(Time64::from_secs(1_609_459_200 + 86_399)),
            "2021-01-01"
        );
        // One second before the epoch is the last day of 1969.
        assert_eq!(format_date(Time64::from_secs(-1)), "1969-12-31");
        // Far after 2038 (the 32-bit boundary) still renders.
        assert_eq!(format_date(Time64::from_secs(4_102_444_800)), "2100-01-01");
    }

    #[test]
    fn civil_conversion_matches_known_days() {
        assert_eq!(civil_from_secs(0), (1970, 1, 1));
        assert_eq!(civil_from_secs(-86_400), (1969, 12, 31));
        // A leap day.
        assert_eq!(civil_from_secs(1_582_934_400), (2020, 2, 29));
    }
}
