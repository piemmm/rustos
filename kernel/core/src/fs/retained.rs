//! Uncommitted-write retention for removable volumes
//! (`plans/DEVICES.md` D4).
//!
//! Every filesystem driver in this tree writes straight through to its
//! block device, so the only bytes a surprise removal can lose are the
//! writes the *device* has accepted but not yet committed to its medium —
//! everything written since the last successful device flush
//! (SCSI `SYNCHRONIZE CACHE` on the USB path). [`RetainedWrites`] is the
//! kernel's bounded, in-RAM record of exactly that set: each device block
//! written since the last committed flush, coalesced per LBA, so a volume
//! yanked mid-write still has its uncommitted data held for the verified
//! re-insert replay or an explicit, audited force-discard.
//!
//! [`JournaledBlock`] is the [`Block`] wrapper the runtime volume attach
//! path slips between the kernel blkio client and the partition window:
//! it forwards every operation to the device and keeps the journal
//! truthful —
//!
//! * a **successful write** is recorded (an overwrite refreshes the
//!   retained copy in place);
//! * a **failed write** marks the journal [lost](RetainedWrites::is_lost):
//!   the medium's state for that range is unknown, so the retained set can
//!   no longer claim to be the complete uncommitted delta (fail closed);
//! * a **successful discard** drops the covered blocks — the filesystem
//!   has deallocated them, so their content no longer needs replaying;
//! * once the journal holds more than its **commit watermark**, the write
//!   path issues an opportunistic device flush ([`FlushBlock::flush`])
//!   and, on success, empties the journal — this is the quiesce-time
//!   `SYNCHRONIZE CACHE` the removal plan requires, and it is what keeps
//!   the uncommitted set small on a long copy instead of letting it grow
//!   until the budget degrades retention.
//!
//! # Budget and pressure
//!
//! The journal is the *only* copy of data the medium may not hold, so it
//! is not a reclaimable cache: nothing may shrink it behind its owner's
//! back. It is instead bounded by the same documented heap fraction the
//! bounded caches use ([`CacheBudget`]) and gated per growth by the
//! system pressure gauge ([`MemoryPressure::growth_permitted`]). When the
//! budget or the gauge refuses growth the journal degrades honestly: it
//! wipes and drops what it held and reports itself
//! [lost](RetainedWrites::is_lost) — the surprise-removal path then says
//! "uncommitted data existed and was not retained" rather than quietly
//! pretending. A subsequent *successful* device flush returns the journal
//! to clean: after a committed flush there is no uncommitted data left to
//! have lost.
//!
//! # Secret hygiene
//!
//! Retained bytes are user data (they may include credential-bearing
//! blocks a filesystem wrote); every retained buffer is volatilely wiped
//! before its entry is released — on refresh-discharge, commit, loss, and
//! teardown — exactly as the block cache wipes its entries.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use rustos_abi::driver::block::{Block, BlockGeometry, DeviceHealth, DiscardCapability};
use rustos_abi::driver::BufferClass;
use rustos_abi::DriverError;
use rustos_kernel_mem::{CacheBudget, MemoryPressure};
use rustos_sync::SpinLock;
use zeroize::Zeroize;

/// Approximate per-entry bookkeeping cost (map node, the fixed entry
/// fields) charged on top of a block's payload so the budget tracks real
/// heap footprint, not just payload bytes. Mirrors the block cache's
/// per-entry figure.
const ENTRY_OVERHEAD: usize = 96;

/// The commit watermark as a divisor of the budget's hard limit: once the
/// journal's accounted bytes exceed `hard / COMMIT_DIVISOR`, the write
/// path issues an opportunistic device flush and empties the journal on
/// success. The fraction keeps the uncommitted set a small slice of the
/// budget (with the 64 MiB heap: a 4 MiB hard limit commits every
/// 256 KiB), so a long sequential copy flushes steadily instead of
/// ballooning to the budget and degrading retention.
const COMMIT_DIVISOR: usize = 16;

