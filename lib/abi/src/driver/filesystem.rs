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

use super::{DriverError, DriverHandle};
use crate::time::Time64;
use crate::CapabilityId;

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
    /// Reject `setuid` / `setgid` bits on this mount.
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
    /// default for `/System/Logs`.
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
    /// [`DriverHandle`] for the
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
/// A closed set: a format that stores a kind this does not name reports
/// [`DriverError::Unsupported`] rather than mapping it onto the nearest
/// match, so an unrecognised on-disk kind can never be mistaken for a
/// directory the VFS would descend or a file it would read.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum NodeKind {
    /// A directory whose children are enumerable with
    /// [`FilesystemRead::read_dir`].
    Directory = 0,
    /// A regular file whose bytes are readable with
    /// [`FilesystemRead::read_at`].
    RegularFile = 1,
    /// A symbolic link whose target path is read with
    /// [`FilesystemRead::read_link`] and which is created only by
    /// [`FilesystemWrite::create_link`] — never by
    /// [`FilesystemWrite::create`], which carries no target to store.
    Symlink = 2,
}

/// Structural metadata about a node, returned by
/// [`FilesystemRead::node_info`].
///
/// This is *structural* information only — its `size` and `kind` come
/// from the on-disk layout. Ownership, mode bits, ACLs, and the
/// capability gate live in the VFS metadata, not here; a read driver
/// never makes a permission decision (the VFS is
/// the policy point, the driver is raw structural I/O).
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct NodeInfo {
    /// Whether the node is a directory or a regular file.
    pub kind: NodeKind,
    /// How many directory entries name this node — POSIX `st_nlink`, read
    /// from the format and never derived.
    ///
    /// A format that records no count reports `1`: a node reached through a
    /// directory entry has at least the one name the caller just walked, and
    /// that is a fact rather than a guess. A directory's count includes its
    /// own `.` and each child's `..`, so an empty directory is `2`.
    ///
    /// This is the driver's answer because only the format knows it. The VFS
    /// carries it up to [`FileStat`](crate::fs::FileStat) unchanged rather
    /// than counting names itself, which it could not do without walking
    /// every directory on the volume.
    pub nlink: u32,
    /// File length in bytes. Always `0` for a directory.
    pub size: u64,
    /// Bytes of on-disk storage the node's data occupies — the real
    /// allocation the format tracks (ext4 `i_blocks`, a FAT cluster
    /// chain, `ARXFS` mapped extents), never a value derived from `size`
    /// when the format knows better. `0` for a node whose data occupies
    /// no dedicated blocks (an empty file, or a directory whose entries
    /// live in shared metadata structures).
    pub allocated: u64,
    /// The node's four timestamps, read from the format in the *same*
    /// structural read as `kind`/`size` — so a caller never pays a second
    /// inode read (nor a second on-disk walk) to learn a node's times. A
    /// stamp the format does not keep is [`Time64::UNIX_EPOCH`] (ARXFS, for
    /// instance, tracks no access time), never a fabricated wall time.
    pub times: NodeTimes,
}

impl NodeInfo {
    /// The [`nlink`](Self::nlink) a format that records no per-node name
    /// count reports.
    ///
    /// Such a format also has no second-name object, so one name is not a
    /// floor it might exceed — it is the whole truth: the caller reached the
    /// node through exactly one directory entry and no other can exist. The
    /// one definition every such driver reads, so two of them cannot drift
    /// into disagreeing about what "no count" means.
    pub const SINGLE_NAME: u32 = 1;
}

/// A single entry yielded by [`FilesystemRead::read_dir`].
///
/// The entry's name is written into the caller-provided buffer; this
/// struct carries the entry's identity, its structural metadata, the
/// number of name bytes written, and the cursor that resumes iteration
/// after it, keeping the read surface allocation-free.
///
/// The entry carries the child's full [`NodeInfo`] because the driver has
/// the child's metadata in hand while producing the entry: a listing
/// consumer (`du`, `ls`) would otherwise re-resolve every child by path —
/// a fresh full walk per entry on an uncached, authenticated volume.
///
/// The same reasoning carries the child's timestamps ([`NodeInfo::times`]):
/// the listing driver has already read the child's inode or directory
/// record, and a format such as FAT stores the stamps *only* in the
/// parent's directory record, so the listing is the one place every driver
/// can report them without a second walk.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DirEntry {
    /// The child node's identifier.
    pub node: NodeId,
    /// The child's structural metadata (kind, size, allocated bytes, and
    /// its four timestamps). The listing driver has already read the
    /// child's inode or directory record, so the stamps come free with the
    /// entry — a listing consumer (`ls -lt`, a file manager) reads them
    /// from [`NodeInfo::times`] rather than re-`stat`ing each child.
    pub info: NodeInfo,
    /// Number of name bytes written into the caller's buffer.
    pub name_len: usize,
    /// Opaque cursor resuming iteration at the entry *after* this one:
    /// pass it back to [`FilesystemRead::read_dir`] to continue the
    /// listing in O(1), never by rescanning from the start.
    pub next_cursor: u64,
}

/// Read-only structural access to a mounted filesystem.
///
/// This is the **versioned `abi-v1` extension** the VFS uses to
/// delegate path-resolution I/O to a block-backed
/// `drivers/filesystem/*` driver. It is deliberately a *separate*
/// trait from [`Filesystem`] (which remains mount/unmount only and
/// frozen): new behaviour ships as a new trait, never by widening a
/// shipped one. A future `FilesystemWrite`
/// trait will add the mutating surface.
///
/// Implementations expose raw structural access and make **no**
/// permission decisions: the VFS authorises every traversal against
/// the model before calling here. Names are
/// raw on-disk bytes; case-folding and Unicode normalisation policy
/// belong to the VFS, not the driver.
///
/// # Capabilities
///
/// Calls are reached only through the kernel-issued
/// [`DriverHandle`] the host minted at
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
    /// is neither `.` nor `..` (the VFS resolves those itself).
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

    /// Read the target path of the symbolic link `link` into `out`,
    /// returning its length in bytes.
    ///
    /// The target is returned exactly as stored — not resolved, not
    /// normalised, and with no terminator — because resolution is the VFS's
    /// policy decision, made component by component under the caller's
    /// attested identity, never the driver's.
    ///
    /// A format that has no symbolic links leaves this defaulted, so it
    /// refuses rather than inventing a target.
    ///
    /// # Errors
    ///
    /// * [`DriverError::Unsupported`] if the format stores no links, or if
    ///   `link` is not a [`NodeKind::Symlink`].
    /// * [`DriverError::NotFound`] if `link` does not name a live node.
    /// * [`DriverError::BufferTooSmall`] if the target does not fit `out`.
    /// * [`DriverError::DeviceFault`] on an unrecoverable block read.
    fn read_link(&mut self, link: NodeId, out: &mut [u8]) -> Result<usize, DriverError> {
        let _ = (link, out);
        Err(DriverError::Unsupported)
    }

    /// Yield the next child of directory `dir` at or after `cursor`,
    /// writing the child's name into `name_out`.
    ///
    /// `cursor` is an **opaque resume token** (the `getdents` `d_off`
    /// model): `0` starts the listing, and each returned entry carries
    /// the [`DirEntry::next_cursor`] that continues it in O(1) — a full
    /// listing therefore costs one bounded scan of the directory, never a
    /// quadratic rescan from the start per entry. Tokens are meaningful
    /// only for the directory that produced them, while it is unmodified;
    /// after a mutation a retained token remains *safe* (bounded,
    /// fail-closed, no panic) but the remainder of that listing is
    /// unspecified — the caller restarts from `0` for a coherent view.
    /// An arbitrary value that was never returned is handled the same
    /// way: bounds-checked, yielding `Ok(None)`, a valid tail, or
    /// [`DriverError::DeviceFault`], never undefined behaviour.
    ///
    /// Iteration order is the implementation's stable on-disk order.
    /// Returns `Ok(None)` once the listing is exhausted, which is how a
    /// caller detects the end of the directory.
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
        cursor: u64,
        name_out: &mut [u8],
    ) -> Result<Option<DirEntry>, DriverError>;
}

