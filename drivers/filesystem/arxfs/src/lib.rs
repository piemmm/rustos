//! TAIRiX native filesystem driver (`arxfs`).
//!
//! `arxfs` is the native TAIRiX filesystem: a block-backed, copy-on-write
//! filesystem that stores full POSIX metadata plus an inline access-control
//! list and an optional capability gate **per inode**. It
//! sits behind any [`tairix_abi::driver::block::Block`] device and exposes
//! itself through the versioned [`FilesystemRead`] / [`FilesystemWrite`] /
//! [`FilesystemSecurity`] surfaces (new behaviour ships as a new trait, never by widening the
//! frozen mount/unmount [`Filesystem`](tairix_abi::driver::filesystem::Filesystem)).
//! It reports creation, modification, and metadata-change timestamps through
//! [`NodeInfo::times`](tairix_abi::driver::filesystem::NodeInfo::times); it
//! deliberately does **not** track access time (atime), reporting
//! [`Time64::UNIX_EPOCH`] for it.
//!
//! # Crash consistency (copy-on-write + superblock ring)
//!
//! Every mutation is a transaction. Modified metadata and data are written
//! **copy-on-write** to freshly allocated blocks — a block reachable from the
//! last committed transaction root is never overwritten in place — and the
//! transaction publishes a new root through a ring of superblock slots. The
//! commit order (`transaction` module) is: write the copy-on-write blocks,
//! write the new transaction root carrying its commit record, then publish the
//! next superblock-ring slot pointing at that root. A crash before the slot is
//! published leaves the previous committed root selected; `open` scans the ring
//! and selects the highest-generation slot whose root validates, so a mount
//! after a power loss lands on a whole transaction boundary, never a torn one.
//!
//! Every metadata block is self-identifying (`header` module): it carries its
//! type, the volume UUID, a generation, and its expected address under a
//! checksum, so a stale, misdirected, wrong-type, or torn block is rejected at
//! decode time (fail closed).
//!
//! # Public surface
//!
//! Per the only public *function* is [`register`]. [`ARXFS`]
//! is a public *type* the driver host instantiates with [`ARXFS::format`] /
//! [`ARXFS::open`].
//!
//! # Capabilities
//!
//! Loading requires
//! [`CapabilityId::DRV_LOAD`](tairix_abi::CapabilityId::DRV_LOAD). The driver
//! runs in user space; it does not request `CAP_DRV_KERNEL`.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;

use tairix_abi::driver::block::Block;
use tairix_abi::driver::filesystem::{
    DirEntry, FilesystemAttrs, FilesystemAttrsFs, FilesystemAttrsProvider, FilesystemRead,
    FilesystemSecurity, FilesystemStats, FilesystemWrite, NodeId, NodeInfo, NodeKind, NodeTimes,
    VolumeStats,
};
pub use tairix_abi::driver::filesystem::{
    NodeSecurity as Security, SecurityAcl as AclEntry, SecuritySubject as AclSubject,
};
use tairix_abi::fs::FS_SYMLINK_MAX;
use tairix_abi::time::Time64;
use tairix_abi::{CapabilityId, DriverError, DriverHandle, DriverHost};
use tairix_crypto::{AeadKey, MacKey};
use tairix_fsmeta::{AttrFlags, AttrKey, AttrSet};
use zeroize::Zeroize;

mod allocator;
mod allocmap;
mod btree;
mod check;
mod cluster;
mod crypto;
mod dedupe;
mod discard;
mod header;
mod health;
mod integrity;
mod pagecache;
mod reconcile;
mod scratch;
mod scrub;
mod superblock;
mod transaction;
mod unlock;
mod xform;

#[cfg(test)]
mod tests;

pub use check::{CheckReport, RescueReport, RescueSink, StructureVerdict};
use crypto::{
    decrypt_region, encrypt_region, CryptoHeader, VolumeKeys, CRYPTO_HEADER_LEN, CRYPTO_TRAILER,
};
pub use crypto::{EntropySource, VolumeKey, VOLUME_KEY_LEN};
pub use discard::{TrimReport, TRIM_BATCH_RANGES};
pub use health::{HealthReport, HealthState, HealthThresholds};
use integrity::{
    logical_hash, physical_checksum, read_stored_form, write_stored_form, DataFault, StoredForm,
    COMPRESSION_DESCRIPTOR_LEN, DATA_INTEGRITY_TRAILER, LOGICAL_HASH_LEN, PHYS_CHECKSUM_LEN,
};
pub use scrub::{PassVerdict, ScrubBudget, ScrubReport};
pub use unlock::{
    UnlockDescriptor, ROOT_UNLOCK_NAME, SYSTEM_VOLUME_KEY, UNLOCK_DEFAULT_ITERATIONS,
    UNLOCK_DESCRIPTOR_LEN, UNLOCK_MAX_ITERATIONS, UNLOCK_MIN_ITERATIONS, UNLOCK_SALT_LEN,
};
pub use xform::{ClusterCache, MAX_CLUSTER_PLAINTEXT};

use allocator::{Allocator, MAX_PENDING_DISCARD};
use allocmap::MapGeometry;
use btree::{NodeTrail, TreeWalk};
use dedupe::{
    chunk_spec, dedupe_key, reverse_ref_spec, ChunkRecord, DedupeCandidate, REVERSE_REF_CAP,
};
use header::{BlockHeader, BlockType, FORMAT_VERSION, HEADER_LEN, HEADER_MAGIC};
use superblock::{slot_block, Superblock, RING_BLOCKS, RING_SLOTS};
use transaction::TxnRoot;

/// Per-driver `DriverHandle` marker returned by [`register`].
const REGISTER_HANDLE_MARKER: u64 = 0x5275_7374_4653_0002;

/// Driver entry point.
///
/// # Errors
///
/// * [`DriverError::PermissionDenied`] if the host did not grant
///   [`CapabilityId::DRV_LOAD`].
///
/// # Capabilities
///
/// Requires [`CapabilityId::DRV_LOAD`].
pub fn register(host: &dyn DriverHost) -> Result<DriverHandle, DriverError> {
    if !host.has_capability(CapabilityId::DRV_LOAD) {
        return Err(DriverError::PermissionDenied);
    }
    DriverHandle::from_raw(REGISTER_HANDLE_MARKER)
}

/// Plant a regular file at `components` (a path of directory names ending in
/// the file name) under `parent`, creating each intermediate directory that
/// does not already exist.
///
/// The single definition of the store-planting helper:
/// the image builder (`tools/mkimage`) lays the signed driver bundles into a
/// `/System` volume's `Drivers/` store with it, and the image-fixture crates
/// reuse the same routine so the test images and the real installation give
/// the autoload scan an identical on-disk shape. `components`
/// is the path *under `parent`* of the bundle's leaf file (for example,
/// relative to the `/System` volume root,
/// `&[b"Drivers", b"bus_mailbox", b"vcmailbox", b"Run"]`). The bytes are the
/// signed `.rxe` bundle exactly as the store scanner reads it back.
///
/// # Errors
///
/// Propagates any [`DriverError`] from the underlying create/write, or
/// [`DriverError::Unsupported`] for an empty `components` path. A short write
/// surfaces as [`DriverError::DeviceFault`] (never a
/// truncated bundle).
pub fn plant_nested_file<B>(
    fs: &mut ARXFS<B>,
    parent: NodeId,
    components: &[&[u8]],
    bytes: &[u8],
) -> Result<(), DriverError>
where
    B: Block,
{
    let (file_name, dirs) = components.split_last().ok_or(DriverError::Unsupported)?;
    let mut node = parent;
    for dir in dirs {
        node = match fs.lookup(node, dir) {
            Ok(existing) => existing,
            Err(_) => fs.create(node, dir, NodeKind::Directory)?,
        };
    }
    fs.create(node, file_name, NodeKind::RegularFile)?;
    let written = fs.write_at(node, file_name, 0, bytes)?;
    if written != bytes.len() {
        return Err(DriverError::DeviceFault);
    }
    Ok(())
}

/// Largest block size the driver stages through its on-stack scratch
/// buffers. No Tier-1 block device exceeds 4096 bytes per block.
const MAX_BLOCK_SIZE: usize = 4096;

/// Widest value any of the driver's B-trees stores beside a key.
///
/// A mutation's node buffer carries one entry of this width past the block, so
/// an insert lays a full node out and then splits it rather than needing a
/// second layout path for the overflowing case. A tree with a wider record has
/// to raise this, which the assertion below is here to force.
const MAX_TREE_VALUE_LEN: usize = INODE_SIZE;
const _: () = assert!(
    INODE_SIZE <= MAX_TREE_VALUE_LEN
        && EXTENT_VALUE_LEN <= MAX_TREE_VALUE_LEN
        && dedupe::CHUNK_VALUE_LEN <= MAX_TREE_VALUE_LEN
        && dedupe::REVERSE_REF_VALUE_LEN <= MAX_TREE_VALUE_LEN
);
/// Smallest block size the format supports.
const MIN_BLOCK_SIZE: usize = 512;

/// Bytes one coalesced data-run device request covers.
///
/// An extent maps a *contiguous* physical run, so a read that spans one asks
/// the device once for the whole run instead of once per block: a storage
/// controller moves this much on a single DMA transfer, so a megabyte-scale
/// read parks the calling task across tens of completion interrupts rather
/// than hundreds. The window also bounds the staging one read allocates — a
/// larger read loops — so many volumes on a small machine stay bounded.
const READ_RUN_BYTES: usize = 64 * 1024;

// A run must cover at least one block of every supported geometry, so a read
// always makes progress, and a whole number of them, so no request is
// misaligned. Held at compile time: the run length needs no runtime clamp.
const _: () =
    assert!(READ_RUN_BYTES >= MAX_BLOCK_SIZE && READ_RUN_BYTES.is_multiple_of(MAX_BLOCK_SIZE));

/// Staging for the coalesced run reads of one serving read
/// ([`ARXFS::read_block_run`]).
///
/// Holds the [`READ_RUN_BYTES`] window when it can be allocated and a single
/// on-stack block when it cannot, so a machine short of memory reads slower
/// rather than failing the read (deterministic OOM, never a panic). The
/// staged bytes are decrypted user content, so they are wiped when the stage
/// is dropped, on every path out of the read.
struct RunStage {
    /// The run window, empty when its allocation was refused.
    window: Vec<u8>,
    /// The fallback single block, used only while `window` is empty.
    block: [u8; MAX_BLOCK_SIZE],
    /// Device blocks one request through this stage covers.
    blocks: usize,
}

impl RunStage {
    /// A stage for a read wanting `blocks` device blocks of `block_size`,
    /// bounded by the run window.
    fn new(block_size: usize, blocks: usize) -> Self {
        let want = blocks.clamp(1, READ_RUN_BYTES / block_size);
        let span = want * block_size;
        let mut window = Vec::new();
        // A fallible reservation, then the zeroing the slice needs: `vec![0;
        // span]` aborts the whole system on allocation failure, where a
        // filesystem read must fall back to the single block instead.
        if window.try_reserve_exact(span).is_ok() {
            window.resize(span, 0);
        }
        let blocks = if window.is_empty() { 1 } else { want };
        Self {
            window,
            block: [0u8; MAX_BLOCK_SIZE],
            blocks,
        }
    }

    /// Device blocks one request through this stage covers: never zero, so a
    /// read always makes progress.
    fn blocks(&self) -> usize {
        self.blocks
    }

    /// The staging bytes: the run window, or the single block it fell back to.
    fn buf(&mut self) -> &mut [u8] {
        if self.window.is_empty() {
            &mut self.block
        } else {
            &mut self.window
        }
    }
}

impl Drop for RunStage {
    fn drop(&mut self) {
        self.window.zeroize();
        self.block.zeroize();
    }
}

/// Fixed on-disk size of one inode record, in bytes.
const INODE_SIZE: usize = 256;
/// Per-slot directory-entry header: the 4-byte inode number followed by the
/// 4-byte name length.
const DIRENT_HEADER: usize = 8;
/// Longest directory-entry name, in bytes. This matches the ext4 limit so a
/// name that is valid on ext4 (`drivers/filesystem/ext4`) is valid here, and
/// vice versa. Names are raw bytes compared exactly, so they are
/// case-sensitive (`docs/src/filesystem/arxfs-spec.md` §13).
const NAME_MAX: usize = 255;
/// Fixed on-disk size of one directory slot, in bytes: the [`DIRENT_HEADER`]
/// plus room for a maximum-length name. A fixed-width slot keeps directory
/// scanning, insertion, and removal O(1) per slot with no in-block
/// compaction — the deliberate speed/simplicity trade the copy-on-write
/// directory block design relies on (`docs/src/filesystem/arxfs-spec.md` §13).
const DIRENT_SIZE: usize = DIRENT_HEADER + NAME_MAX;

/// Maximum number of inline ACL entries stored in an inode.
const ACL_MAX: usize = 8;
const _: () = assert!(ACL_MAX == tairix_abi::driver::filesystem::MAX_ACL_ENTRIES);

/// Inode-table index of the root directory. Index 0 is the reserved "no
/// inode" sentinel, so a zeroed directory slot reads as free.
const ROOT_INO: u32 = 1;

/// `used` marker stored in a live inode's first word.
const INODE_USED: u32 = 0x494E_4F44; // "INOD"

/// What an inode is.
///
/// A closed set decoded from the on-disk `kind` word, so a value the format
/// does not define is rejected rather than coerced onto the nearest match.
/// It is an enum rather than a `bool`-plus-`u32` precisely so that every
/// place which used to ask "is this a directory?" and treat everything else
/// as a regular file has to say what it means for a link too.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum InodeKind {
    /// A directory. Its content blocks are mirrored metadata blocks
    /// ([`BlockType::Directory`]), not single-copy data.
    Dir,
    /// A regular file. Its content blocks are single-copy data records.
    File,
    /// A symbolic link. Its stored target is held in its data blocks exactly
    /// like a regular file's bytes (`docs/src/filesystem/arxfs-spec.md` §20),
    /// so every data-accounting path treats it as data — but it is not
    /// byte-readable, and only [`FilesystemRead::read_link`] surfaces the
    /// target.
    Link,
}

impl InodeKind {
    /// The on-disk `kind` word.
    const fn as_u32(self) -> u32 {
        match self {
            Self::Dir => 1,
            Self::File => 2,
            Self::Link => 3,
        }
    }

    /// Recover a kind from its on-disk word, or `None` for a value this
    /// format does not define.
    const fn from_u32(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Dir),
            2 => Some(Self::File),
            3 => Some(Self::Link),
            _ => None,
        }
    }

    /// The structural [`NodeKind`] the read surface reports.
    const fn node_kind(self) -> NodeKind {
        match self {
            Self::Dir => NodeKind::Directory,
            Self::File => NodeKind::RegularFile,
            Self::Link => NodeKind::Symlink,
        }
    }

    /// Whether this node's content blocks are **mirrored metadata** rather
    /// than single-copy data. Only a directory's are, so allocation
    /// accounting, freeing, and scrub all key on this and a link's target
    /// blocks are accounted as the data records they are.
    const fn content_is_metadata(self) -> bool {
        matches!(self, Self::Dir)
    }
}

// ---------------------------------------------------------------------------
// Little-endian field accessors over a byte slice. Total over an in-bounds
// offset; callers only address fields inside a buffer they sized.
// ---------------------------------------------------------------------------

fn rd_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

fn rd_u64(buf: &[u8], off: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&buf[off..off + 8]);
    u64::from_le_bytes(bytes)
}

fn rd_u128(buf: &[u8], off: usize) -> u128 {
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&buf[off..off + 16]);
    u128::from_le_bytes(bytes)
}

fn wr_u32(buf: &mut [u8], off: usize, value: u32) {
    buf[off..off + 4].copy_from_slice(&value.to_le_bytes());
}

fn wr_u64(buf: &mut [u8], off: usize, value: u64) {
    buf[off..off + 8].copy_from_slice(&value.to_le_bytes());
}

fn wr_u128(buf: &mut [u8], off: usize, value: u128) {
    buf[off..off + 16].copy_from_slice(&value.to_le_bytes());
}

/// Write a [`Time64`] at `off` (12 bytes: 8-byte seconds + 4-byte nanos).
fn wr_time(buf: &mut [u8], off: usize, value: Time64) {
    buf[off..off + Time64::WIRE_LEN].copy_from_slice(&value.to_le_bytes());
}

/// Read a [`Time64`] at `off`. A non-canonical encoding is corruption.
fn rd_time(buf: &[u8], off: usize) -> Result<Time64, DriverError> {
    Time64::from_bytes(&buf[off..off + Time64::WIRE_LEN]).map_err(|_| DriverError::DeviceFault)
}

/// Narrow a `u64` to a `usize` without an `as` cast.
fn as_usize(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

/// Narrow a `usize` to a `u32` without an `as` cast.
fn as_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// Cheap, bounded, first-party all-zero scan over a write buffer
/// (`docs/src/filesystem/arxfs-spec.md` §19; `plans/SPARSE.md` §16). It
/// allocates nothing and never calls the compressor: an all-zero logical
/// record becomes a metadata-only sparse hole rather than a physical data
/// record.
fn is_all_zero(buf: &[u8]) -> bool {
    buf.iter().all(|&byte| byte == 0)
}

/// Extent-value flag bit marking a compressed cluster extent.
const EXTENT_FLAG_COMPRESSED: u32 = 1;

/// One decoded extent-tree record: a run of logical blocks and the physical
/// blocks that store it (`docs/src/filesystem/arxfs-spec.md` §6, §10).
///
/// A **raw** extent maps each logical block 1:1 onto a physical block. A
/// **compressed** extent covers exactly one aligned compression cluster
/// ([`COMPRESS_CLUSTER_BLOCKS`] logical blocks) whose plaintext is stored as
/// one compressed frame in `stored < len` contiguous physical blocks, so the
/// saved blocks are real free space.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Extent {
    /// First physical block of the stored run.
    phys: u64,
    /// Logical blocks the extent covers.
    len: u64,
    /// Physical blocks backing the run: `len` for a raw extent, fewer for a
    /// compressed cluster.
    stored: u64,
    /// Whether the stored run holds a single compressed cluster frame.
    compressed: bool,
}

impl Extent {
    /// A raw 1:1 run of `len` logical blocks at `phys`.
    fn raw(phys: u64, len: u64) -> Self {
        Self {
            phys,
            len,
            stored: len,
            compressed: false,
        }
    }

    /// A compressed cluster of `len` logical blocks stored in `stored`
    /// contiguous physical blocks at `phys`.
    fn cluster(phys: u64, len: u64, stored: u64) -> Self {
        Self {
            phys,
            len,
            stored,
            compressed: true,
        }
    }

    /// Encode this extent as an on-disk value. A raw extent stores a zero
    /// physical length (its physical length *is* `len`, and a raw run may
    /// exceed `u32::MAX` blocks); a compressed extent stores its bounded
    /// physical length explicitly.
    fn encode(&self) -> [u8; EXTENT_VALUE_LEN] {
        let mut value = [0u8; EXTENT_VALUE_LEN];
        value[0..8].copy_from_slice(&self.phys.to_le_bytes());
        value[8..16].copy_from_slice(&self.len.to_le_bytes());
        if self.compressed {
            value[16..20].copy_from_slice(&as_u32(as_usize(self.stored)).to_le_bytes());
            value[20..24].copy_from_slice(&EXTENT_FLAG_COMPRESSED.to_le_bytes());
        }
        value
    }

    /// Decode an on-disk extent value, rejecting any shape the format does
    /// not define (fail closed): unknown flags, a raw extent carrying a
    /// stored length, or a compressed extent whose geometry is not a bounded
    /// cluster (`0 < stored < len <= COMPRESS_CLUSTER_BLOCKS`).
    fn decode(value: &[u8]) -> Result<Self, DriverError> {
        let phys = rd_u64(value, 0);
        let len = rd_u64(value, 8);
        let stored = u64::from(rd_u32(value, 16));
        let flags = rd_u32(value, 20);
        match flags {
            0 if stored == 0 => Ok(Self::raw(phys, len)),
            EXTENT_FLAG_COMPRESSED
                if stored > 0 && stored < len && len <= COMPRESS_CLUSTER_BLOCKS =>
            {
                Ok(Self::cluster(phys, len, stored))
            }
            _ => Err(DriverError::DeviceFault),
        }
    }
}

