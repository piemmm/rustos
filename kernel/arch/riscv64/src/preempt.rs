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
//! # Per-hart state (SMP)
//!
//! The tick interval and the [`CpuId`] the callback receives are stored
//! *per hart*, indexed by the hart id [`crate::smp::current_hartid`]
//! reports. Each hart records its own interval in `init_local_preempt`
//! and arms its own SBI timer, so different harts
//! may run at different tick rates and each tick reports the hart it
//! fired on. The tick callback function itself is shared (one scheduler
//! entry point), so it stays a single slot.
//!
//! # Inter-processor interrupts
//!
//! A directed reschedule arrives as a supervisor *software* interrupt
//! (the target hart's `sip.SSIP`, raised by the SBI IPI extension —
//! `crate::kernel_arch::RiscvArch::send_ipi`). `on_software_interrupt`
//! acknowledges it (clears `sip.SSIP`) and runs the installed IPI
//! callback with the current hart's id, mirroring the timer path.
//!
//! # Host testability
//!
//! The callback storage, the per-hart interval/`CpuId` slots, and the
//! `scause` decode are plain atomics and `const fn`s, so they build and
//! are unit-tested on the host. Only the SBI `set_timer` re-arm and the
//! `sie` CSR writes are gated to the freestanding riscv64 target.

use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering};

use rustos_arch_api::CpuId;

use crate::smp::current_hartid;

/// `sie.STIE` — supervisor timer interrupt enable (bit 5, privileged
/// spec §4.1.3).
pub const SIE_STIE: u64 = 1 << 5;

/// `scause` cause code for a Supervisor Timer Interrupt (privileged
/// spec table 4.2), paired with [`crate::trap::SCAUSE_INTERRUPT_BIT`].
pub const SCAUSE_SUPERVISOR_TIMER: u64 = 5;

/// `sie.SSIE` — supervisor software interrupt enable (bit 1, privileged
/// spec §4.1.3). Enabling it lets the hart take the IPI delivered as a
/// supervisor software interrupt.
pub const SIE_SSIE: u64 = 1 << 1;

/// `scause` cause code for a Supervisor Software Interrupt (privileged
/// spec table 4.2) — the cause an SBI IPI raises on the target hart.
pub const SCAUSE_SUPERVISOR_SOFTWARE: u64 = 1;

/// `sip.SSIP` — supervisor software interrupt pending (bit 1). Cleared
/// to acknowledge a delivered IPI.
pub const SIP_SSIP: u64 = 1 << 1;

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

/// `true` iff `scause` denotes a supervisor software interrupt — the
/// interrupt bit is set *and* the cause code is
/// [`SCAUSE_SUPERVISOR_SOFTWARE`]. This is how a delivered SBI IPI
/// surfaces on the target hart.
#[must_use]
pub const fn is_supervisor_software_interrupt(scause: u64) -> bool {
    (scause & crate::trap::SCAUSE_INTERRUPT_BIT) != 0
        && (scause & !crate::trap::SCAUSE_INTERRUPT_BIT) == SCAUSE_SUPERVISOR_SOFTWARE
}

/// The callback the timer trap path forwards each tick to, packed into
/// a `usize` (the size of a `fn` pointer) so the trap path swaps it in
/// without a lock. Set up before the timer is armed.
static TIMER_CALLBACK_FN: AtomicUsize = AtomicUsize::new(0);

/// Published per-hart tick-interval slice base (`null` until a
/// [`PreemptStorage`] is registered). The slice is `PREEMPT_LEN` long;
/// slot `i` holds hart `i`'s tick interval in `time`-CSR ticks, `0` until
/// `init_local_preempt` records it.
static PREEMPT_INTERVAL_PTR: AtomicPtr<AtomicU64> = AtomicPtr::new(core::ptr::null_mut());

/// Published per-hart `CpuId` slice base (`null` until a [`PreemptStorage`]
/// is registered). The slice is `PREEMPT_LEN` long; slot `i` holds the
/// `CpuId` passed to the callback, [`NO_CPU`] until recorded.
static PREEMPT_CPU_ID_PTR: AtomicPtr<AtomicU64> = AtomicPtr::new(core::ptr::null_mut());

