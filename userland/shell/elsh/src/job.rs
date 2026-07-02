//! Process identity, exit status, and the job table used for job control.
//!
//! The shell runs each pipeline as a *job*. A foreground job is awaited
//! before the next prompt; a background job (`&`) is recorded here and the
//! prompt returns immediately. `jobs`, `fg`, and `bg` operate on this table.
//!
//! The types here are kernel-plumbing-free: the actual launching, waiting,
//! and signalling is the [`ProcessHost`](crate::ProcessHost) seam's job. This
//! module only models *what the shell tracks* about the children that seam
//! creates.

use alloc::string::String;
use alloc::vec::Vec;

/// Process identifier issued by the kernel when a job is launched.
///
/// For a pipeline this is the process-group leader; signals sent to it reach
/// the whole pipeline.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct Pid(u64);

impl Pid {
    /// Construct a [`Pid`] from its raw kernel value.
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Raw kernel value.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

/// Shell-assigned job number, displayed as `%N` (e.g. `%1`).
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct JobId(u32);

impl JobId {
    /// The job's number.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self.0
    }
}

/// A signal the shell can ask the host to deliver to a job.
///
/// This is the shell's own vocabulary; the [`ProcessHost`](crate::ProcessHost)
/// maps each variant to the platform's signal number. It is deliberately the
/// minimal set job control needs.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Signal {
    /// Resume a stopped job (`bg`, `fg`).
    Continue,
    /// Ask a job to terminate gracefully.
    Terminate,
    /// Forcibly kill a job.
    Kill,
}

/// How a job's process terminated, resolved to a shell exit code.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ExitStatus {
    /// The process called exit with this code.
    Exited(i32),
    /// The process was terminated by the signal with this number.
    Signaled(i32),
}

impl ExitStatus {
    /// The shell exit code: the raw code for a normal exit, or `128 + signal`
    /// for a signalled one (the conventional shell mapping).
    #[must_use]
    pub fn code(self) -> i32 {
        match self {
            Self::Exited(code) => code,
            Self::Signaled(signal) => 128 + signal,
        }
    }

    /// `true` only for a zero-code normal exit.
    #[must_use]
    pub fn success(self) -> bool {
        matches!(self, Self::Exited(0))
    }
}

/// The outcome of waiting on a job: it may exit, be killed, or merely stop.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum WaitOutcome {
    /// The job exited normally with this code.
    Exited(i32),
    /// The job was terminated by the signal with this number.
    Signaled(i32),
    /// The job stopped (and can later be resumed) on this signal number.
    Stopped(i32),
}

impl WaitOutcome {
    /// The terminal [`ExitStatus`], or `None` if the job only stopped.
    #[must_use]
    pub fn terminal(self) -> Option<ExitStatus> {
        match self {
            Self::Exited(code) => Some(ExitStatus::Exited(code)),
            Self::Signaled(signal) => Some(ExitStatus::Signaled(signal)),
            Self::Stopped(_) => None,
        }
    }
}

/// The lifecycle state of a tracked job.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum JobState {
    /// Currently executing.
    Running,
    /// Stopped, awaiting `fg`/`bg`.
    Stopped,
    /// Finished, with its final status. Retained until reported, then pruned.
    Done(ExitStatus),
}

/// One tracked job.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Job {
    /// Shell job number.
    pub id: JobId,
    /// Process-group leader.
    pub pid: Pid,
    /// The source text of the pipeline, for `jobs` display.
    pub command: String,
    /// Current lifecycle state.
    pub state: JobState,
}

/// The set of jobs the shell is tracking, with monotonically-issued ids.
#[derive(Debug, Default)]
pub struct JobTable {
    jobs: Vec<Job>,
    next_id: u32,
}

