//! Handing slow work off an interactive loop, onto a worker thread.
//!
//! An interactive surface owes the user a frame, so it must not carry out a
//! store write, an IPC round trip, or any other wait itself. [`Worker`] is the
//! one arrangement that takes such work: the loop *submits* and carries on
//! drawing, the worker parks until there is something to do, and the answer
//! arrives as a wake on the loop's own wait-set.
//!
//! It is one desk, one worker, latest-wins ([`tairix_util::defer::JobDesk`]),
//! so an interaction that settles repeatedly costs one further job rather than
//! one each, and two answers can never race for what a store ends up saying.
//!
//! # A machine that grants no worker is slower, never wrong
//!
//! The kernel may refuse the wake pipe or the thread. [`Worker::start`] then
//! leaves the desk stopped, which makes every later [`submit`](Worker::submit)
//! carry the job out on the caller's own thread and leave the answer where
//! [`collect`](Worker::collect) finds it. There is one adopt path either way,
//! so no caller needs a second one — and the loop is exactly as responsive as
//! it was before there was a worker at all.

use alloc::sync::Arc;

use tairix_abi::Errno;
use tairix_util::defer::JobDesk;

use crate::sync::{Condvar, Mutex, WorkerWake};
use crate::thread::{JoinHandle, Thread};

/// Why a program has no worker thread, so it can say so in its own words.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NoWorker {
    /// The kernel refused the pipe an answer would have woken the loop over.
    /// Without it a submitted job's answer would never be collected.
    Wake,
    /// The kernel refused the thread.
    Thread(Errno),
}

/// A worker thread and the desk it takes work from.
///
/// `run` is the work itself. It is a plain function pointer rather than a
/// closure or a trait so the same one serves the worker thread and the
/// fall-back path, and no caller can supply two that disagree.
pub struct Worker<Req, Ans> {
    desk: Mutex<JobDesk<Req, Ans>>,
    /// Signalled when a job is submitted, and on teardown.
    work: Condvar,
    wake: WorkerWake,
    run: fn(&Req) -> Ans,
}

impl<Req, Ans> Worker<Req, Ans> {
    /// A worker that carries out `run`, waking its loop over `wake`.
    ///
    /// Nothing is started until [`start`](Self::start).
    #[must_use]
    pub fn new(run: fn(&Req) -> Ans, wake: WorkerWake) -> Self {
        Self {
            desk: Mutex::new(JobDesk::new()),
            work: Condvar::new(),
            wake,
            run,
        }
    }

    /// The wake whose read end the loop adds to its wait-set, and drains when
    /// that token fires.
    #[must_use]
    pub const fn wake(&self) -> &WorkerWake {
        &self.wake
    }

    /// Ask for `job` to be carried out.
    ///
    /// Answers whether one is already waiting to be collected — `true` only
    /// where there is no worker, in which case the job ran on this thread and
    /// its answer is where [`collect`](Self::collect) will find it.
    pub fn submit(&self, job: Req) -> bool {
        let submitted = {
            let mut desk = self.desk.lock();
            if desk.stopping() {
                // No worker will ever take it, so it is carried out here and
                // left on the desk: one adopt path however it was run.
                drop(desk);
                let answer = (self.run)(&job);
                let _ = self.desk.lock().deliver(answer);
                return true;
            }
            desk.submit(job)
        };
        if submitted.wake {
            self.work.notify_one();
        }
        false
    }

    /// Take a landed answer, if one has.
    pub fn collect(&self) -> Option<Ans> {
        self.desk.lock().collect()
    }

    /// Ask the worker to leave, and wake it so it does.
    ///
    /// Also what puts the desk into the state that runs later submissions on
    /// the caller's own thread.
    pub fn stop(&self) {
        self.desk.lock().stop();
        self.work.notify_all();
    }

    /// One worker's whole life: park until there is a job, carry it out, leave
    /// the answer, nudge the loop.
    fn serve(&self) {
        loop {
            let job = {
                let mut desk = self.desk.lock();
                loop {
                    if desk.stopping() {
                        return;
                    }
                    if let Some(job) = desk.next_job() {
                        break job;
                    }
                    desk = self.work.wait(desk);
                }
            };
            // The wait itself, with no lock held: this is the call that would
            // otherwise have frozen the window.
            let answer = (self.run)(&job);
            if self.desk.lock().deliver(answer) {
                self.wake.nudge();
            }
        }
    }
}

