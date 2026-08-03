//! The desktop: the user's own `Desktop` folder shown as a column of icons
//! down the trailing edge of the screen (`plans/NEW-TASKBAR.md` T7).
//!
//! The desktop is a *directory view*, not a new kind of surface. It lists the
//! user's `Desktop` folder through the same [`DirectorySource`] seam the
//! trusted file picker uses, under the session's own identity; it orders the
//! listing with the shared [`sort_entries`]; it classifies each child with the
//! shared content-type registry; and it lays its tiles out with the shared
//! [`GridView`], differing from the file manager's grid only in its
//! [`GridFlow`] — the desktop's column hugs the trailing edge and grows a new
//! column inward as it fills. There is no second grid, no second sort, and no
//! second classifier anywhere in this module.
//!
//! What the desktop *owns* is the behaviour a folder-on-the-screen needs:
//! hover feedback, a selection, keyboard navigation while it holds focus, and
//! activation. Each of those reuses the pure engine that already decides it —
//! [`DoubleClickTracker`] for "is this the second click?" — so the desktop can
//! never disagree with the file manager about what a gesture means.
//!
//! # Why the desktop is not a pin source
//!
//! Dragging an icon onto the taskbar's pin band is *not* a desktop gesture,
//! deliberately. An installed application lives in an application store —
//! machine-wide, or the user's own — and the pin store records exactly that:
//! a `.app` directory a user drops on their `Desktop` is a directory shaped
//! like an application, not an installed one, so pinning it could never
//! succeed and offering the gesture would be a promise the system cannot
//! keep. The pin drag source is the program-library popup, every row of
//! which is a catalogued entry by construction; see
//! [`tairix_taskbar::LibraryPopup`].
//!
//! # Why the re-list is gesture-driven
//!
//! There is no filesystem-change notification in this system: nothing tells a
//! process that a directory it is showing has gained a file. The desktop
//! therefore re-lists at the moments a change could plausibly have happened
//! and the user is about to look: at bring-up, after an action the session
//! itself performed that could have altered the folder, and when the pointer
//! arrives on the desktop having been somewhere else. It runs **no timer and
//! no polling loop** — a periodically-waking desktop would keep a core busy
//! and burn power to discover nothing, which is exactly the busy-poll this
//! system forbids. The pointer-arrival re-list is rate-limited by
//! [`RELIST_MIN_INTERVAL_NS`] so sweeping the mouse on and off the desktop
//! cannot turn a gesture into a re-listing loop.
//!
//! The model holds no authority: it *names* what should happen
//! ([`DesktopAction`]) and the embedder — which holds the spawn and pin
//! capabilities — carries it out.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_browse::render::{grid_metrics, grid_tile};
use tairix_browse::{
    applications_for, media_for_entry, sort_entries, AppAssociation, ClickKind, DirectorySource,
    DoubleClickTracker, Entry, EntryKind, GridFlow, GridView, SortMode,
};
use tairix_controls::state::{ControlState, FocusState, PointerState, SelectionState};
use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_icon::IconArtwork;
use tairix_raster::Surface;
use tairix_theme::Theme;
use tairix_wm::{Key, NamedKey};

/// The inset, in logical pixels at the reference density, between the work
/// area's edges and the first icon.
///
/// A deliberate, fixed piece of visual spacing — not a capacity — so the
/// trailing column does not touch the screen edge and the top icon clears the
/// work area's top edge.
pub const DESKTOP_MARGIN: u32 = 8;

/// The shortest interval, in nanoseconds, between two pointer-arrival
/// re-listings of the desktop folder (one second).
///
/// A deliberate, fixed rate limit on a *gesture*, not a scalable capacity:
/// sweeping the pointer on and off the desktop must not be able to turn a
/// mouse movement into a stream of directory reads. Reaching it never fails
/// anything — the desktop simply keeps showing the listing it already has.
pub const RELIST_MIN_INTERVAL_NS: u64 = 1_000_000_000;

