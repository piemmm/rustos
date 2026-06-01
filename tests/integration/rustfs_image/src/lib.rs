//! Single-source-of-truth rustfs disk-image fixture shared by the
//! end-to-end QEMU rustfs-over-virtio_blk vertical.
//!
//! Unlike the hand-built FAT32 fixture, this image is laid down by the
//! **real** rustfs driver: [`build_image`] formats an in-memory volume
//! through [`RustFs::format`](rustos_drv_fs_rustfs::RustFs::format), plants
//! [`PLANTED_FILE_NAME`] / [`PLANTED_FILE_CONTENT`] through the driver's
//! own write path, and returns the resulting bytes. The on-disk layout
//! therefore has exactly one author — the driver — so the fixture and the
//! driver can never drift (`AGENTS.md` §2.2).
//!
//! The host harness (`tools/xtask`) plants those bytes on the test's
//! backing disk before the guest boots. The freestanding guest tail
//! (`tests/integration/virtio_qemu_support`) mounts that very volume
//! through the real rustfs driver, verifies the planted file, then
//! creates and writes a fresh file and reads it back. Both sides name the
//! same fixed files through the constants below, so the on-disk contract
//! lives in exactly one place.
//!
//! The image is a genuine rustfs volume — 1 MiB, 512-byte blocks, 64
//! inodes — laid out so the real `RustFs::open` validator accepts it. It
//! is `no_std` + `alloc` so it links into both the host build tool and
//! the freestanding guest test.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

use rustos_abi::driver::block::{Block, BlockGeometry};
use rustos_abi::driver::filesystem::{FilesystemRead, FilesystemWrite, NodeKind};
use rustos_abi::DriverError;
use rustos_drv_fs_rustfs::RustFs;

/// Logical block (sector) size of the produced image, in bytes. Matches
/// both the 512-byte sector QEMU's virtio-blk reports by default and the
/// rustfs minimum block size, so the volume the driver formats here maps
/// directly onto the device the guest mounts.
pub const SECTOR_BYTES: usize = 512;

/// Total size of the produced image, in 512-byte sectors (1 MiB). Large
/// enough for the inode table, bitmap, journal, and a non-trivial data
/// region, matching the FAT32 fixture's footprint.
pub const TOTAL_SECTORS: u64 = 2048;

/// Number of inodes the volume is formatted with. Two-per-block at the
/// 512-byte block size, comfortably more than the root plus the planted
/// and written files need.
const INODE_COUNT: u32 = 64;

/// File planted in the root directory before boot. The guest tail looks
/// it up and verifies [`PLANTED_FILE_CONTENT`].
pub const PLANTED_FILE_NAME: &[u8] = b"hello.txt";

/// Contents of [`PLANTED_FILE_NAME`].
pub const PLANTED_FILE_CONTENT: &[u8] = b"Hello from a planted rustfs volume on virtio-blk.\n";

/// File the guest tail creates and writes after mounting.
pub const NEW_FILE_NAME: &[u8] = b"written.txt";

/// Contents the guest tail writes to [`NEW_FILE_NAME`] and reads back.
pub const NEW_FILE_CONTENT: &[u8] = b"RustOS wrote this file to rustfs over virtio-blk.\n";

/// In-memory [`Block`] device backing the fixture build and the host
/// round-trip tests. It addresses [`SECTOR_BYTES`]-byte sectors exactly
/// as the guest's virtio-blk device does.
struct VecBlock {
    store: Vec<u8>,
}

impl VecBlock {
    /// A zeroed device of `sectors` sectors.
    fn new(sectors: u64) -> Self {
        let len = usize::try_from(sectors).unwrap_or(0) * SECTOR_BYTES;
        Self {
            store: vec![0u8; len],
        }
    }

    /// Byte span `[start, end)` for `len` bytes at sector `lba`, or an
    /// error if the access is unaligned or out of range.
    fn span(&self, lba: u64, len: usize) -> Result<(usize, usize), DriverError> {
        if len == 0 || len % SECTOR_BYTES != 0 {
            return Err(DriverError::BufferTooSmall);
        }
        let start = usize::try_from(lba)
            .ok()
            .and_then(|l| l.checked_mul(SECTOR_BYTES))
            .ok_or(DriverError::LengthOutOfRange)?;
        let end = start
            .checked_add(len)
            .ok_or(DriverError::LengthOutOfRange)?;
        if end > self.store.len() {
            return Err(DriverError::LengthOutOfRange);
        }
        Ok((start, end))
    }
}

