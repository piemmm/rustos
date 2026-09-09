//! The app-side shell every windowed `Run` binary composes.
//!
//! Bringing a windowed application up is the same sequence every time: one
//! `ipc_call` transport to the reserved window endpoint, one `port_bind`-bound
//! event mailbox parked on through a wait-set that also carries the machine's
//! memory-pressure band, the desktop's screen/density/appearance queried
//! before anything is sized, and one retained drawing surface behind a shared
//! frame region that survives resizes and releases. It lives here beside the
//! rest of the app half of the channel so there is one of each rather than one
//! per bundle.
//!
//! # One window or many
//!
//! [`WindowPane`] is one window's channel-side state — its id, its region, its
//! layout — and the protocol over it. [`AppWindow`] pairs one with the retained
//! surface and takes the paint as a closure, which is what a single-window app
//! wants; an app that opens a window per document, or a popup above one, holds
//! panes itself and keeps whatever retained picture it actually paints from.
//!
//! # What stays with the application
//!
//! Its own wait-set members and the tokens for them ([`FIRST_APP_TOKEN`]
//! onward), what it *does* about a pressure-band change, and what it paints.
//! The shell owns the frame-region and damage bookkeeping without owning a
//! single pixel of anyone's window.

use core::fmt;

use tairix_abi::driver::display::{DamageRect, DisplayFormat, DisplayMode};
use tairix_abi::window_ipc::{WindowEvent, WindowSizing, WINDOW_ENDPOINT};
use tairix_abi::{Errno, ProcId, WaitSetOp, WaitSourceKind};
use tairix_display::{winframe, SERIAL};
use tairix_raster::Surface;
use tairix_theme::ThemeRegistry;

use crate::client::{WindowClient, WindowTransport};
use crate::desktop::Desktop;
use crate::frames::WindowFrames;
use crate::server::PopupSpec;

/// Exit code when the shared frame region could not be created or granted to
/// the window endpoint. A reserved, fail-closed value.
pub const EXIT_NO_FRAMES: i32 = 81;

/// Exit code when the event mailbox could not be bound or observed through the
/// wait-set. A reserved, fail-closed value: the app exits rather than degrade
/// into a busy re-poll.
pub const EXIT_NO_EVENTS: i32 = 82;

/// Exit code when the desktop session refused the window create (no graphical
/// session, or the channel refused the geometry). A reserved, fail-closed
/// value.
pub const EXIT_NO_WINDOW: i32 = 83;

/// Exit code when a present was refused or the event channel died (the session
/// went away). A reserved, fail-closed value.
pub const EXIT_CHANNEL_LOST: i32 = 84;

/// The wait-set token of the event-mailbox member.
pub const EVENT_TOKEN: u64 = 1;

/// The wait-set token of the memory-pressure member: the kernel wakes the park
/// when the machine's pressure band changes, so a cache is trimmed as memory
/// tightens instead of being held until something else is starved.
pub const PRESSURE_TOKEN: u64 = 2;

/// The lowest token an application may give a wait-set member of its own.
///
/// The shell's own members hold every value below it, so an app numbering from
/// here cannot collide with them however many it adds.
pub const FIRST_APP_TOKEN: u64 = 3;

/// A bring-up refusal: the reserved exit code, the reason to state, and the
/// typed error a caller that must answer in [`Errno`] hands on.
///
/// The reason carries no application name — the app prefixes its own, which is
/// the one part of a diagnostic that genuinely differs per bundle. The errno is
/// carried rather than derived from the exit code, because one code covers
/// several distinct refusals: a window already being open and a surface that
/// could not be allocated are both [`EXIT_NO_WINDOW`], and flattening them to
/// one errno would report an out-of-memory as a programming mistake or the
/// reverse.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ShellError {
    code: i32,
    reason: &'static str,
    errno: Errno,
}

impl ShellError {
    const fn new(code: i32, reason: &'static str, errno: Errno) -> Self {
        Self {
            code,
            reason,
            errno,
        }
    }

    /// The reserved exit code to hand back from `main`.
    #[must_use]
    pub const fn code(&self) -> i32 {
        self.code
    }

    /// The typed error, for a caller whose own contract answers in [`Errno`]
    /// rather than an exit code.
    #[must_use]
    pub const fn errno(&self) -> Errno {
        self.errno
    }
}

impl fmt::Display for ShellError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.reason, self.errno)
    }
}

