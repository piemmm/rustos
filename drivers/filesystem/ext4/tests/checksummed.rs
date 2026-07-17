//! Integration tests for ext4 **checksummed** / wide-descriptor
//! mutation, run against real `mke2fs 1.47.0` images committed under
//! `tests/fixtures/`.
//!
//! `metadata_csum.img` carries `metadata_csum,extent,64bit` (so it
//! exercises the crc32c family *and* the 64-byte group descriptor);
//! `gdt_csum.img` carries the legacy `uninit_bg` (crc16) group-descriptor
//! checksum. Both are 64 KiB single-group volumes with 256-byte inodes.
//!
//! The verifier below recomputes every on-disk checksum with an
//! **independent** crc implementation (distinct from the driver's). The
//! pristine-image test proves that independent crc reproduces exactly what
//! `mke2fs` wrote; the mutation tests then prove the *driver* wrote
//! correct checksums after `create`/`write`/`truncate`/`remove`, without
//! trusting the driver's own checksum code.

// This test does raw little-endian byte arithmetic over a 64 KiB on-disk
// image; the device geometry and every offset are far inside `u32`, so
// the width-narrowing casts are exact for these fixtures.
#![allow(clippy::cast_possible_truncation, clippy::cast_lossless)]

use tairix_abi::driver::block::{Block, BlockGeometry};
use tairix_abi::driver::filesystem::{FilesystemRead, FilesystemWrite, NodeKind};
use tairix_abi::DriverError;
use tairix_drv_fs_ext4::Ext4;

const META_CSUM: &[u8] = include_bytes!("fixtures/metadata_csum.img");
const GDT_CSUM: &[u8] = include_bytes!("fixtures/gdt_csum.img");

const SECTOR: usize = 512;

/// In-memory [`Block`] device over an ext4 image with a 512-byte logical
/// sector distinct from the 1024-byte filesystem block.
struct MemBlock {
    data: Vec<u8>,
}

impl Block for MemBlock {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        Ok(BlockGeometry {
            block_size: SECTOR as u32,
            block_count: (self.data.len() / SECTOR) as u64,
        })
    }
    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        let start = lba as usize * SECTOR;
        let end = start + buf.len();
        buf.copy_from_slice(&self.data[start..end]);
        Ok(())
    }
    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        let start = lba as usize * SECTOR;
        let end = start + buf.len();
        self.data[start..end].copy_from_slice(buf);
        Ok(())
    }
}

// --- Independent reference checksums (NOT the driver's). ---

fn crc32c(mut crc: u32, data: &[u8]) -> u32 {
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0x82F6_3B78
            } else {
                crc >> 1
            };
        }
    }
    crc
}

fn crc16(mut crc: u16, data: &[u8]) -> u16 {
    for &b in data {
        crc ^= u16::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xA001
            } else {
                crc >> 1
            };
        }
    }
    crc
}

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// Decoded geometry the verifier needs.
struct Geom {
    bs: usize,
    seed: u32,
    uuid: [u8; 16],
    metadata_csum: bool,
    gdt_csum: bool,
    desc_size: usize,
    inode_size: usize,
    inodes_per_group: u32,
    blocks_per_group: u32,
    group_count: u64,
    gdt_off: usize,
}

fn geom(img: &[u8]) -> Geom {
    let sb = &img[1024..2048];
    let bs = 1024usize << le32(sb, 0x18);
    let mut uuid = [0u8; 16];
    uuid.copy_from_slice(&sb[0x68..0x68 + 16]);
    let ro = le32(sb, 0x64);
    let incompat = le32(sb, 0x60);
    let is_64bit = incompat & 0x80 != 0;
    let metadata_csum = ro & 0x400 != 0;
    let gdt_csum = !metadata_csum && ro & 0x10 != 0;
    let desc_size = if is_64bit {
        usize::from(le16(sb, 0xFE)).max(32)
    } else {
        32
    };
    let blocks_per_group = le32(sb, 0x20);
    let blocks_count = u64::from(le32(sb, 0x04));
    let group_count = blocks_count.div_ceil(u64::from(blocks_per_group));
    let gdt_off = if bs == 1024 { 2 * bs } else { bs };
    Geom {
        bs,
        seed: crc32c(0xFFFF_FFFF, &uuid),
        uuid,
        metadata_csum,
        gdt_csum,
        desc_size,
        inode_size: usize::from(le16(sb, 0x58)),
        inodes_per_group: le32(sb, 0x28),
        blocks_per_group,
        group_count,
        gdt_off,
    }
}

fn bitmap_bytes(bits: u32, bs: usize) -> usize {
    (bits as usize).div_ceil(8).min(bs)
}

