//! The bar's one right-click context surface, composed from the shared
//! `lib/controls` menu.
//!
//! A secondary press on a pinned shortcut, on the Switchboard capsule, or on
//! a program-library entry row while the popup is open, opens this menu. It
//! is pure presentation over a typed subject: choosing a row only reports a
//! typed [`TaskbarResponse`] outcome through the input router — the session
//! performs the action (a launch, a pin-store edit, a power transition it
//! relays to the one process that holds that authority) under its own
//! authority, and the bar holds none of it. While open the menu is
//! modal exactly like the library popup: every event routes here first, a
//! click away dismisses without acting on what it hit, and Escape dismisses
//! from the keyboard.
//!
//! [`TaskbarResponse`]: crate::input::TaskbarResponse

use tairix_controls::{Menu, MenuAction, MenuItem};
use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, Key, PointerButton};
use tairix_proglib::EntryId;
use tairix_theme::Theme;

use crate::edge::Edge;
use crate::system::{self, SystemAction, SystemPermits};

/// What an open context menu is about.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MenuSubject {
    /// The pinned shortcut at this strip index.
    Pin {
        /// The pin's strip index.
        index: usize,
        /// Whether the pinned application currently has a running window
        /// (chooses between restoring it and launching it on *Open*).
        running: bool,
    },
    /// The program-library entry under the popup's pointer.
    Entry {
        /// The catalog entry the row names.
        entry: EntryId,
        /// The entry's current pin-strip index when it is already pinned,
        /// so the menu offers *Unpin from taskbar* instead of *Pin*.
        pinned: Option<usize>,
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
    /// Restore/focus the running window behind the pin at this index.
    RestorePin(usize),
    /// Launch the (not running) application behind the pin at this index.
    LaunchPin(usize),
    /// Remove the pin at this index from the strip.
    Unpin(usize),
    /// Launch this program-library entry.
    OpenEntry(EntryId),
    /// Pin this program-library entry to the strip.
    PinEntry(EntryId),
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

/// The row index of *Open* in a pinned-shortcut or program-library-entry
/// context menu.
///
/// `rows_for` builds both menus in one fixed order — *Open* first, the pin
/// affordance second — and the menu's own `choose` reads the activated row
/// back through these same two definitions, so the order is stated once and
/// a reordering cannot silently re-map what a row does. They are public
/// because aiming *at* a row is the same fact as reading one back: the
/// desktop's QEMU vertical clicks the pin affordance through
/// [`Menu::row_rect`], and must name the row from this one definition
/// rather than restating its position.
pub const MENU_OPEN_ROW: usize = 0;

/// The row index of the pin affordance in those same menus (see
/// [`MENU_OPEN_ROW`]).
pub const MENU_PIN_ROW: usize = 1;

/// The computed geometry of the open context menu.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct MenuLayout {
    /// The menu plate, in screen coordinates.
    pub panel: Rect,
    /// The corner radius the window manager applies to the menu window —
    /// the same popup radius the menu's own plate is drawn with, so the
    /// window rounding and the painted plate can never disagree.
    pub corner_radius: u32,
}

