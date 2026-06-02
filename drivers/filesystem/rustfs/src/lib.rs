//! RustOS native filesystem driver (`rustfs`).
//!
//! `rustfs` is the native RustOS filesystem: a block-backed, copy-on-write
//! filesystem that stores full POSIX metadata plus an inline access-control
//! list and an optional capability gate **per inode** (`AGENTS.md` §5.3). It
//! sits behind any [`rustos_abi::driver::block::Block`] device and exposes
//! itself through the versioned [`FilesystemRead`] / [`FilesystemWrite`] /
//! [`FilesystemSecurity`] / [`FilesystemTimestamps`] surfaces (`AGENTS.md`
//! §2.4 / §9 — new behaviour ships as a new trait, never by widening the
//! frozen mount/unmount [`Filesystem`](rustos_abi::driver::filesystem::Filesystem)).
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
//! decode time (`AGENTS.md` §5.4 — fail closed).
//!
//! # Public surface
//!
//! Per `AGENTS.md` §8 the only public *function* is [`register`]. [`RustFs`]
//! is a public *type* the driver host instantiates with [`RustFs::format`] /
//! [`RustFs::open`].
//!
//! # Capabilities
//!
//! Loading requires
//! [`CapabilityId::DRV_LOAD`](rustos_abi::CapabilityId::DRV_LOAD). The driver
//! runs in user space; it does not request `CAP_DRV_KERNEL` (`AGENTS.md`
//! §4 / §8).

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

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

mod header;
mod superblock;
mod transaction;

#[cfg(test)]
mod tests;

use header::{BlockHeader, BlockType, HEADER_LEN};
use superblock::{Superblock, RING_SLOTS};
use transaction::TxnRoot;

/// Per-driver `DriverHandle` marker returned by [`register`].
const REGISTER_HANDLE_MARKER: u64 = 0x5275_7374_4653_0002;

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

/// Largest block size the driver stages through its on-stack scratch
/// buffers. No Tier-1 block device exceeds 4096 bytes per block.
const MAX_BLOCK_SIZE: usize = 4096;
/// Smallest block size the format supports.
const MIN_BLOCK_SIZE: usize = 512;

/// Fixed on-disk size of one inode record, in bytes.
const INODE_SIZE: usize = 256;
/// Fixed on-disk size of one directory slot, in bytes.
const DIRENT_SIZE: usize = 64;
/// Bytes available for a name inside a directory slot.
const NAME_MAX: usize = DIRENT_SIZE - 8;

/// Number of direct block pointers stored inline in an inode.
const DIRECT_PTRS: usize = 12;
/// Maximum number of inline ACL entries stored in an inode.
const ACL_MAX: usize = 8;
const _: () = assert!(ACL_MAX == rustos_abi::driver::filesystem::MAX_ACL_ENTRIES);

/// Inode-table index of the root directory. Index 0 is the reserved "no
/// inode" sentinel, so a zeroed directory slot reads as free.
const ROOT_INO: u32 = 1;

/// `used` marker stored in a live inode's first word.
const INODE_USED: u32 = 0x494E_4F44; // "INOD"
/// On-disk inode `kind` value for a directory.
const KIND_DIR: u32 = 1;
/// On-disk inode `kind` value for a regular file.
const KIND_FILE: u32 = 2;

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

    /// Decode the inode record at `buf[..INODE_SIZE]`, returning `None` for
    /// a free (zeroed) slot.
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

/// A best-effort wall clock; the host overrides it with [`RustFs::with_clock`].
fn epoch_clock() -> Time64 {
    Time64::UNIX_EPOCH
}

/// A mounted copy-on-write rustfs volume.
///
/// The on-disk state is the committed transaction root selected from the
/// superblock ring; the in-memory free-block bitmap, inode-allocation bitmap,
/// and inode map are rebuilt by walking that root at [`RustFs::open`] and kept
/// in step as transactions commit. A volume is created with [`RustFs::format`]
/// and reopened with [`RustFs::open`].
pub struct RustFs<B: Block> {
    block: B,
    fs_uuid: u128,
    block_size: usize,
    total_blocks: u64,
    inode_count: u32,
    generation: u64,
    ring_pos: u64,
    inode_map: Vec<u64>,
    map_index_phys: u64,
    map_blocks: Vec<u64>,
    root_phys: u64,
    free: Vec<u64>,
    inode_used: Vec<bool>,
    txn_allocated: Vec<u64>,
    txn_freed: Vec<u64>,
    txn_private: Vec<bool>,
    txn_map_changes: Vec<(usize, u64)>,
    txn_inode_changes: Vec<(u32, bool)>,
    alloc_cursor: u64,
    clock: fn() -> Time64,
}

