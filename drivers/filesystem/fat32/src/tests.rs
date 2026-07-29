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

extern crate std;

use super::*;
use tairix_abi::driver::block::BlockGeometry;
use tairix_abi::DriverKind;

const SECTOR_SIZE: usize = 512;
const SECTOR_COUNT: u32 = 16;
const IMG_LEN: usize = SECTOR_SIZE * (SECTOR_COUNT as usize);

const HELLO_BODY: &[u8] = b"Hello, TAIRiX FAT32!\n";
const DEEP_LEN: usize = 700;

const CLUSTER_ROOT: u32 = 2;
const CLUSTER_HELLO: u32 = 3;
const CLUSTER_SUB: u32 = 4;
const CLUSTER_DEEP_FIRST: u32 = 5;
const CLUSTER_DEEP_SECOND: u32 = 6;
const CLUSTER_LONG: u32 = 7;

/// Body of the long-named file `Greetings Café.txt`.
const LONG_BODY: &[u8] = b"long name body\n";

/// UTF-16 code units of `Greetings Café.txt` (the `é` is U+00E9), the
/// long name carried by the `GREETI~1.TXT` short entry in `SUB/`.
const LONG_NAME_UNITS: [u16; 18] = [
    0x0047, 0x0072, 0x0065, 0x0065, 0x0074, 0x0069, 0x006E, 0x0067, 0x0073, 0x0020, 0x0043, 0x0061,
    0x0066, 0x00E9, 0x002E, 0x0074, 0x0078, 0x0074,
];

/// The expected UTF-8 reconstruction of [`LONG_NAME_UNITS`].
const LONG_NAME_UTF8: &[u8] = b"Greetings Caf\xC3\xA9.txt";

/// The 8.3 short-name alias backing the long name.
fn long_short_name() -> [u8; 11] {
    short_name(b"GREETI~1", b"TXT")
}

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

/// Write one 32-byte long-name directory entry.
fn write_lfn_entry(
    img: &mut [u8],
    offset: usize,
    order: u8,
    checksum: u8,
    slots: &[u16; LFN_UNITS_PER_ENTRY],
) {
    img[offset] = order;
    img[offset + 11] = ATTR_LONG_NAME;
    img[offset + 12] = 0;
    img[offset + 13] = checksum;
    for (k, &char_offset) in LFN_CHAR_OFFSETS.iter().enumerate() {
        set_le16(img, offset + char_offset, slots[k]);
    }
}

/// Write a complete long-name set (physical entries in descending
/// sequence, the first flagged [`LFN_LAST_FLAG`]) followed by its 8.3
/// short entry, returning the number of directory bytes consumed.
///
/// `checksum` is written into every fragment; passing a value other
/// than the short-name checksum models a corrupt set so the fall-back
/// to the 8.3 name can be tested.
#[allow(clippy::too_many_arguments)]
fn write_long_named_entry(
    img: &mut [u8],
    offset: usize,
    units: &[u16],
    short: &[u8; 11],
    checksum: u8,
    attr: u8,
    cluster: u32,
    size: u32,
) -> usize {
    let frag_count = units.len().div_ceil(LFN_UNITS_PER_ENTRY);
    for phys in 0..frag_count {
        let seq = frag_count - phys;
        let mut order = u8::try_from(seq).expect("sequence fits in u8");
        if phys == 0 {
            order |= LFN_LAST_FLAG;
        }
        let base = (seq - 1) * LFN_UNITS_PER_ENTRY;
        let mut slots = [0xFFFFu16; LFN_UNITS_PER_ENTRY];
        for (k, slot) in slots.iter_mut().enumerate() {
            let idx = base + k;
            *slot = match idx.cmp(&units.len()) {
                core::cmp::Ordering::Less => units[idx],
                core::cmp::Ordering::Equal => 0x0000,
                core::cmp::Ordering::Greater => 0xFFFF,
            };
        }
        write_lfn_entry(img, offset + phys * DIR_ENTRY_LEN, order, checksum, &slots);
    }
    write_entry(
        img,
        offset + frag_count * DIR_ENTRY_LEN,
        short,
        attr,
        cluster,
        size,
    );
    (frag_count + 1) * DIR_ENTRY_LEN
}

