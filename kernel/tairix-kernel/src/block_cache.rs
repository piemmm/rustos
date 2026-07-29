//! The whole-disk block-level LRU cache (`plans/SMARTRAM.md` SMART11).
//!
//! [`BlockCache`] wraps the one brought-up boot disk *below* the
//! block-sharing layer (`crate::shared_block::SharedBlock`), so every
//! window onto the disk — the read-only `/System` driver-store window,
//! the encrypted-root unlock window, and the writable-root window —
//! reads through one coherent cache of recently used device blocks.
//! It complements the higher layers rather than duplicating them:
//! `kernel/core::fs::CachedFs` retains *served plaintext* per volume
//! and the SMART3 `TransformClusterCache` retains *decompressed
//! cluster plaintext*; this cache retains the raw device blocks
//! underneath both, so their misses — and every uncached consumer of
//! the disk (partition-table walks, driver-store scans, `ARXFS`
//! metadata block reads) — avoid a device round-trip that otherwise
//! parks the calling task across a completion interrupt.
//!
//! # Classification, budget, pressure
//!
//! At construction the cache declares its [`CacheCandidate`] — class
//! [`ReclaimClass::CleanFileData`] (clean, rebuildable bytes
//! re-readable from the device by one bounded read), owned by the
//! kernel block-device subsystem, cheap to rebuild, treated as user
//! data (the disk carries the encrypted user volume), precisely
//! invalidated by the device's single serialised writer, droppable on
//! demand — and classifies it through the `kernel/mem::reclaim`
//! admission gate. A refusal poisons the cache from birth: every
//! operation passes straight through to the device (fail closed,
//! never an unclassified cache).
//!
//! The cache is bounded by a [`CacheBudget`] and accounted in a
//! [`CacheAccounting`] ledger. Every operation first applies the
//! current pressure band's forced-shrink target for the clean-file
//! class ([`shrink_target`]): shrunk to the low watermark at mild
//! pressure and drained to zero from moderate on, before anonymous
//! pages are handed to `ramzip` (`plans/SWAPSWAPSWAP.md` section 6).
//! Growth is admitted only at normal pressure and never into the
//! reserve ([`MemoryPressure::growth_permitted`]). Inserts over the
//! hard limit first evict least-recently-used blocks down to the low
//! watermark (hysteresis).
//!
//! # Coherence and secret hygiene
//!
//! The cache sits on the device side of the `SharedBlock` sleep lock,
//! so it observes **every** operation any window issues, serialised:
//! a write updates the cached copies of the written blocks in place, a
//! discard invalidates its range, and a failed write invalidates the
//! range (the device state is unknown — fail closed). Reads and
//! writes issued under [`BufferClass::Sensitive`] carry material the
//! caller will scrub (key slots, credentials): they bypass the cache
//! entirely *and* evict any cached copy of their range, so no
//! credential-bearing block is ever retained. Every cached buffer is
//! volatilely wiped before its entry is released — on invalidation,
//! eviction, pressure shrink, poisoning, and teardown — because the
//! disk carries the encrypted user volume and cached ciphertext is
//! still treated as user data.
//!
//! # Bypasses
//!
//! Reads larger than [`LARGE_READ_BYPASS_BLOCKS`] blocks stream
//! through uncached in both directions (a bundle or driver-store bulk
//! load must not flush the hot working set), mirroring `CachedFs`'s
//! large-read bypass. Unaligned or zero-length requests are the
//! device's own error surface and are forwarded untouched.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use tairix_abi::driver::block::{Block, BlockGeometry, DeviceHealth, DiscardCapability};
use tairix_abi::driver::BufferClass;
use tairix_abi::DriverError;
use tairix_kernel_core::{CacheClass, CacheControl, CACHE_CONTROL};
use tairix_kernel_mem::{
    log_cache_poisoned, log_cache_refused, shrink_target, CacheAccounting, CacheBudget,
    CacheCandidate, CachePolicy, InvalidationSource, MemoryPressure, RebuildCost, ReclaimClass,
    ReclaimOwner, ReclaimRule, Sensitivity,
};
use tairix_log::Sink;
use zeroize::Zeroize;

/// Approximate per-entry bookkeeping cost (map nodes, the LRU index
/// slot, the fixed entry fields) charged on top of a block's payload
/// so the ledger tracks real heap footprint, not just payload bytes.
const ENTRY_OVERHEAD: usize = 96;

