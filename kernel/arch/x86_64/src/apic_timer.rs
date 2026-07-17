//! PIT-calibrated LAPIC timer driver.
//!
//! The LAPIC timer runs at the CPU's bus frequency divided by a
//! programmable divisor; that frequency is not advertised by any
//! architectural register on pre-Skylake parts. The portable
//! calibration trick (Intel SDM Vol. 3A §11.5.4, "Local APIC Timer")
//! is to:
//!
//! 1. Program the LAPIC timer in **one-shot** mode with a known
//!    divisor and a large initial-count.
//! 2. Use the i8254 Programmable Interval Timer (PIT) — which has a
//!    fixed 1.193182 MHz tick — to busy-wait a precisely-known
//!    interval.
//! 3. Sample the LAPIC's current-count register before and after the
//!    interval; the delta gives ticks-per-second.
//! 4. From ticks-per-second compute the initial-count to program for
//!    the desired periodic tick (typically 1 ms for the scheduler).
//!
//! This module exposes:
//!
//! * [`PortIo`] — trait wrapping the four PIT I/O ports the calibrator
//!   uses (`0x40`, `0x42`, `0x43`, `0x61`). Production impl
//!   `PolledPit` is `#[cfg(target_os = "none")]`-only and wraps `in`
//!   /`out` instructions with a documented `// SAFETY:` rationale.
//! * [`compute_initial_count`] — the pure ticks-per-second → initial-
//!   count math, host-unit-tested.
//! * [`Calibration`] — the parameters produced by [`calibrate`] and
//!   consumed by [`program_oneshot_disarmed`] (the per-quantum
//!   initial-count the tickless one-shot is armed to).
//!
//! Calibration via PIT channel 2 (gated by port 0x61 bit 0) is the
//! recommended approach because channel 0 may already be in use by
//! firmware and channel 2 has no IRQ side-effects.
//!
//! References:
//! * Intel SDM Vol. 3A §11.5.4 (LAPIC timer).
//! * Intel 8254 Programmable Interval Timer datasheet.

use crate::apic::{Lapic, LapicMmio};

/// PIT base frequency (Hz). This is fixed by the 8254 part — the
/// classical 14.31818 MHz / 12.
pub const PIT_FREQUENCY_HZ: u64 = 1_193_182;

/// Divisor encoded into the LAPIC timer's Divide Configuration
/// Register (SDM §11.5.4). We always calibrate with `Divide16` because
/// it gives a comfortable count range on every CPU we target.
pub const LAPIC_TIMER_DIVIDE_16_RAW: u32 = 0b0011;
/// Numeric divisor selected by [`LAPIC_TIMER_DIVIDE_16_RAW`].
pub const LAPIC_TIMER_DIVIDE_16: u32 = 16;

/// `LVT_TIMER.mode` bits (SDM §11.5.4, Figure 11-11).
pub mod timer_mode {
    /// One-shot mode (initial-count decrements once to zero, then stops).
    ///
    /// TAIRiX programs the LAPIC timer one-shot only (
    /// tickless / `NO_HZ`); there is no periodic-mode constant because no
    /// path arms a fixed-frequency auto-reload (no dead code).
    pub const ONE_SHOT: u32 = 0b00 << 17;
}

/// Errors returned by the calibration helper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalibrationError {
    /// `period_micros` was zero or wider than the PIT's 16-bit reload
    /// can express in a single arming. Use a smaller window and call
    /// [`calibrate`] repeatedly if a longer interval is needed.
    PeriodOutOfRange,
    /// The LAPIC counter did not change measurably during the PIT
    /// window — strongly suggests the LAPIC is not actually running
    /// (timer LVT still masked, divisor mis-programmed, or no MMIO).
    NoLapicTickDetected,
}

