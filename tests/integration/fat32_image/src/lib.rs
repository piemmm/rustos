//! Specification-shaped FAT32 disk-image fixture shared by the
//! end-to-end QEMU FAT32-over-virtio_blk vertical.
//!
//! The host harness (`tools/xtask`) plants the bytes returned by
//! [`build_image`] on the test's backing disk before the guest boots.
//! The freestanding guest tail
//! (`tests/integration/virtio_qemu_support`) mounts that very volume
//! through the real FAT32 driver and verifies it, then writes a fresh
//! file and reads it back. Both sides name the same fixed file through
//! the constants below, so the on-disk contract lives in exactly one
//! place (`AGENTS.md` §2.2).
//!
//! The image is a minimal but genuine FAT32 volume — 1 MiB, two
//! mirrored FATs, one-sector clusters — laid out so the real
//! `Fat32::open` validator (a zero 16-bit FAT size and zero root-entry
//! count, a power-of-two sector and cluster size, a root cluster inside
//! the data region) accepts it. It
//! is `no_std` + `alloc` so it links into both the host build tool and
//! the freestanding guest test.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

/// On-disk bytes-per-sector (BPB offset 11). The on-disk fields carry
/// their natural width so the boot sector is written without any
/// narrowing cast.
const BYTES_PER_SECTOR: u16 = 512;

/// Total size of the produced image, in 512-byte sectors (1 MiB). Large
/// enough that the FAT covers every data cluster, so the write path can
/// allocate freely without the FAT and the data region overlapping.
const TOTAL_SECTORS_U32: u32 = 2048;

/// Logical block (sector) size of the produced image, in bytes. Matches
/// the 512-byte sector QEMU's virtio-blk reports by default and the unit
/// the backing-disk planter addresses in.
pub const SECTOR_BYTES: usize = BYTES_PER_SECTOR as usize;

/// Total size of the produced image, in 512-byte sectors (1 MiB).
pub const TOTAL_SECTORS: u64 = TOTAL_SECTORS_U32 as u64;

/// 8.3 short-named file planted in the root directory. The guest tail
/// looks it up and verifies [`PLANTED_FILE_CONTENT`].
pub const PLANTED_FILE_NAME: &[u8] = b"HELLO.TXT";

/// Contents of [`PLANTED_FILE_NAME`]. One sector keeps it inside a
/// single cluster.
pub const PLANTED_FILE_CONTENT: &[u8] = b"Hello from a planted FAT32 image on virtio-blk.\n";

/// File the guest tail creates and writes after mounting. The mixed
/// case exercises the driver's VFAT long-name round-trip.
pub const NEW_FILE_NAME: &[u8] = b"Written.txt";

/// Contents the guest tail writes to [`NEW_FILE_NAME`] and reads back.
pub const NEW_FILE_CONTENT: &[u8] = b"RustOS wrote this file to FAT32 over virtio-blk.\n";

/// Sectors per cluster (one — the simplest valid geometry).
const SECTORS_PER_CLUSTER: u8 = 1;
/// Reserved sectors before the first FAT (sector 0 is the boot sector).
const RESERVED_SECTORS: u16 = 32;
/// Number of FAT copies; two exercises the driver's FAT mirroring.
const NUM_FATS: u8 = 2;
/// Length of each FAT, in sectors. 16 sectors hold 2048 32-bit entries —
/// more than the data region's cluster count, so every data cluster is
/// addressable.
const FAT_SECTORS: u32 = 16;
/// First cluster of the root directory (FAT32 numbers clusters from 2).
const ROOT_CLUSTER: u32 = 2;
/// Cluster holding [`PLANTED_FILE_CONTENT`].
const HELLO_CLUSTER: u32 = 3;

/// End-of-chain marker written into the FAT.
const FAT_EOC: u32 = 0x0FFF_FFFF;
/// FAT entry 0: media descriptor in the low byte, ones elsewhere.
const FAT_MEDIA: u32 = 0x0FFF_FFF8;
/// On-disk directory-entry size, frozen by the FAT specification.
const DIR_ENTRY_LEN: usize = 32;
/// Attribute byte for an ordinary (archive) file.
const ATTR_ARCHIVE: u8 = 0x20;

