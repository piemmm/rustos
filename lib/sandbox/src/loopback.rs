//! In-process fake transport: the host-test double for the sandbox seam.
//!
//! [`LoopbackLauncher`] plays the [`crate::host::Launcher`] role without a
//! process: each "worker" is a fresh [`crate::worker::Service`] instance
//! from the injected factory, and the [`LoopbackChannel`] runs it inline —
//! bytes written by the parent are buffered until they form a complete
//! request frame, the service handles the payload, and the framed reply is
//! read back. The parent-side code above the seam (framing, containment,
//! the typed decode helpers) runs unchanged, exactly as an `Fs`/`Tty` fake
//! lets an app's state machine run unchanged on the host.
//!
//! [`LoopbackSession`] is the same idea for the duplex seam
//! ([`crate::session`]): one [`crate::session::SessionService`] run inline
//! behind a [`crate::session::SessionTransport`], so a consumer's host
//! tests drive `send` / `on_writable` / `on_readable` / `recv` exactly as
//! its production owner will. [`LoopbackSessionLauncher`] starts one per
//! launch for a supervised session ([`crate::supervise`]).
//!
//! Both fakes model a *healthy* worker. Containment paths are exercised by
//! scripting a failing [`crate::proto::Channel`] or
//! [`crate::session::SessionTransport`] directly (see `crate::host`'s and
//! `crate::session`'s tests); keeping failure injection out of these types
//! keeps their behaviour identical to a correct production worker.

use alloc::vec::Vec;
use tairix_abi::Errno;

use crate::host::Launcher;
use crate::proto::{head_frame, send_frame, Channel, ProtoError, FRAME_HEADER_LEN, MAX_FRAME};
use crate::session::{FrameOut, SessionDescriptors, SessionService, SessionStep, SessionTransport};
use crate::supervise::SessionLauncher;
use crate::worker::Service;

/// Builds one fresh service per launched loopback worker.
pub trait ServiceFactory {
    /// The service each loopback worker runs.
    type Service: Service;

    /// Construct the next worker's service.
    fn build(&mut self) -> Self::Service;
}

/// Every `Fn`-style closure that yields a service is a factory.
impl<S: Service, F: FnMut() -> S> ServiceFactory for F {
    type Service = S;

    fn build(&mut self) -> S {
        self()
    }
}

/// [`Launcher`] whose workers are in-process services.
pub struct LoopbackLauncher<F: ServiceFactory> {
    factory: F,
}

impl<F: ServiceFactory> LoopbackLauncher<F> {
    /// Build the launcher over the service factory.
    pub fn new(factory: F) -> Self {
        Self { factory }
    }
}

impl<F: ServiceFactory> Launcher for LoopbackLauncher<F> {
    type Channel = LoopbackChannel<F::Service>;

    fn launch(&mut self) -> Result<Self::Channel, Errno> {
        Ok(LoopbackChannel {
            service: self.factory.build(),
            request: Vec::new(),
            reply: Vec::new(),
            reply_at: 0,
        })
    }

    fn dispose(&mut self, _channel: Self::Channel) -> Option<i32> {
        // An in-process worker has no process to reap and no exit code.
        None
    }
}

/// The channel to one in-process loopback worker.
pub struct LoopbackChannel<S: Service> {
    service: S,
    /// Parent→worker bytes not yet consumed as a complete frame.
    request: Vec<u8>,
    /// Worker→parent framed reply bytes.
    reply: Vec<u8>,
    reply_at: usize,
}

impl<S: Service> LoopbackChannel<S> {
    /// Run the service over the buffered request bytes if they hold a
    /// complete, in-bound frame.
    fn pump(&mut self) {
        // An oversize declaration cannot come from the in-crate sender
        // (send_frame refuses it first); leaving it unconsumed mirrors a
        // worker that stops reading, and the parent's own bound already
        // failed the request.
        let Some(declared) = head_frame(&self.request).filter(|&len| len <= MAX_FRAME) else {
            return;
        };
        let payload: Vec<u8> = self
            .request
            .drain(..FRAME_HEADER_LEN + declared)
            .skip(FRAME_HEADER_LEN)
            .collect();
        let reply = self.service.handle(&payload);
        // Reclaim the already-consumed prefix of earlier replies before
        // appending this one, so a worker reused across many requests keeps
        // only the reply bytes still in flight rather than every reply it has
        // ever produced (an unbounded buffer that would eventually exhaust
        // memory over a long-running fuzz/soak run).
        self.reply.drain(..self.reply_at);
        self.reply_at = 0;
        // The reply is framed exactly as a real worker's send_frame does.
        let len = u32::try_from(reply.len().min(MAX_FRAME)).unwrap_or(0);
        self.reply.extend_from_slice(&len.to_le_bytes());
        self.reply.extend_from_slice(&reply[..len as usize]);
    }
}

