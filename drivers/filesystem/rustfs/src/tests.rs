//! Unit tests for the copy-on-write rustfs driver (`AGENTS.md` §7, §16).
//!
//! They exercise the Stage-1 foundation — format → open round-trip, the
//! superblock-ring selection, and crash replay at every write count during a
//! commit — plus the full read/write surface the VFS consumes, all over an
//! in-memory [`MemBlock`] double.

use super::*;
use rustos_abi::driver::block::{DeviceHealth, DiscardCapability, HealthSnapshot};
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
    discard: Option<DiscardCapability>,
    discarded: alloc::vec::Vec<(u64, u64)>,
    health: DeviceHealth,
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
            discard: None,
            discarded: alloc::vec::Vec::new(),
            health: DeviceHealth::Unavailable,
        }
    }

    fn from_bytes(bytes: alloc::vec::Vec<u8>, block_size: u32, block_count: u64) -> Self {
        Self {
            store: bytes,
            block_size,
            block_count,
            writes: 0,
            write_budget: None,
            discard: None,
            discarded: alloc::vec::Vec::new(),
            health: DeviceHealth::Unavailable,
        }
    }

    /// Set the health telemetry this device reports, so the health path can
    /// be exercised with a known snapshot.
    fn with_health(mut self, health: DeviceHealth) -> Self {
        self.health = health;
        self
    }

    fn bytes(&self) -> alloc::vec::Vec<u8> {
        self.store.clone()
    }

    /// Enable discard support with the given granularity and per-request cap
    /// (`0` means unlimited), so the trim path can be exercised.
    fn with_discard(mut self, granularity_blocks: u64, max_blocks_per_request: u64) -> Self {
        self.discard = Some(DiscardCapability {
            supported: true,
            granularity_blocks,
            max_blocks_per_request,
        });
        self
    }

    /// Model the backing device being enlarged underneath a live mount (an
    /// admin growing the partition / logical volume / virtual disk): extend
    /// the store with fresh zeroed blocks and report the larger count. Used to
    /// exercise online [`RustFs::grow`].
    fn enlarge_to(&mut self, new_block_count: u64) {
        assert!(new_block_count >= self.block_count, "enlarge cannot shrink");
        self.store
            .resize(self.block_size as usize * as_usize(new_block_count), 0);
        self.block_count = new_block_count;
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

    fn discard_capability(&self) -> Result<DiscardCapability, DriverError> {
        Ok(self.discard.unwrap_or_else(DiscardCapability::unsupported))
    }

    fn device_health(&self) -> Result<DeviceHealth, DriverError> {
        Ok(self.health)
    }

    fn discard(&mut self, lba: u64, blocks: u64) -> Result<(), DriverError> {
        let cap = self.discard.ok_or(DriverError::Unsupported)?;
        let gran = cap.granularity_blocks.max(1);
        assert_eq!(lba % gran, 0, "discard lba {lba} not aligned to {gran}");
        assert_eq!(
            blocks % gran,
            0,
            "discard len {blocks} not aligned to {gran}"
        );
        assert_ne!(blocks, 0, "discard of zero blocks");
        if cap.max_blocks_per_request != 0 {
            assert!(
                blocks <= cap.max_blocks_per_request,
                "discard len {blocks} exceeds per-request cap {}",
                cap.max_blocks_per_request
            );
        }
        if as_usize(lba) + as_usize(blocks) > as_usize(self.block_count) {
            return Err(DriverError::LengthOutOfRange);
        }
        self.discarded.push((lba, blocks));
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

/// A deterministic stand-in for the platform RNG seam (`EntropySource`): a
/// byte counter so each `format` draws distinct, reproducible "random" key
/// material and UUID. Test scaffolding only, never a production source.
struct TestEntropy {
    next: u8,
}

impl TestEntropy {
    fn new() -> Self {
        Self { next: 1 }
    }
}

impl EntropySource for TestEntropy {
    fn fill(&mut self, out: &mut [u8]) -> Result<(), DriverError> {
        for byte in out.iter_mut() {
            *byte = self.next;
            self.next = self.next.wrapping_add(1);
        }
        Ok(())
    }
}

fn fmt(block_size: u32, block_count: u64, inodes: u32) -> RustFs<MemBlock> {
    RustFs::format(
        MemBlock::new(block_size, block_count),
        inodes,
        &TEST_KEY,
        &mut TestEntropy::new(),
    )
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
    let mut fs = RustFs::format(
        MemBlock::new(512, 256),
        32,
        &TEST_KEY,
        &mut TestEntropy::new(),
    )
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
    // while a single differing byte changes it. Stage 7 acts on that hash:
    // identical content is stored once and shared (refcount 2), distinct
    // content is not.
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
    // The two same-content files share one physical chunk (refcount 2), while
    // the differing file is stored separately and keeps the implicit single
    // reference (Stage 7 dedupe).
    let pa = data_block_phys(&mut fs, b"a", 0);
    let pb = data_block_phys(&mut fs, b"b", 0);
    let pc = data_block_phys(&mut fs, b"c", 0);
    assert_eq!(pa, pb, "identical content shares one physical chunk");
    assert_ne!(pa, pc, "different content is not shared");
    assert_eq!(
        fs.data_refcount(pa).expect("refcount"),
        2,
        "the shared chunk is referenced twice"
    );
    assert_eq!(
        fs.data_refcount(pc).expect("refcount"),
        1,
        "the unique block keeps the implicit single reference"
    );
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
    // A file-data block stores the device block minus the crypto trailer, the
    // compression descriptor, and the data-integrity trailer (logical hash +
    // physical checksum).
    let fs = fmt(512, 256, 32);
    assert_eq!(
        fs.data_capacity(),
        (512 - CRYPTO_TRAILER - COMPRESSION_DESCRIPTOR_LEN - DATA_INTEGRITY_TRAILER) as u64
    );
}

// ---------------------------------------------------------------------------
// Stage 6: first-party compression on the §6 data-record pipeline.
// ---------------------------------------------------------------------------

/// Read the on-disk compression descriptor of file `name`'s logical block
/// `bi` (the §8 data-record compression-state field).
fn stored_compression(fs: &mut RustFs<MemBlock>, name: &[u8], bi: u64) -> Compression {
    let phys = data_block_phys(fs, name, bi);
    let mut raw = [0u8; MAX_BLOCK_SIZE];
    fs.read_block(phys, &mut raw).expect("raw read");
    let off = fs.compression_desc_offset();
    read_compression(&raw[off..off + COMPRESSION_DESCRIPTOR_LEN]).expect("descriptor parses")
}

/// A pseudo-random, incompressible buffer of `len` bytes.
fn incompressible(len: usize) -> alloc::vec::Vec<u8> {
    let mut out = alloc::vec::Vec::with_capacity(len);
    let mut state: u32 = 0x1234_5678;
    for _ in 0..len {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        out.push(u8::try_from(state >> 24).unwrap_or(0));
    }
    out
}

#[test]
fn incompressible_record_is_stored_raw_and_round_trips() {
    // Pseudo-random data does not compress, so the §10 adaptive choice stores
    // it raw — yet it must still read back byte-identically.
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"r", NodeKind::RegularFile)
        .expect("create");
    let cap = as_usize(fs.data_capacity());
    let payload = incompressible(cap);
    assert_eq!(fs.write_at(root, b"r", 0, &payload), Ok(payload.len()));

    let desc = stored_compression(&mut fs, b"r", 0);
    assert!(!desc.compressed, "incompressible data is stored raw");
    assert_eq!(
        as_usize(u64::from(desc.stored_len)),
        cap,
        "a raw record occupies the whole content slot"
    );

    let node = fs.lookup(fs.root(), b"r").expect("file survives");
    assert_eq!(read_all(&mut fs, node, payload.len()), payload);
}

#[test]
fn compressible_record_shrinks_at_rest_and_round_trips_across_remount_and_cow() {
    // A compressible block stores fewer at-rest bytes (the §10 win), and reads
    // back byte-identical across a remount and a copy-on-write rewrite, with
    // the logical hash (Stage 7 dedupe seam) unchanged by compression.
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"c", NodeKind::RegularFile)
        .expect("create");
    let cap = as_usize(fs.data_capacity());
    // Three full blocks of a short repeating pattern: highly compressible.
    let mut payload = alloc::vec::Vec::new();
    while payload.len() < cap * 3 {
        payload.extend_from_slice(b"RustOS rustfs ");
    }
    payload.truncate(cap * 3);
    assert_eq!(fs.write_at(root, b"c", 0, &payload), Ok(payload.len()));

    let desc = stored_compression(&mut fs, b"c", 0);
    assert!(desc.compressed, "a repetitive block compresses");
    assert!(
        as_usize(u64::from(desc.stored_len)) < cap,
        "a compressed record shrinks its at-rest footprint: {} >= {cap}",
        desc.stored_len
    );

    let bytes = fs.into_block().bytes();
    let mut fs = RustFs::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY).expect("remount");
    let node = fs.lookup(fs.root(), b"c").expect("file survives");
    assert_eq!(
        read_all(&mut fs, node, payload.len()),
        payload,
        "compressed data reads back byte-identical after a remount"
    );

    // A copy-on-write rewrite of a middle region re-compresses fresh blocks.
    let patch = alloc::vec![0x5Au8; cap];
    let at = u64::try_from(cap).unwrap();
    assert_eq!(fs.write_at(fs.root(), b"c", at, &patch), Ok(patch.len()));
    let mut expected = payload.clone();
    expected[cap..cap * 2].copy_from_slice(&patch);
    let bytes = fs.into_block().bytes();

    let mut fs = RustFs::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY).expect("remount2");
    let node = fs.lookup(fs.root(), b"c").expect("file still there");
    assert_eq!(
        read_all(&mut fs, node, expected.len()),
        expected,
        "compressed data verifies after a COW rewrite"
    );
}

#[test]
fn integrity_still_detected_on_a_compressed_block() {
    // The Stage-5 integrity layers still guard a compressed record: a physical
    // (media) corruption and a logical-hash mismatch are both caught and fail
    // closed (`AGENTS.md` §5.4 / §2.9).
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"c", NodeKind::RegularFile)
        .expect("create");
    let cap = as_usize(fs.data_capacity());
    // A *non-zero* constant block: it compresses through the normal zstd path
    // and produces a physical record. An all-zero block is not used here
    // because sparse handling stores it as a metadata-only hole, never a
    // compressed data record (`.junie/SPARSE.md` §4, §9).
    let payload = alloc::vec![0xFFu8; cap];
    assert_eq!(fs.write_at(root, b"c", 0, &payload), Ok(payload.len()));
    assert!(
        stored_compression(&mut fs, b"c", 0).compressed,
        "a constant block compresses"
    );

    let phys = data_block_phys(&mut fs, b"c", 0);
    let csum_off = fs.phys_checksum_offset();
    let hash_off = fs.logical_hash_offset();
    let bs = 512usize;
    let base = as_usize(phys) * bs;
    let baseline = fs.into_block().bytes();

    let reopen = |bytes: alloc::vec::Vec<u8>| {
        RustFs::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY).expect("remount")
    };

    // Physical: a flipped at-rest byte is caught by the fast checksum.
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
    // Logical: corrupt the stored plaintext hash and repair the checksum, so
    // the AEAD passes but the post-decompression hash mismatches.
    {
        let mut bytes = baseline.clone();
        bytes[base + hash_off] ^= 0x01;
        let fixed = physical_checksum(&bytes[base..base + csum_off]);
        bytes[base + csum_off..base + csum_off + PHYS_CHECKSUM_LEN].copy_from_slice(&fixed);
        let mut fs = reopen(bytes);
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        assert_eq!(
            fs.read_data_block_classified(phys, &mut buf),
            Err(DataFault::Logical)
        );
    }
}

// ---------------------------------------------------------------------------
// Stage 7: chunk table, refcounts, reverse refs, reflinks, dedupe index.
// ---------------------------------------------------------------------------

/// The inode number behind file `name` under the root.
fn file_ino(fs: &mut RustFs<MemBlock>, name: &[u8]) -> u32 {
    let node = fs.lookup(fs.root(), name).expect("lookup");
    u32::try_from(node.raw()).unwrap()
}

/// The number of records in the chunk/refcount tree (one per shared chunk).
fn chunk_count(fs: &mut RustFs<MemBlock>) -> usize {
    fs.btree_collect_entries(fs.chunk_tree_root, chunk_spec())
        .expect("walk chunk tree")
        .len()
}

#[test]
fn byte_verify_before_share_refuses_to_merge_unequal_data() {
    // A dedupe-index entry is only ever a *hint*: before sharing, the
    // candidate's bytes are compared to the incoming record, so two blocks
    // whose index keys collide but whose bytes differ are never merged (§9 —
    // merging unequal data is corruption). A natural logical-hash collision is
    // infeasible, so the colliding index entry is injected through the
    // in-memory index seam.
    let mut fs = fmt(4096, 256, 64);
    let root = fs.root();
    let cap = as_usize(fs.data_capacity());
    fs.create(root, b"a", NodeKind::RegularFile)
        .expect("create a");
    fs.create(root, b"b", NodeKind::RegularFile)
        .expect("create b");

    let content_a = alloc::vec![0x11u8; cap];
    let content_b = alloc::vec![0x22u8; cap];
    assert_eq!(fs.write_at(root, b"b", 0, &content_b), Ok(cap));
    let pb = data_block_phys(&mut fs, b"b", 0);
    let b_ino = file_ino(&mut fs, b"b");

    // Forge an index entry that maps content A's key to content B's block,
    // simulating a hash collision. The pre-share byte check must reject it.
    let mut block_a = alloc::vec![0u8; cap];
    block_a.copy_from_slice(&content_a);
    let hash_a = logical_hash(&block_a);
    let key = dedupe_key(fs.dedupe_domain, u32::try_from(cap).unwrap(), &hash_a);
    fs.dedupe_index.insert(
        key,
        DedupeCandidate {
            phys: pb,
            inode: b_ino,
            logical: 0,
        },
    );

    assert_eq!(fs.write_at(root, b"a", 0, &content_a), Ok(cap));
    let pa = data_block_phys(&mut fs, b"a", 0);
    assert_ne!(
        pa, pb,
        "unequal data is never merged despite a colliding key"
    );

    let node_a = fs.lookup(root, b"a").expect("lookup a");
    assert_eq!(read_all(&mut fs, node_a, cap), content_a);
    let node_b = fs.lookup(root, b"b").expect("lookup b");
    assert_eq!(read_all(&mut fs, node_b, cap), content_b);
}

#[test]
fn overwriting_one_sharer_copies_on_write_and_leaves_the_other() {
    // Two files with identical content share one immutable chunk (refcount 2).
    // Overwriting one copies-on-write a fresh chunk for the writer and drops
    // the shared chunk's refcount, leaving the other sharer's data intact (§9).
    let mut fs = fmt(4096, 256, 64);
    let root = fs.root();
    let cap = as_usize(fs.data_capacity());
    fs.create(root, b"a", NodeKind::RegularFile)
        .expect("create a");
    fs.create(root, b"b", NodeKind::RegularFile)
        .expect("create b");
    let original = alloc::vec![0x5Au8; cap];
    assert_eq!(fs.write_at(root, b"a", 0, &original), Ok(cap));
    assert_eq!(fs.write_at(root, b"b", 0, &original), Ok(cap));

    let shared = data_block_phys(&mut fs, b"a", 0);
    assert_eq!(data_block_phys(&mut fs, b"b", 0), shared, "they share");
    assert_eq!(fs.data_refcount(shared).expect("refcount"), 2);

    // Overwrite "a"; it must copy-on-write off the shared chunk.
    let replacement = alloc::vec![0xA5u8; cap];
    assert_eq!(fs.write_at(root, b"a", 0, &replacement), Ok(cap));
    let pa = data_block_phys(&mut fs, b"a", 0);
    assert_ne!(pa, shared, "the writer copied on write");
    assert_eq!(
        fs.data_refcount(shared).expect("refcount"),
        1,
        "the surviving sharer holds the chunk's implicit single reference"
    );

    let node_a = fs.lookup(root, b"a").expect("lookup a");
    assert_eq!(read_all(&mut fs, node_a, cap), replacement);
    let node_b = fs.lookup(root, b"b").expect("lookup b");
    assert_eq!(
        read_all(&mut fs, node_b, cap),
        original,
        "other side intact"
    );
}