/// The host's write-back timer, as a driver that defers durability sees it.
///
/// A filesystem may keep one transaction open across several operations, so
/// that the commit — its barrier, its root, and the metadata blocks every
/// operation in it rewrote — costs once per burst rather than once per
/// operation. That trades recency, which is only bounded if something
/// publishes the transaction when the volume falls quiet. The driver cannot:
/// it owns no thread and runs only inside a caller's operation. So the host
/// runs the timer and the driver names the instant — the only direction that
/// cannot lose a transaction, since the driver alone knows one is open.
pub trait WritebackHost: Send + Sync {
    /// Monotonic nanoseconds, or [`None`] where the host has no monotonic
    /// clock. A driver that cannot measure elapsed time publishes every
    /// operation instead of deferring against a clock it does not have.
    fn now_ns(&self) -> Option<u64>;

    /// The volume registered as `volume` holds an open transaction due at
    /// absolute monotonic `deadline_ns`, or holds none (`None`).
    ///
    /// Called from inside the driver, under whatever lock serialises it, so
    /// an implementation records the deadline and returns: it must not park,
    /// perform I/O, or take a lock its caller may already hold.
    fn writeback_due(&self, volume: DriverHandle, deadline_ns: Option<u64>);
}

/// Mutating structural access to a mounted filesystem.
///
/// This is the **versioned `abi-v1` extension** the VFS uses to delegate
/// write-path I/O to a block-backed `drivers/filesystem/*` driver, the
/// symmetric counterpart to [`FilesystemRead`]. Like that trait it is a
/// *separate* trait, never a widening of the frozen [`Filesystem`]
/// mount/unmount surface or of [`FilesystemRead`]: new behaviour ships as
/// a new trait.
///
/// # The `(dir, name)` model
///
/// Every mutating method names its target as a (`dir`, `name`) pair rather
/// than by an opaque [`NodeId`]. A [`NodeId`] minted by [`FilesystemRead`]
/// is self-describing but carries no back-pointer to the directory entry
/// that stores a file's length and starting location; filesystems such as
/// FAT keep that metadata *in the parent directory*, so a mutation that
/// grows, shrinks, or unlinks a node must address it through its parent.
/// `name` is a single path component containing no separator and is
/// neither `.` nor `..` (the VFS resolves those itself).
///
/// # No permission decisions
///
/// As with [`FilesystemRead`], implementations expose raw structural
/// mutation and make **no** permission decision: the VFS authorises every
/// write against the model before calling here.
///
/// # Capabilities
///
/// Calls are reached only through the kernel-issued
/// [`DriverHandle`] the host minted at load
/// time, and the VFS additionally requires the mount to be writable (a
/// mount carrying [`MountFlags::READ_ONLY`] is never delegated a write).
pub trait FilesystemWrite {
    /// Create an empty child `name` of kind `kind` in directory `dir`,
    /// returning the new node's [`NodeId`].
    ///
    /// The child is created with zero length and (for a directory) with
    /// its `.`/`..` links in place. Implementations need not allocate
    /// on-disk data for a zero-length regular file.
    ///
    /// # Errors
    ///
    /// * [`DriverError::Unsupported`] if `dir` is not a directory, or if
    ///   `kind` is [`NodeKind::Symlink`] — a link carries a target this
    ///   call has nowhere to put, so it is created only by
    ///   [`create_link`](Self::create_link) and never as a side effect of
    ///   an empty-child create.
    /// * [`DriverError::AlreadyExists`] if a child named `name` already
    ///   exists.
    /// * [`DriverError::LengthOutOfRange`] if `name` is empty or longer
    ///   than the filesystem's maximum component length.
    /// * [`DriverError::NoSpace`] if the volume cannot allocate the inode
    ///   (or a directory's initial data block) because it is full.
    /// * [`DriverError::DeviceFault`] if a block write fails.
    fn create(&mut self, dir: NodeId, name: &[u8], kind: NodeKind) -> Result<NodeId, DriverError>;

    /// Create a symbolic link named `name` in directory `dir` whose stored
    /// target is `target`, returning the new node's [`NodeId`].
    ///
    /// `target` is stored verbatim; the driver neither resolves nor
    /// validates it as a path, so a link may legitimately dangle. The VFS
    /// bounds its length before the call.
    ///
    /// A format that has no symbolic links leaves this defaulted, so it
    /// refuses creation rather than substituting a copy or an empty file.
    ///
    /// # Errors
    ///
    /// * [`DriverError::Unsupported`] if the format stores no links, or if
    ///   `dir` is not a directory.
    /// * [`DriverError::AlreadyExists`] if a child named `name` already
    ///   exists.
    /// * [`DriverError::LengthOutOfRange`] if `name` is empty or longer than
    ///   the filesystem's maximum component length, or if `target` is empty
    ///   or longer than the format can store.
    /// * [`DriverError::NoSpace`] if the volume cannot allocate the node.
    /// * [`DriverError::DeviceFault`] if a block write fails.
    fn create_link(
        &mut self,
        dir: NodeId,
        name: &[u8],
        target: &[u8],
    ) -> Result<NodeId, DriverError> {
        let _ = (dir, name, target);
        Err(DriverError::Unsupported)
    }

    /// Add `name` in directory `dir` as a second directory entry for the
    /// existing node `node` — a hard link — raising the node's
    /// [`NodeInfo::nlink`] count by one.
    ///
    /// The node gains a name, not a copy: both entries reach one inode, so a
    /// write through either is visible through the other, and the node's
    /// storage survives until the *last* name is unlinked. A driver that
    /// implements this must therefore make [`remove`](Self::remove) decrement
    /// the count and free only at zero — implementing one without the other
    /// leaks storage or frees data another name still reaches.
    ///
    /// `dir` and `node` are on the same mounted volume: a directory entry
    /// cannot address an inode in another backing, and the VFS refuses a
    /// cross-volume pair before delegating. The VFS likewise refuses a
    /// directory before the call; a driver still checks, because it owns the
    /// invariant that its own tree stays a tree.
    ///
    /// A format with no second-name concept leaves this defaulted and
    /// refuses, rather than substituting a copy that would silently diverge
    /// from the original on the next write.
    ///
    /// # Errors
    ///
    /// * [`DriverError::Unsupported`] if the format stores only one name per
    ///   node, if `dir` is not a directory, or if `node` is a directory.
    /// * [`DriverError::AlreadyExists`] if a child named `name` already
    ///   exists.
    /// * [`DriverError::NotFound`] if `node` does not name a live node.
    /// * [`DriverError::LengthOutOfRange`] if `name` is empty or longer than
    ///   the filesystem's maximum component length.
    /// * [`DriverError::TooManyLinks`] if the node already carries as many
    ///   names as the format can record — a fixed on-disk bound, so the
    ///   create fails closed rather than wrapping the count.
    /// * [`DriverError::NoSpace`] if the volume cannot grow the directory.
    /// * [`DriverError::DeviceFault`] if a block write fails.
    fn link(&mut self, dir: NodeId, name: &[u8], node: NodeId) -> Result<(), DriverError> {
        let _ = (dir, name, node);
        Err(DriverError::Unsupported)
    }

