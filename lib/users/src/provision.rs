//! The compiled-in system identity: the OS-owned accounts and groups
//! (`plans/USERS.md`).
//!
//! The system/service accounts are **kernel policy, not volume data**:
//! their records are compiled into the kernel (via
//! `kernel/core::groups`), never written to or read from a disk, so they
//! are tamper-proof exactly as the kernel text is and are available from
//! the first instruction of boot on every architecture — before any
//! volume is mounted or unlocked. The on-disk databases under
//! `/System/Security` hold only *human* accounts and their groups; the
//! kernel merges the two halves into one identity table and refuses any
//! on-disk record that collides with the compiled identity (a system-band
//! id or a reserved name), fail closed.
//!
//! The set defined here:
//!
//! * `system` — the non-authenticating uid 0 identity. The kernel's
//!   compiled-in bootstrap credential is uid 0/gid 0 with **no**
//!   capabilities; this record is merely the *name* that identity
//!   resolves to, so `/System`'s owner reads as `system` in listings and
//!   audit output. Its ceiling is empty — powers come from capabilities,
//!   and the boot floor holds none.
//! * One account per system service (`devmgr`, `sysinfod`, `seatmgr`,
//!   `login`, `netstack`, `fontd`), each with its own uid in the system range and primary
//!   group [`SERVICES_GROUP`] — never a shared service user, so §19.4
//!   per-service log partitioning, IPC peer attestation, and blast-radius
//!   containment all key off a real per-service principal. Each carries
//!   exactly its own service's ceiling ([`crate::grants`]), so the
//!   ceiling∩manifest intersection does real work.
//! * The groups those records reference: `system` and `services`. The
//!   well-known removable-storage group ([`crate::STORAGE_GROUP`]) is
//!   **not** part of the compiled identity: storage membership is
//!   admin-managed data about human accounts, so that group lives in the
//!   on-disk registry beside them (`plans/DEVICES.md` D3d).
//!
//! All five accounts are [`AccountState::NoLogin`]: no home, no shell,
//! and the typed never-authenticates password marker — structurally
//! incapable of a session. PID 1 resolves a startup-config account name
//! onto its uid through [`system_account_uid`] (pure, allocation-free)
//! and spawns each service with that concrete `target_uid`.

use alloc::vec::Vec;

use tairix_abi::CapabilityId;
use tairix_caps::CapabilitySet;

use crate::grants::{
    capability_set, DEVMGR_CEILING, FONTD_CEILING, LOGIN_CEILING, NETSTACK_CEILING,
    SEATMGR_CEILING, SYSINFOD_CEILING,
};
use crate::groups::GroupRecord;
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

/// The [`Gid`] of [`SYSTEM_GROUP`]: the kernel's bootstrap gid, 0.
pub const SYSTEM_GID: Gid = Gid(0);

/// Name of the shared service group every service account's primary gid
/// references — common read access to service-facing paths only; each
/// service's authority stays its own account's ceiling.
pub const SERVICES_GROUP: &str = "services";

/// The [`Gid`] of [`SERVICES_GROUP`] (system range, following the
/// `storage:100` precedent).
pub const SERVICES_GID: Gid = Gid(101);

/// Name of the device-manager service account.
pub const DEVMGR_USERNAME: &str = "devmgr";

/// The [`Uid`] of [`DEVMGR_USERNAME`].
pub const DEVMGR_UID: Uid = Uid(10);

/// Name of the System Information broker service account.
pub const SYSINFOD_USERNAME: &str = "sysinfod";

/// The [`Uid`] of [`SYSINFOD_USERNAME`].
pub const SYSINFOD_UID: Uid = Uid(11);

/// Name of the seat-manager service account.
pub const SEATMGR_USERNAME: &str = "seatmgr";

/// The [`Uid`] of [`SEATMGR_USERNAME`].
pub const SEATMGR_UID: Uid = Uid(12);

/// Name of the login service account.
pub const LOGIN_USERNAME: &str = "login";

/// The [`Uid`] of [`LOGIN_USERNAME`].
pub const LOGIN_UID: Uid = Uid(13);

