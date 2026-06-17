//! Microsecond timing seam shared by drivers that need timed waits.
//!
//! [`Delay`] is the one definition of "block for N microseconds, and read a
//! monotonic microsecond clock from the same source" that a driver consumes
//! when a bring-up step has a hardware-dictated settle time (a PCIe link
//! train, a USB hub power-on-good / reset-recovery window). It lives in
//! `lib/abi` so the driver-class crates that need it — the PCIe root-complex
//! bring-up (`drivers/bus/pcie_brcm`) and the bus-agnostic USB stack
//! (`lib/usb`) — depend on one trait rather than each declaring their own
//! (`AGENTS.md` §2.2 / §17.4).
//!
//! It carries no authority of its own: it is a pure timing facility the host
//! supplies (on metal a generic-timer implementation reading the
//! architecture's monotonic counter; host tests a deterministic stand-in).

/// A microsecond timing seam: a busy-delay plus a monotonic clock.
///
/// The host supplies the implementation. On metal it is backed by the
/// architecture's monotonic counter (e.g. `CNTPCT_EL0`/`CNTFRQ_EL0` on
/// aarch64); host tests supply a deterministic stand-in.
pub trait Delay {
    /// Block for at least `us` microseconds.
    fn delay_us(&self, us: u32);

    /// A monotonically non-decreasing microsecond timestamp from the same
    /// source [`delay_us`](Delay::delay_us) blocks against, so a caller can
    /// bound a poll loop by elapsed wall time rather than an iteration count
    /// (a single read that itself blocks cannot then inflate the loop). The
    /// epoch is unspecified; only differences are meaningful.
    fn now_us(&self) -> u64;
}
