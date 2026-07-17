//! The start menu and its entries.
//!
//! The start-menu button sits at the leading end of the taskbar. The menu is
//! seeded with the session controls (log out, lock, shut down, restart) and
//! may additionally carry **application launcher** entries and a **light/dark
//! appearance toggle** appended after them. All kinds are ordinary
//! [`MenuEntry`] values distinguished by their [`MenuAction`], so each was
//! added without changing the public list/activate interface (extend, do not creep; `PLAN.md` Stage 7).
//!
//! The taskbar never launches anything itself: activating a launcher entry
//! reports its [`LauncherId`] so the session glue (the window manager /
//! `appmgr`) starts the matching application bundle. The
//! taskbar holds no capability to spawn processes.

use alloc::borrow::Cow;
use alloc::string::String;
use alloc::vec::Vec;

/// A session-control action offered by the start menu.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SessionControl {
    /// End the current user's session.
    LogOut,
    /// Lock the screen, keeping the session running.
    Lock,
    /// Power the machine off.
    ShutDown,
    /// Reboot the machine.
    Restart,
}

impl SessionControl {
    /// The fixed display label for the control.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::LogOut => "Log Out",
            Self::Lock => "Lock",
            Self::ShutDown => "Shut Down",
            Self::Restart => "Restart",
        }
    }

    /// The session controls, in their fixed menu order.
    #[must_use]
    pub const fn all() -> [Self; 4] {
        [Self::LogOut, Self::Lock, Self::ShutDown, Self::Restart]
    }
}

/// A stable identifier for the application an [`MenuAction::Launch`] entry
/// starts.
///
/// The taskbar does not resolve or spawn applications: it reports the chosen
/// `LauncherId` to its caller, which maps it to an application bundle and
/// launches it. The id is opaque to the taskbar and is
/// assigned by whoever populates the menu.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct LauncherId(pub u32);

/// What activating a [`MenuEntry`] does.
///
/// Every variant is [`Copy`], so a [`MenuEntry`]'s action travels by value
/// through the input router without borrowing the menu.
/// The entry's *display label* is stored on the [`MenuEntry`] itself
/// ([`MenuEntry::label`]), which is why a launcher's human-readable name does
/// not live here.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MenuAction {
    /// Invoke a session control.
    Session(SessionControl),
    /// Launch the application identified by this [`LauncherId`].
    Launch(LauncherId),
    /// Switch the desktop between its light and dark appearance.
    ///
    /// The taskbar holds no theme registry; activating this entry reports the
    /// action and the session glue performs the switch on the shared
    /// `tairix_theme::ThemeRegistry`.
    ToggleAppearance,
}

/// A stable identifier for a [`MenuEntry`] within a menu.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct MenuEntryId(pub u32);

/// One row of the start menu: a stable id, the action it triggers, and the
/// label shown for it.
///
/// The label is owned by the entry so a launcher can carry an
/// application-supplied name while a session control reuses its static label
/// without allocating.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MenuEntry {
    /// The entry's stable id, unique within its menu.
    pub id: MenuEntryId,
    /// The action performed when the entry is activated.
    pub action: MenuAction,
    label: Cow<'static, str>,
}

impl MenuEntry {
    /// The entry's display label.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }
}

/// The start menu: an open/closed toggle and an ordered list of entries.
///
/// The session controls occupy the fixed ids `1..=4` at the head of the list;
/// launcher entries appended with [`add_launcher`](Self::add_launcher) follow
/// them with ascending ids, so the session controls keep a stable position
/// and id regardless of how many launchers are present.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartMenu {
    open: bool,
    entries: Vec<MenuEntry>,
}

impl StartMenu {
    /// A closed menu populated with the session controls in their fixed
    /// order, assigned stable ids `1..=4`.
    #[must_use]
    pub fn with_session_controls() -> Self {
        let mut entries = Vec::new();
        for (index, control) in SessionControl::all().into_iter().enumerate() {
            let ordinal = u32::try_from(index).unwrap_or(u32::MAX);
            entries.push(MenuEntry {
                id: MenuEntryId(ordinal.saturating_add(1)),
                action: MenuAction::Session(control),
                label: Cow::Borrowed(control.label()),
            });
        }
        Self {
            open: false,
            entries,
        }
    }

    /// `true` when the menu is showing.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open
    }

    /// Toggle the menu open/closed (what the start button does) and return
    /// the new state.
    pub fn toggle(&mut self) -> bool {
        self.open = !self.open;
        self.open
    }

    /// Close the menu. Idempotent.
    pub fn close(&mut self) {
        self.open = false;
    }

    /// The menu's entries in display order.
    #[must_use]
    pub fn entries(&self) -> &[MenuEntry] {
        &self.entries
    }

    /// The number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` when the menu has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Append a launcher entry that starts `launcher` and is shown as `label`,
    /// returning the [`MenuEntryId`] assigned to it.
    ///
    /// The new entry follows every existing one and takes the next id after
    /// the current maximum, so already-assigned ids never move. The taskbar only records the launcher; the caller resolves and
    /// starts the application when the entry is activated.
    pub fn add_launcher(&mut self, launcher: LauncherId, label: impl Into<String>) -> MenuEntryId {
        let id = MenuEntryId(self.next_id());
        self.entries.push(MenuEntry {
            id,
            action: MenuAction::Launch(launcher),
            label: Cow::Owned(label.into()),
        });
        id
    }

    /// Append a light/dark appearance-toggle entry shown as `label`,
    /// returning the [`MenuEntryId`] assigned to it.
    ///
    /// Like [`add_launcher`](Self::add_launcher) the entry follows every
    /// existing one and takes the next free id, so the session controls keep
    /// their fixed ids. Activating it reports
    /// [`MenuAction::ToggleAppearance`]; the taskbar does not own the theme
    /// and performs no switch itself.
    pub fn add_appearance_toggle(&mut self, label: impl Into<String>) -> MenuEntryId {
        let id = MenuEntryId(self.next_id());
        self.entries.push(MenuEntry {
            id,
            action: MenuAction::ToggleAppearance,
            label: Cow::Owned(label.into()),
        });
        id
    }

    /// Activate the entry with `id`, closing the menu, and return its
    /// action. An unknown id changes nothing and returns `None` (fail
    /// closed).
    pub fn activate(&mut self, id: MenuEntryId) -> Option<MenuAction> {
        let action = self.entries.iter().find(|e| e.id == id)?.action;
        self.open = false;
        Some(action)
    }

    /// The next free entry id: one past the current maximum, saturating so a
    /// full id space fails closed rather than wrapping.
    fn next_id(&self) -> u32 {
        self.entries
            .iter()
            .map(|entry| entry.id.0)
            .max()
            .map_or(1, |max| max.saturating_add(1))
    }
}

impl Default for StartMenu {
    fn default() -> Self {
        Self::with_session_controls()
    }
}
