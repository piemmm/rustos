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
//! The dispatch loop's work brackets run thousands of times a second on every
//! CPU, so they take no lock and do no survey. Each one folds *its own* CPU's
//! utilisation through per-CPU atomics, and asks nothing for it: work arriving
//! grants no rate here, only [`attention`], which flags a task wake at most
//! once per [`ACTIVE_REVIEW_NS`] however often the machine goes quiet.
//!
//! Deciding the rate needs every CPU's utilisation, which is O(number of
//! CPUs) — so it happens in the waiter, under the binding lock. Holding the
//! lock across that [`survey`] is what makes the sequence and the rate
//! advance together, with no pair a reader can catch half-updated and no
//! second copy of either. Nothing an interrupt handler runs touches this
//! lock.
//!
//! # Bounded without a caller pacing it
//!
//! [`wait`] parks with a deadline of its own choosing: the active cadence
//! while work is running, a window while the rate is above the floor and
//! decaying, and none once it has settled at the floor with nothing running.
//! So a busy machine's rate climbs as its filters fill, a quiescing one walks
//! back down a step per window, and a quiet one sleeps until real work
//! arrives — and the driver needs neither a timeout argument nor a poll.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tairix_abi::cpufreq::{CpuFreqLimits, CpuFreqTarget};
use tairix_abi::Errno;
use tairix_kernel_sec::ProcessId;
use tairix_sync::SpinLock;

use super::governor::{
    fold, held_target_hz, hold_expiry, target_hz, ACTIVE_REVIEW_NS, RESPONSE_WINDOW_NS,
};
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
    /// Monotonic time [`Self::target_hz`] last *became* the maximum, or `0`
    /// while it is not the maximum. Set on the transition only, so a machine
    /// that stays at the ceiling cannot keep extending its own hold.
    at_max_since: u64,
}

/// The one binding, or `None` when the machine has no mechanism.
static BINDING: SpinLock<Option<Binding>> = SpinLock::new(None);

/// Whether a mechanism is bound. It duplicates no value from [`Binding`]; it
/// answers only "is there anything to account for".
static BOUND: AtomicBool = AtomicBool::new(false);

/// The dispatch hooks' whole gate, so a port with no frequency driver pays one
/// relaxed load per dispatch step.
pub(super) fn is_bound() -> bool {
    BOUND.load(Ordering::Acquire)
}

/// Monotonic time the current launch boost expires. Only starting a program
/// extends it, and nothing shortens it.
static LAUNCH_BOOST_UNTIL_NS: AtomicU64 = AtomicU64::new(0);

/// Monotonic time until which the waiter has already been told the machine is
/// active, so a further wake has nothing to add.
///
/// This grants no rate. It bounds how often leaving idle costs a task wake —
/// one per [`ACTIVE_REVIEW_NS`] however often the machine idles and resumes —
/// and, because a wake inside the span is therefore *not* flagged, it is also
/// a promise [`next_review`] must keep: the waiter may not park indefinitely
/// while it stands, or a CPU that resumed inside the span would have no edge
/// left to announce it.
static ATTENTION_UNTIL_NS: AtomicU64 = AtomicU64::new(0);

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
        at_max_since: 0,
    });

    // Start every filter from a clean slate dated *now*, and cover the window
    // it takes to fill with a boost.
    //
    // Two things would otherwise go wrong. A filter still dated time zero
    // reads the whole history of the boot as one idle span, so the first
    // target would be the *minimum* — actively slowing a machine that is
    // demonstrably busy launching this very driver. And on a *re*-bind, the
    // work brackets have been gated off since the last binding was released,
    // so a CPU recorded busy back then still reads busy: it would never take
    // another edge and would count as busy forever.
    //
    // Clearing fabricates no utilisation. It says the governor does not know
    // yet, and a governor that does not know must not slow the machine down.
    for state in cpu_state::states() {
        state.cpu_active_since.store(0, Ordering::Relaxed);
        state.gov_util.store(0, Ordering::Relaxed);
        state.gov_folded_ns.store(now_ns, Ordering::Relaxed);
    }
    // The mechanism driver is itself a program that has just been started, so
    // the machine is demonstrably busy: the same boost a launch gets covers
    // the window the filters take to fill.
    LAUNCH_BOOST_UNTIL_NS.store(now_ns.saturating_add(RESPONSE_WINDOW_NS), Ordering::Relaxed);
    ATTENTION_UNTIL_NS.store(now_ns.saturating_add(ACTIVE_REVIEW_NS), Ordering::Relaxed);
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
    LAUNCH_BOOST_UNTIL_NS.store(0, Ordering::Relaxed);
    ATTENTION_UNTIL_NS.store(0, Ordering::Relaxed);
    true
}

/// Account for a CPU's idle→busy edge at `now_ns` — a dispatch that ran a
/// task body after one that did not.
///
/// Folds the idle span that just ended into the CPU's filter and makes sure
/// the waiter is looking; it grants no rate, because work arriving says only
/// that *something* happened, not that it is worth the clock.
pub(crate) fn on_active_edge(state: &CpuState, now_ns: u64) {
    fold_span(state, now_ns, false);
    attention(now_ns);
}

