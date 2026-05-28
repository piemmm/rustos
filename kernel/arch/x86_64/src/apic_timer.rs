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
//!   consumed by [`program_periodic`].
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
    pub const ONE_SHOT: u32 = 0b00 << 17;
    /// Periodic mode (counter auto-reloads from initial-count).
    pub const PERIODIC: u32 = 0b01 << 17;
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

/// Computed calibration result: LAPIC ticks-per-second and the
/// `initial_count` to program for a periodic interval of
/// `period_micros` microseconds.
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
    // SAFETY-INVARIANT (AGENTS.md §2.9): the bound check above proves
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
pub fn calibrate<L: LapicMmio, P: PortIo>(
    lapic: &mut Lapic<L>,
    pit: &mut P,
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

    // Busy-wait until PIT channel 2 OUT goes high (port 0x61 bit 5
    // mirrors channel 2 OUT). The mock advances this bit
    // deterministically after a fixed number of polls.
    loop {
        if (pit.inb(0x61) & 0x20) != 0 {
            break;
        }
    }

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

    Ok(Calibration {
        ticks_per_second,
        initial_count,
        period_micros: target_period_us,
    })
}

/// Program the LAPIC timer in periodic mode using the result of
/// [`calibrate`].
///
/// `vector` is the CPU interrupt vector the timer LVT fires on.
pub fn program_periodic<L: LapicMmio>(lapic: &mut Lapic<L>, calibration: Calibration, vector: u8) {
    lapic.mmio_mut().write(
        Lapic::<L>::TIMER_DIVIDE_CONFIG_OFFSET,
        LAPIC_TIMER_DIVIDE_16_RAW,
    );
    let lvt = u32::from(vector) | timer_mode::PERIODIC;
    lapic.mmio_mut().write(Lapic::<L>::TIMER_LVT_OFFSET, lvt);
    lapic.mmio_mut().write(
        Lapic::<L>::TIMER_INITIAL_COUNT_OFFSET,
        calibration.initial_count,
    );
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
        // SAFETY: PIT ports 0x40-0x43 and 0x61 are architecturally
        // present on every x86 platform RustOS targets; reading them
        // has no side-effects other than the read itself.
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
    fn outb(&mut self, port: u16, value: u8) {
        // SAFETY: as for `inb`; writes to channel 2 / port 0x61 only
        // affect the speaker gate and the timer pulse we own.
        unsafe {
            core::arch::asm!(
                "out dx, al",
                in("dx") port,
                in("al") value,
                options(nomem, nostack, preserves_flags),
            );
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

        let cal = calibrate(&mut lapic, &mut pit, 10_000, 1_000).unwrap();
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
        assert_eq!(
            calibrate(&mut lapic, &mut pit, 10_000, 1_000).err(),
            Some(CalibrationError::NoLapicTickDetected),
        );
    }

    #[test]
    fn program_periodic_writes_expected_sequence() {
        let mut lapic = Lapic::new(MockLapicMmio::default());
        let cal = Calibration {
            ticks_per_second: 100_000,
            initial_count: 100,
            period_micros: 1_000,
        };
        program_periodic(&mut lapic, cal, 0x40);
        let w = &lapic.mmio_mut().writes;
        // Three writes: divide, LVT, initial-count.
        assert_eq!(w[0].0, Lapic::<MockLapicMmio>::TIMER_DIVIDE_CONFIG_OFFSET);
        assert_eq!(w[0].1, LAPIC_TIMER_DIVIDE_16_RAW);
        assert_eq!(w[1].0, Lapic::<MockLapicMmio>::TIMER_LVT_OFFSET);
        assert_eq!(w[1].1, 0x40 | timer_mode::PERIODIC);
        assert_eq!(w[2].0, Lapic::<MockLapicMmio>::TIMER_INITIAL_COUNT_OFFSET);
        assert_eq!(w[2].1, 100);
    }
}