/// Recompute every on-disk checksum and assert it matches what is stored.
/// Panics with a descriptive message on the first mismatch.
fn verify_all(img: &[u8]) {
    let g = geom(img);

    // Superblock checksum (metadata_csum only).
    if g.metadata_csum {
        let c = crc32c(0xFFFF_FFFF, &img[1024..1024 + 0x3FC]);
        assert_eq!(c, le32(img, 1024 + 0x3FC), "superblock checksum");
    }

    for group in 0..g.group_count {
        let d0 = g.gdt_off + group as usize * g.desc_size;
        let desc = &img[d0..d0 + g.desc_size];
        let group_le = (group as u32).to_le_bytes();

        // Group-descriptor checksum.
        if g.metadata_csum {
            let mut c = crc32c(g.seed, &group_le);
            c = crc32c(c, &desc[..0x1E]);
            c = crc32c(c, &[0, 0]);
            if g.desc_size > 0x20 {
                c = crc32c(c, &desc[0x20..g.desc_size]);
            }
            assert_eq!(c as u16, le16(desc, 0x1E), "group {group} bg_checksum");
        } else if g.gdt_csum {
            let mut c = crc16(0xFFFF, &g.uuid);
            c = crc16(c, &group_le);
            c = crc16(c, &desc[..0x1E]);
            if g.desc_size > 0x20 {
                c = crc16(c, &desc[0x20..g.desc_size]);
            }
            assert_eq!(c, le16(desc, 0x1E), "group {group} gdt crc16");
        }

        if !g.metadata_csum {
            continue;
        }
        let wide = g.desc_size > 0x3C;
        // Block- and inode-bitmap checksums.
        let bbmp = u64::from(le32(desc, 0x00)) as usize * g.bs;
        let bn = bitmap_bytes(g.blocks_per_group, g.bs);
        let bc = crc32c(g.seed, &img[bbmp..bbmp + bn]);
        let want = u32::from(le16(desc, 0x18))
            | if wide {
                u32::from(le16(desc, 0x38)) << 16
            } else {
                0
            };
        let got = bc & if wide { 0xFFFF_FFFF } else { 0xFFFF };
        assert_eq!(got, want, "group {group} block-bitmap csum");

        let ibmp = u64::from(le32(desc, 0x04)) as usize * g.bs;
        let inn = bitmap_bytes(g.inodes_per_group, g.bs);
        let ic = crc32c(g.seed, &img[ibmp..ibmp + inn]);
        let iwant = u32::from(le16(desc, 0x1A))
            | if wide {
                u32::from(le16(desc, 0x3A)) << 16
            } else {
                0
            };
        let igot = ic & if wide { 0xFFFF_FFFF } else { 0xFFFF };
        assert_eq!(igot, iwant, "group {group} inode-bitmap csum");
    }

    if g.metadata_csum {
        verify_inodes_and_dirs(img, &g);
    }
}

/// crc32c seed for inode `ino` with generation `gen`.
fn inode_seed(g: &Geom, ino: u32, gen: u32) -> u32 {
    let c = crc32c(g.seed, &ino.to_le_bytes());
    crc32c(c, &gen.to_le_bytes())
}

fn inode_table_block(img: &[u8], g: &Geom, group: u64) -> u64 {
    let d0 = g.gdt_off + group as usize * g.desc_size;
    let lo = u64::from(le32(&img[d0..], 0x08));
    let hi = if g.desc_size >= 0x2C {
        u64::from(le32(&img[d0..], 0x28))
    } else {
        0
    };
    (hi << 32) | lo
}

fn inode_in_use(img: &[u8], g: &Geom, ino: u32) -> bool {
    let group = u64::from(ino - 1) / u64::from(g.inodes_per_group);
    let bit = (u64::from(ino - 1) % u64::from(g.inodes_per_group)) as usize;
    let d0 = g.gdt_off + group as usize * g.desc_size;
    let bmp = u64::from(le32(&img[d0..], 0x04)) as usize * g.bs;
    img[bmp + bit / 8] & (1 << (bit % 8)) != 0
}

