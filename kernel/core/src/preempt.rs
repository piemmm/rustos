//! Per-CPU pending-preemption latch: the bridge between a timer tick the
//! kernel could not act on immediately and the next point it safely can.
//!
//! The kernel is non-preemptible: a one-shot preemption tick taken while
//! the CPU is in kernel mode (mid-syscall, or in a kernel kthread) must
//! never context-switch away from a half-completed kernel critical
//! section, so the ports' IRQ paths preempt only a tick taken from user
//! mode. Before this latch existed, a quantum that expired *inside* a
//! syscall was silently lost — the fired one-shot was disarmed, nothing
//! recorded that the running task's turn was over, and the task returned
//! to user mode with no timer armed, free to run unpreempted until its
//! next voluntary yield. A task that regularly sat in syscalls (a shell
//! tool walking the filesystem, a process writing the console) could
//! therefore starve every competitor: cooperative scheduling in
//! preemptive clothing.
//!
//! The latch closes that hole with three sites, all on this CPU:
//!
//! * **Set** — the per-tick callback every port's timer IRQ path invokes
//!   on every fired one-shot calls [`note_preempt_tick`]. Latching is
//!   pure accounting (one atomic store), so it is safe on a tick taken
//!   in kernel mode.
//! * **Consume** — when a syscall completes, the production dispatch
//!   hook calls [`take_preempt_pending`]; a latched tick converts the
//!   syscall's ordinary return into the same suspend-back-to-scheduler
//!   path a `yield` syscall takes, so the expired turn is honoured at
//!   the first safe boundary. The running task can now overrun its
//!   quantum by at most the remainder of one bounded syscall, never
//!   indefinitely.
//! * **Clear** — the dispatcher calls `clear_preempt_pending`
//!   immediately before switching a task in: the scheduler has just made
//!   a fresh decision (and re-armed the one-shot for a contended CPU),
//!   which supersedes any tick latched before the switch. This is what
//!   keeps a user-mode tick — which already preempts immediately through
//!   the ports' user-mode preempt point — from also triggering a
//!   spurious yield on the resumed task's next syscall.
//!
//! A tick is latched whether the one-shot fired for a preemption quantum
//! or for a blocking-wait wakeup: both mean the scheduler may have newly
//! runnable work, and the user-mode path already yields on either (the
//! ports preempt on every user-taken timer tick). The next dispatch
//! re-evaluates and re-arms, so an unnecessary yield costs one bounded
//! scheduler round-trip and is never wrong; a *missed* expiry is the
//! starvation defect this module exists to prevent.
//!
//! # The forced-yield latch (a monopolising lone user task)
//!
//! The tick latch above is competitor-gated: a *lone* runnable user task
//! with no competitor and nothing pending is deliberately left running,
//! because rescheduling to the same sole task only churns the
//! address-space/TLB switch. That is correct for scheduling, but it means
//! a CPU-bound user task that never issues a syscall never returns to the
//! dispatch loop at all: its per-dispatch housekeeping (the deferred-wake
//! and console-transmit drains) and its progress/liveness heartbeats stop,
//! and the task withholds the CPU indefinitely — the monopolisation the
//! charter forbids.
//!
//! A second, *un-gated* latch closes that hole. The lockup watchdog's
//! cadence sample — already firing on every CPU for liveness detection —
//! calls [`request_forced_yield`] when it observes an `Active` CPU running
//! a user task whose scheduler progress has been stale past a monopoly
//! guard. [`preempt_current`] honours it at the same return-to-user point
//! by suspending the task back to the dispatcher unconditionally (no
//! competitor required), so the dispatch loop runs one iteration —
//! stamping progress, draining housekeeping — before re-dispatching the
//! task. It arms no new periodic timer: it rides the watchdog cadence that
//! is already running, so the tickless invariant is untouched.

use core::sync::atomic::Ordering;

use tairix_kernel_sched_api::CpuId;
use tairix_sync::once::OnceCell;

