//! The program-library popup: the folder-organised application launcher the
//! taskbar's Library button opens (`plans/NEW-TASKBAR.md` T5).
//!
//! [`LibraryPopup`] is a pure model over the **resolved** program-library
//! [`Catalog`] the session hands it — the machine store merged with the
//! user's overlay through `tairix_proglib::merge`. The popup never touches
//! the VFS and holds no authority: selecting an entry only *reports* a
//! launch outcome carrying the entry's identifier (surfaced as
//! [`TaskbarResponse::LibraryLaunch`](crate::TaskbarResponse::LibraryLaunch)),
//! and the session glue (which holds the spawn capability) resolves the
//! bundle and launches it through the ordinary signature-checked load gate.
//!
//! The surface is composed from the shared Reactive Alloy vocabulary
//! (`lib/controls`): a [`Panel`] anchored at the Library button, a
//! [`SearchField`] filter, one list row per folder or entry, and a
//! [`ScrollBar`] when the rows overflow the viewport. Folders are the closed
//! [`LibraryCategory`] taxonomy in its canonical presentation order; a folder
//! with no entries is never shown, and an empty library renders a calm
//! placeholder rather than an error.
//!
//! # Interaction model
//!
//! While open the popup is modal: the desktop routes every pointer, scroll,
//! and key event here. Two focus fields cycle with `Tab` — the search field
//! and the row list — and every action is reachable from the keyboard:
//!
//! * `Up`/`Down` move the row cursor (wrapping), `Home`/`End` jump,
//!   `PageUp`/`PageDown` move by a viewport.
//! * `Enter` (or space) activates the cursor row: a folder toggles its
//!   expansion, an entry launches.
//! * `Left` collapses the cursor folder (or climbs from an entry to its
//!   folder); `Right` expands a collapsed folder or steps into its first
//!   entry.
//! * Typing anywhere routes into the search field; `Enter` in the search
//!   launches the first match; `Escape` clears a non-empty search, and
//!   dismisses the popup once the search is empty.
//!
//! Everything fails closed: a press on no row changes nothing, a launch is
//! only ever reported for a row that exists, an offer whose row has gone is
//! abandoned rather than guessed at, and degenerate geometry (a screen too
//! small for even one row) renders chrome with an empty viewport rather than
//! panicking.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_controls::{
    ControlRole, ControlState, FocusState, ListRow, Panel, PointerState, ScrollBar, ScrollModel,
    ScrollOrientation, ScrollRange, SearchField, TextAction,
};
use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Region, Scale};
use tairix_input::{InputEvent, Key, Modifiers, NamedKey, PointerButton};
use tairix_proglib::{Catalog, EntryId, LibraryCategory};
use tairix_raster::Surface;
use tairix_theme::{TextRole, Theme};

use crate::edge::Edge;
use crate::layout::BarLayout;

/// Width of the popup panel, in *logical* pixels at the reference density.
const POPUP_WIDTH: u32 = 280;

/// Leading indent of an entry row beneath its folder header, in *logical*
/// pixels, so the two-level structure reads at a glance.
const ENTRY_INDENT: u32 = 20;

/// Viewport rows reserved for the calm placeholder when nothing is listed,
/// so the message has room to breathe without opening a tall empty panel.
const PLACEHOLDER_ROWS: u32 = 3;

/// The popup's window title.
const POPUP_TITLE: &str = "Programs";

/// The human-readable name of a library folder.
///
/// The [`LibraryCategory`] identifiers are locale-neutral store spellings
/// (`SystemTools`); this is the display spelling a launcher shows. It lives
/// beside the popup — its only consumer — rather than in the ABI taxonomy.
#[must_use]
pub fn folder_label(category: LibraryCategory) -> &'static str {
    match category {
        LibraryCategory::Accessories => "Accessories",
        LibraryCategory::Graphics => "Graphics",
        LibraryCategory::Internet => "Internet",
        LibraryCategory::Multimedia => "Multimedia",
        LibraryCategory::Office => "Office",
        LibraryCategory::Programming => "Programming",
        LibraryCategory::Games => "Games",
        LibraryCategory::SystemTools => "System Tools",
        LibraryCategory::Utilities => "Utilities",
        LibraryCategory::Other => "Other",
    }
}

/// One shown row's request for owner-supplied icon artwork: the row's index
/// into [`LibraryPopup::rows`], the pixel side its icon draws at, and the
/// catalog entry to resolve the artwork from.
///
/// The popup reports these for the entry rows it actually shows so the
/// session (which holds the filesystem and decode capabilities the bar does
/// not) resolves each row's icon and hands it back with
/// [`LibraryPopup::set_row_artwork`] — the same render/resolve split the
/// application strip uses. Folder rows raise no request: they draw their
/// built-in folder glyph.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LibraryIconRequest {
    /// The row's index into [`LibraryPopup::rows`].
    pub row: usize,
    /// The pixel side the row draws its icon at, so the owner rasterises at
    /// exactly the size the list row will place.
    pub side: u32,
    /// The catalog entry the row launches, whose own icon the artwork comes
    /// from.
    pub entry: EntryId,
}