impl<B: Block> RustFs<B> {
    fn inodes_per_block(&self) -> usize {
        (self.block_size - HEADER_LEN) / INODE_SIZE
    }

    fn ptrs_per_block(&self) -> usize {
        (self.block_size - HEADER_LEN) / 8
    }

    fn dirents_per_block(&self) -> usize {
        (self.block_size - HEADER_LEN) / DIRENT_SIZE
    }

    /// Inode-map index and the byte offset of inode `ino` inside its block.
    fn inode_loc(&self, ino: u32) -> Option<(usize, usize)> {
        if ino == 0 || ino >= self.inode_count {
            return None;
        }
        let per = self.inodes_per_block();
        Some((
            ino as usize / per,
            HEADER_LEN + (ino as usize % per) * INODE_SIZE,
        ))
    }

    // --- in-memory used-block bitmap ---

    fn bit_used(&self, block: u64) -> bool {
        let word = as_usize(block / 64);
        let bit = block % 64;
        self.free.get(word).is_some_and(|w| (w >> bit) & 1 == 1)
    }

    fn mark_used(&mut self, block: u64) {
        let word = as_usize(block / 64);
        let bit = block % 64;
        if let Some(w) = self.free.get_mut(word) {
            *w |= 1u64 << bit;
        }
    }

    fn mark_free(&mut self, block: u64) {
        let word = as_usize(block / 64);
        let bit = block % 64;
        if let Some(w) = self.free.get_mut(word) {
            *w &= !(1u64 << bit);
        }
    }

    /// Replace the wall clock used to stamp the §21 timestamps. Used by tests
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

    /// Read the raw block at `phys` into the first `block_size` bytes of `buf`.
    fn read_block(&mut self, phys: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        let bs = self.block_size;
        self.block.read_blocks(phys, &mut buf[..bs])
    }

    /// Write the first `block_size` bytes of `buf` to the block at `phys`.
    fn write_block(&mut self, phys: u64, buf: &[u8]) -> Result<(), DriverError> {
        let bs = self.block_size;
        self.block.write_blocks(phys, &buf[..bs])
    }

    /// Read and validate the metadata block at `phys`, confirming it is the
    /// `expect_type` block at that address.
    fn read_meta(
        &mut self,
        phys: u64,
        expect_type: BlockType,
        buf: &mut [u8],
    ) -> Result<BlockHeader, DriverError> {
        self.read_block(phys, buf)?;
        let bs = self.block_size;
        BlockHeader::decode_verify(&buf[..bs], expect_type, self.fs_uuid, phys)
    }

    /// Whether `phys` was allocated by the current, not-yet-committed
    /// transaction and may therefore be overwritten in place.
    fn is_txn_private(&self, phys: u64) -> bool {
        self.txn_private
            .get(as_usize(phys))
            .copied()
            .unwrap_or(false)
    }

    /// Allocate one free block from the pool, marking it used and private to
    /// the current transaction.
    fn alloc_block(&mut self) -> Result<u64, DriverError> {
        let start = RING_SLOTS;
        let total = self.total_blocks;
        let mut scanned = 0u64;
        let span = total.saturating_sub(start);
        let mut block = self.alloc_cursor.max(start);
        while scanned < span {
            if block >= total {
                block = start;
            }
            if !self.bit_used(block) {
                self.mark_used(block);
                if let Some(slot) = self.txn_private.get_mut(as_usize(block)) {
                    *slot = true;
                }
                self.txn_allocated.push(block);
                self.alloc_cursor = block + 1;
                return Ok(block);
            }
            block += 1;
            scanned += 1;
        }
        Err(DriverError::NoSpace)
    }

