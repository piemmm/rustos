//! Per-CPU stall watchdog: report to the debug console when a CPU stops
//! making scheduler progress.
//!
//! # What it detects
//!
//! A **CPU stall** here is a soft lockup: a CPU that keeps executing but
//! does not return to the scheduler for far longer than any legitimate
//! bounded operation should take — a runaway in-kernel loop, a task that
//! never yields, a lock held across a wedged device access. The kernel is
//! non-preemptible while in kernel mode, so such a CPU keeps its held
//! locks and never reaches the dispatch loop; nothing else on that CPU
//! runs. Left unreported it looks, from the outside, like the machine has
//! silently seized. This watchdog makes it *loud* on the serial debug
//! output (fail loud) so the stall is diagnosable instead of mysterious.
//!
//! # How it works — two sites, one clock
//!
//! The mechanism is the tickless-kernel analogue of Linux's softlockup
//! detector, adapted to TAIRiX's one-shot timer and dispatch loop:
//!
//! * **Heartbeat** — the dispatch loop calls [`note_progress`] on every
//!   iteration, stamping this CPU's per-CPU slot with the current
//!   monotonic time. One loop iteration means the scheduler ran and made
//!   a decision, exactly as a Linux watchdog thread getting scheduled
//!   means its CPU is healthy.
//! * **Check** — the port's timer-tick path calls [`check_stall`] on
//!   every fired one-shot. Interrupts stay deliverable while in-kernel
//!   code runs (the kernel is fully preemptive at the interrupt boundary,
//!   even though it does not context-switch mid-syscall), so the tick
//!   still fires on a CPU whose task is looping without yielding. The
//!   check compares now against the last heartbeat: once the gap exceeds
//!   [`DEFAULT_STALL_THRESHOLD_NS`] the CPU is stalled.
//!
//! Both sites read the *same* monotonic clock ([`crate::waitq::WaitQueueArch::now_ns`]
//! / the arch `monotonic_ns`), so the comparison is exact.
//!
//! # Reporting discipline
//!
//! * Each stall episode is reported **once** (a per-CPU latch), never once
//!   per tick — a stalled CPU takes many ticks, and a report per tick
//!   would itself flood the console.
//! * When a reported-stalled CPU makes progress again, the recovery is
//!   reported once too, so a stall that clears leaves an honest,
//!   self-closing record rather than a dangling "stuck forever" line.
//! * The report is **allocation-free and lock-free**: it renders into
//!   stack buffers and writes through the installed `Sink`, whose serial
//!   backing is safe to drive from interrupt context (it refuses same-CPU
//!   re-entrancy and falls back to a bounded direct write). This is why
//!   the check can run in the timer ISR — the one place a stall is
//!   observable, because the dispatcher by definition is not running.
//!
//! # Fail closed
//!
//! Before the report sink is installed (early boot, host tests of
//! unrelated paths), or on a CPU whose heartbeat has never been stamped,
//! or for an out-of-range CPU id, the watchdog makes no judgement and
//! emits nothing — it never fabricates a stall and never panics.

use core::sync::atomic::Ordering;

use tairix_kernel_sched_api::CpuId;
use tairix_log::{Level, Sink};
use tairix_sync::once::OnceCell;
use tairix_util::fmt::format_u64;

use crate::audit::{emit, AuditEvent};
use crate::cpu_state::{self, CpuState};

/// How long a CPU may run without any scheduler progress before it is
/// reported stalled, in nanoseconds (10 seconds).
///
/// This is a diagnostic policy value, not a resource capacity: no correct
/// bounded kernel operation withholds the CPU from the scheduler for ten
/// seconds, so a gap this large is a genuine soft lockup rather than a
/// long-but-legitimate wait. It is deliberately generous so a heavily
/// loaded but healthy machine is never reported, while a truly wedged CPU
/// still surfaces within seconds. It lives here, once, as the single
/// definition every port's tick path checks against.
pub const DEFAULT_STALL_THRESHOLD_NS: u64 = 10_000_000_000;

/// Nanoseconds per millisecond, for rendering the human-facing stall
/// duration.
const NS_PER_MS: u64 = 1_000_000;

/// The installed report sink, or `None` before the boot path wires it.
///
/// Set once per boot ([`install_report_sink`]). While unset the watchdog
/// records heartbeats and detects stalls but emits nothing, so a build
/// that never installs a sink (host tests of unrelated paths) leaves the
/// reporting path a fail-safe no-op.
static REPORT_SINK: OnceCell<&'static (dyn Sink + Sync)> = OnceCell::new();

