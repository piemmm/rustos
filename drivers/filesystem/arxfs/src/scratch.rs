//! Transient on-disk scratch arrays: where a whole-volume pass puts the truth
//! it derives (`docs/src/filesystem/arxfs-spec.md` §12).
//!
//! A pass that must decide something about *every* block or *every* inode —
//! how many extents claim a physical block, which inodes the directory tree
//! reaches, how many names each inode has — needs somewhere to accumulate one
//! small value per index. In RAM that state is proportional to the volume, so
//! on the machine the charter requires a 100 TB volume to be served from it
//! cannot exist at all. Here it is a flat array of fixed-width elements in a
//! contiguous run of the volume's own free space, paged through the shared
//! bounded cache ([`crate::pagecache`]), so the pass holds a fixed handful of
//! blocks whatever the volume's size.
//!
//! The array is **scratch, not metadata**: nothing outside the pass that
//! allocated it ever reads it, it is released before that pass returns, and a
//! crash simply leaves stale bytes in blocks the next mount finds free (free
//! space is derived from the authoritative trees, so an interrupted pass
//! leaks nothing). It is therefore single-copy and updated in place, exactly
//! like the allocation map, and never copy-on-written.
//!
//! Every page is still **sealed** with the ordinary keyed block header, and
//! the pass **writes every page before it reads any**: the array's contents
//! drive corrections to authoritative refcounts, so a page a hostile or
//! failing device substituted must be rejected rather than believed, and
//! "unauthenticated bytes read as zero" would be exactly the fail-open path
//! that allows. A page that does not authenticate at its own address, under
//! this array's owner, is a device fault.
//!
//! # Windows
//!
//! The run a volume can actually provide is not always the whole index space:
//! a nearly-full or fragmented volume has no room for it, and an online pass
//! must not take more than a small share of the free space it is sharing with
//! live writes. So an array covers a **window** of the index space and the
//! pass repeats over successive windows — trading one extra metadata walk per
//! window for a scratch run the volume can spare. Beyond
//! [`MAX_RECONCILE_WINDOWS`] the trade stops being worth it and the caller is
//! told it has no array, so it reports honestly rather than embarking on a
//! pass that will not finish.

use tairix_abi::driver::block::Block;
use tairix_abi::DriverError;

use crate::allocmap::{bit_get, bit_set};
use crate::header::{BlockHeader, BlockType, ReservedOwner, HEADER_LEN};
use crate::pagecache::{page_payload_len, BlockCache};
use crate::superblock::RING_BLOCKS;
use crate::{as_usize, ARXFS, MAX_BLOCK_SIZE, METADATA_RESERVE};

/// Share of the volume's free space one scratch array may occupy.
///
/// An online pass allocates its array from the same free space live writes are
/// competing for, so it takes at most an eighth of it and leaves the rest to
/// the workload: a verification pass must never be the reason a write fails
/// for want of space.
const SCRATCH_FREE_SHARE: u64 = 8;

/// Windows a pass will divide its index space into before it gives up and
/// reports that it could not place its array.
///
/// Each window costs one more walk of the metadata the pass streams, so the
/// ceiling bounds that multiplier. Eight windows over the claim array of a
/// 100 TB volume of 4 KiB blocks is a 1.6 GB run — a hundredth of a per-cent
/// of the device — so a volume with no room for that has none to spare for a
/// pass either.
pub(crate) const MAX_RECONCILE_WINDOWS: u64 = 8;

/// How many bits one element of a scratch array occupies.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum ElementWidth {
    /// One bit: a set membership, such as "the directory tree reaches this
    /// inode".
    Bit,
    /// Four bits, saturating at 15: a claim count. A legal refcount never
    /// exceeds the reverse-reference cap ([`crate::dedupe::REVERSE_REF_CAP`]),
    /// so four bits count every lawful value exactly and still leave a
    /// distinct "more claims than any volume should have" state above them.
    Nibble,
    /// Thirty-two bits: an inode number or a name count, both of which are
    /// `u32` in the on-disk inode.
    Word,
}

impl ElementWidth {
    /// Elements one page holds.
    const fn per_page(self, block_size: usize) -> u64 {
        let payload = page_payload_len(block_size) as u64;
        match self {
            Self::Bit => payload * 8,
            Self::Nibble => payload * 2,
            Self::Word => payload / 4,
        }
    }

