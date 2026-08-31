//! The desktop session: the theme registry and the taskbar model.

use tairix_cursor::CursorTheme;
use tairix_icon::IconSet;
use tairix_taskbar::{Taskbar, TaskbarConfig};
use tairix_theme::{Appearance, Theme, ThemeError, ThemeId, ThemeRegistry};

use crate::assets::{load_cursor_theme, load_icon_set, SessionFileReader};

/// The desktop session: the shared theme registry plus the taskbar model.
///
/// It owns both so a runtime theme switch is a single in-place operation: the
/// registry's active theme changes, the floating form every piece of desktop
/// chrome grounds itself in is re-derived, and the taskbar is re-themed to
/// match. The taskbar holds no authority — its responses are typed reports the
/// embedder (which holds the window-manager, filesystem, and spawn
/// capabilities) acts on.
#[derive(Clone, Debug)]
pub struct DesktopSession {
    themes: ThemeRegistry,
    floating: Theme,
    taskbar: Taskbar,
}

impl DesktopSession {
    /// Build a session for a taskbar placed by `config`, starting from the
    /// built-in themes with the default dark theme active.
    ///
    /// The taskbar comes up with its two permanent leading launchers and an
    /// empty program library; the embedder hands the popup the resolved
    /// catalog once it has read the stores
    /// ([`LibraryPopup::set_catalog`](tairix_taskbar::LibraryPopup::set_catalog)).
    #[must_use]
    pub fn new(config: TaskbarConfig) -> Self {
        let themes = ThemeRegistry::with_builtins();
        let floating = themes.active().clone().floating();
        let taskbar = Taskbar::new(config, &floating);
        Self {
            themes,
            floating,
            taskbar,
        }
    }

    /// The theme registry.
    #[must_use]
    pub const fn themes(&self) -> &ThemeRegistry {
        &self.themes
    }

    /// The active theme, ready to relay to the window manager and apps.
    #[must_use]
    pub fn active_theme(&self) -> &Theme {
        self.themes.active()
    }

    /// The active theme in the *floating* form every piece of desktop chrome is
    /// drawn with: the taskbar, the popups it opens, and every menu plate.
    ///
    /// Derived once here rather than per surface, because the ground is a
    /// property of where a surface is put on screen and the session is what puts
    /// all of these there. One derivation is also what makes a theme switch
    /// unable to leave a surface behind on the ground it had before, and what
    /// keeps a plate's pixels and the row rectangles it is hit-tested against
    /// coming from one theme rather than two.
    #[must_use]
    pub const fn floating_theme(&self) -> &Theme {
        &self.floating
    }

    /// The taskbar model.
    #[must_use]
    pub const fn taskbar(&self) -> &Taskbar {
        &self.taskbar
    }

    /// The taskbar model, mutably (e.g. to update the task list or clock).
    pub fn taskbar_mut(&mut self) -> &mut Taskbar {
        &mut self.taskbar
    }

    /// Make the theme with `id` active and re-theme the taskbar — the
    /// desktop's programmatic theme switch (its interactive home is the
    /// Switchboard's System menu, `plans/NEW-TASKBAR.md` T13).
    ///
    /// # Errors
    ///
    /// Returns [`ThemeError::UnknownTheme`] and changes nothing (neither the
    /// active theme nor the taskbar) if no registered theme has that id.
    pub fn set_theme(&mut self, id: ThemeId) -> Result<(), ThemeError> {
        self.themes.set_active(id)?;
        self.reground();
        Ok(())
    }

    /// Switch the desktop to the built-in theme carrying `appearance` and
    /// re-theme the taskbar — what the system menu's *Light Appearance* and
    /// *Dark Appearance* rows ask for.
    ///
    /// The choice names an appearance rather than a particular theme's
    /// identity, and both built-ins are always registered, so the switch has
    /// no failure mode to surface (contrast [`set_theme`](Self::set_theme),
    /// which can name an unregistered id). Returns the now-active id.
    pub fn set_appearance(&mut self, appearance: Appearance) -> ThemeId {
        let id = self.themes.set_appearance(appearance);
        self.reground();
        id
    }

    /// Load the active theme's cursor set from the on-disk SVG assets under
    /// `/System/Graphics`, ready to register with the window manager's cursor
    /// registry.
    ///
    /// Reads the asset named by the active theme's
    /// [`CursorSet`](tairix_theme::CursorSet) for each cursor kind through
    /// `reader`. It cannot fail: a kind whose asset is missing, unreadable, or
    /// malformed keeps its built-in cursor, so a corrupt or
    /// absent `/System/Graphics` simply yields the built-in cursor set.
    pub fn load_cursors<R>(&self, reader: &mut R) -> CursorTheme
    where
        R: SessionFileReader + ?Sized,
    {
        load_cursor_theme(reader, self.themes.active().cursors())
    }

    /// Load the notification-icon set from the on-disk SVG assets under
    /// `/System/Graphics`, ready to install with the taskbar renderer's
    /// `set_icons`.
    ///
    /// It cannot fail: a kind whose asset is missing, unreadable, or malformed
    /// falls back to its built-in glyph.
    pub fn load_icons<R>(&self, reader: &mut R) -> IconSet
    where
        R: SessionFileReader + ?Sized,
    {
        load_icon_set(reader)
    }

    /// Re-derive the floating chrome theme from the now-active theme and hand it
    /// to the taskbar: the one path every theme switch takes, so no switch can
    /// move one and not the other.
    fn reground(&mut self) {
        self.floating = self.themes.active().clone().floating();
        self.taskbar.apply_theme(&self.floating);
    }

    /// Register a custom theme so it can later be made active.
    ///
    /// # Errors
    ///
    /// Returns [`ThemeError::DuplicateId`] (and registers nothing) if a theme
    /// already uses the same [`ThemeId`].
    pub fn register_theme(&mut self, theme: Theme) -> Result<(), ThemeError> {
        self.themes.register(theme)
    }
}