use crate::cpu_state;
use crate::dispatch_slot::RescheduleAction;
use crate::kthread::reschedule_current;

/// A scheduler-backed query the policy-neutral preempt path uses to decide
/// whether a fired tick actually owes a context switch: does `cpu` have a
/// runnable task *other* than the one currently running there — a
/// competitor the running task must be preempted for?
///
/// `kernel/core` is the one place that names the concrete scheduler policy
/// (the modularity contract), so it installs this once at boot over the live
/// scheduler and the preempt path asks through it without naming the policy.
pub trait PreemptCompetitor: Sync {
    /// `true` if `cpu` has at least one runnable task besides the one it is
    /// currently running (an out-of-range `cpu` reports `false`).
    fn has_runnable_competitor(&self, cpu: CpuId) -> bool;

    /// After a fired tick that does not lead to a dispatch, keep a
    /// *non-tickless* policy's fixed-frequency tick alive on `cpu` by
    /// re-arming its quantum. This covers both a lone runnable task with
    /// nothing pending and a failed immediate suspension: neither path
    /// reaches the dispatch that normally arms the next deadline. The CPU
    /// therefore keeps re-checking its run queue at the steady HZ cadence
    /// (the Linux scheduler-tick model) and promptly picks up work later
    /// enqueued here *without* an IPI — a task drained back from overflow,
    /// a rebalance. Re-arms only the timer; it never reschedules-to-self,
    /// so it incurs none of the address-space/TLB churn a gratuitous
    /// switch would. A *tickless* policy (EEVDF, MLFQ) implements this as
    /// a no-op, so a quiet core still takes no ticks.
    fn keep_periodic_tick(&self, cpu: CpuId);
}

/// The installed competitor gate, or `None` before the boot path wires it.
///
/// Set once per boot ([`install_competitor_gate`]). Before it is set (host
/// tests, early boot) the preempt path treats every tick as owing a
/// reschedule, preserving the pre-gate always-reschedule behaviour.
static COMPETITOR_GATE: OnceCell<&'static dyn PreemptCompetitor> = OnceCell::new();

/// Install the scheduler-backed [`PreemptCompetitor`] gate. Idempotent by
/// policy: the boot path installs exactly one; a later call is a benign
/// no-op.
pub fn install_competitor_gate(gate: &'static dyn PreemptCompetitor) {
    let _ = COMPETITOR_GATE.set(gate);
}

/// The competitor gate the preempt path currently consults.
///
/// In production this is the boot-installed [`COMPETITOR_GATE`]. Under
/// `cfg(test)` a per-thread override (installed by a test that must
/// observe gate routing) takes precedence, so the process-wide set-once
/// gate a *parallel* boot-phase test may have installed cannot mask the
/// gate the current test is exercising. The override is thread-local, so
/// it never perturbs a gate any other test thread is using.
fn active_gate() -> Option<&'static dyn PreemptCompetitor> {
    #[cfg(test)]
    if let Some(gate) = tests::test_gate() {
        return Some(gate);
    }
    COMPETITOR_GATE.get().ok().flatten().copied()
}

/// Whether `cpu` currently owes a reschedule when a tick fires: a runnable
/// competitor exists, or interrupt-context work (a device-IRQ deferred
/// wake, a queued foreground signal) is waiting for the dispatch loop to
/// drain it. Absent the gate, defaults to `true` (always reschedule).
fn reschedule_owed(cpu: CpuId) -> bool {
    let competitor = active_gate().is_none_or(|gate| gate.has_runnable_competitor(cpu));
    competitor
        || crate::waitq::has_pending_deferred_wake()
        || crate::waitq::timed_wake_due()
        || crate::procsignal::has_pending_foreground()
}

/// Keep a non-tickless policy's periodic tick alive on `cpu` after a
/// fired tick that did not owe a switch (see
/// [`PreemptCompetitor::keep_periodic_tick`]). Routed through the
/// installed gate so the preempt path never names the concrete policy; a
/// no-op before the gate is wired (early boot / host tests), where the
/// dispatch loop still re-arms on its next step.
fn keep_periodic_tick(cpu: CpuId) {
    if let Some(gate) = active_gate() {
        gate.keep_periodic_tick(cpu);
    }
}

