//! Behavioural tests for the [`Vfs`] tree: layout enforcement
//! read-only `/System`, and the permission and capability gates.

use super::*;
use crate::fs::perm::Credentials;
use rustos_abi::CapabilityId;
use rustos_caps::CapabilitySet;
use rustos_kernel_sec::{GroupId, UserId};

const ADMIN_UID: u32 = 1;
const ADMIN_GID: u32 = 1;

fn p(text: &str) -> Path {
    Path::parse(text).expect("valid path")
}

fn cred(uid: u32, gid: u32, caps: &CapabilitySet) -> Credentials<'_> {
    Credentials {
        uid: UserId(uid),
        gid: GroupId(gid),
        supplementary_gids: &[],
        caps,
    }
}

fn default_vfs() -> Vfs {
    Vfs::with_default_layout(UserId(ADMIN_UID), GroupId(ADMIN_GID))
}

#[test]
fn default_layout_has_exactly_the_four_top_level_dirs() {
    let vfs = default_vfs();
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut top = vfs.list(&admin, &Path::root()).expect("list root");
    top.sort();
    assert_eq!(top, ["Apps", "Storage", "System", "Users"]);
}

#[test]
fn default_layout_system_writable_exceptions_exist() {
    let vfs = default_vfs();
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let mut sub = vfs.list(&admin, &p("/System")).expect("list /System");
    sub.sort();
    assert_eq!(sub, ["Logs", "Settings"]);
}

#[test]
fn mkdir_legacy_posix_top_level_name_is_allowed() {
    // The OS never authors the legacy POSIX names, but the VFS does not
    // police a user's own request: with write permission on the root a
    // caller may create `/etc`, `/home`, … like any other directory. The
    // refusal is a layout rule the OS keeps to, not a structural ban the
    // kernel imposes on userland.
    let mut vfs = default_vfs();
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    for name in ["etc", "home", "usr", "var", "proc", "tmp", "dev", "bin"] {
        let path = p(&alloc::format!("/{name}"));
        assert!(
            vfs.mkdir(&admin, &path, Mode::from_bits(0o755)).is_ok(),
            "a user may create /{name}"
        );
    }
    // The same names below the top level were never special, and remain fine.
    assert!(vfs
        .mkdir(&admin, &p("/Users/tmp"), Mode::from_bits(0o755))
        .is_ok());
}

#[test]
fn mkdir_nonreserved_top_level_name_is_allowed() {
    let mut vfs = default_vfs();
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    assert!(vfs
        .mkdir(&admin, &p("/Projects"), Mode::from_bits(0o755))
        .is_ok());
}

#[test]
fn system_subtree_is_read_only() {
    let mut vfs = default_vfs();
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    assert_eq!(
        vfs.mkdir(&admin, &p("/System/Drivers"), Mode::from_bits(0o755)),
        Err(VfsError::ReadOnly)
    );
}

#[test]
fn system_logs_and_settings_are_writable() {
    let mut vfs = default_vfs();
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    assert!(vfs
        .mkdir(&admin, &p("/System/Logs/boot"), Mode::from_bits(0o755))
        .is_ok());
    assert!(vfs
        .create_file(
            &admin,
            &p("/System/Settings/hostname"),
            Mode::from_bits(0o644),
            b"rustos".to_vec()
        )
        .is_ok());
}

#[test]
fn create_read_write_round_trip() {
    let mut vfs = default_vfs();
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    let file = p("/Users/notes");
    vfs.create_file(&admin, &file, Mode::from_bits(0o644), b"hello".to_vec())
        .expect("create");
    assert_eq!(vfs.read(&admin, &file).expect("read"), b"hello");
    vfs.write(&admin, &file, b"world".to_vec()).expect("write");
    assert_eq!(vfs.read(&admin, &file).expect("read"), b"world");
}

#[test]
fn create_existing_path_is_rejected() {
    let mut vfs = default_vfs();
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    vfs.mkdir(&admin, &p("/Users/x"), Mode::from_bits(0o755))
        .expect("first");
    assert_eq!(
        vfs.mkdir(&admin, &p("/Users/x"), Mode::from_bits(0o755)),
        Err(VfsError::AlreadyExists)
    );
}

