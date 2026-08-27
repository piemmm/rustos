//! The bounded page cache every in-place paged region reads through.
//!
//! Both paged regions ARXFS keeps outside the copy-on-write trees — the
//! allocation map ([`crate::allocmap`]) and the transient scratch arrays a
//! whole-volume pass streams its derived truth through ([`crate::scratch`]) —
//! are far larger than the RAM a small machine can give them, so neither is
//! ever resident in full. They page through this one cache instead, so a
//! volume's resident cost is a fixed number of blocks rather than a share of
//! its size, and there is one eviction and write-back discipline rather than
//! one per region.

use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

use crate::header::HEADER_LEN;

/// Pages one cache holds at once.
///
/// This is a cache bound, not a capacity: a miss costs one block read, never a
/// failure, so the ceiling is deliberately fixed and **volume-independent**.
/// Sizing it from the device would make resident memory scale with volume
/// size, which is the very thing paging exists to avoid — several 100 TB+
/// volumes must mount together on a 1 GiB machine, so each region's footprint
/// stays a quarter of a megabyte however large the volume is.
pub(crate) const MAX_CACHED_PAGES: usize = 64;

/// One page held in the bounded cache.
struct CachedBlock {
    payload: Vec<u8>,
    dirty: bool,
    stamp: u64,
}

/// A bounded, least-recently-used cache of sealed device blocks, keyed by
/// device address.
///
/// Evicting a dirty page writes it back first, which the owning filesystem
/// does — the cache itself performs no I/O, so it stays free of the block
/// device and is unit-testable on the host.
pub(crate) struct BlockCache {
    entries: BTreeMap<u64, CachedBlock>,
    payload_len: usize,
    clock: u64,
}

/// Bytes a block of `block_size` leaves for a paged region after its sealed
/// identity header.
pub(crate) const fn page_payload_len(block_size: usize) -> usize {
    block_size - HEADER_LEN
}

impl BlockCache {
    /// An empty cache over pages of `block_size`.
    pub(crate) fn new(block_size: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            payload_len: page_payload_len(block_size),
            clock: 0,
        }
    }

    fn tick(&mut self) -> u64 {
        self.clock = self.clock.wrapping_add(1);
        self.clock
    }

    pub(crate) fn contains(&self, page: u64) -> bool {
        self.entries.contains_key(&page)
    }

    /// The least-recently-used page, with whether it holds unwritten
    /// changes, or `None` while the cache still has room.
    pub(crate) fn eviction_candidate(&self) -> Option<(u64, bool)> {
        if self.entries.len() < MAX_CACHED_PAGES {
            return None;
        }
        self.entries
            .iter()
            .min_by_key(|(_, entry)| entry.stamp)
            .map(|(block, entry)| (*block, entry.dirty))
    }

    /// Forget a cached page; the owner has written it back already if
    /// it held changes.
    pub(crate) fn remove(&mut self, page: u64) {
        self.entries.remove(&page);
    }

    /// Install a page whose bytes match the device.
    pub(crate) fn insert_clean(&mut self, page: u64, payload: &[u8]) {
        let stamp = self.tick();
        let mut bytes = vec![0u8; self.payload_len];
        let len = payload.len().min(self.payload_len);
        bytes[..len].copy_from_slice(&payload[..len]);
        self.entries.insert(
            page,
            CachedBlock {
                payload: bytes,
                dirty: false,
                stamp,
            },
        );
    }

    /// Install a page that exists only in RAM so far — a synthesised
    /// all-free page, or a page of a map being built — so a flush writes it.
    pub(crate) fn insert_dirty(&mut self, page: u64, payload: Vec<u8>) {
        let stamp = self.tick();
        self.entries.insert(
            page,
            CachedBlock {
                payload,
                dirty: true,
                stamp,
            },
        );
    }

    /// A cached page's bytes for reading, refreshing its recency.
    pub(crate) fn read(&mut self, page: u64) -> Option<&[u8]> {
        let stamp = self.tick();
        let entry = self.entries.get_mut(&page)?;
        entry.stamp = stamp;
        Some(&entry.payload)
    }

    /// A cached page's bytes for writing, marking it dirty.
    pub(crate) fn write(&mut self, page: u64) -> Option<&mut [u8]> {
        let stamp = self.tick();
        let entry = self.entries.get_mut(&page)?;
        entry.stamp = stamp;
        entry.dirty = true;
        Some(&mut entry.payload)
    }

    /// A cached page's bytes without touching its recency, for the
    /// flush path that is about to write them out.
    pub(crate) fn peek(&self, page: u64) -> Option<&[u8]> {
        self.entries.get(&page).map(|e| e.payload.as_slice())
    }

    /// Region blocks holding unwritten changes, in device-address order.
    pub(crate) fn dirty_pages(&self) -> Vec<u64> {
        self.entries
            .iter()
            .filter(|(_, entry)| entry.dirty)
            .map(|(block, _)| *block)
            .collect()
    }

    /// Note that a page's bytes are now on the device.
    pub(crate) fn mark_written(&mut self, page: u64) {
        if let Some(entry) = self.entries.get_mut(&page) {
            entry.dirty = false;
        }
    }

    /// Drop every cached block, discarding unwritten changes. Used when the
    /// map is replaced wholesale by a rebuild.
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BS: usize = 4096;

    #[test]
    fn cache_evicts_the_least_recently_used_page_once_full() {
        let mut cache = BlockCache::new(BS);
        for page in 0..MAX_CACHED_PAGES as u64 {
            cache.insert_clean(page, &[0u8; 4]);
        }
        assert_eq!(cache.len(), MAX_CACHED_PAGES);
        // Touch every page but page 0, so it is the coldest.
        for page in 1..MAX_CACHED_PAGES as u64 {
            assert!(cache.read(page).is_some());
        }
        assert_eq!(cache.eviction_candidate(), Some((0, false)));
        cache.remove(0);
        assert!(cache.eviction_candidate().is_none());
    }

    #[test]
    fn cache_tracks_dirty_pages_until_they_are_written() {
        let mut cache = BlockCache::new(BS);
        cache.insert_clean(5, &[0u8; 4]);
        assert!(cache.dirty_pages().is_empty());
        cache.write(5).expect("cached")[0] = 0xAA;
        assert_eq!(cache.dirty_pages(), alloc::vec![5]);
        assert_eq!(cache.peek(5).expect("cached")[0], 0xAA);
        cache.mark_written(5);
        assert!(cache.dirty_pages().is_empty());
    }

    #[test]
    fn cache_reports_a_dirty_eviction_candidate_so_it_is_written_back() {
        let mut cache = BlockCache::new(BS);
        for page in 0..MAX_CACHED_PAGES as u64 {
            cache.insert_dirty(page, alloc::vec![0u8; 4]);
        }
        let (page, dirty) = cache.eviction_candidate().expect("full");
        assert_eq!(page, 0);
        assert!(dirty);
    }

    #[test]
    fn clearing_the_cache_drops_every_page() {
        let mut cache = BlockCache::new(BS);
        cache.insert_dirty(1, alloc::vec![0u8; 4]);
        cache.clear();
        assert_eq!(cache.len(), 0);
        assert!(cache.peek(1).is_none());
    }
}