/// The fixed `cache` label this cache's audit records carry.
const CACHE_LABEL: &str = "block";

/// The stable subsystem identifier the cache's memory is charged to:
/// the whole-disk layer below every mounted volume, so no single
/// volume identity fits.
const OWNER_SUBSYSTEM: &str = "boot_block_device";

/// Reads spanning more blocks than this stream through uncached: a
/// bulk sequential load (bundle read, driver-store scan) must not
/// flush the hot working set. Mirrors `CachedFs`'s large-read bypass.
pub const LARGE_READ_BYPASS_BLOCKS: u64 = 64;

/// Readahead only drives off *small* requests: a request already this
/// many blocks wide amortises its own device round-trip, so speculating
/// past it wins nothing and only risks pulling in blocks the caller
/// never asked for. The filesystem's per-block content read (one
/// `data_capacity()`-sized block per iteration) is the request this
/// bound is sized for.
const READAHEAD_TRIGGER_BLOCKS: u64 = 8;

/// The first readahead window opened when a sequential access pattern is
/// detected, in device blocks. The window then doubles on each further
/// sequential miss up to [`READAHEAD_MAX_BLOCKS`], the same ramp Linux's
/// page-cache readahead uses so an isolated read pays no speculation and
/// a long sequential stream quickly reaches the widest coalesced I/O.
const READAHEAD_INIT_BLOCKS: u64 = 8;

/// The widest readahead window, in device blocks. Held at
/// [`LARGE_READ_BYPASS_BLOCKS`] so a coalesced prefetch never retains
/// more of the working set than a single non-bypassed request could,
/// and one prefetch is always one device round-trip.
const READAHEAD_MAX_BLOCKS: u64 = LARGE_READ_BYPASS_BLOCKS;

/// The widest device block the cache will retain. A device reporting
/// a larger (or zero) block size is served uncached: per-block entries
/// must stay individually bounded.
const MAX_BLOCK_SIZE: u32 = 4096;

/// One retained device block: its bytes and its recency tick.
struct Entry {
    data: Vec<u8>,
    tick: u64,
}

/// The reclaimable whole-disk block cache the boot path wraps the
/// brought-up device in, below the block-sharing layer. See the
/// module docs.
pub struct BlockCache<B: Block> {
    device: B,
    geometry: BlockGeometry,
    budget: CacheBudget,
    /// The live cache-admission control (the operator's `cache.block` /
    /// `cache.all` switch). Sampled at the head of every operation: when
    /// the block class is disabled the cache admits nothing and drops
    /// (wiping) what it holds, a real bypass. Defaults to the process-
    /// global [`CACHE_CONTROL`]; a test binds its own.
    control: &'static CacheControl,
    /// The system memory-pressure gauge, sampled at the head of every
    /// operation: the band's forced-shrink target is applied before
    /// the cache is read or grown, and admission is refused outside
    /// normal pressure or when growth would dip into the reserve.
    pressure: &'static MemoryPressure,
    /// The audit sink a classification refusal or detected ledger
    /// defect reports through (`kernel/mem::reclaim_audit`).
    sink: &'static (dyn Sink + Sync),
    accounting: Arc<CacheAccounting>,
    /// The classified admission policy; `None` when classification
    /// refused, which poisons the cache from birth.
    policy: Option<CachePolicy>,
    /// The cache admits nothing further (a classification refusal, an
    /// uncacheable geometry, or a detected ledger defect): every
    /// operation passes straight through to the device (fail closed).
    poisoned: bool,
    /// Monotonic recency counter; every touch assigns a fresh tick,
    /// so ticks are unique and the LRU index is keyed by them.
    tick: u64,
    /// Retained blocks, keyed by LBA.
    entries: BTreeMap<u64, Entry>,
    /// LRU index: tick (oldest first) to entry key.
    lru: BTreeMap<u64, u64>,
    /// The LBA the next read is *expected* at if the caller is streaming
    /// sequentially: the block just past the previous request's span.
    /// A request whose start matches it is a sequential continuation and
    /// arms readahead; any other start is random access and disarms it.
    /// `None` before the first read and after a bypass breaks the run.
    readahead_next: Option<u64>,
    /// The current readahead window in device blocks: `0` while the
    /// pattern looks random, ramping [`READAHEAD_INIT_BLOCKS`] →
    /// [`READAHEAD_MAX_BLOCKS`] (doubling) across a sustained sequential
    /// stream. Only ever a hint — a wrong guess costs at most one
    /// bounded, budget-gated over-read, never a wrong result.
    readahead_window: u64,
}

