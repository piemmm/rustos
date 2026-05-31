//! RustOS FAT32 filesystem driver (read/write).
//!
//! Reads and writes a FAT32 volume sitting behind any
//! [`rustos_abi::driver::block::Block`] device and exposes it through
//! the versioned [`rustos_abi::driver::filesystem::FilesystemRead`] and
//! [`rustos_abi::driver::filesystem::FilesystemWrite`] surfaces
//! (`AGENTS.md` §2.4 / §9 — new behaviour ships as a new trait, never by
//! widening the frozen mount/unmount
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
//! [`FilesystemRead`] and [`FilesystemWrite`] traits.
//!
//! # Scope
//!
//! Read and write. Long file names (VFAT) are reconstructed on read:
//! each entry exposes a single name — its long name when a valid,
//! checksum-matching long-name set precedes the 8.3 short entry, and
//! otherwise the short name (so a volume written without long names is
//! still fully readable). When a long name is present the internal 8.3
//! alias is *not* separately resolvable; the long name is the entry's
//! name. Names are returned as UTF-8 — UTF-16LE long names are decoded,
//! and the driver falls back to the short name on any malformed set
//! rather than surfacing a partial name.
//!
//! Writing creates files and directories, extends/overwrites file data
//! (allocating and chaining clusters, zero-filling sparse gaps),
//! truncates (shrinking frees the tail chain, growing zero-extends), and
//! unlinks files and empty directories. Every created entry is written
//! as a VFAT long-name set bound to a generated, directory-unique 8.3
//! short alias (so arbitrary, case-preserving names round-trip), and
//! every FAT mutation is mirrored across all FAT copies. No
//! `unwrap`/`expect`/`panic!` and no `unsafe`.
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
    DirEntry, FilesystemRead, FilesystemWrite, NodeId, NodeInfo, NodeKind,
};
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
/// End-of-chain value written when allocating the last cluster of a
/// chain (the canonical all-ones marker, `>= FAT32_EOC`).
const FAT32_EOC_WRITE: u32 = 0x0FFF_FFFF;
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
    /// Size of one FAT, in bytes (each of [`Layout::num_fats`] copies).
    fat_size_bytes: u64,
    /// Number of FAT copies; every FAT mutation is mirrored across all
    /// of them.
    num_fats: u64,
    /// Highest valid data-cluster number. Data clusters are numbered
    /// `2..=max_cluster`; allocation never hands out a number above it.
    max_cluster: u32,
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
    /// Device byte offset of the 8.3 short entry (the one carrying the
    /// cluster and size); the write path patches metadata here.
    short_offset: u64,
    /// Slot index (0-based, in 32-byte units from the directory start)
    /// of the first physical entry of this logical entry — the first
    /// long-name fragment, or the short entry when none precede it.
    first_slot: u64,
    /// Number of 32-byte slots this logical entry occupies (long-name
    /// fragments plus the short entry).
    slot_span: u64,
}

