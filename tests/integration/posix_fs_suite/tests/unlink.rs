//! `unlink(2)` conformance: removing a file, `ENOENT`, and reuse of the
//! freed name.

use rustos_test_posix_fs_suite::*;

#[test]
fn unlink_removes_a_file() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.create_via_secured(&owner, &vol_path("gone"), &mut fs)
        .expect("create");
    vfs.remove_via_secured(&owner, &vol_path("gone"), &mut fs, false)
        .expect("remove");

    assert_eq!(
        vfs.stat_via_secured(&owner, &vol_path("gone"), &mut fs)
            .map(|info| info.kind),
        Err(VfsError::NotFound)
    );
}

#[test]
fn unlink_missing_file_is_not_found() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    assert_eq!(
        vfs.remove_via_secured(&owner, &vol_path("never"), &mut fs, false),
        Err(VfsError::NotFound)
    );
}

#[test]
fn the_name_is_reusable_after_unlink() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.create_via_secured(&owner, &vol_path("recycle"), &mut fs)
        .expect("create");
    vfs.remove_via_secured(&owner, &vol_path("recycle"), &mut fs, false)
        .expect("remove");
    vfs.create_via_secured(&owner, &vol_path("recycle"), &mut fs)
        .expect("re-create the freed name");
}

#[test]
fn unlink_leaves_sibling_files_intact() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.create_via_secured(&owner, &vol_path("keep"), &mut fs)
        .expect("create keep");
    vfs.write_via_secured(&owner, &vol_path("keep"), &mut fs, 0, b"intact")
        .expect("write keep");
    vfs.create_via_secured(&owner, &vol_path("drop"), &mut fs)
        .expect("create drop");
    vfs.remove_via_secured(&owner, &vol_path("drop"), &mut fs, false)
        .expect("remove drop");

    let mut buf = [0u8; 6];
    let read = vfs
        .read_via_secured(&owner, &vol_path("keep"), &mut fs, 0, &mut buf)
        .expect("read keep");
    assert_eq!(&buf[..read], b"intact");
}