    /// Write `data` into the regular file `name` in directory `dir`
    /// starting at byte `offset`, returning the number of bytes written.
    ///
    /// Writing past the current end of the file extends it, allocating
    /// backing storage as needed and updating the recorded length. A
    /// write whose `offset` is beyond the current length zero-fills the
    /// gap.
    ///
    /// **The count may be short of `data`.** An implementation is free to
    /// store fewer bytes and report how many, exactly as `write(2)` is: a
    /// driver that bounds the memory one call may pin uses a short count as
    /// its back-pressure, so a caller that needs every byte on the volume
    /// loops — or calls [`write_all`](Self::write_all), which loops once for
    /// everybody.
    ///
    /// # Errors
    ///
    /// * [`DriverError::Unsupported`] if `name` resolves to a directory.
    /// * [`DriverError::NotFound`] if no child named `name` exists.
    /// * [`DriverError::NoSpace`] if the volume cannot allocate a data
    ///   block to back the write because it is full.
    /// * [`DriverError::DeviceFault`] if a block write fails.
    fn write_at(
        &mut self,
        dir: NodeId,
        name: &[u8],
        offset: u64,
        data: &[u8],
    ) -> Result<usize, DriverError>;

    /// Write the whole of `data` into the regular file `name` in directory
    /// `dir` starting at byte `offset`, resuming across short writes.
    ///
    /// [`write_at`](Self::write_at) may legitimately store less than it was
    /// handed, so every caller that needs the whole value on the volume — a
    /// settings document, a key file, a planted image payload, an account
    /// database — has to loop. Looping here rather than at each of those
    /// call sites is what keeps them from disagreeing about it, and what
    /// stops a caller mistaking back-pressure for a failure.
    ///
    /// # Errors
    ///
    /// Whatever [`write_at`](Self::write_at) reports, plus
    /// [`DriverError::NoSpace`] if a call stores nothing while bytes remain:
    /// an implementation that makes no progress is refusing the write, and
    /// retrying it forever is not an answer.
    fn write_all(
        &mut self,
        dir: NodeId,
        name: &[u8],
        offset: u64,
        data: &[u8],
    ) -> Result<(), DriverError> {
        let mut done = 0usize;
        while done < data.len() {
            let at = offset
                .checked_add(done as u64)
                .ok_or(DriverError::OutOfRange)?;
            let written = self.write_at(dir, name, at, &data[done..])?;
            if written == 0 {
                return Err(DriverError::NoSpace);
            }
            done = done.checked_add(written).ok_or(DriverError::OutOfRange)?;
            if done > data.len() {
                // A driver claiming more than it was handed has lost track of
                // the write; fail closed rather than trust the count.
                return Err(DriverError::DeviceFault);
            }
        }
        Ok(())
    }

    /// Set the length of the regular file `name` in directory `dir` to
    /// `size`, freeing or zero-extending its backing storage as needed.
    ///
    /// # Errors
    ///
    /// * [`DriverError::Unsupported`] if `name` resolves to a directory.
    /// * [`DriverError::NotFound`] if no child named `name` exists.
    /// * [`DriverError::NoSpace`] if zero-extending the file cannot
    ///   allocate a data block because the volume is full.
    /// * [`DriverError::DeviceFault`] if a block write fails.
    fn truncate(&mut self, dir: NodeId, name: &[u8], size: u64) -> Result<(), DriverError>;

    /// Unlink the child `name` from directory `dir`, freeing its backing
    /// storage.
    ///
    /// # Errors
    ///
    /// * [`DriverError::Unsupported`] if `dir` is not a directory.
    /// * [`DriverError::NotFound`] if no child named `name` exists.
    /// * [`DriverError::DirectoryNotEmpty`] if `name` is a non-empty
    ///   directory.
    /// * [`DriverError::DeviceFault`] on an unrecoverable block write.
    fn remove(&mut self, dir: NodeId, name: &[u8]) -> Result<(), DriverError>;

    /// Move the child `src_name` of directory `src_dir` so that it becomes
    /// the child `dst_name` of directory `dst_dir`, preserving the moved
    /// node's identity, contents, and metadata.
    ///
    /// `src_name` and `dst_name` are each a single path component
    /// containing no separator and neither `.` nor `..` (the VFS resolves
    /// those itself). `src_dir` and `dst_dir` may be the same directory (a
    /// pure within-directory rename) or different directories on the same
    /// mounted volume.
    ///
    /// # Replacing an existing destination
    ///
    /// If `dst_name` already names an entry in `dst_dir` it is atomically
    /// replaced, subject to kind compatibility: a regular file may replace
    /// a regular file, and a directory may replace an **empty** directory.
    /// The replaced node's backing storage is freed. Replacing a directory
    /// with a non-directory, or a non-directory with a directory, is
    /// refused.
    ///
    /// A rename whose source and destination name the *same* entry
    /// (`src_dir == dst_dir` and `src_name == dst_name`) succeeds and
    /// changes nothing.
    ///
    /// When a directory is moved to a different parent, its `..` link is
    /// repointed at `dst_dir` and both parents' link counts are adjusted.
    /// Moving a directory into itself or into its own subtree is refused
    /// (it would detach the cycle from the tree).
    ///
    /// # Errors
    ///
    /// * [`DriverError::Unsupported`] if `src_dir` or `dst_dir` is not a
    ///   directory, or a kind-incompatible replacement is attempted
    ///   (a file over a directory, or a directory over a file).
    /// * [`DriverError::NotFound`] if `src_name` does not exist in
    ///   `src_dir`.
    /// * [`DriverError::DirectoryNotEmpty`] if `dst_name` is a non-empty
    ///   directory.
    /// * [`DriverError::DirectoryCycle`] if the move would place a directory
    ///   inside its own subtree.
    /// * [`DriverError::LengthOutOfRange`] if `dst_name` is empty or longer
    ///   than the filesystem's maximum component length.
    /// * [`DriverError::NoSpace`] if `dst_dir` cannot grow to hold the new
    ///   entry because the volume is full.
    /// * [`DriverError::DeviceFault`] on an unrecoverable block write.
    fn rename(
        &mut self,
        src_dir: NodeId,
        src_name: &[u8],
        dst_dir: NodeId,
        dst_name: &[u8],
    ) -> Result<(), DriverError>;

    /// Make every write the filesystem has accepted durable on stable
    /// media, returning only once the backing device confirms it.
    ///
    /// This is the durability contract behind the `fs_sync` syscall. A
    /// driver must (1) push any metadata or data it still buffers to the
    /// block device, and (2) force the device's own volatile write cache
    /// to the medium by calling the backing device's
    /// [`Block::flush`](super::block::Block::flush). Writing "through" to
    /// the device synchronously is **not** sufficient: a completed block
    /// write only means the device *accepted* the bytes, which may still
    /// sit in its volatile cache until a flush commits them. A driver that
    /// returns `Ok(())` without forcing the device cache reports
    /// durability it has not delivered.
    ///
    /// A driver with no backing device (an in-RAM filesystem) or a
    /// read-only mount that never wrote has nothing to commit and returns
    /// `Ok(())` truthfully.
    ///
    /// # Errors
    ///
    /// * [`DriverError::DeviceFault`] if the device cannot confirm the
    ///   data is on stable media; the caller fails the sync closed rather
    ///   than reporting durability it cannot vouch for.
    fn flush(&mut self) -> Result<(), DriverError>;