/// What activating a desktop icon means, resolved by the model and carried
/// out by the embedder (which holds the spawn capability).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DesktopActivation {
    /// Open the file manager showing the directory at this absolute path.
    OpenFolder {
        /// The absolute path of the directory to show.
        path: String,
    },
    /// Launch an application: the absolute path of its `Run` binary, the name
    /// to report it by, and — when the user opened a document with it — the
    /// absolute path of the file to hand it as its argument.
    Launch {
        /// Absolute path of the bundle's `Run` entry-point binary.
        run_path: String,
        /// Display name for the launch record and any diagnosis.
        label: String,
        /// The document to open, if this launch came from a plain file.
        argument: Option<String>,
    },
}

/// What one desktop gesture asks the session to do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DesktopAction {
    /// Carry out this activation.
    Activate(DesktopActivation),
    /// The gesture was refused. The line is complete and newline-terminated,
    /// ready for `stderr`: a refused action always says why rather than
    /// failing silently.
    Refuse(String),
}

/// The outcome of one desktop gesture: whether the desktop's own pixels
/// changed, whether the gesture re-listed the folder, and what (if anything)
/// the session must now do.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DesktopOutcome {
    /// The desktop layer must be repainted.
    pub redraw: bool,
    /// The gesture re-listed the folder and its contents had changed.
    ///
    /// The moment the user's own files demonstrably moved under the desktop
    /// is the honest moment to re-read what is installed too, so the
    /// embedder refreshes anything it derives from the installed set (which
    /// application opens which file) here rather than only when the program
    /// library is opened. A re-list that found the folder unchanged reports
    /// `false`, so a pointer sweeping on and off the desktop costs nothing.
    pub relisted: bool,
    /// The action the gesture asks for, if any.
    pub action: Option<DesktopAction>,
}

impl DesktopOutcome {
    /// The gesture changed nothing at all.
    #[must_use]
    pub const fn ignored() -> Self {
        Self {
            redraw: false,
            relisted: false,
            action: None,
        }
    }

    /// The gesture changed only what is drawn.
    #[must_use]
    pub const fn redraw() -> Self {
        Self {
            redraw: true,
            relisted: false,
            action: None,
        }
    }

    /// The gesture changed what is drawn *and* asks for `action`.
    #[must_use]
    pub const fn acting(action: DesktopAction) -> Self {
        Self {
            redraw: true,
            relisted: false,
            action: Some(action),
        }
    }
}

/// The desktop's icon column: the listing, what the pointer and keyboard are
/// doing to it, and the pure double-click engine it shares with the file
/// manager.
///
/// `S` is the injected [`DirectorySource`] — the live VFS listing under the
/// session's own identity in production, an in-memory tree in tests.
pub struct Desktop<S: DirectorySource> {
    source: S,
    /// Root-first components of the user's `Desktop` folder.
    folder: Vec<String>,
    entries: Vec<Entry>,
    selected: Option<usize>,
    hovered: Option<usize>,
    focused: bool,
    clicks: DoubleClickTracker,
    /// Monotonic nanoseconds of the last listing, or `None` before the first.
    listed_at_ns: Option<u64>,
    /// Whether the pointer was last seen over the desktop rather than over a
    /// window or the taskbar — the edge that triggers the arrival re-list.
    pointer_over: bool,
}

impl<S: DirectorySource> Desktop<S> {
    /// A desktop over `source` showing the folder named by the root-first
    /// `folder` components. Nothing is listed until [`relist`](Self::relist).
    #[must_use]
    pub fn new(source: S, folder: Vec<String>) -> Self {
        Self {
            source,
            folder,
            entries: Vec::new(),
            selected: None,
            hovered: None,
            focused: false,
            clicks: DoubleClickTracker::new(),
            listed_at_ns: None,
            pointer_over: false,
        }
    }

    /// The entries currently shown, in the shared listing order.
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The selected icon's index, if any.
    #[must_use]
    pub const fn selected(&self) -> Option<usize> {
        self.selected
    }

    /// The icon the pointer is over, if any.
    #[must_use]
    pub const fn hovered(&self) -> Option<usize> {
        self.hovered
    }

    /// Whether the desktop holds the keyboard (no window is focused).
    #[must_use]
    pub const fn is_focused(&self) -> bool {
        self.focused
    }

