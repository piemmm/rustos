//! The on-disk paged allocation map: its layout and the bit and summary
//! arithmetic over it (`docs/src/filesystem/arxfs-spec.md` §4).
//!
//! Free space is *rebuildable* metadata, so unlike the inode, extent, chunk,
//! and reverse-reference trees the map is not copy-on-written. It lives in a
//! contiguous region of its own, is updated **in place**, and carries a
//! clean/dirty stamp naming the transaction generation whose allocation state
//! it fully reflects. A mount whose committed transaction root names this
//! region, and whose stamp is clean at that generation, adopts the map after
//! reading a single block; anything else — a stale stamp, a dirty stamp, a
//! block that does not authenticate — rebuilds the map from the authoritative
//! trees and rewrites it. Because the map is never authoritative, that
//! fallback is always available, which is what makes in-place update safe and
//! sidesteps the self-allocation problem a copy-on-written free-space tree
//! would have.
//!
//! Mount therefore costs a fixed number of reads instead of a walk over every
//! tree node, inode, and extent, and the resident footprint is a bounded page
//! cache rather than a set holding every used block — so several 100 TB+
//! volumes mount together on a small machine.
//!
//! # Region layout
//!
//! ```text
//! start + 0                       header    identity, coverage, clean stamp
//! start + 1 ..                    summary   free-block count per bitmap page
//! start + 1 + summary_blocks ..   bitmap    one bit per device block
//! ```
//!
//! The summary is what keeps allocation off a linear walk of a near-full
//! volume: a page whose recorded free count is zero holds nothing to allocate
//! and is skipped without being read, and a page whose count equals its
//! capacity is entirely free, so it is *synthesised* as zeroes rather than
//! read. The latter also means a freshly formatted volume writes only its
//! summary and the few pages that carry used bits, never a bitmap page per
//! terabyte.
//!
//! Every region block is sealed with the ordinary keyed block header, so a
//! torn, stale, misdirected, or bit-rotted page is detected and turned into a
//! rebuild rather than silently trusted. The region is deliberately **not**
//! mirrored: a second copy would only protect state the authoritative trees
//! can already reproduce.
//!
//! This module is pure layout and arithmetic. The region pages through the
//! shared bounded cache ([`crate::pagecache`]), and the block I/O that fills,
//! flushes, and stamps it lives in [`crate::allocator`].

use tairix_abi::DriverError;

use crate::as_usize;
use crate::header::ReservedOwner;
use crate::pagecache::page_payload_len;

/// Owner object stamped in every allocation-map block header.
pub(crate) const ALLOC_MAP_OWNER: u64 = ReservedOwner::AllocMap.sentinel();

/// Header payload offsets, relative to the end of the sealed block header.
const H_COVERED: usize = 0;
const H_REGION_START: usize = 8;
const H_REGION_BLOCKS: usize = 16;
const H_CLEAN_GENERATION: usize = 24;
const H_FLAGS: usize = 32;

/// Meaningful bytes of allocation-map header payload.
pub(crate) const MAP_HEADER_PAYLOAD: usize = 40;

/// Header flag: every in-place update up to [`MapHeader::clean_generation`]
/// reached the device, so a mount at that generation may adopt the map. Its
/// absence means an update was in flight, so the map is stale and the mount
/// rebuilds.
const FLAG_CLEAN: u64 = 1;

/// The allocation map's region header: what the map covers, where it lives,
/// and whether its in-place updates finished.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) struct MapHeader {
    /// Device blocks the bitmap has a bit for.
    pub covered: u64,
    /// First block of the region, repeated here so a region that has moved
    /// under a stale transaction root is rejected even though it authenticates
    /// at its own address.
    pub region_start: u64,
    /// Length of the whole region: header, summary, and bitmap pages.
    pub region_blocks: u64,
    /// Transaction generation whose allocation state the map fully reflects.
    /// Meaningful only when [`Self::clean`] is set.
    pub clean_generation: u64,
    /// Whether the map is safe to adopt at [`Self::clean_generation`].
    pub clean: bool,
}