impl<Req: Send + 'static, Ans: Send + 'static> Worker<Req, Ans> {
    /// Start `worker` on its own thread.
    ///
    /// On failure the desk is left stopped, so the program is still correct
    /// without a thread and the caller only has to state why it has none.
    ///
    /// # Errors
    ///
    /// [`NoWorker`] when the kernel refused the wake pipe or the thread.
    pub fn start(worker: &Arc<Self>) -> Result<JoinHandle<()>, NoWorker> {
        if !worker.wake.is_armed() {
            worker.stop();
            return Err(NoWorker::Wake);
        }
        let served = Arc::clone(worker);
        Thread::spawn(move || served.serve()).map_err(|err| {
            worker.stop();
            NoWorker::Thread(err)
        })
    }
}

/// Stops its worker on every way out of the scope holding it, so one is never
/// left working for a program that has ended.
///
/// The thread is *detached* rather than joined: a worker mid-write of a slow
/// store would otherwise hold the teardown for as long as that store takes, and
/// it leaves at its next turn round its loop anyway. Its own handle on the desk
/// keeps it alive until then.
pub struct WorkerGuard<Req, Ans>(Arc<Worker<Req, Ans>>);

impl<Req, Ans> WorkerGuard<Req, Ans> {
    /// Guard `worker`.
    #[must_use]
    pub fn new(worker: &Arc<Worker<Req, Ans>>) -> Self {
        Self(Arc::clone(worker))
    }
}

impl<Req, Ans> Drop for WorkerGuard<Req, Ans> {
    fn drop(&mut self) {
        self.0.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A job carrying more than a scalar, as the real ones do (a profile, an
    /// encoded document), so a reference is the natural way to hand it over.
    #[derive(Clone, Copy)]
    struct Job {
        value: u32,
        what: &'static str,
    }

    /// And an answer that says what it was about, as the real ones do.
    #[derive(Debug, Eq, PartialEq)]
    struct Done {
        doubled: u32,
        of: &'static str,
    }

    /// Doubling stands in for a store round trip: the point is *where* it ran,
    /// not what it computed.
    fn double(job: &Job) -> Done {
        Done {
            doubled: job.value * 2,
            of: job.what,
        }
    }

    /// A job, and the answer carrying it out gives.
    fn job() -> (Job, Done) {
        let job = Job {
            value: 21,
            what: "a settled edit",
        };
        (job, double(&job))
    }

    /// The property the whole fall-back exists for: with no worker the job is
    /// carried out on the caller's own thread and its answer is left exactly
    /// where the loop's collect looks, so there is one adopt path either way.
    #[test]
    fn a_stopped_worker_carries_the_job_out_on_the_caller() {
        let (job, done) = job();
        let worker: Worker<Job, Done> = Worker::new(double, WorkerWake::create());
        worker.stop();
        assert!(
            worker.submit(job),
            "a job nobody will take is run here, and says so"
        );
        assert_eq!(worker.collect(), Some(done));
        assert_eq!(worker.collect(), None, "an answer is collected once");
    }

    /// A running desk defers instead: the loop is told nothing is waiting, and
    /// nothing has been carried out on its thread.
    #[test]
    fn a_running_worker_defers_the_job() {
        let (job, _) = job();
        let worker: Worker<Job, Done> = Worker::new(double, WorkerWake::create());
        assert!(!worker.submit(job), "the job went to the desk");
        assert_eq!(
            worker.collect(),
            None,
            "and no answer was produced on this thread"
        );
    }

    /// The guard stops its worker, so work submitted after the scope that owns
    /// it has gone is still carried out rather than silently dropped.
    #[test]
    fn the_guard_stops_the_worker_it_holds() {
        let (job, done) = job();
        let worker: Arc<Worker<Job, Done>> = Arc::new(Worker::new(double, WorkerWake::create()));
        {
            let _guard = WorkerGuard::new(&worker);
            assert!(!worker.submit(job), "still running inside the scope");
        }
        assert!(worker.submit(job), "stopped, so this one runs here");
        assert_eq!(worker.collect(), Some(done));
    }

    /// A machine that grants no wake pipe grants no worker either: starting
    /// leaves the desk in the state that runs later submissions inline, so the
    /// caller only has to state why.
    #[test]
    fn a_start_without_a_wake_leaves_the_work_on_the_caller() {
        let (job, done) = job();
        let worker: Arc<Worker<Job, Done>> = Arc::new(Worker::new(double, WorkerWake::create()));
        // The host grants no pipe, which is exactly the refusal being modelled.
        assert!(!worker.wake().is_armed());
        assert!(matches!(Worker::start(&worker), Err(NoWorker::Wake)));
        assert!(worker.submit(job));
        assert_eq!(worker.collect(), Some(done));
    }
}
