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

use tairix_abi::driver::filesystem::{NodeSecurity, SecurityAcl, SecuritySubject};
use tairix_abi::driver::DriverError;
use tairix_abi::CapabilityId;

use crate::provision::{CONFD_UID, SERVICES_GID};

/// The login shell a created interactive account starts: the `Run` binary
/// of the default shell's application bundle in the system command store.
///
/// The spelling is pinned to the store layout constants
/// (`tairix_abi::SYSTEM_COMMAND_STORE`, `tairix_abi::BUNDLE_SUFFIX`) by a
/// unit test below, so it cannot drift from where the image builder plants
/// the bundle.
pub const DEFAULT_SHELL: &str = "/System/Commands/elsh.app/Run";

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
/// app-scoped cache under `Library/`, the user's own bundles under
/// `Commands/` and `Applications/` — and a writer that creates only its
/// immediate parent would fail on a brand-new account the first time
/// anything was saved.
///
/// The user's own two program stores are the third and fourth directories a
/// bare command word is resolved against (`tairix_cmdres`), so provisioning
/// them with the account is what makes a user's own commands typeable
/// without any `PATH` edit. Their names are pinned to the shared store
/// definitions by a unit test below.
///
/// Sorted and duplicate-free, so a fresh home lists deterministically.
pub const HOME_SUBDIRS: [&str; 6] = [
    "Applications",
    "Commands",
    "Desktop",
    "Documents",
    "Library",
    "Settings",
];

/// Directory name of the **gated per-app data root** inside each of
/// [`APPDATA_ROOT_PARENTS`]: `Settings/Apps/<bundle-id>/` holds an
/// application's configuration and `Library/Apps/<bundle-id>/` its bulk and
/// volatile data.
///
/// A per-app directory cannot be the gate itself. All of a user's
/// applications run as that one user and may write `Settings/`, so any of
/// them could pre-create a sibling named after another app's bundle id and
/// have the app-data service walk into it. The gate therefore sits on this
/// one fixed parent, which the service owns and no application can create,
/// rename, or remove ([`appdata_root_security`]).
pub const APPDATA_ROOT: &str = "Apps";

/// The home subdirectories that hold a gated per-app data root, in
/// [`HOME_SUBDIRS`] order.
///
/// Configuration lives under `Settings/` and bulk, cache, and temporary data
/// under `Library/`, matching the installed-system contract's own split
/// rather than inventing a third home directory for app data.
pub const APPDATA_ROOT_PARENTS: [&str; 2] = ["Library", "Settings"];

/// The security record a gated per-app data root
/// (`<home>/{Library,Settings}/Apps`) carries.
///
/// Owned by the app-data service account, owner-only, and gated on
/// `CAP_APPDATA_ADMIN` — so the owning user fails the mode check *and* the
/// capability check, and the service passes both. Both halves matter: the
/// capability is what an application with the user's uid cannot obtain, and
/// the ownership is what stops an application that somehow created the
/// directory first from being believed, because a decoy it owns is
/// unreadable to the service.
#[must_use]
pub const fn appdata_root_security() -> NodeSecurity {
    let mut sec = NodeSecurity::new(HOME_MODE, CONFD_UID.0, SERVICES_GID.0);
    sec.required_cap = Some(CapabilityId::APPDATA_ADMIN);
    sec
}

/// The security record a home directory carries when the app-data service
/// must be able to *traverse* it: owner-only as ever, plus a search-only
/// grant to the service's uid.
///
/// Stamped on the home itself and on each [`APPDATA_ROOT_PARENTS`] entry,
/// because a walk to the gated root needs search permission on every
/// directory it descends into and those are owned by the user, not the
/// service. Search alone is the least authority that works: it cannot list
/// the directory and it cannot open any child whose own record refuses it, so
/// the service still reaches nothing but the root it owns.
///
/// # Errors
///
/// [`DriverError::LengthOutOfRange`] if the record cannot hold another ACL
/// entry — structurally impossible for the single entry added here.
pub fn appdata_transit_security(uid: u32, gid: u32) -> Result<NodeSecurity, DriverError> {
    let mut sec = NodeSecurity::new(HOME_MODE, uid, gid);
    sec.push_acl(SecurityAcl {
        subject: SecuritySubject::User(CONFD_UID.0),
        perms: SEARCH_ONLY,
    })?;
    Ok(sec)
}

