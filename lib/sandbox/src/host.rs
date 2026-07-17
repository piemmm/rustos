//! The parent side of the sandbox seam: request dispatch and crash
//! containment.
//!
//! [`ParserSandbox`] is what a calling program holds: it sends one request
//! payload to the sandboxed worker its [`Launcher`] started and returns the
//! reply payload. Every way the worker can fail — a crash, a protocol
//! violation, an oversize reply, an exit without replying — is contained
//! identically: the caller receives a typed [`SandboxError`], the dead
//! worker is disposed of (reaped) and **replaced**, and the event is logged
//! with a stable [`EventId`] so a crashing parser is observable. A parser
//! crash never takes down the calling program.
//!
//! The worker is treated as hostile from the moment it has parsed a byte:
//! nothing it sends is trusted beyond the framing bound here, and the typed
//! payload decoders above this layer (`crate::decode`) validate every
//! field fail-closed.

use tairix_abi::{Errno, FieldValue};
use tairix_log::{Event, EventId, Field, Level, Sink};

use alloc::vec::Vec;

use crate::proto::{recv_frame, send_frame, Channel, MAX_FRAME};

/// Stable event id: a sandboxed worker crashed or violated the protocol
/// mid-request and was disposed of and replaced.
///
/// `lib/sandbox` owns the `6_000..7_000` identifier range.
pub const EVENT_WORKER_CRASHED: EventId = EventId(6000);

/// Stable event id: a sandboxed worker could not be started (an initial
/// launch or a post-crash replacement failed).
pub const EVENT_WORKER_UNAVAILABLE: EventId = EventId(6001);

/// Starts sandboxed workers and reaps dead ones.
///
/// The production launcher spawns the program's own binary in a worker
/// role inside the kernel sandbox spawn mode over a fresh pipe pair
/// (`crate::rt`); host tests inject in-process fakes
/// (`crate::loopback`).
pub trait Launcher {
    /// The transport connected to one launched worker.
    type Channel: Channel;

    /// Start a fresh worker and return the channel to it.
    ///
    /// # Errors
    ///
    /// The typed reason the worker could not be started.
    fn launch(&mut self) -> Result<Self::Channel, Errno>;

    /// Tear down a worker whose channel failed: close the transport, reap
    /// the process, and report its exit code when one is known.
    fn dispose(&mut self, channel: Self::Channel) -> Option<i32>;
}

/// Typed failure a [`ParserSandbox::request`] can report.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SandboxError {
    /// No worker could be started; the carried errno names the launch
    /// failure.
    WorkerUnavailable(Errno),
    /// The worker crashed or violated the protocol mid-request. It has
    /// been disposed of and a replacement was started (or its failure to
    /// start was itself logged). The request was not answered.
    WorkerFailed,
    /// The request payload exceeds [`MAX_FRAME`]; nothing was sent.
    RequestTooLarge,
}

/// The parent-side seam: one sandboxed worker, one outstanding request.
///
/// The type deliberately serialises requests (send, then block for the
/// reply): a parse job is a synchronous question, and one-at-a-time keeps
/// the framing unambiguous with no request ids to validate.
pub struct ParserSandbox<L: Launcher, S: Sink> {
    launcher: L,
    sink: S,
    live: Option<L::Channel>,
}

impl<L: Launcher, S: Sink> ParserSandbox<L, S> {
    /// Build the seam over `launcher`, logging containment events to
    /// `sink`. No worker is started until the first request needs one.
    pub fn new(launcher: L, sink: S) -> Self {
        Self {
            launcher,
            sink,
            live: None,
        }
    }

    /// Send one request payload and return the worker's reply payload.
    ///
    /// On any worker failure the error path runs the full containment
    /// discipline before returning: dispose (reap), log
    /// [`EVENT_WORKER_CRASHED`] with the exit code when known, and start a
    /// replacement worker so the *next* request finds a live sandbox (a
    /// replacement that fails to start is logged as
    /// [`EVENT_WORKER_UNAVAILABLE`] and retried lazily on the next
    /// request).
    ///
    /// # Errors
    ///
    /// [`SandboxError`], as above. The failed request is never retried
    /// automatically: the caller decides whether the parse mattered.
    pub fn request(&mut self, payload: &[u8]) -> Result<Vec<u8>, SandboxError> {
        if payload.len() > MAX_FRAME {
            return Err(SandboxError::RequestTooLarge);
        }
        let channel = if let Some(channel) = self.live.as_mut() {
            channel
        } else {
            let launched = self.launcher.launch().map_err(|errno| {
                self.log_unavailable(errno);
                SandboxError::WorkerUnavailable(errno)
            })?;
            self.live.insert(launched)
        };
        let outcome = send_frame(channel, payload).and_then(|()| recv_frame(channel));
        if let Ok(Some(reply)) = outcome {
            // A reply arrived; the worker stays live for the next request.
            return Ok(reply);
        }
        // The worker exited without answering, died mid-frame, declared an
        // oversize reply, or the transport failed: all are the same
        // containment path — the worker is gone or can no longer be
        // believed.
        self.contain_failure();
        Err(SandboxError::WorkerFailed)
    }

