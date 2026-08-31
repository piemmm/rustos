//! The desktop: the user's own `Desktop` folder shown as a column of icons
//! down the screen edge their settings name (`plans/NEW-TASKBAR.md` T7,
//! `plans/PINBOARD.md`).
//!
//! The desktop is a *directory view*, not a new kind of surface. It lists the
//! user's `Desktop` folder through the same [`DirectorySource`] seam the
//! trusted file picker uses, under the session's own identity; it orders the
//! listing with the shared [`sort_entries`]; it classifies each child with the
//! shared content-type registry; and it lays its tiles out with the shared
//! [`GridView`], differing from the file manager's grid only in its
//! [`GridFlow`] and [`GridFill`] — the desktop's column hugs the edge the
//! user's arrangement names, grows a new column inward as it fills, and keeps
//! a fixed pitch so an icon does not drift when the work area changes size.
//! There is no second grid, no second sort, and no second classifier anywhere
//! in this module.
//!
//! # The pinboard settings live here
//!
//! The desktop owns the user's [`PinboardSettings`] — the wallpaper and its
//! fit, the backdrop colour, the icon arrangement, and the sort order — as the
//! single copy inside the session: the shell reads them from the desktop
//! rather than holding a second set that could drift from the one the icons
//! are actually laid out by. An edit arrives through
//! [`Desktop::apply_settings`], which reports exactly the work it implies, so
//! changing the sort order does not decode a wallpaper and changing the
//! wallpaper does not re-read the folder.
//!
//! What the desktop *owns* is the behaviour a folder-on-the-screen needs:
//! hover feedback, a selection, keyboard navigation while it holds focus, and
//! activation. Each of those reuses the pure engine that already decides it —
//! [`DoubleClickTracker`] for "is this the second click?" — so the desktop can
//! never disagree with the file manager about what a gesture means.
//!
//! # Shortcuts point *into* the desktop, never out of it
//!
//! The program library's row menu asks this folder for a shortcut
//! ([`Desktop::shortcut_to`]) and never the other way round. A `.app`
//! directory a user drops on their own `Desktop` is a directory *shaped* like
//! an application, not a catalogued one, so the desktop is not a source a
//! launcher can be populated from; the library's rows are catalogued entries
//! by construction (see [`tairix_taskbar::LibraryPopup`]) and are what a
//! shortcut is made from.
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
//! ([`DesktopAction`]) and the embedder — which holds the spawn and
//! filesystem capabilities — carries it out.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_browse::render::{grid_metrics, grid_tile};
use tairix_browse::{
    applications_for, entry_icon_request, media_for_entry, sort_entries, suggest_new_dir_name,
    AppAssociation, ClickKind, DirectorySource, DoubleClickTracker, Entry, EntryKind, GridFill,
    GridFlow, GridView, LinkTarget, Listing, SortDirection, SortKey, SortMode,
};
use tairix_controls::state::{ControlState, FocusState, PointerState, SelectionState};
use tairix_controls::IconTile;
use tairix_geometry::{Point, Rect, Region, Scale};
use tairix_icon::IconArtwork;
use tairix_proglib::{Catalog, EntryId};
use tairix_raster::Surface;
use tairix_theme::Theme;
use tairix_wallpaper::{IconFlow, IconSort, PinboardSettings};
use tairix_wm::{Key, NamedKey, PointerButton};