/// The `rwx` triad granting search (execute) and nothing else.
const SEARCH_ONLY: u8 = 0b001;

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
        appdata_root_security, appdata_transit_security, default_home, next_id, IdRange,
        APPDATA_ROOT, APPDATA_ROOT_PARENTS, DEFAULT_SHELL, FIRST_USER_UID, HOME_MODE, HOME_SUBDIRS,
        SEARCH_ONLY,
    };
    use crate::provision::{CONFD_UID, SERVICES_GID};
    use alloc::format;
    use tairix_abi::driver::filesystem::{SecurityAcl, SecuritySubject};
    use tairix_abi::{
        CapabilityId, BUNDLE_SUFFIX, HOME_APPLICATION_STORE_DIR, HOME_COMMAND_STORE_DIR,
        SYSTEM_COMMAND_STORE,
    };

    #[test]
    fn the_default_shell_is_the_store_bundle_run_binary() {
        assert_eq!(
            DEFAULT_SHELL,
            format!("{SYSTEM_COMMAND_STORE}/elsh{BUNDLE_SUFFIX}/Run")
        );
    }

    /// A provisioned home carries the user's own two program stores under
    /// exactly the names command resolution searches; a drift here would
    /// leave a fresh account's own commands unreachable.
    #[test]
    fn a_provisioned_home_carries_the_users_own_program_stores() {
        for store in [HOME_COMMAND_STORE_DIR, HOME_APPLICATION_STORE_DIR] {
            assert!(HOME_SUBDIRS.contains(&store), "{store} is not provisioned");
        }
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
    /// The gated roots hang off directories the home shape already
    /// provisions: a parent a provisioner never creates would leave the
    /// service with nowhere to put an app's data.
    #[test]
    fn every_app_data_root_hangs_off_a_provisioned_home_subdirectory() {
        for parent in APPDATA_ROOT_PARENTS {
            assert!(
                HOME_SUBDIRS.contains(&parent),
                "{parent} is not provisioned"
            );
        }
        assert!(!APPDATA_ROOT.is_empty());
        assert!(!APPDATA_ROOT.contains('/'));
        assert!(APPDATA_ROOT != "." && APPDATA_ROOT != "..");
        // The root must not collide with a home subdirectory name: it lives
        // one level below them, and a reader must not confuse the two.
        assert!(!HOME_SUBDIRS.contains(&APPDATA_ROOT));
    }

    /// The gate is the whole point: the owning user must fail *both* the
    /// capability check and the mode check, and the record must name the
    /// service as owner so a decoy an application created is unreadable to
    /// the service rather than trusted.
    #[test]
    fn the_app_data_root_is_owned_by_the_service_and_capability_gated() {
        let sec = appdata_root_security();
        assert_eq!(sec.required_cap, Some(CapabilityId::APPDATA_ADMIN));
        assert_eq!(sec.uid, CONFD_UID.0);
        assert_eq!(sec.gid, SERVICES_GID.0);
        assert_eq!(sec.mode, HOME_MODE);
        assert!(sec.acl().is_empty(), "the root grants nobody else anything");
    }

    /// The transit record adds search and nothing else: read or write for the
    /// service on a user's own home would be a real widening.
    #[test]
    fn the_transit_record_grants_the_service_search_only() {
        let sec = appdata_transit_security(1000, 1000).expect("one entry fits");
        assert_eq!(sec.uid, 1000);
        assert_eq!(sec.gid, 1000);
        assert_eq!(sec.mode, HOME_MODE);
        assert_eq!(sec.required_cap, None, "a user reaches their own home");
        assert_eq!(
            sec.acl(),
            [SecurityAcl {
                subject: SecuritySubject::User(CONFD_UID.0),
                perms: SEARCH_ONLY,
            }]
        );
        assert_eq!(SEARCH_ONLY, 0b001, "execute only, never read or write");
    }
}
