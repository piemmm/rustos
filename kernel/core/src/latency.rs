//! The task-latency watchdog: what spent an interactive surface's frame
//! budget, and the call stack that spent it (`plans/FIX-STALLTRACE.md`).
//!
//! A surface that declares a budget through
//! [`SyscallNumber::LATENCY_WATCH`](tairix_abi::SyscallNumber::LATENCY_WATCH)
//! is asking to be told when an iteration of its loop overruns. It cannot
//! answer that itself: once the loop notices the cost, the stack that caused
//! it has unwound, and any backtrace it takes names the detector.
//!
//! # Where the overrun is noticed
//!
//! At the two kernel boundaries the thread must cross for the stall to have
//! ended — syscall entry and syscall exit:
//!
//! * **On exit**, the span had not overrun at this syscall's entry and has
//!   now, so *this* syscall is what spent the budget. The frame taken at its
//!   entry names the blocking call site, and the thread's user stack has not
//!   moved since (it has been in the kernel throughout), so walking it now
//!   yields exactly the stall's stack.
//! * **On entry**, the span overran while the thread was running in user
//!   mode. The frame just taken names the code that was executing.
//!
//! Both are captured while the thread is *inside* the kernel, which is what
//! makes the walk sound: a thread executing user code has a stack moving
//! under the reader, and a chain read from one is fiction. That is also why
//! nothing here samples another thread — a stall is diagnosed by its own
//! victim, at a point where its stack is frozen.
//!
//! A thread that never returns from its syscall therefore reports nothing,
//! deliberately: that is a wedge rather than a pause, and the CPU-lockup
//! watchdog ([`crate::watchdog`]), the service-liveness watchdog, and the
//! desktop's own not-responding detector each already own a part of it. A
//! fourth overlapping detector would report the same event three ways.
//!
//! # Bookkeeping only
//!
//! This module holds no identity, reads no address space, and emits no
//! record. It answers "did this span overrun, and with what frame" and the
//! syscall dispatcher — which already holds the capability table and the
//! address-space registry — resolves the attested name, the load base, and
//! the user-stack walk from that. So the whole state machine is
//! host-testable without a kernel.
//!
//! # Debug images only
//!
//! Every item below is behind the `watchdog-diagnostics` feature, which
//! `tools/xtask` turns on for the non-shippable debug image alone. A
//! shippable image carries none of this state and answers every arming call
//! with zero.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;

use tairix_abi::latency::{clamp_budget_ns, StallSample, MIN_REPORT_INTERVAL_NS};
use tairix_arch_api::backtrace::FrameLayout;
use tairix_kernel_sched_api::TaskId;
use tairix_sync::once::OnceCell;
use tairix_sync::{RwLock, SpinLock};

/// One overrun, as the bookkeeping sees it.
///
/// Deliberately identity-free: the thread's attested name and its PIE load
/// base are the dispatcher's to resolve, so this stays pure data the state
/// machine can be tested against.
#[derive(Copy, Clone, Debug)]
pub struct Overrun {
    /// The budget the overrunning thread declared.
    pub budget_ns: u64,
    /// How long the span had been open when the overrun was noticed.
    pub elapsed_ns: u64,
    /// How much of `elapsed_ns` was spent inside syscalls. The remainder
    /// was spent running user code, so the two together say whether this
    /// was a blocking stall or an unbounded-work one.
    pub blocked_ns: u64,
    /// Syscalls completed during the span, so a span spent in many small
    /// round trips is distinguishable from one spent in a single long call.
    pub calls: u32,
    /// The syscall that carried the span past its budget, when one did.
    /// `None` when the budget went to user-mode work.
    pub blocked_in: Option<u64>,
    /// How long that syscall had been running when it crossed the budget.
    pub blocked_in_ns: u64,
    /// The captured user frame, absent on a port that publishes none.
    pub frame: Option<EntryFrame>,
    /// What `frame` names.
    pub sample: StallSample,
}