/// One row of the popup's scrolling list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LibraryRow {
    /// A folder header; activating it toggles the folder's expansion.
    Folder {
        /// The folder this header opens.
        category: LibraryCategory,
        /// Whether the folder's entries are listed beneath it.
        expanded: bool,
        /// How many entries the folder holds (its trailing count caption).
        count: usize,
    },
    /// A launchable application entry.
    Entry {
        /// The catalog identifier the session launches by.
        id: EntryId,
        /// The display name the row shows.
        name: String,
    },
}

/// Which of the popup's focus fields owns the keyboard.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum LibraryFocus {
    /// The search field: printable keys edit the filter.
    Search,
    /// The row list: arrows move the cursor, `Enter` activates.
    Rows,
}

/// What one routed event did to the open popup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PopupOutcome {
    /// Nothing changed.
    Ignored,
    /// Only the popup's own state changed (a hover, a scroll, an edit); the
    /// embedder repaints and nothing else.
    Changed,
    /// The user chose the entry with this identifier; the popup has closed.
    Launch(EntryId),
    /// The user dismissed the popup (click-away or `Escape`); it has closed.
    Dismiss,
}

/// The computed geometry of the open popup, in screen space.
///
/// Like the bar itself the popup is a *rectangular* buffer the window
/// manager places and rounds; the panel chrome, search row, row viewport,
/// and scrollbar gutter are laid out here so the paint and hit-test paths
/// agree by construction. All arithmetic saturates: a pathological screen
/// or scale yields clipped, possibly empty regions rather than wrapping.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LibraryLayout {
    /// The whole popup panel.
    pub panel: Rect,
    /// The corner radius the window manager applies to the popup — the same
    /// radius the panel chrome itself is drawn with, so the two never
    /// disagree.
    pub corner_radius: u32,
    /// The screen point the panel's anchor notch points back at (the centre
    /// of the Library button).
    pub anchor: Point,
    /// The search field's row.
    pub search: Rect,
    /// The row viewport (excludes the scrollbar gutter).
    pub viewport: Rect,
    /// The vertical scrollbar, when the rows overflow the viewport.
    pub scrollbar: Option<Rect>,
    /// The visible rows: each row's index into [`LibraryPopup::rows`] and
    /// its screen rectangle (entry rows are indented beneath their folder).
    pub rows: Vec<(usize, Rect)>,
    /// How many whole rows the viewport holds.
    pub visible_rows: usize,
}

impl LibraryLayout {
    /// The index (into [`LibraryPopup::rows`]) of the row under `point`, or
    /// `None` for a point outside every visible row.
    #[must_use]
    pub fn row_at(&self, point: Point) -> Option<usize> {
        self.rows
            .iter()
            .find(|(_, rect)| rect.contains(point))
            .map(|&(index, _)| index)
    }
}

/// The program-library popup model. See the [module docs](self).
#[derive(Clone, Debug)]
pub struct LibraryPopup {
    open: bool,
    catalog: Catalog,
    /// Folders the user collapsed in this showing; every folder opens
    /// expanded, so one click (or `Enter`) reaches any entry.
    collapsed: Vec<LibraryCategory>,
    search: SearchField,
    scroll: ScrollBar,
    /// The entry row a primary press landed on, remembered so a release over
    /// the same row is a plain click that launches it.
    pressed: Option<usize>,
    rows: Vec<LibraryRow>,
    /// The owner-supplied icon artwork for each row, positionally aligned to
    /// [`Self::rows`] and reset (to all-`None`) whenever the rows are
    /// rebuilt, so a stale index can never draw the wrong application's
    /// icon. `None` for a row falls back to the list row's built-in glyph.
    row_artwork: Vec<Option<Surface>>,
    current: Option<usize>,
    hover: Option<usize>,
    focus: LibraryFocus,
}

impl Default for LibraryPopup {
    fn default() -> Self {
        Self::new()
    }
}

impl LibraryPopup {
    /// A closed popup over an empty catalog.
    #[must_use]
    pub fn new() -> Self {
        Self {
            open: false,
            catalog: Catalog::default(),
            collapsed: Vec::new(),
            search: SearchField::new().with_placeholder("Search programs"),
            scroll: ScrollBar::new(
                ScrollOrientation::Vertical,
                ScrollModel::new(ScrollRange::EMPTY, 1, 1),
            ),
            pressed: None,
            rows: Vec::new(),
            row_artwork: Vec::new(),
            current: None,
            hover: None,
            focus: LibraryFocus::Search,
        }
    }

