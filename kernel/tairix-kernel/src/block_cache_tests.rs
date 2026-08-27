//! Host tests for the whole-disk block cache (`plans/SMARTRAM.md`
//! SMART11): classification, hit/miss accounting, write-through
//! coherence, discard and failed-write invalidation, sensitive-class
//! scrubbing, budget-bounded admission of a wide read, LRU eviction
//! with hysteresis, pressure-band enforcement, fail-closed poisoning,
//! and secret wiping.

use super::*;

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicUsize, Ordering};

use tairix_reclaim::{FreeMemorySource, PressureBand};

/// The test gauge's backing size (1 GiB), so the band watermarks land
/// on readable byte counts.
const TEST_TOTAL: usize = 1 << 30;

/// A controllable memory reading backing a test pressure gauge, counting
/// how often it was read: in production this reading is the physical
/// frame allocator's free-frame count, so every read takes the global
/// frame-allocator lock and the *count per request* is the cost the
/// cache is held to.
struct TestSource {
    free: AtomicUsize,
    readings: AtomicUsize,
}

impl TestSource {
    /// How often the gauge has read this source.
    fn readings(&self) -> usize {
        self.readings.load(Ordering::Relaxed)
    }
}

impl FreeMemorySource for TestSource {
    fn free_bytes(&self) -> usize {
        self.readings.fetch_add(1, Ordering::Relaxed);
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
        readings: AtomicUsize::new(0),
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

/// A leaked capturing sink for the cache's audit records.
fn sink() -> &'static tairix_kernel_core::test_sink::TestSink {
    Box::leak(Box::new(tairix_kernel_core::test_sink::TestSink::new()))
}

/// The test device block size: 512-byte blocks, so one cached block
/// costs `512 + ENTRY_OVERHEAD` ledger bytes.
const BS: u32 = 512;

/// One cached block's ledger cost against the 4 KiB test budget.
const COST: usize = BS as usize + ENTRY_OVERHEAD;

/// The shared backing a test disk and its observer both hold: the byte
/// store plus operation counters, so a test can count device reads and
/// corrupt the underlying bytes to prove a hit never reaches the
/// device.
struct Store {
    data: Vec<u8>,
    reads: u64,
    /// Bytes the device was asked for, so a test can tell a wider
    /// speculative read from the same *number* of narrower ones.
    bytes_read: usize,
    writes: u64,
    flushes: u64,
    discards: Vec<(u64, u64)>,
    fail_writes: bool,
}

/// An in-memory whole-disk device over a shared [`Store`].
struct MemDisk {
    store: Rc<RefCell<Store>>,
    geo: BlockGeometry,
}

impl MemDisk {
    fn with_geometry(block_size: u32, block_count: u64) -> (Rc<RefCell<Store>>, Self) {
        let len = block_size as usize * usize::try_from(block_count).expect("test geometry");
        let store = Rc::new(RefCell::new(Store {
            data: (0..len)
                .map(|i| u8::try_from(i % 251).expect("bounded by the modulus"))
                .collect(),
            reads: 0,
            bytes_read: 0,
            writes: 0,
            flushes: 0,
            discards: Vec::new(),
            fail_writes: false,
        }));
        let disk = Self {
            store: Rc::clone(&store),
            geo: BlockGeometry {
                block_size,
                block_count,
            },
        };
        (store, disk)
    }

    fn new() -> (Rc<RefCell<Store>>, Self) {
        Self::with_geometry(BS, 128)
    }

    fn span(&self, lba: u64, len: usize) -> Result<usize, DriverError> {
        let bs = self.geo.block_size as usize;
        if len == 0 || !len.is_multiple_of(bs) {
            return Err(DriverError::BufferTooSmall);
        }
        let blocks = (len / bs) as u64;
        if lba.saturating_add(blocks) > self.geo.block_count {
            return Err(DriverError::LengthOutOfRange);
        }
        let start = usize::try_from(lba).map_err(|_| DriverError::LengthOutOfRange)? * bs;
        Ok(start)
    }
}

impl Block for MemDisk {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        Ok(self.geo)
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        let start = self.span(lba, buf.len())?;
        let mut store = self.store.borrow_mut();
        store.reads += 1;
        store.bytes_read += buf.len();
        buf.copy_from_slice(&store.data[start..start + buf.len()]);
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        let start = self.span(lba, buf.len())?;
        let mut store = self.store.borrow_mut();
        if store.fail_writes {
            return Err(DriverError::DeviceFault);
        }
        store.writes += 1;
        let at = start;
        store.data[at..at + buf.len()].copy_from_slice(buf);
        Ok(())
    }

    fn discard_capability(&self) -> Result<DiscardCapability, DriverError> {
        Ok(DiscardCapability {
            supported: true,
            granularity_blocks: 1,
            max_blocks_per_request: 0,
        })
    }

