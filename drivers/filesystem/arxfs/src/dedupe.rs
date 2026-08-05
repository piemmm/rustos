//! Deduplication: the chunk/refcount table, the reverse-reference table, the
//! in-memory dedupe index, and reflinks
//! (`docs/src/filesystem/arxfs-spec.md` §4, §6, §8, §9).
//!
//! `ARXFS` stores immutable physical data records ("chunks") that more than one
//! `(file, logical block)` may share. Sharing is **exact and verified**: a
//! candidate is shared only after its bytes are confirmed equal to the new
//! record (*missing a duplicate is acceptable; merging unequal data is
//! corruption*). Three structures back this, reusing the one generic
//! copy-on-write [`crate::btree`] (no second B-tree):
//!
//! * the **chunk/refcount tree** (authoritative): keyed by a chunk's physical
//!   block, its value is a [`ChunkRecord`] — the referrer count, the
//!   encryption domain the chunk belongs to, the plaintext logical hash, and
//!   the logical length. Every file-data block has a record, so the refcount
//!   is authoritative for safe discard;
//! * the **reverse-reference tree** (authoritative): keyed by the same chunk
//!   physical block, its value is a capped list of the `(inode, logical block)`
//!   referrers, used by scrub/check/health and safe discard;
//! * the **dedupe index** ([`DedupeIndex`], rebuildable, never authoritative):
//!   an in-memory `(domain, length, logical hash) -> chunk` map consulted for
//!   bounded foreground discovery on write and warmed by the writes
//!   themselves. Because *missing a duplicate is acceptable*, the index is a
//!   **bounded cache**, not an unbounded map: its resident RAM is capped at
//!   [`DEDUPE_INDEX_BUDGET_BYTES`], split into a [`DEDUPE_HOT_BUDGET_BYTES`]
//!   "frequently used" tier (candidates promoted on a dedupe hit) and a
//!   [`DEDUPE_GENERAL_BUDGET_BYTES`] tier of freshly written candidates. Once a
//!   tier is full it evicts its least-recently-used candidate rather than
//!   growing, so the index never exceeds its budget regardless of volume size.
//!   It is deliberately **not** pre-seeded at mount: walking the chunk tree
//!   would cost a read per chunk on a volume of any size — unbounded on a
//!   100 TB one — to fill a cache that evicts all but its last few thousand
//!   entries anyway. The index warms from the writes that can use it.
//!
//! Dedupe is allowed **only within the same encryption domain**; the
//! domain is carried in every chunk record and index key so the rule holds
//! once multiple domains exist.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::btree::TreeSpec;
use crate::integrity::LOGICAL_HASH_LEN;

/// Owner object stamped in every chunk-tree node header; a
/// reserved sentinel distinct from any inode number and from the inode tree's
/// own [`u64::MAX`] owner.
pub(crate) const CHUNK_TREE_OWNER: u64 = u64::MAX - 1;

/// Owner object stamped in every reverse-reference-tree node header.
pub(crate) const REVERSE_REF_TREE_OWNER: u64 = u64::MAX - 2;

/// Fixed on-disk width of one [`ChunkRecord`]: refcount (8) + domain (8) +
/// logical length (4) + logical hash (32).
pub(crate) const CHUNK_VALUE_LEN: usize = 8 + 8 + 4 + LOGICAL_HASH_LEN;

/// Maximum referrers recorded inline in one reverse-reference record. Sharing
/// that would exceed this declines to dedupe and writes a fresh chunk instead
/// (missing a duplicate is acceptable), so the record never overflows and
/// the referrer set stays exact and bounded.
pub(crate) const REVERSE_REF_CAP: usize = 8;

/// Fixed on-disk width of one reverse-reference record: a referrer count (4)
/// followed by [`REVERSE_REF_CAP`] `(inode: u32, logical block: u64)` pairs.
pub(crate) const REVERSE_REF_VALUE_LEN: usize = 4 + REVERSE_REF_CAP * (4 + 8);

/// The chunk/refcount tree's record shape (keyed by a chunk's physical block).
pub(crate) fn chunk_spec() -> TreeSpec {
    TreeSpec {
        value_len: CHUNK_VALUE_LEN,
        owner: CHUNK_TREE_OWNER,
    }
}

/// The reverse-reference tree's record shape (keyed by a chunk's physical
/// block).
pub(crate) fn reverse_ref_spec() -> TreeSpec {
    TreeSpec {
        value_len: REVERSE_REF_VALUE_LEN,
        owner: REVERSE_REF_TREE_OWNER,
    }
}