    /// Whether the popup is showing. While `true` the desktop treats it as
    /// modal and routes every pointer, scroll, and key event to it.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open
    }

    /// The resolved catalog the popup lists.
    #[must_use]
    pub const fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// Adopt the resolved catalog `catalog` (the machine store merged with
    /// the user's overlay), rebuilding the rows in place.
    pub fn set_catalog(&mut self, catalog: Catalog) {
        self.catalog = catalog;
        self.collapsed.clear();
        self.rebuild();
    }

    /// The current rows, in presentation order.
    #[must_use]
    pub fn rows(&self) -> &[LibraryRow] {
        &self.rows
    }

    /// Which shown rows want owner-supplied icon artwork, and at what size.
    ///
    /// One request per *visible* launchable entry row (folders are omitted —
    /// they draw their own folder glyph), each carrying the row's index, the
    /// pixel side its icon draws at, and the entry to resolve the artwork
    /// from. The session resolves each and hands the result back with
    /// [`set_row_artwork`](Self::set_row_artwork). Only the rows `layout`
    /// shows are reported, so opening a large library never asks the session
    /// to decode an icon nobody sees.
    #[must_use]
    pub fn visible_icon_requests(
        &self,
        layout: &LibraryLayout,
        scale: Scale,
        theme: &Theme,
    ) -> Vec<LibraryIconRequest> {
        layout
            .rows
            .iter()
            .filter_map(|&(index, rect)| {
                let row = self.rows.get(index)?;
                let LibraryRow::Entry { id, .. } = row else {
                    return None;
                };
                let side = list_row(row, false, false, false).icon_side(rect, scale, theme);
                Some(LibraryIconRequest {
                    row: index,
                    side,
                    entry: id.clone(),
                })
            })
            .collect()
    }

    /// Set the owner-resolved icon artwork for the row at `index` (an index
    /// into [`rows`](Self::rows)), or clear it with `None`.
    ///
    /// Out-of-range indices are ignored, so a request resolved against a
    /// since-rebuilt row list is a no-op rather than a panic.
    pub fn set_row_artwork(&mut self, index: usize, artwork: Option<Surface>) {
        if let Some(slot) = self.row_artwork.get_mut(index) {
            *slot = artwork;
        }
    }

    /// The owner-resolved icon artwork for the row at `index`, if any — what
    /// [`render`](crate::TaskbarRenderer::render_library) blits in place of
    /// the row's built-in glyph.
    #[must_use]
    pub fn row_artwork(&self, index: usize) -> Option<&Surface> {
        self.row_artwork.get(index).and_then(Option::as_ref)
    }

    /// The row the keyboard cursor rests on, as an index into
    /// [`rows`](Self::rows).
    #[must_use]
    pub const fn current(&self) -> Option<usize> {
        self.current
    }

    /// The row under the pointer, as an index into [`rows`](Self::rows).
    #[must_use]
    pub const fn hover(&self) -> Option<usize> {
        self.hover
    }

    /// Which focus field owns the keyboard.
    #[must_use]
    pub const fn focus(&self) -> LibraryFocus {
        self.focus
    }

    /// The search filter's current text.
    #[must_use]
    pub fn search_text(&self) -> &str {
        self.search.text()
    }

    /// The search field, for painting.
    #[must_use]
    pub const fn search_field(&self) -> &SearchField {
        &self.search
    }

    /// The scrollbar, for painting.
    #[must_use]
    pub const fn scrollbar(&self) -> &ScrollBar {
        &self.scroll
    }

    /// The calm placeholder shown when no row is listed, or `None` while
    /// rows exist. An empty catalog and a filter matching nothing are
    /// ordinary states of the world, worded apart so the user knows which
    /// one they are looking at.
    #[must_use]
    pub fn placeholder(&self) -> Option<&'static str> {
        if !self.rows.is_empty() {
            return None;
        }
        if self.search.has_query() {
            Some("No matching programs")
        } else {
            Some("No programs are catalogued")
        }
    }

    /// Open the popup afresh: search cleared, every folder expanded, cursor
    /// and scroll at the top, keyboard on the search field. A deterministic
    /// opening state means the same catalog always presents the same way.
    pub(crate) fn open(&mut self) {
        self.open = true;
        self.search.set_text("");
        self.search.set_focused(true);
        self.collapsed.clear();
        self.focus = LibraryFocus::Search;
        self.current = None;
        self.hover = None;
        self.rebuild();
        self.scroll_to(0);
    }

    /// Close the popup, keeping the catalog.
    pub(crate) fn close(&mut self) {
        self.open = false;
        self.hover = None;
    }

    /// Compute the popup's geometry opened from `library_button` on the bar
    /// laid out as `bar` pinned to `edge`, at the desktop `scale` under
    /// `theme`.
    ///
    /// The panel opens *outward* from the bar — above a bottom bar, below a
    /// top bar, to the inner side of a left/right bar — aligned to the
    /// Library button and clamped to the screen. Its height is sized to the
    /// rows it has, capped by the space between the bar and the opposite
    /// screen edge; overflowing rows scroll.
    #[must_use]
    pub fn layout(
        &self,
        edge: Edge,
        bar: &BarLayout,
        screen_width: u32,
        screen_height: u32,
        scale: Scale,
        theme: &Theme,
    ) -> LibraryLayout {
        let metrics = theme.metrics();
        let row_height = scale.scale_length(metrics.control_height);
        let pad = scale.scale_length(metrics.control_gap);
        let corner_radius = scale.scale_length(metrics.window_corner_radius);
        let width = scale.scale_length(POPUP_WIDTH).min(screen_width).max(1);
        let indent = scale.scale_length(ENTRY_INDENT);

        let chrome = probe_chrome(&chrome_panel(Point::ORIGIN), width, scale, theme);

        // Space available for the panel on the popup's side of the bar.
        let available = match edge {
            Edge::Bottom => u32::try_from(bar.bar.top()).unwrap_or(0),
            Edge::Top => screen_height.saturating_sub(u32::try_from(bar.bar.bottom()).unwrap_or(0)),
            Edge::Left | Edge::Right => screen_height,
        };

        let fixed = chrome
            .overhead
            .saturating_add(pad)
            .saturating_add(row_height) // the search row
            .saturating_add(pad)
            .saturating_add(pad);
        let max_rows = available.saturating_sub(fixed) / row_height.max(1);
        let wanted = u32::try_from(self.rows.len()).unwrap_or(u32::MAX);
        let viewport_rows = if self.rows.is_empty() {
            PLACEHOLDER_ROWS.min(max_rows)
        } else {
            wanted.min(max_rows)
        };
        let viewport_height = viewport_rows.saturating_mul(row_height);
        let panel_height = fixed.saturating_add(viewport_height);

        let library = bar.library;
        let panel_origin = panel_origin(edge, bar.bar, library, width, panel_height);
        let panel = Rect::new(panel_origin.x, panel_origin.y, width, panel_height);
        let anchor = Point::new(
            library.left().saturating_add(to_i32(library.width / 2)),
            library.top().saturating_add(to_i32(library.height / 2)),
        );

        let inner_x = panel
            .left()
            .saturating_add(chrome.content_left)
            .saturating_add(to_i32(pad));
        let inner_width = chrome.content_width.saturating_sub(pad.saturating_mul(2));
        let search = Rect::new(
            inner_x,
            panel
                .top()
                .saturating_add(chrome.content_top)
                .saturating_add(to_i32(pad)),
            inner_width,
            row_height,
        );

        let visible = viewport_rows as usize;
        let scrollable = self.rows.len() > visible;
        let gutter = if scrollable {
            scale
                .scale_length(metrics.scrollbar_breadth)
                .saturating_add(pad)
        } else {
            0
        };
        let viewport = Rect::new(
            inner_x,
            search.bottom().saturating_add(to_i32(pad)),
            inner_width.saturating_sub(gutter),
            viewport_height,
        );
        let scrollbar = scrollable.then(|| {
            Rect::new(
                inner_x.saturating_add(to_i32(inner_width.saturating_sub(gutter))) + to_i32(pad),
                viewport.top(),
                scale.scale_length(metrics.scrollbar_breadth),
                viewport_height,
            )
        });

        let rows = self.stack_rows(viewport, row_height, indent, visible);

        LibraryLayout {
            panel,
            corner_radius,
            anchor,
            search,
            viewport,
            scrollbar,
            rows,
            visible_rows: visible,
        }
    }

    /// Stack the visible rows down `viewport`, one `row_height` slot each,
    /// indenting an entry row by `indent` beneath its folder header (never
    /// under a flat search filter).
    fn stack_rows(
        &self,
        viewport: Rect,
        row_height: u32,
        indent: u32,
        visible: usize,
    ) -> Vec<(usize, Rect)> {
        let first = self.first_visible(visible);
        let mut rows = Vec::with_capacity(visible.min(self.rows.len()));
        for (slot, index) in (first..self.rows.len()).take(visible).enumerate() {
            let top = viewport
                .top()
                .saturating_add(to_i32(row_height.saturating_mul(to_u32(slot))));
            let inset = match self.rows[index] {
                LibraryRow::Entry { .. } if !self.search.has_query() => indent,
                _ => 0,
            };
            rows.push((
                index,
                Rect::new(
                    viewport.left().saturating_add(to_i32(inset)),
                    top,
                    viewport.width.saturating_sub(inset),
                    row_height,
                ),
            ));
        }
        rows
    }

    /// Route one pointer `event` (tracked at `point`) into the open popup.
    pub(crate) fn route_pointer(
        &mut self,
        event: &InputEvent,
        point: Point,
        layout: &LibraryLayout,
        theme: &Theme,
        scale: Scale,
        damage: &mut Region,
    ) -> PopupOutcome {
        self.sync_scroll(layout);
        // The search field and scrollbar track the pointer themselves; feed
        // them every event so a caret drag or thumb drag follows motion.
        let mut changed = self
            .search
            .on_pointer(event, layout.search, scale, theme, damage)
            .is_some();
        if let Some(action) = pointer_scroll(&mut self.scroll, event, layout, scale, theme, damage)
        {
            self.scroll_to(action);
            changed = true;
        }

        match *event {
            InputEvent::PointerMoved { .. } => {
                let hover = layout.row_at(point);
                if hover != self.hover {
                    self.hover = hover;
                    changed = true;
                }
                changed_outcome(changed)
            }
            InputEvent::PointerPressed {
                button: PointerButton::Primary,
            } => {
                self.pressed = None;
                if layout.search.contains(point) {
                    self.focus_search();
                    return PopupOutcome::Changed;
                }
                if layout
                    .scrollbar
                    .is_some_and(|scrollbar| scrollbar.contains(point))
                {
                    return changed_outcome(changed);
                }
                if let Some(index) = layout.row_at(point) {
                    // A folder header toggles on the press; an entry row
                    // launches on the release that ends the press, so a
                    // press-and-move-away never launches the wrong thing.
                    if self.entry_at(index).is_none() {
                        return self.activate(index, layout.visible_rows);
                    }
                    self.pressed = Some(index);
                    self.current = Some(index);
                    self.focus = LibraryFocus::Rows;
                    return PopupOutcome::Changed;
                }
                if layout.panel.contains(point) {
                    return changed_outcome(changed);
                }
                self.close();
                PopupOutcome::Dismiss
            }
            InputEvent::PointerPressed {
                button: PointerButton::Secondary | PointerButton::Middle,
            } => {
                if layout.panel.contains(point) {
                    changed_outcome(changed)
                } else {
                    self.close();
                    PopupOutcome::Dismiss
                }
            }
            InputEvent::PointerReleased {
                button: PointerButton::Primary,
            } => {
                match self
                    .pressed
                    .take()
                    .filter(|&row| layout.row_at(point) == Some(row))
                {
                    Some(row) => self.activate(row, layout.visible_rows),
                    None => changed_outcome(changed),
                }
            }
            InputEvent::PointerReleased { .. } => changed_outcome(changed),
            InputEvent::PointerScrolled { dx, dy } => {
                // A popup whose rows fit lays out no bar and cannot scroll,
                // so there is nothing to feed the wheel to.
                if let Some(offset) = layout
                    .scrollbar
                    .and_then(|bounds| self.scroll.wheel(dx, dy, bounds, damage))
                    .map(scroll_offset)
                {
                    self.scroll_to(offset);
                    return PopupOutcome::Changed;
                }
                changed_outcome(changed)
            }
            InputEvent::KeyPressed { .. } | InputEvent::KeyReleased { .. } => {
                changed_outcome(changed)
            }
        }
    }

    /// Route one key press into the open popup.
    pub(crate) fn route_key(
        &mut self,
        key: Key,
        modifiers: Modifiers,
        layout: &LibraryLayout,
        damage: &mut Region,
    ) -> PopupOutcome {
        self.sync_scroll(layout);
        if key == Key::Named(NamedKey::Tab) {
            return self.toggle_focus(layout.visible_rows);
        }
        match self.focus {
            LibraryFocus::Search => self.key_in_search(key, modifiers, layout, damage),
            LibraryFocus::Rows => self.key_in_rows(key, modifiers, layout, damage),
        }
    }

    /// Apply one key press while the search field owns the keyboard.
    fn key_in_search(
        &mut self,
        key: Key,
        modifiers: Modifiers,
        layout: &LibraryLayout,
        damage: &mut Region,
    ) -> PopupOutcome {
        match key {
            Key::Named(NamedKey::Down) => {
                self.focus_rows(layout.visible_rows);
                PopupOutcome::Changed
            }
            Key::Named(NamedKey::Enter) => self.launch_first_entry(layout.visible_rows),
            Key::Named(NamedKey::Escape) if !self.search.has_query() => {
                self.close();
                PopupOutcome::Dismiss
            }
            _ => match self.search.on_key(key, modifiers, layout.search, damage) {
                Some(TextAction::Edited) => {
                    self.rebuild();
                    self.current = None;
                    self.scroll_to(0);
                    PopupOutcome::Changed
                }
                // `Submitted` is the Enter arm above; a `Cancelled` cannot
                // reach here (an empty search dismissed already). Any other
                // handled key repaints the caret or selection.
                Some(_) => PopupOutcome::Changed,
                None => PopupOutcome::Ignored,
            },
        }
    }

    /// Apply one key press while the row list owns the keyboard.
    fn key_in_rows(
        &mut self,
        key: Key,
        modifiers: Modifiers,
        layout: &LibraryLayout,
        damage: &mut Region,
    ) -> PopupOutcome {
        let visible_rows = layout.visible_rows;
        match key {
            Key::Named(NamedKey::Escape) => {
                self.close();
                PopupOutcome::Dismiss
            }
            Key::Named(NamedKey::Up) => self.step(-1, visible_rows),
            Key::Named(NamedKey::Down) => self.step(1, visible_rows),
            Key::Named(NamedKey::Home) => self.jump_to(0, visible_rows),
            Key::Named(NamedKey::End) => {
                self.jump_to(self.rows.len().saturating_sub(1), visible_rows)
            }
            Key::Named(NamedKey::PageUp) => {
                let target = self
                    .current
                    .unwrap_or(0)
                    .saturating_sub(visible_rows.max(1));
                self.jump_to(target, visible_rows)
            }
            Key::Named(NamedKey::PageDown) => {
                let target = self
                    .current
                    .unwrap_or(0)
                    .saturating_add(visible_rows.max(1))
                    .min(self.rows.len().saturating_sub(1));
                self.jump_to(target, visible_rows)
            }
            Key::Named(NamedKey::Enter) | Key::Char(' ') => match self.current {
                Some(index) => self.activate(index, visible_rows),
                None => PopupOutcome::Ignored,
            },
            Key::Named(NamedKey::Left) => self.collapse_current(visible_rows),
            Key::Named(NamedKey::Right) => self.expand_current(visible_rows),
            // Any other key routes into the search field — type-to-filter —
            // moving the keyboard there so the edit is visible where it
            // happened.
            Key::Char(_) | Key::Named(NamedKey::Backspace) => {
                self.focus_search();
                self.key_in_search(key, modifiers, layout, damage)
            }
            Key::Named(_) => PopupOutcome::Ignored,
        }
    }

    /// The catalog identifier of the *entry* row at `index`; `None` for a
    /// folder header or an index no row holds. The one place a row is turned
    /// into the identity a launch or a shortcut names.
    fn entry_at(&self, index: usize) -> Option<EntryId> {
        match self.rows.get(index)? {
            LibraryRow::Entry { id, .. } => Some(id.clone()),
            LibraryRow::Folder { .. } => None,
        }
    }

    /// Activate the row at `index`: toggle a folder, launch an entry.
    fn activate(&mut self, index: usize, visible_rows: usize) -> PopupOutcome {
        match self.rows.get(index) {
            Some(&LibraryRow::Folder { category, .. }) => {
                self.toggle_folder(category, visible_rows);
                PopupOutcome::Changed
            }
            Some(LibraryRow::Entry { id, .. }) => {
                let id = id.clone();
                self.close();
                PopupOutcome::Launch(id)
            }
            None => PopupOutcome::Ignored,
        }
    }

    /// Launch the first listed entry — what `Enter` in the search field does,
    /// so a typed filter concludes without reaching for the arrows.
    fn launch_first_entry(&mut self, visible_rows: usize) -> PopupOutcome {
        let first = self
            .rows
            .iter()
            .position(|row| matches!(row, LibraryRow::Entry { .. }));
        match first {
            Some(index) => self.activate(index, visible_rows),
            None => PopupOutcome::Ignored,
        }
    }

    /// Collapse the cursor folder, or climb from an entry to its folder.
    fn collapse_current(&mut self, visible_rows: usize) -> PopupOutcome {
        match self.current.and_then(|index| self.rows.get(index)) {
            Some(&LibraryRow::Folder {
                category,
                expanded: true,
                ..
            }) => {
                self.toggle_folder(category, visible_rows);
                PopupOutcome::Changed
            }
            Some(LibraryRow::Entry { .. }) => {
                let index = self.current.unwrap_or(0);
                let folder = self.rows[..index]
                    .iter()
                    .rposition(|row| matches!(row, LibraryRow::Folder { .. }));
                match folder {
                    Some(folder) => self.jump_to(folder, visible_rows),
                    None => PopupOutcome::Ignored,
                }
            }
            _ => PopupOutcome::Ignored,
        }
    }

    /// Expand the cursor folder, or step from an expanded one to its first
    /// entry.
    fn expand_current(&mut self, visible_rows: usize) -> PopupOutcome {
        match self.current.and_then(|index| self.rows.get(index)) {
            Some(&LibraryRow::Folder {
                category, expanded, ..
            }) => {
                if expanded {
                    let next = self.current.unwrap_or(0).saturating_add(1);
                    if matches!(self.rows.get(next), Some(LibraryRow::Entry { .. })) {
                        return self.jump_to(next, visible_rows);
                    }
                    PopupOutcome::Ignored
                } else {
                    self.toggle_folder(category, visible_rows);
                    PopupOutcome::Changed
                }
            }
            _ => PopupOutcome::Ignored,
        }
    }

    /// Toggle `category`'s expansion, keeping the cursor on its header.
    fn toggle_folder(&mut self, category: LibraryCategory, visible_rows: usize) {
        match self.collapsed.iter().position(|&c| c == category) {
            Some(index) => {
                self.collapsed.remove(index);
            }
            None => self.collapsed.push(category),
        }
        self.rebuild();
        let header = self.rows.iter().position(
            |row| matches!(row, LibraryRow::Folder { category: c, .. } if *c == category),
        );
        if let Some(header) = header {
            self.place_cursor(header, visible_rows);
        } else {
            self.current = None;
        }
    }

    /// Move the row cursor by `delta`, wrapping at both ends.
    fn step(&mut self, delta: i32, visible_rows: usize) -> PopupOutcome {
        if self.rows.is_empty() {
            return PopupOutcome::Ignored;
        }
        let len = self.rows.len();
        let next = match self.current {
            None => {
                if delta >= 0 {
                    0
                } else {
                    len - 1
                }
            }
            Some(current) if delta >= 0 => (current + 1) % len,
            Some(current) => current.checked_sub(1).unwrap_or(len - 1),
        };
        self.place_cursor(next, visible_rows);
        PopupOutcome::Changed
    }

    /// Move the row cursor to `index`, clamped to the rows that exist.
    fn jump_to(&mut self, index: usize, visible_rows: usize) -> PopupOutcome {
        if self.rows.is_empty() {
            return PopupOutcome::Ignored;
        }
        self.place_cursor(index.min(self.rows.len() - 1), visible_rows);
        PopupOutcome::Changed
    }

    /// Put the cursor on `index`, take row focus, and scroll it into view.
    fn place_cursor(&mut self, index: usize, visible_rows: usize) {
        self.current = Some(index);
        self.focus = LibraryFocus::Rows;
        self.search.set_focused(false);
        self.scroll.set_focused(true);
        let first = self.first_visible(visible_rows);
        if index < first {
            self.scroll_to(to_u64(index));
        } else if visible_rows > 0 && index >= first + visible_rows {
            self.scroll_to(to_u64(index.saturating_sub(visible_rows - 1)));
        }
    }

    /// `Tab`: cycle the keyboard between the search field and the rows.
    fn toggle_focus(&mut self, visible_rows: usize) -> PopupOutcome {
        match self.focus {
            LibraryFocus::Search => {
                if self.rows.is_empty() {
                    return PopupOutcome::Ignored;
                }
                let target = self.current.unwrap_or(0);
                self.jump_to(target, visible_rows)
            }
            LibraryFocus::Rows => {
                self.focus_search();
                PopupOutcome::Changed
            }
        }
    }

    /// Give the keyboard to the search field.
    fn focus_search(&mut self) {
        self.focus = LibraryFocus::Search;
        self.search.set_focused(true);
        self.scroll.set_focused(false);
    }

    /// Give the keyboard to the rows, placing the cursor if it is unset.
    fn focus_rows(&mut self, visible_rows: usize) {
        if self.rows.is_empty() {
            return;
        }
        let target = self.current.unwrap_or(0);
        self.place_cursor(target, visible_rows);
    }

    /// Rebuild the rows from the catalog and the search filter.
    ///
    /// Without a filter: every non-empty folder in the taxonomy's canonical
    /// order, each expanded folder followed by its entries sorted by display
    /// name. With a filter: the flat, name-sorted list of every entry whose
    /// display name contains the query, case-insensitively.
    fn rebuild(&mut self) {
        self.rows.clear();
        self.hover = None;
        if self.search.has_query() {
            let needle = self.search.text().to_lowercase();
            let mut matches: Vec<_> = self
                .catalog
                .entries()
                .filter(|entry| entry.name().as_str().to_lowercase().contains(&needle))
                .collect();
            matches.sort_by(|a, b| {
                (a.name().as_str(), a.id().as_str()).cmp(&(b.name().as_str(), b.id().as_str()))
            });
            self.rows
                .extend(matches.into_iter().map(|entry| LibraryRow::Entry {
                    id: entry.id().clone(),
                    name: String::from(entry.name().as_str()),
                }));
        } else {
            for category in self.catalog.folders() {
                let entries = self.catalog.folder(category);
                let expanded = !self.collapsed.contains(&category);
                self.rows.push(LibraryRow::Folder {
                    category,
                    expanded,
                    count: entries.len(),
                });
                if expanded {
                    self.rows
                        .extend(entries.into_iter().map(|entry| LibraryRow::Entry {
                            id: entry.id().clone(),
                            name: String::from(entry.name().as_str()),
                        }));
                }
            }
        }
        if let Some(current) = self.current {
            if current >= self.rows.len() {
                self.current = None;
            }
        }
        // The row list changed shape, so any resolved artwork is keyed to the
        // old indices: drop it all and let the session re-resolve the new
        // visible rows. A row with no artwork draws its built-in glyph, so the
        // window between here and the next resolution never blanks.
        self.row_artwork.clear();
        self.row_artwork.resize_with(self.rows.len(), || None);
        // A remembered press is keyed to the old indices too, so it cannot be
        // allowed to launch whatever now sits at that position: a rebuild
        // reachable with the button held (typing into the filter, folding a
        // folder) would otherwise launch a different program than the one
        // pressed.
        self.pressed = None;
    }

    /// The index of the first visible row for a viewport of `visible` rows.
    fn first_visible(&self, visible: usize) -> usize {
        let max_first = self.rows.len().saturating_sub(visible);
        usize::try_from(self.scroll.model().offset())
            .unwrap_or(usize::MAX)
            .min(max_first)
    }

    /// Bring the scrollbar's model in step with the rows and the viewport.
    fn sync_scroll(&mut self, layout: &LibraryLayout) {
        let content = to_u64(self.rows.len());
        let viewport = to_u64(layout.visible_rows);
        let offset = self.scroll.model().offset();
        self.scroll.set_model(ScrollModel::new(
            ScrollRange::new(content, viewport, offset),
            1,
            viewport.max(1),
        ));
    }

    /// Scroll the viewport so the row at `offset` is first, clamped.
    fn scroll_to(&mut self, offset: u64) {
        self.scroll.set_model(self.scroll.model().scroll_to(offset));
    }
}

