//! First-class CPU-lockup watchdog: detect, diagnose, and try to recover
//! from both *soft* and *hard* CPU lockups, and make each one loud on the
//! debug console with enough context to explain the *why*.
//!
//! # What it detects
//!
//! * **Soft lockup** — a CPU that keeps taking interrupts but stops
//!   returning to the scheduler for far longer than any bounded operation
//!   should take: a runaway in-kernel loop, a task that never yields, a
//!   lock held across a wedged access. The CPU is alive at the trap level,
//!   so its own timer path can observe it.
//! * **Hard lockup** — a CPU that has stopped taking even the
//!   non-maskable watchdog sample while it is running work: wedged with
//!   maskable interrupts masked, an interrupt storm, or a dead core.
//!   The victim never runs its own tick, so **another** CPU must observe
//!   it over a channel the victim cannot mask (the port's pseudo-NMI: the
//!   aarch64 FIQ). This is the classic hard lockup a soft detector is
//!   structurally blind to.
//!
//! # The two heartbeats and the activity state
//!
//! Every CPU keeps, in its per-CPU `CpuState`:
//!
//! * a **progress** heartbeat ([`note_progress`]), stamped once per
//!   dispatch-loop iteration — "the scheduler ran here"; the soft-lockup
//!   basis;
//! * a **liveness** heartbeat ([`on_watchdog_tick`]), stamped by the
//!   port's non-maskable cadence sample (~[`tairix_arch_api::WATCHDOG_CADENCE_NS`])
//!   — "this CPU is still taking the pseudo-NMI"; the hard-lockup basis;
//! * an **activity** class ([`set_activity`]) — `Active` while it runs the
//!   dispatch loop or a task, `Idle` while parked in `wfi`, `Offline`
//!   before it comes online or after it leaves. Only an `Active` CPU owes
//!   progress, so a legitimately parked or not-yet-online CPU is never
//!   judged (fail closed).
//!
//! # How a lockup is judged
//!
//! * The port's cadence sample calls [`on_watchdog_tick`] on its own CPU:
//!   it stamps liveness, records what it interrupted (PC, task, processor
//!   state) as that CPU's last-known context — the raw "why" — then runs a
//!   **cross-CPU scan** of every *other* CPU:
//!   - liveness frozen past the hard threshold while `Active` → **hard
//!     lockup** (only this path can see it);
//!   - otherwise, progress frozen past the soft threshold while `Active`
//!     **and last seen in the kernel** → **soft lockup** (a CPU wedged in
//!     the kernel even when it is the only runnable task; a lone,
//!     preemptible *user* task owes no progress and is never flagged).
//! * The CPU's own armed timer tick also calls [`check_stall`], the
//!   same-CPU soft check that catches a *contended* CPU whose preemption
//!   stopped making progress. Both share the per-episode latch, so a
//!   lockup is reported exactly once whichever path sees it first.
//!
//! # Diagnosis and recovery
//!
//! A detection renders a rich, allocation-free record — the locked CPU,
//! the observer, how long it has been silent, the last-known interrupted
//! PC and processor state, and the running task — then asks the port to
//! break it out best-effort ([`tairix_arch_api::WatchdogArch`]): a
//! reschedule for a soft lockup, a directed non-maskable attention
//! interrupt for a hard one. The recovery attempt and its honest outcome
//! are themselves on the audit trail; a genuinely wedged core is reported
//! `Unrecoverable`, never silently.
//!
//! # Cost and safety
//!
//! The hot-path additions are single relaxed atomic stores (one per
//! dispatch, one per cadence sample), so normal execution is unperturbed.
//! Detection and reporting are lock-free and allocation-free, safe to run
//! from the non-maskable sample path even while a target CPU holds
//! arbitrary locks. Before the report sink / clock / recovery handles are
//! installed, or on a never-armed CPU, the watchdog emits nothing and
//! never panics (fail closed).

use core::sync::atomic::{AtomicBool, Ordering};

use tairix_arch_api::{
    CpuId, RecoveryOutcome, StuckInterrupt, WatchdogArch, WatchdogKind, WatchdogSample,
};
use tairix_log::{Level, Sink};
use tairix_sync::once::OnceCell;
use tairix_util::fmt::format_hex_u64;

use crate::audit::{emit, AuditEvent};
use crate::cpu_state::{self, CpuState};

/// How long an `Active` CPU may run without any scheduler progress before
/// it is reported **soft**-locked, in nanoseconds (10 seconds).
///
/// A diagnostic policy value, not a resource capacity: no correct bounded
/// kernel operation withholds the CPU from the scheduler for ten seconds,
/// so a gap this large is a genuine soft lockup rather than a
/// long-but-legitimate wait. Deliberately generous so a heavily loaded but
/// healthy machine is never reported.
pub const DEFAULT_SOFT_LOCKUP_THRESHOLD_NS: u64 = 10_000_000_000;

/// How long an `Active` CPU may go without taking its non-maskable
/// watchdog sample before it is reported **hard**-locked, in nanoseconds
/// (10 seconds).
///
/// The cadence sample fires ~once per second, so ten seconds is ~ten
/// missed samples — well beyond any jitter, so a crossing means the CPU
/// genuinely stopped taking the pseudo-NMI rather than merely running
/// late. Diagnostic policy, not a capacity.
pub const DEFAULT_HARD_LOCKUP_THRESHOLD_NS: u64 = 10_000_000_000;

/// How long an `Active` CPU may run a **user** task without returning to
/// the scheduler before the watchdog cadence forces that task to yield, in
/// nanoseconds (1 second).
///
/// A lone CPU-bound user task has no competitor, so the ordinary
/// competitor-gated preemption tick deliberately leaves it running; without
/// this guard it would withhold the CPU from the dispatch loop
/// indefinitely, stalling per-dispatch housekeeping and the progress
/// heartbeat (a runnable task monopolising a CPU by refusing to yield). A
/// task that returns to the scheduler normally re-stamps progress long
/// before this window elapses, so a healthy task never triggers it; only a
/// genuine monopoliser does. Well below the 10-second soft/hard thresholds,
/// so the guard forces a housekeeping yield many times over before a stall
/// could ever be misjudged. A diagnostic/policy value, not a resource
/// capacity, and not a scheduler quantum: it rides the ~1 Hz watchdog
/// cadence already firing on the CPU and arms no new timer, so the tickless
/// invariant is preserved.
pub const DEFAULT_MONOPOLY_YIELD_THRESHOLD_NS: u64 = 1_000_000_000;

/// Nanoseconds per millisecond, for rendering the human-facing duration.
const NS_PER_MS: u64 = 1_000_000;

/// The watchdog activity class a CPU publishes so a cross-CPU check can
/// tell a CPU that *owes* progress apart from one that legitimately does
/// not. Encoded as the `u8` in `CpuState::wd_activity`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchdogActivity {
    /// Not yet online, or has left the dispatch loop for good. Never
    /// judged.
    Offline = 0,
    /// Parked in `wfi` with nothing to run. Legitimately makes no
    /// progress and takes no ticks; never judged.
    Idle = 1,
    /// Running the dispatch loop or a task; owes forward progress.
    Active = 2,
}

impl WatchdogActivity {
    /// Recover the activity class from its stored `u8`, defaulting to
    /// [`Self::Offline`] for any unrecognised value (fail closed — an
    /// unknown state is never judged).
    #[must_use]
    const fn from_u8(raw: u8) -> Self {
        match raw {
            1 => Self::Idle,
            2 => Self::Active,
            _ => Self::Offline,
        }
    }
}

/// The installed report sink, or `None` before the boot path wires it.
static REPORT_SINK: OnceCell<&'static (dyn Sink + Sync)> = OnceCell::new();

/// The installed non-maskable recovery handle, or `None` before boot wires
/// it (a build with no pseudo-NMI channel never installs one).
static RECOVERY: OnceCell<&'static (dyn WatchdogArch + Sync)> = OnceCell::new();