/// Name of the network-stack service account.
pub const NETSTACK_USERNAME: &str = "netstack";

/// The [`Uid`] of [`NETSTACK_USERNAME`].
pub const NETSTACK_UID: Uid = Uid(14);

/// Name of the sandboxed font-service account.
pub const FONTD_USERNAME: &str = "fontd";

/// The [`Uid`] of [`FONTD_USERNAME`].
pub const FONTD_UID: Uid = Uid(15);

/// One compiled-in account's specification: the single row both
/// [`system_accounts`] and [`system_account_uid`] read, so the record
/// set and the name→uid lookup can never diverge.
struct SystemAccountSpec {
    username: &'static str,
    uid: Uid,
    primary_gid: Gid,
    display_name: &'static str,
    ceiling: &'static [CapabilityId],
}

/// The compiled-in account table, in stable order: `system`, then one
/// row per service.
const SYSTEM_ACCOUNTS: &[SystemAccountSpec] = &[
    SystemAccountSpec {
        username: SYSTEM_USERNAME,
        uid: SYSTEM_UID,
        primary_gid: SYSTEM_GID,
        display_name: "System",
        ceiling: &[],
    },
    SystemAccountSpec {
        username: DEVMGR_USERNAME,
        uid: DEVMGR_UID,
        primary_gid: SERVICES_GID,
        display_name: "Device Manager",
        ceiling: DEVMGR_CEILING,
    },
    SystemAccountSpec {
        username: SYSINFOD_USERNAME,
        uid: SYSINFOD_UID,
        primary_gid: SERVICES_GID,
        display_name: "System Information Service",
        ceiling: SYSINFOD_CEILING,
    },
    SystemAccountSpec {
        username: SEATMGR_USERNAME,
        uid: SEATMGR_UID,
        primary_gid: SERVICES_GID,
        display_name: "Seat Manager",
        ceiling: SEATMGR_CEILING,
    },
    SystemAccountSpec {
        username: LOGIN_USERNAME,
        uid: LOGIN_UID,
        primary_gid: SERVICES_GID,
        display_name: "Login Service",
        ceiling: LOGIN_CEILING,
    },
    SystemAccountSpec {
        username: NETSTACK_USERNAME,
        uid: NETSTACK_UID,
        primary_gid: SERVICES_GID,
        display_name: "Network Stack",
        ceiling: NETSTACK_CEILING,
    },
    SystemAccountSpec {
        username: FONTD_USERNAME,
        uid: FONTD_UID,
        primary_gid: SERVICES_GID,
        display_name: "Font Service",
        ceiling: FONTD_CEILING,
    },
];

/// Resolve a compiled-in system account's name onto its [`Uid`].
///
/// Pure and allocation-free: PID 1's startup-config parser validates and
/// resolves each `service`/`session` directive's account name through
/// this lookup at parse time, so a config naming an unknown account is
/// rejected before anything is spawned (fail closed). Returns `None` for
/// any name outside the compiled table — human accounts are not
/// resolvable here; they live in the on-disk database.
#[must_use]
pub fn system_account_uid(name: &str) -> Option<Uid> {
    SYSTEM_ACCOUNTS
        .iter()
        .find(|spec| spec.username == name)
        .map(|spec| spec.uid)
}

/// The compiled-in accounts' `(uid, username)` directory pairing, in
/// stable table order — the rows the kernel's user-directory
/// introspection serves ahead of the on-disk human records, so `ls -l`,
/// `ps`, and `top` render a system uid's name without any volume being
/// mounted. Carries no credential material.
pub fn system_account_directory() -> impl Iterator<Item = (u32, &'static str)> {
    SYSTEM_ACCOUNTS
        .iter()
        .map(|spec| (spec.uid.0, spec.username))
}

/// Whether `name` is a compiled-in system account's username.
///
/// The kernel's identity merge refuses an on-disk account carrying a
/// reserved name, so a tampered or misprovisioned volume can never
/// shadow a system identity.
#[must_use]
pub fn is_system_account_name(name: &str) -> bool {
    system_account_uid(name).is_some()
}

