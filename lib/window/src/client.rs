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

use alloc::vec::Vec;

use tairix_abi::desktop::DesktopInfo;
use tairix_abi::driver::display::{DamageRect, DisplayMode};
use tairix_abi::input::{
    KeyInput, KeyValue, Modifiers as WireModifiers, NamedKeyCode, PointerButtonCode,
};
use tairix_abi::reply::decode_status_reply;
use tairix_abi::window_ipc::{
    decode_create_reply, decode_desktop_reply, BundleRef, PointerAction, WindowEvent,
    WindowRequest, WindowTitle, WINDOW_CREATE_REPLY_LEN, WINDOW_DESKTOP_REPLY_LEN,
};
use tairix_abi::{Errno, ProcId};
use tairix_geometry::Point;
use tairix_input::{InputEvent, Key, Modifiers, NamedKey, PointerButton};

/// High tag of an app's event-mailbox endpoint id (see
/// [`event_endpoint_for`]).
const EVENT_ENDPOINT_TAG: u64 = 0xE117_0000_0000_0000;

/// The shared input events one delivered [`PointerAction`] at window-local
/// `point` means, in the order a control must receive them.
///
/// Every app that hands pointer input to the shared control family needs the
/// same translation, so it has one definition here rather than a private copy
/// per app. A wire pointer event always carries a position, while the
/// controls' button transitions do not: the position is therefore always
/// delivered first as an [`InputEvent::PointerMoved`], and a press or release
/// follows it, so a control is never asked to decide about a button at a
/// position it has not been told about yet.
///
/// The mapping is total — every action and every button code has exactly one
/// meaning — so an app filters what it does not want by matching the events,
/// never by guessing at an unhandled code.
pub fn pointer_input_events(
    action: PointerAction,
    point: Point,
) -> impl Iterator<Item = InputEvent> {
    let transition = match action {
        PointerAction::Moved => None,
        PointerAction::Pressed(button) => Some(InputEvent::PointerPressed {
            button: pointer_button(button),
        }),
        PointerAction::Released(button) => Some(InputEvent::PointerReleased {
            button: pointer_button(button),
        }),
    };
    [Some(InputEvent::PointerMoved { to: point }), transition]
        .into_iter()
        .flatten()
}

/// The control-facing button one wire button code names.
const fn pointer_button(code: PointerButtonCode) -> PointerButton {
    match code {
        PointerButtonCode::Primary => PointerButton::Primary,
        PointerButtonCode::Secondary => PointerButton::Secondary,
        PointerButtonCode::Middle => PointerButton::Middle,
    }
}

/// The shared input event one delivered [`KeyInput`] means.
///
/// The companion to [`pointer_input_events`] for the keyboard: every app
/// that hands key input to the shared control family needs the same
/// translation from the wire vocabulary, so it has one definition here
/// rather than a private copy per app. The mapping is total — every key and
/// every modifier has exactly one meaning — so no app has to decide what an
/// unhandled code means.
#[must_use]
pub const fn key_input_event(input: KeyInput) -> InputEvent {
    match input {
        KeyInput::Pressed { key, modifiers } => InputEvent::KeyPressed {
            key: key_value(key),
            modifiers: key_modifiers(modifiers),
        },
        KeyInput::Released { key, modifiers } => InputEvent::KeyReleased {
            key: key_value(key),
            modifiers: key_modifiers(modifiers),
        },
    }
}

/// The control-facing key one wire key value names.
const fn key_value(value: KeyValue) -> Key {
    match value {
        KeyValue::Char(ch) => Key::Char(ch),
        KeyValue::Named(named) => Key::Named(named_key(named)),
    }
}

