//! `truncate(2)` conformance: shrinking, growing (with a zero-filled
//! extension), `EISDIR`, and `ENOENT`.

use tairix_test_posix_fs_suite::*;

#[test]
fn truncate_shrinks_a_file() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.create_via_secured(&owner, &vol_path("f"), &mut fs)
        .expect("create");
    vfs.write_via_secured(&owner, &vol_path("f"), &mut fs, 0, b"0123456789")
        .expect("write");

    vfs.truncate_via_secured(&owner, &vol_path("f"), &mut fs, 4)
        .expect("truncate to 4");

    let info = vfs
        .stat_via_secured(&owner, &vol_path("f"), &mut fs)
        .expect("stat");
    assert_eq!(info.size, 4);

    let mut buf = [0u8; 16];
    let read = vfs
        .read_via_secured(&owner, &vol_path("f"), &mut fs, 0, &mut buf)
        .expect("read");
    assert_eq!(&buf[..read], b"0123");
}

#[test]
fn truncate_grows_a_file_with_zero_fill() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.create_via_secured(&owner, &vol_path("g"), &mut fs)
        .expect("create");
    vfs.write_via_secured(&owner, &vol_path("g"), &mut fs, 0, b"abc")
        .expect("write");

    vfs.truncate_via_secured(&owner, &vol_path("g"), &mut fs, 8)
        .expect("grow to 8");

    let info = vfs
        .stat_via_secured(&owner, &vol_path("g"), &mut fs)
        .expect("stat");
    assert_eq!(info.size, 8);

    let mut buf = [0xFFu8; 8];
    let read = vfs
        .read_via_secured(&owner, &vol_path("g"), &mut fs, 0, &mut buf)
        .expect("read");
    assert_eq!(read, 8);
    assert_eq!(&buf[..3], b"abc");
    assert_eq!(&buf[3..], &[0u8; 5]);
}

#[test]
fn truncate_of_a_directory_is_is_a_directory() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.mkdir_via_secured(&owner, &vol_path("d"), &mut fs)
        .expect("mkdir");
    assert_eq!(
        vfs.truncate_via_secured(&owner, &vol_path("d"), &mut fs, 0),
        Err(VfsError::IsADirectory)
    );
}

#[test]
fn truncate_of_a_missing_file_is_not_found() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    assert_eq!(
        vfs.truncate_via_secured(&owner, &vol_path("missing"), &mut fs, 0),
        Err(VfsError::NotFound)
    );
}