/// Resolves a stuck controller line to the task that owns its IRQ binding.
///
/// The watchdog reads a raw interrupt id from the shared controller when it
/// catches a hard lockup ([`WatchdogArch::stuck_interrupt`]); that id alone
/// does not say *whose* device it is. This seam lets the arch-neutral
/// watchdog attribute the line to the driver that bound it — turning
/// `stuck_irq=<id>` into `stuck_irq=<id> stuck_owner=<task>` for a bound
/// line, or `unbound` for a spurious/contained line no driver owns — without
/// the watchdog naming the kernel IRQ table type. The kernel binary installs
/// one backed by the live `IrqTable` ([`install_irq_owner`]).
pub trait StuckOwnerResolver: Sync {
    /// The task id that owns the binding for `line`, or `None` if `line` is
    /// bound to no task. A read-only lookup: it grants no authority and
    /// mutates nothing.
    fn owner_of_line(&self, line: u32) -> Option<u64>;
}

/// The installed stuck-line owner resolver, or `None` before boot wires it.
static IRQ_OWNER: OnceCell<&'static (dyn StuckOwnerResolver + Sync)> = OnceCell::new();

/// Install the sink the watchdog reports lockups through. Idempotent by
/// policy: the boot path installs exactly one; a later call is a benign
/// no-op.
pub fn install_report_sink(sink: &'static (dyn Sink + Sync)) {
    let _ = REPORT_SINK.set(sink);
}

/// Install the architecture recovery handle the watchdog drives to break a
/// locked-up CPU out of its lockup. Idempotent; a port with no pseudo-NMI
/// channel simply never calls this and hard-lockup recovery stays inert.
pub fn install_recovery(arch: &'static (dyn WatchdogArch + Sync)) {
    let _ = RECOVERY.set(arch);
}

/// The report sink the watchdog currently emits through, if installed.
fn report_sink() -> Option<&'static (dyn Sink + Sync)> {
    REPORT_SINK.get().ok().flatten().copied()
}

/// The installed recovery handle, if any.
fn recovery() -> Option<&'static (dyn WatchdogArch + Sync)> {
    RECOVERY.get().ok().flatten().copied()
}

/// Install the resolver that attributes a stuck controller line to the task
/// that owns its IRQ binding (see [`StuckOwnerResolver`]). Idempotent; a
/// build that never installs one simply omits the `stuck_owner` field.
pub fn install_irq_owner(resolver: &'static (dyn StuckOwnerResolver + Sync)) {
    let _ = IRQ_OWNER.set(resolver);
}

/// The installed stuck-line owner resolver, if any.
fn irq_owner() -> Option<&'static (dyn StuckOwnerResolver + Sync)> {
    IRQ_OWNER.get().ok().flatten().copied()
}

/// The outcome of one heartbeat evaluation on a CPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sample {
    /// The heartbeat has never been stamped — fail closed, no judgement.
    Unarmed,
    /// The heartbeat is recent (within the threshold): healthy.
    Healthy,
    /// The threshold was crossed and this sample first observed the
    /// episode: report it. Carries the elapsed gap in nanoseconds.
    Onset(u64),
    /// The threshold is crossed but the episode was already reported.
    Still,
}

/// Evaluate one heartbeat `last_ts` against `now`/`threshold`, latching
/// `reported` so a crossed threshold reports exactly once per episode.
/// Pure but for the latch swap; the caller does the I/O.
fn evaluate(last_ts: u64, reported: &AtomicBool, now: u64, threshold: u64) -> Sample {
    if last_ts == 0 {
        return Sample::Unarmed;
    }
    let elapsed = now.saturating_sub(last_ts);
    if elapsed < threshold {
        return Sample::Healthy;
    }
    if reported.swap(true, Ordering::AcqRel) {
        Sample::Still
    } else {
        Sample::Onset(elapsed)
    }
}

/// Publish `cpu`'s watchdog activity class so the cross-CPU scan can tell
/// a CPU that owes progress apart from one that legitimately does not.
/// An out-of-range `cpu` is a no-op (fail closed).
pub fn set_activity(cpu: CpuId, activity: WatchdogActivity) {
    if let Some(state) = cpu_state::get(cpu) {
        state.wd_activity.store(activity as u8, Ordering::Release);
    }
}

// --- Heartbeats -----------------------------------------------------

/// Stamp `state`'s progress heartbeat at `now_ns`, clearing the soft
/// episode latch. Returns `Some(stalled_ns)` when the CPU was in a
/// *reported* soft lockup and has now recovered (the caller emits a
/// recovery record); `None` on the ordinary healthy path. A `0` reading is
/// stamped as `1` so a stamped heartbeat is never mistaken for the
/// "unarmed" sentinel.
fn record_progress(state: &CpuState, now_ns: u64) -> Option<u64> {
    let prev = state
        .last_progress_ns
        .swap(now_ns.max(1), Ordering::Release);
    if state.stall_reported.swap(false, Ordering::AcqRel) {
        Some(now_ns.saturating_sub(prev))
    } else {
        None
    }
}

/// Record that the scheduler made progress on `cpu` at monotonic time
/// `now_ns` (the dispatch loop calls this once per iteration).
///
/// Pure accounting on the common path (one atomic store); it emits only on
/// the rare edge where a previously-reported soft lockup clears, so it is
/// cheap enough for the hot dispatch path. An out-of-range `cpu` is a
/// no-op (fail closed).
pub fn note_progress(cpu: CpuId, now_ns: u64) {
    let Some(state) = cpu_state::get(cpu) else {
        return;
    };
    if let Some(stalled_ns) = record_progress(state, now_ns) {
        report_lockup(
            AuditEvent::CpuStallCleared,
            Level::Warn,
            cpu,
            None,
            stalled_ns,
            &Diag::EMPTY,
        );
    }
}

/// Stamp `state`'s liveness heartbeat at `now_ns` *without* a sampled
/// context, returning `Some(stalled_ns)` when the CPU was in a *reported*
/// hard lockup and has now recovered.
///
/// This is the liveness analogue of [`record_progress`]: reaching the
/// dispatch loop is itself proof the CPU is alive and takes interrupts (it
/// either just woke from `wfi` by taking one, or is running continuously),
/// so the dispatcher stamps liveness here for the same reason it stamps
/// progress. Without it a CPU returning to `Active` after a long idle park
/// would carry the stale liveness heartbeat from *before* the park — its
/// non-maskable sample is only taken while running, not while parked — and
/// a cross-CPU scan would falsely report it hard-locked the instant it
/// republishes `Active`. Unlike [`record_liveness`] it leaves the recorded
/// sample context untouched: the dispatcher has no interrupted PC/PSTATE to
/// record, and the context only feeds a report's "why".
fn record_alive(state: &CpuState, now_ns: u64) -> Option<u64> {
    let prev = state.last_seen_ns.swap(now_ns.max(1), Ordering::Release);
    if state.hard_reported.swap(false, Ordering::AcqRel) {
        Some(now_ns.saturating_sub(prev))
    } else {
        None
    }
}

/// Record that `cpu` is alive at monotonic time `now_ns` from a context
/// that proves liveness without a sampled interrupt — the dispatch loop
/// calls this once per iteration, alongside [`note_progress`], before it
/// republishes the CPU as `Active`.
///
/// Pure accounting on the common path (one atomic store); it emits only on
/// the rare edge where a previously-reported hard lockup clears, so it is
/// cheap enough for the hot dispatch path. An out-of-range `cpu` is a
/// no-op (fail closed).
pub fn note_alive(cpu: CpuId, now_ns: u64) {
    let Some(state) = cpu_state::get(cpu) else {
        return;
    };
    if let Some(stalled_ns) = record_alive(state, now_ns) {
        report_lockup(
            AuditEvent::CpuHardLockupCleared,
            Level::Warn,
            cpu,
            None,
            stalled_ns,
            &Diag::EMPTY,
        );
    }
}