    /// Defer-free a block: it is reclaimed only after the transaction commits,
    /// so a block reachable from the committed root is never reused mid-flight.
    fn free_block(&mut self, phys: u64) {
        if phys != 0 {
            self.txn_freed.push(phys);
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
            let p = self.alloc_block()?;
            if old_phys != 0 {
                self.free_block(old_phys);
            }
            p
        };
        let payload_len = as_u32(self.block_size - HEADER_LEN);
        let header = BlockHeader {
            block_type,
            fs_uuid: self.fs_uuid,
            owner,
            generation: self.generation.wrapping_add(1),
            logical_addr: logical,
            physical_addr: new_phys,
            payload_len,
        };
        let bs = self.block_size;
        header.seal(&mut buf[..bs])?;
        self.write_block(new_phys, buf)?;
        Ok(new_phys)
    }

    /// Read inode `ino` from its (possibly copy-on-written) inode block.
    fn read_inode(&mut self, ino: u32) -> Result<Inode, DriverError> {
        let (idx, off) = self.inode_loc(ino).ok_or(DriverError::NotFound)?;
        let phys = self.inode_map.get(idx).copied().unwrap_or(0);
        if phys == 0 {
            return Err(DriverError::NotFound);
        }
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        self.read_meta(phys, BlockType::Inode, &mut buf)?;
        Inode::decode(&buf[off..off + INODE_SIZE])?.ok_or(DriverError::NotFound)
    }

    /// Copy-on-write inode `ino`'s block with `inode` encoded into its slot.
    fn write_inode(&mut self, ino: u32, inode: &Inode) -> Result<(), DriverError> {
        let (idx, off) = self.inode_loc(ino).ok_or(DriverError::NotFound)?;
        let old = self.inode_map.get(idx).copied().unwrap_or(0);
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        if old != 0 {
            self.read_meta(old, BlockType::Inode, &mut buf)?;
        } else {
            for byte in &mut buf[..self.block_size] {
                *byte = 0;
            }
        }
        inode.encode(&mut buf[off..off + INODE_SIZE]);
        let new = self.cow_meta(old, &mut buf, BlockType::Inode, u64::from(ino), idx as u64)?;
        if new != old {
            self.txn_map_changes.push((idx, old));
            self.inode_map[idx] = new;
        }
        Ok(())
    }

    /// Allocate a free inode index, store `inode` there, and return the index.
    fn alloc_inode(&mut self, inode: &Inode) -> Result<u32, DriverError> {
        for ino in 1..self.inode_count {
            if !self.inode_used[ino as usize] {
                self.txn_inode_changes.push((ino, false));
                self.inode_used[ino as usize] = true;
                self.write_inode(ino, inode)?;
                return Ok(ino);
            }
        }
        Err(DriverError::NoSpace)
    }

    /// Mark inode `ino` free and zero its on-disk slot (copy-on-write).
    fn free_inode(&mut self, ino: u32) -> Result<(), DriverError> {
        let (idx, off) = self.inode_loc(ino).ok_or(DriverError::NotFound)?;
        self.txn_inode_changes
            .push((ino, self.inode_used[ino as usize]));
        self.inode_used[ino as usize] = false;
        let old = self.inode_map.get(idx).copied().unwrap_or(0);
        if old != 0 {
            let mut buf = [0u8; MAX_BLOCK_SIZE];
            self.read_meta(old, BlockType::Inode, &mut buf)?;
            for byte in &mut buf[off..off + INODE_SIZE] {
                *byte = 0;
            }
            let new = self.cow_meta(old, &mut buf, BlockType::Inode, u64::from(ino), idx as u64)?;
            if new != old {
                self.txn_map_changes.push((idx, old));
                self.inode_map[idx] = new;
            }
        }
        Ok(())
    }

    /// Reset the per-transaction bookkeeping at the start of an operation.
    fn begin(&mut self) {
        self.txn_allocated.clear();
        self.txn_freed.clear();
        self.txn_map_changes.clear();
        self.txn_inode_changes.clear();
    }

    /// Discard an operation that failed before committing: undo the in-memory
    /// inode map, inode-allocation bitmap, and block allocations. Nothing was
    /// published, so the committed on-disk root is untouched.
    fn rollback(&mut self) {
        while let Some((ino, prev)) = self.txn_inode_changes.pop() {
            self.inode_used[ino as usize] = prev;
        }
        while let Some((idx, prev)) = self.txn_map_changes.pop() {
            self.inode_map[idx] = prev;
        }
        let allocated = core::mem::take(&mut self.txn_allocated);
        for block in allocated {
            self.mark_free(block);
            if let Some(slot) = self.txn_private.get_mut(as_usize(block)) {
                *slot = false;
            }
        }
        self.txn_freed.clear();
    }

