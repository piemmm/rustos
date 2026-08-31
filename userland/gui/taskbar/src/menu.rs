//! The bar's menu *subjects*: what each of its four menus offers, and how a
//! chosen row is read back (`plans/NEW-MENUS.md` M3.4).
//!
//! The bar describes and the desktop's one menu service decides. Each builder
//! here answers with a [`MenuRequest`] — a [`ChainModel`], where it hangs, and
//! which menu it is — and the inverse reads a chosen row back into the typed
//! [`TaskbarResponse`] the embedder carries out
//! ([`Taskbar::menu_chosen`](crate::Taskbar::menu_chosen)). The plate, the
//! title band, the placement, the grab, keyboard traversal and dismissal are
//! the chain's; nothing here draws a menu pixel or keeps a menu shell.
//!
//! Rows are built from one ordered list per subject and read back against that
//! same list, so a reordering cannot re-map what a row does. A row's id is its
//! *command's* position in that list rather than its position on the plate, so
//! a menu that leaves a row out — the system menu without *Switch User…* —
//! shifts no other row's meaning.
//!
//! An application's own menu is the one subject whose rows are not the bar's:
//! the application declared them over the window channel, and
//! [`ChainModel::from_app_menu`] decodes exactly what it declared. The one row
//! the bar owns inside such a menu is the information row, whose child is the
//! desktop-drawn panel of the bundle's **signed** manifest, so an application
//! cannot state an identity that is not its own.

use tairix_abi::window_ipc::{AppMenu, AppMenuItemId};
use tairix_controls::{ChainModel, ChainRow, Fact, FactList, MenuItem, PlatePlacement, PlateSide};
use tairix_geometry::Rect;
use tairix_proglib::EntryId;

use crate::apps::AppIdentity;
use crate::clock_menu::{self, ClockPermits};
use crate::edge::Edge;
use crate::input::TaskbarResponse;
use crate::system::{self, SystemPermits};

/// Which of the bar's menus a chain belongs to, and what a chosen row of it
/// acts on.
///
/// The address the desktop answers the chain at, and the least that answer
/// needs: the rows the plate was built from belong to the chain, and the bar
/// keeps no copy of them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MenuSubject {
    /// The menu the application at this strip index declared. A chosen row is
    /// the application's own id, relayed straight back to it.
    App {
        /// The application's strip index.
        app: usize,
    },
    /// The context menu on this program-library entry's row.
    Entry {
        /// The catalog entry the row names.
        entry: EntryId,
    },
    /// The desktop's system quick actions.
    System,
    /// The bar's clock.
    Clock,
}

impl MenuSubject {
    /// What choosing the row `item` of this menu asks the embedder for, or
    /// `None` for an id the menu never declared (fail closed — never a
    /// guessed command).
    ///
    /// The bar interprets no application row: it hands the id straight back to
    /// the process that declared it.
    #[must_use]
    pub(crate) fn chosen(&self, item: AppMenuItemId) -> Option<TaskbarResponse> {
        match self {
            Self::App { app } => Some(TaskbarResponse::AppMenuChosen { app: *app, item }),
            Self::Entry { entry } => match EntryRow::at(item.index())? {
                EntryRow::Open => Some(TaskbarResponse::LibraryLaunch {
                    entry: entry.clone(),
                }),
                EntryRow::Shortcut => Some(TaskbarResponse::CreateDesktopShortcut {
                    entry: entry.clone(),
                }),
            },
            Self::System => system::response_at(item.index()),
            Self::Clock => clock_menu::response_at(item.index()),
        }
    }

    /// Whether choosing a row of this menu closes the program-library popup.
    ///
    /// Only the row menu *inside* that popup does: launching a bundle or
    /// putting a shortcut on the desktop both act somewhere the popup would
    /// stand between the user and the result.
    #[must_use]
    pub(crate) const fn closes_library(&self) -> bool {
        matches!(self, Self::Entry { .. })
    }
}

/// What the bar asks the desktop to open: which menu, its rows, and where it
/// hangs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MenuRequest {
    /// Which menu, and what a chosen row acts on.
    pub subject: MenuSubject,
    /// The rows to draw, their ids, and the root plate's title.
    pub model: ChainModel,
    /// The slot or row the press landed on, and the side the plate opens on.
    pub placement: PlatePlacement,
}

/// One row of a program-library entry's context menu.
///
/// Both things the popup can do to a row that its own click cannot: launch the
/// bundle, and put a shortcut to it on the desktop. The bar performs neither —
/// each becomes a typed response the session carries out under its own
/// authority.
///
/// Public because aiming *at* a row is the same fact as reading one back: an
/// embedder's own test, or a QEMU pointer script, finds the row by the label
/// [`EntryRow::label`] gives it rather than by restating its position.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum EntryRow {
    /// Launch the entry's bundle.
    Open,
    /// Create a symbolic link to the entry's bundle in the user's own
    /// `Desktop` folder.
    Shortcut,
}

