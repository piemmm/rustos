//! Errno conformance: the stable user/kernel [`Errno`] a failed operation
//! surfaces at the syscall boundary. The refusals a tool must tell apart
//! carry dedicated codes (`AlreadyExists`, `NotADirectory`, `NotEmpty`);
//! `abi-v1` still has no dedicated `EISDIR`/`EINVAL`/`EIO`, so those remain
//! many-to-one (documented on `VfsError::to_errno`). This suite pins the
//! contract so a future change cannot silently alter it.

use rustos_test_posix_fs_suite::*;

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
    assert_eq!(VfsError::IsADirectory.to_errno(), Errno::OutOfRange);
    assert_eq!(VfsError::AlreadyExists.to_errno(), Errno::AlreadyExists);
    assert_eq!(VfsError::NotEmpty.to_errno(), Errno::NotEmpty);
    assert_eq!(VfsError::CrossVolume.to_errno(), Errno::CrossVolume);
    assert_eq!(VfsError::Io.to_errno(), Errno::NotImplemented);
}

#[test]
fn a_real_missing_lookup_surfaces_enoent() {
    let (vfs, mut fs) = rustfs_backed_vfs(false);
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);

    let err = vfs
        .stat_via_secured(&owner, &vol_path("absent"), &mut fs)
        .map(|info| info.size)
        .expect_err("a missing path fails");
    assert_eq!(err.to_errno(), Errno::NotFound);
}

#[test]
fn a_real_permission_denial_surfaces_eacces() {
    let (vfs, mut fs) = rustfs_backed_vfs(false);
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
