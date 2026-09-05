//! Unit tests for the copy-on-write arxfs driver.
//!
//! They exercise the Stage-1 foundation — format → open round-trip, the
//! superblock-ring selection, and crash replay at every write count during a
//! commit — plus the full read/write surface the VFS consumes, all over an
//! in-memory [`MemBlock`] double.

use alloc::collections::BTreeSet;

use super::*;
use crate::pagecache::MAX_CACHED_PAGES;
use crate::wcache::TestWritebackHost;
use tairix_abi::driver::block::{DeviceHealth, DiscardCapability, HealthSnapshot};
use tairix_abi::driver::filesystem::{
    FilesystemAttrs, FilesystemRead, FilesystemSecurity, FilesystemWrite, NodeKind,
};
use tairix_fsmeta::preset;
use tairix_reclaim::{PressureBand, ReportedPressure};

/// In-memory block device. Optionally drops writes once a budget is reached,
/// modelling a power loss mid-commit: a dropped write simply never reaches the
/// platter, and the driver's in-memory state is discarded by re-opening from
/// the stored bytes. It can also fail the *read* or the *write* of named
/// blocks, modelling the single-sector media error a mirrored metadata copy
/// exists for, and hold accepted writes in a volatile cache until a barrier
/// commits them, modelling the reordering every SD card, consumer SSD, and
/// HDD is free to perform.
struct MemBlock {
    store: alloc::vec::Vec<u8>,
    block_size: u32,
    block_count: u64,
    writes: u32,
    write_budget: Option<u32>,
    /// Blocks whose reads fail with a device fault, modelling unreadable
    /// media at a specific LBA.
    read_faults: BTreeSet<u64>,
    /// Blocks whose writes fail with a device fault, modelling media the
    /// device cannot program at a specific LBA.
    write_faults: BTreeSet<u64>,
    /// Once this many writes have been accepted, every later one faults —
    /// a device that starts refusing partway through a transfer, wherever the
    /// filesystem happens to have placed its blocks.
    write_fault_after: Option<u32>,
    /// When set, every barrier fails, modelling a device that cannot confirm
    /// its cache reached media.
    fail_flush: bool,
    /// Accepted writes not yet on stable media. `None` models a device with no
    /// volatile cache — every accepted write is durable at once, which is what
    /// every other test wants. `Some` models one that has a cache: a
    /// [`Block::flush`] commits it, and [`MemBlock::power_loss`] commits an
    /// arbitrary subset and drops the rest.
    volatile: Option<alloc::collections::BTreeMap<u64, alloc::vec::Vec<u8>>>,
    discard: Option<DiscardCapability>,
    discarded: alloc::vec::Vec<(u64, u64)>,
    health: DeviceHealth,
    /// Lowest stack address seen by a device call, or `usize::MAX` before any.
    /// Every filesystem operation reaches the device at the bottom of its call
    /// chain, so this is how deep the operation's stack actually went.
    stack_floor: usize,
}

impl MemBlock {
    fn new(block_size: u32, block_count: u64) -> Self {
        publish_boot_hash_key();
        let len = block_size as usize * as_usize(block_count);
        Self {
            store: alloc::vec![0u8; len],
            block_size,
            block_count,
            writes: 0,
            write_budget: None,
            read_faults: BTreeSet::new(),
            write_faults: BTreeSet::new(),
            write_fault_after: None,
            fail_flush: false,
            volatile: None,
            discard: None,
            discarded: alloc::vec::Vec::new(),
            health: DeviceHealth::Unavailable,
            stack_floor: usize::MAX,
        }
    }

    fn from_bytes(bytes: alloc::vec::Vec<u8>, block_size: u32, block_count: u64) -> Self {
        publish_boot_hash_key();
        Self {
            store: bytes,
            block_size,
            block_count,
            writes: 0,
            write_budget: None,
            read_faults: BTreeSet::new(),
            write_faults: BTreeSet::new(),
            write_fault_after: None,
            fail_flush: false,
            volatile: None,
            discard: None,
            discarded: alloc::vec::Vec::new(),
            health: DeviceHealth::Unavailable,
            stack_floor: usize::MAX,
        }
    }

    /// Make every read of `lba` fail, so the driver has to recover the block
    /// from its companion mirror rather than from an authentication failure.
    fn fail_reads_of(mut self, lba: u64) -> Self {
        self.read_faults.insert(lba);
        self
    }

    /// Hold accepted writes in a volatile cache until a barrier commits them.
    fn with_volatile_cache(mut self) -> Self {
        self.volatile = Some(alloc::collections::BTreeMap::new());
        self
    }

    /// Blocks the device has accepted but not yet committed to media.
    fn volatile_blocks(&self) -> alloc::vec::Vec<u64> {
        self.volatile
            .as_ref()
            .map(|held| held.keys().copied().collect())
            .unwrap_or_default()
    }

    /// Cut the power: commit only the volatile blocks `keep` accepts and drop
    /// the rest, which is exactly the freedom a device with a write cache has
    /// between barriers.
    fn power_loss(&mut self, keep: impl Fn(u64) -> bool) {
        let Some(held) = self.volatile.take() else {
            return;
        };
        for (lba, bytes) in held {
            if keep(lba) {
                self.commit_block(lba, &bytes);
            }
        }
        self.volatile = Some(alloc::collections::BTreeMap::new());
    }

    /// Put one block's bytes on media.
    fn commit_block(&mut self, lba: u64, bytes: &[u8]) {
        let start = as_usize(lba) * self.block_size as usize;
        let end = start + bytes.len();
        if end <= self.store.len() {
            self.store[start..end].copy_from_slice(bytes);
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
    /// exercise online [`ARXFS::grow`].
    /// Note how deep the stack is at this device call. `#[inline(never)]`
    /// keeps this frame's own size out of the comparison between calls.
    #[inline(never)]
    fn note_stack(&mut self) {
        let here = 0u8;
        let sp = core::ptr::addr_of!(here) as usize;
        self.stack_floor = self.stack_floor.min(sp);
    }

    /// Start a fresh stack measurement.
    fn arm_stack(&mut self) {
        self.stack_floor = usize::MAX;
    }

    /// Bytes of stack the measured operation used below `base`, the address of
    /// a local in the frame that started it.
    fn stack_used(&self, base: usize) -> usize {
        assert_ne!(
            self.stack_floor,
            usize::MAX,
            "the measured operation never reached the device"
        );
        base.abs_diff(self.stack_floor)
    }

    fn enlarge_to(&mut self, new_block_count: u64) {
        assert!(new_block_count >= self.block_count, "enlarge cannot shrink");
        self.store
            .resize(self.block_size as usize * as_usize(new_block_count), 0);
        self.block_count = new_block_count;
    }
}

impl Block for MemBlock {
    fn geometry(&self) -> Result<tairix_abi::driver::block::BlockGeometry, DriverError> {
        Ok(tairix_abi::driver::block::BlockGeometry {
            block_size: self.block_size,
            block_count: self.block_count,
        })
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        self.note_stack();
        let bs = self.block_size as usize;
        if buf.is_empty() || !buf.len().is_multiple_of(bs) {
            return Err(DriverError::BufferTooSmall);
        }
        let span = (buf.len() / bs) as u64;
        if (lba..lba + span).any(|b| self.read_faults.contains(&b)) {
            return Err(DriverError::DeviceFault);
        }
        let start = as_usize(lba) * bs;
        let end = start + buf.len();
        if end > self.store.len() {
            return Err(DriverError::LengthOutOfRange);
        }
        buf.copy_from_slice(&self.store[start..end]);
        // A device serves reads from its own write cache, so an uncommitted
        // block reads back as written, not as the media still holds it.
        if let Some(held) = self.volatile.as_ref() {
            for (index, chunk) in buf.chunks_mut(bs).enumerate() {
                let at = lba + index as u64;
                if let Some(bytes) = held.get(&at) {
                    chunk.copy_from_slice(bytes);
                }
            }
        }
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        self.note_stack();
        let bs = self.block_size as usize;
        if buf.is_empty() || !buf.len().is_multiple_of(bs) {
            return Err(DriverError::BufferTooSmall);
        }
        let span = (buf.len() / bs) as u64;
        if (lba..lba + span).any(|b| self.write_faults.contains(&b))
            || matches!(self.write_fault_after, Some(n) if self.writes >= n)
        {
            return Err(DriverError::DeviceFault);
        }
        let start = as_usize(lba) * bs;
        let end = start + buf.len();
        if end > self.store.len() {
            return Err(DriverError::LengthOutOfRange);
        }
        // Model power loss: once the budget is spent, the write never lands.
        let drop_write = matches!(self.write_budget, Some(b) if self.writes >= b);
        self.writes += 1;
        if drop_write {
            return Ok(());
        }
        match self.volatile.as_mut() {
            Some(held) => {
                for (index, chunk) in buf.chunks(bs).enumerate() {
                    held.insert(lba + index as u64, chunk.to_vec());
                }
            }
            None => self.store[start..end].copy_from_slice(buf),
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

    fn flush(&mut self) -> Result<(), DriverError> {
        if self.fail_flush {
            return Err(DriverError::DeviceFault);
        }
        // The barrier: everything the device has accepted so far reaches media
        // before anything accepted after it can.
        self.power_loss(|_| true);
        Ok(())
    }
}

/// A *sparse* in-memory block device that reports a huge logical block count
/// but stores only the blocks actually written, keyed by block index in a
/// `BTreeMap`. It lets a test model a multi-terabyte volume without allocating
/// a multi-terabyte backing store: an unwritten block reads back as zeroes,
/// exactly as a freshly provisioned device would. The resident footprint
/// equals the working set (the handful of metadata blocks `format`/commit
/// touch), not the device size, so it proves the driver's own in-RAM state is
/// likewise working-set-bounded.
struct SparseBlock {
    blocks: alloc::collections::BTreeMap<u64, alloc::vec::Vec<u8>>,
    block_size: u32,
    block_count: u64,
    discard: Option<DiscardCapability>,
}

impl SparseBlock {
    fn new(block_size: u32, block_count: u64) -> Self {
        Self {
            blocks: alloc::collections::BTreeMap::new(),
            block_size,
            block_count,
            discard: None,
        }
    }

    fn with_discard(mut self, granularity_blocks: u64, max_blocks_per_request: u64) -> Self {
        self.discard = Some(DiscardCapability {
            supported: true,
            granularity_blocks,
            max_blocks_per_request,
        });
        self
    }

    /// Number of distinct blocks physically stored — the device's resident
    /// footprint, which a test asserts stays small on a huge volume.
    fn stored_blocks(&self) -> usize {
        self.blocks.len()
    }
}

impl Block for SparseBlock {
    fn geometry(&self) -> Result<tairix_abi::driver::block::BlockGeometry, DriverError> {
        Ok(tairix_abi::driver::block::BlockGeometry {
            block_size: self.block_size,
            block_count: self.block_count,
        })
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        let bs = self.block_size as usize;
        if buf.is_empty() || !buf.len().is_multiple_of(bs) {
            return Err(DriverError::BufferTooSmall);
        }
        let count = (buf.len() / bs) as u64;
        if lba.saturating_add(count) > self.block_count {
            return Err(DriverError::LengthOutOfRange);
        }
        for (i, chunk) in buf.chunks_mut(bs).enumerate() {
            match self.blocks.get(&(lba + i as u64)) {
                Some(stored) => chunk.copy_from_slice(stored),
                None => chunk.fill(0),
            }
        }
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        let bs = self.block_size as usize;
        if buf.is_empty() || !buf.len().is_multiple_of(bs) {
            return Err(DriverError::BufferTooSmall);
        }
        let count = (buf.len() / bs) as u64;
        if lba.saturating_add(count) > self.block_count {
            return Err(DriverError::LengthOutOfRange);
        }
        for (i, chunk) in buf.chunks(bs).enumerate() {
            self.blocks.insert(lba + i as u64, chunk.to_vec());
        }
        Ok(())
    }

    fn discard_capability(&self) -> Result<DiscardCapability, DriverError> {
        Ok(self.discard.unwrap_or_else(DiscardCapability::unsupported))
    }

    fn device_health(&self) -> Result<DeviceHealth, DriverError> {
        Ok(DeviceHealth::Unavailable)
    }

    fn discard(&mut self, lba: u64, blocks: u64) -> Result<(), DriverError> {
        self.discard.ok_or(DriverError::Unsupported)?;
        if lba.saturating_add(blocks) > self.block_count {
            return Err(DriverError::LengthOutOfRange);
        }
        for b in lba..lba + blocks {
            self.blocks.remove(&b);
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        Ok(())
    }
}

/// A clean "100 TiB" volume on a [`SparseBlock`]: `100 * 2^28` 4 KiB blocks
/// (≈ 26.8 billion blocks). Formatting it touches only a handful of metadata
/// blocks, so both the device and the driver stay working-set-bounded.
const HUGE_BLOCK_COUNT: u64 = 100 * (1 << 28);

fn fmt_huge() -> ARXFS<SparseBlock> {
    ARXFS::format(
        SparseBlock::new(4096, HUGE_BLOCK_COUNT).with_discard(1, 0),
        128,
        &TEST_KEY,
        &mut TestEntropy::new(),
    )
    .expect("format a 100 TiB sparse device")
    .with_clock(fixed_clock)
}

fn fixed_clock() -> Time64 {
    Time64::from_secs(1_700_000_000)
}

/// The volume key every test formats and reopens with. `ARXFS` has no
/// plaintext mode (`docs/src/filesystem/arxfs-spec.md` §5), so every test
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

/// Stand in for the kernel's per-boot publication, so the dedupe index is
/// keyed here exactly as it is on a booted system. A second call is refused
/// and ignored: the key is published once per process, as it is once per boot.
fn publish_boot_hash_key() {
    let _ = tairix_hash::publish(tairix_hash::HashSeed::from_words(
        0x4152_5846_5300_0001,
        0x4152_5846_5300_0002,
    ));
}

fn fmt(block_size: u32, block_count: u64, inodes: u32) -> ARXFS<MemBlock> {
    publish_boot_hash_key();
    ARXFS::format(
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
    let bytes = fs.into_block().expect("the volume closes").bytes();
    let reopened = ARXFS::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY).expect("reopen");
    assert_eq!(reopened.root(), NodeId::from_raw(1));
}

#[test]
fn open_read_only_reads_back_committed_content() {
    // Author a volume, then mount it read-only and prove its content reads
    // back through the ordinary read path (the design-B `/System` mount).
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"file", NodeKind::RegularFile)
        .expect("create");
    let body = alloc::vec![0x5Cu8; 1500];
    assert_eq!(fs.write_at(root, b"file", 0, &body), Ok(1500));
    let bytes = fs.into_block().expect("the volume closes").bytes();

    let mut ro = ARXFS::open_read_only(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY)
        .expect("read-only mount");
    let node = ro.lookup(ro.root(), b"file").expect("lookup on RO mount");
    let mut back = alloc::vec![0u8; 1500];
    assert_eq!(ro.read_at(node, 0, &mut back), Ok(1500));
    assert_eq!(back, body);
}

#[test]
fn a_read_only_mount_refuses_every_mutation_fail_closed() {
    // Author a real file, then prove a read-only mount refuses every
    // mutator fail-closed — never a panic — and
    // leaves the existing content intact.
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"file", NodeKind::RegularFile)
        .expect("create");
    assert_eq!(fs.write_at(root, b"file", 0, b"original"), Ok(8));
    let bytes = fs.into_block().expect("the volume closes").bytes();

    let mut ro = ARXFS::open_read_only(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY)
        .expect("read-only mount");
    let root = ro.root();
    assert_eq!(
        ro.create(root, b"new", NodeKind::RegularFile),
        Err(DriverError::PermissionDenied)
    );
    assert_eq!(
        ro.write_at(root, b"file", 0, b"clobber!"),
        Err(DriverError::PermissionDenied)
    );
    assert_eq!(ro.remove(root, b"file"), Err(DriverError::PermissionDenied));
    assert_eq!(
        ro.truncate(root, b"file", 0),
        Err(DriverError::PermissionDenied)
    );
    // The inherent mutators (not part of a `Filesystem*` trait) fail closed
    // too — they must early-deny, not rely on the `commit` backstop after
    // dirtying the device.
    assert_eq!(
        ro.reflink(root, b"file", b"clone"),
        Err(DriverError::PermissionDenied)
    );
    let node = ro.lookup(root, b"file").expect("the original file exists");
    assert_eq!(
        ro.set_security(node, Security::new(0o600, 0, 0)),
        Err(DriverError::PermissionDenied)
    );
    // The refused create never appeared, and the existing file is intact.
    assert!(ro.lookup(root, b"new").is_err());
    let node = ro
        .lookup(root, b"file")
        .expect("the original file survives");
    let mut back = [0u8; 8];
    assert_eq!(ro.read_at(node, 0, &mut back), Ok(8));
    assert_eq!(&back, b"original");
}

#[test]
fn a_read_only_mount_never_writes_the_device() {
    // A read-only handle must not mutate a single byte of the backing
    // device — no mount-time companion repairs, no anything. Author a real
    // file so the inherent `reflink`/`set_security` mutators below exercise
    // their *full* copy-on-write/inode-write path (a live source, a live
    // node) and must still refuse before touching the device.
    let mut authored = fmt(512, 256, 32);
    let authored_root = authored.root();
    authored
        .create(authored_root, b"x", NodeKind::RegularFile)
        .expect("create");
    assert_eq!(authored.write_at(authored_root, b"x", 0, b"payload"), Ok(7));
    let bytes = authored.into_block().expect("the volume closes").bytes();
    let before = bytes.clone();
    let mut ro = ARXFS::open_read_only(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY)
        .expect("read-only mount");
    // Touch the read path and attempt (refused) mutations — including the
    // inherent `reflink`/`set_security` mutators, which must refuse before
    // writing a single block (their old `commit`-only backstop would have
    // dirtied free blocks first).
    let root = ro.root();
    let node = ro.lookup(root, b"x").expect("the authored file exists");
    let _ = ro.read_dir(root, 0, &mut [0u8; 64]);
    let _ = ro.create(root, b"z", NodeKind::RegularFile);
    let _ = ro.reflink(root, b"x", b"y");
    let _ = ro.set_security(node, Security::new(0o600, 0, 0));
    let after = ro.into_block().expect("the volume closes").bytes();
    assert_eq!(
        after, before,
        "the read-only handle left the device untouched"
    );
}

#[test]
fn open_rejects_an_unformatted_device() {
    let dev = MemBlock::new(512, 256);
    assert!(matches!(
        ARXFS::open(dev, &TEST_KEY),
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
    let bytes = fs.into_block().expect("the volume closes").bytes();
    let mut fs = ARXFS::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY).expect("reopen");
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
        Err(DriverError::AlreadyExists)
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
    assert_eq!(fs.remove(root, b"d"), Err(DriverError::DirectoryNotEmpty));
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
    let mut fs = ARXFS::format(
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
    let times = fs.node_info(node).unwrap().times;
    assert_eq!(times.created, old());
    // ARXFS does not track access time: it is always the epoch.
    assert_eq!(times.accessed, Time64::UNIX_EPOCH);

    // A far-future value survives a remount.
    let mut sec = Security::new(0o600, 1, 2);
    sec.required_cap = Some(CapabilityId::AUDIT_READ);
    fs = fs.with_clock(future);
    fs.set_security(node, sec).expect("set_security");
    let bytes = fs.into_block().expect("the volume closes").bytes();
    let mut fs = ARXFS::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY).expect("reopen");
    let node = fs.lookup(fs.root(), b"t").unwrap();
    assert_eq!(fs.security(node).unwrap(), sec);
    // `changed` was stamped by `set_security` under the future clock, and the
    // access time stays the epoch across the remount.
    let times = fs.node_info(node).unwrap().times;
    assert_eq!(times.changed, future());
    assert_eq!(times.accessed, Time64::UNIX_EPOCH);
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
    let bytes = fs.into_block().expect("the volume closes").bytes();
    let mut fs = ARXFS::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY).expect("reopen");
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
    let baseline = base.into_block().expect("the volume closes").bytes();

    for budget in 0..64u32 {
        let mut dev = MemBlock::from_bytes(baseline.clone(), 512, 256);
        dev.write_budget = Some(budget);
        let mut fs = ARXFS::open(dev, &TEST_KEY).expect("baseline opens");
        let root = fs.root();
        // The single transaction may be cut short at `budget` writes.
        let _ = fs.write_at(root, b"new", 0, b"freshdata");
        let bytes = fs.into_block().expect("the volume closes").bytes();

        // Re-open from the (possibly torn) image: it must mount, and the
        // pre-existing files must always be intact.
        let mut fs = ARXFS::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY)
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

/// Every `(key, value)` of the tree at `root`, in key order. The driver walks
/// trees a leaf at a time so its resident bytes never scale with the tree; a
/// test asserting over a whole small tree gathers the walk's steps here.
fn tree_entries(fs: &mut ARXFS<MemBlock>, root: u64, spec: btree::TreeSpec) -> Vec<(u64, Vec<u8>)> {
    let mut walk = TreeWalk::new(fs.block_size).expect("walk buffer");
    let mut out = Vec::new();
    while fs
        .btree_next_leaf(root, spec, &mut walk)
        .expect("walk tree")
    {
        out.extend(walk.entries().map(|(key, value)| (key, value.to_vec())));
    }
    out
}

/// Physical addresses of every node of the tree at `root`, in walk order.
fn tree_nodes(fs: &mut ARXFS<MemBlock>, root: u64, spec: btree::TreeSpec) -> Vec<u64> {
    let mut walk = TreeWalk::new(fs.block_size).expect("walk buffer");
    let mut trail = NodeTrail::new();
    let mut out = Vec::new();
    while fs
        .btree_next_leaf(root, spec, &mut walk)
        .expect("walk tree")
    {
        trail.advance(walk.path());
        out.extend_from_slice(trail.entered());
    }
    out
}

/// Number of nodes in the inode B-tree (its block count on disk).
fn inode_tree_nodes(fs: &mut ARXFS<MemBlock>) -> usize {
    let spec = inode_spec();
    tree_nodes(fs, fs.inode_tree_root, spec).len()
}

/// Number of nodes in `ino`'s per-file extent tree.
fn extent_tree_nodes(fs: &mut ARXFS<MemBlock>, ino: u32) -> usize {
    let inode = fs.read_inode(ino).expect("read inode");
    let spec = extent_spec(ino);
    tree_nodes(fs, inode.extent_root, spec).len()
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
    let bytes = fs.into_block().expect("the volume closes").bytes();
    let mut fs = ARXFS::open(MemBlock::from_bytes(bytes, 4096, 4096), &TEST_KEY).expect("reopen");
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
    let bytes = fs.into_block().expect("the volume closes").bytes();
    let mut fs = ARXFS::open(MemBlock::from_bytes(bytes, 4096, 4096), &TEST_KEY).expect("reopen2");
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

    let check = |fs: &mut ARXFS<MemBlock>| {
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
    let bytes = fs.into_block().expect("the volume closes").bytes();
    let mut fs = ARXFS::open(MemBlock::from_bytes(bytes, 512, 4096), &TEST_KEY).expect("reopen");
    check(&mut fs);
}

#[test]
fn large_contiguous_write_collapses_to_few_extents() {
    // A single sequential write of incompressible data lands in contiguous
    // physical blocks through the per-block path, so the run-merging extent
    // map keeps it to one record, not one per block.
    let mut fs = fmt(4096, 4096, 64);
    let root = fs.root();
    fs.create(root, b"big", NodeKind::RegularFile)
        .expect("create");
    let body = incompressible(4096 * 64);
    assert_eq!(fs.write_at(root, b"big", 0, &body), Ok(body.len()));
    let node = fs.lookup(root, b"big").expect("lookup");
    let ino = u32::try_from(node.raw()).unwrap();
    let inode = fs.read_inode(ino).expect("inode");
    let extents = tree_entries(&mut fs, inode.extent_root, extent_spec(ino));
    assert_eq!(extents.len(), 1, "contiguous write should be one extent");

    // The compressible variant instead stores one bounded compressed extent
    // per whole cluster (plus the raw tail), never one record per block.
    fs.create(root, b"zip", NodeKind::RegularFile)
        .expect("create zip");
    let body = alloc::vec![0x7Eu8; 4096 * 64];
    assert_eq!(fs.write_at(root, b"zip", 0, &body), Ok(body.len()));
    let cap = fs.data_capacity();
    let blocks = (body.len() as u64).div_ceil(cap);
    let clusters = blocks / COMPRESS_CLUSTER_BLOCKS;
    let ino = u32::try_from(fs.lookup(root, b"zip").expect("lookup").raw()).unwrap();
    let inode = fs.read_inode(ino).expect("inode");
    let total = fs.total_blocks;
    let entries = tree_entries(&mut fs, inode.extent_root, extent_spec(ino));
    let compressed = entries
        .iter()
        .filter(|(_, v)| Extent::decode(v, total).expect("decodes").compressed)
        .count() as u64;
    assert_eq!(
        compressed, clusters,
        "each whole cluster stores as one compressed extent"
    );
    assert!(
        entries.len() as u64 <= clusters + 1,
        "the raw tail merges into at most one extra run"
    );
}

#[test]
fn free_space_rebuild_matches_authoritative_extents() {
    // Build a volume with files, a sparse file, and deletions, then assert the
    // free-block set rebuilt by walking the trees at mount is byte-for-byte the
    // set the live filesystem maintained (`docs/src/filesystem/arxfs-spec.md` §16).
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
    let live = fs.used_blocks();

    let bytes = fs.into_block().expect("the volume closes").bytes();
    let mut rebuilt =
        ARXFS::open(MemBlock::from_bytes(bytes, 4096, 2048), &TEST_KEY).expect("reopen");
    assert_eq!(
        rebuilt.used_blocks(),
        live,
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
    // repair the primary on disk (`docs/src/filesystem/arxfs-spec.md` §8).
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"f", NodeKind::RegularFile)
        .expect("create");
    fs.write_at(root, b"f", 0, b"hello").expect("write");
    let target = fs.inode_tree_root;
    assert_ne!(target, 0, "the volume has an inode tree");

    let bs = 512usize;
    let mut bytes = fs.into_block().expect("the volume closes").bytes();
    let off = as_usize(target) * bs + HEADER_LEN; // first payload byte
    let original = bytes[off];
    bytes[off] ^= 0xff; // wound only the primary copy

    let mut fs = ARXFS::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY)
        .expect("mounts by falling back to the companion mirror");
    let node = fs.lookup(fs.root(), b"f").expect("file survives");
    let mut buf = [0u8; 5];
    let n = fs.read_at(node, 0, &mut buf).expect("read");
    assert_eq!(&buf[..n], b"hello");

    // The primary copy was repaired in place from the good companion.
    let healed = fs.into_block().expect("the volume closes").bytes();
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
    // and never trusting the corrupt bytes.
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"f", NodeKind::RegularFile)
        .expect("create");
    fs.write_at(root, b"f", 0, b"hello").expect("write");
    let target = fs.inode_tree_root;
    assert_ne!(target, 0);

    let bs = 512usize;
    let mut bytes = fs.into_block().expect("the volume closes").bytes();
    bytes[as_usize(target) * bs + HEADER_LEN] ^= 0xff;
    bytes[as_usize(target + 1) * bs + HEADER_LEN] ^= 0xff;

    assert!(
        ARXFS::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY).is_err(),
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
    let mut bytes = fs.into_block().expect("the volume closes").bytes();
    // Every ring primary lives at an even block in `0..RING_BLOCKS`; corrupt
    // the keyed tag of every primary slot, leaving each companion intact.
    for slot in 0..superblock::RING_SLOTS {
        let primary = superblock::slot_block(slot);
        bytes[as_usize(primary) * bs + 80] ^= 0xff; // inside the tag slot
    }
    let mut fs = ARXFS::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY)
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
    let baseline = base.into_block().expect("the volume closes").bytes();
    let new = alloc::vec![0x55u8; 512 * 24];

    for budget in 0..160u32 {
        let mut dev = MemBlock::from_bytes(baseline.clone(), 512, 512);
        dev.write_budget = Some(budget);
        let mut fs = ARXFS::open(dev, &TEST_KEY).expect("baseline opens");
        let root = fs.root();
        let _ = fs.write_at(root, b"f", 0, &new);
        let bytes = fs.into_block().expect("the volume closes").bytes();

        let mut fs = ARXFS::open(MemBlock::from_bytes(bytes, 512, 512), &TEST_KEY)
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
    // `PermissionDenied` — never a panic, never a misread.
    let mut fs = fmt(512, 256, 32);
    fs.create(fs.root(), b"f", NodeKind::RegularFile)
        .expect("create");
    let bytes = fs.into_block().expect("the volume closes").bytes();

    let mut wrong = TEST_KEY;
    wrong[0] ^= 0x01;
    assert!(matches!(
        ARXFS::open(MemBlock::from_bytes(bytes.clone(), 512, 256), &wrong),
        Err(DriverError::PermissionDenied)
    ));
    // The correct key still mounts the very same image.
    ARXFS::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY)
        .expect("the right key still mounts");
}

#[test]
fn passphrase_derived_key_unlocks_the_volume() {
    // The passphrase-unlock indirection (`unlock::UnlockDescriptor`) drives a
    // real volume end to end: a volume formatted under the
    // key *derived* from a passphrase mounts only when the same passphrase +
    // descriptor re-derive that key, and a wrong passphrase is refused
    // fail-closed exactly like any other wrong key.
    let mut salt_entropy = TestEntropy::new();
    let desc = UnlockDescriptor::provision(UNLOCK_MIN_ITERATIONS, &mut salt_entropy)
        .expect("provision a descriptor");
    let passphrase = b"correct horse battery staple";
    let key = desc.derive_volume_key(passphrase);

    let fs = ARXFS::format(MemBlock::new(512, 256), 32, &key, &mut TestEntropy::new())
        .expect("format under the passphrase-derived key");
    let bytes = fs.into_block().expect("the volume closes").bytes();

    // The same passphrase + descriptor re-derive the identical key and mount.
    let rederived = desc.derive_volume_key(passphrase);
    ARXFS::open(MemBlock::from_bytes(bytes.clone(), 512, 256), &rederived)
        .expect("the re-derived key mounts the volume");

    // A wrong passphrase derives a different key, refused like any wrong key.
    let wrong = desc.derive_volume_key(b"wrong passphrase");
    assert!(matches!(
        ARXFS::open(MemBlock::from_bytes(bytes, 512, 256), &wrong),
        Err(DriverError::PermissionDenied)
    ));
}

#[test]
fn no_plaintext_filename_or_data_at_rest() {
    // ARXFS has no plaintext mode: a distinctive filename and file content
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
    let bytes = fs.into_block().expect("the volume closes").bytes();

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
    let bytes = fs.into_block().expect("the volume closes").bytes();

    let mut fs = ARXFS::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY).expect("remount");
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
    // the AEAD authenticator on read — a failed decrypt fails closed, never returning mis-decrypted bytes.
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
    let mut bytes = fs.into_block().expect("the volume closes").bytes();
    bytes[as_usize(phys) * bs] ^= 0xff; // wound the ciphertext

    let mut fs = ARXFS::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY).expect("remount");
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
fn data_block_phys(fs: &mut ARXFS<MemBlock>, name: &[u8], bi: u64) -> u64 {
    let node = fs.lookup(fs.root(), name).expect("lookup");
    let ino = u32::try_from(node.raw()).unwrap();
    let inode = fs.read_inode(ino).expect("read inode");
    let phys = fs.block_ptr(&inode, bi).expect("block pointer");
    assert_ne!(phys, 0, "the file has a mapped data block");
    phys
}

/// Read the whole of file `node` into a fresh vector.
fn read_all(fs: &mut ARXFS<MemBlock>, node: NodeId, len: usize) -> alloc::vec::Vec<u8> {
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
    // corruption, and all three fail closed. The test
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
    let baseline = fs.into_block().expect("the volume closes").bytes();

    let reopen = |bytes: alloc::vec::Vec<u8>| {
        ARXFS::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY).expect("remount")
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

    let stored_hash = |fs: &mut ARXFS<MemBlock>, name: &[u8]| -> [u8; LOGICAL_HASH_LEN] {
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
    let bytes = fs.into_block().expect("the volume closes").bytes();

    let mut fs = ARXFS::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY).expect("remount");
    let node = fs.lookup(fs.root(), b"f").expect("file survives");
    assert_eq!(read_all(&mut fs, node, expected.len()), expected);

    let patch = alloc::vec![0x99u8; 600];
    assert_eq!(fs.write_at(fs.root(), b"f", 100, &patch), Ok(patch.len()));
    expected[100..700].copy_from_slice(&patch);
    let bytes = fs.into_block().expect("the volume closes").bytes();

    let mut fs = ARXFS::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY).expect("remount2");
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
// Stage 6: first-party compression on the data-record pipeline.
// ---------------------------------------------------------------------------

/// Read the on-disk stored-form descriptor of the data block at `phys`.
fn stored_form_at(fs: &mut ARXFS<MemBlock>, phys: u64) -> StoredForm {
    let mut raw = [0u8; MAX_BLOCK_SIZE];
    fs.read_block(phys, &mut raw).expect("raw read");
    let off = fs.compression_desc_offset();
    read_stored_form(&raw[off..off + COMPRESSION_DESCRIPTOR_LEN]).expect("descriptor parses")
}

/// The extent covering logical block `bi` of file `name`, with its starting
/// logical block.
fn extent_of(fs: &mut ARXFS<MemBlock>, name: &[u8], bi: u64) -> (u64, Extent) {
    let node = fs.lookup(fs.root(), name).expect("lookup");
    let ino = u32::try_from(node.raw()).unwrap();
    let inode = fs.read_inode(ino).expect("read inode");
    fs.extent_lookup(&inode, bi)
        .expect("extent lookup")
        .expect("block is mapped")
}

/// A whole-cluster payload of repeating (highly compressible) text.
fn compressible_cluster(fs: &ARXFS<MemBlock>) -> alloc::vec::Vec<u8> {
    let len = as_usize(fs.data_capacity() * COMPRESS_CLUSTER_BLOCKS);
    let mut payload = alloc::vec::Vec::new();
    while payload.len() < len {
        payload.extend_from_slice(b"TAIRiX arxfs ");
    }
    payload.truncate(len);
    payload
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
fn single_block_records_are_stored_raw_and_round_trip() {
    // A single-block record is always stored raw — even highly compressible
    // content: inside a fixed 1:1 block a compressed frame frees nothing, so
    // compressing it would burn CPU for zero benefit. Both an incompressible
    // and a compressible block round-trip byte-identically.
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    let cap = as_usize(fs.data_capacity());
    for (name, payload) in [
        (b"r".as_slice(), incompressible(cap)),
        (b"c", alloc::vec![0x41u8; cap]),
    ] {
        fs.create(root, name, NodeKind::RegularFile)
            .expect("create");
        assert_eq!(fs.write_at(root, name, 0, &payload), Ok(payload.len()));
        let phys = data_block_phys(&mut fs, name, 0);
        assert_eq!(
            stored_form_at(&mut fs, phys),
            StoredForm::Raw,
            "a single-block record is stored raw"
        );
        let node = fs.lookup(fs.root(), name).expect("file survives");
        assert_eq!(read_all(&mut fs, node, payload.len()), payload);
    }
}

#[test]
fn compressible_cluster_frees_blocks_and_round_trips_across_remount_and_cow() {
    // A whole-cluster write of compressible data stores as a compressed
    // extent occupying strictly fewer physical blocks — real freed space,
    // the win mandatory compression exists for — and reads back
    // byte-identical across a remount. A partial overwrite decomposes the
    // cluster back to per-block records and still reads back correctly.
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"c", NodeKind::RegularFile)
        .expect("create");
    let cap = as_usize(fs.data_capacity());
    let payload = compressible_cluster(&fs);
    assert_eq!(fs.write_at(root, b"c", 0, &payload), Ok(payload.len()));

    let (start, ext) = extent_of(&mut fs, b"c", 0);
    assert_eq!(start, 0, "the compressed extent is cluster-aligned");
    assert!(ext.compressed, "a repetitive cluster compresses");
    assert_eq!(ext.len, COMPRESS_CLUSTER_BLOCKS);
    assert!(
        ext.stored < COMPRESS_CLUSTER_BLOCKS,
        "a compressed cluster frees whole blocks: stored {}",
        ext.stored
    );
    assert!(
        matches!(
            stored_form_at(&mut fs, ext.phys),
            StoredForm::ClusterHead { .. }
        ),
        "the first stored block identifies itself as the cluster head"
    );
    let node = fs.lookup(root, b"c").expect("lookup");
    let info = fs.node_info(node).expect("info");
    assert_eq!(info.size, payload.len() as u64);
    assert_eq!(
        info.allocated,
        ext.stored * 512,
        "allocated bytes reflect the stored run, not the logical size"
    );
    assert!(
        info.allocated < info.size,
        "compression saves real space: {} >= {}",
        info.allocated,
        info.size
    );

    let bytes = fs.into_block().expect("the volume closes").bytes();
    let mut fs = ARXFS::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY).expect("remount");
    let node = fs.lookup(fs.root(), b"c").expect("file survives");
    assert_eq!(
        read_all(&mut fs, node, payload.len()),
        payload,
        "compressed data reads back byte-identical after a remount"
    );

    // A partial (one-block) overwrite decomposes the cluster back into
    // ordinary per-block records and the file still verifies.
    let patch = incompressible(cap);
    let at = u64::try_from(cap).unwrap();
    assert_eq!(fs.write_at(fs.root(), b"c", at, &patch), Ok(patch.len()));
    let mut expected = payload.clone();
    expected[cap..cap * 2].copy_from_slice(&patch);
    let (_, ext) = extent_of(&mut fs, b"c", 1);
    assert!(
        !ext.compressed,
        "a partially overwritten cluster decomposes to per-block records"
    );
    let bytes = fs.into_block().expect("the volume closes").bytes();

    let mut fs = ARXFS::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY).expect("remount2");
    let node = fs.lookup(fs.root(), b"c").expect("file still there");
    assert_eq!(
        read_all(&mut fs, node, expected.len()),
        expected,
        "decomposed data verifies after the COW rewrite"
    );
}

#[test]
fn integrity_faults_on_a_compressed_cluster_fail_closed() {
    // The integrity layers guard every stored block of a compressed cluster:
    // an at-rest (media) flip is a physical fault, a tampered content-slot
    // hash is a logical fault, and the production read path fails closed on
    // both.
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"c", NodeKind::RegularFile)
        .expect("create");
    let payload = compressible_cluster(&fs);
    assert_eq!(fs.write_at(root, b"c", 0, &payload), Ok(payload.len()));
    let (_, ext) = extent_of(&mut fs, b"c", 0);
    assert!(ext.compressed, "the cluster is stored compressed");

    let csum_off = fs.phys_checksum_offset();
    let hash_off = fs.logical_hash_offset();
    let bs = 512usize;
    let base = as_usize(ext.phys) * bs;
    let baseline = fs.into_block().expect("the volume closes").bytes();

    let reopen = |bytes: alloc::vec::Vec<u8>| {
        ARXFS::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY).expect("remount")
    };

    // Physical: a flipped at-rest byte in a stored block is caught by the
    // fast checksum, and the production read path fails closed.
    {
        let mut bytes = baseline.clone();
        bytes[base] ^= 0x01;
        let mut fs = reopen(bytes);
        let (_, ext) = extent_of(&mut fs, b"c", 0);
        assert_eq!(
            fs.read_data_cluster_classified(&ext).unwrap_err(),
            DataFault::Physical
        );
        let node = fs.lookup(fs.root(), b"c").expect("lookup");
        let mut out = [0u8; 64];
        assert!(
            matches!(fs.read_at(node, 0, &mut out), Err(DriverError::DeviceFault)),
            "the production read path fails closed on a corrupt cluster"
        );
    }
    // Logical: corrupt the stored content-slot hash and repair the checksum,
    // so the AEAD passes but the slot hash mismatches.
    {
        let mut bytes = baseline.clone();
        bytes[base + hash_off] ^= 0x01;
        let fixed = physical_checksum(&bytes[base..base + csum_off]);
        bytes[base + csum_off..base + csum_off + PHYS_CHECKSUM_LEN].copy_from_slice(&fixed);
        let mut fs = reopen(bytes);
        let (_, ext) = extent_of(&mut fs, b"c", 0);
        assert_eq!(
            fs.read_data_cluster_classified(&ext).unwrap_err(),
            DataFault::Logical
        );
    }
}

#[test]
fn extent_codec_round_trips_and_rejects_undefined_shapes() {
    // The widened extent value decodes exactly what was encoded and refuses
    // every shape the format does not define (fail closed).
    const DEVICE: u64 = 1 << 41;
    let raw = Extent::raw(7, 1 << 40);
    assert_eq!(
        Extent::decode(&raw.encode(), DEVICE).expect("raw decodes"),
        raw
    );
    let cluster = Extent::cluster(9, COMPRESS_CLUSTER_BLOCKS, 3);
    assert_eq!(
        Extent::decode(&cluster.encode(), DEVICE).expect("cluster decodes"),
        cluster
    );

    // Unknown flag bits.
    let mut bad = cluster.encode();
    bad[20] = 0xFF;
    assert_eq!(Extent::decode(&bad, DEVICE), Err(DriverError::DeviceFault));
    // A compressed cluster must occupy strictly fewer stored blocks.
    let full = Extent::cluster(9, COMPRESS_CLUSTER_BLOCKS, COMPRESS_CLUSTER_BLOCKS);
    assert_eq!(
        Extent::decode(&full.encode(), DEVICE),
        Err(DriverError::DeviceFault)
    );
    let empty = Extent::cluster(9, COMPRESS_CLUSTER_BLOCKS, 0);
    assert_eq!(
        Extent::decode(&empty.encode(), DEVICE),
        Err(DriverError::DeviceFault)
    );
    // ... and never cover more than one cluster.
    let long = Extent::cluster(9, COMPRESS_CLUSTER_BLOCKS * 2, 3);
    assert_eq!(
        Extent::decode(&long.encode(), DEVICE),
        Err(DriverError::DeviceFault)
    );
    // A zero-length run maps nothing. No write path produces one, and the
    // downward tail free would stop on it and report a tree it had emptied
    // while the record still stood — leaving its nodes allocated and reachable
    // from nothing once the inode went.
    assert_eq!(
        Extent::decode(&Extent::raw(7, 0).encode(), DEVICE),
        Err(DriverError::DeviceFault),
        "a zero-length raw run is a device fault"
    );
    assert_eq!(
        Extent::decode(&Extent::cluster(9, 0, 0).encode(), DEVICE),
        Err(DriverError::DeviceFault),
        "a zero-length cluster is a device fault"
    );
    // A raw extent never carries a stored length.
    let mut crooked = Extent::raw(7, 4).encode();
    crooked[16] = 1;
    assert_eq!(
        Extent::decode(&crooked, DEVICE),
        Err(DriverError::DeviceFault)
    );

    // A stored run must fit inside the device. Without this the free path's
    // `phys + offset` arithmetic wraps: a run naming the end of the address
    // space would release blocks it never covered.
    assert_eq!(
        Extent::decode(&Extent::raw(DEVICE - 4, 5).encode(), DEVICE),
        Err(DriverError::DeviceFault),
        "a raw run past the end of the device is a device fault"
    );
    assert_eq!(
        Extent::decode(&Extent::raw(u64::MAX - 1, u64::MAX).encode(), DEVICE),
        Err(DriverError::DeviceFault),
        "a raw run wrapping the address space is a device fault"
    );
    assert_eq!(
        Extent::decode(&Extent::cluster(DEVICE - 2, 8, 3).encode(), DEVICE),
        Err(DriverError::DeviceFault),
        "a cluster's stored run past the end of the device is a device fault"
    );
    // The exact fit is lawful.
    assert!(Extent::decode(&Extent::raw(DEVICE - 5, 5).encode(), DEVICE).is_ok());
}

#[test]
fn all_zero_cluster_write_becomes_holes_not_a_compressed_extent() {
    // Zero detection outranks compression: a whole-cluster write of zeroes
    // maps nothing at all (a compressed extent would still cost blocks).
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"z", NodeKind::RegularFile)
        .expect("create");
    let len = as_usize(fs.data_capacity() * COMPRESS_CLUSTER_BLOCKS);
    let zeros = alloc::vec![0u8; len];
    assert_eq!(fs.write_at(root, b"z", 0, &zeros), Ok(len));
    let ino = file_ino(&mut fs, b"z");
    assert_eq!(mapped_block_count(&mut fs, ino), 0, "zeroes map nothing");
    assert_reads_all_zero(&mut fs, b"z", len);
}

#[test]
fn unaligned_and_sub_cluster_writes_store_per_block() {
    // Compression clusters form only on aligned whole-cluster spans: an
    // unaligned cluster-sized write and a small compressible file both store
    // through the per-block raw path and round-trip.
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    let cap = fs.data_capacity();
    let payload = compressible_cluster(&fs);

    fs.create(root, b"u", NodeKind::RegularFile)
        .expect("create u");
    assert_eq!(
        fs.write_at(root, b"u", cap, &payload),
        Ok(payload.len()),
        "cluster-sized write, one block off alignment"
    );
    let ino = file_ino(&mut fs, b"u");
    let inode = fs.read_inode(ino).expect("inode");
    let total = fs.total_blocks;
    for (_, value) in tree_entries(&mut fs, inode.extent_root, extent_spec(ino)) {
        assert!(
            !Extent::decode(&value, total).expect("decodes").compressed,
            "an unaligned span never forms a compressed extent"
        );
    }
    let node = fs.lookup(root, b"u").expect("lookup");
    let mut got = alloc::vec![0u8; payload.len()];
    assert_eq!(fs.read_at(node, cap, &mut got), Ok(payload.len()));
    assert_eq!(got, payload);

    fs.create(root, b"s", NodeKind::RegularFile)
        .expect("create s");
    let small = &payload[..as_usize(cap) * 3];
    assert_eq!(fs.write_at(root, b"s", 0, small), Ok(small.len()));
    let (_, ext) = extent_of(&mut fs, b"s", 0);
    assert!(!ext.compressed, "a sub-cluster file stores per block");
}

#[test]
fn truncate_into_a_compressed_cluster_decomposes_and_keeps_the_prefix() {
    // Cutting a file mid-cluster decomposes the cluster, frees the truncated
    // tail, and preserves the surviving prefix byte-exactly.
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"t", NodeKind::RegularFile)
        .expect("create");
    let cap = fs.data_capacity();
    let payload = compressible_cluster(&fs);
    assert_eq!(fs.write_at(root, b"t", 0, &payload), Ok(payload.len()));
    let (_, ext) = extent_of(&mut fs, b"t", 0);
    assert!(ext.compressed, "starts compressed");

    let keep = as_usize(cap * 5 + 17);
    fs.truncate(root, b"t", cap * 5 + 17).expect("truncate");
    let node = fs.lookup(root, b"t").expect("lookup");
    assert_eq!(fs.node_info(node).expect("info").size, (keep) as u64);
    let ino = file_ino(&mut fs, b"t");
    let inode = fs.read_inode(ino).expect("inode");
    let total = fs.total_blocks;
    for (_, value) in tree_entries(&mut fs, inode.extent_root, extent_spec(ino)) {
        assert!(
            !Extent::decode(&value, total).expect("decodes").compressed,
            "the straddled cluster decomposed"
        );
    }
    assert_eq!(read_all(&mut fs, node, keep), payload[..keep].to_vec());
    assert_extents_ordered_and_disjoint(&mut fs, ino);
}

#[test]
fn overwriting_and_removing_a_compressed_cluster_returns_its_space() {
    // A whole-cluster overwrite replaces the old stored run without leaking
    // it, and removing the file returns every stored block to the free pool.
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    // Baseline after `create`: the directory block the new entry grew stays
    // with the directory, so the file's own storage is what must return.
    fs.create(root, b"w", NodeKind::RegularFile)
        .expect("create");
    let before_write = fs.free_count;
    let payload = compressible_cluster(&fs);
    assert_eq!(fs.write_at(root, b"w", 0, &payload), Ok(payload.len()));
    let (_, first) = extent_of(&mut fs, b"w", 0);
    assert!(first.compressed);

    // Overwrite the whole cluster with different compressible content.
    let mut second_payload = payload.clone();
    for byte in &mut second_payload {
        *byte = byte.wrapping_add(1);
    }
    assert_eq!(
        fs.write_at(root, b"w", 0, &second_payload),
        Ok(second_payload.len())
    );
    let node = fs.lookup(root, b"w").expect("lookup");
    assert_eq!(
        read_all(&mut fs, node, second_payload.len()),
        second_payload
    );
    let (_, ext) = extent_of(&mut fs, b"w", 0);
    assert!(ext.compressed, "the overwrite stored a fresh cluster");

    // Removing the file returns its stored run to the free pool. The
    // metadata trees may shrink further on removal, so the total may exceed
    // the post-create baseline — it must never drop below it (a leak).
    let (_, last) = extent_of(&mut fs, b"w", 0);
    fs.remove(root, b"w").expect("remove");
    for b in 0..last.stored {
        assert!(
            !fs.is_used(last.phys + b),
            "stored cluster block {b} returns to the free pool"
        );
    }
    assert!(
        fs.free_count >= before_write,
        "no blocks leaked: {} < {before_write}",
        fs.free_count
    );
    // The mount-time rebuild reproduces the same used set, so nothing was
    // double-freed either.
    let live = fs.used_blocks();
    let bytes = fs.into_block().expect("the volume closes").bytes();
    let mut fs = ARXFS::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY).expect("reopen");
    assert_eq!(
        fs.used_blocks(),
        live,
        "the rebuilt used set agrees after removal"
    );
}

#[test]
fn reflink_shares_a_compressed_cluster_and_diverges_on_write() {
    // A reflink of a compressed file shares the stored run at cluster
    // granularity (refcount 2, no data copied); overwriting one side
    // copy-on-writes it while the other still reads the original bytes.
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"src", NodeKind::RegularFile)
        .expect("create");
    let cap = as_usize(fs.data_capacity());
    let payload = compressible_cluster(&fs);
    assert_eq!(fs.write_at(root, b"src", 0, &payload), Ok(payload.len()));
    let (_, ext) = extent_of(&mut fs, b"src", 0);
    assert!(ext.compressed);

    fs.reflink(root, b"src", b"dup").expect("reflink");
    assert_eq!(
        fs.data_refcount(ext.phys).expect("refcount"),
        2,
        "the cluster is shared as one refcounted unit"
    );
    let (_, dup_ext) = extent_of(&mut fs, b"dup", 0);
    assert_eq!(dup_ext, ext, "the clone maps the same stored run");
    let node_dup = fs.lookup(root, b"dup").expect("lookup dup");
    assert_eq!(read_all(&mut fs, node_dup, payload.len()), payload);

    // Diverge: a partial write to the clone decomposes and copies, leaving
    // the source's compressed cluster intact.
    let patch = incompressible(cap);
    assert_eq!(fs.write_at(root, b"dup", 0, &patch), Ok(patch.len()));
    let mut expected = payload.clone();
    expected[..cap].copy_from_slice(&patch);
    let node_dup = fs.lookup(root, b"dup").expect("lookup dup");
    assert_eq!(read_all(&mut fs, node_dup, expected.len()), expected);
    let node_src = fs.lookup(root, b"src").expect("lookup src");
    assert_eq!(read_all(&mut fs, node_src, payload.len()), payload);
    let (_, src_ext) = extent_of(&mut fs, b"src", 0);
    assert!(
        src_ext.compressed,
        "the source keeps its compressed cluster"
    );
}

#[test]
fn rebuild_scrub_and_check_agree_on_a_compressed_volume() {
    // The mount-time free-space rebuild reproduces the live used set for a
    // volume holding compressed clusters, raw blocks, and holes; scrub and
    // the offline check both pass it clean.
    let mut fs = fmt(512, 512, 32);
    let root = fs.root();
    let cap = fs.data_capacity();
    fs.create(root, b"mix", NodeKind::RegularFile)
        .expect("create");
    let payload = compressible_cluster(&fs);
    assert_eq!(fs.write_at(root, b"mix", 0, &payload), Ok(payload.len()));
    // A raw tail block and a hole beyond it.
    let tail = incompressible(as_usize(cap));
    assert_eq!(
        fs.write_at(root, b"mix", cap * COMPRESS_CLUSTER_BLOCKS, &tail),
        Ok(tail.len())
    );
    fs.truncate(root, b"mix", cap * (COMPRESS_CLUSTER_BLOCKS + 8))
        .expect("extend with a hole");
    let live = fs.used_blocks();

    let bytes = fs.into_block().expect("the volume closes").bytes();
    let mut fs = ARXFS::open(MemBlock::from_bytes(bytes, 512, 512), &TEST_KEY).expect("reopen");
    assert_eq!(
        fs.used_blocks(),
        live,
        "the rebuilt free set accounts compressed extents by stored size"
    );
    let report = scrub_full(&mut fs);
    assert_eq!(report.pass, PassVerdict::Complete, "scrub completes");
    assert!(!report.found_faults(), "the volume is clean: {report:?}");
    assert!(
        report.data_blocks_checked >= 1,
        "the cluster's stored blocks were verified"
    );
    let check = fs.check(&GrantAll, &NullSink).expect("check");
    assert_eq!(
        check.structure,
        StructureVerdict::Sound,
        "check validates: {check:?}"
    );

    let node = fs.lookup(fs.root(), b"mix").expect("lookup");
    assert_eq!(read_all(&mut fs, node, payload.len()), payload);
}

#[test]
fn alloc_data_run_claims_contiguous_blocks_and_fails_closed_when_fragmented() {
    // The run allocator returns physically contiguous claimed blocks and
    // reports NoSpace — never a partial claim — when no gap is wide enough.
    let mut fs = fmt(512, 64, 8);
    fs.begin().expect("begin");
    let run = fs.alloc_data_run(4).expect("run allocates");
    for b in 0..4 {
        assert!(fs.is_used(run + b), "run block {b} is claimed");
    }
    // Fragment the remaining pool: claim every other free block, leaving no
    // 3-block gap anywhere.
    let mut block = RING_BLOCKS;
    while block < fs.total_blocks {
        if fs.is_used(block) {
            block += 1;
        } else {
            fs.claim_run(block, 1);
            block += 2;
        }
    }
    assert_eq!(
        fs.alloc_data_run(3),
        Err(DriverError::NoSpace),
        "no contiguous gap of three blocks remains"
    );
    fs.rollback();
}

// ---------------------------------------------------------------------------
// Stage 7: chunk table, refcounts, reverse refs, reflinks, dedupe index.
// ---------------------------------------------------------------------------

/// The inode number behind file `name` under the root.
fn file_ino(fs: &mut ARXFS<MemBlock>, name: &[u8]) -> u32 {
    let node = fs.lookup(fs.root(), name).expect("lookup");
    u32::try_from(node.raw()).unwrap()
}

/// The number of records in the chunk/refcount tree (one per shared chunk).
fn chunk_count(fs: &mut ARXFS<MemBlock>) -> usize {
    tree_entries(fs, fs.chunk_tree_root, chunk_spec()).len()
}

#[test]
fn byte_verify_before_share_refuses_to_merge_unequal_data() {
    // A dedupe-index entry is only ever a *hint*: before sharing, the
    // candidate's bytes are compared to the incoming record, so two blocks
    // whose index keys collide but whose bytes differ are never merged (merging unequal data is corruption). A natural logical-hash collision is
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
    fs.dedupe_index_mut().insert(
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
    // the shared chunk's refcount, leaving the other sharer's data intact.
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
    // blocks diverge.
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
    // the freed space is reusable and the chunk tree is empty.
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
    let bytes = fs.into_block().expect("the volume closes").bytes();
    let mut fs = ARXFS::open(MemBlock::from_bytes(bytes, 4096, 256), &TEST_KEY).expect("remount");
    assert_eq!(chunk_count(&mut fs), 0);
    assert!(
        fs.dedupe_index_mut().is_empty(),
        "a fresh mount starts with an empty dedupe cache"
    );
    // The reclaimed space is reusable.
    fs.create(fs.root(), b"c", NodeKind::RegularFile)
        .expect("create c");
    assert_eq!(fs.write_at(fs.root(), b"c", 0, &body), Ok(cap));
}

#[test]
fn the_dedupe_cache_warms_from_writes_rather_than_a_mount_time_walk() {
    // The dedupe index is a bounded cache, never authoritative, and is not
    // pre-seeded at mount: walking the chunk tree would cost a read per chunk
    // on a volume of any size. A fresh mount therefore starts cold and misses
    // the first duplicate — an allowed miss — then shares every later one.
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

    let bytes = fs.into_block().expect("the volume closes").bytes();
    let mut fs = ARXFS::open(MemBlock::from_bytes(bytes, 4096, 256), &TEST_KEY).expect("remount");
    let root = fs.root();
    assert_eq!(chunk_count(&mut fs), 1, "the shared chunk survives");

    fs.create(root, b"c", NodeKind::RegularFile)
        .expect("create c");
    assert_eq!(fs.write_at(root, b"c", 0, &body), Ok(cap));
    let warmed = data_block_phys(&mut fs, b"c", 0);
    assert_ne!(
        warmed, shared,
        "a cold cache misses the pre-existing chunk, which is allowed"
    );
    assert_eq!(
        fs.data_refcount(shared).expect("refcount"),
        2,
        "the missed duplicate left the original chunk's refcount alone"
    );

    // That write warmed the cache, so the next identical write shares it.
    fs.create(root, b"d", NodeKind::RegularFile)
        .expect("create d");
    assert_eq!(fs.write_at(root, b"d", 0, &body), Ok(cap));
    assert_eq!(
        data_block_phys(&mut fs, b"d", 0),
        warmed,
        "the warmed cache shares the chunk this session wrote"
    );
    assert_eq!(
        fs.data_refcount(warmed).expect("refcount"),
        2,
        "the fourth writer joined the warmed chunk"
    );
}

#[test]
fn dedupe_is_scoped_to_the_encryption_domain() {
    // Every chunk record carries the volume's encryption domain, and the
    // dedupe-index key is domain-scoped, so dedupe never crosses a domain.
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
    let domain = fs.dedupe_domain;
    assert!(
        fs.dedupe_index_mut()
            .contains_key(&dedupe_key(domain, len, &hash)),
        "the chunk is indexed under its own domain"
    );
    assert!(
        !fs.dedupe_index_mut()
            .contains_key(&dedupe_key(domain ^ 0x1, len, &hash)),
        "a different domain keys to a different slot"
    );
}

#[test]
fn integrity_and_compression_hold_on_a_shared_chunk() {
    // A shared chunk is still a data record: compressed at rest, integrity-
    // protected, and byte-exact across a remount and a COW rewrite. Corrupting
    // the shared physical block fails closed for *every* sharer.
    let mut fs = fmt(4096, 256, 64);
    let root = fs.root();
    let cap = as_usize(fs.data_capacity());
    // Compressible, identical content so the two files share one chunk.
    let mut body = alloc::vec::Vec::new();
    while body.len() < cap {
        body.extend_from_slice(b"TAIRiX arxfs dedupe ");
    }
    body.truncate(cap);
    for name in [b"a".as_slice(), b"b"] {
        fs.create(root, name, NodeKind::RegularFile)
            .expect("create");
        assert_eq!(fs.write_at(root, name, 0, &body), Ok(cap));
    }
    let shared = data_block_phys(&mut fs, b"a", 0);
    assert_eq!(fs.data_refcount(shared).expect("refcount"), 2);
    assert_eq!(
        stored_form_at(&mut fs, shared),
        StoredForm::Raw,
        "a shared single-block chunk is stored raw (clusters carry compression)"
    );

    // Round-trips across a remount.
    let bytes = fs.into_block().expect("the volume closes").bytes();
    let mut fs = ARXFS::open(MemBlock::from_bytes(bytes, 4096, 256), &TEST_KEY).expect("remount");
    let root = fs.root();
    let node_a = fs.lookup(root, b"a").expect("lookup a");
    let node_b = fs.lookup(root, b"b").expect("lookup b");
    assert_eq!(read_all(&mut fs, node_a, cap), body);
    assert_eq!(read_all(&mut fs, node_b, cap), body);

    // Corrupting the shared at-rest block is caught for both sharers (the fast
    // physical checksum covers the at-rest bytes).
    let base = as_usize(shared) * 4096;
    let mut bytes = fs.into_block().expect("the volume closes").bytes();
    bytes[base] ^= 0x01;
    let mut fs = ARXFS::open(MemBlock::from_bytes(bytes, 4096, 256), &TEST_KEY).expect("remount2");
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
// Host-injected cluster transform cache (`plans/SMARTRAM.md` SMART3).
// ---------------------------------------------------------------------------

/// Shared observation counters for [`TestClusterCache`], readable after the
/// cache itself has been moved into the mounted volume.
#[derive(Default)]
struct CacheCounts {
    hits: core::sync::atomic::AtomicU64,
    puts: core::sync::atomic::AtomicU64,
    invalidations: core::sync::atomic::AtomicU64,
    purges: core::sync::atomic::AtomicU64,
}

impl CacheCounts {
    fn bump(counter: &core::sync::atomic::AtomicU64) {
        counter.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }

    fn read(counter: &core::sync::atomic::AtomicU64) -> u64 {
        counter.load(core::sync::atomic::Ordering::Relaxed)
    }
}

/// A minimal in-memory [`ClusterCache`] honouring the seam's coherence
/// contract (run-covering invalidation, whole-cache purge), instrumented
/// through shared counters. Test scaffolding only: the production
/// implementation with classification, budgets, pressure, and zeroisation
/// lives in `tairix-kernel`.
struct TestClusterCache {
    counts: alloc::sync::Arc<CacheCounts>,
    entries: alloc::collections::BTreeMap<u64, (u64, alloc::vec::Vec<u8>)>,
}

impl ClusterCache for TestClusterCache {
    fn get(&mut self, phys: u64) -> Option<&[u8]> {
        let entry = self.entries.get(&phys)?;
        CacheCounts::bump(&self.counts.hits);
        Some(entry.1.as_slice())
    }

    fn put(&mut self, phys: u64, stored: u64, plaintext: &[u8]) {
        CacheCounts::bump(&self.counts.puts);
        self.entries.insert(phys, (stored, plaintext.to_vec()));
    }

    fn invalidate_run(&mut self, phys: u64, len: u64) {
        let Some(last) = len.checked_sub(1).map(|last| phys + last) else {
            return;
        };
        let covering = self
            .entries
            .range(..phys)
            .next_back()
            .filter(|(start, (stored, _))| phys < *start + *stored)
            .map(|(start, _)| *start);
        for start in covering.into_iter().chain(
            self.entries
                .range(phys..=last)
                .map(|(&start, _)| start)
                .collect::<alloc::vec::Vec<_>>(),
        ) {
            if self.entries.remove(&start).is_some() {
                CacheCounts::bump(&self.counts.invalidations);
            }
        }
    }

    fn purge(&mut self) {
        CacheCounts::bump(&self.counts.purges);
        self.entries.clear();
    }
}

/// A formatted volume with the instrumented cluster cache installed, plus
/// the shared counter handle.
fn cached_fmt() -> (alloc::sync::Arc<CacheCounts>, ARXFS<MemBlock>) {
    let counts = alloc::sync::Arc::new(CacheCounts::default());
    let cache = TestClusterCache {
        counts: alloc::sync::Arc::clone(&counts),
        entries: alloc::collections::BTreeMap::new(),
    };
    let fs = fmt(512, 256, 32).with_cluster_cache(alloc::boxed::Box::new(cache));
    (counts, fs)
}

/// Read the whole `len` bytes of `node` in single-block chunks, so every
/// chunk exercises the compressed-cluster serving arm separately.
fn read_chunked(fs: &mut ARXFS<MemBlock>, node: NodeId, len: usize) -> alloc::vec::Vec<u8> {
    let mut out = alloc::vec![0u8; len];
    let mut done = 0usize;
    while done < len {
        let end = (done + 512).min(len);
        let n = fs
            .read_at(node, u64::try_from(done).unwrap(), &mut out[done..end])
            .expect("chunked read");
        assert!(n > 0, "chunked read makes progress");
        done += n;
    }
    out
}

#[test]
fn a_retained_cluster_serves_repeat_reads_without_the_transform_pipeline() {
    // The first read of a compressed cluster decompresses it once and
    // offers the plaintext for retention; every later read of the cluster
    // is served from the retained copy. Proof: after the cache is
    // populated, corrupt the stored cluster on the device — a read that
    // touched the device would now fail closed, so a correct result can
    // only have come from the cache.
    let (counts, mut fs) = cached_fmt();
    let root = fs.root();
    fs.create(root, b"c", NodeKind::RegularFile)
        .expect("create");
    let payload = compressible_cluster(&fs);
    assert_eq!(fs.write_at(root, b"c", 0, &payload), Ok(payload.len()));
    assert_eq!(
        CacheCounts::read(&counts.puts),
        0,
        "the write path retains nothing"
    );

    let node = fs.lookup(root, b"c").expect("lookup");
    assert_eq!(read_chunked(&mut fs, node, payload.len()), payload);
    assert_eq!(
        CacheCounts::read(&counts.puts),
        1,
        "one decompression populated the cache"
    );
    assert!(
        CacheCounts::read(&counts.hits) > 0,
        "the later chunks of the first pass already hit"
    );

    // Corrupt the stored cluster head on the device. Only the data block
    // is touched; the metadata tree stays intact.
    let (_, ext) = extent_of(&mut fs, b"c", 0);
    let mut raw = [0u8; MAX_BLOCK_SIZE];
    fs.read_block(ext.phys, &mut raw).expect("raw read");
    raw[HEADER_LEN] ^= 0xFF;
    fs.write_device(ext.phys, &raw).expect("raw write");

    assert_eq!(
        read_chunked(&mut fs, node, payload.len()),
        payload,
        "repeat reads are served from the retained plaintext, not the device"
    );
}

#[test]
fn overwriting_a_cluster_invalidates_its_retained_plaintext() {
    let (counts, mut fs) = cached_fmt();
    let root = fs.root();
    fs.create(root, b"c", NodeKind::RegularFile)
        .expect("create");
    let payload = compressible_cluster(&fs);
    assert_eq!(fs.write_at(root, b"c", 0, &payload), Ok(payload.len()));
    let node = fs.lookup(root, b"c").expect("lookup");
    assert_eq!(read_chunked(&mut fs, node, payload.len()), payload);

    // Overwrite the whole cluster with different compressible content: the
    // superseded stored run is freed, which must drop the retained entry.
    let mut second = payload.clone();
    for byte in &mut second {
        *byte = byte.wrapping_add(1);
    }
    assert_eq!(fs.write_at(root, b"c", 0, &second), Ok(second.len()));
    assert!(
        CacheCounts::read(&counts.invalidations) >= 1,
        "freeing the superseded run invalidated the entry"
    );
    assert_eq!(
        read_chunked(&mut fs, node, second.len()),
        second,
        "reads after the overwrite see the new content, never the stale entry"
    );
}

#[test]
fn truncating_into_a_cluster_invalidates_its_retained_plaintext() {
    let (counts, mut fs) = cached_fmt();
    let root = fs.root();
    fs.create(root, b"c", NodeKind::RegularFile)
        .expect("create");
    let payload = compressible_cluster(&fs);
    assert_eq!(fs.write_at(root, b"c", 0, &payload), Ok(payload.len()));
    let node = fs.lookup(root, b"c").expect("lookup");
    assert_eq!(read_chunked(&mut fs, node, payload.len()), payload);

    // A mid-cluster truncate decomposes the cluster: its stored run is
    // freed, so the retained whole-cluster plaintext must go with it.
    let cap = as_usize(fs.data_capacity());
    let keep = cap * 8;
    fs.truncate(root, b"c", u64::try_from(keep).unwrap())
        .expect("truncate");
    assert!(
        CacheCounts::read(&counts.invalidations) >= 1,
        "decomposing the cluster invalidated the entry"
    );
    assert_eq!(
        read_chunked(&mut fs, node, keep),
        payload[..keep],
        "the kept prefix reads back from the per-block records"
    );
}

#[test]
fn a_failed_mutation_purges_the_retained_plaintext() {
    let (counts, mut fs) = cached_fmt();
    let root = fs.root();
    fs.create(root, b"c", NodeKind::RegularFile)
        .expect("create");
    let payload = compressible_cluster(&fs);
    assert_eq!(fs.write_at(root, b"c", 0, &payload), Ok(payload.len()));
    let node = fs.lookup(root, b"c").expect("lookup");
    assert_eq!(read_chunked(&mut fs, node, payload.len()), payload);

    // A refused mutation rolls its transaction back; the rollback returns
    // this transaction's blocks to the pool without per-block frees, so
    // the whole cache is purged (fail closed).
    assert!(fs.create(root, b"bad/name", NodeKind::RegularFile).is_err());
    assert!(
        CacheCounts::read(&counts.purges) >= 1,
        "the rollback purged the cache"
    );
    assert_eq!(
        read_chunked(&mut fs, node, payload.len()),
        payload,
        "the purged cache repopulates from the intact volume"
    );
}

#[test]
fn a_reflink_shared_cluster_keeps_its_retained_plaintext() {
    // Removing one referrer of a shared cluster decrements the refcount
    // without freeing the stored run, so the retained plaintext stays
    // valid and keeps serving the surviving referrer.
    let (counts, mut fs) = cached_fmt();
    let root = fs.root();
    fs.create(root, b"src", NodeKind::RegularFile)
        .expect("create");
    let payload = compressible_cluster(&fs);
    assert_eq!(fs.write_at(root, b"src", 0, &payload), Ok(payload.len()));
    fs.reflink(root, b"src", b"dst").expect("reflink");
    let src = fs.lookup(root, b"src").expect("src");
    assert_eq!(read_chunked(&mut fs, src, payload.len()), payload);

    fs.remove(root, b"src").expect("remove one referrer");
    let dst = fs.lookup(root, b"dst").expect("dst survives");
    let before = CacheCounts::read(&counts.hits);
    assert_eq!(
        read_chunked(&mut fs, dst, payload.len()),
        payload,
        "the surviving referrer reads the shared cluster"
    );
    assert!(
        CacheCounts::read(&counts.hits) > before,
        "the shared cluster's entry survived the referrer removal"
    );
}

#[test]
fn a_wrong_sized_cache_entry_fails_closed_instead_of_stalling() {
    /// A defective cache handing back an empty slice for every cluster.
    struct LyingCache;

    impl ClusterCache for LyingCache {
        fn get(&mut self, _phys: u64) -> Option<&[u8]> {
            Some(&[])
        }

        fn put(&mut self, _phys: u64, _stored: u64, _plaintext: &[u8]) {}

        fn invalidate_run(&mut self, _phys: u64, _len: u64) {}

        fn purge(&mut self) {}
    }

    let mut fs = fmt(512, 256, 32).with_cluster_cache(alloc::boxed::Box::new(LyingCache));
    let root = fs.root();
    fs.create(root, b"c", NodeKind::RegularFile)
        .expect("create");
    let payload = compressible_cluster(&fs);
    assert_eq!(fs.write_at(root, b"c", 0, &payload), Ok(payload.len()));
    let node = fs.lookup(root, b"c").expect("lookup");
    let mut out = [0u8; 512];
    assert_eq!(
        fs.read_at(node, 0, &mut out),
        Err(DriverError::DeviceFault),
        "a zero-progress cache entry fails the read closed, never a hang"
    );
}

// ---------------------------------------------------------------------------
// Stage 8: online scrub (verify + repair, resumable).
// ---------------------------------------------------------------------------

use tairix_abi::CapabilityQuery;
use tairix_log::{Event, Sink};

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
    fn saw(&self, id: tairix_log::EventId) -> bool {
        self.ids.borrow().contains(&id.0)
    }
}
impl Sink for RecordingSink {
    fn write_event(&self, event: &Event<'_>) {
        self.ids.borrow_mut().push(event.id.0);
    }
}

/// Run a full scrub with all capabilities granted, asserting it succeeds.
/// Assert that a verification pass left every block the committed volume
/// depends on byte-identical, and the free-space accounting exactly as it was.
///
/// This is what "a clean pass mutates nothing" means now that the refcount
/// reconcile streams its per-block claim counts through a transient scratch
/// array in free space (`crate::scratch`): the committed structures, the
/// allocation map, the used-block set, and the free count are all unchanged,
/// while the run the pass borrowed and handed back holds whatever it wrote.
/// Comparing the used set as well as the bytes pins more than a whole-device
/// comparison did — a pass that leaked or lost a block would fail here even if
/// every surviving block matched.
fn assert_committed_state_unchanged(
    before: &[u8],
    fs: &mut ARXFS<MemBlock>,
    used_before: &BTreeSet<u64>,
    free_before: u64,
) {
    assert_eq!(fs.free_count, free_before, "the free count is unchanged");
    let used_after = fs.used_blocks();
    assert_eq!(&used_after, used_before, "the used-block set is unchanged");
    let bs = 4096;
    let after = fs.block_mut().bytes();
    for block in used_after {
        let at = as_usize(block) * bs;
        assert_eq!(
            &after[at..at + bs],
            &before[at..at + bs],
            "block {block} of the committed volume changed"
        );
    }
}

fn scrub_full(fs: &mut ARXFS<MemBlock>) -> ScrubReport {
    fs.scrub(&GrantAll, &NullSink, ScrubBudget::Unlimited)
        .expect("scrub")
}

/// Populate a small volume with a directory tree, plain files, a pair of
/// identical-content files that dedupe, and a reflink, so scrub has metadata,
/// data, and shared chunks to verify.
fn populated() -> ARXFS<MemBlock> {
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
    // (`docs/src/filesystem/arxfs-spec.md` §12; the report is the only output,
    // never a silent mutation).
    let before = populated().into_block().expect("the volume closes").bytes();
    let mut fs =
        ARXFS::open(MemBlock::from_bytes(before.clone(), 4096, 512), &TEST_KEY).expect("reopen");

    let used_before = fs.used_blocks();
    let free_before = fs.free_count;
    let report = scrub_full(&mut fs);
    assert_eq!(report.pass, PassVerdict::Complete);
    assert!(!report.found_faults(), "{report:?}");
    assert_eq!(report.metadata_repaired, 0);
    assert_eq!(report.metadata_unrepairable, 0);
    assert_eq!(report.divergences_corrected, 0);
    assert!(report.metadata_blocks_checked > 0, "metadata was verified");
    assert!(report.data_blocks_checked > 0, "data was verified");
    assert!(report.claims_counted, "the claim counts were recomputed");

    // A clean scrub mutates nothing the committed volume depends on.
    assert_committed_state_unchanged(&before, &mut fs, &used_before, free_before);

    // Idempotent: a second scrub agrees.
    let after = fs.into_block().expect("the volume closes").bytes();
    let mut fs = ARXFS::open(MemBlock::from_bytes(after, 4096, 512), &TEST_KEY).expect("reopen2");
    let again = scrub_full(&mut fs);
    assert_eq!(again, report, "scrub is idempotent on a clean volume");
}

#[test]
fn scrub_requires_the_fs_mount_capability() {
    // Scrub is capability-gated like any privileged FS operation: without `CAP_FS_MOUNT` it fails closed and logs the
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
    // (`docs/src/filesystem/arxfs-spec.md` §8, §12).
    let mut fs = populated();
    // Target a directory data block: `open`'s free-space walk reads (and would
    // self-heal) every tree node, but it never reads directory *contents*, so
    // a wounded directory-block primary survives the mount for scrub to repair.
    let root_inode = fs.read_inode(ROOT_INO).expect("root inode");
    let target = fs.block_ptr(&root_inode, 0).expect("root dir block");
    assert_ne!(target, 0);
    let bs = 4096usize;
    let mut bytes = fs.into_block().expect("the volume closes").bytes();
    let off = as_usize(target) * bs + HEADER_LEN;
    let original = bytes[off];
    bytes[off] ^= 0xff; // wound only the primary copy

    let mut fs = ARXFS::open(MemBlock::from_bytes(bytes, 4096, 512), &TEST_KEY)
        .expect("mounts via the companion mirror");
    let report = scrub_full(&mut fs);
    assert_eq!(report.pass, PassVerdict::Complete);
    assert_eq!(report.metadata_repaired, 1, "{report:?}");
    assert_eq!(report.metadata_unrepairable, 0);

    // The primary copy is healed back to match its companion.
    let healed = fs.into_block().expect("the volume closes").bytes();
    let p = as_usize(target) * bs;
    let c = as_usize(target + 1) * bs;
    assert_eq!(healed[p..p + bs], healed[c..c + bs], "primary repaired");
    assert_eq!(healed[off], original, "the corrupted byte is restored");
}

#[test]
fn scrub_classifies_data_block_physical_and_logical_faults() {
    // Scrub runs every data block through the Stage 5/6 integrity pipeline and
    // classifies a failure by its layer without panicking
    // (`docs/src/filesystem/arxfs-spec.md` §6, §12). Deep data repair is a
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
        let mut bytes = fs.into_block().expect("the volume closes").bytes();
        bytes[as_usize(phys) * bs] ^= 0x01;
        let mut fs =
            ARXFS::open(MemBlock::from_bytes(bytes, 4096, 256), &TEST_KEY).expect("reopen");
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
        let mut bytes = fs.into_block().expect("the volume closes").bytes();
        bytes[base + hash_off] ^= 0x01;
        let fixed = physical_checksum(&bytes[base..base + csum_off]);
        bytes[base + csum_off..base + csum_off + PHYS_CHECKSUM_LEN].copy_from_slice(&fixed);
        let mut fs =
            ARXFS::open(MemBlock::from_bytes(bytes, 4096, 256), &TEST_KEY).expect("reopen");
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
    // (`docs/src/filesystem/arxfs-spec.md` §9, §12).
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
    fs.begin().expect("begin");
    fs.chunk_put(shared, &bumped).expect("put");
    fs.commit().expect("commit");
    assert_eq!(fs.data_refcount(shared).expect("refcount"), 5);

    let report = scrub_full(&mut fs);
    assert_eq!(report.pass, PassVerdict::Complete);
    assert!(report.refcount_divergences >= 1, "{report:?}");
    assert!(report.divergences_corrected >= 1, "{report:?}");
    assert_eq!(
        fs.data_refcount(shared).expect("refcount"),
        2,
        "scrub restored the refcount to the extent-derived truth"
    );

    // The correction holds across a remount and a re-scrub is clean.
    let bytes = fs.into_block().expect("the volume closes").bytes();
    let mut fs = ARXFS::open(MemBlock::from_bytes(bytes, 4096, 256), &TEST_KEY).expect("reopen");
    assert_eq!(fs.data_refcount(shared).expect("refcount"), 2);
    let again = scrub_full(&mut fs);
    assert_eq!(again.refcount_divergences, 0, "clean after correction");
    assert_eq!(again.divergences_corrected, 0);
}

/// Every reserved owner sentinel is distinct and sits outside the
/// inode-number range, so no two classes of metadata block seal themselves
/// under the same identity.
///
/// The allocation map and the scrub-progress record both used to stamp
/// `u64::MAX - 3`, declared as separate constants in separate files. Nothing
/// read the owner back, so nothing failed — but the field exists to say
/// *which object* a block belongs to, and two objects sharing an answer makes
/// it unable to. Deriving each sentinel from one enum's discriminant turns a
/// repeat into a compile error; this pins the range they must all stay in.
#[test]
fn every_reserved_owner_sentinel_is_distinct() {
    use crate::header::ReservedOwner::{
        AllocMap, ChunkTree, HealthBaseline, InodeTree, ReverseRefTree, ScratchClaims,
        ScratchFrontier, ScratchNames, ScratchReachable, ScrubProgress,
    };
    let owners = [
        InodeTree,
        ChunkTree,
        ReverseRefTree,
        ScrubProgress,
        HealthBaseline,
        AllocMap,
        ScratchClaims,
        ScratchReachable,
        ScratchFrontier,
        ScratchNames,
    ];
    let mut seen = BTreeSet::new();
    for owner in owners {
        let sentinel = owner.sentinel();
        assert!(
            seen.insert(sentinel),
            "{owner:?} reuses the sentinel {sentinel}"
        );
        assert!(
            sentinel > u64::from(u32::MAX),
            "{owner:?} sits inside the inode-number range"
        );
    }
}

/// Point a second file's extent at another file's block without touching the
/// chunk table, so the volume holds a claim the refcount does not know about.
fn claim_block_behind_the_refcount(fs: &mut ARXFS<MemBlock>, name: &[u8], phys: u64) {
    let root = fs.root();
    let ino = fs.lookup(root, name).expect("look up the claimant");
    let ino = fs.ino_of(ino).expect("inode number");
    let mut inode = fs.read_inode(ino).expect("read the claimant");
    fs.begin().expect("begin");
    fs.extent_assign(&mut inode, ino, 0, phys)
        .expect("assign the extent");
    fs.write_inode(ino, &inode).expect("write the claimant");
    fs.commit().expect("commit the extra claim");
}

/// A third extent claiming a shared block is counted, and the refcount is
/// raised to match rather than left below the truth.
///
/// This is the corruption class that costs data: a refcount below the number
/// of live claims frees the block while an extent still maps it. Detecting it
/// needs the *exact* claim count, which is why the reconcile streams one per
/// block through its scratch array — the referrer list alone cannot show that
/// a claim is missing from it.
#[test]
fn an_unlisted_claim_raises_the_refcount_to_the_counted_truth() {
    let mut fs = fmt(4096, 256, 64);
    let root = fs.root();
    let cap = as_usize(fs.data_capacity());
    for name in [b"a".as_slice(), b"b".as_slice(), b"c".as_slice()] {
        fs.create(root, name, NodeKind::RegularFile)
            .expect("create");
    }
    let body = alloc::vec![0x5Au8; cap];
    fs.write_at(root, b"a", 0, &body).expect("write a");
    fs.write_at(root, b"b", 0, &body).expect("write b");
    fs.write_at(root, b"c", 0, &alloc::vec![0x11u8; cap])
        .expect("write c");
    let shared = data_block_phys(&mut fs, b"a", 0);
    assert_eq!(fs.data_refcount(shared).expect("refcount"), 2);

    claim_block_behind_the_refcount(&mut fs, b"c", shared);
    assert_eq!(
        fs.data_refcount(shared).expect("refcount"),
        2,
        "the chunk record still says two"
    );

    let report = scrub_full(&mut fs);
    assert!(report.claims_counted);
    assert!(report.refcount_divergences >= 1, "{report:?}");
    assert_eq!(
        fs.data_refcount(shared).expect("refcount"),
        3,
        "the refcount rose to the counted truth"
    );
    let referrers = fs.reverse_refs(shared).expect("reverse refs");
    assert_eq!(referrers.len(), 3, "the missing referrer was recovered");
    let c_node = fs.lookup(root, b"c").expect("c");
    let c = fs.ino_of(c_node).expect("ino");
    assert!(referrers.contains(&(c, 0)), "{referrers:?}");

    let again = scrub_full(&mut fs);
    assert_eq!(again.refcount_divergences, 0, "clean after correction");
    assert_eq!(again.reverse_ref_divergences, 0);
}

/// Two extents claiming a block that has no chunk record at all is reported.
///
/// The record cannot be recreated here — a chunk's logical length and hash come
/// from its data, not from the extents — so the pass reports the divergence
/// instead of inventing one.
#[test]
fn a_block_claimed_twice_with_no_chunk_record_is_reported() {
    let mut fs = fmt(4096, 256, 64);
    let root = fs.root();
    let cap = as_usize(fs.data_capacity());
    for name in [b"a".as_slice(), b"c".as_slice()] {
        fs.create(root, name, NodeKind::RegularFile)
            .expect("create");
    }
    fs.write_at(root, b"a", 0, &alloc::vec![0x22u8; cap])
        .expect("write a");
    fs.write_at(root, b"c", 0, &alloc::vec![0x33u8; cap])
        .expect("write c");
    let lone = data_block_phys(&mut fs, b"a", 0);
    assert!(fs.chunk_get(lone).expect("get").is_none(), "not shared yet");

    claim_block_behind_the_refcount(&mut fs, b"c", lone);

    let report = scrub_full(&mut fs);
    assert!(report.claims_counted);
    assert!(report.refcount_divergences >= 1, "{report:?}");
    assert!(
        fs.chunk_get(lone).expect("get").is_none(),
        "no record was invented for it"
    );
}

/// An inode above the high-water mark the committed root records makes the
/// structural check fail closed rather than reclaim on the strength of a range
/// the inode falls outside.
///
/// The reachability array has one bit per allocated inode, so such an inode
/// cannot be recorded reachable — and if it is a directory, its own children
/// would then look like orphans and be freed. Both the root and the inode tree
/// are sealed under the same key, so the two disagreeing is a driver defect,
/// not a volume to repair.
#[test]
fn an_inode_above_the_high_water_mark_fails_the_check_closed() {
    let mut fs = fmt(4096, 256, 64);
    let root = fs.root();
    fs.create(root, b"live", NodeKind::RegularFile)
        .expect("create");
    let beyond = u32::try_from(fs.next_ino).expect("inode numbers are 32-bit") + 5;
    let node = fs.lookup(root, b"live").expect("look up");
    let ino = fs.ino_of(node).expect("inode number");
    let inode = fs.read_inode(ino).expect("read");
    fs.begin().expect("begin");
    fs.write_inode(beyond, &inode).expect("plant the inode");
    fs.commit().expect("commit the impossible inode");

    assert_eq!(
        fs.check(&GrantAll, &NullSink),
        Err(DriverError::DeviceFault),
        "an inode outside the allocated range is refused, not repaired"
    );
    // The live file is untouched: the failed pass rolled back.
    assert!(fs.lookup(fs.root(), b"live").is_ok());
}

/// A chunk record keyed on a block the volume does not have is stale, and is
/// removed.
///
/// Windows cover the device, so such a record falls outside every one of them;
/// nothing can claim a block that does not exist, which makes removing it exact
/// rather than a guess.
#[test]
fn a_chunk_record_beyond_the_last_block_is_removed() {
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
    let record = fs.chunk_get(shared).expect("get").expect("shared");

    let beyond = fs.total_blocks + 4096;
    fs.begin().expect("begin");
    fs.chunk_put(beyond, &record).expect("put");
    fs.commit().expect("commit the impossible record");

    let report = scrub_full(&mut fs);
    assert!(report.refcount_divergences >= 1, "{report:?}");
    assert!(
        fs.chunk_get(beyond).expect("get").is_none(),
        "the record for a block off the end of the device was removed"
    );
    assert_eq!(
        fs.chunk_get(shared).expect("get"),
        Some(record),
        "the live record is untouched"
    );
}

/// A chunk record no extent claims at all is stale, and is removed.
///
/// The old reconcile built its truth by walking the extents, so a block no
/// extent named never appeared in it and its record was invisible — leaving a
/// refcount that holds a block allocated forever.
#[test]
fn a_chunk_record_no_extent_claims_is_removed() {
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
    let record = fs.chunk_get(shared).expect("get").expect("shared");

    // A record keyed on a block nothing maps.
    let unclaimed = shared + 1;
    assert!(fs.chunk_get(unclaimed).expect("get").is_none());
    fs.begin().expect("begin");
    fs.chunk_put(unclaimed, &record).expect("put");
    fs.commit().expect("commit the stale record");

    let report = scrub_full(&mut fs);
    assert!(report.refcount_divergences >= 1, "{report:?}");
    assert!(report.divergences_corrected >= 1);
    assert!(
        fs.chunk_get(unclaimed).expect("get").is_none(),
        "the stale record was removed"
    );
    assert_eq!(
        fs.chunk_get(shared).expect("get"),
        Some(record),
        "the live record is untouched"
    );
}

/// A reconcile that has to cover the volume in several scratch windows reaches
/// exactly the report one window reaches.
///
/// The window count is set by the run the volume can spare, so the same volume
/// must be reconciled identically however the array was sized — otherwise a
/// full volume would silently get a weaker check than an empty one.
#[test]
fn a_windowed_reconcile_reaches_the_same_report_as_a_single_window() {
    // 512-byte blocks put 768 claim counts in a page, so this volume needs
    // several pages and a one-page array covers it in several windows.
    let mut fs = fmt(512, 2400, 64);
    let root = fs.root();
    let cap = as_usize(fs.data_capacity());
    let body = alloc::vec![0x71u8; cap];
    for name in [b"a".as_slice(), b"b".as_slice()] {
        fs.create(root, name, NodeKind::RegularFile)
            .expect("create");
        fs.write_at(root, name, 0, &body).expect("write");
    }
    let shared = data_block_phys(&mut fs, b"a", 0);
    let record = fs.chunk_get(shared).expect("get").expect("shared");
    fs.begin().expect("begin");
    fs.chunk_put(
        shared,
        &ChunkRecord {
            refcount: 6,
            ..record
        },
    )
    .expect("put");
    fs.commit().expect("commit the tampered refcount");
    let tampered = fs.into_block().expect("the volume closes").bytes();

    let mut single =
        ARXFS::open(MemBlock::from_bytes(tampered.clone(), 512, 2400), &TEST_KEY).expect("open");
    let one_window = scrub_full(&mut single);
    assert!(one_window.claims_counted);
    assert!(one_window.refcount_divergences >= 1, "{one_window:?}");
    assert_eq!(single.data_refcount(shared).expect("refcount"), 2);

    let mut windowed =
        ARXFS::open(MemBlock::from_bytes(tampered, 512, 2400), &TEST_KEY).expect("open");
    let mut report = ScrubReport::default();
    windowed.begin().expect("begin");
    let corrected = windowed
        .reconcile_refcounts_in_windows(1, &mut report)
        .expect("windowed reconcile");
    assert!(corrected);
    windowed.commit().expect("commit the windowed corrections");
    assert!(
        report.refcount_divergences >= 1 && report.divergences_corrected >= 1,
        "{report:?}"
    );
    assert_eq!(windowed.data_refcount(shared).expect("refcount"), 2);
    assert_eq!(
        (
            report.refcount_divergences,
            report.reverse_ref_divergences,
            report.divergences_corrected
        ),
        (
            one_window.refcount_divergences,
            one_window.reverse_ref_divergences,
            one_window.divergences_corrected
        ),
        "windowed: {report:?} single: {one_window:?}"
    );
}

/// A volume with no room for a scratch array reports what it could not verify
/// rather than reporting a soundness nothing established.
///
/// This is the honest end of the "grow before you fail" rule: the pass takes at
/// most a share of the free space so it never squeezes a running system, and
/// where even that will not fit it does the half that needs no array and says
/// the other half did not run. Guessing a refcount from a partial count would
/// be worse than not counting: a refcount lowered wrongly frees a block a live
/// extent still maps.
#[test]
fn a_volume_with_no_room_for_a_scratch_array_says_so() {
    let mut fs = fmt(512, 96, 64);
    let root = fs.root();
    let body = alloc::vec![0x5Au8; 4096];
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
            Ok(_) => idx += 1,
            Err(DriverError::NoSpace) => break,
            Err(e) => panic!("unexpected {e:?}"),
        }
        assert!(idx <= 10_000, "never ran out of space");
    }
    assert!(idx > 0, "the volume holds something to verify");
    // Then take the last of it a block at a time, down to the metadata reserve
    // the allocator keeps back so a shrinking transaction can still
    // copy-on-write itself.
    let mut at = 0;
    while fs.write_at(root, b"f0", at, &[0x11]).is_ok() {
        at += 512;
        assert!(at < 1 << 20, "never ran out of space");
    }
    assert!(fs.free_count <= 17, "free after filling: {}", fs.free_count);

    let report = scrub_full(&mut fs);
    assert_eq!(report.pass, PassVerdict::Complete, "{report:?}");
    assert!(
        !report.claims_counted,
        "a full volume cannot spare a run, and must say so: {report:?}"
    );
    assert_eq!(
        report.divergences_corrected, 0,
        "no correction is made from a truth the pass does not have"
    );
    assert!(report.metadata_blocks_checked > 0, "verification still ran");

    let check = check_full(&mut fs);
    assert_eq!(
        check.structure,
        StructureVerdict::NotWalked,
        "nothing walked the structure, so nothing vouches for it: {check:?}"
    );
    assert_eq!(
        check.orphaned_inodes, 0,
        "and nothing was reclaimed on a guess"
    );
    assert!(check.rebuilt_derived_state, "the rebuild still ran");
}

/// A read-only mount cannot place a scratch array, so it says so and corrects
/// nothing — and still writes not one block.
#[test]
fn a_read_only_scrub_counts_no_claims_and_writes_nothing() {
    let bytes = populated().into_block().expect("the volume closes").bytes();
    let mut fs = ARXFS::open_read_only(MemBlock::from_bytes(bytes.clone(), 4096, 512), &TEST_KEY)
        .expect("read-only mount");
    let report = scrub_full(&mut fs);
    assert_eq!(report.pass, PassVerdict::Complete);
    assert!(
        !report.claims_counted,
        "a read-only handle has no allocator, so it counts nothing"
    );
    assert_eq!(report.divergences_corrected, 0, "{report:?}");
    assert!(!report.found_faults(), "{report:?}");
    let after = fs.into_block().expect("the volume closes");
    assert_eq!(after.writes, 0, "a read-only scrub issues no write");
    assert_eq!(after.bytes(), bytes, "the device is untouched");
}

/// The pass that counted no claims reports every divergence it can see and
/// still writes nothing, even one it would have corrected had it counted.
///
/// This is the invariant that keeps a read-only handle — the one that can never
/// place a scratch array — off the write path entirely, rather than relying on
/// each repair site to remember to check.
#[test]
fn a_pass_that_counted_no_claims_reports_a_divergence_without_writing() {
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
    let record = fs.chunk_get(shared).expect("get").expect("shared");
    fs.begin().expect("begin");
    fs.chunk_put(
        shared,
        &ChunkRecord {
            refcount: 5,
            ..record
        },
    )
    .expect("put");
    // A record for a block the volume does not have: the counted pass removes
    // it, so this proves the uncounted one leaves even that alone.
    fs.chunk_put(fs.total_blocks + 8, &record).expect("put");
    fs.commit().expect("commit the tampered records");
    let bytes = fs.into_block().expect("the volume closes").bytes();

    let mut fs = ARXFS::open_read_only(MemBlock::from_bytes(bytes.clone(), 4096, 256), &TEST_KEY)
        .expect("read-only mount");
    let report = scrub_full(&mut fs);
    assert!(!report.claims_counted, "{report:?}");
    assert!(
        report.refcount_divergences >= 1,
        "the divergence is still reported: {report:?}"
    );
    assert_eq!(
        report.divergences_corrected, 0,
        "and nothing is corrected from a truth the pass does not have"
    );
    let after = fs.into_block().expect("the volume closes");
    assert_eq!(after.writes, 0, "no write is even attempted");
    assert_eq!(after.bytes(), bytes, "the device is untouched");
}

/// Wound one physical copy of a live metadata block and scrub a read-only
/// mount: the damaged mirror is reported, and the medium is not written.
///
/// The read-only rule was observed at the repair-on-read sites and missing from
/// scrub's own copy-repair, so a mount held read-only precisely because its
/// medium must not be touched wrote to it on the first repairable block.
#[test]
fn a_read_only_scrub_reports_a_damaged_mirror_without_repairing_it() {
    let mut fs = populated();
    // A directory data block: the mount's own reads never reach directory
    // contents, so a wounded primary survives to be scrubbed.
    let root_inode = fs.read_inode(ROOT_INO).expect("root inode");
    let target = fs.block_ptr(&root_inode, 0).expect("root dir block");
    assert_ne!(target, 0);
    let bs = 4096usize;
    let mut bytes = fs.into_block().expect("the volume closes").bytes();
    bytes[as_usize(target) * bs + HEADER_LEN] ^= 0xff;

    let mut fs = ARXFS::open_read_only(MemBlock::from_bytes(bytes.clone(), 4096, 512), &TEST_KEY)
        .expect("mounts via the companion mirror");
    let report = scrub_full(&mut fs);
    assert_eq!(report.pass, PassVerdict::Complete, "{report:?}");
    assert_eq!(
        report.metadata_damaged, 1,
        "the damaged mirror is reported: {report:?}"
    );
    assert_eq!(
        report.metadata_repaired, 0,
        "never as a repair that did not happen: {report:?}"
    );
    assert_eq!(report.metadata_unrepairable, 0, "{report:?}");
    assert!(report.found_faults(), "a damaged mirror is a finding");

    let after = fs.into_block().expect("the volume closes");
    assert_eq!(after.writes, 0, "a read-only scrub issues no write");
    assert_eq!(after.bytes(), bytes, "the wounded copy is left as it is");
}

/// A bounded read-only scrub reports the chunk it verified instead of failing
/// at the cursor it may not persist, and says the position was not kept.
///
/// Persisting the cursor allocates, which a read-only handle refuses, so the
/// whole call failed after doing its work — and the maintenance runner drives
/// exactly this call. A pass that keeps no position is a different audit fact
/// from one that will be resumed: repeating it never reaches past its budget.
#[test]
fn a_read_only_bounded_scrub_reports_progress_without_persisting_a_cursor() {
    let bytes = populated().into_block().expect("the volume closes").bytes();
    let mut fs = ARXFS::open_read_only(MemBlock::from_bytes(bytes.clone(), 4096, 512), &TEST_KEY)
        .expect("read-only mount");
    let sink = RecordingSink::new();
    let report = fs
        .scrub(&GrantAll, &sink, ScrubBudget::Inodes(1))
        .expect("a bounded read-only scrub reports rather than failing");
    assert_eq!(
        report.pass,
        PassVerdict::Stopped,
        "the budget stopped the pass and kept no cursor: {report:?}"
    );
    assert!(
        report.metadata_blocks_checked > 0,
        "and it verified a chunk first: {report:?}"
    );
    assert_eq!(fs.scrub_progress_root, 0, "no cursor was persisted");
    assert!(sink.saw(scrub::SCRUB_STOPPED), "the stop is logged as such");
    assert!(
        !sink.saw(scrub::SCRUB_PAUSED),
        "never as a pause a later call resumes"
    );
    let after = fs.into_block().expect("the volume closes");
    assert_eq!(after.writes, 0, "and no write was issued");
    assert_eq!(after.bytes(), bytes, "the device is untouched");
}

/// A read-only scrub that finishes a pass a read-write mount had paused leaves
/// the progress record the committed root still names.
///
/// Clearing it freed metadata and committed, which a read-only handle refuses,
/// failing a scrub that had already completed its verification; and dropping
/// the reference in memory alone would hide a record the volume still names.
#[test]
fn a_read_only_scrub_leaves_a_paused_cursor_the_volume_still_names() {
    let mut fs = populated();
    fs.scrub(&GrantAll, &NullSink, ScrubBudget::Inodes(1))
        .expect("pause a scrub");
    let paused_at = fs.scrub_progress_root;
    assert_ne!(paused_at, 0, "a read-write pass persisted its cursor");
    let bytes = fs.into_block().expect("the volume closes").bytes();

    let mut fs = ARXFS::open_read_only(MemBlock::from_bytes(bytes.clone(), 4096, 512), &TEST_KEY)
        .expect("read-only mount");
    assert_eq!(fs.scrub_progress_root, paused_at, "the record is adopted");
    let report = scrub_full(&mut fs);
    assert_eq!(
        report.pass,
        PassVerdict::Complete,
        "the resumed pass finished: {report:?}"
    );
    assert_eq!(
        fs.scrub_progress_root, paused_at,
        "and left the record the committed root still names"
    );
    let after = fs.into_block().expect("the volume closes");
    assert_eq!(after.writes, 0, "no write was issued");
    assert_eq!(after.bytes(), bytes, "the device is untouched");
}

/// A read-only mount refuses `check` before it touches anything, completing the
/// guarantee across every maintenance operation: `scrub` and `health` verify
/// and report, `trim` and `check` are refused, and none of the four writes.
///
/// `check`'s first act is to rebuild the free-space derivation, which a
/// read-only handle has no allocator for, so it fails closed there rather than
/// part-way through a repair it cannot finish.
#[test]
fn a_read_only_mount_refuses_check_before_touching_the_device() {
    let bytes = populated().into_block().expect("the volume closes").bytes();
    let mut fs = ARXFS::open_read_only(MemBlock::from_bytes(bytes.clone(), 4096, 512), &TEST_KEY)
        .expect("read-only mount");
    assert_eq!(
        fs.check(&GrantAll, &NullSink),
        Err(DriverError::PermissionDenied)
    );
    let after = fs.into_block().expect("the volume closes");
    assert_eq!(after.writes, 0, "no write was issued");
    assert_eq!(after.bytes(), bytes, "the device is untouched");
}

/// A read-only mount refuses the discard sweep before it touches anything: a
/// discard is destructive and irreversible, so a medium whose state is in doubt
/// never receives one.
#[test]
fn a_read_only_mount_never_trims() {
    let bytes = populated().into_block().expect("the volume closes").bytes();
    let mut fs = ARXFS::open_read_only(
        MemBlock::from_bytes(bytes.clone(), 4096, 512).with_discard(1, 0),
        &TEST_KEY,
    )
    .expect("read-only mount");
    assert_eq!(
        fs.trim(&GrantAll, &NullSink),
        Err(DriverError::PermissionDenied)
    );
    let after = fs.into_block().expect("the volume closes");
    assert!(
        after.discarded.is_empty(),
        "not one range was discarded: {:?}",
        after.discarded
    );
    assert_eq!(after.writes, 0, "and no write was issued");
    assert_eq!(after.bytes(), bytes, "the device is untouched");
}

/// A directory scan reused across directories reads each one's own blocks.
///
/// The cursor keeps one directory block resident, so its identity has to be
/// dropped when the scan is re-seeked: a block index means nothing without the
/// directory it came from, and keeping the previous one made the second
/// directory read back the first one's entries.
#[test]
fn a_directory_scan_reused_across_directories_reads_each_ones_entries() {
    let mut fs = fmt(4096, 256, 64);
    let root = fs.root();
    fs.create(root, b"d1", NodeKind::Directory).expect("d1");
    fs.create(root, b"d2", NodeKind::Directory).expect("d2");
    let d1 = fs.lookup(root, b"d1").expect("d1");
    let d2 = fs.lookup(root, b"d2").expect("d2");
    fs.create(d1, b"only-in-one", NodeKind::RegularFile)
        .expect("x");
    fs.create(d2, b"only-in-two", NodeKind::RegularFile)
        .expect("y");

    let d1_ino = fs.ino_of(d1).expect("ino");
    let d2_ino = fs.ino_of(d2).expect("ino");
    let d1 = fs.read_inode(d1_ino).expect("inode");
    let d2 = fs.read_inode(d2_ino).expect("inode");
    let mut scan = DirScan::new(4096).expect("scan");
    let mut first = alloc::vec::Vec::new();
    while fs.dir_next(&d1, &mut scan).expect("step").is_some() {
        first.push(scan.name().to_vec());
    }
    scan.seek(0);
    let mut second = alloc::vec::Vec::new();
    while fs.dir_next(&d2, &mut scan).expect("step").is_some() {
        second.push(scan.name().to_vec());
    }
    assert!(first.contains(&b"only-in-one".to_vec()), "{first:?}");
    assert!(second.contains(&b"only-in-two".to_vec()), "{second:?}");
    assert!(
        !second.contains(&b"only-in-one".to_vec()),
        "the second directory read the first one's block: {second:?}"
    );
}

/// A free run is found across a bitmap-page boundary, and a wholly-used page
/// ends the run rather than being walked block by block.
#[test]
fn a_free_run_is_found_across_bitmap_page_boundaries() {
    // 512-byte blocks give 3072 bits per bitmap page, so this volume spans
    // several pages and a run can straddle one.
    let mut fs = fmt(512, 9000, 64);
    let total = fs.total_blocks;
    let bits_per_page = (512 - HEADER_LEN) as u64 * 8;
    fs.mark_range_used(0, total).expect("fill the volume");
    assert_eq!(
        fs.map_find_free_run(1, 0, total).expect("search"),
        None,
        "a full volume has no run"
    );

    // A run that starts in one page and ends in the next.
    let straddle = bits_per_page - 4;
    fs.mark_range_free(straddle, 8).expect("free a run");
    assert_eq!(
        fs.map_find_free_run(8, 0, total).expect("search"),
        Some(straddle),
        "the run straddling the page boundary is found whole"
    );
    assert_eq!(
        fs.map_find_free_run(9, 0, total).expect("search"),
        None,
        "and is not reported as longer than it is"
    );

    // A second, longer run further on: the shorter one no longer satisfies the
    // request, and the search carries on past it rather than stopping.
    let later = bits_per_page * 2 + 100;
    fs.mark_range_free(later, 20).expect("free a longer run");
    assert_eq!(
        fs.map_find_free_run(20, 0, total).expect("search"),
        Some(later)
    );
    assert_eq!(
        fs.map_find_free_run(8, 0, total).expect("search"),
        Some(straddle),
        "the lowest run that fits still wins"
    );
    // A window that excludes both runs finds nothing.
    assert_eq!(
        fs.map_find_free_run(8, 0, straddle + 4).expect("search"),
        None,
        "a run is never reported past the window's end"
    );
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
    fs.begin().expect("begin");
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
    assert_eq!(report.pass, PassVerdict::Complete);
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
    // (`docs/src/filesystem/arxfs-spec.md` §12).
    let base = populated().into_block().expect("the volume closes").bytes();

    // Uninterrupted reference pass.
    let mut whole = ARXFS::open(MemBlock::from_bytes(base.clone(), 4096, 512), &TEST_KEY)
        .expect("reopen whole");
    let reference = scrub_full(&mut whole);
    assert_eq!(reference.pass, PassVerdict::Complete);

    // Resumed pass: one inode per call until it completes.
    let mut fs =
        ARXFS::open(MemBlock::from_bytes(base, 4096, 512), &TEST_KEY).expect("reopen stepwise");
    let mut calls = 0;
    let last = loop {
        let report = fs
            .scrub(&GrantAll, &NullSink, ScrubBudget::Inodes(1))
            .expect("scrub step");
        calls += 1;
        if report.pass == PassVerdict::Complete {
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
    // ordinary recovery never needs scrub (`docs/src/filesystem/arxfs-spec.md`
    // §4, §14). The half-done scrub then resumes to completion.
    let mut fs = populated();
    let paused = fs
        .scrub(&GrantAll, &NullSink, ScrubBudget::Inodes(1))
        .expect("first step");
    assert_eq!(paused.pass, PassVerdict::Paused);
    assert_ne!(fs.scrub_progress_root, 0);

    // Simulate a crash: drop the in-memory state and reopen from disk.
    let bytes = fs.into_block().expect("the volume closes").bytes();
    let mut fs = ARXFS::open(MemBlock::from_bytes(bytes, 4096, 512), &TEST_KEY)
        .expect("a volume with a scrub in progress still mounts");
    assert_ne!(fs.scrub_progress_root, 0, "the cursor survived the crash");

    // The file system is fully usable, and the scrub resumes and completes.
    let root = fs.root();
    assert!(fs.lookup(root, b"plain").is_ok());
    let report = fs
        .scrub(&GrantAll, &NullSink, ScrubBudget::Unlimited)
        .expect("resume");
    assert_eq!(report.pass, PassVerdict::Complete);
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
    assert_eq!(report.pass, PassVerdict::Complete, "{report:?}");
    assert!(!report.found_faults(), "{report:?}");

    // Remount, then rewrite one sharer (copy-on-write off the shared chunk).
    let bytes = fs.into_block().expect("the volume closes").bytes();
    let mut fs = ARXFS::open(MemBlock::from_bytes(bytes, 4096, 512), &TEST_KEY).expect("reopen");
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
    assert_eq!(report.pass, PassVerdict::Complete, "{report:?}");
    assert!(!report.found_faults(), "{report:?}");
    let node_a = fs.lookup(root, b"a").expect("lookup a");
    assert_eq!(read_all(&mut fs, node_a, cap), replacement);
    let node_b = fs.lookup(root, b"b").expect("lookup b");
    assert_eq!(read_all(&mut fs, node_b, cap), body);
}

// ---------------------------------------------------------------------------
// Stage 9: offline check and rescue.
// ---------------------------------------------------------------------------

use crate::check::{self, RescueSink, StructureVerdict};
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
fn check_full(fs: &mut ARXFS<MemBlock>) -> CheckReport {
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
    // — running it again is identical (`docs/src/filesystem/arxfs-spec.md`
    // §12).
    let before = populated().into_block().expect("the volume closes").bytes();
    let mut fs =
        ARXFS::open(MemBlock::from_bytes(before.clone(), 4096, 512), &TEST_KEY).expect("reopen");

    let used_before = fs.used_blocks();
    let free_before = fs.free_count;
    let sink = RecordingSink::new();
    let report = fs.check(&GrantAll, &sink).expect("check");
    assert!(report.complete);
    assert_eq!(report.structure, StructureVerdict::Sound, "{report:?}");
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

    // A clean check mutates nothing the committed volume depends on.
    assert_committed_state_unchanged(&before, &mut fs, &used_before, free_before);

    // Idempotent: a second check agrees.
    let after = fs.into_block().expect("the volume closes").bytes();
    let mut fs = ARXFS::open(MemBlock::from_bytes(after, 4096, 512), &TEST_KEY).expect("reopen2");
    let again = check_full(&mut fs);
    assert_eq!(again, report, "check is idempotent on a clean volume");
}

#[test]
fn check_rebuilds_a_corrupt_free_space_derivation() {
    // The allocation map is rebuildable derived state, never authoritative. A
    // corrupt derivation must never keep a sound volume unmountable: check
    // rebuilds it from the authoritative trees, and the result matches a
    // freshly mounted reference.
    let bytes = populated().into_block().expect("the volume closes").bytes();
    let mut reference =
        ARXFS::open(MemBlock::from_bytes(bytes.clone(), 4096, 512), &TEST_KEY).expect("reference");
    let good_used = reference.used_blocks();
    let good_count = reference.free_count;

    let mut fs = ARXFS::open(MemBlock::from_bytes(bytes, 4096, 512), &TEST_KEY).expect("reopen");
    // Wreck the derived state: release a block the trees say is live, and
    // report a free count no allocation could satisfy.
    let live = *good_used.iter().next_back().expect("a used block");
    fs.mark_run_free(live, 1);
    fs.free_count = 0;

    let report = check_full(&mut fs);
    assert!(report.complete);
    assert!(report.rebuilt_derived_state);
    assert_eq!(
        fs.used_blocks(),
        good_used,
        "the allocation map was rebuilt"
    );
    assert_eq!(fs.free_count, good_count, "the free count was rebuilt");
    // The volume is mountable and the structure is sound.
    assert_eq!(report.structure, StructureVerdict::Sound, "{report:?}");
    let bytes = fs.into_block().expect("the volume closes").bytes();
    assert!(ARXFS::open(MemBlock::from_bytes(bytes, 4096, 512), &TEST_KEY).is_ok());
}

#[test]
fn check_reclaims_an_orphaned_inode() {
    // An inode no directory reaches is an orphan. Check detects it, reclaims it
    // (freeing its slot), and the volume stays sound
    // (`docs/src/filesystem/arxfs-spec.md` §12).
    let mut fs = fmt(4096, 512, 128);
    let root = fs.root();
    fs.create(root, b"keep", NodeKind::RegularFile)
        .expect("create keep");

    // Inject an orphan: allocate an inode and never link it into any directory.
    fs.begin().expect("begin");
    let sec = Security::new(0o644, 0, 0);
    let orphan = fs
        .alloc_inode(&Inode::empty(InodeKind::File, sec, fixed_clock()))
        .expect("alloc orphan");
    fs.commit().expect("commit orphan");
    assert!(fs.read_inode(orphan).is_ok(), "the orphan exists pre-check");

    let report = check_full(&mut fs);
    assert!(report.complete);
    assert_eq!(report.orphaned_inodes, 1, "{report:?}");
    assert_eq!(report.orphans_reclaimed, 1);
    assert!(report.made_repairs());
    assert_eq!(
        report.structure,
        StructureVerdict::Sound,
        "the orphan was safely reclaimed"
    );

    // The orphan is gone, and the named file is untouched.
    assert_eq!(fs.read_inode(orphan), Err(DriverError::NotFound));
    assert!(fs.lookup(fs.root(), b"keep").is_ok());

    // A re-check finds nothing left to reclaim.
    let again = check_full(&mut fs);
    assert_eq!(again.orphans_reclaimed, 0);
    assert_eq!(again.structure, StructureVerdict::Sound);
}

#[test]
fn check_corrects_a_refcount_divergence_and_reports_what_it_cannot_fix() {
    // Check reuses the scrub verification core: it corrects a refcount
    // divergence it can fix, and reports a data integrity fault it cannot
    // safely repair as an unrecoverable finding
    // (`docs/src/filesystem/arxfs-spec.md` §9, §12).
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
    fs.begin().expect("begin");
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
    // ignored or pretended fixed.
    let mut fs = fmt(4096, 256, 64);
    let root = fs.root();
    fs.create(root, b"f", NodeKind::RegularFile)
        .expect("create");
    fs.write_at(root, b"f", 0, &alloc::vec![0x33u8; 400])
        .expect("write");
    let phys = data_block_phys(&mut fs, b"f", 0);
    let bs = 4096usize;
    let mut bytes = fs.into_block().expect("the volume closes").bytes();
    bytes[as_usize(phys) * bs] ^= 0x01; // wound the ciphertext (physical fault)

    let mut fs = ARXFS::open(MemBlock::from_bytes(bytes, 4096, 256), &TEST_KEY).expect("reopen");
    let report = check_full(&mut fs);
    assert!(report.complete);
    assert_eq!(report.verification.data_physical_faults, 1, "{report:?}");
    assert_eq!(
        report.structure,
        StructureVerdict::Unsound,
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
    let bytes = populated().into_block().expect("the volume closes").bytes();
    let sink = RecordingSink::new();
    let mut out = CollectSink::new();
    assert_eq!(
        ARXFS::rescue(
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
    // (`docs/src/filesystem/arxfs-spec.md` §12).
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
    let mut bytes = fs.into_block().expect("the volume closes").bytes();

    // The wounded ring makes an ordinary mount fail closed.
    damage_superblock_ring(&mut bytes, bs);
    assert!(
        ARXFS::open(MemBlock::from_bytes(bytes.clone(), 4096, 512), &TEST_KEY).is_err(),
        "the damaged ring no longer mounts normally"
    );

    let sink = RecordingSink::new();
    let mut out = CollectSink::new();
    let report = ARXFS::rescue(
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
    let after = ARXFS::rescue(
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
    // (`docs/src/filesystem/arxfs-spec.md` §6, §12). The good block of the
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
    let mut bytes = fs.into_block().expect("the volume closes").bytes();

    // Wound the second block's ciphertext (a physical-checksum fault) and the
    // ring (so rescue is the recovery path).
    bytes[as_usize(bad_phys) * bs] ^= 0x01;
    damage_superblock_ring(&mut bytes, bs);

    let mut out = CollectSink::new();
    let report = ARXFS::rescue(
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
// Stage: TRIM/discard (return freed space to the device, safely;).
// ---------------------------------------------------------------------------

/// Format a fresh volume on a discard-capable [`MemBlock`] and clear the
/// record of the mkfs-time discard so a trim test starts from a clean slate.
fn fmt_discard(
    block_count: u64,
    granularity_blocks: u64,
    max_blocks_per_request: u64,
) -> ARXFS<MemBlock> {
    let block =
        MemBlock::new(512, block_count).with_discard(granularity_blocks, max_blocks_per_request);
    let mut fs = ARXFS::format(block, 32, &TEST_KEY, &mut TestEntropy::new())
        .expect("format a discard-capable device")
        .with_clock(fixed_clock);
    fs.block.discarded.clear();
    fs
}

/// Enqueue every block in `[start, end)` for discard, asserting each is
/// currently free so the test models the real invariant (only free blocks are
/// ever queued).
fn enqueue_free_range(fs: &mut ARXFS<MemBlock>, start: u64, end: u64) {
    for block in start..end {
        assert!(!fs.is_used(block), "test block {block} must start free");
        fs.enqueue_discard_run(block, 1);
    }
}

#[test]
fn trim_requires_the_fs_mount_capability() {
    // Fail closed: without CAP_FS_MOUNT trim refuses, logs the refusal, and
    // leaves the queue untouched.
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
    // and reports `supported = false`. There is no trim=off mode.
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
        assert!(!fs.is_used(block));
        fs.enqueue_discard_run(block, 1);
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
    // fall outside the aligned window are requeued for a later pass.
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
    // skipped, never discarded — discard can never touch live data.
    let mut fs = fmt_discard(512, 1, 0);
    assert!(!fs.is_used(100));
    fs.enqueue_discard_run(100, 1);
    fs.mark_run_used(100, 1); // the block is handed back out before trim runs.
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
    // queued and a second trim pass drains them.
    let mut fs = fmt_discard(2048, 1, 0);
    let runs = discard::TRIM_BATCH_RANGES + 1;
    let mut blocks = alloc::vec::Vec::new();
    for run in 0..runs as u64 {
        let block = 100 + run * 2; // gaps keep every block its own run.
        assert!(!fs.is_used(block));
        fs.enqueue_discard_run(block, 1);
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
    // encrypted structures are laid down (mkfs flow).
    let block = MemBlock::new(512, 512).with_discard(1, 0);
    let fs = ARXFS::format(block, 32, &TEST_KEY, &mut TestEntropy::new()).expect("format");
    assert_eq!(
        fs.into_block().expect("the volume closes").discarded,
        alloc::vec![(0, 512)],
        "the full block range is discarded once at mkfs time"
    );
}

#[test]
fn mkfs_on_a_device_without_discard_still_formats() {
    // A device without discard support is recorded, not failed: format still
    // succeeds and the volume mounts.
    let fs = ARXFS::format(
        MemBlock::new(512, 512),
        32,
        &TEST_KEY,
        &mut TestEntropy::new(),
    )
    .expect("format");
    assert!(fs
        .into_block()
        .expect("the volume closes")
        .discarded
        .is_empty());
}

#[test]
fn trim_never_discards_a_block_still_shared_by_dedupe() {
    // The hard constraint, end-to-end: a data block shared by two files
    // (dedupe refcount 2) is not freed when one sharer is removed — refcount
    // falls to 1, the block stays reachable — so trim must never discard it and
    // the surviving file must still read back.
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
        fs.is_used(shared),
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
    // The pending-discard queue is rebuildable, transient state: a crash
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
    let bytes = fs.into_block().expect("the volume closes").bytes();
    let mut fs = ARXFS::open(MemBlock::from_bytes(bytes, 512, 512), &TEST_KEY).expect("remount");
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
fn fmt_health(block_count: u64, health: DeviceHealth) -> ARXFS<MemBlock> {
    ARXFS::format(
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
) -> ARXFS<MemBlock> {
    ARXFS::open(
        MemBlock::from_bytes(bytes, 4096, block_count).with_health(health),
        &TEST_KEY,
    )
    .expect("reopen with health")
}

#[test]
fn health_requires_the_fs_mount_capability() {
    // Reading health (which may trigger a scrub) is capability-gated like the
    // other privileged FS operations: without
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
    // signal for (`HealthUnavailable`).
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
    let bytes = fs.into_block().expect("the volume closes").bytes();
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
    // failing (no magic numbers — see `HealthThresholds::DEFAULT`).
    let t = HealthThresholds::DEFAULT;

    let mut fs = fmt_health(256, DeviceHealth::Available(healthy_snapshot(0, 0)));
    let report = fs.health(&GrantAll, &NullSink).expect("health clean");
    assert_eq!(report.state, HealthState::Healthy, "{report:?}");
    let bytes = fs.into_block().expect("the volume closes").bytes();

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
    let bytes = fs.into_block().expect("the volume closes").bytes();

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
    // metadata scrub, run through the Stage-8 machinery (— no parallel verifier). Once the baseline advances, a pass with no
    // further delta does not re-scrub.
    let mut fs = fmt_health(256, DeviceHealth::Available(healthy_snapshot(0, 0)));
    fs.health(&GrantAll, &NullSink).expect("establish baseline");
    let bytes = fs.into_block().expect("the volume closes").bytes();

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
    let bytes = fs.into_block().expect("the volume closes").bytes();
    let mut fs = open_health(bytes, 256, DeviceHealth::Available(healthy_snapshot(3, 0)));
    let report = fs.health(&GrantAll, &NullSink).expect("health 2");
    assert_eq!(report.unsafe_shutdown_delta, 0);
    assert!(report.scrub.is_none(), "no new delta, no scrub");
    assert_eq!(report.scrubs_triggered, 1, "the lifetime count persisted");
}

/// Reopen a 4096-byte volume from `bytes` read-only, reporting `health`.
fn open_health_read_only(
    bytes: alloc::vec::Vec<u8>,
    block_count: u64,
    health: DeviceHealth,
) -> ARXFS<MemBlock> {
    ARXFS::open_read_only(
        MemBlock::from_bytes(bytes, 4096, block_count).with_health(health),
        &TEST_KEY,
    )
    .expect("read-only mount with health")
}

/// A read-only health pass returns the reading it took and stores no baseline.
///
/// It read the telemetry, compared the baseline, classified the volume, and
/// then died at the baseline commit — throwing away a valid reading it already
/// held.
#[test]
fn a_read_only_health_pass_returns_its_reading_and_stores_no_baseline() {
    let mut fs = fmt_health(256, DeviceHealth::Available(healthy_snapshot(0, 0)));
    fs.health(&GrantAll, &NullSink)
        .expect("establish a baseline");
    let bytes = fs.into_block().expect("the volume closes").bytes();

    let mut fs = open_health_read_only(
        bytes.clone(),
        256,
        DeviceHealth::Available(healthy_snapshot(3, 0)),
    );
    let baseline_at = fs.health_baseline_root;
    assert_ne!(baseline_at, 0, "the stored baseline is adopted");
    let report = fs
        .health(&GrantAll, &NullSink)
        .expect("a read-only health pass reports rather than failing");
    assert_eq!(report.unsafe_shutdown_delta, 3, "{report:?}");
    assert!(report.metadata_scrub_recommended, "{report:?}");
    assert!(report.scrub.is_some(), "the recommendation was acted on");
    assert!(report.device.is_some(), "the telemetry is reported");
    assert_eq!(report.state, HealthState::Healthy, "{report:?}");
    assert_eq!(
        fs.health_baseline_root, baseline_at,
        "the stored baseline is untouched"
    );
    let after = fs.into_block().expect("the volume closes");
    assert_eq!(after.writes, 0, "a read-only health pass issues no write");
    assert_eq!(after.bytes(), bytes, "the device is untouched");
}

/// A mirror the read-only scrub found damaged but may not rewrite classifies
/// the volume exactly as a repaired one would.
///
/// A copy that went bad is the same medium signal either way, and it can never
/// enter the durable history because the handle that observes it stores none —
/// so without reaching the classification directly the finding would be lost
/// and the volume would read as healthy.
#[test]
fn a_read_only_volume_with_a_damaged_mirror_classifies_degraded() {
    let mut fs = fmt_health(512, DeviceHealth::Available(healthy_snapshot(0, 0)));
    let root = fs.root();
    fs.create(root, b"f", NodeKind::RegularFile)
        .expect("create");
    fs.health(&GrantAll, &NullSink)
        .expect("establish a baseline");
    let root_inode = fs.read_inode(ROOT_INO).expect("root inode");
    let target = fs.block_ptr(&root_inode, 0).expect("root dir block");
    let bs = 4096usize;
    let mut bytes = fs.into_block().expect("the volume closes").bytes();
    bytes[as_usize(target) * bs + HEADER_LEN] ^= 0xff;

    let mut fs = open_health_read_only(
        bytes.clone(),
        512,
        DeviceHealth::Available(healthy_snapshot(1, 0)),
    );
    let report = fs.health(&GrantAll, &NullSink).expect("health");
    let scrub = report.scrub.expect("the delta triggered a scrub");
    assert_eq!(scrub.metadata_damaged, 1, "{scrub:?}");
    assert_eq!(scrub.metadata_repaired, 0, "{scrub:?}");
    assert_eq!(
        report.metadata_repaired, 0,
        "and no repair reached the lifetime counters: {report:?}"
    );
    assert_eq!(
        report.state,
        HealthState::Degraded,
        "a mirror that went bad is a watch-level signal whether or not it \
         could be rewritten: {report:?}"
    );
    let after = fs.into_block().expect("the volume closes");
    assert_eq!(after.writes, 0, "and the pass wrote nothing");
    assert_eq!(after.bytes(), bytes, "the device is untouched");
}

#[test]
fn health_baseline_survives_a_crash_during_its_update() {
    // The persisted baseline is updated inside a copy-on-write transaction, so
    // a power loss at any write count during a health pass leaves a mountable
    // volume with no live data lost: either the new baseline
    // committed in full or the previous one remains selected.
    let mut base = fmt_health(256, DeviceHealth::Available(healthy_snapshot(0, 0)));
    let root = base.root();
    base.create(root, b"keep", NodeKind::RegularFile)
        .expect("create keep");
    base.write_at(root, b"keep", 0, b"baseline")
        .expect("write keep");
    base.health(&GrantAll, &NullSink)
        .expect("establish baseline");
    let baseline = base.into_block().expect("the volume closes").bytes();

    for budget in 0..96u32 {
        let mut dev = MemBlock::from_bytes(baseline.clone(), 4096, 256)
            .with_health(DeviceHealth::Available(healthy_snapshot(5, 5)));
        dev.write_budget = Some(budget);
        let mut fs = ARXFS::open(dev, &TEST_KEY).expect("baseline opens");
        // The health pass (a scrub plus a baseline commit) may be cut short.
        let _ = fs.health(&GrantAll, &NullSink);
        let bytes = fs.into_block().expect("the volume closes").bytes();

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
// (`docs/src/filesystem/arxfs-spec.md` §15.12, §16).
//
// These are the adversarial superset of the per-stage tests, built on the
// same seams the earlier stages already provide: the
// block identity + companion mirror, the `DataFault` classes, the
// `verify_everything` scrub/check core, and the `MemBlock` write-budget
// fault-injection. They add no second verifier and no second on-disk decode
// path. (The fuzz harness for every decode path — mount, metadata, directory,
// compression, check, rescue — lives in `tests/fuzz_mount.rs` and the
// `tairix-compress` `fuzz_compress` harness, wired into `cargo xtask fuzz`.)
// ---------------------------------------------------------------------------

/// The crash-replay block geometry: a 4096-byte volume with room for several
/// files, a multi-block file, shared chunks, a reflink, and a subdirectory.
const CRASH_BS: u32 = 4096;
const CRASH_BC: u64 = 256;

/// Immutable witness file content. Every crash-replay trial asserts this file
/// reads back byte-for-byte after the (possibly torn) re-mount: live data is
/// never lost, whatever write count the power loss cut the transaction at
/// (`docs/src/filesystem/arxfs-spec.md` §14).
const CRASH_KEEP: &[u8] = b"keep-content-that-must-never-be-torn-or-lost";

/// Build a committed crash-replay baseline: a witness file (`keep`), a victim
/// to remove, a two-block file to truncate, a reflink source, an empty write
/// target, and a subdirectory — every operation the sweep replays already
/// has its precondition committed.
fn crash_baseline() -> alloc::vec::Vec<u8> {
    let mut fs = ARXFS::format(
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
    fs.into_block().expect("the volume closes").bytes()
}

/// How a volume under test publishes what its operations stage.
#[derive(Copy, Clone)]
enum Publish {
    /// No write-back host, so there is no window to age a transaction against
    /// and every operation publishes its own.
    PerOperation,
    /// A host whose clock the test controls, so operations join one
    /// transaction until something closes it.
    AsOneBatch(&'static TestWritebackHost),
}

impl Publish {
    /// Install what the mode needs on a freshly opened handle.
    fn apply(self, fs: ARXFS<MemBlock>) -> ARXFS<MemBlock> {
        match self {
            Self::PerOperation => fs,
            Self::AsOneBatch(host) => fs.with_writeback_host(TestWritebackHost::volume(), host),
        }
    }
}

/// Replay one representative transaction at every commit step: cut the device
/// off after each write count, then assert the re-opened volume always mounts
/// on a whole-transaction boundary and never loses the witness file.
///
/// `op` performs the transaction under the write budget; `check` asserts the
/// all-or-nothing post-condition on the re-mounted volume. The shared witness
/// assertion (`keep` reads back intact) runs for every budget before `check`.
///
/// `publish` decides whether each of `op`'s operations publishes its own
/// transaction or they all join one, so the same sweep prices both commit
/// shapes rather than a second sweep pricing the batched one.
fn crash_replay_each_step<Op, Check>(
    baseline: &[u8],
    max_budget: u32,
    publish: Publish,
    mut op: Op,
    mut check: Check,
) where
    Op: FnMut(&mut ARXFS<MemBlock>),
    Check: FnMut(&mut ARXFS<MemBlock>, u32),
{
    let device = |bytes: alloc::vec::Vec<u8>| {
        MemBlock::from_bytes(bytes, CRASH_BS, CRASH_BC)
            .with_discard(1, 0)
            .with_health(DeviceHealth::Available(healthy_snapshot(0, 0)))
    };
    for budget in 0..max_budget {
        let mut dev = device(baseline.to_vec());
        dev.write_budget = Some(budget);
        let mut fs = publish.apply(
            ARXFS::open(dev, &TEST_KEY)
                .expect("baseline opens")
                .with_clock(fixed_clock),
        );
        op(&mut fs);
        // The close is inside the crash window too: a batch is published by
        // it, and a budget that runs out during it is a power loss mid-commit.
        // The device is read from the handle rather than taken from it,
        // because a commit that fails does not hand the volume back.
        let _ = FilesystemWrite::flush(&mut fs);
        let bytes = fs.block_mut().bytes();

        let mut fs = ARXFS::open(device(bytes), &TEST_KEY)
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
fn file_size(fs: &mut ARXFS<MemBlock>, name: &[u8]) -> Option<u64> {
    let node = fs.lookup(fs.root(), name).ok()?;
    Some(fs.node_info(node).expect("node info").size)
}

/// The crash-replay write-budget ceiling: larger than the write count of any
/// single transaction the sweeps replay, so every commit step is covered.
const CRASH_BUDGET: u32 = 200;

#[test]
fn crash_replay_at_every_commit_step_for_create_write_truncate() {
    // "crash replay at every commit step" for the namespace/data
    // transactions: a power loss at every write count must leave a volume that
    // mounts on a whole-transaction boundary, with the operation's effect fully
    // present or fully absent (never torn) and no live data lost.
    let baseline = crash_baseline();

    // create: the new file is either present (empty) or absent — never half-made.
    crash_replay_each_step(
        &baseline,
        CRASH_BUDGET,
        Publish::PerOperation,
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
        Publish::PerOperation,
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
        Publish::PerOperation,
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
    // Crash replay for the unlink / clone / discard transactions.
    let baseline = crash_baseline();

    // remove: the victim is either fully present (with its content) or gone.
    crash_replay_each_step(
        &baseline,
        CRASH_BUDGET,
        Publish::PerOperation,
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
        Publish::PerOperation,
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
    // queue is transient and discard never zeroes live data, so the
    // re-mount is always clean and the witness file survives.
    crash_replay_each_step(
        &baseline,
        CRASH_BUDGET,
        Publish::PerOperation,
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
    // them) with no live data lost. The shared witness assertion in
    // `crash_replay_each_step` already covers "no live data lost".
    let baseline = crash_baseline();
    crash_replay_each_step(
        &baseline,
        CRASH_BUDGET,
        Publish::PerOperation,
        |fs| {
            let _ = fs.scrub(&GrantAll, &NullSink, ScrubBudget::Unlimited);
        },
        |_, _| {},
    );
    crash_replay_each_step(
        &baseline,
        CRASH_BUDGET,
        Publish::PerOperation,
        |fs| {
            let _ = fs.check(&GrantAll, &NullSink);
        },
        |_, _| {},
    );
    crash_replay_each_step(
        &baseline,
        CRASH_BUDGET,
        Publish::PerOperation,
        |fs| {
            let _ = fs.health(&GrantAll, &NullSink);
        },
        |_, _| {},
    );
}

#[test]
fn crash_replay_at_every_step_of_a_batch_publishes_all_of_it_or_none() {
    // The batched commit shape: three operations join one transaction, so a
    // power loss at any write count during it leaves either every one of them
    // or none — a subset would mean a caller was told an operation happened
    // that the volume then forgot while keeping a later one.
    let baseline = crash_baseline();
    let host = TestWritebackHost::leaked(0);
    let implies = |a: bool, b: bool| !a || b;
    // Whether the device let all three operations through. Where it did, the
    // batch is one transaction and the outcome is all or nothing; where an
    // operation was refused it was reported failed and undone and the ones
    // after it never ran, so what survives is a prefix.
    let all_ran = core::cell::Cell::new(false);
    let mut nothing = 0u32;
    let mut everything = 0u32;
    crash_replay_each_step(
        &baseline,
        CRASH_BUDGET,
        Publish::AsOneBatch(host),
        |fs| {
            all_ran.set(false);
            let root = fs.root();
            if fs.create(root, b"fresh", NodeKind::RegularFile).is_err() {
                return;
            }
            if fs.write_at(root, b"target", 0, b"freshdata").is_err() {
                return;
            }
            if fs.remove(root, b"victim").is_err() {
                return;
            }
            all_ran.set(true);
        },
        |fs, budget| {
            let root = fs.root();
            let created = fs.lookup(root, b"fresh").is_ok();
            let written = file_size(fs, b"target") == Some(9);
            let removed = matches!(fs.lookup(root, b"victim"), Err(DriverError::NotFound));
            if all_ran.get() {
                assert!(
                    created == written && written == removed,
                    "a batch whose three operations all succeeded was \
                     published in part at budget {budget}: created {created}, \
                     written {written}, removed {removed}"
                );
            } else {
                assert!(
                    implies(written, created) && implies(removed, written),
                    "a batch published with a hole at budget {budget}: \
                     created {created}, written {written}, removed {removed}"
                );
            }
            if written {
                let node = fs.lookup(root, b"target").expect("target");
                assert_eq!(
                    read_all(fs, node, 9),
                    b"freshdata",
                    "torn write at budget {budget}"
                );
            } else {
                assert_eq!(
                    file_size(fs, b"target"),
                    Some(0),
                    "torn write at budget {budget}"
                );
            }
            assert_victim_whole_or_gone(fs, budget);
            match (created, written, removed) {
                (false, false, false) => nothing += 1,
                (true, true, true) => everything += 1,
                _ => {}
            }
        },
    );
    assert!(
        nothing > 0 && everything > 0,
        "the sweep never straddled the commit: {nothing} steps published \
         nothing and {everything} published the whole batch"
    );
}

/// Assert the `victim` file is either fully present with its committed content
/// or wholly absent — never a torn unlink (`docs/src/filesystem/arxfs-spec.md`
/// §14).
fn assert_victim_whole_or_gone(fs: &mut ARXFS<MemBlock>, budget: u32) {
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
    let mut fs = ARXFS::format(
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

    (
        fs.into_block().expect("the volume closes").bytes(),
        targets,
        keep_body,
    )
}

/// Flip the first payload byte of the block at `block`, breaking its keyed
/// authenticator. `block + 1` is the companion mirror.
fn wound_copy(bytes: &mut [u8], block: u64) {
    let off = as_usize(block) * 4096 + HEADER_LEN;
    bytes[off] ^= 0xff;
}

fn open_corruption(bytes: alloc::vec::Vec<u8>) -> Result<ARXFS<MemBlock>, DriverError> {
    ARXFS::open(MemBlock::from_bytes(bytes, 4096, 512), &TEST_KEY)
        .map(|fs| fs.with_clock(fixed_clock))
}

#[test]
fn corruption_injection_single_metadata_copy_is_recovered_from_the_companion() {
    // Wound exactly one physical copy of every on-disk metadata structure
    // class. The companion-mirror seam must recover each: the volume mounts,
    // scrub reports nothing unrepairable, check finds the structure sound, and
    // the witness file reads back intact (`docs/src/filesystem/arxfs-spec.md`
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
        assert_eq!(
            check.structure,
            StructureVerdict::Sound,
            "{label}: {check:?}"
        );
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
    // neither copy authenticating, the mirror cannot repair the block, so
    // ARXFS never trusts the corruption: it either fails the mount closed or, because the superblock ring retains earlier
    // whole transactions, selects an older committed root that does not
    // reference the wounded block and is fully consistent (a partial or
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
                assert_eq!(
                    report.structure,
                    StructureVerdict::Sound,
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
    // dropping or fabricating entries (`docs/src/filesystem/arxfs-spec.md` §8,
    // §12).
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
    // metadata: even with both copies bad the volume mounts, a fresh scrub
    // simply restarts, a health pass re-derives from a default baseline, and no
    // live data is lost (`docs/src/filesystem/arxfs-spec.md` §11, §12).
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
    // Data blocks are not mirrored (only metadata is). A wounded data block is
    // therefore detected and classified by its `DataFault` layer, and scrub
    // records the fault rather than repairing it (deep data repair is out of
    // scope); the production read path fails closed
    // (`docs/src/filesystem/arxfs-spec.md` §12).
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"f", NodeKind::RegularFile)
        .expect("create");
    fs.write_at(root, b"f", 0, &alloc::vec![0x42u8; 300])
        .expect("write");
    let phys = data_block_phys(&mut fs, b"f", 0);
    let bs = 512usize;
    let base = as_usize(phys) * bs;
    let mut bytes = fs.into_block().expect("the volume closes").bytes();
    bytes[base] ^= 0x01; // wound the at-rest ciphertext

    let mut fs =
        ARXFS::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY).expect("still mounts");
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

/// A committed volume on a device with a volatile write cache: the witness
/// `keep` holding [`CRASH_KEEP`], an empty `new` to write to, everything on
/// media, and the ring slot the next commit will publish.
fn volatile_volume(publish: Publish) -> (ARXFS<MemBlock>, u64) {
    let mut fs = publish.apply(
        ARXFS::format(
            MemBlock::new(CRASH_BS, CRASH_BC).with_volatile_cache(),
            64,
            &TEST_KEY,
            &mut TestEntropy::new(),
        )
        .expect("format")
        .with_clock(fixed_clock),
    );
    let root = fs.root();
    fs.create(root, b"keep", NodeKind::RegularFile)
        .expect("create keep");
    fs.write_at(root, b"keep", 0, CRASH_KEEP)
        .expect("write keep");
    fs.create(root, b"new", NodeKind::RegularFile)
        .expect("create new");
    fs.create(root, b"pad", NodeKind::RegularFile)
        .expect("create pad");
    // Fill the ring, so the slot the next commit overwrites already holds a
    // decodable older one. That is what makes the primary copy — the one a
    // mount prefers — the single write that publishes a transaction; on a ring
    // whose slots have never been written, an absent primary lets the
    // companion decide instead.
    // A commit is what advances the ring, and under a batched handle the pad
    // writes would otherwise all join one.
    let mut at = 0u64;
    while fs.ring_pos < RING_SLOTS {
        fs.write_at(root, b"pad", at, b"x").expect("pad");
        FilesystemWrite::flush(&mut fs).expect("pad sync");
        at += 1;
    }
    // Two syncs. The first persists the map and leaves only its clean stamp in
    // the device cache — losing that stamp costs a rebuild at the next mount,
    // never correctness, so it is deliberately the one write no barrier
    // follows — and the second commits it, so the window starts genuinely
    // empty.
    FilesystemWrite::flush(&mut fs).expect("sync");
    FilesystemWrite::flush(&mut fs).expect("sync");
    assert!(
        fs.block_mut().volatile_blocks().is_empty(),
        "a sync leaves nothing in the device cache"
    );
    let slot = slot_block(fs.ring_pos % RING_SLOTS);
    (fs, slot)
}

/// Payload the volatile-cache tests write, wide enough to need several data
/// blocks and a tree above them.
const VOLATILE_PAYLOAD: &[u8] = &[0x5A; 3 * CRASH_BS as usize + 17];

/// A commit keeps resident allocation-map changes in RAM, so only the
/// publishing slot remains volatile. Explicit sync drains those pages through
/// the shared set and leaves only the dispensable clean stamp volatile.
#[test]
fn a_commit_keeps_map_pages_in_ram_until_sync() {
    let (mut fs, slot) = volatile_volume(Publish::PerOperation);
    let root = fs.root();
    assert_eq!(
        fs.write_at(root, b"new", 0, VOLATILE_PAYLOAD),
        Ok(VOLATILE_PAYLOAD.len())
    );
    let companion = ARXFS::<MemBlock>::companion(slot);
    assert_eq!(
        fs.block_mut().volatile_blocks(),
        alloc::vec![slot, companion],
        "an ordinary commit wrote rebuildable map pages eagerly"
    );
    assert!(!fs.map_is_stamped_clean());
    let map_header = fs.map_region_start();
    FilesystemWrite::flush(&mut fs).expect("sync");
    assert_eq!(
        fs.block_mut().volatile_blocks(),
        alloc::vec![map_header],
        "sync left more than the dispensable clean stamp volatile"
    );
}

/// A power loss immediately after a commit commits an arbitrary subset of the
/// volatile window and drops the rest — and every one of those outcomes leaves
/// the volume mountable at a whole transaction boundary, holding either the
/// prior committed state or the new one.
///
/// Keeping both copies of the slot lands the new state, because every block its
/// root names crossed the barrier first. Keeping neither lands the prior state.
/// Keeping one leaves the mirror to decide, and either answer is whole.
#[test]
fn a_power_loss_after_a_commit_leaves_prior_or_new_whatever_it_drops() {
    for kept in 0..4u8 {
        let (mut fs, slot) = volatile_volume(Publish::PerOperation);
        let root = fs.root();
        assert_eq!(
            fs.write_at(root, b"new", 0, VOLATILE_PAYLOAD),
            Ok(VOLATILE_PAYLOAD.len())
        );
        let comp = ARXFS::<MemBlock>::companion(slot);
        fs.block_mut()
            .power_loss(|lba| (lba == slot && kept & 1 != 0) || (lba == comp && kept & 2 != 0));
        let bytes = fs.into_block().expect("the volume closes").bytes();
        let mut fs = ARXFS::open(MemBlock::from_bytes(bytes, CRASH_BS, CRASH_BC), &TEST_KEY)
            .expect("a power loss always leaves a mountable volume");

        let witness = fs.lookup(fs.root(), b"keep").expect("the witness survives");
        let mut out = alloc::vec![0u8; CRASH_KEEP.len()];
        assert_eq!(fs.read_at(witness, 0, &mut out), Ok(CRASH_KEEP.len()));
        assert_eq!(out, CRASH_KEEP, "live data lost (kept {kept:#b})");

        // The primary copy is the commit point: the new state is selected
        // exactly when it landed, whatever became of the companion.
        let published = kept & 1 != 0;
        let node = fs.lookup(fs.root(), b"new").expect("the target survives");
        let size = fs.node_info(node).expect("stat").size;
        if !published {
            assert_eq!(
                size, 0,
                "the new state appeared without its slot ({kept:#b})"
            );
            continue;
        }
        assert_eq!(
            size,
            VOLATILE_PAYLOAD.len() as u64,
            "torn size at kept {kept:#b}"
        );
        let mut read = alloc::vec![0u8; VOLATILE_PAYLOAD.len()];
        assert_eq!(
            fs.read_at(node, 0, &mut read),
            Ok(VOLATILE_PAYLOAD.len()),
            "the published root names a block that never reached media \
             (kept {kept:#b})"
        );
        assert_eq!(read, VOLATILE_PAYLOAD, "torn contents at kept {kept:#b}");
    }
}

/// A power loss immediately after the commit that publishes a *batch* leaves
/// every operation the batch carried, or none of them — and each of them
/// readable, because every block the batch's root names crossed the one
/// barrier before the slot that publishes them all.
///
/// The batch is closed by its dirty-age window rather than by a sync, because
/// a sync's trailing barrier would commit the device cache and leave a power
/// loss nothing to drop. That is also the close a real idle volume gets.
#[test]
fn a_power_loss_after_a_batchs_commit_publishes_all_of_it_or_none() {
    for kept in 0..4u8 {
        let host = TestWritebackHost::leaked(0);
        let (mut fs, slot) = volatile_volume(Publish::AsOneBatch(host));
        let root = fs.root();
        assert_eq!(
            fs.write_at(root, b"new", 0, VOLATILE_PAYLOAD),
            Ok(VOLATILE_PAYLOAD.len())
        );
        fs.create(root, b"second", NodeKind::RegularFile)
            .expect("create");
        assert_eq!(
            fs.write_at(root, b"second", 0, CRASH_KEEP),
            Ok(CRASH_KEEP.len())
        );
        assert!(
            fs.block_mut().volatile_blocks().is_empty(),
            "the batch published before its window elapsed (kept {kept:#b})"
        );
        // The window elapses, so the next operation publishes the batch it
        // joined — the whole of it, behind one barrier.
        host.set_now(PAST_EVERY_WINDOW);
        fs.create(root, b"tick", NodeKind::RegularFile)
            .expect("the aged batch publishes");

        let comp = ARXFS::<MemBlock>::companion(slot);
        fs.block_mut()
            .power_loss(|lba| (lba == slot && kept & 1 != 0) || (lba == comp && kept & 2 != 0));
        let bytes = fs.block_mut().bytes();
        let mut fs = ARXFS::open(MemBlock::from_bytes(bytes, CRASH_BS, CRASH_BC), &TEST_KEY)
            .expect("a power loss always leaves a mountable volume");
        let root = fs.root();

        let witness = fs.lookup(root, b"keep").expect("the witness survives");
        assert_eq!(
            read_all(&mut fs, witness, CRASH_KEEP.len()),
            CRASH_KEEP,
            "live data lost (kept {kept:#b})"
        );

        // The primary copy is the commit point: the batch is selected exactly
        // when it landed, whatever became of the companion.
        let published = kept & 1 != 0;
        let sized = |fs: &mut ARXFS<MemBlock>, name: &[u8]| {
            fs.lookup(fs.root(), name)
                .and_then(|node| fs.node_info(node))
                .map(|info| info.size)
        };
        if !published {
            assert_eq!(sized(&mut fs, b"new"), Ok(0));
            assert_eq!(sized(&mut fs, b"second"), Err(DriverError::NotFound));
            assert_eq!(sized(&mut fs, b"tick"), Err(DriverError::NotFound));
            continue;
        }
        assert_eq!(
            sized(&mut fs, b"new"),
            Ok(VOLATILE_PAYLOAD.len() as u64),
            "torn batch at kept {kept:#b}"
        );
        assert_eq!(sized(&mut fs, b"second"), Ok(CRASH_KEEP.len() as u64));
        assert_eq!(sized(&mut fs, b"tick"), Ok(0));
        let new = fs.lookup(root, b"new").expect("the payload survives");
        assert_eq!(
            read_all(&mut fs, new, VOLATILE_PAYLOAD.len()),
            VOLATILE_PAYLOAD,
            "the published root names a block that never reached media \
             (kept {kept:#b})"
        );
        let second = fs.lookup(root, b"second").expect("the second survives");
        assert_eq!(read_all(&mut fs, second, CRASH_KEEP.len()), CRASH_KEEP);
    }
}

/// A batch the write-back ceiling forces out mid-way publishes whole
/// operations: a crash straight after keeps no more than the caller was told
/// was written, and every published byte is exact.
#[test]
fn a_crash_after_a_forced_commit_keeps_no_more_than_was_reported() {
    let host = TestWritebackHost::leaked(0);
    let mut fs = floor_bounded(fmt(CRASH_BS, CRASH_BC, 64))
        .with_writeback_host(TestWritebackHost::volume(), host);
    let root = fs.root();
    fs.create(root, b"batched", NodeKind::RegularFile)
        .expect("create");
    FilesystemWrite::flush(&mut fs).expect("start from a published volume");

    // A quarter of the ceiling per call, so several operations join the
    // transaction before the ceiling forces it out, and an incompressible
    // body so the write path prices a real store rather than a codec's luck.
    let body = incompressible(4 * RUN_BYTES);
    let slice = RUN_BYTES / 4;
    let mut reported = 0usize;
    let mut published = 0u64;
    while reported < body.len() {
        let take = slice.min(body.len() - reported);
        let written = fs
            .write_at(
                root,
                b"batched",
                reported as u64,
                &body[reported..reported + take],
            )
            .expect("a bounded write still stores bytes");
        assert!(written > 0, "the ceiling must never stall a writer");
        reported += written;
        let mut crashed = reopen_device(&mut fs, CRASH_BS, CRASH_BC);
        let node = crashed
            .lookup(crashed.root(), b"batched")
            .expect("the file was published with the fixture");
        published = crashed.node_info(node).expect("stat").size;
        if published > 0 {
            let at = as_usize(published);
            assert!(
                at <= reported,
                "a forced commit published {at} bytes against {reported} \
                 reported written"
            );
            assert_eq!(
                read_all(&mut crashed, node, at),
                body[..at],
                "the published prefix is not what was written"
            );
            break;
        }
    }
    assert!(published > 0, "the ceiling never forced a commit");

    // The rest of the body lands, and the whole of it reads back.
    while reported < body.len() {
        let written = fs
            .write_at(root, b"batched", reported as u64, &body[reported..])
            .expect("a bounded write still stores bytes");
        assert!(written > 0);
        reported += written;
    }
    FilesystemWrite::flush(&mut fs).expect("publish the tail");
    let node = fs.lookup(root, b"batched").expect("the file");
    assert_eq!(fs.node_info(node).expect("stat").size, body.len() as u64);
    assert_eq!(read_all(&mut fs, node, body.len()), body);
}

/// A barrier that faults is a failed barrier: the slot is never written, so the
/// transaction did not happen. The handle rolls back and a later commit
/// publishes only its own change — a caller told a commit failed can never have
/// its trees published behind its back.
#[test]
fn a_barrier_that_faults_publishes_nothing_and_leaves_no_transaction_behind() {
    let (mut fs, slot) = volatile_volume(Publish::PerOperation);
    let root = fs.root();
    fs.block_mut().fail_flush = true;
    assert_eq!(
        fs.write_at(root, b"new", 0, VOLATILE_PAYLOAD),
        Err(DriverError::DeviceFault)
    );
    let held = fs.block_mut().volatile_blocks();
    let comp = ARXFS::<MemBlock>::companion(slot);
    assert!(
        !held.contains(&slot) && !held.contains(&comp),
        "a commit whose barrier failed wrote its slot anyway: {held:?}"
    );
    // The device recovers; the next operation is the only one that commits.
    fs.block_mut().fail_flush = false;
    fs.write_at(root, b"new", 0, b"second").expect("write");
    FilesystemWrite::flush(&mut fs).expect("sync");
    let bytes = fs.into_block().expect("the volume closes").bytes();
    let mut fs =
        ARXFS::open(MemBlock::from_bytes(bytes, CRASH_BS, CRASH_BC), &TEST_KEY).expect("reopen");
    let node = fs.lookup(fs.root(), b"new").expect("target");
    assert_eq!(
        fs.node_info(node).expect("stat").size,
        b"second".len() as u64,
        "the failed transaction's tree was published by the next commit"
    );
    let mut out = [0u8; 6];
    assert_eq!(fs.read_at(node, 0, &mut out), Ok(6));
    assert_eq!(&out, b"second");
}

/// A slot write that faults leaves publication genuinely unknown — the device
/// may have taken one copy — so the handle forces itself read-only rather than
/// guessing. Nothing is freed, so whichever of the two roots the device
/// actually holds is still intact for the next mount, and no later operation
/// can hand out a block either of them names.
///
/// Whichever copy fails, the mount selects the *prior* state, matching the
/// failure the caller was told about. That is what writing the companion first
/// buys: the primary — the copy a mount prefers — is the last write of the
/// commit, so a half-written pair never publishes.
#[test]
fn a_failed_slot_write_freezes_the_handle_and_publishes_nothing() {
    let (probe, probe_slot) = volatile_volume(Publish::PerOperation);
    drop(probe);
    for faulted in [probe_slot, ARXFS::<MemBlock>::companion(probe_slot)] {
        let (mut fs, slot) = volatile_volume(Publish::PerOperation);
        assert_eq!(slot, probe_slot, "the fixture is deterministic");
        let root = fs.root();
        let old_root = fs.root_phys;
        fs.block_mut().write_faults.insert(faulted);
        assert_eq!(
            fs.write_at(root, b"new", 0, VOLATILE_PAYLOAD),
            Err(DriverError::DeviceFault)
        );
        assert!(
            fs.read_only,
            "a commit of unknown publication must force the handle read-only \
             (faulted {faulted})"
        );
        assert_eq!(
            fs.write_at(root, b"new", 0, b"again"),
            Err(DriverError::PermissionDenied),
            "a frozen handle accepts no further mutation"
        );
        assert_eq!(
            fs.inode_tree_root, fs.saved_txn.inode_tree_root,
            "the frozen handle exposed unpublished tree state"
        );
        // Both candidate roots stay reserved: the prior one because a mount may
        // still select it, the new one because a mount may select that instead.
        assert!(
            fs.is_used(old_root) && fs.is_used(ARXFS::<MemBlock>::companion(old_root)),
            "the previously committed root was freed while it may still be \
             selected (faulted {faulted})"
        );
        let unpublished: alloc::vec::Vec<core::ops::Range<u64>> = fs
            .allocator()
            .expect("allocator")
            .txn_private
            .iter()
            .collect();
        assert!(!unpublished.is_empty(), "the fixture published nothing");
        assert!(
            unpublished
                .iter()
                .flat_map(core::clone::Clone::clone)
                .all(|block| fs.is_used(block)),
            "the frozen handle released a block the unpublished root may name \
             (faulted {faulted})"
        );
        let mapped_free = fs.total_blocks - fs.used_blocks().len() as u64;
        assert_eq!(
            fs.free_count, mapped_free,
            "the frozen handle's free count contradicts its own map"
        );
        // Reads still work on the frozen handle.
        let witness = fs.lookup(fs.root(), b"keep").expect("the witness survives");
        let mut out = alloc::vec![0u8; CRASH_KEEP.len()];
        assert_eq!(fs.read_at(witness, 0, &mut out), Ok(CRASH_KEEP.len()));
        assert_eq!(out, CRASH_KEEP);

        let mut device = fs.into_block().expect("the volume closes");
        device.write_faults.clear();
        device.power_loss(|_| true);
        let bytes = device.bytes();
        let mut fs = ARXFS::open(MemBlock::from_bytes(bytes, CRASH_BS, CRASH_BC), &TEST_KEY)
            .expect("the volume still mounts");
        let witness = fs.lookup(fs.root(), b"keep").expect("the witness survives");
        let mut out = alloc::vec![0u8; CRASH_KEEP.len()];
        assert_eq!(fs.read_at(witness, 0, &mut out), Ok(CRASH_KEEP.len()));
        assert_eq!(out, CRASH_KEEP);
        let node = fs.lookup(fs.root(), b"new").expect("the target survives");
        assert_eq!(
            fs.node_info(node).expect("stat").size,
            0,
            "a commit reported as failed was published anyway (faulted {faulted})"
        );
    }
}

/// A verification or telemetry pass whose commit fails leaves nothing for a
/// later commit to publish either. These passes report the failure straight to
/// their caller, so the rollback belongs to the commit itself rather than to
/// each of its call sites.
#[test]
fn a_failed_commit_on_a_maintenance_pass_restores_the_published_roots() {
    let (mut fs, _slot) = volatile_volume(Publish::PerOperation);
    let baseline = fs.health_baseline_root;
    assert_ne!(baseline, 0, "mkfs stored a baseline to supersede");
    fs.block_mut().fail_flush = true;
    assert_eq!(
        fs.health(&GrantAll, &NullSink),
        Err(DriverError::DeviceFault)
    );
    assert_eq!(
        fs.health_baseline_root, baseline,
        "the handle kept an unpublished baseline for a later commit to publish"
    );
    fs.block_mut().fail_flush = false;
    let root = fs.root();
    fs.write_at(root, b"new", 0, b"after").expect("write");
    FilesystemWrite::flush(&mut fs).expect("sync");
    let bytes = fs.into_block().expect("the volume closes").bytes();
    let fs =
        ARXFS::open(MemBlock::from_bytes(bytes, CRASH_BS, CRASH_BC), &TEST_KEY).expect("reopen");
    assert_eq!(
        fs.health_baseline_root, baseline,
        "the next commit published the failed pass's record"
    );
}

/// The dirty set is pinned memory bounded by one transaction, never a cache
/// that accumulates: it holds nothing between operations, whether the last one
/// committed, rolled back after a fault partway through its drain, or rolled
/// back after a failed barrier.
///
/// That is what makes the footprint the *operation's* working set rather than
/// the volume's, which is the property a small machine serving several huge
/// volumes depends on. What one transaction holds at its peak is the blocks it
/// wrote, counted to the command by the write-amplification ledger.
#[test]
fn the_dirty_set_holds_nothing_between_operations() {
    let (mut fs, _slot) = volatile_volume(Publish::PerOperation);
    let root = fs.root();
    assert_eq!(fs.dirty.len(), 0, "a synced volume stages nothing");
    assert_eq!(
        fs.write_at(root, b"new", 0, VOLATILE_PAYLOAD),
        Ok(VOLATILE_PAYLOAD.len())
    );
    assert_eq!(fs.dirty.len(), 0, "a commit drains the set it filled");

    // Let the drain's first run land and refuse the next, so it faults with
    // this transaction's remaining blocks still staged. The drain issues one
    // request per physical run — here the copied data blocks and the mirrored
    // metadata pairs above them — so refusing the second lands the fault
    // inside the drain rather than on the slot write after the barrier.
    fs.block_mut().writes = 0;
    fs.block_mut().write_fault_after = Some(1);
    assert_eq!(
        fs.write_at(root, b"new", 0, &[0x11; 4 * CRASH_BS as usize]),
        Err(DriverError::DeviceFault)
    );
    assert_eq!(
        fs.block_mut().writes,
        1,
        "the drain must have written before it faulted, so blocks were left \
         staged for the rollback to clear"
    );
    assert!(
        !fs.read_only,
        "the fault was inside the drain, so the commit rolled back rather than \
         leaving publication unknown"
    );
    assert_eq!(
        fs.dirty.len(),
        0,
        "a rolled-back transaction stages nothing"
    );

    fs.block_mut().write_fault_after = None;
    fs.block_mut().fail_flush = true;
    assert_eq!(
        fs.write_at(root, b"new", 0, b"barrier"),
        Err(DriverError::DeviceFault)
    );
    assert_eq!(
        fs.dirty.len(),
        0,
        "a transaction whose barrier failed stages nothing"
    );
}

/// Refusing the first allocation-map page during sync leaves the commit's
/// invalid stamp durable but its publishing slot volatile. Losing that slot
/// selects the prior tree and rebuilds its map.
#[test]
fn the_map_turns_its_clean_stamp_dirty_durably_before_its_first_page_write() {
    let (mut fs, slot) = volatile_volume(Publish::PerOperation);
    let root = fs.root();
    assert!(
        fs.map_is_stamped_clean(),
        "the fixture's sync left the map stamped clean"
    );
    assert_eq!(
        fs.write_at(root, b"new", 0, VOLATILE_PAYLOAD),
        Ok(VOLATILE_PAYLOAD.len())
    );
    let start = fs.map_region_start();
    for block in start + 1..start + fs.map_region_blocks() {
        fs.block_mut().write_faults.insert(block);
    }
    assert_eq!(
        FilesystemWrite::flush(&mut fs),
        Err(DriverError::DeviceFault)
    );
    let held = fs.block_mut().volatile_blocks();
    assert!(held.contains(&slot));
    let mut device = fs.into_block().expect("the volume closes");
    device.write_faults.clear();
    device.power_loss(|_| false);
    let bytes = device.bytes();
    let mut fs =
        ARXFS::open(MemBlock::from_bytes(bytes, CRASH_BS, CRASH_BC), &TEST_KEY).expect("reopen");
    assert!(
        !fs.map_is_stamped_clean(),
        "the mount adopted the interrupted allocation map"
    );
    let node = fs.lookup(fs.root(), b"new").expect("target");
    assert_eq!(fs.node_info(node).expect("stat").size, 0);
}

/// A failed sync may leave any subset of map pages in stable storage. The
/// durable invalid stamp forces a rebuild against whichever slot survived, so
/// neither partial map can free a live block or retain an orphan.
#[test]
fn a_failed_sync_rebuilds_every_partially_persisted_map() {
    for publish in [false, true] {
        for map_pattern in 0..4u8 {
            let (mut fs, slot) = volatile_volume(Publish::PerOperation);
            let old_used = fs.used_blocks();
            let old_free = fs.free_count;
            let root = fs.root();
            fs.write_at(root, b"new", 0, VOLATILE_PAYLOAD)
                .expect("write");
            let new_used = fs.used_blocks();
            let new_free = fs.free_count;
            let map_start = fs.map_region_start();
            let map_end = map_start + fs.map_region_blocks();
            fs.block_mut().fail_flush = true;
            assert_eq!(
                FilesystemWrite::flush(&mut fs),
                Err(DriverError::DeviceFault)
            );
            assert!(
                fs.block_mut()
                    .volatile_blocks()
                    .iter()
                    .any(|block| (map_start + 1..map_end).contains(block)),
                "the failed sync reached no map page"
            );

            let companion = ARXFS::<MemBlock>::companion(slot);
            let mut device = fs.into_block().expect("the volume closes");
            device.fail_flush = false;
            device.power_loss(|block| {
                if block == slot || block == companion {
                    return publish;
                }
                if !(map_start + 1..map_end).contains(&block) {
                    return false;
                }
                match map_pattern {
                    0 => false,
                    1 => true,
                    2 => block.is_multiple_of(2),
                    _ => !block.is_multiple_of(2),
                }
            });
            let bytes = device.bytes();
            let mut reopened =
                ARXFS::open(MemBlock::from_bytes(bytes, CRASH_BS, CRASH_BC), &TEST_KEY)
                    .expect("rebuild");
            assert!(!reopened.map_is_stamped_clean());
            let (expected_used, expected_free) = if publish {
                (&new_used, new_free)
            } else {
                (&old_used, old_free)
            };
            let actual_used = reopened.used_blocks();
            assert_eq!(
                &actual_used, expected_used,
                "wrong map for publish={publish}, pattern={map_pattern}"
            );
            assert_eq!(
                reopened.free_count, expected_free,
                "wrong free count for publish={publish}, pattern={map_pattern}"
            );
        }
    }
}

/// An operation refused for an ordinary reason undoes its own marks and leaves
/// the map trusted, so the next one does not pay for a rebuild. Only a device
/// fault makes the map ambiguous enough to need one, which the sibling tests
/// cover.
///
/// The map is checked against the trees either side of the refusal: an undo
/// that got a bit wrong would be as bad as the rebuild it avoids.
#[test]
fn a_refused_operation_leaves_the_allocation_map_exact_and_trusted() {
    let (mut fs, _slot) = volatile_volume(Publish::PerOperation);
    let root = fs.root();
    fs.create(root, b"taken", NodeKind::RegularFile)
        .expect("create");
    let before_used = fs.used_blocks();
    let before_free = fs.free_count;

    assert_eq!(
        fs.create(root, b"taken", NodeKind::RegularFile),
        Err(DriverError::AlreadyExists),
        "a name already taken must be refused"
    );

    assert!(
        !fs.allocator().expect("allocator").needs_rebuild,
        "an ordinary refusal demanded a whole-volume map rebuild"
    );
    assert_eq!(
        fs.used_blocks(),
        before_used,
        "the refusal moved a bit in the allocation map"
    );
    assert_eq!(
        fs.free_count, before_free,
        "the refusal moved the free count"
    );
    let alloc = fs.allocator().expect("allocator");
    assert!(
        alloc.txn_private.is_empty() && alloc.txn_freed.is_empty(),
        "the refusal left transaction bookkeeping behind"
    );

    // And the volume is still writable and correct afterwards.
    fs.create(root, b"fresh", NodeKind::RegularFile)
        .expect("create after a refusal");
    FilesystemWrite::flush(&mut fs).expect("sync");
    let expected_used = fs.used_blocks();
    let bytes = fs.into_block().expect("the volume closes").bytes();
    let mut reopened =
        ARXFS::open(MemBlock::from_bytes(bytes, CRASH_BS, CRASH_BC), &TEST_KEY).expect("reopen");
    assert_eq!(reopened.used_blocks(), expected_used);
    assert!(reopened.lookup(reopened.root(), b"fresh").is_ok());
}

/// A failed transaction that *did* free blocks reserves them back, so a block
/// the committed root still owns can never be handed out because a later
/// operation was refused.
#[test]
fn a_failed_commit_reserves_the_blocks_it_had_already_deferred_for_freeing() {
    let (mut fs, _slot) = volatile_volume(Publish::PerOperation);
    let root = fs.root();
    fs.write_at(root, b"new", 0, VOLATILE_PAYLOAD)
        .expect("write");
    let before_used = fs.used_blocks();
    let before_free = fs.free_count;
    let old_root = fs.root_phys;

    // Rewriting the file releases its old data and metadata into the deferred
    // set, and the commit's barrier is the first thing that can fail after
    // `prepare_deferred_frees` has already marked them free.
    fs.block_mut().fail_flush = true;
    assert_eq!(
        fs.write_at(root, b"new", 0, b"replacement"),
        Err(DriverError::DeviceFault)
    );
    fs.block_mut().fail_flush = false;

    assert!(
        !fs.read_only,
        "a pre-slot failure must not freeze the handle"
    );
    assert!(
        fs.is_used(old_root) && fs.is_used(ARXFS::<MemBlock>::companion(old_root)),
        "the committed root was left free after a failed commit"
    );
    assert_eq!(
        fs.used_blocks(),
        before_used,
        "a failed commit left the map disagreeing with the committed trees"
    );
    assert_eq!(
        fs.free_count, before_free,
        "a failed commit moved the count"
    );
    let mut out = alloc::vec![0u8; VOLATILE_PAYLOAD.len()];
    let victim = fs.lookup(root, b"new").expect("the file survives");
    assert_eq!(
        fs.read_at(victim, 0, &mut out),
        Ok(VOLATILE_PAYLOAD.len()),
        "the committed contents were lost"
    );
    assert_eq!(out, VOLATILE_PAYLOAD);
}

#[test]
fn a_failed_sync_rebuilds_before_a_same_handle_check_and_write() {
    let (mut fs, _slot) = volatile_volume(Publish::PerOperation);
    let root = fs.root();
    fs.write_at(root, b"new", 0, VOLATILE_PAYLOAD)
        .expect("write");
    fs.block_mut().fail_flush = true;
    assert_eq!(
        FilesystemWrite::flush(&mut fs),
        Err(DriverError::DeviceFault)
    );
    assert!(fs.allocator().expect("allocator").needs_rebuild);
    assert_eq!(fs.dirty.len(), 0, "failed staging was retained ambiguously");

    fs.block_mut().fail_flush = false;
    let report = fs.check(&GrantAll, &NullSink).expect("same-handle check");
    assert_eq!(report.structure, StructureVerdict::Sound, "{report:?}");
    assert!(!fs.allocator().expect("allocator").needs_rebuild);
    fs.write_at(root, b"new", 1, b"after recovery")
        .expect("same-handle write");
    let expected_used = fs.used_blocks();
    let expected_free = fs.free_count;
    FilesystemWrite::flush(&mut fs).expect("sync recovered mount");

    let bytes = fs.into_block().expect("the volume closes").bytes();
    let mut reopened =
        ARXFS::open(MemBlock::from_bytes(bytes, CRASH_BS, CRASH_BC), &TEST_KEY).expect("reopen");
    assert_eq!(reopened.used_blocks(), expected_used);
    assert_eq!(reopened.free_count, expected_free);
}

#[test]
fn a_failed_sync_rebuilds_before_same_handle_growth() {
    let (mut fs, _slot) = volatile_volume(Publish::PerOperation);
    let root = fs.root();
    fs.write_at(root, b"new", 0, VOLATILE_PAYLOAD)
        .expect("write");
    fs.block_mut().fail_flush = true;
    assert_eq!(
        FilesystemWrite::flush(&mut fs),
        Err(DriverError::DeviceFault)
    );
    fs.block_mut().fail_flush = false;
    let grown_blocks = CRASH_BC + 1024;
    fs.block_mut().enlarge_to(grown_blocks);
    assert_eq!(fs.grow(), Ok(1024));
    let expected_used = fs.used_blocks();
    let expected_free = fs.free_count;
    FilesystemWrite::flush(&mut fs).expect("sync grown volume");

    let bytes = fs.into_block().expect("the volume closes").bytes();
    let mut reopened = ARXFS::open(
        MemBlock::from_bytes(bytes, CRASH_BS, grown_blocks),
        &TEST_KEY,
    )
    .expect("reopen grown volume");
    assert_eq!(reopened.used_blocks(), expected_used, "grown map mismatch");
    assert_eq!(reopened.free_count, expected_free);
}

#[test]
fn confirming_map_invalidation_leaves_authoritative_blocks_staged() {
    let (mut fs, _slot) = volatile_volume(Publish::PerOperation);
    assert!(fs.map_is_stamped_clean());
    let authoritative = fs.map_region_start() + fs.map_region_blocks();
    let block = [0xA5; MAX_BLOCK_SIZE];
    let block_size = fs.block_size;
    fs.dirty
        .stage(
            WritePhase::BeforeBarrier,
            authoritative,
            &block[..block_size],
        )
        .expect("stage authoritative block");
    fs.map_confirm_dirty().expect("confirm invalidation");
    assert!(!fs.map_is_stamped_clean());
    assert!(
        fs.block_mut().volatile_blocks().is_empty(),
        "the invalid marker did not cross its barrier"
    );
    assert_eq!(
        fs.dirty.staged_in(authoritative, 1),
        1,
        "map invalidation drained unrelated transaction state"
    );
}

// ---------------------------------------------------------------------------
// Sparse files: ZERO/Hole extents (`plans/SPARSE.md`). ARXFS represents a
// hole implicitly as a gap between extent mappings (permitted by); an
// all-zero logical record is stored as such a hole rather than a physical
// data record, and reads of a hole synthesise zero bytes.
// ---------------------------------------------------------------------------

/// The number of *physical data blocks* file `ino` maps: the sum of its
/// extent-run lengths. A fully sparse range contributes nothing, so a hole
/// costs zero data payload (`plans/SPARSE.md` §14).
fn mapped_block_count(fs: &mut ARXFS<MemBlock>, ino: u32) -> u64 {
    let inode = fs.read_inode(ino).expect("read inode");
    let spec = extent_spec(ino);
    let total = fs.total_blocks;
    tree_entries(fs, inode.extent_root, spec)
        .iter()
        .map(|(_, value)| Extent::decode(value, total).expect("extent decodes").len)
        .sum()
}

/// Assert file `ino`'s committed extent map is sorted by logical offset and
/// holds no overlapping runs (`plans/SPARSE.md` §7).
fn assert_extents_ordered_and_disjoint(fs: &mut ARXFS<MemBlock>, ino: u32) {
    let inode = fs.read_inode(ino).expect("read inode");
    let spec = extent_spec(ino);
    let entries = tree_entries(fs, inode.extent_root, spec);
    let total = fs.total_blocks;
    let mut prev_end = 0u64;
    for (start, value) in entries {
        assert!(start >= prev_end, "extent at {start} overlaps prior run");
        let ext = Extent::decode(&value, total).expect("extent decodes");
        prev_end = start + ext.len;
    }
}

/// Read the whole of file `name` under the root and assert every byte is zero.
fn assert_reads_all_zero(fs: &mut ARXFS<MemBlock>, name: &[u8], len: usize) {
    let node = fs.lookup(fs.root(), name).expect("lookup");
    let got = read_all(fs, node, len);
    assert!(got.iter().all(|&b| b == 0), "sparse read must be all zero");
}

#[test]
fn sparse_ten_mib_zero_file_allocates_no_data_payload() {
    // a 10 MiB all-zero file has a 10 MiB logical size, maps zero
    // physical data blocks, and reads back as zeroes. The volume is encrypted
    // (`TEST_KEY`), so this also covers: no plaintext data payload
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
    let bytes = fs.into_block().expect("the volume closes").bytes();
    let mut fs = ARXFS::open(MemBlock::from_bytes(bytes, 4096, 4096), &TEST_KEY).expect("reopen");
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
    // writing non-zero data into the middle of a sparse file leaves the
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
    // overwriting existing data with zeroes makes the range read as
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
    // growing a file with no written data creates a hole; reads of the
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
    // shrinking frees data extents beyond the new EOF through the normal
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
    // cloning a sparse file keeps its holes metadata-only and creates no
    // dedupe chunk for any zero range (a zero range is never a chunk).
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
    // scrub and check both pass on a sparse file. Because a hole
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
    assert_eq!(
        report.pass,
        PassVerdict::Complete,
        "scrub completes on a sparse file"
    );
    assert!(!report.found_faults(), "a sparse file is clean: {report:?}");

    let check = fs.check(&GrantAll, &NullSink).expect("check");
    assert_eq!(
        check.structure,
        StructureVerdict::Sound,
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
    // an all-zero record produces no zstd payload (it is a hole with no
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
    let ff_phys = data_block_phys(&mut fs, b"ff", 0);
    assert_eq!(
        stored_form_at(&mut fs, ff_phys),
        StoredForm::Raw,
        "a non-zero constant block is a raw physical record, never a hole"
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
    // the maximum name length matches ext4 (255 bytes). A maximum-length
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

    let bytes = fs.into_block().expect("the volume closes").bytes();
    let mut reopened =
        ARXFS::open(MemBlock::from_bytes(bytes, 4096, 256), &TEST_KEY).expect("reopen");
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
    // ARXFS allows every byte ext4 allows in a name — anything except `/`
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
    // names are compared byte-for-byte, so casing distinguishes entries.
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
    let mut cursor = 0u64;
    let mut buf = alloc::vec![0u8; NAME_MAX];
    while let Some(entry) = fs.read_dir(root, cursor, &mut buf).expect("read_dir") {
        seen += 1;
        cursor = entry.next_cursor;
    }
    assert_eq!(seen, names.len() as u64, "every entry enumerates back");
}

/// Allocate files of one data block each until the volume is full.
fn fill_until_no_space(fs: &mut ARXFS<MemBlock>) {
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
    let bytes = fs.into_block().expect("the volume closes").bytes();
    let mut reopened =
        ARXFS::open(MemBlock::from_bytes(bytes, 512, 1024), &TEST_KEY).expect("reopen");
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
    let bytes = fs.into_block().expect("the volume closes").bytes();
    let truncated = MemBlock::from_bytes(bytes, 512, 200);
    assert!(matches!(
        ARXFS::open(truncated, &TEST_KEY),
        Err(DriverError::BadMagic)
    ));
}

#[test]
fn the_transaction_private_tracker_is_sparse_not_one_entry_per_block() {
    // Regression: the per-transaction private-block tracker must be a sparse
    // set bounded by the transaction's working set, never a dense structure
    // sized to the whole device. The previous `vec![false; total_blocks]`
    // allocated one byte per device block, so a real multi-GB volume's mount
    // ballooned to tens of MiB and exhausted the kernel heap (the Raspberry
    // Pi 4 eMMC2 boot OOM); a sparse set scales with the work, not the disk.
    let mut fs = fmt(512, 4096, 32);
    assert_eq!(fs.total_blocks, 4096);
    // A freshly formatted (and committed) volume holds no private blocks: the
    // tracker is empty, not `total_blocks` entries long as the dense form was.
    assert!(
        fs.allocator().expect("writable").txn_private.is_empty(),
        "the tracker must start empty, not pre-sized to the device block count"
    );

    // A committed write marks only the handful of blocks the transaction
    // touches private, then clears them at commit — so between operations the
    // tracker is empty and never approaches the device block count.
    let root = fs.root();
    fs.create(root, b"f", NodeKind::RegularFile)
        .expect("create");
    let body = alloc::vec![0xA5u8; 4000];
    assert_eq!(fs.write_at(root, b"f", 0, &body), Ok(4000));
    assert!(
        fs.allocator().expect("writable").txn_private.is_empty(),
        "a committed transaction clears its private-block set"
    );
}

#[test]
fn the_allocation_map_costs_a_bounded_cache_not_one_entry_per_block() {
    // Regression: mounting must never cost memory proportional to the device.
    // The first form allocated a dense `Vec<u64>` bitmap at mount, so a real
    // multi-GB eMMC volume's mount alone exhausted the bounded kernel heap
    // (the Raspberry Pi 4 boot OOM); the sparse used-set that replaced it
    // still grew with the blocks in use. The paged on-disk map costs a fixed
    // cache of region blocks, whatever the volume size or the working set.
    let mut fs = fmt(512, 65536, 32);
    assert_eq!(fs.total_blocks, 65536);
    assert!(
        fs.map_cached_blocks() <= MAX_CACHED_PAGES,
        "the resident map is a bounded cache, not one entry per device block \
         (cached {} of at most {MAX_CACHED_PAGES})",
        fs.map_cached_blocks()
    );
    // The region the map occupies on the device is a rounding error of it.
    assert!(
        fs.map_region_blocks() * 64 < fs.total_blocks,
        "the map region is {} of {} blocks",
        fs.map_region_blocks(),
        fs.total_blocks
    );
    // Every used block is a real in-range block.
    let before = fs.used_blocks();
    assert!(before.iter().all(|&b| b < fs.total_blocks));

    // A committed write grows the used set by only the blocks the file
    // occupies, and the resident cache stays bounded.
    let root = fs.root();
    fs.create(root, b"f", NodeKind::RegularFile)
        .expect("create");
    let body = alloc::vec![0xA5u8; 4000];
    assert_eq!(fs.write_at(root, b"f", 0, &body), Ok(4000));
    assert!(
        fs.used_blocks().len() < before.len() + 64,
        "a small write must add only a handful of used blocks"
    );
    assert!(fs.map_cached_blocks() <= MAX_CACHED_PAGES);
}

#[test]
fn a_volume_smaller_than_the_device_mounts_and_leaves_the_tail_unused() {
    // Format a 256-block volume, then present the same image on a larger
    // (1024-block) device. It mounts at its committed size; the surplus tail is
    // simply unused until a grow.
    let fs = fmt(512, 256, 32);
    let mut bytes = fs.into_block().expect("the volume closes").bytes();
    bytes.resize(512 * 1024, 0);
    let mut reopened =
        ARXFS::open(MemBlock::from_bytes(bytes, 512, 1024), &TEST_KEY).expect("reopen larger");
    assert_eq!(reopened.total_blocks, 256, "mounts at the committed size");
    let added = reopened.grow().expect("grow into the surplus");
    assert_eq!(added, 1024 - 256);
    assert_eq!(reopened.total_blocks, 1024);
}

// ---------------------------------------------------------------------------
// Scaling: a 100 TiB volume must format, mount, and serve on a tiny-RAM
// machine. Every in-RAM structure scales with the working set, never with the
// device block count (100 TB+ volumes on a machine with as little as 1 GiB of
// RAM).
// ---------------------------------------------------------------------------

#[test]
fn a_100_tib_volume_formats_mounts_and_serves_with_working_set_bounded_memory() {
    // The whole point of the sparse free-space design: a 100 TiB device
    // (~26.8 billion 4 KiB blocks) must be usable without any in-RAM structure
    // sized to that block count. A dense per-block bitmap would need ~3 GiB
    // resident at 4 KiB blocks (and ~24 GiB at 512-byte blocks) and could never
    // mount on a 1 GiB machine; the sparse used-set + free *count* tracks only
    // the blocks actually in use.
    let mut fs = fmt_huge();
    assert_eq!(fs.total_blocks, HUGE_BLOCK_COUNT);

    // A freshly formatted volume holds only a bounded cache of map blocks —
    // a few dozen at most, never one entry per device block.
    assert!(
        fs.map_cached_blocks() <= MAX_CACHED_PAGES,
        "a near-empty 100 TiB volume must hold only a bounded map cache \
         (cached {})",
        fs.map_cached_blocks()
    );
    // The map region costs well under a thousandth of the device, and
    // essentially the whole 100 TiB is free.
    let reserved = fs.map_region_blocks();
    assert!(
        reserved * 1000 < HUGE_BLOCK_COUNT,
        "the map region is {reserved} of {HUGE_BLOCK_COUNT} blocks"
    );
    assert!(
        fs.free_count > HUGE_BLOCK_COUNT - reserved - 2000,
        "almost the entire device is free"
    );
    assert!(fs.free_count < HUGE_BLOCK_COUNT);

    // Serve real I/O: create a file, write several blocks, read them back.
    let root = fs.root();
    fs.create(root, b"big", NodeKind::RegularFile)
        .expect("create on a huge volume");
    let body = alloc::vec![0xC3u8; 200_000];
    assert_eq!(fs.write_at(root, b"big", 0, &body), Ok(200_000));
    let node = fs.lookup(root, b"big").expect("lookup");
    let mut back = alloc::vec![0u8; 200_000];
    assert_eq!(fs.read_at(node, 0, &mut back), Ok(200_000));
    assert_eq!(back, body);

    // Serving that file left the resident map a bounded cache, not something
    // proportional to the 100 TiB device.
    assert!(
        fs.map_cached_blocks() <= MAX_CACHED_PAGES,
        "serving a 200 KiB file must leave the map cache bounded (cached {})",
        fs.map_cached_blocks()
    );
    // And left nothing staged: the write-back set is the transaction's own
    // working set, drained at its commit, so a huge volume pins no more of it
    // than a small one.
    assert_eq!(fs.dirty.len(), 0, "the commit drained its dirty set");

    // The device itself stored only the working set — proof the test models a
    // 100 TiB volume without a 100 TiB backing allocation, and that the driver
    // never touched a volume-proportional span of blocks.
    let device = fs.into_block().expect("the volume closes");
    assert!(
        device.stored_blocks() < 4000,
        "format + serve touched only the working set of blocks (stored {})",
        device.stored_blocks()
    );

    // Reopening the huge volume is likewise working-set-bounded and reads the
    // committed content back unchanged.
    let mut reopened = ARXFS::open(device, &TEST_KEY).expect("reopen 100 TiB volume");
    assert_eq!(reopened.total_blocks, HUGE_BLOCK_COUNT);
    assert!(
        reopened.map_cached_blocks() <= MAX_CACHED_PAGES,
        "mounting a 100 TiB volume holds only a bounded map cache (cached {})",
        reopened.map_cached_blocks()
    );
    let node = reopened.lookup(reopened.root(), b"big").expect("lookup");
    let mut back = alloc::vec![0u8; 200_000];
    assert_eq!(reopened.read_at(node, 0, &mut back), Ok(200_000));
    assert_eq!(back, body);
}

#[test]
fn metadata_allocates_at_the_high_end_of_a_huge_volume_without_a_whole_volume_scan() {
    // Metadata (the transaction root, tree nodes) is allocated by scanning
    // *downward* from the top of the device. On a 100 TiB volume the top block
    // is ~26.8 billion; the allocator must reach it via the cursor in O(1),
    // never by walking the whole volume. The committed root therefore lives
    // near the top of the device, proving the high-end allocation works at
    // scale.
    let fs = fmt_huge();
    let top = HUGE_BLOCK_COUNT - 1;
    let device = fs.into_block().expect("the volume closes");
    // Some block in the top few of the device was written (the committed root
    // pair), confirming the downward metadata cursor reached the high end.
    let highest_written = device
        .blocks
        .keys()
        .copied()
        .next_back()
        .expect("format wrote metadata");
    assert!(
        highest_written >= top - 8,
        "metadata must allocate at the very top of the device (highest written {highest_written}, top {top})"
    );
}

#[test]
fn the_pending_discard_queue_is_capped_independent_of_volume_size() {
    // Regression: the pending-discard queue must be bounded by a fixed,
    // volume-independent ceiling, never by the device block count. The previous
    // `pending_discard.len() < as_usize(self.total_blocks)` cap let the queue
    // grow toward the block count — `as_usize` saturates to `usize::MAX` on a
    // huge volume — so a long-running mount could grow it until the bounded
    // kernel heap was exhausted. A fixed cap keeps the worst-case footprint
    // constant whatever the device size.
    let mut fs = fmt_huge();
    // Every second block, so no two runs coalesce and each enqueue really does
    // cost an entry: the cap is on runs, which is what the queue's memory is.
    let start = RING_BLOCKS + 1;
    for run in 0..MAX_PENDING_DISCARD_RUNS as u64 + 1000 {
        fs.enqueue_discard_run(start + run * 2, 1);
    }
    assert_eq!(
        fs.pending_discard_runs(),
        MAX_PENDING_DISCARD_RUNS,
        "the discard queue must cap at MAX_PENDING_DISCARD_RUNS, never grow with the volume"
    );
}

#[test]
fn one_freed_run_of_any_length_costs_the_discard_queue_one_entry() {
    // The queue holds runs, so a very large free is one entry and the blocks it
    // covers are still all accounted for. A per-block queue capped the same way
    // would have dropped everything past the cap.
    let mut fs = fmt_huge();
    let start = RING_BLOCKS + 1;
    fs.enqueue_discard_run(start, 1 << 20);
    assert_eq!(fs.pending_discard_runs(), 1);
    assert_eq!(fs.pending_discard_count(), 1 << 20);
    // A neighbouring run extends the same entry rather than adding one.
    fs.enqueue_discard_run(start + (1 << 20), 16);
    assert_eq!(fs.pending_discard_runs(), 1);
    assert_eq!(fs.pending_discard_count(), (1 << 20) + 16);
}

#[test]
fn rename_within_directory_moves_a_file_preserving_contents() {
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"a", NodeKind::RegularFile)
        .expect("create");
    assert_eq!(fs.write_at(root, b"a", 0, b"hello"), Ok(5));
    fs.rename(root, b"a", root, b"b").expect("rename");
    assert_eq!(fs.lookup(root, b"a"), Err(DriverError::NotFound));
    let node = fs.lookup(root, b"b").expect("dst present");
    let mut back = [0u8; 5];
    assert_eq!(fs.read_at(node, 0, &mut back), Ok(5));
    assert_eq!(&back, b"hello");
}

#[test]
fn rename_self_is_a_noop() {
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"a", NodeKind::RegularFile)
        .expect("create");
    fs.rename(root, b"a", root, b"a").expect("noop");
    assert!(fs.lookup(root, b"a").is_ok());
}

#[test]
fn rename_missing_source_fails_closed() {
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    assert_eq!(
        fs.rename(root, b"x", root, b"y"),
        Err(DriverError::NotFound)
    );
}

#[test]
fn rename_across_directories_moves_a_file() {
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"src", NodeKind::Directory)
        .expect("mkdir src");
    fs.create(root, b"dst", NodeKind::Directory)
        .expect("mkdir dst");
    let src = fs.lookup(root, b"src").unwrap();
    let dst = fs.lookup(root, b"dst").unwrap();
    fs.create(src, b"f", NodeKind::RegularFile).expect("create");
    assert_eq!(fs.write_at(src, b"f", 0, b"data"), Ok(4));
    fs.rename(src, b"f", dst, b"g").expect("rename across");
    assert_eq!(fs.lookup(src, b"f"), Err(DriverError::NotFound));
    let node = fs.lookup(dst, b"g").expect("moved");
    let mut back = [0u8; 4];
    assert_eq!(fs.read_at(node, 0, &mut back), Ok(4));
    assert_eq!(&back, b"data");
}

#[test]
fn rename_overwrites_an_existing_regular_file() {
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"a", NodeKind::RegularFile).unwrap();
    fs.create(root, b"b", NodeKind::RegularFile).unwrap();
    assert_eq!(fs.write_at(root, b"a", 0, b"AAAA"), Ok(4));
    assert_eq!(fs.write_at(root, b"b", 0, b"BB"), Ok(2));
    fs.rename(root, b"a", root, b"b").expect("overwrite");
    assert_eq!(fs.lookup(root, b"a"), Err(DriverError::NotFound));
    let node = fs.lookup(root, b"b").unwrap();
    let mut back = [0u8; 4];
    assert_eq!(fs.read_at(node, 0, &mut back), Ok(4));
    assert_eq!(&back, b"AAAA");
}

#[test]
fn rename_replaces_an_empty_directory() {
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"a", NodeKind::Directory).unwrap();
    fs.create(root, b"b", NodeKind::Directory).unwrap();
    let a = fs.lookup(root, b"a").unwrap();
    fs.create(a, b"x", NodeKind::RegularFile).unwrap();
    fs.rename(root, b"a", root, b"b")
        .expect("dir over empty dir");
    assert_eq!(fs.lookup(root, b"a"), Err(DriverError::NotFound));
    let b = fs.lookup(root, b"b").unwrap();
    assert!(fs.lookup(b, b"x").is_ok());
}

#[test]
fn rename_refuses_kind_mismatch_and_nonempty_dir_target() {
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"file", NodeKind::RegularFile).unwrap();
    fs.create(root, b"dir", NodeKind::Directory).unwrap();
    assert_eq!(
        fs.rename(root, b"file", root, b"dir"),
        Err(DriverError::Unsupported)
    );
    assert_eq!(
        fs.rename(root, b"dir", root, b"file"),
        Err(DriverError::Unsupported)
    );
    fs.create(root, b"dir2", NodeKind::Directory).unwrap();
    let dir2 = fs.lookup(root, b"dir2").unwrap();
    fs.create(dir2, b"child", NodeKind::RegularFile).unwrap();
    assert_eq!(
        fs.rename(root, b"dir", root, b"dir2"),
        Err(DriverError::DirectoryNotEmpty)
    );
}

#[test]
fn rename_moves_a_directory_across_parents_and_repoints_dotdot() {
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"p1", NodeKind::Directory).unwrap();
    fs.create(root, b"p2", NodeKind::Directory).unwrap();
    let p1 = fs.lookup(root, b"p1").unwrap();
    let p2 = fs.lookup(root, b"p2").unwrap();
    fs.create(p1, b"d", NodeKind::Directory).unwrap();
    let d = fs.lookup(p1, b"d").unwrap();
    fs.create(d, b"leaf", NodeKind::RegularFile).unwrap();
    fs.rename(p1, b"d", p2, b"d").expect("move dir");
    assert_eq!(fs.lookup(p1, b"d"), Err(DriverError::NotFound));
    let moved = fs.lookup(p2, b"d").expect("moved dir");
    assert!(fs.lookup(moved, b"leaf").is_ok());
    assert_eq!(fs.lookup(moved, b"..").unwrap(), p2);
}

#[test]
fn rename_refuses_moving_a_directory_into_its_own_subtree() {
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"a", NodeKind::Directory).unwrap();
    let a = fs.lookup(root, b"a").unwrap();
    fs.create(a, b"b", NodeKind::Directory).unwrap();
    let b = fs.lookup(a, b"b").unwrap();
    assert_eq!(
        fs.rename(root, b"a", b, b"a"),
        Err(DriverError::DirectoryCycle)
    );
    assert_eq!(
        fs.rename(root, b"a", a, b"x"),
        Err(DriverError::DirectoryCycle)
    );
}

#[test]
fn rename_rejects_bad_destination_name() {
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"a", NodeKind::RegularFile).unwrap();
    assert_eq!(
        fs.rename(root, b"a", root, b""),
        Err(DriverError::LengthOutOfRange)
    );
    assert_eq!(
        fs.rename(root, b"a", root, b".."),
        Err(DriverError::Unsupported)
    );
}

// ---------------------------------------------------------------------------
// Extended file metadata (`docs/src/filesystem/arxfs-spec.md` §21).
// ---------------------------------------------------------------------------

/// Read the whole value of attribute `key` on `node` into an owned vector, or
/// `None` when the attribute is absent.
fn get_attr_vec(fs: &mut ARXFS<MemBlock>, node: NodeId, key: &[u8]) -> Option<alloc::vec::Vec<u8>> {
    let mut buf = alloc::vec![0u8; MAX_BLOCK_SIZE];
    fs.get_attr(node, key, &mut buf)
        .expect("get_attr")
        .map(|n| buf[..n].to_vec())
}

/// Every attribute key on `node`, in on-disk order.
fn list_attr_keys(fs: &mut ARXFS<MemBlock>, node: NodeId) -> alloc::vec::Vec<alloc::vec::Vec<u8>> {
    let mut out = alloc::vec::Vec::new();
    let mut i = 0u64;
    let mut buf = alloc::vec![0u8; MAX_BLOCK_SIZE];
    while let Some(n) = fs.list_attr(node, i, &mut buf).expect("list_attr") {
        out.push(buf[..n].to_vec());
        i += 1;
    }
    out
}

/// Whether `haystack` contains `needle` as a contiguous subsequence.
fn contains_subsequence(haystack: &[u8], needle: &[u8]) -> bool {
    needle.len() <= haystack.len()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

#[test]
fn attributes_round_trip_across_namespaces_and_remount() {
    let mut fs = fmt(4096, 512, 64);
    let root = fs.root();
    fs.create(root, b"file", NodeKind::RegularFile)
        .expect("create");
    let node = fs.lookup(root, b"file").expect("lookup");

    fs.set_attr(node, b"user.comment", b"hello")
        .expect("set user");
    fs.set_attr(node, b"acorn.filetype", b"fff")
        .expect("set acorn");
    fs.set_attr(node, b"mac.type", b"TEXT").expect("set mac");

    assert_eq!(
        get_attr_vec(&mut fs, node, b"user.comment").as_deref(),
        Some(&b"hello"[..])
    );
    assert_eq!(
        get_attr_vec(&mut fs, node, b"acorn.filetype").as_deref(),
        Some(&b"fff"[..])
    );
    assert_eq!(
        list_attr_keys(&mut fs, node),
        alloc::vec![
            b"user.comment".to_vec(),
            b"acorn.filetype".to_vec(),
            b"mac.type".to_vec()
        ]
    );

    // Attributes survive a remount (they are committed metadata).
    let bytes = fs.into_block().expect("the volume closes").bytes();
    let mut fs = ARXFS::open(MemBlock::from_bytes(bytes, 4096, 512), &TEST_KEY).expect("reopen");
    let root = fs.root();
    let node = fs.lookup(root, b"file").expect("lookup after remount");
    assert_eq!(
        get_attr_vec(&mut fs, node, b"mac.type").as_deref(),
        Some(&b"TEXT"[..])
    );

    // Replace, then remove; a removed attribute is gone and removing it again
    // fails closed.
    fs.set_attr(node, b"user.comment", b"changed")
        .expect("replace");
    assert_eq!(
        get_attr_vec(&mut fs, node, b"user.comment").as_deref(),
        Some(&b"changed"[..])
    );
    fs.remove_attr(node, b"user.comment").expect("remove");
    assert_eq!(get_attr_vec(&mut fs, node, b"user.comment"), None);
    assert_eq!(
        fs.remove_attr(node, b"user.comment"),
        Err(DriverError::NotFound)
    );
}

#[test]
fn attribute_keys_are_case_sensitive() {
    let mut fs = fmt(4096, 512, 64);
    let root = fs.root();
    fs.create(root, b"f", NodeKind::RegularFile)
        .expect("create");
    let node = fs.lookup(root, b"f").expect("lookup");
    fs.set_attr(node, b"user.Name", b"upper").expect("set");
    fs.set_attr(node, b"user.name", b"lower").expect("set");
    assert_eq!(
        get_attr_vec(&mut fs, node, b"user.Name").as_deref(),
        Some(&b"upper"[..])
    );
    assert_eq!(
        get_attr_vec(&mut fs, node, b"user.name").as_deref(),
        Some(&b"lower"[..])
    );
}

#[test]
fn attribute_grammar_and_bounds_fail_closed() {
    let mut fs = fmt(4096, 512, 64);
    let root = fs.root();
    fs.create(root, b"f", NodeKind::RegularFile)
        .expect("create");
    let node = fs.lookup(root, b"f").expect("lookup");

    // Unknown namespace and malformed keys are rejected, never stored.
    assert_eq!(
        fs.set_attr(node, b"bogus.k", b"v"),
        Err(DriverError::OutOfRange)
    );
    assert_eq!(
        fs.set_attr(node, b"nodot", b"v"),
        Err(DriverError::OutOfRange)
    );

    // A bad key on get/remove is a fail-closed rejection, not "absent".
    let mut buf = [0u8; 8];
    assert_eq!(
        fs.get_attr(node, b"bogus.k", &mut buf),
        Err(DriverError::OutOfRange)
    );
    assert_eq!(
        fs.remove_attr(node, b"bogus.k"),
        Err(DriverError::OutOfRange)
    );

    // An oversize value is rejected.
    let huge = alloc::vec![b'x'; tairix_fsmeta::VALUE_MAX + 1];
    assert_eq!(
        fs.set_attr(node, b"user.k", &huge),
        Err(DriverError::LengthOutOfRange)
    );

    // A too-small get buffer fails closed rather than truncating.
    fs.set_attr(node, b"user.k", b"abcdef").expect("set");
    let mut tiny = [0u8; 2];
    assert_eq!(
        fs.get_attr(node, b"user.k", &mut tiny),
        Err(DriverError::BufferTooSmall)
    );
    // The prior value is intact after every rejected call.
    assert_eq!(
        get_attr_vec(&mut fs, node, b"user.k").as_deref(),
        Some(&b"abcdef"[..])
    );
}

#[test]
fn attribute_set_that_overflows_the_block_fails_closed_and_leaves_prior_intact() {
    // A 512-byte block leaves a small attribute payload, so a value that fits
    // the fsmeta bound can still overflow one metadata block: it must fail
    // closed with NoSpace and leave the previously-stored attribute intact.
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"f", NodeKind::RegularFile)
        .expect("create");
    let node = fs.lookup(root, b"f").expect("lookup");
    fs.set_attr(node, b"user.keep", b"safe").expect("set small");

    let big = alloc::vec![b'x'; 1024];
    assert_eq!(
        fs.set_attr(node, b"user.big", &big),
        Err(DriverError::NoSpace)
    );
    // The block-overflow rejection did not disturb the existing attribute.
    assert_eq!(
        get_attr_vec(&mut fs, node, b"user.keep").as_deref(),
        Some(&b"safe"[..])
    );
    assert_eq!(get_attr_vec(&mut fs, node, b"user.big"), None);
}

#[test]
fn attributes_are_encrypted_at_rest() {
    let mut fs = fmt(4096, 512, 64);
    let root = fs.root();
    fs.create(root, b"f", NodeKind::RegularFile)
        .expect("create");
    let node = fs.lookup(root, b"f").expect("lookup");
    let marker_key = b"user.secretmarkerkey";
    let marker_value = b"PLAINTEXT-ATTR-MARKER-9F3A";
    fs.set_attr(node, marker_key, marker_value).expect("set");
    let bytes = fs.into_block().expect("the volume closes").bytes();
    // Neither the key nor the value appears in cleartext on the raw device.
    assert!(
        !contains_subsequence(&bytes, marker_value),
        "attribute value leaked in plaintext"
    );
    assert!(
        !contains_subsequence(&bytes, b"secretmarkerkey"),
        "attribute key leaked in plaintext"
    );
}

#[test]
fn a_read_only_mount_refuses_attribute_mutation() {
    let mut fs = fmt(4096, 512, 64);
    let root = fs.root();
    fs.create(root, b"f", NodeKind::RegularFile)
        .expect("create");
    let node = fs.lookup(root, b"f").expect("lookup");
    fs.set_attr(node, b"user.k", b"v").expect("set");
    let bytes = fs.into_block().expect("the volume closes").bytes();

    let mut ro = ARXFS::open_read_only(MemBlock::from_bytes(bytes, 4096, 512), &TEST_KEY)
        .expect("read-only mount");
    let node = ro.lookup(ro.root(), b"f").expect("lookup ro");
    // Reading an attribute on a read-only mount still works.
    assert_eq!(
        get_attr_vec(&mut ro, node, b"user.k").as_deref(),
        Some(&b"v"[..])
    );
    // Mutating one fails closed.
    assert_eq!(
        ro.set_attr(node, b"user.k", b"w"),
        Err(DriverError::PermissionDenied)
    );
    assert_eq!(
        ro.remove_attr(node, b"user.k"),
        Err(DriverError::PermissionDenied)
    );
}

#[test]
fn setting_an_attribute_is_one_transaction_and_crash_replay_never_tears() {
    // Baseline: a committed file with one attribute. The crashed trial sets a
    // second attribute in a single transaction, so a post-crash mount must see
    // either the prior set {a} or the new set {a,b} — never a torn/undecodable
    // attribute block.
    let mut base = fmt(512, 256, 32);
    let root = base.root();
    base.create(root, b"f", NodeKind::RegularFile)
        .expect("create");
    let node = base.lookup(root, b"f").expect("lookup");
    base.set_attr(node, b"user.a", b"aaa").expect("set a");
    let baseline = base.into_block().expect("the volume closes").bytes();

    for budget in 0..48u32 {
        let mut dev = MemBlock::from_bytes(baseline.clone(), 512, 256);
        dev.write_budget = Some(budget);
        let mut fs = ARXFS::open(dev, &TEST_KEY).expect("baseline opens");
        let node = fs.lookup(fs.root(), b"f").expect("lookup");
        let _ = fs.set_attr(node, b"user.b", b"bbb");
        let bytes = fs.into_block().expect("the volume closes").bytes();

        let mut fs = ARXFS::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY)
            .expect("post-crash mount always succeeds");
        let node = fs.lookup(fs.root(), b"f").expect("f survives");
        // The prior attribute is always intact.
        assert_eq!(
            get_attr_vec(&mut fs, node, b"user.a").as_deref(),
            Some(&b"aaa"[..])
        );
        // The new attribute is present in full or absent — never partial.
        if let Some(v) = get_attr_vec(&mut fs, node, b"user.b") {
            assert_eq!(v, b"bbb");
        }
    }
}

#[test]
fn removing_a_file_frees_its_attribute_block() {
    // A file with attributes is removed, then many files are created and
    // removed in a loop. If the attribute block leaked or double-freed, the
    // free-space rebuild at remount would diverge and the mount would fail; a
    // clean remount proves the block was reclaimed exactly once.
    let mut fs = fmt(4096, 512, 64);
    let root = fs.root();
    fs.create(root, b"victim", NodeKind::RegularFile)
        .expect("create");
    let node = fs.lookup(root, b"victim").expect("lookup");
    fs.set_attr(node, b"user.a", b"value").expect("set");
    fs.set_attr(node, b"acorn.filetype", b"fff").expect("set");
    fs.remove(root, b"victim").expect("remove");

    for i in 0..32u32 {
        let name = alloc::format!("t{i}");
        fs.create(root, name.as_bytes(), NodeKind::RegularFile)
            .expect("create");
        let n = fs.lookup(root, name.as_bytes()).expect("lookup");
        fs.set_attr(n, b"user.tag", b"x").expect("set");
        fs.remove(root, name.as_bytes()).expect("remove");
    }

    let bytes = fs.into_block().expect("the volume closes").bytes();
    let free_before = fs_free_count_after_reopen(bytes.clone());
    // A second identical cycle must return to the same free-block count: no
    // slow leak of attribute blocks.
    let mut fs = ARXFS::open(MemBlock::from_bytes(bytes, 4096, 512), &TEST_KEY).expect("reopen");
    let root = fs.root();
    fs.create(root, b"again", NodeKind::RegularFile)
        .expect("create");
    let n = fs.lookup(root, b"again").expect("lookup");
    fs.set_attr(n, b"user.a", b"value").expect("set");
    fs.remove(root, b"again").expect("remove");
    let after = fs_free_count_after_reopen(fs.into_block().expect("the volume closes").bytes());
    assert_eq!(
        free_before, after,
        "attribute blocks leaked across a create/remove cycle"
    );
}

/// Reopen a stored image and report its free-block count (a proxy for "no
/// blocks leaked").
fn fs_free_count_after_reopen(bytes: alloc::vec::Vec<u8>) -> u64 {
    let fs = ARXFS::open(MemBlock::from_bytes(bytes, 4096, 512), &TEST_KEY).expect("reopen");
    fs.free_count
}

#[test]
fn reflink_copies_attributes_independently() {
    let mut fs = fmt(4096, 512, 64);
    let root = fs.root();
    fs.create(root, b"src", NodeKind::RegularFile)
        .expect("create");
    let src = fs.lookup(root, b"src").expect("lookup");
    assert_eq!(fs.write_at(root, b"src", 0, b"body"), Ok(4));
    fs.set_attr(src, b"user.tag", b"orig").expect("set");

    let dst = fs.reflink(root, b"src", b"copy").expect("reflink");
    // The clone carries the source's attributes.
    assert_eq!(
        get_attr_vec(&mut fs, dst, b"user.tag").as_deref(),
        Some(&b"orig"[..])
    );
    // Mutating the clone's attribute does not disturb the source's.
    fs.set_attr(dst, b"user.tag", b"clone")
        .expect("set on clone");
    assert_eq!(
        get_attr_vec(&mut fs, src, b"user.tag").as_deref(),
        Some(&b"orig"[..])
    );
    assert_eq!(
        get_attr_vec(&mut fs, dst, b"user.tag").as_deref(),
        Some(&b"clone"[..])
    );
    // Removing the source frees only its own attribute block; the clone keeps
    // its attributes and the volume remounts cleanly.
    fs.remove(root, b"src").expect("remove src");
    assert_eq!(
        get_attr_vec(&mut fs, dst, b"user.tag").as_deref(),
        Some(&b"clone"[..])
    );
    let bytes = fs.into_block().expect("the volume closes").bytes();
    let mut fs = ARXFS::open(MemBlock::from_bytes(bytes, 4096, 512), &TEST_KEY).expect("reopen");
    let dst = fs.lookup(fs.root(), b"copy").expect("copy survives");
    assert_eq!(
        get_attr_vec(&mut fs, dst, b"user.tag").as_deref(),
        Some(&b"clone"[..])
    );
}

#[test]
fn acorn_preset_metadata_round_trips_through_the_store() {
    // A synthetic ADFS Text file (&FFF) with a load/exec-encoded timestamp:
    // the driver stores the decoded filetype plus the exact raw load/exec, so
    // a later export reproduces the native fields byte-for-byte.
    let (load, exec) = preset::acorn::encode_typed(0xFFF, 0x12_3456_789A).expect("encode typed");
    let mut fs = fmt(4096, 512, 64);
    let root = fs.root();
    fs.create(root, b"doc", NodeKind::RegularFile)
        .expect("create");
    let node = fs.lookup(root, b"doc").expect("lookup");

    let filetype = preset::acorn::filetype_to_value(0xFFF).expect("filetype");
    fs.set_attr(node, b"acorn.filetype", &filetype)
        .expect("set filetype");
    fs.set_attr(node, b"acorn.loadaddr", &preset::acorn::addr_to_value(load))
        .expect("set load");
    fs.set_attr(node, b"acorn.execaddr", &preset::acorn::addr_to_value(exec))
        .expect("set exec");

    // Read back and reconstruct the native fields exactly.
    let ft = get_attr_vec(&mut fs, node, b"acorn.filetype").expect("filetype present");
    assert_eq!(
        preset::acorn::filetype_from_value(&ft).expect("decode"),
        0xFFF
    );
    let rload = preset::acorn::addr_from_value(
        &get_attr_vec(&mut fs, node, b"acorn.loadaddr").expect("load present"),
    )
    .expect("decode load");
    let rexec = preset::acorn::addr_from_value(
        &get_attr_vec(&mut fs, node, b"acorn.execaddr").expect("exec present"),
    )
    .expect("decode exec");
    assert_eq!((rload, rexec), (load, exec));
}

// --- Allocated storage (`NodeInfo::allocated` from the extent tree). ---

#[test]
fn node_info_reports_mapped_extent_allocation() {
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"file", NodeKind::RegularFile)
        .expect("create");
    let node = fs.lookup(root, b"file").expect("lookup");
    // No extent tree yet: nothing is allocated.
    assert_eq!(fs.node_info(node).expect("info").allocated, 0);

    let body = alloc::vec![7u8; 1500];
    assert_eq!(fs.write_at(root, b"file", 0, &body), Ok(1500));
    // Each data block carries the crypto, compression-descriptor, and
    // integrity trailers, so 1500 bytes span four 512-byte blocks.
    assert_eq!(fs.node_info(node).expect("info").allocated, 4 * 512);

    fs.truncate(root, b"file", 100).expect("truncate");
    // The freed tail leaves one mapped block.
    assert_eq!(fs.node_info(node).expect("info").allocated, 512);
}

#[test]
fn stats_report_tracks_allocation() {
    let mut fs = fmt(512, 256, 32);
    let before = fs.stats().expect("stats");
    // The geometry is reported verbatim; the invariants hold; the reserve is
    // withheld from the available count; and the dynamic inode tree reports
    // the honest zero pair.
    assert_eq!(before.block_size, 512);
    assert_eq!(before.total_blocks, 256);
    assert!(before.free_blocks <= before.total_blocks);
    assert_eq!(
        before.avail_blocks,
        before.free_blocks - METADATA_RESERVE,
        "the metadata reserve is withheld from ordinary data allocation"
    );
    assert_eq!((before.files, before.files_free), (0, 0));

    // Writing data consumes free blocks; the report tracks the allocator.
    let root = fs.root();
    fs.create(root, b"file", NodeKind::RegularFile)
        .expect("create");
    let body = alloc::vec![7u8; 1500];
    assert_eq!(fs.write_at(root, b"file", 0, &body), Ok(1500));
    let after = fs.stats().expect("stats");
    assert!(
        after.free_blocks < before.free_blocks,
        "allocation shrinks the free count"
    );
    assert!(after.avail_blocks <= after.free_blocks);
    assert!(after.free_blocks <= after.total_blocks);
}

/// A [`MemBlock`] that counts device reads, so a test can assert an
/// operation's I/O cost stays bounded (each read is a real device
/// round-trip plus a whole-block authentication on an encrypted volume).
struct CountingBlock {
    inner: MemBlock,
    reads: u64,
    flushes: u64,
}

impl Block for CountingBlock {
    fn geometry(&self) -> Result<tairix_abi::driver::block::BlockGeometry, DriverError> {
        self.inner.geometry()
    }
    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        self.reads += 1;
        self.inner.read_blocks(lba, buf)
    }
    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        self.inner.write_blocks(lba, buf)
    }
    fn flush(&mut self) -> Result<(), DriverError> {
        self.flushes += 1;
        self.inner.flush()
    }
}

/// A reopened multi-block directory of `files` small files, over a
/// counting device: the fixture for the cursor-listing tests.
fn counted_dir_fixture(files: u32) -> (ARXFS<CountingBlock>, NodeId) {
    let mut fs = ARXFS::format(
        MemBlock::new(4096, 8192),
        512,
        &TEST_KEY,
        &mut TestEntropy::new(),
    )
    .expect("format");
    let root = fs.root();
    let dir = fs.create(root, b"docs", NodeKind::Directory).expect("dir");
    for i in 0..files {
        let name = alloc::format!("f{i}.txt").into_bytes();
        fs.create(dir, &name, NodeKind::RegularFile)
            .expect("create");
        fs.write_at(dir, &name, 0, b"hello world").expect("write");
    }
    fs.flush().expect("flush");
    let bytes = fs.into_block().expect("the volume closes").bytes();
    let counting = CountingBlock {
        inner: MemBlock::from_bytes(bytes, 4096, 8192),
        reads: 0,
        flushes: 0,
    };
    let fs = ARXFS::open(counting, &TEST_KEY).expect("reopen");
    (fs, dir)
}

/// A full cursor-chained listing yields every entry exactly once, in on-disk
/// order, each carrying the child's own size and allocation, and an
/// arbitrary cursor that was never returned ends the listing safely.
#[test]
fn read_dir_cursor_chain_lists_every_entry_with_its_sizes() {
    const FILES: u32 = 96;
    let (mut fs, dir) = counted_dir_fixture(FILES);
    let mut name = [0u8; MAX_BLOCK_SIZE];
    let mut seen = alloc::vec::Vec::new();
    let mut cursor = 0u64;
    while let Some(entry) = fs.read_dir(dir, cursor, &mut name).expect("read_dir") {
        assert!(entry.next_cursor > cursor, "the cursor always advances");
        assert_eq!(entry.info.kind, NodeKind::RegularFile);
        assert_eq!(entry.info.size, b"hello world".len() as u64);
        // One 4 KiB data block backs each 11-byte file.
        assert_eq!(entry.info.allocated, 4096);
        seen.push(alloc::string::String::from(
            core::str::from_utf8(&name[..entry.name_len]).expect("utf8"),
        ));
        cursor = entry.next_cursor;
    }
    assert_eq!(seen.len(), FILES as usize, "every entry listed once");
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), FILES as usize, "no entry repeated");
    // A cursor far past the directory ends the listing, fail closed.
    assert_eq!(fs.read_dir(dir, u64::MAX, &mut name), Ok(None));
}

/// Resuming a listing from a returned cursor is O(1): the read cost of
/// fetching the *last* entry directly is a small constant, independent of
/// the entries before it — never a rescan of the whole directory.
#[test]
fn read_dir_resume_cost_is_independent_of_directory_position() {
    const FILES: u32 = 96;
    let (mut fs, dir) = counted_dir_fixture(FILES);
    let mut name = [0u8; MAX_BLOCK_SIZE];
    // Walk to the last entry, remembering the cursor that names it.
    let mut cursor = 0u64;
    let mut last_at = 0u64;
    while let Some(entry) = fs.read_dir(dir, cursor, &mut name).expect("read_dir") {
        last_at = cursor;
        cursor = entry.next_cursor;
    }
    // Fetch the first and the last entry each from a cold counter: the two
    // costs must match — position in the directory must not change the
    // price of one resumed step.
    fs.block_mut().reads = 0;
    fs.read_dir(dir, 0, &mut name)
        .expect("first")
        .expect("an entry");
    let first_cost = fs.block_mut().reads;
    fs.block_mut().reads = 0;
    fs.read_dir(dir, last_at, &mut name)
        .expect("last")
        .expect("an entry");
    let last_cost = fs.block_mut().reads;
    assert_eq!(
        first_cost, last_cost,
        "resuming at the end reads as little as resuming at the start"
    );
}

// ---------------------------------------------------------------------------
// Serving-read device cost: an extent maps a contiguous physical run, so a
// read spanning one asks the device once per run rather than once per block.
// The round-trips a reading task parks across scale with the run window, not
// with the file size (`docs/src/filesystem/arxfs-spec.md` §6).
// ---------------------------------------------------------------------------

/// Content length the run-coalescing fixtures store: large enough to span
/// many runs, so a per-block cost is unmistakable next to a per-run one.
const RUN_FIXTURE_BYTES: usize = 1024 * 1024;

/// Device blocks one coalesced request covers at the 4096-byte geometry the
/// run fixtures format.
const RUN_FIXTURE_BLOCKS: usize = RUN_BYTES / 4096;

/// A volume holding `f`, a [`RUN_FIXTURE_BYTES`] file of incompressible
/// content (so it is stored as raw 1:1 extents, not compressed clusters),
/// reopened over a counting device with a cold read counter.
fn counted_file_fixture() -> (ARXFS<CountingBlock>, NodeId, alloc::vec::Vec<u8>) {
    let body = incompressible(RUN_FIXTURE_BYTES);
    let mut fs = fmt(4096, 4096, 64);
    let root = fs.root();
    fs.create(root, b"f", NodeKind::RegularFile)
        .expect("create");
    assert_eq!(fs.write_at(root, b"f", 0, &body), Ok(body.len()));
    FilesystemWrite::flush(&mut fs).expect("sync");
    let mut fs = ARXFS::open(
        counting(
            fs.into_block().expect("the volume closes").bytes(),
            4096,
            4096,
        ),
        &TEST_KEY,
    )
    .expect("reopen");
    let node = fs.lookup(fs.root(), b"f").expect("lookup");
    fs.block_mut().reads = 0;
    (fs, node, body)
}

#[test]
fn a_whole_file_read_asks_the_device_once_per_contiguous_run() {
    let (mut fs, node, body) = counted_file_fixture();
    let cap = as_usize(fs.data_capacity());
    let blocks = body.len().div_ceil(cap);
    let runs = blocks.div_ceil(RUN_FIXTURE_BLOCKS);
    let mut out = alloc::vec![0u8; body.len()];
    assert_eq!(fs.read_at(node, 0, &mut out), Ok(body.len()));
    assert_eq!(out, body, "a coalesced read returns the file exactly");
    // One data request per run, plus one extent-tree walk per run: the price
    // of the read tracks the runs it spans, never the blocks inside them.
    let whole = fs.block_mut().reads;
    let ceiling = 4 * u64::try_from(runs).unwrap();
    assert!(
        whole <= ceiling,
        "{blocks} blocks in {runs} runs must cost at most {ceiling} device \
         requests, not one per block: {whole}"
    );

    // The same bytes fetched a block at a time — the cost a per-block serving
    // path pays — measures the gap the run window buys.
    fs.block_mut().reads = 0;
    for bi in 0..blocks {
        let at = bi * cap;
        let want = cap.min(body.len() - at);
        assert_eq!(
            fs.read_at(node, u64::try_from(at).unwrap(), &mut out[..want]),
            Ok(want)
        );
    }
    let per_block = fs.block_mut().reads;
    assert!(
        whole * 8 <= per_block,
        "one call spanning {blocks} blocks must cost an order of magnitude \
         less than a call per block: {whole} vs {per_block} device requests"
    );
}

#[test]
fn a_coalesced_read_returns_the_same_bytes_from_any_offset() {
    // A read that starts inside a block, on a run boundary, or either side of
    // one still returns exactly the stored bytes: the run window changes how
    // many requests a read makes, never what it answers.
    let (mut fs, node, body) = counted_file_fixture();
    let cap = as_usize(fs.data_capacity());
    let run = RUN_FIXTURE_BLOCKS * cap;
    for start in [1, cap - 1, cap, cap + 1, run - 1, run, run + 1, 3 * run + 7] {
        for len in [1, cap + 3, run + 5, 2 * run] {
            let end = (start + len).min(body.len());
            let mut out = alloc::vec![0u8; end - start];
            let want = out.len();
            assert_eq!(
                fs.read_at(node, u64::try_from(start).unwrap(), &mut out),
                Ok(want),
                "offset {start} length {len} reads whole"
            );
            assert_eq!(out, body[start..end], "offset {start} length {len}");
        }
    }
}

#[test]
fn a_wounded_block_inside_a_coalesced_run_fails_the_read_closed() {
    // One request now fetches a whole run, but every block in it is still
    // verified on its own: a block wounded *inside* a run fails the read
    // rather than being served, and the fault stays contained to its run.
    let body = incompressible(RUN_FIXTURE_BYTES);
    let mut fs = fmt(4096, 4096, 64);
    let root = fs.root();
    fs.create(root, b"f", NodeKind::RegularFile)
        .expect("create");
    assert_eq!(fs.write_at(root, b"f", 0, &body), Ok(body.len()));
    let interior = 5u64;
    assert!(
        interior < u64::try_from(RUN_FIXTURE_BLOCKS).unwrap(),
        "the wounded block must sit inside the first run, not start it"
    );
    let phys = data_block_phys(&mut fs, b"f", interior);
    let mut bytes = fs.into_block().expect("the volume closes").bytes();
    bytes[as_usize(phys) * 4096] ^= 0x01;

    let mut fs =
        ARXFS::open(MemBlock::from_bytes(bytes, 4096, 4096), &TEST_KEY).expect("still mounts");
    let node = fs.lookup(fs.root(), b"f").expect("lookup");
    let cap = as_usize(fs.data_capacity());
    let mut out = alloc::vec![0u8; body.len()];
    assert!(
        matches!(fs.read_at(node, 0, &mut out), Err(DriverError::DeviceFault)),
        "a wounded block inside a coalesced run fails the whole read closed"
    );
    let later = RUN_FIXTURE_BLOCKS * cap;
    let mut out = alloc::vec![0u8; cap];
    assert_eq!(
        fs.read_at(node, u64::try_from(later).unwrap(), &mut out),
        Ok(cap),
        "a run that does not cover the wound still reads"
    );
    assert_eq!(out, body[later..later + cap], "and answers exactly");
}

#[test]
fn a_compressed_cluster_read_asks_the_device_once_for_its_stored_run() {
    // A compressed cluster's stored blocks are contiguous too, so the frame
    // is fetched in one request instead of one per stored block.
    let mut fs = fmt(4096, 4096, 64);
    let root = fs.root();
    fs.create(root, b"c", NodeKind::RegularFile)
        .expect("create");
    // A cluster that compresses, but not to a single block: half noise, half
    // constant padding, so its stored run genuinely spans several blocks.
    let logical = as_usize(fs.data_capacity() * COMPRESS_CLUSTER_BLOCKS);
    let mut payload = incompressible(logical / 2);
    payload.resize(logical, b'.');
    assert_eq!(fs.write_at(root, b"c", 0, &payload), Ok(payload.len()));
    FilesystemWrite::flush(&mut fs).expect("sync");
    let (_, ext) = extent_of(&mut fs, b"c", 0);
    assert!(
        ext.compressed && ext.stored > 1,
        "the fixture must store a multi-block run: stored {}",
        ext.stored
    );

    let mut fs = ARXFS::open(
        counting(
            fs.into_block().expect("the volume closes").bytes(),
            4096,
            4096,
        ),
        &TEST_KEY,
    )
    .expect("reopen");
    fs.block_mut().reads = 0;
    let plain = fs.read_data_cluster(&ext).expect("cluster reads");
    assert_eq!(plain, payload, "the cluster decompresses exactly");
    assert_eq!(
        fs.block_mut().reads,
        1,
        "one device request fetches the whole stored run ({} blocks)",
        ext.stored
    );
}

/// An ordinary commit and an explicit sync each issue one barrier. The commit
/// makes the map's invalidation durable before its pages; the sync makes those
/// pages and the publishing slot durable before restoring the clean stamp.
#[test]
fn a_commit_and_a_sync_each_barrier_once() {
    let (mut fs, dir) = counted_dir_fixture(1);
    fs.block_mut().flushes = 0;
    fs.write_at(dir, b"f0.txt", 0, b"durable payload")
        .expect("write");
    assert_eq!(
        fs.block_mut().flushes,
        1,
        "a commit issues exactly one barrier, before its superblock slot"
    );
    FilesystemWrite::flush(&mut fs).expect("flush");
    assert_eq!(
        fs.block_mut().flushes,
        2,
        "an explicit sync adds one durability barrier"
    );
}

/// A pass that publishes nothing writes nothing, so it owes the device no
/// barrier at all: the map image it rebuilt in RAM needs the invalidation
/// marker only once a page is actually going to the device.
///
/// The sync that follows one then pays the clean-to-dirty transition itself —
/// the marker's barrier, then the pages' — because no commit came between to
/// carry the marker in its own pre-slot barrier. That is two barriers once per
/// sync period, never per write, and a check never followed by a sync costs
/// none.
#[test]
fn a_clean_check_costs_no_barrier_and_its_following_sync_the_transition() {
    let (mut fs, dir) = counted_dir_fixture(1);
    assert!(fs.map_is_stamped_clean(), "the fixture starts clean");
    fs.block_mut().flushes = 0;
    let report = fs.check(&GrantAll, &NullSink).expect("check");
    assert_eq!(report.structure, StructureVerdict::Sound, "{report:?}");
    assert_eq!(
        fs.block_mut().flushes,
        0,
        "a check that published nothing forced the device cache"
    );
    FilesystemWrite::flush(&mut fs).expect("sync");
    assert_eq!(
        fs.block_mut().flushes,
        2,
        "sync used more than the clean-to-dirty transition's two barriers"
    );
    // A second sync has no transition left to pay for.
    fs.block_mut().flushes = 0;
    fs.write_at(dir, b"f0.txt", 0, b"x").expect("write");
    FilesystemWrite::flush(&mut fs).expect("sync");
    assert_eq!(
        fs.block_mut().flushes,
        2,
        "a write and its sync must each barrier exactly once"
    );
}

/// A read-only mount never wrote, so its flush is a no-op that never
/// touches the device — the damaged-device rescue path must not write.
#[test]
fn a_read_only_mount_flush_issues_no_device_flush() {
    let (fs, _dir) = counted_dir_fixture(1);
    let bytes = fs.into_block().expect("the volume closes").inner.bytes();
    let counting = CountingBlock {
        inner: MemBlock::from_bytes(bytes, 4096, 8192),
        reads: 0,
        flushes: 0,
    };
    let mut ro = ARXFS::open_read_only(counting, &TEST_KEY).expect("reopen read-only");
    FilesystemWrite::flush(&mut ro).expect("flush");
    assert_eq!(
        ro.block_mut().flushes,
        0,
        "a read-only mount forces nothing to the device"
    );
}

// ---------------------------------------------------------------------------
// The on-disk allocation map: mount cost, read-only mounts, and recovery.
// ---------------------------------------------------------------------------

/// A populated, synced volume's bytes, plus the used-block set and free count
/// the live mount held when it was synced.
fn synced_volume(files: u8) -> (alloc::vec::Vec<u8>, BTreeSet<u64>, u64) {
    let mut fs = fmt(4096, 2048, 128);
    let root = fs.root();
    for i in 0..files {
        let name = alloc::format!("f{i}").into_bytes();
        fs.create(root, &name, NodeKind::RegularFile)
            .expect("create");
        fs.write_at(root, &name, 0, &alloc::vec![i; 9000])
            .expect("write");
    }
    FilesystemWrite::flush(&mut fs).expect("sync");
    let used = fs.used_blocks();
    let free = fs.free_count;
    (
        fs.into_block().expect("the volume closes").bytes(),
        used,
        free,
    )
}

fn counting(bytes: alloc::vec::Vec<u8>, block_size: u32, blocks: u64) -> CountingBlock {
    CountingBlock {
        inner: MemBlock::from_bytes(bytes, block_size, blocks),
        reads: 0,
        flushes: 0,
    }
}

#[test]
fn a_synced_volume_adopts_its_map_instead_of_walking_the_whole_volume() {
    // The point of the on-disk map: mounting reads the superblock ring, the
    // committed root, and the map's own header — not every tree node, inode,
    // and extent on the volume. Before the map existed, mounting the 128 MiB
    // `/System` image cost over ten thousand block reads and stalled the boot
    // for seconds.
    let (bytes, used, free) = synced_volume(24);
    let mut fs = ARXFS::open(counting(bytes, 4096, 2048), &TEST_KEY).expect("reopen");
    assert!(
        fs.map_is_stamped_clean(),
        "a synced map is adopted, not rebuilt"
    );
    let mount_reads = fs.block_mut().reads;
    assert!(
        mount_reads < 32,
        "adopting the map costs a handful of reads, not a walk of the volume \
         (read {mount_reads} blocks)"
    );
    assert_eq!(
        fs.free_count, free,
        "the committed free count survives the mount"
    );
    assert_eq!(
        fs.used_blocks(),
        used,
        "the adopted map matches the live one"
    );
}

#[test]
fn a_volume_that_was_not_synced_rebuilds_its_map_at_the_next_mount() {
    // Ordinary commits leave the rebuildable map dirty on the device; only an
    // explicit sync stamps it clean. A mount that finds it dirty rebuilds from
    // the authoritative trees rather than trusting a half-written map, and
    // lands on exactly the same allocation state.
    let (bytes, _, _) = synced_volume(24);
    let mut fs = ARXFS::open(MemBlock::from_bytes(bytes, 4096, 2048), &TEST_KEY).expect("reopen");
    let root = fs.root();
    fs.create(root, b"extra", NodeKind::RegularFile)
        .expect("create");
    fs.write_at(root, b"extra", 0, &alloc::vec![7u8; 9000])
        .expect("write");
    let live = fs.used_blocks();
    let free = fs.free_count;
    let bytes = fs.into_block().expect("the volume closes").bytes();

    let mut reopened = ARXFS::open(counting(bytes, 4096, 2048), &TEST_KEY).expect("reopen");
    assert!(
        !reopened.map_is_stamped_clean(),
        "a map left dirty by an unclean shutdown is rebuilt, never adopted"
    );
    assert!(
        reopened.block_mut().reads > 32,
        "the rebuild really does walk the authoritative trees"
    );
    assert_eq!(
        reopened.used_blocks(),
        live,
        "the rebuild reproduces the live map exactly"
    );
    assert_eq!(reopened.free_count, free, "and the same free count");
}

#[test]
fn a_read_only_mount_builds_no_allocation_state_at_all() {
    // A read-only handle can never allocate, free, dedupe, or trim, so it
    // holds no allocator to do it with — that is a property of the type, not a
    // convention — and pays none of the cost of building one.
    let (bytes, _, free) = synced_volume(24);
    let mut fs =
        ARXFS::open_read_only(counting(bytes, 4096, 2048), &TEST_KEY).expect("read-only mount");
    assert!(
        fs.allocator().is_err(),
        "a read-only handle has no allocation state"
    );
    let mount_reads = fs.block_mut().reads;
    assert!(
        mount_reads < 32,
        "a read-only mount reads a handful of blocks (read {mount_reads})"
    );
    // It still reports the committed free space honestly, without the map.
    let stats = FilesystemStats::stats(&mut fs).expect("stats");
    assert_eq!(stats.free_blocks, free);
    assert_eq!(stats.total_blocks, 2048);
    // Every mutating path fails closed.
    let root = fs.root();
    assert_eq!(
        fs.create(root, b"nope", NodeKind::RegularFile),
        Err(DriverError::PermissionDenied)
    );
    assert_eq!(
        fs.write_at(root, b"f0", 0, b"nope"),
        Err(DriverError::PermissionDenied)
    );
    assert_eq!(fs.remove(root, b"f0"), Err(DriverError::PermissionDenied));
    assert!(matches!(
        fs.trim(&GrantAll, &NullSink),
        Err(DriverError::PermissionDenied)
    ));
    // And it still serves reads.
    let node = fs.lookup(root, b"f0").expect("lookup");
    let mut back = alloc::vec![0u8; 9000];
    assert_eq!(fs.read_at(node, 0, &mut back), Ok(9000));
    assert_eq!(back, alloc::vec![0u8; 9000]);
}

#[test]
fn the_allocation_map_region_stays_reserved_under_churn() {
    // The region holds the map itself, so no allocation may ever land in it —
    // otherwise a file would overwrite the map that says the file is there.
    let mut fs = fmt(512, 1024, 64);
    let start = fs.map_region_start();
    let region = fs.map_region_blocks();
    assert!(region > 0 && start >= RING_BLOCKS);
    let root = fs.root();
    for i in 0..40u8 {
        let name = alloc::format!("f{i}").into_bytes();
        fs.create(root, &name, NodeKind::RegularFile)
            .expect("create");
        let _ = fs.write_at(root, &name, 0, &alloc::vec![i; 900]);
    }
    for i in (0..40u8).step_by(2) {
        let name = alloc::format!("f{i}").into_bytes();
        fs.remove(root, &name).expect("remove");
    }
    for block in start..start + region {
        assert!(
            fs.is_used(block),
            "map region block {block} must stay reserved"
        );
    }
    // The map on the device is still readable, which it would not be had a
    // write landed on it.
    FilesystemWrite::flush(&mut fs).expect("sync");
    let bytes = fs.into_block().expect("the volume closes").bytes();
    let reopened = ARXFS::open(MemBlock::from_bytes(bytes, 512, 1024), &TEST_KEY).expect("open");
    assert!(
        reopened.map_is_stamped_clean(),
        "the map survived the churn"
    );
    assert_eq!(reopened.map_region_start(), start);
}

#[test]
fn growing_past_the_region_relays_the_map_and_keeps_the_volume_sound() {
    // 512-byte blocks hold 3072 bitmap bits per page, so a 3000-block volume
    // needs one page and a 12000-block one needs four: the region must get
    // longer, which relays it. Content and allocation must survive that.
    let mut fs = fmt(512, 3000, 64);
    let root = fs.root();
    fs.create(root, b"keep", NodeKind::RegularFile)
        .expect("create");
    let body = alloc::vec![0x5Au8; 4000];
    fs.write_at(root, b"keep", 0, &body).expect("write");
    FilesystemWrite::flush(&mut fs).expect("sync");
    let small_region = fs.map_region_blocks();

    let mut bytes = fs.into_block().expect("the volume closes").bytes();
    bytes.resize(512 * 12000, 0);
    let mut fs = ARXFS::open(MemBlock::from_bytes(bytes, 512, 12000), &TEST_KEY).expect("reopen");
    assert_eq!(fs.grow().expect("grow"), 12000 - 3000);
    assert!(
        fs.map_region_blocks() > small_region,
        "the wider volume needs a longer region"
    );
    assert!(fs.free_count > 8000, "the added tail is free");

    // The relaid map still describes a sound volume: the old file reads back
    // and new space is allocatable out of the added tail.
    let root = fs.root();
    let node = fs.lookup(root, b"keep").expect("lookup");
    let mut back = alloc::vec![0u8; 4000];
    assert_eq!(fs.read_at(node, 0, &mut back), Ok(4000));
    assert_eq!(back, body);
    fs.create(root, b"after", NodeKind::RegularFile)
        .expect("create after grow");
    assert_eq!(fs.write_at(root, b"after", 0, &body), Ok(4000));
    let live = fs.used_blocks();

    FilesystemWrite::flush(&mut fs).expect("sync");
    let bytes = fs.into_block().expect("the volume closes").bytes();
    let mut reopened =
        ARXFS::open(MemBlock::from_bytes(bytes, 512, 12000), &TEST_KEY).expect("reopen grown");
    assert!(reopened.map_is_stamped_clean(), "the relaid map is adopted");
    assert_eq!(reopened.used_blocks(), live);
}

// ---------------------------------------------------------------------------
// Symbolic links: the on-disk kind, the target stored as node data, and the
// incompatible-feature declaration (`docs/src/filesystem/arxfs-spec.md` §20).
// ---------------------------------------------------------------------------

#[test]
fn a_created_link_reports_its_kind_and_target_length() {
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    let target = b"/System/Commands/ls.app";
    let node = fs
        .create_link(root, b"alias", target)
        .expect("create the link");

    let info = fs.node_info(node).expect("stat the link");
    assert_eq!(info.kind, NodeKind::Symlink);
    assert_eq!(info.size, target.len() as u64);

    let mut out = [0u8; 64];
    assert_eq!(fs.read_link(node, &mut out), Ok(target.len()));
    assert_eq!(&out[..target.len()], target);
}

#[test]
fn a_links_target_survives_a_remount() {
    // The target is ordinary node data, so it goes through the whole
    // checksum / authenticate / encrypt pipeline and must read back
    // byte-identical from the committed volume.
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    let target = b"../relative/with/../dots";
    fs.create_link(root, b"alias", target)
        .expect("create the link");
    let bytes = fs.into_block().expect("the volume closes").bytes();

    let mut re = ARXFS::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY).expect("reopen");
    let node = re.lookup(re.root(), b"alias").expect("lookup the link");
    assert_eq!(re.node_info(node).expect("stat").kind, NodeKind::Symlink);
    let mut out = [0u8; 64];
    assert_eq!(re.read_link(node, &mut out), Ok(target.len()));
    assert_eq!(&out[..target.len()], target);
}

#[test]
fn a_maximum_length_target_round_trips_across_several_blocks() {
    // `FS_SYMLINK_MAX` bytes exceed one 512-byte block's content capacity, so
    // this exercises the multi-block extent path a target shares with file
    // data — and proves the format's own limit is the ABI's, not smaller.
    let mut fs = fmt(512, 1024, 32);
    let root = fs.root();
    let target = alloc::vec![b'x'; FS_SYMLINK_MAX];
    let node = fs
        .create_link(root, b"long", &target)
        .expect("create a maximum-length link");
    let mut out = alloc::vec![0u8; FS_SYMLINK_MAX];
    assert_eq!(fs.read_link(node, &mut out), Ok(FS_SYMLINK_MAX));
    assert_eq!(out, target);

    // One byte more is refused, and nothing is left behind.
    let over = alloc::vec![b'x'; FS_SYMLINK_MAX + 1];
    assert_eq!(
        fs.create_link(root, b"over", &over),
        Err(DriverError::LengthOutOfRange)
    );
    assert_eq!(fs.lookup(root, b"over"), Err(DriverError::NotFound));
    assert_eq!(
        fs.create_link(root, b"empty", b""),
        Err(DriverError::LengthOutOfRange)
    );
}

#[test]
fn a_link_is_never_byte_readable_and_never_byte_writable() {
    // Its content is a path, reached only with `read_link`; the driver fails
    // closed on its own rather than relying on the VFS to resolve first.
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    let node = fs
        .create_link(root, b"alias", b"/target")
        .expect("create the link");

    let mut buf = [0u8; 8];
    assert_eq!(fs.read_at(node, 0, &mut buf), Err(DriverError::Unsupported));
    assert_eq!(
        fs.write_at(root, b"alias", 0, b"clobber"),
        Err(DriverError::Unsupported)
    );
    assert_eq!(
        fs.truncate(root, b"alias", 0),
        Err(DriverError::Unsupported)
    );
    // A reflink clones data blocks into a fresh regular file, which would
    // silently turn the link into a file holding its target's text.
    assert_eq!(
        fs.reflink(root, b"alias", b"clone"),
        Err(DriverError::Unsupported)
    );
    // The refusals changed nothing.
    let mut out = [0u8; 16];
    assert_eq!(fs.read_link(node, &mut out), Ok(7));
    assert_eq!(&out[..7], b"/target");
}

#[test]
fn create_refuses_a_link_kind_and_read_link_refuses_a_non_link() {
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    // A link carries a target `create` has nowhere to put.
    assert_eq!(
        fs.create(root, b"alias", NodeKind::Symlink),
        Err(DriverError::Unsupported)
    );
    assert_eq!(fs.lookup(root, b"alias"), Err(DriverError::NotFound));

    fs.create(root, b"file", NodeKind::RegularFile)
        .expect("create a file");
    let file = fs.lookup(root, b"file").expect("lookup");
    let mut out = [0u8; 16];
    assert_eq!(fs.read_link(file, &mut out), Err(DriverError::Unsupported));
    assert_eq!(fs.read_link(root, &mut out), Err(DriverError::Unsupported));
}

#[test]
fn read_link_refuses_an_undersized_buffer_rather_than_truncating() {
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    let node = fs
        .create_link(root, b"alias", b"/a/long/enough/target")
        .expect("create the link");
    let mut small = [0u8; 4];
    assert_eq!(
        fs.read_link(node, &mut small),
        Err(DriverError::BufferTooSmall)
    );
    assert_eq!(small, [0u8; 4]);
}

#[test]
fn a_link_is_listed_as_a_link_and_its_blocks_are_accounted_as_data() {
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create_link(root, b"alias", &alloc::vec![b'y'; 1200])
        .expect("create a multi-block link");

    let mut name = [0u8; 255];
    let entry = fs
        .read_dir(root, 0, &mut name)
        .expect("read_dir")
        .expect("one entry");
    assert_eq!(entry.info.kind, NodeKind::Symlink);
    assert_eq!(entry.info.size, 1200);
    assert!(
        entry.info.allocated > 0,
        "a link's target occupies real data blocks"
    );

    // A remount rebuilds the allocation map by walking the trees, so the
    // rebuilt free count agreeing with the live one is what proves the walk
    // accounts a link's target blocks as the single-copy data records they
    // are, rather than as a directory's mirrored metadata pairs.
    let live = fs.stats().expect("stats").free_blocks;
    let bytes = fs.into_block().expect("the volume closes").bytes();
    let mut re = ARXFS::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY).expect("reopen");
    assert_eq!(re.stats().expect("stats").free_blocks, live);

    let root = re.root();
    re.remove(root, b"alias").expect("remove the link");
    assert_eq!(re.lookup(root, b"alias"), Err(DriverError::NotFound));
    assert!(
        re.stats().expect("stats").free_blocks > live,
        "removing a link returns its target blocks"
    );
}

#[test]
fn a_link_renames_and_replaces_like_any_other_name() {
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create_link(root, b"alias", b"/target")
        .expect("create the link");
    fs.rename(root, b"alias", root, b"moved")
        .expect("rename the link");
    let node = fs.lookup(root, b"moved").expect("the moved link");
    let mut out = [0u8; 16];
    assert_eq!(fs.read_link(node, &mut out), Ok(7));

    // A rename may replace a link with a file and a file with a link: both
    // are non-directories, so the kind-compatibility rule permits it.
    fs.create(root, b"plain", NodeKind::RegularFile)
        .expect("create a file");
    fs.rename(root, b"plain", root, b"moved")
        .expect("a file replaces a link");
    let replaced = fs.lookup(root, b"moved").expect("the replacement");
    assert_eq!(
        fs.node_info(replaced).expect("stat").kind,
        NodeKind::RegularFile
    );
}

#[test]
fn a_volume_declares_the_symlink_feature_only_once_it_holds_a_link() {
    // The bit is set by the first link, not at format time, so a volume that
    // never holds one stays readable by a build that does not know the kind.
    let mut fs = fmt(512, 256, 32);
    assert_eq!(fs.incompat, 0);
    let root = fs.root();
    fs.create(root, b"file", NodeKind::RegularFile)
        .expect("create a file");
    assert_eq!(fs.incompat, 0, "an ordinary file declares nothing");

    fs.create_link(root, b"alias", b"/target")
        .expect("create the link");
    assert_eq!(fs.incompat, superblock::INCOMPAT_SYMLINKS);

    // And the declaration is committed, not merely in memory.
    let bytes = fs.into_block().expect("the volume closes").bytes();
    let re = ARXFS::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY).expect("reopen");
    assert_eq!(re.incompat, superblock::INCOMPAT_SYMLINKS);
}

#[test]
fn a_refused_link_creation_leaves_the_feature_undeclared() {
    // The bit is set before the link is minted, so a rolled-back transaction
    // must take it back with the rest of the state.
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"file", NodeKind::RegularFile)
        .expect("create a file");
    assert_eq!(
        fs.create_link(root, b"file", b"/target"),
        Err(DriverError::AlreadyExists)
    );
    assert_eq!(fs.incompat, 0);
}

#[test]
fn an_unknown_declared_feature_refuses_the_mount_with_its_reason() {
    // The word is covered by the keyed authenticator, so a bit that survives
    // the check is one the volume's writer really set: refuse rather than
    // mount and misread structure this build does not implement.
    let fs = fmt(512, 256, 32);
    let key = fs.mac_key;
    let uuid = fs.fs_uuid;
    let generation = fs.generation;
    let root_phys = fs.root_phys;
    let crypto_header = fs.crypto_header;
    let mut bytes = fs.into_block().expect("the volume closes").bytes();

    // Re-seal every ring slot claiming a feature beyond this build's set.
    let sb = Superblock {
        block_size: 512,
        total_blocks: 256,
        inode_count: 32,
        generation,
        root_phys,
        incompat: superblock::INCOMPAT_SUPPORTED | (1 << 63),
    };
    for slot in 0..RING_SLOTS {
        for phys in [slot_block(slot), slot_block(slot) + 1] {
            let mut block = [0u8; 512];
            sb.seal(&mut block, uuid, slot_block(slot), &key, &crypto_header)
                .expect("seal the slot");
            let start = as_usize(phys) * 512;
            bytes[start..start + 512].copy_from_slice(&block);
        }
    }

    assert_eq!(
        ARXFS::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY).err(),
        Some(DriverError::Unsupported)
    );
}

#[test]
fn check_widens_a_declared_feature_set_that_understates_the_volume() {
    // A volume holding a link but not declaring it could be mounted and
    // misread by a link-unaware reader, so the offline check declares it.
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create_link(root, b"alias", b"/target")
        .expect("create the link");
    // Understate the volume, exactly as a driver defect would.
    fs.incompat = 0;
    fs.commit().expect("commit the understated word");

    let report = fs.check(&GrantAll, &NullSink).expect("check");
    assert_eq!(report.features_declared, 1);
    assert!(report.made_repairs());
    assert_eq!(fs.incompat, superblock::INCOMPAT_SYMLINKS);
    // And it is idempotent: a correctly-declared volume needs no widening.
    let again = fs.check(&GrantAll, &NullSink).expect("check again");
    assert_eq!(again.features_declared, 0);
}

#[test]
fn scrub_verifies_a_links_target_blocks_as_data() {
    // A link's target is node data, so scrub runs it through the data
    // integrity pipeline rather than treating it as mirrored metadata.
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create_link(root, b"alias", &alloc::vec![b'z'; 1200])
        .expect("create a multi-block link");

    let report = fs
        .scrub(&GrantAll, &NullSink, ScrubBudget::Unlimited)
        .expect("scrub");
    assert!(report.data_blocks_checked > 0);
    assert_eq!(report.metadata_unrepairable, 0);
    assert_eq!(report.data_physical_faults, 0);
    assert_eq!(report.data_aead_faults, 0);
    assert_eq!(report.data_logical_faults, 0);
    // The offline check agrees, and finds no orphan or dangling entry.
    let check = fs.check(&GrantAll, &NullSink).expect("check");
    assert_eq!(check.structure, StructureVerdict::Sound);
    assert_eq!(check.orphaned_inodes, 0);
    assert_eq!(check.dangling_entries, 0);
}

#[test]
fn rescue_counts_a_link_rather_than_extracting_its_target_as_file_bytes() {
    // The sink carries file bytes, so emitting a link's target through it
    // would recreate the link as a regular file holding the target's text.
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"file", NodeKind::RegularFile)
        .expect("create a file");
    assert_eq!(fs.write_at(root, b"file", 0, b"payload"), Ok(7));
    fs.create_link(root, b"alias", b"/target")
        .expect("create the link");
    let bytes = fs.into_block().expect("the volume closes").bytes();

    let mut sink = CollectSink::new();
    let report = ARXFS::rescue(
        MemBlock::from_bytes(bytes, 512, 256),
        &TEST_KEY,
        &GrantAll,
        &NullSink,
        &mut sink,
    )
    .expect("rescue");
    assert_eq!(report.links_skipped, 1);
    assert_eq!(report.files_mapped, 1);
    assert!(report.blocks_extracted > 0);
    // Nothing the sink received carries the target's bytes.
    assert!(
        !sink
            .blocks
            .values()
            .any(|data| data.starts_with(b"/target")),
        "a link's target must never be emitted as file content"
    );
}

// ---------------------------------------------------------------------------
// Companion-mirror recovery: an unreadable copy is as absent as an
// unauthenticated one (`docs/src/filesystem/arxfs-spec.md` §8).
// ---------------------------------------------------------------------------

#[test]
fn an_unreadable_primary_superblock_slot_mounts_from_its_companion() {
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"witness", NodeKind::RegularFile)
        .expect("create");
    let bytes = fs.into_block().expect("the volume closes").bytes();
    // The committed slot's primary block cannot be read at all.
    let dev = MemBlock::from_bytes(bytes, 512, 256).fail_reads_of(slot_block(1));

    let mut re = ARXFS::open(dev, &TEST_KEY).expect("mount from the companion");
    re.lookup(re.root(), b"witness")
        .expect("the committed content is intact");
}

#[test]
fn an_unreadable_primary_transaction_root_mounts_from_its_companion() {
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"witness", NodeKind::RegularFile)
        .expect("create");
    let root_phys = fs.root_phys;
    let bytes = fs.into_block().expect("the volume closes").bytes();
    let dev = MemBlock::from_bytes(bytes, 512, 256).fail_reads_of(root_phys);

    let mut re = ARXFS::open(dev, &TEST_KEY).expect("mount from the companion");
    re.lookup(re.root(), b"witness")
        .expect("the committed content is intact");
}

#[test]
fn an_unreadable_primary_metadata_block_reads_from_its_companion() {
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"witness", NodeKind::RegularFile)
        .expect("create");
    let inode_tree_root = fs.inode_tree_root;
    let bytes = fs.into_block().expect("the volume closes").bytes();
    // A tree node's primary copy is unreadable; the lookup must still work.
    let dev = MemBlock::from_bytes(bytes, 512, 256).fail_reads_of(inode_tree_root);

    let mut re = ARXFS::open(dev, &TEST_KEY).expect("mount");
    re.lookup(re.root(), b"witness")
        .expect("the inode tree reads from its mirror");
}

#[test]
fn a_hard_link_is_a_second_name_for_one_inode() {
    // Two names, one node id: the whole point of the operation. A write
    // through either name is visible through the other because there is only
    // one set of blocks behind them.
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    let node = fs
        .create(root, b"first", NodeKind::RegularFile)
        .expect("create the file");
    fs.write_at(root, b"first", 0, b"shared bytes")
        .expect("write through the first name");

    fs.link(root, b"second", node).expect("add a second name");
    assert_eq!(fs.lookup(root, b"second"), Ok(node));
    assert_eq!(fs.lookup(root, b"first"), Ok(node));
    assert_eq!(fs.node_info(node).expect("stat").nlink, 2);

    let mut out = [0u8; 32];
    let read = fs
        .read_at(node, 0, &mut out[..12])
        .expect("read through the node");
    assert_eq!(&out[..read], b"shared bytes");

    // A write through the *second* name reaches the same bytes.
    fs.write_at(root, b"second", 0, b"rewritten   ")
        .expect("write through the second name");
    let read = fs.read_at(node, 0, &mut out[..12]).expect("read back");
    assert_eq!(&out[..read], b"rewritten   ");
}

#[test]
fn unlinking_one_name_keeps_the_other_readable_and_its_blocks_allocated() {
    // The lifecycle this stage exists for: an unlink that is not the last
    // must free nothing, or it destroys data the remaining name reaches.
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    let node = fs
        .create(root, b"first", NodeKind::RegularFile)
        .expect("create the file");
    fs.write_at(root, b"first", 0, b"payload").expect("write");
    fs.link(root, b"second", node).expect("add a second name");
    let with_both = fs.stats().expect("stats").free_blocks;

    fs.remove(root, b"first").expect("drop the first name");
    assert_eq!(fs.lookup(root, b"first"), Err(DriverError::NotFound));
    assert_eq!(fs.lookup(root, b"second"), Ok(node));
    assert_eq!(fs.node_info(node).expect("stat").nlink, 1);
    assert_eq!(
        fs.stats().expect("stats").free_blocks,
        with_both,
        "a name went, no storage did"
    );
    let mut out = [0u8; 16];
    let read = fs.read_at(node, 0, &mut out[..7]).expect("still readable");
    assert_eq!(&out[..read], b"payload");

    // Only the last name frees the inode and its blocks.
    fs.remove(root, b"second").expect("drop the last name");
    assert_eq!(fs.lookup(root, b"second"), Err(DriverError::NotFound));
    assert_eq!(fs.node_info(node), Err(DriverError::NotFound));
    assert!(
        fs.stats().expect("stats").free_blocks > with_both,
        "the last name returns the blocks"
    );
}

#[test]
fn the_link_count_survives_a_remount_and_the_rebuilt_free_map_agrees() {
    // The count is on-disk state, and the allocation-map rebuild walks the
    // inode tree, so a twice-named inode's blocks are accounted exactly once.
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    let node = fs
        .create(root, b"first", NodeKind::RegularFile)
        .expect("create the file");
    fs.write_at(root, b"first", 0, b"payload").expect("write");
    fs.link(root, b"second", node).expect("add a second name");

    let live = fs.stats().expect("stats").free_blocks;
    let bytes = fs.into_block().expect("the volume closes").bytes();
    let mut re = ARXFS::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY).expect("reopen");
    assert_eq!(
        re.stats().expect("stats").free_blocks,
        live,
        "the rebuilt map counts a twice-named inode's blocks once"
    );
    let root = re.root();
    let node = re.lookup(root, b"second").expect("the second name");
    assert_eq!(re.node_info(node).expect("stat").nlink, 2);
}

#[test]
fn a_hard_link_to_a_directory_is_refused_and_creates_nothing() {
    // A second name for a directory would let the tree hold a cycle, which
    // the VFS's physical `..` walk depends on being impossible.
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    let dir = fs
        .create(root, b"dir", NodeKind::Directory)
        .expect("create a directory");
    assert_eq!(fs.link(root, b"alias", dir), Err(DriverError::Unsupported));
    assert_eq!(fs.lookup(root, b"alias"), Err(DriverError::NotFound));
    assert_eq!(fs.incompat, 0, "a refused link declares nothing");
}

#[test]
fn a_hard_link_over_a_taken_name_is_refused_and_changes_nothing() {
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    let node = fs
        .create(root, b"first", NodeKind::RegularFile)
        .expect("create the file");
    fs.create(root, b"taken", NodeKind::RegularFile)
        .expect("create the occupant");
    assert_eq!(
        fs.link(root, b"taken", node),
        Err(DriverError::AlreadyExists)
    );
    assert_eq!(fs.node_info(node).expect("stat").nlink, 1);
    assert_eq!(fs.incompat, 0, "a refused link declares nothing");
}

#[test]
fn a_hard_link_to_a_symbolic_link_names_the_link_itself() {
    // The driver is handed a node, not a path, so what gains a name is
    // exactly what the caller resolved — the link, not its target.
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    let link = fs
        .create_link(root, b"alias", b"/target")
        .expect("create the symbolic link");
    fs.link(root, b"alias2", link).expect("a second name");
    let second = fs.lookup(root, b"alias2").expect("the second name");
    assert_eq!(second, link);
    let info = fs.node_info(second).expect("stat");
    assert_eq!(info.kind, NodeKind::Symlink);
    assert_eq!(info.nlink, 2);
    let mut out = [0u8; 16];
    assert_eq!(fs.read_link(second, &mut out), Ok(7));
}

#[test]
fn a_volume_declares_the_hardlink_feature_only_once_it_holds_one() {
    // Stronger than the symbolic-link case: an unaware reader would not
    // misread a second name, it would free an inode the other name reaches.
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    let node = fs
        .create(root, b"first", NodeKind::RegularFile)
        .expect("create the file");
    assert_eq!(fs.incompat, 0, "an ordinary file declares nothing");

    fs.link(root, b"second", node).expect("add a second name");
    assert_eq!(fs.incompat, superblock::INCOMPAT_HARDLINKS);

    let bytes = fs.into_block().expect("the volume closes").bytes();
    let re = ARXFS::open(MemBlock::from_bytes(bytes, 512, 256), &TEST_KEY).expect("reopen");
    assert_eq!(re.incompat, superblock::INCOMPAT_HARDLINKS);
}

#[test]
fn a_volume_holding_both_link_kinds_declares_both() {
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    let node = fs
        .create(root, b"first", NodeKind::RegularFile)
        .expect("create the file");
    fs.create_link(root, b"sym", b"/target")
        .expect("create the symbolic link");
    fs.link(root, b"second", node).expect("add a second name");
    assert_eq!(
        fs.incompat,
        superblock::INCOMPAT_SYMLINKS | superblock::INCOMPAT_HARDLINKS
    );
}

#[test]
fn a_refused_hard_link_leaves_the_feature_undeclared() {
    // The bit is set before the entry is written, so a rolled-back
    // transaction must take it back with the rest of the state.
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    let node = fs
        .create(root, b"first", NodeKind::RegularFile)
        .expect("create the file");
    assert_eq!(
        fs.link(root, b"first", node),
        Err(DriverError::AlreadyExists),
        "the name is taken"
    );
    assert_eq!(fs.incompat, 0);
    // And the count did not move either.
    assert_eq!(fs.node_info(node).expect("stat").nlink, 1);
}

#[test]
fn check_corrects_a_link_count_that_disagrees_with_the_names_on_disk() {
    // The count decides when an unlink frees blocks, so a drifted value is
    // either a storage leak or a live-data free. `check` counts the truth.
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    let node = fs
        .create(root, b"first", NodeKind::RegularFile)
        .expect("create the file");
    fs.link(root, b"second", node).expect("add a second name");

    // Understate the node, exactly as a driver defect would: one more unlink
    // than names would then free blocks the surviving name still reaches.
    let ino = u32::try_from(node.raw()).expect("inode number");
    let mut inode = fs.read_inode(ino).expect("read the inode");
    inode.nlink = 1;
    fs.write_inode(ino, &inode).expect("write the inode");
    fs.commit().expect("commit the understated count");

    let report = fs.check(&GrantAll, &NullSink).expect("check");
    assert_eq!(report.link_counts_corrected, 1);
    assert!(report.made_repairs());
    assert_eq!(fs.node_info(node).expect("stat").nlink, 2);

    // Idempotent: a volume whose counts already match needs no correction.
    let again = fs.check(&GrantAll, &NullSink).expect("check again");
    assert_eq!(again.link_counts_corrected, 0);
}

#[test]
fn check_leaves_correct_link_counts_alone_for_every_kind() {
    // Directories are counted by the same rule — `.` and `..` are real
    // entries — so a sound volume must need no correction at all.
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"dir", NodeKind::Directory)
        .expect("create a directory");
    let nested = fs.lookup(root, b"dir").expect("the directory");
    fs.create(nested, b"inner", NodeKind::Directory)
        .expect("create a nested directory");
    let file = fs
        .create(root, b"file", NodeKind::RegularFile)
        .expect("create a file");
    fs.link(root, b"file2", file).expect("a second name");
    fs.create_link(root, b"sym", b"/target")
        .expect("create a symbolic link");

    let report = fs.check(&GrantAll, &NullSink).expect("check");
    assert_eq!(report.link_counts_corrected, 0);
    assert_eq!(report.structure, StructureVerdict::Sound);
}

#[test]
fn scrub_verifies_a_twice_named_inode_once() {
    // Scrub walks the inode tree, not the name space, so a second name adds
    // no second verification of the same blocks.
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    let node = fs
        .create(root, b"first", NodeKind::RegularFile)
        .expect("create the file");
    fs.write_at(root, b"first", 0, b"payload").expect("write");
    let before = fs
        .scrub(&GrantAll, &NullSink, ScrubBudget::Unlimited)
        .expect("scrub");

    fs.link(root, b"second", node).expect("add a second name");
    let after = fs
        .scrub(&GrantAll, &NullSink, ScrubBudget::Unlimited)
        .expect("scrub again");
    assert_eq!(
        after.data_blocks_checked, before.data_blocks_checked,
        "a second name is not a second copy of the data"
    );
}

#[test]
fn rescue_extracts_a_twice_named_inode_once() {
    // The sink is keyed by inode, so a multiply-named file is recovered once
    // rather than emitted (and counted) again under its other name.
    let mut fs = fmt(512, 256, 32);
    let root = fs.root();
    let node = fs
        .create(root, b"first", NodeKind::RegularFile)
        .expect("create the file");
    fs.write_at(root, b"first", 0, b"payload").expect("write");
    fs.link(root, b"second", node).expect("add a second name");
    let ino = u32::try_from(node.raw()).expect("inode number");

    let bytes = fs.into_block().expect("the volume closes").bytes();
    let mut sink = CollectSink::new();
    let report = ARXFS::rescue(
        MemBlock::from_bytes(bytes, 512, 256),
        &TEST_KEY,
        &GrantAll,
        &NullSink,
        &mut sink,
    )
    .expect("rescue");
    assert_eq!(report.files_mapped, 1);
    assert_eq!(
        sink.blocks.keys().filter(|(i, _)| *i == ino).count(),
        1,
        "one inode, one emitted block, however many names reach it"
    );
}

// ---------------------------------------------------------------------------
// Bounded, resumable tree iteration: the walk is the driver's only way to
// read more than one record, so its order, its resumability, its node
// reporting, and its refusal of an impossible tree are all load-bearing.
// ---------------------------------------------------------------------------

/// A three-level inode tree on a 512-byte volume, whose one-entry leaves make
/// the tree deep without needing thousands of inodes: `count` leaves under
/// 23-entry internal nodes is a root, a middle level, and the leaves.
fn deep_inode_tree(count: u32) -> (ARXFS<MemBlock>, Vec<u64>) {
    let mut fs = fmt(512, 4096, 64);
    let root = fs.root();
    let mut keys = Vec::new();
    for i in 0..count {
        let name = alloc::format!("f{i}");
        fs.create(root, name.as_bytes(), NodeKind::RegularFile)
            .expect("create");
        keys.push(fs.lookup(root, name.as_bytes()).expect("lookup").raw());
    }
    keys.push(u64::from(ROOT_INO));
    keys.sort_unstable();
    (fs, keys)
}

/// The level of the tree at `root` (0 for a single leaf), read from its root
/// node — the depth the walk must descend on every step.
fn tree_level(fs: &mut ARXFS<MemBlock>, root: u64) -> u32 {
    let mut buf = [0u8; MAX_BLOCK_SIZE];
    fs.read_meta(root, BlockType::Btree, &mut buf)
        .expect("read root node");
    rd_u32(&buf, btree::N_LEVEL)
}

#[test]
fn a_walk_yields_every_entry_in_key_order_one_leaf_at_a_time() {
    let (mut fs, keys) = deep_inode_tree(300);
    let spec = inode_spec();
    let inode_root = fs.inode_tree_root;
    assert!(
        tree_level(&mut fs, inode_root) >= 2,
        "the fixture must build a tree deeper than one internal level"
    );

    let mut walk = TreeWalk::new(fs.block_size).expect("walk buffer");
    let mut seen = Vec::new();
    let mut steps = 0u32;
    // A 512-byte leaf holds exactly one inode record, so no step may yield
    // more: the walk's yield is bounded by the node, never by the tree.
    while fs
        .btree_next_leaf(inode_root, spec, &mut walk)
        .expect("walk the inode tree")
    {
        let batch: Vec<u64> = walk.entries().map(|(key, _)| key).collect();
        assert!(!batch.is_empty(), "a step that yields must yield an entry");
        assert!(batch.len() <= 1, "one 512-byte leaf holds one inode record");
        seen.extend(batch);
        steps += 1;
    }
    assert_eq!(seen, keys, "every entry, once, in key order");
    assert_eq!(steps as usize, keys.len(), "one leaf per step");
}

#[test]
fn a_walk_interrupted_at_every_entry_yields_the_uninterrupted_sequence() {
    let (mut fs, keys) = deep_inode_tree(120);
    let spec = inode_spec();
    let inode_root = fs.inode_tree_root;

    // Stop after every entry, persist that entry's key as a scrub would, and
    // resume a fresh walk there: the concatenation must be the whole tree.
    for stride in [1usize, 3, 7] {
        let mut resumed = Vec::new();
        let mut from = 0u64;
        loop {
            let mut walk = TreeWalk::new(fs.block_size).expect("walk buffer");
            walk.seek(from);
            let mut taken = 0usize;
            let mut stopped = None;
            'steps: while fs
                .btree_next_leaf(inode_root, spec, &mut walk)
                .expect("walk")
            {
                for (key, _) in walk.entries() {
                    if taken == stride {
                        stopped = Some(key);
                        break 'steps;
                    }
                    resumed.push(key);
                    taken += 1;
                }
            }
            match stopped {
                Some(key) => from = key,
                None => break,
            }
        }
        assert_eq!(resumed, keys, "resuming at stride {stride} loses nothing");
    }
}

#[test]
fn a_walk_seeks_to_a_key_a_gap_and_past_the_end() {
    let (mut fs, keys) = deep_inode_tree(80);
    let spec = inode_spec();
    let inode_root = fs.inode_tree_root;
    let last = *keys.last().expect("keys");

    let from = |fs: &mut ARXFS<MemBlock>, key: u64| -> Vec<u64> {
        let mut walk = TreeWalk::new(fs.block_size).expect("walk buffer");
        walk.seek(key);
        let mut out = Vec::new();
        while fs
            .btree_next_leaf(inode_root, spec, &mut walk)
            .expect("walk")
        {
            out.extend(walk.entries().map(|(k, _)| k));
        }
        out
    };

    // A key that exists starts there; the tail is exact either side of it.
    let middle = keys[keys.len() / 2];
    assert_eq!(from(&mut fs, middle), keys[keys.len() / 2..]);
    // Before every key the walk yields the whole tree; past the last key it
    // yields nothing, and neither does an absent tree.
    assert_eq!(from(&mut fs, 0), keys);
    assert!(from(&mut fs, last + 1).is_empty());
    let mut walk = TreeWalk::new(fs.block_size).expect("walk buffer");
    assert!(!fs
        .btree_next_leaf(0, spec, &mut walk)
        .expect("an absent tree walks to nothing"));

    // A sparse file's extent tree has real gaps between its keys, which is
    // where seeking matters: a truncate seeks straight to the run covering
    // the cut. A seek into a gap resumes at the next run that exists.
    let root = fs.root();
    let cap = fs.data_capacity();
    for run in 0..60u64 {
        assert_eq!(fs.write_at(root, b"f0", run * 4 * cap, &[0x11]), Ok(1));
    }
    let ino = file_ino(&mut fs, b"f0");
    let inode = fs.read_inode(ino).expect("inode");
    let extent_spec = extent_spec(ino);
    let starts: Vec<u64> = tree_entries(&mut fs, inode.extent_root, extent_spec)
        .into_iter()
        .map(|(start, _)| start)
        .collect();
    assert_eq!(starts.len(), 60, "each written run is its own extent");
    for (i, start) in starts.iter().enumerate() {
        let mut walk = TreeWalk::new(fs.block_size).expect("walk buffer");
        // One past a run's start is inside the hole that follows it.
        walk.seek(start + 1);
        let mut seen = Vec::new();
        while fs
            .btree_next_leaf(inode.extent_root, extent_spec, &mut walk)
            .expect("walk")
        {
            seen.extend(walk.entries().map(|(k, _)| k));
        }
        assert_eq!(seen, starts[i + 1..], "a seek into a hole skips no run");
    }
}

#[test]
fn a_walk_reports_every_node_of_the_tree_exactly_once() {
    let (mut fs, _) = deep_inode_tree(200);
    let spec = inode_spec();
    let inode_root = fs.inode_tree_root;
    let nodes = tree_nodes(&mut fs, inode_root, spec);

    assert_eq!(
        nodes.first(),
        Some(&inode_root),
        "the root is reported first"
    );
    let unique: BTreeSet<u64> = nodes.iter().copied().collect();
    assert_eq!(unique.len(), nodes.len(), "no node is reported twice");

    // Cross-check against scrub's own enumeration, which descends the tree by
    // its child pointers rather than by key order: two independent walks must
    // find the same number of nodes.
    let mut report = ScrubReport::default();
    assert!(fs.scrub_btree(inode_root, &mut report).expect("scrub tree"));
    assert_eq!(report.metadata_blocks_checked, nodes.len() as u64);
}

#[test]
fn a_tree_whose_shape_is_impossible_is_refused_rather_than_walked_forever() {
    let spec = inode_spec();

    // A child pointer that leads back to its own parent: levels no longer
    // decrease, which is the shape that would otherwise descend forever.
    let (mut fs, _) = deep_inode_tree(60);
    let root = fs.inode_tree_root;
    let mut buf = [0u8; MAX_BLOCK_SIZE];
    fs.read_meta(root, BlockType::Btree, &mut buf)
        .expect("root");
    wr_u64(&mut buf, btree::N_ENTRIES + 8, root);
    fs.begin().expect("begin");
    let cycled = fs
        .cow_meta(0, &mut buf, BlockType::Btree, spec.owner, 0)
        .expect("seal the cyclic node");
    let mut walk = TreeWalk::new(fs.block_size).expect("walk buffer");
    assert_eq!(
        fs.btree_next_leaf(cycled, spec, &mut walk),
        Err(DriverError::DeviceFault),
        "a child that does not sit one level down is refused"
    );
    assert_eq!(
        fs.btree_get(cycled, 2, spec),
        Err(DriverError::DeviceFault),
        "a point lookup refuses the same shape"
    );

    // A node claiming a level no device could hold a tree that deep for.
    let mut deep = [0u8; MAX_BLOCK_SIZE];
    fs.read_meta(root, BlockType::Btree, &mut deep)
        .expect("root");
    wr_u32(&mut deep, btree::N_LEVEL, 200);
    let too_deep = fs
        .cow_meta(0, &mut deep, BlockType::Btree, spec.owner, 0)
        .expect("seal the over-deep node");
    let mut walk = TreeWalk::new(fs.block_size).expect("walk buffer");
    assert_eq!(
        fs.btree_next_leaf(too_deep, spec, &mut walk),
        Err(DriverError::DeviceFault)
    );

    // A leaf claiming more entries than its block can hold: reading them
    // would index past the buffer.
    let mut wide = [0u8; MAX_BLOCK_SIZE];
    let leaf = tree_nodes(&mut fs, root, spec)
        .into_iter()
        .find(|node| {
            let mut probe = [0u8; MAX_BLOCK_SIZE];
            fs.read_meta(*node, BlockType::Btree, &mut probe).is_ok()
                && tree_level(&mut fs, *node) == 0
        })
        .expect("a leaf");
    fs.read_meta(leaf, BlockType::Btree, &mut wide)
        .expect("leaf");
    wr_u32(&mut wide, btree::N_COUNT, 4096);
    let overfull = fs
        .cow_meta(0, &mut wide, BlockType::Btree, spec.owner, 0)
        .expect("seal the overfull node");
    let mut walk = TreeWalk::new(fs.block_size).expect("walk buffer");
    assert_eq!(
        fs.btree_next_leaf(overfull, spec, &mut walk),
        Err(DriverError::DeviceFault)
    );
    fs.rollback();

    // A leaf whose keys descend cannot be walked in key order: stepping past
    // its last key would skip the entries above it, so the walk refuses it
    // rather than quietly handing back part of the tree.
    let mut fs = fmt(4096, 512, 64);
    let root = fs.root();
    for i in 0..4u32 {
        let name = alloc::format!("g{i}");
        fs.create(root, name.as_bytes(), NodeKind::RegularFile)
            .expect("create");
    }
    let mut leaf = [0u8; MAX_BLOCK_SIZE];
    let inode_root = fs.inode_tree_root;
    fs.read_meta(inode_root, BlockType::Btree, &mut leaf)
        .expect("the small tree is one leaf");
    assert_eq!(tree_level(&mut fs, inode_root), 0);
    let stride = 8 + INODE_SIZE;
    let first = btree::N_ENTRIES;
    let last = first + 4 * stride;
    let high = rd_u64(&leaf, last);
    wr_u64(&mut leaf, first, high + 1);
    wr_u64(&mut leaf, last, 1);
    fs.begin().expect("begin");
    let descending = fs
        .cow_meta(0, &mut leaf, BlockType::Btree, spec.owner, 0)
        .expect("seal the descending leaf");
    let mut walk = TreeWalk::new(fs.block_size).expect("walk buffer");
    walk.seek(high);
    assert_eq!(
        fs.btree_next_leaf(descending, spec, &mut walk),
        Err(DriverError::DeviceFault)
    );
    fs.rollback();
}

#[test]
fn the_write_path_refuses_an_impossible_tree_as_the_read_path_does() {
    // A mutation descends the same shapes a walk does, and validates each
    // level on the way back up as well, so a child pointer that leads to an
    // ancestor or a node claiming an impossible level is refused rather than
    // descended until the guard page stops it.
    let spec = inode_spec();
    let (mut fs, _) = deep_inode_tree(60);
    let root = fs.inode_tree_root;
    let mut record = [0u8; INODE_SIZE];
    record[0] = 1;

    let mut buf = [0u8; MAX_BLOCK_SIZE];
    fs.read_meta(root, BlockType::Btree, &mut buf)
        .expect("root");
    wr_u64(&mut buf, btree::N_ENTRIES + 8, root);
    fs.begin().expect("begin");
    let cycled = fs
        .cow_meta(0, &mut buf, BlockType::Btree, spec.owner, 0)
        .expect("seal the cyclic node");
    assert_eq!(
        fs.btree_insert(cycled, 2, &record, spec),
        Err(DriverError::DeviceFault)
    );
    assert_eq!(
        fs.btree_remove(cycled, 2, spec),
        Err(DriverError::DeviceFault)
    );

    let mut deep = [0u8; MAX_BLOCK_SIZE];
    fs.read_meta(root, BlockType::Btree, &mut deep)
        .expect("root");
    wr_u32(&mut deep, btree::N_LEVEL, 200);
    let too_deep = fs
        .cow_meta(0, &mut deep, BlockType::Btree, spec.owner, 0)
        .expect("seal the over-deep node");
    assert_eq!(
        fs.btree_insert(too_deep, 2, &record, spec),
        Err(DriverError::DeviceFault)
    );
    assert_eq!(
        fs.btree_remove(too_deep, 2, spec),
        Err(DriverError::DeviceFault)
    );

    // A record that is not the tree's own width cannot be stored, so it is
    // refused rather than truncated or written past the entry.
    assert_eq!(
        fs.btree_insert(root, 2, &record[..8], spec),
        Err(DriverError::LengthOutOfRange)
    );
    fs.rollback();
}

#[test]
fn a_merge_of_two_empty_siblings_is_refused_rather_than_indexed_into() {
    // An internal node whose children are both empty is a shape no tree this
    // driver writes: a leaf emptied by a remove is merged away or refilled
    // before the operation ends. Reaching one means the volume is corrupt, so
    // the merge that would have to name a key the pair no longer holds fails
    // closed instead.
    let mut fs = fmt(512, 4096, 64);
    let spec = inode_spec();
    fs.begin().expect("begin");

    let mut leaf = [0u8; MAX_BLOCK_SIZE];
    fs.btree_init_node(&mut leaf, 0, 1);
    wr_u64(&mut leaf, btree::N_ENTRIES, 7);
    let low = fs
        .cow_meta(0, &mut leaf, BlockType::Btree, spec.owner, 0)
        .expect("seal the one-entry leaf");

    let mut empty = [0u8; MAX_BLOCK_SIZE];
    fs.btree_init_node(&mut empty, 0, 0);
    let high = fs
        .cow_meta(0, &mut empty, BlockType::Btree, spec.owner, 0)
        .expect("seal the empty leaf");

    let mut root = [0u8; MAX_BLOCK_SIZE];
    fs.btree_init_node(&mut root, 1, 2);
    let second = btree::N_ENTRIES + btree::INTERNAL_STRIDE;
    wr_u64(&mut root, btree::N_ENTRIES, 7);
    wr_u64(&mut root, btree::N_ENTRIES + 8, low);
    wr_u64(&mut root, second, 8);
    wr_u64(&mut root, second + 8, high);
    let root = fs
        .cow_meta(0, &mut root, BlockType::Btree, spec.owner, 0)
        .expect("seal the root naming both leaves");

    assert_eq!(
        fs.btree_remove(root, 7, spec),
        Err(DriverError::DeviceFault)
    );
    fs.rollback();
}

/// Stack bytes one call of `op` used, measured from the caller's own frame down
/// to the deepest device call it reached.
fn stack_cost<T>(
    fs: &mut ARXFS<MemBlock>,
    op: impl FnOnce(&mut ARXFS<MemBlock>) -> T,
) -> (T, usize) {
    let anchor = 0u8;
    let base = core::ptr::addr_of!(anchor) as usize;
    fs.block_mut().arm_stack();
    let out = op(fs);
    let used = fs.block_mut().stack_used(base);
    (out, used)
}

#[test]
fn a_mutation_costs_the_same_stack_whatever_the_tree_depth() {
    // Half the 32 KiB thread stack the kernel hosts this driver on. What the
    // whole chain spends is the driver's on-stack block staging, a chain of
    // block-sized buffers down the write path, not the tree edit: the
    // mutation's own frames are a few hundred bytes each.
    const STACK_BUDGET: usize = 16 * 1024;

    // The kernel hosts this driver on a 32 KiB thread stack behind a guard
    // page, so a mutation whose stack grows per tree level faults on an
    // ordinary write to a fragmented file. Both trees below take the same
    // insert and the same remove; what must not differ is how deep the stack
    // went, because nothing on the path recurses and every node buffer the
    // edit needs lives in the scratch the mount lends it.
    let mut shallow = fmt(512, 1 << 16, 64);
    let mut deep = fmt(512, 1 << 16, 64);
    let root = shallow.root();
    shallow
        .create(root, b"f", NodeKind::RegularFile)
        .expect("create");
    deep.create(root, b"f", NodeKind::RegularFile)
        .expect("create");
    let cap = shallow.data_capacity();
    let shallow_level = fragment(&mut shallow, b"f", 4);
    let deep_level = fragment(&mut deep, b"f", 900);
    assert_eq!(shallow_level, 0, "four extents are one leaf");
    assert!(
        deep_level >= 2,
        "the deep fixture must be at least a three-level tree (level {deep_level})"
    );

    // One more extent past the end of each file: an insert that descends the
    // whole tree and, in the deep case, may split on the way back up.
    let at = |runs: u64| runs * 2 * cap;
    let (wrote, shallow_insert) =
        stack_cost(&mut shallow, |fs| fs.write_at(root, b"f", at(4), &[0x5A]));
    assert_eq!(wrote, Ok(1));
    let (wrote, deep_insert) =
        stack_cost(&mut deep, |fs| fs.write_at(root, b"f", at(900), &[0x5A]));
    assert_eq!(wrote, Ok(1));

    // Then a remove of the whole map, which is where the rebalance lives.
    let (out, shallow_remove) = stack_cost(&mut shallow, |fs| fs.truncate(root, b"f", 0));
    assert_eq!(out, Ok(()));
    let (out, deep_remove) = stack_cost(&mut deep, |fs| fs.truncate(root, b"f", 0));
    assert_eq!(out, Ok(()));

    // A step that recursed would carry at least the node it is editing into
    // every extra level, so per-level growth cannot hide below one block.
    let node = deep.block_size;
    assert!(
        deep_insert.abs_diff(shallow_insert) < node,
        "an insert into a {deep_level}-level tree used {deep_insert} stack bytes \
         against {shallow_insert} for a single leaf"
    );
    assert!(
        deep_remove.abs_diff(shallow_remove) < node,
        "a remove over a {deep_level}-level tree used {deep_remove} stack bytes \
         against {shallow_remove} for a single leaf"
    );

    assert!(
        deep_insert <= STACK_BUDGET && deep_remove <= STACK_BUDGET,
        "insert used {deep_insert} bytes and remove {deep_remove}, past the \
         {STACK_BUDGET}-byte budget"
    );
}

/// Fragment `runs` single-block writes into `name`, one extent each, and
/// return the resulting extent tree's level.
fn fragment(fs: &mut ARXFS<MemBlock>, name: &[u8], runs: u64) -> u32 {
    let root = fs.root();
    let cap = fs.data_capacity();
    for run in 0..runs {
        assert_eq!(fs.write_at(root, name, run * 2 * cap, &[0x5A]), Ok(1));
    }
    let ino = file_ino(fs, name);
    let inode = fs.read_inode(ino).expect("inode");
    tree_level(fs, inode.extent_root)
}

/// Free space after creating `name`, fragmenting it into `runs` extents, and
/// discarding it: by `Discard::Truncate` back to zero length, or by
/// `Discard::Remove` of the whole file. Also returns the free count right
/// after the (empty) create and the extent tree's level.
fn free_after_discard(runs: u64, discard: Discard) -> (u64, u64, u32) {
    let mut fs = fmt(512, 8192, 64);
    let root = fs.root();
    fs.create(root, b"frag", NodeKind::RegularFile)
        .expect("create");
    fs.commit().expect("commit");
    let after_create = fs.free_count;
    let level = fragment(&mut fs, b"frag", runs);
    match discard {
        Discard::Truncate => fs.truncate(root, b"frag", 0).expect("truncate"),
        Discard::Remove => fs.remove(root, b"frag").expect("remove"),
    }
    (fs.free_count, after_create, level)
}

/// How [`free_after_discard`] gives the file's blocks back.
#[derive(Copy, Clone)]
enum Discard {
    Truncate,
    Remove,
}

#[test]
fn freeing_a_deep_extent_tree_returns_every_block_it_held() {
    // A fragmented file on a 512-byte volume is one extent per written run,
    // so a few hundred writes build a multi-level extent tree. The walk must
    // free every node of it exactly once: a node it misses leaks the pair for
    // the life of the volume, and one it frees twice corrupts the map.
    let (deep_truncate, after_create, deep_level) = free_after_discard(300, Discard::Truncate);
    assert!(deep_level >= 2, "the fixture must build a deep extent tree");
    assert_eq!(
        deep_truncate, after_create,
        "truncating the deep file to zero returns every extent and every node          of the tree that mapped them"
    );

    // Deleting the file frees the same extents and nodes; what create left
    // behind (the inode record's tree node, the directory's block) is not the
    // file's to return, so the assertion is that the depth of the tree makes
    // no difference to what a delete gives back.
    let (deep_remove, _, level) = free_after_discard(300, Discard::Remove);
    assert!(level >= 2);
    let (shallow_remove, _, shallow_level) = free_after_discard(1, Discard::Remove);
    assert_eq!(shallow_level, 0, "one extent is one leaf");
    assert_eq!(
        deep_remove, shallow_remove,
        "deleting a file with a three-level extent tree returns as much as          deleting one with a single leaf"
    );
}

#[test]
fn rebuilding_free_space_over_a_deep_tree_reproduces_the_live_map() {
    // The rebuild marks every node of every tree from the walk's path rather
    // than from a collected node list, so it must reach exactly the blocks the
    // live map already holds.
    let (mut fs, _) = deep_inode_tree(200);
    let root = fs.root();
    let cap = fs.data_capacity();
    for run in 0..200u64 {
        assert_eq!(fs.write_at(root, b"f0", run * 2 * cap, &[0xA5]), Ok(1));
    }
    fs.commit().expect("commit");
    let live = fs.used_blocks();
    let free = fs.free_count;

    fs.rebuild_free_space().expect("rebuild the allocation map");
    assert_eq!(
        fs.used_blocks(),
        live,
        "the rebuilt used set is the live one"
    );
    assert_eq!(fs.free_count, free);
    // The rebuild reads the trees a leaf at a time, so the only thing it holds
    // across the walk is the map's own bounded page cache.
    assert!(
        fs.map_cached_blocks() <= MAX_CACHED_PAGES,
        "the rebuild left a bounded map cache (cached {})",
        fs.map_cached_blocks()
    );
}

// --- the commit scheduler ------------------------------------------------
//
// A transaction stays open and the next operation joins it, so the tests
// below drive a volume whose host clock stays at the mount (the window never
// elapses) and close the transaction deliberately, or move that clock past
// every class's window to prove the age closes it.

/// A monotonic reading past the widest window any device class is served
/// with, so moving the host clock to it ages the open transaction out.
const PAST_EVERY_WINDOW: u64 = 60_000_000_000;

/// A volume whose operations batch, and the timer it reports to: the default
/// `MemBlock` declares the paravirtual class, so its window is the shortest,
/// and a clock held at the mount keeps the transaction open regardless.
fn fmt_batched_with_host(
    block_size: u32,
    block_count: u64,
    inodes: u32,
) -> (ARXFS<MemBlock>, &'static TestWritebackHost) {
    let host = TestWritebackHost::leaked(0);
    let fs =
        fmt(block_size, block_count, inodes).with_writeback_host(TestWritebackHost::volume(), host);
    (fs, host)
}

/// The batching volume alone, for a test that does not inspect the timer.
fn fmt_batched(block_size: u32, block_count: u64, inodes: u32) -> ARXFS<MemBlock> {
    fmt_batched_with_host(block_size, block_count, inodes).0
}

/// Reopen a mid-flight snapshot of the device, exactly as a power loss and a
/// remount would see it — the driver's staged blocks are not in it.
fn reopen_device(fs: &mut ARXFS<MemBlock>, block_size: u32, block_count: u64) -> ARXFS<MemBlock> {
    let bytes = fs.block_mut().bytes();
    ARXFS::open(
        MemBlock::from_bytes(bytes, block_size, block_count),
        &TEST_KEY,
    )
    .expect("the committed state remounts")
}

#[test]
fn operations_join_one_transaction_until_something_closes_it() {
    let mut fs = fmt_batched(512, 512, 32);
    let root = fs.root();
    for name in [b"one".as_slice(), b"two", b"three"] {
        fs.create(root, name, NodeKind::RegularFile)
            .expect("create");
    }
    // Nothing has published: a remount of the device as it stands finds the
    // freshly formatted volume.
    let mut before = reopen_device(&mut fs, 512, 512);
    let before_root = before.root();
    assert_eq!(
        before.lookup(before_root, b"one"),
        Err(DriverError::NotFound),
        "an open transaction must not be visible on the device"
    );

    fs.flush().expect("sync publishes the batch");
    let mut after = reopen_device(&mut fs, 512, 512);
    let after_root = after.root();
    for name in [b"one".as_slice(), b"two", b"three"] {
        after
            .lookup(after_root, name)
            .expect("every name published");
    }
}

#[test]
fn one_batch_costs_one_commit_where_three_operations_cost_three() {
    let mut batched = fmt_batched(512, 512, 32);
    let root = batched.root();
    let start = batched.generation;
    for name in [b"one".as_slice(), b"two", b"three"] {
        batched
            .create(root, name, NodeKind::RegularFile)
            .expect("create");
    }
    batched.flush().expect("sync");
    assert_eq!(
        batched.generation - start,
        1,
        "three operations inside the window publish one transaction root"
    );

    let mut per_op = fmt(512, 512, 32);
    let root = per_op.root();
    let start = per_op.generation;
    for name in [b"one".as_slice(), b"two", b"three"] {
        per_op
            .create(root, name, NodeKind::RegularFile)
            .expect("create");
    }
    assert_eq!(
        per_op.generation - start,
        3,
        "with no monotonic clock every operation publishes its own"
    );
}

#[test]
fn an_operation_after_the_window_publishes_the_transaction_it_would_have_joined() {
    let (mut fs, host) = fmt_batched_with_host(512, 512, 32);
    let root = fs.root();
    fs.create(root, b"early", NodeKind::RegularFile)
        .expect("create");
    let published = fs.generation;
    // Time passes past the window; the next operation must not extend the
    // transaction it joins.
    host.set_now(PAST_EVERY_WINDOW);
    fs.create(root, b"late", NodeKind::RegularFile)
        .expect("create");
    assert_eq!(
        fs.generation,
        published + 1,
        "an aged-out transaction publishes at the end of the operation"
    );
    let mut disk = reopen_device(&mut fs, 512, 512);
    let disk_root = disk.root();
    disk.lookup(disk_root, b"early").expect("early published");
    disk.lookup(disk_root, b"late").expect("late published");
}

#[test]
fn a_refused_operation_leaves_the_batch_it_joined_intact() {
    let mut fs = fmt_batched(512, 512, 32);
    let root = fs.root();
    fs.create(root, b"kept", NodeKind::RegularFile)
        .expect("create");
    assert_eq!(fs.write_at(root, b"kept", 0, b"payload"), Ok(7));
    // An ordinary refusal part-way through a joined operation.
    assert_eq!(
        fs.create(root, b"kept", NodeKind::RegularFile),
        Err(DriverError::AlreadyExists)
    );
    assert_eq!(
        fs.write_at(root, b"absent", 0, b"nothing"),
        Err(DriverError::NotFound)
    );
    fs.flush().expect("sync");

    let mut disk = reopen_device(&mut fs, 512, 512);
    let disk_root = disk.root();
    let node = disk.lookup(disk_root, b"kept").expect("the kept name");
    let mut back = [0u8; 7];
    assert_eq!(disk.read_at(node, 0, &mut back), Ok(7));
    assert_eq!(&back, b"payload");
    assert_eq!(
        disk.lookup(disk_root, b"absent"),
        Err(DriverError::NotFound)
    );
    assert_eq!(
        disk.check(&GrantAll, &NullSink).expect("check").structure,
        StructureVerdict::Sound,
        "a refused operation inside a batch must leave the volume sound"
    );
}

#[test]
fn an_operation_that_runs_out_of_space_leaves_the_batch_it_joined_readable() {
    // The failure that mutates before it fails: a write that allocates its
    // way to the end of the volume. Everything the batch already reported
    // successful must survive it, byte for byte.
    let mut fs = fmt_batched(512, 512, 32);
    let root = fs.root();
    fs.create(root, b"kept", NodeKind::RegularFile)
        .expect("create");
    let body = incompressible(4096);
    assert_eq!(fs.write_at(root, b"kept", 0, &body), Ok(4096));
    fs.create(root, b"greedy", NodeKind::RegularFile)
        .expect("create");
    // Fill the volume until one write runs out of space part-way through,
    // after it has already allocated and rewritten tree blocks the batch's
    // earlier operations also touched.
    let chunk = incompressible(8192);
    let mut at = 0u64;
    let mut refused = false;
    for _ in 0..64 {
        match fs.write_at(root, b"greedy", at, &chunk) {
            Ok(written) => at += written as u64,
            Err(DriverError::NoSpace) => {
                refused = true;
                break;
            }
            Err(other) => panic!("unexpected refusal {other:?}"),
        }
    }
    assert!(refused, "the volume must run out of space");
    fs.flush().expect("sync what survived");

    let mut disk = reopen_device(&mut fs, 512, 512);
    let disk_root = disk.root();
    let node = disk.lookup(disk_root, b"kept").expect("the kept name");
    let mut back = alloc::vec![0u8; 4096];
    assert_eq!(disk.read_at(node, 0, &mut back), Ok(4096));
    assert_eq!(back, body);
    disk.lookup(disk_root, b"greedy").expect("the greedy name");
    assert_eq!(
        disk.check(&GrantAll, &NullSink).expect("check").structure,
        StructureVerdict::Sound
    );
}

#[test]
fn an_unsynced_batch_is_lost_whole_and_leaves_the_prior_state_sound() {
    let mut fs = fmt_batched(512, 512, 32);
    let root = fs.root();
    fs.create(root, b"durable", NodeKind::RegularFile)
        .expect("create");
    assert_eq!(fs.write_at(root, b"durable", 0, b"safe"), Ok(4));
    fs.flush().expect("sync");
    // A second batch that never closes.
    fs.create(root, b"volatile", NodeKind::RegularFile)
        .expect("create");
    assert_eq!(fs.write_at(root, b"durable", 0, b"lost"), Ok(4));

    let mut disk = reopen_device(&mut fs, 512, 512);
    let disk_root = disk.root();
    let node = disk
        .lookup(disk_root, b"durable")
        .expect("the durable name");
    let mut back = [0u8; 4];
    assert_eq!(disk.read_at(node, 0, &mut back), Ok(4));
    assert_eq!(&back, b"safe", "consistency is never traded, only recency");
    assert_eq!(
        disk.lookup(disk_root, b"volatile"),
        Err(DriverError::NotFound)
    );
    assert_eq!(
        disk.check(&GrantAll, &NullSink).expect("check").structure,
        StructureVerdict::Sound
    );
}

#[test]
fn every_barrier_requiring_operation_publishes_the_open_transaction_first() {
    // Each of these reads or rewrites the committed volume, so none may run
    // over a volume half of whose state is still staged in RAM.
    let mut fs = ARXFS::format(
        MemBlock::new(512, 512).with_discard(1, 0),
        32,
        &TEST_KEY,
        &mut TestEntropy::new(),
    )
    .expect("format")
    .with_clock(fixed_clock)
    .with_writeback_host(TestWritebackHost::volume(), TestWritebackHost::leaked(0));
    let root = fs.root();

    let mut ran = 0;
    for (name, run) in [
        (
            b"a".as_slice(),
            &(|fs: &mut ARXFS<MemBlock>| fs.trim(&GrantAll, &NullSink).map(|_| ()))
                as &dyn Fn(&mut ARXFS<MemBlock>) -> Result<(), DriverError>,
        ),
        (b"b", &|fs| {
            fs.scrub(&GrantAll, &NullSink, ScrubBudget::Unlimited)
                .map(|_| ())
        }),
        (b"c", &|fs| fs.check(&GrantAll, &NullSink).map(|_| ())),
        (b"d", &|fs| fs.health(&GrantAll, &NullSink).map(|_| ())),
    ] {
        fs.create(root, name, NodeKind::RegularFile)
            .expect("create joins the open transaction");
        assert!(fs.schedule.is_open(), "the transaction stayed open");
        run(&mut fs).expect("the maintenance pass runs");
        assert!(
            !fs.schedule.is_open(),
            "a barrier-requiring operation must close the transaction first"
        );
        let mut disk = reopen_device(&mut fs, 512, 512);
        let disk_root = disk.root();
        disk.lookup(disk_root, name)
            .expect("the joined operation was published before the pass ran");
        ran += 1;
    }
    assert_eq!(ran, 4);
}

#[test]
fn growing_a_volume_publishes_the_open_transaction_first() {
    let mut fs = fmt_batched(512, 256, 32);
    let root = fs.root();
    fs.create(root, b"before", NodeKind::RegularFile)
        .expect("create");
    assert!(fs.schedule.is_open());
    fs.block_mut().enlarge_to(512);
    assert_eq!(fs.grow(), Ok(256));
    assert!(!fs.schedule.is_open());
    let mut disk = reopen_device(&mut fs, 512, 512);
    let disk_root = disk.root();
    disk.lookup(disk_root, b"before")
        .expect("the joined operation was published before the region moved");
}

#[test]
fn handing_the_volume_on_publishes_the_open_transaction() {
    let mut fs = fmt_batched(512, 512, 32);
    let root = fs.root();
    fs.create(root, b"handed", NodeKind::RegularFile)
        .expect("create");
    let bytes = fs.into_block().expect("the volume closes").bytes();
    let mut disk = ARXFS::open(MemBlock::from_bytes(bytes, 512, 512), &TEST_KEY).expect("remount");
    let disk_root = disk.root();
    disk.lookup(disk_root, b"handed")
        .expect("handing the volume on published the batch");
}

#[test]
fn a_commit_that_loses_reported_operations_freezes_the_handle() {
    let mut fs = fmt_batched(512, 512, 32);
    let root = fs.root();
    fs.create(root, b"reported", NodeKind::RegularFile)
        .expect("create");
    assert_eq!(fs.schedule.acknowledged(), 1);
    // The device stops taking writes before the batch can be published.
    fs.block_mut().write_fault_after = Some(0);
    assert!(fs.flush().is_err(), "the sync must fail closed");
    assert!(
        fs.read_only,
        "a handle that can no longer honour an operation it reported must stop \
         accepting writes"
    );
    assert_eq!(
        fs.create(root, b"more", NodeKind::RegularFile),
        Err(DriverError::PermissionDenied)
    );
    // The volume itself is untouched: the abandoned batch published nothing.
    fs.block_mut().write_fault_after = None;
    let mut disk = reopen_device(&mut fs, 512, 512);
    let disk_root = disk.root();
    assert_eq!(
        disk.lookup(disk_root, b"reported"),
        Err(DriverError::NotFound)
    );
    assert_eq!(
        disk.check(&GrantAll, &NullSink).expect("check").structure,
        StructureVerdict::Sound
    );
}

#[test]
fn no_operation_leaves_its_savepoint_installed_behind_it() {
    // A savepoint outliving the operation that took it would record the next
    // caller's changes against a snapshot nothing will unwind to.
    let mut fs = fmt_batched(512, 512, 32);
    let root = fs.root();
    fs.create(root, b"one", NodeKind::RegularFile)
        .expect("a successful operation");
    assert!(!fs.dirty.operation_running());
    assert_eq!(
        fs.create(root, b"one", NodeKind::RegularFile),
        Err(DriverError::AlreadyExists),
        "a refused operation"
    );
    assert!(!fs.dirty.operation_running());
    fs.rename(root, b"one", root, b"one")
        .expect("a rename onto the same entry changes nothing");
    assert!(!fs.dirty.operation_running());
    fs.flush().expect("sync");
    assert!(!fs.dirty.operation_running());
}

#[test]
fn a_long_batch_of_mixed_operations_and_refusals_publishes_a_sound_volume() {
    // The interleaving that exercises the savepoint hardest: operations that
    // free and reuse blocks earlier operations of the same transaction
    // claimed, with refusals scattered through them, all inside one
    // transaction.
    let mut fs = fmt_batched(512, 2048, 64);
    let root = fs.root();
    let body = incompressible(3000);
    for round in 0..8u8 {
        let name = [b'f', b'0' + round];
        fs.create(root, &name, NodeKind::RegularFile)
            .expect("create");
        assert_eq!(fs.write_at(root, &name, 0, &body), Ok(body.len()));
        assert_eq!(
            fs.create(root, &name, NodeKind::RegularFile),
            Err(DriverError::AlreadyExists),
            "a refusal part-way through the batch"
        );
        fs.truncate(root, &name, 512).expect("truncate");
        assert_eq!(
            fs.remove(root, b"absent"),
            Err(DriverError::NotFound),
            "another refusal"
        );
        if round.is_multiple_of(3) {
            fs.remove(root, &name).expect("remove");
        }
    }
    assert!(fs.schedule.is_open(), "the whole run was one transaction");
    fs.flush().expect("sync");

    let mut disk = reopen_device(&mut fs, 512, 2048);
    let disk_root = disk.root();
    for round in 0..8u8 {
        let name = [b'f', b'0' + round];
        let found = disk.lookup(disk_root, &name);
        if round.is_multiple_of(3) {
            assert_eq!(found, Err(DriverError::NotFound), "round {round} removed");
        } else {
            let node = found.expect("round survived");
            let mut back = alloc::vec![0u8; 512];
            assert_eq!(disk.read_at(node, 0, &mut back), Ok(512));
            assert_eq!(back, body[..512], "round {round} kept its truncated bytes");
        }
    }
    let report = disk.check(&GrantAll, &NullSink).expect("check");
    assert_eq!(report.structure, StructureVerdict::Sound, "{report:?}");
    assert_eq!(report.unrecoverable_findings, 0, "{report:?}");
}

#[test]
fn a_commit_that_loses_nothing_reported_leaves_the_handle_writable() {
    // The single-operation case must behave exactly as it always has: the
    // caller is told its operation failed, and the volume carries on.
    let mut fs = fmt(512, 512, 32);
    let root = fs.root();
    fs.create(root, b"first", NodeKind::RegularFile)
        .expect("create");
    fs.block_mut().write_fault_after = Some(0);
    assert!(fs.create(root, b"second", NodeKind::RegularFile).is_err());
    fs.block_mut().write_fault_after = None;
    assert!(!fs.read_only, "no reported operation was lost");
    fs.create(root, b"third", NodeKind::RegularFile)
        .expect("the handle still serves writes");
}

// ---------------------------------------------------------------------------
// Extent-based deferred freeing: a transaction's block bookkeeping is a set of
// runs, so what it holds follows the runs it touches and never the blocks.
// ---------------------------------------------------------------------------

/// Runs and blocks the open transaction has deferred for freeing.
fn deferred(fs: &ARXFS<MemBlock>) -> (usize, u64) {
    let alloc = fs.allocator().expect("writable");
    (alloc.txn_freed.len(), alloc.txn_freed.covered())
}

#[test]
fn deleting_a_contiguous_file_defers_one_run_not_one_entry_per_block() {
    // A file written sequentially lands in one physical run, so releasing it
    // must cost the deferred-free set one entry. The per-block set this
    // replaced held one `u64` per block, so an `rm` of a large file allocated
    // memory proportional to its size.
    let mut fs = fmt_batched(512, 4096, 64);
    let root = fs.root();
    let cap = as_usize(fs.data_capacity());
    let blocks = 600usize;
    let body = incompressible(cap * blocks);
    fs.create(root, b"big", NodeKind::RegularFile)
        .expect("create");
    assert_eq!(fs.write_at(root, b"big", 0, &body), Ok(body.len()));
    fs.flush()
        .expect("publish the file so its blocks are committed");

    let ino = file_ino(&mut fs, b"big");
    let inode = fs.read_inode(ino).expect("inode");
    let extents = tree_entries(&mut fs, inode.extent_root, extent_spec(ino)).len();
    assert!(
        extents <= 2,
        "the fixture must really be contiguous (it holds {extents} extents)"
    );

    fs.remove(root, b"big").expect("remove");
    let (runs, freed) = deferred(&fs);
    assert!(
        freed >= blocks as u64,
        "the delete must really have released the file's blocks (freed {freed})"
    );
    assert!(
        runs <= 4,
        "releasing {freed} blocks of a contiguous file held {runs} deferred \
         runs; the data run, its extent-tree node pair, and the inode record \
         are all that should be in there"
    );
    fs.flush().expect("sync");
    let report = fs.check(&GrantAll, &NullSink).expect("check");
    assert_eq!(report.structure, StructureVerdict::Sound, "{report:?}");
}

#[test]
fn a_failed_operation_undoes_only_the_deferred_runs_it_added() {
    // Two operations of one transaction defer overlapping runs. Undoing the
    // second must leave the first's blocks deferred: a run set records the part
    // each operation actually added, so the overlap is not double-counted and
    // then lost.
    let mut fs = fmt_batched(512, 512, 32);
    let base = RING_BLOCKS + 8;

    fs.begin().expect("first operation");
    fs.defer_free(base, 10);
    assert_eq!(deferred(&fs), (1, 10));
    fs.end_operation().expect("the first operation succeeds");

    fs.begin().expect("second operation");
    fs.defer_free(base + 5, 15);
    assert_eq!(deferred(&fs), (1, 20), "the two runs coalesce");
    assert_eq!(
        fs.allocator().expect("writable").op_deferred.covered(),
        10,
        "the second operation added only the part the first had not"
    );
    fs.rollback();

    assert_eq!(
        deferred(&fs),
        (1, 10),
        "undoing the second operation took back exactly its own runs"
    );
}

#[test]
fn releasing_a_run_frees_the_unshared_blocks_and_keeps_the_shared_one() {
    // The range release walks the chunk tree instead of asking per block, so
    // the interesting case is a run whose middle block is shared: everything
    // around it is freed as runs, and the shared block survives with the
    // reference the other file still holds.
    let mut fs = fmt(512, 512, 32);
    let root = fs.root();
    let cap = as_usize(fs.data_capacity());
    // A distinct, incompressible-enough pattern per block, so the four blocks
    // neither dedupe onto one another nor cluster into a compressed extent.
    let mut body = alloc::vec![0u8; cap * 4];
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    for byte in &mut body {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *byte = state.to_le_bytes()[3];
    }
    fs.create(root, b"a", NodeKind::RegularFile)
        .expect("create");
    assert_eq!(fs.write_at(root, b"a", 0, &body), Ok(body.len()));

    let ino = file_ino(&mut fs, b"a");
    let inode = fs.read_inode(ino).expect("inode");
    let entries = tree_entries(&mut fs, inode.extent_root, extent_spec(ino));
    assert_eq!(entries.len(), 1, "the fixture must be one run: {entries:?}");
    let total = fs.total_blocks;
    let ext = Extent::decode(&entries[0].1, total).expect("extent decodes");
    assert_eq!(ext.len, 4);

    // A second file holding a byte-identical copy of block 1 dedupes onto
    // `a`'s physical block, so exactly one block of the run is shared.
    fs.create(root, b"share", NodeKind::RegularFile)
        .expect("create");
    assert_eq!(fs.write_at(root, b"share", 0, &body[cap..cap * 2]), Ok(cap));
    let shared = ext.phys + 1;
    assert_eq!(
        fs.data_refcount(shared).expect("refcount"),
        2,
        "the fixture must really share the middle block"
    );

    fs.remove(root, b"a").expect("remove");
    fs.flush().expect("sync");
    assert!(
        fs.is_used(shared),
        "the shared block was freed while another file still names it"
    );
    assert_eq!(
        fs.data_refcount(shared).expect("refcount"),
        1,
        "the shared block returns to the implicit single reference"
    );
    for offset in [0u64, 2, 3] {
        assert!(
            !fs.is_used(ext.phys + offset),
            "unshared block {offset} of the run was not released"
        );
    }
    let node = fs.lookup(root, b"share").expect("the sharer survives");
    let mut back = alloc::vec![0u8; cap];
    assert_eq!(fs.read_at(node, 0, &mut back), Ok(cap));
    assert_eq!(back, body[cap..cap * 2], "the surviving file's bytes");
    let report = fs.check(&GrantAll, &NullSink).expect("check");
    assert_eq!(report.structure, StructureVerdict::Sound, "{report:?}");
}

#[test]
fn a_ceiling_query_finds_the_next_key_in_this_leaf_or_the_next_subtree() {
    // The range release steps on the ceiling query, so it must be exact at
    // both of its two answers: inside the leaf the descent landed in, and in
    // the next subtree when every key of that leaf is below the query.
    let mut fs = fmt(512, 512, 32);
    let spec = chunk_spec();
    let mut root = 0u64;
    let keys: alloc::vec::Vec<u64> = (0..64u64).map(|i| 100 + i * 10).collect();
    let record = ChunkRecord {
        refcount: 2,
        domain: 0,
        length: 16,
        logical_hash: [0x5A; LOGICAL_HASH_LEN],
    };
    for &key in &keys {
        root = fs
            .btree_insert(root, key, &record.encode(), spec)
            .expect("insert");
    }

    for &key in &keys {
        for probe in [key - 9, key - 1, key] {
            let found = fs
                .btree_get_ceil(root, probe, spec)
                .expect("ceiling")
                .map(|(found, _)| found);
            assert_eq!(found, Some(key), "ceiling of {probe}");
        }
    }
    assert_eq!(
        fs.btree_get_ceil(root, 0, spec)
            .expect("ceiling")
            .map(|f| f.0),
        Some(keys[0]),
        "a key below every entry finds the smallest"
    );
    assert_eq!(
        fs.btree_get_ceil(root, u64::MAX, spec).expect("ceiling"),
        None,
        "a key above every entry finds nothing"
    );
    assert_eq!(
        fs.btree_get_ceil(0, 5, spec).expect("ceiling"),
        None,
        "an empty tree answers nothing"
    );
}

#[test]
fn trim_splits_a_queued_run_around_a_reallocated_block() {
    // The queue holds runs, so trim must split one against the live map rather
    // than trusting it whole: a block handed back out inside a queued run is
    // skipped and the free parts either side are still discarded.
    let mut fs = fmt_discard(512, 1, 0);
    let start = RING_BLOCKS + 16;
    fs.enqueue_discard_run(start, 12);
    fs.mark_run_used(start + 5, 2);

    let report = fs.trim(&GrantAll, &NullSink).expect("trim");
    assert!(report.supported);
    assert_eq!(
        report.blocks_skipped_in_use, 2,
        "the reallocated blocks must be skipped: {report:?}"
    );
    assert_eq!(
        report.blocks_discarded, 10,
        "both free parts of the run must still be discarded: {report:?}"
    );
    assert_eq!(
        report.ranges_discarded, 2,
        "the run splits into two device ranges: {report:?}"
    );
    assert_eq!(
        fs.block.discarded,
        alloc::vec![(start, 5), (start + 7, 5)],
        "the device saw the two free parts, not the whole run"
    );
}

#[test]
fn a_deferred_mark_over_a_long_run_costs_one_pending_entry() {
    // The map's deferred marks are runs too: a free whose bitmap pages are not
    // resident used to record one entry per block, which is the other half of
    // the same unbounded set.
    let mut fs = fmt_huge();
    // Drop every resident page, so the marks below can only be deferred.
    fs.allocator_mut().expect("writable").cache.clear();
    let start = RING_BLOCKS + 64;
    fs.mark_run_free(start, 1 << 20);
    let alloc = fs.allocator().expect("writable");
    assert_eq!(alloc.pending_free.len(), 1);
    assert_eq!(alloc.pending_free.covered(), 1 << 20);
    assert!(alloc.pending_used.is_empty());

    // The opposite mark over part of the run displaces it rather than being
    // held alongside it, so the latest mark over a block is the only one.
    fs.mark_run_used(start + 16, 32);
    let alloc = fs.allocator().expect("writable");
    assert_eq!(alloc.pending_used.covered(), 32);
    assert_eq!(alloc.pending_free.covered(), (1 << 20) - 32);
    assert_eq!(alloc.pending_free.len(), 2, "the free run split");

    // Folding them in leaves the map exact and the pending sets empty.
    fs.map_fold_pending().expect("fold");
    let alloc = fs.allocator().expect("writable");
    assert!(alloc.pending_used.is_empty() && alloc.pending_free.is_empty());
    assert!(fs.is_used(start + 16) && fs.is_used(start + 47));
    assert!(!fs.is_used(start + 15) && !fs.is_used(start + 48));
}

// ---------------------------------------------------------------------------
// The pending-delete set: freeing across transactions
// (`docs/src/filesystem/arxfs-spec.md` §14; `plans/OPEN-DEFECTS.md` D67).
// ---------------------------------------------------------------------------

/// Machine size whose per-volume write-back share is exactly one transfer
/// window — the smallest a volume may be mounted on, and the ceiling the tests
/// below make a delete yield to. Derived from the backing bytes exactly as a
/// mount derives it, never spelled as a ceiling.
const FLOOR_MACHINE_BYTES: usize = 16 * RUN_BYTES;

/// Extents a fragmented fixture holds: comfortably more run bookkeeping than
/// the floor ceiling admits, so a delete of it cannot fit one transaction.
const SPANNING_EXTENTS: u64 = 1200;

/// Bound `fs` as a host bounds a mount on the smallest machine a volume may be
/// mounted on, so the write-back ceiling is one transfer window.
fn floor_bounded(fs: ARXFS<MemBlock>) -> ARXFS<MemBlock> {
    let gauge: &'static ReportedPressure = Box::leak(Box::new(ReportedPressure::unknown()));
    gauge.report(PressureBand::Normal);
    fs.with_writeback_bound(
        FLOOR_MACHINE_BYTES,
        gauge,
        Arc::new(PinnedAccounting::new()),
    )
    .expect("the floor machine bounds the volume")
}

/// Every inode the pending-delete set names.
fn pending_deletes(fs: &mut ARXFS<MemBlock>) -> Vec<u64> {
    tree_entries(fs, fs.pending_delete_root, pending_delete_spec())
        .into_iter()
        .map(|(key, _)| key)
        .collect()
}

/// A bounded volume holding a witness file and a fragmented file of
/// [`SPANNING_EXTENTS`] single-block extents, committed.
fn spanning_delete_volume() -> ARXFS<MemBlock> {
    let mut fs = floor_bounded(fmt(512, 1 << 14, 64));
    let root = fs.root();
    fs.create(root, b"keep", NodeKind::RegularFile)
        .expect("create the witness");
    fs.write_at(root, b"keep", 0, b"witness")
        .expect("write the witness");
    fs.create(root, b"frag", NodeKind::RegularFile)
        .expect("create");
    fragment(&mut fs, b"frag", SPANNING_EXTENTS);
    fs.flush().expect("publish the fixture");
    fs
}

#[test]
fn a_delete_that_outruns_its_transaction_is_published_as_pending() {
    // The point of the set: the operation that detaches the last name commits
    // in bounded time whatever the file's extent count, naming the inode so
    // the freeing can continue afterwards.
    let mut fs = spanning_delete_volume();
    let root = fs.root();
    let ino = u64::from(file_ino(&mut fs, b"frag"));
    let free_before = fs.free_count;

    fs.begin().expect("begin");
    fs.remove_inner(root, b"frag").expect("detach the name");
    assert_eq!(
        pending_deletes(&mut fs),
        alloc::vec![ino],
        "the first transaction could not free {SPANNING_EXTENTS} extents, so it \
         must have published the inode instead"
    );
    assert_eq!(
        fs.lookup(root, b"frag"),
        Err(DriverError::NotFound),
        "the name is gone in that same transaction"
    );
    assert!(
        fs.free_count > free_before,
        "the first step still freed part of the tail"
    );

    // Draining finishes it, and the inode itself goes with the last step.
    fs.drain_pending_deletes().expect("drain");
    assert!(pending_deletes(&mut fs).is_empty());
    assert_eq!(
        fs.read_inode(u32::try_from(ino).unwrap()),
        Err(DriverError::NotFound)
    );
}

#[test]
fn an_interrupted_delete_is_finished_by_the_next_mount() {
    // A delete that spans transactions can be cut off between them. What
    // reaches the medium is then a volume with an unreachable inode the set
    // names, and the next writable mount must finish it before serving —
    // otherwise the blocks are leaked for the life of the volume.
    let mut fs = spanning_delete_volume();
    let root = fs.root();
    let ino = u64::from(file_ino(&mut fs, b"frag"));
    let empty = {
        // What the volume's free count is once the file is entirely gone,
        // measured on a second identical fixture.
        let mut whole = spanning_delete_volume();
        let root = whole.root();
        whole.remove(root, b"frag").expect("remove");
        whole.flush().expect("publish");
        whole.free_count
    };

    fs.begin().expect("begin");
    fs.remove_inner(root, b"frag").expect("detach the name");
    fs.flush().expect("publish the interrupted state");
    assert_eq!(pending_deletes(&mut fs), alloc::vec![ino]);
    let bytes = fs.into_block().expect("the volume closes").bytes();

    let mut fs = floor_bounded(
        ARXFS::open(MemBlock::from_bytes(bytes, 512, 1 << 14), &TEST_KEY)
            .expect("the interrupted volume mounts")
            .with_clock(fixed_clock),
    );
    assert!(
        pending_deletes(&mut fs).is_empty(),
        "the mount must finish the delete before it serves"
    );
    assert_eq!(
        fs.read_inode(u32::try_from(ino).unwrap()),
        Err(DriverError::NotFound)
    );
    assert_eq!(
        fs.free_count, empty,
        "the resumed delete returned exactly the blocks an uninterrupted one does"
    );
    // The witness is untouched and the map agrees with the trees.
    let keep = fs.lookup(fs.root(), b"keep").expect("the witness survives");
    assert_eq!(read_all(&mut fs, keep, 7), b"witness");
    let live = fs.used_blocks();
    fs.rebuild_free_space().expect("rebuild");
    assert_eq!(
        fs.used_blocks(),
        live,
        "no block was leaked or double-freed"
    );
}

#[test]
fn a_read_only_mount_leaves_a_pending_delete_alone() {
    // Read-only means read-only, including for recovery: the set stays, the
    // blocks stay, and nothing is written to the device.
    let mut fs = spanning_delete_volume();
    let root = fs.root();
    fs.begin().expect("begin");
    fs.remove_inner(root, b"frag").expect("detach the name");
    fs.flush().expect("publish the interrupted state");
    let bytes = fs.into_block().expect("the volume closes").bytes();

    let before = bytes.clone();
    let fs = ARXFS::open_read_only(MemBlock::from_bytes(bytes, 512, 1 << 14), &TEST_KEY)
        .expect("the interrupted volume mounts read-only");
    assert_ne!(fs.pending_delete_root, 0, "the set is left as it was found");
    let after = fs.into_block().expect("the volume closes").bytes();
    assert_eq!(after, before, "a read-only mount wrote nothing");
}

#[test]
fn a_stale_handle_cannot_give_a_pending_node_a_new_name() {
    // A node the set names is being reclaimed. Hard-linking it would put a live
    // name on blocks the reclaim is about to free, so the link is refused —
    // the one operation on a pending node that could lose data.
    let mut fs = spanning_delete_volume();
    let root = fs.root();
    let ino = file_ino(&mut fs, b"frag");
    let stale = NodeId::from_raw(u64::from(ino));

    fs.begin().expect("begin");
    fs.remove_inner(root, b"frag").expect("detach the name");
    assert_eq!(pending_deletes(&mut fs), alloc::vec![u64::from(ino)]);
    assert_eq!(
        fs.link(root, b"resurrected", stale),
        Err(DriverError::NotFound),
        "a node with no names left is not linkable"
    );
    fs.drain_pending_deletes().expect("drain");
    assert_eq!(fs.lookup(root, b"resurrected"), Err(DriverError::NotFound));
}

#[test]
fn a_truncate_that_outruns_its_transaction_is_only_ever_a_shorter_file() {
    // Freeing a tail upward from the cut would leave, after a crash, a file of
    // its original length with holes where its data had been. Freeing downward
    // leaves a *shorter* file: every byte below the published size is the byte
    // that was always there.
    let mut fs = floor_bounded(fmt(512, 1 << 14, 64));
    let root = fs.root();
    let cap = as_usize(fs.data_capacity());
    // Two files written a block at a time in turn, so each block of "a" has a
    // physical neighbour belonging to "b" and no two extents of "a" merge.
    fs.create(root, b"a", NodeKind::RegularFile)
        .expect("create");
    fs.create(root, b"b", NodeKind::RegularFile)
        .expect("create");
    let body = read_all_pattern(cap * as_usize(SPANNING_EXTENTS));
    for blk in 0..as_usize(SPANNING_EXTENTS) {
        let at = (blk * cap) as u64;
        for name in [b"a".as_slice(), b"b".as_slice()] {
            let mut done = 0usize;
            while done < cap {
                let n = fs
                    .write_at(
                        root,
                        name,
                        at + done as u64,
                        &body[blk * cap + done..(blk + 1) * cap],
                    )
                    .expect("write");
                assert!(n > 0);
                done += n;
            }
        }
    }
    fs.flush().expect("publish the fixture");
    let node = fs.lookup(root, b"a").expect("lookup");
    let whole = fs.node_info(node).expect("stat").size;
    assert_eq!(whole, (cap * as_usize(SPANNING_EXTENTS)) as u64);

    // Step the truncate by hand so the intermediate states are observable.
    let ino = file_ino(&mut fs, b"a");
    let mut steps = 0u32;
    let mut last = whole;
    loop {
        fs.begin().expect("begin");
        let done = fs.truncate_step(ino, 0).expect("a truncate step");
        let size = fs.node_info(node).expect("stat").size;
        assert!(
            size <= last,
            "the size only ever falls: {size} after {last}"
        );
        assert_eq!(
            read_all(&mut fs, node, as_usize(size)),
            body[..as_usize(size)],
            "the file that is left is the prefix of the one that was there"
        );
        last = size;
        steps += 1;
        if done {
            break;
        }
    }
    assert!(
        steps > 1,
        "the fixture must outrun one transaction, or nothing was measured"
    );
    assert_eq!(fs.node_info(node).expect("stat").size, 0);
    // The other file is untouched, and the map still agrees with the trees.
    let other = fs.lookup(root, b"b").expect("lookup");
    assert_eq!(read_all(&mut fs, other, body.len()), body);
    let live = fs.used_blocks();
    fs.rebuild_free_space().expect("rebuild");
    assert_eq!(fs.used_blocks(), live);
}

#[test]
fn a_check_reclaims_an_orphan_through_the_pending_set() {
    // `check` publishes each orphan it finds and lets the bounded drain free
    // it, for the same reason an unlink does: a very large orphan cannot be
    // freed inside the check's own transaction.
    let mut fs = floor_bounded(fmt(512, 1 << 14, 64));
    let root = fs.root();
    fs.create(root, b"keep", NodeKind::RegularFile)
        .expect("create the witness");
    fs.create(root, b"orphan", NodeKind::RegularFile)
        .expect("create");
    fragment(&mut fs, b"orphan", SPANNING_EXTENTS);
    fs.flush().expect("publish");
    let free_with_orphan = fs.free_count;

    // Detach the name without touching the inode, which is exactly the state a
    // damaged directory leaves behind.
    let mut dir = fs.read_inode(ROOT_INO).expect("root inode");
    fs.begin().expect("begin");
    fs.remove_entry(&mut dir, ROOT_INO, b"orphan")
        .expect("drop the entry");
    fs.write_inode(ROOT_INO, &dir).expect("write the root");
    fs.end_operation().expect("publish");

    let report = fs.check(&GrantAll, &NullSink).expect("check");
    assert_eq!((report.orphaned_inodes, report.orphans_reclaimed), (1, 1));
    assert!(pending_deletes(&mut fs).is_empty(), "the drain finished it");
    assert!(
        fs.free_count > free_with_orphan,
        "the orphan's blocks came back"
    );
    let live = fs.used_blocks();
    fs.rebuild_free_space().expect("rebuild");
    assert_eq!(fs.used_blocks(), live);
}

#[test]
fn the_reclaim_never_frees_a_node_a_name_still_reaches() {
    // Only a damaged volume can name a live node in the set — the name's
    // removal and the entry are one transaction — but if one does, the reclaim
    // must lose the space rather than the data.
    let mut fs = fmt(512, 8192, 64);
    let root = fs.root();
    fs.create(root, b"live", NodeKind::RegularFile)
        .expect("create");
    let body = read_all_pattern(2000);
    fs.write_at(root, b"live", 0, &body).expect("write");
    fs.commit().expect("commit");
    let ino = file_ino(&mut fs, b"live");

    fs.begin().expect("begin");
    fs.publish_pending_delete(ino).expect("plant the record");
    fs.end_operation().expect("publish");
    assert_eq!(pending_deletes(&mut fs), alloc::vec![u64::from(ino)]);

    fs.drain_pending_deletes().expect("drain");
    assert!(pending_deletes(&mut fs).is_empty(), "the record is dropped");
    let node = fs.lookup(root, b"live").expect("the name still resolves");
    assert_eq!(read_all(&mut fs, node, body.len()), body, "and its content");
    let live = fs.used_blocks();
    fs.rebuild_free_space().expect("rebuild");
    assert_eq!(fs.used_blocks(), live);
}