use crate::library::catalogued;
use crate::pinboard::PinboardCommand;

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
    /// Create a directory at this absolute path.
    ///
    /// The name is already chosen — through the shared new-directory naming
    /// the file manager uses, over the listing the desktop is showing — so the
    /// embedder only holds the filesystem capability and makes the directory.
    CreateFolder {
        /// Absolute path of the directory to create.
        path: String,
    },
    /// Create a symbolic link — a desktop shortcut — at `link`, storing
    /// `target` verbatim.
    ///
    /// The name is already spelled and already validated against the one
    /// shared name rule ([`Desktop::shortcut_to`]), so the embedder only holds
    /// the filesystem capability and makes the link. The target is stored as
    /// *data* and never resolved here: a shortcut whose bundle is later
    /// removed dangles honestly rather than being prevented at creation.
    CreateShortcut {
        /// Absolute path of the link to create, inside the desktop folder.
        link: String,
        /// The path the link stores, exactly as it was given.
        target: String,
    },
    /// Adopt these settings: persist them to the user's own store and hand
    /// them back through [`Desktop::apply_settings`], which reports the work
    /// the edit actually implies.
    ///
    /// The model names the new settings; it does not apply them itself, so
    /// there is exactly one place settings are adopted and the persisted
    /// document and the live desktop can never drift apart.
    AdoptSettings(PinboardSettings),
    /// Open the wallpaper chooser, which is an installed application the
    /// embedder resolves and launches (the model knows no bundle paths).
    ChangeBackground,
    /// The gesture was refused. The line is complete and newline-terminated,
    /// ready for `stderr`: a refused action always says why rather than
    /// failing silently.
    Refuse(String),
}

/// The work a settings edit implies, beyond the repaint that having changed
/// anything at all already implies.
///
/// Each field names one piece of work the *edit* asks for, so a change of sort
/// order does not cost a wallpaper decode and a change of wallpaper does not
/// cost a directory read. An edit that only changes the backdrop colour asks
/// for none of them — the repaint alone shows it.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct PinboardChange {
    /// The icon arrangement moved: the grid must be laid out again before the
    /// next paint or hit-test.
    pub relayout: bool,
    /// The sort order changed: the folder must be listed again to pick the new
    /// order up.
    pub relist: bool,
    /// The wallpaper image or its fit changed: the embedder must prepare the
    /// screen-sized wallpaper surface again and hand it to the shell.
    pub wallpaper: bool,
}

