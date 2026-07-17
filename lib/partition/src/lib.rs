//! Shared, scheme-neutral partition-table model and a partition-window
//! [`Block`] adapter.
//!
//! A flashed RustOS disk (an SD card, a USB stick, a UEFI hard disk, a
//! `virt` virtio-blk image) carries a partition table that names a FAT
//! boot partition the firmware reads and the encrypted `ARXFS` root
//! partition the kernel mounts (`tools/mkimage`).
//! The table is **not** one scheme on one board: a Raspberry Pi image is
//! an MBR disk, a UEFI x86_64 disk is GPT, and RustOS must read either on
//! any architecture (nothing here is board-specific).
//!
//! Two places must agree, byte for byte, on the on-disk layout — the
//! image author (`tools/mkimage`) that *writes* it and the boot path
//! (`kernel/rustos-kernel`) that *reads* it back to find the partitions.
//! This crate is that one definition, so the author and the reader can
//! never drift:
//!
//! * the scheme-neutral [`Partition`] / [`PartitionType`] /
//!   [`PartitionTable`] model, in *device logical blocks*, large enough
//!   for 64-bit GPT LBAs;
//! * [`parse_partition_table`], which detects the scheme on a [`Block`]
//!   device and dispatches to the [`mbr`] or [`gpt`] parser, fail-closed
//!   against an untrusted, possibly-hostile disk;
//! * the per-scheme [`mbr`] and [`gpt`] modules;
//! * [`PartitionBlock`], which presents one partition's extent of an
//!   underlying [`Block`] device as a standalone, bounds-checked
//!   [`Block`], so a filesystem driver mounts a partition without ever
//!   seeing — or being able to reach — the bytes outside it.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

#[cfg(test)]
extern crate alloc;

pub mod gpt;
pub mod mbr;

use rustos_abi::driver::block::{Block, BlockGeometry};
use rustos_abi::driver::BufferClass;
use rustos_abi::DriverError;

/// Largest number of present partitions [`parse_partition_table`] retains
/// from one disk.
///
/// This is GPT's standard default entry count, and serves here as a
/// fail-closed bound on an untrusted on-disk table (a defensive parse bound, not a scalable capacity). Real RustOS
/// disks carry a handful of partitions; a table declaring more present
/// partitions than this is rejected rather than truncated, so the root
/// partition is never silently dropped.
pub const MAX_PARTITIONS: usize = 128;

/// The role RustOS assigns a partition, derived from the scheme-specific
/// type identifier (an MBR type byte or a GPT type GUID).
///
/// RustOS's boot path only needs to *find* two roles; every other
/// partition is [`PartitionType::Other`] and carried for completeness but
/// not consumed. The type is a routing hint, never a trusted identity:
/// the filesystem a partition is handed to still validates its own
/// on-disk magic (a match is necessary, never
/// sufficient).
// `ARXFS` is the filesystem's product name and is spelled in full capitals
// everywhere; the mixed-case `Arxfs` the acronym lint would otherwise require
// is not an accepted spelling of the name.
#[allow(clippy::upper_case_acronyms)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PartitionType {
    /// A FAT partition the platform firmware boots from: an MBR
    /// FAT32-LBA partition, or a GPT EFI System Partition.
    FatBoot,
    /// The read-only, signed-bundle `ARXFS` `/System` partition the
    /// kernel mounts read-only **before** unlocking the encrypted data
    /// root (the design-B pre-unlock driver store, `plans/PI.md`). It
    /// carries no secrets, so it is keyed by a non-secret well-known
    /// volume key; tamper-evidence comes from the per-bundle Ed25519
    /// signatures the load gate verifies, not from
    /// encryption.
    ARXFSSystem,
    /// The encrypted `ARXFS` data-root partition the kernel mounts after
    /// the operator unlocks it; carries `/Users`, `/Apps`, `/Storage`, and
    /// `/System/Security`.
    ARXFSRoot,
    /// Any other partition; RustOS's boot path does not consume it.
    Other,
}

/// One partition's extent on a disk, in the device's logical blocks.
///
/// LBAs are 64-bit so a GPT partition past the 2 TiB MBR ceiling is
/// representable; an MBR partition's 32-bit fields widen losslessly into
/// it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Partition {
    /// The role this partition plays for RustOS.
    pub ty: PartitionType,
    /// First logical block (LBA) of the partition.
    pub start_lba: u64,
    /// Number of logical blocks in the partition.
    pub block_count: u64,
}

