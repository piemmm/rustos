// The integration tests construct fixed-size four-element arrays and
// index them with `cpu as usize` / `i as u32`; clippy's pedantic
// truncation lints fire on those even though the casts are bounded
// at compile time (`AGENTS.md` §15 rule 10 — justified allow).
#![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]

//! Cross-crate integration tests for `kernel/sched`.
//!
//! These tests drive the scheduler through its public API with a
//! [`TestArch`] backing. They cover the Stage 2.3 deliverables:
//!
//! * fairness across ≥ 4 simulated cores,
//! * work-stealing,
//! * IPI-based preemption,
//! * starvation-freedom under the priority boost rule,
//! * cancellation-safe lifecycle (`spawn`, `park`, `unpark`, `exit`).
//!
//! The tests use a single host thread and the deterministic [`TestArch`]
//! so they are reproducible (`AGENTS.md` §7 — no flaky tests).

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use rustos_kernel_sched_mlfq::{
    Priority, SchedError, Scheduler, SchedulerConfig, TaskAction, TaskState, TestArch,
};

fn mk(cpus: u32) -> (Arc<TestArch>, Scheduler<TestArch>) {
    mk_with(cpus, |c| {
        SchedulerConfig {
            cpus: c,
            queue_capacity_per_band: 64,
            yields_before_demotion: 1,
            boost_interval_ticks: 1_000_000, // disabled unless a test wants it
        }
    })
}

fn mk_with(
    cpus: u32,
    mut shape: impl FnMut(u32) -> SchedulerConfig,
) -> (Arc<TestArch>, Scheduler<TestArch>) {
    let arch = Arc::new(TestArch::new(cpus).expect("test arch"));
    let cfg = shape(cpus);
    let sched = Scheduler::new(cfg, arch.clone()).expect("scheduler");
    (arch, sched)
}

/// Drive `cpu` through the scheduler until either `max_steps` elapse or
/// the predicate `done` is satisfied. Returns the number of steps taken.
fn drive_until(
    arch: &TestArch,
    sched: &Scheduler<TestArch>,
    cpu: u32,
    max_steps: u64,
    mut done: impl FnMut() -> bool,
) -> u64 {
    arch.set_current_cpu(cpu);
    for n in 0..max_steps {
        if done() {
            return n;
        }
        let _ = sched.step(cpu).expect("step");
        arch.advance_ticks(1);
    }
    max_steps
}

#[test]
fn fairness_four_cores_equal_workloads() {
    // Spawn N identical CPU-bound yielders, one on each of four cores;
    // drive each core in a round-robin fashion; assert every task got
    // a similar share of the CPU.
    let (arch, sched) = mk(4);
    let runs: Vec<Arc<AtomicU32>> = (0..4).map(|_| Arc::new(AtomicU32::new(0))).collect();
    let mut ids = Vec::new();
    for (cpu, r) in runs.iter().enumerate() {
        let r = r.clone();
        let id = sched
            .spawn(cpu as u32, Priority::Normal, move |_| {
                if r.fetch_add(1, Ordering::Relaxed) >= 99 {
                    TaskAction::Exit
                } else {
                    TaskAction::Yield
                }
            })
            .expect("spawn");
        ids.push(id);
    }
    // Round-robin steps; each task should run exactly 100 times.
    'outer: for _round in 0..1000 {
        let mut all_done = true;
        for cpu in 0..4u32 {
            arch.set_current_cpu(cpu);
            let _ = sched.step(cpu).expect("step");
            arch.advance_ticks(1);
            if sched.live_task_count() > 0 {
                all_done = false;
            }
        }
        if all_done {
            break 'outer;
        }
    }
    for (i, r) in runs.iter().enumerate() {
        let v = r.load(Ordering::Relaxed);
        assert_eq!(v, 100, "task on cpu {i} should have run exactly 100 times");
    }
}

