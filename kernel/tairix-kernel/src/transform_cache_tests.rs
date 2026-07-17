//! Host tests for the `ARXFS` transform cache (`plans/SMARTRAM.md`
//! SMART3): classification, hit/miss accounting, LRU eviction with
//! hysteresis, run-covering invalidation, purge, pressure-band
//! enforcement, bounded admission, secret wiping, and an end-to-end run
//! over a real in-memory `ARXFS` volume.

use super::*;

use alloc::sync::Arc;
use alloc::vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use tairix_abi::driver::block::{Block, BlockGeometry};
use tairix_abi::driver::filesystem::{FilesystemRead, FilesystemWrite, NodeKind};
use tairix_abi::DriverError;
use tairix_drv_fs_arxfs::{EntropySource, VolumeKey, ARXFS, VOLUME_KEY_LEN};
use tairix_kernel_mem::{FreeMemorySource, PressureBand};

/// The test gauge's backing size (1 GiB), so the band watermarks land
/// on readable byte counts.
const TEST_TOTAL: usize = 1 << 30;

/// A controllable memory reading backing a test pressure gauge.
struct TestSource {
    free: AtomicUsize,
}

impl FreeMemorySource for TestSource {
    fn free_bytes(&self) -> usize {
        self.free.load(Ordering::Relaxed)
    }

    fn total_bytes(&self) -> usize {
        TEST_TOTAL
    }
}

/// A gauge plus its adjustable source, starting with `free` bytes free.
fn pressured(free: usize) -> (&'static TestSource, &'static MemoryPressure) {
    let source: &'static TestSource = Box::leak(Box::new(TestSource {
        free: AtomicUsize::new(free),
    }));
    (source, Box::leak(Box::new(MemoryPressure::over(source))))
}

/// A gauge pinned at plentiful free memory: normal pressure.
fn unpressured() -> &'static MemoryPressure {
    pressured(TEST_TOTAL / 2).1
}

/// A free reading that folds to `band` from any shallower state.
fn free_for(band: PressureBand) -> usize {
    match band {
        PressureBand::Normal => TEST_TOTAL / 2,
        PressureBand::Mild => TEST_TOTAL / 5 - 4096,
        PressureBand::Moderate => TEST_TOTAL / 10 - 4096,
        PressureBand::Severe => TEST_TOTAL / 16 - 4096,
        PressureBand::Critical => TEST_TOTAL / 32 - 4096,
    }
}

/// A comfortable test budget: 4 KiB hard limit, 3 KiB low watermark.
fn budget() -> CacheBudget {
    CacheBudget::from_backing(64 * 1024)
}

fn owner() -> ReclaimOwner {
    ReclaimOwner::FilesystemVolume { volume: 7 }
}

/// A leaked capturing sink for the cache's audit records.
fn sink() -> &'static tairix_kernel_core::test_sink::TestSink {
    Box::leak(Box::new(tairix_kernel_core::test_sink::TestSink::new()))
}

fn cache() -> TransformClusterCache {
    TransformClusterCache::new(budget(), owner(), unpressured(), sink())
}

/// One kibibyte of recognisable plaintext.
fn payload(fill: u8) -> alloc::vec::Vec<u8> {
    vec![fill; 1024]
}

#[test]
fn classification_admits_the_declared_candidate_and_charges_the_volume() {
    let cache = cache();
    assert_eq!(cache.owner(), Some(owner()));
}

#[test]
fn a_miss_then_a_put_then_a_hit_account_correctly() {
    let mut cache = cache();
    assert!(cache.get(10).is_none());
    assert_eq!(cache.accounting().misses(), 1);

    cache.put(10, 3, &payload(0xAB));
    assert_eq!(cache.accounting().insertions(), 1);
    assert_eq!(
        cache.accounting().class_bytes(ReclaimClass::TransformCache),
        1024 + ENTRY_OVERHEAD
    );

    let hit = cache.get(10).expect("retained");
    assert_eq!(hit, payload(0xAB).as_slice());
    assert_eq!(cache.accounting().hits(), 1);
}

