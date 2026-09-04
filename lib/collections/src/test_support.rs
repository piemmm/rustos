//! Shared helpers for this crate's tests.

extern crate std;

use alloc::rc::Rc;
use core::cell::Cell;

/// A value that records its own drop, so a container can be held to exactly
/// one drop per element it ever took — across growth, overwrite, removal,
/// retention, and an abandoned owning iterator.
pub(crate) struct Counted(Rc<Cell<usize>>);

impl Counted {
    /// A fresh drop counter, at zero.
    pub(crate) fn counter() -> Rc<Cell<usize>> {
        Rc::new(Cell::new(0))
    }

    /// A value reporting its drop to `counter`.
    pub(crate) fn new(counter: &Rc<Cell<usize>>) -> Self {
        Self(Rc::clone(counter))
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}