/// The production [`WindowTransport`]: one synchronous `ipc_call` to the
/// reserved window endpoint per request.
///
/// The session attests the caller kernel-side on every request, so the
/// transport carries no claimed authority.
pub struct RtWindowTransport;

impl WindowTransport for RtWindowTransport {
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
        tairix_rt::ipc_call(WINDOW_ENDPOINT, request, reply).map_err(Errno::from_syscall)
    }
}

/// What a park woke for.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Wake {
    /// The event mailbox has a frame to drain.
    Event,
    /// The machine's memory-pressure band changed, so a cache should be
    /// trimmed. What to hand back is the application's own business — several
    /// hold more than a glyph cache.
    PressureChanged,
    /// The pressure member woke but the band is unchanged: nothing is owed.
    PressureUnchanged,
    /// One of the application's own members, by its token.
    App(u64),
}

fn classify(token: u64) -> Wake {
    match token {
        EVENT_TOKEN => Wake::Event,
        PRESSURE_TOKEN => {
            if tairix_procinfo::pressure::refresh() {
                Wake::PressureChanged
            } else {
                Wake::PressureUnchanged
            }
        }
        other => Wake::App(other),
    }
}

/// Park on `set` until a member is ready, and say which.
///
/// # Errors
///
/// The kernel's refusal, which for a live set handle means the set was torn
/// down under the caller. Never a timeout: this park has no deadline.
pub fn park(set: u64) -> Result<Wake, Errno> {
    let mut token = 0u64;
    let rc = tairix_rt::waitset_wait(set, u64::MAX, &mut token);
    if rc == 0 {
        return Ok(classify(token));
    }
    Err(Errno::from_syscall(rc))
}

/// Park on `set` until a member is ready or `timeout_ns` elapses, answering
/// `None` on the deadline.
///
/// One-shot by construction: the deadline is the caller's next due event, so a
/// loop that has nothing pending passes no timeout at all and the CPU is given
/// up entirely.
///
/// # Errors
///
/// The kernel's refusal. A reached deadline is `Ok(None)`, not an error.
pub fn park_until(set: u64, timeout_ns: u64) -> Result<Option<Wake>, Errno> {
    let mut token = 0u64;
    let rc = tairix_rt::waitset_wait(set, timeout_ns, &mut token);
    if rc == 0 {
        return Ok(Some(classify(token)));
    }
    match Errno::from_syscall(rc) {
        Errno::TimedOut => Ok(None),
        err => Err(err),
    }
}

/// The app's bound event mailbox endpoint and the wait-set it parks on.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Binding {
    endpoint: u64,
    set: u64,
}

impl Binding {
    /// The endpoint the session delivers this app's events to.
    #[must_use]
    pub const fn endpoint(&self) -> u64 {
        self.endpoint
    }

    /// The wait-set handle, for [`park`] and for adding members of the app's
    /// own.
    #[must_use]
    pub const fn set(&self) -> u64 {
        self.set
    }
}

/// Bind this process's event mailbox and add it, with the memory-pressure
/// band, to a fresh wait-set.
///
/// Fails closed rather than degrading into a re-poll: an app that cannot be
/// woken by its own events has no correct way to carry on.
///
/// # Errors
///
/// [`EXIT_NO_EVENTS`] for every refusal along the way, with the step named.
pub fn bind_event_mailbox() -> Result<Binding, ShellError> {
    let Ok(origin) = tairix_rt::self_origin() else {
        return Err(ShellError::new(
            EXIT_NO_EVENTS,
            "own identity unavailable",
            Errno::NotFound,
        ));
    };
    let endpoint = crate::client::event_endpoint_for(origin.pid());
    if tairix_abi::ipc::is_reserved_endpoint(endpoint)
        || tairix_rt::port_bind(
            endpoint,
            WindowEvent::WIRE_LEN,
            crate::client::EVENT_MAILBOX_CAPACITY,
        ) != 0
    {
        return Err(ShellError::new(
            EXIT_NO_EVENTS,
            "event mailbox bind refused",
            Errno::NotFound,
        ));
    }
    let set = tairix_rt::waitset_create();
    if set < 0 {
        return Err(ShellError::new(
            EXIT_NO_EVENTS,
            "wait-set refused",
            Errno::NotFound,
        ));
    }
    #[allow(clippy::cast_sign_loss)] // `set >= 0` checked above; it is a kernel handle.
    let set = set as u64;
    if tairix_rt::waitset_ctl(
        set,
        WaitSetOp::Add,
        WaitSourceKind::Port,
        endpoint,
        EVENT_TOKEN,
    ) != 0
    {
        return Err(ShellError::new(
            EXIT_NO_EVENTS,
            "event mailbox wait refused",
            Errno::NotFound,
        ));
    }
    if !tairix_procinfo::pressure::watch(set, PRESSURE_TOKEN) {
        return Err(ShellError::new(
            EXIT_NO_EVENTS,
            "memory-pressure wake refused",
            Errno::NotFound,
        ));
    }
    Ok(Binding { endpoint, set })
}

