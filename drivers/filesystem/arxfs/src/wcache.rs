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
//! A staged block is pinned memory, not reclaimable cache: it exists nowhere
//! else, so it can only be written, never dropped. The set performs no I/O —
//! the owning filesystem does, exactly as [`crate::pagecache`] — so it stays
//! free of the block device and unit-testable on the host.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use tairix_abi::DriverError;
use zeroize::Zeroize;

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

    /// Hand every staged block to `write` in ascending address order, wiping
    /// each buffer as it leaves the set.
    ///
    /// # Errors
    ///
    /// Whatever `write` reports, at which point the blocks not yet reached stay
    /// staged for the rollback to discard.
    pub(crate) fn drain<F>(&mut self, mut write: F) -> Result<(), DriverError>
    where
        F: FnMut(u64, &[u8]) -> Result<(), DriverError>,
    {
        while let Some((phys, mut bytes)) = self.entries.pop_first() {
            let outcome = write(phys, &bytes);
            bytes.zeroize();
            outcome?;
        }
        Ok(())
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

    fn block(fill: u8) -> [u8; BS] {
        [fill; BS]
    }

    #[test]
    fn a_rewrite_replaces_the_staged_block_rather_than_adding_one() {
        let mut set = DirtySet::new(BS);
        set.stage(7, &block(0xAA)).expect("stage");
        set.stage(7, &block(0xBB)).expect("restage");
        assert_eq!(set.len(), 1);
        let mut seen = alloc::vec::Vec::new();
        set.drain(|phys, bytes| {
            seen.push((phys, bytes[0]));
            Ok(())
        })
        .expect("drain");
        assert_eq!(seen, alloc::vec![(7, 0xBB)]);
    }

    #[test]
    fn the_drain_runs_in_ascending_device_order() {
        let mut set = DirtySet::new(BS);
        for (fill, phys) in [(1_u8, 9_u64), (2, 2), (3, 40), (4, 3)] {
            set.stage(phys, &block(fill)).expect("stage");
        }
        let mut order = alloc::vec::Vec::new();
        set.drain(|phys, _| {
            order.push(phys);
            Ok(())
        })
        .expect("drain");
        assert_eq!(order, alloc::vec![2, 3, 9, 40]);
        assert_eq!(set.len(), 0);
    }

    #[test]
    fn a_failing_write_leaves_the_blocks_it_did_not_reach_staged() {
        let mut set = DirtySet::new(BS);
        for phys in 0..4_u64 {
            set.stage(phys, &block(0)).expect("stage");
        }
        let mut written = 0_usize;
        let outcome = set.drain(|_, _| {
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
        let mut order = alloc::vec::Vec::new();
        set.drain(|phys, _| {
            order.push(phys);
            Ok(())
        })
        .expect("drain");
        assert_eq!(order, alloc::vec![5]);
    }
}