/// Length of the published per-hart slices (`0` until a [`PreemptStorage`]
/// is registered, so every per-hart access fails closed — `AGENTS.md`
/// §2.9).
static PREEMPT_LEN: AtomicUsize = AtomicUsize::new(0);

/// Set-once guard so a second [`PreemptStorage::register`] is refused
/// rather than silently re-pointing the live per-hart slices.
static PREEMPT_REGISTERED: AtomicBool = AtomicBool::new(false);

/// Failure mode of [`PreemptStorage::register`].
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum PreemptStorageError {
    /// Storage was already registered; the slot is set-once per boot
    /// (`AGENTS.md` §2.1).
    AlreadyRegistered,
}

/// Caller-owned, `&'static` per-hart preemption backing, sized by the
/// constructing caller for its machine (`AGENTS.md` §24.1 — the per-hart
/// timer bookkeeping is derived from the §18-discovered hart count, never
/// a fixed `const` ceiling baked into the arch crate).
///
/// The const parameter `N` is the number of harts the caller sizes for: a
/// single-hart boot path or vertical uses `PreemptStorage<1>`, and a
/// multi-hart boot path sizes `N` from the device-tree hart count. The
/// arch crate stays allocator-free (`AGENTS.md` §24.1 watch-out — no
/// `alloc` in a bare-metal arch crate), so the caller provides the storage
/// as a `static` (allocator-free bins) or a leaked allocation and
/// publishes it through [`PreemptStorage::register`] before
/// `init_local_preempt`.
#[repr(C)]
pub struct PreemptStorage<const N: usize> {
    interval_ticks: [AtomicU64; N],
    cpu_id: [AtomicU64; N],
}

impl<const N: usize> PreemptStorage<N> {
    /// A backing in which every interval is `0` and every `CpuId` slot is
    /// the `NO_CPU` sentinel. `const` so the allocator-free bins can
    /// place it in a `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            interval_ticks: [const { AtomicU64::new(0) }; N],
            cpu_id: [const { AtomicU64::new(NO_CPU) }; N],
        }
    }

    /// Publish this backing as the per-hart preemption slices, then return
    /// the covered hart count `N`. Must be called on the boot hart,
    /// exactly once, before any `init_local_preempt`.
    ///
    /// # Errors
    ///
    /// [`PreemptStorageError::AlreadyRegistered`] on the second publish
    /// (set-once per boot, `AGENTS.md` §2.1).
    pub fn register(&'static self) -> Result<usize, PreemptStorageError> {
        if PREEMPT_REGISTERED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(PreemptStorageError::AlreadyRegistered);
        }
        PREEMPT_INTERVAL_PTR.store(self.interval_ticks.as_ptr().cast_mut(), Ordering::Release);
        PREEMPT_CPU_ID_PTR.store(self.cpu_id.as_ptr().cast_mut(), Ordering::Release);
        PREEMPT_LEN.store(N, Ordering::Release);
        Ok(N)
    }
}

impl<const N: usize> Default for PreemptStorage<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// The IPI callback the software-interrupt path forwards each delivered
/// IPI to, packed into a `usize`. Set up before any IPI is enabled.
static IPI_CALLBACK_FN: AtomicUsize = AtomicUsize::new(0);