// Inode field byte offsets within a 256-byte record.
const I_USED: usize = 0;
const I_KIND: usize = 4;
const I_MODE: usize = 8;
const I_UID: usize = 12;
const I_GID: usize = 16;
const I_NLINK: usize = 20;
const I_REQCAP: usize = 24;
const I_ACLCOUNT: usize = 28;
const I_SIZE: usize = 32;
// The tracked timestamps, each a 12-byte Time64: created (40..52),
// modified (52..64), changed (76..88). Bytes 64..76 are a reserved atime
// slot: ARXFS does not track access time, always writes it zero, and never
// reads it, so `NodeTimes::accessed` is always `Time64::UNIX_EPOCH`.
const I_CREATED: usize = 40;
const I_MODIFIED: usize = 52;
const I_CHANGED: usize = 76;
const I_ACL_BASE: usize = 88;
const I_ACL_STRIDE: usize = 8;
/// Physical block of this inode's per-file extent-tree root, or `0` when the
/// file has no mapped blocks (`btree` module).
const I_EXTENT_ROOT: usize = 152;
/// Physical block of this inode's extended-attribute set ([`BlockType::Attr`]),
/// or `0` when the inode carries no attributes (`docs/src/filesystem/arxfs-spec.md` §21).
const I_ATTR_ROOT: usize = 160;

/// In-memory image of one on-disk inode.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Inode {
    kind: InodeKind,
    sec: Security,
    nlink: u32,
    size: u64,
    times: NodeTimes,
    /// Physical block of this file's copy-on-write extent-tree root, `0` when
    /// the file maps no blocks yet.
    extent_root: u64,
    /// Physical block of this inode's extended-attribute set, `0` when it
    /// carries no attributes. Stored as an encrypted, mirrored copy-on-write
    /// metadata block ([`BlockType::Attr`]).
    attr_root: u64,
}

impl Inode {
    fn empty(kind: InodeKind, sec: Security, now: Time64) -> Self {
        Self {
            kind,
            sec,
            nlink: 1,
            size: 0,
            times: NodeTimes {
                created: now,
                modified: now,
                // ARXFS does not track access time.
                accessed: Time64::UNIX_EPOCH,
                changed: now,
            },
            extent_root: 0,
            attr_root: 0,
        }
    }

    fn is_dir(&self) -> bool {
        self.kind == InodeKind::Dir
    }

    /// Decode the inode record at `buf[..INODE_SIZE]`, returning `None` for
    /// a free (zeroed) slot.
    fn decode(buf: &[u8]) -> Result<Option<Self>, DriverError> {
        if rd_u32(buf, I_USED) != INODE_USED {
            return Ok(None);
        }
        let kind = InodeKind::from_u32(rd_u32(buf, I_KIND)).ok_or(DriverError::DeviceFault)?;
        let required_cap = match rd_u32(buf, I_REQCAP) {
            0 => None,
            raw => {
                let id = u16::try_from(raw).map_err(|_| DriverError::DeviceFault)?;
                Some(CapabilityId::from_raw(id).map_err(|_| DriverError::DeviceFault)?)
            }
        };
        let mut sec = Security::new(rd_u32(buf, I_MODE), rd_u32(buf, I_UID), rd_u32(buf, I_GID));
        sec.required_cap = required_cap;
        let acl_count = rd_u32(buf, I_ACLCOUNT) as usize;
        if acl_count > ACL_MAX {
            return Err(DriverError::DeviceFault);
        }
        for i in 0..acl_count {
            let base = I_ACL_BASE + i * I_ACL_STRIDE;
            let kind_byte = buf[base];
            let perms = buf[base + 1];
            let id = rd_u32(buf, base + 4);
            let subject = match kind_byte {
                1 => AclSubject::User(id),
                2 => AclSubject::Group(id),
                _ => return Err(DriverError::DeviceFault),
            };
            sec.push_acl(AclEntry { subject, perms })?;
        }
        let times = NodeTimes {
            created: rd_time(buf, I_CREATED)?,
            modified: rd_time(buf, I_MODIFIED)?,
            // ARXFS does not track access time; the on-disk slot is reserved
            // and ignored on read.
            accessed: Time64::UNIX_EPOCH,
            changed: rd_time(buf, I_CHANGED)?,
        };
        Ok(Some(Self {
            kind,
            sec,
            nlink: rd_u32(buf, I_NLINK),
            size: rd_u64(buf, I_SIZE),
            times,
            extent_root: rd_u64(buf, I_EXTENT_ROOT),
            attr_root: rd_u64(buf, I_ATTR_ROOT),
        }))
    }

    fn encode(&self, buf: &mut [u8]) {
        for byte in buf.iter_mut().take(INODE_SIZE) {
            *byte = 0;
        }
        wr_u32(buf, I_USED, INODE_USED);
        wr_u32(buf, I_KIND, self.kind.as_u32());
        wr_u32(buf, I_MODE, self.sec.mode);
        wr_u32(buf, I_UID, self.sec.uid);
        wr_u32(buf, I_GID, self.sec.gid);
        wr_u32(buf, I_NLINK, self.nlink);
        wr_u32(
            buf,
            I_REQCAP,
            self.sec.required_cap.map_or(0, |c| u32::from(c.as_u16())),
        );
        let acl = self.sec.acl();
        wr_u32(buf, I_ACLCOUNT, as_u32(acl.len()));
        wr_u64(buf, I_SIZE, self.size);
        wr_time(buf, I_CREATED, self.times.created);
        wr_time(buf, I_MODIFIED, self.times.modified);
        // Bytes 64..76 are the reserved atime slot; left zero by the buffer
        // clear above (ARXFS does not track access time).
        wr_time(buf, I_CHANGED, self.times.changed);
        for (i, entry) in acl.iter().enumerate() {
            let base = I_ACL_BASE + i * I_ACL_STRIDE;
            let (kind_byte, id) = match entry.subject {
                AclSubject::User(id) => (1u8, id),
                AclSubject::Group(id) => (2u8, id),
            };
            buf[base] = kind_byte;
            buf[base + 1] = entry.perms;
            wr_u32(buf, base + 4, id);
        }
        wr_u64(buf, I_EXTENT_ROOT, self.extent_root);
        wr_u64(buf, I_ATTR_ROOT, self.attr_root);
    }
}

/// Write directory slot `slot` of `buf` with entry `(ino, name)`.
fn put_dirent(buf: &mut [u8], slot: usize, ino: u32, name: &[u8]) {
    let base = HEADER_LEN + slot * DIRENT_SIZE;
    for byte in &mut buf[base..base + DIRENT_SIZE] {
        *byte = 0;
    }
    wr_u32(buf, base, ino);
    wr_u32(buf, base + 4, as_u32(name.len()));
    buf[base + 8..base + 8 + name.len()].copy_from_slice(name);
}

/// A best-effort wall clock; the host overrides it with [`ARXFS::with_clock`].
fn epoch_clock() -> Time64 {
    Time64::UNIX_EPOCH
}

/// A mounted copy-on-write arxfs volume.
///
/// The on-disk state is the committed transaction root selected from the
/// superblock ring. That root names the **inode tree** (a copy-on-write
/// B-tree keyed by inode number; `btree` module) and the next free inode
/// number. Each file inode in turn names its own **extent tree** mapping a
/// logical block offset to a physical run. The in-memory free-block bitmap is
/// rebuilt by walking those trees at [`ARXFS::open`] and kept in step as
/// transactions commit. A volume is created with [`ARXFS::format`] and
/// reopened with [`ARXFS::open`].
// `ARXFS` is the filesystem's product name and is spelled in full capitals
// everywhere; the mixed-case `Arxfs` the acronym lint would otherwise require
// is not an accepted spelling of the name.
#[allow(clippy::upper_case_acronyms)]
pub struct ARXFS<B: Block> {
    block: B,
    fs_uuid: u128,
    mac_key: MacKey,
    /// AEAD key encrypting directory-entry names at rest (`crypto` module).
    filename_key: AeadKey,
    /// AEAD key encrypting file data at rest (`crypto` module).
    content_key: AeadKey,
    /// Encoded plaintext crypto discovery header (the wrapped master key and
    /// its salt) written into every superblock-ring slot at commit.
    crypto_header: [u8; CRYPTO_HEADER_LEN],
    block_size: usize,
    total_blocks: u64,
    inode_hint: u32,
    generation: u64,
    ring_pos: u64,
    inode_tree_root: u64,
    next_ino: u64,
    /// Root of the authoritative chunk/refcount tree, `0` until a chunk is
    /// shared (`dedupe` module, `docs/src/filesystem/arxfs-spec.md` §4, §9).
    chunk_tree_root: u64,
    /// Root of the authoritative reverse-reference tree, `0` until a chunk
    /// records referrers (`dedupe` module).
    reverse_ref_tree_root: u64,
    /// Physical block of the scrub-progress record, `0` when no online scrub
    /// is mid-pass. Rebuildable metadata reached from the transaction root
    /// (`scrub` module, `docs/src/filesystem/arxfs-spec.md` §4, §12).
    scrub_progress_root: u64,
    /// Physical block of the device-health baseline record, `0` until a
    /// baseline is stored (`format` stores the first one). It holds the last
    /// clean device-health snapshot and the volume's accumulated
    /// filesystem-observed fault counters, reached from the transaction root
    /// (`health` module, `docs/src/filesystem/arxfs-spec.md` §4, §11).
    health_baseline_root: u64,
    /// The volume's deduplication domain (`crypto` module). Dedupe never
    /// crosses it.
    dedupe_domain: u64,
    root_phys: u64,
    /// Free blocks in the committed volume, read straight from the transaction
    /// root at mount. A read-only handle holds no allocator, so this is the
    /// only free-space figure it has — and the only one it needs, since it can
    /// never allocate.
    free_count: u64,
    /// Everything only a writable mount can use: the on-disk allocation map
    /// and its cursors, the per-transaction bookkeeping, the pending-discard
    /// queue, and the dedupe index (`allocator` module). `None` on a read-only
    /// handle, which is what makes "a read-only mount never allocates" a
    /// property of the type rather than a convention — and what lets it mount
    /// without reading or rebuilding any allocation state at all.
    alloc: Option<Allocator>,
    /// First block of the allocation-map region, as the committed transaction
    /// root records it. Held here rather than read back from the allocator so
    /// a grow can commit the region's new home before the map moves into it.
    alloc_map_start: u64,
    /// Incompatible on-disk features the committed volume declares
    /// (`superblock` module). A volume gains a bit the first time it uses the
    /// structure that bit names, so a volume that never uses one stays
    /// readable by a build that does not know it.
    incompat: u64,
    saved_inode_tree_root: u64,
    saved_next_ino: u64,
    saved_chunk_tree_root: u64,
    saved_reverse_ref_tree_root: u64,
    saved_scrub_progress_root: u64,
    saved_health_baseline_root: u64,
    saved_incompat: u64,
    clock: fn() -> Time64,
    /// When `true`, the repair-on-read paths (`read_meta`, `read_sb_slot`,
    /// `read_txn_root`) skip writing a good companion back over a bad primary,
    /// so the handle never mutates the backing device. The offline
    /// [`ARXFS::rescue`] sets it: rescue is read-only on the damaged volume by
    /// default (`docs/src/filesystem/arxfs-spec.md` §12).
    read_only: bool,
    /// Host-injected retention of decompressed cluster plaintext
    /// ([`ClusterCache`], `plans/SMARTRAM.md` SMART3), or `None` for a
    /// volume that serves every read through the full transform
    /// pipeline. Installed by [`ARXFS::with_cluster_cache`];
    /// invalidated by [`ARXFS::free_block`] and purged by
    /// [`ARXFS::rollback`], so it can never serve a stale cluster.
    cluster_cache: Option<Box<dyn ClusterCache>>,
    /// The mount's B-tree mutation scratch (`btree` module), lent to one
    /// insert or remove at a time. Held here so a steady-state mutation
    /// allocates nothing and no node buffer reaches the stack; `None` until
    /// the first mutation, so a read-only handle never pays for one.
    tree_edit: Option<btree::TreeEdit>,
}

/// A bounded cursor over one directory's entries.
///
/// Holds one directory block, so walking a directory of any size costs the
/// block size — the same contract [`btree::TreeWalk`] gives the metadata
/// trees. The position is a single slot index, so a caller may stop, remember
/// it, and resume.
pub(crate) struct DirScan {
    buf: Vec<u8>,
    /// Slot to examine next, as `block * dirents_per_block + slot`.
    next: u64,
    /// The directory block currently in `buf`.
    loaded: Option<u64>,
    /// Where the last yielded entry's name sits in `buf`, and its length.
    entry: Option<(usize, usize)>,
}

impl DirScan {
    /// A scan positioned before the directory's first entry.
    ///
    /// # Errors
    ///
    /// [`DriverError::NoSpace`] when the one block-sized buffer cannot be
    /// allocated; the scan allocates nothing thereafter.
    pub(crate) fn new(block_size: usize) -> Result<Self, DriverError> {
        let mut buf = Vec::new();
        buf.try_reserve_exact(block_size)
            .map_err(|_| DriverError::NoSpace)?;
        buf.resize(block_size, 0);
        Ok(Self {
            buf,
            next: 0,
            loaded: None,
            entry: None,
        })
    }

    /// Position the scan at slot `position`, so the next step yields the first
    /// occupied entry at or after it.
    ///
    /// The resident block is dropped: a scan is reused across *directories*,
    /// and a block index means nothing without the directory it came from.
    pub(crate) fn seek(&mut self, position: u64) {
        self.next = position;
        self.loaded = None;
        self.entry = None;
    }

    /// The name of the entry the last step yielded (empty before the first).
    pub(crate) fn name(&self) -> &[u8] {
        match self.entry {
            Some((at, len)) => &self.buf[at..at + len],
            None => &[],
        }
    }

    /// Whether the last entry is a directory's own `.` or its parent `..`,
    /// which name a directory without being a reference to it.
    pub(crate) fn is_dot(&self) -> bool {
        let name = self.name();
        name == b"." || name == b".."
    }
}

/// Value width of one extent record: physical start block, logical run
/// length, stored physical length, and flags ([`Extent`]).
const EXTENT_VALUE_LEN: usize = 24;

/// Logical blocks per compression cluster: the aligned unit the write path
/// compresses as one frame (`docs/src/filesystem/arxfs-spec.md` §10). A
/// compressed extent always covers exactly one whole cluster, so reading any
/// byte decompresses at most this many blocks — a constant bound that keeps
/// random access O(log n) regardless of file size.
const COMPRESS_CLUSTER_BLOCKS: u64 = 16;

/// Free blocks held back from file *data* allocation so a shrinking
/// transaction (delete, truncate) can always copy-on-write its metadata and
/// commit a new root even on an otherwise-full volume. Metadata allocation may
/// draw on this reserve; data allocation stops above it (`alloc_block`).
const METADATA_RESERVE: u64 = 16;

/// The inode tree's record shape: a 256-byte inode keyed by its number.
fn inode_spec() -> btree::TreeSpec {
    btree::TreeSpec {
        value_len: INODE_SIZE,
        owner: u64::MAX,
    }
}

/// A file's extent-tree record shape: an [`Extent`] keyed by its starting
/// logical block, owned by inode `ino`.
fn extent_spec(ino: u32) -> btree::TreeSpec {
    btree::TreeSpec {
        value_len: EXTENT_VALUE_LEN,
        owner: u64::from(ino),
    }
}

impl<B: Block> ARXFS<B> {
    /// Directory slots per block. The block's tail holds the per-block crypto
    /// trailer (nonce + tag) that encrypts the entry names at rest, so the
    /// slots occupy `[HEADER_LEN, block_size - CRYPTO_TRAILER)`.
    fn dirents_per_block(&self) -> usize {
        (self.block_size - HEADER_LEN - CRYPTO_TRAILER) / DIRENT_SIZE
    }

    /// First byte of the per-block crypto trailer: the offset where a data or
    /// directory block's encrypted region ends and its nonce + tag begin.
    fn crypto_trailer_offset(&self) -> usize {
        self.block_size - CRYPTO_TRAILER
    }

    /// Usable file-content bytes per data block: the block minus its crypto
    /// trailer (nonce + tag), its compression descriptor, and its
    /// data-integrity trailer (logical hash + physical checksum, `integrity`
    /// module). File offsets map through this capacity, not the raw device
    /// block size. A logical block always maps this many plaintext bytes even
    /// when compression stores fewer bytes at rest
    /// (`docs/src/filesystem/arxfs-spec.md` §10).
    fn data_capacity(&self) -> u64 {
        (self.block_size - CRYPTO_TRAILER - COMPRESSION_DESCRIPTOR_LEN - DATA_INTEGRITY_TRAILER)
            as u64
    }

    /// Replace the wall clock used to stamp the timestamps. Used by tests
    /// to inject a deterministic clock; the default returns the Unix epoch.
    #[must_use]
    pub fn with_clock(mut self, clock: fn() -> Time64) -> Self {
        self.clock = clock;
        self
    }

    /// Consume the filesystem and return the backing block device.
    pub fn into_block(self) -> B {
        self.block
    }

    /// Mutable access to the backing block device, for tests that need to
    /// model the device changing underneath a live mount (e.g. enlarging it
    /// to exercise [`Self::grow`]).
    #[cfg(test)]
    pub(crate) fn block_mut(&mut self) -> &mut B {
        &mut self.block
    }

    /// Create `dst_name` in directory `dir` as a **reflink** of the existing
    /// regular file `src_name`: a copy-on-write clone that shares every data
    /// block with the source until either side is written
    /// (`docs/src/filesystem/arxfs-spec.md` §9). The two files read back
    /// identically, share their physical chunks (refcount ≥ 2), and diverge
    /// only block-by-block as each is overwritten.
    ///
    /// This is an inherent driver operation, not part of a frozen
    /// `Filesystem*` ABI trait, so it does not widen a shipped interface.
    ///
    /// # Errors
    ///
    /// * [`DriverError::NotFound`] if `src_name` does not exist.
    /// * [`DriverError::Busy`] if `dst_name` already exists.
    /// * [`DriverError::Unsupported`] if `dir` is not a directory or the
    ///   source is not a regular file.
    /// * [`DriverError::LengthOutOfRange`] if `dst_name` is empty or too long.
    /// * [`DriverError::PermissionDenied`] if the handle is read-only — the
    ///   refusal is returned **before** any state is touched, so a read-only
    ///   `/System` mount never dirties the device.
    /// * [`DriverError::DeviceFault`] on an unrecoverable block or metadata
    ///   failure (fail-closed).
    pub fn reflink(
        &mut self,
        dir: NodeId,
        src_name: &[u8],
        dst_name: &[u8],
    ) -> Result<NodeId, DriverError> {
        self.deny_if_read_only()?;
        self.begin();
        let result = self.reflink_inner(dir, src_name, dst_name);
        if result.is_err() {
            self.rollback();
        }
        result
    }

    /// Install a host-provided transform cache retaining decompressed
    /// cluster plaintext between reads (`plans/SMARTRAM.md` SMART3).
    ///
    /// The cache spares the serving read path a repeated
    /// verify/decrypt/decompress of a cluster it already produced; every
    /// classification, budget, pressure, and zeroisation decision lives
    /// in the injected implementation ([`ClusterCache`]). Without one,
    /// the volume behaves exactly as before. Install at mount time,
    /// before the volume serves reads.
    #[must_use]
    pub fn with_cluster_cache(mut self, cache: Box<dyn ClusterCache>) -> Self {
        self.cluster_cache = Some(cache);
        self
    }

    /// Install a freshly derived or unwrapped key set as the volume's working
    /// keys (`crypto` module).
    fn apply_keys(&mut self, keys: &VolumeKeys) {
        self.mac_key = keys.mac_key;
        self.filename_key = keys.filename_key;
        self.content_key = keys.content_key;
        self.dedupe_domain = keys.dedupe_domain;
    }

    /// Read the raw block at `phys` into the first `block_size` bytes of `buf`.
    fn read_block(&mut self, phys: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        self.read_block_run(phys, 1, buf)
    }

    /// Read the contiguous run of `blocks` raw blocks at `phys` into the first
    /// `blocks * block_size` bytes of `buf`, in **one** device request.
    ///
    /// The one device-read primitive: a caller that knows its blocks are
    /// contiguous (an extent's stored run) pays one round-trip for the run
    /// instead of one per block. A `buf` too short for the run is refused
    /// rather than truncated.
    fn read_block_run(
        &mut self,
        phys: u64,
        blocks: usize,
        buf: &mut [u8],
    ) -> Result<(), DriverError> {
        let span = blocks
            .checked_mul(self.block_size)
            .ok_or(DriverError::LengthOutOfRange)?;
        let run = buf.get_mut(..span).ok_or(DriverError::LengthOutOfRange)?;
        self.block.read_blocks(phys, run)
    }

    /// Write the first `block_size` bytes of `buf` to the block at `phys`.
    fn write_block(&mut self, phys: u64, buf: &[u8]) -> Result<(), DriverError> {
        let bs = self.block_size;
        self.block.write_blocks(phys, &buf[..bs])
    }

