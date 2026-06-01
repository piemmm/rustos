//! `rmdir(2)` conformance: removing an empty directory, `ENOTEMPTY` for a
//! populated one, and `ENOENT`.

use rustos_test_posix_fs_suite::*;

#[test]
fn rmdir_removes_an_empty_directory() {
    let (vfs, mut fs) = rustfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.mkdir_via_secured(&owner, &vol_path("empty"), &mut fs)
        .expect("mkdir");
    vfs.remove_via_secured(&owner, &vol_path("empty"), &mut fs)
        .expect("rmdir");

    assert_eq!(
        vfs.stat_via_secured(&owner, &vol_path("empty"), &mut fs)
            .map(|info| info.kind),
        Err(VfsError::NotFound)
    );
}

#[test]
fn rmdir_of_non_empty_directory_is_not_empty() {
    let (vfs, mut fs) = rustfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.mkdir_via_secured(&owner, &vol_path("populated"), &mut fs)
        .expect("mkdir");
    vfs.create_via_secured(&owner, &vol_path("populated/child"), &mut fs)
        .expect("create child");

    assert_eq!(
        vfs.remove_via_secured(&owner, &vol_path("populated"), &mut fs),
        Err(VfsError::NotEmpty)
    );
}

#[test]
fn rmdir_succeeds_once_the_directory_is_emptied() {
    let (vfs, mut fs) = rustfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.mkdir_via_secured(&owner, &vol_path("d"), &mut fs)
        .expect("mkdir");
    vfs.create_via_secured(&owner, &vol_path("d/child"), &mut fs)
        .expect("create child");
    vfs.remove_via_secured(&owner, &vol_path("d/child"), &mut fs)
        .expect("remove child");
    vfs.remove_via_secured(&owner, &vol_path("d"), &mut fs)
        .expect("rmdir once emptied");
}

#[test]
fn rmdir_missing_directory_is_not_found() {
    let (vfs, mut fs) = rustfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    assert_eq!(
        vfs.remove_via_secured(&owner, &vol_path("nope"), &mut fs),
        Err(VfsError::NotFound)
    );
}
