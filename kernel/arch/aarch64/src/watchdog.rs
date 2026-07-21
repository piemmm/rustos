//! aarch64 lockup-watchdog cadence timer and cross-CPU recovery.
//!
//! The aarch64 port of the Arch HAL watchdog surface
//! ([`tairix_arch_api::watchdog`]). It drives the architecture-neutral
//! detector in `kernel/core` with a periodic *liveness sample* and raises
//! the cross-CPU recovery signal a detected lockup asks for.
//!
//! # The cadence sample
//!
//! Hard-lockup detection needs a heartbeat that *stops* when a CPU stops
//! taking interrupts, sampled often enough that a multi-second threshold
//! has margin. This module arms the EL1 **virtual** generic timer
//! (`CNTV_*_EL0`, GIC PPI `WATCHDOG_PPI`) as a ~1 Hz one-shot on every
//! online CPU — a channel independent of the physical-timer one-shot the
//! tickless preemption path owns (`crate::preempt`), so the two never
//! interfere. It is programmed through the *relative* down-counter
//! `CNTV_TVAL_EL0`, so it needs no absolute virtual-count offset
//! (`CNTVOFF_EL2`, UNKNOWN at boot) — only "fire this many ticks from
//! now".
//!
//! On the QEMU `virt` board and the Raspberry Pi 4 the kernel runs at EL1
//! **non-secure** on a **GICv2**, where FIQ (Group 0) is the secure-world
//! interrupt a non-secure kernel cannot route to. So the sample is
//! delivered as an ordinary **IRQ**, and hard-lockup detection is the
//! cross-CPU *buddy* kind: a CPU that stops taking its watchdog IRQ is
//! observed by another CPU that is still taking its own. This is the
//! correct and complete detector for GICv2 non-secure, where FIQ (the
//! only non-maskable channel) belongs to the secure world. A board that
//! *does* expose a non-maskable channel (a GICv3 core with `ICC_PMR`
//! priority masking) can deliver this same sample as a true pseudo-NMI
//! behind the unchanged HAL surface, with no `kernel/core` change.
//!
//! The interrupt is dispatched by [`crate::exceptions`]' IRQ path, which
//! recognises `WATCHDOG_PPI`, calls `on_watchdog_interrupt` (re-arm +
//! invoke the installed callback), and runs the GIC end-of-interrupt
//! handshake. The installed callback (a bin-supplied `extern "C"
//! fn(CpuId)`, the layering-clean analogue of the timer callback) reads
//! the interrupted `ELR_EL1`/`SPSR_EL1` through `read_elr_el1` /
//! `read_spsr_el1`, builds the neutral sample, and forwards it to
//! `kernel/core`'s `on_watchdog_tick`.
//!
//! # Recovery
//!
//! `Watchdog` implements [`tairix_arch_api::WatchdogArch`]: a soft
//! lockup is met with a reschedule IPI (the reschedule SGI
//! [`crate::preempt::IPI_SGI`]) so the offending CPU re-enters the
//! scheduler; a hard lockup is met with the same directed SGI as a
//! best-effort attention signal — a CPU that can still take an IRQ
//! recovers, and one that genuinely cannot is left for the loud report
//! the detector already emitted (honest, never a silent no-op).

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use tairix_arch_api::{CpuId, RecoveryOutcome, StuckInterrupt, WatchdogArch, WatchdogKind};

/// GIC INTID of the EL1 **virtual** generic-timer private-peripheral
/// interrupt (the ARM Generic Timer raises the virtual timer on PPI 27).
/// Distinct from the physical-timer PPI [`crate::preempt::TIMER_PPI`] (30)
/// the preemption path uses, so the watchdog and preemption timers are
/// independent.
pub const WATCHDOG_PPI: u32 = 27;

/// `CNTV_CTL_EL0.ENABLE` (bit 0): start the virtual timer counting down.
pub const CNTV_CTL_ENABLE: u64 = 1 << 0;

/// `CNTV_CTL_EL0.IMASK` (bit 1): when set, the timer does not raise its
/// interrupt. Left clear so the timer condition reaches the GIC.
pub const CNTV_CTL_IMASK: u64 = 1 << 1;

