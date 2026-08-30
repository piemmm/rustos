//! Google Goldfish real-time-clock driver (`google,goldfish-rtc`).
//!
//! The Goldfish RTC is a 64-bit count of nanoseconds since the Unix epoch,
//! addressed as two 32-bit halves. There are no calendar registers, no BCD,
//! no century, and no enable bit: the counter runs whenever the device is
//! present, so the decode is a widening of one register pair.
//!
//! Both directions are ordered low-half-first, and the order is the whole
//! reason the pair is reached through one function each way: reading
//! `TIME_LOW` latches `TIME_HIGH`, and a write commits on the `TIME_HIGH`
//! store, so a reversed access composes two halves either side of a carry.
//!
//! # Public surface
//!
//! [`register`] is the driver entry point every driver crate exposes.
//! [`Goldfish`] is public so the `Run` binary can construct it over its
//! granted register window; afterwards the service reaches it only through
//! the [`Rtc`] class trait.
//!
//! # What it can and cannot vouch for
//!
//! The device models no backup cell and the riscv64 `virt` device tree
//! declares none, so [`RtcStatus::battery_backed`] is reported `false`
//! rather than claiming a persistence the driver cannot demonstrate. The
//! part carries no oscillator-stopped flag either, so the counter is its own
//! health signal: zero is the Unix epoch, which no running machine reports,
//! and [`Goldfish::read`] answers `Ok(None)` for it rather than handing on a
//! value the device has nothing behind. Judging whether a *non-zero* count
//! is a believable wall time is clock policy and belongs to the process that
//! sets the clock, not here.
//!
//! The alarm registers (`ALARM_LOW` through `CLEAR_INTERRUPT`) are left
//! alone: this driver neither arms nor services an alarm, and its one client
//! polls the counter.
//!
//! Reference: the Goldfish RTC as instantiated by QEMU's
//! `hw/rtc/goldfish_rtc.c` and bound by Linux's `drivers/rtc/rtc-goldfish.c`.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use tairix_abi::driver::rtc::{Rtc, RtcStatus};
use tairix_abi::time::{Duration64, Time64, NANOS_PER_SEC};
use tairix_abi::{
    CapabilityId, DriverBindKey, DriverError, DriverHandle, DriverHost, HwMatchKey, RegisterWindow,
};

#[cfg(test)]
mod tests;

/// Per-driver `DriverHandle` marker returned by [`register`], mirroring the
/// convention every driver crate uses: the host re-issues its own host-local
/// handle when binding the driver, and this constant is the on-the-wire
/// signal that the load-time gate cleared. The bytes spell `"GFRT"`.
const REGISTER_HANDLE_MARKER: u64 = 0x4746_5254_0000_0001;

/// Device-tree `compatible` string this driver binds to.
pub const GOLDFISH_COMPATIBLE: &[u8] = b"google,goldfish-rtc";

/// The bind priority [`BIND_KEYS`] carries. An exact `compatible`-string
/// match ranks above a generic class-wildcard driver.
const BIND_PRIORITY: u16 = 10;

/// The driver's canonical bind table — the single source both the installed
/// bundle's signed manifest and the autoload match are built from, so the
/// match data can never drift from the driver.
pub const BIND_KEYS: &[DriverBindKey] = &[DriverBindKey::new(
    BIND_PRIORITY,
    match HwMatchKey::compatible(GOLDFISH_COMPATIBLE) {
        Ok(key) => key,
        // Unreachable: the literal is well within `HW_COMPATIBLE_MAX`. A
        // too-long literal would be a compile-time const-eval error here,
        // never a runtime panic.
        Err(_) => panic!("compatible string fits HW_COMPATIBLE_MAX"),
    },
)];

/// Byte length of the register block the device presents. The alarm and
/// interrupt registers share a 4 KiB page with the counter, so a granted
/// window is a page; the driver reads only the counter pair at its base.
pub const REGISTER_BLOCK_LEN: usize = 0x1000;

