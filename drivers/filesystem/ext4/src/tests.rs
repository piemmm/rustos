//! ext4 read-only driver unit tests against a hand-built in-memory
//! image.
//!
//! The image is a specification-shaped ext4 volume held in a fixed
//! array (the crate is `no_std`, so the tests stay allocation-free),
//! driven through a [`MockBlock`] device. Block size is 1024, one block
//! group, 128-byte inodes, the `filetype` feature on:
//!
//! ```text
//! /                       (inode 2,  extent-mapped directory)
//! ├── hello.txt           (inode 11, extent-mapped regular file)
//! ├── classic.bin         (inode 12, block-mapped: direct + holes +
//! │                        single indirect)
//! └── sub/                (inode 13, extent-mapped directory)
//!     └── deep.bin         (inode 14, extent-mapped regular file)
//! ```

extern crate alloc;

use super::*;
use alloc::vec;
use alloc::vec::Vec;
use rustos_abi::driver::block::BlockGeometry;
use rustos_abi::DriverKind;

const FS_BLOCK: usize = 1024;
const FS_BLOCK_COUNT: usize = 40;
const IMG_LEN: usize = FS_BLOCK * FS_BLOCK_COUNT;

const DEV_SECTOR: usize = 512;
const DEV_SECTOR_COUNT: u64 = (IMG_LEN / DEV_SECTOR) as u64;

const BLOCK_BITMAP_BLOCK: usize = 3;
const INODE_BITMAP_BLOCK: usize = 4;
const INODE_TABLE_BLOCK: usize = 5;
const INODES_PER_GROUP: u32 = 16;
const INODE_SIZE: usize = 128;

/// The first data block that starts the run of free space the write
/// tests allocate from (blocks `0..=14` are metadata or planted data).
const FIRST_FREE_BLOCK: usize = 15;
/// Number of free data blocks the fixture leaves (`15..40`).
const FREE_BLOCKS: usize = FS_BLOCK_COUNT - FIRST_FREE_BLOCK;
/// Inodes `1..=14` are in use (reserved + the four planted files /
/// directories); inodes `15` and `16` are free.
const FREE_INODES: usize = 2;

const ROOT_DATA_BLOCK: u32 = 7;
const HELLO_DATA_BLOCK: u32 = 8;
const SUB_DATA_BLOCK: u32 = 9;
const DEEP_DATA_BLOCK: u32 = 10;

const HELLO_BODY: &[u8] = b"Hello from ext4 via extents!\n";
const DEEP_BODY: &[u8] = b"deep file body in a subdirectory\n";

/// Owner of `hello.txt`. Both ids span the low (`i_uid`/`i_gid`) and
/// high (osd2 `l_i_*_high`) halves so the combined decode is exercised.
const HELLO_UID: u32 = 0x0001_2345;
const HELLO_GID: u32 = 0x0002_6789;

/// The classic-mapped file spans 13 logical blocks. Only logical blocks
/// 0, 1 (direct pointers) and 12 (reached through the single-indirect
/// block) carry data; every other logical block is a sparse hole
/// (pointer 0) and reads back as zeros.
const CLASSIC_BLOCKS: usize = 13;
const CLASSIC_LEN: usize = CLASSIC_BLOCKS * FS_BLOCK;
const CLASSIC_DIRECT_0: u32 = 11;
const CLASSIC_DIRECT_1: u32 = 12;
const CLASSIC_INDIRECT_BLOCK: u32 = 13;
const CLASSIC_LOGICAL_12_BLOCK: u32 = 14;

fn u32c(value: usize) -> u32 {
    u32::try_from(value).expect("value fits in u32")
}

fn u16c(value: usize) -> u16 {
    u16::try_from(value).expect("value fits in u16")
}

fn u8c(value: usize) -> u8 {
    u8::try_from(value).expect("value fits in u8")
}

fn set_le16(img: &mut [u8], off: usize, value: u16) {
    img[off..off + 2].copy_from_slice(&value.to_le_bytes());
}

fn set_le32(img: &mut [u8], off: usize, value: u32) {
    img[off..off + 4].copy_from_slice(&value.to_le_bytes());
}

fn inode_offset(ino: u32) -> usize {
    INODE_TABLE_BLOCK * FS_BLOCK + (ino as usize - 1) * INODE_SIZE
}

/// Set an inode's owner, splitting each id into its low half
/// (`i_uid`/`i_gid`) and osd2 high half (`l_i_uid_high`/`l_i_gid_high`).
fn set_owner(img: &mut [u8], ino: u32, uid: u32, gid: u32) {
    let base = inode_offset(ino);
    set_le16(img, base + 0x02, (uid & 0xFFFF) as u16);
    set_le16(img, base + 0x78, (uid >> 16) as u16);
    set_le16(img, base + 0x18, (gid & 0xFFFF) as u16);
    set_le16(img, base + 0x7A, (gid >> 16) as u16);
}

fn block_offset(block: u32) -> usize {
    block as usize * FS_BLOCK
}

/// Deterministic byte for the classic file at an absolute file offset.
fn classic_byte(offset: usize) -> u8 {
    u8c(offset % 251)
}

/// Whether the classic file's logical block carries data (the others
/// are sparse holes).
fn classic_present(logical: usize) -> bool {
    matches!(logical, 0 | 1 | 12)
}

/// Physical block backing a present classic logical block.
fn classic_phys(logical: usize) -> u32 {
    match logical {
        0 => CLASSIC_DIRECT_0,
        1 => CLASSIC_DIRECT_1,
        12 => CLASSIC_LOGICAL_12_BLOCK,
        _ => 0,
    }
}

/// Write an inode's common fields plus an extent map covering
/// `extents` — each `(logical_start, len_blocks, physical_start)`.
fn write_extent_inode(img: &mut [u8], ino: u32, mode: u16, size: u32, extents: &[(u32, u16, u32)]) {
    let base = inode_offset(ino);
    set_le16(img, base, mode);
    set_le32(img, base + 0x04, size);
    set_le32(img, base + 0x20, INODE_FLAG_EXTENTS);

    let ib = base + I_BLOCK_OFFSET;
    set_le16(img, ib, EXTENT_MAGIC);
    set_le16(img, ib + 2, u16c(extents.len()));
    set_le16(img, ib + 4, 4);
    set_le16(img, ib + 6, 0);
    set_le32(img, ib + 8, 0);
    for (i, &(logical, len, phys)) in extents.iter().enumerate() {
        let e = ib + 12 + i * 12;
        set_le32(img, e, logical);
        set_le16(img, e + 4, len);
        set_le16(img, e + 6, 0); // ee_start_hi: all test blocks fit in 32 bits
        set_le32(img, e + 8, phys);
    }
}

/// Write an inode with the classic block map: up to 12 direct pointers
/// and one single-indirect pointer.
fn write_classic_inode(
    img: &mut [u8],
    ino: u32,
    mode: u16,
    size: u32,
    direct: &[u32; 12],
    single_indirect: u32,
) {
    let base = inode_offset(ino);
    set_le16(img, base, mode);
    set_le32(img, base + 0x04, size);
    set_le32(img, base + 0x20, 0);

    let ib = base + I_BLOCK_OFFSET;
    for (i, &ptr) in direct.iter().enumerate() {
        set_le32(img, ib + i * 4, ptr);
    }
    set_le32(img, ib + 12 * 4, single_indirect);
}