/// The preemption callback the timer trap path forwards each tick **taken
/// from U-mode** to, packed into a `usize`. Installed by the binary before
/// the timer is armed; absent (`0`) the timer tick is pure accounting and
/// nothing is preempted, so an image that arms the timer without wiring
/// preemption simply keeps cooperative scheduling (fail-safe, `AGENTS.md`
/// §2.9).
///
/// This is the involuntary analogue of the cooperative reschedule the
/// `ecall` syscall path drives: a supervisor-timer interrupt taken while
/// U-mode was running lands on the interrupted task's own kernel stack
/// (the same stack an `ecall` trap uses, via the `sscratch` swap in
/// `trap.s`), so the installed callback can suspend that task back to the
/// scheduler exactly as the cooperative `yield` path does. The callback is
/// invoked **only** for a tick taken from U-mode — a tick taken in S-mode
/// never preempts (the kernel is non-preemptible, `AGENTS.md` §4 watch-out:
/// a half-completed kernel critical section must never be switched away
/// from). In production the kernel runs with `sstatus.SIE == 0`, so the
/// privilege rule (U < S) is the *only* path on which a tick is taken at
/// all, but the explicit SPP gate is defence-in-depth so a future S-mode
/// `SIE` enable can never accidentally preempt the kernel (`AGENTS.md`
/// §2.9).
static PREEMPT_CALLBACK_FN: AtomicUsize = AtomicUsize::new(0);

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

/// Install the IPI callback the software-interrupt path forwards each
/// delivered IPI to. Storing a `fn` (not a closure) keeps it safe to
/// call from trap context.
pub fn set_ipi_callback(cb: extern "C" fn(CpuId)) {
    IPI_CALLBACK_FN.store(cb as usize, Ordering::Relaxed);
}

/// Read the currently-installed IPI callback, if any. Test/diagnostic.
#[must_use]
pub fn ipi_callback() -> Option<extern "C" fn(CpuId)> {
    let raw = IPI_CALLBACK_FN.load(Ordering::Relaxed);
    if raw == 0 {
        None
    } else {
        // SAFETY: every store into `IPI_CALLBACK_FN` rounds-trips a
        // valid `extern "C" fn(CpuId)` pointer through
        // `set_ipi_callback`.
        Some(unsafe { core::mem::transmute::<usize, extern "C" fn(CpuId)>(raw) })
    }
}

/// Install the U-mode-preemption callback the timer trap path forwards
/// each tick taken from U-mode to (the private `PREEMPT_CALLBACK_FN`
/// slot).
///
/// Storing a `fn` (not a closure) keeps it safe to call from trap
/// context: there is no captured environment to drop mid-flight. The
/// binary installs the callback (which suspends the running user task
/// back to the scheduler) before arming the timer.
pub fn set_preempt_callback(cb: extern "C" fn(CpuId)) {
    PREEMPT_CALLBACK_FN.store(cb as usize, Ordering::Relaxed);
}

/// Read the currently-installed U-mode-preemption callback, if any.
/// Test/diagnostic.
#[must_use]
pub fn preempt_callback() -> Option<extern "C" fn(CpuId)> {
    let raw = PREEMPT_CALLBACK_FN.load(Ordering::Relaxed);
    if raw == 0 {
        None
    } else {
        // SAFETY: every store into `PREEMPT_CALLBACK_FN` round-trips a
        // valid `extern "C" fn(CpuId)` pointer through
        // `set_preempt_callback`.
        Some(unsafe { core::mem::transmute::<usize, extern "C" fn(CpuId)>(raw) })
    }
}

/// Invoke the installed U-mode-preemption callback for `cpu`, if any.
///
/// Called from the trap path **only** for a supervisor-timer tick taken
/// from U-mode (the saved `sstatus.SPP == 0`), **after**
/// [`on_timer_interrupt`] has re-armed the SBI timer (so the timer line is
/// no longer pending while the callback context-switches away). A build
/// that armed the timer without installing the callback keeps cooperative
/// scheduling — the tick is pure accounting (`AGENTS.md` §2.9, fail-safe).
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub(crate) fn on_u_mode_preempt_point(cpu: CpuId) {
    let raw = PREEMPT_CALLBACK_FN.load(Ordering::Relaxed);
    if raw != 0 {
        // SAFETY: every store into `PREEMPT_CALLBACK_FN` round-trips a
        // valid `extern "C" fn(CpuId)` pointer through
        // `set_preempt_callback`; the callback carries no captured
        // environment and is safe to call from trap context.
        let cb: extern "C" fn(CpuId) =
            unsafe { core::mem::transmute::<usize, extern "C" fn(CpuId)>(raw) };
        cb(cpu);
    }
}

