//! The transaction root and its inline commit record
//! (`docs/src/filesystem/arxfs-spec.md` §14).
//!
//! A transaction root is the single block a committed transaction publishes
//! through the superblock ring (`superblock` module). It is a self-identifying
//! block ([`BlockType::TxnRoot`]) whose payload names the roots of the
//! copy-on-write metadata the transaction produced — the inode-tree root, the
//! next free inode number, and the pending-delete set naming every inode still
//! to be reclaimed — and ends with a **commit record**: a commit magic plus a
//! second copy of the generation.
//!
//! Co-locating the commit record in the same sealed block makes commit atomic
//! against a torn write: the block's checksum (`header`) and the commit
//! record are validated together, so a half-written root is rejected and the
//! ring falls back to the previous committed root. The commit order is
//! therefore: send every copy-on-write block and this root (carrying its commit
//! record) to the device, barrier, then publish the superblock slot pointing at
//! it. The barrier is what enforces that order on a device free to reorder
//! within its write cache (`docs/src/filesystem/arxfs-spec.md` §22).

use tairix_abi::DriverError;
use tairix_crypto::MacKey;

use crate::header::{BlockHeader, BlockType, HEADER_LEN};

/// Commit-record magic stored in the root payload trailer: `"RFSCMMIT"`.
const COMMIT_MAGIC: u64 = 0x5246_5343_4d4d_4954;

// Transaction-root payload field offsets, relative to the end of the header.
const P_GENERATION: usize = HEADER_LEN;
const P_INODE_TREE_ROOT: usize = HEADER_LEN + 8;
const P_NEXT_INO: usize = HEADER_LEN + 16;
const P_CHUNK_TREE_ROOT: usize = HEADER_LEN + 24;
const P_REVERSE_REF_TREE_ROOT: usize = HEADER_LEN + 32;
const P_SCRUB_PROGRESS_ROOT: usize = HEADER_LEN + 40;
const P_HEALTH_BASELINE_ROOT: usize = HEADER_LEN + 48;
const P_ALLOC_MAP_START: usize = HEADER_LEN + 56;
const P_ALLOC_MAP_COVERED: usize = HEADER_LEN + 64;
const P_FREE_COUNT: usize = HEADER_LEN + 72;
const P_PENDING_DELETE_ROOT: usize = HEADER_LEN + 80;
const P_COMMIT_MAGIC: usize = HEADER_LEN + 88;
const P_COMMIT_GENERATION: usize = HEADER_LEN + 96;
/// Bytes of meaningful transaction-root payload following the header.
const PAYLOAD_LEN: u32 = 104;

fn rd_u64(buf: &[u8], off: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&buf[off..off + 8]);
    u64::from_le_bytes(bytes)
}

fn wr_u64(buf: &mut [u8], off: usize, value: u64) {
    buf[off..off + 8].copy_from_slice(&value.to_le_bytes());
}

/// One decoded transaction root.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct TxnRoot {
    /// Transaction generation this root commits.
    pub generation: u64,
    /// Physical block of the inode-tree root node, or `0` when the volume
    /// holds no inodes yet.
    pub inode_tree_root: u64,
    /// The next inode number to hand out (the allocation high-water mark);
    /// every live inode number is below it.
    pub next_ino: u64,
    /// Physical block of the chunk/refcount-tree root, or `0` when the volume
    /// holds no shared chunks yet (`docs/src/filesystem/arxfs-spec.md` §4, §9).
    pub chunk_tree_root: u64,
    /// Physical block of the reverse-reference-tree root, or `0` when no chunk
    /// has recorded referrers yet (`docs/src/filesystem/arxfs-spec.md` §4, §9).
    pub reverse_ref_tree_root: u64,
    /// Physical block of the scrub-progress record, or `0` when no online
    /// scrub is mid-pass. It holds a resumable scrub's cursor and accumulated
    /// counts (rebuildable metadata, `docs/src/filesystem/arxfs-spec.md` §4); a crash mid-scrub leaves it set but never blocks an ordinary
    /// mount.
    pub scrub_progress_root: u64,
    /// Physical block of the device-health baseline record, or `0` when no
    /// baseline has been stored yet. It holds the last clean device-health
    /// snapshot and the volume's accumulated filesystem-observed fault
    /// counters (`docs/src/filesystem/arxfs-spec.md` §4, §11); a crash
    /// mid-update leaves the previous baseline (or none) selected and never
    /// blocks an ordinary mount.
    pub health_baseline_root: u64,
    /// Physical block of the pending-delete set's tree root, or `0` when no
    /// inode awaits reclaim. The set names every inode the volume has detached
    /// from its last name and not yet finished freeing, so an interrupted
    /// delete is something the next mount finds and completes rather than an
    /// unreachable inode holding blocks for the life of the volume
    /// (`docs/src/filesystem/arxfs-spec.md` §4, §14).
    pub pending_delete_root: u64,
    /// First block of the allocation-map region (`allocmap`), or `0` when the
    /// volume has none. Free space is rebuildable, so the region is updated in
    /// place rather than copy-on-written; the root only records where it lives
    /// and what it covers, and the region's own header says whether its last
    /// update finished.
    pub alloc_map_start: u64,
    /// Device blocks the allocation map covers. A map whose coverage does not
    /// match the committed volume size is stale and the mount rebuilds it.
    pub alloc_map_covered: u64,
    /// Free blocks in the committed volume. Recorded here so a read-only mount
    /// — which builds no allocation state at all — still reports honest volume
    /// statistics without reading or rebuilding the map.
    pub free_count: u64,
}

