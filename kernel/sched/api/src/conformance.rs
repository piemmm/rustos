//! Shared `SchedulerPolicy` conformance suite.
//!
//! Every concrete scheduler must pass this suite. It is written purely
//! against the [`SchedulerPolicy`] trait and the host [`TestArch`], so it
//! drives any policy without naming a concrete type. The companion
//! integration test `kernel/sched/api/tests/conformance.rs` runs
//! [`run_all`] against the selected implementation, and each
//! `kernel/sched/<impl>` crate runs it against itself.
//!
//! The suite is deterministic and single-threaded: it drives `cpus`
//! simulated cores round-robin from one host thread, advancing
//! [`TestArch`]'s tick counter explicitly. That keeps the assertions
//! reproducible (no flaky tests) while still exercising
//! the ≥ 4-core SMP paths (per-CPU queues, work stealing, IPI bookkeeping).
//!
//! Only the public trait surface is used — no implementation internals —
//! so the same source is a valid acceptance test for a future EEVDF, RT,
//! or any other policy.

#![cfg(feature = "conformance")]

extern crate alloc;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch::{CoreClass, TestArch};
use crate::config::SchedulerConfig;
use crate::error::SchedError;
use crate::outcome::{ExitDisposition, StepOutcome};
use crate::policy::SchedulerPolicy;
use crate::task::{Priority, SchedClass, TaskAction, TaskState};

/// Run the entire conformance suite against scheduler policy `S`.
///
/// # Panics
///
/// Panics (failing the test) if any required property does not hold.
pub fn run_all<S: SchedulerPolicy<TestArch>>() {
    spawn_runs_and_exits::<S>();
    spawn_parked_stays_parked_until_unpark::<S>();
    block_wake_roundtrip::<S>();
    unpark_before_park_is_not_lost::<S>();
    cross_cpu_ipc_reply_wakes_the_caller_without_delay::<S>();
    lifecycle_error_codes::<S>();
    yield_current_semantics::<S>();
    running_cpu_agrees_with_current_task::<S>();
    cpu_time_is_accounted_per_dispatch::<S>();
    load_observations_track_dispatch::<S>();
    no_starvation_under_priority_boost::<S>();
    fairness_no_band_is_starved::<S>();
    realtime_is_dispatched_ahead_of_a_full_time_shared_queue::<S>();
    realtime_is_not_preempted_by_time_shared_then_releases_on_park::<S>();
    realtime_peers_share_the_cpu_round_robin::<S>();
    sched_class_reports_and_fails_closed_on_unknown::<S>();
    priority_reports_changes_and_fails_closed::<S>();
    smp_stress_four_cores::<S>();
    heterogeneous_topology_preserves_liveness::<S>();
}

/// Build a policy with `cpus` cores and the given per-band capacity.
fn make<S: SchedulerPolicy<TestArch>>(
    cpus: u32,
    queue_capacity_per_band: usize,
) -> (Arc<TestArch>, S) {
    let arch = Arc::new(TestArch::new(cpus).expect("test arch"));
    let cfg = SchedulerConfig {
        cpus,
        queue_capacity_per_band,
        yields_before_demotion: 1,
        boost_interval_ticks: 2,
    };
    let sched = S::new(cfg, arch.clone()).expect("scheduler builds");
    (arch, sched)
}

/// A spawned task runs exactly once and reaches `Exited`, sending an IPI.
fn spawn_runs_and_exits<S: SchedulerPolicy<TestArch>>() {
    let (arch, sched) = make::<S>(2, 64);
    let counter = Arc::new(AtomicU64::new(0));
    let c2 = counter.clone();
    let id = sched
        .spawn(0, Priority::Normal, move |_ctx| {
            c2.fetch_add(1, Ordering::Relaxed);
            TaskAction::Exit
        })
        .expect("spawn");
    arch.set_current_cpu(0);
    assert_eq!(sched.step(0), Ok(StepOutcome::Ran(id)), "task dispatched");
    assert_eq!(counter.load(Ordering::Relaxed), 1, "body ran once");
    assert_eq!(sched.state_of(id), TaskState::Exited, "task exited");
    assert!(arch.ipi_count(0) >= 1, "spawn notifies the home CPU");
    assert_eq!(sched.step(0), Ok(StepOutcome::Idle), "no work remains");
}

