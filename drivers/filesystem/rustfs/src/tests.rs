//! Unit tests for the copy-on-write rustfs driver (`AGENTS.md` §7, §16).
//!
//! They exercise the Stage-1 foundation — format → open round-trip, the
//! superblock-ring selection, and crash replay at every write count during a
//! commit — plus the full read/write surface the VFS consumes, all over an
//! in-memory [`MemBlock`] double.

use super::*;
use rustos_abi::driver::filesystem::{
    FilesystemRead, FilesystemSecurity, FilesystemTimestamps, FilesystemWrite, NodeKind,
};

/// In-memory block device. Optionally drops writes once a budget is reached,
/// modelling a power loss mid-commit: a dropped write simply never reaches the
/// platter, and the driver's in-memory state is discarded by re-opening from
/// the stored bytes.
struct MemBlock {
    store: alloc::vec::Vec<u8>,
    block_size: u32,
    block_count: u64,
    writes: u32,
    write_budget: Option<u32>,
}

impl MemBlock {
    fn new(block_size: u32, block_count: u64) -> Self {
        let len = block_size as usize * as_usize(block_count);
        Self {
            store: alloc::vec![0u8; len],
            block_size,
            block_count,
            writes: 0,
            write_budget: None,
        }
    }

    fn from_bytes(bytes: alloc::vec::Vec<u8>, block_size: u32, block_count: u64) -> Self {
        Self {
            store: bytes,
            block_size,
            block_count,
            writes: 0,
            write_budget: None,
        }
    }

    fn bytes(&self) -> alloc::vec::Vec<u8> {
        self.store.clone()
    }
}

impl Block for MemBlock {
    fn geometry(&self) -> Result<rustos_abi::driver::block::BlockGeometry, DriverError> {
        Ok(rustos_abi::driver::block::BlockGeometry {
            block_size: self.block_size,
            block_count: self.block_count,
        })
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        let bs = self.block_size as usize;
        if buf.is_empty() || buf.len() % bs != 0 {
            return Err(DriverError::BufferTooSmall);
        }
        let start = as_usize(lba) * bs;
        let end = start + buf.len();
        if end > self.store.len() {
            return Err(DriverError::LengthOutOfRange);
        }
        buf.copy_from_slice(&self.store[start..end]);
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        let bs = self.block_size as usize;
        if buf.is_empty() || buf.len() % bs != 0 {
            return Err(DriverError::BufferTooSmall);
        }
        let start = as_usize(lba) * bs;
        let end = start + buf.len();
        if end > self.store.len() {
            return Err(DriverError::LengthOutOfRange);
        }
        // Model power loss: once the budget is spent, the write never lands.
        let drop_write = matches!(self.write_budget, Some(b) if self.writes >= b);
        self.writes += 1;
        if !drop_write {
            self.store[start..end].copy_from_slice(buf);
        }
        Ok(())
    }
}

fn fixed_clock() -> Time64 {
    Time64::from_secs(1_700_000_000)
}

/// The volume key every test formats and reopens with. `RustFS` has no
/// plaintext mode (`docs/src/filesystem/rustfs-spec.md` §5), so every test
/// volume is encrypted under this fixed key.
const TEST_KEY: VolumeKey = [0x5a; VOLUME_KEY_LEN];

fn fmt(block_size: u32, block_count: u64, inodes: u32) -> RustFs<MemBlock> {
    RustFs::format(MemBlock::new(block_size, block_count), inodes, &TEST_KEY)
        .expect("format a blank device")
        .with_clock(fixed_clock)
}

#[test]
fn format_then_open_round_trips() {
    let fs = fmt(512, 256, 32);
    let bytes = fs.into_block().bytes();
    let reopened = RustFs::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY).expect("reopen");
    assert_eq!(reopened.root(), NodeId::from_raw(1));
}

#[test]
fn open_rejects_an_unformatted_device() {
    let dev = MemBlock::new(512, 256);
    assert!(matches!(
        RustFs::open(dev, &TEST_KEY),
        Err(DriverError::BadMagic)
    ));
}

#[test]
fn create_write_read_back_and_survive_remount() {
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"file", NodeKind::RegularFile)
        .expect("create");
    let body = alloc::vec![0xABu8; 9000];
    assert_eq!(fs.write_at(root, b"file", 0, &body), Ok(9000));
    let node = fs.lookup(root, b"file").expect("lookup");
    assert_eq!(fs.node_info(node).expect("info").size, 9000);

    let mut back = alloc::vec![0u8; 9000];
    let mut done = 0;
    while done < 9000 {
        let n = fs
            .read_at(node, done as u64, &mut back[done..])
            .expect("read");
        if n == 0 {
            break;
        }
        done += n;
    }
    assert_eq!(back, body);

    // Survive a remount.
    let bytes = fs.into_block().bytes();
    let mut fs = RustFs::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY).expect("reopen");
    let node = fs.lookup(fs.root(), b"file").expect("lookup after remount");
    let mut back2 = alloc::vec![0u8; 9000];
    let mut done = 0;
    while done < 9000 {
        let n = fs
            .read_at(node, done as u64, &mut back2[done..])
            .expect("read");
        if n == 0 {
            break;
        }
        done += n;
    }
    assert_eq!(back2, body);
}

