//! Maxim DS3231 / DS1307 real-time-clock driver (`maxim,ds3231`).
//!
//! The DS3231 is a temperature-compensated I²C calendar chip at bus address
//! `0x68`. Its `0x00..=0x06` block — seconds, minutes, hours, day of week,
//! date, month with a century bit, and a two-digit year — is the register
//! layout the DS1307 defined, so `maxim,ds1307` binds here too and the same
//! decode serves both.
//!
//! # Public surface
//!
//! [`register`] is the driver entry point every driver crate exposes.
//! [`Ds3231`] is public so the `Run` binary can construct it over the
//! transfer port its endpoint grant resolves to; afterwards the service
//! reaches it only through the [`Rtc`] class trait.
//!
//! # What it can and cannot vouch for
//!
//! The status register's oscillator-stop flag is set by the chip whenever the
//! oscillator has been interrupted — a flat backup cell, a first power-on —
//! and stays set until something clears it. Until it is cleared the counter
//! is meaningless, so [`Ds3231::read`] answers `Ok(None)` rather than
//! reporting whatever the registers happen to hold; [`Ds3231::set`] clears it
//! after a successful write, so the two agree.
//!
//! Judging whether a *decoded* time is a believable wall time is clock policy
//! and belongs to the process that sets the clock. The one exception is the
//! two-digit year, which names one year in every hundred: it resolves through
//! the class's shared `resolve_two_digit_year`, against the same fixed window
//! the wall clock validates every reading against. The century bit is
//! deliberately not used for that — it is a carry the chip toggles when the
//! year field wraps, so it says nothing about which century a freshly
//! powered part is in.
//!
//! The DS3231 carries its own backup-cell input and keeps counting from it,
//! which is the part's whole purpose, so [`RtcStatus::battery_backed`] is
//! reported `true`. A flat cell shows up as the oscillator-stop flag rather
//! than as a claim of persistence.
//!
//! Reference: Maxim DS3231 data sheet (19-5170), register map §Registers.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use tairix_abi::driver::i2c::I2cPort;
use tairix_abi::driver::rtc::{
    bcd_to_bin, bin_to_bcd, hour_from_twelve, resolve_two_digit_year, Rtc, RtcStatus,
};
use tairix_abi::time::{CivilTime, Duration64, Time64};
use tairix_abi::{CapabilityId, DriverBindKey, DriverError, DriverHandle, DriverHost, HwMatchKey};
use tairix_i2c::Device;

#[cfg(test)]
mod tests;

/// Per-driver `DriverHandle` marker returned by [`register`], mirroring the
/// convention every driver crate uses. The bytes spell `"D231"`.
const REGISTER_HANDLE_MARKER: u64 = 0x4432_3331_0000_0001;

/// The DS3231's bus address. Fixed in the part — it has no address pins — so
/// discovery's `reg` and this agree by construction.
pub const DS3231_BUS_ADDRESS: u8 = 0x68;

/// Device-tree `compatible` string of the DS3231.
pub const DS3231_COMPATIBLE: &[u8] = b"maxim,ds3231";

/// Device-tree `compatible` string of the DS1307, whose `0x00..=0x06`
/// calendar block this decode is: the DS3231 kept the layout, so one driver
/// serves both parts.
pub const DS1307_COMPATIBLE: &[u8] = b"maxim,ds1307";

/// The bind priority [`BIND_KEYS`] carries. An exact `compatible`-string
/// match ranks above a generic class-wildcard driver.
const BIND_PRIORITY: u16 = 10;

/// The driver's canonical bind table — the single source both the installed
/// bundle's signed manifest and the autoload match are built from.
pub const BIND_KEYS: &[DriverBindKey] = &[
    DriverBindKey::new(BIND_PRIORITY, compatible_key(DS3231_COMPATIBLE)),
    DriverBindKey::new(BIND_PRIORITY, compatible_key(DS1307_COMPATIBLE)),
];

/// Build a bind key at compile time, so a too-long literal is a const-eval
/// error rather than a runtime panic.
const fn compatible_key(compatible: &[u8]) -> HwMatchKey {
    match HwMatchKey::compatible(compatible) {
        Ok(key) => key,
        Err(_) => panic!("compatible string fits HW_COMPATIBLE_MAX"),
    }
}

/// First register of the calendar block.
const REG_SECONDS: u8 = 0x00;
/// Bytes in the calendar block: seconds, minutes, hours, day of week, date,
/// month, year.
const CALENDAR_LEN: usize = 7;
/// Status register, whose bit 7 is the oscillator-stop flag.
const REG_STATUS: u8 = 0x0F;

/// Status register bit 7: the oscillator has stopped since the counter was
/// last written, so the calendar registers mean nothing.
const STATUS_OSC_STOPPED: u8 = 1 << 7;

/// Hours register bit 6: the field is a 12-hour reading with a PM flag rather
/// than a 24-hour one.
const HOUR_TWELVE: u8 = 1 << 6;
/// Hours register bit 5 in 12-hour mode: the PM flag. In 24-hour mode it is
/// the twenty-hours digit and part of the value.
const HOUR_PM: u8 = 1 << 5;

/// Month register bit 7: the century carry the chip toggles when the year
/// field wraps. Masked off before the month is decoded and never read as a
/// century — see the module docs.
const MONTH_CENTURY: u8 = 1 << 7;

/// The chip's counting granularity.
const PRECISION: Duration64 = Duration64::from_secs(1);

/// The Maxim DS3231 (and the DS1307-compatible block it inherited).
pub struct Ds3231<P: I2cPort> {
    device: Device<P>,
}

