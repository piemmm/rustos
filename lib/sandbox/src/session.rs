//! The duplex, long-lived worker seam: a *session* over a sandboxed
//! worker, beside the one-shot [`crate::host`] / [`crate::worker`] pair.
//!
//! [`crate::host::ParserSandbox`] is right for a parse — a synchronous
//! question with an idempotent answer — and wrong for a protocol
//! connection, where either side originates, one inbound frame may produce
//! zero or many outbound ones, and the owner multiplexes many such
//! conversations on one wait-set. Three differences make a session its own
//! shape rather than a flag on the request path:
//!
//! 1. **Not request/reply-locked.** Many frames are in flight each way.
//! 2. **The parent never blocks.** Every transport operation is one
//!    syscall, taken only when the owner's readiness source said so, so one
//!    slow peer cannot stall the others.
//! 3. **A crashed worker ends the session; it is never replaced.** A parse
//!    is idempotent, so `ParserSandbox` restarts its worker. A session
//!    worker holds the protocol state — keys, sequence numbers — so a
//!    silent replacement would be a correctness hole, not resilience.
//!
//! # Why this cannot deadlock
//!
//! The worker is a pure reactor, and the kernel forces it to be: the
//! sandbox syscall allow-list (`docs/src/security/sandbox.md`) has no
//! wait-set call, no clock, and no RNG, so the pipe is the only thing that
//! can ever wake it. It may therefore use the ordinary **blocking**
//! [`crate::proto::Channel`], and the invariant that makes that safe is
//! that the parent never blocks:
//!
//! * a worker blocked writing is woken because the parent's read readiness
//!   fires and it drains;
//! * a worker blocked reading is woken because the parent's *room*
//!   readiness fires and it writes.
//!
//! Both legs need their wake source, which is why the owner registers
//! [`tairix_abi::WaitSourceKind::Stream`] on the reply descriptor **and**
//! [`tairix_abi::WaitSourceKind::StreamRoom`] on the request descriptor
//! ([`SandboxSession::descriptors`]). Polling for either is forbidden.
//!
//! # Driving it
//!
//! The owner arms each member according to [`SandboxSession::wants_read`]
//! and [`SandboxSession::wants_write`], and on a wake calls the matching
//! `on_readable` / `on_writable`, then drains [`SandboxSession::recv`]
//! until it yields nothing. Disarming a member the session does not want is
//! what keeps a level-triggered readiness source from spinning, and
//! `wants_read` going false is the back-pressure that fills the pipe and
//! blocks the worker.

use alloc::vec::Vec;

use tairix_abi::{Errno, FieldValue};
use tairix_collections::ByteQueue;
use tairix_log::{Event, EventId, Field, Level, Sink};

use crate::host::log_worker_crashed;
use crate::proto::{
    head_declared, head_frame, recv_frame_into, send_frame, Channel, ProtoError, FRAME_HEADER_LEN,
    MAX_FRAME,
};
use crate::worker::ServeEnd;

/// Stable event id: a session worker crashed, violated the framing, or its
/// transport failed. The worker was disposed of and **not** replaced.
///
/// `lib/sandbox` owns the `6_000..7_000` identifier range.
pub const EVENT_SESSION_FAILED: EventId = EventId(6002);

/// Smallest queue bound a session can work with: one frame header plus a
/// payload byte. Below it no frame carrying anything could ever cross, so
/// the session could never make progress.
pub const MIN_QUEUE_BYTES: usize = FRAME_HEADER_LEN + 1;

/// The two descriptor numbers a production transport occupies in the
/// owner's own table, so the owner can register them with its readiness
/// source.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SessionDescriptors {
    /// The worker→owner stream: register for read readiness
    /// ([`tairix_abi::WaitSourceKind::Stream`]).
    pub read_fd: u32,
    /// The owner→worker stream: register for write-room readiness
    /// ([`tairix_abi::WaitSourceKind::StreamRoom`]).
    pub write_fd: u32,
}

