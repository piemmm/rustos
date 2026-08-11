//! The pinboard's context menu: the desktop's one right-click surface,
//! composed from the shared `lib/controls` menu (`plans/PINBOARD.md` §7).
//!
//! A secondary press on the backdrop — on an icon or on empty space — opens
//! this menu. Its command set is *closed*: the rows are built by one pass that
//! pairs each [`MenuItem`] with the [`PinboardCommand`] it names, so a row
//! index can never disagree with the command it dispatches and reordering the
//! menu cannot silently re-map what a row does.
//!
//! The menu holds no authority whatsoever. It *names* a command; the desktop
//! model resolves that command against its own state
//! ([`Desktop::command`](crate::Desktop::command)) and the session — which
//! holds the filesystem, spawn, and settings-store capabilities — carries the
//! resulting action out. Nothing here reads a directory, writes a store, or
//! launches anything.
//!
//! The plate is anchored at the pointer and clamped wholly onto the screen, so
//! a right-click in the bottom-right corner opens a menu the user can actually
//! reach rather than one that runs off the edge.

use alloc::vec::Vec;

use tairix_controls::{damage, ActivityState, ControlState, Menu, MenuAction, MenuItem};
use tairix_geometry::{Point, Rect, Scale};
use tairix_raster::Surface;
use tairix_theme::Theme;
use tairix_wallpaper::{IconFlow, IconSort, PinboardSettings};
use tairix_wm::{InputEvent, Key};

/// The listing orders the menu offers, in the order it shows them.
const SORTS: [IconSort; 4] = [
    IconSort::Name,
    IconSort::Kind,
    IconSort::Size,
    IconSort::Date,
];

/// The icon arrangements the menu offers, in the order it shows them.
const ARRANGEMENTS: [IconFlow; 2] = [IconFlow::Leading, IconFlow::Trailing];

/// Why the sort order already in force is not offered again.
const REASON_CURRENT_ORDER: &str = "already the listing order";

/// Why the arrangement already in force is not offered again.
const REASON_CURRENT_ARRANGEMENT: &str = "already the arrangement";

/// What choosing a pinboard menu row asks for.
///
/// A closed set: every row of the menu names exactly one of these, and the
/// desktop model is the single place each is turned into a
/// [`DesktopAction`](crate::DesktopAction).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PinboardCommand {
    /// Open the icon the menu was opened on.
    Open,
    /// Create a new folder in the desktop folder.
    NewFolder,
    /// List the desktop folder in this order.
    SortBy(IconSort),
    /// Arrange the desktop icons from this edge.
    ArrangeFrom(IconFlow),
    /// List the desktop folder again now.
    Refresh,
    /// Show the desktop folder in the file manager.
    OpenDesktopFolder,
    /// Open the wallpaper chooser.
    ChangeBackground,
}

/// The outcome of routing one input event into the menu.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PinboardMenuOutcome {
    /// Claimed with no state change.
    Ignored,
    /// Claimed; only pixels changed (the highlighted row moved).
    Changed,
    /// This command was chosen; the menu has closed.
    Chose(PinboardCommand),
    /// The menu was dismissed without choosing (a press away, or Escape); it
    /// has closed.
    Dismissed,
}

/// One row of the menu: what it draws, and the command it names.
struct Row {
    item: MenuItem,
    command: PinboardCommand,
}

impl Row {
    /// Pair `item` with the `command` choosing it asks for.
    fn new(item: MenuItem, command: PinboardCommand) -> Self {
        Self { item, command }
    }
}

/// The desktop's context menu: closed, or open at one anchor.
///
/// While open it is modal exactly like the taskbar's own menu: input routes
/// here first, a press away dismisses without acting on what it hit, and
/// Escape dismisses from the keyboard.
pub struct PinboardMenu {
    /// The pointer position the open menu is anchored at, or `None` when the
    /// menu is closed — the one place "is it open?" is recorded, so a closed
    /// menu cannot carry a stale plate position.
    anchor: Option<Point>,
    menu: Menu,
    /// The command each row names, by row index.
    commands: Vec<PinboardCommand>,
}

impl PinboardMenu {
    /// A closed menu.
    #[must_use]
    pub fn new() -> Self {
        Self {
            anchor: None,
            menu: Menu::new(Vec::new()),
            commands: Vec::new(),
        }
    }