#[test]
fn eviction_takes_the_least_recently_used_entry_first() {
    // Budget: hard 4096, low 3072. Three 1120-byte entries fit (3360);
    // a fourth forces eviction down past the low watermark, taking the
    // oldest entry.
    let mut cache = cache();
    cache.put(10, 1, &payload(1));
    cache.put(20, 1, &payload(2));
    cache.put(30, 1, &payload(3));
    // Touch the oldest so recency, not insertion order, decides.
    assert!(cache.get(10).is_some());

    cache.put(40, 1, &payload(4));
    assert!(cache.accounting().evictions() >= 1);
    assert!(cache.get(20).is_none(), "the least recently used went");
    assert!(cache.get(10).is_some(), "the touched entry survived");
    assert!(cache.get(40).is_some(), "the new entry was admitted");
    assert!(
        cache.accounting().total_bytes() <= budget().hard(),
        "the ledger respects the hard limit"
    );
}

#[test]
fn oversized_empty_and_shapeless_offers_are_refused() {
    let mut cache = cache();
    // Larger than the whole budget.
    cache.put(10, 16, &vec![0u8; 8192]);
    assert!(cache.get(10).is_none());
    // Larger than any real cluster plaintext.
    cache.put(20, 16, &vec![0u8; MAX_CLUSTER_PLAINTEXT + 1]);
    assert!(cache.get(20).is_none());
    // Empty plaintext and a zero-length stored run carry no coherent
    // identity.
    cache.put(30, 1, &[]);
    cache.put(40, 0, &payload(0));
    assert!(cache.get(30).is_none());
    assert!(cache.get(40).is_none());
    assert!(cache.accounting().refusals() >= 4);
    assert_eq!(cache.accounting().total_bytes(), 0);
}

#[test]
fn invalidation_covers_the_whole_stored_run_and_only_it() {
    let mut cache = cache();
    cache.put(100, 3, &payload(0x11));
    // A free outside the run leaves the entry standing.
    cache.invalidate(99);
    cache.invalidate(103);
    assert!(cache.get(100).is_some());
    // A free of any block inside the run drops it.
    cache.invalidate(102);
    assert!(cache.get(100).is_none());
    assert_eq!(cache.accounting().invalidations(), 1);
    assert_eq!(cache.accounting().total_bytes(), 0);
}

#[test]
fn a_replacement_at_the_same_key_keeps_the_ledger_balanced() {
    let mut cache = cache();
    cache.put(50, 2, &payload(0x22));
    cache.put(50, 2, &vec![0x33u8; 512]);
    assert_eq!(
        cache.accounting().class_bytes(ReclaimClass::TransformCache),
        512 + ENTRY_OVERHEAD
    );
    assert_eq!(
        cache.get(50).expect("retained"),
        vec![0x33u8; 512].as_slice()
    );
}

#[test]
fn purge_drops_everything_and_zeroes_the_ledger() {
    let mut cache = cache();
    cache.put(10, 1, &payload(1));
    cache.put(20, 1, &payload(2));
    cache.purge();
    assert_eq!(cache.accounting().total_bytes(), 0);
    assert!(cache.get(10).is_none());
    assert!(cache.get(20).is_none());
}

#[test]
fn growth_stops_outside_normal_pressure_and_moderate_drains_the_class() {
    let (source, pressure) = pressured(free_for(PressureBand::Normal));
    let mut cache = TransformClusterCache::new(budget(), owner(), pressure, sink());
    cache.put(10, 1, &payload(1));
    assert!(cache.get(10).is_some());

    // Mild pressure: no new growth, but the transform class is
    // preserved (its shrink target stays at the hard limit).
    source
        .free
        .store(free_for(PressureBand::Mild), Ordering::Relaxed);
    cache.put(20, 1, &payload(2));
    assert!(cache.get(20).is_none(), "no growth at mild pressure");
    assert!(cache.get(10).is_some(), "existing entries are preserved");

    // Moderate pressure: the class drains to zero before `ramzip` is
    // handed anything.
    source
        .free
        .store(free_for(PressureBand::Moderate), Ordering::Relaxed);
    assert!(
        cache.get(10).is_none(),
        "moderate pressure drains the class"
    );
    assert_eq!(cache.accounting().total_bytes(), 0);
}

#[test]
fn a_zero_backing_admits_nothing() {
    let mut cache =
        TransformClusterCache::new(CacheBudget::from_backing(0), owner(), unpressured(), sink());
    cache.put(10, 1, &payload(1));
    assert!(cache.get(10).is_none());
    assert!(cache.accounting().refusals() >= 1);
}

