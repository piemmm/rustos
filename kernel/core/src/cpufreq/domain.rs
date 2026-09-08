//! The bound mechanism and the target published to it.
//!
//! Exactly one frequency mechanism serves a machine, so this holds one
//! binding: the process that took the role, the handle it was issued, the
//! [`CpuFreqLimits`] it declared, and the target currently published to it.
//! A second bind is refused rather than arbitrated — two drivers taking turns
//! on one clock would be worse than neither.
//!
//! # What is on the hot path and what is not
//!
//! The dispatch loop's idle brackets run thousands of times a second on every
//! CPU, so they take no lock and do no survey. Each one folds *its own* CPU's
//! utilisation through per-CPU atomics, and on the idle→active edge extends
//! [`BOOST_UNTIL_NS`] with one `fetch_max`. A wake is flagged only when the
//! previous boost had already lapsed, which bounds wakes to one per
//! [`RESPONSE_WINDOW_NS`] however often the machine idles and wakes: while a
//! boost is live the published target is already the maximum, so there is
//! nothing to tell the mechanism.
//!
//! Deciding the rate needs every CPU's utilisation, which is O(number of
//! CPUs) — so it happens in the waiter, under the binding lock, at most once
//! per window per direction. Holding the lock across that survey is what
//! makes the sequence and the rate advance together, with no pair a reader
//! can catch half-updated and no second copy of either. Nothing an interrupt
//! handler runs touches this lock.
//!
//! # Bounded without a caller pacing it
//!
//! [`wait`] parks with a deadline of its own choosing: one window ahead while
//! the target is above the minimum, and none once it has settled there. So a
//! quiescing machine walks down a step per window and then sleeps until real
//! work arrives, and the driver needs neither a timeout argument nor a poll.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tairix_abi::cpufreq::{CpuFreqLimits, CpuFreqTarget};
use tairix_abi::Errno;
use tairix_kernel_sec::ProcessId;
use tairix_sync::SpinLock;

use super::governor::{fold, target_hz, RESPONSE_WINDOW_NS, UTIL_ONE};
use crate::cpu_state::{self, CpuState};

/// The live mechanism binding.
struct Binding {
    /// The process that took the role. Its teardown releases the binding.
    process: ProcessId,
    /// The handle issued to it, which every [`wait`] re-checks.
    handle: u64,
    /// The range the mechanism declared.
    limits: CpuFreqLimits,
    /// The rate currently asked for, and the sequence that advances with it.
    /// Both live here so they can never be read out of step.
    target_hz: u64,
    seq: u64,
}

/// The one binding, or `None` when the machine has no mechanism.
static BINDING: SpinLock<Option<Binding>> = SpinLock::new(None);

/// Whether a mechanism is bound — the idle path's whole gate.
///
/// One relaxed load, so a machine with no frequency driver (every port but
/// the Raspberry Pi today) pays exactly that per idle transition and nothing
/// else. It duplicates no value from [`Binding`]; it answers only "is there
/// anything to account for".
static BOUND: AtomicBool = AtomicBool::new(false);

/// Monotonic time the current boost expires. A wake or a launch extends it;
/// nothing shortens it.
static BOOST_UNTIL_NS: AtomicU64 = AtomicU64::new(0);

/// The next binding handle to issue. Monotonic from one, so a handle left
/// over from a released binding cannot name the next one, and zero is never
/// issued.
static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);

/// Take the mechanism role for `process` over `limits` as of `now_ns`,
/// returning its handle.
///
/// # Errors
///
/// [`Errno::AlreadyExists`] when a live binding is already held.
pub(crate) fn bind(process: ProcessId, limits: CpuFreqLimits, now_ns: u64) -> Result<u64, Errno> {
    let mut slot = BINDING.lock();
    if slot.is_some() {
        return Err(Errno::AlreadyExists);
    }
    // The handle names the binding rather than carrying authority: the
    // capability gate is the authority and every wait re-checks the holding
    // process, exactly as an `irq_bind` handle is checked against its owner.
    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    *slot = Some(Binding {
        process,
        handle,
        limits,
        // No rate has been asked for yet. The minimum is a legal target, so
        // zero — which no legal range contains — is what marks "none", and
        // the first decision therefore always publishes.
        target_hz: 0,
        seq: 0,
    });

    // Start every filter from a clean slate dated *now*, and cover the window
    // it takes to fill with a boost.
    //
    // Two things would otherwise go wrong. A filter still dated time zero
    // reads the whole history of the boot as one idle span, so the first
    // target would be the *minimum* — actively slowing a machine that is
    // demonstrably busy launching this very driver. And on a *re*-bind, the
    // idle brackets have been gated off since the last binding was released,
    // so a CPU recorded active back then still reads active: it would never
    // take another idle→active edge and would count as busy forever.
    //
    // Clearing fabricates no utilisation. It says the governor does not know
    // yet, and a governor that does not know must not slow the machine down.
    for state in cpu_state::states() {
        state.cpu_active_since.store(0, Ordering::Relaxed);
        state.gov_util.store(0, Ordering::Relaxed);
        state.gov_folded_ns.store(now_ns, Ordering::Relaxed);
    }
    BOOST_UNTIL_NS.store(now_ns.saturating_add(RESPONSE_WINDOW_NS), Ordering::Relaxed);
    // Arm the idle path last, once the binding behind it exists.
    BOUND.store(true, Ordering::Release);
    Ok(handle)
}

