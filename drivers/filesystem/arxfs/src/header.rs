//! Self-identifying metadata-block header (`docs/src/filesystem/arxfs-spec.md` §8 block
//! identity).
//!
//! Every metadata block arxfs writes — superblock-ring slots, transaction
//! roots, commit records, inode-tree and extent-tree nodes, and directory
//! blocks — carries a fixed-size header in its first
//! [`HEADER_LEN`] bytes. The header makes a block self-describing: it records
//! what the block *is* (`magic`, [`BlockType`], format version), which volume
//! and object it belongs to (filesystem UUID, owner object, generation),
//! where it is meant to live (its logical and physical address), and a keyed
//! authenticator over the identity plus the payload.
//!
//! Decoding verifies all of that against the address the reader *expected*,
//! so a stale, misdirected, wrong-type, torn, or bit-rotted block is rejected
//! at decode time and the caller fails closed rather than
//! trusting corrupt bytes.
//!
//! # Authenticator
//!
//! The block is sealed with an HMAC-SHA256 keyed authenticator
//! ([`mac_tag`]) computed through `lib/crypto` (crypto is
//! the standing "don't roll your own" exception). The tag covers every byte
//! of the block except the tag slot itself, so it authenticates the identity
//! (type, UUID, owner, generation, logical and physical address, payload
//! length) *and* the payload (`docs/src/filesystem/arxfs-spec.md` §8). A
//! block that fails the keyed check — because it is stale, misdirected, torn,
//! bit-rotted, or sealed under a different key — does not decode, and the
//! caller falls back to the block's redundant copy
//! ([`crate::ARXFS::read_meta`]).

use tairix_abi::DriverError;
use tairix_crypto::{ct_eq, hmac_sha256, MacKey, MacTag, MAC_TAG_LEN};

use crate::{rd_u128, rd_u32, rd_u64, wr_u128, wr_u32, wr_u64};

/// Magic in a metadata block header's first eight bytes: `"ARXFSB\3"`. The
/// trailing byte tracks the on-disk block layout; it advanced to `3` when the
/// fast physical checksum became the keyed authenticator (Stage 3). The one
/// definition lives in `lib/fsprobe`, which the volume manager's signature
/// probe shares, so the probe and this driver can never disagree.
pub use tairix_fsprobe::ARXFS_HEADER_MAGIC as HEADER_MAGIC;

/// On-disk format version understood by this build. A volume written by a
/// different version is refused rather than misread. Version 2 widened the
/// extent record to carry a physical length and a compressed flag
/// (`docs/src/filesystem/arxfs-spec.md` §10 compressed extents).
pub const FORMAT_VERSION: u32 = 2;

/// Fixed size of a metadata-block header, in bytes. The payload of a block
/// begins at this offset. It is large enough to hold the 32-byte keyed
/// authenticator alongside the identity fields.
pub const HEADER_LEN: usize = 128;

/// Largest device block the authenticator stages through its on-stack
/// scratch buffer. Matches `crate::MAX_BLOCK_SIZE`; no Tier-1 block device
/// exceeds it.
const MAX_META_BLOCK: usize = 4096;

/// The kind of object a metadata block holds. Decoding a block with a
/// `block_type` other than the one the reader expects is a misdirected or
/// corrupt read and is rejected.
#[repr(u32)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum BlockType {
    /// A superblock-ring slot (`superblock` module).
    Superblock = 1,
    /// A transaction root with its inline commit record (`transaction`).
    TxnRoot = 2,
    /// A copy-on-write B-tree node — the inode tree or a per-file extent
    /// tree (`btree` module).
    Btree = 3,
    /// A directory data block.
    Directory = 4,
    /// A scrub-progress record: the resumable cursor and accumulated counts
    /// of an online scrub (`docs/src/filesystem/arxfs-spec.md` §4 rebuildable
    /// metadata). It is reached from the transaction root and never
    /// required for ordinary crash recovery.
    ScrubProgress = 5,
    /// A device-health baseline record: the last clean device-health
    /// snapshot plus the volume's accumulated filesystem-observed fault and
    /// repair counters (`docs/src/filesystem/arxfs-spec.md` §4, §11). Like
    /// the scrub-progress record it is reached from the transaction root and
    /// is never required for ordinary crash recovery.
    HealthBaseline = 6,
    /// An inode's extended-attribute set: the encoded `tairix_fsmeta`
    /// attribute store, reached from the owning inode. Its payload is
    /// encrypted at rest under the metadata (filename) key exactly like a
    /// directory block's entry names, so no plaintext attribute key or value
    /// leaks on a raw-device read
    /// (`docs/src/filesystem/arxfs-spec.md` §21).
    Attr = 7,
    /// A block of the allocation-map region: its header, one of its summary
    /// blocks, or one of its bitmap pages (`allocmap`). Free space is
    /// rebuildable metadata, so unlike every type above it is updated in
    /// place and stored as a single copy — a page that fails to authenticate
    /// makes the mount rebuild the map rather than repair it.
    AllocMap = 8,
    /// A page of a transient scratch array a whole-volume pass streams its
    /// derived truth through (`crate::scratch`). Like an allocation-map page
    /// it is single-copy and updated in place; unlike one it exists only for
    /// the length of the pass that allocated it, and the pass writes every
    /// page before it reads any, so a page that fails to authenticate is a
    /// device fault rather than a rebuild.
    Scratch = 9,
}