/// Append a directory entry into `block` at `pos`, returning the next
/// write position. When `fill_to_end` the entry's `rec_len` covers the
/// rest of the block (the on-disk convention for the final entry).
fn put_dirent(
    block: &mut [u8],
    pos: usize,
    ino: u32,
    name: &[u8],
    file_type: u8,
    fill_to_end: bool,
) -> usize {
    let needed = (DIRENT_HEADER + name.len()).div_ceil(4) * 4;
    let rec_len = if fill_to_end { FS_BLOCK - pos } else { needed };
    set_le32(block, pos, ino);
    set_le16(block, pos + 4, u16c(rec_len));
    block[pos + 6] = u8c(name.len());
    block[pos + 7] = file_type;
    block[pos + DIRENT_HEADER..pos + DIRENT_HEADER + name.len()].copy_from_slice(name);
    pos + rec_len
}

/// Write the superblock, the single group descriptor, and the block /
/// inode bitmaps that the write path's allocator consumes.
fn write_volume_metadata(img: &mut [u8]) {
    // --- Superblock at byte 1024 (block 1). ---
    let sb = usize::try_from(SUPERBLOCK_OFFSET).expect("offset fits");
    set_le32(img, sb, INODES_PER_GROUP); // s_inodes_count
    set_le32(img, sb + 0x04, u32c(FS_BLOCK_COUNT)); // s_blocks_count_lo
    set_le32(img, sb + 0x0C, u32c(FREE_BLOCKS)); // s_free_blocks_count_lo
    set_le32(img, sb + 0x10, u32c(FREE_INODES)); // s_free_inodes_count
    set_le32(img, sb + 0x14, 1); // s_first_data_block (1024-byte blocks)
    set_le32(img, sb + 0x18, 0); // s_log_block_size -> 1024
    set_le32(img, sb + 0x20, u32c(FS_BLOCK_COUNT)); // s_blocks_per_group
    set_le32(img, sb + 0x28, INODES_PER_GROUP); // s_inodes_per_group
    set_le16(img, sb + 0x38, EXT_MAGIC); // s_magic
    set_le32(img, sb + 0x4C, 1); // s_rev_level (dynamic)
    set_le16(img, sb + 0x58, u16c(INODE_SIZE)); // s_inode_size
    set_le32(img, sb + 0x60, INCOMPAT_FILETYPE); // s_feature_incompat

    // --- Group descriptor 0 at block 2. ---
    let gd = 2 * FS_BLOCK;
    set_le32(img, gd, u32c(BLOCK_BITMAP_BLOCK)); // bg_block_bitmap_lo
    set_le32(img, gd + 0x04, u32c(INODE_BITMAP_BLOCK)); // bg_inode_bitmap_lo
    set_le32(img, gd + 0x08, u32c(INODE_TABLE_BLOCK)); // bg_inode_table_lo
    set_le16(img, gd + 0x0C, u16c(FREE_BLOCKS)); // bg_free_blocks_count_lo
    set_le16(img, gd + 0x0E, u16c(FREE_INODES)); // bg_free_inodes_count_lo
    set_le16(img, gd + 0x10, 2); // bg_used_dirs_count_lo (root + sub)

    // --- Block bitmap (block 3): blocks 1..=14 used, 15..=39 free.
    //     Bit `b` represents block `s_first_data_block + b`, i.e. b + 1. ---
    let bbm = block_offset(u32c(BLOCK_BITMAP_BLOCK));
    for block in 1..FIRST_FREE_BLOCK {
        let bit = block - 1;
        img[bbm + bit / 8] |= 1 << (bit % 8);
    }

    // --- Inode bitmap (block 4): inodes 1..=14 used, 15..=16 free. ---
    let ibm = block_offset(u32c(INODE_BITMAP_BLOCK));
    for ino in 1..=14 {
        let bit = ino - 1;
        img[ibm + bit / 8] |= 1 << (bit % 8);
    }
}

/// Build the in-memory ext4 image described in the module docs.
fn build_image() -> Vec<u8> {
    let mut img = vec![0u8; IMG_LEN];
    write_volume_metadata(&mut img);

    // --- Root directory (inode 2), one extent-mapped block. ---
    write_extent_inode(
        &mut img,
        ROOT_INODE,
        S_IFDIR | 0o755,
        u32c(FS_BLOCK),
        &[(0, 1, ROOT_DATA_BLOCK)],
    );
    {
        let off = block_offset(ROOT_DATA_BLOCK);
        let block = &mut img[off..off + FS_BLOCK];
        let mut pos = put_dirent(block, 0, ROOT_INODE, b".", FT_DIR, false);
        pos = put_dirent(block, pos, ROOT_INODE, b"..", FT_DIR, false);
        pos = put_dirent(block, pos, 11, b"hello.txt", FT_REG, false);
        pos = put_dirent(block, pos, 12, b"classic.bin", FT_REG, false);
        let _ = put_dirent(block, pos, 13, b"sub", FT_DIR, true);
    }

    // --- hello.txt (inode 11), one extent-mapped block. ---
    write_extent_inode(
        &mut img,
        11,
        S_IFREG | 0o644,
        u32c(HELLO_BODY.len()),
        &[(0, 1, HELLO_DATA_BLOCK)],
    );
    set_owner(&mut img, 11, HELLO_UID, HELLO_GID);
    {
        let off = block_offset(HELLO_DATA_BLOCK);
        img[off..off + HELLO_BODY.len()].copy_from_slice(HELLO_BODY);
    }

    // --- sub/ (inode 13), one extent-mapped block. ---
    write_extent_inode(
        &mut img,
        13,
        S_IFDIR | 0o755,
        u32c(FS_BLOCK),
        &[(0, 1, SUB_DATA_BLOCK)],
    );
    {
        let off = block_offset(SUB_DATA_BLOCK);
        let block = &mut img[off..off + FS_BLOCK];
        let mut pos = put_dirent(block, 0, 13, b".", FT_DIR, false);
        pos = put_dirent(block, pos, ROOT_INODE, b"..", FT_DIR, false);
        let _ = put_dirent(block, pos, 14, b"deep.bin", FT_REG, true);
    }

    // --- sub/deep.bin (inode 14), one extent-mapped block. ---
    write_extent_inode(
        &mut img,
        14,
        S_IFREG | 0o600,
        u32c(DEEP_BODY.len()),
        &[(0, 1, DEEP_DATA_BLOCK)],
    );
    {
        let off = block_offset(DEEP_DATA_BLOCK);
        img[off..off + DEEP_BODY.len()].copy_from_slice(DEEP_BODY);
    }

    // --- classic.bin (inode 12): block-mapped direct + holes + single
    //     indirect. Direct pointers map logical 0 and 1; logical 2..=11
    //     are holes (pointer 0). Logical 12 is reached via the indirect
    //     block, exercising the direct/indirect boundary. ---
    let mut direct = [0u32; 12];
    direct[0] = CLASSIC_DIRECT_0;
    direct[1] = CLASSIC_DIRECT_1;
    write_classic_inode(
        &mut img,
        12,
        S_IFREG | 0o644,
        u32c(CLASSIC_LEN),
        &direct,
        CLASSIC_INDIRECT_BLOCK,
    );
    // The indirect block's first pointer maps logical block 12.
    set_le32(
        &mut img,
        block_offset(CLASSIC_INDIRECT_BLOCK),
        CLASSIC_LOGICAL_12_BLOCK,
    );
    // Fill each present data block with the deterministic pattern.
    for logical in 0..CLASSIC_BLOCKS {
        if !classic_present(logical) {
            continue;
        }
        let off = block_offset(classic_phys(logical));
        for i in 0..FS_BLOCK {
            img[off + i] = classic_byte(logical * FS_BLOCK + i);
        }
    }

    img
}