/// Install the sink the watchdog reports stalls through. Idempotent by
/// policy: the boot path installs exactly one; a later call is a benign
/// no-op.
pub fn install_report_sink(sink: &'static (dyn Sink + Sync)) {
    let _ = REPORT_SINK.set(sink);
}

/// The report sink the watchdog currently emits through, if installed.
fn report_sink() -> Option<&'static (dyn Sink + Sync)> {
    REPORT_SINK.get().ok().flatten().copied()
}

/// The outcome of one watchdog sample on a CPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sample {
    /// The CPU's heartbeat has never been stamped — no dispatch has run
    /// here yet, so the watchdog makes no judgement (fail closed).
    Unarmed,
    /// Progress is recent (within the threshold): the CPU is healthy.
    Healthy,
    /// The CPU crossed the threshold and this is the first sample to
    /// observe the episode: report a new stall. Carries the elapsed
    /// no-progress duration in nanoseconds.
    StallOnset(u64),
    /// The CPU is stalled but the episode was already reported: stay
    /// quiet (report each episode once, not once per tick).
    StillStalled,
}

/// Classify `state` at time `now_ns` against `threshold_ns`, latching the
/// per-CPU "already reported" flag so a crossed threshold reports exactly
/// once per episode. Pure but for the latch swap; the caller does the I/O.
fn sample(state: &CpuState, now_ns: u64, threshold_ns: u64) -> Sample {
    let last = state.last_progress_ns.load(Ordering::Acquire);
    if last == 0 {
        return Sample::Unarmed;
    }
    let elapsed = now_ns.saturating_sub(last);
    if elapsed < threshold_ns {
        return Sample::Healthy;
    }
    if state.stall_reported.swap(true, Ordering::AcqRel) {
        Sample::StillStalled
    } else {
        Sample::StallOnset(elapsed)
    }
}

/// Stamp `state` with progress at `now_ns`, clearing the episode latch.
///
/// Returns `Some(stalled_ns)` when the CPU was in a *reported* stall and
/// has now recovered (so the caller emits a recovery record), where
/// `stalled_ns` is how long the CPU went without progress up to this
/// recovery; returns `None` on the ordinary healthy path. A `0` reading
/// is stamped as `1` so a stamped heartbeat is never mistaken for the
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
/// Pure accounting on the common path (one atomic store); it emits only
/// on the rare edge where a previously-reported stall clears, so it is
/// cheap enough for the hot dispatch path. An out-of-range `cpu` is a
/// no-op (fail closed).
pub fn note_progress(cpu: CpuId, now_ns: u64) {
    let Some(state) = cpu_state::get(cpu) else {
        return;
    };
    if let Some(stalled_ns) = record_progress(state, now_ns) {
        report(cpu, AuditEvent::CpuStallCleared, stalled_ns);
    }
}

/// Check `cpu` for a stall from the port's timer-tick path.
///
/// Reads the current monotonic time from the installed wait-queue arch
/// hook (the same clock the heartbeat uses) and reports a newly detected
/// stall through the installed sink. Safe to call from interrupt context:
/// the sample is lock-free and the report path is allocation-free and
/// interrupt-re-entrancy-safe. Before the clock hook or the report sink
/// is installed, or for an out-of-range `cpu`, it is a fail-safe no-op.
pub fn check_stall(cpu: CpuId) {
    let Some(now_ns) = crate::waitq::wait_now_ns() else {
        return;
    };
    let Some(state) = cpu_state::get(cpu) else {
        return;
    };
    if let Sample::StallOnset(elapsed_ns) = sample(state, now_ns, DEFAULT_STALL_THRESHOLD_NS) {
        report(cpu, AuditEvent::CpuStallDetected, elapsed_ns);
    }
}

/// Emit one watchdog record through the installed sink, if any.
fn report(cpu: CpuId, event: AuditEvent, elapsed_ns: u64) {
    if let Some(sink) = report_sink() {
        report_to(sink, cpu, event, elapsed_ns);
    }
}