/// Stamp `state`'s liveness heartbeat at `now_ns` and record `sample` as
/// its last-known context. Returns `Some(stalled_ns)` when the CPU was in
/// a *reported* hard lockup and has now recovered.
fn record_liveness(state: &CpuState, now_ns: u64, sample: &WatchdogSample) -> Option<u64> {
    // Record the context first so a buddy that observes the fresh liveness
    // heartbeat also sees a matching (or newer) context.
    state.wd_ctx_pc.store(sample.pc, Ordering::Relaxed);
    state.wd_ctx_task.store(sample.task, Ordering::Relaxed);
    state.wd_ctx_aux.store(sample.aux, Ordering::Relaxed);
    state
        .wd_ctx_in_kernel
        .store(sample.in_kernel, Ordering::Relaxed);
    let prev = state.last_seen_ns.swap(now_ns.max(1), Ordering::Release);
    if state.hard_reported.swap(false, Ordering::AcqRel) {
        Some(now_ns.saturating_sub(prev))
    } else {
        None
    }
}

// --- The non-maskable cadence sample --------------------------------

/// Handle one non-maskable watchdog cadence sample taken *on* `cpu` at
/// monotonic time `now_ns`, with `sample` describing what it interrupted.
///
/// The port's pseudo-NMI cadence path (the aarch64 FIQ watchdog) calls
/// this on its own CPU. It stamps this CPU's liveness heartbeat, records
/// the sample as its last-known context (the raw "why"), reports a
/// recovery if this CPU was previously hard-locked, and then runs the
/// cross-CPU scan — the only place a *hard* lockup on another CPU becomes
/// observable. Lock-free and allocation-free, safe to call from the
/// non-maskable path. An out-of-range `cpu` still runs the scan (the CPU
/// is simply not itself sampled), so a stray id never blinds the detector.
pub fn on_watchdog_tick(cpu: CpuId, now_ns: u64, sample: &WatchdogSample) {
    if let Some(state) = cpu_state::get(cpu) {
        if let Some(stalled_ns) = record_liveness(state, now_ns, sample) {
            report_lockup(
                AuditEvent::CpuHardLockupCleared,
                Level::Warn,
                cpu,
                None,
                stalled_ns,
                &Diag::EMPTY,
            );
        }
        // A lone CPU-bound *user* task that never returns to the scheduler
        // keeps taking this very cadence sample (so it is not wedged), but
        // it withholds the CPU from the dispatch loop, stalling housekeeping
        // and the progress heartbeat. Force it back to the dispatcher once:
        // the return-to-user preempt point this same interrupt runs honours
        // the request, so the task cannot monopolise the CPU by refusing to
        // yield. Nothing is armed here beyond the cadence already firing.
        if monopolises_cpu(state, now_ns, sample.in_kernel) {
            crate::preempt::request_forced_yield(cpu);
        }
    }
    scan(cpu, now_ns);
}

/// Whether `state`'s CPU is an `Active` core running a **user** task that
/// has withheld the CPU from the scheduler past
/// [`DEFAULT_MONOPOLY_YIELD_THRESHOLD_NS`].
///
/// Pure predicate (no I/O, no latch), so the monopoly policy is unit-tested
/// directly. Only a CPU that is `Active`, was sampled in *user* mode
/// (kernel code is never preempted — the kernel is non-preemptible), and
/// has an *armed* progress heartbeat older than the guard qualifies; an
/// unarmed heartbeat (`0`) or a clock that went backwards never does (fail
/// closed — no phantom yield).
fn monopolises_cpu(state: &CpuState, now_ns: u64, in_kernel: bool) -> bool {
    if in_kernel {
        return false;
    }
    if WatchdogActivity::from_u8(state.wd_activity.load(Ordering::Acquire))
        != WatchdogActivity::Active
    {
        return false;
    }
    let last_progress = state.last_progress_ns.load(Ordering::Acquire);
    if last_progress == 0 {
        return false;
    }
    now_ns.saturating_sub(last_progress) >= DEFAULT_MONOPOLY_YIELD_THRESHOLD_NS
}

/// The verdict of classifying one CPU during the cross-CPU scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// Healthy, idle, offline, or an already-reported ongoing episode.
    Quiet,
    /// A newly detected hard lockup, carrying the silent gap in ns.
    HardOnset(u64),
    /// A newly detected soft lockup, carrying the no-progress gap in ns.
    SoftOnset(u64),
}

/// Classify one CPU's `state` at `now_ns`, latching its per-episode flag so
/// a crossing is reported exactly once. Pure but for the latch swap; the
/// caller does the I/O. Only an `Active` CPU is judged; a hard lockup takes
/// precedence over a soft one, and a soft lockup is only reported for a CPU
/// last seen *in the kernel* (a lone preemptible user task owes no
/// progress).
fn classify(state: &CpuState, now_ns: u64) -> Verdict {
    if WatchdogActivity::from_u8(state.wd_activity.load(Ordering::Acquire))
        != WatchdogActivity::Active
    {
        return Verdict::Quiet;
    }
    match evaluate(
        state.last_seen_ns.load(Ordering::Acquire),
        &state.hard_reported,
        now_ns,
        DEFAULT_HARD_LOCKUP_THRESHOLD_NS,
    ) {
        Sample::Onset(elapsed) => return Verdict::HardOnset(elapsed),
        // Still hard-locked (already reported): do not also raise a soft
        // report for the same wedged CPU.
        Sample::Still => return Verdict::Quiet,
        Sample::Healthy | Sample::Unarmed => {}
    }
    if !state.wd_ctx_in_kernel.load(Ordering::Acquire) {
        return Verdict::Quiet;
    }
    match evaluate(
        state.last_progress_ns.load(Ordering::Acquire),
        &state.stall_reported,
        now_ns,
        DEFAULT_SOFT_LOCKUP_THRESHOLD_NS,
    ) {
        Sample::Onset(elapsed) => Verdict::SoftOnset(elapsed),
        _ => Verdict::Quiet,
    }
}

/// Scan every CPU other than the `observer` for a lockup, reporting and
/// attempting recovery for each newly detected episode.
///
/// Runs from the observer's non-maskable sample, so it can see a CPU that
/// has stopped taking maskable interrupts entirely (a hard lockup) —
/// exactly the case the victim's own timer path is blind to.
fn scan(observer: CpuId, now_ns: u64) {
    for (index, state) in cpu_state::states().iter().enumerate() {
        // The observer is, by definition, alive — skip it.
        if observer as usize == index {
            continue;
        }
        let Ok(target) = CpuId::try_from(index) else {
            continue;
        };
        match classify(state, now_ns) {
            Verdict::HardOnset(elapsed) => {
                // The victim's own last-known sample is stale (it went
                // silent), so mark it pre-silence and read the shared
                // controller live for the device line actually wedging it —
                // the "why" the stale sample cannot give.
                let mut diag = Diag::snapshot(state);
                diag.sample_stale = true;
                diag.stuck = recovery().and_then(WatchdogArch::stuck_interrupt);
                diag.stuck_owner = resolve_stuck_owner(diag.stuck);
                report_lockup(
                    AuditEvent::CpuHardLockupDetected,
                    Level::Error,
                    target,
                    Some(observer),
                    elapsed,
                    &diag,
                );
                drive_recovery(target, WatchdogKind::Hard);
            }
            Verdict::SoftOnset(elapsed) => {
                // A soft-locked CPU is still taking its watchdog sample, so
                // its recorded context is fresh (`sample_stale` stays false)
                // and there is no stuck-line story to tell.
                let diag = Diag::snapshot(state);
                report_lockup(
                    AuditEvent::CpuStallDetected,
                    Level::Error,
                    target,
                    Some(observer),
                    elapsed,
                    &diag,
                );
                drive_recovery(target, WatchdogKind::Soft);
            }
            Verdict::Quiet => {}
        }
    }
}

