//! The runtime theme registry.
//!
//! [`ThemeRegistry`] owns the themes available on a running system and the
//! one that is currently active. It always contains the two built-in
//! themes, so there is always an active theme to return; switching themes
//! at runtime is [`set_active`](ThemeRegistry::set_active), and adding a
//! custom theme is [`register`](ThemeRegistry::register) — data, not code.
//!
//! Both mutators fail closed: selecting an unknown theme
//! or registering a duplicate id returns a [`ThemeError`] and leaves the
//! registry unchanged, rather than panicking.

use alloc::vec::Vec;

use crate::theme::{Accessibility, Appearance, Theme, ThemeId};

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

/// The set of available themes, the active selection, and the accessibility
/// axes laid over it.
///
/// The two built-in themes are held in a fixed-size array so the registry
/// is provably never empty: [`active`](Self::active) can always return a
/// theme without an `unwrap` or an out-of-bounds index.
///
/// [`active`](Self::active) answers the *drawn* theme — the selected one
/// with [`Accessibility`] applied — because every consumer wants the theme
/// as it is on screen and none of them should have to remember to apply the
/// axes itself. The adjusted theme is derived once, whenever the selection
/// or the axes move, rather than per call: `active` is read on every paint
/// and every hit test, and re-deriving a metric table there would put the
/// axis arithmetic on the hot path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThemeRegistry {
    builtins: [Theme; 2],
    custom: Vec<Theme>,
    active: ThemeId,
    axes: Accessibility,
    /// The selected theme with [`axes`](Self::accessibility) applied: what
    /// [`active`](Self::active) answers.
    drawn: Theme,
}

impl ThemeRegistry {
    /// A registry holding the built-in dark and light themes, with the
    /// built-in of the default [`Appearance`] active.
    #[must_use]
    pub fn with_builtins() -> Self {
        let builtins = [Theme::dark(), Theme::light()];
        let active = Self::builtin_for(Appearance::default());
        let drawn = match builtins.iter().find(|theme| theme.id() == active) {
            Some(theme) => theme.clone(),
            None => builtins[0].clone(),
        };
        Self {
            builtins,
            custom: Vec::new(),
            active,
            axes: Accessibility::default(),
            drawn,
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
        self.redraw();
        Ok(())
    }

    /// Lay `axes` over whichever theme is active, returning whether they
    /// changed.
    ///
    /// This is the runtime contrast / density / reduced-motion control's
    /// primitive, and the counterpart of
    /// [`set_appearance`](Self::set_appearance): the axes belong to the
    /// desktop rather than to a theme, so they survive a theme switch and a
    /// custom theme gets them too.
    pub fn set_accessibility(&mut self, axes: Accessibility) -> bool {
        if self.axes == axes {
            return false;
        }
        self.axes = axes;
        self.redraw();
        true
    }

    /// The accessibility axes laid over the active theme.
    #[must_use]
    pub const fn accessibility(&self) -> Accessibility {
        self.axes
    }

    /// Re-derive the drawn theme from the selection and the axes.
    fn redraw(&mut self) {
        let selected = match self.get(self.active) {
            Some(theme) => theme,
            None => &self.builtins[0],
        };
        self.drawn = selected.clone().with_axes(self.axes);
    }

    /// The id of the active theme.
    #[must_use]
    pub fn active_id(&self) -> ThemeId {
        self.active
    }

    /// Make the built-in theme of the given [`Appearance`] the active one,
    /// returning its id.
    ///
    /// This is the runtime light/dark control's primitive.
    /// The two built-ins are always present, so selecting one by appearance
    /// always succeeds — there is no failure mode to surface (contrast
    /// [`set_active`](Self::set_active), which can name an unregistered id).
    /// A custom theme that happens to be active is replaced by the matching
    /// built-in.
    pub fn set_appearance(&mut self, appearance: Appearance) -> ThemeId {
        let id = Self::builtin_for(appearance);
        self.active = id;
        self.redraw();
        id
    }

    /// Switch between the built-in light and dark themes, returning the
    /// now-active id.
    ///
    /// The toggle is driven by the *active* theme's [`Appearance`]: a dark
    /// theme (built-in or custom) switches to the light built-in and a light
    /// theme to the dark built-in. This is exactly what a "switch to
    /// light/dark" desktop control does.
    pub fn toggle_appearance(&mut self) -> ThemeId {
        let next = match self.active().appearance() {
            Appearance::Dark => Appearance::Light,
            Appearance::Light => Appearance::Dark,
        };
        self.set_appearance(next)
    }

    /// The id of the built-in theme for an [`Appearance`].
    const fn builtin_for(appearance: Appearance) -> ThemeId {
        match appearance {
            Appearance::Dark => ThemeId::DARK,
            Appearance::Light => ThemeId::LIGHT,
        }
    }

    /// The active theme **as it is drawn**: the selected one with the
    /// accessibility axes applied.
    ///
    /// Never fails, and never needs to: the drawn theme is derived and held
    /// whenever the selection or the axes move, so there is nothing here to
    /// look up or fall back from.
    #[must_use]
    pub const fn active(&self) -> &Theme {
        &self.drawn
    }

    /// The active theme as it was *registered*, with no accessibility axes
    /// applied.
    ///
    /// What a surface that is editing the axes reads, so it shows what the
    /// theme declares rather than what the current axes already did to it.
    #[must_use]
    pub fn selected(&self) -> &Theme {
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
