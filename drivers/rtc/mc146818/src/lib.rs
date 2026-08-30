//! Motorola MC146818-compatible PC CMOS real-time-clock driver
//! (`motorola,mc146818`).
//!
//! The chip every PC-compatible machine carries: a battery-backed calendar
//! register block reached not through a window but through a two-port
//! index/data pair — write a register index to the first port, read or write
//! its byte at the second. It is the calendar family's reference part, so the
//! BCD codec and the two-digit-year window come from the shared class module
//! and only the register map, the update-window handshake, and the format
//! bits live here.
//!
//! # Public surface
//!
//! [`register`] is the driver entry point every driver crate exposes.
//! [`Mc146818`] is public so the `Run` binary can construct it over its
//! granted port range; afterwards the service reaches it only through the
//! [`Rtc`] class trait. [`CmosPorts`] is the access seam the binary
//! implements over the capability-gated port traps and a host test
//! implements over a model of the chip.
//!
//! # Reading across the update window
//!
//! The chip updates its own registers once a second and does not
//! double-buffer them, so a read landing in that window can compose a second
//! from before a carry with a minute from after it. Status A's
//! update-in-progress bit marks the window, and the defence is the standard
//! double read: probe the bit clear, read the whole block, read it again, and
//! accept only two blocks that agree — agreement means no tick fell between
//! them.
//!
//! # The century register is not read
//!
//! Register `0x32` holds a century on most modern chipsets, but *whether* it
//! does is declared by the ACPI FADT, which this driver never sees. Reading
//! it unconditionally would put a firmware byte of unknown meaning into the
//! year, so the two-digit year is resolved through the shared plausibility
//! window instead — a year that window admits is one the wall clock would
//! have accepted anyway.
//!
//! Reference: Motorola MC146818A datasheet (the register map, the UIP timing,
//! and the Register B format bits), as carried forward by every PC chipset
//! and by QEMU's `hw/rtc/mc146818rtc.c`.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use tairix_abi::driver::rtc::{bcd_to_bin, bin_to_bcd, resolve_two_digit_year, Rtc, RtcStatus};
use tairix_abi::time::{CivilTime, Duration64, Time64};
use tairix_abi::{CapabilityId, DriverBindKey, DriverError, DriverHandle, DriverHost, HwMatchKey};

#[cfg(test)]
mod tests;

/// Per-driver `DriverHandle` marker returned by [`register`], mirroring the
/// convention every driver crate uses: the host re-issues its own host-local
/// handle when binding the driver, and this constant is the on-the-wire
/// signal that the load-time gate cleared. The bytes spell `"CMOS"`.
const REGISTER_HANDLE_MARKER: u64 = 0x434D_4F53_0000_0001;

/// Device-tree `compatible` string this driver binds to — the Linux
/// devicetree binding name for the part, so a discovery source that already
/// speaks that vocabulary needs no translation.
pub const MC146818_COMPATIBLE: &[u8] = b"motorola,mc146818";

/// The bind priority [`BIND_KEYS`] carries. An exact `compatible`-string
/// match ranks above a generic class-wildcard driver.
const BIND_PRIORITY: u16 = 10;

/// The driver's canonical bind table — the single source both the installed
/// bundle's signed manifest and the autoload match are built from, so the
/// match data can never drift from the driver.
pub const BIND_KEYS: &[DriverBindKey] = &[DriverBindKey::new(
    BIND_PRIORITY,
    match HwMatchKey::compatible(MC146818_COMPATIBLE) {
        Ok(key) => key,
        // Unreachable: the literal is well within `HW_COMPATIBLE_MAX`. A
        // too-long literal would be a compile-time const-eval error here,
        // never a runtime panic.
        Err(_) => panic!("compatible string fits HW_COMPATIBLE_MAX"),
    },
)];

/// Ports of I/O space the chip's index/data pair occupies. A granted range
/// shorter than this cannot address the chip at all.
pub const PORT_RANGE_LEN: u16 = 2;

/// Offset of the index port within the granted range: a write here selects
/// the register the data port then addresses.
const INDEX_PORT: u16 = 0;
/// Offset of the data port within the granted range.
const DATA_PORT: u16 = 1;

