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
//! aarch64 timer into that architecture-neutral surface (no interface creep).
//!
//! # Host testability
//!
//! The callback storage, the per-CPU interval/`CpuId` slots, and the
//! interval math are plain atomics and `const fn`s, so they build and
//! are unit-tested on the host. Only the `CNTP_*_EL0` system-register
//! writes and the GIC programming are gated to the freestanding aarch64
//! target.

use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering};

use rustos_arch_api::CpuId;

/// GIC INTID of the EL1 physical-timer private-peripheral interrupt
/// (ARM Generic Timer: the non-secure EL1 physical timer raises PPI 30).
pub const TIMER_PPI: u32 = 30;

/// `CNTP_CTL_EL0.ENABLE` (bit 0): start the timer counting down.
pub const CNTP_CTL_ENABLE: u64 = 1 << 0;

/// `CNTP_CTL_EL0.IMASK` (bit 1): when set, the timer does not raise its
/// interrupt. Left clear so the timer condition reaches the GIC.
pub const CNTP_CTL_IMASK: u64 = 1 << 1;

/// GIC INTID of the software-generated interrupt (SGI) the directed
/// inter-processor interrupt is delivered on. INTIDs `0..16` are SGIs;
/// INTID 0 is the reschedule IPI [`crate::kernel_arch::Aarch64Arch`]'s
/// `send_ipi` raises through `crate::gic::send_sgi`.
pub const IPI_SGI: u32 = 0;

/// `u32` sentinel meaning "no CPU `CpuId` recorded yet".
const NO_CPU: u64 = u32::MAX as u64;

/// Sentinel meaning "no deadline pending" in the per-CPU quantum / wakeup
/// slots ([`NO_DEADLINE`]). A real `CNTPCT_EL0` value never reaches
/// [`u64::MAX`] in any realistic uptime, so it is unambiguous.
const NO_DEADLINE: u64 = u64::MAX;

/// The callback the timer IRQ path forwards each tick to, packed into a
/// `usize` so the path swaps it in without a lock. Set up before the
/// timer is armed.
static TIMER_CALLBACK_FN: AtomicUsize = AtomicUsize::new(0);

/// Published per-CPU tick-interval slice base (`null` until a
/// [`PreemptStorage`] is registered). The slice is `PREEMPT_LEN` long;
/// slot `i` holds CPU `i`'s tick interval in counter ticks, `0` until
/// `init_local_preempt` records it.
static PREEMPT_INTERVAL_PTR: AtomicPtr<AtomicU64> = AtomicPtr::new(core::ptr::null_mut());

/// Published per-CPU `CpuId` slice base (`null` until a [`PreemptStorage`]
/// is registered). The slice is `PREEMPT_LEN` long; slot `i` holds the
/// `CpuId` passed to the callback, [`NO_CPU`] until recorded.
static PREEMPT_CPU_ID_PTR: AtomicPtr<AtomicU64> = AtomicPtr::new(core::ptr::null_mut());

/// Published per-CPU **quantum-deadline** slice base (`null` until a
/// [`PreemptStorage`] is registered). Slot `i` holds the absolute
/// `CNTPCT_EL0` tick at which the running task's preemption quantum
/// expires, or [`NO_DEADLINE`] when no quantum is armed (the CPU runs a
/// sole task / is idle). One half of the tickless one-shot combiner.
static PREEMPT_QUANTUM_PTR: AtomicPtr<AtomicU64> = AtomicPtr::new(core::ptr::null_mut());

/// Published per-CPU **wakeup-deadline** slice base (`null` until a
/// [`PreemptStorage`] is registered). Slot `i` holds the absolute
/// `CNTPCT_EL0` tick of the nearest pending blocking-wait timeout, or
/// [`NO_DEADLINE`] when none is pending. The other half of the combiner
/// (the nearest armed wakeup).
static PREEMPT_WAKEUP_PTR: AtomicPtr<AtomicU64> = AtomicPtr::new(core::ptr::null_mut());

/// Length of the published per-CPU slices (`0` until a [`PreemptStorage`]
/// is registered, so every per-CPU access fails closed).
static PREEMPT_LEN: AtomicUsize = AtomicUsize::new(0);

/// Set-once guard so a second [`PreemptStorage::register`] is refused
/// rather than silently re-pointing the live per-CPU slices.
static PREEMPT_REGISTERED: AtomicBool = AtomicBool::new(false);