/// A task admitted through `spawn_parked` is born [`TaskState::Parked`]
/// and is **not** dispatchable on any CPU until an explicit `unpark`: a
/// `step` before the unpark finds no runnable work, and no wake IPI is
/// raised by the parked admission itself. This is the property the
/// process-admit path relies on to install a task's capability record and
/// address space under the returned id before any core can run the task
/// and take its first syscall.
fn spawn_parked_stays_parked_until_unpark<S: SchedulerPolicy<TestArch>>() {
    let (arch, sched) = make::<S>(2, 64);
    arch.set_current_cpu(0);
    let counter = Arc::new(AtomicU64::new(0));
    let c2 = counter.clone();
    let id = sched
        .spawn_parked(0, Priority::Normal, move |_ctx| {
            c2.fetch_add(1, Ordering::Relaxed);
            TaskAction::Exit
        })
        .expect("spawn_parked");
    assert_eq!(
        sched.state_of(id),
        TaskState::Parked,
        "born parked, not ready"
    );
    // No wake IPI from the parked admission, and no CPU can dispatch it.
    assert_eq!(arch.ipi_count(0), 0, "parked admission raises no IPI");
    assert_eq!(sched.step(0), Ok(StepOutcome::Idle), "not runnable yet");
    assert_eq!(counter.load(Ordering::Relaxed), 0, "body never ran");
    assert_eq!(sched.state_of(id), TaskState::Parked, "still parked");

    // Unpark makes it runnable exactly once, exactly like a spawned task.
    sched.unpark(id).expect("unpark the parked task");
    let mut ran = false;
    for cpu in 0..2 {
        arch.set_current_cpu(cpu);
        if sched.step(cpu) == Ok(StepOutcome::Ran(id)) {
            ran = true;
            break;
        }
    }
    assert!(ran, "task dispatched after unpark");
    assert_eq!(counter.load(Ordering::Relaxed), 1, "body ran once");
    assert_eq!(sched.state_of(id), TaskState::Exited, "task exited");
}

/// The per-CPU load observations are truthful: `queue_depth` reflects
/// the tasks queued runnable on the CPU (a sample, excluding the running
/// task), `cpu_switches` counts one context switch per dispatched body,
/// and both fail closed on an out-of-range CPU.
fn load_observations_track_dispatch<S: SchedulerPolicy<TestArch>>() {
    let (arch, sched) = make::<S>(1, 64);
    arch.set_current_cpu(0);
    assert_eq!(sched.queue_depth(0), Ok(0), "empty queue at rest");
    assert_eq!(sched.has_ready_work(0), Ok(false), "no idle work at rest");
    assert_eq!(sched.cpu_switches(0), Ok(0), "no dispatches at rest");

    for _ in 0..2 {
        sched
            .spawn(0, Priority::Normal, |_| TaskAction::Exit)
            .expect("spawn");
    }
    assert_eq!(sched.queue_depth(0), Ok(2), "both tasks queued runnable");
    assert_eq!(
        sched.has_ready_work(0),
        Ok(true),
        "queued tasks prevent idle"
    );

    assert!(matches!(sched.step(0), Ok(StepOutcome::Ran(_))));
    assert_eq!(sched.queue_depth(0), Ok(1), "one task consumed");
    assert_eq!(sched.cpu_switches(0), Ok(1), "one switch per dispatch");

    assert!(matches!(sched.step(0), Ok(StepOutcome::Ran(_))));
    assert_eq!(sched.queue_depth(0), Ok(0), "queue drained");
    assert_eq!(sched.has_ready_work(0), Ok(false), "drained CPU may idle");
    assert_eq!(sched.cpu_switches(0), Ok(2), "switches are cumulative");

    // Out-of-range CPUs fail closed, never a fabricated figure.
    assert_eq!(sched.queue_depth(99), Err(SchedError::NoSuchCpu));
    assert_eq!(sched.has_ready_work(99), Err(SchedError::NoSuchCpu));
    assert_eq!(sched.cpu_switches(99), Err(SchedError::NoSuchCpu));
}

/// `park` then `unpark` re-admits the task for dispatch (block/wake).
fn block_wake_roundtrip<S: SchedulerPolicy<TestArch>>() {
    let (_arch, sched) = make::<S>(1, 64);
    let id = sched
        .spawn(0, Priority::Normal, |_| TaskAction::Park)
        .expect("spawn");
    assert_eq!(sched.step(0), Ok(StepOutcome::Ran(id)), "body ran");
    assert_eq!(sched.state_of(id), TaskState::Parked, "body parked it");
    sched.unpark(id).expect("unpark a parked task");
    assert_eq!(sched.step(0), Ok(StepOutcome::Ran(id)), "re-dispatched");
}