/// A [`Block`] device that can additionally commit its volatile write
/// cache to the medium.
///
/// The [`Block`] contract itself carries no flush (the in-kernel boot
/// devices need none); the kernel blkio client serves one
/// (`BlkOp::Flush` — SCSI `SYNCHRONIZE CACHE` on the USB mass-storage
/// path), and [`JournaledBlock`] needs it to commit the journal at the
/// watermark.
pub trait FlushBlock: Block {
    /// Commit every completed write to the medium.
    ///
    /// # Errors
    ///
    /// The device's own error surface; the caller treats a failure as
    /// "the uncommitted set is still uncommitted", never as data loss.
    fn flush(&mut self) -> Result<(), DriverError>;
}

/// The bounded per-volume record of device blocks written since the last
/// committed device flush. See the module docs.
pub struct RetainedWrites {
    /// The device's block size in bytes, fixed at attach.
    block_size: usize,
    /// The byte ceiling (payload + per-entry overhead) retention may
    /// occupy.
    budget: CacheBudget,
    /// Retained block payloads, keyed by device-absolute LBA.
    blocks: BTreeMap<u64, Vec<u8>>,
    /// Accounted bytes: payload plus [`ENTRY_OVERHEAD`] per entry.
    bytes: usize,
    /// Retention was abandoned since the last committed flush (budget or
    /// pressure refused growth, or a write failed leaving the medium
    /// state unknown). Cleared by [`Self::commit`].
    lost: bool,
}

impl RetainedWrites {
    /// An empty journal for a device with `block_size`-byte blocks,
    /// bounded by `budget`.
    #[must_use]
    pub fn new(block_size: u32, budget: CacheBudget) -> Self {
        Self {
            block_size: block_size as usize,
            budget,
            blocks: BTreeMap::new(),
            bytes: 0,
            lost: false,
        }
    }