/// Check `cpu` for a **soft** lockup from its own armed timer-tick path.
///
/// The same-CPU complement of the cross-CPU `scan`'s check: it catches a
/// *contended* CPU whose preemption has stopped making scheduler progress
/// (the tick only fires when the CPU has a competitor, so a lone
/// preemptible task is never sampled here — no false positive). Reads the
/// installed monotonic clock; before it, or for an out-of-range `cpu`, it
/// is a fail-safe no-op. Shares the soft-episode latch with `scan`, so a
/// soft lockup is reported exactly once whichever path sees it first.
pub fn check_stall(cpu: CpuId) {
    let Some(now_ns) = crate::waitq::wait_now_ns() else {
        return;
    };
    let Some(state) = cpu_state::get(cpu) else {
        return;
    };
    let soft = evaluate(
        state.last_progress_ns.load(Ordering::Acquire),
        &state.stall_reported,
        now_ns,
        DEFAULT_SOFT_LOCKUP_THRESHOLD_NS,
    );
    if let Sample::Onset(elapsed) = soft {
        let diag = Diag::snapshot(state);
        report_lockup(
            AuditEvent::CpuStallDetected,
            Level::Error,
            cpu,
            None,
            elapsed,
            &diag,
        );
    }
}

// --- Recovery -------------------------------------------------------

/// A short, fixed tag for a [`RecoveryOutcome`], for the audit record.
fn outcome_tag(outcome: RecoveryOutcome) -> &'static str {
    match outcome {
        RecoveryOutcome::Rescheduled => "rescheduled",
        RecoveryOutcome::AttentionRaised => "attention",
        RecoveryOutcome::Unrecoverable => "unrecoverable",
        RecoveryOutcome::Unsupported => "unsupported",
    }
}

/// Ask the installed port to break `target` out of a `kind` lockup,
/// best-effort, and record the attempt and its honest outcome. A build
/// with no recovery handle records the attempt as `unsupported` rather
/// than silently doing nothing.
fn drive_recovery(target: CpuId, kind: WatchdogKind) {
    let outcome = recovery().map_or(RecoveryOutcome::Unsupported, |arch| {
        arch.request_recovery(target, kind)
    });
    if let Some(sink) = report_sink() {
        recovery_to(sink, target, kind, outcome);
    }
}

/// Render one recovery-attempt record through `sink`. Split from
/// [`drive_recovery`] so host tests can drive the render against a
/// recording sink without touching the process-wide install seam.
fn recovery_to(sink: &dyn Sink, target: CpuId, kind: WatchdogKind, outcome: RecoveryOutcome) {
    let fields = [
        tairix_log::Field {
            key: "cpu",
            value: tairix_log::FieldValue::UnsignedInt(u64::from(target)),
        },
        tairix_log::Field {
            key: "kind",
            value: tairix_log::FieldValue::Str(kind.tag()),
        },
        tairix_log::Field {
            key: "outcome",
            value: tairix_log::FieldValue::Str(outcome_tag(outcome)),
        },
    ];
    emit(sink, Level::Warn, AuditEvent::CpuLockupRecovery, &fields);
}

// --- Diagnostics ("why") --------------------------------------------

/// A snapshot of a CPU's last-known interrupted context — the raw "why" a
/// lockup diagnosis renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Diag {
    /// Last-known interrupted program counter (`0` = none).
    pc: u64,
    /// Last-known running task id ([`WatchdogSample::NO_TASK`] = none).
    task: u64,
    /// Last-known port-defined processor-state word (`0` = none).
    aux: u64,
    /// Whether the last-known context was **kernel** code (`true`) or a
    /// **user** task (`false`). Rendered as the `context` field so a report
    /// says at a glance whether a wedged CPU was last executing in the
    /// kernel (a held lock / in-flight syscall / masked in-kernel spin) or a
    /// user task — the single most decisive clue for the "why".
    in_kernel: bool,
    /// Whether the recorded sample is *stale* — the last one taken before
    /// the CPU went silent (a hard lockup), so pc/aux/context name the
    /// innocent code the CPU last returned to, not the wedge. Rendered as
    /// `sampled=pre_silence`. A soft lockup's sample is live, so this is
    /// `false` and no marker is rendered.
    sample_stale: bool,
    /// The device interrupt the **observer** read live as stuck in the
    /// shared controller (active over pending), or `None`. Rendered as
    /// `stuck_irq=<id>` plus `stuck_state=<active|pending>` — the "why" the
    /// stale sample cannot give. Only a line that could still be delivered
    /// is reported (active, or enabled-and-pending); a masked line cannot
    /// reach a CPU, so it is never blamed. Filled only on the hard-lockup
    /// path (the observer's cross-CPU read); a snapshot of the CPU's own
    /// context leaves it `None`.
    stuck: Option<StuckInterrupt>,
    /// Who owns the [`Self::stuck`] line, resolved against the kernel IRQ
    /// table on the hard-lockup path. It disambiguates the raw `stuck_irq`
    /// id — the recurring "which device is this line?" question — by naming
    /// the driver that bound it (`stuck_owner=<task>`), or reporting that no
    /// driver owns it (`stuck_owner=unbound`, a spurious/contained line, so
    /// the wedge is elsewhere). [`StuckOwner::Unknown`] (no stuck line, or no
    /// resolver installed) renders nothing, so a record never claims an
    /// attribution it does not have.
    stuck_owner: StuckOwner,
}

/// Attribution of a stuck controller line to the task that owns its IRQ
/// binding, resolved by the watchdog for a hard-lockup report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StuckOwner {
    /// No stuck line, or no owner resolver installed: nothing is rendered,
    /// so a record never claims an attribution it could not make.
    Unknown,
    /// The stuck line is bound to no task — a spurious/contained line no
    /// driver owns (rendered `stuck_owner=unbound`), which says the wedge is
    /// elsewhere, not this line.
    Unbound,
    /// The stuck line is bound to this task (rendered `stuck_owner=<hex>`) —
    /// the driver whose device is asserting it.
    Task(u64),
}

impl Diag {
    /// An empty diagnosis (a recovery/clear record carries no context).
    const EMPTY: Self = Self {
        pc: 0,
        task: WatchdogSample::NO_TASK,
        aux: 0,
        in_kernel: false,
        sample_stale: false,
        stuck: None,
        stuck_owner: StuckOwner::Unknown,
    };

    /// Whether this diagnosis carries a real captured sample (as opposed to
    /// the empty recovery/clear record). Only then is the kernel/user
    /// `context` meaningful.
    fn has_sample(&self) -> bool {
        self.pc != 0 || self.task != WatchdogSample::NO_TASK
    }

    /// Read a CPU's recorded last-known context. The observer-supplied
    /// `sample_stale` / `stuck` / `stuck_owner` fields default off; the
    /// hard-lockup path sets them after the snapshot.
    fn snapshot(state: &CpuState) -> Self {
        Self {
            pc: state.wd_ctx_pc.load(Ordering::Acquire),
            task: state.wd_ctx_task.load(Ordering::Acquire),
            aux: state.wd_ctx_aux.load(Ordering::Acquire),
            in_kernel: state.wd_ctx_in_kernel.load(Ordering::Acquire),
            sample_stale: false,
            stuck: None,
            stuck_owner: StuckOwner::Unknown,
        }
    }
}

/// Attribute a stuck line to the driver that bound it, via the installed
/// owner resolver. `None` (no stuck line) or an uninstalled resolver yields
/// [`StuckOwner::Unknown`] so nothing is rendered — a report never claims an
/// attribution it could not make. A stuck line the resolver finds unbound is
/// [`StuckOwner::Unbound`] (a spurious/contained line, so the wedge is
/// elsewhere); a bound line names its owning task.
fn resolve_stuck_owner(stuck: Option<StuckInterrupt>) -> StuckOwner {
    resolve_stuck_owner_with(stuck, irq_owner().map(|r| r as &dyn StuckOwnerResolver))
}

