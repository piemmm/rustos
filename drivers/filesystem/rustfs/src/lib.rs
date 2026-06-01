//! RustOS native filesystem driver (`rustfs`).
//!
//! `rustfs` is the native RustOS filesystem: a block-backed, journaled,
//! copy-on-write filesystem that stores full POSIX metadata plus an
//! inline access-control list and an optional capability gate **per
//! inode** (`AGENTS.md` §5.3). It sits behind any
//! [`rustos_abi::driver::block::Block`] device and exposes itself through
//! the versioned [`FilesystemRead`] and [`FilesystemWrite`]
//! surfaces (`AGENTS.md` §2.4 / §9 — new behaviour ships as a new trait,
//! never by widening the frozen mount/unmount
//! [`Filesystem`](rustos_abi::driver::filesystem::Filesystem)).
//!
//! # Crash consistency
//!
//! Metadata updates (the superblock, the data-block bitmap, inode-table
//! blocks, and directory blocks) are applied through a physical
//! redo-log journal: a transaction stages its modified block images into
//! the journal, writes a checksummed commit record, and only then writes
//! the images to their home locations. A mount replays a committed but
//! un-checkpointed transaction and discards an uncommitted one, so a
//! crash leaves the metadata at a transaction boundary, never half-way.
//! File *data* is written copy-on-write — a modified block is written to
//! a freshly allocated block and the inode is re-pointed in the same
//! transaction — so a crash never exposes a torn data block.
//!
//! # Permissions
//!
//! The driver *stores* each inode's owner, mode, ACL, and capability
//! gate, but makes **no** permission decision itself: the VFS is the
//! policy point (`AGENTS.md` §5.4). The stored security record is
//! surfaced to the host through [`RustFs::security`] /
//! [`RustFs::set_security`].
//!
//! # Public surface
//!
//! Per `AGENTS.md` §8 the only public *function* is [`register`].
//! [`RustFs`] is a public *type* the driver host instantiates with
//! [`RustFs::format`] / [`RustFs::open`].
//!
//! # Capabilities
//!
//! Loading requires
//! [`CapabilityId::DRV_LOAD`](rustos_abi::CapabilityId::DRV_LOAD). The
//! driver runs in user space; it does not request `CAP_DRV_KERNEL`
//! (`AGENTS.md` §4 / §8).

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use rustos_abi::driver::block::Block;
use rustos_abi::driver::filesystem::{
    DirEntry, FilesystemRead, FilesystemSecurity, FilesystemTimestamps, FilesystemWrite, NodeId,
    NodeInfo, NodeKind, NodeTimes,
};
pub use rustos_abi::driver::filesystem::{
    NodeSecurity as Security, SecurityAcl as AclEntry, SecuritySubject as AclSubject,
};
use rustos_abi::time::Time64;
use rustos_abi::{CapabilityId, DriverError, DriverHandle, DriverHost};

#[cfg(test)]
mod tests;

/// Per-driver `DriverHandle` marker returned by [`register`].
const REGISTER_HANDLE_MARKER: u64 = 0x5275_7374_4653_0001; // "RustFS" + index

/// Driver entry point (`AGENTS.md` §8).
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

/// Magic in the superblock's first eight bytes: `"RUSTFS\0\1"`.
const SUPERBLOCK_MAGIC: u64 = 0x5255_5354_4653_0001;
/// On-disk format version understood by this build.
///
/// Version 2 added the four §21 [`Time64`] timestamps to each inode
/// (`created`/`modified`/`accessed`/`changed`), which reshaped the inode
/// record; a version-1 volume is refused rather than misread.
const FORMAT_VERSION: u32 = 2;

/// Largest block size the driver stages through its on-stack scratch
/// buffers. No Tier-1 block device exceeds 4096 bytes per block.
const MAX_BLOCK_SIZE: usize = 4096;
/// Smallest block size the format supports (must hold one inode).
const MIN_BLOCK_SIZE: usize = 512;

/// Fixed on-disk size of one inode record, in bytes.
const INODE_SIZE: usize = 256;
/// Fixed on-disk size of one directory slot, in bytes.
const DIRENT_SIZE: usize = 64;
/// Bytes available for a name inside a directory slot.
const NAME_MAX: usize = DIRENT_SIZE - 8;

/// Number of direct block pointers stored inline in an inode.
///
/// Reduced from 16 to 12 in format version 2 to make room for the four
/// §21 [`Time64`] timestamps inside the fixed 256-byte inode record.
const DIRECT_PTRS: usize = 12;
/// Maximum number of inline ACL entries stored in an inode.
///
/// The on-disk inode reserves room for exactly this many entries; it must
/// equal the ABI record's [`MAX_ACL_ENTRIES`](rustos_abi::driver::filesystem::MAX_ACL_ENTRIES)
/// so a full on-disk ACL round-trips through [`Security`] without loss.
const ACL_MAX: usize = 8;
const _: () = assert!(ACL_MAX == rustos_abi::driver::filesystem::MAX_ACL_ENTRIES);

/// Inode-table index of the root directory. Index 0 is reserved as the
/// "no inode" sentinel so that a zeroed directory slot reads as free.
const ROOT_INO: u32 = 1;

/// `used` marker stored in a live inode's first word.
const INODE_USED: u32 = 0x494E_4F44; // "INOD"

/// On-disk inode `kind` value for a directory.
const KIND_DIR: u32 = 1;
/// On-disk inode `kind` value for a regular file.
const KIND_FILE: u32 = 2;

// ---------------------------------------------------------------------------
// Little-endian field accessors over a byte slice.
//
// Each reads/writes a fixed-width field at a byte offset. They are total
// over an in-bounds offset; callers only ever address fields inside a
// scratch buffer they sized, so a slice index can never be out of range.
// ---------------------------------------------------------------------------

fn rd_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

fn rd_u64(buf: &[u8], off: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&buf[off..off + 8]);
    u64::from_le_bytes(bytes)
}

fn wr_u32(buf: &mut [u8], off: usize, value: u32) {
    buf[off..off + 4].copy_from_slice(&value.to_le_bytes());
}

fn wr_u64(buf: &mut [u8], off: usize, value: u64) {
    buf[off..off + 8].copy_from_slice(&value.to_le_bytes());
}

/// Write a [`Time64`] at `off` (12 bytes: 8-byte seconds + 4-byte nanos).
fn wr_time(buf: &mut [u8], off: usize, value: Time64) {
    buf[off..off + Time64::WIRE_LEN].copy_from_slice(&value.to_le_bytes());
}

