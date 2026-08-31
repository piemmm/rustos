//! Raspberry Pi real-time-clock driver (`raspberrypi,rpi-rtc`).
//!
//! The Pi 5's clock lives inside the board's power-management IC, not in the
//! chip's MMIO space, so there is no register window to map: the `VideoCore`
//! firmware owns the chip and exposes it as two numbered registers behind the
//! mailbox property channel. This driver is therefore the first RTC in the
//! tree whose sole resource is an IPC path — it maps nothing and its bring-up
//! touches no MMIO.
//!
//! # Public surface
//!
//! [`register`] is the driver entry point every driver crate exposes.
//! [`RpiRtc`] is public so the `Run` binary can construct it over the
//! board-neutral [`MailboxChannel`] its host provides; afterwards the service
//! reaches it only through the [`Rtc`] class trait.
//!
//! # What it can and cannot vouch for
//!
//! The counter is a 32-bit Unix seconds count that reads **zero** until
//! something programs it, which is the state a board with no backup cell
//! comes up in. Zero is therefore the chip's own "never set since it lost
//! power" signal, and [`RpiRtc::read`] answers `Ok(None)` for it rather than
//! reporting 1970 as a wall time; by the same token [`RpiRtc::set`] refuses
//! an instant of exactly zero, so nothing this driver writes can read back as
//! "no time". Whether a *non-zero* value is a believable wall time is clock
//! policy and belongs to the process that sets the clock.
//!
//! The backup cell's voltage register is the only honest evidence the counter
//! survives a power cycle — the battery is an optional accessory on every Pi
//! that has this clock — so it, not a board name, is what
//! [`RtcStatus::battery_backed`] reports.
//!
//! # Firmware that refuses the tag
//!
//! Some firmware revisions answer every property request with the top-level
//! success code while never processing the tag (`raspberrypi/linux` issue
//! 7230). `lib/vcmailbox` requires the per-tag response bit, so such a reply
//! is a fault here rather than a fabricated 1970 reading, and the driver
//! reports the chip unreadable instead of waiting on it — there is nothing to
//! wait for.
//!
//! Reference: the Raspberry Pi firmware property interface
//! (`RPI_FIRMWARE_GET_RTC_REG` / `RPI_FIRMWARE_SET_RTC_REG` and the register
//! selectors), as the `rtc-rpi` driver in the Raspberry Pi Linux tree spells
//! it.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use tairix_abi::driver::mailbox::MailboxChannel;
use tairix_abi::driver::rtc::{Rtc, RtcStatus};
use tairix_abi::time::{Duration64, Time64};
use tairix_abi::{CapabilityId, DriverBindKey, DriverError, DriverHandle, DriverHost, HwMatchKey};
use tairix_vcmailbox::{
    decode_rtc_register_response, decode_rtc_register_write_response, encode_rtc_register_query,
    encode_rtc_register_write, MailboxError, RtcRegister,
};

#[cfg(test)]
mod tests;

/// Per-driver `DriverHandle` marker returned by [`register`], mirroring the
/// convention every driver crate uses: the host re-issues its own host-local
/// handle when binding the driver, and this constant is the on-the-wire
/// signal that the load-time gate cleared. The bytes spell `"RPRT"`.
const REGISTER_HANDLE_MARKER: u64 = 0x5250_5254_0000_0001;

/// Device-tree `compatible` string this driver binds to — the Raspberry Pi
/// binding name for the firmware-mediated clock, so a discovery source that
/// already speaks that vocabulary needs no translation.
pub const RPI_RTC_COMPATIBLE: &[u8] = b"raspberrypi,rpi-rtc";

/// The bind priority [`BIND_KEYS`] carries. An exact `compatible`-string
/// match ranks above a generic class-wildcard driver.
const BIND_PRIORITY: u16 = 10;

/// The driver's canonical bind table — the single source both the installed
/// bundle's signed manifest and the autoload match are built from, so the
/// match data can never drift from the driver.
pub const BIND_KEYS: &[DriverBindKey] = &[DriverBindKey::new(
    BIND_PRIORITY,
    match HwMatchKey::compatible(RPI_RTC_COMPATIBLE) {
        Ok(key) => key,
        // Unreachable: the literal is well within `HW_COMPATIBLE_MAX`. A
        // too-long literal would be a compile-time const-eval error here,
        // never a runtime panic.
        Err(_) => panic!("compatible string fits HW_COMPATIBLE_MAX"),
    },
)];

/// Counter value that means the chip has never been programmed since it lost
/// power. Refused on write as well as disbelieved on read, so the two agree.
const COUNTER_UNSET: u32 = 0;

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

/// The Pi's firmware-mediated clock, reached over a property-mailbox channel.
pub struct RpiRtc<C: MailboxChannel> {
    channel: C,
}

impl<C: MailboxChannel> RpiRtc<C> {
    /// Bind the driver to the clock the firmware behind `channel` owns.
    ///
    /// There is no bring-up step: the chip is already running whenever the
    /// board is powered, and the firmware owns its configuration. A channel
    /// that cannot reach the firmware surfaces at the first read.
    pub const fn new(channel: C) -> Self {
        Self { channel }
    }

    /// Read one of the chip's numbered registers.
    fn read_register(&self, register: RtcRegister) -> Result<u32, DriverError> {
        let mut message = encode_rtc_register_query(register);
        self.channel.exchange(&mut message)?;
        decode_rtc_register_response(register, &message).map_err(MailboxError::as_driver_error)
    }

    /// Write one of the chip's numbered registers.
    fn write_register(&self, register: RtcRegister, value: u32) -> Result<(), DriverError> {
        let mut message = encode_rtc_register_write(register, value);
        self.channel.exchange(&mut message)?;
        decode_rtc_register_write_response(register, &message)
            .map_err(MailboxError::as_driver_error)
    }
}

impl<C: MailboxChannel> Rtc for RpiRtc<C> {
    fn status(&mut self) -> Result<RtcStatus, DriverError> {
        // Read afresh rather than caching the counter from `read`: the trait
        // places no order on the two calls, and a stale flag would report a
        // clock the chip cannot vouch for as sound.
        let counter = self.read_register(RtcRegister::Time)?;
        let backup_mv = self.read_register(RtcRegister::BackupVolts)?;
        Ok(RtcStatus {
            precision: Duration64::from_secs(1),
            battery_backed: backup_mv > 0,
            oscillator_stopped: counter == COUNTER_UNSET,
        })
    }

    fn read(&mut self) -> Result<Option<Time64>, DriverError> {
        let counter = self.read_register(RtcRegister::Time)?;
        if counter == COUNTER_UNSET {
            return Ok(None);
        }
        Ok(Some(Time64::from_secs(i64::from(counter))))
    }

    fn set(&mut self, time: Time64) -> Result<(), DriverError> {
        // The counter is an unsigned 32-bit seconds count, so it holds
        // 1970-01-01 through 2106-02-07 and nothing else; an instant outside
        // that is refused whole rather than wrapped. The epoch second itself
        // is refused too, because the chip cannot distinguish it from never
        // having been set. The sub-second part is dropped, which the declared
        // one-second precision already states.
        let secs = u32::try_from(time.secs()).map_err(|_| DriverError::OutOfRange)?;
        if secs == COUNTER_UNSET {
            return Err(DriverError::OutOfRange);
        }
        self.write_register(RtcRegister::Time, secs)
    }
}
