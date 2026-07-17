//! Shared harness for the pjdfstest-equivalent POSIX filesystem
//! conformance suite (`PLAN.md` Stage 5).
//!
//! The suite drives the **real** `arxfs` driver
//! ([`tairix_drv_fs_arxfs::ARXFS`]) through the **real**
//! [`tairix_kernel_core::fs::Vfs`] policy layer and asserts the
//! POSIX-visible semantics of every filesystem operation the system
//! exposes: directory and file creation, unlink/rmdir, truncate,
//! readdir/stat, the permission model (mode bits, ACLs, and the
//! optional per-inode capability gate), the on-disk layout rules, and
//! the stable errno mapping. It is the analogue of `pjdfstest`: a body of
//! black-box assertions about return values and error codes, run against
//! the production code paths rather than a parallel re-implementation.
//!
//! Like `pjdfstest`, the suite is filesystem-agnostic by construction: it
//! talks to the VFS and a `drivers/filesystem/*` driver behind the frozen
//! ABI traits, so a second driver can be exercised by swapping the backing
//! constructor. `arxfs` is the first subject because it is the native FS
//! that stores a full per-inode record, which the
//! capability/ACL-gate assertions require.
//!
//! The block-device-over-QEMU mount path is covered separately by the
//! `arxfs`/`fat32` virtio-blk verticals; this crate is the *semantics*
//! suite and runs on the host against the identical driver and VFS code.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use tairix_abi::driver::block::{Block, BlockGeometry};
use tairix_abi::driver::filesystem::MountFlags;
use tairix_abi::driver::DriverHandle;
use tairix_abi::DriverError;
use tairix_drv_fs_arxfs::{EntropySource, VolumeKey, ARXFS, VOLUME_KEY_LEN};

/// Volume key the suite formats its arxfs test volume with. `ARXFS` is
/// encrypted-by-default (`docs/src/filesystem/arxfs-spec.md` §5).
const SUITE_KEY: VolumeKey = [0x5a; VOLUME_KEY_LEN];

/// Deterministic stand-in for the platform RNG seam: a byte counter that gives
/// `ARXFS::format` distinct, reproducible key material and UUID. Test
/// scaffolding only, never a production entropy source.
struct SuiteEntropy {
    next: u8,
}

impl EntropySource for SuiteEntropy {
    fn fill(&mut self, out: &mut [u8]) -> Result<(), DriverError> {
        for byte in out.iter_mut() {
            *byte = self.next;
            self.next = self.next.wrapping_add(1);
        }
        Ok(())
    }
}

pub use tairix_abi::driver::filesystem::{
    FilesystemRead, FilesystemSecurity, FilesystemWrite, NodeId, NodeKind, NodeSecurity,
    SecurityAcl, SecuritySubject,
};
pub use tairix_abi::CapabilityId;
pub use tairix_abi::Errno;
pub use tairix_caps::CapabilitySet;
pub use tairix_kernel_core::fs::{Credentials, Mode, Path, Vfs, VfsError};
pub use tairix_kernel_sec::{GroupId, UserId};

/// Logical block (sector) size of the in-memory device, in bytes. The
/// `arxfs` minimum block size, matching the verticals' 512-byte sectors.
pub const SECTOR_BYTES: usize = 512;

/// Size of the in-memory device, in 512-byte sectors (1 MiB) — large
/// enough for the inode table, bitmap, journal, and a non-trivial data
/// region.
pub const TOTAL_SECTORS: u64 = 2048;

/// Number of inodes the test volume is formatted with.
pub const INODE_COUNT: u32 = 64;

/// Mount point at which the `arxfs` volume is attached in the test VFS.
/// Lives under `/Storage`, the top-level directory for mounted
/// volumes, which is writable in the default layout.
pub const MOUNT: &str = "/Storage/vol";

/// Uid and gid `arxfs` stamps onto the volume root and every node it
/// creates (see `ARXFS::format`/`create`). A credential with this
/// identity owns the whole tree, so it stands in for the administrative
/// installer user that lays the volume down.
pub const ROOT_UID: u32 = 0;
/// Owning gid of the `arxfs` volume root and freshly created nodes.
pub const ROOT_GID: u32 = 0;

/// The live `arxfs` driver instance the suite drives, bound to the
/// in-memory [`VecBlock`] device.
pub type LiveFs = ARXFS<VecBlock>;

/// An in-memory [`Block`] device backing the test volume. Addresses
/// [`SECTOR_BYTES`]-byte sectors exactly as the virtio-blk device the
/// verticals mount does.
pub struct VecBlock {
    store: Vec<u8>,
}

impl VecBlock {
    /// A zeroed device of `sectors` sectors.
    #[must_use]
    pub fn new(sectors: u64) -> Self {
        let len = usize::try_from(sectors).unwrap_or(0) * SECTOR_BYTES;
        Self {
            store: vec![0u8; len],
        }
    }

