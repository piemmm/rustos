//! RustOS ext4 filesystem driver (read-only).
//!
//! Reads an ext2/ext3/ext4 volume sitting behind any
//! [`rustos_abi::driver::block::Block`] device and exposes it through
//! the versioned [`rustos_abi::driver::filesystem::FilesystemRead`]
//! surface (`AGENTS.md` §2.4 / §9 — new behaviour ships as a new trait,
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
//! [`FilesystemRead`] trait.
//!
//! # Scope
//!
//! Read-only. Supports the modern ext4 on-disk shape — block sizes
//! 1024..=4096, 128- or 256-byte inodes, 32- or 64-byte group
//! descriptors (the `64bit` feature), extent-mapped inodes (the default
//! since ext4) including multi-level extent trees, and the classic
//! ext2/ext3 indirect block map (direct + single/double/triple
//! indirect) — and linear (non-hash-indexed leaf) directory blocks. The
//! root block of a hash-indexed (`htree`) directory is read through its
//! linear `.`/`..` view; deeply indexed interior directory nodes are not
//! traversed. A [`NodeId`] is the on-disk inode number, so there is no
//! in-memory inode table. No `unwrap`/`expect`/`panic!` and no `unsafe`.
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
use rustos_abi::driver::filesystem::{DirEntry, FilesystemRead, NodeId, NodeInfo, NodeKind};
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
}

/// A decoded inode: only the structural fields the read surface needs.
struct Inode {
    /// `i_mode`, including the type bits.
    mode: u16,
    /// File length in bytes (low + high halves combined).
    size: u64,
    /// `i_flags`.
    flags: u32,
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

    /// Read and decode inode number `ino`.
    fn read_inode(&mut self, ino: u32) -> Result<Inode, DriverError> {
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

        let inode_offset = inode_table_block
            .checked_mul(u64::from(self.layout.block_size))
            .and_then(|base| base.checked_add(index * u64::from(self.layout.inode_size)))
            .ok_or(DriverError::DeviceFault)?;
        let mut raw = [0u8; 128];
        device_read(
            &mut self.block,
            self.block_size,
            self.block_count,
            inode_offset,
            &mut raw,
        )?;

        let mode = le16(&raw, 0);
        let size_lo = u64::from(le32(&raw, 0x04));
        let size_hi = u64::from(le32(&raw, 0x6C));
        let flags = le32(&raw, 0x20);
        let mut block = [0u8; I_BLOCK_LEN];
        block.copy_from_slice(&raw[I_BLOCK_OFFSET..I_BLOCK_OFFSET + I_BLOCK_LEN]);
        Ok(Inode {
            mode,
            size: (size_hi << 32) | size_lo,
            flags,
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

/// Decode a [`NodeId`] back into an on-disk inode number.
fn node_inode(node: NodeId) -> Result<u32, DriverError> {
    if node == NodeId::NONE {
        return Err(DriverError::NotFound);
    }
    u32::try_from(node.raw()).map_err(|_| DriverError::NotFound)
}

#[cfg(test)]
mod tests;
