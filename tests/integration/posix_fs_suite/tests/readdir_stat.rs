//! `readdir`/`stat` conformance: directory listing, `ENOTDIR` on listing
//! a file, and the size/kind a `stat` reports.

use tairix_test_posix_fs_suite::*;

#[test]
fn readdir_lists_created_children() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.create_via_secured(&owner, &vol_path("alpha"), &mut fs)
        .expect("create alpha");
    vfs.create_via_secured(&owner, &vol_path("beta"), &mut fs)
        .expect("create beta");
    vfs.mkdir_via_secured(&owner, &vol_path("gamma"), &mut fs)
        .expect("mkdir gamma");

    let mut names: Vec<(NodeKind, String)> = vfs
        .list_via_secured(&owner, &path(MOUNT), &mut fs, FinalLink::Follow)
        .expect("list mount root")
        .into_iter()
        .map(|(info, name)| (info.kind, name))
        .collect();
    names.sort_by(|a, b| a.1.cmp(&b.1));
    assert_eq!(
        names,
        [
            (NodeKind::RegularFile, String::from("alpha")),
            (NodeKind::RegularFile, String::from("beta")),
            (NodeKind::Directory, String::from("gamma")),
        ]
    );
}

#[test]
fn readdir_of_a_file_is_not_a_directory() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.create_via_secured(&owner, &vol_path("file"), &mut fs)
        .expect("create");
    assert_eq!(
        vfs.list_via_secured(&owner, &vol_path("file"), &mut fs, FinalLink::Follow),
        Err(VfsError::NotADirectory)
    );
}

#[test]
fn readdir_of_empty_directory_is_empty() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.mkdir_via_secured(&owner, &vol_path("empty"), &mut fs)
        .expect("mkdir");
    let names = vfs
        .list_via_secured(&owner, &vol_path("empty"), &mut fs, FinalLink::Follow)
        .expect("list");
    assert!(names.is_empty());
}

#[test]
fn stat_reports_size_after_writes() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.create_via_secured(&owner, &vol_path("sized"), &mut fs)
        .expect("create");
    vfs.write_via_secured(&owner, &vol_path("sized"), &mut fs, 0, b"twelve bytes")
        .expect("write");

    let info = vfs
        .stat_via_secured(&owner, &vol_path("sized"), &mut fs, FinalLink::Follow)
        .expect("stat");
    assert_eq!(info.kind, NodeKind::RegularFile);
    assert_eq!(info.size, 12);
}

#[test]
fn stat_of_a_missing_path_is_not_found() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    assert_eq!(
        vfs.stat_via_secured(&owner, &vol_path("ghost"), &mut fs, FinalLink::Follow)
            .map(|info| info.size),
        Err(VfsError::NotFound)
    );
}