    /// Apply a committed transaction's deferred frees and clear the private
    /// markers, making superseded blocks reusable by the next transaction.
    fn finish_txn(&mut self) {
        let allocated = core::mem::take(&mut self.txn_allocated);
        for block in allocated {
            if let Some(slot) = self.txn_private.get_mut(as_usize(block)) {
                *slot = false;
            }
        }
        let freed = core::mem::take(&mut self.txn_freed);
        for block in freed {
            self.mark_free(block);
            if let Some(slot) = self.txn_private.get_mut(as_usize(block)) {
                *slot = false;
            }
        }
        self.txn_map_changes.clear();
        self.txn_inode_changes.clear();
    }

    /// Commit the staged transaction: serialise the inode map to copy-on-write
    /// blocks, write the new transaction root with its commit record, then
    /// publish the next superblock-ring slot pointing at it
    /// (`transaction` / `superblock`).
    fn commit(&mut self) -> Result<(), DriverError> {
        let bs = self.block_size;
        let next_gen = self.generation.wrapping_add(1);
        let per = self.ptrs_per_block();
        let n_inode_blocks = self.inode_map.len();
        let n_map_blocks = n_inode_blocks.div_ceil(per).max(1);
        if n_map_blocks > per {
            return Err(DriverError::NoSpace);
        }
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        let mut new_map_blocks = Vec::with_capacity(n_map_blocks);
        for i in 0..n_map_blocks {
            for byte in &mut buf[HEADER_LEN..bs] {
                *byte = 0;
            }
            for j in 0..per {
                let idx = i * per + j;
                let value = self.inode_map.get(idx).copied().unwrap_or(0);
                wr_u64(&mut buf, HEADER_LEN + j * 8, value);
            }
            let old = self.map_blocks.get(i).copied().unwrap_or(0);
            let phys = self.cow_meta(old, &mut buf, BlockType::InodeMap, i as u64, i as u64)?;
            new_map_blocks.push(phys);
        }
        let extra: Vec<u64> = self.map_blocks.iter().skip(n_map_blocks).copied().collect();
        for old in extra {
            self.free_block(old);
        }
        for byte in &mut buf[HEADER_LEN..bs] {
            *byte = 0;
        }
        for (i, phys) in new_map_blocks.iter().enumerate() {
            wr_u64(&mut buf, HEADER_LEN + i * 8, *phys);
        }
        let new_index = self.cow_meta(
            self.map_index_phys,
            &mut buf,
            BlockType::InodeMap,
            u64::MAX,
            0,
        )?;
        let old_root = self.root_phys;
        let root_phys = self.alloc_block()?;
        let root = TxnRoot {
            generation: next_gen,
            map_index_phys: new_index,
            inode_blocks: n_inode_blocks as u64,
        };
        root.seal(&mut buf[..bs], self.fs_uuid, root_phys)?;
        self.write_block(root_phys, &buf)?;
        let slot = self.ring_pos % RING_SLOTS;
        let sb = Superblock {
            block_size: as_u32(bs),
            total_blocks: self.total_blocks,
            inode_count: self.inode_count,
            generation: next_gen,
            root_phys,
        };
        sb.seal(&mut buf[..bs], self.fs_uuid, slot)?;
        self.write_block(slot, &buf)?;
        // Commit point passed: the new root is durably published.
        self.generation = next_gen;
        self.ring_pos = self.ring_pos.wrapping_add(1);
        self.map_blocks = new_map_blocks;
        self.map_index_phys = new_index;
        self.root_phys = root_phys;
        self.free_block(old_root);
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
        if total_blocks <= RING_SLOTS + 8 {
            return Err(DriverError::NoSpace);
        }
        let words = as_usize(total_blocks.div_ceil(64));
        let mut fs = Self {
            block,
            fs_uuid: 0,
            block_size,
            total_blocks,
            inode_count: 0,
            generation: 0,
            ring_pos: 0,
            inode_map: Vec::new(),
            map_index_phys: 0,
            map_blocks: Vec::new(),
            root_phys: 0,
            free: vec![0u64; words],
            inode_used: Vec::new(),
            txn_allocated: Vec::new(),
            txn_freed: Vec::new(),
            txn_private: vec![false; as_usize(total_blocks)],
            txn_map_changes: Vec::new(),
            txn_inode_changes: Vec::new(),
            alloc_cursor: RING_SLOTS,
            clock: epoch_clock,
        };
        for slot in 0..RING_SLOTS {
            fs.mark_used(slot);
        }
        Ok(fs)
    }