impl<S: Service> Channel for LoopbackChannel<S> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Errno> {
        self.pump();
        if buf.is_empty() || self.reply_at == self.reply.len() {
            // No reply pending: a real pipe would block; the loopback has
            // nothing further coming, which the framing reports as the
            // peer being gone. Reaching this is a caller bug (a read with
            // no outstanding request), surfaced loudly rather than hung.
            return Ok(0);
        }
        let take = buf.len().min(self.reply.len() - self.reply_at);
        buf[..take].copy_from_slice(&self.reply[self.reply_at..self.reply_at + take]);
        self.reply_at += take;
        Ok(take)
    }

    fn write(&mut self, buf: &[u8]) -> Result<usize, Errno> {
        self.request.extend_from_slice(buf);
        Ok(buf.len())
    }
}

/// In-process [`SessionTransport`]: one [`SessionService`] run inline.
///
/// The parent's writes accumulate until they form a complete frame, the
/// service handles it and whatever it emits is framed into the reply
/// buffer, and the parent's reads drain that. A read with nothing pending
/// reports [`Errno::WouldBlock`] — "nothing right now" — which the session
/// treats as the no-op it is, so a host test can drive `on_readable`
/// freely without a wait-set to tell it when to.
pub struct LoopbackSession<S: SessionService> {
    service: S,
    /// Parent→worker bytes not yet consumed as a complete frame.
    request: Vec<u8>,
    /// Worker→parent framed bytes, with the prefix the parent has read.
    reply: Vec<u8>,
    reply_at: usize,
    /// Set once the service closed the session: reads then report
    /// end-of-stream and writes broken-pipe, exactly as a real worker's
    /// exit does.
    finished: bool,
}

impl<S: SessionService> LoopbackSession<S> {
    /// Build the fake over the service this session's worker runs.
    pub fn new(service: S) -> Self {
        Self {
            service,
            request: Vec::new(),
            reply: Vec::new(),
            reply_at: 0,
            finished: false,
        }
    }

    /// Run the service over every complete frame the parent has written.
    fn pump(&mut self) {
        // Reclaim what the parent has already read before producing more,
        // so a long-lived session holds only the frames still in flight
        // rather than every frame it has ever emitted.
        self.reply.drain(..self.reply_at);
        self.reply_at = 0;
        while !self.finished {
            // An oversize declaration cannot come from the session's own
            // sender, which bounds every payload first.
            let Some(declared) = head_frame(&self.request).filter(|&len| len <= MAX_FRAME) else {
                return;
            };
            let payload: Vec<u8> = self
                .request
                .drain(..FRAME_HEADER_LEN + declared)
                .skip(FRAME_HEADER_LEN)
                .collect();
            let Self { service, reply, .. } = self;
            let mut out = ReplyFrames { reply };
            if service.handle(&payload, &mut out) == SessionStep::Finished {
                self.finished = true;
            }
        }
    }
}

impl<S: SessionService> SessionTransport for LoopbackSession<S> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Errno> {
        if self.reply_at == self.reply.len() {
            return if self.finished {
                Ok(0)
            } else {
                Err(Errno::WouldBlock)
            };
        }
        let take = buf.len().min(self.reply.len() - self.reply_at);
        buf[..take].copy_from_slice(&self.reply[self.reply_at..self.reply_at + take]);
        self.reply_at += take;
        Ok(take)
    }

    fn write(&mut self, buf: &[u8]) -> Result<usize, Errno> {
        if self.finished {
            return Err(Errno::BrokenPipe);
        }
        self.request.extend_from_slice(buf);
        self.pump();
        Ok(buf.len())
    }

    fn descriptors(&self) -> Option<SessionDescriptors> {
        // An in-process worker occupies no descriptor, so a host test's
        // owner has nothing to register and drives the seam directly.
        None
    }

    fn dispose(self) -> Option<i32> {
        None
    }
}