/// The transport one session runs over.
///
/// Deliberately **not** [`crate::proto::Channel`]: that contract is
/// blocking, and a blocking channel handed to a session would be a latent
/// hang the type system would not catch. Every method here performs at most
/// one transport operation and never parks, because the owner calls it only
/// when its readiness source said the operation would complete.
pub trait SessionTransport: Sized {
    /// Perform **exactly one** transport read. `Ok(0)` is end-of-stream:
    /// the owner calls this only when the read side is ready, so an empty
    /// result means the peer closed rather than that nothing had arrived.
    ///
    /// # Errors
    ///
    /// The transport's typed failure. [`Errno::WouldBlock`] is the one the
    /// session treats as "nothing right now" rather than a failure, so a
    /// spurious readiness report — or a fake with no readiness to consult
    /// — costs nothing.
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Errno>;

    /// Perform **exactly one** transport write and report how many bytes
    /// were accepted; a short write is normal. The owner calls this only
    /// when the write side has room, so a zero-byte result for a non-empty
    /// `buf` means the peer is gone.
    ///
    /// # Errors
    ///
    /// The transport's typed failure (e.g. [`Errno::BrokenPipe`]);
    /// [`Errno::WouldBlock`] leaves the queue untouched, as for
    /// [`Self::read`].
    fn write(&mut self, buf: &[u8]) -> Result<usize, Errno>;

    /// The owner's descriptor numbers for the two directions, or `None` for
    /// an in-process fake that occupies no descriptor.
    fn descriptors(&self) -> Option<SessionDescriptors>;

    /// Close the transport and reap the worker, reporting its exit code
    /// when one is known.
    fn dispose(self) -> Option<i32>;
}

/// Typed failure a session operation can report.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SessionError {
    /// The worker crashed, violated the framing, or the transport failed,
    /// and has been disposed of (reaped). A plain session is over — its
    /// worker held the protocol state — and every later call reports the
    /// same error without touching the transport; a supervised one
    /// ([`crate::supervise`]) starts a replacement after its paced delay.
    WorkerFailed,
    /// The payload is larger than this session's outbound bound could ever
    /// carry. **Permanent**: nothing was queued and a retry cannot succeed.
    FrameTooLarge,
    /// The outbound queue has no room for this frame right now.
    /// **Transient**: nothing was queued, and the frame fits an empty
    /// queue, so the owner can stop producing, drain, and retry.
    OutboundFull,
    /// A queue bound below [`MIN_QUEUE_BYTES`]. Refused at
    /// [`SessionBounds::new`].
    BoundTooSmall,
    /// The session's queues could not be committed. Refused at
    /// [`SandboxSession::new`], which disposes of the worker it was handed.
    OutOfMemory,
}

/// How much a session may hold queued in each direction.
///
/// These are **containment bounds**, not capacities that grow: they bound
/// what one session costs a machine serving many of them at once, and both
/// are committed up front so the cost is known when the session is admitted
/// rather than discovered when the memory is gone.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SessionBounds {
    outbound: usize,
    inbound: usize,
}

impl SessionBounds {
    /// Bound a session to `outbound_bytes` of queued frames toward the
    /// worker and `inbound_bytes` of frames read back from it.
    ///
    /// # Errors
    ///
    /// [`SessionError::BoundTooSmall`] for either bound below
    /// [`MIN_QUEUE_BYTES`].
    pub const fn new(outbound_bytes: usize, inbound_bytes: usize) -> Result<Self, SessionError> {
        if outbound_bytes < MIN_QUEUE_BYTES || inbound_bytes < MIN_QUEUE_BYTES {
            return Err(SessionError::BoundTooSmall);
        }
        Ok(Self {
            outbound: outbound_bytes,
            inbound: inbound_bytes,
        })
    }

    /// Bytes of framed traffic the outbound queue holds.
    #[must_use]
    pub const fn outbound_bytes(self) -> usize {
        self.outbound
    }

    /// Bytes of framed traffic the inbound accumulator holds.
    #[must_use]
    pub const fn inbound_bytes(self) -> usize {
        self.inbound
    }