#[test]
fn a_forced_pressure_shrink_is_counted() {
    let (source, pressure) = pressured(free_for(PressureBand::Normal));
    let mut cache = TransformClusterCache::new(budget(), owner(), pressure, sink());
    cache.put(10, 1, &payload(1));
    assert_eq!(cache.accounting().pressure_shrinks(), 0);
    source
        .free
        .store(free_for(PressureBand::Moderate), Ordering::Relaxed);
    assert!(cache.get(10).is_none());
    assert_eq!(cache.accounting().pressure_shrinks(), 1);
}

#[test]
fn payload_and_metadata_bytes_are_accounted_separately() {
    let mut cache = cache();
    cache.put(10, 1, &payload(1));
    let acct = cache.accounting();
    let class = ReclaimClass::TransformCache;
    assert_eq!(acct.class_payload_bytes(class), 1024);
    assert_eq!(acct.class_metadata_bytes(class), ENTRY_OVERHEAD);
    assert_eq!(acct.class_bytes(class), 1024 + ENTRY_OVERHEAD);
}

#[test]
fn a_purge_counts_an_owner_teardown_drain() {
    let mut cache = cache();
    cache.put(10, 1, &payload(1));
    assert_eq!(cache.accounting().teardowns(), 0);
    cache.purge();
    assert_eq!(cache.accounting().teardowns(), 1);
    assert_eq!(cache.accounting().total_bytes(), 0);
}

#[test]
fn a_detected_defect_is_counted_and_reported_once() {
    let captured = sink();
    let mut cache = TransformClusterCache::new(budget(), owner(), unpressured(), captured);
    cache.put(10, 1, &payload(1));
    cache.poison("ledger_imbalance");
    assert_eq!(cache.accounting().failures(), 1);
    assert!(cache.accounting().teardowns() >= 1);
    assert_eq!(cache.accounting().total_bytes(), 0);
    let events = captured.snapshot();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].id.0, 2001);
    // The record's field shape is closed — fixed labels and numeric
    // handles only, never plaintext or a block address payload.
    let keys: alloc::vec::Vec<&str> = events[0].fields.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, ["cache", "owner", "owner_id", "cause"]);
    assert_eq!(events[0].fields[0].1, "transform");
    assert_eq!(events[0].fields[1].1, "volume");
    assert_eq!(events[0].fields[2].1, "7");
    assert_eq!(events[0].fields[3].1, "ledger_imbalance");
    // An already-poisoned cache never reports again.
    cache.poison("orphan_index_slot");
    assert_eq!(captured.snapshot().len(), 1);
    assert_eq!(cache.accounting().failures(), 1);
}

#[test]
fn normal_operation_emits_no_audit_records() {
    let captured = sink();
    let mut cache = TransformClusterCache::new(budget(), owner(), unpressured(), captured);
    cache.put(10, 1, &payload(1));
    assert!(cache.get(10).is_some());
    assert!(cache.get(99).is_none());
    cache.invalidate(10);
    cache.purge();
    assert!(captured.snapshot().is_empty());
}

#[test]
fn the_wipe_zeroes_the_plaintext_in_place() {
    let mut plain = payload(0xEE);
    TransformClusterCache::wipe(&mut plain);
    assert_eq!(plain.len(), 1024, "the wipe keeps the length");
    assert!(plain.iter().all(|&b| b == 0), "every byte is wiped");
}

// ---------------------------------------------------------------------------
// End to end: the production cache inside a real ARXFS volume.
// ---------------------------------------------------------------------------

/// An in-memory block device for the end-to-end volume.
struct VecBlock {
    store: alloc::vec::Vec<u8>,
    block_size: u32,
    block_count: u64,
}

impl VecBlock {
    fn new(block_size: u32, block_count: u64) -> Self {
        let len = block_size as usize * usize::try_from(block_count).expect("test geometry");
        Self {
            store: vec![0u8; len],
            block_size,
            block_count,
        }
    }
}