/// Ask the session for its desktop and build this app's [`Desktop`] model and
/// [`ThemeRegistry`], with the session's current appearance already applied.
///
/// The screen, the density, and the look are current before anything is sized
/// or painted, so the first frame is right rather than a guess corrected once
/// the user has seen it.
///
/// # Errors
///
/// [`EXIT_NO_WINDOW`] for a refused query, or for a desktop this client cannot
/// draw at (a scale outside the range [`Desktop`] admits is refused, never
/// clamped — drawing at a density the session did not ask for would misplace
/// every hit-test in the window).
pub fn bring_up_desktop<T: WindowTransport>(
    client: &mut WindowClient<T>,
) -> Result<(Desktop, ThemeRegistry), ShellError> {
    let info = client
        .desktop()
        .map_err(|err| ShellError::new(EXIT_NO_WINDOW, "desktop query refused", err))?;
    let desktop = Desktop::new(info)
        .map_err(|err| ShellError::new(EXIT_NO_WINDOW, "cannot draw this desktop", err))?;
    let mut themes = ThemeRegistry::with_builtins();
    themes.set_appearance(desktop.appearance());
    Ok((desktop, themes))
}

/// Bytes per pixel of a [`mode_for`] surface, which an app that writes the
/// shared frame itself takes its own arithmetic from rather than restating.
pub const BYTES_PER_PIXEL: u32 = 4;

/// A `width_px` × `height_px` RGBA window mode, one frame's worth per row.
///
/// The one place a window's mode is shaped, so a create, a resize, and every
/// present agree on stride and format.
#[must_use]
pub fn mode_for(width_px: u32, height_px: u32) -> DisplayMode {
    DisplayMode {
        width_px,
        height_px,
        stride_bytes: width_px.saturating_mul(BYTES_PER_PIXEL),
        format: DisplayFormat::Rgba8888,
    }
}

/// Total bytes a `frame_count`-frame region shaped as `mode` needs, or `None`
/// when that product does not fit an address.
///
/// Checked rather than saturating, because either failure mode is fatal to the
/// caller: a wrapped length asks for a region too small for the window it
/// describes, and a saturated one for a region no machine can map. On a 32-bit
/// target the product of two `u32` dimensions genuinely does not fit.
#[must_use]
pub fn region_bytes(mode: &DisplayMode, frame_count: u32) -> Option<usize> {
    usize::try_from(mode.stride_bytes)
        .ok()?
        .checked_mul(usize::try_from(mode.height_px).ok()?)?
        .checked_mul(usize::try_from(frame_count).ok()?)
}

/// Frames in a window's shared region.
///
/// The window protocol serialises a present — the app is parked in the call
/// while the session reads — so a single frame is race-free; the constant names
/// the choice.
pub const FRAME_COUNT: u32 = 1;
/// One open window's channel-side state: the id the session knows it by, the
/// shared frame region every present crosses, and the pixel layout both are
/// shaped as.
///
/// It holds no picture. What a window *looks* like is the application's — a
/// plain [`Surface`] for most, a screen model carrying its own cell diff for a
/// terminal — and a pane that owned a surface would force a second
/// window-sized allocation on every app whose retained picture is not literally
/// one. So the pane owns the protocol: attach, encode, present, re-map, close.
///
/// A single-window app composes one inside [`AppWindow`], which pairs it with
/// the retained surface and takes the paint as a closure. An app that opens a
/// window per document, or a popup above one, holds them itself.
///
/// Dropping a pane unmaps its region but tells the session nothing, so a caller
/// that means to take a window off the screen calls [`Self::close`].
pub struct WindowPane {
    window: u64,
    frames: WindowFrames,
    mode: DisplayMode,
}

