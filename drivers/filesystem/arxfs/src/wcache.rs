//! The open transaction's dirty block set: where a sealed block waits between
//! the write path and the device.
//!
//! A copy-on-write filesystem rewrites the same metadata block many times
//! inside one transaction — every stored data block re-writes the extent leaf
//! that maps it, the spine above that leaf, and the transaction root — and only
//! the last version of each survives. Holding them here, keyed by physical
//! address, means the device is issued one write per block per transaction
//! rather than one per rewrite, and it gives the commit a point at which every
//! block the new root names has been sent and can be barriered ahead of the
//! superblock slot that publishes it.
//!
//! Holding them also lets the drain hand the device *runs* rather than blocks:
//! a transaction's data blocks are allocated consecutively and its mirrored
//! metadata blocks are adjacent pairs, so gathering the set's ascending order
//! into contiguous runs collapses a command and a completion wait per block
//! into one per run.
//!
//! A staged block is pinned memory, not reclaimable cache: it exists nowhere
//! else, so it can only be written, never dropped. The set performs no I/O —
//! the owning filesystem does, exactly as [`crate::pagecache`] — so it stays
//! free of the block device and unit-testable on the host.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use tairix_abi::DriverError;
use zeroize::Zeroize;

use crate::RunWindow;

/// Sealed blocks the open transaction has produced but not yet sent to the
/// device, keyed by physical address so a rewrite replaces in place and the
/// drain runs in ascending device order.
pub(crate) struct DirtySet {
    entries: BTreeMap<u64, Vec<u8>>,
    block_size: usize,
}

