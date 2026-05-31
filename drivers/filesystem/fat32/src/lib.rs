//! RustOS FAT32 filesystem driver (read-only).
//!
//! Reads a FAT32 volume sitting behind any
//! [`rustos_abi::driver::block::Block`] device and exposes it through
//! the versioned [`rustos_abi::driver::filesystem::FilesystemRead`]
//! surface (`AGENTS.md` §2.4 / §9 — new behaviour ships as a new
//! trait, never by widening the frozen mount/unmount
//! [`Filesystem`](rustos_abi::driver::filesystem::Filesystem)).
//!
//! FAT32 has no per-inode owner, mode, ACL, or capability gate; those
//! live in the VFS metadata layer (`AGENTS.md` §5.3) that mounts this
//! driver. The driver therefore makes **no** permission decisions
//! (§5.4 — the VFS is the policy point, this is raw structural I/O).
//!
//! # Public surface
//!
//! Per `AGENTS.md` §8 the only public *function* is [`register`].
//! [`Fat32`] is a public *type* the driver host instantiates with
//! [`Fat32::open`]; the host reaches into it only through the
//! [`FilesystemRead`] trait.
//!
//! # Scope
//!
//! Read-only. Long file names (VFAT) are reconstructed: each entry
//! exposes a single name — its long name when a valid, checksum-
//! matching long-name set precedes the 8.3 short entry, and otherwise
//! the short name (so a volume written without long names is still
//! fully readable). When a long name is present the internal 8.3 alias
//! is *not* separately resolvable; the long name is the entry's name.
//! Names are returned as UTF-8 — UTF-16LE long names are decoded, and
//! the driver falls back to the short name on any malformed set rather
//! than surfacing a partial name. Writing is out of scope; a future
//! `FilesystemWrite` trait is tracked in `PLAN.md`.
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
const REGISTER_HANDLE_MARKER: u64 = 0x4641_5433_3200_0001; // "FAT32" + index

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
/// on-stack scratch buffer. FAT volumes never use a sector larger than
/// 4096 bytes, and no Tier-1 block device exceeds it either.
const MAX_BLOCK_SIZE: u32 = 4096;

/// On-disk directory-entry size, frozen by the FAT specification.
const DIR_ENTRY_LEN: usize = 32;

/// Attribute byte: the entry describes a subdirectory.
const ATTR_DIRECTORY: u8 = 0x10;
/// Attribute byte: the entry is the volume label, not a file.
const ATTR_VOLUME_ID: u8 = 0x08;
/// Attribute byte value marking a long-file-name fragment (skipped).
const ATTR_LONG_NAME: u8 = 0x0F;

/// First name byte marking the end of the directory.
const END_OF_DIR: u8 = 0x00;
/// First name byte marking a deleted (free) directory entry.
const DELETED_ENTRY: u8 = 0xE5;
/// First name byte `0x05` stands in for a leading `0xE5` (Kanji).
const KANJI_LEAD: u8 = 0x05;

/// 28-bit mask applied to raw FAT32 cluster values.
const FAT32_CLUSTER_MASK: u32 = 0x0FFF_FFFF;
/// Smallest end-of-chain marker; `value >= EOC` terminates a chain.
const FAT32_EOC: u32 = 0x0FFF_FFF8;
/// The single "bad cluster" sentinel.
const FAT32_BAD: u32 = 0x0FFF_FFF7;

/// Maximum number of UTF-16 code units in a long file name, frozen by
/// the VFAT specification.
const MAX_LONG_NAME_UNITS: usize = 255;

/// Number of UTF-16 code-unit slots a single long-name entry carries
/// (5 + 6 + 2).
const LFN_UNITS_PER_ENTRY: usize = 13;

/// Maximum number of long-name entries in a single set. A 255-unit name
/// needs 20 entries (the last is partially filled); a higher sequence
/// number is malformed.
const LFN_MAX_FRAGMENTS: usize = 20;

/// Number of UTF-16 code-unit slots reserved while reassembling a set,
/// covering the partially-filled final fragment.
const LFN_BUFFER_UNITS: usize = LFN_MAX_FRAGMENTS * LFN_UNITS_PER_ENTRY;