/// Failure mode of [`PreemptStorage::register`] /
/// [`register_preempt_slices`].
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum PreemptStorageError {
    /// Storage was already registered; the slot is set-once per boot.
    AlreadyRegistered,
    /// The caller's per-CPU slices disagree on the CPU count; nothing
    /// was published (fail closed).
    MismatchedLengths,
}

/// Caller-owned, `&'static` per-CPU preemption backing, sized by the
/// constructing caller for its machine (the per-CPU
/// timer bookkeeping is derived from the-discovered core count, never
/// a fixed `const` ceiling baked into the arch crate).
///
/// The const parameter `N` is the number of logical CPUs the caller sizes
/// for: a single-CPU boot path or vertical uses `PreemptStorage<1>`, and a
/// multi-core boot path sizes `N` from the device-tree CPU count. The arch
/// crate stays allocator-free (watch-out — no `alloc` in
/// a bare-metal arch crate), so the caller provides the storage as a
/// `static` (allocator-free bins) or a leaked allocation and publishes it
/// through [`PreemptStorage::register`] before `init_local_preempt`.
#[repr(C)]
pub struct PreemptStorage<const N: usize> {
    interval_ticks: [AtomicU64; N],
    cpu_id: [AtomicU64; N],
    quantum_abs: [AtomicU64; N],
    wakeup_abs: [AtomicU64; N],
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
            quantum_abs: [const { AtomicU64::new(NO_DEADLINE) }; N],
            wakeup_abs: [const { AtomicU64::new(NO_DEADLINE) }; N],
        }
    }

    /// Publish this backing as the per-CPU preemption slices, then return
    /// the covered CPU count `N`. Must be called on the boot core, exactly
    /// once, before any `init_local_preempt`.
    ///
    /// # Errors
    ///
    /// [`PreemptStorageError::AlreadyRegistered`] on the second publish
    /// (set-once per boot).
    pub fn register(&'static self) -> Result<usize, PreemptStorageError> {
        register_preempt_slices(
            &self.interval_ticks,
            &self.cpu_id,
            &self.quantum_abs,
            &self.wakeup_abs,
        )
    }
}

/// Publish caller-leaked per-CPU preemption slices — the runtime-sized
/// twin of [`PreemptStorage::register`] for an allocator-having boot
/// path that sizes its backing to the *discovered* core count instead
/// of a compile-time `N`. The `const`-generic register delegates here,
/// so there is exactly one publish body.
///
/// Every slot is (re-)initialised to its documented sentinel before the
/// publish (`0` interval, `NO_CPU`, `NO_DEADLINE`), so a caller may hand
/// in plainly-zeroed slices. All four slices must be the same length;
/// set-once per boot.
///
/// # Errors
///
/// * [`PreemptStorageError::MismatchedLengths`] when the slices
///   disagree on the CPU count (nothing is published — fail closed).
/// * [`PreemptStorageError::AlreadyRegistered`] on the second publish.
pub fn register_preempt_slices(
    interval_ticks: &'static [AtomicU64],
    cpu_id: &'static [AtomicU64],
    quantum_abs: &'static [AtomicU64],
    wakeup_abs: &'static [AtomicU64],
) -> Result<usize, PreemptStorageError> {
    let count = interval_ticks.len();
    if cpu_id.len() != count || quantum_abs.len() != count || wakeup_abs.len() != count {
        return Err(PreemptStorageError::MismatchedLengths);
    }
    if PREEMPT_REGISTERED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(PreemptStorageError::AlreadyRegistered);
    }
    for slot in interval_ticks {
        slot.store(0, Ordering::Relaxed);
    }
    for slot in cpu_id {
        slot.store(NO_CPU, Ordering::Relaxed);
    }
    for slot in quantum_abs {
        slot.store(NO_DEADLINE, Ordering::Relaxed);
    }
    for slot in wakeup_abs {
        slot.store(NO_DEADLINE, Ordering::Relaxed);
    }
    PREEMPT_INTERVAL_PTR.store(interval_ticks.as_ptr().cast_mut(), Ordering::Release);
    PREEMPT_CPU_ID_PTR.store(cpu_id.as_ptr().cast_mut(), Ordering::Release);
    PREEMPT_QUANTUM_PTR.store(quantum_abs.as_ptr().cast_mut(), Ordering::Release);
    PREEMPT_WAKEUP_PTR.store(wakeup_abs.as_ptr().cast_mut(), Ordering::Release);
    PREEMPT_LEN.store(count, Ordering::Release);
    Ok(count)
}