impl WindowPane {
    /// Create and grant a `mode`-shaped frame region.
    fn region(mode: &DisplayMode) -> Result<(WindowFrames, u64), ShellError> {
        let Some(len) = region_bytes(mode, FRAME_COUNT) else {
            return Err(ShellError::new(
                EXIT_NO_FRAMES,
                "frame region larger than the address width",
                Errno::OutOfRange,
            ));
        };
        let Some(frames) = WindowFrames::create(len) else {
            return Err(ShellError::new(
                EXIT_NO_FRAMES,
                "shared frame region refused",
                Errno::OutOfMemory,
            ));
        };
        let Some(grant) = frames.grant() else {
            return Err(ShellError::new(
                EXIT_NO_FRAMES,
                "frame region grant refused",
                Errno::OutOfMemory,
            ));
        };
        Ok((frames, grant))
    }

    /// Open a top-level window of `mode`, titled `title`, sized as `sizing`
    /// allows, and answer it with the serving session's [`ProcId`].
    ///
    /// # Errors
    ///
    /// [`EXIT_NO_FRAMES`] when the shared region could not be sized, created,
    /// or granted, and [`EXIT_NO_WINDOW`] when the session refused the create.
    /// Every refusal unmaps whatever it had allocated.
    pub fn open<T: WindowTransport>(
        client: &mut WindowClient<T>,
        event_endpoint: u64,
        mode: &DisplayMode,
        title: &str,
        sizing: WindowSizing,
    ) -> Result<(Self, ProcId), ShellError> {
        let (frames, grant) = Self::region(mode)?;
        let (window, server) = client
            .create(grant, event_endpoint, FRAME_COUNT, mode, title, sizing)
            .map_err(|err| {
                ShellError::new(EXIT_NO_WINDOW, "desktop session refused the window", err)
            })?;
        Ok((
            Self {
                window,
                frames,
                mode: *mode,
            },
            server,
        ))
    }

    /// Open an undecorated popup of `mode` at `offset` from `parent`'s client
    /// origin, refusing a create reply that did not come from `server` — the
    /// session that opened the parent.
    ///
    /// A reply naming another sender is something else answering for the window
    /// endpoint, so the window it named is closed and the region dropped rather
    /// than drawn into.
    ///
    /// A negative offset is legitimate: the session resolves it against the
    /// parent's screen position and clamps the popup on screen, so a popup
    /// larger than its parent still shows whole.
    ///
    /// # Errors
    ///
    /// [`EXIT_NO_FRAMES`] for the region, [`EXIT_NO_WINDOW`] for a refused
    /// create, and [`Errno::PermissionDenied`] under [`EXIT_NO_WINDOW`] for the
    /// imposter reply above.
    pub fn open_popup<T: WindowTransport>(
        client: &mut WindowClient<T>,
        parent: u64,
        server: ProcId,
        event_endpoint: u64,
        mode: &DisplayMode,
        offset: (i32, i32),
    ) -> Result<Self, ShellError> {
        let (frames, grant) = Self::region(mode)?;
        let (window, replied) = client
            .create_popup(&PopupSpec {
                parent_window_id: parent,
                shm_handle: grant,
                event_endpoint,
                frame_count: FRAME_COUNT,
                surface: *mode,
                offset_x: offset.0,
                offset_y: offset.1,
            })
            .map_err(|err| {
                ShellError::new(EXIT_NO_WINDOW, "desktop session refused the popup", err)
            })?;
        if replied != server {
            let _ = client.close(window);
            return Err(ShellError::new(
                EXIT_NO_WINDOW,
                "popup reply came from another sender",
                Errno::PermissionDenied,
            ));
        }
        Ok(Self {
            window,
            frames,
            mode: *mode,
        })
    }

    /// The session's id for this window, which its delivered events name.
    #[must_use]
    pub const fn id(&self) -> u64 {
        self.window
    }

    /// The layout the region and every present are shaped as.
    #[must_use]
    pub const fn mode(&self) -> &DisplayMode {
        &self.mode
    }

    /// Whether the session has released its copy of the window's pixels and
    /// this side has given the region back.
    ///
    /// A released region holds none of the pixels a partial present would leave
    /// standing, so a caller resolving a *reported* damage set promotes it to
    /// the whole window when this is true — otherwise a round that reported
    /// nothing would leave the window blank.
    #[must_use]
    pub const fn content_released(&self) -> bool {
        self.frames.is_released()
    }