    /// Largest payload [`SandboxSession::send`] accepts: what an empty
    /// outbound queue holds once its header is paid for, never above
    /// [`MAX_FRAME`].
    ///
    /// Deriving it from the bound is what makes
    /// [`SessionError::OutboundFull`] *transient* — an accepted payload
    /// always fits an empty queue, so a refusal is back-pressure rather
    /// than a frame that can never be sent.
    #[must_use]
    pub const fn max_send_payload(self) -> usize {
        let by_queue = self.outbound - FRAME_HEADER_LEN;
        if by_queue < MAX_FRAME {
            by_queue
        } else {
            MAX_FRAME
        }
    }

    /// Largest payload this session admits *from* the worker. A frame
    /// declaring more is a protocol violation and contains the session:
    /// the owner's configured inbound bound is the ceiling its service must
    /// respect, exactly as a one-shot service respects its documented reply
    /// caps.
    #[must_use]
    pub const fn max_recv_payload(self) -> usize {
        let by_queue = self.inbound - FRAME_HEADER_LEN;
        if by_queue < MAX_FRAME {
            by_queue
        } else {
            MAX_FRAME
        }
    }
}

/// Whether `pending` divides exactly into whole frames with nothing over —
/// the end-of-stream cleanliness test. A stream that ends mid-frame is a
/// truncated conversation, never silently shortened data.
fn whole_frames(pending: &[u8]) -> bool {
    let mut at = 0;
    while at < pending.len() {
        match head_frame(&pending[at..]) {
            Some(payload) => at += FRAME_HEADER_LEN + payload,
            None => return false,
        }
    }
    true
}

/// What a contained worker's failure means, which decides the event it is
/// recorded under.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum AfterFailure {
    /// The session ends: its worker held protocol state a fresh one could not
    /// continue.
    End,
    /// A supervisor replaces the worker ([`crate::supervise`]).
    Replace,
}

/// The parent side of a duplex session: one sandboxed worker, frames in
/// flight both ways, and a single containment path.
///
/// Every operation is non-blocking. The owner drives it from its own
/// readiness source (a wait-set over [`Self::descriptors`]) and never waits
/// inside the session, so one session can never stall another.
pub struct SandboxSession<T: SessionTransport, S: Sink> {
    /// The live transport. `None` once the session has been contained;
    /// this worker is never revived, though a supervisor may start another.
    transport: Option<T>,
    sink: S,
    bounds: SessionBounds,
    outbound: ByteQueue,
    inbound: ByteQueue,
    /// Set when the worker closed its reply stream on a frame boundary.
    peer_finished: bool,
    /// Set by containment. Implies `transport` is `None`.
    failed: bool,
    after_failure: AfterFailure,
}

impl<T: SessionTransport, S: Sink> SandboxSession<T, S> {
    /// Build the seam over a live `transport`, committing both queues.
    ///
    /// There is no launcher: a session is one worker for its lifetime, so
    /// starting it is the transport's own constructor and reaping it is the
    /// transport's `dispose`. A launch that failed before this point is
    /// logged by the owner through
    /// [`crate::host::log_unavailable`].
    ///
    /// # Errors
    ///
    /// [`SessionError::OutOfMemory`] when the queues cannot be committed;
    /// the worker is disposed of rather than left running unreachable.
    pub fn new(transport: T, bounds: SessionBounds, sink: S) -> Result<Self, SessionError> {
        Self::admit(transport, bounds, sink, AfterFailure::End)
    }

    /// As [`Self::new`], for a worker a supervisor replaces when it fails:
    /// its containment is recorded as a crash to be replaced rather than a
    /// session ended.
    pub(crate) fn supervised(
        transport: T,
        bounds: SessionBounds,
        sink: S,
    ) -> Result<Self, SessionError> {
        Self::admit(transport, bounds, sink, AfterFailure::Replace)
    }

    fn admit(
        transport: T,
        bounds: SessionBounds,
        sink: S,
        after_failure: AfterFailure,
    ) -> Result<Self, SessionError> {
        let (Ok(outbound), Ok(inbound)) = (
            ByteQueue::committed(bounds.outbound_bytes()),
            ByteQueue::committed(bounds.inbound_bytes()),
        ) else {
            let _ = transport.dispose();
            return Err(SessionError::OutOfMemory);
        };
        Ok(Self {
            transport: Some(transport),
            sink,
            bounds,
            outbound,
            inbound,
            peer_finished: false,
            failed: false,
            after_failure,
        })
    }

