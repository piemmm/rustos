//! ADFS driver unit tests.
//!
//! Every variant is formatted in memory with [`Adfs::format`],
//! exercised through the filesystem traits (round-trips, growth,
//! truncation, renames, metadata), and then deliberately corrupted —
//! map checksums, directory check bytes, zone checks, the cross-check,
//! the boot block, and big-directory structures — to prove the driver
//! fails closed rather than trusting a damaged volume.

extern crate std;

use super::*;
use std::vec;
use std::vec::Vec;
use tairix_abi::driver::block::BlockGeometry;
use tairix_abi::DriverKind;

/// In-memory block device backing the test volumes.
struct MemDisk {
    data: Vec<u8>,
    block_size: u32,
}

impl MemDisk {
    fn new(bytes: usize, block_size: u32) -> Self {
        Self {
            data: vec![0; bytes],
            block_size,
        }
    }
}

impl Block for MemDisk {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        Ok(BlockGeometry {
            block_size: self.block_size,
            block_count: (self.data.len() as u64) / u64::from(self.block_size),
        })
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        let at = usize::try_from(lba * u64::from(self.block_size))
            .map_err(|_| DriverError::LengthOutOfRange)?;
        let end = at
            .checked_add(buf.len())
            .ok_or(DriverError::LengthOutOfRange)?;
        if end > self.data.len() {
            return Err(DriverError::LengthOutOfRange);
        }
        buf.copy_from_slice(&self.data[at..end]);
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, data: &[u8]) -> Result<(), DriverError> {
        let at = usize::try_from(lba * u64::from(self.block_size))
            .map_err(|_| DriverError::LengthOutOfRange)?;
        let end = at
            .checked_add(data.len())
            .ok_or(DriverError::LengthOutOfRange)?;
        if end > self.data.len() {
            return Err(DriverError::LengthOutOfRange);
        }
        self.data[at..end].copy_from_slice(data);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        Ok(())
    }
}

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

/// Every supported variant with the device size the tests give it (a
/// hard disc gets a 4 MiB device).
const VARIANTS: [(AdfsVariant, usize); 10] = [
    (AdfsVariant::S, 160 * 1024),
    (AdfsVariant::M, 320 * 1024),
    (AdfsVariant::L, 640 * 1024),
    (AdfsVariant::D, 800 * 1024),
    (AdfsVariant::E, 800 * 1024),
    (AdfsVariant::EPlus, 800 * 1024),
    (AdfsVariant::F, 1600 * 1024),
    (AdfsVariant::FPlus, 1600 * 1024),
    (AdfsVariant::HardDisc, 4 * 1024 * 1024),
    (AdfsVariant::HardDiscPlus, 4 * 1024 * 1024),
];

fn fresh(variant: AdfsVariant, bytes: usize) -> Adfs<MemDisk> {
    match Adfs::format(MemDisk::new(bytes, 512), variant) {
        Ok(fs) => fs,
        Err(err) => panic!("{variant:?}: format failed: {err:?}"),
    }
}

fn make(fs: &mut Adfs<MemDisk>, dir: NodeId, name: &[u8], data: &[u8]) -> NodeId {
    fs.create(dir, name, NodeKind::RegularFile).expect("create");
    if !data.is_empty() {
        assert_eq!(fs.write_at(dir, name, 0, data).expect("write"), data.len());
    }
    fs.lookup(dir, name).expect("created file resolves")
}

fn read_all(fs: &mut Adfs<MemDisk>, node: NodeId) -> Vec<u8> {
    let info = fs.node_info(node).expect("node info");
    let mut out = vec![0u8; usize::try_from(info.size).expect("size fits")];
    // Read through a small window to exercise offset handling.
    let mut done = 0usize;
    while done < out.len() {
        let take = (out.len() - done).min(333);
        let got = fs
            .read_at(node, done as u64, &mut out[done..done + take])
            .expect("read");
        assert!(got > 0, "unexpected EOF at {done}");
        done += got;
    }
    out
}