    /// Copy `damage` of `surface` into the shared frame and present exactly
    /// that rectangle.
    ///
    /// A region the session released is re-attached first, so a paint after a
    /// release lands in a live one. Covering the whole window when
    /// [`Self::content_released`] said so is the caller's, because only the
    /// caller knows what it painted.
    ///
    /// # Errors
    ///
    /// [`Errno::NotAttached`] when the region could not be re-attached,
    /// whatever the frame codec refuses (a rectangle outside the surface or
    /// past the region), and otherwise the session's refusal of the present.
    pub fn present<T: WindowTransport>(
        &mut self,
        client: &mut WindowClient<T>,
        surface: &Surface,
        damage: DamageRect,
    ) -> Result<(), Errno> {
        let mode = self.mode;
        let window = self.window;
        let pixels = client
            .frame_pixels(&mut self.frames, window, FRAME_COUNT, &mode)
            .ok_or(Errno::NotAttached)?;
        winframe::encode(surface, pixels, &mode, damage, &SERIAL)?;
        client.present(window, 0, damage)
    }

    /// Re-map the frame region onto `new_mode`, answering whether the new
    /// geometry was adopted.
    ///
    /// Fail-closed by ordering: the fresh region is created and granted first
    /// and adopted only once the session has accepted the resize, so the old
    /// region stays mapped — and the window drawable at its current size —
    /// through every refusal, while the fresh one is unmapped by its own drop.
    ///
    /// `false` therefore means "still at the old size, and still drawable",
    /// never "broken".
    pub fn resize<T: WindowTransport>(
        &mut self,
        client: &mut WindowClient<T>,
        new_mode: &DisplayMode,
    ) -> bool {
        let Some(len) = region_bytes(new_mode, FRAME_COUNT) else {
            return false;
        };
        let Some(spare) = WindowFrames::create(len) else {
            return false;
        };
        let Some(grant) = spare.grant() else {
            return false;
        };
        if client
            .resize(self.window, grant, FRAME_COUNT, new_mode)
            .is_err()
        {
            return false;
        }
        self.frames = spare;
        self.mode = *new_mode;
        true
    }

    /// Answer the session's release of its own copy by giving this side's
    /// region back, so the pages are actually freed.
    ///
    /// The pages only go when both halves let go, which is the whole point.
    pub fn release_frames(&mut self) {
        self.frames.release();
    }

    /// Close the window and answer what the session said.
    ///
    /// The pane is consumed either way — so the frame region is unmapped and
    /// nothing is left pinned even when the session refuses — which is why a
    /// caller with nothing to report may ignore the answer.
    ///
    /// # Errors
    ///
    /// The session's refusal, which for a window it no longer knows about means
    /// the teardown this call was asking for has already happened.
    pub fn close<T: WindowTransport>(self, client: &mut WindowClient<T>) -> Result<(), Errno> {
        client.close(self.window)
    }
}

/// A single-window app's open window: the pane and the surface every frame is
/// drawn into.
///
/// The surface is held for the life of the window because allocating and
/// zeroing one per present would be a whole-window pass of its own, and holding
/// it is what makes a clipped repaint sound — every pixel outside the clip is
/// the one already on screen.
struct Retained {
    pane: WindowPane,
    surface: Surface,
}

/// The live window channel an app owns, and the one window it may or may not
/// have open.
///
/// An app is on the icon bar whether or not a window is open, so the channel
/// outlives every window that crosses it. An app that opens more than one holds
/// [`WindowPane`]s itself instead.
pub struct AppWindow {
    client: WindowClient<RtWindowTransport>,
    retained: Option<Retained>,
}