    /// Install the host's write-back timer for this mount, registered under
    /// `volume`.
    ///
    /// Called once, by the host, as the mount is registered — so a driver
    /// that defers durability can only ever do so with a timer that will
    /// publish it. A driver that publishes at every operation has no
    /// deadline to report and leaves this defaulted; one that *does* defer
    /// overrides it and, from then on, reports each transaction's deadline
    /// through [`WritebackHost::writeback_due`] as the transaction opens and
    /// reports `None` as it closes.
    ///
    /// A driver never given a host must publish eagerly rather than hold a
    /// transaction nothing will close.
    fn set_writeback_host(&mut self, volume: DriverHandle, host: &'static dyn WritebackHost) {
        let _ = (volume, host);
    }
}

/// Maximum number of inline ACL entries a [`NodeSecurity`] record carries.
///
/// Eight inline entries keep the record fixed-size and allocation-free,
/// matching the per-inode inline-ACL budget a `drivers/filesystem/*`
/// driver stores for the model.
pub const MAX_ACL_ENTRIES: usize = 8;

/// The principal a [`SecurityAcl`] entry grants rights to.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SecuritySubject {
    /// The user with this uid.
    User(u32),
    /// The group with this gid (matched against a caller's primary and
    /// supplementary groups by the VFS).
    Group(u32),
}

/// One inline access-control-list entry of a node's security record.
///
/// `perms` is a POSIX-style `rwx` triad in its low three bits (`0b100`
/// read, `0b010` write, `0b001` execute/search) **granted** to `subject`.
/// The surface is grant-only — the POSIX ACL model — so a driver never
/// surfaces an explicit deny; the VFS composes these grants with the mode
/// bits when it applies the model.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SecurityAcl {
    /// The user or group the entry grants rights to.
    pub subject: SecuritySubject,
    /// The `rwx` permission bits granted to `subject`.
    pub perms: u8,
}

/// The complete security record a filesystem driver stores for one
/// node, surfaced to the VFS through [`FilesystemSecurity::security`].
///
/// This is an in-process policy record the VFS consumes, not a serialized
/// wire type: each `drivers/filesystem/*` driver owns its own on-disk
/// encoding and translates to and from this shape. The driver stores the
/// record but makes **no** permission decision from it (the VFS is the policy point).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct NodeSecurity {
    /// POSIX mode bits (type bits are not stored here; see [`NodeKind`]).
    pub mode: u32,
    /// Owning user id.
    pub uid: u32,
    /// Owning group id.
    pub gid: u32,
    /// An optional capability the caller must hold to access the node at
    /// all, on top of the mode/ACL checks (`None` = no capability gate).
    pub required_cap: Option<CapabilityId>,
    acl: [SecurityAcl; MAX_ACL_ENTRIES],
    acl_len: usize,
}

impl NodeSecurity {
    /// A record owned by `(uid, gid)` with mode `mode`, no ACL entries,
    /// and no capability gate.
    #[must_use]
    pub const fn new(mode: u32, uid: u32, gid: u32) -> Self {
        Self {
            mode,
            uid,
            gid,
            required_cap: None,
            acl: [SecurityAcl {
                subject: SecuritySubject::User(0),
                perms: 0,
            }; MAX_ACL_ENTRIES],
            acl_len: 0,
        }
    }

    /// The node's ACL entries.
    #[must_use]
    pub fn acl(&self) -> &[SecurityAcl] {
        &self.acl[..self.acl_len]
    }

    /// Append an ACL entry.
    ///
    /// # Errors
    ///
    /// [`DriverError::LengthOutOfRange`] if the record already holds
    /// [`MAX_ACL_ENTRIES`] entries.
    pub fn push_acl(&mut self, entry: SecurityAcl) -> Result<(), DriverError> {
        if self.acl_len >= MAX_ACL_ENTRIES {
            return Err(DriverError::LengthOutOfRange);
        }
        self.acl[self.acl_len] = entry;
        self.acl_len += 1;
        Ok(())
    }
}

/// Per-node security access to a mounted filesystem.
///
/// This is a **versioned `abi-v1` extension** — a *separate* trait from
/// [`FilesystemRead`] / [`FilesystemWrite`], never a widening of either
/// nor of the frozen [`Filesystem`]; new behaviour ships as a new trait. A driver that stores full POSIX metadata per
/// inode — owner, mode, ACL, and an optional capability gate —
/// implements it so the VFS can use that **stored** record as the policy
/// input instead of a uniform mount-point template. A driver such as FAT
/// that keeps no per-file owner does not implement it, and the VFS keeps
/// applying the mount-point template.
///
/// The driver only *stores and reports* the record; it makes no
/// permission decision (the VFS is the policy point, and every caller of
/// [`set_security`](Self::set_security) — today the kernel's
/// `CAP_USER_ADMIN` account-administration engine — authorises the write
/// before delegating here).
///
/// # Capabilities
///
/// Calls are reached only through the kernel-issued
/// [`DriverHandle`] the host minted at load
/// time ([`CapabilityId::DRV_LOAD`](crate::CapabilityId::DRV_LOAD)).
pub trait FilesystemSecurity {
    /// Report the security record stored for `node`.
    ///
    /// # Errors
    ///
    /// * [`DriverError::NotFound`] if `node` does not name a live node.
    /// * [`DriverError::DeviceFault`] on an unrecoverable block read.
    fn security(&mut self, node: NodeId) -> Result<NodeSecurity, DriverError>;

    /// Replace the security record stored for `node`.
    ///
    /// # Errors
    ///
    /// * [`DriverError::PermissionDenied`] if the volume is mounted
    ///   read-only — refused before any state is touched.
    /// * [`DriverError::NotFound`] if `node` does not name a live node.
    /// * [`DriverError::DeviceFault`] on an unrecoverable block write.
    fn set_security(&mut self, node: NodeId, security: NodeSecurity) -> Result<(), DriverError>;
}

/// The four timestamps stored for a filesystem node.
///
/// Every field is a 64-bit-native [`Time64`]: absolute
/// time is never a seconds-only scalar, so the full pre-1970 and
/// post-2038 range round-trips without truncation. The four instants
/// follow the POSIX model:
///
/// * `created` — set once when the node is created and never changed.
/// * `modified` — last change to the node's *contents* (mtime).
/// * `accessed` — last access to the node's contents (atime).
/// * `changed` — last change to the node's *metadata* (ctime).
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct NodeTimes {
    /// Creation instant; set once and never changed.
    pub created: Time64,
    /// Last contents-modification instant (mtime).
    pub modified: Time64,
    /// Last access instant (atime).
    pub accessed: Time64,
    /// Last metadata-change instant (ctime).
    pub changed: Time64,
}

