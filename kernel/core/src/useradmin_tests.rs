//! Host tests for the `CAP_USER_ADMIN` account-administration engine
//! (`plans/CAPABILITY_USE.md` CU4): every rule the engine enforces on
//! top of the dispatch gate, the whole-or-nothing commit, and the
//! next-spawn/next-login binding semantics.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use rustos_abi::users_admin::{
    decode_group_list, decode_user_list, gid_list_into, grant_list_into, CreateUser, ModifyUser,
    UsersAdminRequest,
};
use rustos_abi::{CapabilityId, CapabilityQuery, Errno};
use rustos_caps::CapabilitySet;
use rustos_sync::SpinLock;
use rustos_users::{
    AccountState, Gid, GroupRecord, GroupsDb, Identity, PasswordRecord, Salt, Uid, UserRecord,
    UsersDb, MIN_ITERATIONS, SALT_LEN,
};

use crate::fs::LateIdentity;
use crate::test_sink::TestSink;
use crate::useradmin::{UserAdminBacking, UserAdminEngine, UsersAdmin};
use crate::users::{HeldUsersDbSource, LateUsersDb, UsersDbSource};

const SALT: Salt = [0x22; SALT_LEN];

/// A caller holding the debug administrator's effective set.
struct AdminCaller;

impl CapabilityQuery for AdminCaller {
    fn holds(&self, cap: CapabilityId) -> bool {
        rustos_users::administrator_ceiling().contains(cap)
    }
}

/// A caller holding only the session baseline (no `CAP_TIME_SET`, …).
struct BaselineCaller;

impl CapabilityQuery for BaselineCaller {
    fn holds(&self, cap: CapabilityId) -> bool {
        rustos_users::SESSION_BASELINE.contains(&cap)
    }
}

/// A recording, fault-injectable [`UserAdminBacking`].
#[derive(Default)]
struct RecordingBacking {
    persisted: SpinLock<Vec<(String, String)>>,
    homes: SpinLock<Vec<(String, u32, u32)>>,
    fail_persist: SpinLock<bool>,
}

impl RecordingBacking {
    fn persist_count(&self) -> usize {
        self.persisted.lock().len()
    }

    fn set_fail(&self) {
        *self.fail_persist.lock() = true;
    }
}

impl UserAdminBacking for RecordingBacking {
    fn persist(&self, users_text: &str, groups_text: &str) -> Result<(), Errno> {
        if *self.fail_persist.lock() {
            return Err(Errno::NoSpace);
        }
        self.persisted
            .lock()
            .push((String::from(users_text), String::from(groups_text)));
        Ok(())
    }

    fn provision_home(&self, home: &str, uid: u32, gid: u32) -> Result<(), Errno> {
        self.homes.lock().push((String::from(home), uid, gid));
        Ok(())
    }
}

fn record(username: &str, uid: u32, grants: CapabilitySet, password: &[u8]) -> UserRecord {
    UserRecord::with_password(
        Identity {
            username,
            uid: Uid(uid),
            primary_gid: Gid(0),
            supplementary_gids: &[],
            display_name: "",
            home: "/Users/test",
            shell: "/System/Apps/elsh.app/Run",
            capabilities: grants,
            state: AccountState::Active,
        },
        password,
        SALT,
        MIN_ITERATIONS,
    )
    .expect("valid record")
}

struct Fixture {
    engine: UserAdminEngine,
    users_cell: &'static LateUsersDb,
    identity_cell: &'static LateIdentity,
    backing: &'static RecordingBacking,
    sink: &'static TestSink,
}

/// Build an engine over a two-account, two-group state, with the boot
/// databases installed into leaked live cells exactly as the boot path
/// does.
fn fixture() -> Fixture {
    let admin_grants = rustos_users::administrator_ceiling();
    let mut baseline = CapabilitySet::empty();
    for cap in rustos_users::SESSION_BASELINE {
        baseline.insert(*cap);
    }
    let users = UsersDb::new(alloc::vec![
        record("root", 0, admin_grants, b"root"),
        record("ada", 1000, baseline, b"byron"),
    ])
    .expect("valid users db");
    let groups = GroupsDb::new(alloc::vec![
        GroupRecord::new("system", Gid(0)).expect("valid group"),
        GroupRecord::new("staff", Gid(100)).expect("valid group"),
    ])
    .expect("valid groups db");

    let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
    let users_cell: &'static LateUsersDb = Box::leak(Box::new(LateUsersDb::new()));
    let identity_cell: &'static LateIdentity = Box::leak(Box::new(LateIdentity::new()));
    users_cell
        .install(HeldUsersDbSource::new(users.serialise().into_bytes()))
        .expect("boot install");
    identity_cell
        .install(crate::groups::build_identity_table(&users, &groups, sink).expect("boot verify"))
        .expect("boot install");
    let backing: &'static RecordingBacking = Box::leak(Box::new(RecordingBacking::default()));
    sink.clear();

    Fixture {
        engine: UserAdminEngine::new(users, groups, users_cell, identity_cell, backing, sink),
        users_cell,
        identity_cell,
        backing,
        sink,
    }
}