    /// Restore the bad physical copy of a mirrored metadata block at `phys`
    /// from `good`, the good copy's still-sealed bytes, reporting whether the
    /// copy was rewritten.
    ///
    /// The one place a mirror copy-repair happens, so the read-only rule
    /// cannot be observed at three sites and forgotten at a fourth. A
    /// read-only handle rewrites nothing and reports `false`: a volume held
    /// read-only because its non-mutation could not be proven is exactly where
    /// a well-meant repair is itself the damage. The good copy served the read
    /// either way, so a caller reports the finding rather than losing it
    /// (`docs/src/filesystem/arxfs-spec.md` §8, §12).
    fn repair_meta_copy(&mut self, phys: u64, good: &[u8]) -> Result<bool, DriverError> {
        if self.read_only {
            return Ok(false);
        }
        self.write_block(phys, good)?;
        Ok(true)
    }

    /// The companion mirror of metadata block `phys`: its adjacent block at
    /// `phys + 1`. Every metadata block is stored twice — at `phys` and at
    /// `companion(phys)` — so a stale, torn, or bit-rotted copy can be
    /// repaired from the other (`docs/src/filesystem/arxfs-spec.md` §5, §8).
    /// One rule covers superblock-ring slots, transaction roots, B-tree nodes,
    /// and directory blocks, so there is a single redundancy mechanism.
    const fn companion(phys: u64) -> u64 {
        phys + 1
    }

    /// Write a sealed metadata block to both its primary location `phys` and
    /// its companion mirror, so the two physical copies are identical. The
    /// header in `buf` names `phys` as its physical address; the companion
    /// stores the same bytes and is verified against `phys` on read.
    fn write_meta(&mut self, phys: u64, buf: &[u8]) -> Result<(), DriverError> {
        self.write_block(phys, buf)?;
        self.write_block(Self::companion(phys), buf)
    }

    /// Read and validate the metadata block at `phys`, confirming it is the
    /// `expect_type` block at that address.
    ///
    /// Reads the primary copy first; if it fails to authenticate (stale,
    /// misdirected, torn, bit-rotted, or wrong-key), it falls back to the
    /// companion mirror and, when that copy is good, **repairs** the primary
    /// from it ([`Self::repair_meta_copy`], which a read-only handle declines —
    /// `docs/src/filesystem/arxfs-spec.md` §8, try redundant copies, repair bad
    /// from good). On success `buf` holds the good block's bytes. If neither
    /// copy authenticates the read fails closed with
    /// [`DriverError::DeviceFault`].
    fn read_meta(
        &mut self,
        phys: u64,
        expect_type: BlockType,
        buf: &mut [u8],
    ) -> Result<BlockHeader, DriverError> {
        let bs = self.block_size;
        // A copy that cannot be read is as absent as one that fails to
        // authenticate, so both fall through to the companion.
        if self.read_block(phys, buf).is_ok() {
            if let Ok(header) = BlockHeader::decode_verify(
                &buf[..bs],
                expect_type,
                self.fs_uuid,
                phys,
                &self.mac_key,
            ) {
                self.decrypt_meta_payload(expect_type, buf, phys)?;
                return Ok(header);
            }
        }
        // Primary failed: fall back to the companion mirror, validating it
        // against the *primary's* identity (both copies carry that address).
        self.read_block(Self::companion(phys), buf)?;
        let header =
            BlockHeader::decode_verify(&buf[..bs], expect_type, self.fs_uuid, phys, &self.mac_key)?;
        // The companion is good: repair the primary from its still-encrypted
        // bytes, then decrypt the caller's copy.
        self.repair_meta_copy(phys, buf)?;
        self.decrypt_meta_payload(expect_type, buf, phys)?;
        Ok(header)
    }

    /// After a metadata block authenticates, decrypt its at-rest-encrypted
    /// payload in place for the caller. Only directory blocks carry an
    /// encrypted payload (the entry names); every other metadata block is
    /// authenticated-only and returned unchanged. The block authenticated
    /// before this point, so decryption cannot yield mis-decrypted bytes
    /// (`docs/src/filesystem/arxfs-spec.md` §6 read path).
    fn decrypt_meta_payload(
        &self,
        block_type: BlockType,
        buf: &mut [u8],
        phys: u64,
    ) -> Result<(), DriverError> {
        if !matches!(block_type, BlockType::Directory | BlockType::Attr) {
            return Ok(());
        }
        let off = self.crypto_trailer_offset();
        let (region, trailer) = buf[HEADER_LEN..self.block_size].split_at_mut(off - HEADER_LEN);
        decrypt_region(&self.filename_key, region, trailer, phys)
            .map_err(|_| DriverError::DeviceFault)
    }

    /// Whether `phys` was allocated by the current, not-yet-committed
    /// transaction and may therefore be overwritten in place.
    fn is_txn_private(&self, phys: u64) -> bool {
        self.allocator()
            .is_ok_and(|alloc| alloc.txn_private.contains(&phys))
    }

    /// Free a block. A block allocated **by this transaction** (still private,
    /// never published in any committed root) is reclaimed *immediately*, so a
    /// transaction that repeatedly copies-on-writes the same metadata — e.g. an
    /// extent tree that splits and re-merges as a large write streams in — does
    /// not pin every superseded copy until commit. Reusing such a block within
    /// the same transaction is safe for crash consistency: nothing committed
    /// ever referenced it. A block inherited from an earlier committed root is
    /// instead deferred and reclaimed only at [`Self::finish_txn`], so a block
    /// reachable from the last committed root is never reused mid-flight
    /// (`docs/src/filesystem/arxfs-spec.md` §2).
    fn free_block(&mut self, phys: u64) {
        if phys == 0 {
            return;
        }
        // The block's bytes are about to leave the committed tree: any
        // cluster plaintext derived from a run covering it must go now,
        // before the block can be reallocated and rewritten.
        if let Some(cache) = self.cluster_cache.as_mut() {
            cache.invalidate(phys);
        }
        if self.is_txn_private(phys) {
            self.mark_free(phys);
            if let Ok(alloc) = self.allocator_mut() {
                alloc.txn_private.remove(&phys);
            }
        } else if let Ok(alloc) = self.allocator_mut() {
            alloc.txn_freed.insert(phys);
        }
    }

    /// Defer-free a metadata block and its companion mirror together (they are
    /// always allocated and freed as a unit).
    fn free_meta(&mut self, phys: u64) {
        if phys != 0 {
            self.free_block(phys);
            self.free_block(Self::companion(phys));
        }
    }

    /// Copy-on-write a metadata block whose payload is already laid out in
    /// `buf[HEADER_LEN..]`. Reuses `old_phys` in place when it is private to
    /// this transaction, otherwise allocates a fresh block and defer-frees the
    /// old one. Returns the block's (possibly new) physical address.
    fn cow_meta(
        &mut self,
        old_phys: u64,
        buf: &mut [u8],
        block_type: BlockType,
        owner: u64,
        logical: u64,
    ) -> Result<u64, DriverError> {
        let new_phys = if old_phys != 0 && self.is_txn_private(old_phys) {
            old_phys
        } else {
            let p = self.alloc_block(true)?;
            if old_phys != 0 {
                self.free_meta(old_phys);
            }
            p
        };
        let payload_len = as_u32(self.block_size - HEADER_LEN);
        let next_gen = self.generation.wrapping_add(1);
        // A directory block's entry names and an attribute block's keys and
        // values are encrypted at rest under the metadata (filename) key
        // before the block is authenticated, so the keyed authenticator seals
        // the ciphertext (encrypt-then-MAC; the read path authenticates then
        // decrypts — `docs/src/filesystem/arxfs-spec.md` §6, §7, §21). Other
        // metadata blocks are authenticated-only.
        if matches!(block_type, BlockType::Directory | BlockType::Attr) {
            let off = self.crypto_trailer_offset();
            let (region, trailer) = buf[HEADER_LEN..self.block_size].split_at_mut(off - HEADER_LEN);
            encrypt_region(&self.filename_key, region, trailer, new_phys, next_gen)
                .map_err(|_| DriverError::DeviceFault)?;
        }
        let header = BlockHeader {
            block_type,
            fs_uuid: self.fs_uuid,
            owner,
            generation: next_gen,
            logical_addr: logical,
            physical_addr: new_phys,
            payload_len,
        };
        let bs = self.block_size;
        header.seal(&mut buf[..bs], &self.mac_key)?;
        self.write_meta(new_phys, buf)?;
        Ok(new_phys)
    }

    /// Read inode `ino` from the copy-on-write inode tree.
    fn read_inode(&mut self, ino: u32) -> Result<Inode, DriverError> {
        let spec = inode_spec();
        let value = self
            .btree_get(self.inode_tree_root, u64::from(ino), spec)?
            .ok_or(DriverError::NotFound)?;
        Inode::decode(&value)?.ok_or(DriverError::NotFound)
    }

    /// Insert or replace inode `ino` in the inode tree (copy-on-write).
    fn write_inode(&mut self, ino: u32, inode: &Inode) -> Result<(), DriverError> {
        let spec = inode_spec();
        let mut value = [0u8; INODE_SIZE];
        inode.encode(&mut value);
        self.inode_tree_root =
            self.btree_insert(self.inode_tree_root, u64::from(ino), &value, spec)?;
        Ok(())
    }

    /// Hand out the next inode number, store `inode` under it, and return it.
    fn alloc_inode(&mut self, inode: &Inode) -> Result<u32, DriverError> {
        let ino = u32::try_from(self.next_ino).map_err(|_| DriverError::NoSpace)?;
        self.next_ino = self.next_ino.wrapping_add(1);
        self.write_inode(ino, inode)?;
        Ok(ino)
    }

    /// Remove inode `ino` from the inode tree (copy-on-write).
    fn free_inode(&mut self, ino: u32) -> Result<(), DriverError> {
        let spec = inode_spec();
        self.inode_tree_root = self.btree_remove(self.inode_tree_root, u64::from(ino), spec)?;
        Ok(())
    }

    /// Reset the per-transaction bookkeeping at the start of an operation and
    /// snapshot the published tree state so a failed operation can roll back.
    fn begin(&mut self) {
        if let Ok(alloc) = self.allocator_mut() {
            alloc.txn_allocated.clear();
            alloc.txn_freed.clear();
        }
        self.saved_inode_tree_root = self.inode_tree_root;
        self.saved_next_ino = self.next_ino;
        self.saved_chunk_tree_root = self.chunk_tree_root;
        self.saved_reverse_ref_tree_root = self.reverse_ref_tree_root;
        self.saved_scrub_progress_root = self.scrub_progress_root;
        self.saved_health_baseline_root = self.health_baseline_root;
        self.saved_incompat = self.incompat;
    }

    /// Discard an operation that failed before committing: restore the inode
    /// tree root and inode counter and free this transaction's allocations.
    /// Nothing was published, so the committed on-disk root is untouched.
    fn rollback(&mut self) {
        self.inode_tree_root = self.saved_inode_tree_root;
        self.next_ino = self.saved_next_ino;
        self.chunk_tree_root = self.saved_chunk_tree_root;
        self.reverse_ref_tree_root = self.saved_reverse_ref_tree_root;
        self.scrub_progress_root = self.saved_scrub_progress_root;
        self.health_baseline_root = self.saved_health_baseline_root;
        self.incompat = self.saved_incompat;
        let allocated = match self.allocator_mut() {
            Ok(alloc) => {
                alloc.txn_freed.clear();
                core::mem::take(&mut alloc.txn_allocated)
            }
            Err(_) => Vec::new(),
        };
        for block in allocated {
            // A block this transaction allocated and then released again is
            // already back in the pool; only one that is still private has an
            // allocation left to undo.
            if self
                .allocator_mut()
                .is_ok_and(|alloc| alloc.txn_private.remove(&block))
            {
                self.mark_free(block);
            }
        }
        // The freed allocations bypassed `free_block`, so no per-block
        // invalidation ran: drop everything rather than risk a stale
        // cluster over a recycled run (fail closed).
        if let Some(cache) = self.cluster_cache.as_mut() {
            cache.purge();
        }
    }

    /// Apply a committed transaction's deferred frees and clear the private
    /// markers, making superseded blocks reusable by the next transaction.
    fn finish_txn(&mut self) {
        let Ok(alloc) = self.allocator_mut() else {
            return;
        };
        // Every transaction-private block came from this transaction's
        // allocations, so clearing both sets releases exactly the same
        // markers the pair recorded.
        alloc.txn_allocated.clear();
        alloc.txn_private.clear();
        let freed = core::mem::take(&mut alloc.txn_freed);
        for block in freed {
            self.mark_free(block);
            self.enqueue_discard(block);
        }
    }

    /// Queue a now-free block for a later device discard ([`Self::trim`]).
    ///
    /// The queue is transient, rebuildable state: it only ever holds
    /// blocks already marked free, [`Self::trim`] re-checks each is still free
    /// before discarding, and a crash that drops it loses no live data. The
    /// queue is capped at a fixed, volume-independent [`MAX_PENDING_DISCARD`]
    /// blocks so a long-running mount that never trims cannot grow it without
    /// bound and a huge device cannot size it into a heap-exhausting allocation;
    /// a dropped entry merely stays un-discarded (still free) until a future
    /// free, trim pass, or mount rebuild requeues it.
    fn enqueue_discard(&mut self, block: u64) {
        if block < RING_BLOCKS || block >= self.total_blocks {
            return;
        }
        if let Ok(alloc) = self.allocator_mut() {
            if alloc.pending_discard.len() < MAX_PENDING_DISCARD {
                alloc.pending_discard.push(block);
            }
        }
    }

    /// Refuse a mutating operation on a read-only handle **before** it
    /// touches any state, so a read-only `/System` mount does no wasted
    /// copy-on-write work and never dirties the device. The [`Self::commit`] guard is the structural backstop for any
    /// internal write path that does not funnel through here.
    fn deny_if_read_only(&self) -> Result<(), DriverError> {
        if self.read_only {
            return Err(DriverError::PermissionDenied);
        }
        Ok(())
    }

    /// Commit the staged transaction. The inode tree and every extent tree are
    /// already copy-on-written in place as the operation runs, so commit just
    /// writes the new transaction root naming the inode-tree root, then
    /// publishes the next superblock-ring slot pointing at it
    /// (`transaction` / `superblock`).
    fn commit(&mut self) -> Result<(), DriverError> {
        // A read-only handle never publishes a transaction: every mutating
        // operation funnels through here, so refusing to commit fails the
        // whole mutation closed — the read-only `/System`
        // mount can be read but never written.
        if self.read_only {
            return Err(DriverError::PermissionDenied);
        }
        let bs = self.block_size;
        let next_gen = self.generation.wrapping_add(1);
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        let old_root = self.root_phys;
        let root_phys = self.alloc_block(true)?;
        // Release the superseded root into the deferred-free set *before* the
        // new root is sealed, so the free count the root records is the one
        // the committed volume will actually have. The blocks stay reserved
        // until `finish_txn`, and nothing allocates between here and the
        // commit point.
        self.free_meta(old_root);
        self.map_fold_pending()?;
        let deferred = self.allocator()?.txn_freed.len() as u64;
        let alloc_map_start = self.alloc_map_start;
        let root = TxnRoot {
            generation: next_gen,
            inode_tree_root: self.inode_tree_root,
            next_ino: self.next_ino,
            chunk_tree_root: self.chunk_tree_root,
            reverse_ref_tree_root: self.reverse_ref_tree_root,
            scrub_progress_root: self.scrub_progress_root,
            health_baseline_root: self.health_baseline_root,
            alloc_map_start,
            alloc_map_covered: self.total_blocks,
            free_count: self.free_count.saturating_add(deferred),
        };
        root.seal(&mut buf[..bs], self.fs_uuid, root_phys, &self.mac_key)?;
        self.write_meta(root_phys, &buf)?;
        let slot = slot_block(self.ring_pos % RING_SLOTS);
        let sb = Superblock {
            block_size: as_u32(bs),
            total_blocks: self.total_blocks,
            inode_count: self.inode_hint,
            generation: next_gen,
            root_phys,
            incompat: self.incompat,
        };
        sb.seal(
            &mut buf[..bs],
            self.fs_uuid,
            slot,
            &self.mac_key,
            &self.crypto_header,
        )?;
        self.write_meta(slot, &buf)?;
        // Commit point passed: the slot naming the new root is written, so a
        // mount now selects the new state. Ordered, but not yet barriered
        // against a device that reorders (`plans/ARXFS-WRITEBACK.md` WB1).
        self.generation = next_gen;
        self.ring_pos = self.ring_pos.wrapping_add(1);
        self.root_phys = root_phys;
        self.finish_txn();
        Ok(())
    }

    /// Validate device geometry and build a zeroed in-memory volume state.
    fn bootstrap(block: B) -> Result<Self, DriverError> {
        let geo = block.geometry()?;
        let block_size = geo.block_size as usize;
        if !(MIN_BLOCK_SIZE..=MAX_BLOCK_SIZE).contains(&block_size) || !block_size.is_power_of_two()
        {
            return Err(DriverError::Unsupported);
        }
        let total_blocks = geo.block_count;
        if total_blocks <= RING_BLOCKS + 8 {
            return Err(DriverError::NoSpace);
        }
        let fs = Self {
            block,
            fs_uuid: 0,
            mac_key: [0u8; tairix_crypto::MAC_KEY_LEN],
            filename_key: [0u8; tairix_crypto::AEAD_KEY_LEN],
            content_key: [0u8; tairix_crypto::AEAD_KEY_LEN],
            crypto_header: [0u8; CRYPTO_HEADER_LEN],
            block_size,
            total_blocks,
            inode_hint: 0,
            generation: 0,
            ring_pos: 0,
            inode_tree_root: 0,
            next_ino: u64::from(ROOT_INO) + 1,
            chunk_tree_root: 0,
            reverse_ref_tree_root: 0,
            scrub_progress_root: 0,
            health_baseline_root: 0,
            dedupe_domain: 0,
            root_phys: 0,
            free_count: total_blocks,
            alloc: None,
            alloc_map_start: 0,
            // A fresh volume declares nothing; the committed word is adopted
            // at mount and a feature bit is set by the first use of the
            // structure it names.
            incompat: 0,
            saved_inode_tree_root: 0,
            saved_next_ino: 0,
            saved_chunk_tree_root: 0,
            saved_reverse_ref_tree_root: 0,
            saved_scrub_progress_root: 0,
            saved_health_baseline_root: 0,
            saved_incompat: 0,
            clock: epoch_clock,
            read_only: false,
            cluster_cache: None,
            tree_edit: None,
        };
        Ok(fs)
    }

    /// Lay a fresh, empty arxfs volume onto `block` and return it mounted.
    /// `inode_hint` sizes nothing on disk any more — the inode tree grows on
    /// demand — but it is retained in the `format` signature and stored in the
    /// superblock for tools, and a value below two is still rejected so at
    /// least the root directory fits.
    ///
    /// The volume is encrypted at rest under `volume_key` (the installer's /
    /// recovery flow's key material): `format` provisions the per-volume key
    /// hierarchy (a wrapped master key deriving the metadata-authentication,
    /// filename, and content keys) and stores only the wrapped master key on
    /// disk. There is **no** plaintext layout path
    /// (`docs/src/filesystem/arxfs-spec.md` §5, §7). A fresh volume holds no
    /// shared chunks, so the chunk/refcount and reverse-reference trees and the
    /// dedupe index start empty and grow on demand.
    ///
    /// The random per-volume UUID and the master key, wrapping salt, and wrap
    /// nonce are drawn from `entropy`, the [`EntropySource`] seam onto the
    /// platform RNG (`lib/rng`'s `CsRng`). A failed draw
    /// fails closed: no volume is laid out with predictable key material.
    ///
    /// # Errors
    ///
    /// * [`DriverError::Unsupported`] if the device block size is unsupported.
    /// * [`DriverError::NoSpace`] if the device is too small or `inode_hint`
    ///   is below two.
    /// * [`DriverError`] from `entropy` if a random draw is unavailable.
    /// * [`DriverError::DeviceFault`] on an unrecoverable block write.
    pub fn format(
        block: B,
        inode_hint: u32,
        volume_key: &VolumeKey,
        entropy: &mut dyn EntropySource,
    ) -> Result<Self, DriverError> {
        let mut fs = Self::bootstrap(block)?;
        if inode_hint < 2 {
            return Err(DriverError::NoSpace);
        }
        fs.inode_hint = inode_hint;
        fs.fs_uuid = random_uuid(entropy)?;
        // Provision the per-volume key hierarchy: a random master key wrapped
        // under the caller's volume key, deriving the metadata-authentication,
        // filename, and content keys. There is no plaintext path
        // (`docs/src/filesystem/arxfs-spec.md` §5, §7).
        let (crypto_header, keys) = crypto::provision(volume_key, entropy)?;
        fs.apply_keys(&keys);
        crypto_header.encode(&mut fs.crypto_header);

        // Tell a discard-capable device the whole volume is free before the
        // encrypted structures are written (`docs/src/filesystem/arxfs-spec.md`
        // §11 mkfs flow). Discard is advisory: a device without support, or a
        // discard fault, must not stop a fresh volume from being created
        // (recorded, not failed), so the outcome is intentionally not
        // propagated as a format error.
        let _ = fs.mkfs_discard();
        // Lay the allocation map down before anything is allocated from it.
        // The region sits immediately above the superblock ring, so a fresh
        // volume's layout is fully determined by its block size and size.
        fs.rebuild_free_space_at(RING_BLOCKS, fs.total_blocks)?;

        fs.begin();
        let now = (fs.clock)();
        let mut root = Inode::empty(InodeKind::Dir, Security::new(0o755, 0, 0), now);
        root.nlink = 2;
        // Insert "." and ".." through the normal directory-insertion path so
        // they occupy as many directory blocks as the device's block size
        // requires (a 512-byte block holds only a single 263-byte slot).
        fs.add_entry(&mut root, ROOT_INO, ROOT_INO, b".")?;
        fs.add_entry(&mut root, ROOT_INO, ROOT_INO, b"..")?;
        fs.write_inode(ROOT_INO, &root)?;
        // mkfs stores a device-health baseline: the initial clean snapshot the
        // next mount compares against (`docs/src/filesystem/arxfs-spec.md`
        // §11). A device without health telemetry stores an `Unavailable`
        // baseline (recorded, not failed) and the health subsystem stays
        // enabled regardless.
        fs.store_initial_health_baseline()?;
        fs.commit()?;
        // Leave the map stamped clean at the committed generation, so the very
        // first mount of a freshly built image adopts it instead of walking
        // the volume.
        fs.map_persist()?;
        Ok(fs)
    }