/// A wake delivered *before* a task commits to park is not lost
/// (no lost wake-ups). This is the park/unpark race a
/// blocking syscall hits: the caller checks its condition, the event then
/// fires (an `unpark` against the still-running caller), and only *then*
/// does the caller try to park. The wake must cancel that park rather than
/// sleep through it. Every policy must close this race via the wake-pending
/// token, so the property is asserted here, for both policies, through the
/// public surface only.
fn unpark_before_park_is_not_lost<S: SchedulerPolicy<TestArch>>() {
    let (_arch, sched) = make::<S>(1, 64);
    // The body always asks to park; whether it actually sleeps depends on
    // the token, which is what this test exercises.
    let id = sched
        .spawn(0, Priority::Normal, |_| TaskAction::Park)
        .expect("spawn");
    // Deliver the wake while the task has not yet parked (it is `Ready`,
    // never dispatched). `unpark` cannot move a non-parked task, so it must
    // record a pending token rather than no-op the wake away.
    sched
        .unpark(id)
        .expect("unpark a not-yet-parked task records a token");
    assert_eq!(sched.step(0), Ok(StepOutcome::Ran(id)), "body ran");
    assert_eq!(
        sched.state_of(id),
        TaskState::Ready,
        "the early wake cancelled the park; the task stays runnable"
    );
    // With the token consumed, a genuine later park is honoured normally —
    // the token must not suppress every future park.
    assert_eq!(
        sched.step(0),
        Ok(StepOutcome::Ran(id)),
        "re-dispatched after the cancelled park"
    );
    assert_eq!(
        sched.state_of(id),
        TaskState::Parked,
        "with no pending token, the next park takes effect"
    );
}

/// A cross-core IPC round-trip wakes the parked caller **promptly**: the
/// reply delivers an inter-processor interrupt to the exact core the woken
/// caller lands on, so that core reschedules at once instead of waiting for
/// its next timer tick.
///
/// This is the multicore IPC-latency property. A blocking `ipc_call` parks
/// the caller off the run queue awaiting its reply; the server, running on
/// a different core, issues `ipc_reply`, which wakes the caller via
/// `scheduler.unpark(caller)`. If that `unpark` did not kick the caller's
/// core, the reply would sit unseen until an unrelated event happened to
/// reschedule that core (a timer tick, another wake) — an unduly long,
/// load-dependent inter-process round-trip latency on an otherwise idle
/// core. Every policy must deliver the wake IPI, and to exactly one core
/// (wake-one, never a broadcast that would disturb unrelated cores), so the
/// property is asserted here for all of them on an SMP topology, through the
/// public trait surface only. The suite is deterministic (it counts IPIs
/// rather than timing a wall clock), so this can never become a flaky
/// load-dependent test.
fn cross_cpu_ipc_reply_wakes_the_caller_without_delay<S: SchedulerPolicy<TestArch>>() {
    // Four cores stand in for processes that may be pinned to different
    // cores: the caller (client) blocks awaiting its reply; the server, on
    // another core, issues the reply-wake.
    let (arch, sched) = make::<S>(4, 64);
    arch.set_current_cpu(0);

    // The caller: a task that parks, exactly as a thread blocked in
    // `ipc_call` awaiting the server's reply. Dispatch it once, wherever it
    // was placed, so it commits to `Parked`.
    let caller = sched
        .spawn(1, Priority::Normal, |_| TaskAction::Park)
        .expect("spawn caller");
    let mut ran_on = None;
    for cpu in 0..sched.cpu_count() {
        if sched.step(cpu) == Ok(StepOutcome::Ran(caller)) {
            ran_on = Some(cpu);
            break;
        }
    }
    assert!(
        ran_on.is_some(),
        "the caller ran once and blocked awaiting its reply"
    );
    assert_eq!(
        sched.state_of(caller),
        TaskState::Parked,
        "the blocked caller is off the run queue awaiting the reply"
    );

    // Snapshot per-core IPI counts, then model the server's `ipc_reply`:
    // wake the parked caller from another core. Everything after this point
    // is attributable to the reply-wake alone.
    let before: Vec<u64> = (0..sched.cpu_count()).map(|c| arch.ipi_count(c)).collect();
    let strays_before = arch.stray_ipi_count();
    sched
        .unpark(caller)
        .expect("the reply wakes the parked caller");

    // Exactly one core — the one the woken caller was placed on — receives
    // exactly one IPI; every other core is left undisturbed, and no IPI is
    // ever sent to an out-of-range core.
    let kicked: Vec<u32> = (0..sched.cpu_count())
        .filter(|&c| arch.ipi_count(c) == before[c as usize] + 1)
        .collect();
    let undisturbed = (0..sched.cpu_count())
        .filter(|&c| arch.ipi_count(c) == before[c as usize])
        .count();
    assert_eq!(
        kicked.len(),
        1,
        "the reply kicks exactly the caller's core so it reschedules at once"
    );
    assert_eq!(
        undisturbed,
        (sched.cpu_count() - 1) as usize,
        "no unrelated core is disturbed (wake-one, never a broadcast)"
    );
    assert_eq!(
        arch.stray_ipi_count(),
        strays_before,
        "the wake never targets an out-of-range core"
    );

    // And the kicked core dispatches the woken caller immediately — the
    // reply is picked up now, not deferred to some later tick.
    let core = kicked[0];
    assert_eq!(
        sched.step(core),
        Ok(StepOutcome::Ran(caller)),
        "the kicked core runs the woken caller at once"
    );
}

