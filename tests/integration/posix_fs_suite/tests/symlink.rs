//! `symlink(2)` / `readlink(2)` conformance, and the follow matrix every
//! other operation takes around a link (`docs/src/filesystem/overview.md`
//! §"Which operations follow a final link").
//!
//! Driven against a **real** ARXFS volume through the real VFS policy layer,
//! so the on-disk spelling, the per-component resolution, and the follow
//! posture of each operation are all the production code paths.

use tairix_test_posix_fs_suite::*;

/// Create the link `link` naming `target` under the volume root, owned by the
/// volume's owner.
fn make_link(vfs: &Vfs, fs: &mut LiveFs, link: &str, target: &str) {
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);
    vfs.symlink_via_secured(&owner, &vol_path(link), fs, target)
        .unwrap_or_else(|e| panic!("create the link {link} -> {target}: {e:?}"));
}

#[test]
fn a_created_link_reads_its_target_back_verbatim() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.create_via_secured(&owner, &vol_path("target"), &mut fs)
        .expect("create the target");
    make_link(&vfs, &mut fs, "link", "target");

    assert_eq!(
        vfs.readlink_via_secured(&owner, &vol_path("link"), &mut fs),
        Ok(String::from("target"))
    );
}

#[test]
fn a_relative_target_carrying_dotdot_is_stored_as_typed() {
    // A target is *data*: the resolver's own grammar accepts `..` and
    // relative spellings, and nothing normalises them on the way in — the
    // spelling that comes back is the spelling that went in.
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    make_link(&vfs, &mut fs, "up", "../elsewhere/x");
    assert_eq!(
        vfs.readlink_via_secured(&owner, &vol_path("up"), &mut fs),
        Ok(String::from("../elsewhere/x"))
    );
}

#[test]
fn readlink_refuses_anything_that_is_not_a_link() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.create_via_secured(&owner, &vol_path("plain"), &mut fs)
        .expect("create");
    vfs.mkdir_via_secured(&owner, &vol_path("dir"), &mut fs)
        .expect("mkdir");

    // A name that is not a link has no target to read: the same domain
    // refusal for a file and for a directory.
    assert_eq!(
        vfs.readlink_via_secured(&owner, &vol_path("plain"), &mut fs),
        Err(VfsError::InvalidPath)
    );
    assert_eq!(
        vfs.readlink_via_secured(&owner, &vol_path("dir"), &mut fs),
        Err(VfsError::InvalidPath)
    );
    assert_eq!(
        vfs.readlink_via_secured(&owner, &vol_path("absent"), &mut fs),
        Err(VfsError::NotFound)
    );
}

#[test]
fn lstat_describes_the_link_and_stat_describes_its_target() {
    // The two readings of one name: `Keep` is POSIX `lstat`, `Follow` is
    // POSIX `stat`. They must report *different nodes*, not merely different
    // sizes.
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.create_via_secured(&owner, &vol_path("target"), &mut fs)
        .expect("create the target");
    vfs.write_via_secured(&owner, &vol_path("target"), &mut fs, 0, b"0123456789")
        .expect("write the target");
    make_link(&vfs, &mut fs, "link", "target");

    let kept = vfs
        .stat_via_secured(&owner, &vol_path("link"), &mut fs, FinalLink::Keep)
        .expect("lstat the link");
    assert_eq!(kept.kind, NodeKind::Symlink);
    // A link's own length is the length of the path it stores.
    assert_eq!(kept.size, "target".len() as u64);

    let followed = vfs
        .stat_via_secured(&owner, &vol_path("link"), &mut fs, FinalLink::Follow)
        .expect("stat the target");
    assert_eq!(followed.kind, NodeKind::RegularFile);
    assert_eq!(followed.size, 10);
    // Two different nodes, not one node described twice.
    assert_ne!(kept.node, followed.node);
    let target = vfs
        .stat_via_secured(&owner, &vol_path("target"), &mut fs, FinalLink::Keep)
        .expect("lstat the target");
    assert_eq!(followed.node, target.node);
}