    /// Tell the desktop whether it holds the keyboard. Returns whether that
    /// changed, so the caller repaints only when the focus ring appears or
    /// disappears.
    pub fn set_focused(&mut self, focused: bool) -> bool {
        let changed = self.focused != focused;
        self.focused = focused;
        changed
    }

    /// Note that the pointer is somewhere other than the desktop (over a
    /// window, the taskbar, or one of its popovers), clearing the hover.
    ///
    /// The next arrival on the desktop is then a real *entry*, which is what
    /// the rate-limited re-list keys on.
    pub fn pointer_left(&mut self) -> DesktopOutcome {
        self.pointer_over = false;
        if self.hovered.take().is_some() {
            return DesktopOutcome::redraw();
        }
        DesktopOutcome::ignored()
    }

    /// Re-list the folder now, whatever the rate limit says: the caller knows
    /// something changed (bring-up, or an action the session itself performed
    /// on the folder). Returns whether the shown set changed.
    ///
    /// A listing the source refuses leaves the desktop empty and selects
    /// nothing rather than showing a stale or guessed folder.
    pub fn relist(&mut self, now_ns: u64) -> bool {
        self.listed_at_ns = Some(now_ns);
        let mut entries = self.source.list(&self.folder).unwrap_or_default();
        sort_entries(&mut entries, SortMode::default_order());
        if entries == self.entries {
            return false;
        }
        // The selection follows the *name* it was on, so a re-list that adds
        // or removes a file never silently moves the selection to a different
        // icon under the user's pointer.
        let chosen = self
            .selected
            .and_then(|index| self.entries.get(index))
            .map(|entry| entry.name().to_string());
        self.entries = entries;
        self.selected = chosen.and_then(|name| {
            self.entries
                .iter()
                .position(|entry| entry.name() == name.as_str())
        });
        self.hovered = None;
        true
    }

    /// The pointer arrived on the desktop having been elsewhere: re-list, but
    /// no more often than [`RELIST_MIN_INTERVAL_NS`]. Returns whether the
    /// shown set changed.
    fn relist_on_arrival(&mut self, now_ns: u64) -> bool {
        let due = self
            .listed_at_ns
            .is_none_or(|last| now_ns.saturating_sub(last) >= RELIST_MIN_INTERVAL_NS);
        due && self.relist(now_ns)
    }

    /// The grid the desktop's icons are laid out in: the shared tile geometry
    /// under the trailing-edge column flow, inset from `work_area` by
    /// [`DESKTOP_MARGIN`].
    ///
    /// `work_area` is the screen with the taskbar's band removed, so an icon
    /// can never be drawn under the bar or hit-tested through it.
    #[must_use]
    pub fn layout(&self, work_area: Rect, scale: Scale, font: BitmapFont) -> GridView {
        let (cell_width, cell_height, gap) = grid_metrics(font);
        let margin = scale.scale_length(DESKTOP_MARGIN);
        let viewport = Rect::new(
            work_area.origin.x.saturating_add_unsigned(margin),
            work_area.origin.y.saturating_add_unsigned(margin),
            work_area.width.saturating_sub(margin.saturating_mul(2)),
            work_area.height.saturating_sub(margin.saturating_mul(2)),
        );
        GridView::new(
            viewport,
            cell_width,
            cell_height,
            gap,
            0,
            self.entries.len(),
            GridFlow::ColumnsFromTrailing,
        )
    }

    /// Paint the visible icons into `surface` through the shared card tile,
    /// resolving each one's artwork from `artwork` at exactly the slot side
    /// the tile will draw it in.
    ///
    /// Only the icons the column actually shows are painted and only their
    /// artwork is asked for, so a folder with more icons than fit costs
    /// nothing for the ones off screen. An icon whose artwork the lookup
    /// declines falls back to the shared class glyph inside the card, so a
    /// system with no `/System/Graphics` still shows a meaningful desktop.
    pub fn render(
        &self,
        surface: &mut Surface,
        layout: &GridView,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
        artwork: &mut dyn IconArtwork,
    ) {
        for index in layout.visible_range(0) {
            let Some(entry) = self.entries.get(index) else {
                break;
            };
            let Some(bounds) = layout.cell_rect(0, index) else {
                continue;
            };
            let kind = media_for_entry(entry, &self.folder).icon();
            let tile = grid_tile(entry, self.icon_state(index), kind);
            let side = tile.icon_side(bounds, scale, theme, font);
            let art = artwork.artwork(kind, side);
            tile.render(surface, bounds, scale, theme, font, art);
        }
    }