impl BlockType {
    /// The raw on-disk discriminant.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self as u32
    }

    /// Recover a [`BlockType`] from its raw discriminant.
    fn from_u32(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Superblock),
            2 => Some(Self::TxnRoot),
            3 => Some(Self::Btree),
            4 => Some(Self::Directory),
            5 => Some(Self::ScrubProgress),
            6 => Some(Self::HealthBaseline),
            7 => Some(Self::Attr),
            8 => Some(Self::AllocMap),
            9 => Some(Self::Scratch),
            _ => None,
        }
    }
}

/// Every object that owns metadata blocks without being an inode, and the
/// [`BlockHeader::owner`] sentinel it stamps.
///
/// The sentinels must stay mutually distinct and out of the inode-number
/// range, so they are enumerated here once rather than declared beside each
/// owner: two owners that happened to pick the same value would seal their
/// blocks under the same identity, and a check on the owner alone could not
/// tell one's block from the other's.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ReservedOwner {
    /// The inode tree's nodes.
    InodeTree,
    /// The chunk/refcount tree's nodes (`crate::dedupe`).
    ChunkTree,
    /// The reverse-reference tree's nodes (`crate::dedupe`).
    ReverseRefTree,
    /// The scrub-progress record (`crate::scrub`).
    ScrubProgress,
    /// The device-health baseline record (`crate::health`).
    HealthBaseline,
    /// The allocation-map region (`crate::allocmap`).
    AllocMap,
    /// A reconcile pass's per-block claim-count array (`crate::scratch`).
    ScratchClaims,
    /// A check pass's reachable-inode bitmap (`crate::scratch`).
    ScratchReachable,
    /// A check pass's directory-expansion frontier (`crate::scratch`).
    ScratchFrontier,
    /// A check pass's per-inode name-count array (`crate::scratch`).
    ScratchNames,
    /// The pending-delete set's tree nodes.
    PendingDeleteTree,
}

impl ReservedOwner {
    /// The owner sentinel this object seals its blocks under. Counted down
    /// from [`u64::MAX`], so every sentinel sits far above any inode number.
    #[must_use]
    pub const fn sentinel(self) -> u64 {
        u64::MAX - (self as u64)
    }
}

/// Inode numbers are 32-bit, so the sentinels — the top handful of the 64-bit
/// space — can never collide with one, and the discriminants keep them
/// distinct from each other by construction.
const _: () = {
    assert!(ReservedOwner::InodeTree.sentinel() == u64::MAX);
    assert!(ReservedOwner::PendingDeleteTree.sentinel() > u32::MAX as u64);
};

/// The identity a metadata block carries, independent of its payload.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct BlockHeader {
    /// What kind of object the block holds.
    pub block_type: BlockType,
    /// The filesystem UUID; ties the block to one volume.
    pub fs_uuid: u128,
    /// The object that owns the block (e.g. an inode number, or `0`).
    pub owner: u64,
    /// The transaction generation that wrote the block.
    pub generation: u64,
    /// The block's logical address within its object.
    pub logical_addr: u64,
    /// The block's physical block address on the device.
    pub physical_addr: u64,
    /// Length of the meaningful payload following the header, in bytes.
    pub payload_len: u32,
}

// Header field byte offsets.
const H_MAGIC: usize = 0;
const H_TYPE: usize = 8;
const H_VERSION: usize = 12;
const H_UUID: usize = 16;
const H_OWNER: usize = 32;
const H_GENERATION: usize = 40;
const H_LOGICAL: usize = 48;
const H_PHYSICAL: usize = 56;
const H_PAYLOAD_LEN: usize = 64;
// Bytes 68..72 are reserved (zeroed). The keyed authenticator occupies
// 72..104; bytes 104..HEADER_LEN are reserved (zeroed).
const H_RESERVED: usize = 68;
const H_MAC: usize = 72;
const H_MAC_END: usize = H_MAC + MAC_TAG_LEN;