    fn discard(&mut self, lba: u64, blocks: u64) -> Result<(), DriverError> {
        if lba.saturating_add(blocks) > self.geo.block_count {
            return Err(DriverError::LengthOutOfRange);
        }
        self.store.borrow_mut().discards.push((lba, blocks));
        Ok(())
    }

    fn device_health(&self) -> Result<DeviceHealth, DriverError> {
        Ok(DeviceHealth::Unavailable)
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        self.store.borrow_mut().flushes += 1;
        Ok(())
    }
}

/// A cache over a fresh test disk at normal pressure, plus the shared
/// store observer.
fn cached() -> (Rc<RefCell<Store>>, BlockCache<MemDisk>) {
    let (store, disk) = MemDisk::new();
    let cache = BlockCache::new(disk, budget(), unpressured(), sink()).expect("geometry queried");
    (store, cache)
}

/// One block of recognisable bytes.
fn block_of(fill: u8) -> Vec<u8> {
    vec![fill; BS as usize]
}

/// A budget large enough to retain a full readahead window (and then
/// some), so a readahead test observes the coalescing effect rather
/// than the eviction gate the small [`budget`] exercises elsewhere.
fn roomy_budget() -> CacheBudget {
    CacheBudget::from_backing(4 * 1024 * 1024)
}

/// A cache over a 256-block test disk with a roomy budget: the fixture
/// the readahead tests stream a long sequential run through.
fn roomy_cached() -> (Rc<RefCell<Store>>, BlockCache<MemDisk>) {
    let (store, disk) = MemDisk::with_geometry(BS, 256);
    let cache =
        BlockCache::new(disk, roomy_budget(), unpressured(), sink()).expect("geometry queried");
    (store, cache)
}

#[test]
fn classification_admits_and_charges_the_block_device_subsystem() {
    let (_store, cache) = cached();
    assert_eq!(
        cache.owner(),
        Some(ReclaimOwner::KernelSubsystem(OWNER_SUBSYSTEM))
    );
}

#[test]
fn a_miss_reads_the_device_and_a_hit_never_does() {
    let (store, mut cache) = cached();
    let mut first = block_of(0);
    cache.read_blocks(3, &mut first).unwrap();
    assert_eq!(store.borrow().reads, 1);
    assert_eq!(cache.accounting().misses(), 1);

    // Corrupt the underlying store: a hit must still serve the bytes
    // the cache retained, proving the device is never touched.
    store.borrow_mut().data[3 * BS as usize] ^= 0xFF;
    let mut second = block_of(0);
    cache.read_blocks(3, &mut second).unwrap();
    assert_eq!(store.borrow().reads, 1, "the hit issued no device read");
    assert_eq!(second, first);
    assert_eq!(cache.accounting().hits(), 1);
    assert_eq!(
        cache.accounting().class_bytes(ReclaimClass::CleanFileData),
        COST
    );
}

#[test]
fn a_multi_block_read_is_served_only_when_fully_cached() {
    let (store, mut cache) = cached();
    let mut one = block_of(0);
    cache.read_blocks(4, &mut one).unwrap();
    assert_eq!(store.borrow().reads, 1);

    // Blocks 4..6: block 5 is uncached, so the whole span re-reads the
    // device (one call) and both blocks are then retained.
    let mut two = vec![0u8; 2 * BS as usize];
    cache.read_blocks(4, &mut two).unwrap();
    assert_eq!(store.borrow().reads, 2);

    // Now fully cached: the same span is a single hit.
    let mut three = vec![0u8; 2 * BS as usize];
    cache.read_blocks(4, &mut three).unwrap();
    assert_eq!(store.borrow().reads, 2, "the full span hit issued no read");
    assert_eq!(three, two);
}

#[test]
fn a_write_through_updates_the_device_and_the_cached_copy() {
    let (store, mut cache) = cached();
    let mut old = block_of(0);
    cache.read_blocks(7, &mut old).unwrap();

    let fresh = block_of(0x5A);
    cache.write_blocks(7, &fresh).unwrap();
    assert_eq!(store.borrow().writes, 1, "the write reached the device");
    let at = 7 * BS as usize;
    assert_eq!(&store.borrow().data[at..at + BS as usize], &fresh[..]);

    // The cached copy was refreshed in place: the read-after-write is
    // served from cache with the new bytes.
    let reads_before = store.borrow().reads;
    let mut back = block_of(0);
    cache.read_blocks(7, &mut back).unwrap();
    assert_eq!(store.borrow().reads, reads_before);
    assert_eq!(back, fresh);
}

#[test]
fn a_write_admits_nothing_new() {
    let (_store, mut cache) = cached();
    cache.write_blocks(9, &block_of(0x11)).unwrap();
    assert_eq!(
        cache.accounting().total_bytes(),
        0,
        "a write is not a read prediction"
    );
}