/// The ticks that elapse while a task's body runs accumulate on that task
/// (and only that task), observable through `cpu_ticks_of`; an unknown id
/// is a typed refusal, never a fabricated zero.
fn cpu_time_is_accounted_per_dispatch<S: SchedulerPolicy<TestArch>>() {
    let (arch, sched) = make::<S>(1, 64);
    arch.set_current_cpu(0);
    let busy_arch = arch.clone();
    let busy = sched
        .spawn(0, Priority::Normal, move |_| {
            // Model 5 ticks of on-CPU work per dispatch.
            busy_arch.advance_ticks(5);
            TaskAction::Yield
        })
        .expect("spawn busy");
    let idle = sched
        .spawn(0, Priority::Normal, |_| TaskAction::Yield)
        .expect("spawn idle");
    assert_eq!(sched.cpu_ticks_of(busy), Ok(0), "no run, no time");
    let mut dispatched = 0;
    while dispatched < 8 {
        if matches!(sched.step(0), Ok(StepOutcome::Ran(_))) {
            dispatched += 1;
        }
    }
    let busy_ticks = sched.cpu_ticks_of(busy).expect("busy is live");
    let idle_ticks = sched.cpu_ticks_of(idle).expect("idle is live");
    assert!(busy_ticks >= 5, "the busy task accrued its work");
    assert!(
        busy_ticks > idle_ticks,
        "time accrues to the task that spent it ({busy_ticks} vs {idle_ticks})"
    );
    assert!(
        sched.cpu_ticks_of(u64::MAX).is_err(),
        "an unknown id is refused, not reported as zero"
    );
    // The same dispatch bracket feeds the per-CPU busy total: everything
    // the tasks accrued on CPU 0 is in CPU 0's total, and an out-of-range
    // CPU is a typed refusal, never a fabricated zero.
    let cpu_busy = sched.cpu_busy_ticks(0).expect("cpu 0 exists");
    assert_eq!(
        cpu_busy,
        busy_ticks + idle_ticks,
        "the CPU total is the sum of the work dispatched on it"
    );
    assert!(
        sched.cpu_busy_ticks(u32::MAX).is_err(),
        "an out-of-range CPU is refused, not reported as zero"
    );
}

/// Lifecycle entry points report the documented typed errors and are
/// idempotent where promised.
fn lifecycle_error_codes<S: SchedulerPolicy<TestArch>>() {
    let (_arch, sched) = make::<S>(1, 64);
    assert_eq!(
        sched.park(999),
        Err(crate::SchedError::NoSuchTask),
        "park unknown"
    );
    assert_eq!(
        sched.unpark(999),
        Err(crate::SchedError::NoSuchTask),
        "unpark unknown"
    );
    assert_eq!(
        sched.exit(999),
        Err(crate::SchedError::NoSuchTask),
        "exit unknown"
    );
    let id = sched
        .spawn(0, Priority::Normal, |_| TaskAction::Yield)
        .expect("spawn");
    // A never-dispatched (Ready) task is not executing on any CPU, so the
    // first `exit` quiesces it and the caller owns teardown; a repeat owes
    // nothing (reclaim happens exactly once).
    assert_eq!(
        sched.exit(id),
        Ok(ExitDisposition::Quiesced),
        "first exit of a non-executing task quiesces it"
    );
    assert_eq!(
        sched.exit(id),
        Ok(ExitDisposition::AlreadyExited),
        "a repeat exit owes no teardown (idempotent)"
    );
    assert_eq!(sched.state_of(id), TaskState::Exited);
    assert_eq!(
        sched.on_timer_tick(7),
        Err(crate::SchedError::NoSuchCpu),
        "tick rejects unknown cpu"
    );
}

/// `yield_current` requeues a running task and rejects bad transitions.
fn yield_current_semantics<S: SchedulerPolicy<TestArch>>() {
    let (_arch, sched) = make::<S>(1, 64);
    assert_eq!(
        sched.yield_current(999),
        Err(crate::SchedError::NoSuchTask),
        "yield unknown task"
    );
    // A freshly spawned, not-yet-running task is `Ready`, not `Running`,
    // so a syscall-style `yield_current` against it must be rejected.
    let id = sched
        .spawn(0, Priority::Normal, |_| TaskAction::Yield)
        .expect("spawn");
    assert_eq!(
        sched.yield_current(id),
        Err(crate::SchedError::InvalidState),
        "yield a non-running task"
    );
}