/// Maximum number of UTF-8 bytes a reconstructed long name can occupy:
/// every code unit decodes to at most 3 UTF-8 bytes (a surrogate pair
/// spends two units on a single 4-byte sequence, which is fewer bytes
/// per unit, so this bound holds).
const MAX_NAME_BYTES: usize = MAX_LONG_NAME_UNITS * 3;

/// `order` byte bit marking the last logical (first physical) long-name
/// entry of a set.
const LFN_LAST_FLAG: u8 = 0x40;

/// Mask isolating the 1-based sequence number from a long-name `order`
/// byte.
const LFN_SEQUENCE_MASK: u8 = 0x1F;

/// Byte offsets within a long-name entry holding UTF-16 code units.
const LFN_CHAR_OFFSETS: [usize; LFN_UNITS_PER_ENTRY] =
    [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];

/// `NodeId` bit carrying the directory flag (cluster numbers are 28-bit,
/// so bit 28 is free).
const NODE_DIR_FLAG: u64 = 1 << 28;
/// `NodeId` validity bit, set on every live node so that no live node
/// ever equals [`NodeId::NONE`] (`0`) — an empty file has cluster 0.
const NODE_VALID_FLAG: u64 = 1 << 29;
/// Bit position at which a regular file's size is packed into a
/// `NodeId`.
const NODE_SIZE_SHIFT: u64 = 32;

/// Pack a directory entry's identity into a self-describing [`NodeId`].
fn pack_node(cluster: u32, is_dir: bool, size: u32) -> NodeId {
    let mut raw = u64::from(cluster & FAT32_CLUSTER_MASK) | NODE_VALID_FLAG;
    if is_dir {
        raw |= NODE_DIR_FLAG;
    } else {
        raw |= u64::from(size) << NODE_SIZE_SHIFT;
    }
    NodeId::from_raw(raw)
}

/// First cluster encoded in a [`NodeId`].
fn node_cluster(node: NodeId) -> u32 {
    // The masked value spans at most 28 bits, so it always fits in `u32`.
    u32::try_from(node.raw() & u64::from(FAT32_CLUSTER_MASK)).unwrap_or(0)
}

/// Whether a [`NodeId`] denotes a directory.
fn node_is_dir(node: NodeId) -> bool {
    node.raw() & NODE_DIR_FLAG != 0
}

/// File size encoded in a [`NodeId`] (`0` for directories).
fn node_size(node: NodeId) -> u32 {
    // The high 32 bits of a `u64` always fit in `u32`.
    u32::try_from(node.raw() >> NODE_SIZE_SHIFT).unwrap_or(0)
}

/// Result of following one FAT chain link.
enum ChainStep {
    /// The chain continues at this cluster.
    Next(u32),
    /// The cluster was the last in its chain.
    End,
    /// The link is the reserved "bad cluster" value or otherwise
    /// structurally invalid.
    Bad,
}

/// Classify a raw FAT32 table value as a chain step.
fn classify_chain(value: u32) -> ChainStep {
    let masked = value & FAT32_CLUSTER_MASK;
    if masked >= FAT32_EOC {
        ChainStep::End
    } else if masked == FAT32_BAD || masked < 2 {
        ChainStep::Bad
    } else {
        ChainStep::Next(masked)
    }
}

/// Computed geometry of a validated FAT32 volume, in bytes.
struct Layout {
    bytes_per_cluster: u64,
    fat_start_byte: u64,
    data_start_byte: u64,
    root_cluster: u32,
}

/// A single decoded directory entry. `name` holds the file name as
/// UTF-8 bytes — the reconstructed long name when one is present and
/// valid, otherwise the 8.3 short name.
struct ParsedEntry {
    name: [u8; MAX_NAME_BYTES],
    name_len: usize,
    cluster: u32,
    size: u32,
    is_dir: bool,
}

/// Cursor walking a directory's cluster chain, 32 bytes at a time.
struct DirCursor {
    cluster: u32,
    intra: u64,
}

/// A read-only FAT32 volume backed by a [`Block`] device.
pub struct Fat32<B: Block> {
    block: B,
    block_size: u32,
    block_count: u64,
    layout: Layout,
}

/// Read `u16` little-endian from `buf` at `offset`.
fn le16(buf: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([buf[offset], buf[offset + 1]])
}

/// Read `u32` little-endian from `buf` at `offset`.
fn le32(buf: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
    ])
}