/// Feed one pointer event to the scrollbar, returning the offset it asked
/// for. Split out so the borrow on the popup's scrollbar ends before the
/// popup applies the offset.
fn pointer_scroll(
    scroll: &mut ScrollBar,
    event: &InputEvent,
    layout: &LibraryLayout,
    scale: Scale,
    theme: &Theme,
    damage: &mut Region,
) -> Option<u64> {
    let bounds = layout.scrollbar?;
    scroll
        .on_pointer(event, bounds, scale, theme, damage)
        .map(scroll_offset)
}

/// The offset a scroll action asks for.
fn scroll_offset(action: tairix_controls::ScrollAction) -> u64 {
    let tairix_controls::ScrollAction::ScrollTo { offset } = action;
    offset
}

/// Collapse a "did the visuals change" flag to a popup outcome.
fn changed_outcome(changed: bool) -> PopupOutcome {
    if changed {
        PopupOutcome::Changed
    } else {
        PopupOutcome::Ignored
    }
}

/// The popup's chrome: the shared panel, anchored back at the invoker. It
/// draws with the bar's theme, so it is the floating ground every surface the
/// bar opens shares — a translucent plate the compositor blurs the desktop
/// behind.
pub(crate) fn chrome_panel(anchor: Point) -> Panel {
    Panel::new(POPUP_TITLE)
        .with_role(ControlRole::Navigation)
        .with_anchor(anchor)
}

