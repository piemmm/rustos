//! `link(2)` conformance: a second directory entry for one inode, and the
//! lifecycle that makes it safe — a name goes, the storage stays until the
//! last one does (`plans/SYMLINKS.md` S6).
//!
//! Driven against a **real** ARXFS volume through the real VFS policy layer,
//! so the on-disk `nlink` accounting, the per-component resolution, and the
//! follow posture of each operand are all the production code paths.

use tairix_test_posix_fs_suite::*;

/// Add `link` as a second name for `existing` under the volume root, owned by
/// the volume's owner, keeping the existing name's final component as typed.
fn make_link(vfs: &Vfs, fs: &mut LiveFs, existing: &str, link: &str) -> Result<(), VfsError> {
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);
    vfs.link_via_secured(
        &owner,
        &vol_path(existing),
        &vol_path(link),
        fs,
        FinalLink::Keep,
    )
}

/// A file at `name` holding `body`.
fn make_file(vfs: &Vfs, fs: &mut LiveFs, name: &str, body: &[u8]) {
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);
    vfs.create_via_secured(&owner, &vol_path(name), fs)
        .unwrap_or_else(|e| panic!("create {name}: {e:?}"));
    if !body.is_empty() {
        vfs.write_via_secured(&owner, &vol_path(name), fs, 0, body)
            .unwrap_or_else(|e| panic!("write {name}: {e:?}"));
    }
}

#[test]
fn two_names_reach_one_inode_and_one_stat() {
    // The defining property: not two files with equal contents but one file
    // with two names, so both paths stat to the same node and count two.
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    make_file(&vfs, &mut fs, "first", b"shared");
    make_link(&vfs, &mut fs, "first", "second").expect("add a second name");

    let first = vfs
        .stat_via_secured(&owner, &vol_path("first"), &mut fs, FinalLink::Keep)
        .expect("stat the first name");
    let second = vfs
        .stat_via_secured(&owner, &vol_path("second"), &mut fs, FinalLink::Keep)
        .expect("stat the second name");
    assert_eq!(first.node, second.node, "one node, two names");
    assert_eq!(first.nlink, 2);
    assert_eq!(second.nlink, 2);
    assert_eq!(first.size, second.size);
}

#[test]
fn a_write_through_one_name_is_visible_through_the_other() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    make_file(&vfs, &mut fs, "first", b"before");
    make_link(&vfs, &mut fs, "first", "second").expect("add a second name");

    vfs.write_via_secured(&owner, &vol_path("second"), &mut fs, 0, b"after ")
        .expect("write through the second name");
    let mut buf = [0u8; 6];
    let read = vfs
        .read_via_secured(&owner, &vol_path("first"), &mut fs, 0, &mut buf)
        .expect("read through the first name");
    assert_eq!(&buf[..read], b"after ");
}

#[test]
fn unlinking_one_name_leaves_the_other_readable() {
    // The lifecycle change this stage exists for: an unlink that is not the
    // last frees nothing, or it destroys data the remaining name reaches.
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    make_file(&vfs, &mut fs, "first", b"payload");
    make_link(&vfs, &mut fs, "first", "second").expect("add a second name");

    vfs.remove_via_secured(&owner, &vol_path("first"), &mut fs, false)
        .expect("drop the first name");
    assert_eq!(
        vfs.stat_via_secured(&owner, &vol_path("first"), &mut fs, FinalLink::Keep)
            .map(|i| i.size),
        Err(VfsError::NotFound)
    );
    let survivor = vfs
        .stat_via_secured(&owner, &vol_path("second"), &mut fs, FinalLink::Keep)
        .expect("the other name still resolves");
    assert_eq!(survivor.nlink, 1, "one name went, one remains");

    let mut buf = [0u8; 7];
    let read = vfs
        .read_via_secured(&owner, &vol_path("second"), &mut fs, 0, &mut buf)
        .expect("the bytes are still there");
    assert_eq!(&buf[..read], b"payload");
}

#[test]
fn unlinking_the_last_name_frees_the_storage() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    let empty = fs.stats().expect("stats").free_blocks;
    make_file(&vfs, &mut fs, "first", b"payload");
    make_link(&vfs, &mut fs, "first", "second").expect("add a second name");
    let with_data = fs.stats().expect("stats").free_blocks;
    assert!(with_data < empty, "the file occupies blocks");

    vfs.remove_via_secured(&owner, &vol_path("first"), &mut fs, false)
        .expect("drop one name");
    assert_eq!(
        fs.stats().expect("stats").free_blocks,
        with_data,
        "a name went, no storage did"
    );

    vfs.remove_via_secured(&owner, &vol_path("second"), &mut fs, false)
        .expect("drop the last name");
    assert!(
        fs.stats().expect("stats").free_blocks > with_data,
        "the last name returns the blocks"
    );
}