fn password_record(password: &[u8]) -> String {
    PasswordRecord::new(password, SALT, MIN_ITERATIONS)
        .expect("valid password")
        .encode()
}

fn served_db(users_cell: &LateUsersDb) -> UsersDb {
    let text = users_cell.text().expect("served text");
    let text = String::from(core::str::from_utf8(&text).expect("utf8"));
    UsersDb::parse(&text).expect("served text parses")
}

fn handle(fixture: &Fixture, request: &UsersAdminRequest<'_>) -> Result<u64, Errno> {
    fixture.engine.handle(0, &AdminCaller, request, &mut [])
}

#[test]
fn create_user_provisions_home_persists_and_binds_at_next_resolution() {
    let f = fixture();
    let mut grant_backing = [0u8; 8];
    let grants = grant_list_into(
        &[CapabilityId::FS_ACCESS, CapabilityId::PROC_SPAWN],
        &mut grant_backing,
    )
    .expect("fits");
    let mut gid_backing = [0u8; 4];
    let supplementary_gids = gid_list_into(&[100], &mut gid_backing).expect("fits");
    let password = password_record(b"lovelace");
    let request = UsersAdminRequest::CreateUser(CreateUser {
        username: "grace",
        uid: 1001,
        primary_gid: 100,
        supplementary_gids,
        display_name: "Grace Hopper",
        home: "/Users/grace",
        shell: "/System/Apps/elsh.app/Run",
        grants,
        password_record: &password,
    });
    assert_eq!(handle(&f, &request), Ok(0));

    // Persisted, home provisioned under the new identity.
    assert_eq!(f.backing.persist_count(), 1);
    assert_eq!(
        f.backing.homes.lock().as_slice(),
        &[(String::from("/Users/grace"), 1001, 100)]
    );

    // The next login sees the account: the served text authenticates it.
    let db = served_db(f.users_cell);
    db.authenticate("grace", b"lovelace")
        .expect("new account authenticates");

    // The next spawn resolves the new ceiling from the live table.
    let (gid, sups, ceiling) = f
        .identity_cell
        .resolve_credential(1001)
        .expect("new uid resolves");
    assert_eq!(gid.0, 100);
    assert_eq!(sups.len(), 1);
    assert!(ceiling.contains(CapabilityId::FS_ACCESS));
    assert!(!ceiling.contains(CapabilityId::USER_ADMIN));

    // The decision was audited as applied (the commit's identity
    // re-verification emits its own record first).
    assert_eq!(f.sink.event_ids().last(), Some(&4045));
}

#[test]
fn a_grant_the_caller_does_not_hold_is_never_minted() {
    let f = fixture();
    // A baseline caller tries to create an account holding CAP_TIME_SET.
    let mut grant_backing = [0u8; 2];
    let grants = grant_list_into(&[CapabilityId::TIME_SET], &mut grant_backing).expect("fits");
    let mut gid_backing = [0u8; 0];
    let supplementary_gids = gid_list_into(&[], &mut gid_backing).expect("fits");
    let password = password_record(b"pw");
    let request = UsersAdminRequest::CreateUser(CreateUser {
        username: "mallory",
        uid: 2000,
        primary_gid: 0,
        supplementary_gids,
        display_name: "",
        home: "/Users/mallory",
        shell: "/System/Apps/elsh.app/Run",
        grants,
        password_record: &password,
    });
    assert_eq!(
        f.engine.handle(1000, &BaselineCaller, &request, &mut []),
        Err(Errno::PermissionDenied)
    );
    // Nothing changed anywhere.
    assert_eq!(f.backing.persist_count(), 0);
    assert!(served_db(f.users_cell).lookup("mallory").is_none());
    assert_eq!(f.sink.event_ids().last(), Some(&4046));

    // The same widening through SetGrants is refused too.
    let mut grant_backing = [0u8; 2];
    let grants = grant_list_into(&[CapabilityId::TIME_SET], &mut grant_backing).expect("fits");
    assert_eq!(
        f.engine.handle(
            1000,
            &BaselineCaller,
            &UsersAdminRequest::SetGrants {
                username: "ada",
                grants,
            },
            &mut [],
        ),
        Err(Errno::PermissionDenied)
    );
}