#[test]
fn a_failed_write_invalidates_the_range() {
    let (store, mut cache) = cached();
    let mut old = block_of(0);
    cache.read_blocks(7, &mut old).unwrap();

    store.borrow_mut().fail_writes = true;
    assert_eq!(
        cache.write_blocks(7, &block_of(0x77)),
        Err(DriverError::DeviceFault)
    );
    assert_eq!(cache.accounting().invalidations(), 1);
    store.borrow_mut().fail_writes = false;

    // The next read goes back to the device: the cache no longer
    // vouches for a range whose device state is unknown.
    let reads_before = store.borrow().reads;
    let mut back = block_of(0);
    cache.read_blocks(7, &mut back).unwrap();
    assert_eq!(store.borrow().reads, reads_before + 1);
}

#[test]
fn a_discard_invalidates_its_range_and_is_forwarded() {
    let (store, mut cache) = cached();
    let mut buf = vec![0u8; 2 * BS as usize];
    cache.read_blocks(10, &mut buf).unwrap();
    assert_eq!(cache.accounting().total_bytes(), 2 * COST);

    cache.discard(10, 2).unwrap();
    assert_eq!(store.borrow().discards.as_slice(), &[(10, 2)]);
    assert_eq!(cache.accounting().invalidations(), 2);
    assert_eq!(cache.accounting().total_bytes(), 0);
}

#[test]
fn a_sensitive_read_bypasses_the_cache_and_scrubs_the_range() {
    let (store, mut cache) = cached();
    let mut plain = block_of(0);
    cache.read_blocks(5, &mut plain).unwrap();
    assert_eq!(cache.accounting().total_bytes(), COST);

    // The sensitive read reaches the device and evicts the cached
    // copy: no block a key slot lives in is ever retained.
    let mut secret = block_of(0);
    cache
        .read_blocks_with_class(5, &mut secret, BufferClass::Sensitive)
        .unwrap();
    assert_eq!(store.borrow().reads, 2);
    assert_eq!(cache.accounting().invalidations(), 1);
    assert_eq!(cache.accounting().total_bytes(), 0);

    // And it admitted nothing: the next plain read is a device read.
    let mut again = block_of(0);
    cache.read_blocks(5, &mut again).unwrap();
    assert_eq!(store.borrow().reads, 3);
}

#[test]
fn a_sensitive_write_is_never_retained() {
    let (store, mut cache) = cached();
    let mut plain = block_of(0);
    cache.read_blocks(6, &mut plain).unwrap();

    cache
        .write_blocks_with_class(6, &block_of(0x99), BufferClass::Sensitive)
        .unwrap();
    assert_eq!(store.borrow().writes, 1, "the write reached the device");
    assert_eq!(cache.accounting().total_bytes(), 0, "no copy is retained");

    // The next read is served from the device, seeing the new bytes.
    let mut back = block_of(0);
    cache.read_blocks(6, &mut back).unwrap();
    assert_eq!(back, block_of(0x99));
}

#[test]
fn a_non_sensitive_classified_read_is_cached_like_a_plain_one() {
    let (store, mut cache) = cached();
    let mut buf = block_of(0);
    cache
        .read_blocks_with_class(8, &mut buf, BufferClass::NonSensitive)
        .unwrap();
    assert_eq!(cache.accounting().total_bytes(), COST);
    let mut again = block_of(0);
    cache
        .read_blocks_with_class(8, &mut again, BufferClass::NonSensitive)
        .unwrap();
    assert_eq!(store.borrow().reads, 1, "the second read was a hit");
}

#[test]
fn a_read_the_budget_could_not_hold_streams_through_uncached() {
    // The only size rule left: a request wider than the watermark the
    // cache evicts down to would flush everything else to hold itself,
    // so it streams through instead. The bound follows the budget.
    let (store, mut cache) = cached();
    let blocks = budget().low() / BS as usize + 1;
    let mut bulk = vec![0u8; blocks * BS as usize];
    cache.read_blocks(0, &mut bulk).unwrap();
    assert_eq!(store.borrow().reads, 1);
    assert_eq!(
        cache.accounting().total_bytes(),
        0,
        "a load the budget cannot hold must not flush the working set"
    );
}

#[test]
fn a_wide_read_the_budget_holds_is_retained_and_repeats_from_the_cache() {
    // The regression: a filesystem that coalesces its reads into whole
    // contiguous runs (`tairix-drv-fs-arxfs`'s 64 KiB run window) is
    // exactly the caller whose repeat must come from RAM. Request size
    // decides how a miss is fetched, never whether the bytes are kept.
    let (store, mut cache) = roomy_cached();
    let blocks = 64 * 1024 / BS as usize;
    let mut run = vec![0u8; blocks * BS as usize];
    cache.read_blocks(0, &mut run).unwrap();
    assert_eq!(
        store.borrow().reads,
        1,
        "one device request fetched the whole run"
    );
    assert_eq!(
        cache.accounting().total_bytes(),
        blocks * COST,
        "every block of the run is retained"
    );

    let mut again = vec![0u8; blocks * BS as usize];
    cache.read_blocks(0, &mut again).unwrap();
    assert_eq!(
        store.borrow().reads,
        1,
        "the repeated run never reached the device"
    );
    assert_eq!(again, run, "and answered exactly the same bytes");
}