    /// The largest value an element can hold; an increment stops here rather
    /// than wrapping, so a saturated element states "at least this many"
    /// instead of a wildly wrong count.
    pub(crate) const fn ceiling(self) -> u32 {
        match self {
            Self::Bit => 1,
            Self::Nibble => 15,
            Self::Word => u32::MAX,
        }
    }

    /// Read element `slot` of a page payload.
    fn get(self, payload: &[u8], slot: u64) -> u32 {
        match self {
            Self::Bit => u32::from(bit_get(payload, slot)),
            Self::Nibble => {
                let byte = as_usize(slot / 2);
                let raw = payload.get(byte).copied().unwrap_or(0);
                u32::from(if slot.is_multiple_of(2) {
                    raw & 0x0F
                } else {
                    raw >> 4
                })
            }
            Self::Word => {
                let at = as_usize(slot * 4);
                match payload.get(at..at + 4) {
                    Some(bytes) => {
                        let mut raw = [0u8; 4];
                        raw.copy_from_slice(bytes);
                        u32::from_le_bytes(raw)
                    }
                    None => 0,
                }
            }
        }
    }

    /// Write element `slot` of a page payload, clamped to [`Self::ceiling`].
    fn set(self, payload: &mut [u8], slot: u64, value: u32) {
        let value = value.min(self.ceiling());
        match self {
            Self::Bit => {
                bit_set(payload, slot, value != 0);
            }
            Self::Nibble => {
                let byte = as_usize(slot / 2);
                if let Some(raw) = payload.get_mut(byte) {
                    let nibble = (value & 0x0F) as u8;
                    *raw = if slot.is_multiple_of(2) {
                        (*raw & 0xF0) | nibble
                    } else {
                        (*raw & 0x0F) | (nibble << 4)
                    };
                }
            }
            Self::Word => {
                let at = as_usize(slot * 4);
                if let Some(bytes) = payload.get_mut(at..at + 4) {
                    bytes.copy_from_slice(&value.to_le_bytes());
                }
            }
        }
    }
}

/// One transient array: where it sits on the device, which slice of the index
/// space it currently covers, and its resident pages.
pub(crate) struct ScratchArray {
    role: ReservedOwner,
    /// First device block of the run.
    start: u64,
    /// Device blocks the run occupies.
    blocks: u64,
    width: ElementWidth,
    /// Elements one page holds.
    per_page: u64,
    /// First index of the window this array currently covers.
    base: u64,
    /// Indices the whole pass must cover, across every window.
    total: u64,
    cache: BlockCache,
}

impl ScratchArray {
    /// Indices one window covers.
    pub(crate) fn span(&self) -> u64 {
        self.blocks * self.per_page
    }

    /// First index of the current window.
    pub(crate) fn base(&self) -> u64 {
        self.base
    }

    /// One past the last index of the current window, never past the index
    /// space itself.
    pub(crate) fn window_end(&self) -> u64 {
        self.base.saturating_add(self.span()).min(self.total)
    }

    /// Whether `index` falls in the window this array currently covers.
    pub(crate) fn covers(&self, index: u64) -> bool {
        index >= self.base && index < self.window_end()
    }

    /// The region page and slot holding `index`, or `None` when the index is
    /// outside the current window.
    fn locate(&self, index: u64) -> Option<(u64, u64)> {
        if !self.covers(index) {
            return None;
        }
        let offset = index - self.base;
        Some((offset / self.per_page, offset % self.per_page))
    }
}

