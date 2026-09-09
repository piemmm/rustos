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
//! # What stays with the application
//!
//! Its own wait-set members and the tokens for them ([`FIRST_APP_TOKEN`]
//! onward), what it *does* about a pressure-band change, and what it paints.
//! [`AppWindow::present`] takes the paint as a closure precisely so the shell
//! owns the frame-region and damage bookkeeping without owning a single pixel
//! of anyone's window.

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

/// Total bytes a `frame_count`-frame region shaped as `mode` needs.
#[must_use]
pub fn region_bytes(mode: &DisplayMode, frame_count: u32) -> usize {
    (mode.stride_bytes as usize) * (mode.height_px as usize) * (frame_count as usize)
}

/// Frames in a window's shared region.
///
/// The window protocol serialises a present — the app is parked in the call
/// while the session reads — so a single frame is race-free; the constant names
/// the choice.
pub const FRAME_COUNT: u32 = 1;

/// One open window: its id, its shared frame region, its current mode, and the
/// retained surface every frame is drawn into.
///
/// The surface is held for the life of the window because allocating and
/// zeroing one per present would be a whole-window pass of its own, and holding
/// it is what makes a clipped repaint sound — every pixel outside the clip is
/// the one already on screen.
struct Pane {
    window: u64,
    frames: WindowFrames,
    mode: DisplayMode,
    surface: Surface,
}

/// The live window channel an app owns, and the window it may or may not have
/// open.
///
/// An app is on the icon bar whether or not a window is open, so the channel
/// outlives every window that crosses it.
pub struct AppWindow {
    client: WindowClient<RtWindowTransport>,
    pane: Option<Pane>,
}

impl AppWindow {
    /// A channel with no window open yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            client: WindowClient::new(RtWindowTransport),
            pane: None,
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
        self.pane.is_some()
    }

    /// The open window's id, or `None` with none open.
    #[must_use]
    pub const fn window_id(&self) -> Option<u64> {
        match &self.pane {
            Some(pane) => Some(pane.window),
            None => None,
        }
    }

    /// Whether the session has released its copy of the window's pixels and
    /// this side has given the region back.
    ///
    /// A released region holds none of the pixels a partial present would
    /// leave standing, so a caller that resolves a *reported* damage set
    /// before presenting promotes it to the whole window when this is true —
    /// otherwise a round that reports nothing would leave the window blank.
    /// [`Self::present`] performs the same promotion for the rectangle it is
    /// handed; this is for the callers that must decide earlier.
    #[must_use]
    pub const fn content_released(&self) -> bool {
        match &self.pane {
            Some(pane) => pane.frames.is_released(),
            None => false,
        }
    }

    /// The open window's current shape, or `None` with none open.
    #[must_use]
    pub const fn mode(&self) -> Option<&DisplayMode> {
        match &self.pane {
            Some(pane) => Some(&pane.mode),
            None => None,
        }
    }

    /// Open a window of `mode`, titled `title`, sized as `sizing` allows, and
    /// answer the serving session's [`ProcId`] from the create reply.
    ///
    /// # Errors
    ///
    /// [`EXIT_NO_FRAMES`] when the shared region could not be created or
    /// granted, and [`EXIT_NO_WINDOW`] when the drawing surface could not be
    /// allocated or the session refused the create. A window already being open
    /// is likewise [`EXIT_NO_WINDOW`]: an app that asks for a second through
    /// this one channel has lost track of the first.
    pub fn open(
        &mut self,
        event_endpoint: u64,
        mode: &DisplayMode,
        title: &str,
        sizing: WindowSizing,
    ) -> Result<ProcId, ShellError> {
        if self.pane.is_some() {
            return Err(ShellError::new(
                EXIT_NO_WINDOW,
                "a window is already open",
                Errno::AlreadyExists,
            ));
        }
        let Some(frames) = WindowFrames::create(region_bytes(mode, FRAME_COUNT)) else {
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
        let Some(surface) = Surface::new(mode.width_px, mode.height_px) else {
            return Err(ShellError::new(
                EXIT_NO_WINDOW,
                "no memory for the window surface",
                Errno::OutOfMemory,
            ));
        };
        let (window, server) = self
            .client
            .create(grant, event_endpoint, FRAME_COUNT, mode, title, sizing)
            .map_err(|err| {
                ShellError::new(EXIT_NO_WINDOW, "desktop session refused the window", err)
            })?;
        self.pane = Some(Pane {
            window,
            frames,
            mode: *mode,
            surface,
        });
        Ok(server)
    }

    /// Draw `damage` of the window through `paint` and present that rectangle.
    ///
    /// A region the session released while the window was hidden is re-attached
    /// first and presented whole, because it holds none of the pixels a partial
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
        let Some(pane) = self.pane.as_mut() else {
            return Ok(());
        };
        let damage = if pane.frames.is_released() {
            DamageRect::full(&pane.mode)
        } else {
            damage
        };
        pane.surface
            .with_clip(damage.x, damage.y, damage.width_px, damage.height_px, paint);
        let pixels = self
            .client
            .frame_pixels(&mut pane.frames, pane.window, FRAME_COUNT, &pane.mode)
            .ok_or(Errno::NotAttached)?;
        winframe::encode(&pane.surface, pixels, &pane.mode, damage, &SERIAL)?;
        self.client.present(pane.window, 0, damage)
    }

    /// Re-map the frame region onto `new_mode`, answering whether the new
    /// geometry was adopted.
    ///
    /// The ordering is fail-closed: a fresh region and a fresh drawing surface
    /// are allocated and the region granted **first**, and adopted only if the
    /// session accepts the resize. On success the old region is unmapped by
    /// being dropped — never before, so a refused resize leaves the current
    /// surface intact; on refusal the freshly-allocated region is unmapped so
    /// nothing leaks. Anything that cannot be allocated at all keeps the
    /// current size rather than crashing or presenting nothing.
    ///
    /// `false` therefore means "still at the old size, and still drawable", not
    /// "broken". The caller repaints the whole window either way, since even a
    /// refused resize leaves the reported client size unchanged and the current
    /// picture already matches it.
    pub fn resize(&mut self, new_mode: DisplayMode) -> bool {
        let Some(pane) = self.pane.as_mut() else {
            return false;
        };
        let Some(spare) = WindowFrames::create(region_bytes(&new_mode, FRAME_COUNT)) else {
            return false;
        };
        let Some(surface) = Surface::new(new_mode.width_px, new_mode.height_px) else {
            return false;
        };
        let Some(grant) = spare.grant() else {
            return false;
        };
        if self
            .client
            .resize(pane.window, grant, FRAME_COUNT, &new_mode)
            .is_err()
        {
            return false;
        }
        pane.frames = spare;
        pane.mode = new_mode;
        pane.surface = surface;
        true
    }

    /// Answer the session's release of its own copy by giving this side's
    /// region back, so the pages are actually freed.
    ///
    /// The pages only go when both halves let go, which is the whole point.
    pub fn release_frames(&mut self) {
        if let Some(pane) = self.pane.as_mut() {
            pane.frames.release();
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
        match self.pane.take() {
            Some(pane) => self.client.close(pane.window),
            None => Ok(()),
        }
    }
}

impl Default for AppWindow {
    fn default() -> Self {
        Self::new()
    }
}