#[test]
fn one_request_takes_one_pressure_reading_however_many_blocks_it_retains() {
    // The regression: `populate` asked the gauge per *device block*, and
    // the gauge's reading is the physical frame allocator's free-frame
    // count — so a 128-block coalesced read took the global
    // frame-allocator lock 257 times (one per `enforce_pressure`, two per
    // block for the band and the reserve test) to answer a question that
    // cannot change inside one request. One reading per request, drawn
    // down per block, is both cheaper and the stricter bound.
    let (source, pressure) = pressured(TEST_TOTAL / 2);
    let (store, disk) = MemDisk::with_geometry(BS, 256);
    let mut cache =
        BlockCache::new(disk, roomy_budget(), pressure, sink()).expect("geometry queried");

    let blocks = 128;
    let before = source.readings();
    let mut run = vec![0u8; blocks * BS as usize];
    cache.read_blocks(0, &mut run).unwrap();
    assert_eq!(store.borrow().reads, 1, "one device request for the run");
    assert_eq!(
        cache.accounting().total_bytes(),
        blocks * COST,
        "every block of the run is retained"
    );
    assert_eq!(
        source.readings() - before,
        1,
        "the whole run is admitted from one gauge reading"
    );

    // A hit costs the same one reading, and a write — which admits
    // nothing — costs one too.
    let hit = source.readings();
    cache.read_blocks(0, &mut run).unwrap();
    assert_eq!(source.readings() - hit, 1, "a served hit reads once");
    let write = source.readings();
    cache.write_blocks(0, &run).unwrap();
    assert_eq!(source.readings() - write, 1, "a write reads once");
}

#[test]
fn a_cache_that_can_retain_nothing_does_not_speculate() {
    // Readahead exists so the *next* read comes from RAM. With nothing
    // admissible — here the operator has switched the block class off — the
    // prefetched blocks are discarded as fast as they arrive, so the wider
    // device read is pure waste and must not be issued.
    let control: &'static CacheControl = Box::leak(Box::new(CacheControl::new()));
    let (store, disk) = MemDisk::with_geometry(BS, 256);
    let mut cache = BlockCache::new(disk, roomy_budget(), unpressured(), sink())
        .expect("geometry queried")
        .with_cache_control(control);

    // Enabled: a sequential pair of single-block reads opens a window, so
    // the second read fetches more than the one block it was asked for.
    let mut one = block_of(0);
    cache.read_blocks(0, &mut one).unwrap();
    cache.read_blocks(1, &mut one).unwrap();
    let admitted = cache.accounting().total_bytes();
    assert!(
        admitted > 2 * COST,
        "the sequential second read prefetched a window"
    );

    // Off: the same sequential pattern must read exactly what was asked.
    control.set(CacheClass::Block, tairix_kernel_core::CacheMode::Off);
    let before = store.borrow().bytes_read;
    cache.read_blocks(10, &mut one).unwrap();
    cache.read_blocks(11, &mut one).unwrap();
    assert_eq!(
        store.borrow().bytes_read - before,
        2 * BS as usize,
        "a cache that retains nothing reads exactly the requested blocks"
    );
    assert_eq!(cache.accounting().total_bytes(), 0);
}

#[test]
fn an_unaligned_request_takes_the_device_error_surface() {
    let (_store, mut cache) = cached();
    let mut tiny = [0u8; 8];
    assert_eq!(
        cache.read_blocks(0, &mut tiny),
        Err(DriverError::BufferTooSmall)
    );
    assert_eq!(cache.accounting().total_bytes(), 0);
}