    /// Maximum number of map blocks an inode map of `n_inode_blocks` needs.
    fn n_map_blocks(&self, n_inode_blocks: usize) -> usize {
        n_inode_blocks.div_ceil(self.ptrs_per_block()).max(1)
    }

    /// Lay a fresh, empty rustfs volume onto `block`, formatted for
    /// `inode_count` inodes, and return it mounted.
    ///
    /// The volume's structure (superblock ring + a first committed transaction
    /// root holding the empty root directory) is written, but encryption,
    /// compression, and dedupe are later stages of the staged build
    /// (`.junie/RUSTFS.md` §15): a Stage-1 volume is a complete, mountable
    /// copy-on-write filesystem but is not yet encrypted at rest.
    ///
    /// # Errors
    ///
    /// * [`DriverError::Unsupported`] if the device block size is unsupported.
    /// * [`DriverError::NoSpace`] if the device is too small, `inode_count` is
    ///   below two, or the geometry needs more than one inode-map index block.
    /// * [`DriverError::DeviceFault`] on an unrecoverable block write.
    pub fn format(block: B, inode_count: u32) -> Result<Self, DriverError> {
        let mut fs = Self::bootstrap(block)?;
        if inode_count < 2 {
            return Err(DriverError::NoSpace);
        }
        fs.inode_count = inode_count;
        let per = fs.inodes_per_block();
        let n_inode_blocks = (inode_count as usize).div_ceil(per);
        if fs.n_map_blocks(n_inode_blocks) > fs.ptrs_per_block() {
            return Err(DriverError::NoSpace);
        }
        fs.inode_map = vec![0u64; n_inode_blocks];
        fs.inode_used = vec![false; inode_count as usize];
        fs.fs_uuid = derive_uuid(fs.total_blocks, inode_count, fs.block_size);

        fs.begin();
        let now = (fs.clock)();
        let bs = fs.block_size;
        let mut root = Inode::empty(KIND_DIR, Security::new(0o755, 0, 0), now);
        root.nlink = 2;
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        for byte in &mut buf[HEADER_LEN..bs] {
            *byte = 0;
        }
        put_dirent(&mut buf, 0, ROOT_INO, b".");
        put_dirent(&mut buf, 1, ROOT_INO, b"..");
        let db = fs.cow_meta(0, &mut buf, BlockType::Directory, u64::from(ROOT_INO), 0)?;
        root.direct[0] = db;
        root.size = bs as u64;
        fs.inode_used[ROOT_INO as usize] = true;
        fs.write_inode(ROOT_INO, &root)?;
        fs.commit()?;
        Ok(fs)
    }