#[test]
fn extent_tree_backs_large_files() {
    // A file spanning many blocks exercises the per-file extent tree, which
    // replaced Stage 1's 12-direct + single-indirect map (no fixed cap).
    let mut fs = fmt(512, 512, 32);
    let root = fs.root();
    fs.create(root, b"big", NodeKind::RegularFile)
        .expect("create");
    let body = alloc::vec![0xCDu8; 512 * 200];
    assert_eq!(fs.write_at(root, b"big", 0, &body), Ok(body.len()));
    let node = fs.lookup(root, b"big").expect("lookup");
    let mut back = alloc::vec![0u8; body.len()];
    let mut done = 0;
    while done < body.len() {
        let n = fs
            .read_at(node, done as u64, &mut back[done..])
            .expect("read");
        if n == 0 {
            break;
        }
        done += n;
    }
    assert_eq!(back, body);
}

#[test]
fn nested_directories_and_listing() {
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"sub", NodeKind::Directory).expect("mkdir");
    let sub = fs.lookup(root, b"sub").expect("lookup sub");
    fs.create(sub, b"inner", NodeKind::RegularFile)
        .expect("create inner");
    let inner = fs.lookup(sub, b"inner").unwrap();
    assert_eq!(fs.node_info(inner).unwrap().size, 0);

    let mut names = alloc::vec::Vec::new();
    let mut buf = [0u8; 64];
    let mut i = 0u64;
    while let Some(e) = fs.read_dir(root, i, &mut buf).expect("read_dir") {
        names.push(buf[..e.name_len].to_vec());
        i += 1;
    }
    assert!(names.iter().any(|n| n == b"sub"));
}

#[test]
fn truncate_keeps_the_surviving_prefix() {
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"f", NodeKind::RegularFile)
        .expect("create");
    let body = alloc::vec![7u8; 4096];
    fs.write_at(root, b"f", 0, &body).expect("write");
    fs.truncate(root, b"f", 2048).expect("truncate");
    let node = fs.lookup(root, b"f").unwrap();
    assert_eq!(fs.node_info(node).unwrap().size, 2048);
    let mut back = alloc::vec![0u8; 2048];
    fs.read_at(node, 0, &mut back).expect("read");
    assert_eq!(back, alloc::vec![7u8; 2048]);
}

#[test]
fn fail_closed_extremes() {
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"dup", NodeKind::RegularFile)
        .expect("create");
    assert_eq!(
        fs.create(root, b"dup", NodeKind::RegularFile),
        Err(DriverError::Busy)
    );
    assert_eq!(
        fs.create(root, b"", NodeKind::RegularFile),
        Err(DriverError::LengthOutOfRange)
    );
    let oversize = alloc::vec![b'x'; NAME_MAX + 1];
    assert_eq!(
        fs.create(root, &oversize, NodeKind::RegularFile),
        Err(DriverError::LengthOutOfRange)
    );
    fs.create(root, b"d", NodeKind::Directory).expect("mkdir");
    let d = fs.lookup(root, b"d").unwrap();
    fs.create(d, b"child", NodeKind::RegularFile)
        .expect("child");
    assert_eq!(fs.remove(root, b"d"), Err(DriverError::Busy));
    assert_eq!(fs.remove(root, b"nope"), Err(DriverError::NotFound));
}

#[test]
fn remove_reclaims_space_so_allocation_resumes() {
    // Tiny device: fill until NoSpace, free one file, confirm a write succeeds.
    let mut fs = fmt(512, 64, 16);
    let root = fs.root();
    let body = alloc::vec![0x5Au8; 4096];
    let mut last = alloc::string::String::new();
    let mut idx = 0;
    loop {
        let name = alloc::format!("f{idx}");
        if fs
            .create(root, name.as_bytes(), NodeKind::RegularFile)
            .is_err()
        {
            break;
        }
        match fs.write_at(root, name.as_bytes(), 0, &body) {
            Ok(_) => {
                last = name;
                idx += 1;
            }
            Err(DriverError::NoSpace) => break,
            Err(e) => panic!("unexpected {e:?}"),
        }
        assert!(idx <= 10_000, "never ran out of space");
    }
    assert!(!last.is_empty(), "at least one file should have landed");
    fs.remove(root, last.as_bytes())
        .expect("remove frees space");
    fs.create(root, b"after", NodeKind::RegularFile)
        .expect("create after free");
    assert_eq!(
        fs.write_at(root, b"after", 0, &alloc::vec![1u8; 512]),
        Ok(512)
    );
}