/// The pure core of [`resolve_stuck_owner`]: attribute `stuck` against the
/// given `resolver`, split out so the mapping is host-tested with a fake
/// resolver rather than the process-global install seam.
fn resolve_stuck_owner_with(
    stuck: Option<StuckInterrupt>,
    resolver: Option<&dyn StuckOwnerResolver>,
) -> StuckOwner {
    let Some(stuck) = stuck else {
        return StuckOwner::Unknown;
    };
    match resolver {
        None => StuckOwner::Unknown,
        Some(resolver) => match resolver.owner_of_line(stuck.intid) {
            Some(task) => StuckOwner::Task(task),
            None => StuckOwner::Unbound,
        },
    }
}

/// A short, stable tag for a stuck line's state: whether it is actively
/// storming (`active`) — a handler in flight, the signature of a live
/// wedge — or merely `pending` (enabled and asserted, but not yet taken).
/// Only deliverable lines are ever reported, so a masked line never
/// reaches this tag.
fn stuck_state_tag(stuck: StuckInterrupt) -> &'static str {
    if stuck.active {
        "active"
    } else {
        "pending"
    }
}

/// Render `value` into `buf` as `0x`-prefixed 16-nibble lowercase hex.
fn hex0x(value: u64, buf: &mut [u8; 18]) -> &str {
    buf[0] = b'0';
    buf[1] = b'x';
    let mut hex = [0u8; 16];
    let rendered = format_hex_u64(value, &mut hex);
    let bytes = rendered.as_bytes();
    buf[2..2 + bytes.len()].copy_from_slice(bytes);
    core::str::from_utf8(&buf[..2 + bytes.len()]).unwrap_or("0x")
}

/// Emit one lockup record through the installed sink, if any.
///
/// Allocation-free (stack buffers only) and lock-free, so it is safe on
/// the non-maskable sample path and the dispatch hot path alike. `observer`
/// names the CPU that caught a cross-CPU lockup (a hard lockup, or a
/// buddy-observed soft one); `None` for a same-CPU detection or a
/// recovery/clear record. `diag` carries the last-known context for the
/// "why"; a zero PC / no-task field is omitted so a record only ever
/// carries context it actually has.
fn report_lockup(
    event: AuditEvent,
    level: Level,
    cpu: CpuId,
    observer: Option<CpuId>,
    elapsed_ns: u64,
    diag: &Diag,
) {
    if let Some(sink) = report_sink() {
        report_to(sink, event, level, cpu, observer, elapsed_ns, diag);
    }
}