/// Read a [`Time64`] at `off`. A non-canonical on-disk encoding (a
/// sub-second field at or above one second) is treated as corruption.
fn rd_time(buf: &[u8], off: usize) -> Result<Time64, DriverError> {
    Time64::from_bytes(&buf[off..off + Time64::WIRE_LEN]).map_err(|_| DriverError::DeviceFault)
}

/// Narrow a `u64` to a `usize` without an `as` cast. Every caller passes
/// a value already bounded by a validated block size or block index, so
/// the saturating fall-back is never reached on a 64-bit target and is a
/// safe ceiling on a 32-bit one.
fn as_usize(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

/// Narrow a `usize` to a `u32` without an `as` cast. Callers pass small,
/// bounded counts (ACL length, block size, name length, transaction
/// length), so the fall-back ceiling is never reached.
fn as_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

// Inode field byte offsets within a 256-byte record (format version 2).
const I_USED: usize = 0;
const I_KIND: usize = 4;
const I_MODE: usize = 8;
const I_UID: usize = 12;
const I_GID: usize = 16;
const I_NLINK: usize = 20;
const I_REQCAP: usize = 24;
const I_ACLCOUNT: usize = 28;
const I_SIZE: usize = 32;
// The four §21 timestamps, each a 12-byte Time64, occupy bytes 40..88.
const I_CREATED: usize = 40;
const I_MODIFIED: usize = 52;
const I_ACCESSED: usize = 64;
const I_CHANGED: usize = 76;
const I_ACL_BASE: usize = 88;
const I_ACL_STRIDE: usize = 8;
const I_DIRECT_BASE: usize = 152;
const I_INDIRECT: usize = 248;

/// In-memory image of one on-disk inode.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Inode {
    kind: u32,
    sec: Security,
    nlink: u32,
    size: u64,
    times: NodeTimes,
    direct: [u64; DIRECT_PTRS],
    indirect: u64,
}

impl Inode {
    fn empty(kind: u32, sec: Security, now: Time64) -> Self {
        Self {
            kind,
            sec,
            nlink: 1,
            size: 0,
            times: NodeTimes {
                created: now,
                modified: now,
                accessed: now,
                changed: now,
            },
            direct: [0; DIRECT_PTRS],
            indirect: 0,
        }
    }

    fn is_dir(&self) -> bool {
        self.kind == KIND_DIR
    }

    fn decode(buf: &[u8]) -> Result<Option<Self>, DriverError> {
        if rd_u32(buf, I_USED) != INODE_USED {
            return Ok(None);
        }
        let kind = rd_u32(buf, I_KIND);
        if kind != KIND_DIR && kind != KIND_FILE {
            return Err(DriverError::DeviceFault);
        }
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
            accessed: rd_time(buf, I_ACCESSED)?,
            changed: rd_time(buf, I_CHANGED)?,
        };
        let mut direct = [0u64; DIRECT_PTRS];
        for (i, slot) in direct.iter_mut().enumerate() {
            *slot = rd_u64(buf, I_DIRECT_BASE + i * 8);
        }
        Ok(Some(Self {
            kind,
            sec,
            nlink: rd_u32(buf, I_NLINK),
            size: rd_u64(buf, I_SIZE),
            times,
            direct,
            indirect: rd_u64(buf, I_INDIRECT),
        }))
    }

    fn encode(&self, buf: &mut [u8]) {
        for byte in buf.iter_mut().take(INODE_SIZE) {
            *byte = 0;
        }
        wr_u32(buf, I_USED, INODE_USED);
        wr_u32(buf, I_KIND, self.kind);
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
        wr_time(buf, I_ACCESSED, self.times.accessed);
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
        for (i, ptr) in self.direct.iter().enumerate() {
            wr_u64(buf, I_DIRECT_BASE + i * 8, *ptr);
        }
        wr_u64(buf, I_INDIRECT, self.indirect);
    }
}

// Superblock field byte offsets within block 0.
const S_MAGIC: usize = 0;
const S_VERSION: usize = 8;
const S_BLOCK_SIZE: usize = 12;
const S_TOTAL_BLOCKS: usize = 16;
const S_INODE_COUNT: usize = 24;
const S_INODE_START: usize = 32;
const S_INODE_BLOCKS: usize = 40;
const S_BITMAP_START: usize = 48;
const S_BITMAP_BLOCKS: usize = 56;
const S_JOURNAL_START: usize = 64;
const S_JOURNAL_BLOCKS: usize = 72;
const S_DATA_START: usize = 80;
const S_DATA_BLOCKS: usize = 88;
const S_ROOT_INO: usize = 96;

/// Validated geometry of a mounted volume, derived once at
/// [`RustFs::open`] / [`RustFs::format`] time and never mutated after.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Layout {
    block_size: usize,
    total_blocks: u64,
    inode_count: u32,
    inode_start: u64,
    inode_blocks: u64,
    bitmap_start: u64,
    bitmap_blocks: u64,
    journal_start: u64,
    journal_blocks: u64,
    data_start: u64,
    data_blocks: u64,
    root_ino: u32,
}

impl Layout {
    fn inodes_per_block(&self) -> u32 {
        as_u32(self.block_size / INODE_SIZE)
    }

    fn ptrs_per_block(&self) -> usize {
        self.block_size / 8
    }

    fn dirents_per_block(&self) -> usize {
        self.block_size / DIRENT_SIZE
    }

    /// Block holding inode `ino`, and the byte offset of its record
    /// within that block. Inode index 0 is the reserved sentinel.
    fn inode_loc(&self, ino: u32) -> Option<(u64, usize)> {
        if ino == 0 || ino >= self.inode_count {
            return None;
        }
        let per = self.inodes_per_block();
        let block = self.inode_start + u64::from(ino / per);
        let offset = (ino % per) as usize * INODE_SIZE;
        Some((block, offset))
    }

    fn encode(&self, buf: &mut [u8]) {
        for byte in buf.iter_mut() {
            *byte = 0;
        }
        wr_u64(buf, S_MAGIC, SUPERBLOCK_MAGIC);
        wr_u32(buf, S_VERSION, FORMAT_VERSION);
        wr_u32(buf, S_BLOCK_SIZE, as_u32(self.block_size));
        wr_u64(buf, S_TOTAL_BLOCKS, self.total_blocks);
        wr_u32(buf, S_INODE_COUNT, self.inode_count);
        wr_u64(buf, S_INODE_START, self.inode_start);
        wr_u64(buf, S_INODE_BLOCKS, self.inode_blocks);
        wr_u64(buf, S_BITMAP_START, self.bitmap_start);
        wr_u64(buf, S_BITMAP_BLOCKS, self.bitmap_blocks);
        wr_u64(buf, S_JOURNAL_START, self.journal_start);
        wr_u64(buf, S_JOURNAL_BLOCKS, self.journal_blocks);
        wr_u64(buf, S_DATA_START, self.data_start);
        wr_u64(buf, S_DATA_BLOCKS, self.data_blocks);
        wr_u32(buf, S_ROOT_INO, self.root_ino);
    }