/// The HMAC-SHA256 keyed authenticator over every byte of `block` *except*
/// the tag slot, computed through `lib/crypto`. Covers the identity *and* the
/// payload, so any stale, misdirected, torn, or bit-rotted write — or a write
/// sealed under a different key — changes the result and is rejected
/// (`docs/src/filesystem/arxfs-spec.md` §5, §8).
///
/// The tag slot (`H_MAC..H_MAC_END`) is treated as zero so that the value is
/// independent of whatever tag the block currently carries; [`BlockHeader::seal`]
/// zeroes it before sealing and decoding recomputes against the same zeroed
/// view.
#[must_use]
fn mac_tag(key: &MacKey, block: &[u8]) -> MacTag {
    let len = block.len().min(MAX_META_BLOCK);
    let mut scratch = [0u8; MAX_META_BLOCK];
    scratch[..len].copy_from_slice(&block[..len]);
    for byte in &mut scratch[H_MAC..H_MAC_END] {
        *byte = 0;
    }
    hmac_sha256(key, &scratch[..len])
}

impl BlockHeader {
    /// Write `self` into the first [`HEADER_LEN`] bytes of `block` and seal
    /// the block with a fresh keyed authenticator ([`mac_tag`]) over the
    /// whole block under `key`.
    ///
    /// `block.len()` is the device block size and must be at least
    /// [`HEADER_LEN`]; the caller always passes a full block buffer.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] if `block` is shorter than
    /// [`HEADER_LEN`] (a programming error, surfaced rather than panicked).
    pub fn seal(&self, block: &mut [u8], key: &MacKey) -> Result<(), DriverError> {
        if block.len() < HEADER_LEN {
            return Err(DriverError::DeviceFault);
        }
        wr_u64(block, H_MAGIC, HEADER_MAGIC);
        wr_u32(block, H_TYPE, self.block_type.as_u32());
        wr_u32(block, H_VERSION, FORMAT_VERSION);
        wr_u128(block, H_UUID, self.fs_uuid);
        wr_u64(block, H_OWNER, self.owner);
        wr_u64(block, H_GENERATION, self.generation);
        wr_u64(block, H_LOGICAL, self.logical_addr);
        wr_u64(block, H_PHYSICAL, self.physical_addr);
        wr_u32(block, H_PAYLOAD_LEN, self.payload_len);
        // Zero the reserved gaps and the tag slot so the sealed bytes are
        // deterministic regardless of any stale buffer contents.
        for byte in &mut block[H_RESERVED..H_MAC] {
            *byte = 0;
        }
        for byte in &mut block[H_MAC..H_MAC_END] {
            *byte = 0;
        }
        for byte in &mut block[H_MAC_END..HEADER_LEN] {
            *byte = 0;
        }
        let tag = mac_tag(key, block);
        block[H_MAC..H_MAC_END].copy_from_slice(&tag);
        Ok(())
    }

    /// Decode and fully validate the header of `block`, confirming the
    /// block is the one the reader expected and that it authenticates under
    /// `key`.
    ///
    /// Verifies, in order: the block is large enough; the magic and format
    /// version match; the keyed authenticator is intact (rejecting a torn,
    /// bit-rotted, or wrong-key block); the `block_type`, filesystem UUID, and
    /// physical address match what the caller expected (rejecting a
    /// wrong-type, foreign-volume, stale, or misdirected block). Every failure
    /// is [`DriverError::DeviceFault`] so the caller fails closed.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] on any validation failure.
    pub fn decode_verify(
        block: &[u8],
        expect_type: BlockType,
        expect_uuid: u128,
        expect_physical: u64,
        key: &MacKey,
    ) -> Result<Self, DriverError> {
        if block.len() < HEADER_LEN {
            return Err(DriverError::DeviceFault);
        }
        if rd_u64(block, H_MAGIC) != HEADER_MAGIC || rd_u32(block, H_VERSION) != FORMAT_VERSION {
            return Err(DriverError::DeviceFault);
        }
        let mut stored = [0u8; MAC_TAG_LEN];
        stored.copy_from_slice(&block[H_MAC..H_MAC_END]);
        let expected = mac_tag(key, block);
        if !ct_eq(&expected, &stored) {
            return Err(DriverError::DeviceFault);
        }
        let block_type =
            BlockType::from_u32(rd_u32(block, H_TYPE)).ok_or(DriverError::DeviceFault)?;
        if block_type != expect_type {
            return Err(DriverError::DeviceFault);
        }
        let fs_uuid = rd_u128(block, H_UUID);
        if fs_uuid != expect_uuid {
            return Err(DriverError::DeviceFault);
        }
        let physical_addr = rd_u64(block, H_PHYSICAL);
        if physical_addr != expect_physical {
            return Err(DriverError::DeviceFault);
        }
        Ok(Self {
            block_type,
            fs_uuid,
            owner: rd_u64(block, H_OWNER),
            generation: rd_u64(block, H_GENERATION),
            logical_addr: rd_u64(block, H_LOGICAL),
            physical_addr,
            payload_len: rd_u32(block, H_PAYLOAD_LEN),
        })
    }