impl TxnRoot {
    /// Encode this root and its commit record into `block`, sealing it with a
    /// header naming physical address `phys` and `fs_uuid`.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] if `block` cannot hold the payload (a
    /// programming error, surfaced rather than panicked).
    pub fn seal(
        &self,
        block: &mut [u8],
        fs_uuid: u128,
        phys: u64,
        key: &MacKey,
    ) -> Result<(), DriverError> {
        if block.len() < P_COMMIT_GENERATION + 8 {
            return Err(DriverError::DeviceFault);
        }
        for byte in block.iter_mut() {
            *byte = 0;
        }
        wr_u64(block, P_GENERATION, self.generation);
        wr_u64(block, P_INODE_TREE_ROOT, self.inode_tree_root);
        wr_u64(block, P_NEXT_INO, self.next_ino);
        wr_u64(block, P_CHUNK_TREE_ROOT, self.chunk_tree_root);
        wr_u64(block, P_REVERSE_REF_TREE_ROOT, self.reverse_ref_tree_root);
        wr_u64(block, P_SCRUB_PROGRESS_ROOT, self.scrub_progress_root);
        wr_u64(block, P_HEALTH_BASELINE_ROOT, self.health_baseline_root);
        wr_u64(block, P_ALLOC_MAP_START, self.alloc_map_start);
        wr_u64(block, P_ALLOC_MAP_COVERED, self.alloc_map_covered);
        wr_u64(block, P_FREE_COUNT, self.free_count);
        wr_u64(block, P_PENDING_DELETE_ROOT, self.pending_delete_root);
        wr_u64(block, P_COMMIT_MAGIC, COMMIT_MAGIC);
        wr_u64(block, P_COMMIT_GENERATION, self.generation);
        let header = BlockHeader {
            block_type: BlockType::TxnRoot,
            fs_uuid,
            owner: 0,
            generation: self.generation,
            logical_addr: 0,
            physical_addr: phys,
            payload_len: PAYLOAD_LEN,
        };
        header.seal(block, key)
    }

    /// Decode and validate the transaction root at physical address `phys`,
    /// rejecting a torn block, a foreign UUID, a generation mismatch, or an
    /// absent/incomplete commit record.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] if the root is invalid; the caller treats
    /// that as "this slot did not commit" and falls back in the ring.
    pub fn decode_verify(
        block: &[u8],
        fs_uuid: u128,
        phys: u64,
        expect_generation: u64,
        key: &MacKey,
    ) -> Result<Self, DriverError> {
        let header = BlockHeader::decode_verify(block, BlockType::TxnRoot, fs_uuid, phys, key)?;
        let generation = rd_u64(block, P_GENERATION);
        if generation != header.generation || generation != expect_generation {
            return Err(DriverError::DeviceFault);
        }
        if rd_u64(block, P_COMMIT_MAGIC) != COMMIT_MAGIC
            || rd_u64(block, P_COMMIT_GENERATION) != generation
        {
            return Err(DriverError::DeviceFault);
        }
        Ok(Self {
            generation,
            alloc_map_start: rd_u64(block, P_ALLOC_MAP_START),
            alloc_map_covered: rd_u64(block, P_ALLOC_MAP_COVERED),
            free_count: rd_u64(block, P_FREE_COUNT),
            inode_tree_root: rd_u64(block, P_INODE_TREE_ROOT),
            next_ino: rd_u64(block, P_NEXT_INO),
            chunk_tree_root: rd_u64(block, P_CHUNK_TREE_ROOT),
            reverse_ref_tree_root: rd_u64(block, P_REVERSE_REF_TREE_ROOT),
            scrub_progress_root: rd_u64(block, P_SCRUB_PROGRESS_ROOT),
            health_baseline_root: rd_u64(block, P_HEALTH_BASELINE_ROOT),
            pending_delete_root: rd_u64(block, P_PENDING_DELETE_ROOT),
        })
    }

    /// Decode and validate the transaction root at physical address `phys`
    /// **without** an externally-supplied expected generation: the root is
    /// accepted when its header authenticates and its inline commit record is
    /// internally consistent (the commit magic is present and the commit
    /// generation matches the header generation). Used by the offline rescue
    /// path (Stage 9), which scans for committed roots when the superblock ring
    /// no longer names one; the generation it recovers is the root's own.
    ///
    /// Returns `None` for any block that is not a valid committed root at
    /// `phys` (corruption is surfaced as a skip, never panicked).
    #[must_use]
    pub fn decode_any(block: &[u8], fs_uuid: u128, phys: u64, key: &MacKey) -> Option<Self> {
        let header = BlockHeader::try_decode(block, BlockType::TxnRoot, fs_uuid, phys, key)?;
        let generation = rd_u64(block, P_GENERATION);
        if generation != header.generation {
            return None;
        }
        if rd_u64(block, P_COMMIT_MAGIC) != COMMIT_MAGIC
            || rd_u64(block, P_COMMIT_GENERATION) != generation
        {
            return None;
        }
        Some(Self {
            generation,
            alloc_map_start: rd_u64(block, P_ALLOC_MAP_START),
            alloc_map_covered: rd_u64(block, P_ALLOC_MAP_COVERED),
            free_count: rd_u64(block, P_FREE_COUNT),
            inode_tree_root: rd_u64(block, P_INODE_TREE_ROOT),
            next_ino: rd_u64(block, P_NEXT_INO),
            chunk_tree_root: rd_u64(block, P_CHUNK_TREE_ROOT),
            reverse_ref_tree_root: rd_u64(block, P_REVERSE_REF_TREE_ROOT),
            scrub_progress_root: rd_u64(block, P_SCRUB_PROGRESS_ROOT),
            health_baseline_root: rd_u64(block, P_HEALTH_BASELINE_ROOT),
            pending_delete_root: rd_u64(block, P_PENDING_DELETE_ROOT),
        })
    }
}
