//! Errno conformance: the stable user/kernel [`Errno`] a failed operation
//! surfaces at the syscall boundary. The refusals a tool must tell apart
//! carry dedicated codes (`AlreadyExists`, `NotADirectory`, `IsADirectory`,
//! `NotEmpty`, `TooManyLinks`, and `DeviceFault` — the `EIO` analogue an
//! unrecoverable backing fault reports); `abi-v1` still has no dedicated
//! `EINVAL`, so a malformed path or attribute key — and a rename that would
//! make a directory its own descendant — remains many-to-one (documented on
//! `VfsError::to_errno`). This suite pins the contract so a future change
//! cannot silently alter it.

use tairix_test_posix_fs_suite::*;

#[test]
fn vfs_error_maps_to_the_documented_stable_errno() {
    assert_eq!(VfsError::NotFound.to_errno(), Errno::NotFound);
    assert_eq!(
        VfsError::PermissionDenied.to_errno(),
        Errno::PermissionDenied
    );
    assert_eq!(VfsError::ReadOnly.to_errno(), Errno::PermissionDenied);
    assert_eq!(VfsError::InvalidPath.to_errno(), Errno::OutOfRange);
    assert_eq!(VfsError::NotADirectory.to_errno(), Errno::NotADirectory);
    assert_eq!(VfsError::IsADirectory.to_errno(), Errno::IsADirectory);
    assert_eq!(VfsError::AlreadyExists.to_errno(), Errno::AlreadyExists);
    assert_eq!(VfsError::NotEmpty.to_errno(), Errno::NotEmpty);
    assert_eq!(VfsError::DirectoryCycle.to_errno(), Errno::OutOfRange);
    assert_eq!(VfsError::CrossVolume.to_errno(), Errno::CrossVolume);
    assert_eq!(VfsError::TooManyLinks.to_errno(), Errno::TooManyLinks);
    assert_eq!(VfsError::Io.to_errno(), Errno::DeviceFault);
}

#[test]
fn a_real_missing_lookup_surfaces_enoent() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    let err = vfs
        .stat_via_secured(&owner, &vol_path("absent"), &mut fs, FinalLink::Follow)
        .map(|info| info.size)
        .expect_err("a missing path fails");
    assert_eq!(err.to_errno(), Errno::NotFound);
}

#[test]
fn a_real_permission_denial_surfaces_eacces() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let admin = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.create_via_secured(&admin, &vol_path("guarded"), &mut fs)
        .expect("create");
    let node = root_node_id(&mut fs, b"guarded");
    fs.set_security(node, NodeSecurity::new(0o600, 1000, 1000))
        .expect("make owner-only");

    let mut buf = [0u8; 4];
    let stranger = cred(2000, 2000, &caps);
    let err = vfs
        .read_via_secured(&stranger, &vol_path("guarded"), &mut fs, 0, &mut buf)
        .expect_err("a stranger is denied");
    assert_eq!(err.to_errno(), Errno::PermissionDenied);
}

/// The three structural conflicts a real volume can raise reach user space
/// as three different codes.
///
/// They shared one driver value until each was given its own, so which one a
/// tool saw depended on the mapping the call site happened to pick — and a
/// taken name could arrive as `EWOULDBLOCK`, advice to retry something no
/// retry clears.
#[test]
fn each_structural_conflict_surfaces_its_own_errno() {
    let (vfs, mut fs) = arxfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    vfs.mkdir_via_secured(&owner, &vol_path("outer"), &mut fs)
        .expect("mkdir outer");
    vfs.mkdir_via_secured(&owner, &vol_path("outer/inner"), &mut fs)
        .expect("mkdir inner");
    vfs.mkdir_via_secured(&owner, &vol_path("spare"), &mut fs)
        .expect("mkdir spare");

    let taken = vfs
        .mkdir_via_secured(&owner, &vol_path("outer"), &mut fs)
        .expect_err("the name is taken");
    let populated = vfs
        .rename_via_secured(&owner, &vol_path("spare"), &vol_path("outer"), &mut fs)
        .expect_err("the destination still holds an entry");
    let cycle = vfs
        .rename_via_secured(
            &owner,
            &vol_path("outer"),
            &vol_path("outer/inner/outer"),
            &mut fs,
        )
        .expect_err("a directory cannot become its own descendant");

    assert_eq!(taken.to_errno(), Errno::AlreadyExists);
    assert_eq!(populated.to_errno(), Errno::NotEmpty);
    assert_eq!(cycle.to_errno(), Errno::OutOfRange);
    for errno in [taken.to_errno(), populated.to_errno(), cycle.to_errno()] {
        assert_ne!(errno, Errno::WouldBlock, "no conflict invites a retry");
    }
}
