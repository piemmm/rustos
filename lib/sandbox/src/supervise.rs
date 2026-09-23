//! A long-lived sandboxed worker that is **replaced** when it fails: the
//! duplex session seam ([`crate::session`]) under a supervisor.
//!
//! A plain session ends when its worker dies, because that worker held the
//! connection's protocol state. A worker that holds only what its owner can
//! hand a fresh one — a decoder of a stream of untrusted input keeping a
//! cache it can rebuild — should instead be replaced, with the one-shot
//! seam's discipline: a typed error to the owner, the dead worker reaped,
//! the event logged ([`crate::host::EVENT_WORKER_CRASHED`]), a replacement
//! started. [`SupervisedSession`] is that, without the request/reply shape,
//! so a stream never costs a round trip per item.
//!
//! # Replacement is paced
//!
//! What kills a streaming worker may be its input, and the sender chooses
//! both the input and its rate, so restarting at once would sell a process
//! spawn per crafted message. Each failure is paced through
//! [`RestartPacer`]: a delay doubling from 100 ms to a 30 s cap, forgotten
//! once a replacement stays up for 30 s. The owner arms one timer for
//! [`SupervisedSession::restart_deadline`]; nothing polls. A launch that
//! fails is paced the same way, so one that can never succeed costs an
//! attempt per capped interval and never a spin.
//!
//! # Generations
//!
//! Each worker the supervisor starts is a new generation, and
//! [`SupervisedSession::start`] reports it. The owner then discards whatever
//! it derived from the previous worker, sends the new one whatever state it
//! needs, and re-registers the descriptors, which belong to a new pipe pair.

use tairix_abi::Errno;
use tairix_log::Sink;
use tairix_util::retry::RestartPacer;

use crate::host::{log_unavailable, log_worker_crashed};
use crate::session::{
    SandboxSession, SessionBounds, SessionDescriptors, SessionError, SessionTransport,
};

/// The delay before a failed worker's first replacement.
const RESTART_BASE_NS: u64 = 100_000_000;

/// The longest a replacement waits however often its predecessors failed:
/// a worker killed on purpose costs its sender at most one spawn per
/// interval.
const RESTART_CAP_NS: u64 = 30_000_000_000;

/// How long a replacement must stay up before its predecessors' failures
/// are forgotten.
const RESTART_STABLE_NS: u64 = 30_000_000_000;

/// Starts the workers a [`SupervisedSession`] supervises.
///
/// The production launcher spawns the program's own binary in its session
/// worker role (`crate::rt`); host tests start in-process services
/// ([`crate::loopback::LoopbackSessionLauncher`]).
pub trait SessionLauncher {
    /// The transport connected to one launched worker.
    type Transport: SessionTransport;

    /// Start a fresh worker and return the transport to it.
    ///
    /// # Errors
    ///
    /// The typed reason the worker could not be started.
    fn launch(&mut self) -> Result<Self::Transport, Errno>;
}

/// A duplex session whose worker is replaced, after a paced delay, whenever
/// it fails.
///
/// Every I/O operation is the non-blocking [`SandboxSession`] one. Those
/// that can discover a failure take `now`, the monotonic nanosecond clock,
/// so the failure is paced from the instant it was seen.
/// [`SessionError::WorkerFailed`] means no worker is live: it was reaped
/// and logged, and a replacement is due at [`Self::restart_deadline`].
pub struct SupervisedSession<L: SessionLauncher, S: Sink + Clone> {
    launcher: L,
    sink: S,
    bounds: SessionBounds,
    pacer: RestartPacer,
    live: Option<SandboxSession<L::Transport, S>>,
    /// The earliest instant a start may be attempted while none is live.
    start_at: u64,
    generation: u64,
}

impl<L: SessionLauncher, S: Sink + Clone> SupervisedSession<L, S> {
    /// Supervise workers `launcher` starts, each admitted under `bounds`,
    /// logging containment to `sink`. No worker runs until the first
    /// [`Self::start`].
    pub fn new(launcher: L, bounds: SessionBounds, sink: S) -> Self {
        Self {
            launcher,
            sink,
            bounds,
            pacer: RestartPacer::new(RESTART_BASE_NS, RESTART_CAP_NS, RESTART_STABLE_NS),
            live: None,
            start_at: 0,
            generation: 0,
        }
    }