/// The outcome of one desktop gesture: whether the gesture re-listed the
/// folder, and what (if anything) the session must now do.
///
/// What the gesture *changed on screen* is not here. Every gesture takes a
/// [`Region`] sink and adds the icon cells it altered to it, so the embedder
/// repaints those cells rather than the whole desktop layer: the desktop is
/// the bottom layer, and marking all of it recomposites every window above it
/// and throws away every frosted backdrop over it — a screenful of work to
/// move one highlight.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DesktopOutcome {
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
    /// The gesture asks for nothing.
    #[must_use]
    pub const fn ignored() -> Self {
        Self {
            relisted: false,
            action: None,
        }
    }

    /// The gesture asks for `action`.
    #[must_use]
    pub const fn acting(action: DesktopAction) -> Self {
        Self {
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
    /// The user's pinboard settings. The desktop is their single owner inside
    /// the session: the shell reads them from here rather than keeping a
    /// second copy that could drift.
    settings: PinboardSettings,
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
    /// `folder` components, on the default pinboard settings. Nothing is
    /// listed until [`relist`](Self::relist).
    ///
    /// The settings an absent store document implies are the defaults, so a
    /// desktop is fully specified before the embedder has read anything; the
    /// user's own document arrives through
    /// [`apply_settings`](Self::apply_settings).
    #[must_use]
    pub fn new(source: S, folder: Vec<String>) -> Self {
        Self {
            source,
            folder,
            settings: PinboardSettings::default(),
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

    /// The pinboard settings in force.
    #[must_use]
    pub const fn settings(&self) -> &PinboardSettings {
        &self.settings
    }

    /// The absolute path of the folder the desktop shows.
    #[must_use]
    pub fn folder_path(&self) -> String {
        tairix_browse::vfs::spell_absolute_path(&self.folder)
    }

    /// What creating a shortcut to the catalogued program `entry` asks for.
    ///
    /// The program library's row menu is the one caller: a shortcut is a
    /// symbolic link in *this* folder, so the desktop — which owns the folder
    /// and its naming — decides the name and spells the link, and the embedder
    /// only makes it.
    ///
    /// The link takes the entry's **display name** and points at the bundle
    /// *directory* it launches. That is what makes the shortcut read as an
    /// application on the desktop: bundle-ness is decided from the target's own
    /// leaf name, never from the link's, so `Chess` → `/Apps/chess.app` is an
    /// application while the link itself is just a name. The target is carried
    /// verbatim and never resolved here, so a shortcut whose bundle is later
    /// removed dangles honestly rather than being prevented at creation.
    ///
    /// A display name is not automatically a file name: it must be one legal
    /// filesystem component under the same shared
    /// [`tairix_path::validate_file_name`] rule a typed folder or rename name
    /// obeys, so the desktop and the file manager can never disagree about what
    /// a name may be. A name that rule refuses — one carrying a `/` or a `:`,
    /// say — is [`DesktopAction::Refuse`]d with the rule's own reason rather
    /// than spelled into a path the create could only fail on.
    ///
    /// An existing name is **not** worked around: the link replaces nothing, so
    /// a name already taken is the kernel's own `AlreadyExists` at create time
    /// and is reported as the refusal it is. Picking a free name instead would
    /// silently make a *second*, differently-named shortcut for a user who
    /// already has one, and could only be decided against a listing that may
    /// already be stale.
    #[must_use]
    pub fn shortcut_to(&self, catalog: &Catalog, entry: &EntryId) -> DesktopAction {
        let chosen = match catalogued(catalog, entry) {
            Ok(chosen) => chosen,
            Err(reason) => return DesktopAction::Refuse(reason),
        };
        let name = chosen.name().as_str();
        if let Err(err) = tairix_path::validate_file_name(name) {
            return DesktopAction::Refuse(format!(
                "desktop: '{name}' cannot be a shortcut name ({err})\n"
            ));
        }
        DesktopAction::CreateShortcut {
            link: self.path_of(name),
            target: chosen.bundle().to_string(),
        }
    }

    /// Adopt `settings`, reporting the work the edit implies.
    ///
    /// `None` means `settings` were already in force: nothing changed and there
    /// is nothing to do. Anything else means the desktop layer must be
    /// repainted — that is what a change *is* — and the returned
    /// [`PinboardChange`] names the further work on top of it, so the caller
    /// re-lays out, re-lists, or re-prepares the wallpaper only when the edit
    /// actually asks for it. The desktop applies nothing beyond its own state.
    pub fn apply_settings(&mut self, settings: PinboardSettings) -> Option<PinboardChange> {
        if settings == self.settings {
            return None;
        }
        let change = PinboardChange {
            relayout: settings.icons != self.settings.icons,
            relist: settings.sort != self.settings.sort,
            wallpaper: settings.wallpaper != self.settings.wallpaper
                || settings.fit != self.settings.fit,
        };
        self.settings = settings;
        Some(change)
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

    /// Tell the desktop whether it holds the keyboard, adding what that
    /// changed to `damage`.
    ///
    /// Only the selected icon wears the Focus Ring, so gaining or losing the
    /// keyboard changes that one cell and nothing else — and with nothing
    /// selected it changes no pixel at all. This is the click that moves
    /// focus between the desktop and a window, which is exactly when the
    /// screen must *not* be repainted wholesale.
    pub fn set_focused(&mut self, focused: bool, layout: &GridView, damage: &mut Region) {
        if self.focused == focused {
            return;
        }
        self.focused = focused;
        Self::mark_cell(layout, self.selected, damage);
    }

    /// Add the cell the icon at `index` occupies to `damage`.
    ///
    /// The one place an icon's footprint is spelled: a tile draws strictly
    /// inside the cell the shared grid gives it, so repainting that rectangle
    /// is the whole of repainting the icon. An index the column does not
    /// currently show has no cell and damages nothing.
    fn mark_cell(layout: &GridView, index: Option<usize>, damage: &mut Region) {
        if let Some(rect) = index.and_then(|index| layout.cell_rect(0, index)) {
            damage.add(rect);
        }
    }

    /// Note that the pointer is somewhere other than the desktop (over a
    /// window, the taskbar, or one of its popovers), clearing the hover.
    ///
    /// The next arrival on the desktop is then a real *entry*, which is what
    /// the rate-limited re-list keys on.
    pub fn pointer_left(&mut self, layout: &GridView, damage: &mut Region) -> DesktopOutcome {
        self.pointer_over = false;
        Self::mark_cell(layout, self.hovered.take(), damage);
        DesktopOutcome::ignored()
    }

    /// Ask for a fresh listing of the folder now, whatever the rate limit says:
    /// the caller knows something changed (bring-up, or an action the session
    /// itself performed on the folder). Returns whether the shown set changed.
    ///
    /// A listing the source refuses leaves the desktop empty and selects
    /// nothing rather than showing a stale or guessed folder.
    ///
    /// A source that reads the folder elsewhere answers "not yet", and this
    /// changes nothing at all — the icons already on screen stay there, and the
    /// caller calls again on the wake that says the read finished. Blanking the
    /// column while a read is in flight would make every re-list flicker.
    pub fn relist(&mut self, now_ns: u64) -> bool {
        self.listed_at_ns = Some(now_ns);
        let mut entries = match self.source.list(&self.folder) {
            Ok(Listing::Ready(entries)) => entries,
            Ok(Listing::Pending) => return false,
            Err(_) => Vec::new(),
        };
        sort_entries(&mut entries, sort_mode(self.settings.sort));
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
    /// under the column flow the settings' arrangement names, inset from
    /// `work_area` by [`DESKTOP_MARGIN`].
    ///
    /// `work_area` is the screen with the taskbar's band removed, so an icon
    /// can never be drawn under the bar or hit-tested through it.
    #[must_use]
    pub fn layout(&self, work_area: Rect, scale: Scale, theme: &Theme) -> GridView {
        let margin = scale.scale_length(DESKTOP_MARGIN);
        let viewport = Rect::new(
            work_area.origin.x.saturating_add_unsigned(margin),
            work_area.origin.y.saturating_add_unsigned(margin),
            work_area.width.saturating_sub(margin.saturating_mul(2)),
            work_area.height.saturating_sub(margin.saturating_mul(2)),
        );
        // The field is fixed, not resizable: keeping the pitch anchored to the
        // edge the icons hug means an icon stays where the user last saw it
        // whatever the work area's exact extent is, rather than drifting as the
        // file manager's spreading grid deliberately does.
        GridView::new(
            viewport,
            grid_metrics(scale, theme),
            0,
            self.entries.len(),
            grid_flow(self.settings.icons),
            GridFill::FixedPitch,
        )
    }

    /// Paint the visible icons that fall inside `area` into `surface` through
    /// the shared icon tile, resolving each one's artwork from `artwork` at
    /// exactly the slot side the tile will draw it in.
    ///
    /// Only the icons the column actually shows are painted and only their
    /// artwork is asked for, so a folder with more icons than fit costs
    /// nothing for the ones off screen. `area` narrows that again to the
    /// rectangle being repainted, so moving a highlight costs the cells that
    /// changed rather than every icon on screen — a tile draws strictly inside
    /// its own cell, so a cell `area` misses has nothing in `area` to draw.
    /// An application bundle on the desktop names itself in its request, so it
    /// draws the icon it carries in its own `Resources/` rather than the
    /// generic bundle picture. An icon whose artwork the lookup declines falls
    /// back to the shared class glyph inside the tile, so a system with no
    /// `/System/Graphics` still shows a meaningful desktop.
    pub fn render(
        &self,
        surface: &mut Surface,
        layout: &GridView,
        scale: Scale,
        theme: &Theme,
        artwork: &mut dyn IconArtwork,
        area: Rect,
    ) {
        // Spelled once for the whole pass; a bundle icon appends its own leaf
        // into this one buffer rather than allocating a path per tile.
        let dir = tairix_browse::vfs::spell_absolute_path(&self.folder);
        let mut bundle = String::new();
        for index in layout.visible_range(0) {
            let Some(entry) = self.entries.get(index) else {
                break;
            };
            let Some(bounds) = layout.cell_rect(0, index) else {
                continue;
            };
            if bounds.intersection(&area).is_empty() {
                continue;
            }
            let kind = media_for_entry(entry, &self.folder).icon();
            let tile = grid_tile(entry, self.icon_state(index), kind);
            let side = IconTile::icon_side(bounds, scale, theme);
            let request = entry_icon_request(&dir, entry, kind, &mut bundle);
            let art = artwork.artwork(request, side);
            tile.render(surface, bounds, scale, theme, art);
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
    /// A re-list that changed the shown set reports `relisted`, which is the
    /// caller's signal to repaint the whole column: the icons themselves
    /// moved, so no cell of the old layout describes the new one.
    pub fn pointer_moved(
        &mut self,
        at: Point,
        layout: &GridView,
        now_ns: u64,
        damage: &mut Region,
    ) -> DesktopOutcome {
        let arrived = !core::mem::replace(&mut self.pointer_over, true);
        let relisted = arrived && self.relist_on_arrival(now_ns);
        let hovered = index_at(layout, at);
        if self.hovered != hovered {
            Self::mark_cell(layout, self.hovered, damage);
            Self::mark_cell(layout, hovered, damage);
            self.hovered = hovered;
        }
        DesktopOutcome {
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
        damage: &mut Region,
    ) -> DesktopOutcome {
        // Taking the keyboard puts the ring on whatever is selected, so the
        // old selection's cell is repainted whether the press moves the
        // selection or merely claims focus.
        self.focused = true;
        Self::mark_cell(layout, self.selected, damage);
        let Some(index) = index_at(layout, at) else {
            self.selected = None;
            return DesktopOutcome::ignored();
        };
        self.selected = Some(index);
        Self::mark_cell(layout, self.selected, damage);
        if self.clicks.register(now_ns, index, PointerButton::Primary) == ClickKind::Double {
            return self.activate(index, apps);
        }
        DesktopOutcome::ignored()
    }

    /// A secondary (right) press at screen position `at`: the pinboard's
    /// context-menu gesture. Answers whether the press landed on an icon,
    /// which is the only thing that decides whether the menu offers `Open`.
    ///
    /// A press on an icon selects it, so the menu acts on the thing the user
    /// pointed at; a press on empty backdrop leaves the selection exactly as
    /// it was, because asking for the menu is not a way to lose a selection.
    /// The gesture claims no keyboard focus: the window manager does not move
    /// focus for a secondary press on the backdrop, and the desktop does not
    /// pretend otherwise.
    ///
    /// It names no [`DesktopAction`]: the menu is the seat's one chain, opened
    /// by the embedder that owns it, so the desktop model describes the rows
    /// and never asks for a surface.
    pub fn context_press(&mut self, at: Point, layout: &GridView, damage: &mut Region) -> bool {
        let on_icon = index_at(layout, at);
        if let Some(index) = on_icon {
            if self.selected != Some(index) {
                Self::mark_cell(layout, self.selected, damage);
                self.selected = Some(index);
                Self::mark_cell(layout, self.selected, damage);
            }
        }
        on_icon.is_some()
    }

    /// Resolve one pinboard menu `command` against the desktop's own state, at
    /// monotonic time `now_ns`.
    ///
    /// This is the single translation from a named command to a
    /// [`DesktopAction`]: `Open` resolves through the very same activation the
    /// double-click path uses, a sort or arrangement row names the settings the
    /// embedder is to adopt (never applying them behind its back), a new folder
    /// is named through the shared new-directory naming over the listing on
    /// screen, and `Refresh` re-lists here and now. A command that asks for
    /// what is already in force changes nothing.
    pub fn command(
        &mut self,
        command: PinboardCommand,
        apps: &[AppAssociation],
        now_ns: u64,
    ) -> DesktopOutcome {
        match command {
            PinboardCommand::Open => self.activate_selection(apps),
            PinboardCommand::NewFolder => DesktopOutcome::acting(DesktopAction::CreateFolder {
                path: self.path_of(&suggest_new_dir_name(&self.entries)),
            }),
            PinboardCommand::SortBy(sort) => self.adopt(PinboardSettings {
                sort,
                ..self.settings.clone()
            }),
            PinboardCommand::ArrangeFrom(icons) => self.adopt(PinboardSettings {
                icons,
                ..self.settings.clone()
            }),
            PinboardCommand::Refresh => DesktopOutcome {
                relisted: self.relist(now_ns),
                action: None,
            },
            PinboardCommand::OpenDesktopFolder => {
                DesktopOutcome::acting(DesktopAction::Activate(DesktopActivation::OpenFolder {
                    path: self.folder_path(),
                }))
            }
            PinboardCommand::ChangeBackground => {
                DesktopOutcome::acting(DesktopAction::ChangeBackground)
            }
        }
    }

    /// Name the settings edit `next` for the embedder to adopt, or change
    /// nothing when it asks for the settings already in force.
    fn adopt(&self, next: PinboardSettings) -> DesktopOutcome {
        if next == self.settings {
            return DesktopOutcome::ignored();
        }
        DesktopOutcome::acting(DesktopAction::AdoptSettings(next))
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
        damage: &mut Region,
    ) -> DesktopOutcome {
        if !pressed || !self.focused {
            return DesktopOutcome::ignored();
        }
        match key {
            Key::Named(NamedKey::Enter) => self.activate_selection(apps),
            Key::Named(NamedKey::Escape) => {
                Self::mark_cell(layout, self.selected.take(), damage);
                DesktopOutcome::ignored()
            }
            Key::Named(named) => match Step::for_key(named, self.settings.icons) {
                Some(step) => self.move_selection(step, layout, damage),
                None => DesktopOutcome::ignored(),
            },
            Key::Char(_) => DesktopOutcome::ignored(),
        }
    }

    /// Move the selection one `step` along the listing, clamped to its ends.
    /// With nothing selected the first arrow selects the first icon, so the
    /// keyboard always has somewhere to start.
    fn move_selection(
        &mut self,
        step: Step,
        layout: &GridView,
        damage: &mut Region,
    ) -> DesktopOutcome {
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
        Self::mark_cell(layout, self.selected, damage);
        self.selected = Some(next);
        Self::mark_cell(layout, self.selected, damage);
        DesktopOutcome::ignored()
    }

    /// Activate whatever is selected, if anything.
    ///
    /// The one definition of "open the selection", so the menu's `Open` row,
    /// the `Enter` key, and a double-click can never disagree about what
    /// opening an icon means.
    fn activate_selection(&self, apps: &[AppAssociation]) -> DesktopOutcome {
        match self.selected {
            Some(index) => self.activate(index, apps),
            None => DesktopOutcome::ignored(),
        }
    }

    /// Resolve what activating the icon at `index` means.
    ///
    /// A directory opens the file manager at it; an application bundle
    /// launches; a plain file launches the application the shared association
    /// model picks for it, with the file as its argument. A file nothing is
    /// associated with is refused with a stated reason and nothing else
    /// happens.
    ///
    /// A **shortcut** — a symbolic link on the desktop — acts on what it
    /// names: a folder or a file is opened through the link (the kernel
    /// resolves the final link), while a bundle is launched by its *resolved*
    /// path, because the spawn gate parses an entry point as
    /// `…/<Name>.app/Run` and a shortcut named after the program is not that
    /// shape. A shortcut whose target has gone is refused with its reason,
    /// never launched blind.
    fn activate(&self, index: usize, apps: &[AppAssociation]) -> DesktopOutcome {
        let Some(entry) = self.entries.get(index) else {
            return DesktopOutcome::ignored();
        };
        let path = self.path_of(entry.name());
        let bundle_path = match entry.kind() {
            EntryKind::Link(_) => match entry.target() {
                Some(target) => tairix_browse::resolve_target(&self.folder_path(), target),
                None => {
                    return DesktopOutcome::acting(DesktopAction::Refuse(format!(
                        "desktop: the shortcut '{}' names nothing\n",
                        entry.name()
                    )));
                }
            },
            _ => path.clone(),
        };
        match entry.kind() {
            EntryKind::Directory | EntryKind::Link(LinkTarget::Directory) => {
                DesktopOutcome::acting(DesktopAction::Activate(DesktopActivation::OpenFolder {
                    path,
                }))
            }
            EntryKind::Bundle | EntryKind::Link(LinkTarget::Bundle) => {
                DesktopOutcome::acting(DesktopAction::Activate(launch_of(
                    &bundle_path,
                    bundle_label(leaf_of(&bundle_path)),
                    None,
                )))
            }
            EntryKind::File | EntryKind::Link(LinkTarget::File) => {
                match applications_for(entry.name(), apps).first() {
                    Some(app) => DesktopOutcome::acting(DesktopAction::Activate(launch_of(
                        app.bundle_path(),
                        app.name().to_string(),
                        Some(path),
                    ))),
                    None => DesktopOutcome::acting(DesktopAction::Refuse(format!(
                        "desktop: no installed application opens '{}'\n",
                        entry.name()
                    ))),
                }
            }
            // A shortcut whose target cannot be reached: reported, never
            // launched or opened on the chance that it works.
            EntryKind::Link(LinkTarget::Dangling) => {
                DesktopOutcome::acting(DesktopAction::Refuse(format!(
                    "desktop: the shortcut '{}' points at something that is not there\n",
                    entry.name()
                )))
            }
        }
    }

    /// The absolute path of the child called `name` inside the desktop folder.
    fn path_of(&self, name: &str) -> String {
        let mut path = tairix_browse::vfs::spell_absolute_path(&self.folder);
        tairix_browse::vfs::push_child(&mut path, name);
        path
    }
}

/// One arrow-key move over the icon column.
///
/// The desktop's icons flow *down* a column before wrapping, so up/down is one
/// icon while right/left is one whole column — and "one column" is however
/// many icons the live grid fits, never a number this module guesses.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Step {
    /// One icon further down the listing.
    NextIcon,
    /// One icon back up the listing.
    PreviousIcon,
    /// One whole column later in the listing.
    NextColumn,
    /// One whole column earlier in the listing.
    PreviousColumn,
}

impl Step {
    /// The step an arrow key asks for under the `flow` the icons are arranged
    /// in, or `None` when the key means nothing to the desktop.
    ///
    /// Which horizontal arrow runs *later* into the listing is a property of
    /// the arrangement, not a constant: columns grow rightward from the
    /// leading edge and leftward from the trailing one, so the mapping is read
    /// off the live arrangement. Otherwise one of the two arrangements would
    /// move the selection the opposite way to the icons the user can see.
    const fn for_key(key: NamedKey, flow: IconFlow) -> Option<Self> {
        let rightward_is_later = matches!(flow, IconFlow::Leading);
        match key {
            NamedKey::Down => Some(Self::NextIcon),
            NamedKey::Up => Some(Self::PreviousIcon),
            NamedKey::Right if rightward_is_later => Some(Self::NextColumn),
            NamedKey::Right => Some(Self::PreviousColumn),
            NamedKey::Left if rightward_is_later => Some(Self::PreviousColumn),
            NamedKey::Left => Some(Self::NextColumn),
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

/// The shared grid flow the settings' icon arrangement names.
const fn grid_flow(flow: IconFlow) -> GridFlow {
    match flow {
        IconFlow::Leading => GridFlow::ColumnsFromLeading,
        IconFlow::Trailing => GridFlow::ColumnsFromTrailing,
    }
}

/// The shared listing order the settings' icon sort names.
///
/// The two vocabularies meet in exactly this one function, and the settings
/// engine deliberately does not speak [`SortMode`] itself: that type belongs to
/// the file-browser engine, and a five-line configuration document that every
/// consumer of the user's pinboard store must parse has no business dragging
/// that engine's dependency weight in behind it. Bridging here keeps the
/// desktop ordering its listing through the single shared sort — there is still
/// no second sort — while the store stays a store.
const fn sort_mode(sort: IconSort) -> SortMode {
    let key = match sort {
        IconSort::Name => SortKey::Name,
        IconSort::Kind => SortKey::Kind,
        IconSort::Size => SortKey::Size,
        IconSort::Date => SortKey::Modified,
    };
    SortMode {
        key,
        direction: SortDirection::Ascending,
    }
}

/// The launch activation for the bundle at `bundle`, reported as `label` and
/// optionally handed `argument`. One spelling of "a bundle's entry point is
/// its `Run` binary", so the desktop's three launch paths cannot diverge.
fn launch_of(bundle: &str, label: String, argument: Option<String>) -> DesktopActivation {
    DesktopActivation::Launch {
        run_path: format!("{bundle}{}", crate::apps::BUNDLE_RUN_SUFFIX),
        label,
        argument,
    }
}

/// The final component of the absolute `path` — the name a resolved bundle is
/// reported by. The one shared spelling rule, through the browser engine that
/// already owns this app's path handling.
fn leaf_of(path: &str) -> &str {
    tairix_browse::leaf_name(path)
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