/// Account for a CPU's busy→idle edge at `now_ns` — the mirror of
/// [`on_active_edge`].
///
/// It folds the busy span that just ended and flags nothing: a rate only ever
/// falls at the waiter's own paced re-evaluation, so a CPU running out of work
/// never costs a survey or a wake.
pub(crate) fn on_idle_edge(state: &CpuState, now_ns: u64) {
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
    let until = now_ns.saturating_add(RESPONSE_WINDOW_NS);
    let previous = LAUNCH_BOOST_UNTIL_NS.fetch_max(until, Ordering::Relaxed);
    // While a boost is live the published target is already the maximum, so
    // there is nothing to say; a lapsed one means the rate may have decayed
    // and the mechanism must be told. Stamping before testing is what makes
    // that safe — a waiter mid-decision has either not yet read the boost,
    // and will before it parks, or has already published the maximum.
    if previous <= now_ns {
        crate::waitq::cpufreq_wake();
    }
    attention(now_ns);
}

/// Make sure the waiter is looking at the machine, without asking for a rate.
///
/// A CPU that stays busy produces no further transition, so the rise has to
/// be looked for; the waiter does that on its own cadence while any CPU is
/// active, and this is what restarts that cadence after the machine has been
/// fully quiescent. The wake is flagged only when the waiter has *not* been
/// told recently, which bounds it to one per [`ACTIVE_REVIEW_NS`] however
/// often the machine idles and resumes.
fn attention(now_ns: u64) {
    let until = now_ns.saturating_add(ACTIVE_REVIEW_NS);
    let previous = ATTENTION_UNTIL_NS.fetch_max(until, Ordering::Relaxed);
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

/// What one survey of the machine found: the busiest CPU's utilisation as of
/// `now_ns`, and whether any CPU is running work.
struct Survey {
    peak_util: u64,
    any_active: bool,
}

/// Survey every CPU as of `now_ns`.
///
/// The busiest CPU sets the rate, because the machine has one clock: scaling
/// to an average would under-serve a single-threaded workload on an otherwise
/// idle machine, which is most of what a desktop does. Whether anything is
/// running comes from the same pass, since it costs nothing extra and decides
/// whether the rise is still worth looking for.
///
/// The reviewing CPU does not report itself busy merely for running this
/// review, and that is what lets a quiet machine settle rather than sustain
/// its own cadence for ever. The busy edge is stamped in the dispatch loop's
/// `Ran` arm *after* a task body returns, so while the mechanism task executes
/// its CPU still carries what its previous dispatch left: idle where it woke
/// from a park to serve this review, busy where it was already working through
/// other tasks. Both are the true answer, and the busy one is what keeps a
/// continuously loaded CPU under review.
fn survey(now_ns: u64) -> Survey {
    let mut found = Survey {
        peak_util: 0,
        any_active: false,
    };
    for state in cpu_state::states() {
        if state.cpu_active_since.load(Ordering::Relaxed) != 0 {
            found.any_active = true;
        }
        let util = util_at(state, now_ns);
        if util > found.peak_util {
            found.peak_util = util;
        }
    }
    found
}

/// When the answer could next change, and so when to look again.
///
/// Five cases, in order. A held ceiling comes first: while the minimum hold
/// stands nothing else can move the published rate, so its expiry is the only
/// event worth waking for. A live launch boost expires at a known instant, so
/// that is the next thing to reconsider. Work that is still running produces
/// no transition to observe, so a busy machine is revisited on the active
/// cadence — that is what turns a rising filter into a rising rate. An
/// `attention_until` still in the future must be honoured even when nothing
/// looks busy right now, because a wake inside that span is deliberately not
/// flagged: this deadline is what promises the look instead, and without it a
/// CPU that resumed just after a survey found the machine quiet could stay
/// busy indefinitely at the floor with no edge left to announce it. Otherwise
/// the rate can only fall as utilisation decays, which takes a window; and
/// once it has reached the floor with nothing running and nothing promised,
/// nothing can move it until real work arrives, so the machine takes no
/// wakeup at all.
fn next_review(
    limits: &CpuFreqLimits,
    target: u64,
    now_ns: u64,
    launch_until: u64,
    attention_until: u64,
    any_active: bool,
    held_until: Option<u64>,
) -> u64 {
    if let Some(expiry) = held_until {
        expiry
    } else if now_ns < launch_until {
        launch_until
    } else if any_active {
        now_ns.saturating_add(ACTIVE_REVIEW_NS)
    } else if now_ns < attention_until {
        attention_until
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
    let launch_until = LAUNCH_BOOST_UNTIL_NS.load(Ordering::Relaxed);
    let mut slot = BINDING.lock();
    let bound = match slot.as_mut() {
        Some(bound) if bound.process == process && bound.handle == handle => bound,
        _ => return Err(Errno::NotFound),
    };
    let found = survey(now_ns);
    let want = target_hz(&bound.limits, found.peak_util, now_ns < launch_until);
    let held_until = hold_expiry(
        &bound.limits,
        want,
        bound.target_hz,
        bound.at_max_since,
        now_ns,
    );
    let target = held_target_hz(
        &bound.limits,
        want,
        bound.target_hz,
        bound.at_max_since,
        now_ns,
    );
    if bound.target_hz != target {
        // The hold runs from the instant the ceiling was reached, so it is
        // stamped on the way in and cleared on the way out — never refreshed
        // by a machine that simply stays there.
        bound.at_max_since = if target == bound.limits.max_hz {
            now_ns
        } else {
            0
        };
        bound.target_hz = target;
        bound.seq += 1;
    }
    let review_at = next_review(
        &bound.limits,
        target,
        now_ns,
        launch_until,
        ATTENTION_UNTIL_NS.load(Ordering::Relaxed),
        found.any_active,
        held_until,
    );
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