/// [`SchedulerPolicy::running_cpu`] never fabricates a CPU: a task that is
/// not currently dispatching reports `None`, and whenever a CPU reports a
/// current task, `running_cpu` points back to exactly that CPU.
///
/// This pins the read-only contract the System Information introspection
/// feed relies on (a truthful current-CPU, never a stale one) through the
/// public trait surface, for every policy.
fn running_cpu_agrees_with_current_task<S: SchedulerPolicy<TestArch>>() {
    let (arch, sched) = make::<S>(2, 64);
    // A freshly-spawned, not-yet-dispatched task is Ready, so it is on no
    // CPU; an id no task ever held is likewise on no CPU.
    let id = sched
        .spawn(0, Priority::Normal, |_| TaskAction::Yield)
        .expect("spawn");
    assert_eq!(
        sched.running_cpu(id),
        None,
        "a Ready (not-running) task is on no CPU"
    );
    assert_eq!(sched.running_cpu(999), None, "an unknown task is on no CPU");
    // The agreement invariant: at every observation point, if a CPU reports
    // a current task then `running_cpu` of that task is that same CPU.
    arch.set_current_cpu(0);
    for _ in 0..4 {
        let _ = sched.step(0).expect("step");
        for cpu in 0..sched.cpu_count() {
            if let Some(current) = sched.current_task(cpu) {
                assert_eq!(
                    sched.running_cpu(current),
                    Some(cpu),
                    "running_cpu must point back at the CPU running the task"
                );
            }
        }
        arch.advance_ticks(1);
    }
}

/// A Low-priority task that keeps yielding still runs to completion within
/// a bounded number of steps — the periodic priority boost prevents
/// starvation. Uses only the public surface.
fn no_starvation_under_priority_boost<S: SchedulerPolicy<TestArch>>() {
    let (arch, sched) = make::<S>(1, 64);
    let runs = Arc::new(AtomicU64::new(0));
    let r2 = runs.clone();
    let id = sched
        .spawn(0, Priority::Low, move |_| {
            if r2.fetch_add(1, Ordering::Relaxed) >= 4 {
                TaskAction::Exit
            } else {
                TaskAction::Yield
            }
        })
        .expect("spawn");
    arch.set_current_cpu(0);
    let mut steps = 0u64;
    while sched.live_task_count() > 0 && steps < 1000 {
        let _ = sched.step(0).expect("step");
        arch.advance_ticks(1);
        steps += 1;
    }
    assert_eq!(sched.state_of(id), TaskState::Exited, "low task completed");
    assert_eq!(runs.load(Ordering::Relaxed), 5, "ran the expected times");
    assert!(steps < 1000, "completed within the bound (no starvation)");
}

/// With one High-band and one Low-band perpetual yielder on a single CPU,
/// neither band is starved over a fixed step budget: both accumulate runs.
fn fairness_no_band_is_starved<S: SchedulerPolicy<TestArch>>() {
    const BUDGET: u64 = 200;
    let (arch, sched) = make::<S>(1, 64);
    let high_runs = Arc::new(AtomicU64::new(0));
    let low_runs = Arc::new(AtomicU64::new(0));
    let hr = high_runs.clone();
    let lr = low_runs.clone();
    sched
        .spawn(0, Priority::High, move |_| {
            hr.fetch_add(1, Ordering::Relaxed);
            TaskAction::Yield
        })
        .expect("spawn high");
    sched
        .spawn(0, Priority::Low, move |_| {
            lr.fetch_add(1, Ordering::Relaxed);
            TaskAction::Yield
        })
        .expect("spawn low");
    arch.set_current_cpu(0);
    for _ in 0..BUDGET {
        let _ = sched.step(0).expect("step");
        arch.advance_ticks(1);
    }
    assert!(high_runs.load(Ordering::Relaxed) > 0, "high-band task ran");
    assert!(
        low_runs.load(Ordering::Relaxed) > 0,
        "low-band task is not starved (priority boost gives it turns)"
    );
}

/// A [`SchedClass::Realtime`] task is dispatched ahead of a CPU whose fair
/// run queue is already full of ready time-shared tasks, regardless of its
/// virtual runtime — the strict-priority guarantee. This is the property an
/// IRQ-woken driver relies on: it must run *now*, before any CPU-bound
/// workload, so it can service its device before the hardware ring it polls
/// drains. Modelled on the single-CPU worst case (a queue packed with fair
/// competitors) through the public surface only.
fn realtime_is_dispatched_ahead_of_a_full_time_shared_queue<S: SchedulerPolicy<TestArch>>() {
    let (arch, sched) = make::<S>(1, 64);
    arch.set_current_cpu(0);

    // Pack CPU 0 with perpetually-runnable fair tasks and let them cycle so
    // they accrue virtual runtime and settle into the ready set — the loaded
    // machine an interrupt arrives on.
    for _ in 0..5 {
        sched
            .spawn(0, Priority::Normal, |_| TaskAction::Yield)
            .expect("spawn fair hog");
    }
    for _ in 0..10 {
        let _ = sched.step(0).expect("step");
        arch.advance_ticks(1);
    }

    // A real-time task arrives exactly as an IRQ-woken driver does: admitted
    // parked, classed realtime, then unparked (the wake). Its vruntime is at
    // the front, but strict priority — not vruntime — must decide the pick.
    let rt = sched
        .spawn_parked(0, Priority::Normal, |_| TaskAction::Park)
        .expect("spawn_parked rt");
    sched
        .set_sched_class(rt, SchedClass::Realtime)
        .expect("class it realtime");
    assert_eq!(
        sched.sched_class(rt),
        Ok(SchedClass::Realtime),
        "the class change is observable"
    );
    sched.unpark(rt).expect("wake the realtime task");

    // The very next step dispatches the realtime task, ahead of every ready
    // fair task on the CPU — never one fair pick first.
    assert_eq!(
        sched.step(0),
        Ok(StepOutcome::Ran(rt)),
        "the realtime task preempts a full fair queue on the next pick"
    );
}