/// Collect `(name, is_dir, size)` for every entry of `dir`.
fn list(fs: &mut Adfs<MemDisk>, dir: NodeId) -> Vec<(Vec<u8>, bool, u64)> {
    let mut out = Vec::new();
    let mut cursor = 0u64;
    let mut name = [0u8; 255];
    while let Some(entry) = fs.read_dir(dir, cursor, &mut name).expect("read_dir") {
        out.push((
            name[..entry.name_len].to_vec(),
            entry.info.kind == NodeKind::Directory,
            entry.info.size,
        ));
        cursor = entry.next_cursor;
    }
    out
}

#[test]
fn sparse_writes_zero_fill_the_gap() {
    for (variant, bytes) in VARIANTS {
        let mut fs = fresh(variant, bytes);
        let root = FilesystemRead::root(&fs);
        make(&mut fs, root, b"Sparse", b"head");
        // Writing past the end zero-fills between the old end and the
        // write offset.
        assert_eq!(
            fs.write_at(root, b"Sparse", 5000, b"tail").expect("sparse"),
            4
        );
        let node = fs.lookup(root, b"Sparse").expect("node");
        let body = read_all(&mut fs, node);
        assert_eq!(&body[..4], b"head", "{variant:?}");
        assert!(body[4..5000].iter().all(|&b| b == 0), "{variant:?}: gap");
        assert_eq!(&body[5000..], b"tail", "{variant:?}");
    }
}

#[test]
fn truncate_grows_shrinks_and_frees() {
    for (variant, bytes) in VARIANTS {
        let mut fs = fresh(variant, bytes);
        let root = FilesystemRead::root(&fs);
        let body: Vec<u8> = (0..20_000u32).map(|i| (i % 241) as u8).collect();
        make(&mut fs, root, b"File", &body);
        let free_full = fs.stats().expect("stats").free_blocks;

        // Shrink: the survivor keeps its data and space returns.
        fs.truncate(root, b"File", 700).expect("shrink");
        let node = fs.lookup(root, b"File").expect("node");
        assert_eq!(read_all(&mut fs, node), &body[..700], "{variant:?}");
        assert!(
            fs.stats().expect("stats").free_blocks > free_full,
            "{variant:?}: shrink frees space"
        );

        // Grow: the tail is zero-filled.
        fs.truncate(root, b"File", 2000).expect("grow");
        let node = fs.lookup(root, b"File").expect("node");
        let grown = read_all(&mut fs, node);
        assert_eq!(&grown[..700], &body[..700], "{variant:?}");
        assert!(grown[700..].iter().all(|&b| b == 0), "{variant:?}: zeros");

        // Truncate to nothing releases the whole allocation.
        fs.truncate(root, b"File", 0).expect("empty");
        let node = fs.lookup(root, b"File").expect("node");
        let info = fs.node_info(node).expect("info");
        assert_eq!((info.size, info.allocated), (0, 0), "{variant:?}");

        // A directory cannot be truncated.
        fs.create(root, b"Dir", NodeKind::Directory).expect("mkdir");
        assert_eq!(
            fs.truncate(root, b"Dir", 0),
            Err(DriverError::Unsupported),
            "{variant:?}"
        );
    }
}

#[test]
fn remove_reclaims_space_and_names() {
    for (variant, bytes) in VARIANTS {
        let mut fs = fresh(variant, bytes);
        let root = FilesystemRead::root(&fs);
        let baseline = fs.stats().expect("stats").free_blocks;
        make(&mut fs, root, b"Doomed", &vec![7u8; 30_000]);
        let sub = fs.create(root, b"Nest", NodeKind::Directory).expect("dir");
        make(&mut fs, sub, b"child", b"x");

        // A populated directory refuses removal; its child does not.
        assert_eq!(
            fs.remove(root, b"Nest"),
            Err(DriverError::Busy),
            "{variant:?}"
        );
        fs.remove(sub, b"child").expect("remove child");
        fs.remove(root, b"Nest").expect("remove emptied dir");
        fs.remove(root, b"Doomed").expect("remove file");
        assert_eq!(fs.remove(root, b"Doomed"), Err(DriverError::NotFound));

        // Every block returns and the names are reusable.
        assert_eq!(
            fs.stats().expect("stats").free_blocks,
            baseline,
            "{variant:?}: all space reclaimed"
        );
        make(&mut fs, root, b"Doomed", b"fresh");
        let node = fs.lookup(root, b"doomed").expect("reused name");
        assert_eq!(read_all(&mut fs, node), b"fresh", "{variant:?}");
    }
}

