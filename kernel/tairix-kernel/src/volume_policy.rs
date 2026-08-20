//! The removable-volume mount policy (`plans/DEVICES.md` D3d): the
//! storage-group identity map for filesystems that store no owner model.
//!
//! A foreign filesystem such as FAT32 stores no per-file owner, mode,
//! ACL, or capability gate, so its driver honestly reports one uniform,
//! restrictive record and delegates user access to mount policy. This
//! module is that policy's mechanism:
//!
//! * [`LateStorageGid`] / [`LATE_STORAGE_GID`] — the set-once cell the
//!   trusted root-unlock step publishes the well-known
//!   [`tairix_users::STORAGE_GROUP`] group's gid into, resolved **by
//!   name** from the loaded `/System/Security/Groups` registry. Until it
//!   is installed (or when the registry has no such group) the identity
//!   map is simply absent and a foreign volume stays system-owned — fail
//!   closed, never an invented gid.
//! * [`GroupMappedFs`] — the security-overriding adapter the runtime
//!   volume attach wraps an ownerless filesystem driver in: every node
//!   appears owned by the system user and the storage group, directories
//!   `rwxrwxr-x` and files `rw-rw-r--`, so any logged-in member of the
//!   group reads and writes the medium while non-members read only.
//!   `set_security` stays refused (the format cannot store a TAIRiX
//!   security record; a silently-lossy store is forbidden), and the
//!   structural read/write/stats surfaces pass through untouched.
//!
//! Volumes with a real owner model (`ARXFS`, ext4) are never wrapped:
//! their on-disk owners, modes, and ACLs keep governing access.

use tairix_abi::driver::filesystem::{
    DirEntry, FilesystemAttrs, FilesystemAttrsFs, FilesystemAttrsProvider, FilesystemRead,
    FilesystemSecurity, FilesystemStats, FilesystemWrite, NodeId, NodeInfo, NodeKind, NodeSecurity,
    VolumeStats,
};
use tairix_abi::DriverError;
use tairix_kernel_sec::GroupId;
use tairix_sync::OnceCell;

/// Mode the identity map reports for a directory: owner and group
/// read/write/search, others read/search.
const MAPPED_DIR_MODE: u32 = 0o775;

/// Mode the identity map reports for a regular file: owner and group
/// read/write, others read.
const MAPPED_FILE_MODE: u32 = 0o664;

/// The set-once storage-group gid cell.
///
/// Installed exactly once by the trusted unlock step that loads the group
/// registry; read by the runtime volume attach path on every
/// ownerless-filesystem mount. Before the install — or on a system whose
/// registry defines no storage group — [`get`](Self::get) is `None` and
/// the attach path applies no identity map.
pub struct LateStorageGid {
    cell: OnceCell<GroupId>,
}

impl LateStorageGid {
    /// An empty cell; [`get`](Self::get) is `None` until an install.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            cell: OnceCell::new(),
        }
    }

    /// Publish the resolved storage-group gid. First-wins and idempotent,
    /// like the other late-installed seams: the unlock runs once, so a
    /// second install is a logic error and is ignored rather than
    /// replacing live policy.
    pub fn install(&self, gid: GroupId) {
        let _ = self.cell.set(gid);
    }

    /// The installed gid, or `None` before the unlock publishes it (or
    /// when the registry defines no storage group).
    #[must_use]
    pub fn get(&self) -> Option<GroupId> {
        match self.cell.get() {
            Ok(Some(gid)) => Some(*gid),
            _ => None,
        }
    }
}

impl Default for LateStorageGid {
    fn default() -> Self {
        Self::new()
    }
}

/// The one production cell: the root unlock installs into it and the
/// runtime volume attach reads it.
pub static LATE_STORAGE_GID: LateStorageGid = LateStorageGid::new();

/// The storage-group identity map over an ownerless filesystem driver.
///
/// See the module docs. Structural I/O and space accounting pass through
/// to the wrapped driver; only the security surface is replaced.
pub struct GroupMappedFs<F> {
    inner: F,
    gid: GroupId,
}

impl<F> GroupMappedFs<F> {
    /// Wrap `inner` so every node reports system ownership under `gid`
    /// with group read/write.
    #[must_use]
    pub fn new(inner: F, gid: GroupId) -> Self {
        Self { inner, gid }
    }
}

impl<F: FilesystemRead> FilesystemRead for GroupMappedFs<F> {
    fn root(&self) -> NodeId {
        self.inner.root()
    }

    fn node_info(&mut self, node: NodeId) -> Result<NodeInfo, DriverError> {
        self.inner.node_info(node)
    }

    fn lookup(&mut self, dir: NodeId, name: &[u8]) -> Result<NodeId, DriverError> {
        self.inner.lookup(dir, name)
    }

    fn read_at(&mut self, file: NodeId, offset: u64, buf: &mut [u8]) -> Result<usize, DriverError> {
        self.inner.read_at(file, offset, buf)
    }

    fn read_dir(
        &mut self,
        dir: NodeId,
        index: u64,
        name_out: &mut [u8],
    ) -> Result<Option<DirEntry>, DriverError> {
        self.inner.read_dir(dir, index, name_out)
    }
}