/// The control-facing named key one wire key code names.
const fn named_key(code: NamedKeyCode) -> NamedKey {
    match code {
        NamedKeyCode::Enter => NamedKey::Enter,
        NamedKeyCode::Escape => NamedKey::Escape,
        NamedKeyCode::Backspace => NamedKey::Backspace,
        NamedKeyCode::Tab => NamedKey::Tab,
        NamedKeyCode::Delete => NamedKey::Delete,
        NamedKeyCode::Insert => NamedKey::Insert,
        NamedKeyCode::Home => NamedKey::Home,
        NamedKeyCode::End => NamedKey::End,
        NamedKeyCode::PageUp => NamedKey::PageUp,
        NamedKeyCode::PageDown => NamedKey::PageDown,
        NamedKeyCode::Left => NamedKey::Left,
        NamedKeyCode::Right => NamedKey::Right,
        NamedKeyCode::Up => NamedKey::Up,
        NamedKeyCode::Down => NamedKey::Down,
        NamedKeyCode::F1 => NamedKey::Function { number: 1 },
        NamedKeyCode::F2 => NamedKey::Function { number: 2 },
        NamedKeyCode::F3 => NamedKey::Function { number: 3 },
        NamedKeyCode::F4 => NamedKey::Function { number: 4 },
        NamedKeyCode::F5 => NamedKey::Function { number: 5 },
        NamedKeyCode::F6 => NamedKey::Function { number: 6 },
        NamedKeyCode::F7 => NamedKey::Function { number: 7 },
        NamedKeyCode::F8 => NamedKey::Function { number: 8 },
        NamedKeyCode::F9 => NamedKey::Function { number: 9 },
        NamedKeyCode::F10 => NamedKey::Function { number: 10 },
        NamedKeyCode::F11 => NamedKey::Function { number: 11 },
        NamedKeyCode::F12 => NamedKey::Function { number: 12 },
    }
}

/// The control-facing modifiers the wire modifiers name.
const fn key_modifiers(modifiers: WireModifiers) -> Modifiers {
    Modifiers {
        shift: modifiers.shift,
        ctrl: modifiers.ctrl,
        alt: modifiers.alt,
        meta: modifiers.meta,
    }
}

/// Bounded slot count every window-channel app binds its event mailbox
/// with: input-rate events, drained after every wake, so a small queue is
/// ample and a stalled app costs the kernel a bounded mailbox rather than
/// unbounded memory.
///
/// The depth is one value for the whole channel, not each app's own
/// choice: the session's delivery path treats a refused send as evidence
/// that the owner has stopped draining, so two apps disagreeing about how
/// much slack they grant it would make that evidence mean different things
/// per app. Sized against a drained-every-wake consumer, with the session
/// coalescing an adjacent run of pointer motion over one window into its
/// newest sample before it ever reaches this queue.
///
/// A full mailbox costs the app nothing it cannot recover: the session
/// *holds* the refused event and delivers it once the app drains, parked on
/// the room the drain frees rather than polling for it. So this depth
/// governs how much the kernel buffers, not what an app may miss.
pub const EVENT_MAILBOX_CAPACITY: usize = 32;

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

/// What a window last presented, so the client can re-present it when
/// the session asks for a redraw.
///
/// The frame index alone is not enough: full-window damage needs the
/// window's current client extent, which the create/resize calls are the
/// only place that knows.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct LastPresent {
    /// The window the record belongs to.
    window_id: u64,
    /// Client width in pixels, as last created or resized.
    width_px: u32,
    /// Client height in pixels, as last created or resized.
    height_px: u32,
    /// The frame index of the last accepted present, if the window has
    /// presented at all since it was created or resized.
    frame_index: Option<u32>,
}

/// A typed handle on the desktop session's window service.
///
/// The client remembers each of its windows' extent and last presented
/// frame index so it can answer a
/// [`WindowEvent::RedrawRequested`]
/// on the app's behalf — see [`WindowEvents::wait`].
pub struct WindowClient<T: WindowTransport> {
    transport: T,
    presented: Vec<LastPresent>,
}

impl<T: WindowTransport> WindowClient<T> {
    /// A client over `transport`.
    pub const fn new(transport: T) -> Self {
        Self {
            transport,
            presented: Vec::new(),
        }
    }