/// Render one lockup record through `sink`. Split from [`report_lockup`]
/// so host tests can drive the full render against a recording sink
/// without touching the process-wide install seam.
fn report_to(
    sink: &dyn Sink,
    event: AuditEvent,
    level: Level,
    cpu: CpuId,
    observer: Option<CpuId>,
    elapsed_ns: u64,
    diag: &Diag,
) {
    let mut pc_buf = [0u8; 18];
    let mut aux_buf = [0u8; 18];
    let mut owner_buf = [0u8; 18];

    // Build the field list on the stack. The order is stable so a reader
    // and a parser see the same shape every time.
    let mut fields: [tairix_log::Field<'_>; 11] = [tairix_log::Field {
        key: "cpu",
        value: tairix_log::FieldValue::UnsignedInt(u64::from(cpu)),
    }; 11];
    let mut n = 1;
    if let Some(obs) = observer {
        fields[n] = tairix_log::Field {
            key: "observer",
            value: tairix_log::FieldValue::UnsignedInt(u64::from(obs)),
        };
        n += 1;
    }
    fields[n] = tairix_log::Field {
        key: "stalled_ms",
        value: tairix_log::FieldValue::UnsignedInt(elapsed_ns / NS_PER_MS),
    };
    n += 1;
    if diag.pc != 0 {
        fields[n] = tairix_log::Field {
            key: "pc",
            value: tairix_log::FieldValue::Str(hex0x(diag.pc, &mut pc_buf)),
        };
        n += 1;
    }
    if diag.task != WatchdogSample::NO_TASK {
        fields[n] = tairix_log::Field {
            key: "task",
            value: tairix_log::FieldValue::UnsignedInt(diag.task),
        };
        n += 1;
    }
    if diag.aux != 0 {
        fields[n] = tairix_log::Field {
            key: "pstate",
            value: tairix_log::FieldValue::Str(hex0x(diag.aux, &mut aux_buf)),
        };
        n += 1;
    }
    if diag.has_sample() {
        fields[n] = tairix_log::Field {
            key: "context",
            value: tairix_log::FieldValue::Str(if diag.in_kernel { "kernel" } else { "user" }),
        };
        n += 1;
        // A hard lockup's context is the last sample taken *before* the CPU
        // went silent (~`stalled_ms` old), not a live reading — mark it so
        // a reader does not mistake the innocent code the CPU last returned
        // to for the actual wedge.
        if diag.sample_stale {
            fields[n] = tairix_log::Field {
                key: "sampled",
                value: tairix_log::FieldValue::Str("pre_silence"),
            };
            n += 1;
        }
    }
    // The device interrupt currently stuck in the shared controller, read
    // live by the observer — the "why" the stale sample cannot give. Only
    // a line that can still reach a CPU is reported, so `stuck_state` just
    // says whether it is a live storm (`active`) or an enabled line
    // asserted but not yet taken (`pending`).
    if let Some(stuck) = diag.stuck {
        fields[n] = tairix_log::Field {
            key: "stuck_irq",
            value: tairix_log::FieldValue::UnsignedInt(u64::from(stuck.intid)),
        };
        n += 1;
        fields[n] = tairix_log::Field {
            key: "stuck_state",
            value: tairix_log::FieldValue::Str(stuck_state_tag(stuck)),
        };
        n += 1;
        // Who owns the stuck line — the driver whose device is asserting it
        // (`<task>`), or `unbound` for a spurious/contained line no driver
        // owns (so the wedge is elsewhere, not this line). Only rendered
        // when the owner was actually resolved, never a claim we cannot make.
        match diag.stuck_owner {
            StuckOwner::Task(task) => {
                fields[n] = tairix_log::Field {
                    key: "stuck_owner",
                    value: tairix_log::FieldValue::Str(hex0x(task, &mut owner_buf)),
                };
                n += 1;
            }
            StuckOwner::Unbound => {
                fields[n] = tairix_log::Field {
                    key: "stuck_owner",
                    value: tairix_log::FieldValue::Str("unbound"),
                };
                n += 1;
            }
            StuckOwner::Unknown => {}
        }
    }
    emit(sink, level, event, &fields[..n]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_sink::TestSink;
    use alloc::boxed::Box;

    /// Reset a shared per-CPU test slot to the pristine state so each test
    /// starts clean. Tests use disjoint CPU indices (like the sibling
    /// `preempt` tests) so parallel threads never collide on a slot.
    fn reset(cpu: CpuId) -> &'static CpuState {
        let state = cpu_state::get(cpu).expect("test CPU slot exists");
        state.last_progress_ns.store(0, Ordering::Relaxed);
        state.stall_reported.store(false, Ordering::Relaxed);
        state.last_seen_ns.store(0, Ordering::Relaxed);
        state
            .wd_activity
            .store(WatchdogActivity::Offline as u8, Ordering::Relaxed);
        state.hard_reported.store(false, Ordering::Relaxed);
        state.wd_ctx_pc.store(0, Ordering::Relaxed);
        state.wd_ctx_task.store(u64::MAX, Ordering::Relaxed);
        state.wd_ctx_aux.store(0, Ordering::Relaxed);
        state.wd_ctx_in_kernel.store(false, Ordering::Relaxed);
        state
    }

    fn field<'a>(ev: &'a crate::test_sink::CapturedEvent, key: &str) -> Option<&'a str> {
        ev.fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    // --- The pure heartbeat evaluator -------------------------------

    #[test]
    fn an_unarmed_heartbeat_is_never_judged() {
        let latch = AtomicBool::new(false);
        assert_eq!(evaluate(0, &latch, 1_000_000_000, 10), Sample::Unarmed);
    }

    #[test]
    fn recent_progress_is_healthy_and_the_boundary_is_inclusive() {
        let latch = AtomicBool::new(false);
        assert_eq!(evaluate(100, &latch, 109, 10), Sample::Healthy);
        assert_eq!(evaluate(100, &latch, 110, 10), Sample::Onset(10));
    }

    #[test]
    fn crossing_the_threshold_reports_the_episode_once() {
        let latch = AtomicBool::new(false);
        assert_eq!(evaluate(1_000, &latch, 1_010, 10), Sample::Onset(10));
        assert_eq!(evaluate(1_000, &latch, 1_050, 10), Sample::Still);
        assert_eq!(evaluate(1_000, &latch, 1_999, 10), Sample::Still);
    }

    // --- Progress heartbeat + soft recovery -------------------------

    #[test]
    fn progress_after_a_reported_soft_lockup_reports_recovery_once() {
        let state = reset(40);
        state.last_progress_ns.store(100, Ordering::Relaxed);
        assert_eq!(
            evaluate(100, &state.stall_reported, 200, 10),
            Sample::Onset(100)
        );
        assert_eq!(record_progress(state, 250), Some(150));
        assert_eq!(record_progress(state, 260), None);
    }

    #[test]
    fn progress_without_a_reported_soft_lockup_is_silent() {
        let state = reset(41);
        assert_eq!(record_progress(state, 100), None);
    }

    #[test]
    fn a_stamped_heartbeat_is_never_the_unarmed_sentinel() {
        let state = reset(42);
        record_progress(state, 0);
        assert_ne!(state.last_progress_ns.load(Ordering::Relaxed), 0);
    }

    // --- Liveness heartbeat + context capture + hard recovery -------

    #[test]
    fn liveness_records_context_and_reports_hard_recovery_once() {
        let state = reset(43);
        let sample = WatchdogSample {
            pc: 0xdead_beef,
            task: 7,
            aux: 0x3c5,
            in_kernel: true,
        };
        // No prior hard report: healthy path, context captured.
        assert_eq!(record_liveness(state, 1_000, &sample), None);
        assert_eq!(state.wd_ctx_pc.load(Ordering::Relaxed), 0xdead_beef);
        assert_eq!(state.wd_ctx_task.load(Ordering::Relaxed), 7);
        assert_eq!(state.wd_ctx_aux.load(Ordering::Relaxed), 0x3c5);
        assert!(state.wd_ctx_in_kernel.load(Ordering::Relaxed));
        // A latched hard episode: the next liveness clears it and reports
        // the recovery gap once.
        state.hard_reported.store(true, Ordering::Relaxed);
        assert_eq!(record_liveness(state, 1_500, &sample), Some(500));
        assert_eq!(record_liveness(state, 1_600, &sample), None);
    }

    #[test]
    fn alive_refreshes_liveness_without_touching_context() {
        let state = reset(35);
        // A stale liveness heartbeat and a captured context from an earlier
        // real sample.
        state.last_seen_ns.store(1, Ordering::Relaxed);
        state.wd_ctx_pc.store(0xabcd, Ordering::Relaxed);
        state.wd_ctx_task.store(3, Ordering::Relaxed);
        state.wd_ctx_aux.store(0x345, Ordering::Relaxed);
        state.wd_ctx_in_kernel.store(true, Ordering::Relaxed);
        // Proof-of-life from the dispatcher: liveness advances, context is
        // left exactly as the last real sample recorded it.
        assert_eq!(record_alive(state, 5_000), None);
        assert_eq!(state.last_seen_ns.load(Ordering::Relaxed), 5_000);
        assert_eq!(state.wd_ctx_pc.load(Ordering::Relaxed), 0xabcd);
        assert_eq!(state.wd_ctx_task.load(Ordering::Relaxed), 3);
        assert_eq!(state.wd_ctx_aux.load(Ordering::Relaxed), 0x345);
        assert!(state.wd_ctx_in_kernel.load(Ordering::Relaxed));
    }

    #[test]
    fn alive_after_a_reported_hard_lockup_reports_recovery_once() {
        let state = reset(36);
        state.last_seen_ns.store(1_000, Ordering::Relaxed);
        state.hard_reported.store(true, Ordering::Relaxed);
        // Reaching the dispatcher clears a latched hard episode and reports
        // the recovery gap exactly once.
        assert_eq!(record_alive(state, 3_000), Some(2_000));
        assert_eq!(record_alive(state, 3_100), None);
    }

    #[test]
    fn a_stamped_liveness_heartbeat_is_never_the_unarmed_sentinel() {
        let state = reset(37);
        record_alive(state, 0);
        assert_ne!(state.last_seen_ns.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_cpu_returning_from_a_long_idle_park_is_not_hard_locked() {
        // The regression: a CPU idle-parked far longer than the hard
        // threshold (its non-maskable sample is not taken while parked)
        // carries a stale liveness heartbeat. When it wakes it must stamp
        // liveness *before* it republishes Active — exactly the order the
        // dispatch loop uses (`note_alive` then `set_activity(Active)`) — so
        // the cross-CPU scan never sees it Active with the pre-park
        // heartbeat and never reports a false hard lockup.
        let state = reset(39);
        state.wd_ctx_in_kernel.store(true, Ordering::Relaxed);
        // Last real sample was taken before a long idle park.
        state.last_seen_ns.store(1_000, Ordering::Relaxed);
        state.last_progress_ns.store(1_000, Ordering::Relaxed);
        let now = 1_000 + 20 * DEFAULT_HARD_LOCKUP_THRESHOLD_NS;
        // The dispatch loop's proof-of-life stamps at the top, before Active.
        record_alive(state, now);
        record_progress(state, now);
        state
            .wd_activity
            .store(WatchdogActivity::Active as u8, Ordering::Relaxed);
        assert_eq!(classify(state, now), Verdict::Quiet);
        // And without the fix — Active republished over the pre-park
        // heartbeat — the very same scan would have fired.
        state.last_seen_ns.store(1_000, Ordering::Relaxed);
        state.last_progress_ns.store(1_000, Ordering::Relaxed);
        assert!(matches!(classify(state, now), Verdict::HardOnset(_)));
    }

    // --- Classification --------------------------------------------

    #[test]
    fn a_non_active_cpu_is_never_classified() {
        let state = reset(44);
        state.last_seen_ns.store(1, Ordering::Relaxed);
        state.last_progress_ns.store(1, Ordering::Relaxed);
        state.wd_ctx_in_kernel.store(true, Ordering::Relaxed);
        // Idle and Offline: quiet even with ancient heartbeats.
        for activity in [WatchdogActivity::Idle, WatchdogActivity::Offline] {
            state.wd_activity.store(activity as u8, Ordering::Relaxed);
            assert_eq!(classify(state, 1_000_000_000_000), Verdict::Quiet);
        }
    }

    #[test]
    fn an_active_cpu_that_stops_taking_the_sample_is_hard_locked() {
        let state = reset(45);
        state
            .wd_activity
            .store(WatchdogActivity::Active as u8, Ordering::Relaxed);
        state.last_seen_ns.store(1_000, Ordering::Relaxed);
        let now = 1_000 + DEFAULT_HARD_LOCKUP_THRESHOLD_NS;
        assert_eq!(
            classify(state, now),
            Verdict::HardOnset(DEFAULT_HARD_LOCKUP_THRESHOLD_NS)
        );
        // Latched: a later scan of the same wedged CPU stays quiet.
        assert_eq!(classify(state, now + 1_000), Verdict::Quiet);
    }

    #[test]
    fn hard_lockup_takes_precedence_over_soft() {
        let state = reset(46);
        state
            .wd_activity
            .store(WatchdogActivity::Active as u8, Ordering::Relaxed);
        state.wd_ctx_in_kernel.store(true, Ordering::Relaxed);
        // Both heartbeats are ancient, but liveness (hard) wins.
        state.last_seen_ns.store(1, Ordering::Relaxed);
        state.last_progress_ns.store(1, Ordering::Relaxed);
        let now = 1 + DEFAULT_HARD_LOCKUP_THRESHOLD_NS + 5;
        assert!(matches!(classify(state, now), Verdict::HardOnset(_)));
        // The soft latch was never taken, so a subsequent same-CPU
        // `check_stall` could still report the soft condition if the CPU
        // somehow resumed taking samples but not dispatching.
        assert!(!state.stall_reported.load(Ordering::Relaxed));
    }

    #[test]
    fn an_active_in_kernel_cpu_that_stops_dispatching_is_soft_locked() {
        let state = reset(47);
        state
            .wd_activity
            .store(WatchdogActivity::Active as u8, Ordering::Relaxed);
        state.wd_ctx_in_kernel.store(true, Ordering::Relaxed);
        let now = 1_000 + DEFAULT_SOFT_LOCKUP_THRESHOLD_NS;
        // Liveness fresh (not hard), progress ancient.
        state.last_seen_ns.store(now, Ordering::Relaxed);
        state.last_progress_ns.store(1_000, Ordering::Relaxed);
        assert_eq!(
            classify(state, now),
            Verdict::SoftOnset(DEFAULT_SOFT_LOCKUP_THRESHOLD_NS)
        );
    }

    #[test]
    fn a_lone_user_task_is_never_soft_locked() {
        let state = reset(48);
        state
            .wd_activity
            .store(WatchdogActivity::Active as u8, Ordering::Relaxed);
        // Last seen in *user* mode: no scheduler progress is owed.
        state.wd_ctx_in_kernel.store(false, Ordering::Relaxed);
        let now = 1_000 + DEFAULT_SOFT_LOCKUP_THRESHOLD_NS;
        state.last_seen_ns.store(now, Ordering::Relaxed);
        state.last_progress_ns.store(1_000, Ordering::Relaxed);
        assert_eq!(classify(state, now), Verdict::Quiet);
    }

    #[test]
    fn a_healthy_active_cpu_is_quiet() {
        let state = reset(49);
        state
            .wd_activity
            .store(WatchdogActivity::Active as u8, Ordering::Relaxed);
        state.wd_ctx_in_kernel.store(true, Ordering::Relaxed);
        let now = 1_000_000;
        state.last_seen_ns.store(now, Ordering::Relaxed);
        state.last_progress_ns.store(now, Ordering::Relaxed);
        assert_eq!(classify(state, now), Verdict::Quiet);
    }

    // --- Public entry points fail closed ----------------------------

    #[test]
    fn public_entry_points_fail_closed_for_an_out_of_range_cpu() {
        note_progress(u32::MAX, 1_000);
        check_stall(u32::MAX);
        set_activity(u32::MAX, WatchdogActivity::Active);
        // An out-of-range sampling CPU still runs the scan harmlessly.
        on_watchdog_tick(u32::MAX, 1_000, &WatchdogSample::EMPTY);
    }

    #[test]
    fn on_watchdog_tick_stamps_liveness_and_context() {
        let state = reset(38);
        let sample = WatchdogSample {
            pc: 0x1234,
            task: 9,
            aux: 0x2c0,
            in_kernel: true,
        };
        on_watchdog_tick(38, 5_000, &sample);
        assert_ne!(state.last_seen_ns.load(Ordering::Relaxed), 0);
        assert_eq!(state.wd_ctx_pc.load(Ordering::Relaxed), 0x1234);
        assert_eq!(state.wd_ctx_task.load(Ordering::Relaxed), 9);
    }

    // --- Monopoly guard (a lone CPU-bound user task) ----------------

    /// Arm `state` as an Active CPU whose scheduler progress was last
    /// stamped at `progress_ns`.
    fn active_with_progress(cpu: CpuId, progress_ns: u64) -> &'static CpuState {
        let state = reset(cpu);
        state
            .wd_activity
            .store(WatchdogActivity::Active as u8, Ordering::Relaxed);
        state.last_progress_ns.store(progress_ns, Ordering::Relaxed);
        state
    }

    #[test]
    fn a_lone_user_task_past_the_guard_monopolises() {
        let state = active_with_progress(50, 1_000);
        let now = 1_000 + DEFAULT_MONOPOLY_YIELD_THRESHOLD_NS;
        assert!(monopolises_cpu(state, now, false));
        // Boundary is inclusive; a hair under is not yet a monopoly.
        assert!(!monopolises_cpu(state, now - 1, false));
    }

    #[test]
    fn kernel_code_is_never_force_yielded() {
        let state = active_with_progress(51, 1_000);
        let now = 1_000 + 10 * DEFAULT_MONOPOLY_YIELD_THRESHOLD_NS;
        // The kernel is non-preemptible: a CPU sampled in kernel code is
        // never a monopoly candidate, however long since progress.
        assert!(!monopolises_cpu(state, now, true));
    }

    #[test]
    fn a_non_active_or_unarmed_cpu_never_monopolises() {
        // Not Active (parked/offline): owes no progress, never a monopoly.
        let idle = reset(52);
        idle.last_progress_ns.store(1_000, Ordering::Relaxed);
        let now = 1_000 + 10 * DEFAULT_MONOPOLY_YIELD_THRESHOLD_NS;
        assert!(!monopolises_cpu(idle, now, false));
        // Active but progress never armed (0): fail closed, no phantom yield.
        let fresh = reset(53);
        fresh
            .wd_activity
            .store(WatchdogActivity::Active as u8, Ordering::Relaxed);
        assert!(!monopolises_cpu(fresh, now, false));
    }

    #[test]
    fn on_watchdog_tick_requests_a_forced_yield_for_a_monopolising_user_cpu() {
        let state = active_with_progress(54, 1_000);
        state.force_yield.store(false, Ordering::Relaxed);
        let sample = WatchdogSample {
            pc: 0x4000_0000,
            task: 21,
            aux: 0x6000_0000,
            in_kernel: false,
        };
        on_watchdog_tick(54, 1_000 + DEFAULT_MONOPOLY_YIELD_THRESHOLD_NS, &sample);
        assert!(state.force_yield.load(Ordering::Relaxed));
    }

    #[test]
    fn on_watchdog_tick_does_not_force_yield_a_kernel_or_recent_cpu() {
        // Sampled in the kernel: never force-yielded.
        let in_kernel = active_with_progress(55, 1_000);
        in_kernel.force_yield.store(false, Ordering::Relaxed);
        let ksample = WatchdogSample {
            pc: 0x1000,
            task: WatchdogSample::NO_TASK,
            aux: 0x3c5,
            in_kernel: true,
        };
        on_watchdog_tick(
            55,
            1_000 + 10 * DEFAULT_MONOPOLY_YIELD_THRESHOLD_NS,
            &ksample,
        );
        assert!(!in_kernel.force_yield.load(Ordering::Relaxed));
        // A user task that returned to the scheduler recently: not a
        // monopoly, so no forced yield.
        let recent = active_with_progress(56, 1_000);
        recent.force_yield.store(false, Ordering::Relaxed);
        let usample = WatchdogSample {
            pc: 0x4000_0000,
            task: 22,
            aux: 0x6000_0000,
            in_kernel: false,
        };
        on_watchdog_tick(56, 1_500, &usample);
        assert!(!recent.force_yield.load(Ordering::Relaxed));
    }

    // --- Diagnostics rendering --------------------------------------

    #[test]
    fn hex0x_prefixes_and_pads_to_sixteen_nibbles() {
        let mut buf = [0u8; 18];
        assert_eq!(hex0x(0xabc, &mut buf), "0x0000000000000abc");
    }

    #[test]
    fn a_hard_lockup_record_carries_the_full_diagnosis() {
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let diag = Diag {
            pc: 0xffff_0000_1234_5678,
            task: 12,
            aux: 0x3c5,
            in_kernel: true,
            sample_stale: true,
            stuck: Some(StuckInterrupt {
                intid: 37,
                active: true,
            }),
            stuck_owner: StuckOwner::Task(13),
        };
        report_to(
            sink,
            AuditEvent::CpuHardLockupDetected,
            Level::Error,
            2,
            Some(0),
            7_000_000_000,
            &diag,
        );
        let events = sink.snapshot();
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev.id, AuditEvent::CpuHardLockupDetected.id());
        assert_eq!(ev.level, Level::Error);
        assert_eq!(field(ev, "cpu"), Some("2"));
        assert_eq!(field(ev, "observer"), Some("0"));
        assert_eq!(field(ev, "stalled_ms"), Some("7000"));
        assert_eq!(field(ev, "pc"), Some("0xffff000012345678"));
        assert_eq!(field(ev, "task"), Some("12"));
        assert_eq!(field(ev, "pstate"), Some("0x00000000000003c5"));
        // The kernel/user context distinguishes an in-kernel wedge from a
        // spinning user task — the most decisive clue for the "why".
        assert_eq!(field(ev, "context"), Some("kernel"));
        // The recorded pc/pstate are the last sample *before* the CPU went
        // silent, and the observer read the live stuck controller line.
        assert_eq!(field(ev, "sampled"), Some("pre_silence"));
        assert_eq!(field(ev, "stuck_irq"), Some("37"));
        // The state pins whether the line is a live storm (`active`) or an
        // enabled-but-not-yet-taken line (`pending`).
        assert_eq!(field(ev, "stuck_state"), Some("active"));
        // A bound line names the driver that owns it, so a reader knows
        // whose device is asserting it rather than a bare interrupt id.
        assert_eq!(field(ev, "stuck_owner"), Some("0x000000000000000d"));
    }

    #[test]
    fn a_hard_lockup_without_a_stuck_line_omits_it() {
        // No SPI is stuck (the wedge is a pure in-kernel spin with IRQs
        // masked, not a storm), so the observer reports no line rather than
        // a fabricated one — but still marks the sample pre-silence.
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let diag = Diag {
            pc: 0x0021_d42c,
            task: WatchdogSample::NO_TASK,
            aux: 0x345,
            in_kernel: true,
            sample_stale: true,
            stuck: None,
            stuck_owner: StuckOwner::Unknown,
        };
        report_to(
            sink,
            AuditEvent::CpuHardLockupDetected,
            Level::Error,
            1,
            Some(0),
            10_000_000_000,
            &diag,
        );
        let ev = &sink.snapshot()[0];
        assert_eq!(field(ev, "context"), Some("kernel"));
        assert_eq!(field(ev, "sampled"), Some("pre_silence"));
        assert_eq!(field(ev, "stuck_irq"), None);
        assert_eq!(field(ev, "stuck_state"), None);
        // No stuck line means no owner to attribute.
        assert_eq!(field(ev, "stuck_owner"), None);
    }

    #[test]
    fn stuck_state_tag_names_active_and_pending() {
        let tag = |active| stuck_state_tag(StuckInterrupt { intid: 37, active });
        assert_eq!(tag(true), "active");
        assert_eq!(tag(false), "pending");
    }

    #[test]
    fn a_pending_unbound_stuck_line_renders_pending_and_unbound() {
        // Only deliverable lines reach a report, so a `pending` line is an
        // enabled, asserted-but-not-yet-taken line — a real suspect. When no
        // driver owns it (`unbound`), that says the wedge is elsewhere; the
        // masked, undeliverable line the observer used to blame is never
        // reported at all now.
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let diag = Diag {
            pc: 0x0021_d524,
            task: WatchdogSample::NO_TASK,
            aux: 0x345,
            in_kernel: true,
            sample_stale: true,
            stuck: Some(StuckInterrupt {
                intid: 50,
                active: false,
            }),
            stuck_owner: StuckOwner::Unbound,
        };
        report_to(
            sink,
            AuditEvent::CpuHardLockupDetected,
            Level::Error,
            1,
            Some(0),
            10_000_000_000,
            &diag,
        );
        let ev = &sink.snapshot()[0];
        assert_eq!(field(ev, "stuck_irq"), Some("50"));
        assert_eq!(field(ev, "stuck_state"), Some("pending"));
        assert_eq!(field(ev, "stuck_owner"), Some("unbound"));
    }

    #[test]
    fn a_user_context_record_says_user() {
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let diag = Diag {
            pc: 0x4000_0000,
            task: 15,
            aux: 0x6000_0000,
            in_kernel: false,
            sample_stale: false,
            stuck: None,
            stuck_owner: StuckOwner::Unknown,
        };
        report_to(
            sink,
            AuditEvent::CpuStallDetected,
            Level::Error,
            1,
            None,
            10_000_000_000,
            &diag,
        );
        let ev = &sink.snapshot()[0];
        assert_eq!(field(ev, "context"), Some("user"));
        // A soft lockup's sample is live (the CPU still takes its watchdog
        // sample), so it carries no pre-silence marker and no stuck line.
        assert_eq!(field(ev, "sampled"), None);
        assert_eq!(field(ev, "stuck_irq"), None);
        assert_eq!(field(ev, "stuck_state"), None);
    }

    #[test]
    fn a_record_omits_context_it_does_not_have() {
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        report_to(
            sink,
            AuditEvent::CpuStallCleared,
            Level::Warn,
            1,
            None,
            0,
            &Diag::EMPTY,
        );
        let events = sink.snapshot();
        let ev = &events[0];
        assert_eq!(field(ev, "cpu"), Some("1"));
        assert_eq!(field(ev, "stalled_ms"), Some("0"));
        assert_eq!(field(ev, "observer"), None);
        assert_eq!(field(ev, "pc"), None);
        assert_eq!(field(ev, "task"), None);
        assert_eq!(field(ev, "pstate"), None);
        // A clear record carries no sample, so neither the pre-silence
        // marker nor a stuck line is rendered.
        assert_eq!(field(ev, "sampled"), None);
        assert_eq!(field(ev, "stuck_irq"), None);
        assert_eq!(field(ev, "stuck_state"), None);
    }

    #[test]
    fn resolve_stuck_owner_attributes_a_bound_line_and_reports_unbound_otherwise() {
        // A fake resolver standing in for the live IRQ table: line 42 is
        // bound to task 7, every other line is unbound.
        struct FakeOwner;
        impl StuckOwnerResolver for FakeOwner {
            fn owner_of_line(&self, line: u32) -> Option<u64> {
                (line == 42).then_some(7)
            }
        }
        let fake = FakeOwner;
        let resolver: Option<&dyn StuckOwnerResolver> = Some(&fake);

        let si = |intid| {
            Some(StuckInterrupt {
                intid,
                active: false,
            })
        };
        // No stuck line: nothing to attribute (renders no owner).
        assert_eq!(
            resolve_stuck_owner_with(None, resolver),
            StuckOwner::Unknown
        );
        // A stuck line bound to a driver names its task.
        assert_eq!(
            resolve_stuck_owner_with(si(42), resolver),
            StuckOwner::Task(7)
        );
        // A stuck line no driver owns is unbound — the wedge is elsewhere.
        assert_eq!(
            resolve_stuck_owner_with(si(111), resolver),
            StuckOwner::Unbound
        );
        // With no resolver installed at all, nothing is claimed.
        assert_eq!(resolve_stuck_owner_with(si(42), None), StuckOwner::Unknown);
    }

    #[test]
    fn a_recovery_record_names_the_kind_and_outcome() {
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        recovery_to(
            sink,
            3,
            WatchdogKind::Hard,
            RecoveryOutcome::AttentionRaised,
        );
        let events = sink.snapshot();
        let ev = &events[0];
        assert_eq!(ev.id, AuditEvent::CpuLockupRecovery.id());
        assert_eq!(field(ev, "cpu"), Some("3"));
        assert_eq!(field(ev, "kind"), Some("hard"));
        assert_eq!(field(ev, "outcome"), Some("attention"));
    }
}