#[test]
fn the_last_active_administrator_is_protected() {
    let f = fixture();
    // Deleting the only administrator is refused.
    assert_eq!(
        handle(&f, &UsersAdminRequest::DeleteUser { username: "root" }),
        Err(Errno::PermissionDenied)
    );
    // Locking the only administrator is refused.
    assert_eq!(
        handle(
            &f,
            &UsersAdminRequest::SetAccountState {
                username: "root",
                locked: true,
            },
        ),
        Err(Errno::PermissionDenied)
    );
    // Stripping CAP_USER_ADMIN from the only administrator is refused.
    let mut grant_backing = [0u8; 2];
    let grants = grant_list_into(&[CapabilityId::FS_ACCESS], &mut grant_backing).expect("fits");
    assert_eq!(
        handle(
            &f,
            &UsersAdminRequest::SetGrants {
                username: "root",
                grants,
            },
        ),
        Err(Errno::PermissionDenied)
    );
    assert_eq!(f.backing.persist_count(), 0);
}

#[test]
fn locking_binds_at_the_next_login_and_unlock_restores() {
    let f = fixture();
    assert_eq!(
        handle(
            &f,
            &UsersAdminRequest::SetAccountState {
                username: "ada",
                locked: true,
            },
        ),
        Ok(0)
    );
    // The next login is refused indistinguishably; the identity row
    // stays resolvable, so the locked account's files keep their groups.
    let db = served_db(f.users_cell);
    assert!(db.authenticate("ada", b"byron").is_err());
    assert!(f.identity_cell.resolve_credential(1000).is_ok());

    assert_eq!(
        handle(
            &f,
            &UsersAdminRequest::SetAccountState {
                username: "ada",
                locked: false,
            },
        ),
        Ok(0)
    );
    served_db(f.users_cell)
        .authenticate("ada", b"byron")
        .expect("reactivated account authenticates");
}

#[test]
fn delete_user_removes_the_account_and_missing_targets_fail_closed() {
    let f = fixture();
    assert_eq!(
        handle(&f, &UsersAdminRequest::DeleteUser { username: "ada" }),
        Ok(0)
    );
    assert!(served_db(f.users_cell).lookup("ada").is_none());
    assert_eq!(
        f.identity_cell.resolve_credential(1000),
        Err(Errno::PermissionDenied)
    );
    assert_eq!(
        handle(&f, &UsersAdminRequest::DeleteUser { username: "ada" }),
        Err(Errno::NotFound)
    );
}

#[test]
fn set_password_replaces_the_stored_record_and_rejects_malformed_ones() {
    let f = fixture();
    let password = password_record(b"new-secret");
    assert_eq!(
        handle(
            &f,
            &UsersAdminRequest::SetPassword {
                username: "ada",
                password_record: &password,
            },
        ),
        Ok(0)
    );
    let db = served_db(f.users_cell);
    db.authenticate("ada", b"new-secret").expect("new password");
    assert!(db.authenticate("ada", b"byron").is_err());

    assert_eq!(
        handle(
            &f,
            &UsersAdminRequest::SetPassword {
                username: "ada",
                password_record: "not-a-record",
            },
        ),
        Err(Errno::OutOfRange)
    );
}

#[test]
fn duplicate_accounts_and_groups_are_refused() {
    let f = fixture();
    let mut grant_backing = [0u8; 0];
    let grants = grant_list_into(&[], &mut grant_backing).expect("fits");
    let mut gid_backing = [0u8; 0];
    let supplementary_gids = gid_list_into(&[], &mut gid_backing).expect("fits");
    let password = password_record(b"pw");
    // Same name as an existing account.
    let request = UsersAdminRequest::CreateUser(CreateUser {
        username: "ada",
        uid: 3000,
        primary_gid: 0,
        supplementary_gids,
        display_name: "",
        home: "/Users/ada2",
        shell: "/System/Apps/elsh.app/Run",
        grants,
        password_record: &password,
    });
    assert_eq!(handle(&f, &request), Err(Errno::AlreadyExists));
    // Same gid as an existing group.
    assert_eq!(
        handle(
            &f,
            &UsersAdminRequest::CreateGroup {
                name: "wheel",
                gid: 100,
            },
        ),
        Err(Errno::AlreadyExists)
    );
}

