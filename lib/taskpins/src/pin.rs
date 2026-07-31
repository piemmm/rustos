//! The validated model of pinned shortcuts.
//!
//! A pin list ([`PinList`]) is an ordered sequence of targets
//! ([`PinTarget`]): either a program-library catalog entry ([`EntryId`])
//! or a direct application bundle ([`BundlePath`]). The model enforces
//! uniqueness — a target is either pinned or it is not — and capacity
//! ([`MAX_PINS`]), so a list can never exceed the taskbar's display bound.

use alloc::vec::Vec;
use core::fmt;

use tairix_proglib::{BundlePath, EntryId};

use crate::store::MAX_PINS;

/// The target of a pinned shortcut.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PinTarget {
    /// A pin referencing a program-library catalog entry.
    Entry(EntryId),
    /// A direct pin of an application bundle that is not catalogued.
    Bundle(BundlePath),
}

/// Why a pin operation was refused.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PinError {
    /// The target is already in the list.
    AlreadyPinned,
    /// The list has reached [`MAX_PINS`].
    Full,
}

impl fmt::Display for PinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyPinned => f.write_str("target is already pinned"),
            Self::Full => f.write_str("pin list is full"),
        }
    }
}

/// An ordered list of pinned shortcuts.
///
/// Order is the user's display order, preserved exactly. The list enforces
/// uniqueness ([`PinError::AlreadyPinned`]) and capacity ([`MAX_PINS`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PinList {
    pins: Vec<PinTarget>,
}

impl PinList {
    /// An empty pin list.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append `target` to the end of the list.
    ///
    /// # Errors
    ///
    /// [`PinError::AlreadyPinned`] if the target is already present;
    /// [`PinError::Full`] if the list is at [`MAX_PINS`].
    pub fn pin(&mut self, target: PinTarget) -> Result<usize, PinError> {
        if self.position(&target).is_some() {
            return Err(PinError::AlreadyPinned);
        }
        if self.pins.len() >= MAX_PINS {
            return Err(PinError::Full);
        }
        let index = self.pins.len();
        self.pins.push(target);
        Ok(index)
    }

    /// Insert `target` at `index`.
    ///
    /// If `index` is greater than or equal to [`Self::len()`], the pin is
    /// appended to the end.
    ///
    /// # Errors
    ///
    /// [`PinError::AlreadyPinned`] if the target is already present;
    /// [`PinError::Full`] if the list is at [`MAX_PINS`].
    pub fn pin_at(&mut self, index: usize, target: PinTarget) -> Result<(), PinError> {
        if self.position(&target).is_some() {
            return Err(PinError::AlreadyPinned);
        }
        if self.pins.len() >= MAX_PINS {
            return Err(PinError::Full);
        }
        let index = core::cmp::min(index, self.pins.len());
        self.pins.insert(index, target);
        Ok(())
    }

    /// Remove the pin at `index`.
    ///
    /// Returns the removed target, or `None` if `index` was out of range.
    /// Fails closed (no panic).
    pub fn unpin(&mut self, index: usize) -> Option<PinTarget> {
        if index < self.pins.len() {
            Some(self.pins.remove(index))
        } else {
            None
        }
    }

    /// Move a pin from `from` to `to`.
    ///
    /// The move follows the "remove then insert at clamped destination"
    /// model: the pin at `from` is removed, then inserted at `to` (clamped
    /// to the new length).
    ///
    /// Returns `false` when `from` is out of range.
    pub fn move_pin(&mut self, from: usize, to: usize) -> bool {
        if from >= self.pins.len() {
            return false;
        }
        let target = self.pins.remove(from);
        let to = core::cmp::min(to, self.pins.len());
        self.pins.insert(to, target);
        true
    }

    /// The index of `target`, if it is pinned.
    #[must_use]
    pub fn position(&self, target: &PinTarget) -> Option<usize> {
        self.pins.iter().position(|p| p == target)
    }

    /// The target at `index`.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&PinTarget> {
        self.pins.get(index)
    }

    /// An iterator over the pinned targets, in display order.
    pub fn iter(&self) -> core::slice::Iter<'_, PinTarget> {
        self.pins.iter()
    }

    /// The number of pinned shortcuts.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pins.len()
    }

    /// Whether the list is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pins.is_empty()
    }
}

impl<'a> IntoIterator for &'a PinList {
    type Item = &'a PinTarget;
    type IntoIter = core::slice::Iter<'a, PinTarget>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

#[cfg(test)]
#[path = "pin_tests.rs"]
mod tests;