#[test]
fn reflink_shares_chunks_until_one_side_is_written() {
    // A reflink is a copy-on-write clone: it shares every data block with the
    // source (refcount 2) until a side is written, when only the written
    // blocks diverge (§9).
    let mut fs = fmt(4096, 256, 64);
    let root = fs.root();
    let cap = as_usize(fs.data_capacity());
    fs.create(root, b"src", NodeKind::RegularFile)
        .expect("create src");
    let body = read_all_pattern(cap * 3);
    assert_eq!(fs.write_at(root, b"src", 0, &body), Ok(body.len()));

    let dst = fs.reflink(root, b"src", b"dst").expect("reflink");
    assert_eq!(fs.node_info(dst).expect("info").size, body.len() as u64);
    let dst_node = fs.lookup(root, b"dst").expect("lookup dst");
    assert_eq!(
        read_all(&mut fs, dst_node, body.len()),
        body,
        "clone matches"
    );
    for bi in 0..3u64 {
        let ps = data_block_phys(&mut fs, b"src", bi);
        let pd = data_block_phys(&mut fs, b"dst", bi);
        assert_eq!(ps, pd, "reflink shares block {bi}");
        assert_eq!(fs.data_refcount(ps).expect("refcount"), 2);
    }

    // Writing the middle block of the clone diverges only that block.
    let patch = alloc::vec![0x7Eu8; cap];
    let at = u64::try_from(cap).unwrap();
    assert_eq!(fs.write_at(root, b"dst", at, &patch), Ok(cap));
    assert_ne!(
        data_block_phys(&mut fs, b"dst", 1),
        data_block_phys(&mut fs, b"src", 1),
        "the written block diverged"
    );
    assert_eq!(
        data_block_phys(&mut fs, b"dst", 0),
        data_block_phys(&mut fs, b"src", 0),
        "untouched blocks still share"
    );
    let src_node = fs.lookup(root, b"src").expect("lookup src");
    assert_eq!(read_all(&mut fs, src_node, body.len()), body, "src intact");
    let mut expected = body.clone();
    expected[cap..cap * 2].copy_from_slice(&patch);
    assert_eq!(read_all(&mut fs, dst_node, body.len()), expected);
}

#[test]
fn refcount_to_zero_frees_the_chunk_and_the_rebuild_agrees() {
    // Sharing creates one chunk record (refcount 2). Removing one sharer drops
    // it to the implicit single reference (the record disappears); removing the
    // last frees the physical block. A remount's free-space rebuild agrees:
    // the freed space is reusable and the chunk tree is empty (§9, §4).
    let mut fs = fmt(4096, 256, 64);
    let root = fs.root();
    let cap = as_usize(fs.data_capacity());
    fs.create(root, b"a", NodeKind::RegularFile)
        .expect("create a");
    fs.create(root, b"b", NodeKind::RegularFile)
        .expect("create b");
    let body = alloc::vec![0x42u8; cap];
    assert_eq!(fs.write_at(root, b"a", 0, &body), Ok(cap));
    assert_eq!(fs.write_at(root, b"b", 0, &body), Ok(cap));
    let shared = data_block_phys(&mut fs, b"a", 0);
    assert_eq!(chunk_count(&mut fs), 1, "one shared chunk recorded");
    assert_eq!(fs.data_refcount(shared).expect("refcount"), 2);

    fs.remove(root, b"a").expect("remove a");
    assert_eq!(chunk_count(&mut fs), 0, "back to one implicit reference");
    assert_eq!(fs.data_refcount(shared).expect("refcount"), 1);
    // "b" still reads its data from the surviving block.
    let node_b = fs.lookup(root, b"b").expect("lookup b");
    assert_eq!(read_all(&mut fs, node_b, cap), body);

    fs.remove(root, b"b").expect("remove b");

    // The remount rebuilds free space from the trees; the chunk tree is empty
    // and the volume mounts cleanly, so the freed block is accounted for.
    let bytes = fs.into_block().bytes();
    let mut fs = RustFs::open(MemBlock::from_bytes(bytes, 4096, 256), &TEST_KEY).expect("remount");
    assert_eq!(chunk_count(&mut fs), 0);
    assert!(fs.dedupe_index.is_empty(), "index rebuilt empty");
    // The reclaimed space is reusable.
    fs.create(fs.root(), b"c", NodeKind::RegularFile)
        .expect("create c");
    assert_eq!(fs.write_at(fs.root(), b"c", 0, &body), Ok(cap));
}

#[test]
fn dedupe_index_rebuilds_from_the_chunk_tree_at_mount() {
    // The dedupe index is rebuildable, never authoritative: after a remount it
    // is rebuilt from the chunk + reverse-reference trees and yields the same
    // sharing — a third identical write joins the existing chunk (§9).
    let mut fs = fmt(4096, 256, 64);
    let root = fs.root();
    let cap = as_usize(fs.data_capacity());
    let body = alloc::vec![0x3Cu8; cap];
    for name in [b"a".as_slice(), b"b"] {
        fs.create(root, name, NodeKind::RegularFile)
            .expect("create");
        assert_eq!(fs.write_at(root, name, 0, &body), Ok(cap));
    }
    let shared = data_block_phys(&mut fs, b"a", 0);
    assert_eq!(fs.data_refcount(shared).expect("refcount"), 2);

    let bytes = fs.into_block().bytes();
    let mut fs = RustFs::open(MemBlock::from_bytes(bytes, 4096, 256), &TEST_KEY).expect("remount");
    let root = fs.root();
    assert_eq!(chunk_count(&mut fs), 1, "the shared chunk survives");

    fs.create(root, b"c", NodeKind::RegularFile)
        .expect("create c");
    assert_eq!(fs.write_at(root, b"c", 0, &body), Ok(cap));
    assert_eq!(
        data_block_phys(&mut fs, b"c", 0),
        shared,
        "the rebuilt index re-finds the shared chunk"
    );
    assert_eq!(
        fs.data_refcount(shared).expect("refcount"),
        3,
        "the third writer joined the shared chunk"
    );
}

#[test]
fn dedupe_is_scoped_to_the_encryption_domain() {
    // Every chunk record carries the volume's encryption domain, and the
    // dedupe-index key is domain-scoped, so dedupe never crosses a domain (§7).
    let mut fs = fmt(4096, 256, 64);
    let root = fs.root();
    let cap = as_usize(fs.data_capacity());
    let body = alloc::vec![0x6Du8; cap];
    for name in [b"a".as_slice(), b"b"] {
        fs.create(root, name, NodeKind::RegularFile)
            .expect("create");
        assert_eq!(fs.write_at(root, name, 0, &body), Ok(cap));
    }
    let shared = data_block_phys(&mut fs, b"a", 0);
    let record = fs.chunk_get(shared).expect("chunk get").expect("shared");
    assert_eq!(
        record.domain, fs.dedupe_domain,
        "the chunk belongs to the volume's domain"
    );

    // The index key is domain-scoped: a key built with a different domain does
    // not resolve to the chunk, so a foreign domain could never share it.
    let mut block = alloc::vec![0u8; cap];
    block.copy_from_slice(&body);
    let hash = logical_hash(&block);
    let len = u32::try_from(cap).unwrap();
    assert!(
        fs.dedupe_index
            .contains_key(&dedupe_key(fs.dedupe_domain, len, &hash)),
        "the chunk is indexed under its own domain"
    );
    assert!(
        !fs.dedupe_index
            .contains_key(&dedupe_key(fs.dedupe_domain ^ 0x1, len, &hash)),
        "a different domain keys to a different slot"
    );
}

#[test]
fn integrity_and_compression_hold_on_a_shared_chunk() {
    // A shared chunk is still a §6 data record: compressed at rest, integrity-
    // protected, and byte-exact across a remount and a COW rewrite. Corrupting
    // the shared physical block fails closed for *every* sharer (§5.4 / §6).
    let mut fs = fmt(4096, 256, 64);
    let root = fs.root();
    let cap = as_usize(fs.data_capacity());
    // Compressible, identical content so the two files share one chunk.
    let mut body = alloc::vec::Vec::new();
    while body.len() < cap {
        body.extend_from_slice(b"RustOS rustfs dedupe ");
    }
    body.truncate(cap);
    for name in [b"a".as_slice(), b"b"] {
        fs.create(root, name, NodeKind::RegularFile)
            .expect("create");
        assert_eq!(fs.write_at(root, name, 0, &body), Ok(cap));
    }
    let shared = data_block_phys(&mut fs, b"a", 0);
    assert_eq!(fs.data_refcount(shared).expect("refcount"), 2);
    assert!(
        stored_compression(&mut fs, b"a", 0).compressed,
        "the shared chunk is stored compressed"
    );

    // Round-trips across a remount.
    let bytes = fs.into_block().bytes();
    let mut fs = RustFs::open(MemBlock::from_bytes(bytes, 4096, 256), &TEST_KEY).expect("remount");
    let root = fs.root();
    let node_a = fs.lookup(root, b"a").expect("lookup a");
    let node_b = fs.lookup(root, b"b").expect("lookup b");
    assert_eq!(read_all(&mut fs, node_a, cap), body);
    assert_eq!(read_all(&mut fs, node_b, cap), body);

    // Corrupting the shared at-rest block is caught for both sharers (the fast
    // physical checksum covers the at-rest bytes).
    let base = as_usize(shared) * 4096;
    let mut bytes = fs.into_block().bytes();
    bytes[base] ^= 0x01;
    let mut fs = RustFs::open(MemBlock::from_bytes(bytes, 4096, 256), &TEST_KEY).expect("remount2");
    let root = fs.root();
    let node_a = fs.lookup(root, b"a").expect("lookup a");
    let node_b = fs.lookup(root, b"b").expect("lookup b");
    let mut buf = alloc::vec![0u8; cap];
    assert!(
        matches!(
            fs.read_at(node_a, 0, &mut buf),
            Err(DriverError::DeviceFault)
        ),
        "corruption of the shared chunk fails closed for sharer a"
    );
    assert!(
        matches!(
            fs.read_at(node_b, 0, &mut buf),
            Err(DriverError::DeviceFault)
        ),
        "corruption of the shared chunk fails closed for sharer b"
    );
}

/// A deterministic, high-entropy buffer of `len` bytes — distinct per block, so
/// it is neither compressible nor deduplicable (used to exercise reflink block
/// sharing without accidental cross-block dedupe).
fn read_all_pattern(len: usize) -> alloc::vec::Vec<u8> {
    let mut out = alloc::vec::Vec::with_capacity(len);
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    for _ in 0..len {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        out.push(state.wrapping_mul(0x2545_F491_4F6C_DD1D).to_le_bytes()[0]);
    }
    out
}

// ---------------------------------------------------------------------------
// Stage 8: online scrub (verify + repair, resumable).
// ---------------------------------------------------------------------------

use rustos_abi::CapabilityQuery;
use rustos_log::{Event, Sink};

/// A capability set granting every capability (the scrub gate is satisfied).
struct GrantAll;
impl CapabilityQuery for GrantAll {
    fn holds(&self, _cap: CapabilityId) -> bool {
        true
    }
}

/// A capability set granting nothing (the scrub gate fails closed).
struct GrantNone;
impl CapabilityQuery for GrantNone {
    fn holds(&self, _cap: CapabilityId) -> bool {
        false
    }
}

