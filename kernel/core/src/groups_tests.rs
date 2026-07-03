//! Behavioural tests for the boot-time group-registry load
//! ([`crate::groups::load_groups_db`]) and the identity-table build
//! ([`crate::groups::build_identity_table`]): the success paths and every
//! fail-closed refusal, each with its audit record.

use alloc::string::String;
use alloc::vec::Vec;

use rustos_abi::driver::filesystem::{FilesystemRead, FilesystemWrite, NodeKind, NodeSecurity};
use rustos_abi::{CapabilityId, Errno};
use rustos_caps::CapabilitySet;
use rustos_kernel_sec::{GroupId, UserId};
use rustos_users::{
    AccountState, Gid, GroupRecord, GroupsDb, Identity, ParseError, Uid, UserRecord, UsersDb,
    MIN_ITERATIONS,
};

use crate::fs::memfs::RwMockFs;
use crate::fs::VfsError;
use crate::groups::{build_identity_table, load_groups_db, GroupsLoadError};
use crate::test_sink::TestSink;

/// An in-memory root volume carrying `/System/Security/Groups` with the
/// given file bytes, owned by `uid 0` so the bootstrap reader can traverse
/// and read it (mirroring the mkimage-authored skeleton).
fn planted(groups: &[u8]) -> RwMockFs {
    let mut fs = RwMockFs::new().with_create_owner(0, 0, 0o755);
    fs.set_root_security(NodeSecurity::new(0o755, 0, 0));
    let root = fs.root();
    let system = fs
        .create(root, b"System", NodeKind::Directory)
        .expect("System");
    let security = fs
        .create(system, b"Security", NodeKind::Directory)
        .expect("Security");
    fs.create(security, b"Groups", NodeKind::RegularFile)
        .expect("Groups");
    fs.write_at(security, b"Groups", 0, groups)
        .expect("write Groups");
    fs
}

/// A root volume with the `/System/Security` tree but **no** `Groups` file.
fn without_groups() -> RwMockFs {
    let mut fs = RwMockFs::new().with_create_owner(0, 0, 0o755);
    fs.set_root_security(NodeSecurity::new(0o755, 0, 0));
    let root = fs.root();
    let system = fs
        .create(root, b"System", NodeKind::Directory)
        .expect("System");
    fs.create(system, b"Security", NodeKind::Directory)
        .expect("Security");
    fs
}

fn valid_groups_text() -> String {
    GroupsDb::new(alloc::vec![
        GroupRecord::new("wheel", Gid(0)).expect("valid"),
        GroupRecord::new("ada", Gid(1000)).expect("valid"),
    ])
    .expect("valid db")
    .serialise()
}

/// A user record naming `primary` and `supplementary` groups.
fn user(name: &str, uid: u32, primary: u32, supplementary: &[Gid]) -> UserRecord {
    let mut caps = CapabilitySet::empty();
    caps.insert(CapabilityId::PROC_SPAWN);
    UserRecord::with_password(
        Identity {
            username: name,
            uid: Uid(uid),
            primary_gid: Gid(primary),
            supplementary_gids: supplementary,
            display_name: "",
            home: "/Users/test",
            shell: "/System/Apps/elsh.app/Run",
            capabilities: caps,
            state: AccountState::Active,
        },
        b"correct horse",
        [0x42; 16],
        MIN_ITERATIONS,
    )
    .expect("valid record")
}

#[test]
fn a_valid_registry_loads_and_is_audited() {
    let sink = TestSink::new();
    let mut fs = planted(valid_groups_text().as_bytes());

    let db = load_groups_db(&mut fs, &sink).expect("valid registry loads");
    assert_eq!(db.records().len(), 2);
    assert_eq!(db.lookup("wheel").map(GroupRecord::gid), Some(Gid(0)));

    let events = sink.snapshot();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].id.0, 4043);
    assert!(events[0]
        .fields
        .iter()
        .any(|(k, v)| k == "records" && v == "2"));
}

