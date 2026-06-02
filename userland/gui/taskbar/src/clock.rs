//! The taskbar clock label.
//!
//! [`Clock`] holds the *rendered* clock text shown at the trailing end of the
//! bar. Formatting a `Time64` value into a label (and the locale and
//! 12/24-hour policy that drives it) is a caller concern: the taskbar stores
//! only the string to draw, so the bar carries no time ABI of its own
//! (`AGENTS.md` §21 keeps absolute time in `lib/abi`). An empty label draws
//! nothing.

use alloc::string::String;

/// The clock's display label.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Clock {
    label: String,
}

impl Clock {
    /// A clock with no label yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            label: String::new(),
        }
    }

    /// The current display label (empty until set).
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Replace the display label with the caller's already-formatted text.
    pub fn set_label(&mut self, label: impl Into<String>) {
        self.label = label.into();
    }
}
