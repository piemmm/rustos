//! Block allocation: the write-path-only allocator state and the block I/O
//! that reads, updates, and stamps the on-disk allocation map
//! (`docs/src/filesystem/arxfs-spec.md` §4, §6).
//!
//! Everything here exists only to *change* a volume. A read-only handle holds
//! no [`Allocator`] at all — the field is `None` — so a read-only mount cannot
//! allocate, free, dedupe, or trim even by mistake, and pays none of the cost
//! of the state that would let it: it reads no allocation map, builds no
//! index, and walks no tree. That is what makes mounting the signed, read-only
//! `/System` volume a handful of block reads rather than a walk over every
//! inode and extent on it.
//!
//! The map's layout, cache, and bit arithmetic are [`crate::allocmap`]; this
//! module is the I/O and the allocation policy over them.
//!
//! # Keeping the resident cost bounded
//!
//! Marking a block used or free must never fail, because it happens on paths
//! that cannot report an error — rolling a failed operation back, for one. So
//! [`ARXFS::mark_used`] and [`ARXFS::mark_free`] apply the change directly
//! when the block's bitmap page *and* its summary block are already resident
//! (the ordinary case, since allocation has just read them) and otherwise
//! record it as pending. Pending changes are folded in — exactly, with the
//! block reads that needs — at the next allocation and at commit, so they
//! never accumulate beyond one transaction's worth and the free count is exact
//! whenever the volume is at rest.
//!
//! # When the map is trustworthy
//!
//! The map is stamped clean, naming the generation it reflects, at an explicit
//! sync (`fs_sync`) and at mkfs. Ordinary commits leave it dirty on the device
//! while keeping it exact in RAM, so a mount after a crash rebuilds rather
//! than adopting a half-written map. That is the rebuildable-metadata contract
//! being used as designed: the authoritative trees can always reproduce free
//! space, so the map never needs the copy-on-write and self-allocation
//! machinery an authoritative structure would.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec;
use alloc::vec::Vec;

use tairix_abi::driver::block::Block;
use tairix_abi::DriverError;

use crate::allocmap::{
    bit_get, bit_set, find_free_bit, find_free_bit_rev, map_payload_len, set_bit_range,
    summary_get, summary_set, MapCache, MapGeometry, MapHeader, ALLOC_MAP_OWNER,
};
use crate::dedupe::DedupeIndex;
use crate::header::{BlockHeader, BlockType, HEADER_LEN};
use crate::superblock::RING_BLOCKS;
use crate::{as_u32, ARXFS, MAX_BLOCK_SIZE, METADATA_RESERVE};

/// Upper bound on the transient pending-discard queue, in blocks. The queue
/// batches freed-but-not-yet-trimmed blocks; it is rebuildable,
/// non-authoritative state, so a deliberately bounded, **volume-independent**
/// ceiling keeps its worst-case footprint fixed (8 bytes per entry, so under
/// 1 MiB here) no matter how large the device is. A device-sized cap would
/// scale the queue with the block count and exhaust the bounded kernel heap on
/// a large volume; dropping a freed block from a full queue merely leaves it
/// un-discarded (still free) until a future free, trim pass, or mount rebuild
/// requeues it, so nothing is lost.
pub(crate) const MAX_PENDING_DISCARD: usize = 1 << 16;