#[test]
fn a_missing_registry_is_not_found_and_audited() {
    let sink = TestSink::new();
    let mut fs = without_groups();

    let err = load_groups_db(&mut fs, &sink).expect_err("missing registry refused");
    assert_eq!(err, GroupsLoadError::Vfs(VfsError::NotFound));

    let events = sink.snapshot();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].id.0, 4044);
    assert!(events[0]
        .fields
        .iter()
        .any(|(k, v)| k == "cause" && v == "not_found"));
}

#[test]
fn a_bad_header_is_rejected_fail_closed() {
    let sink = TestSink::new();
    let mut fs = planted(b"not-the-groups-header\n");

    let err = load_groups_db(&mut fs, &sink).expect_err("bad header refused");
    assert_eq!(err, GroupsLoadError::Parse(ParseError::Header));

    let events = sink.snapshot();
    assert_eq!(events[0].id.0, 4044);
    assert!(events[0]
        .fields
        .iter()
        .any(|(k, v)| k == "cause" && v == "parse_rejected"));
}

#[test]
fn non_utf8_bytes_are_rejected_fail_closed() {
    let sink = TestSink::new();
    // A lone 0xFF byte is not valid UTF-8, so the registry is refused before
    // the `groups-v1` parser ever runs.
    let mut fs = planted(&[0xff, 0xfe, 0x00]);

    let err = load_groups_db(&mut fs, &sink).expect_err("non-utf8 refused");
    assert_eq!(err, GroupsLoadError::NotUtf8);

    let events = sink.snapshot();
    assert_eq!(events[0].id.0, 4044);
    assert!(events[0]
        .fields
        .iter()
        .any(|(k, v)| k == "cause" && v == "not_utf8"));
}

#[test]
fn the_identity_table_builds_from_users_and_groups() {
    let sink = TestSink::new();
    let groups = GroupsDb::new(alloc::vec![
        GroupRecord::new("wheel", Gid(0)).expect("valid"),
        GroupRecord::new("ada", Gid(1000)).expect("valid"),
    ])
    .expect("valid groups");
    let users = UsersDb::new(alloc::vec![user("ada", 1000, 1000, &[Gid(0)])]).expect("valid users");

    let table = build_identity_table(&users, &groups, &sink).expect("table verifies");
    let record = table.user(UserId(1000)).expect("user present");
    assert_eq!(record.primary_gid, GroupId(1000));
    assert_eq!(record.supplementary_gids, alloc::vec![GroupId(0)]);
    assert!(record.capability_grants.contains(CapabilityId::PROC_SPAWN));
    // The verifier emits exactly one IdentityTableLoaded record.
    assert_eq!(sink.snapshot().len(), 1);
}

#[test]
fn a_user_referencing_an_unknown_group_fails_closed() {
    let sink = TestSink::new();
    // The registry declares only gid 0; the user names gid 1000 as its
    // primary group, so referential integrity must reject the table.
    let groups = GroupsDb::new(alloc::vec![
        GroupRecord::new("wheel", Gid(0)).expect("valid")
    ])
    .expect("ok");
    let users = UsersDb::new(alloc::vec![user("ada", 1000, 1000, &[])]).expect("valid users");

    let err = build_identity_table(&users, &groups, &sink).expect_err("dangling group rejected");
    assert_eq!(err, Errno::NotFound);
}

#[test]
fn a_user_supplementary_group_must_also_resolve() {
    let sink = TestSink::new();
    let groups = GroupsDb::new(alloc::vec![
        GroupRecord::new("wheel", Gid(0)).expect("valid")
    ])
    .expect("ok");
    // Primary gid 0 resolves, but supplementary gid 7 does not.
    let users = UsersDb::new(alloc::vec![user("ada", 1000, 0, &[Gid(7)])]).expect("valid users");

    let err =
        build_identity_table(&users, &groups, &sink).expect_err("dangling supplementary rejected");
    assert_eq!(err, Errno::NotFound);
}

#[test]
fn an_empty_database_pair_builds_an_empty_table() {
    let sink = TestSink::new();
    let groups = GroupsDb::new(Vec::new()).expect("empty groups");
    let users = UsersDb::new(Vec::new()).expect("empty users");

    let table = build_identity_table(&users, &groups, &sink).expect("empty table verifies");
    assert_eq!(table.user_count(), 0);
    assert_eq!(table.group_count(), 0);
    assert!(table.user(UserId(0)).is_err());
}