    fn decode(buf: &[u8]) -> Result<Self, DriverError> {
        if rd_u64(buf, S_MAGIC) != SUPERBLOCK_MAGIC {
            return Err(DriverError::BadMagic);
        }
        if rd_u32(buf, S_VERSION) != FORMAT_VERSION {
            return Err(DriverError::BadMagic);
        }
        let block_size = rd_u32(buf, S_BLOCK_SIZE) as usize;
        if !(MIN_BLOCK_SIZE..=MAX_BLOCK_SIZE).contains(&block_size) || !block_size.is_power_of_two()
        {
            return Err(DriverError::BadMagic);
        }
        let layout = Self {
            block_size,
            total_blocks: rd_u64(buf, S_TOTAL_BLOCKS),
            inode_count: rd_u32(buf, S_INODE_COUNT),
            inode_start: rd_u64(buf, S_INODE_START),
            inode_blocks: rd_u64(buf, S_INODE_BLOCKS),
            bitmap_start: rd_u64(buf, S_BITMAP_START),
            bitmap_blocks: rd_u64(buf, S_BITMAP_BLOCKS),
            journal_start: rd_u64(buf, S_JOURNAL_START),
            journal_blocks: rd_u64(buf, S_JOURNAL_BLOCKS),
            data_start: rd_u64(buf, S_DATA_START),
            data_blocks: rd_u64(buf, S_DATA_BLOCKS),
            root_ino: rd_u32(buf, S_ROOT_INO),
        };
        layout.validate()?;
        Ok(layout)
    }

    /// Confirm the regions tile the device in order and do not overlap.
    fn validate(&self) -> Result<(), DriverError> {
        if self.root_ino != ROOT_INO || self.inode_count < 2 {
            return Err(DriverError::BadMagic);
        }
        if self.inodes_per_block() == 0 {
            return Err(DriverError::BadMagic);
        }
        let expect_inode_blocks =
            u64::from(self.inode_count).div_ceil(u64::from(self.inodes_per_block()));
        if self.inode_start != 1
            || self.inode_blocks != expect_inode_blocks
            || self.bitmap_start != self.inode_start + self.inode_blocks
            || self.journal_start != self.bitmap_start + self.bitmap_blocks
            || self.data_start != self.journal_start + self.journal_blocks
            || self.data_start + self.data_blocks != self.total_blocks
        {
            return Err(DriverError::BadMagic);
        }
        let bits_per_block = (self.block_size * 8) as u64;
        if self.data_blocks.div_ceil(bits_per_block) != self.bitmap_blocks {
            return Err(DriverError::BadMagic);
        }
        Ok(())
    }
}

/// Journal header magic (`"RFSJRNL\1"`).
const JOURNAL_MAGIC: u64 = 0x5246_534A_524E_4C01;
/// Journal header byte offsets within the journal's first block.
const J_MAGIC: usize = 0;
const J_STATE: usize = 8;
const J_COUNT: usize = 12;
const J_CHECKSUM: usize = 16;
const J_TARGETS_BASE: usize = 32;
/// Journal state: no transaction awaiting checkpoint.
const J_STATE_EMPTY: u32 = 0;
/// Journal state: a committed transaction must be replayed to its homes.
const J_STATE_COMMITTED: u32 = 1;

/// Maximum number of distinct metadata blocks one transaction batches.
/// A single high-level operation touches far fewer (a handful of inode,
/// bitmap, directory, and indirect blocks); the journal data area is
/// sized to hold this many.
const MAX_TXN_BLOCKS: usize = 16;
/// Blocks reserved for the journal: one header plus the data area.
const JOURNAL_BLOCKS: u64 = MAX_TXN_BLOCKS as u64 + 1;

/// FNV-1a 64-bit hash, used to checksum a committed journal record so a
/// torn header write is detected and discarded on replay.
fn fnv1a(seed: u64, bytes: &[u8]) -> u64 {
    let mut hash = seed;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash
}

/// The set of home blocks whose new images are staged in the on-disk
/// journal data area for the current transaction.
///
/// Only the home block numbers live in RAM; the modified block images
/// themselves are written straight into the journal (slot `i` lives at
/// `journal_start + 1 + i`), keeping the in-memory footprint to a small
/// fixed array rather than a per-block buffer. The journal entry doubles
/// as the read-your-writes cache: a read of a staged block is served from
/// its journal slot.
struct Pending {
    targets: [u64; MAX_TXN_BLOCKS],
    len: usize,
}

impl Pending {
    fn new() -> Self {
        Self {
            targets: [0; MAX_TXN_BLOCKS],
            len: 0,
        }
    }

    fn clear(&mut self) {
        self.len = 0;
    }

    fn slot_of(&self, block: u64) -> Option<usize> {
        self.targets[..self.len].iter().position(|&b| b == block)
    }
}

/// Mounted `rustfs` volume over a [`Block`] device `B`.
///
/// Constructed by the driver host with [`RustFs::format`] (lay down a
/// fresh filesystem) or [`RustFs::open`] (attach an existing one, which
/// also replays any committed-but-un-checkpointed journal transaction).
pub struct RustFs<B: Block> {
    block: B,
    layout: Layout,
    pending: Pending,
    clock: fn() -> Time64,
}

/// Default clock for a freshly mounted volume: every §21 stamp is the
/// Unix epoch until the host installs a real clock with
/// [`RustFs::with_clock`]. A driver running headless on a board with no
/// wall clock yet keeps deterministic, in-range timestamps this way
/// rather than panicking or inventing a time (`AGENTS.md` §2.9, §21).
fn epoch_clock() -> Time64 {
    Time64::UNIX_EPOCH
}

/// FNV-1a 64-bit offset basis, the journal-checksum seed.
const FNV_SEED: u64 = 0xcbf2_9ce4_8422_2325;

/// Write a directory slot `(ino, name)` into `buf` at `slot`.
fn put_dirent(buf: &mut [u8], slot: usize, ino: u32, name: &[u8]) {
    let base = slot * DIRENT_SIZE;
    for byte in &mut buf[base..base + DIRENT_SIZE] {
        *byte = 0;
    }
    wr_u32(buf, base, ino);
    wr_u32(buf, base + 4, as_u32(name.len()));
    buf[base + 8..base + 8 + name.len()].copy_from_slice(name);
}