#[test]
fn the_count_changes_with_each_name() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);
    let count = |fs: &mut LiveFs, name: &str| {
        vfs.stat_via_secured(&owner, &vol_path(name), fs, FinalLink::Keep)
            .expect("stat")
            .nlink
    };

    make_file(&vfs, &mut fs, "one", b"x");
    assert_eq!(count(&mut fs, "one"), 1);
    make_link(&vfs, &mut fs, "one", "two").expect("second name");
    assert_eq!(count(&mut fs, "one"), 2);
    make_link(&vfs, &mut fs, "two", "three").expect("third name");
    assert_eq!(count(&mut fs, "one"), 3);
    vfs.remove_via_secured(&owner, &vol_path("two"), &mut fs, false)
        .expect("drop one");
    assert_eq!(count(&mut fs, "one"), 2);
}

#[test]
fn a_directory_is_refused_and_nothing_is_created() {
    // The tree must stay a tree: that is what makes the resolver's physical
    // `..` name the directory the walk actually came through.
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.mkdir_via_secured(&owner, &vol_path("dir"), &mut fs)
        .expect("create the directory");
    assert_eq!(
        make_link(&vfs, &mut fs, "dir", "alias"),
        Err(VfsError::IsADirectory)
    );
    assert_eq!(
        vfs.stat_via_secured(&owner, &vol_path("alias"), &mut fs, FinalLink::Keep)
            .map(|i| i.size),
        Err(VfsError::NotFound),
        "the refused name was never created"
    );
}

#[test]
fn a_new_name_off_this_volume_has_no_backing_to_land_in() {
    // A directory entry addresses an inode in its own backing, so the new
    // name has to be on this volume. A path under the in-RAM layout has no
    // driver behind it at all, so there is nowhere to write the entry — the
    // cross-*volume* refusal between two backed mounts is exercised in
    // `kernel/core`'s delegation suite, where a second one can be mounted.
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    make_file(&vfs, &mut fs, "here", b"x");
    let outside = Path::parse("/Users/elsewhere").expect("a well-formed path");
    assert_eq!(
        vfs.link_via_secured(
            &owner,
            &vol_path("here"),
            &outside,
            &mut fs,
            FinalLink::Keep
        ),
        Err(VfsError::NotFound)
    );
}

#[test]
fn a_taken_name_is_refused_and_replaces_nothing() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    make_file(&vfs, &mut fs, "first", b"first body");
    make_file(&vfs, &mut fs, "taken", b"occupant");
    assert_eq!(
        make_link(&vfs, &mut fs, "first", "taken"),
        Err(VfsError::AlreadyExists)
    );

    // The occupant is untouched: same node, same bytes.
    let mut buf = [0u8; 8];
    let read = vfs
        .read_via_secured(&owner, &vol_path("taken"), &mut fs, 0, &mut buf)
        .expect("read the occupant");
    assert_eq!(&buf[..read], b"occupant");
    assert_eq!(
        vfs.stat_via_secured(&owner, &vol_path("first"), &mut fs, FinalLink::Keep)
            .expect("stat")
            .nlink,
        1,
        "a refused link raises no count"
    );
}

#[test]
fn the_default_posture_names_a_symbolic_link_itself() {
    // POSIX `link()`: the node that gains a name is the one the caller
    // spelled, so a symbolic link planted on the way cannot redirect the new
    // name onto an object the caller never asked for.
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    make_file(&vfs, &mut fs, "target", b"target body");
    vfs.symlink_via_secured(&owner, &vol_path("sym"), &mut fs, "target")
        .expect("create the symbolic link");
    make_link(&vfs, &mut fs, "sym", "sym2").expect("a second name for the link");

    let second = vfs
        .stat_via_secured(&owner, &vol_path("sym2"), &mut fs, FinalLink::Keep)
        .expect("lstat the second name");
    assert_eq!(second.kind, NodeKind::Symlink, "the link itself gained it");
    assert_eq!(second.nlink, 2);
    assert_eq!(
        vfs.readlink_via_secured(&owner, &vol_path("sym2"), &mut fs),
        Ok(String::from("target")),
        "both names read the same stored target"
    );
    // The target itself gained nothing.
    assert_eq!(
        vfs.stat_via_secured(&owner, &vol_path("target"), &mut fs, FinalLink::Keep)
            .expect("stat")
            .nlink,
        1
    );
}