/// The cadence interval in counter ticks (`0` until
/// `init_local_watchdog` records it). Uniform across CPUs — every core
/// shares one `CNTFRQ_EL0` — so one global copy, not a per-CPU slice.
static WATCHDOG_INTERVAL_TICKS: AtomicU64 = AtomicU64::new(0);

/// The callback the watchdog IRQ path forwards each cadence sample to,
/// packed into a `usize` so the path swaps it in without a lock. Set up
/// before the watchdog is armed; absent (`0`) the sample is a no-op (the
/// timer still re-arms), so an image that arms the watchdog without wiring
/// the detector simply keeps sampling harmlessly (fail-safe).
static WATCHDOG_CALLBACK_FN: AtomicUsize = AtomicUsize::new(0);

/// Install the watchdog cadence callback.
///
/// Invoked from the watchdog IRQ path on every cadence sample with the
/// CPU's [`CpuId`]. Storing a `fn` (not a closure) keeps it safe to call
/// from interrupt context: there is no captured environment to drop
/// mid-flight.
pub fn set_watchdog_callback(cb: extern "C" fn(CpuId)) {
    WATCHDOG_CALLBACK_FN.store(cb as usize, Ordering::Relaxed);
}

/// Read the currently-installed watchdog callback, if any. Test/diagnostic.
#[must_use]
pub fn watchdog_callback() -> Option<extern "C" fn(CpuId)> {
    let raw = WATCHDOG_CALLBACK_FN.load(Ordering::Relaxed);
    if raw == 0 {
        None
    } else {
        // SAFETY: every store into `WATCHDOG_CALLBACK_FN` round-trips a
        // valid `extern "C" fn(CpuId)` pointer through
        // `set_watchdog_callback`.
        Some(unsafe { core::mem::transmute::<usize, extern "C" fn(CpuId)>(raw) })
    }
}

/// The recorded cadence interval in counter ticks (`0` if unset).
/// Test/diagnostic observer.
#[must_use]
pub fn watchdog_interval_ticks() -> u64 {
    WATCHDOG_INTERVAL_TICKS.load(Ordering::Relaxed)
}

/// `true` iff a saved `SPSR_EL1` describes an interrupted **kernel**
/// (EL1) context rather than an EL0 user task.
///
/// `SPSR_EL1.M[3:2]` holds the exception level of the interrupted state;
/// `0b00` is EL0. Any higher value means the sample interrupted kernel
/// code, which owes the scheduler progress even when it is the only
/// runnable context — the distinction the soft-lockup check needs to avoid
/// flagging a legitimate lone user task.
#[must_use]
pub const fn spsr_in_kernel(spsr: u64) -> bool {
    ((spsr >> 2) & 0b11) != 0
}

// --- Freestanding timer programming + dispatch ---------------------

/// Read the interrupted return PC `ELR_EL1` (valid throughout an
/// exception handler, until the `eret`).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
#[must_use]
pub fn read_elr_el1() -> u64 {
    let elr: u64;
    // SAFETY: reading `ELR_EL1` has no side effects.
    unsafe {
        core::arch::asm!("mrs {}, ELR_EL1", out(reg) elr, options(nomem, nostack, preserves_flags));
    }
    elr
}

/// Read the interrupted processor state `SPSR_EL1` (valid throughout an
/// exception handler, until the `eret`).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
#[must_use]
pub fn read_spsr_el1() -> u64 {
    let spsr: u64;
    // SAFETY: reading `SPSR_EL1` has no side effects.
    unsafe {
        core::arch::asm!("mrs {}, SPSR_EL1", out(reg) spsr, options(nomem, nostack, preserves_flags));
    }
    spsr
}

