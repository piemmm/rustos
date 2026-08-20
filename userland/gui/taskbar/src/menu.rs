//! The bar's one right-click context surface, composed from the shared
//! `lib/controls` menu.
//!
//! A secondary press on a running application's slot, on the Switchboard
//! capsule, or on a program-library entry row while the popup is open, opens
//! this menu. It is pure presentation over a typed subject: choosing a row
//! only reports a typed [`TaskbarResponse`] outcome through the input router
//! — the session performs the action (a launch, a shortcut it writes, a menu
//! outcome it relays to the declaring application, a power transition it
//! relays to the one process that holds that authority) under its own
//! authority, and the bar holds none of it.
//!
//! An application's own menu is the one subject whose *rows* are not the
//! bar's: the application declared them over the window channel, and the bar
//! draws exactly what it declared. The one row the bar owns inside such a
//! menu is [`AppMenuRow::About`] — its submenu is the application's
//! information panel, drawn from the bundle's **signed** manifest, so an
//! application cannot state an identity that is not its own. While open the menu is
//! modal exactly like the library popup: every event routes here first, a
//! click away dismisses without acting on what it hit, and Escape dismisses
//! from the keyboard.
//!
//! [`TaskbarResponse`]: crate::input::TaskbarResponse

use alloc::vec::Vec;

use tairix_abi::window_ipc::{AppMenu, AppMenuItemId, AppMenuMark, AppMenuRow};
use tairix_controls::{ControlState, Fact, FactList, Menu, MenuAction, MenuItem, MenuMark};
use tairix_geometry::{Point, Rect, Region, Scale};
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_proglib::EntryId;
use tairix_theme::Theme;

use crate::apps::AppIdentity;
use crate::edge::Edge;
use crate::system::{self, SystemAction, SystemPermits};

/// What an open context menu is about.
// `App` carries the application's whole declared menu inline, because that
// is what the declaration is: a fixed, bounded model the window channel
// already delivered by value. Boxing it to equalise the variants would move
// one transient subject — built on a secondary press and dropped when the
// menu closes — onto the heap for nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MenuSubject {
    /// The running application at this strip index, showing the menu the
    /// application itself declared.
    App {
        /// The application's strip index.
        index: usize,
        /// The menu the application declared over the window channel.
        menu: AppMenu,
        /// The identity the application's information panel states, read
        /// from its signed manifest.
        identity: AppIdentity,
    },
    /// The program-library entry under the popup's pointer.
    Entry {
        /// The catalog entry the row names.
        entry: EntryId,
    },
    /// The desktop itself — the system quick-actions menu the Switchboard
    /// capsule opens.
    System {
        /// What the rows may offer, as attested by the processes that would
        /// carry each command out. The bar renders this; it never decides
        /// it.
        permits: SystemPermits,
    },
}

/// What choosing a menu row asks for; the input router translates this into
/// a typed [`TaskbarResponse`](crate::input::TaskbarResponse).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum MenuChoice {
    /// Relay this row's own id to the application that declared it.
    AppMenu {
        /// The application's strip index.
        index: usize,
        /// The id the application gave the chosen row.
        item: AppMenuItemId,
    },
    /// Launch this program-library entry.
    OpenEntry(EntryId),
    /// Perform this system quick action.
    System(SystemAction),
}

/// The outcome of routing one input event into the open menu.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum MenuOutcome {
    /// Claimed with no state change.
    Ignored,
    /// Claimed; only pixels changed (a hover or keyboard highlight moved).
    Changed,
    /// A row was chosen; the menu has closed.
    Choose(MenuChoice),
    /// The menu was dismissed without choosing (click away or Escape).
    Dismissed,
}

/// The row index of *Open* in a program-library-entry context menu.
///
/// `rows_for` builds that menu from this one definition and the menu's own
/// `choose` reads the activated row back through it, so the order is stated
/// once and a reordering cannot silently re-map what a row does. It is
/// public because aiming *at* a row is the same fact as reading one back:
/// the desktop's QEMU vertical clicks the row through [`Menu::row_rect`],
/// and must name it from this one definition rather than restating its
/// position.
pub const MENU_OPEN_ROW: usize = 0;

/// What the open menu shows beside its own plate: nothing, a submenu of the
/// application's own rows, or the application's information panel.
#[derive(Clone, Debug, Eq, PartialEq)]
enum OpenChild {
    /// The submenu the declared row at this top-level index opens, and the
    /// declared-row indices its rows map back to.
    Rows {
        /// The parent row's index in the top-level control.
        parent: usize,
        /// The submenu control.
        menu: Menu,
        /// Declared-row index per submenu row, in order.
        declared: Vec<usize>,
    },
    /// The application's information panel, attached to its *About* row.
    Info {
        /// The parent row's index in the top-level control.
        parent: usize,
        /// The facts the panel states, read from the signed manifest.
        facts: FactList,
    },
}

