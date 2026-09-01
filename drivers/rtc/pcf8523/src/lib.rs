//! NXP PCF8523 real-time-clock driver (`nxp,pcf8523`).
//!
//! The PCF8523 is the I²C calendar chip at bus address `0x68` that Raspberry
//! Pi's own `i2c-rtc` overlay documentation names. Its calendar block runs
//! `0x03..=0x09`: seconds with a clock-integrity flag in the top bit, then
//! minutes, hours, days, weekdays, months, and a two-digit year.
//!
//! # Public surface
//!
//! [`register`] is the driver entry point every driver crate exposes.
//! [`Pcf8523`] is public so the `Run` binary can construct it over the
//! transfer port its endpoint grant resolves to; afterwards the service
//! reaches it only through the [`Rtc`] class trait.
//!
//! # What it can and cannot vouch for
//!
//! The seconds register's top bit is the chip's own clock-integrity flag: it
//! is set whenever the oscillator has stopped — a flat backup cell, a first
//! power-on, a brown-out — and stays set until something clears it. While it
//! is set the calendar registers mean nothing, so [`Pcf8523::read`] answers
//! `Ok(None)`; [`Pcf8523::set`] clears it after a successful write, so the
//! two agree.
//!
//! Bring-up leaves the chip in **24-hour** mode: the mode lives in a control
//! register rather than in the hours field, so a driver that did not set it
//! would have to trust whatever a previous owner left there. The read path
//! still honours a 12-hour field, because the mode bit and the field can
//! legitimately disagree in the window before the first write.
//!
//! The part has a backup-cell input and a switch-over circuit, but the cell
//! is an optional accessory a board may not fit — so
//! [`RtcStatus::battery_backed`] is read from the control register's
//! battery-switch-over setting rather than asserted from the part number. A
//! board that disabled switch-over is honestly reported as not backed.
//!
//! The two-digit year names one year in every hundred and resolves through
//! the class's shared `resolve_two_digit_year`, against the same fixed window
//! the wall clock validates every reading against. The chip has no century
//! register at all.
//!
//! Reference: NXP PCF8523 data sheet, §8 (register overview).

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
/// convention every driver crate uses. The bytes spell `"P852"`.
const REGISTER_HANDLE_MARKER: u64 = 0x5038_3532_0000_0001;

/// The PCF8523's bus address. Fixed in the part — it has no address pins.
pub const PCF8523_BUS_ADDRESS: u8 = 0x68;

/// Device-tree `compatible` string this driver binds to.
pub const PCF8523_COMPATIBLE: &[u8] = b"nxp,pcf8523";

/// The bind priority [`BIND_KEYS`] carries. An exact `compatible`-string
/// match ranks above a generic class-wildcard driver.
const BIND_PRIORITY: u16 = 10;

/// The driver's canonical bind table — the single source both the installed
/// bundle's signed manifest and the autoload match are built from.
pub const BIND_KEYS: &[DriverBindKey] = &[DriverBindKey::new(
    BIND_PRIORITY,
    match HwMatchKey::compatible(PCF8523_COMPATIBLE) {
        Ok(key) => key,
        // Unreachable: the literal is well within `HW_COMPATIBLE_MAX`. A
        // too-long literal would be a compile-time const-eval error here,
        // never a runtime panic.
        Err(_) => panic!("compatible string fits HW_COMPATIBLE_MAX"),
    },
)];

/// Control register 1, holding the hour-format and stop bits.
const REG_CONTROL_1: u8 = 0x00;
/// Control register 3, holding the power-management (battery switch-over)
/// mode.
const REG_CONTROL_3: u8 = 0x02;
/// First register of the calendar block.
const REG_SECONDS: u8 = 0x03;
/// Bytes in the calendar block: seconds, minutes, hours, days, weekdays,
/// months, years.
const CALENDAR_LEN: usize = 7;

/// Control 1 bit 3: the hours field is a 12-hour reading with a PM flag
/// rather than a 24-hour one.
const CONTROL_1_TWELVE_HOUR: u8 = 1 << 3;
/// Control 1 bit 5: the RTC time circuits are stopped.
const CONTROL_1_STOP: u8 = 1 << 5;

/// Control 3 bits 7:5: the power-management mode. The three high values
/// (`0b101`, `0b110`, `0b111`) all disable battery switch-over, so a board
/// wired without a cell reports no persistence.
const CONTROL_3_PM_MASK: u8 = 0b1110_0000;
/// The lowest power-management encoding that disables switch-over.
const CONTROL_3_PM_NO_BATTERY: u8 = 0b1010_0000;