impl<P: I2cPort> Ds3231<P> {
    /// Bind the driver to the part behind `port`.
    ///
    /// There is no bring-up step: the chip runs from its backup cell whenever
    /// it can, and its configuration is the board's. A port that cannot reach
    /// the part surfaces at the first read.
    pub const fn new(port: P) -> Self {
        Self {
            device: Device::new(port),
        }
    }

    /// Whether the chip reports its oscillator stopped.
    fn oscillator_stopped(&self) -> Result<bool, DriverError> {
        Ok(self.device.read_one(REG_STATUS)? & STATUS_OSC_STOPPED != 0)
    }

    /// Decode the hours register to `0..=23`, or `None` for a field that is
    /// not a real hour.
    fn decode_hour(raw: u8) -> Option<u8> {
        if raw & HOUR_TWELVE == 0 {
            let hour = bcd_to_bin(raw & !(HOUR_TWELVE | HOUR_PM))?;
            return (hour < 24).then_some(hour);
        }
        let pm = raw & HOUR_PM != 0;
        hour_from_twelve(bcd_to_bin(raw & !(HOUR_TWELVE | HOUR_PM))?, pm)
    }
}

impl<P: I2cPort> Rtc for Ds3231<P> {
    fn status(&mut self) -> Result<RtcStatus, DriverError> {
        Ok(RtcStatus {
            precision: PRECISION,
            // The part exists to keep counting from its own backup cell; a
            // flat one shows up as the oscillator-stop flag, not as a false
            // claim of persistence.
            battery_backed: true,
            oscillator_stopped: self.oscillator_stopped()?,
        })
    }

    fn read(&mut self) -> Result<Option<Time64>, DriverError> {
        if self.oscillator_stopped()? {
            return Ok(None);
        }
        let mut block = [0u8; CALENDAR_LEN];
        self.device.read(REG_SECONDS, &mut block)?;
        // Every field is refused rather than reinterpreted when its nibbles
        // are not decimal digits, and the composed date is refused when it is
        // not a real calendar instant — a corrupt block never yields a
        // plausible-looking time.
        let Some(second) = bcd_to_bin(block[0] & 0x7F) else {
            return Ok(None);
        };
        let Some(minute) = bcd_to_bin(block[1] & 0x7F) else {
            return Ok(None);
        };
        let Some(hour) = Self::decode_hour(block[2]) else {
            return Ok(None);
        };
        let Some(day) = bcd_to_bin(block[4] & 0x3F) else {
            return Ok(None);
        };
        let Some(month) = bcd_to_bin(block[5] & !MONTH_CENTURY) else {
            return Ok(None);
        };
        let Some(yy) = bcd_to_bin(block[6]) else {
            return Ok(None);
        };
        let Some(year) = resolve_two_digit_year(yy) else {
            return Ok(None);
        };
        Ok(CivilTime {
            year,
            month: u32::from(month),
            day: u32::from(day),
            hour: u32::from(hour),
            minute: u32::from(minute),
            second: u32::from(second),
        }
        .to_time64())
    }

    fn set(&mut self, time: Time64) -> Result<(), DriverError> {
        let civil = CivilTime::from_time64(time);
        // The two-digit year is what the registers hold, so an instant
        // outside the window the class resolves it against could not be read
        // back as itself — refuse it whole rather than write a year that
        // reads as some other century.
        let year = civil.year;
        let yy = u8::try_from(year.rem_euclid(100)).map_err(|_| DriverError::OutOfRange)?;
        if resolve_two_digit_year(yy) != Some(year) {
            return Err(DriverError::OutOfRange);
        }
        let field = |value: u32| -> Result<u8, DriverError> {
            let narrow = u8::try_from(value).map_err(|_| DriverError::OutOfRange)?;
            bin_to_bcd(narrow).ok_or(DriverError::OutOfRange)
        };
        // Written in 24-hour form: the chip accepts either, and the reading
        // is unambiguous without a mode bit to agree on.
        let mut block = [0u8; CALENDAR_LEN];
        block[0] = field(civil.second)?;
        block[1] = field(civil.minute)?;
        block[2] = field(civil.hour)?;
        // The day-of-week field is not part of the instant and nothing reads
        // it back; the chip only ever increments it. `1` is the value the
        // data sheet's own range starts at.
        block[3] = 1;
        block[4] = field(civil.day)?;
        block[5] = field(civil.month)?;
        block[6] = bin_to_bcd(yy).ok_or(DriverError::OutOfRange)?;
        self.device.write(REG_SECONDS, &block)?;
        // Only once the counter holds a real time is the stop flag cleared,
        // so a failed write leaves the chip honestly reporting that it cannot
        // vouch for itself.
        self.device
            .update_one(REG_STATUS, |status| status & !STATUS_OSC_STOPPED)
    }
}

/// Driver entry point.
///
/// # Errors
///
/// [`DriverError::PermissionDenied`] if the host did not grant
/// [`CapabilityId::DRV_LOAD`].
///
/// # Capabilities
///
/// Requires [`CapabilityId::DRV_LOAD`]. The driver requests no clock
/// authority: it reads and writes its own chip, and the machine clock is set
/// by the one holder of `CAP_TIME_SET`.
pub fn register(host: &dyn DriverHost) -> Result<DriverHandle, DriverError> {
    if !host.has_capability(CapabilityId::DRV_LOAD) {
        return Err(DriverError::PermissionDenied);
    }
    DriverHandle::from_raw(REGISTER_HANDLE_MARKER)
}