impl<B: Block> RustFs<B> {
    /// Lay down a fresh `rustfs` volume on `block` with `inode_count`
    /// inodes, then return it mounted.
    ///
    /// # Errors
    ///
    /// * [`DriverError::BadMagic`] if the device geometry cannot host a
    ///   valid layout (block size out of range or not a power of two).
    /// * [`DriverError::DeviceFault`] if the device is too small for the
    ///   requested inode count plus a non-empty data region, or a block
    ///   write fails.
    ///
    /// # Capabilities
    ///
    /// Reached only through the driver's [`DriverHandle`].
    pub fn format(mut block: B, inode_count: u32) -> Result<Self, DriverError> {
        let geo = block.geometry()?;
        let bs = geo.block_size as usize;
        if !(MIN_BLOCK_SIZE..=MAX_BLOCK_SIZE).contains(&bs) || !bs.is_power_of_two() {
            return Err(DriverError::BadMagic);
        }
        if inode_count < 2 {
            return Err(DriverError::OutOfRange);
        }
        let total = geo.block_count;
        let inodes_per_block = (bs / INODE_SIZE) as u64;
        let inode_blocks = u64::from(inode_count).div_ceil(inodes_per_block);
        let inode_start = 1;
        let bitmap_start = inode_start + inode_blocks;
        let bits_per_block = (bs * 8) as u64;
        let mut bitmap_blocks = 1u64;
        let (data_start, data_blocks) = loop {
            let data_start = bitmap_start + bitmap_blocks + JOURNAL_BLOCKS;
            if data_start >= total {
                return Err(DriverError::DeviceFault);
            }
            let data_blocks = total - data_start;
            let need = data_blocks.div_ceil(bits_per_block);
            if need == bitmap_blocks {
                break (data_start, data_blocks);
            }
            bitmap_blocks = need;
        };
        if data_blocks == 0 {
            return Err(DriverError::DeviceFault);
        }
        let layout = Layout {
            block_size: bs,
            total_blocks: total,
            inode_count,
            inode_start,
            inode_blocks,
            bitmap_start,
            bitmap_blocks,
            journal_start: bitmap_start + bitmap_blocks,
            journal_blocks: JOURNAL_BLOCKS,
            data_start,
            data_blocks,
            root_ino: ROOT_INO,
        };
        layout.validate()?;

        let mut scratch = [0u8; MAX_BLOCK_SIZE];
        // Superblock.
        layout.encode(&mut scratch[..bs]);
        block.write_blocks(0, &scratch[..bs])?;
        // Zero the inode table and the bitmap.
        for byte in &mut scratch[..bs] {
            *byte = 0;
        }
        for b in 0..layout.inode_blocks {
            block.write_blocks(layout.inode_start + b, &scratch[..bs])?;
        }
        for b in 0..layout.bitmap_blocks {
            block.write_blocks(layout.bitmap_start + b, &scratch[..bs])?;
        }
        Self::clear_journal(&mut block, &layout)?;
        // Mark data block 0 (the root directory's first block) allocated.
        scratch[0] = 0b0000_0001;
        block.write_blocks(layout.bitmap_start, &scratch[..bs])?;
        // Root directory data block: "." and "..", both the root itself.
        for byte in &mut scratch[..bs] {
            *byte = 0;
        }
        put_dirent(&mut scratch, 0, ROOT_INO, b".");
        put_dirent(&mut scratch, 1, ROOT_INO, b"..");
        let root_data = layout.data_start;
        block.write_blocks(root_data, &scratch[..bs])?;
        // Root inode.
        let mut root = Inode::empty(KIND_DIR, Security::new(0o755, 0, 0), Time64::UNIX_EPOCH);
        root.nlink = 2;
        root.size = bs as u64;
        root.direct[0] = root_data;
        let Some((iblock, ioff)) = layout.inode_loc(ROOT_INO) else {
            return Err(DriverError::DeviceFault);
        };
        for byte in &mut scratch[..bs] {
            *byte = 0;
        }
        block.read_blocks(iblock, &mut scratch[..bs])?;
        root.encode(&mut scratch[ioff..ioff + INODE_SIZE]);
        block.write_blocks(iblock, &scratch[..bs])?;

        Ok(Self {
            block,
            layout,
            pending: Pending::new(),
            clock: epoch_clock,
        })
    }

    /// Attach an existing `rustfs` volume, replaying any committed but
    /// un-checkpointed journal transaction first (`AGENTS.md` §2.5 — no
    /// half-applied metadata survives a crash).
    ///
    /// # Errors
    ///
    /// * [`DriverError::BadMagic`] if the superblock fails validation.
    /// * [`DriverError::DeviceFault`] on an unrecoverable block read or
    ///   write (including journal replay).
    ///
    /// # Capabilities
    ///
    /// Reached only through the driver's [`DriverHandle`].
    pub fn open(mut block: B) -> Result<Self, DriverError> {
        let geo = block.geometry()?;
        let bs = geo.block_size as usize;
        if !(MIN_BLOCK_SIZE..=MAX_BLOCK_SIZE).contains(&bs) || !bs.is_power_of_two() {
            return Err(DriverError::BadMagic);
        }
        let mut scratch = [0u8; MAX_BLOCK_SIZE];
        block.read_blocks(0, &mut scratch[..bs])?;
        let layout = Layout::decode(&scratch[..bs])?;
        if layout.block_size != bs || layout.total_blocks != geo.block_count {
            return Err(DriverError::BadMagic);
        }
        Self::recover(&mut block, &layout)?;
        Ok(Self {
            block,
            layout,
            pending: Pending::new(),
            clock: epoch_clock,
        })
    }

    /// Install the clock used to stamp the §21 [`Time64`] timestamps on
    /// subsequent mutations (create, write, truncate, security change,
    /// directory update). Without it, every stamp is the Unix epoch.
    ///
    /// The clock is a pure `fn() -> Time64`; the host points it at the
    /// kernel's monotonic-to-wall clock source. Replacing it never
    /// rewrites timestamps already on disk.
    ///
    /// # Capabilities
    ///
    /// Reached only through the driver's [`DriverHandle`].
    #[must_use]
    pub fn with_clock(mut self, clock: fn() -> Time64) -> Self {
        self.clock = clock;
        self
    }

    /// Consume the filesystem, returning the backing block device.
    #[must_use]
    pub fn into_block(self) -> B {
        self.block
    }

    /// Write an empty (no pending transaction) journal header.
    fn clear_journal(block: &mut B, layout: &Layout) -> Result<(), DriverError> {
        let bs = layout.block_size;
        let mut hdr = [0u8; MAX_BLOCK_SIZE];
        wr_u64(&mut hdr, J_MAGIC, JOURNAL_MAGIC);
        wr_u32(&mut hdr, J_STATE, J_STATE_EMPTY);
        block.write_blocks(layout.journal_start, &hdr[..bs])
    }