#[test]
fn timestamps_round_trip_extreme_values() {
    fn old() -> Time64 {
        Time64::from_secs(-2_000_000_000)
    }
    fn future() -> Time64 {
        Time64::from_secs(4_200_000_000)
    }
    let mut fs = RustFs::format(MemBlock::new(512, 256), 32, &TEST_KEY)
        .expect("format")
        .with_clock(old);
    let root = fs.root();
    fs.create(root, b"t", NodeKind::RegularFile)
        .expect("create");
    let node = fs.lookup(root, b"t").unwrap();
    assert_eq!(fs.times(node).unwrap().created, old());

    // A far-future value survives a remount.
    let mut sec = Security::new(0o600, 1, 2);
    sec.required_cap = Some(CapabilityId::AUDIT_READ);
    fs = fs.with_clock(future);
    fs.set_security(node, sec).expect("set_security");
    let bytes = fs.into_block().bytes();
    let mut fs = RustFs::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY).expect("reopen");
    let node = fs.lookup(fs.root(), b"t").unwrap();
    assert_eq!(fs.security(node).unwrap(), sec);
    assert_eq!(fs.times(node).unwrap().changed, future());
}

#[test]
fn superblock_ring_selects_the_highest_committed_generation() {
    // Several commits advance the generation; the latest must be selected.
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    for i in 0..8 {
        let name = alloc::format!("g{i}");
        fs.create(root, name.as_bytes(), NodeKind::RegularFile)
            .expect("create");
    }
    let bytes = fs.into_block().bytes();
    let mut fs = RustFs::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY).expect("reopen");
    for i in 0..8 {
        let name = alloc::format!("g{i}");
        assert!(fs.lookup(fs.root(), name.as_bytes()).is_ok());
    }
}

#[test]
fn crash_at_every_write_count_during_commit_never_tears() {
    // Baseline: a formatted volume with a committed file and an empty target
    // file. The crashed trial performs exactly one transaction (a single
    // `write_at`), so the only valid post-crash outcomes are "the write
    // committed in full" or "it did not commit at all" — never a torn middle.
    let mut base = fmt(512, 256, 32);
    let root = base.root();
    base.create(root, b"keep", NodeKind::RegularFile)
        .expect("create keep");
    base.write_at(root, b"keep", 0, b"baseline")
        .expect("write keep");
    base.create(root, b"new", NodeKind::RegularFile)
        .expect("create new");
    let baseline = base.into_block().bytes();

    for budget in 0..64u32 {
        let mut dev = MemBlock::from_bytes(baseline.clone(), 512, 256);
        dev.write_budget = Some(budget);
        let mut fs = RustFs::open(dev, &TEST_KEY).expect("baseline opens");
        let root = fs.root();
        // The single transaction may be cut short at `budget` writes.
        let _ = fs.write_at(root, b"new", 0, b"freshdata");
        let bytes = fs.into_block().bytes();

        // Re-open from the (possibly torn) image: it must mount, and the
        // pre-existing files must always be intact.
        let mut fs = RustFs::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY)
            .expect("post-crash mount always succeeds");
        let root = fs.root();
        let keep = fs.lookup(root, b"keep").expect("keep always survives");
        let mut buf = [0u8; 8];
        let n = fs.read_at(keep, 0, &mut buf).expect("read keep");
        assert_eq!(&buf[..n], b"baseline");

        // "new" is always present (created in the baseline). Its contents are
        // either the committed write (9 bytes) or the pre-write empty file —
        // never a torn partial.
        let node = fs.lookup(root, b"new").expect("new always survives");
        let size = fs.node_info(node).expect("info").size;
        assert!(size == 0 || size == 9, "torn size {size}");
        if size == 9 {
            let mut nb = [0u8; 9];
            let n = fs.read_at(node, 0, &mut nb).expect("read new");
            assert_eq!(&nb[..n], b"freshdata");
        }
    }
}

// ---------------------------------------------------------------------------
// Stage 2: copy-on-write inode tree + per-file extent trees.
// ---------------------------------------------------------------------------

/// Number of nodes in the inode B-tree (its block count on disk).
fn inode_tree_nodes(fs: &mut RustFs<MemBlock>) -> usize {
    let spec = inode_spec();
    fs.btree_collect_nodes(fs.inode_tree_root, spec)
        .expect("walk inode tree")
        .len()
}

/// Number of nodes in `ino`'s per-file extent tree.
fn extent_tree_nodes(fs: &mut RustFs<MemBlock>, ino: u32) -> usize {
    let inode = fs.read_inode(ino).expect("read inode");
    let spec = extent_spec(ino);
    fs.btree_collect_nodes(inode.extent_root, spec)
        .expect("walk extent tree")
        .len()
}