/// Seconds register bit 7: the clock-integrity flag. Set means the
/// oscillator stopped and the calendar means nothing.
const SECONDS_INTEGRITY_LOST: u8 = 1 << 7;

/// Hours register bit 5 in 12-hour mode: the PM flag.
const HOUR_PM: u8 = 1 << 5;

/// The chip's counting granularity.
const PRECISION: Duration64 = Duration64::from_secs(1);

/// The NXP PCF8523.
pub struct Pcf8523<P: I2cPort> {
    device: Device<P>,
}

impl<P: I2cPort> Pcf8523<P> {
    /// Bind the driver to the part behind `port` and put it in 24-hour mode
    /// with its time circuits running.
    ///
    /// Both settings live in a control register rather than in the calendar
    /// block, so a driver that read them without setting them would be
    /// trusting whatever a previous owner left there. Read-modify-write, so
    /// the board's other settings survive.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] and its siblings, from the transfer.
    pub fn open(port: P) -> Result<Self, DriverError> {
        let chip = Self {
            device: Device::new(port),
        };
        chip.device.update_one(REG_CONTROL_1, |control| {
            control & !(CONTROL_1_TWELVE_HOUR | CONTROL_1_STOP)
        })?;
        Ok(chip)
    }

    /// Whether the chip reports its clock integrity lost.
    fn integrity_lost(&self) -> Result<bool, DriverError> {
        Ok(self.device.read_one(REG_SECONDS)? & SECONDS_INTEGRITY_LOST != 0)
    }

    /// Decode the hours register to `0..=23`, honouring the mode the control
    /// register declares.
    fn decode_hour(raw: u8, twelve_hour: bool) -> Option<u8> {
        if twelve_hour {
            return hour_from_twelve(bcd_to_bin(raw & !HOUR_PM)?, raw & HOUR_PM != 0);
        }
        let hour = bcd_to_bin(raw & 0x3F)?;
        (hour < 24).then_some(hour)
    }
}

impl<P: I2cPort> Rtc for Pcf8523<P> {
    fn status(&mut self) -> Result<RtcStatus, DriverError> {
        let control_3 = self.device.read_one(REG_CONTROL_3)?;
        Ok(RtcStatus {
            precision: PRECISION,
            // Evidence from the board's own configuration, not from the part
            // number: the cell is an optional accessory.
            battery_backed: control_3 & CONTROL_3_PM_MASK < CONTROL_3_PM_NO_BATTERY,
            oscillator_stopped: self.integrity_lost()?,
        })
    }

    fn read(&mut self) -> Result<Option<Time64>, DriverError> {
        // The mode bit and the field can legitimately disagree in the window
        // before this driver's first write, so the field is decoded by what
        // the chip currently declares rather than by what bring-up asked for.
        let twelve_hour = self.device.read_one(REG_CONTROL_1)? & CONTROL_1_TWELVE_HOUR != 0;
        let mut block = [0u8; CALENDAR_LEN];
        self.device.read(REG_SECONDS, &mut block)?;
        if block[0] & SECONDS_INTEGRITY_LOST != 0 {
            return Ok(None);
        }
        // Every field is refused rather than reinterpreted when its nibbles
        // are not decimal digits, and the composed date is refused when it is
        // not a real calendar instant.
        let Some(second) = bcd_to_bin(block[0] & !SECONDS_INTEGRITY_LOST) else {
            return Ok(None);
        };
        let Some(minute) = bcd_to_bin(block[1] & 0x7F) else {
            return Ok(None);
        };
        let Some(hour) = Self::decode_hour(block[2], twelve_hour) else {
            return Ok(None);
        };
        let Some(day) = bcd_to_bin(block[3] & 0x3F) else {
            return Ok(None);
        };
        let Some(month) = bcd_to_bin(block[5] & 0x1F) else {
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
        // Written in 24-hour form, which bring-up put the chip in.
        let mut block = [0u8; CALENDAR_LEN];
        // Writing the seconds field with the integrity bit clear is what
        // clears it: the chip has no separate acknowledge, so a failed write
        // leaves the flag set and the part honestly unable to vouch.
        block[0] = field(civil.second)?;
        block[1] = field(civil.minute)?;
        block[2] = field(civil.hour)?;
        block[3] = field(civil.day)?;
        // The weekday field is not part of the instant and nothing reads it
        // back; the chip only ever increments it.
        block[4] = 0;
        block[5] = field(civil.month)?;
        block[6] = bin_to_bcd(yy).ok_or(DriverError::OutOfRange)?;
        self.device.write(REG_SECONDS, &block)
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