#[test]
fn rename_moves_replaces_and_guards_cycles() {
    for (variant, bytes) in VARIANTS {
        let mut fs = fresh(variant, bytes);
        let root = FilesystemRead::root(&fs);
        let one = fs.create(root, b"One", NodeKind::Directory).expect("dir");
        let two = fs.create(root, b"Two", NodeKind::Directory).expect("dir");
        make(&mut fs, one, b"file", b"payload");

        // A plain rename within a directory.
        fs.rename(one, b"file", one, b"named").expect("rename");
        assert_eq!(fs.lookup(one, b"file"), Err(DriverError::NotFound));
        let node = fs.lookup(one, b"named").expect("renamed");
        assert_eq!(read_all(&mut fs, node), b"payload");

        // A move across directories.
        fs.rename(one, b"named", two, b"moved").expect("move");
        let node = fs.lookup(two, b"moved").expect("moved");
        assert_eq!(read_all(&mut fs, node), b"payload");

        // Replacement frees the victim's space and keeps the mover's data.
        make(&mut fs, two, b"victim", &vec![3u8; 9000]);
        let before = fs.stats().expect("stats").free_blocks;
        fs.rename(two, b"moved", two, b"victim").expect("replace");
        assert!(
            fs.stats().expect("stats").free_blocks > before,
            "{variant:?}: replaced data freed"
        );
        let node = fs.lookup(two, b"victim").expect("kept");
        assert_eq!(read_all(&mut fs, node), b"payload", "{variant:?}");
        assert_eq!(fs.lookup(two, b"moved"), Err(DriverError::NotFound));

        // A directory move updates the child's parent linkage, and a
        // directory can never move into its own subtree.
        fs.rename(root, b"One", two, b"Inner").expect("move dir");
        let inner = fs.lookup(two, b"Inner").expect("inner");
        make(&mut fs, inner, b"leaf", b"leaf body");
        assert_eq!(
            fs.rename(root, b"Two", inner, b"Loop"),
            Err(DriverError::Busy),
            "{variant:?}: cycle refused"
        );

        // Kind mismatches are refused.
        assert_eq!(
            fs.rename(two, b"victim", two, b"Inner"),
            Err(DriverError::Unsupported),
            "{variant:?}: file over directory"
        );

        // Renaming an entry onto itself is a no-op.
        fs.rename(two, b"victim", two, b"VICTIM").expect("self");
        assert!(fs.lookup(two, b"victim").is_ok());
    }
}

#[test]
fn growth_survives_a_blocking_neighbour() {
    // Force the relocation path: A is pinned behind B, so growing A
    // cannot extend in place.
    for (variant, bytes) in VARIANTS {
        let mut fs = fresh(variant, bytes);
        let root = FilesystemRead::root(&fs);
        let a: Vec<u8> = (0..4000u32).map(|i| (i % 197) as u8).collect();
        make(&mut fs, root, b"A", &a);
        make(&mut fs, root, b"B", &vec![0xBB; 4000]);
        let grown: Vec<u8> = (0..90_000u32).map(|i| (i % 193) as u8).collect();
        assert_eq!(
            fs.write_at(root, b"A", 0, &grown).expect("grow A"),
            grown.len(),
            "{variant:?}"
        );
        let node = fs.lookup(root, b"A").expect("A");
        assert_eq!(read_all(&mut fs, node), grown, "{variant:?}: A grown");
        let node = fs.lookup(root, b"B").expect("B");
        assert_eq!(
            read_all(&mut fs, node),
            vec![0xBB; 4000],
            "{variant:?}: B intact"
        );
    }
}