#[test]
fn a_dangling_link_is_describable_only_without_following() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    // Nothing is resolved at creation, so a link may legitimately dangle.
    make_link(&vfs, &mut fs, "dangling", "nowhere");

    let kept = vfs
        .stat_via_secured(&owner, &vol_path("dangling"), &mut fs, FinalLink::Keep)
        .expect("lstat the dangling link");
    assert_eq!(kept.kind, NodeKind::Symlink);
    // Following it reports exactly what `stat(2)` reports: absent.
    assert_eq!(
        vfs.stat_via_secured(&owner, &vol_path("dangling"), &mut fs, FinalLink::Follow)
            .map(|info| info.kind),
        Err(VfsError::NotFound)
    );
    // Its target still reads back: the link itself is perfectly readable.
    assert_eq!(
        vfs.readlink_via_secured(&owner, &vol_path("dangling"), &mut fs),
        Ok(String::from("nowhere"))
    );
}

#[test]
fn a_write_through_a_link_reaches_the_target_and_leaves_the_link_a_link() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.create_via_secured(&owner, &vol_path("target"), &mut fs)
        .expect("create the target");
    make_link(&vfs, &mut fs, "link", "target");

    vfs.write_via_secured(&owner, &vol_path("link"), &mut fs, 0, b"through")
        .expect("write through the link");

    // The bytes landed in the target.
    let mut buf = [0u8; 16];
    let read = vfs
        .read_via_secured(&owner, &vol_path("target"), &mut fs, 0, &mut buf)
        .expect("read the target");
    assert_eq!(&buf[..read], b"through");
    // The link is still a link — a write never replaces it.
    assert_eq!(
        vfs.stat_via_secured(&owner, &vol_path("link"), &mut fs, FinalLink::Keep)
            .map(|info| info.kind),
        Ok(NodeKind::Symlink)
    );
    assert_eq!(
        vfs.readlink_via_secured(&owner, &vol_path("link"), &mut fs),
        Ok(String::from("target"))
    );
}

#[test]
fn a_truncate_through_a_link_reaches_the_target() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.create_via_secured(&owner, &vol_path("target"), &mut fs)
        .expect("create the target");
    vfs.write_via_secured(&owner, &vol_path("target"), &mut fs, 0, b"0123456789")
        .expect("write the target");
    make_link(&vfs, &mut fs, "link", "target");

    vfs.truncate_via_secured(&owner, &vol_path("link"), &mut fs, 4)
        .expect("truncate through the link");

    assert_eq!(
        vfs.stat_via_secured(&owner, &vol_path("target"), &mut fs, FinalLink::Keep)
            .map(|info| info.size),
        Ok(4)
    );
    // And the link is untouched.
    assert_eq!(
        vfs.stat_via_secured(&owner, &vol_path("link"), &mut fs, FinalLink::Keep)
            .map(|info| info.kind),
        Ok(NodeKind::Symlink)
    );
}

#[test]
fn a_write_or_truncate_through_a_dangling_link_is_not_found_and_creates_nothing() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    make_link(&vfs, &mut fs, "dangling", "nowhere");

    assert_eq!(
        vfs.write_via_secured(&owner, &vol_path("dangling"), &mut fs, 0, b"x"),
        Err(VfsError::NotFound)
    );
    assert_eq!(
        vfs.truncate_via_secured(&owner, &vol_path("dangling"), &mut fs, 0),
        Err(VfsError::NotFound)
    );
    // Neither call left the target behind.
    assert_eq!(
        vfs.stat_via_secured(&owner, &vol_path("nowhere"), &mut fs, FinalLink::Keep)
            .map(|info| info.kind),
        Err(VfsError::NotFound)
    );
}

#[test]
fn a_create_through_a_dangling_link_creates_the_target() {
    // `open` with `O_CREAT` follows a final link, so creating "through" a
    // dangling one creates the file the link names rather than reporting the
    // link's own name as taken.
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    make_link(&vfs, &mut fs, "dangling", "made");

    vfs.create_via_secured(&owner, &vol_path("dangling"), &mut fs)
        .expect("create through the dangling link");

    // The *target* now exists as a regular file...
    assert_eq!(
        vfs.stat_via_secured(&owner, &vol_path("made"), &mut fs, FinalLink::Keep)
            .map(|info| info.kind),
        Ok(NodeKind::RegularFile)
    );
    // ...and the link is still a link, now resolving to it.
    assert_eq!(
        vfs.stat_via_secured(&owner, &vol_path("dangling"), &mut fs, FinalLink::Keep)
            .map(|info| info.kind),
        Ok(NodeKind::Symlink)
    );
    assert_eq!(
        vfs.stat_via_secured(&owner, &vol_path("dangling"), &mut fs, FinalLink::Follow)
            .map(|info| info.kind),
        Ok(NodeKind::RegularFile)
    );
}