/// Byte offset of the long-named file's first directory entry in `SUB/`
/// (after `.`, `..`, and `DEEP.BIN`).
fn long_entry_offset() -> usize {
    cluster_offset(CLUSTER_SUB) + 3 * DIR_ENTRY_LEN
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

    // `Greetings Café.txt` (`GREETI~1.TXT`): a two-fragment long name.
    let long_short = long_short_name();
    let long_size = u32::try_from(LONG_BODY.len()).expect("body fits in u32");
    write_long_named_entry(
        &mut img,
        sub + 3 * DIR_ENTRY_LEN,
        &LONG_NAME_UNITS,
        &long_short,
        short_name_checksum(&long_short),
        0x20,
        CLUSTER_LONG,
        long_size,
    );
    set_fat(&mut img, CLUSTER_LONG as usize, eoc);
    let long_body = cluster_offset(CLUSTER_LONG);
    img[long_body..long_body + LONG_BODY.len()].copy_from_slice(LONG_BODY);

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
        if len == 0 || !len.is_multiple_of(SECTOR_SIZE) {
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

    fn flush(&mut self) -> Result<(), DriverError> {
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
    assert_eq!(first.info.kind, NodeKind::RegularFile);

    let second = fs
        .read_dir(root, first.next_cursor, &mut name)
        .expect("ok")
        .expect("entry");
    assert_eq!(&name[..second.name_len], b"SUB");
    assert_eq!(second.info.kind, NodeKind::Directory);

    assert_eq!(fs.read_dir(root, second.next_cursor, &mut name), Ok(None));
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

#[test]
fn long_file_name_is_reconstructed_in_listing() {
    let mut fs = mount();
    let root = fs.root();
    let sub = fs.lookup(root, b"sub").expect("subdir");

    let mut name = [0u8; 64];
    // `.`/`..` are skipped, so the first entry is DEEP.BIN and the next is
    // the long-named file.
    let deep = fs.read_dir(sub, 0, &mut name).expect("ok").expect("entry");
    assert_eq!(&name[..deep.name_len], b"DEEP.BIN");

    let long = fs
        .read_dir(sub, deep.next_cursor, &mut name)
        .expect("ok")
        .expect("entry");
    assert_eq!(&name[..long.name_len], LONG_NAME_UTF8);
    assert_eq!(long.info.kind, NodeKind::RegularFile);

    assert_eq!(fs.read_dir(sub, long.next_cursor, &mut name), Ok(None));
}

#[test]
fn lookup_resolves_a_long_file_name_case_insensitively() {
    let mut fs = mount();
    let root = fs.root();
    let sub = fs.lookup(root, b"sub").expect("subdir");

    let by_long = fs.lookup(sub, LONG_NAME_UTF8).expect("found by long name");
    // The ASCII portion folds case; the non-ASCII `é` byte is compared
    // verbatim.
    let by_mixed = fs
        .lookup(sub, b"greetings caf\xC3\xA9.TXT")
        .expect("found case-insensitively");
    assert_eq!(by_long, by_mixed);

    let mut buf = [0u8; 32];
    let read = fs.read_at(by_long, 0, &mut buf).expect("read");
    assert_eq!(&buf[..read], LONG_BODY);
}

#[test]
fn long_name_supersedes_its_short_alias() {
    // Each entry exposes a single name: when a valid long name is
    // present it is the entry's name, and the internal 8.3 alias is not
    // separately resolvable (the VFS namespace uses the long name).
    let mut fs = mount();
    let root = fs.root();
    let sub = fs.lookup(root, b"sub").expect("subdir");

    assert!(fs.lookup(sub, LONG_NAME_UTF8).is_ok());
    assert_eq!(fs.lookup(sub, b"GREETI~1.TXT"), Err(DriverError::NotFound));
}

#[test]
fn corrupt_long_name_checksum_falls_back_to_short_name() {
    let mut data = build_image();
    // Overwrite the checksum byte (offset 13) of both long-name
    // fragments with a value that cannot match the short name.
    let first = long_entry_offset();
    data[first + 13] ^= 0xFF;
    data[first + DIR_ENTRY_LEN + 13] ^= 0xFF;

    let mut fs = Fat32::open(MockBlock { data }).expect("valid volume");
    let root = fs.root();
    let sub = fs.lookup(root, b"sub").expect("subdir");

    let mut name = [0u8; 64];
    let deep = fs.read_dir(sub, 0, &mut name).expect("ok").expect("entry");
    let entry = fs
        .read_dir(sub, deep.next_cursor, &mut name)
        .expect("ok")
        .expect("entry");
    assert_eq!(&name[..entry.name_len], b"GREETI~1.TXT");

    assert_eq!(fs.lookup(sub, LONG_NAME_UTF8), Err(DriverError::NotFound));
}

#[test]
fn read_dir_rejects_a_buffer_too_small_for_a_long_name() {
    let mut fs = mount();
    let root = fs.root();
    let sub = fs.lookup(root, b"sub").expect("subdir");

    let mut name = [0u8; 64];
    let deep = fs.read_dir(sub, 0, &mut name).expect("ok").expect("entry");
    let mut small = [0u8; LONG_NAME_UTF8.len() - 1];
    assert_eq!(
        fs.read_dir(sub, deep.next_cursor, &mut small),
        Err(DriverError::BufferTooSmall)
    );
}

#[test]
fn short_name_checksum_matches_the_specification() {
    // The reference checksum of the 11-byte field "GREETI~1TXT".
    let short = short_name(b"GREETI~1", b"TXT");
    let mut expected = 0u8;
    for &byte in &short {
        expected = expected.rotate_right(1).wrapping_add(byte);
    }
    assert_eq!(short_name_checksum(&short), expected);
}

#[test]
fn decode_utf16le_handles_a_surrogate_pair() {
    // U+1F600 GRINNING FACE encodes as the surrogate pair D83D DE00.
    let units = [0xD83Du16, 0xDE00];
    let mut out = [0u8; 8];
    let len = decode_utf16le(&units, &mut out).expect("decoded");
    assert_eq!(&out[..len], b"\xF0\x9F\x98\x80");
}

#[test]
fn decode_utf16le_stops_at_the_terminator() {
    let units = [0x0041u16, 0x0042, 0x0000, 0xFFFF, 0x0043];
    let mut out = [0u8; 8];
    let len = decode_utf16le(&units, &mut out).expect("decoded");
    assert_eq!(&out[..len], b"AB");
}

#[test]
fn decode_utf16le_rejects_unpaired_surrogates() {
    let mut out = [0u8; 8];
    assert_eq!(decode_utf16le(&[0xD83Du16], &mut out), None);
    assert_eq!(decode_utf16le(&[0xD83Du16, 0x0041], &mut out), None);
    assert_eq!(decode_utf16le(&[0xDC00u16], &mut out), None);
}

#[test]
fn decode_utf16le_rejects_an_overflowing_output() {
    let units = [0x0041u16, 0x0042, 0x0043];
    let mut out = [0u8; 2];
    assert_eq!(decode_utf16le(&units, &mut out), None);
}

// ---------------------------------------------------------------------
// Write-path tests (`FilesystemWrite`).
//
// They mutate the in-memory image through the same `MockBlock` and then
// read the result back through the `FilesystemRead` surface, so a
// round-trip exercises both halves of the driver.
// ---------------------------------------------------------------------

#[test]
fn create_then_write_and_read_back_a_short_named_file() {
    let mut fs = mount();
    let root = fs.root();
    fs.create(root, b"NOTES.TXT", NodeKind::RegularFile)
        .expect("create");

    let payload = b"hello world";
    let written = fs.write_at(root, b"NOTES.TXT", 0, payload).expect("write");
    assert_eq!(written, payload.len());

    // Re-resolve so the node carries the updated size, then read back.
    let node = fs.lookup(root, b"notes.txt").expect("lookup");
    assert_eq!(fs.node_info(node).expect("info").size, payload.len() as u64);
    let mut buf = [0u8; 32];
    let read = fs.read_at(node, 0, &mut buf).expect("read");
    assert_eq!(&buf[..read], payload);
}

#[test]
fn create_a_long_named_file_and_resolve_it() {
    let mut fs = mount();
    let root = fs.root();
    let name = b"My Long Document.txt";
    fs.create(root, name, NodeKind::RegularFile)
        .expect("create");
    let body = b"long content";
    fs.write_at(root, name, 0, body).expect("write");

    // Resolvable by the exact long name and case-insensitively.
    let exact = fs.lookup(root, name).expect("exact");
    let folded = fs.lookup(root, b"MY LONG DOCUMENT.TXT").expect("folded");
    assert_eq!(exact, folded);

    let mut buf = [0u8; 32];
    let read = fs.read_at(exact, 0, &mut buf).expect("read");
    assert_eq!(&buf[..read], body);
}

#[test]
fn write_extends_across_a_cluster_boundary() {
    let mut fs = mount();
    let root = fs.root();
    fs.create(root, b"BIG.BIN", NodeKind::RegularFile)
        .expect("create");

    let mut payload = [0u8; 600];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = deep_byte(i);
    }
    assert_eq!(fs.write_at(root, b"BIG.BIN", 0, &payload), Ok(600));

    let node = fs.lookup(root, b"BIG.BIN").expect("lookup");
    assert_eq!(fs.node_info(node).expect("info").size, 600);
    let mut buf = [0u8; 600];
    assert_eq!(fs.read_at(node, 0, &mut buf), Ok(600));
    assert_eq!(buf, payload);
}

#[test]
fn write_past_end_zero_fills_the_gap() {
    let mut fs = mount();
    let root = fs.root();
    fs.create(root, b"SPARSE.BIN", NodeKind::RegularFile)
        .expect("create");
    assert_eq!(fs.write_at(root, b"SPARSE.BIN", 5, b"XYZ"), Ok(3));

    let node = fs.lookup(root, b"SPARSE.BIN").expect("lookup");
    assert_eq!(fs.node_info(node).expect("info").size, 8);
    let mut buf = [0u8; 8];
    assert_eq!(fs.read_at(node, 0, &mut buf), Ok(8));
    assert_eq!(&buf, b"\0\0\0\0\0XYZ");
}

#[test]
fn truncate_shrinks_and_grows_a_file() {
    let mut fs = mount();
    let root = fs.root();
    fs.create(root, b"T.BIN", NodeKind::RegularFile)
        .expect("create");
    let mut payload = [0u8; 600];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = deep_byte(i);
    }
    fs.write_at(root, b"T.BIN", 0, &payload).expect("write");

    // Shrink to 100 bytes.
    fs.truncate(root, b"T.BIN", 100).expect("shrink");
    let node = fs.lookup(root, b"T.BIN").expect("lookup");
    assert_eq!(fs.node_info(node).expect("info").size, 100);
    let mut head = [0u8; 100];
    assert_eq!(fs.read_at(node, 0, &mut head), Ok(100));
    assert_eq!(&head[..], &payload[..100]);

    // Grow to 300 bytes; the new tail reads back as zero.
    fs.truncate(root, b"T.BIN", 300).expect("grow");
    let node = fs.lookup(root, b"T.BIN").expect("lookup");
    assert_eq!(fs.node_info(node).expect("info").size, 300);
    let mut all = [0xAAu8; 300];
    assert_eq!(fs.read_at(node, 0, &mut all), Ok(300));
    assert_eq!(&all[..100], &payload[..100]);
    assert!(all[100..].iter().all(|&b| b == 0));
}

#[test]
fn remove_unlinks_a_file_and_frees_the_name() {
    let mut fs = mount();
    let root = fs.root();
    fs.create(root, b"TMP.TXT", NodeKind::RegularFile)
        .expect("create");
    fs.write_at(root, b"TMP.TXT", 0, b"bye").expect("write");
    fs.remove(root, b"TMP.TXT").expect("remove");
    assert_eq!(fs.lookup(root, b"TMP.TXT"), Err(DriverError::NotFound));

    // The name (and its slots) can be reused immediately.
    fs.create(root, b"TMP.TXT", NodeKind::RegularFile)
        .expect("recreate");
    assert!(fs.lookup(root, b"TMP.TXT").is_ok());
}

#[test]
fn mkdir_then_create_a_file_inside() {
    let mut fs = mount();
    let root = fs.root();
    let dir = fs
        .create(root, b"Docs", NodeKind::Directory)
        .expect("mkdir");
    assert_eq!(fs.node_info(dir).expect("info").kind, NodeKind::Directory);

    // The directory is visible from the root.
    let resolved = fs.lookup(root, b"docs").expect("lookup dir");
    assert_eq!(resolved, dir);

    fs.create(dir, b"inner.txt", NodeKind::RegularFile)
        .expect("create inside");
    fs.write_at(dir, b"inner.txt", 0, b"nested").expect("write");

    let file = fs.lookup(dir, b"inner.txt").expect("lookup inside");
    let mut buf = [0u8; 16];
    let read = fs.read_at(file, 0, &mut buf).expect("read");
    assert_eq!(&buf[..read], b"nested");
}

#[test]
fn creating_an_existing_name_is_busy() {
    let mut fs = mount();
    let root = fs.root();
    fs.create(root, b"DUP.TXT", NodeKind::RegularFile)
        .expect("create");
    assert_eq!(
        fs.create(root, b"DUP.TXT", NodeKind::RegularFile),
        Err(DriverError::Busy)
    );
}

#[test]
fn writing_to_a_directory_is_unsupported() {
    let mut fs = mount();
    let root = fs.root();
    assert_eq!(
        fs.write_at(root, b"SUB", 0, b"x"),
        Err(DriverError::Unsupported)
    );
    assert_eq!(fs.truncate(root, b"SUB", 0), Err(DriverError::Unsupported));
}

#[test]
fn removing_a_non_empty_directory_is_busy() {
    let mut fs = mount();
    let root = fs.root();
    assert_eq!(fs.remove(root, b"SUB"), Err(DriverError::Busy));
}

#[test]
fn removing_an_empty_directory_succeeds() {
    let mut fs = mount();
    let root = fs.root();
    fs.create(root, b"Empty", NodeKind::Directory)
        .expect("mkdir");
    fs.remove(root, b"Empty").expect("rmdir");
    assert_eq!(fs.lookup(root, b"Empty"), Err(DriverError::NotFound));
}

#[test]
fn writing_a_missing_file_is_not_found() {
    let mut fs = mount();
    let root = fs.root();
    assert_eq!(
        fs.write_at(root, b"GHOST.TXT", 0, b"x"),
        Err(DriverError::NotFound)
    );
    assert_eq!(
        fs.truncate(root, b"GHOST.TXT", 0),
        Err(DriverError::NotFound)
    );
    assert_eq!(fs.remove(root, b"GHOST.TXT"), Err(DriverError::NotFound));
}

#[test]
fn flush_is_a_synchronous_no_op() {
    let mut fs = mount();
    assert!(fs.flush().is_ok());
}

// --- Allocated storage (`NodeInfo::allocated` from the FAT chain). ---

#[test]
fn node_info_reports_the_chain_allocation() {
    let mut fs = mount();
    let root = fs.root();
    // One 512-byte cluster each for the root directory and hello.txt;
    // deep.bin chains two clusters.
    assert_eq!(fs.node_info(root).expect("info").allocated, 512);
    let hello = fs.lookup(root, b"hello.txt").expect("found");
    assert_eq!(fs.node_info(hello).expect("info").allocated, 512);
    let sub = fs.lookup(root, b"sub").expect("subdir");
    let deep = fs.lookup(sub, b"deep.bin").expect("deep");
    assert_eq!(fs.node_info(deep).expect("info").allocated, 1024);
}

#[test]
fn node_info_reports_no_allocation_for_an_empty_file() {
    let mut fs = mount();
    let root = fs.root();
    let file = fs
        .create(root, b"empty.txt", NodeKind::RegularFile)
        .expect("create");
    // A fresh file has no first cluster, so no chain and no allocation.
    assert_eq!(fs.node_info(file).expect("info").allocated, 0);
}

/// The caller-minted BPB volume serial the format tests lay volumes down
/// with; production callers mint a fresh one per volume.
const TEST_SERIAL: u32 = 0x1234_5678;

/// Tests for [`Fat32::format`]: laying down a genuine, mountable FAT32
/// volume on a fresh device, sized large enough to be a real FAT32
/// filesystem, and the data-exhaustion (`NoSpace`) extreme. These use a
/// heap-backed device, so they live behind `std` like the round-trip
/// fixtures above.
mod format {
    use super::*;
    use std::vec;
    use std::vec::Vec;

    /// A heap-backed [`Block`] device of `block_count` 512-byte sectors,
    /// starting all-zero like a freshly attached disk.
    struct VecBlock {
        store: Vec<u8>,
    }

    impl VecBlock {
        fn new(block_count: u64) -> Self {
            let len = usize::try_from(block_count * SECTOR_SIZE as u64).expect("fits usize");
            Self {
                store: vec![0u8; len],
            }
        }

        fn span(&self, lba: u64, len: usize) -> Result<(usize, usize), DriverError> {
            if len == 0 || !len.is_multiple_of(SECTOR_SIZE) {
                return Err(DriverError::BufferTooSmall);
            }
            let start = usize::try_from(lba)
                .ok()
                .and_then(|l| l.checked_mul(SECTOR_SIZE))
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
                block_size: u32::try_from(SECTOR_SIZE).expect("512 fits u32"),
                block_count: (self.store.len() / SECTOR_SIZE) as u64,
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

        fn flush(&mut self) -> Result<(), DriverError> {
            Ok(())
        }
    }

    /// 64 MiB in 512-byte sectors — large enough to be a genuine FAT32
    /// volume (more than `MIN_FAT32_CLUSTERS` clusters) yet cheap to fill.
    const SECTORS_64MIB: u64 = (64 << 20) / 512;

    #[test]
    fn format_produces_a_mountable_empty_volume() {
        let mut fs = Fat32::format(VecBlock::new(SECTORS_64MIB), TEST_SERIAL).expect("format");
        let root = fs.root();
        assert_eq!(
            fs.node_info(root).expect("root info").kind,
            NodeKind::Directory
        );
        // A freshly formatted root directory is empty.
        let mut name = [0u8; MAX_NAME_BYTES];
        assert_eq!(fs.read_dir(root, 0, &mut name), Ok(None));
    }

    #[test]
    fn format_lays_out_fsinfo_and_the_backup_boot_pair() {
        // Regression: the formatter used to leave BPB offsets 48/50 zero
        // and write no FSInfo structure at all — a format-conformance
        // defect that also left a TAIRiX-formatted volume without the
        // mutation-evidence window the verified re-insert path compares
        // (`plans/DEVICES.md` D4c).
        let fs = Fat32::format(VecBlock::new(SECTORS_64MIB), TEST_SERIAL).expect("format");
        let image = fs.into_block();
        let boot = &image.store[..512];
        let fsinfo_sector = u16::from_le_bytes([boot[48], boot[49]]) as usize;
        let backup_sector = u16::from_le_bytes([boot[50], boot[51]]) as usize;
        assert_eq!(fsinfo_sector, 1);
        assert_eq!(backup_sector, 6);

        // Both FSInfo copies carry the structure's three signatures and
        // the documented "unknown" hints (this driver counts free space
        // from the FAT itself).
        for base in [fsinfo_sector * 512, (backup_sector + 1) * 512] {
            let info = &image.store[base..base + 512];
            assert_eq!(&info[0..4], &0x4161_5252u32.to_le_bytes());
            assert_eq!(&info[484..488], &0x6141_7272u32.to_le_bytes());
            assert_eq!(&info[488..492], &0xFFFF_FFFFu32.to_le_bytes());
            assert_eq!(&info[492..496], &0xFFFF_FFFFu32.to_le_bytes());
            assert_eq!(&info[508..512], &0xAA55_0000u32.to_le_bytes());
        }
        // The backup boot sector is byte-identical to the primary.
        assert_eq!(
            &image.store[backup_sector * 512..backup_sector * 512 + 512],
            boot
        );
        // The formatted head now declares an evidence window covering the
        // boot sector through FSInfo.
        assert_eq!(
            tairix_fsprobe::evidence_len(&image.store[..tairix_fsprobe::PROBE_HEAD_LEN]),
            Some(((fsinfo_sector as u64) + 1) * 512)
        );
    }

    #[test]
    fn format_then_reopen_round_trips_files_and_directories() {
        let dev = {
            let mut fs = Fat32::format(VecBlock::new(SECTORS_64MIB), TEST_SERIAL).expect("format");
            let root = fs.root();
            fs.create(root, b"Notes.txt", NodeKind::RegularFile)
                .expect("create file");
            fs.write_at(root, b"Notes.txt", 0, b"formatted on TAIRiX")
                .expect("write file");
            let dir = fs
                .create(root, b"Folder", NodeKind::Directory)
                .expect("mkdir");
            fs.create(dir, b"inner.bin", NodeKind::RegularFile)
                .expect("create nested");
            fs.write_at(dir, b"inner.bin", 0, b"nested body")
                .expect("write nested");
            fs.into_block()
        };

        // Re-mount the very bytes the formatter and writes produced.
        let mut fs = Fat32::open(dev).expect("reopen formatted volume");
        let root = fs.root();
        let file = fs.lookup(root, b"Notes.txt").expect("file present");
        let mut buf = [0u8; 32];
        let n = fs.read_at(file, 0, &mut buf).expect("read file");
        assert_eq!(&buf[..n], b"formatted on TAIRiX");

        let dir = fs.lookup(root, b"Folder").expect("dir present");
        let nested = fs.lookup(dir, b"inner.bin").expect("nested present");
        let n = fs.read_at(nested, 0, &mut buf).expect("read nested");
        assert_eq!(&buf[..n], b"nested body");
    }

    #[test]
    fn stats_track_allocation_and_identity_is_remount_stable() {
        let mut fs = Fat32::format(VecBlock::new(SECTORS_64MIB), TEST_SERIAL).expect("format");
        let identity = fs.volume_identity();
        assert_ne!(
            identity, [0u8; 16],
            "the tag byte keeps the identity non-nil"
        );

        let before = fs.stats().expect("stats");
        assert!(before.total_blocks > 0);
        assert!(before.free_blocks <= before.total_blocks);
        assert_eq!(before.avail_blocks, before.free_blocks);
        // FAT32 has no inode table; the zero pair reports that honestly.
        assert_eq!((before.files, before.files_free), (0, 0));

        // Allocating a multi-cluster body drops the maintained count;
        // removing it restores the exact figure.
        let root = fs.root();
        fs.create(root, b"stats.bin", NodeKind::RegularFile)
            .expect("create");
        let body = std::vec![0x5Au8; 40_000];
        assert_eq!(fs.write_at(root, b"stats.bin", 0, &body), Ok(body.len()));
        let after = fs.stats().expect("stats");
        assert!(after.free_blocks < before.free_blocks);
        fs.remove(root, b"stats.bin").expect("remove");
        let restored = fs.stats().expect("stats");
        assert_eq!(restored.free_blocks, before.free_blocks);

        // A fresh open's full FAT scan agrees with the maintained count,
        // and the identity survives the remount.
        let mut fs = Fat32::open(fs.into_block()).expect("reopen");
        assert_eq!(fs.stats().expect("stats").free_blocks, before.free_blocks);
        assert_eq!(fs.volume_identity(), identity);
    }

    #[test]
    fn security_is_uniform_and_stores_are_refused() {
        let mut fs = Fat32::format(VecBlock::new(SECTORS_64MIB), TEST_SERIAL).expect("format");
        let root = fs.root();
        fs.create(root, b"file.txt", NodeKind::RegularFile)
            .expect("create");
        let file = fs.lookup(root, b"file.txt").expect("lookup");

        // The format stores no owner model: one uniform, restrictive,
        // system-owned record per kind, never fabricated per-file
        // ownership.
        let dir_sec = fs.security(root).expect("dir security");
        assert_eq!((dir_sec.mode, dir_sec.uid, dir_sec.gid), (0o755, 0, 0));
        assert!(dir_sec.required_cap.is_none());
        let file_sec = fs.security(file).expect("file security");
        assert_eq!((file_sec.mode, file_sec.uid, file_sec.gid), (0o644, 0, 0));

        // A store would be silently lossy, so it is refused whole.
        assert_eq!(
            fs.set_security(file, NodeSecurity::new(0o600, 1000, 100)),
            Err(DriverError::Unsupported)
        );
    }

    #[test]
    fn filling_a_formatted_volume_reports_no_space() {
        let mut fs = Fat32::format(VecBlock::new(SECTORS_64MIB), TEST_SERIAL).expect("format");
        let root = fs.root();
        fs.create(root, b"BIG.DAT", NodeKind::RegularFile)
            .expect("create");
        // Append 1 MiB at a time until the data region is exhausted. The
        // first failure must be NoSpace (the volume is full), never a
        // DeviceFault, and the volume must fill well before any absurd size.
        let chunk = vec![0xCDu8; 1 << 20];
        let mut offset = 0u64;
        let mut result = Ok(0usize);
        while offset < (128 << 20) {
            result = fs.write_at(root, b"BIG.DAT", offset, &chunk);
            match result {
                Ok(n) => offset += n as u64,
                Err(_) => break,
            }
        }
        assert_eq!(result, Err(DriverError::NoSpace));
        // Something substantial was stored before the volume filled.
        assert!(
            offset >= (32 << 20),
            "only {offset} bytes written before full"
        );
    }

    #[test]
    fn rename_within_directory_preserves_contents() {
        let mut fs = Fat32::format(VecBlock::new(SECTORS_64MIB), TEST_SERIAL).expect("format");
        let root = fs.root();
        fs.create(root, b"A.TXT", NodeKind::RegularFile).unwrap();
        fs.write_at(root, b"A.TXT", 0, b"hello").unwrap();
        fs.rename(root, b"A.TXT", root, b"B.TXT").expect("rename");
        assert_eq!(fs.lookup(root, b"A.TXT"), Err(DriverError::NotFound));
        let node = fs.lookup(root, b"B.TXT").expect("dst");
        let mut buf = [0u8; 8];
        let n = fs.read_at(node, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello");
    }

    #[test]
    fn rename_missing_source_is_not_found() {
        let mut fs = Fat32::format(VecBlock::new(SECTORS_64MIB), TEST_SERIAL).expect("format");
        let root = fs.root();
        assert_eq!(
            fs.rename(root, b"X", root, b"Y"),
            Err(DriverError::NotFound)
        );
    }

    #[test]
    fn rename_across_directories_persists() {
        let dev = {
            let mut fs = Fat32::format(VecBlock::new(SECTORS_64MIB), TEST_SERIAL).expect("format");
            let root = fs.root();
            let src = fs.create(root, b"SRC", NodeKind::Directory).unwrap();
            let dst = fs.create(root, b"DST", NodeKind::Directory).unwrap();
            fs.create(src, b"F.BIN", NodeKind::RegularFile).unwrap();
            fs.write_at(src, b"F.BIN", 0, b"data").unwrap();
            fs.rename(src, b"F.BIN", dst, b"G.BIN").expect("move");
            fs.into_block()
        };
        let mut fs = Fat32::open(dev).expect("reopen");
        let root = fs.root();
        let src = fs.lookup(root, b"SRC").unwrap();
        let dst = fs.lookup(root, b"DST").unwrap();
        assert_eq!(fs.lookup(src, b"F.BIN"), Err(DriverError::NotFound));
        let node = fs.lookup(dst, b"G.BIN").expect("moved");
        let mut buf = [0u8; 8];
        let n = fs.read_at(node, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"data");
    }

    #[test]
    fn rename_overwrites_existing_file() {
        let mut fs = Fat32::format(VecBlock::new(SECTORS_64MIB), TEST_SERIAL).expect("format");
        let root = fs.root();
        fs.create(root, b"A.TXT", NodeKind::RegularFile).unwrap();
        fs.write_at(root, b"A.TXT", 0, b"AAAA").unwrap();
        fs.create(root, b"B.TXT", NodeKind::RegularFile).unwrap();
        fs.write_at(root, b"B.TXT", 0, b"BB").unwrap();
        fs.rename(root, b"A.TXT", root, b"B.TXT")
            .expect("overwrite");
        assert_eq!(fs.lookup(root, b"A.TXT"), Err(DriverError::NotFound));
        let node = fs.lookup(root, b"B.TXT").unwrap();
        let mut buf = [0u8; 8];
        let n = fs.read_at(node, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"AAAA");
    }

    #[test]
    fn rename_refuses_kind_mismatch_and_nonempty_dir_target() {
        let mut fs = Fat32::format(VecBlock::new(SECTORS_64MIB), TEST_SERIAL).expect("format");
        let root = fs.root();
        fs.create(root, b"F.TXT", NodeKind::RegularFile).unwrap();
        fs.create(root, b"D", NodeKind::Directory).unwrap();
        assert_eq!(
            fs.rename(root, b"F.TXT", root, b"D"),
            Err(DriverError::Unsupported)
        );
        assert_eq!(
            fs.rename(root, b"D", root, b"F.TXT"),
            Err(DriverError::Unsupported)
        );
        let d2 = fs.create(root, b"D2", NodeKind::Directory).unwrap();
        fs.create(d2, b"CHILD", NodeKind::RegularFile).unwrap();
        assert_eq!(fs.rename(root, b"D", root, b"D2"), Err(DriverError::Busy));
    }

    #[test]
    fn rename_moves_a_directory_across_parents() {
        let dev = {
            let mut fs = Fat32::format(VecBlock::new(SECTORS_64MIB), TEST_SERIAL).expect("format");
            let root = fs.root();
            let p1 = fs.create(root, b"P1", NodeKind::Directory).unwrap();
            let p2 = fs.create(root, b"P2", NodeKind::Directory).unwrap();
            let d = fs.create(p1, b"D", NodeKind::Directory).unwrap();
            fs.create(d, b"LEAF.BIN", NodeKind::RegularFile).unwrap();
            fs.write_at(d, b"LEAF.BIN", 0, b"x").unwrap();
            fs.rename(p1, b"D", p2, b"D").expect("move dir");
            fs.into_block()
        };
        let mut fs = Fat32::open(dev).expect("reopen");
        let root = fs.root();
        let p1 = fs.lookup(root, b"P1").unwrap();
        let p2 = fs.lookup(root, b"P2").unwrap();
        assert_eq!(fs.lookup(p1, b"D"), Err(DriverError::NotFound));
        let moved = fs.lookup(p2, b"D").expect("moved");
        let leaf = fs.lookup(moved, b"LEAF.BIN").expect("leaf intact");
        let mut buf = [0u8; 4];
        let n = fs.read_at(leaf, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"x");
    }

    #[test]
    fn rename_refuses_moving_directory_into_its_subtree() {
        let mut fs = Fat32::format(VecBlock::new(SECTORS_64MIB), TEST_SERIAL).expect("format");
        let root = fs.root();
        let a = fs.create(root, b"A", NodeKind::Directory).unwrap();
        let b = fs.create(a, b"B", NodeKind::Directory).unwrap();
        assert_eq!(fs.rename(root, b"A", b, b"A"), Err(DriverError::Busy));
        assert_eq!(fs.rename(root, b"A", a, b"X"), Err(DriverError::Busy));
    }

    #[test]
    fn rename_rejects_bad_destination_name() {
        let mut fs = Fat32::format(VecBlock::new(SECTORS_64MIB), TEST_SERIAL).expect("format");
        let root = fs.root();
        fs.create(root, b"A.TXT", NodeKind::RegularFile).unwrap();
        assert_eq!(
            fs.rename(root, b"A.TXT", root, b""),
            Err(DriverError::LengthOutOfRange)
        );
        assert_eq!(
            fs.rename(root, b"A.TXT", root, b".."),
            Err(DriverError::Unsupported)
        );
    }

    #[test]
    fn format_rejects_a_device_too_small_for_fat32() {
        // 8 MiB is far below the FAT32 minimum cluster count.
        let too_small = (8 << 20) / 512;
        assert_eq!(
            Fat32::format(VecBlock::new(too_small), TEST_SERIAL).map(|_| ()),
            Err(DriverError::OutOfRange)
        );
    }

    #[test]
    fn format_refuses_a_zero_serial() {
        // The all-zero serial is the "none recorded" value: laying it
        // down would give every fresh volume one shared identity.
        assert_eq!(
            Fat32::format(VecBlock::new(SECTORS_64MIB), 0).map(|_| ()),
            Err(DriverError::OutOfRange)
        );
    }

    #[test]
    fn distinct_serials_yield_distinct_identities() {
        let a = Fat32::format(VecBlock::new(SECTORS_64MIB), 0x0000_0001).expect("format a");
        let b = Fat32::format(VecBlock::new(SECTORS_64MIB), 0x0000_0002).expect("format b");
        assert_ne!(
            a.volume_identity(),
            b.volume_identity(),
            "the caller-minted serial distinguishes otherwise identical volumes"
        );
    }
}

/// Pack a DOS date word from its calendar fields.
fn dos_date(year: u16, month: u16, day: u16) -> u16 {
    ((year - 1980) << 9) | (month << 5) | day
}

/// Pack a DOS time word from its clock fields.
fn dos_time(hour: u16, minute: u16, second: u16) -> u16 {
    (hour << 11) | (minute << 5) | (second / 2)
}

#[test]
fn dos_datetime_start_of_epoch_decodes() {
    // 1980-01-01 00:00:00 UTC, the earliest FAT timestamp.
    assert_eq!(
        crate::dos_datetime_to_time64(dos_date(1980, 1, 1), dos_time(0, 0, 0)),
        Time64::from_secs(315_532_800)
    );
}

#[test]
fn dos_datetime_leap_day_decodes() {
    // 2020-02-29 00:00:00 UTC — a valid leap day.
    assert_eq!(
        crate::dos_datetime_to_time64(dos_date(2020, 2, 29), dos_time(0, 0, 0)),
        Time64::from_secs(1_582_934_400)
    );
}

#[test]
fn dos_datetime_end_of_range_decodes_past_2038() {
    // 2107-12-31 23:59:58 UTC, the last representable FAT instant — far
    // beyond the 32-bit 2038 boundary, so the wide decode must not wrap.
    assert_eq!(
        crate::dos_datetime_to_time64(dos_date(2107, 12, 31), dos_time(23, 59, 58)),
        Time64::from_secs(4_354_819_198)
    );
}

#[test]
fn dos_datetime_undecodable_fields_report_no_stamp() {
    let ok_time = dos_time(12, 0, 0);
    let ok_date = dos_date(2024, 6, 15);
    // Month 0 and 13, day 0, a non-leap February 29th, and hour 24 are not
    // calendar instants; each reports the "no stamp" epoch, never a guess.
    assert_eq!(
        crate::dos_datetime_to_time64((44 << 9) | 15, ok_time),
        Time64::UNIX_EPOCH
    );
    assert_eq!(
        crate::dos_datetime_to_time64((44 << 9) | (13 << 5) | 15, ok_time),
        Time64::UNIX_EPOCH
    );
    assert_eq!(
        crate::dos_datetime_to_time64(dos_date(2024, 6, 0), ok_time),
        Time64::UNIX_EPOCH
    );
    assert_eq!(
        crate::dos_datetime_to_time64(dos_date(2023, 2, 29), ok_time),
        Time64::UNIX_EPOCH
    );
    assert_eq!(
        crate::dos_datetime_to_time64(ok_date, 24 << 11),
        Time64::UNIX_EPOCH
    );
}
