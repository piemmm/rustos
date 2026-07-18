//! Unit tests for the storage-group identity map and its late gid cell.

use super::*;
use tairix_abi::driver::filesystem::{
    DirEntry, FilesystemRead, FilesystemSecurity, FilesystemStats, FilesystemWrite, NodeId,
    NodeInfo, NodeKind, NodeSecurity, NodeTimes, VolumeStats,
};
use tairix_abi::DriverError;
use tairix_kernel_sec::GroupId;

/// A two-node double: node 1 is the root directory, node 2 a regular
/// file. Every surface reports distinctive values so passthrough is
/// observable, and its own security record is deliberately restrictive
/// so a mapping bypass would be caught.
struct TwoNodeFs;

const ROOT: NodeId = NodeId::from_raw(1);
const FILE: NodeId = NodeId::from_raw(2);

impl FilesystemRead for TwoNodeFs {
    fn root(&self) -> NodeId {
        ROOT
    }

    fn node_info(&mut self, node: NodeId) -> Result<NodeInfo, DriverError> {
        match node {
            ROOT => Ok(NodeInfo {
                kind: NodeKind::Directory,
                size: 0,
                allocated: 0,
                times: NodeTimes::default(),
            }),
            FILE => Ok(NodeInfo {
                kind: NodeKind::RegularFile,
                size: 7,
                allocated: 512,
                times: NodeTimes::default(),
            }),
            _ => Err(DriverError::NotFound),
        }
    }

    fn lookup(&mut self, dir: NodeId, name: &[u8]) -> Result<NodeId, DriverError> {
        if dir == ROOT && name == b"file" {
            Ok(FILE)
        } else {
            Err(DriverError::NotFound)
        }
    }

    fn read_at(&mut self, file: NodeId, offset: u64, buf: &mut [u8]) -> Result<usize, DriverError> {
        if file != FILE || offset != 0 || buf.len() < 7 {
            return Err(DriverError::NotFound);
        }
        buf[..7].copy_from_slice(b"payload");
        Ok(7)
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

impl FilesystemWrite for TwoNodeFs {
    fn create(
        &mut self,
        _dir: NodeId,
        _name: &[u8],
        _kind: NodeKind,
    ) -> Result<NodeId, DriverError> {
        Err(DriverError::Unsupported)
    }

    fn write_at(
        &mut self,
        dir: NodeId,
        name: &[u8],
        _offset: u64,
        data: &[u8],
    ) -> Result<usize, DriverError> {
        if dir == ROOT && name == b"file" {
            Ok(data.len())
        } else {
            Err(DriverError::NotFound)
        }
    }

    fn truncate(&mut self, _dir: NodeId, _name: &[u8], _size: u64) -> Result<(), DriverError> {
        Err(DriverError::Unsupported)
    }

    fn remove(&mut self, _dir: NodeId, _name: &[u8]) -> Result<(), DriverError> {
        Err(DriverError::Unsupported)
    }

    fn rename(
        &mut self,
        _src_dir: NodeId,
        _src_name: &[u8],
        _dst_dir: NodeId,
        _dst_name: &[u8],
    ) -> Result<(), DriverError> {
        Err(DriverError::Unsupported)
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        Ok(())
    }
}

impl FilesystemSecurity for TwoNodeFs {
    fn security(&mut self, _node: NodeId) -> Result<NodeSecurity, DriverError> {
        // The restrictive record the wrapper must replace, never leak.
        Ok(NodeSecurity::new(0o600, 0, 0))
    }

    fn set_security(&mut self, _node: NodeId, _security: NodeSecurity) -> Result<(), DriverError> {
        Err(DriverError::Unsupported)
    }
}

impl FilesystemStats for TwoNodeFs {
    fn stats(&mut self) -> Result<VolumeStats, DriverError> {
        Ok(VolumeStats {
            block_size: 512,
            total_blocks: 100,
            free_blocks: 50,
            avail_blocks: 50,
            files: 0,
            files_free: 0,
        })
    }
}

const STORAGE: GroupId = GroupId(100);

#[test]
fn directories_and_files_map_to_the_storage_group() {
    let mut fs = GroupMappedFs::new(TwoNodeFs, STORAGE);
    let dir = fs.security(ROOT).expect("root maps");
    assert_eq!((dir.mode, dir.uid, dir.gid), (0o775, 0, STORAGE.0));
    assert_eq!(dir.required_cap, None);
    assert!(dir.acl().is_empty());

    let file = fs.security(FILE).expect("file maps");
    assert_eq!((file.mode, file.uid, file.gid), (0o664, 0, STORAGE.0));
}

#[test]
fn a_missing_node_still_refuses() {
    let mut fs = GroupMappedFs::new(TwoNodeFs, STORAGE);
    assert_eq!(fs.security(NodeId::from_raw(9)), Err(DriverError::NotFound));
}

#[test]
fn security_stores_stay_refused() {
    let mut fs = GroupMappedFs::new(TwoNodeFs, STORAGE);
    assert_eq!(
        fs.set_security(FILE, NodeSecurity::new(0o777, 5, 5)),
        Err(DriverError::Unsupported)
    );
}

#[test]
fn structural_surfaces_pass_through() {
    let mut fs = GroupMappedFs::new(TwoNodeFs, STORAGE);
    assert_eq!(fs.root(), ROOT);
    assert_eq!(fs.lookup(ROOT, b"file"), Ok(FILE));
    let mut buf = [0u8; 16];
    assert_eq!(fs.read_at(FILE, 0, &mut buf), Ok(7));
    assert_eq!(&buf[..7], b"payload");
    assert_eq!(fs.write_at(ROOT, b"file", 0, b"xy"), Ok(2));
    assert_eq!(fs.stats().expect("stats").total_blocks, 100);
}

#[test]
fn the_late_cell_is_set_once_and_fail_closed_before_install() {
    let cell = LateStorageGid::new();
    assert_eq!(cell.get(), None);
    cell.install(GroupId(100));
    assert_eq!(cell.get(), Some(GroupId(100)));
    // A second install is ignored, never a replacement of live policy.
    cell.install(GroupId(200));
    assert_eq!(cell.get(), Some(GroupId(100)));
}
