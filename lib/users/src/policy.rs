//! Shared account-authoring policy: the defaults every creator of an
//! account or group record composes from.
//!
//! More than one program authors records into the databases under
//! `/System/Security` — the interactive `users` session, the one-shot
//! `useradd`/`groupadd` commands, the image builder, and the installer.
//! The values they must agree on (the login shell every interactive
//! account starts, the home-directory layout, and how a free numeric id
//! is chosen when the caller does not name one) are policy, not
//! per-author choice, so they are defined here once beside the record
//! format and the grant sets ([`crate::grants`]) and imported everywhere,
//! never copy-pasted.

use alloc::format;
use alloc::string::String;

/// The login shell a created interactive account starts: the `Run` binary
/// of the default shell's application bundle in the system app store.
///
/// The spelling is pinned to the store layout constants
/// (`rustos_abi::SYSTEM_APP_STORE`, `rustos_abi::BUNDLE_SUFFIX`) by a unit
/// test below, so it cannot drift from where the image builder plants the
/// bundle.
pub const DEFAULT_SHELL: &str = "/System/Apps/elsh.app/Run";

/// The default home directory for the account named `username`: the
/// `/Users/<name>` layout the installed-system contract fixes.
#[must_use]
pub fn default_home(username: &str) -> String {
    format!("/Users/{username}")
}

/// Allocate the next free numeric id given every id already `taken`:
/// one greater than the highest existing id, or `0` for an empty
/// database.
///
/// Never re-using an id below the current maximum keeps a deleted
/// account's id out of circulation while any higher account exists, so
/// on-disk objects owned by a removed principal are not silently
/// inherited by an unrelated new one. The allocation is deterministic —
/// the database is the single authority on collisions, so two racing
/// creations resolve to exactly one winner there.
///
/// Returns [`None`] when the id space is exhausted (the highest taken id
/// is `u32::MAX`); the caller fails closed rather than wrapping.
#[must_use]
pub fn next_id(taken: impl IntoIterator<Item = u32>) -> Option<u32> {
    match taken.into_iter().max() {
        None => Some(0),
        Some(highest) => highest.checked_add(1),
    }
}

#[cfg(test)]
mod tests {
    use super::{default_home, next_id, DEFAULT_SHELL};
    use alloc::format;
    use rustos_abi::{BUNDLE_SUFFIX, SYSTEM_APP_STORE};

    #[test]
    fn the_default_shell_is_the_store_bundle_run_binary() {
        assert_eq!(
            DEFAULT_SHELL,
            format!("{SYSTEM_APP_STORE}/elsh{BUNDLE_SUFFIX}/Run")
        );
    }

    #[test]
    fn the_default_home_follows_the_users_layout() {
        assert_eq!(default_home("alice"), "/Users/alice");
    }

    #[test]
    fn an_empty_database_allocates_id_zero() {
        assert_eq!(next_id([]), Some(0));
    }

    #[test]
    fn the_next_id_is_one_above_the_highest_taken() {
        assert_eq!(next_id([0, 5, 3]), Some(6));
        // Gaps below the maximum are deliberately not re-used.
        assert_eq!(next_id([0, 2]), Some(3));
    }

    #[test]
    fn an_exhausted_id_space_fails_closed() {
        assert_eq!(next_id([0, u32::MAX]), None);
    }
}