/// Record that `cpu`'s one-shot timer fired, so the task running there
/// owes the scheduler a reschedule at its next preemption point.
///
/// Called from the ports' per-tick timer callbacks on **every** fired
/// one-shot, regardless of the interrupted privilege level. Pure
/// accounting — it never context-switches — so it is safe from interrupt
/// context with the kernel mid-operation. An out-of-range `cpu` is
/// dropped rather than indexing out of bounds (fail closed).
pub fn note_preempt_tick(cpu: CpuId) {
    if let Some(state) = cpu_state::get(cpu) {
        state.preempt_pending.store(true, Ordering::Release);
    }
    // Piggy-back the live-core-frequency estimator on this per-CPU periodic
    // point: it reads the calling CPU's core/reference counters and divides
    // the deltas since the previous tick (pure accounting, no blocking). Off
    // the context-switch hot path and taken only when the port drives a
    // core-clock source.
    crate::cpufreq::sample(cpu);
}

/// Consume `cpu`'s pending-preemption latch: `true` exactly once per
/// latched tick.
///
/// Called by the syscall dispatch hook after a syscall completes; a
/// `true` return obliges the caller to suspend the current task back to
/// the scheduler (the involuntary analogue of a `yield` syscall) before
/// returning to user mode. An out-of-range `cpu` reports `false`
/// (fail closed: no phantom reschedule for a CPU with no slot).
#[must_use]
pub fn take_preempt_pending(cpu: CpuId) -> bool {
    cpu_state::get(cpu).is_some_and(|state| state.preempt_pending.swap(false, Ordering::AcqRel))
}

/// Request a **forced** yield-to-scheduler for the user task currently
/// running on `cpu`, even when it is the only runnable task there.
///
/// The watchdog cadence calls this when it samples an `Active` CPU running
/// a user task that has withheld the CPU from the scheduler past the
/// monopoly-guard window. Unlike [`note_preempt_tick`], the yield this
/// requests is **not** gated on a runnable competitor: its purpose is
/// precisely to return a lone CPU-bound task to the dispatcher so the
/// dispatch loop's housekeeping (deferred-wake drain, console-transmit
/// drain) and its progress/liveness heartbeats run again — the charter's
/// guarantee that a runnable task can never monopolise a CPU by refusing
/// to yield. It arms no new timer; it rides the watchdog cadence already
/// firing on the CPU, so the tickless invariant is preserved. Pure
/// accounting (one atomic store); an out-of-range `cpu` is dropped (fail
/// closed).
pub fn request_forced_yield(cpu: CpuId) {
    if let Some(state) = cpu_state::get(cpu) {
        state.force_yield.store(true, Ordering::Release);
    }
}

/// Consume `cpu`'s forced-yield latch: `true` exactly once per request.
#[must_use]
fn take_forced_yield(cpu: CpuId) -> bool {
    cpu_state::get(cpu).is_some_and(|state| state.force_yield.swap(false, Ordering::AcqRel))
}

/// Discard any tick latched on `cpu` before the scheduler's current
/// dispatch decision.
///
/// Called by the dispatcher immediately before switching a task in: the
/// decision (and the freshly re-armed one-shot) supersedes a tick that
/// fired before the switch, so the incoming task starts its quantum
/// without inheriting a stale reschedule obligation. An out-of-range
/// `cpu` is a no-op.
pub(crate) fn clear_preempt_pending(cpu: CpuId) {
    if let Some(state) = cpu_state::get(cpu) {
        state.preempt_pending.store(false, Ordering::Release);
        // The dispatcher has just made a fresh decision, so a monopoly
        // yield requested against the task it is switching away from is
        // superseded too: the incoming task starts clean and earns its own
        // monopoly-guard window from this point.
        state.force_yield.store(false, Ordering::Release);
    }
}