/// A log sink that discards events.
struct NullSink;
impl Sink for NullSink {
    fn write_event(&self, _event: &Event<'_>) {}
}

/// A log sink that records the event IDs it receives.
struct RecordingSink {
    ids: core::cell::RefCell<alloc::vec::Vec<u32>>,
}
impl RecordingSink {
    fn new() -> Self {
        Self {
            ids: core::cell::RefCell::new(alloc::vec::Vec::new()),
        }
    }
    fn saw(&self, id: rustos_log::EventId) -> bool {
        self.ids.borrow().contains(&id.0)
    }
}
impl Sink for RecordingSink {
    fn write_event(&self, event: &Event<'_>) {
        self.ids.borrow_mut().push(event.id.0);
    }
}

/// Run a full scrub with all capabilities granted, asserting it succeeds.
fn scrub_full(fs: &mut RustFs<MemBlock>) -> ScrubReport {
    fs.scrub(&GrantAll, &NullSink, ScrubBudget::Unlimited)
        .expect("scrub")
}

/// Populate a small volume with a directory tree, plain files, a pair of
/// identical-content files that dedupe, and a reflink, so scrub has metadata,
/// data, and shared chunks to verify.
fn populated() -> RustFs<MemBlock> {
    let mut fs = fmt(4096, 512, 128);
    let root = fs.root();
    let cap = as_usize(fs.data_capacity());
    fs.create(root, b"plain", NodeKind::RegularFile)
        .expect("create plain");
    let body = alloc::vec![0x24u8; cap + 100];
    fs.write_at(root, b"plain", 0, &body).expect("write plain");

    fs.create(root, b"a", NodeKind::RegularFile)
        .expect("create a");
    fs.create(root, b"b", NodeKind::RegularFile)
        .expect("create b");
    let shared_body = alloc::vec![0x5Au8; cap];
    fs.write_at(root, b"a", 0, &shared_body).expect("write a");
    fs.write_at(root, b"b", 0, &shared_body).expect("write b");

    fs.reflink(root, b"plain", b"clone").expect("reflink");

    fs.create(root, b"dir", NodeKind::Directory)
        .expect("create dir");
    let dir = fs.lookup(root, b"dir").expect("lookup dir");
    fs.create(dir, b"nested", NodeKind::RegularFile)
        .expect("create nested");
    fs.write_at(dir, b"nested", 0, b"nested file")
        .expect("write nested");
    fs
}

#[test]
fn clean_scrub_finds_nothing_and_is_idempotent() {
    // A scrub of a clean, populated volume reports zero faults, makes no
    // repairs, and changes nothing on disk — running it again is identical
    // (`docs/src/filesystem/rustfs-spec.md` §12; the report is the only output,
    // never a silent mutation).
    let before = populated().into_block().bytes();
    let mut fs =
        RustFs::open(MemBlock::from_bytes(before.clone(), 4096, 512), &TEST_KEY).expect("reopen");

    let report = scrub_full(&mut fs);
    assert!(report.complete);
    assert!(!report.found_faults(), "{report:?}");
    assert_eq!(report.metadata_repaired, 0);
    assert_eq!(report.metadata_unrepairable, 0);
    assert_eq!(report.divergences_corrected, 0);
    assert!(report.metadata_blocks_checked > 0, "metadata was verified");
    assert!(report.data_blocks_checked > 0, "data was verified");

    // A clean scrub mutates nothing on disk.
    let after = fs.into_block().bytes();
    assert_eq!(after, before, "a clean scrub must change nothing");

    // Idempotent: a second scrub agrees.
    let mut fs = RustFs::open(MemBlock::from_bytes(after, 4096, 512), &TEST_KEY).expect("reopen2");
    let again = scrub_full(&mut fs);
    assert_eq!(again, report, "scrub is idempotent on a clean volume");
}

#[test]
fn scrub_requires_the_fs_mount_capability() {
    // Scrub is capability-gated like any privileged FS operation (§13,
    // `AGENTS.md` §5.4): without `CAP_FS_MOUNT` it fails closed and logs the
    // refusal with its stable event ID.
    let mut fs = populated();
    let sink = RecordingSink::new();
    assert_eq!(
        fs.scrub(&GrantNone, &sink, ScrubBudget::Unlimited),
        Err(DriverError::PermissionDenied)
    );
    assert!(sink.saw(scrub::SCRUB_DENIED), "the refusal is logged");
}

#[test]
fn scrub_repairs_a_single_copy_metadata_corruption_from_the_companion() {
    // Wound only the primary copy of a live metadata block (the inode-tree
    // root). Scrub authenticates both copies, repairs the bad primary from its
    // good companion (the Stage 3 seam), and reports exactly one repair
    // (`docs/src/filesystem/rustfs-spec.md` §8, §12).
    let mut fs = populated();
    // Target a directory data block: `open`'s free-space walk reads (and would
    // self-heal) every tree node, but it never reads directory *contents*, so
    // a wounded directory-block primary survives the mount for scrub to repair.
    let root_inode = fs.read_inode(ROOT_INO).expect("root inode");
    let target = fs.block_ptr(&root_inode, 0).expect("root dir block");
    assert_ne!(target, 0);
    let bs = 4096usize;
    let mut bytes = fs.into_block().bytes();
    let off = as_usize(target) * bs + HEADER_LEN;
    let original = bytes[off];
    bytes[off] ^= 0xff; // wound only the primary copy

    let mut fs = RustFs::open(MemBlock::from_bytes(bytes, 4096, 512), &TEST_KEY)
        .expect("mounts via the companion mirror");
    let report = scrub_full(&mut fs);
    assert!(report.complete);
    assert_eq!(report.metadata_repaired, 1, "{report:?}");
    assert_eq!(report.metadata_unrepairable, 0);

    // The primary copy is healed back to match its companion.
    let healed = fs.into_block().bytes();
    let p = as_usize(target) * bs;
    let c = as_usize(target + 1) * bs;
    assert_eq!(healed[p..p + bs], healed[c..c + bs], "primary repaired");
    assert_eq!(healed[off], original, "the corrupted byte is restored");
}

#[test]
fn scrub_classifies_data_block_physical_and_logical_faults() {
    // Scrub runs every data block through the Stage 5/6 integrity pipeline and
    // classifies a failure by its layer without panicking
    // (`docs/src/filesystem/rustfs-spec.md` §6, §12). Deep data repair is a
    // later stage, so the fault is recorded, not fixed.
    let bs = 4096usize;

    // A flipped ciphertext byte is a physical-checksum fault.
    {
        let mut fs = fmt(4096, 256, 64);
        let root = fs.root();
        fs.create(root, b"f", NodeKind::RegularFile)
            .expect("create");
        let body = alloc::vec![0x33u8; 400];
        fs.write_at(root, b"f", 0, &body).expect("write");
        let phys = data_block_phys(&mut fs, b"f", 0);
        let mut bytes = fs.into_block().bytes();
        bytes[as_usize(phys) * bs] ^= 0x01;
        let mut fs =
            RustFs::open(MemBlock::from_bytes(bytes, 4096, 256), &TEST_KEY).expect("reopen");
        let report = scrub_full(&mut fs);
        assert_eq!(report.data_physical_faults, 1, "{report:?}");
        assert_eq!(report.data_aead_faults, 0);
        assert_eq!(report.data_logical_faults, 0);
    }

    // Corrupting the stored logical hash (with the physical checksum repaired)
    // is a logical fault: the AEAD passes but the plaintext hash mismatches.
    {
        let mut fs = fmt(4096, 256, 64);
        let root = fs.root();
        fs.create(root, b"f", NodeKind::RegularFile)
            .expect("create");
        let body = alloc::vec![0x44u8; 400];
        fs.write_at(root, b"f", 0, &body).expect("write");
        let phys = data_block_phys(&mut fs, b"f", 0);
        let csum_off = fs.phys_checksum_offset();
        let hash_off = fs.logical_hash_offset();
        let base = as_usize(phys) * bs;
        let mut bytes = fs.into_block().bytes();
        bytes[base + hash_off] ^= 0x01;
        let fixed = physical_checksum(&bytes[base..base + csum_off]);
        bytes[base + csum_off..base + csum_off + PHYS_CHECKSUM_LEN].copy_from_slice(&fixed);
        let mut fs =
            RustFs::open(MemBlock::from_bytes(bytes, 4096, 256), &TEST_KEY).expect("reopen");
        let report = scrub_full(&mut fs);
        assert_eq!(report.data_logical_faults, 1, "{report:?}");
        assert_eq!(report.data_physical_faults, 0);
    }
}

#[test]
fn scrub_detects_and_corrects_a_refcount_divergence() {
    // Two identical files share one chunk (refcount 2). Tamper the on-disk
    // chunk record's refcount; scrub recomputes the truth from the live
    // extents, flags the divergence, and corrects it without losing a referrer
    // (`docs/src/filesystem/rustfs-spec.md` §9, §12).
    let mut fs = fmt(4096, 256, 64);
    let root = fs.root();
    let cap = as_usize(fs.data_capacity());
    fs.create(root, b"a", NodeKind::RegularFile)
        .expect("create a");
    fs.create(root, b"b", NodeKind::RegularFile)
        .expect("create b");
    let body = alloc::vec![0x5Au8; cap];
    fs.write_at(root, b"a", 0, &body).expect("write a");
    fs.write_at(root, b"b", 0, &body).expect("write b");
    let shared = data_block_phys(&mut fs, b"a", 0);
    assert_eq!(fs.data_refcount(shared).expect("refcount"), 2);

    // Tamper: bump the stored refcount to a value the extents do not support.
    let record = fs.chunk_get(shared).expect("get").expect("shared");
    let bumped = ChunkRecord {
        refcount: 5,
        ..record
    };
    fs.begin();
    fs.chunk_put(shared, &bumped).expect("put");
    fs.commit().expect("commit");
    assert_eq!(fs.data_refcount(shared).expect("refcount"), 5);

    let report = scrub_full(&mut fs);
    assert!(report.complete);
    assert!(report.refcount_divergences >= 1, "{report:?}");
    assert!(report.divergences_corrected >= 1, "{report:?}");
    assert_eq!(
        fs.data_refcount(shared).expect("refcount"),
        2,
        "scrub restored the refcount to the extent-derived truth"
    );

    // The correction holds across a remount and a re-scrub is clean.
    let bytes = fs.into_block().bytes();
    let mut fs = RustFs::open(MemBlock::from_bytes(bytes, 4096, 256), &TEST_KEY).expect("reopen");
    assert_eq!(fs.data_refcount(shared).expect("refcount"), 2);
    let again = scrub_full(&mut fs);
    assert_eq!(again.refcount_divergences, 0, "clean after correction");
    assert_eq!(again.divergences_corrected, 0);
}

#[test]
fn scrub_detects_and_corrects_a_reverse_reference_divergence() {
    // A shared chunk's reverse-reference set must match its live referrers.
    // Inject a bogus referrer; scrub recomputes the true set and corrects it.
    let mut fs = fmt(4096, 256, 64);
    let root = fs.root();
    let cap = as_usize(fs.data_capacity());
    fs.create(root, b"a", NodeKind::RegularFile)
        .expect("create a");
    fs.create(root, b"b", NodeKind::RegularFile)
        .expect("create b");
    let body = alloc::vec![0x5Au8; cap];
    fs.write_at(root, b"a", 0, &body).expect("write a");
    fs.write_at(root, b"b", 0, &body).expect("write b");
    let shared = data_block_phys(&mut fs, b"a", 0);

    let mut referrers = fs.reverse_refs(shared).expect("reverse refs");
    referrers.push((9999, 7)); // a referrer no live extent supports
    fs.begin();
    fs.reverse_refs_put(shared, &referrers).expect("put");
    fs.commit().expect("commit");

    let report = scrub_full(&mut fs);
    assert!(report.reverse_ref_divergences >= 1, "{report:?}");
    assert!(report.divergences_corrected >= 1);
    let healed = fs.reverse_refs(shared).expect("reverse refs");
    assert!(
        !healed.contains(&(9999, 7)),
        "the bogus referrer was struck out"
    );
    assert_eq!(healed.len(), 2, "exactly the two live referrers remain");
}

#[test]
fn scrub_accounts_a_shared_chunk_once_and_respects_the_domain() {
    // A chunk with refcount 2 is verified, and the recompute accounts both
    // referrers (so no spurious divergence). The volume's single dedupe domain
    // is honoured: the chunk record's domain matches and is left intact.
    let mut fs = fmt(4096, 256, 64);
    let root = fs.root();
    let cap = as_usize(fs.data_capacity());
    fs.create(root, b"a", NodeKind::RegularFile)
        .expect("create a");
    fs.create(root, b"b", NodeKind::RegularFile)
        .expect("create b");
    let body = alloc::vec![0x5Au8; cap];
    fs.write_at(root, b"a", 0, &body).expect("write a");
    fs.write_at(root, b"b", 0, &body).expect("write b");
    let shared = data_block_phys(&mut fs, b"a", 0);
    assert_eq!(fs.data_refcount(shared).expect("refcount"), 2);
    let before = fs.chunk_get(shared).expect("get").expect("shared");

    let report = scrub_full(&mut fs);
    assert!(report.complete);
    assert_eq!(report.refcount_divergences, 0, "{report:?}");
    assert_eq!(report.reverse_ref_divergences, 0);
    assert_eq!(report.divergences_corrected, 0);
    let after = fs.chunk_get(shared).expect("get").expect("still shared");
    assert_eq!(after, before, "the shared chunk record is untouched");
    assert_eq!(after.domain, fs.dedupe_domain, "domain preserved");
}

#[test]
fn scrub_is_resumable_and_matches_an_uninterrupted_pass() {
    // A budgeted scrub stops after each inode, persists a resumable cursor, and
    // resumes; the accumulated report of the resumed scrub equals one
    // uninterrupted pass, and the cursor is cleared on completion
    // (`docs/src/filesystem/rustfs-spec.md` §12).
    let base = populated().into_block().bytes();

    // Uninterrupted reference pass.
    let mut whole = RustFs::open(MemBlock::from_bytes(base.clone(), 4096, 512), &TEST_KEY)
        .expect("reopen whole");
    let reference = scrub_full(&mut whole);
    assert!(reference.complete);

    // Resumed pass: one inode per call until it completes.
    let mut fs =
        RustFs::open(MemBlock::from_bytes(base, 4096, 512), &TEST_KEY).expect("reopen stepwise");
    let mut calls = 0;
    let last = loop {
        let report = fs
            .scrub(&GrantAll, &NullSink, ScrubBudget::Inodes(1))
            .expect("scrub step");
        calls += 1;
        if report.complete {
            break report;
        }
        assert_ne!(
            fs.scrub_progress_root, 0,
            "a paused scrub persists a cursor"
        );
        assert!(calls < 1000, "scrub must terminate");
    };
    assert!(calls > 1, "the budget actually paused the pass");
    assert_eq!(
        fs.scrub_progress_root, 0,
        "the cursor is cleared on completion"
    );
    assert_eq!(
        last, reference,
        "a resumed scrub reaches the same result as one pass"
    );
}

#[test]
fn a_crash_mid_scrub_leaves_a_mountable_volume() {
    // Interrupting a scrub mid-pass (after it persisted a cursor) must leave a
    // mountable volume: the progress record is rebuildable metadata and
    // ordinary recovery never needs scrub (`docs/src/filesystem/rustfs-spec.md`
    // §4, §14). The half-done scrub then resumes to completion.
    let mut fs = populated();
    let paused = fs
        .scrub(&GrantAll, &NullSink, ScrubBudget::Inodes(1))
        .expect("first step");
    assert!(!paused.complete);
    assert_ne!(fs.scrub_progress_root, 0);

    // Simulate a crash: drop the in-memory state and reopen from disk.
    let bytes = fs.into_block().bytes();
    let mut fs = RustFs::open(MemBlock::from_bytes(bytes, 4096, 512), &TEST_KEY)
        .expect("a volume with a scrub in progress still mounts");
    assert_ne!(fs.scrub_progress_root, 0, "the cursor survived the crash");

    // The file system is fully usable, and the scrub resumes and completes.
    let root = fs.root();
    assert!(fs.lookup(root, b"plain").is_ok());
    let report = fs
        .scrub(&GrantAll, &NullSink, ScrubBudget::Unlimited)
        .expect("resume");
    assert!(report.complete);
    assert_eq!(fs.scrub_progress_root, 0);
}

#[test]
fn invariants_hold_across_scrub_remount_and_cow_rewrite() {
    // Integrity + compression + dedupe invariants survive a scrub, a remount,
    // and a copy-on-write rewrite of one sharer.
    let mut fs = fmt(4096, 512, 128);
    let root = fs.root();
    let cap = as_usize(fs.data_capacity());
    fs.create(root, b"a", NodeKind::RegularFile)
        .expect("create a");
    fs.create(root, b"b", NodeKind::RegularFile)
        .expect("create b");
    let body = alloc::vec![0x5Au8; cap];
    fs.write_at(root, b"a", 0, &body).expect("write a");
    fs.write_at(root, b"b", 0, &body).expect("write b");
    let shared = data_block_phys(&mut fs, b"a", 0);
    assert_eq!(fs.data_refcount(shared).expect("refcount"), 2);

    let report = scrub_full(&mut fs);
    assert!(report.complete && !report.found_faults(), "{report:?}");

    // Remount, then rewrite one sharer (copy-on-write off the shared chunk).
    let bytes = fs.into_block().bytes();
    let mut fs = RustFs::open(MemBlock::from_bytes(bytes, 4096, 512), &TEST_KEY).expect("reopen");
    let replacement = alloc::vec![0xA5u8; cap];
    fs.write_at(root, b"a", 0, &replacement).expect("rewrite a");
    let pa = data_block_phys(&mut fs, b"a", 0);
    assert_ne!(pa, shared, "the writer copied on write");
    assert_eq!(
        fs.data_refcount(shared).expect("refcount"),
        1,
        "the surviving sharer holds the implicit single reference"
    );

    // Scrub again: still clean, and the data reads back correctly.
    let report = scrub_full(&mut fs);
    assert!(report.complete && !report.found_faults(), "{report:?}");
    let node_a = fs.lookup(root, b"a").expect("lookup a");
    assert_eq!(read_all(&mut fs, node_a, cap), replacement);
    let node_b = fs.lookup(root, b"b").expect("lookup b");
    assert_eq!(read_all(&mut fs, node_b, cap), body);
}

// ---------------------------------------------------------------------------
// Stage 9: offline check and rescue.
// ---------------------------------------------------------------------------

use crate::check::{self, RescueSink};
use crate::CheckReport;

/// A rescue sink that collects every emitted block keyed by `(inode, logical)`.
struct CollectSink {
    blocks: alloc::collections::BTreeMap<(u32, u64), alloc::vec::Vec<u8>>,
}
impl CollectSink {
    fn new() -> Self {
        Self {
            blocks: alloc::collections::BTreeMap::new(),
        }
    }
}
impl RescueSink for CollectSink {
    fn emit_block(&mut self, inode: u32, logical_block: u64, _size: u64, data: &[u8]) {
        self.blocks.insert((inode, logical_block), data.to_vec());
    }
}

/// Run a full check with all capabilities granted, asserting it succeeds.
fn check_full(fs: &mut RustFs<MemBlock>) -> CheckReport {
    fs.check(&GrantAll, &NullSink).expect("check")
}

#[test]
fn check_requires_the_fs_mount_capability() {
    let mut fs = populated();
    let sink = RecordingSink::new();
    assert_eq!(
        fs.check(&GrantNone, &sink),
        Err(DriverError::PermissionDenied)
    );
    assert!(sink.saw(check::CHECK_DENIED), "the refusal is logged");
}

#[test]
fn check_on_a_clean_volume_is_sound_and_rebuilds_nothing() {
    // A check of a clean, populated volume reports a sound structure, rebuilds
    // the derived state (always), repairs nothing, and changes nothing on disk
    // — running it again is identical (`docs/src/filesystem/rustfs-spec.md`
    // §12).
    let before = populated().into_block().bytes();
    let mut fs =
        RustFs::open(MemBlock::from_bytes(before.clone(), 4096, 512), &TEST_KEY).expect("reopen");

    let sink = RecordingSink::new();
    let report = fs.check(&GrantAll, &sink).expect("check");
    assert!(report.complete);
    assert!(report.structure_sound, "{report:?}");
    assert!(report.rebuilt_derived_state);
    assert_eq!(report.unrecoverable_findings, 0);
    assert!(!report.made_repairs(), "a clean check repairs nothing");
    assert_eq!(report.orphaned_inodes, 0);
    assert_eq!(report.dangling_entries, 0);
    assert!(report.directories_checked > 0, "directories were walked");
    assert!(
        sink.saw(check::CHECK_CLEAN),
        "a clean check logs CHECK_CLEAN"
    );

    // A clean check mutates nothing on disk.
    let after = fs.into_block().bytes();
    assert_eq!(after, before, "a clean check must change nothing");

    // Idempotent: a second check agrees.
    let mut fs = RustFs::open(MemBlock::from_bytes(after, 4096, 512), &TEST_KEY).expect("reopen2");
    let again = check_full(&mut fs);
    assert_eq!(again, report, "check is idempotent on a clean volume");
}

#[test]
fn check_rebuilds_a_corrupt_free_space_and_dedupe_derivation() {
    // The free-space bitmap and the dedupe index are rebuildable derived state
    // (§4, §9), never authoritative. A corrupt derivation must never keep a
    // sound volume unmountable: check rebuilds both from the authoritative
    // trees, and the result matches a freshly mounted reference.
    let bytes = populated().into_block().bytes();
    let reference =
        RustFs::open(MemBlock::from_bytes(bytes.clone(), 4096, 512), &TEST_KEY).expect("reference");
    let good_free = reference.free.clone();
    let good_count = reference.free_count;
    assert!(
        !reference.dedupe_index.is_empty(),
        "the populated volume has shared chunks indexed"
    );

    let mut fs = RustFs::open(MemBlock::from_bytes(bytes, 4096, 512), &TEST_KEY).expect("reopen");
    // Wreck the in-memory derived state: flip the free bitmap and clear the
    // dedupe index.
    for word in &mut fs.free {
        *word = !*word;
    }
    fs.free_count = 0;
    fs.dedupe_index.clear();

    let report = check_full(&mut fs);
    assert!(report.complete);
    assert!(report.rebuilt_derived_state);
    assert_eq!(fs.free, good_free, "the free bitmap was rebuilt");
    assert_eq!(fs.free_count, good_count, "the free count was rebuilt");
    assert!(
        !fs.dedupe_index.is_empty(),
        "the dedupe index was rebuilt from the chunk trees"
    );
    // The volume is mountable and the structure is sound.
    assert!(report.structure_sound, "{report:?}");
    let bytes = fs.into_block().bytes();
    assert!(RustFs::open(MemBlock::from_bytes(bytes, 4096, 512), &TEST_KEY).is_ok());
}

#[test]
fn check_reclaims_an_orphaned_inode() {
    // An inode no directory reaches is an orphan. Check detects it, reclaims it
    // (freeing its slot), and the volume stays sound
    // (`docs/src/filesystem/rustfs-spec.md` §12).
    let mut fs = fmt(4096, 512, 128);
    let root = fs.root();
    fs.create(root, b"keep", NodeKind::RegularFile)
        .expect("create keep");

    // Inject an orphan: allocate an inode and never link it into any directory.
    fs.begin();
    let sec = Security::new(0o644, 0, 0);
    let orphan = fs
        .alloc_inode(&Inode::empty(KIND_FILE, sec, fixed_clock()))
        .expect("alloc orphan");
    fs.commit().expect("commit orphan");
    assert!(fs.read_inode(orphan).is_ok(), "the orphan exists pre-check");

    let report = check_full(&mut fs);
    assert!(report.complete);
    assert_eq!(report.orphaned_inodes, 1, "{report:?}");
    assert_eq!(report.orphans_reclaimed, 1);
    assert!(report.made_repairs());
    assert!(report.structure_sound, "the orphan was safely reclaimed");

    // The orphan is gone, and the named file is untouched.
    assert_eq!(fs.read_inode(orphan), Err(DriverError::NotFound));
    assert!(fs.lookup(fs.root(), b"keep").is_ok());

    // A re-check finds nothing left to reclaim.
    let again = check_full(&mut fs);
    assert_eq!(again.orphans_reclaimed, 0);
    assert!(again.structure_sound);
}

#[test]
fn check_corrects_a_refcount_divergence_and_reports_what_it_cannot_fix() {
    // Check reuses the scrub verification core: it corrects a refcount
    // divergence it can fix, and reports a data integrity fault it cannot
    // safely repair as an unrecoverable finding
    // (`docs/src/filesystem/rustfs-spec.md` §9, §12).
    let mut fs = fmt(4096, 256, 64);
    let root = fs.root();
    let cap = as_usize(fs.data_capacity());
    fs.create(root, b"a", NodeKind::RegularFile)
        .expect("create a");
    fs.create(root, b"b", NodeKind::RegularFile)
        .expect("create b");
    let body = alloc::vec![0x5Au8; cap];
    fs.write_at(root, b"a", 0, &body).expect("write a");
    fs.write_at(root, b"b", 0, &body).expect("write b");
    let shared = data_block_phys(&mut fs, b"a", 0);
    assert_eq!(fs.data_refcount(shared).expect("refcount"), 2);

    // Tamper the on-disk chunk refcount to a value the extents do not support.
    let record = fs.chunk_get(shared).expect("get").expect("shared");
    fs.begin();
    fs.chunk_put(
        shared,
        &ChunkRecord {
            refcount: 9,
            ..record
        },
    )
    .expect("put");
    fs.commit().expect("commit");

    let report = check_full(&mut fs);
    assert!(report.complete);
    assert!(report.verification.refcount_divergences >= 1, "{report:?}");
    assert!(report.verification.divergences_corrected >= 1);
    assert_eq!(
        fs.data_refcount(shared).expect("refcount"),
        2,
        "the refcount was corrected toward the extent-derived truth"
    );
    // The correctable divergence does not leave an unrecoverable finding.
    assert_eq!(report.unrecoverable_findings, 0, "{report:?}");
}

#[test]
fn check_reports_an_unrepairable_data_block_it_cannot_fix() {
    // A both-layers-corrupt data block (a deep fault check does not yet
    // reconstruct) is recorded as an unrecoverable finding, not silently
    // ignored or pretended fixed (`AGENTS.md` §2.1).
    let mut fs = fmt(4096, 256, 64);
    let root = fs.root();
    fs.create(root, b"f", NodeKind::RegularFile)
        .expect("create");
    fs.write_at(root, b"f", 0, &alloc::vec![0x33u8; 400])
        .expect("write");
    let phys = data_block_phys(&mut fs, b"f", 0);
    let bs = 4096usize;
    let mut bytes = fs.into_block().bytes();
    bytes[as_usize(phys) * bs] ^= 0x01; // wound the ciphertext (physical fault)

    let mut fs = RustFs::open(MemBlock::from_bytes(bytes, 4096, 256), &TEST_KEY).expect("reopen");
    let report = check_full(&mut fs);
    assert!(report.complete);
    assert_eq!(report.verification.data_physical_faults, 1, "{report:?}");
    assert!(
        !report.structure_sound,
        "an unrepaired data fault is reported, not hidden"
    );
    assert!(report.unrecoverable_findings >= 1);
}

/// Byte offset inside the keyed-tag slot of a metadata-block header; flipping
/// it breaks that block's authenticator (mirrors the fuzz harness constant).
const HEADER_TAG_BYTE: usize = HEADER_LEN - 48 + 8; // inside H_MAC..H_MAC_END

/// Wound the keyed authenticator of every superblock-ring block (both copies
/// of every slot) so the volume no longer mounts, while leaving the plaintext
/// crypto discovery header and every other metadata block intact.
fn damage_superblock_ring(bytes: &mut [u8], bs: usize) {
    for block in 0..RING_BLOCKS {
        bytes[as_usize(block) * bs + HEADER_TAG_BYTE] ^= 0xff;
    }
}

#[test]
fn rescue_requires_the_fs_mount_capability() {
    let bytes = populated().into_block().bytes();
    let sink = RecordingSink::new();
    let mut out = CollectSink::new();
    assert_eq!(
        RustFs::rescue(
            MemBlock::from_bytes(bytes, 4096, 512),
            &TEST_KEY,
            &GrantNone,
            &sink,
            &mut out,
        ),
        Err(DriverError::PermissionDenied)
    );
    assert!(sink.saw(check::RESCUE_DENIED), "the refusal is logged");
}

#[test]
fn rescue_discovers_a_root_and_extracts_files_from_a_damaged_ring() {
    // With the superblock ring wounded the volume no longer mounts, but rescue
    // recovers the keys from the surviving discovery header, scans for a valid
    // transaction root, and extracts the readable file data
    // (`docs/src/filesystem/rustfs-spec.md` §12).
    let bs = 4096usize;
    let mut fs = fmt(4096, 512, 128);
    let root = fs.root();
    let cap = as_usize(fs.data_capacity());
    let body = read_all_pattern(cap + cap / 2); // two logical blocks
    fs.create(root, b"doc", NodeKind::RegularFile)
        .expect("create");
    fs.write_at(root, b"doc", 0, &body).expect("write");
    let doc_node = fs.lookup(root, b"doc").expect("lookup");
    let doc_ino = u32::try_from(doc_node.raw()).unwrap();
    let mut bytes = fs.into_block().bytes();

    // The wounded ring makes an ordinary mount fail closed.
    damage_superblock_ring(&mut bytes, bs);
    assert!(
        RustFs::open(MemBlock::from_bytes(bytes.clone(), 4096, 512), &TEST_KEY).is_err(),
        "the damaged ring no longer mounts normally"
    );

    let sink = RecordingSink::new();
    let mut out = CollectSink::new();
    let report = RustFs::rescue(
        MemBlock::from_bytes(bytes.clone(), 4096, 512),
        &TEST_KEY,
        &GrantAll,
        &sink,
        &mut out,
    )
    .expect("rescue");
    assert!(report.found_root(), "rescue discovered a valid root");
    assert!(report.files_mapped >= 1);
    assert_eq!(report.blocks_extracted, 2, "{report:?}");
    assert_eq!(report.blocks_skipped, 0);
    assert!(sink.saw(check::RESCUE_COMPLETE));

    // The recovered blocks reconstruct the file content.
    let b0 = out.blocks.get(&(doc_ino, 0)).expect("block 0 recovered");
    let b1 = out.blocks.get(&(doc_ino, 1)).expect("block 1 recovered");
    assert_eq!(&b0[..cap], &body[..cap]);
    assert_eq!(&b1[..body.len() - cap], &body[cap..]);

    // Rescue is read-only on the damaged volume: the device is unchanged.
    let after = RustFs::rescue(
        MemBlock::from_bytes(bytes.clone(), 4096, 512),
        &TEST_KEY,
        &GrantAll,
        &NullSink,
        &mut CollectSink::new(),
    );
    assert!(after.is_ok(), "rescue is repeatable and never mutates");
}

#[test]
fn rescue_never_emits_a_block_that_fails_integrity() {
    // A data block whose integrity check fails is skipped, never handed back
    // (`docs/src/filesystem/rustfs-spec.md` §6, §12). The good block of the
    // same file is still extracted.
    let bs = 4096usize;
    let mut fs = fmt(4096, 512, 128);
    let root = fs.root();
    let cap = as_usize(fs.data_capacity());
    let body = read_all_pattern(2 * cap); // exactly two logical blocks
    fs.create(root, b"doc", NodeKind::RegularFile)
        .expect("create");
    fs.write_at(root, b"doc", 0, &body).expect("write");
    let doc_node = fs.lookup(root, b"doc").expect("lookup");
    let doc_ino = u32::try_from(doc_node.raw()).unwrap();
    let bad_phys = data_block_phys(&mut fs, b"doc", 1);
    let mut bytes = fs.into_block().bytes();

    // Wound the second block's ciphertext (a physical-checksum fault) and the
    // ring (so rescue is the recovery path).
    bytes[as_usize(bad_phys) * bs] ^= 0x01;
    damage_superblock_ring(&mut bytes, bs);

    let mut out = CollectSink::new();
    let report = RustFs::rescue(
        MemBlock::from_bytes(bytes, 4096, 512),
        &TEST_KEY,
        &GrantAll,
        &NullSink,
        &mut out,
    )
    .expect("rescue");
    assert!(report.found_root());
    assert_eq!(report.blocks_extracted, 1, "only the good block is emitted");
    assert_eq!(report.blocks_skipped, 1, "the corrupt block is skipped");
    assert!(
        out.blocks.contains_key(&(doc_ino, 0)),
        "the readable block is recovered"
    );
    assert!(
        !out.blocks.contains_key(&(doc_ino, 1)),
        "a block that fails integrity is never emitted"
    );
}

// ---------------------------------------------------------------------------
// Stage: TRIM/discard (return freed space to the device, safely; §11).
// ---------------------------------------------------------------------------

/// Format a fresh volume on a discard-capable [`MemBlock`] and clear the
/// record of the mkfs-time discard so a trim test starts from a clean slate.
fn fmt_discard(
    block_count: u64,
    granularity_blocks: u64,
    max_blocks_per_request: u64,
) -> RustFs<MemBlock> {
    let block =
        MemBlock::new(512, block_count).with_discard(granularity_blocks, max_blocks_per_request);
    let mut fs = RustFs::format(block, 32, &TEST_KEY, &mut TestEntropy::new())
        .expect("format a discard-capable device")
        .with_clock(fixed_clock);
    fs.block.discarded.clear();
    fs
}

/// Enqueue every block in `[start, end)` for discard, asserting each is
/// currently free so the test models the real invariant (only free blocks are
/// ever queued).
fn enqueue_free_range(fs: &mut RustFs<MemBlock>, start: u64, end: u64) {
    for block in start..end {
        assert!(!fs.bit_used(block), "test block {block} must start free");
        fs.enqueue_discard(block);
    }
}

#[test]
fn trim_requires_the_fs_mount_capability() {
    // Fail closed: without CAP_FS_MOUNT trim refuses, logs the refusal, and
    // leaves the queue untouched (`AGENTS.md` §5.4).
    let mut fs = fmt_discard(512, 1, 0);
    enqueue_free_range(&mut fs, 100, 110);
    let before = fs.pending_discard_count();
    let sink = RecordingSink::new();
    assert_eq!(
        fs.trim(&GrantNone, &sink),
        Err(DriverError::PermissionDenied)
    );
    assert!(sink.saw(discard::TRIM_DENIED), "the refusal is logged");
    assert_eq!(fs.pending_discard_count(), before, "the queue is untouched");
    assert!(fs.block.discarded.is_empty(), "nothing was discarded");
}

#[test]
fn trim_on_a_device_without_discard_drains_the_queue() {
    // Recorded, not failed: a device without discard support drains the queue
    // and reports `supported = false` (§11). There is no trim=off mode.
    let mut fs = fmt(512, 512, 32);
    enqueue_free_range(&mut fs, 100, 110);
    assert!(fs.pending_discard_count() > 0);
    let sink = RecordingSink::new();
    let report = fs.trim(&GrantAll, &sink).expect("trim is never an error");
    assert!(!report.supported);
    assert_eq!(report.blocks_discarded, 0);
    assert_eq!(fs.pending_discard_count(), 0, "the queue is drained");
    assert!(sink.saw(discard::TRIM_UNSUPPORTED));
}

#[test]
fn trim_with_an_empty_queue_is_clean() {
    let mut fs = fmt_discard(512, 1, 0);
    let sink = RecordingSink::new();
    let report = fs.trim(&GrantAll, &sink).expect("trim");
    assert!(report.supported);
    assert_eq!(report.ranges_discarded, 0);
    assert_eq!(report.blocks_discarded, 0);
    assert!(sink.saw(discard::TRIM_CLEAN));
    assert!(fs.block.discarded.is_empty());
}

#[test]
fn trim_coalesces_contiguous_free_blocks_into_one_range() {
    // Out-of-order, contiguous free blocks become a single discard range.
    let mut fs = fmt_discard(512, 1, 0);
    for block in [105u64, 100, 103, 101, 104, 102] {
        assert!(!fs.bit_used(block));
        fs.enqueue_discard(block);
    }
    let sink = RecordingSink::new();
    let report = fs.trim(&GrantAll, &sink).expect("trim");
    assert_eq!(report.ranges_discarded, 1);
    assert_eq!(report.blocks_discarded, 6);
    assert_eq!(report.blocks_deferred, 0);
    assert_eq!(fs.block.discarded, alloc::vec![(100, 6)]);
    assert_eq!(fs.pending_discard_count(), 0);
    assert!(sink.saw(discard::TRIM_DISCARDED));
}

#[test]
fn trim_aligns_inward_and_requeues_the_unaligned_edges() {
    // A run is aligned inward to the device granularity; the head and tail that
    // fall outside the aligned window are requeued for a later pass (§11).
    let mut fs = fmt_discard(512, 8, 0);
    enqueue_free_range(&mut fs, 100, 130);
    let sink = RecordingSink::new();
    let report = fs.trim(&GrantAll, &sink).expect("trim");
    // align_up(100,8)=104, align_down(130,8)=128 -> discard [104,128).
    assert_eq!(fs.block.discarded, alloc::vec![(104, 24)]);
    assert_eq!(report.blocks_discarded, 24);
    assert_eq!(report.ranges_discarded, 1);
    // Edges [100,104) and [128,130) -> 4 + 2 = 6 deferred and requeued.
    assert_eq!(report.blocks_deferred, 6);
    assert_eq!(fs.pending_discard_count(), 6);
}

#[test]
fn trim_run_shorter_than_one_granularity_window_is_requeued_whole() {
    let mut fs = fmt_discard(512, 8, 0);
    // [100,104): no multiple of 8 lies inside, so nothing can be discarded.
    enqueue_free_range(&mut fs, 100, 104);
    let sink = RecordingSink::new();
    let report = fs.trim(&GrantAll, &sink).expect("trim");
    assert!(fs.block.discarded.is_empty());
    assert_eq!(report.blocks_discarded, 0);
    assert_eq!(report.blocks_deferred, 4);
    assert_eq!(fs.pending_discard_count(), 4);
    assert!(sink.saw(discard::TRIM_CLEAN));
}

#[test]
fn trim_splits_a_run_to_the_per_request_cap() {
    // A run longer than the device's per-request maximum is split into several
    // aligned discards, each within the cap (the MemBlock double asserts it).
    let mut fs = fmt_discard(512, 4, 8);
    enqueue_free_range(&mut fs, 100, 124); // 24 blocks, all multiples-of-4 aligned.
    let sink = RecordingSink::new();
    let report = fs.trim(&GrantAll, &sink).expect("trim");
    assert_eq!(
        fs.block.discarded,
        alloc::vec![(100, 8), (108, 8), (116, 8)]
    );
    assert_eq!(report.ranges_discarded, 3);
    assert_eq!(report.blocks_discarded, 24);
    assert_eq!(report.blocks_deferred, 0);
}

#[test]
fn trim_skips_a_block_that_was_reallocated() {
    // A queued block that is no longer free (reallocated since it was freed) is
    // skipped, never discarded — discard can never touch live data (§11, §14).
    let mut fs = fmt_discard(512, 1, 0);
    assert!(!fs.bit_used(100));
    fs.enqueue_discard(100);
    fs.mark_used(100); // the block is handed back out before trim runs.
    let sink = RecordingSink::new();
    let report = fs.trim(&GrantAll, &sink).expect("trim");
    assert_eq!(report.blocks_skipped_in_use, 1);
    assert_eq!(report.blocks_discarded, 0);
    assert!(
        fs.block.discarded.is_empty(),
        "the live block is never discarded"
    );
}

#[test]
fn trim_rate_limits_to_the_batch_size_and_drains_over_passes() {
    // More distinct runs than the per-call batch limit: the surplus runs stay
    // queued and a second trim pass drains them (§11).
    let mut fs = fmt_discard(2048, 1, 0);
    let runs = discard::TRIM_BATCH_RANGES + 1;
    let mut blocks = alloc::vec::Vec::new();
    for run in 0..runs as u64 {
        let block = 100 + run * 2; // gaps keep every block its own run.
        assert!(!fs.bit_used(block));
        fs.enqueue_discard(block);
        blocks.push(block);
    }
    let sink = RecordingSink::new();
    let first = fs.trim(&GrantAll, &sink).expect("trim pass 1");
    assert_eq!(
        first.ranges_discarded,
        u64::try_from(discard::TRIM_BATCH_RANGES).unwrap()
    );
    assert_eq!(first.blocks_deferred, 1, "one surplus run is requeued");
    assert_eq!(fs.pending_discard_count(), 1);

    let second = fs.trim(&GrantAll, &sink).expect("trim pass 2");
    assert_eq!(second.ranges_discarded, 1, "the remainder drains next pass");
    assert_eq!(fs.pending_discard_count(), 0);
    assert_eq!(
        fs.block.discarded.len(),
        runs,
        "every run is eventually discarded"
    );
}

#[test]
fn mkfs_discards_the_whole_volume_on_a_capable_device() {
    // mkfs tells a discard-capable device the whole volume is free before the
    // encrypted structures are laid down (§11 mkfs flow).
    let block = MemBlock::new(512, 512).with_discard(1, 0);
    let fs = RustFs::format(block, 32, &TEST_KEY, &mut TestEntropy::new()).expect("format");
    assert_eq!(
        fs.into_block().discarded,
        alloc::vec![(0, 512)],
        "the full block range is discarded once at mkfs time"
    );
}

#[test]
fn mkfs_on_a_device_without_discard_still_formats() {
    // A device without discard support is recorded, not failed: format still
    // succeeds and the volume mounts (§11).
    let fs = RustFs::format(
        MemBlock::new(512, 512),
        32,
        &TEST_KEY,
        &mut TestEntropy::new(),
    )
    .expect("format");
    assert!(fs.into_block().discarded.is_empty());
}

#[test]
fn trim_never_discards_a_block_still_shared_by_dedupe() {
    // The §11 hard constraint, end-to-end: a data block shared by two files
    // (dedupe refcount 2) is not freed when one sharer is removed — refcount
    // falls to 1, the block stays reachable — so trim must never discard it and
    // the surviving file must still read back (§11, §14).
    let mut fs = fmt_discard(512, 1, 0);
    let root = fs.root();
    let payload = alloc::vec![0x33u8; 64];
    fs.create(root, b"a", NodeKind::RegularFile)
        .expect("create a");
    fs.create(root, b"b", NodeKind::RegularFile)
        .expect("create b");
    fs.write_at(root, b"a", 0, &payload).expect("write a");
    fs.write_at(root, b"b", 0, &payload).expect("write b");
    let shared = data_block_phys(&mut fs, b"a", 0);
    assert_eq!(
        shared,
        data_block_phys(&mut fs, b"b", 0),
        "identical content dedupes to one physical block"
    );

    fs.block.discarded.clear();
    fs.remove(root, b"a").expect("remove a");
    assert!(
        fs.bit_used(shared),
        "the block b still shares must stay allocated after a is removed"
    );

    let report = fs.trim(&GrantAll, &NullSink).expect("trim");
    assert!(
        !fs.block
            .discarded
            .iter()
            .any(|&(lba, len)| { shared >= lba && shared < lba + len }),
        "a still-shared block is never discarded ({report:?})"
    );
    let node = fs.lookup(root, b"b").expect("b survives");
    assert_eq!(read_all(&mut fs, node, payload.len()), payload);
}

#[test]
fn the_discard_queue_is_transient_across_a_remount() {
    // The pending-discard queue is rebuildable, transient state (§4): a crash
    // before trim runs simply drops it. The volume remounts cleanly, the queue
    // is empty, and no live data is lost.
    let mut fs = fmt_discard(512, 1, 0);
    let root = fs.root();
    let keep = alloc::vec![0x21u8; 80];
    fs.create(root, b"keep", NodeKind::RegularFile)
        .expect("create keep");
    fs.write_at(root, b"keep", 0, &keep).expect("write keep");
    fs.create(root, b"gone", NodeKind::RegularFile)
        .expect("create gone");
    fs.write_at(root, b"gone", 0, &[0x9au8; 80])
        .expect("write gone");
    fs.remove(root, b"gone").expect("remove gone");
    assert!(
        fs.pending_discard_count() > 0,
        "removing a file queued its freed blocks"
    );

    // Simulate a crash before trim: the in-memory queue never reaches disk.
    let bytes = fs.into_block().bytes();
    let mut fs = RustFs::open(MemBlock::from_bytes(bytes, 512, 512), &TEST_KEY).expect("remount");
    assert_eq!(
        fs.pending_discard_count(),
        0,
        "the queue is transient and is not rebuilt on mount"
    );
    let node = fs
        .lookup(fs.root(), b"keep")
        .expect("keep survives the crash");
    assert_eq!(read_all(&mut fs, node, keep.len()), keep);
    assert!(
        fs.lookup(fs.root(), b"gone").is_err(),
        "the removed file stays removed"
    );
}

// ---------------------------------------------------------------------------
// Stage 11: device-health baselines and health-triggered scrub.
// ---------------------------------------------------------------------------

/// A benign device-health snapshot: every failing/degraded counter is clear
/// and spare/wear are healthy, so only the fields a test varies move the
/// classification.
fn healthy_snapshot(unsafe_shutdowns: u64, media_errors: u64) -> HealthSnapshot {
    HealthSnapshot {
        power_on_hours: 100,
        unsafe_shutdowns,
        media_errors,
        reallocated_sectors: 0,
        pending_sectors: 0,
        uncorrectable_sectors: 0,
        crc_errors: 0,
        percentage_used: 0,
        available_spare: 100,
        temperature_kelvin: 300,
        critical_warning: false,
    }
}

/// Format a 4096-byte volume reporting `health`, then return it mounted.
fn fmt_health(block_count: u64, health: DeviceHealth) -> RustFs<MemBlock> {
    RustFs::format(
        MemBlock::new(4096, block_count).with_health(health),
        128,
        &TEST_KEY,
        &mut TestEntropy::new(),
    )
    .expect("format with health")
    .with_clock(fixed_clock)
}

/// Reopen a 4096-byte volume from `bytes`, reporting `health`.
fn open_health(
    bytes: alloc::vec::Vec<u8>,
    block_count: u64,
    health: DeviceHealth,
) -> RustFs<MemBlock> {
    RustFs::open(
        MemBlock::from_bytes(bytes, 4096, block_count).with_health(health),
        &TEST_KEY,
    )
    .expect("reopen with health")
}

#[test]
fn health_requires_the_fs_mount_capability() {
    // Reading health (which may trigger a scrub) is capability-gated like the
    // other privileged FS operations (§13, `AGENTS.md` §5.4): without
    // `CAP_FS_MOUNT` it fails closed and logs the refusal.
    let mut fs = fmt_health(256, DeviceHealth::Unavailable);
    let sink = RecordingSink::new();
    assert_eq!(
        fs.health(&GrantNone, &sink),
        Err(DriverError::PermissionDenied)
    );
    assert!(sink.saw(health::HEALTH_DENIED), "the refusal is logged");
}

#[test]
fn health_on_a_device_without_telemetry_still_classifies_and_persists() {
    // A device that exposes no telemetry still gets a working health
    // subsystem: the pass classifies from the filesystem-observed counters
    // alone, persists a baseline block, and never triggers a scrub it has no
    // signal for (§11; `HealthUnavailable`).
    let mut fs = fmt_health(256, DeviceHealth::Unavailable);
    assert_ne!(
        fs.health_baseline_root, 0,
        "mkfs stored an initial baseline block"
    );
    let report = fs.health(&GrantAll, &NullSink).expect("health");
    assert_eq!(report.state, HealthState::Healthy);
    assert!(report.device.is_none(), "no telemetry");
    assert!(report.scrub.is_none(), "no signal, no scrub");
    assert!(!report.read_only_recommended);

    // The baseline persists across a remount (it is reached from the root).
    let bytes = fs.into_block().bytes();
    let mut fs = open_health(bytes, 256, DeviceHealth::Unavailable);
    assert_ne!(
        fs.health_baseline_root, 0,
        "the baseline survived the remount"
    );
    let again = fs.health(&GrantAll, &NullSink).expect("health again");
    assert_eq!(again.state, HealthState::Healthy);
}

#[test]
fn health_classifies_healthy_degraded_then_failing_as_signals_accumulate() {
    // As the device's media-error count climbs across mounts, the volume's
    // classification crosses the documented thresholds: healthy → degraded →
    // failing (§11; no magic numbers — see `HealthThresholds::DEFAULT`).
    let t = HealthThresholds::DEFAULT;

    let mut fs = fmt_health(256, DeviceHealth::Available(healthy_snapshot(0, 0)));
    let report = fs.health(&GrantAll, &NullSink).expect("health clean");
    assert_eq!(report.state, HealthState::Healthy, "{report:?}");
    let bytes = fs.into_block().bytes();

    // Media errors at the degraded threshold but below failing.
    let degraded_errors = t.degraded_media_errors;
    let mut fs = open_health(
        bytes,
        256,
        DeviceHealth::Available(healthy_snapshot(0, degraded_errors)),
    );
    let report = fs.health(&GrantAll, &NullSink).expect("health degraded");
    assert_eq!(report.state, HealthState::Degraded, "{report:?}");
    assert!(
        report.deep_scrub_recommended,
        "a media-error delta is a deep scrub"
    );
    let bytes = fs.into_block().bytes();

    // Media errors at the failing threshold.
    let mut fs = open_health(
        bytes,
        256,
        DeviceHealth::Available(healthy_snapshot(0, t.failing_media_errors)),
    );
    let report = fs.health(&GrantAll, &NullSink).expect("health failing");
    assert_eq!(report.state, HealthState::Failing, "{report:?}");
    assert!(
        report.read_only_recommended,
        "critical device health recommends a read-only mount"
    );
}

#[test]
fn health_triggers_a_scrub_when_the_device_reports_new_unsafe_shutdowns() {
    // An unsafe-shutdown delta since the last clean baseline schedules a
    // metadata scrub, run through the Stage-8 machinery (§11; `AGENTS.md`
    // §2.2 — no parallel verifier). Once the baseline advances, a pass with no
    // further delta does not re-scrub.
    let mut fs = fmt_health(256, DeviceHealth::Available(healthy_snapshot(0, 0)));
    fs.health(&GrantAll, &NullSink).expect("establish baseline");
    let bytes = fs.into_block().bytes();

    let mut fs = open_health(bytes, 256, DeviceHealth::Available(healthy_snapshot(3, 0)));
    let sink = RecordingSink::new();
    let report = fs.health(&GrantAll, &sink).expect("health");
    assert_eq!(report.unsafe_shutdown_delta, 3);
    assert!(report.metadata_scrub_recommended);
    assert!(report.scrub.is_some(), "the recommendation was acted on");
    assert!(
        sink.saw(health::HEALTH_SCRUB_TRIGGERED),
        "the trigger is logged"
    );
    assert!(
        sink.saw(scrub::SCRUB_CLEAN),
        "the triggered scrub ran and logged its outcome"
    );
    assert_eq!(report.scrubs_triggered, 1);

    // The baseline has advanced to the new snapshot, so a second pass with the
    // same telemetry sees no delta and triggers no scrub.
    let bytes = fs.into_block().bytes();
    let mut fs = open_health(bytes, 256, DeviceHealth::Available(healthy_snapshot(3, 0)));
    let report = fs.health(&GrantAll, &NullSink).expect("health 2");
    assert_eq!(report.unsafe_shutdown_delta, 0);
    assert!(report.scrub.is_none(), "no new delta, no scrub");
    assert_eq!(report.scrubs_triggered, 1, "the lifetime count persisted");
}

#[test]
fn health_baseline_survives_a_crash_during_its_update() {
    // The persisted baseline is updated inside a copy-on-write transaction, so
    // a power loss at any write count during a health pass leaves a mountable
    // volume with no live data lost (§4, §14): either the new baseline
    // committed in full or the previous one remains selected.
    let mut base = fmt_health(256, DeviceHealth::Available(healthy_snapshot(0, 0)));
    let root = base.root();
    base.create(root, b"keep", NodeKind::RegularFile)
        .expect("create keep");
    base.write_at(root, b"keep", 0, b"baseline")
        .expect("write keep");
    base.health(&GrantAll, &NullSink)
        .expect("establish baseline");
    let baseline = base.into_block().bytes();

    for budget in 0..96u32 {
        let mut dev = MemBlock::from_bytes(baseline.clone(), 4096, 256)
            .with_health(DeviceHealth::Available(healthy_snapshot(5, 5)));
        dev.write_budget = Some(budget);
        let mut fs = RustFs::open(dev, &TEST_KEY).expect("baseline opens");
        // The health pass (a scrub plus a baseline commit) may be cut short.
        let _ = fs.health(&GrantAll, &NullSink);
        let bytes = fs.into_block().bytes();

        // Re-open from the (possibly torn) image: it must mount, the file must
        // be intact, and a fresh health pass must still succeed.
        let mut fs = open_health(bytes, 256, DeviceHealth::Available(healthy_snapshot(5, 5)));
        let keep = fs.lookup(fs.root(), b"keep").expect("keep survives");
        let mut buf = [0u8; 8];
        let n = fs.read_at(keep, 0, &mut buf).expect("read keep");
        assert_eq!(
            &buf[..n],
            b"baseline",
            "live data is never lost (budget {budget})"
        );
        assert_ne!(fs.health_baseline_root, 0, "a baseline is always selected");
        fs.health(&GrantAll, &NullSink)
            .expect("health works after the crash");
    }
}

// ---------------------------------------------------------------------------
// Stage 12: the fuzz / crash-replay / corruption-injection suites
// (`docs/src/filesystem/rustfs-spec.md` §15.12, §16; `AGENTS.md` §7 / §19.6).
//
// These are the adversarial superset of the per-stage tests, built on the
// same seams the earlier stages already provide (`AGENTS.md` §2.2): the §8
// block identity + companion mirror, the `DataFault` classes, the
// `verify_everything` scrub/check core, and the `MemBlock` write-budget
// fault-injection. They add no second verifier and no second on-disk decode
// path. (The fuzz harness for every decode path — mount, metadata, directory,
// compression, check, rescue — lives in `tests/fuzz_mount.rs` and the
// `rustos-compress` `fuzz_compress` harness, wired into `cargo xtask fuzz`.)
// ---------------------------------------------------------------------------

/// The crash-replay block geometry: a 4096-byte volume with room for several
/// files, a multi-block file, shared chunks, a reflink, and a subdirectory.
const CRASH_BS: u32 = 4096;
const CRASH_BC: u64 = 256;

/// Immutable witness file content. Every crash-replay trial asserts this file
/// reads back byte-for-byte after the (possibly torn) re-mount: live data is
/// never lost, whatever write count the power loss cut the transaction at
/// (`docs/src/filesystem/rustfs-spec.md` §14).
const CRASH_KEEP: &[u8] = b"keep-content-that-must-never-be-torn-or-lost";

/// Build a committed crash-replay baseline: a witness file (`keep`), a victim
/// to remove, a two-block file to truncate, a reflink source, an empty write
/// target, and a subdirectory — every operation the §16 sweep replays already
/// has its precondition committed.
fn crash_baseline() -> alloc::vec::Vec<u8> {
    let mut fs = RustFs::format(
        MemBlock::new(CRASH_BS, CRASH_BC)
            .with_discard(1, 0)
            .with_health(DeviceHealth::Available(healthy_snapshot(0, 0))),
        64,
        &TEST_KEY,
        &mut TestEntropy::new(),
    )
    .expect("format crash baseline")
    .with_clock(fixed_clock);
    let root = fs.root();
    fs.create(root, b"keep", NodeKind::RegularFile)
        .expect("create keep");
    fs.write_at(root, b"keep", 0, CRASH_KEEP)
        .expect("write keep");
    fs.create(root, b"victim", NodeKind::RegularFile)
        .expect("create victim");
    fs.write_at(root, b"victim", 0, b"victim-data-block")
        .expect("write victim");
    fs.create(root, b"big", NodeKind::RegularFile)
        .expect("create big");
    let big = alloc::vec![0x07u8; CRASH_BS as usize * 2];
    fs.write_at(root, b"big", 0, &big).expect("write big");
    fs.create(root, b"src", NodeKind::RegularFile)
        .expect("create src");
    fs.write_at(root, b"src", 0, b"reflink-source-content")
        .expect("write src");
    fs.create(root, b"target", NodeKind::RegularFile)
        .expect("create target");
    fs.create(root, b"dir", NodeKind::Directory)
        .expect("create dir");
    fs.into_block().bytes()
}

/// Replay one representative transaction at every commit step: cut the device
/// off after each write count, then assert the re-opened volume always mounts
/// on a whole-transaction boundary and never loses the witness file.
///
/// `op` performs the transaction under the write budget; `check` asserts the
/// all-or-nothing post-condition on the re-mounted volume. The shared witness
/// assertion (`keep` reads back intact) runs for every budget before `check`.
fn crash_replay_each_step<Op, Check>(baseline: &[u8], max_budget: u32, mut op: Op, mut check: Check)
where
    Op: FnMut(&mut RustFs<MemBlock>),
    Check: FnMut(&mut RustFs<MemBlock>, u32),
{
    let device = |bytes: alloc::vec::Vec<u8>| {
        MemBlock::from_bytes(bytes, CRASH_BS, CRASH_BC)
            .with_discard(1, 0)
            .with_health(DeviceHealth::Available(healthy_snapshot(0, 0)))
    };
    for budget in 0..max_budget {
        let mut dev = device(baseline.to_vec());
        dev.write_budget = Some(budget);
        let mut fs = RustFs::open(dev, &TEST_KEY)
            .expect("baseline opens")
            .with_clock(fixed_clock);
        op(&mut fs);
        let bytes = fs.into_block().bytes();

        let mut fs = RustFs::open(device(bytes), &TEST_KEY)
            .expect("post-crash mount always succeeds on a whole-txn boundary")
            .with_clock(fixed_clock);
        let keep = fs.lookup(fs.root(), b"keep").expect("keep always survives");
        assert_eq!(
            read_all(&mut fs, keep, CRASH_KEEP.len()),
            CRASH_KEEP,
            "live data lost at budget {budget}"
        );
        check(&mut fs, budget);
    }
}

/// The size of file `name`, or `None` if it no longer exists.
fn file_size(fs: &mut RustFs<MemBlock>, name: &[u8]) -> Option<u64> {
    let node = fs.lookup(fs.root(), name).ok()?;
    Some(fs.node_info(node).expect("node info").size)
}

/// The crash-replay write-budget ceiling: larger than the write count of any
/// single transaction the sweeps replay, so every commit step is covered.
const CRASH_BUDGET: u32 = 200;

#[test]
fn crash_replay_at_every_commit_step_for_create_write_truncate() {
    // §16 "crash replay at every commit step" for the namespace/data
    // transactions: a power loss at every write count must leave a volume that
    // mounts on a whole-transaction boundary, with the operation's effect fully
    // present or fully absent (never torn) and no live data lost (§14).
    let baseline = crash_baseline();

    // create: the new file is either present (empty) or absent — never half-made.
    crash_replay_each_step(
        &baseline,
        CRASH_BUDGET,
        |fs| {
            let root = fs.root();
            let _ = fs.create(root, b"fresh", NodeKind::RegularFile);
        },
        |fs, b| match file_size(fs, b"fresh") {
            None | Some(0) => {}
            Some(other) => panic!("torn create at budget {b}: size {other}"),
        },
    );

    // write: the target file is either empty or holds the whole new payload.
    crash_replay_each_step(
        &baseline,
        CRASH_BUDGET,
        |fs| {
            let root = fs.root();
            let _ = fs.write_at(root, b"target", 0, b"freshdata");
        },
        |fs, b| match file_size(fs, b"target") {
            Some(0) => {}
            Some(9) => {
                let node = fs.lookup(fs.root(), b"target").expect("target");
                assert_eq!(read_all(fs, node, 9), b"freshdata", "torn write at {b}");
            }
            other => panic!("torn write at budget {b}: {other:?}"),
        },
    );

    // truncate: the file keeps either its full length or the truncated length,
    // and the surviving prefix is always intact.
    crash_replay_each_step(
        &baseline,
        CRASH_BUDGET,
        |fs| {
            let root = fs.root();
            let _ = fs.truncate(root, b"big", u64::from(CRASH_BS));
        },
        |fs, b| {
            let size = file_size(fs, b"big").expect("big always survives");
            let full = u64::from(CRASH_BS) * 2;
            assert!(
                size == full || size == u64::from(CRASH_BS),
                "torn truncate at budget {b}: size {size}"
            );
            let node = fs.lookup(fs.root(), b"big").expect("big");
            let prefix = read_all(fs, node, CRASH_BS as usize);
            assert_eq!(
                prefix,
                alloc::vec![0x07u8; CRASH_BS as usize],
                "surviving prefix corrupt at budget {b}"
            );
        },
    );
}

#[test]
fn crash_replay_at_every_commit_step_for_remove_reflink_trim() {
    // Crash replay for the unlink / clone / discard transactions (§14, §16).
    let baseline = crash_baseline();

    // remove: the victim is either fully present (with its content) or gone.
    crash_replay_each_step(
        &baseline,
        CRASH_BUDGET,
        |fs| {
            let root = fs.root();
            let _ = fs.remove(root, b"victim");
        },
        assert_victim_whole_or_gone,
    );

    // reflink: the clone is either absent or a full, identical copy; the
    // source is always intact.
    crash_replay_each_step(
        &baseline,
        CRASH_BUDGET,
        |fs| {
            let root = fs.root();
            let _ = fs.reflink(root, b"src", b"clone");
        },
        |fs, b| {
            let src = fs.lookup(fs.root(), b"src").expect("src always survives");
            assert_eq!(
                read_all(fs, src, b"reflink-source-content".len()),
                b"reflink-source-content",
                "reflink damaged the source at budget {b}"
            );
            if let Ok(clone) = fs.lookup(fs.root(), b"clone") {
                assert_eq!(
                    read_all(fs, clone, b"reflink-source-content".len()),
                    b"reflink-source-content",
                    "torn reflink at budget {b}"
                );
            }
        },
    );

    // trim: free a file then trim within the cut-off budget. The pending-discard
    // queue is transient (§4) and discard never zeroes live data, so the
    // re-mount is always clean and the witness file survives.
    crash_replay_each_step(
        &baseline,
        CRASH_BUDGET,
        |fs| {
            let root = fs.root();
            let _ = fs.remove(root, b"victim");
            let _ = fs.trim(&GrantAll, &NullSink);
        },
        assert_victim_whole_or_gone,
    );
}

#[test]
fn crash_replay_at_every_commit_step_for_maintenance_passes() {
    // Crash replay for the maintenance passes (scrub, check, health): a power
    // loss mid-pass leaves a mountable volume (ordinary recovery never needs
    // them, §14) with no live data lost. The shared witness assertion in
    // `crash_replay_each_step` already covers "no live data lost".
    let baseline = crash_baseline();
    crash_replay_each_step(
        &baseline,
        CRASH_BUDGET,
        |fs| {
            let _ = fs.scrub(&GrantAll, &NullSink, ScrubBudget::Unlimited);
        },
        |_, _| {},
    );
    crash_replay_each_step(
        &baseline,
        CRASH_BUDGET,
        |fs| {
            let _ = fs.check(&GrantAll, &NullSink);
        },
        |_, _| {},
    );
    crash_replay_each_step(
        &baseline,
        CRASH_BUDGET,
        |fs| {
            let _ = fs.health(&GrantAll, &NullSink);
        },
        |_, _| {},
    );
}

/// Assert the `victim` file is either fully present with its committed content
/// or wholly absent — never a torn unlink (`docs/src/filesystem/rustfs-spec.md`
/// §14).
fn assert_victim_whole_or_gone(fs: &mut RustFs<MemBlock>, budget: u32) {
    match fs.lookup(fs.root(), b"victim") {
        Ok(node) => assert_eq!(
            read_all(fs, node, b"victim-data-block".len()),
            b"victim-data-block",
            "torn remove at budget {budget}"
        ),
        Err(DriverError::NotFound) => {}
        Err(e) => panic!("unexpected at budget {budget}: {e:?}"),
    }
}

/// Every on-disk metadata structure class, located by its primary block so the
/// corruption-injection suite can wound one or both physical copies of each.
struct CorruptionTargets {
    txn_root: u64,
    inode_tree: u64,
    extent_tree: u64,
    chunk_tree: u64,
    reverse_ref_tree: u64,
    directory: u64,
    scrub_progress: u64,
    health_baseline: u64,
}

/// Build a richly-populated corruption baseline and capture the primary block
/// of every on-disk structure class, returning the committed image, the
/// targets, and the witness file's content. The baseline carries shared chunks
/// (chunk + reverse-reference trees), a reflink, a subdirectory, a paused scrub
/// (a scrub-progress record), and a health pass (a health-baseline record), so
/// every structure class is live and reachable.
fn corruption_baseline() -> (alloc::vec::Vec<u8>, CorruptionTargets, alloc::vec::Vec<u8>) {
    let mut fs = RustFs::format(
        MemBlock::new(4096, 512).with_health(DeviceHealth::Available(healthy_snapshot(0, 0))),
        128,
        &TEST_KEY,
        &mut TestEntropy::new(),
    )
    .expect("format corruption baseline")
    .with_clock(fixed_clock);
    let root = fs.root();
    let cap = as_usize(fs.data_capacity());

    let keep_body = alloc::vec![0x6Bu8; cap + 50];
    fs.create(root, b"keep", NodeKind::RegularFile)
        .expect("create keep");
    fs.write_at(root, b"keep", 0, &keep_body)
        .expect("write keep");

    let shared = alloc::vec![0x5Au8; cap];
    fs.create(root, b"a", NodeKind::RegularFile)
        .expect("create a");
    fs.create(root, b"b", NodeKind::RegularFile)
        .expect("create b");
    fs.write_at(root, b"a", 0, &shared).expect("write a");
    fs.write_at(root, b"b", 0, &shared).expect("write b");
    fs.reflink(root, b"a", b"aclone").expect("reflink");

    fs.create(root, b"dir", NodeKind::Directory)
        .expect("create dir");
    let dir = fs.lookup(root, b"dir").expect("lookup dir");
    fs.create(dir, b"nested", NodeKind::RegularFile)
        .expect("create nested");
    fs.write_at(dir, b"nested", 0, b"nested file")
        .expect("write nested");

    // A bounded scrub pauses mid-pass, persisting a scrub-progress record.
    let _ = fs.scrub(&GrantAll, &NullSink, ScrubBudget::Inodes(1));
    // A health pass persists a health-baseline record.
    fs.health(&GrantAll, &NullSink).expect("health baseline");

    let a_ino = u32::try_from(fs.lookup(root, b"a").expect("lookup a").raw()).expect("ino");
    let extent_tree = fs.read_inode(a_ino).expect("read a inode").extent_root;
    let root_inode = fs.read_inode(ROOT_INO).expect("root inode");
    let directory = fs.block_ptr(&root_inode, 0).expect("root dir block");

    let targets = CorruptionTargets {
        txn_root: fs.root_phys,
        inode_tree: fs.inode_tree_root,
        extent_tree,
        chunk_tree: fs.chunk_tree_root,
        reverse_ref_tree: fs.reverse_ref_tree_root,
        directory,
        scrub_progress: fs.scrub_progress_root,
        health_baseline: fs.health_baseline_root,
    };
    assert_ne!(targets.extent_tree, 0, "the shared file has an extent tree");
    assert_ne!(targets.chunk_tree, 0, "shared content built a chunk tree");
    assert_ne!(
        targets.reverse_ref_tree, 0,
        "shared content built a reverse-reference tree"
    );
    assert_ne!(targets.scrub_progress, 0, "a scrub-progress record exists");
    assert_ne!(
        targets.health_baseline, 0,
        "a health-baseline record exists"
    );

    (fs.into_block().bytes(), targets, keep_body)
}

/// Flip the first payload byte of the block at `block`, breaking its keyed
/// authenticator. `block + 1` is the companion mirror (`AGENTS.md` §2.2).
fn wound_copy(bytes: &mut [u8], block: u64) {
    let off = as_usize(block) * 4096 + HEADER_LEN;
    bytes[off] ^= 0xff;
}

fn open_corruption(bytes: alloc::vec::Vec<u8>) -> Result<RustFs<MemBlock>, DriverError> {
    RustFs::open(MemBlock::from_bytes(bytes, 4096, 512), &TEST_KEY)
        .map(|fs| fs.with_clock(fixed_clock))
}

#[test]
fn corruption_injection_single_metadata_copy_is_recovered_from_the_companion() {
    // Wound exactly one physical copy of every on-disk metadata structure
    // class. The §8 companion-mirror seam must recover each: the volume mounts,
    // scrub reports nothing unrepairable, check finds the structure sound, and
    // the witness file reads back intact (`docs/src/filesystem/rustfs-spec.md`
    // §8, §12). This is the adversarial superset of the per-stage repair tests.
    let (baseline, t, keep_body) = corruption_baseline();
    let classes: [(&str, u64); 8] = [
        ("txn_root", t.txn_root),
        ("inode_tree", t.inode_tree),
        ("extent_tree", t.extent_tree),
        ("chunk_tree", t.chunk_tree),
        ("reverse_ref_tree", t.reverse_ref_tree),
        ("directory", t.directory),
        ("scrub_progress", t.scrub_progress),
        ("health_baseline", t.health_baseline),
    ];
    for (label, block) in classes {
        let mut bytes = baseline.clone();
        wound_copy(&mut bytes, block); // wound only the primary copy
        let mut fs = open_corruption(bytes)
            .unwrap_or_else(|e| panic!("{label}: single-copy damage must still mount: {e:?}"));

        let scrub = fs
            .scrub(&GrantAll, &NullSink, ScrubBudget::Unlimited)
            .unwrap_or_else(|e| panic!("{label}: scrub: {e:?}"));
        assert_eq!(
            scrub.metadata_unrepairable, 0,
            "{label}: a single bad copy is always repairable ({scrub:?})"
        );
        let check = fs
            .check(&GrantAll, &NullSink)
            .unwrap_or_else(|e| panic!("{label}: check: {e:?}"));
        assert!(check.structure_sound, "{label}: {check:?}");
        fs.health(&GrantAll, &NullSink)
            .unwrap_or_else(|e| panic!("{label}: health: {e:?}"));

        let keep = fs.lookup(fs.root(), b"keep").expect("keep survives");
        assert_eq!(
            read_all(&mut fs, keep, keep_body.len()),
            keep_body,
            "{label}: live data must survive single-copy damage"
        );
    }
}

#[test]
fn corruption_injection_both_copies_of_mount_critical_metadata_never_tears() {
    // Wound *both* physical copies of a structure the mount must read. With
    // neither copy authenticating, the §8 mirror cannot repair the block, so
    // RustFS never trusts the corruption: it either fails the mount closed
    // (`AGENTS.md` §5.4 / §2.9) or, because the superblock ring retains earlier
    // whole transactions, selects an older committed root that does not
    // reference the wounded block and is fully consistent (§14 — a partial or
    // unreadable transaction is ignored, never torn). The minimal-volume strict
    // fail-closed case is `both_metadata_copies_corrupted_fails_closed`; this is
    // its adversarial, multi-generation superset.
    let (baseline, t, keep_body) = corruption_baseline();
    let classes: [(&str, u64); 5] = [
        ("txn_root", t.txn_root),
        ("inode_tree", t.inode_tree),
        ("extent_tree", t.extent_tree),
        ("chunk_tree", t.chunk_tree),
        ("reverse_ref_tree", t.reverse_ref_tree),
    ];
    for (label, block) in classes {
        let mut bytes = baseline.clone();
        wound_copy(&mut bytes, block);
        wound_copy(&mut bytes, block + 1);
        match open_corruption(bytes) {
            // Fail closed: neither this nor any earlier root is usable.
            Err(_) => {}
            // Recovered an earlier whole transaction: it must be sound and
            // still carry the witness file — never torn, never corrupt.
            Ok(mut fs) => {
                let report = fs
                    .check(&GrantAll, &NullSink)
                    .unwrap_or_else(|e| panic!("{label}: check: {e:?}"));
                assert!(
                    report.structure_sound,
                    "{label}: an earlier root must be consistent, not torn ({report:?})"
                );
                let keep = fs.lookup(fs.root(), b"keep").expect("keep survives");
                assert_eq!(
                    read_all(&mut fs, keep, keep_body.len()),
                    keep_body,
                    "{label}: the recovered root still carries the witness file"
                );
            }
        }
    }
}

#[test]
fn corruption_injection_both_copies_of_a_directory_block_are_reported_not_torn() {
    // A directory block is metadata the mount-time free-space walk never reads,
    // so a both-copies-bad directory still mounts — but reading the directory
    // fails closed and scrub records it as unrepairable, never silently
    // dropping or fabricating entries (`docs/src/filesystem/rustfs-spec.md`
    // §8, §12; `AGENTS.md` §2.9).
    let (baseline, t, _keep) = corruption_baseline();
    let mut bytes = baseline;
    wound_copy(&mut bytes, t.directory);
    wound_copy(&mut bytes, t.directory + 1);
    let mut fs = open_corruption(bytes).expect("a damaged directory block still mounts");

    let root = fs.root();
    let mut name = [0u8; 256];
    assert!(
        fs.read_dir(root, 0, &mut name).is_err(),
        "reading a both-copies-bad directory fails closed"
    );
    // The baseline left a paused scrub whose cursor is already past the root
    // inode, so the first (resuming) pass would skip the wounded root
    // directory; let it drain and clear the progress record, then run a fresh
    // full pass that re-verifies every inode and records the unrepairable
    // directory block.
    fs.scrub(&GrantAll, &NullSink, ScrubBudget::Unlimited)
        .expect("resuming scrub drains and completes");
    let report = fs
        .scrub(&GrantAll, &NullSink, ScrubBudget::Unlimited)
        .expect("a fresh scrub records the fault");
    assert!(
        report.metadata_unrepairable >= 1,
        "the both-copies-bad directory is reported unrepairable ({report:?})"
    );
}

#[test]
fn corruption_injection_both_copies_of_a_transient_record_recover_gracefully() {
    // The scrub-progress and health-baseline records are rebuildable, transient
    // metadata (§4): even with both copies bad the volume mounts, a fresh scrub
    // simply restarts, a health pass re-derives from a default baseline, and no
    // live data is lost (`docs/src/filesystem/rustfs-spec.md` §11, §12).
    let (baseline, t, keep_body) = corruption_baseline();
    for (label, block) in [
        ("scrub_progress", t.scrub_progress),
        ("health_baseline", t.health_baseline),
    ] {
        let mut bytes = baseline.clone();
        wound_copy(&mut bytes, block);
        wound_copy(&mut bytes, block + 1);
        let mut fs = open_corruption(bytes)
            .unwrap_or_else(|e| panic!("{label}: a transient record both-bad still mounts: {e:?}"));

        fs.scrub(&GrantAll, &NullSink, ScrubBudget::Unlimited)
            .unwrap_or_else(|e| panic!("{label}: scrub restarts cleanly: {e:?}"));
        fs.health(&GrantAll, &NullSink)
            .unwrap_or_else(|e| panic!("{label}: health re-derives: {e:?}"));

        let keep = fs.lookup(fs.root(), b"keep").expect("keep survives");
        assert_eq!(
            read_all(&mut fs, keep, keep_body.len()),
            keep_body,
            "{label}: live data must survive a corrupt transient record"
        );
    }
}

#[test]
fn corruption_injection_data_block_faults_are_classified_not_repaired() {
    // Data blocks are not mirrored (only metadata is, §8). A wounded data block
    // is therefore detected and classified by its `DataFault` layer, and scrub
    // records the fault rather than repairing it (deep data repair is out of
    // scope, §12); the production read path fails closed
    // (`docs/src/filesystem/rustfs-spec.md` §12; `AGENTS.md` §2.9).
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"f", NodeKind::RegularFile)
        .expect("create");
    fs.write_at(root, b"f", 0, &alloc::vec![0x42u8; 300])
        .expect("write");
    let phys = data_block_phys(&mut fs, b"f", 0);
    let bs = 512usize;
    let base = as_usize(phys) * bs;
    let mut bytes = fs.into_block().bytes();
    bytes[base] ^= 0x01; // wound the at-rest ciphertext