#[test]
fn work_stealing_balances_load_across_cores() {
    // Spawn all tasks on CPU 0; drive all four cores. With work-stealing
    // the other CPUs must pick up work; without it CPU 0's queue would
    // be the only one consumed.
    let (arch, sched) = mk(4);
    let executor_cpu: Arc<[AtomicU32; 4]> = Arc::new([
        AtomicU32::new(0),
        AtomicU32::new(0),
        AtomicU32::new(0),
        AtomicU32::new(0),
    ]);
    let task_count = 32u32;
    for _ in 0..task_count {
        let cpus = executor_cpu.clone();
        sched
            .spawn(0, Priority::Normal, move |ctx| {
                cpus[ctx.cpu as usize].fetch_add(1, Ordering::Relaxed);
                TaskAction::Exit
            })
            .expect("spawn");
    }
    // Drive all four cores until everything has run.
    for _round in 0..1000 {
        if sched.live_task_count() == 0 {
            break;
        }
        for cpu in 0..4u32 {
            arch.set_current_cpu(cpu);
            let _ = sched.step(cpu).expect("step");
            arch.advance_ticks(1);
        }
    }
    assert_eq!(sched.live_task_count(), 0, "all tasks completed");
    // At least two distinct CPUs must have executed work; otherwise
    // there was no stealing.
    let non_zero = executor_cpu
        .iter()
        .filter(|c| c.load(Ordering::Relaxed) > 0)
        .count();
    assert!(
        non_zero >= 2,
        "work-stealing should have spread {task_count} tasks across multiple CPUs (saw {non_zero})"
    );
    let total: u32 = executor_cpu.iter().map(|c| c.load(Ordering::Relaxed)).sum();
    assert_eq!(total, task_count);
}

#[test]
fn spawn_unpark_send_ipi_to_home_cpu() {
    // Preemption hook: spawning and unparking must notify the home CPU.
    let (arch, sched) = mk(2);
    let id = sched
        .spawn(1, Priority::High, |_| TaskAction::Park)
        .expect("spawn");
    assert!(arch.ipi_count(1) >= 1, "spawn must IPI home CPU");
    arch.set_current_cpu(1);
    sched.step(1).expect("step");
    assert_eq!(sched.state_of(id), TaskState::Parked);
    let before = arch.ipi_count(1);
    sched.unpark(id).expect("unpark");
    assert!(
        arch.ipi_count(1) > before,
        "unpark must IPI the home CPU again"
    );
    assert_eq!(arch.stray_ipi_count(), 0, "no stray IPIs");
}

#[test]
fn starvation_freedom_via_priority_boost() {
    // A High-priority task and a Low-priority task share a single CPU.
    // The High task yields forever (would normally starve the Low one).
    // With a boost interval of 4 ticks, the Low task must still run.
    let (arch, sched) = mk_with(1, |c| SchedulerConfig {
        cpus: c,
        queue_capacity_per_band: 64,
        yields_before_demotion: 1,
        boost_interval_ticks: 4,
    });
    let low_runs = Arc::new(AtomicU64::new(0));
    let low_clone = low_runs.clone();
    sched
        .spawn(0, Priority::High, |_| TaskAction::Yield)
        .expect("hi");
    sched
        .spawn(0, Priority::Low, move |_| {
            low_clone.fetch_add(1, Ordering::Relaxed);
            TaskAction::Yield
        })
        .expect("lo");
    drive_until(&arch, &sched, 0, 5_000, || false);
    let low = low_runs.load(Ordering::Relaxed);
    assert!(
        low >= 10,
        "low-priority task must escape starvation under priority boost, ran {low} times"
    );
}

#[test]
fn cancellation_safe_park_during_dispatch() {
    // External park while the task is mid-run: the body's next
    // intent must be overridden to "Parked".
    let (arch, sched) = mk(1);
    let park_after = Arc::new(AtomicU32::new(0));
    let park_after_clone = park_after.clone();
    let id = sched
        .spawn(0, Priority::Normal, move |_| {
            park_after_clone.fetch_add(1, Ordering::Relaxed);
            TaskAction::Yield
        })
        .expect("spawn");
    arch.set_current_cpu(0);
    let _ = sched.step(0).expect("step"); // first run, yielded
    sched.park(id).expect("park");
    assert_eq!(sched.state_of(id), TaskState::Parked);
    // No subsequent step should run the body.
    let runs_before = park_after.load(Ordering::Relaxed);
    let _ = sched.step(0).expect("step");
    assert_eq!(park_after.load(Ordering::Relaxed), runs_before);
}

#[test]
fn exit_drains_registry() {
    let (arch, sched) = mk(1);
    let id = sched
        .spawn(0, Priority::Normal, |_| TaskAction::Yield)
        .expect("spawn");
    sched.exit(id).expect("exit");
    // The next step finds the queued ID, observes Exited, and prunes.
    arch.set_current_cpu(0);
    let _ = sched.step(0).expect("step");
    assert_eq!(sched.live_task_count(), 0);
    assert_eq!(sched.run_count(id), Err(SchedError::NoSuchTask));
}

#[test]
fn unknown_cpu_is_rejected() {
    let (_arch, sched) = mk(2);
    assert_eq!(
        sched.spawn(99, Priority::Normal, |_| TaskAction::Exit),
        Err(SchedError::NoSuchCpu)
    );
    assert_eq!(sched.step(99), Err(SchedError::NoSuchCpu));
}