#[test]
fn inode_tree_grows_and_shrinks_across_many_inodes() {
    // Far more inodes than fit one B-tree node, forcing the inode tree to
    // split into internal nodes, then deleting half to force borrow/merge.
    let mut fs = fmt(4096, 4096, 64);
    let root = fs.root();
    let count = 400u32;
    for i in 0..count {
        let name = alloc::format!("f{i:04}");
        fs.create(root, name.as_bytes(), NodeKind::RegularFile)
            .expect("create");
    }
    assert!(
        inode_tree_nodes(&mut fs) > 1,
        "inode tree should have split past a single node"
    );

    // Survive a remount: every inode is reachable and the free-space rebuild
    // matched (open would have failed otherwise).
    let bytes = fs.into_block().bytes();
    let mut fs = RustFs::open(MemBlock::from_bytes(bytes, 4096, 4096), &TEST_KEY).expect("reopen");
    let root = fs.root();
    for i in 0..count {
        let name = alloc::format!("f{i:04}");
        assert!(fs.lookup(root, name.as_bytes()).is_ok(), "missing {name}");
    }

    // Delete every other file: exercises leaf/internal borrow and merge.
    for i in (0..count).step_by(2) {
        let name = alloc::format!("f{i:04}");
        fs.remove(root, name.as_bytes()).expect("remove");
    }
    for i in 0..count {
        let name = alloc::format!("f{i:04}");
        let present = fs.lookup(root, name.as_bytes()).is_ok();
        assert_eq!(present, i % 2 == 1, "wrong presence for {name}");
    }

    // The survivors persist across another remount.
    let bytes = fs.into_block().bytes();
    let mut fs = RustFs::open(MemBlock::from_bytes(bytes, 4096, 4096), &TEST_KEY).expect("reopen2");
    let root = fs.root();
    for i in (1..count).step_by(2) {
        let name = alloc::format!("f{i:04}");
        assert!(fs.lookup(root, name.as_bytes()).is_ok(), "lost {name}");
    }
}

#[test]
fn file_with_many_noncontiguous_extents_round_trips() {
    // Writing single blocks at every other logical block leaves holes between
    // them, so the runs never merge and the per-file extent tree must hold
    // many records — enough to split past one node.
    let mut fs = fmt(512, 4096, 64);
    let root = fs.root();
    fs.create(root, b"sparse", NodeKind::RegularFile)
        .expect("create");
    // Each run fills exactly one data block, so the stride is the data-block
    // *content* capacity (the block minus its crypto trailer), not the raw
    // device block size — writing at every other logical block leaves holes.
    let cap = fs.data_capacity();
    let cap_bytes = as_usize(cap);
    let runs = 80u8;
    for i in 0..runs {
        let val = i.wrapping_add(1);
        let block = alloc::vec![val; cap_bytes];
        let off = u64::from(i) * 2 * cap;
        assert_eq!(fs.write_at(root, b"sparse", off, &block), Ok(cap_bytes));
    }
    let node = fs.lookup(root, b"sparse").expect("lookup");
    let ino = u32::try_from(node.raw()).unwrap();
    assert!(
        extent_tree_nodes(&mut fs, ino) > 1,
        "extent tree should have split past a single node"
    );

    let check = |fs: &mut RustFs<MemBlock>| {
        let node = fs.lookup(fs.root(), b"sparse").expect("lookup");
        for i in 0..runs {
            let val = i.wrapping_add(1);
            let mut got = alloc::vec![0u8; cap_bytes];
            fs.read_at(node, u64::from(i) * 2 * cap, &mut got)
                .expect("read run");
            assert_eq!(got, alloc::vec![val; cap_bytes], "run {i} wrong");
            // The block between this run and the next is a hole reading zero.
            if i + 1 < runs {
                let mut hole = alloc::vec![0xFFu8; cap_bytes];
                fs.read_at(node, u64::from(i) * 2 * cap + cap, &mut hole)
                    .expect("read hole");
                assert_eq!(hole, alloc::vec![0u8; cap_bytes], "hole {i} not zero");
            }
        }
    };
    check(&mut fs);
    let bytes = fs.into_block().bytes();
    let mut fs = RustFs::open(MemBlock::from_bytes(bytes, 512, 4096), &TEST_KEY).expect("reopen");
    check(&mut fs);
}

#[test]
fn large_contiguous_write_collapses_to_few_extents() {
    // A single sequential write lands in contiguous physical blocks, so the
    // run-merging extent map keeps it to one record, not one per block.
    let mut fs = fmt(4096, 4096, 64);
    let root = fs.root();
    fs.create(root, b"big", NodeKind::RegularFile)
        .expect("create");
    let body = alloc::vec![0x7Eu8; 4096 * 64];
    assert_eq!(fs.write_at(root, b"big", 0, &body), Ok(body.len()));
    let node = fs.lookup(root, b"big").expect("lookup");
    let ino = u32::try_from(node.raw()).unwrap();
    let inode = fs.read_inode(ino).expect("inode");
    let extents = fs
        .btree_collect_entries(inode.extent_root, extent_spec(ino))
        .expect("walk extents");
    assert_eq!(extents.len(), 1, "contiguous write should be one extent");
}

