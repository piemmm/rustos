// Cast bounds in this test are all compile-time constants well below
// the truncation thresholds clippy warns about (TASKS == 10_000,
// CPUS == 4). Allowing the casts here keeps the test readable
// (`AGENTS.md` §15 rule 10 — justified allow).
#![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]

//! Stress test for the scheduler.
//!
//! 10 000 tasks across 4 simulated cores. Asserts:
//!
//! 1. No deadlock — the scheduler always either dispatches work or
//!    returns `Idle`; we drive it to completion in a bounded number of
//!    rounds.
//! 2. No task is lost or run twice (counter equality).
//! 3. Bounded latency — every task's first run occurs within
//!    `MAX_FIRST_RUN_LATENCY_STEPS` global scheduler steps of its
//!    spawn time.
//!
//! `AGENTS.md` §7 — full coverage of the stress requirement listed in
//! the Stage 2.3 deliverables.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use rustos_kernel_sched::{Priority, Scheduler, SchedulerConfig, TaskAction, TestArch};

const TASKS: u64 = 10_000;
const CPUS: u32 = 4;
const MAX_FIRST_RUN_LATENCY_STEPS: u64 = 10 * TASKS / CPUS as u64;

#[test]
fn ten_thousand_tasks_across_four_cores() {
    let arch = Arc::new(TestArch::new(CPUS).expect("arch"));
    let cfg = SchedulerConfig {
        cpus: CPUS,
        // Power-of-two ≥ TASKS / CPUS so each home queue alone can hold its
        // entire local share; work-stealing still kicks in for skew.
        queue_capacity_per_band: 4096,
        yields_before_demotion: 4,
        boost_interval_ticks: 256,
    };
    let sched = Arc::new(Scheduler::new(cfg, arch.clone()).expect("sched"));

    let executions = Arc::new(AtomicU64::new(0));
    // The global "logical tick" at the time of each task's spawn — used to
    // compute first-run latency.
    let global_step = Arc::new(AtomicU64::new(0));
    let spawn_step: Arc<Vec<AtomicU64>> =
        Arc::new((0..TASKS).map(|_| AtomicU64::new(u64::MAX)).collect());
    let first_run_step: Arc<Vec<AtomicU64>> =
        Arc::new((0..TASKS).map(|_| AtomicU64::new(u64::MAX)).collect());

    // Spawn tasks evenly across the four cores. Each task yields once
    // (to exercise re-enqueue) and then exits.
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

    // Drive the scheduler. Round-robin across cores; bail once every
    // task has exited. Bounded round count guarantees test termination
    // (i.e. no deadlock).
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

    // Bounded first-run latency.
    let mut worst = 0u64;
    for i in 0..TASKS {
        let first = first_run_step[i as usize].load(Ordering::Relaxed);
        let spawn = spawn_step[i as usize].load(Ordering::Relaxed);
        assert!(first != u64::MAX, "task {i} never ran");
        let latency = first.saturating_sub(spawn);
        worst = worst.max(latency);
    }
    assert!(
        worst <= MAX_FIRST_RUN_LATENCY_STEPS,
        "worst-case first-run latency {worst} exceeds bound {MAX_FIRST_RUN_LATENCY_STEPS}"
    );
}