impl<const N: usize> Default for PreemptStorage<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// The IPI callback the SGI IRQ path forwards each delivered IPI to,
/// packed into a `usize`. Set up before any IPI is enabled.
static IPI_CALLBACK_FN: AtomicUsize = AtomicUsize::new(0);

/// The preemption callback the timer IRQ path forwards each tick **taken
/// from EL0** to, packed into a `usize`. Installed by the binary before
/// the timer is armed; absent (`0`) the timer tick is pure accounting and
/// nothing is preempted, so an image that arms the timer without wiring
/// preemption simply keeps cooperative scheduling (fail-safe).
///
/// This is the involuntary analogue of the cooperative reschedule the
/// `svc` syscall path drives: a timer interrupt taken while EL0 was
/// running lands on the interrupted task's own kernel stack (the same
/// stack a syscall trap uses), so the installed callback can suspend that
/// task back to the scheduler exactly as `reschedule_current` does for a
/// `yield` syscall. The callback runs **after** the GIC end-of-interrupt
/// handshake (so the line is no longer active across the context switch)
/// and **only** for a tick taken from EL0 — a tick taken in EL1 never
/// preempts (the kernel is non-preemptible watch-out: a
/// half-completed kernel critical section must never be switched away
/// from).
static PREEMPT_CALLBACK_FN: AtomicUsize = AtomicUsize::new(0);

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

/// Install the IPI callback the SGI IRQ path forwards each delivered IPI
/// to. Storing a `fn` (not a closure) keeps it safe to call from
/// interrupt context: there is no captured environment to drop
/// mid-flight.
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
        // SAFETY: every store into `IPI_CALLBACK_FN` round-trips a valid
        // `extern "C" fn(CpuId)` pointer through `set_ipi_callback`.
        Some(unsafe { core::mem::transmute::<usize, extern "C" fn(CpuId)>(raw) })
    }
}

/// Install the EL0-preemption callback the timer IRQ path forwards each
/// tick taken from EL0 to (the private `PREEMPT_CALLBACK_FN` slot).
///
/// Storing a `fn` (not a closure) keeps it safe to call from interrupt
/// context: there is no captured environment to drop mid-flight. The
/// binary installs the callback (which suspends the running user task
/// back to the scheduler) before arming the timer.
pub fn set_preempt_callback(cb: extern "C" fn(CpuId)) {
    PREEMPT_CALLBACK_FN.store(cb as usize, Ordering::Relaxed);
}

/// Read the currently-installed EL0-preemption callback, if any.
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

/// Invoke the installed EL0-preemption callback for `cpu`, if any.
///
/// Called from the IRQ path **only** for a timer tick taken from EL0,
/// **after** the GIC end-of-interrupt handshake (so the timer line is no
/// longer active while the callback context-switches away). A build that
/// armed the timer without installing the callback keeps cooperative
/// scheduling — the tick is pure accounting (fail-safe).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub(crate) fn on_el0_preempt_point(cpu: CpuId) {
    let raw = PREEMPT_CALLBACK_FN.load(Ordering::Relaxed);
    if raw != 0 {
        // SAFETY: every store into `PREEMPT_CALLBACK_FN` round-trips a
        // valid `extern "C" fn(CpuId)` pointer through
        // `set_preempt_callback`; the callback carries no captured
        // environment and is safe to call from interrupt context.
        let cb: extern "C" fn(CpuId) =
            unsafe { core::mem::transmute::<usize, extern "C" fn(CpuId)>(raw) };
        cb(cpu);
    }
}

/// Index into the registered per-CPU slices for `cpu`, or `None` if no
/// [`PreemptStorage`] is registered yet.
///
/// The clamp to the published length is defence-in-depth so an
/// out-of-range `CpuId` can never index outside the slices; `None` before registration makes every per-CPU access fail
/// closed rather than dereference a null slice base.
fn per_cpu_index(cpu: CpuId) -> Option<usize> {
    let len = PREEMPT_LEN.load(Ordering::Acquire);
    if len == 0 {
        None
    } else {
        Some((cpu as usize).min(len - 1))
    }
}