/// The root of a user call chain, as the port reported it at a kernel entry.
///
/// Only what a frame-pointer walk consumes: the chain's root, the pc that
/// names its top, the port's layout for following it, and the port's honest
/// verdict on whether that root is a frame pointer at all. The whole
/// register file is the fault path's business, not a latency report's.
#[derive(Copy, Clone, Debug)]
pub struct EntryFrame {
    /// The interrupted user program counter.
    pub pc: u64,
    /// The user frame-pointer register.
    pub fp: u64,
    /// Whether [`Self::fp`] is a frame pointer the port actually saved.
    pub fp_valid: bool,
    /// How one user frame is laid out on this port.
    pub layout: FrameLayout,
}

/// The syscall a watched thread is currently inside.
#[derive(Copy, Clone, Debug)]
struct InFlight {
    /// Its `abi-v1` number, so the report names the call.
    number: u64,
    /// When the thread entered it.
    at_ns: u64,
    /// The user frame the port published at that entry.
    frame: Option<EntryFrame>,
}

/// One watched thread's span accounting.
#[derive(Copy, Clone, Debug)]
struct Watch {
    /// The declared budget, always already clamped and non-zero (a disarm
    /// removes the watch rather than storing a zero).
    budget_ns: u64,
    /// When the open span began; `None` between an event wait and its
    /// return, when the surface owes nothing.
    span_start_ns: Option<u64>,
    /// Nanoseconds of the open span spent inside completed syscalls.
    blocked_ns: u64,
    /// Syscalls completed during the open span.
    calls: u32,
    /// Whether this span has already produced a report, so one pause is
    /// one record however many boundaries it crosses afterwards.
    reported: bool,
    /// When this thread last produced a report, for the rate floor.
    last_report_ns: Option<u64>,
    /// The syscall in flight, if any.
    in_flight: Option<InFlight>,
}

impl Watch {
    /// A freshly armed watch: no span open yet, so the budget takes effect
    /// from the first event this surface answers rather than from whatever
    /// bring-up work happens to follow the arming call.
    const fn new(budget_ns: u64) -> Self {
        Self {
            budget_ns,
            span_start_ns: None,
            blocked_ns: 0,
            calls: 0,
            reported: false,
            last_report_ns: None,
            in_flight: None,
        }
    }

    /// Whether an overrun should be reported at `now_ns`, honouring the
    /// per-span latch and the per-thread rate floor.
    fn owes_report(&self, now_ns: u64) -> Option<u64> {
        let elapsed = now_ns.saturating_sub(self.span_start_ns?);
        if self.reported || elapsed < self.budget_ns {
            return None;
        }
        if let Some(last) = self.last_report_ns {
            if now_ns.saturating_sub(last) < MIN_REPORT_INTERVAL_NS {
                return None;
            }
        }
        Some(elapsed)
    }
}

/// The watched threads, keyed by thread id — the authoritative store, read
/// only when a thread is armed, forgotten, or switched in.
///
/// It is deliberately **not** on the syscall path. Its `RwLock` acquires by
/// compare-exchange on one shared word, so consulting it per syscall would
/// put a contended write on one cache line in front of every syscall on
/// every CPU the moment any surface armed a budget — a machine-wide
/// serialisation point in the one image a developer profiles in, added by
/// the tool meant to measure responsiveness. A syscall reads the per-CPU
/// publication ([`Published`]) instead.
static WATCHES: RwLock<BTreeMap<TaskId, Arc<SpinLock<Watch>>>> = RwLock::new(BTreeMap::new());

