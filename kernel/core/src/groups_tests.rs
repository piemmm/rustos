//! Behavioural tests for the boot-time group-registry load
//! ([`crate::groups::load_groups_db`]) and the identity-table build
//! ([`crate::groups::build_identity_table`]): the success paths and every
//! fail-closed refusal, each with its audit record.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::driver::filesystem::{FilesystemRead, FilesystemWrite, NodeKind, NodeSecurity};
use tairix_abi::{CapabilityId, Errno};
use tairix_caps::CapabilitySet;
use tairix_kernel_sec::{GroupId, UserId};
use tairix_users::{
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
        GroupRecord::new("wheel", Gid(1000)).expect("valid"),
        GroupRecord::new("ada", Gid(1001)).expect("valid"),
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
            home: Some("/Users/test"),
            shell: Some("/System/Apps/elsh.app/Run"),
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
    assert_eq!(db.lookup("wheel").map(GroupRecord::gid), Some(Gid(1000)));

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
fn the_identity_table_merges_the_compiled_half_with_the_on_disk_records() {
    let sink = TestSink::new();
    let groups = GroupsDb::new(alloc::vec![
        GroupRecord::new("wheel", Gid(1000)).expect("valid"),
        GroupRecord::new("ada", Gid(1001)).expect("valid"),
    ])
    .expect("valid groups");
    let users =
        UsersDb::new(alloc::vec![user("ada", 1000, 1001, &[Gid(1000)])]).expect("valid users");

    let table = build_identity_table(&users, &groups, &sink).expect("table verifies");
    // The on-disk human record resolves…
    let record = table.user(UserId(1000)).expect("user present");
    assert_eq!(record.primary_gid, GroupId(1001));
    assert_eq!(record.supplementary_gids, alloc::vec![GroupId(1000)]);
    assert!(record.capability_grants.contains(CapabilityId::PROC_SPAWN));
    // …and so does the compiled-in system identity: `system` (uid 0,
    // empty ceiling) and every service account with exactly its own
    // ceiling, available with no volume mounted.
    let system = table.user(UserId(0)).expect("system present");
    assert!(system.capability_grants.is_empty());
    let devmgr = table
        .user(UserId(tairix_users::DEVMGR_UID.0))
        .expect("devmgr present");
    assert!(devmgr.capability_grants.contains(CapabilityId::DRV_LOAD));
    assert_eq!(devmgr.primary_gid, GroupId(tairix_users::SERVICES_GID.0));
    // The verifier emits exactly one IdentityTableLoaded record.
    assert_eq!(sink.snapshot().len(), 1);
}

#[test]
fn a_user_referencing_an_unknown_group_fails_closed() {
    let sink = TestSink::new();
    // The registry declares only gid 1000; the user names gid 2000 as its
    // primary group, so referential integrity must reject the table.
    let groups = GroupsDb::new(alloc::vec![
        GroupRecord::new("wheel", Gid(1000)).expect("valid")
    ])
    .expect("ok");
    let users = UsersDb::new(alloc::vec![user("ada", 1000, 2000, &[])]).expect("valid users");

    let err = build_identity_table(&users, &groups, &sink).expect_err("dangling group rejected");
    assert_eq!(err, Errno::NotFound);
}

#[test]
fn a_user_supplementary_group_must_also_resolve() {
    let sink = TestSink::new();
    let groups = GroupsDb::new(alloc::vec![
        GroupRecord::new("wheel", Gid(1000)).expect("valid")
    ])
    .expect("ok");
    // Primary gid 1000 resolves, but supplementary gid 2000 does not.
    let users =
        UsersDb::new(alloc::vec![user("ada", 1000, 1000, &[Gid(2000)])]).expect("valid users");

    let err =
        build_identity_table(&users, &groups, &sink).expect_err("dangling supplementary rejected");
    assert_eq!(err, Errno::NotFound);
}

#[test]
fn an_empty_database_pair_builds_exactly_the_compiled_identity() {
    let sink = TestSink::new();
    let groups = GroupsDb::new(Vec::new()).expect("empty groups");
    let users = UsersDb::new(Vec::new()).expect("empty users");

    let merged = build_identity_table(&users, &groups, &sink).expect("merged table verifies");
    let compiled = crate::groups::system_identity_table(&sink).expect("compiled table verifies");
    for table in [&merged, &compiled] {
        assert_eq!(table.user_count(), 7);
        assert_eq!(table.group_count(), 2);
        assert!(table.user(UserId(0)).is_ok());
        assert!(table.user(UserId(tairix_users::LOGIN_UID.0)).is_ok());
        assert!(table.user(UserId(1000)).is_err());
    }
}

#[test]
fn an_on_disk_user_in_the_system_band_is_rejected() {
    let sink = TestSink::new();
    let groups = GroupsDb::new(alloc::vec![
        GroupRecord::new("wheel", Gid(1000)).expect("valid")
    ])
    .expect("ok");
    // uid 999 is in the reserved system band: a tampered volume must not
    // be able to plant a system-band principal.
    let users = UsersDb::new(alloc::vec![user("imposter", 999, 1000, &[])]).expect("valid users");

    let err = build_identity_table(&users, &groups, &sink).expect_err("system-band uid rejected");
    assert_eq!(err, Errno::PermissionDenied);
    let events = sink.snapshot();
    assert!(events.iter().any(|e| e.id.0 == 4041
        && e.fields
            .iter()
            .any(|(k, v)| k == "cause" && v == "reserved_identity")));
}

#[test]
fn an_on_disk_user_with_a_reserved_name_is_rejected() {
    let sink = TestSink::new();
    let groups = GroupsDb::new(alloc::vec![
        GroupRecord::new("wheel", Gid(1000)).expect("valid")
    ])
    .expect("ok");
    // A user-band uid under a reserved name must not shadow the compiled
    // `devmgr` identity in listings or name lookups.
    let users = UsersDb::new(alloc::vec![user("devmgr", 1500, 1000, &[])]).expect("valid users");

    let err = build_identity_table(&users, &groups, &sink).expect_err("reserved name rejected");
    assert_eq!(err, Errno::PermissionDenied);
}

#[test]
fn an_on_disk_group_in_the_system_band_is_rejected() {
    let sink = TestSink::new();
    let groups =
        GroupsDb::new(alloc::vec![GroupRecord::new("hack", Gid(5)).expect("valid")]).expect("ok");
    let users = UsersDb::new(Vec::new()).expect("empty users");

    let err = build_identity_table(&users, &groups, &sink).expect_err("system-band gid rejected");
    assert_eq!(err, Errno::PermissionDenied);
    let events = sink.snapshot();
    assert!(events.iter().any(|e| e.id.0 == 4044
        && e.fields
            .iter()
            .any(|(k, v)| k == "cause" && v == "reserved_identity")));
}

#[test]
fn an_on_disk_group_with_a_reserved_name_is_rejected() {
    let sink = TestSink::new();
    let groups = GroupsDb::new(alloc::vec![
        GroupRecord::new("services", Gid(1500)).expect("valid")
    ])
    .expect("ok");
    let users = UsersDb::new(Vec::new()).expect("empty users");

    let err = build_identity_table(&users, &groups, &sink).expect_err("reserved name rejected");
    assert_eq!(err, Errno::PermissionDenied);
}

#[test]
fn the_storage_group_is_accepted_only_under_its_pinned_pairing() {
    // The well-known pairing is the one system-band record the on-disk
    // registry legitimately carries…
    let sink = TestSink::new();
    let groups = GroupsDb::new(alloc::vec![GroupRecord::new(
        tairix_users::STORAGE_GROUP,
        tairix_users::STORAGE_GID
    )
    .expect("valid")])
    .expect("ok");
    let users = UsersDb::new(Vec::new()).expect("empty users");
    let table = build_identity_table(&users, &groups, &sink).expect("storage pairing accepted");
    assert!(table.group(GroupId(tairix_users::STORAGE_GID.0)).is_ok());

    // …but neither half of the pairing may be repurposed: the name under
    // another gid, or the gid under another name, both reject.
    for (name, gid) in [
        (tairix_users::STORAGE_GROUP, Gid(555)),
        ("media", tairix_users::STORAGE_GID),
    ] {
        let groups =
            GroupsDb::new(alloc::vec![GroupRecord::new(name, gid).expect("valid")]).expect("ok");
        let users = UsersDb::new(Vec::new()).expect("empty users");
        let err = build_identity_table(&users, &groups, &sink)
            .expect_err("a repurposed storage pairing is rejected");
        assert_eq!(err, Errno::PermissionDenied);
    }
}