    /// Whether uncommitted writes exist (retained or lost) since the last
    /// committed flush.
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        !self.blocks.is_empty() || self.lost
    }

    /// Whether retention was abandoned since the last committed flush:
    /// uncommitted data existed that the journal does not hold.
    #[must_use]
    pub fn is_lost(&self) -> bool {
        self.lost
    }

    /// The retained payload bytes (excluding bookkeeping overhead).
    #[must_use]
    pub fn retained_bytes(&self) -> u64 {
        (self.blocks.len() * self.block_size) as u64
    }

    /// Whether the accounted bytes have passed the commit watermark and
    /// the owner should issue a device flush.
    #[must_use]
    fn wants_commit(&self) -> bool {
        self.bytes > self.budget.hard() / COMMIT_DIVISOR
    }

    /// Volatilely wipe a retained buffer before its entry is released.
    fn wipe(data: &mut Vec<u8>) {
        data.as_mut_slice().zeroize();
    }

    /// Record one successful device write of whole blocks starting at
    /// `lba`, coalescing per block. Growth past the budget, or growth the
    /// pressure gauge refuses, abandons retention ([`Self::mark_lost`])
    /// rather than evicting: the journal is the only copy, so partial
    /// retention would be a lie.
    fn record(&mut self, lba: u64, data: &[u8], pressure: &MemoryPressure) {
        if self.lost || self.block_size == 0 {
            return;
        }
        let cost = self.block_size + ENTRY_OVERHEAD;
        let mut at = 0usize;
        let mut block = lba;
        while at < data.len() {
            let bytes = &data[at..at + self.block_size];
            at += self.block_size;
            if let Some(entry) = self.blocks.get_mut(&block) {
                entry.copy_from_slice(bytes);
            } else {
                if self.bytes.saturating_add(cost) > self.budget.hard()
                    || !pressure.growth_permitted(cost)
                {
                    self.mark_lost();
                    return;
                }
                let mut copy = Vec::new();
                if copy.try_reserve_exact(self.block_size).is_err() {
                    self.mark_lost();
                    return;
                }
                copy.extend_from_slice(bytes);
                self.blocks.insert(block, copy);
                self.bytes += cost;
            }
            block += 1;
        }
    }

    /// Drop (wiped) every retained block in `lba .. lba + blocks`: the
    /// filesystem discarded the range, so its content no longer needs
    /// replaying.
    fn drop_range(&mut self, lba: u64, blocks: u64) {
        let end = lba.saturating_add(blocks);
        while let Some((&key, _)) = self.blocks.range(lba..end).next() {
            if let Some(mut entry) = self.blocks.remove(&key) {
                Self::wipe(&mut entry);
                self.bytes = self.bytes.saturating_sub(self.block_size + ENTRY_OVERHEAD);
            }
        }
    }

    /// The device committed its cache: everything retained (or lost) is
    /// on the medium, so the journal empties and returns to clean.
    pub fn commit(&mut self) {
        for entry in self.blocks.values_mut() {
            Self::wipe(entry);
        }
        self.blocks.clear();
        self.bytes = 0;
        self.lost = false;
    }

    /// Deliberately discard everything retained (or the lost mark): the
    /// audited force-unmount is abandoning the volume's uncommitted data,
    /// so the journal wipes its buffers and returns to clean. The caller
    /// reads [`Self::retained_bytes`] first to log what was given up —
    /// discarding is never silent.
    pub fn discard_all(&mut self) {
        for entry in self.blocks.values_mut() {
            Self::wipe(entry);
        }
        self.blocks.clear();
        self.bytes = 0;
        self.lost = false;
    }

    /// Abandon retention: wipe and drop everything held and remember that
    /// uncommitted data existed which the journal no longer covers.
    fn mark_lost(&mut self) {
        for entry in self.blocks.values_mut() {
            Self::wipe(entry);
        }
        self.blocks.clear();
        self.bytes = 0;
        self.lost = true;
    }
}

impl Drop for RetainedWrites {
    /// Teardown wipes every retained buffer: retained blocks are user
    /// data, which must not outlive their owner in reusable heap memory.
    fn drop(&mut self) {
        for entry in self.blocks.values_mut() {
            Self::wipe(entry);
        }
    }
}

/// A [`Block`] wrapper that keeps a shared [`RetainedWrites`] journal
/// truthful for the device it forwards to. See the module docs.
pub struct JournaledBlock<B: FlushBlock> {
    device: B,
    journal: Arc<SpinLock<RetainedWrites>>,
    pressure: &'static MemoryPressure,
}

impl<B: FlushBlock> JournaledBlock<B> {
    /// Wrap `device`, recording its successful writes into `journal`
    /// under the `pressure` gauge's growth gate.
    pub fn new(
        device: B,
        journal: Arc<SpinLock<RetainedWrites>>,
        pressure: &'static MemoryPressure,
    ) -> Self {
        Self {
            device,
            journal,
            pressure,
        }
    }

    /// The shared journal handle (the volume service holds a clone to
    /// inspect and commit it from the detach and surprise-removal paths).
    #[must_use]
    pub fn journal(&self) -> Arc<SpinLock<RetainedWrites>> {
        Arc::clone(&self.journal)
    }

    /// Record the outcome of one forwarded write, then commit through the
    /// device when the journal has passed its watermark.
    ///
    /// The journal lock is never held across the device flush (the flush
    /// parks on the serving driver): the watermark decision is taken
    /// under the lock, the flush runs outside it, and the commit re-takes
    /// it. A removal firing between the successful flush and the commit
    /// can only over-claim dirtiness — fail-safe, never data loss.
    fn after_write(&mut self, lba: u64, data: &[u8], result: Result<(), DriverError>) {
        let wants_commit = {
            let mut journal = self.journal.lock();
            match result {
                Ok(()) => journal.record(lba, data, self.pressure),
                // The medium's state for the range is unknown: the
                // retained set can no longer claim completeness.
                Err(_) => journal.mark_lost(),
            }
            journal.wants_commit()
        };
        if wants_commit && self.device.flush().is_ok() {
            self.journal.lock().commit();
        }
    }
}