#[test]
fn mkdir_over_a_link_is_already_exists_live_or_dangling() {
    // POSIX `mkdir` keeps the name as typed, so a link occupying it is an
    // occupied name — whatever it resolves to.
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.mkdir_via_secured(&owner, &vol_path("real"), &mut fs)
        .expect("mkdir the target");
    make_link(&vfs, &mut fs, "live", "real");
    make_link(&vfs, &mut fs, "dead", "nowhere");

    assert_eq!(
        vfs.mkdir_via_secured(&owner, &vol_path("live"), &mut fs),
        Err(VfsError::AlreadyExists)
    );
    assert_eq!(
        vfs.mkdir_via_secured(&owner, &vol_path("dead"), &mut fs),
        Err(VfsError::AlreadyExists)
    );
    // Both links survive the refusal.
    for name in ["live", "dead"] {
        assert_eq!(
            vfs.stat_via_secured(&owner, &vol_path(name), &mut fs, FinalLink::Keep)
                .map(|info| info.kind),
            Ok(NodeKind::Symlink)
        );
    }
}

#[test]
fn symlink_never_replaces_an_existing_name() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.create_via_secured(&owner, &vol_path("taken"), &mut fs)
        .expect("create");
    assert_eq!(
        vfs.symlink_via_secured(&owner, &vol_path("taken"), &mut fs, "elsewhere"),
        Err(VfsError::AlreadyExists)
    );
    // The file it would have replaced is still a file.
    assert_eq!(
        vfs.stat_via_secured(&owner, &vol_path("taken"), &mut fs, FinalLink::Keep)
            .map(|info| info.kind),
        Ok(NodeKind::RegularFile)
    );
}

#[test]
fn unlink_removes_the_link_and_never_its_target() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.create_via_secured(&owner, &vol_path("target"), &mut fs)
        .expect("create the target");
    vfs.write_via_secured(&owner, &vol_path("target"), &mut fs, 0, b"kept")
        .expect("write the target");
    make_link(&vfs, &mut fs, "link", "target");

    vfs.remove_via_secured(&owner, &vol_path("link"), &mut fs, false)
        .expect("unlink the link");

    assert_eq!(
        vfs.stat_via_secured(&owner, &vol_path("link"), &mut fs, FinalLink::Keep)
            .map(|info| info.kind),
        Err(VfsError::NotFound)
    );
    let mut buf = [0u8; 8];
    let read = vfs
        .read_via_secured(&owner, &vol_path("target"), &mut fs, 0, &mut buf)
        .expect("the target survived");
    assert_eq!(&buf[..read], b"kept");
}

#[test]
fn rename_moves_the_link_not_what_it_names() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.create_via_secured(&owner, &vol_path("target"), &mut fs)
        .expect("create the target");
    make_link(&vfs, &mut fs, "link", "target");

    vfs.rename_via_secured(&owner, &vol_path("link"), &vol_path("moved"), &mut fs)
        .expect("rename the link");

    // The new name is the link, holding the same target.
    assert_eq!(
        vfs.stat_via_secured(&owner, &vol_path("moved"), &mut fs, FinalLink::Keep)
            .map(|info| info.kind),
        Ok(NodeKind::Symlink)
    );
    assert_eq!(
        vfs.readlink_via_secured(&owner, &vol_path("moved"), &mut fs),
        Ok(String::from("target"))
    );
    // The old name is gone and the target never moved.
    assert_eq!(
        vfs.stat_via_secured(&owner, &vol_path("link"), &mut fs, FinalLink::Keep)
            .map(|info| info.kind),
        Err(VfsError::NotFound)
    );
    assert_eq!(
        vfs.stat_via_secured(&owner, &vol_path("target"), &mut fs, FinalLink::Keep)
            .map(|info| info.kind),
        Ok(NodeKind::RegularFile)
    );
}