/// Render and emit one watchdog record through `sink`.
///
/// Allocation-free (stack buffers only) and lock-free, so it is safe on
/// the timer ISR and the dispatch hot path alike. Split from [`report`]
/// so host tests can drive the full render-and-emit path against a
/// recording sink without touching the process-wide install seam.
fn report_to(sink: &dyn Sink, cpu: CpuId, event: AuditEvent, elapsed_ns: u64) {
    let level = match event {
        AuditEvent::CpuStallDetected => Level::Error,
        _ => Level::Warn,
    };
    let mut cpu_buf = [0u8; 20];
    let mut ms_buf = [0u8; 20];
    let cpu_str = format_u64(u64::from(cpu), &mut cpu_buf);
    let ms_str = format_u64(elapsed_ns / NS_PER_MS, &mut ms_buf);
    let fields = [
        tairix_log::Field {
            key: "cpu",
            value: tairix_log::FieldValue::Str(cpu_str),
        },
        tairix_log::Field {
            key: "stalled_ms",
            value: tairix_log::FieldValue::Str(ms_str),
        },
    ];
    emit(sink, level, event, &fields);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_sink::TestSink;
    use alloc::boxed::Box;

    /// Reset a shared per-CPU test slot to the pristine "unarmed" state so
    /// each test starts clean. Tests use disjoint CPU indices (like the
    /// sibling `preempt` tests) so parallel threads never collide.
    fn reset(cpu: CpuId) -> &'static CpuState {
        let state = cpu_state::get(cpu).expect("test CPU slot exists");
        state.last_progress_ns.store(0, Ordering::Relaxed);
        state.stall_reported.store(false, Ordering::Relaxed);
        state
    }

    #[test]
    fn an_unarmed_cpu_is_never_judged_stalled() {
        let state = reset(50);
        assert_eq!(sample(state, 1_000_000_000, 10), Sample::Unarmed);
    }

    #[test]
    fn recent_progress_is_healthy() {
        let state = reset(51);
        assert_eq!(record_progress(state, 1_000), None);
        assert_eq!(sample(state, 1_000 + 9, 10), Sample::Healthy);
    }

    #[test]
    fn crossing_the_threshold_reports_the_episode_once() {
        let state = reset(52);
        assert_eq!(record_progress(state, 1_000), None);
        // First sample past the threshold: onset, carrying the elapsed gap.
        assert_eq!(sample(state, 1_000 + 10, 10), Sample::StallOnset(10));
        // Every later sample in the same episode stays quiet.
        assert_eq!(sample(state, 1_000 + 50, 10), Sample::StillStalled);
        assert_eq!(sample(state, 1_000 + 999, 10), Sample::StillStalled);
    }

    #[test]
    fn the_threshold_boundary_is_inclusive() {
        let state = reset(53);
        record_progress(state, 100);
        // Exactly at the threshold is stalled; one below is healthy.
        assert_eq!(sample(state, 109, 10), Sample::Healthy);
        assert_eq!(sample(state, 110, 10), Sample::StallOnset(10));
    }

    #[test]
    fn progress_after_a_reported_stall_reports_recovery_once() {
        let state = reset(54);
        record_progress(state, 100);
        assert_eq!(sample(state, 200, 10), Sample::StallOnset(100));
        // Recovery: the gap up to recovery is reported exactly once.
        assert_eq!(record_progress(state, 250), Some(150));
        // A second progress with no intervening stall reports nothing.
        assert_eq!(record_progress(state, 260), None);
    }

    #[test]
    fn progress_without_a_reported_stall_is_silent() {
        let state = reset(55);
        record_progress(state, 100);
        // Healthy sample never latched the report flag, so progress is
        // silent even though time advanced.
        assert_eq!(sample(state, 105, 10), Sample::Healthy);
        assert_eq!(record_progress(state, 110), None);
    }

    #[test]
    fn a_stamped_heartbeat_is_never_the_unarmed_sentinel() {
        let state = reset(56);
        // A genuine zero reading must not read back as "unarmed".
        record_progress(state, 0);
        assert_ne!(state.last_progress_ns.load(Ordering::Relaxed), 0);
        assert_eq!(sample(state, 0, 10), Sample::Healthy);
    }

    #[test]
    fn public_entry_points_fail_closed_for_an_out_of_range_cpu() {
        // No panic, no report: the slot lookup simply misses.
        note_progress(u32::MAX, 1_000);
        check_stall(u32::MAX);
    }

    #[test]
    fn report_renders_cpu_and_duration_fields() {
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        report_to(sink, 3, AuditEvent::CpuStallDetected, 12_345_000_000);
        let events = sink.snapshot();
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev.id, AuditEvent::CpuStallDetected.id());
        assert_eq!(ev.level, Level::Error);
        let field = |key: &str| {
            ev.fields
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(field("cpu"), Some("3"));
        // 12_345_000_000 ns == 12345 ms.
        assert_eq!(field("stalled_ms"), Some("12345"));
    }

    #[test]
    fn recovery_report_is_a_warning() {
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        report_to(sink, 0, AuditEvent::CpuStallCleared, 0);
        let events = sink.snapshot();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].level, Level::Warn);
        assert_eq!(events[0].id, AuditEvent::CpuStallCleared.id());
    }
}
