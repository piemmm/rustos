//! FAT32 read-only driver unit tests against an in-memory image.
//!
//! The test image is a hand-built, specification-shaped FAT32 volume
//! held in a fixed array (the crate is `no_std` and the tests stay
//! allocation-free), driven through a [`MockBlock`] device:
//!
//! ```text
//! /
//! ├── HELLO.TXT        (one cluster)
//! └── SUB/             (one cluster; carries `.`/`..`)
//!     └── DEEP.BIN      (two clusters — exercises chain following)
//! ```

use super::*;
use rustos_abi::driver::block::BlockGeometry;
use rustos_abi::DriverKind;

const SECTOR_SIZE: usize = 512;
const SECTOR_COUNT: u32 = 16;
const IMG_LEN: usize = SECTOR_SIZE * (SECTOR_COUNT as usize);

const HELLO_BODY: &[u8] = b"Hello, RustOS FAT32!\n";
const DEEP_LEN: usize = 700;

const CLUSTER_ROOT: u32 = 2;
const CLUSTER_HELLO: u32 = 3;
const CLUSTER_SUB: u32 = 4;
const CLUSTER_DEEP_FIRST: u32 = 5;
const CLUSTER_DEEP_SECOND: u32 = 6;

fn set_le16(img: &mut [u8], offset: usize, value: u16) {
    img[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn set_le32(img: &mut [u8], offset: usize, value: u32) {
    img[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn set_fat(img: &mut [u8], cluster: usize, value: u32) {
    set_le32(img, SECTOR_SIZE + cluster * 4, value);
}

/// Build an 11-byte 8.3 short name from base and extension fields.
fn short_name(base: &[u8], ext: &[u8]) -> [u8; 11] {
    let mut out = [b' '; 11];
    out[..base.len()].copy_from_slice(base);
    out[8..8 + ext.len()].copy_from_slice(ext);
    out
}

/// Byte offset of a cluster's data. In this image cluster `n` lives at
/// absolute sector `n` (reserved + FAT occupy sectors 0 and 1).
fn cluster_offset(cluster: u32) -> usize {
    (cluster as usize) * SECTOR_SIZE
}

fn write_entry(img: &mut [u8], offset: usize, name: &[u8; 11], attr: u8, cluster: u32, size: u32) {
    img[offset..offset + 11].copy_from_slice(name);
    img[offset + 11] = attr;
    let cluster_bytes = cluster.to_le_bytes();
    img[offset + 20..offset + 22].copy_from_slice(&cluster_bytes[2..4]);
    img[offset + 26..offset + 28].copy_from_slice(&cluster_bytes[0..2]);
    set_le32(img, offset + 28, size);
}

fn deep_byte(index: usize) -> u8 {
    u8::try_from(index % 256).expect("index % 256 fits in u8")
}

/// Construct the in-memory FAT32 image described in the module docs.
fn build_image() -> [u8; IMG_LEN] {
    let mut img = [0u8; IMG_LEN];

    // BIOS parameter block (boot sector).
    set_le16(&mut img, 11, 512); // bytes per sector
    img[13] = 1; // sectors per cluster
    set_le16(&mut img, 14, 1); // reserved sectors
    img[16] = 1; // number of FATs
    set_le16(&mut img, 17, 0); // root entry count (0 for FAT32)
    set_le16(&mut img, 22, 0); // 16-bit FAT size (0 for FAT32)
    set_le32(&mut img, 32, SECTOR_COUNT); // total sectors (32-bit)
    set_le32(&mut img, 36, 1); // 32-bit FAT size (sectors)
    set_le32(&mut img, 44, CLUSTER_ROOT); // root cluster
    img[510] = 0x55;
    img[511] = 0xAA;

    // File allocation table (single FAT in sector 1).
    let eoc = 0x0FFF_FFFF;
    set_fat(&mut img, 0, 0x0FFF_FFF8); // media descriptor
    set_fat(&mut img, 1, eoc);
    set_fat(&mut img, CLUSTER_ROOT as usize, eoc);
    set_fat(&mut img, CLUSTER_HELLO as usize, eoc);
    set_fat(&mut img, CLUSTER_SUB as usize, eoc);
    set_fat(&mut img, CLUSTER_DEEP_FIRST as usize, CLUSTER_DEEP_SECOND);
    set_fat(&mut img, CLUSTER_DEEP_SECOND as usize, eoc);

    // Root directory (cluster 2).
    let root = cluster_offset(CLUSTER_ROOT);
    let hello_size = u32::try_from(HELLO_BODY.len()).expect("body fits in u32");
    write_entry(
        &mut img,
        root,
        &short_name(b"HELLO", b"TXT"),
        0x20,
        CLUSTER_HELLO,
        hello_size,
    );
    write_entry(
        &mut img,
        root + DIR_ENTRY_LEN,
        &short_name(b"SUB", b""),
        ATTR_DIRECTORY,
        CLUSTER_SUB,
        0,
    );

    // HELLO.TXT contents (cluster 3).
    let hello = cluster_offset(CLUSTER_HELLO);
    img[hello..hello + HELLO_BODY.len()].copy_from_slice(HELLO_BODY);

    // SUB directory (cluster 4): `.`, `..`, then DEEP.BIN.
    let sub = cluster_offset(CLUSTER_SUB);
    write_entry(
        &mut img,
        sub,
        &short_name(b".", b""),
        ATTR_DIRECTORY,
        CLUSTER_SUB,
        0,
    );
    write_entry(
        &mut img,
        sub + DIR_ENTRY_LEN,
        &short_name(b"..", b""),
        ATTR_DIRECTORY,
        0,
        0,
    );
    let deep_size = u32::try_from(DEEP_LEN).expect("deep fits in u32");
    write_entry(
        &mut img,
        sub + 2 * DIR_ENTRY_LEN,
        &short_name(b"DEEP", b"BIN"),
        0x20,
        CLUSTER_DEEP_FIRST,
        deep_size,
    );

    // DEEP.BIN contents span clusters 5 and 6.
    let first = cluster_offset(CLUSTER_DEEP_FIRST);
    for i in 0..SECTOR_SIZE {
        img[first + i] = deep_byte(i);
    }
    let second = cluster_offset(CLUSTER_DEEP_SECOND);
    for i in SECTOR_SIZE..DEEP_LEN {
        img[second + (i - SECTOR_SIZE)] = deep_byte(i);
    }

    img
}

/// A fixed-size in-memory [`Block`] device over a FAT32 image.
struct MockBlock {
    data: [u8; IMG_LEN],
}

impl MockBlock {
    fn span(lba: u64, len: usize) -> Result<(usize, usize), DriverError> {
        if len == 0 || len % SECTOR_SIZE != 0 {
            return Err(DriverError::BufferTooSmall);
        }
        let start = usize::try_from(lba)
            .map_err(|_| DriverError::LengthOutOfRange)?
            .saturating_mul(SECTOR_SIZE);
        let end = start
            .checked_add(len)
            .ok_or(DriverError::LengthOutOfRange)?;
        if end > IMG_LEN {
            return Err(DriverError::LengthOutOfRange);
        }
        Ok((start, end))
    }
}

impl Block for MockBlock {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        Ok(BlockGeometry {
            block_size: 512,
            block_count: u64::from(SECTOR_COUNT),
        })
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        let (start, end) = Self::span(lba, buf.len())?;
        buf.copy_from_slice(&self.data[start..end]);
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        let (start, end) = Self::span(lba, buf.len())?;
        self.data[start..end].copy_from_slice(buf);
        Ok(())
    }
}

/// Mock driver host modelling the load-time `CAP_DRV_LOAD` grant.
struct MockHost {
    drv_load: bool,
}

impl DriverHost for MockHost {
    fn has_capability(&self, cap: CapabilityId) -> bool {
        matches!(cap, CapabilityId::DRV_LOAD if self.drv_load)
    }

    fn kind(&self) -> DriverKind {
        DriverKind::UserSpace
    }
}

fn mount() -> Fat32<MockBlock> {
    Fat32::open(MockBlock {
        data: build_image(),
    })
    .expect("image is a valid FAT32 volume")
}

#[test]
fn register_requires_drv_load() {
    assert!(register(&MockHost { drv_load: true }).is_ok());
    assert_eq!(
        register(&MockHost { drv_load: false }),
        Err(DriverError::PermissionDenied)
    );
}

#[test]
fn open_reports_directory_root() {
    let mut fs = mount();
    let root = fs.root();
    assert_eq!(fs.node_info(root).expect("info").kind, NodeKind::Directory);
}

#[test]
fn root_directory_lists_its_entries_in_order() {
    let mut fs = mount();
    let root = fs.root();
    let mut name = [0u8; 16];

    let first = fs.read_dir(root, 0, &mut name).expect("ok").expect("entry");
    assert_eq!(&name[..first.name_len], b"HELLO.TXT");
    assert_eq!(first.kind, NodeKind::RegularFile);

    let second = fs.read_dir(root, 1, &mut name).expect("ok").expect("entry");
    assert_eq!(&name[..second.name_len], b"SUB");
    assert_eq!(second.kind, NodeKind::Directory);

    assert_eq!(fs.read_dir(root, 2, &mut name), Ok(None));
}

#[test]
fn lookup_is_case_insensitive_and_reads_file_contents() {
    let mut fs = mount();
    let root = fs.root();
    let file = fs.lookup(root, b"hello.txt").expect("found");
    let info = fs.node_info(file).expect("info");
    assert_eq!(info.kind, NodeKind::RegularFile);
    assert_eq!(info.size, u64::try_from(HELLO_BODY.len()).unwrap());

    let mut buf = [0u8; 64];
    let read = fs.read_at(file, 0, &mut buf).expect("read");
    assert_eq!(&buf[..read], HELLO_BODY);
}

#[test]
fn read_follows_the_cluster_chain_across_clusters() {
    let mut fs = mount();
    let root = fs.root();
    let sub = fs.lookup(root, b"sub").expect("subdir");
    assert_eq!(fs.node_info(sub).expect("info").kind, NodeKind::Directory);

    let deep = fs.lookup(sub, b"deep.bin").expect("deep");
    assert_eq!(fs.node_info(deep).expect("info").size, DEEP_LEN as u64);

    let mut buf = [0u8; DEEP_LEN];
    let read = fs.read_at(deep, 0, &mut buf).expect("read");
    assert_eq!(read, DEEP_LEN);
    for (i, byte) in buf.iter().enumerate() {
        assert_eq!(*byte, deep_byte(i));
    }
}

#[test]
fn read_window_straddling_a_cluster_boundary() {
    let mut fs = mount();
    let root = fs.root();
    let sub = fs.lookup(root, b"sub").expect("subdir");
    let deep = fs.lookup(sub, b"deep.bin").expect("deep");

    let mut window = [0u8; 20];
    let read = fs.read_at(deep, 502, &mut window).expect("read");
    assert_eq!(read, 20);
    for (k, byte) in window.iter().enumerate() {
        assert_eq!(*byte, deep_byte(502 + k));
    }
}

#[test]
fn read_at_or_past_eof_yields_zero() {
    let mut fs = mount();
    let root = fs.root();
    let file = fs.lookup(root, b"HELLO.TXT").expect("found");
    let size = u64::try_from(HELLO_BODY.len()).unwrap();
    let mut buf = [0u8; 8];
    assert_eq!(fs.read_at(file, size, &mut buf), Ok(0));
    assert_eq!(fs.read_at(file, size + 100, &mut buf), Ok(0));
}

#[test]
fn read_at_with_offset_returns_tail() {
    let mut fs = mount();
    let root = fs.root();
    let file = fs.lookup(root, b"HELLO.TXT").expect("found");
    let mut buf = [0u8; 5];
    let read = fs.read_at(file, 7, &mut buf).expect("read");
    assert_eq!(&buf[..read], &HELLO_BODY[7..7 + read]);
}

#[test]
fn lookup_missing_name_is_not_found() {
    let mut fs = mount();
    let root = fs.root();
    assert_eq!(fs.lookup(root, b"NOPE.TXT"), Err(DriverError::NotFound));
}

#[test]
fn directory_operations_on_a_file_are_unsupported() {
    let mut fs = mount();
    let root = fs.root();
    let file = fs.lookup(root, b"HELLO.TXT").expect("found");
    assert_eq!(fs.lookup(file, b"X"), Err(DriverError::Unsupported));
    let mut name = [0u8; 16];
    assert_eq!(
        fs.read_dir(file, 0, &mut name),
        Err(DriverError::Unsupported)
    );
}

#[test]
fn reading_a_directory_as_a_file_is_unsupported() {
    let mut fs = mount();
    let root = fs.root();
    let mut buf = [0u8; 8];
    assert_eq!(fs.read_at(root, 0, &mut buf), Err(DriverError::Unsupported));
}

#[test]
fn read_dir_rejects_a_name_buffer_that_is_too_small() {
    let mut fs = mount();
    let root = fs.root();
    let mut tiny = [0u8; 3];
    assert_eq!(
        fs.read_dir(root, 0, &mut tiny),
        Err(DriverError::BufferTooSmall)
    );
}

#[test]
fn open_rejects_a_bad_boot_signature() {
    let mut data = build_image();
    data[510] = 0;
    assert_eq!(
        Fat32::open(MockBlock { data }).err(),
        Some(DriverError::BadMagic)
    );
}

#[test]
fn open_rejects_a_non_fat32_volume() {
    let mut data = build_image();
    // A non-zero 16-bit root-entry count is the FAT12/FAT16 shape.
    data[17] = 16;
    assert_eq!(
        Fat32::open(MockBlock { data }).err(),
        Some(DriverError::BadMagic)
    );
}

#[test]
fn into_block_returns_the_backing_device() {
    let fs = mount();
    let block = fs.into_block();
    assert_eq!(block.geometry().expect("geo").block_count, 16);
}