#[test]
fn filling_the_volume_fails_closed_and_recovers() {
    let (variant, bytes) = VARIANTS[0]; // The smallest volume (S).
    let mut fs = fresh(variant, bytes);
    let root = FilesystemRead::root(&fs);
    let chunk = vec![0x5A; 32 * 1024];
    let mut created = 0u32;
    loop {
        let mut name = *b"Fill0";
        name[4] = b'0' + (created % 10) as u8;
        fs.create(root, &name, NodeKind::RegularFile)
            .expect("create");
        match fs.write_at(root, &name, 0, &chunk) {
            Ok(len) => assert_eq!(len, chunk.len()),
            Err(DriverError::NoSpace) => break,
            Err(other) => panic!("unexpected error: {other:?}"),
        }
        created += 1;
        assert!(created < 10, "volume never filled");
    }
    // The volume still validates and recovers space on delete.
    let mut name = *b"Fill0";
    name[4] = b'0';
    fs.remove(root, &name).expect("remove");
    make(&mut fs, root, b"After", b"still works");
    let node = fs.lookup(root, b"After").expect("post-full create");
    assert_eq!(read_all(&mut fs, node), b"still works");
}

#[test]
fn invalid_names_are_rejected() {
    for (variant, bytes) in [VARIANTS[0], VARIANTS[4], VARIANTS[5]] {
        let mut fs = fresh(variant, bytes);
        let root = FilesystemRead::root(&fs);
        for bad in [
            &b"has space"[..],
            b"dot.name",
            b"star*",
            b"hash#",
            b"quo\"te",
        ] {
            assert_eq!(
                fs.create(root, bad, NodeKind::RegularFile),
                Err(DriverError::OutOfRange),
                "{variant:?}: {bad:?}"
            );
        }
        assert_eq!(
            fs.create(root, b"", NodeKind::RegularFile),
            Err(DriverError::LengthOutOfRange),
            "{variant:?}: empty name"
        );
        let long = [b'x'; 300];
        assert_eq!(
            fs.create(root, &long, NodeKind::RegularFile),
            Err(DriverError::LengthOutOfRange),
            "{variant:?}: oversized name"
        );
        if fs.is_big_dir() {
            fs.create(root, b"ALongBigDirectoryName", NodeKind::RegularFile)
                .expect("big directories take long names");
        } else {
            assert_eq!(
                fs.create(root, b"ElevenChars", NodeKind::RegularFile),
                Err(DriverError::LengthOutOfRange),
                "{variant:?}: fixed directories cap names at ten bytes"
            );
        }
    }
}

#[test]
fn big_directories_grow_and_survive_remount() {
    for (variant, bytes) in [VARIANTS[5], VARIANTS[7], VARIANTS[9]] {
        let mut fs = fresh(variant, bytes);
        let root = FilesystemRead::root(&fs);
        // Enough long-named entries to overflow the initial 2048-byte
        // directory several times over.
        let count = 120u32;
        for i in 0..count {
            let mut name = *b"AVeryLongObjectName-000";
            name[20] = b'0' + (i / 100 % 10) as u8;
            name[21] = b'0' + (i / 10 % 10) as u8;
            name[22] = b'0' + (i % 10) as u8;
            make(&mut fs, root, &name, &i.to_le_bytes());
        }
        assert_eq!(list(&mut fs, root).len(), count as usize, "{variant:?}");
        // The grown root (its size now lives in the disc record)
        // survives a remount, and every entry still resolves.
        let mut fs = Adfs::open(fs.volume.into_device()).expect("remount");
        let root = FilesystemRead::root(&fs);
        for i in 0..count {
            let mut name = *b"AVeryLongObjectName-000";
            name[20] = b'0' + (i / 100 % 10) as u8;
            name[21] = b'0' + (i / 10 % 10) as u8;
            name[22] = b'0' + (i % 10) as u8;
            let node = fs.lookup(root, &name).expect("entry survives");
            assert_eq!(read_all(&mut fs, node), i.to_le_bytes(), "{variant:?}");
        }
        // Removal keeps the heap and pointers coherent.
        for i in (0..count).step_by(2) {
            let mut name = *b"AVeryLongObjectName-000";
            name[20] = b'0' + (i / 100 % 10) as u8;
            name[21] = b'0' + (i / 10 % 10) as u8;
            name[22] = b'0' + (i % 10) as u8;
            fs.remove(root, &name).expect("remove");
        }
        assert_eq!(list(&mut fs, root).len(), count as usize / 2, "{variant:?}");
    }
}