/// PIT I/O port abstraction. Production uses [`PolledPit`]; tests use
/// an in-memory mock (see the `tests` module, `#[cfg(test)]`-only).
pub trait PortIo {
    /// Read one byte from `port`.
    fn inb(&mut self, port: u16) -> u8;
    /// Write one byte to `port`.
    fn outb(&mut self, port: u16, value: u8);
}

/// Time-stamp-counter reader. Production uses [`Rdtsc`]; tests use the
/// in-memory `MockTsc` (see the `tests` module).
///
/// The TSC is sampled across the same PIT calibration window the LAPIC
/// is measured over, so the resulting `tsc_per_second` is derived from
/// exactly the same time base as `ticks_per_second`. Wiring the reader
/// as a trait rather than calling `_rdtsc()` directly inside
/// [`calibrate`] keeps the calibration deterministic under host unit
/// tests (no flaky tests).
pub trait TscReader {
    /// Read the current TSC value.
    fn read(&mut self) -> u64;
}

/// Production [`TscReader`]: invokes the `rdtsc` instruction.
///
/// `rdtsc` is unconditionally available on every x86_64 CPU and on the
/// host toolchain TAIRiX builds against; the same impl is therefore
/// reused on both `target_os = "none"` and `target_os = "linux"`
/// builds (host unit tests of consumers that drive `calibrate` against
/// a real CPU).
#[derive(Debug, Default, Clone, Copy)]
pub struct Rdtsc;

impl TscReader for Rdtsc {
    fn read(&mut self) -> u64 {
        #[cfg(target_arch = "x86_64")]
        {
            // SAFETY: `rdtsc` is unprivileged, has no memory side
            // effects, and is documented in Intel SDM Vol. 2B. It is
            // unconditionally available on every x86_64 CPU (it predates
            // the architecture); the surrounding `cfg(target_arch =
            // "x86_64")` guarantees the instruction is only emitted for
            // an x86_64 code generator. The instruction reads the
            // monotonically-non-decreasing time-stamp counter into
            // EDX:EAX; we recombine it into a single `u64`.
            let lo: u32;
            let hi: u32;
            unsafe {
                core::arch::asm!(
                    "rdtsc",
                    out("eax") lo,
                    out("edx") hi,
                    options(nomem, nostack, preserves_flags),
                );
            }
            (u64::from(hi) << 32) | u64::from(lo)
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            // Non-x86_64 host build (e.g. a developer's aarch64 or
            // riscv64 workstation running `cargo test`). `rdtsc` has no
            // encoding off x86_64, so emitting it would fail to
            // assemble. The production reader is never exercised on such
            // hosts — `calibrate`'s unit tests drive the calibration
            // window through `MockTsc` — so a constant keeps the crate
            // building without inventing a fake timebase. Returning a
            // value (rather than panicking) honours.
            0
        }
    }
}

/// Computed calibration result: LAPIC ticks-per-second, the matching
/// `initial_count`, and the time-stamp-counter rate sampled across the
/// same PIT calibration window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Calibration {
    /// LAPIC counter ticks observed per second, with the divisor
    /// applied. Use [`compute_initial_count`] to convert this into
    /// the value programmed into the LAPIC's initial-count register
    /// for any period.
    pub ticks_per_second: u64,
    /// `initial_count` that produces a period of `period_micros`.
    pub initial_count: u32,
    /// The period the `initial_count` field above was derived for.
    pub period_micros: u32,
    /// Time-stamp-counter ticks observed per second, sampled across
    /// the same PIT window as `ticks_per_second`.
    ///
    /// Consumed by `BinArch::monotonic_ns` (Stage 2.7 follow-up (f3))
    /// to convert an `rdtsc` reading into nanoseconds-since-boot for
    /// the `clock_get` syscall. The conversion goes through
    /// [`Self::tsc_ticks_to_ns`]; callers must use that helper instead
    /// of open-coding the math so saturation behaviour is consistent.
    pub tsc_per_second: u64,
}

