//! CPU frequency scaling ABI: the operating range a mechanism declares, and
//! the performance target the kernel publishes to it.
//!
//! Dynamic voltage/frequency scaling splits into a *policy* — how fast should
//! this machine run right now — and a *mechanism* — how a rate is actually
//! applied. The two live on opposite sides of this ABI:
//!
//! * The **kernel** owns the policy. Its governor watches the per-CPU idle
//!   transitions and the program-launch path and publishes one
//!   [`CpuFreqTarget`] for the machine. That has to be in the kernel: a
//!   target computed an IPC round trip after the work appeared would arrive
//!   after the latency it exists to avoid.
//! * A **user-space driver** owns the mechanism, because applying a rate is a
//!   device operation — on a Raspberry Pi it is a `VideoCore` firmware
//!   property exchange, elsewhere a register write or a power-controller
//!   transaction. It declares its [`CpuFreqLimits`] through
//!   [`crate::SyscallNumber::CPUFREQ_BIND`] and then blocks in
//!   [`crate::SyscallNumber::CPUFREQ_WAIT`] until the target moves.
//!
//! # Why the driver waits on a sequence, not on the rate
//!
//! A mechanism does not get to apply exactly what it was asked for: firmware
//! clamps a request to the clock's range and rounds it to a rate the PLL can
//! synthesise. A driver that blocked until "target equals what I applied"
//! would therefore spin forever on any rounded rate. [`CpuFreqTarget::seq`]
//! is what advances on a real change, so the wait is exact and targets
//! coalesce latest-wins: a driver that was busy applying one rate observes
//! only the newest.
//!
//! Nothing here reports the *achieved* frequency back. That is not the
//! mechanism's word to take — the kernel's per-CPU estimator measures the
//! live core clock from the silicon's own counters and the System Information
//! API reports that, so a mechanism whose rate never took effect is visible
//! rather than believed.

use crate::error::Errno;
use crate::le::{put_u64, read_u64};

/// The operating range a frequency mechanism declares at bind time.
///
/// These are the only facts about the mechanism the kernel cannot discover
/// for itself. Everything else the governor needs — which CPUs are busy, when
/// a program was launched — it already knows.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CpuFreqLimits {
    /// Lowest rate the mechanism can deliver, in Hz.
    pub min_hz: u64,
    /// Highest rate the mechanism can deliver, in Hz.
    pub max_hz: u64,
    /// Granularity the governor quantises a target to, in Hz.
    ///
    /// The governor rounds a proportional target *up* to a multiple of this,
    /// so tiny movements in utilisation do not each cost a mechanism round
    /// trip. A mechanism that can deliver any rate declares the coarsest step
    /// it wants to be asked at, not `1`.
    pub step_hz: u64,
}

impl CpuFreqLimits {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = 24;

    /// Construct a range, rejecting one no governor could pick from.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] unless `0 < min_hz <= max_hz` and `step_hz` is
    /// at least one hertz and no wider than the range itself. A zero `min_hz`
    /// would let a mechanism be asked for a stopped clock, a zero `step_hz`
    /// would make quantisation a division by zero, and a step wider than the
    /// range would collapse every target onto one rate.
    pub const fn new(min_hz: u64, max_hz: u64, step_hz: u64) -> Result<Self, Errno> {
        if min_hz == 0 || max_hz < min_hz || step_hz == 0 {
            return Err(Errno::OutOfRange);
        }
        if step_hz > max_hz - min_hz && min_hz != max_hz {
            return Err(Errno::OutOfRange);
        }
        Ok(Self {
            min_hz,
            max_hz,
            step_hz,
        })
    }

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u64(&mut out, 0, self.min_hz);
        put_u64(&mut out, 8, self.max_hz);
        put_u64(&mut out, 16, self.step_hz);
        out
    }

    /// Decode from `bytes`, validating the range as [`Self::new`] does.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] if short.
    /// * [`Errno::OutOfRange`] for a range [`Self::new`] would reject.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        Self::new(read_u64(bytes, 0), read_u64(bytes, 8), read_u64(bytes, 16))
    }
}

