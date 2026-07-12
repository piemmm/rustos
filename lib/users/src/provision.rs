//! The canonical default system/service account and group set
//! (`plans/USERS.md`).
//!
//! Every author of a fresh `/System/Security` pair seeds the same
//! defaults: the image builder (`tools/mkimage`, both profiles), the
//! installer's first-boot provisioning, and the QEMU test fixtures all
//! import [`default_system_accounts`] / [`default_groups`] from here —
//! one definition, never three hand-maintained copies.
//!
//! The set it defines:
//!
//! * `system` — the locked, non-authenticating uid 0 record. The kernel's
//!   compiled-in bootstrap credential is uid 0/gid 0 with **no**
//!   capabilities; this record is merely the *name* the loaded registry
//!   gives that identity, so `/System`'s owner resolves to `system` in
//!   listings and audit output. Its ceiling is empty — powers come from
//!   capabilities, and the boot floor holds none.
//! * One account per system service (`devmgr`, `sysinfod`, `seatmgr`,
//!   `login`), each with its own uid in the system range and primary
//!   group [`SERVICES_GROUP`] — never a shared service user, so §19.4
//!   per-service log partitioning, IPC peer attestation, and blast-radius
//!   containment all key off a real per-service principal. Each carries
//!   exactly its own service's ceiling ([`crate::grants`]).
//! * The groups those records reference: `system`, `services`, and the
//!   well-known removable-storage group ([`STORAGE_GROUP`]).
//!
//! All five accounts are [`AccountState::NoLogin`]: no home, no shell,
//! and the typed never-authenticates password marker — structurally
//! incapable of a session. The uid/gid constants seed provisioning only;
//! runtime consumers resolve accounts and groups **by name** against the
//! loaded registry and fail closed if a record is missing (the
//! [`STORAGE_GROUP`] precedent).

use alloc::vec::Vec;

use rustos_abi::CapabilityId;
use rustos_caps::CapabilitySet;

use crate::grants::{
    capability_set, DEVMGR_CEILING, LOGIN_CEILING, SEATMGR_CEILING, SYSINFOD_CEILING,
};
use crate::groups::{GroupRecord, STORAGE_GID, STORAGE_GROUP};
use crate::password::StoredPassword;
use crate::record::{AccountState, Gid, Identity, Uid, UserRecord};
use crate::ParseError;

/// Name of the system account every OS-owned file resolves to.
pub const SYSTEM_USERNAME: &str = "system";

/// The [`Uid`] the `system` record names: the kernel's bootstrap
/// identity, uid 0.
pub const SYSTEM_UID: Uid = Uid(0);

/// Name of the system account's primary group.
pub const SYSTEM_GROUP: &str = "system";

/// The [`Gid`] provisioning seeds [`SYSTEM_GROUP`] with: the kernel's
/// bootstrap gid, 0.
pub const SYSTEM_GID: Gid = Gid(0);

/// Name of the shared service group every service account's primary gid
/// references — common read access to service-facing paths only; each
/// service's authority stays its own account's ceiling.
pub const SERVICES_GROUP: &str = "services";

/// The [`Gid`] provisioning seeds [`SERVICES_GROUP`] with (system range,
/// following the `storage:100` precedent).
pub const SERVICES_GID: Gid = Gid(101);

/// Name of the device-manager service account.
pub const DEVMGR_USERNAME: &str = "devmgr";

/// The [`Uid`] provisioning seeds [`DEVMGR_USERNAME`] with.
pub const DEVMGR_UID: Uid = Uid(10);

/// Name of the System Information broker service account.
pub const SYSINFOD_USERNAME: &str = "sysinfod";

/// The [`Uid`] provisioning seeds [`SYSINFOD_USERNAME`] with.
pub const SYSINFOD_UID: Uid = Uid(11);

/// Name of the seat-manager service account.
pub const SEATMGR_USERNAME: &str = "seatmgr";

/// The [`Uid`] provisioning seeds [`SEATMGR_USERNAME`] with.
pub const SEATMGR_UID: Uid = Uid(12);

/// Name of the login service account.
pub const LOGIN_USERNAME: &str = "login";

/// The [`Uid`] provisioning seeds [`LOGIN_USERNAME`] with.
pub const LOGIN_UID: Uid = Uid(13);

/// Build one no-login record: no home, no shell, the typed
/// never-authenticates password, and exactly `ceiling` as its grants.
fn no_login_account(
    username: &'static str,
    uid: Uid,
    primary_gid: Gid,
    display_name: &'static str,
    ceiling: CapabilitySet,
) -> Result<UserRecord, ParseError> {
    UserRecord::new(
        Identity {
            username,
            uid,
            primary_gid,
            supplementary_gids: &[],
            display_name,
            home: None,
            shell: None,
            capabilities: ceiling,
            state: AccountState::NoLogin,
        },
        StoredPassword::NeverAuthenticates,
    )
}