/// Index into the registered per-hart slices for `hartid`, or `None` if
/// no [`PreemptStorage`] is registered yet.
///
/// The clamp to the published length is defence-in-depth so a malformed
/// `tp` can never index outside the slices (`AGENTS.md` §2.9); `None`
/// before registration makes every per-hart access fail closed rather
/// than dereference a null slice base.
fn per_cpu_index(hartid: CpuId) -> Option<usize> {
    let len = PREEMPT_LEN.load(Ordering::Acquire);
    if len == 0 {
        None
    } else {
        Some((hartid as usize).min(len - 1))
    }
}

/// Borrow the tick-interval slot at `idx`. `idx` must come from
/// [`per_cpu_index`] (so `idx < PREEMPT_LEN` and the base is non-null).
fn interval_slot(idx: usize) -> &'static AtomicU64 {
    let base = PREEMPT_INTERVAL_PTR.load(Ordering::Acquire);
    // SAFETY: a non-zero `PREEMPT_LEN` (which `per_cpu_index` checked
    // before yielding `idx`) is published in the same `register` call that
    // stores the non-null base from a `&'static PreemptStorage`'s
    // `interval_ticks` array of that length. `idx < len`, so
    // `base.add(idx)` is in bounds and its referent lives for `'static`.
    unsafe { &*base.add(idx) }
}

/// Borrow the `CpuId` slot at `idx`. `idx` must come from
/// [`per_cpu_index`] (so `idx < PREEMPT_LEN` and the base is non-null).
fn cpu_id_slot(idx: usize) -> &'static AtomicU64 {
    let base = PREEMPT_CPU_ID_PTR.load(Ordering::Acquire);
    // SAFETY: as for [`interval_slot`] — the non-null `cpu_id` base and the
    // matching `PREEMPT_LEN` are published together, and `idx < len`.
    unsafe { &*base.add(idx) }
}

/// The calling hart's recorded tick interval in `time`-CSR ticks (`0`
/// if unset or no storage registered). Test/diagnostic observer.
#[must_use]
pub fn timer_interval_ticks() -> u64 {
    match per_cpu_index(current_hartid()) {
        Some(idx) => interval_slot(idx).load(Ordering::Relaxed),
        None => 0,
    }
}

/// The calling hart's recorded `CpuId`, or `u32::MAX` if none (or no
/// storage registered). Test/diagnostic.
#[must_use]
pub fn timer_cpu_id() -> u32 {
    let Some(idx) = per_cpu_index(current_hartid()) else {
        return u32::MAX;
    };
    // The slot only ever holds values stored from a `CpuId` (`u32`) or
    // the `u32::MAX` sentinel, so the low 32 bits are the whole value.
    #[allow(clippy::cast_possible_truncation)]
    let cpu = cpu_id_slot(idx).load(Ordering::Relaxed) as u32;
    cpu
}

#[cfg(test)]
fn clear_for_tests() {
    TIMER_CALLBACK_FN.store(0, Ordering::Relaxed);
    IPI_CALLBACK_FN.store(0, Ordering::Relaxed);
    PREEMPT_CALLBACK_FN.store(0, Ordering::Relaxed);
    let len = PREEMPT_LEN.load(Ordering::Acquire);
    for idx in 0..len {
        interval_slot(idx).store(0, Ordering::Relaxed);
        cpu_id_slot(idx).store(NO_CPU, Ordering::Relaxed);
    }
}