impl MapHeader {
    /// Write this header into a region block's payload.
    pub(crate) fn encode(&self, payload: &mut [u8]) -> Result<(), DriverError> {
        if payload.len() < MAP_HEADER_PAYLOAD {
            return Err(DriverError::DeviceFault);
        }
        wr(payload, H_COVERED, self.covered);
        wr(payload, H_REGION_START, self.region_start);
        wr(payload, H_REGION_BLOCKS, self.region_blocks);
        wr(payload, H_CLEAN_GENERATION, self.clean_generation);
        wr(payload, H_FLAGS, u64::from(self.clean) * FLAG_CLEAN);
        Ok(())
    }

    /// Read a header back out of a region block's payload.
    pub(crate) fn decode(payload: &[u8]) -> Result<Self, DriverError> {
        if payload.len() < MAP_HEADER_PAYLOAD {
            return Err(DriverError::DeviceFault);
        }
        Ok(Self {
            covered: rd(payload, H_COVERED),
            region_start: rd(payload, H_REGION_START),
            region_blocks: rd(payload, H_REGION_BLOCKS),
            clean_generation: rd(payload, H_CLEAN_GENERATION),
            clean: rd(payload, H_FLAGS) & FLAG_CLEAN != 0,
        })
    }
}

fn rd(payload: &[u8], at: usize) -> u64 {
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&payload[at..at + 8]);
    u64::from_le_bytes(raw)
}