    /// Open the menu at pointer position `at`, offering the rows `settings`
    /// and `on_icon` imply.
    ///
    /// `on_icon` is whether the press landed on an icon, which is the only
    /// thing that decides whether `Open` is offered: a menu opened on empty
    /// backdrop has nothing to open. Opening again re-builds the rows, so the
    /// marked sort and arrangement always show the settings in force.
    pub fn open(&mut self, at: Point, on_icon: bool, settings: &PinboardSettings) {
        let rows = rows_for(on_icon, settings);
        let mut items = Vec::with_capacity(rows.len());
        let mut commands = Vec::with_capacity(rows.len());
        for row in rows {
            items.push(row.item);
            commands.push(row.command);
        }
        self.menu = Menu::new(items);
        self.commands = commands;
        self.anchor = Some(at);
    }

    /// Close the menu, dropping its rows.
    pub fn close(&mut self) {
        self.anchor = None;
        self.menu = Menu::new(Vec::new());
        self.commands.clear();
    }

    /// Whether the menu is open.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.anchor.is_some()
    }

    /// The pointer position the open menu is anchored at, or `None` when it is
    /// closed.
    #[must_use]
    pub const fn anchor(&self) -> Option<Point> {
        self.anchor
    }

    /// The shared menu control behind the surface: its rows and their states.
    #[must_use]
    pub const fn menu(&self) -> &Menu {
        &self.menu
    }

    /// The command the row at `index` names, or `None` for an index the open
    /// menu does not have (fail closed — never guess at a command).
    #[must_use]
    pub fn command_at(&self, index: usize) -> Option<PinboardCommand> {
        self.commands.get(index).copied()
    }

    /// The plate the open menu occupies on `screen`, or `None` when it is
    /// closed.
    ///
    /// The plate takes the size its rows ask for at `scale`, starts at the
    /// anchor, and is then pulled back along each axis so it stays wholly on
    /// screen — a menu opened in a corner moves inward rather than running off
    /// the edge. A closed menu has no plate at all rather than a fabricated
    /// one at the origin.
    #[must_use]
    pub fn layout(&self, screen: Rect, scale: Scale, theme: &Theme) -> Option<Rect> {
        let at = self.anchor?;
        let width = self.menu.preferred_width(scale, theme);
        let height = self.menu.preferred_height(scale, theme);
        Some(Rect::new(
            clamped_edge(screen.origin.x, screen.width, at.x, width),
            clamped_edge(screen.origin.y, screen.height, at.y, height),
            width,
            height,
        ))
    }

    /// Paint the open menu into `surface` at `bounds` through the shared menu
    /// control. A closed menu draws nothing.
    pub fn render(&self, surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
        if !self.is_open() {
            return;
        }
        self.menu.render(surface, bounds, scale, theme);
    }

    /// Route one pointer `event`, with the live `pointer` position, into the
    /// open menu drawn at `bounds`.
    ///
    /// A press outside the plate dismisses without acting on whatever was
    /// under it; a completed click on a row chooses that row's command. A
    /// closed menu claims nothing.
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        pointer: Point,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
    ) -> PinboardMenuOutcome {
        if !self.is_open() {
            return PinboardMenuOutcome::Ignored;
        }
        if matches!(event, InputEvent::PointerPressed { .. }) && !bounds.contains(pointer) {
            self.close();
            return PinboardMenuOutcome::Dismissed;
        }
        let before = self.menu.current();
        let acted = self
            .menu
            .on_pointer(event, bounds, scale, theme, &mut damage::sink());
        self.resolve(acted, before)
    }

    /// Route one `key` into the open menu drawn at `bounds`: the arrows and
    /// Home/End move the highlighted row, Enter chooses it, and Escape
    /// dismisses. A closed menu claims nothing.
    pub fn on_key(
        &mut self,
        key: Key,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
    ) -> PinboardMenuOutcome {
        if !self.is_open() {
            return PinboardMenuOutcome::Ignored;
        }
        let before = self.menu.current();
        let acted = self
            .menu
            .on_key(key, bounds, scale, theme, &mut damage::sink());
        self.resolve(acted, before)
    }

    /// Turn what the shared menu did into this surface's own outcome, closing
    /// the menu on anything that ends it.
    ///
    /// `before` is the highlighted row as it was, so an event that only moved
    /// the highlight reports a repaint and one that moved nothing costs
    /// nothing.
    fn resolve(&mut self, acted: Option<MenuAction>, before: Option<usize>) -> PinboardMenuOutcome {
        match acted {
            Some(MenuAction::Activated { index }) => {
                let chosen = self.command_at(index);
                self.close();
                match chosen {
                    Some(command) => PinboardMenuOutcome::Chose(command),
                    // A row the command list does not name could only come
                    // from a row built without one; dismissing rather than
                    // guessing keeps the surface closed.
                    None => PinboardMenuOutcome::Dismissed,
                }
            }
            // No row of this menu owns a submenu, so there is nothing to open.
            Some(MenuAction::OpenSubmenu { .. }) => PinboardMenuOutcome::Ignored,
            Some(MenuAction::Dismissed) => {
                self.close();
                PinboardMenuOutcome::Dismissed
            }
            None if self.menu.current() == before => PinboardMenuOutcome::Ignored,
            None => PinboardMenuOutcome::Changed,
        }
    }
}