/// Whether `name` is a compiled-in system group's name.
///
/// The kernel's identity merge refuses an on-disk group carrying a
/// reserved name, exactly as [`is_system_account_name`] does for users.
#[must_use]
pub fn is_system_group_name(name: &str) -> bool {
    name == SYSTEM_GROUP || name == SERVICES_GROUP
}

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

/// The compiled-in system accounts, in stable order: `system`, then one
/// record per service.
///
/// Consumed by the kernel's identity-table build (`kernel/core`), never
/// authored to disk: the on-disk `/System/Security/Users` database holds
/// only human accounts.
///
/// # Errors
///
/// The matching [`ParseError`] should any compiled record violate the
/// format invariants — unreachable by construction (pinned by the tests
/// below), but the consumer fails closed rather than panicking.
pub fn system_accounts() -> Result<Vec<UserRecord>, ParseError> {
    let mut records = Vec::with_capacity(SYSTEM_ACCOUNTS.len());
    for spec in SYSTEM_ACCOUNTS {
        records.push(no_login_account(
            spec.username,
            spec.uid,
            spec.primary_gid,
            spec.display_name,
            capability_set(spec.ceiling),
        )?);
    }
    Ok(records)
}

/// The compiled-in system groups: `system` and `services` — exactly the
/// groups the [`system_accounts`] reference.
///
/// Consumed by the kernel's identity-table build beside
/// [`system_accounts`]; the removable-storage group stays in the on-disk
/// registry (`plans/DEVICES.md` D3d).
///
/// # Errors
///
/// As [`system_accounts`]: unreachable by construction, failed closed
/// rather than panicked over.
pub fn system_groups() -> Result<Vec<GroupRecord>, ParseError> {
    Ok(alloc::vec![
        GroupRecord::new(SYSTEM_GROUP, SYSTEM_GID)?,
        GroupRecord::new(SERVICES_GROUP, SERVICES_GID)?,
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
    fn the_system_account_set_is_pinned() {
        let records = system_accounts().expect("valid compiled identity");
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
                ("netstack", 14, 101),
                ("fontd", 15, 101),
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
        let records = system_accounts().expect("valid compiled identity");
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
        assert_eq!(
            by_name("fontd").capabilities(),
            capability_set(FONTD_CEILING)
        );
    }

    #[test]
    fn the_name_lookup_matches_the_record_set_exactly() {
        // Both read the same table, so this pins the lookup honest.
        for record in system_accounts().expect("valid compiled identity") {
            assert_eq!(
                system_account_uid(record.username()),
                Some(record.uid()),
                "{} resolves to its own uid",
                record.username()
            );
            assert!(is_system_account_name(record.username()));
        }
        assert_eq!(system_account_uid("root"), None);
        assert_eq!(system_account_uid(""), None);
        assert!(!is_system_account_name("root"));
    }

    #[test]
    fn the_group_name_guard_covers_exactly_the_compiled_groups() {
        let groups = system_groups().expect("valid compiled identity");
        for group in &groups {
            assert!(is_system_group_name(group.name()));
        }
        assert!(!is_system_group_name("storage"));
        assert!(!is_system_group_name("wheel"));
        assert!(!is_system_group_name(""));
    }

    #[test]
    fn the_system_groups_cover_every_compiled_reference() {
        let groups = GroupsDb::new(system_groups().expect("valid compiled identity"))
            .expect("valid registry");
        let summary: Vec<(&str, u32)> = groups
            .records()
            .iter()
            .map(|g| (g.name(), g.gid().0))
            .collect();
        assert_eq!(summary, [("system", 0), ("services", 101)]);
        for record in system_accounts().expect("valid compiled identity") {
            assert!(groups.lookup_gid(record.primary_gid()).is_some());
        }
    }

    #[test]
    fn the_system_accounts_form_a_valid_database_no_one_can_log_into() {
        let db = UsersDb::new(system_accounts().expect("valid compiled identity"))
            .expect("valid database");
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
