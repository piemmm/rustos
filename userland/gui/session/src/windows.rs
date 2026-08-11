//! The session-side composition of served application windows
//! (`plans/APPWIN.md` AW3).
//!
//! [`SessionWindows`] owns the session's window bookkeeping — the map
//! between the window channel's session-minted ids and the compositor's
//! [`WindowId`]s, plus each window's persistent content surface — and
//! [`ShellWindowHost`] is the [`WindowHost`](tairix_window::WindowHost) bridge the
//! `tairix_window::WindowServer` drives: an accepted `Create` opens a
//! desktop window (cascaded, focused, listed on the taskbar), a
//! validated `Present` converts exactly the damaged pixels of the app's
//! shared frame into the window's surface, and a `Close` (or a dead
//! client's teardown) removes the window and its task entry.
//!
//! The engine has already validated everything that reaches this bridge
//! (ownership, frame bounds, damage-in-surface); the bridge still
//! indexes fail-closed and refuses rather than guesses when a record and
//! its frame disagree.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::desktop::DesktopInfo;
use tairix_abi::driver::display::{DamageRect, DisplayFormat, DisplayMode};
use tairix_abi::window_ipc::WindowEvent;
use tairix_abi::{Errno, ProcId};
use tairix_icon::{IconKind, IconRequest};
use tairix_window::PinDecision;
use tairix_wm::{Color, Compositor, Point, Rect, Surface, WindowControlKind, WindowId};

use crate::launch::LaunchTable;
use crate::picker::PickerSlot;
use crate::pins::{
    bundle_icon_source, bundle_manifest_path, decode_bundle_manifest, PinBridge, BUNDLE_RUN_SUFFIX,
};
use crate::shell::DesktopShell;

/// The freshly opened window's fill until the app's first present lands:
/// an opaque near-black, so an app that is slow to render shows a blank
/// window body rather than stale or transparent pixels.
const OPEN_FILL: Color = Color::rgb(0x20, 0x20, 0x24);

/// Top-left of the first opened window, in screen pixels. Public so a
/// host-side observer (the AW3 QEMU vertical's screendump assertion)
/// measures the served window where the session actually places it,
/// never a re-derived guess.
pub const CASCADE_ORIGIN: i32 = 48;

/// Cascade step between successively opened windows, in screen pixels.
const CASCADE_STEP: i32 = 32;

/// Number of cascade steps before the placement wraps back to
/// [`CASCADE_ORIGIN`], so late windows never walk off screen.
const CASCADE_WRAP: i32 = 8;

/// One served window's session-side state.
struct WindowRecord {
    /// The compositor window presenting this served window. The window's
    /// content surface lives there and nowhere else: a present converts
    /// its damaged pixels straight into that one owned buffer
    /// (`Compositor::present_window_content`), so the session keeps no
    /// second copy to convert into and clone from.
    wm: WindowId,
    /// For a popup surface, the compositor window of the parent it is glued
    /// above; `None` for a top-level window. Held as the compositor id
    /// because that is what the stacking coupling needs, and a window's
    /// compositor id never changes while it lives.
    parent: Option<WindowId>,
}

/// The session's bookkeeping for every live served window.
#[derive(Default)]
pub struct SessionWindows {
    /// Window-channel id → session-side record.
    records: BTreeMap<u64, WindowRecord>,
    /// Compositor id → window-channel id, for routing input back to the
    /// owning app.
    by_wm: BTreeMap<WindowId, u64>,
    /// Monotonic count of opens, driving the cascade placement.
    opened: u64,
    /// How many live records are popups. Kept beside the map so the
    /// per-wake stacking pass costs one integer test on the overwhelmingly
    /// common wake with no popup open, instead of walking every record.
    popups: usize,
    /// Windows opened since the last drain, each with the kernel-attested
    /// process that opened it, awaiting identification of the application
    /// they belong to.
    opened_owners: Vec<(WindowId, ProcId)>,
    /// Window-channel ids that successfully presented content since the
    /// last frame-report decision. Drained with the report so a frame whose
    /// only content is the Switchboard's own paint is not reported back.
    presented: Vec<u64>,
}

impl SessionWindows {
    /// An empty window table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The window-channel id of the served window shown as `wm`, if any
    /// (the taskbar and popup windows are not served windows).
    #[must_use]
    pub fn ipc_id(&self, wm: WindowId) -> Option<u64> {
        self.by_wm.get(&wm).copied()
    }

    /// The compositor window showing the served window `ipc`, if it is live.
    #[must_use]
    pub fn wm_id(&self, ipc: u64) -> Option<WindowId> {
        self.records.get(&ipc).map(|record| record.wm)
    }

    /// Window-channel ids that successfully presented since the last take,
    /// clearing the set so the next report decision starts fresh.
    pub fn take_presented(&mut self) -> Vec<u64> {
        core::mem::take(&mut self.presented)
    }

    /// Every live served window, as `(window-channel id, compositor id)`
    /// pairs in channel-id order — how the embedder walks the served windows
    /// to find one belonging to a given app (resolving each id's owner
    /// through the window engine's attested records).
    pub fn served(&self) -> impl Iterator<Item = (u64, WindowId)> + '_ {
        self.records.iter().map(|(&ipc, record)| (ipc, record.wm))
    }

    /// Number of live served windows.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// `true` when no served window is live.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// The cascade origin for the next opened window.
    fn next_origin(&self) -> Point {
        cascade_origin_for(self.opened)
    }

    /// Take the windows opened since the last call, each paired with the
    /// kernel-attested process that opened it.
    ///
    /// The owning application is named *after* the request that opened the
    /// window is served, because both halves of the answer — the task id an
    /// attested process ran as, and the desktop's own launch records — are
    /// borrowed for the duration of that serve pass. Draining leaves the
    /// list empty, so each window is offered for identification once.
    pub fn take_opened_owners(&mut self) -> Vec<(WindowId, ProcId)> {
        core::mem::take(&mut self.opened_owners)
    }

    /// Re-assert every live popup's stacking: each popup is restacked
    /// directly above the parent that owns it.
    ///
    /// The embedder calls this immediately before each composite. A popup is
    /// its parent's transient — a menu, a sheet — so the pair is raised as a
    /// unit, which is what guarantees nothing raised elsewhere during the
    /// same wake can land between a parent and its popup. Idle (one integer
    /// test) while no popup is open.
    pub fn keep_popups_stacked(&self, compositor: &mut Compositor) {
        if self.popups == 0 {
            return;
        }
        for record in self.records.values() {
            if let Some(parent) = record.parent {
                compositor.raise(parent);
                compositor.raise(record.wm);
            }
        }
    }

    /// Record the freshly opened window `ipc`, shown as `wm` and owned by
    /// `parent` when it is a popup.
    fn insert(&mut self, ipc: u64, wm: WindowId, parent: Option<WindowId>) {
        if parent.is_some() {
            self.popups += 1;
        }
        self.records.insert(ipc, WindowRecord { wm, parent });
        self.by_wm.insert(wm, ipc);
    }

    /// Forget the window `ipc`, returning its record when it was live.
    fn take(&mut self, ipc: u64) -> Option<WindowRecord> {
        let record = self.records.remove(&ipc)?;
        self.by_wm.remove(&record.wm);
        if record.parent.is_some() {
            self.popups = self.popups.saturating_sub(1);
        }
        Some(record)
    }
}

/// Top-left of the `opened`-th served window (zero-based), in screen
/// pixels: the diagonal cascade from [`CASCADE_ORIGIN`], wrapping so late
/// windows never walk off screen. The one placement rule the session
/// applies and a host-side observer (the AW3/AW4 QEMU vertical's click
/// script and screendump assertions) measures against — never a
/// re-derived guess.
#[must_use]
pub fn cascade_origin_for(opened: u64) -> Point {
    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
    // Wrapped modulo `CASCADE_WRAP`, so the value is always tiny.
    let step = (opened % CASCADE_WRAP as u64) as i32;
    Point::new(
        CASCADE_ORIGIN + step * CASCADE_STEP,
        CASCADE_ORIGIN + step * CASCADE_STEP,
    )
}

