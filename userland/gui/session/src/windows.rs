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

use tairix_abi::desktop::{DesktopInfo, Motion};
use tairix_abi::driver::display::{DamageRect, DisplayMode};
use tairix_abi::window_ipc::{
    AppBar, AppMenu, HandOverDocument, HandOverOutcome, LayerDepth, MenuRefusal, TerrainPlate,
    WindowEvent, WindowRegion,
};
use tairix_abi::{AppIdentity as AttestedApp, BundleId, Errno, ProcId};
use tairix_controls::{ChainModel, PlatePlacement, WindowSizeState};
use tairix_display::winframe;
use tairix_icon::{ArtworkOutcome, IconKind, IconRequest};
use tairix_log::{EventId, Field, FieldValue};
use tairix_window::{CursorSetName, HandOverDesk, OpenEntry, WallpaperName};

use crate::launch::{
    bundle_of_run_path, resolve_launch, DocumentRelay, Launch, LaunchHost, LaunchTarget,
};
use tairix_taskbar::menu::info_facts;
use tairix_window::WindowSizing;
use tairix_wm::{
    Color, Compositor, Pixel, Point, Rect, Surface, Window, WindowControlKind, WindowId,
};

use crate::apps::{AppBarBridge, BundleIndex};
use crate::layer::{
    apply_participation, clamped_origin, fits_layer_bound, stack_at_depth, terrain_into,
    LayerDecision, LayerState, LayerSurface,
};
use crate::menu::{ChainGeometry, ChainOwner, MenuChain, ModelRefused};
use crate::picker::PickerSlot;
use crate::session::DesktopSession;
use crate::shell::DesktopShell;
use crate::wallpaper::WallpaperService;

/// Event id of the one-shot announcement that a served window's first
/// painted frame reached the display, in the desktop session's reserved
/// range (`DESKTOP_SESSION_RANGE_START`).
///
/// The session is the only component that knows this: an application learns
/// that its present was *accepted*, and the compositor that a frame was
/// *composed*, but only the session sees a composed frame carrying that
/// window reach the display. So "the window is visible" is announced here or
/// nowhere, and anything else asking the question — a user diagnosing an
/// application that launched but showed nothing, the icon-bar QEMU vertical
/// deciding when the screen is worth reading — reads this one record.
pub const WINDOW_SHOWN: EventId = EventId(20_003);

/// The exact message [`WINDOW_SHOWN`] is emitted with. A log consumer
/// matches on this constant rather than on a copy of its text.
pub const WINDOW_SHOWN_MESSAGE: &str = "served window first frame on screen";

/// Event id of the announcement that a served window, already on screen, is
/// on screen wearing the new title its application gave it, in the desktop
/// session's reserved range.
///
/// [`WINDOW_SHOWN`] speaks for a window's first frame only. The title bar is
/// the session's own furniture, so the session alone knows when a retitle has
/// been drawn — and because an application's requests are served in order, a
/// frame carrying the new title also carries every frame the application
/// presented before asking for it.
pub const WINDOW_RETITLED: EventId = EventId(20_016);

/// The exact message [`WINDOW_RETITLED`] is emitted with. A log consumer
/// matches on this constant rather than on a copy of its text.
pub const WINDOW_RETITLED_MESSAGE: &str = "served window title on screen";

/// Event id of the announcement that a served window is on screen at the
/// size state it was just given, in the desktop session's reserved range.
///
/// The window manager applies a size state and the application answers with
/// a frame at the new extent; only the session sees the two meet on the
/// display. The record names the state, the extent, and how the frame reached
/// the display.
pub const WINDOW_SIZED: EventId = EventId(20_018);

/// The exact message [`WINDOW_SIZED`] is emitted with. A log consumer matches
/// on this constant rather than on a copy of its text.
pub const WINDOW_SIZED_MESSAGE: &str = "served window on screen at its new size";

/// The name [`WINDOW_SIZED`] records `state` under.
#[must_use]
pub const fn size_state_name(state: WindowSizeState) -> &'static str {
    match state {
        WindowSizeState::Restored => "restored",
        WindowSizeState::Maximized => "maximized",
        WindowSizeState::Fullscreen => "fullscreen",
    }
}

/// The fields of one [`WINDOW_SIZED`] record, spelled once for the session
/// that writes it and every consumer that reads the line back.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SizedRecord<'a> {
    /// The served window, by its window-channel id.
    pub window: u64,
    /// The size state, as [`size_state_name`] spells it.
    pub state: &'a str,
    /// The client extent the state gave, in pixels.
    pub extent: (u32, u32),
    /// How the frame carrying it reached the display.
    pub path: &'a str,
}

impl<'a> SizedRecord<'a> {
    const WINDOW: &'static str = "window";
    const STATE: &'static str = "state";
    const WIDTH: &'static str = "width";
    const HEIGHT: &'static str = "height";
    const PATH: &'static str = "path";

    /// The record as the fields it is logged with.
    #[must_use]
    pub fn fields(&self) -> [Field<'a>; 5] {
        [
            Field {
                key: Self::WINDOW,
                value: FieldValue::UnsignedInt(self.window),
            },
            Field {
                key: Self::STATE,
                value: FieldValue::Str(self.state),
            },
            Field {
                key: Self::WIDTH,
                value: FieldValue::UnsignedInt(u64::from(self.extent.0)),
            },
            Field {
                key: Self::HEIGHT,
                value: FieldValue::UnsignedInt(u64::from(self.extent.1)),
            },
            Field {
                key: Self::PATH,
                value: FieldValue::Str(self.path),
            },
        ]
    }

    /// Read a record back through `field`, which answers the rendered value
    /// the line carries under a key; `None` if any field is absent or an
    /// integer does not parse.
    pub fn read(field: impl Fn(&str) -> Option<&'a str>) -> Option<Self> {
        Some(Self {
            window: field(Self::WINDOW)?.parse().ok()?,
            state: field(Self::STATE)?,
            extent: (
                field(Self::WIDTH)?.parse().ok()?,
                field(Self::HEIGHT)?.parse().ok()?,
            ),
            path: field(Self::PATH)?,
        })
    }
}

/// Event id of the one-shot announcement that an open menu chain's plates
/// reached the display, in the desktop session's reserved range.
///
/// The sibling of [`WINDOW_SHOWN`], for the surfaces no application owns. A
/// chain's plates are the session's own compositor windows, so nothing on the
/// window channel says a word about them: an application learns that its open
/// was *accepted* and never that a plate was drawn, and the desktop's own menus
/// cross no channel at all. So "the menu is on screen" is announced here or
/// nowhere — which is what lets a user diagnosing a menu that never appeared,
/// or a QEMU vertical deciding when a plate is worth reading and clicking,
/// gate on a fact instead of a delay.
pub const MENU_SHOWN: EventId = EventId(20_006);

/// The exact message [`MENU_SHOWN`] is emitted with. A log consumer matches on
/// this constant rather than on a copy of its text.
pub const MENU_SHOWN_MESSAGE: &str = "menu chain on screen";

/// Event id of the record the session emits when it hands a window's frame
/// region back under memory pressure, in the desktop session's reserved
/// range.
///
/// Every other reclaim decision on the machine is recorded — a cache's
/// evictions and refusals through `lib/reclaim`'s audit sink, the band itself
/// by the kernel — and window content is the largest block the desktop gives
/// back, so leaving it silent would make the one release that matters most the
/// only one nobody can see. It is also the only reclaim the *user* can
/// perceive, since the owning application is asked to re-establish its pixels
/// afterwards.
pub const CONTENT_RELEASED: EventId = EventId(20_005);

/// The exact message [`CONTENT_RELEASED`] is emitted with. A log consumer
/// matches on this constant rather than on a copy of its text.
pub const CONTENT_RELEASED_MESSAGE: &str = "window content released under memory pressure";

/// The freshly opened popup's fill until its app's first present lands: an
/// opaque near-black, so a plate whose content lands a frame later is never
/// stale or transparent pixels.
///
/// A popup is placed relative to its parent's client and shown at once
/// because the gesture that opened it is the user's own; a *top-level* served
/// window has no such fill, because it is not shown until its application has
/// presented something to see.
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

/// How far a served window has got towards being visible, so the
/// [`WINDOW_SHOWN`] announcement is made only for a window whose pixels
/// genuinely reached the screen — once, and once again after a release took
/// them away.
#[derive(Clone, Copy, Eq, PartialEq)]
enum FirstFrame {
    /// Opened, and never presented into: the window is still off screen,
    /// because there is nothing of the application's to show.
    ///
    /// Distinct from [`Awaited`](Self::Awaited), which is a window already on
    /// screen whose pixels memory pressure took back: that one is minimised or
    /// not by the user's own choice, and a present must not un-minimise it.
    Unpresented,
    /// Mapped, but holding no pixels of the application's: a popup before its
    /// first present, or a window whose pixels memory pressure took back.
    Awaited,
    /// A present landed, so the next frame the display takes carries it.
    Painted,
    /// A frame carrying it reached the display, and that was announced.
    Shown,
}

/// One served window's session-side state.
struct WindowRecord {
    /// The compositor window presenting this served window. The window's
    /// content surface lives there and nowhere else: a present converts
    /// its damaged pixels straight into that one owned buffer
    /// (`Compositor::present_window_content`), so the session keeps no
    /// second copy to convert into and clone from.
    wm: WindowId,
    /// For a popup surface, the compositor window of the parent that owns
    /// it; `None` for a top-level window. It is what tells a close which
    /// teardown the surface takes, and the window manager holds the same
    /// link itself for stacking, so no re-assertion happens here.
    parent: Option<WindowId>,
    /// How far this window has got towards being seen.
    first_frame: FirstFrame,
    /// Whether a retitle of this already-shown window has yet to be carried
    /// by a frame that reached the display.
    retitled: bool,
    /// A size state applied to this window and not yet on screen: the state
    /// and the client extent it was given.
    sized: Option<(WindowSizeState, (u32, u32))>,
    /// The extent of the frame the application last presented.
    presented_extent: Option<(u32, u32)>,
    /// The process the kernel attested opened this window (a popup inherits
    /// its parent's). What a menu chain's information row resolves its
    /// attested identity from.
    owner: ProcId,
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
    /// Windows opened since the last drain, each with the kernel-attested
    /// process that opened it, awaiting identification of the application
    /// they belong to.
    opened_owners: Vec<(WindowId, ProcId)>,
    /// Window-channel ids that successfully presented content since the
    /// last frame-report decision. Drained with the report so a frame whose
    /// only content is the Switchboard's own paint is not reported back.
    presented: Vec<u64>,
    /// App-ward events the host produced while answering a request and
    /// the serve loop has yet to deliver.
    ///
    /// A host holds no event sink, and a size-state change still owes its
    /// app the new extent — on the one `Resized` path every other extent
    /// change takes, so the hold-back and the client's folding treat it
    /// identically.
    owed: Vec<WindowEvent>,
    /// The seat's desktop layer surface and its two feeds.
    pub layers: LayerState,
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

    /// Note that the served window `ipc` was given `state` at a client of
    /// `extent`, to be announced once a frame drawn at that extent is on the
    /// display ([`report_on_screen`](Self::report_on_screen)).
    ///
    /// Every path that applies a size state records it here, so a state the
    /// user chose from the title bar is announced exactly as one the
    /// application asked for.
    fn note_sized(&mut self, ipc: u64, state: WindowSizeState, extent: (u32, u32)) {
        if let Some(record) = self.records.get_mut(&ipc) {
            record.sized = Some((state, extent));
        }
    }

    /// Window-channel ids that successfully presented since the last take,
    /// clearing the set so the next report decision starts fresh.
    pub fn take_presented(&mut self) -> Vec<u64> {
        core::mem::take(&mut self.presented)
    }

    /// The app-ward events the host owes since the last take, clearing the
    /// queue so each is delivered once.
    pub fn take_owed_events(&mut self) -> Vec<WindowEvent> {
        core::mem::take(&mut self.owed)
    }

    /// Report what the frame just handed to the display shows of the served
    /// windows: each whose awaited frame it carries, each already on screen
    /// whose new title it carries, and each whose last applied size state it
    /// carries at that state's extent, where `visible` says the window was
    /// composited into it.
    ///
    /// Called immediately after a frame was handed to the display, which is
    /// what makes the claims true: a window the application has presented into
    /// is carried by that frame, so it is on screen now. A window still
    /// awaiting its first present says nothing — its body is the session's own
    /// opening fill, not the application's pixels — and one already announced
    /// is not announced again until [`content_released`](Self::content_released)
    /// makes it awaited afresh. A first frame carries the title the window
    /// wears, so it retires any retitle still pending; a retitle is announced
    /// only for a window whose own pixels are on screen, so the record never
    /// claims a title bar over a released or hidden window, and a burst of
    /// retitles between two frames is one announcement. A size state is
    /// announced only once the application has presented at the extent the
    /// state gave it, so the record never claims a window is fullscreen while
    /// the display still shows the frame it drew before; a burst of changes
    /// between two frames is one announcement, of the last.
    ///
    /// One walk for all three, taking reporters rather than returning
    /// collections, so an ordinary frame allocates nothing; `visible`, which
    /// may search the compositor's windows, is asked only about a window with
    /// an announcement pending.
    pub fn report_on_screen(
        &mut self,
        visible: impl Fn(WindowId) -> bool,
        mut shown: impl FnMut(u64),
        mut retitled: impl FnMut(u64),
        mut sized: impl FnMut(u64, WindowSizeState, (u32, u32)),
    ) {
        for (&ipc, record) in &mut self.records {
            match record.first_frame {
                FirstFrame::Painted => {
                    record.first_frame = FirstFrame::Shown;
                    record.retitled = false;
                    shown(ipc);
                }
                FirstFrame::Shown if record.retitled && visible(record.wm) => {
                    record.retitled = false;
                    retitled(ipc);
                }
                _ => {}
            }
            let Some((state, extent)) = record.sized else {
                continue;
            };
            if record.first_frame == FirstFrame::Shown
                && record.presented_extent == Some(extent)
                && visible(record.wm)
            {
                record.sized = None;
                sized(ipc, state, extent);
            }
        }
    }

