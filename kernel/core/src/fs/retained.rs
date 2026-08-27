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
//!   path issues an opportunistic device flush ([`Block::flush`])
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
//! system pressure gauge
//! ([`tairix_reclaim::PressureGauge::growth_permitted`]). When the
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

use tairix_abi::blkio::BlkDeviceClass;
use tairix_abi::driver::block::{Block, BlockGeometry, DeviceHealth, DiscardCapability};
use tairix_abi::driver::BufferClass;
use tairix_abi::DriverError;
use tairix_reclaim::{CacheBudget, GrowthAllowance, MemoryPressure};
use tairix_sync::SpinLock;
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

/// The dual-acceptance shadow of a volume's mutation-evidence window
/// (`plans/DEVICES.md` D4c): the on-medium region any foreign mutation of
/// the volume must rewrite (the format's superblock ring / superblock /
/// boot+`FSInfo` head, sized by `tairix_fsprobe::evidence_len`), held as two
/// acceptable states — the content of the last **committed** device flush
/// and the **latest** content this kernel wrote. At re-insert each
/// evidence block on the medium must equal one of the two (the device may
/// have committed any per-block subset of the uncommitted writes before
/// the yank); anything else is a foreign mutation and replay is refused.
struct EvidenceShadow {
    /// Device-absolute LBA of the window's first block.
    first_lba: u64,
    /// The window's content as of the last committed device flush.
    committed: Vec<u8>,
    /// The window's content including every write recorded since.
    latest: Vec<u8>,
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
    /// The mutation-evidence shadow the verified re-insert compares, or
    /// `None` when no evidence is held (never seeded, or invalidated by a
    /// lost episode / an overlapping discard) — no evidence means no
    /// replay, fail closed.
    evidence: Option<EvidenceShadow>,
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
            evidence: None,
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

    /// Seed the mutation-evidence shadow with the window read from the
    /// device at attach time: `bytes` is the window's current on-medium
    /// content, starting at device-absolute `first_lba` and spanning
    /// whole blocks. A malformed window (empty, or not block-aligned) or
    /// a refused allocation leaves the journal without evidence — the
    /// re-insert then fails closed to the conflict path, never a guess.
    /// The window is bounded by `tairix_fsprobe::EVIDENCE_MAX`, far below
    /// the retention budget, so it is not charged against it.
    pub fn set_evidence(&mut self, first_lba: u64, bytes: &[u8]) {
        self.invalidate_evidence();
        if bytes.is_empty() || self.block_size == 0 || !bytes.len().is_multiple_of(self.block_size)
        {
            return;
        }
        let mut committed = Vec::new();
        let mut latest = Vec::new();
        if committed.try_reserve_exact(bytes.len()).is_err()
            || latest.try_reserve_exact(bytes.len()).is_err()
        {
            return;
        }
        committed.extend_from_slice(bytes);
        latest.extend_from_slice(bytes);
        self.evidence = Some(EvidenceShadow {
            first_lba,
            committed,
            latest,
        });
    }

    /// The held evidence window as `(first_lba, byte_len)`, for the
    /// re-insert path's device re-read; `None` when no evidence is held.
    #[must_use]
    pub fn evidence_window(&self) -> Option<(u64, usize)> {
        self.evidence
            .as_ref()
            .map(|shadow| (shadow.first_lba, shadow.latest.len()))
    }

    /// Whether `current` — the evidence window re-read from a re-inserted
    /// device — proves the volume was not mutated elsewhere: evidence is
    /// held, the lengths agree, and every block equals its committed or
    /// its latest shadow copy (the device may have committed any
    /// per-block subset of the uncommitted writes before the yank).
    #[must_use]
    pub fn verify_evidence(&self, current: &[u8]) -> bool {
        let Some(shadow) = self.evidence.as_ref() else {
            return false;
        };
        if current.len() != shadow.latest.len() || self.block_size == 0 {
            return false;
        }
        current
            .chunks(self.block_size)
            .zip(shadow.committed.chunks(self.block_size))
            .zip(shadow.latest.chunks(self.block_size))
            .all(|((cur, committed), latest)| cur == committed || cur == latest)
    }

