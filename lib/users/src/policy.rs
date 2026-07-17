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
/// (`tairix_abi::SYSTEM_APP_STORE`, `tairix_abi::BUNDLE_SUFFIX`) by a unit
/// test below, so it cannot drift from where the image builder plants the
/// bundle.
pub const DEFAULT_SHELL: &str = "/System/Apps/elsh.app/Run";

/// The default home directory for the account named `username`: the
/// `/Users/<name>` layout the installed-system contract fixes.
#[must_use]
pub fn default_home(username: &str) -> String {
    format!("/Users/{username}")
}

/// First uid the interactive-user range starts at; everything below is
/// reserved for the system account (`uid 0`) and the service accounts.
pub const FIRST_USER_UID: u32 = 1000;

/// First gid the interactive-user range starts at; everything below is
/// reserved for the system groups (`system`, `services`, `storage`, …).
pub const FIRST_USER_GID: u32 = 1000;

/// Which reserved id band an allocation draws from.
///
/// System uids/gids occupy `0..`[`FIRST_USER_UID`]; interactive users
/// start at [`FIRST_USER_UID`]. The split keeps a service identity
/// visually and mechanically distinct from a person in every listing,
/// log line, and audit record.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum IdRange {
    /// The reserved system/service band, `0..=999`.
    System,
    /// The interactive-user band, `1000..=u32::MAX`.
    User,
}

impl IdRange {
    /// The inclusive `(first, last)` ids of the band.
    const fn bounds(self) -> (u32, u32) {
        match self {
            Self::System => (0, FIRST_USER_UID - 1),
            Self::User => (FIRST_USER_UID, u32::MAX),
        }
    }

    /// Whether `id` falls inside the band.
    #[must_use]
    pub fn contains(self, id: u32) -> bool {
        let (first, last) = self.bounds();
        id >= first && id <= last
    }
}

/// Allocate the next free numeric id inside `range`, given every id
/// already `taken` (ids outside the band are ignored): one greater than
/// the highest taken id in the band, or the band's first id when it is
/// empty.
///
/// Never re-using an id below the band's current maximum keeps a deleted
/// account's id out of circulation while any higher account exists, so
/// on-disk objects owned by a removed principal are not silently
/// inherited by an unrelated new one. The allocation is deterministic —
/// the database is the single authority on collisions, so two racing
/// creations resolve to exactly one winner there.
///
/// Returns [`None`] when the band is exhausted (its highest id is
/// taken); the caller fails closed rather than wrapping or spilling into
/// the neighbouring band.
#[must_use]
pub fn next_id(range: IdRange, taken: impl IntoIterator<Item = u32>) -> Option<u32> {
    let (first, last) = range.bounds();
    match taken.into_iter().filter(|id| range.contains(*id)).max() {
        None => Some(first),
        Some(highest) if highest == last => None,
        Some(highest) => Some(highest + 1),
    }
}

#[cfg(test)]
mod tests {
    use super::{default_home, next_id, IdRange, DEFAULT_SHELL, FIRST_USER_UID};
    use alloc::format;
    use tairix_abi::{BUNDLE_SUFFIX, SYSTEM_APP_STORE};

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
    fn an_empty_band_allocates_its_first_id() {
        assert_eq!(next_id(IdRange::System, []), Some(0));
        assert_eq!(next_id(IdRange::User, []), Some(FIRST_USER_UID));
    }

    #[test]
    fn the_next_id_is_one_above_the_highest_taken_in_the_band() {
        assert_eq!(next_id(IdRange::System, [0, 5, 3]), Some(6));
        // Gaps below the maximum are deliberately not re-used.
        assert_eq!(next_id(IdRange::System, [0, 2]), Some(3));
        assert_eq!(next_id(IdRange::User, [1000, 1004]), Some(1005));
    }

    #[test]
    fn ids_outside_the_band_do_not_steer_the_allocation() {
        // A user-range allocation ignores the system accounts entirely…
        assert_eq!(next_id(IdRange::User, [0, 10, 999]), Some(FIRST_USER_UID));
        // …and a system-range allocation ignores the interactive users.
        assert_eq!(next_id(IdRange::System, [0, 10, 1000, 5000]), Some(11));
    }

    #[test]
    fn an_exhausted_band_fails_closed_without_spilling() {
        assert_eq!(next_id(IdRange::System, [FIRST_USER_UID - 1]), None);
        assert_eq!(next_id(IdRange::User, [u32::MAX]), None);
    }

    #[test]
    fn the_band_split_is_pinned() {
        assert!(IdRange::System.contains(0));
        assert!(IdRange::System.contains(999));
        assert!(!IdRange::System.contains(FIRST_USER_UID));
        assert!(IdRange::User.contains(FIRST_USER_UID));
        assert!(IdRange::User.contains(u32::MAX));
        assert!(!IdRange::User.contains(999));
    }
}