/// Arm the virtual timer one-shot to fire `interval` counter ticks from
/// now (relative `CNTV_TVAL_EL0`), with its interrupt unmasked.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn arm(interval: u64) {
    // SAFETY: `CNTV_TVAL_EL0`/`CNTV_CTL_EL0` are writable at EL1; setting
    // the relative down-counter and enabling the timer (IMASK clear) has
    // no effect beyond the system registers. The interval is clamped to at
    // least one tick so a degenerate `0` cannot arm the current instant
    // with no forward progress.
    unsafe {
        core::arch::asm!(
            "msr CNTV_TVAL_EL0, {interval}",
            "msr CNTV_CTL_EL0, {ctl}",
            interval = in(reg) interval.max(1),
            ctl = in(reg) CNTV_CTL_ENABLE,
            options(nomem, nostack),
        );
    }
}

/// Initialise the lockup watchdog on the calling CPU: record the cadence
/// `interval_ticks`, enable the virtual-timer PPI at the GIC, and arm the
/// first one-shot.
///
/// Unlike the tickless preemption timer this stays armed for the CPU's
/// lifetime — each sample re-arms the next ([`on_watchdog_interrupt`]) —
/// so every online CPU keeps a fresh liveness heartbeat and runs the
/// cross-CPU scan even when idle. The ~1 Hz cadence costs one timer
/// interrupt per second per core, negligible against normal execution.
///
/// # Safety
///
/// * `interval_ticks` must be the counter-tick count for the cadence
///   (`CNTFRQ_EL0` for ~1 s).
/// * The GIC must be initialised ([`crate::gic::init`]) and the vector
///   table installed ([`crate::exceptions::init_vectors`]); the caller
///   unmasks IRQs separately ([`crate::exceptions::enable_irq`]).
/// * The watchdog callback should be installed ([`set_watchdog_callback`])
///   first, though an absent callback is a fail-safe no-op.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub unsafe fn init_local_watchdog(interval_ticks: u64) {
    WATCHDOG_INTERVAL_TICKS.store(interval_ticks.max(1), Ordering::Relaxed);
    // SAFETY: the GIC distributor is enabled by the caller's contract;
    // enabling the virtual-timer PPI lets the armed one-shot reach the CPU.
    unsafe {
        crate::gic::enable_ppi(WATCHDOG_PPI);
    }
    arm(interval_ticks);
}

/// Handle a virtual-timer watchdog interrupt on `cpu`: re-arm the next
/// one-shot and invoke the installed cadence callback.
///
/// Called only from [`crate::exceptions`]' IRQ path on [`WATCHDOG_PPI`],
/// with interrupts masked (the PE masked them on exception entry) and
/// before the GIC end-of-interrupt handshake. Re-arming first guarantees
/// the cadence continues even if the callback path is heavy. An absent
/// callback re-arms and returns (fail-safe).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub(crate) fn on_watchdog_interrupt(cpu: CpuId) {
    arm(WATCHDOG_INTERVAL_TICKS.load(Ordering::Relaxed));
    let raw = WATCHDOG_CALLBACK_FN.load(Ordering::Relaxed);
    if raw != 0 {
        // SAFETY: every store into `WATCHDOG_CALLBACK_FN` round-trips a
        // valid `extern "C" fn(CpuId)` through `set_watchdog_callback`; the
        // callback carries no captured environment.
        let cb: extern "C" fn(CpuId) =
            unsafe { core::mem::transmute::<usize, extern "C" fn(CpuId)>(raw) };
        cb(cpu);
    }
}

// --- Cross-CPU recovery -------------------------------------------

/// The aarch64 [`WatchdogArch`] recovery handle.
///
/// A zero-sized handle (the recovery mechanism is the GIC SGI path, which
/// holds no per-instance state), so it lives in a `static`
/// ([`AARCH64_WATCHDOG`]) the kernel installs by reference.
pub struct Watchdog;

/// The installed-by-reference recovery handle
/// ([`crate::kernel_arch::Aarch64Arch`] returns it to `kernel/core`).
pub static AARCH64_WATCHDOG: Watchdog = Watchdog;

