//! The desktop session: the theme registry, the taskbar, and the glue that
//! resolves taskbar responses into session-level events.

use rustos_taskbar::{MenuAction, Taskbar, TaskbarConfig, TaskbarResponse};
use rustos_theme::{Theme, ThemeError, ThemeId, ThemeRegistry};

/// What a resolved [`TaskbarResponse`] means to the rest of the desktop.
///
/// The session acts on exactly one taskbar response — the appearance toggle —
/// because that is the one whose effect lives in the session's own state (the
/// [`ThemeRegistry`]). Everything else needs capabilities the session does not
/// hold (a window-manager handle, the power/spawn capabilities) and is
/// [`Forward`](SessionEvent::Forward)ed to the embedder unchanged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionEvent {
    /// The session switched the desktop appearance and re-themed the taskbar.
    /// The embedder relays the now-active theme — read with
    /// [`DesktopSession::active_theme`] — to the window manager and apps.
    AppearanceChanged(ThemeId),
    /// The session did not act on this response; the embedder handles it
    /// (drive the window manager, perform a session control, launch an app).
    Forward(TaskbarResponse),
}

/// The desktop session: the shared theme registry plus the taskbar model.
///
/// It owns both so a runtime theme switch is a single in-place operation: the
/// registry's active theme changes and the taskbar is re-themed to match,
/// through one private apply path so the relay is never duplicated
/// (`AGENTS.md` §2.2).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesktopSession {
    themes: ThemeRegistry,
    taskbar: Taskbar,
}

impl DesktopSession {
    /// Build a session for a taskbar placed by `config`, starting from the
    /// built-in themes with the default dark theme active (`AGENTS.md` §10).
    ///
    /// The start menu is seeded with its session controls and a light/dark
    /// appearance-toggle entry labelled `appearance_label`, so the toggle is
    /// reachable from the menu the moment the desktop comes up.
    #[must_use]
    pub fn new(config: TaskbarConfig, appearance_label: &str) -> Self {
        let themes = ThemeRegistry::with_builtins();
        let mut taskbar = Taskbar::new(config, themes.active());
        taskbar
            .start_menu_mut()
            .add_appearance_toggle(appearance_label);
        Self { themes, taskbar }
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

    /// The taskbar model.
    #[must_use]
    pub const fn taskbar(&self) -> &Taskbar {
        &self.taskbar
    }

    /// The taskbar model, mutably (e.g. to update the task list or clock).
    pub fn taskbar_mut(&mut self) -> &mut Taskbar {
        &mut self.taskbar
    }

    /// Resolve one taskbar response, performing the appearance toggle itself
    /// and forwarding everything else.
    ///
    /// Selecting the start menu's appearance-toggle entry switches the
    /// built-in light/dark theme on the registry, re-themes the taskbar, and
    /// returns [`SessionEvent::AppearanceChanged`] with the now-active
    /// [`ThemeId`]. Any other response — including a launcher or
    /// session-control selection, which the session has no capability to act
    /// on — is [`SessionEvent::Forward`]ed unchanged (`AGENTS.md` §10, §16.5).
    pub fn resolve(&mut self, response: TaskbarResponse) -> SessionEvent {
        if let TaskbarResponse::MenuEntrySelected {
            action: MenuAction::ToggleAppearance,
            ..
        } = response
        {
            return SessionEvent::AppearanceChanged(self.toggle_appearance());
        }
        SessionEvent::Forward(response)
    }

    /// Switch the desktop between its built-in light and dark themes, returning
    /// the now-active [`ThemeId`] and re-theming the taskbar.
    ///
    /// The switch is driven by the *active* theme's appearance, so a custom
    /// dark theme toggles to the built-in light theme and vice versa
    /// (`AGENTS.md` §10). It cannot fail: the two built-ins are always present.
    pub fn toggle_appearance(&mut self) -> ThemeId {
        let id = self.themes.toggle_appearance();
        self.reapply_theme();
        id
    }

    /// Make the theme with `id` active and re-theme the taskbar.
    ///
    /// # Errors
    ///
    /// Returns [`ThemeError::UnknownTheme`] and changes nothing (neither the
    /// active theme nor the taskbar) if no registered theme has that id
    /// (`AGENTS.md` §5.4 / §2.9).
    pub fn set_theme(&mut self, id: ThemeId) -> Result<(), ThemeError> {
        self.themes.set_active(id)?;
        self.reapply_theme();
        Ok(())
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

    /// Re-theme the taskbar from the active theme. The single place a theme
    /// switch reaches the taskbar, so the relay is not duplicated between
    /// [`toggle_appearance`](Self::toggle_appearance) and
    /// [`set_theme`](Self::set_theme) (`AGENTS.md` §2.2).
    fn reapply_theme(&mut self) {
        self.taskbar.apply_theme(self.themes.active());
    }
}