/// The partitions parsed from a disk's table, in entry order.
///
/// Holds the extents inline (no allocation) so the boot path parses the
/// table off the stack.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PartitionTable {
    entries: [Partition; MAX_PARTITIONS],
    len: usize,
}

impl PartitionTable {
    /// An empty table.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            entries: [Partition {
                ty: PartitionType::Other,
                start_lba: 0,
                block_count: 0,
            }; MAX_PARTITIONS],
            len: 0,
        }
    }

    /// Append a present partition, failing closed past [`MAX_PARTITIONS`].
    ///
    /// # Errors
    ///
    /// [`MAX_PARTITIONS`] reached.
    pub fn push(&mut self, part: Partition) -> Result<(), PartitionError> {
        if self.len >= MAX_PARTITIONS {
            return Err(PartitionError::TooManyPartitions);
        }
        self.entries[self.len] = part;
        self.len += 1;
        Ok(())
    }

    /// The present partitions, in entry order.
    #[must_use]
    pub fn partitions(&self) -> &[Partition] {
        &self.entries[..self.len]
    }

    /// The first present partition playing the given role, or [`None`].
    #[must_use]
    pub fn first_of_type(&self, ty: PartitionType) -> Option<Partition> {
        self.partitions().iter().copied().find(|p| p.ty == ty)
    }
}

impl Default for PartitionTable {
    fn default() -> Self {
        Self::empty()
    }
}

/// Why a partition table could not be parsed off a device.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PartitionError {
    /// No recognised partition scheme (neither a valid MBR signature nor
    /// a GPT header) was found.
    NoScheme,
    /// The MBR sector is malformed ([`mbr::MbrError`]).
    Mbr(mbr::MbrError),
    /// The GPT header or entry array is malformed ([`gpt::GptError`]).
    Gpt(gpt::GptError),
    /// More than [`MAX_PARTITIONS`] present partitions were found.
    TooManyPartitions,
    /// A block read against the device failed.
    Device(DriverError),
}

impl From<DriverError> for PartitionError {
    fn from(e: DriverError) -> Self {
        PartitionError::Device(e)
    }
}

/// Parse the partition table off `dev`, detecting the scheme.
///
/// `dev` is an untrusted device, so the table is fully validated before
/// any extent is trusted. Detection is by
/// content, not by guessing: LBA 1 is inspected for the GPT
/// `"EFI PART"` signature (the GPT case also has a protective MBR in LBA
/// 0); otherwise LBA 0 is parsed as a classic MBR. A table that violates
/// any invariant is rejected **whole**.
///
/// LBAs in the returned [`PartitionTable`] are in `dev`'s logical blocks.
///
/// # Errors
///
/// A [`PartitionError`]: [`PartitionError::NoScheme`] if neither scheme is
/// present, [`PartitionError::Mbr`] / [`PartitionError::Gpt`] for a
/// malformed table, or [`PartitionError::Device`] on a read fault.
pub fn parse_partition_table<B: Block>(dev: &mut B) -> Result<PartitionTable, PartitionError> {
    let geo = dev.geometry()?;
    if gpt::is_gpt_disk(dev, &geo)? {
        gpt::parse(dev, &geo)
    } else {
        let mut lba0 = [0u8; mbr::MBR_SECTOR_LEN];
        read_lba0(dev, &geo, &mut lba0)?;
        mbr::parse(&lba0).map_err(PartitionError::Mbr)
    }
}

/// Read LBA 0 into a [`mbr::MBR_SECTOR_LEN`]-byte buffer regardless of the
/// device's (possibly larger) logical-block size.
fn read_lba0<B: Block>(
    dev: &mut B,
    geo: &BlockGeometry,
    out: &mut [u8; mbr::MBR_SECTOR_LEN],
) -> Result<(), PartitionError> {
    let bs = geo.block_size as usize;
    if bs == 0 || bs < mbr::MBR_SECTOR_LEN {
        return Err(PartitionError::NoScheme);
    }
    if bs == mbr::MBR_SECTOR_LEN {
        dev.read_blocks(0, out)?;
    } else {
        let mut block = [0u8; gpt::MAX_BLOCK_SIZE];
        let buf = block.get_mut(..bs).ok_or(PartitionError::NoScheme)?;
        dev.read_blocks(0, buf)?;
        out.copy_from_slice(&buf[..mbr::MBR_SECTOR_LEN]);
    }
    Ok(())
}