    /// Note that window `ipc`'s content was released, so it is awaiting its
    /// pixels again.
    ///
    /// A released window composites transparent — the desktop shows through —
    /// so "a frame carrying this window's own pixels reached the display" has
    /// stopped being true, and the record must stop claiming it. The window is
    /// announced again ([`report_on_screen`](Self::report_on_screen)) when
    /// the application answers the redraw its next showing sends and that
    /// frame lands, which is the only honest moment to say its pixels are
    /// back.
    pub fn content_released(&mut self, ipc: u64) {
        if let Some(record) = self.records.get_mut(&ipc) {
            record.first_frame = FirstFrame::Awaited;
        }
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

    /// Put a drained window back on the identification list because its
    /// application's picture is still being decoded.
    ///
    /// A window is otherwise offered for identification once. One whose
    /// artwork was not ready keeps its place instead, so the pass the landing
    /// decode drives pictures it — rather than the window wearing the shared
    /// application glyph for as long as it stays open.
    pub fn defer_identity(&mut self, wm: WindowId, owner: ProcId) {
        self.opened_owners.push((wm, owner));
    }

    /// Record the freshly opened window `ipc`, shown as `wm` and owned by
    /// `parent` when it is a popup, having got as far as `first_frame`.
    fn insert(
        &mut self,
        ipc: u64,
        wm: WindowId,
        parent: Option<WindowId>,
        owner: ProcId,
        first_frame: FirstFrame,
    ) {
        self.records.insert(
            ipc,
            WindowRecord {
                wm,
                parent,
                first_frame,
                retitled: false,
                sized: None,
                presented_extent: None,
                owner,
            },
        );
        self.by_wm.insert(wm, ipc);
    }

    /// The process the kernel attested owns the served window `ipc`.
    fn owner_of(&self, ipc: u64) -> Option<ProcId> {
        self.records.get(&ipc).map(|record| record.owner)
    }

    /// Forget the window `ipc`, returning its record when it was live.
    fn take(&mut self, ipc: u64) -> Option<WindowRecord> {
        let record = self.records.remove(&ipc)?;
        self.by_wm.remove(&record.wm);
        Some(record)
    }
}

/// The `opened`-th cascade slot (zero-based), in screen pixels: the diagonal
/// cascade from [`CASCADE_ORIGIN`], wrapping so late windows never walk off
/// screen.
///
/// A *slot*, not a placement: [`placed_outer`] is what a window actually
/// opens at, because a window big enough to overhang the work area from its
/// slot is pulled back onto it.
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

/// Where the `opened`-th served window of `outer` size actually opens: its
/// cascade slot, pulled fully onto `work_area`.
///
/// The one placement rule the session applies and a host-side observer (the
/// AW3/AW4 QEMU vertical's click script and screendump assertions) measures
/// against — never a re-derived guess.
///
/// The slot alone is a preference. A window big enough to overhang the work
/// area from it would open with its right and bottom edges off screen or
/// behind the taskbar, where the pointer cannot reach them — the invisible
/// resize edges among them, which is what made resizing look broken on every
/// window after the first. A window larger than the work area in an axis is
/// pinned to that axis's start, so the title bar and the leading edge stay
/// reachable whatever else is not.
#[must_use]
pub fn placed_outer(opened: u64, outer: (u32, u32), work_area: Rect) -> Rect {
    let slot = cascade_origin_for(opened);
    Rect::new(slot.x, slot.y, outer.0, outer.1).clamped_onto(work_area)
}

/// Apply a title-bar window-control command to `wm`'s lifecycle and return
/// the app-ward [`WindowEvent`] the session must deliver to the owning
/// client (or `None` when the command is purely window-manager-local, or
/// `wm` is not a served window, or the command does not apply).
///
/// This is the one place the four [`WindowControlKind`]s map to lifecycle,
/// so the live serve loop and the host tests drive the same rule. It has two
/// halves, and they answer to different owners:
///
/// * The **window-manager-local** half — minimise, put-to-back, size
///   toggle — is performed for *any* decorated window, a session-owned
///   dialog's as much as a served application's. The window manager owns
///   those, so a dialog the session paints itself must answer its own
///   title bar rather than having three of its four controls do nothing.
/// * The **app-ward** half is produced only for a served window, which is
///   the only kind with a client to tell.
///
/// Per control:
///
/// * [`Close`](WindowControlKind::Close) never destroys the window behind
///   the app's back — it returns a [`WindowEvent::CloseRequested`] so the
///   app tears down cooperatively (it decides when, having saved). On a
///   session-owned window it does nothing here: what closing means is the
///   owner's (a cancelled pick, a declined prompt), so the embedder routes
///   it.
/// * [`Minimize`](WindowControlKind::Minimize) hides the window and marks
///   its taskbar entry minimised, and returns a
///   [`WindowEvent::Minimized`] so the app may pause non-essential work.
/// * [`PutToBack`](WindowControlKind::PutToBack) restacks the window to the
///   bottom, with no app-ward event.
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
    windows: &mut SessionWindows,
) -> Option<WindowEvent> {
    let resized = apply_window_control(control, wm, work_area, shell, compositor);
    // Only a served window has a window-channel id and an owning app.
    let window_id = windows.ipc_id(wm)?;
    match control {
        WindowControlKind::Close => Some(WindowEvent::CloseRequested { window_id }),
        WindowControlKind::Minimize => Some(WindowEvent::Minimized { window_id }),
        WindowControlKind::PutToBack => None,
        WindowControlKind::SizeToggle => resized.map(|(state, client)| {
            windows.note_sized(window_id, state, (client.width, client.height));
            WindowEvent::Resized {
                window_id,
                width_px: client.width,
                height_px: client.height,
                state,
            }
        }),
    }
}

/// Perform the window-manager-local half of a title-bar command on the
/// decorated window `wm`, reporting the new size state and client
/// rectangle where the command resized it.
fn apply_window_control(
    control: WindowControlKind,
    wm: WindowId,
    work_area: Rect,
    shell: &mut DesktopShell,
    compositor: &mut Compositor,
) -> Option<(WindowSizeState, Rect)> {
    match control {
        // Closing is never the window manager's to do: a served window's
        // client tears itself down, and a session-owned window's owner
        // decides what its dismissal means.
        WindowControlKind::Close => None,
        WindowControlKind::Minimize => {
            shell.minimize_window(compositor, wm);
            None
        }
        WindowControlKind::PutToBack => {
            compositor.lower(wm);
            None
        }
        WindowControlKind::SizeToggle => compositor.toggle_window_size(wm, work_area),
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

/// Give every window opened since the last pass the title-bar icon of the
/// application that opened it.
///
/// `windows` supplies each freshly opened compositor window paired with the
/// kernel-attested process that opened it, `task_of` resolves that process
/// to the task id the desktop's launch records are keyed by, and `launched`
/// is those records. Nothing an application sent is consulted, so no
/// program can wear another's identity. The icon bar's own slot resolves
/// its picture from the same bundle through the same cache
/// ([`AppBarService::slots`](crate::apps::AppBarService::slots)), so the
/// two surfaces cannot show different applications.
///
/// A window whose owner carries no attested application identity, or whose
/// bundle the installed-store index cannot resolve, is left with no identity,
/// so its title keeps the whole band rather than wearing a badge for an
/// application that cannot be named. An identified bundle whose declared artwork is absent,
/// refused, or undecodable keeps the identity and loses only the picture,
/// falling back to the shared application-bundle artwork and then to the
/// built-in glyph. Resolution never fails a window; it is already open.
///
/// The icon is resolved through the session's one artwork cache at the
/// title band's own pixel side, so a second window of the same application
/// costs a lookup rather than a read and a decode.
pub fn resolve_window_identities<F>(
    shell: &mut DesktopShell,
    compositor: &mut Compositor,
    windows: &mut SessionWindows,
    bundles: &BundleIndex,
    app_of: F,
) where
    F: Fn(ProcId) -> Option<AttestedApp>,
{
    for (wm, owner) in windows.take_opened_owners() {
        let Some(bundle) = app_of(owner).and_then(|app| bundles.path_of(&app)) else {
            continue;
        };
        // An undecorated window draws no identity slot and reports no side,
        // so there is nothing to resolve for it.
        let Some(side) = compositor.window_title_icon_side(wm) else {
            continue;
        };
        let (artwork, pending) = bundle_artwork(shell, bundle, side);
        compositor.set_window_identity(wm, IconKind::AppBundle, artwork);
        // The band stores the picture rather than drawing it from the cache,
        // so a decode still in flight has to be asked for again; the second
        // pass is a cache hit. A window already gone asks for nothing and so
        // is never re-queued.
        if pending {
            windows.defer_identity(wm, owner);
        }
    }
}

/// The picture at `side` pixels for the application installed at the
/// `bundle` directory, resolved through the session's one artwork cache.
///
/// `bundle` is the bundle *directory* of the application the kernel attested
/// owns the window, never a path an application sent, so the artwork layer
/// reads and validates its manifest itself — the same bundle tier a
/// file-manager tile resolves through, not a second reading of it here.
///
/// `side` is the pixel side of the slot that draws it — the window's title
/// band or the bar's task slot — so the artwork is rasterised at exactly the
/// size drawn, and a slot that has already asked for that size is served
/// from the cache. The shipped application-bundle artwork stands in for an
/// application declaring no icon of its own; `None` when neither reads or
/// decodes, leaving the slot on its built-in glyph.
/// The second half of the answer is whether the decode is still in flight, so
/// the caller knows to ask again rather than leaving the slot on its glyph.
fn bundle_artwork(shell: &mut DesktopShell, bundle: &str, side: u32) -> (Option<Surface>, bool) {
    let (cache, resolver) = shell.artwork_parts();
    let request = IconRequest::bundle(IconKind::AppBundle, bundle);
    match cache.owned_artwork(resolver, request, side) {
        ArtworkOutcome::Ready(artwork) => (Some(artwork), false),
        ArtworkOutcome::Refused => (None, false),
        ArtworkOutcome::Pending => (None, true),
    }
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
    /// The session's icon-bar service
    /// ([`AppBarService`](crate::apps::AppBarService) in production): a
    /// validated icon-bar declaration lands here.
    pub apps: &'a mut dyn AppBarBridge,
    /// The seat's one menu chain, which a validated `OpenMenu` brings up.
    pub menu: &'a mut MenuChain,
    /// Whether a surface a menu may not displace holds the seat — the screen
    /// lock, the trusted picker, or a system-modal prompt. An accepted open is
    /// answered `SeatBusy` rather than drawing over one.
    ///
    /// Resolved by the session, which owns all of them; the host is handed the
    /// answer rather than reaching for each in turn.
    pub seat_held: bool,
    /// How a hand-over's document authority reaches the instance that will
    /// show it — the session's own three-syscall relay in production.
    pub relay: &'a mut dyn DocumentRelay,
    /// The desktop's shipped-wallpaper service: the catalog a browsing
    /// application lists, and the previews it asks the session to render
    /// because it holds no authority to read or decode a picture itself.
    pub wallpapers: &'a mut dyn WallpaperService,
    /// The cursor sets this desktop offers, in the order a chooser lists
    /// them — what the settings application's pointer row is built from,
    /// because it holds no authority to read the store either.
    ///
    /// A plain slice rather than a seam: the session listed the store once
    /// at bring-up and holds the answer, so relaying it needs no policy
    /// worth testing apart from the engine's own.
    pub cursor_sets: &'a [CursorSetName],
}

/// The [`LaunchHost`] a hand-over resolves through: the engine's own routes,
/// plus the session's relay and its shell for the raise.
///
/// So a hand-over and the desktop's own launches take the *same* decision
/// ([`resolve_launch`]) rather than each carrying its own ladder.
struct DeskReach<'a, 'b> {
    desk: &'a mut dyn HandOverDesk,
    host: &'a mut ShellWindowHost<'b>,
}

impl LaunchHost for DeskReach<'_, '_> {
    fn queue_open_target(&mut self, app: ProcId, target: LaunchTarget<'_>) -> bool {
        let entry = match target {
            LaunchTarget::Path(path) => OpenEntry::Path(String::from(path)),
            LaunchTarget::Document { name, grant, from } => {
                // The relay is what makes the document the instance's to
                // read: the grant it arrived as was minted to the session.
                // A refused relay delegates nothing, so the launch falls
                // back to a fresh process, which still has the document.
                match self.host.relay.relay(grant, from, app) {
                    Ok(grant) => OpenEntry::Document {
                        name: String::from(name),
                        grant,
                    },
                    Err(_) => return false,
                }
            }
            LaunchTarget::Pane(pane) => OpenEntry::Pane(String::from(pane)),
        };
        self.desk.hand_over(app, entry)
    }

    fn ask_default(&mut self, app: ProcId) -> bool {
        self.desk.ask_default(app)
    }

    fn raise_recent_window(&mut self, app: ProcId) -> bool {
        let Some(wm) = self
            .desk
            .recent_window(app)
            .and_then(|ipc| self.host.windows.wm_id(ipc))
        else {
            return false;
        };
        self.host.shell.raise_window(self.host.compositor, wm)
    }
}

impl ShellWindowHost<'_> {
    /// Why the seat cannot carry a chain right now, if it cannot.
    fn seat_refusal(&self) -> Option<MenuRefusal> {
        seat_menu_refusal(self.compositor.screen_rect(), self.seat_held)
    }
}

/// Why the seat cannot carry a menu chain on a `screen`, with `seat_held`
/// saying whether a surface a menu may not displace has the seat.
///
/// One rule for every chain, whoever asked for it: an application's `OpenMenu`
/// and the desktop's own backdrop menu both resolve through this, so a menu
/// cannot appear over a lock screen or the trusted picker by arriving from the
/// other direction. Drawing over one would take the seat's grab away from a
/// password field or a file choice the user is being asked to make.
#[must_use]
pub fn seat_menu_refusal(screen: Rect, seat_held: bool) -> Option<MenuRefusal> {
    if screen.is_empty() {
        return Some(MenuRefusal::NoDisplay);
    }
    if seat_held {
        return Some(MenuRefusal::SeatBusy);
    }
    None
}

/// Where a window's own menu opens against the press `anchor` its application
/// reported.
///
/// Trailing with no clearance is what makes the press point a *corner* of the
/// plate rather than a point inside it, and edge-adjacency is what a chain
/// needs so travelling from a parent row into its own child crosses no dead
/// space. Named here because two independent readers need the same values:
/// the session, which places the chain, and the QEMU vertical, which
/// reconstructs where a row was drawn in order to click it.
#[must_use]
pub const fn window_menu_placement(anchor: Rect) -> PlatePlacement {
    PlatePlacement::adjacent(anchor)
}

/// The scale, theme and screen every menu-chain geometry answer resolves at.
///
/// Taken from the session and the compositor rather than a copy of its own, so
/// a chain is placed at exactly the density and theme the desktop is drawn at —
/// the *floating* theme, since a plate is desktop chrome over a blurred
/// backdrop like the bar and its popups. One definition, so the pixels a plate
/// is painted with and the row rectangles it is hit-tested against can never
/// come from two themes.
///
/// It takes the session rather than the whole shell so the caller can hold the
/// chain, or the shell's own window records, mutably while it reads these.
#[must_use]
pub fn chain_geometry<'a>(
    session: &'a DesktopSession,
    compositor: &Compositor,
) -> ChainGeometry<'a> {
    ChainGeometry {
        screen: compositor.screen_rect(),
        scale: compositor.scale(),
        theme: session.floating_theme(),
        epoch: compositor.chrome_epoch(),
    }
}

