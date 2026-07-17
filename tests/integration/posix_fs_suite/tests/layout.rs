//! on-disk-layout enforcement: the four permitted
//! top-level directories, the read-only `/System` subtree with its writable
//! `/System/Logs` and `/System/Settings` exceptions, and the
//! read-only-mount refusal on a driver-backed volume.

use tairix_test_posix_fs_suite::*;

/// Legacy POSIX top-level names. The OS never authors these, but the VFS
/// does not refuse a user's own request to create one; a representative
/// subset is enough to assert that.
const LEGACY_NAMES: &[&str] = &[
    "etc", "home", "usr", "var", "proc", "sys", "bin", "sbin", "dev", "tmp", "root", "boot",
];

#[test]
fn default_layout_exposes_exactly_the_four_top_level_directories() {
    let vfs = default_layout_vfs();
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    let mut top = vfs.list(&owner, &path("/")).expect("list root");
    top.sort();
    assert_eq!(top, ["Apps", "Storage", "System", "Users"]);
}

#[test]
fn legacy_top_level_names_are_not_refused_for_a_user() {
    let mut vfs = default_layout_vfs();
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    for name in LEGACY_NAMES {
        let target = path(&format!("/{name}"));
        vfs.mkdir(&owner, &target, Mode::from_bits(0o755))
            .unwrap_or_else(|e| panic!("a user may create /{name}, got {e:?}"));
    }

    // The same names below the top level were never special either.
    vfs.mkdir(&owner, &path("/Users/tmp"), Mode::from_bits(0o755))
        .expect("/Users/tmp is permitted");
}

#[test]
fn system_subtree_is_read_only_at_runtime() {
    let mut vfs = default_layout_vfs();
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    assert_eq!(
        vfs.mkdir(&owner, &path("/System/Drivers"), Mode::from_bits(0o755)),
        Err(VfsError::ReadOnly)
    );
}

#[test]
fn system_logs_and_settings_are_writable() {
    let mut vfs = default_layout_vfs();
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.mkdir(&owner, &path("/System/Logs/boot"), Mode::from_bits(0o755))
        .expect("/System/Logs is writable");
    vfs.create_file(
        &owner,
        &path("/System/Settings/hostname"),
        Mode::from_bits(0o644),
        b"tairix".to_vec(),
    )
    .expect("/System/Settings is writable");
}

#[test]
fn write_to_a_read_only_mount_is_refused() {
    // A volume mounted read-only refuses delegated mutation
    // before the driver is ever touched.
    let (vfs, mut fs) = arxfs_backed_vfs(true);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    assert_eq!(
        vfs.create_via_secured(&owner, &vol_path("nope"), &mut fs),
        Err(VfsError::ReadOnly)
    );
}
