// Cast bounds in this test are compile-time constants whose values are
// far below the truncation thresholds clippy warns about. Allowing the
// casts here keeps the test readable (`AGENTS.md` §15 rule 10).
#![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]

//! Workspace-level scheduler stress test (Stage 2 deliverable).
//!
//! See `lib.rs` for the design rationale. The test drives 20 000 tasks
//! across 4 simulated cores, asserts deadlock-freedom, exact execution
//! count, and bounded first-run latency.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use rustos_kernel_sched_mlfq::{Priority, Scheduler, SchedulerConfig, TaskAction, TestArch};
use rustos_test_scheduler_stress::{DEFAULT_CPUS, DEFAULT_TASKS};

#[test]
fn workspace_stress_four_cores_twenty_thousand_tasks() {
    let tasks: u64 = DEFAULT_TASKS;
    let cpus: u32 = DEFAULT_CPUS;
    assert!(cpus >= 4, "Stage-2 deliverable mandates ≥ 4 cores");

    let arch = Arc::new(TestArch::new(cpus).expect("arch"));
    let cfg = SchedulerConfig {
        cpus,
        queue_capacity_per_band: 8192,
        yields_before_demotion: 4,
        boost_interval_ticks: 256,
    };
    let sched = Arc::new(Scheduler::new(cfg, arch.clone()).expect("sched"));

    let executions = Arc::new(AtomicU64::new(0));
    let global_step = Arc::new(AtomicU64::new(0));
    let spawn_step: Arc<Vec<AtomicU64>> =
        Arc::new((0..tasks).map(|_| AtomicU64::new(u64::MAX)).collect());
    let first_run_step: Arc<Vec<AtomicU64>> =
        Arc::new((0..tasks).map(|_| AtomicU64::new(u64::MAX)).collect());

    for i in 0..tasks {
        let exec = executions.clone();
        let gstep = global_step.clone();
        let first = first_run_step.clone();
        let idx = i as usize;
        spawn_step[idx].store(global_step.load(Ordering::Relaxed), Ordering::Relaxed);
        let yielded = AtomicU64::new(0);
        sched
            .spawn(
                (i % u64::from(cpus)) as u32,
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

    // Bounded round count → deadlock-freedom assertion. If the scheduler
    // wedges, this loop exits before `live_task_count()` reaches zero
    // and the assertion below fires.
    let max_rounds: u64 = tasks * 8;
    let mut rounds = 0u64;
    while sched.live_task_count() > 0 && rounds < max_rounds {
        for cpu in 0..cpus {
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
        "deadlock: {} tasks remained after {rounds} rounds",
        sched.live_task_count()
    );
    assert_eq!(
        executions.load(Ordering::Relaxed),
        tasks * 2,
        "each task ran exactly twice (yield + exit)"
    );

    // Bounded first-run latency: a task spawned on CPU `c` must run
    // within `10 * tasks / cpus` global steps. The bound is the same
    // one the kernel/sched stress test uses; scaling it with `tasks`
    // keeps the assertion meaningful if a future tweak grows the load.
    let max_first_run_latency: u64 = 10 * tasks / u64::from(cpus);
    let mut worst = 0u64;
    for i in 0..tasks {
        let first = first_run_step[i as usize].load(Ordering::Relaxed);
        let spawn = spawn_step[i as usize].load(Ordering::Relaxed);
        assert!(first != u64::MAX, "task {i} never ran");
        worst = worst.max(first.saturating_sub(spawn));
    }
    assert!(
        worst <= max_first_run_latency,
        "worst-case first-run latency {worst} exceeded bound {max_first_run_latency}"
    );
}
