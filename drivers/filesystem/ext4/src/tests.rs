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

use super::*;
use rustos_abi::driver::block::BlockGeometry;
use rustos_abi::DriverKind;

const FS_BLOCK: usize = 1024;
const FS_BLOCK_COUNT: usize = 15;
const IMG_LEN: usize = FS_BLOCK * FS_BLOCK_COUNT;

const DEV_SECTOR: usize = 512;
const DEV_SECTOR_COUNT: u64 = (IMG_LEN / DEV_SECTOR) as u64;

const INODE_TABLE_BLOCK: usize = 5;
const INODES_PER_GROUP: u32 = 16;
const INODE_SIZE: usize = 128;

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

/// Build the in-memory ext4 image described in the module docs.
fn build_image() -> [u8; IMG_LEN] {
    let mut img = [0u8; IMG_LEN];

    // --- Superblock at byte 1024 (block 1). ---
    let sb = usize::try_from(SUPERBLOCK_OFFSET).expect("offset fits");
    set_le32(&mut img, sb, INODES_PER_GROUP * 2); // s_inodes_count
    set_le32(&mut img, sb + 0x04, u32c(FS_BLOCK_COUNT)); // s_blocks_count_lo
    set_le32(&mut img, sb + 0x18, 0); // s_log_block_size -> 1024
    set_le32(&mut img, sb + 0x20, u32c(FS_BLOCK_COUNT)); // s_blocks_per_group
    set_le32(&mut img, sb + 0x28, INODES_PER_GROUP); // s_inodes_per_group
    set_le16(&mut img, sb + 0x38, EXT_MAGIC); // s_magic
    set_le32(&mut img, sb + 0x4C, 1); // s_rev_level (dynamic)
    set_le16(&mut img, sb + 0x58, u16c(INODE_SIZE)); // s_inode_size
    set_le32(&mut img, sb + 0x60, INCOMPAT_FILETYPE); // s_feature_incompat

    // --- Group descriptor 0 at block 2. ---
    let gd = 2 * FS_BLOCK;
    set_le32(&mut img, gd + 0x08, u32c(INODE_TABLE_BLOCK)); // bg_inode_table_lo

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
    data: [u8; IMG_LEN],
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