/// Every piece of state that only a *writable* mount can use: the allocation
/// map and its cursors, the per-transaction bookkeeping, the pending-discard
/// queue, and the dedupe index.
///
/// Grouping them behind one `Option` on [`ARXFS`] is what makes "a read-only
/// handle never allocates" a property of the types rather than a convention:
/// there is no allocator to consult, so no read path can reach one.
pub(crate) struct Allocator {
    /// Where the map region sits and what it covers.
    pub(crate) geom: MapGeometry,
    /// Bounded cache of resident region blocks.
    pub(crate) cache: MapCache,
    /// Bit changes whose page or summary block was not resident when they were
    /// made, folded in at the next allocation or commit.
    pub(crate) pending: BTreeMap<u64, bool>,
    /// Whether the region header on the device currently reads clean at the
    /// committed generation. A page is never written while it does: the stamp
    /// is turned dirty first, so an interrupted update is rebuilt rather than
    /// adopted. It starts `false` because only [`ARXFS::map_adopt`] has
    /// actually read a clean stamp; assuming one would risk writing pages
    /// under a stamp that still vouches for the old ones.
    pub(crate) stamped_clean: bool,
    /// Where the next upward data scan starts.
    pub(crate) alloc_cursor: u64,
    /// Where the next downward metadata scan starts.
    pub(crate) meta_cursor: u64,
    /// Blocks this transaction allocated and has not published, so it may
    /// reuse or reclaim them immediately.
    pub(crate) txn_private: BTreeSet<u64>,
    /// Every block this transaction allocated, for rollback.
    pub(crate) txn_allocated: Vec<u64>,
    /// Blocks inherited from a committed root that this transaction released;
    /// reclaimed only once the transaction commits. A set, not a list, so a
    /// block released twice by different paths counts once — the commit reads
    /// its size to record the free count the committed volume will have.
    pub(crate) txn_freed: BTreeSet<u64>,
    /// Freed blocks awaiting a device discard.
    pub(crate) pending_discard: Vec<u64>,
    /// The bounded, rebuildable `(domain, length, logical hash) -> chunk`
    /// dedupe cache.
    pub(crate) dedupe_index: DedupeIndex,
}

/// One free-block count moved by a single block: down when the block became
/// used, up when it became free. Saturating, so a miscounted map can never
/// wrap the count into a wildly wrong value.
fn step(free: u64, used: bool) -> u64 {
    if used {
        free.saturating_sub(1)
    } else {
        free.saturating_add(1)
    }
}

impl Allocator {
    /// A cold allocator over `geom`, with empty transaction bookkeeping.
    pub(crate) fn new(geom: MapGeometry, block_size: usize, total_blocks: u64) -> Self {
        Self {
            geom,
            cache: MapCache::new(block_size),
            pending: BTreeMap::new(),
            stamped_clean: false,
            alloc_cursor: RING_BLOCKS,
            meta_cursor: total_blocks.saturating_sub(1),
            txn_private: BTreeSet::new(),
            txn_allocated: Vec::new(),
            txn_freed: BTreeSet::new(),
            pending_discard: Vec::new(),
            dedupe_index: DedupeIndex::new(),
        }
    }
}

impl<B: Block> ARXFS<B> {
    /// The allocator, or [`DriverError::PermissionDenied`] on a read-only
    /// handle. This is the structural backstop behind the entry-point
    /// read-only guards: a write path that somehow reached this far still
    /// fails closed rather than inventing allocation state.
    pub(crate) fn allocator(&self) -> Result<&Allocator, DriverError> {
        self.alloc.as_ref().ok_or(DriverError::PermissionDenied)
    }

    /// Mutable access to the allocator; fails closed exactly as
    /// [`Self::allocator`].
    pub(crate) fn allocator_mut(&mut self) -> Result<&mut Allocator, DriverError> {
        self.alloc.as_mut().ok_or(DriverError::PermissionDenied)
    }

    /// The mounted map's layout, or an error on a read-only handle.
    fn map_geometry(&self) -> Result<MapGeometry, DriverError> {
        Ok(self.allocator()?.geom)
    }

    // --- region block I/O -------------------------------------------------

    /// Read region block `region_block` into the cache, verifying its seal.
    /// Already-resident blocks are a no-op.
    pub(crate) fn map_read(&mut self, region_block: u64) -> Result<(), DriverError> {
        if self.allocator()?.cache.contains(region_block) {
            return Ok(());
        }
        self.map_make_room()?;
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        let bs = self.block_size;
        self.read_block(region_block, &mut buf)?;
        BlockHeader::decode_verify(
            &buf[..bs],
            BlockType::AllocMap,
            self.fs_uuid,
            region_block,
            &self.mac_key,
        )?;
        self.allocator_mut()?
            .cache
            .insert_clean(region_block, &buf[HEADER_LEN..bs]);
        Ok(())
    }

    /// Evict the least-recently-used region block when the cache is full,
    /// writing it back first if it holds changes.
    fn map_make_room(&mut self) -> Result<(), DriverError> {
        let Some((victim, dirty)) = self.allocator()?.cache.eviction_candidate() else {
            return Ok(());
        };
        if dirty {
            self.map_write(victim)?;
        }
        self.allocator_mut()?.cache.remove(victim);
        Ok(())
    }

