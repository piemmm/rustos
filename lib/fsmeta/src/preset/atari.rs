//! Atari GEMDOS / TOS preset conversions.
//!
//! Atari TOS uses a FAT-derived on-disk format. A file carries an 8-bit
//! GEMDOS attribute byte and a FAT-style packed date/time with two-second
//! resolution and a 1980 epoch. The registry keeps the Atari attributes
//! distinct from a generic FAT mapping so intent is not lost, and converts the
//! packed date/time to and from a [`Time64`] through the checked path.

use rustos_abi::time::Time64;

use crate::calendar::{civil_from_days, days_from_civil};
use crate::MetadataError;

/// Read-only attribute bit.
pub const ATTR_READ_ONLY: u8 = 0x01;
/// Hidden attribute bit.
pub const ATTR_HIDDEN: u8 = 0x02;
/// System attribute bit.
pub const ATTR_SYSTEM: u8 = 0x04;
/// Volume-label attribute bit.
pub const ATTR_VOLUME_LABEL: u8 = 0x08;
/// Sub-directory attribute bit.
pub const ATTR_DIRECTORY: u8 = 0x10;
/// Archive attribute bit.
pub const ATTR_ARCHIVE: u8 = 0x20;

/// Every defined GEMDOS attribute bit.
const ATTR_KNOWN_MASK: u8 =
    ATTR_READ_ONLY | ATTR_HIDDEN | ATTR_SYSTEM | ATTR_VOLUME_LABEL | ATTR_DIRECTORY | ATTR_ARCHIVE;

/// The GEMDOS/FAT date epoch year.
const FAT_EPOCH_YEAR: i64 = 1980;

/// Largest year the 7-bit FAT year field can encode (`1980 + 127`).
const FAT_MAX_YEAR: i64 = FAT_EPOCH_YEAR + 127;

/// Seconds in a day.
const SECS_PER_DAY: i64 = 86_400;

/// Validate a raw GEMDOS attribute byte, rejecting unknown bits. The byte is
/// stored verbatim as the `atari.attributes` value.
///
/// # Errors
///
/// [`MetadataError::NotRepresentable`] if any bit outside the known mask is
/// set.
pub fn validate_attributes(bits: u8) -> Result<(), MetadataError> {
    if bits & !ATTR_KNOWN_MASK != 0 {
        return Err(MetadataError::NotRepresentable);
    }
    Ok(())
}

/// Convert a FAT-style packed `(date, time)` pair to a [`Time64`].
///
/// # Errors
///
/// [`MetadataError::NotRepresentable`] if a packed field is out of range
/// (month `0`/`>12`, day `0`/`>31`, hour `>23`, minute `>59`, doubled-second
/// `>58`).
pub fn datetime_to_time64(date: u16, time: u16) -> Result<Time64, MetadataError> {
    let year = FAT_EPOCH_YEAR + i64::from(date >> 9);
    let month = u32::from((date >> 5) & 0x0F);
    let day = u32::from(date & 0x1F);
    let hour = i64::from(time >> 11);
    let minute = i64::from((time >> 5) & 0x3F);
    let second = i64::from((time & 0x1F) * 2);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 58
    {
        return Err(MetadataError::NotRepresentable);
    }
    let days = days_from_civil(year, month, day);
    let secs = days * SECS_PER_DAY + hour * 3600 + minute * 60 + second;
    Ok(Time64::from_secs(secs))
}

/// Convert a [`Time64`] to a FAT-style packed `(date, time)` pair, checked.
///
/// # Errors
///
/// [`MetadataError::TimestampOutOfRange`] if the instant falls outside the
/// 1980..=2107 range the FAT date field can encode, or carries sub-second
/// (nanosecond) or odd-second precision the two-second FAT time field cannot
/// represent (never silently dropped).
pub fn time64_to_datetime(time: Time64) -> Result<(u16, u16), MetadataError> {
    if time.subsec_nanos() != 0 {
        return Err(MetadataError::TimestampOutOfRange);
    }
    let secs = time.secs();
    let days = secs.div_euclid(SECS_PER_DAY);
    let tod = secs.rem_euclid(SECS_PER_DAY);
    if tod % 2 != 0 {
        return Err(MetadataError::TimestampOutOfRange);
    }
    let (year, month, day) = civil_from_days(days);
    if !(FAT_EPOCH_YEAR..=FAT_MAX_YEAR).contains(&year) {
        return Err(MetadataError::TimestampOutOfRange);
    }
    let hour = tod / 3600;
    let minute = (tod % 3600) / 60;
    let second = tod % 60;
    let year_field = u16::try_from(year - FAT_EPOCH_YEAR).unwrap_or(0);
    let month_field = u16::try_from(month).unwrap_or(0);
    let day_field = u16::try_from(day).unwrap_or(0);
    let date = (year_field << 9) | (month_field << 5) | day_field;
    let hour_field = u16::try_from(hour).unwrap_or(0);
    let minute_field = u16::try_from(minute).unwrap_or(0);
    let second_field = u16::try_from(second / 2).unwrap_or(0);
    let packed_time = (hour_field << 11) | (minute_field << 5) | second_field;
    Ok((date, packed_time))
}
