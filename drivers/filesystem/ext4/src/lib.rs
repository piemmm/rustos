//! TAIRiX ext4 filesystem driver (read/write).
//!
//! Attaches an ext2/ext3/ext4 volume sitting behind any
//! [`tairix_abi::driver::block::Block`] device and exposes it through
//! the versioned [`tairix_abi::driver::filesystem::FilesystemRead`],
//! [`FilesystemWrite`], and [`FilesystemSecurity`] surfaces
//! (new behaviour ships as a new trait,
//! never by widening the frozen mount/unmount
//! [`Filesystem`](tairix_abi::driver::filesystem::Filesystem)).
//!
//! The driver makes **no** permission decisions: owner, mode, ACL, and
//! the capability gate live in the VFS metadata layer that mounts
//! this driver (the VFS is the policy point, this is
//! raw structural I/O).
//!
//! # Public surface
//!
//! Per the only public *function* is [`register`].
//! [`Ext4`] is a public *type* the driver host instantiates with
//! [`Ext4::open`]; the host reaches into it only through the
//! [`FilesystemRead`], [`FilesystemWrite`], and [`FilesystemSecurity`]
//! traits.
//!
//! # Scope
//!
//! Reads the modern ext4 on-disk shape — block sizes 1024..=4096, 128-
//! or 256-byte inodes, 32- or 64-byte group descriptors (the `64bit`
//! feature), extent-mapped inodes (the default since ext4) including
//! multi-level extent trees, and the classic ext2/ext3 indirect block
//! map (direct + single/double/triple indirect) — and linear
//! (non-hash-indexed leaf) directory blocks. The root block of a
//! hash-indexed (`htree`) directory is read through its linear `.`/`..`
//! view; deeply indexed interior directory nodes are not traversed. A
//! [`NodeId`] is the on-disk inode number, so there is no in-memory
//! inode table.
//!
//! Writing (`create`/`write_at`/`truncate`/`remove`) allocates from the
//! block and inode bitmaps, maintains the group-descriptor and
//! superblock free counts, and creates new objects with the classic
//! block map. The write path maintains every on-disk checksum a volume
//! carries — the first-party crc32c `metadata_csum` feature
//! (superblock, group descriptors, block/inode bitmaps, inodes,
//! directory-leaf and extent-block tails) and the legacy crc16
//! `gdt_csum`/`uninit_bg` group-descriptor checksum — and the wide
//! (`64bit`) group-descriptor high halves, so those volumes are now
//! mutated in place. Mutation still fails closed
//! ([`DriverError::Unsupported`]) on feature sets it cannot maintain
//! (e.g. `bigalloc`, `meta_bg`, `inline_data`, an explicit
//! `checksum_seed`) and refuses to free a mapping that is neither the
//! classic map nor an extent tree of depth ≤ 1, rather than orphan
//! blocks (fail closed).
//!
//! [`Ext4::format`] lays a fresh, empty volume onto a blank device (no
//! `mkfs` shell-out) using a conservative
//! checksum-free `filetype`+`extent` feature set the reader accepts, and
//! hands it straight to [`Ext4::open`].
//!
//! No `unwrap`/`expect`/`panic!` and no `unsafe`.
//!
//! # Capabilities
//!
//! Loading requires
//! [`CapabilityId::DRV_LOAD`](tairix_abi::CapabilityId::DRV_LOAD). The
//! driver runs in user space; it does not request `CAP_DRV_KERNEL`.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use tairix_abi::driver::block::Block;
use tairix_abi::driver::filesystem::{
    DirEntry, FilesystemAttrsProvider, FilesystemRead, FilesystemSecurity, FilesystemStats,
    FilesystemWrite, NodeId, NodeInfo, NodeKind, NodeSecurity, NodeTimes, SecurityAcl,
    SecuritySubject, VolumeStats,
};
use tairix_abi::time::Time64;
use tairix_abi::{CapabilityId, DriverError, DriverHandle, DriverHost};

/// Per-driver `DriverHandle` marker returned by [`register`].
const REGISTER_HANDLE_MARKER: u64 = 0x4558_5434_0000_0001; // "EXT4" + index

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

/// Largest device logical-block size the driver stages through its
/// on-stack scratch buffer. No Tier-1 block device exceeds 4096 bytes
/// and ext4 block sizes never exceed it on these targets.
const MAX_BLOCK_SIZE: u32 = 4096;

/// The ext2/3/4 superblock begins at this fixed byte offset, regardless
/// of block size.
const SUPERBLOCK_OFFSET: u64 = 1024;

/// Encoded length of the fixed superblock fields the driver reads.
const SUPERBLOCK_LEN: usize = 1024;

/// On-disk superblock magic (`s_magic`), little-endian `0xEF53`. The one
/// definition lives in `lib/fsprobe`, which the volume manager's signature
/// probe shares, so the probe and this driver can never disagree.
const EXT_MAGIC: u16 = tairix_fsprobe::EXT4_SUPERBLOCK_MAGIC;

/// `s_feature_incompat`: directory entries carry a file-type byte.
const INCOMPAT_FILETYPE: u32 = 0x0002;
/// `s_feature_incompat`: 64-bit block numbers; group descriptors are
/// `s_desc_size` bytes wide rather than the legacy 32.
const INCOMPAT_64BIT: u32 = 0x0080;

/// `s_feature_ro_compat`: lazy block-group initialisation, with a
/// per-group descriptor checksum (`bg_checksum`). Mutating such a
/// volume safely requires recomputing that checksum.
const RO_COMPAT_GDT_CSUM: u32 = 0x0010;
/// `s_feature_ro_compat`: every metadata block carries a crc32c
/// checksum. Mutating such a volume safely requires recomputing them.
const RO_COMPAT_METADATA_CSUM: u32 = 0x0400;

/// `s_feature_incompat` bits the write path can maintain. Mutation is
/// refused (fail closed) on any volume carrying an incompat feature
/// outside this set, because it would change the on-disk layout the
/// write path assumes (`bigalloc` clustering, `meta_bg` descriptor
/// placement, `inline_data` small files, an explicit `checksum_seed`,
/// …): `filetype` (0x2), `extent` (0x40), `64bit` (0x80), `flex_bg`
/// (0x200).
const SAFE_INCOMPAT: u32 = INCOMPAT_FILETYPE | 0x0040 | INCOMPAT_64BIT | 0x0200;

/// `s_feature_ro_compat` bits the write path can maintain: `sparse_super`
/// (0x1), `large_file` (0x2), `huge_file` (0x8), `gdt_csum` (0x10),
/// `dir_nlink` (0x20), `extra_isize` (0x40), `metadata_csum` (0x400).
/// `bigalloc` (0x200), `quota` (0x100), and `project` (0x2000) are
/// outside the set, so mutation of such a volume fails closed.
const SAFE_RO_COMPAT: u32 =
    0x0001 | 0x0002 | 0x0008 | RO_COMPAT_GDT_CSUM | 0x0020 | 0x0040 | RO_COMPAT_METADATA_CSUM;

/// Byte offset of `s_checksum` within the superblock; the crc32c is
/// computed over every byte before it.
const SB_CHECKSUM_OFFSET: usize = 0x3FC;
/// Byte offset of `s_uuid` within the superblock (16 bytes).
const SB_UUID_OFFSET: usize = 0x68;

/// Group-descriptor field offsets shared by the read and write paths.
/// `bg_block_bitmap_csum_lo`.
const GD_BLOCK_BITMAP_CSUM_LO: usize = 0x18;
/// `bg_inode_bitmap_csum_lo`.
const GD_INODE_BITMAP_CSUM_LO: usize = 0x1A;
/// `bg_checksum`.
const GD_CHECKSUM: usize = 0x1E;
/// `bg_flags` (`INODE_UNINIT` / `BLOCK_UNINIT` / `ITABLE_ZEROED`).
const GD_FLAGS: usize = 0x12;
/// `bg_itable_unused_lo`.
const GD_ITABLE_UNUSED_LO: usize = 0x1C;
/// `bg_block_bitmap_csum_hi` (present only with a 64-byte descriptor).
const GD_BLOCK_BITMAP_CSUM_HI: usize = 0x38;
/// `bg_inode_bitmap_csum_hi`.
const GD_INODE_BITMAP_CSUM_HI: usize = 0x3A;
/// `bg_itable_unused_hi`.
const GD_ITABLE_UNUSED_HI: usize = 0x3C;

/// `bg_flags`: the group's inode bitmap is not initialised on disk.
const BG_INODE_UNINIT: u16 = 0x0001;
/// `bg_flags`: the group's block bitmap is not initialised on disk.
const BG_BLOCK_UNINIT: u16 = 0x0002;

/// `l_i_checksum_lo` byte offset within an inode (in the Linux `osd2`).
const INODE_CHECKSUM_LO: usize = 0x7C;
/// `i_checksum_hi` byte offset, present when `i_extra_isize` covers it.
const INODE_CHECKSUM_HI: usize = 0x82;
/// `i_generation` byte offset within an inode.
const INODE_GENERATION: usize = 0x64;
/// `i_dtime` (deletion time) byte offset within an inode.
const INODE_DTIME: usize = 0x14;
/// `i_links_count` byte offset within an inode.
const INODE_LINKS: usize = 0x1A;
/// `i_blocks_lo` byte offset within an inode.
const INODE_BLOCKS_LO: usize = 0x1C;
/// Sentinel `i_dtime` stamped on a removed inode. The driver has no
/// clock source, but a non-zero `i_dtime` is what marks an inode deleted.
/// It must exceed `s_inodes_count` so a checker does not mistake it for
/// an orphan-list next-inode pointer (ext4 reuses `i_dtime` for that
/// chain); a fixed plausible Unix timestamp satisfies that for any real
/// volume. The exact instant is not load-bearing.
const DELETED_DTIME: u32 = 1_700_000_000;

/// Byte length of an `ext4_dir_entry_tail` / `ext4_extent_tail`: the
/// directory-leaf tail is a 12-byte fake entry whose last 4 bytes hold
/// the crc32c; the extent tail is just the 4-byte crc32c.
const DIR_TAIL_LEN: usize = 12;
/// Byte length of an extent-block checksum tail (`ext4_extent_tail`).
const EXTENT_TAIL_LEN: usize = 4;
/// `det_reserved_ft` marker (`0xDE`) identifying a directory tail entry.
const DIR_TAIL_FT: u8 = 0xDE;

/// `i_extra_isize` value stamped into inodes this driver creates on an
/// enlarged-inode volume, matching the mke2fs default so the inode
/// checksum's high half is covered.
const NEW_EXTRA_ISIZE: u16 = 32;

/// `i_flags`: the inode is mapped by an extent tree, not block pointers.
const INODE_FLAG_EXTENTS: u32 = 0x0008_0000;
/// `EXT4_HUGE_FILE_FL`: this inode's `i_blocks` counts filesystem blocks
/// rather than 512-byte sectors (valid only under the `huge_file`
/// read-only-compat feature).
const INODE_FLAG_HUGE_FILE: u32 = 0x0004_0000;
/// osd2 `l_i_blocks_high` byte offset within an inode.
const INODE_BLOCKS_HI: usize = 0x74;
/// osd2 `l_i_file_acl_high` byte offset within an inode.
const INODE_FILE_ACL_HI: usize = 0x76;

/// Extent-tree node header magic (`eh_magic`), little-endian `0xF30A`.
const EXTENT_MAGIC: u16 = 0xF30A;

/// Number of extent (or index) entries the inline `i_block` root holds
/// after its 12-byte header: `(60 - 12) / 12 = 4`.
const INLINE_EXTENT_MAX: usize = (I_BLOCK_LEN - 12) / 12;

/// `i_mode` type mask and the two node kinds the read surface models.
const S_IFMT: u16 = 0xF000;
/// `i_mode` value for a directory.
const S_IFDIR: u16 = 0x4000;
/// `i_mode` value for a regular file.
const S_IFREG: u16 = 0x8000;

/// Directory-entry `file_type` value for a regular file.
const FT_REG: u8 = 1;
/// Directory-entry `file_type` value for a directory.
const FT_DIR: u8 = 2;

/// The root directory is always inode 2.
const ROOT_INODE: u32 = 2;

/// On-disk inode size in `i_block`: 15 u32 pointers / 60 bytes.
const I_BLOCK_LEN: usize = 60;
/// Byte offset of the `i_block` array within an inode.
const I_BLOCK_OFFSET: usize = 40;

/// Maximum extent-tree depth the driver will descend (a sane on-disk
/// tree never approaches this; the bound prevents a malformed image from
/// looping).
const MAX_EXTENT_DEPTH: u16 = 5;

/// Largest on-disk inode record the driver stages on the stack while
/// reading the inline extended-attribute region. ext4 inode sizes are a
/// power of two no larger than the block size, which itself never
/// exceeds [`MAX_BLOCK_SIZE`].
const MAX_INODE_SIZE: usize = MAX_BLOCK_SIZE as usize;

/// Extended-attribute header magic (`h_magic` for the external block and
/// the inode-body header alike), little-endian `0xEA02_0000`.
const XATTR_MAGIC: u32 = 0xEA02_0000;

/// Byte length of an external xattr block's header, ahead of its first
/// entry (`struct ext4_xattr_header`).
const XATTR_BLOCK_HEADER_LEN: usize = 32;

/// Byte length of the inode-body xattr header (`struct
/// ext4_xattr_ibody_header`): just the 4-byte magic.
const XATTR_IBODY_HEADER_LEN: usize = 4;

/// Fixed length of one xattr entry header, ahead of its (4-byte aligned)
/// name (`struct ext4_xattr_entry`).
const XATTR_ENTRY_HEADER_LEN: usize = 16;

/// `e_name_index` for the `system.posix_acl_access` attribute: the whole
/// name is encoded by the index, so its `e_name_len` is zero.
const XATTR_INDEX_POSIX_ACL_ACCESS: u8 = 2;

/// `i_extra_isize` lives here, immediately after the 128-byte classic
/// inode; the inline xattr region begins `i_extra_isize` bytes further on.
const I_EXTRA_ISIZE_OFFSET: usize = 128;

/// `a_version` of a `POSIX_ACL_XATTR` value, little-endian `2`.
const POSIX_ACL_VERSION: u32 = 2;
/// `e_tag` for a named-user ACL entry; `e_id` is the uid granted.
const ACL_TAG_USER: u16 = 0x02;
/// `e_tag` for a named-group ACL entry; `e_id` is the gid granted.
const ACL_TAG_GROUP: u16 = 0x08;
/// Low three bits of an ACL `e_perm`: the POSIX `rwx` triad.
const ACL_PERM_MASK: u16 = 0x07;
/// Byte length of one `posix_acl_xattr_entry` (`e_tag`, `e_perm`, `e_id`).
const POSIX_ACL_ENTRY_LEN: usize = 8;

/// Read a little-endian `u16` at `off`, or `0` if out of bounds.
fn le16(buf: &[u8], off: usize) -> u16 {
    match buf.get(off..off + 2) {
        Some(b) => u16::from_le_bytes([b[0], b[1]]),
        None => 0,
    }
}

/// Read a little-endian `u32` at `off`, or `0` if out of bounds.
fn le32(buf: &[u8], off: usize) -> u32 {
    match buf.get(off..off + 4) {
        Some(b) => u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        None => 0,
    }
}

/// Decode an on-disk inode timestamp into a [`Time64`].
///
/// `secs` is the classic 32-bit seconds field, signed when no extra field
/// extends it. `extra`, when the enlarged inode record carries it, packs
/// two epoch-extension bits (bits 0..2, prepended above bit 31 of the
/// seconds — the ext4 disk-layout extended-timestamp encoding) and a
/// 30-bit nanosecond count (bits 2..32). A nanosecond count at or above
/// one second is on-disk corruption and fails closed rather than being
/// clamped or wrapped.
fn decode_inode_time(secs: u32, extra: Option<u32>) -> Result<Time64, DriverError> {
    // The classic field is signed on disk; reinterpret the bits, never a
    // value-changing conversion.
    let signed_secs = i32::from_le_bytes(secs.to_le_bytes());
    let Some(extra) = extra else {
        return Ok(Time64::from_secs(i64::from(signed_secs)));
    };
    let epoch = u64::from(extra & 0x3);
    let seconds = if epoch == 0 {
        i64::from(signed_secs)
    } else {
        // The epoch bits extend the seconds above bit 31; the low 32 bits
        // are then unsigned. At most 2 + 32 significant bits, so the wide
        // conversion is total; a failure can only mean a broken invariant
        // and is treated as corruption.
        i64::try_from((epoch << 32) | u64::from(secs)).map_err(|_| DriverError::DeviceFault)?
    };
    let nanos = extra >> 2;
    Time64::new(seconds, nanos).map_err(|_| DriverError::DeviceFault)
}