    /// Seal and write one cached region block to the device.
    ///
    /// The region header is stamped dirty first, so any interrupted sequence
    /// of page writes is detected at the next mount and rebuilt rather than
    /// adopted.
    fn map_write(&mut self, region_block: u64) -> Result<(), DriverError> {
        self.map_mark_dirty()?;
        self.map_write_raw(region_block)
    }

    /// Seal and write a cached region block without touching the clean stamp;
    /// the stamping paths use this so they control the ordering themselves.
    fn map_write_raw(&mut self, region_block: u64) -> Result<(), DriverError> {
        let bs = self.block_size;
        let start = self.map_geometry()?.start();
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        {
            let alloc = self.allocator()?;
            let payload = alloc
                .cache
                .peek(region_block)
                .ok_or(DriverError::DeviceFault)?;
            buf[HEADER_LEN..HEADER_LEN + payload.len()].copy_from_slice(payload);
        }
        let header = BlockHeader {
            block_type: BlockType::AllocMap,
            fs_uuid: self.fs_uuid,
            owner: ALLOC_MAP_OWNER,
            generation: self.generation,
            logical_addr: region_block - start,
            physical_addr: region_block,
            payload_len: as_u32(map_payload_len(bs)),
        };
        header.seal(&mut buf[..bs], &self.mac_key)?;
        self.write_block(region_block, &buf)?;
        self.allocator_mut()?.cache.mark_written(region_block);
        Ok(())
    }

    /// Write the region header with the given clean stamp.
    fn map_stamp(&mut self, clean: bool, generation: u64) -> Result<(), DriverError> {
        let geom = self.map_geometry()?;
        let bs = self.block_size;
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        let header = MapHeader {
            covered: geom.covered(),
            region_start: geom.start(),
            region_blocks: geom.region_blocks(),
            clean_generation: generation,
            clean,
        };
        header.encode(&mut buf[HEADER_LEN..bs])?;
        let block = BlockHeader {
            block_type: BlockType::AllocMap,
            fs_uuid: self.fs_uuid,
            owner: ALLOC_MAP_OWNER,
            generation: self.generation,
            logical_addr: 0,
            physical_addr: geom.header_block(),
            payload_len: as_u32(map_payload_len(bs)),
        };
        block.seal(&mut buf[..bs], &self.mac_key)?;
        self.write_block(geom.header_block(), &buf)?;
        self.allocator_mut()?.stamped_clean = clean;
        Ok(())
    }

    /// Record on the device that the map is being changed, before the first
    /// such change reaches it.
    fn map_mark_dirty(&mut self) -> Result<(), DriverError> {
        if !self.allocator()?.stamped_clean {
            return Ok(());
        }
        self.map_stamp(false, 0)
    }

    /// Write every changed region block out, force the device cache, and stamp
    /// the map clean at the committed generation so the next mount can adopt
    /// it without a rebuild.
    ///
    /// This is the whole of an explicit sync: the one barrier makes the
    /// transaction's own blocks *and* the map pages durable, and the clean
    /// stamp is written only afterwards, so a stamp can never reach stable
    /// media ahead of the pages it vouches for. The stamp itself needs no
    /// second barrier — losing it costs a rebuild at the next mount, never
    /// correctness.
    pub(crate) fn map_persist(&mut self) -> Result<(), DriverError> {
        self.map_fold_pending()?;
        for region_block in self.allocator()?.cache.dirty_blocks() {
            self.map_write(region_block)?;
        }
        self.block.flush()?;
        if self.allocator()?.stamped_clean {
            return Ok(());
        }
        let generation = self.generation;
        self.map_stamp(true, generation)
    }

    // --- bit and summary access -------------------------------------------

    /// Ensure bitmap page `page` is resident, along with the summary block
    /// that records its free count.
    ///
    /// A page the summary reports entirely free is *synthesised* as zeroes
    /// rather than read: it holds no information the summary does not, and a
    /// freshly formatted volume never wrote it.
    fn map_page_load(&mut self, page: u64) -> Result<(), DriverError> {
        let geom = self.map_geometry()?;
        let (summary_index, offset) = geom.summary_slot_of(page);
        self.map_read(geom.summary_block(summary_index))?;
        let page_block = geom.page_block(page);
        if self.allocator()?.cache.contains(page_block) {
            return Ok(());
        }
        let free = {
            let alloc = self.allocator_mut()?;
            let summary = alloc
                .cache
                .read(geom.summary_block(summary_index))
                .ok_or(DriverError::DeviceFault)?;
            summary_get(summary, offset)
        };
        if free >= geom.page_capacity(page) {
            self.map_make_room()?;
            let bytes = vec![0u8; map_payload_len(self.block_size)];
            self.allocator_mut()?.cache.insert_clean(page_block, &bytes);
            return Ok(());
        }
        self.map_read(page_block)
    }

