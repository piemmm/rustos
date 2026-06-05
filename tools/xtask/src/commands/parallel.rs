//! Bounded-concurrency runner for independent `cargo` jobs.
//!
//! The fuzz (§19.6) and proptest (§19.7) orchestrators each run a registry of
//! independent, wall-clock-budgeted `cargo test` harnesses. Running them one
//! after another makes a `cargo xtask ci` pay the *sum* of every harness's
//! budget; running them concurrently pays roughly the longest single budget
//! instead. Each harness is an isolated host process whose only shared
//! resource is cargo's build lock (which cargo itself serialises), so
//! concurrency only lowers each harness's per-second iteration count — never
//! its correctness, and never its `>= budget` run time.
//!
//! This is the single place that "run more than one at a time" lives
//! (`AGENTS.md` §2.2): both the fuzz and the proptest orchestrators route
//! through it rather than each growing its own thread pool. The QEMU matrix
//! is deliberately *not* routed through it — a QEMU run enforces a hard
//! wall-clock deadline ([`rustos_qemu::Runner::run`]) and several enrolments
//! wait on a fixed number of emulated timer ticks, so co-scheduling guests
//! under TCG would let host CPU contention push a guest past its deadline and
//! turn a green test red. §7 forbids flaky tests, so the QEMU matrix runs one
//! guest at a time.
//!
//! Output handling: when more than one job runs at once, each job's stdio is
//! captured and printed as a single atomic block when the job finishes, so
//! concurrent jobs never interleave their lines. A single job (the common
//! `--target NAME` case, or a single-core host) instead streams its stdio
//! live, exactly like a plain sequential run. The runner fails closed: every
//! job runs to completion even if an earlier one failed, and the aggregate
//! result is an error naming every job that failed.

use std::collections::VecDeque;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::thread;

/// One labelled command to run to completion.
pub struct Job {
    label: String,
    command: Command,
}

impl Job {
    /// Pair a human-readable label with the command that performs the job.
    pub fn new(label: impl Into<String>, command: Command) -> Self {
        Self {
            label: label.into(),
            command,
        }
    }
}

/// Concurrency to use for `count` jobs: the host's available parallelism,
/// clamped to the job count and to at least one. Returns one when the host
/// parallelism cannot be determined, so the runner degrades to a plain
/// sequential run rather than failing.
#[must_use]
pub fn default_concurrency(count: usize) -> usize {
    let cpus = thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    count.min(cpus).max(1)
}

/// Run every job, at most `concurrency` at a time, and fail closed.
///
/// All jobs run to completion regardless of earlier failures, so one broken
/// harness never hides another. The returned error names every job that
/// failed.
pub fn run(jobs: Vec<Job>, concurrency: usize) -> Result<(), String> {
    let total = jobs.len();
    if total == 0 {
        return Ok(());
    }
    let concurrency = concurrency.clamp(1, total);

    if concurrency == 1 {
        // Serial path: stream each job's stdio live (no thread overhead) and
        // still run them all so the failure report is complete.
        let failures: Vec<String> = jobs.into_iter().filter_map(run_streaming).collect();
        return aggregate(failures);
    }

    let queue: Mutex<VecDeque<(usize, Job)>> = Mutex::new(jobs.into_iter().enumerate().collect());
    let results: Mutex<Vec<Option<Result<(), String>>>> =
        Mutex::new((0..total).map(|_| None).collect());
    let stdio_lock = Mutex::new(());

    thread::scope(|scope| {
        for _ in 0..concurrency {
            scope.spawn(|| loop {
                let next = queue.lock().expect("job queue lock").pop_front();
                let Some((idx, job)) = next else {
                    break;
                };
                let outcome = run_captured(job, &stdio_lock);
                results.lock().expect("results lock")[idx] = Some(outcome);
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

/// Run one job inheriting the parent's stdio (live output). Returns the
/// failure message when the job fails, or `None` on success.
fn run_streaming(job: Job) -> Option<String> {
    let Job { label, mut command } = job;
    eprintln!("xtask: [{label}] {command:?}");
    match command.status() {
        Ok(status) if status.success() => None,
        Ok(status) => Some(format!("{label} failed with {status}")),
        Err(err) => Some(format!("{label} could not be spawned: {err}")),
    }
}

/// Run one job with its stdio captured, printing a single atomic block when
/// it finishes so concurrent jobs never interleave their output.
fn run_captured(job: Job, stdio_lock: &Mutex<()>) -> Result<(), String> {
    let Job { label, mut command } = job;
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
    use super::{default_concurrency, run, Job};
    use std::process::Command;

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
}