/// Release the binding if `process` holds it, and report whether it did.
///
/// Called from the one shared task-reclaim path, so a driver that exits,
/// faults, or is killed leaves no binding behind and a replacement can take
/// the role.
pub(crate) fn release_process(process: ProcessId) -> bool {
    let mut slot = BINDING.lock();
    if slot.as_ref().is_none_or(|bound| bound.process != process) {
        return false;
    }
    // Disarm the idle path first: a hook that reads the gate as clear does
    // nothing at all, so nothing accounts against a binding being torn down.
    BOUND.store(false, Ordering::Release);
    *slot = None;
    BOOST_UNTIL_NS.store(0, Ordering::Relaxed);
    true
}

/// Account for a CPU's idle→active edge at `now_ns`.
///
/// The caller ([`super::note_active`]) owns the edge detection, since the
/// live-clock estimator needs the same edge on every port. This folds the idle
/// span that just ended into the CPU's filter and extends the boost, so work
/// that has just arrived is served at full speed rather than at whatever rate
/// the quiet machine had settled to. One relaxed load on a machine with no
/// mechanism bound.
pub(crate) fn on_active_edge(state: &CpuState, now_ns: u64) {
    if !BOUND.load(Ordering::Acquire) {
        return;
    }
    fold_span(state, now_ns, false);
    demand_rose(now_ns);
}

/// Account for a CPU's active→idle edge at `now_ns` — the mirror of
/// [`on_active_edge`].
///
/// It folds the busy span that just ended and flags nothing: a rate only ever
/// falls at the waiter's own paced re-evaluation, so a CPU parking never costs
/// a survey or a wake.
pub(crate) fn on_idle_edge(state: &CpuState, now_ns: u64) {
    if !BOUND.load(Ordering::Acquire) {
        return;
    }
    fold_span(state, now_ns, true);
}

/// Note that a program is being launched as of `now_ns`.
///
/// A launch is latency-sensitive before it has done anything measurable, and
/// much of it is spent waiting on the volume the bundle is read from — during
/// which every CPU may be idle and no utilisation accrues at all. So it says
/// so directly rather than waiting to be inferred.
pub(crate) fn note_launch(now_ns: u64) {
    if !BOUND.load(Ordering::Acquire) {
        return;
    }
    demand_rose(now_ns);
}

/// Extend the boost to a window past `now_ns`, and tell the mechanism if it
/// does not already know.
///
/// The wake is flagged only when the previous boost had *lapsed*. While one
/// is live the published target is already the maximum, so there is nothing
/// to say — which is what bounds wakes to one per window however often the
/// machine idles. Stamping before testing is what makes that safe: a waiter
/// mid-decision has either not yet read the boost, and will before it parks,
/// or has already published the maximum.
fn demand_rose(now_ns: u64) {
    let until = now_ns.saturating_add(RESPONSE_WINDOW_NS);
    let previous = BOOST_UNTIL_NS.fetch_max(until, Ordering::Relaxed);
    if previous <= now_ns {
        crate::waitq::cpufreq_wake();
    }
}

/// Fold the span ending at `now_ns` into `state`'s filter.
///
/// `busy` says what the CPU was doing throughout it, which the dispatch
/// loop's brackets guarantee is one thing or the other. A clock that appears
/// to have gone backwards folds nothing rather than a negative span.
fn fold_span(state: &CpuState, now_ns: u64, busy: bool) {
    let folded_ns = state.gov_folded_ns.load(Ordering::Relaxed);
    let span = now_ns.saturating_sub(folded_ns);
    let util = state.gov_util.load(Ordering::Relaxed);
    state
        .gov_util
        .store(fold(util, span, busy), Ordering::Relaxed);
    state.gov_folded_ns.store(now_ns, Ordering::Relaxed);
}