    /// The composed control state of the icon at `index`: selected, hovered,
    /// and — when the desktop holds the keyboard and this is the selection —
    /// focused.
    fn icon_state(&self, index: usize) -> ControlState {
        let mut state = ControlState::idle();
        if self.selected == Some(index) {
            state.selection = SelectionState::Selected;
            if self.focused {
                state.focus = FocusState::FOCUSED;
            }
        }
        if self.hovered == Some(index) {
            state.pointer = PointerState::Hover;
        }
        state
    }

    /// Pointer motion to screen position `at`.
    ///
    /// Arriving on the desktop from somewhere else re-lists the folder (rate
    /// limited), so a file created while the user was in another window is
    /// there when they look. Motion otherwise only drives the hover highlight.
    pub fn pointer_moved(&mut self, at: Point, layout: &GridView, now_ns: u64) -> DesktopOutcome {
        let arrived = !core::mem::replace(&mut self.pointer_over, true);
        let relisted = arrived && self.relist_on_arrival(now_ns);
        let mut redraw = relisted;
        let hovered = index_at(layout, at);
        if self.hovered != hovered {
            self.hovered = hovered;
            redraw = true;
        }
        DesktopOutcome {
            redraw,
            relisted,
            action: None,
        }
    }

    /// A primary press at screen position `at`, at monotonic time `now_ns`.
    ///
    /// A press on an icon selects it and arms the double-click engine, so a
    /// second press on the same icon within the shared window activates it. A
    /// press on empty desktop clears the selection.
    pub fn press(
        &mut self,
        at: Point,
        layout: &GridView,
        now_ns: u64,
        apps: &[AppAssociation],
    ) -> DesktopOutcome {
        self.focused = true;
        let Some(index) = index_at(layout, at) else {
            let had = self.selected.take().is_some();
            return if had {
                DesktopOutcome::redraw()
            } else {
                DesktopOutcome::ignored()
            };
        };
        self.selected = Some(index);
        if self.clicks.register(now_ns, index) == ClickKind::Double {
            return self.activate(index, apps);
        }
        DesktopOutcome::redraw()
    }

    /// A key while the desktop holds the keyboard: the arrows move the
    /// selection, `Enter` activates it, and `Escape` clears it.
    ///
    /// A key the desktop has no meaning for changes nothing. Releases are
    /// ignored: every desktop key acts on the press.
    pub fn key(
        &mut self,
        key: Key,
        pressed: bool,
        layout: &GridView,
        apps: &[AppAssociation],
    ) -> DesktopOutcome {
        if !pressed || !self.focused {
            return DesktopOutcome::ignored();
        }
        match key {
            Key::Named(NamedKey::Enter) => match self.selected {
                Some(index) => self.activate(index, apps),
                None => DesktopOutcome::ignored(),
            },
            Key::Named(NamedKey::Escape) => {
                if self.selected.take().is_some() {
                    DesktopOutcome::redraw()
                } else {
                    DesktopOutcome::ignored()
                }
            }
            Key::Named(named) => match Step::for_key(named) {
                Some(step) => self.move_selection(step, layout),
                None => DesktopOutcome::ignored(),
            },
            Key::Char(_) => DesktopOutcome::ignored(),
        }
    }

    /// Move the selection one `step` along the listing, clamped to its ends.
    /// With nothing selected the first arrow selects the first icon, so the
    /// keyboard always has somewhere to start.
    fn move_selection(&mut self, step: Step, layout: &GridView) -> DesktopOutcome {
        if self.entries.is_empty() {
            return DesktopOutcome::ignored();
        }
        let last = self.entries.len().saturating_sub(1);
        let next = match self.selected {
            None => 0,
            Some(current) => step.applied(current, layout.cells_per_line()).min(last),
        };
        if self.selected == Some(next) {
            return DesktopOutcome::ignored();
        }
        self.selected = Some(next);
        DesktopOutcome::redraw()
    }