    /// Free blocks bitmap page `page` currently records.
    fn map_page_free(&mut self, page: u64) -> Result<u64, DriverError> {
        let geom = self.map_geometry()?;
        let (summary_index, offset) = geom.summary_slot_of(page);
        let summary_block = geom.summary_block(summary_index);
        self.map_read(summary_block)?;
        let alloc = self.allocator_mut()?;
        let summary = alloc
            .cache
            .read(summary_block)
            .ok_or(DriverError::DeviceFault)?;
        Ok(summary_get(summary, offset))
    }

    /// Whether `block` is used, reading its page if necessary. A block outside
    /// the map's coverage reads used, so it is never handed out.
    pub(crate) fn bit_used(&mut self, block: u64) -> Result<bool, DriverError> {
        if let Some(&pending) = self.allocator()?.pending.get(&block) {
            return Ok(pending);
        }
        let geom = self.map_geometry()?;
        if block >= geom.covered() {
            return Ok(true);
        }
        let page = geom.page_of(block);
        self.map_page_load(page)?;
        let page_block = geom.page_block(page);
        let bit = geom.bit_of(block);
        let alloc = self.allocator_mut()?;
        let payload = alloc
            .cache
            .read(page_block)
            .ok_or(DriverError::DeviceFault)?;
        Ok(bit_get(payload, bit))
    }

    /// Apply one bit change exactly, reading whatever the map needs, and keep
    /// the page summary and the volume's free count in step.
    fn map_apply(&mut self, block: u64, used: bool) -> Result<(), DriverError> {
        let geom = self.map_geometry()?;
        if block >= geom.covered() {
            return Ok(());
        }
        let page = geom.page_of(block);
        // The summary is loaded first and the page second, so the page load
        // can never evict the summary it just consulted: the cache evicts its
        // least-recently-used block and both were touched moments ago.
        self.map_page_load(page)?;
        let page_block = geom.page_block(page);
        let (summary_index, offset) = geom.summary_slot_of(page);
        let summary_block = geom.summary_block(summary_index);
        let bit = geom.bit_of(block);
        let changed = {
            let alloc = self.allocator_mut()?;
            alloc.pending.remove(&block);
            let changed = alloc
                .cache
                .write(page_block)
                .map(|payload| bit_set(payload, bit, used))
                .ok_or(DriverError::DeviceFault)?;
            if changed {
                let summary = alloc
                    .cache
                    .write(summary_block)
                    .ok_or(DriverError::DeviceFault)?;
                let free = summary_get(summary, offset);
                summary_set(summary, offset, step(free, used));
            }
            changed
        };
        if changed {
            self.free_count = step(self.free_count, used);
        }
        Ok(())
    }

    /// Fold every deferred bit change into the map, making the free count and
    /// the page summaries exact again.
    pub(crate) fn map_fold_pending(&mut self) -> Result<(), DriverError> {
        loop {
            let Some((block, used)) = self.allocator_mut()?.pending.pop_first() else {
                return Ok(());
            };
            self.map_apply(block, used)?;
        }
    }

    /// Mark `block` used. Infallible: it applies the change when the map
    /// pages it needs are already resident and defers it otherwise.
    pub(crate) fn mark_used(&mut self, block: u64) {
        self.map_mark(block, true);
    }

    /// Mark `block` free, on the same terms as [`Self::mark_used`].
    pub(crate) fn mark_free(&mut self, block: u64) {
        self.map_mark(block, false);
    }

    fn map_mark(&mut self, block: u64, used: bool) {
        let Ok(geom) = self.map_geometry() else {
            return;
        };
        if block >= self.total_blocks || block >= geom.covered() {
            return;
        }
        let page = geom.page_of(block);
        let page_block = geom.page_block(page);
        let (summary_index, offset) = geom.summary_slot_of(page);
        let summary_block = geom.summary_block(summary_index);
        let changed = {
            let Ok(alloc) = self.allocator_mut() else {
                return;
            };
            if !alloc.cache.contains(page_block) || !alloc.cache.contains(summary_block) {
                alloc.pending.insert(block, used);
                return;
            }
            alloc.pending.remove(&block);
            let changed = alloc
                .cache
                .write(page_block)
                .is_some_and(|payload| bit_set(payload, geom.bit_of(block), used));
            if changed {
                if let Some(summary) = alloc.cache.write(summary_block) {
                    let free = summary_get(summary, offset);
                    summary_set(summary, offset, step(free, used));
                }
            }
            changed
        };
        if changed {
            self.free_count = step(self.free_count, used);
        }
    }

