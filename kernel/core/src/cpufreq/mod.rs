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
//! every wake as if it had, pinned an idle desktop at its ceiling. Starting a
//! **program** is the exception, because it is latency-critical before it has
//! run an instruction and spends most of its time waiting on a volume where
//! no utilisation accrues, so the kernel commits to the maximum outright for
//! that.
//!
//! # The two halves watch different edges
//!
//! They must, because they are asking about different things, and sharing one
//! edge marker is what made each of them wrong once:
//!
//! * The governor's edge is **work**: a dispatch that ran a task body opens
//!   the busy span, and one that found nothing closes it. A dispatcher looking
//!   for work is not a CPU doing any.
//! * The estimator's edge is the **park**: the core clock stops there while
//!   its reference does not, so the sampling baseline is invalidated on the
//!   way in and re-seeded on the way out.
//!
//! None of it arms a timer. Both edges ride transitions the dispatch loop was
//! making anyway, and the utilisation filter is advanced lazily at the moment
//! somebody reads it. On a machine with no frequency driver — every port but
//! the Raspberry Pi today — the governor's whole cost is one relaxed load per
//! dispatch step, and nothing at all beyond that.

mod domain;
mod estimate;
mod governor;

#[cfg(test)]
mod tests;

pub use estimate::{current_freq_hz, enable_this_cpu, install, is_supported, reference_hz, sample};

pub(crate) use domain::{bind, note_launch, release_process, wait, TargetWaiter};

use core::sync::atomic::Ordering;

use tairix_kernel_sched_api::CpuId;
#[cfg(test)]
use tairix_kernel_sec::ProcessId;

/// Serialise a test that touches the machine-global mechanism state.
///
/// The binding, the boost deadline, and the per-CPU filters are one machine's
/// worth of state, so two tests running at once would read each other's. This
/// module's own tests, the syscall-handler tests, and the dispatch-loop tests
/// all hold this one lock, which is why it lives here rather than beside any
/// of them.
#[cfg(test)]
pub(crate) fn with_mechanism_lock<R>(body: impl FnOnce() -> R) -> R {
    static MECHANISM_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = MECHANISM_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    body()
}

/// Run `body` with `limits` bound to `process` at `now_ns`, holding the
/// machine-global mechanism state for the duration.
///
/// The release is a drop guard rather than a statement after `body`, so a
/// failing assertion cannot strand the role: a stranded binding refuses every
/// later `bind`, which turns one test's failure into a cascade through every
/// other test that needs the role.
#[cfg(test)]
pub(crate) fn with_bound_mechanism<R>(
    process: ProcessId,
    limits: tairix_abi::cpufreq::CpuFreqLimits,
    now_ns: u64,
    body: impl FnOnce(u64) -> R,
) -> R {
    struct Release(ProcessId);
    impl Drop for Release {
        fn drop(&mut self) {
            assert!(
                domain::release_process(self.0),
                "the binding must still be the one this test took"
            );
        }
    }
    with_mechanism_lock(|| {
        // A previous test that stranded the role would otherwise fail every
        // bind from here on; releasing first is what keeps a failure local.
        let _ = domain::release_process(process);
        let handle = domain::bind(process, limits, now_ns).expect("the role is free");
        let _release = Release(process);
        body(handle)
    })
}

/// Note that `cpu` executed a task body over the span ending at `now_ns`.
///
/// Called from the dispatch loop's `Ran` arm, and only an *edge* does any
/// work: a CPU already recorded busy costs one relaxed load, one slot lookup,
/// and a branch. `now_ns` is the iteration's top-of-loop instant, so the span
/// covers the run rather than starting after it and tiles exactly against
/// [`note_idle`]'s.
///
/// A dispatch that ran nothing is not busy, and this is the only thing that
/// opens the span. Utilisation has to be the quantity the system reports as
/// busy time (`Scheduler::cpu_busy_ticks`, time inside task bodies) or the two
/// contradict each other, which is how a machine at a few percent load came to
/// be pinned at its ceiling.
pub(crate) fn note_active(cpu: CpuId, now_ns: u64) {
    if !domain::is_bound() {
        return;
    }
    let Some(state) = crate::cpu_state::get(cpu) else {
        return;
    };
    if state.cpu_active_since.load(Ordering::Relaxed) != 0 {
        return;
    }
    domain::on_active_edge(state, now_ns);
    state
        .cpu_active_since
        .store(now_ns.max(1), Ordering::Relaxed);
}

/// Note that `cpu`'s dispatch found nothing to run as of `now_ns` — the
/// mirror of [`note_active`], closing the busy span.
pub(crate) fn note_idle(cpu: CpuId, now_ns: u64) {
    if !domain::is_bound() {
        return;
    }
    let Some(state) = crate::cpu_state::get(cpu) else {
        return;
    };
    if state.cpu_active_since.load(Ordering::Relaxed) == 0 {
        return;
    }
    domain::on_idle_edge(state, now_ns);
    state.cpu_active_since.store(0, Ordering::Relaxed);
}

/// Note that `cpu` is about to park, so the live-clock estimator must not
/// measure across the stop.
///
/// The core-clock counter is gated with the PE on most ports while its
/// reference counter keeps running, so a window straddling a park divides
/// running cycles by running-plus-parked ticks. Invalidating the baseline here
/// is what makes that impossible: the interrupt that ends the park samples
/// from interrupt context before the dispatch loop regains control, so it
/// finds no baseline to divide against and re-seeds instead of publishing a
/// duty cycle. Called with device interrupts masked, immediately before the
/// wait.
pub(crate) fn note_park(cpu: CpuId) {
    estimate::invalidate(cpu);
}

/// Note that `cpu` has resumed, re-seeding the estimator's baseline from the
/// live counters so the *first* sample after a park still yields a figure —
/// one measured over running time only.
pub(crate) fn note_resume(cpu: CpuId) {
    estimate::rebase(cpu);
}