/// The panel chrome's measured overhead for one popup width: what the
/// header band and frame consume, and where the content region sits.
///
/// Shared by the program-library popup and the notification popover so both
/// measure the same shared [`Panel`] chrome rather than each re-deriving it.
pub(crate) struct ChromeProbe {
    /// Total vertical pixels the chrome consumes (header plus frame).
    pub(crate) overhead: u32,
    /// The content region's offset from the panel's top edge.
    pub(crate) content_top: i32,
    /// The content region's offset from the panel's left edge.
    pub(crate) content_left: i32,
    /// The content region's width.
    pub(crate) content_width: u32,
}

/// Measure `panel`'s chrome overhead for a popup `width` px wide.
///
/// The overhead is measured by probing the shared [`Panel`] geometry rather
/// than re-deriving its arithmetic, so a metrics change can never drift a
/// popover layout from what the panel actually draws. Taking the panel as a
/// parameter lets the program-library popup and the notification popover
/// share this one measurement.
pub(crate) fn probe_chrome(panel: &Panel, width: u32, scale: Scale, theme: &Theme) -> ChromeProbe {
    let probe_height = 4096;
    let probe = panel.content_rect(Rect::new(0, 0, width, probe_height), scale, theme);
    match probe {
        Some(content) => ChromeProbe {
            overhead: probe_height.saturating_sub(content.height),
            content_top: content.top(),
            content_left: content.left(),
            content_width: content.width,
        },
        None => ChromeProbe {
            overhead: 0,
            content_top: 0,
            content_left: 0,
            content_width: width,
        },
    }
}