/// Borrow the tick-interval slot at `idx`. `idx` must come from
/// [`per_cpu_index`] (so `idx < PREEMPT_LEN` and the base is non-null).
fn interval_slot(idx: usize) -> &'static AtomicU64 {
    let base = PREEMPT_INTERVAL_PTR.load(Ordering::Acquire);
    // SAFETY: a non-zero `PREEMPT_LEN` (which `per_cpu_index` checked
    // before yielding `idx`) is published in the same `register` call that
    // stores the non-null base from a `&'static PreemptStorage`'s
    // `interval_ticks` array of that length. `idx < len`, so `base.add(idx)`
    // is in bounds and its referent lives for `'static`.
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

/// Borrow the quantum-deadline slot at `idx`. `idx` must come from
/// [`per_cpu_index`] (so `idx < PREEMPT_LEN` and the base is non-null).
fn quantum_slot(idx: usize) -> &'static AtomicU64 {
    let base = PREEMPT_QUANTUM_PTR.load(Ordering::Acquire);
    // SAFETY: as for [`interval_slot`] — the non-null `quantum_abs` base and
    // the matching `PREEMPT_LEN` are published together, and `idx < len`.
    unsafe { &*base.add(idx) }
}

/// Borrow the wakeup-deadline slot at `idx`. `idx` must come from
/// [`per_cpu_index`] (so `idx < PREEMPT_LEN` and the base is non-null).
fn wakeup_slot(idx: usize) -> &'static AtomicU64 {
    let base = PREEMPT_WAKEUP_PTR.load(Ordering::Acquire);
    // SAFETY: as for [`interval_slot`] — the non-null `wakeup_abs` base and
    // the matching `PREEMPT_LEN` are published together, and `idx < len`.
    unsafe { &*base.add(idx) }
}

/// Decode a stored deadline slot value into [`Option`] form
/// ([`NO_DEADLINE`] ⇒ `None`).
const fn slot_deadline(raw: u64) -> Option<u64> {
    if raw == NO_DEADLINE {
        None
    } else {
        Some(raw)
    }
}

/// Record CPU `cpu`'s preemption-quantum deadline (absolute `CNTPCT_EL0`
/// ticks), or clear it with `None`, then reprogram the one-shot to the
/// earlier of the quantum and any pending wakeup.
///
/// Called from [`crate::kernel_arch::Aarch64Arch`]'s `set_preemption`. A
/// no-op before a [`PreemptStorage`] is registered (fail closed).
pub fn record_quantum_deadline(cpu: CpuId, deadline: Option<u64>) {
    if let Some(idx) = per_cpu_index(cpu) {
        quantum_slot(idx).store(deadline.unwrap_or(NO_DEADLINE), Ordering::Relaxed);
        reprogram(cpu);
    }
}

/// Record CPU `cpu`'s nearest blocking-wait deadline (absolute
/// `CNTPCT_EL0` ticks), or clear it with `None`, then reprogram the
/// one-shot to the earlier of this wakeup and any armed quantum. Called from `set_wakeup`. A no-op before a
/// [`PreemptStorage`] is registered (fail closed).
pub fn record_wakeup_deadline(cpu: CpuId, deadline: Option<u64>) {
    if let Some(idx) = per_cpu_index(cpu) {
        wakeup_slot(idx).store(deadline.unwrap_or(NO_DEADLINE), Ordering::Relaxed);
        reprogram(cpu);
    }
}

/// The currently recorded quantum / wakeup deadlines for `cpu` (each
/// `None` when unset). Test/diagnostic observer (also keeps the per-CPU
/// slots live on the host build).
#[must_use]
pub fn recorded_deadlines(cpu: CpuId) -> (Option<u64>, Option<u64>) {
    match per_cpu_index(cpu) {
        Some(idx) => (
            slot_deadline(quantum_slot(idx).load(Ordering::Relaxed)),
            slot_deadline(wakeup_slot(idx).load(Ordering::Relaxed)),
        ),
        None => (None, None),
    }
}