/// `cpu`'s utilisation as of `now_ns`, advancing its filter over the span
/// since the last fold without committing it.
///
/// Reading rather than committing is what lets an idle CPU's utilisation
/// decay with no timer: the span since it parked is folded in at the moment
/// somebody asks.
fn util_at(state: &CpuState, now_ns: u64) -> u64 {
    let folded_ns = state.gov_folded_ns.load(Ordering::Relaxed);
    let util = state.gov_util.load(Ordering::Relaxed);
    let active_since = state.cpu_active_since.load(Ordering::Relaxed);
    fold(util, now_ns.saturating_sub(folded_ns), active_since != 0)
}

/// The busiest CPU's utilisation as of `now_ns`.
///
/// The machine has one clock, so the busiest CPU sets the rate: scaling to an
/// average would under-serve a single-threaded workload on an otherwise idle
/// machine, which is most of what a desktop does.
fn peak_util(now_ns: u64) -> u64 {
    let mut peak = 0;
    for state in cpu_state::states() {
        let util = util_at(state, now_ns);
        if util > peak {
            peak = util;
        }
        if peak == UTIL_ONE {
            break;
        }
    }
    peak
}

/// When a settled target could next change on its own.
///
/// A target above the minimum can fall as utilisation decays, so it is
/// revisited a window later. One already at the minimum with no boost live
/// cannot move until real work arrives, so it carries no deadline at all and
/// the machine takes no wakeup.
fn next_review(limits: &CpuFreqLimits, target: u64, now_ns: u64, boost_until: u64) -> u64 {
    if now_ns < boost_until {
        boost_until
    } else if target > limits.min_hz {
        now_ns.saturating_add(RESPONSE_WINDOW_NS)
    } else {
        crate::waitq::NO_DEADLINE
    }
}

/// The seam [`wait`] parks and reads the clock through.
///
/// Keeping the two behind a trait is what makes the wait loop host-testable:
/// the syscall handler supplies the real monotonic clock and a park off the
/// run queue, and a test supplies a scripted clock and a park that returns.
pub(crate) trait TargetWaiter {
    /// The kernel monotonic clock, in nanoseconds.
    fn now_ns(&self) -> u64;
    /// Park the calling task until woken or until `deadline_ns`
    /// ([`crate::waitq::NO_DEADLINE`] for no timed wake).
    fn park(&self, deadline_ns: u64);
    /// Whether a termination is pending against the calling task, which
    /// unwinds the wait so the kill lands at the syscall boundary.
    fn kill_pending(&self) -> bool;
}

/// Decide `process`'s rate at `now_ns`, publish it if it moved, and report
/// the binding's sequence, its target, and when the answer could next change.
///
/// One critical section covers the survey, the decision, and the
/// publication, so the sequence and the rate can never be observed out of
/// step.
///
/// # Errors
///
/// [`Errno::NotFound`] when the binding is absent, held by another process,
/// or named by a different handle.
fn review(process: ProcessId, handle: u64, now_ns: u64) -> Result<(u64, u64, u64), Errno> {
    let boost_until = BOOST_UNTIL_NS.load(Ordering::Relaxed);
    let mut slot = BINDING.lock();
    let bound = match slot.as_mut() {
        Some(bound) if bound.process == process && bound.handle == handle => bound,
        _ => return Err(Errno::NotFound),
    };
    let target = target_hz(&bound.limits, peak_util(now_ns), now_ns < boost_until);
    if bound.target_hz != target {
        bound.target_hz = target;
        bound.seq += 1;
    }
    let review_at = next_review(&bound.limits, target, now_ns, boost_until);
    Ok((bound.seq, target, review_at))
}

/// Block until the published sequence differs from `last_seq`, then report
/// the target.
///
/// # Errors
///
/// * [`Errno::NotFound`] — `handle` is not `process`'s live binding,
///   including after the binding was released underneath the waiter.
/// * [`Errno::Interrupted`] — a termination unwound the wait.
pub(crate) fn wait(
    process: ProcessId,
    handle: u64,
    last_seq: u64,
    waiter: &dyn TargetWaiter,
) -> Result<CpuFreqTarget, Errno> {
    loop {
        let now = waiter.now_ns();
        let (seq, target_hz, review_at) = review(process, handle, now)?;
        if seq != last_seq {
            return Ok(CpuFreqTarget { seq, target_hz });
        }
        waiter.park(review_at);
        if waiter.kill_pending() {
            return Err(Errno::Interrupted);
        }
    }
}