/// The performance target the kernel's governor currently asks for.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct CpuFreqTarget {
    /// Monotonic counter, advanced on every real change of
    /// [`Self::target_hz`].
    ///
    /// A driver passes the last value it saw to
    /// [`crate::SyscallNumber::CPUFREQ_WAIT`] and blocks until this differs,
    /// so no change is missed and none is observed twice. `0` is the value
    /// before any target has been published, so a driver may pass `0` for its
    /// first wait and be answered immediately.
    pub seq: u64,
    /// The rate the governor wants, in Hz — always within the bound
    /// [`CpuFreqLimits`] and a multiple of its step, except for the range's
    /// own endpoints.
    pub target_hz: u64,
}

impl CpuFreqTarget {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = 16;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u64(&mut out, 0, self.seq);
        put_u64(&mut out, 8, self.target_hz);
        out
    }

    /// Decode from `bytes`.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] if short.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        Ok(Self {
            seq: read_u64(bytes, 0),
            target_hz: read_u64(bytes, 8),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{CpuFreqLimits, CpuFreqTarget};
    use crate::error::Errno;

    /// A Pi 4B's range: 600 MHz to 1.5 GHz in 100 MHz steps.
    const PI4: Result<CpuFreqLimits, Errno> =
        CpuFreqLimits::new(600_000_000, 1_500_000_000, 100_000_000);

    #[test]
    fn limits_round_trip() {
        let limits = PI4.expect("a real range");
        assert_eq!(CpuFreqLimits::from_bytes(&limits.to_le_bytes()), Ok(limits));
    }

    #[test]
    fn a_fixed_rate_mechanism_is_a_legal_range() {
        // min == max describes a mechanism with one rate. The step cannot be
        // narrower than a zero-width range, so the width check stands aside.
        let fixed = CpuFreqLimits::new(1_000_000_000, 1_000_000_000, 100_000_000);
        assert!(fixed.is_ok());
    }

    #[test]
    fn limits_reject_a_range_no_governor_could_pick_from() {
        assert_eq!(
            CpuFreqLimits::new(0, 1_500_000_000, 100_000_000),
            Err(Errno::OutOfRange),
            "a stopped clock is not a rate"
        );
        assert_eq!(
            CpuFreqLimits::new(1_500_000_000, 600_000_000, 100_000_000),
            Err(Errno::OutOfRange),
            "inverted range"
        );
        assert_eq!(
            CpuFreqLimits::new(600_000_000, 1_500_000_000, 0),
            Err(Errno::OutOfRange),
            "a zero step would divide by zero"
        );
        assert_eq!(
            CpuFreqLimits::new(600_000_000, 700_000_000, 200_000_000),
            Err(Errno::OutOfRange),
            "a step wider than the range collapses every target"
        );
    }

    #[test]
    fn limits_decode_fails_closed_on_a_short_buffer() {
        assert_eq!(
            CpuFreqLimits::from_bytes(&[0u8; CpuFreqLimits::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn limits_decode_rejects_a_corrupt_range() {
        // A wire range is validated on the way in, not trusted: a zeroed
        // buffer is a stopped clock, not a mechanism.
        assert_eq!(
            CpuFreqLimits::from_bytes(&[0u8; CpuFreqLimits::WIRE_LEN]),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn target_round_trips_and_fails_closed_on_a_short_buffer() {
        let target = CpuFreqTarget {
            seq: 42,
            target_hz: 1_500_000_000,
        };
        assert_eq!(CpuFreqTarget::from_bytes(&target.to_le_bytes()), Ok(target));
        assert_eq!(
            CpuFreqTarget::from_bytes(&[0u8; CpuFreqTarget::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
    }
}