impl<B: Block> ARXFS<B> {
    /// Place a scratch array of `elements` indices, or report that the volume
    /// cannot spare a run for one.
    ///
    /// `max_windows` is how many passes over the index space the caller is
    /// willing to make: a structure it reads at random (a reachability bitmap,
    /// a work queue) must have its whole space at once and passes `1`, while a
    /// streamed one accepts up to [`MAX_RECONCILE_WINDOWS`] and gets a run the
    /// volume can actually spare.
    ///
    /// `Ok(None)` is the honest "no array" answer, not an error: a read-only
    /// handle holds no allocator at all, and a nearly-full or badly fragmented
    /// volume may have no run to give. A caller that gets it reports what it
    /// could not verify rather than pretending it did.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] on an unrecoverable error while reserving
    /// or initialising the run (never a panic).
    pub(crate) fn scratch_alloc(
        &mut self,
        role: ReservedOwner,
        width: ElementWidth,
        elements: u64,
        max_windows: u64,
    ) -> Result<Option<ScratchArray>, DriverError> {
        let per_page = width.per_page(self.block_size);
        if elements == 0 || per_page == 0 || self.allocator().is_err() {
            return Ok(None);
        }
        let want = elements.div_ceil(per_page);
        let share = self.free_count / SCRATCH_FREE_SHARE;
        let mut windows = 1;
        while windows <= max_windows.min(MAX_RECONCILE_WINDOWS) {
            let run = want.div_ceil(windows).max(1);
            windows *= 2;
            if run > share {
                continue;
            }
            if let Some(array) = self.scratch_place(role, width, elements, run)? {
                return Ok(Some(array));
            }
        }
        Ok(None)
    }

    /// Reserve and clear a run of exactly `run` blocks for an array of
    /// `elements` indices, or report that no such run is free.
    ///
    /// [`Self::scratch_alloc`] is the policy over this: which run lengths to
    /// try, and in what order.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] on an unrecoverable error while reserving
    /// or initialising the run (never a panic).
    pub(crate) fn scratch_place(
        &mut self,
        role: ReservedOwner,
        width: ElementWidth,
        elements: u64,
        run: u64,
    ) -> Result<Option<ScratchArray>, DriverError> {
        let per_page = width.per_page(self.block_size);
        if elements == 0 || per_page == 0 || run == 0 {
            return Ok(None);
        }
        // The metadata reserve is what lets a shrinking transaction
        // copy-on-write itself on a full volume; a verification pass must not
        // eat into it.
        if self.free_count.saturating_sub(run) <= METADATA_RESERVE {
            return Ok(None);
        }
        let Some(start) = self.map_find_free_run(run, RING_BLOCKS, self.total_blocks)? else {
            return Ok(None);
        };
        self.mark_range_used(start, run)?;
        let mut array = ScratchArray {
            role,
            start,
            blocks: run,
            width,
            per_page,
            base: 0,
            total: elements,
            cache: BlockCache::new(self.block_size),
        };
        if let Err(err) = self.scratch_zero(&mut array) {
            // Handing the run back cannot make the failure worse: free space is
            // derived from the authoritative trees, so a release that fails too
            // is corrected by the next mount's rebuild.
            let _ = self.mark_range_free(start, run);
            return Err(err);
        }
        Ok(Some(array))
    }

    /// Move `array` onto the window starting at `base` and clear it, so the
    /// next window of a multi-window pass starts from zeroes.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] on an unrecoverable write while clearing
    /// the run.
    pub(crate) fn scratch_rebase(
        &mut self,
        array: &mut ScratchArray,
        base: u64,
    ) -> Result<(), DriverError> {
        array.base = base;
        self.scratch_zero(array)
    }

    /// Hand the array's run back to the allocator. Resident changes are
    /// dropped unwritten: nothing outside the finished pass may read them.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] on an unrecoverable error while releasing
    /// the run in the allocation map.
    pub(crate) fn scratch_release(&mut self, mut array: ScratchArray) -> Result<(), DriverError> {
        array.cache.clear();
        self.mark_range_free(array.start, array.blocks)
    }

    /// The element at `index`, or `0` for an index outside the current window.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] when a page does not authenticate at its
    /// own address under this array's owner (fail closed).
    pub(crate) fn scratch_get(
        &mut self,
        array: &mut ScratchArray,
        index: u64,
    ) -> Result<u32, DriverError> {
        let Some((page, slot)) = array.locate(index) else {
            return Ok(0);
        };
        self.scratch_page_load(array, page)?;
        let width = array.width;
        let payload = array.cache.read(page).ok_or(DriverError::DeviceFault)?;
        Ok(width.get(payload, slot))
    }

    /// Store `value` at `index`, clamped to the element width's ceiling. An
    /// index outside the current window is ignored.
    ///
    /// # Errors
    ///
    /// As [`Self::scratch_get`].
    pub(crate) fn scratch_set(
        &mut self,
        array: &mut ScratchArray,
        index: u64,
        value: u32,
    ) -> Result<(), DriverError> {
        let Some((page, slot)) = array.locate(index) else {
            return Ok(());
        };
        self.scratch_page_load(array, page)?;
        let width = array.width;
        let payload = array.cache.write(page).ok_or(DriverError::DeviceFault)?;
        width.set(payload, slot, value);
        Ok(())
    }

