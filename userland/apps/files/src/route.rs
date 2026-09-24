//! Event-routing decisions: which of this process's windows an event names,
//! and which part of a Properties window a key acts on.
//!
//! # Why this is its own module
//!
//! The `Run` binary around it is a freestanding program — it only exists when
//! the crate is built for a bare-metal target — so nothing inside it can be
//! reached by a host test. Resolving an event's window is worth testing: a
//! popup is a window in its own right with its own id, so an id that names one
//! belongs to the window that *holds* it, and only that window knows the two
//! are related.
//!
//! Getting this wrong is silent and total: an id matched against the window
//! list alone resolves to nothing, so every key and every click delivered to a
//! popup is dropped and the popup sits on screen inert. Matching it to its
//! owner without saying *which* of the two it named is the opposite failure —
//! the owner's surface is resized, released, or closed by an event that was
//! never addressed to it.
//!
//! # No I/O, and no authority
//!
//! Nothing here touches a window. It resolves one event and the program acts
//! on the answer.

use tairix_abi::input::{KeyValue, NamedKeyCode};
use tairix_browse::render::PropertiesTab;

/// Which of a window's two surfaces an event named.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Addressed {
    /// The window itself.
    Window,
    /// A popup the window holds.
    Popup,
}

/// The window `target` names, as an index into `windows`, and which of its
/// surfaces it addressed.
///
/// `windows` gives each window's own id and the id of the popup it currently
/// holds, in list order. [`None`] for an id no live window or held popup
/// carries: a window that has just closed, whose events have nowhere to land.
///
/// A window's own id is matched before any popup's, so a process that somehow
/// held a popup claiming a live window's id could never have that window's
/// events diverted.
pub fn addressee<I>(windows: I, target: u64) -> Option<(usize, Addressed)>
where
    I: IntoIterator<Item = (u64, Option<u64>)> + Clone,
{
    let owned = windows
        .clone()
        .into_iter()
        .position(|(window, _)| window == target);
    if let Some(index) = owned {
        return Some((index, Addressed::Window));
    }
    windows
        .into_iter()
        .position(|(_, popup)| popup == Some(target))
        .map(|index| (index, Addressed::Popup))
}

/// What a key press on a Properties window acts on.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PropertiesKey {
    /// Walk the section strip by this many steps.
    Section(i32),
    /// The attributes section's own list cursor and `key = value` editor.
    Attributes,
    /// The permissions section's own cursor, its flags and its ownership
    /// cells.
    Permissions,
    /// Close the window.
    Close,
    /// Nothing the showing section draws.
    Ignored,
}

/// What `key` acts on while `tab` is the section on show, `held` saying
/// whether the permissions section has taken the keyboard from the strip.
///
/// The section strip is the window's own navigation, so it answers on every
/// section until the permissions section takes the keyboard: from then on
/// Left and Right walk that section's flags, and `Escape` hands the keyboard
/// back rather than closing the window. Everything else belongs to the section
/// that draws a control for it — the attributes section's list cursor and its
/// one text surface, the permissions section's cursor — and a section that
/// draws neither must not reach them: typing on General would otherwise fill
/// a field the user cannot see, and a stray arrow would move an invisible
/// cursor and pay for a repaint. Otherwise `Escape` closes the window, since
/// that is the window's own answer and not a section's.
#[must_use]
pub fn properties_key(tab: PropertiesTab, held: bool, key: KeyValue) -> PropertiesKey {
    let permissions = tab == PropertiesTab::Permissions;
    match key {
        _ if permissions && held => PropertiesKey::Permissions,
        KeyValue::Named(NamedKeyCode::Left) => PropertiesKey::Section(-1),
        KeyValue::Named(NamedKeyCode::Right) => PropertiesKey::Section(1),
        _ if tab == PropertiesTab::Attributes => PropertiesKey::Attributes,
        KeyValue::Named(NamedKeyCode::Escape) => PropertiesKey::Close,
        _ if permissions => PropertiesKey::Permissions,
        _ => PropertiesKey::Ignored,
    }
}