/// [`SessionLauncher`] whose workers are [`LoopbackSession`]s over services
/// the factory builds, a fresh one per launch.
pub struct LoopbackSessionLauncher<F> {
    factory: F,
}

impl<F> LoopbackSessionLauncher<F> {
    /// Build the launcher over the service factory.
    pub fn new(factory: F) -> Self {
        Self { factory }
    }
}

impl<S: SessionService, F: FnMut() -> S> SessionLauncher for LoopbackSessionLauncher<F> {
    type Transport = LoopbackSession<S>;

    fn launch(&mut self) -> Result<LoopbackSession<S>, Errno> {
        Ok(LoopbackSession::new((self.factory)()))
    }
}

/// The loopback's reply buffer as a write-only channel, so the fake frames
/// through [`send_frame`] rather than re-encoding the header.
struct ReplyFrames<'a> {
    reply: &'a mut Vec<u8>,
}

impl Channel for ReplyFrames<'_> {
    fn read(&mut self, _buf: &mut [u8]) -> Result<usize, Errno> {
        // Write-only: the worker never reads its own output back.
        Ok(0)
    }

    fn write(&mut self, buf: &[u8]) -> Result<usize, Errno> {
        self.reply.extend_from_slice(buf);
        Ok(buf.len())
    }
}

impl FrameOut for ReplyFrames<'_> {
    fn frame(&mut self, payload: &[u8]) -> Result<(), ProtoError> {
        send_frame(self, payload)
    }
}

#[cfg(test)]
mod tests {
    use super::LoopbackLauncher;
    use crate::host::ParserSandbox;
    use crate::worker::Service;
    use alloc::vec::Vec;
    use tairix_log::{Event, Sink};

    /// Discards every event (the loopback happy path logs nothing).
    struct NullSink;

    impl Sink for NullSink {
        fn write_event(&self, _event: &Event<'_>) {}
    }

    /// Echoes each request with a `>` prefix.
    struct Tagger;

    impl Service for Tagger {
        fn handle(&mut self, request: &[u8]) -> Vec<u8> {
            let mut out = Vec::with_capacity(request.len() + 1);
            out.push(b'>');
            out.extend_from_slice(request);
            out
        }
    }

    #[test]
    fn the_full_parent_path_runs_over_the_in_process_worker() {
        let mut sandbox = ParserSandbox::new(LoopbackLauncher::new(|| Tagger), NullSink);
        assert_eq!(sandbox.request(b"alpha"), Ok(b">alpha".to_vec()));
        assert_eq!(sandbox.request(b"beta"), Ok(b">beta".to_vec()));
    }

    #[test]
    fn a_reused_worker_does_not_grow_its_reply_buffer_across_requests() {
        // Drive one reused loopback worker through many round trips and
        // confirm its internal reply buffer tracks only the bytes still in
        // flight, never every reply ever produced. Before the consumed
        // prefix was reclaimed this buffer grew without bound, exhausting
        // memory over a long-running fuzz/soak run.
        use crate::host::Launcher;
        use crate::proto::{recv_frame, send_frame, FRAME_HEADER_LEN};

        let mut launcher = LoopbackLauncher::new(|| Tagger);
        let mut channel = launcher.launch().expect("loopback launch never fails");
        for _ in 0..10_000 {
            send_frame(&mut channel, b"payload").expect("send succeeds");
            let reply = recv_frame(&mut channel)
                .expect("recv succeeds")
                .expect("a reply frame arrives");
            assert_eq!(reply, b">payload".to_vec());
            // One framed reply (`>payload`) is 4 header + 8 payload bytes.
            // The buffer never accumulates past a single reply's worth.
            assert!(
                channel.reply.len() <= FRAME_HEADER_LEN + b">payload".len(),
                "reply buffer grew to {} bytes across reuse",
                channel.reply.len(),
            );
        }
    }
}