impl Default for PinboardMenu {
    fn default() -> Self {
        Self::new()
    }
}

/// The menu's rows, in the order they are shown, each paired with the command
/// it names.
///
/// One pass builds both halves of every row, so the row list and the command
/// list are the same list by construction — a row can never dispatch a command
/// belonging to its neighbour. `Open` is offered only when the press landed on
/// an icon; the sort order and arrangement in force are shown marked and
/// non-actionable, because choosing what is already in force is not a command
/// but a statement of where the desktop already is.
fn rows_for(on_icon: bool, settings: &PinboardSettings) -> Vec<Row> {
    let mut rows = Vec::new();
    if on_icon {
        rows.push(Row::new(MenuItem::new("Open"), PinboardCommand::Open));
    }
    rows.push(Row::new(
        // The first row of the plate never opens a group: with no `Open` above
        // it there is nothing to divide it from.
        MenuItem::new("New Folder").with_group_break(on_icon),
        PinboardCommand::NewFolder,
    ));
    for (position, sort) in SORTS.into_iter().enumerate() {
        let item = MenuItem::new(sort_label(sort)).with_group_break(position == 0);
        rows.push(Row::new(
            mark_current(item, sort == settings.sort, REASON_CURRENT_ORDER),
            PinboardCommand::SortBy(sort),
        ));
    }
    for (position, flow) in ARRANGEMENTS.into_iter().enumerate() {
        let item = MenuItem::new(arrangement_label(flow)).with_group_break(position == 0);
        rows.push(Row::new(
            mark_current(item, flow == settings.icons, REASON_CURRENT_ARRANGEMENT),
            PinboardCommand::ArrangeFrom(flow),
        ));
    }
    rows.push(Row::new(
        MenuItem::new("Refresh").with_group_break(true),
        PinboardCommand::Refresh,
    ));
    rows.push(Row::new(
        MenuItem::new("Open Desktop Folder"),
        PinboardCommand::OpenDesktopFolder,
    ));
    rows.push(Row::new(
        MenuItem::new("Change Background…"),
        PinboardCommand::ChangeBackground,
    ));
    rows
}

/// `item` marked as the setting already in force when `current`, with `reason`
/// stated; otherwise `item` unchanged.
///
/// The mark is the shared control vocabulary's completed-activity bead, which
/// is what draws a menu row as chosen, and the row is non-actionable so it
/// reports what the desktop is doing rather than offering a command that would
/// change nothing.
fn mark_current(item: MenuItem, current: bool, reason: &'static str) -> MenuItem {
    if !current {
        return item;
    }
    item.with_state(ControlState::disabled().with_activity(ActivityState::Complete))
        .with_reason(reason)
}

/// The label of the row that asks for `sort`.
const fn sort_label(sort: IconSort) -> &'static str {
    match sort {
        IconSort::Name => "Sort by Name",
        IconSort::Kind => "Sort by Kind",
        IconSort::Size => "Sort by Size",
        IconSort::Date => "Sort by Date",
    }
}

/// The label of the row that asks for `flow`.
///
/// The rows are worded as the *directions on screen* a user sees, not as the
/// settings vocabulary's leading/trailing edges, because a menu is read by
/// someone looking at their icons.
const fn arrangement_label(flow: IconFlow) -> &'static str {
    match flow {
        IconFlow::Leading => "Arrange from the Left",
        IconFlow::Trailing => "Arrange from the Right",
    }
}

/// One axis of the on-screen clamp: a plate of `side` pixels starting at `at`,
/// pulled back so its far edge stays inside the `extent` pixels from `origin`,
/// and never started before `origin` itself.
///
/// Pinning to the leading edge when the plate is larger than the screen keeps
/// the rule total: the menu's first rows stay reachable rather than the plate
/// being pushed off the far side.
fn clamped_edge(origin: i32, extent: u32, at: i32, side: u32) -> i32 {
    let limit = origin
        .saturating_add_unsigned(extent)
        .saturating_sub_unsigned(side);
    at.min(limit).max(origin)
}