    /// Open a window: `frame_count` frames shaped as `surface`, laid out
    /// back-to-back in the region granted as `shm_handle`, titled
    /// `title`, with this window's events delivered to the app's own
    /// `event_endpoint`.
    ///
    /// `resizable` asks the window manager to present the window with a
    /// resize grabber and a live maximize/restore size toggle; a
    /// fixed-size app passes `false` and is offered neither affordance
    /// (and never receives a [`WindowEvent::Resized`]). A resizable app
    /// re-lays-out to each reported size and re-maps its region with
    /// [`Self::resize`].
    ///
    /// The size is the app's own choice; [`Self::desktop`] is how it
    /// learns the screen it must fit on before making that choice.
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
    ///
    /// [`WindowEvent::Resized`]: tairix_abi::window_ipc::WindowEvent::Resized
    pub fn create(
        &mut self,
        shm_handle: u64,
        event_endpoint: u64,
        frame_count: u32,
        surface: &DisplayMode,
        title: &str,
        resizable: bool,
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
            resizable,
        }
        .to_le_bytes();
        let mut reply = [0u8; WINDOW_CREATE_REPLY_LEN];
        let len = self.transport.call(&request, &mut reply)?;
        let (window_id, server) = decode_create_reply(&reply[..len])?;
        self.note_extent(window_id, surface.width_px, surface.height_px);
        Ok((window_id, server))
    }

    /// Ask the session to describe the desktop this app's windows are
    /// displayed on: the screen extent, the UI scale, and the active
    /// appearance.
    ///
    /// An app calls this **before** [`Self::create`], so its first window
    /// is sized to a screen it knows and its first frame is painted at the
    /// right density in the right colours, rather than at a guess it has
    /// to correct once the user has already seen it. The session pushes a
    /// [`WindowEvent::DesktopChanged`] afterwards whenever any of it
    /// changes; [`Desktop`](crate::Desktop) holds the answer and keeps it
    /// current from those events.
    ///
    /// # Errors
    ///
    /// The session's typed refusal (a session that is tearing down and no
    /// longer has a screen to describe), a transport failure, or a corrupt
    /// reply — never a guessed extent.
    ///
    /// [`WindowEvent::DesktopChanged`]: tairix_abi::window_ipc::WindowEvent::DesktopChanged
    pub fn desktop(&mut self) -> Result<DesktopInfo, Errno> {
        let request = WindowRequest::QueryDesktop.to_le_bytes();
        let mut reply = [0u8; WINDOW_DESKTOP_REPLY_LEN];
        let len = self.transport.call(&request, &mut reply)?;
        decode_desktop_reply(&reply[..len])
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
        self.status_call(&request)?;
        if let Some(record) = self.record_mut(window_id) {
            record.frame_index = Some(frame_index);
        }
        Ok(())
    }

    /// Close window `window_id`.
    ///
    /// # Errors
    ///
    /// The session's typed refusal, a transport failure, or a corrupt
    /// status frame.
    pub fn close(&mut self, window_id: u64) -> Result<(), Errno> {
        let request = WindowRequest::Close { window_id }.to_le_bytes();
        self.status_call(&request)?;
        self.presented
            .retain(|record| record.window_id != window_id);
        Ok(())
    }

    /// Re-map window `window_id` onto a fresh frame region: `frame_count`
    /// frames shaped as `surface`, laid out back-to-back in the region
    /// granted as `shm_handle`, keeping the same window id, title, and
    /// event endpoint.
    ///
    /// A resizable app calls this after the window manager tells it a new
    /// client size ([`WindowEvent::Resized`]): it renders into the new
    /// region and re-maps the existing window onto it, so the resize keeps
    /// the window identity rather than opening a new window.
    ///
    /// # Errors
    ///
    /// The session's typed refusal (e.g. [`Errno::NotFound`] for a window
    /// the caller does not own), a transport failure, or a corrupt status
    /// frame.
    ///
    /// [`WindowEvent::Resized`]: tairix_abi::window_ipc::WindowEvent::Resized
    pub fn resize(
        &mut self,
        window_id: u64,
        shm_handle: u64,
        frame_count: u32,
        surface: &DisplayMode,
    ) -> Result<(), Errno> {
        let request = WindowRequest::Resize {
            window_id,
            shm_handle,
            frame_count,
            width_px: surface.width_px,
            height_px: surface.height_px,
            stride_bytes: surface.stride_bytes,
            format: surface.format,
        }
        .to_le_bytes();
        self.status_call(&request)?;
        // The old frames describe the old extent, so the remembered
        // present is stale until the app paints the new size.
        self.note_extent(window_id, surface.width_px, surface.height_px);
        Ok(())
    }

    /// Ask the session to run its trusted file picker for window
    /// `window_id` (`plans/CAPABILITY_USE.md` CU6).
    ///
    /// A success is only the acceptance: the pick concludes
    /// asynchronously with a [`WindowEvent::FilePicked`] (carrying the
    /// one-shot `fd_redeem` handle) or a [`WindowEvent::PickCancelled`]
    /// on the app's event endpoint, so the app keeps parking on its
    /// ordinary event wait.
    ///
    /// # Errors
    ///
    /// The session's typed refusal ([`Errno::AlreadyExists`] while a pick
    /// is already pending on the window; [`Errno::NotFound`] for a window
    /// the caller does not own), a transport failure, or a corrupt status
    /// frame.
    ///
    /// [`WindowEvent::FilePicked`]: tairix_abi::window_ipc::WindowEvent::FilePicked
    /// [`WindowEvent::PickCancelled`]: tairix_abi::window_ipc::WindowEvent::PickCancelled
    pub fn pick_file(&mut self, window_id: u64) -> Result<(), Errno> {
        let request = WindowRequest::PickFile { window_id }.to_le_bytes();
        self.status_call(&request)
    }

    /// Ask the session to pin `path` — an absolute `<Name>.app` bundle
    /// path — to the taskbar on behalf of window `window_id`
    /// (`plans/NEW-TASKBAR.md` T7).
    ///
    /// # Errors
    ///
    /// * [`Errno::LengthOutOfRange`] / [`Errno::OutOfRange`] — a path the
    ///   protocol refuses, caught before any call (never truncated).
    /// * [`Errno::AlreadyExists`] — the bundle is already pinned.
    /// * [`Errno::NoSpace`] — the pin store is full.
    /// * [`Errno::PermissionDenied`] — the session refuses the pin, or
    ///   `window_id` is not one of the caller's own windows.
    /// * A transport failure, or a corrupt status frame.
    pub fn pin_bundle(&mut self, window_id: u64, path: &str) -> Result<(), Errno> {
        let path = BundleRef::new(path)?;
        let request = WindowRequest::PinBundle {
            window: window_id,
            path,
        }
        .to_le_bytes();
        self.status_call(&request)
    }

    /// Offer `path` — an absolute `<Name>.app` bundle path — as an
    /// app-reference drag started in window `window_id`. A primary
    /// release may later drop it onto the taskbar's pin strip.
    ///
    /// # Errors
    ///
    /// * [`Errno::LengthOutOfRange`] / [`Errno::OutOfRange`] — a path the
    ///   protocol refuses, caught before any call (never truncated).
    /// * [`Errno::PermissionDenied`] — the session refuses the offer, or
    ///   `window_id` is not one of the caller's own windows.
    /// * A transport failure, or a corrupt status frame.
    pub fn drag_offer(&mut self, window_id: u64, path: &str) -> Result<(), Errno> {
        let path = BundleRef::new(path)?;
        let request = WindowRequest::DragOffer {
            window: window_id,
            path,
        }
        .to_le_bytes();
        self.status_call(&request)
    }

    /// Withdraw a drag offer previously armed by [`Self::drag_offer`] for
    /// window `window_id` (e.g. the app itself cancelled the drag with
    /// Escape before a release).
    ///
    /// # Errors
    ///
    /// [`Errno::PermissionDenied`] if `window_id` is not one of the
    /// caller's own windows, a transport failure, or a corrupt status
    /// frame.
    pub fn drag_withdraw(&mut self, window_id: u64) -> Result<(), Errno> {
        let request = WindowRequest::DragWithdraw { window: window_id }.to_le_bytes();
        self.status_call(&request)
    }

    /// Set window `window_id`'s backdrop-blur radius to `radius_px`
    /// logical pixels: the session blurs whatever is already composited
    /// behind the window's rectangle before blending the window's own
    /// (typically translucent) pixels over it, so a frosted-glass panel
    /// reads correctly. `0` disables the effect.
    ///
    /// # Errors
    ///
    /// [`Errno::LengthOutOfRange`] for a radius above
    /// [`tairix_abi::window_ipc::WINDOW_BACKDROP_BLUR_MAX_PX`],
    /// [`Errno::NotFound`] if `window_id` is not one of the caller's own
    /// windows, a transport failure, or a corrupt status frame.
    pub fn set_backdrop_blur(&mut self, window_id: u64, radius_px: u16) -> Result<(), Errno> {
        let request = WindowRequest::SetBackdropBlur {
            window_id,
            radius_px,
        }
        .to_le_bytes();
        self.status_call(&request)
    }

    /// Re-present window `window_id`'s last presented frame with
    /// full-window damage, answering a session redraw request.
    ///
    /// Returns whether a frame was re-presented: a window this client
    /// does not own, or one that has not presented since it was created
    /// or resized, has nothing to re-send and is ignored.
    ///
    /// An app that renders in place (single-buffered) may have the frame
    /// half-painted when this fires, so the session can briefly show a
    /// partly drawn frame — the same tearing in-place rendering already
    /// accepts, and strictly better than leaving the window blank.
    ///
    /// # Errors
    ///
    /// The session's typed refusal, a transport failure, or a corrupt
    /// status frame — exactly as [`Self::present`].
    pub fn answer_redraw(&mut self, window_id: u64) -> Result<bool, Errno> {
        let Some(record) = self
            .presented
            .iter()
            .copied()
            .find(|record| record.window_id == window_id)
        else {
            return Ok(false);
        };
        let Some(frame_index) = record.frame_index else {
            return Ok(false);
        };
        self.present(
            window_id,
            frame_index,
            DamageRect {
                x: 0,
                y: 0,
                width_px: record.width_px,
                height_px: record.height_px,
            },
        )?;
        Ok(true)
    }

    /// Remember `window_id`'s client extent, discarding any frame index
    /// recorded against a previous extent.
    fn note_extent(&mut self, window_id: u64, width_px: u32, height_px: u32) {
        if let Some(record) = self.record_mut(window_id) {
            record.width_px = width_px;
            record.height_px = height_px;
            record.frame_index = None;
            return;
        }
        self.presented.push(LastPresent {
            window_id,
            width_px,
            height_px,
            frame_index: None,
        });
    }

    /// The mutable record for `window_id`, if this client owns it.
    fn record_mut(&mut self, window_id: u64) -> Option<&mut LastPresent> {
        self.presented
            .iter_mut()
            .find(|record| record.window_id == window_id)
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

    /// Park until the next event arrives, decode it, and answer a
    /// redraw request on the app's behalf before returning it.
    ///
    /// The session releases a window's retained pixels under memory
    /// pressure and asks for them again with
    /// [`WindowEvent::RedrawRequested`].
    /// Answering it is mechanical — re-present the last frame with
    /// full-window damage — so the library does it through `client`
    /// ([`WindowClient::answer_redraw`]) and no app has to. A failed
    /// re-present is not fatal: the window stays blank until the app
    /// presents for a reason of its own, which is exactly the outcome
    /// the event already documents.
    ///
    /// The event is still returned, so an app that would rather re-render
    /// its content genuinely (a live view whose last frame is already
    /// out of date) can act on it.
    ///
    /// # Errors
    ///
    /// A source failure, or the typed refusal of a malformed frame — a
    /// corrupt event is refused, never guessed at (the app may keep
    /// waiting or tear down; the frame is already consumed either way).
    pub fn wait<T: WindowTransport>(
        &mut self,
        client: &mut WindowClient<T>,
    ) -> Result<WindowEvent, Errno> {
        let mut frame = [0u8; WindowEvent::WIRE_LEN];
        self.source.next(&mut frame)?;
        let event = WindowEvent::from_bytes(&frame)?;
        if let WindowEvent::RedrawRequested { window_id } = event {
            let _ = client.answer_redraw(window_id);
        }
        Ok(event)
    }
}