#[test]
fn eviction_takes_the_least_recently_used_block_first() {
    // Six 608-byte blocks fit under the 4096 hard limit (3648); a
    // seventh evicts down past the 3072 low watermark, taking the
    // oldest untouched block. The LBAs are spaced apart so this tests
    // pure LRU eviction, never the sequential-readahead path (which a
    // 0,1,2,… run would arm).
    let (store, mut cache) = cached();
    for lba in [0u64, 2, 4, 6, 8, 10] {
        let mut buf = block_of(0);
        cache.read_blocks(lba, &mut buf).unwrap();
    }
    // Touch block 0 so recency, not insertion order, decides.
    let mut touch = block_of(0);
    cache.read_blocks(0, &mut touch).unwrap();

    let mut seventh = block_of(0);
    cache.read_blocks(12, &mut seventh).unwrap();
    assert!(cache.accounting().evictions() >= 1);
    assert!(
        cache.accounting().total_bytes() <= budget().hard(),
        "the ledger respects the hard limit"
    );

    // Block 2 (the least recently used) went; block 0 survived.
    let reads_before = store.borrow().reads;
    let mut hit = block_of(0);
    cache.read_blocks(0, &mut hit).unwrap();
    assert_eq!(store.borrow().reads, reads_before, "the touched block held");
    let mut miss = block_of(0);
    cache.read_blocks(2, &mut miss).unwrap();
    assert_eq!(
        store.borrow().reads,
        reads_before + 1,
        "the least recently used block was evicted"
    );
}

#[test]
fn growth_follows_the_band_ceiling_and_moderate_drains_the_class() {
    let (gauge_source, pressure) = pressured(free_for(PressureBand::Normal));
    let (store, disk) = MemDisk::new();
    let mut cache = BlockCache::new(disk, budget(), pressure, sink()).unwrap();
    let mut buf = block_of(0);
    cache.read_blocks(0, &mut buf).unwrap();
    assert_eq!(cache.accounting().total_bytes(), COST);

    // Mild pressure begins reclaiming clean file data rather than
    // stopping it: the class may hold its low watermark, so a read under
    // that watermark is still cached and the resident block held.
    gauge_source
        .free
        .store(free_for(PressureBand::Mild), Ordering::Relaxed);
    // Block 2 (not the adjacent block 1) so the read is random, not a
    // sequential continuation that would arm readahead.
    let mut other = block_of(0);
    cache.read_blocks(2, &mut other).unwrap();
    assert_eq!(cache.accounting().refusals(), 0, "growth to the watermark");
    assert!(cache.accounting().total_bytes() <= budget().low());
    let reads_before = store.borrow().reads;
    cache.read_blocks(0, &mut buf).unwrap();
    assert_eq!(
        store.borrow().reads,
        reads_before,
        "the resident block held"
    );

    // Moderate pressure: the clean-file class drains to zero before
    // `ramzip` is handed anything, while the device keeps serving.
    gauge_source
        .free
        .store(free_for(PressureBand::Moderate), Ordering::Relaxed);
    cache.read_blocks(0, &mut buf).unwrap();
    assert_eq!(cache.accounting().total_bytes(), 0);
    assert!(cache.accounting().pressure_shrinks() >= 1);
    // A drained class admits nothing there either.
    cache.read_blocks(4, &mut other).unwrap();
    assert_eq!(cache.accounting().total_bytes(), 0);

    // Recovery: normal pressure admits growth again.
    gauge_source
        .free
        .store(free_for(PressureBand::Normal), Ordering::Relaxed);
    cache.read_blocks(0, &mut buf).unwrap();
    assert_eq!(cache.accounting().total_bytes(), COST);
}

#[test]
fn mild_pressure_shrinks_a_full_cache_to_the_low_watermark() {
    let (gauge_source, pressure) = pressured(free_for(PressureBand::Normal));
    let (_store, disk) = MemDisk::new();
    let mut cache = BlockCache::new(disk, budget(), pressure, sink()).unwrap();
    // Spaced LBAs: fill the cache without arming sequential readahead.
    for lba in [0u64, 2, 4, 6, 8, 10] {
        let mut buf = block_of(0);
        cache.read_blocks(lba, &mut buf).unwrap();
    }
    assert!(cache.accounting().total_bytes() > budget().low());

    gauge_source
        .free
        .store(free_for(PressureBand::Mild), Ordering::Relaxed);
    let mut buf = block_of(0);
    cache.read_blocks(0, &mut buf).unwrap();
    assert!(
        cache.accounting().total_bytes() <= budget().low(),
        "mild pressure shrinks the class to the low watermark"
    );
    assert!(cache.accounting().pressure_shrinks() >= 1);
}

#[test]
fn a_zero_backing_admits_nothing_but_still_serves() {
    let (store, disk) = MemDisk::new();
    let mut cache =
        BlockCache::new(disk, CacheBudget::from_backing(0), unpressured(), sink()).unwrap();
    let mut buf = block_of(0);
    cache.read_blocks(2, &mut buf).unwrap();
    assert_eq!(cache.accounting().total_bytes(), 0);
    assert!(cache.accounting().refusals() >= 1);
    let at = 2 * BS as usize;
    assert_eq!(&buf[..], &store.borrow().data[at..at + BS as usize]);
}