/// Reprogram CPU `cpu`'s single physical one-shot to fire at the earlier
/// of its recorded quantum and wakeup deadlines, or disarm it when
/// neither is pending (the tickless one-shot is armed
/// only for a real pending event).
///
/// The combining math is the shared, host-tested
/// [`rustos_arch_api::wakeup`] helper; only the `CNTPCT_EL0` read and the
/// `arm_oneshot` / `disarm` programming are aarch64-specific. Off the
/// freestanding target there is no generic timer, so the arming is inert
/// (the deadline bookkeeping above still runs for host tests).
fn reprogram(cpu: CpuId) {
    let Some(idx) = per_cpu_index(cpu) else {
        return;
    };
    let quantum = slot_deadline(quantum_slot(idx).load(Ordering::Relaxed));
    let wakeup = slot_deadline(wakeup_slot(idx).load(Ordering::Relaxed));
    let target = rustos_arch_api::wakeup::earliest(quantum, wakeup);
    #[cfg(all(target_arch = "aarch64", target_os = "none"))]
    {
        match target {
            Some(abs) => {
                let now = crate::kernel_arch::read_cntpct();
                arm_oneshot(rustos_arch_api::wakeup::ticks_from_now(abs, now));
            }
            None => disarm(),
        }
    }
    #[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
    {
        let _ = target;
    }
}

/// The recorded tick interval for `cpu` in counter ticks (`0` if unset).
/// Test/diagnostic observer.
#[must_use]
pub fn timer_interval_ticks(cpu: CpuId) -> u64 {
    match per_cpu_index(cpu) {
        Some(idx) => interval_slot(idx).load(Ordering::Relaxed),
        None => 0,
    }
}

/// The `CpuId` recorded for `cpu`'s timer slot, or `None` if
/// `init_local_preempt` has not run for it yet. Test/diagnostic observer
/// (also keeps the per-CPU slot live on the host build).
#[must_use]
pub fn timer_cpu_id(cpu: CpuId) -> Option<CpuId> {
    let idx = per_cpu_index(cpu)?;
    let recorded = cpu_id_slot(idx).load(Ordering::Relaxed);
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
    IPI_CALLBACK_FN.store(0, Ordering::Relaxed);
    PREEMPT_CALLBACK_FN.store(0, Ordering::Relaxed);
    let len = PREEMPT_LEN.load(Ordering::Acquire);
    for idx in 0..len {
        interval_slot(idx).store(0, Ordering::Relaxed);
        cpu_id_slot(idx).store(NO_CPU, Ordering::Relaxed);
        quantum_slot(idx).store(NO_DEADLINE, Ordering::Relaxed);
        wakeup_slot(idx).store(NO_DEADLINE, Ordering::Relaxed);
    }
}

#[cfg(test)]
fn reset_preempt_storage_for_tests() {
    PREEMPT_REGISTERED.store(false, Ordering::Release);
    PREEMPT_LEN.store(0, Ordering::Release);
    PREEMPT_INTERVAL_PTR.store(core::ptr::null_mut(), Ordering::Release);
    PREEMPT_CPU_ID_PTR.store(core::ptr::null_mut(), Ordering::Release);
    PREEMPT_QUANTUM_PTR.store(core::ptr::null_mut(), Ordering::Release);
    PREEMPT_WAKEUP_PTR.store(core::ptr::null_mut(), Ordering::Release);
}

/// Compute the tick interval, in counter ticks, for `hz` ticks per
/// second given a `counter_hz` clock. Clamps to at least one tick so a
/// pathological `hz > counter_hz` cannot arm a zero-interval timer that
/// re-fires without progress.
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

/// Arm the EL1 physical generic timer **one-shot** to fire once after
/// `ticks_from_now` counter ticks, then stop until armed again.
///
/// The down-counter is loaded with `ticks_from_now` (clamped to at least
/// one tick so a degenerate `0` cannot re-trap with no progress) and the timer is enabled with its interrupt unmasked
/// (`CNTP_CTL_EL0.ENABLE`, `IMASK = 0`). There is **no** periodic re-arm:
/// once it fires, the next fire happens only if the scheduler arms it
/// again via [`crate::kernel_arch::Aarch64Arch`]'s `set_preemption`
/// (tickless / `NO_HZ`). The GIC PPI must already be
/// enabled (done by [`init_local_preempt`]).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub fn arm_oneshot(ticks_from_now: u64) {
    write_tval(ticks_from_now.max(1));
    // SAFETY: enabling `CNTP_CTL_EL0.ENABLE` with `IMASK` clear starts the
    // timer and lets it raise PPI 30 once the down-counter reaches zero;
    // no memory side effects beyond the system register.
    unsafe {
        core::arch::asm!("msr CNTP_CTL_EL0, {}", in(reg) CNTP_CTL_ENABLE, options(nomem, nostack));
    }
}