/// While a [`SchedClass::Realtime`] task stays runnable it is dispatched on
/// every pick ahead of ready time-shared tasks (never preempted by fair
/// work); once it voluntarily blocks (`Park`), the CPU is released to the
/// time-shared band. This pins both halves of the strict-priority contract
/// — fair work cannot steal the CPU from a runnable realtime task, and a
/// realtime task that blocks does not wedge the fair band.
fn realtime_is_not_preempted_by_time_shared_then_releases_on_park<S: SchedulerPolicy<TestArch>>() {
    // A realtime task that runs this many quanta then blocks.
    const RT_QUANTA: u64 = 5;

    let (arch, sched) = make::<S>(1, 64);
    arch.set_current_cpu(0);

    let fair_runs = Arc::new(AtomicU64::new(0));
    for _ in 0..3 {
        let fr = fair_runs.clone();
        sched
            .spawn(0, Priority::Normal, move |_| {
                fr.fetch_add(1, Ordering::Relaxed);
                TaskAction::Yield
            })
            .expect("spawn fair");
    }

    // Admitted parked, classed realtime, then woken — the interrupt-driven-
    // driver shape, and the path the class contract guarantees for every
    // policy.
    let rt_runs = Arc::new(AtomicU64::new(0));
    let rr = rt_runs.clone();
    let rt = sched
        .spawn_parked(0, Priority::Normal, move |_| {
            if rr.fetch_add(1, Ordering::Relaxed) + 1 >= RT_QUANTA {
                TaskAction::Park
            } else {
                TaskAction::Yield
            }
        })
        .expect("spawn_parked rt");
    sched
        .set_sched_class(rt, SchedClass::Realtime)
        .expect("class it realtime");
    sched.unpark(rt).expect("wake the realtime task");

    // Every step until the realtime task blocks dispatches the realtime task
    // and only it: no fair task runs while a realtime task is runnable.
    for _ in 0..RT_QUANTA {
        assert_eq!(
            sched.step(0),
            Ok(StepOutcome::Ran(rt)),
            "the realtime task holds the CPU against fair work"
        );
        assert_eq!(
            fair_runs.load(Ordering::Relaxed),
            0,
            "no time-shared task runs while the realtime task is runnable"
        );
    }
    assert_eq!(sched.state_of(rt), TaskState::Parked, "it blocked");
    assert_eq!(rt_runs.load(Ordering::Relaxed), RT_QUANTA);

    // With the realtime task blocked, the fair band is released and runs.
    let _ = sched.step(0).expect("step");
    assert!(
        fair_runs.load(Ordering::Relaxed) > 0,
        "a blocked realtime task does not wedge the time-shared band"
    );
}

/// Two runnable [`SchedClass::Realtime`] peers on one CPU share it
/// round-robin (FIFO re-enqueue on yield), so neither realtime task starves
/// the other. Strict priority is *between* classes; within the realtime
/// class the peers alternate.
fn realtime_peers_share_the_cpu_round_robin<S: SchedulerPolicy<TestArch>>() {
    let (arch, sched) = make::<S>(1, 64);
    arch.set_current_cpu(0);

    let a_runs = Arc::new(AtomicU64::new(0));
    let b_runs = Arc::new(AtomicU64::new(0));
    let ar = a_runs.clone();
    let br = b_runs.clone();
    let a = sched
        .spawn_parked(0, Priority::Normal, move |_| {
            ar.fetch_add(1, Ordering::Relaxed);
            TaskAction::Yield
        })
        .expect("spawn_parked rt a");
    let b = sched
        .spawn_parked(0, Priority::Normal, move |_| {
            br.fetch_add(1, Ordering::Relaxed);
            TaskAction::Yield
        })
        .expect("spawn_parked rt b");
    sched
        .set_sched_class(a, SchedClass::Realtime)
        .expect("a realtime");
    sched
        .set_sched_class(b, SchedClass::Realtime)
        .expect("b realtime");
    sched.unpark(a).expect("wake a");
    sched.unpark(b).expect("wake b");

    for _ in 0..20 {
        let _ = sched.step(0).expect("step");
        arch.advance_ticks(1);
    }
    let a_final = a_runs.load(Ordering::Relaxed);
    let b_final = b_runs.load(Ordering::Relaxed);
    assert!(a_final > 0 && b_final > 0, "both realtime peers ran");
    let (hi, lo) = (a_final.max(b_final), a_final.min(b_final));
    assert!(
        hi - lo <= 1,
        "realtime peers alternate evenly (round-robin): {a_final} vs {b_final}"
    );
}