/// Apply a title-bar window-control command to `wm`'s lifecycle and return
/// the app-ward [`WindowEvent`] the session must deliver to the owning
/// client (or `None` when the command is purely window-manager-local, or
/// `wm` is not a served window, or the command does not apply).
///
/// This is the one place the four [`WindowControlKind`]s map to lifecycle,
/// so the live serve loop and the host tests drive the same rule:
///
/// * [`Close`](WindowControlKind::Close) never destroys the window behind
///   the app's back — it returns a [`WindowEvent::CloseRequested`] so the
///   app tears down cooperatively (it decides when, having saved).
/// * [`Minimize`](WindowControlKind::Minimize) hides the window and marks
///   its taskbar entry minimised (window-manager-side), and returns a
///   [`WindowEvent::Minimized`] so the app may pause non-essential work.
/// * [`PutToBack`](WindowControlKind::PutToBack) restacks the window to the
///   bottom — a window-manager-local action with no app-ward event.
/// * [`SizeToggle`](WindowControlKind::SizeToggle) maximizes or restores
///   the window against `work_area` and returns a [`WindowEvent::Resized`]
///   carrying the new client size so the app re-lays-out; it yields `None`
///   (nothing changes) for a window that cannot maximize.
///
/// The caller delivers the returned event over the existing window path
/// (owner-validated by the engine); no new syscall and no ambient
/// authority are involved.
#[must_use]
pub fn window_control_event(
    control: WindowControlKind,
    wm: WindowId,
    work_area: Rect,
    shell: &mut DesktopShell,
    compositor: &mut Compositor,
    windows: &SessionWindows,
) -> Option<WindowEvent> {
    // Only a served window has a window-channel id and an owning app; a
    // press on any other decorated surface has nothing to route.
    let window_id = windows.ipc_id(wm)?;
    match control {
        WindowControlKind::Close => Some(WindowEvent::CloseRequested { window_id }),
        WindowControlKind::Minimize => {
            shell.minimize_window(compositor, wm);
            Some(WindowEvent::Minimized { window_id })
        }
        WindowControlKind::PutToBack => {
            compositor.lower(wm);
            None
        }
        WindowControlKind::SizeToggle => {
            let (_, client) = compositor.toggle_window_size(wm, work_area)?;
            Some(WindowEvent::Resized {
                window_id,
                width_px: client.width,
                height_px: client.height,
            })
        }
    }
}

/// The app-ward [`WindowEvent`] a secondary press on `wm`'s title-bar
/// `control` must be delivered as, or `None` when the gesture means
/// nothing here.
///
/// It is deliberately narrow: only the close control carries an alternate
/// meaning — a request the app interprets for itself (a file manager steps
/// up a folder rather than closing) — and the session performs no window
/// action of its own for it, so an app that ignores the event sees nothing
/// change. A press on any other control, or on a window the session itself
/// owns (the trusted picker, the greeter), has no owning app to tell and
/// yields `None`.
#[must_use]
pub fn window_control_alternate_event(
    control: WindowControlKind,
    wm: WindowId,
    windows: &SessionWindows,
) -> Option<WindowEvent> {
    let window_id = windows.ipc_id(wm)?;
    match control {
        WindowControlKind::Close => Some(WindowEvent::AlternateCloseRequested { window_id }),
        WindowControlKind::Minimize
        | WindowControlKind::PutToBack
        | WindowControlKind::SizeToggle => None,
    }
}

/// Give every window opened since the last pass the icon of the
/// application that opened it — on its own title bar *and* on its taskbar
/// entry, both from this one resolution, so the two can never show
/// different applications.
///
/// `windows` supplies each freshly opened compositor window paired with the
/// kernel-attested process that opened it, `task_of` resolves that process
/// to the task id the desktop's launch records are keyed by, and `launched`
/// is those records. Nothing an application sent is consulted, so no
/// program can wear another's identity.
///
/// A window whose owner this desktop did not launch — a shell-spawned
/// program, a child process — is left with no identity, so its title keeps
/// the whole band and its taskbar entry keeps the shared application icon,
/// rather than either wearing a badge for an application that cannot be
/// named. An identified bundle whose declared artwork is absent, refused,
/// or undecodable keeps the identity and loses only the picture: both
/// surfaces fall back to the shared application-bundle artwork and then to
/// their built-in glyph. Resolution never fails a window; it is already
/// open.
///
/// Each icon is resolved through the session's one artwork cache, at the
/// pixel side of the slot that draws it — the window's own title band, and
/// the bar's own task slot — so a second window of the same application
/// costs a lookup rather than a read and a decode.
pub fn resolve_window_identities<F>(
    shell: &mut DesktopShell,
    compositor: &mut Compositor,
    windows: &mut SessionWindows,
    launched: &LaunchTable,
    task_of: F,
) where
    F: Fn(ProcId) -> Option<u64>,
{
    for (wm, owner) in windows.take_opened_owners() {
        let Some(bundle) = task_of(owner)
            .and_then(|task| launched.get(task))
            .and_then(|app| app.run_path.strip_suffix(BUNDLE_RUN_SUFFIX))
        else {
            continue;
        };
        // Read the bundle's manifest once and rasterise from it per slot:
        // the two slots differ only in pixel side, and the declared source
        // is the same answer for both.
        let declared = bundle_icon_asset(shell, bundle);
        // An undecorated window draws no identity slot and reports no side;
        // it still has a taskbar entry to identify.
        if let Some(side) = compositor.window_title_icon_side(wm) {
            let artwork = bundle_artwork(shell, declared.as_deref(), side);
            compositor.set_window_identity(wm, IconKind::AppBundle, artwork);
        }
        if let Some(task) = shell.tasks().task_for(wm) {
            let side = shell.session().taskbar().task_icon_side(compositor.scale());
            let artwork = bundle_artwork(shell, declared.as_deref(), side);
            shell.set_task_artwork(compositor, task, artwork);
        }
    }
}

/// The asset path `bundle`'s own manifest declares its icon at, if it
/// declares one that reads and decodes.
///
/// `bundle` is the bundle *directory* of the application the kernel attested
/// owns the window, never a path an application sent. `None` — no manifest,
/// a manifest that will not decode, or one declaring no icon — resolves the
/// shipped application-bundle artwork instead, exactly as a pinned
/// application does.
fn bundle_icon_asset(shell: &mut DesktopShell, bundle: &str) -> Option<String> {
    let manifest_path = bundle_manifest_path(bundle);
    let (_, reader, _) = shell.artwork_parts();
    reader
        .read(&manifest_path)
        .as_deref()
        .and_then(decode_bundle_manifest)
        .and_then(|header| bundle_icon_source(&header, bundle))
        .map(|source| source.path())
}

/// The picture at `side` pixels for an application whose manifest declares
/// its icon at `declared`, resolved through the session's one artwork cache
/// and its sandboxed decode seam.
///
/// `side` is the pixel side of the slot that draws it — the window's title
/// band or the bar's task slot — so the artwork is rasterised at exactly the
/// size drawn, and a slot that has already asked for that size is served
/// from the cache. The shipped application-bundle artwork stands in for an
/// application declaring no icon of its own; `None` when neither reads or
/// decodes, leaving the slot on its built-in glyph.
fn bundle_artwork(shell: &mut DesktopShell, declared: Option<&str>, side: u32) -> Option<Surface> {
    let (cache, reader, rasteriser) = shell.artwork_parts();
    let request = declared.map_or_else(
        || IconRequest::kind(IconKind::AppBundle),
        |path| IconRequest::asset(IconKind::AppBundle, path),
    );
    cache.artwork(reader, rasteriser, request, side).cloned()
}

/// The [`WindowHost`] bridge one serve pass borrows: the desktop shell,
/// the compositor, the session's window table, and the trusted picker
/// slot a validated `PickFile` opens.
///
/// [`WindowHost`]: tairix_window::WindowHost
pub struct ShellWindowHost<'a> {
    /// The desktop shell (taskbar, focus, window list).
    pub shell: &'a mut DesktopShell,
    /// The compositor the windows are composed into.
    pub compositor: &'a mut Compositor,
    /// The session's served-window bookkeeping.
    pub windows: &'a mut SessionWindows,
    /// The session's single trusted-picker slot
    /// ([`SessionPicker`](crate::SessionPicker) in production): a
    /// validated `PickFile` opens it, and a closing window takes its own
    /// pick down with it.
    pub picker: &'a mut dyn PickerSlot,
    /// The session's pin service ([`PinService`](crate::pins::PinService)
    /// in production): a validated pin request or drag offer lands here.
    pub pins: &'a mut dyn PinBridge,
}