/// Disarm the EL1 physical generic timer so no further interrupt fires
/// until the next [`arm_oneshot`].
///
/// Clears `CNTP_CTL_EL0` (both `ENABLE` and `IMASK`): the timer stops and
/// raises no interrupt, so a CPU running a sole runnable task takes no
/// timer ticks at all. Disarming an
/// already-stopped timer is a harmless no-op.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub fn disarm() {
    // SAFETY: clearing `CNTP_CTL_EL0` disables the timer and masks its
    // interrupt; no memory side effects beyond the system register.
    unsafe {
        core::arch::asm!("msr CNTP_CTL_EL0, {}", in(reg) 0u64, options(nomem, nostack));
    }
}

/// Initialise generic-timer preemption on `cpu`.
///
/// Records the CPU and the per-quantum `interval_ticks` (the value the
/// scheduler's one-shot is later armed to) and enables the timer PPI at
/// the GIC, but **leaves the timer disarmed**: RustOS is tickless, so the
/// timer is armed only when the scheduler has a task to bound, via
/// [`arm_oneshot`] from [`crate::kernel_arch::Aarch64Arch`]'s
/// `set_preemption` (`NO_HZ`). It does **not** unmask
/// interrupts at the PE (`DAIF`); the caller does that via
/// [`crate::exceptions::enable_irq`] once it is ready to take ticks —
/// matching the riscv64 `init_local_preempt` / `sstatus.SIE` split.
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
    let Some(slot) = per_cpu_index(cpu) else {
        // No `PreemptStorage` registered (or `cpu` outside it): fail
        // closed rather than record a quantum whose tick can never be
        // dispatched. A registered caller never hits
        // this branch.
        return;
    };
    let interval = interval_ticks.max(1);
    interval_slot(slot).store(interval, Ordering::Relaxed);
    cpu_id_slot(slot).store(u64::from(cpu), Ordering::Relaxed);

    // SAFETY: the GIC distributor is enabled by the caller's contract;
    // enabling the timer PPI lets a later-armed one-shot reach the CPU.
    unsafe {
        crate::gic::enable_ppi(TIMER_PPI);
    }

    // Leave the timer disarmed: the scheduler arms the first one-shot when
    // it dispatches a task onto a contended CPU (tickless). The
    // per-quantum interval the one-shot is later armed to is read back
    // through [`timer_interval_ticks`] (the single stored copy).
    disarm();
}

/// Handle a generic-timer interrupt: clear the timer condition and
/// dispatch the (observation-only) scheduler-tick callback.
///
/// RustOS is tickless: the timer was armed **one-shot**
/// by the scheduler, so this handler does **not** re-arm it — the next
/// fire happens only when the scheduler arms another quantum via
/// [`arm_oneshot`]. It disarms (clearing the now-asserted timer condition
/// so the IRQ does not immediately re-trap) and then dispatches the tick
/// callback. The *preemption* of the running EL0 task is driven separately
/// by [`on_el0_preempt_point`], called from the IRQ path after the GIC
/// end-of-interrupt handshake; the scheduler's next dispatch re-arms the
/// one-shot for whichever task it runs next.
///
/// Called only from [`crate::exceptions`]' IRQ path, with interrupts
/// masked (the PE masked them on exception entry).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub(crate) fn on_timer_interrupt(cpu: CpuId) {
    // Clear the fired one-shot's timer condition so the line deasserts;
    // the scheduler re-arms a fresh one-shot on its next dispatch.
    disarm();
    // The quantum (if any) just expired, so clear its recorded deadline:
    // the dispatch that follows the preempt point re-arms a fresh quantum,
    // and the per-tick wakeup sweep (the timer callback below) must not
    // re-arm the one-shot against this already-fired deadline. The wakeup deadline is owned by the sweep and left untouched.
    if let Some(slot) = per_cpu_index(cpu) {
        quantum_slot(slot).store(NO_DEADLINE, Ordering::Relaxed);
    }
    let Some(slot) = per_cpu_index(cpu) else {
        // No registered per-CPU slot for this core: nothing to dispatch
        // (fail closed).
        return;
    };
    let recorded = cpu_id_slot(slot).load(Ordering::Relaxed);
    if recorded != NO_CPU {
        // Dispatch the tick through the Arch HAL timer surface so the
        // callback invoke lives in exactly one place;
        // the HAL handle reaches the same `TIMER_CALLBACK_FN` static this
        // module owns. `recorded` was stored from a `CpuId` (`u32`) in
        // `init_local_preempt`, so the low 32 bits are the whole value.
        #[allow(clippy::cast_possible_truncation)]
        let recorded_cpu = recorded as u32;
        use rustos_arch_api::Timer;
        crate::timer_hal::TimerHal::new().dispatch_tick(recorded_cpu);
    }
}