#[test]
fn an_uncacheable_geometry_poisons_but_the_device_still_serves() {
    let captured = sink();
    let (store, disk) = MemDisk::with_geometry(8192, 8);
    let mut cache = BlockCache::new(disk, budget(), unpressured(), captured).unwrap();
    let mut buf = vec![0u8; 8192];
    cache.read_blocks(1, &mut buf).unwrap();
    assert_eq!(store.borrow().reads, 1);
    assert_eq!(cache.accounting().total_bytes(), 0);
    let events = captured.snapshot();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].id.0, 2001);
    assert_eq!(events[0].fields[3].1, "uncacheable_geometry");
}

#[test]
fn a_geometry_fault_refuses_to_wrap_the_device() {
    struct FaultyGeometry;
    impl Block for FaultyGeometry {
        fn geometry(&self) -> Result<BlockGeometry, DriverError> {
            Err(DriverError::DeviceFault)
        }
        fn read_blocks(&mut self, _: u64, _: &mut [u8]) -> Result<(), DriverError> {
            Err(DriverError::Unsupported)
        }
        fn write_blocks(&mut self, _: u64, _: &[u8]) -> Result<(), DriverError> {
            Err(DriverError::Unsupported)
        }
        fn flush(&mut self) -> Result<(), DriverError> {
            Err(DriverError::Unsupported)
        }
    }
    assert_eq!(
        BlockCache::new(FaultyGeometry, budget(), unpressured(), sink()).err(),
        Some(DriverError::DeviceFault)
    );
}

#[test]
fn flush_is_forwarded_to_the_device() {
    // The cache is write-through, so a flush is a pure durability barrier:
    // it must reach the device, never be swallowed by the wrapper.
    let (store, mut cache) = cached();
    assert_eq!(store.borrow().flushes, 0);
    cache.flush().expect("flush");
    assert_eq!(
        store.borrow().flushes,
        1,
        "the block cache forwards flush to the backing device"
    );
}

#[test]
fn geometry_capability_and_health_are_forwarded() {
    let (_store, cache) = cached();
    let geo = Block::geometry(&cache).unwrap();
    assert_eq!(geo.block_size, BS);
    assert_eq!(geo.block_count, 128);
    assert!(cache.discard_capability().unwrap().supported);
    assert!(matches!(
        cache.device_health().unwrap(),
        DeviceHealth::Unavailable
    ));
}

#[test]
fn payload_and_metadata_bytes_are_accounted_separately() {
    let (_store, mut cache) = cached();
    let mut buf = block_of(0);
    cache.read_blocks(0, &mut buf).unwrap();
    let acct = cache.accounting();
    let class = ReclaimClass::CleanFileData;
    assert_eq!(acct.class_payload_bytes(class), BS as usize);
    assert_eq!(acct.class_metadata_bytes(class), ENTRY_OVERHEAD);
    assert_eq!(acct.class_bytes(class), COST);
}

#[test]
fn a_detected_defect_is_counted_and_reported_once() {
    let captured = sink();
    let (store, disk) = MemDisk::new();
    let mut cache = BlockCache::new(disk, budget(), unpressured(), captured).unwrap();
    let mut buf = block_of(0);
    cache.read_blocks(0, &mut buf).unwrap();
    cache.poison("ledger_imbalance");
    assert_eq!(cache.accounting().failures(), 1);
    assert!(cache.accounting().teardowns() >= 1);
    assert_eq!(cache.accounting().total_bytes(), 0);
    let events = captured.snapshot();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].id.0, 2001);
    // The record's field shape is closed — fixed labels and numeric
    // handles only, never plaintext or a block address payload.
    let keys: Vec<&str> = events[0].fields.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, ["cache", "owner", "owner_id", "cause"]);
    assert_eq!(events[0].fields[0].1, "block");
    assert_eq!(events[0].fields[1].1, OWNER_SUBSYSTEM);
    assert_eq!(events[0].fields[2].1, "0");
    assert_eq!(events[0].fields[3].1, "ledger_imbalance");
    // An already-poisoned cache never reports again, and still serves.
    cache.poison("orphan_index_slot");
    assert_eq!(captured.snapshot().len(), 1);
    let reads_before = store.borrow().reads;
    cache.read_blocks(0, &mut buf).unwrap();
    assert_eq!(store.borrow().reads, reads_before + 1);
    assert_eq!(cache.accounting().total_bytes(), 0);
}

#[test]
fn normal_operation_emits_no_audit_records() {
    let captured = sink();
    let (_store, disk) = MemDisk::new();
    let mut cache = BlockCache::new(disk, budget(), unpressured(), captured).unwrap();
    let mut buf = block_of(0);
    cache.read_blocks(0, &mut buf).unwrap();
    cache.read_blocks(0, &mut buf).unwrap();
    cache.write_blocks(0, &block_of(1)).unwrap();
    cache.discard(0, 1).unwrap();
    assert!(captured.snapshot().is_empty());
}

