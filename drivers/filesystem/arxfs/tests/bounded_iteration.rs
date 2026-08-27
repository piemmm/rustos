//! Regression guard for bounded metadata iteration: an operation that walks a
//! tree must hold a node's worth of bytes, not the tree's.
//!
//! The driver reads every tree a leaf at a time through one caller-owned
//! buffer, so the memory an operation needs is set by the block size and not
//! by how many records the tree holds. That is what lets a small machine mount
//! and serve volumes far larger than its RAM, and it is invisible to a
//! correctness test: a walk that gathered the whole tree first would return
//! exactly the same answers, just with a footprint that grows with the volume.
//!
//! This test measures it instead of asserting it by inspection. A counting
//! global allocator tracks live bytes and their high-water mark, and each
//! operation runs over trees of very different sizes: a stat's footprint must
//! be identical, and the map rebuild — the driver's widest metadata walk —
//! must stay inside a budget fixed by the block geometry. A regression, any
//! path that gathers a tree before it reads it, allocates per record and blows
//! both.
//!
//! Scope: the read paths whose own work *is* the walk. A mutating pass holds
//! more than the walk — the transaction's freed-block set, and the mutation
//! path's own per-record decoding — so its footprint is not the walk's to
//! bound and is not measured here (`plans/ARXFS-WRITEBACK.md`,
//! `plans/IMPLEMENT-OUTSTANDING-ARXFS.md`). Neither is `scrub` or the offline
//! `check`, which accumulate whole-volume reconciliation state of their own.

use std::alloc::System;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use core::alloc::{GlobalAlloc, Layout};

use tairix_abi::driver::block::{Block, BlockGeometry, DiscardCapability};
use tairix_abi::driver::filesystem::{FilesystemRead, FilesystemWrite, NodeKind};
use tairix_abi::DriverError;
use tairix_drv_fs_arxfs::{EntropySource, VolumeKey, ARXFS, VOLUME_KEY_LEN};

/// A pass-through allocator that tracks live bytes and their high-water mark
/// while counting is armed, so a measured window can be asserted to hold a
/// bounded footprint.
struct CountingAlloc;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static ARMED: AtomicBool = AtomicBool::new(false);