/// The row/search font: ordinary interface text at the desktop density.
pub(crate) fn popup_font(theme: &Theme, scale: Scale) -> BitmapFont {
    BitmapFont::for_role(theme.fonts(), TextRole::Body, scale)
}

/// The list row for `row`, styled for its cursor/hover state.
#[must_use]
pub(crate) fn list_row(row: &LibraryRow, current: bool, hovered: bool, row_focus: bool) -> ListRow {
    let (label, icon, trailing) = match row {
        LibraryRow::Folder {
            category,
            expanded,
            count,
        } => (
            String::from(folder_label(*category)),
            if *expanded {
                tairix_icon::IconKind::FolderOpen
            } else {
                tairix_icon::IconKind::Folder
            },
            Some(alloc::format!("{count}")),
        ),
        LibraryRow::Entry { name, .. } => (name.clone(), tairix_icon::IconKind::AppBundle, None),
    };
    let mut state = ControlState::idle();
    if hovered {
        state = state.with_pointer(PointerState::Hover);
    }
    if current && row_focus {
        state = state.with_focus(FocusState::FOCUSED);
    }
    let mut listed = ListRow::new(label).with_icon(icon).with_state(state);
    if let Some(trailing) = trailing {
        listed = listed.with_trailing(trailing);
    }
    if current {
        listed.set_selected(true);
    }
    listed
}