    /// Start a worker when none is live and the pacing allows one at `now`,
    /// returning the new worker's generation when one started.
    ///
    /// A launch that fails, or whose queues cannot be committed, is logged
    /// as [`crate::host::EVENT_WORKER_UNAVAILABLE`] and paced like a crash.
    pub fn start(&mut self, now: u64) -> Option<u64> {
        if self.live.is_some() || now < self.start_at {
            return None;
        }
        let admitted = match self.launcher.launch() {
            Ok(transport) => SandboxSession::supervised(transport, self.bounds, self.sink.clone())
                .map_err(|_| Errno::OutOfMemory),
            Err(errno) => Err(errno),
        };
        match admitted {
            Ok(session) => {
                self.live = Some(session);
                self.pacer.started(now);
                self.generation = self.generation.saturating_add(1);
                Some(self.generation)
            }
            Err(errno) => {
                log_unavailable(&self.sink, errno);
                self.start_at = self.pacer.failed(now).at;
                None
            }
        }
    }

    /// The instant [`Self::start`] should next be called, or `None` while a
    /// worker is live.
    #[must_use]
    pub fn restart_deadline(&self) -> Option<u64> {
        self.live.is_none().then_some(self.start_at)
    }

    /// Whether a worker is live.
    #[must_use]
    pub fn is_live(&self) -> bool {
        self.live.is_some()
    }

    /// The live worker's descriptors, or `None` when none is live or the
    /// transport is in-process.
    #[must_use]
    pub fn descriptors(&self) -> Option<SessionDescriptors> {
        self.live.as_ref()?.descriptors()
    }

    /// Whether the owner should arm write-room readiness.
    #[must_use]
    pub fn wants_write(&self) -> bool {
        self.live.as_ref().is_some_and(SandboxSession::wants_write)
    }

    /// Whether the owner should arm read readiness.
    #[must_use]
    pub fn wants_read(&self) -> bool {
        self.live.as_ref().is_some_and(SandboxSession::wants_read)
    }

    /// Queue one payload for the live worker ([`SandboxSession::send`]).
    ///
    /// # Errors
    ///
    /// [`SessionError::WorkerFailed`] when none is live, otherwise as
    /// [`SandboxSession::send`].
    pub fn send(&mut self, payload: &[u8]) -> Result<(), SessionError> {
        self.live
            .as_mut()
            .ok_or(SessionError::WorkerFailed)?
            .send(payload)
    }

    /// Take one transport read ([`SandboxSession::on_readable`]).
    ///
    /// # Errors
    ///
    /// [`SessionError::WorkerFailed`] when none is live or this read found
    /// the worker failed.
    pub fn on_readable(&mut self, now: u64) -> Result<(), SessionError> {
        let outcome = self
            .live
            .as_mut()
            .ok_or(SessionError::WorkerFailed)?
            .on_readable();
        self.settle(now, outcome)
    }

    /// Take one transport write ([`SandboxSession::on_writable`]).
    ///
    /// # Errors
    ///
    /// As [`Self::on_readable`].
    pub fn on_writable(&mut self, now: u64) -> Result<(), SessionError> {
        let outcome = self
            .live
            .as_mut()
            .ok_or(SessionError::WorkerFailed)?
            .on_writable();
        self.settle(now, outcome)
    }

    /// Lend the next complete frame the worker sent to `take`
    /// ([`SandboxSession::recv`]).
    ///
    /// A worker that closed its stream is a failed one here, once every frame
    /// it sent before closing has been taken: a supervised worker serves
    /// until its owner stops it.
    ///
    /// # Errors
    ///
    /// [`SessionError::WorkerFailed`] when none is live, the head frame broke
    /// the framing, or the worker has ended.
    pub fn recv<R>(
        &mut self,
        now: u64,
        take: impl FnOnce(&[u8]) -> R,
    ) -> Result<Option<R>, SessionError> {
        let session = self.live.as_mut().ok_or(SessionError::WorkerFailed)?;
        match session.recv(take) {
            Ok(None) if session.peer_finished() => {
                if let Some(ended) = self.live.take() {
                    log_worker_crashed(&self.sink, "worker ended its stream", ended.end());
                }
                self.start_at = self.pacer.failed(now).at;
                Err(SessionError::WorkerFailed)
            }
            outcome => self.settle(now, outcome),
        }
    }

    /// Contain the live worker because its owner could not believe what it
    /// sent: reaped, logged, and replaced after the paced delay, exactly as
    /// a framing violation.
    pub fn condemn(&mut self, now: u64, reason: &'static str) {
        if let Some(mut session) = self.live.take() {
            session.condemn(reason);
            self.start_at = self.pacer.failed(now).at;
        }
    }

    /// Retire the worker when `outcome` reports it failed; the session has
    /// already reaped and logged it.
    fn settle<T>(&mut self, now: u64, outcome: Result<T, SessionError>) -> Result<T, SessionError> {
        if matches!(outcome, Err(SessionError::WorkerFailed)) {
            self.live = None;
            self.start_at = self.pacer.failed(now).at;
        }
        outcome
    }
}

#[cfg(test)]
#[path = "supervise_tests.rs"]
mod tests;