#[test]
fn write_without_permission_is_denied() {
    let mut vfs = default_vfs();
    let admin_caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &admin_caps);
    // Owner-only file: mode 0600.
    let file = p("/Users/private");
    vfs.create_file(&admin, &file, Mode::from_bits(0o600), b"x".to_vec())
        .expect("create");

    // A different, non-member user gets the (empty) other triad.
    let other_caps = CapabilitySet::empty();
    let other = cred(42, 42, &other_caps);
    assert_eq!(
        vfs.write(&other, &file, b"y".to_vec()),
        Err(VfsError::PermissionDenied)
    );
    assert_eq!(vfs.read(&other, &file), Err(VfsError::PermissionDenied));
}

#[test]
fn capability_gated_file_is_unreadable_without_the_capability() {
    let mut vfs = default_vfs();
    let admin_caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &admin_caps);
    // World-readable mode, but gated on CAP_AUDIT_READ.
    let file = p("/Users/auditlog");
    vfs.create_file(&admin, &file, Mode::from_bits(0o644), b"secret".to_vec())
        .expect("create");
    vfs.set_required_cap(&admin, &file, Some(CapabilityId::AUDIT_READ))
        .expect("set cap gate");

    // The owner, at mode 0644, still cannot read without the capability.
    assert_eq!(vfs.read(&admin, &file), Err(VfsError::PermissionDenied));

    let mut with = CapabilitySet::empty();
    with.insert(CapabilityId::AUDIT_READ);
    let auditor = cred(ADMIN_UID, ADMIN_GID, &with);
    assert_eq!(vfs.read(&auditor, &file).expect("read"), b"secret");
}

#[test]
fn set_required_cap_requires_ownership() {
    let mut vfs = default_vfs();
    let admin_caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &admin_caps);
    let file = p("/Users/owned");
    vfs.create_file(&admin, &file, Mode::from_bits(0o666), b"x".to_vec())
        .expect("create");

    let stranger_caps = CapabilitySet::empty();
    let stranger = cred(7, 7, &stranger_caps);
    assert_eq!(
        vfs.set_required_cap(&stranger, &file, Some(CapabilityId::AUDIT_READ)),
        Err(VfsError::PermissionDenied)
    );
}

#[test]
fn remove_non_empty_directory_is_refused() {
    let mut vfs = default_vfs();
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    vfs.mkdir(&admin, &p("/Users/dir"), Mode::from_bits(0o755))
        .expect("mkdir");
    vfs.create_file(
        &admin,
        &p("/Users/dir/f"),
        Mode::from_bits(0o644),
        Vec::new(),
    )
    .expect("file");
    assert_eq!(
        vfs.remove(&admin, &p("/Users/dir"), false),
        Err(VfsError::NotEmpty)
    );
    // A directory-only removal reaching the file is refused atomically
    // (the `rmdir` posture), and the file survives.
    assert_eq!(
        vfs.remove(&admin, &p("/Users/dir/f"), true),
        Err(VfsError::NotADirectory)
    );
    vfs.metadata(&admin, &p("/Users/dir/f"))
        .expect("file survives the refused dir-only removal");
    // Removing the file then the now-empty directory (dir-only) succeeds.
    vfs.remove(&admin, &p("/Users/dir/f"), false)
        .expect("remove file");
    vfs.remove(&admin, &p("/Users/dir"), true)
        .expect("remove empty dir");
    assert_eq!(
        vfs.metadata(&admin, &p("/Users/dir")),
        Err(VfsError::NotFound)
    );
}

#[test]
fn remove_under_read_only_mount_is_refused() {
    let mut vfs = default_vfs();
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    assert_eq!(
        vfs.remove(&admin, &p("/System/Logs"), false),
        Err(VfsError::ReadOnly)
    );
}

#[test]
fn missing_path_is_not_found() {
    let vfs = default_vfs();
    let caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &caps);
    assert_eq!(vfs.read(&admin, &p("/Users/nope")), Err(VfsError::NotFound));
}

#[test]
fn search_permission_is_required_to_traverse() {
    let mut vfs = default_vfs();
    let admin_caps = CapabilitySet::empty();
    let admin = cred(ADMIN_UID, ADMIN_GID, &admin_caps);
    // A directory the admin owns but with no other-search bit.
    vfs.mkdir(&admin, &p("/Users/closed"), Mode::from_bits(0o700))
        .expect("mkdir");
    vfs.create_file(
        &admin,
        &p("/Users/closed/f"),
        Mode::from_bits(0o644),
        Vec::new(),
    )
    .expect("file");

    let other_caps = CapabilitySet::empty();
    let other = cred(99, 99, &other_caps);
    // Cannot search the closed directory → traversal fails before reaching f.
    assert_eq!(
        vfs.read(&other, &p("/Users/closed/f")),
        Err(VfsError::PermissionDenied)
    );
}
