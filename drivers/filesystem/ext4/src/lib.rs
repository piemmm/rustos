//! RustOS ext4 filesystem driver (read/write).
//!
//! Attaches an ext2/ext3/ext4 volume sitting behind any
//! [`rustos_abi::driver::block::Block`] device and exposes it through
//! the versioned [`rustos_abi::driver::filesystem::FilesystemRead`],
//! [`FilesystemWrite`], and [`FilesystemSecurity`] surfaces
//! (`AGENTS.md` §2.4 / §9 — new behaviour ships as a new trait,
//! never by widening the frozen mount/unmount
//! [`Filesystem`](rustos_abi::driver::filesystem::Filesystem)).
//!
//! The driver makes **no** permission decisions: owner, mode, ACL, and
//! the §5.3 capability gate live in the VFS metadata layer that mounts
//! this driver (`AGENTS.md` §5.4 — the VFS is the policy point, this is
//! raw structural I/O).
//!
//! # Public surface
//!
//! Per `AGENTS.md` §8 the only public *function* is [`register`].
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
//! block map. Because correct on-disk checksums and wide descriptors are
//! a prerequisite for safe mutation, the write path refuses
//! ([`DriverError::Unsupported`]) any volume carrying the
//! `metadata_csum`, `gdt_csum`, or `64bit` features (such volumes stay
//! fully readable); it likewise refuses to free a mapping that is
//! neither the classic map nor an inline depth-0 extent root, rather
//! than orphan blocks (`AGENTS.md` §2.1 / §5.4 — fail closed).
//!
//! No `unwrap`/`expect`/`panic!` and no `unsafe`.
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
    DirEntry, FilesystemRead, FilesystemSecurity, FilesystemWrite, NodeId, NodeInfo, NodeKind,
    NodeSecurity, SecurityAcl, SecuritySubject,
};
use rustos_abi::{CapabilityId, DriverError, DriverHandle, DriverHost};

/// Per-driver `DriverHandle` marker returned by [`register`].
const REGISTER_HANDLE_MARKER: u64 = 0x4558_5434_0000_0001; // "EXT4" + index

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

/// Largest device logical-block size the driver stages through its
/// on-stack scratch buffer. No Tier-1 block device exceeds 4096 bytes
/// and ext4 block sizes never exceed it on these targets.
const MAX_BLOCK_SIZE: u32 = 4096;

/// The ext2/3/4 superblock begins at this fixed byte offset, regardless
/// of block size.
const SUPERBLOCK_OFFSET: u64 = 1024;

/// Encoded length of the fixed superblock fields the driver reads.
const SUPERBLOCK_LEN: usize = 1024;

/// On-disk superblock magic (`s_magic`), little-endian `0xEF53`.
const EXT_MAGIC: u16 = 0xEF53;

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

/// `i_flags`: the inode is mapped by an extent tree, not block pointers.
const INODE_FLAG_EXTENTS: u32 = 0x0008_0000;

/// Extent-tree node header magic (`eh_magic`), little-endian `0xF30A`.
const EXTENT_MAGIC: u16 = 0xF30A;

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

/// Narrow a `u64` to `u32`, mapping overflow to [`DriverError::DeviceFault`].
fn u32_of(value: u64) -> Result<u32, DriverError> {
    u32::try_from(value).map_err(|_| DriverError::DeviceFault)
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
    /// **mutate**. Writes refuse (`Unsupported`) when it is not — e.g.
    /// the `metadata_csum`, `gdt_csum`/`uninit_bg`, or `64bit` features
    /// require checksum or wide-descriptor maintenance the write path
    /// does not perform (`AGENTS.md` §5.4 — fail closed).
    write_safe: bool,
}

/// A decoded inode: only the structural fields the read and security
/// surfaces need.
struct Inode {
    /// `i_mode`, including the type bits.
    mode: u16,
    /// Owning user id (`i_uid` low half combined with the osd2 high half).
    uid: u32,
    /// Owning group id (`i_gid` low half combined with the osd2 high half).
    gid: u32,
    /// File length in bytes (low + high halves combined).
    size: u64,
    /// `i_flags`.
    flags: u32,
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