/// `sched_class` reports the current class and fails closed on an unknown
/// id; `set_sched_class` likewise refuses an unknown id rather than
/// fabricating a task.
fn sched_class_reports_and_fails_closed_on_unknown<S: SchedulerPolicy<TestArch>>() {
    let (_arch, sched) = make::<S>(1, 64);
    assert_eq!(
        sched.sched_class(999),
        Err(SchedError::NoSuchTask),
        "class of an unknown id is refused"
    );
    assert_eq!(
        sched.set_sched_class(999, SchedClass::Realtime),
        Err(SchedError::NoSuchTask),
        "classing an unknown id is refused"
    );
    let id = sched
        .spawn(0, Priority::Normal, |_| TaskAction::Yield)
        .expect("spawn");
    assert_eq!(
        sched.sched_class(id),
        Ok(SchedClass::TimeShared),
        "a task is born time-shared"
    );
    // Idempotent: re-setting the class a task already holds is a no-op.
    sched
        .set_sched_class(id, SchedClass::TimeShared)
        .expect("idempotent no-op");
    assert_eq!(sched.sched_class(id), Ok(SchedClass::TimeShared));
}

/// `priority` reports the admitted level, `set_priority` records a new one
/// observably and idempotently, and both fail closed on an unknown id; a
/// terminal task refuses a change rather than applying it.
fn priority_reports_changes_and_fails_closed<S: SchedulerPolicy<TestArch>>() {
    let (arch, sched) = make::<S>(1, 64);
    assert_eq!(
        sched.priority(999),
        Err(SchedError::NoSuchTask),
        "priority of an unknown id is refused"
    );
    assert_eq!(
        sched.set_priority(999, Priority::Low),
        Err(SchedError::NoSuchTask),
        "re-prioritising an unknown id is refused"
    );
    let id = sched
        .spawn_parked(0, Priority::Normal, |_ctx| TaskAction::Exit)
        .expect("spawn");
    assert_eq!(
        sched.priority(id),
        Ok(Priority::Normal),
        "a task reports the priority it was admitted with"
    );
    sched
        .set_priority(id, Priority::Low)
        .expect("lower the task");
    assert_eq!(
        sched.priority(id),
        Ok(Priority::Low),
        "the priority change is observable before the task ever runs"
    );
    // Idempotent: re-setting the priority a task already holds is a no-op.
    sched
        .set_priority(id, Priority::Low)
        .expect("idempotent no-op");
    assert_eq!(sched.priority(id), Ok(Priority::Low));
    // A lowered task still runs to completion — the change is a service
    // level, never a suspension.
    sched.unpark(id).expect("wake the lowered task");
    arch.set_current_cpu(0);
    assert_eq!(sched.step(0), Ok(StepOutcome::Ran(id)), "lowered task ran");
    assert_eq!(sched.state_of(id), TaskState::Exited, "task exited");
    // A terminal task refuses the change; whether the record is still
    // retained (InvalidState) or already drained (NoSuchTask) is the
    // policy's own reclamation timing, but it never applies.
    assert!(
        matches!(
            sched.set_priority(id, Priority::High),
            Err(SchedError::InvalidState | SchedError::NoSuchTask)
        ),
        "a terminal task cannot be re-prioritised"
    );
}