fn wr(payload: &mut [u8], at: usize, value: u64) {
    payload[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

/// Where every part of the map region sits, derived from the device block
/// size and the block count the map must cover.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) struct MapGeometry {
    /// First block of the region on the device.
    start: u64,
    /// Device blocks the bitmap covers.
    covered: u64,
    /// Bits — device blocks — one full bitmap page accounts for.
    bits_per_page: u64,
    /// Bitmap pages in the region.
    pages: u64,
    /// Summary blocks preceding the bitmap pages.
    summary_blocks: u64,
    /// Per-page free counts one summary block holds.
    summary_slots: u64,
}

impl MapGeometry {
    /// Lay out a map covering `covered` device blocks, starting at `start`.
    ///
    /// # Errors
    ///
    /// [`DriverError::Unsupported`] when the block size cannot hold a bitmap
    /// page whose free count fits a summary slot, or when `covered` is zero.
    /// The supported block-size range always satisfies the former.
    pub(crate) fn new(start: u64, block_size: usize, covered: u64) -> Result<Self, DriverError> {
        if covered == 0 {
            return Err(DriverError::Unsupported);
        }
        let payload = page_payload_len(block_size);
        let bits_per_page = (payload as u64) * 8;
        let summary_slots = (payload / 2) as u64;
        if bits_per_page == 0 || bits_per_page > u64::from(u16::MAX) || summary_slots == 0 {
            return Err(DriverError::Unsupported);
        }
        let pages = covered.div_ceil(bits_per_page);
        let summary_blocks = pages.div_ceil(summary_slots);
        Ok(Self {
            start,
            covered,
            bits_per_page,
            pages,
            summary_blocks,
            summary_slots,
        })
    }

    pub(crate) fn start(&self) -> u64 {
        self.start
    }

    pub(crate) fn covered(&self) -> u64 {
        self.covered
    }

    pub(crate) fn pages(&self) -> u64 {
        self.pages
    }

    pub(crate) fn summary_blocks(&self) -> u64 {
        self.summary_blocks
    }

    /// Total blocks the region occupies: header, summary, and bitmap pages.
    pub(crate) fn region_blocks(&self) -> u64 {
        1 + self.summary_blocks + self.pages
    }

    /// Device address of the region's header block.
    pub(crate) fn header_block(&self) -> u64 {
        self.start
    }

    /// Device address of summary block `index`.
    pub(crate) fn summary_block(&self, index: u64) -> u64 {
        self.start + 1 + index
    }

    /// Device address of bitmap page `page`.
    pub(crate) fn page_block(&self, page: u64) -> u64 {
        self.start + 1 + self.summary_blocks + page
    }

    /// The bitmap page holding `block`'s bit.
    pub(crate) fn page_of(&self, block: u64) -> u64 {
        block / self.bits_per_page
    }

    /// `block`'s bit index within its page.
    pub(crate) fn bit_of(&self, block: u64) -> u64 {
        block % self.bits_per_page
    }

    /// First device block accounted for by bitmap page `page`.
    pub(crate) fn page_first_block(&self, page: u64) -> u64 {
        page * self.bits_per_page
    }

    /// Bits of bitmap page `page` that name a real device block. Every page
    /// but the last is full; the last covers only the remainder.
    pub(crate) fn page_capacity(&self, page: u64) -> u64 {
        let first = self.page_first_block(page);
        self.covered.saturating_sub(first).min(self.bits_per_page)
    }

    /// Summary block index and byte offset holding page `page`'s free count.
    pub(crate) fn summary_slot_of(&self, page: u64) -> (u64, usize) {
        (
            page / self.summary_slots,
            as_usize((page % self.summary_slots) * 2),
        )
    }
}

/// Whether bit `bit` of a bitmap page payload is set (the block is used).
pub(crate) fn bit_get(payload: &[u8], bit: u64) -> bool {
    let byte = (bit / 8) as usize;
    match payload.get(byte) {
        Some(value) => value & (1u8 << (bit % 8)) != 0,
        None => true,
    }
}

/// Set or clear bit `bit` of a bitmap page payload, returning whether it
/// changed.
pub(crate) fn bit_set(payload: &mut [u8], bit: u64, used: bool) -> bool {
    let byte = (bit / 8) as usize;
    let Some(slot) = payload.get_mut(byte) else {
        return false;
    };
    let mask = 1u8 << (bit % 8);
    let was = *slot & mask != 0;
    if was == used {
        return false;
    }
    if used {
        *slot |= mask;
    } else {
        *slot &= !mask;
    }
    true
}

/// Set every bit in `lo..hi` of a bitmap page payload, returning how many of
/// them were clear beforehand.
///
/// Whole bytes are filled at once, so reserving a long run — the map region
/// itself is hundreds of thousands of blocks on a 100 TB volume — costs one
/// byte write per eight blocks rather than a read-modify-write per block.
pub(crate) fn set_bit_range(payload: &mut [u8], lo: u64, hi: u64) -> u64 {
    let mut changed = 0;
    let mut bit = lo;
    while bit < hi {
        let byte = (bit / 8) as usize;
        let Some(slot) = payload.get_mut(byte) else {
            break;
        };
        let offset = bit % 8;
        if offset == 0 && hi - bit >= 8 {
            changed += u64::from(slot.count_zeros());
            *slot = u8::MAX;
            bit += 8;
            continue;
        }
        let mask = 1u8 << offset;
        if *slot & mask == 0 {
            *slot |= mask;
            changed += 1;
        }
        bit += 1;
    }
    changed
}

/// Clear every bit in `lo..hi` of a bitmap page payload, returning how many of
/// them were set beforehand.
///
/// The mirror of [`set_bit_range`], and it matters for the same reason: the
/// runs released whole — a relayed map region, a reconcile's scratch array —
/// are hundreds of thousands of blocks long.
pub(crate) fn clear_bit_range(payload: &mut [u8], lo: u64, hi: u64) -> u64 {
    let mut changed = 0;
    let mut bit = lo;
    while bit < hi {
        let byte = (bit / 8) as usize;
        let Some(slot) = payload.get_mut(byte) else {
            break;
        };
        let offset = bit % 8;
        if offset == 0 && hi - bit >= 8 {
            changed += u64::from(slot.count_ones());
            *slot = 0;
            bit += 8;
            continue;
        }
        let mask = 1u8 << offset;
        if *slot & mask != 0 {
            *slot &= !mask;
            changed += 1;
        }
        bit += 1;
    }
    changed
}

/// The lowest clear bit in `from..capacity`, skipping wholly-used bytes.
pub(crate) fn find_free_bit(payload: &[u8], from: u64, capacity: u64) -> Option<u64> {
    let mut bit = from;
    while bit < capacity {
        let byte = (bit / 8) as usize;
        let value = *payload.get(byte)?;
        if value == u8::MAX {
            bit = (bit / 8 + 1) * 8;
            continue;
        }
        if value & (1u8 << (bit % 8)) == 0 {
            return Some(bit);
        }
        bit += 1;
    }
    None
}

/// The lowest set bit in `from..capacity`, skipping wholly-clear bytes.
///
/// The dual of [`find_free_bit`]: together they turn a page into its free
/// spans in time proportional to its bytes rather than its bits, which is what
/// keeps [`crate::ARXFS::map_find_free_run`] off a per-block walk.
pub(crate) fn find_used_bit(payload: &[u8], from: u64, capacity: u64) -> Option<u64> {
    let mut bit = from;
    while bit < capacity {
        let byte = (bit / 8) as usize;
        let value = *payload.get(byte)?;
        if value == 0 {
            bit = (bit / 8 + 1) * 8;
            continue;
        }
        if value & (1u8 << (bit % 8)) != 0 {
            return Some(bit);
        }
        bit += 1;
    }
    None
}

/// The highest clear bit in `floor..=from`, skipping wholly-used bytes.
pub(crate) fn find_free_bit_rev(payload: &[u8], from: u64, floor: u64) -> Option<u64> {
    let mut bit = from;
    loop {
        if bit < floor {
            return None;
        }
        let byte = (bit / 8) as usize;
        let value = *payload.get(byte)?;
        if value == u8::MAX {
            let byte_first = (bit / 8) * 8;
            if byte_first == 0 {
                return None;
            }
            bit = byte_first - 1;
            continue;
        }
        if value & (1u8 << (bit % 8)) == 0 {
            return Some(bit);
        }
        if bit == 0 {
            return None;
        }
        bit -= 1;
    }
}

/// The free-block count a summary block records for one bitmap page.
pub(crate) fn summary_get(payload: &[u8], offset: usize) -> u64 {
    match (payload.get(offset), payload.get(offset + 1)) {
        (Some(lo), Some(hi)) => u64::from(u16::from_le_bytes([*lo, *hi])),
        _ => 0,
    }
}

/// Record a bitmap page's free-block count in its summary block.
pub(crate) fn summary_set(payload: &mut [u8], offset: usize, value: u64) {
    let clamped = u16::try_from(value).unwrap_or(u16::MAX);
    if let Some(slot) = payload.get_mut(offset..offset + 2) {
        slot.copy_from_slice(&clamped.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BS: usize = 4096;
    const PAYLOAD: usize = page_payload_len(BS);

    #[test]
    fn geometry_lays_header_summary_then_pages() {
        let geom = MapGeometry::new(8, BS, 32_768).expect("geometry");
        assert_eq!(geom.header_block(), 8);
        assert_eq!(geom.summary_blocks(), 1);
        assert_eq!(geom.pages(), 32_768u64.div_ceil((PAYLOAD * 8) as u64));
        assert_eq!(geom.summary_block(0), 9);
        assert_eq!(geom.page_block(0), 10);
        assert_eq!(geom.region_blocks(), 1 + 1 + geom.pages());
    }

    #[test]
    fn geometry_maps_blocks_to_pages_bits_and_summary_slots() {
        let bits = (PAYLOAD * 8) as u64;
        let geom = MapGeometry::new(8, BS, 4 * bits).expect("geometry");
        assert_eq!(geom.page_of(0), 0);
        assert_eq!(geom.bit_of(0), 0);
        assert_eq!(geom.page_of(bits), 1);
        assert_eq!(geom.bit_of(bits), 0);
        assert_eq!(geom.page_of(bits + 5), 1);
        assert_eq!(geom.bit_of(bits + 5), 5);
        assert_eq!(geom.page_first_block(2), 2 * bits);
        assert_eq!(geom.summary_slot_of(0), (0, 0));
        assert_eq!(geom.summary_slot_of(3), (0, 6));
    }

    #[test]
    fn last_page_capacity_is_the_remainder() {
        let bits = (PAYLOAD * 8) as u64;
        let geom = MapGeometry::new(8, BS, bits + 17).expect("geometry");
        assert_eq!(geom.pages(), 2);
        assert_eq!(geom.page_capacity(0), bits);
        assert_eq!(geom.page_capacity(1), 17);
    }

    #[test]
    fn geometry_scales_to_a_hundred_terabyte_volume() {
        // 100 TiB of 4 KiB blocks: the map must stay a rounding error of the
        // device and its summary must stay small enough to page through.
        let covered = 100u64 * 1024 * 1024 * 1024 * 1024 / 4096;
        let geom = MapGeometry::new(8, BS, covered).expect("geometry");
        let overhead = geom.region_blocks() * 10_000 / covered;
        assert!(overhead < 5, "map overhead {overhead} per 10000 blocks");
        assert!(geom.summary_blocks() < 1024);
        assert!(geom.page_capacity(geom.pages() - 1) > 0);
    }

    #[test]
    fn geometry_refuses_a_zero_coverage_map() {
        assert!(MapGeometry::new(8, BS, 0).is_err());
    }

    #[test]
    fn header_round_trips_through_its_payload() {
        let header = MapHeader {
            covered: 32_768,
            region_start: 8,
            region_blocks: 4,
            clean_generation: 7,
            clean: true,
        };
        let mut payload = [0u8; PAYLOAD];
        header.encode(&mut payload).expect("encode");
        assert_eq!(MapHeader::decode(&payload).expect("decode"), header);

        let dirty = MapHeader {
            clean: false,
            ..header
        };
        dirty.encode(&mut payload).expect("encode");
        assert_eq!(MapHeader::decode(&payload).expect("decode"), dirty);
    }

    #[test]
    fn header_refuses_a_payload_too_short_to_hold_it() {
        let mut tiny = [0u8; MAP_HEADER_PAYLOAD - 1];
        let header = MapHeader {
            covered: 1,
            region_start: 0,
            region_blocks: 1,
            clean_generation: 0,
            clean: false,
        };
        assert!(header.encode(&mut tiny).is_err());
        assert!(MapHeader::decode(&tiny).is_err());
    }

    #[test]
    fn bits_set_clear_and_report_change() {
        let mut payload = [0u8; 8];
        assert!(!bit_get(&payload, 3));
        assert!(bit_set(&mut payload, 3, true));
        assert!(bit_get(&payload, 3));
        assert!(!bit_set(&mut payload, 3, true));
        assert!(bit_set(&mut payload, 3, false));
        assert!(!bit_get(&payload, 3));
    }

    #[test]
    fn a_bit_beyond_the_payload_reads_used_and_refuses_to_change() {
        let mut payload = [0u8; 2];
        assert!(bit_get(&payload, 64));
        assert!(!bit_set(&mut payload, 64, false));
    }

    #[test]
    fn forward_scan_finds_the_lowest_free_bit_and_skips_full_bytes() {
        let mut payload = [0u8; 4];
        payload[0] = 0xFF;
        payload[1] = 0xFF;
        payload[2] = 0b0000_0111;
        assert_eq!(find_free_bit(&payload, 0, 32), Some(19));
        assert_eq!(find_free_bit(&payload, 20, 32), Some(20));
        assert_eq!(find_free_bit(&payload, 0, 19), None);
    }

    #[test]
    fn a_range_is_set_whole_bytes_at_a_time_and_counts_only_the_changes() {
        let mut payload = [0u8; 4];
        payload[1] = 0b0000_0011;
        assert_eq!(set_bit_range(&mut payload, 3, 21), 16);
        assert_eq!(payload[0], 0b1111_1000);
        assert_eq!(payload[1], 0xFF);
        assert_eq!(payload[2], 0b0001_1111);
        // A second pass changes nothing.
        assert_eq!(set_bit_range(&mut payload, 3, 21), 0);
    }

    #[test]
    fn a_range_beyond_the_payload_stops_rather_than_running_off_the_end() {
        let mut payload = [0u8; 2];
        assert_eq!(set_bit_range(&mut payload, 0, 64), 16);
        assert_eq!(payload, [0xFF, 0xFF]);
    }

    #[test]
    fn forward_scan_reports_a_wholly_used_span_as_empty() {
        let payload = [0xFFu8; 4];
        assert_eq!(find_free_bit(&payload, 0, 32), None);
    }

    #[test]
    fn reverse_scan_finds_the_highest_free_bit_and_skips_full_bytes() {
        let mut payload = [0u8; 4];
        payload[3] = 0xFF;
        payload[2] = 0xFF;
        payload[1] = 0b1111_1110;
        assert_eq!(find_free_bit_rev(&payload, 31, 0), Some(8));
        assert_eq!(find_free_bit_rev(&payload, 7, 0), Some(7));
        assert_eq!(find_free_bit_rev(&payload, 31, 9), None);
    }

    #[test]
    fn reverse_scan_of_a_wholly_used_low_span_terminates() {
        let payload = [0xFFu8; 2];
        assert_eq!(find_free_bit_rev(&payload, 15, 0), None);
    }

    #[test]
    fn summary_slots_round_trip_and_clamp() {
        let mut payload = [0u8; 8];
        summary_set(&mut payload, 2, 1234);
        assert_eq!(summary_get(&payload, 2), 1234);
        summary_set(&mut payload, 4, u64::MAX);
        assert_eq!(summary_get(&payload, 4), u64::from(u16::MAX));
        assert_eq!(summary_get(&payload, 64), 0);
    }
}