impl DirtySet {
    /// An empty set over blocks of `block_size`. Allocates nothing until the
    /// first block is staged.
    pub(crate) const fn new(block_size: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            block_size,
        }
    }

    /// Stage the sealed block `bytes` at `phys`, replacing any version already
    /// held for that address.
    ///
    /// # Errors
    ///
    /// [`DriverError::BufferTooSmall`] when `bytes` is shorter than a block,
    /// [`DriverError::NoSpace`] when a first-time entry cannot be allocated.
    pub(crate) fn stage(&mut self, phys: u64, bytes: &[u8]) -> Result<(), DriverError> {
        let block = bytes
            .get(..self.block_size)
            .ok_or(DriverError::BufferTooSmall)?;
        if let Some(held) = self.entries.get_mut(&phys) {
            held.copy_from_slice(block);
            return Ok(());
        }
        let mut held = Vec::new();
        held.try_reserve_exact(self.block_size)
            .map_err(|_| DriverError::NoSpace)?;
        held.extend_from_slice(block);
        self.entries.insert(phys, held);
        Ok(())
    }

    /// How many of the `blocks` addresses from `phys` are staged.
    pub(crate) fn staged_in(&self, phys: u64, blocks: u64) -> u64 {
        let end = phys.saturating_add(blocks);
        self.entries.range(phys..end).count() as u64
    }

    /// Overlay every staged block of the run starting at `phys` onto `run`,
    /// which holds that run's bytes as the device has them.
    ///
    /// # Errors
    ///
    /// [`DriverError::BufferTooSmall`] when `run` is not a whole number of
    /// blocks.
    pub(crate) fn overlay(&self, phys: u64, run: &mut [u8]) -> Result<(), DriverError> {
        if !run.len().is_multiple_of(self.block_size) {
            return Err(DriverError::BufferTooSmall);
        }
        let blocks = (run.len() / self.block_size) as u64;
        for (at, bytes) in self.entries.range(phys..phys.saturating_add(blocks)) {
            let index = usize::try_from(at - phys).map_err(|_| DriverError::LengthOutOfRange)?;
            let off = index
                .checked_mul(self.block_size)
                .ok_or(DriverError::LengthOutOfRange)?;
            let slot = run
                .get_mut(off..off + self.block_size)
                .ok_or(DriverError::BufferTooSmall)?;
            slot.copy_from_slice(bytes);
        }
        Ok(())
    }

    /// Hand every staged block to `write` in ascending address order, gathering
    /// adjacent ones into runs of at most `run_bytes` and wiping each buffer as
    /// it leaves the set.
    ///
    /// `write` receives a run's start address and the whole of its payload: a
    /// positive whole number of blocks the set holds at consecutive addresses.
    /// A mirrored metadata block is two entries with identical bytes one apart,
    /// so it leaves as one two-block request. A run stops at the first address
    /// the set does not hold, so it never names a block outside the
    /// transaction, and at the gather window's end, so it never exceeds the
    /// device's transfer bound. With no window to gather into — a machine too
    /// short of memory to reserve one — each block is written straight from the
    /// set, so a commit costs more requests rather than failing.
    ///
    /// # Errors
    ///
    /// Whatever `write` reports, at which point the blocks past the failed run
    /// stay staged for the rollback to discard.
    pub(crate) fn drain<F>(&mut self, run_bytes: usize, mut write: F) -> Result<(), DriverError>
    where
        F: FnMut(u64, &[u8]) -> Result<(), DriverError>,
    {
        let bound = run_bytes / self.block_size * self.block_size;
        let mut window = RunWindow::new(self.longest_run_bytes(bound));
        while let Some((&start, _)) = self.entries.first_key_value() {
            let (blocks, outcome) = match window.buf() {
                Some(buf) => {
                    let (blocks, run) = self.gather(start, buf)?;
                    (blocks, write(start, run))
                }
                None => match self.entries.get(&start) {
                    Some(block) => (1, write(start, block)),
                    None => (1, Err(DriverError::BufferTooSmall)),
                },
            };
            self.release(start, blocks);
            outcome?;
        }
        Ok(())
    }

    /// Bytes the longest run of adjacent staged blocks occupies, capped at
    /// `bound`.
    ///
    /// The gather window is reserved for what the set actually holds rather
    /// than for the transfer bound, so a metadata-only transaction stages a
    /// handful of blocks instead of the whole window — the same sizing the read
    /// path applies to a small read.
    fn longest_run_bytes(&self, bound: usize) -> usize {
        let mut longest = 0usize;
        let mut run = 0usize;
        let mut want = None;
        for &at in self.entries.keys() {
            run = if want == Some(at) { run + 1 } else { 1 };
            longest = longest.max(run);
            want = Some(at.saturating_add(1));
        }
        longest.saturating_mul(self.block_size).min(bound)
    }

    /// Copy the run of adjacent staged blocks starting at `start` into
    /// `window`, returning how many blocks it holds and their payload.
    ///
    /// One pass both decides the run's length and lays its bytes out, so the
    /// length the set then releases and the bytes the device is handed cannot
    /// disagree.
    ///
    /// # Errors
    ///
    /// [`DriverError::BufferTooSmall`] when `window` cannot hold one block, the
    /// set holds none at `start`, or a staged entry is not a whole block.
    fn gather<'w>(
        &self,
        start: u64,
        window: &'w mut [u8],
    ) -> Result<(usize, &'w [u8]), DriverError> {
        let mut want = start;
        let mut span = 0usize;
        for (&at, bytes) in self.entries.range(start..) {
            if at != want {
                break;
            }
            let Some(end) = span.checked_add(self.block_size) else {
                break;
            };
            let Some(slot) = window.get_mut(span..end) else {
                break;
            };
            slot.copy_from_slice(
                bytes
                    .get(..self.block_size)
                    .ok_or(DriverError::BufferTooSmall)?,
            );
            span = end;
            want = want.saturating_add(1);
        }
        if span == 0 {
            return Err(DriverError::BufferTooSmall);
        }
        let run = window.get(..span).ok_or(DriverError::BufferTooSmall)?;
        Ok((span / self.block_size, run))
    }

    /// Drop and wipe the `blocks` staged blocks from `start`, whose bytes have
    /// left the set.
    fn release(&mut self, start: u64, blocks: usize) {
        let mut at = start;
        for _ in 0..blocks {
            if let Some(mut bytes) = self.entries.remove(&at) {
                bytes.zeroize();
            }
            at = at.saturating_add(1);
        }
    }

    /// Drop the block staged at `phys`, if any: its bytes name a block nothing
    /// will reference, so writing them out would cost a device command for
    /// contents no reader can reach.
    pub(crate) fn discard(&mut self, phys: u64) {
        if let Some(mut bytes) = self.entries.remove(&phys) {
            bytes.zeroize();
        }
    }

    /// Drop every staged block. The transaction that produced them did not
    /// publish, so nothing referenced them.
    pub(crate) fn clear(&mut self) {
        while let Some((_, mut bytes)) = self.entries.pop_first() {
            bytes.zeroize();
        }
    }

    /// Blocks currently staged.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

impl Drop for DirtySet {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BS: usize = 512;

    /// Gather bound wide enough that a test's runs are never window-bounded,
    /// so a test that means to measure adjacency does not measure the bound.
    const WIDE: usize = 64 * BS;

    fn block(fill: u8) -> [u8; BS] {
        [fill; BS]
    }

    /// A block whose every byte names the address it is staged at, so a
    /// gathered run's contents can be checked against the blocks it came from.
    fn tagged(phys: u64) -> [u8; BS] {
        block(u8::try_from(phys).expect("a test address fits a byte"))
    }

    /// Every request the drain issued: start address, block count, and the
    /// fill byte of each block it carried.
    fn requests(set: &mut DirtySet, run_bytes: usize) -> Vec<(u64, usize, Vec<u8>)> {
        let mut seen = Vec::new();
        set.drain(run_bytes, |phys, run| {
            seen.push((
                phys,
                run.len() / BS,
                run.chunks(BS).map(|block| block[0]).collect(),
            ));
            Ok(())
        })
        .expect("drain");
        seen
    }