/// Number of leading bytes of `field` that are not the ASCII padding
/// space `0x20`, counting from the end.
fn trimmed_len(field: &[u8]) -> usize {
    let mut len = field.len();
    while len > 0 && field[len - 1] == b' ' {
        len -= 1;
    }
    len
}

/// Decode a 32-byte short-name directory entry.
fn parse_short_entry(raw: &[u8; DIR_ENTRY_LEN]) -> ParsedEntry {
    let mut name = [0u8; MAX_NAME_BYTES];
    let mut len = 0;

    let base_len = trimmed_len(&raw[0..8]);
    for (i, &raw_byte) in raw[..8].iter().enumerate().take(base_len) {
        let byte = if i == 0 && raw_byte == KANJI_LEAD {
            DELETED_ENTRY
        } else {
            raw_byte
        };
        name[len] = byte;
        len += 1;
    }

    let ext_len = trimmed_len(&raw[8..11]);
    if ext_len > 0 {
        name[len] = b'.';
        len += 1;
        for &raw_byte in &raw[8..8 + ext_len] {
            name[len] = raw_byte;
            len += 1;
        }
    }

    let cluster = (u32::from(le16(raw, 20)) << 16) | u32::from(le16(raw, 26));
    ParsedEntry {
        name,
        name_len: len,
        cluster,
        size: le32(raw, 28),
        is_dir: raw[11] & ATTR_DIRECTORY != 0,
    }
}

/// VFAT short-name checksum binding a long-name set to its 8.3 entry.
///
/// Computed over the raw 11-byte on-disk short-name field (base +
/// extension, space-padded), exactly as the bytes are stored — the
/// `0x05` Kanji-lead substitution is *not* undone here, because the
/// generating implementation checksums the stored bytes.
fn short_name_checksum(short: &[u8; 11]) -> u8 {
    let mut sum = 0u8;
    for &byte in short {
        sum = sum.rotate_right(1).wrapping_add(byte);
    }
    sum
}

/// Decode UTF-16LE code `units` into UTF-8 `out`, stopping at the first
/// `0x0000` terminator.
///
/// Returns the number of bytes written, or `None` if the units contain
/// an unpaired surrogate, an invalid scalar value, or would overflow
/// `out` (callers fall back to the 8.3 short name on `None`).
fn decode_utf16le(units: &[u16], out: &mut [u8]) -> Option<usize> {
    const HIGH_SURROGATES: core::ops::RangeInclusive<u16> = 0xD800..=0xDBFF;
    const LOW_SURROGATES: core::ops::RangeInclusive<u16> = 0xDC00..=0xDFFF;
    const SURROGATE_BASE: u32 = 0x1_0000;

    let mut written: usize = 0;
    let mut index: usize = 0;
    while index < units.len() {
        let unit = units[index];
        if unit == 0 {
            break;
        }
        let scalar = if HIGH_SURROGATES.contains(&unit) {
            index += 1;
            let low = *units.get(index)?;
            if !LOW_SURROGATES.contains(&low) {
                return None;
            }
            SURROGATE_BASE
                + ((u32::from(unit - *HIGH_SURROGATES.start()) << 10)
                    | u32::from(low - *LOW_SURROGATES.start()))
        } else if LOW_SURROGATES.contains(&unit) {
            return None;
        } else {
            u32::from(unit)
        };
        let decoded = char::from_u32(scalar)?;
        let mut scratch = [0u8; 4];
        let encoded = decoded.encode_utf8(&mut scratch);
        let end = written.checked_add(encoded.len())?;
        if end > out.len() {
            return None;
        }
        out[written..end].copy_from_slice(encoded.as_bytes());
        written = end;
        index += 1;
    }
    Some(written)
}

/// Reassembles a VFAT long-name set from its physical directory
/// entries, which precede the short entry in reverse sequence order
/// (the entry flagged [`LFN_LAST_FLAG`] appears first).
struct LongName {
    units: [u16; LFN_BUFFER_UNITS],
    total_units: usize,
    next_sequence: u8,
    checksum: u8,
    started: bool,
    valid: bool,
}

impl LongName {
    fn new() -> Self {
        Self {
            units: [0u16; LFN_BUFFER_UNITS],
            total_units: 0,
            next_sequence: 0,
            checksum: 0,
            started: false,
            valid: false,
        }
    }