    /// The bounds this session was admitted under.
    #[must_use]
    pub fn bounds(&self) -> SessionBounds {
        self.bounds
    }

    /// The owner's descriptor numbers for the two directions, or `None` for
    /// an in-process fake and for a contained session.
    #[must_use]
    pub fn descriptors(&self) -> Option<SessionDescriptors> {
        self.transport.as_ref()?.descriptors()
    }

    /// Queue one payload for the worker.
    ///
    /// The whole frame is checked against the remaining room *before* any
    /// of it is written, so a refusal leaves the queue exactly as it was —
    /// never half a frame.
    ///
    /// # Errors
    ///
    /// [`SessionError::WorkerFailed`] once contained;
    /// [`SessionError::FrameTooLarge`] for a payload above
    /// [`SessionBounds::max_send_payload`] (permanent);
    /// [`SessionError::OutboundFull`] when the queue is short of room
    /// (transient — drain and retry).
    pub fn send(&mut self, payload: &[u8]) -> Result<(), SessionError> {
        if self.failed {
            return Err(SessionError::WorkerFailed);
        }
        if payload.len() > self.bounds.max_send_payload() {
            return Err(SessionError::FrameTooLarge);
        }
        // The bound above keeps the length in `u32` range on every target.
        let Ok(declared) = u32::try_from(payload.len()) else {
            return Err(SessionError::FrameTooLarge);
        };
        // A committed queue never allocates, so the only refusal is room.
        let Ok(slot) = self.outbound.append_slot(FRAME_HEADER_LEN + payload.len()) else {
            return Err(SessionError::OutboundFull);
        };
        slot[..FRAME_HEADER_LEN].copy_from_slice(&declared.to_le_bytes());
        slot[FRAME_HEADER_LEN..].copy_from_slice(payload);
        Ok(())
    }

    /// Whether the owner should arm write-room readiness: bytes are queued
    /// for the worker.
    #[must_use]
    pub fn wants_write(&self) -> bool {
        !self.failed && !self.outbound.is_empty()
    }

    /// Whether the owner should arm read readiness: the worker's stream is
    /// still open and the inbound accumulator has room.
    ///
    /// Going false is the back-pressure: the owner disarms, the pipe fills,
    /// and the kernel blocks the worker's write until [`Self::recv`] makes
    /// room again.
    #[must_use]
    pub fn wants_read(&self) -> bool {
        !self.failed && !self.peer_finished && self.inbound.room() > 0
    }

    /// Take one transport read into the inbound accumulator.
    ///
    /// Called when the owner's read readiness fired. A spurious call the
    /// session does not want is a no-op rather than a read that could park.
    ///
    /// # Errors
    ///
    /// [`SessionError::WorkerFailed`] when the transport failed, the worker
    /// declared a frame above [`SessionBounds::max_recv_payload`], or its
    /// stream ended part-way through a frame. All three contain the session
    /// (dispose, log, latch).
    pub fn on_readable(&mut self) -> Result<(), SessionError> {
        if self.failed {
            return Err(SessionError::WorkerFailed);
        }
        if !self.wants_read() {
            return Ok(());
        }
        let outcome = {
            let Self {
                transport, inbound, ..
            } = self;
            match transport.as_mut() {
                Some(transport) => inbound.fill(|buf| transport.read(buf)),
                None => Err(Errno::BrokenPipe),
            }
        };
        match outcome {
            Ok(0) => {
                if whole_frames(self.inbound.pending()) {
                    self.peer_finished = true;
                    Ok(())
                } else {
                    Err(self.contain("worker stream ended mid-frame"))
                }
            }
            Ok(_) => self.check_inbound_head(),
            // "Nothing right now", not a failure: a level-triggered
            // readiness source can report a stream ready whose bytes
            // something else already took, and an in-process fake has no
            // readiness to consult at all.
            Err(Errno::WouldBlock) => Ok(()),
            Err(errno) => Err(self.contain(transport_reason(errno))),
        }
    }

