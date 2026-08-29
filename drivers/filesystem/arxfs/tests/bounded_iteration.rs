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
//! Scope: the read paths whose own work *is* the walk, a single-record insert
//! and remove — the mutation path edits nodes in place through one borrowed
//! scratch, so it too costs the same at any depth — and a whole-volume scrub
//! and check, whose derived truth streams through on-disk scratch arrays rather
//! than RAM. What is still not bounded is the transaction's own freed-block set
//! and the dirty blocks it stages until its commit drains them: a pass that
//! frees or dirties *many* records holds one entry per block, which is the
//! write-back plan's ceiling to derive (`plans/ARXFS-WRITEBACK.md` WB5).

use std::alloc::System;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use core::alloc::{GlobalAlloc, Layout};

use tairix_abi::driver::block::{Block, BlockGeometry, DiscardCapability};
use tairix_abi::driver::filesystem::{FilesystemRead, FilesystemWrite, NodeKind};
use tairix_abi::{CapabilityId, CapabilityQuery, DriverError};
use tairix_drv_fs_arxfs::{
    EntropySource, PassVerdict, ScrubBudget, VolumeKey, ARXFS, RUN_BYTES, VOLUME_KEY_LEN,
};
use tairix_log::{Event, Sink};

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

/// Grants every capability: these measurements are about footprint, not the
/// capability gate the driver's own tests cover.
struct GrantAll;

impl CapabilityQuery for GrantAll {
    fn holds(&self, _id: CapabilityId) -> bool {
        true
    }
}

/// Discards the findings: the reports are asserted directly.
struct NullSink;