    fn reset(&mut self) {
        self.started = false;
        self.valid = false;
        self.total_units = 0;
        self.next_sequence = 0;
    }

    /// Absorb one long-name directory entry.
    fn push(&mut self, raw: &[u8; DIR_ENTRY_LEN]) {
        let order = raw[0];
        if order == DELETED_ENTRY {
            self.reset();
            return;
        }
        let sequence = order & LFN_SEQUENCE_MASK;
        let is_last = order & LFN_LAST_FLAG != 0;
        if sequence == 0 || usize::from(sequence) > LFN_MAX_FRAGMENTS {
            self.reset();
            return;
        }

        if is_last {
            self.units = [0u16; LFN_BUFFER_UNITS];
            self.total_units = usize::from(sequence) * LFN_UNITS_PER_ENTRY;
            self.checksum = raw[13];
            self.next_sequence = sequence;
            self.started = true;
            self.valid = true;
        } else if !self.started
            || !self.valid
            || sequence != self.next_sequence
            || raw[13] != self.checksum
        {
            self.valid = false;
            return;
        }

        let base = (usize::from(sequence) - 1) * LFN_UNITS_PER_ENTRY;
        for (slot, &offset) in LFN_CHAR_OFFSETS.iter().enumerate() {
            self.units[base + slot] = le16(raw, offset);
        }
        self.next_sequence = sequence - 1;
    }

    /// Reconstruct the name into `out` if a complete, checksum-matching
    /// set was accumulated for the short entry `short`.
    fn finish(&self, short: &[u8; 11], out: &mut [u8]) -> Option<usize> {
        if !self.started || !self.valid || self.next_sequence != 0 {
            return None;
        }
        if self.checksum != short_name_checksum(short) {
            return None;
        }
        let len = decode_utf16le(&self.units[..self.total_units], out)?;
        if len == 0 {
            return None;
        }
        Some(len)
    }
}

impl<B: Block> Fat32<B> {
    /// Validate the FAT32 boot sector on `block` and bring the volume
    /// online read-only.
    ///
    /// FAT type is identified by the FAT32 boot-sector shape — a zero
    /// 16-bit FAT size and a zero root-entry count — rather than by
    /// re-deriving the cluster-count threshold: a FAT12/FAT16 volume
    /// has non-zero values in those fields and is rejected, so the
    /// distinction is exact for the volumes this driver accepts.
    ///
    /// # Errors
    ///
    /// * [`DriverError::DeviceFault`] if the device geometry is
    ///   degenerate or a block read fails.
    /// * [`DriverError::BadMagic`] if the boot sector is not a valid
    ///   FAT32 BPB (bad signature, non-power-of-two sector/cluster
    ///   size, missing FAT, or non-FAT32 markers).
    ///
    /// # Capabilities
    ///
    /// Caller must already hold the driver's [`DriverHandle`].
    pub fn open(mut block: B) -> Result<Self, DriverError> {
        let geometry = block.geometry()?;
        let block_size = geometry.block_size;
        if block_size == 0 || block_size > MAX_BLOCK_SIZE || !block_size.is_power_of_two() {
            return Err(DriverError::DeviceFault);
        }
        let block_count = geometry.block_count;
        let total_bytes = u64::from(block_size)
            .checked_mul(block_count)
            .ok_or(DriverError::DeviceFault)?;

        let mut boot = [0u8; 512];
        device_read(&mut block, block_size, block_count, 0, &mut boot)?;

        if le16(&boot, 510) != 0xAA55 {
            return Err(DriverError::BadMagic);
        }
        let bytes_per_sector = u32::from(le16(&boot, 11));
        let sectors_per_cluster = u32::from(boot[13]);
        let reserved_sectors = u64::from(le16(&boot, 14));
        let num_fats = u64::from(boot[16]);
        let root_entry_count = le16(&boot, 17);
        let fat_size_16 = le16(&boot, 22);
        let fat_size_32 = u64::from(le32(&boot, 36));
        let root_cluster = le32(&boot, 44);

        let sector_ok = (512..=MAX_BLOCK_SIZE).contains(&bytes_per_sector)
            && bytes_per_sector.is_power_of_two();
        let cluster_ok =
            (1..=128).contains(&sectors_per_cluster) && sectors_per_cluster.is_power_of_two();
        let fat32_markers = root_entry_count == 0 && fat_size_16 == 0;
        if !sector_ok
            || !cluster_ok
            || num_fats < 1
            || !fat32_markers
            || fat_size_32 == 0
            || root_cluster < 2
        {
            return Err(DriverError::BadMagic);
        }

        let fat_start_byte = reserved_sectors
            .checked_mul(u64::from(bytes_per_sector))
            .ok_or(DriverError::BadMagic)?;
        let data_start_sectors = reserved_sectors
            .checked_add(
                num_fats
                    .checked_mul(fat_size_32)
                    .ok_or(DriverError::BadMagic)?,
            )
            .ok_or(DriverError::BadMagic)?;
        let data_start_byte = data_start_sectors
            .checked_mul(u64::from(bytes_per_sector))
            .ok_or(DriverError::BadMagic)?;
        let bytes_per_cluster = u64::from(sectors_per_cluster) * u64::from(bytes_per_sector);
        if data_start_byte >= total_bytes {
            return Err(DriverError::BadMagic);
        }

        Ok(Self {
            block,
            block_size,
            block_count,
            layout: Layout {
                bytes_per_cluster,
                fat_start_byte,
                data_start_byte,
                root_cluster,
            },
        })
    }