/// The watch published for the thread currently running on a CPU.
///
/// Held per CPU (in [`CpuState`](crate::cpu_state::CpuState)) and replaced at
/// every user switch-in, so a syscall boundary reaches its own thread's watch
/// through lines only that CPU touches. The `task` is carried with it and
/// checked on every access: a slot naming a different thread is treated as no
/// watch at all, so a mis-sequenced publication can never attribute one
/// thread's span to another.
///
/// The [`Arc`] is what makes the publication safe against a concurrent
/// [`forget`] — a sibling termination may drop the map entry while this
/// thread still runs, and the published clone keeps the watch alive until the
/// slot is replaced.
pub(crate) struct Published {
    task: TaskId,
    watch: Arc<SpinLock<Watch>>,
}

/// The port's user frame-pointer layout, published once at boot.
///
/// It is a per-port constant, so carrying it through the per-entry observer
/// would be two words repeated on every kernel entry.
static USER_FRAME_LAYOUT: OnceCell<FrameLayout> = OnceCell::new();

/// Arm the facility on this port: publish its user frame layout and install
/// the Arch HAL user-entry observer.
///
/// Called once during boot from each port's wiring, beside its syscall
/// dispatch install, with the same `Backtracer::LAYOUT` the port's fault
/// path already threads through its register frame. Until it is called the
/// port reports nothing (its entry hook sees no observer) and no report
/// carries a frame, so a port that has not wired it degrades honestly
/// rather than walking a chain it cannot describe.
pub fn install(layout: FrameLayout) {
    let _ = USER_FRAME_LAYOUT.set(layout);
    tairix_arch_api::userentry::set_user_entry_observer(note_user_entry_frame);
}

/// Record the user frame `cpu` is entering the kernel with.
///
/// Installed as the Arch HAL's user-entry observer, so it is called by the
/// port's syscall entry — the only place the saved user frame is in hand —
/// with the values it already holds. `fp_valid` is the port's honest
/// statement of whether the frame-pointer register was saved at all.
pub extern "C" fn note_user_entry_frame(cpu: u32, pc: u64, fp: u64, fp_valid: bool) {
    use core::sync::atomic::Ordering;
    let Some(state) = crate::cpu_state::get(cpu) else {
        return;
    };
    state.ue_pc.store(pc, Ordering::Relaxed);
    state.ue_fp.store(fp, Ordering::Relaxed);
    state.ue_fp_valid.store(fp_valid, Ordering::Relaxed);
    state.ue_present.store(true, Ordering::Relaxed);
}

/// Read back the frame published for `cpu`, as a self-describing bundle.
///
/// `None` when the port published nothing or no layout was installed —
/// never a zeroed frame dressed up as an observation.
fn published_frame(cpu: u32) -> Option<EntryFrame> {
    use core::sync::atomic::Ordering;
    let state = crate::cpu_state::get(cpu)?;
    if !state.ue_present.load(Ordering::Relaxed) {
        return None;
    }
    Some(EntryFrame {
        pc: state.ue_pc.load(Ordering::Relaxed),
        fp: state.ue_fp.load(Ordering::Relaxed),
        fp_valid: state.ue_fp_valid.load(Ordering::Relaxed),
        layout: USER_FRAME_LAYOUT.get().ok().flatten().copied()?,
    })
}

/// The watch for `task`, or `None` when it holds none.
/// Run `f` against the watch published for `task` on `cpu`.
///
/// [`None`] when this CPU has no publication, or its publication names a
/// different thread — both meaning "this thread owes nothing", which is the
/// fail-closed answer. Touches only the per-CPU slot and the watch's own
/// allocation, so no syscall boundary contends with another CPU's.
fn with_watch<R>(cpu: u32, task: TaskId, f: impl FnOnce(&mut Watch) -> R) -> Option<R> {
    let state = crate::cpu_state::get(cpu)?;
    // The clone is one refcount step on the watch's own allocation — a line
    // only this thread's boundaries touch — and it releases the slot before
    // the body runs, so a publication replacing the slot never waits on it.
    let watch = {
        let slot = state.latency_watch.lock();
        let published = slot.as_ref()?;
        if published.task != task {
            return None;
        }
        Arc::clone(&published.watch)
    };
    let mut guard = watch.lock();
    Some(f(&mut guard))
}