/// The rows a program-library entry's context menu offers, in row order.
///
/// The one definition of that menu: the model states these labels and the
/// chosen id is read back through the same list, so a reordering cannot
/// silently re-map what a row does.
const ENTRY_ROWS: [EntryRow; 2] = [EntryRow::Open, EntryRow::Shortcut];

impl EntryRow {
    /// The label this row draws with.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::Shortcut => "Create Desktop Shortcut",
        }
    }

    /// The row at `index` of the menu, or `None` for an index it never drew.
    fn at(index: usize) -> Option<Self> {
        ENTRY_ROWS.get(index).copied()
    }
}

/// The title the entry menu's plate carries: the surface the rows act on,
/// named as the user knows it.
const ENTRY_TITLE: &str = "Program";

/// The model a program-library entry's context menu opens with.
#[must_use]
pub(crate) fn entry_menu() -> ChainModel {
    let mut model = ChainModel::new(ENTRY_TITLE);
    for (index, row) in ENTRY_ROWS.into_iter().enumerate() {
        if let Some(id) = AppMenuItemId::for_index(index) {
            model.push(ChainRow::item(id, MenuItem::new(row.label())));
        }
    }
    model
}

/// The model the menu the application `name` declared opens with, titled from
/// its **signed** manifest and stating `identity` in its information panel.
///
/// The wire model decodes into the one service model, so what the plate draws
/// is exactly what the application declared — and nothing it declared can
/// claim the system lacks authority for a row, because the wire carries no
/// field for that.
#[must_use]
pub(crate) fn app_menu(name: &str, declared: &AppMenu, identity: &AppIdentity) -> ChainModel {
    ChainModel::from_app_menu(name, declared, Some(&info_facts(identity)))
}

/// The title the desktop's system quick-actions plate carries.
const SYSTEM_TITLE: &str = "System";

/// The model the system quick-actions menu opens with, for what `permits`
/// offers.
#[must_use]
pub(crate) fn system_menu(permits: SystemPermits) -> ChainModel {
    rows_from(SYSTEM_TITLE, system::rows(permits))
}

/// The title the clock's plate carries.
const CLOCK_TITLE: &str = "Clock";

/// The model the clock's menu opens with, for what `permits` offers.
#[must_use]
pub(crate) fn clock_menu(permits: &ClockPermits) -> ChainModel {
    rows_from(CLOCK_TITLE, clock_menu::rows(permits))
}

/// A flat model titled `title` from rows a subject's own table produced, each
/// carrying the id of the command's position in that table.
///
/// The one place the bar's three own menus turn a table position into a row
/// id, so the numbering and [`MenuSubject::chosen`]'s inverse cannot drift.
fn rows_from(title: &str, rows: alloc::vec::Vec<(usize, MenuItem)>) -> ChainModel {
    let mut model = ChainModel::new(title);
    for (index, item) in rows {
        let Some(id) = AppMenuItemId::for_index(index) else {
            continue;
        };
        model.push(ChainRow::item(id, item));
    }
    model
}

/// Where a menu anchored at `anchor` opens for a bar on `edge`, with `gap`
/// pixels of clearance.
///
/// A bar's own menu opens away from the bar, so the plate never covers the
/// slot the user pressed; which side it lands on when that would leave the
/// screen is the shared placement rule's answer, not a second one here.
#[must_use]
pub(crate) const fn placement(anchor: Rect, edge: Edge, gap: u32) -> PlatePlacement {
    PlatePlacement {
        anchor,
        side: match edge {
            Edge::Bottom => PlateSide::Above,
            Edge::Top => PlateSide::Below,
            Edge::Left => PlateSide::Trailing,
            Edge::Right => PlateSide::Leading,
        },
        gap,
    }
}

/// The facts an application's information panel states, in a fixed order.
///
/// Name and version are always present (a signed manifest carries both), so
/// the panel can never be empty; purpose and author appear only when the
/// manifest states them, rather than as blank rows.
#[must_use]
pub fn info_facts(identity: &AppIdentity) -> FactList {
    let mut facts = alloc::vec![
        Fact::new("Name", identity.name.clone()),
        Fact::new("Version", identity.version.clone()),
    ];
    if let Some(purpose) = &identity.purpose {
        facts.push(Fact::new("Purpose", purpose.clone()));
    }
    if let Some(author) = &identity.author {
        facts.push(Fact::new("Author", author.clone()));
    }
    FactList::new(facts).with_separators(true)
}