impl WatchdogArch for Watchdog {
    fn request_recovery(&self, target: CpuId, kind: WatchdogKind) -> RecoveryOutcome {
        // Both a soft and a hard lockup are met with the directed
        // reschedule SGI: for a soft lockup it forces the offending CPU
        // back into the scheduler; for a hard lockup it is a best-effort
        // attention signal — a CPU still able to take an IRQ recovers, and
        // one that genuinely cannot is left for the loud report already
        // emitted (never a silent no-op). On a GICv2 non-secure kernel
        // there is no non-maskable channel to force a wedged core — that is
        // inherent to the hardware, and the loud cross-CPU report is the
        // complete answer for it (`plans/WATCHDOG.md`).
        #[cfg(all(target_arch = "aarch64", target_os = "none"))]
        {
            crate::gic::send_sgi(target);
        }
        #[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
        {
            let _ = target;
        }
        match kind {
            WatchdogKind::Soft => RecoveryOutcome::Rescheduled,
            WatchdogKind::Hard => RecoveryOutcome::AttentionRaised,
        }
    }

    fn stuck_interrupt(&self) -> Option<StuckInterrupt> {
        // The observer reads the distributor's globally-shared status: a
        // device SPI stuck active (its handler never completing, or the
        // line storming) is the "why" the hard-locked CPU's own stale
        // sample cannot give. SGIs/PPIs are banked per CPU and so are not
        // observable from here — only shared SPIs. Only a line that can
        // still reach a CPU is reported (active, or enabled-and-pending); a
        // masked line is skipped, since it cannot be the wedge. The reply's
        // active flag tells a live storm from an asserted-but-untaken line.
        #[cfg(all(target_arch = "aarch64", target_os = "none"))]
        {
            crate::gic::stuck_spi()
        }
        #[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
        {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watchdog_ppi_is_the_virtual_timer_and_distinct_from_preemption() {
        assert_eq!(WATCHDOG_PPI, 27);
        assert_ne!(WATCHDOG_PPI, crate::preempt::TIMER_PPI);
    }

    #[test]
    fn cntv_ctl_bits_match_the_arm_spec() {
        assert_eq!(CNTV_CTL_ENABLE, 0b01);
        assert_eq!(CNTV_CTL_IMASK, 0b10);
    }

    #[test]
    fn spsr_el0_is_user_and_el1_is_kernel() {
        // SPSR_EL1.M[3:0]: 0b0000 = EL0t (user); 0b0101 = EL1h (kernel).
        assert!(!spsr_in_kernel(0b0000));
        assert!(spsr_in_kernel(0b0101));
        assert!(spsr_in_kernel(0b0100));
        // The mask/condition bits above M do not affect the verdict.
        assert!(!spsr_in_kernel(0x6000_0000));
        assert!(spsr_in_kernel(0x6000_0005));
    }

    #[test]
    fn callback_round_trips_through_the_slot() {
        extern "C" fn cb(_cpu: CpuId) {}
        set_watchdog_callback(cb);
        let got = watchdog_callback().expect("callback installed");
        assert_eq!(got as usize, cb as *const () as usize);
        WATCHDOG_CALLBACK_FN.store(0, Ordering::Relaxed);
    }

    #[test]
    fn recovery_reports_the_signal_it_raised() {
        // On the host the SGI send is compiled out; the outcome still names
        // what a real send would have done for each kind.
        assert_eq!(
            AARCH64_WATCHDOG.request_recovery(1, WatchdogKind::Soft),
            RecoveryOutcome::Rescheduled
        );
        assert_eq!(
            AARCH64_WATCHDOG.request_recovery(1, WatchdogKind::Hard),
            RecoveryOutcome::AttentionRaised
        );
    }

    #[test]
    fn recovery_passes_the_arch_hal_conformance_vertical() {
        assert_eq!(
            tairix_arch_api::watchdog::conformance::run_all(&AARCH64_WATCHDOG, 0),
            Ok(())
        );
    }

    #[test]
    fn stuck_interrupt_is_none_off_metal() {
        // The distributor read is metal-only (it touches real GIC MMIO);
        // on the host the handle honestly reports no stuck line rather than
        // fabricating one, exactly as the recovery SGI compiles out.
        assert_eq!(AARCH64_WATCHDOG.stuck_interrupt(), None);
    }
}