    /// Byte span `[start, end)` for `len` bytes at sector `lba`, or an
    /// error if the access is unaligned or out of range.
    fn span(&self, lba: u64, len: usize) -> Result<(usize, usize), DriverError> {
        if len == 0 || len % SECTOR_BYTES != 0 {
            return Err(DriverError::BufferTooSmall);
        }
        let start = usize::try_from(lba)
            .ok()
            .and_then(|l| l.checked_mul(SECTOR_BYTES))
            .ok_or(DriverError::LengthOutOfRange)?;
        let end = start
            .checked_add(len)
            .ok_or(DriverError::LengthOutOfRange)?;
        if end > self.store.len() {
            return Err(DriverError::LengthOutOfRange);
        }
        Ok((start, end))
    }
}

impl Block for VecBlock {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        Ok(BlockGeometry {
            block_size: u32::try_from(SECTOR_BYTES).unwrap_or(0),
            block_count: TOTAL_SECTORS,
        })
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        let (start, end) = self.span(lba, buf.len())?;
        buf.copy_from_slice(&self.store[start..end]);
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        let (start, end) = self.span(lba, buf.len())?;
        self.store[start..end].copy_from_slice(buf);
        Ok(())
    }
}

/// Parse an absolute path, panicking if it is malformed.
///
/// # Panics
///
/// Panics if `text` is not a valid absolute path. The suite only ever
/// passes path literals it controls, so a failure is a test bug.
#[must_use]
pub fn path(text: &str) -> Path {
    Path::parse(text).expect("test path literal is a valid absolute path")
}

/// A path inside the mounted `arxfs` volume: `MOUNT` joined with `rel`
/// (which must not begin with `/`).
///
/// # Panics
///
/// Panics if the resulting path is malformed (a test bug).
#[must_use]
pub fn vol_path(rel: &str) -> Path {
    path(&format!("{MOUNT}/{rel}"))
}

/// Build a [`Credentials`] for `(uid, gid)` with no supplementary groups
/// and the borrowed capability set `caps`.
#[must_use]
pub fn cred(uid: u32, gid: u32, caps: &CapabilitySet) -> Credentials<'_> {
    Credentials {
        uid: UserId(uid),
        gid: GroupId(gid),
        supplementary_gids: &[],
        caps,
    }
}

/// Build a [`Credentials`] for `(uid, gid)` carrying the supplementary
/// groups `sup` and the borrowed capability set `caps`.
#[must_use]
pub fn cred_with_groups<'a>(
    uid: u32,
    gid: u32,
    sup: &'a [GroupId],
    caps: &'a CapabilitySet,
) -> Credentials<'a> {
    Credentials {
        uid: UserId(uid),
        gid: GroupId(gid),
        supplementary_gids: sup,
        caps,
    }
}

/// Build a default-layout [`Vfs`] (owner `(ROOT_UID, ROOT_GID)`) with a
/// freshly formatted `arxfs` volume mounted at [`MOUNT`], and return both
/// the VFS and the live driver.
///
/// The mount carries [`MountFlags::READ_ONLY`] when `read_only` is set, so
/// the suite can exercise the read-only-mount refusal on a backed
/// subtree.
///
/// # Panics
///
/// Panics if the fixed-geometry volume fails to format or the mount
/// point cannot be laid down — either is a test-harness bug, not a
/// runtime condition.
#[must_use]
pub fn arxfs_backed_vfs(read_only: bool) -> (Vfs, LiveFs) {
    let fs = ARXFS::format(
        VecBlock::new(TOTAL_SECTORS),
        INODE_COUNT,
        &SUITE_KEY,
        &mut SuiteEntropy { next: 1 },
    )
    .expect("format the fixed-geometry arxfs test volume");

    let mut vfs = Vfs::with_default_layout(UserId(ROOT_UID), GroupId(ROOT_GID));
    let caps = CapabilitySet::empty();
    let owner = cred(ROOT_UID, ROOT_GID, &caps);
    vfs.mkdir(&owner, &path(MOUNT), Mode::from_bits(0o755))
        .expect("create the volume mount point under /Storage");

    let handle = DriverHandle::from_raw(0x5f5f).expect("non-zero driver handle");
    let flags = if read_only {
        MountFlags::READ_ONLY
    } else {
        MountFlags::default()
    };
    vfs.mounts_write()
        .mount(path(MOUNT), flags, Some(handle))
        .expect("mount the arxfs volume");

    (vfs, fs)
}

/// A default-layout [`Vfs`] (owner `(ROOT_UID, ROOT_GID)`) with no driver
/// mounted, for the in-RAM layout-enforcement tests.
#[must_use]
pub fn default_layout_vfs() -> Vfs {
    Vfs::with_default_layout(UserId(ROOT_UID), GroupId(ROOT_GID))
}

/// Look up `name` directly under the driver root and return its
/// [`NodeId`], for tests that set or read a node's stored record.
///
/// # Panics
///
/// Panics if `name` is not present at the driver root (a test bug).
#[must_use]
pub fn root_node_id(fs: &mut LiveFs, name: &[u8]) -> NodeId {
    let root = fs.root();
    fs.lookup(root, name)
        .expect("a node the test just created is present at the volume root")
}