/// Enable the inter-processor-interrupt SGI ([`IPI_SGI`]) at the GIC on
/// the calling CPU so a directed IPI raised by
/// [`crate::kernel_arch::Aarch64Arch`]'s `send_ipi` traps to this CPU's
/// IRQ path.
///
/// Like [`init_local_preempt`], this does **not** unmask interrupts at
/// the PE (`DAIF`); the caller enables IRQs via
/// [`crate::exceptions::enable_irq`] once ready — matching the riscv64
/// `enable_ipi` / `sstatus.SIE` split.
///
/// # Safety
///
/// The GIC must be initialised ([`crate::gic::init`]); enabling the SGI
/// lets a delivered IPI reach the CPU. The caller must have installed
/// the vector table ([`crate::exceptions::init_vectors`]) and the IPI
/// callback ([`set_ipi_callback`]) first.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub unsafe fn enable_ipi() {
    // SAFETY: the GIC distributor is enabled by the caller's contract;
    // enabling the IPI SGI's distributor bit lets a directed SGI reach
    // this CPU. `enable_ppi` programs the priority + set-enable bit for
    // the INTID, which is valid for an SGI as well as a PPI.
    unsafe {
        crate::gic::enable_ppi(IPI_SGI);
    }
}

/// Handle a delivered IPI (the reschedule SGI [`IPI_SGI`]): invoke the
/// installed IPI callback with the running CPU's id.
///
/// Called only from [`crate::exceptions`]' IRQ path, with interrupts
/// masked (the PE masked them on exception entry). The GIC
/// end-of-interrupt handshake stays in the IRQ path, so this handler
/// only dispatches the callback — mirroring riscv64's
/// `on_software_interrupt`.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub(crate) fn on_ipi_interrupt(cpu: CpuId) {
    let raw = IPI_CALLBACK_FN.load(Ordering::Relaxed);
    if raw != 0 {
        // SAFETY: every store into `IPI_CALLBACK_FN` round-trips a valid
        // `extern "C" fn(CpuId)` pointer through `set_ipi_callback`; the
        // callback is a `fn` with no captured environment.
        let cb: extern "C" fn(CpuId) =
            unsafe { core::mem::transmute::<usize, extern "C" fn(CpuId)>(raw) };
        cb(cpu);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern "C" fn host_cb(_cpu: CpuId) {}

    /// Serialises the tests that mutate the process-wide preemption statics
    /// (the callback slots and the registered per-CPU slices). The host test
    /// runner executes a crate's tests on parallel threads, and these statics
    /// are global: without this lock one test's `clear_for_tests` /
    /// `reset_preempt_storage_for_tests` races another's record-then-read and
    /// the suite fails intermittently. Poison is tolerated (a panicking test
    /// still leaves the statics in a defined state, which each test resets on
    /// entry) so one failure does not cascade into spurious failures.
    static GLOBAL_STATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_global_state() -> std::sync::MutexGuard<'static, ()> {
        GLOBAL_STATE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

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
        let _guard = lock_global_state();
        clear_for_tests();
        assert!(timer_callback().is_none());
        set_timer_callback(host_cb);
        let got = timer_callback().expect("callback installed");
        assert_eq!(got as usize, host_cb as *const () as usize);
        clear_for_tests();
    }

    #[test]
    fn ipi_callback_round_trips_through_its_own_slot() {
        let _guard = lock_global_state();
        clear_for_tests();
        assert!(ipi_callback().is_none());
        set_ipi_callback(host_cb);
        let got = ipi_callback().expect("ipi callback installed");
        assert_eq!(got as usize, host_cb as *const () as usize);
        // The timer slot is independent of the IPI slot.
        assert!(timer_callback().is_none());
        clear_for_tests();
    }

    #[test]
    fn preempt_callback_round_trips_through_its_own_slot() {
        let _guard = lock_global_state();
        clear_for_tests();
        assert!(preempt_callback().is_none());
        set_preempt_callback(host_cb);
        let got = preempt_callback().expect("preempt callback installed");
        assert_eq!(got as usize, host_cb as *const () as usize);
        // The preempt slot is independent of the timer and IPI slots, so
        // arming preemption never disturbs the tick/IPI dispatch.
        assert!(timer_callback().is_none());
        assert!(ipi_callback().is_none());
        clear_for_tests();
    }

    #[test]
    fn ipi_sgi_is_a_software_generated_interrupt() {
        // INTIDs 0..16 are SGIs (GICv2 §2.2.1); the IPI uses INTID 0.
        const _: () = assert!(IPI_SGI < 16, "the IPI INTID must be an SGI");
        assert_eq!(IPI_SGI, 0);
    }

    #[test]
    fn per_cpu_slots_track_the_registered_storage() {
        // A caller-sized backing covers exactly its `N` slots (the
        // capacity is the discovered core count, not a baked-in `MAX_CPUS`);
        // a second backing proves registration is set-once. Declared first so
        // they precede the statements that drive them.
        static STORAGE: PreemptStorage<4> = PreemptStorage::new();
        static STORAGE2: PreemptStorage<2> = PreemptStorage::new();

        let _guard = lock_global_state();
        reset_preempt_storage_for_tests();

        // Before any storage is registered, every per-CPU observer fails
        // closed (`0` / `None`) instead of dereferencing a null base.
        assert_eq!(per_cpu_index(0), None);
        assert_eq!(timer_interval_ticks(0), 0);
        assert_eq!(timer_cpu_id(0), None);

        assert_eq!(STORAGE.register(), Ok(4));
        assert_eq!(per_cpu_index(0), Some(0));
        assert_eq!(per_cpu_index(3), Some(3));
        // An out-of-range id clamps to the last slot rather than indexing
        // past the slice end.
        assert_eq!(per_cpu_index(4), Some(3));
        assert_eq!(per_cpu_index(u32::MAX), Some(3));

        // Recording a CPU's interval/id round-trips through the published
        // slices (the bare-metal `init_local_preempt` writes the same
        // slots).
        let idx = per_cpu_index(2).expect("registered slot");
        interval_slot(idx).store(99, Ordering::Relaxed);
        cpu_id_slot(idx).store(u64::from(2u32), Ordering::Relaxed);
        assert_eq!(timer_interval_ticks(2), 99);
        assert_eq!(timer_cpu_id(2), Some(2));

        // The tickless one-shot combiner's per-CPU deadline bookkeeping
        // (Design D P-2): recording a quantum and/or a wakeup round-trips
        // through `recorded_deadlines`, and clearing with `None` removes it.
        // (The physical-timer arming inside `reprogram` is cfg-gated to the
        // freestanding target, so on the host only the bookkeeping runs.)
        assert_eq!(recorded_deadlines(1), (None, None));
        record_quantum_deadline(1, Some(5_000));
        assert_eq!(recorded_deadlines(1), (Some(5_000), None));
        record_wakeup_deadline(1, Some(3_000));
        assert_eq!(recorded_deadlines(1), (Some(5_000), Some(3_000)));
        // The combiner would arm the earlier of the two (the wakeup here).
        assert_eq!(
            rustos_arch_api::wakeup::earliest(Some(5_000), Some(3_000)),
            Some(3_000)
        );
        record_quantum_deadline(1, None);
        record_wakeup_deadline(1, None);
        assert_eq!(recorded_deadlines(1), (None, None));

        // Registration is set-once: a second backing is refused rather
        // than silently re-pointing the live slices.
        assert_eq!(
            STORAGE2.register(),
            Err(PreemptStorageError::AlreadyRegistered)
        );

        clear_for_tests();
        assert_eq!(timer_interval_ticks(2), 0);
        assert_eq!(timer_cpu_id(2), None);
        reset_preempt_storage_for_tests();
    }
}