    /// Open the arxfs volume on `block`, selecting the highest-generation
    /// committed transaction root from the superblock ring and rebuilding the
    /// in-memory free and inode-allocation state by walking it.
    ///
    /// A crash during a previous commit leaves an earlier committed root
    /// selected rather than a torn one (`docs/src/filesystem/arxfs-spec.md` §14).
    ///
    /// The volume is encrypted: `volume_key` must be the key material the
    /// volume was formatted with. `open` recovers the key hierarchy by
    /// unwrapping the master key stored in a superblock slot's discovery
    /// header; a wrong key never unwraps and the mount is refused with
    /// [`DriverError::PermissionDenied`], fail-closed,
    /// never a panic.
    ///
    /// # Errors
    ///
    /// * [`DriverError::Unsupported`] if the device block size is unsupported.
    /// * [`DriverError::PermissionDenied`] if `volume_key` does not unwrap the
    ///   volume (wrong key on an otherwise-valid arxfs volume).
    /// * [`DriverError::BadMagic`] if no committed superblock slot validates
    ///   (e.g. the device is not a arxfs volume).
    /// * [`DriverError::DeviceFault`] on an unrecoverable block read.
    pub fn open(block: B, volume_key: &VolumeKey) -> Result<Self, DriverError> {
        Self::open_inner(block, volume_key, false)
    }

    /// The volume's stable identity: the 16 raw bytes of the random
    /// per-volume UUID minted at [`Self::format`] and verified into every
    /// block header, in on-disk byte order.
    ///
    /// This is what the kernel volume forest publishes so the volume is
    /// addressable as `id::<volume-id>/…` (`docs/src/filesystem/drives.md`
    /// §8). Identity only — holding it grants nothing.
    #[must_use]
    pub fn volume_uuid(&self) -> [u8; 16] {
        self.fs_uuid.to_le_bytes()
    }

    /// Open the arxfs volume on `block` **read-only**, under `volume_key`.
    ///
    /// Identical to [`Self::open`] in how it selects and replays the
    /// committed transaction, but the returned handle is read-only: no
    /// block is ever written to the device, neither the mount-time
    /// companion-mirror repairs (the internal read paths skip them on a
    /// read-only handle) nor any later mutation — every mutating operation
    /// fails closed with [`DriverError::PermissionDenied`], never a panic.
    ///
    /// This is how the boot path mounts the read-only, signed-bundle
    /// `/System` volume (the design-B pre-unlock driver store,
    /// `plans/PI.md`): the volume carries no secrets and is keyed by a
    /// non-secret well-known key, so opening it grants read access to the
    /// store while the kernel can never mutate it.
    ///
    /// # Errors
    ///
    /// As [`Self::open`].
    pub fn open_read_only(block: B, volume_key: &VolumeKey) -> Result<Self, DriverError> {
        Self::open_inner(block, volume_key, true)
    }

    /// Shared body of [`Self::open`] and [`Self::open_read_only`]: select
    /// and replay the highest-generation committed transaction. When
    /// `read_only` is set the handle never writes the device (one open path for both modes).
    fn open_inner(block: B, volume_key: &VolumeKey, read_only: bool) -> Result<Self, DriverError> {
        let mut fs = Self::bootstrap(block)?;
        // Set the read-only flag before the ring scan so the mount-time
        // companion-mirror repairs are suppressed on a read-only handle.
        fs.read_only = read_only;
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        // Establish the volume keys before reading any authenticated metadata:
        // unwrap the master key from a superblock slot's plaintext discovery
        // header under `volume_key`. This sets `fs_uuid` and the working keys,
        // or fails closed on a wrong key.
        fs.establish_keys(volume_key, &mut buf)?;

        let mut best: Option<(Superblock, u128, u64)> = None;
        let uuid_pin: Option<u128> = Some(fs.fs_uuid);
        for slot in 0..RING_SLOTS {
            let primary = slot_block(slot);
            let Some((sb, uuid)) = fs.read_sb_slot(primary, uuid_pin, &mut buf)? else {
                continue;
            };
            // The superblock pins the *filesystem's* committed block count,
            // which may be smaller than the backing device when the device
            // was enlarged but the volume not yet grown ([`Self::grow`]).
            // Accept any committed size that fits within the device; reject
            // one that claims more blocks than the device has (a truncated or
            // corrupt device) or one too small to hold the reserved region.
            if sb.block_size as usize != fs.block_size
                || sb.total_blocks > fs.total_blocks
                || sb.total_blocks <= RING_BLOCKS + 8
            {
                continue;
            }
            if sb.root_phys < RING_BLOCKS || sb.root_phys >= sb.total_blocks {
                continue;
            }
            if fs
                .read_txn_root(uuid, sb.root_phys, sb.generation, &mut buf)
                .is_err()
            {
                continue;
            }
            if best.is_none_or(|(b, _, _)| sb.generation > b.generation) {
                best = Some((sb, uuid, slot));
            }
        }
        let (sb, _uuid, best_slot) = best.ok_or(DriverError::BadMagic)?;

        fs.inode_hint = sb.inode_count;
        fs.generation = sb.generation;
        fs.root_phys = sb.root_phys;
        fs.incompat = sb.incompat;
        fs.ring_pos = best_slot + 1;
        // Operate within the committed filesystem size, which may be smaller
        // than the backing device (the surplus tail is unused until a grow).
        fs.adopt_total_blocks(sb.total_blocks);

        let root = fs.read_txn_root(fs.fs_uuid, sb.root_phys, sb.generation, &mut buf)?;
        fs.inode_tree_root = root.inode_tree_root;
        fs.next_ino = root.next_ino;
        fs.chunk_tree_root = root.chunk_tree_root;
        fs.reverse_ref_tree_root = root.reverse_ref_tree_root;
        fs.scrub_progress_root = root.scrub_progress_root;
        fs.health_baseline_root = root.health_baseline_root;

        fs.free_count = root.free_count;
        // A read-only handle builds no allocation state at all: it cannot
        // allocate, so the map would only be dead weight, and skipping it is
        // what makes mounting a read-only volume a handful of block reads.
        if !read_only {
            fs.adopt_or_rebuild_alloc_map(root.alloc_map_start, root.alloc_map_covered)?;
        }
        Ok(fs)
    }

    /// Take up the allocation map the committed root names, rebuilding it from
    /// the authoritative trees when it cannot be trusted — after a crash, or
    /// when a page no longer authenticates.
    fn adopt_or_rebuild_alloc_map(&mut self, start: u64, covered: u64) -> Result<(), DriverError> {
        // The root is authenticated, so a region it names outside the volume,
        // or a coverage that disagrees with the committed size, is real
        // corruption rather than a stale value: refuse the mount instead of
        // laying a fresh region over live data.
        if start < RING_BLOCKS || start >= self.total_blocks || covered != self.total_blocks {
            return Err(DriverError::DeviceFault);
        }
        self.alloc_map_start = start;
        if self.map_adopt(start, covered) {
            return Ok(());
        }
        self.rebuild_free_space_at(start, covered)
    }

    /// Set the volume's working block count to span exactly `total` blocks and
    /// reset the allocation cursors into the new range so the next scan stays
    /// in bounds. The allocation map's own coverage is changed separately, by
    /// the grow path, because moving it may mean relaying the region.
    fn adopt_total_blocks(&mut self, total: u64) {
        self.total_blocks = total;
        if let Ok(alloc) = self.allocator_mut() {
            alloc.alloc_cursor = RING_BLOCKS;
            alloc.meta_cursor = total.saturating_sub(1);
        }
    }

    /// Grow the mounted volume to fill an enlarged backing device, online and
    /// in place (`docs/src/filesystem/arxfs-spec.md` §13).
    ///
    /// The committed filesystem size is pinned in the superblock and may be
    /// smaller than the device — for example after an administrator enlarges
    /// the underlying partition, logical volume, or virtual disk. `grow`
    /// re-reads the device geometry, folds the newly available tail blocks
    /// into the free pool, and commits a new superblock recording the larger
    /// size. It returns the number of blocks added (`0` when the volume
    /// already spans the whole device).
    ///
    /// The new blocks start life free, so no existing data moves and the
    /// operation is a single atomic transaction: a crash before the commit
    /// point leaves the previous (smaller) committed size selected on the next
    /// mount, never a torn geometry. The grown space is usable
    /// immediately, without remounting.
    ///
    /// Online *shrink* is deliberately not offered: it would require
    /// relocating any live blocks out of the truncated tail first, which a
    /// mounted volume cannot do safely in place. A device that has shrunk
    /// below the committed size is rejected.
    ///
    /// This is an inherent driver operation, not part of a frozen
    /// `Filesystem*` ABI trait, so it does not widen a shipped interface.
    ///
    /// # Errors
    ///
    /// * [`DriverError::Unsupported`] if the handle is read-only, the device
    ///   block size has changed, or the device is now *smaller* than the
    ///   committed filesystem size (an attempted online shrink).
    /// * [`DriverError::DeviceFault`] / [`DriverError::NoSpace`] on an
    ///   unrecoverable failure while committing the new size (fail-closed) — the in-memory geometry is restored so the
    ///   handle stays consistent with the still-committed on-disk size.
    pub fn grow(&mut self) -> Result<u64, DriverError> {
        if self.read_only {
            return Err(DriverError::Unsupported);
        }
        let geo = self.block.geometry()?;
        if geo.block_size as usize != self.block_size {
            return Err(DriverError::Unsupported);
        }
        let new_total = geo.block_count;
        if new_total < self.total_blocks {
            return Err(DriverError::Unsupported);
        }
        if new_total == self.total_blocks {
            return Ok(0);
        }
        let old_total = self.total_blocks;
        let old_start = self.alloc_map_start;
        let added = new_total - old_total;
        let new_start = self.plan_grown_map(new_total)?;
        self.adopt_total_blocks(new_total);
        self.alloc_map_start = new_start;
        // Widen (or relay) the map before the commit, so the free count the
        // new root records already accounts for the added tail.
        self.extend_alloc_map(new_start, new_total)?;
        self.begin();
        match self.commit() {
            Ok(()) => Ok(added),
            Err(err) => {
                // The commit did not publish: undo this transaction's
                // allocations and restore the previous in-memory geometry so
                // the handle still matches the committed on-disk size. The map
                // now describes a volume the device never committed to, so it
                // goes back too; failing that leaves the handle unusable and
                // the caller must see it.
                self.rollback();
                self.adopt_total_blocks(old_total);
                self.alloc_map_start = old_start;
                self.rebuild_free_space_at(old_start, old_total)?;
                Err(err)
            }
        }
    }

    /// Where the allocation map will live once the volume spans `new_total`
    /// blocks: in place when the region does not need to be longer, otherwise
    /// a contiguous free run — preferring the freshly added tail, which is
    /// free by construction.
    ///
    /// # Errors
    ///
    /// [`DriverError::NoSpace`] when no contiguous run long enough for the
    /// larger region exists.
    fn plan_grown_map(&mut self, new_total: u64) -> Result<u64, DriverError> {
        let geom = self.allocator()?.geom;
        let grown = MapGeometry::new(geom.start(), self.block_size, new_total)?;
        if grown.region_blocks() <= geom.region_blocks() {
            return Ok(geom.start());
        }
        let needed = grown.region_blocks();
        let old_total = self.total_blocks;
        if new_total - old_total >= needed {
            return Ok(old_total);
        }
        self.map_find_free_run(needed, RING_BLOCKS, old_total)?
            .ok_or(DriverError::NoSpace)
    }

    /// Take the allocation map from its current coverage up to `covered`
    /// blocks at `start`: widened in place when the region is unchanged and
    /// only the last page gained capacity, relaid from the authoritative trees
    /// when the region itself must be longer or must move.
    fn extend_alloc_map(&mut self, start: u64, covered: u64) -> Result<(), DriverError> {
        let geom = self.allocator()?.geom;
        let grown = MapGeometry::new(start, self.block_size, covered)?;
        if start != geom.start() || grown.region_blocks() != geom.region_blocks() {
            return self.rebuild_free_space_at(start, covered);
        }
        // Same region, same pages: only the last page's capacity moved, and
        // the bits it gained were already clear because nothing ever set a bit
        // beyond the old coverage.
        let last = grown.pages().saturating_sub(1);
        let gained = grown
            .page_capacity(last)
            .saturating_sub(geom.page_capacity(last));
        self.allocator_mut()?.geom = grown;
        self.free_count = self.free_count.saturating_add(covered - geom.covered());
        if gained == 0 {
            return Ok(());
        }
        let (index, offset) = grown.summary_slot_of(last);
        let summary_block = grown.summary_block(index);
        self.map_read(summary_block)?;
        let alloc = self.allocator_mut()?;
        let summary = alloc
            .cache
            .write(summary_block)
            .ok_or(DriverError::DeviceFault)?;
        let free = allocmap::summary_get(summary, offset);
        allocmap::summary_set(summary, offset, free.saturating_add(gained));
        Ok(())
    }

    /// Establish the working key set by unwrapping the master key with
    /// `volume_key` from a superblock slot's plaintext crypto discovery header
    /// (`crypto` module). Sets [`Self::fs_uuid`], the working keys, and the
    /// encoded discovery header that every commit re-publishes.
    ///
    /// # Errors
    ///
    /// * [`DriverError::PermissionDenied`] if a structurally-valid arxfs
    ///   superblock is present but `volume_key` does not unwrap it (wrong key)
    ///   — fail-closed.
    /// * [`DriverError::BadMagic`] if no slot even looks like a arxfs
    ///   superblock (the device is not a arxfs volume).
    fn establish_keys(
        &mut self,
        volume_key: &VolumeKey,
        buf: &mut [u8],
    ) -> Result<(), DriverError> {
        let mut saw_structure = false;
        for slot in 0..RING_SLOTS {
            let primary = slot_block(slot);
            for phys in [primary, Self::companion(primary)] {
                if self.read_block(phys, buf).is_err() {
                    continue;
                }
                if rd_u64(buf, 0) != HEADER_MAGIC || rd_u32(buf, 12) != FORMAT_VERSION {
                    continue;
                }
                saw_structure = true;
                let Some(header) = CryptoHeader::decode(&buf[superblock::CRYPTO_OFFSET..]) else {
                    continue;
                };
                if let Ok(keys) = crypto::unwrap(volume_key, &header) {
                    let mut uuid_bytes = [0u8; 16];
                    uuid_bytes.copy_from_slice(&buf[16..32]);
                    self.fs_uuid = u128::from_le_bytes(uuid_bytes);
                    self.apply_keys(&keys);
                    self.crypto_header.copy_from_slice(
                        &buf[superblock::CRYPTO_OFFSET
                            ..superblock::CRYPTO_OFFSET + CRYPTO_HEADER_LEN],
                    );
                    return Ok(());
                }
            }
        }
        Err(if saw_structure {
            DriverError::PermissionDenied
        } else {
            DriverError::BadMagic
        })
    }

    /// Read a superblock-ring slot at primary block `primary`, falling back to
    /// its companion mirror and repairing the primary from a good companion
    /// (`docs/src/filesystem/arxfs-spec.md` §8). Returns the decoded slot and
    /// its UUID, or `Ok(None)` when neither copy is usable (the ring scan then
    /// skips the slot). Authenticated under the volume's metadata-authentication
    /// key, recovered in [`Self::establish_keys`].
    ///
    /// A copy that cannot be **read** is as absent as one that fails to
    /// authenticate — a media error on a single sector is exactly what the
    /// mirror is for — so the fallback covers both.
    ///
    /// # Errors
    ///
    /// [`DriverError::Unsupported`] when a copy authenticates but declares an
    /// on-disk feature this build does not implement: the volume is refused
    /// with its reason rather than reported as unrecognisable.
    fn read_sb_slot(
        &mut self,
        primary: u64,
        uuid_pin: Option<u128>,
        buf: &mut [u8],
    ) -> Result<Option<(Superblock, u128)>, DriverError> {
        let bs = self.block_size;
        if self.read_block(primary, buf).is_ok() {
            if let Some(found) =
                Superblock::try_decode(&buf[..bs], uuid_pin, primary, &self.mac_key)?
            {
                return Ok(Some(found));
            }
        }
        if self.read_block(Self::companion(primary), buf).is_err() {
            return Ok(None);
        }
        let Some(found) = Superblock::try_decode(&buf[..bs], uuid_pin, primary, &self.mac_key)?
        else {
            return Ok(None);
        };
        // The ring scan tries every slot and keeps the best, so a repair the
        // device refuses must not make a slot that decoded cleanly invisible.
        let _ = self.repair_meta_copy(primary, buf);
        Ok(Some(found))
    }

    /// Read the transaction root at `root_phys`, falling back to its companion
    /// mirror and repairing the primary from a good companion
    /// (`docs/src/filesystem/arxfs-spec.md` §8). On success `buf` holds the
    /// good root's bytes.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] when neither copy is a valid committed
    /// root for `expect_generation` (the ring scan treats that as "this slot
    /// did not commit" and falls back).
    fn read_txn_root(
        &mut self,
        uuid: u128,
        root_phys: u64,
        expect_generation: u64,
        buf: &mut [u8],
    ) -> Result<TxnRoot, DriverError> {
        let bs = self.block_size;
        let key = self.mac_key;
        // A copy that cannot be read is as absent as one that fails to
        // authenticate, so both fall through to the companion.
        if self.read_block(root_phys, buf).is_ok() {
            if let Ok(root) =
                TxnRoot::decode_verify(&buf[..bs], uuid, root_phys, expect_generation, &key)
            {
                return Ok(root);
            }
        }
        self.read_block(Self::companion(root_phys), buf)?;
        let root = TxnRoot::decode_verify(&buf[..bs], uuid, root_phys, expect_generation, &key)?;
        self.repair_meta_copy(root_phys, buf)?;
        Ok(root)
    }

    /// Mark every block reachable from the committed trees used while the
    /// allocation map is being rebuilt: every chunk / reverse-reference and
    /// inode-tree node, and, for each inode, its extent-tree nodes plus the
    /// physical runs they map. Every metadata block accounts for both its
    /// physical copies (`docs/src/filesystem/arxfs-spec.md` §4, §5).
    pub(crate) fn mark_reachable_metadata(&mut self) -> Result<(), DriverError> {
        let mut walk = TreeWalk::new(self.block_size)?;
        let mut extent_walk = TreeWalk::new(self.block_size)?;
        self.mark_tree_nodes(self.chunk_tree_root, chunk_spec(), &mut walk)?;
        self.mark_tree_nodes(self.reverse_ref_tree_root, reverse_ref_spec(), &mut walk)?;

        let spec = inode_spec();
        let mut trail = NodeTrail::new();
        walk.restart();
        while self.btree_next_leaf(self.inode_tree_root, spec, &mut walk)? {
            trail.advance(walk.path());
            for &node in trail.entered() {
                self.mark_meta_used_checked(node)?;
            }
            for (key, value) in walk.entries() {
                let inode = Inode::decode(value)?.ok_or(DriverError::DeviceFault)?;
                let ino = u32::try_from(key).map_err(|_| DriverError::DeviceFault)?;
                self.mark_inode_blocks(ino, &inode, &mut extent_walk)?;
            }
        }
        Ok(())
    }