#[test]
fn group_lifecycle_is_enforced_with_referential_integrity() {
    let f = fixture();
    assert_eq!(
        handle(
            &f,
            &UsersAdminRequest::CreateGroup {
                name: "wheel",
                gid: 200,
            },
        ),
        Ok(0)
    );
    // Deleting an unreferenced group succeeds.
    assert_eq!(
        handle(&f, &UsersAdminRequest::DeleteGroup { name: "wheel" }),
        Ok(0)
    );
    // Deleting a group an account references is refused: the identity
    // verification fails closed and nothing is persisted.
    let before = f.backing.persist_count();
    assert!(handle(&f, &UsersAdminRequest::DeleteGroup { name: "system" }).is_err());
    assert_eq!(f.backing.persist_count(), before);
    // A missing group fails closed.
    assert_eq!(
        handle(&f, &UsersAdminRequest::DeleteGroup { name: "ghost" }),
        Err(Errno::NotFound)
    );
}

#[test]
fn modify_user_replaces_identity_fields_and_provisions_a_changed_home() {
    let f = fixture();
    let mut gid_backing = [0u8; 4];
    let supplementary_gids = gid_list_into(&[100], &mut gid_backing).expect("fits");
    assert_eq!(
        handle(
            &f,
            &UsersAdminRequest::ModifyUser(ModifyUser {
                username: "ada",
                primary_gid: 100,
                supplementary_gids,
                display_name: "Ada Lovelace",
                home: "/Users/ada",
                shell: "/System/Apps/elsh.app/Run",
            }),
        ),
        Ok(0)
    );
    // The home changed from the fixture's "/Users/test", so it was
    // provisioned under the account's identity.
    assert_eq!(
        f.backing.homes.lock().as_slice(),
        &[(String::from("/Users/ada"), 1000, 100)]
    );
    let (gid, sups, ceiling) = f.identity_cell.resolve_credential(1000).expect("resolves");
    assert_eq!(gid.0, 100);
    assert_eq!(sups.len(), 1);
    // The security fields are untouched by a modify.
    assert!(ceiling.contains(CapabilityId::FS_ACCESS));
    let db = served_db(f.users_cell);
    db.authenticate("ada", b"byron")
        .expect("password unchanged");
}

#[test]
fn a_failed_persist_changes_nothing() {
    let f = fixture();
    let before = served_db(f.users_cell).serialise();
    f.backing.set_fail();
    assert_eq!(
        handle(
            &f,
            &UsersAdminRequest::SetAccountState {
                username: "ada",
                locked: true,
            },
        ),
        Err(Errno::NoSpace)
    );
    // The live view and the engine state are untouched.
    assert_eq!(served_db(f.users_cell).serialise(), before);
    served_db(f.users_cell)
        .authenticate("ada", b"byron")
        .expect("still active");
    assert_eq!(f.sink.event_ids().last(), Some(&4046));
}

#[test]
fn list_users_and_groups_answer_the_non_secret_view() {
    let f = fixture();
    let mut out = alloc::vec![0u8; 4096];
    let len = handle_list(&f, &UsersAdminRequest::ListUsers, &mut out);
    let response = &out[..len];
    // No password material: not even the scheme tag appears.
    assert!(!contains(response, b"pbkdf2"));
    let entries: Vec<_> = decode_user_list(response)
        .expect("decodes")
        .collect::<Result<_, _>>()
        .expect("all entries decode");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].username, "root");
    assert!(!entries[0].locked);
    assert!(entries[0]
        .grants
        .iter()
        .any(|cap| cap == CapabilityId::USER_ADMIN));
    assert_eq!(entries[1].username, "ada");

    let len = handle_list(&f, &UsersAdminRequest::ListGroups, &mut out);
    let entries: Vec<_> = decode_group_list(&out[..len])
        .expect("decodes")
        .collect::<Result<_, _>>()
        .expect("all entries decode");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].name, "system");
    assert_eq!(entries[1].gid, 100);

    // An undersized buffer fails closed whole-or-nothing.
    let mut tiny = [0u8; 8];
    assert_eq!(
        f.engine
            .handle(0, &AdminCaller, &UsersAdminRequest::ListUsers, &mut tiny),
        Err(Errno::BufferTooSmall)
    );
}

fn handle_list(f: &Fixture, request: &UsersAdminRequest<'_>, out: &mut [u8]) -> usize {
    let len = f
        .engine
        .handle(0, &AdminCaller, request, out)
        .expect("list succeeds");
    usize::try_from(len).expect("fits")
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}