    /// The inode's base §5.3 security record (owner, group, mode bits),
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
    /// online read-only.
    ///
    /// # Errors
    ///
    /// * [`DriverError::DeviceFault`] if the device geometry is
    ///   degenerate or a block read fails.
    /// * [`DriverError::BadMagic`] if the superblock magic is wrong or
    ///   the geometry is structurally invalid.
    /// * [`DriverError::Unsupported`] if the volume requires a feature
    ///   this read-only driver does not implement.
    ///
    /// # Capabilities
    ///
    /// Caller must already hold the driver's [`DriverHandle`].
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
        let write_safe =
            !is_64bit && feature_ro_compat & (RO_COMPAT_METADATA_CSUM | RO_COMPAT_GDT_CSUM) == 0;
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
            },
        })
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
        let mut raw = [0u8; 128];
        device_read(
            &mut self.block,
            self.block_size,
            self.block_count,
            inode_offset,
            &mut raw,
        )?;

        let mode = le16(&raw, 0);
        let uid = u32::from(le16(&raw, 0x02)) | (u32::from(le16(&raw, 0x78)) << 16);
        let gid = u32::from(le16(&raw, 0x18)) | (u32::from(le16(&raw, 0x7A)) << 16);
        let size_lo = u64::from(le32(&raw, 0x04));
        let size_hi = u64::from(le32(&raw, 0x6C));
        let flags = le32(&raw, 0x20);
        let file_acl = u64::from(le32(&raw, 0x68)) | (u64::from(le16(&raw, 0x74)) << 32);
        let mut block = [0u8; I_BLOCK_LEN];
        block.copy_from_slice(&raw[I_BLOCK_OFFSET..I_BLOCK_OFFSET + I_BLOCK_LEN]);
        Ok(Inode {
            mode,
            uid,
            gid,
            size: (size_hi << 32) | size_lo,
            flags,
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
    /// The `n`-th real child, skipping `.` / `..` and unused slots.
    ByIndex(u64),
}

/// A directory entry located by [`Ext4::find_entry`].
struct FoundEntry {
    /// The child inode number.
    ino: u32,
    /// The child kind from the entry's `file_type` byte, or `None` when
    /// the volume lacks the `filetype` feature (the caller stats the
    /// child inode instead).
    kind: Option<NodeKind>,
    /// Number of name bytes written into the caller's output buffer.
    name_len: usize,
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
        let mut counter = 0u64;
        for logical in 0..total_blocks {
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
                        let entry_kind = if self.layout.filetype {
                            file_type_kind(block_buf[pos + 7])
                        } else {
                            None
                        };
                        match query {
                            DirQuery::ByName(target) => {
                                if name == target {
                                    return Ok(Some(FoundEntry {
                                        ino,
                                        kind: entry_kind,
                                        name_len: 0,
                                    }));
                                }
                            }
                            DirQuery::ByIndex(target) => {
                                if counter == target {
                                    if name_len > name_out.len() {
                                        return Err(DriverError::BufferTooSmall);
                                    }
                                    name_out[..name_len].copy_from_slice(name);
                                    return Ok(Some(FoundEntry {
                                        ino,
                                        kind: entry_kind,
                                        name_len,
                                    }));
                                }
                                counter += 1;
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

/// Map a directory-entry `file_type` byte to a [`NodeKind`].
fn file_type_kind(ft: u8) -> Option<NodeKind> {
    match ft {
        FT_REG => Some(NodeKind::RegularFile),
        FT_DIR => Some(NodeKind::Directory),
        _ => None,
    }
}

impl<B: Block> Ext4<B> {
    /// Refuse mutation of a volume whose feature set this driver cannot
    /// maintain (`AGENTS.md` §5.4 — fail closed).
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

    /// Write a `u32` superblock field at byte offset `field`.
    fn set_sb_u32(&mut self, field: u64, value: u32) -> Result<(), DriverError> {
        device_write(
            &mut self.block,
            self.block_size,
            self.block_count,
            SUPERBLOCK_OFFSET + field,
            &value.to_le_bytes(),
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

    /// Write group `group`'s descriptor back from a fixed buffer.
    fn write_group_desc(&mut self, group: u64, desc: &[u8; 64]) -> Result<(), DriverError> {
        let off = self.group_desc_offset(group)?;
        let len = self.layout.desc_size as usize;
        device_write(
            &mut self.block,
            self.block_size,
            self.block_count,
            off,
            &desc[..len],
        )
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

    /// Write inode `ino`'s full on-disk record from `raw[..inode_size]`.
    fn write_inode_raw(&mut self, ino: u32, raw: &[u8]) -> Result<(), DriverError> {
        let off = self.locate_inode(ino)?;
        let len = self.layout.inode_size as usize;
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
    /// count, and the superblock free count.
    fn alloc_block(&mut self) -> Result<u64, DriverError> {
        let bpg = u64::from(self.layout.blocks_per_group);
        let bs = self.layout.block_size as usize;
        for group in 0..self.layout.group_count {
            let mut desc = self.read_group_desc(group)?;
            let free = le16(&desc, 0x0C);
            if free == 0 {
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
                    self.write_group_desc(group, &desc)?;
                    let sb_free = self.sb_u32(0x0C)?;
                    self.set_sb_u32(0x0C, sb_free.saturating_sub(1))?;
                    let zero = [0u8; MAX_BLOCK_SIZE as usize];
                    self.write_fs_block(abs, &zero)?;
                    return Ok(abs);
                }
            }
        }
        Err(DriverError::DeviceFault)
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
            self.write_group_desc(group, &desc)?;
            let sb_free = self.sb_u32(0x0C)?;
            self.set_sb_u32(0x0C, sb_free + 1)?;
        }
        Ok(())
    }

    /// Allocate one free inode, returning its number. `is_dir` bumps the
    /// group's directory count. Updates the inode bitmap and free counts.
    fn alloc_inode(&mut self, is_dir: bool) -> Result<u32, DriverError> {
        let ipg = u64::from(self.layout.inodes_per_group);
        let total = u64::from(self.sb_u32(0x00)?);
        let bs = self.layout.block_size as usize;
        for group in 0..self.layout.group_count {
            let mut desc = self.read_group_desc(group)?;
            let free = le16(&desc, 0x0E);
            if free == 0 {
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
                    self.write_group_desc(group, &desc)?;
                    let sb_free = self.sb_u32(0x10)?;
                    self.set_sb_u32(0x10, sb_free.saturating_sub(1))?;
                    return u32::try_from(ino).map_err(|_| DriverError::DeviceFault);
                }
            }
        }
        Err(DriverError::DeviceFault)
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
            self.write_group_desc(group, &desc)?;
            let sb_free = self.sb_u32(0x10)?;
            self.set_sb_u32(0x10, sb_free + 1)?;
        }
        Ok(())
    }
}

impl<B: Block> Ext4<B> {
    /// Map logical block `logical` of the inode whose raw record is in
    /// `raw` to a physical block, allocating backing storage (and any
    /// indirect/extent metadata) when it is a hole. `allocated` is bumped
    /// by every filesystem block this call newly allocated so the caller
    /// can maintain `i_blocks`.
    fn map_or_alloc(
        &mut self,
        raw: &mut [u8],
        logical: u64,
        allocated: &mut u64,
    ) -> Result<u64, DriverError> {
        if le32(raw, 0x20) & INODE_FLAG_EXTENTS != 0 {
            self.map_or_alloc_extent(raw, logical, allocated)
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

    /// Inline-extent allocation: serves the depth-0 extent root held in
    /// `i_block`, extending the last extent when the new block is
    /// logically and physically contiguous, otherwise adding a fresh
    /// extent while the four inline slots last. An interior extent tree
    /// (non-zero depth) or a full root is not grown (`DeviceFault`).
    fn map_or_alloc_extent(
        &mut self,
        raw: &mut [u8],
        logical: u64,
        allocated: &mut u64,
    ) -> Result<u64, DriverError> {
        let ib = I_BLOCK_OFFSET;
        if le16(raw, ib) != EXTENT_MAGIC || le16(raw, ib + 6) != 0 {
            return Err(DriverError::DeviceFault);
        }
        let entries = usize::from(le16(raw, ib + 2));
        let max_entries = (I_BLOCK_LEN - 12) / 12;
        if entries > max_entries {
            return Err(DriverError::DeviceFault);
        }
        for i in 0..entries {
            let off = ib + 12 + i * 12;
            let ee_block = u64::from(le32(raw, off));
            let raw_len = le16(raw, off + 4);
            let len = if raw_len > 32_768 {
                u64::from(raw_len - 32_768)
            } else {
                u64::from(raw_len)
            };
            if logical >= ee_block && logical < ee_block + len {
                let phys = (u64::from(le16(raw, off + 6)) << 32) | u64::from(le32(raw, off + 8));
                return Ok(phys + (logical - ee_block));
            }
        }
        let blk = self.alloc_block()?;
        *allocated += 1;
        if entries > 0 {
            let off = ib + 12 + (entries - 1) * 12;
            let ee_block = u64::from(le32(raw, off));
            let raw_len = le16(raw, off + 4);
            let phys = (u64::from(le16(raw, off + 6)) << 32) | u64::from(le32(raw, off + 8));
            if raw_len < 32_768
                && logical == ee_block + u64::from(raw_len)
                && blk == phys + u64::from(raw_len)
            {
                put_le16(raw, off + 4, raw_len + 1);
                return Ok(blk);
            }
        }
        if entries < max_entries {
            let off = ib + 12 + entries * 12;
            put_le32(
                raw,
                off,
                u32::try_from(logical).map_err(|_| DriverError::DeviceFault)?,
            );
            put_le16(raw, off + 4, 1);
            put_le16(raw, off + 6, u16::try_from(blk >> 32).unwrap_or(0));
            put_le32(raw, off + 8, u32_of(blk)?);
            put_le16(raw, ib + 2, u16_of(entries + 1)?);
            return Ok(blk);
        }
        self.free_block(blk)?;
        Err(DriverError::DeviceFault)
    }
}

/// Round `n` up to the next multiple of four (the directory-entry
/// `rec_len` alignment).
fn align4(n: usize) -> usize {
    (n + 3) & !3
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
/// (`AGENTS.md` §5.4 — fail closed, never widen).
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
                    self.write_dirent(block, pos, slot_ino, u16_of(used)?, &[], 0)?;
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
        let bs = self.layout.block_size as usize;
        let dir = self.read_inode(dir_ino)?;
        let total_blocks = dir.size.div_ceil(u64::from(self.layout.block_size));
        let mut block_buf = [0u8; MAX_BLOCK_SIZE as usize];
        for logical in 0..total_blocks {
            let Some(phys) = self.map_block(&dir, logical)? else {
                continue;
            };
            self.read_fs_block(phys, &mut block_buf)?;
            if self.place_in_block(&mut block_buf[..bs], needed, child_ino, name, file_type)? {
                self.write_fs_block(phys, &block_buf)?;
                return Ok(());
            }
        }
        let mut raw = [0u8; MAX_BLOCK_SIZE as usize];
        self.read_inode_raw(dir_ino, &mut raw)?;
        let mut allocated = 0u64;
        let phys = self.map_or_alloc(&mut raw, total_blocks, &mut allocated)?;
        let mut new_block = [0u8; MAX_BLOCK_SIZE as usize];
        self.write_dirent(&mut new_block, 0, child_ino, u16_of(bs)?, name, file_type)?;
        self.write_fs_block(phys, &new_block)?;
        let new_size = (total_blocks + 1) * u64::from(self.layout.block_size);
        put_le32(&mut raw, 0x04, u32_of(new_size)?);
        let blocks = le32(&raw, 0x1C);
        put_le32(
            &mut raw,
            0x1C,
            blocks + u32_of(allocated)? * self.sectors_per_block(),
        );
        self.write_inode_raw(dir_ino, &raw)?;
        Ok(())
    }

    /// Remove the entry named `name` from directory inode `dir_ino`,
    /// returning the child inode number. The freed slot is merged into
    /// the preceding entry (or zeroed when it is first in its block).
    fn remove_dirent(&mut self, dir_ino: u32, name: &[u8]) -> Result<u32, DriverError> {
        let bs = self.layout.block_size as usize;
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
            while pos + DIRENT_HEADER <= bs {
                let (slot_ino, rec_len, name_len) = self.read_dirent_header(&block_buf, pos, bs)?;
                if slot_ino != 0 && name_len > 0 && DIRENT_HEADER + name_len <= rec_len {
                    let slot_name = &block_buf[pos + DIRENT_HEADER..pos + DIRENT_HEADER + name_len];
                    if slot_name == name {
                        match prev {
                            Some(pp) => {
                                let (_, prev_rec, _) =
                                    self.read_dirent_header(&block_buf, pp, bs)?;
                                put_le16(&mut block_buf, pp + 4, u16_of(prev_rec + rec_len)?);
                            }
                            None => put_le32(&mut block_buf, pos, 0),
                        }
                        self.write_fs_block(phys, &block_buf)?;
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
        let bs = self.layout.block_size as usize;
        let dir = self.read_inode(dir_ino)?;
        let total_blocks = dir.size.div_ceil(u64::from(self.layout.block_size));
        let mut block_buf = [0u8; MAX_BLOCK_SIZE as usize];
        for logical in 0..total_blocks {
            let Some(phys) = self.map_block(&dir, logical)? else {
                continue;
            };
            self.read_fs_block(phys, &mut block_buf)?;
            let mut pos = 0usize;
            while pos + DIRENT_HEADER <= bs {
                let (slot_ino, rec_len, name_len) = self.read_dirent_header(&block_buf, pos, bs)?;
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
}

impl<B: Block> FilesystemRead for Ext4<B> {
    fn root(&self) -> NodeId {
        NodeId::from_raw(u64::from(ROOT_INODE))
    }

    fn node_info(&mut self, node: NodeId) -> Result<NodeInfo, DriverError> {
        let ino = node_inode(node)?;
        let inode = self.read_inode(ino)?;
        let kind = inode.kind().ok_or(DriverError::NotFound)?;
        let size = match kind {
            NodeKind::Directory => 0,
            NodeKind::RegularFile => inode.size,
        };
        Ok(NodeInfo { kind, size })
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
        index: u64,
        name_out: &mut [u8],
    ) -> Result<Option<DirEntry>, DriverError> {
        let ino = node_inode(dir)?;
        let inode = self.read_inode(ino)?;
        if inode.kind() != Some(NodeKind::Directory) {
            return Err(DriverError::Unsupported);
        }
        let Some(found) = self.find_entry(&inode, DirQuery::ByIndex(index), name_out)? else {
            return Ok(None);
        };
        let kind = match found.kind {
            Some(kind) => kind,
            None => self
                .read_inode(found.ino)?
                .kind()
                .ok_or(DriverError::DeviceFault)?,
        };
        Ok(Some(DirEntry {
            node: NodeId::from_raw(u64::from(found.ino)),
            kind,
            name_len: found.name_len,
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
    fn truncate_blocks(&mut self, raw: &mut [u8], keep: u64) -> Result<u64, DriverError> {
        if le32(raw, 0x20) & INODE_FLAG_EXTENTS != 0 {
            self.truncate_extent_blocks(raw, keep)
        } else {
            self.truncate_classic_blocks(raw, keep)
        }
    }

    /// [`Self::truncate_blocks`] for the inline depth-0 extent root.
    fn truncate_extent_blocks(&mut self, raw: &mut [u8], keep: u64) -> Result<u64, DriverError> {
        let ib = I_BLOCK_OFFSET;
        if le16(raw, ib) != EXTENT_MAGIC || le16(raw, ib + 6) != 0 {
            return Err(DriverError::Unsupported);
        }
        let entries = usize::from(le16(raw, ib + 2));
        let max_entries = (I_BLOCK_LEN - 12) / 12;
        if entries > max_entries {
            return Err(DriverError::Unsupported);
        }
        let mut freed = 0u64;
        let mut kept = 0usize;
        for i in 0..entries {
            let off = ib + 12 + i * 12;
            let ee_block = u64::from(le32(raw, off));
            let raw_len = le16(raw, off + 4);
            let len = if raw_len > 32_768 {
                u64::from(raw_len - 32_768)
            } else {
                u64::from(raw_len)
            };
            let phys = (u64::from(le16(raw, off + 6)) << 32) | u64::from(le32(raw, off + 8));
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
                let dst = ib + 12 + kept * 12;
                put_le32(raw, dst, u32_of(ee_block)?);
                put_le16(raw, dst + 4, keep_len);
                put_le16(raw, dst + 6, u16::try_from(phys >> 32).unwrap_or(0));
                put_le32(raw, dst + 8, u32_of(phys)?);
                kept += 1;
            }
        }
        for i in kept..entries {
            let off = ib + 12 + i * 12;
            for b in &mut raw[off..off + 12] {
                *b = 0;
            }
        }
        put_le16(raw, ib + 2, u16_of(kept)?);
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
            let mut dir_block = [0u8; MAX_BLOCK_SIZE as usize];
            self.write_dirent(&mut dir_block, 0, new_ino, 12, b".", FT_DIR)?;
            self.write_dirent(&mut dir_block, 12, dir_ino, u16_of(bs - 12)?, b"..", FT_DIR)?;
            self.write_fs_block(blk, &dir_block)?;
            put_le32(&mut raw, I_BLOCK_OFFSET, u32_of(blk)?);
            put_le32(&mut raw, 0x04, u32_of(bs as u64)?);
            put_le32(&mut raw, 0x1C, self.sectors_per_block());
        } else {
            put_le16(&mut raw, 0, NEW_FILE_MODE);
            put_le16(&mut raw, 0x1A, 1);
        }
        self.write_inode_raw(new_ino, &raw)?;

        let file_type = if is_dir { FT_DIR } else { FT_REG };
        self.insert_dirent(dir_ino, name, new_ino, file_type)?;
        if is_dir {
            let mut draw = [0u8; MAX_BLOCK_SIZE as usize];
            self.read_inode_raw(dir_ino, &mut draw)?;
            let links = le16(&draw, 0x1A);
            put_le16(&mut draw, 0x1A, links + 1);
            self.write_inode_raw(dir_ino, &draw)?;
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
            let phys = self.map_or_alloc(&mut raw, logical, &mut allocated)?;
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
        self.write_inode_raw(child, &raw)?;
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
            let freed = self.truncate_blocks(&mut raw, keep)?;
            let blocks = le32(&raw, 0x1C);
            let dec = u32_of(freed)? * self.sectors_per_block();
            put_le32(&mut raw, 0x1C, blocks.saturating_sub(dec));
        }
        put_le32(&mut raw, 0x04, u32_of(size)?);
        put_le32(&mut raw, 0x6C, u32_of(size >> 32)?);
        self.write_inode_raw(child, &raw)?;

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
        self.truncate_blocks(&mut raw, 0)?;
        self.remove_dirent(dir_ino, name)?;
        self.free_inode(child, is_dir)?;
        if is_dir {
            let mut draw = [0u8; MAX_BLOCK_SIZE as usize];
            self.read_inode_raw(dir_ino, &mut draw)?;
            let links = le16(&draw, 0x1A);
            put_le16(&mut draw, 0x1A, links.saturating_sub(1));
            self.write_inode_raw(dir_ino, &draw)?;
        }
        Ok(())
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

#[cfg(test)]
mod tests;