    /// Mark every node of the tree at `root` as used, without reading its
    /// records: what the trees whose values name no further blocks need.
    fn mark_tree_nodes(
        &mut self,
        root: u64,
        spec: btree::TreeSpec,
        walk: &mut TreeWalk,
    ) -> Result<(), DriverError> {
        let mut trail = NodeTrail::new();
        walk.restart();
        while self.btree_next_leaf(root, spec, walk)? {
            trail.advance(walk.path());
            for &node in trail.entered() {
                self.mark_meta_used_checked(node)?;
            }
        }
        Ok(())
    }

    /// Mark every extent-tree node and every physical run reachable from
    /// `inode` (number `ino`) as used while the allocation map is rebuilt.
    fn mark_inode_blocks(
        &mut self,
        ino: u32,
        inode: &Inode,
        walk: &mut TreeWalk,
    ) -> Result<(), DriverError> {
        let spec = extent_spec(ino);
        // A directory's content blocks are themselves metadata
        // ([`BlockType::Directory`], mirrored pairs); a regular file's and a
        // link's are single-copy data. Account for the directory mirror so
        // the rebuilt free set matches the live one
        // (`docs/src/filesystem/arxfs-spec.md` §5).
        let mirrored = inode.kind.content_is_metadata();
        let mut trail = NodeTrail::new();
        walk.restart();
        while self.btree_next_leaf(inode.extent_root, spec, walk)? {
            trail.advance(walk.path());
            for &node in trail.entered() {
                self.mark_meta_used_checked(node)?;
            }
            for (_, value) in walk.entries() {
                let ext = Extent::decode(value)?;
                for b in 0..ext.stored {
                    if mirrored {
                        self.mark_meta_used_checked(ext.phys + b)?;
                    } else {
                        self.mark_used_checked(ext.phys + b)?;
                    }
                }
            }
        }
        // The attribute block is a mirrored metadata pair like a directory
        // block, so account for its companion too.
        if inode.attr_root != 0 {
            self.mark_meta_used_checked(inode.attr_root)?;
        }
        Ok(())
    }

    /// Upper bound on a file's block count: the whole device. The extent tree
    /// removes the Stage-1 direct/indirect addressing cap, so a file may span
    /// the volume (`docs/src/filesystem/arxfs-spec.md` §6).
    fn max_file_blocks(&self) -> u64 {
        self.total_blocks
    }

    /// The extent covering logical block `bi` of `inode`, with its starting
    /// logical block, or `None` for a hole. Resolves with a floor lookup on
    /// the extent tree.
    fn extent_lookup(
        &mut self,
        inode: &Inode,
        bi: u64,
    ) -> Result<Option<(u64, Extent)>, DriverError> {
        let spec = extent_spec(0);
        match self.btree_get_floor(inode.extent_root, bi, spec)? {
            Some((start, value)) => {
                let ext = Extent::decode(&value)?;
                if bi < start + ext.len {
                    Ok(Some((start, ext)))
                } else {
                    Ok(None)
                }
            }
            None => Ok(None),
        }
    }

    /// The data block backing logical block `bi` of `inode`, `0` for a hole.
    ///
    /// Serves the paths whose blocks are always raw 1:1 records: directory
    /// content, and the per-block file paths after any covering compressed
    /// cluster has been decomposed. A compressed extent has no per-block
    /// backing, so finding one here is corruption and fails closed.
    fn block_ptr(&mut self, inode: &Inode, bi: u64) -> Result<u64, DriverError> {
        match self.extent_lookup(inode, bi)? {
            Some((_, ext)) if ext.compressed => Err(DriverError::DeviceFault),
            Some((start, ext)) => Ok(ext.phys + (bi - start)),
            None => Ok(0),
        }
    }

    /// Drop the mapping for logical block `bi` of `inode` (number `ino`),
    /// splitting the run that covers it. The physical block is not freed here;
    /// the caller owns that (e.g. [`cow_data`](Self::cow_data) frees a
    /// superseded block).
    fn extent_remove(&mut self, inode: &mut Inode, ino: u32, bi: u64) -> Result<(), DriverError> {
        let spec = extent_spec(ino);
        let Some((start, value)) = self.btree_get_floor(inode.extent_root, bi, spec)? else {
            return Ok(());
        };
        let ext = Extent::decode(&value)?;
        if bi >= start + ext.len {
            return Ok(());
        }
        // A compressed cluster has no per-block mapping to split; callers
        // decompose it first, so covering one here is corruption. Fail closed
        // rather than orphan the stored run.
        if ext.compressed {
            return Err(DriverError::DeviceFault);
        }
        inode.extent_root = self.btree_remove(inode.extent_root, start, spec)?;
        if bi > start {
            let left = Extent::raw(ext.phys, bi - start).encode();
            inode.extent_root = self.btree_insert(inode.extent_root, start, &left, spec)?;
        }
        let end = start + ext.len;
        if bi + 1 < end {
            let rphys = ext.phys + (bi + 1 - start);
            let right = Extent::raw(rphys, end - (bi + 1)).encode();
            inode.extent_root = self.btree_insert(inode.extent_root, bi + 1, &right, spec)?;
        }
        Ok(())
    }

    /// Map logical block `bi` of `inode` (number `ino`) to physical block
    /// `ptr`, merging with a physically contiguous neighbour so a sequential
    /// write collapses to a single run.
    fn extent_assign(
        &mut self,
        inode: &mut Inode,
        ino: u32,
        bi: u64,
        ptr: u64,
    ) -> Result<(), DriverError> {
        self.extent_remove(inode, ino, bi)?;
        if ptr == 0 {
            return Ok(());
        }
        let spec = extent_spec(ino);
        let mut start = bi;
        let mut phys = ptr;
        let mut len = 1u64;
        // Only raw neighbours merge: a compressed cluster is a sealed unit
        // whose stored run never coalesces with a 1:1 block.
        if bi > 0 {
            if let Some((ls, value)) = self.btree_get_floor(inode.extent_root, bi - 1, spec)? {
                let left = Extent::decode(&value)?;
                if !left.compressed && ls + left.len == bi && left.phys + left.len == ptr {
                    inode.extent_root = self.btree_remove(inode.extent_root, ls, spec)?;
                    start = ls;
                    phys = left.phys;
                    len = left.len + 1;
                }
            }
        }
        if let Some((rs, value)) = self.btree_get_floor(inode.extent_root, bi + 1, spec)? {
            let right = Extent::decode(&value)?;
            if !right.compressed && rs == bi + 1 && phys + len == right.phys {
                inode.extent_root = self.btree_remove(inode.extent_root, rs, spec)?;
                len += right.len;
            }
        }
        let value = Extent::raw(phys, len).encode();
        inode.extent_root = self.btree_insert(inode.extent_root, start, &value, spec)?;
        Ok(())
    }

    /// Copy-on-write the raw data block referenced at `(ino, bi)`: reuse
    /// `old_ptr` in place only when it is private to this transaction **and**
    /// not a shared chunk (a shared chunk is immutable — overwriting
    /// shared data creates a new physical record). Otherwise allocate a fresh
    /// block and drop the old reference. Returns the (unwritten) block.
    fn cow_data(&mut self, old_ptr: u64, ino: u32, bi: u64) -> Result<u64, DriverError> {
        if old_ptr != 0 && self.is_txn_private(old_ptr) && self.data_refcount(old_ptr)? == 1 {
            return Ok(old_ptr);
        }
        let new = self.alloc_block(false)?;
        if old_ptr != 0 {
            self.release_block_ref(old_ptr, ino, bi)?;
        }
        Ok(new)
    }

    /// Store the plaintext in `blk[..data_capacity()]` as the data record for
    /// `(ino, bi)`, currently backed by `old_ptr` (`0` if unmapped).
    ///
    /// Deduplication is attempted first: if a live, byte-identical chunk in the
    /// same encryption domain already exists, `(ino, bi)` is pointed at it and
    /// no new physical block is written
    /// (`docs/src/filesystem/arxfs-spec.md` §4, §6, §9). Otherwise the block is
    /// copy-on-written, sealed, and the dedupe index records it as a future
    /// candidate. Sharing is only ever taken after the candidate's bytes are
    /// confirmed equal, so unequal data is never merged.
    fn store_block(
        &mut self,
        inode: &mut Inode,
        ino: u32,
        bi: u64,
        old_ptr: u64,
        blk: &mut [u8],
    ) -> Result<(), DriverError> {
        let capu = as_usize(self.data_capacity());
        // Sparse storage pipeline (`plans/SPARSE.md` §4, §6): an all-zero
        // logical record is detected before compression, dedupe, encryption,
        // or physical allocation and stored as a metadata-only hole. The old
        // physical block (if any) is released through the normal COW/refcount
        // path; the gap then reads back as zero (`read_file`). A zero range is
        // never passed to the compressor or entered in the dedupe index.
        if is_all_zero(&blk[..capu]) {
            self.extent_remove(inode, ino, bi)?;
            if old_ptr != 0 {
                self.release_block_ref(old_ptr, ino, bi)?;
            }
            return Ok(());
        }
        let hash = logical_hash(&blk[..capu]);
        let domain = self.dedupe_domain;
        if let Some(cand) = self.dedupe_lookup(domain, &hash, &blk[..capu])? {
            if cand.phys == old_ptr {
                // The position already points at this exact record; the COW
                // would be a no-op, so leave the mapping untouched.
                return Ok(());
            }
            self.share_block_ref(cand, ino, bi, domain, &hash)?;
            if old_ptr != 0 {
                self.release_block_ref(old_ptr, ino, bi)?;
            }
            self.extent_assign(inode, ino, bi, cand.phys)?;
            return Ok(());
        }
        let new_ptr = self.cow_data(old_ptr, ino, bi)?;
        self.write_data_block(new_ptr, blk)?;
        self.extent_assign(inode, ino, bi, new_ptr)?;
        self.index_insert(domain, &hash, new_ptr, ino, bi);
        Ok(())
    }

    /// Look up the chunk/refcount record for the data block at `phys`, or
    /// `None` when the block is not a shared chunk (its reference count is the
    /// implicit `1`, `docs/src/filesystem/arxfs-spec.md` §9).
    fn chunk_get(&mut self, phys: u64) -> Result<Option<ChunkRecord>, DriverError> {
        match self.btree_get(self.chunk_tree_root, phys, chunk_spec())? {
            Some(value) => Ok(Some(
                ChunkRecord::decode(&value).ok_or(DriverError::DeviceFault)?,
            )),
            None => Ok(None),
        }
    }

    /// Insert or replace the chunk/refcount record for the block at `phys`.
    fn chunk_put(&mut self, phys: u64, record: &ChunkRecord) -> Result<(), DriverError> {
        self.chunk_tree_root =
            self.btree_insert(self.chunk_tree_root, phys, &record.encode(), chunk_spec())?;
        Ok(())
    }

    /// Drop the chunk/refcount record for the block at `phys` (it returns to
    /// the implicit reference count of `1`).
    fn chunk_remove(&mut self, phys: u64) -> Result<(), DriverError> {
        self.chunk_tree_root = self.btree_remove(self.chunk_tree_root, phys, chunk_spec())?;
        Ok(())
    }

    /// The current referrer list for the shared chunk at `phys`, or an empty
    /// list when the block records no referrers (it is not yet shared).
    fn reverse_refs(&mut self, phys: u64) -> Result<Vec<dedupe::Referrer>, DriverError> {
        match self.btree_get(self.reverse_ref_tree_root, phys, reverse_ref_spec())? {
            Some(value) => dedupe::decode_reverse_ref(&value).ok_or(DriverError::DeviceFault),
            None => Ok(Vec::new()),
        }
    }

    /// Insert or replace the reverse-reference record for the chunk at `phys`.
    fn reverse_refs_put(
        &mut self,
        phys: u64,
        referrers: &[dedupe::Referrer],
    ) -> Result<(), DriverError> {
        let value = dedupe::encode_reverse_ref(referrers);
        self.reverse_ref_tree_root =
            self.btree_insert(self.reverse_ref_tree_root, phys, &value, reverse_ref_spec())?;
        Ok(())
    }

    /// Drop the reverse-reference record for the chunk at `phys`.
    fn reverse_refs_remove(&mut self, phys: u64) -> Result<(), DriverError> {
        self.reverse_ref_tree_root =
            self.btree_remove(self.reverse_ref_tree_root, phys, reverse_ref_spec())?;
        Ok(())
    }

    /// The reference count of the data block at `phys`: the stored chunk record
    /// when shared, otherwise the implicit `1`.
    fn data_refcount(&mut self, phys: u64) -> Result<u64, DriverError> {
        Ok(self.chunk_get(phys)?.map_or(1, |record| record.refcount))
    }

    /// Drop the reference `(ino, bi)` holds on the data block at `phys`.
    ///
    /// A block with the implicit reference count of `1` is freed outright. A
    /// shared chunk is decremented and the `(ino, bi)` referrer struck from its
    /// reverse-reference list; when only one referrer remains the chunk returns
    /// to the implicit count and keeps its (now sole) physical block
    /// (`docs/src/filesystem/arxfs-spec.md` §9).
    fn release_block_ref(&mut self, phys: u64, ino: u32, bi: u64) -> Result<(), DriverError> {
        let Some(record) = self.chunk_get(phys)? else {
            self.free_block(phys);
            return Ok(());
        };
        let mut referrers = self.reverse_refs(phys)?;
        referrers.retain(|&(r_ino, r_bi)| !(r_ino == ino && r_bi == bi));
        let remaining = record.refcount.saturating_sub(1);
        if remaining <= 1 {
            self.chunk_remove(phys)?;
            self.reverse_refs_remove(phys)?;
        } else {
            let updated = ChunkRecord {
                refcount: remaining,
                ..record
            };
            self.chunk_put(phys, &updated)?;
            self.reverse_refs_put(phys, &referrers)?;
        }
        Ok(())
    }

    /// Add a reference from `(new_ino, new_bi)` to the existing data block at
    /// `cand.phys`, promoting it from an implicit single reference to a shared
    /// chunk on first share (recording both the original referrer carried in
    /// `cand` and the new one) or bumping its count and appending the referrer
    /// thereafter (`docs/src/filesystem/arxfs-spec.md` §9).
    fn share_block_ref(
        &mut self,
        cand: DedupeCandidate,
        new_ino: u32,
        new_bi: u64,
        domain: u64,
        logical_hash: &[u8; LOGICAL_HASH_LEN],
    ) -> Result<(), DriverError> {
        if let Some(record) = self.chunk_get(cand.phys)? {
            let mut referrers = self.reverse_refs(cand.phys)?;
            referrers.push((new_ino, new_bi));
            let updated = ChunkRecord {
                refcount: record.refcount + 1,
                ..record
            };
            self.chunk_put(cand.phys, &updated)?;
            self.reverse_refs_put(cand.phys, &referrers)?;
        } else {
            let record = ChunkRecord {
                refcount: 2,
                domain,
                length: as_u32(as_usize(self.data_capacity())),
                logical_hash: *logical_hash,
            };
            self.chunk_put(cand.phys, &record)?;
            self.reverse_refs_put(cand.phys, &[(cand.inode, cand.logical), (new_ino, new_bi)])?;
        }
        Ok(())
    }

    /// Find a live, byte-identical, shareable chunk for `content` in `domain`,
    /// consulting the rebuildable dedupe index (never authoritative).
    ///
    /// A candidate is returned only when it is still live (its recorded
    /// referrer's extent map still points at it), has room for another referrer
    /// ([`REVERSE_REF_CAP`]), and its bytes are confirmed equal to `content`.
    /// A candidate that fails the liveness or byte check is a stale index entry
    /// and is dropped; a full candidate is left in place but not shared (the
    /// write proceeds unique, an allowed missed duplicate).
    fn dedupe_lookup(
        &mut self,
        domain: u64,
        logical_hash: &[u8; LOGICAL_HASH_LEN],
        content: &[u8],
    ) -> Result<Option<DedupeCandidate>, DriverError> {
        let key = dedupe_key(domain, as_u32(content.len()), logical_hash);
        let Some(cand) = self.allocator_mut()?.dedupe_index.get(&key) else {
            return Ok(None);
        };
        if !self.candidate_is_live(cand)? {
            self.allocator_mut()?.dedupe_index.remove(&key);
            return Ok(None);
        }
        if usize::try_from(self.data_refcount(cand.phys)?).unwrap_or(usize::MAX) >= REVERSE_REF_CAP
        {
            return Ok(None);
        }
        if !self.byte_identical(cand.phys, content) {
            self.allocator_mut()?.dedupe_index.remove(&key);
            return Ok(None);
        }
        Ok(Some(cand))
    }

    /// Whether `cand`'s recorded referrer still maps its logical block to
    /// `cand.phys`; if not, the index entry is stale (the referrer was
    /// overwritten or removed).
    fn candidate_is_live(&mut self, cand: DedupeCandidate) -> Result<bool, DriverError> {
        let inode = match self.read_inode(cand.inode) {
            Ok(inode) => inode,
            Err(DriverError::NotFound) => return Ok(false),
            Err(other) => return Err(other),
        };
        if inode.is_dir() {
            return Ok(false);
        }
        // A candidate always names a raw 1:1 record; a hole or a compressed
        // cluster now covering its logical block means it was overwritten.
        match self.extent_lookup(&inode, cand.logical)? {
            Some((start, ext)) if !ext.compressed => {
                Ok(ext.phys + (cand.logical - start) == cand.phys)
            }
            _ => Ok(false),
        }
    }

    /// Whether the data block at `phys` decodes to plaintext byte-identical to
    /// `content`. A read or integrity failure reads as "not identical" so a
    /// damaged candidate is never shared.
    fn byte_identical(&mut self, phys: u64, content: &[u8]) -> bool {
        let capu = as_usize(self.data_capacity());
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        match self.read_data_block(phys, &mut buf) {
            Ok(()) => content.len() <= capu && buf[..content.len()] == *content,
            Err(_) => false,
        }
    }

    /// Record the freshly written unique block at `phys` as a future dedupe
    /// candidate for `content` in `domain`, introduced by referrer `(ino, bi)`.
    fn index_insert(
        &mut self,
        domain: u64,
        logical_hash: &[u8; LOGICAL_HASH_LEN],
        phys: u64,
        ino: u32,
        bi: u64,
    ) {
        let key = dedupe_key(domain, as_u32(as_usize(self.data_capacity())), logical_hash);
        if let Ok(alloc) = self.allocator_mut() {
            alloc.dedupe_index.insert(
                key,
                DedupeCandidate {
                    phys,
                    inode: ino,
                    logical: bi,
                },
            );
        }
    }

    /// Byte offset of a data block's compression descriptor: immediately after
    /// the content region and its crypto trailer (`integrity` module).
    fn compression_desc_offset(&self) -> usize {
        as_usize(self.data_capacity()) + CRYPTO_TRAILER
    }

    /// Byte offset of a data block's logical-hash field: immediately after the
    /// compression descriptor (`integrity` module).
    fn logical_hash_offset(&self) -> usize {
        self.compression_desc_offset() + COMPRESSION_DESCRIPTOR_LEN
    }

    /// Byte offset of a data block's physical-checksum field: immediately after
    /// the logical-hash field. The checksum covers everything before it.
    fn phys_checksum_offset(&self) -> usize {
        self.logical_hash_offset() + LOGICAL_HASH_LEN
    }