impl Block for VecBlock {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        Ok(BlockGeometry {
            block_size: u32::try_from(SECTOR_BYTES).unwrap_or(0),
            block_count: TOTAL_SECTORS,
        })
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        let (start, end) = self.span(lba, buf.len())?;
        buf.copy_from_slice(&self.store[start..end]);
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        let (start, end) = self.span(lba, buf.len())?;
        self.store[start..end].copy_from_slice(buf);
        Ok(())
    }
}

/// Build the rustfs image described in the module docs by driving the
/// real rustfs driver: format a fresh in-memory volume, plant
/// [`PLANTED_FILE_NAME`] with [`PLANTED_FILE_CONTENT`], flush, and return
/// the resulting on-disk bytes.
///
/// # Errors
///
/// Propagates any [`DriverError`] from the driver. The fixed geometry and
/// payload sizes make a failure a programming error in this fixture, but
/// the result is surfaced rather than panicked so the builder holds to
/// `AGENTS.md` §2.9 in every path it links into.
pub fn build_image() -> Result<Vec<u8>, DriverError> {
    let dev = VecBlock::new(TOTAL_SECTORS);
    let mut fs = RustFs::format(dev, INODE_COUNT)?;
    let root = fs.root();
    fs.create(root, PLANTED_FILE_NAME, NodeKind::RegularFile)?;
    let written = fs.write_at(root, PLANTED_FILE_NAME, 0, PLANTED_FILE_CONTENT)?;
    if written != PLANTED_FILE_CONTENT.len() {
        return Err(DriverError::DeviceFault);
    }
    fs.flush()?;
    Ok(fs.into_block().into_bytes())
}

impl VecBlock {
    /// Consume the device, yielding its raw image bytes.
    fn into_bytes(self) -> Vec<u8> {
        self.store
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mount() -> RustFs<VecBlock> {
        let bytes = build_image().expect("the fixture builds a valid rustfs volume");
        let dev = VecBlock { store: bytes };
        RustFs::open(dev).expect("the built image is a valid rustfs volume")
    }

    #[test]
    fn image_is_exactly_the_advertised_size() {
        let bytes = build_image().expect("build image");
        let expected =
            usize::try_from(TOTAL_SECTORS).expect("sector count fits usize") * SECTOR_BYTES;
        assert_eq!(bytes.len(), expected);
    }

    #[test]
    fn driver_mounts_the_built_image() {
        let _fs = mount();
    }

    #[test]
    fn planted_file_reads_back_its_known_contents() {
        let mut fs = mount();
        let root = fs.root();
        let node = fs.lookup(root, PLANTED_FILE_NAME).expect("planted present");
        let mut buf = [0u8; 128];
        let n = fs.read_at(node, 0, &mut buf).expect("read planted file");
        assert_eq!(&buf[..n], PLANTED_FILE_CONTENT);
    }

    #[test]
    fn a_fresh_file_round_trips_through_create_write_and_read() {
        let mut fs = mount();
        let root = fs.root();
        fs.create(root, NEW_FILE_NAME, NodeKind::RegularFile)
            .expect("create new file");
        let written = fs
            .write_at(root, NEW_FILE_NAME, 0, NEW_FILE_CONTENT)
            .expect("write new file");
        assert_eq!(written, NEW_FILE_CONTENT.len());

        let node = fs.lookup(root, NEW_FILE_NAME).expect("new file present");
        let mut buf = [0u8; 128];
        let n = fs.read_at(node, 0, &mut buf).expect("read new file");
        assert_eq!(&buf[..n], NEW_FILE_CONTENT);
    }

    #[test]
    fn the_planted_file_survives_a_second_file_being_written() {
        let mut fs = mount();
        let root = fs.root();
        fs.create(root, NEW_FILE_NAME, NodeKind::RegularFile)
            .expect("create new file");
        fs.write_at(root, NEW_FILE_NAME, 0, NEW_FILE_CONTENT)
            .expect("write new file");

        let node = fs.lookup(root, PLANTED_FILE_NAME).expect("planted present");
        let mut buf = [0u8; 128];
        let n = fs.read_at(node, 0, &mut buf).expect("read planted file");
        assert_eq!(&buf[..n], PLANTED_FILE_CONTENT);
    }
}