/// One chunk/refcount-table record: an immutable physical data record that one
/// or more `(file, logical block)` referrers share.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) struct ChunkRecord {
    /// Number of `(file, logical block)` referrers pointing at the chunk.
    /// Decrementing to zero frees the chunk.
    pub refcount: u64,
    /// The encryption domain the chunk belongs to; dedupe never crosses it.
    pub domain: u64,
    /// Logical (plaintext) length the chunk maps, in bytes.
    pub length: u32,
    /// SHA-256 of the chunk's plaintext — the dedupe key.
    pub logical_hash: [u8; LOGICAL_HASH_LEN],
}

impl ChunkRecord {
    /// Encode the record into a [`CHUNK_VALUE_LEN`]-byte value.
    pub(crate) fn encode(&self) -> [u8; CHUNK_VALUE_LEN] {
        let mut out = [0u8; CHUNK_VALUE_LEN];
        out[0..8].copy_from_slice(&self.refcount.to_le_bytes());
        out[8..16].copy_from_slice(&self.domain.to_le_bytes());
        out[16..20].copy_from_slice(&self.length.to_le_bytes());
        out[20..20 + LOGICAL_HASH_LEN].copy_from_slice(&self.logical_hash);
        out
    }

    /// Decode a record from a chunk-tree value. Returns `None` if `value` is
    /// too short (corruption is surfaced rather than panicked).
    pub(crate) fn decode(value: &[u8]) -> Option<Self> {
        if value.len() < CHUNK_VALUE_LEN {
            return None;
        }
        let mut refcount = [0u8; 8];
        refcount.copy_from_slice(&value[0..8]);
        let mut domain = [0u8; 8];
        domain.copy_from_slice(&value[8..16]);
        let mut length = [0u8; 4];
        length.copy_from_slice(&value[16..20]);
        let mut logical_hash = [0u8; LOGICAL_HASH_LEN];
        logical_hash.copy_from_slice(&value[20..20 + LOGICAL_HASH_LEN]);
        Some(Self {
            refcount: u64::from_le_bytes(refcount),
            domain: u64::from_le_bytes(domain),
            length: u32::from_le_bytes(length),
            logical_hash,
        })
    }
}

/// One reverse-reference: the `(inode, logical block)` of a file position that
/// points at a shared chunk.
pub(crate) type Referrer = (u32, u64);

/// Encode a referrer list (at most [`REVERSE_REF_CAP`] entries) into a fixed
/// [`REVERSE_REF_VALUE_LEN`]-byte reverse-reference value.
pub(crate) fn encode_reverse_ref(referrers: &[Referrer]) -> [u8; REVERSE_REF_VALUE_LEN] {
    let mut out = [0u8; REVERSE_REF_VALUE_LEN];
    let count = referrers.len().min(REVERSE_REF_CAP);
    let count_field = u32::try_from(count).unwrap_or_default();
    out[0..4].copy_from_slice(&count_field.to_le_bytes());
    for (i, (inode, logical)) in referrers.iter().take(count).enumerate() {
        let base = 4 + i * (4 + 8);
        out[base..base + 4].copy_from_slice(&inode.to_le_bytes());
        out[base + 4..base + 12].copy_from_slice(&logical.to_le_bytes());
    }
    out
}

/// Decode a reverse-reference value into its referrer list. Returns `None` if
/// `value` is too short or its count exceeds [`REVERSE_REF_CAP`] (corruption is
/// surfaced, never panicked).
pub(crate) fn decode_reverse_ref(value: &[u8]) -> Option<Vec<Referrer>> {
    if value.len() < REVERSE_REF_VALUE_LEN {
        return None;
    }
    let mut count_bytes = [0u8; 4];
    count_bytes.copy_from_slice(&value[0..4]);
    let count = u32::from_le_bytes(count_bytes) as usize;
    if count > REVERSE_REF_CAP {
        return None;
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let base = 4 + i * (4 + 8);
        let mut inode = [0u8; 4];
        inode.copy_from_slice(&value[base..base + 4]);
        let mut logical = [0u8; 8];
        logical.copy_from_slice(&value[base + 4..base + 12]);
        out.push((u32::from_le_bytes(inode), u64::from_le_bytes(logical)));
    }
    Some(out)
}