#[test]
fn free_space_rebuild_matches_authoritative_extents() {
    // Build a volume with files, a sparse file, and deletions, then assert the
    // free-block set rebuilt by walking the trees at mount is byte-for-byte the
    // set the live filesystem maintained (`docs/src/filesystem/rustfs-spec.md` §16).
    let mut fs = fmt(4096, 2048, 64);
    let root = fs.root();
    for i in 0u8..40 {
        let name = alloc::format!("d{i}");
        fs.create(root, name.as_bytes(), NodeKind::RegularFile)
            .expect("create");
        let body = alloc::vec![i; 4096 * 3];
        fs.write_at(root, name.as_bytes(), 0, &body).expect("write");
    }
    fs.create(root, b"sparse", NodeKind::RegularFile)
        .expect("create sparse");
    for i in 0..30u64 {
        fs.write_at(root, b"sparse", i * 4096 * 2, &alloc::vec![9u8; 4096])
            .expect("sparse write");
    }
    for i in (0u8..40).step_by(3) {
        let name = alloc::format!("d{i}");
        fs.remove(root, name.as_bytes()).expect("remove");
    }
    let live = fs.free.clone();

    let bytes = fs.into_block().bytes();
    let rebuilt = RustFs::open(MemBlock::from_bytes(bytes, 4096, 2048), &TEST_KEY).expect("reopen");
    assert_eq!(
        rebuilt.free, live,
        "mount-time free-space rebuild must equal the authoritative live set"
    );
}

// ---------------------------------------------------------------------------
// Stage 3: keyed metadata authenticator + duplicated critical metadata.
// ---------------------------------------------------------------------------

#[test]
fn metadata_bit_flip_is_detected_and_repaired_from_the_companion() {
    // Corrupt the *primary* copy of a live metadata block (the inode-tree
    // root). On remount the read must fail the keyed authenticator on the
    // primary, fall back to the intact companion mirror, serve the data, and
    // repair the primary on disk (`docs/src/filesystem/rustfs-spec.md` §8).
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"f", NodeKind::RegularFile)
        .expect("create");
    fs.write_at(root, b"f", 0, b"hello").expect("write");
    let target = fs.inode_tree_root;
    assert_ne!(target, 0, "the volume has an inode tree");

    let bs = 512usize;
    let mut bytes = fs.into_block().bytes();
    let off = as_usize(target) * bs + HEADER_LEN; // first payload byte
    let original = bytes[off];
    bytes[off] ^= 0xff; // wound only the primary copy

    let mut fs = RustFs::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY)
        .expect("mounts by falling back to the companion mirror");
    let node = fs.lookup(fs.root(), b"f").expect("file survives");
    let mut buf = [0u8; 5];
    let n = fs.read_at(node, 0, &mut buf).expect("read");
    assert_eq!(&buf[..n], b"hello");

    // The primary copy was repaired in place from the good companion.
    let healed = fs.into_block().bytes();
    let p = as_usize(target) * bs;
    let c = as_usize(target + 1) * bs;
    assert_eq!(
        healed[p..p + bs],
        healed[c..c + bs],
        "primary repaired to match its companion mirror"
    );
    assert_eq!(healed[off], original, "the corrupted byte is restored");
}

#[test]
fn both_metadata_copies_corrupted_fails_closed() {
    // Wound *both* physical copies of the inode-tree root. Neither
    // authenticates, so the mount fails closed with an error — never a panic
    // and never trusting the corrupt bytes (`AGENTS.md` §5.4 / §2.9).
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"f", NodeKind::RegularFile)
        .expect("create");
    fs.write_at(root, b"f", 0, b"hello").expect("write");
    let target = fs.inode_tree_root;
    assert_ne!(target, 0);

    let bs = 512usize;
    let mut bytes = fs.into_block().bytes();
    bytes[as_usize(target) * bs + HEADER_LEN] ^= 0xff;
    bytes[as_usize(target + 1) * bs + HEADER_LEN] ^= 0xff;

    assert!(
        RustFs::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY).is_err(),
        "both copies corrupt must fail closed, not panic or trust corruption"
    );
}

#[test]
fn corrupting_one_superblock_copy_still_mounts_via_the_mirror() {
    // The superblock ring is mirrored too: wounding the primary block of the
    // committed slot must not lose the volume — `open` falls back to the
    // companion and repairs it.
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"keep", NodeKind::RegularFile)
        .expect("create");
    let bs = 512usize;
    let mut bytes = fs.into_block().bytes();
    // Every ring primary lives at an even block in `0..RING_BLOCKS`; corrupt
    // the keyed tag of every primary slot, leaving each companion intact.
    for slot in 0..superblock::RING_SLOTS {
        let primary = superblock::slot_block(slot);
        bytes[as_usize(primary) * bs + 80] ^= 0xff; // inside the tag slot
    }
    let mut fs = RustFs::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY)
        .expect("mounts from the superblock mirrors");
    assert!(fs.lookup(fs.root(), b"keep").is_ok());
}