/// Cursor walking a directory's cluster chain, 32 bytes at a time.
struct DirCursor {
    cluster: u32,
    intra: u64,
    /// Slot index of the next entry to read, counted in 32-byte units
    /// from the directory's first slot across its whole cluster chain.
    slot: u64,
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
        short_offset: 0,
        first_slot: 0,
        slot_span: 0,
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

/// Map a name byte to a valid 8.3 short-name byte: ASCII letters are
/// upper-cased, digits and a small safe set pass through, and everything
/// else (including non-ASCII) becomes `_`.
fn sanitize_short_char(byte: u8) -> u8 {
    const SAFE: &[u8] = b"$%'-_@~`!(){}^#&";
    if byte.is_ascii_alphanumeric() {
        byte.to_ascii_uppercase()
    } else if SAFE.contains(&byte) {
        byte
    } else {
        b'_'
    }
}

/// Split `name` into its base and extension at the last interior `.`
/// (a leading dot is part of the base). The extension excludes the dot.
fn split_name(name: &[u8]) -> (&[u8], &[u8]) {
    let mut dot = None;
    for (i, &b) in name.iter().enumerate() {
        if b == b'.' && i != 0 {
            dot = Some(i);
        }
    }
    match dot {
        Some(i) => (&name[..i], &name[i + 1..]),
        None => (name, &[]),
    }
}

/// Write the decimal form of `value` into `out`, returning its length.
fn u32_to_decimal(value: u32, out: &mut [u8; 7]) -> usize {
    if value == 0 {
        out[0] = b'0';
        return 1;
    }
    let mut tmp = [0u8; 7];
    let mut n = value;
    let mut len = 0;
    while n > 0 {
        tmp[len] = b'0' + u8::try_from(n % 10).unwrap_or(0);
        n /= 10;
        len += 1;
    }
    for i in 0..len {
        out[i] = tmp[len - 1 - i];
    }
    len
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

/// Encode UTF-8 `name` into UTF-16 code `units`, returning the unit
/// count. `None` if `name` is not valid UTF-8 or needs more than
/// [`MAX_LONG_NAME_UNITS`] units.
fn encode_utf16le(name: &[u8], units: &mut [u16; MAX_LONG_NAME_UNITS]) -> Option<usize> {
    let text = core::str::from_utf8(name).ok()?;
    let mut count = 0usize;
    let mut scratch = [0u16; 2];
    for ch in text.chars() {
        for &unit in ch.encode_utf16(&mut scratch).iter() {
            if count >= units.len() {
                return None;
            }
            units[count] = unit;
            count += 1;
        }
    }
    Some(count)
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
        let fat_size_bytes = fat_size_32
            .checked_mul(u64::from(bytes_per_sector))
            .ok_or(DriverError::BadMagic)?;
        let data_clusters = (total_bytes - data_start_byte) / bytes_per_cluster;
        // Data clusters are numbered from 2, so the last valid number is
        // `data_clusters + 1`. Clamp to the 28-bit cluster space.
        let max_cluster = u32::try_from((data_clusters + 1).min(u64::from(FAT32_CLUSTER_MASK)))
            .map_err(|_| DriverError::BadMagic)?;
        if max_cluster < 2 || root_cluster > max_cluster {
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
                fat_size_bytes,
                num_fats,
                max_cluster,
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

    /// Write an arbitrary byte range to the backing device.
    fn write_bytes(&mut self, offset: u64, buf: &[u8]) -> Result<(), DriverError> {
        device_write(
            &mut self.block,
            self.block_size,
            self.block_count,
            offset,
            buf,
        )
    }

    /// Read the masked 28-bit FAT entry for `cluster` from the first FAT.
    fn fat_entry(&mut self, cluster: u32) -> Result<u32, DriverError> {
        let offset = self.layout.fat_start_byte + u64::from(cluster) * 4;
        let mut raw = [0u8; 4];
        self.read_bytes(offset, &mut raw)?;
        Ok(u32::from_le_bytes(raw) & FAT32_CLUSTER_MASK)
    }

    /// Write the 28-bit `value` into `cluster`'s entry in every FAT copy,
    /// preserving each copy's reserved top 4 bits.
    fn set_fat(&mut self, cluster: u32, value: u32) -> Result<(), DriverError> {
        let value = value & FAT32_CLUSTER_MASK;
        for fat in 0..self.layout.num_fats {
            let offset = self.layout.fat_start_byte
                + fat * self.layout.fat_size_bytes
                + u64::from(cluster) * 4;
            let mut raw = [0u8; 4];
            self.read_bytes(offset, &mut raw)?;
            let reserved = u32::from_le_bytes(raw) & !FAT32_CLUSTER_MASK;
            self.write_bytes(offset, &(reserved | value).to_le_bytes())?;
        }
        Ok(())
    }

    /// Zero the entire data region of `cluster`.
    fn zero_cluster(&mut self, cluster: u32) -> Result<(), DriverError> {
        let zeros = [0u8; MAX_BLOCK_SIZE as usize];
        let mut at = self.cluster_byte(cluster);
        let mut remaining = self.layout.bytes_per_cluster;
        while remaining > 0 {
            let chunk = remaining.min(zeros.len() as u64);
            let chunk_usize = usize::try_from(chunk).map_err(|_| DriverError::DeviceFault)?;
            self.write_bytes(at, &zeros[..chunk_usize])?;
            at += chunk;
            remaining -= chunk;
        }
        Ok(())
    }

    /// Allocate one free data cluster, mark it end-of-chain, optionally
    /// zero it, and return its number. Fails with [`DriverError::DeviceFault`]
    /// when the volume is full.
    fn alloc_cluster(&mut self, zero: bool) -> Result<u32, DriverError> {
        let mut candidate = 2u32;
        while candidate <= self.layout.max_cluster {
            if self.fat_entry(candidate)? == 0 {
                self.set_fat(candidate, FAT32_EOC_WRITE)?;
                if zero {
                    self.zero_cluster(candidate)?;
                }
                return Ok(candidate);
            }
            candidate += 1;
        }
        Err(DriverError::DeviceFault)
    }

    /// Free an entire cluster chain starting at `first`.
    fn free_chain(&mut self, first: u32) -> Result<(), DriverError> {
        let mut cluster = first;
        while (2..=self.layout.max_cluster).contains(&cluster) {
            let next = self.fat_entry(cluster)?;
            self.set_fat(cluster, 0)?;
            match classify_chain(next) {
                ChainStep::Next(n) => cluster = n,
                _ => break,
            }
        }
        Ok(())
    }

    /// Return the last cluster of the chain starting at `first` (the one
    /// whose FAT entry is an end-of-chain marker).
    fn chain_last(&mut self, first: u32) -> Result<u32, DriverError> {
        let mut cluster = first;
        loop {
            match self.next_cluster(cluster)? {
                ChainStep::Next(next) => cluster = next,
                ChainStep::End => return Ok(cluster),
                ChainStep::Bad => return Err(DriverError::DeviceFault),
            }
        }
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
        let mut run_start: Option<u64> = None;
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
            let entry_slot = cursor.slot;
            let mut raw = [0u8; DIR_ENTRY_LEN];
            self.read_bytes(entry_byte, &mut raw)?;
            cursor.intra += DIR_ENTRY_LEN as u64;
            cursor.slot += 1;

            let first = raw[0];
            if first == END_OF_DIR {
                return Ok(None);
            }
            if first == DELETED_ENTRY {
                long.reset();
                run_start = None;
                continue;
            }
            let attr = raw[11];
            if attr == ATTR_LONG_NAME {
                if run_start.is_none() {
                    run_start = Some(entry_slot);
                }
                long.push(&raw);
                continue;
            }
            if attr & ATTR_VOLUME_ID != 0 || first == b'.' {
                long.reset();
                run_start = None;
                continue;
            }

            let mut entry = parse_short_entry(&raw);
            let mut short = [0u8; 11];
            short.copy_from_slice(&raw[0..11]);
            if let Some(long_len) = long.finish(&short, &mut entry.name) {
                entry.name_len = long_len;
            }
            entry.short_offset = entry_byte;
            entry.first_slot = run_start.unwrap_or(entry_slot);
            entry.slot_span = entry_slot - entry.first_slot + 1;
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

/// Write `buf.len()` bytes starting at device byte `offset`, staging
/// through one logical block at a time.
///
/// A block touched only partially is read-modified-written so the
/// untouched bytes of that block are preserved; a fully covered block is
/// written directly.
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
        if within == 0 && take == bs_usize {
            scratch[..bs_usize].copy_from_slice(&buf[done..done + bs_usize]);
        } else {
            block.read_blocks(lba, &mut scratch[..bs_usize])?;
            scratch[within..within + take].copy_from_slice(&buf[done..done + take]);
        }
        block.write_blocks(lba, &scratch[..bs_usize])?;
        done += take;
    }
    Ok(())
}

/// One created directory entry pending write (a long-name fragment or
/// the 8.3 short entry).
type RawEntry = [u8; DIR_ENTRY_LEN];

impl<B: Block> Fat32<B> {
    /// Number of 32-byte directory slots in one cluster.
    fn slots_per_cluster(&self) -> u64 {
        self.layout.bytes_per_cluster / DIR_ENTRY_LEN as u64
    }

    /// Device byte offset of directory `slot_index` (counted from the
    /// directory's first slot across its cluster chain), or `None` if the
    /// chain ends before reaching it.
    fn dir_slot_offset(
        &mut self,
        dir_first_cluster: u32,
        slot_index: u64,
    ) -> Result<Option<u64>, DriverError> {
        let per = self.slots_per_cluster();
        let cluster_skip = slot_index / per;
        let intra = (slot_index % per) * DIR_ENTRY_LEN as u64;
        let mut cluster = dir_first_cluster;
        for _ in 0..cluster_skip {
            match self.next_cluster(cluster)? {
                ChainStep::Next(next) => cluster = next,
                ChainStep::End => return Ok(None),
                ChainStep::Bad => return Err(DriverError::DeviceFault),
            }
        }
        Ok(Some(self.cluster_byte(cluster) + intra))
    }

    /// Append one freshly zeroed cluster to directory `dir_first_cluster`.
    fn grow_directory(&mut self, dir_first_cluster: u32) -> Result<(), DriverError> {
        let last = self.chain_last(dir_first_cluster)?;
        let fresh = self.alloc_cluster(true)?;
        self.set_fat(last, fresh)?;
        Ok(())
    }

    /// Read the raw 32-byte slot at `slot_index`, or `None` past chain end.
    fn read_slot(
        &mut self,
        dir_first_cluster: u32,
        slot_index: u64,
    ) -> Result<Option<RawEntry>, DriverError> {
        match self.dir_slot_offset(dir_first_cluster, slot_index)? {
            Some(offset) => {
                let mut raw = [0u8; DIR_ENTRY_LEN];
                self.read_bytes(offset, &mut raw)?;
                Ok(Some(raw))
            }
            None => Ok(None),
        }
    }

    /// Write the raw 32-byte slot at `slot_index`, growing the directory
    /// if the slot lies past the current chain end.
    fn write_slot(
        &mut self,
        dir_first_cluster: u32,
        slot_index: u64,
        raw: &RawEntry,
    ) -> Result<(), DriverError> {
        let offset = loop {
            match self.dir_slot_offset(dir_first_cluster, slot_index)? {
                Some(offset) => break offset,
                None => self.grow_directory(dir_first_cluster)?,
            }
        };
        self.write_bytes(offset, raw)
    }

    /// Look up child `name` in directory `dir_cluster`, returning its
    /// parsed entry (with on-disk slot/offset metadata) if present.
    fn find_child(
        &mut self,
        dir_cluster: u32,
        name: &[u8],
    ) -> Result<Option<ParsedEntry>, DriverError> {
        let mut cursor = DirCursor {
            cluster: dir_cluster,
            intra: 0,
            slot: 0,
        };
        while let Some(entry) = self.next_entry(&mut cursor)? {
            if entry.name[..entry.name_len].eq_ignore_ascii_case(name) {
                return Ok(Some(entry));
            }
        }
        Ok(None)
    }

    /// Patch the first-cluster and size fields of the short entry at
    /// `short_offset`.
    fn set_entry_meta(
        &mut self,
        short_offset: u64,
        cluster: u32,
        size: u32,
    ) -> Result<(), DriverError> {
        let cb = cluster.to_le_bytes();
        self.write_bytes(short_offset + 20, &cb[2..4])?;
        self.write_bytes(short_offset + 26, &cb[0..2])?;
        self.write_bytes(short_offset + 28, &size.to_le_bytes())
    }

    /// Find a contiguous run of `count` free directory slots, growing the
    /// directory as needed. Returns the start slot and whether the run
    /// begins at the directory's end-of-entries marker (so the caller
    /// must re-terminate the directory after writing).
    fn find_free_slots(
        &mut self,
        dir_first_cluster: u32,
        count: u64,
    ) -> Result<(u64, bool), DriverError> {
        let mut slot = 0u64;
        let mut run_start = 0u64;
        let mut run_len = 0u64;
        loop {
            let first = match self.read_slot(dir_first_cluster, slot)? {
                Some(raw) => raw[0],
                None => END_OF_DIR,
            };
            if first == END_OF_DIR {
                // Everything from here on is free; ensure the run reaches
                // `count`, anchored no earlier than this slot.
                if run_len == 0 {
                    run_start = slot;
                }
                return Ok((run_start, true));
            }
            if first == DELETED_ENTRY {
                if run_len == 0 {
                    run_start = slot;
                }
                run_len += 1;
                if run_len == count {
                    return Ok((run_start, false));
                }
            } else {
                run_len = 0;
            }
            slot += 1;
        }
    }

    /// Whether the raw 11-byte short-name `candidate` is already used by a
    /// live entry in directory `dir_first_cluster`.
    fn short_name_taken(
        &mut self,
        dir_first_cluster: u32,
        candidate: &[u8; 11],
    ) -> Result<bool, DriverError> {
        let mut slot = 0u64;
        while let Some(raw) = self.read_slot(dir_first_cluster, slot)? {
            let first = raw[0];
            if first == END_OF_DIR {
                break;
            }
            slot += 1;
            if first == DELETED_ENTRY || raw[11] == ATTR_LONG_NAME || raw[11] & ATTR_VOLUME_ID != 0
            {
                continue;
            }
            if &raw[0..11] == candidate.as_slice() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Generate a unique 8.3 short name for `name` within
    /// `dir_first_cluster`, using a `~N` numeric tail.
    fn make_short_name(
        &mut self,
        dir_first_cluster: u32,
        name: &[u8],
    ) -> Result<[u8; 11], DriverError> {
        let (base_src, ext_src) = split_name(name);

        let mut ext = [b' '; 3];
        let mut ei = 0;
        for &b in ext_src {
            if ei == 3 {
                break;
            }
            if b == b' ' || b == b'.' {
                continue;
            }
            ext[ei] = sanitize_short_char(b);
            ei += 1;
        }

        let mut base = [0u8; 8];
        let mut bn = 0;
        for &b in base_src {
            if bn == 8 {
                break;
            }
            if b == b' ' || b == b'.' {
                continue;
            }
            base[bn] = sanitize_short_char(b);
            bn += 1;
        }
        if bn == 0 {
            base[0] = b'_';
            bn = 1;
        }

        for tail in 1..=u32::from(u16::MAX) {
            let mut digits = [0u8; 7];
            let digit_len = u32_to_decimal(tail, &mut digits);
            let suffix_len = 1 + digit_len; // '~' + digits
            if suffix_len >= 8 {
                break;
            }
            let keep = core::cmp::min(bn, 8 - suffix_len);
            let mut field = [b' '; 11];
            field[..keep].copy_from_slice(&base[..keep]);
            field[keep] = b'~';
            field[keep + 1..keep + 1 + digit_len].copy_from_slice(&digits[..digit_len]);
            field[8..11].copy_from_slice(&ext);
            if !self.short_name_taken(dir_first_cluster, &field)? {
                return Ok(field);
            }
        }
        Err(DriverError::DeviceFault)
    }

    /// Build one long-name fragment for sequence `seq` (1-based), covering
    /// `units[(seq-1)*13 ..]`, flagged last when `is_last`.
    fn build_lfn_entry(units: &[u16], seq: usize, is_last: bool, checksum: u8) -> RawEntry {
        let mut raw = [0u8; DIR_ENTRY_LEN];
        let mut order = u8::try_from(seq).unwrap_or(0);
        if is_last {
            order |= LFN_LAST_FLAG;
        }
        raw[0] = order;
        raw[11] = ATTR_LONG_NAME;
        raw[13] = checksum;
        let base = (seq - 1) * LFN_UNITS_PER_ENTRY;
        for (k, &offset) in LFN_CHAR_OFFSETS.iter().enumerate() {
            let idx = base + k;
            let unit = match idx.cmp(&units.len()) {
                core::cmp::Ordering::Less => units[idx],
                core::cmp::Ordering::Equal => 0x0000,
                core::cmp::Ordering::Greater => 0xFFFF,
            };
            raw[offset..offset + 2].copy_from_slice(&unit.to_le_bytes());
        }
        raw
    }

    /// Build a short directory entry with the given raw name field,
    /// attribute, first cluster, and size.
    fn build_short_entry(field: &[u8; 11], attr: u8, cluster: u32, size: u32) -> RawEntry {
        let mut raw = [0u8; DIR_ENTRY_LEN];
        raw[0..11].copy_from_slice(field);
        raw[11] = attr;
        let cb = cluster.to_le_bytes();
        raw[20..22].copy_from_slice(&cb[2..4]);
        raw[26..28].copy_from_slice(&cb[0..2]);
        raw[28..32].copy_from_slice(&size.to_le_bytes());
        raw
    }

    /// Initialise a freshly allocated directory cluster with its `.` and
    /// `..` links. `parent_cluster` is `0` when the parent is the root.
    fn init_dir_cluster(
        &mut self,
        child_cluster: u32,
        parent_cluster: u32,
    ) -> Result<(), DriverError> {
        let dot = {
            let mut f = [b' '; 11];
            f[0] = b'.';
            Self::build_short_entry(&f, ATTR_DIRECTORY, child_cluster, 0)
        };
        let dotdot = {
            let mut f = [b' '; 11];
            f[0] = b'.';
            f[1] = b'.';
            let pc = if parent_cluster == self.layout.root_cluster {
                0
            } else {
                parent_cluster
            };
            Self::build_short_entry(&f, ATTR_DIRECTORY, pc, 0)
        };
        let base = self.cluster_byte(child_cluster);
        self.write_bytes(base, &dot)?;
        self.write_bytes(base + DIR_ENTRY_LEN as u64, &dotdot)
    }

    /// Length (in clusters) and last cluster of the chain at `first`.
    fn chain_len(&mut self, first: u32) -> Result<(u64, u32), DriverError> {
        let mut cluster = first;
        let mut len = 1u64;
        loop {
            match self.next_cluster(cluster)? {
                ChainStep::Next(next) => {
                    cluster = next;
                    len += 1;
                }
                ChainStep::End => return Ok((len, cluster)),
                ChainStep::Bad => return Err(DriverError::DeviceFault),
            }
        }
    }

    /// Ensure the file whose first cluster is `first` (0 if empty) has at
    /// least `needed` clusters, allocating zeroed clusters as required.
    /// Returns the (possibly newly allocated) first cluster.
    fn ensure_chain(&mut self, first: u32, needed: u64) -> Result<u32, DriverError> {
        if needed == 0 {
            return Ok(first);
        }
        let (head, mut have, mut last) = if first < 2 {
            let fresh = self.alloc_cluster(true)?;
            (fresh, 1u64, fresh)
        } else {
            let (len, last) = self.chain_len(first)?;
            (first, len, last)
        };
        while have < needed {
            let fresh = self.alloc_cluster(true)?;
            self.set_fat(last, fresh)?;
            last = fresh;
            have += 1;
        }
        Ok(head)
    }

    /// Write `buf` into the data chain `first` starting at byte
    /// `byte_offset`. The chain must already be long enough.
    fn write_data(&mut self, first: u32, byte_offset: u64, buf: &[u8]) -> Result<(), DriverError> {
        if buf.is_empty() {
            return Ok(());
        }
        let bpc = self.layout.bytes_per_cluster;
        let mut cluster = first;
        let mut skip = byte_offset / bpc;
        while skip > 0 {
            match self.next_cluster(cluster)? {
                ChainStep::Next(next) => cluster = next,
                _ => return Err(DriverError::DeviceFault),
            }
            skip -= 1;
        }
        let mut intra = byte_offset % bpc;
        let mut done = 0usize;
        while done < buf.len() {
            if cluster < 2 {
                return Err(DriverError::DeviceFault);
            }
            let room = usize::try_from(bpc - intra).map_err(|_| DriverError::LengthOutOfRange)?;
            let take = core::cmp::min(room, buf.len() - done);
            let at = self.cluster_byte(cluster) + intra;
            self.write_bytes(at, &buf[done..done + take])?;
            done += take;
            intra = 0;
            if done < buf.len() {
                match self.next_cluster(cluster)? {
                    ChainStep::Next(next) => cluster = next,
                    _ => return Err(DriverError::DeviceFault),
                }
            }
        }
        Ok(())
    }

    /// Zero `len` bytes of the data chain `first` starting at `start`.
    fn zero_range(&mut self, first: u32, start: u64, len: u64) -> Result<(), DriverError> {
        let zeros = [0u8; MAX_BLOCK_SIZE as usize];
        let mut remaining = len;
        let mut at = start;
        while remaining > 0 {
            let chunk = remaining.min(zeros.len() as u64);
            let chunk_usize = usize::try_from(chunk).map_err(|_| DriverError::DeviceFault)?;
            self.write_data(first, at, &zeros[..chunk_usize])?;
            at += chunk;
            remaining -= chunk;
        }
        Ok(())
    }

    /// Shared implementation of [`FilesystemWrite::create`].
    fn create_child(
        &mut self,
        dir: NodeId,
        name: &[u8],
        kind: NodeKind,
    ) -> Result<NodeId, DriverError> {
        if !node_is_dir(dir) {
            return Err(DriverError::Unsupported);
        }
        if name.is_empty() || name.len() > MAX_NAME_BYTES {
            return Err(DriverError::LengthOutOfRange);
        }
        let dir_cluster = node_cluster(dir);
        if self.find_child(dir_cluster, name)?.is_some() {
            return Err(DriverError::Busy);
        }

        let mut units = [0u16; MAX_LONG_NAME_UNITS];
        let unit_count = encode_utf16le(name, &mut units).ok_or(DriverError::LengthOutOfRange)?;
        if unit_count == 0 {
            return Err(DriverError::LengthOutOfRange);
        }
        let frag_count = unit_count.div_ceil(LFN_UNITS_PER_ENTRY);
        if frag_count > LFN_MAX_FRAGMENTS {
            return Err(DriverError::LengthOutOfRange);
        }

        let short = self.make_short_name(dir_cluster, name)?;
        let checksum = short_name_checksum(&short);
        let total_slots = frag_count as u64 + 1;
        let (start_slot, at_end) = self.find_free_slots(dir_cluster, total_slots)?;

        let is_dir = matches!(kind, NodeKind::Directory);
        let child_cluster = if is_dir {
            let fresh = self.alloc_cluster(true)?;
            self.init_dir_cluster(fresh, dir_cluster)?;
            fresh
        } else {
            0
        };

        // Physical order: the highest sequence (flagged last-logical) is
        // written first, descending to sequence 1, then the short entry.
        for phys in 0..frag_count {
            let seq = frag_count - phys;
            let entry = Self::build_lfn_entry(&units[..unit_count], seq, phys == 0, checksum);
            self.write_slot(dir_cluster, start_slot + phys as u64, &entry)?;
        }
        let attr = if is_dir { ATTR_DIRECTORY } else { 0x20 };
        let short_entry = Self::build_short_entry(&short, attr, child_cluster, 0);
        self.write_slot(dir_cluster, start_slot + frag_count as u64, &short_entry)?;

        if at_end {
            let terminator = [0u8; DIR_ENTRY_LEN];
            self.write_slot(dir_cluster, start_slot + total_slots, &terminator)?;
        }

        Ok(pack_node(child_cluster, is_dir, 0))
    }

    /// Shared implementation of [`FilesystemWrite::write_at`].
    fn write_file(
        &mut self,
        dir: NodeId,
        name: &[u8],
        offset: u64,
        data: &[u8],
    ) -> Result<usize, DriverError> {
        if !node_is_dir(dir) {
            return Err(DriverError::Unsupported);
        }
        let dir_cluster = node_cluster(dir);
        let entry = self
            .find_child(dir_cluster, name)?
            .ok_or(DriverError::NotFound)?;
        if entry.is_dir {
            return Err(DriverError::Unsupported);
        }
        if data.is_empty() {
            return Ok(0);
        }
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or(DriverError::LengthOutOfRange)?;
        let old_size = u64::from(entry.size);
        let bpc = self.layout.bytes_per_cluster;
        let needed = end.div_ceil(bpc);
        let first = self.ensure_chain(entry.cluster, needed)?;
        if offset > old_size {
            self.zero_range(first, old_size, offset - old_size)?;
        }
        self.write_data(first, offset, data)?;
        let new_size =
            u32::try_from(old_size.max(end)).map_err(|_| DriverError::LengthOutOfRange)?;
        self.set_entry_meta(entry.short_offset, first, new_size)?;
        Ok(data.len())
    }

    /// Shared implementation of [`FilesystemWrite::truncate`].
    fn truncate_file(&mut self, dir: NodeId, name: &[u8], size: u64) -> Result<(), DriverError> {
        if !node_is_dir(dir) {
            return Err(DriverError::Unsupported);
        }
        let dir_cluster = node_cluster(dir);
        let entry = self
            .find_child(dir_cluster, name)?
            .ok_or(DriverError::NotFound)?;
        if entry.is_dir {
            return Err(DriverError::Unsupported);
        }
        let old_size = u64::from(entry.size);
        if size == old_size {
            return Ok(());
        }
        let bpc = self.layout.bytes_per_cluster;
        let new_size = u32::try_from(size).map_err(|_| DriverError::LengthOutOfRange)?;

        if size < old_size {
            let needed = size.div_ceil(bpc);
            if needed == 0 {
                if entry.cluster >= 2 {
                    self.free_chain(entry.cluster)?;
                }
                self.set_entry_meta(entry.short_offset, 0, 0)?;
                return Ok(());
            }
            // Walk to the new last cluster, sever and free the remainder.
            let mut cluster = entry.cluster;
            for _ in 0..needed - 1 {
                match self.next_cluster(cluster)? {
                    ChainStep::Next(next) => cluster = next,
                    _ => return Err(DriverError::DeviceFault),
                }
            }
            if let ChainStep::Next(tail) = self.next_cluster(cluster)? {
                self.free_chain(tail)?;
            }
            self.set_fat(cluster, FAT32_EOC_WRITE)?;
            self.set_entry_meta(entry.short_offset, entry.cluster, new_size)?;
        } else {
            let needed = size.div_ceil(bpc);
            let first = self.ensure_chain(entry.cluster, needed)?;
            self.zero_range(first, old_size, size - old_size)?;
            self.set_entry_meta(entry.short_offset, first, new_size)?;
        }
        Ok(())
    }

    /// Shared implementation of [`FilesystemWrite::remove`].
    fn remove_child(&mut self, dir: NodeId, name: &[u8]) -> Result<(), DriverError> {
        if !node_is_dir(dir) {
            return Err(DriverError::Unsupported);
        }
        let dir_cluster = node_cluster(dir);
        let entry = self
            .find_child(dir_cluster, name)?
            .ok_or(DriverError::NotFound)?;
        if entry.is_dir {
            let mut child = DirCursor {
                cluster: entry.cluster,
                intra: 0,
                slot: 0,
            };
            if self.next_entry(&mut child)?.is_some() {
                return Err(DriverError::Busy);
            }
        }
        if entry.cluster >= 2 {
            self.free_chain(entry.cluster)?;
        }
        for i in 0..entry.slot_span {
            if let Some(offset) = self.dir_slot_offset(dir_cluster, entry.first_slot + i)? {
                self.write_bytes(offset, &[DELETED_ENTRY])?;
            }
        }
        Ok(())
    }
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
            slot: 0,
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
            slot: 0,
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

impl<B: Block> FilesystemWrite for Fat32<B> {
    fn create(&mut self, dir: NodeId, name: &[u8], kind: NodeKind) -> Result<NodeId, DriverError> {
        self.create_child(dir, name, kind)
    }

    fn write_at(
        &mut self,
        dir: NodeId,
        name: &[u8],
        offset: u64,
        data: &[u8],
    ) -> Result<usize, DriverError> {
        self.write_file(dir, name, offset, data)
    }

    fn truncate(&mut self, dir: NodeId, name: &[u8], size: u64) -> Result<(), DriverError> {
        self.truncate_file(dir, name, size)
    }

    fn remove(&mut self, dir: NodeId, name: &[u8]) -> Result<(), DriverError> {
        self.remove_child(dir, name)
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        // All mutations are written straight through to the block device.
        Ok(())
    }
}

#[cfg(test)]
mod tests;