/// The bar's context menu: closed, or open over one [`MenuSubject`].
#[derive(Clone, Debug)]
pub struct BarMenu {
    subject: Option<MenuSubject>,
    anchor: Rect,
    menu: Menu,
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
            menu: Menu::new(alloc::vec::Vec::new()),
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
        self.menu = Menu::new(rows_for(&subject));
        self.anchor = anchor;
        self.subject = Some(subject);
    }

    /// Close the menu without acting.
    pub(crate) fn close(&mut self) {
        self.subject = None;
        self.menu = Menu::new(alloc::vec::Vec::new());
        self.anchor = Rect::EMPTY;
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
        let panel = Rect::new(x, y, width, height).clamped_onto(Rect::new(
            0,
            0,
            screen_width,
            screen_height,
        ));
        Some(MenuLayout {
            panel,
            corner_radius: scale.scale_length(theme.metrics().popup_corner_radius),
        })
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
    ) -> MenuOutcome {
        match event {
            InputEvent::PointerMoved { .. } => {
                let before = self.menu.current();
                let _ = self.menu.on_pointer(event, layout.panel, scale, theme);
                if self.menu.current() == before {
                    MenuOutcome::Ignored
                } else {
                    MenuOutcome::Changed
                }
            }
            InputEvent::PointerPressed { .. } if !layout.panel.contains(pointer) => {
                self.close();
                MenuOutcome::Dismissed
            }
            InputEvent::PointerPressed {
                button: PointerButton::Primary,
            }
            | InputEvent::PointerReleased {
                button: PointerButton::Primary,
            } => match self.menu.on_pointer(event, layout.panel, scale, theme) {
                Some(MenuAction::Activated { index }) => self.choose(index),
                Some(MenuAction::Dismissed) => {
                    self.close();
                    MenuOutcome::Dismissed
                }
                // Rows here never open submenus; an armed press is a pixel
                // change (the row highlights).
                Some(MenuAction::OpenSubmenu { .. }) | None => MenuOutcome::Changed,
            },
            // Any other button or a scroll over the plate is claimed by the
            // modal surface and does nothing.
            _ => MenuOutcome::Ignored,
        }
    }

    /// Route a key into the open menu: arrows move the highlight,
    /// Enter/Space choose, Escape dismisses.
    pub(crate) fn route_key(&mut self, key: Key) -> MenuOutcome {
        match self.menu.on_key(key) {
            Some(MenuAction::Activated { index }) => self.choose(index),
            Some(MenuAction::Dismissed) => {
                self.close();
                MenuOutcome::Dismissed
            }
            // Rows here never open submenus; a cursor move is a pixel change.
            Some(MenuAction::OpenSubmenu { .. }) | None => MenuOutcome::Changed,
        }
    }

    /// Translate the activated row into the subject's typed choice and
    /// close; an out-of-range row (impossible through the control) is a
    /// dismissal, never a guessed action.
    fn choose(&mut self, row: usize) -> MenuOutcome {
        let Some(subject) = self.subject.take() else {
            return MenuOutcome::Dismissed;
        };
        self.close();
        let choice = match (subject, row) {
            (
                MenuSubject::Pin {
                    index,
                    running: true,
                },
                MENU_OPEN_ROW,
            ) => MenuChoice::RestorePin(index),
            (
                MenuSubject::Pin {
                    index,
                    running: false,
                },
                MENU_OPEN_ROW,
            ) => MenuChoice::LaunchPin(index),
            (
                MenuSubject::Pin { index, .. }
                | MenuSubject::Entry {
                    pinned: Some(index),
                    ..
                },
                MENU_PIN_ROW,
            ) => MenuChoice::Unpin(index),
            (MenuSubject::Entry { entry, .. }, MENU_OPEN_ROW) => MenuChoice::OpenEntry(entry),
            (
                MenuSubject::Entry {
                    entry,
                    pinned: None,
                },
                MENU_PIN_ROW,
            ) => MenuChoice::PinEntry(entry),
            (MenuSubject::System { permits }, row) => match system::action_at(permits, row) {
                Some(action) => MenuChoice::System(action),
                None => return MenuOutcome::Dismissed,
            },
            _ => return MenuOutcome::Dismissed,
        };
        MenuOutcome::Choose(choice)
    }
}

/// The rows a subject offers, in fixed order.
///
/// A pin or entry subject offers *Open* first, then the pin affordance; the
/// desktop subject offers the system quick actions, whose one ordered
/// definition lives in [`crate::system::ROWS`].
fn rows_for(subject: &MenuSubject) -> alloc::vec::Vec<MenuItem> {
    let pin_row = match subject {
        MenuSubject::Pin { .. } => MenuItem::new("Unpin"),
        MenuSubject::Entry {
            pinned: Some(_), ..
        } => MenuItem::new("Unpin from taskbar"),
        MenuSubject::Entry { pinned: None, .. } => MenuItem::new("Pin to taskbar"),
        MenuSubject::System { permits } => return system::rows(*permits),
    };
    alloc::vec![MenuItem::new("Open"), pin_row]
}

/// Saturating `u32` → `i32`, clamping rather than wrapping.
fn to_i32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}