#[test]
fn the_wipe_zeroes_the_payload_in_place() {
    let mut data = block_of(0xEE);
    BlockCache::<MemDisk>::wipe(&mut data);
    assert_eq!(data.len(), BS as usize, "the wipe keeps the length");
    assert!(data.iter().all(|&b| b == 0), "every byte is wiped");
}

#[test]
fn a_sequential_stream_coalesces_into_far_fewer_device_reads() {
    // The measurement that motivates readahead: the filesystem serves a
    // file (a program image, a bundle) one content block per iteration,
    // so a cold sequential read of N blocks would, block-for-block, cost
    // N device round-trips — each a full submit/park/wake on virtio or
    // emmc2. The adaptive window (8 → 16 → 32 → 64) collapses those into
    // a handful of coalesced reads.
    let (store, mut cache) = roomy_cached();
    for lba in 0..64u64 {
        let mut buf = block_of(0);
        cache.read_blocks(lba, &mut buf).unwrap();
    }
    let reads = store.borrow().reads;
    assert!(
        reads < 64,
        "readahead must cut device round-trips below one-per-block, got {reads}"
    );
    // 1 (cold block 0) + windows 8,16,32,64 cover blocks 0..121, so the
    // 64-block stream costs five device reads, not sixty-four.
    assert_eq!(reads, 5, "the adaptive window ramps 8→16→32→64");
    // Every miss admitted a hit for the following blocks: the stream is
    // mostly cache hits after the first block of each window.
    assert!(cache.accounting().hits() >= 59);
}

#[test]
fn a_sequential_stream_returns_exactly_the_device_bytes() {
    // Coalescing must not corrupt or misorder data: each served block
    // equals the device's bytes for that block.
    let (store, mut cache) = roomy_cached();
    for lba in 0..40u64 {
        let mut buf = block_of(0);
        cache.read_blocks(lba, &mut buf).unwrap();
        let at = usize::try_from(lba).unwrap() * BS as usize;
        assert_eq!(
            &buf[..],
            &store.borrow().data[at..at + BS as usize],
            "block {lba} served wrong bytes"
        );
    }
}

#[test]
fn a_prefetched_block_is_served_from_cache_not_the_device() {
    // A sequential miss retains the whole window, so the *next* blocks
    // of the stream are genuine cache hits — proven by corrupting the
    // device copy of a prefetched block and still reading the retained
    // bytes without touching the device.
    let (store, mut cache) = roomy_cached();
    let mut b0 = block_of(0);
    cache.read_blocks(0, &mut b0).unwrap();
    // Block 1 continues the stream: this miss prefetches the window
    // [1, 1 + READAHEAD_INIT_BLOCKS), retaining block 2 among others.
    let mut b1 = block_of(0);
    cache.read_blocks(1, &mut b1).unwrap();

    let expected2 = store.borrow().data[2 * BS as usize..3 * BS as usize].to_vec();
    store.borrow_mut().data[2 * BS as usize] ^= 0xFF;
    let reads_before = store.borrow().reads;
    let mut b2 = block_of(0);
    cache.read_blocks(2, &mut b2).unwrap();
    assert_eq!(
        store.borrow().reads,
        reads_before,
        "the prefetched block was served from cache"
    );
    assert_eq!(b2, expected2, "the retained (pre-corruption) bytes served");
}

#[test]
fn random_access_never_speculates() {
    // Readahead arms only on a sequential continuation; scattered reads
    // each cost exactly one single-block device read and admit exactly
    // one block, so a random workload is never penalised by speculative
    // over-reads.
    let (store, mut cache) = roomy_cached();
    let pattern = [10u64, 3, 40, 1, 99];
    for lba in pattern {
        let mut buf = block_of(0);
        cache.read_blocks(lba, &mut buf).unwrap();
    }
    assert_eq!(
        store.borrow().reads,
        pattern.len() as u64,
        "each random read issued exactly one device read"
    );
    assert_eq!(
        cache.accounting().total_bytes(),
        pattern.len() * COST,
        "random access admitted exactly the requested blocks"
    );
}

#[test]
fn a_bulk_bypass_breaks_the_sequential_run() {
    // A bulk (bypassed) read must not leave a stale sequential
    // expectation that mis-fires readahead on the next small read.
    let (store, mut cache) = cached();
    let mut b0 = block_of(0);
    cache.read_blocks(0, &mut b0).unwrap();
    // A read the budget cannot hold streams uncached and resets the
    // tracker.
    let bulk_blocks = budget().low() / BS as usize + 1;
    let mut bulk = vec![0u8; bulk_blocks * BS as usize];
    cache.read_blocks(1, &mut bulk).unwrap();
    let reads_after_bulk = store.borrow().reads;
    // Block 1 begins a fresh, cold pattern: no speculation yet (the run
    // was broken), so this is a single-block device read.
    let mut b1 = block_of(0);
    cache.read_blocks(1, &mut b1).unwrap();
    assert_eq!(
        store.borrow().reads,
        reads_after_bulk + 1,
        "the reset run reads exactly the requested block"
    );
}