/// A standalone [`Block`] device backed by one partition's extent of an
/// underlying device.
///
/// Reads and writes are expressed in the partition's own block space
/// (LBA `0` is the partition's first block) and translated onto the inner
/// device, so a filesystem driver mounting a partition can never address
/// — or even name — a block outside it (every input
/// validated; — the window is a fixed extent, not a growable
/// capacity). The window's bounds are validated against the inner
/// device's geometry once, at construction; every access is then bounded
/// by the window length.
pub struct PartitionBlock<B: Block> {
    inner: B,
    start_block: u64,
    block_count: u64,
    block_size: u32,
}

impl<B: Block> PartitionBlock<B> {
    /// Build a partition window covering `block_count` blocks of `inner`
    /// starting at `start_block`, in `inner`'s logical-block space.
    ///
    /// # Errors
    ///
    /// * [`DriverError::DeviceFault`] if the inner geometry cannot be read.
    /// * [`DriverError::LengthOutOfRange`] if the window is empty or runs
    ///   past the end of the inner device.
    pub fn new(inner: B, start_block: u64, block_count: u64) -> Result<Self, DriverError> {
        let geo = inner.geometry()?;
        if block_count == 0 {
            return Err(DriverError::LengthOutOfRange);
        }
        let end = start_block
            .checked_add(block_count)
            .ok_or(DriverError::LengthOutOfRange)?;
        if end > geo.block_count {
            return Err(DriverError::LengthOutOfRange);
        }
        Ok(Self {
            inner,
            start_block,
            block_count,
            block_size: geo.block_size,
        })
    }

    /// Build a partition window from a parsed [`Partition`], whose LBAs are
    /// already in the inner device's logical blocks (the unit
    /// [`parse_partition_table`] returns).
    ///
    /// # Errors
    ///
    /// As [`PartitionBlock::new`].
    pub fn from_partition(inner: B, part: &Partition) -> Result<Self, DriverError> {
        Self::new(inner, part.start_lba, part.block_count)
    }

    /// The number of logical blocks the window spans.
    #[must_use]
    pub fn block_count(&self) -> u64 {
        self.block_count
    }

    /// Consume the window and return the underlying device.
    #[must_use]
    pub fn into_inner(self) -> B {
        self.inner
    }

    /// Translate a window-relative request to an inner-device LBA after
    /// bounds-checking it against the window length.
    fn inner_lba(&self, lba: u64, buf_len: usize) -> Result<u64, DriverError> {
        let bs = self.block_size as usize;
        if buf_len == 0 || bs == 0 || buf_len % bs != 0 {
            return Err(DriverError::BufferTooSmall);
        }
        let blocks = u64::try_from(buf_len / bs).map_err(|_| DriverError::LengthOutOfRange)?;
        let end = lba
            .checked_add(blocks)
            .ok_or(DriverError::LengthOutOfRange)?;
        if end > self.block_count {
            return Err(DriverError::LengthOutOfRange);
        }
        // `start_block + lba` cannot overflow: the window's end block was
        // validated `<= inner.block_count` at construction and `lba` is
        // strictly within the window.
        Ok(self.start_block + lba)
    }
}

impl<B: Block> Block for PartitionBlock<B> {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        Ok(BlockGeometry {
            block_size: self.block_size,
            block_count: self.block_count,
        })
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        let inner_lba = self.inner_lba(lba, buf.len())?;
        self.inner.read_blocks(inner_lba, buf)
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        let inner_lba = self.inner_lba(lba, buf.len())?;
        self.inner.write_blocks(inner_lba, buf)
    }

    fn read_blocks_with_class(
        &mut self,
        lba: u64,
        buf: &mut [u8],
        class: BufferClass,
    ) -> Result<(), DriverError> {
        // Forward the sensitivity class so the inner driver still scrubs
        // any private staging copy.
        let inner_lba = self.inner_lba(lba, buf.len())?;
        self.inner.read_blocks_with_class(inner_lba, buf, class)
    }

    fn write_blocks_with_class(
        &mut self,
        lba: u64,
        buf: &[u8],
        class: BufferClass,
    ) -> Result<(), DriverError> {
        let inner_lba = self.inner_lba(lba, buf.len())?;
        self.inner.write_blocks_with_class(inner_lba, buf, class)
    }
}

#[cfg(test)]
mod tests;
