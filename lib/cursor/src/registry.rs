//! The runtime cursor-set registry.
//!
//! [`CursorRegistry`] owns the cursor sets available on a running system and
//! the one currently active. It always contains the built-in set, so there
//! is always an active [`CursorTheme`] to return; swapping the whole pointer
//! look at runtime is [`set_active`](CursorRegistry::set_active), and adding
//! another set is [`register`](CursorRegistry::register) — data, not code.
//! This is the "replaceable with other cursor sets" requirement (`PLAN.md`
//! Stage 7): a different cursor set is a different `CursorTheme` under a new
//! [`CursorSetId`], with no window-manager change. The desktop session fills
//! one of these from the shipped store (`crate::store`) at bring-up, so
//! activating a set afterwards costs no I/O at all.
//!
//! Both mutators fail closed: selecting an unknown set or
//! registering a duplicate id returns a [`CursorRegistryError`] and leaves
//! the registry unchanged rather than panicking.

use alloc::vec::Vec;

use tairix_theme::{CursorKind, CursorSetId};

use crate::theme::CursorTheme;
use crate::vector::VectorCursor;

/// Why a [`CursorRegistry`] mutation was refused.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CursorRegistryError {
    /// [`set_active`](CursorRegistry::set_active) named a set that is not
    /// registered.
    UnknownSet(CursorSetId),
    /// [`register`](CursorRegistry::register) supplied an id already in use
    /// (including the built-in id).
    DuplicateId(CursorSetId),
}

/// One registered cursor set: its id and its cursors.
#[derive(Clone, Debug, PartialEq)]
struct Entry {
    id: CursorSetId,
    theme: CursorTheme,
}

/// The available cursor sets plus the active selection.
///
/// The built-in set is held in its own field (not the [`Vec`]) so the
/// registry is provably never empty: [`active`](Self::active) always returns
/// a set without an `unwrap` or an out-of-bounds index.
#[derive(Clone, Debug, PartialEq)]
pub struct CursorRegistry {
    builtin: CursorTheme,
    custom: Vec<Entry>,
    active: CursorSetId,
}

impl CursorRegistry {
    /// A registry holding only the built-in set, which is active.
    #[must_use]
    pub fn with_builtin() -> Self {
        Self {
            builtin: CursorTheme::builtin(),
            custom: Vec::new(),
            active: CursorSetId::builtin(),
        }
    }

    /// Register another cursor set under `id`.
    ///
    /// # Errors
    ///
    /// Returns [`CursorRegistryError::DuplicateId`] (and registers nothing)
    /// if a set — built-in or custom — already uses `id`.
    pub fn register(
        &mut self,
        id: CursorSetId,
        theme: CursorTheme,
    ) -> Result<(), CursorRegistryError> {
        if self.get(id).is_some() {
            return Err(CursorRegistryError::DuplicateId(id));
        }
        self.custom.push(Entry { id, theme });
        Ok(())
    }

    /// Make the set with `id` the active one.
    ///
    /// # Errors
    ///
    /// Returns [`CursorRegistryError::UnknownSet`] (and changes nothing) if
    /// no registered set has that id.
    pub fn set_active(&mut self, id: CursorSetId) -> Result<(), CursorRegistryError> {
        if self.get(id).is_none() {
            return Err(CursorRegistryError::UnknownSet(id));
        }
        self.active = id;
        Ok(())
    }

    /// The id of the active set.
    #[must_use]
    pub const fn active_id(&self) -> CursorSetId {
        self.active
    }

    /// The active cursor set.
    ///
    /// Never fails: the active id always names a registered set (the
    /// built-in is always present and [`set_active`](Self::set_active)
    /// rejects unknown ids), and the fallback to the built-in keeps the
    /// method total even if a future change broke that invariant.
    #[must_use]
    pub fn active(&self) -> &CursorTheme {
        self.get(self.active).unwrap_or(&self.builtin)
    }

    /// The active set's cursor for `kind` — the common lookup.
    #[must_use]
    pub fn active_cursor(&self, kind: CursorKind) -> &VectorCursor {
        self.active().cursor(kind)
    }

    /// The set with `id`, if registered.
    #[must_use]
    pub fn get(&self, id: CursorSetId) -> Option<&CursorTheme> {
        if id.is_builtin() {
            return Some(&self.builtin);
        }
        self.custom
            .iter()
            .find(|entry| entry.id == id)
            .map(|entry| &entry.theme)
    }

    /// Every registered set id, built-in first, then custom in registration
    /// order.
    pub fn ids(&self) -> impl Iterator<Item = CursorSetId> + '_ {
        core::iter::once(CursorSetId::builtin()).chain(self.custom.iter().map(|entry| entry.id))
    }

    /// The number of registered sets (always at least one).
    #[must_use]
    pub fn len(&self) -> usize {
        1 + self.custom.len()
    }

    /// Always `false`: a registry always holds the built-in set. Present so
    /// Clippy does not flag [`len`](Self::len) as lacking an `is_empty`
    /// companion; it documents the non-empty invariant.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        false
    }
}

impl Default for CursorRegistry {
    fn default() -> Self {
        Self::with_builtin()
    }
}