/// Write a little-endian `u16` at `off`; a no-op if out of bounds.
fn put_le16(buf: &mut [u8], off: usize, value: u16) {
    if let Some(b) = buf.get_mut(off..off + 2) {
        b.copy_from_slice(&value.to_le_bytes());
    }
}

/// Write a little-endian `u32` at `off`; a no-op if out of bounds.
fn put_le32(buf: &mut [u8], off: usize, value: u32) {
    if let Some(b) = buf.get_mut(off..off + 4) {
        b.copy_from_slice(&value.to_le_bytes());
    }
}

/// First-party CRC-32C (Castagnoli) over `data`, continuing from `crc`.
///
/// Reflected, with the reversed polynomial `0x82F6_3B78` and no final
/// inversion — the convention the Linux ext4 driver uses for the
/// `metadata_csum` feature (the seed already carries the `~0`
/// initialisation). The charter reserves “never roll your own” for
/// *cryptographic* primitives; a storage checksum is first-party here.
fn crc32c(mut crc: u32, data: &[u8]) -> u32 {
    const POLY: u32 = 0x82F6_3B78;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ POLY
            } else {
                crc >> 1
            };
        }
    }
    crc
}

/// First-party CRC-16 (reversed polynomial `0xA001`) over `data`,
/// continuing from `crc` — the checksum the legacy `gdt_csum`/
/// `uninit_bg` feature stores in each group descriptor.
fn crc16(mut crc: u16, data: &[u8]) -> u16 {
    for &byte in data {
        crc ^= u16::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xA001
            } else {
                crc >> 1
            };
        }
    }
    crc
}

/// Narrow a `u64` to `u32`, mapping overflow to [`DriverError::DeviceFault`].
fn u32_of(value: u64) -> Result<u32, DriverError> {
    u32::try_from(value).map_err(|_| DriverError::DeviceFault)
}

/// The low 32 bits of `value`, little-endian — the on-disk encoding a
/// group/inode number takes in a checksum's seed chain.
fn u32_le_truncate(value: u64) -> [u8; 4] {
    ((value & 0xFFFF_FFFF) as u32).to_le_bytes()
}

/// Narrow a `usize` to `u16`, mapping overflow to [`DriverError::DeviceFault`].
fn u16_of(value: usize) -> Result<u16, DriverError> {
    u16::try_from(value).map_err(|_| DriverError::DeviceFault)
}

/// Narrow a `u64` to `usize`, mapping overflow to [`DriverError::DeviceFault`].
fn usize_of(value: u64) -> Result<usize, DriverError> {
    usize::try_from(value).map_err(|_| DriverError::DeviceFault)
}

/// Validated geometry of an ext4 volume, in bytes/blocks.
//
// The four booleans are independent on-disk feature flags (`filetype`,
// the write-safe gate, `metadata_csum`, `gdt_csum`); they describe
// distinct toggles rather than a state machine, so a flat record reads
// more clearly than an enum here.
#[allow(clippy::struct_excessive_bools)]
struct Layout {
    /// Filesystem block size in bytes (`1024 << s_log_block_size`).
    block_size: u32,
    /// Total blocks in the volume.
    blocks_count: u64,
    /// Inodes per block group.
    inodes_per_group: u32,
    /// On-disk inode record size in bytes.
    inode_size: u32,
    /// Group-descriptor record size in bytes (32 or `s_desc_size`).
    desc_size: u32,
    /// Whether directory entries carry a `file_type` byte.
    filetype: bool,
    /// Byte offset of the group-descriptor table.
    gdt_offset: u64,
    /// Number of block groups in the volume.
    group_count: u64,
    /// Blocks per block group.
    blocks_per_group: u32,
    /// First data block (1 when `block_size == 1024`, else 0); also the
    /// block number that bit 0 of group 0's block bitmap represents.
    first_data_block: u64,
    /// Whether the volume's feature set is one this driver can safely
    /// **mutate**. Writes refuse (`Unsupported`) when it is not — a
    /// feature outside [`SAFE_INCOMPAT`] / [`SAFE_RO_COMPAT`] would need
    /// on-disk maintenance the write path does not perform (fail closed).
    write_safe: bool,
    /// Whether the volume carries the `metadata_csum` feature: every
    /// metadata block (superblock, group descriptors, bitmaps, inodes,
    /// directory-leaf and extent-block tails) carries a crc32c the write
    /// path must recompute.
    metadata_csum: bool,
    /// Whether the volume carries the legacy `gdt_csum`/`uninit_bg`
    /// feature: each group descriptor carries a crc16 (and a
    /// `bg_itable_unused` count) the write path must maintain.
    gdt_csum: bool,
    /// crc32c checksum seed for `metadata_csum`: `crc32c(~0, s_uuid)`.
    csum_seed: u32,
    /// The volume UUID, seed for the legacy `gdt_csum` crc16.
    uuid: [u8; 16],
}

impl Layout {
    /// Whether group descriptors are wide enough to carry the high
    /// halves of the 64-bit bitmap-checksum / `itable_unused` fields.
    fn wide_desc(&self) -> bool {
        self.desc_size as usize > GD_ITABLE_UNUSED_HI
    }
}

/// A decoded inode: only the structural fields the read and security
/// surfaces need.
struct Inode {
    /// `i_mode`, including the type bits.
    mode: u16,
    /// The node's four timestamps, decoded from `i_atime`/`i_ctime`/
    /// `i_mtime`/`i_crtime` (and their `_extra` nanosecond+epoch fields
    /// where the enlarged inode record carries them). ext4 stores a real
    /// access time, so `times.accessed` is meaningful here; `times.created`
    /// is `i_crtime` when the record carries it, else the epoch.
    times: NodeTimes,
    /// Owning user id (`i_uid` low half combined with the osd2 high half).
    uid: u32,
    /// Owning group id (`i_gid` low half combined with the osd2 high half).
    gid: u32,
    /// File length in bytes (low + high halves combined).
    size: u64,
    /// `i_flags`.
    flags: u32,
    /// `i_blocks` (low half combined with the osd2 high half): allocated
    /// storage in 512-byte sectors, or in filesystem blocks when the
    /// huge-file inode flag is set.
    blocks: u64,
    /// External extended-attribute block (`i_file_acl` low half combined
    /// with the osd2 high half); `0` when the inode has none.
    file_acl: u64,
    /// Raw `i_block` array (extent root or block-pointer map).
    block: [u8; I_BLOCK_LEN],
}

impl Inode {
    /// The node kind, or `None` for an unsupported special file.
    fn kind(&self) -> Option<NodeKind> {
        match self.mode & S_IFMT {
            S_IFDIR => Some(NodeKind::Directory),
            S_IFREG => Some(NodeKind::RegularFile),
            _ => None,
        }
    }

    /// Whether the inode is extent-mapped rather than block-mapped.
    fn uses_extents(&self) -> bool {
        self.flags & INODE_FLAG_EXTENTS != 0
    }

    /// The inode's base security record (owner, group, mode bits),
    /// before any extended-attribute ACL is folded in.
    ///
    /// ext4 stores the POSIX mode bits (`i_mode` low 12 bits, the type
    /// bits stripped), owner, and group per inode, and has no inline
    /// capability gate. Named-user / named-group POSIX ACL grants live in
    /// extended attributes and are decoded into this record separately by
    /// [`Ext4::decode_inode_acl`], which needs the backing device.
    fn security(&self) -> NodeSecurity {
        NodeSecurity::new(u32::from(self.mode) & 0x0FFF, self.uid, self.gid)
    }
}

/// Read-only ext4 driver over a [`Block`] device.
pub struct Ext4<B: Block> {
    block: B,
    block_size: u32,
    block_count: u64,
    layout: Layout,
}

/// Read `buf.len()` bytes starting at device byte `offset`, staging
/// through one logical block at a time.
fn device_read<B: Block>(
    block: &mut B,
    block_size: u32,
    block_count: u64,
    offset: u64,
    buf: &mut [u8],
) -> Result<(), DriverError> {
    let bs = u64::from(block_size);
    let bs_usize = block_size as usize;
    let mut scratch = [0u8; MAX_BLOCK_SIZE as usize];
    let mut done: usize = 0;
    while done < buf.len() {
        let cursor = offset + done as u64;
        let lba = cursor / bs;
        let within = usize::try_from(cursor % bs).map_err(|_| DriverError::DeviceFault)?;
        if lba >= block_count {
            return Err(DriverError::DeviceFault);
        }
        block.read_blocks(lba, &mut scratch[..bs_usize])?;
        let take = core::cmp::min(bs_usize - within, buf.len() - done);
        buf[done..done + take].copy_from_slice(&scratch[within..within + take]);
        done += take;
    }
    Ok(())
}

/// Write `buf.len()` bytes starting at device byte `offset`, staging
/// through one logical block at a time (a read-modify-write when the
/// span does not cover whole device blocks).
fn device_write<B: Block>(
    block: &mut B,
    block_size: u32,
    block_count: u64,
    offset: u64,
    buf: &[u8],
) -> Result<(), DriverError> {
    let bs = u64::from(block_size);
    let bs_usize = block_size as usize;
    let mut scratch = [0u8; MAX_BLOCK_SIZE as usize];
    let mut done: usize = 0;
    while done < buf.len() {
        let cursor = offset + done as u64;
        let lba = cursor / bs;
        let within = usize::try_from(cursor % bs).map_err(|_| DriverError::DeviceFault)?;
        if lba >= block_count {
            return Err(DriverError::DeviceFault);
        }
        let take = core::cmp::min(bs_usize - within, buf.len() - done);
        if within != 0 || take != bs_usize {
            block.read_blocks(lba, &mut scratch[..bs_usize])?;
        }
        scratch[within..within + take].copy_from_slice(&buf[done..done + take]);
        block.write_blocks(lba, &scratch[..bs_usize])?;
        done += take;
    }
    Ok(())
}

impl<B: Block> Ext4<B> {
    /// Validate the ext4 superblock on `block` and bring the volume
    /// online.
    ///
    /// # Errors
    ///
    /// * [`DriverError::DeviceFault`] if the device geometry is
    ///   degenerate or a block read fails.
    /// * [`DriverError::BadMagic`] if the superblock magic is wrong or
    ///   the geometry is structurally invalid.
    /// * [`DriverError::Unsupported`] if the volume requires a feature
    ///   the driver does not implement.
    ///
    /// # Capabilities
    ///
    /// Caller must already hold the driver's [`DriverHandle`].
    // A single linear superblock validation: every line is one geometry
    // field decoded and bounds-checked in sequence, which reads more
    // clearly as one function than split across artificial helpers.
    #[allow(clippy::too_many_lines)]
    pub fn open(mut block: B) -> Result<Self, DriverError> {
        let geometry = block.geometry()?;
        let dev_block_size = geometry.block_size;
        if dev_block_size == 0
            || dev_block_size > MAX_BLOCK_SIZE
            || !dev_block_size.is_power_of_two()
        {
            return Err(DriverError::DeviceFault);
        }
        let dev_block_count = geometry.block_count;
        let total_bytes = u64::from(dev_block_size)
            .checked_mul(dev_block_count)
            .ok_or(DriverError::DeviceFault)?;
        if total_bytes < SUPERBLOCK_OFFSET + SUPERBLOCK_LEN as u64 {
            return Err(DriverError::BadMagic);
        }

        let mut sb = [0u8; SUPERBLOCK_LEN];
        device_read(
            &mut block,
            dev_block_size,
            dev_block_count,
            SUPERBLOCK_OFFSET,
            &mut sb,
        )?;
        if le16(&sb, 0x38) != EXT_MAGIC {
            return Err(DriverError::BadMagic);
        }

        let log_block_size = le32(&sb, 0x18);
        if log_block_size > 2 {
            return Err(DriverError::Unsupported);
        }
        let block_size = 1024u32 << log_block_size;
        if block_size > MAX_BLOCK_SIZE {
            return Err(DriverError::Unsupported);
        }

        let inodes_per_group = le32(&sb, 0x28);
        let blocks_per_group = le32(&sb, 0x20);
        if inodes_per_group == 0 || blocks_per_group == 0 {
            return Err(DriverError::BadMagic);
        }

        let rev_level = le32(&sb, 0x4C);
        let inode_size = if rev_level == 0 {
            128
        } else {
            u32::from(le16(&sb, 0x58))
        };
        if inode_size < 128 || !inode_size.is_power_of_two() || inode_size > block_size {
            return Err(DriverError::BadMagic);
        }

        let feature_incompat = le32(&sb, 0x60);
        let is_64bit = feature_incompat & INCOMPAT_64BIT != 0;
        let filetype = feature_incompat & INCOMPAT_FILETYPE != 0;
        let desc_size = if is_64bit {
            let raw = u32::from(le16(&sb, 0xFE));
            if raw < 32 {
                32
            } else {
                raw
            }
        } else {
            32
        };
        if desc_size > block_size {
            return Err(DriverError::BadMagic);
        }

        let blocks_count = compute_blocks_count(&sb, is_64bit, total_bytes, block_size)?;
        let group_count = blocks_count
            .checked_add(u64::from(blocks_per_group) - 1)
            .ok_or(DriverError::BadMagic)?
            / u64::from(blocks_per_group);
        if group_count == 0 {
            return Err(DriverError::BadMagic);
        }

        // The group-descriptor table starts in the block following the
        // one that holds the superblock: block 1 when the block size
        // exceeds 1024 (the superblock shares block 0), block 2 when the
        // block size is exactly 1024 (the superblock fills block 1).
        let gdt_block: u64 = if block_size == 1024 { 2 } else { 1 };
        let gdt_offset = gdt_block * u64::from(block_size);

        let feature_ro_compat = le32(&sb, 0x64);
        let metadata_csum = feature_ro_compat & RO_COMPAT_METADATA_CSUM != 0;
        let gdt_csum = !metadata_csum && feature_ro_compat & RO_COMPAT_GDT_CSUM != 0;
        // Fail closed: only mutate volumes whose entire feature
        // set the write path can maintain. The `checksum_seed` incompat
        // (0x2000) would invalidate the `crc32c(~0, uuid)` seed, so it is
        // deliberately outside `SAFE_INCOMPAT`.
        let write_safe =
            feature_incompat & !SAFE_INCOMPAT == 0 && feature_ro_compat & !SAFE_RO_COMPAT == 0;
        let mut uuid = [0u8; 16];
        uuid.copy_from_slice(&sb[SB_UUID_OFFSET..SB_UUID_OFFSET + 16]);
        let csum_seed = crc32c(0xFFFF_FFFF, &uuid);
        let first_data_block = u64::from(le32(&sb, 0x14));

        Ok(Self {
            block,
            block_size: dev_block_size,
            block_count: dev_block_count,
            layout: Layout {
                block_size,
                blocks_count,
                inodes_per_group,
                inode_size,
                desc_size,
                filetype,
                gdt_offset,
                group_count,
                blocks_per_group,
                first_data_block,
                write_safe,
                metadata_csum,
                gdt_csum,
                csum_seed,
                uuid,
            },
        })
    }

    /// The volume's stable identity: the superblock `s_uuid`, minted by
    /// the formatter and stable across re-inserts.
    #[must_use]
    pub fn volume_uuid(&self) -> [u8; 16] {
        self.layout.uuid
    }