/// Format a variant, populate it lightly, and hand back the raw image.
fn image_of(variant: AdfsVariant, bytes: usize) -> Vec<u8> {
    let mut fs = fresh(variant, bytes);
    let root = FilesystemRead::root(&fs);
    make(&mut fs, root, b"File", b"survivor");
    fs.create(root, b"Dir", NodeKind::Directory).expect("dir");
    fs.volume.into_device().data
}

/// Reopen an image, asserting the driver rejects it as corrupt.
fn assert_rejected(variant: AdfsVariant, data: Vec<u8>, what: &str) {
    let device = MemDisk {
        data,
        block_size: 512,
    };
    match Adfs::open(device) {
        Err(DriverError::BadMagic) => {}
        other => panic!(
            "{variant:?}: {what}: expected BadMagic, got {other:?}",
            other = other.map(|_| "Ok")
        ),
    }
}

#[test]
fn corrupt_old_map_checksums_are_rejected() {
    for variant in [
        AdfsVariant::S,
        AdfsVariant::M,
        AdfsVariant::L,
        AdfsVariant::D,
    ] {
        let bytes = usize::try_from(variant.fixed_size().expect("floppy")).expect("fits");
        // Sector 0 checksum (free-space starts).
        let mut data = image_of(variant, bytes);
        data[0x40] ^= 0x01;
        assert_rejected(variant, data, "sector 0 corruption");
        // Sector 1 checksum (free-space lengths).
        let mut data = image_of(variant, bytes);
        data[0x140] ^= 0x01;
        assert_rejected(variant, data, "sector 1 corruption");
        // A free-space entry pointing past the disc.
        let mut data = image_of(variant, bytes);
        data[0x00] = 0xFF;
        data[0x01] = 0xFF;
        data[0x02] = 0x1F;
        assert_rejected(variant, data, "out-of-range free area");
    }
}

#[test]
fn corrupt_directories_are_rejected() {
    // The root's check byte, marker, and sequence bytes are validated
    // on every load; damage to any of them refuses the volume.
    for (variant, offset) in [
        (AdfsVariant::S, 0x200usize), // root at sector 2
        (AdfsVariant::D, 0x400),      // root at sector 4
    ] {
        let bytes = usize::try_from(variant.fixed_size().expect("floppy")).expect("fits");
        // Marker byte.
        let mut data = image_of(variant, bytes);
        data[offset + 1] = b'X';
        assert_rejected(variant, data, "root marker");
        // A name byte inside a live entry breaks the check byte.
        let mut data = image_of(variant, bytes);
        data[offset + 5] ^= 0x01;
        assert_rejected(variant, data, "root entry bytes");
        // Mismatched head/tail sequence numbers.
        let mut data = image_of(variant, bytes);
        data[offset] = data[offset].wrapping_add(1);
        assert_rejected(variant, data, "sequence mismatch");
    }
}

#[test]
fn corrupt_new_map_zones_are_rejected() {
    // E: single zone at the disc start.
    let bytes = 800 * 1024;
    let mut data = image_of(AdfsVariant::E, bytes);
    data[0x40] ^= 0x01; // Map bits: breaks the zone check.
    assert_rejected(AdfsVariant::E, data, "zone check");

    // The cross-check byte of zone 0 must XOR with the others to 0xFF.
    let mut data = image_of(AdfsVariant::E, bytes);
    let map_at = 0usize;
    data[map_at + 3] ^= 0x10;
    assert_rejected(AdfsVariant::E, data, "cross-check");

    // A disc record claiming an impossible geometry.
    let mut data = image_of(AdfsVariant::E, bytes);
    data[4] = 42; // log2secsize
    assert_rejected(AdfsVariant::E, data, "impossible disc record");
}