    /// A cloned snapshot of the retained blocks for the re-insert replay,
    /// ordered by LBA. `None` when an allocation is refused (the caller
    /// then fails closed to the conflict path). The snapshot wipes its
    /// buffers when dropped — the copies carry the same user data the
    /// journal itself wipes on release, so hygiene cannot be forgotten.
    #[must_use]
    pub fn retained_snapshot(&self) -> Option<ReplaySnapshot> {
        let mut out = Vec::new();
        if out.try_reserve_exact(self.blocks.len()).is_err() {
            return None;
        }
        for (&lba, data) in &self.blocks {
            let mut copy = Vec::new();
            if copy.try_reserve_exact(data.len()).is_err() {
                out.push((lba, copy));
                drop(ReplaySnapshot(out));
                return None;
            }
            copy.extend_from_slice(data);
            out.push((lba, copy));
        }
        Some(ReplaySnapshot(out))
    }

    /// Wipe and drop the evidence shadow: the window's on-medium state is
    /// no longer known (a lost episode, an overlapping discard), so it
    /// can never again prove non-mutation.
    fn invalidate_evidence(&mut self) {
        if let Some(mut shadow) = self.evidence.take() {
            Self::wipe(&mut shadow.committed);
            Self::wipe(&mut shadow.latest);
        }
    }

    /// Fold one recorded whole-block write into the evidence shadow's
    /// `latest` copy where it overlaps the window.
    fn evidence_update(&mut self, block: u64, bytes: &[u8]) {
        let block_size = self.block_size;
        let Some(shadow) = self.evidence.as_mut() else {
            return;
        };
        let Some(index) = block.checked_sub(shadow.first_lba) else {
            return;
        };
        let Some(offset) = usize::try_from(index)
            .ok()
            .and_then(|i| i.checked_mul(block_size))
        else {
            return;
        };
        if let Some(slot) = shadow.latest.get_mut(offset..offset + block_size) {
            slot.copy_from_slice(bytes);
        }
    }

