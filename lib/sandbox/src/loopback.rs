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
//! This fake models a *healthy* worker. Containment paths are exercised by
//! scripting a failing [`crate::proto::Channel`] directly (see
//! `crate::host`'s tests); keeping failure injection out of this type keeps
//! its behaviour identical to a correct production worker.

use alloc::vec::Vec;
use tairix_abi::Errno;

use crate::host::Launcher;
use crate::proto::{Channel, FRAME_HEADER_LEN, MAX_FRAME};
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
        if self.request.len() < FRAME_HEADER_LEN {
            return;
        }
        let declared = u32::from_le_bytes([
            self.request[0],
            self.request[1],
            self.request[2],
            self.request[3],
        ]) as usize;
        // An oversize declaration cannot come from the in-crate sender
        // (send_frame refuses it first); leaving it unconsumed mirrors a
        // worker that stops reading, and the parent's own bound already
        // failed the request.
        if declared > MAX_FRAME || self.request.len() < FRAME_HEADER_LEN + declared {
            return;
        }
        let payload: Vec<u8> = self
            .request
            .drain(..FRAME_HEADER_LEN + declared)
            .skip(FRAME_HEADER_LEN)
            .collect();
        let reply = self.service.handle(&payload);
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
}