/// A fixed-size in-memory [`Block`] device over an ext4 image, using a
/// 512-byte logical-block size distinct from the 1024-byte filesystem
/// block so the device-staging path is exercised.
struct MockBlock {
    data: Vec<u8>,
}

impl MockBlock {
    fn span(lba: u64, len: usize) -> Result<(usize, usize), DriverError> {
        if len == 0 || len % DEV_SECTOR != 0 {
            return Err(DriverError::BufferTooSmall);
        }
        let start = usize::try_from(lba)
            .map_err(|_| DriverError::LengthOutOfRange)?
            .saturating_mul(DEV_SECTOR);
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
            block_size: u32c(DEV_SECTOR),
            block_count: DEV_SECTOR_COUNT,
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

fn mount() -> Ext4<MockBlock> {
    Ext4::open(MockBlock {
        data: build_image(),
    })
    .expect("image is a valid ext4 volume")
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
fn open_rejects_bad_magic() {
    let mut data = build_image();
    let sb = usize::try_from(SUPERBLOCK_OFFSET).expect("offset fits");
    set_le16(&mut data, sb + 0x38, 0x1234);
    assert_eq!(
        Ext4::open(MockBlock { data }).err(),
        Some(DriverError::BadMagic)
    );
}

#[test]
fn root_is_a_directory() {
    let mut fs = mount();
    let info = fs.node_info(fs.root()).expect("info");
    assert_eq!(info.kind, NodeKind::Directory);
    assert_eq!(info.size, 0);
}

#[test]
fn root_lists_its_entries_in_on_disk_order() {
    let mut fs = mount();
    let root = fs.root();
    let mut name = [0u8; 32];

    let e0 = fs.read_dir(root, 0, &mut name).expect("ok").expect("entry");
    assert_eq!(&name[..e0.name_len], b"hello.txt");
    assert_eq!(e0.kind, NodeKind::RegularFile);

    let e1 = fs.read_dir(root, 1, &mut name).expect("ok").expect("entry");
    assert_eq!(&name[..e1.name_len], b"classic.bin");
    assert_eq!(e1.kind, NodeKind::RegularFile);

    let e2 = fs.read_dir(root, 2, &mut name).expect("ok").expect("entry");
    assert_eq!(&name[..e2.name_len], b"sub");
    assert_eq!(e2.kind, NodeKind::Directory);

    // `.` and `..` are not surfaced, and iteration terminates.
    assert_eq!(fs.read_dir(root, 3, &mut name), Ok(None));
}

#[test]
fn read_dir_rejects_a_too_small_name_buffer() {
    let mut fs = mount();
    let mut tiny = [0u8; 4];
    assert_eq!(
        fs.read_dir(fs.root(), 0, &mut tiny),
        Err(DriverError::BufferTooSmall)
    );
}

#[test]
fn lookup_and_read_an_extent_mapped_file() {
    let mut fs = mount();
    let root = fs.root();
    let file = fs.lookup(root, b"hello.txt").expect("found");
    let info = fs.node_info(file).expect("info");
    assert_eq!(info.kind, NodeKind::RegularFile);
    assert_eq!(info.size, HELLO_BODY.len() as u64);

    let mut buf = [0u8; 64];
    let n = fs.read_at(file, 0, &mut buf).expect("read");
    assert_eq!(&buf[..n], HELLO_BODY);

    // A read at EOF yields zero bytes.
    assert_eq!(fs.read_at(file, info.size, &mut buf), Ok(0));
    // A mid-file offset returns the trailing bytes.
    let n = fs.read_at(file, 7, &mut buf).expect("read");
    assert_eq!(&buf[..n], &HELLO_BODY[7..]);
}

#[test]
fn lookup_missing_child_is_not_found() {
    let mut fs = mount();
    assert_eq!(fs.lookup(fs.root(), b"nope"), Err(DriverError::NotFound));
}

#[test]
fn traverses_into_a_subdirectory() {
    let mut fs = mount();
    let root = fs.root();
    let sub = fs.lookup(root, b"sub").expect("subdir");
    assert_eq!(fs.node_info(sub).expect("info").kind, NodeKind::Directory);

    let deep = fs.lookup(sub, b"deep.bin").expect("deep");
    let mut buf = [0u8; 64];
    let n = fs.read_at(deep, 0, &mut buf).expect("read");
    assert_eq!(&buf[..n], DEEP_BODY);
}

#[test]
fn reads_a_classic_block_mapped_file_across_holes_and_indirect() {
    let mut fs = mount();
    let root = fs.root();
    let file = fs.lookup(root, b"classic.bin").expect("found");
    assert_eq!(fs.node_info(file).expect("info").size, CLASSIC_LEN as u64);

    // Read the whole file in one call and check every byte: the present
    // blocks carry the deterministic pattern, the holes read as zeros.
    let mut buf = [0u8; CLASSIC_LEN];
    let n = fs.read_at(file, 0, &mut buf).expect("read");
    assert_eq!(n, CLASSIC_LEN);
    for (offset, byte) in buf.iter().enumerate() {
        let logical = offset / FS_BLOCK;
        let expected = if classic_present(logical) {
            classic_byte(offset)
        } else {
            0
        };
        assert_eq!(*byte, expected, "mismatch at offset {offset}");
    }
}

#[test]
fn classic_read_spanning_the_indirect_boundary() {
    let mut fs = mount();
    let file = fs.lookup(fs.root(), b"classic.bin").expect("found");
    // Straddle the direct/indirect boundary: the tail of logical block
    // 11 (a hole) into logical block 12 (present, behind the
    // single-indirect pointer).
    let start = 12 * FS_BLOCK - 4;
    let mut buf = [0u8; 8];
    let n = fs.read_at(file, start as u64, &mut buf).expect("read");
    assert_eq!(n, 8);
    for (i, byte) in buf.iter().enumerate() {
        let offset = start + i;
        let expected = if classic_present(offset / FS_BLOCK) {
            classic_byte(offset)
        } else {
            0
        };
        assert_eq!(*byte, expected);
    }
}

#[test]
fn read_at_on_a_directory_is_unsupported() {
    let mut fs = mount();
    let mut buf = [0u8; 16];
    assert_eq!(
        fs.read_at(fs.root(), 0, &mut buf),
        Err(DriverError::Unsupported)
    );
}

#[test]
fn lookup_in_a_regular_file_is_unsupported() {
    let mut fs = mount();
    let file = fs.lookup(fs.root(), b"hello.txt").expect("found");
    assert_eq!(fs.lookup(file, b"x"), Err(DriverError::Unsupported));
}

#[test]
fn node_id_none_is_not_found() {
    let mut fs = mount();
    assert_eq!(fs.node_info(NodeId::NONE), Err(DriverError::NotFound));
}

#[test]
fn into_block_returns_the_underlying_device() {
    let fs = mount();
    let dev = fs.into_block();
    assert_eq!(
        dev.geometry().expect("geometry").block_count,
        DEV_SECTOR_COUNT
    );
}

#[test]
fn security_reports_a_files_mode_and_owner() {
    let mut fs = mount();
    let file = fs.lookup(fs.root(), b"hello.txt").expect("found");
    let sec = fs.security(file).expect("security");
    // The mode is the low 12 bits; the directory/file type bits are stripped.
    assert_eq!(sec.mode, 0o644);
    // uid/gid recombine the low half with the osd2 high half.
    assert_eq!(sec.uid, HELLO_UID);
    assert_eq!(sec.gid, HELLO_GID);
    // ext4 stores no inline capability gate and no inline ACL here.
    assert_eq!(sec.required_cap, None);
    assert!(sec.acl().is_empty());
}

#[test]
fn security_reports_a_directorys_record() {
    let mut fs = mount();
    let sec = fs.security(fs.root()).expect("security");
    assert_eq!(sec.mode, 0o755);
    // The root inode in the fixture leaves the owner at the default 0/0.
    assert_eq!(sec.uid, 0);
    assert_eq!(sec.gid, 0);
}

#[test]
fn security_of_an_absent_node_is_not_found() {
    let mut fs = mount();
    assert_eq!(fs.security(NodeId::NONE), Err(DriverError::NotFound));
}

// --- Write surface (`FilesystemWrite`). ---

/// Re-open the volume backing `fs`, exercising the persistence of any
/// writes through a fresh mount.
fn remount(fs: Ext4<MockBlock>) -> Ext4<MockBlock> {
    Ext4::open(fs.into_block()).expect("re-open the mutated image")
}

#[test]
fn create_and_write_a_regular_file_round_trips() {
    let mut fs = mount();
    let root = fs.root();
    let file = fs
        .create(root, b"new.txt", NodeKind::RegularFile)
        .expect("create");
    assert_eq!(fs.node_info(file).expect("info").size, 0);
    assert_eq!(fs.lookup(root, b"new.txt"), Ok(file));

    // A payload spanning two filesystem blocks.
    let mut payload = [0u8; FS_BLOCK + 500];
    let mut next = 0u8;
    for b in &mut payload {
        *b = next;
        next = next.wrapping_add(1);
    }
    let n = fs.write_at(root, b"new.txt", 0, &payload).expect("write");
    assert_eq!(n, payload.len());
    assert_eq!(fs.node_info(file).expect("info").size, payload.len() as u64);

    let mut fs = remount(fs);
    let file = fs
        .lookup(fs.root(), b"new.txt")
        .expect("found after remount");
    let mut buf = [0u8; FS_BLOCK + 500];
    let n = fs.read_at(file, 0, &mut buf).expect("read");
    assert_eq!(n, payload.len());
    assert_eq!(buf, payload);
}

#[test]
fn create_then_appears_in_directory_listing() {
    let mut fs = mount();
    let root = fs.root();
    fs.create(root, b"zeta.dat", NodeKind::RegularFile)
        .expect("create");
    let mut name = [0u8; 32];
    let mut found = false;
    for index in 0.. {
        let Some(entry) = fs.read_dir(root, index, &mut name).expect("read_dir") else {
            break;
        };
        if &name[..entry.name_len] == b"zeta.dat" {
            assert_eq!(entry.kind, NodeKind::RegularFile);
            found = true;
        }
    }
    assert!(found, "the created file is listed");
}

#[test]
fn create_rejects_a_duplicate_name() {
    let mut fs = mount();
    let root = fs.root();
    assert_eq!(
        fs.create(root, b"hello.txt", NodeKind::RegularFile),
        Err(DriverError::Busy)
    );
}

#[test]
fn create_in_a_regular_file_is_unsupported() {
    let mut fs = mount();
    let file = fs.lookup(fs.root(), b"hello.txt").expect("found");
    assert_eq!(
        fs.create(file, b"x", NodeKind::RegularFile),
        Err(DriverError::Unsupported)
    );
}

#[test]
fn create_rejects_an_invalid_name() {
    let mut fs = mount();
    let root = fs.root();
    assert_eq!(
        fs.create(root, b"", NodeKind::RegularFile),
        Err(DriverError::LengthOutOfRange)
    );
    assert_eq!(
        fs.create(root, b"a/b", NodeKind::RegularFile),
        Err(DriverError::LengthOutOfRange)
    );
    assert_eq!(
        fs.create(root, b"..", NodeKind::RegularFile),
        Err(DriverError::LengthOutOfRange)
    );
}

#[test]
fn write_past_eof_leaves_a_sparse_hole() {
    let mut fs = mount();
    let root = fs.root();
    fs.create(root, b"sparse.bin", NodeKind::RegularFile)
        .expect("create");
    let tail = b"TAIL";
    let n = fs.write_at(root, b"sparse.bin", 2000, tail).expect("write");
    assert_eq!(n, tail.len());

    let file = fs.lookup(root, b"sparse.bin").expect("found");
    assert_eq!(
        fs.node_info(file).expect("info").size,
        2000 + tail.len() as u64
    );
    let mut buf = [0u8; 2000 + 4];
    let read = fs.read_at(file, 0, &mut buf).expect("read");
    assert_eq!(read, buf.len());
    assert!(
        buf[..2000].iter().all(|&b| b == 0),
        "the gap reads as zeros"
    );
    assert_eq!(&buf[2000..], tail);
}

#[test]
fn truncate_shrink_then_grow() {
    let mut fs = mount();
    let root = fs.root();
    fs.create(root, b"trunc.bin", NodeKind::RegularFile)
        .expect("create");
    let payload = [0xABu8; 3 * FS_BLOCK];
    fs.write_at(root, b"trunc.bin", 0, &payload).expect("write");

    // Shrink to mid-first-block: the freed tail blocks return to the pool.
    fs.truncate(root, b"trunc.bin", 100).expect("shrink");
    let file = fs.lookup(root, b"trunc.bin").expect("found");
    assert_eq!(fs.node_info(file).expect("info").size, 100);
    let mut buf = [0u8; 200];
    let n = fs.read_at(file, 0, &mut buf).expect("read");
    assert_eq!(n, 100);
    assert!(buf[..100].iter().all(|&b| b == 0xAB));

    // Grow back: the extension reads as zeros (sparse).
    fs.truncate(root, b"trunc.bin", FS_BLOCK as u64)
        .expect("grow");
    let mut big = [0xFFu8; FS_BLOCK];
    let n = fs.read_at(file, 0, &mut big).expect("read");
    assert_eq!(n, FS_BLOCK);
    assert!(big[..100].iter().all(|&b| b == 0xAB));
    assert!(big[100..].iter().all(|&b| b == 0), "grown region is zero");
}

/// Logical blocks written far enough apart to stay distinct extents, so
/// the inline four-slot extent root overflows and converts to a depth-1
/// tree (`hello.txt`'s planted extent already occupies the first slot,
/// so the fifth write here is the one that forces the conversion).
const GROWTH_LOGICALS: [u64; 5] = [100, 200, 300, 400, 500];

/// A deterministic, non-zero fill byte for the block at `logical`.
fn growth_marker(logical: u64) -> u8 {
    u8::try_from(logical / 100).expect("marker index fits") * 17 + 3
}

/// Sparsely write one distinct block at each [`GROWTH_LOGICALS`] offset of
/// `hello.txt`, forcing its depth-0 inline root to grow into a depth-1
/// extent tree.
fn grow_hello_into_depth1(fs: &mut Ext4<MockBlock>) {
    let root = fs.root();
    for &logical in &GROWTH_LOGICALS {
        let block = [growth_marker(logical); FS_BLOCK];
        let n = fs
            .write_at(root, b"hello.txt", logical * FS_BLOCK as u64, &block)
            .expect("sparse write grows the extent tree");
        assert_eq!(n, FS_BLOCK);
    }
}

#[test]
fn an_extent_file_grows_into_a_depth1_tree() {
    let mut fs = mount();
    grow_hello_into_depth1(&mut fs);

    // Re-open to prove the converted tree persists on disk.
    let mut fs = remount(fs);
    let root = fs.root();
    let file = fs.lookup(root, b"hello.txt").expect("found");

    // The original first-block contents survive the conversion.
    let mut head = [0u8; 64];
    let n = fs.read_at(file, 0, &mut head).expect("read head");
    assert_eq!(n, head.len());
    assert_eq!(&head[..HELLO_BODY.len()], HELLO_BODY);

    // Every sparsely-written block reads back its marker...
    for &logical in &GROWTH_LOGICALS {
        let mut buf = [0u8; FS_BLOCK];
        let n = fs
            .read_at(file, logical * FS_BLOCK as u64, &mut buf)
            .expect("read a grown block");
        assert_eq!(n, FS_BLOCK);
        assert!(
            buf.iter().all(|&b| b == growth_marker(logical)),
            "logical block {logical} reads back its marker"
        );
    }

    // ...and a gap between them is a sparse hole of zeros.
    let mut hole = [0xFFu8; FS_BLOCK];
    let n = fs
        .read_at(file, 150 * FS_BLOCK as u64, &mut hole)
        .expect("read hole");
    assert_eq!(n, FS_BLOCK);
    assert!(hole.iter().all(|&b| b == 0), "the gap reads as zeros");
}

#[test]
fn truncating_a_depth1_extent_file_to_zero_frees_its_tree() {
    let mut fs = mount();
    grow_hello_into_depth1(&mut fs);
    let root = fs.root();
    fs.truncate(root, b"hello.txt", 0)
        .expect("truncate to zero");
    let file = fs.lookup(root, b"hello.txt").expect("found");
    assert_eq!(fs.node_info(file).expect("info").size, 0);

    // The freed leaf + data blocks return to the pool: re-growing into a
    // fresh depth-1 tree succeeds and round-trips across a remount.
    grow_hello_into_depth1(&mut fs);
    let mut fs = remount(fs);
    let file = fs.lookup(fs.root(), b"hello.txt").expect("found");
    for &logical in &GROWTH_LOGICALS {
        let mut buf = [0u8; FS_BLOCK];
        let n = fs
            .read_at(file, logical * FS_BLOCK as u64, &mut buf)
            .expect("read a regrown block");
        assert_eq!(n, FS_BLOCK);
        assert!(buf.iter().all(|&b| b == growth_marker(logical)));
    }
}

#[test]
fn removing_a_depth1_extent_file_frees_it() {
    let mut fs = mount();
    grow_hello_into_depth1(&mut fs);
    let root = fs.root();
    fs.remove(root, b"hello.txt")
        .expect("remove a depth-1 file");
    assert_eq!(fs.lookup(root, b"hello.txt"), Err(DriverError::NotFound));

    // The inode and all of its data + leaf blocks are reusable afterwards.
    fs.create(root, b"fresh.txt", NodeKind::RegularFile)
        .expect("create reuses the freed metadata");
    fs.write_at(root, b"fresh.txt", 0, b"ok").expect("write");
    let mut fs = remount(fs);
    let file = fs.lookup(fs.root(), b"fresh.txt").expect("found");
    let mut buf = [0u8; 8];
    let n = fs.read_at(file, 0, &mut buf).expect("read");
    assert_eq!(&buf[..n], b"ok");
}

#[test]
fn create_a_directory_with_dot_and_dotdot() {
    let mut fs = mount();
    let root = fs.root();
    let dir = fs
        .create(root, b"newdir", NodeKind::Directory)
        .expect("mkdir");
    assert_eq!(fs.node_info(dir).expect("info").kind, NodeKind::Directory);

    // A fresh directory lists no children (`.`/`..` are skipped).
    let mut name = [0u8; 32];
    assert_eq!(fs.read_dir(dir, 0, &mut name), Ok(None));

    // It accepts a child, which then resolves and lists.
    let child = fs
        .create(dir, b"inner.txt", NodeKind::RegularFile)
        .expect("create");
    assert_eq!(fs.lookup(dir, b"inner.txt"), Ok(child));

    let mut fs = remount(fs);
    let dir = fs.lookup(fs.root(), b"newdir").expect("dir after remount");
    assert!(fs.lookup(dir, b"inner.txt").is_ok());
}

#[test]
fn remove_a_file_frees_its_inode_for_reuse() {
    let mut fs = mount();
    let root = fs.root();
    fs.remove(root, b"hello.txt").expect("remove");
    assert_eq!(fs.lookup(root, b"hello.txt"), Err(DriverError::NotFound));

    // The freed inode and blocks are reusable: creating + writing succeeds
    // and round-trips across a remount.
    fs.create(root, b"again.txt", NodeKind::RegularFile)
        .expect("create reuses freed metadata");
    let body = b"reused";
    fs.write_at(root, b"again.txt", 0, body).expect("write");

    let mut fs = remount(fs);
    let file = fs.lookup(fs.root(), b"again.txt").expect("found");
    let mut buf = [0u8; 16];
    let n = fs.read_at(file, 0, &mut buf).expect("read");
    assert_eq!(&buf[..n], body);
}

#[test]
fn remove_a_non_empty_directory_is_busy() {
    let mut fs = mount();
    let root = fs.root();
    assert_eq!(fs.remove(root, b"sub"), Err(DriverError::Busy));
}

#[test]
fn remove_an_emptied_directory() {
    let mut fs = mount();
    let root = fs.root();
    let sub = fs.lookup(root, b"sub").expect("sub");
    fs.remove(sub, b"deep.bin").expect("empty the dir");
    fs.remove(root, b"sub").expect("remove the now-empty dir");
    assert_eq!(fs.lookup(root, b"sub"), Err(DriverError::NotFound));
}

#[test]
fn write_to_a_directory_is_unsupported() {
    let mut fs = mount();
    let root = fs.root();
    assert_eq!(
        fs.write_at(root, b"sub", 0, b"x"),
        Err(DriverError::Unsupported)
    );
}

#[test]
fn write_to_a_missing_child_is_not_found() {
    let mut fs = mount();
    let root = fs.root();
    assert_eq!(
        fs.write_at(root, b"absent", 0, b"x"),
        Err(DriverError::NotFound)
    );
}

#[test]
fn mutation_is_refused_on_an_unsupported_feature_set() {
    // The `metadata_csum`/`gdt_csum` and `64bit` feature sets are now
    // mutated in place (see `tests/checksummed.rs`, which validates the
    // maintained checksums against real `mke2fs` images). Mutation still
    // fails closed (§5.4) on a feature the write path cannot maintain.
    // `checksum_seed` (incompat 0x2000) is one: it would invalidate the
    // `crc32c(~0, uuid)` seed, so the volume stays read-only.
    let mut data = build_image();
    let sb = usize::try_from(SUPERBLOCK_OFFSET).expect("offset fits");
    set_le32(&mut data, sb + 0x60, INCOMPAT_FILETYPE | 0x2000); // + checksum_seed
    let mut fs = Ext4::open(MockBlock { data }).expect("opens read-only");
    // Reads still work; only mutation is refused.
    assert!(fs.lookup(fs.root(), b"hello.txt").is_ok());
    assert_eq!(
        fs.create(fs.root(), b"x", NodeKind::RegularFile),
        Err(DriverError::Unsupported)
    );
    assert_eq!(
        fs.write_at(fs.root(), b"hello.txt", 0, b"x"),
        Err(DriverError::Unsupported)
    );
    assert_eq!(
        fs.remove(fs.root(), b"hello.txt"),
        Err(DriverError::Unsupported)
    );
}

#[test]
fn create_exhausts_the_free_inodes() {
    let mut fs = mount();
    let root = fs.root();
    // The fixture leaves exactly two free inodes (15, 16).
    fs.create(root, b"one", NodeKind::RegularFile)
        .expect("first");
    fs.create(root, b"two", NodeKind::RegularFile)
        .expect("second");
    assert_eq!(
        fs.create(root, b"three", NodeKind::RegularFile),
        Err(DriverError::NoSpace)
    );
}

// --- Extended-attribute POSIX ACLs (`FilesystemSecurity`). ---

/// Write a `POSIX_ACL_XATTR` value (version word + 8-byte
/// `(e_tag, e_perm, e_id)` entries) into `img` at `pos`, returning its
/// byte length.
fn write_posix_acl(img: &mut [u8], pos: usize, entries: &[(u16, u16, u32)]) -> usize {
    set_le32(img, pos, 2); // a_version
    let mut off = pos + 4;
    for &(tag, perm, id) in entries {
        set_le16(img, off, tag);
        set_le16(img, off + 2, perm);
        set_le32(img, off + 4, id);
        off += 8;
    }
    off - pos
}

/// Write one `ext4_xattr_entry` for `system.posix_acl_access` (the whole
/// name is encoded by `e_name_index = 2`, so `e_name_len` is zero) at
/// `pos`.
fn write_acl_xattr_entry(img: &mut [u8], pos: usize, value_offs: u16, value_size: u32) {
    img[pos] = 0; // e_name_len
    img[pos + 1] = 2; // e_name_index = POSIX_ACL_ACCESS
    set_le16(img, pos + 2, value_offs); // e_value_offs
    set_le32(img, pos + 4, 0); // e_value_inum (in-block value)
    set_le32(img, pos + 8, value_size); // e_value_size
    set_le32(img, pos + 12, 0); // e_hash
}

/// The six entries a complete `getfacl`-style ACL carries; only the two
/// named entries (`ACL_USER` 1000, `ACL_GROUP` 2000) surface as grants.
const SAMPLE_ACL: [(u16, u16, u32); 6] = [
    (1, 6, !0),   // ACL_USER_OBJ rw-
    (2, 4, 1000), // ACL_USER 1000 r--
    (4, 4, !0),   // ACL_GROUP_OBJ r--
    (8, 5, 2000), // ACL_GROUP 2000 r-x
    (16, 7, !0),  // ACL_MASK rwx
    (32, 4, !0),  // ACL_OTHER r--
];

#[test]
fn decode_posix_acl_keeps_only_named_user_and_group_grants() {
    let mut value = [0u8; 4 + SAMPLE_ACL.len() * 8];
    let len = write_posix_acl(&mut value, 0, &SAMPLE_ACL);
    let mut sec = NodeSecurity::new(0o644, 0, 0);
    decode_posix_acl(&value[..len], &mut sec);
    assert_eq!(
        sec.acl(),
        &[
            SecurityAcl {
                subject: SecuritySubject::User(1000),
                perms: 4,
            },
            SecurityAcl {
                subject: SecuritySubject::Group(2000),
                perms: 5,
            },
        ]
    );
}

#[test]
fn decode_posix_acl_rejects_a_bad_version() {
    let mut value = [0u8; 12];
    set_le32(&mut value, 0, 99); // not POSIX_ACL_VERSION
    set_le16(&mut value, 4, 2); // ACL_USER
    set_le16(&mut value, 6, 7);
    set_le32(&mut value, 8, 1000);
    let mut sec = NodeSecurity::new(0o644, 0, 0);
    decode_posix_acl(&value, &mut sec);
    assert!(sec.acl().is_empty());
}

#[test]
fn decode_posix_acl_stops_at_the_inline_budget() {
    // Ten named-user entries; the record only holds MAX_ACL_ENTRIES (8).
    let mut value = vec![0u8; 4 + 10 * 8];
    set_le32(&mut value, 0, 2);
    for i in 0..10u32 {
        let off = 4 + i as usize * 8;
        set_le16(&mut value, off, 2); // ACL_USER
        set_le16(&mut value, off + 2, 4);
        set_le32(&mut value, off + 4, i);
    }
    let mut sec = NodeSecurity::new(0, 0, 0);
    decode_posix_acl(&value, &mut sec);
    assert_eq!(sec.acl().len(), 8);
}

#[test]
fn find_posix_acl_locates_a_value_with_a_block_value_base() {
    let mut region = [0u8; 256];
    let value = [1u8, 2, 3, 4, 5];
    let value_offs = 128usize;
    region[value_offs..value_offs + value.len()].copy_from_slice(&value);
    write_acl_xattr_entry(&mut region, 32, u16c(value_offs), u32c(value.len()));
    let found = find_posix_acl(&region, 32, 0).expect("attribute present");
    assert_eq!(found, &value);
}

#[test]
fn find_posix_acl_locates_a_value_with_an_inode_value_base() {
    let mut region = [0u8; 256];
    let entries_start = 64usize; // header magic + 4 in the inode body
    let value_offs = 80usize; // measured from entries_start
    let value = [9u8, 8, 7];
    let at = entries_start + value_offs;
    region[at..at + value.len()].copy_from_slice(&value);
    write_acl_xattr_entry(
        &mut region,
        entries_start,
        u16c(value_offs),
        u32c(value.len()),
    );
    let found = find_posix_acl(&region, entries_start, entries_start).expect("present");
    assert_eq!(found, &value);
}

#[test]
fn find_posix_acl_skips_unrelated_attributes() {
    let mut region = [0u8; 64];
    // A `user.attr` entry (name_index 1), then the zero-word terminator.
    region[0] = 4; // e_name_len
    region[1] = 1; // e_name_index = "user."
    set_le16(&mut region, 2, 40);
    set_le32(&mut region, 8, 4); // e_value_size
    region[16..20].copy_from_slice(b"attr");
    assert!(find_posix_acl(&region, 0, 0).is_none());
}

#[test]
fn security_decodes_a_posix_acl_from_the_external_block() {
    let mut img = build_image();
    let acl_block: u32 = 16;
    set_le32(&mut img, inode_offset(11) + 0x68, acl_block); // i_file_acl_lo

    let base = block_offset(acl_block);
    set_le32(&mut img, base, 0xEA02_0000); // h_magic
    set_le32(&mut img, base + 4, 1); // h_refcount
    set_le32(&mut img, base + 8, 1); // h_blocks
    let value_offs = 128usize; // measured from the block start
    let value_size = write_posix_acl(&mut img, base + value_offs, &SAMPLE_ACL);
    write_acl_xattr_entry(
        &mut img,
        base + XATTR_BLOCK_HEADER_LEN,
        u16c(value_offs),
        u32c(value_size),
    );

    let mut fs = Ext4::open(MockBlock { data: img }).expect("valid volume");
    let file = fs.lookup(fs.root(), b"hello.txt").expect("found");
    let sec = fs.security(file).expect("security");
    assert_eq!(sec.mode, 0o644);
    assert_eq!(
        sec.acl(),
        &[
            SecurityAcl {
                subject: SecuritySubject::User(1000),
                perms: 4,
            },
            SecurityAcl {
                subject: SecuritySubject::Group(2000),
                perms: 5,
            },
        ]
    );
}

#[test]
fn security_ignores_a_garbage_external_block() {
    let mut img = build_image();
    let acl_block: u32 = 16;
    set_le32(&mut img, inode_offset(11) + 0x68, acl_block);
    // The block carries no xattr magic: the ACL is simply absent.
    let base = block_offset(acl_block);
    set_le32(&mut img, base, 0xDEAD_BEEF);

    let mut fs = Ext4::open(MockBlock { data: img }).expect("valid volume");
    let file = fs.lookup(fs.root(), b"hello.txt").expect("found");
    let sec = fs.security(file).expect("security");
    assert!(sec.acl().is_empty());
}

#[test]
fn security_decodes_an_inline_posix_acl_from_the_inode_body() {
    // A second, 256-byte-inode volume: the enlarged inode record has room
    // for an inline xattr region after `i_extra_isize`.
    const BS: usize = 1024;
    const ISIZE: usize = 256;
    const IPG: u32 = 16;
    const ITAB: usize = 5; // inode table spans blocks 5..=8 (16 * 256 B)
    const ROOT_DATA: u32 = 9;
    let mut img = vec![0u8; IMG_LEN];

    let sb = usize::try_from(SUPERBLOCK_OFFSET).expect("offset fits");
    set_le32(&mut img, sb, IPG); // s_inodes_count
    set_le32(&mut img, sb + 0x04, u32c(FS_BLOCK_COUNT)); // s_blocks_count_lo
    set_le32(&mut img, sb + 0x14, 1); // s_first_data_block
    set_le32(&mut img, sb + 0x18, 0); // s_log_block_size -> 1024
    set_le32(&mut img, sb + 0x20, u32c(FS_BLOCK_COUNT)); // s_blocks_per_group
    set_le32(&mut img, sb + 0x28, IPG); // s_inodes_per_group
    set_le16(&mut img, sb + 0x38, EXT_MAGIC);
    set_le32(&mut img, sb + 0x4C, 1); // s_rev_level (dynamic)
    set_le16(&mut img, sb + 0x58, u16c(ISIZE)); // s_inode_size
    set_le32(&mut img, sb + 0x60, INCOMPAT_FILETYPE);

    let gd = 2 * BS;
    set_le32(&mut img, gd, u32c(BLOCK_BITMAP_BLOCK));
    set_le32(&mut img, gd + 0x04, u32c(INODE_BITMAP_BLOCK));
    set_le32(&mut img, gd + 0x08, u32c(ITAB));

    let ino_off = |ino: u32| ITAB * BS + (ino as usize - 1) * ISIZE;

    // Root inode 2: extent-mapped directory, one block.
    {
        let b = ino_off(ROOT_INODE);
        set_le16(&mut img, b, S_IFDIR | 0o755);
        set_le32(&mut img, b + 0x04, u32c(BS));
        set_le32(&mut img, b + 0x20, INODE_FLAG_EXTENTS);
        let ib = b + I_BLOCK_OFFSET;
        set_le16(&mut img, ib, EXTENT_MAGIC);
        set_le16(&mut img, ib + 2, 1);
        set_le16(&mut img, ib + 4, 4);
        set_le16(&mut img, ib + 16, 1); // ee_len
        set_le32(&mut img, ib + 20, ROOT_DATA); // ee_start_lo
    }
    {
        let off = block_offset(ROOT_DATA);
        let block = &mut img[off..off + BS];
        let mut pos = put_dirent(block, 0, ROOT_INODE, b".", FT_DIR, false);
        pos = put_dirent(block, pos, ROOT_INODE, b"..", FT_DIR, false);
        let _ = put_dirent(block, pos, 11, b"f", FT_REG, true);
    }

    // File inode 11: empty regular file carrying an inline POSIX ACL.
    {
        let b = ino_off(11);
        set_le16(&mut img, b, S_IFREG | 0o600);
        set_le32(&mut img, b + 0x20, INODE_FLAG_EXTENTS);
        let ib = b + I_BLOCK_OFFSET;
        set_le16(&mut img, ib, EXTENT_MAGIC);
        set_le16(&mut img, ib + 4, 4); // eh_max

        let extra = 32usize;
        set_le16(&mut img, b + 0x80, u16c(extra)); // i_extra_isize
        let header = b + 0x80 + extra;
        set_le32(&mut img, header, 0xEA02_0000); // inline xattr magic
        let entries_start = header + 4;
        let value_offs = 20usize; // measured from entries_start
        let value_size = write_posix_acl(&mut img, entries_start + value_offs, &SAMPLE_ACL);
        write_acl_xattr_entry(&mut img, entries_start, u16c(value_offs), u32c(value_size));
    }

    let mut fs = Ext4::open(MockBlock { data: img }).expect("valid inline-ACL volume");
    let file = fs.lookup(fs.root(), b"f").expect("found");
    let sec = fs.security(file).expect("security");
    assert_eq!(sec.mode, 0o600);
    assert_eq!(
        sec.acl(),
        &[
            SecurityAcl {
                subject: SecuritySubject::User(1000),
                perms: 4,
            },
            SecurityAcl {
                subject: SecuritySubject::Group(2000),
                perms: 5,
            },
        ]
    );
}

// --- First-party formatter (`Ext4::format`). ---

/// Logical-block (sector) size of the configurable in-memory device the
/// formatter tests run against.
const FMT_SECTOR: usize = 512;

/// 8 MiB in 512-byte sectors: a single block group at the 1024-byte
/// filesystem block size the formatter picks for sub-64-MiB volumes.
const ONE_GROUP_SECTORS: u64 = (8 * 1024 * 1024 / FMT_SECTOR) as u64;

/// 16 MiB in 512-byte sectors: two block groups at the 1024-byte block
/// size (`blocks_per_group = 8 * 1024 = 8192` blocks = 8 MiB each).
const TWO_GROUP_SECTORS: u64 = (16 * 1024 * 1024 / FMT_SECTOR) as u64;

/// A zero-initialised, `Vec`-backed [`Block`] device of a configurable
/// size, for exercising [`Ext4::format`] on volumes larger than the
/// fixed read fixture.
struct SizedBlock {
    data: Vec<u8>,
}

impl SizedBlock {
    fn new(sectors: u64) -> Self {
        let len = usize::try_from(sectors).expect("fits") * FMT_SECTOR;
        Self {
            data: vec![0u8; len],
        }
    }

    fn span(&self, lba: u64, len: usize) -> Result<(usize, usize), DriverError> {
        if len == 0 || len % FMT_SECTOR != 0 {
            return Err(DriverError::BufferTooSmall);
        }
        let start = usize::try_from(lba)
            .map_err(|_| DriverError::LengthOutOfRange)?
            .saturating_mul(FMT_SECTOR);
        let end = start
            .checked_add(len)
            .ok_or(DriverError::LengthOutOfRange)?;
        if end > self.data.len() {
            return Err(DriverError::LengthOutOfRange);
        }
        Ok((start, end))
    }
}

impl Block for SizedBlock {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        Ok(BlockGeometry {
            block_size: u32c(FMT_SECTOR),
            block_count: (self.data.len() / FMT_SECTOR) as u64,
        })
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        let (start, end) = self.span(lba, buf.len())?;
        buf.copy_from_slice(&self.data[start..end]);
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        let (start, end) = self.span(lba, buf.len())?;
        self.data[start..end].copy_from_slice(buf);
        Ok(())
    }
}

/// A three-byte file name `f<NN>` for the inode-exhaustion test.
fn nth_name(i: usize) -> [u8; 3] {
    [b'f', b'0' + u8c(i / 10), b'0' + u8c(i % 10)]
}

/// Per-file size cap when filling the data region, in bytes. Kept well
/// below the classic block-map reach (12 direct + one single-indirect
/// block, i.e. ~268 KiB at the 1024-byte block size), since
/// [`FilesystemWrite::create`] lays files down with the classic map.
const FILL_FILE_CAP: u64 = 256 * 1024;

/// Fill the volume's data region by creating bounded-size files until an
/// allocation reports [`DriverError::NoSpace`], returning the total
/// number of data bytes successfully written. Any other error is a
/// driver defect and panics the test.
fn fill_to_no_space(fs: &mut Ext4<SizedBlock>, root: NodeId) -> u64 {
    let chunk = [0xABu8; 4096];
    let mut total = 0u64;
    let mut idx = 0usize;
    'outer: loop {
        let name = nth_name(idx);
        match fs.create(root, &name, NodeKind::RegularFile) {
            Ok(_) => {}
            Err(DriverError::NoSpace) => break,
            Err(e) => panic!("unexpected create error: {e:?}"),
        }
        idx += 1;
        assert!(idx < 100, "exhausted test file names before filling");
        let mut size = 0u64;
        while size < FILL_FILE_CAP {
            match fs.write_at(root, &name, size, &chunk) {
                Ok(n) => {
                    size += n as u64;
                    total += n as u64;
                }
                Err(DriverError::NoSpace) => break 'outer,
                Err(e) => panic!("unexpected write error: {e:?}"),
            }
        }
    }
    total
}

#[test]
fn format_rejects_a_device_too_small_for_one_group() {
    // 1 MiB cannot host even a single 8-MiB (1024-byte block) group.
    let sectors = 1024 * 1024 / FMT_SECTOR as u64;
    assert_eq!(
        Ext4::format(SizedBlock::new(sectors), 64).err(),
        Some(DriverError::OutOfRange)
    );
}

#[test]
fn format_rejects_a_zero_inode_budget() {
    assert_eq!(
        Ext4::format(SizedBlock::new(ONE_GROUP_SECTORS), 0).err(),
        Some(DriverError::OutOfRange)
    );
}

#[test]
fn format_produces_a_mountable_empty_volume() {
    let mut fs = Ext4::format(SizedBlock::new(ONE_GROUP_SECTORS), 256).expect("format");
    let root = fs.root();
    assert_eq!(fs.node_info(root).expect("info").kind, NodeKind::Directory);
    // A freshly formatted root has no children (only `.`/`..`, which the
    // reader does not surface).
    let mut name = [0u8; 64];
    assert_eq!(fs.read_dir(root, 0, &mut name), Ok(None));
}

#[test]
fn format_create_write_read_roundtrips_across_a_remount() {
    let body = b"a fresh ext4 volume, formatted in RustOS\n";
    let mut fs = Ext4::format(SizedBlock::new(ONE_GROUP_SECTORS), 256).expect("format");
    let root = fs.root();
    fs.create(root, b"hello.txt", NodeKind::RegularFile)
        .expect("create");
    assert_eq!(fs.write_at(root, b"hello.txt", 0, body), Ok(body.len()));

    // Remount from the same device and read the file back.
    let mut fs = Ext4::open(fs.into_block()).expect("reopen");
    let file = fs.lookup(fs.root(), b"hello.txt").expect("found");
    assert_eq!(fs.node_info(file).expect("info").size, body.len() as u64);
    let mut buf = [0u8; 64];
    let n = fs.read_at(file, 0, &mut buf).expect("read");
    assert_eq!(&buf[..n], body);
}

#[test]
fn format_data_region_fills_to_no_space_then_recovers() {
    let mut fs = Ext4::format(SizedBlock::new(ONE_GROUP_SECTORS), 256).expect("format");
    let root = fs.root();
    let written = fill_to_no_space(&mut fs, root);
    assert!(written > 0, "expected to write at least one block");

    // The full volume is still consistent: an early file reads back its
    // first block intact.
    let file = fs.lookup(root, &nth_name(0)).expect("found");
    let mut buf = [0u8; 4096];
    assert_eq!(fs.read_at(file, 0, &mut buf), Ok(4096));
    assert!(buf.iter().all(|&b| b == 0xAB));

    // Freeing space lets allocation resume (NoSpace is not terminal):
    // removing a file frees its blocks, after which a write succeeds.
    fs.remove(root, &nth_name(0)).expect("remove");
    fs.create(root, b"after", NodeKind::RegularFile)
        .expect("create after free");
    let chunk = [0x11u8; 4096];
    assert_eq!(fs.write_at(root, b"after", 0, &chunk), Ok(chunk.len()));
}

#[test]
fn format_inode_table_fills_to_no_space() {
    // 16 inodes per group, 10 reserved (1..=10) → exactly 6 usable.
    let mut fs = Ext4::format(SizedBlock::new(ONE_GROUP_SECTORS), 16).expect("format");
    let root = fs.root();
    let mut created = 0usize;
    loop {
        let name = nth_name(created);
        match fs.create(root, &name, NodeKind::RegularFile) {
            Ok(_) => created += 1,
            Err(DriverError::NoSpace) => break,
            Err(e) => panic!("unexpected error exhausting inodes: {e:?}"),
        }
        assert!(created <= 64, "inode table never reported full");
    }
    assert_eq!(created, 6);
}

#[test]
fn format_spans_multiple_block_groups() {
    let mut fs = Ext4::format(SizedBlock::new(TWO_GROUP_SECTORS), 1024).expect("format");
    let root = fs.root();
    let written = fill_to_no_space(&mut fs, root);
    // One block group holds 8 MiB of data blocks; writing past that
    // proves the allocator crossed into the second block group.
    assert!(
        written > 8 * 1024 * 1024,
        "expected a multi-group fill, only wrote {written} bytes"
    );

    // Remount and confirm an early file survives, including a block in
    // the second group's address range.
    let mut fs = Ext4::open(fs.into_block()).expect("reopen");
    let file = fs.lookup(fs.root(), &nth_name(0)).expect("found");
    let mut buf = [0u8; 4096];
    assert_eq!(fs.read_at(file, 0, &mut buf), Ok(4096));
    assert!(buf.iter().all(|&b| b == 0xAB));
}