    /// Replay a committed-but-un-checkpointed transaction, or discard an
    /// uncommitted / checksum-mismatched one.
    fn recover(block: &mut B, layout: &Layout) -> Result<(), DriverError> {
        let bs = layout.block_size;
        let mut hdr = [0u8; MAX_BLOCK_SIZE];
        block.read_blocks(layout.journal_start, &mut hdr[..bs])?;
        if rd_u64(&hdr, J_MAGIC) != JOURNAL_MAGIC || rd_u32(&hdr, J_STATE) != J_STATE_COMMITTED {
            return Ok(());
        }
        let count = rd_u32(&hdr, J_COUNT) as usize;
        if count == 0 || count > MAX_TXN_BLOCKS {
            return Self::clear_journal(block, layout);
        }
        let stored = rd_u64(&hdr, J_CHECKSUM);
        let mut sum = fnv1a(FNV_SEED, &(count as u64).to_le_bytes());
        let mut img = [0u8; MAX_BLOCK_SIZE];
        for i in 0..count {
            let target = rd_u64(&hdr, J_TARGETS_BASE + i * 8);
            block.read_blocks(layout.journal_start + 1 + i as u64, &mut img[..bs])?;
            sum = fnv1a(sum, &target.to_le_bytes());
            sum = fnv1a(sum, &img[..bs]);
        }
        if sum != stored {
            return Self::clear_journal(block, layout);
        }
        for i in 0..count {
            let target = rd_u64(&hdr, J_TARGETS_BASE + i * 8);
            if target == 0 || target >= layout.total_blocks {
                return Err(DriverError::DeviceFault);
            }
            block.read_blocks(layout.journal_start + 1 + i as u64, &mut img[..bs])?;
            block.write_blocks(target, &img[..bs])?;
        }
        Self::clear_journal(block, layout)
    }

    /// Byte LBA of journal data slot `i` (its image's home in the log).
    fn journal_slot(&self, i: usize) -> u64 {
        self.layout.journal_start + 1 + i as u64
    }

    /// Atomically apply the staged transaction: the modified images are
    /// already in the journal data area (written by [`Self::stage_meta`]);
    /// write the checksummed commit record, checkpoint each image to its
    /// home block, then clear the journal.
    fn commit(&mut self) -> Result<(), DriverError> {
        let n = self.pending.len;
        if n == 0 {
            return Ok(());
        }
        let bs = self.layout.block_size;
        let mut hdr = [0u8; MAX_BLOCK_SIZE];
        wr_u64(&mut hdr, J_MAGIC, JOURNAL_MAGIC);
        wr_u32(&mut hdr, J_STATE, J_STATE_COMMITTED);
        wr_u32(&mut hdr, J_COUNT, u32::try_from(n).unwrap_or(0));
        let mut sum = fnv1a(FNV_SEED, &(n as u64).to_le_bytes());
        let mut img = [0u8; MAX_BLOCK_SIZE];
        for i in 0..n {
            let target = self.pending.targets[i];
            wr_u64(&mut hdr, J_TARGETS_BASE + i * 8, target);
            let slot = self.journal_slot(i);
            self.block.read_blocks(slot, &mut img[..bs])?;
            sum = fnv1a(sum, &target.to_le_bytes());
            sum = fnv1a(sum, &img[..bs]);
        }
        wr_u64(&mut hdr, J_CHECKSUM, sum);
        self.block
            .write_blocks(self.layout.journal_start, &hdr[..bs])?;
        for i in 0..n {
            let target = self.pending.targets[i];
            let slot = self.journal_slot(i);
            self.block.read_blocks(slot, &mut img[..bs])?;
            self.block.write_blocks(target, &img[..bs])?;
        }
        Self::clear_journal(&mut self.block, &self.layout)?;
        self.pending.clear();
        Ok(())
    }

    /// Abandon the staged (uncommitted) transaction. The stale journal
    /// data blocks are harmless: the journal header is left empty, so a
    /// subsequent mount discards them (`AGENTS.md` §2.5).
    fn rollback(&mut self) {
        self.pending.clear();
    }

    /// Read metadata block `block_no`, serving a staged block from its
    /// journal slot (read-your-writes).
    fn read_meta(&mut self, block_no: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        let bs = self.layout.block_size;
        if let Some(i) = self.pending.slot_of(block_no) {
            let slot = self.journal_slot(i);
            return self.block.read_blocks(slot, &mut buf[..bs]);
        }
        self.block.read_blocks(block_no, &mut buf[..bs])
    }

    /// Stage metadata block `block_no` by writing its new image into the
    /// journal data area, recording its home block for the commit record.
    fn stage_meta(&mut self, block_no: u64, buf: &[u8]) -> Result<(), DriverError> {
        let bs = self.layout.block_size;
        let i = if let Some(i) = self.pending.slot_of(block_no) {
            i
        } else {
            if self.pending.len >= MAX_TXN_BLOCKS {
                return Err(DriverError::DeviceFault);
            }
            let i = self.pending.len;
            self.pending.targets[i] = block_no;
            self.pending.len += 1;
            i
        };
        let slot = self.journal_slot(i);
        self.block.write_blocks(slot, &buf[..bs])
    }

    /// Allocate one free data block, marking it used in the bitmap.
    fn alloc_block(&mut self) -> Result<u64, DriverError> {
        let bs = self.layout.block_size;
        let bits_per_block = (bs * 8) as u64;
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        for bb in 0..self.layout.bitmap_blocks {
            let bmblock = self.layout.bitmap_start + bb;
            self.read_meta(bmblock, &mut buf)?;
            for byte_idx in 0..bs {
                let byte = buf[byte_idx];
                if byte == 0xFF {
                    continue;
                }
                for bit in 0..8u32 {
                    if byte & (1 << bit) != 0 {
                        continue;
                    }
                    let data_idx = bb * bits_per_block + (byte_idx as u64) * 8 + u64::from(bit);
                    if data_idx >= self.layout.data_blocks {
                        return Err(DriverError::DeviceFault);
                    }
                    buf[byte_idx] |= 1 << bit;
                    self.stage_meta(bmblock, &buf)?;
                    return Ok(self.layout.data_start + data_idx);
                }
            }
        }
        Err(DriverError::DeviceFault)
    }