impl<F: FilesystemRead + FilesystemWrite> FilesystemWrite for GroupMappedFs<F> {
    fn create(&mut self, dir: NodeId, name: &[u8], kind: NodeKind) -> Result<NodeId, DriverError> {
        self.inner.create(dir, name, kind)
    }

    fn write_at(
        &mut self,
        dir: NodeId,
        name: &[u8],
        offset: u64,
        data: &[u8],
    ) -> Result<usize, DriverError> {
        self.inner.write_at(dir, name, offset, data)
    }

    fn truncate(&mut self, dir: NodeId, name: &[u8], size: u64) -> Result<(), DriverError> {
        self.inner.truncate(dir, name, size)
    }

    fn remove(&mut self, dir: NodeId, name: &[u8]) -> Result<(), DriverError> {
        self.inner.remove(dir, name)
    }

    fn rename(
        &mut self,
        src_dir: NodeId,
        src_name: &[u8],
        dst_dir: NodeId,
        dst_name: &[u8],
    ) -> Result<(), DriverError> {
        self.inner.rename(src_dir, src_name, dst_dir, dst_name)
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        self.inner.flush()
    }
}

impl<F: FilesystemRead> FilesystemSecurity for GroupMappedFs<F> {
    fn security(&mut self, node: NodeId) -> Result<NodeSecurity, DriverError> {
        // The mapped record replaces the wrapped driver's uniform
        // restrictive default wholesale; only the node's structural kind
        // is consulted, so a missing node still refuses honestly.
        let info = self.inner.node_info(node)?;
        let mode = match info.kind {
            NodeKind::Directory => MAPPED_DIR_MODE,
            // A link gets the file mode, never a wider one. Traversal
            // authority is decided by the directories the resolution walks
            // and by the target it lands on, so a permissive mode on the
            // link itself would grant nothing useful and would be the only
            // fail-open value available here.
            NodeKind::RegularFile | NodeKind::Symlink => MAPPED_FILE_MODE,
        };
        Ok(NodeSecurity::new(mode, 0, self.gid.0))
    }

    fn set_security(&mut self, _node: NodeId, _security: NodeSecurity) -> Result<(), DriverError> {
        // The mapped ownership is mount policy, not stored state, and the
        // underlying format cannot hold a TAIRiX security record; storing
        // a silently-lossy one is forbidden, so the write is refused
        // whole (fail closed).
        Err(DriverError::Unsupported)
    }
}

impl<F: FilesystemStats> FilesystemStats for GroupMappedFs<F> {
    fn stats(&mut self) -> Result<VolumeStats, DriverError> {
        self.inner.stats()
    }
}

/// Attribute calls pass through to the wrapped driver's store, but the
/// facet hands out the *mapped* view (this wrapper), never the inner
/// driver: the secured VFS resolves and authorises attribute paths
/// against the mapped ownership, exactly as it does every other
/// delegated operation on the volume.
impl<F: FilesystemRead + FilesystemAttrsProvider> FilesystemAttrs for GroupMappedFs<F> {
    fn get_attr(
        &mut self,
        node: NodeId,
        key: &[u8],
        value_out: &mut [u8],
    ) -> Result<Option<usize>, DriverError> {
        // Reachable only through `attrs_fs`, which answers `None` when the
        // wrapped driver stores no attributes; the guard keeps a facet-
        // ignoring caller failing closed.
        let Some(inner) = self.inner.attrs_fs() else {
            return Err(DriverError::Unsupported);
        };
        inner.get_attr(node, key, value_out)
    }

    fn set_attr(&mut self, node: NodeId, key: &[u8], value: &[u8]) -> Result<(), DriverError> {
        let Some(inner) = self.inner.attrs_fs() else {
            return Err(DriverError::Unsupported);
        };
        inner.set_attr(node, key, value)
    }

    fn list_attr(
        &mut self,
        node: NodeId,
        index: u64,
        key_out: &mut [u8],
    ) -> Result<Option<usize>, DriverError> {
        let Some(inner) = self.inner.attrs_fs() else {
            return Err(DriverError::Unsupported);
        };
        inner.list_attr(node, index, key_out)
    }

    fn remove_attr(&mut self, node: NodeId, key: &[u8]) -> Result<(), DriverError> {
        let Some(inner) = self.inner.attrs_fs() else {
            return Err(DriverError::Unsupported);
        };
        inner.remove_attr(node, key)
    }
}

impl<F: FilesystemRead + FilesystemAttrsProvider> FilesystemAttrsProvider for GroupMappedFs<F> {
    fn attrs_fs(&mut self) -> Option<&mut dyn FilesystemAttrsFs> {
        // Support is the wrapped driver's fact; the returned view is the
        // mapped wrapper so authorisation sees the mapped ownership.
        if self.inner.attrs_fs().is_some() {
            Some(self)
        } else {
            None
        }
    }
}

#[cfg(test)]
#[path = "volume_policy_tests.rs"]
mod tests;