    /// Mark a metadata block and its companion mirror used, exactly, reading
    /// whatever the map needs. Used by the rebuild walk, where deferring would
    /// let the pending set grow with the volume.
    pub(crate) fn mark_meta_used_checked(&mut self, phys: u64) -> Result<(), DriverError> {
        self.map_apply(phys, true)?;
        self.map_apply(Self::companion(phys), true)
    }

    /// Mark a single block used, exactly. The rebuild walk's counterpart to
    /// [`Self::mark_used`].
    pub(crate) fn mark_used_checked(&mut self, block: u64) -> Result<(), DriverError> {
        self.map_apply(block, true)
    }

    /// Reserve the whole run `from..from + len`, a page at a time.
    ///
    /// The rebuild reserves the superblock ring and the map region this way.
    /// The region is hundreds of thousands of blocks on a very large volume,
    /// so marking it block by block would cost a cache lookup each; filling
    /// whole bytes of a resident page instead keeps the rebuild proportional
    /// to the *pages* the run spans.
    ///
    /// Only the rebuild uses this, immediately after the map is laid out, so
    /// there are never deferred marks in the run to reconcile.
    pub(crate) fn mark_range_used(&mut self, from: u64, len: u64) -> Result<(), DriverError> {
        let geom = self.map_geometry()?;
        let end = from.saturating_add(len).min(geom.covered());
        let mut block = from;
        while block < end {
            let page = geom.page_of(block);
            let first = geom.page_first_block(page);
            let page_end = (first + geom.page_capacity(page)).min(end);
            self.map_page_load(page)?;
            let (summary_index, offset) = geom.summary_slot_of(page);
            let summary_block = geom.summary_block(summary_index);
            let claimed = {
                let alloc = self.allocator_mut()?;
                let payload = alloc
                    .cache
                    .write(geom.page_block(page))
                    .ok_or(DriverError::DeviceFault)?;
                set_bit_range(payload, block - first, page_end - first)
            };
            if claimed > 0 {
                let alloc = self.allocator_mut()?;
                let summary = alloc
                    .cache
                    .write(summary_block)
                    .ok_or(DriverError::DeviceFault)?;
                let free = summary_get(summary, offset);
                summary_set(summary, offset, free.saturating_sub(claimed));
                self.free_count = self.free_count.saturating_sub(claimed);
            }
            block = page_end;
        }
        Ok(())
    }

    // --- scanning ---------------------------------------------------------

    /// The lowest free block in `lo..hi`, or `None`. Pages the summary reports
    /// full are skipped without being read, so a near-full volume does not
    /// cost a walk of every block.
    fn map_scan_up(&mut self, lo: u64, hi: u64) -> Result<Option<u64>, DriverError> {
        let geom = self.map_geometry()?;
        let hi = hi.min(geom.covered());
        let mut block = lo;
        while block < hi {
            let page = geom.page_of(block);
            let first = geom.page_first_block(page);
            let capacity = geom.page_capacity(page);
            if self.map_page_free(page)? == 0 {
                block = first + capacity;
                continue;
            }
            self.map_page_load(page)?;
            let page_block = geom.page_block(page);
            let limit = capacity.min(hi - first);
            let found = {
                let alloc = self.allocator_mut()?;
                let payload = alloc
                    .cache
                    .read(page_block)
                    .ok_or(DriverError::DeviceFault)?;
                find_free_bit(payload, block - first, limit)
            };
            match found {
                Some(bit) => return Ok(Some(first + bit)),
                None => block = first + capacity,
            }
        }
        Ok(None)
    }

