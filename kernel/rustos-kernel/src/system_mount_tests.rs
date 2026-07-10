//! Host unit tests for the `/System` `fs_*` mount wiring (`system_mount`).
//!
//! These cover the two pieces that are testable without a live disk or the
//! global boot statics: the production VFS layout the `/System` volume is
//! mounted under ([`system_vfs`]), and the `Box<dyn KernelFs>` forwarding
//! impls that let the board-specific mounted driver be the single concrete
//! type the boot statics name. The live `install_system_mount` path (a
//! second `'static` window onto the boot disk) is exercised by the FS QEMU
//! vertical, not a host test.

use alloc::boxed::Box;

use rustos_abi::driver::filesystem::{
    DirEntry, FilesystemRead, FilesystemSecurity, FilesystemStats, FilesystemWrite, NodeId,
    NodeInfo, NodeKind, NodeSecurity, VolumeStats,
};
use rustos_abi::DriverError;
use rustos_kernel_core::Path;

use super::{system_vfs, KernelFs};

/// A mock filesystem driver whose every method returns a distinct sentinel,
/// so a forwarded call through `Box<dyn KernelFs>` is observable by its
/// return value alone (no shared state, so the mock can be `'static`-boxed).
struct SentinelFs;

impl FilesystemRead for SentinelFs {
    fn root(&self) -> NodeId {
        NodeId::from_raw(7)
    }

    fn node_info(&mut self, _node: NodeId) -> Result<NodeInfo, DriverError> {
        Err(DriverError::Unsupported)
    }

    fn lookup(&mut self, _dir: NodeId, _name: &[u8]) -> Result<NodeId, DriverError> {
        Ok(NodeId::from_raw(11))
    }

    fn read_at(
        &mut self,
        _file: NodeId,
        _offset: u64,
        _buf: &mut [u8],
    ) -> Result<usize, DriverError> {
        Ok(3)
    }

    fn read_dir(
        &mut self,
        _dir: NodeId,
        _index: u64,
        _name_out: &mut [u8],
    ) -> Result<Option<DirEntry>, DriverError> {
        Ok(None)
    }
}

impl FilesystemWrite for SentinelFs {
    fn create(
        &mut self,
        _dir: NodeId,
        _name: &[u8],
        _kind: NodeKind,
    ) -> Result<NodeId, DriverError> {
        Ok(NodeId::from_raw(13))
    }

    fn write_at(
        &mut self,
        _dir: NodeId,
        _name: &[u8],
        _offset: u64,
        _data: &[u8],
    ) -> Result<usize, DriverError> {
        Ok(5)
    }

    fn truncate(&mut self, _dir: NodeId, _name: &[u8], _size: u64) -> Result<(), DriverError> {
        Ok(())
    }

    fn remove(&mut self, _dir: NodeId, _name: &[u8]) -> Result<(), DriverError> {
        Ok(())
    }

    fn rename(
        &mut self,
        _src_dir: NodeId,
        _src_name: &[u8],
        _dst_dir: NodeId,
        _dst_name: &[u8],
    ) -> Result<(), DriverError> {
        // A distinct sentinel so the forwarding test observes the call.
        Err(DriverError::Busy)
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        Ok(())
    }
}

impl FilesystemSecurity for SentinelFs {
    fn security(&mut self, _node: NodeId) -> Result<NodeSecurity, DriverError> {
        Ok(NodeSecurity::new(0o644, 1, 2))
    }

    fn set_security(&mut self, _node: NodeId, _security: NodeSecurity) -> Result<(), DriverError> {
        // The same sentinel refusal the write surface reports.
        Err(DriverError::Busy)
    }
}

impl FilesystemStats for SentinelFs {
    fn stats(&mut self) -> Result<VolumeStats, DriverError> {
        Ok(VolumeStats {
            block_size: 512,
            total_blocks: 17,
            free_blocks: 17,
            avail_blocks: 17,
            files: 0,
            files_free: 0,
        })
    }
}

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
    use rustos_abi::driver::filesystem::MountFlags;

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

#[test]
fn boxed_kernel_fs_forwards_every_trait_method() {
    // The `Box<dyn KernelFs>` forwarding impls must reach the inner driver:
    // each method returns the inner mock's distinct sentinel.
    let mut boxed: Box<dyn KernelFs> = Box::new(SentinelFs);

    // FilesystemRead
    let root = boxed.root();
    assert_eq!(root.raw(), 7);
    assert_eq!(boxed.lookup(root, b"x").expect("lookup forwards").raw(), 11);
    let mut buf = [0u8; 8];
    assert_eq!(
        boxed
            .read_at(NodeId::from_raw(1), 0, &mut buf)
            .expect("read forwards"),
        3
    );
    assert!(boxed
        .read_dir(NodeId::from_raw(1), 0, &mut buf)
        .expect("read_dir forwards")
        .is_none());

    // FilesystemWrite
    assert_eq!(
        boxed
            .create(NodeId::from_raw(1), b"f", NodeKind::RegularFile)
            .expect("create forwards")
            .raw(),
        13
    );
    assert_eq!(
        boxed
            .write_at(NodeId::from_raw(1), b"f", 0, b"data")
            .expect("write forwards"),
        5
    );
    boxed
        .truncate(NodeId::from_raw(1), b"f", 0)
        .expect("truncate forwards");
    boxed
        .remove(NodeId::from_raw(1), b"f")
        .expect("remove forwards");
    assert_eq!(
        boxed.rename(NodeId::from_raw(1), b"a", NodeId::from_raw(1), b"b"),
        Err(DriverError::Busy),
        "rename forwards to the inner driver"
    );
    boxed.flush().expect("flush forwards");

    // FilesystemSecurity
    boxed
        .security(NodeId::from_raw(1))
        .expect("security forwards");

    // FilesystemStats
    assert_eq!(
        boxed.stats().expect("stats forwards").total_blocks,
        17,
        "stats forwards to the inner driver"
    );
}