impl tairix_window::WindowHost for ShellWindowHost<'_> {
    fn window_opened(
        &mut self,
        owner: ProcId,
        window_id: u64,
        surface: &DisplayMode,
        title: &str,
        resizable: bool,
    ) -> Result<(), Errno> {
        // The engine validated the geometry (non-zero, stride covers a
        // row); a surface too large to allocate is refused, never a
        // panic.
        let Some(content) =
            Surface::filled(surface.width_px, surface.height_px, OPEN_FILL.premultiply())
        else {
            return Err(Errno::LengthOutOfRange);
        };
        let origin = self.windows.next_origin();
        // The compositor takes ownership of the content surface; the
        // session keeps only the id mapping, never a second copy.
        let Some(wm) = self
            .shell
            .open_window(self.compositor, origin, content, title)
        else {
            return Err(Errno::LengthOutOfRange);
        };
        // A served application window is decorated by the window manager: the
        // title bar (with the Close / Minimize / PutToBack / SizeToggle
        // controls) and the frame rim are composed around the app's content.
        // The app never draws its own chrome; it reacts to the typed lifecycle
        // events the controls raise over the window path. The app's own
        // `resizable` request decides whether a live size toggle and the
        // invisible resize edges — which overlap the client's outer pixels
        // rather than reserving a visible band — are offered.
        self.shell
            .decorate_window(self.compositor, wm, title, resizable);
        // A served window's pixels come from the app, which the session can
        // ask to present them again, so the compositor may give them back
        // under memory pressure. Windows the session paints itself (the
        // taskbar, the picker, a confirmation prompt) never declare this
        // and so are never released: there would be no client to ask.
        self.compositor.set_app_presented(wm, true);
        self.windows.opened += 1;
        self.windows.insert(window_id, wm, None);
        // Who owns this window is the kernel's answer, kept for the
        // identification pass that runs once this request is served.
        self.windows.opened_owners.push((wm, owner));
        Ok(())
    }

    fn popup_opened(
        &mut self,
        window_id: u64,
        parent_window_id: u64,
        offset_x: i32,
        offset_y: i32,
        surface: &DisplayMode,
    ) -> Result<(), Errno> {
        // An app is never told where its own window sits, so the offset it
        // asked for is relative to its parent's client origin and the
        // absolute point is the session's to resolve. A parent the session
        // has no window for refuses the popup rather than placing it
        // somewhere invented.
        let Some(parent) = self.windows.wm_id(parent_window_id) else {
            return Err(Errno::NotFound);
        };
        let Some(client) = self.compositor.window_client_rect(parent) else {
            return Err(Errno::NotFound);
        };
        let Some(content) =
            Surface::filled(surface.width_px, surface.height_px, OPEN_FILL.premultiply())
        else {
            return Err(Errno::LengthOutOfRange);
        };
        let placed = Rect::new(
            client.left().saturating_add(offset_x),
            client.top().saturating_add(offset_y),
            surface.width_px,
            surface.height_px,
        )
        .clamped_onto(self.compositor.screen_rect());
        // Undecorated on purpose: a popup is a transient its parent owns, so
        // it wears no title bar, no controls, and no taskbar entry, and the
        // app that opened it is what dismisses it.
        let wm = self
            .shell
            .open_popup_window(self.compositor, placed.origin, content);
        self.compositor.set_app_presented(wm, true);
        self.windows.insert(window_id, wm, Some(parent));
        Ok(())
    }

    fn window_presented(
        &mut self,
        window_id: u64,
        surface: &DisplayMode,
        frame: &[u8],
        damage: DamageRect,
    ) -> Result<(), Errno> {
        let Some(record) = self.windows.records.get(&window_id) else {
            return Err(Errno::NotFound);
        };
        let wm = record.wm;
        // Convert exactly the damaged pixels of the presented frame
        // directly into the compositor's own window surface — the single
        // owned copy — and mark dirty only the pixels the conversion
        // genuinely changed, so an app that repaints its whole
        // composition for a one-row highlight costs one row of
        // recomposition rather than a whole window. The presented mode is
        // what that surface is sized from, so a frame drawn at a geometry
        // the window manager has already moved on from still lands: the
        // frame around the client is the session's, the pixels inside it
        // are the app's. The engine validated the damage against the
        // window's surface and handed a frame slice sized from the mode,
        // but every index in `convert_damage` is still checked: a
        // disagreement refuses the present rather than reading out of
        // bounds, and refuses it before writing anything. A window the
        // compositor no longer knows, or one whose pixels cannot be
        // allocated, fails closed.
        let Some(result) = self.compositor.present_window_content(
            wm,
            surface.width_px,
            surface.height_px,
            |content| match convert_damage(content, surface, frame, damage) {
                Ok(changed) => (Ok(()), changed),
                Err(err) => (Err(err), Rect::EMPTY),
            },
        ) else {
            return Err(Errno::NotFound);
        };
        result?;
        // A successful present is what the frame-report gate classifies: the
        // Switchboard measuring itself must not re-excite a report.
        if !self.windows.presented.contains(&window_id) {
            self.windows.presented.push(window_id);
        }
        Ok(())
    }

    fn window_resized(&mut self, window_id: u64, surface: &DisplayMode) -> Result<(), Errno> {
        // The engine validated the new geometry and re-mapped the frame
        // region; move the window's frame to reserve the new client size, so
        // the furniture and the pointer's idea of the window agree with what
        // the app has re-mapped. The app's own pixels arrive with its next
        // present, which is what sizes their buffer. The window id →
        // compositor id mapping is unchanged by a resize.
        let Some(record) = self.windows.records.get(&window_id) else {
            return Err(Errno::NotFound);
        };
        if self
            .compositor
            .resize_window_client(record.wm, surface.width_px, surface.height_px)
        {
            Ok(())
        } else {
            // An empty client size, or a window the compositor no longer
            // knows: refuse the resize (fail closed), leaving the old
            // geometry the engine will keep in step.
            Err(Errno::LengthOutOfRange)
        }
    }

    fn window_retitled(&mut self, window_id: u64, title: &str) -> Result<(), Errno> {
        // The engine attested the caller and validated that the window is
        // its own, and bounded the title on decode. One shell call moves
        // the title bar and the taskbar entry together, so the two can
        // never name different subjects. A popup carries neither, and a
        // window the session no longer knows fails closed.
        let Some(record) = self.windows.records.get(&window_id) else {
            return Err(Errno::NotFound);
        };
        if self.shell.retitle_window(self.compositor, record.wm, title) {
            Ok(())
        } else {
            Err(Errno::NotFound)
        }
    }

    fn window_closed(&mut self, window_id: u64) {
        // A window that dies mid-pick takes its picker down with it: the
        // engine already dropped the pending pick with the record, so no
        // conclusion is (or could be) delivered.
        self.picker
            .abort_for(window_id, self.shell, self.compositor);
        if let Some(record) = self.windows.take(window_id) {
            // A popup was never a task, so it leaves through the taskbar-less
            // path; the engine tears a parent's popups down with it, so each
            // arrives here in its own turn.
            let _ = if record.parent.is_some() {
                self.shell.close_popup_window(self.compositor, record.wm)
            } else {
                self.shell.close_window(self.compositor, record.wm)
            };
        }
    }

    fn pick_requested(&mut self, window_id: u64) -> Result<(), Errno> {
        // The engine already validated ownership and the per-window
        // single-pending rule; the slot enforces the session's own
        // modality (one picker at a time) and brings the UI up under the
        // session's authority, refusing fail-closed when it cannot.
        self.picker.begin(window_id, self.shell, self.compositor)
    }

    fn pin_requested(&mut self, _owner: ProcId, _window: u64, path: &str) -> PinDecision {
        // The engine attested the caller and validated window ownership;
        // the pin service validates the bundle itself and applies (or
        // refuses) the edit under the session's own identity.
        self.pins.pin_requested(path)
    }

    fn drag_offered(&mut self, _owner: ProcId, window: u64, path: &str) -> bool {
        self.pins.drag_offered(window, path)
    }

    fn drag_withdrawn(&mut self, _owner: ProcId, window: u64) {
        self.pins.drag_withdrawn(window);
    }

    fn backdrop_blur_set(&mut self, window_id: u64, radius_px: u16) {
        // The engine attested the caller and validated that the window is
        // one of its own, and bounded the radius on decode; the effect
        // reaches only the window's own rectangle, so the compositor needs
        // no further authority to apply it. A window the session no longer
        // has a record for frosts nothing rather than guessing which
        // window was meant.
        if let Some(record) = self.windows.records.get(&window_id) {
            self.compositor.set_backdrop_blur(record.wm, radius_px);
        }
    }

    fn desktop(&mut self) -> Result<DesktopInfo, Errno> {
        desktop_info(self.compositor)
    }
}