    /// Lay down a fresh, empty ext4 volume on `block` and return it
    /// mounted.
    ///
    /// The formatter writes a deliberately conservative on-disk feature
    /// set the read/write path fully supports: `filetype` + `extent`
    /// (`s_feature_incompat`), no read-only-compat features, 128-byte
    /// inodes and 32-byte group descriptors, and **no** checksum
    /// (`metadata_csum`/`gdt_csum`) or `64bit` feature. Every block group
    /// is fully materialised (no lazy/`UNINIT` groups), so the volume can
    /// be filled to exhaustion. The block size is 4096 bytes for volumes
    /// of at least 128 MiB and 1024 bytes otherwise; `blocks_per_group`
    /// is the bitmap-maximal `8 * block_size`.
    ///
    /// `inode_count` is the *minimum* total inode budget; the actual
    /// count is rounded up to a whole number of inodes per group (at
    /// least 16 per group). The reserved inodes 1..=10 and an
    /// extent-mapped empty root directory (inode 2) are laid down; the
    /// remainder are free. `uuid` is the volume's stable identity
    /// (`s_uuid`); the caller mints it from its own entropy source — this
    /// `no_std` driver has none — and the reserved all-zero value is
    /// refused, so a formatted volume always has a publishable identity.
    ///
    /// # Errors
    ///
    /// * [`DriverError::DeviceFault`] if the device geometry is
    ///   degenerate or a block write fails.
    /// * [`DriverError::OutOfRange`] if the device is too small to host a
    ///   single block group's metadata plus a non-empty data region,
    ///   `inode_count` is zero, or `uuid` is the refused all-zero value.
    ///
    /// # Capabilities
    ///
    /// Reached only through the driver's [`DriverHandle`].
    pub fn format(mut block: B, inode_count: u32, uuid: [u8; 16]) -> Result<Self, DriverError> {
        format::write_volume(&mut block, inode_count, uuid)?;
        Self::open(block)
    }

    /// Consume the driver, returning the underlying block device.
    #[must_use]
    pub fn into_block(self) -> B {
        self.block
    }
}

/// Combine the 32-bit low and (with the `64bit` feature) high halves of
/// `s_blocks_count`, validating it against the device capacity.
fn compute_blocks_count(
    sb: &[u8],
    is_64bit: bool,
    total_bytes: u64,
    block_size: u32,
) -> Result<u64, DriverError> {
    let lo = u64::from(le32(sb, 0x04));
    let hi = if is_64bit {
        u64::from(le32(sb, 0x150))
    } else {
        0
    };
    let blocks_count = (hi << 32) | lo;
    if blocks_count == 0 {
        return Err(DriverError::BadMagic);
    }
    let needed = blocks_count
        .checked_mul(u64::from(block_size))
        .ok_or(DriverError::BadMagic)?;
    if needed > total_bytes {
        return Err(DriverError::BadMagic);
    }
    Ok(blocks_count)
}

impl<B: Block> Ext4<B> {
    /// Number of 4-byte block pointers in one filesystem block.
    fn pointers_per_block(&self) -> u64 {
        u64::from(self.layout.block_size) / 4
    }

    /// Read filesystem block `block_num` into `buf[..block_size]`.
    fn read_fs_block(&mut self, block_num: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        if block_num == 0 || block_num >= self.layout.blocks_count {
            return Err(DriverError::DeviceFault);
        }
        let bs = self.layout.block_size as usize;
        let offset = block_num
            .checked_mul(u64::from(self.layout.block_size))
            .ok_or(DriverError::DeviceFault)?;
        device_read(
            &mut self.block,
            self.block_size,
            self.block_count,
            offset,
            &mut buf[..bs],
        )
    }

    /// Compute the device byte offset of inode number `ino`'s on-disk
    /// record.
    fn locate_inode(&mut self, ino: u32) -> Result<u64, DriverError> {
        if ino == 0 {
            return Err(DriverError::NotFound);
        }
        let group = u64::from(ino - 1) / u64::from(self.layout.inodes_per_group);
        let index = u64::from(ino - 1) % u64::from(self.layout.inodes_per_group);
        if group >= self.layout.group_count {
            return Err(DriverError::NotFound);
        }

        let desc_offset = self
            .layout
            .gdt_offset
            .checked_add(group * u64::from(self.layout.desc_size))
            .ok_or(DriverError::DeviceFault)?;
        let mut desc = [0u8; 64];
        let desc_len = self.layout.desc_size as usize;
        device_read(
            &mut self.block,
            self.block_size,
            self.block_count,
            desc_offset,
            &mut desc[..desc_len],
        )?;
        let table_lo = u64::from(le32(&desc, 0x08));
        let table_hi = if desc_len >= 0x2C {
            u64::from(le32(&desc, 0x28))
        } else {
            0
        };
        let inode_table_block = (table_hi << 32) | table_lo;
        if inode_table_block == 0 || inode_table_block >= self.layout.blocks_count {
            return Err(DriverError::DeviceFault);
        }

        inode_table_block
            .checked_mul(u64::from(self.layout.block_size))
            .and_then(|base| base.checked_add(index * u64::from(self.layout.inode_size)))
            .ok_or(DriverError::DeviceFault)
    }

    /// Read and decode inode number `ino`.
    fn read_inode(&mut self, ino: u32) -> Result<Inode, DriverError> {
        let inode_offset = self.locate_inode(ino)?;
        // 160 bytes covers the classic 128-byte record plus the enlarged
        // record's `i_extra_isize` (0x80) and every extended-timestamp field
        // through `i_crtime_extra` (0x94..0x98); the read never exceeds the
        // volume's own inode record size.
        let mut raw = [0u8; 160];
        let want = core::cmp::min(self.layout.inode_size as usize, raw.len());
        device_read(
            &mut self.block,
            self.block_size,
            self.block_count,
            inode_offset,
            &mut raw[..want],
        )?;
        let raw = &raw[..want];

        let mode = le16(raw, 0);
        let uid = u32::from(le16(raw, 0x02)) | (u32::from(le16(raw, 0x78)) << 16);
        let gid = u32::from(le16(raw, 0x18)) | (u32::from(le16(raw, 0x7A)) << 16);
        let size_lo = u64::from(le32(raw, 0x04));
        let size_hi = u64::from(le32(raw, 0x6C));
        let flags = le32(raw, 0x20);
        let blocks =
            u64::from(le32(raw, INODE_BLOCKS_LO)) | (u64::from(le16(raw, INODE_BLOCKS_HI)) << 32);
        let file_acl = u64::from(le32(raw, 0x68)) | (u64::from(le16(raw, INODE_FILE_ACL_HI)) << 32);
        let mut block = [0u8; I_BLOCK_LEN];
        block.copy_from_slice(&raw[I_BLOCK_OFFSET..I_BLOCK_OFFSET + I_BLOCK_LEN]);
        // The nanosecond+epoch `_extra` timestamp fields live in the
        // enlarged inode record beyond the classic 128 bytes, present only
        // when `i_extra_isize` (0x80) covers them: ctime_extra 0x84,
        // mtime_extra 0x88, atime_extra 0x8C, crtime 0x90, crtime_extra
        // 0x94 (ext4 disk layout).
        let raw_len = raw.len();
        let extra_isize = usize::from(le16(raw, 0x80));
        let extra_at = |off: usize| -> Option<u32> {
            if raw_len >= off + 4 && 128 + extra_isize >= off + 4 {
                Some(le32(raw, off))
            } else {
                None
            }
        };
        let accessed = decode_inode_time(le32(raw, 0x08), extra_at(0x8C))?;
        let changed = decode_inode_time(le32(raw, 0x0C), extra_at(0x84))?;
        let modified = decode_inode_time(le32(raw, 0x10), extra_at(0x88))?;
        // `i_crtime` (birth time) exists only in an enlarged record; absent,
        // the node reports the epoch rather than a fabricated instant.
        let created = if raw_len >= 0x94 && 128 + extra_isize >= 0x90 + 4 {
            decode_inode_time(le32(raw, 0x90), extra_at(0x94))?
        } else {
            Time64::UNIX_EPOCH
        };
        let times = NodeTimes {
            created,
            modified,
            accessed,
            changed,
        };
        Ok(Inode {
            mode,
            times,
            uid,
            gid,
            size: (size_hi << 32) | size_lo,
            flags,
            blocks,
            file_acl,
            block,
        })
    }

    /// Map logical block `logical` of `inode` to a physical filesystem
    /// block, or `None` for a sparse hole.
    fn map_block(&mut self, inode: &Inode, logical: u64) -> Result<Option<u64>, DriverError> {
        if inode.uses_extents() {
            self.map_block_extent(inode, logical)
        } else {
            self.map_block_classic(inode, logical)
        }
    }

    /// Walk the extent tree rooted in `inode.block`.
    fn map_block_extent(
        &mut self,
        inode: &Inode,
        logical: u64,
    ) -> Result<Option<u64>, DriverError> {
        let mut node = [0u8; MAX_BLOCK_SIZE as usize];
        node[..I_BLOCK_LEN].copy_from_slice(&inode.block);
        let mut node_len = I_BLOCK_LEN;
        let mut iterations: u16 = 0;
        loop {
            if iterations > MAX_EXTENT_DEPTH {
                return Err(DriverError::DeviceFault);
            }
            iterations += 1;
            if le16(&node, 0) != EXTENT_MAGIC {
                return Err(DriverError::DeviceFault);
            }
            let entries = usize::from(le16(&node, 2));
            let depth = le16(&node, 6);
            let max_entries = (node_len - 12) / 12;
            if entries > max_entries {
                return Err(DriverError::DeviceFault);
            }
            if depth == 0 {
                for i in 0..entries {
                    let off = 12 + i * 12;
                    let ee_block = u64::from(le32(&node, off));
                    let raw_len = le16(&node, off + 4);
                    let len = if raw_len > 32_768 {
                        u64::from(raw_len - 32_768)
                    } else {
                        u64::from(raw_len)
                    };
                    if len == 0 {
                        continue;
                    }
                    if logical >= ee_block && logical < ee_block + len {
                        let phys = (u64::from(le16(&node, off + 6)) << 32)
                            | u64::from(le32(&node, off + 8));
                        return Ok(Some(phys + (logical - ee_block)));
                    }
                }
                return Ok(None);
            }
            let mut chosen: Option<u64> = None;
            let mut best_block = 0u64;
            for i in 0..entries {
                let off = 12 + i * 12;
                let ei_block = u64::from(le32(&node, off));
                if ei_block <= logical && (chosen.is_none() || ei_block >= best_block) {
                    best_block = ei_block;
                    chosen = Some(
                        (u64::from(le16(&node, off + 8)) << 32) | u64::from(le32(&node, off + 4)),
                    );
                }
            }
            let Some(child) = chosen else {
                return Ok(None);
            };
            self.read_fs_block(child, &mut node)?;
            node_len = self.layout.block_size as usize;
        }
    }

    /// Resolve a logical block via the classic direct/indirect block map.
    fn map_block_classic(
        &mut self,
        inode: &Inode,
        logical: u64,
    ) -> Result<Option<u64>, DriverError> {
        let ppb = self.pointers_per_block();
        if logical < 12 {
            let off = usize::try_from(logical * 4).map_err(|_| DriverError::DeviceFault)?;
            return Ok(nonzero(le32(&inode.block, off)));
        }
        let mut rem = logical - 12;
        if rem < ppb {
            return self.map_indirect(le32(&inode.block, 48), rem, 1);
        }
        rem -= ppb;
        let double_span = ppb * ppb;
        if rem < double_span {
            return self.map_indirect(le32(&inode.block, 52), rem, 2);
        }
        rem -= double_span;
        let triple_span = double_span
            .checked_mul(ppb)
            .ok_or(DriverError::DeviceFault)?;
        if rem < triple_span {
            return self.map_indirect(le32(&inode.block, 56), rem, 3);
        }
        Ok(None)
    }

    /// Resolve `index` within an indirect block of the given `level`
    /// (1 = single, 2 = double, 3 = triple), rooted at `block_num`.
    fn map_indirect(
        &mut self,
        block_num: u32,
        index: u64,
        level: u32,
    ) -> Result<Option<u64>, DriverError> {
        if block_num == 0 {
            return Ok(None);
        }
        let ppb = self.pointers_per_block();
        let mut buf = [0u8; MAX_BLOCK_SIZE as usize];
        self.read_fs_block(u64::from(block_num), &mut buf)?;
        if level == 1 {
            let off = usize::try_from(index * 4).map_err(|_| DriverError::DeviceFault)?;
            return Ok(nonzero(le32(&buf, off)));
        }
        let mut span = 1u64;
        for _ in 1..level {
            span = span.checked_mul(ppb).ok_or(DriverError::DeviceFault)?;
        }
        let child_idx = index / span;
        let off = usize::try_from(child_idx * 4).map_err(|_| DriverError::DeviceFault)?;
        self.map_indirect(le32(&buf, off), index % span, level - 1)
    }
}

/// `Some(block)` for a non-zero pointer, `None` for the zero (hole)
/// pointer.
fn nonzero(ptr: u32) -> Option<u64> {
    if ptr == 0 {
        None
    } else {
        Some(u64::from(ptr))
    }
}

/// How a directory walk selects the entry it returns.
#[derive(Copy, Clone)]
enum DirQuery<'a> {
    /// The entry whose name matches these raw bytes exactly.
    ByName(&'a [u8]),
    /// The first real child (skipping `.` / `..` and unused slots) whose
    /// on-disk byte offset within the directory is at or past this
    /// cursor. `0` starts the listing; the offset after a returned entry
    /// ([`FoundEntry::next_cursor`]) resumes it in O(1). The walk starts
    /// at the containing block's first record and skips forward, so a
    /// cursor that does not name a record boundary — including an
    /// arbitrary value that was never returned — can only skip entries,
    /// never mis-parse mid-record.
    ByCursor(u64),
}

/// A directory entry located by [`Ext4::find_entry`].
struct FoundEntry {
    /// The child inode number.
    ino: u32,
    /// Number of name bytes written into the caller's output buffer.
    name_len: usize,
    /// Byte offset within the directory of the record *after* this one:
    /// the [`DirQuery::ByCursor`] value that resumes the listing there.
    next_cursor: u64,
}

/// Minimum on-disk directory-entry header length: the four fixed header
/// fields (inode number, record length, name length, and file type)
/// that precede the name bytes.
const DIRENT_HEADER: usize = 8;

impl<B: Block> Ext4<B> {
    /// Read up to `buf.len()` bytes of `inode`'s data starting at byte
    /// `offset`, honouring sparse holes (read back as zero).
    fn read_data(
        &mut self,
        inode: &Inode,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, DriverError> {
        if offset >= inode.size {
            return Ok(0);
        }
        let available = inode.size - offset;
        let want = core::cmp::min(buf.len() as u64, available);
        let want = usize::try_from(want).map_err(|_| DriverError::DeviceFault)?;
        let bs = u64::from(self.layout.block_size);
        let mut scratch = [0u8; MAX_BLOCK_SIZE as usize];
        let mut done = 0usize;
        while done < want {
            let cursor = offset + done as u64;
            let logical = cursor / bs;
            let within = usize::try_from(cursor % bs).map_err(|_| DriverError::DeviceFault)?;
            let take = core::cmp::min(self.layout.block_size as usize - within, want - done);
            match self.map_block(inode, logical)? {
                Some(phys) => {
                    self.read_fs_block(phys, &mut scratch)?;
                    buf[done..done + take].copy_from_slice(&scratch[within..within + take]);
                }
                None => {
                    for b in &mut buf[done..done + take] {
                        *b = 0;
                    }
                }
            }
            done += take;
        }
        Ok(done)
    }

    /// Walk the linear directory blocks of `dir`, returning the entry
    /// selected by `query`. The matched entry's name is written into
    /// `name_out` (only when an entry is returned).
    fn find_entry(
        &mut self,
        dir: &Inode,
        query: DirQuery<'_>,
        name_out: &mut [u8],
    ) -> Result<Option<FoundEntry>, DriverError> {
        let bs = self.layout.block_size as usize;
        let mut block_buf = [0u8; MAX_BLOCK_SIZE as usize];
        let total_blocks = dir.size.div_ceil(u64::from(self.layout.block_size));
        // A cursor resume seeks straight to its containing block; the
        // in-block scan below still starts at the block's first record so
        // parsing always begins on a record boundary.
        let start_block = match query {
            DirQuery::ByCursor(cursor) => cursor / u64::from(self.layout.block_size),
            DirQuery::ByName(_) => 0,
        };
        for logical in start_block..total_blocks {
            let Some(phys) = self.map_block(dir, logical)? else {
                continue;
            };
            self.read_fs_block(phys, &mut block_buf)?;
            let mut pos = 0usize;
            while pos + DIRENT_HEADER <= bs {
                let ino = le32(&block_buf, pos);
                let rec_len = usize::from(le16(&block_buf, pos + 4));
                if rec_len < DIRENT_HEADER || rec_len % 4 != 0 || pos + rec_len > bs {
                    return Err(DriverError::DeviceFault);
                }
                let name_len = if self.layout.filetype {
                    usize::from(block_buf[pos + 6])
                } else {
                    usize::from(le16(&block_buf, pos + 6))
                };
                if ino != 0 && name_len > 0 && DIRENT_HEADER + name_len <= rec_len {
                    let name = &block_buf[pos + DIRENT_HEADER..pos + DIRENT_HEADER + name_len];
                    if name != b"." && name != b".." {
                        let entry_offset = logical * u64::from(self.layout.block_size) + pos as u64;
                        match query {
                            DirQuery::ByName(target) => {
                                if name == target {
                                    return Ok(Some(FoundEntry {
                                        ino,
                                        name_len: 0,
                                        next_cursor: entry_offset + rec_len as u64,
                                    }));
                                }
                            }
                            DirQuery::ByCursor(cursor) => {
                                if entry_offset >= cursor {
                                    if name_len > name_out.len() {
                                        return Err(DriverError::BufferTooSmall);
                                    }
                                    name_out[..name_len].copy_from_slice(name);
                                    return Ok(Some(FoundEntry {
                                        ino,
                                        name_len,
                                        next_cursor: entry_offset + rec_len as u64,
                                    }));
                                }
                            }
                        }
                    }
                }
                pos += rec_len;
            }
        }
        Ok(None)
    }
}

impl<B: Block> Ext4<B> {
    /// Refuse mutation of a volume whose feature set this driver cannot
    /// maintain (fail closed).
    fn ensure_writable(&self) -> Result<(), DriverError> {
        if self.layout.write_safe {
            Ok(())
        } else {
            Err(DriverError::Unsupported)
        }
    }