    /// Consume the driver, returning the underlying block device.
    pub fn into_block(self) -> B {
        self.block
    }

    /// Read an arbitrary byte range from the backing device.
    fn read_bytes(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        device_read(
            &mut self.block,
            self.block_size,
            self.block_count,
            offset,
            buf,
        )
    }

    /// Byte offset of cluster `cluster`'s data on the device.
    fn cluster_byte(&self, cluster: u32) -> u64 {
        self.layout.data_start_byte + (u64::from(cluster) - 2) * self.layout.bytes_per_cluster
    }

    /// Follow one link of the FAT chain starting at `cluster`.
    fn next_cluster(&mut self, cluster: u32) -> Result<ChainStep, DriverError> {
        let offset = self.layout.fat_start_byte + u64::from(cluster) * 4;
        let mut raw = [0u8; 4];
        self.read_bytes(offset, &mut raw)?;
        Ok(classify_chain(u32::from_le_bytes(raw)))
    }

    /// Return the next valid entry at or after `cursor`, advancing the
    /// cursor past it. `Ok(None)` marks end-of-directory.
    ///
    /// The entry's name is the reconstructed VFAT long name when a
    /// valid, checksum-matching long-name set precedes the short entry,
    /// and otherwise the 8.3 short name. Deleted entries, orphaned
    /// long-name fragments, volume labels, and the `.`/`..` self/parent
    /// links are skipped (the VFS resolves `.`/`..` itself,
    /// `AGENTS.md` §16).
    fn next_entry(&mut self, cursor: &mut DirCursor) -> Result<Option<ParsedEntry>, DriverError> {
        let mut long = LongName::new();
        loop {
            if cursor.cluster < 2 {
                return Ok(None);
            }
            if cursor.intra >= self.layout.bytes_per_cluster {
                match self.next_cluster(cursor.cluster)? {
                    ChainStep::Next(next) => {
                        cursor.cluster = next;
                        cursor.intra = 0;
                    }
                    ChainStep::End => return Ok(None),
                    ChainStep::Bad => return Err(DriverError::DeviceFault),
                }
                continue;
            }

            let entry_byte = self.cluster_byte(cursor.cluster) + cursor.intra;
            let mut raw = [0u8; DIR_ENTRY_LEN];
            self.read_bytes(entry_byte, &mut raw)?;
            cursor.intra += DIR_ENTRY_LEN as u64;

            let first = raw[0];
            if first == END_OF_DIR {
                return Ok(None);
            }
            if first == DELETED_ENTRY {
                long.reset();
                continue;
            }
            let attr = raw[11];
            if attr == ATTR_LONG_NAME {
                long.push(&raw);
                continue;
            }
            if attr & ATTR_VOLUME_ID != 0 || first == b'.' {
                long.reset();
                continue;
            }

            let mut entry = parse_short_entry(&raw);
            let mut short = [0u8; 11];
            short.copy_from_slice(&raw[0..11]);
            if let Some(long_len) = long.finish(&short, &mut entry.name) {
                entry.name_len = long_len;
            }
            return Ok(Some(entry));
        }
    }

