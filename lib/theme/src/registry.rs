//! The runtime theme registry.
//!
//! [`ThemeRegistry`] owns the themes available on a running system and the
//! one that is currently active. It always contains the two built-in
//! themes, so there is always an active theme to return; switching themes
//! at runtime is [`set_active`](ThemeRegistry::set_active), and adding a
//! custom theme is [`register`](ThemeRegistry::register) — data, not code
//! (`AGENTS.md` §10).
//!
//! Both mutators fail closed (`AGENTS.md` §5.4): selecting an unknown theme
//! or registering a duplicate id returns a [`ThemeError`] and leaves the
//! registry unchanged, rather than panicking (`AGENTS.md` §2.9).

use alloc::vec::Vec;

use crate::theme::{Theme, ThemeId};

/// Why a registry mutation was refused.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ThemeError {
    /// [`set_active`](ThemeRegistry::set_active) named a theme that is not
    /// registered.
    UnknownTheme(ThemeId),
    /// [`register`](ThemeRegistry::register) supplied an id that is already
    /// in use (including a built-in id).
    DuplicateId(ThemeId),
}

/// The set of available themes plus the active selection.
///
/// The two built-in themes are held in a fixed-size array so the registry
/// is provably never empty: [`active`](Self::active) can always return a
/// theme without an `unwrap` or an out-of-bounds index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThemeRegistry {
    builtins: [Theme; 2],
    custom: Vec<Theme>,
    active: ThemeId,
}

impl ThemeRegistry {
    /// A registry holding the built-in dark and light themes, with the
    /// dark theme active (RustOS's default, `AGENTS.md` §10).
    #[must_use]
    pub fn with_builtins() -> Self {
        Self {
            builtins: [Theme::dark(), Theme::light()],
            custom: Vec::new(),
            active: ThemeId::DARK,
        }
    }

    /// Register a custom theme.
    ///
    /// # Errors
    ///
    /// Returns [`ThemeError::DuplicateId`] (and registers nothing) if a
    /// theme — built-in or custom — already uses the same [`ThemeId`].
    pub fn register(&mut self, theme: Theme) -> Result<(), ThemeError> {
        if self.get(theme.id()).is_some() {
            return Err(ThemeError::DuplicateId(theme.id()));
        }
        self.custom.push(theme);
        Ok(())
    }

    /// Make the theme with `id` the active one.
    ///
    /// # Errors
    ///
    /// Returns [`ThemeError::UnknownTheme`] (and changes nothing) if no
    /// registered theme has that id.
    pub fn set_active(&mut self, id: ThemeId) -> Result<(), ThemeError> {
        if self.get(id).is_none() {
            return Err(ThemeError::UnknownTheme(id));
        }
        self.active = id;
        Ok(())
    }

    /// The id of the active theme.
    #[must_use]
    pub fn active_id(&self) -> ThemeId {
        self.active
    }

    /// The active theme.
    ///
    /// Never fails: the active id always names a registered theme (the
    /// built-ins are always present and [`set_active`](Self::set_active)
    /// rejects unknown ids), and the fallback to the first built-in keeps
    /// the method total even if a future change broke that invariant.
    #[must_use]
    pub fn active(&self) -> &Theme {
        match self.get(self.active) {
            Some(theme) => theme,
            None => &self.builtins[0],
        }
    }

    /// The theme with `id`, if registered.
    #[must_use]
    pub fn get(&self, id: ThemeId) -> Option<&Theme> {
        self.themes().find(|theme| theme.id() == id)
    }

    /// Every registered theme, built-ins first, then custom themes in
    /// registration order.
    pub fn themes(&self) -> impl Iterator<Item = &Theme> {
        self.builtins.iter().chain(self.custom.iter())
    }

    /// The number of registered themes (always at least two).
    #[must_use]
    pub fn len(&self) -> usize {
        self.builtins.len() + self.custom.len()
    }

    /// Always `false`: a registry always holds the two built-in themes.
    /// Present so Clippy does not flag [`len`](Self::len) as lacking an
    /// `is_empty` companion; it documents the non-empty invariant.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        false
    }
}

impl Default for ThemeRegistry {
    fn default() -> Self {
        Self::with_builtins()
    }
}
