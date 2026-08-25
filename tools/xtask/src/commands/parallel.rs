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
//! The QEMU matrix uses weight as **host-capacity demand**. A uniprocessor
//! guest charges two units (its vCPU plus emulator/I/O work); an SMP TCG guest
//! charges the complete budget and therefore runs alone, because a co-scheduled
//! SMP guest starves so badly it trips its own in-guest lockup watchdog. The
//! budget is one third of the host's logical CPUs — deliberate headroom, since
//! a QEMU guest is far heavier than its lone vCPU thread and each guest runs
//! its own real-time watchdogs, so admitting one guest per logical CPU
//! oversubscribes the host into missing those internal deadlines. Within that
//! headroom a few uniprocessor guests overlap; the guest's *inactivity*
//! deadline ([`tairix_qemu::Runner::run`]) is reset by every line it prints,
//! so a guest that merely runs a little slower is never falsely timed out —
//! the no-flaky-tests / no-retry rules hold — while its absolute runtime
//! ceiling still bounds a guest that keeps talking without finishing. (A
//! single job heavier than the whole budget — a guest with more vCPUs than the
//! budget — still runs, alone, when nothing else is in flight, rather than
//! deadlocking.)
//!
//! # Output handling
//!
//! When more than one job can run at once, each job's output is serialised
//! into an atomic block so concurrent jobs never interleave their lines: a
//! [`Work::Command`] job has its stdio captured and printed on completion;
//! a [`Work::Closure`] job prints a start marker and, on completion, its
//! verdict and wall-clock duration (the QEMU closures keep their serial logs
//! inside the returned error, so a failure's log is printed once, by
//! [`aggregate`], with no interleaving). Reporting every completion is what
//! makes an outstanding job visible: without it a still-running job is
//! indistinguishable from a finished one in the log.
//! Each job is additionally announced at the moment it is *admitted*, with the
//! resulting in-flight weight, so the log reflects real concurrency rather
//! than a wall of near-simultaneous start lines that says nothing about
//! admission.
//! When at most one job can ever run at a time (budget `1`), a
//! [`Work::Command`] job instead streams its stdio live, exactly like a plain
//! sequential run. The runner fails closed: every job runs to completion even
//! if an earlier one failed, and the aggregate result names every failure.