/// Honour `cpu`'s pending-preemption latch by suspending the user task
/// currently running there back to the scheduler, counting the
/// preemption when one is performed.
///
/// This is the single definition of the involuntary-preemption action
/// every architecture's return-to-user preempt callback invokes, so the
/// "consult the latch, reschedule if owed, count it" logic lives in one
/// place rather than duplicated per port. It reschedules **only** when
/// this CPU owes one ([`take_preempt_pending`] is set): an interrupt that
/// woke nothing returns straight to user mode with no gratuitous context
/// switch. When a reschedule is owed it suspends the running task with
/// [`RescheduleAction::Yield`] — the involuntary analogue of a `yield`
/// syscall — so the scheduler picks the next runnable task (giving
/// EEVDF-ordered time-slicing and running the woken-work drain).
///
/// Returns `true` if a task was preempted (and the counter advanced),
/// `false` if nothing was owed or no resumable user task is published on
/// `cpu` (the fail-closed [`reschedule_current`] return, e.g. a stray
/// invocation with none switched in). The counter advances only on a real
/// suspension, never on a spurious call.
#[must_use]
pub fn preempt_current(cpu: CpuId) -> bool {
    // Consume both latches on every visit so neither lingers to fire a
    // spurious switch on a later, unrelated preempt point.
    let tick_pending = take_preempt_pending(cpu);
    let forced = take_forced_yield(cpu);
    if !tick_pending && !forced {
        return false;
    }
    // A forced yield is *not* gated on a competitor: it exists precisely to
    // return a lone CPU-bound user task to the dispatcher (so the dispatch
    // loop's housekeeping and its progress/liveness heartbeats run again),
    // which a competitor-gated tick would skip. It rides the watchdog
    // cadence already firing on this CPU, so it arms no new timer.
    if forced {
        if !reschedule_current(cpu, RescheduleAction::Yield) {
            keep_periodic_tick(cpu);
            return false;
        }
        if let Some(state) = cpu_state::get(cpu) {
            state.preemptions.fetch_add(1, Ordering::Relaxed);
        }
        return true;
    }
    // A fired tick owes a context switch only when it would change what
    // runs: a runnable competitor to switch to, or interrupt-context work
    // (a device-IRQ deferred wake, a queued foreground signal) waiting for
    // the dispatch loop to drain it. A lone runnable task with nothing
    // pending is left running — the periodic tick still fired (TAIRiX
    // stays non-tickless under the CFQ policy), but rescheduling to the
    // *same* sole task every quantum has no scheduling effect and only
    // churns the per-dispatch user-address-space switch (and, on an
    // emulated target, its full TLB flush), which starves the task's own
    // forward progress. This mirrors Linux's `check_preempt_tick`, which
    // preempts only when a competitor should run. The latch is consumed
    // either way — the tick was honoured.
    if !reschedule_owed(cpu) {
        // The tick fired but nothing owes a switch (a lone runnable task).
        // Keep a non-tickless policy's periodic tick alive so this CPU
        // re-checks its run queue at the next tick and promptly picks up
        // work later enqueued here without an IPI (a task drained back
        // from overflow, a rebalance); without this the lone task's tick
        // would fall silent and strand such work — the source of the
        // heavy-load stall. This re-arms only the timer, so it avoids the
        // reschedule-to-self address-space/TLB churn. A tickless policy
        // no-ops, so a quiet core keeps taking no ticks.
        keep_periodic_tick(cpu);
        return false;
    }
    if !reschedule_current(cpu, RescheduleAction::Yield) {
        // The fired one-shot was consumed, but no dispatcher ran to arm a
        // replacement quantum. Restore a non-tickless policy's periodic
        // deadline before returning; otherwise this CPU can resume a busy
        // user task with no future preemption interrupt.
        keep_periodic_tick(cpu);
        return false;
    }
    if let Some(state) = cpu_state::get(cpu) {
        state.preemptions.fetch_add(1, Ordering::Relaxed);
    }
    true
}