impl Block for VecBlock {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        Ok(BlockGeometry {
            block_size: self.block_size,
            block_count: self.block_count,
        })
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        let start = usize::try_from(lba).map_err(|_| DriverError::LengthOutOfRange)?
            * self.block_size as usize;
        let end = start
            .checked_add(buf.len())
            .ok_or(DriverError::LengthOutOfRange)?;
        let source = self
            .store
            .get(start..end)
            .ok_or(DriverError::LengthOutOfRange)?;
        buf.copy_from_slice(source);
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        let start = usize::try_from(lba).map_err(|_| DriverError::LengthOutOfRange)?
            * self.block_size as usize;
        let end = start
            .checked_add(buf.len())
            .ok_or(DriverError::LengthOutOfRange)?;
        let target = self
            .store
            .get_mut(start..end)
            .ok_or(DriverError::LengthOutOfRange)?;
        target.copy_from_slice(buf);
        Ok(())
    }
}

/// A deterministic stand-in for the platform RNG seam. Test scaffolding
/// only, never a production source.
struct TestEntropy {
    next: u8,
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

/// Observation counters shared with the forwarding wrapper below, so
/// the production cache's behaviour stays observable after it moves
/// into the mounted volume.
#[derive(Default)]
struct SeamCounts {
    hits: AtomicU64,
    invalidations_or_purges: AtomicU64,
}

/// A transparent forwarder around the production cache, counting the
/// driver's calls through the seam.
struct Observed {
    inner: TransformClusterCache,
    counts: Arc<SeamCounts>,
}

impl tairix_drv_fs_arxfs::ClusterCache for Observed {
    fn get(&mut self, phys: u64) -> Option<&[u8]> {
        let served = self.inner.get(phys);
        if served.is_some() {
            self.counts.hits.fetch_add(1, Ordering::Relaxed);
        }
        served
    }

    fn put(&mut self, phys: u64, stored: u64, plaintext: &[u8]) {
        self.inner.put(phys, stored, plaintext);
    }