impl Calibration {
    /// Convert a TSC-tick count into nanoseconds, saturating on
    /// overflow.
    ///
    /// Computed as `ticks * 1_000_000_000 / tsc_per_second`. The
    /// numerator overflows above `u64::MAX / 1_000_000_000 ≈ 1.84e10`
    /// ticks; we promote through `u128` to avoid a panic and saturate
    /// at `u64::MAX` on the (theoretical) overflow path. A
    /// `tsc_per_second` of zero (host-only mock; should never appear
    /// on bare metal because [`calibrate`] returns
    /// [`CalibrationError::NoLapicTickDetected`] long before we'd
    /// observe a zero TSC delta) returns `0` to keep the call site
    /// from panicking (no `unwrap` in production
    /// paths).
    #[must_use]
    pub fn tsc_ticks_to_ns(self, ticks: u64) -> u64 {
        if self.tsc_per_second == 0 {
            return 0;
        }
        let numerator = u128::from(ticks).saturating_mul(1_000_000_000);
        let ns = numerator / u128::from(self.tsc_per_second);
        // `u64::try_from` saturates via `unwrap_or` so the function
        // never panics on the overflow path;
        // (no `expect`/`unwrap` in production paths).
        u64::try_from(ns).unwrap_or(u64::MAX)
    }
}

/// Pure ticks/sec → initial-count math.
///
/// Caps the result at `u32::MAX`; the LAPIC register is 32-bit and a
/// saturated value simply produces the slowest tick the divisor
/// supports, which is acceptable for the scheduler.
///
/// # Errors
///
/// Returns [`CalibrationError::PeriodOutOfRange`] if `period_micros`
/// is `0`.
pub fn compute_initial_count(
    ticks_per_second: u64,
    period_micros: u32,
) -> Result<u32, CalibrationError> {
    if period_micros == 0 {
        return Err(CalibrationError::PeriodOutOfRange);
    }
    // ticks_in_period = ticks_per_second * period_micros / 1_000_000.
    let numerator = ticks_per_second.saturating_mul(u64::from(period_micros));
    let count = numerator / 1_000_000;
    Ok(u32::try_from(count).unwrap_or(u32::MAX))
}

/// Compute PIT reload value for a one-shot delay of `period_micros`.
///
/// The reload register is 16-bit, so the longest delay this expresses
/// is `65535 / 1.193182 MHz ≈ 54.9 ms`. Callers that need longer
/// windows must iterate.
///
/// # Errors
///
/// Returns [`CalibrationError::PeriodOutOfRange`] if the computed
/// reload would exceed `0xFFFF` or be zero.
pub fn compute_pit_reload(period_micros: u32) -> Result<u16, CalibrationError> {
    if period_micros == 0 {
        return Err(CalibrationError::PeriodOutOfRange);
    }
    let reload = (PIT_FREQUENCY_HZ * u64::from(period_micros)) / 1_000_000;
    if reload == 0 || reload > 0xFFFF {
        return Err(CalibrationError::PeriodOutOfRange);
    }
    // SAFETY-INVARIANT: the bound check above proves
    // `reload <= 0xFFFF`, so the u16 conversion is infallible. We use
    // `unwrap_or` rather than `expect` to satisfy the "no expect in
    // production paths" rule without conditionally panicking on a
    // statically-impossible branch.
    Ok(u16::try_from(reload).unwrap_or(u16::MAX))
}

// --- Calibration driver ---------------------------------------------