/// The in-memory dedupe-index key: `(domain, length, logical hash)`. It is the
/// rebuildable map's key (the index is never authoritative), packed into
/// a fixed array so it orders deterministically in a `BTreeMap`.
pub(crate) type DedupeKey = [u8; 8 + 4 + LOGICAL_HASH_LEN];

/// Build the dedupe-index key for a chunk in `domain` of plaintext `length`
/// whose plaintext hashes to `logical_hash`.
pub(crate) fn dedupe_key(
    domain: u64,
    length: u32,
    logical_hash: &[u8; LOGICAL_HASH_LEN],
) -> DedupeKey {
    let mut key = [0u8; 8 + 4 + LOGICAL_HASH_LEN];
    key[0..8].copy_from_slice(&domain.to_le_bytes());
    key[8..12].copy_from_slice(&length.to_le_bytes());
    key[12..12 + LOGICAL_HASH_LEN].copy_from_slice(logical_hash);
    key
}

/// A dedupe-index candidate: the physical block of a chunk plus the
/// `(inode, logical block)` referrer that introduced it, so a foreground
/// lookup can confirm the candidate is still live (its referrer's extent map
/// still points at it) before byte-verifying and sharing.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) struct DedupeCandidate {
    pub phys: u64,
    pub inode: u32,
    pub logical: u64,
}

/// Hard upper bound on resident RAM for the whole dedupe index, in bytes.
/// *Missing a duplicate is acceptable*, so the index is a bounded cache:
/// once full it evicts rather than growing, and never exceeds this budget
/// regardless of how much unique data the volume holds.
pub(crate) const DEDUPE_INDEX_BUDGET_BYTES: usize = 100 * 1024 * 1024;

/// The slice of [`DEDUPE_INDEX_BUDGET_BYTES`] reserved for the "frequently
/// used" hot tier: candidates promoted on a dedupe hit.
pub(crate) const DEDUPE_HOT_BUDGET_BYTES: usize = 20 * 1024 * 1024;

/// The remaining slice for the general tier of freshly written candidates.
pub(crate) const DEDUPE_GENERAL_BUDGET_BYTES: usize =
    DEDUPE_INDEX_BUDGET_BYTES - DEDUPE_HOT_BUDGET_BYTES;

/// Conservative resident-RAM estimate for one cached candidate, in bytes,
/// across both backing maps of an [`LruTier`]. The `by_key` map stores a
/// [`DedupeKey`] (44) + [`DedupeCandidate`] (24 with alignment padding) + a
/// `u64` recency stamp (8) = 76 bytes; the `by_recency` map stores the stamp
/// (8) + the key (44) = 52 bytes; 128 bytes of payload in total. The figure is
/// doubled to bound `BTreeMap` node bookkeeping and partial node fill, a
/// deliberate over-estimate so the entry caps below keep the index strictly
/// within its byte budget rather than merely near it.
const ENTRY_FOOTPRINT_BYTES: usize = 256;

/// Maximum candidates held in the hot tier (derived from its byte budget).
const HOT_CAP: usize = DEDUPE_HOT_BUDGET_BYTES / ENTRY_FOOTPRINT_BYTES;

/// Maximum candidates held in the general tier (derived from its byte budget).
const GENERAL_CAP: usize = DEDUPE_GENERAL_BUDGET_BYTES / ENTRY_FOOTPRINT_BYTES;

/// Compile-time guarantee that the derived per-tier entry caps keep the index
/// strictly within its byte budgets — the byte budget is a hard ceiling, and a
/// future change to a budget or the footprint that would break it fails to
/// build rather than silently overshooting the RAM cap.
const _: () = {
    assert!(HOT_CAP > 0 && GENERAL_CAP > 0);
    assert!(HOT_CAP * ENTRY_FOOTPRINT_BYTES <= DEDUPE_HOT_BUDGET_BYTES);
    assert!(GENERAL_CAP * ENTRY_FOOTPRINT_BYTES <= DEDUPE_GENERAL_BUDGET_BYTES);
    assert!((HOT_CAP + GENERAL_CAP) * ENTRY_FOOTPRINT_BYTES <= DEDUPE_INDEX_BUDGET_BYTES);
};

/// A fixed-capacity least-recently-used map from [`DedupeKey`] to
/// [`DedupeCandidate`]. Recency is a monotonic stamp; `by_recency` mirrors
/// `by_key` ordered by that stamp so the least-recently-used entry is
/// `by_recency`'s first key — eviction and refresh are both `O(log n)` with no
/// linear scan.
struct LruTier {
    cap: usize,
    next_stamp: u64,
    by_key: BTreeMap<DedupeKey, (DedupeCandidate, u64)>,
    by_recency: BTreeMap<u64, DedupeKey>,
}

