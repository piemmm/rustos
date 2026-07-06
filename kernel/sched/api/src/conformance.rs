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
use crate::outcome::StepOutcome;
use crate::policy::SchedulerPolicy;
use crate::task::{Priority, TaskAction, TaskState};

/// Run the entire conformance suite against scheduler policy `S`.
///
/// # Panics
///
/// Panics (failing the test) if any required property does not hold.
pub fn run_all<S: SchedulerPolicy<TestArch>>() {
    spawn_runs_and_exits::<S>();
    block_wake_roundtrip::<S>();
    unpark_before_park_is_not_lost::<S>();
    lifecycle_error_codes::<S>();
    yield_current_semantics::<S>();
    running_cpu_agrees_with_current_task::<S>();
    cpu_time_is_accounted_per_dispatch::<S>();
    no_starvation_under_priority_boost::<S>();
    fairness_no_band_is_starved::<S>();
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
    sched.exit(id).expect("first exit");
    sched.exit(id).expect("exit is idempotent");
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