#[test]
fn crash_during_multiblock_extent_write_never_tears() {
    // A larger transaction (a 24-block write that grows the extent tree) is
    // faulted after every write count; the file is always either fully the new
    // contents or fully the old, never a torn mix.
    let mut base = fmt(512, 512, 32);
    let root = base.root();
    base.create(root, b"f", NodeKind::RegularFile)
        .expect("create");
    let old = alloc::vec![0xAAu8; 512 * 24];
    base.write_at(root, b"f", 0, &old).expect("seed write");
    let baseline = base.into_block().bytes();
    let new = alloc::vec![0x55u8; 512 * 24];

    for budget in 0..160u32 {
        let mut dev = MemBlock::from_bytes(baseline.clone(), 512, 512);
        dev.write_budget = Some(budget);
        let mut fs = RustFs::open(dev, &TEST_KEY).expect("baseline opens");
        let root = fs.root();
        let _ = fs.write_at(root, b"f", 0, &new);
        let bytes = fs.into_block().bytes();

        let mut fs = RustFs::open(MemBlock::from_bytes(bytes, 512, 512), &TEST_KEY)
            .expect("post-crash mount always succeeds");
        let node = fs.lookup(fs.root(), b"f").expect("file survives");
        let mut got = alloc::vec![0u8; 512 * 24];
        let mut done = 0usize;
        while done < got.len() {
            let off = u64::try_from(done).unwrap();
            let n = fs.read_at(node, off, &mut got[done..]).expect("read");
            if n == 0 {
                break;
            }
            done += n;
        }
        assert!(
            got == old || got == new,
            "torn multi-block contents at budget {budget}"
        );
    }
}

// ---------------------------------------------------------------------------
// Stage 4: per-volume key hierarchy + filename/data encryption.
// ---------------------------------------------------------------------------

/// Whether `haystack` contains the byte run `needle` anywhere.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn wrong_volume_key_refuses_the_mount() {
    // A volume formatted under one key never mounts under another: the wrapped
    // master key fails to authenticate, so `open` fails closed with
    // `PermissionDenied` (`AGENTS.md` §5.4) — never a panic, never a misread.
    let mut fs = fmt(512, 256, 32);
    fs.create(fs.root(), b"f", NodeKind::RegularFile)
        .expect("create");
    let bytes = fs.into_block().bytes();

    let mut wrong = TEST_KEY;
    wrong[0] ^= 0x01;
    assert!(matches!(
        RustFs::open(MemBlock::from_bytes(bytes.clone(), 512, 256), &wrong),
        Err(DriverError::PermissionDenied)
    ));
    // The correct key still mounts the very same image.
    RustFs::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY)
        .expect("the right key still mounts");
}

#[test]
fn no_plaintext_filename_or_data_at_rest() {
    // RustFS has no plaintext mode: a distinctive filename and file content
    // must be absent from the raw on-disk bytes (encrypted at rest).
    let name: &[u8] = b"ZxQvBnMkLpSecret";
    let payload = {
        let mut v = alloc::vec::Vec::new();
        for _ in 0..200 {
            v.extend_from_slice(b"PLAINTEXT-MARKER");
        }
        v
    };
    let mut fs = fmt(512, 512, 32);
    let root = fs.root();
    fs.create(root, name, NodeKind::RegularFile)
        .expect("create");
    assert_eq!(fs.write_at(root, name, 0, &payload), Ok(payload.len()));
    let bytes = fs.into_block().bytes();

    assert!(
        !contains(&bytes, name),
        "the filename must not appear in cleartext on disk"
    );
    assert!(
        !contains(&bytes, b"PLAINTEXT-MARKER"),
        "the file content must not appear in cleartext on disk"
    );
}

#[test]
fn filename_and_data_round_trip_through_encryption_across_remount() {
    // The encrypted name and a multi-block encrypted payload decrypt back to
    // exactly what was written, across a remount.
    let name: &[u8] = b"document.txt";
    let payload = alloc::vec![0xC3u8; 512 * 5 + 17];
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, name, NodeKind::RegularFile)
        .expect("create");
    assert_eq!(fs.write_at(root, name, 0, &payload), Ok(payload.len()));
    let bytes = fs.into_block().bytes();

    let mut fs = RustFs::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY).expect("remount");
    // The name decrypts: lookup by the cleartext name succeeds.
    let node = fs.lookup(fs.root(), name).expect("encrypted name decrypts");
    let mut back = alloc::vec![0u8; payload.len()];
    let mut done = 0usize;
    while done < payload.len() {
        let off = u64::try_from(done).unwrap();
        let n = fs.read_at(node, off, &mut back[done..]).expect("read");
        if n == 0 {
            break;
        }
        done += n;
    }
    assert_eq!(back, payload, "encrypted data round-trips across a remount");
}

