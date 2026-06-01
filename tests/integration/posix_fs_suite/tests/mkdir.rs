//! `mkdir(2)` conformance: directory creation, `EEXIST`, `ENOENT`,
//! `ENOTDIR`, and the refusal to mutate the mount root itself.
//!
//! Every case drives the real `rustfs` driver through
//! [`Vfs::mkdir_via_secured`], the per-inode-security delegation path the
//! kernel uses for a native filesystem.

use rustos_test_posix_fs_suite::*;

#[test]
fn mkdir_creates_a_directory() {
    let (vfs, mut fs) = rustfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.mkdir_via_secured(&owner, &vol_path("d"), &mut fs)
        .expect("mkdir succeeds");

    let info = vfs
        .stat_via_secured(&owner, &vol_path("d"), &mut fs)
        .expect("stat the new directory");
    assert_eq!(info.kind, NodeKind::Directory);
}

#[test]
fn mkdir_creates_nested_directories() {
    let (vfs, mut fs) = rustfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.mkdir_via_secured(&owner, &vol_path("a"), &mut fs)
        .expect("mkdir a");
    vfs.mkdir_via_secured(&owner, &vol_path("a/b"), &mut fs)
        .expect("mkdir a/b");

    let names = vfs
        .list_via_secured(&owner, &vol_path("a"), &mut fs)
        .expect("list a");
    assert_eq!(names, ["b"]);
}

#[test]
fn mkdir_existing_name_is_already_exists() {
    let (vfs, mut fs) = rustfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.mkdir_via_secured(&owner, &vol_path("dup"), &mut fs)
        .expect("first mkdir");
    assert_eq!(
        vfs.mkdir_via_secured(&owner, &vol_path("dup"), &mut fs),
        Err(VfsError::AlreadyExists)
    );
}

#[test]
fn mkdir_over_existing_file_is_already_exists() {
    let (vfs, mut fs) = rustfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.create_via_secured(&owner, &vol_path("f"), &mut fs)
        .expect("create file");
    assert_eq!(
        vfs.mkdir_via_secured(&owner, &vol_path("f"), &mut fs),
        Err(VfsError::AlreadyExists)
    );
}

#[test]
fn mkdir_in_missing_parent_is_not_found() {
    let (vfs, mut fs) = rustfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    assert_eq!(
        vfs.mkdir_via_secured(&owner, &vol_path("missing/child"), &mut fs),
        Err(VfsError::NotFound)
    );
}

#[test]
fn mkdir_with_file_as_parent_is_not_a_directory() {
    let (vfs, mut fs) = rustfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.create_via_secured(&owner, &vol_path("file"), &mut fs)
        .expect("create file");
    assert_eq!(
        vfs.mkdir_via_secured(&owner, &vol_path("file/child"), &mut fs),
        Err(VfsError::NotADirectory)
    );
}

#[test]
fn mkdir_of_the_mount_root_itself_is_invalid() {
    let (vfs, mut fs) = rustfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    assert_eq!(
        vfs.mkdir_via_secured(&owner, &path(MOUNT), &mut fs),
        Err(VfsError::InvalidPath)
    );
}