impl<B: Block> BlockCache<B> {
    /// Wrap `device` in a cache bounded by `budget` and governed by the
    /// system `pressure` gauge, querying the device geometry once.
    ///
    /// The candidate declaration passes the `kernel/mem::reclaim`
    /// classification gate; a refusal — or a device block size the
    /// per-block entry model cannot bound (zero or wider than
    /// `MAX_BLOCK_SIZE`) — poisons the cache from birth, so the disk
    /// still serves and every operation passes straight through (fail
    /// closed, never an unclassified or unbounded cache).
    ///
    /// # Errors
    ///
    /// Propagates [`Block::geometry`]'s error: the device is never
    /// wrapped on a geometry fault, exactly as the block-sharing layer
    /// refuses it.
    pub fn new(
        device: B,
        budget: CacheBudget,
        pressure: &'static MemoryPressure,
        sink: &'static (dyn Sink + Sync),
    ) -> Result<Self, DriverError> {
        let geometry = device.geometry()?;
        let owner = ReclaimOwner::KernelSubsystem(OWNER_SUBSYSTEM);
        let candidate = CacheCandidate {
            class: Some(ReclaimClass::CleanFileData),
            owner: Some(owner),
            rebuild_cost: RebuildCost::Cheap,
            sensitivity: Some(Sensitivity::UserData),
            invalidation: Some(InvalidationSource::SourceMutation),
            rule: Some(ReclaimRule::Drop),
            entry_metadata_bytes: ENTRY_OVERHEAD,
        };
        let policy = match candidate.classify() {
            Ok(policy) => Some(policy),
            Err(refusal) => {
                log_cache_refused(sink, CACHE_LABEL, Some(owner), refusal);
                None
            }
        };
        let mut cache = Self {
            device,
            geometry,
            budget,
            control: &CACHE_CONTROL,
            pressure,
            sink,
            accounting: Arc::new(CacheAccounting::new()),
            poisoned: policy.is_none(),
            policy,
            tick: 0,
            entries: BTreeMap::new(),
            lru: BTreeMap::new(),
            readahead_next: None,
            readahead_window: 0,
        };
        if geometry.block_size == 0 || geometry.block_size > MAX_BLOCK_SIZE {
            cache.poison("uncacheable_geometry");
        }
        Ok(cache)
    }

    /// The cache the boot path wraps the one brought-up disk in before
    /// the block-sharing layer, budgeted from the kernel heap arena
    /// exactly like the volume caches above it and governed by the
    /// system `pressure` gauge.
    ///
    /// # Errors
    ///
    /// Propagates [`Block::geometry`]'s error, exactly as [`Self::new`].
    pub fn for_boot_disk(
        device: B,
        pressure: &'static MemoryPressure,
        sink: &'static (dyn Sink + Sync),
    ) -> Result<Self, DriverError> {
        Self::new(
            device,
            // Budget from discovered physical RAM (the kernel heap is now
            // growable, so its bootstrap size is no longer the memory to size
            // a cache against); falls back to the bootstrap size before the
            // boot path publishes RAM.
            CacheBudget::from_backing(tairix_kernel_core::memstats::cache_backing_bytes()),
            pressure,
            sink,
        )
    }

    /// Bind a specific [`CacheControl`] instead of the process-global
    /// [`CACHE_CONTROL`] this cache consults by default. Production uses
    /// the shared global the unlock path applies the operator's
    /// configuration to; a host test drives the disable path against its
    /// own control through this builder.
    #[must_use]
    pub fn with_cache_control(mut self, control: &'static CacheControl) -> Self {
        self.control = control;
        self
    }

    /// The cache's byte ledger and event counters.
    #[must_use]
    pub fn accounting(&self) -> &CacheAccounting {
        &self.accounting
    }

    /// A shared handle to this cache's ledger, for registration with the
    /// System Information memory-statistics registry. Observation-only:
    /// the holder gets lock-free reads of the same saturating
    /// diagnostics this cache keeps.
    #[must_use]
    pub fn accounting_shared(&self) -> Arc<CacheAccounting> {
        Arc::clone(&self.accounting)
    }

    /// The owner the cache's memory is charged to, or `None` when
    /// classification refused the cache (it is then poisoned and
    /// serves every operation uncached).
    #[must_use]
    pub fn owner(&self) -> Option<ReclaimOwner> {
        self.policy.map(CachePolicy::owner)
    }