/// Register indices of the calendar fields and the three status registers
/// this driver reads. The gaps hold the alarm registers, which it never
/// touches.
const REG_SECOND: u8 = 0x00;
const REG_MINUTE: u8 = 0x02;
const REG_HOUR: u8 = 0x04;
const REG_DAY: u8 = 0x07;
const REG_MONTH: u8 = 0x08;
const REG_YEAR: u8 = 0x09;
const REG_STATUS_A: u8 = 0x0A;
const REG_STATUS_B: u8 = 0x0B;
const REG_STATUS_D: u8 = 0x0D;

/// Status A bit 7, `UIP`: an update is in progress or imminent, so the
/// calendar registers may be mid-carry.
const STATUS_A_UIP: u8 = 1 << 7;

/// Status B bit 7, `SET`: while it is high the chip does not advance its
/// calendar registers, so a multi-register write cannot be overtaken.
const STATUS_B_SET: u8 = 1 << 7;

/// Status B bit 2, `DM`: the calendar fields are plain binary rather than
/// packed BCD.
const STATUS_B_BINARY: u8 = 1 << 2;

/// Status B bit 1: the hours field is 24-hour rather than 12-hour with a PM
/// flag in bit 7.
const STATUS_B_24_HOUR: u8 = 1 << 1;

/// Status D bit 7, `VRT`: the backup cell has held the RAM and time. Clear
/// means the cell has failed and every calendar register is meaningless.
const STATUS_D_VALID_RAM: u8 = 1 << 7;

/// Hours-register bit 7 in 12-hour mode: the PM flag, which is not part of
/// the hour value and must be masked off before decoding it.
const HOUR_PM: u8 = 1 << 7;

/// Status A reads spent waiting for one update window to close.
///
/// The chip holds `UIP` for at most about 2 ms once a second, and each probe
/// is a whole index/data access (a microsecond or so on the ISA bus, more
/// through the port trap), so this covers the window several times over
/// either way. A window that never closes is a broken chip, and no number of
/// further attempts changes that, so the read gives up rather than spinning:
/// the sanctioned bounded hardware handshake, not a wait for work to arrive.
const UIP_PROBES: u32 = 4_096;

/// Block reads spent looking for two that agree.
///
/// Sized to the fact it defends against rather than to a clock rate: a
/// disagreeing pair means one tick fell between the two reads, and a tick
/// comes once a second while a block read takes tens of microseconds — so a
/// healthy chip needs one retry at worst and a handful is already generous.
/// Kept separate from [`UIP_PROBES`] because a block read costs six times a
/// `UIP` probe, so one budget covering both would make a chip that never
/// settles enormously more expensive than the window it is waiting on.
const AGREEMENT_ATTEMPTS: u32 = 8;

/// Byte-wide access seam for the chip's index/data port pair.
///
/// Every access the [`Mc146818`] decode makes goes through this trait, so the
/// update-window handshake and the format decode are proven host-side against
/// a model of the chip. `offset` is relative to the driver's granted port
/// range, never an absolute I/O port: the driver therefore names no fixed
/// address and cannot reach outside what its matched node requested.
///
/// Both methods take `&mut self` because a model must represent the index
/// latch a write installs and a `UIP` bit that evolves as the driver probes
/// it.
pub trait CmosPorts {
    /// Read one byte from the port at `offset` within the granted range.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] if the access falls outside the granted
    /// range or the transfer was refused.
    fn read(&mut self, offset: u16) -> Result<u8, DriverError>;

    /// Write `value` to the port at `offset` within the granted range.
    ///
    /// # Errors
    ///
    /// As [`read`](Self::read).
    fn write(&mut self, offset: u16, value: u8) -> Result<(), DriverError>;
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

/// The encoding Register B declares the calendar fields are in.
///
/// Read afresh on every access rather than latched at bring-up: firmware or
/// another owner can have programmed either bit, and the driver honours what
/// it finds instead of imposing a format.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Format {
    binary: bool,
    twenty_four_hour: bool,
}

impl Format {
    const fn from_status_b(status_b: u8) -> Self {
        Self {
            binary: status_b & STATUS_B_BINARY != 0,
            twenty_four_hour: status_b & STATUS_B_24_HOUR != 0,
        }
    }

    /// Decode one calendar field in this format.
    fn field(self, raw: u8) -> Option<u8> {
        if self.binary {
            Some(raw)
        } else {
            bcd_to_bin(raw)
        }
    }

