//! The backdrop menu's row model: what the desktop's own right-click offers,
//! and how a chosen row is read back (`plans/PINBOARD.md` §7).
//!
//! The desktop describes and the desktop's own menu service decides. This
//! module builds a [`ChainModel`] and reads a chosen row back through
//! [`PinboardCommand::from_item`]; the plate, the band, the placement, the
//! grab and the dismissal are the chain's (`plans/NEW-MENUS.md`), and nothing
//! here draws a menu pixel.
//!
//! Rows are built from one ordered [`PinboardCommand::ALL`] list and read back
//! against that same list, so a reordering cannot re-map what a row does and
//! the rows a gesture actually offers — `Open` only over an icon — cannot
//! shift the meaning of the rows around them.
//!
//! The menu holds no authority whatsoever. It *names* a command; the desktop
//! model resolves that command against its own state
//! ([`Desktop::command`](crate::Desktop::command)) and the session — which
//! holds the filesystem, spawn, and settings-store capabilities — carries the
//! resulting action out. Nothing here reads a directory, writes a store, or
//! launches anything.

use tairix_abi::window_ipc::AppMenuItemId;
use tairix_controls::{ChainModel, ChainRow, ControlState, MenuItem, MenuMark};
use tairix_wallpaper::{IconFlow, IconSort, PinboardSettings};

/// The root plate's title: the surface the menu acts on, named as the user
/// knows it.
const PINBOARD_TITLE: &str = "Desktop";

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

impl PinboardCommand {
    /// Every command, in the order the menu lists them.
    ///
    /// A row's id is its position here, so the list fixes both the order the
    /// plate shows and the id an answer names — and a gesture that leaves
    /// `Open` out shifts neither.
    pub const ALL: [Self; 11] = [
        Self::Open,
        Self::NewFolder,
        Self::SortBy(IconSort::Name),
        Self::SortBy(IconSort::Kind),
        Self::SortBy(IconSort::Size),
        Self::SortBy(IconSort::Date),
        Self::ArrangeFrom(IconFlow::Leading),
        Self::ArrangeFrom(IconFlow::Trailing),
        Self::Refresh,
        Self::OpenDesktopFolder,
        Self::ChangeBackground,
    ];

    /// The command the chosen row `item` names, or `None` for an id this menu
    /// never declared (fail closed — a command is never guessed at).
    ///
    /// The inverse of the numbering [`model`] gives each row, which is the
    /// shared one every command-list menu uses.
    #[must_use]
    pub fn from_item(item: AppMenuItemId) -> Option<Self> {
        Self::ALL.get(item.index()).copied()
    }

    /// The row label.
    ///
    /// The arrangement rows are worded as the *directions on screen* a user
    /// sees, not as the settings vocabulary's leading/trailing edges, because
    /// a menu is read by someone looking at their icons.
    ///
    /// Public because a host-side observer resolves a drawn row by the text it
    /// shows, so the label it looks for is this one definition rather than a
    /// copy of the wording.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::NewFolder => "New Folder",
            Self::SortBy(IconSort::Name) => "Sort by Name",
            Self::SortBy(IconSort::Kind) => "Sort by Kind",
            Self::SortBy(IconSort::Size) => "Sort by Size",
            Self::SortBy(IconSort::Date) => "Sort by Date",
            Self::ArrangeFrom(IconFlow::Leading) => "Arrange from the Left",
            Self::ArrangeFrom(IconFlow::Trailing) => "Arrange from the Right",
            Self::Refresh => "Refresh",
            Self::OpenDesktopFolder => "Open Desktop Folder",
            Self::ChangeBackground => "Change Background…",
        }
    }

    /// Whether this command begins a new group, drawing a divider above it.
    const fn opens_group(self) -> bool {
        matches!(
            self,
            Self::NewFolder
                | Self::SortBy(IconSort::Name)
                | Self::ArrangeFrom(IconFlow::Leading)
                | Self::Refresh
        )
    }

    /// Whether `settings` already has this command's effect in force.
    ///
    /// The sort orders and the arrangements are each a group of alternatives
    /// exactly one of which holds, so the one in force is the group's chosen
    /// member; every other command asks for an action rather than for a state,
    /// and none of them is ever already done.
    fn in_force(self, settings: &PinboardSettings) -> bool {
        match self {
            Self::SortBy(sort) => sort == settings.sort,
            Self::ArrangeFrom(flow) => flow == settings.icons,
            _ => false,
        }
    }

    /// Why choosing this command would change nothing, when it would.
    const fn already_reason(self) -> Option<&'static str> {
        match self {
            Self::SortBy(_) => Some(REASON_CURRENT_ORDER),
            Self::ArrangeFrom(_) => Some(REASON_CURRENT_ARRANGEMENT),
            _ => None,
        }
    }

    /// Whether this command is offered when the press did not land on an icon.
    ///
    /// A menu opened on empty backdrop has nothing to open.
    const fn needs_icon(self) -> bool {
        matches!(self, Self::Open)
    }
}

/// The backdrop menu the desktop's own secondary press opens, with the rows
/// `on_icon` and `settings` imply.
///
/// `on_icon` is whether the press landed on an icon, which is the only thing
/// that decides whether `Open` is offered. The sort order and the arrangement
/// in force are shown as their group's chosen member and are non-actionable,
/// because choosing what already holds is a statement of where the desktop is
/// rather than a command — and the first row of the plate never opens a group,
/// since there is nothing above it to divide it from.
#[must_use]
pub fn model(on_icon: bool, settings: &PinboardSettings) -> ChainModel {
    let mut model = ChainModel::new(PINBOARD_TITLE);
    for (index, command) in PinboardCommand::ALL.into_iter().enumerate() {
        if command.needs_icon() && !on_icon {
            continue;
        }
        let Some(id) = AppMenuItemId::for_index(index) else {
            continue;
        };
        let mut item = MenuItem::new(command.label());
        if command.in_force(settings) {
            item = item
                .with_mark(MenuMark::Radio)
                .with_state(ControlState::disabled());
            if let Some(reason) = command.already_reason() {
                item = item.with_reason(reason);
            }
        }
        let mut row = ChainRow::item(id, item);
        if command.opens_group() && !model.rows().is_empty() {
            row = row.grouped();
        }
        model.push(row);
    }
    model
}