    /// Dispose of the failed worker, log the crash, and start the
    /// replacement.
    fn contain_failure(&mut self) {
        let exit_code = self
            .live
            .take()
            .and_then(|channel| self.launcher.dispose(channel));
        let exit_field = match exit_code {
            Some(code) => FieldValue::SignedInt(i64::from(code)),
            None => FieldValue::Null,
        };
        tairix_log::log(
            &self.sink,
            &Event {
                level: Level::Warn,
                id: EVENT_WORKER_CRASHED,
                message: "parser sandbox worker crashed; replaced",
                fields: &[Field {
                    key: "exit_code",
                    value: exit_field,
                }],
            },
        );
        match self.launcher.launch() {
            Ok(channel) => self.live = Some(channel),
            Err(errno) => self.log_unavailable(errno),
        }
    }

    /// Log a failed launch (initial or replacement).
    fn log_unavailable(&self, errno: Errno) {
        log_unavailable_to(&self.sink, errno);
    }
}

impl<L: Launcher, S: Sink> Drop for ParserSandbox<L, S> {
    fn drop(&mut self) {
        // A live worker is shut down through the launcher (transport
        // closed, process reaped), so no worker outlives its seam.
        if let Some(channel) = self.live.take() {
            let _ = self.launcher.dispose(channel);
        }
    }
}

/// Emit the [`EVENT_WORKER_UNAVAILABLE`] event to `sink`.
fn log_unavailable_to<S: Sink>(sink: &S, errno: Errno) {
    tairix_log::log(
        sink,
        &Event {
            level: Level::Error,
            id: EVENT_WORKER_UNAVAILABLE,
            message: "parser sandbox worker could not be started",
            fields: &[Field {
                key: "error",
                value: FieldValue::Error(errno),
            }],
        },
    );
}

#[cfg(test)]
mod tests {
    use super::{
        Launcher, ParserSandbox, SandboxError, EVENT_WORKER_CRASHED, EVENT_WORKER_UNAVAILABLE,
    };
    use crate::proto::{Channel, MAX_FRAME};
    use alloc::rc::Rc;
    use alloc::vec;
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use tairix_abi::Errno;
    use tairix_log::{Event, EventId, Level, Sink};

    /// Captures `(id, level)` pairs of every logged event.
    #[derive(Clone, Default)]
    struct RecordingSink {
        events: Rc<RefCell<Vec<(EventId, Level)>>>,
    }

    impl Sink for RecordingSink {
        fn write_event(&self, event: &Event<'_>) {
            self.events.borrow_mut().push((event.id, event.level));
        }
    }

    /// One scripted worker: answers every request with `reply` until
    /// `answers` runs out, then reports end-of-stream (the worker "died").
    struct ScriptedChannel {
        reply: Vec<u8>,
        answers: usize,
        pending: Vec<u8>,
        at: usize,
    }

    impl Channel for ScriptedChannel {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, Errno> {
            if self.at == self.pending.len() {
                if self.answers == 0 {
                    return Ok(0);
                }
                self.answers -= 1;
                let mut framed = Vec::new();
                framed.extend_from_slice(
                    &u32::try_from(self.reply.len())
                        .expect("small reply")
                        .to_le_bytes(),
                );
                framed.extend_from_slice(&self.reply);
                self.pending = framed;
                self.at = 0;
            }
            let take = buf.len().min(self.pending.len() - self.at);
            buf[..take].copy_from_slice(&self.pending[self.at..self.at + take]);
            self.at += take;
            Ok(take)
        }

        fn write(&mut self, buf: &[u8]) -> Result<usize, Errno> {
            Ok(buf.len())
        }
    }

    /// Launcher whose successive `launch` calls produce the scripted
    /// workers; records launches and disposals.
    struct ScriptedLauncher {
        scripts: Vec<Result<ScriptedChannel, Errno>>,
        launched: usize,
        disposed: usize,
    }

    impl Launcher for ScriptedLauncher {
        type Channel = ScriptedChannel;

        fn launch(&mut self) -> Result<ScriptedChannel, Errno> {
            self.launched += 1;
            if self.scripts.is_empty() {
                return Err(Errno::NotFound);
            }
            self.scripts.remove(0)
        }

        fn dispose(&mut self, _channel: ScriptedChannel) -> Option<i32> {
            self.disposed += 1;
            Some(139)
        }
    }

    fn worker(answers: usize) -> ScriptedChannel {
        ScriptedChannel {
            reply: b"ok".to_vec(),
            answers,
            pending: Vec::new(),
            at: 0,
        }
    }

    #[test]
    fn a_reply_flows_back_and_the_worker_is_reused() {
        let launcher = ScriptedLauncher {
            scripts: vec![Ok(worker(2))],
            launched: 0,
            disposed: 0,
        };
        let sink = RecordingSink::default();
        let mut sandbox = ParserSandbox::new(launcher, sink.clone());
        assert_eq!(sandbox.request(b"one"), Ok(b"ok".to_vec()));
        assert_eq!(sandbox.request(b"two"), Ok(b"ok".to_vec()));
        assert_eq!(sandbox.launcher.launched, 1);
        assert!(sink.events.borrow().is_empty());
    }

