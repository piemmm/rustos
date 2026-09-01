//! Real-time-clock driver class (`drivers/rtc/*`).
//!
//! An RTC is the machine's *local* time source: a counter the board keeps
//! running across a power cycle, so a machine has a plausible wall time before
//! any network exists. It is the `Firmware` rung of the wall clock's
//! provenance ladder ([`WallTimeState`](crate::WallTimeState)) — believed
//! until a validated network sample replaces it, never the other way round.
//!
//! # The authority split
//!
//! A driver implementing [`Rtc`] holds **no** clock authority. It reads and
//! writes its own chip and nothing else; the machine clock is set by the one
//! process holding `CAP_TIME_SET` (`userland/system/timed`), which reads this
//! class over [`crate::rtc_ipc`] and tags the reading itself. A compromised
//! RTC driver can therefore lie about the *chip*, but it can neither assert a
//! provenance it did not earn nor overwrite a network sync
//! (`plans/TIMESYNC.md`).
//!
//! # Two register shapes, one codec
//!
//! Concrete chips divide into two families:
//!
//! * **Linear counters** — a seconds or nanoseconds count since an epoch the
//!   chip documents (ARM PL031, the Goldfish RTC). Nothing to decode beyond
//!   the width.
//! * **Calendar register blocks** — packed binary-coded-decimal fields for
//!   second/minute/hour/day/month/year (MC146818 CMOS, DS3231, PCF8523,
//!   PCF85063A). The BCD conversion and the bridge to
//!   [`crate::time::CivilTime`] are the same for every one of them,
//!   so they are defined here once ([`bcd_to_bin`], [`bin_to_bcd`],
//!   [`resolve_two_digit_year`]) and each chip's driver contributes only its
//!   own register offsets, century handling, and quirks.

use crate::time::{CivilTime, Duration64, Time64, RELEASE_EPOCH_SECS};

use super::DriverError;

/// Convert one packed two-digit BCD byte to binary.
///
/// Returns `None` when either nibble is `0xA..=0xF`, which is not a decimal
/// digit: a register block holding one has not been programmed with a real
/// time, so the reading is refused rather than reinterpreted.
#[must_use]
pub const fn bcd_to_bin(bcd: u8) -> Option<u8> {
    let high = bcd >> 4;
    let low = bcd & 0x0F;
    if high > 9 || low > 9 {
        return None;
    }
    Some(high * 10 + low)
}

/// Convert a binary value in `0..=99` to one packed two-digit BCD byte.
///
/// Returns `None` for a value the two digits cannot hold, so a caller
/// composing a register block fails closed rather than writing a wrapped
/// field the chip would then report back as a different time.
#[must_use]
pub const fn bin_to_bcd(bin: u8) -> Option<u8> {
    if bin > 99 {
        return None;
    }
    Some(((bin / 10) << 4) | (bin % 10))
}

/// The hour of day (`0..=23`) a 12-hour clock reading names.
///
/// Returns `None` unless `twelve` is `1..=12`. A 12-hour clock spells
/// midnight as 12 AM and noon as 12 PM, which is why twelve maps to zero
/// before the PM offset — the mistake this exists to make once.
///
/// Which register bit carries `pm`, and whether the field is BCD or binary,
/// differ per chip; this arithmetic does not, so every calendar driver shares
/// it.
#[must_use]
pub const fn hour_from_twelve(twelve: u8, pm: bool) -> Option<u8> {
    if twelve < 1 || twelve > 12 {
        return None;
    }
    let hour = twelve % 12;
    Some(if pm { hour + 12 } else { hour })
}

/// The 12-hour reading and PM flag for an hour of day — the inverse of
/// [`hour_from_twelve`].
///
/// Returns `None` for an `hour` outside `0..=23`, so a caller composing a
/// register block fails closed rather than writing a wrapped field.
#[must_use]
pub const fn twelve_from_hour(hour: u8) -> Option<(u8, bool)> {
    if hour > 23 {
        return None;
    }
    let twelve = match hour % 12 {
        0 => 12,
        other => other,
    };
    Some((twelve, hour >= 12))
}