/// The default system/service accounts every image starts with, in
/// stable file order: `system`, then one record per service.
///
/// The debug image appends its interactive administrator account on top
/// of these; the installer appends the operator's first user. Neither
/// replaces or edits this set.
///
/// # Errors
///
/// The matching [`ParseError`] should any seeded record violate the
/// format invariants — unreachable by construction, but a provisioning
/// author fails closed rather than panicking.
pub fn default_system_accounts() -> Result<Vec<UserRecord>, ParseError> {
    let services: &[(&'static str, Uid, &'static str, &[CapabilityId])] = &[
        (
            DEVMGR_USERNAME,
            DEVMGR_UID,
            "Device Manager",
            DEVMGR_CEILING,
        ),
        (
            SYSINFOD_USERNAME,
            SYSINFOD_UID,
            "System Information Service",
            SYSINFOD_CEILING,
        ),
        (
            SEATMGR_USERNAME,
            SEATMGR_UID,
            "Seat Manager",
            SEATMGR_CEILING,
        ),
        (LOGIN_USERNAME, LOGIN_UID, "Login Service", LOGIN_CEILING),
    ];
    let mut records = Vec::with_capacity(services.len() + 1);
    records.push(no_login_account(
        SYSTEM_USERNAME,
        SYSTEM_UID,
        SYSTEM_GID,
        "System",
        CapabilitySet::empty(),
    )?);
    for (username, uid, display_name, ceiling) in services {
        records.push(no_login_account(
            username,
            *uid,
            SERVICES_GID,
            display_name,
            capability_set(ceiling),
        )?);
    }
    Ok(records)
}

/// The default group registry every image starts with: `system`,
/// `services`, and the well-known removable-storage group.
///
/// The debug image appends its administrator account's primary group on
/// top of these; the installer appends the first user's.
///
/// # Errors
///
/// As [`default_system_accounts`]: unreachable by construction, failed
/// closed rather than panicked over.
pub fn default_groups() -> Result<Vec<GroupRecord>, ParseError> {
    Ok(alloc::vec![
        GroupRecord::new(SYSTEM_GROUP, SYSTEM_GID)?,
        GroupRecord::new(SERVICES_GROUP, SERVICES_GID)?,
        GroupRecord::new(STORAGE_GROUP, STORAGE_GID)?,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::UsersDb;
    use crate::groups::GroupsDb;
    use crate::policy::IdRange;
    use crate::AuthError;

    #[test]
    fn the_default_account_set_is_pinned() {
        let records = default_system_accounts().expect("valid defaults");
        let summary: Vec<(&str, u32, u32)> = records
            .iter()
            .map(|r| (r.username(), r.uid().0, r.primary_gid().0))
            .collect();
        assert_eq!(
            summary,
            [
                ("system", 0, 0),
                ("devmgr", 10, 101),
                ("sysinfod", 11, 101),
                ("seatmgr", 12, 101),
                ("login", 13, 101),
            ]
        );
        for record in &records {
            assert_eq!(record.state(), AccountState::NoLogin);
            assert_eq!(record.home(), None);
            assert_eq!(record.shell(), None);
            assert_eq!(*record.password(), StoredPassword::NeverAuthenticates);
            assert!(IdRange::System.contains(record.uid().0));
            assert!(IdRange::System.contains(record.primary_gid().0));
        }
    }

    #[test]
    fn each_service_account_carries_exactly_its_own_ceiling() {
        let records = default_system_accounts().expect("valid defaults");
        let by_name = |name: &str| {
            records
                .iter()
                .find(|r| r.username() == name)
                .expect("account present")
        };
        assert_eq!(by_name("system").capabilities(), CapabilitySet::empty());
        assert_eq!(
            by_name("devmgr").capabilities(),
            capability_set(DEVMGR_CEILING)
        );
        assert_eq!(
            by_name("sysinfod").capabilities(),
            capability_set(SYSINFOD_CEILING)
        );
        assert_eq!(
            by_name("seatmgr").capabilities(),
            capability_set(SEATMGR_CEILING)
        );
        assert_eq!(
            by_name("login").capabilities(),
            capability_set(LOGIN_CEILING)
        );
    }

    #[test]
    fn the_default_group_registry_is_pinned_and_covers_every_reference() {
        let groups = GroupsDb::new(default_groups().expect("valid defaults")).expect("valid db");
        let summary: Vec<(&str, u32)> = groups
            .records()
            .iter()
            .map(|g| (g.name(), g.gid().0))
            .collect();
        assert_eq!(
            summary,
            [("system", 0), ("services", 101), ("storage", 100)]
        );
        // Every gid a default account references exists in the registry.
        for record in default_system_accounts().expect("valid defaults") {
            assert!(groups.lookup_gid(record.primary_gid()).is_some());
        }
    }

    #[test]
    fn the_default_accounts_form_a_valid_database_no_one_can_log_into() {
        let db =
            UsersDb::new(default_system_accounts().expect("valid defaults")).expect("valid db");
        let text = db.serialise();
        assert_eq!(UsersDb::parse(&text), Ok(db.clone()));
        for record in db.records() {
            assert_eq!(
                db.authenticate(record.username(), b""),
                Err(AuthError::InvalidCredentials)
            );
            assert_eq!(
                db.authenticate(record.username(), b"*"),
                Err(AuthError::InvalidCredentials)
            );
        }
    }
}