impl AppWindow {
    /// A channel with no window open yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            client: WindowClient::new(RtWindowTransport),
            retained: None,
        }
    }

    /// The channel itself, for the requests the shell does not wrap (the
    /// app-bar declaration, a pick, a menu, a tooltip, a retitle).
    pub fn client(&mut self) -> &mut WindowClient<RtWindowTransport> {
        &mut self.client
    }

    /// Whether a window is open.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.retained.is_some()
    }

    /// The open window's id, or `None` with none open.
    #[must_use]
    pub const fn window_id(&self) -> Option<u64> {
        match &self.retained {
            Some(held) => Some(held.pane.id()),
            None => None,
        }
    }

    /// Whether the session has released its copy of the window's pixels.
    ///
    /// [`Self::present`] performs the whole-window promotion itself for the
    /// rectangle it is handed; this is for the callers that must decide
    /// earlier.
    #[must_use]
    pub const fn content_released(&self) -> bool {
        match &self.retained {
            Some(held) => held.pane.content_released(),
            None => false,
        }
    }

    /// The open window's current shape, or `None` with none open.
    #[must_use]
    pub const fn mode(&self) -> Option<&DisplayMode> {
        match &self.retained {
            Some(held) => Some(held.pane.mode()),
            None => None,
        }
    }

    /// Open a window of `mode`, titled `title`, sized as `sizing` allows, and
    /// answer the serving session's [`ProcId`] from the create reply.
    ///
    /// The drawing surface is allocated before the session is asked, so a
    /// window the app could not draw into is never put on screen.
    ///
    /// # Errors
    ///
    /// [`EXIT_NO_WINDOW`] when the surface could not be allocated or the
    /// session refused the create, [`EXIT_NO_FRAMES`] for the shared region,
    /// and [`EXIT_NO_WINDOW`] again when a window is already open: an app that
    /// asks for a second through this one channel has lost track of the first.
    pub fn open(
        &mut self,
        event_endpoint: u64,
        mode: &DisplayMode,
        title: &str,
        sizing: WindowSizing,
    ) -> Result<ProcId, ShellError> {
        if self.retained.is_some() {
            return Err(ShellError::new(
                EXIT_NO_WINDOW,
                "a window is already open",
                Errno::AlreadyExists,
            ));
        }
        let Some(surface) = Surface::new(mode.width_px, mode.height_px) else {
            return Err(ShellError::new(
                EXIT_NO_WINDOW,
                "no memory for the window surface",
                Errno::OutOfMemory,
            ));
        };
        let (pane, server) =
            WindowPane::open(&mut self.client, event_endpoint, mode, title, sizing)?;
        self.retained = Some(Retained { pane, surface });
        Ok(server)
    }

    /// Draw `damage` of the window through `paint` and present that rectangle.
    ///
    /// A region the session released while the window was hidden is re-attached
    /// and presented whole, because it holds none of the pixels a partial
    /// present would leave standing. With no window open this is a no-op, so a
    /// loop need not sort its repaints by whether one is showing.
    ///
    /// # Errors
    ///
    /// [`Errno::NotAttached`] when the region could not be re-attached, and
    /// otherwise the session's refusal of the present.
    pub fn present(
        &mut self,
        damage: DamageRect,
        paint: impl FnOnce(&mut Surface),
    ) -> Result<(), Errno> {
        let Some(held) = self.retained.as_mut() else {
            return Ok(());
        };
        let damage = if held.pane.content_released() {
            DamageRect::full(held.pane.mode())
        } else {
            damage
        };
        held.surface
            .with_clip(damage.x, damage.y, damage.width_px, damage.height_px, paint);
        held.pane.present(&mut self.client, &held.surface, damage)
    }

    /// Re-map the frame region onto `new_mode`, answering whether the new
    /// geometry was adopted.
    ///
    /// The fresh surface is allocated before the session is asked, and the pane
    /// adopts the fresh region only once the session has accepted it, so every
    /// refusal leaves the current geometry standing and still drawable.
    ///
    /// `false` therefore means "still at the old size, and still drawable", not
    /// "broken". The caller repaints the whole window either way, since even a
    /// refused resize leaves the reported client size unchanged and the current
    /// picture already matches it.
    pub fn resize(&mut self, new_mode: DisplayMode) -> bool {
        let Some(held) = self.retained.as_mut() else {
            return false;
        };
        let Some(surface) = Surface::new(new_mode.width_px, new_mode.height_px) else {
            return false;
        };
        if !held.pane.resize(&mut self.client, &new_mode) {
            return false;
        }
        held.surface = surface;
        true
    }

    /// Answer the session's release of its own copy by giving this side's
    /// region back, so the pages are actually freed.
    pub fn release_frames(&mut self) {
        if let Some(held) = self.retained.as_mut() {
            held.pane.release_frames();
        }
    }

    /// Close the open window, if any, leaving the app on the icon bar, and
    /// answer what the session said.
    ///
    /// The pane is dropped either way — so the frame region is unmapped and
    /// nothing is left pinned even when the session refuses — which is why a
    /// caller with nothing to report may ignore the answer. One that owes its
    /// own caller an outcome hands this on rather than inventing success.
    ///
    /// # Errors
    ///
    /// The session's refusal, which for a window it no longer knows about means
    /// the teardown this call was asking for has already happened.
    pub fn close(&mut self) -> Result<(), Errno> {
        match self.retained.take() {
            Some(held) => held.pane.close(&mut self.client),
            None => Ok(()),
        }
    }
}

impl Default for AppWindow {
    fn default() -> Self {
        Self::new()
    }
}