#[test]
fn corrupt_boot_blocks_fall_back_and_fail_closed() {
    // F carries its disc record in the boot block; damaging the record
    // while fixing the checksum must still be rejected (zone checks),
    // and damaging the checksum makes the volume unrecognisable.
    let bytes = 1600 * 1024;
    let mut data = image_of(AdfsVariant::F, bytes);
    data[0xC00 + 0x1FF] ^= 0xFF; // Checksum byte.
    assert_rejected(AdfsVariant::F, data, "boot block checksum");

    let mut data = image_of(AdfsVariant::F, bytes);
    // Point the root at the free-space id, re-fixing the checksum so
    // only the record's content is wrong.
    data[0xC00 + 0x1C0 + 0x0C] = 0x00;
    data[0xC00 + 0x1C0 + 0x0D] = 0x00;
    let mut block = [0u8; 512];
    block.copy_from_slice(&data[0xC00..0xE00]);
    block[511] = 0;
    let sum = disc::boot_block_checksum(&block);
    data[0xC00 + 0x1FF] = sum;
    assert_rejected(AdfsVariant::F, data, "hostile disc record");
}

#[test]
fn corrupt_big_directories_are_rejected() {
    let bytes = 800 * 1024;
    let base = image_of(AdfsVariant::EPlus, bytes);
    // Find the root directory ("SBPr") in the image.
    let root_at = base
        .windows(4)
        .position(|w| w == b"SBPr")
        .expect("root header present")
        - 4;

    // Header marker.
    let mut data = base.clone();
    data[root_at + 4] = b'Z';
    assert_rejected(AdfsVariant::EPlus, data, "big dir start marker");

    // Version bytes must be zero.
    let mut data = base.clone();
    data[root_at + 1] = 9;
    assert_rejected(AdfsVariant::EPlus, data, "big dir version");

    // An entry count far past what the directory can hold.
    let mut data = base.clone();
    data[root_at + 16] = 0xFF;
    data[root_at + 17] = 0xFF;
    assert_rejected(AdfsVariant::EPlus, data, "big dir entry count");

    // A name heap byte breaks the check byte.
    let mut data = base.clone();
    data[root_at + 0x1C] ^= 0x01;
    assert_rejected(AdfsVariant::EPlus, data, "big dir name heap");

    // Tail marker ("oven" at the directory end).
    let oven_at = base
        .windows(4)
        .position(|w| w == b"oven")
        .expect("root tail present");
    let mut data = base.clone();
    data[oven_at] = b'!';
    assert_rejected(AdfsVariant::EPlus, data, "big dir tail marker");
}

#[test]
fn corrupt_entry_pointers_fail_closed_at_use() {
    // An entry whose indirect address points at the free-space id: the
    // volume opens (the directory is structurally intact only if the
    // check byte matches, so rewrite it), and using the entry fails
    // closed instead of reading arbitrary bytes.
    let mut fs = fresh(AdfsVariant::E, 800 * 1024);
    let root = FilesystemRead::root(&fs);
    make(&mut fs, root, b"File", b"data");
    // Forge the entry through the driver's own directory codec so the
    // check byte stays valid.
    let (index, mut object) = fs
        .dir_lookup(node_addr(root), 0, b"File")
        .expect("io")
        .expect("found");
    object.indaddr = 0x00F0 << 8; // An id the map never allocated.
    fs.dir_update_at(node_addr(root), 0, index, &object)
        .expect("forge");
    let node = fs.lookup(root, b"File").expect("entry resolves");
    assert_eq!(
        fs.read_at(node, 0, &mut [0u8; 16]),
        Err(DriverError::BadMagic),
        "dangling indirect address fails closed"
    );
}