/// The first full year the wall-clock plausibility window admits — the civil
/// year of [`RELEASE_EPOCH_SECS`], derived rather than restated.
fn release_year() -> i64 {
    CivilTime::from_unix_secs(RELEASE_EPOCH_SECS).year
}

/// Resolve a chip's two-digit year into the full year the plausibility window
/// admits.
///
/// A calendar chip with no century register (PCF8523, PCF85063A) or whose
/// century field is not trustworthy stores only `00..=99`, which names one
/// year in every hundred. Exactly one of them lies inside the window the wall
/// clock already validates against — `[release year, release year + 100)` —
/// so that is the one meant. Returns `None` for a `yy` outside `0..=99`.
///
/// This is not a guess dressed up as arithmetic: the window is the same fixed
/// validation bound every other wall-clock reading is judged against, so a
/// year it resolves is one the clock would have accepted anyway.
#[must_use]
pub fn resolve_two_digit_year(yy: u8) -> Option<i64> {
    if yy > 99 {
        return None;
    }
    let base = release_year();
    let base_yy = base.rem_euclid(100);
    Some(base + (i64::from(yy) + 100 - base_yy).rem_euclid(100))
}

/// What a driver can honestly say about its chip.
///
/// Read live rather than declared once, because [`oscillator_stopped`] is a
/// register bit that can be set at any power-on, not a build-time fact.
///
/// [`oscillator_stopped`]: Self::oscillator_stopped
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct RtcStatus {
    /// The granularity the chip actually keeps: one second for a calendar
    /// register block, one nanosecond for the Goldfish counter. A consumer
    /// uses it to decide how much of a reading to believe, never to round a
    /// value the driver already returned.
    pub precision: Duration64,
    /// `true` when the counter is kept running across a power cycle by a
    /// battery or supercapacitor the board actually fits. A chip that only
    /// runs while the board is powered reports `false`, so a consumer knows
    /// the reading is no older than this boot.
    pub battery_backed: bool,
    /// `true` when the chip reports that its oscillator stopped, or that its
    /// clock-integrity flag is set, since the counter was last written — the
    /// value it holds is then meaningless. [`Rtc::read`] answers `Ok(None)`
    /// in that state; the flag is surfaced separately so a consumer can say
    /// *why* it has no time.
    pub oscillator_stopped: bool,
}

/// Trait every real-time-clock driver implements.
///
/// # Capabilities
///
/// Methods are gated by ownership of the
/// [`DriverHandle`](crate::driver::DriverHandle) (load-time
/// [`CapabilityId::DRV_LOAD`](crate::CapabilityId::DRV_LOAD)) and, over IPC,
/// by the endpoint's own required capabilities ([`crate::rtc_ipc`]). The
/// class declares no clock capability of its own: reading a chip is not
/// setting the machine clock, and no RTC driver holds
/// [`CapabilityId::TIME_SET`](crate::CapabilityId::TIME_SET).
pub trait Rtc {
    /// What the chip can currently vouch for.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] if the register access itself failed.
    fn status(&mut self) -> Result<RtcStatus, DriverError>;

    /// The instant the chip holds, or `Ok(None)` when it cannot vouch for one
    /// — its oscillator stopped, its clock-integrity flag is set, its update
    /// window never settled, or its fields are not a real calendar date.
    ///
    /// `Ok(None)` is the honest answer for a board whose backup cell is flat,
    /// which is an ordinary state and not a failure; a driver must never
    /// substitute an epoch, a build date, or any other fabricated instant.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] if the register access itself failed.
    fn read(&mut self) -> Result<Option<Time64>, DriverError>;

    /// Write `time` to the chip, clearing any oscillator-stopped or
    /// clock-integrity flag so a later [`read`](Self::read) can vouch for it.
    ///
    /// The sub-second part of `time` is discarded by a chip whose precision
    /// is coarser than it; [`RtcStatus::precision`] declares which.
    ///
    /// # Errors
    ///
    /// * [`DriverError::OutOfRange`] if `time` is outside the range the
    ///   chip's registers can hold — never a truncated or wrapped write.
    /// * [`DriverError::Unsupported`] if the chip is read-only.
    /// * [`DriverError::DeviceFault`] if the register access itself failed.
    fn set(&mut self, time: Time64) -> Result<(), DriverError>;
}

