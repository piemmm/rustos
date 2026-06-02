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
use rustos_crypto::{AeadKey, MacKey};

mod btree;
mod crypto;
mod header;
mod integrity;
mod superblock;
mod transaction;

#[cfg(test)]
mod tests;

use crypto::{
    decrypt_region, encrypt_region, CryptoHeader, VolumeKeys, CRYPTO_HEADER_LEN, CRYPTO_TRAILER,
};
pub use crypto::{VolumeKey, VOLUME_KEY_LEN};
use integrity::{
    logical_hash, physical_checksum, read_compression, write_compression, Compression, DataFault,
    COMPRESSION_DESCRIPTOR_LEN, DATA_INTEGRITY_TRAILER, LOGICAL_HASH_LEN, PHYS_CHECKSUM_LEN,
};

use header::{BlockHeader, BlockType, FORMAT_VERSION, HEADER_LEN, HEADER_MAGIC};
use superblock::{slot_block, Superblock, RING_BLOCKS, RING_SLOTS};
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

/// Encode an extent value: physical start block followed by run length.
fn encode_extent(phys: u64, len: u64) -> [u8; EXTENT_VALUE_LEN] {
    let mut value = [0u8; EXTENT_VALUE_LEN];
    value[0..8].copy_from_slice(&phys.to_le_bytes());
    value[8..16].copy_from_slice(&len.to_le_bytes());
    value
}

