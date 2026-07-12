//! The client half: the app-side presenter and event wait.
//!
//! [`WindowClient`] speaks the wire protocol over an injected
//! [`WindowTransport`] (the `ipc_call` syscall in production, a mock in
//! tests). [`WindowEvents`] wraps an injected [`EventSource`] — the
//! app's **parked** wait on its own event endpoint, never a poll — and
//! decodes each delivered event fail-closed.
//!
//! # Zero-copy shape
//!
//! The app owns the shared frame region (its own `shm_create` mapping):
//! it renders into a frame, then presents by frame *index* plus the
//! damage rectangle it just changed — never pixel bytes. The session
//! reads the pixels through its own mapping of the granted region.

use rustos_abi::driver::display::{DamageRect, DisplayMode};
use rustos_abi::reply::decode_status_reply;
use rustos_abi::window_ipc::{
    decode_create_reply, WindowEvent, WindowRequest, WindowTitle, WINDOW_CREATE_REPLY_LEN,
};
use rustos_abi::{Errno, ProcId};

/// High tag of an app's event-mailbox endpoint id (see
/// [`event_endpoint_for`]).
const EVENT_ENDPOINT_TAG: u64 = 0xE117_0000_0000_0000;

/// The event-mailbox endpoint id an app binds for its window events: the
/// app's kernel task id (never reused) under a fixed high tag, so every
/// app instance binds a distinct, collision-free, non-reserved id — the
/// one naming rule every window-channel app shares, so two apps can never
/// disagree about the id space. The mailbox is owner-only to receive and
/// every message carries its sender's kernel-attested origin, so the id
/// needs no secrecy.
#[must_use]
pub const fn event_endpoint_for(pid: u64) -> u64 {
    EVENT_ENDPOINT_TAG | pid
}

/// The one call the client issues: send one request frame, receive one
/// reply frame — the `ipc_call` syscall behind a seam, so the client is
/// host-testable.
pub trait WindowTransport {
    /// Issue one synchronous call to the window endpoint.
    ///
    /// Returns the reply length written into `reply`.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the transport surfaces (no such endpoint, a dead
    /// session); the caller treats a transport failure exactly like a
    /// refused request.
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno>;
}

/// A typed handle on the desktop session's window service.
pub struct WindowClient<T: WindowTransport> {
    transport: T,
}

impl<T: WindowTransport> WindowClient<T> {
    /// A client over `transport`.
    pub const fn new(transport: T) -> Self {
        Self { transport }
    }

    /// Open a window: `frame_count` frames shaped as `surface`, laid out
    /// back-to-back in the region granted as `shm_handle`, titled
    /// `title`, with this window's events delivered to the app's own
    /// `event_endpoint`.
    ///
    /// Returns the session-minted window id and the serving session's
    /// [`ProcId`]: the identity the app then requires of every event's
    /// kernel-attested sender, so no other process can feed it forged
    /// input (the reply is trustworthy because the window rendezvous is
    /// squat-protected).
    ///
    /// # Errors
    ///
    /// * [`Errno::LengthOutOfRange`] / [`Errno::OutOfRange`] — a title
    ///   or geometry the protocol refuses, caught before any call.
    /// * The session's typed refusal, a transport failure, or a corrupt
    ///   reply (fail closed, never a guessed id or an unauthenticatable
    ///   event stream).
    pub fn create(
        &mut self,
        shm_handle: u64,
        event_endpoint: u64,
        frame_count: u32,
        surface: &DisplayMode,
        title: &str,
    ) -> Result<(u64, ProcId), Errno> {
        let title = WindowTitle::new(title)?;
        let request = WindowRequest::Create {
            shm_handle,
            event_endpoint,
            frame_count,
            width_px: surface.width_px,
            height_px: surface.height_px,
            stride_bytes: surface.stride_bytes,
            format: surface.format,
            title,
        }
        .to_le_bytes();
        let mut reply = [0u8; WINDOW_CREATE_REPLY_LEN];
        let len = self.transport.call(&request, &mut reply)?;
        decode_create_reply(&reply[..len])
    }

    /// Present frame `frame_index` of window `window_id`, of which
    /// `damage` changed.
    ///
    /// # Errors
    ///
    /// The session's typed refusal, a transport failure, or a corrupt
    /// status frame.
    pub fn present(
        &mut self,
        window_id: u64,
        frame_index: u32,
        damage: DamageRect,
    ) -> Result<(), Errno> {
        let request = WindowRequest::Present {
            window_id,
            frame_index,
            damage,
        }
        .to_le_bytes();
        self.status_call(&request)
    }

    /// Close window `window_id`.
    ///
    /// # Errors
    ///
    /// The session's typed refusal, a transport failure, or a corrupt
    /// status frame.
    pub fn close(&mut self, window_id: u64) -> Result<(), Errno> {
        let request = WindowRequest::Close { window_id }.to_le_bytes();
        self.status_call(&request)
    }

    /// Issue `request` and decode the shared status reply.
    fn status_call(&mut self, request: &[u8]) -> Result<(), Errno> {
        let mut reply = [0u8; WINDOW_CREATE_REPLY_LEN];
        let len = self.transport.call(request, &mut reply)?;
        decode_status_reply(&reply[..len])
    }
}

/// The event-arrival seam: block — parked on the app's wait-set, never
/// spinning — until the session delivers the next encoded event to the
/// app's event endpoint.
pub trait EventSource {
    /// Fill `event` with the next delivered event frame, parking the
    /// task until one arrives.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the wait surfaces (the endpoint torn down, the
    /// session gone); the app treats it as the channel ending.
    fn next(&mut self, event: &mut [u8; WindowEvent::WIRE_LEN]) -> Result<(), Errno>;
}

/// The app's typed event wait over an [`EventSource`].
pub struct WindowEvents<S: EventSource> {
    source: S,
}

impl<S: EventSource> WindowEvents<S> {
    /// A typed wait over `source`.
    pub const fn new(source: S) -> Self {
        Self { source }
    }

    /// Park until the next event arrives and decode it.
    ///
    /// # Errors
    ///
    /// A source failure, or the typed refusal of a malformed frame — a
    /// corrupt event is refused, never guessed at (the app may keep
    /// waiting or tear down; the frame is already consumed either way).
    pub fn wait(&mut self) -> Result<WindowEvent, Errno> {
        let mut frame = [0u8; WindowEvent::WIRE_LEN];
        self.source.next(&mut frame)?;
        WindowEvent::from_bytes(&frame)
    }
}
