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

/// Permission mode a home directory, and every directory the OS creates
/// inside it, is stamped with: owner-only.
///
/// A home is private by construction, so an account's files are unreadable
/// to every other ordinary principal without needing per-file hardening.
/// Every route that provisions a home — the account-administration path,
/// the image builder, the test fixtures — stamps this one value, so none of
/// them can quietly leave a home world-readable.
pub const HOME_MODE: u32 = 0o700;

/// The fixed shape of a home directory: the subdirectories provisioning
/// creates inside `/Users/<name>`.
///
/// The installed-system contract fixes this set and forbids an application
/// inventing a sibling beside it. They are created **with** the account
/// rather than on first use, because the write paths that land in them are
/// one level deeper — a per-user settings store under `Settings/`, an
/// app-scoped cache under `Library/`, the user's own bundles under `Apps/`
/// — and a writer that creates only its immediate parent would fail on a
/// brand-new account the first time anything was saved.
///
/// Sorted and duplicate-free, so a fresh home lists deterministically.
pub const HOME_SUBDIRS: [&str; 5] = ["Apps", "Desktop", "Documents", "Library", "Settings"];

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
    use super::{
        default_home, next_id, IdRange, DEFAULT_SHELL, FIRST_USER_UID, HOME_MODE, HOME_SUBDIRS,
    };
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

    /// Each entry must be a single, plain directory name: a component
    /// carrying a separator, a dot segment, or nothing at all would have a
    /// provisioner creating something other than a child of the home.
    #[test]
    fn every_home_subdirectory_is_one_plain_component() {
        for name in HOME_SUBDIRS {
            assert!(!name.is_empty(), "{name:?} is empty");
            assert!(!name.contains('/'), "{name:?} is not one component");
            assert!(name != "." && name != "..", "{name:?} is a dot segment");
        }
    }

    /// Sorted and duplicate-free: a provisioner walks the table in order,
    /// so a repeat would try to create the same child twice and an
    /// out-of-order entry would list a fresh home unpredictably.
    #[test]
    fn the_home_shape_is_sorted_and_free_of_duplicates() {
        let mut sorted = HOME_SUBDIRS;
        sorted.sort_unstable();
        assert_eq!(sorted, HOME_SUBDIRS, "the table is authored in order");
        for (index, name) in HOME_SUBDIRS.iter().enumerate() {
            assert!(
                !HOME_SUBDIRS[index + 1..].contains(name),
                "{name:?} appears twice"
            );
        }
    }

    /// A home is reachable by its owner alone: no group or other bits.
    #[test]
    fn a_home_is_owner_only() {
        assert_eq!(HOME_MODE & 0o077, 0, "no group or other access");
        assert_eq!(HOME_MODE & 0o700, 0o700, "the owner may enter and write");
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