/// Publish `task`'s watch on `cpu`, or clear the slot when it holds none.
///
/// Called at every user switch-in, which is the one place the running thread
/// changes — so this is the only map lookup the dispatch path performs, and
/// it happens per switch rather than per syscall.
pub fn publish(cpu: u32, task: TaskId) {
    let Some(state) = crate::cpu_state::get(cpu) else {
        return;
    };
    let published = WATCHES.read().get(&task).map(|watch| Published {
        task,
        watch: Arc::clone(watch),
    });
    *state.latency_watch.lock() = published;
}

/// Clear `cpu`'s publication: the thread that owned it is no longer running
/// here, so a later boundary must not reach its watch through this slot.
pub fn unpublish(cpu: u32) {
    if let Some(state) = crate::cpu_state::get(cpu) {
        *state.latency_watch.lock() = None;
    }
}

/// Declare `task`'s frame budget, returning the budget actually armed.
///
/// A budget of [`BUDGET_DISARM`](tairix_abi::latency::BUDGET_DISARM) removes
/// the watch and answers zero; anything else is clamped up to the armable
/// floor rather than refused. Re-arming an existing watch replaces its
/// budget and abandons the open span, so a surface that changes its mind
/// mid-loop is measured against the budget it now declares.
pub fn arm(cpu: u32, task: TaskId, budget_ns: u64) -> u64 {
    let Some(budget) = clamp_budget_ns(budget_ns) else {
        forget(cpu, task);
        return 0;
    };
    {
        let mut watches = WATCHES.write();
        if let Some(existing) = watches.get(&task) {
            *existing.lock() = Watch::new(budget);
        } else {
            watches.insert(task, Arc::new(SpinLock::new(Watch::new(budget))));
        }
    }
    // The arming thread is the one running on `cpu`, and its switch-in
    // published whatever it held before, so the slot is refreshed here
    // rather than at a switch that has already happened.
    publish(cpu, task);
    budget
}

/// Drop `task`'s watch.
///
/// Called from the thread and process teardown paths, so a dead thread
/// leaves no accounting behind and a reused thread id inherits none.
pub fn forget(cpu: u32, task: TaskId) {
    WATCHES.write().remove(&task);
    // A thread landing down may be running on `cpu` (its own teardown) or
    // elsewhere (a sibling termination). Clearing the slot here covers the
    // first; the second is covered by the published `Arc`, which keeps the
    // watch alive until that CPU's next switch replaces it.
    if with_watch(cpu, task, |_| ()).is_some() {
        unpublish(cpu);
    }
}

/// Close `task`'s span: it is entering an event wait, so it owes nothing
/// until that wait returns.
///
/// The syscall in flight is discarded with the span, because the wait
/// itself is what the surface is *allowed* to spend: charging it would make
/// every idle desktop look stalled.
pub fn close_span(cpu: u32, task: TaskId) {
    with_watch(cpu, task, |watch| {
        watch.span_start_ns = None;
        watch.in_flight = None;
    });
}

/// Open a fresh span for `task` at `now_ns`: an event wait has returned, so
/// the surface owes an answer from here.
pub fn open_span(cpu: u32, task: TaskId, now_ns: u64) {
    with_watch(cpu, task, |watch| {
        watch.span_start_ns = Some(now_ns);
        watch.blocked_ns = 0;
        watch.calls = 0;
        watch.reported = false;
        watch.in_flight = None;
    });
}

