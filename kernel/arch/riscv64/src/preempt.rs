//! Supervisor-timer-driven preemption on riscv64.
//!
//! The riscv64 analogue of `kernel/arch/x86_64::preempt`. It owns the
//! per-hart preemption surface:
//!
//! * The set-once callback ([`set_timer_callback`]) the timer trap path
//!   forwards each tick to, with the hart's `rustos_arch_api::CpuId`.
//! * The supervisor-timer enable bit ([`SIE_STIE`]) and the `scause`
//!   code ([`SCAUSE_SUPERVISOR_TIMER`]) the trap handler matches.
//! * `init_local_preempt`, which records the tick interval and the
//!   hart's `CpuId`, arms the first SBI timer, and enables `sie.STIE`.
//! * `on_timer_interrupt`, called from the S-mode trap handler on a
//!   supervisor-timer interrupt: it invokes the callback and re-arms the
//!   SBI timer, which clears the pending `sip.STIP` so `sret` does not
//!   immediately re-trap.
//!
//! The kernel-side preemption logic lives in
//! `kernel/sched::Scheduler::on_timer_tick`; this module only wires the
//! riscv64 timer into that architecture-neutral surface (`AGENTS.md`
//! §2.4 — no interface creep).
//!
//! # Single-hart slice
//!
//! The boot-to-`BootCompleted` slice brings up one hart, so the tick
//! callback is invoked with a single stored [`CpuId`]. Multi-hart
//! preemption (per-hart intervals and `tp`-derived hart identity) lands
//! with riscv64 SMP bring-up, alongside the SBI `IPI` extension the
//! `RiscvArch::send_ipi` no-op documents.
//!
//! # Host testability
//!
//! The callback storage, the interval/`CpuId` slots, and the `scause`
//! decode are plain atomics and `const fn`s, so they build and are
//! unit-tested on the host. Only the SBI `set_timer` re-arm and the
//! `sie.STIE` CSR write are gated to the freestanding riscv64 target.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use rustos_arch_api::CpuId;

/// `sie.STIE` — supervisor timer interrupt enable (bit 5, privileged
/// spec §4.1.3).
pub const SIE_STIE: u64 = 1 << 5;

/// `scause` cause code for a Supervisor Timer Interrupt (privileged
/// spec table 4.2), paired with [`crate::trap::SCAUSE_INTERRUPT_BIT`].
pub const SCAUSE_SUPERVISOR_TIMER: u64 = 5;

/// `u32` sentinel meaning "no hart `CpuId` recorded yet".
const NO_CPU: u64 = u32::MAX as u64;

/// `true` iff `scause` denotes a supervisor timer interrupt — the
/// interrupt bit is set *and* the cause code is
/// [`SCAUSE_SUPERVISOR_TIMER`].
#[must_use]
pub const fn is_supervisor_timer_interrupt(scause: u64) -> bool {
    (scause & crate::trap::SCAUSE_INTERRUPT_BIT) != 0
        && (scause & !crate::trap::SCAUSE_INTERRUPT_BIT) == SCAUSE_SUPERVISOR_TIMER
}

/// The callback the timer trap path forwards each tick to, packed into
/// a `usize` (the size of a `fn` pointer) so the trap path swaps it in
/// without a lock. Set up before the timer is armed.
static TIMER_CALLBACK_FN: AtomicUsize = AtomicUsize::new(0);

/// Tick interval in `time`-CSR ticks; `0` until `init_local_preempt`
/// records it.
static TIMER_INTERVAL_TICKS: AtomicU64 = AtomicU64::new(0);

/// The hart `CpuId` passed to the callback; [`NO_CPU`] until recorded.
static TIMER_CPU_ID: AtomicU64 = AtomicU64::new(NO_CPU);

/// Install the per-hart timer callback.
///
/// Invoked from the timer trap path on every tick with the hart's
/// [`CpuId`]. Storing a `fn` (not a closure) keeps it safe to call from
/// trap context: there is no captured environment to drop mid-flight.
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
        // SAFETY: every store into `TIMER_CALLBACK_FN` rounds-trips a
        // valid `extern "C" fn(CpuId)` pointer through
        // `set_timer_callback`.
        Some(unsafe { core::mem::transmute::<usize, extern "C" fn(CpuId)>(raw) })
    }
}

/// The recorded tick interval in `time`-CSR ticks (`0` if unset).
/// Test/diagnostic observer.
#[must_use]
pub fn timer_interval_ticks() -> u64 {
    TIMER_INTERVAL_TICKS.load(Ordering::Relaxed)
}

