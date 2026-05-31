//! Filesystem driver class (`drivers/filesystem/*`).
//!
//! A filesystem driver attaches a block-backed image to a mount point
//! and detaches it again. Path resolution, permission enforcement,
//! and the VFS itself live in `kernel/core` (Stage 5); this trait
//! exposes only the operations the host needs in order to wire a
//! driver into the mount table.
//!
//! The block-device side of the mount lives behind the
//! [`block::Block`](crate::driver::block::Block) trait; this module
//! does not duplicate it.

use super::DriverError;

/// Mount-flag bitmap, frozen at `abi-v1`.
///
/// Unknown bits must be zero; a non-zero unknown bit causes
/// [`DriverError::OutOfRange`] at mount time.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Default)]
pub struct MountFlags(u32);

impl MountFlags {
    /// Mount the filesystem read-only.
    pub const READ_ONLY: Self = Self(1 << 0);
    /// Reject `setuid` / `setgid` bits on this mount (`AGENTS.md` §5.3).
    pub const NOSUID: Self = Self(1 << 1);
    /// Reject device-special files on this mount.
    pub const NODEV: Self = Self(1 << 2);
    /// Reject executable mappings of files on this mount.
    pub const NOEXEC: Self = Self(1 << 3);

    /// Bitwise OR of every flag defined in `abi-v1`. Any bit outside
    /// this mask is reserved and rejected on the wire.
    pub const KNOWN_MASK: Self =
        Self(Self::READ_ONLY.0 | Self::NOSUID.0 | Self::NODEV.0 | Self::NOEXEC.0);