    /// Write `buf[..block_size]` to filesystem block `block_num`.
    fn write_fs_block(&mut self, block_num: u64, buf: &[u8]) -> Result<(), DriverError> {
        if block_num == 0 || block_num >= self.layout.blocks_count {
            return Err(DriverError::DeviceFault);
        }
        let bs = self.layout.block_size as usize;
        let offset = block_num
            .checked_mul(u64::from(self.layout.block_size))
            .ok_or(DriverError::DeviceFault)?;
        device_write(
            &mut self.block,
            self.block_size,
            self.block_count,
            offset,
            &buf[..bs],
        )
    }

    /// Read a `u32` superblock field at byte offset `field`.
    fn sb_u32(&mut self, field: u64) -> Result<u32, DriverError> {
        let mut raw = [0u8; 4];
        device_read(
            &mut self.block,
            self.block_size,
            self.block_count,
            SUPERBLOCK_OFFSET + field,
            &mut raw,
        )?;
        Ok(u32::from_le_bytes(raw))
    }

    /// Write a `u32` superblock field at byte offset `field`, then (on a
    /// `metadata_csum` volume) recompute `s_checksum` over the whole
    /// superblock.
    fn set_sb_u32(&mut self, field: u64, value: u32) -> Result<(), DriverError> {
        device_write(
            &mut self.block,
            self.block_size,
            self.block_count,
            SUPERBLOCK_OFFSET + field,
            &value.to_le_bytes(),
        )?;
        self.update_sb_checksum()
    }

    /// Recompute and persist `s_checksum` (`crc32c(~0, sb[..0x3FC])`) when
    /// the volume carries `metadata_csum`; a no-op otherwise.
    fn update_sb_checksum(&mut self) -> Result<(), DriverError> {
        if !self.layout.metadata_csum {
            return Ok(());
        }
        let mut sb = [0u8; SUPERBLOCK_LEN];
        device_read(
            &mut self.block,
            self.block_size,
            self.block_count,
            SUPERBLOCK_OFFSET,
            &mut sb,
        )?;
        let csum = crc32c(0xFFFF_FFFF, &sb[..SB_CHECKSUM_OFFSET]);
        device_write(
            &mut self.block,
            self.block_size,
            self.block_count,
            SUPERBLOCK_OFFSET + SB_CHECKSUM_OFFSET as u64,
            &csum.to_le_bytes(),
        )
    }

    /// Device byte offset of group `group`'s descriptor.
    fn group_desc_offset(&self, group: u64) -> Result<u64, DriverError> {
        self.layout
            .gdt_offset
            .checked_add(group * u64::from(self.layout.desc_size))
            .ok_or(DriverError::DeviceFault)
    }

    /// Read group `group`'s descriptor into a fixed buffer.
    fn read_group_desc(&mut self, group: u64) -> Result<[u8; 64], DriverError> {
        let off = self.group_desc_offset(group)?;
        let mut desc = [0u8; 64];
        let len = self.layout.desc_size as usize;
        device_read(
            &mut self.block,
            self.block_size,
            self.block_count,
            off,
            &mut desc[..len],
        )?;
        Ok(desc)
    }

    /// Write group `group`'s descriptor back from a fixed buffer, first
    /// stamping `bg_checksum` (crc32c low half for `metadata_csum`, crc16
    /// for the legacy `gdt_csum`) over the finished descriptor.
    fn write_group_desc(&mut self, group: u64, desc: &mut [u8; 64]) -> Result<(), DriverError> {
        let len = self.layout.desc_size as usize;
        self.stamp_group_desc_checksum(group, desc);
        let off = self.group_desc_offset(group)?;
        device_write(
            &mut self.block,
            self.block_size,
            self.block_count,
            off,
            &desc[..len],
        )
    }

    /// Compute `bg_checksum` over `desc` (with the checksum field treated
    /// as zero) and store it at [`GD_CHECKSUM`]. The `metadata_csum`
    /// crc32c keeps only the low 16 bits.
    fn stamp_group_desc_checksum(&self, group: u64, desc: &mut [u8; 64]) {
        let len = self.layout.desc_size as usize;
        let group_le = u32_le_truncate(group);
        if self.layout.metadata_csum {
            let mut c = crc32c(self.layout.csum_seed, &group_le);
            c = crc32c(c, &desc[..GD_CHECKSUM]);
            c = crc32c(c, &[0, 0]);
            if len > GD_CHECKSUM + 2 {
                c = crc32c(c, &desc[GD_CHECKSUM + 2..len]);
            }
            put_le16(desc, GD_CHECKSUM, (c & 0xFFFF) as u16);
        } else if self.layout.gdt_csum {
            let mut c = crc16(0xFFFF, &self.layout.uuid);
            c = crc16(c, &group_le);
            c = crc16(c, &desc[..GD_CHECKSUM]);
            if len > GD_CHECKSUM + 2 {
                c = crc16(c, &desc[GD_CHECKSUM + 2..len]);
            }
            put_le16(desc, GD_CHECKSUM, c);
        }
    }

    /// Number of leading bitmap bytes a bitmap checksum covers for a
    /// per-group count of `bits` (`(bits + 7) / 8`, capped at the block).
    fn bitmap_csum_bytes(&self, bits: u32) -> usize {
        core::cmp::min((bits as usize).div_ceil(8), self.layout.block_size as usize)
    }

    /// Store a bitmap's crc32c (lo at `lo_off`, hi at `hi_off` on a wide
    /// descriptor) into `desc` when the volume carries `metadata_csum`.
    fn set_bitmap_csum(&self, desc: &mut [u8; 64], bitmap: &[u8], lo_off: usize, hi_off: usize) {
        if !self.layout.metadata_csum {
            return;
        }
        let c = crc32c(self.layout.csum_seed, bitmap);
        put_le16(desc, lo_off, (c & 0xFFFF) as u16);
        if self.layout.wide_desc() {
            put_le16(desc, hi_off, ((c >> 16) & 0xFFFF) as u16);
        }
    }

    /// The per-inode crc32c seed: `crc32c(crc32c(fs_seed, ino), gen)`,
    /// used for both the inode checksum and its extent-block tails.
    fn inode_csum_seed(&self, ino: u32, generation: u32) -> u32 {
        let c = crc32c(self.layout.csum_seed, &ino.to_le_bytes());
        crc32c(c, &generation.to_le_bytes())
    }

    /// Read inode `ino`'s full on-disk record into `raw[..inode_size]`.
    fn read_inode_raw(&mut self, ino: u32, raw: &mut [u8]) -> Result<(), DriverError> {
        let off = self.locate_inode(ino)?;
        let len = self.layout.inode_size as usize;
        device_read(
            &mut self.block,
            self.block_size,
            self.block_count,
            off,
            &mut raw[..len],
        )
    }

    /// Write inode `ino`'s full on-disk record from `raw[..inode_size]`,
    /// first stamping `i_checksum_lo`/`i_checksum_hi` on a `metadata_csum`
    /// volume (the two checksum fields are computed as zero).
    fn write_inode_raw(&mut self, ino: u32, raw: &mut [u8]) -> Result<(), DriverError> {
        let off = self.locate_inode(ino)?;
        let len = self.layout.inode_size as usize;
        if self.layout.metadata_csum {
            let generation = le32(raw, INODE_GENERATION);
            let has_hi = len > 128 && usize::from(le16(raw, I_EXTRA_ISIZE_OFFSET)) >= 4;
            put_le16(raw, INODE_CHECKSUM_LO, 0);
            if has_hi {
                put_le16(raw, INODE_CHECKSUM_HI, 0);
            }
            let seed = self.inode_csum_seed(ino, generation);
            let c = crc32c(seed, &raw[..len]);
            put_le16(raw, INODE_CHECKSUM_LO, (c & 0xFFFF) as u16);
            if has_hi {
                put_le16(raw, INODE_CHECKSUM_HI, ((c >> 16) & 0xFFFF) as u16);
            }
        }
        device_write(
            &mut self.block,
            self.block_size,
            self.block_count,
            off,
            &raw[..len],
        )
    }
}

impl<B: Block> Ext4<B> {
    /// Allocate one free data block, returning its zero-filled absolute
    /// block number. Updates the block bitmap, the group-descriptor free
    /// count, and the superblock free count. Fails with
    /// [`DriverError::NoSpace`] when every group is full.
    fn alloc_block(&mut self) -> Result<u64, DriverError> {
        let bpg = u64::from(self.layout.blocks_per_group);
        let bs = self.layout.block_size as usize;
        for group in 0..self.layout.group_count {
            let mut desc = self.read_group_desc(group)?;
            let free = le16(&desc, 0x0C);
            // Skip a group whose block bitmap is not materialised on disk
            // (fail closed rather than initialise an uninit group).
            if free == 0 || le16(&desc, GD_FLAGS) & BG_BLOCK_UNINIT != 0 {
                continue;
            }
            let bitmap_block = u64::from(le32(&desc, 0x00));
            let mut bm = [0u8; MAX_BLOCK_SIZE as usize];
            self.read_fs_block(bitmap_block, &mut bm)?;
            for bit in 0..bpg {
                let abs = self.layout.first_data_block + group * bpg + bit;
                if abs >= self.layout.blocks_count {
                    break;
                }
                let byte = (bit / 8) as usize;
                if byte >= bs {
                    break;
                }
                let mask = 1u8 << (bit % 8);
                if bm[byte] & mask == 0 {
                    bm[byte] |= mask;
                    self.write_fs_block(bitmap_block, &bm)?;
                    put_le16(&mut desc, 0x0C, free - 1);
                    let nbytes = self.bitmap_csum_bytes(self.layout.blocks_per_group);
                    self.set_bitmap_csum(
                        &mut desc,
                        &bm[..nbytes],
                        GD_BLOCK_BITMAP_CSUM_LO,
                        GD_BLOCK_BITMAP_CSUM_HI,
                    );
                    self.write_group_desc(group, &mut desc)?;
                    let sb_free = self.sb_u32(0x0C)?;
                    self.set_sb_u32(0x0C, sb_free.saturating_sub(1))?;
                    let zero = [0u8; MAX_BLOCK_SIZE as usize];
                    self.write_fs_block(abs, &zero)?;
                    return Ok(abs);
                }
            }
        }
        Err(DriverError::NoSpace)
    }

    /// Release data block `abs`, clearing its bitmap bit and restoring
    /// the group-descriptor and superblock free counts.
    fn free_block(&mut self, abs: u64) -> Result<(), DriverError> {
        if abs < self.layout.first_data_block {
            return Err(DriverError::DeviceFault);
        }
        let rel = abs - self.layout.first_data_block;
        let bpg = u64::from(self.layout.blocks_per_group);
        let group = rel / bpg;
        let bit = rel % bpg;
        if group >= self.layout.group_count {
            return Err(DriverError::DeviceFault);
        }
        let mut desc = self.read_group_desc(group)?;
        let bitmap_block = u64::from(le32(&desc, 0x00));
        let mut bm = [0u8; MAX_BLOCK_SIZE as usize];
        self.read_fs_block(bitmap_block, &mut bm)?;
        let byte = (bit / 8) as usize;
        let mask = 1u8 << (bit % 8);
        if bm[byte] & mask != 0 {
            bm[byte] &= !mask;
            self.write_fs_block(bitmap_block, &bm)?;
            let free = le16(&desc, 0x0C);
            put_le16(&mut desc, 0x0C, free + 1);
            let nbytes = self.bitmap_csum_bytes(self.layout.blocks_per_group);
            self.set_bitmap_csum(
                &mut desc,
                &bm[..nbytes],
                GD_BLOCK_BITMAP_CSUM_LO,
                GD_BLOCK_BITMAP_CSUM_HI,
            );
            self.write_group_desc(group, &mut desc)?;
            let sb_free = self.sb_u32(0x0C)?;
            self.set_sb_u32(0x0C, sb_free + 1)?;
        }
        Ok(())
    }

    /// Allocate one free inode, returning its number. `is_dir` bumps the
    /// group's directory count. Updates the inode bitmap and free counts.
    /// Fails with [`DriverError::NoSpace`] when every group is full.
    fn alloc_inode(&mut self, is_dir: bool) -> Result<u32, DriverError> {
        let ipg = u64::from(self.layout.inodes_per_group);
        let total = u64::from(self.sb_u32(0x00)?);
        let bs = self.layout.block_size as usize;
        for group in 0..self.layout.group_count {
            let mut desc = self.read_group_desc(group)?;
            let free = le16(&desc, 0x0E);
            // Skip a group whose inode bitmap is not materialised on disk
            // (fail closed rather than initialise an uninit group).
            if free == 0 || le16(&desc, GD_FLAGS) & BG_INODE_UNINIT != 0 {
                continue;
            }
            let bitmap_block = u64::from(le32(&desc, 0x04));
            let mut bm = [0u8; MAX_BLOCK_SIZE as usize];
            self.read_fs_block(bitmap_block, &mut bm)?;
            for bit in 0..ipg {
                let ino = group * ipg + bit + 1;
                if ino > total {
                    break;
                }
                let byte = (bit / 8) as usize;
                if byte >= bs {
                    break;
                }
                let mask = 1u8 << (bit % 8);
                if bm[byte] & mask == 0 {
                    bm[byte] |= mask;
                    self.write_fs_block(bitmap_block, &bm)?;
                    put_le16(&mut desc, 0x0E, free - 1);
                    if is_dir {
                        let dirs = le16(&desc, 0x10);
                        put_le16(&mut desc, 0x10, dirs + 1);
                    }
                    self.update_itable_unused(&mut desc, bit);
                    let nbytes = self.bitmap_csum_bytes(self.layout.inodes_per_group);
                    self.set_bitmap_csum(
                        &mut desc,
                        &bm[..nbytes],
                        GD_INODE_BITMAP_CSUM_LO,
                        GD_INODE_BITMAP_CSUM_HI,
                    );
                    self.write_group_desc(group, &mut desc)?;
                    let sb_free = self.sb_u32(0x10)?;
                    self.set_sb_u32(0x10, sb_free.saturating_sub(1))?;
                    return u32::try_from(ino).map_err(|_| DriverError::DeviceFault);
                }
            }
        }
        Err(DriverError::NoSpace)
    }

