//! The taskbar clock label.
//!
//! [`Clock`] holds the *rendered* clock text shown at the trailing end of the
//! bar. Formatting a `Time64` value into a label (and the locale and
//! 12/24-hour policy that drives it) is a caller concern: the taskbar stores
//! only the string to draw, so the bar carries no time ABI of its own
//! (keeps absolute time in `lib/abi`). An empty label draws
//! nothing.
//!
//! A clock the user can press is how a time gets *set*, so a machine with no
//! wall time must still show one: [`UNSET_LABEL`] is what a caller draws
//! then, and the menu's heading reads the same constant to say the time is
//! unset.

use alloc::string::String;

/// The label a caller sets when no wall-clock time has been established.
///
/// `HH:mm`-shaped, so the clock keeps its place and its width, and
/// unmistakably not a reading. The one definition of that spelling: the
/// session spells an unset reading as this, and
/// [`clock_menu`](crate::clock_menu) reads it back to state that the time is
/// unset.
pub const UNSET_LABEL: &str = "--:--";

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