    let mut fs =
        RustFs::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY).expect("still mounts");
    let mut buf = [0u8; MAX_BLOCK_SIZE];
    assert_eq!(
        fs.read_data_block_classified(phys, &mut buf),
        Err(DataFault::Physical),
        "an at-rest data flip is classified as a physical fault"
    );
    let report = scrub_full(&mut fs);
    assert!(
        report.data_physical_faults >= 1,
        "scrub records the data fault ({report:?})"
    );
    assert_eq!(
        report.metadata_repaired, 0,
        "a data fault is never a metadata repair"
    );
    let node = fs.lookup(fs.root(), b"f").expect("file survives");
    let mut out = [0u8; 300];
    assert!(
        matches!(fs.read_at(node, 0, &mut out), Err(DriverError::DeviceFault)),
        "the production read path fails closed on an unrepairable data fault"
    );
}

// ---------------------------------------------------------------------------
// Sparse files: ZERO/Hole extents (`.junie/SPARSE.md`). RustFS represents a
// hole implicitly as a gap between extent mappings (permitted by §2/§3); an
// all-zero logical record is stored as such a hole rather than a physical
// data record, and reads of a hole synthesise zero bytes.
// ---------------------------------------------------------------------------

/// The number of *physical data blocks* file `ino` maps: the sum of its
/// extent-run lengths. A fully sparse range contributes nothing, so a hole
/// costs zero data payload (`.junie/SPARSE.md` §14).
fn mapped_block_count(fs: &mut RustFs<MemBlock>, ino: u32) -> u64 {
    let inode = fs.read_inode(ino).expect("read inode");
    let spec = extent_spec(ino);
    fs.btree_collect_entries(inode.extent_root, spec)
        .expect("walk extent tree")
        .iter()
        .map(|(_, value)| decode_extent(value).1)
        .sum()
}