    /// Resolve what activating the icon at `index` means.
    ///
    /// A directory opens the file manager at it; an application bundle
    /// launches; a plain file launches the application the shared association
    /// model picks for it, with the file as its argument. A file nothing is
    /// associated with is refused with a stated reason and nothing else
    /// happens.
    fn activate(&self, index: usize, apps: &[AppAssociation]) -> DesktopOutcome {
        let Some(entry) = self.entries.get(index) else {
            return DesktopOutcome::ignored();
        };
        let path = self.path_of(entry.name());
        match entry.kind() {
            EntryKind::Directory => {
                DesktopOutcome::acting(DesktopAction::Activate(DesktopActivation::OpenFolder {
                    path,
                }))
            }
            EntryKind::Bundle => DesktopOutcome::acting(DesktopAction::Activate(launch_of(
                &path,
                bundle_label(entry.name()),
                None,
            ))),
            EntryKind::File => match applications_for(entry.name(), apps).first() {
                Some(app) => DesktopOutcome::acting(DesktopAction::Activate(launch_of(
                    app.bundle_path(),
                    app.name().to_string(),
                    Some(path),
                ))),
                None => DesktopOutcome::acting(DesktopAction::Refuse(format!(
                    "desktop: no installed application opens '{}'\n",
                    entry.name()
                ))),
            },
        }
    }

    /// The absolute path of the child called `name` inside the desktop folder.
    fn path_of(&self, name: &str) -> String {
        let mut path = String::new();
        for component in &self.folder {
            path.push('/');
            path.push_str(component);
        }
        path.push('/');
        path.push_str(name);
        path
    }
}

/// One arrow-key move over the icon column.
///
/// The desktop's icons flow *down* a column before wrapping, and the columns
/// grow inward from the trailing edge, so up/down is one icon while
/// right/left is one whole column — and "one column" is however many icons
/// the live grid fits, never a number this module guesses.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Step {
    /// One icon further down the listing.
    NextIcon,
    /// One icon back up the listing.
    PreviousIcon,
    /// One whole column inward (leftward), which is later in the listing.
    NextColumn,
    /// One whole column outward (rightward), which is earlier in the listing.
    PreviousColumn,
}

impl Step {
    /// The step an arrow key asks for, or `None` when the key means nothing to
    /// the desktop.
    const fn for_key(key: NamedKey) -> Option<Self> {
        match key {
            NamedKey::Down => Some(Self::NextIcon),
            NamedKey::Up => Some(Self::PreviousIcon),
            NamedKey::Left => Some(Self::NextColumn),
            NamedKey::Right => Some(Self::PreviousColumn),
            _ => None,
        }
    }

    /// This step applied to the icon at `current` in a column holding
    /// `per_column` icons, saturating at the start of the listing.
    fn applied(self, current: usize, per_column: usize) -> usize {
        let column = per_column.max(1);
        match self {
            Self::NextIcon => current.saturating_add(1),
            Self::PreviousIcon => current.saturating_sub(1),
            Self::NextColumn => current.saturating_add(column),
            Self::PreviousColumn => current.saturating_sub(column),
        }
    }
}

/// The launch activation for the bundle at `bundle`, reported as `label` and
/// optionally handed `argument`. One spelling of "a bundle's entry point is
/// its `Run` binary", so the desktop's three launch paths cannot diverge.
fn launch_of(bundle: &str, label: String, argument: Option<String>) -> DesktopActivation {
    DesktopActivation::Launch {
        run_path: format!("{bundle}/Run"),
        label,
        argument,
    }
}

/// The name an application bundle is reported by: its directory name without
/// the bundle suffix.
fn bundle_label(name: &str) -> String {
    name.strip_suffix(tairix_abi::BUNDLE_SUFFIX)
        .unwrap_or(name)
        .to_string()
}

/// The icon at screen position `at`, through the shared grid hit-test. A
/// negative coordinate is off every icon.
fn index_at(layout: &GridView, at: Point) -> Option<usize> {
    let x = u32::try_from(at.x).ok()?;
    let y = u32::try_from(at.y).ok()?;
    layout.index_at(0, x, y)
}