    /// Take one transport write from the outbound queue.
    ///
    /// Called when the owner's write-room readiness fired. A short write is
    /// normal: what was accepted leaves the queue and the rest waits for
    /// the next wake.
    ///
    /// # Errors
    ///
    /// [`SessionError::WorkerFailed`] when the transport failed or made no
    /// progress on a non-empty queue (the worker is gone); the session is
    /// contained.
    pub fn on_writable(&mut self) -> Result<(), SessionError> {
        if self.failed {
            return Err(SessionError::WorkerFailed);
        }
        if self.outbound.is_empty() {
            return Ok(());
        }
        let outcome = {
            let Self {
                transport,
                outbound,
                ..
            } = self;
            match transport.as_mut() {
                Some(transport) => transport.write(outbound.pending()),
                None => Err(Errno::BrokenPipe),
            }
        };
        match outcome {
            // A write that accepts nothing cannot make progress; the peer
            // is gone rather than momentarily short of room.
            Ok(0) => Err(self.contain("worker accepted no bytes")),
            Ok(wrote) => {
                self.outbound.consume(wrote);
                Ok(())
            }
            // A spurious room report leaves the queue exactly as it was.
            Err(Errno::WouldBlock) => Ok(()),
            Err(errno) => Err(self.contain(transport_reason(errno))),
        }
    }

    /// Lend the next complete frame the worker sent to `take` and consume
    /// it, or `None` when the accumulator does not yet hold one.
    ///
    /// The payload is read in place, so taking a frame never allocates: an
    /// event loop cannot be left holding a frame it has no memory to take.
    ///
    /// # Errors
    ///
    /// [`SessionError::WorkerFailed`] once contained, and when the frame at
    /// the head declares more than [`SessionBounds::max_recv_payload`] —
    /// refused before a payload byte is read, and the session contained.
    pub fn recv<R>(&mut self, take: impl FnOnce(&[u8]) -> R) -> Result<Option<R>, SessionError> {
        if self.failed {
            return Err(SessionError::WorkerFailed);
        }
        self.check_inbound_head()?;
        let Some(payload_len) = head_frame(self.inbound.pending()) else {
            return Ok(None);
        };
        let frame_len = FRAME_HEADER_LEN + payload_len;
        let taken = take(&self.inbound.pending()[FRAME_HEADER_LEN..frame_len]);
        self.inbound.consume(frame_len);
        Ok(Some(taken))
    }

    /// Whether the worker closed its reply stream on a frame boundary. Any
    /// frames already accumulated are still drainable through
    /// [`Self::recv`]; the owner then [`Self::end`]s the session.
    #[must_use]
    pub fn peer_finished(&self) -> bool {
        self.peer_finished
    }

    /// Close the transport and reap the worker, reporting its exit code
    /// when one is known. A contained session has already been reaped and
    /// reports `None`.
    #[must_use]
    pub fn end(mut self) -> Option<i32> {
        self.transport.take().and_then(SessionTransport::dispose)
    }

    /// Refuse a head frame the worker declared above this session's inbound
    /// ceiling — before a payload byte of it is copied, and without waiting
    /// for the rest of it to arrive.
    fn check_inbound_head(&mut self) -> Result<(), SessionError> {
        match head_declared(self.inbound.pending()) {
            Some(declared) if declared > self.bounds.max_recv_payload() => {
                Err(self.contain("worker declared an oversize frame"))
            }
            _ => Ok(()),
        }
    }

    /// Contain the worker because its owner found what it sent unbelievable,
    /// exactly as a framing violation is contained.
    pub(crate) fn condemn(&mut self, reason: &'static str) {
        let _ = self.contain(reason);
    }

