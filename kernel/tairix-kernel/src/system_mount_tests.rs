//! Host unit tests for the `/System` `fs_*` mount wiring (`system_mount`).
//!
//! These cover the piece that is testable without a live disk or the global
//! boot statics: the production VFS layout the `/System` volume is mounted
//! under ([`system_vfs`]). The `Box<dyn KernelFs>` forwarding impls are
//! covered by the shared wrapper conformance suite (`crate::kernel_fs`),
//! and the live `install_system_mount` path (a second `'static` window onto
//! the boot disk) is exercised by the FS QEMU vertical.

use tairix_kernel_core::Path;

use super::system_vfs;

#[test]
fn system_vfs_mounts_the_writable_volume_as_root() {
    // The corrected layering: the encrypted, writable root volume *is* `/`,
    // so `/` itself and the persistent top-level trees (`/Users`, `/Apps`,
    // `/Storage`) resolve to a writable, driver-backed mount — not the
    // volatile in-RAM tree, and not the read-only `/System` volume. This is
    // the regression guard for the "writes outside /System/{Logs,Settings}
    // were non-persistent" defect.
    let vfs = system_vfs().expect("the production VFS builds");
    let mounts = vfs.mounts();
    let root = mounts.resolve(&Path::parse("/").expect("valid"));
    assert_eq!(root.path(), &Path::parse("/").expect("valid"));
    assert!(!root.is_read_only(), "/ is writable");
    let root_handle = root
        .backing()
        .expect("/ is driver-backed by the writable root volume");
    // The whole-volume root mount roots at the volume's own root.
    assert!(
        root.backing_subtree().is_empty(),
        "/ is a whole-volume mount (no rebasing)"
    );

    for top in ["/Users", "/Apps", "/Storage"] {
        let under = Path::parse(&alloc::format!("{top}/alice/file")).expect("valid path");
        let mount = mounts.resolve(&under);
        assert_eq!(
            mount.path(),
            &Path::parse(top).expect("valid"),
            "{top} is its own mount"
        );
        assert!(!mount.is_read_only(), "{top} is writable (persistent)");
        assert_eq!(
            mount.backing(),
            Some(root_handle),
            "{top} is backed by the one writable root volume"
        );
        // Rebased onto the volume's own same-named directory so the one
        // driver resolves from its own root.
        assert_eq!(
            mount.backing_subtree(),
            &[alloc::string::String::from(top.trim_start_matches('/'))],
            "{top} is rebased onto the volume's own {top}"
        );
    }
}

#[test]
fn system_vfs_shadows_root_with_the_read_only_system_volume() {
    // The read-only `/System` volume is mounted *over* the writable root at
    // `/System`: a path under `/System` resolves to a read-only,
    // driver-backed mount on a *different* volume from `/`, so reads delegate
    // to the immutable volume and writes are refused.
    let vfs = system_vfs().expect("the production VFS builds");
    let root_handle = vfs
        .mounts()
        .resolve(&Path::parse("/").expect("valid"))
        .backing()
        .expect("/ is driver-backed");

    let under_system = Path::parse("/System/Drivers/x").expect("valid path");
    let mounts = vfs.mounts();
    let mount = mounts.resolve(&under_system);
    assert_eq!(mount.path(), &Path::parse("/System").expect("valid"));
    assert!(mount.is_read_only(), "/System is mounted read-only");
    let system_handle = mount
        .backing()
        .expect("/System is driver-backed so the VFS delegates to the live volume");
    assert!(
        mount.backing_subtree().is_empty(),
        "/System is a whole-volume mount (its content is the volume root)"
    );
    assert_ne!(
        system_handle, root_handle,
        "/System is a different volume from the writable root"
    );
}

#[test]
fn system_vfs_carves_logs_and_settings_back_to_the_writable_volume() {
    use tairix_abi::driver::filesystem::MountFlags;

    // `/System/Logs` and `/System/Settings` are the only writable paths
    // beneath `/System`: each is a `nosuid,nodev,noexec` writable sub-mount of
    // the *writable root volume* (the same handle that backs `/`), rebased
    // onto that volume's own `/System/<name>` directory. `MountTable`
    // longest-prefix resolution makes the writable child shadow the read-only
    // `/System`.
    let vfs = system_vfs().expect("the production VFS builds");
    let root_handle = vfs
        .mounts()
        .resolve(&Path::parse("/").expect("valid"))
        .backing()
        .expect("/ is driver-backed");
    let system_handle = vfs
        .mounts()
        .resolve(&Path::parse("/System/Drivers/x").expect("valid"))
        .backing()
        .expect("/System is driver-backed");

    let nosuid_nodev_noexec = MountFlags::NOSUID
        .union(MountFlags::NODEV)
        .union(MountFlags::NOEXEC);
    let mounts = vfs.mounts();
    for name in ["Logs", "Settings"] {
        let under = Path::parse(&alloc::format!("/System/{name}/file")).expect("valid path");
        let mount = mounts.resolve(&under);
        assert_eq!(
            mount.path(),
            &Path::parse(&alloc::format!("/System/{name}")).expect("valid"),
            "the writable {name} sub-mount shadows the read-only /System"
        );
        assert!(!mount.is_read_only(), "/System/{name} is writable");
        assert_eq!(
            mount.flags(),
            nosuid_nodev_noexec,
            "/System/{name} is mounted nosuid,nodev,noexec"
        );
        let handle = mount
            .backing()
            .expect("the writable sub-mount is driver-backed");
        assert_eq!(
            handle, root_handle,
            "the writable {name} subtree is the one writable root volume"
        );
        assert_ne!(
            handle, system_handle,
            "the writable backing is a different volume from read-only /System"
        );
        // Rebased onto the backing volume's own `/System/<name>` directory.
        assert_eq!(
            mount.backing_subtree(),
            &[
                alloc::string::String::from("System"),
                alloc::string::String::from(name),
            ],
            "/System/{name} is rebased onto the volume's own /System/{name}"
        );
    }
}