/// The desktop `compositor` composites, as the window channel reports it
/// to an application.
///
/// One definition for both directions: the answer an application's query
/// receives, and the announcement the session pushes when any of it
/// changes. All three facts come from the compositor — it owns the output
/// it scans out to, that output's density, and the active theme — so an
/// application reads the very values the desktop draws itself with rather
/// than a copy that could drift.
///
/// # Errors
///
/// [`Errno::OutOfRange`] for an output the record cannot describe: a
/// zero-sized screen, or a density outside the percentage the wire
/// carries. The query is refused rather than answered with a plausible
/// guess, so an application never lays itself out to a screen that is not
/// there.
pub fn desktop_info(compositor: &Compositor) -> Result<DesktopInfo, Errno> {
    let screen = compositor.screen_rect();
    let scale = u16::try_from(compositor.scale().percent()).map_err(|_| Errno::OutOfRange)?;
    DesktopInfo::new(
        screen.width,
        screen.height,
        scale,
        compositor.theme().appearance(),
    )
}

/// Convert the pixels of `damage` from the presented `frame` (laid out
/// per `mode`) into `surface`, returning the sub-rectangle of `damage`
/// whose pixels the conversion actually *changed* — [`Rect::EMPTY`] when
/// the presented pixels were identical to the ones already there.
///
/// The returned rectangle is the window's real damage, and it is what
/// keeps a repaint proportional to the change. An app re-renders its
/// whole composition and presents whole-window damage, because a toolkit
/// generally cannot say which pixels its own paint touched; taking that
/// claim at face value would recomposite every pixel of the window for a
/// hover highlight a few rows tall. The comparison is exact — a pixel
/// reported unchanged carries the byte-identical value it already had —
/// and costs one extra read on a loop that already reads the frame and
/// writes the surface.
///
/// Fails closed: every index the conversion will use is validated before
/// the first write, so a malformed or hostile geometry refuses the whole
/// present rather than leaving the window half-converted.
fn convert_damage(
    surface: &mut Surface,
    mode: &DisplayMode,
    frame: &[u8],
    damage: DamageRect,
) -> Result<Rect, Errno> {
    let bpp = mode.format.bytes_per_pixel() as usize;
    let x_end = damage
        .x
        .checked_add(damage.width_px)
        .ok_or(Errno::OutOfRange)?;
    let y_end = damage
        .y
        .checked_add(damage.height_px)
        .ok_or(Errno::OutOfRange)?;
    if x_end > surface.width() || y_end > surface.height() {
        return Err(Errno::OutOfRange);
    }
    // The alpha byte is the app's own and honoured (premultiplied for the
    // compositor's blend), so an app can render translucent regions. A
    // format this bridge does not know how to convert is refused, never
    // guessed (the enum is non-exhaustive by design). Resolving it once
    // keeps the decision off the per-pixel path.
    let decode: fn([u8; 4]) -> Color = match mode.format {
        DisplayFormat::Rgba8888 => |b| Color::rgba(b[0], b[1], b[2], b[3]),
        DisplayFormat::Bgra8888 => |b| Color::rgba(b[2], b[1], b[0], b[3]),
        _ => return Err(Errno::OutOfRange),
    };
    let stride = mode.stride_bytes as usize;
    let start = (damage.x as usize)
        .checked_mul(bpp)
        .ok_or(Errno::OutOfRange)?;
    let span = (damage.width_px as usize)
        .checked_mul(bpp)
        .ok_or(Errno::OutOfRange)?;
    // Prove every row of the request is inside `frame` before a single
    // pixel is written: a refusal must leave the window exactly as it
    // was, never half-converted.
    let row_offset = |y: u32| -> Result<usize, Errno> {
        (y as usize)
            .checked_mul(stride)
            .and_then(|row| row.checked_add(start))
            .ok_or(Errno::OutOfRange)
    };
    for y in damage.y..y_end {
        let offset = row_offset(y)?;
        let end = offset.checked_add(span).ok_or(Errno::OutOfRange)?;
        if end > frame.len() {
            return Err(Errno::OutOfRange);
        }
    }

    let columns = damage.width_px as usize;
    let mut changed = DamageBounds::default();
    for y in damage.y..y_end {
        let offset = row_offset(y)?;
        let (Some(source), Some((_, target))) = (
            frame.get(offset..offset.saturating_add(span)),
            surface.row_span_mut(y, damage.x, damage.width_px),
        ) else {
            return Err(Errno::OutOfRange);
        };
        // The surface span is the width the request claimed, or the whole
        // present is refused: a row shortened for any reason must never
        // silently convert part of a scanline.
        if target.len() != columns {
            return Err(Errno::OutOfRange);
        }
        // One row address and one bounds check per row, not per pixel: this
        // loop runs over every damaged pixel of every application repaint.
        for (index, (chunk, slot)) in source.chunks_exact(bpp).zip(target).enumerate() {
            let [b0, b1, b2, b3, ..] = *chunk else {
                return Err(Errno::OutOfRange);
            };
            let pixel = decode([b0, b1, b2, b3]).premultiply();
            if *slot == pixel {
                continue;
            }
            *slot = pixel;
            let Ok(step) = u32::try_from(index) else {
                return Err(Errno::OutOfRange);
            };
            changed.include(damage.x.saturating_add(step), y);
        }
    }
    changed.rect()
}

/// The bounding box of the pixels a conversion changed, accumulated as
/// inclusive edges so an untouched conversion stays distinguishable from
/// one that changed the single pixel at the origin.
#[derive(Default)]
struct DamageBounds {
    edges: Option<(u32, u32, u32, u32)>,
}

impl DamageBounds {
    /// Grow the box to cover the changed pixel `(x, y)`.
    fn include(&mut self, x: u32, y: u32) {
        self.edges = Some(match self.edges {
            None => (x, y, x, y),
            Some((left, top, right, bottom)) => {
                (left.min(x), top.min(y), right.max(x), bottom.max(y))
            }
        });
    }