#[test]
fn bit_flip_in_encrypted_data_is_detected() {
    // Flipping a byte of an encrypted data block's ciphertext must be caught by
    // the AEAD authenticator on read — a failed decrypt fails closed
    // (`AGENTS.md` §5.4), never returning mis-decrypted bytes.
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"f", NodeKind::RegularFile)
        .expect("create");
    let payload = alloc::vec![0x77u8; 300];
    assert_eq!(fs.write_at(root, b"f", 0, &payload), Ok(payload.len()));
    let node = fs.lookup(root, b"f").expect("lookup");
    let ino = u32::try_from(node.raw()).unwrap();
    let inode = fs.read_inode(ino).expect("read inode");
    let phys = fs.block_ptr(&inode, 0).expect("data block pointer");
    assert_ne!(phys, 0, "the file has a data block");

    let bs = 512usize;
    let mut bytes = fs.into_block().bytes();
    bytes[as_usize(phys) * bs] ^= 0xff; // wound the ciphertext

    let mut fs = RustFs::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY).expect("remount");
    let node = fs.lookup(fs.root(), b"f").expect("file survives");
    let mut buf = [0u8; 300];
    assert!(
        fs.read_at(node, 0, &mut buf).is_err(),
        "a bit-flip in encrypted data must be detected, not mis-decrypted"
    );
}

// ---------------------------------------------------------------------------
// Stage 5: per-data-record physical checksum + logical content hash.
// ---------------------------------------------------------------------------

/// The physical address of file `name`'s logical block `bi` on `fs`.
fn data_block_phys(fs: &mut RustFs<MemBlock>, name: &[u8], bi: u64) -> u64 {
    let node = fs.lookup(fs.root(), name).expect("lookup");
    let ino = u32::try_from(node.raw()).unwrap();
    let inode = fs.read_inode(ino).expect("read inode");
    let phys = fs.block_ptr(&inode, bi).expect("block pointer");
    assert_ne!(phys, 0, "the file has a mapped data block");
    phys
}

/// Read the whole of file `node` into a fresh vector.
fn read_all(fs: &mut RustFs<MemBlock>, node: NodeId, len: usize) -> alloc::vec::Vec<u8> {
    let mut out = alloc::vec![0u8; len];
    let mut done = 0usize;
    while done < len {
        let off = u64::try_from(done).unwrap();
        let n = fs.read_at(node, off, &mut out[done..]).expect("read");
        if n == 0 {
            break;
        }
        done += n;
    }
    out
}

#[test]
fn data_block_integrity_layers_are_distinct_and_fail_closed() {
    // Each of the two integrity layers — the fast physical checksum and the
    // logical content hash — plus the Stage-4 AEAD detects its own class of
    // corruption, and all three fail closed (`AGENTS.md` §5.4 / §2.9). The test
    // isolates each layer by repairing the checks that sit in front of it.
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"f", NodeKind::RegularFile)
        .expect("create");
    let payload = alloc::vec![0x77u8; 300];
    assert_eq!(fs.write_at(root, b"f", 0, &payload), Ok(payload.len()));
    let phys = data_block_phys(&mut fs, b"f", 0);
    let csum_off = fs.phys_checksum_offset();
    let hash_off = fs.logical_hash_offset();
    let bs = 512usize;
    let base = as_usize(phys) * bs;
    let baseline = fs.into_block().bytes();

    let reopen = |bytes: alloc::vec::Vec<u8>| {
        RustFs::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY).expect("remount")
    };
    // Recompute the physical checksum over a block's at-rest bytes so a
    // corruption can be slipped past the fast check to reach a deeper layer.
    let repair_checksum = |bytes: &mut alloc::vec::Vec<u8>| {
        let fixed = physical_checksum(&bytes[base..base + csum_off]);
        bytes[base + csum_off..base + csum_off + PHYS_CHECKSUM_LEN].copy_from_slice(&fixed);
    };

    // 1. A flipped ciphertext byte is caught by the fast physical checksum
    //    (it covers the at-rest ciphertext), before the AEAD even runs.
    {
        let mut bytes = baseline.clone();
        bytes[base] ^= 0x01;
        let mut fs = reopen(bytes);
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        assert_eq!(
            fs.read_data_block_classified(phys, &mut buf),
            Err(DataFault::Physical)
        );
    }
    // 2. The same flip, but with the physical checksum repaired, slips past the
    //    fast check and is caught by the AEAD tag instead.
    {
        let mut bytes = baseline.clone();
        bytes[base] ^= 0x01;
        repair_checksum(&mut bytes);
        let mut fs = reopen(bytes);
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        assert_eq!(
            fs.read_data_block_classified(phys, &mut buf),
            Err(DataFault::Aead)
        );
    }
    // 3. Corrupting the stored logical hash (with the checksum repaired) leaves
    //    the ciphertext intact, so the AEAD passes but the recomputed plaintext
    //    hash mismatches — the logical layer catches it.
    {
        let mut bytes = baseline.clone();
        bytes[base + hash_off] ^= 0x01;
        repair_checksum(&mut bytes);
        let mut fs = reopen(bytes);
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        assert_eq!(
            fs.read_data_block_classified(phys, &mut buf),
            Err(DataFault::Logical)
        );
    }
    // The production read path surfaces every layer as a fail-closed
    // `DeviceFault`, never a panic and never a misread.
    {
        let mut bytes = baseline.clone();
        bytes[base] ^= 0x01;
        let mut fs = reopen(bytes);
        let node = fs.lookup(fs.root(), b"f").expect("file survives");
        let mut buf = [0u8; 300];
        assert!(matches!(
            fs.read_at(node, 0, &mut buf),
            Err(DriverError::DeviceFault)
        ));
    }
}

