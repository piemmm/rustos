//! Raspberry Pi CPU frequency driver (`raspberrypi,firmware-clocks`).
//!
//! On a Pi the ARM core clock belongs to the `VideoCore` firmware, not to the
//! ARM cores: there is no register a driver can write, only a property
//! exchange asking the firmware to run the clock at a rate. So this driver
//! maps no MMIO and takes no interrupt — its only path to the hardware is the
//! mailbox channel, exactly like the Pi's PMIC clock
//! (`drivers/rtc/rpi`).
//!
//! # Why the board needs it at all
//!
//! The firmware leaves the ARM clock wherever it last put it, which after the
//! boot window is `arm_freq_min` — 600 MHz on a Pi 4B whose parts are rated
//! at 1.5 GHz. Nothing raises it again unless an OS driver asks, so a machine
//! with no frequency driver runs at 40% of its rated speed no matter how much
//! work it has. That is the defect this driver exists to close.
//!
//! # What it decides, and what it does not
//!
//! Nothing here decides *what* rate to run at. The kernel's governor watches
//! the per-CPU idle transitions and the program-launch path and publishes a
//! target; this driver reports the range it can deliver, then applies
//! whatever it is handed. Policy is kernel-side because it must answer within
//! the transition it is reacting to; the mechanism is here because reaching
//! the firmware is a device operation.
//!
//! # What it can and cannot vouch for
//!
//! The firmware clamps a request to the clock's own range and rounds it to a
//! rate the PLL can synthesise, so what it applied is not what it was asked
//! for. [`RpiCpuFreq::apply`] therefore reports the firmware's own answer
//! rather than echoing the request. A rate of zero is how the property
//! interface spells "no such clock", so it is refused rather than read as a
//! stopped clock.
//!
//! Reference: the Raspberry Pi firmware property interface
//! (`RPI_FIRMWARE_{GET,SET}_CLOCK_RATE`, `RPI_FIRMWARE_GET_MIN_CLOCK_RATE`,
//! `RPI_FIRMWARE_GET_MAX_CLOCK_RATE` and `RPI_FIRMWARE_ARM_CLK_ID`), as the
//! `clk-raspberrypi` and `raspberrypi-cpufreq` drivers in the Raspberry Pi
//! Linux tree spell it.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use tairix_abi::cpufreq::CpuFreqLimits;
use tairix_abi::driver::mailbox::MailboxChannel;
use tairix_abi::{CapabilityId, DriverBindKey, DriverError, DriverHandle, DriverHost, HwMatchKey};
use tairix_vcmailbox::{
    decode_clock_rate_response, decode_clock_rate_write_response, encode_clock_rate_query,
    encode_clock_rate_write, ClockRateQuery, FirmwareClock, MailboxError,
};

#[cfg(test)]
mod tests;

/// Per-driver `DriverHandle` marker returned by [`register`], mirroring the
/// convention every driver crate uses: the host re-issues its own host-local
/// handle when binding the driver, and this constant is the on-the-wire
/// signal that the load-time gate cleared. The bytes spell `"RPCF"`.
const REGISTER_HANDLE_MARKER: u64 = 0x5250_4346_0000_0001;

/// Device-tree `compatible` string this driver binds to — the Raspberry Pi
/// binding name for the node through which the firmware exposes its clocks,
/// so a discovery source that already speaks that vocabulary needs no
/// translation.
pub const FIRMWARE_CLOCKS_COMPATIBLE: &[u8] = b"raspberrypi,firmware-clocks";

/// The bind priority [`BIND_KEYS`] carries. An exact `compatible`-string
/// match ranks above a generic class-wildcard driver.
const BIND_PRIORITY: u16 = 10;

/// The driver's canonical bind table — the single source both the installed
/// bundle's signed manifest and the autoload match are built from, so the
/// match data can never drift from the driver.
pub const BIND_KEYS: &[DriverBindKey] = &[DriverBindKey::new(
    BIND_PRIORITY,
    match HwMatchKey::compatible(FIRMWARE_CLOCKS_COMPATIBLE) {
        Ok(key) => key,
        // Unreachable: the literal is well within `HW_COMPATIBLE_MAX`. A
        // too-long literal would be a compile-time const-eval error here,
        // never a runtime panic.
        Err(_) => panic!("compatible string fits HW_COMPATIBLE_MAX"),
    },
)];

/// The capabilities the driver needs, and the single definition of them.
///
/// The installed bundle's signed manifest is built from this and the program
/// re-checks itself against it, exactly as [`BIND_KEYS`] serves the manifest
/// and the autoload match — so neither can drift from the driver. The mailbox
/// channel is its only path to the clock; the mechanism role is what lets it
/// take the machine's DVFS seam. The autoload gate must be able to delegate
/// both, or the matched driver is refused for escalation.
pub const REQUIRED_CAPABILITIES: &[CapabilityId] = &[CapabilityId::MAILBOX, CapabilityId::CPUFREQ];