/// The recorded hart `CpuId`, or `u32::MAX` if none. Test/diagnostic.
#[must_use]
pub fn timer_cpu_id() -> u32 {
    // The slot only ever holds values stored from a `CpuId` (`u32`) or
    // the `u32::MAX` sentinel, so the low 32 bits are the whole value.
    #[allow(clippy::cast_possible_truncation)]
    let cpu = TIMER_CPU_ID.load(Ordering::Relaxed) as u32;
    cpu
}

#[cfg(test)]
fn clear_for_tests() {
    TIMER_CALLBACK_FN.store(0, Ordering::Relaxed);
    TIMER_INTERVAL_TICKS.store(0, Ordering::Relaxed);
    TIMER_CPU_ID.store(NO_CPU, Ordering::Relaxed);
}

/// Compute the tick interval, in `time`-CSR ticks, for `hz` ticks per
/// second given a `timebase_hz` clock. Clamps to at least one tick so a
/// pathological `hz > timebase_hz` cannot arm a zero-interval timer that
/// re-fires without progress (`AGENTS.md` §2.9).
#[must_use]
pub const fn interval_for_hz(timebase_hz: u64, hz: u64) -> u64 {
    let interval = timebase_hz / if hz == 0 { 1 } else { hz };
    if interval == 0 {
        1
    } else {
        interval
    }
}

/// Initialise supervisor-timer preemption on the calling hart.
///
/// Records the hart `cpu` and the tick `interval_ticks`, arms the first
/// SBI timer at `now + interval_ticks`, and enables `sie.STIE`. The
/// function does **not** set `sstatus.SIE`; the caller enables global
/// interrupts (via [`crate::trap::init_traps`]) once it is ready to
/// take ticks — matching the x86_64 `init_local_preempt` / `sti` split.
///
/// # Safety
///
/// * `cpu` must be the calling hart's `CpuId`.
/// * The timer callback must already be installed via
///   [`set_timer_callback`].
/// * Setting `sie.STIE` makes the hart take supervisor timer interrupts;
///   the caller must have installed the trap vector first.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub unsafe fn init_local_preempt(cpu: CpuId, interval_ticks: u64) {
    let interval = interval_ticks.max(1);
    TIMER_INTERVAL_TICKS.store(interval, Ordering::Relaxed);
    TIMER_CPU_ID.store(u64::from(cpu), Ordering::Relaxed);
    crate::sbi::set_timer(crate::kernel_arch::read_time().wrapping_add(interval));
    // SAFETY: setting `sie.STIE` enables supervisor timer interrupts;
    // it has no memory side effects beyond the named CSR. The caller's
    // contract guarantees the trap vector and callback are installed.
    unsafe {
        core::arch::asm!("csrs sie, {}", in(reg) SIE_STIE, options(nomem, nostack));
    }
}

/// Handle a supervisor timer interrupt: invoke the installed callback
/// with the recorded hart `CpuId`, then re-arm the SBI timer for the
/// next tick (which clears the pending `sip.STIP`).
///
/// Called only from [`crate::trap`]'s S-mode handler, with interrupts
/// disabled (hardware cleared `sstatus.SIE` on trap entry).
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub(crate) fn on_timer_interrupt() {
    let raw = TIMER_CALLBACK_FN.load(Ordering::Relaxed);
    let cpu = TIMER_CPU_ID.load(Ordering::Relaxed);
    if raw != 0 && cpu != NO_CPU {
        // SAFETY: every store into `TIMER_CALLBACK_FN` is the round-trip
        // of a valid `extern "C" fn(CpuId)` pointer through
        // `set_timer_callback`; the callback is a `fn` with no captured
        // environment, safe to invoke from trap context.
        let cb: extern "C" fn(CpuId) =
            unsafe { core::mem::transmute::<usize, extern "C" fn(CpuId)>(raw) };
        // `cpu` was stored from a `CpuId` (`u32`) in `init_local_preempt`,
        // so the low 32 bits are the whole value.
        #[allow(clippy::cast_possible_truncation)]
        let cpu = cpu as u32;
        cb(cpu);
    }
    // Re-arm (and acknowledge) the timer last so the scheduler runs at
    // least one tick before the next interrupt can stack.
    let interval = TIMER_INTERVAL_TICKS.load(Ordering::Relaxed);
    if interval != 0 {
        crate::sbi::set_timer(crate::kernel_arch::read_time().wrapping_add(interval));
    }
}

#[cfg(test)]
#[path = "preempt_tests.rs"]
mod tests;