#[cfg(test)]
mod tests {
    use super::{
        bcd_to_bin, bin_to_bcd, hour_from_twelve, release_year, resolve_two_digit_year,
        twelve_from_hour,
    };

    #[test]
    fn bcd_round_trips_every_representable_value() {
        for bin in 0u8..=99 {
            let bcd = bin_to_bcd(bin).expect("in range");
            assert_eq!(bcd_to_bin(bcd), Some(bin), "round trip of {bin}");
        }
    }

    #[test]
    fn bcd_encodes_the_documented_nibble_layout() {
        assert_eq!(bin_to_bcd(0), Some(0x00));
        assert_eq!(bin_to_bcd(9), Some(0x09));
        assert_eq!(bin_to_bcd(10), Some(0x10));
        assert_eq!(bin_to_bcd(59), Some(0x59));
        assert_eq!(bin_to_bcd(99), Some(0x99));
    }

    #[test]
    fn a_non_decimal_nibble_is_refused_rather_than_reinterpreted() {
        for bcd in [0xAAu8, 0x0A, 0xA0, 0x1F, 0xF1, 0xFF] {
            assert_eq!(bcd_to_bin(bcd), None, "{bcd:#04x} must not decode");
        }
        // Every byte whose nibbles are both decimal digits *does* decode, and
        // nothing else does.
        let decodable = (0u8..=u8::MAX).filter(|b| bcd_to_bin(*b).is_some()).count();
        assert_eq!(decodable, 100);
    }

    #[test]
    fn a_value_two_digits_cannot_hold_is_refused() {
        assert_eq!(bin_to_bcd(100), None);
        assert_eq!(bin_to_bcd(u8::MAX), None);
    }

    #[test]
    fn the_twelve_hour_clock_round_trips_every_hour_of_the_day() {
        for hour in 0u8..24 {
            let (twelve, pm) = twelve_from_hour(hour).expect("in range");
            assert!((1..=12).contains(&twelve), "{hour} spells {twelve}");
            assert_eq!(hour_from_twelve(twelve, pm), Some(hour));
        }
        // The two ends a 12-hour clock gets wrong if written by hand.
        assert_eq!(twelve_from_hour(0), Some((12, false)));
        assert_eq!(twelve_from_hour(12), Some((12, true)));
        assert_eq!(hour_from_twelve(12, false), Some(0));
        assert_eq!(hour_from_twelve(12, true), Some(12));
    }

    #[test]
    fn an_hour_field_outside_its_range_is_refused() {
        assert_eq!(hour_from_twelve(0, false), None);
        assert_eq!(hour_from_twelve(13, false), None);
        assert_eq!(hour_from_twelve(u8::MAX, true), None);
        assert_eq!(twelve_from_hour(24), None);
        assert_eq!(twelve_from_hour(u8::MAX), None);
    }

    #[test]
    fn two_digit_years_resolve_inside_the_plausibility_window() {
        let base = release_year();
        for yy in 0u8..=99 {
            let year = resolve_two_digit_year(yy).expect("in range");
            assert_eq!(year.rem_euclid(100), i64::from(yy), "{yy} keeps its digits");
            assert!(
                (base..base + 100).contains(&year),
                "{yy} resolved to {year}, outside [{base}, {})",
                base + 100
            );
        }
        // The window is half-open at the base year, so the base's own two
        // digits resolve to the base itself rather than a century later.
        let base_yy = u8::try_from(base.rem_euclid(100)).expect("0..=99");
        assert_eq!(resolve_two_digit_year(base_yy), Some(base));
    }

    #[test]
    fn a_year_field_outside_two_digits_is_refused() {
        assert_eq!(resolve_two_digit_year(100), None);
        assert_eq!(resolve_two_digit_year(u8::MAX), None);
    }
}