fn note_alloc(bytes: usize) {
    if !ARMED.load(Ordering::Relaxed) {
        return;
    }
    ALLOCS.fetch_add(1, Ordering::Relaxed);
    let live = LIVE.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

fn note_free(bytes: usize) {
    if ARMED.load(Ordering::Relaxed) {
        LIVE.fetch_sub(bytes.min(LIVE.load(Ordering::Relaxed)), Ordering::Relaxed);
    }
}

// SAFETY: every method forwards to the system allocator unchanged; the added
// behaviour is relaxed atomic counters, which cannot affect memory safety.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note_alloc(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        note_free(layout.size());
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note_alloc(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note_alloc(new_size.saturating_sub(layout.size()));
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAlloc = CountingAlloc;

/// Run `op` with the counters armed, returning its result, the peak live bytes
/// the window reached above where it started, and how many allocations it made.
fn measure<T>(op: impl FnOnce() -> T) -> (T, usize, usize) {
    LIVE.store(0, Ordering::Relaxed);
    PEAK.store(0, Ordering::Relaxed);
    ALLOCS.store(0, Ordering::Relaxed);
    ARMED.store(true, Ordering::Relaxed);
    let out = op();
    ARMED.store(false, Ordering::Relaxed);
    (
        out,
        PEAK.load(Ordering::Relaxed),
        ALLOCS.load(Ordering::Relaxed),
    )
}

const BLOCK_SIZE: u32 = 512;
const TEST_KEY: VolumeKey = [0x3c; VOLUME_KEY_LEN];

/// Deterministic stand-in for the platform RNG seam. Test scaffolding only.
struct TestEntropy(u8);

impl EntropySource for TestEntropy {
    fn fill(&mut self, out: &mut [u8]) -> Result<(), DriverError> {
        for byte in out.iter_mut() {
            *byte = self.0;
            self.0 = self.0.wrapping_add(1);
        }
        Ok(())
    }
}

/// Sparse in-RAM device: it stores only the blocks actually written, so a
/// device far larger than the host's RAM costs only its working set. Absent
/// blocks read as zeroes, exactly as a freshly provisioned device does.
struct SparseBlock {
    blocks: BTreeMap<u64, Vec<u8>>,
    block_count: u64,
}

impl Block for SparseBlock {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        Ok(BlockGeometry {
            block_size: BLOCK_SIZE,
            block_count: self.block_count,
        })
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        let bs = BLOCK_SIZE as usize;
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
        let bs = BLOCK_SIZE as usize;
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
        Ok(DiscardCapability::unsupported())
    }

    fn device_health(&self) -> Result<tairix_abi::driver::block::DeviceHealth, DriverError> {
        Ok(tairix_abi::driver::block::DeviceHealth::Unavailable)
    }

    fn discard(&mut self, _lba: u64, _blocks: u64) -> Result<(), DriverError> {
        Err(DriverError::Unsupported)
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        Ok(())
    }
}

/// Blocks of the device the volumes are formatted on. Large enough that the
/// bigger fixture never runs out, and sparse, so it costs only what it uses.
const DEVICE_BLOCKS: u64 = 1 << 18;

/// A volume holding `files` fragmented files of `runs` single-block writes
/// each: one extent per run, so both the inode tree and every extent tree grow
/// past a single node. On a 512-byte volume a leaf holds one record, which is
/// what makes a few hundred records a genuinely multi-level tree. Each write
/// commits, so the fixture returns with no open transaction.
fn fragmented_volume(files: u32, runs: u64) -> ARXFS<SparseBlock> {
    let device = SparseBlock {
        blocks: BTreeMap::new(),
        block_count: DEVICE_BLOCKS,
    };
    let mut fs = ARXFS::format(device, 64, &TEST_KEY, &mut TestEntropy(0xA1))
        .expect("format the fixture volume");
    let root = fs.root();
    for file in 0..files {
        let name = format!("f{file}");
        fs.create(root, name.as_bytes(), NodeKind::RegularFile)
            .expect("create");
        for run in 0..runs {
            let at = run * 2 * u64::from(BLOCK_SIZE);
            assert_eq!(fs.write_at(root, name.as_bytes(), at, &[0x5A]), Ok(1));
        }
    }
    fs
}

#[test]
fn every_measured_walk_stays_bounded() {
    // One test, because the counters are process-global and `cargo test` runs
    // test functions concurrently: two armed windows at once would measure
    // each other.
    a_stat_holds_a_node_not_a_whole_extent_map();
    a_map_rebuild_walks_every_tree_within_a_fixed_budget();
}

/// `node_info` reports a file's allocated bytes by walking its whole extent
/// map, which makes it the cleanest measurement of the walk itself: four times
/// the extents, and the footprint must not move.
fn a_stat_holds_a_node_not_a_whole_extent_map() {
    let mut small = fragmented_volume(1, 200);
    let mut large = fragmented_volume(1, 800);
    let small_node = small.lookup(small.root(), b"f0").expect("lookup");
    let large_node = large.lookup(large.root(), b"f0").expect("lookup");

    let (small_info, small_peak, small_allocs) = measure(|| small.node_info(small_node));
    let (large_info, large_peak, large_allocs) = measure(|| large.node_info(large_node));
    let small_info = small_info.expect("stat the small file");
    let large_info = large_info.expect("stat the large file");
    assert_eq!(
        large_info.allocated,
        small_info.allocated * 4,
        "the fixture must really hold four times the mapped blocks"
    );

    assert_eq!(
        large_peak, small_peak,
        "a stat's footprint must not grow with the extent count \
         (small {small_peak} bytes, large {large_peak} bytes)"
    );
    assert_eq!(
        large_allocs, small_allocs,
        "a stat must allocate the same number of times whatever the extent \
         count (small {small_allocs}, large {large_allocs})"
    );
    // The walk's one node buffer and the inode record it decoded: a walk that
    // gathered the map first would allocate per record instead.
    assert!(
        large_allocs <= 4,
        "a stat allocates a bounded handful (made {large_allocs})"
    );
    assert!(
        large_peak <= 4 * usize::try_from(BLOCK_SIZE).expect("block size fits"),
        "a stat holds a few nodes at most (peak {large_peak} bytes)"
    );
}

/// Growing onto a longer map region relays the allocation map from the
/// authoritative trees: every node of the inode tree, every node of every
/// file's extent tree, and every run they map. It is the driver's widest
/// metadata walk.
fn a_map_rebuild_walks_every_tree_within_a_fixed_budget() {
    // Sixteen times the records, against a budget fixed by the geometry
    // rather than by the volume: the rebuild holds two node buffers and the
    // allocation map's bounded page cache (64 blocks), and the sparse device
    // double's own per-block bookkeeping is inside the measured window too.
    // A walk that collected a tree would allocate per record — thousands of
    // times over for the larger volume — and blow both budgets by orders of
    // magnitude.
    const PEAK_BUDGET: usize = 64 * 1024;
    const ALLOC_BUDGET: usize = 400;

    for (files, runs) in [(20u32, 20u64), (80, 80)] {
        let fs = fragmented_volume(files, runs);
        let mut device = fs.into_block();
        device.block_count = DEVICE_BLOCKS * 4;
        let mut fs = ARXFS::open(device, &TEST_KEY).expect("reopen on a larger device");
        let (added, peak, allocs) = measure(|| fs.grow());
        assert_eq!(
            added,
            Ok(DEVICE_BLOCKS * 3),
            "the grow must take the whole surplus, so the map region really \
             was relaid from the trees"
        );
        assert!(
            peak <= PEAK_BUDGET,
            "rebuilding the map over {files} files of {runs} runs held {peak} \
             bytes, past the {PEAK_BUDGET}-byte budget"
        );
        assert!(
            allocs <= ALLOC_BUDGET,
            "rebuilding the map over {files} files of {runs} runs allocated \
             {allocs} times, past the {ALLOC_BUDGET} budget"
        );
    }
}