/// Per-node extended-attribute access to a mounted filesystem.
///
/// This is a **versioned `abi-v1` extension** — a *separate* trait from
/// [`FilesystemRead`] / [`FilesystemWrite`] / [`FilesystemSecurity`],
/// never a widening of any of them nor of the frozen
/// [`Filesystem`]; new behaviour ships as a new trait. A driver whose on-disk
/// format can hold a general-purpose, namespaced `key → value` store per inode
/// implements it so the VFS can offer extended attributes and preserve foreign
/// per-file metadata (Acorn/Amiga/Atari/Mac) across a copy; a driver whose
/// format has nowhere to keep them simply does not implement it.
///
/// # The key grammar and bounds live in `lib/fsmeta`
///
/// A `key` is a namespaced, byte-for-byte case-sensitive
/// `tairix_fsmeta`-grammar key (`namespace.rest`, e.g. `acorn.filetype`). The
/// driver validates every key and value against that shared grammar and the
/// fixed security bounds and **fails closed** on any violation — an unknown
/// namespace, a malformed key, or an oversize value is rejected, never stored.
/// Values are opaque bytes; the driver never interprets them.
///
/// # No permission decisions
///
/// As with the sibling traits, the driver makes **no** permission decision:
/// the VFS authorises every attribute operation against the model before
/// calling here. A key's *namespace* decides its access class — the `user`,
/// `acorn`, `amiga`, `atari`, `mac`, and `tairix` namespaces are ordinary file
/// metadata governed by the file's own owner/mode/ACL, while the `system` and
/// `trusted` namespaces guard a security boundary the VFS gates with a
/// capability before delegating. A caller not permitted a namespace never
/// reaches the driver for it, and [`list_attr`](FilesystemAttrs::list_attr)
/// enumerates only keys whose namespace the caller may read.
///
/// # Buffers, not allocation
///
/// [`get_attr`](FilesystemAttrs::get_attr) and
/// [`list_attr`](FilesystemAttrs::list_attr) write into a caller-provided
/// buffer and report the byte count, mirroring
/// [`read_dir`](FilesystemRead::read_dir); a value or key that does not fit is
/// [`DriverError::BufferTooSmall`], not a truncation.
///
/// # Capabilities
///
/// Calls are reached only through the kernel-issued
/// [`DriverHandle`] the host minted at load
/// time ([`CapabilityId::DRV_LOAD`](crate::CapabilityId::DRV_LOAD)), and the
/// VFS additionally requires the mount to be writable for
/// [`set_attr`](FilesystemAttrs::set_attr) /
/// [`remove_attr`](FilesystemAttrs::remove_attr) (a mount carrying
/// [`MountFlags::READ_ONLY`] is never delegated a mutation).
pub trait FilesystemAttrs {
    /// Read the value of attribute `key` on `node` into `value_out`,
    /// returning the number of bytes written, or `None` if `node` carries no
    /// such attribute.
    ///
    /// # Errors
    ///
    /// * [`DriverError::OutOfRange`] if `key` is not a valid namespaced key.
    /// * [`DriverError::BufferTooSmall`] if the value does not fit `value_out`.
    /// * [`DriverError::NotFound`] if `node` does not name a live node.
    /// * [`DriverError::DeviceFault`] on an unrecoverable block read.
    fn get_attr(
        &mut self,
        node: NodeId,
        key: &[u8],
        value_out: &mut [u8],
    ) -> Result<Option<usize>, DriverError>;

    /// Set attribute `key` on `node` to `value`, inserting or replacing it in
    /// one copy-on-write transaction.
    ///
    /// # Errors
    ///
    /// * [`DriverError::OutOfRange`] if `key` is not a valid namespaced key
    ///   (unknown namespace, malformed bytes).
    /// * [`DriverError::LengthOutOfRange`] if `key` or `value` exceeds its
    ///   fixed bound.
    /// * [`DriverError::NoSpace`] if the attribute count, total attribute
    ///   bytes, or the metadata block would be exceeded.
    /// * [`DriverError::NotFound`] if `node` does not name a live node.
    /// * [`DriverError::DeviceFault`] on an unrecoverable block read/write.
    fn set_attr(&mut self, node: NodeId, key: &[u8], value: &[u8]) -> Result<(), DriverError>;

    /// Yield the `index`-th attribute key of `node`, writing it into
    /// `key_out` and returning its length. Returns `Ok(None)` once `index` is
    /// past the last attribute, which is how a caller detects the end.
    ///
    /// Iteration order is the stable on-disk order.
    ///
    /// # Errors
    ///
    /// * [`DriverError::BufferTooSmall`] if the key does not fit `key_out`.
    /// * [`DriverError::NotFound`] if `node` does not name a live node.
    /// * [`DriverError::DeviceFault`] on an unrecoverable block read.
    fn list_attr(
        &mut self,
        node: NodeId,
        index: u64,
        key_out: &mut [u8],
    ) -> Result<Option<usize>, DriverError>;

    /// Remove attribute `key` from `node` in one copy-on-write transaction.
    ///
    /// # Errors
    ///
    /// * [`DriverError::OutOfRange`] if `key` is not a valid namespaced key.
    /// * [`DriverError::NotFound`] if `node` does not name a live node, or
    ///   carries no such attribute.
    /// * [`DriverError::DeviceFault`] on an unrecoverable block read/write.
    fn remove_attr(&mut self, node: NodeId, key: &[u8]) -> Result<(), DriverError>;
}

/// The combined per-node view an attribute operation runs against: path
/// resolution ([`FilesystemRead`]), the per-inode security records the VFS
/// authorises with ([`FilesystemSecurity`]), and the attribute store itself
/// ([`FilesystemAttrs`]) — one object, because the secured VFS must resolve
/// and authorise on the *same* driver it then reads or mutates.
///
/// Blanket-implemented for every type carrying the three traits; never
/// implemented by hand.
pub trait FilesystemAttrsFs: FilesystemRead + FilesystemSecurity + FilesystemAttrs {}

impl<T: FilesystemRead + FilesystemSecurity + FilesystemAttrs + ?Sized> FilesystemAttrsFs for T {}

/// Opt-in discovery of a driver's extended-attribute support.
///
/// [`FilesystemAttrs`] is deliberately implemented only by drivers whose
/// on-disk format can hold attributes, so a type-erased mount (the kernel's
/// `Box<dyn KernelFs>`) cannot require it as a bound. This facet is the
/// honest bridge: every mountable driver implements the *provider*, and the
/// default answer is `None` — "this format stores no attributes" — so an
/// `fs_attr_*` call on such a mount fails closed with a typed refusal. A
/// driver that does implement [`FilesystemAttrs`] overrides
/// [`attrs_fs`](Self::attrs_fs) to return itself; a caching or forwarding
/// wrapper delegates to its inner driver. The facet grants nothing: the VFS
/// still authorises every operation against the per-inode model before the
/// returned view is touched.
pub trait FilesystemAttrsProvider {
    /// The attribute-capable view of this driver, or `None` when its
    /// on-disk format has nowhere to store extended attributes.
    fn attrs_fs(&mut self) -> Option<&mut dyn FilesystemAttrsFs> {
        None
    }
}

