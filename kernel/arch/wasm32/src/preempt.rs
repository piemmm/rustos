//! Cooperative, `requestAnimationFrame`-driven preemption on wasm32.
//!
//! The wasm32 analogue of the bare-metal ports' timer-interrupt
//! preemption. A WebAssembly module cannot be pre-empted by a hardware
//! timer — it runs to completion on the host's single JavaScript turn —
//! so TAIRiX yields *cooperatively*: the host schedules a
//! `requestAnimationFrame` callback that re-enters [`on_animation_frame`],
//! which drives one scheduler tick and asks the host for the next frame.
//! This module owns that surface:
//!
//! * The set-once tick callback ([`set_tick_callback`]) each frame
//!   forwards to, with this context's [`CpuId`].
//! * [`init_local_preempt`], which records this context's `CpuId` and
//!   requests the first animation frame.
//! * [`on_animation_frame`], called by the host each frame: it invokes
//!   the tick callback, counts the tick, and requests the next frame.
//!
//! The kernel-side preemption logic lives in
//! `kernel/sched::Scheduler::on_timer_tick`; this module only wires the
//! wasm32 frame loop into that architecture-neutral surface (no interface creep).
//!
//! # Inter-context interrupts
//!
//! A directed reschedule to another worker arrives over a
//! `MessageChannel` post (`crate::kernel_arch::WasmArch::send_ipi`); the
//! receiving worker's message handler calls [`on_ipi_message`], which
//! runs the installed IPI callback with that context's `CpuId`,
//! mirroring the frame path.
//!
//! # Per-context state
//!
//! Each Web Worker runs a distinct WebAssembly module instance with its
//! own linear memory and its own copy of these statics, so the recorded
//! `CpuId` and the tick counter are naturally per-context — no
//! per-worker array is needed (contrast the bare-metal ports, where all
//! harts share one address space).
//!
//! # Host testability
//!
//! The callback storage, the `CpuId` slot, the tick counter, and the
//! [`cooperative_budget_exhausted`] helper are plain atomics and a
//! `const fn`, so they build and are unit-tested on the host.
//! [`on_animation_frame`] and [`on_ipi_message`] are host-callable (so a
//! test can drive a tick directly); only the host `requestAnimationFrame`
//! re-arm is gated to the wasm target.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use tairix_arch_api::{CpuId, Timer};

/// `u64` sentinel meaning "no `CpuId` recorded for this context yet".
const NO_CPU: u64 = u32::MAX as u64;

/// The callback the frame loop forwards each tick to, packed into a
/// `usize` so it swaps in without a lock. Set up before the first frame.
static TICK_CALLBACK_FN: AtomicUsize = AtomicUsize::new(0);

/// The IPI callback the `MessageChannel` handler forwards each delivered
/// reschedule to, packed into a `usize`. Set up before any IPI is sent.
static IPI_CALLBACK_FN: AtomicUsize = AtomicUsize::new(0);

/// This context's `CpuId`, passed to both callbacks; [`NO_CPU`] until
/// [`init_local_preempt`] records it.
static TICK_CPU_ID: AtomicU64 = AtomicU64::new(NO_CPU);

/// Count of animation-frame ticks delivered to this context. Observed by
/// the browser harness to prove the frame loop drives the scheduler.
static TICK_COUNT: AtomicU64 = AtomicU64::new(0);

/// Install the per-context tick callback.
///
/// Invoked from the frame loop on every tick with this context's
/// [`CpuId`]. Storing a `fn` (not a closure) keeps it safe to call from
/// the host callback: there is no captured environment to drop.
pub fn set_tick_callback(cb: extern "C" fn(CpuId)) {
    TICK_CALLBACK_FN.store(cb as usize, Ordering::Release);
}

/// Read the currently-installed tick callback, if any. Test/diagnostic.
#[must_use]
pub fn tick_callback() -> Option<extern "C" fn(CpuId)> {
    decode_callback(TICK_CALLBACK_FN.load(Ordering::Acquire))
}

/// Install the IPI callback the `MessageChannel` handler forwards each
/// delivered reschedule to. Storing a `fn` (not a closure) keeps it safe
/// to call from the host callback.
pub fn set_ipi_callback(cb: extern "C" fn(CpuId)) {
    IPI_CALLBACK_FN.store(cb as usize, Ordering::Release);
}

/// Read the currently-installed IPI callback, if any. Test/diagnostic.
#[must_use]
pub fn ipi_callback() -> Option<extern "C" fn(CpuId)> {
    decode_callback(IPI_CALLBACK_FN.load(Ordering::Acquire))
}

fn decode_callback(raw: usize) -> Option<extern "C" fn(CpuId)> {
    if raw == 0 {
        None
    } else {
        // SAFETY: every store into a callback slot round-trips a valid
        // `extern "C" fn(CpuId)` pointer through `set_*_callback`.
        Some(unsafe { core::mem::transmute::<usize, extern "C" fn(CpuId)>(raw) })
    }
}