/// A device that rejects any read wider than one block: a coalesced
/// readahead read fails against it, but a single-block read succeeds.
/// Proves readahead's fallback keeps a real read correct — a
/// speculative over-read never widens or invents a fault.
struct SingleBlockOnly {
    store: Rc<RefCell<Store>>,
    geo: BlockGeometry,
}

impl Block for SingleBlockOnly {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        Ok(self.geo)
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        let bs = self.geo.block_size as usize;
        if buf.len() != bs {
            // The device (or its transport) cannot service a multi-block
            // request: the coalesced readahead read must fall back.
            return Err(DriverError::Unsupported);
        }
        let start = usize::try_from(lba).unwrap() * bs;
        let mut store = self.store.borrow_mut();
        store.reads += 1;
        buf.copy_from_slice(&store.data[start..start + bs]);
        Ok(())
    }

    fn write_blocks(&mut self, _lba: u64, _buf: &[u8]) -> Result<(), DriverError> {
        Err(DriverError::Unsupported)
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        Ok(())
    }
}

#[test]
fn readahead_falls_back_to_the_exact_span_when_the_coalesced_read_faults() {
    let len = BS as usize * 64;
    let store = Rc::new(RefCell::new(Store {
        data: (0..len).map(|i| u8::try_from(i % 251).unwrap()).collect(),
        reads: 0,
        bytes_read: 0,
        writes: 0,
        flushes: 0,
        discards: Vec::new(),
        fail_writes: false,
    }));
    let disk = SingleBlockOnly {
        store: Rc::clone(&store),
        geo: BlockGeometry {
            block_size: BS,
            block_count: 64,
        },
    };
    let mut cache = BlockCache::new(disk, roomy_budget(), unpressured(), sink()).unwrap();
    // Stream sequentially: block 1 onward would coalesce, but the device
    // refuses the wide read, so each such miss falls back to the exact
    // single-block read. Correctness is preserved throughout.
    for lba in 0..16u64 {
        let mut buf = block_of(0);
        cache.read_blocks(lba, &mut buf).unwrap();
        let at = usize::try_from(lba).unwrap() * BS as usize;
        assert_eq!(
            &buf[..],
            &store.borrow().data[at..at + BS as usize],
            "block {lba} served wrong bytes through the fallback"
        );
    }
}

/// A leaked cache-admission control with the block class disabled
/// (`cache.block off`), for the switch tests below. Its own instance, so
/// it never touches the process-global control other tests rely on.
fn block_disabled_control() -> &'static CacheControl {
    let control: &'static CacheControl = Box::leak(Box::new(CacheControl::new()));
    control.set(CacheClass::Block, tairix_kernel_core::CacheMode::Off);
    control
}

#[test]
fn a_disabled_block_cache_admits_nothing_and_always_reads_the_device() {
    let (store, disk) = MemDisk::new();
    let mut cache = BlockCache::new(disk, budget(), unpressured(), sink())
        .expect("geometry queried")
        .with_cache_control(block_disabled_control());
    let mut first = block_of(0);
    cache.read_blocks(3, &mut first).unwrap();
    assert_eq!(store.borrow().reads, 1);
    assert_eq!(cache.accounting().total_bytes(), 0, "off admits nothing");

    // A second read of the same block still reaches the device: the
    // switch is a real bypass, not a flag that is read and ignored.
    let mut second = block_of(0);
    cache.read_blocks(3, &mut second).unwrap();
    assert_eq!(
        store.borrow().reads,
        2,
        "every read hits the device when off"
    );
    assert_eq!(cache.accounting().total_bytes(), 0);
    assert_eq!(cache.accounting().hits(), 0);
}

#[test]
fn flipping_the_block_switch_off_purges_the_held_blocks() {
    // Start enabled: the cache fills as usual.
    let control: &'static CacheControl = Box::leak(Box::new(CacheControl::new()));
    let (store, disk) = MemDisk::new();
    let mut cache = BlockCache::new(disk, budget(), unpressured(), sink())
        .expect("geometry queried")
        .with_cache_control(control);
    let mut buf = block_of(0);
    cache.read_blocks(3, &mut buf).unwrap();
    assert_eq!(cache.accounting().total_bytes(), COST);

    // The operator disables the class: the next operation drops (wiping)
    // everything the cache held, then serves from the device.
    control.set(CacheClass::Block, tairix_kernel_core::CacheMode::Off);
    let reads_before = store.borrow().reads;
    cache.read_blocks(3, &mut buf).unwrap();
    assert_eq!(cache.accounting().total_bytes(), 0, "the purge dropped it");
    assert_eq!(store.borrow().reads, reads_before + 1, "served from device");
    assert!(cache.accounting().teardowns() >= 1);
}