/// Drive a full PIT-channel-2 calibration window and program the LAPIC
/// timer for the requested periodic period.
///
/// The caller passes:
///
/// * `lapic` — handle to a LAPIC already software-enabled per
///   `Lapic::software_enable`.
/// * `pit` — implementation of [`PortIo`] for the four PIT-relevant
///   ports.
/// * `calibration_window_us` — width of the busy-wait calibration
///   pulse on PIT channel 2 (typical value: 10 000 µs = 10 ms; long
///   enough for a usable sample, short enough to be one PIT reload).
/// * `target_period_us` — desired periodic tick period for the
///   scheduler (typical value: 1 000 µs = 1 ms).
///
/// # Errors
///
/// Returns [`CalibrationError`] if the PIT reload would not fit in
/// 16 bits or the LAPIC counter showed no progress during the window.
pub fn calibrate<L: LapicMmio, P: PortIo, T: TscReader>(
    lapic: &mut Lapic<L>,
    pit: &mut P,
    tsc: &mut T,
    calibration_window_us: u32,
    target_period_us: u32,
) -> Result<Calibration, CalibrationError> {
    let reload = compute_pit_reload(calibration_window_us)?;

    // Set LAPIC divisor.
    lapic.mmio_mut().write(
        Lapic::<L>::TIMER_DIVIDE_CONFIG_OFFSET,
        LAPIC_TIMER_DIVIDE_16_RAW,
    );
    // Mask the LVT and select one-shot mode for the calibration pass.
    let lvt_masked_oneshot = (1u32 << 16) | timer_mode::ONE_SHOT;
    lapic
        .mmio_mut()
        .write(Lapic::<L>::TIMER_LVT_OFFSET, lvt_masked_oneshot);

    // Arm PIT channel 2 in one-shot mode: write CW = 0xB0 (chan 2,
    // access lobyte/hibyte, mode 0, binary), then the reload, low
    // byte first.
    // Gate channel 2 via port 0x61 bit 0; ensure the speaker (bit 1)
    // stays off, and clear the gate before re-arming.
    let gate = pit.inb(0x61);
    pit.outb(0x61, (gate & 0xFC) | 0x01);
    pit.outb(0x43, 0xB0);
    pit.outb(0x42, (reload & 0xFF) as u8);
    pit.outb(0x42, (reload >> 8) as u8);

    // Start the LAPIC counter at its maximum value so we can measure
    // how many ticks pass during the PIT pulse.
    lapic
        .mmio_mut()
        .write(Lapic::<L>::TIMER_INITIAL_COUNT_OFFSET, u32::MAX);
    let tsc_start = tsc.read();

    // Busy-wait until PIT channel 2 OUT goes high (port 0x61 bit 5
    // mirrors channel 2 OUT). The mock advances this bit
    // deterministically after a fixed number of polls.
    loop {
        if (pit.inb(0x61) & 0x20) != 0 {
            break;
        }
    }
    let tsc_end = tsc.read();

    // Stop the LAPIC counter by writing 0 to initial-count (SDM
    // §11.5.4: writing 0 to ICR halts the timer).
    let current = lapic
        .mmio_mut()
        .read(Lapic::<L>::TIMER_CURRENT_COUNT_OFFSET);
    lapic
        .mmio_mut()
        .write(Lapic::<L>::TIMER_INITIAL_COUNT_OFFSET, 0);

    if current == u32::MAX {
        return Err(CalibrationError::NoLapicTickDetected);
    }
    let ticks_observed = u64::from(u32::MAX - current);
    // ticks_per_second = ticks_observed * 1_000_000 / calibration_window_us
    // (multiply first to keep precision).
    let ticks_per_second =
        ticks_observed.saturating_mul(1_000_000) / u64::from(calibration_window_us);

    let initial_count = compute_initial_count(ticks_per_second, target_period_us)?;

    // TSC sample across the same PIT window. `tsc_end` is observed
    // after `tsc_start` was sampled, so a monotonically-non-decreasing
    // TSC guarantees `tsc_end >= tsc_start`; we use `saturating_sub` to
    // be defensive against an arch port that ever wires up a
    // non-monotonic mock without violating the calibration contract.
    let tsc_observed = tsc_end.saturating_sub(tsc_start);
    let tsc_per_second = tsc_observed.saturating_mul(1_000_000) / u64::from(calibration_window_us);

    Ok(Calibration {
        ticks_per_second,
        initial_count,
        period_micros: target_period_us,
        tsc_per_second,
    })
}