    /// Lower `bg_itable_unused` (the count of never-used inodes at the
    /// tail of the group's inode table) so it stays consistent after
    /// allocating the inode at zero-based index `bit`. The count is only
    /// present with `gdt_csum`/`metadata_csum`; otherwise this is a no-op.
    fn update_itable_unused(&self, desc: &mut [u8; 64], bit: u64) {
        if !(self.layout.metadata_csum || self.layout.gdt_csum) {
            return;
        }
        let lo = u64::from(le16(desc, GD_ITABLE_UNUSED_LO));
        let hi = if self.layout.wide_desc() {
            u64::from(le16(desc, GD_ITABLE_UNUSED_HI))
        } else {
            0
        };
        let unused = (hi << 16) | lo;
        let used_boundary = bit + 1;
        let max_unused = u64::from(self.layout.inodes_per_group).saturating_sub(used_boundary);
        let new_unused = core::cmp::min(unused, max_unused);
        put_le16(desc, GD_ITABLE_UNUSED_LO, (new_unused & 0xFFFF) as u16);
        if self.layout.wide_desc() {
            put_le16(
                desc,
                GD_ITABLE_UNUSED_HI,
                ((new_unused >> 16) & 0xFFFF) as u16,
            );
        }
    }

    /// Release inode `ino`, clearing its bitmap bit and restoring the
    /// free counts. `is_dir` decrements the group's directory count.
    fn free_inode(&mut self, ino: u32, is_dir: bool) -> Result<(), DriverError> {
        let ipg = u64::from(self.layout.inodes_per_group);
        let group = u64::from(ino - 1) / ipg;
        let bit = u64::from(ino - 1) % ipg;
        if group >= self.layout.group_count {
            return Err(DriverError::DeviceFault);
        }
        let mut desc = self.read_group_desc(group)?;
        let bitmap_block = u64::from(le32(&desc, 0x04));
        let mut bm = [0u8; MAX_BLOCK_SIZE as usize];
        self.read_fs_block(bitmap_block, &mut bm)?;
        let byte = (bit / 8) as usize;
        let mask = 1u8 << (bit % 8);
        if bm[byte] & mask != 0 {
            bm[byte] &= !mask;
            self.write_fs_block(bitmap_block, &bm)?;
            let free = le16(&desc, 0x0E);
            put_le16(&mut desc, 0x0E, free + 1);
            if is_dir {
                let dirs = le16(&desc, 0x10);
                put_le16(&mut desc, 0x10, dirs.saturating_sub(1));
            }
            let nbytes = self.bitmap_csum_bytes(self.layout.inodes_per_group);
            self.set_bitmap_csum(
                &mut desc,
                &bm[..nbytes],
                GD_INODE_BITMAP_CSUM_LO,
                GD_INODE_BITMAP_CSUM_HI,
            );
            self.write_group_desc(group, &mut desc)?;
            let sb_free = self.sb_u32(0x10)?;
            self.set_sb_u32(0x10, sb_free + 1)?;
        }
        Ok(())
    }
}

impl<B: Block> Ext4<B> {
    /// Extent entries an allocated leaf/index block holds: the block,
    /// less the 12-byte header and (on a `metadata_csum` volume) the
    /// 4-byte `ext4_extent_tail`, divided by the 12-byte entry size.
    fn leaf_cap(&self) -> usize {
        let tail = if self.layout.metadata_csum {
            EXTENT_TAIL_LEN
        } else {
            0
        };
        (self.layout.block_size as usize - 12 - tail) / 12
    }

    /// Write extent block `blk` from `buf`, first stamping the
    /// `ext4_extent_tail` crc32c at `12 + eh_max * 12` on a
    /// `metadata_csum` volume. `seed` is the owning inode's
    /// [`Self::inode_csum_seed`].
    fn write_extent_block(
        &mut self,
        seed: u32,
        blk: u64,
        buf: &mut [u8],
    ) -> Result<(), DriverError> {
        if self.layout.metadata_csum {
            let eh_max = usize::from(le16(buf, 4));
            let off = 12 + eh_max * 12;
            if off + EXTENT_TAIL_LEN <= self.layout.block_size as usize {
                let csum = crc32c(seed, &buf[..off]);
                put_le32(buf, off, csum);
            }
        }
        self.write_fs_block(blk, buf)
    }

    /// Map logical block `logical` of inode `ino` (whose raw record is in
    /// `raw`) to a physical block, allocating backing storage (and any
    /// indirect/extent metadata) when it is a hole. `allocated` is bumped
    /// by every filesystem block this call newly allocated so the caller
    /// can maintain `i_blocks`. `ino` seeds extent-block tail checksums.
    fn map_or_alloc(
        &mut self,
        ino: u32,
        raw: &mut [u8],
        logical: u64,
        allocated: &mut u64,
    ) -> Result<u64, DriverError> {
        if le32(raw, 0x20) & INODE_FLAG_EXTENTS != 0 {
            self.map_or_alloc_extent(ino, raw, logical, allocated)
        } else {
            self.map_or_alloc_classic(raw, logical, allocated)
        }
    }

    /// Classic block-map allocation: 12 direct pointers plus the single
    /// indirect block. Double/triple indirect growth is not written
    /// (`DeviceFault`); files this driver creates never reach it.
    fn map_or_alloc_classic(
        &mut self,
        raw: &mut [u8],
        logical: u64,
        allocated: &mut u64,
    ) -> Result<u64, DriverError> {
        let ib = I_BLOCK_OFFSET;
        if logical < 12 {
            let off = ib + usize_of(logical)? * 4;
            let ptr = le32(raw, off);
            if ptr != 0 {
                return Ok(u64::from(ptr));
            }
            let blk = self.alloc_block()?;
            *allocated += 1;
            put_le32(
                raw,
                off,
                u32::try_from(blk).map_err(|_| DriverError::DeviceFault)?,
            );
            return Ok(blk);
        }
        let ppb = self.pointers_per_block();
        let rem = logical - 12;
        if rem >= ppb {
            return Err(DriverError::DeviceFault);
        }
        let mut ind = le32(raw, ib + 48);
        if ind == 0 {
            let blk = self.alloc_block()?;
            *allocated += 1;
            ind = u32::try_from(blk).map_err(|_| DriverError::DeviceFault)?;
            put_le32(raw, ib + 48, ind);
        }
        let mut ind_buf = [0u8; MAX_BLOCK_SIZE as usize];
        self.read_fs_block(u64::from(ind), &mut ind_buf)?;
        let off = usize_of(rem)? * 4;
        let ptr = le32(&ind_buf, off);
        if ptr != 0 {
            return Ok(u64::from(ptr));
        }
        let blk = self.alloc_block()?;
        *allocated += 1;
        put_le32(
            &mut ind_buf,
            off,
            u32::try_from(blk).map_err(|_| DriverError::DeviceFault)?,
        );
        self.write_fs_block(u64::from(ind), &ind_buf)?;
        Ok(blk)
    }

    /// Extent allocation. Serves the depth-0 inline root held in
    /// `i_block` directly — extending the last extent when contiguous,
    /// else appending while the four inline slots last — and grows the
    /// root into a depth-1 tree (a single index level over leaf blocks)
    /// once those slots are exhausted. A tree that would need a second
    /// index level is refused (`DeviceFault`); this driver never builds
    /// one, and the read path still maps any depth on disk.
    fn map_or_alloc_extent(
        &mut self,
        ino: u32,
        raw: &mut [u8],
        logical: u64,
        allocated: &mut u64,
    ) -> Result<u64, DriverError> {
        let ib = I_BLOCK_OFFSET;
        if le16(raw, ib) != EXTENT_MAGIC {
            return Err(DriverError::DeviceFault);
        }
        match le16(raw, ib + 6) {
            0 => {
                if usize::from(le16(raw, ib + 2)) > INLINE_EXTENT_MAX {
                    return Err(DriverError::DeviceFault);
                }
                if let Some(phys) = leaf_find(raw, ib, logical) {
                    return Ok(phys);
                }
                let blk = self.alloc_block()?;
                *allocated += 1;
                if leaf_place(raw, ib, INLINE_EXTENT_MAX, logical, blk, true)? {
                    return Ok(blk);
                }
                self.grow_root_to_depth1(ino, raw, logical, blk, allocated)
            }
            1 => self.alloc_in_depth1(ino, raw, logical, allocated),
            _ => Err(DriverError::DeviceFault),
        }
    }

    /// Convert a full inline depth-0 extent root into a depth-1 tree:
    /// move its extents into a freshly allocated leaf block, place the
    /// new `logical`→`data_blk` mapping there, and rewrite the inode root
    /// as a single index entry pointing at that leaf.
    fn grow_root_to_depth1(
        &mut self,
        ino: u32,
        raw: &mut [u8],
        logical: u64,
        data_blk: u64,
        allocated: &mut u64,
    ) -> Result<u64, DriverError> {
        let ib = I_BLOCK_OFFSET;
        let leaf_cap = self.leaf_cap();
        let seed = self.inode_csum_seed(ino, le32(raw, INODE_GENERATION));
        let entries = usize::from(le16(raw, ib + 2));
        let leaf = self.alloc_block()?;
        *allocated += 1;
        let mut leaf_buf = [0u8; MAX_BLOCK_SIZE as usize];
        put_le16(&mut leaf_buf, 0, EXTENT_MAGIC);
        put_le16(&mut leaf_buf, 2, u16_of(entries)?);
        put_le16(&mut leaf_buf, 4, u16_of(leaf_cap)?);
        put_le16(&mut leaf_buf, 6, 0);
        for i in 0..entries {
            let src = ib + 12 + i * 12;
            let dst = 12 + i * 12;
            leaf_buf[dst..dst + 12].copy_from_slice(&raw[src..src + 12]);
        }
        if !leaf_place(&mut leaf_buf, 0, leaf_cap, logical, data_blk, true)? {
            return Err(DriverError::DeviceFault);
        }
        let first_block = le32(&leaf_buf, 12);
        self.write_extent_block(seed, leaf, &mut leaf_buf)?;
        for b in &mut raw[ib + 12..ib + I_BLOCK_LEN] {
            *b = 0;
        }
        put_le16(raw, ib + 2, 1);
        put_le16(raw, ib + 4, u16_of(INLINE_EXTENT_MAX)?);
        put_le16(raw, ib + 6, 1);
        let off = ib + 12;
        put_le32(raw, off, first_block);
        put_le32(raw, off + 4, u32_of(leaf)?);
        put_le16(raw, off + 8, u16::try_from(leaf >> 32).unwrap_or(0));
        put_le16(raw, off + 10, 0);
        Ok(data_blk)
    }

    /// Allocate `logical` within a depth-1 extent tree rooted in the
    /// inode: descend to the covering leaf, extend/append within it, or
    /// attach a fresh single-extent leaf via a new (ascending-ordered)
    /// root index entry. A tree that would need a second index level is
    /// refused (`DeviceFault`).
    fn alloc_in_depth1(
        &mut self,
        ino: u32,
        raw: &mut [u8],
        logical: u64,
        allocated: &mut u64,
    ) -> Result<u64, DriverError> {
        let ib = I_BLOCK_OFFSET;
        let leaf_cap = self.leaf_cap();
        let seed = self.inode_csum_seed(ino, le32(raw, INODE_GENERATION));
        let entries = usize::from(le16(raw, ib + 2));
        if entries == 0 || entries > INLINE_EXTENT_MAX {
            return Err(DriverError::DeviceFault);
        }
        let mut chosen = 0usize;
        let mut best: Option<u64> = None;
        for i in 0..entries {
            let eib = u64::from(le32(raw, ib + 12 + i * 12));
            let better = match best {
                None => true,
                Some(b) => eib >= b,
            };
            if eib <= logical && better {
                best = Some(eib);
                chosen = i;
            }
        }
        let coff = ib + 12 + chosen * 12;
        let leaf_ptr = (u64::from(le16(raw, coff + 8)) << 32) | u64::from(le32(raw, coff + 4));
        let mut leaf_buf = [0u8; MAX_BLOCK_SIZE as usize];
        self.read_fs_block(leaf_ptr, &mut leaf_buf)?;
        if le16(&leaf_buf, 0) != EXTENT_MAGIC || le16(&leaf_buf, 6) != 0 {
            return Err(DriverError::DeviceFault);
        }
        if let Some(phys) = leaf_find(&leaf_buf, 0, logical) {
            return Ok(phys);
        }
        let is_rightmost = chosen == entries - 1;
        let blk = self.alloc_block()?;
        *allocated += 1;
        if leaf_place(&mut leaf_buf, 0, leaf_cap, logical, blk, is_rightmost)? {
            self.write_extent_block(seed, leaf_ptr, &mut leaf_buf)?;
            return Ok(blk);
        }
        if entries >= INLINE_EXTENT_MAX {
            self.free_block(blk)?;
            return Err(DriverError::DeviceFault);
        }
        let new_leaf = self.alloc_block()?;
        *allocated += 1;
        let mut nb = [0u8; MAX_BLOCK_SIZE as usize];
        put_le16(&mut nb, 0, EXTENT_MAGIC);
        put_le16(&mut nb, 4, u16_of(leaf_cap)?);
        if !leaf_place(&mut nb, 0, leaf_cap, logical, blk, false)? {
            return Err(DriverError::DeviceFault);
        }
        self.write_extent_block(seed, new_leaf, &mut nb)?;
        insert_root_index(raw, entries, logical, new_leaf)?;
        Ok(blk)
    }
}

/// Round `n` up to the next multiple of four (the directory-entry
/// `rec_len` alignment).
fn align4(n: usize) -> usize {
    (n + 3) & !3
}

/// Find logical block `logical` within the leaf extent node whose 12-byte
/// header begins at `hdr` in `buf`, returning the physical block backing
/// it, or `None` when the leaf does not map it (a sparse hole or beyond
/// its extents).
fn leaf_find(buf: &[u8], hdr: usize, logical: u64) -> Option<u64> {
    let entries = usize::from(le16(buf, hdr + 2));
    for i in 0..entries {
        let off = hdr + 12 + i * 12;
        let ee_block = u64::from(le32(buf, off));
        let raw_len = le16(buf, off + 4);
        let len = if raw_len > 32_768 {
            u64::from(raw_len - 32_768)
        } else {
            u64::from(raw_len)
        };
        if len == 0 {
            continue;
        }
        if logical >= ee_block && logical < ee_block + len {
            let phys = (u64::from(le16(buf, off + 6)) << 32) | u64::from(le32(buf, off + 8));
            return Some(phys + (logical - ee_block));
        }
    }
    None
}

/// Insert an index entry for `leaf` covering logical block `ei_block`
/// into the inode extent root, keeping the `entries` existing entries in
/// ascending `ei_block` order.
fn insert_root_index(
    raw: &mut [u8],
    entries: usize,
    ei_block: u64,
    leaf: u64,
) -> Result<(), DriverError> {
    let ib = I_BLOCK_OFFSET;
    let mut pos = entries;
    for i in 0..entries {
        if u64::from(le32(raw, ib + 12 + i * 12)) > ei_block {
            pos = i;
            break;
        }
    }
    raw.copy_within(
        ib + 12 + pos * 12..ib + 12 + entries * 12,
        ib + 12 + (pos + 1) * 12,
    );
    let off = ib + 12 + pos * 12;
    put_le32(raw, off, u32_of(ei_block)?);
    put_le32(raw, off + 4, u32_of(leaf)?);
    put_le16(raw, off + 8, u16::try_from(leaf >> 32).unwrap_or(0));
    put_le16(raw, off + 10, 0);
    put_le16(raw, ib + 2, u16_of(entries + 1)?);
    Ok(())
}

/// Map logical block `logical` to physical block `blk` in the leaf extent
/// node at `hdr`, extending the final extent when `allow_extend` and the
/// new block is logically and physically contiguous, otherwise appending
/// a fresh extent while a slot (`max_entries`) remains. Returns `true`
/// when the mapping was placed, `false` when the leaf is full.
fn leaf_place(
    buf: &mut [u8],
    hdr: usize,
    max_entries: usize,
    logical: u64,
    blk: u64,
    allow_extend: bool,
) -> Result<bool, DriverError> {
    let entries = usize::from(le16(buf, hdr + 2));
    if allow_extend && entries > 0 {
        let off = hdr + 12 + (entries - 1) * 12;
        let ee_block = u64::from(le32(buf, off));
        let raw_len = le16(buf, off + 4);
        let phys = (u64::from(le16(buf, off + 6)) << 32) | u64::from(le32(buf, off + 8));
        if raw_len < 32_768
            && logical == ee_block + u64::from(raw_len)
            && blk == phys + u64::from(raw_len)
        {
            put_le16(buf, off + 4, raw_len + 1);
            return Ok(true);
        }
    }
    if entries < max_entries {
        let off = hdr + 12 + entries * 12;
        put_le32(buf, off, u32_of(logical)?);
        put_le16(buf, off + 4, 1);
        put_le16(buf, off + 6, u16::try_from(blk >> 32).unwrap_or(0));
        put_le32(buf, off + 8, u32_of(blk)?);
        put_le16(buf, hdr + 2, u16_of(entries + 1)?);
        return Ok(true);
    }
    Ok(false)
}

/// Locate the `system.posix_acl_access` value within an extended-attribute
/// region.
///
/// `region` is the whole staged buffer (the inode body, or the external
/// xattr block). `entries_start` is the byte offset of the first
/// `ext4_xattr_entry`; `value_base` is the offset that entries' `e_value_offs`
/// are measured from (the first-entry offset for an inode-body region, `0`
/// for a block). Returns the value slice, or `None` when the attribute is
/// absent or the region is malformed. The walk always advances by at least
/// one entry header, so a corrupt region terminates rather than loops.
fn find_posix_acl(region: &[u8], entries_start: usize, value_base: usize) -> Option<&[u8]> {
    let mut pos = entries_start;
    loop {
        if pos + XATTR_ENTRY_HEADER_LEN > region.len() {
            return None;
        }
        // The end of the entry list is a zeroed first word.
        if le32(region, pos) == 0 {
            return None;
        }
        let name_len = usize::from(region[pos]);
        let name_index = region[pos + 1];
        let value_offs = usize::from(le16(region, pos + 2));
        let value_inum = le32(region, pos + 4);
        let value_size = usize_of(u64::from(le32(region, pos + 8))).ok()?;
        if name_index == XATTR_INDEX_POSIX_ACL_ACCESS && name_len == 0 && value_inum == 0 {
            let start = value_base.checked_add(value_offs)?;
            let end = start.checked_add(value_size)?;
            return region.get(start..end);
        }
        pos = pos.checked_add(align4(XATTR_ENTRY_HEADER_LEN + name_len))?;
    }
}

/// Fold a `POSIX_ACL_XATTR` value into `sec`, pushing one grant-only
/// [`SecurityAcl`] per named-user (`ACL_USER`) and named-group
/// (`ACL_GROUP`) entry. The owner/owning-group/other/mask entries are
/// already expressed by the mode bits, so they are skipped. A value with
/// the wrong version, or one that overflows the inline ACL budget, is
/// folded as far as it cleanly can be — the mode bits always still apply
/// (fail closed, never widen).
fn decode_posix_acl(value: &[u8], sec: &mut NodeSecurity) {
    if value.len() < 4 || le32(value, 0) != POSIX_ACL_VERSION {
        return;
    }
    let mut off = 4;
    while off + POSIX_ACL_ENTRY_LEN <= value.len() {
        let tag = le16(value, off);
        let perms = (le16(value, off + 2) & ACL_PERM_MASK) as u8;
        let id = le32(value, off + 4);
        let subject = match tag {
            ACL_TAG_USER => Some(SecuritySubject::User(id)),
            ACL_TAG_GROUP => Some(SecuritySubject::Group(id)),
            _ => None,
        };
        if let Some(subject) = subject {
            if sec.push_acl(SecurityAcl { subject, perms }).is_err() {
                break;
            }
        }
        off += POSIX_ACL_ENTRY_LEN;
    }
}

impl<B: Block> Ext4<B> {
    /// Sectors (512-byte `i_blocks` units) per filesystem block.
    fn sectors_per_block(&self) -> u32 {
        self.layout.block_size / 512
    }