    /// The highest free block in `floor..=from`, or `None`.
    fn map_scan_down(&mut self, from: u64, floor: u64) -> Result<Option<u64>, DriverError> {
        let geom = self.map_geometry()?;
        if floor > from || geom.covered() == 0 {
            return Ok(None);
        }
        let mut block = from.min(geom.covered() - 1);
        loop {
            if block < floor {
                return Ok(None);
            }
            let page = geom.page_of(block);
            let first = geom.page_first_block(page);
            if self.map_page_free(page)? == 0 {
                if first <= floor {
                    return Ok(None);
                }
                block = first - 1;
                continue;
            }
            self.map_page_load(page)?;
            let page_block = geom.page_block(page);
            let low_bit = floor.saturating_sub(first);
            let found = {
                let alloc = self.allocator_mut()?;
                let payload = alloc
                    .cache
                    .read(page_block)
                    .ok_or(DriverError::DeviceFault)?;
                find_free_bit_rev(payload, block - first, low_bit)
            };
            if let Some(bit) = found {
                return Ok(Some(first + bit));
            }
            if first <= floor {
                return Ok(None);
            }
            block = first - 1;
        }
    }

    /// The lowest start of `run` consecutive free blocks in `lo..hi`.
    pub(crate) fn map_find_free_run(
        &mut self,
        run: u64,
        lo: u64,
        hi: u64,
    ) -> Result<Option<u64>, DriverError> {
        if run == 0 {
            return Ok(None);
        }
        let mut block = lo;
        while block + run <= hi {
            let Some(start) = self.map_scan_up(block, hi)? else {
                return Ok(None);
            };
            if start + run > hi {
                return Ok(None);
            }
            let mut offset = 1;
            while offset < run {
                if self.bit_used(start + offset)? {
                    break;
                }
                offset += 1;
            }
            if offset == run {
                return Ok(Some(start));
            }
            block = start + offset + 1;
        }
        Ok(None)
    }

    // --- allocation -------------------------------------------------------

    /// Allocate one block: a mirrored metadata pair when `metadata`, otherwise
    /// a single data block.
    ///
    /// Data and metadata draw from opposite ends of the pool: file data scans
    /// **upward** from the low end and metadata (tree nodes, the transaction
    /// root, directory blocks) scans **downward** from the high end. Keeping
    /// the two streams apart lets a large sequential write land in physically
    /// contiguous blocks even though it interleaves extent-tree growth, so it
    /// collapses to one extent run rather than fragmenting
    /// (`docs/src/filesystem/arxfs-spec.md` §6). Metadata also draws on the
    /// last [`METADATA_RESERVE`] free blocks so a delete or other shrinking
    /// transaction can still copy-on-write itself on an otherwise-full volume;
    /// data allocation stops at the reserve and fails closed with
    /// [`DriverError::NoSpace`].
    pub(crate) fn alloc_block(&mut self, metadata: bool) -> Result<u64, DriverError> {
        if metadata {
            self.alloc_meta_pair()
        } else {
            self.alloc_data_block()
        }
    }

    /// Mark `block` used, private to this transaction, and recorded for
    /// rollback.
    pub(crate) fn claim_block(&mut self, block: u64) {
        self.mark_used(block);
        let Ok(alloc) = self.allocator_mut() else {
            return;
        };
        alloc.txn_private.insert(block);
        alloc.txn_allocated.push(block);
    }

    /// Allocate one data block, scanning **upward** from the low end.
    fn alloc_data_block(&mut self) -> Result<u64, DriverError> {
        self.map_fold_pending()?;
        if self.free_count <= METADATA_RESERVE {
            return Err(DriverError::NoSpace);
        }
        let start = RING_BLOCKS;
        let total = self.total_blocks;
        let cursor = self.allocator()?.alloc_cursor.max(start);
        let found = match self.map_scan_up(cursor, total)? {
            Some(block) => Some(block),
            None => self.map_scan_up(start, cursor)?,
        };
        let block = found.ok_or(DriverError::NoSpace)?;
        self.claim_block(block);
        self.allocator_mut()?.alloc_cursor = block + 1;
        Ok(block)
    }