    #[test]
    fn a_rewrite_replaces_the_staged_block_rather_than_adding_one() {
        let mut set = DirtySet::new(BS);
        set.stage(7, &block(0xAA)).expect("stage");
        set.stage(7, &block(0xBB)).expect("restage");
        assert_eq!(set.len(), 1);
        assert_eq!(
            requests(&mut set, WIDE),
            alloc::vec![(7, 1, alloc::vec![0xBB])]
        );
    }

    #[test]
    fn the_drain_runs_in_ascending_device_order() {
        let mut set = DirtySet::new(BS);
        for (fill, phys) in [(1_u8, 9_u64), (2, 2), (3, 40), (4, 3)] {
            set.stage(phys, &block(fill)).expect("stage");
        }
        // 2 and 3 are adjacent and leave together; 9 and 40 stand alone.
        assert_eq!(
            requests(&mut set, WIDE),
            alloc::vec![
                (2, 2, alloc::vec![2, 4]),
                (9, 1, alloc::vec![1]),
                (40, 1, alloc::vec![3]),
            ]
        );
        assert_eq!(set.len(), 0);
    }

    #[test]
    fn adjacent_blocks_leave_as_one_request_and_a_gap_ends_the_run() {
        let mut set = DirtySet::new(BS);
        for phys in [10_u64, 11, 12, 14, 15] {
            set.stage(phys, &tagged(phys)).expect("stage");
        }
        assert_eq!(
            requests(&mut set, WIDE),
            alloc::vec![
                (10, 3, alloc::vec![10, 11, 12]),
                (14, 2, alloc::vec![14, 15]),
            ],
            "a run must cover only addresses the set holds"
        );
    }

    #[test]
    fn a_run_is_bounded_by_the_gather_window() {
        let mut set = DirtySet::new(BS);
        for phys in 0..5_u64 {
            set.stage(phys, &tagged(phys)).expect("stage");
        }
        // Five adjacent blocks under a two-block bound: 2 + 2 + 1.
        let issued = requests(&mut set, 2 * BS);
        assert_eq!(
            issued
                .iter()
                .map(|&(phys, blocks, _)| (phys, blocks))
                .collect::<Vec<_>>(),
            alloc::vec![(0, 2), (2, 2), (4, 1)]
        );
    }

    #[test]
    fn with_no_gather_window_each_block_is_written_from_the_set() {
        let mut set = DirtySet::new(BS);
        for phys in 0..3_u64 {
            set.stage(phys, &tagged(phys)).expect("stage");
        }
        // A bound below one block leaves nothing to gather into, which is the
        // path a refused window allocation takes.
        assert_eq!(
            requests(&mut set, BS - 1),
            alloc::vec![
                (0, 1, alloc::vec![0]),
                (1, 1, alloc::vec![1]),
                (2, 1, alloc::vec![2]),
            ]
        );
        assert_eq!(set.len(), 0);
    }

    #[test]
    fn a_failing_write_leaves_the_blocks_it_did_not_reach_staged() {
        let mut set = DirtySet::new(BS);
        for phys in [0_u64, 2, 4, 6] {
            set.stage(phys, &block(0)).expect("stage");
        }
        let mut written = 0_usize;
        let outcome = set.drain(WIDE, |_, _| {
            written += 1;
            if written == 2 {
                return Err(DriverError::DeviceFault);
            }
            Ok(())
        });
        assert_eq!(outcome, Err(DriverError::DeviceFault));
        assert_eq!(written, 2);
        assert_eq!(set.len(), 2, "the untouched blocks stay for the rollback");
    }

    #[test]
    fn the_overlay_replaces_only_the_staged_blocks_of_a_run() {
        let mut set = DirtySet::new(BS);
        set.stage(11, &block(0xCC)).expect("stage");
        let mut run = alloc::vec![0x11_u8; 3 * BS];
        set.overlay(10, &mut run).expect("overlay");
        assert_eq!(run[0], 0x11);
        assert_eq!(run[BS], 0xCC);
        assert_eq!(run[2 * BS], 0x11);
        assert_eq!(set.staged_in(10, 3), 1);
        assert_eq!(set.staged_in(12, 3), 0);
    }

    #[test]
    fn a_short_or_ragged_buffer_is_refused_rather_than_truncated() {
        let mut set = DirtySet::new(BS);
        assert_eq!(
            set.stage(1, &[0u8; BS - 1]),
            Err(DriverError::BufferTooSmall)
        );
        assert_eq!(
            set.overlay(0, &mut [0u8; BS + 1]),
            Err(DriverError::BufferTooSmall)
        );
    }

    #[test]
    fn discarding_a_reclaimed_block_drops_it_from_the_drain() {
        let mut set = DirtySet::new(BS);
        set.stage(4, &block(1)).expect("stage");
        set.stage(5, &block(2)).expect("stage");
        set.discard(4);
        set.discard(99);
        assert_eq!(
            requests(&mut set, WIDE),
            alloc::vec![(5, 1, alloc::vec![2])],
            "a discarded block must not rejoin its neighbour's run"
        );
    }
}