/// Decode an extent value into `(physical start, run length)`.
fn decode_extent(value: &[u8]) -> (u64, u64) {
    (rd_u64(value, 0), rd_u64(value, 8))
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
/// Physical block of this inode's per-file extent-tree root, or `0` when the
/// file has no mapped blocks (`btree` module).
const I_EXTENT_ROOT: usize = 152;

/// In-memory image of one on-disk inode.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Inode {
    kind: u32,
    sec: Security,
    nlink: u32,
    size: u64,
    times: NodeTimes,
    /// Physical block of this file's copy-on-write extent-tree root, `0` when
    /// the file maps no blocks yet.
    extent_root: u64,
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
            extent_root: 0,
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
        Ok(Some(Self {
            kind,
            sec,
            nlink: rd_u32(buf, I_NLINK),
            size: rd_u64(buf, I_SIZE),
            times,
            extent_root: rd_u64(buf, I_EXTENT_ROOT),
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
        wr_u64(buf, I_EXTENT_ROOT, self.extent_root);
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
/// superblock ring. That root names the **inode tree** (a copy-on-write
/// B-tree keyed by inode number; `btree` module) and the next free inode
/// number. Each file inode in turn names its own **extent tree** mapping a
/// logical block offset to a physical run. The in-memory free-block bitmap is
/// rebuilt by walking those trees at [`RustFs::open`] and kept in step as
/// transactions commit. A volume is created with [`RustFs::format`] and
/// reopened with [`RustFs::open`].
pub struct RustFs<B: Block> {
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
    root_phys: u64,
    free: Vec<u64>,
    free_count: u64,
    txn_allocated: Vec<u64>,
    txn_freed: Vec<u64>,
    txn_private: Vec<bool>,
    saved_inode_tree_root: u64,
    saved_next_ino: u64,
    alloc_cursor: u64,
    meta_cursor: u64,
    clock: fn() -> Time64,
}

/// Value width of one extent record: physical start block plus run length.
const EXTENT_VALUE_LEN: usize = 16;

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

/// A file's extent-tree record shape: a `(phys, len)` run keyed by its
/// starting logical block, owned by inode `ino`.
fn extent_spec(ino: u32) -> btree::TreeSpec {
    btree::TreeSpec {
        value_len: EXTENT_VALUE_LEN,
        owner: u64::from(ino),
    }
}

impl<B: Block> RustFs<B> {
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
    /// (`docs/src/filesystem/rustfs-spec.md` §10).
    fn data_capacity(&self) -> u64 {
        (self.block_size - CRYPTO_TRAILER - COMPRESSION_DESCRIPTOR_LEN - DATA_INTEGRITY_TRAILER)
            as u64
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
            if (*w >> bit) & 1 == 0 {
                self.free_count = self.free_count.saturating_sub(1);
            }
            *w |= 1u64 << bit;
        }
    }

    fn mark_free(&mut self, block: u64) {
        let word = as_usize(block / 64);
        let bit = block % 64;
        if let Some(w) = self.free.get_mut(word) {
            if (*w >> bit) & 1 == 1 {
                self.free_count = self.free_count.saturating_add(1);
            }
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

    /// Install a freshly derived or unwrapped key set as the volume's working
    /// keys (`crypto` module).
    fn apply_keys(&mut self, keys: &VolumeKeys) {
        self.mac_key = keys.mac_key;
        self.filename_key = keys.filename_key;
        self.content_key = keys.content_key;
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

    /// The companion mirror of metadata block `phys`: its adjacent block at
    /// `phys + 1`. Every metadata block is stored twice — at `phys` and at
    /// `companion(phys)` — so a stale, torn, or bit-rotted copy can be
    /// repaired from the other (`docs/src/filesystem/rustfs-spec.md` §5, §8).
    /// One rule covers superblock-ring slots, transaction roots, B-tree nodes,
    /// and directory blocks, so there is a single redundancy mechanism
    /// (`AGENTS.md` §2.2).
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
    /// from it (`docs/src/filesystem/rustfs-spec.md` §8 — try redundant
    /// copies, repair bad from good). On success `buf` holds the good block's
    /// bytes. If neither copy authenticates the read fails closed with
    /// [`DriverError::DeviceFault`] (`AGENTS.md` §5.4).
    fn read_meta(
        &mut self,
        phys: u64,
        expect_type: BlockType,
        buf: &mut [u8],
    ) -> Result<BlockHeader, DriverError> {
        let bs = self.block_size;
        self.read_block(phys, buf)?;
        if let Ok(header) =
            BlockHeader::decode_verify(&buf[..bs], expect_type, self.fs_uuid, phys, &self.mac_key)
        {
            self.decrypt_meta_payload(expect_type, buf, phys)?;
            return Ok(header);
        }
        // Primary failed: fall back to the companion mirror, validating it
        // against the *primary's* identity (both copies carry that address).
        self.read_block(Self::companion(phys), buf)?;
        let header =
            BlockHeader::decode_verify(&buf[..bs], expect_type, self.fs_uuid, phys, &self.mac_key)?;
        // The companion is good: repair the primary copy from it (the
        // still-encrypted bytes), then decrypt the caller's copy.
        self.write_block(phys, buf)?;
        self.decrypt_meta_payload(expect_type, buf, phys)?;
        Ok(header)
    }

    /// After a metadata block authenticates, decrypt its at-rest-encrypted
    /// payload in place for the caller. Only directory blocks carry an
    /// encrypted payload (the entry names); every other metadata block is
    /// authenticated-only and returned unchanged. The block authenticated
    /// before this point, so decryption cannot yield mis-decrypted bytes
    /// (`docs/src/filesystem/rustfs-spec.md` §6 read path).
    fn decrypt_meta_payload(
        &self,
        block_type: BlockType,
        buf: &mut [u8],
        phys: u64,
    ) -> Result<(), DriverError> {
        if block_type != BlockType::Directory {
            return Ok(());
        }
        let off = self.crypto_trailer_offset();
        let (region, trailer) = buf[HEADER_LEN..self.block_size].split_at_mut(off - HEADER_LEN);
        decrypt_region(&self.filename_key, region, trailer, phys)
            .map_err(|_| DriverError::DeviceFault)
    }

    /// Mark a metadata block and its companion mirror used in the free-space
    /// bitmap (the rebuild and live paths both account for both copies).
    fn mark_meta_used(&mut self, phys: u64) {
        self.mark_used(phys);
        self.mark_used(Self::companion(phys));
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
    /// the current transaction. For `metadata` the returned block is the
    /// **primary** of a mirrored pair: its companion at `companion(primary)`
    /// is reserved at the same time, so the two physical copies a metadata
    /// block needs (`docs/src/filesystem/rustfs-spec.md` §5) are always
    /// adjacent.
    ///
    /// Data and metadata draw from opposite ends of the pool: file data scans
    /// **upward** from the low end and metadata (tree nodes, the transaction
    /// root, directory blocks) scans **downward** from the high end. Keeping
    /// the two streams apart lets a large sequential write land in physically
    /// contiguous blocks even though it interleaves extent-tree growth, so it
    /// collapses to one extent run rather than fragmenting (`docs/src/filesystem/rustfs-spec.md`
    /// §6). Metadata also draws on the last [`METADATA_RESERVE`] free blocks so
    /// a delete or other shrinking transaction can still copy-on-write itself
    /// on an otherwise-full volume; data allocation stops at the reserve and
    /// fails closed with [`DriverError::NoSpace`].
    fn alloc_block(&mut self, metadata: bool) -> Result<u64, DriverError> {
        if metadata {
            self.alloc_meta_pair()
        } else {
            self.alloc_data_block()
        }
    }

    /// Mark `block` used, private to this transaction, and recorded for
    /// rollback.
    fn claim_block(&mut self, block: u64) {
        self.mark_used(block);
        if let Some(slot) = self.txn_private.get_mut(as_usize(block)) {
            *slot = true;
        }
        self.txn_allocated.push(block);
    }

    /// Allocate one data block, scanning **upward** from the low end.
    fn alloc_data_block(&mut self) -> Result<u64, DriverError> {
        if self.free_count <= METADATA_RESERVE {
            return Err(DriverError::NoSpace);
        }
        let start = RING_BLOCKS;
        let total = self.total_blocks;
        let span = total.saturating_sub(start);
        let mut scanned = 0u64;
        let mut block = self.alloc_cursor.max(start);
        while scanned < span {
            if block >= total {
                block = start;
            }
            if !self.bit_used(block) {
                self.claim_block(block);
                self.alloc_cursor = block + 1;
                return Ok(block);
            }
            block += 1;
            scanned += 1;
        }
        Err(DriverError::NoSpace)
    }

    /// Allocate a mirrored metadata pair, scanning **downward** from the high
    /// end for two adjacent free blocks `(primary, primary + 1)`. Returns the
    /// primary; both blocks are claimed. Fails closed with
    /// [`DriverError::NoSpace`] when no adjacent free pair remains
    /// (`AGENTS.md` §5.4 / §2.9) — never a panic.
    fn alloc_meta_pair(&mut self) -> Result<u64, DriverError> {
        let start = RING_BLOCKS;
        let total = self.total_blocks;
        // `hi` is the companion (upper) block; the primary is `hi - 1`, which
        // must stay at or above the reserved ring region.
        let mut hi = self.meta_cursor.clamp(start + 1, total - 1);
        let span = total.saturating_sub(start + 1);
        let mut scanned = 0u64;
        while scanned <= span {
            if hi < start + 1 {
                hi = total - 1;
            }
            let primary = hi - 1;
            if !self.bit_used(hi) && !self.bit_used(primary) {
                self.claim_block(primary);
                self.claim_block(hi);
                self.meta_cursor = primary.saturating_sub(1).max(start + 1);
                return Ok(primary);
            }
            hi = hi.saturating_sub(1);
            scanned += 1;
        }
        Err(DriverError::NoSpace)
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
    /// (`docs/src/filesystem/rustfs-spec.md` §2).
    fn free_block(&mut self, phys: u64) {
        if phys == 0 {
            return;
        }
        if self.is_txn_private(phys) {
            self.mark_free(phys);
            if let Some(slot) = self.txn_private.get_mut(as_usize(phys)) {
                *slot = false;
            }
        } else {
            self.txn_freed.push(phys);
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
        // A directory block's entry names are encrypted at rest under the
        // filename key before the block is authenticated, so the keyed
        // authenticator seals the ciphertext (encrypt-then-MAC; the read path
        // authenticates then decrypts — `docs/src/filesystem/rustfs-spec.md`
        // §6, §7). Other metadata blocks are authenticated-only.
        if block_type == BlockType::Directory {
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
        self.txn_allocated.clear();
        self.txn_freed.clear();
        self.saved_inode_tree_root = self.inode_tree_root;
        self.saved_next_ino = self.next_ino;
    }

    /// Discard an operation that failed before committing: restore the inode
    /// tree root and inode counter and free this transaction's allocations.
    /// Nothing was published, so the committed on-disk root is untouched.
    fn rollback(&mut self) {
        self.inode_tree_root = self.saved_inode_tree_root;
        self.next_ino = self.saved_next_ino;
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
    }

    /// Commit the staged transaction. The inode tree and every extent tree are
    /// already copy-on-written in place as the operation runs, so commit just
    /// writes the new transaction root naming the inode-tree root, then
    /// publishes the next superblock-ring slot pointing at it
    /// (`transaction` / `superblock`).
    fn commit(&mut self) -> Result<(), DriverError> {
        let bs = self.block_size;
        let next_gen = self.generation.wrapping_add(1);
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        let old_root = self.root_phys;
        let root_phys = self.alloc_block(true)?;
        let root = TxnRoot {
            generation: next_gen,
            inode_tree_root: self.inode_tree_root,
            next_ino: self.next_ino,
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
        };
        sb.seal(
            &mut buf[..bs],
            self.fs_uuid,
            slot,
            &self.mac_key,
            &self.crypto_header,
        )?;
        self.write_meta(slot, &buf)?;
        // Commit point passed: the new root (and its mirror) is durably
        // published, as is the superblock slot pointing at it.
        self.generation = next_gen;
        self.ring_pos = self.ring_pos.wrapping_add(1);
        self.root_phys = root_phys;
        self.free_meta(old_root);
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
        let words = as_usize(total_blocks.div_ceil(64));
        let mut fs = Self {
            block,
            fs_uuid: 0,
            mac_key: [0u8; rustos_crypto::MAC_KEY_LEN],
            filename_key: [0u8; rustos_crypto::AEAD_KEY_LEN],
            content_key: [0u8; rustos_crypto::AEAD_KEY_LEN],
            crypto_header: [0u8; CRYPTO_HEADER_LEN],
            block_size,
            total_blocks,
            inode_hint: 0,
            generation: 0,
            ring_pos: 0,
            inode_tree_root: 0,
            next_ino: u64::from(ROOT_INO) + 1,
            root_phys: 0,
            free: vec![0u64; words],
            free_count: total_blocks,
            txn_allocated: Vec::new(),
            txn_freed: Vec::new(),
            txn_private: vec![false; as_usize(total_blocks)],
            saved_inode_tree_root: 0,
            saved_next_ino: 0,
            alloc_cursor: RING_BLOCKS,
            meta_cursor: total_blocks - 1,
            clock: epoch_clock,
        };
        for block in 0..RING_BLOCKS {
            fs.mark_used(block);
        }
        Ok(fs)
    }

    /// Lay a fresh, empty rustfs volume onto `block` and return it mounted.
    /// `inode_hint` sizes nothing on disk any more — the inode tree grows on
    /// demand — but it is retained in the frozen `format` signature and stored
    /// in the superblock for tools, and a value below two is still rejected so
    /// at least the root directory fits.
    ///
    /// The volume is encrypted at rest under `volume_key` (the installer's /
    /// recovery flow's key material): `format` provisions the per-volume key
    /// hierarchy (a wrapped master key deriving the metadata-authentication,
    /// filename, and content keys) and stores only the wrapped master key on
    /// disk. There is **no** plaintext layout path
    /// (`docs/src/filesystem/rustfs-spec.md` §5, §7). Compression and dedupe
    /// remain later stages of the staged build (§15).
    ///
    /// # Errors
    ///
    /// * [`DriverError::Unsupported`] if the device block size is unsupported.
    /// * [`DriverError::NoSpace`] if the device is too small or `inode_hint`
    ///   is below two.
    /// * [`DriverError::DeviceFault`] on an unrecoverable block write.
    pub fn format(block: B, inode_hint: u32, volume_key: &VolumeKey) -> Result<Self, DriverError> {
        let mut fs = Self::bootstrap(block)?;
        if inode_hint < 2 {
            return Err(DriverError::NoSpace);
        }
        fs.inode_hint = inode_hint;
        fs.fs_uuid = derive_uuid(fs.total_blocks, inode_hint, fs.block_size);
        // Provision the per-volume key hierarchy: a master key wrapped under
        // the caller's volume key, deriving the metadata-authentication,
        // filename, and content keys. There is no plaintext path
        // (`docs/src/filesystem/rustfs-spec.md` §5, §7).
        let (crypto_header, keys) =
            crypto::provision(volume_key, fs.fs_uuid).map_err(|_| DriverError::DeviceFault)?;
        fs.apply_keys(&keys);
        crypto_header.encode(&mut fs.crypto_header);

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
        fs.extent_assign(&mut root, ROOT_INO, 0, db)?;
        root.size = bs as u64;
        fs.write_inode(ROOT_INO, &root)?;
        fs.commit()?;
        Ok(fs)
    }

    /// Open the rustfs volume on `block`, selecting the highest-generation
    /// committed transaction root from the superblock ring and rebuilding the
    /// in-memory free and inode-allocation state by walking it.
    ///
    /// A crash during a previous commit leaves an earlier committed root
    /// selected rather than a torn one (`docs/src/filesystem/rustfs-spec.md` §14).
    ///
    /// The volume is encrypted: `volume_key` must be the key material the
    /// volume was formatted with. `open` recovers the key hierarchy by
    /// unwrapping the master key stored in a superblock slot's discovery
    /// header; a wrong key never unwraps and the mount is refused with
    /// [`DriverError::PermissionDenied`], fail-closed (`AGENTS.md` §5.4),
    /// never a panic (§2.9).
    ///
    /// # Errors
    ///
    /// * [`DriverError::Unsupported`] if the device block size is unsupported.
    /// * [`DriverError::PermissionDenied`] if `volume_key` does not unwrap the
    ///   volume (wrong key on an otherwise-valid rustfs volume).
    /// * [`DriverError::BadMagic`] if no committed superblock slot validates
    ///   (e.g. the device is not a rustfs volume).
    /// * [`DriverError::DeviceFault`] on an unrecoverable block read.
    pub fn open(block: B, volume_key: &VolumeKey) -> Result<Self, DriverError> {
        let mut fs = Self::bootstrap(block)?;
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
            let Some((sb, uuid)) = fs.read_sb_slot(primary, uuid_pin, &mut buf) else {
                continue;
            };
            if sb.block_size as usize != fs.block_size || sb.total_blocks != fs.total_blocks {
                continue;
            }
            if sb.root_phys < RING_BLOCKS || sb.root_phys >= fs.total_blocks {
                continue;
            }
            if fs
                .read_txn_root(uuid, sb.root_phys, sb.generation, &mut buf)
                .is_err()
            {
                continue;
            }
            if best.map_or(true, |(b, _, _)| sb.generation > b.generation) {
                best = Some((sb, uuid, slot));
            }
        }
        let (sb, _uuid, best_slot) = best.ok_or(DriverError::BadMagic)?;

        fs.inode_hint = sb.inode_count;
        fs.generation = sb.generation;
        fs.root_phys = sb.root_phys;
        fs.ring_pos = best_slot + 1;

        let root = fs.read_txn_root(fs.fs_uuid, sb.root_phys, sb.generation, &mut buf)?;
        fs.inode_tree_root = root.inode_tree_root;
        fs.next_ino = root.next_ino;

        // Rebuild the free-block bitmap by walking the live trees: the
        // superblock ring (reserved in `bootstrap`), the published transaction
        // root, every inode-tree node, and, for each inode, its extent-tree
        // nodes plus the physical runs they map. Every metadata block accounts
        // for both its physical copies (`docs/src/filesystem/rustfs-spec.md`
        // §4 — free space is rebuildable; §5 — two copies).
        fs.mark_meta_used(sb.root_phys);
        let inode_spec = inode_spec();
        for node in fs.btree_collect_nodes(fs.inode_tree_root, inode_spec)? {
            fs.mark_meta_used(node);
        }
        let inodes = fs.btree_collect_entries(fs.inode_tree_root, inode_spec)?;
        for (ino, value) in inodes {
            let inode = Inode::decode(&value)?.ok_or(DriverError::DeviceFault)?;
            let ino = u32::try_from(ino).map_err(|_| DriverError::DeviceFault)?;
            fs.mark_inode_blocks(ino, &inode)?;
        }
        fs.alloc_cursor = RING_BLOCKS;
        fs.meta_cursor = fs.total_blocks - 1;
        Ok(fs)
    }

    /// Establish the working key set by unwrapping the master key with
    /// `volume_key` from a superblock slot's plaintext crypto discovery header
    /// (`crypto` module). Sets [`Self::fs_uuid`], the working keys, and the
    /// encoded discovery header that every commit re-publishes.
    ///
    /// # Errors
    ///
    /// * [`DriverError::PermissionDenied`] if a structurally-valid rustfs
    ///   superblock is present but `volume_key` does not unwrap it (wrong key)
    ///   — fail-closed (`AGENTS.md` §5.4).
    /// * [`DriverError::BadMagic`] if no slot even looks like a rustfs
    ///   superblock (the device is not a rustfs volume).
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
    /// (`docs/src/filesystem/rustfs-spec.md` §8). Returns the decoded slot and
    /// its UUID, or `None` when neither copy authenticates (the ring scan then
    /// skips the slot). Authenticated under the volume's metadata-authentication
    /// key, recovered in [`Self::establish_keys`].
    fn read_sb_slot(
        &mut self,
        primary: u64,
        uuid_pin: Option<u128>,
        buf: &mut [u8],
    ) -> Option<(Superblock, u128)> {
        let bs = self.block_size;
        self.read_block(primary, buf).ok()?;
        if let Some((sb, uuid)) =
            Superblock::try_decode(&buf[..bs], uuid_pin, primary, &self.mac_key)
        {
            return Some((sb, uuid));
        }
        self.read_block(Self::companion(primary), buf).ok()?;
        let (sb, uuid) = Superblock::try_decode(&buf[..bs], uuid_pin, primary, &self.mac_key)?;
        let _ = self.write_block(primary, buf);
        Some((sb, uuid))
    }

    /// Read the transaction root at `root_phys`, falling back to its companion
    /// mirror and repairing the primary from a good companion
    /// (`docs/src/filesystem/rustfs-spec.md` §8). On success `buf` holds the
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
        self.read_block(root_phys, buf)?;
        if let Ok(root) =
            TxnRoot::decode_verify(&buf[..bs], uuid, root_phys, expect_generation, &key)
        {
            return Ok(root);
        }
        self.read_block(Self::companion(root_phys), buf)?;
        let root = TxnRoot::decode_verify(&buf[..bs], uuid, root_phys, expect_generation, &key)?;
        self.write_block(root_phys, buf)?;
        Ok(root)
    }

    /// Mark every extent-tree node and every physical run reachable from
    /// `inode` (number `ino`) as used while rebuilding the free bitmap at
    /// mount.
    fn mark_inode_blocks(&mut self, ino: u32, inode: &Inode) -> Result<(), DriverError> {
        let spec = extent_spec(ino);
        for node in self.btree_collect_nodes(inode.extent_root, spec)? {
            self.mark_meta_used(node);
        }
        // A directory's content blocks are themselves metadata
        // ([`BlockType::Directory`], mirrored pairs); a regular file's are
        // single-copy data. Account for the directory mirror so the rebuilt
        // free set matches the live one (`docs/src/filesystem/rustfs-spec.md`
        // §5).
        let is_dir = inode.is_dir();
        for (_, value) in self.btree_collect_entries(inode.extent_root, spec)? {
            let (phys, len) = decode_extent(&value);
            for b in 0..len {
                if is_dir {
                    self.mark_meta_used(phys + b);
                } else {
                    self.mark_used(phys + b);
                }
            }
        }
        Ok(())
    }

    /// Upper bound on a file's block count: the whole device. The extent tree
    /// removes the Stage-1 direct/indirect addressing cap, so a file may span
    /// the volume (`docs/src/filesystem/rustfs-spec.md` §6).
    fn max_file_blocks(&self) -> u64 {
        self.total_blocks
    }

    /// The data block backing logical block `bi` of `inode`, `0` for a hole.
    /// Resolves the extent run covering `bi` with a floor lookup.
    fn block_ptr(&mut self, inode: &Inode, bi: u64) -> Result<u64, DriverError> {
        let spec = extent_spec(0);
        match self.btree_get_floor(inode.extent_root, bi, spec)? {
            Some((start, value)) => {
                let (phys, len) = decode_extent(&value);
                if bi < start + len {
                    Ok(phys + (bi - start))
                } else {
                    Ok(0)
                }
            }
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
        let (phys, len) = decode_extent(&value);
        if bi >= start + len {
            return Ok(());
        }
        inode.extent_root = self.btree_remove(inode.extent_root, start, spec)?;
        if bi > start {
            let left = encode_extent(phys, bi - start);
            inode.extent_root = self.btree_insert(inode.extent_root, start, &left, spec)?;
        }
        let end = start + len;
        if bi + 1 < end {
            let rphys = phys + (bi + 1 - start);
            let right = encode_extent(rphys, end - (bi + 1));
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
        if bi > 0 {
            if let Some((ls, value)) = self.btree_get_floor(inode.extent_root, bi - 1, spec)? {
                let (lp, ll) = decode_extent(&value);
                if ls + ll == bi && lp + ll == ptr {
                    inode.extent_root = self.btree_remove(inode.extent_root, ls, spec)?;
                    start = ls;
                    phys = lp;
                    len = ll + 1;
                }
            }
        }
        if let Some((rs, value)) = self.btree_get_floor(inode.extent_root, bi + 1, spec)? {
            let (rp, rl) = decode_extent(&value);
            if rs == bi + 1 && phys + len == rp {
                inode.extent_root = self.btree_remove(inode.extent_root, rs, spec)?;
                len += rl;
            }
        }
        let value = encode_extent(phys, len);
        inode.extent_root = self.btree_insert(inode.extent_root, start, &value, spec)?;
        Ok(())
    }

    /// Copy-on-write a raw (header-less) data block: reuse `old_ptr` when it is
    /// private to this transaction, else allocate a fresh block and defer-free
    /// the old one. Returns the block's physical address (unwritten).
    fn cow_data(&mut self, old_ptr: u64) -> Result<u64, DriverError> {
        if old_ptr != 0 && self.is_txn_private(old_ptr) {
            return Ok(old_ptr);
        }
        let new = self.alloc_block(false)?;
        if old_ptr != 0 {
            self.free_block(old_ptr);
        }
        Ok(new)
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

    /// Read the data block at `phys`, verify its two-layer integrity field, and
    /// decrypt its content in place, leaving the plaintext in
    /// `buf[..data_capacity()]` (`docs/src/filesystem/rustfs-spec.md` §6).
    ///
    /// The read path is the spec's: verify the fast physical checksum over the
    /// at-rest block first (so media corruption is caught cheaply, before the
    /// AEAD), then authenticate-and-decrypt the content, then verify the
    /// plaintext against its stored logical hash. Each layer is kept distinct
    /// ([`DataFault`]) even though all three surface as one frozen
    /// [`DriverError::DeviceFault`] (§9).
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] on a read failure or on any integrity
    /// layer failing (`AGENTS.md` §5.4 / §2.9 — fail closed, never a panic).
    fn read_data_block(&mut self, phys: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        self.read_data_block_classified(phys, buf)
            .map_err(|_| DriverError::DeviceFault)
    }

    /// As [`read_data_block`](Self::read_data_block), but reports *which*
    /// integrity layer rejected the block. The classification drives the Stage
    /// 5 tests and is the seam Stage 8 scrub / Stage 11 health will record
    /// against; production callers go through
    /// [`read_data_block`](Self::read_data_block) and see only a fail-closed
    /// [`DriverError::DeviceFault`].
    fn read_data_block_classified(&mut self, phys: u64, buf: &mut [u8]) -> Result<(), DataFault> {
        self.read_block(phys, buf)
            .map_err(|_| DataFault::Physical)?;
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
        // Decompress after decrypt and before verifying the logical hash
        // (`docs/src/filesystem/rustfs-spec.md` §6 read path). The hash always
        // covers the recovered plaintext, so the Stage 7 dedupe seam is
        // unaffected by whether the record was stored compressed or raw.
        let desc = read_compression(&buf[desc_off..desc_off + COMPRESSION_DESCRIPTOR_LEN])?;
        if desc.compressed {
            let stored_len = as_usize(u64::from(desc.stored_len));
            if stored_len > cap {
                return Err(DataFault::Logical);
            }
            let mut plain = [0u8; MAX_BLOCK_SIZE];
            let produced = rustos_compress::decompress(&buf[..stored_len], &mut plain[..cap])
                .map_err(|_| DataFault::Logical)?;
            if produced != cap {
                return Err(DataFault::Logical);
            }
            buf[..cap].copy_from_slice(&plain[..cap]);
        }
        let mut expect = [0u8; LOGICAL_HASH_LEN];
        expect.copy_from_slice(&buf[hash_off..hash_off + LOGICAL_HASH_LEN]);
        if logical_hash(&buf[..cap]) != expect {
            return Err(DataFault::Logical);
        }
        Ok(())
    }

    /// Compress the content in `buf[..data_capacity()]`, encrypt the stored
    /// representation under the content key, seal the compression descriptor
    /// and the data-integrity trailer (logical hash of the plaintext, then a
    /// fast physical checksum over the at-rest bytes), and write the resulting
    /// block to `phys`. The nonce is unique per `(phys, generation)` so
    /// copy-on-write never reuses a `(key, nonce)` pair (`crypto` module).
    ///
    /// The pipeline is the spec's: `compress -> encrypt`
    /// (`docs/src/filesystem/rustfs-spec.md` §6, §10). Compression runs over
    /// the plaintext; when the compressed frame is not smaller than the
    /// logical capacity the record is stored **raw** (a §1 allowed adaptive
    /// choice). Either way the full content slot is encrypted, so the crypto
    /// and integrity layers are identical for compressed and raw records and
    /// the logical hash always names the plaintext.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] on a seal failure or a block write failure.
    fn write_data_block(&mut self, phys: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        let cap = as_usize(self.data_capacity());
        let next_gen = self.generation.wrapping_add(1);
        // The logical hash names the *plaintext*, so it is taken before both
        // compression and encryption (`docs/src/filesystem/rustfs-spec.md` §6
        // write path).
        let hash = logical_hash(&buf[..cap]);

        // Compress the plaintext into scratch; keep it only when it wins (the
        // frame is strictly smaller than the logical capacity), otherwise store
        // the record raw. The content slot is always `cap` bytes — a compressed
        // record stores fewer *at-rest* bytes but a logical block still maps one
        // file block, so the slot is zero-padded after the compressed frame.
        let mut scratch = [0u8; MAX_BLOCK_SIZE];
        let desc = match rustos_compress::compress(&buf[..cap], &mut scratch[..cap]) {
            Ok(n) if n < cap => {
                buf[..n].copy_from_slice(&scratch[..n]);
                buf[n..cap].fill(0);
                Compression {
                    compressed: true,
                    stored_len: as_u32(n),
                }
            }
            _ => Compression {
                compressed: false,
                stored_len: as_u32(cap),
            },
        };

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
        write_compression(
            &mut buf[desc_off..desc_off + COMPRESSION_DESCRIPTOR_LEN],
            desc,
        );
        buf[hash_off..hash_off + LOGICAL_HASH_LEN].copy_from_slice(&hash);
        // The physical checksum covers the at-rest representation: ciphertext,
        // crypto trailer, compression descriptor, and logical hash — everything
        // before the checksum.
        let csum_off = self.phys_checksum_offset();
        let checksum = physical_checksum(&buf[..csum_off]);
        buf[csum_off..csum_off + PHYS_CHECKSUM_LEN].copy_from_slice(&checksum);
        self.write_block(phys, buf)
    }

    /// Free every physical run backing `inode` (number `ino`) and every node
    /// of its extent tree, leaving an empty zero-length file.
    ///
    /// A directory's content blocks are metadata mirrored pairs, so they are
    /// freed with their companion ([`Self::free_meta`]); a regular file's are
    /// single-copy data ([`Self::free_block`]).
    fn free_all_blocks(&mut self, inode: &mut Inode, ino: u32) -> Result<(), DriverError> {
        let spec = extent_spec(ino);
        let is_dir = inode.is_dir();
        for (_, value) in self.btree_collect_entries(inode.extent_root, spec)? {
            let (phys, len) = decode_extent(&value);
            for b in 0..len {
                if is_dir {
                    self.free_meta(phys + b);
                } else {
                    self.free_block(phys + b);
                }
            }
        }
        for node in self.btree_collect_nodes(inode.extent_root, spec)? {
            self.free_meta(node);
        }
        inode.extent_root = 0;
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
        let mut data = [0u8; MAX_BLOCK_SIZE];
        while pos < end {
            let bi = pos / cap;
            let within = as_usize(pos % cap);
            let chunk = as_usize((cap - within as u64).min(end - pos));
            let ptr = self.block_ptr(inode, bi)?;
            if ptr == 0 {
                for byte in &mut out[done..done + chunk] {
                    *byte = 0;
                }
            } else {
                self.read_data_block(ptr, &mut data)?;
                out[done..done + chunk].copy_from_slice(&data[within..within + chunk]);
            }
            done += chunk;
            pos += chunk as u64;
        }
        Ok(done)
    }

    /// Copy-on-write `data` into file `inode` (number `ino`) at `offset`.
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
        let mut done = 0usize;
        let mut pos = offset;
        let mut blk = [0u8; MAX_BLOCK_SIZE];
        while done < data.len() {
            let bi = pos / cap;
            let within = as_usize(pos % cap);
            let chunk = (capu - within).min(data.len() - done);
            let old_ptr = self.block_ptr(inode, bi)?;
            for byte in &mut blk[..capu] {
                *byte = 0;
            }
            if (within != 0 || chunk != capu) && old_ptr != 0 {
                self.read_data_block(old_ptr, &mut blk)?;
            }
            blk[within..within + chunk].copy_from_slice(&data[done..done + chunk]);
            let new_ptr = self.cow_data(old_ptr)?;
            self.write_data_block(new_ptr, &mut blk)?;
            self.extent_assign(inode, ino, bi, new_ptr)?;
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
            self.free_extent_tail(inode, ino, keep)?;
            let tail = as_usize(size % cap);
            if tail != 0 {
                let bi = size / cap;
                let old_ptr = self.block_ptr(inode, bi)?;
                if old_ptr != 0 {
                    let mut blk = [0u8; MAX_BLOCK_SIZE];
                    self.read_data_block(old_ptr, &mut blk)?;
                    for byte in &mut blk[tail..as_usize(cap)] {
                        *byte = 0;
                    }
                    let new_ptr = self.cow_data(old_ptr)?;
                    self.write_data_block(new_ptr, &mut blk)?;
                    self.extent_assign(inode, ino, bi, new_ptr)?;
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
        for (start, value) in self.btree_collect_entries(inode.extent_root, spec)? {
            let (phys, len) = decode_extent(&value);
            let end = start + len;
            if end <= keep {
                continue;
            }
            let cut = keep.max(start);
            for b in cut..end {
                self.free_block(phys + (b - start));
            }
            inode.extent_root = self.btree_remove(inode.extent_root, start, spec)?;
            if cut > start {
                let head = encode_extent(phys, cut - start);
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
            self.extent_assign(&mut child, child_ino, 0, db)?;
            child.size = bs;
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
        let written = self.write_file(&mut child, child_ino, offset, data)?;
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
        self.free_all_blocks(&mut child, child_ino)?;
        self.free_inode(child_ino)?;
        if child.is_dir() {
            dir_inode.nlink = dir_inode.nlink.saturating_sub(1);
        }
        self.remove_entry(&mut dir_inode, dir_ino, name)?;
        let now = (self.clock)();
        dir_inode.times.modified = now;
        dir_inode.times.changed = now;
        self.write_inode(dir_ino, &dir_inode)?;
        self.commit()
    }
}

/// Derive a non-zero filesystem UUID from the volume geometry. Stage 1 has no
/// platform RNG dependency (`docs/src/filesystem/rustfs-spec.md` §3 — no external crates); a
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