    /// Allocate a mirrored metadata pair, scanning **downward** from the high
    /// end for two adjacent free blocks `(primary, primary + 1)`. Returns the
    /// primary; both blocks are claimed. Fails closed with
    /// [`DriverError::NoSpace`] when no adjacent free pair remains — never a
    /// panic.
    fn alloc_meta_pair(&mut self) -> Result<u64, DriverError> {
        self.map_fold_pending()?;
        let floor = RING_BLOCKS + 1;
        let total = self.total_blocks;
        if total <= floor {
            return Err(DriverError::NoSpace);
        }
        let cursor = self.allocator()?.meta_cursor.clamp(floor, total - 1);
        let primary = match self.find_meta_pair(cursor, floor)? {
            Some(primary) => primary,
            None => self
                .find_meta_pair(total - 1, cursor)?
                .ok_or(DriverError::NoSpace)?,
        };
        self.claim_block(primary);
        self.claim_block(primary + 1);
        self.allocator_mut()?.meta_cursor = primary.saturating_sub(1).max(floor);
        Ok(primary)
    }

    /// The highest `primary` in `floor..=from` whose pair `(primary,
    /// primary + 1)` is entirely free.
    fn find_meta_pair(&mut self, from: u64, floor: u64) -> Result<Option<u64>, DriverError> {
        let mut high = from;
        loop {
            let Some(candidate) = self.map_scan_down(high, floor)? else {
                return Ok(None);
            };
            if candidate > floor && !self.bit_used(candidate - 1)? {
                return Ok(Some(candidate - 1));
            }
            if candidate <= floor {
                return Ok(None);
            }
            high = candidate - 1;
        }
    }

    // --- building and adopting the map ------------------------------------

    /// Lay out an allocation map covering `covered` blocks at `start` and
    /// install it, with every page recorded entirely free.
    ///
    /// Nothing reaches the device here: the summary blocks enter the cache
    /// dirty and are written when they are evicted or persisted, so laying the
    /// map out on a volume whose summary fits in the cache costs no I/O at
    /// all. A bitmap page the summary reports wholly free is synthesised on
    /// demand and never read or written either, so even a very large volume's
    /// layout costs a handful of writes rather than one per terabyte.
    fn map_install(&mut self, start: u64, covered: u64) -> Result<(), DriverError> {
        let geom = MapGeometry::new(start, self.block_size, covered)?;
        if start < RING_BLOCKS || start.saturating_add(geom.region_blocks()) > covered {
            return Err(DriverError::NoSpace);
        }
        match self.alloc.as_mut() {
            Some(alloc) => {
                // The clean stamp is left as it is: it describes what the
                // *device* currently says, and the map being replaced in RAM
                // does not change that. The first page write turns it dirty.
                alloc.geom = geom;
                alloc.cache.clear();
                alloc.pending.clear();
            }
            None => {
                self.alloc = Some(Allocator::new(geom, self.block_size, self.total_blocks));
            }
        }
        self.alloc_map_start = start;
        let payload_len = map_payload_len(self.block_size);
        let slots = payload_len / 2;
        for index in 0..geom.summary_blocks() {
            self.map_make_room()?;
            let mut payload = vec![0u8; payload_len];
            for slot in 0..slots {
                let page = index * (slots as u64) + slot as u64;
                if page >= geom.pages() {
                    break;
                }
                summary_set(&mut payload, slot * 2, geom.page_capacity(page));
            }
            self.allocator_mut()?
                .cache
                .insert_dirty(geom.summary_block(index), payload);
        }
        self.free_count = covered;
        Ok(())
    }

    /// Rebuild the allocation map from scratch by walking the live trees: the
    /// superblock ring (always reserved), the map region itself, the published
    /// transaction root, the scrub-progress and health-baseline records, every
    /// chunk / reverse-reference and inode-tree node, and, for each inode, its
    /// extent-tree nodes plus the physical runs they map. Every metadata block
    /// accounts for both its physical copies
    /// (`docs/src/filesystem/arxfs-spec.md` §4, §5).
    ///
    /// Free space is rebuildable derived state, never authoritative, so this
    /// is the single rebuild walk shared by mount recovery, [`ARXFS::grow`],
    /// and the offline check. It is idempotent: a second rebuild of an
    /// unchanged volume produces the same map.
    pub(crate) fn rebuild_free_space(&mut self) -> Result<(), DriverError> {
        let start = self.map_geometry()?.start();
        self.rebuild_free_space_at(start, self.total_blocks)
    }