    /// The device block size in bytes.
    fn block_size(&self) -> usize {
        self.geometry.block_size as usize
    }

    /// The next unique recency tick.
    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    /// The accounted `(payload, metadata)` byte cost of one cached
    /// block plus the fixed per-entry bookkeeping.
    fn cost_of(&self) -> (usize, usize) {
        (self.block_size(), ENTRY_OVERHEAD)
    }

    /// Volatilely wipe a cached buffer: the disk carries the encrypted
    /// user volume, so released bytes must not linger in reusable heap
    /// memory.
    fn wipe(data: &mut Vec<u8>) {
        data.as_mut_slice().zeroize();
    }

    /// Remove (and wipe) the block cached at `lba`, dropping its LRU
    /// slot and discharging its cost. `false` when no such entry
    /// exists.
    fn remove_lba(&mut self, lba: u64) -> bool {
        let Some(mut entry) = self.entries.remove(&lba) else {
            return false;
        };
        Self::wipe(&mut entry.data);
        self.lru.remove(&entry.tick);
        let (payload, metadata) = self.cost_of();
        if self
            .accounting
            .discharge(ReclaimClass::CleanFileData, payload, metadata)
            .is_err()
        {
            self.poison("ledger_imbalance");
        }
        true
    }

    /// Remove (and wipe) every cached block in `lba .. lba + blocks`,
    /// counting each as an invalidation.
    fn invalidate_range(&mut self, lba: u64, blocks: u64) {
        let end = lba.saturating_add(blocks);
        while let Some((&key, _)) = self.entries.range(lba..end).next() {
            if self.remove_lba(key) {
                self.accounting.record_invalidation();
            }
            if self.poisoned {
                return;
            }
        }
    }

    /// Drop every entry (wiped) and admit nothing further: the
    /// fail-closed response to the internal defect named by `cause`.
    /// The device keeps serving; only the cache is disabled. The
    /// defect is counted and reported once through the audit sink.
    fn poison(&mut self, cause: &'static str) {
        if !self.poisoned {
            self.accounting.record_failure(ReclaimClass::CleanFileData);
            log_cache_poisoned(self.sink, CACHE_LABEL, self.owner(), cause);
        }
        self.poisoned = true;
        self.drop_all();
    }

    /// Drop every entry, wiping all payload and rebalancing the ledger
    /// to empty. Every whole-cache drain is counted as a teardown.
    fn drop_all(&mut self) {
        self.accounting.record_teardown(ReclaimClass::CleanFileData);
        for entry in self.entries.values_mut() {
            Self::wipe(&mut entry.data);
        }
        self.entries.clear();
        self.lru.clear();
        self.accounting.zero_ledger();
    }

    /// Evict least-recently-used blocks until the ledger total is at
    /// most `target`.
    fn evict_until(&mut self, target: usize) {
        while self.accounting.total_bytes() > target {
            let Some((_, &lba)) = self.lru.first_key_value() else {
                return;
            };
            if !self.remove_lba(lba) {
                // An index slot with no backing entry is a ledger
                // defect; fail closed rather than loop.
                self.poison("orphan_index_slot");
                return;
            }
            self.accounting.record_eviction();
        }
    }

    /// Apply the current pressure band's forced-shrink target for the
    /// clean-file class, called at the head of every cache-touching
    /// operation before the cache is read or grown
    /// (`plans/SMARTRAM.md` section 7): shrunk to the low watermark at
    /// mild pressure, drained to zero from moderate on. Every evicted
    /// buffer is wiped on the way out.
    fn enforce_pressure(&mut self) {
        if self.poisoned {
            return;
        }
        // The operator disabled the block cache (`cache.block` or the
        // master `cache.all` off): drop (wiping) everything and admit
        // nothing further — a real bypass, every operation passes straight
        // through to the device. Re-enabling lets the next read refill it.
        if !self.control.admits(CacheClass::Block) {
            if self.accounting.total_bytes() > 0 {
                self.drop_all();
            }
            return;
        }
        let band = self.pressure.sample();
        let target = shrink_target(band, ReclaimClass::CleanFileData, self.budget);
        if self.accounting.total_bytes() > target {
            self.accounting
                .record_pressure_shrink(ReclaimClass::CleanFileData);
            self.evict_until(target);
        }
    }