    /// Usable directory-block bytes for entries: the whole block, less
    /// the 12-byte `ext4_dir_entry_tail` reserved at the end on a
    /// `metadata_csum` volume.
    fn dir_data_end(&self) -> usize {
        self.layout.block_size as usize
            - if self.layout.metadata_csum {
                DIR_TAIL_LEN
            } else {
                0
            }
    }

    /// The crc32c seed for directory `dir_ino`'s leaf-block tails
    /// (`crc32c(crc32c(fs_seed, ino), i_generation)`); `0` when the
    /// volume carries no `metadata_csum` (the seed is then unused).
    fn dir_block_seed(&mut self, dir_ino: u32) -> Result<u32, DriverError> {
        if !self.layout.metadata_csum {
            return Ok(0);
        }
        let mut raw = [0u8; MAX_BLOCK_SIZE as usize];
        self.read_inode_raw(dir_ino, &mut raw)?;
        Ok(self.inode_csum_seed(dir_ino, le32(&raw, INODE_GENERATION)))
    }

    /// Write directory block `phys` from `block`, first writing the
    /// `ext4_dir_entry_tail` and stamping its crc32c on a `metadata_csum`
    /// volume. `seed` is [`Self::dir_block_seed`] of the owning directory.
    fn write_dir_block(
        &mut self,
        seed: u32,
        phys: u64,
        block: &mut [u8],
    ) -> Result<(), DriverError> {
        if self.layout.metadata_csum {
            let end = self.layout.block_size as usize;
            let tail = end - DIR_TAIL_LEN;
            put_le32(block, tail, 0);
            put_le16(block, tail + 4, u16_of(DIR_TAIL_LEN)?);
            block[tail + 6] = 0;
            block[tail + 7] = DIR_TAIL_FT;
            let csum = crc32c(seed, &block[..tail]);
            put_le32(block, end - EXTENT_TAIL_LEN, csum);
        }
        self.write_fs_block(phys, block)
    }

    /// Write a directory entry header + name at `pos` of `block`,
    /// honouring the `filetype` feature for the name-length / file-type
    /// byte layout.
    fn write_dirent(
        &self,
        block: &mut [u8],
        pos: usize,
        ino: u32,
        rec_len: u16,
        name: &[u8],
        file_type: u8,
    ) -> Result<(), DriverError> {
        put_le32(block, pos, ino);
        put_le16(block, pos + 4, rec_len);
        if self.layout.filetype {
            block[pos + 6] = u8::try_from(name.len()).map_err(|_| DriverError::LengthOutOfRange)?;
            block[pos + 7] = file_type;
        } else {
            put_le16(block, pos + 6, u16_of(name.len())?);
        }
        block[pos + DIRENT_HEADER..pos + DIRENT_HEADER + name.len()].copy_from_slice(name);
        Ok(())
    }

    /// Read a directory entry's `(inode, rec_len, name_len)` triple at
    /// `pos`, validating `rec_len` against the block size `bs`.
    fn read_dirent_header(
        &self,
        block: &[u8],
        pos: usize,
        bs: usize,
    ) -> Result<(u32, usize, usize), DriverError> {
        let ino = le32(block, pos);
        let rec_len = usize::from(le16(block, pos + 4));
        if rec_len < DIRENT_HEADER || rec_len % 4 != 0 || pos + rec_len > bs {
            return Err(DriverError::DeviceFault);
        }
        let name_len = if self.layout.filetype {
            usize::from(block[pos + 6])
        } else {
            usize::from(le16(block, pos + 6))
        };
        Ok((ino, rec_len, name_len))
    }

    /// Try to place a `needed`-byte entry into directory block `block`,
    /// either reusing an unused slot or splitting one with slack.
    /// Returns whether the entry was placed.
    fn place_in_block(
        &self,
        block: &mut [u8],
        needed: usize,
        ino: u32,
        name: &[u8],
        file_type: u8,
    ) -> Result<bool, DriverError> {
        let bs = block.len();
        let mut pos = 0usize;
        while pos + DIRENT_HEADER <= bs {
            let (slot_ino, rec_len, name_len) = self.read_dirent_header(block, pos, bs)?;
            let used = if slot_ino == 0 {
                0
            } else {
                align4(DIRENT_HEADER + name_len)
            };
            if rec_len >= used + needed {
                if slot_ino == 0 {
                    self.write_dirent(block, pos, ino, u16_of(rec_len)?, name, file_type)?;
                } else {
                    // Shrink the occupied slot to its actual size by
                    // rewriting only its `rec_len`, preserving the name
                    // and file-type bytes, then place the new entry in the
                    // freed slack.
                    put_le16(block, pos + 4, u16_of(used)?);
                    let np = pos + used;
                    self.write_dirent(block, np, ino, u16_of(rec_len - used)?, name, file_type)?;
                }
                return Ok(true);
            }
            pos += rec_len;
        }
        Ok(false)
    }

    /// Insert child `(child_ino, name, file_type)` into directory inode
    /// `dir_ino`, growing the directory by one block when no existing
    /// block has room.
    fn insert_dirent(
        &mut self,
        dir_ino: u32,
        name: &[u8],
        child_ino: u32,
        file_type: u8,
    ) -> Result<(), DriverError> {
        let needed = align4(DIRENT_HEADER + name.len());
        let end = self.dir_data_end();
        let seed = self.dir_block_seed(dir_ino)?;
        let dir = self.read_inode(dir_ino)?;
        let total_blocks = dir.size.div_ceil(u64::from(self.layout.block_size));
        let mut block_buf = [0u8; MAX_BLOCK_SIZE as usize];
        for logical in 0..total_blocks {
            let Some(phys) = self.map_block(&dir, logical)? else {
                continue;
            };
            self.read_fs_block(phys, &mut block_buf)?;
            if self.place_in_block(&mut block_buf[..end], needed, child_ino, name, file_type)? {
                self.write_dir_block(seed, phys, &mut block_buf)?;
                return Ok(());
            }
        }
        let mut raw = [0u8; MAX_BLOCK_SIZE as usize];
        self.read_inode_raw(dir_ino, &mut raw)?;
        let mut allocated = 0u64;
        let phys = self.map_or_alloc(dir_ino, &mut raw, total_blocks, &mut allocated)?;
        let mut new_block = [0u8; MAX_BLOCK_SIZE as usize];
        self.write_dirent(&mut new_block, 0, child_ino, u16_of(end)?, name, file_type)?;
        self.write_dir_block(seed, phys, &mut new_block)?;
        let new_size = (total_blocks + 1) * u64::from(self.layout.block_size);
        put_le32(&mut raw, 0x04, u32_of(new_size)?);
        let blocks = le32(&raw, 0x1C);
        put_le32(
            &mut raw,
            0x1C,
            blocks + u32_of(allocated)? * self.sectors_per_block(),
        );
        self.write_inode_raw(dir_ino, &mut raw)?;
        Ok(())
    }

    /// Remove the entry named `name` from directory inode `dir_ino`,
    /// returning the child inode number. The freed slot is merged into
    /// the preceding entry (or zeroed when it is first in its block).
    fn remove_dirent(&mut self, dir_ino: u32, name: &[u8]) -> Result<u32, DriverError> {
        let end = self.dir_data_end();
        let seed = self.dir_block_seed(dir_ino)?;
        let dir = self.read_inode(dir_ino)?;
        let total_blocks = dir.size.div_ceil(u64::from(self.layout.block_size));
        let mut block_buf = [0u8; MAX_BLOCK_SIZE as usize];
        for logical in 0..total_blocks {
            let Some(phys) = self.map_block(&dir, logical)? else {
                continue;
            };
            self.read_fs_block(phys, &mut block_buf)?;
            let mut pos = 0usize;
            let mut prev: Option<usize> = None;
            while pos + DIRENT_HEADER <= end {
                let (slot_ino, rec_len, name_len) =
                    self.read_dirent_header(&block_buf, pos, end)?;
                if slot_ino != 0 && name_len > 0 && DIRENT_HEADER + name_len <= rec_len {
                    let slot_name = &block_buf[pos + DIRENT_HEADER..pos + DIRENT_HEADER + name_len];
                    if slot_name == name {
                        match prev {
                            Some(pp) => {
                                let (_, prev_rec, _) =
                                    self.read_dirent_header(&block_buf, pp, end)?;
                                put_le16(&mut block_buf, pp + 4, u16_of(prev_rec + rec_len)?);
                            }
                            None => put_le32(&mut block_buf, pos, 0),
                        }
                        self.write_dir_block(seed, phys, &mut block_buf)?;
                        return Ok(slot_ino);
                    }
                }
                prev = Some(pos);
                pos += rec_len;
            }
        }
        Err(DriverError::NotFound)
    }

    /// Whether directory inode `dir_ino` holds only `.` / `..`.
    fn dir_is_empty(&mut self, dir_ino: u32) -> Result<bool, DriverError> {
        let end = self.dir_data_end();
        let dir = self.read_inode(dir_ino)?;
        let total_blocks = dir.size.div_ceil(u64::from(self.layout.block_size));
        let mut block_buf = [0u8; MAX_BLOCK_SIZE as usize];
        for logical in 0..total_blocks {
            let Some(phys) = self.map_block(&dir, logical)? else {
                continue;
            };
            self.read_fs_block(phys, &mut block_buf)?;
            let mut pos = 0usize;
            while pos + DIRENT_HEADER <= end {
                let (slot_ino, rec_len, name_len) =
                    self.read_dirent_header(&block_buf, pos, end)?;
                if slot_ino != 0 && name_len > 0 && DIRENT_HEADER + name_len <= rec_len {
                    let slot_name = &block_buf[pos + DIRENT_HEADER..pos + DIRENT_HEADER + name_len];
                    if slot_name != b"." && slot_name != b".." {
                        return Ok(false);
                    }
                }
                pos += rec_len;
            }
        }
        Ok(true)
    }

    /// Resolve a child of directory `dir_ino` by name to its inode.
    fn lookup_child(&mut self, dir_ino: u32, name: &[u8]) -> Result<u32, DriverError> {
        let dir = self.read_inode(dir_ino)?;
        if dir.kind() != Some(NodeKind::Directory) {
            return Err(DriverError::Unsupported);
        }
        let mut scratch = [0u8; 0];
        match self.find_entry(&dir, DirQuery::ByName(name), &mut scratch)? {
            Some(found) => Ok(found.ino),
            None => Err(DriverError::NotFound),
        }
    }

    /// Add `delta` to inode `ino`'s `i_links_count`, saturating at the
    /// `u16` bounds. Used to maintain a directory's link count as child
    /// directories (each contributing a `..` back-link) move in and out.
    fn adjust_links(&mut self, ino: u32, delta: i16) -> Result<(), DriverError> {
        let mut raw = [0u8; MAX_BLOCK_SIZE as usize];
        self.read_inode_raw(ino, &mut raw)?;
        let links = le16(&raw, INODE_LINKS);
        let magnitude = delta.unsigned_abs();
        let new = if delta < 0 {
            links.saturating_sub(magnitude)
        } else {
            links.saturating_add(magnitude)
        };
        put_le16(&mut raw, INODE_LINKS, new);
        self.write_inode_raw(ino, &mut raw)
    }

    /// Repoint the directory entry named `name` in directory `dir_ino` at
    /// `new_ino`, leaving its name and record length untouched. Used to
    /// rewrite a moved directory's `..` link to its new parent.
    fn set_dirent_inode(
        &mut self,
        dir_ino: u32,
        name: &[u8],
        new_ino: u32,
    ) -> Result<(), DriverError> {
        let end = self.dir_data_end();
        let seed = self.dir_block_seed(dir_ino)?;
        let dir = self.read_inode(dir_ino)?;
        let total_blocks = dir.size.div_ceil(u64::from(self.layout.block_size));
        let mut block_buf = [0u8; MAX_BLOCK_SIZE as usize];
        for logical in 0..total_blocks {
            let Some(phys) = self.map_block(&dir, logical)? else {
                continue;
            };
            self.read_fs_block(phys, &mut block_buf)?;
            let mut pos = 0usize;
            while pos + DIRENT_HEADER <= end {
                let (slot_ino, rec_len, name_len) =
                    self.read_dirent_header(&block_buf, pos, end)?;
                if slot_ino != 0 && name_len > 0 && DIRENT_HEADER + name_len <= rec_len {
                    let slot_name = &block_buf[pos + DIRENT_HEADER..pos + DIRENT_HEADER + name_len];
                    if slot_name == name {
                        put_le32(&mut block_buf, pos, new_ino);
                        self.write_dir_block(seed, phys, &mut block_buf)?;
                        return Ok(());
                    }
                }
                pos += rec_len;
            }
        }
        Err(DriverError::NotFound)
    }