fn set_le16(img: &mut [u8], offset: usize, value: u16) {
    img[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn set_le32(img: &mut [u8], offset: usize, value: u32) {
    img[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

/// Byte offset of `cluster`'s FAT slot within FAT copy `fat_index`.
fn fat_slot_offset(fat_index: u32, cluster: u32) -> usize {
    let fat_start_sector =
        usize::from(RESERVED_SECTORS) + fat_index as usize * FAT_SECTORS as usize;
    fat_start_sector * SECTOR_BYTES + cluster as usize * 4
}

/// Write `value` into `cluster`'s slot in every FAT copy.
fn set_fat(img: &mut [u8], cluster: u32, value: u32) {
    for fat_index in 0..u32::from(NUM_FATS) {
        set_le32(img, fat_slot_offset(fat_index, cluster), value);
    }
}

/// Byte offset of `cluster`'s data within the image.
fn cluster_byte_offset(cluster: u32) -> usize {
    let data_start_sector =
        usize::from(RESERVED_SECTORS) + usize::from(NUM_FATS) * FAT_SECTORS as usize;
    let sector = data_start_sector
        + (cluster as usize - ROOT_CLUSTER as usize) * usize::from(SECTORS_PER_CLUSTER);
    sector * SECTOR_BYTES
}

/// Build an 11-byte 8.3 short name (`base` left-justified in the first
/// eight bytes, `ext` in the last three, both space-padded).
fn short_name(base: &[u8], ext: &[u8]) -> [u8; 11] {
    let mut out = [b' '; 11];
    out[..base.len()].copy_from_slice(base);
    out[8..8 + ext.len()].copy_from_slice(ext);
    out
}

/// Write one 8.3 directory entry at `offset`.
fn write_short_entry(
    img: &mut [u8],
    offset: usize,
    name: &[u8; 11],
    attr: u8,
    cluster: u32,
    size: u32,
) {
    img[offset..offset + 11].copy_from_slice(name);
    img[offset + 11] = attr;
    let cluster_bytes = cluster.to_le_bytes();
    img[offset + 20..offset + 22].copy_from_slice(&cluster_bytes[2..4]);
    img[offset + 26..offset + 28].copy_from_slice(&cluster_bytes[0..2]);
    set_le32(img, offset + 28, size);
}

/// Construct the FAT32 image described in the module docs.
///
/// # Panics
///
/// Never in practice: the layout constants are sized so every write
/// stays in bounds and [`PLANTED_FILE_CONTENT`] fits one cluster. The
/// debug assertions document those invariants for a future edit.
#[must_use]
pub fn build_image() -> Vec<u8> {
    debug_assert!(
        PLANTED_FILE_CONTENT.len() <= SECTOR_BYTES * usize::from(SECTORS_PER_CLUSTER),
        "planted file must fit one cluster"
    );

    let mut img = vec![0u8; TOTAL_SECTORS_U32 as usize * SECTOR_BYTES];

    // BIOS parameter block (boot sector).
    set_le16(&mut img, 11, BYTES_PER_SECTOR); // bytes per sector
    img[13] = SECTORS_PER_CLUSTER; // sectors per cluster
    set_le16(&mut img, 14, RESERVED_SECTORS); // reserved sectors
    img[16] = NUM_FATS; // number of FATs
    set_le16(&mut img, 17, 0); // root entry count (0 for FAT32)
    set_le16(&mut img, 22, 0); // 16-bit FAT size (0 for FAT32)
    set_le32(&mut img, 32, TOTAL_SECTORS_U32); // total sectors (32-bit)
    set_le32(&mut img, 36, FAT_SECTORS); // 32-bit FAT size (sectors)
    set_le32(&mut img, 44, ROOT_CLUSTER); // root cluster
    img[510] = 0x55;
    img[511] = 0xAA;

    // File allocation table (mirrored across both copies).
    set_fat(&mut img, 0, FAT_MEDIA);
    set_fat(&mut img, 1, FAT_EOC);
    set_fat(&mut img, ROOT_CLUSTER, FAT_EOC);
    set_fat(&mut img, HELLO_CLUSTER, FAT_EOC);

    // Root directory (cluster 2): one entry for HELLO.TXT.
    let hello_size = u32::try_from(PLANTED_FILE_CONTENT.len()).unwrap_or(0);
    write_short_entry(
        &mut img,
        cluster_byte_offset(ROOT_CLUSTER),
        &short_name(b"HELLO", b"TXT"),
        ATTR_ARCHIVE,
        HELLO_CLUSTER,
        hello_size,
    );

    // HELLO.TXT contents (cluster 3).
    let hello = cluster_byte_offset(HELLO_CLUSTER);
    img[hello..hello + PLANTED_FILE_CONTENT.len()].copy_from_slice(PLANTED_FILE_CONTENT);

    debug_assert_eq!(DIR_ENTRY_LEN, 32);
    img
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_abi::driver::block::{Block, BlockGeometry};
    use rustos_abi::driver::filesystem::{FilesystemRead, FilesystemWrite, NodeKind};
    use rustos_abi::DriverError;
    use rustos_drv_fs_fat32::Fat32;

    /// In-memory [`Block`] device over the built image, used to drive
    /// the real FAT32 driver on the host exactly as the guest drives it
    /// over virtio-blk.
    struct VecBlock {
        store: Vec<u8>,
    }

    impl VecBlock {
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
                block_size: u32::from(BYTES_PER_SECTOR),
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

    fn mount() -> Fat32<VecBlock> {
        let dev = VecBlock {
            store: build_image(),
        };
        Fat32::open(dev).expect("the built image is a valid FAT32 volume")
    }

    #[test]
    fn driver_mounts_the_built_image() {
        let _fs = mount();
    }

    #[test]
    fn planted_file_reads_back_its_known_contents() {
        let mut fs = mount();
        let root = fs.root();
        let node = fs
            .lookup(root, PLANTED_FILE_NAME)
            .expect("HELLO.TXT present");
        let mut buf = [0u8; 128];
        let n = fs.read_at(node, 0, &mut buf).expect("read HELLO.TXT");
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

        let node = fs
            .lookup(root, PLANTED_FILE_NAME)
            .expect("HELLO.TXT present");
        let mut buf = [0u8; 128];
        let n = fs.read_at(node, 0, &mut buf).expect("read HELLO.TXT");
        assert_eq!(&buf[..n], PLANTED_FILE_CONTENT);
    }

    #[test]
    fn image_is_exactly_the_advertised_size() {
        let expected =
            usize::try_from(TOTAL_SECTORS).expect("sector count fits usize") * SECTOR_BYTES;
        assert_eq!(build_image().len(), expected);
    }
}