    /// Contain a failed session: dispose of the worker (reaping it), log
    /// the stable event, and latch so every later call refuses without
    /// touching the transport. This worker is never revived: it held the
    /// session's protocol state, so only a supervisor that can re-establish
    /// that state may start a fresh one.
    fn contain(&mut self, reason: &'static str) -> SessionError {
        if self.failed {
            return SessionError::WorkerFailed;
        }
        self.failed = true;
        let exit_code = self.transport.take().and_then(SessionTransport::dispose);
        match self.after_failure {
            AfterFailure::Replace => log_worker_crashed(&self.sink, reason, exit_code),
            AfterFailure::End => {
                let exit_field = match exit_code {
                    Some(code) => FieldValue::SignedInt(i64::from(code)),
                    None => FieldValue::Null,
                };
                tairix_log::log(
                    &self.sink,
                    &Event {
                        level: Level::Warn,
                        id: EVENT_SESSION_FAILED,
                        message: "sandbox session worker failed; session ended",
                        fields: &[
                            Field {
                                key: "reason",
                                value: FieldValue::Str(reason),
                            },
                            Field {
                                key: "exit_code",
                                value: exit_field,
                            },
                        ],
                    },
                );
            }
        }
        SessionError::WorkerFailed
    }
}

impl<T: SessionTransport, S: Sink> Drop for SandboxSession<T, S> {
    fn drop(&mut self) {
        // A live worker is reaped through the transport, so none outlives
        // its session.
        if let Some(transport) = self.transport.take() {
            let _ = transport.dispose();
        }
    }
}

/// The containment reason a transport errno names.
fn transport_reason(errno: Errno) -> &'static str {
    if errno == Errno::BrokenPipe {
        "worker transport closed"
    } else {
        "worker transport failed"
    }
}

/// How a [`SessionService`] emits frames back to the parent.
///
/// A frame is written straight to the transport, so a service that answers
/// one inbound frame with many outbound ones allocates nothing per frame.
pub trait FrameOut {
    /// Frame and write one payload.
    ///
    /// # Errors
    ///
    /// The framing or transport failure. The serve loop ends on the first
    /// one, so a service may propagate it or simply stop.
    fn frame(&mut self, payload: &[u8]) -> Result<(), ProtoError>;
}

/// Whether a served frame leaves the session open.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SessionStep {
    /// Keep serving.
    Continue,
    /// The service closed the session: the loop ends and the worker exits.
    Finished,
}

/// One long-lived protocol a session worker can serve.
///
/// Unlike [`crate::worker::Service`] a handler is not obliged to answer,
/// and may answer more than once: it emits whatever the protocol calls for
/// through `out`. It is still **total** — a malformed request is a typed
/// error *frame* or a [`SessionStep::Finished`], never a panic.
pub trait SessionService {
    /// Handle one inbound frame, emitting zero or more outbound ones.
    fn handle(&mut self, request: &[u8], out: &mut dyn FrameOut) -> SessionStep;
}

/// The channel-backed [`FrameOut`], latching its first failure so the loop
/// stops on it rather than writing into a dead transport.
struct ChannelOut<'a, C: Channel> {
    chan: &'a mut C,
    failure: Option<ProtoError>,
}

impl<C: Channel> FrameOut for ChannelOut<'_, C> {
    fn frame(&mut self, payload: &[u8]) -> Result<(), ProtoError> {
        if let Some(failure) = self.failure {
            return Err(failure);
        }
        match send_frame(self.chan, payload) {
            Ok(()) => Ok(()),
            Err(err) => {
                self.failure = Some(err);
                Err(err)
            }
        }
    }
}

/// Serve a session until the parent closes the request stream, the service
/// closes the session, or the transport fails.
///
/// The channel is the ordinary blocking [`Channel`]: the worker is a pure
/// reactor whose only wake source is this pipe, and the parent's
/// never-blocking seam is what keeps both directions moving (see the module
/// documentation).
pub fn serve_session<C: Channel, S: SessionService>(chan: &mut C, service: &mut S) -> ServeEnd {
    let mut out = ChannelOut {
        chan,
        failure: None,
    };
    // One buffer for the life of the session: a streaming worker is fed at
    // whatever rate its input arrives, so allocating per frame is a cost
    // the sender would choose.
    let mut request = Vec::new();
    loop {
        match recv_frame_into(&mut *out.chan, &mut request) {
            Ok(true) => {}
            Ok(false) => return ServeEnd::Finished,
            Err(err) => return ServeEnd::Failed(err),
        }
        let step = service.handle(&request, &mut out);
        if let Some(failure) = out.failure {
            return ServeEnd::Failed(failure);
        }
        if step == SessionStep::Finished {
            return ServeEnd::Ended;
        }
    }
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