    /// Encode one calendar field in this format.
    fn encode_field(self, value: u8) -> Option<u8> {
        if self.binary {
            Some(value)
        } else {
            bin_to_bcd(value)
        }
    }

    /// Decode the hours register to `0..=23`.
    ///
    /// In 12-hour mode bit 7 is the PM flag rather than part of the value, so
    /// it is masked off before the digits are decoded; midnight is stored as
    /// 12 AM and noon as 12 PM, which is why 12 maps to 0 before the PM
    /// offset is applied.
    fn hour(self, raw: u8) -> Option<u8> {
        if self.twenty_four_hour {
            let hour = self.field(raw)?;
            (hour < 24).then_some(hour)
        } else {
            let pm = raw & HOUR_PM != 0;
            let twelve = self.field(raw & !HOUR_PM)?;
            if !(1..=12).contains(&twelve) {
                return None;
            }
            let hour = twelve % 12;
            Some(if pm { hour + 12 } else { hour })
        }
    }

    /// Encode `hour` (`0..=23`) into the hours register.
    fn encode_hour(self, hour: u8) -> Option<u8> {
        if hour > 23 {
            return None;
        }
        if self.twenty_four_hour {
            return self.encode_field(hour);
        }
        let twelve = match hour % 12 {
            0 => 12,
            other => other,
        };
        let pm = if hour >= 12 { HOUR_PM } else { 0 };
        Some(self.encode_field(twelve)? | pm)
    }
}

/// One read of the whole calendar block, as raw register bytes.
///
/// Undecoded on purpose: the double read compares what the chip actually
/// held, so a decode that happened to map two different register images onto
/// one instant could not hide a torn read.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Calendar {
    second: u8,
    minute: u8,
    hour: u8,
    day: u8,
    month: u8,
    year: u8,
}

/// The two digits the chip stores for `year`, or `None` when the plausibility
/// window would resolve those digits to a different year — a write the chip
/// could only report back as another century is refused whole.
fn two_digit_year(year: i64) -> Option<u8> {
    let yy = u8::try_from(year.rem_euclid(100)).ok()?;
    (resolve_two_digit_year(yy) == Some(year)).then_some(yy)
}

/// An MC146818-compatible CMOS clock reached through its granted port pair.
pub struct Mc146818<P: CmosPorts> {
    ports: P,
}

impl<P: CmosPorts> Mc146818<P> {
    /// Bind the driver to the chip reachable through `ports`.
    ///
    /// Performs **no** I/O: the chip is left in whatever format and state
    /// firmware programmed, and the first [`read`](Rtc::read) is the first
    /// access. There is no bring-up sequence to run — the oscillator runs off
    /// the backup cell and has no enable bit this driver owns.
    #[must_use]
    pub fn new(ports: P) -> Self {
        Self { ports }
    }

    /// Read one register through the index/data pair.
    fn read_reg(&mut self, index: u8) -> Result<u8, DriverError> {
        self.ports.write(INDEX_PORT, index)?;
        self.ports.read(DATA_PORT)
    }

    /// Write one register through the index/data pair.
    fn write_reg(&mut self, index: u8, value: u8) -> Result<(), DriverError> {
        self.ports.write(INDEX_PORT, index)?;
        self.ports.write(DATA_PORT, value)
    }

    /// `true` while the backup cell vouches for the RAM and time.
    fn valid_ram(&mut self) -> Result<bool, DriverError> {
        Ok(self.read_reg(REG_STATUS_D)? & STATUS_D_VALID_RAM != 0)
    }

    /// One read of the whole calendar block, in register order.
    fn read_calendar(&mut self) -> Result<Calendar, DriverError> {
        Ok(Calendar {
            second: self.read_reg(REG_SECOND)?,
            minute: self.read_reg(REG_MINUTE)?,
            hour: self.read_reg(REG_HOUR)?,
            day: self.read_reg(REG_DAY)?,
            month: self.read_reg(REG_MONTH)?,
            year: self.read_reg(REG_YEAR)?,
        })
    }

    /// Wait for the current update window to close.
    ///
    /// `Ok(Some(waited))` once `UIP` is clear, where `waited` reports whether
    /// the bit was ever seen set — which means an update has just happened.
    /// `Ok(None)` if the window never closed within its budget.
    fn await_update_window(&mut self) -> Result<Option<bool>, DriverError> {
        let mut waited = false;
        for _ in 0..UIP_PROBES {
            if self.read_reg(REG_STATUS_A)? & STATUS_A_UIP == 0 {
                return Ok(Some(waited));
            }
            waited = true;
        }
        Ok(None)
    }