    /// Open the rustfs volume on `block`, selecting the highest-generation
    /// committed transaction root from the superblock ring and rebuilding the
    /// in-memory free and inode-allocation state by walking it.
    ///
    /// A crash during a previous commit leaves an earlier committed root
    /// selected rather than a torn one (`.junie/RUSTFS.md` §14).
    ///
    /// # Errors
    ///
    /// * [`DriverError::Unsupported`] if the device block size is unsupported.
    /// * [`DriverError::BadMagic`] if no committed superblock slot validates
    ///   (e.g. the device is not a rustfs volume).
    /// * [`DriverError::DeviceFault`] on an unrecoverable block read.
    pub fn open(block: B) -> Result<Self, DriverError> {
        let mut fs = Self::bootstrap(block)?;
        let bs = fs.block_size;
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        let mut best: Option<(Superblock, u128, u64, u64)> = None;
        let mut uuid_pin: Option<u128> = None;
        for slot in 0..RING_SLOTS {
            fs.read_block(slot, &mut buf)?;
            let Some((sb, uuid)) = Superblock::try_decode(&buf[..bs], uuid_pin, slot) else {
                continue;
            };
            if sb.block_size as usize != fs.block_size || sb.total_blocks != fs.total_blocks {
                continue;
            }
            if sb.root_phys < RING_SLOTS || sb.root_phys >= fs.total_blocks {
                continue;
            }
            fs.read_block(sb.root_phys, &mut buf)?;
            if TxnRoot::decode_verify(&buf[..bs], uuid, sb.root_phys, sb.generation).is_err() {
                continue;
            }
            uuid_pin = Some(uuid);
            if best.map_or(true, |(b, _, _, _)| sb.generation > b.generation) {
                best = Some((sb, uuid, slot, sb.generation));
            }
        }
        let (sb, uuid, best_slot, _) = best.ok_or(DriverError::BadMagic)?;

        fs.fs_uuid = uuid;
        fs.inode_count = sb.inode_count;
        fs.generation = sb.generation;
        fs.root_phys = sb.root_phys;
        fs.ring_pos = best_slot + 1;

        fs.read_block(sb.root_phys, &mut buf)?;
        let root = TxnRoot::decode_verify(&buf[..bs], uuid, sb.root_phys, sb.generation)?;
        let n_inode_blocks = as_usize(root.inode_blocks);
        let per = fs.ptrs_per_block();
        let n_map_blocks = fs.n_map_blocks(n_inode_blocks);

        fs.map_index_phys = root.map_index_phys;
        fs.map_blocks = Vec::with_capacity(n_map_blocks);
        fs.inode_map = vec![0u64; n_inode_blocks];
        if root.map_index_phys != 0 {
            fs.read_meta(root.map_index_phys, BlockType::InodeMap, &mut buf)?;
            let mut index = [0u64; MAX_BLOCK_SIZE / 8];
            for (i, slot) in index.iter_mut().take(n_map_blocks).enumerate() {
                *slot = rd_u64(&buf, HEADER_LEN + i * 8);
            }
            for &map_phys in index.iter().take(n_map_blocks) {
                fs.map_blocks.push(map_phys);
            }
            let mut map_buf = [0u8; MAX_BLOCK_SIZE];
            for (i, &map_phys) in fs.map_blocks.clone().iter().enumerate() {
                fs.read_meta(map_phys, BlockType::InodeMap, &mut map_buf)?;
                for j in 0..per {
                    let idx = i * per + j;
                    if idx >= n_inode_blocks {
                        break;
                    }
                    fs.inode_map[idx] = rd_u64(&map_buf, HEADER_LEN + j * 8);
                }
            }
        }

        fs.inode_used = vec![false; sb.inode_count as usize];
        fs.mark_used(sb.root_phys);
        if root.map_index_phys != 0 {
            fs.mark_used(root.map_index_phys);
        }
        for &map_phys in &fs.map_blocks.clone() {
            if map_phys != 0 {
                fs.mark_used(map_phys);
            }
        }
        for &inode_phys in &fs.inode_map.clone() {
            if inode_phys != 0 {
                fs.mark_used(inode_phys);
            }
        }
        for ino in 1..fs.inode_count {
            match fs.read_inode(ino) {
                Ok(inode) => {
                    fs.inode_used[ino as usize] = true;
                    fs.mark_inode_blocks(&inode)?;
                }
                Err(DriverError::NotFound) => {}
                Err(e) => return Err(e),
            }
        }
        fs.alloc_cursor = RING_SLOTS;
        Ok(fs)
    }

    /// Mark every data, directory, and indirect block reachable from `inode`
    /// as used while rebuilding the free bitmap at mount.
    fn mark_inode_blocks(&mut self, inode: &Inode) -> Result<(), DriverError> {
        let bs = self.block_size as u64;
        let blocks = if inode.is_dir() {
            inode.size / bs
        } else {
            inode.size.div_ceil(bs)
        };
        for bi in 0..blocks {
            let ptr = self.block_ptr(inode, bi)?;
            if ptr != 0 {
                self.mark_used(ptr);
            }
        }
        if inode.indirect != 0 {
            self.mark_used(inode.indirect);
        }
        Ok(())
    }

    /// Largest number of blocks a file can address (direct + one indirect
    /// block's worth of pointers).
    fn max_file_blocks(&self) -> u64 {
        DIRECT_PTRS as u64 + self.ptrs_per_block() as u64
    }