/// Assert file `ino`'s committed extent map is sorted by logical offset and
/// holds no overlapping runs (`.junie/SPARSE.md` §7).
fn assert_extents_ordered_and_disjoint(fs: &mut RustFs<MemBlock>, ino: u32) {
    let inode = fs.read_inode(ino).expect("read inode");
    let spec = extent_spec(ino);
    let entries = fs
        .btree_collect_entries(inode.extent_root, spec)
        .expect("walk extent tree");
    let mut prev_end = 0u64;
    for (start, value) in entries {
        assert!(start >= prev_end, "extent at {start} overlaps prior run");
        let (_, len) = decode_extent(&value);
        prev_end = start + len;
    }
}

/// Read the whole of file `name` under the root and assert every byte is zero.
fn assert_reads_all_zero(fs: &mut RustFs<MemBlock>, name: &[u8], len: usize) {
    let node = fs.lookup(fs.root(), name).expect("lookup");
    let got = read_all(fs, node, len);
    assert!(got.iter().all(|&b| b == 0), "sparse read must be all zero");
}

#[test]
fn sparse_ten_mib_zero_file_allocates_no_data_payload() {
    // §17.1: a 10 MiB all-zero file has a 10 MiB logical size, maps zero
    // physical data blocks, and reads back as zeroes. The volume is encrypted
    // (`TEST_KEY`), so this also covers §17.10: no plaintext data payload
    // exists for the sparse range.
    let mut fs = fmt(4096, 4096, 256);
    let root = fs.root();
    fs.create(root, b"zero", NodeKind::RegularFile)
        .expect("create");
    let size = 10 * 1024 * 1024usize;
    let zeros = alloc::vec![0u8; size];
    assert_eq!(fs.write_at(root, b"zero", 0, &zeros), Ok(size));

    let ino = file_ino(&mut fs, b"zero");
    assert_eq!(
        mapped_block_count(&mut fs, ino),
        0,
        "an all-zero file allocates no data blocks"
    );
    assert_eq!(
        extent_tree_nodes(&mut fs, ino),
        0,
        "an all-zero file has an empty extent tree"
    );
    let node = fs.lookup(fs.root(), b"zero").expect("lookup");
    assert_eq!(
        fs.node_info(node).expect("info").size,
        size as u64,
        "the logical size is the full 10 MiB"
    );
    assert_reads_all_zero(&mut fs, b"zero", size);

    // Survives a remount unchanged: still all-zero, still no data payload.
    let bytes = fs.into_block().bytes();
    let mut fs = RustFs::open(MemBlock::from_bytes(bytes, 4096, 4096), &TEST_KEY).expect("reopen");
    let ino = file_ino(&mut fs, b"zero");
    assert_eq!(
        mapped_block_count(&mut fs, ino),
        0,
        "still sparse after remount"
    );
    assert_reads_all_zero(&mut fs, b"zero", size);
}