    /// The calendar block, read twice outside the update window and agreeing,
    /// or `Ok(None)` when the chip never offered such a pair.
    ///
    /// An update seen between two blocks discards the earlier one rather than
    /// merely skipping a read: a block from before the update must never be
    /// paired with one from after it, or the pair could agree across a carry.
    fn settled_calendar(&mut self) -> Result<Option<Calendar>, DriverError> {
        let mut previous: Option<Calendar> = None;
        for _ in 0..AGREEMENT_ATTEMPTS {
            match self.await_update_window()? {
                // A stuck window is terminal, so retrying the block read
                // would only spend the budget re-discovering it.
                None => return Ok(None),
                Some(true) => previous = None,
                Some(false) => {}
            }
            let current = self.read_calendar()?;
            if previous == Some(current) {
                return Ok(Some(current));
            }
            previous = Some(current);
        }
        Ok(None)
    }
}

/// Decode a settled register block into an instant, or `None` when the
/// fields are not a real calendar date.
fn decode(calendar: Calendar, format: Format) -> Option<Time64> {
    CivilTime {
        year: resolve_two_digit_year(format.field(calendar.year)?)?,
        month: u32::from(format.field(calendar.month)?),
        day: u32::from(format.field(calendar.day)?),
        hour: u32::from(format.hour(calendar.hour)?),
        minute: u32::from(format.field(calendar.minute)?),
        second: u32::from(format.field(calendar.second)?),
    }
    .to_time64()
}

/// Encode a civil instant into the register block, or `None` when a field
/// does not fit the chip's format.
fn encode(civil: CivilTime, format: Format) -> Option<Calendar> {
    let yy = two_digit_year(civil.year)?;
    Some(Calendar {
        second: format.encode_field(u8::try_from(civil.second).ok()?)?,
        minute: format.encode_field(u8::try_from(civil.minute).ok()?)?,
        hour: format.encode_hour(u8::try_from(civil.hour).ok()?)?,
        day: format.encode_field(u8::try_from(civil.day).ok()?)?,
        month: format.encode_field(u8::try_from(civil.month).ok()?)?,
        year: format.encode_field(yy)?,
    })
}

impl<P: CmosPorts> Rtc for Mc146818<P> {
    fn status(&mut self) -> Result<RtcStatus, DriverError> {
        Ok(RtcStatus {
            precision: Duration64::from_secs(1),
            // The part is battery-backed by design, and Register D's
            // valid-RAM bit is the cell's own attestation of it.
            battery_backed: true,
            oscillator_stopped: !self.valid_ram()?,
        })
    }

    fn read(&mut self) -> Result<Option<Time64>, DriverError> {
        if !self.valid_ram()? {
            return Ok(None);
        }
        let format = Format::from_status_b(self.read_reg(REG_STATUS_B)?);
        let Some(calendar) = self.settled_calendar()? else {
            return Ok(None);
        };
        Ok(decode(calendar, format))
    }

    fn set(&mut self, time: Time64) -> Result<(), DriverError> {
        // Register B first, and its format is honoured rather than replaced:
        // reprogramming the chip's encoding would change what every other
        // owner of it reads.
        let status_b = self.read_reg(REG_STATUS_B)?;
        let calendar = encode(
            CivilTime::from_time64(time),
            Format::from_status_b(status_b),
        )
        .ok_or(DriverError::OutOfRange)?;

        self.write_reg(REG_STATUS_B, status_b | STATUS_B_SET)?;
        let written = self
            .write_reg(REG_SECOND, calendar.second)
            .and_then(|()| self.write_reg(REG_MINUTE, calendar.minute))
            .and_then(|()| self.write_reg(REG_HOUR, calendar.hour))
            .and_then(|()| self.write_reg(REG_DAY, calendar.day))
            .and_then(|()| self.write_reg(REG_MONTH, calendar.month))
            .and_then(|()| self.write_reg(REG_YEAR, calendar.year));
        // Released whatever the field writes did: a chip left with `SET` high
        // would sit frozen, so a mid-write fault must not cost the machine
        // its running clock.
        let released = self.write_reg(REG_STATUS_B, status_b & !STATUS_B_SET);
        written.and(released)
    }
}
