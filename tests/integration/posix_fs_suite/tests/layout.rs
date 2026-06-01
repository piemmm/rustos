//! `AGENTS.md` §16 on-disk-layout enforcement: the four permitted
//! top-level directories, the refusal of reserved legacy POSIX names, the
//! read-only `/System` subtree with its writable `/System/Logs` and
//! `/System/Settings` exceptions, and the §16.2 read-only-mount refusal on
//! a driver-backed volume.

use rustos_test_posix_fs_suite::*;

/// The reserved legacy POSIX top-level names the installer and VFS refuse
/// (`AGENTS.md` §16.1). A representative subset is enough to assert the
/// rule; the exhaustive list is unit-tested in `kernel/core`.
const RESERVED: &[&str] = &[
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
fn reserved_top_level_names_are_refused() {
    let mut vfs = default_layout_vfs();
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    for name in RESERVED {
        let target = path(&format!("/{name}"));
        assert_eq!(
            vfs.mkdir(&owner, &target, Mode::from_bits(0o755)),
            Err(VfsError::ReservedPath),
            "/{name} must be refused"
        );
    }
}

#[test]
fn reserved_name_below_top_level_is_allowed() {
    // The reservation is top-level only; `/Users/tmp` is a normal name.
    let mut vfs = default_layout_vfs();
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

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
        b"rustos".to_vec(),
    )
    .expect("/System/Settings is writable");
}

#[test]
fn write_to_a_read_only_mount_is_refused() {
    // A volume mounted read-only (§16.2) refuses delegated mutation
    // before the driver is ever touched.
    let (vfs, mut fs) = rustfs_backed_vfs(true);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    assert_eq!(
        vfs.create_via_secured(&owner, &vol_path("nope"), &mut fs),
        Err(VfsError::ReadOnly)
    );
}