/// This context's recorded `CpuId`, or `u32::MAX` if unset.
/// Test/diagnostic observer.
#[must_use]
pub fn tick_cpu_id() -> u32 {
    // The slot only ever holds a value stored from a `CpuId` (`u32`) or
    // the `u32::MAX` sentinel, so the low 32 bits are the whole value.
    #[allow(clippy::cast_possible_truncation)]
    let cpu = TICK_CPU_ID.load(Ordering::Acquire) as u32;
    cpu
}

/// Count of animation-frame ticks this context has delivered.
#[must_use]
pub fn tick_count() -> u64 {
    TICK_COUNT.load(Ordering::Acquire)
}

#[cfg(test)]
fn clear_for_tests() {
    TICK_CALLBACK_FN.store(0, Ordering::Release);
    IPI_CALLBACK_FN.store(0, Ordering::Release);
    TICK_CPU_ID.store(NO_CPU, Ordering::Release);
    TICK_COUNT.store(0, Ordering::Release);
}

/// `true` once `elapsed_ms` of work in a single frame has reached the
/// cooperative `budget_ms`, signalling the scheduler should yield back to
/// the host rather than starve rendering and input.
///
/// A non-positive `budget_ms` (a misconfiguration) yields immediately so
/// a degenerate budget can never let a frame run unbounded (fail closed). A non-finite `elapsed_ms` likewise yields.
#[must_use]
pub fn cooperative_budget_exhausted(elapsed_ms: f64, budget_ms: f64) -> bool {
    if !budget_ms.is_finite() || budget_ms <= 0.0 {
        return true;
    }
    if !elapsed_ms.is_finite() {
        return true;
    }
    elapsed_ms >= budget_ms
}

/// Initialise cooperative preemption for this context.
///
/// Records this context's `cpu` and requests the first animation frame.
/// The tick callback must already be installed via [`set_tick_callback`].
pub fn init_local_preempt(cpu: CpuId) {
    TICK_CPU_ID.store(u64::from(cpu), Ordering::Release);
    request_frame();
}

/// Handle one animation frame: invoke the installed tick callback with
/// this context's recorded `CpuId`, count the tick, then request the
/// next frame so the loop continues.
///
/// Called by the host `requestAnimationFrame` callback (and directly by
/// the host unit tests). Requesting the next frame last keeps the
/// scheduler running at least one tick before the loop can re-enter.
pub fn on_animation_frame() {
    let cpu = TICK_CPU_ID.load(Ordering::Acquire);
    if cpu != NO_CPU {
        // Dispatch the tick through the Arch HAL timer surface so the
        // callback invoke lives in exactly one place;
        // the HAL handle reaches the same `TICK_CALLBACK_FN` static this
        // module owns. `cpu` was stored from a `CpuId` (`u32`), so the
        // low 32 bits are the whole value.
        #[allow(clippy::cast_possible_truncation)]
        let cpu = cpu as u32;
        if crate::timer_hal::TimerHal::new().dispatch_tick(cpu) {
            TICK_COUNT.fetch_add(1, Ordering::AcqRel);
        }
    }
    request_frame();
}

/// Handle a delivered inter-context reschedule (a `MessageChannel`
/// message): invoke the installed IPI callback with this context's
/// recorded `CpuId`.
///
/// Called by the host message handler (and directly by the host unit
/// tests). Unlike the frame path it does not re-arm anything — the
/// sender drives delivery.
pub fn on_ipi_message() {
    let cpu = TICK_CPU_ID.load(Ordering::Acquire);
    if let Some(cb) = ipi_callback() {
        if cpu != NO_CPU {
            #[allow(clippy::cast_possible_truncation)]
            let cpu = cpu as u32;
            cb(cpu);
        }
    }
}

/// Ask the host to schedule the next animation frame.
#[cfg(target_arch = "wasm32")]
fn request_frame() {
    crate::bindings::host_request_frame();
}

/// Host no-op: there is no `requestAnimationFrame` off the wasm target,
/// so the unit tests drive [`on_animation_frame`] directly. Never linked
/// into a wasm image.
#[cfg(not(target_arch = "wasm32"))]
fn request_frame() {}

/// Serialises the host tests that mutate this module's process-global
/// callback / counter statics — both the [`crate::preempt`] suite and
/// the [`crate::timer_hal`] conformance vertical, which forwards to the
/// same statics. Lives here (not in the test module) so both files can
/// share one lock and the suites do not race (no flaky
/// tests).
#[cfg(test)]
static STATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Acquire the shared host-test serialisation lock. A poisoned lock from
/// an unrelated test panic must not cascade, so the guard is recovered.
#[cfg(test)]
pub(crate) fn test_state_lock() -> std::sync::MutexGuard<'static, ()> {
    STATE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
#[path = "preempt_tests.rs"]
mod tests;