#[test]
fn the_follow_posture_names_what_a_symbolic_link_names() {
    // `linkat(AT_SYMLINK_FOLLOW)`, which `ln -L` asks for: the second name
    // goes to the target, and the link is left alone.
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    make_file(&vfs, &mut fs, "target", b"target body");
    vfs.symlink_via_secured(&owner, &vol_path("sym"), &mut fs, "target")
        .expect("create the symbolic link");
    vfs.link_via_secured(
        &owner,
        &vol_path("sym"),
        &vol_path("hard"),
        &mut fs,
        FinalLink::Follow,
    )
    .expect("a second name for the target");

    let hard = vfs
        .stat_via_secured(&owner, &vol_path("hard"), &mut fs, FinalLink::Keep)
        .expect("lstat the new name");
    assert_eq!(hard.kind, NodeKind::RegularFile, "the target gained it");
    assert_eq!(hard.nlink, 2);
    let target = vfs
        .stat_via_secured(&owner, &vol_path("target"), &mut fs, FinalLink::Keep)
        .expect("stat the target");
    assert_eq!(hard.node, target.node);
    // The symbolic link is untouched and still a link with one name.
    let sym = vfs
        .stat_via_secured(&owner, &vol_path("sym"), &mut fs, FinalLink::Keep)
        .expect("lstat the link");
    assert_eq!(sym.kind, NodeKind::Symlink);
    assert_eq!(sym.nlink, 1);
}

#[test]
fn a_dangling_link_has_nothing_to_follow_to() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.symlink_via_secured(&owner, &vol_path("dangling"), &mut fs, "absent")
        .expect("create the dangling link");
    assert_eq!(
        vfs.link_via_secured(
            &owner,
            &vol_path("dangling"),
            &vol_path("hard"),
            &mut fs,
            FinalLink::Follow,
        ),
        Err(VfsError::NotFound)
    );
    // Kept, the link itself is a perfectly good node to name twice.
    make_link(&vfs, &mut fs, "dangling", "alias").expect("name the link itself");
    assert_eq!(
        vfs.readlink_via_secured(&owner, &vol_path("alias"), &mut fs),
        Ok(String::from("absent"))
    );
}

#[test]
fn a_missing_existing_name_creates_nothing() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    assert_eq!(
        make_link(&vfs, &mut fs, "absent", "alias"),
        Err(VfsError::NotFound)
    );
    assert_eq!(
        vfs.stat_via_secured(&owner, &vol_path("alias"), &mut fs, FinalLink::Keep)
            .map(|i| i.size),
        Err(VfsError::NotFound)
    );
}

#[test]
fn a_read_only_mount_refuses_and_an_unauthorised_parent_refuses() {
    // The same two gates every other mutation passes: the mount's own flag,
    // then the caller's write permission on the new name's parent.
    let (read_only, mut fs) = arxfs_backed_vfs(true);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);
    assert_eq!(
        read_only.link_via_secured(
            &owner,
            &vol_path("anything"),
            &vol_path("alias"),
            &mut fs,
            FinalLink::Keep,
        ),
        Err(VfsError::ReadOnly)
    );

    let (vfs, mut fs) = arxfs_backed_vfs(false);
    make_file(&vfs, &mut fs, "first", b"x");
    let stranger = cred(ROOT_UID + 7, ROOT_GID + 7, &caps);
    assert_eq!(
        vfs.link_via_secured(
            &stranger,
            &vol_path("first"),
            &vol_path("alias"),
            &mut fs,
            FinalLink::Keep,
        ),
        Err(VfsError::PermissionDenied)
    );
    assert_eq!(
        vfs.stat_via_secured(&owner, &vol_path("alias"), &mut fs, FinalLink::Keep)
            .map(|i| i.size),
        Err(VfsError::NotFound),
        "a refused link leaves no name behind"
    );
}

#[test]
fn a_second_name_survives_a_remount_with_its_count() {
    // The count is on-disk state, not a mount-time derivation.
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    make_file(&vfs, &mut fs, "first", b"payload");
    make_link(&vfs, &mut fs, "first", "second").expect("add a second name");

    let mut remounted = remount(fs);
    let stat = vfs
        .stat_via_secured(&owner, &vol_path("second"), &mut remounted, FinalLink::Keep)
        .expect("stat after the remount");
    assert_eq!(stat.nlink, 2);
    let mut buf = [0u8; 7];
    let read = vfs
        .read_via_secured(&owner, &vol_path("second"), &mut remounted, 0, &mut buf)
        .expect("read after the remount");
    assert_eq!(&buf[..read], b"payload");
}