    /// Add one to the element at `index`, saturating at the width's ceiling,
    /// and return the new value. An index outside the window returns `0`.
    ///
    /// # Errors
    ///
    /// As [`Self::scratch_get`].
    pub(crate) fn scratch_bump(
        &mut self,
        array: &mut ScratchArray,
        index: u64,
    ) -> Result<u32, DriverError> {
        let Some((page, slot)) = array.locate(index) else {
            return Ok(0);
        };
        self.scratch_page_load(array, page)?;
        let width = array.width;
        let payload = array.cache.write(page).ok_or(DriverError::DeviceFault)?;
        let next = width.get(payload, slot).saturating_add(1);
        width.set(payload, slot, next);
        Ok(next.min(width.ceiling()))
    }

    /// Seal every page of the array's run as zeroes.
    ///
    /// This is what lets the array be read without a fail-open path: after it,
    /// every page in the run authenticates as ours, so a page that does not is
    /// a fault rather than an empty slot. The run is a hundredth of a per-cent
    /// of the volume the pass then reads in full, so writing it out beats
    /// carrying a resident record of which pages have been touched — which
    /// would be proportional to the volume all over again.
    fn scratch_zero(&mut self, array: &mut ScratchArray) -> Result<(), DriverError> {
        array.cache.clear();
        let bs = self.block_size;
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        for page in 0..array.blocks {
            for byte in &mut buf[..bs] {
                *byte = 0;
            }
            self.scratch_seal(array, page, &mut buf)?;
            self.write_block(array.start + page, &buf[..bs])?;
        }
        Ok(())
    }

    /// Make sure region page `page` is resident, writing back whatever the
    /// cache must evict to make room.
    fn scratch_page_load(
        &mut self,
        array: &mut ScratchArray,
        page: u64,
    ) -> Result<(), DriverError> {
        if array.cache.contains(page) {
            return Ok(());
        }
        if let Some((victim, dirty)) = array.cache.eviction_candidate() {
            if dirty {
                self.scratch_page_write(array, victim)?;
            }
            array.cache.remove(victim);
        }
        let bs = self.block_size;
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        let phys = array.start + page;
        self.read_block(phys, &mut buf)?;
        let header = BlockHeader::decode_verify(
            &buf[..bs],
            BlockType::Scratch,
            self.fs_uuid,
            phys,
            &self.mac_key,
        )?;
        if header.owner != array.role.sentinel() || header.logical_addr != page {
            return Err(DriverError::DeviceFault);
        }
        array.cache.insert_clean(page, &buf[HEADER_LEN..bs]);
        Ok(())
    }

    /// Write one resident page back to its run.
    fn scratch_page_write(
        &mut self,
        array: &mut ScratchArray,
        page: u64,
    ) -> Result<(), DriverError> {
        let bs = self.block_size;
        let mut buf = [0u8; MAX_BLOCK_SIZE];
        let payload = array.cache.peek(page).ok_or(DriverError::DeviceFault)?;
        let len = payload.len().min(bs - HEADER_LEN);
        buf[HEADER_LEN..HEADER_LEN + len].copy_from_slice(&payload[..len]);
        self.scratch_seal(array, page, &mut buf)?;
        self.write_block(array.start + page, &buf[..bs])?;
        array.cache.mark_written(page);
        Ok(())
    }