/// Program the LAPIC timer in **one-shot** mode and leave it disarmed.
///
/// Sets the divide configuration and the LVT to one-shot delivery on
/// `vector` (the divisor and mode persist across one-shot fires, so the
/// later `crate::preempt::arm_oneshot` only has to rewrite the
/// initial-count), and writes an initial-count of `0` to halt the timer
/// (SDM §11.5.4). TAIRiX is tickless: the scheduler
/// arms the one-shot to one quantum (`calibration.initial_count` ticks)
/// only when a CPU is contended, and disarms otherwise — there is no
/// periodic auto-reload. The calibration is consumed by the caller
/// (`crate::preempt::init_local_preempt`) to record that per-quantum
/// count.
///
/// `vector` is the CPU interrupt vector the timer LVT fires on.
pub fn program_oneshot_disarmed<L: LapicMmio>(lapic: &mut Lapic<L>, vector: u8) {
    lapic.mmio_mut().write(
        Lapic::<L>::TIMER_DIVIDE_CONFIG_OFFSET,
        LAPIC_TIMER_DIVIDE_16_RAW,
    );
    let lvt = u32::from(vector) | timer_mode::ONE_SHOT;
    lapic.mmio_mut().write(Lapic::<L>::TIMER_LVT_OFFSET, lvt);
    // Initial-count 0 halts the timer: it stays disarmed until the
    // scheduler arms a one-shot quantum.
    lapic
        .mmio_mut()
        .write(Lapic::<L>::TIMER_INITIAL_COUNT_OFFSET, 0);
}

// --- Production PIT impl --------------------------------------------

/// Polled PIT I/O implementation using x86 `in` / `out`. Only available
/// on `target_os = "none"`.
#[cfg(any(target_os = "none", doc))]
#[derive(Debug, Default)]
pub struct PolledPit;

#[cfg(any(target_os = "none", doc))]
impl PortIo for PolledPit {
    fn inb(&mut self, port: u16) -> u8 {
        #[cfg(target_arch = "x86_64")]
        {
            // SAFETY: PIT ports 0x40-0x43 and 0x61 are architecturally
            // present on every x86 platform TAIRiX targets; reading
            // them has no side-effects other than the read itself. The
            // surrounding `cfg(target_arch = "x86_64")` guarantees
            // `in`/`out` are only emitted for an x86_64 code generator.
            unsafe {
                let value: u8;
                core::arch::asm!(
                    "in al, dx",
                    in("dx") port,
                    out("al") value,
                    options(nomem, nostack, preserves_flags),
                );
                value
            }
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            // Non-x86_64 host build: the legacy PIT port I/O space
            // exists only on x86, so `in`/`out` have no encoding here.
            // `PolledPit` is the production calibration backend and is
            // never reached on such hosts (`calibrate`'s unit tests
            // drive a mock `PortIo`), so the shim returns a constant
            // rather than emitting an invalid instruction. Returning a
            // value (rather than panicking) honours.
            let _ = port;
            0
        }
    }
    fn outb(&mut self, port: u16, value: u8) {
        #[cfg(target_arch = "x86_64")]
        {
            // SAFETY: as for `inb`; writes to channel 2 / port 0x61
            // only affect the speaker gate and the timer pulse we own.
            // The surrounding `cfg(target_arch = "x86_64")` guarantees
            // `in`/`out` are only emitted for an x86_64 code generator.
            unsafe {
                core::arch::asm!(
                    "out dx, al",
                    in("dx") port,
                    in("al") value,
                    options(nomem, nostack, preserves_flags),
                );
            }
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            // Non-x86_64 host build: see `inb`. No PIT port space exists
            // off x86, and this backend is never reached on such hosts.
            let _ = (port, value);
        }
    }
}