/// The computed geometry of the open context menu.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MenuLayout {
    /// The menu plate, in screen coordinates.
    pub panel: Rect,
    /// The open submenu or information panel's plate, or [`Rect::EMPTY`]
    /// when nothing is open beside the menu.
    pub child: Rect,
    /// The corner radius the window manager applies to the menu window —
    /// the same popup radius the menu's own plate is drawn with, so the
    /// window rounding and the painted plate can never disagree.
    pub corner_radius: u32,
}

impl MenuLayout {
    /// The whole surface the menu occupies: its plate together with
    /// whatever is open beside it.
    ///
    /// The session presents the menu as one window, so this is the window's
    /// extent — a submenu is part of the same surface rather than a second
    /// window to keep stacked against it.
    #[must_use]
    pub fn bounds(&self) -> Rect {
        if self.child.is_empty() {
            return self.panel;
        }
        self.panel.union(&self.child)
    }

    /// Whether `point` lies on the menu or on what is open beside it.
    #[must_use]
    pub fn contains(&self, point: Point) -> bool {
        self.panel.contains(point) || (!self.child.is_empty() && self.child.contains(point))
    }
}

/// The bar's context menu: closed, or open over one [`MenuSubject`].
#[derive(Clone, Debug)]
pub struct BarMenu {
    subject: Option<MenuSubject>,
    anchor: Rect,
    menu: Menu,
    /// Declared-row index per top-level row, for an application's own menu;
    /// empty for a menu whose rows the bar itself wrote.
    declared: Vec<usize>,
    child: Option<OpenChild>,
}

impl Default for BarMenu {
    fn default() -> Self {
        Self::new()
    }
}

impl BarMenu {
    /// A closed menu.
    #[must_use]
    pub fn new() -> Self {
        Self {
            subject: None,
            anchor: Rect::EMPTY,
            menu: Menu::new(Vec::new()),
            declared: Vec::new(),
            child: None,
        }
    }