    /// The data block backing logical block `bi` of `inode`, `0` for a hole.
    fn block_ptr(&mut self, inode: &Inode, bi: u64) -> Result<u64, DriverError> {
        if bi < DIRECT_PTRS as u64 {
            return Ok(inode.direct[as_usize(bi)]);
        }
        let idx = as_usize(bi - DIRECT_PTRS as u64);
        if idx >= self.ptrs_per_block() {
            return Err(DriverError::LengthOutOfRange);
        }
        if inode.indirect == 0 {
            return Ok(0);
        }
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        self.read_meta(inode.indirect, BlockType::Indirect, &mut buf)?;
        Ok(rd_u64(&buf, HEADER_LEN + idx * 8))
    }

    /// Point logical block `bi` of `inode` at `ptr`, copy-on-writing the
    /// single-indirect block (allocating it on first use).
    fn set_block_ptr(&mut self, inode: &mut Inode, bi: u64, ptr: u64) -> Result<(), DriverError> {
        if bi < DIRECT_PTRS as u64 {
            inode.direct[as_usize(bi)] = ptr;
            return Ok(());
        }
        let idx = as_usize(bi - DIRECT_PTRS as u64);
        if idx >= self.ptrs_per_block() {
            return Err(DriverError::LengthOutOfRange);
        }
        let old = inode.indirect;
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        if old != 0 {
            self.read_meta(old, BlockType::Indirect, &mut buf)?;
        } else {
            for byte in &mut buf[HEADER_LEN..self.block_size] {
                *byte = 0;
            }
        }
        wr_u64(&mut buf, HEADER_LEN + idx * 8, ptr);
        inode.indirect = self.cow_meta(old, &mut buf, BlockType::Indirect, 0, 0)?;
        Ok(())
    }

    /// Copy-on-write a raw (header-less) data block: reuse `old_ptr` when it is
    /// private to this transaction, else allocate a fresh block and defer-free
    /// the old one. Returns the block's physical address (unwritten).
    fn cow_data(&mut self, old_ptr: u64) -> Result<u64, DriverError> {
        if old_ptr != 0 && self.is_txn_private(old_ptr) {
            return Ok(old_ptr);
        }
        let new = self.alloc_block()?;
        if old_ptr != 0 {
            self.free_block(old_ptr);
        }
        Ok(new)
    }

    /// Free every data block backing `inode` and its indirect block.
    fn free_all_blocks(&mut self, inode: &mut Inode) -> Result<(), DriverError> {
        let bs = self.block_size as u64;
        let blocks = inode.size.div_ceil(bs);
        for bi in 0..blocks {
            let ptr = self.block_ptr(inode, bi)?;
            if ptr != 0 {
                self.free_block(ptr);
            }
        }
        if inode.indirect != 0 {
            self.free_block(inode.indirect);
            inode.indirect = 0;
        }
        inode.direct = [0; DIRECT_PTRS];
        inode.size = 0;
        Ok(())
    }

    fn dir_block_count(&self, dir: &Inode) -> u64 {
        dir.size / self.block_size as u64
    }