    /// Mark data block `block` free in the bitmap.
    fn free_block(&mut self, block: u64) -> Result<(), DriverError> {
        if block < self.layout.data_start || block >= self.layout.total_blocks {
            return Err(DriverError::DeviceFault);
        }
        let bs = self.layout.block_size;
        let bits_per_block = (bs * 8) as u64;
        let data_idx = block - self.layout.data_start;
        let bb = data_idx / bits_per_block;
        let within = data_idx % bits_per_block;
        let byte_idx = (within / 8) as usize;
        let bit = (within % 8) as u32;
        let bmblock = self.layout.bitmap_start + bb;
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        self.read_meta(bmblock, &mut buf)?;
        buf[byte_idx] &= !(1u8 << bit);
        self.stage_meta(bmblock, &buf)
    }

    /// Read inode `ino`, honouring staged writes.
    fn read_inode(&mut self, ino: u32) -> Result<Inode, DriverError> {
        let (block, off) = self.layout.inode_loc(ino).ok_or(DriverError::NotFound)?;
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        self.read_meta(block, &mut buf)?;
        Inode::decode(&buf[off..off + INODE_SIZE])?.ok_or(DriverError::NotFound)
    }

    /// Stage a write of inode `ino`.
    fn write_inode(&mut self, ino: u32, inode: &Inode) -> Result<(), DriverError> {
        let (block, off) = self.layout.inode_loc(ino).ok_or(DriverError::NotFound)?;
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        self.read_meta(block, &mut buf)?;
        inode.encode(&mut buf[off..off + INODE_SIZE]);
        self.stage_meta(block, &buf)
    }

    /// Allocate a free inode slot, staging the new record there.
    fn alloc_inode(&mut self, inode: &Inode) -> Result<u32, DriverError> {
        for ino in 1..self.layout.inode_count {
            let Some((block, off)) = self.layout.inode_loc(ino) else {
                continue;
            };
            let mut buf = [0u8; MAX_BLOCK_SIZE];
            self.read_meta(block, &mut buf)?;
            if rd_u32(&buf, off + I_USED) != INODE_USED {
                inode.encode(&mut buf[off..off + INODE_SIZE]);
                self.stage_meta(block, &buf)?;
                return Ok(ino);
            }
        }
        Err(DriverError::DeviceFault)
    }

    /// Stage a free of inode `ino` (zeroes the record).
    fn free_inode(&mut self, ino: u32) -> Result<(), DriverError> {
        let (block, off) = self.layout.inode_loc(ino).ok_or(DriverError::NotFound)?;
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        self.read_meta(block, &mut buf)?;
        for byte in &mut buf[off..off + INODE_SIZE] {
            *byte = 0;
        }
        self.stage_meta(block, &buf)
    }

    /// Largest number of blocks a file can address (direct + one
    /// single-indirect block's worth of pointers).
    fn max_file_blocks(&self) -> u64 {
        DIRECT_PTRS as u64 + self.layout.ptrs_per_block() as u64
    }

    /// The data block backing logical block `bi` of `inode`, or `0` for
    /// a hole / unallocated block.
    fn block_ptr(&mut self, inode: &Inode, bi: u64) -> Result<u64, DriverError> {
        if bi < DIRECT_PTRS as u64 {
            return Ok(inode.direct[as_usize(bi)]);
        }
        let idx = as_usize(bi - DIRECT_PTRS as u64);
        if idx >= self.layout.ptrs_per_block() {
            return Err(DriverError::LengthOutOfRange);
        }
        if inode.indirect == 0 {
            return Ok(0);
        }
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        self.read_meta(inode.indirect, &mut buf)?;
        Ok(rd_u64(&buf, idx * 8))
    }

    /// Point logical block `bi` of `inode` at data block `ptr`,
    /// allocating the single-indirect block on first use.
    fn set_block_ptr(&mut self, inode: &mut Inode, bi: u64, ptr: u64) -> Result<(), DriverError> {
        if bi < DIRECT_PTRS as u64 {
            inode.direct[as_usize(bi)] = ptr;
            return Ok(());
        }
        let idx = as_usize(bi - DIRECT_PTRS as u64);
        if idx >= self.layout.ptrs_per_block() {
            return Err(DriverError::LengthOutOfRange);
        }
        if inode.indirect == 0 {
            let ib = self.alloc_block()?;
            let zero = [0u8; MAX_BLOCK_SIZE];
            self.stage_meta(ib, &zero)?;
            inode.indirect = ib;
        }
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        self.read_meta(inode.indirect, &mut buf)?;
        wr_u64(&mut buf, idx * 8, ptr);
        self.stage_meta(inode.indirect, &buf)
    }

    /// Free every data block backing `inode` (its direct blocks, the
    /// single-indirect block's targets, and the indirect block itself).
    fn free_all_blocks(&mut self, inode: &mut Inode) -> Result<(), DriverError> {
        let bs = self.layout.block_size as u64;
        let blocks = inode.size.div_ceil(bs);
        for bi in 0..blocks {
            let ptr = self.block_ptr(inode, bi)?;
            if ptr != 0 {
                self.free_block(ptr)?;
            }
        }
        if inode.indirect != 0 {
            self.free_block(inode.indirect)?;
            inode.indirect = 0;
        }
        inode.direct = [0; DIRECT_PTRS];
        inode.size = 0;
        Ok(())
    }

    /// Number of whole directory blocks backing `dir`.
    fn dir_block_count(&self, dir: &Inode) -> u64 {
        dir.size / self.layout.block_size as u64
    }

    /// Resolve `name` within directory `dir`, returning its inode index.
    fn dir_lookup(&mut self, dir: &Inode, name: &[u8]) -> Result<Option<u32>, DriverError> {
        let per = self.layout.dirents_per_block();
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        for blk in 0..self.dir_block_count(dir) {
            let ptr = self.block_ptr(dir, blk)?;
            if ptr == 0 {
                continue;
            }
            self.read_meta(ptr, &mut buf)?;
            for slot in 0..per {
                let base = slot * DIRENT_SIZE;
                let ino = rd_u32(&buf, base);
                if ino == 0 {
                    continue;
                }
                let name_len = rd_u32(&buf, base + 4) as usize;
                if name_len > NAME_MAX {
                    return Err(DriverError::DeviceFault);
                }
                if &buf[base + 8..base + 8 + name_len] == name {
                    return Ok(Some(ino));
                }
            }
        }
        Ok(None)
    }