/// Note `task` entering syscall `number` on `cpu`.
///
/// `now` is read only for a watched thread, so an unwatched one costs one
/// per-CPU slot read and no clock read at all.
///
/// Returns an overrun when the span was already past its budget before this
/// entry — the budget went to user-mode work, and the frame just published
/// names the code that was running.
pub fn on_syscall_entry(
    cpu: u32,
    task: TaskId,
    number: u64,
    now: impl FnOnce() -> u64,
) -> Option<Overrun> {
    with_watch(cpu, task, |watch| {
        let now_ns = now();
        let frame = published_frame(cpu);
        watch.in_flight = Some(InFlight {
            number,
            at_ns: now_ns,
            frame,
        });
        let elapsed_ns = watch.owes_report(now_ns)?;
        watch.reported = true;
        watch.last_report_ns = Some(now_ns);
        Some(Overrun {
            budget_ns: watch.budget_ns,
            elapsed_ns,
            blocked_ns: watch.blocked_ns,
            calls: watch.calls,
            // Nothing blocked: the span crossed its budget before this call.
            blocked_in: None,
            blocked_in_ns: 0,
            frame,
            sample: frame.map_or(StallSample::None, |_| StallSample::Running),
        })
    })
    .flatten()
}

/// Note `task` leaving its in-flight syscall.
///
/// `now` is read only for a watched thread, exactly as on entry.
///
/// Returns an overrun when the span crosses its budget here, in which case
/// the syscall just completed is what spent it and the frame taken at that
/// syscall's entry names the blocking call site.
pub fn on_syscall_exit(cpu: u32, task: TaskId, now: impl FnOnce() -> u64) -> Option<Overrun> {
    with_watch(cpu, task, |watch| {
        let now_ns = now();
        let in_flight = watch.in_flight.take();
        if let Some(call) = in_flight {
            watch.blocked_ns = watch
                .blocked_ns
                .saturating_add(now_ns.saturating_sub(call.at_ns));
            watch.calls = watch.calls.saturating_add(1);
        }
        let elapsed_ns = watch.owes_report(now_ns)?;
        watch.reported = true;
        watch.last_report_ns = Some(now_ns);
        let frame = in_flight.and_then(|call| call.frame);
        Some(Overrun {
            budget_ns: watch.budget_ns,
            elapsed_ns,
            blocked_ns: watch.blocked_ns,
            calls: watch.calls,
            blocked_in: in_flight.map(|call| call.number),
            blocked_in_ns: in_flight.map_or(0, |call| now_ns.saturating_sub(call.at_ns)),
            frame,
            sample: frame.map_or(StallSample::None, |_| StallSample::Blocking),
        })
    })
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tairix_abi::latency::{DEFAULT_FRAME_BUDGET_NS, MIN_FRAME_BUDGET_NS};

    /// The registry and the per-CPU slots are process-global, so each case
    /// owns its own thread id *and* its own CPU and never observes another
    /// test thread's watch or publication.
    const fn task(n: u64) -> TaskId {
        0x1a7e_0000 + n
    }

    /// The CPU case `n` runs on, distinct per case for the same reason.
    const fn cpu(n: u32) -> u32 {
        n
    }

    /// A watch armed on `c` with the default budget, with its span already
    /// open at `t = 0` — the state every case below starts from. `arm`
    /// publishes to `c` itself, as it does for a thread arming while it
    /// runs.
    fn armed(c: u32, t: TaskId) -> u64 {
        let budget = arm(c, t, DEFAULT_FRAME_BUDGET_NS);
        open_span(c, t, 0);
        budget
    }

    #[test]
    fn arming_answers_the_clamped_budget_and_disarming_answers_zero() {
        let t = task(1);
        let c = cpu(1);
        assert_eq!(arm(c, t, DEFAULT_FRAME_BUDGET_NS), DEFAULT_FRAME_BUDGET_NS);
        assert_eq!(arm(c, t, 1), MIN_FRAME_BUDGET_NS);
        assert_eq!(arm(c, t, 0), 0);
        // Disarmed means gone, not zero-budgeted: a later boundary must not
        // find a watch at all.
        assert!(with_watch(c, t, |w| *w).is_none());
    }

    #[test]
    fn an_unwatched_thread_is_never_reported() {
        let t = task(2);
        let c = cpu(2);
        assert!(on_syscall_entry(c, t, 1, || 0).is_none());
        assert!(on_syscall_exit(c, t, || u64::MAX).is_none());
    }

    #[test]
    fn a_span_within_budget_reports_nothing() {
        let t = task(3);
        let c = cpu(3);
        let budget = armed(c, t);
        assert!(on_syscall_entry(c, t, 7, || 1_000).is_none());
        assert!(on_syscall_exit(c, t, || budget - 1).is_none());
    }

    #[test]
    fn the_syscall_that_crosses_the_budget_is_the_one_reported() {
        let t = task(4);
        let c = cpu(4);
        let budget = armed(c, t);
        // Two short calls, then one that carries the span over.
        assert!(on_syscall_entry(c, t, 11, || 0).is_none());
        assert!(on_syscall_exit(c, t, || 10).is_none());
        assert!(on_syscall_entry(c, t, 12, || 20).is_none());
        assert!(on_syscall_exit(c, t, || 30).is_none());
        assert!(on_syscall_entry(c, t, 13, || 40).is_none());
        let over = on_syscall_exit(c, t, || budget + 5).expect("the crossing call reports");
        assert_eq!(over.budget_ns, budget);
        assert_eq!(over.elapsed_ns, budget + 5);
        assert_eq!(over.blocked_in, Some(13), "the culprit call names itself");
        assert_eq!(over.blocked_in_ns, budget + 5 - 40);
        assert_eq!(over.calls, 3);
        assert_eq!(over.blocked_ns, 10 + 10 + (budget + 5 - 40));
    }

    #[test]
    fn one_span_produces_one_report_however_many_boundaries_follow() {
        let t = task(5);
        let c = cpu(5);
        let budget = armed(c, t);
        assert!(on_syscall_entry(c, t, 21, || 0).is_none());
        assert!(on_syscall_exit(c, t, || budget + 1).is_some());
        // Still over budget, and still the same pause.
        assert!(on_syscall_entry(c, t, 22, || budget + 2).is_none());
        assert!(on_syscall_exit(c, t, || budget + 3).is_none());
    }

    #[test]
    fn a_fresh_span_can_report_again_once_the_rate_floor_has_passed() {
        let t = task(6);
        let c = cpu(6);
        let budget = armed(c, t);
        assert!(on_syscall_entry(c, t, 31, || 0).is_none());
        assert!(on_syscall_exit(c, t, || budget).is_some());
        // A new span far enough past the first report reports again.
        let later = budget + MIN_REPORT_INTERVAL_NS;
        open_span(c, t, later);
        assert!(on_syscall_entry(c, t, 32, || later).is_none());
        assert!(on_syscall_exit(c, t, || later + budget).is_some());
    }

    #[test]
    fn the_rate_floor_suppresses_a_thread_cycling_through_spans() {
        let t = task(7);
        let c = cpu(7);
        let budget = armed(c, t);
        assert!(on_syscall_entry(c, t, 41, || 0).is_none());
        assert!(on_syscall_exit(c, t, || budget).is_some());
        // A second overrunning span inside the floor is real but silent, so
        // a park/wake cycle cannot turn every frame into a log record.
        open_span(c, t, budget);
        assert!(on_syscall_entry(c, t, 42, || budget).is_none());
        assert!(on_syscall_exit(c, t, || budget * 2).is_none());
    }

    #[test]
    fn an_overrun_spent_in_user_mode_reports_at_the_next_entry_and_blames_no_call() {
        let t = task(8);
        let c = cpu(8);
        let budget = armed(c, t);
        // No syscall at all until well past the budget: the span was spent
        // running user code.
        let over = on_syscall_entry(c, t, 51, || budget + 7).expect("the next entry reports");
        assert_eq!(over.elapsed_ns, budget + 7);
        assert_eq!(over.blocked_in, None, "nothing blocked");
        assert_eq!(over.blocked_in_ns, 0);
        assert_eq!(over.blocked_ns, 0);
        assert_eq!(over.calls, 0);
    }

    #[test]
    fn a_closed_span_owes_nothing_however_long_the_wait() {
        let t = task(9);
        let c = cpu(9);
        armed(c, t);
        close_span(c, t);
        // The wait itself is what the surface is allowed to spend.
        assert!(on_syscall_exit(c, t, || u64::MAX / 2).is_none());
        assert!(on_syscall_entry(c, t, 61, || u64::MAX / 2).is_none());
    }

    #[test]
    fn closing_a_span_discards_the_wait_it_is_entering() {
        let t = task(10);
        let c = cpu(10);
        let budget = armed(c, t);
        // The park's own entry is recorded, then dropped by the close, so
        // the fresh span is not charged for the time spent parked.
        assert!(on_syscall_entry(c, t, 45, || 0).is_none());
        close_span(c, t);
        open_span(c, t, 10_000_000_000);
        assert!(on_syscall_exit(c, t, || 10_000_000_000).is_none());
        let watch = with_watch(c, t, |w| *w).expect("still armed");
        assert_eq!(watch.blocked_ns, 0);
        assert_eq!(watch.calls, 0);
        assert_eq!(watch.budget_ns, budget);
    }

    #[test]
    fn a_watch_armed_but_never_opened_reports_nothing() {
        let t = task(11);
        let c = cpu(11);
        arm(c, t, DEFAULT_FRAME_BUDGET_NS);
        // The budget takes effect from the first event the surface answers,
        // so bring-up work between arming and the first wait is not a
        // missed frame.
        assert!(on_syscall_entry(c, t, 71, || u64::MAX / 2).is_none());
        assert!(on_syscall_exit(c, t, || u64::MAX / 2).is_none());
    }

    #[test]
    fn re_arming_replaces_the_budget_and_abandons_the_open_span() {
        let t = task(12);
        let c = cpu(12);
        armed(c, t);
        assert!(on_syscall_entry(c, t, 81, || 0).is_none());
        let tighter = arm(c, t, MIN_FRAME_BUDGET_NS);
        assert_eq!(tighter, MIN_FRAME_BUDGET_NS);
        // No span is open against the new budget yet, so nothing is owed.
        assert!(on_syscall_exit(c, t, || u64::MAX / 2).is_none());
        open_span(c, t, 0);
        assert!(on_syscall_entry(c, t, 82, || 0).is_none());
        let over = on_syscall_exit(c, t, || tighter).expect("measured against the new budget");
        assert_eq!(over.budget_ns, tighter);
    }

    #[test]
    fn forget_leaves_no_accounting_behind() {
        let t = task(13);
        let c = cpu(13);
        armed(c, t);
        forget(c, t);
        assert!(with_watch(c, t, |w| *w).is_none());
        // A thread id drawn again inherits nothing.
        assert!(on_syscall_entry(c, t, 91, || u64::MAX / 2).is_none());
    }

    #[test]
    fn a_report_with_no_published_frame_says_so_rather_than_implying_one() {
        let t = task(14);
        let c = cpu(14);
        let budget = armed(c, t);
        // No port has published a frame for this CPU in a host test.
        assert!(on_syscall_entry(c, t, 101, || 0).is_none());
        let over = on_syscall_exit(c, t, || budget).expect("reports");
        assert!(over.frame.is_none());
        assert_eq!(over.sample, StallSample::None);
    }

    /// A boundary on a CPU whose slot names a *different* thread reports
    /// nothing, rather than charging this thread's span to that watch.
    #[test]
    fn a_slot_naming_another_thread_is_no_watch_at_all() {
        let t = task(17);
        let c = cpu(17);
        let budget = armed(c, t);
        // A second thread is switched in on the same CPU without a watch of
        // its own: the publication is replaced, so the first thread's watch
        // is unreachable here.
        let other = task(18);
        publish(c, other);
        assert!(on_syscall_entry(c, other, 131, || budget * 4).is_none());
        assert!(on_syscall_exit(c, other, || budget * 4).is_none());
        // And the original thread's own accounting is intact once it is
        // published again, which is what a migration back looks like.
        publish(c, t);
        assert!(on_syscall_entry(c, t, 132, || 0).is_none());
        assert!(on_syscall_exit(c, t, || budget).is_some());
    }

    /// A watch follows its thread across a migration: the destination CPU's
    /// switch-in publishes it, and the span it opened elsewhere continues.
    #[test]
    fn a_watch_follows_its_thread_to_another_cpu() {
        let t = task(19);
        let from = cpu(19);
        let to = cpu(20);
        let budget = armed(from, t);
        assert!(on_syscall_entry(from, t, 141, || 0).is_none());
        // The scheduler moves it: the source slot is cleared and the
        // destination publishes the same watch.
        unpublish(from);
        publish(to, t);
        // The span opened on `from` is still open, so the call that crosses
        // the budget on `to` is reported there.
        let over = on_syscall_exit(to, t, || budget).expect("the span survived the move");
        assert_eq!(over.budget_ns, budget);
        assert_eq!(over.blocked_in, Some(141));
        // The vacated CPU reaches nothing.
        assert!(on_syscall_entry(from, t, 142, || budget * 4).is_none());
    }

    /// An unpublished CPU reaches no watch, so a boundary taken between a
    /// switch-out and the next switch-in is inert rather than misattributed.
    #[test]
    fn an_unpublished_cpu_reaches_no_watch() {
        let t = task(21);
        let c = cpu(21);
        let budget = armed(c, t);
        unpublish(c);
        assert!(on_syscall_entry(c, t, 151, || budget * 4).is_none());
        assert!(on_syscall_exit(c, t, || budget * 4).is_none());
    }

    /// Forgetting a thread that is *not* the one published here leaves this
    /// CPU's publication alone: a sibling termination must not silently
    /// disarm the thread still running.
    #[test]
    fn forgetting_a_sibling_leaves_the_running_thread_watched() {
        let t = task(22);
        let c = cpu(22);
        let budget = armed(c, t);
        let sibling = task(23);
        arm(cpu(23), sibling, DEFAULT_FRAME_BUDGET_NS);
        forget(c, sibling);
        assert!(on_syscall_entry(c, t, 161, || 0).is_none());
        assert!(
            on_syscall_exit(c, t, || budget).is_some(),
            "the running thread's watch survived a sibling's teardown"
        );
    }

    #[test]
    fn a_published_frame_is_attributed_to_the_blocking_call() {
        let t = task(15);
        let c = cpu(15);
        let budget = armed(c, t);
        install(FrameLayout {
            saved_fp_offset: 0,
            return_addr_offset: 8,
        });
        note_user_entry_frame(c, 0xdead_0000, 0x7fff_0100, true);
        assert!(on_syscall_entry(c, t, 111, || 0).is_none());
        let over = on_syscall_exit(c, t, || budget).expect("reports");
        let frame = over.frame.expect("the published frame reaches the report");
        assert_eq!(frame.pc, 0xdead_0000);
        assert_eq!(frame.fp, 0x7fff_0100);
        assert!(frame.fp_valid);
        assert_eq!(over.sample, StallSample::Blocking);
    }

    #[test]
    fn a_frame_published_after_the_overrun_names_the_running_code() {
        let t = task(16);
        let c = cpu(16);
        let budget = armed(c, t);
        install(FrameLayout {
            saved_fp_offset: 0,
            return_addr_offset: 8,
        });
        note_user_entry_frame(c, 0xbeef_0000, 0x7ffe_0100, true);
        let over = on_syscall_entry(c, t, 121, || budget + 1).expect("reports");
        assert_eq!(over.sample, StallSample::Running);
        assert_eq!(
            over.frame.expect("frame present").pc,
            0xbeef_0000,
            "the entry that follows the overrun is the live sample"
        );
    }
}
