//! Bounded-concurrency runner for independent jobs.
//!
//! Several `cargo xtask` stages run a set of independent, isolated jobs and
//! want them to overlap instead of paying the *sum* of their wall-clock costs.
//! Three callers share this one runner ("run more than one
//! at a time" lives in exactly one place):
//!
//! * the fuzz and proptest orchestrators, each a registry of
//!   wall-clock-budgeted `cargo test` harnesses ([`Work::Command`]);
//! * the QEMU integration matrix ([`Work::Closure`]), which plants per-test
//!   backing images and drives [`tairix_qemu::Runner::run`] in-process.
//!
//! # Weighted admission
//!
//! A job carries a **weight** and the runner admits jobs while the sum of the
//! in-flight weights stays within a **budget**. For the fuzz/proptest
//! harnesses every job weighs `1` and the budget is a job *count*, so the
//! model collapses to "run at most N at a time" — its historical behaviour.
//!
//! The QEMU matrix uses weight as **host-capacity demand**. One-vCPU guests
//! charge their vCPU and emulator/I/O work; SMP TCG guests charge the complete
//! host-derived budget and therefore run alone because their synchronising
//! vCPU threads require simultaneous progress. That is what makes guest
//! scheduling safe under TCG: a guest is not starved of host CPU, so its
//! wall-clock deadline
//! ([`tairix_qemu::Runner::run`]) stays as reachable as it is for a solo run,
//! and the no-flaky-tests / no-retry rules hold. (A single job heavier than
//! the whole budget — a guest with more vCPUs than the host has cores — still
//! runs, alone, when nothing else is in flight, rather than deadlocking.)
//!
//! # Output handling
//!
//! When more than one job can run at once, each job's output is serialised
//! into an atomic block so concurrent jobs never interleave their lines: a
//! [`Work::Command`] job has its stdio captured and printed on completion;
//! a [`Work::Closure`] job prints a start/end marker around its own run (the
//! QEMU closures keep their serial logs inside the returned error, so a
//! failure's log is printed once, by [`aggregate`], with no interleaving).
//! When at most one job can ever run at a time (budget `1`), a
//! [`Work::Command`] job instead streams its stdio live, exactly like a plain
//! sequential run. The runner fails closed: every job runs to completion even
//! if an earlier one failed, and the aggregate result names every failure.

use std::process::{Command, Stdio};
use std::sync::{Condvar, Mutex};
use std::thread;

/// What a [`Job`] actually does when run.
enum Work {
    /// An external command whose stdio the runner owns (captures or streams).
    Command(Command),
    /// An arbitrary unit of work that reports its own pass/fail and prints
    /// (or buffers) its own output. Used by the QEMU matrix, where the work
    /// is "plant images, then drive a guest to completion".
    Closure(Box<dyn FnOnce() -> Result<(), String> + Send>),
}

/// One labelled, weighted unit of work to run to completion.
pub struct Job {
    label: String,
    /// In-flight cost charged against the runner's budget while this job runs
    /// (see the module-level "Weighted admission"). Always at least one.
    weight: usize,
    work: Work,
}

impl Job {
    /// Pair a human-readable label with the command that performs the job.
    /// The job weighs one unit, so a count-based budget runs at most `budget`
    /// such jobs at a time.
    pub fn new(label: impl Into<String>, command: Command) -> Self {
        Self {
            label: label.into(),
            weight: 1,
            work: Work::Command(command),
        }
    }

    /// Pair a label with a closure that performs the job and reports its own
    /// pass/fail, charging `weight` against the runner's budget while it runs
    /// (clamped to at least one).
    pub fn closure(
        label: impl Into<String>,
        weight: usize,
        work: impl FnOnce() -> Result<(), String> + Send + 'static,
    ) -> Self {
        Self {
            label: label.into(),
            weight: weight.max(1),
            work: Work::Closure(Box::new(work)),
        }
    }
}

/// The host's available parallelism (logical CPU count), or one when it cannot
/// be determined so callers degrade to a sequential run rather than failing.
#[must_use]
pub fn host_parallelism() -> usize {
    thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
}

/// Concurrency to use for `count` weight-one jobs: the host's available
/// parallelism, clamped to the job count and to at least one.
#[must_use]
pub fn default_concurrency(count: usize) -> usize {
    count.min(host_parallelism()).max(1)
}