#[test]
fn a_cycle_is_refused_rather_than_walked() {
    // Two links naming each other: resolution is bounded, so the walk is
    // refused with the loop errno instead of running until something else
    // gives out.
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    make_link(&vfs, &mut fs, "a", "b");
    make_link(&vfs, &mut fs, "b", "a");

    assert_eq!(
        vfs.stat_via_secured(&owner, &vol_path("a"), &mut fs, FinalLink::Follow)
            .map(|info| info.kind),
        Err(VfsError::LinkLoop)
    );
    // A self-cycle is the same answer.
    make_link(&vfs, &mut fs, "self", "self");
    assert_eq!(
        vfs.stat_via_secured(&owner, &vol_path("self"), &mut fs, FinalLink::Follow)
            .map(|info| info.kind),
        Err(VfsError::LinkLoop)
    );
    // Each link is still perfectly describable as itself.
    for name in ["a", "b", "self"] {
        assert_eq!(
            vfs.stat_via_secured(&owner, &vol_path(name), &mut fs, FinalLink::Keep)
                .map(|info| info.kind),
            Ok(NodeKind::Symlink)
        );
    }
}

#[test]
fn a_link_is_listed_as_a_link_by_its_parent() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.create_via_secured(&owner, &vol_path("target"), &mut fs)
        .expect("create the target");
    make_link(&vfs, &mut fs, "link", "target");

    let listing = vfs
        .list_via_secured(&owner, &path(MOUNT), &mut fs, FinalLink::Follow)
        .expect("list the volume root");
    let link = listing
        .iter()
        .find(|(_, name)| name == "link")
        .expect("the link is listed");
    // The stream reports each child's *own* kind, so a link arrives as a
    // link however the listing was opened.
    assert_eq!(link.0.kind, NodeKind::Symlink);
}

#[test]
fn a_link_is_not_byte_readable() {
    // Its content is a path, reached only with `readlink`; a byte read fails
    // closed rather than handing the spelling out as file content.
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    make_link(&vfs, &mut fs, "link", "nowhere");

    let mut buf = [0u8; 16];
    assert_eq!(
        vfs.read_via_secured(&owner, &vol_path("link"), &mut fs, 0, &mut buf),
        Err(VfsError::NotFound)
    );
}

#[test]
fn an_interior_link_is_always_followed() {
    // A component used *as a directory* is resolved whatever the final
    // component's posture is: what matters there is what it names.
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.mkdir_via_secured(&owner, &vol_path("real"), &mut fs)
        .expect("mkdir");
    vfs.create_via_secured(&owner, &vol_path("real/inner"), &mut fs)
        .expect("create inside it");
    vfs.write_via_secured(&owner, &vol_path("real/inner"), &mut fs, 0, b"deep")
        .expect("write");
    make_link(&vfs, &mut fs, "door", "real");

    // Both postures resolve the interior `door`; only the final component's
    // treatment differs, and `inner` is not a link either way.
    for links in [FinalLink::Keep, FinalLink::Follow] {
        assert_eq!(
            vfs.stat_via_secured(&owner, &vol_path("door/inner"), &mut fs, links)
                .map(|info| info.kind),
            Ok(NodeKind::RegularFile)
        );
    }
    let mut buf = [0u8; 8];
    let read = vfs
        .read_via_secured(&owner, &vol_path("door/inner"), &mut fs, 0, &mut buf)
        .expect("read through the interior link");
    assert_eq!(&buf[..read], b"deep");
}