    /// Serve `blocks` device blocks starting at `lba` into `buf` from
    /// the cache, refreshing recency. `false` (with `buf` untouched)
    /// unless every block in the span is cached.
    fn try_serve(&mut self, lba: u64, blocks: u64, buf: &mut [u8]) -> bool {
        for i in 0..blocks {
            if !self.entries.contains_key(&(lba + i)) {
                return false;
            }
        }
        let bs = self.block_size();
        let mut at = 0usize;
        for i in 0..blocks {
            let key = lba + i;
            let tick = self.next_tick();
            let Some(entry) = self.entries.get_mut(&key) else {
                // Vanished between the presence check and the copy: a
                // ledger/index defect; fail closed to the device path.
                self.poison("index_desync");
                return false;
            };
            buf[at..at + bs].copy_from_slice(&entry.data);
            at += bs;
            self.lru.remove(&entry.tick);
            self.lru.insert(tick, key);
            entry.tick = tick;
        }
        true
    }

    /// Retain the just-read `buf` per block, replacing stale copies and
    /// admitting new blocks under the budget and pressure gates. Stops
    /// at the first refused admission: one refusal means the next
    /// insert faces the same gate.
    fn populate(&mut self, lba: u64, blocks: u64, buf: &[u8]) {
        if !self.control.admits(CacheClass::Block) {
            self.accounting.record_refusal(ReclaimClass::CleanFileData);
            return;
        }
        let bs = self.block_size();
        let (payload, metadata) = self.cost_of();
        let cost = payload.saturating_add(metadata);
        let mut at = 0usize;
        for i in 0..blocks {
            if self.poisoned {
                return;
            }
            let key = lba + i;
            let bytes = &buf[at..at + bs];
            at += bs;
            // A cached copy is refreshed in place: same length, so the
            // ledger is unchanged and no re-admission is needed.
            if let Some(entry) = self.entries.get_mut(&key) {
                entry.data.copy_from_slice(bytes);
                continue;
            }
            if cost > self.budget.hard() || !self.pressure.growth_permitted(cost) {
                self.accounting.record_refusal(ReclaimClass::CleanFileData);
                return;
            }
            if self.accounting.total_bytes().saturating_add(cost) > self.budget.hard() {
                let headroom = self.budget.low().min(self.budget.hard() - cost);
                self.evict_until(headroom);
                if self.poisoned {
                    return;
                }
            }
            // The copy is fallible: heap exhaustion refuses the entry
            // instead of aborting.
            let mut data = Vec::new();
            if data.try_reserve_exact(bs).is_err() {
                self.accounting.record_refusal(ReclaimClass::CleanFileData);
                return;
            }
            data.extend_from_slice(bytes);
            if self
                .accounting
                .charge(ReclaimClass::CleanFileData, payload, metadata)
                .is_err()
            {
                Self::wipe(&mut data);
                self.accounting.record_refusal(ReclaimClass::CleanFileData);
                return;
            }
            let tick = self.next_tick();
            self.lru.insert(tick, key);
            self.entries.insert(key, Entry { data, tick });
        }
    }

    /// Whether a request shaped `(lba, len)` is cacheable: block-
    /// aligned, non-empty, within the small-read bound, and not
    /// wrapping the LBA space. Anything else streams through uncached
    /// and takes the device's own error surface.
    fn cacheable_span(&self, lba: u64, len: usize) -> Option<u64> {
        let bs = self.block_size();
        if self.poisoned || len == 0 || bs == 0 || !len.is_multiple_of(bs) {
            return None;
        }
        let blocks = (len / bs) as u64;
        if blocks > LARGE_READ_BYPASS_BLOCKS || lba.checked_add(blocks).is_none() {
            return None;
        }
        Some(blocks)
    }

    /// Reset the sequential-readahead tracker: the next small read
    /// starts a fresh pattern with no speculation. Called when a bypass
    /// or a poisoned pass-through breaks the run.
    fn readahead_reset(&mut self) {
        self.readahead_next = None;
        self.readahead_window = 0;
    }