    /// The parent inode of directory `dir_ino`, read straight from its
    /// `..` entry. The directory reader does not surface `.`/`..`, so the
    /// `..` back-link is read raw here rather than through `lookup_child`.
    fn dir_parent_ino(&mut self, dir_ino: u32) -> Result<u32, DriverError> {
        let end = self.dir_data_end();
        let dir = self.read_inode(dir_ino)?;
        let total_blocks = dir.size.div_ceil(u64::from(self.layout.block_size));
        let mut block_buf = [0u8; MAX_BLOCK_SIZE as usize];
        for logical in 0..total_blocks {
            let Some(phys) = self.map_block(&dir, logical)? else {
                continue;
            };
            self.read_fs_block(phys, &mut block_buf)?;
            let mut pos = 0usize;
            while pos + DIRENT_HEADER <= end {
                let (slot_ino, rec_len, name_len) =
                    self.read_dirent_header(&block_buf, pos, end)?;
                if slot_ino != 0 && name_len > 0 && DIRENT_HEADER + name_len <= rec_len {
                    let slot_name = &block_buf[pos + DIRENT_HEADER..pos + DIRENT_HEADER + name_len];
                    if slot_name == b".." {
                        return Ok(slot_ino);
                    }
                }
                pos += rec_len;
            }
        }
        Err(DriverError::DeviceFault)
    }

    /// Whether directory `candidate` is `ancestor` itself or lives anywhere
    /// beneath it, walking `..` links up to the root. Refuses moving a
    /// directory into its own subtree (which would detach the cycle).
    fn is_subdir_of(&mut self, mut candidate: u32, ancestor: u32) -> Result<bool, DriverError> {
        loop {
            if candidate == ancestor {
                return Ok(true);
            }
            if candidate == ROOT_INODE {
                return Ok(false);
            }
            let parent = self.dir_parent_ino(candidate)?;
            if parent == candidate {
                return Ok(false);
            }
            candidate = parent;
        }
    }

    /// Shared implementation of [`FilesystemWrite::rename`].
    ///
    /// The move re-links the source inode under the destination name and
    /// unlinks the source name, so file data and the inode's identity are
    /// preserved. An existing destination of a compatible kind is replaced
    /// (its inode freed); across directories a moved directory's `..` is
    /// repointed and both parents' link counts adjusted. ext4 here is
    /// non-journaled, so replacement is best-effort rather than atomic,
    /// matching the create/remove paths.
    fn rename_inner(
        &mut self,
        src_dir: NodeId,
        src_name: &[u8],
        dst_dir: NodeId,
        dst_name: &[u8],
    ) -> Result<(), DriverError> {
        validate_name(dst_name)?;
        let src_dir_ino = node_inode(src_dir)?;
        let dst_dir_ino = node_inode(dst_dir)?;
        if self.read_inode(src_dir_ino)?.kind() != Some(NodeKind::Directory)
            || self.read_inode(dst_dir_ino)?.kind() != Some(NodeKind::Directory)
        {
            return Err(DriverError::Unsupported);
        }

        if src_dir_ino == dst_dir_ino && src_name == dst_name {
            return Ok(());
        }

        let src_ino = self.lookup_child(src_dir_ino, src_name)?;
        let mut src_raw = [0u8; MAX_BLOCK_SIZE as usize];
        self.read_inode_raw(src_ino, &mut src_raw)?;
        let moving_dir = le16(&src_raw, 0) & S_IFMT == S_IFDIR;

        if moving_dir && self.is_subdir_of(dst_dir_ino, src_ino)? {
            return Err(DriverError::Busy);
        }

        let dst_existing = match self.lookup_child(dst_dir_ino, dst_name) {
            Ok(ino) => Some(ino),
            Err(DriverError::NotFound) => None,
            Err(e) => return Err(e),
        };
        if let Some(dst_ino) = dst_existing {
            if dst_ino == src_ino {
                return Ok(());
            }
            let mut dst_raw = [0u8; MAX_BLOCK_SIZE as usize];
            self.read_inode_raw(dst_ino, &mut dst_raw)?;
            let dst_is_dir = le16(&dst_raw, 0) & S_IFMT == S_IFDIR;
            if dst_is_dir != moving_dir {
                return Err(DriverError::Unsupported);
            }
            if dst_is_dir && !self.dir_is_empty(dst_ino)? {
                return Err(DriverError::Busy);
            }
            self.truncate_blocks(dst_ino, &mut dst_raw, 0)?;
            put_le16(&mut dst_raw, INODE_LINKS, 0);
            put_le32(&mut dst_raw, 0x04, 0);
            put_le32(&mut dst_raw, 0x6C, 0);
            put_le32(&mut dst_raw, INODE_BLOCKS_LO, 0);
            put_le32(&mut dst_raw, INODE_DTIME, DELETED_DTIME);
            self.write_inode_raw(dst_ino, &mut dst_raw)?;
            self.remove_dirent(dst_dir_ino, dst_name)?;
            self.free_inode(dst_ino, dst_is_dir)?;
            if dst_is_dir {
                self.adjust_links(dst_dir_ino, -1)?;
            }
        }

        let file_type = if moving_dir { FT_DIR } else { FT_REG };
        self.insert_dirent(dst_dir_ino, dst_name, src_ino, file_type)?;
        self.remove_dirent(src_dir_ino, src_name)?;

        if moving_dir && src_dir_ino != dst_dir_ino {
            self.set_dirent_inode(src_ino, b"..", dst_dir_ino)?;
            self.adjust_links(src_dir_ino, -1)?;
            self.adjust_links(dst_dir_ino, 1)?;
        }
        Ok(())
    }
}

impl<B: Block> Ext4<B> {
    /// The [`NodeInfo`] of a decoded inode record: its kind, apparent
    /// size, and the real allocation `i_blocks` tracks. The one
    /// definition `node_info` and `read_dir` both report, so the two can
    /// never disagree about a node's sizes.
    fn inode_info(&self, inode: &Inode) -> Result<NodeInfo, DriverError> {
        let kind = inode.kind().ok_or(DriverError::NotFound)?;
        let size = match kind {
            NodeKind::Directory => 0,
            NodeKind::RegularFile => inode.size,
        };
        // `i_blocks` counts 512-byte sectors, or whole filesystem blocks
        // for a huge-file inode.
        let unit = if inode.flags & INODE_FLAG_HUGE_FILE != 0 {
            u64::from(self.layout.block_size)
        } else {
            512
        };
        let allocated = inode.blocks.saturating_mul(unit);
        Ok(NodeInfo {
            kind,
            size,
            allocated,
            times: inode.times,
        })
    }
}

impl<B: Block> FilesystemRead for Ext4<B> {
    fn root(&self) -> NodeId {
        NodeId::from_raw(u64::from(ROOT_INODE))
    }

    fn node_info(&mut self, node: NodeId) -> Result<NodeInfo, DriverError> {
        let ino = node_inode(node)?;
        let inode = self.read_inode(ino)?;
        self.inode_info(&inode)
    }

    fn lookup(&mut self, dir: NodeId, name: &[u8]) -> Result<NodeId, DriverError> {
        let ino = node_inode(dir)?;
        let inode = self.read_inode(ino)?;
        if inode.kind() != Some(NodeKind::Directory) {
            return Err(DriverError::Unsupported);
        }
        let mut scratch = [0u8; 0];
        match self.find_entry(&inode, DirQuery::ByName(name), &mut scratch)? {
            Some(found) => Ok(NodeId::from_raw(u64::from(found.ino))),
            None => Err(DriverError::NotFound),
        }
    }

    fn read_at(&mut self, file: NodeId, offset: u64, buf: &mut [u8]) -> Result<usize, DriverError> {
        let ino = node_inode(file)?;
        let inode = self.read_inode(ino)?;
        match inode.kind() {
            Some(NodeKind::RegularFile) => self.read_data(&inode, offset, buf),
            Some(NodeKind::Directory) => Err(DriverError::Unsupported),
            None => Err(DriverError::NotFound),
        }
    }

    fn read_dir(
        &mut self,
        dir: NodeId,
        cursor: u64,
        name_out: &mut [u8],
    ) -> Result<Option<DirEntry>, DriverError> {
        let ino = node_inode(dir)?;
        let inode = self.read_inode(ino)?;
        if inode.kind() != Some(NodeKind::Directory) {
            return Err(DriverError::Unsupported);
        }
        let Some(found) = self.find_entry(&inode, DirQuery::ByCursor(cursor), name_out)? else {
            return Ok(None);
        };
        // The child inode is read once here and its metadata returned with
        // the entry, so a listing consumer never re-resolves the child by
        // path to learn its kind or sizes.
        let child = self.read_inode(found.ino)?;
        Ok(Some(DirEntry {
            node: NodeId::from_raw(u64::from(found.ino)),
            info: self.inode_info(&child)?,
            name_len: found.name_len,
            next_cursor: found.next_cursor,
        }))
    }
}

impl<B: Block> Ext4<B> {
    /// Fold inode `ino`'s POSIX-ACL extended attributes into `sec`.
    ///
    /// ext4 keeps `system.posix_acl_access` in two places: an inline
    /// region in the tail of an enlarged (`inode_size > 128`) inode record,
    /// after `i_extra_isize`, and/or an external block named by
    /// `i_file_acl`. Both share the same entry encoding, differing only in
    /// where `e_value_offs` is measured from. A volume may use either, both,
    /// or neither; an absent or malformed region simply contributes no
    /// grants (the mode bits still apply).
    fn decode_inode_acl(
        &mut self,
        ino: u32,
        inode: &Inode,
        sec: &mut NodeSecurity,
    ) -> Result<(), DriverError> {
        let inode_size = self.layout.inode_size as usize;
        if inode_size > I_EXTRA_ISIZE_OFFSET {
            let offset = self.locate_inode(ino)?;
            let staged = inode_size.min(MAX_INODE_SIZE);
            let mut raw = [0u8; MAX_INODE_SIZE];
            device_read(
                &mut self.block,
                self.block_size,
                self.block_count,
                offset,
                &mut raw[..staged],
            )?;
            let extra = usize::from(le16(&raw, I_EXTRA_ISIZE_OFFSET));
            let header = I_EXTRA_ISIZE_OFFSET + extra;
            if header + XATTR_IBODY_HEADER_LEN <= staged && le32(&raw, header) == XATTR_MAGIC {
                let entries_start = header + XATTR_IBODY_HEADER_LEN;
                if let Some(value) = find_posix_acl(&raw[..staged], entries_start, entries_start) {
                    decode_posix_acl(value, sec);
                }
            }
        }

        if inode.file_acl != 0 && inode.file_acl < self.layout.blocks_count {
            let bs = self.layout.block_size as usize;
            let mut block = [0u8; MAX_BLOCK_SIZE as usize];
            self.read_fs_block(inode.file_acl, &mut block[..bs])?;
            if le32(&block, 0) == XATTR_MAGIC {
                if let Some(value) = find_posix_acl(&block[..bs], XATTR_BLOCK_HEADER_LEN, 0) {
                    decode_posix_acl(value, sec);
                }
            }
        }
        Ok(())
    }
}

impl<B: Block> FilesystemSecurity for Ext4<B> {
    fn security(&mut self, node: NodeId) -> Result<NodeSecurity, DriverError> {
        let ino = node_inode(node)?;
        let inode = self.read_inode(ino)?;
        if inode.kind().is_none() {
            return Err(DriverError::NotFound);
        }
        let mut sec = inode.security();
        self.decode_inode_acl(ino, &inode, &mut sec)?;
        Ok(sec)
    }

    fn set_security(&mut self, _node: NodeId, _security: NodeSecurity) -> Result<(), DriverError> {
        // The ext4 on-disk format cannot faithfully store a full TAIRiX
        // security record: it has no representation for the capability
        // gate, and this driver does not rewrite POSIX-ACL xattr blocks.
        // Storing a silently-lossy record is forbidden, so the write is
        // refused whole (fail closed); the record a node was created
        // with stands.
        Err(DriverError::Unsupported)
    }
}

/// The on-disk format has nowhere to store TAIRiX extended attributes, so
/// the default facet answer stands: a mounted volume refuses the
/// `fs_attr_*` surface with the typed unsupported-backing error.
impl<B: Block> FilesystemAttrsProvider for Ext4<B> {}

impl<B: Block> FilesystemStats for Ext4<B> {
    fn stats(&mut self) -> Result<VolumeStats, DriverError> {
        // Read the live superblock: this driver's own write path maintains
        // the on-disk free counts, so the device is the single source of
        // truth (an in-memory shadow could drift from a crash-recovered
        // volume). One bounded read per query, off every hot path.
        let mut sb = [0u8; SUPERBLOCK_LEN];
        device_read(
            &mut self.block,
            self.block_size,
            self.block_count,
            SUPERBLOCK_OFFSET,
            &mut sb,
        )?;
        let is_64bit = le32(&sb, 0x60) & INCOMPAT_64BIT != 0;
        let hi = |offset: usize| {
            if is_64bit {
                u64::from(le32(&sb, offset)) << 32
            } else {
                0
            }
        };
        let total_blocks = u64::from(le32(&sb, 0x04)) | hi(0x150);
        let reserved_blocks = u64::from(le32(&sb, 0x08)) | hi(0x154);
        let free_blocks = u64::from(le32(&sb, 0x0C)) | hi(0x158);
        Ok(VolumeStats {
            block_size: self.layout.block_size,
            total_blocks,
            free_blocks,
            // Blocks reserved for the superuser are free but not available
            // to an ordinary allocation, the POSIX `f_bavail` distinction.
            avail_blocks: free_blocks.saturating_sub(reserved_blocks),
            files: u64::from(le32(&sb, 0x00)),
            files_free: u64::from(le32(&sb, 0x10)),
        })
    }
}

/// Longest directory-entry component this driver writes (the ext
/// `EXT4_NAME_LEN`).
const MAX_NAME_LEN: usize = 255;

/// `i_mode` for a regular file this driver creates (`0o644`).
const NEW_FILE_MODE: u16 = S_IFREG | 0o644;
/// `i_mode` for a directory this driver creates (`0o755`).
const NEW_DIR_MODE: u16 = S_IFDIR | 0o755;

/// Validate a path component the write surface will store.
fn validate_name(name: &[u8]) -> Result<(), DriverError> {
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return Err(DriverError::LengthOutOfRange);
    }
    if name == b"." || name == b".." || name.iter().any(|&b| b == b'/' || b == 0) {
        return Err(DriverError::LengthOutOfRange);
    }
    Ok(())
}

impl<B: Block> Ext4<B> {
    /// Free every block of the inode in `raw` whose logical index is at
    /// least `keep`, compacting the mapping in place. Returns the number
    /// of filesystem blocks freed (for `i_blocks` maintenance). Supports
    /// the classic map and the inline depth-0 extent root; anything else
    /// is refused (`Unsupported`).
    fn truncate_blocks(&mut self, ino: u32, raw: &mut [u8], keep: u64) -> Result<u64, DriverError> {
        if le32(raw, 0x20) & INODE_FLAG_EXTENTS != 0 {
            self.truncate_extent_blocks(ino, raw, keep)
        } else {
            self.truncate_classic_blocks(raw, keep)
        }
    }

    /// [`Self::truncate_blocks`] for the extent map. Handles the inline
    /// depth-0 root and a depth-1 tree (freeing emptied leaf blocks and
    /// dropping their root index entries); a deeper tree is refused
    /// (`Unsupported`).
    fn truncate_extent_blocks(
        &mut self,
        ino: u32,
        raw: &mut [u8],
        keep: u64,
    ) -> Result<u64, DriverError> {
        let ib = I_BLOCK_OFFSET;
        if le16(raw, ib) != EXTENT_MAGIC {
            return Err(DriverError::Unsupported);
        }
        match le16(raw, ib + 6) {
            0 => {
                if usize::from(le16(raw, ib + 2)) > INLINE_EXTENT_MAX {
                    return Err(DriverError::Unsupported);
                }
                let (freed, _kept) = self.trim_leaf(raw, ib, INLINE_EXTENT_MAX, keep)?;
                Ok(freed)
            }
            1 => self.truncate_depth1_blocks(ino, raw, keep),
            _ => Err(DriverError::Unsupported),
        }
    }