impl LruTier {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            next_stamp: 0,
            by_key: BTreeMap::new(),
            by_recency: BTreeMap::new(),
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.by_key.len()
    }

    fn contains(&self, key: &DedupeKey) -> bool {
        self.by_key.contains_key(key)
    }

    /// Stamp the entry under `key` (if any) as most-recently-used and return
    /// its candidate.
    fn touch(&mut self, key: &DedupeKey) -> Option<DedupeCandidate> {
        let (cand, old) = *self.by_key.get(key)?;
        self.by_recency.remove(&old);
        let stamp = self.next_stamp;
        self.next_stamp += 1;
        self.by_recency.insert(stamp, *key);
        self.by_key.insert(*key, (cand, stamp));
        Some(cand)
    }

    fn remove(&mut self, key: &DedupeKey) -> Option<DedupeCandidate> {
        let (cand, stamp) = self.by_key.remove(key)?;
        self.by_recency.remove(&stamp);
        Some(cand)
    }

    /// Insert or update `key` as most-recently-used, evicting and returning the
    /// least-recently-used `(key, candidate)` when that pushes the tier over
    /// its capacity. A zero capacity holds nothing and evicts immediately.
    fn insert(
        &mut self,
        key: DedupeKey,
        cand: DedupeCandidate,
    ) -> Option<(DedupeKey, DedupeCandidate)> {
        if let Some((_, old)) = self.by_key.get(&key).copied() {
            self.by_recency.remove(&old);
        }
        let stamp = self.next_stamp;
        self.next_stamp += 1;
        self.by_key.insert(key, (cand, stamp));
        self.by_recency.insert(stamp, key);
        if self.by_key.len() <= self.cap {
            return None;
        }
        let (&victim_stamp, &victim_key) = self.by_recency.iter().next()?;
        self.by_recency.remove(&victim_stamp);
        let (victim_cand, _) = self.by_key.remove(&victim_key)?;
        Some((victim_key, victim_cand))
    }
}

/// The bounded, two-tier in-memory dedupe index. A **general** tier holds
/// freshly written candidates; a smaller **hot** ("frequently used") tier holds
/// candidates promoted on a dedupe hit. Each tier evicts its least-recently-used
/// entry when full, so total resident RAM never exceeds
/// [`DEDUPE_INDEX_BUDGET_BYTES`]. Dropping a candidate only forgoes a future
/// dedupe opportunity, which is explicitly acceptable.
pub(crate) struct DedupeIndex {
    hot: LruTier,
    general: LruTier,
}

impl DedupeIndex {
    /// A new, empty index sized to the configured hot and general budgets.
    pub(crate) fn new() -> Self {
        Self {
            hot: LruTier::new(HOT_CAP),
            general: LruTier::new(GENERAL_CAP),
        }
    }

    /// A new, empty index with explicit tier capacities, for tests that want to
    /// exercise eviction without filling the production-sized budgets.
    #[cfg(test)]
    fn with_caps(hot_cap: usize, general_cap: usize) -> Self {
        Self {
            hot: LruTier::new(hot_cap),
            general: LruTier::new(general_cap),
        }
    }