/// Run every job, keeping the sum of in-flight weights within `budget`, and
/// fail closed.
///
/// All jobs run to completion regardless of earlier failures, so one broken
/// job never hides another. The returned error names every job that failed.
pub fn run(jobs: Vec<Job>, budget: usize) -> Result<(), String> {
    let total = jobs.len();
    if total == 0 {
        return Ok(());
    }
    let budget = budget.max(1);

    if budget == 1 {
        // At most one job can ever be in flight (every weight is >= 1), so
        // stream each job's output live (no thread overhead) and still run
        // them all so the failure report is complete.
        let failures: Vec<String> = jobs.into_iter().filter_map(run_streaming).collect();
        return aggregate(failures);
    }

    let results: Mutex<Vec<Option<Result<(), String>>>> =
        Mutex::new((0..total).map(|_| None).collect());
    let stdio_lock = Mutex::new(());
    // Current sum of in-flight weights, guarded by a condvar the producer
    // waits on for capacity and each finished worker notifies.
    let in_flight = Mutex::new(0usize);
    let capacity = Condvar::new();

    // Shared, borrowed by every worker (distinct names so the owned `results`
    // can still be consumed after the scope); the per-iteration
    // `job`/`idx`/`weight` are moved into each worker instead.
    let results_ref = &results;
    let stdio_ref = &stdio_lock;
    let in_flight_ref = &in_flight;
    let capacity_ref = &capacity;

    thread::scope(|scope| {
        for (idx, job) in jobs.into_iter().enumerate() {
            let weight = job.weight;
            // Admit the job once it fits the remaining budget, or once nothing
            // is running (so a job heavier than the whole budget still runs,
            // alone, rather than deadlocking).
            {
                let mut flight = in_flight_ref.lock().expect("in-flight lock");
                while *flight != 0 && *flight + weight > budget {
                    flight = capacity_ref.wait(flight).expect("capacity wait");
                }
                *flight += weight;
            }
            scope.spawn(move || {
                let outcome = run_captured(job, stdio_ref);
                results_ref.lock().expect("results lock")[idx] = Some(outcome);
                let mut flight = in_flight_ref.lock().expect("in-flight lock");
                *flight -= weight;
                capacity_ref.notify_all();
            });
        }
    });

    let failures: Vec<String> = results
        .into_inner()
        .expect("results lock")
        .into_iter()
        .flatten()
        .filter_map(Result::err)
        .collect();
    aggregate(failures)
}

/// Run one command job inheriting the parent's stdio (live output). Returns
/// the failure message when the job fails, or `None` on success. Only used on
/// the budget-one streaming path, where every job is run one at a time.
fn run_streaming(job: Job) -> Option<String> {
    let Job { label, work, .. } = job;
    match work {
        Work::Command(mut command) => {
            eprintln!("xtask: [{label}] {command:?}");
            match command.status() {
                Ok(status) if status.success() => None,
                Ok(status) => Some(format!("{label} failed with {status}")),
                Err(err) => Some(format!("{label} could not be spawned: {err}")),
            }
        }
        Work::Closure(work) => {
            eprintln!("xtask: [{label}] starting");
            work().err()
        }
    }
}

/// Run one job with its output serialised into a single atomic block so
/// concurrent jobs never interleave their lines.
fn run_captured(job: Job, stdio_lock: &Mutex<()>) -> Result<(), String> {
    let Job { label, work, .. } = job;
    match work {
        Work::Command(mut command) => {
            {
                let _guard = stdio_lock.lock().expect("stdio lock");
                eprintln!("xtask: [{label}] starting: {command:?}");
            }
            command.stdin(Stdio::null());
            let output = match command.output() {
                Ok(output) => output,
                Err(err) => return Err(format!("{label} could not be spawned: {err}")),
            };

            {
                let _guard = stdio_lock.lock().expect("stdio lock");
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                eprintln!("xtask: [{label}] ----- output -----");
                if !stdout.trim().is_empty() {
                    eprint!("{stdout}");
                }
                if !stderr.trim().is_empty() {
                    eprint!("{stderr}");
                }
                eprintln!("xtask: [{label}] ----- end ({}) -----", output.status);
            }

            if output.status.success() {
                Ok(())
            } else {
                Err(format!("{label} failed with {}", output.status))
            }
        }
        Work::Closure(work) => {
            {
                let _guard = stdio_lock.lock().expect("stdio lock");
                eprintln!("xtask: [{label}] starting");
            }
            // The closure reports its own pass/fail; on failure it returns the
            // full detail (e.g. a guest's serial log), which `aggregate`
            // prints once at the end so nothing interleaves with a live job.
            work()
        }
    }
}

