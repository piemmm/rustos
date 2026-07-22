//! Activating an entry: the one dispatch-by-kind decision the file manager
//! and the trusted picker share.
//!
//! "Activating" is what a double-click — or `Enter` on the selection — means.
//! The browser cannot decide it in the app, because the file manager
//! (`userland/apps/files`) and the desktop session's trusted picker both
//! compose the same [`Browser`](crate::Browser) and must dispatch identically;
//! so the decision lives here, once, and every consumer acts on the same
//! [`Activation`].
//!
//! The decision is exhaustive over the three entry kinds
//! ([`EntryKind`](crate::entry::EntryKind)): a directory is *descended into* by
//! the engine itself (the browser's own transactional navigation); an
//! application bundle is *named* for the caller to launch through the ordinary
//! signed app-load gate; a regular file is *named* for the caller to open in
//! the associated viewer. The engine holds no launch or open authority of its
//! own — it decides *what* the target is and *what should happen*, never *does*
//! the spawn or the `fs_open`: those stay in the app's own capability-checked
//! tail, under the launching user's identity, so composing this engine grants
//! nothing (the read-only picker never launches at all).

use alloc::string::String;

/// What activating an entry does, decided once by the shared engine.
///
/// Returned by [`Browser::activate_selected`](crate::Browser::activate_selected)
/// / [`Browser::activate_index`](crate::Browser::activate_index). The `path`
/// variants carry the target's validated absolute path (the same spelling the
/// VFS fetch uses, so a launch or open can never name a different node than the
/// browser shows); the engine performs no launch or open itself.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Activation {
    /// The entry was a directory and the browser descended into it: the
    /// listing changed and the caller repaints. The descent is the engine's
    /// own fail-closed navigation (an unreadable target leaves the browser
    /// where it was and surfaces the error instead), so there is nothing for
    /// the caller to launch.
    Descended,
    /// The entry is an application bundle (`<Name>.app`) at `path`: the caller
    /// launches it through the ordinary signed app-load gate — never a private
    /// path. The engine only names the bundle directory.
    LaunchBundle {
        /// The bundle directory's validated absolute path.
        path: String,
    },
    /// The entry is a regular file at `path`: the caller opens it in the
    /// associated viewer, handing it a read-only descriptor. The engine only
    /// names the file; it opens nothing itself.
    OpenFile {
        /// The file's validated absolute path.
        path: String,
    },
}
