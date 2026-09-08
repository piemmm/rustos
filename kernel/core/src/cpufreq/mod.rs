//! CPU frequency: measuring the live core clock, and deciding what it should
//! be.
//!
//! Two halves that answer different questions and must not be confused with
//! one another:
//!
//! * `estimate` measures what each core is *actually* running at, from the
//!   silicon's own counters, for the System Information API to report. It
//!   never asks for a rate and never trusts one it was told.
//! * `governor` and `domain` decide what the machine *should* be running
//!   at and publish that to whichever user-space driver holds the mechanism
//!   role. They never report a target as though it were achieved.
//!
//! Keeping those apart is what makes the pair honest: a mechanism whose rate
//! silently failed shows up as a measured frequency that does not match the
//! target, rather than as a target read back to itself.
//!
//! # What the machine does
//!
//! The rate follows measured utilisation: a machine that is a few percent
//! busy asks for the floor, one that is fully busy asks for the ceiling, and
//! one in between asks proportionally. Sustained work climbs as the filters
//! fill, and a machine that falls quiet walks back down a step at a time.
//!
//! Leaving idle is *not* itself a reason to go fast — a CPU that wakes for a
//! millisecond and parks again has not earned the top rate, and treating
//! every wake as if it had pinned an idle desktop at its ceiling. Starting a
//! **program** is the exception, because it is latency-critical before it has
//! run an instruction and spends most of its time waiting on a volume where
//! no utilisation accrues, so the kernel commits to the maximum outright for
//! that.
//!
//! None of it arms a timer. The dispatch loop already brackets idle exactly,
//! so both hooks below ride transitions the loop was making anyway, and the
//! utilisation filter is advanced lazily at the moment somebody reads it. On a
//! machine with no frequency driver — every port but the Raspberry Pi today —
//! the whole cost is a per-CPU slot lookup and one relaxed load per dispatch
//! step, and nothing at all beyond that.

mod domain;
mod estimate;
mod governor;

#[cfg(test)]
mod tests;

pub use estimate::{current_freq_hz, enable_this_cpu, install, is_supported, reference_hz, sample};

pub(crate) use domain::{bind, note_launch, release_process, wait, TargetWaiter};

use core::sync::atomic::Ordering;

use tairix_kernel_sched_api::CpuId;

/// Serialise a test that touches the machine-global mechanism state.
///
/// The binding, the boost deadline, and the per-CPU filters are one machine's
/// worth of state, so two tests running at once would read each other's. Both
/// this module's own tests and the syscall-handler tests hold this one lock,
/// which is why it lives here rather than beside either of them.
#[cfg(test)]
pub(crate) fn with_mechanism_lock<R>(body: impl FnOnce() -> R) -> R {
    static MECHANISM_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = MECHANISM_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    body()
}

/// Note that `cpu` is running work as of `now_ns`.
///
/// Called at the top of **every** dispatch-loop iteration, which is where a
/// CPU resumes from its idle park — but also where it arrives after every
/// ordinary dispatch. So the idle→active *edge* is detected here, and only an
/// edge does any work: a CPU already recorded active costs one slot lookup, one
/// relaxed load, and a branch.
///
/// That distinction is load-bearing for both halves. The estimator moves its
/// sampling baseline on the edge, because the core-clock counter was gated
/// while the core was parked and a window straddling the park would report a
/// duty cycle rather than a frequency — but moving it on every dispatch step
/// would leave every window too short to divide, and the estimator would
/// publish nothing at all. The governor folds the idle span that just ended
/// into the CPU's utilisation and asks for full speed for the work that has
/// evidently arrived, which is a statement about the span, not about a step.
///
/// The edge itself is a fact about the CPU rather than about either consumer,
/// so it is tracked whether or not a frequency mechanism is bound: the
/// estimator needs it on every port, and the governor's own accounting behind
/// it is what gates on the binding.
pub(crate) fn note_active(cpu: CpuId, now_ns: u64) {
    let Some(state) = crate::cpu_state::get(cpu) else {
        return;
    };
    if state.cpu_active_since.load(Ordering::Relaxed) != 0 {
        return;
    }
    estimate::rebase(cpu);
    domain::on_active_edge(state, now_ns);
    state
        .cpu_active_since
        .store(now_ns.max(1), Ordering::Relaxed);
}

/// Note that `cpu` is about to park as of `now_ns`.
///
/// Called immediately before the dispatch loop's idle wait, and the mirror of
/// [`note_active`]: it detects the active→idle edge and closes the busy span
/// for the governor. The estimator needs nothing here — it is the resumption
/// that invalidates its baseline, not the park.
pub(crate) fn note_idle(cpu: CpuId, now_ns: u64) {
    let Some(state) = crate::cpu_state::get(cpu) else {
        return;
    };
    if state.cpu_active_since.load(Ordering::Relaxed) == 0 {
        return;
    }
    domain::on_idle_edge(state, now_ns);
    state.cpu_active_since.store(0, Ordering::Relaxed);
}