impl<B: FlushBlock> Block for JournaledBlock<B> {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        self.device.geometry()
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        self.device.read_blocks(lba, buf)
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        let result = self.device.write_blocks(lba, buf);
        self.after_write(lba, buf, result);
        result
    }

    fn read_blocks_with_class(
        &mut self,
        lba: u64,
        buf: &mut [u8],
        class: BufferClass,
    ) -> Result<(), DriverError> {
        self.device.read_blocks_with_class(lba, buf, class)
    }

    fn write_blocks_with_class(
        &mut self,
        lba: u64,
        buf: &[u8],
        class: BufferClass,
    ) -> Result<(), DriverError> {
        // A sensitive write is retained like any other: the journal is
        // the only copy of an uncommitted mutation, and every retained
        // buffer is wiped before release, so hygiene is preserved without
        // sacrificing the retention guarantee.
        let result = self.device.write_blocks_with_class(lba, buf, class);
        self.after_write(lba, buf, result);
        result
    }

    fn discard_capability(&self) -> Result<DiscardCapability, DriverError> {
        self.device.discard_capability()
    }

    fn discard(&mut self, lba: u64, blocks: u64) -> Result<(), DriverError> {
        let result = self.device.discard(lba, blocks);
        if result.is_ok() {
            self.journal.lock().drop_range(lba, blocks);
        }
        result
    }

    fn device_health(&self) -> Result<DeviceHealth, DriverError> {
        self.device.device_health()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern crate std;
    use std::boxed::Box;
    use std::vec;
    use std::vec::Vec;

    use rustos_kernel_mem::FreeMemorySource;

    const BLOCK_SIZE: u32 = 512;

    /// A fixed, ample memory source for the pressure gauge.
    struct AmpleSource;
    impl FreeMemorySource for AmpleSource {
        fn free_bytes(&self) -> usize {
            1 << 30
        }
        fn total_bytes(&self) -> usize {
            1 << 30
        }
    }
    static AMPLE: AmpleSource = AmpleSource;

    /// A memory source under critical pressure: no free bytes to speak of.
    struct StarvedSource;
    impl FreeMemorySource for StarvedSource {
        fn free_bytes(&self) -> usize {
            0
        }
        fn total_bytes(&self) -> usize {
            1 << 30
        }
    }
    static STARVED: StarvedSource = StarvedSource;

    fn ample_pressure() -> &'static MemoryPressure {
        Box::leak(Box::new(MemoryPressure::over(&AMPLE)))
    }

    /// A RAM device with a flush counter and scriptable write failure.
    struct RamFlushBlock {
        data: Vec<u8>,
        flushes: usize,
        fail_writes: bool,
        fail_flush: bool,
    }

    impl RamFlushBlock {
        fn new(blocks: u64) -> Self {
            Self {
                data: vec![0u8; usize::try_from(blocks).unwrap() * BLOCK_SIZE as usize],
                flushes: 0,
                fail_writes: false,
                fail_flush: false,
            }
        }
    }

    impl Block for RamFlushBlock {
        fn geometry(&self) -> Result<BlockGeometry, DriverError> {
            Ok(BlockGeometry {
                block_size: BLOCK_SIZE,
                block_count: (self.data.len() / BLOCK_SIZE as usize) as u64,
            })
        }

        fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
            let start = usize::try_from(lba).unwrap() * BLOCK_SIZE as usize;
            buf.copy_from_slice(&self.data[start..start + buf.len()]);
            Ok(())
        }

        fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
            if self.fail_writes {
                return Err(DriverError::DeviceFault);
            }
            let start = usize::try_from(lba).unwrap() * BLOCK_SIZE as usize;
            self.data[start..start + buf.len()].copy_from_slice(buf);
            Ok(())
        }

        fn discard(&mut self, lba: u64, blocks: u64) -> Result<(), DriverError> {
            let start = usize::try_from(lba).unwrap() * BLOCK_SIZE as usize;
            let len = usize::try_from(blocks).unwrap() * BLOCK_SIZE as usize;
            self.data[start..start + len].fill(0);
            Ok(())
        }
    }

    impl FlushBlock for RamFlushBlock {
        fn flush(&mut self) -> Result<(), DriverError> {
            if self.fail_flush {
                return Err(DriverError::DeviceFault);
            }
            self.flushes += 1;
            Ok(())
        }
    }

    /// A budget whose watermark admits a few blocks before demanding a
    /// commit: hard = 16 blocks' cost, watermark = 1/16 of that.
    fn small_budget() -> CacheBudget {
        CacheBudget::from_backing((BLOCK_SIZE as usize + ENTRY_OVERHEAD) * 16 * 16)
    }

    fn journaled(
        blocks: u64,
        budget: CacheBudget,
    ) -> (JournaledBlock<RamFlushBlock>, Arc<SpinLock<RetainedWrites>>) {
        let journal = Arc::new(SpinLock::new(RetainedWrites::new(BLOCK_SIZE, budget)));
        let device = RamFlushBlock::new(blocks);
        let wrapped = JournaledBlock::new(device, Arc::clone(&journal), ample_pressure());
        (wrapped, journal)
    }

    #[test]
    fn successful_writes_are_retained_and_coalesced() {
        let (mut device, journal) = journaled(64, small_budget());
        let payload = [7u8; BLOCK_SIZE as usize];
        device.write_blocks(3, &payload).unwrap();
        device.write_blocks(3, &[9u8; BLOCK_SIZE as usize]).unwrap();
        let journal = journal.lock();
        assert!(journal.is_dirty());
        assert!(!journal.is_lost());
        assert_eq!(journal.retained_bytes(), u64::from(BLOCK_SIZE));
    }

    #[test]
    fn a_committed_flush_empties_the_journal() {
        let (mut device, journal) = journaled(64, small_budget());
        device.write_blocks(0, &[1u8; BLOCK_SIZE as usize]).unwrap();
        journal.lock().commit();
        assert!(!journal.lock().is_dirty());
    }

    #[test]
    fn a_forced_discard_empties_a_dirty_journal_to_clean() {
        let (mut device, journal) = journaled(64, small_budget());
        device.write_blocks(0, &[1u8; BLOCK_SIZE as usize]).unwrap();
        assert!(journal.lock().is_dirty());
        journal.lock().discard_all();
        let journal = journal.lock();
        assert!(!journal.is_dirty());
        assert!(!journal.is_lost());
        assert_eq!(journal.retained_bytes(), 0);
    }

    #[test]
    fn a_forced_discard_resets_a_lost_journal_to_clean() {
        let (mut device, journal) = journaled(8, small_budget());
        device.device.fail_writes = true;
        let _ = device.write_blocks(0, &[1u8; BLOCK_SIZE as usize]);
        assert!(journal.lock().is_lost());
        journal.lock().discard_all();
        let journal = journal.lock();
        assert!(!journal.is_dirty());
        assert!(!journal.is_lost());
    }

    #[test]
    fn the_watermark_issues_a_device_flush_and_commits() {
        let (mut device, journal) = journaled(64, small_budget());
        // The watermark is one block's cost; the second distinct block
        // pushes past it and triggers the opportunistic flush.
        device.write_blocks(0, &[1u8; BLOCK_SIZE as usize]).unwrap();
        device.write_blocks(1, &[2u8; BLOCK_SIZE as usize]).unwrap();
        assert!(device.device.flushes >= 1, "watermark flush issued");
        assert!(!journal.lock().is_dirty(), "journal committed");
    }

    #[test]
    fn a_refused_watermark_flush_keeps_the_journal() {
        let (mut device, journal) = journaled(64, small_budget());
        device.device.fail_flush = true;
        device.write_blocks(0, &[1u8; BLOCK_SIZE as usize]).unwrap();
        device.write_blocks(1, &[2u8; BLOCK_SIZE as usize]).unwrap();
        let journal = journal.lock();
        assert!(journal.is_dirty(), "uncommitted data is still uncommitted");
        assert!(!journal.is_lost());
    }

    #[test]
    fn budget_exhaustion_degrades_to_lost() {
        // A budget whose hard limit holds two blocks; keep the watermark
        // flush from rescuing it by failing the device flush.
        let budget = CacheBudget::from_backing((BLOCK_SIZE as usize + ENTRY_OVERHEAD) * 2 * 16);
        let (mut device, journal) = journaled(64, budget);
        device.device.fail_flush = true;
        for lba in 0..3u64 {
            device
                .write_blocks(lba, &[0xA5u8; BLOCK_SIZE as usize])
                .unwrap();
        }
        let journal = journal.lock();
        assert!(journal.is_lost(), "retention was abandoned");
        assert!(journal.is_dirty(), "lost still reads as dirty");
        assert_eq!(journal.retained_bytes(), 0, "nothing is retained");
    }

    #[test]
    fn pressure_refusal_degrades_to_lost() {
        let journal = Arc::new(SpinLock::new(RetainedWrites::new(
            BLOCK_SIZE,
            small_budget(),
        )));
        let pressure: &'static MemoryPressure = Box::leak(Box::new(MemoryPressure::over(&STARVED)));
        let mut device = JournaledBlock::new(RamFlushBlock::new(8), Arc::clone(&journal), pressure);
        device.device.fail_flush = true;
        device.write_blocks(0, &[1u8; BLOCK_SIZE as usize]).unwrap();
        assert!(journal.lock().is_lost());
    }

    #[test]
    fn a_failed_write_marks_the_journal_lost() {
        let (mut device, journal) = journaled(8, small_budget());
        device.write_blocks(0, &[1u8; BLOCK_SIZE as usize]).unwrap();
        device.device.fail_writes = true;
        assert_eq!(
            device.write_blocks(1, &[2u8; BLOCK_SIZE as usize]),
            Err(DriverError::DeviceFault)
        );
        assert!(journal.lock().is_lost());
    }

    #[test]
    fn a_commit_resets_a_lost_journal_to_clean() {
        let (mut device, journal) = journaled(8, small_budget());
        device.device.fail_writes = true;
        let _ = device.write_blocks(0, &[1u8; BLOCK_SIZE as usize]);
        assert!(journal.lock().is_lost());
        journal.lock().commit();
        let journal = journal.lock();
        assert!(!journal.is_lost());
        assert!(!journal.is_dirty());
    }

    #[test]
    fn a_discarded_range_is_dropped_from_the_journal() {
        // Budget generous enough that no watermark flush interferes.
        let budget = CacheBudget::from_backing(1 << 30);
        let (mut device, journal) = journaled(64, budget);
        for lba in 0..4u64 {
            device
                .write_blocks(lba, &[3u8; BLOCK_SIZE as usize])
                .unwrap();
        }
        device.discard(1, 2).unwrap();
        let journal = journal.lock();
        assert_eq!(journal.retained_bytes(), 2 * u64::from(BLOCK_SIZE));
        assert!(!journal.is_lost());
    }

    #[test]
    fn reads_touch_nothing() {
        let (mut device, journal) = journaled(8, small_budget());
        let mut buf = [0u8; BLOCK_SIZE as usize];
        device.read_blocks(0, &mut buf).unwrap();
        assert!(!journal.lock().is_dirty());
    }
}