    /// Validate `block` as in [`Self::decode_verify`] but tolerate a torn or
    /// otherwise invalid block by returning `None` instead of an error.
    ///
    /// Used when scanning the superblock ring, where an unwritten or
    /// half-written slot is expected and must simply be skipped, not treated
    /// as device failure.
    #[must_use]
    pub fn try_decode(
        block: &[u8],
        expect_type: BlockType,
        expect_uuid: u128,
        expect_physical: u64,
        key: &MacKey,
    ) -> Option<Self> {
        Self::decode_verify(block, expect_type, expect_uuid, expect_physical, key).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID: u128 = 0x0123_4567_89ab_cdef_0123_4567_89ab_cdef;
    const KEY: MacKey = [0x5au8; MAC_TAG_LEN];

    fn sealed() -> [u8; 512] {
        let mut block = [0u8; 512];
        let header = BlockHeader {
            block_type: BlockType::TxnRoot,
            fs_uuid: UUID,
            owner: 7,
            generation: 42,
            logical_addr: 3,
            physical_addr: 100,
            payload_len: 16,
        };
        block[HEADER_LEN..HEADER_LEN + 4].copy_from_slice(&[1, 2, 3, 4]);
        header.seal(&mut block, &KEY).expect("seal");
        block
    }

    #[test]
    fn seal_then_decode_round_trips() {
        let block = sealed();
        let header = BlockHeader::decode_verify(&block, BlockType::TxnRoot, UUID, 100, &KEY)
            .expect("valid header decodes");
        assert_eq!(header.owner, 7);
        assert_eq!(header.generation, 42);
        assert_eq!(header.logical_addr, 3);
        assert_eq!(header.payload_len, 16);
    }

    #[test]
    fn wrong_magic_is_rejected() {
        let mut block = sealed();
        block[0] ^= 0xff;
        assert_eq!(
            BlockHeader::decode_verify(&block, BlockType::TxnRoot, UUID, 100, &KEY),
            Err(DriverError::DeviceFault)
        );
    }

    #[test]
    fn wrong_type_is_rejected() {
        let block = sealed();
        assert_eq!(
            BlockHeader::decode_verify(&block, BlockType::Btree, UUID, 100, &KEY),
            Err(DriverError::DeviceFault)
        );
    }

    #[test]
    fn wrong_expected_address_is_rejected() {
        let block = sealed();
        assert_eq!(
            BlockHeader::decode_verify(&block, BlockType::TxnRoot, UUID, 101, &KEY),
            Err(DriverError::DeviceFault)
        );
    }

    #[test]
    fn foreign_uuid_is_rejected() {
        let block = sealed();
        assert_eq!(
            BlockHeader::decode_verify(&block, BlockType::TxnRoot, UUID ^ 1, 100, &KEY),
            Err(DriverError::DeviceFault)
        );
    }

    #[test]
    fn flipped_payload_byte_is_rejected() {
        let mut block = sealed();
        block[HEADER_LEN] ^= 0xff;
        assert_eq!(
            BlockHeader::decode_verify(&block, BlockType::TxnRoot, UUID, 100, &KEY),
            Err(DriverError::DeviceFault)
        );
    }

    #[test]
    fn wrong_key_is_rejected() {
        let block = sealed();
        let other: MacKey = [0x17u8; MAC_TAG_LEN];
        assert_eq!(
            BlockHeader::decode_verify(&block, BlockType::TxnRoot, UUID, 100, &other),
            Err(DriverError::DeviceFault)
        );
    }

    #[test]
    fn a_different_format_version_is_refused() {
        // Patch the version field and re-seal the authenticator so *only*
        // the version differs: a volume written by another format version is
        // refused rather than misread, even when otherwise intact.
        let mut block = sealed();
        block[H_VERSION..H_VERSION + 4].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
        let tag = mac_tag(&KEY, &block);
        block[H_MAC..H_MAC_END].copy_from_slice(&tag);
        assert_eq!(
            BlockHeader::decode_verify(&block, BlockType::TxnRoot, UUID, 100, &KEY),
            Err(DriverError::DeviceFault)
        );
    }

    #[test]
    fn try_decode_skips_a_torn_slot() {
        let mut block = sealed();
        block[H_MAC] ^= 0xff;
        assert_eq!(
            BlockHeader::try_decode(&block, BlockType::TxnRoot, UUID, 100, &KEY),
            None
        );
    }
}
