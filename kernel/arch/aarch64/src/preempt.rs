//! Generic-timer-driven preemption on aarch64.
//!
//! The aarch64 analogue of `kernel/arch/{x86_64,riscv64}::preempt`. It
//! owns the per-CPU preemption surface built on the ARM EL1 *physical*
//! generic timer (`CNTP_*_EL0`) and its GIC private-peripheral interrupt
//! [`TIMER_PPI`]:
//!
//! * The set-once callback ([`set_timer_callback`]) the IRQ path forwards
//!   each tick to, with the CPU's `rustos_arch_api::CpuId`.
//! * `init_local_preempt`, which records the tick interval and the CPU's
//!   `CpuId`, enables the timer PPI at the GIC, programs the first
//!   countdown into `CNTP_TVAL_EL0`, and enables the timer
//!   (`CNTP_CTL_EL0.ENABLE`, `IMASK = 0`).
//! * `on_timer_interrupt`, called from the IRQ exception path on the
//!   timer PPI: it invokes the callback and re-arms `CNTP_TVAL_EL0`,
//!   which clears the timer condition so the next `eret` does not
//!   immediately re-trap.
//!
//! The kernel-side preemption logic lives in
//! `kernel/sched::Scheduler::on_timer_tick`; this module only wires the
//! aarch64 timer into that architecture-neutral surface (`AGENTS.md`
//! §2.4 — no interface creep).
//!
//! # Host testability
//!
//! The callback storage, the per-CPU interval/`CpuId` slots, and the
//! interval math are plain atomics and `const fn`s, so they build and
//! are unit-tested on the host. Only the `CNTP_*_EL0` system-register
//! writes and the GIC programming are gated to the freestanding aarch64
//! target.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use rustos_arch_api::CpuId;

use crate::kernel_arch::MAX_CPUS;

/// GIC INTID of the EL1 physical-timer private-peripheral interrupt
/// (ARM Generic Timer: the non-secure EL1 physical timer raises PPI 30).
pub const TIMER_PPI: u32 = 30;

/// `CNTP_CTL_EL0.ENABLE` (bit 0): start the timer counting down.
pub const CNTP_CTL_ENABLE: u64 = 1 << 0;

/// `CNTP_CTL_EL0.IMASK` (bit 1): when set, the timer does not raise its
/// interrupt. Left clear so the timer condition reaches the GIC.
pub const CNTP_CTL_IMASK: u64 = 1 << 1;

/// `u32` sentinel meaning "no CPU `CpuId` recorded yet".
const NO_CPU: u64 = u32::MAX as u64;

/// The callback the timer IRQ path forwards each tick to, packed into a
/// `usize` so the path swaps it in without a lock. Set up before the
/// timer is armed.
static TIMER_CALLBACK_FN: AtomicUsize = AtomicUsize::new(0);

/// Per-CPU tick interval in counter ticks; `0` until `init_local_preempt`
/// records it for that CPU.
static TIMER_INTERVAL_TICKS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// Per-CPU `CpuId` passed to the callback; [`NO_CPU`] until recorded.
static TIMER_CPU_ID: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(NO_CPU) }; MAX_CPUS];

/// Install the timer callback.
///
/// Invoked from the timer IRQ path on every tick with the CPU's
/// [`CpuId`]. Storing a `fn` (not a closure) keeps it safe to call from
/// interrupt context: there is no captured environment to drop
/// mid-flight.
pub fn set_timer_callback(cb: extern "C" fn(CpuId)) {
    TIMER_CALLBACK_FN.store(cb as usize, Ordering::Relaxed);
}

/// Read the currently-installed timer callback, if any. Test/diagnostic.
#[must_use]
pub fn timer_callback() -> Option<extern "C" fn(CpuId)> {
    let raw = TIMER_CALLBACK_FN.load(Ordering::Relaxed);
    if raw == 0 {
        None
    } else {
        // SAFETY: every store into `TIMER_CALLBACK_FN` round-trips a
        // valid `extern "C" fn(CpuId)` pointer through
        // `set_timer_callback`.
        Some(unsafe { core::mem::transmute::<usize, extern "C" fn(CpuId)>(raw) })
    }
}

/// Bounded index into the per-CPU slots for `cpu`.
///
/// The clamp is defence-in-depth so an out-of-range `CpuId` can never
/// index outside the arrays (`AGENTS.md` §2.9).
fn cpu_slot(cpu: CpuId) -> usize {
    (cpu as usize).min(MAX_CPUS - 1)
}

/// The recorded tick interval for `cpu` in counter ticks (`0` if unset).
/// Test/diagnostic observer.
#[must_use]
pub fn timer_interval_ticks(cpu: CpuId) -> u64 {
    TIMER_INTERVAL_TICKS[cpu_slot(cpu)].load(Ordering::Relaxed)
}

/// The `CpuId` recorded for `cpu`'s timer slot, or `None` if
/// `init_local_preempt` has not run for it yet. Test/diagnostic observer
/// (also keeps the per-CPU slot live on the host build).
#[must_use]
pub fn timer_cpu_id(cpu: CpuId) -> Option<CpuId> {
    let recorded = TIMER_CPU_ID[cpu_slot(cpu)].load(Ordering::Relaxed);
    if recorded == NO_CPU {
        None
    } else {
        // The slot only ever holds a `CpuId` (`u32`) or the `NO_CPU`
        // sentinel, so the low 32 bits are the whole value.
        #[allow(clippy::cast_possible_truncation)]
        Some(recorded as CpuId)
    }
}