    /// [`Self::rebuild_free_space`], laying the map out at `start` over
    /// `covered` blocks first. Used when the map moves — a fresh volume, or a
    /// grow that outgrows its region.
    pub(crate) fn rebuild_free_space_at(
        &mut self,
        start: u64,
        covered: u64,
    ) -> Result<(), DriverError> {
        self.map_install(start, covered)?;
        self.mark_range_used(0, RING_BLOCKS)?;
        let geom = self.map_geometry()?;
        self.mark_range_used(geom.start(), geom.region_blocks())?;
        if self.root_phys != 0 {
            self.mark_meta_used_checked(self.root_phys)?;
        }
        if self.scrub_progress_root != 0 {
            self.mark_meta_used_checked(self.scrub_progress_root)?;
        }
        if self.health_baseline_root != 0 {
            self.mark_meta_used_checked(self.health_baseline_root)?;
        }
        self.mark_reachable_metadata()?;
        let alloc = self.allocator_mut()?;
        alloc.alloc_cursor = RING_BLOCKS;
        alloc.meta_cursor = covered.saturating_sub(1);
        Ok(())
    }

    /// Adopt the on-disk map named by the committed transaction root, or
    /// report that it cannot be trusted.
    ///
    /// The map is adopted only when its region block authenticates at the
    /// address the root names, describes exactly the region and coverage the
    /// root committed, and is stamped clean at the root's generation. Anything
    /// else — a crash mid-update, a stale stamp, a torn or misdirected block —
    /// yields `false` and the caller rebuilds.
    pub(crate) fn map_adopt(&mut self, start: u64, covered: u64) -> bool {
        if start < RING_BLOCKS || covered != self.total_blocks {
            return false;
        }
        let Ok(geom) = MapGeometry::new(start, self.block_size, covered) else {
            return false;
        };
        if start.saturating_add(geom.region_blocks()) > covered {
            return false;
        }
        let bs = self.block_size;
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        if self.read_block(geom.header_block(), &mut buf).is_err() {
            return false;
        }
        if BlockHeader::decode_verify(
            &buf[..bs],
            BlockType::AllocMap,
            self.fs_uuid,
            geom.header_block(),
            &self.mac_key,
        )
        .is_err()
        {
            return false;
        }
        let Ok(header) = MapHeader::decode(&buf[HEADER_LEN..bs]) else {
            return false;
        };
        if header.region_start != start
            || header.covered != covered
            || header.region_blocks != geom.region_blocks()
            || !header.clean
            || header.clean_generation != self.generation
        {
            return false;
        }
        let mut allocator = Allocator::new(geom, self.block_size, self.total_blocks);
        allocator.stamped_clean = true;
        self.alloc = Some(allocator);
        true
    }
}

#[cfg(test)]
impl<B: Block> ARXFS<B> {
    /// Every block the allocation map records as used.
    ///
    /// Tests compare a live map against one rebuilt from the authoritative
    /// trees, which the map's paged form no longer exposes as a single
    /// in-memory set. Reading the whole map is fine on a test-sized volume and
    /// deliberately not offered outside tests.
    pub(crate) fn used_blocks(&mut self) -> BTreeSet<u64> {
        self.map_fold_pending().expect("fold the pending marks");
        let mut used = BTreeSet::new();
        for block in 0..self.total_blocks {
            if self.bit_used(block).expect("read the allocation map") {
                used.insert(block);
            }
        }
        used
    }

    /// Whether `block` is used, for tests that assert on allocation state.
    pub(crate) fn is_used(&mut self, block: u64) -> bool {
        self.bit_used(block).expect("read the allocation map")
    }

    /// The dedupe index of a writable handle.
    pub(crate) fn dedupe_index_mut(&mut self) -> &mut DedupeIndex {
        &mut self
            .allocator_mut()
            .expect("a writable handle has an allocator")
            .dedupe_index
    }

    /// Region blocks currently resident in the bounded map cache.
    pub(crate) fn map_cached_blocks(&self) -> usize {
        self.allocator().map_or(0, |alloc| alloc.cache.len())
    }

    /// Whether the map on the device is currently stamped clean.
    pub(crate) fn map_is_stamped_clean(&self) -> bool {
        self.allocator().is_ok_and(|alloc| alloc.stamped_clean)
    }

    /// First block of the mounted allocation-map region.
    pub(crate) fn map_region_start(&self) -> u64 {
        self.allocator().map_or(0, |alloc| alloc.geom.start())
    }

    /// Blocks the mounted allocation-map region occupies.
    pub(crate) fn map_region_blocks(&self) -> u64 {
        self.allocator()
            .map_or(0, |alloc| alloc.geom.region_blocks())
    }
}