    /// Raw bitmap.
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Construct a [`MountFlags`] from a raw bitmap.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError::OutOfRange`] if `raw` has bits set
    /// outside [`Self::KNOWN_MASK`].
    ///
    /// # Capabilities
    ///
    /// None.
    pub const fn from_bits(raw: u32) -> Result<Self, DriverError> {
        if raw & !Self::KNOWN_MASK.0 != 0 {
            return Err(DriverError::OutOfRange);
        }
        Ok(Self(raw))
    }

    /// Returns `true` iff every bit in `other` is set in `self`.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// The set of flags present in either `self` or `other`.
    ///
    /// Both operands are already-validated [`MountFlags`], so the union
    /// stays within [`Self::KNOWN_MASK`] and needs no re-validation. Used
    /// to build composite mount policies such as the `nosuid,nodev,noexec`
    /// default for `/System/Logs` (`AGENTS.md` §16.2).
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// Trait every filesystem driver implements.
///
/// # Capabilities
///
/// The trait's `mount` / `unmount` methods are gated by
/// [`CapabilityId::FS_MOUNT`](crate::CapabilityId::FS_MOUNT) at the
/// dispatch site, on top of the load-time
/// [`CapabilityId::DRV_LOAD`](crate::CapabilityId::DRV_LOAD) check.
pub trait Filesystem {
    /// Attach a backing image and bring the filesystem online.
    ///
    /// `source_block_handle` is the kernel-issued
    /// [`DriverHandle`](crate::driver::DriverHandle) for the
    /// underlying [`block::Block`](crate::driver::block::Block) device,
    /// not a path — path resolution belongs to the VFS, not to this
    /// trait.
    ///
    /// # Errors
    ///
    /// * [`DriverError::PermissionDenied`] if the dispatcher has not
    ///   verified `CAP_FS_MOUNT`.
    /// * [`DriverError::BadMagic`] if the on-disk filesystem
    ///   superblock fails validation.
    /// * [`DriverError::DeviceFault`] if the block device reports an
    ///   unrecoverable read error.
    /// * [`DriverError::Busy`] if the filesystem is already mounted.
    ///
    /// # Capabilities
    ///
    /// Requires [`CapabilityId::FS_MOUNT`](crate::CapabilityId::FS_MOUNT).
    fn mount(
        &mut self,
        source_block_handle: crate::driver::DriverHandle,
        flags: MountFlags,
    ) -> Result<(), DriverError>;

    /// Detach the filesystem.
    ///
    /// Implementations must flush any in-flight write before
    /// returning success.
    ///
    /// # Errors
    ///
    /// * [`DriverError::PermissionDenied`] if the dispatcher has not
    ///   verified `CAP_FS_MOUNT`.
    /// * [`DriverError::Busy`] if the filesystem still has open
    ///   files.
    /// * [`DriverError::NotFound`] if the filesystem is not mounted.
    ///
    /// # Capabilities
    ///
    /// Requires [`CapabilityId::FS_MOUNT`](crate::CapabilityId::FS_MOUNT).
    fn unmount(&mut self) -> Result<(), DriverError>;
}

/// Opaque identifier for a node (file or directory) within a single
/// mounted filesystem.
///
/// A `NodeId` is minted by a [`FilesystemRead`] implementation and is
/// meaningful only to the implementation that issued it; the VFS treats
/// it as an opaque token. The all-zero value is reserved as
/// [`NodeId::NONE`] and is never returned for a live node.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct NodeId(u64);

impl NodeId {
    /// The reserved "no node" sentinel.
    pub const NONE: Self = Self(0);

    /// Wrap a driver-defined raw value as a [`NodeId`].
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// The driver-defined raw value behind this [`NodeId`].
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// The kind of a filesystem node.
///
/// `abi-v1` distinguishes only the two kinds the read surface needs;
/// special-file kinds are introduced by a later trait version rather
/// than by widening this enum (`AGENTS.md` §2.4 / §9).
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum NodeKind {
    /// A directory whose children are enumerable with
    /// [`FilesystemRead::read_dir`].
    Directory = 0,
    /// A regular file whose bytes are readable with
    /// [`FilesystemRead::read_at`].
    RegularFile = 1,
}

/// Structural metadata about a node, returned by
/// [`FilesystemRead::node_info`].
///
/// This is *structural* information only — its `size` and `kind` come
/// from the on-disk layout. Ownership, mode bits, ACLs, and the §5.3
/// capability gate live in the VFS metadata, not here; a read driver
/// never makes a permission decision (`AGENTS.md` §5.4 — the VFS is
/// the policy point, the driver is raw structural I/O).
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct NodeInfo {
    /// Whether the node is a directory or a regular file.
    pub kind: NodeKind,
    /// File length in bytes. Always `0` for a directory.
    pub size: u64,
}

/// A single entry yielded by [`FilesystemRead::read_dir`].
///
/// The entry's name is written into the caller-provided buffer; this
/// struct carries the entry's identity and the number of name bytes
/// written, keeping the read surface allocation-free.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DirEntry {
    /// The child node's identifier.
    pub node: NodeId,
    /// Whether the child is a directory or a regular file.
    pub kind: NodeKind,
    /// Number of name bytes written into the caller's buffer.
    pub name_len: usize,
}

/// Read-only structural access to a mounted filesystem.
///
/// This is the **versioned `abi-v1` extension** the VFS uses to
/// delegate path-resolution I/O to a block-backed
/// `drivers/filesystem/*` driver. It is deliberately a *separate*
/// trait from [`Filesystem`] (which remains mount/unmount only and
/// frozen): new behaviour ships as a new trait, never by widening a
/// shipped one (`AGENTS.md` §2.4 / §9). A future `FilesystemWrite`
/// trait will add the mutating surface.
///
/// Implementations expose raw structural access and make **no**
/// permission decisions: the VFS authorises every traversal against
/// the §5.3 model before calling here (`AGENTS.md` §5.4). Names are
/// raw on-disk bytes; case-folding and Unicode normalisation policy
/// belong to the VFS, not the driver.
///
/// # Capabilities
///
/// Calls are reached only through the kernel-issued
/// [`DriverHandle`](crate::driver::DriverHandle) the host minted at
/// load time ([`CapabilityId::DRV_LOAD`](crate::CapabilityId::DRV_LOAD)).
pub trait FilesystemRead {
    /// The identifier of the filesystem's root directory.
    fn root(&self) -> NodeId;

    /// Report the structural metadata of `node`.
    ///
    /// # Errors
    ///
    /// * [`DriverError::NotFound`] if `node` does not name a live node.
    /// * [`DriverError::DeviceFault`] on an unrecoverable block read.
    fn node_info(&mut self, node: NodeId) -> Result<NodeInfo, DriverError>;

    /// Resolve a single path component `name` within directory `dir`.
    ///
    /// `name` is a single component: it contains no path separator and
    /// is neither `.` nor `..` (the VFS resolves those itself, §16).
    ///
    /// # Errors
    ///
    /// * [`DriverError::Unsupported`] if `dir` is not a directory.
    /// * [`DriverError::NotFound`] if no child named `name` exists.
    /// * [`DriverError::DeviceFault`] on an unrecoverable block read.
    fn lookup(&mut self, dir: NodeId, name: &[u8]) -> Result<NodeId, DriverError>;

    /// Read up to `buf.len()` bytes from `file` starting at byte
    /// `offset`, returning the number of bytes read.
    ///
    /// A return value shorter than `buf.len()` indicates end-of-file;
    /// reading at or past the file's size returns `0`.
    ///
    /// # Errors
    ///
    /// * [`DriverError::Unsupported`] if `file` is a directory.
    /// * [`DriverError::NotFound`] if `file` does not name a live node.
    /// * [`DriverError::DeviceFault`] on an unrecoverable block read.
    fn read_at(&mut self, file: NodeId, offset: u64, buf: &mut [u8]) -> Result<usize, DriverError>;

    /// Yield the `index`-th child of directory `dir`, writing the
    /// child's name into `name_out`.
    ///
    /// Iteration order is the implementation's stable on-disk order.
    /// Returns `Ok(None)` once `index` is past the last child, which is
    /// how a caller detects the end of the directory.
    ///
    /// # Errors
    ///
    /// * [`DriverError::Unsupported`] if `dir` is not a directory.
    /// * [`DriverError::BufferTooSmall`] if the child's name does not
    ///   fit in `name_out`.
    /// * [`DriverError::DeviceFault`] on an unrecoverable block read.
    fn read_dir(
        &mut self,
        dir: NodeId,
        index: u64,
        name_out: &mut [u8],
    ) -> Result<Option<DirEntry>, DriverError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::DriverHandle;

    #[test]
    fn known_mask_covers_every_named_flag() {
        let all = MountFlags::READ_ONLY.bits()
            | MountFlags::NOSUID.bits()
            | MountFlags::NODEV.bits()
            | MountFlags::NOEXEC.bits();
        assert_eq!(MountFlags::KNOWN_MASK.bits(), all);
    }

    #[test]
    fn from_bits_rejects_unknown() {
        assert_eq!(MountFlags::from_bits(1 << 31), Err(DriverError::OutOfRange));
    }

    #[test]
    fn union_combines_known_flags() {
        let combined = MountFlags::NOSUID
            .union(MountFlags::NODEV)
            .union(MountFlags::NOEXEC);
        assert!(combined.contains(MountFlags::NOSUID));
        assert!(combined.contains(MountFlags::NODEV));
        assert!(combined.contains(MountFlags::NOEXEC));
        assert!(!combined.contains(MountFlags::READ_ONLY));
        // The union of known flags is itself within the known mask.
        assert_eq!(combined.bits() & !MountFlags::KNOWN_MASK.bits(), 0);
    }

    #[test]
    fn contains_is_bitwise_subset() {
        let Ok(f) = MountFlags::from_bits(MountFlags::NOSUID.bits() | MountFlags::NODEV.bits())
        else {
            unreachable!("known bits")
        };
        assert!(f.contains(MountFlags::NOSUID));
        assert!(!f.contains(MountFlags::READ_ONLY));
    }

    struct MockFs {
        mounted: bool,
    }

    impl Filesystem for MockFs {
        fn mount(&mut self, _src: DriverHandle, _flags: MountFlags) -> Result<(), DriverError> {
            if self.mounted {
                return Err(DriverError::Busy);
            }
            self.mounted = true;
            Ok(())
        }

        fn unmount(&mut self) -> Result<(), DriverError> {
            if !self.mounted {
                return Err(DriverError::NotFound);
            }
            self.mounted = false;
            Ok(())
        }
    }

    #[test]
    fn mock_round_trip() {
        let Ok(handle) = DriverHandle::from_raw(1) else {
            unreachable!("1 is non-zero")
        };
        let mut fs = MockFs { mounted: false };
        assert!(fs.mount(handle, MountFlags::READ_ONLY).is_ok());
        assert_eq!(
            fs.mount(handle, MountFlags::READ_ONLY),
            Err(DriverError::Busy)
        );
        assert!(fs.unmount().is_ok());
        assert_eq!(fs.unmount(), Err(DriverError::NotFound));
    }

    #[test]
    fn node_id_round_trips_raw_value() {
        let id = NodeId::from_raw(0xDEAD_BEEF);
        assert_eq!(id.raw(), 0xDEAD_BEEF);
        assert_eq!(NodeId::NONE.raw(), 0);
        assert_ne!(id, NodeId::NONE);
    }

    /// A fixed, allocation-free `FilesystemRead` over a root directory
    /// holding a single regular file `"readme"` with three bytes of
    /// content. Exercises the whole `abi-v1` read surface.
    struct MockReadFs;

    const ROOT: NodeId = NodeId::from_raw(1);
    const FILE: NodeId = NodeId::from_raw(2);
    const FILE_NAME: &[u8] = b"readme";
    const FILE_BODY: &[u8] = b"abc";

    impl FilesystemRead for MockReadFs {
        fn root(&self) -> NodeId {
            ROOT
        }

        fn node_info(&mut self, node: NodeId) -> Result<NodeInfo, DriverError> {
            if node == ROOT {
                Ok(NodeInfo {
                    kind: NodeKind::Directory,
                    size: 0,
                })
            } else if node == FILE {
                Ok(NodeInfo {
                    kind: NodeKind::RegularFile,
                    size: FILE_BODY.len() as u64,
                })
            } else {
                Err(DriverError::NotFound)
            }
        }

        fn lookup(&mut self, dir: NodeId, name: &[u8]) -> Result<NodeId, DriverError> {
            if dir != ROOT {
                return Err(DriverError::Unsupported);
            }
            if name == FILE_NAME {
                Ok(FILE)
            } else {
                Err(DriverError::NotFound)
            }
        }

        fn read_at(
            &mut self,
            file: NodeId,
            offset: u64,
            buf: &mut [u8],
        ) -> Result<usize, DriverError> {
            if file != FILE {
                return Err(DriverError::Unsupported);
            }
            let Ok(start) = usize::try_from(offset) else {
                return Ok(0);
            };
            if start >= FILE_BODY.len() {
                return Ok(0);
            }
            let n = core::cmp::min(buf.len(), FILE_BODY.len() - start);
            buf[..n].copy_from_slice(&FILE_BODY[start..start + n]);
            Ok(n)
        }

        fn read_dir(
            &mut self,
            dir: NodeId,
            index: u64,
            name_out: &mut [u8],
        ) -> Result<Option<DirEntry>, DriverError> {
            if dir != ROOT {
                return Err(DriverError::Unsupported);
            }
            if index != 0 {
                return Ok(None);
            }
            if name_out.len() < FILE_NAME.len() {
                return Err(DriverError::BufferTooSmall);
            }
            name_out[..FILE_NAME.len()].copy_from_slice(FILE_NAME);
            Ok(Some(DirEntry {
                node: FILE,
                kind: NodeKind::RegularFile,
                name_len: FILE_NAME.len(),
            }))
        }
    }

    #[test]
    fn mock_read_fs_lookup_and_read() {
        let mut fs = MockReadFs;
        assert_eq!(fs.root(), ROOT);
        let file = fs.lookup(fs.root(), FILE_NAME).expect("file present");
        assert_eq!(file, FILE);
        let info = fs.node_info(file).expect("info");
        assert_eq!(info.kind, NodeKind::RegularFile);
        assert_eq!(info.size, 3);

        let mut buf = [0u8; 8];
        let n = fs.read_at(file, 0, &mut buf).expect("read");
        assert_eq!(&buf[..n], FILE_BODY);
        // Reading at EOF yields zero bytes.
        assert_eq!(fs.read_at(file, 3, &mut buf), Ok(0));
    }

    #[test]
    fn mock_read_fs_dir_iteration_terminates() {
        let mut fs = MockReadFs;
        let mut name = [0u8; 16];
        let first = fs.read_dir(ROOT, 0, &mut name).expect("entry 0");
        let entry = first.expect("one entry");
        assert_eq!(entry.node, FILE);
        assert_eq!(&name[..entry.name_len], FILE_NAME);
        assert_eq!(fs.read_dir(ROOT, 1, &mut name), Ok(None));
    }

    #[test]
    fn mock_read_fs_rejects_small_dir_buffer() {
        let mut fs = MockReadFs;
        let mut tiny = [0u8; 2];
        assert_eq!(
            fs.read_dir(ROOT, 0, &mut tiny),
            Err(DriverError::BufferTooSmall)
        );
    }

    #[test]
    fn mock_read_fs_lookup_in_non_dir_is_unsupported() {
        let mut fs = MockReadFs;
        assert_eq!(fs.lookup(FILE, FILE_NAME), Err(DriverError::Unsupported));
    }
}