impl Sink for NullSink {
    fn write_event(&self, _event: &Event<'_>) {}
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
    a_single_record_edit_allocates_the_same_at_any_depth();
    a_whole_volume_verification_holds_no_per_record_state();
}

/// A scrub and an offline check over sixteen times the records hold the same
/// bounded footprint: the truth they derive about every block and every inode
/// lives in transient on-disk scratch arrays, not in RAM.
///
/// This is the measurement the reconcile was rebuilt for. The path that
/// recomputed refcounts into a `BTreeMap<u64, Vec<Referrer>>` keyed by every
/// physical data block held one entry per block *before* it reconciled
/// anything — around ninety bytes each with the map node and the referrer
/// vector, so a hundred-terabyte volume asked for something no machine could
/// give it. Sixteen times the records here took sixteen times the bytes with
/// it, and would blow the budget below by a wide margin.
fn a_whole_volume_verification_holds_no_per_record_state() {
    // Fixed by the geometry, not the volume: the walks' node buffers, the two
    // bounded page caches (the allocation map's and a scratch array's, 64
    // pages each), and the scratch arrays' own inode-space pages. The old
    // per-block accumulator would have wanted some 576 KiB for the larger
    // fixture alone.
    const PEAK_BUDGET: usize = 128 * 1024;

    let mut verification = Vec::new();
    for (files, runs) in [(20u32, 20u64), (80, 80)] {
        let mut fs = fragmented_volume(files, runs);
        // Warm the device double first: a pass writes its scratch run, and the
        // double stores every block written to it. That storage is the
        // harness's, not the driver's footprint, and it is charged once —
        // measuring the second pass leaves only what the driver itself holds.
        fs.scrub(&GrantAll, &NullSink, ScrubBudget::Unlimited)
            .expect("warm the device double");
        fs.check(&GrantAll, &NullSink)
            .expect("warm the device double");

        let (report, scrub_peak, _) =
            measure(|| fs.scrub(&GrantAll, &NullSink, ScrubBudget::Unlimited));
        let report = report.expect("scrub the fixture");
        assert_eq!(report.pass, PassVerdict::Complete, "{report:?}");
        assert!(
            report.claims_counted,
            "the claim counts were recomputed: {report:?}"
        );
        assert!(
            report.data_blocks_checked >= u64::from(files) * runs,
            "the fixture must really hold the records: {report:?}"
        );

        let (check, check_peak, _) = measure(|| fs.check(&GrantAll, &NullSink));
        let check = check.expect("check the fixture");
        assert!(check.complete && check.directories_checked > 0, "{check:?}");

        for (what, peak) in [("scrub", scrub_peak), ("check", check_peak)] {
            assert!(
                peak <= PEAK_BUDGET,
                "a {what} over {files} files of {runs} runs held {peak} bytes, \
                 past the {PEAK_BUDGET}-byte budget"
            );
        }
        verification.push(scrub_peak);
    }

    // Sharper than the budget: the shared verification core — the metadata and
    // data walks and the whole refcount reconcile — must hold the *same* bytes
    // at sixteen times the records, not merely stay under a ceiling. (An
    // offline check adds the free-space rebuild, whose own footprint is the
    // map's bounded page cache filling up, so it is held to the budget above
    // rather than to flatness.)
    let (small, large) = (verification[0], verification[1]);
    assert!(
        large <= small * 2,
        "a scrub over sixteen times the records held {large} bytes against \
         {small}: the verification core is tracking the record count"
    );
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
    // rather than by the volume: the rebuild holds two node buffers, the
    // allocation map's bounded page cache (64 blocks), and the commit drain's
    // gather window, and the sparse device double's own per-block bookkeeping
    // is inside the measured window too. A walk that collected a tree would
    // allocate per record — thousands of times over for the larger volume —
    // and blow both budgets by orders of magnitude.
    const PEAK_BUDGET: usize = 64 * 1024;
    const ALLOC_BUDGET: usize = 400;

    for (files, runs) in [(20u32, 20u64), (80, 80)] {
        let fs = fragmented_volume(files, runs);
        let mut device = fs.into_block().expect("the volume closes");
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

/// One extent added and one dropped, on volumes whose extent trees hold forty
/// times the records apart: the edit works in place in the node buffers of the
/// scratch the mount lends it, so what it allocates is the transaction's own
/// bookkeeping and the descent's depth — nothing per record.
///
/// The path that decoded each node's entries into a vector first allocated
/// twice per record, per node, per level: measured on the larger volume here,
/// **596** allocations for one removed extent against **118** now, and rising
/// with the block size as wider nodes hold more records.
fn a_single_record_edit_allocates_the_same_at_any_depth() {
    // Fixed by the geometry rather than by the volume. The allocation budget
    // sits between what the per-record decode cost and what an in-place edit
    // costs, so a reintroduced decode fails it. The byte figure is the sparse
    // device double's own per-block storage for the blocks the copy-on-write
    // rewrote, which is inside the measured window, plus the commit drain's
    // gather window — one buffer sized to the transaction's longest physical
    // run and never past the transfer bound. It is here to catch an operation
    // that suddenly holds far more, not to bound the driver to the byte.
    const ALLOC_BUDGET: usize = 160;
    const PEAK_BUDGET: usize = 24 * 1024 + RUN_BYTES;
    let stride = 2 * u64::from(BLOCK_SIZE);

    for runs in [20u64, 800] {
        let mut fs = fragmented_volume(1, runs);
        let root = fs.root();

        // An insert that descends the whole tree and may split on the way up.
        let (wrote, peak, allocs) = measure(|| fs.write_at(root, b"f0", runs * stride, &[0x5A]));
        assert_eq!(wrote, Ok(1));
        assert!(
            allocs <= ALLOC_BUDGET && peak <= PEAK_BUDGET,
            "inserting one extent into a {runs}-extent tree allocated {allocs} \
             times and held {peak} bytes, past the {ALLOC_BUDGET} / \
             {PEAK_BUDGET} budgets"
        );

        // A remove of exactly one record, which is where borrow-or-merge lives.
        let (cut, peak, allocs) = measure(|| fs.truncate(root, b"f0", (runs - 1) * stride));
        assert_eq!(cut, Ok(()));
        assert!(
            allocs <= ALLOC_BUDGET && peak <= PEAK_BUDGET,
            "removing one extent from a {runs}-extent tree allocated {allocs} \
             times and held {peak} bytes, past the {ALLOC_BUDGET} / \
             {PEAK_BUDGET} budgets"
        );
    }
}