/// The space accounting a mounted volume reports about itself.
///
/// Sizes are counted in whole blocks of `block_size` bytes — the unit the
/// mounted format actually allocates in — so a consumer multiplies rather
/// than guessing a divisor. Every count is 64-bit: a volume, and therefore
/// its block counts, may exceed what 32 bits (or pointer width) can hold.
///
/// `avail_blocks` is the portion of `free_blocks` an ordinary data
/// allocation may consume; a format that holds blocks back (e.g. a metadata
/// reserve that keeps a full volume repairable) reports the smaller number
/// here, so `avail_blocks <= free_blocks` always holds and a consumer is
/// never promised space the driver would refuse.
///
/// `files` / `files_free` report the volume's inode capacity for a format
/// with a fixed inode table. A format whose inodes are allocated dynamically
/// (`arxfs`) has no fixed capacity to report and carries `0` in both — the
/// honest "untracked" answer, never a fabricated total.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct VolumeStats {
    /// The allocation unit, in bytes, the block counts are denominated in.
    pub block_size: u32,
    /// Total data blocks the volume holds.
    pub total_blocks: u64,
    /// Blocks currently unallocated.
    pub free_blocks: u64,
    /// Blocks an ordinary data allocation may still consume
    /// (`free_blocks` minus any reserve the format withholds).
    pub avail_blocks: u64,
    /// Total inode capacity, or `0` when the format tracks no fixed table.
    pub files: u64,
    /// Free inodes, or `0` when the format tracks no fixed table.
    pub files_free: u64,
}