    /// Verify the two-layer integrity field of the data block staged in `buf`
    /// from address `phys`, and decrypt its content in place, leaving the
    /// decrypted content slot in `buf[..data_capacity()]` and returning how
    /// the record is stored (`docs/src/filesystem/arxfs-spec.md` §6).
    ///
    /// The read path is the spec's: verify the fast physical checksum over the
    /// at-rest block first (so media corruption is caught cheaply, before the
    /// AEAD), then authenticate-and-decrypt the content, then verify the
    /// decrypted slot against its stored logical hash. Each layer is kept
    /// distinct ([`DataFault`]); the classification drives the Stage 5 tests
    /// and is the seam scrub and health record against.
    ///
    /// The checks are separate from the fetch ([`read_block_run`](Self::read_block_run))
    /// so one device request can serve a whole contiguous run without
    /// weakening any of them: every block passes its own checksum, AEAD, and
    /// content-slot hash keyed by its own `phys`, so a misdirected block
    /// inside a run fails closed exactly as a single-block read would.
    fn verify_data_block(&self, phys: u64, buf: &mut [u8]) -> Result<StoredForm, DataFault> {
        let cap = as_usize(self.data_capacity());
        let csum_off = self.phys_checksum_offset();
        let mut stored = [0u8; PHYS_CHECKSUM_LEN];
        stored.copy_from_slice(&buf[csum_off..csum_off + PHYS_CHECKSUM_LEN]);
        if physical_checksum(&buf[..csum_off]) != stored {
            return Err(DataFault::Physical);
        }
        let desc_off = self.compression_desc_offset();
        let hash_off = self.logical_hash_offset();
        {
            let (region, rest) = buf[..desc_off].split_at_mut(cap);
            decrypt_region(&self.content_key, region, &rest[..CRYPTO_TRAILER], phys)
                .map_err(|_| DataFault::Aead)?;
        }
        let form = read_stored_form(&buf[desc_off..desc_off + COMPRESSION_DESCRIPTOR_LEN])?;
        let mut expect = [0u8; LOGICAL_HASH_LEN];
        expect.copy_from_slice(&buf[hash_off..hash_off + LOGICAL_HASH_LEN]);
        if logical_hash(&buf[..cap]) != expect {
            return Err(DataFault::Logical);
        }
        Ok(form)
    }

    /// Read the **raw** single-block data record at `phys`, leaving its
    /// plaintext in `buf[..data_capacity()]`.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] on a read failure or on any integrity
    /// layer failing (fail closed, never a panic).
    fn read_data_block(&mut self, phys: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        self.read_data_block_classified(phys, buf)
            .map_err(|_| DriverError::DeviceFault)
    }

    /// As [`read_data_block`](Self::read_data_block), but reports *which*
    /// integrity layer rejected the block. The block must hold a raw
    /// single-block record; a compressed-cluster block reached through a
    /// per-block path is a wrong-shape read and classifies as a logical
    /// fault. Production callers go through
    /// [`read_data_block`](Self::read_data_block) and see only a fail-closed
    /// [`DriverError::DeviceFault`].
    fn read_data_block_classified(&mut self, phys: u64, buf: &mut [u8]) -> Result<(), DataFault> {
        self.read_block(phys, buf)
            .map_err(|_| DataFault::Physical)?;
        self.verify_raw_block(phys, buf)
    }

    /// [`verify_data_block`](Self::verify_data_block) for a block a per-block
    /// path staged: the record must be a raw single-block one, so a
    /// compressed-cluster block reached this way is a wrong-shape read and
    /// classifies as a logical fault.
    fn verify_raw_block(&self, phys: u64, buf: &mut [u8]) -> Result<(), DataFault> {
        match self.verify_data_block(phys, buf)? {
            StoredForm::Raw => Ok(()),
            StoredForm::ClusterHead { .. } | StoredForm::ClusterPart { .. } => {
                Err(DataFault::Logical)
            }
        }
    }