/// The panel origin for a popover of `width` × `height` opened outward from
/// `bar` (pinned to `edge`) and aligned to the `anchor` rectangle it invokes
/// from. Shared by the program-library popup (anchored at the Library button)
/// and the notification popover (anchored at the notification/clock region)
/// so the opening geometry is defined once.
///
/// Along the bar's length the popover is clamped to the *bar*, not to the
/// screen: the bar floats clear of the screen edges it faces, and a popover
/// running on to a screen edge would fill the wallpaper gap the bar leaves.
/// A popover longer than the bar starts at the bar's leading end rather than
/// hanging off its trailing one.
pub(crate) fn panel_origin(edge: Edge, bar: Rect, anchor: Rect, width: u32, height: u32) -> Point {
    let along = |value: i32, start: i32, end: i32, extent: u32| -> i32 {
        let last = end.saturating_sub(to_i32(extent));
        value.clamp(start, last.max(start))
    };
    match edge {
        Edge::Bottom => Point::new(
            along(anchor.left(), bar.left(), bar.right(), width),
            bar.top().saturating_sub(to_i32(height)).max(0),
        ),
        Edge::Top => Point::new(
            along(anchor.left(), bar.left(), bar.right(), width),
            bar.bottom(),
        ),
        Edge::Left => Point::new(
            bar.right(),
            along(anchor.top(), bar.top(), bar.bottom(), height),
        ),
        Edge::Right => Point::new(
            bar.left().saturating_sub(to_i32(width)).max(0),
            along(anchor.top(), bar.top(), bar.bottom(), height),
        ),
    }
}

/// Saturating `u32` → `i32`.
fn to_i32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// Saturating `usize` → `u32`.
fn to_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// Lossless `usize` → `u64` on every supported target.
fn to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}
