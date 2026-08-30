//! ARM PrimeCell PL031 real-time-clock driver (`arm,pl031`).
//!
//! The PL031 is a 32-bit free-running seconds counter with a load register
//! and an enable bit — no calendar registers, no BCD, no century. Its count
//! is seconds since the Unix epoch on every platform TAIRiX meets it on (the
//! aarch64 `virt` board, which seeds it from the host clock at reset), so the
//! decode is a widening of one register read.
//!
//! # Public surface
//!
//! [`register`] is the driver entry point every driver crate exposes.
//! [`Pl031`] is public so the `Run` binary can construct it over its granted
//! register window; afterwards the service reaches it only through the
//! [`Rtc`] class trait.
//!
//! # What it can and cannot vouch for
//!
//! The counter runs only while `RTCCR`'s start bit is set — write-once, and
//! zero out of reset on real silicon — so bring-up sets it and
//! [`Pl031::read`] answers `Ok(None)` if the bit stays clear. That is the
//! only health signal a bare counter has: the part carries no
//! oscillator-stopped flag and the device tree says nothing about a backup
//! cell, so [`RtcStatus::battery_backed`] is reported `false` rather than
//! claiming a persistence the driver cannot demonstrate. Judging whether the
//! *value* is a believable wall time is clock policy and belongs to the
//! process that sets the clock, not here.
//!
//! Reference: ARM PrimeCell Real Time Clock (PL031) Technical Reference
//! Manual, DDI 0224.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use tairix_abi::driver::rtc::{Rtc, RtcStatus};
use tairix_abi::time::{Duration64, Time64};
use tairix_abi::{
    CapabilityId, DriverBindKey, DriverError, DriverHandle, DriverHost, HwMatchKey, RegisterWindow,
};

#[cfg(test)]
mod tests;

/// Per-driver `DriverHandle` marker returned by [`register`], mirroring the
/// convention every driver crate uses: the host re-issues its own host-local
/// handle when binding the driver, and this constant is the on-the-wire
/// signal that the load-time gate cleared. The bytes spell `"PL31"`.
const REGISTER_HANDLE_MARKER: u64 = 0x504C_3331_0000_0001;

/// Device-tree `compatible` string this driver binds to.
pub const PL031_COMPATIBLE: &[u8] = b"arm,pl031";

/// The bind priority [`BIND_KEYS`] carries. An exact `compatible`-string
/// match ranks above a generic class-wildcard driver.
const BIND_PRIORITY: u16 = 10;

/// The driver's canonical bind table — the single source both the installed
/// bundle's signed manifest and the autoload match are built from, so the
/// match data can never drift from the driver.
pub const BIND_KEYS: &[DriverBindKey] = &[DriverBindKey::new(
    BIND_PRIORITY,
    match HwMatchKey::compatible(PL031_COMPATIBLE) {
        Ok(key) => key,
        // Unreachable: the literal is well within `HW_COMPATIBLE_MAX`. A
        // too-long literal would be a compile-time const-eval error here,
        // never a runtime panic.
        Err(_) => panic!("compatible string fits HW_COMPATIBLE_MAX"),
    },
)];

/// Byte length of the register block the driver touches. The PrimeCell
/// identification registers sit at the top of a 4 KiB page, so a granted
/// window is a page; the driver reads only the first four registers.
pub const REGISTER_BLOCK_LEN: usize = 0x1000;

/// `RTCDR`: the current counter value, read-only.
const RTCDR: usize = 0x000;
/// `RTCLR`: the load register — a write sets the counter.
const RTCLR: usize = 0x008;
/// `RTCCR`: the control register.
const RTCCR: usize = 0x00C;
/// `RTCCR` bit 0, `RTCStart`: the counter runs while it is set. Write-once,
/// and zero out of reset on real silicon.
const RTCCR_START: u32 = 1 << 0;

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

/// A PL031 reached through its mapped register window.
pub struct Pl031 {
    regs: RegisterWindow,
}

impl Pl031 {
    /// Bind the driver to the counter in `regs` and start it.
    ///
    /// Setting `RTCCR`'s start bit is the documented bring-up step: the bit
    /// is write-once and reset-clear, so a counter nobody has started is not
    /// counting. A part that already has it set is left alone, and one that
    /// refuses to start is not treated as an error here — [`Self::read`]
    /// reports it as a chip that cannot vouch for a time.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] if the window is too small to hold the
    /// register block, so a mis-provisioned grant fails closed rather than
    /// reading whatever lies at offset zero.
    pub fn new(regs: RegisterWindow) -> Result<Self, DriverError> {
        let mut rtc = Self { regs };
        let control = rtc.read_reg(RTCCR)?;
        if control & RTCCR_START == 0 {
            rtc.write_reg(RTCCR, control | RTCCR_START)?;
        }
        Ok(rtc)
    }

    /// Read one register, mapping a bounds or alignment refusal to the
    /// class-level fault so a short grant can never read past its window.
    fn read_reg(&self, offset: usize) -> Result<u32, DriverError> {
        self.regs
            .read_u32(offset)
            .map_err(|_| DriverError::DeviceFault)
    }

    /// Write one register, with the same fail-closed bounds mapping.
    fn write_reg(&mut self, offset: usize, value: u32) -> Result<(), DriverError> {
        self.regs
            .write_u32(offset, value)
            .map_err(|_| DriverError::DeviceFault)
    }

    /// `true` when the counter is running.
    fn running(&self) -> Result<bool, DriverError> {
        Ok(self.read_reg(RTCCR)? & RTCCR_START != 0)
    }
}

impl Rtc for Pl031 {
    fn status(&mut self) -> Result<RtcStatus, DriverError> {
        Ok(RtcStatus {
            precision: Duration64::from_secs(1),
            battery_backed: false,
            oscillator_stopped: !self.running()?,
        })
    }

    fn read(&mut self) -> Result<Option<Time64>, DriverError> {
        if !self.running()? {
            return Ok(None);
        }
        Ok(Some(Time64::from_secs(i64::from(self.read_reg(RTCDR)?))))
    }

    fn set(&mut self, time: Time64) -> Result<(), DriverError> {
        // The counter is an unsigned 32-bit seconds count, so it holds
        // 1970-01-01 through 2106-02-07 and nothing else. An instant outside
        // that is refused whole rather than wrapped or clamped into a
        // different time. The sub-second part is dropped, which the declared
        // one-second precision already states.
        let secs = u32::try_from(time.secs()).map_err(|_| DriverError::OutOfRange)?;
        self.write_reg(RTCLR, secs)?;
        // A load on a stopped counter would sit there frozen, so make sure
        // the start bit is set — the same write-once bit bring-up sets.
        let control = self.read_reg(RTCCR)?;
        if control & RTCCR_START == 0 {
            self.write_reg(RTCCR, control | RTCCR_START)?;
        }
        Ok(())
    }
}