/// Granularity the governor is asked to quantise its targets to, in Hz.
///
/// The firmware accepts any rate and rounds, so this is not a hardware
/// constraint but the interval worth *asking* at: finer would spend a
/// firmware round trip on a change too small to matter, coarser would leave
/// usable rates unreachable. A hundred megahertz is the interval the Pi's own
/// Linux `cpufreq` driver builds its operating points on
/// (`RASPBERRYPI_FREQ_INTERVAL`), so a TAIRiX machine is asked at the same
/// points the vendor's own driver uses.
const RATE_INTERVAL_HZ: u64 = 100_000_000;

/// Driver entry point.
///
/// # Errors
///
/// [`DriverError::PermissionDenied`] if the host did not grant
/// [`CapabilityId::DRV_LOAD`].
///
/// # Capabilities
///
/// Requires [`CapabilityId::DRV_LOAD`] to load, and — to take the mechanism
/// role once running — `CAP_CPUFREQ`, which the kernel checks at the bind
/// trap rather than here.
pub fn register(host: &dyn DriverHost) -> Result<DriverHandle, DriverError> {
    if !host.has_capability(CapabilityId::DRV_LOAD) {
        return Err(DriverError::PermissionDenied);
    }
    DriverHandle::from_raw(REGISTER_HANDLE_MARKER)
}

/// The Pi's firmware-owned ARM core clock, reached over a property-mailbox
/// channel.
pub struct RpiCpuFreq<C: MailboxChannel> {
    channel: C,
}

impl<C: MailboxChannel> RpiCpuFreq<C> {
    /// Bind the driver to the ARM clock the firmware behind `channel` owns.
    ///
    /// There is no bring-up step: the clock is already running whenever the
    /// board is powered. A channel that cannot reach the firmware surfaces at
    /// the first query.
    pub const fn new(channel: C) -> Self {
        Self { channel }
    }

    /// Ask the firmware for one of the ARM clock's rates, in Hz.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] when the firmware refuses the tag or
    /// answers zero — its spelling of "no such clock", which must not be read
    /// as a stopped one. Other faults map from [`MailboxError`].
    fn query(&self, which: ClockRateQuery) -> Result<u64, DriverError> {
        let mut message = encode_clock_rate_query(FirmwareClock::Arm, which);
        self.channel.exchange(&mut message)?;
        let rate = decode_clock_rate_response(FirmwareClock::Arm, which, &message)
            .map_err(MailboxError::as_driver_error)?;
        if rate == 0 {
            return Err(DriverError::DeviceFault);
        }
        Ok(u64::from(rate))
    }

    /// The operating range to declare to the kernel governor.
    ///
    /// Both ends come from the firmware, never from a board constant: a Pi
    /// whose `config.txt` raises `arm_freq` or lifts `arm_freq_min` reports
    /// its own range and is driven over it.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] when the firmware refuses a tag or answers
    /// a zero rate, [`DriverError::OutOfRange`] when it reports a range no
    /// governor could pick from (an inverted pair, or one narrower than the
    /// interval worth asking at) — refused rather than second-guessed — and
    /// otherwise as [`MailboxError`] maps.
    pub fn limits(&self) -> Result<CpuFreqLimits, DriverError> {
        let min_hz = self.query(ClockRateQuery::Min)?;
        let max_hz = self.query(ClockRateQuery::Max)?;
        // A board whose whole range is narrower than one interval is driven
        // at a single rate rather than being asked at points it cannot
        // deliver; the step is then irrelevant and any legal value serves.
        let step_hz = if max_hz.saturating_sub(min_hz) < RATE_INTERVAL_HZ {
            max_hz.saturating_sub(min_hz).max(1)
        } else {
            RATE_INTERVAL_HZ
        };
        CpuFreqLimits::new(min_hz, max_hz, step_hz).map_err(|_| DriverError::OutOfRange)
    }

    /// The rate the ARM clock is running at now, in Hz.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] when the firmware refuses the tag or
    /// answers a zero rate — its spelling of "no such clock", which must not
    /// be read as a stopped one — and otherwise as [`MailboxError`] maps.
    pub fn current(&self) -> Result<u64, DriverError> {
        self.query(ClockRateQuery::Current)
    }

    /// Ask the firmware to run the ARM clock at `rate_hz`, returning the rate
    /// it actually applied.
    ///
    /// The answer is the firmware's own, not an echo: it clamps to the
    /// clock's range and rounds to a synthesisable rate, so only it knows
    /// what the cores are now running at.
    ///
    /// # Errors
    ///
    /// [`DriverError::OutOfRange`] for a target wider than the firmware's
    /// 32-bit rate field, [`DriverError::DeviceFault`] when the firmware
    /// refuses the tag or reports a zero rate, and otherwise as
    /// [`MailboxError`] maps.
    pub fn apply(&self, rate_hz: u64) -> Result<u64, DriverError> {
        let rate = u32::try_from(rate_hz).map_err(|_| DriverError::OutOfRange)?;
        let mut message = encode_clock_rate_write(FirmwareClock::Arm, rate);
        self.channel.exchange(&mut message)?;
        let applied = decode_clock_rate_write_response(FirmwareClock::Arm, &message)
            .map_err(MailboxError::as_driver_error)?;
        if applied == 0 {
            return Err(DriverError::DeviceFault);
        }
        Ok(u64::from(applied))
    }
}