#[test]
fn sparse_write_nonzero_into_hole_splits_around_data() {
    // §17.2: writing non-zero data into the middle of a sparse file leaves the
    // surrounding ranges as holes, the written region reads back correctly, and
    // the extent map stays ordered and non-overlapping.
    let mut fs = fmt(512, 512, 64);
    let root = fs.root();
    fs.create(root, b"f", NodeKind::RegularFile)
        .expect("create");
    let cap = fs.data_capacity();
    let capu = as_usize(cap);

    // A 8-block hole, then a single non-zero block at logical block 4.
    fs.truncate(root, b"f", cap * 8).expect("extend");
    let patch = alloc::vec![0xABu8; capu];
    assert_eq!(fs.write_at(root, b"f", cap * 4, &patch), Ok(capu));

    let ino = file_ino(&mut fs, b"f");
    assert_eq!(
        mapped_block_count(&mut fs, ino),
        1,
        "only the one written block is backed by data"
    );
    assert_extents_ordered_and_disjoint(&mut fs, ino);

    let node = fs.lookup(fs.root(), b"f").expect("lookup");
    let mut got = alloc::vec![0u8; capu];
    fs.read_at(node, cap * 4, &mut got).expect("read data");
    assert_eq!(got, patch, "the written region reads back correctly");
    // Surrounding blocks are still holes reading zero.
    let mut before = alloc::vec![0xFFu8; capu];
    fs.read_at(node, cap * 3, &mut before).expect("read hole");
    assert!(before.iter().all(|&b| b == 0), "block before stays a hole");
    let mut after = alloc::vec![0xFFu8; capu];
    fs.read_at(node, cap * 5, &mut after).expect("read hole");
    assert!(after.iter().all(|&b| b == 0), "block after stays a hole");
}