impl JobTable {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self {
            jobs: Vec::new(),
            next_id: 1,
        }
    }

    /// Record a newly-launched job and return its assigned [`JobId`].
    pub fn add(&mut self, pid: Pid, command: impl Into<String>, state: JobState) -> JobId {
        let id = JobId(self.next_id);
        self.next_id += 1;
        self.jobs.push(Job {
            id,
            pid,
            command: command.into(),
            state,
        });
        id
    }

    /// Number of tracked jobs (including not-yet-reported `Done` jobs).
    #[must_use]
    pub fn len(&self) -> usize {
        self.jobs.len()
    }

    /// `true` if no jobs are tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }

    /// All tracked jobs, in id order.
    #[must_use]
    pub fn all(&self) -> &[Job] {
        &self.jobs
    }

    /// Look up a job by its number.
    #[must_use]
    pub fn by_id(&self, id: JobId) -> Option<&Job> {
        self.jobs.iter().find(|j| j.id == id)
    }

    /// Look up the job whose leader has this [`Pid`].
    #[must_use]
    pub fn by_pid(&self, pid: Pid) -> Option<&Job> {
        self.jobs.iter().find(|j| j.pid == pid)
    }

    /// The most-recently-added job that is not finished — the default target
    /// of `fg`/`bg` with no argument (POSIX "current job").
    #[must_use]
    pub fn current(&self) -> Option<JobId> {
        self.jobs
            .iter()
            .rev()
            .find(|j| !matches!(j.state, JobState::Done(_)))
            .map(|j| j.id)
    }

    /// Replace the state of the job with this leader [`Pid`]. Returns `false`
    /// if no such job is tracked.
    pub fn set_state(&mut self, pid: Pid, state: JobState) -> bool {
        match self.jobs.iter_mut().find(|j| j.pid == pid) {
            Some(job) => {
                job.state = state;
                true
            }
            None => false,
        }
    }

    /// Remove the job with this id, returning it if present.
    pub fn remove(&mut self, id: JobId) -> Option<Job> {
        let pos = self.jobs.iter().position(|j| j.id == id)?;
        Some(self.jobs.remove(pos))
    }

    /// Remove and return every finished (`Done`) job, in id order — used to
    /// report completed background jobs before a prompt.
    pub fn drain_done(&mut self) -> Vec<Job> {
        let mut done = Vec::new();
        let mut i = 0;
        while i < self.jobs.len() {
            if matches!(self.jobs[i].state, JobState::Done(_)) {
                done.push(self.jobs.remove(i));
            } else {
                i += 1;
            }
        }
        done
    }
}

#[cfg(test)]
mod tests {
    use super::{ExitStatus, Job, JobState, JobTable, Pid, WaitOutcome};

    #[test]
    fn exit_status_maps_codes() {
        assert_eq!(ExitStatus::Exited(0).code(), 0);
        assert!(ExitStatus::Exited(0).success());
        assert_eq!(ExitStatus::Exited(3).code(), 3);
        assert!(!ExitStatus::Exited(3).success());
        // SIGTERM-like 15 -> 143.
        assert_eq!(ExitStatus::Signaled(15).code(), 143);
        assert!(!ExitStatus::Signaled(15).success());
    }

    #[test]
    fn wait_outcome_distinguishes_stop_from_termination() {
        assert_eq!(
            WaitOutcome::Exited(2).terminal(),
            Some(ExitStatus::Exited(2))
        );
        assert_eq!(
            WaitOutcome::Signaled(9).terminal(),
            Some(ExitStatus::Signaled(9))
        );
        assert_eq!(WaitOutcome::Stopped(19).terminal(), None);
    }

    #[test]
    fn table_assigns_increasing_ids() {
        let mut table = JobTable::new();
        let a = table.add(Pid::new(10), "sleep 1", JobState::Running);
        let b = table.add(Pid::new(11), "sleep 2", JobState::Running);
        assert_eq!(a.as_u32(), 1);
        assert_eq!(b.as_u32(), 2);
        assert_eq!(table.len(), 2);
        assert_eq!(table.by_id(a).map(|j| j.pid), Some(Pid::new(10)));
        assert_eq!(table.by_pid(Pid::new(11)).map(|j| j.id), Some(b));
    }

    #[test]
    fn current_is_most_recent_unfinished() {
        let mut table = JobTable::new();
        let a = table.add(Pid::new(10), "a", JobState::Running);
        let b = table.add(Pid::new(11), "b", JobState::Running);
        assert_eq!(table.current(), Some(b));
        table.set_state(Pid::new(11), JobState::Done(ExitStatus::Exited(0)));
        assert_eq!(table.current(), Some(a));
    }

    #[test]
    fn drain_done_returns_and_prunes_finished_jobs() {
        let mut table = JobTable::new();
        table.add(Pid::new(10), "a", JobState::Done(ExitStatus::Exited(0)));
        table.add(Pid::new(11), "b", JobState::Running);
        table.add(Pid::new(12), "c", JobState::Done(ExitStatus::Exited(1)));
        let done: alloc::vec::Vec<Job> = table.drain_done();
        assert_eq!(done.len(), 2);
        assert_eq!(table.len(), 1);
        assert_eq!(table.all()[0].pid, Pid::new(11));
    }

    #[test]
    fn set_and_remove_report_presence() {
        let mut table = JobTable::new();
        let a = table.add(Pid::new(10), "a", JobState::Running);
        assert!(table.set_state(Pid::new(10), JobState::Stopped));
        assert!(!table.set_state(Pid::new(99), JobState::Stopped));
        assert_eq!(table.remove(a).map(|j| j.pid), Some(Pid::new(10)));
        assert!(table.is_empty());
        assert!(table.remove(a).is_none());
    }
}