    /// The accumulated box as a surface-local rectangle, or
    /// [`Rect::EMPTY`] when nothing changed. Fails closed rather than
    /// wrapping if a surface is somehow wider than the coordinate space
    /// the compositor addresses.
    fn rect(self) -> Result<Rect, Errno> {
        let Some((left, top, right, bottom)) = self.edges else {
            return Ok(Rect::EMPTY);
        };
        let (Ok(x), Ok(y)) = (i32::try_from(left), i32::try_from(top)) else {
            return Err(Errno::OutOfRange);
        };
        Ok(Rect::new(
            x,
            y,
            right.saturating_sub(left).saturating_add(1),
            bottom.saturating_sub(top).saturating_add(1),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tairix_abi::driver::display::{DamageRect, DisplayFormat, DisplayMode};
    use tairix_reclaim::{PressureBand, ReportedPressure};
    use tairix_taskbar::TaskbarConfig;
    use tairix_window::WindowHost;
    use tairix_wm::{InputEvent, PointerButton};

    use crate::tests::window_owner;

    fn mode(width: u32, height: u32, format: DisplayFormat) -> DisplayMode {
        DisplayMode {
            width_px: width,
            height_px: height,
            stride_bytes: width * 4,
            format,
        }
    }

    fn desktop() -> (DesktopShell, Compositor) {
        let shell = crate::tests::shell_for(TaskbarConfig::bottom_bar(640, 480));
        let compositor = Compositor::new(
            mode(640, 480, DisplayFormat::Rgba8888),
            Color::rgb(0, 0, 0),
            crate::tests::test_chrome_cache(),
            crate::tests::test_frost_cache(),
            crate::tests::test_pressure(),
        )
        .expect("compositor builds");
        (shell, compositor)
    }

    /// A picker slot recording the bridge's calls: these tests exercise
    /// the window lifecycle, not the picker (which has its own suite in
    /// `crate::tests`), so the slot only observes.
    #[derive(Default)]
    struct RecordingSlot {
        begun: alloc::vec::Vec<u64>,
        aborted: alloc::vec::Vec<u64>,
    }

    impl crate::picker::PickerSlot for RecordingSlot {
        fn begin(
            &mut self,
            for_window: u64,
            _shell: &mut DesktopShell,
            _compositor: &mut Compositor,
        ) -> Result<(), Errno> {
            self.begun.push(for_window);
            Ok(())
        }

        fn abort_for(
            &mut self,
            window_id: u64,
            _shell: &mut DesktopShell,
            _compositor: &mut Compositor,
        ) {
            self.aborted.push(window_id);
        }
    }

    /// A pin seam that refuses everything: these tests exercise the window
    /// lifecycle, not pinning (which has its own suite in `crate::tests`).
    struct RefusingPins;

    impl crate::pins::PinBridge for RefusingPins {
        fn pin_requested(&mut self, _path: &str) -> PinDecision {
            PinDecision::Refused
        }

        fn drag_offered(&mut self, _window: u64, _path: &str) -> bool {
            false
        }

        fn drag_withdrawn(&mut self, _window: u64) {}
    }

    /// An accepted open composes a focused desktop window, records both
    /// id mappings, and cascades successive origins.
    #[test]
    fn open_composes_a_window_and_maps_both_ids() {
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                pins: &mut RefusingPins,
            };
            host.window_opened(
                window_owner(1),
                7,
                &mode(64, 48, DisplayFormat::Rgba8888),
                "files",
                false,
            )
            .expect("opens");
            host.window_opened(
                window_owner(1),
                9,
                &mode(64, 48, DisplayFormat::Rgba8888),
                "terminal",
                false,
            )
            .expect("opens");
        }
        assert_eq!(windows.len(), 2);
        let wm_of_7 = windows.records.get(&7).expect("recorded").wm;
        assert_eq!(windows.ipc_id(wm_of_7), Some(7));
        let origin_7 = compositor.window(wm_of_7).expect("live").origin();
        let wm_of_9 = windows.records.get(&9).expect("recorded").wm;
        let origin_9 = compositor.window(wm_of_9).expect("live").origin();
        assert_ne!(origin_7, origin_9, "successive opens cascade");
    }

    /// A present converts exactly the damaged pixels — in both channel
    /// orders — into the composed window's surface, leaving undamaged
    /// content intact.
    #[test]
    fn present_converts_damaged_pixels_in_both_formats() {
        for (format, bytes, want) in [
            (
                DisplayFormat::Rgba8888,
                [0x11u8, 0x22, 0x33, 0xFF],
                Color::rgba(0x11, 0x22, 0x33, 0xFF),
            ),
            (
                DisplayFormat::Bgra8888,
                [0x33u8, 0x22, 0x11, 0xFF],
                Color::rgba(0x11, 0x22, 0x33, 0xFF),
            ),
        ] {
            let (mut shell, mut compositor) = desktop();
            let mut windows = SessionWindows::new();
            let mut picker = RecordingSlot::default();
            let m = mode(4, 4, format);
            {
                let mut host = ShellWindowHost {
                    shell: &mut shell,
                    compositor: &mut compositor,
                    windows: &mut windows,
                    picker: &mut picker,
                    pins: &mut RefusingPins,
                };
                host.window_opened(window_owner(1), 1, &m, "w", false)
                    .expect("opens");
                // One frame with the probe pixel at (2, 1).
                let mut frame = [0u8; 4 * 4 * 4];
                let offset = (4 + 2) * 4;
                frame[offset..offset + 4].copy_from_slice(&bytes);
                host.window_presented(
                    1,
                    &m,
                    &frame,
                    DamageRect {
                        x: 2,
                        y: 1,
                        width_px: 1,
                        height_px: 1,
                    },
                )
                .expect("presents");
            }
            // The window's one surface lives in the compositor; the
            // present converted the damaged pixel straight into it.
            let wm = windows.records.get(&1).expect("live").wm;
            let content = compositor
                .window(wm)
                .expect("composited")
                .content()
                .expect("content is retained");
            assert_eq!(content.get(2, 1), Some(want.premultiply()));
            // Undamaged pixels keep the open fill.
            assert_eq!(content.get(0, 0), Some(OPEN_FILL.premultiply()));
        }
    }

    /// A present whose damage or frame disagrees with the recorded
    /// surface refuses fail-closed instead of indexing out of bounds,
    /// and an unknown window is `NotFound`.
    #[test]
    fn present_refuses_bad_damage_short_frames_and_unknown_windows() {
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let mut host = ShellWindowHost {
            shell: &mut shell,
            compositor: &mut compositor,
            windows: &mut windows,
            picker: &mut picker,
            pins: &mut RefusingPins,
        };
        let m = mode(4, 4, DisplayFormat::Rgba8888);
        host.window_opened(window_owner(1), 1, &m, "w", false)
            .expect("opens");
        let frame = [0u8; 4 * 4 * 4];
        let full = DamageRect {
            x: 0,
            y: 0,
            width_px: 4,
            height_px: 4,
        };
        // Damage outside the surface.
        assert_eq!(
            host.window_presented(
                1,
                &m,
                &frame,
                DamageRect {
                    x: 3,
                    y: 3,
                    width_px: 2,
                    height_px: 2
                }
            ),
            Err(Errno::OutOfRange)
        );
        // A frame shorter than the damage needs.
        assert_eq!(
            host.window_presented(1, &m, &frame[..8], full),
            Err(Errno::OutOfRange)
        );
        // An unknown window.
        assert_eq!(
            host.window_presented(99, &m, &frame, full),
            Err(Errno::NotFound)
        );
    }

    /// A window-manager resize the app has not been told about yet must not
    /// refuse the app's next present.
    ///
    /// A resize-grab shrinks the window's frame on every motion and the app
    /// is told once, when the drag settles, so an app draining a backlog of
    /// input presents at the geometry it last knew while the frame is already
    /// smaller. That present is stale, not hostile: the frame is the window
    /// manager's, the pixels are the client's, and the client's frame is what
    /// its buffer is sized from. Refusing it is indistinguishable from a dead
    /// session, which is what an app exits on.
    #[test]
    fn present_survives_a_frame_resize_the_app_has_not_seen() {
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let m = mode(8, 8, DisplayFormat::Rgba8888);
        let full = DamageRect {
            x: 0,
            y: 0,
            width_px: 8,
            height_px: 8,
        };
        let frame = [0x40u8; 8 * 8 * 4];
        let wm = {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                pins: &mut RefusingPins,
            };
            host.window_opened(window_owner(1), 1, &m, "w", true)
                .expect("opens");
            host.window_presented(1, &m, &frame, full)
                .expect("the first present lands");
            host.windows.records.get(&1).expect("live").wm
        };
        // One motion of a resize-grab: the frame shrinks below the client
        // geometry the app is still drawing at.
        let outer = compositor.window(wm).expect("live").bounds();
        assert!(compositor.resize_window(
            wm,
            Rect::new(
                outer.origin.x,
                outer.origin.y,
                outer.width - 4,
                outer.height - 4,
            ),
        ));
        assert_eq!(
            compositor.window(wm).expect("live").client_size(),
            (4, 4),
            "the window manager shrank the frame it draws"
        );
        let mut host = ShellWindowHost {
            shell: &mut shell,
            compositor: &mut compositor,
            windows: &mut windows,
            picker: &mut picker,
            pins: &mut RefusingPins,
        };
        let mut next = frame;
        next[0..4].copy_from_slice(&[0xFF, 0x00, 0x00, 0xFF]);
        host.window_presented(1, &m, &next, full)
            .expect("a present at the app's own geometry still lands");
        let content = compositor
            .window(wm)
            .expect("composited")
            .content()
            .expect("content is retained");
        assert_eq!(
            (content.width(), content.height()),
            (8, 8),
            "the buffer is the geometry the client presented"
        );
        assert_eq!(
            content.get(0, 0),
            Some(Color::rgba(0xFF, 0x00, 0x00, 0xFF).premultiply())
        );
    }