/// Verify every in-use inode's checksum, and every directory-leaf block's
/// tail checksum (directories here are depth-0 inline-extent mapped).
fn verify_inodes_and_dirs(img: &[u8], g: &Geom) {
    let total = g.inodes_per_group as u64 * g.group_count;
    for ino in 1..=total as u32 {
        if !inode_in_use(img, g, ino) {
            continue;
        }
        let group = u64::from(ino - 1) / u64::from(g.inodes_per_group);
        let idx = (u64::from(ino - 1) % u64::from(g.inodes_per_group)) as usize;
        let off = inode_table_block(img, g, group) as usize * g.bs + idx * g.inode_size;
        let raw = &img[off..off + g.inode_size];

        // Inode checksum (lo at 0x7C, hi at 0x82 when extra_isize covers it).
        let gen = le32(raw, 0x64);
        let has_hi = g.inode_size > 128 && le16(raw, 0x80) >= 4;
        let mut work = raw.to_vec();
        work[0x7C] = 0;
        work[0x7D] = 0;
        if has_hi {
            work[0x82] = 0;
            work[0x83] = 0;
        }
        let c = crc32c(inode_seed(g, ino, gen), &work);
        assert_eq!(c as u16, le16(raw, 0x7C), "inode {ino} csum lo");
        if has_hi {
            assert_eq!((c >> 16) as u16, le16(raw, 0x82), "inode {ino} csum hi");
        }

        // Directory-leaf tails (depth-0 inline extent map only).
        let mode = le16(raw, 0);
        if mode & 0xF000 != 0x4000 {
            continue;
        }
        let ib = 40usize;
        if le16(raw, ib) != 0xF30A || le16(raw, ib + 6) != 0 {
            continue;
        }
        let entries = usize::from(le16(raw, ib + 2));
        let seed = inode_seed(g, ino, gen);
        for e in 0..entries {
            let eo = ib + 12 + e * 12;
            let len = le16(raw, eo + 4);
            let len = if len > 32_768 { len - 32_768 } else { len };
            let phys = (u64::from(le16(raw, eo + 6)) << 32) | u64::from(le32(raw, eo + 8));
            for b in 0..u64::from(len) {
                let blk = (phys + b) as usize * g.bs;
                let block = &img[blk..blk + g.bs];
                let tail = g.bs - 12;
                let cc = crc32c(seed, &block[..tail]);
                assert_eq!(cc, le32(block, g.bs - 4), "dir inode {ino} block tail");
            }
        }
    }
}

/// Drive a representative create/write/truncate/mkdir/remove cycle and
/// hand back the resulting image bytes.
fn mutate(fixture: &[u8]) -> Vec<u8> {
    let mut fs = Ext4::open(MemBlock {
        data: fixture.to_vec(),
    })
    .expect("open fixture");
    let root = fs.root();

    let file = fs
        .create(root, b"report.txt", NodeKind::RegularFile)
        .expect("create file");
    let body = b"checksummed ext4 mutation round-trip, spanning blocks.\n";
    assert_eq!(fs.write_at(root, b"report.txt", 0, body), Ok(body.len()));

    // A multi-block write (crosses several 1 KiB blocks).
    let big = vec![0x5Au8; 3500];
    assert_eq!(fs.write_at(root, b"report.txt", 0, &big), Ok(big.len()));

    // Read it back through the driver.
    let mut rb = vec![0u8; big.len()];
    assert_eq!(fs.read_at(file, 0, &mut rb), Ok(big.len()));
    assert_eq!(rb, big);

    // Shrink, then a subdirectory create + remove.
    fs.truncate(root, b"report.txt", 1000).expect("truncate");
    fs.create(root, b"sub", NodeKind::Directory).expect("mkdir");
    fs.remove(root, b"sub").expect("rmdir");

    fs.into_block().data
}

#[test]
fn reference_crc_matches_known_vector() {
    // CRC-32C of "123456789" with the Linux (no final-xor) convention.
    assert_eq!(crc32c(0xFFFF_FFFF, b"123456789"), 0x1CF9_6D7C);
}

#[test]
fn pristine_metadata_csum_image_verifies() {
    // Proves the independent reference crc reproduces exactly what mke2fs
    // stored, so it is a trustworthy oracle for the mutation tests.
    verify_all(META_CSUM);
}

#[test]
fn pristine_gdt_csum_image_verifies() {
    verify_all(GDT_CSUM);
}

#[test]
fn metadata_csum_volume_is_mutable_and_stays_consistent() {
    let img = mutate(META_CSUM);
    verify_all(&img);

    // The mutation also survives a fresh mount (data round-trips).
    let mut fs = Ext4::open(MemBlock { data: img }).expect("remount");
    let file = fs.lookup(fs.root(), b"report.txt").expect("lookup");
    assert_eq!(fs.node_info(file).expect("info").size, 1000);
    let mut buf = vec![0u8; 16];
    let n = fs.read_at(file, 0, &mut buf).expect("read");
    assert_eq!(&buf[..n], &[0x5Au8; 16]);
    assert_eq!(fs.lookup(fs.root(), b"sub"), Err(DriverError::NotFound));
}

#[test]
fn gdt_csum_volume_is_mutable_and_stays_consistent() {
    let img = mutate(GDT_CSUM);
    verify_all(&img);

    let mut fs = Ext4::open(MemBlock { data: img }).expect("remount");
    let file = fs.lookup(fs.root(), b"report.txt").expect("lookup");
    assert_eq!(fs.node_info(file).expect("info").size, 1000);
}