#[cfg(test)]
fn clear_for_tests() {
    TIMER_CALLBACK_FN.store(0, Ordering::Relaxed);
    for slot in &TIMER_INTERVAL_TICKS {
        slot.store(0, Ordering::Relaxed);
    }
    for slot in &TIMER_CPU_ID {
        slot.store(NO_CPU, Ordering::Relaxed);
    }
}

/// Compute the tick interval, in counter ticks, for `hz` ticks per
/// second given a `counter_hz` clock. Clamps to at least one tick so a
/// pathological `hz > counter_hz` cannot arm a zero-interval timer that
/// re-fires without progress (`AGENTS.md` §2.9).
#[must_use]
pub const fn interval_for_hz(counter_hz: u64, hz: u64) -> u64 {
    let interval = counter_hz / if hz == 0 { 1 } else { hz };
    if interval == 0 {
        1
    } else {
        interval
    }
}

// --- Freestanding timer programming -------------------------------

/// Write `CNTP_TVAL_EL0` (the down-counter) to arm the next tick.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn write_tval(interval: u64) {
    // SAFETY: `CNTP_TVAL_EL0` is writable at EL1; setting it programs the
    // physical timer's countdown and has no other side effects.
    unsafe {
        core::arch::asm!("msr CNTP_TVAL_EL0, {}", in(reg) interval, options(nomem, nostack));
    }
}

/// Initialise generic-timer preemption on `cpu`.
///
/// Records the CPU and the tick `interval_ticks`, enables the timer PPI
/// at the GIC, programs the first countdown, and enables the timer with
/// its interrupt unmasked. It does **not** unmask interrupts at the PE
/// (`DAIF`); the caller does that via [`crate::exceptions::enable_irq`]
/// once it is ready to take ticks — matching the riscv64
/// `init_local_preempt` / `sstatus.SIE` split.
///
/// # Safety
///
/// * `cpu` must be the calling CPU's `CpuId`.
/// * The timer callback must already be installed via
///   [`set_timer_callback`] and the vector table via
///   [`crate::exceptions::init_vectors`].
/// * The GIC must be initialised ([`crate::gic::init`]).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub unsafe fn init_local_preempt(cpu: CpuId, interval_ticks: u64) {
    let slot = cpu_slot(cpu);
    let interval = interval_ticks.max(1);
    TIMER_INTERVAL_TICKS[slot].store(interval, Ordering::Relaxed);
    TIMER_CPU_ID[slot].store(u64::from(cpu), Ordering::Relaxed);

    // SAFETY: the GIC distributor is enabled by the caller's contract;
    // enabling the timer PPI lets the armed timer reach the CPU.
    unsafe {
        crate::gic::enable_ppi(TIMER_PPI);
    }

    write_tval(interval);
    // SAFETY: enabling `CNTP_CTL_EL0.ENABLE` with `IMASK` clear starts
    // the timer and lets it raise PPI 30; no memory side effects beyond
    // the system register.
    unsafe {
        core::arch::asm!("msr CNTP_CTL_EL0, {}", in(reg) CNTP_CTL_ENABLE, options(nomem, nostack));
    }
}

/// Handle a generic-timer interrupt: invoke the installed callback with
/// the recorded CPU `CpuId`, then re-arm `CNTP_TVAL_EL0` for the next
/// tick (which clears the timer condition).
///
/// Called only from [`crate::exceptions`]' IRQ path, with interrupts
/// masked (the PE masked them on exception entry).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub(crate) fn on_timer_interrupt(cpu: CpuId) {
    let slot = cpu_slot(cpu);
    let raw = TIMER_CALLBACK_FN.load(Ordering::Relaxed);
    let recorded = TIMER_CPU_ID[slot].load(Ordering::Relaxed);
    if raw != 0 && recorded != NO_CPU {
        // SAFETY: every store into `TIMER_CALLBACK_FN` round-trips a
        // valid `extern "C" fn(CpuId)` pointer through
        // `set_timer_callback`; the callback is a `fn` with no captured
        // environment, safe to invoke from interrupt context.
        let cb: extern "C" fn(CpuId) =
            unsafe { core::mem::transmute::<usize, extern "C" fn(CpuId)>(raw) };
        #[allow(clippy::cast_possible_truncation)]
        let recorded_cpu = recorded as u32;
        cb(recorded_cpu);
    }
    // Re-arm last so the scheduler runs at least one tick before the next
    // interrupt can stack.
    let interval = TIMER_INTERVAL_TICKS[slot].load(Ordering::Relaxed);
    if interval != 0 {
        write_tval(interval);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern "C" fn host_cb(_cpu: CpuId) {}

    #[test]
    fn interval_clamps_to_at_least_one_tick() {
        assert_eq!(interval_for_hz(1000, 100), 10);
        // hz larger than the clock would divide to zero; clamp to 1.
        assert_eq!(interval_for_hz(10, 100), 1);
        // hz == 0 must not divide by zero.
        assert_eq!(interval_for_hz(1000, 0), 1000);
    }

    #[test]
    fn timer_ppi_and_ctl_bits_match_arm_spec() {
        assert_eq!(TIMER_PPI, 30);
        assert_eq!(CNTP_CTL_ENABLE, 0b01);
        assert_eq!(CNTP_CTL_IMASK, 0b10);
    }

    #[test]
    fn callback_round_trips_through_the_slot() {
        clear_for_tests();
        assert!(timer_callback().is_none());
        set_timer_callback(host_cb);
        let got = timer_callback().expect("callback installed");
        assert_eq!(got as usize, host_cb as *const () as usize);
        clear_for_tests();
    }
}