    /// Re-presenting pixels the window already carries marks no damage at
    /// all: an app that repaints its whole composition and claims
    /// whole-window damage must not cost a whole-window recomposite when
    /// nothing it drew actually differs.
    #[test]
    fn present_of_identical_pixels_marks_no_damage() {
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let m = mode(8, 8, DisplayFormat::Rgba8888);
        let frame = [0x40u8; 8 * 8 * 4];
        let full = DamageRect {
            x: 0,
            y: 0,
            width_px: 8,
            height_px: 8,
        };
        {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                pins: &mut RefusingPins,
            };
            host.window_opened(window_owner(1), 1, &m, "w", false)
                .expect("opens");
            host.window_presented(1, &m, &frame, full)
                .expect("first present lands");
        }
        // Drain the damage the open and the first present produced.
        compositor.composite();
        assert!(!compositor.has_damage());
        {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                pins: &mut RefusingPins,
            };
            host.window_presented(1, &m, &frame, full)
                .expect("the repeat present is accepted");
        }
        assert!(
            !compositor.has_damage(),
            "an identical present must not dirty a single pixel"
        );
    }

    /// A whole-window present that changes one pixel marks exactly that
    /// pixel — placed at the window's content origin, so a decorated
    /// window's frame is never dragged into the damage.
    #[test]
    fn present_marks_only_the_pixels_that_changed() {
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let m = mode(8, 8, DisplayFormat::Rgba8888);
        let mut frame = [0x40u8; 8 * 8 * 4];
        let full = DamageRect {
            x: 0,
            y: 0,
            width_px: 8,
            height_px: 8,
        };
        {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                pins: &mut RefusingPins,
            };
            host.window_opened(window_owner(1), 1, &m, "w", false)
                .expect("opens");
            host.window_presented(1, &m, &frame, full)
                .expect("first present lands");
        }
        compositor.composite();
        let wm = windows.records.get(&1).expect("live").wm;
        let client = compositor.window(wm).expect("composited").client_rect();
        // Change the single content pixel at (5, 3).
        let offset = (3 * 8 + 5) * 4;
        frame[offset..offset + 4].copy_from_slice(&[0xFF, 0x00, 0x00, 0xFF]);
        {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                pins: &mut RefusingPins,
            };
            host.window_presented(1, &m, &frame, full)
                .expect("the second present lands");
        }
        assert_eq!(
            compositor.composite().bounds(),
            Rect::new(client.left() + 5, client.top() + 3, 1, 1),
            "only the one changed pixel is recomposited"
        );
    }

    /// A frame too short for the requested damage is refused *before* any
    /// pixel is written, so a rejected present can never leave the window
    /// half-converted.
    #[test]
    fn a_refused_present_writes_nothing() {
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let m = mode(8, 8, DisplayFormat::Rgba8888);
        // Long enough for the first rows, short of the last.
        let frame = [0xFFu8; 8 * 6 * 4];
        {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                pins: &mut RefusingPins,
            };
            host.window_opened(window_owner(1), 1, &m, "w", false)
                .expect("opens");
            assert_eq!(
                host.window_presented(
                    1,
                    &m,
                    &frame,
                    DamageRect {
                        x: 0,
                        y: 0,
                        width_px: 8,
                        height_px: 8,
                    },
                ),
                Err(Errno::OutOfRange)
            );
        }
        let wm = windows.records.get(&1).expect("live").wm;
        let content = compositor
            .window(wm)
            .expect("composited")
            .content()
            .expect("content is retained");
        assert_eq!(
            content.get(0, 0),
            Some(OPEN_FILL.premultiply()),
            "the refused present left the opening fill intact"
        );
    }

    /// A close removes the window from the compositor and both maps; a
    /// second close of the same id is a no-op.
    #[test]
    fn close_removes_the_window_and_its_mappings() {
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let mut host = ShellWindowHost {
            shell: &mut shell,
            compositor: &mut compositor,
            windows: &mut windows,
            picker: &mut picker,
            pins: &mut RefusingPins,
        };
        let m = mode(8, 8, DisplayFormat::Rgba8888);
        host.window_opened(window_owner(1), 1, &m, "w", false)
            .expect("opens");
        let wm = host.windows.records.get(&1).expect("live").wm;
        host.window_closed(1);
        assert!(host.windows.is_empty());
        assert_eq!(host.windows.ipc_id(wm), None);
        assert!(host.compositor.window(wm).is_none());
        host.window_closed(1);
        assert!(host.windows.is_empty());
    }

    #[test]
    fn window_opened_decorates_the_served_window_with_its_title() {
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let wm = {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                pins: &mut RefusingPins,
            };
            host.window_opened(
                window_owner(1),
                3,
                &mode(120, 80, DisplayFormat::Rgba8888),
                "Files",
                false,
            )
            .expect("opens");
            host.windows.records.get(&3).expect("live").wm
        };
        // The window manager decorated the served window with its channel
        // title; the app itself drew no chrome.
        let frame = compositor
            .window_frame(wm)
            .expect("the served window is decorated");
        assert_eq!(frame.title_bar().title(), "Files");
        assert!(frame.furniture().movable, "movable by its title bar");
        assert!(
            !frame.furniture().resizable,
            "the served window presents a fixed size"
        );
        // The reserved frame band grows the outer bounds; the client keeps the
        // app's requested content size and never covers the furniture.
        let client = compositor.window_client_rect(wm).expect("client");
        assert_eq!((client.width, client.height), (120, 80));
        let outer = compositor.window(wm).expect("live").bounds();
        assert!(outer.width > client.width && outer.height > client.height);
    }

    #[test]
    fn window_opened_honours_a_resizable_request() {
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let wm = {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                pins: &mut RefusingPins,
            };
            open_one_sized(&mut host, 3, true)
        };
        // The app asked to be resizable, so the window manager decorates it
        // with a resizable frame — its invisible resize edges and a live size
        // toggle. A fixed-size open (asserted separately) gets neither.
        let frame = compositor
            .window_frame(wm)
            .expect("the served window is decorated");
        assert!(frame.furniture().movable, "movable by its title bar");
        assert!(
            frame.furniture().resizable,
            "a resizable-requested window is decorated resizable"
        );
        // And the size toggle now drives a real maximize (a fixed-size window
        // yields nothing): the mechanism is live for the opted-in window.
        let work_area = shell.work_area(&compositor);
        assert!(
            matches!(
                window_control_event(
                    WindowControlKind::SizeToggle,
                    wm,
                    work_area,
                    &mut shell,
                    &mut compositor,
                    &windows,
                ),
                Some(WindowEvent::Resized { window_id: 3, .. })
            ),
            "the size toggle maximizes a resizable window and reports the new client size"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One linear end-to-end drive of all four command controls.
    fn clicking_each_title_bar_control_drives_the_window_lifecycle_end_to_end() {
        use tairix_wm::{InputEvent, InputResponse, PointerButton};

        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();

        // Open a served, decorated window exactly as the serve loop does.
        let wm = {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                pins: &mut RefusingPins,
            };
            host.window_opened(
                window_owner(1),
                7,
                &mode(480, 320, DisplayFormat::Rgba8888),
                "Files",
                false,
            )
            .expect("opens");
            host.windows.records.get(&7).expect("live").wm
        };

        // The screen centre of each command control, read from the same frame
        // and title-bar layout the compositor renders and hit-tests through, so
        // the click lands on the real furniture, never a guessed position.
        let scale = compositor.scale();
        let control_point = |compositor: &Compositor, kind: WindowControlKind| -> Point {
            let bounds = compositor.window(wm).expect("live").bounds();
            let frame = compositor.window_frame(wm).expect("decorated");
            let title_rect = frame.layout(bounds, scale, compositor.theme()).title_bar;
            let layout = frame
                .title_bar()
                .layout(title_rect, scale, compositor.theme());
            let rect = layout
                .controls
                .iter()
                .find(|(k, _)| *k == kind)
                .expect("the control has a slot")
                .1;
            #[allow(clippy::cast_possible_wrap)]
            Point::new(
                rect.left() + (rect.width / 2) as i32,
                rect.top() + (rect.height / 2) as i32,
            )
        };

        // A full primary click at `at`, returning the release outcome — a
        // command control fires on release, exactly as the input router routes
        // it live.
        let click = |shell: &mut DesktopShell,
                     compositor: &mut Compositor,
                     at: Point|
         -> crate::ShellOutcome {
            shell.handle(InputEvent::PointerMoved { to: at }, compositor, 0);
            shell.handle(
                InputEvent::PointerPressed {
                    button: PointerButton::Primary,
                },
                compositor,
                0,
            );
            shell.handle(
                InputEvent::PointerReleased {
                    button: PointerButton::Primary,
                },
                compositor,
                0,
            )
        };

        let work_area = shell.work_area(&compositor);

        // PutToBack: the router classifies the control, and the shared mapping
        // makes it a window-manager-local restack with no app-ward event.
        let at = control_point(&compositor, WindowControlKind::PutToBack);
        let outcome = click(&mut shell, &mut compositor, at);
        assert!(
            matches!(
                outcome,
                crate::ShellOutcome::WindowManager(InputResponse::WindowControl {
                    window,
                    control: WindowControlKind::PutToBack,
                }) if window == wm
            ),
            "the put-to-back control click routes to the window manager: {outcome:?}"
        );
        assert_eq!(
            window_control_event(
                WindowControlKind::PutToBack,
                wm,
                work_area,
                &mut shell,
                &mut compositor,
                &windows,
            ),
            None
        );

        // SizeToggle: on a fixed-size window the control is disabled (rendered
        // with a reason, never vanished), so pressing it activates nothing —
        // the frame consumes the press but raises no command. Even routed
        // directly, the mapping is a no-op for a non-resizable window.
        let at = control_point(&compositor, WindowControlKind::SizeToggle);
        let outcome = click(&mut shell, &mut compositor, at);
        assert!(
            matches!(
                outcome,
                crate::ShellOutcome::WindowManager(InputResponse::FurniturePressed { window })
                    if window == wm
            ),
            "the disabled size-toggle raises no command: {outcome:?}"
        );
        assert_eq!(
            window_control_event(
                WindowControlKind::SizeToggle,
                wm,
                work_area,
                &mut shell,
                &mut compositor,
                &windows,
            ),
            None,
            "a fixed-size window does not maximize"
        );

        // Close: the app tears down cooperatively — a CloseRequested event, the
        // window still alive until the app acts.
        let at = control_point(&compositor, WindowControlKind::Close);
        let outcome = click(&mut shell, &mut compositor, at);
        assert!(matches!(
            outcome,
            crate::ShellOutcome::WindowManager(InputResponse::WindowControl {
                control: WindowControlKind::Close,
                ..
            })
        ));
        assert_eq!(
            window_control_event(
                WindowControlKind::Close,
                wm,
                work_area,
                &mut shell,
                &mut compositor,
                &windows,
            ),
            Some(WindowEvent::CloseRequested { window_id: 7 })
        );
        assert!(
            compositor.window(wm).is_some(),
            "close is cooperative: the window manager never destroys it"
        );

        // Minimize (last, since it hides the window): the taskbar entry is
        // marked minimised and the app is told to pause.
        let at = control_point(&compositor, WindowControlKind::Minimize);
        let outcome = click(&mut shell, &mut compositor, at);
        assert!(matches!(
            outcome,
            crate::ShellOutcome::WindowManager(InputResponse::WindowControl {
                control: WindowControlKind::Minimize,
                ..
            })
        ));
        assert_eq!(
            window_control_event(
                WindowControlKind::Minimize,
                wm,
                work_area,
                &mut shell,
                &mut compositor,
                &windows,
            ),
            Some(WindowEvent::Minimized { window_id: 7 })
        );
        assert!(!compositor.window(wm).expect("live").is_visible());
    }

    /// Open one served window and return its window-channel id → compositor id.
    fn open_one(host: &mut ShellWindowHost<'_>, window_id: u64) -> WindowId {
        open_one_sized(host, window_id, false)
    }

    /// Open one served window with an explicit `resizable` request.
    fn open_one_sized(host: &mut ShellWindowHost<'_>, window_id: u64, resizable: bool) -> WindowId {
        host.window_opened(
            window_owner(1),
            window_id,
            &mode(120, 80, DisplayFormat::Rgba8888),
            "app",
            resizable,
        )
        .expect("opens");
        host.windows.records.get(&window_id).expect("live").wm
    }

    /// A resizable, active decoration furniture for a maximizable test window.
    fn resizable_frame() -> tairix_wm::WindowFrame {
        tairix_wm::WindowFrame::new(tairix_wm::WindowFurnitureState {
            activation: tairix_wm::WindowActivationState::Active,
            size: tairix_wm::WindowSizeState::Restored,
            movable: true,
            resizable: true,
        })
    }

    #[test]
    fn close_control_yields_a_close_request_for_the_owning_window() {
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let (wm, stray) = {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                pins: &mut RefusingPins,
            };
            let wm = open_one(&mut host, 7);
            // A compositor window the session does not track (e.g. the taskbar
            // surface): a control press on it routes nowhere.
            let stray = host
                .compositor
                .add_window(Point::new(0, 0), Surface::new(4, 4).expect("surface"));
            (wm, stray)
        };
        let work_area = shell.work_area(&compositor);
        assert_eq!(
            window_control_event(
                WindowControlKind::Close,
                wm,
                work_area,
                &mut shell,
                &mut compositor,
                &windows,
            ),
            Some(WindowEvent::CloseRequested { window_id: 7 })
        );
        // A non-served window yields nothing to route.
        assert_eq!(
            window_control_event(
                WindowControlKind::Close,
                stray,
                work_area,
                &mut shell,
                &mut compositor,
                &windows,
            ),
            None
        );
    }

    #[test]
    fn a_secondary_close_reaches_only_the_owning_app_and_closes_nothing() {
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let wm = {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                pins: &mut RefusingPins,
            };
            open_one(&mut host, 7)
        };
        // A session-owned window (the trusted picker, the greeter) is served
        // to no app, so it has nothing to notify.
        let session_owned = shell
            .open_window(
                &mut compositor,
                Point::new(10, 10),
                Surface::new(40, 20).expect("surface"),
                "Picker",
            )
            .expect("a session window");

        assert_eq!(
            window_control_alternate_event(WindowControlKind::Close, wm, &windows),
            Some(WindowEvent::AlternateCloseRequested { window_id: 7 })
        );
        // The session performs nothing of its own: the window is still open,
        // visible, and where it was.
        assert!(compositor.window(wm).expect("live").is_visible());
        assert!(windows.records.contains_key(&7));
        // Every other control's secondary press means nothing.
        for control in [
            WindowControlKind::Minimize,
            WindowControlKind::PutToBack,
            WindowControlKind::SizeToggle,
        ] {
            assert_eq!(
                window_control_alternate_event(control, wm, &windows),
                None,
                "{control:?} has no alternate meaning"
            );
        }
        // A session-owned window's close control leaks no event.
        assert_eq!(
            window_control_alternate_event(WindowControlKind::Close, session_owned, &windows),
            None
        );
        assert!(compositor.window(session_owned).expect("live").is_visible());
    }

    /// A validated backdrop-blur request frosts exactly the window it
    /// names, and a window the session has no record of frosts nothing.
    #[test]
    fn backdrop_blur_reaches_the_named_window_only() {
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let mut host = ShellWindowHost {
            shell: &mut shell,
            compositor: &mut compositor,
            windows: &mut windows,
            picker: &mut picker,
            pins: &mut RefusingPins,
        };
        let first = open_one(&mut host, 7);
        let second = open_one(&mut host, 8);

        host.backdrop_blur_set(7, 12);
        // No record: the id names no window of this session's, so nothing
        // is frosted rather than guessing which window was meant.
        host.backdrop_blur_set(99, 24);

        assert_eq!(
            host.compositor.window(first).expect("live").blur_radius(),
            12
        );
        assert_eq!(
            host.compositor.window(second).expect("live").blur_radius(),
            0,
            "the sibling window is untouched"
        );
        assert!(host.compositor.has_backdrop_blur());

        host.backdrop_blur_set(7, 0);
        assert_eq!(
            host.compositor.window(first).expect("live").blur_radius(),
            0
        );
        assert!(!host.compositor.has_backdrop_blur());
    }

    #[test]
    fn minimize_control_hides_the_window_and_notifies_the_app() {
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let wm = {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                pins: &mut RefusingPins,
            };
            open_one(&mut host, 7)
        };
        let work_area = shell.work_area(&compositor);
        assert_eq!(
            window_control_event(
                WindowControlKind::Minimize,
                wm,
                work_area,
                &mut shell,
                &mut compositor,
                &windows,
            ),
            Some(WindowEvent::Minimized { window_id: 7 })
        );
        // The window is hidden and its taskbar entry marked minimised.
        assert!(!compositor.window(wm).expect("live").is_visible());
        let task = shell.tasks().task_for(wm).expect("tracked task");
        assert!(shell.session().taskbar().tasks().is_minimised(task));
    }

    #[test]
    fn put_to_back_restacks_without_an_app_event() {
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let (front, _back) = {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                pins: &mut RefusingPins,
            };
            let back = open_one(&mut host, 7);
            let front = open_one(&mut host, 9);
            (front, back)
        };
        // The two cascaded windows overlap; the later one is on top.
        let overlap = Point::new(90, 88);
        assert_eq!(compositor.window_at(overlap), Some(front));
        let work_area = shell.work_area(&compositor);
        assert_eq!(
            window_control_event(
                WindowControlKind::PutToBack,
                front,
                work_area,
                &mut shell,
                &mut compositor,
                &windows,
            ),
            None,
            "put-to-back is window-manager-local: no app-ward event"
        );
        assert_ne!(
            compositor.window_at(overlap),
            Some(front),
            "the window was sent to the back"
        );
    }

    #[test]
    fn size_toggle_maximizes_then_restores_and_reports_the_new_client_size() {
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let wm = {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                pins: &mut RefusingPins,
            };
            open_one(&mut host, 7)
        };
        assert!(compositor.set_window_frame(wm, resizable_frame()));
        let work_area = shell.work_area(&compositor);

        // Maximize: the reported client size matches the compositor's, the
        // window records the maximized state, and it never covers the taskbar
        // (its outer bounds fill the work area, not the whole screen).
        let event = window_control_event(
            WindowControlKind::SizeToggle,
            wm,
            work_area,
            &mut shell,
            &mut compositor,
            &windows,
        );
        let client = compositor.window_client_rect(wm).expect("client");
        assert_eq!(
            event,
            Some(WindowEvent::Resized {
                window_id: 7,
                width_px: client.width,
                height_px: client.height,
            })
        );
        assert_eq!(
            compositor.window(wm).expect("live").size_state(),
            tairix_wm::WindowSizeState::Maximized
        );
        assert_eq!(compositor.window(wm).expect("live").bounds(), work_area);
        assert!(work_area.height < compositor.screen_rect().height);

        // Restore: a second toggle reports the restored client size.
        let event = window_control_event(
            WindowControlKind::SizeToggle,
            wm,
            work_area,
            &mut shell,
            &mut compositor,
            &windows,
        );
        let restored = compositor.window_client_rect(wm).expect("client");
        assert_eq!(
            event,
            Some(WindowEvent::Resized {
                window_id: 7,
                width_px: restored.width,
                height_px: restored.height,
            })
        );
        assert_eq!(
            compositor.window(wm).expect("live").size_state(),
            tairix_wm::WindowSizeState::Restored
        );
    }

    #[test]
    fn window_resized_moves_the_compositor_client_geometry() {
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let mut host = ShellWindowHost {
            shell: &mut shell,
            compositor: &mut compositor,
            windows: &mut windows,
            picker: &mut picker,
            pins: &mut RefusingPins,
        };
        let wm = open_one(&mut host, 7);
        // A resize moves the client geometry the compositor draws and lays
        // furniture out from; the app's pixels follow with its next present.
        host.window_resized(7, &mode(200, 150, DisplayFormat::Rgba8888))
            .expect("resizes");
        let size = host.compositor.window(wm).expect("live").client_size();
        assert_eq!(size, (200, 150));
        // An unknown window is refused.
        assert_eq!(
            host.window_resized(99, &mode(10, 10, DisplayFormat::Rgba8888)),
            Err(Errno::NotFound)
        );
    }

    #[test]
    fn window_retitled_moves_the_chrome_and_the_taskbar_label_together() {
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let wm = {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                pins: &mut RefusingPins,
            };
            let wm = open_one(&mut host, 7);
            host.window_retitled(7, "Files - Documents")
                .expect("the owner retitles");
            // An unknown window is refused and changes nothing.
            assert_eq!(host.window_retitled(99, "ghost"), Err(Errno::NotFound));
            wm
        };
        let title = compositor
            .window(wm)
            .expect("live")
            .frame()
            .expect("decorated")
            .title_bar()
            .title();
        assert_eq!(title, "Files - Documents");
        let labels: alloc::vec::Vec<&str> = shell
            .session()
            .taskbar()
            .tasks()
            .entries()
            .iter()
            .map(|entry| entry.title.as_str())
            .collect();
        assert_eq!(labels, ["Files - Documents"]);
    }

    /// The bridge forwards a validated pick request to the slot and
    /// aborts the window's pick when the window closes.
    #[test]
    fn pick_requests_and_closures_reach_the_picker_slot() {
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let mut host = ShellWindowHost {
            shell: &mut shell,
            compositor: &mut compositor,
            windows: &mut windows,
            picker: &mut picker,
            pins: &mut RefusingPins,
        };
        let m = mode(8, 8, DisplayFormat::Rgba8888);
        host.window_opened(window_owner(1), 1, &m, "w", false)
            .expect("opens");
        host.pick_requested(1).expect("slot accepts");
        host.window_closed(1);
        assert_eq!(picker.begun, alloc::vec![1]);
        assert_eq!(picker.aborted, alloc::vec![1]);
    }

    #[test]
    fn trimming_caches_releases_served_window_content_and_spares_the_focused_one() {
        // The desktop's whole answer to a deepened band, driven end to end:
        // the shell runs the content ladder with the window its own router
        // focuses, the bar it paints itself is untouched, and every window
        // whose pixels went is queued for the redraw request the embedder
        // delivers.
        static PRESSURE: ReportedPressure = ReportedPressure::unknown();
        PRESSURE.report(PressureBand::Normal);
        let (mut shell, mut compositor) = crate::tests::desktop_over(
            TaskbarConfig::bottom_bar(640, 480),
            mode(640, 480, DisplayFormat::Rgba8888),
            &PRESSURE,
        );
        shell.present(&mut compositor);
        let bar = shell.presenter().bar_window().expect("the bar is painted");

        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let (focused, background, hidden) = {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                pins: &mut RefusingPins,
            };
            (
                open_one_sized(&mut host, 1, false),
                open_one_sized(&mut host, 2, false),
                open_one_sized(&mut host, 3, false),
            )
        };
        assert!(compositor.set_visible(hidden, false));
        // The last window opened took focus; move focus to the one under
        // test the way a user does — a pointer press in its client area.
        // The cascade puts the first window's client at (48, 48) and the
        // second at (80, 80), so this point is over the first alone.
        shell.handle(
            InputEvent::PointerMoved {
                to: Point::new(55, 55),
            },
            &mut compositor,
            0,
        );
        shell.handle(
            InputEvent::PointerPressed {
                button: PointerButton::Primary,
            },
            &mut compositor,
            0,
        );
        assert_eq!(shell.router().focused(), Some(focused));
        let _ = compositor.pending_redraws();

        let held = |c: &Compositor, id| c.window(id).expect("window").has_content();

        // Normal: the desktop gives nothing back.
        assert_eq!(shell.trim_caches(&mut compositor), 0);
        assert!(held(&compositor, focused));
        assert!(held(&compositor, background));
        assert!(held(&compositor, hidden));

        // Mild: only what nobody is looking at.
        PRESSURE.report(PressureBand::Mild);
        assert!(shell.trim_caches(&mut compositor) > 0);
        assert!(held(&compositor, focused));
        assert!(held(&compositor, background));
        assert!(!held(&compositor, hidden));
        assert_eq!(compositor.pending_redraws(), alloc::vec![hidden]);

        // Critical: the background window too, never the focused one, and
        // never the bar the session paints itself.
        PRESSURE.report(PressureBand::Critical);
        assert!(shell.trim_caches(&mut compositor) > 0);
        assert!(
            held(&compositor, focused),
            "there would be nothing to show in the focused window's place"
        );
        assert!(!held(&compositor, background));
        assert!(
            held(&compositor, bar),
            "no client would answer a redraw for the session's own bar"
        );
        assert_eq!(compositor.pending_redraws(), alloc::vec![background]);
        PRESSURE.report(PressureBand::Normal);
    }
}