/// The number of involuntary preemptions performed on `cpu` since boot.
///
/// An out-of-range `cpu` reports `0` (fail closed: no phantom count for a
/// CPU with no slot).
#[must_use]
pub fn preemption_count(cpu: CpuId) -> u64 {
    cpu_state::get(cpu).map_or(0, |state| state.preemptions.load(Ordering::Relaxed))
}

/// The sum of [`preemption_count`] across every CPU slot.
#[must_use]
pub fn total_preemption_count() -> u64 {
    cpu_state::total_preemptions()
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::AtomicU64;

    struct TestCompetitorGate {
        periodic_rearms: [AtomicU64; crate::cpu_state::TEST_CPUS],
    }

    impl TestCompetitorGate {
        const fn new() -> Self {
            Self {
                periodic_rearms: [const { AtomicU64::new(0) }; crate::cpu_state::TEST_CPUS],
            }
        }

        fn periodic_rearms(&self, cpu: CpuId) -> u64 {
            self.periodic_rearms[cpu as usize].load(Ordering::Relaxed)
        }
    }

    impl PreemptCompetitor for TestCompetitorGate {
        fn has_runnable_competitor(&self, _cpu: CpuId) -> bool {
            true
        }

        fn keep_periodic_tick(&self, cpu: CpuId) {
            self.periodic_rearms[cpu as usize].fetch_add(1, Ordering::Relaxed);
        }
    }

    static TEST_COMPETITOR_GATE: TestCompetitorGate = TestCompetitorGate::new();

    // Per-thread competitor-gate override. The process-wide
    // `COMPETITOR_GATE` is set-once, so a parallel boot-phase test that
    // installs the real gate would otherwise make a later
    // `install_competitor_gate` here a no-op and leave this test observing
    // the wrong gate. A thread-local override, consulted first by
    // `active_gate`, isolates each test thread's gate from every other's.
    std::thread_local! {
        static TEST_GATE: core::cell::Cell<Option<&'static dyn PreemptCompetitor>> =
            const { core::cell::Cell::new(None) };
    }

    /// The current thread's competitor-gate override, if one was installed
    /// by [`set_test_gate`]. Read by [`super::active_gate`].
    pub(super) fn test_gate() -> Option<&'static dyn PreemptCompetitor> {
        TEST_GATE.with(core::cell::Cell::get)
    }

    /// Install a competitor gate for the current test thread only.
    fn set_test_gate(gate: &'static dyn PreemptCompetitor) {
        TEST_GATE.with(|g| g.set(Some(gate)));
    }

    /// Tests share the process-wide per-CPU slots; each test uses its own
    /// CPU index so parallel test threads never observe each other.
    #[test]
    fn a_latched_tick_is_consumed_exactly_once() {
        const CPU: CpuId = 40;
        assert!(!take_preempt_pending(CPU));
        note_preempt_tick(CPU);
        assert!(take_preempt_pending(CPU));
        // Consumed: a second take sees nothing.
        assert!(!take_preempt_pending(CPU));
    }

    #[test]
    fn repeated_ticks_before_the_preemption_point_coalesce() {
        const CPU: CpuId = 41;
        note_preempt_tick(CPU);
        note_preempt_tick(CPU);
        assert!(take_preempt_pending(CPU));
        assert!(!take_preempt_pending(CPU));
    }

    #[test]
    fn clearing_supersedes_a_latched_tick() {
        const CPU: CpuId = 42;
        note_preempt_tick(CPU);
        clear_preempt_pending(CPU);
        assert!(!take_preempt_pending(CPU));
    }

    #[test]
    fn each_cpu_has_its_own_latch() {
        const CPU_A: CpuId = 43;
        const CPU_B: CpuId = 44;
        note_preempt_tick(CPU_A);
        assert!(!take_preempt_pending(CPU_B));
        assert!(take_preempt_pending(CPU_A));
    }

    #[test]
    fn an_out_of_range_cpu_fails_closed() {
        let beyond = u32::try_from(crate::cpu_state::TEST_CPUS).expect("test CPU count fits u32");
        note_preempt_tick(beyond);
        assert!(!take_preempt_pending(beyond));
        clear_preempt_pending(beyond);
        note_preempt_tick(u32::MAX);
        assert!(!take_preempt_pending(u32::MAX));
    }

    /// With no latch set, [`preempt_current`] is a no-op: it neither
    /// reschedules nor advances the count. (In a host test no user
    /// resume handle is published, so even a latched call cannot switch
    /// — the counting-under-real-preemption path is exercised by the
    /// aarch64 QEMU preemption vertical.)
    #[test]
    fn preempt_current_without_a_latch_does_nothing() {
        const CPU: CpuId = 45;
        assert_eq!(preemption_count(CPU), 0);
        assert!(!preempt_current(CPU));
        assert_eq!(preemption_count(CPU), 0);
    }

    /// A latched tick with no published user task fails closed through
    /// [`reschedule_current`] and does **not** advance the count: the
    /// counter tracks real suspensions only, never a spurious call. CFQ's
    /// periodic tick is restored because no dispatch occurred to arm the
    /// next quantum; otherwise a busy user task could run indefinitely
    /// after this failed suspension.
    #[test]
    fn failed_suspension_keeps_the_periodic_tick_alive() {
        const CPU: CpuId = 46;
        set_test_gate(&TEST_COMPETITOR_GATE);
        let rearms_before = TEST_COMPETITOR_GATE.periodic_rearms(CPU);
        note_preempt_tick(CPU);
        assert!(!preempt_current(CPU));
        assert_eq!(preemption_count(CPU), 0);
        assert_eq!(TEST_COMPETITOR_GATE.periodic_rearms(CPU), rearms_before + 1);
        // The latch was taken even though no switch happened.
        assert!(!take_preempt_pending(CPU));
    }

    #[test]
    fn preemption_count_out_of_range_cpu_reports_zero() {
        assert_eq!(preemption_count(u32::MAX), 0);
    }

    /// A forced yield is consumed exactly once and needs no competitor
    /// gate: it exists precisely to preempt a *lone* task. In a host test
    /// no user resume handle is published, so the suspension fails closed
    /// (the counting-under-real-preemption path is the aarch64 QEMU
    /// vertical), but the latch is still consumed and no phantom count is
    /// recorded.
    #[test]
    fn a_forced_yield_is_consumed_and_needs_no_competitor() {
        const CPU: CpuId = 57;
        assert!(!take_forced_yield(CPU));
        request_forced_yield(CPU);
        assert!(!preempt_current(CPU));
        assert!(!take_forced_yield(CPU));
        assert_eq!(preemption_count(CPU), 0);
    }

    /// The dispatcher's `clear_preempt_pending` supersedes a forced yield
    /// requested against the outgoing task, so the incoming task starts its
    /// monopoly-guard window clean.
    #[test]
    fn clearing_supersedes_a_forced_yield() {
        const CPU: CpuId = 58;
        request_forced_yield(CPU);
        clear_preempt_pending(CPU);
        assert!(!take_forced_yield(CPU));
    }

    /// A forced yield fires even with no competitor and no latched tick —
    /// the whole point is to preempt a lone task the competitor gate would
    /// otherwise leave running.
    #[test]
    fn a_forced_yield_alone_reaches_the_reschedule_path() {
        const CPU: CpuId = 59;
        // No tick latched, no competitor: without the forced latch this
        // would short-circuit to `false` before any reschedule attempt.
        assert!(!take_preempt_pending(CPU));
        request_forced_yield(CPU);
        // Reaches the (host: fail-closed) reschedule path and consumes the
        // latch; returns false only because no user task is published here.
        assert!(!preempt_current(CPU));
        assert!(!take_forced_yield(CPU));
    }
}