    fn invalidate(&mut self, phys: u64) {
        let before = self.inner.accounting().invalidations();
        self.inner.invalidate(phys);
        if self.inner.accounting().invalidations() > before {
            self.counts
                .invalidations_or_purges
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    fn purge(&mut self) {
        self.inner.purge();
        self.counts
            .invalidations_or_purges
            .fetch_add(1, Ordering::Relaxed);
    }
}

const TEST_KEY: VolumeKey = [0x5a; VOLUME_KEY_LEN];

/// A formatted in-memory volume with the production cache installed
/// behind the counting forwarder, plus the shared gauge (and its
/// source) and the seam counters.
fn cached_volume() -> (
    &'static TestSource,
    &'static MemoryPressure,
    Arc<SeamCounts>,
    ARXFS<VecBlock>,
) {
    let (source, pressure) = pressured(free_for(PressureBand::Normal));
    let counts = Arc::new(SeamCounts::default());
    let observed = Observed {
        inner: TransformClusterCache::new(
            CacheBudget::from_backing(16 * 1024 * 1024),
            ReclaimOwner::FilesystemVolume {
                volume: 0x524F_4F54,
            },
            pressure,
            sink(),
        ),
        counts: Arc::clone(&counts),
    };
    let fs = ARXFS::format(
        VecBlock::new(512, 256),
        32,
        &TEST_KEY,
        &mut TestEntropy { next: 1 },
    )
    .expect("format")
    .with_cluster_cache(Box::new(observed));
    (source, pressure, counts, fs)
}

/// A whole-cluster payload of repeating (highly compressible) text,
/// sized from the mounted volume's own geometry.
fn compressible_cluster(len: usize) -> alloc::vec::Vec<u8> {
    let mut payload = alloc::vec::Vec::new();
    while payload.len() < len {
        payload.extend_from_slice(b"TAIRiX smartram ");
    }
    payload.truncate(len);
    payload
}

#[test]
fn the_production_cache_serves_and_invalidates_inside_a_real_volume() {
    let (source, _pressure, counts, mut fs) = cached_volume();
    let root = fs.root();
    fs.create(root, b"c", NodeKind::RegularFile)
        .expect("create");
    // A compression cluster on a 512-byte-block volume is 16 blocks of
    // content capacity (strictly under 8 KiB), so 16 KiB of compressible
    // text is guaranteed to cover at least one whole aligned cluster.
    let generous = compressible_cluster(16 * 1024);
    assert_eq!(fs.write_at(root, b"c", 0, &generous), Ok(generous.len()));

    let node = fs.lookup(root, b"c").expect("lookup");
    let mut out = vec![0u8; generous.len()];
    // First pass populates; second pass must be served from the cache.
    for _ in 0..2 {
        let mut done = 0usize;
        while done < out.len() {
            let n = fs
                .read_at(node, u64::try_from(done).expect("offset"), &mut out[done..])
                .expect("read");
            if n == 0 {
                break;
            }
            done += n;
        }
        assert_eq!(out, generous);
    }
    assert!(
        counts.hits.load(Ordering::Relaxed) > 0,
        "the second pass hit the production cache"
    );

    // Overwriting the file frees the superseded stored run: the driver
    // must have invalidated (or purged) through the seam.
    let mut second = generous.clone();
    for byte in &mut second {
        *byte = byte.wrapping_add(1);
    }
    assert_eq!(fs.write_at(root, b"c", 0, &second), Ok(second.len()));
    assert!(
        counts.invalidations_or_purges.load(Ordering::Relaxed) > 0,
        "the mutation reached the cache"
    );
    let mut back = vec![0u8; 512];
    let n = fs
        .read_at(node, 0, &mut back)
        .expect("read after overwrite");
    assert_eq!(&back[..n], &second[..n], "no stale plaintext is served");

    // Moderate pressure drains the class end to end: the next reads
    // stop hitting.
    source
        .free
        .store(free_for(PressureBand::Moderate), Ordering::Relaxed);
    let before = counts.hits.load(Ordering::Relaxed);
    let mut chunk = vec![0u8; 512];
    fs.read_at(node, 0, &mut chunk)
        .expect("read under pressure");
    fs.read_at(node, 0, &mut chunk)
        .expect("read under pressure");
    assert_eq!(
        counts.hits.load(Ordering::Relaxed),
        before,
        "moderate pressure drained the transform cache"
    );
}

#[test]
fn both_cache_layers_share_one_gauge_and_drain_before_the_ramzip_handoff() {
    // The production stack (`plans/SMARTRAM.md` SMART10): the clean
    // filesystem cache wraps the driver whose read path consults the
    // transform cache, both governed by the one boot gauge.
    let (source, pressure, counts, mut fs) = cached_volume();
    let root = fs.root();
    fs.create(root, b"c", NodeKind::RegularFile)
        .expect("create");
    let generous = compressible_cluster(16 * 1024);
    assert_eq!(fs.write_at(root, b"c", 0, &generous), Ok(generous.len()));

    let mut cached = tairix_kernel_core::CachedFs::new(
        fs,
        CacheBudget::from_backing(16 * 1024 * 1024),
        ReclaimOwner::FilesystemVolume {
            volume: 0x524F_4F54,
        },
        pressure,
        sink(),
    );
    let node = cached.lookup(root, b"c").expect("lookup");

    // First small read populates both layers (a driver read that fills
    // the transform cache, a chunk copy in the filesystem cache).
    let mut out = vec![0u8; 512];
    assert_eq!(cached.read_at(node, 0, &mut out), Ok(out.len()));
    assert_eq!(&out[..], &generous[..512]);
    assert!(
        cached.accounting().class_bytes(ReclaimClass::CleanFileData) > 0,
        "the filesystem cache holds the served chunk"
    );

    // A warm repeat is served by the filesystem cache alone: the
    // transform seam sees no further traffic.
    let transform_hits = counts.hits.load(Ordering::Relaxed);
    assert_eq!(cached.read_at(node, 0, &mut out), Ok(out.len()));
    assert_eq!(&out[..], &generous[..512]);
    assert_eq!(
        counts.hits.load(Ordering::Relaxed),
        transform_hits,
        "a filesystem-cache hit never reaches the transform layer"
    );

    // Moderate pressure on the shared gauge drains both layers on
    // their own next operations, and the volume still serves correct
    // bytes straight through the driver's full transform pipeline.
    source
        .free
        .store(free_for(PressureBand::Moderate), Ordering::Relaxed);
    assert_eq!(cached.read_at(node, 0, &mut out), Ok(out.len()));
    assert_eq!(&out[..], &generous[..512]);
    assert_eq!(
        cached.accounting().class_bytes(ReclaimClass::CleanFileData),
        0,
        "moderate pressure drains the clean filesystem cache"
    );
    let drained_hits = counts.hits.load(Ordering::Relaxed);
    assert_eq!(cached.read_at(node, 0, &mut out), Ok(out.len()));
    assert_eq!(
        counts.hits.load(Ordering::Relaxed),
        drained_hits,
        "moderate pressure drained the transform cache below it"
    );
}