/// `TIME_LOW`: low 32 bits of the counter. Reading it latches `TIME_HIGH`,
/// so it is the first access of every read.
const TIME_LOW: usize = 0x00;
/// `TIME_HIGH`: high 32 bits of the value the last `TIME_LOW` read latched,
/// and the store the device commits a write on.
const TIME_HIGH: usize = 0x04;

/// Bytes of the window the counter pair occupies. Bring-up demands this much
/// and no more: the driver touches nothing above it, and requiring the whole
/// page would refuse a narrower grant it could serve.
const COUNTER_SPAN: usize = TIME_HIGH + core::mem::size_of::<u32>();

/// Split a counter value into its `(TIME_LOW, TIME_HIGH)` halves.
///
/// Through the little-endian byte image rather than a masked narrowing cast,
/// so the split is total and carries no unreachable error arm.
const fn counter_halves(nanos: u64) -> (u32, u32) {
    let bytes = nanos.to_le_bytes();
    (
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
    )
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

/// A Goldfish RTC reached through its mapped register window.
pub struct Goldfish {
    regs: RegisterWindow,
}

impl Goldfish {
    /// Bind the driver to the counter in `regs`.
    ///
    /// There is no bring-up sequence to run — the device has no enable bit
    /// and no flag to clear — so binding is the window check alone.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] if the window is too small to hold the
    /// counter pair, so a mis-provisioned grant fails at bind rather than on
    /// every request, and never reads whatever lies past its window.
    pub fn new(regs: RegisterWindow) -> Result<Self, DriverError> {
        if regs.len() < COUNTER_SPAN {
            return Err(DriverError::DeviceFault);
        }
        Ok(Self { regs })
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

    /// The counter, in nanoseconds since the Unix epoch.
    ///
    /// `TIME_LOW` first: that access is what latches `TIME_HIGH`, so reading
    /// the halves the other way round can straddle a carry.
    fn counter_nanos(&self) -> Result<u64, DriverError> {
        let low = self.read_reg(TIME_LOW)?;
        let high = self.read_reg(TIME_HIGH)?;
        Ok(u64::from(low) | (u64::from(high) << 32))
    }
}

impl Rtc for Goldfish {
    fn status(&mut self) -> Result<RtcStatus, DriverError> {
        Ok(RtcStatus {
            precision: Duration64::from_nanos(1),
            battery_backed: false,
            oscillator_stopped: self.counter_nanos()? == 0,
        })
    }

    fn read(&mut self) -> Result<Option<Time64>, DriverError> {
        let nanos = self.counter_nanos()?;
        if nanos == 0 {
            return Ok(None);
        }
        // Exact for every counter value, so the saturating bounds are never
        // in play: a `u64` nanosecond count is under 18.5e9 whole seconds,
        // and the offset from the epoch adds nothing to that.
        Ok(Some(
            Time64::UNIX_EPOCH.saturating_add(Duration64::from_nanos(nanos)),
        ))
    }

    fn set(&mut self, time: Time64) -> Result<(), DriverError> {
        // The counter is an unsigned 64-bit nanosecond count, so it holds
        // 1970-01-01 through 2554-07-21 and nothing else. An instant outside
        // that is refused whole rather than wrapped or clamped into a
        // different time.
        let nanos = u64::try_from(time.secs())
            .ok()
            .and_then(|secs| secs.checked_mul(u64::from(NANOS_PER_SEC)))
            .and_then(|whole| whole.checked_add(u64::from(time.subsec_nanos())))
            .ok_or(DriverError::OutOfRange)?;
        let (low, high) = counter_halves(nanos);
        // Low half first: the device commits the pair on the `TIME_HIGH`
        // store, so the high half must be the last write.
        self.write_reg(TIME_LOW, low)?;
        self.write_reg(TIME_HIGH, high)
    }
}