/// Deadlock-free, lossless, bounded-latency dispatch of a large task
/// population across ≥ 4 simulated cores.
#[allow(clippy::cast_possible_truncation)]
fn smp_stress_four_cores<S: SchedulerPolicy<TestArch>>() {
    const TASKS: u64 = 10_000;
    const CPUS: u32 = 4;
    const MAX_FIRST_RUN_LATENCY_STEPS: u64 = 10 * TASKS / CPUS as u64;

    let arch = Arc::new(TestArch::new(CPUS).expect("arch"));
    let cfg = SchedulerConfig {
        cpus: CPUS,
        queue_capacity_per_band: 4096,
        yields_before_demotion: 4,
        boost_interval_ticks: 256,
    };
    let sched = S::new(cfg, arch.clone()).expect("sched");

    let executions = Arc::new(AtomicU64::new(0));
    let global_step = Arc::new(AtomicU64::new(0));
    let spawn_step: Arc<Vec<AtomicU64>> =
        Arc::new((0..TASKS).map(|_| AtomicU64::new(u64::MAX)).collect());
    let first_run_step: Arc<Vec<AtomicU64>> =
        Arc::new((0..TASKS).map(|_| AtomicU64::new(u64::MAX)).collect());

    for i in 0..TASKS {
        let exec = executions.clone();
        let gstep = global_step.clone();
        let first = first_run_step.clone();
        let idx = i as usize;
        spawn_step[idx].store(global_step.load(Ordering::Relaxed), Ordering::Relaxed);
        let yielded = AtomicU64::new(0);
        sched
            .spawn(
                (i % u64::from(CPUS)) as u32,
                Priority::Normal,
                move |_ctx| {
                    exec.fetch_add(1, Ordering::Relaxed);
                    let _ = first[idx].compare_exchange(
                        u64::MAX,
                        gstep.load(Ordering::Relaxed),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                    if yielded.fetch_add(1, Ordering::Relaxed) == 0 {
                        TaskAction::Yield
                    } else {
                        TaskAction::Exit
                    }
                },
            )
            .expect("spawn");
    }

    let max_rounds: u64 = TASKS * 8;
    let mut rounds = 0u64;
    while sched.live_task_count() > 0 && rounds < max_rounds {
        for cpu in 0..CPUS {
            arch.set_current_cpu(cpu);
            let _ = sched.step(cpu).expect("step");
        }
        arch.advance_ticks(1);
        global_step.fetch_add(1, Ordering::Relaxed);
        rounds += 1;
    }
    assert_eq!(
        sched.live_task_count(),
        0,
        "all tasks completed (no deadlock)"
    );
    assert_eq!(
        executions.load(Ordering::Relaxed),
        TASKS * 2,
        "each task ran exactly twice (yield + exit)"
    );

    let mut worst = 0u64;
    for i in 0..TASKS {
        let first = first_run_step[i as usize].load(Ordering::Relaxed);
        let spawn = spawn_step[i as usize].load(Ordering::Relaxed);
        assert!(first != u64::MAX, "task {i} never ran");
        worst = worst.max(first.saturating_sub(spawn));
    }
    assert!(
        worst <= MAX_FIRST_RUN_LATENCY_STEPS,
        "worst-case first-run latency {worst} exceeds bound {MAX_FIRST_RUN_LATENCY_STEPS}"
    );
}

/// On an asymmetric machine (performance + efficiency cores) a mixed
/// population of interactive and background tasks must still be dispatched
/// to completion with no loss and no deadlock.
///
/// Heterogeneous placement steers `Low` work onto efficiency cores and
/// interactive work onto performance cores ([`crate::CoreClass`]); this
/// asserts the universal property every policy must keep regardless of how
/// it places work — the steering must never strand a task on a class with
/// no runnable CPU, double-count it, or wedge the dispatch loop. Exact
/// placement is policy-internal (and not publicly observable under
/// work-stealing), so each policy's own unit tests cover *where* tasks
/// land; this guards *liveness* across both.
fn heterogeneous_topology_preserves_liveness<S: SchedulerPolicy<TestArch>>() {
    const TASKS: u64 = 400;
    const CPUS: u32 = 4;

    let arch = Arc::new(TestArch::new(CPUS).expect("arch"));
    // CPUs 0/1 performance, CPUs 2/3 efficiency.
    arch.set_core_class(2, CoreClass::Efficiency);
    arch.set_core_class(3, CoreClass::Efficiency);
    let cfg = SchedulerConfig {
        cpus: CPUS,
        queue_capacity_per_band: 1024,
        yields_before_demotion: 2,
        boost_interval_ticks: 32,
    };
    let sched = S::new(cfg, arch.clone()).expect("sched");

    let executions = Arc::new(AtomicU64::new(0));
    for i in 0..TASKS {
        let exec = executions.clone();
        // Alternate background (Low) and interactive (High) work, spread
        // across every CPU as the spawn hint.
        let prio = if i % 2 == 0 {
            Priority::Low
        } else {
            Priority::High
        };
        let yielded = AtomicU64::new(0);
        // A modulus by the `u32` CPU count is always a real CPU index.
        #[allow(clippy::cast_possible_truncation)]
        let home = (i % u64::from(CPUS)) as u32;
        sched
            .spawn(home, prio, move |_ctx| {
                exec.fetch_add(1, Ordering::Relaxed);
                // Yield once (forcing a re-placement decision), then exit.
                if yielded.fetch_add(1, Ordering::Relaxed) == 0 {
                    TaskAction::Yield
                } else {
                    TaskAction::Exit
                }
            })
            .expect("spawn");
    }

    let max_rounds: u64 = TASKS * 16;
    let mut rounds = 0u64;
    while sched.live_task_count() > 0 && rounds < max_rounds {
        for cpu in 0..CPUS {
            arch.set_current_cpu(cpu);
            let _ = sched.step(cpu).expect("step");
        }
        arch.advance_ticks(1);
        rounds += 1;
    }
    assert_eq!(
        sched.live_task_count(),
        0,
        "every task completes on a heterogeneous machine (no task stranded)"
    );
    assert_eq!(
        executions.load(Ordering::Relaxed),
        TASKS * 2,
        "each task ran exactly twice (yield + exit) — no loss, no duplication"
    );
}