/// `get_attr` into a `Vec`, growing the buffer to the returned length.
fn attr(fs: &mut Adfs<MemDisk>, node: NodeId, key: &[u8]) -> Option<Vec<u8>> {
    let mut buf = [0u8; 64];
    fs.get_attr(node, key, &mut buf)
        .expect("get_attr")
        .map(|len| buf[..len].to_vec())
}

#[test]
fn acorn_metadata_round_trips() {
    for (variant, bytes) in VARIANTS {
        let mut fs = fresh(variant, bytes);
        let root = FilesystemRead::root(&fs);
        let node = make(&mut fs, root, b"Typed", b"body");

        // A fresh file is untyped: raw addresses, no filetype or stamp.
        assert_eq!(
            attr(&mut fs, node, b"acorn.loadaddr").as_deref(),
            Some(&b"00000000"[..])
        );
        assert_eq!(attr(&mut fs, node, b"acorn.filetype"), None);
        assert_eq!(attr(&mut fs, node, b"acorn.datestamp"), None);
        assert_eq!(
            attr(&mut fs, node, b"acorn.attr").as_deref(),
            Some(&b"RW/"[..])
        );
        assert_eq!(fs.times(node).expect("times"), NodeTimes::default());

        // Typing the file, then stamping it, round-trips exactly.
        fs.set_attr(node, b"acorn.filetype", b"ffb")
            .expect("filetype");
        fs.set_attr(node, b"acorn.datestamp", b"00a1b2c3d4")
            .expect("stamp");
        assert_eq!(
            attr(&mut fs, node, b"acorn.filetype").as_deref(),
            Some(&b"ffb"[..])
        );
        assert_eq!(
            attr(&mut fs, node, b"acorn.datestamp").as_deref(),
            Some(&b"00a1b2c3d4"[..]),
            "{variant:?}"
        );
        // The stamp now decodes as the node's times and listing stamp.
        let stamp = acorn::centiseconds_to_time64(0x00_A1B2_C3D4).expect("in range");
        assert_eq!(fs.times(node).expect("times").modified, stamp);

        // Attribute letters round-trip within the format's storable set.
        fs.set_attr(node, b"acorn.attr", b"RWL/r").expect("attr");
        assert_eq!(
            attr(&mut fs, node, b"acorn.attr").as_deref(),
            Some(&b"RWL/r"[..])
        );
        // The directory bit cannot be forged onto a file.
        assert_eq!(
            fs.set_attr(node, b"acorn.attr", b"RWD/"),
            Err(DriverError::OutOfRange),
            "{variant:?}"
        );

        // Raw addresses are settable on untyped objects and reported
        // exactly.
        fs.remove_attr(node, b"acorn.filetype").expect("untype");
        assert_eq!(attr(&mut fs, node, b"acorn.filetype"), None);
        fs.set_attr(node, b"acorn.loadaddr", b"0000eafe")
            .expect("load");
        fs.set_attr(node, b"acorn.execaddr", b"0000eb00")
            .expect("exec");
        assert_eq!(
            attr(&mut fs, node, b"acorn.loadaddr").as_deref(),
            Some(&b"0000eafe"[..])
        );
        assert_eq!(
            attr(&mut fs, node, b"acorn.execaddr").as_deref(),
            Some(&b"0000eb00"[..])
        );

        // Enumeration lists exactly the present keys.
        let mut keys = Vec::new();
        let mut index = 0u64;
        let mut key_buf = [0u8; 32];
        while let Some(len) = fs.list_attr(node, index, &mut key_buf).expect("list") {
            keys.push(key_buf[..len].to_vec());
            index += 1;
        }
        assert_eq!(
            keys,
            vec![
                b"acorn.loadaddr".to_vec(),
                b"acorn.execaddr".to_vec(),
                b"acorn.attr".to_vec()
            ],
            "{variant:?}"
        );

        // Foreign namespaces have nowhere to live; malformed keys and
        // values fail closed.
        assert_eq!(attr(&mut fs, node, b"user.comment"), None);
        assert_eq!(
            fs.set_attr(node, b"user.comment", b"x"),
            Err(DriverError::Unsupported)
        );
        assert_eq!(
            fs.get_attr(node, b"not-a-key", &mut [0u8; 8]),
            Err(DriverError::OutOfRange)
        );
        assert_eq!(
            fs.set_attr(node, b"acorn.filetype", b"zz"),
            Err(DriverError::OutOfRange)
        );
        assert_eq!(
            fs.get_attr(node, b"acorn.loadaddr", &mut [0u8; 4]),
            Err(DriverError::BufferTooSmall)
        );
    }
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
fn format_open_roundtrip_every_variant() {
    for (variant, bytes) in VARIANTS {
        let mut fs = fresh(variant, bytes);
        let root = FilesystemRead::root(&fs);
        assert!(node_is_dir(root), "{variant:?}: root is a directory");
        // A fresh volume has an empty root and sane space accounting.
        assert!(list(&mut fs, root).is_empty(), "{variant:?}: empty root");
        let stats = fs.stats().expect("stats");
        assert!(stats.total_blocks > 0, "{variant:?}: total space");
        assert!(
            stats.free_blocks > 0 && stats.free_blocks <= stats.total_blocks,
            "{variant:?}: free within total"
        );
        assert_eq!(stats.avail_blocks, stats.free_blocks);

        // Create, write, and read back a file whose body crosses the
        // staging-buffer and allocation-unit boundaries.
        let body: Vec<u8> = (0..9000u32).map(|i| (i * 31 % 251) as u8).collect();
        let node = make(&mut fs, root, b"Data", &body);
        assert_eq!(read_all(&mut fs, node), body, "{variant:?}: body");
        let info = fs.node_info(node).expect("info");
        assert_eq!(info.size, body.len() as u64);
        assert!(
            info.allocated >= info.size,
            "{variant:?}: allocation covers the data"
        );

        // Lookups are case-insensitive, misses fail closed.
        assert_eq!(fs.lookup(root, b"dAtA").expect("case fold"), node);
        assert_eq!(fs.lookup(root, b"Other"), Err(DriverError::NotFound));

        // The listing reflects the file, and reopening the same device
        // sees the identical volume.
        let listed = list(&mut fs, root);
        assert_eq!(listed, vec![(b"Data".to_vec(), false, body.len() as u64)]);
        let device = fs.volume.into_device();
        let mut reopened = Adfs::open(device).expect("reopen");
        let node = reopened
            .lookup(FilesystemRead::root(&reopened), b"Data")
            .expect("file survives remount");
        assert_eq!(read_all(&mut reopened, node), body, "{variant:?}: remount");
    }
}

#[test]
fn directories_nest_and_list_sorted() {
    for (variant, bytes) in VARIANTS {
        let mut fs = fresh(variant, bytes);
        let root = FilesystemRead::root(&fs);
        let sub = fs.create(root, b"Sub", NodeKind::Directory).expect("mkdir");
        assert!(node_is_dir(sub));
        make(&mut fs, sub, b"inner", b"inner body");
        // Insertion keeps the case-insensitive sort order.
        make(&mut fs, root, b"zz", b"");
        make(&mut fs, root, b"AA", b"");
        let names: Vec<Vec<u8>> = list(&mut fs, root).into_iter().map(|(n, _, _)| n).collect();
        assert_eq!(
            names,
            vec![b"AA".to_vec(), b"Sub".to_vec(), b"zz".to_vec()],
            "{variant:?}: sorted listing"
        );
        // The nested file resolves through its directory.
        let inner = fs.lookup(sub, b"INNER").expect("nested lookup");
        assert_eq!(read_all(&mut fs, inner), b"inner body");
        // A directory cannot be read as a file, nor a file listed.
        assert_eq!(
            fs.read_at(sub, 0, &mut [0u8; 8]),
            Err(DriverError::Unsupported),
            "{variant:?}: read_at on dir"
        );
        let file = fs.lookup(sub, b"inner").expect("file");
        assert_eq!(
            fs.read_dir(file, 0, &mut [0u8; 32]),
            Err(DriverError::Unsupported)
        );
    }
}