#[cfg(test)]
fn reset_preempt_storage_for_tests() {
    PREEMPT_REGISTERED.store(false, Ordering::Release);
    PREEMPT_LEN.store(0, Ordering::Release);
    PREEMPT_INTERVAL_PTR.store(core::ptr::null_mut(), Ordering::Release);
    PREEMPT_CPU_ID_PTR.store(core::ptr::null_mut(), Ordering::Release);
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
    let Some(slot) = per_cpu_index(current_hartid()) else {
        // No `PreemptStorage` registered (or this hart outside it): fail
        // closed rather than arm a timer whose tick can never be recorded
        // or dispatched (`AGENTS.md` §2.9). A registered caller never hits
        // this branch.
        return;
    };
    let interval = interval_ticks.max(1);
    interval_slot(slot).store(interval, Ordering::Relaxed);
    cpu_id_slot(slot).store(u64::from(cpu), Ordering::Relaxed);
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
    use rustos_arch_api::Timer;
    let Some(slot) = per_cpu_index(current_hartid()) else {
        // No registered per-hart slot for this hart: nothing to dispatch
        // or re-arm (fail closed, `AGENTS.md` §2.9).
        return;
    };
    let cpu = cpu_id_slot(slot).load(Ordering::Relaxed);
    if cpu != NO_CPU {
        // Dispatch the tick through the Arch HAL timer surface so the
        // callback invoke lives in exactly one place (`AGENTS.md` §2.2);
        // the HAL handle reaches the same `TIMER_CALLBACK_FN` static this
        // module owns. `cpu` was stored from a `CpuId` (`u32`) in
        // `init_local_preempt`, so the low 32 bits are the whole value.
        #[allow(clippy::cast_possible_truncation)]
        let cpu = cpu as u32;
        crate::timer_hal::TimerHal::new().dispatch_tick(cpu);
    }
    // Re-arm (and acknowledge) the timer last so the scheduler runs at
    // least one tick before the next interrupt can stack.
    let interval = interval_slot(slot).load(Ordering::Relaxed);
    if interval != 0 {
        crate::sbi::set_timer(crate::kernel_arch::read_time().wrapping_add(interval));
    }
}

/// Enable supervisor software interrupts (`sie.SSIE`) on the calling
/// hart so a delivered SBI IPI traps here.
///
/// Like [`init_local_preempt`], this does **not** set `sstatus.SIE`; the
/// caller enables global interrupts once ready.
///
/// # Safety
///
/// Setting `sie.SSIE` makes the hart take supervisor software
/// interrupts; the caller must have installed the trap vector
/// ([`crate::trap::init_traps`]) and the IPI callback
/// ([`set_ipi_callback`]) first.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub unsafe fn enable_ipi() {
    // SAFETY: setting `sie.SSIE` enables supervisor software interrupts;
    // it has no memory side effects beyond the named CSR.
    unsafe {
        core::arch::asm!("csrs sie, {}", in(reg) SIE_SSIE, options(nomem, nostack));
    }
}

/// Handle a delivered IPI (a supervisor software interrupt): acknowledge
/// it by clearing `sip.SSIP`, then invoke the installed IPI callback
/// with the current hart's id.
///
/// Called only from [`crate::trap`]'s S-mode handler, with interrupts
/// disabled. The acknowledge happens first so a fresh IPI arriving
/// during the callback re-pends `SSIP` and is serviced on the next trap
/// rather than being lost.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub(crate) fn on_software_interrupt() {
    // SAFETY: clearing `sip.SSIP` acknowledges the pending IPI; the CSR
    // write has no other memory side effects. (`set_timer` clears
    // `sip.STIP` for the timer; the software bit must be cleared
    // explicitly because no SBI call does it.)
    unsafe {
        core::arch::asm!("csrc sip, {}", in(reg) SIP_SSIP, options(nomem, nostack));
    }
    let raw = IPI_CALLBACK_FN.load(Ordering::Relaxed);
    if raw != 0 {
        // SAFETY: every store into `IPI_CALLBACK_FN` round-trips a valid
        // `extern "C" fn(CpuId)` pointer through `set_ipi_callback`; the
        // callback is a `fn` with no captured environment.
        let cb: extern "C" fn(CpuId) =
            unsafe { core::mem::transmute::<usize, extern "C" fn(CpuId)>(raw) };
        cb(current_hartid());
    }
}

#[cfg(test)]
#[path = "preempt_tests.rs"]
mod tests;