    /// Total candidates currently cached across both tiers.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.hot.len() + self.general.len()
    }

    /// Whether the index currently caches no candidate.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether either tier currently caches a candidate for `key`, without the
    /// promotion/refresh side effect of [`Self::get`].
    #[cfg(test)]
    pub(crate) fn contains_key(&self, key: &DedupeKey) -> bool {
        self.hot.contains(key) || self.general.contains(key)
    }

    /// Look up a candidate for `key`. A hit refreshes the hot tier or, when the
    /// candidate lives in the general tier, **promotes** it into the hot tier —
    /// repeated reuse keeps a candidate "frequently used". A hot-tier eviction
    /// caused by the promotion is demoted back into the general tier rather than
    /// dropped outright.
    pub(crate) fn get(&mut self, key: &DedupeKey) -> Option<DedupeCandidate> {
        if let Some(cand) = self.hot.touch(key) {
            return Some(cand);
        }
        let cand = self.general.remove(key)?;
        if let Some((evk, evc)) = self.hot.insert(*key, cand) {
            self.general.insert(evk, evc);
        }
        Some(cand)
    }

    /// Record a freshly written candidate. A key already promoted to the hot
    /// tier is updated in place; otherwise it enters the general tier.
    pub(crate) fn insert(&mut self, key: DedupeKey, cand: DedupeCandidate) {
        if self.hot.contains(&key) {
            self.hot.insert(key, cand);
            return;
        }
        self.general.insert(key, cand);
    }

    /// Forget any candidate for `key` (used when a lookup finds it stale).
    pub(crate) fn remove(&mut self, key: &DedupeKey) {
        self.hot.remove(key);
        self.general.remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        dedupe_key, DedupeCandidate, DedupeIndex, DEDUPE_GENERAL_BUDGET_BYTES,
        DEDUPE_HOT_BUDGET_BYTES, DEDUPE_INDEX_BUDGET_BYTES, LOGICAL_HASH_LEN,
    };

    /// A distinct dedupe key for test candidate `n` in domain 0, length 4096.
    fn key(n: u64) -> [u8; 8 + 4 + LOGICAL_HASH_LEN] {
        let mut hash = [0u8; LOGICAL_HASH_LEN];
        hash[..8].copy_from_slice(&n.to_le_bytes());
        dedupe_key(0, 4096, &hash)
    }

    fn cand(n: u64) -> DedupeCandidate {
        DedupeCandidate {
            phys: n,
            inode: 1,
            logical: n,
        }
    }

    #[test]
    fn budgets_split_into_hot_and_general() {
        assert_eq!(DEDUPE_HOT_BUDGET_BYTES, 20 * 1024 * 1024);
        assert_eq!(
            DEDUPE_GENERAL_BUDGET_BYTES,
            DEDUPE_INDEX_BUDGET_BYTES - DEDUPE_HOT_BUDGET_BYTES
        );
        assert_eq!(DEDUPE_GENERAL_BUDGET_BYTES, 80 * 1024 * 1024);
        assert_eq!(DEDUPE_INDEX_BUDGET_BYTES, 100 * 1024 * 1024);
    }

    #[test]
    fn general_tier_evicts_lru_when_full_and_stays_bounded() {
        let mut index = DedupeIndex::with_caps(2, 3);
        for n in 0..100 {
            index.insert(key(n), cand(n));
            assert!(index.general.len() <= 3);
            assert!(index.len() <= 5);
        }
        // The three most recently inserted survive; older ones were evicted.
        assert!(index.get(&key(99)).is_some());
        assert!(index.get(&key(0)).is_none());
    }

    #[test]
    fn lookup_promotes_general_candidate_into_hot_tier() {
        let mut index = DedupeIndex::with_caps(2, 3);
        index.insert(key(1), cand(1));
        assert_eq!(index.general.len(), 1);
        assert_eq!(index.hot.len(), 0);

        assert_eq!(index.get(&key(1)), Some(cand(1)));
        assert_eq!(index.hot.len(), 1);
        assert_eq!(index.general.len(), 0);
    }

    #[test]
    fn hot_eviction_from_promotion_demotes_into_general() {
        let mut index = DedupeIndex::with_caps(1, 3);
        index.insert(key(1), cand(1));
        index.insert(key(2), cand(2));
        // Promote key(1): hot now holds it.
        assert!(index.get(&key(1)).is_some());
        assert_eq!(index.hot.len(), 1);
        // Promote key(2): hot is full, so key(1) is demoted back to general.
        assert!(index.get(&key(2)).is_some());
        assert_eq!(index.hot.len(), 1);
        // key(1) survived the eviction by demotion, not dropped.
        assert!(index.get(&key(1)).is_some());
    }

    #[test]
    fn remove_clears_from_either_tier() {
        let mut index = DedupeIndex::with_caps(2, 3);
        index.insert(key(1), cand(1));
        index.insert(key(2), cand(2));
        // Promote key(1) into hot, leave key(2) in general.
        assert!(index.get(&key(1)).is_some());

        index.remove(&key(1));
        index.remove(&key(2));
        assert!(index.get(&key(1)).is_none());
        assert!(index.get(&key(2)).is_none());
        assert_eq!(index.len(), 0);
    }

    #[test]
    fn total_entries_never_exceed_caps_under_mixed_load() {
        let mut index = DedupeIndex::with_caps(2, 3);
        for n in 0..1000 {
            index.insert(key(n), cand(n));
            if n % 3 == 0 {
                let _ = index.get(&key(n / 2));
            }
            assert!(index.hot.len() <= 2);
            assert!(index.general.len() <= 3);
            assert!(index.len() <= 5);
        }
    }
}