#[test]
fn sparse_overwrite_data_with_zeroes_frees_only_when_unshared() {
    // §17.3: overwriting existing data with zeroes makes the range read as
    // zero, but a block still referenced by a reflink (a snapshot view) is
    // retained, so the reflink keeps seeing the old data.
    let mut fs = fmt(512, 512, 64);
    let root = fs.root();
    fs.create(root, b"f", NodeKind::RegularFile)
        .expect("create");
    let cap = fs.data_capacity();
    let capu = as_usize(cap);
    let body = alloc::vec![0x33u8; capu];
    assert_eq!(fs.write_at(root, b"f", 0, &body), Ok(capu));

    fs.reflink(root, b"f", b"snap").expect("reflink");

    // Overwrite the single data block with zeroes: it becomes a hole.
    let zeros = alloc::vec![0u8; capu];
    assert_eq!(fs.write_at(root, b"f", 0, &zeros), Ok(capu));

    let f_ino = file_ino(&mut fs, b"f");
    assert_eq!(
        mapped_block_count(&mut fs, f_ino),
        0,
        "the zeroed block is now a hole"
    );
    assert_reads_all_zero(&mut fs, b"f", capu);

    // The reflink still sees the original data: the old chunk was retained
    // because it was still referenced.
    let snap = fs.lookup(fs.root(), b"snap").expect("snap lookup");
    assert_eq!(
        read_all(&mut fs, snap, capu),
        body,
        "the snapshot keeps old data"
    );
}

#[test]
fn sparse_truncate_up_creates_a_hole() {
    // §17.4: growing a file with no written data creates a hole; reads of the
    // new range return zeroes and no data blocks are allocated.
    let mut fs = fmt(512, 256, 64);
    let root = fs.root();
    fs.create(root, b"f", NodeKind::RegularFile)
        .expect("create");
    let cap = fs.data_capacity();
    fs.truncate(root, b"f", cap * 6).expect("truncate up");

    let ino = file_ino(&mut fs, b"f");
    assert_eq!(
        mapped_block_count(&mut fs, ino),
        0,
        "extending into a hole allocates no data"
    );
    let node = fs.lookup(fs.root(), b"f").expect("lookup");
    assert_eq!(fs.node_info(node).expect("info").size, cap * 6);
    assert_reads_all_zero(&mut fs, b"f", as_usize(cap * 6));
}