    /// Record one successful device write of whole blocks starting at
    /// `lba`, coalescing per block. Growth past the budget, or growth the
    /// pressure gauge refuses, abandons retention ([`Self::mark_lost`])
    /// rather than evicting: the journal is the only copy, so partial
    /// retention would be a lie.
    ///
    /// The gauge is read **at most once** for the whole write and each
    /// retained block draws its cost down from that one allowance: the
    /// reading is the physical frame allocator, so re-asking per block took
    /// the global frame-allocator lock once per 4 KiB of every write, and
    /// answered from a free figure that does not move as blocks are
    /// retained — so the reserve bounded each block instead of the run. A
    /// write that refreshes only blocks already held admits nothing and so
    /// takes no reading at all.
    fn record(&mut self, lba: u64, data: &[u8], pressure: &MemoryPressure) {
        if self.lost || self.block_size == 0 {
            return;
        }
        let mut allowance: Option<GrowthAllowance> = None;
        let cost = self.block_size + ENTRY_OVERHEAD;
        let mut at = 0usize;
        let mut block = lba;
        while at < data.len() {
            let bytes = &data[at..at + self.block_size];
            at += self.block_size;
            self.evidence_update(block, bytes);
            if let Some(entry) = self.blocks.get_mut(&block) {
                entry.copy_from_slice(bytes);
            } else {
                if self.bytes.saturating_add(cost) > self.budget.hard()
                    || !allowance
                        .get_or_insert_with(|| pressure.growth_allowance())
                        .take_reserve(cost)
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
    /// replaying. A discard overlapping the evidence window leaves that
    /// window's on-medium content unspecified, so the evidence is
    /// invalidated with it (fail closed).
    fn drop_range(&mut self, lba: u64, blocks: u64) {
        let end = lba.saturating_add(blocks);
        if let Some((first, len)) = self.evidence_window() {
            let evidence_end = first.saturating_add((len / self.block_size.max(1)) as u64);
            if lba < evidence_end && end > first {
                self.invalidate_evidence();
            }
        }
        while let Some((&key, _)) = self.blocks.range(lba..end).next() {
            if let Some(mut entry) = self.blocks.remove(&key) {
                Self::wipe(&mut entry);
                self.bytes = self.bytes.saturating_sub(self.block_size + ENTRY_OVERHEAD);
            }
        }
    }

    /// The device committed its cache: everything retained (or lost) is
    /// on the medium, so the journal empties and returns to clean, and
    /// the evidence window's latest content becomes its committed
    /// content.
    pub fn commit(&mut self) {
        for entry in self.blocks.values_mut() {
            Self::wipe(entry);
        }
        self.blocks.clear();
        self.bytes = 0;
        self.lost = false;
        if let Some(shadow) = self.evidence.as_mut() {
            shadow.committed.copy_from_slice(&shadow.latest);
        }
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
    /// uncommitted data existed which the journal no longer covers. The
    /// evidence shadow goes with it — writes stop being folded into it
    /// while the journal is lost, so it could never again be truthful.
    fn mark_lost(&mut self) {
        for entry in self.blocks.values_mut() {
            Self::wipe(entry);
        }
        self.blocks.clear();
        self.bytes = 0;
        self.lost = true;
        self.invalidate_evidence();
    }
}

/// A wiped-on-drop clone of a journal's retained blocks, ordered by LBA:
/// the working copy the verified re-insert replays onto the device
/// ([`RetainedWrites::retained_snapshot`]).
pub struct ReplaySnapshot(Vec<(u64, Vec<u8>)>);

impl ReplaySnapshot {
    /// The `(device-absolute LBA, block content)` pairs to replay.
    #[must_use]
    pub fn blocks(&self) -> &[(u64, Vec<u8>)] {
        &self.0
    }
}

impl Drop for ReplaySnapshot {
    /// The copies carry retained user data; wipe them on release exactly
    /// as the journal wipes its own buffers.
    fn drop(&mut self) {
        for (_, data) in &mut self.0 {
            RetainedWrites::wipe(data);
        }
    }
}

impl Drop for RetainedWrites {
    /// Teardown wipes every retained buffer: retained blocks are user
    /// data, which must not outlive their owner in reusable heap memory.
    /// The evidence shadow is device content too and is wiped with them.
    fn drop(&mut self) {
        for entry in self.blocks.values_mut() {
            Self::wipe(entry);
        }
        self.invalidate_evidence();
    }
}

/// A [`Block`] wrapper that keeps a shared [`RetainedWrites`] journal
/// truthful for the device it forwards to. See the module docs.
pub struct JournaledBlock<B: Block> {
    device: B,
    journal: Arc<SpinLock<RetainedWrites>>,
    pressure: &'static MemoryPressure,
}

impl<B: Block> JournaledBlock<B> {
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

impl<B: Block> Block for JournaledBlock<B> {
    /// The journalled device's own class: retaining writes changes what is
    /// remembered on a fault, never what the hardware is, so the real
    /// device's I/O budget survives this layer.
    fn device_class(&self) -> BlkDeviceClass {
        self.device.device_class()
    }

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

    fn flush(&mut self) -> Result<(), DriverError> {
        // Commit the device's volatile cache. On success every write this
        // journal was holding is now on the medium, so the retained set is
        // committed (emptied) — exactly the watermark path's discipline,
        // driven explicitly. A failure leaves the journal untouched: the
        // uncommitted set is still uncommitted, never treated as durable.
        self.device.flush()?;
        self.journal.lock().commit();
        Ok(())
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

    use tairix_reclaim::FreeMemorySource;

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

    /// An ample source that counts how often it was read. In production
    /// that reading is the physical frame allocator, so each one takes
    /// the global frame-allocator lock and the count per write is the
    /// cost the journal is held to.
    struct CountingSource {
        readings: core::sync::atomic::AtomicUsize,
    }
    impl FreeMemorySource for CountingSource {
        fn free_bytes(&self) -> usize {
            self.readings
                .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            1 << 30
        }
        fn total_bytes(&self) -> usize {
            1 << 30
        }
    }
    static COUNTING: CountingSource = CountingSource {
        readings: core::sync::atomic::AtomicUsize::new(0),
    };

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

    /// A memory source in the mild band: free memory is tightening, but the
    /// reserve floor is nowhere near.
    struct TighteningSource;
    impl FreeMemorySource for TighteningSource {
        fn free_bytes(&self) -> usize {
            // Just below the mild enter watermark (a fifth of the total).
            (1 << 30) / 5 - 4096
        }
        fn total_bytes(&self) -> usize {
            1 << 30
        }
    }
    static TIGHTENING: TighteningSource = TighteningSource;

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
    fn an_explicit_flush_commits_the_device_and_empties_the_journal() {
        // The `fs_sync` durability path: one write below the watermark
        // stays dirty until an explicit flush forces the device cache and
        // clears the journal.
        let (mut device, journal) = journaled(64, small_budget());
        device.write_blocks(0, &[1u8; BLOCK_SIZE as usize]).unwrap();
        assert!(journal.lock().is_dirty(), "write retained, not yet durable");
        assert_eq!(device.device.flushes, 0, "no flush issued below watermark");
        Block::flush(&mut device).unwrap();
        assert_eq!(device.device.flushes, 1, "explicit flush hit the device");
        assert!(!journal.lock().is_dirty(), "journal committed after flush");
    }

    #[test]
    fn an_explicit_flush_that_the_device_refuses_keeps_the_journal_dirty() {
        // Fail-closed: a refused device flush is never reported as
        // durability, and the uncommitted set stays uncommitted.
        let (mut device, journal) = journaled(64, small_budget());
        device.write_blocks(0, &[1u8; BLOCK_SIZE as usize]).unwrap();
        device.device.fail_flush = true;
        assert_eq!(Block::flush(&mut device), Err(DriverError::DeviceFault));
        assert!(
            journal.lock().is_dirty(),
            "journal still dirty after refusal"
        );
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
    fn a_tightening_band_alone_does_not_throw_away_uncommitted_writes() {
        // The journal is the only copy of data the medium may not hold, so it
        // is not a reclaimable cache and no band ceiling applies to it: what
        // bounds its growth is its own budget and the reserve floor. A band
        // that has merely begun tightening therefore keeps the retained set —
        // losing it would forfeit the surprise-removal report for no memory
        // the system could use.
        let journal = Arc::new(SpinLock::new(RetainedWrites::new(
            BLOCK_SIZE,
            small_budget(),
        )));
        let pressure: &'static MemoryPressure =
            Box::leak(Box::new(MemoryPressure::over(&TIGHTENING)));
        let mut device = JournaledBlock::new(RamFlushBlock::new(8), Arc::clone(&journal), pressure);
        device.device.fail_flush = true;
        device.write_blocks(0, &[1u8; BLOCK_SIZE as usize]).unwrap();
        let held = journal.lock();
        assert!(!held.is_lost(), "a tightening band retains the write");
        assert!(held.retained_bytes() > 0);
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
    fn one_write_takes_one_pressure_reading_however_many_blocks_it_retains() {
        // The regression: `record` asked the gauge per *device block* of
        // every write, and that reading is the physical frame allocator's
        // free-frame count — so a 16-block write took the global
        // frame-allocator lock 32 times to answer a question that cannot
        // change inside one write. One reading per write, drawn down per
        // block, is both cheaper and the stricter bound.
        let journal = Arc::new(SpinLock::new(RetainedWrites::new(
            BLOCK_SIZE,
            small_budget(),
        )));
        let pressure: &'static MemoryPressure =
            Box::leak(Box::new(MemoryPressure::over(&COUNTING)));
        let mut device =
            JournaledBlock::new(RamFlushBlock::new(64), Arc::clone(&journal), pressure);
        // The device refuses the opportunistic watermark flush, so the
        // whole write stays retained and its per-block admissions are
        // what the reading count is measured over.
        device.device.fail_flush = true;
        let blocks = 16usize;
        let before = COUNTING
            .readings
            .load(core::sync::atomic::Ordering::Relaxed);
        device
            .write_blocks(0, &vec![5u8; blocks * BLOCK_SIZE as usize])
            .unwrap();
        let after = COUNTING
            .readings
            .load(core::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            after - before,
            1,
            "the whole write is retained from one gauge reading"
        );
        {
            let held = journal.lock();
            assert!(!held.is_lost());
            assert_eq!(
                held.retained_bytes(),
                u64::from(BLOCK_SIZE) * blocks as u64,
                "every block of the write is retained"
            );
        }

        // Rewriting the same blocks admits nothing, so it takes no reading at
        // all — the allowance is drawn lazily, on the first admission.
        let repeat = COUNTING
            .readings
            .load(core::sync::atomic::Ordering::Relaxed);
        device
            .write_blocks(0, &vec![6u8; blocks * BLOCK_SIZE as usize])
            .unwrap();
        assert_eq!(
            COUNTING
                .readings
                .load(core::sync::atomic::Ordering::Relaxed),
            repeat,
            "a refresh-only write reads the gauge not at all"
        );
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

    /// A journal with a two-block evidence window seeded at LBA 0.
    fn journaled_with_evidence(
        blocks: u64,
    ) -> (JournaledBlock<RamFlushBlock>, Arc<SpinLock<RetainedWrites>>) {
        let (device, journal) = journaled(blocks, CacheBudget::from_backing(1 << 30));
        journal
            .lock()
            .set_evidence(0, &[0u8; 2 * BLOCK_SIZE as usize]);
        (device, journal)
    }

    #[test]
    fn evidence_accepts_committed_or_latest_per_block_and_nothing_else() {
        let (mut device, journal) = journaled_with_evidence(64);
        // Write into the second evidence block; the first stays as seeded.
        device.write_blocks(1, &[9u8; BLOCK_SIZE as usize]).unwrap();

        let seeded = [0u8; 2 * BLOCK_SIZE as usize];
        let mut written = seeded;
        written[BLOCK_SIZE as usize..].fill(9);
        // A mixed medium (block 0 committed, block 1 either state) passes.
        let journal_ref = journal.lock();
        assert!(
            journal_ref.verify_evidence(&seeded),
            "device lost the write"
        );
        assert!(
            journal_ref.verify_evidence(&written),
            "device kept the write"
        );
        // A foreign byte anywhere in the window is a mutation.
        let mut foreign = written;
        foreign[3] = 0xEE;
        assert!(!journal_ref.verify_evidence(&foreign));
        // A wrong-length window can prove nothing.
        assert!(!journal_ref.verify_evidence(&written[..BLOCK_SIZE as usize]));
    }

    #[test]
    fn a_commit_promotes_latest_evidence_to_committed() {
        let (mut device, journal) = journaled_with_evidence(64);
        device.write_blocks(0, &[7u8; BLOCK_SIZE as usize]).unwrap();
        journal.lock().commit();
        let mut window = [0u8; 2 * BLOCK_SIZE as usize];
        window[..BLOCK_SIZE as usize].fill(7);
        let journal = journal.lock();
        assert!(journal.verify_evidence(&window));
        // The pre-commit content is no longer an acceptable state: the
        // device confirmed the flush, so the medium must carry the write.
        assert!(!journal.verify_evidence(&[0u8; 2 * BLOCK_SIZE as usize]));
    }

    #[test]
    fn a_lost_journal_invalidates_its_evidence() {
        let (mut device, journal) = journaled_with_evidence(8);
        device.device.fail_writes = true;
        let _ = device.write_blocks(0, &[1u8; BLOCK_SIZE as usize]);
        let journal = journal.lock();
        assert!(journal.is_lost());
        assert_eq!(journal.evidence_window(), None);
        assert!(!journal.verify_evidence(&[0u8; 2 * BLOCK_SIZE as usize]));
    }

    #[test]
    fn a_discard_overlapping_the_window_invalidates_the_evidence() {
        let (mut device, journal) = journaled_with_evidence(64);
        device.discard(1, 4).unwrap();
        assert_eq!(journal.lock().evidence_window(), None);
        // A discard clear of the window leaves it intact.
        let (mut device, journal) = journaled_with_evidence(64);
        device.discard(2, 4).unwrap();
        assert_eq!(
            journal.lock().evidence_window(),
            Some((0, 2 * BLOCK_SIZE as usize))
        );
    }

    #[test]
    fn a_malformed_evidence_window_is_refused() {
        let (_, journal) = journaled(8, small_budget());
        let mut journal = journal.lock();
        journal.set_evidence(0, &[0u8; 100]); // not block-aligned
        assert_eq!(journal.evidence_window(), None);
        journal.set_evidence(0, &[]);
        assert_eq!(journal.evidence_window(), None);
    }

    #[test]
    fn the_replay_snapshot_clones_retained_blocks_in_lba_order() {
        let (mut device, journal) = journaled(64, CacheBudget::from_backing(1 << 30));
        device.write_blocks(5, &[5u8; BLOCK_SIZE as usize]).unwrap();
        device.write_blocks(2, &[2u8; BLOCK_SIZE as usize]).unwrap();
        let snapshot = journal.lock().retained_snapshot().expect("snapshot");
        let blocks = snapshot.blocks();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].0, 2);
        assert!(blocks[0].1.iter().all(|&b| b == 2));
        assert_eq!(blocks[1].0, 5);
        assert!(blocks[1].1.iter().all(|&b| b == 5));
    }

    #[test]
    fn reads_touch_nothing() {
        let (mut device, journal) = journaled(8, small_budget());
        let mut buf = [0u8; BLOCK_SIZE as usize];
        device.read_blocks(0, &mut buf).unwrap();
        assert!(!journal.lock().is_dirty());
    }
}
