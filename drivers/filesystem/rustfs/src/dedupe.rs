//! Deduplication: the chunk/refcount table, the reverse-reference table, the
//! in-memory dedupe index, and reflinks
//! (`docs/src/filesystem/rustfs-spec.md` §4, §6, §8, §9).
//!
//! `RustFS` stores immutable physical data records ("chunks") that more than one
//! `(file, logical block)` may share. Sharing is **exact and verified**: a
//! candidate is shared only after its bytes are confirmed equal to the new
//! record (§9 — *missing a duplicate is acceptable; merging unequal data is
//! corruption*). Three structures back this, reusing the one generic
//! copy-on-write [`crate::btree`] (`AGENTS.md` §2.2 — no second B-tree):
//!
//! * the **chunk/refcount tree** (authoritative): keyed by a chunk's physical
//!   block, its value is a [`ChunkRecord`] — the referrer count, the
//!   encryption domain the chunk belongs to, the plaintext logical hash, and
//!   the logical length. Every file-data block has a record, so the refcount
//!   is authoritative for safe discard;
//! * the **reverse-reference tree** (authoritative): keyed by the same chunk
//!   physical block, its value is a capped list of the `(inode, logical block)`
//!   referrers, used by scrub/check/health and safe discard;
//! * the **dedupe index** (rebuildable, never authoritative): an in-memory
//!   `(domain, length, logical hash) -> chunk` map rebuilt from the chunk tree
//!   at mount and consulted for bounded foreground discovery on write.
//!
//! Dedupe is allowed **only within the same encryption domain** (§7); the
//! domain is carried in every chunk record and index key so the rule holds
//! once multiple domains exist.

use alloc::vec::Vec;

use crate::btree::TreeSpec;
use crate::integrity::LOGICAL_HASH_LEN;

/// Owner object stamped in every chunk-tree node header (`AGENTS.md` §8); a
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
/// (§9 — missing a duplicate is acceptable), so the record never overflows and
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
    /// Decrementing to zero frees the chunk (§9).
    pub refcount: u64,
    /// The encryption domain the chunk belongs to; dedupe never crosses it
    /// (§7).
    pub domain: u64,
    /// Logical (plaintext) length the chunk maps, in bytes.
    pub length: u32,
    /// SHA-256 of the chunk's plaintext — the dedupe key (§6, §9).
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
    /// too short (corruption is surfaced rather than panicked, `AGENTS.md`
    /// §2.9).
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
/// surfaced, never panicked, `AGENTS.md` §2.9).
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
/// rebuildable map's key (§9 — the index is never authoritative), packed into
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