impl tairix_window::WindowHost for ShellWindowHost<'_> {
    fn window_opened(
        &mut self,
        owner: ProcId,
        window_id: u64,
        surface: &DisplayMode,
        title: &str,
        sizing: WindowSizing,
    ) -> Result<(), Errno> {
        let origin = self.windows.next_origin();
        // Opened off screen: the pixels are the application's, so there is
        // nothing to show until it presents. The window is listed on the bar
        // from here on, and `window_presented` maps it.
        let Some(wm) = self.shell.open_unpresented_window(
            self.compositor,
            origin,
            (surface.width_px, surface.height_px),
            title,
        ) else {
            return Err(Errno::LengthOutOfRange);
        };
        // A served application window is decorated by the window manager: the
        // title bar (with the Close / Minimize / PutToBack / SizeToggle
        // controls) and the frame rim are composed around the app's content.
        // The app never draws its own chrome; it reacts to the typed lifecycle
        // events the controls raise over the window path. The app's own
        // sizing choice decides whether a live size toggle and the invisible
        // resize edges — which overlap the client's outer pixels rather than
        // reserving a visible band — are offered.
        self.shell
            .decorate_window(self.compositor, wm, title, sizing.resizable());
        // The range of clients the app said it can lay out bounds what a
        // *user* may drag the window to, so a drag never squeezes the app past
        // the point where it resizes itself back and the two fight, and never
        // grows a window past the size its content stops filling. The window
        // manager still enforces its own furniture floor over the top.
        self.compositor.set_window_client_size_range(
            wm,
            (sizing.min_width_px(), sizing.min_height_px()),
            (sizing.max_width_px(), sizing.max_height_px()),
        );
        // Placed once the decoration band is on and the outer rectangle is
        // therefore known, so the clamp measures the real window rather than
        // re-deriving the frame's insets. A slot that already fits moves
        // nothing and costs no damage.
        if let Some(bounds) = self.compositor.window(wm).map(Window::bounds) {
            let placed = placed_outer(
                self.windows.opened,
                (bounds.width, bounds.height),
                self.shell.work_area(self.compositor),
            );
            self.compositor.move_window(wm, placed.origin);
        }
        self.windows.opened += 1;
        self.windows
            .insert(window_id, wm, None, owner, FirstFrame::Unpresented);
        // Who owns this window is the kernel's answer, kept for the
        // identification pass that runs once this request is served.
        self.windows.opened_owners.push((wm, owner));
        Ok(())
    }

    fn layer_opened(
        &mut self,
        owner: ProcId,
        window_id: u64,
        surface: &DisplayMode,
        x: i32,
        y: i32,
        depth: LayerDepth,
    ) -> Result<(), Errno> {
        // The wire bound is the ceiling at a UI scale of one; this is the
        // same bound in the pixels the desktop is actually drawn in, so a
        // surface cannot grow past it by asking on a dense screen.
        if !fits_layer_bound(
            (surface.width_px, surface.height_px),
            self.compositor.scale(),
        ) {
            self.windows
                .layers
                .note(LayerDecision::Refused(Errno::LengthOutOfRange));
            return Err(Errno::LengthOutOfRange);
        }
        let work_area = self.shell.work_area(self.compositor);
        if work_area.is_empty() {
            self.windows
                .layers
                .note(LayerDecision::Refused(Errno::NotFound));
            return Err(Errno::NotFound);
        }
        let origin = clamped_origin((x, y), (surface.width_px, surface.height_px), work_area);
        // Opened *transparent* rather than unpresented, and off the taskbar.
        // A window opened unpresented stays hidden until something maps it,
        // and the only mapping path is the taskbar's — which a companion is
        // deliberately not on, so it would never have become visible at all.
        // A transparent surface is in the stack from the start, which the
        // stacking and terrain paths want anyway, and its shaped hit test
        // catches nothing until the client has drawn into it.
        let Some(blank) = Surface::filled(surface.width_px, surface.height_px, Pixel::TRANSPARENT)
        else {
            self.windows
                .layers
                .note(LayerDecision::Refused(Errno::OutOfMemory));
            return Err(Errno::OutOfMemory);
        };
        let wm = self.compositor.add_window(origin, blank);
        self.compositor.set_app_presented(wm, true);
        apply_participation(
            self.compositor,
            wm,
            self.shell.presenter().bar_window(),
            depth,
        );
        // A surface opened while a trusted surface is up stays hidden until
        // that surface goes, rather than appearing over it.
        if self.windows.layers.is_suppressed() {
            self.compositor.set_visible(wm, false);
        }
        self.windows.layers.opened(LayerSurface {
            ipc: window_id,
            wm,
            depth,
        });
        self.windows.layers.note(LayerDecision::Opened);
        self.windows
            .insert(window_id, wm, None, owner, FirstFrame::Awaited);
        Ok(())
    }

    fn layer_refused(&mut self, owner: ProcId, reason: Errno) {
        let _ = owner;
        self.windows.layers.note(LayerDecision::Refused(reason));
    }

    fn layer_placed(
        &mut self,
        window_id: u64,
        x: i32,
        y: i32,
        depth: LayerDepth,
    ) -> Result<(), Errno> {
        let Some(wm) = self.windows.wm_id(window_id) else {
            return Err(Errno::NotFound);
        };
        let Some(bounds) = self.compositor.window(wm).map(Window::bounds) else {
            return Err(Errno::NotFound);
        };
        let work_area = self.shell.work_area(self.compositor);
        if work_area.is_empty() {
            return Err(Errno::NotFound);
        }
        let origin = clamped_origin((x, y), (bounds.width, bounds.height), work_area);
        self.compositor.move_window(wm, origin);
        stack_at_depth(
            self.compositor,
            wm,
            self.shell.presenter().bar_window(),
            depth,
        );
        self.windows.layers.placed(depth);
        Ok(())
    }

    fn layer_terrain(&mut self, window_id: u64, out: &mut [TerrainPlate]) -> Result<usize, Errno> {
        let Some(wm) = self.windows.wm_id(window_id) else {
            return Err(Errno::NotFound);
        };
        Ok(terrain_into(self.compositor, wm, out))
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
        let Some(owner) = self.windows.owner_of(parent_window_id) else {
            return Err(Errno::NotFound);
        };
        let Some(client) = self.compositor.window_client_rect(parent) else {
            return Err(Errno::NotFound);
        };
        let Some(content) =
            Surface::filled(surface.width_px, surface.height_px, OPEN_FILL.premultiply())
        else {
            return Err(Errno::OutOfMemory);
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
        // app that opened it is what dismisses it. The window manager stacks
        // it on its parent from here on.
        let Some(wm) =
            self.shell
                .open_popup_window(self.compositor, parent, placed.origin, content)
        else {
            return Err(Errno::NotFound);
        };
        self.compositor.set_app_presented(wm, true);
        self.windows
            .insert(window_id, wm, Some(parent), owner, FirstFrame::Awaited);
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
        // but every index the conversion uses is still checked: a
        // disagreement refuses the present rather than reading out of
        // bounds, and refuses it before writing anything. A window the
        // compositor no longer knows, or one whose pixels cannot be
        // allocated, fails closed.
        //
        // The rows go across the machine's cores, because this is the one
        // whole-window pass the session cannot bound: the app declares the
        // damage, so a client that repaints everything makes the desktop
        // convert everything. Read back rather than installed here, so the
        // conversion and the composite share one answer about how wide the
        // machine is.
        let runner = self.compositor.job_runner();
        let Some(result) = self.compositor.present_window_content(
            wm,
            surface.width_px,
            surface.height_px,
            |content| match winframe::decode(frame, content, surface, damage, runner) {
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
        // The window now holds the app's own pixels, so the next frame the
        // display takes is the one that shows it. Only a window awaiting its
        // pixels moves on; one already on screen stays announced.
        //
        // A window that had never presented is also mapped here, before the
        // frame is composed — so the frame that first carries it is the one
        // that puts it on screen, and the announcement above still follows
        // pixels the display took. A *released* window is only awaiting its
        // pixels again and is left exactly as the user left it, minimised or
        // not: a present must never un-minimise a window that keeps painting.
        let map = self
            .windows
            .records
            .get_mut(&window_id)
            .is_some_and(|record| {
                record.presented_extent = Some((surface.width_px, surface.height_px));
                let unpresented = record.first_frame == FirstFrame::Unpresented;
                if matches!(
                    record.first_frame,
                    FirstFrame::Unpresented | FirstFrame::Awaited
                ) {
                    record.first_frame = FirstFrame::Painted;
                }
                unpresented
            });
        if map {
            self.shell.map_window(self.compositor, wm);
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
        // An interactive drag owns the geometry for its whole duration: it
        // recomputes the outer rectangle from the pointer on every sample, so
        // adopting the size the app re-mapped at would set the window back to
        // wherever the app had got to and the two would fight once per sample.
        // The re-map itself is accepted — that is what sizes the pixels the
        // app is about to present — and the settled size goes out when the
        // grab ends.
        if self.shell.router().wm().resizing() == Some(record.wm) {
            return Ok(());
        }
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
        let Some(record) = self.windows.records.get_mut(&window_id) else {
            return Err(Errno::NotFound);
        };
        if !self.shell.retitle_window(self.compositor, record.wm, title) {
            return Err(Errno::NotFound);
        }
        // A window not yet on screen shows its title with its first frame,
        // which is already announced.
        record.retitled |= record.first_frame == FirstFrame::Shown;
        Ok(())
    }

    fn window_sizing_changed(&mut self, window_id: u64, sizing: WindowSizing) -> Result<(), Errno> {
        // The engine attested the caller, validated that the window is its
        // own, and refused a range naming no reachable size. What is left to
        // check is the one thing only the session can see: whether the
        // sizing agrees with the furniture the window was decorated with.
        // Resizability decides whether there is a grabber and a live size
        // toggle at all, so a window cannot change kind under a range
        // restatement — it fails closed instead of ending up decorated one
        // way and bounded the other.
        let Some(record) = self.windows.records.get(&window_id) else {
            return Err(Errno::NotFound);
        };
        let wm = record.wm;
        // The *declaration*, not the furniture currently drawn: a
        // fullscreen window restating its range means the range it is held
        // to when it returns.
        let decorated_resizable = self
            .compositor
            .window_declared_resizable(wm)
            .unwrap_or(false);
        if decorated_resizable != sizing.resizable() {
            return Err(Errno::NotSupported);
        }
        if self.compositor.set_window_client_size_range(
            wm,
            (sizing.min_width_px(), sizing.min_height_px()),
            (sizing.max_width_px(), sizing.max_height_px()),
        ) {
            Ok(())
        } else {
            Err(Errno::NotFound)
        }
    }

    fn window_size_state_changed(
        &mut self,
        window_id: u64,
        state: WindowSizeState,
    ) -> Result<(), Errno> {
        // The engine attested the caller and validated ownership; the
        // geometry is the window manager's, so it decides and reports what
        // it actually applied. A window it will not move is refused rather
        // than half-applied.
        let Some(record) = self.windows.records.get(&window_id) else {
            return Err(Errno::NotFound);
        };
        let wm = record.wm;
        let work_area = self.shell.work_area(self.compositor);
        let Some((applied, client)) = self.compositor.set_window_size_state(wm, state, work_area)
        else {
            return Err(Errno::NotSupported);
        };
        self.windows
            .note_sized(window_id, applied, (client.width, client.height));
        self.windows.owed.push(WindowEvent::Resized {
            window_id,
            width_px: client.width,
            height_px: client.height,
            state: applied,
        });
        Ok(())
    }

    fn tooltip_declared(
        &mut self,
        window_id: u64,
        region: WindowRegion,
        text: &str,
    ) -> Result<(), Errno> {
        // The window is the caller's own — the engine resolved ownership from
        // the attested caller before this — so the seat holds what it says
        // about it and the dwell decides when to show it. Resolved to the
        // compositor window here, where the map is at hand, so placing the
        // plate later needs only the screen.
        let wm = self.windows.wm_id(window_id).ok_or(Errno::NotFound)?;
        self.shell
            .declare_tooltip(wm, region, text, self.compositor);
        Ok(())
    }

    fn window_closed(&mut self, window_id: u64) {
        // Nothing a dead window declared can still be true.
        if let Some(wm) = self.windows.wm_id(window_id) {
            self.shell.forget_tooltip(wm);
        }
        // A chain is scoped by the window that asked for it, so the window
        // going means the chain goes; its answer is queued for the session's
        // one delivery point.
        self.menu.dismiss_owner(window_id);
        // A window that dies mid-pick takes its picker down with it: the
        // engine already dropped the pending pick with the record, so no
        // conclusion is (or could be) delivered.
        self.picker
            .abort_for(window_id, self.shell, self.compositor);
        // Retiring a layer surface is an ordinary close, so the feeds stop
        // here rather than needing a teardown path of their own.
        let was_layer = self.windows.layers.closed(window_id);
        if let Some(record) = self.windows.take(window_id) {
            // A popup and a layer surface were never tasks, so they leave
            // through the taskbar-less path; the engine tears a parent's
            // popups down with it, so each arrives here in its own turn.
            let _ = if record.parent.is_some() || was_layer {
                self.shell.close_popup_window(self.compositor, record.wm)
            } else {
                self.shell.close_window(self.compositor, record.wm)
            };
        }
    }

    fn menu_open_requested(
        &mut self,
        window_id: u64,
        open_id: u64,
        anchor: WindowRegion,
        menu: &AppMenu,
    ) -> Result<(), Errno> {
        // An application is never told where its window sits, so the anchor it
        // states is window-local and resolving it against the live client
        // origin is the session's job. A window the session cannot place
        // refuses the chain rather than anchoring it somewhere invented.
        let Some(wm) = self.windows.wm_id(window_id) else {
            return Err(Errno::NotFound);
        };
        let Some(client) = self.compositor.window_client_rect(wm) else {
            return Err(Errno::NotFound);
        };
        let anchor = Rect::new(
            client.left().saturating_add(anchor.x()),
            client.top().saturating_add(anchor.y()),
            anchor.width_px(),
            anchor.height_px(),
        );
        // A seat condition is not a bad request: the open is accepted, so the
        // application is owed its one answer, and it gets the reason instead
        // of a chain. Refusing the call would spend no id and leave the
        // application unable to tell "the desktop cannot" from "I asked
        // wrongly".
        let owner = ChainOwner::Window { window_id, open_id };
        if let Some(reason) = self.seat_refusal() {
            self.menu.refuse(owner, reason);
            return Ok(());
        }
        // The information row's text is the session's, from the bundle's
        // signed manifest, so an application cannot state an identity that is
        // not its own — and a process with nothing attesting one gets no
        // information row at all rather than a fabricated panel.
        let facts = self
            .windows
            .owner_of(window_id)
            .and_then(|owner| self.apps.attested_identity(owner))
            .map(|identity| info_facts(&identity));
        let model = ChainModel::from_app_menu(menu.title(), menu, facts.as_ref());
        let geom = chain_geometry(self.shell.session(), self.compositor);
        self.menu
            .open(owner, model, window_menu_placement(anchor), &geom)
            .map_err(|ModelRefused::NoRows| Errno::OutOfRange)
    }

    fn pick_requested(&mut self, window_id: u64) -> Result<(), Errno> {
        // The engine already validated ownership and the per-window
        // single-pending rule; the slot enforces the session's own
        // modality (one picker at a time) and brings the UI up under the
        // session's authority, refusing fail-closed when it cannot.
        self.picker.begin(window_id, self.shell, self.compositor)
    }

    fn hand_over_requested(
        &mut self,
        desk: &mut dyn HandOverDesk,
        caller: ProcId,
        run_path: &str,
        document: Option<&HandOverDocument>,
    ) -> Result<HandOverOutcome, Errno> {
        // The same funnel the desktop's own launches take, so a launcher that
        // is not the desktop cannot get a second instance of a bundle that
        // declares one. The resident slot is where a live instance is found:
        // an application with no icon-bar presence has no application-scoped
        // route, which is the same reason a bare launch cannot ask it for its
        // default action.
        let bundle = bundle_of_run_path(run_path);
        let running = self.apps.resident(bundle);
        let one_instance = self.apps.runs_one_instance(bundle);
        // The grant is redeemed only as the caller's own, so a caller naming
        // a delegation somebody else minted to the session gets nothing.
        let target = document.map(|doc| LaunchTarget::Document {
            name: doc.name.as_str(),
            grant: doc.grant,
            from: caller,
        });
        let mut reach = DeskReach { desk, host: self };
        match resolve_launch(&mut reach, running, one_instance, target) {
            Launch::Reused { .. } => Ok(HandOverOutcome::Reached),
            // Nothing took it, so the caller launches the bundle itself — and
            // with a document that is the only thing that still shows it. The
            // grant it sent was for the session to hand on, so it is consumed.
            Launch::Spawn => {
                if let Some(document) = document {
                    self.relay.decline(document.grant, caller);
                }
                Ok(HandOverOutcome::NotRunning)
            }
        }
    }

    fn app_bar_declared(&mut self, owner: ProcId, bar: &AppBar) -> Result<(), Errno> {
        // The engine attested the caller and bounded the declaration; the
        // icon-bar service records it, and the strip is re-resolved from the
        // dirty latch before the next present.
        self.apps.app_bar_declared(owner, bar)
    }

    fn app_bar_withdrawn(&mut self, owner: ProcId) {
        self.apps.app_bar_withdrawn(owner);
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

    fn wallpaper_catalog(&mut self) -> &[WallpaperName] {
        self.wallpapers.catalog()
    }

    fn cursor_sets(&mut self) -> &[CursorSetName] {
        self.cursor_sets
    }

    fn notify_sources(&mut self, caller: Option<&AttestedApp>) -> Result<&[BundleId], Errno> {
        self.shell.notify_sources(caller)
    }

    fn lock_screen(&mut self, caller: Option<&AttestedApp>) -> Result<(), Errno> {
        self.shell.request_lock(caller)
    }

    fn wallpaper_render_requested(
        &mut self,
        window_id: u64,
        shm_handle: u64,
        index: u16,
        side: u16,
    ) -> Result<(), Errno> {
        // The engine has checked the window and the one-at-a-time rule;
        // resolving the catalog position, mapping the region and getting
        // the decode off this loop are the session's.
        self.wallpapers.render(window_id, shm_handle, index, side)
    }
}

/// The desktop `compositor` composites, as the window channel reports it
/// to an application.
///
/// One definition for both directions: the answer an application's query
/// receives, and the announcement the session pushes when any of it
/// changes. Every fact comes from the compositor — it owns the output it
/// scans out to, that output's density, and the theme the desktop is
/// actually drawn with, accessibility axes and all — so an application
/// reads the very values the desktop draws itself with rather than a copy
/// that could drift.
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
    let theme = compositor.theme();
    DesktopInfo::new(screen.width, screen.height, scale, theme.appearance())?
        .with_axes(
            theme.contrast(),
            theme.density(),
            Motion::from_reduced(theme.motion().reduced_motion()),
        )
        .with_double_click(compositor.double_click())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tairix_abi::driver::display::{DamageRect, DisplayFormat, DisplayMode};
    use tairix_reclaim::{PressureBand, ReportedPressure};
    use tairix_taskbar::TaskbarConfig;
    use tairix_window::WindowHost;
    use tairix_wm::{InputEvent, PointerButton, ResizeEdge};

    use crate::tests::window_owner;

    /// A resizable window declaring no minimum client extent of its own, so
    /// only the window manager's furniture floor bounds a drag.
    const RESIZABLE: WindowSizing = WindowSizing::Resizable {
        min_width_px: 0,
        min_height_px: 0,
        max_width_px: 0,
        max_height_px: 0,
    };

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
            shell.session().active_theme().clone(),
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

    /// A desktop that listed no store: it offers nothing and renders
    /// nothing, which is what a machine with no shipped pictures looks
    /// like.
    struct NoStore;

    impl WallpaperService for NoStore {
        fn catalog(&self) -> &[WallpaperName] {
            &[]
        }

        fn render(&mut self, _w: u64, _s: u64, _i: u16, _side: u16) -> Result<(), Errno> {
            Err(Errno::NotFound)
        }
    }

    /// A wallpaper service recording the bridge's calls: these tests
    /// exercise the window lifecycle, not the gallery (whose own policy is
    /// `crate::wallpaper`'s suite), so it only observes.
    #[derive(Default)]
    struct RecordingGallery {
        catalog: alloc::vec::Vec<WallpaperName>,
        rendered: alloc::vec::Vec<(u64, u16, u16)>,
    }

    impl WallpaperService for RecordingGallery {
        fn catalog(&self) -> &[WallpaperName] {
            &self.catalog
        }

        fn render(
            &mut self,
            window_id: u64,
            _shm_handle: u64,
            index: u16,
            side: u16,
        ) -> Result<(), Errno> {
            self.rendered.push((window_id, index, side));
            Ok(())
        }
    }

    /// A relay that hands nothing on: these tests exercise the window
    /// lifecycle, and the hand-over has its own suite in `crate::tests`.
    struct RefusingRelay;

    impl DocumentRelay for RefusingRelay {
        fn relay(&mut self, _grant: u64, _from: ProcId, _app: ProcId) -> Result<u64, Errno> {
            Err(Errno::NotSupported)
        }

        fn decline(&mut self, _grant: u64, _from: ProcId) {}
    }

    /// An icon-bar seam that records what the bridge relayed: these tests
    /// exercise the window lifecycle, and the icon bar has its own suite in
    /// `crate::tests`.
    #[derive(Default)]
    struct RecordingBar {
        declared: Vec<ProcId>,
        withdrawn: Vec<ProcId>,
        /// The resident instance each bundle resolves to, if a test wires one.
        residents: Vec<(alloc::string::String, ProcId)>,
    }

    impl AppBarBridge for RecordingBar {
        fn app_bar_declared(&mut self, owner: ProcId, _bar: &AppBar) -> Result<(), Errno> {
            self.declared.push(owner);
            Ok(())
        }

        fn attested_identity(&self, _owner: ProcId) -> Option<tairix_taskbar::AppIdentity> {
            None
        }

        fn runs_one_instance(&self, _bundle: &str) -> bool {
            true
        }

        fn resident(&self, bundle: &str) -> Option<ProcId> {
            self.residents
                .iter()
                .find(|(from, _)| from == bundle)
                .map(|(_, owner)| *owner)
        }

        fn app_bar_withdrawn(&mut self, owner: ProcId) {
            self.withdrawn.push(owner);
        }
    }

    /// A desk recording what the host asked the engine to do, and answering
    /// with whatever a test wired.
    #[derive(Default)]
    struct RecordingDesk {
        handed: alloc::vec::Vec<(ProcId, OpenEntry)>,
        defaults: alloc::vec::Vec<ProcId>,
        /// Whether a hand-over is taken.
        takes: bool,
        /// The window each application most recently opened, if a test says.
        recent: alloc::vec::Vec<(ProcId, u64)>,
    }

    impl HandOverDesk for RecordingDesk {
        fn hand_over(&mut self, app: ProcId, entry: OpenEntry) -> bool {
            self.handed.push((app, entry));
            self.takes
        }

        fn ask_default(&mut self, app: ProcId) -> bool {
            self.defaults.push(app);
            false
        }

        fn recent_window(&self, app: ProcId) -> Option<u64> {
            self.recent
                .iter()
                .find(|(held, _)| *held == app)
                .map(|(_, id)| *id)
        }
    }

    /// A relay that hands on whatever a test wired, recording every ask.
    #[derive(Default)]
    struct WiredRelay {
        relayed: alloc::vec::Vec<(u64, ProcId, ProcId)>,
        declined: alloc::vec::Vec<(u64, ProcId)>,
        mints: Option<u64>,
    }

    impl DocumentRelay for WiredRelay {
        fn relay(&mut self, grant: u64, from: ProcId, app: ProcId) -> Result<u64, Errno> {
            self.relayed.push((grant, from, app));
            self.mints.ok_or(Errno::NotSupported)
        }

        fn decline(&mut self, grant: u64, from: ProcId) {
            self.declined.push((grant, from));
        }
    }

    /// A hand-over reaches the resident instance of the bundle it names, with
    /// the document relayed to *that* instance — and nothing is delegated on
    /// any path that does not reach one.
    #[test]
    fn a_hand_over_relays_a_document_to_the_resident_instance_or_delegates_nothing() {
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let mut menu = MenuChain::new();
        let resident = crate::tests::window_owner(1);
        let caller = crate::tests::window_owner(2);
        let bundle = "/System/Applications/view.app";
        let run_path = alloc::format!("{bundle}/Run");
        let document = HandOverDocument {
            name: tairix_abi::window_ipc::DocumentName::new("holiday.png").expect("a valid name"),
            grant: 31,
        };

        let mut reach = |bar: &mut RecordingBar,
                         desk: &mut RecordingDesk,
                         relay: &mut WiredRelay,
                         document: Option<&HandOverDocument>| {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                apps: bar,
                menu: &mut menu,
                seat_held: false,
                relay,
                wallpapers: &mut RecordingGallery::default(),
                cursor_sets: &[],
            };
            host.hand_over_requested(desk, caller, &run_path, document)
        };

        // No resident instance: nothing is relayed and nothing is queued, so
        // the caller launches the bundle itself.
        let mut bar = RecordingBar::default();
        let mut desk = RecordingDesk {
            takes: true,
            ..RecordingDesk::default()
        };
        let mut relay = WiredRelay {
            mints: Some(77),
            ..WiredRelay::default()
        };
        assert_eq!(
            reach(&mut bar, &mut desk, &mut relay, Some(&document)),
            Ok(HandOverOutcome::NotRunning)
        );
        assert!(relay.relayed.is_empty(), "nothing was delegated");
        assert!(desk.handed.is_empty());
        assert_eq!(
            relay.declined,
            [(31, caller)],
            "the grant sent to the session was left pending in its table, or \
             was consumed as somebody else's"
        );
        relay.declined.clear();

        // With one resident, the document is relayed to *it* and queued under
        // the handle the relay minted — never the one the caller sent, which
        // was minted to the session.
        bar.residents
            .push((alloc::string::String::from(bundle), resident));
        assert_eq!(
            reach(&mut bar, &mut desk, &mut relay, Some(&document)),
            Ok(HandOverOutcome::Reached)
        );
        assert_eq!(
            relay.relayed,
            [(31, caller, resident)],
            "the grant is redeemed only as the asking process's own"
        );
        assert!(
            relay.declined.is_empty(),
            "a relayed grant is the instance's"
        );
        assert_eq!(
            desk.handed,
            [(
                resident,
                OpenEntry::Document {
                    name: alloc::string::String::from("holiday.png"),
                    grant: 77,
                }
            )]
        );

        // A relay the kernel refused delegates nothing and queues nothing, so
        // the caller launches instead of the document silently vanishing.
        let mut refusing = WiredRelay::default();
        desk.handed.clear();
        assert_eq!(
            reach(&mut bar, &mut desk, &mut refusing, Some(&document)),
            Ok(HandOverOutcome::NotRunning)
        );
        assert_eq!(refusing.relayed, [(31, caller, resident)]);
        assert!(desk.handed.is_empty(), "a refused relay queues nothing");

        // A bare hand-over asks the instance for its icon-bar default and
        // relays nothing at all; with the default refused and no window to
        // raise, the caller launches.
        relay.relayed.clear();
        assert_eq!(
            reach(&mut bar, &mut desk, &mut relay, None),
            Ok(HandOverOutcome::NotRunning)
        );
        assert_eq!(desk.defaults, [resident]);
        assert!(relay.relayed.is_empty(), "a bare launch names no document");
    }

    /// The one seat rule every chain resolves through, whichever direction it
    /// arrives from — an application's `OpenMenu` or the desktop's own
    /// backdrop press. A chain that took the grab from a password field or a
    /// trusted file choice would be the defect this refusal exists to stop.
    #[test]
    fn no_chain_is_carried_on_a_held_seat_or_a_screen_with_no_extent() {
        let screen = Rect::new(0, 0, 640, 480);
        assert_eq!(seat_menu_refusal(screen, false), None);
        assert_eq!(
            seat_menu_refusal(screen, true),
            Some(MenuRefusal::SeatBusy),
            "a lock screen or the trusted picker holds the seat"
        );
        assert_eq!(
            seat_menu_refusal(Rect::EMPTY, false),
            Some(MenuRefusal::NoDisplay)
        );
        assert_eq!(
            seat_menu_refusal(Rect::EMPTY, true),
            Some(MenuRefusal::NoDisplay),
            "no display is the answer before the seat is even consulted"
        );
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
                apps: &mut RecordingBar::default(),
                menu: &mut MenuChain::new(),
                seat_held: false,
                relay: &mut RefusingRelay,
                wallpapers: &mut RecordingGallery::default(),
                cursor_sets: &[],
            };
            host.window_opened(
                window_owner(1),
                7,
                &mode(64, 48, DisplayFormat::Rgba8888),
                "files",
                WindowSizing::default(),
            )
            .expect("opens");
            host.window_opened(
                window_owner(1),
                9,
                &mode(64, 48, DisplayFormat::Rgba8888),
                "terminal",
                WindowSizing::default(),
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
                    apps: &mut RecordingBar::default(),
                    menu: &mut MenuChain::new(),
                    seat_held: false,
                    relay: &mut RefusingRelay,
                    wallpapers: &mut RecordingGallery::default(),
                    cursor_sets: &[],
                };
                host.window_opened(window_owner(1), 1, &m, "w", WindowSizing::default())
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
            // The first present is what established the buffer, and an
            // established buffer starts transparent, so an undamaged pixel is
            // simply one the client has yet to paint. That is why the whole
            // client area is marked on an established present.
            assert_eq!(
                content.get(0, 0),
                Some(Color::rgba(0, 0, 0, 0).premultiply())
            );
        }
    }

    /// A served window's pixels are its application's, so the session shows
    /// the window when the application first presents into it — not when it
    /// asks for one.
    ///
    /// The reported defect: `view` launched on its own opens a window, then
    /// asks the session's trusted picker for a document. Mapped at create,
    /// it flashed an empty near-black window and left it sitting behind the
    /// chooser for as long as the user took to choose. Opened off screen it
    /// appears with the picture in it. The task is listed throughout, so the
    /// application is reachable while it gets ready.
    #[test]
    fn a_served_window_is_shown_by_its_clients_first_present() {
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let m = mode(64, 48, DisplayFormat::Rgba8888);
        {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                apps: &mut RecordingBar::default(),
                menu: &mut MenuChain::new(),
                seat_held: false,
                relay: &mut RefusingRelay,
                wallpapers: &mut RecordingGallery::default(),
                cursor_sets: &[],
            };
            host.window_opened(window_owner(1), 7, &m, "view", WindowSizing::default())
                .expect("opens");
        }
        let wm = windows.wm_id(7).expect("recorded");
        let window = compositor.window(wm).expect("live");
        assert!(!window.is_visible(), "an unpresented window is off screen");
        assert!(
            !window.has_content(),
            "nothing was allocated for pixels the client has yet to send"
        );
        assert_ne!(
            shell.router().focused(),
            Some(wm),
            "a window nobody can see must not hold the keyboard"
        );
        assert!(
            shell.tasks().task_for(wm).is_some(),
            "the task is listed while its application gets ready"
        );

        {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                apps: &mut RecordingBar::default(),
                menu: &mut MenuChain::new(),
                seat_held: false,
                relay: &mut RefusingRelay,
                wallpapers: &mut RecordingGallery::default(),
                cursor_sets: &[],
            };
            host.window_presented(7, &m, &[0u8; 64 * 48 * 4], whole(&m))
                .expect("presents");
        }
        let window = compositor.window(wm).expect("live");
        assert!(window.is_visible(), "the first present maps the window");
        assert!(window.has_content());
        assert_eq!(shell.router().focused(), Some(wm), "and gives it focus");
    }

    /// A present maps a window that has never been on screen, and only that
    /// one: a window the *user* minimised stays minimised however often its
    /// application keeps painting.
    #[test]
    fn a_present_never_un_minimises_a_window_the_user_minimised() {
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let m = mode(64, 48, DisplayFormat::Rgba8888);
        let wm = {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                apps: &mut RecordingBar::default(),
                menu: &mut MenuChain::new(),
                seat_held: false,
                relay: &mut RefusingRelay,
                wallpapers: &mut RecordingGallery::default(),
                cursor_sets: &[],
            };
            open_one_full(&mut host, 7, 64, 48, WindowSizing::default())
        };
        assert!(compositor.window(wm).expect("live").is_visible());
        assert!(shell.minimize_window(&mut compositor, wm));

        // A clock, a progress bar, a blinking cursor: an application carries
        // on presenting into a minimised window.
        {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                apps: &mut RecordingBar::default(),
                menu: &mut MenuChain::new(),
                seat_held: false,
                relay: &mut RefusingRelay,
                wallpapers: &mut RecordingGallery::default(),
                cursor_sets: &[],
            };
            host.window_presented(7, &m, &[0x40u8; 64 * 48 * 4], whole(&m))
                .expect("presents");
        }
        assert!(
            !compositor.window(wm).expect("live").is_visible(),
            "a present un-minimised a window the user put away"
        );
    }

    /// A served window is announced on screen once its first present has
    /// landed and a frame has been taken — once, and never before.
    #[test]
    fn a_window_is_reported_shown_once_its_first_frame_has_been_taken() {
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let m = mode(4, 4, DisplayFormat::Rgba8888);
        {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                apps: &mut RecordingBar::default(),
                menu: &mut MenuChain::new(),
                seat_held: false,
                relay: &mut RefusingRelay,
                wallpapers: &mut RecordingGallery::default(),
                cursor_sets: &[],
            };
            host.window_opened(window_owner(1), 1, &m, "w", WindowSizing::default())
                .expect("opens");
        }
        // Opened but never presented: the window is off screen, so there is
        // nothing of the application's to announce.
        assert_eq!(shown(&mut windows), Vec::<u64>::new());
        {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                apps: &mut RecordingBar::default(),
                menu: &mut MenuChain::new(),
                seat_held: false,
                relay: &mut RefusingRelay,
                wallpapers: &mut RecordingGallery::default(),
                cursor_sets: &[],
            };
            host.window_presented(1, &m, &[0u8; 4 * 4 * 4], whole(&m))
                .expect("presents");
        }
        assert_eq!(shown(&mut windows), alloc::vec![1]);
        // The announcement is one-shot: a later frame, and a later present,
        // say nothing more about a window already seen.
        assert_eq!(shown(&mut windows), Vec::<u64>::new());
        {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                apps: &mut RecordingBar::default(),
                menu: &mut MenuChain::new(),
                seat_held: false,
                relay: &mut RefusingRelay,
                wallpapers: &mut RecordingGallery::default(),
                cursor_sets: &[],
            };
            host.window_presented(1, &m, &[0u8; 4 * 4 * 4], whole(&m))
                .expect("presents again");
        }
        assert_eq!(shown(&mut windows), Vec::<u64>::new());

        // A release is the one thing that makes it news again: the window
        // composites transparent until its app answers the redraw, so the
        // record must stop claiming its pixels are on screen and start again
        // from the frame that brings them back.
        windows.content_released(1);
        assert_eq!(shown(&mut windows), Vec::<u64>::new(), "not yet re-painted");
        {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                apps: &mut RecordingBar::default(),
                menu: &mut MenuChain::new(),
                seat_held: false,
                relay: &mut RefusingRelay,
                wallpapers: &mut RecordingGallery::default(),
                cursor_sets: &[],
            };
            host.window_presented(1, &m, &[0u8; 4 * 4 * 4], whole(&m))
                .expect("re-attached and presents");
        }
        assert_eq!(shown(&mut windows), alloc::vec![1]);
        assert_eq!(shown(&mut windows), Vec::<u64>::new(), "one-shot again");
        // A window the session never knew is not invented by a release.
        windows.content_released(9999);
        assert_eq!(shown(&mut windows), Vec::<u64>::new());
    }

    /// Each window is announced on its own first frame, so one application
    /// opening a second window announces exactly that window — which is what
    /// makes the announcement a per-window fact rather than a per-app one.
    #[test]
    fn each_window_is_reported_on_its_own_first_frame() {
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let m = mode(4, 4, DisplayFormat::Rgba8888);
        let owner = window_owner(1);
        {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                apps: &mut RecordingBar::default(),
                menu: &mut MenuChain::new(),
                seat_held: false,
                relay: &mut RefusingRelay,
                wallpapers: &mut RecordingGallery::default(),
                cursor_sets: &[],
            };
            host.window_opened(owner, 1, &m, "one", WindowSizing::default())
                .expect("opens");
            host.window_opened(owner, 2, &m, "two", WindowSizing::default())
                .expect("opens");
            host.window_presented(1, &m, &[0u8; 4 * 4 * 4], whole(&m))
                .expect("presents");
        }
        // Only the window that painted; its sibling is still awaited.
        assert_eq!(shown(&mut windows), alloc::vec![1]);
        {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                apps: &mut RecordingBar::default(),
                menu: &mut MenuChain::new(),
                seat_held: false,
                relay: &mut RefusingRelay,
                wallpapers: &mut RecordingGallery::default(),
                cursor_sets: &[],
            };
            host.window_presented(2, &m, &[0u8; 4 * 4 * 4], whole(&m))
                .expect("presents");
        }
        assert_eq!(shown(&mut windows), alloc::vec![2]);
    }

    /// A refused present leaves the window unseen: nothing was drawn, so a
    /// frame taken afterwards carries no pixels of the application's to
    /// announce.
    #[test]
    fn a_refused_present_reports_no_window_shown() {
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let m = mode(4, 4, DisplayFormat::Rgba8888);
        {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                apps: &mut RecordingBar::default(),
                menu: &mut MenuChain::new(),
                seat_held: false,
                relay: &mut RefusingRelay,
                wallpapers: &mut RecordingGallery::default(),
                cursor_sets: &[],
            };
            host.window_opened(window_owner(1), 1, &m, "w", WindowSizing::default())
                .expect("opens");
            // A frame shorter than the mode describes is refused.
            assert!(host.window_presented(1, &m, &[0u8; 4], whole(&m)).is_err());
        }
        assert_eq!(shown(&mut windows), Vec::<u64>::new());
    }

    /// Damage covering the whole of `m`.
    fn whole(m: &DisplayMode) -> DamageRect {
        DamageRect {
            x: 0,
            y: 0,
            width_px: m.width_px,
            height_px: m.height_px,
        }
    }

    /// The windows `windows` reports as newly on screen, in report order.
    fn shown(windows: &mut SessionWindows) -> Vec<u64> {
        on_screen(windows, |_| true).0
    }

    /// A window at a size state, as [`WINDOW_SIZED`] reports it.
    type Sized = (u64, WindowSizeState, (u32, u32));

    /// What `windows` reports the frame just taken carries — the windows newly
    /// on screen, those wearing a new title, and those at a new size state —
    /// with `visible` saying which windows it composited.
    fn on_screen(
        windows: &mut SessionWindows,
        visible: impl Fn(WindowId) -> bool,
    ) -> (Vec<u64>, Vec<u64>, Vec<Sized>) {
        let (mut shown, mut retitled, mut sized) = (Vec::new(), Vec::new(), Vec::new());
        windows.report_on_screen(
            visible,
            |window| shown.push(window),
            |window| retitled.push(window),
            |window, state, extent| sized.push((window, state, extent)),
        );
        (shown, retitled, sized)
    }

    /// Run `act` against a host over `shell`, `compositor` and `windows`.
    fn hosted<R>(
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
        windows: &mut SessionWindows,
        act: impl FnOnce(&mut ShellWindowHost<'_>) -> R,
    ) -> R {
        let mut host = ShellWindowHost {
            shell,
            compositor,
            windows,
            picker: &mut RecordingSlot::default(),
            apps: &mut RecordingBar::default(),
            menu: &mut MenuChain::new(),
            seat_held: false,
            relay: &mut RefusingRelay,
            wallpapers: &mut RecordingGallery::default(),
            cursor_sets: &[],
        };
        act(&mut host)
    }

    /// A size record reads back from the values its fields render as, and a
    /// record missing a field or carrying a malformed extent reads as none.
    #[test]
    fn a_size_record_reads_back_from_its_rendered_fields() {
        use alloc::string::ToString;

        fn read<'a>(line: &'a [(&str, String)]) -> Option<SizedRecord<'a>> {
            SizedRecord::read(|key| {
                line.iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, value)| value.as_str())
            })
        }

        let record = SizedRecord {
            window: 7,
            state: size_state_name(WindowSizeState::Maximized),
            extent: (1022, 685),
            path: "composited",
        };
        let line: Vec<(&str, String)> = record
            .fields()
            .iter()
            .map(|field| (field.key, field.value.to_string()))
            .collect();
        assert_eq!(read(&line), Some(record));
        for missing in 0..line.len() {
            let mut short = line.clone();
            short.remove(missing);
            assert_eq!(read(&short), None, "without {}", line[missing].0);
        }
        for malformed in ["window", "width", "height"] {
            let bad: Vec<(&str, String)> = line
                .iter()
                .map(|(key, value)| {
                    let value = if *key == malformed {
                        "-1"
                    } else {
                        value.as_str()
                    };
                    (*key, value.to_string())
                })
                .collect();
            assert_eq!(read(&bad), None, "a malformed {malformed}");
        }
    }

    /// A size state is announced by the frame that shows the window at the
    /// extent it was given: once, never while the display still shows the
    /// frame drawn at the old size, and for a burst of changes only the state
    /// the burst ends in.
    #[test]
    fn a_size_state_is_announced_once_the_frame_drawn_for_it_is_on_screen() {
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let visible = |_| true;
        let present = |host: &mut ShellWindowHost<'_>, (width, height): (u32, u32)| {
            let m = mode(width, height, DisplayFormat::Rgba8888);
            let frame = alloc::vec![0u8; (width as usize) * (height as usize) * 4];
            host.window_presented(3, &m, &frame, whole(&m))
                .expect("presents");
        };
        hosted(&mut shell, &mut compositor, &mut windows, |host| {
            open_one_sized(host, 3, RESIZABLE)
        });
        assert_eq!(on_screen(&mut windows, visible).2, Vec::<Sized>::new());

        let screen = hosted(&mut shell, &mut compositor, &mut windows, |host| {
            host.window_size_state_changed(3, WindowSizeState::Fullscreen)
                .expect("a resizable window may go fullscreen");
            host.compositor.screen_rect()
        });
        let full = (screen.width, screen.height);
        assert_eq!(
            on_screen(&mut windows, visible).2,
            Vec::<Sized>::new(),
            "the display still shows the frame drawn before the change"
        );
        hosted(&mut shell, &mut compositor, &mut windows, |host| {
            present(host, full);
        });
        assert_eq!(
            on_screen(&mut windows, visible).2,
            [(3, WindowSizeState::Fullscreen, full)]
        );
        assert_eq!(on_screen(&mut windows, visible).2, Vec::<Sized>::new());

        let restored = hosted(&mut shell, &mut compositor, &mut windows, |host| {
            host.window_size_state_changed(3, WindowSizeState::Maximized)
                .expect("maximizes");
            host.window_size_state_changed(3, WindowSizeState::Restored)
                .expect("restores");
            match host.windows.take_owed_events().last() {
                Some(&WindowEvent::Resized {
                    width_px,
                    height_px,
                    ..
                }) => (width_px, height_px),
                other => panic!("no restored extent owed: {other:?}"),
            }
        });
        hosted(&mut shell, &mut compositor, &mut windows, |host| {
            present(host, restored);
        });
        assert_eq!(
            on_screen(&mut windows, visible).2,
            [(3, WindowSizeState::Restored, restored)]
        );
    }

    /// A frame with nothing to announce asks nothing of the compositor:
    /// `visible` searches its windows, so asking it about every shown window
    /// on every frame made each frame quadratic in the windows open.
    #[test]
    fn a_frame_with_nothing_pending_never_asks_which_windows_are_visible() {
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        hosted(&mut shell, &mut compositor, &mut windows, |host| {
            for window in 1..=8 {
                open_one_sized(host, window, RESIZABLE);
            }
        });
        let asked = core::cell::Cell::new(0_u32);
        let visible = |_| {
            asked.set(asked.get() + 1);
            true
        };
        assert_eq!(on_screen(&mut windows, visible).0.len(), 8);
        assert_eq!(on_screen(&mut windows, visible), Default::default());
        assert_eq!(asked.get(), 0, "no window had an announcement pending");

        hosted(&mut shell, &mut compositor, &mut windows, |host| {
            host.window_size_state_changed(5, WindowSizeState::Fullscreen)
                .expect("a resizable window may go fullscreen");
        });
        on_screen(&mut windows, visible);
        assert_eq!(asked.get(), 0, "its frame at the new extent is not in yet");
    }

    /// A size state the user chooses from the title bar is announced exactly
    /// as one the application asked for: once, by the frame drawn at the
    /// extent it gave.
    #[test]
    fn a_title_bar_size_toggle_is_announced_like_a_requested_state() {
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let wm = hosted(&mut shell, &mut compositor, &mut windows, |host| {
            open_one_sized(host, 3, RESIZABLE)
        });
        let work_area = shell.work_area(&compositor);
        let event = window_control_event(
            WindowControlKind::SizeToggle,
            wm,
            work_area,
            &mut shell,
            &mut compositor,
            &mut windows,
        );
        let Some(WindowEvent::Resized {
            width_px,
            height_px,
            state,
            ..
        }) = event
        else {
            panic!("the toggle resized the window: {event:?}");
        };
        assert_eq!(state, WindowSizeState::Maximized);
        let visible = |_| true;
        assert_eq!(on_screen(&mut windows, visible).2, Vec::<Sized>::new());
        hosted(&mut shell, &mut compositor, &mut windows, |host| {
            let m = mode(width_px, height_px, DisplayFormat::Rgba8888);
            let frame = alloc::vec![0u8; (width_px as usize) * (height_px as usize) * 4];
            host.window_presented(3, &m, &frame, whole(&m))
                .expect("presents");
        });
        assert_eq!(
            on_screen(&mut windows, visible).2,
            [(3, WindowSizeState::Maximized, (width_px, height_px))]
        );
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
            apps: &mut RecordingBar::default(),
            menu: &mut MenuChain::new(),
            seat_held: false,
            relay: &mut RefusingRelay,
            wallpapers: &mut RecordingGallery::default(),
            cursor_sets: &[],
        };
        let m = mode(4, 4, DisplayFormat::Rgba8888);
        host.window_opened(window_owner(1), 1, &m, "w", WindowSizing::default())
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
                apps: &mut RecordingBar::default(),
                menu: &mut MenuChain::new(),
                seat_held: false,
                relay: &mut RefusingRelay,
                wallpapers: &mut RecordingGallery::default(),
                cursor_sets: &[],
            };
            host.window_opened(window_owner(1), 1, &m, "w", RESIZABLE)
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
            apps: &mut RecordingBar::default(),
            menu: &mut MenuChain::new(),
            seat_held: false,
            relay: &mut RefusingRelay,
            wallpapers: &mut RecordingGallery::default(),
            cursor_sets: &[],
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
                apps: &mut RecordingBar::default(),
                menu: &mut MenuChain::new(),
                seat_held: false,
                relay: &mut RefusingRelay,
                wallpapers: &mut RecordingGallery::default(),
                cursor_sets: &[],
            };
            host.window_opened(window_owner(1), 1, &m, "w", WindowSizing::default())
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
                apps: &mut RecordingBar::default(),
                menu: &mut MenuChain::new(),
                seat_held: false,
                relay: &mut RefusingRelay,
                wallpapers: &mut RecordingGallery::default(),
                cursor_sets: &[],
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
                apps: &mut RecordingBar::default(),
                menu: &mut MenuChain::new(),
                seat_held: false,
                relay: &mut RefusingRelay,
                wallpapers: &mut RecordingGallery::default(),
                cursor_sets: &[],
            };
            host.window_opened(window_owner(1), 1, &m, "w", WindowSizing::default())
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
                apps: &mut RecordingBar::default(),
                menu: &mut MenuChain::new(),
                seat_held: false,
                relay: &mut RefusingRelay,
                wallpapers: &mut RecordingGallery::default(),
                cursor_sets: &[],
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
        let full = DamageRect {
            x: 0,
            y: 0,
            width_px: 8,
            height_px: 8,
        };
        // One whole frame of a known colour, then one long enough for the
        // first rows but short of the last.
        let landed: Vec<u8> = [0x11u8, 0x22, 0x33, 0xFF].repeat(8 * 8);
        let short = [0xFFu8; 8 * 6 * 4];
        {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                apps: &mut RecordingBar::default(),
                menu: &mut MenuChain::new(),
                seat_held: false,
                relay: &mut RefusingRelay,
                wallpapers: &mut RecordingGallery::default(),
                cursor_sets: &[],
            };
            host.window_opened(window_owner(1), 1, &m, "w", WindowSizing::default())
                .expect("opens");
            host.window_presented(1, &m, &landed, full)
                .expect("the first present lands");
            assert_eq!(
                host.window_presented(1, &m, &short, full),
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
            Some(Color::rgba(0x11, 0x22, 0x33, 0xFF).premultiply()),
            "the refused present overwrote the frame that had landed"
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
            apps: &mut RecordingBar::default(),
            menu: &mut MenuChain::new(),
            seat_held: false,
            relay: &mut RefusingRelay,
            wallpapers: &mut RecordingGallery::default(),
            cursor_sets: &[],
        };
        let m = mode(8, 8, DisplayFormat::Rgba8888);
        host.window_opened(window_owner(1), 1, &m, "w", WindowSizing::default())
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
                apps: &mut RecordingBar::default(),
                menu: &mut MenuChain::new(),
                seat_held: false,
                relay: &mut RefusingRelay,
                wallpapers: &mut RecordingGallery::default(),
                cursor_sets: &[],
            };
            host.window_opened(
                window_owner(1),
                3,
                &mode(120, 80, DisplayFormat::Rgba8888),
                "Files",
                WindowSizing::default(),
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
                apps: &mut RecordingBar::default(),
                menu: &mut MenuChain::new(),
                seat_held: false,
                relay: &mut RefusingRelay,
                wallpapers: &mut RecordingGallery::default(),
                cursor_sets: &[],
            };
            open_one_sized(&mut host, 3, RESIZABLE)
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
                    &mut windows,
                ),
                Some(WindowEvent::Resized { window_id: 3, .. })
            ),
            "the size toggle maximizes a resizable window and reports the new client size"
        );
    }

    #[test]
    fn an_app_asking_for_fullscreen_is_sized_to_the_screen_and_told_so() {
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let mut host = ShellWindowHost {
            shell: &mut shell,
            compositor: &mut compositor,
            windows: &mut windows,
            picker: &mut picker,
            apps: &mut RecordingBar::default(),
            menu: &mut MenuChain::new(),
            seat_held: false,
            relay: &mut RefusingRelay,
            wallpapers: &mut RecordingGallery::default(),
            cursor_sets: &[],
        };
        let wm = open_one_sized(&mut host, 3, RESIZABLE);

        host.window_size_state_changed(3, WindowSizeState::Fullscreen)
            .expect("a resizable window may go fullscreen");

        let screen = host.compositor.screen_rect();
        assert_eq!(host.compositor.window(wm).expect("live").bounds(), screen);
        // Fullscreen covers the taskbar band, so it is bigger than the
        // work area a maximize would have taken.
        assert!(host.shell.work_area(host.compositor).height < screen.height);
        // The app learns its new extent and its new state together, on the
        // one `Resized` path every other extent change takes.
        assert_eq!(
            host.windows.take_owed_events(),
            [WindowEvent::Resized {
                window_id: 3,
                width_px: screen.width,
                height_px: screen.height,
                state: WindowSizeState::Fullscreen,
            }]
        );

        // Asking again for the state already in force changes nothing and
        // owes nothing, rather than reporting a resize that did not happen.
        assert_eq!(
            host.window_size_state_changed(3, WindowSizeState::Fullscreen),
            Err(Errno::NotSupported)
        );
        assert!(host.windows.take_owed_events().is_empty());

        // A window this session does not serve is refused by name.
        assert_eq!(
            host.window_size_state_changed(999, WindowSizeState::Fullscreen),
            Err(Errno::NotFound)
        );
    }

    #[test]
    fn a_fixed_size_app_is_refused_fullscreen_and_does_not_move() {
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let mut host = ShellWindowHost {
            shell: &mut shell,
            compositor: &mut compositor,
            windows: &mut windows,
            picker: &mut picker,
            apps: &mut RecordingBar::default(),
            menu: &mut MenuChain::new(),
            seat_held: false,
            relay: &mut RefusingRelay,
            wallpapers: &mut RecordingGallery::default(),
            cursor_sets: &[],
        };
        let wm = open_one_sized(&mut host, 3, WindowSizing::Fixed);
        let before = host.compositor.window(wm).expect("live").bounds();

        assert_eq!(
            host.window_size_state_changed(3, WindowSizeState::Fullscreen),
            Err(Errno::NotSupported),
            "a window with only the size it was created at has no state to take"
        );
        assert_eq!(host.compositor.window(wm).expect("live").bounds(), before);
        assert!(host.windows.take_owed_events().is_empty());
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One linear end-to-end drive of all four command controls.
    fn clicking_each_title_bar_control_drives_the_window_lifecycle_end_to_end() {
        use tairix_wm::{InputEvent, InputResponse, PointerButton};

        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();

        // Open a served, decorated window exactly as the serve loop does, and
        // land the first present that puts it on screen.
        let wm = {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                apps: &mut RecordingBar::default(),
                menu: &mut MenuChain::new(),
                seat_held: false,
                relay: &mut RefusingRelay,
                wallpapers: &mut RecordingGallery::default(),
                cursor_sets: &[],
            };
            open_one_full(&mut host, 7, 480, 320, WindowSizing::default())
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
                .controls()
                .iter()
                .find(|(k, _)| *k == kind)
                .expect("the control has a slot")
                .1;
            rect.center()
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
                &mut windows,
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
                &mut windows,
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
                &mut windows,
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
                &mut windows,
            ),
            Some(WindowEvent::Minimized { window_id: 7 })
        );
        assert!(!compositor.window(wm).expect("live").is_visible());
    }

    /// Open one served window and return its window-channel id → compositor id.
    fn open_one(host: &mut ShellWindowHost<'_>, window_id: u64) -> WindowId {
        open_one_sized(host, window_id, WindowSizing::default())
    }

    /// Open one served window of an explicit client size and sizing contract,
    /// **and land its first present**, so the window is on screen.
    ///
    /// A served window opens off screen and is mapped by its client's first
    /// present, which every application sends the moment it has anything to
    /// show; a gesture test acts on the window that present put in front of
    /// the pointer.
    fn open_one_full(
        host: &mut ShellWindowHost<'_>,
        window_id: u64,
        width: u32,
        height: u32,
        sizing: WindowSizing,
    ) -> WindowId {
        let m = mode(width, height, DisplayFormat::Rgba8888);
        host.window_opened(window_owner(1), window_id, &m, "app", sizing)
            .expect("opens");
        let frame = alloc::vec![0u8; (width as usize) * (height as usize) * 4];
        host.window_presented(
            window_id,
            &m,
            &frame,
            DamageRect {
                x: 0,
                y: 0,
                width_px: width,
                height_px: height,
            },
        )
        .expect("presents");
        host.windows.records.get(&window_id).expect("live").wm
    }

    /// Open one shown served window with an explicit sizing contract.
    fn open_one_sized(
        host: &mut ShellWindowHost<'_>,
        window_id: u64,
        sizing: WindowSizing,
    ) -> WindowId {
        open_one_full(host, window_id, 120, 80, sizing)
    }

    /// A window big enough to overhang its cascade slot is pulled onto the
    /// work area, so every edge the pointer must reach — the resize edges
    /// among them — is on screen and clear of the taskbar.
    ///
    /// The reported defect: on a 640x480 desktop the second cascaded
    /// terminal opened with its right edge past the screen and its bottom
    /// behind the bar, which left the right edge and both bottom corners
    /// unreachable and made diagonal resizing look broken on every window
    /// after the first.
    #[test]
    fn an_opened_window_is_pulled_fully_onto_the_work_area() {
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let wm = {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                apps: &mut RecordingBar::default(),
                menu: &mut MenuChain::new(),
                seat_held: false,
                relay: &mut RefusingRelay,
                wallpapers: &mut RecordingGallery::default(),
                cursor_sets: &[],
            };
            // A client wide and tall enough that even the first cascade slot
            // overhangs the 640x480 work area.
            open_one_full(&mut host, 7, 600, 400, RESIZABLE)
        };
        let work_area = shell.work_area(&compositor);
        let bounds = compositor.window(wm).expect("live").bounds();
        assert_eq!(
            bounds.clamped_onto(work_area),
            bounds,
            "an opened window sits wholly inside the work area"
        );
        // Every band the frame offers starts its own resize-grab *and drags*,
        // which is what the placement is for: the corners resize both axes at
        // once and are the furthest thing on the frame from the title bar. The
        // points are the bands' inward half, because a window flush against
        // the work area has its outward half over the taskbar or off the
        // screen, where nothing can reach it. Asserting the drag and not only
        // the latch is deliberate: a grab that arms and then refuses every
        // motion is what shipped, and a test that stopped at the latch could
        // not see it.
        for (at, edge, delta) in [
            (
                Point::new(bounds.right() - 1, bounds.bottom() - 1),
                ResizeEdge::BottomRight,
                Point::new(-24, -18),
            ),
            (
                Point::new(bounds.left(), bounds.bottom() - 1),
                ResizeEdge::BottomLeft,
                Point::new(24, -18),
            ),
            (
                Point::new(bounds.right() - 1, bounds.bottom() - 32),
                ResizeEdge::Right,
                Point::new(-24, 0),
            ),
            (
                Point::new(bounds.left() + 96, bounds.bottom() - 1),
                ResizeEdge::Bottom,
                Point::new(0, -18),
            ),
        ] {
            let before = compositor.window(wm).expect("live").bounds();
            shell.handle(InputEvent::PointerMoved { to: at }, &mut compositor, 0);
            shell.handle(
                InputEvent::PointerPressed {
                    button: PointerButton::Primary,
                },
                &mut compositor,
                0,
            );
            assert_eq!(
                shell.router().wm().resizing_edge(),
                Some(edge),
                "a press at {at:?} grabs the {edge:?} band"
            );
            shell.handle(
                InputEvent::PointerMoved {
                    to: Point::new(at.x + delta.x, at.y + delta.y),
                },
                &mut compositor,
                0,
            );
            assert_ne!(
                compositor.window(wm).expect("live").bounds(),
                before,
                "and dragging the {edge:?} band moves that edge"
            );
            shell.handle(
                InputEvent::PointerReleased {
                    button: PointerButton::Primary,
                },
                &mut compositor,
                0,
            );
            // Each band is aimed at the rectangle the window *opened* at, so
            // the drag just made is undone before the next one is aimed.
            assert!(compositor.resize_window(wm, bounds));
        }
    }

    /// [`placed_outer`] pins an axis the window is too big for, so a window
    /// larger than the work area shows its top and leading edge — the title
    /// bar and the side the pointer reaches it by — rather than its middle.
    #[test]
    fn placed_outer_pins_an_axis_the_window_cannot_fit() {
        let work_area = Rect::new(0, 0, 640, 435);
        // Fits at its slot: left exactly there.
        assert_eq!(
            placed_outer(0, (200, 120), work_area),
            Rect::new(cascade_origin_for(0).x, cascade_origin_for(0).y, 200, 120)
        );
        // Overhangs: pulled back so the far edges land on the work area's.
        assert_eq!(
            placed_outer(1, (562, 380), work_area),
            Rect::new(640 - 562, 435 - 380, 562, 380)
        );
        // Bigger than the work area in both axes: pinned to its start.
        assert_eq!(
            placed_outer(3, (900, 700), work_area),
            Rect::new(0, 0, 900, 700)
        );
    }

    /// The clamp is a bound, not a placement rule: a window that fits its
    /// cascade slot opens exactly there, so successive opens still step.
    #[test]
    fn a_window_that_fits_its_cascade_slot_opens_there() {
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let (first, second) = {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                apps: &mut RecordingBar::default(),
                menu: &mut MenuChain::new(),
                seat_held: false,
                relay: &mut RefusingRelay,
                wallpapers: &mut RecordingGallery::default(),
                cursor_sets: &[],
            };
            (
                open_one_full(&mut host, 7, 200, 120, RESIZABLE),
                open_one_full(&mut host, 9, 200, 120, RESIZABLE),
            )
        };
        assert_eq!(
            compositor.window(first).expect("live").bounds().origin,
            cascade_origin_for(0)
        );
        assert_eq!(
            compositor.window(second).expect("live").bounds().origin,
            cascade_origin_for(1),
            "the cascade still steps for a window that fits"
        );
    }

    #[test]
    fn window_opened_gives_the_window_manager_the_declared_range() {
        // What the app said it needs, honoured by whoever drags the window —
        // never by the app clamping a size it was granted and resizing back,
        // which fights the drag once per pointer sample.
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let (declared, bare) = {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                apps: &mut RecordingBar::default(),
                menu: &mut MenuChain::new(),
                seat_held: false,
                relay: &mut RefusingRelay,
                wallpapers: &mut RecordingGallery::default(),
                cursor_sets: &[],
            };
            (
                open_one_sized(
                    &mut host,
                    3,
                    WindowSizing::Resizable {
                        min_width_px: 900,
                        min_height_px: 700,
                        max_width_px: 1200,
                        max_height_px: 1000,
                    },
                ),
                open_one_sized(&mut host, 4, RESIZABLE),
            )
        };
        let bounds = compositor
            .window_resize_bounds(declared)
            .expect("decorated");
        assert!(
            bounds.min_width > 900 && bounds.min_height > 700,
            "the floor holds the declared client and the furniture around it, not {bounds:?}"
        );
        assert!(
            bounds.max_width.is_some_and(|width| width > 1200)
                && bounds.max_height.is_some_and(|height| height > 1000),
            "and the ceiling holds the declared client and the same furniture, not {bounds:?}"
        );
        let bare = compositor.window_resize_bounds(bare).expect("decorated");
        assert!(
            bare.min_width < bounds.min_width && bare.min_height < bounds.min_height,
            "a window declaring no minimum of its own is bounded by the furniture alone"
        );
        assert_eq!(
            (bare.max_width, bare.max_height),
            (None, None),
            "and one declaring no maximum is bounded above by nothing"
        );
    }

    #[test]
    fn a_restated_range_replaces_the_declared_one_but_never_the_window_s_kind() {
        // An app whose content constraints move restates its range on the
        // window it already has: without this the window manager goes on
        // enforcing the range of content the app has stopped showing — the
        // defect a board switching to a larger board used to hit. What it
        // may not restate is whether the window is resizable at all, which
        // is what it was decorated for.
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let mut host = ShellWindowHost {
            shell: &mut shell,
            compositor: &mut compositor,
            windows: &mut windows,
            picker: &mut picker,
            apps: &mut RecordingBar::default(),
            menu: &mut MenuChain::new(),
            seat_held: false,
            relay: &mut RefusingRelay,
            wallpapers: &mut RecordingGallery::default(),
            cursor_sets: &[],
        };
        let wm = open_one_sized(&mut host, 3, RESIZABLE);
        assert_eq!(
            host.window_sizing_changed(
                3,
                WindowSizing::Resizable {
                    min_width_px: 300,
                    min_height_px: 200,
                    max_width_px: 800,
                    max_height_px: 600,
                },
            ),
            Ok(())
        );
        // A fixed sizing on a window decorated resizable would leave it with
        // a grabber and no range to drag within, so it is refused whole.
        assert_eq!(
            host.window_sizing_changed(3, WindowSizing::Fixed),
            Err(Errno::NotSupported)
        );
        // And a window the session does not know is not one to bound.
        assert_eq!(
            host.window_sizing_changed(99, RESIZABLE),
            Err(Errno::NotFound)
        );

        let bounds = compositor.window_resize_bounds(wm).expect("decorated");
        assert!(bounds.min_width > 300 && bounds.min_height > 200);
        assert!(
            bounds.max_width.is_some_and(|width| width > 800)
                && bounds.max_height.is_some_and(|height| height > 600),
            "the restated range is the one in force, not the one the create declared"
        );
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
                apps: &mut RecordingBar::default(),
                menu: &mut MenuChain::new(),
                seat_held: false,
                relay: &mut RefusingRelay,
                wallpapers: &mut RecordingGallery::default(),
                cursor_sets: &[],
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
                &mut windows,
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
                &mut windows,
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
                apps: &mut RecordingBar::default(),
                menu: &mut MenuChain::new(),
                seat_held: false,
                relay: &mut RefusingRelay,
                wallpapers: &mut RecordingGallery::default(),
                cursor_sets: &[],
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
            apps: &mut RecordingBar::default(),
            menu: &mut MenuChain::new(),
            seat_held: false,
            relay: &mut RefusingRelay,
            wallpapers: &mut RecordingGallery::default(),
            cursor_sets: &[],
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
        assert_eq!(
            host.compositor.window(second).expect("live").blur_radius(),
            0,
            "no app window frosts once the request is withdrawn"
        );

        // The compositor still reports a frost, and the desktop's own bar is
        // why: it is floating chrome and asks for one for as long as it is on
        // screen, so the flag can no longer stand in for "no app frosts".
        let bar = shell
            .presenter()
            .bar_window()
            .expect("the desktop paints its bar");
        assert!(compositor.window(bar).expect("live").blur_radius() > 0);
        assert!(compositor.has_backdrop_blur());
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
                apps: &mut RecordingBar::default(),
                menu: &mut MenuChain::new(),
                seat_held: false,
                relay: &mut RefusingRelay,
                wallpapers: &mut RecordingGallery::default(),
                cursor_sets: &[],
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
                &mut windows,
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
                apps: &mut RecordingBar::default(),
                menu: &mut MenuChain::new(),
                seat_held: false,
                relay: &mut RefusingRelay,
                wallpapers: &mut RecordingGallery::default(),
                cursor_sets: &[],
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
                &mut windows,
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
                apps: &mut RecordingBar::default(),
                menu: &mut MenuChain::new(),
                seat_held: false,
                relay: &mut RefusingRelay,
                wallpapers: &mut RecordingGallery::default(),
                cursor_sets: &[],
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
            &mut windows,
        );
        let client = compositor.window_client_rect(wm).expect("client");
        assert_eq!(
            event,
            Some(WindowEvent::Resized {
                window_id: 7,
                width_px: client.width,
                height_px: client.height,
                state: tairix_wm::WindowSizeState::Maximized,
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
            &mut windows,
        );
        let restored = compositor.window_client_rect(wm).expect("client");
        assert_eq!(
            event,
            Some(WindowEvent::Resized {
                window_id: 7,
                width_px: restored.width,
                height_px: restored.height,
                state: tairix_wm::WindowSizeState::Restored,
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
            apps: &mut RecordingBar::default(),
            menu: &mut MenuChain::new(),
            seat_held: false,
            relay: &mut RefusingRelay,
            wallpapers: &mut RecordingGallery::default(),
            cursor_sets: &[],
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

    /// A live resize-grab owns the window's geometry, so the size the app
    /// re-mapped at is accepted without moving it: the drag recomputes the
    /// outer rectangle from the pointer every sample, and adopting the app's
    /// size instead would set the window back to wherever the app had got to
    /// and the two would fight once per sample.
    #[test]
    fn a_live_resize_grab_keeps_the_geometry_the_drag_set() {
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let wm = {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                apps: &mut RecordingBar::default(),
                menu: &mut MenuChain::new(),
                seat_held: false,
                relay: &mut RefusingRelay,
                wallpapers: &mut RecordingGallery::default(),
                cursor_sets: &[],
            };
            open_one_full(&mut host, 7, 200, 120, RESIZABLE)
        };
        // Grab the bottom-right corner and drag it out.
        let bounds = compositor.window(wm).expect("live").bounds();
        let corner = Point::new(bounds.right() - 1, bounds.bottom() - 1);
        shell.handle(InputEvent::PointerMoved { to: corner }, &mut compositor, 0);
        shell.handle(
            InputEvent::PointerPressed {
                button: PointerButton::Primary,
            },
            &mut compositor,
            0,
        );
        shell.handle(
            InputEvent::PointerMoved {
                to: Point::new(corner.x + 40, corner.y + 30),
            },
            &mut compositor,
            0,
        );
        let dragged = compositor.window(wm).expect("live").client_size();
        assert_eq!(dragged, (240, 150), "the drag set the client extent");

        // The app catches up a sample late: its re-map is accepted, and the
        // geometry stays the drag's.
        {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                apps: &mut RecordingBar::default(),
                menu: &mut MenuChain::new(),
                seat_held: false,
                relay: &mut RefusingRelay,
                wallpapers: &mut RecordingGallery::default(),
                cursor_sets: &[],
            };
            host.window_resized(7, &mode(220, 135, DisplayFormat::Rgba8888))
                .expect("accepted");
        }
        assert_eq!(
            compositor.window(wm).expect("live").client_size(),
            dragged,
            "a stale re-map never pulls the window back"
        );

        // Once the grab ends the app's own size moves the window again.
        shell.handle(
            InputEvent::PointerReleased {
                button: PointerButton::Primary,
            },
            &mut compositor,
            0,
        );
        {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                apps: &mut RecordingBar::default(),
                menu: &mut MenuChain::new(),
                seat_held: false,
                relay: &mut RefusingRelay,
                wallpapers: &mut RecordingGallery::default(),
                cursor_sets: &[],
            };
            host.window_resized(7, &mode(240, 150, DisplayFormat::Rgba8888))
                .expect("resizes");
        }
        assert_eq!(
            compositor.window(wm).expect("live").client_size(),
            (240, 150)
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
                apps: &mut RecordingBar::default(),
                menu: &mut MenuChain::new(),
                seat_held: false,
                relay: &mut RefusingRelay,
                wallpapers: &mut RecordingGallery::default(),
                cursor_sets: &[],
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

    /// What the frame just taken carries, judged against what the compositor
    /// actually shows.
    fn composited(windows: &mut SessionWindows, compositor: &Compositor) -> (Vec<u64>, Vec<u64>) {
        let (shown, retitled, _) = on_screen(windows, |wm| {
            compositor
                .window(wm)
                .is_some_and(tairix_wm::Window::is_visible)
        });
        (shown, retitled)
    }

    /// A retitle of a window already on screen is announced by the frame that
    /// carries it — once however many retitles it folds, and only while the
    /// window is composited — while a retitle before a first frame, or before
    /// the frame that shows the window afresh, is covered by that frame's own
    /// witness.
    #[test]
    fn a_retitle_is_announced_once_by_the_frame_that_shows_it() {
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let m = mode(4, 4, DisplayFormat::Rgba8888);
        let mut host = ShellWindowHost {
            shell: &mut shell,
            compositor: &mut compositor,
            windows: &mut windows,
            picker: &mut picker,
            apps: &mut RecordingBar::default(),
            menu: &mut MenuChain::new(),
            seat_held: false,
            relay: &mut RefusingRelay,
            wallpapers: &mut RecordingGallery::default(),
            cursor_sets: &[],
        };
        host.window_opened(window_owner(1), 1, &m, "opened", WindowSizing::default())
            .expect("opens");
        host.window_retitled(1, "before").expect("retitles");
        host.window_presented(1, &m, &[0u8; 4 * 4 * 4], whole(&m))
            .expect("presents");
        let wm = host.windows.records.get(&1).expect("live").wm;
        assert_eq!(
            composited(host.windows, host.compositor),
            (alloc::vec![1], Vec::new()),
            "the first frame speaks for the title it opened with"
        );

        host.window_retitled(1, "after").expect("retitles");
        host.window_retitled(1, "again").expect("retitles");
        assert!(host.compositor.set_visible(wm, false));
        assert_eq!(
            composited(host.windows, host.compositor),
            (Vec::new(), Vec::new()),
            "a hidden window's title bar is not on screen"
        );
        assert!(host.compositor.set_visible(wm, true));
        assert_eq!(
            composited(host.windows, host.compositor),
            (Vec::new(), alloc::vec![1])
        );
        assert_eq!(
            composited(host.windows, host.compositor),
            (Vec::new(), Vec::new()),
            "one announcement per retitle carried"
        );

        host.window_retitled(1, "released").expect("retitles");
        host.windows.content_released(1);
        host.window_presented(1, &m, &[0u8; 4 * 4 * 4], whole(&m))
            .expect("presents");
        assert_eq!(
            composited(host.windows, host.compositor),
            (alloc::vec![1], Vec::new()),
            "a frame showing the window afresh carries its title too"
        );
        assert_eq!(
            composited(host.windows, host.compositor),
            (Vec::new(), Vec::new())
        );
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
            apps: &mut RecordingBar::default(),
            menu: &mut MenuChain::new(),
            seat_held: false,
            relay: &mut RefusingRelay,
            wallpapers: &mut RecordingGallery::default(),
            cursor_sets: &[],
        };
        let m = mode(8, 8, DisplayFormat::Rgba8888);
        host.window_opened(window_owner(1), 1, &m, "w", WindowSizing::default())
            .expect("opens");
        host.pick_requested(1).expect("slot accepts");
        host.window_closed(1);
        assert_eq!(picker.begun, alloc::vec![1]);
        assert_eq!(picker.aborted, alloc::vec![1]);
    }

    /// The bridge answers the catalog from the host's own listing and
    /// relays a render request to it; the engine has already checked the
    /// window and the one-at-a-time rule.
    #[test]
    fn the_wallpaper_catalog_and_a_render_reach_the_gallery() {
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let mut gallery = RecordingGallery {
            catalog: alloc::vec![WallpaperName {
                category: String::from("TAIRiX"),
                file: String::from("a.jpg"),
            }],
            rendered: alloc::vec::Vec::new(),
        };
        let mut host = ShellWindowHost {
            shell: &mut shell,
            compositor: &mut compositor,
            windows: &mut windows,
            picker: &mut picker,
            apps: &mut RecordingBar::default(),
            menu: &mut MenuChain::new(),
            seat_held: false,
            relay: &mut RefusingRelay,
            wallpapers: &mut gallery,
            cursor_sets: &[],
        };
        assert_eq!(host.wallpaper_catalog().len(), 1);
        assert_eq!(host.wallpaper_catalog()[0].file, "a.jpg");
        host.wallpaper_render_requested(7, 0x99, 0, 64)
            .expect("the gallery accepts");
        assert_eq!(gallery.rendered, alloc::vec![(7, 0, 64)]);
    }

    /// A host that lists no store offers nothing and renders nothing,
    /// rather than refusing the query: a desktop with no shipped pictures
    /// is an honest answer, not an error.
    #[test]
    fn a_desktop_with_no_store_offers_an_empty_catalog() {
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let mut host = ShellWindowHost {
            shell: &mut shell,
            compositor: &mut compositor,
            windows: &mut windows,
            picker: &mut picker,
            apps: &mut RecordingBar::default(),
            menu: &mut MenuChain::new(),
            seat_held: false,
            relay: &mut RefusingRelay,
            wallpapers: &mut NoStore,
            cursor_sets: &[],
        };
        assert!(host.wallpaper_catalog().is_empty());
        assert_eq!(
            host.wallpaper_render_requested(7, 0x99, 0, 64),
            Err(Errno::NotFound)
        );
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
                apps: &mut RecordingBar::default(),
                menu: &mut MenuChain::new(),
                seat_held: false,
                relay: &mut RefusingRelay,
                wallpapers: &mut RecordingGallery::default(),
                cursor_sets: &[],
            };
            (
                open_one_sized(&mut host, 1, WindowSizing::default()),
                open_one_sized(&mut host, 2, WindowSizing::default()),
                open_one_sized(&mut host, 3, WindowSizing::default()),
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

        // Mild: only what nobody is looking at — and it is *not* asked to
        // present again. Asking would have the client establish the buffer
        // the release just freed, for pixels nobody can see: the release
        // would free nothing and cost a repaint per hidden window, under
        // pressure. What it gets instead is the news that it may let go of
        // its own two copies as well.
        PRESSURE.report(PressureBand::Mild);
        assert!(shell.trim_caches(&mut compositor) > 0);
        assert!(held(&compositor, focused));
        assert!(held(&compositor, background));
        assert!(!held(&compositor, hidden));
        assert_eq!(
            compositor.pending_redraws(),
            alloc::vec![],
            "a window nobody can see is not asked to present"
        );
        assert_eq!(compositor.take_released_notices(), alloc::vec![hidden]);

        // Showing it again is what asks: the window has nothing to draw, so
        // the request goes out at the moment it can be seen.
        assert!(compositor.set_visible(hidden, true));
        assert_eq!(compositor.pending_redraws(), alloc::vec![hidden]);
        assert!(compositor.set_visible(hidden, false));

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
        assert_eq!(
            compositor.pending_redraws(),
            alloc::vec![background],
            "a visible window must not be left blank"
        );
        assert_eq!(
            compositor.take_released_notices(),
            alloc::vec![],
            "a window that was asked to present is not told to let go"
        );
        PRESSURE.report(PressureBand::Normal);
    }

    /// A window that has never presented holds no pixels, so memory pressure
    /// takes nothing from it and it is *not* told its content was released.
    ///
    /// That matters beyond the arithmetic: a release is what puts a window
    /// back to "awaiting its pixels", and an awaited window's next present
    /// deliberately does not map it. A contentless window reported as
    /// released would therefore stop being mappable and never appear at all,
    /// so the release path's own "released nothing, say nothing" rule is what
    /// keeps the first present in charge of showing it.
    #[test]
    fn an_unpresented_window_is_never_reported_released_and_still_maps() {
        static PRESSURE: ReportedPressure = ReportedPressure::unknown();
        PRESSURE.report(PressureBand::Normal);
        let (mut shell, mut compositor) = crate::tests::desktop_over(
            TaskbarConfig::bottom_bar(640, 480),
            mode(640, 480, DisplayFormat::Rgba8888),
            &PRESSURE,
        );
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let m = mode(64, 48, DisplayFormat::Rgba8888);
        {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                apps: &mut RecordingBar::default(),
                menu: &mut MenuChain::new(),
                seat_held: false,
                relay: &mut RefusingRelay,
                wallpapers: &mut RecordingGallery::default(),
                cursor_sets: &[],
            };
            host.window_opened(window_owner(1), 7, &m, "view", WindowSizing::default())
                .expect("opens");
        }
        let wm = windows.wm_id(7).expect("recorded");

        PRESSURE.report(PressureBand::Critical);
        assert_eq!(
            shell.trim_caches(&mut compositor),
            0,
            "a window holding no pixels had some taken from it"
        );
        assert_eq!(
            compositor.take_released_notices(),
            alloc::vec![],
            "a window with nothing to release was told it had released"
        );
        PRESSURE.report(PressureBand::Normal);

        {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                apps: &mut RecordingBar::default(),
                menu: &mut MenuChain::new(),
                seat_held: false,
                relay: &mut RefusingRelay,
                wallpapers: &mut RecordingGallery::default(),
                cursor_sets: &[],
            };
            host.window_presented(7, &m, &[0u8; 64 * 48 * 4], whole(&m))
                .expect("presents");
        }
        assert!(
            compositor.window(wm).expect("live").is_visible(),
            "the first present no longer maps the window"
        );
    }

    #[test]
    fn minimising_on_an_already_tight_machine_releases_the_window_there_and_then() {
        // The band's wake is edge-triggered, so a desktop that acted only on
        // it never released a window the user minimised after pressure had
        // settled — the ordinary sequence, and the largest block the desktop
        // could have given back. The gesture is the other edge.
        static PRESSURE: ReportedPressure = ReportedPressure::unknown();
        PRESSURE.report(PressureBand::Normal);
        let (mut shell, mut compositor) = crate::tests::desktop_over(
            TaskbarConfig::bottom_bar(640, 480),
            mode(640, 480, DisplayFormat::Rgba8888),
            &PRESSURE,
        );
        shell.present(&mut compositor);

        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let window = {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                apps: &mut RecordingBar::default(),
                menu: &mut MenuChain::new(),
                seat_held: false,
                relay: &mut RefusingRelay,
                wallpapers: &mut RecordingGallery::default(),
                cursor_sets: &[],
            };
            open_one_sized(&mut host, 1, WindowSizing::default())
        };
        let held = |c: &Compositor, id| c.window(id).expect("window").has_content();
        assert!(held(&compositor, window));

        // The band moves to mild, and the desktop's answer to that wake keeps
        // the visible window's pixels — correctly, since it is on screen.
        PRESSURE.report(PressureBand::Mild);
        let _ = shell.trim_caches(&mut compositor);
        assert!(held(&compositor, window));
        let _ = compositor.pending_redraws();
        let _ = compositor.take_released_notices();

        // Now the user minimises it. No band change follows, and none is
        // needed: the pixels go, and the client is owed the news.
        assert!(shell.minimize_window(&mut compositor, window));
        assert!(!held(&compositor, window));
        assert_eq!(compositor.take_released_notices(), alloc::vec![window]);
        assert!(
            compositor.pending_redraws().is_empty(),
            "nothing is asked of an app whose window nobody can see"
        );

        // Raising it is what asks — the window picker's own path.
        assert!(shell.raise_window(&mut compositor, window));
        assert_eq!(compositor.pending_redraws(), alloc::vec![window]);
        PRESSURE.report(PressureBand::Normal);
    }

    #[test]
    fn minimising_a_comfortable_machines_window_keeps_its_pixels() {
        // Every release costs the owning app a repaint, so a machine with
        // memory to spare spends none: a minimise at normal pressure is a
        // visibility change and nothing else.
        static PRESSURE: ReportedPressure = ReportedPressure::unknown();
        PRESSURE.report(PressureBand::Normal);
        let (mut shell, mut compositor) = crate::tests::desktop_over(
            TaskbarConfig::bottom_bar(640, 480),
            mode(640, 480, DisplayFormat::Rgba8888),
            &PRESSURE,
        );
        shell.present(&mut compositor);

        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let window = {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
                apps: &mut RecordingBar::default(),
                menu: &mut MenuChain::new(),
                seat_held: false,
                relay: &mut RefusingRelay,
                wallpapers: &mut RecordingGallery::default(),
                cursor_sets: &[],
            };
            open_one_sized(&mut host, 1, WindowSizing::default())
        };

        assert!(shell.minimize_window(&mut compositor, window));
        assert!(compositor.window(window).expect("window").has_content());
        assert!(compositor.take_released_notices().is_empty());
        assert!(compositor.pending_redraws().is_empty());
    }
}