#[test]
fn sparse_truncate_down_frees_data_but_not_holes() {
    // §17.5: shrinking frees data extents beyond the new EOF through the normal
    // path, while removed holes need no physical free. A file that is data
    // followed by a hole shrinks correctly either way.
    let mut fs = fmt(512, 512, 64);
    let root = fs.root();
    fs.create(root, b"f", NodeKind::RegularFile)
        .expect("create");
    let cap = fs.data_capacity();
    let capu = as_usize(cap);
    // Two *distinct* data blocks (so dedupe does not collapse them to one
    // shared chunk) then extend with a hole.
    let mut body = alloc::vec![0x44u8; capu * 2];
    for byte in &mut body[capu..] {
        *byte = 0x45;
    }
    assert_eq!(fs.write_at(root, b"f", 0, &body), Ok(capu * 2));
    fs.truncate(root, b"f", cap * 8).expect("extend with hole");

    let ino = file_ino(&mut fs, b"f");
    assert_eq!(
        mapped_block_count(&mut fs, ino),
        2,
        "two data blocks mapped"
    );
    let free_before = fs.free_count;

    // Shrink to drop the hole only: data blocks remain, no free needed.
    fs.truncate(root, b"f", cap * 2).expect("drop hole");
    assert_eq!(
        mapped_block_count(&mut fs, ino),
        2,
        "data survives the hole drop"
    );

    // Shrink into the data: the freed data block returns to the free pool.
    fs.truncate(root, b"f", cap).expect("drop a data block");
    assert_eq!(mapped_block_count(&mut fs, ino), 1, "one data block freed");
    assert!(
        fs.free_count > free_before,
        "freeing a data block returns it to the free pool"
    );
}

#[test]
fn sparse_reflink_preserves_holes_without_dedupe_chunks() {
    // §17.6: cloning a sparse file keeps its holes metadata-only and creates no
    // dedupe chunk for any zero range (§8 — a zero range is never a chunk).
    let mut fs = fmt(512, 512, 64);
    let root = fs.root();
    fs.create(root, b"f", NodeKind::RegularFile)
        .expect("create");
    let cap = fs.data_capacity();
    let capu = as_usize(cap);
    // Distinct data at block 0 and block 5 (so they are two separate chunks,
    // not one deduped chunk), a hole between, then a trailing hole.
    let patch0 = alloc::vec![0x6Cu8; capu];
    let patch5 = alloc::vec![0x6Du8; capu];
    assert_eq!(fs.write_at(root, b"f", 0, &patch0), Ok(capu));
    assert_eq!(fs.write_at(root, b"f", cap * 5, &patch5), Ok(capu));
    fs.truncate(root, b"f", cap * 8).expect("trailing hole");

    let chunks_before = chunk_count(&mut fs);
    fs.reflink(root, b"f", b"clone").expect("reflink");

    let src = file_ino(&mut fs, b"f");
    let clone = file_ino(&mut fs, b"clone");
    assert_eq!(
        mapped_block_count(&mut fs, src),
        mapped_block_count(&mut fs, clone),
        "the clone maps exactly the source's data blocks"
    );
    assert_eq!(
        mapped_block_count(&mut fs, clone),
        2,
        "only the two written blocks are backed; holes stay metadata-only"
    );
    // Reflink shares the two real data blocks (so chunk_count grows by 2), but
    // never invents a chunk for a zero range.
    assert_eq!(
        chunk_count(&mut fs) - chunks_before,
        2,
        "a reflink shares only the real data blocks, never a zero range"
    );

    // Both files read identically, holes included.
    let node = fs.lookup(fs.root(), b"clone").expect("clone lookup");
    let mut got = alloc::vec![0xFFu8; capu];
    fs.read_at(node, cap, &mut got).expect("read hole");
    assert!(got.iter().all(|&b| b == 0), "the clone's hole reads zero");
}

#[test]
fn sparse_scrub_and_check_validate_metadata_only() {
    // §17.7 / §17.8: scrub and check both pass on a sparse file. Because a hole
    // has no extent record, no physical read is attempted for it and there is
    // nothing for the integrity layers to fault on.
    let mut fs = fmt(4096, 512, 128);
    let root = fs.root();
    fs.create(root, b"sparse", NodeKind::RegularFile)
        .expect("create");
    let cap = fs.data_capacity();
    let capu = as_usize(cap);
    let patch = alloc::vec![0x77u8; capu];
    assert_eq!(fs.write_at(root, b"sparse", cap * 3, &patch), Ok(capu));
    fs.truncate(root, b"sparse", cap * 16)
        .expect("trailing hole");

    let report = scrub_full(&mut fs);
    assert!(report.complete, "scrub completes on a sparse file");
    assert!(!report.found_faults(), "a sparse file is clean: {report:?}");

    let check = fs.check(&GrantAll, &NullSink).expect("check");
    assert!(
        check.structure_sound,
        "sparse metadata validates: {check:?}"
    );

    // The data still reads back correctly around its holes.
    let node = fs.lookup(fs.root(), b"sparse").expect("lookup");
    let mut got = alloc::vec![0u8; capu];
    fs.read_at(node, cap * 3, &mut got).expect("read data");
    assert_eq!(got, patch);
}

#[test]
fn sparse_all_zero_bypasses_compression_but_nonzero_constant_compresses() {
    // §17.9: an all-zero record produces no zstd payload (it is a hole with no
    // physical block at all), whereas a repeated *non-zero* constant follows
    // the normal compression path and is backed by a compressed data record.
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    let cap = as_usize(fs.data_capacity());

    fs.create(root, b"zero", NodeKind::RegularFile)
        .expect("create zero");
    let zeros = alloc::vec![0u8; cap];
    assert_eq!(fs.write_at(root, b"zero", 0, &zeros), Ok(cap));
    let zero_ino = file_ino(&mut fs, b"zero");
    assert_eq!(
        mapped_block_count(&mut fs, zero_ino),
        0,
        "an all-zero record creates no physical (zstd or raw) payload"
    );

    fs.create(root, b"ff", NodeKind::RegularFile)
        .expect("create ff");
    let ff = alloc::vec![0xFFu8; cap];
    assert_eq!(fs.write_at(root, b"ff", 0, &ff), Ok(cap));
    assert!(
        stored_compression(&mut fs, b"ff", 0).compressed,
        "a repeated non-zero constant compresses through the normal path"
    );
    let node = fs.lookup(fs.root(), b"ff").expect("lookup ff");
    assert_eq!(
        read_all(&mut fs, node, cap),
        ff,
        "the constant block round-trips"
    );
}

#[test]
fn names_up_to_255_bytes_round_trip() {
    // §13: the maximum name length matches ext4 (255 bytes). A maximum-length
    // name is creatable, lookable, enumerable, and survives a remount.
    let mut fs = fmt(4096, 256, 64);
    let root = fs.root();
    let name = alloc::vec![b'n'; NAME_MAX];
    assert_eq!(name.len(), 255);
    fs.create(root, &name, NodeKind::RegularFile)
        .expect("create a 255-byte name");
    let body = alloc::vec![0x9u8; 1000];
    assert_eq!(fs.write_at(root, &name, 0, &body), Ok(1000));

    let mut name_out = alloc::vec![0u8; NAME_MAX];
    let entry = fs
        .read_dir(root, 0, &mut name_out)
        .expect("read_dir")
        .expect("one entry");
    assert_eq!(entry.name_len, NAME_MAX);
    assert_eq!(&name_out[..entry.name_len], &name[..]);

    let bytes = fs.into_block().bytes();
    let mut reopened =
        RustFs::open(MemBlock::from_bytes(bytes, 4096, 256), &TEST_KEY).expect("reopen");
    let node = reopened
        .lookup(reopened.root(), &name)
        .expect("lookup after remount");
    assert_eq!(read_all(&mut reopened, node, 1000), body);
}

#[test]
fn a_name_longer_than_255_bytes_is_rejected() {
    let mut fs = fmt(4096, 256, 64);
    let root = fs.root();
    let too_long = alloc::vec![b'x'; NAME_MAX + 1];
    assert_eq!(
        fs.create(root, &too_long, NodeKind::RegularFile),
        Err(DriverError::LengthOutOfRange)
    );
}

#[test]
fn names_use_the_ext4_charset_rejecting_only_slash_and_nul() {
    // §13: RustFS allows every byte ext4 allows in a name — anything except `/`
    // and NUL — and reserves only `.` and `..`.
    let mut fs = fmt(4096, 256, 64);
    let root = fs.root();
    let legal: [&[u8]; 7] = [
        b"hello world",
        b"file.name.tar.gz",
        b".hidden",
        b"..prefixed",
        b"caf\xc3\xa9",        // UTF-8
        &[0x01u8, 0x1f, b'a'], // control bytes other than NUL
        b"\xff\xfe",           // arbitrary high bytes
    ];
    for name in legal {
        fs.create(root, name, NodeKind::RegularFile)
            .unwrap_or_else(|e| panic!("ext4-legal name {name:?} rejected: {e:?}"));
        assert!(fs.lookup(root, name).is_ok(), "{name:?} must be present");
    }
    // The two forbidden bytes and the two reserved names.
    assert_eq!(
        fs.create(root, b"a/b", NodeKind::RegularFile),
        Err(DriverError::Unsupported)
    );
    assert_eq!(
        fs.create(root, &[b'a', 0u8, b'b'], NodeKind::RegularFile),
        Err(DriverError::Unsupported)
    );
    assert_eq!(
        fs.create(root, b".", NodeKind::RegularFile),
        Err(DriverError::Unsupported)
    );
    assert_eq!(
        fs.create(root, b"..", NodeKind::RegularFile),
        Err(DriverError::Unsupported)
    );
}

#[test]
fn names_are_case_sensitive() {
    // §13: names are compared byte-for-byte, so casing distinguishes entries.
    let mut fs = fmt(4096, 256, 64);
    let root = fs.root();
    fs.create(root, b"File", NodeKind::RegularFile)
        .expect("File");
    fs.create(root, b"file", NodeKind::RegularFile)
        .expect("file");
    fs.create(root, b"FILE", NodeKind::RegularFile)
        .expect("FILE");
    let a = fs.lookup(root, b"File").expect("File");
    let b = fs.lookup(root, b"file").expect("file");
    let c = fs.lookup(root, b"FILE").expect("FILE");
    assert_ne!(a, b);
    assert_ne!(a, c);
    assert_ne!(b, c);
    assert_eq!(fs.lookup(root, b"fIle"), Err(DriverError::NotFound));
}

#[test]
fn directory_entries_span_multiple_blocks_on_a_512_byte_volume() {
    // A 512-byte block holds a single 263-byte dirent slot, so a directory with
    // several entries spans several blocks. Every entry stays retrievable and
    // enumerable, and the layout survives a remount.
    let mut fs = fmt(512, 256, 64);
    let root = fs.root();
    let names: [&[u8]; 6] = [
        b"alpha", b"bravo", b"charlie", b"delta", b"echo", b"foxtrot",
    ];
    for n in names {
        fs.create(root, n, NodeKind::RegularFile).expect("create");
    }
    for n in names {
        assert!(fs.lookup(root, n).is_ok(), "{n:?} must be present");
    }
    let mut seen = 0u64;
    let mut idx = 0u64;
    let mut buf = alloc::vec![0u8; NAME_MAX];
    while fs
        .read_dir(root, idx, &mut buf)
        .expect("read_dir")
        .is_some()
    {
        seen += 1;
        idx += 1;
    }
    assert_eq!(seen, names.len() as u64, "every entry enumerates back");
}

/// Allocate files of one data block each until the volume is full.
fn fill_until_no_space(fs: &mut RustFs<MemBlock>) {
    let root = fs.root();
    let body = alloc::vec![0x42u8; 256];
    let mut idx = 0u64;
    loop {
        let name = alloc::format!("fill{idx}");
        if fs
            .create(root, name.as_bytes(), NodeKind::RegularFile)
            .is_err()
        {
            break;
        }
        match fs.write_at(root, name.as_bytes(), 0, &body) {
            Ok(_) => idx += 1,
            Err(DriverError::NoSpace) => break,
            Err(e) => panic!("unexpected {e:?}"),
        }
        assert!(idx < 100_000, "the small volume must fill");
    }
}

#[test]
fn grow_extends_a_mounted_volume_online() {
    // A 256-block volume is grown to fill a device enlarged to 1024 blocks
    // underneath the live mount. No remount is needed and existing data is
    // intact; the larger size is durable.
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"keep", NodeKind::RegularFile)
        .expect("create");
    let body = alloc::vec![0x33u8; 4096];
    assert_eq!(fs.write_at(root, b"keep", 0, &body), Ok(4096));
    assert_eq!(fs.total_blocks, 256);
    fill_until_no_space(&mut fs);

    // The device is enlarged underneath the mount; grow folds in the new tail.
    fs.block_mut().enlarge_to(1024);
    let added = fs.grow().expect("grow");
    assert_eq!(added, 1024 - 256);
    assert_eq!(fs.total_blocks, 1024);

    // The grown space is immediately usable, without a remount.
    fs.create(root, b"after_grow", NodeKind::RegularFile)
        .expect("create after grow");
    assert_eq!(
        fs.write_at(root, b"after_grow", 0, &body),
        Ok(4096),
        "the new blocks are allocatable"
    );

    // Original data is intact and the larger size survives a remount.
    let node = fs.lookup(root, b"keep").expect("keep lookup");
    assert_eq!(read_all(&mut fs, node, 4096), body);
    let bytes = fs.into_block().bytes();
    let mut reopened =
        RustFs::open(MemBlock::from_bytes(bytes, 512, 1024), &TEST_KEY).expect("reopen");
    assert_eq!(reopened.total_blocks, 1024, "the grown size is durable");
    let node = reopened.lookup(reopened.root(), b"keep").expect("lookup");
    assert_eq!(read_all(&mut reopened, node, 4096), body);
    // Already spanning the whole device: a further grow is a no-op.
    assert_eq!(reopened.grow(), Ok(0));
}

#[test]
fn grow_is_a_noop_when_the_device_has_not_grown() {
    let mut fs = fmt(512, 256, 32);
    assert_eq!(fs.total_blocks, 256);
    assert_eq!(fs.grow(), Ok(0), "no extra device space means no growth");
    assert_eq!(fs.total_blocks, 256);
}

#[test]
fn grow_refuses_an_online_shrink() {
    // If the device reports fewer blocks than the committed filesystem size,
    // grow refuses rather than truncating live data (fail-closed).
    let mut fs = fmt(512, 256, 32);
    fs.block.block_count = 128;
    assert_eq!(fs.grow(), Err(DriverError::Unsupported));
    assert_eq!(fs.total_blocks, 256, "the committed size is untouched");
}

#[test]
fn open_rejects_a_committed_size_larger_than_the_device() {
    // A volume formatted for 256 blocks presented on a device that now claims
    // only 200 blocks (a truncated device) refuses to mount.
    let fs = fmt(512, 256, 32);
    let bytes = fs.into_block().bytes();
    let truncated = MemBlock::from_bytes(bytes, 512, 200);
    assert!(matches!(
        RustFs::open(truncated, &TEST_KEY),
        Err(DriverError::BadMagic)
    ));
}

#[test]
fn a_volume_smaller_than_the_device_mounts_and_leaves_the_tail_unused() {
    // Format a 256-block volume, then present the same image on a larger
    // (1024-block) device. It mounts at its committed size; the surplus tail is
    // simply unused until a grow.
    let fs = fmt(512, 256, 32);
    let mut bytes = fs.into_block().bytes();
    bytes.resize(512 * 1024, 0);
    let mut reopened =
        RustFs::open(MemBlock::from_bytes(bytes, 512, 1024), &TEST_KEY).expect("reopen larger");
    assert_eq!(reopened.total_blocks, 256, "mounts at the committed size");
    let added = reopened.grow().expect("grow into the surplus");
    assert_eq!(added, 1024 - 256);
    assert_eq!(reopened.total_blocks, 1024);
}
