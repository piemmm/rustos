//! How the window follows the directory it is showing.
//!
//! Two decisions the graphical loop makes live here rather than inside the
//! freestanding-only program module, so both are exercised on the host: the
//! title the window carries, and what a close gesture on the window's Close
//! control means.

use alloc::string::String;

use tairix_abi::window_ipc::WINDOW_TITLE_MAX;
use tairix_browse::vfs::{spell_absolute_path, spell_title_location};
use tairix_browse::{Browser, DirectorySource};

/// What the window must do after the desktop reports a *secondary* press on
/// its Close control.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Leave {
    /// The browser moved to the parent directory; repaint the window.
    Climbed,
    /// There was no parent left, so the window closes.
    Closed,
    /// The parent could not be listed; the browser stayed where it was and the
    /// window stays open. The text names the place that was refused, for the
    /// program's one fail-loud reporting path.
    Refused(String),
}

/// Leave the current directory for its parent, reporting what the window must
/// do.
///
/// The gesture means "leave this folder", not "leave this window", so it
/// closes only at the top where there is nothing left to leave. A parent that
/// cannot be listed is refused rather than read as the top: an unreadable
/// ancestor must not destroy the window (fail closed). Climbing is the
/// engine's own transactional navigation, so a refusal leaves the listing and
/// the history exactly as they were.
pub fn leave_directory<S: DirectorySource>(browser: &mut Browser<S>) -> Leave {
    match browser.go_up() {
        Ok(true) => Leave::Climbed,
        Ok(false) => Leave::Closed,
        Err(_) => Leave::Refused(refusal(browser)),
    }
}

/// What the user is told when the parent could not be listed: the place the
/// climb was refused, spelled the one way every path is spelled.
///
/// The browser has not moved, so the parent is the location less its leaf. A
/// refusal only arises where there *was* a parent; spelling from the split
/// keeps that an observation rather than an assumption.
fn refusal<S: DirectorySource>(browser: &Browser<S>) -> String {
    let parent = browser.components().split_last().map_or_else(
        || spell_absolute_path(&[]),
        |(_, rest)| spell_absolute_path(rest),
    );
    alloc::format!("could not open {parent}")
}

/// The title the window carries while it shows `browser`'s location.
///
/// The title *is* the location, fitted to the protocol's bounded title field by
/// the shared spelling — so a path too long for the field is shortened the one
/// way every surface shortens text, never refused.
#[must_use]
pub fn location_title<S: DirectorySource>(browser: &Browser<S>) -> String {
    spell_title_location(browser.components(), WINDOW_TITLE_MAX)
}

/// The title the window should carry, or `None` when `shown` — the text the
/// session was last told — already carries it.
///
/// The location changes with a navigation and with nothing else, so comparing
/// against what was last sent keeps a repaint that did not move (a selection, a
/// scroll, a resize, a theme switch) off the window channel.
#[must_use]
pub fn retitle<S: DirectorySource>(browser: &Browser<S>, shown: &str) -> Option<String> {
    let wanted = location_title(browser);
    (wanted != shown).then_some(wanted)
}

#[cfg(test)]
#[path = "location_tests.rs"]
mod tests;