#[test]
fn identical_content_shares_a_logical_hash_distinct_content_differs() {
    // The logical content hash names the plaintext: two blocks with identical
    // content carry the same stored hash (the seam Stage 7 dedupe keys on),
    // while a single differing byte changes it. The hash is over plaintext, so
    // it matches even though the two blocks encrypt to different ciphertext
    // (distinct `(phys, generation)` nonces).
    let mut fs = fmt(512, 512, 32);
    let root = fs.root();
    let full = as_usize(fs.data_capacity());
    let block = alloc::vec![0xABu8; full];
    let mut other = block.clone();
    other[0] ^= 0x01;
    for name in [b"a".as_slice(), b"b", b"c"] {
        fs.create(root, name, NodeKind::RegularFile)
            .expect("create");
    }
    assert_eq!(fs.write_at(root, b"a", 0, &block), Ok(full));
    assert_eq!(fs.write_at(root, b"b", 0, &block), Ok(full));
    assert_eq!(fs.write_at(root, b"c", 0, &other), Ok(full));

    let stored_hash = |fs: &mut RustFs<MemBlock>, name: &[u8]| -> [u8; LOGICAL_HASH_LEN] {
        let phys = data_block_phys(fs, name, 0);
        let mut raw = [0u8; MAX_BLOCK_SIZE];
        fs.read_block(phys, &mut raw).expect("raw read");
        let off = fs.logical_hash_offset();
        let mut hash = [0u8; LOGICAL_HASH_LEN];
        hash.copy_from_slice(&raw[off..off + LOGICAL_HASH_LEN]);
        hash
    };
    let ha = stored_hash(&mut fs, b"a");
    let hb = stored_hash(&mut fs, b"b");
    let hc = stored_hash(&mut fs, b"c");
    assert_eq!(ha, hb, "identical plaintext shares one logical hash");
    assert_ne!(ha, hc, "different plaintext hashes differently");
    // The hash genuinely depends on content, not a zeroed placeholder.
    assert_ne!(ha, [0u8; LOGICAL_HASH_LEN]);
    // Confirm the two same-content blocks really are stored as distinct
    // ciphertext, so the equal hash is a property of the plaintext.
    let pa = data_block_phys(&mut fs, b"a", 0);
    let pb = data_block_phys(&mut fs, b"b", 0);
    let bs = 512usize;
    let bytes = fs.into_block().bytes();
    let ca = &bytes[as_usize(pa) * bs..as_usize(pa) * bs + full];
    let cb = &bytes[as_usize(pb) * bs..as_usize(pb) * bs + full];
    assert_ne!(ca, cb, "same plaintext encrypts to different ciphertext");
}

#[test]
fn integrity_survives_remount_and_a_cow_rewrite() {
    // A multi-block file's data integrity verifies after a remount, and again
    // after a copy-on-write overwrite of a middle region writes fresh blocks
    // with freshly sealed integrity trailers.
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"f", NodeKind::RegularFile)
        .expect("create");
    let mut expected = alloc::vec![0x33u8; 1500];
    assert_eq!(fs.write_at(root, b"f", 0, &expected), Ok(expected.len()));
    let bytes = fs.into_block().bytes();

    let mut fs = RustFs::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY).expect("remount");
    let node = fs.lookup(fs.root(), b"f").expect("file survives");
    assert_eq!(read_all(&mut fs, node, expected.len()), expected);

    let patch = alloc::vec![0x99u8; 600];
    assert_eq!(fs.write_at(fs.root(), b"f", 100, &patch), Ok(patch.len()));
    expected[100..700].copy_from_slice(&patch);
    let bytes = fs.into_block().bytes();

    let mut fs = RustFs::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY).expect("remount2");
    let node = fs.lookup(fs.root(), b"f").expect("file still there");
    assert_eq!(
        read_all(&mut fs, node, expected.len()),
        expected,
        "integrity verifies after a COW rewrite"
    );
}

#[test]
fn data_block_capacity_reserves_the_integrity_trailer() {
    // A file-data block stores the device block minus both the crypto trailer
    // and the data-integrity trailer (logical hash + physical checksum).
    let fs = fmt(512, 256, 32);
    assert_eq!(
        fs.data_capacity(),
        (512 - CRYPTO_TRAILER - DATA_INTEGRITY_TRAILER) as u64
    );
}