    /// How many contiguous device blocks to fetch for a miss at `lba`
    /// that wants `blocks` blocks: `blocks` (no speculation) unless a
    /// sequential stream is detected, in which case a bounded readahead
    /// window is opened and doubled up to [`READAHEAD_MAX_BLOCKS`],
    /// clamped to the end of the device. Never returns fewer than
    /// `blocks`.
    ///
    /// Readahead is a pure performance hint: it turns one device
    /// round-trip per block of a sequential read (the filesystem's
    /// per-block content path) into one round-trip per window, so a
    /// cold sequential load — a program image, a bundle, a
    /// driver-store scan — pays a small fraction of the round-trips.
    /// A wrong guess (random access, or a stream that ends early)
    /// over-reads at most one bounded, budget-gated window and never
    /// changes a result.
    fn plan_readahead(&mut self, lba: u64, blocks: u64, sequential: bool) -> u64 {
        if blocks > READAHEAD_TRIGGER_BLOCKS || !sequential {
            self.readahead_window = 0;
            return blocks;
        }
        let window = if self.readahead_window == 0 {
            READAHEAD_INIT_BLOCKS
        } else {
            self.readahead_window
                .saturating_mul(2)
                .min(READAHEAD_MAX_BLOCKS)
        };
        self.readahead_window = window;
        let remaining = self.geometry.block_count.saturating_sub(lba);
        window.min(remaining).max(blocks)
    }

    /// The shared read path: serve from cache when the whole span is
    /// retained, else read the device through `forward` and retain the
    /// result. A sequential miss reads a bounded readahead window ahead
    /// in one device request and retains it, so the following blocks of
    /// a streaming read are served from cache. Bypassed spans go
    /// straight to the device.
    fn cached_read(
        &mut self,
        lba: u64,
        buf: &mut [u8],
        forward: impl Fn(&mut B, u64, &mut [u8]) -> Result<(), DriverError>,
    ) -> Result<(), DriverError> {
        self.enforce_pressure();
        let Some(blocks) = self.cacheable_span(lba, buf.len()) else {
            // A bypassed (uncacheable or bulk) read breaks the tracked
            // sequential run; the next small read starts cold.
            self.readahead_reset();
            return forward(&mut self.device, lba, buf);
        };
        // A request that begins exactly where the previous one ended is a
        // sequential continuation. Recorded before serving so both a hit
        // and a miss advance the expectation to the block past this span.
        let sequential = self.readahead_next == Some(lba);
        self.readahead_next = Some(lba.saturating_add(blocks));
        if self.try_serve(lba, blocks, buf) {
            self.accounting.record_hit(ReclaimClass::CleanFileData);
            return Ok(());
        }
        if self.poisoned {
            self.readahead_reset();
            return forward(&mut self.device, lba, buf);
        }
        self.accounting.record_miss(ReclaimClass::CleanFileData);
        let prefetch = self.plan_readahead(lba, blocks, sequential);
        let want_bytes = usize::try_from(prefetch)
            .unwrap_or(0)
            .saturating_mul(self.block_size());
        if want_bytes > buf.len() {
            // A fallible, zeroed staging buffer for the one coalesced
            // readahead device read. `vec![0; want_bytes]` would abort on
            // allocation failure; the kernel must instead fail closed and
            // fall back to the exact read, so the buffer is grown through
            // `try_reserve_exact` and then zeroed — deterministic OOM, no
            // panic.
            #[allow(clippy::slow_vector_initialization)]
            let mut scratch = Vec::new();
            if scratch.try_reserve_exact(want_bytes).is_ok() {
                scratch.resize(want_bytes, 0);
                if forward(&mut self.device, lba, &mut scratch).is_ok() {
                    buf.copy_from_slice(&scratch[..buf.len()]);
                    self.populate(lba, prefetch, &scratch);
                    Self::wipe(&mut scratch);
                    return Ok(());
                }
                // The coalesced read faulted: fall through to the
                // exact-span read so a genuine device fault is reported
                // for the requested blocks alone — a speculative
                // over-read never widens a caller's fault.
                Self::wipe(&mut scratch);
            }
            // Reservation failed under memory pressure: fall through to
            // the exact-span read (never fail a real read for want of a
            // speculative buffer).
        }
        forward(&mut self.device, lba, buf)?;
        self.populate(lba, blocks, buf);
        Ok(())
    }