    /// Build the [`NodeId`] for a decoded directory entry.
    fn entry_node(entry: &ParsedEntry) -> NodeId {
        let size = if entry.is_dir { 0 } else { entry.size };
        pack_node(entry.cluster, entry.is_dir, size)
    }
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

impl<B: Block> FilesystemRead for Fat32<B> {
    fn root(&self) -> NodeId {
        pack_node(self.layout.root_cluster, true, 0)
    }

    fn node_info(&mut self, node: NodeId) -> Result<NodeInfo, DriverError> {
        if node_is_dir(node) {
            Ok(NodeInfo {
                kind: NodeKind::Directory,
                size: 0,
            })
        } else {
            Ok(NodeInfo {
                kind: NodeKind::RegularFile,
                size: u64::from(node_size(node)),
            })
        }
    }

    fn lookup(&mut self, dir: NodeId, name: &[u8]) -> Result<NodeId, DriverError> {
        if !node_is_dir(dir) {
            return Err(DriverError::Unsupported);
        }
        if name.is_empty() || name.len() > MAX_NAME_BYTES {
            return Err(DriverError::NotFound);
        }
        let mut cursor = DirCursor {
            cluster: node_cluster(dir),
            intra: 0,
        };
        while let Some(entry) = self.next_entry(&mut cursor)? {
            if entry.name[..entry.name_len].eq_ignore_ascii_case(name) {
                return Ok(Self::entry_node(&entry));
            }
        }
        Err(DriverError::NotFound)
    }

    fn read_at(&mut self, file: NodeId, offset: u64, buf: &mut [u8]) -> Result<usize, DriverError> {
        if node_is_dir(file) {
            return Err(DriverError::Unsupported);
        }
        let size = u64::from(node_size(file));
        if buf.is_empty() || offset >= size {
            return Ok(0);
        }
        let want = core::cmp::min(buf.len() as u64, size - offset);
        let want = usize::try_from(want).map_err(|_| DriverError::LengthOutOfRange)?;

        let first = node_cluster(file);
        if first < 2 {
            return Err(DriverError::DeviceFault);
        }
        let bytes_per_cluster = self.layout.bytes_per_cluster;

        let mut cluster = first;
        let mut to_skip = offset / bytes_per_cluster;
        while to_skip > 0 {
            match self.next_cluster(cluster)? {
                ChainStep::Next(next) => cluster = next,
                _ => return Err(DriverError::DeviceFault),
            }
            to_skip -= 1;
        }

        let mut intra = usize::try_from(offset % bytes_per_cluster)
            .map_err(|_| DriverError::LengthOutOfRange)?;
        let cluster_len =
            usize::try_from(bytes_per_cluster).map_err(|_| DriverError::LengthOutOfRange)?;
        let mut produced = 0;
        while produced < want {
            if cluster < 2 {
                return Err(DriverError::DeviceFault);
            }
            let take = core::cmp::min(cluster_len - intra, want - produced);
            let start = self.cluster_byte(cluster) + intra as u64;
            self.read_bytes(start, &mut buf[produced..produced + take])?;
            produced += take;
            intra = 0;
            if produced < want {
                match self.next_cluster(cluster)? {
                    ChainStep::Next(next) => cluster = next,
                    ChainStep::End => break,
                    ChainStep::Bad => return Err(DriverError::DeviceFault),
                }
            }
        }
        Ok(produced)
    }

    fn read_dir(
        &mut self,
        dir: NodeId,
        index: u64,
        name_out: &mut [u8],
    ) -> Result<Option<DirEntry>, DriverError> {
        if !node_is_dir(dir) {
            return Err(DriverError::Unsupported);
        }
        let mut cursor = DirCursor {
            cluster: node_cluster(dir),
            intra: 0,
        };
        let mut position = 0u64;
        while let Some(entry) = self.next_entry(&mut cursor)? {
            if position == index {
                if name_out.len() < entry.name_len {
                    return Err(DriverError::BufferTooSmall);
                }
                name_out[..entry.name_len].copy_from_slice(&entry.name[..entry.name_len]);
                let kind = if entry.is_dir {
                    NodeKind::Directory
                } else {
                    NodeKind::RegularFile
                };
                return Ok(Some(DirEntry {
                    node: Self::entry_node(&entry),
                    kind,
                    name_len: entry.name_len,
                }));
            }
            position += 1;
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests;
