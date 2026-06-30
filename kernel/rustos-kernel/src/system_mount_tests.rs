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
    DirEntry, FilesystemRead, FilesystemSecurity, FilesystemWrite, NodeId, NodeInfo, NodeKind,
    NodeSecurity,
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
}

#[test]
fn system_vfs_mounts_system_read_only_and_driver_backed() {
    // The production `/System` mount: a path under `/System` resolves to a
    // read-only, driver-backed mount, so the secured VFS delegates reads to
    // the live volume and refuses writes.
    let vfs = system_vfs().expect("the production /System VFS builds");
    let under_system = Path::parse("/System/Drivers/x").expect("valid path");
    let mount = vfs.mounts().resolve(&under_system);
    assert_eq!(mount.path(), &Path::parse("/System").expect("valid"));
    assert!(mount.is_read_only(), "/System is mounted read-only");
    assert!(
        mount.backing().is_some(),
        "/System is driver-backed so the VFS delegates to the live volume"
    );
}

#[test]
fn system_vfs_removes_the_backingless_logs_and_settings_submounts() {
    // The default layout's writable `/System/Logs` / `/System/Settings`
    // submounts carry no backing driver yet (P-B), so they are removed and a
    // path under them resolves to the one driver-backed `/System` mount
    // rather than a backing-less shadow that would refuse reads.
    let vfs = system_vfs().expect("the production /System VFS builds");
    let logs = Path::parse("/System/Logs/boot").expect("valid path");
    let mount = vfs.mounts().resolve(&logs);
    assert_eq!(
        mount.path(),
        &Path::parse("/System").expect("valid"),
        "no backing-less /System/Logs submount shadows the driver-backed /System"
    );
    assert!(mount.backing().is_some());
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
}