    /// The shared write path: write through to the device via
    /// `forward`, then refresh the cached copies of the written blocks
    /// in place. A failed write invalidates the range instead — the
    /// device state is unknown, so the cache must not vouch for it
    /// (fail closed).
    fn cached_write(
        &mut self,
        lba: u64,
        buf: &[u8],
        forward: impl FnOnce(&mut B, u64, &[u8]) -> Result<(), DriverError>,
    ) -> Result<(), DriverError> {
        self.enforce_pressure();
        let result = forward(&mut self.device, lba, buf);
        if let Some(blocks) = self.cacheable_span(lba, buf.len()) {
            match result {
                Ok(()) => self.refresh_in_place(lba, blocks, buf),
                Err(_) => self.invalidate_range(lba, blocks),
            }
        } else if !self.poisoned {
            // A bypassed write still mutates the device: nothing it
            // covers may be served stale.
            let bs = self.block_size();
            if bs != 0 && buf.len().is_multiple_of(bs) {
                self.invalidate_range(lba, (buf.len() / bs) as u64);
            } else {
                // An unaligned mutation's extent is unknowable in
                // block terms; drop everything rather than guess.
                self.drop_all();
            }
        }
        result
    }

    /// Overwrite the cached copies of the written blocks in place;
    /// blocks not cached are left uncached (a write is not a read
    /// prediction, so it admits nothing new).
    fn refresh_in_place(&mut self, lba: u64, blocks: u64, buf: &[u8]) {
        let bs = self.block_size();
        let mut at = 0usize;
        for i in 0..blocks {
            if let Some(entry) = self.entries.get_mut(&(lba + i)) {
                entry.data.copy_from_slice(&buf[at..at + bs]);
            }
            at += bs;
        }
    }

    /// Evict (wiped) whatever a sensitive-class request covers: key
    /// slots and credentials are never cacheable, so no copy of the
    /// range may be retained.
    fn scrub_sensitive_span(&mut self, lba: u64, len: usize) {
        if self.poisoned {
            return;
        }
        let bs = self.block_size();
        if bs != 0 && len.is_multiple_of(bs) {
            self.invalidate_range(lba, (len / bs) as u64);
        } else {
            self.drop_all();
        }
    }
}

impl<B: Block> Block for BlockCache<B> {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        // Served from the construction-time query: immutable for the
        // life of a disk, exactly as the block-sharing layer caches it.
        Ok(self.geometry)
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        self.cached_read(lba, buf, Block::read_blocks)
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        self.cached_write(lba, buf, Block::write_blocks)
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        // A read cache: every write already went straight through to the
        // device (`cached_write`), so nothing is buffered here. Forward the
        // durability flush to the device so its volatile cache commits.
        self.device.flush()
    }

    fn read_blocks_with_class(
        &mut self,
        lba: u64,
        buf: &mut [u8],
        class: BufferClass,
    ) -> Result<(), DriverError> {
        if class.is_sensitive() {
            // Sensitive reads carry material the caller will scrub;
            // the cache must retain no copy of the range, before or
            // after.
            self.scrub_sensitive_span(lba, buf.len());
            return self.device.read_blocks_with_class(lba, buf, class);
        }
        self.cached_read(lba, buf, |device, lba, buf| {
            device.read_blocks_with_class(lba, buf, class)
        })
    }

    fn write_blocks_with_class(
        &mut self,
        lba: u64,
        buf: &[u8],
        class: BufferClass,
    ) -> Result<(), DriverError> {
        if class.is_sensitive() {
            // A sensitive write's bytes are never retained, and any
            // prior cached copy of the range goes with them.
            self.scrub_sensitive_span(lba, buf.len());
            return self.device.write_blocks_with_class(lba, buf, class);
        }
        self.cached_write(lba, buf, |device, lba, buf| {
            device.write_blocks_with_class(lba, buf, class)
        })
    }

    fn discard_capability(&self) -> Result<DiscardCapability, DriverError> {
        self.device.discard_capability()
    }

    fn discard(&mut self, lba: u64, blocks: u64) -> Result<(), DriverError> {
        // Discarded blocks have no defined content either way: the
        // range is invalidated whether or not the device accepts it.
        if !self.poisoned {
            self.invalidate_range(lba, blocks);
        }
        self.device.discard(lba, blocks)
    }

    fn device_health(&self) -> Result<DeviceHealth, DriverError> {
        self.device.device_health()
    }
}

impl<B: Block> Drop for BlockCache<B> {
    /// Teardown wipes every retained buffer: the cached blocks are
    /// treated as user data, which must not outlive their owner in
    /// reusable heap memory.
    fn drop(&mut self) {
        for entry in self.entries.values_mut() {
            Self::wipe(&mut entry.data);
        }
    }
}

#[cfg(test)]
#[path = "block_cache_tests.rs"]
mod tests;