    /// Free every extent at or beyond logical block `keep` within the
    /// leaf extent node whose 12-byte header begins at `hdr` in `buf`,
    /// compacting (and, where `keep` falls inside one, trimming) the
    /// survivors in place. Returns `(blocks_freed, surviving_extents)`.
    fn trim_leaf(
        &mut self,
        buf: &mut [u8],
        hdr: usize,
        max_entries: usize,
        keep: u64,
    ) -> Result<(u64, usize), DriverError> {
        let entries = usize::from(le16(buf, hdr + 2));
        if entries > max_entries {
            return Err(DriverError::Unsupported);
        }
        let mut freed = 0u64;
        let mut kept = 0usize;
        for i in 0..entries {
            let off = hdr + 12 + i * 12;
            let ee_block = u64::from(le32(buf, off));
            let raw_len = le16(buf, off + 4);
            let len = if raw_len > 32_768 {
                u64::from(raw_len - 32_768)
            } else {
                u64::from(raw_len)
            };
            let phys = (u64::from(le16(buf, off + 6)) << 32) | u64::from(le32(buf, off + 8));
            if ee_block >= keep {
                for b in 0..len {
                    self.free_block(phys + b)?;
                    freed += 1;
                }
            } else {
                let keep_len = if ee_block + len <= keep {
                    raw_len
                } else {
                    for b in (keep - ee_block)..len {
                        self.free_block(phys + b)?;
                        freed += 1;
                    }
                    u16_of(usize_of(keep - ee_block)?)?
                };
                let dst = hdr + 12 + kept * 12;
                put_le32(buf, dst, u32_of(ee_block)?);
                put_le16(buf, dst + 4, keep_len);
                put_le16(buf, dst + 6, u16::try_from(phys >> 32).unwrap_or(0));
                put_le32(buf, dst + 8, u32_of(phys)?);
                kept += 1;
            }
        }
        for i in kept..entries {
            let off = hdr + 12 + i * 12;
            for b in &mut buf[off..off + 12] {
                *b = 0;
            }
        }
        put_le16(buf, hdr + 2, u16_of(kept)?);
        Ok((freed, kept))
    }

    /// [`Self::truncate_extent_blocks`] for a depth-1 tree: trim each
    /// leaf to `keep`, free a leaf left empty, drop its root index entry,
    /// and collapse the root back to an empty depth-0 node when no leaf
    /// survives.
    fn truncate_depth1_blocks(
        &mut self,
        ino: u32,
        raw: &mut [u8],
        keep: u64,
    ) -> Result<u64, DriverError> {
        let ib = I_BLOCK_OFFSET;
        let leaf_cap = self.leaf_cap();
        let seed = self.inode_csum_seed(ino, le32(raw, INODE_GENERATION));
        let entries = usize::from(le16(raw, ib + 2));
        if entries == 0 || entries > INLINE_EXTENT_MAX {
            return Err(DriverError::Unsupported);
        }
        let mut freed = 0u64;
        let mut kept = 0usize;
        let mut leaf_buf = [0u8; MAX_BLOCK_SIZE as usize];
        for i in 0..entries {
            let off = ib + 12 + i * 12;
            let ei_block = u64::from(le32(raw, off));
            let leaf_ptr = (u64::from(le16(raw, off + 8)) << 32) | u64::from(le32(raw, off + 4));
            self.read_fs_block(leaf_ptr, &mut leaf_buf)?;
            if le16(&leaf_buf, 0) != EXTENT_MAGIC || le16(&leaf_buf, 6) != 0 {
                return Err(DriverError::Unsupported);
            }
            let (leaf_freed, surviving) = self.trim_leaf(&mut leaf_buf, 0, leaf_cap, keep)?;
            freed += leaf_freed;
            if surviving == 0 {
                self.free_block(leaf_ptr)?;
                freed += 1;
            } else {
                self.write_extent_block(seed, leaf_ptr, &mut leaf_buf)?;
                let dst = ib + 12 + kept * 12;
                put_le32(raw, dst, u32_of(ei_block)?);
                put_le32(raw, dst + 4, u32_of(leaf_ptr)?);
                put_le16(raw, dst + 8, u16::try_from(leaf_ptr >> 32).unwrap_or(0));
                put_le16(raw, dst + 10, 0);
                kept += 1;
            }
        }
        for i in kept..entries {
            let off = ib + 12 + i * 12;
            for b in &mut raw[off..off + 12] {
                *b = 0;
            }
        }
        if kept == 0 {
            put_le16(raw, ib + 2, 0);
            put_le16(raw, ib + 4, u16_of(INLINE_EXTENT_MAX)?);
            put_le16(raw, ib + 6, 0);
        } else {
            put_le16(raw, ib + 2, u16_of(kept)?);
        }
        Ok(freed)
    }

    /// [`Self::truncate_blocks`] for the classic direct + single-indirect
    /// block map.
    fn truncate_classic_blocks(&mut self, raw: &mut [u8], keep: u64) -> Result<u64, DriverError> {
        let ib = I_BLOCK_OFFSET;
        let mut freed = 0u64;
        for logical in keep.min(12)..12 {
            let off = ib + usize_of(logical)? * 4;
            let ptr = le32(raw, off);
            if ptr != 0 {
                self.free_block(u64::from(ptr))?;
                freed += 1;
                put_le32(raw, off, 0);
            }
        }
        let ind = le32(raw, ib + 48);
        if ind != 0 {
            let mut ind_buf = [0u8; MAX_BLOCK_SIZE as usize];
            self.read_fs_block(u64::from(ind), &mut ind_buf)?;
            let ppb = usize_of(self.pointers_per_block())?;
            let mut remaining = false;
            let mut modified = false;
            for idx in 0..ppb {
                let logical = 12 + idx as u64;
                let ptr = le32(&ind_buf, idx * 4);
                if ptr != 0 {
                    if logical >= keep {
                        self.free_block(u64::from(ptr))?;
                        freed += 1;
                        put_le32(&mut ind_buf, idx * 4, 0);
                        modified = true;
                    } else {
                        remaining = true;
                    }
                }
            }
            if modified {
                self.write_fs_block(u64::from(ind), &ind_buf)?;
            }
            if !remaining {
                self.free_block(u64::from(ind))?;
                freed += 1;
                put_le32(raw, ib + 48, 0);
            }
        }
        if le32(raw, ib + 52) != 0 || le32(raw, ib + 56) != 0 {
            return Err(DriverError::Unsupported);
        }
        Ok(freed)
    }

    /// Read a regular-file inode's raw record for mutation, rejecting a
    /// directory (`Unsupported`).
    fn open_regular_for_write(
        &mut self,
        ino: u32,
    ) -> Result<[u8; MAX_BLOCK_SIZE as usize], DriverError> {
        let mut raw = [0u8; MAX_BLOCK_SIZE as usize];
        self.read_inode_raw(ino, &mut raw)?;
        if le16(&raw, 0) & S_IFMT != S_IFREG {
            return Err(DriverError::Unsupported);
        }
        Ok(raw)
    }
}

impl<B: Block> FilesystemWrite for Ext4<B> {
    fn create(&mut self, dir: NodeId, name: &[u8], kind: NodeKind) -> Result<NodeId, DriverError> {
        self.ensure_writable()?;
        validate_name(name)?;
        let dir_ino = node_inode(dir)?;
        let dir_inode = self.read_inode(dir_ino)?;
        if dir_inode.kind() != Some(NodeKind::Directory) {
            return Err(DriverError::Unsupported);
        }
        let mut scratch = [0u8; 0];
        if self
            .find_entry(&dir_inode, DirQuery::ByName(name), &mut scratch)?
            .is_some()
        {
            return Err(DriverError::Busy);
        }

        let is_dir = kind == NodeKind::Directory;
        let new_ino = self.alloc_inode(is_dir)?;
        let mut raw = [0u8; MAX_BLOCK_SIZE as usize];
        let bs = self.layout.block_size as usize;
        if is_dir {
            put_le16(&mut raw, 0, NEW_DIR_MODE);
            put_le16(&mut raw, 0x1A, 2);
            let blk = match self.alloc_block() {
                Ok(b) => b,
                Err(e) => {
                    let _ = self.free_inode(new_ino, true);
                    return Err(e);
                }
            };
            let end = self.dir_data_end();
            let mut dir_block = [0u8; MAX_BLOCK_SIZE as usize];
            self.write_dirent(&mut dir_block, 0, new_ino, 12, b".", FT_DIR)?;
            self.write_dirent(
                &mut dir_block,
                12,
                dir_ino,
                u16_of(end - 12)?,
                b"..",
                FT_DIR,
            )?;
            let seed = self.inode_csum_seed(new_ino, 0);
            self.write_dir_block(seed, blk, &mut dir_block)?;
            put_le32(&mut raw, I_BLOCK_OFFSET, u32_of(blk)?);
            put_le32(&mut raw, 0x04, u32_of(bs as u64)?);
            put_le32(&mut raw, 0x1C, self.sectors_per_block());
        } else {
            put_le16(&mut raw, 0, NEW_FILE_MODE);
            put_le16(&mut raw, 0x1A, 1);
        }
        // Match the mke2fs default so the inode checksum's high half is
        // covered on an enlarged-inode volume.
        if self.layout.inode_size > 128 {
            put_le16(&mut raw, I_EXTRA_ISIZE_OFFSET, NEW_EXTRA_ISIZE);
        }
        self.write_inode_raw(new_ino, &mut raw)?;

        let file_type = if is_dir { FT_DIR } else { FT_REG };
        self.insert_dirent(dir_ino, name, new_ino, file_type)?;
        if is_dir {
            let mut draw = [0u8; MAX_BLOCK_SIZE as usize];
            self.read_inode_raw(dir_ino, &mut draw)?;
            let links = le16(&draw, 0x1A);
            put_le16(&mut draw, 0x1A, links + 1);
            self.write_inode_raw(dir_ino, &mut draw)?;
        }
        Ok(NodeId::from_raw(u64::from(new_ino)))
    }

    fn write_at(
        &mut self,
        dir: NodeId,
        name: &[u8],
        offset: u64,
        data: &[u8],
    ) -> Result<usize, DriverError> {
        self.ensure_writable()?;
        let dir_ino = node_inode(dir)?;
        let child = self.lookup_child(dir_ino, name)?;
        let mut raw = self.open_regular_for_write(child)?;
        if data.is_empty() {
            return Ok(0);
        }
        let bs = u64::from(self.layout.block_size);
        let size = (u64::from(le32(&raw, 0x6C)) << 32) | u64::from(le32(&raw, 0x04));
        let mut allocated = 0u64;
        let mut written = 0usize;
        let mut block_buf = [0u8; MAX_BLOCK_SIZE as usize];
        while written < data.len() {
            let cursor = offset + written as u64;
            let logical = cursor / bs;
            let within = usize::try_from(cursor % bs).map_err(|_| DriverError::DeviceFault)?;
            let take = core::cmp::min(
                self.layout.block_size as usize - within,
                data.len() - written,
            );
            let phys = self.map_or_alloc(child, &mut raw, logical, &mut allocated)?;
            self.read_fs_block(phys, &mut block_buf)?;
            block_buf[within..within + take].copy_from_slice(&data[written..written + take]);
            self.write_fs_block(phys, &block_buf)?;
            written += take;
        }
        let new_size = core::cmp::max(size, offset + data.len() as u64);
        put_le32(&mut raw, 0x04, u32_of(new_size)?);
        put_le32(&mut raw, 0x6C, u32_of(new_size >> 32)?);
        let blocks = le32(&raw, 0x1C);
        put_le32(
            &mut raw,
            0x1C,
            blocks + u32_of(allocated)? * self.sectors_per_block(),
        );
        self.write_inode_raw(child, &mut raw)?;
        Ok(written)
    }

    fn truncate(&mut self, dir: NodeId, name: &[u8], size: u64) -> Result<(), DriverError> {
        self.ensure_writable()?;
        let dir_ino = node_inode(dir)?;
        let child = self.lookup_child(dir_ino, name)?;
        let mut raw = self.open_regular_for_write(child)?;
        let cur = (u64::from(le32(&raw, 0x6C)) << 32) | u64::from(le32(&raw, 0x04));
        let bs = u64::from(self.layout.block_size);
        if size < cur {
            let keep = size.div_ceil(bs);
            let freed = self.truncate_blocks(child, &mut raw, keep)?;
            let blocks = le32(&raw, 0x1C);
            let dec = u32_of(freed)? * self.sectors_per_block();
            put_le32(&mut raw, 0x1C, blocks.saturating_sub(dec));
        }
        put_le32(&mut raw, 0x04, u32_of(size)?);
        put_le32(&mut raw, 0x6C, u32_of(size >> 32)?);
        self.write_inode_raw(child, &mut raw)?;

        // Zero the tail of a retained partial block so a later extension
        // reads back zeros rather than the discarded bytes (POSIX).
        let within = usize::try_from(size % bs).map_err(|_| DriverError::DeviceFault)?;
        if size < cur && within != 0 {
            let inode = self.read_inode(child)?;
            if let Some(phys) = self.map_block(&inode, size / bs)? {
                let mut block_buf = [0u8; MAX_BLOCK_SIZE as usize];
                self.read_fs_block(phys, &mut block_buf)?;
                for b in &mut block_buf[within..self.layout.block_size as usize] {
                    *b = 0;
                }
                self.write_fs_block(phys, &block_buf)?;
            }
        }
        Ok(())
    }

    fn remove(&mut self, dir: NodeId, name: &[u8]) -> Result<(), DriverError> {
        self.ensure_writable()?;
        let dir_ino = node_inode(dir)?;
        let dir_inode = self.read_inode(dir_ino)?;
        if dir_inode.kind() != Some(NodeKind::Directory) {
            return Err(DriverError::Unsupported);
        }
        let child = self.lookup_child(dir_ino, name)?;
        let mut raw = [0u8; MAX_BLOCK_SIZE as usize];
        self.read_inode_raw(child, &mut raw)?;
        let mode = le16(&raw, 0);
        let is_dir = mode & S_IFMT == S_IFDIR;
        if is_dir && !self.dir_is_empty(child)? {
            return Err(DriverError::Busy);
        }
        self.truncate_blocks(child, &mut raw, 0)?;
        // Mark the inode deleted: drop every link, zero the size and
        // block count, and stamp `i_dtime` so a checker treats it as a
        // freed inode rather than a live but unreferenced one.
        put_le16(&mut raw, INODE_LINKS, 0);
        put_le32(&mut raw, 0x04, 0);
        put_le32(&mut raw, 0x6C, 0);
        put_le32(&mut raw, INODE_BLOCKS_LO, 0);
        put_le32(&mut raw, INODE_DTIME, DELETED_DTIME);
        self.write_inode_raw(child, &mut raw)?;
        self.remove_dirent(dir_ino, name)?;
        self.free_inode(child, is_dir)?;
        if is_dir {
            let mut draw = [0u8; MAX_BLOCK_SIZE as usize];
            self.read_inode_raw(dir_ino, &mut draw)?;
            let links = le16(&draw, 0x1A);
            put_le16(&mut draw, 0x1A, links.saturating_sub(1));
            self.write_inode_raw(dir_ino, &mut draw)?;
        }
        Ok(())
    }

    fn rename(
        &mut self,
        src_dir: NodeId,
        src_name: &[u8],
        dst_dir: NodeId,
        dst_name: &[u8],
    ) -> Result<(), DriverError> {
        self.ensure_writable()?;
        self.rename_inner(src_dir, src_name, dst_dir, dst_name)
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        Ok(())
    }
}

/// Decode a [`NodeId`] back into an on-disk inode number.
fn node_inode(node: NodeId) -> Result<u32, DriverError> {
    if node == NodeId::NONE {
        return Err(DriverError::NotFound);
    }
    u32::try_from(node.raw()).map_err(|_| DriverError::NotFound)
}

mod format;

#[cfg(test)]
mod tests;