#[test]
fn dotdot_in_a_target_is_resolved_physically() {
    // `..` in a link's target pops the directory the walk *really came
    // through*, never the one the spelling suggests. Collapsing it textually
    // is the classic symlink-escape bug, so the test is built so a lexical
    // resolution would succeed and reach a *different* file — proving which
    // rule is in force rather than merely that something resolved.
    //
    // A caller's own path can never spell `..` (`Path::parse` refuses it), so
    // the `..` lives where it legitimately can: inside a stored target.
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.mkdir_via_secured(&owner, &vol_path("here"), &mut fs)
        .expect("mkdir here");
    vfs.mkdir_via_secured(&owner, &vol_path("there"), &mut fs)
        .expect("mkdir there");
    // Two files of the same leaf name, one at the volume root and one beside
    // the link: physical resolution must reach the first, lexical the second.
    vfs.create_via_secured(&owner, &vol_path("prize"), &mut fs)
        .expect("create the root prize");
    vfs.write_via_secured(&owner, &vol_path("prize"), &mut fs, 0, b"physical")
        .expect("write the root prize");
    vfs.create_via_secured(&owner, &vol_path("here/prize"), &mut fs)
        .expect("create the decoy");
    vfs.write_via_secured(&owner, &vol_path("here/prize"), &mut fs, 0, b"lexical!")
        .expect("write the decoy");

    // `here/door` names `there`; `there/out` names its parent's `prize`.
    make_link(&vfs, &mut fs, "here/door", "/there");
    make_link(&vfs, &mut fs, "there/out", "../prize");

    // Resolving `here/door/out` follows the interior `door` into `/there`,
    // then `out`'s `..` pops `/there` — the node it actually passed through —
    // landing at the volume root.
    let mut buf = [0u8; 16];
    let read = vfs
        .read_via_secured(&owner, &vol_path("here/door/out"), &mut fs, 0, &mut buf)
        .expect("read through the link chain");
    assert_eq!(
        &buf[..read],
        b"physical",
        "`..` popped the spelled parent instead of the real one"
    );
}

#[test]
fn a_link_cannot_escape_the_volume_that_stores_it() {
    // An absolute target resolves against the *mounted volume's* own root, so
    // a link on this volume cannot name the system tree above it — stricter
    // than POSIX, deliberately.
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.create_via_secured(&owner, &vol_path("inside"), &mut fs)
        .expect("create inside the volume");
    // Spelled as if it named the system security store.
    make_link(&vfs, &mut fs, "escape", "/System/Security/Users");
    // Nothing of that name exists *on this volume*, so it dangles rather than
    // reaching the real tree.
    assert_eq!(
        vfs.stat_via_secured(&owner, &vol_path("escape"), &mut fs, FinalLink::Follow)
            .map(|info| info.kind),
        Err(VfsError::NotFound)
    );
    // Climbing out of the volume root with `..` lands back at the root, as
    // POSIX specifies for `/..`.
    make_link(&vfs, &mut fs, "climb", "/../../inside");
    assert_eq!(
        vfs.stat_via_secured(&owner, &vol_path("climb"), &mut fs, FinalLink::Follow)
            .map(|info| info.kind),
        Ok(NodeKind::RegularFile)
    );
}

#[test]
fn creating_a_link_needs_a_writable_mount_and_an_authorised_parent() {
    // The call authorises exactly one thing: the right to create a name in
    // the link's own parent. It grants nothing over what the target names.
    let (read_only, mut fs) = arxfs_backed_vfs(true);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);
    assert_eq!(
        read_only.symlink_via_secured(&owner, &vol_path("nope"), &mut fs, "target"),
        Err(VfsError::ReadOnly)
    );

    let (vfs, mut fs) = arxfs_backed_vfs(false);
    // A principal who may not write the parent cannot create a name in it.
    let stranger = cred(ROOT_UID + 7, ROOT_GID + 7, &caps);
    assert_eq!(
        vfs.symlink_via_secured(&stranger, &vol_path("nope"), &mut fs, "target"),
        Err(VfsError::PermissionDenied)
    );
    assert_eq!(
        vfs.stat_via_secured(&owner, &vol_path("nope"), &mut fs, FinalLink::Keep)
            .map(|info| info.kind),
        Err(VfsError::NotFound)
    );
}

#[test]
fn an_unwalkable_target_is_refused_rather_than_stored() {
    // A target's *grammar* is checked before it is written, so a link that
    // could only ever fail is never created. Parsing is not resolving: `..`
    // and relative spellings stay legal.
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    let over_long = "x".repeat(tairix_abi::FS_SYMLINK_MAX + 1);
    for (target, why) in [("", "empty"), (over_long.as_str(), "over the bound")] {
        assert!(
            vfs.symlink_via_secured(&owner, &vol_path("bad"), &mut fs, target)
                .is_err(),
            "a {why} target must be refused"
        );
        assert_eq!(
            vfs.stat_via_secured(&owner, &vol_path("bad"), &mut fs, FinalLink::Keep)
                .map(|info| info.kind),
            Err(VfsError::NotFound),
            "a refused {why} target must leave no name behind"
        );
    }
}