    #[test]
    fn a_dead_worker_is_contained_logged_and_replaced() {
        // First worker answers once then dies; the replacement answers.
        let launcher = ScriptedLauncher {
            scripts: vec![Ok(worker(1)), Ok(worker(1))],
            launched: 0,
            disposed: 0,
        };
        let sink = RecordingSink::default();
        let mut sandbox = ParserSandbox::new(launcher, sink.clone());

        assert_eq!(sandbox.request(b"one"), Ok(b"ok".to_vec()));
        // The worker's stream ends before this reply: typed failure...
        assert_eq!(sandbox.request(b"two"), Err(SandboxError::WorkerFailed));
        // ...the dead worker was reaped, the crash logged with the stable
        // id, and a replacement started eagerly.
        assert_eq!(sandbox.launcher.disposed, 1);
        assert_eq!(sandbox.launcher.launched, 2);
        assert_eq!(
            sink.events.borrow().as_slice(),
            &[(EVENT_WORKER_CRASHED, Level::Warn)]
        );
        // The caller survives and the replacement serves the next request.
        assert_eq!(sandbox.request(b"three"), Ok(b"ok".to_vec()));
        assert_eq!(sandbox.launcher.launched, 2);
    }

    #[test]
    fn a_failed_initial_launch_is_typed_and_logged() {
        let launcher = ScriptedLauncher {
            scripts: vec![Err(Errno::PermissionDenied)],
            launched: 0,
            disposed: 0,
        };
        let sink = RecordingSink::default();
        let mut sandbox = ParserSandbox::new(launcher, sink.clone());
        assert_eq!(
            sandbox.request(b"one"),
            Err(SandboxError::WorkerUnavailable(Errno::PermissionDenied))
        );
        assert_eq!(
            sink.events.borrow().as_slice(),
            &[(EVENT_WORKER_UNAVAILABLE, Level::Error)]
        );
    }

    #[test]
    fn a_failed_replacement_is_logged_and_retried_on_the_next_request() {
        // One worker that dies immediately; no replacement available; then
        // a later launch succeeds.
        let launcher = ScriptedLauncher {
            scripts: vec![Ok(worker(0)), Err(Errno::OutOfMemory), Ok(worker(1))],
            launched: 0,
            disposed: 0,
        };
        let sink = RecordingSink::default();
        let mut sandbox = ParserSandbox::new(launcher, sink.clone());

        assert_eq!(sandbox.request(b"one"), Err(SandboxError::WorkerFailed));
        assert_eq!(
            sink.events.borrow().as_slice(),
            &[
                (EVENT_WORKER_CRASHED, Level::Warn),
                (EVENT_WORKER_UNAVAILABLE, Level::Error),
            ]
        );
        // The lazy retry on the next request finds the working script.
        assert_eq!(sandbox.request(b"two"), Ok(b"ok".to_vec()));
    }

    #[test]
    fn an_oversize_request_is_refused_before_any_launch() {
        let launcher = ScriptedLauncher {
            scripts: vec![Ok(worker(1))],
            launched: 0,
            disposed: 0,
        };
        let sink = RecordingSink::default();
        let mut sandbox = ParserSandbox::new(launcher, sink.clone());
        let oversize = vec![0u8; MAX_FRAME + 1];
        assert_eq!(
            sandbox.request(&oversize),
            Err(SandboxError::RequestTooLarge)
        );
        assert_eq!(sandbox.launcher.launched, 0);
        assert!(sink.events.borrow().is_empty());
    }

    #[test]
    fn dropping_the_seam_disposes_the_live_worker() {
        /// Launcher that records disposals somewhere the test can still
        /// see after the seam (which owns the launcher) is dropped.
        struct CountingLauncher {
            disposed: Rc<RefCell<usize>>,
        }

        impl Launcher for CountingLauncher {
            type Channel = ScriptedChannel;

            fn launch(&mut self) -> Result<ScriptedChannel, Errno> {
                Ok(worker(1))
            }

            fn dispose(&mut self, _channel: ScriptedChannel) -> Option<i32> {
                *self.disposed.borrow_mut() += 1;
                None
            }
        }

        let disposed = Rc::new(RefCell::new(0));
        let launcher = CountingLauncher {
            disposed: disposed.clone(),
        };
        let mut sandbox = ParserSandbox::new(launcher, RecordingSink::default());
        assert_eq!(sandbox.request(b"one"), Ok(b"ok".to_vec()));
        drop(sandbox);
        // The healthy live worker was shut down through the launcher.
        assert_eq!(*disposed.borrow(), 1);
    }

    #[test]
    fn the_event_ids_are_frozen() {
        // The identifiers are a contract with log consumers; renumbering
        // them is an ABI break this test refuses.
        assert_eq!(EVENT_WORKER_CRASHED, EventId(6000));
        assert_eq!(EVENT_WORKER_UNAVAILABLE, EventId(6001));
    }
}