    /// Whether the menu is open (and therefore modal).
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.subject.is_some()
    }

    /// The open menu's subject, if any.
    #[must_use]
    pub fn subject(&self) -> Option<&MenuSubject> {
        self.subject.as_ref()
    }

    /// The shared menu control, for painting.
    #[must_use]
    pub fn control(&self) -> &Menu {
        &self.menu
    }

    /// Open the menu for `subject`, anchored at the screen-space rectangle
    /// of the slot or row the secondary press landed on.
    pub(crate) fn open(&mut self, subject: MenuSubject, anchor: Rect) {
        let (rows, declared) = rows_for(&subject);
        self.menu = Menu::new(rows);
        self.declared = declared;
        self.child = None;
        self.anchor = anchor;
        self.subject = Some(subject);
    }

    /// Close the menu without acting.
    pub(crate) fn close(&mut self) {
        self.subject = None;
        self.menu = Menu::new(Vec::new());
        self.declared = Vec::new();
        self.child = None;
        self.anchor = Rect::EMPTY;
    }

    /// The open submenu control, for painting, and the rectangle it occupies.
    #[must_use]
    pub fn submenu(&self) -> Option<&Menu> {
        match self.child.as_ref()? {
            OpenChild::Rows { menu, .. } => Some(menu),
            OpenChild::Info { .. } => None,
        }
    }

    /// The open information panel, for painting.
    #[must_use]
    pub fn info_panel(&self) -> Option<&FactList> {
        match self.child.as_ref()? {
            OpenChild::Info { facts, .. } => Some(facts),
            OpenChild::Rows { .. } => None,
        }
    }

    /// The menu's geometry: its preferred size opening outward from the
    /// anchor on the bar's `edge`, clamped to the screen. `None` while
    /// closed.
    #[must_use]
    pub fn layout(
        &self,
        edge: Edge,
        screen_width: u32,
        screen_height: u32,
        scale: Scale,
        theme: &Theme,
    ) -> Option<MenuLayout> {
        self.subject.as_ref()?;
        let width = self.menu.preferred_width(scale, theme).max(1);
        let height = self.menu.preferred_height(scale, theme).max(1);
        let gap = scale.scale_length(theme.metrics().control_gap);
        let (x, y) = match edge {
            Edge::Bottom => (
                self.anchor.left(),
                self.anchor.top() - to_i32(height) - to_i32(gap),
            ),
            Edge::Top => (self.anchor.left(), self.anchor.bottom() + to_i32(gap)),
            Edge::Left => (self.anchor.right() + to_i32(gap), self.anchor.top()),
            Edge::Right => (
                self.anchor.left() - to_i32(width) - to_i32(gap),
                self.anchor.top(),
            ),
        };
        let screen = Rect::new(0, 0, screen_width, screen_height);
        let panel = Rect::new(x, y, width, height).clamped_onto(screen);
        let child = self.child_rect(&panel, screen, scale, theme);
        Some(MenuLayout {
            panel,
            child,
            corner_radius: scale.scale_length(theme.metrics().popup_corner_radius),
        })
    }

    /// The rectangle whatever is open beside the menu occupies, or
    /// [`Rect::EMPTY`] when nothing is.
    ///
    /// A child opens to the trailing side of its parent row and flips to the
    /// leading side when that would leave the screen, so it is never clipped
    /// — the same rule the shared control applies to its own submenu anchor,
    /// which is why the anchor comes from [`Menu::row_rect`] rather than
    /// being re-derived from the row height here.
    fn child_rect(&self, panel: &Rect, screen: Rect, scale: Scale, theme: &Theme) -> Rect {
        let (parent, width, height) = match self.child.as_ref() {
            None => return Rect::EMPTY,
            Some(OpenChild::Rows { parent, menu, .. }) => (
                *parent,
                menu.preferred_width(scale, theme).max(1),
                menu.preferred_height(scale, theme).max(1),
            ),
            Some(OpenChild::Info { parent, facts }) => (
                *parent,
                info_panel_width(scale, theme),
                facts.measured_height(scale, theme).max(1),
            ),
        };
        let Some(row) = self.menu.row_rect(parent, *panel, scale, theme) else {
            return Rect::EMPTY;
        };
        let trailing = Rect::new(panel.right(), row.top(), width, height);
        let candidate = if trailing.right() <= screen.right() {
            trailing
        } else {
            Rect::new(panel.left() - to_i32(width), row.top(), width, height)
        };
        candidate.clamped_onto(screen)
    }

    /// Route a pointer event into the open menu (which is modal): events
    /// over the plate drive the shared control; a press anywhere else
    /// dismisses without acting on what it hit.
    pub(crate) fn route_pointer(
        &mut self,
        event: &InputEvent,
        pointer: Point,
        layout: &MenuLayout,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> MenuOutcome {
        // A pointer over the open child belongs to the child: the parent row
        // stays highlighted (it is what the child hangs from) and the child
        // owns hover and activation within its own plate.
        if !layout.child.is_empty() && layout.child.contains(pointer) {
            return self.route_child_pointer(event, layout, scale, theme, damage);
        }
        match event {
            InputEvent::PointerMoved { .. } => {
                let before = self.menu.current();
                let _ = self
                    .menu
                    .on_pointer(event, layout.panel, scale, theme, damage);
                if self.menu.current() == before {
                    MenuOutcome::Ignored
                } else {
                    MenuOutcome::Changed
                }
            }
            InputEvent::PointerPressed { .. } if !layout.contains(pointer) => {
                self.close();
                MenuOutcome::Dismissed
            }
            InputEvent::PointerPressed {
                button: PointerButton::Primary,
            }
            | InputEvent::PointerReleased {
                button: PointerButton::Primary,
            } => match self
                .menu
                .on_pointer(event, layout.panel, scale, theme, damage)
            {
                Some(MenuAction::Activated { index }) => self.choose(index),
                Some(MenuAction::Dismissed) => {
                    self.close();
                    MenuOutcome::Dismissed
                }
                Some(MenuAction::OpenSubmenu { index }) => {
                    self.open_child(index);
                    MenuOutcome::Changed
                }
                // An armed press is a pixel change (the row highlights).
                None => MenuOutcome::Changed,
            },
            // Any other button or a scroll over the plate is claimed by the
            // modal surface and does nothing.
            _ => MenuOutcome::Ignored,
        }
    }

    /// Route a key into the open menu: arrows move the highlight,
    /// Enter/Space choose, Escape dismisses.
    pub(crate) fn route_key(
        &mut self,
        key: Key,
        layout: &MenuLayout,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> MenuOutcome {
        // Escape inside an open child closes the child, not the menu: one
        // key, one step back.
        if self.child.is_some() {
            if key == Key::Named(NamedKey::Escape) {
                self.child = None;
                return MenuOutcome::Changed;
            }
            if let Some(outcome) = self.route_child_key(key, layout, scale, theme, damage) {
                return outcome;
            }
        }
        match self.menu.on_key(key, layout.panel, scale, theme, damage) {
            Some(MenuAction::Activated { index }) => self.choose(index),
            Some(MenuAction::Dismissed) => {
                self.close();
                MenuOutcome::Dismissed
            }
            Some(MenuAction::OpenSubmenu { index }) => {
                self.open_child(index);
                MenuOutcome::Changed
            }
            // A cursor move is a pixel change.
            None => MenuOutcome::Changed,
        }
    }

    /// Route a pointer event that landed on the open child.
    fn route_child_pointer(
        &mut self,
        event: &InputEvent,
        layout: &MenuLayout,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> MenuOutcome {
        let Some(OpenChild::Rows { menu, declared, .. }) = self.child.as_mut() else {
            // The information panel states facts and offers no action, so a
            // pointer over it is claimed and does nothing.
            return MenuOutcome::Ignored;
        };
        let before = menu.current();
        match menu.on_pointer(event, layout.child, scale, theme, damage) {
            Some(MenuAction::Activated { index }) => {
                let row = declared.get(index).copied();
                self.choose_declared(row)
            }
            Some(MenuAction::Dismissed) => {
                self.child = None;
                MenuOutcome::Changed
            }
            // A submenu is one level deep, so a row inside one opens nothing.
            Some(MenuAction::OpenSubmenu { .. }) => MenuOutcome::Ignored,
            None => {
                if menu.current() == before {
                    MenuOutcome::Ignored
                } else {
                    MenuOutcome::Changed
                }
            }
        }
    }

    /// Route a key into the open child, or `None` to let the parent have it.
    fn route_child_key(
        &mut self,
        key: Key,
        layout: &MenuLayout,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> Option<MenuOutcome> {
        let Some(OpenChild::Rows { menu, declared, .. }) = self.child.as_mut() else {
            return None;
        };
        match menu.on_key(key, layout.child, scale, theme, damage) {
            Some(MenuAction::Activated { index }) => {
                let row = declared.get(index).copied();
                Some(self.choose_declared(row))
            }
            Some(MenuAction::Dismissed) => {
                self.child = None;
                Some(MenuOutcome::Changed)
            }
            Some(MenuAction::OpenSubmenu { .. }) => Some(MenuOutcome::Ignored),
            None => Some(MenuOutcome::Changed),
        }
    }

    /// Open whatever the top-level row at `index` hangs off itself: the
    /// application's rows for a declared submenu, its information panel for
    /// the *About* row. A row that hangs nothing off itself opens nothing.
    fn open_child(&mut self, index: usize) {
        let Some(MenuSubject::App { menu, identity, .. }) = self.subject.as_ref() else {
            return;
        };
        let Some(&declared_parent) = self.declared.get(index) else {
            return;
        };
        let rows: Vec<(usize, AppMenuRow)> = menu
            .rows()
            .enumerate()
            .filter_map(|(row, (kind, parent))| (parent? == declared_parent).then_some((row, kind)))
            .collect();
        let Some((_, parent_row)) = menu
            .rows()
            .nth(declared_parent)
            .map(|r| (declared_parent, r.0))
        else {
            return;
        };
        self.child = match parent_row {
            AppMenuRow::About => Some(OpenChild::Info {
                parent: index,
                facts: info_facts(identity),
            }),
            AppMenuRow::Submenu { .. } if !rows.is_empty() => Some(OpenChild::Rows {
                parent: index,
                menu: Menu::new(rows.iter().map(|&(_, kind)| declared_item(kind)).collect()),
                declared: rows.iter().map(|&(row, _)| row).collect(),
            }),
            _ => None,
        };
    }

    /// Report the choice a declared row names, closing the whole menu.
    ///
    /// A row that names no item — a separator or a submenu parent reached
    /// through a stale index — dismisses rather than guessing an action.
    fn choose_declared(&mut self, row: Option<usize>) -> MenuOutcome {
        let Some(MenuSubject::App { index, menu, .. }) = self.subject.as_ref() else {
            self.close();
            return MenuOutcome::Dismissed;
        };
        let index = *index;
        let item = row
            .and_then(|row| menu.rows().nth(row))
            .and_then(|(kind, _)| match kind {
                AppMenuRow::Item { id, .. } => Some(id),
                _ => None,
            });
        self.close();
        match item {
            Some(item) => MenuOutcome::Choose(MenuChoice::AppMenu { index, item }),
            None => MenuOutcome::Dismissed,
        }
    }

    /// Translate the activated top-level row into the subject's typed
    /// choice and close; a row that names no action (impossible through the
    /// control) is a dismissal, never a guessed one.
    fn choose(&mut self, row: usize) -> MenuOutcome {
        let Some(subject) = self.subject.clone() else {
            return MenuOutcome::Dismissed;
        };
        match (subject, row) {
            (MenuSubject::App { .. }, row) => {
                let declared = self.declared.get(row).copied();
                self.choose_declared(declared)
            }
            (MenuSubject::Entry { entry }, MENU_OPEN_ROW) => {
                self.close();
                MenuOutcome::Choose(MenuChoice::OpenEntry(entry))
            }
            (MenuSubject::System { permits }, row) => {
                self.close();
                match system::action_at(permits, row) {
                    Some(action) => MenuOutcome::Choose(MenuChoice::System(action)),
                    None => MenuOutcome::Dismissed,
                }
            }
            _ => {
                self.close();
                MenuOutcome::Dismissed
            }
        }
    }
}

/// The rows a subject offers, with the declared-row index each maps back to.
///
/// An application's menu is the application's own: every top-level row it
/// declared becomes a row here in declaration order, and the returned index
/// list is how a chosen row is read back to the declaration — the mapping is
/// stated once rather than re-derived on the way back. Every other subject's
/// rows are the bar's own and map back to nothing.
fn rows_for(subject: &MenuSubject) -> (Vec<MenuItem>, Vec<usize>) {
    match subject {
        MenuSubject::App { menu, .. } => {
            let mut items = Vec::new();
            let mut declared = Vec::new();
            let mut group_break = false;
            for (row, (kind, parent)) in menu.rows().enumerate() {
                if parent.is_some() {
                    continue;
                }
                // A separator is not a row of its own: it opens the group
                // the next row begins, which is how the shared control keeps
                // every reported index a real command.
                if matches!(kind, AppMenuRow::Separator) {
                    group_break = true;
                    continue;
                }
                items.push(declared_item(kind).with_group_break(group_break));
                declared.push(row);
                group_break = false;
            }
            (items, declared)
        }
        MenuSubject::Entry { .. } => (alloc::vec![MenuItem::new("Open")], Vec::new()),
        MenuSubject::System { permits } => (system::rows(*permits), Vec::new()),
    }
}

/// The shared control row one declared application row draws as.
///
/// The *About* row's label is the bar's, not the application's: the panel it
/// opens is system chrome stating an attested identity, so every application
/// reaches it by the same name.
fn declared_item(row: AppMenuRow) -> MenuItem {
    match row {
        AppMenuRow::Item {
            label,
            enabled,
            mark,
            ..
        } => {
            let item = MenuItem::new(label.as_str()).with_mark(match mark {
                AppMenuMark::None => MenuMark::None,
                AppMenuMark::Check => MenuMark::Check,
                AppMenuMark::Radio => MenuMark::Radio,
            });
            if enabled {
                item
            } else {
                item.with_state(ControlState::disabled())
            }
        }
        AppMenuRow::Submenu { label, enabled } => {
            let item = MenuItem::new(label.as_str()).with_submenu(true);
            if enabled {
                item
            } else {
                item.with_state(ControlState::disabled())
            }
        }
        AppMenuRow::About => MenuItem::new(ABOUT_ROW_LABEL).with_submenu(true),
        // A separator never becomes a row (`rows_for` folds it into the next
        // row's group break); an unreachable one draws as a spacer rather
        // than as something choosable.
        AppMenuRow::Separator => MenuItem::new("").with_state(ControlState::disabled()),
    }
}

/// The label the bar gives every application's information row.
const ABOUT_ROW_LABEL: &str = "About";

/// Logical width of the application information panel at the reference
/// density: wide enough for a one-line purpose beside its label.
const INFO_PANEL_WIDTH: u32 = 260;

/// The information panel's width in physical pixels at `scale`.
fn info_panel_width(scale: Scale, _theme: &Theme) -> u32 {
    scale.scale_length(INFO_PANEL_WIDTH).max(1)
}

/// The facts an application's information panel states, in a fixed order.
///
/// Name and version are always present (a signed manifest carries both), so
/// the panel can never be empty; purpose and author appear only when the
/// manifest states them, rather than as blank rows.
fn info_facts(identity: &AppIdentity) -> FactList {
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

/// Saturating `u32` → `i32`, clamping rather than wrapping.
fn to_i32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}