    /// Whether `dir` holds no entries other than `.` and `..`.
    fn dir_is_empty(&mut self, dir: &Inode) -> Result<bool, DriverError> {
        let per = self.layout.dirents_per_block();
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        for blk in 0..self.dir_block_count(dir) {
            let ptr = self.block_ptr(dir, blk)?;
            if ptr == 0 {
                continue;
            }
            self.read_meta(ptr, &mut buf)?;
            for slot in 0..per {
                let base = slot * DIRENT_SIZE;
                let ino = rd_u32(&buf, base);
                if ino == 0 {
                    continue;
                }
                let name_len = rd_u32(&buf, base + 4) as usize;
                if name_len > NAME_MAX {
                    return Err(DriverError::DeviceFault);
                }
                let name = &buf[base + 8..base + 8 + name_len];
                if name != b"." && name != b".." {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    /// Add directory entry `(child_ino, name)` to `dir`, growing the
    /// directory by one block if every existing slot is occupied.
    fn add_entry(
        &mut self,
        dir: &mut Inode,
        child_ino: u32,
        name: &[u8],
    ) -> Result<(), DriverError> {
        let bs = self.layout.block_size;
        let per = self.layout.dirents_per_block();
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        for blk in 0..self.dir_block_count(dir) {
            let ptr = self.block_ptr(dir, blk)?;
            if ptr == 0 {
                continue;
            }
            self.read_meta(ptr, &mut buf)?;
            for slot in 0..per {
                if rd_u32(&buf, slot * DIRENT_SIZE) == 0 {
                    put_dirent(&mut buf, slot, child_ino, name);
                    return self.stage_meta(ptr, &buf);
                }
            }
        }
        // No free slot: append a fresh, zeroed directory block.
        let new_blk = self.alloc_block()?;
        let blk_index = self.dir_block_count(dir);
        if blk_index >= self.max_file_blocks() {
            return Err(DriverError::DeviceFault);
        }
        let mut fresh = [0u8; MAX_BLOCK_SIZE];
        put_dirent(&mut fresh, 0, child_ino, name);
        self.stage_meta(new_blk, &fresh)?;
        self.set_block_ptr(dir, blk_index, new_blk)?;
        dir.size += bs as u64;
        Ok(())
    }

    /// Mark the entry named `name` free in `dir`. Returns the inode it
    /// referenced.
    fn remove_entry(&mut self, dir: &Inode, name: &[u8]) -> Result<u32, DriverError> {
        let per = self.layout.dirents_per_block();
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        for blk in 0..self.dir_block_count(dir) {
            let ptr = self.block_ptr(dir, blk)?;
            if ptr == 0 {
                continue;
            }
            self.read_meta(ptr, &mut buf)?;
            for slot in 0..per {
                let base = slot * DIRENT_SIZE;
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
                    self.stage_meta(ptr, &buf)?;
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
        let bs = self.layout.block_size as u64;
        let end = inode.size.min(offset + out.len() as u64);
        let mut done = 0usize;
        let mut pos = offset;
        let mut data = [0u8; MAX_BLOCK_SIZE];
        while pos < end {
            let bi = pos / bs;
            let within = as_usize(pos % bs);
            let chunk = as_usize((bs - within as u64).min(end - pos));
            let ptr = self.block_ptr(inode, bi)?;
            if ptr == 0 {
                for byte in &mut out[done..done + chunk] {
                    *byte = 0;
                }
            } else {
                self.read_meta(ptr, &mut data)?;
                out[done..done + chunk].copy_from_slice(&data[within..within + chunk]);
            }
            done += chunk;
            pos += chunk as u64;
        }
        Ok(done)
    }

    /// Copy-on-write `data` into file `inode` at `offset`.
    fn write_file(
        &mut self,
        inode: &mut Inode,
        offset: u64,
        data: &[u8],
    ) -> Result<usize, DriverError> {
        if data.is_empty() {
            return Ok(0);
        }
        let bs = self.layout.block_size;
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or(DriverError::LengthOutOfRange)?;
        if end.div_ceil(bs as u64) > self.max_file_blocks() {
            return Err(DriverError::LengthOutOfRange);
        }
        let mut done = 0usize;
        let mut pos = offset;
        let mut blk = [0u8; MAX_BLOCK_SIZE];
        while done < data.len() {
            let bi = pos / bs as u64;
            let within = as_usize(pos % bs as u64);
            let chunk = (bs - within).min(data.len() - done);
            let old_ptr = self.block_ptr(inode, bi)?;
            for byte in &mut blk[..bs] {
                *byte = 0;
            }
            if (within != 0 || chunk != bs) && old_ptr != 0 {
                self.read_meta(old_ptr, &mut blk)?;
            }
            blk[within..within + chunk].copy_from_slice(&data[done..done + chunk]);
            let new_ptr = self.alloc_block()?;
            self.block.write_blocks(new_ptr, &blk[..bs])?;
            self.set_block_ptr(inode, bi, new_ptr)?;
            if old_ptr != 0 {
                self.free_block(old_ptr)?;
            }
            done += chunk;
            pos += chunk as u64;
        }
        if end > inode.size {
            inode.size = end;
        }
        Ok(done)
    }

    /// Shrink or grow file `inode` to `size`.
    ///
    /// Shrinking frees the blocks past `size` and copy-on-write zeroes the
    /// tail of the partially-kept block, so a later grow (or sparse read)
    /// past the shrink point reads zeros rather than stale bytes.
    fn truncate_file(&mut self, inode: &mut Inode, size: u64) -> Result<(), DriverError> {
        let bs = self.layout.block_size as u64;
        if size.div_ceil(bs) > self.max_file_blocks() {
            return Err(DriverError::LengthOutOfRange);
        }
        if size < inode.size {
            let keep = size.div_ceil(bs);
            let had = inode.size.div_ceil(bs);
            for bi in keep..had {
                let ptr = self.block_ptr(inode, bi)?;
                if ptr != 0 {
                    self.free_block(ptr)?;
                    self.set_block_ptr(inode, bi, 0)?;
                }
            }
            if keep <= DIRECT_PTRS as u64 && inode.indirect != 0 {
                self.free_block(inode.indirect)?;
                inode.indirect = 0;
            }
            let tail = as_usize(size % bs);
            if tail != 0 {
                let bi = size / bs;
                let old_ptr = self.block_ptr(inode, bi)?;
                if old_ptr != 0 {
                    let mut blk = [0u8; MAX_BLOCK_SIZE];
                    self.read_meta(old_ptr, &mut blk)?;
                    for byte in &mut blk[tail..as_usize(bs)] {
                        *byte = 0;
                    }
                    let new_ptr = self.alloc_block()?;
                    self.block.write_blocks(new_ptr, &blk[..as_usize(bs)])?;
                    self.set_block_ptr(inode, bi, new_ptr)?;
                    self.free_block(old_ptr)?;
                }
            }
        }
        inode.size = size;
        Ok(())
    }

    /// Map a [`NodeId`] to a validated inode index.
    fn ino_of(&self, node: NodeId) -> Result<u32, DriverError> {
        let raw = node.raw();
        let ino = u32::try_from(raw).map_err(|_| DriverError::NotFound)?;
        if ino == 0 || ino >= self.layout.inode_count {
            return Err(DriverError::NotFound);
        }
        Ok(ino)
    }

    fn check_name(name: &[u8]) -> Result<(), DriverError> {
        if name.is_empty() || name.len() > NAME_MAX {
            return Err(DriverError::LengthOutOfRange);
        }
        if name == b"." || name == b".." || name.contains(&b'/') {
            return Err(DriverError::Unsupported);
        }
        Ok(())
    }

    /// Replace the security record stored for `node`.
    ///
    /// # Errors
    ///
    /// * [`DriverError::NotFound`] if `node` does not name a live inode.
    /// * [`DriverError::DeviceFault`] on an unrecoverable block write.
    pub fn set_security(&mut self, node: NodeId, sec: Security) -> Result<(), DriverError> {
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
        let bs = self.layout.block_size as u64;
        let (kind_val, mode) = match kind {
            NodeKind::Directory => (KIND_DIR, 0o755),
            NodeKind::RegularFile => (KIND_FILE, 0o644),
        };
        let mut child = Inode::empty(kind_val, Security::new(mode, 0, 0), now);
        if kind_val == KIND_DIR {
            child.nlink = 2;
        }
        let child_ino = self.alloc_inode(&child)?;
        if kind_val == KIND_DIR {
            let db = self.alloc_block()?;
            let mut buf = [0u8; MAX_BLOCK_SIZE];
            put_dirent(&mut buf, 0, child_ino, b".");
            put_dirent(&mut buf, 1, dir_ino, b"..");
            self.stage_meta(db, &buf)?;
            child.direct[0] = db;
            child.size = bs;
            self.write_inode(child_ino, &child)?;
            dir_inode.nlink += 1;
        }
        self.add_entry(&mut dir_inode, child_ino, name)?;
        dir_inode.times.modified = now;
        dir_inode.times.changed = now;
        self.write_inode(dir_ino, &dir_inode)?;
        self.commit()?;
        Ok(NodeId::from_raw(u64::from(child_ino)))
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
        if child.is_dir() {
            return Err(DriverError::Unsupported);
        }
        let written = self.write_file(&mut child, offset, data)?;
        let now = (self.clock)();
        child.times.modified = now;
        child.times.accessed = now;
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
        if child.is_dir() {
            return Err(DriverError::Unsupported);
        }
        self.truncate_file(&mut child, size)?;
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
        self.free_all_blocks(&mut child)?;
        self.free_inode(child_ino)?;
        if child.is_dir() {
            dir_inode.nlink = dir_inode.nlink.saturating_sub(1);
        }
        self.remove_entry(&dir_inode, name)?;
        let now = (self.clock)();
        dir_inode.times.modified = now;
        dir_inode.times.changed = now;
        self.write_inode(dir_ino, &dir_inode)?;
        self.commit()
    }
}

impl<B: Block> FilesystemRead for RustFs<B> {
    fn root(&self) -> NodeId {
        NodeId::from_raw(u64::from(ROOT_INO))
    }

    fn node_info(&mut self, node: NodeId) -> Result<NodeInfo, DriverError> {
        let ino = self.ino_of(node)?;
        let inode = self.read_inode(ino)?;
        if inode.is_dir() {
            Ok(NodeInfo {
                kind: NodeKind::Directory,
                size: 0,
            })
        } else {
            Ok(NodeInfo {
                kind: NodeKind::RegularFile,
                size: inode.size,
            })
        }
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
        if inode.is_dir() {
            return Err(DriverError::Unsupported);
        }
        self.read_file(&inode, offset, buf)
    }

    fn read_dir(
        &mut self,
        dir: NodeId,
        index: u64,
        name_out: &mut [u8],
    ) -> Result<Option<DirEntry>, DriverError> {
        let dir_ino = self.ino_of(dir)?;
        let dir_inode = self.read_inode(dir_ino)?;
        if !dir_inode.is_dir() {
            return Err(DriverError::Unsupported);
        }
        let per = self.layout.dirents_per_block();
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        let mut seen = 0u64;
        for blk in 0..self.dir_block_count(&dir_inode) {
            let ptr = self.block_ptr(&dir_inode, blk)?;
            if ptr == 0 {
                continue;
            }
            self.read_meta(ptr, &mut buf)?;
            for slot in 0..per {
                let base = slot * DIRENT_SIZE;
                let ino = rd_u32(&buf, base);
                if ino == 0 {
                    continue;
                }
                let name_len = rd_u32(&buf, base + 4) as usize;
                if name_len == 0 || name_len > NAME_MAX {
                    return Err(DriverError::DeviceFault);
                }
                let name = &buf[base + 8..base + 8 + name_len];
                if name == b"." || name == b".." {
                    continue;
                }
                if seen == index {
                    if name_out.len() < name_len {
                        return Err(DriverError::BufferTooSmall);
                    }
                    name_out[..name_len].copy_from_slice(name);
                    let child = self.read_inode(ino)?;
                    let kind = if child.is_dir() {
                        NodeKind::Directory
                    } else {
                        NodeKind::RegularFile
                    };
                    return Ok(Some(DirEntry {
                        node: NodeId::from_raw(u64::from(ino)),
                        kind,
                        name_len,
                    }));
                }
                seen += 1;
            }
        }
        Ok(None)
    }
}

impl<B: Block> FilesystemWrite for RustFs<B> {
    fn create(&mut self, dir: NodeId, name: &[u8], kind: NodeKind) -> Result<NodeId, DriverError> {
        let result = self.create_inner(dir, name, kind);
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
        let result = self.write_inner(dir, name, offset, data);
        if result.is_err() {
            self.rollback();
        }
        result
    }

    fn truncate(&mut self, dir: NodeId, name: &[u8], size: u64) -> Result<(), DriverError> {
        let result = self.truncate_inner(dir, name, size);
        if result.is_err() {
            self.rollback();
        }
        result
    }

    fn remove(&mut self, dir: NodeId, name: &[u8]) -> Result<(), DriverError> {
        let result = self.remove_inner(dir, name);
        if result.is_err() {
            self.rollback();
        }
        result
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        self.commit()
    }
}

impl<B: Block> FilesystemSecurity for RustFs<B> {
    fn security(&mut self, node: NodeId) -> Result<Security, DriverError> {
        let ino = self.ino_of(node)?;
        Ok(self.read_inode(ino)?.sec)
    }
}

impl<B: Block> FilesystemTimestamps for RustFs<B> {
    fn times(&mut self, node: NodeId) -> Result<NodeTimes, DriverError> {
        let ino = self.ino_of(node)?;
        Ok(self.read_inode(ino)?.times)
    }
}