use std::process::{Command, Stdio};
use std::sync::{Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::{
    await_within, effective_timeout, soak_deadline, spawn_in_own_group, DEFAULT_COMMAND_TIMEOUT,
};

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
    /// Wall-clock budget for a [`Work::Command`] job, after which its process
    /// group is killed and the job fails.
    ///
    /// Unused by a [`Work::Closure`] job: a running thread cannot be
    /// cancelled from outside, so a closure **must** bound its own work (the
    /// QEMU closure does so through its guest runtime ceiling). A closure that
    /// can run forever would hang the whole runner.
    budget: Duration,
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
            budget: DEFAULT_COMMAND_TIMEOUT,
            work: Work::Command(command),
        }
    }

    /// Give this job a wall-clock budget other than the default.
    ///
    /// For a job that is legitimately slower than an ordinary step — a
    /// cross-compile that rebuilds the standard library from source, say — so
    /// that a correct-but-slow job is not mistaken for a hung one. It is the
    /// parallel counterpart of the sequential runner's per-step budget, and
    /// the same operator override raises it.
    #[must_use]
    pub fn with_budget(mut self, budget: Duration) -> Self {
        self.budget = budget;
        self
    }

    /// Budget this job to outlast an inner soak of `harness`, through the
    /// shared [`soak_deadline`] policy every soak orchestrator uses.
    ///
    /// A soaking fuzz harness or property model is *supposed* to run for its
    /// whole budget, so the job must outlast it or the runner would kill work
    /// doing exactly what it was asked to — a nightly 24-hour soak most of
    /// all. `None` — a single smoke iteration rather than a soak — simply
    /// keeps the ordinary budget.
    #[must_use]
    pub fn with_soak_budget(self, harness: Option<Duration>) -> Self {
        match harness {
            Some(soak) => self.with_budget(soak_deadline(soak)),
            None => self,
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
            budget: DEFAULT_COMMAND_TIMEOUT,
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

/// Order jobs heaviest-weight first, keeping equal weights in the order given.
///
/// Admission is head-of-line: a job waits until its weight fits the remaining
/// budget. A whole-budget job therefore starts only once nothing else is
/// running, so meeting one part-way through drains the runner to empty and
/// leaves the host idle while it finishes — one bubble per heavy job. Taking
/// the heavy jobs while the runner is *already* empty pays that drain once, at
/// the start, and the lighter jobs then pack in behind them. Simulated over the
/// measured QEMU matrix (11 whole-budget guests among 151) this lands within
/// three seconds of the theoretical floor, where the given order costs fifty
/// more.
///
/// A stable sort, so a set of equal-weight jobs keeps its caller's order and
/// the uniform weight-one gate groups are unaffected.
fn heaviest_first(jobs: &mut [Job]) {
    jobs.sort_by_key(|job| core::cmp::Reverse(job.weight));
}

/// Run every job, keeping the sum of in-flight weights within `budget`, and
/// fail closed.
///
/// All jobs run to completion regardless of earlier failures, so one broken
/// job never hides another. The returned error names every job that failed.
pub fn run(mut jobs: Vec<Job>, budget: usize) -> Result<(), String> {
    let total = jobs.len();
    if total == 0 {
        return Ok(());
    }
    let budget = budget.max(1);
    heaviest_first(&mut jobs);

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
            let in_flight_now = {
                let mut flight = in_flight_ref.lock().expect("in-flight lock");
                while *flight != 0 && *flight + weight > budget {
                    flight = capacity_ref.wait(flight).expect("capacity wait");
                }
                *flight += weight;
                *flight
            };
            // Announce the job at the moment it is *admitted*, with the
            // resulting in-flight weight, so the log honestly reflects how much
            // work is running concurrently. Without this, successful closure
            // jobs print nothing on completion, so their worker-side start
            // lines pile up into a wall that looks simultaneous and says
            // nothing about admission. A whole-budget job now shows
            // `admitted (N/N in flight)` with no further admission until it
            // completes, making its isolation visible in the log itself.
            {
                let _guard = stdio_ref.lock().expect("stdio lock");
                eprintln!(
                    "xtask: [{}] admitted ({in_flight_now}/{budget} in flight)",
                    job.label
                );
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
    let Job {
        label,
        work,
        budget,
        ..
    } = job;
    match work {
        Work::Command(mut command) => {
            let budget = match effective_timeout(budget) {
                Ok(budget) => budget,
                Err(err) => return Some(err),
            };
            eprintln!("xtask: [{label}] {command:?} (timeout {budget:?})");
            let mut child = match spawn_in_own_group(&mut command) {
                Ok(child) => child,
                Err(err) => return Some(format!("{label} could not be spawned: {err}")),
            };
            let pid = child.id();
            match await_within(&label, pid, budget, move || child.wait()) {
                Ok(status) if status.success() => None,
                Ok(status) => Some(format!("{label} failed with {status}")),
                Err(err) => Some(err),
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
    let Job {
        label,
        work,
        budget,
        ..
    } = job;
    match work {
        Work::Command(mut command) => {
            let budget = effective_timeout(budget)?;
            {
                let _guard = stdio_lock.lock().expect("stdio lock");
                eprintln!("xtask: [{label}] starting: {command:?} (timeout {budget:?})");
            }
            // Piped explicitly because the job is spawned directly rather
            // than through `Command::output`, which would set these itself —
            // spawning it here is what puts it in its own process group, so a
            // job that hangs can be killed along with everything it forked.
            command
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let child = match spawn_in_own_group(&mut command) {
                Ok(child) => child,
                Err(err) => return Err(format!("{label} could not be spawned: {err}")),
            };
            let pid = child.id();
            let output = await_within(&label, pid, budget, move || child.wait_with_output())?;

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
            let started = Instant::now();
            let outcome = work();
            {
                let _guard = stdio_lock.lock().expect("stdio lock");
                eprintln!(
                    "xtask: [{label}] {} in {:?}",
                    if outcome.is_ok() { "done" } else { "FAILED" },
                    started.elapsed()
                );
            }
            outcome
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

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

    /// A command job that would never finish on its own, budgeted so tightly
    /// that only a working kill can end it.
    ///
    /// The sleep is minutes long against a sub-second budget, so the margin is
    /// enormous in both directions: a busy host cannot make it pass by
    /// accident, and it can only complete quickly if the job was really
    /// killed.
    fn hung_job(label: &str) -> Job {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 600"]);
        Job::new(label, cmd).with_budget(Duration::from_millis(200))
    }

    #[test]
    fn host_parallelism_is_at_least_one() {
        assert!(host_parallelism() >= 1);
    }

    #[test]
    fn a_hung_job_is_killed_and_fails_when_jobs_run_concurrently() {
        let started = std::time::Instant::now();
        let err = run(vec![hung_job("stuck"), ok_job("fine")], 2)
            .expect_err("a job that outlives its budget must fail the run");
        assert!(err.contains("stuck"), "error should name the job: {err}");
        assert!(err.contains("timeout"), "error should say why: {err}");
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "the runner must not wait out a hung job, took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_hung_job_is_killed_and_fails_when_jobs_run_one_at_a_time() {
        // A budget of one takes the streaming path, which is a separate
        // implementation from the captured one above and so needs its own
        // proof that it is bounded.
        let started = std::time::Instant::now();
        let err = run(vec![hung_job("stuck")], 1)
            .expect_err("a job that outlives its budget must fail the run");
        assert!(err.contains("stuck"), "error should name the job: {err}");
        assert!(err.contains("timeout"), "error should say why: {err}");
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "the runner must not wait out a hung job, took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_soaking_job_outlasts_the_soak_it_is_running() {
        // A nightly soak runs for its whole budget by design; budgeting the
        // job at or below that would kill work doing exactly what was asked.
        let soak = Duration::from_hours(24);
        let job = ok_job("soaker").with_soak_budget(Some(soak));
        assert!(
            job.budget > soak,
            "a soaking job must outlast its own soak, got {:?} for {soak:?}",
            job.budget
        );
        // No soak means no reason to depart from the ordinary budget.
        assert_eq!(
            ok_job("smoke").with_soak_budget(None).budget,
            ok_job("d").budget
        );
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
    fn a_whole_budget_job_runs_strictly_alone() {
        // A job weighing the entire budget must never overlap another job:
        // this is the isolation the QEMU matrix relies on for a full-boot SMP
        // guest, whose mutually synchronising vCPU threads starve if
        // co-scheduled. Small jobs mark themselves "live" while they run; the
        // whole-budget job asserts nothing else is live for its whole run.
        let budget = 4;
        let live = Arc::new(AtomicUsize::new(0));
        let overlapped = Arc::new(AtomicBool::new(false));
        let mut jobs: Vec<Job> = Vec::new();
        for i in 0..8 {
            let live_small = Arc::clone(&live);
            jobs.push(Job::closure(format!("small{i}"), 1, move || {
                live_small.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(5));
                live_small.fetch_sub(1, Ordering::SeqCst);
                Ok(())
            }));
            // Drop the whole-budget job into the middle of the stream so
            // small jobs are in flight both before and after it.
            if i == 4 {
                let live_whole = Arc::clone(&live);
                let overlapped_whole = Arc::clone(&overlapped);
                jobs.push(Job::closure("whole", budget, move || {
                    // On entry and after a deliberate hold, no other job may
                    // be live — the whole-budget job owns the runner alone.
                    if live_whole.load(Ordering::SeqCst) != 0 {
                        overlapped_whole.store(true, Ordering::SeqCst);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(25));
                    if live_whole.load(Ordering::SeqCst) != 0 {
                        overlapped_whole.store(true, Ordering::SeqCst);
                    }
                    Ok(())
                }));
            }
        }
        assert!(run(jobs, budget).is_ok());
        assert!(
            !overlapped.load(Ordering::SeqCst),
            "a whole-budget job ran concurrently with another job"
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

    #[test]
    fn a_whole_budget_job_is_admitted_before_the_lighter_ones() {
        // Admission is head-of-line, so a whole-budget job listed last drains
        // the runner to empty first and leaves the host idle while it runs.
        // Recording the admission order proves the heavy job goes first.
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut jobs = Vec::new();
        for label in ["light-a", "light-b", "light-c"] {
            let seen = Arc::clone(&order);
            jobs.push(Job::closure(label, 2, move || {
                seen.lock().expect("order").push(label);
                Ok(())
            }));
        }
        let seen = Arc::clone(&order);
        jobs.push(Job::closure("heavy", 8, move || {
            seen.lock().expect("order").push("heavy");
            Ok(())
        }));

        assert!(run(jobs, 8).is_ok());
        let seen = order.lock().expect("order");
        assert_eq!(
            seen.first().copied(),
            Some("heavy"),
            "the whole-budget job must run before the lighter ones, got {seen:?}"
        );
        assert_eq!(seen.len(), 4, "every job must still run: {seen:?}");
    }

    #[test]
    fn equal_weight_jobs_keep_the_order_they_were_given() {
        // The gate groups are all weight one, so reordering must be a no-op
        // for them: a stable sort is what keeps their reporting deterministic.
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut jobs = Vec::new();
        for label in ["first", "second", "third"] {
            let seen = Arc::clone(&order);
            jobs.push(Job::closure(label, 1, move || {
                seen.lock().expect("order").push(label);
                Ok(())
            }));
        }
        // Budget one takes the streaming path, which runs strictly one at a
        // time, so the recorded order is the admission order exactly.
        assert!(run(jobs, 1).is_ok());
        assert_eq!(
            *order.lock().expect("order"),
            vec!["first", "second", "third"]
        );
    }
}