/// Collapse the collected failures into one `Result`.
fn aggregate(failures: Vec<String>) -> Result<(), String> {
    match failures.len() {
        0 => Ok(()),
        1 => Err(failures.into_iter().next().unwrap_or_default()),
        n => Err(format!("{n} job(s) failed:\n{}", failures.join("\n"))),
    }
}

#[cfg(test)]
mod tests {
    use super::{default_concurrency, host_parallelism, run, Job};
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn ok_job(label: &str) -> Job {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "exit 0"]);
        Job::new(label, cmd)
    }

    fn fail_job(label: &str) -> Job {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "exit 1"]);
        Job::new(label, cmd)
    }

    #[test]
    fn host_parallelism_is_at_least_one() {
        assert!(host_parallelism() >= 1);
    }

    #[test]
    fn concurrency_is_clamped_to_the_job_count_and_at_least_one() {
        assert_eq!(default_concurrency(0), 1);
        assert_eq!(default_concurrency(1), 1);
        // Never exceeds the job count even on a many-core host.
        assert!(default_concurrency(2) <= 2);
        assert!(default_concurrency(2) >= 1);
    }

    #[test]
    fn an_empty_job_list_succeeds() {
        assert!(run(Vec::new(), 4).is_ok());
    }

    #[test]
    fn all_successful_jobs_pass_in_parallel() {
        let jobs = vec![ok_job("a"), ok_job("b"), ok_job("c")];
        assert!(run(jobs, 3).is_ok());
    }

    #[test]
    fn all_successful_jobs_pass_when_forced_serial() {
        let jobs = vec![ok_job("a"), ok_job("b")];
        assert!(run(jobs, 1).is_ok());
    }

    #[test]
    fn a_single_failing_job_fails_closed() {
        let err = run(vec![ok_job("a"), fail_job("bad"), ok_job("c")], 3)
            .expect_err("a failing job must fail the run");
        assert!(
            err.contains("bad"),
            "error should name the failed job: {err}"
        );
    }

    #[test]
    fn every_failure_is_reported_not_just_the_first() {
        let err = run(vec![fail_job("first"), fail_job("second")], 2)
            .expect_err("failures must propagate");
        assert!(err.contains("first"), "missing first failure: {err}");
        assert!(err.contains("second"), "missing second failure: {err}");
    }

    #[test]
    fn closure_jobs_run_and_report_their_own_result() {
        let ran = Arc::new(AtomicUsize::new(0));
        let jobs: Vec<Job> = (0..4)
            .map(|i| {
                let ran = Arc::clone(&ran);
                Job::closure(format!("c{i}"), 1, move || {
                    ran.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            })
            .collect();
        assert!(run(jobs, 2).is_ok());
        assert_eq!(ran.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn a_failing_closure_job_is_reported_by_label_and_detail() {
        let jobs = vec![
            Job::closure("good", 1, || Ok(())),
            Job::closure("weighty", 2, || Err("boom detail".to_string())),
        ];
        let err = run(jobs, 4).expect_err("a failing closure must fail the run");
        assert!(err.contains("boom detail"), "missing detail: {err}");
    }

    #[test]
    fn weighted_admission_never_exceeds_the_budget() {
        // Track concurrent in-flight weight and assert it never exceeds the
        // budget. Each job holds its weight briefly so overlap is real.
        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let budget = 4;
        let weights = [1usize, 2, 1, 3, 1, 1, 2];
        let jobs: Vec<Job> = weights
            .iter()
            .enumerate()
            .map(|(i, &w)| {
                let live = Arc::clone(&live);
                let peak = Arc::clone(&peak);
                Job::closure(format!("w{i}"), w, move || {
                    let now = live.fetch_add(w, Ordering::SeqCst) + w;
                    peak.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    live.fetch_sub(w, Ordering::SeqCst);
                    Ok(())
                })
            })
            .collect();
        assert!(run(jobs, budget).is_ok());
        assert!(
            peak.load(Ordering::SeqCst) <= budget,
            "in-flight weight {} exceeded budget {budget}",
            peak.load(Ordering::SeqCst)
        );
    }

    #[test]
    fn a_job_heavier_than_the_budget_still_runs_alone() {
        let ran = Arc::new(AtomicUsize::new(0));
        let ran2 = Arc::clone(&ran);
        // Weight 9 against a budget of 4: it cannot fit the budget, so it must
        // run when nothing else is in flight rather than deadlock.
        let jobs = vec![
            Job::closure("oversized", 9, move || {
                ran2.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
            ok_job("small"),
        ];
        assert!(run(jobs, 4).is_ok());
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }
}