    /// Stamp and authenticate one region page in `buf`.
    fn scratch_seal(
        &self,
        array: &ScratchArray,
        page: u64,
        buf: &mut [u8],
    ) -> Result<(), DriverError> {
        let bs = self.block_size;
        let header = BlockHeader {
            block_type: BlockType::Scratch,
            fs_uuid: self.fs_uuid,
            owner: array.role.sentinel(),
            generation: self.generation,
            logical_addr: page,
            physical_addr: array.start + page,
            payload_len: crate::as_u32(page_payload_len(bs)),
        };
        header.seal(&mut buf[..bs], &self.mac_key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BS: usize = 4096;
    const PAYLOAD: usize = page_payload_len(BS);

    #[test]
    fn each_width_packs_its_elements_into_a_page() {
        assert_eq!(ElementWidth::Bit.per_page(BS), (PAYLOAD * 8) as u64);
        assert_eq!(ElementWidth::Nibble.per_page(BS), (PAYLOAD * 2) as u64);
        assert_eq!(ElementWidth::Word.per_page(BS), (PAYLOAD / 4) as u64);
    }

    #[test]
    fn a_bit_element_round_trips() {
        let mut payload = [0u8; 8];
        ElementWidth::Bit.set(&mut payload, 5, 1);
        assert_eq!(ElementWidth::Bit.get(&payload, 5), 1);
        assert_eq!(ElementWidth::Bit.get(&payload, 6), 0);
        ElementWidth::Bit.set(&mut payload, 5, 0);
        assert_eq!(ElementWidth::Bit.get(&payload, 5), 0);
    }

    #[test]
    fn nibble_elements_share_a_byte_without_disturbing_each_other() {
        let mut payload = [0u8; 4];
        ElementWidth::Nibble.set(&mut payload, 0, 9);
        ElementWidth::Nibble.set(&mut payload, 1, 4);
        assert_eq!(ElementWidth::Nibble.get(&payload, 0), 9);
        assert_eq!(ElementWidth::Nibble.get(&payload, 1), 4);
        assert_eq!(payload[0], 0x49);
        ElementWidth::Nibble.set(&mut payload, 0, 2);
        assert_eq!(ElementWidth::Nibble.get(&payload, 1), 4);
    }

    #[test]
    fn a_nibble_saturates_rather_than_wrapping() {
        let mut payload = [0u8; 2];
        ElementWidth::Nibble.set(&mut payload, 0, 4_000);
        assert_eq!(ElementWidth::Nibble.get(&payload, 0), 15);
    }

    #[test]
    fn a_word_element_round_trips_little_endian() {
        let mut payload = [0u8; 8];
        ElementWidth::Word.set(&mut payload, 1, 0x0102_0304);
        assert_eq!(ElementWidth::Word.get(&payload, 1), 0x0102_0304);
        assert_eq!(&payload[4..8], &[0x04, 0x03, 0x02, 0x01]);
    }

    #[test]
    fn an_element_past_the_payload_reads_zero_and_refuses_to_change() {
        let mut payload = [0u8; 2];
        ElementWidth::Word.set(&mut payload, 4, 7);
        assert_eq!(ElementWidth::Word.get(&payload, 4), 0);
        ElementWidth::Nibble.set(&mut payload, 64, 7);
        assert_eq!(ElementWidth::Nibble.get(&payload, 64), 0);
    }

    /// A one-page window over a wider index space locates only its own
    /// elements, so a pass that streams the whole space can be trusted to skip
    /// what the current window does not hold.
    #[test]
    fn a_window_covers_only_its_own_slice_of_the_index_space() {
        let per_page = ElementWidth::Nibble.per_page(BS);
        let array = ScratchArray {
            role: ReservedOwner::ScratchClaims,
            start: 100,
            blocks: 1,
            width: ElementWidth::Nibble,
            per_page,
            base: per_page,
            total: per_page * 3 + 5,
            cache: BlockCache::new(BS),
        };
        assert_eq!(array.span(), per_page);
        assert!(!array.covers(per_page - 1));
        assert!(array.covers(per_page));
        assert!(array.covers(2 * per_page - 1));
        assert!(!array.covers(2 * per_page));
        assert_eq!(array.locate(per_page), Some((0, 0)));
        assert_eq!(array.locate(per_page + 3), Some((0, 3)));
        assert_eq!(array.locate(0), None);
    }

    /// The last window stops at the index space's end, so a pass never counts
    /// an index the volume does not have.
    #[test]
    fn the_last_window_ends_at_the_index_space() {
        let per_page = ElementWidth::Bit.per_page(BS);
        let array = ScratchArray {
            role: ReservedOwner::ScratchReachable,
            start: 8,
            blocks: 1,
            width: ElementWidth::Bit,
            per_page,
            base: per_page,
            total: per_page + 10,
            cache: BlockCache::new(BS),
        };
        assert_eq!(array.window_end(), per_page + 10);
        assert!(array.covers(per_page + 9));
        assert!(!array.covers(per_page + 10));
    }
}