    /// Encrypt the content slot in `buf[..data_capacity()]` under the content
    /// key, seal the stored-form descriptor and the data-integrity trailer
    /// (logical hash of the decrypted slot, then a fast physical checksum over
    /// the at-rest bytes), and write the block to `phys`. The nonce is unique
    /// per `(phys, generation)` so copy-on-write never reuses a `(key, nonce)`
    /// pair (`crypto` module).
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] on a seal failure or a block write failure.
    fn seal_data_block(
        &mut self,
        phys: u64,
        buf: &mut [u8],
        form: StoredForm,
    ) -> Result<(), DriverError> {
        let cap = as_usize(self.data_capacity());
        let next_gen = self.generation.wrapping_add(1);
        // The logical hash names the decrypted content slot, so it is taken
        // before encryption (`docs/src/filesystem/arxfs-spec.md` §6 write
        // path).
        let hash = logical_hash(&buf[..cap]);
        let desc_off = self.compression_desc_offset();
        let hash_off = self.logical_hash_offset();
        {
            let (region, trailer) = buf[..desc_off].split_at_mut(cap);
            encrypt_region(
                &self.content_key,
                region,
                &mut trailer[..CRYPTO_TRAILER],
                phys,
                next_gen,
            )
            .map_err(|_| DriverError::DeviceFault)?;
        }
        write_stored_form(
            &mut buf[desc_off..desc_off + COMPRESSION_DESCRIPTOR_LEN],
            form,
        );
        buf[hash_off..hash_off + LOGICAL_HASH_LEN].copy_from_slice(&hash);
        // The physical checksum covers the at-rest representation: ciphertext,
        // crypto trailer, stored-form descriptor, and logical hash — everything
        // before the checksum.
        let csum_off = self.phys_checksum_offset();
        let checksum = physical_checksum(&buf[..csum_off]);
        buf[csum_off..csum_off + PHYS_CHECKSUM_LEN].copy_from_slice(&checksum);
        self.write_block(phys, buf)
    }

    /// Store the plaintext in `buf[..data_capacity()]` as a **raw**
    /// single-block data record at `phys`.
    ///
    /// A single block is always stored raw: inside a fixed 1:1 block a
    /// compressed frame frees nothing (its padding is encrypted, so not even
    /// a lower layer could reclaim it), it only costs CPU on the hot data
    /// path. Real space savings come from the cluster path
    /// (`cluster` module), which stores a whole compressed cluster in fewer
    /// physical blocks (`docs/src/filesystem/arxfs-spec.md` §10).
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] on a seal failure or a block write failure.
    fn write_data_block(&mut self, phys: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        self.seal_data_block(phys, buf, StoredForm::Raw)
    }

    /// Free every physical run backing `inode` (number `ino`) and every node
    /// of its extent tree, leaving an empty zero-length file.
    ///
    /// A directory's content blocks are metadata mirrored pairs, so they are
    /// freed with their companion ([`Self::free_meta`]); a regular file's and
    /// a link's are single-copy data ([`Self::free_block`]).
    fn free_all_blocks(&mut self, inode: &mut Inode, ino: u32) -> Result<(), DriverError> {
        let spec = extent_spec(ino);
        let mirrored = inode.kind.content_is_metadata();
        let mut walk = TreeWalk::new(self.block_size)?;
        let mut trail = NodeTrail::new();
        // One walk frees both the runs and the tree that maps them: a node is
        // freed as the walk leaves it, so no later step descends through a
        // block that has already gone back to the allocator.
        while self.btree_next_leaf(inode.extent_root, spec, &mut walk)? {
            trail.advance(walk.path());
            for &node in trail.closed() {
                self.free_meta(node);
            }
            for (start, value) in walk.entries() {
                let ext = Extent::decode(value)?;
                if ext.compressed {
                    self.release_cluster(&ext, ino, start)?;
                    continue;
                }
                for b in 0..ext.len {
                    if mirrored {
                        self.free_meta(ext.phys + b);
                    } else {
                        self.release_block_ref(ext.phys + b, ino, start + b)?;
                    }
                }
            }
        }
        for &node in trail.close() {
            self.free_meta(node);
        }
        if inode.attr_root != 0 {
            self.free_meta(inode.attr_root);
            inode.attr_root = 0;
        }
        inode.extent_root = 0;
        inode.size = 0;
        Ok(())
    }

    /// The structural [`NodeInfo`] of `inode` (number `ino`).
    ///
    /// The one definition `node_info` and `read_dir` both report, so a stat
    /// and a listing can never disagree about a node's kind or its sizes.
    fn inode_info(&mut self, ino: u32, inode: &Inode) -> Result<NodeInfo, DriverError> {
        let allocated = self.allocated_bytes(inode, ino)?;
        Ok(NodeInfo {
            kind: inode.kind.node_kind(),
            // Read from the inode, never counted: ARXFS maintains the field
            // for every kind, so a stat and a listing report the same names.
            nlink: inode.nlink,
            size: match inode.kind {
                // A directory's entries are not a byte length.
                InodeKind::Dir => 0,
                // A link's size is its target's length, exactly as a file's
                // is its content's.
                InodeKind::File | InodeKind::Link => inode.size,
            },
            allocated,
            times: inode.times,
        })
    }

    fn dir_block_count(&self, dir: &Inode) -> u64 {
        dir.size / self.block_size as u64
    }

    /// Advance `scan` to the next occupied entry of directory `dir`, returning
    /// its slot position and the inode it names, or `None` at the end.
    ///
    /// One directory block is resident at a time, in the scan's own buffer, so
    /// a caller that must see every entry of a directory of any size — path
    /// resolution, a listing, the structural check — holds a block rather than
    /// the directory. The name is [`DirScan::name`] once this returns.
    pub(crate) fn dir_next(
        &mut self,
        dir: &Inode,
        scan: &mut DirScan,
    ) -> Result<Option<(u64, u32)>, DriverError> {
        let per = self.dirents_per_block() as u64;
        let blocks = self.dir_block_count(dir);
        scan.entry = None;
        loop {
            let blk = scan.next / per;
            if blk >= blocks {
                return Ok(None);
            }
            if scan.loaded != Some(blk) {
                let ptr = self.block_ptr(dir, blk)?;
                if ptr == 0 {
                    // A hole holds no entries at all, so skip the block whole
                    // rather than its slots one by one.
                    scan.next = (blk + 1) * per;
                    continue;
                }
                self.read_meta(ptr, BlockType::Directory, &mut scan.buf)?;
                scan.loaded = Some(blk);
            }
            let position = scan.next;
            let base = HEADER_LEN + as_usize(scan.next % per) * DIRENT_SIZE;
            scan.next += 1;
            let ino = rd_u32(&scan.buf, base);
            if ino == 0 {
                continue;
            }
            let name_len = as_usize(u64::from(rd_u32(&scan.buf, base + 4)));
            if name_len == 0 || name_len > NAME_MAX {
                return Err(DriverError::DeviceFault);
            }
            scan.entry = Some((base + 8, name_len));
            return Ok(Some((position, ino)));
        }
    }

    /// Resolve `name` within directory `dir`, returning its inode index.
    fn dir_lookup(&mut self, dir: &Inode, name: &[u8]) -> Result<Option<u32>, DriverError> {
        let mut scan = DirScan::new(self.block_size)?;
        while let Some((_, ino)) = self.dir_next(dir, &mut scan)? {
            if scan.name() == name {
                return Ok(Some(ino));
            }
        }
        Ok(None)
    }

    /// Whether `dir` holds no entries other than `.` and `..`.
    fn dir_is_empty(&mut self, dir: &Inode) -> Result<bool, DriverError> {
        let mut scan = DirScan::new(self.block_size)?;
        while self.dir_next(dir, &mut scan)?.is_some() {
            if !scan.is_dot() {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Add directory entry `(child_ino, name)` to directory `dir` (number
    /// `dir_ino`), growing it by one copy-on-write block when every existing
    /// slot is occupied.
    fn add_entry(
        &mut self,
        dir: &mut Inode,
        dir_ino: u32,
        child_ino: u32,
        name: &[u8],
    ) -> Result<(), DriverError> {
        let bs = self.block_size;
        let per = self.dirents_per_block();
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        for blk in 0..self.dir_block_count(dir) {
            let ptr = self.block_ptr(dir, blk)?;
            if ptr == 0 {
                continue;
            }
            self.read_meta(ptr, BlockType::Directory, &mut buf)?;
            for slot in 0..per {
                if rd_u32(&buf, HEADER_LEN + slot * DIRENT_SIZE) == 0 {
                    put_dirent(&mut buf, slot, child_ino, name);
                    let new = self.cow_meta(
                        ptr,
                        &mut buf,
                        BlockType::Directory,
                        u64::from(dir_ino),
                        blk,
                    )?;
                    if new != ptr {
                        self.extent_assign(dir, dir_ino, blk, new)?;
                    }
                    return Ok(());
                }
            }
        }
        let blk_index = self.dir_block_count(dir);
        if blk_index >= self.max_file_blocks() {
            return Err(DriverError::NoSpace);
        }
        for byte in &mut buf[HEADER_LEN..bs] {
            *byte = 0;
        }
        put_dirent(&mut buf, 0, child_ino, name);
        let new_blk = self.cow_meta(
            0,
            &mut buf,
            BlockType::Directory,
            u64::from(dir_ino),
            blk_index,
        )?;
        self.extent_assign(dir, dir_ino, blk_index, new_blk)?;
        dir.size += bs as u64;
        Ok(())
    }

    /// Clear the entry named `name` in directory `dir` (number `dir_ino`),
    /// returning the inode it named.
    fn remove_entry(
        &mut self,
        dir: &mut Inode,
        dir_ino: u32,
        name: &[u8],
    ) -> Result<u32, DriverError> {
        let per = self.dirents_per_block();
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        for blk in 0..self.dir_block_count(dir) {
            let ptr = self.block_ptr(dir, blk)?;
            if ptr == 0 {
                continue;
            }
            self.read_meta(ptr, BlockType::Directory, &mut buf)?;
            for slot in 0..per {
                let base = HEADER_LEN + slot * DIRENT_SIZE;
                let ino = rd_u32(&buf, base);
                if ino == 0 {
                    continue;
                }
                let name_len = rd_u32(&buf, base + 4) as usize;
                if name_len > NAME_MAX {
                    return Err(DriverError::DeviceFault);
                }
                if &buf[base + 8..base + 8 + name_len] == name {
                    wr_u32(&mut buf, base, 0);
                    let new = self.cow_meta(
                        ptr,
                        &mut buf,
                        BlockType::Directory,
                        u64::from(dir_ino),
                        blk,
                    )?;
                    if new != ptr {
                        self.extent_assign(dir, dir_ino, blk, new)?;
                    }
                    return Ok(ino);
                }
            }
        }
        Err(DriverError::NotFound)
    }

    /// Read up to `out.len()` bytes of file `inode` from `offset`.
    fn read_file(
        &mut self,
        inode: &Inode,
        offset: u64,
        out: &mut [u8],
    ) -> Result<usize, DriverError> {
        if offset >= inode.size || out.is_empty() {
            return Ok(0);
        }
        // File offsets map through the data-block content capacity (the block
        // minus its crypto trailer), not the raw device block size.
        let cap = self.data_capacity();
        let end = inode.size.min(offset + out.len() as u64);
        let mut done = 0usize;
        let mut pos = offset;
        let block_size = self.block_size;
        // Blocks this read can cross at most: its content span, plus one for a
        // read that starts inside a block. Sizing the stage to the request
        // keeps a small read from staging the whole run window.
        let span_blocks = as_usize((end - offset).div_ceil(cap)) + 1;
        // Staged on the first raw extent the read actually crosses, so a file
        // served wholly from compressed clusters allocates no run window.
        let mut stage: Option<RunStage> = None;
        while pos < end {
            let bi = pos / cap;
            let within = as_usize(pos % cap);
            match self.extent_lookup(inode, bi)? {
                // A compressed cluster serves everything it covers from one
                // bounded decompression — or, when the host installed a
                // transform cache, from the plaintext retained the last
                // time this cluster was decompressed.
                Some((start, ext)) if ext.compressed => {
                    let cluster_off = as_usize(bi - start) * as_usize(cap) + within;
                    let want = as_usize(end - pos);
                    let mut chunk = None;
                    if let Some(cache) = self.cluster_cache.as_mut() {
                        if let Some(plain) = cache.get(ext.phys) {
                            chunk = Some(xform::copy_from_cluster(
                                plain,
                                cluster_off,
                                &mut out[done..],
                                want,
                            ));
                        }
                    }
                    let chunk = if let Some(chunk) = chunk {
                        chunk
                    } else {
                        let plain = self.read_data_cluster(&ext)?;
                        let chunk =
                            xform::copy_from_cluster(&plain, cluster_off, &mut out[done..], want);
                        if let Some(cache) = self.cluster_cache.as_mut() {
                            cache.put(ext.phys, ext.stored, &plain);
                        }
                        xform::scrub(plain);
                        chunk
                    };
                    if chunk == 0 {
                        // The cluster's plaintext must cover this offset
                        // (`pos < end <= size` inside the extent), so a
                        // zero-byte copy means a wrong-sized entry: fail
                        // closed rather than loop without progress.
                        return Err(DriverError::DeviceFault);
                    }
                    done += chunk;
                    pos += chunk as u64;
                }
                Some((start, ext)) => {
                    // A raw extent maps a contiguous physical run, so fetch
                    // every block of it this read still wants in one device
                    // request, then verify and decrypt each block in place.
                    // The next iteration resumes past the run, so the extent
                    // tree is walked once per run rather than once per block.
                    let stage = stage.get_or_insert_with(|| RunStage::new(block_size, span_blocks));
                    let first = bi - start;
                    let run = (as_usize(end - pos) + within)
                        .div_ceil(as_usize(cap))
                        .min(as_usize(ext.len - first))
                        .min(stage.blocks());
                    self.read_block_run(ext.phys + first, run, stage.buf())?;
                    for slot in 0..run {
                        let block = &mut stage.buf()[slot * block_size..(slot + 1) * block_size];
                        self.verify_raw_block(ext.phys + first + slot as u64, block)
                            .map_err(|_| DriverError::DeviceFault)?;
                        let at = if slot == 0 { within } else { 0 };
                        let chunk = (as_usize(cap) - at).min(as_usize(end - pos));
                        out[done..done + chunk].copy_from_slice(&block[at..at + chunk]);
                        done += chunk;
                        pos += chunk as u64;
                    }
                }
                None => {
                    let chunk = as_usize((cap - within as u64).min(end - pos));
                    for byte in &mut out[done..done + chunk] {
                        *byte = 0;
                    }
                    done += chunk;
                    pos += chunk as u64;
                }
            }
        }
        Ok(done)
    }

    /// Bytes of storage `inode`'s mapped extents occupy: the sum of its
    /// extent-run lengths, in whole blocks. An inode with no extent tree
    /// maps no blocks and occupies nothing. Walks the (bounded) extent
    /// tree, so the cost scales with the file's extent count, never its
    /// byte size.
    fn allocated_bytes(&mut self, inode: &Inode, ino: u32) -> Result<u64, DriverError> {
        if inode.extent_root == 0 {
            return Ok(0);
        }
        let spec = extent_spec(ino);
        let mut walk = TreeWalk::new(self.block_size)?;
        let mut blocks = 0u64;
        while self.btree_next_leaf(inode.extent_root, spec, &mut walk)? {
            for (_, value) in walk.entries() {
                let ext = Extent::decode(value)?;
                blocks = blocks.saturating_add(ext.stored);
            }
        }
        Ok(blocks.saturating_mul(self.block_size as u64))
    }

    /// Copy-on-write `data` into file `inode` (number `ino`) at `offset`.
    ///
    /// A span that covers a whole aligned compression cluster takes the
    /// cluster route ([`store_cluster`](Self::store_cluster)): compressed
    /// into fewer physical blocks when that wins, per-block otherwise. Any
    /// compressed cluster the write only partially covers is first
    /// decomposed back into per-block records (bounded work), then the
    /// ordinary per-block copy-on-write path proceeds.
    fn write_file(
        &mut self,
        inode: &mut Inode,
        ino: u32,
        offset: u64,
        data: &[u8],
    ) -> Result<usize, DriverError> {
        if data.is_empty() {
            return Ok(0);
        }
        let cap = self.data_capacity();
        let capu = as_usize(cap);
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or(DriverError::LengthOutOfRange)?;
        if end.div_ceil(cap) > self.max_file_blocks() {
            return Err(DriverError::LengthOutOfRange);
        }
        let cluster_bytes = capu * as_usize(COMPRESS_CLUSTER_BLOCKS);
        let mut done = 0usize;
        let mut pos = offset;
        let mut blk = [0u8; MAX_BLOCK_SIZE];
        while done < data.len() {
            let bi = pos / cap;
            let within = as_usize(pos % cap);
            if within == 0
                && bi.is_multiple_of(COMPRESS_CLUSTER_BLOCKS)
                && data.len() - done >= cluster_bytes
            {
                self.store_cluster(inode, ino, bi, &data[done..done + cluster_bytes])?;
                done += cluster_bytes;
                pos += cluster_bytes as u64;
                continue;
            }
            if let Some((start, ext)) = self.extent_lookup(inode, bi)? {
                if ext.compressed {
                    self.decompose_cluster(inode, ino, start, &ext)?;
                }
            }
            let chunk = (capu - within).min(data.len() - done);
            let old_ptr = self.block_ptr(inode, bi)?;
            for byte in &mut blk[..capu] {
                *byte = 0;
            }
            if (within != 0 || chunk != capu) && old_ptr != 0 {
                self.read_data_block(old_ptr, &mut blk)?;
            }
            blk[within..within + chunk].copy_from_slice(&data[done..done + chunk]);
            self.store_block(inode, ino, bi, old_ptr, &mut blk)?;
            done += chunk;
            pos += chunk as u64;
        }
        if end > inode.size {
            inode.size = end;
        }
        Ok(done)
    }

    /// Shrink or grow file `inode` (number `ino`) to `size`, freeing whole
    /// truncated runs and copy-on-writing the partial tail block.
    fn truncate_file(&mut self, inode: &mut Inode, ino: u32, size: u64) -> Result<(), DriverError> {
        let cap = self.data_capacity();
        if size.div_ceil(cap) > self.max_file_blocks() {
            return Err(DriverError::LengthOutOfRange);
        }
        if size < inode.size {
            let keep = size.div_ceil(cap);
            // A compressed cluster straddling the cut cannot be trimmed per
            // block: decompose it first, then trim its raw remainder.
            if let Some((start, ext)) = self.extent_lookup(inode, keep)? {
                if ext.compressed && start < keep {
                    self.decompose_cluster(inode, ino, start, &ext)?;
                }
            }
            self.free_extent_tail(inode, ino, keep)?;
            let tail = as_usize(size % cap);
            if tail != 0 {
                let bi = size / cap;
                // A fully kept cluster whose last block holds the partial
                // tail must also be decomposed before that block is
                // rewritten per block.
                if let Some((start, ext)) = self.extent_lookup(inode, bi)? {
                    if ext.compressed {
                        self.decompose_cluster(inode, ino, start, &ext)?;
                    }
                }
                let old_ptr = self.block_ptr(inode, bi)?;
                if old_ptr != 0 {
                    let mut blk = [0u8; MAX_BLOCK_SIZE];
                    self.read_data_block(old_ptr, &mut blk)?;
                    for byte in &mut blk[tail..as_usize(cap)] {
                        *byte = 0;
                    }
                    self.store_block(inode, ino, bi, old_ptr, &mut blk)?;
                }
            }
        }
        inode.size = size;
        Ok(())
    }

    /// Free every block at or beyond logical block `keep` of `inode` (number
    /// `ino`), trimming each extent run-wise rather than block-by-block.
    fn free_extent_tail(
        &mut self,
        inode: &mut Inode,
        ino: u32,
        keep: u64,
    ) -> Result<(), DriverError> {
        let spec = extent_spec(ino);
        // The run covering `keep` is where the tail starts, so the walk skips
        // straight to it instead of reading the map from block zero.
        let begin = self
            .btree_get_floor(inode.extent_root, keep, spec)?
            .map_or(keep, |(start, _)| start);
        let mut walk = TreeWalk::new(self.block_size)?;
        walk.seek(begin);
        // Each step re-descends from the current root, so removing and
        // reinserting a run mid-walk cannot leave the walk on a superseded
        // node. Taking one entry per step is what keeps that true.
        while self.btree_next_leaf(inode.extent_root, spec, &mut walk)? {
            let Some((start, ext)) = walk
                .entries()
                .next()
                .map(|(start, value)| Extent::decode(value).map(|ext| (start, ext)))
                .transpose()?
            else {
                break;
            };
            match start.checked_add(1) {
                Some(next) => walk.seek(next),
                // Nothing follows the largest key a tree can hold.
                None => walk.stop(),
            }
            let end = start + ext.len;
            if end <= keep {
                continue;
            }
            let cut = keep.max(start);
            if ext.compressed {
                // The caller decomposes a straddled cluster before freeing
                // the tail, so a compressed extent here is cut whole; a
                // partial cut would orphan stored blocks. Fail closed.
                if cut > start {
                    return Err(DriverError::DeviceFault);
                }
                self.release_cluster(&ext, ino, start)?;
                inode.extent_root = self.btree_remove(inode.extent_root, start, spec)?;
                continue;
            }
            for b in cut..end {
                self.release_block_ref(ext.phys + (b - start), ino, b)?;
            }
            inode.extent_root = self.btree_remove(inode.extent_root, start, spec)?;
            if cut > start {
                let head = Extent::raw(ext.phys, cut - start).encode();
                inode.extent_root = self.btree_insert(inode.extent_root, start, &head, spec)?;
            }
        }
        Ok(())
    }

    /// Map a [`NodeId`] to a validated inode index.
    fn ino_of(&self, node: NodeId) -> Result<u32, DriverError> {
        let raw = node.raw();
        let ino = u32::try_from(raw).map_err(|_| DriverError::NotFound)?;
        if ino == 0 || u64::from(ino) >= self.next_ino {
            return Err(DriverError::NotFound);
        }
        Ok(ino)
    }

    /// Validate a single path-component name against the rules `ARXFS` shares
    /// with ext4 (`drivers/filesystem/ext4`,
    /// `docs/src/filesystem/arxfs-spec.md` §13):
    ///
    /// * it is non-empty and at most [`NAME_MAX`] (255) bytes long;
    /// * it is neither `.` nor `..` (the VFS owns those);
    /// * it contains no path separator (`/`) and no NUL byte — exactly the two
    ///   bytes ext4 forbids in a name. Every other byte is allowed verbatim,
    ///   and names are compared byte-for-byte, so they are case-sensitive.
    ///
    /// Fails closed: an invalid name is rejected
    /// before any directory state is touched.
    fn check_name(name: &[u8]) -> Result<(), DriverError> {
        if name.is_empty() || name.len() > NAME_MAX {
            return Err(DriverError::LengthOutOfRange);
        }
        if name == b"." || name == b".." {
            return Err(DriverError::Unsupported);
        }
        if name.contains(&b'/') || name.contains(&0u8) {
            return Err(DriverError::Unsupported);
        }
        Ok(())
    }

    /// Replace the security record stored for `node`.
    ///
    /// # Errors
    ///
    /// * [`DriverError::PermissionDenied`] if the handle is read-only — the
    ///   refusal is returned **before** any state is touched, so a read-only
    ///   `/System` mount never dirties the device.
    /// * [`DriverError::NotFound`] if `node` does not name a live inode.
    /// * [`DriverError::DeviceFault`] on an unrecoverable block write.
    pub fn set_security(&mut self, node: NodeId, sec: Security) -> Result<(), DriverError> {
        self.deny_if_read_only()?;
        self.begin();
        let result = self.set_security_inner(node, sec);
        if result.is_err() {
            self.rollback();
        }
        result
    }

    fn set_security_inner(&mut self, node: NodeId, sec: Security) -> Result<(), DriverError> {
        let ino = self.ino_of(node)?;
        let mut inode = self.read_inode(ino)?;
        inode.sec = sec;
        inode.times.changed = (self.clock)();
        self.write_inode(ino, &inode)?;
        self.commit()
    }

    fn create_inner(
        &mut self,
        dir: NodeId,
        name: &[u8],
        kind: NodeKind,
    ) -> Result<NodeId, DriverError> {
        Self::check_name(name)?;
        let now = (self.clock)();
        let dir_ino = self.ino_of(dir)?;
        let mut dir_inode = self.read_inode(dir_ino)?;
        if !dir_inode.is_dir() {
            return Err(DriverError::Unsupported);
        }
        if self.dir_lookup(&dir_inode, name)?.is_some() {
            return Err(DriverError::Busy);
        }
        let (kind_val, mode) = match kind {
            NodeKind::Directory => (InodeKind::Dir, 0o755),
            NodeKind::RegularFile => (InodeKind::File, 0o644),
            // A link carries a target this call has nowhere to put, so it
            // is created only by `create_link`.
            NodeKind::Symlink => return Err(DriverError::Unsupported),
        };
        let mut child = Inode::empty(kind_val, Security::new(mode, 0, 0), now);
        if kind_val == InodeKind::Dir {
            child.nlink = 2;
        }
        let child_ino = self.alloc_inode(&child)?;
        if kind_val == InodeKind::Dir {
            // Insert "." and ".." through the normal insertion path so they
            // span as many directory blocks as the block size needs (a
            // 512-byte block holds only a single 263-byte slot).
            self.add_entry(&mut child, child_ino, child_ino, b".")?;
            self.add_entry(&mut child, child_ino, dir_ino, b"..")?;
            self.write_inode(child_ino, &child)?;
            dir_inode.nlink += 1;
        }
        self.add_entry(&mut dir_inode, dir_ino, child_ino, name)?;
        dir_inode.times.modified = now;
        dir_inode.times.changed = now;
        self.write_inode(dir_ino, &dir_inode)?;
        self.commit()?;
        Ok(NodeId::from_raw(u64::from(child_ino)))
    }

    /// Refuse a node whose data blocks do not hold file bytes.
    ///
    /// A directory's blocks are its entries, and a link's hold its target —
    /// writing either as file content would corrupt structure rather than
    /// data, so both fail closed even though the VFS resolves a final link
    /// before it delegates a write.
    fn deny_non_file_content(kind: InodeKind) -> Result<(), DriverError> {
        match kind {
            InodeKind::File => Ok(()),
            InodeKind::Dir | InodeKind::Link => Err(DriverError::Unsupported),
        }
    }

    /// Create the symbolic link `name` in `dir` holding `target`.
    ///
    /// The target is stored as the node's data through the ordinary file
    /// write path (`docs/src/filesystem/arxfs-spec.md` §20), so it inherits
    /// the volume's checksums, authentication, and encryption with no second
    /// storage path — and the volume declares
    /// [`INCOMPAT_SYMLINKS`](superblock::INCOMPAT_SYMLINKS) in the same
    /// transaction, so a reader that does not know the kind refuses the
    /// volume rather than misreading the link.
    ///
    /// The bytes are stored verbatim: ARXFS neither resolves nor validates
    /// the target as a path (the VFS checked its grammar and bounded its
    /// length), so a link may legitimately dangle.
    fn create_link_inner(
        &mut self,
        dir: NodeId,
        name: &[u8],
        target: &[u8],
    ) -> Result<NodeId, DriverError> {
        Self::check_name(name)?;
        if target.is_empty() || target.len() > FS_SYMLINK_MAX {
            return Err(DriverError::LengthOutOfRange);
        }
        let now = (self.clock)();
        let dir_ino = self.ino_of(dir)?;
        let mut dir_inode = self.read_inode(dir_ino)?;
        if !dir_inode.is_dir() {
            return Err(DriverError::Unsupported);
        }
        if self.dir_lookup(&dir_inode, name)?.is_some() {
            return Err(DriverError::Busy);
        }
        // Declared before the link is minted, so nothing this transaction
        // writes can be reached from a volume that has not admitted to
        // holding links; `rollback` restores it with the rest of the state.
        self.incompat |= superblock::INCOMPAT_SYMLINKS;
        // A link's own mode never gates access — resolution authorises the
        // target — so the conventional world-traversable `lrwxrwxrwx` is
        // stored, which is also what a listing shows.
        let mut child = Inode::empty(InodeKind::Link, Security::new(0o777, 0, 0), now);
        let child_ino = self.alloc_inode(&child)?;
        self.write_file(&mut child, child_ino, 0, target)?;
        self.write_inode(child_ino, &child)?;
        self.add_entry(&mut dir_inode, dir_ino, child_ino, name)?;
        dir_inode.times.modified = now;
        dir_inode.times.changed = now;
        self.write_inode(dir_ino, &dir_inode)?;
        self.commit()?;
        Ok(NodeId::from_raw(u64::from(child_ino)))
    }

    /// Add `name` in `dir` as a second directory entry for the existing node
    /// `node` — a hard link.
    ///
    /// The node gains a name, not a copy: one inode, one extent map, one set
    /// of blocks, reached through two entries. The count that decides when
    /// those blocks are freed is [`Inode::nlink`], which this raises and
    /// [`Self::remove_inner`] lowers, freeing only at zero.
    ///
    /// The volume declares
    /// [`INCOMPAT_HARDLINKS`](superblock::INCOMPAT_HARDLINKS) in the same
    /// transaction, because a reader that did not know about the second name
    /// would free the inode on the first unlink and destroy data the other
    /// name still reaches.
    fn link_inner(&mut self, dir: NodeId, name: &[u8], node: NodeId) -> Result<(), DriverError> {
        Self::check_name(name)?;
        let target_ino = self.ino_of(node)?;
        let mut target = self.read_inode(target_ino)?;
        // A second name for a directory would let the tree hold a cycle, and
        // the VFS's physical `..` walk depends on it being a tree.
        match target.kind {
            InodeKind::File | InodeKind::Link => {}
            InodeKind::Dir => return Err(DriverError::Unsupported),
        }
        let now = (self.clock)();
        let dir_ino = self.ino_of(dir)?;
        let mut dir_inode = self.read_inode(dir_ino)?;
        if !dir_inode.is_dir() {
            return Err(DriverError::Unsupported);
        }
        if self.dir_lookup(&dir_inode, name)?.is_some() {
            return Err(DriverError::Busy);
        }
        // A fixed on-disk field, so an overflow fails closed here: wrapping
        // it would put a live inode one unlink away from being freed.
        target.nlink = target
            .nlink
            .checked_add(1)
            .ok_or(DriverError::TooManyLinks)?;
        // Declared before the entry is written, so nothing this transaction
        // publishes can be reached from a volume that has not admitted to
        // holding more than one name per inode; `rollback` restores it.
        self.incompat |= superblock::INCOMPAT_HARDLINKS;
        self.add_entry(&mut dir_inode, dir_ino, target_ino, name)?;
        // The node's own contents did not change, only the set of names that
        // reach it: POSIX moves `ctime`, never `mtime`.
        target.times.changed = now;
        self.write_inode(target_ino, &target)?;
        dir_inode.times.modified = now;
        dir_inode.times.changed = now;
        self.write_inode(dir_ino, &dir_inode)?;
        self.commit()
    }

    /// Drop one name from `child` (inode `child_ino`), freeing its blocks and
    /// its inode slot only when the name that just went was the last.
    ///
    /// This is the whole hard-link lifecycle. A node with names left keeps
    /// every block it maps — freeing them because *a* name went would destroy
    /// data the remaining names still reach — and records the drop as a
    /// metadata change.
    fn drop_name(
        &mut self,
        child: &mut Inode,
        child_ino: u32,
        now: Time64,
    ) -> Result<(), DriverError> {
        child.nlink = child.nlink.saturating_sub(1);
        if child.nlink > 0 {
            child.times.changed = now;
            return self.write_inode(child_ino, child);
        }
        self.free_all_blocks(child, child_ino)?;
        self.free_inode(child_ino)
    }

    /// Create `dst_name` in `dir` as a reflink of the existing regular file
    /// `src_name`: a copy-on-write clone that **shares** every data block with
    /// the source until one side is written
    /// (`docs/src/filesystem/arxfs-spec.md` §9). Each shared block's chunk is
    /// reference-counted, so a later overwrite of either side copies-on-write a
    /// fresh record and leaves the other intact ([`Self::cow_data`]).
    fn reflink_inner(
        &mut self,
        dir: NodeId,
        src_name: &[u8],
        dst_name: &[u8],
    ) -> Result<NodeId, DriverError> {
        Self::check_name(dst_name)?;
        let now = (self.clock)();
        let dir_ino = self.ino_of(dir)?;
        let mut dir_inode = self.read_inode(dir_ino)?;
        if !dir_inode.is_dir() {
            return Err(DriverError::Unsupported);
        }
        let src_ino = self
            .dir_lookup(&dir_inode, src_name)?
            .ok_or(DriverError::NotFound)?;
        let src = self.read_inode(src_ino)?;
        // A reflink clones data blocks into a fresh regular file, so cloning
        // a link would silently produce a file holding the target's text
        // instead of a second link.
        Self::deny_non_file_content(src.kind)?;
        if self.dir_lookup(&dir_inode, dst_name)?.is_some() {
            return Err(DriverError::Busy);
        }
        let mut dst = Inode::empty(InodeKind::File, src.sec, now);
        let dst_ino = self.alloc_inode(&dst)?;
        // Walk the source's extents: a raw run shares per block, a compressed
        // cluster shares its whole stored run in one reference. Only the
        // destination's tree is written, so the source walk reads a tree
        // nothing is changing under it.
        let src_spec = extent_spec(src_ino);
        let mut walk = TreeWalk::new(self.block_size)?;
        while self.btree_next_leaf(src.extent_root, src_spec, &mut walk)? {
            for (start, value) in walk.entries() {
                let ext = Extent::decode(value)?;
                if ext.compressed {
                    self.clone_cluster_ref(src_ino, &mut dst, dst_ino, start, &ext)?;
                    continue;
                }
                for b in 0..ext.len {
                    self.clone_block_ref(src_ino, &mut dst, dst_ino, start + b, ext.phys + b)?;
                }
            }
        }
        dst.size = src.size;
        // A reflink is a copy, so it carries the source's extended attributes.
        // The destination gets its own attribute block (never a shared
        // pointer), so freeing one inode never frees the other's attributes.
        let attrs = self.read_attrs(&src)?;
        self.write_attrs(&mut dst, dst_ino, &attrs)?;
        self.write_inode(dst_ino, &dst)?;
        self.add_entry(&mut dir_inode, dir_ino, dst_ino, dst_name)?;
        dir_inode.times.modified = now;
        dir_inode.times.changed = now;
        self.write_inode(dir_ino, &dir_inode)?;
        self.commit()?;
        Ok(NodeId::from_raw(u64::from(dst_ino)))
    }

    /// Point logical block `bi` of `dst` at the data block `src_ptr` already
    /// held by `(src_ino, bi)`, sharing the chunk. When that chunk has reached
    /// the reverse-reference cap ([`REVERSE_REF_CAP`]) the block is copied
    /// uniquely for `dst` instead, so the referrer set stays exact and bounded
    /// (`docs/src/filesystem/arxfs-spec.md` §9).
    fn clone_block_ref(
        &mut self,
        src_ino: u32,
        dst: &mut Inode,
        dst_ino: u32,
        bi: u64,
        src_ptr: u64,
    ) -> Result<(), DriverError> {
        let capu = as_usize(self.data_capacity());
        let domain = self.dedupe_domain;
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        self.read_data_block(src_ptr, &mut buf)?;
        let hash = logical_hash(&buf[..capu]);
        if usize::try_from(self.data_refcount(src_ptr)?).unwrap_or(usize::MAX) >= REVERSE_REF_CAP {
            let new_ptr = self.alloc_block(false)?;
            self.write_data_block(new_ptr, &mut buf)?;
            self.extent_assign(dst, dst_ino, bi, new_ptr)?;
            self.index_insert(domain, &hash, new_ptr, dst_ino, bi);
            return Ok(());
        }
        let cand = DedupeCandidate {
            phys: src_ptr,
            inode: src_ino,
            logical: bi,
        };
        self.share_block_ref(cand, dst_ino, bi, domain, &hash)?;
        self.extent_assign(dst, dst_ino, bi, src_ptr)?;
        Ok(())
    }

    fn write_inner(
        &mut self,
        dir: NodeId,
        name: &[u8],
        offset: u64,
        data: &[u8],
    ) -> Result<usize, DriverError> {
        let dir_ino = self.ino_of(dir)?;
        let dir_inode = self.read_inode(dir_ino)?;
        if !dir_inode.is_dir() {
            return Err(DriverError::Unsupported);
        }
        let child_ino = self
            .dir_lookup(&dir_inode, name)?
            .ok_or(DriverError::NotFound)?;
        let mut child = self.read_inode(child_ino)?;
        Self::deny_non_file_content(child.kind)?;
        let written = self.write_file(&mut child, child_ino, offset, data)?;
        let now = (self.clock)();
        child.times.modified = now;
        child.times.changed = now;
        self.write_inode(child_ino, &child)?;
        self.commit()?;
        Ok(written)
    }

    fn truncate_inner(&mut self, dir: NodeId, name: &[u8], size: u64) -> Result<(), DriverError> {
        let dir_ino = self.ino_of(dir)?;
        let dir_inode = self.read_inode(dir_ino)?;
        if !dir_inode.is_dir() {
            return Err(DriverError::Unsupported);
        }
        let child_ino = self
            .dir_lookup(&dir_inode, name)?
            .ok_or(DriverError::NotFound)?;
        let mut child = self.read_inode(child_ino)?;
        Self::deny_non_file_content(child.kind)?;
        self.truncate_file(&mut child, child_ino, size)?;
        let now = (self.clock)();
        child.times.modified = now;
        child.times.changed = now;
        self.write_inode(child_ino, &child)?;
        self.commit()
    }

    fn remove_inner(&mut self, dir: NodeId, name: &[u8]) -> Result<(), DriverError> {
        let dir_ino = self.ino_of(dir)?;
        let mut dir_inode = self.read_inode(dir_ino)?;
        if !dir_inode.is_dir() {
            return Err(DriverError::Unsupported);
        }
        let child_ino = self
            .dir_lookup(&dir_inode, name)?
            .ok_or(DriverError::NotFound)?;
        let mut child = self.read_inode(child_ino)?;
        if child.is_dir() && !self.dir_is_empty(&child)? {
            return Err(DriverError::Busy);
        }
        let now = (self.clock)();
        // An empty directory loses both its name and its own `.`; every other
        // kind loses the one name being removed.
        if child.is_dir() {
            child.nlink = child.nlink.saturating_sub(1);
            dir_inode.nlink = dir_inode.nlink.saturating_sub(1);
        }
        self.drop_name(&mut child, child_ino, now)?;
        self.remove_entry(&mut dir_inode, dir_ino, name)?;
        dir_inode.times.modified = now;
        dir_inode.times.changed = now;
        self.write_inode(dir_ino, &dir_inode)?;
        self.commit()
    }

    /// Whether directory `candidate` is `ancestor` itself or lives anywhere
    /// beneath it, walking parent (`..`) links up to the root. Used to refuse
    /// moving a directory into its own subtree, which would detach the cycle
    /// from the tree.
    fn is_subdir_of(&mut self, mut candidate: u32, ancestor: u32) -> Result<bool, DriverError> {
        loop {
            if candidate == ancestor {
                return Ok(true);
            }
            if candidate == ROOT_INO {
                return Ok(false);
            }
            let inode = self.read_inode(candidate)?;
            let parent = self
                .dir_lookup(&inode, b"..")?
                .ok_or(DriverError::DeviceFault)?;
            if parent == candidate {
                return Ok(false);
            }
            candidate = parent;
        }
    }

    fn rename_inner(
        &mut self,
        src_dir: NodeId,
        src_name: &[u8],
        dst_dir: NodeId,
        dst_name: &[u8],
    ) -> Result<(), DriverError> {
        Self::check_name(dst_name)?;
        let now = (self.clock)();
        let src_dir_ino = self.ino_of(src_dir)?;
        let dst_dir_ino = self.ino_of(dst_dir)?;
        let same_dir = src_dir_ino == dst_dir_ino;

        let mut src_dir_inode = self.read_inode(src_dir_ino)?;
        if !src_dir_inode.is_dir() {
            return Err(DriverError::Unsupported);
        }
        let src_ino = self
            .dir_lookup(&src_dir_inode, src_name)?
            .ok_or(DriverError::NotFound)?;

        // A rename onto the same entry changes nothing.
        if same_dir && src_name == dst_name {
            return Ok(());
        }

        let src_child = self.read_inode(src_ino)?;
        let moving_dir = src_child.is_dir();

        // The destination directory's working inode: `None` means the move is
        // within one directory and both sides mutate `src_dir_inode`.
        let mut dst_dir_inode = if same_dir {
            None
        } else {
            let d = self.read_inode(dst_dir_ino)?;
            if !d.is_dir() {
                return Err(DriverError::Unsupported);
            }
            Some(d)
        };

        // Refuse moving a directory into itself or its own subtree.
        if moving_dir && self.is_subdir_of(dst_dir_ino, src_ino)? {
            return Err(DriverError::Busy);
        }

        // Replace an existing destination, subject to kind compatibility.
        let dst_existing = {
            let dst_ref = dst_dir_inode.as_ref().unwrap_or(&src_dir_inode);
            self.dir_lookup(dst_ref, dst_name)?
        };
        if let Some(dst_ino) = dst_existing {
            if dst_ino == src_ino {
                // Source and destination resolve to the same node already.
                return Ok(());
            }
            let mut dst_child = self.read_inode(dst_ino)?;
            if dst_child.is_dir() != moving_dir {
                return Err(DriverError::Unsupported);
            }
            if dst_child.is_dir() && !self.dir_is_empty(&dst_child)? {
                return Err(DriverError::Busy);
            }
            // The replaced node loses this name like any other unlink: its
            // blocks go only if no other name still reaches them.
            if dst_child.is_dir() {
                dst_child.nlink = dst_child.nlink.saturating_sub(1);
            }
            self.drop_name(&mut dst_child, dst_ino, now)?;
            match &mut dst_dir_inode {
                Some(d) => self.remove_entry(d, dst_dir_ino, dst_name)?,
                None => self.remove_entry(&mut src_dir_inode, dst_dir_ino, dst_name)?,
            };
            if dst_child.is_dir() {
                match &mut dst_dir_inode {
                    Some(d) => d.nlink = d.nlink.saturating_sub(1),
                    None => src_dir_inode.nlink = src_dir_inode.nlink.saturating_sub(1),
                }
            }
        }

        // Detach the source name; add the destination name in its place.
        self.remove_entry(&mut src_dir_inode, src_dir_ino, src_name)?;
        if moving_dir {
            src_dir_inode.nlink = src_dir_inode.nlink.saturating_sub(1);
        }
        match &mut dst_dir_inode {
            Some(d) => self.add_entry(d, dst_dir_ino, src_ino, dst_name)?,
            None => self.add_entry(&mut src_dir_inode, dst_dir_ino, src_ino, dst_name)?,
        }
        if moving_dir {
            match &mut dst_dir_inode {
                Some(d) => d.nlink += 1,
                None => src_dir_inode.nlink += 1,
            }
        }

        // Repoint the moved directory's `..` at its new parent.
        let mut moved = src_child;
        if moving_dir && !same_dir {
            self.remove_entry(&mut moved, src_ino, b"..")?;
            self.add_entry(&mut moved, src_ino, dst_dir_ino, b"..")?;
        }
        moved.times.changed = now;
        self.write_inode(src_ino, &moved)?;

        src_dir_inode.times.modified = now;
        src_dir_inode.times.changed = now;
        self.write_inode(src_dir_ino, &src_dir_inode)?;
        if let Some(mut d) = dst_dir_inode {
            d.times.modified = now;
            d.times.changed = now;
            self.write_inode(dst_dir_ino, &d)?;
        }
        self.commit()
    }

    /// Bytes available for an encoded attribute set inside one metadata block:
    /// the block payload between the header and the crypto trailer. An encoded
    /// set larger than this does not fit a single block and is refused with
    /// [`DriverError::NoSpace`], the fixed-block-size consequence of the
    /// `lib/fsmeta` value bound (`docs/src/filesystem/arxfs-spec.md` §21).
    fn attr_capacity(&self) -> usize {
        self.crypto_trailer_offset() - HEADER_LEN
    }

    /// Read `inode`'s extended-attribute set, or an empty set when it carries
    /// none. The attribute block is authenticated and decrypted through the
    /// same redundant, repair-on-read metadata path as every other metadata
    /// block ([`Self::read_meta`]); its decrypted payload is then decoded by
    /// the shared `lib/fsmeta` decoder, which fails closed on a malformed or
    /// out-of-bounds encoding.
    fn read_attrs(&mut self, inode: &Inode) -> Result<AttrSet, DriverError> {
        if inode.attr_root == 0 {
            return Ok(AttrSet::new());
        }
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        self.read_meta(inode.attr_root, BlockType::Attr, &mut buf)?;
        let end = self.crypto_trailer_offset();
        AttrSet::decode(&buf[HEADER_LEN..end]).map_err(DriverError::from)
    }

    /// Store `set` as `inode`'s extended-attribute set in one copy-on-write
    /// transaction, updating `inode.attr_root` in place (the caller persists
    /// the inode). An empty set frees the attribute block and clears the
    /// pointer, so an inode with no attributes holds none on disk. Fails
    /// closed with [`DriverError::NoSpace`] if the encoded set does not fit one
    /// metadata block ([`Self::attr_capacity`]).
    fn write_attrs(
        &mut self,
        inode: &mut Inode,
        ino: u32,
        set: &AttrSet,
    ) -> Result<(), DriverError> {
        if set.is_empty() {
            if inode.attr_root != 0 {
                self.free_meta(inode.attr_root);
                inode.attr_root = 0;
            }
            return Ok(());
        }
        let encoded = set.encode();
        if encoded.len() > self.attr_capacity() {
            return Err(DriverError::NoSpace);
        }
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        buf[HEADER_LEN..HEADER_LEN + encoded.len()].copy_from_slice(&encoded);
        let new = self.cow_meta(
            inode.attr_root,
            &mut buf,
            BlockType::Attr,
            u64::from(ino),
            0,
        )?;
        inode.attr_root = new;
        Ok(())
    }

    /// Set attribute `key` on `node` to `value` in one copy-on-write
    /// transaction. The `lib/fsmeta` grammar and bounds are validated by
    /// [`AttrSet::set`]; an unknown namespace, malformed key, oversize value,
    /// or exhausted per-inode budget is rejected fail-closed before anything
    /// is written.
    fn set_attr_inner(
        &mut self,
        node: NodeId,
        key: &[u8],
        value: &[u8],
    ) -> Result<(), DriverError> {
        let ino = self.ino_of(node)?;
        let mut inode = self.read_inode(ino)?;
        let mut set = self.read_attrs(&inode)?;
        set.set(key, AttrFlags::empty(), value)
            .map_err(DriverError::from)?;
        self.write_attrs(&mut inode, ino, &set)?;
        inode.times.changed = (self.clock)();
        self.write_inode(ino, &inode)?;
        self.commit()
    }

    /// Remove attribute `key` from `node` in one copy-on-write transaction,
    /// failing closed with [`DriverError::NotFound`] when the key is absent.
    fn remove_attr_inner(&mut self, node: NodeId, key: &[u8]) -> Result<(), DriverError> {
        AttrKey::parse(key).map_err(DriverError::from)?;
        let ino = self.ino_of(node)?;
        let mut inode = self.read_inode(ino)?;
        let mut set = self.read_attrs(&inode)?;
        if !set.remove(key) {
            return Err(DriverError::NotFound);
        }
        self.write_attrs(&mut inode, ino, &set)?;
        inode.times.changed = (self.clock)();
        self.write_inode(ino, &inode)?;
        self.commit()
    }
}

/// Draw a random, non-zero per-volume filesystem UUID from the platform RNG
/// seam. The value only needs to be unique and non-zero to anchor the
/// block-identity checks within one volume; an all-zero draw (which the
/// checks reserve as "no UUID") is nudged to a non-zero value rather than
/// retried, since any non-zero anchor satisfies the invariant.
///
/// # Errors
///
/// Propagates the [`EntropySource`] error if the random draw is unavailable,
/// so a volume is never anchored on predictable identity material.
fn random_uuid(entropy: &mut dyn EntropySource) -> Result<u128, DriverError> {
    let mut bytes = [0u8; 16];
    entropy.fill(&mut bytes)?;
    let uuid = u128::from_le_bytes(bytes);
    Ok(if uuid == 0 { 1 } else { uuid })
}

impl<B: Block> FilesystemRead for ARXFS<B> {
    fn root(&self) -> NodeId {
        NodeId::from_raw(u64::from(ROOT_INO))
    }

    fn node_info(&mut self, node: NodeId) -> Result<NodeInfo, DriverError> {
        let ino = self.ino_of(node)?;
        let inode = self.read_inode(ino)?;
        self.inode_info(ino, &inode)
    }

    fn lookup(&mut self, dir: NodeId, name: &[u8]) -> Result<NodeId, DriverError> {
        let dir_ino = self.ino_of(dir)?;
        let dir_inode = self.read_inode(dir_ino)?;
        if !dir_inode.is_dir() {
            return Err(DriverError::Unsupported);
        }
        let child = self
            .dir_lookup(&dir_inode, name)?
            .ok_or(DriverError::NotFound)?;
        Ok(NodeId::from_raw(u64::from(child)))
    }

    fn read_at(&mut self, file: NodeId, offset: u64, buf: &mut [u8]) -> Result<usize, DriverError> {
        let ino = self.ino_of(file)?;
        let inode = self.read_inode(ino)?;
        match inode.kind {
            InodeKind::File => self.read_file(&inode, offset, buf),
            // A directory's content is its entries; a link's is a path,
            // reached only through `read_link`. Neither is a byte stream.
            InodeKind::Dir | InodeKind::Link => Err(DriverError::Unsupported),
        }
    }

    fn read_link(&mut self, link: NodeId, out: &mut [u8]) -> Result<usize, DriverError> {
        let ino = self.ino_of(link)?;
        let inode = self.read_inode(ino)?;
        match inode.kind {
            InodeKind::Link => {}
            InodeKind::Dir | InodeKind::File => return Err(DriverError::Unsupported),
        }
        // The target is the node's whole content, so its recorded length is
        // its length; a buffer that cannot hold it is refused rather than
        // handed a truncated path.
        let len = as_usize(inode.size);
        if len == 0 || len > FS_SYMLINK_MAX {
            return Err(DriverError::DeviceFault);
        }
        if out.len() < len {
            return Err(DriverError::BufferTooSmall);
        }
        if self.read_file(&inode, 0, &mut out[..len])? != len {
            // The extent map holds fewer bytes than the inode claims: the
            // link is damaged, so refuse rather than resolve a partial path.
            return Err(DriverError::DeviceFault);
        }
        Ok(len)
    }

    fn read_dir(
        &mut self,
        dir: NodeId,
        cursor: u64,
        name_out: &mut [u8],
    ) -> Result<Option<DirEntry>, DriverError> {
        let dir_ino = self.ino_of(dir)?;
        let dir_inode = self.read_inode(dir_ino)?;
        if !dir_inode.is_dir() {
            return Err(DriverError::Unsupported);
        }
        // The cursor is the entry's global slot position, so resumption seeks
        // straight past the previously returned entry instead of rescanning
        // the whole directory per call. Any cursor past the last block —
        // including an arbitrary value that was never returned — ends the
        // listing (fail closed, never out of bounds).
        let mut scan = DirScan::new(self.block_size)?;
        scan.seek(cursor);
        while let Some((position, ino)) = self.dir_next(&dir_inode, &mut scan)? {
            if scan.is_dot() {
                continue;
            }
            let name = scan.name();
            let name_len = name.len();
            if name_out.len() < name_len {
                return Err(DriverError::BufferTooSmall);
            }
            name_out[..name_len].copy_from_slice(name);
            // The child inode is read once here and its metadata returned with
            // the entry, so a listing consumer never re-resolves the child by
            // path to learn its sizes.
            let child = self.read_inode(ino)?;
            let child_info = self.inode_info(ino, &child)?;
            return Ok(Some(DirEntry {
                node: NodeId::from_raw(u64::from(ino)),
                info: child_info,
                name_len,
                next_cursor: position + 1,
            }));
        }
        Ok(None)
    }
}

impl<B: Block> FilesystemWrite for ARXFS<B> {
    fn create(&mut self, dir: NodeId, name: &[u8], kind: NodeKind) -> Result<NodeId, DriverError> {
        self.deny_if_read_only()?;
        self.begin();
        let result = self.create_inner(dir, name, kind);
        if result.is_err() {
            self.rollback();
        }
        result
    }

    fn create_link(
        &mut self,
        dir: NodeId,
        name: &[u8],
        target: &[u8],
    ) -> Result<NodeId, DriverError> {
        self.deny_if_read_only()?;
        self.begin();
        let result = self.create_link_inner(dir, name, target);
        if result.is_err() {
            self.rollback();
        }
        result
    }

    fn link(&mut self, dir: NodeId, name: &[u8], node: NodeId) -> Result<(), DriverError> {
        self.deny_if_read_only()?;
        self.begin();
        let result = self.link_inner(dir, name, node);
        if result.is_err() {
            self.rollback();
        }
        result
    }

    fn write_at(
        &mut self,
        dir: NodeId,
        name: &[u8],
        offset: u64,
        data: &[u8],
    ) -> Result<usize, DriverError> {
        self.deny_if_read_only()?;
        self.begin();
        let result = self.write_inner(dir, name, offset, data);
        if result.is_err() {
            self.rollback();
        }
        result
    }

    fn truncate(&mut self, dir: NodeId, name: &[u8], size: u64) -> Result<(), DriverError> {
        self.deny_if_read_only()?;
        self.begin();
        let result = self.truncate_inner(dir, name, size);
        if result.is_err() {
            self.rollback();
        }
        result
    }

    fn remove(&mut self, dir: NodeId, name: &[u8]) -> Result<(), DriverError> {
        self.deny_if_read_only()?;
        self.begin();
        let result = self.remove_inner(dir, name);
        if result.is_err() {
            self.rollback();
        }
        result
    }

    fn rename(
        &mut self,
        src_dir: NodeId,
        src_name: &[u8],
        dst_dir: NodeId,
        dst_name: &[u8],
    ) -> Result<(), DriverError> {
        self.deny_if_read_only()?;
        self.begin();
        let result = self.rename_inner(src_dir, src_name, dst_dir, dst_name);
        if result.is_err() {
            self.rollback();
        }
        result
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        // Every mutating operation already copy-on-writes its data and
        // metadata to the device and publishes the transaction atomically
        // through the superblock ring, so there is nothing buffered *here*
        // to write out. What is not yet guaranteed durable is the device's
        // own volatile write cache: force it to stable media so a caller's
        // `fs_sync` is a real durability barrier, not a no-op. A read-only
        // handle never wrote, so it has nothing to commit.
        if self.read_only {
            return Ok(());
        }
        // The allocation map is rebuildable, so ordinary commits leave it
        // dirty on the device and exact only in RAM. An explicit sync is the
        // point at which it is worth writing out and stamping clean, so the
        // next mount can adopt it instead of walking the volume; a crash
        // between syncs simply costs that walk. The map's own persist forces
        // the device cache, which is this sync's durability barrier too.
        self.map_persist()
    }
}

impl<B: Block> FilesystemSecurity for ARXFS<B> {
    fn security(&mut self, node: NodeId) -> Result<Security, DriverError> {
        let ino = self.ino_of(node)?;
        Ok(self.read_inode(ino)?.sec)
    }

    fn set_security(&mut self, node: NodeId, security: Security) -> Result<(), DriverError> {
        ARXFS::set_security(self, node, security)
    }
}

impl<B: Block> FilesystemStats for ARXFS<B> {
    fn stats(&mut self) -> Result<VolumeStats, DriverError> {
        // A pure read of the mounted volume's in-memory accounting — no
        // device I/O, so it cannot fault. Data allocation stops at the
        // metadata reserve, so the blocks an ordinary write may still
        // consume exclude it. Inodes are B-tree records allocated on
        // demand: there is no fixed table, and the zero pair reports that
        // honestly rather than fabricating a capacity.
        Ok(VolumeStats {
            block_size: as_u32(self.block_size),
            total_blocks: self.total_blocks,
            free_blocks: self.free_count,
            avail_blocks: self.free_count.saturating_sub(METADATA_RESERVE),
            files: 0,
            files_free: 0,
        })
    }
}

impl<B: Block> FilesystemAttrsProvider for ARXFS<B> {
    /// `ARXFS` stores a per-inode attribute set as first-class metadata,
    /// so the mounted volume serves the `fs_attr_*` surface itself.
    fn attrs_fs(&mut self) -> Option<&mut dyn FilesystemAttrsFs> {
        Some(self)
    }
}

impl<B: Block> FilesystemAttrs for ARXFS<B> {
    fn get_attr(
        &mut self,
        node: NodeId,
        key: &[u8],
        value_out: &mut [u8],
    ) -> Result<Option<usize>, DriverError> {
        // Reject a malformed or unknown-namespace key up front, so a bad key
        // is a fail-closed rejection rather than an "absent" result.
        AttrKey::parse(key).map_err(DriverError::from)?;
        let ino = self.ino_of(node)?;
        let inode = self.read_inode(ino)?;
        let set = self.read_attrs(&inode)?;
        match set.get(key) {
            Some(value) => {
                if value_out.len() < value.len() {
                    return Err(DriverError::BufferTooSmall);
                }
                value_out[..value.len()].copy_from_slice(value);
                Ok(Some(value.len()))
            }
            None => Ok(None),
        }
    }

    fn set_attr(&mut self, node: NodeId, key: &[u8], value: &[u8]) -> Result<(), DriverError> {
        self.deny_if_read_only()?;
        self.begin();
        let result = self.set_attr_inner(node, key, value);
        if result.is_err() {
            self.rollback();
        }
        result
    }

    fn list_attr(
        &mut self,
        node: NodeId,
        index: u64,
        key_out: &mut [u8],
    ) -> Result<Option<usize>, DriverError> {
        let ino = self.ino_of(node)?;
        let inode = self.read_inode(ino)?;
        let set = self.read_attrs(&inode)?;
        let idx = usize::try_from(index).unwrap_or(usize::MAX);
        let Some(entry) = set.iter().nth(idx) else {
            return Ok(None);
        };
        let key = entry.key().as_bytes();
        if key_out.len() < key.len() {
            return Err(DriverError::BufferTooSmall);
        }
        key_out[..key.len()].copy_from_slice(key);
        Ok(Some(key.len()))
    }

    fn remove_attr(&mut self, node: NodeId, key: &[u8]) -> Result<(), DriverError> {
        self.deny_if_read_only()?;
        self.begin();
        let result = self.remove_attr_inner(node, key);
        if result.is_err() {
            self.rollback();
        }
        result
    }
}