/// Whole-volume space statistics for a mounted filesystem.
///
/// This is a **versioned `abi-v1` extension** — a *separate* trait from
/// [`FilesystemRead`] / [`FilesystemWrite`] / [`FilesystemSecurity`] /
/// [`FilesystemAttrs`], never a widening of any
/// of them nor of the frozen [`Filesystem`]; new behaviour ships as a new
/// trait. Every mountable driver implements it: a volume that can be
/// mounted always has a size, and reporting it is a read of the driver's
/// own accounting, never a device walk.
///
/// The driver only *reports* the numbers; it makes no permission decision
/// (the query is authorised kernel-side before the driver is reached).
///
/// # Capabilities
///
/// Calls are reached only through the kernel-issued
/// [`DriverHandle`] the host minted at load
/// time ([`CapabilityId::DRV_LOAD`](crate::CapabilityId::DRV_LOAD)).
pub trait FilesystemStats {
    /// Report the volume's current space accounting.
    ///
    /// # Errors
    ///
    /// * [`DriverError::DeviceFault`] on an unrecoverable failure of the
    ///   underlying device (a driver that keeps its accounting in memory
    ///   never fails).
    fn stats(&mut self) -> Result<VolumeStats, DriverError>;
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
                    nlink: 2,
                    size: 0,
                    allocated: 0,
                    times: NodeTimes::default(),
                })
            } else if node == FILE {
                Ok(NodeInfo {
                    kind: NodeKind::RegularFile,
                    nlink: 1,
                    size: FILE_BODY.len() as u64,
                    allocated: FILE_BODY.len() as u64,
                    times: NodeTimes::default(),
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
            cursor: u64,
            name_out: &mut [u8],
        ) -> Result<Option<DirEntry>, DriverError> {
            if dir != ROOT {
                return Err(DriverError::Unsupported);
            }
            if cursor != 0 {
                return Ok(None);
            }
            if name_out.len() < FILE_NAME.len() {
                return Err(DriverError::BufferTooSmall);
            }
            name_out[..FILE_NAME.len()].copy_from_slice(FILE_NAME);
            Ok(Some(DirEntry {
                node: FILE,
                info: NodeInfo {
                    kind: NodeKind::RegularFile,
                    nlink: 1,
                    size: FILE_BODY.len() as u64,
                    allocated: FILE_BODY.len() as u64,
                    times: NodeTimes::default(),
                },
                name_len: FILE_NAME.len(),
                next_cursor: 1,
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
        assert_eq!(entry.info.kind, NodeKind::RegularFile);
        assert_eq!(entry.info.size, FILE_BODY.len() as u64);
        assert_eq!(&name[..entry.name_len], FILE_NAME);
        assert_eq!(fs.read_dir(ROOT, entry.next_cursor, &mut name), Ok(None));
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

    /// A `FilesystemWrite` that stores at most `step` bytes per call, as a
    /// driver bounding the memory one write may pin does.
    struct ShortWriteFs {
        step: usize,
        body: [u8; 8],
        len: usize,
        calls: usize,
    }

    impl FilesystemWrite for ShortWriteFs {
        fn create(&mut self, _: NodeId, _: &[u8], _: NodeKind) -> Result<NodeId, DriverError> {
            Err(DriverError::Unsupported)
        }

        fn write_at(
            &mut self,
            _dir: NodeId,
            _name: &[u8],
            offset: u64,
            data: &[u8],
        ) -> Result<usize, DriverError> {
            self.calls += 1;
            let start = usize::try_from(offset).map_err(|_| DriverError::OutOfRange)?;
            let take = data.len().min(self.step);
            let end = start.checked_add(take).ok_or(DriverError::OutOfRange)?;
            let slot = self
                .body
                .get_mut(start..end)
                .ok_or(DriverError::DeviceFault)?;
            slot.copy_from_slice(&data[..take]);
            self.len = self.len.max(end);
            Ok(take)
        }

        fn truncate(&mut self, _: NodeId, _: &[u8], _: u64) -> Result<(), DriverError> {
            Err(DriverError::Unsupported)
        }

        fn remove(&mut self, _: NodeId, _: &[u8]) -> Result<(), DriverError> {
            Err(DriverError::Unsupported)
        }

        fn rename(&mut self, _: NodeId, _: &[u8], _: NodeId, _: &[u8]) -> Result<(), DriverError> {
            Err(DriverError::Unsupported)
        }

        fn flush(&mut self) -> Result<(), DriverError> {
            Ok(())
        }
    }

    #[test]
    fn write_all_resumes_across_short_writes() {
        let mut fs = ShortWriteFs {
            step: 2,
            body: [0; 8],
            len: 0,
            calls: 0,
        };
        assert_eq!(fs.write_all(W_ROOT, W_NAME, 0, b"abcdef"), Ok(()));
        assert_eq!(&fs.body[..6], b"abcdef");
        assert_eq!(fs.calls, 3, "two bytes a call, six bytes, three calls");
        // Each resumed call must land at the offset the last one reached, not
        // back at the start.
        assert_eq!(fs.len, 6);
    }

    #[test]
    fn write_all_refuses_a_driver_that_makes_no_progress() {
        // A zero count with bytes left is a refusal, not back-pressure:
        // retrying it forever is not an answer.
        let mut fs = ShortWriteFs {
            step: 0,
            body: [0; 8],
            len: 0,
            calls: 0,
        };
        assert_eq!(
            fs.write_all(W_ROOT, W_NAME, 0, b"ab"),
            Err(DriverError::NoSpace)
        );
        assert_eq!(fs.calls, 1, "a stalled write is refused, never looped");
    }

    #[test]
    fn write_all_of_nothing_touches_the_driver_not_at_all() {
        let mut fs = ShortWriteFs {
            step: 2,
            body: [0; 8],
            len: 0,
            calls: 0,
        };
        assert_eq!(fs.write_all(W_ROOT, W_NAME, 0, &[]), Ok(()));
        assert_eq!(fs.calls, 0);
    }

    /// A minimal `(dir, name)`-addressed `FilesystemWrite` holding one
    /// regular file directly under a root directory. It exercises the
    /// whole `abi-v1` write surface: create, extend via `write_at`,
    /// `truncate`, `remove`, and `rename` (which re-labels the single
    /// file within the root).
    struct MockWriteFs {
        present: bool,
        name: [u8; 8],
        name_len: usize,
        body: [u8; 8],
        len: usize,
    }

    const W_ROOT: NodeId = NodeId::from_raw(1);
    const W_FILE: NodeId = NodeId::from_raw(2);
    const W_NAME: &[u8] = b"data";

    impl MockWriteFs {
        fn empty() -> Self {
            Self {
                present: false,
                name: [0; 8],
                name_len: 0,
                body: [0; 8],
                len: 0,
            }
        }

        fn name(&self) -> &[u8] {
            &self.name[..self.name_len]
        }

        fn store_name(&mut self, name: &[u8]) -> Result<(), DriverError> {
            if name.is_empty() || name.len() > self.name.len() {
                return Err(DriverError::LengthOutOfRange);
            }
            self.name[..name.len()].copy_from_slice(name);
            self.name_len = name.len();
            Ok(())
        }
    }

    impl FilesystemWrite for MockWriteFs {
        fn create(
            &mut self,
            dir: NodeId,
            name: &[u8],
            kind: NodeKind,
        ) -> Result<NodeId, DriverError> {
            if dir != W_ROOT {
                return Err(DriverError::Unsupported);
            }
            if kind != NodeKind::RegularFile {
                return Err(DriverError::Unsupported);
            }
            if self.present {
                return Err(DriverError::AlreadyExists);
            }
            self.store_name(name)?;
            self.present = true;
            self.len = 0;
            Ok(W_FILE)
        }

        fn write_at(
            &mut self,
            dir: NodeId,
            name: &[u8],
            offset: u64,
            data: &[u8],
        ) -> Result<usize, DriverError> {
            if dir != W_ROOT {
                return Err(DriverError::Unsupported);
            }
            if !self.present {
                return Err(DriverError::NotFound);
            }
            if name != self.name() {
                return Err(DriverError::Unsupported);
            }
            let start = usize::try_from(offset).map_err(|_| DriverError::LengthOutOfRange)?;
            let end = start
                .checked_add(data.len())
                .ok_or(DriverError::LengthOutOfRange)?;
            if end > self.body.len() {
                return Err(DriverError::DeviceFault);
            }
            for byte in &mut self.body[self.len..start] {
                *byte = 0;
            }
            self.body[start..end].copy_from_slice(data);
            self.len = self.len.max(end);
            Ok(data.len())
        }

        fn truncate(&mut self, dir: NodeId, name: &[u8], size: u64) -> Result<(), DriverError> {
            if dir != W_ROOT {
                return Err(DriverError::Unsupported);
            }
            if !self.present {
                return Err(DriverError::NotFound);
            }
            if name != self.name() {
                return Err(DriverError::Unsupported);
            }
            let new = usize::try_from(size).map_err(|_| DriverError::LengthOutOfRange)?;
            if new > self.body.len() {
                return Err(DriverError::DeviceFault);
            }
            for byte in &mut self.body[self.len.min(new)..new] {
                *byte = 0;
            }
            self.len = new;
            Ok(())
        }

        fn remove(&mut self, dir: NodeId, name: &[u8]) -> Result<(), DriverError> {
            if dir != W_ROOT {
                return Err(DriverError::Unsupported);
            }
            if !self.present || name != self.name() {
                return Err(DriverError::NotFound);
            }
            self.present = false;
            self.len = 0;
            self.name_len = 0;
            Ok(())
        }

        fn rename(
            &mut self,
            src_dir: NodeId,
            src_name: &[u8],
            dst_dir: NodeId,
            dst_name: &[u8],
        ) -> Result<(), DriverError> {
            if src_dir != W_ROOT || dst_dir != W_ROOT {
                return Err(DriverError::Unsupported);
            }
            if !self.present || src_name != self.name() {
                return Err(DriverError::NotFound);
            }
            if dst_name == src_name {
                return Ok(());
            }
            // The model holds a single file, so the destination name can
            // never already be in use; re-label the present file.
            self.store_name(dst_name)
        }

        fn flush(&mut self) -> Result<(), DriverError> {
            Ok(())
        }
    }

    #[test]
    fn mock_write_fs_round_trip() {
        let mut fs = MockWriteFs::empty();
        assert_eq!(fs.create(W_ROOT, W_NAME, NodeKind::RegularFile), Ok(W_FILE));
        // Creating it again is rejected as a taken name, never as a
        // transient a caller would retry.
        assert_eq!(
            fs.create(W_ROOT, W_NAME, NodeKind::RegularFile),
            Err(DriverError::AlreadyExists)
        );
        assert_eq!(fs.write_at(W_ROOT, W_NAME, 0, b"abc"), Ok(3));
        assert_eq!(fs.len, 3);
        // Writing past the end zero-fills the gap and extends.
        assert_eq!(fs.write_at(W_ROOT, W_NAME, 5, b"Z"), Ok(1));
        assert_eq!(&fs.body[..fs.len], b"abc\0\0Z");
        fs.truncate(W_ROOT, W_NAME, 2).expect("shrink");
        assert_eq!(&fs.body[..fs.len], b"ab");
        assert!(fs.flush().is_ok());
        fs.remove(W_ROOT, W_NAME).expect("unlink");
        assert_eq!(
            fs.write_at(W_ROOT, W_NAME, 0, b"x"),
            Err(DriverError::NotFound)
        );
    }

    #[test]
    fn mock_write_fs_rejects_non_root_dir() {
        let mut fs = MockWriteFs::empty();
        assert_eq!(
            fs.create(W_FILE, W_NAME, NodeKind::RegularFile),
            Err(DriverError::Unsupported)
        );
        assert_eq!(fs.remove(W_FILE, W_NAME), Err(DriverError::Unsupported));
    }

    #[test]
    fn mock_write_fs_rename_relabels_the_file() {
        let mut fs = MockWriteFs::empty();
        assert_eq!(fs.create(W_ROOT, W_NAME, NodeKind::RegularFile), Ok(W_FILE));
        assert_eq!(fs.write_at(W_ROOT, W_NAME, 0, b"hi"), Ok(2));
        // Renaming a missing source fails closed.
        assert_eq!(
            fs.rename(W_ROOT, b"absent", W_ROOT, b"moved"),
            Err(DriverError::NotFound)
        );
        // The self-rename is a no-op success.
        assert_eq!(fs.rename(W_ROOT, W_NAME, W_ROOT, W_NAME), Ok(()));
        // Moving to a new name re-labels the file and preserves contents.
        assert_eq!(fs.rename(W_ROOT, W_NAME, W_ROOT, b"moved"), Ok(()));
        assert_eq!(
            fs.write_at(W_ROOT, W_NAME, 0, b"x"),
            Err(DriverError::Unsupported)
        );
        assert_eq!(fs.write_at(W_ROOT, b"moved", 2, b"!"), Ok(1));
        assert_eq!(&fs.body[..fs.len], b"hi!");
        // A non-root directory is refused.
        assert_eq!(
            fs.rename(W_FILE, b"moved", W_ROOT, b"x"),
            Err(DriverError::Unsupported)
        );
    }

    #[test]
    fn node_security_acl_is_bounded() {
        let mut sec = NodeSecurity::new(0o640, 7, 9);
        assert!(sec.acl().is_empty());
        assert_eq!(sec.required_cap, None);
        for _ in 0..MAX_ACL_ENTRIES {
            sec.push_acl(SecurityAcl {
                subject: SecuritySubject::User(1),
                perms: 0b100,
            })
            .expect("within bound");
        }
        assert_eq!(sec.acl().len(), MAX_ACL_ENTRIES);
        assert_eq!(
            sec.push_acl(SecurityAcl {
                subject: SecuritySubject::Group(2),
                perms: 0b010,
            }),
            Err(DriverError::LengthOutOfRange)
        );
    }

    /// A node whose stored record the VFS reads and replaces through the
    /// trait.
    struct MockSecurityFs {
        sec: NodeSecurity,
    }

    impl FilesystemSecurity for MockSecurityFs {
        fn security(&mut self, node: NodeId) -> Result<NodeSecurity, DriverError> {
            if node == NodeId::NONE {
                return Err(DriverError::NotFound);
            }
            Ok(self.sec)
        }

        fn set_security(
            &mut self,
            node: NodeId,
            security: NodeSecurity,
        ) -> Result<(), DriverError> {
            if node == NodeId::NONE {
                return Err(DriverError::NotFound);
            }
            self.sec = security;
            Ok(())
        }
    }

    #[test]
    fn mock_security_fs_reports_stored_record() {
        let mut sec = NodeSecurity::new(0o600, 7, 9);
        sec.required_cap = Some(CapabilityId::AUDIT_READ);
        sec.push_acl(SecurityAcl {
            subject: SecuritySubject::Group(11),
            perms: 0b110,
        })
        .expect("acl");
        let mut fs = MockSecurityFs { sec };
        assert_eq!(fs.security(NodeId::from_raw(1)), Ok(sec));
        assert_eq!(fs.security(NodeId::NONE), Err(DriverError::NotFound));

        let replacement = NodeSecurity::new(0o750, 8, 10);
        assert_eq!(
            fs.set_security(NodeId::NONE, replacement),
            Err(DriverError::NotFound)
        );
        assert_eq!(fs.security(NodeId::from_raw(1)), Ok(sec));
        assert_eq!(fs.set_security(NodeId::from_raw(1), replacement), Ok(()));
        assert_eq!(fs.security(NodeId::from_raw(1)), Ok(replacement));
    }

    /// A node holding a single extended attribute in fixed buffers, enough to
    /// exercise the [`FilesystemAttrs`] contract (buffer sizing, `None` for
    /// absent, `NotFound` for a dead node) without an allocator. The key
    /// grammar and bounds are validated in `lib/fsmeta` and by the `ARXFS`
    /// integration tests, not here.
    struct MockAttrsFs {
        key: [u8; 64],
        key_len: usize,
        value: [u8; 64],
        value_len: usize,
        present: bool,
    }

    impl MockAttrsFs {
        fn empty() -> Self {
            Self {
                key: [0u8; 64],
                key_len: 0,
                value: [0u8; 64],
                value_len: 0,
                present: false,
            }
        }

        fn matches(&self, key: &[u8]) -> bool {
            self.present && &self.key[..self.key_len] == key
        }
    }

    impl FilesystemAttrs for MockAttrsFs {
        fn get_attr(
            &mut self,
            node: NodeId,
            key: &[u8],
            value_out: &mut [u8],
        ) -> Result<Option<usize>, DriverError> {
            if node == NodeId::NONE {
                return Err(DriverError::NotFound);
            }
            if !self.matches(key) {
                return Ok(None);
            }
            if value_out.len() < self.value_len {
                return Err(DriverError::BufferTooSmall);
            }
            value_out[..self.value_len].copy_from_slice(&self.value[..self.value_len]);
            Ok(Some(self.value_len))
        }

        fn set_attr(&mut self, node: NodeId, key: &[u8], value: &[u8]) -> Result<(), DriverError> {
            if node == NodeId::NONE {
                return Err(DriverError::NotFound);
            }
            if key.len() > self.key.len() || value.len() > self.value.len() {
                return Err(DriverError::NoSpace);
            }
            self.key[..key.len()].copy_from_slice(key);
            self.key_len = key.len();
            self.value[..value.len()].copy_from_slice(value);
            self.value_len = value.len();
            self.present = true;
            Ok(())
        }

        fn list_attr(
            &mut self,
            node: NodeId,
            index: u64,
            key_out: &mut [u8],
        ) -> Result<Option<usize>, DriverError> {
            if node == NodeId::NONE {
                return Err(DriverError::NotFound);
            }
            if index != 0 || !self.present {
                return Ok(None);
            }
            if key_out.len() < self.key_len {
                return Err(DriverError::BufferTooSmall);
            }
            key_out[..self.key_len].copy_from_slice(&self.key[..self.key_len]);
            Ok(Some(self.key_len))
        }

        fn remove_attr(&mut self, node: NodeId, key: &[u8]) -> Result<(), DriverError> {
            if node == NodeId::NONE {
                return Err(DriverError::NotFound);
            }
            if !self.matches(key) {
                return Err(DriverError::NotFound);
            }
            self.present = false;
            Ok(())
        }
    }

    #[test]
    fn mock_attrs_fs_round_trips() {
        let mut fs = MockAttrsFs::empty();
        let node = NodeId::from_raw(1);
        assert_eq!(fs.set_attr(node, b"user.comment", b"hi"), Ok(()));

        let mut buf = [0u8; 16];
        assert_eq!(fs.get_attr(node, b"user.comment", &mut buf), Ok(Some(2)));
        assert_eq!(&buf[..2], b"hi");
        assert_eq!(fs.get_attr(node, b"user.absent", &mut buf), Ok(None));

        // A too-small value buffer fails closed rather than truncating.
        let mut tiny = [0u8; 1];
        assert_eq!(
            fs.get_attr(node, b"user.comment", &mut tiny),
            Err(DriverError::BufferTooSmall)
        );

        // Listing yields the one key then terminates.
        let mut key_buf = [0u8; 64];
        assert_eq!(fs.list_attr(node, 0, &mut key_buf), Ok(Some(12)));
        assert_eq!(&key_buf[..12], b"user.comment");
        assert_eq!(fs.list_attr(node, 1, &mut key_buf), Ok(None));

        assert_eq!(fs.remove_attr(node, b"user.comment"), Ok(()));
        assert_eq!(
            fs.remove_attr(node, b"user.comment"),
            Err(DriverError::NotFound)
        );
        assert_eq!(fs.get_attr(node, b"user.comment", &mut buf), Ok(None));

        // A dead node fails closed on every operation.
        assert_eq!(
            fs.get_attr(NodeId::NONE, b"user.comment", &mut buf),
            Err(DriverError::NotFound)
        );
        assert_eq!(
            fs.set_attr(NodeId::NONE, b"user.comment", b"x"),
            Err(DriverError::NotFound)
        );
    }

    /// Reports a fixed accounting; a zeroed record models an untracked
    /// inode table.
    struct MockStatsFs {
        stats: VolumeStats,
    }

    impl FilesystemStats for MockStatsFs {
        fn stats(&mut self) -> Result<VolumeStats, DriverError> {
            Ok(self.stats)
        }
    }

    #[test]
    fn mock_stats_fs_reports_stored_record() {
        let stats = VolumeStats {
            block_size: 4096,
            total_blocks: 1024,
            free_blocks: 512,
            avail_blocks: 480,
            files: 0,
            files_free: 0,
        };
        let mut fs = MockStatsFs { stats };
        let reported = fs.stats().expect("stats");
        assert_eq!(reported, stats);
        // The contract: available never exceeds free, free never exceeds
        // total, and the dynamic-inode answer is the zero pair.
        assert!(reported.avail_blocks <= reported.free_blocks);
        assert!(reported.free_blocks <= reported.total_blocks);
        assert_eq!((reported.files, reported.files_free), (0, 0));
    }
}