// --- Tests -----------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use crate::apic::tests_support::MockLapicMmio;
    use std::vec::Vec;

    /// Mock PIT that "completes" channel-2 after a fixed number of
    /// polls; each poll increments a wall-clock-like counter that
    /// also drives the LAPIC's current-count countdown via the
    /// `LapicAdvancer` callback wired below.
    pub struct MockPortIo {
        pub outs: Vec<(u16, u8)>,
        pub polls_until_done: u32,
        pub polls: u32,
        pub gate_state: u8,
    }
    impl MockPortIo {
        pub fn new(polls_until_done: u32) -> Self {
            Self {
                outs: Vec::new(),
                polls_until_done,
                polls: 0,
                gate_state: 0,
            }
        }
    }
    /// Test [`TscReader`] returning a deterministic ramp.
    ///
    /// Each call increments `current` by `step` and returns the new
    /// value, so two consecutive samples in [`calibrate`] differ by
    /// exactly `step` ticks. Tests pass a known `step` and assert on
    /// the resulting `tsc_per_second` (`step * 1_000_000 /
    /// calibration_window_us`), keeping the calibration deterministic
    /// (no flaky tests).
    pub struct MockTsc {
        current: u64,
        step: u64,
    }

    impl MockTsc {
        pub fn new(start: u64, step: u64) -> Self {
            Self {
                current: start,
                step,
            }
        }
    }

    impl TscReader for MockTsc {
        fn read(&mut self) -> u64 {
            self.current = self.current.wrapping_add(self.step);
            self.current
        }
    }

    impl PortIo for MockPortIo {
        fn inb(&mut self, port: u16) -> u8 {
            if port == 0x61 {
                self.polls += 1;
                if self.polls >= self.polls_until_done {
                    return self.gate_state | 0x20;
                }
                return self.gate_state;
            }
            0
        }
        fn outb(&mut self, port: u16, value: u8) {
            self.outs.push((port, value));
            if port == 0x61 {
                self.gate_state = value & 0x01;
            }
        }
    }

    #[test]
    fn compute_initial_count_basic_math() {
        // 1 GHz LAPIC ticks, 1 ms period -> 1_000_000 ticks.
        let n = compute_initial_count(1_000_000_000, 1_000).unwrap();
        assert_eq!(n, 1_000_000);
    }

    #[test]
    fn compute_initial_count_saturates_at_u32_max() {
        let n = compute_initial_count(u64::MAX, 1_000_000).unwrap();
        assert_eq!(n, u32::MAX);
    }

    #[test]
    fn compute_initial_count_rejects_zero_period() {
        assert_eq!(
            compute_initial_count(1_000_000_000, 0).err(),
            Some(CalibrationError::PeriodOutOfRange),
        );
    }

    #[test]
    fn compute_pit_reload_round_trip() {
        // 10 ms -> 11_931 ticks (fits in 16 bits).
        assert_eq!(compute_pit_reload(10_000).unwrap(), 11_931);
    }

    #[test]
    fn compute_pit_reload_rejects_too_long() {
        // 60 ms would overflow the 16-bit reload.
        assert_eq!(
            compute_pit_reload(60_000).err(),
            Some(CalibrationError::PeriodOutOfRange),
        );
    }

    #[test]
    fn calibrate_records_pit_program_sequence() {
        let mut lapic = Lapic::new(MockLapicMmio::default());
        // Pre-load LAPIC current-count so the calibration sees a decrement.
        lapic.mmio_mut().regs.insert(
            Lapic::<MockLapicMmio>::TIMER_CURRENT_COUNT_OFFSET,
            u32::MAX - 1_000,
        );
        let mut pit = MockPortIo::new(1);

        let mut tsc = MockTsc::new(0, 250_000); // 250k ticks between two reads
        let cal = calibrate(&mut lapic, &mut pit, &mut tsc, 10_000, 1_000).unwrap();
        // PIT outs must include the 0xB0 control word and the 11_931
        // reload (low byte 0x9B, high byte 0x2E).
        let cw = pit.outs.iter().any(|(p, v)| *p == 0x43 && *v == 0xB0);
        let lo = pit.outs.iter().any(|(p, v)| *p == 0x42 && *v == 0x9B);
        let hi = pit.outs.iter().any(|(p, v)| *p == 0x42 && *v == 0x2E);
        assert!(cw && lo && hi, "PIT was not armed: {:?}", pit.outs);

        // ticks_observed=1000 over 10ms -> 100_000 ticks/sec; 1ms -> 100.
        assert_eq!(cal.ticks_per_second, 100_000);
        assert_eq!(cal.initial_count, 100);
        assert_eq!(cal.period_micros, 1_000);
        // The MockTsc advances `step` ticks per read, so the two reads
        // inside `calibrate` yield a delta of exactly one `step`. Over
        // a 10 ms window that is `step * 100` ticks/sec.
        assert_eq!(cal.tsc_per_second, 250_000 * 100);
    }

    #[test]
    fn calibrate_detects_no_lapic_progress() {
        let mut lapic = Lapic::new(MockLapicMmio::default());
        // Leave current-count at default u32::MAX so the loop sees no
        // decrement. (MockLapicMmio::read returns 0 by default; we
        // pre-load u32::MAX to make the "no decrement" condition real.)
        lapic
            .mmio_mut()
            .regs
            .insert(Lapic::<MockLapicMmio>::TIMER_CURRENT_COUNT_OFFSET, u32::MAX);
        let mut pit = MockPortIo::new(1);
        let mut tsc = MockTsc::new(0, 1);
        assert_eq!(
            calibrate(&mut lapic, &mut pit, &mut tsc, 10_000, 1_000).err(),
            Some(CalibrationError::NoLapicTickDetected),
        );
    }

    #[test]
    fn tsc_ticks_to_ns_is_saturating_and_handles_zero_rate() {
        let cal = Calibration {
            ticks_per_second: 100_000,
            initial_count: 100,
            period_micros: 1_000,
            tsc_per_second: 1_000_000_000, // 1 GHz
        };
        // 1 tick at 1 GHz -> 1 ns.
        assert_eq!(cal.tsc_ticks_to_ns(1), 1);
        // 1_000 ticks at 1 GHz -> 1_000 ns.
        assert_eq!(cal.tsc_ticks_to_ns(1_000), 1_000);
        // 1e9 ticks at 1 GHz -> 1e9 ns (exactly 1 s).
        assert_eq!(cal.tsc_ticks_to_ns(1_000_000_000), 1_000_000_000);
        // u64::MAX ticks must not panic and must saturate.
        let _ = cal.tsc_ticks_to_ns(u64::MAX);

        let zero = Calibration {
            ticks_per_second: 0,
            initial_count: 0,
            period_micros: 0,
            tsc_per_second: 0,
        };
        // A zero rate must not panic; `0` is the documented fallback.
        assert_eq!(zero.tsc_ticks_to_ns(123_456), 0);
    }

    #[test]
    fn program_oneshot_disarmed_writes_expected_sequence() {
        let mut lapic = Lapic::new(MockLapicMmio::default());
        program_oneshot_disarmed(&mut lapic, 0x40);
        let w = &lapic.mmio_mut().writes;
        // Three writes: divide, LVT (one-shot), initial-count 0 (disarmed).
        assert_eq!(w[0].0, Lapic::<MockLapicMmio>::TIMER_DIVIDE_CONFIG_OFFSET);
        assert_eq!(w[0].1, LAPIC_TIMER_DIVIDE_16_RAW);
        assert_eq!(w[1].0, Lapic::<MockLapicMmio>::TIMER_LVT_OFFSET);
        assert_eq!(w[1].1, 0x40 | timer_mode::ONE_SHOT);
        assert_eq!(w[2].0, Lapic::<MockLapicMmio>::TIMER_INITIAL_COUNT_OFFSET);
        assert_eq!(w[2].1, 0, "timer must be left disarmed (initial-count 0)");
    }
}