    /// Resolve `name` within directory `dir`, returning its inode index.
    fn dir_lookup(&mut self, dir: &Inode, name: &[u8]) -> Result<Option<u32>, DriverError> {
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
                    return Ok(Some(ino));
                }
            }
        }
        Ok(None)
    }

    /// Whether `dir` holds no entries other than `.` and `..`.
    fn dir_is_empty(&mut self, dir: &Inode) -> Result<bool, DriverError> {
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
                let name = &buf[base + 8..base + 8 + name_len];
                if name != b"." && name != b".." {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    /// Add directory entry `(child_ino, name)` to `dir`, growing it by one
    /// copy-on-write block when every existing slot is occupied.
    fn add_entry(
        &mut self,
        dir: &mut Inode,
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
                    let new = self.cow_meta(ptr, &mut buf, BlockType::Directory, 0, blk)?;
                    if new != ptr {
                        self.set_block_ptr(dir, blk, new)?;
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
        let new_blk = self.cow_meta(0, &mut buf, BlockType::Directory, 0, blk_index)?;
        self.set_block_ptr(dir, blk_index, new_blk)?;
        dir.size += bs as u64;
        Ok(())
    }

    /// Clear the entry named `name` in `dir`, returning the inode it named.
    fn remove_entry(&mut self, dir: &mut Inode, name: &[u8]) -> Result<u32, DriverError> {
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
                    let new = self.cow_meta(ptr, &mut buf, BlockType::Directory, 0, blk)?;
                    if new != ptr {
                        self.set_block_ptr(dir, blk, new)?;
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
        let bs = self.block_size as u64;
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
                self.read_block(ptr, &mut data)?;
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
        let bs = self.block_size;
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
                self.read_block(old_ptr, &mut blk)?;
            }
            blk[within..within + chunk].copy_from_slice(&data[done..done + chunk]);
            let new_ptr = self.cow_data(old_ptr)?;
            self.write_block(new_ptr, &blk)?;
            self.set_block_ptr(inode, bi, new_ptr)?;
            done += chunk;
            pos += chunk as u64;
        }
        if end > inode.size {
            inode.size = end;
        }
        Ok(done)
    }

    /// Shrink or grow file `inode` to `size`, copy-on-writing the partial tail.
    fn truncate_file(&mut self, inode: &mut Inode, size: u64) -> Result<(), DriverError> {
        let bs = self.block_size as u64;
        if size.div_ceil(bs) > self.max_file_blocks() {
            return Err(DriverError::LengthOutOfRange);
        }
        if size < inode.size {
            let keep = size.div_ceil(bs);
            let had = inode.size.div_ceil(bs);
            for bi in keep..had {
                let ptr = self.block_ptr(inode, bi)?;
                if ptr != 0 {
                    self.free_block(ptr);
                    self.set_block_ptr(inode, bi, 0)?;
                }
            }
            if keep <= DIRECT_PTRS as u64 && inode.indirect != 0 {
                self.free_block(inode.indirect);
                inode.indirect = 0;
            }
            let tail = as_usize(size % bs);
            if tail != 0 {
                let bi = size / bs;
                let old_ptr = self.block_ptr(inode, bi)?;
                if old_ptr != 0 {
                    let mut blk = [0u8; MAX_BLOCK_SIZE];
                    self.read_block(old_ptr, &mut blk)?;
                    for byte in &mut blk[tail..as_usize(bs)] {
                        *byte = 0;
                    }
                    let new_ptr = self.cow_data(old_ptr)?;
                    self.write_block(new_ptr, &blk)?;
                    self.set_block_ptr(inode, bi, new_ptr)?;
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
        if ino == 0 || ino >= self.inode_count {
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
        let bs = self.block_size as u64;
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
            let mut buf = [0u8; MAX_BLOCK_SIZE];
            for byte in &mut buf[HEADER_LEN..self.block_size] {
                *byte = 0;
            }
            put_dirent(&mut buf, 0, child_ino, b".");
            put_dirent(&mut buf, 1, dir_ino, b"..");
            let db = self.cow_meta(0, &mut buf, BlockType::Directory, u64::from(child_ino), 0)?;
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
        self.remove_entry(&mut dir_inode, name)?;
        let now = (self.clock)();
        dir_inode.times.modified = now;
        dir_inode.times.changed = now;
        self.write_inode(dir_ino, &dir_inode)?;
        self.commit()
    }
}

/// Derive a non-zero filesystem UUID from the volume geometry. Stage 1 has no
/// platform RNG dependency (`.junie/RUSTFS.md` §3 — no external crates); a
/// random per-volume UUID arrives with the installer's RNG in a later stage.
/// The value only needs to be stable and non-zero to anchor the §8 block
/// identity checks within one volume.
fn derive_uuid(total_blocks: u64, inode_count: u32, block_size: usize) -> u128 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for word in [total_blocks, u64::from(inode_count), block_size as u64] {
        hash ^= word;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (u128::from(hash) << 64) | u128::from(hash ^ 0x5255_5354_4653_5631)
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
        let per = self.dirents_per_block();
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        let mut seen = 0u64;
        for blk in 0..self.dir_block_count(&dir_inode) {
            let ptr = self.block_ptr(&dir_inode, blk)?;
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
        self.begin();
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
        self.begin();
        let result = self.write_inner(dir, name, offset, data);
        if result.is_err() {
            self.rollback();
        }
        result
    }

    fn truncate(&mut self, dir: NodeId, name: &[u8], size: u64) -> Result<(), DriverError> {
        self.begin();
        let result = self.truncate_inner(dir, name, size);
        if result.is_err() {
            self.rollback();
        }
        result
    }

    fn remove(&mut self, dir: NodeId, name: &[u8]) -> Result<(), DriverError> {
        self.begin();
        let result = self.remove_inner(dir, name);
        if result.is_err() {
            self.rollback();
        }
        result
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        Ok(())
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
