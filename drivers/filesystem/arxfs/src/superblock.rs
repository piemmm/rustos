//! The superblock ring (`docs/src/filesystem/arxfs-spec.md` §4 / §14).
//!
//! A arxfs volume opens at a ring of [`RING_SLOTS`] superblock slots, one
//! per block at the very start of the device. Each slot is a self-identifying
//! block (`header` module, [`BlockType::Superblock`]) whose payload pins the
//! volume's fixed geometry and points at the **transaction root** committed by
//! one transaction generation.
//!
//! Committing publishes a new slot round-robin and the highest valid
//! generation wins, so a crash during a publish falls back to the previous
//! slot — the previously committed root — rather than a torn one. `open`
//! scans the ring and selects the highest-generation slot that decodes
//! cleanly *and* whose referenced root also decodes cleanly (`transaction`).

use tairix_abi::DriverError;
use tairix_crypto::MacKey;

use crate::crypto::CRYPTO_HEADER_LEN;
use crate::header::{BlockHeader, BlockType, HEADER_LEN};

/// Number of logical superblock-ring slots. Four slots retain a short window
/// of recent transaction roots while keeping the ring scan trivial.
pub const RING_SLOTS: u64 = 4;

/// Physical blocks the ring occupies at the start of the device. Each logical
/// slot is stored in a **mirrored pair** of adjacent blocks (the primary and
/// its companion at `primary + 1`), so the committed superblock always has two
/// physical copies (`docs/src/filesystem/arxfs-spec.md` §5 — critical
/// metadata copies: 2 minimum). The mirroring uses the same companion rule
/// (`primary + 1`) as every other metadata block, so there is one redundancy
/// mechanism, not two.
///
/// The one definition lives in `lib/fsprobe` (like the header magic): the
/// verified re-insert path sizes its mutation-evidence window from the same
/// value, so the driver and the verifier can never disagree about where the
/// ring ends.
pub use tairix_fsprobe::ARXFS_RING_BLOCKS as RING_BLOCKS;

/// The shared ring constant covers exactly the mirrored slots this module
/// lays out.
const _: () = assert!(RING_BLOCKS == RING_SLOTS * 2);

/// Primary block address of logical ring slot `slot` (`0..RING_SLOTS`); its
/// companion mirror lives at `slot_block(slot) + 1`.
#[must_use]
pub const fn slot_block(slot: u64) -> u64 {
    slot * 2
}

// Superblock payload field offsets, relative to the end of the header.
const P_BLOCK_SIZE: usize = HEADER_LEN;
const P_TOTAL_BLOCKS: usize = HEADER_LEN + 8;
const P_INODE_COUNT: usize = HEADER_LEN + 16;
const P_GENERATION: usize = HEADER_LEN + 24;
const P_ROOT_PHYS: usize = HEADER_LEN + 32;
/// Offset, within the block, of the plaintext crypto discovery header — the
/// wrapped master key and its salt (`crate::crypto`). It follows the geometry
/// fields and precedes the keyed authenticator written by the header.
pub const CRYPTO_OFFSET: usize = HEADER_LEN + 40;
/// Offset, within the block, of the incompatible-feature word.
///
/// It sits after the crypto discovery header and is itself plaintext,
/// because a reader must learn whether it understands the volume's on-disk
/// features *before* it can unwrap a key or read a single tree node.
const P_INCOMPAT: usize = CRYPTO_OFFSET + CRYPTO_HEADER_LEN;
/// Bytes of meaningful superblock payload following the header: the 40-byte
/// geometry block, the crypto discovery header, and the 8-byte feature word.
// `CRYPTO_HEADER_LEN` is a tiny compile-time constant (< 256), so the
// `usize`-to-`u32` widening cannot truncate.
#[allow(clippy::cast_possible_truncation)]
const PAYLOAD_LEN: u32 = 40 + CRYPTO_HEADER_LEN as u32 + 8;

/// The volume stores symbolic links: inodes of on-disk kind `3`, whose
/// target is held as node data (`docs/src/filesystem/arxfs-spec.md` §20).
///
/// A reader that does not know the kind would read a link inode as
/// structurally invalid, so a volume that holds one declares the feature and
/// a reader lacking it refuses the mount instead. The bit is set by the
/// **first** link a volume gets, not at format time, so a volume that has
/// never held a link stays mountable by a link-unaware reader.
pub const INCOMPAT_SYMLINKS: u64 = 1 << 0;

/// Every incompatible on-disk feature this build understands.
///
/// A volume declaring a bit outside this set is refused at mount rather than
/// misread: the whole point of the word is that an unrecognised structure is
/// a definite "no", never a guess.
pub const INCOMPAT_SUPPORTED: u64 = INCOMPAT_SYMLINKS;

fn rd_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

fn rd_u64(buf: &[u8], off: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&buf[off..off + 8]);
    u64::from_le_bytes(bytes)
}

fn wr_u32(buf: &mut [u8], off: usize, value: u32) {
    buf[off..off + 4].copy_from_slice(&value.to_le_bytes());
}

fn wr_u64(buf: &mut [u8], off: usize, value: u64) {
    buf[off..off + 8].copy_from_slice(&value.to_le_bytes());
}

/// One decoded superblock-ring slot.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Superblock {
    /// Device block size in bytes (pinned at format time).
    pub block_size: u32,
    /// Total device blocks.
    pub total_blocks: u64,
    /// Number of inodes the volume was formatted with.
    pub inode_count: u32,
    /// Transaction generation that wrote this slot.
    pub generation: u64,
    /// Physical block address of the transaction root this slot commits.
    pub root_phys: u64,
    /// Incompatible on-disk features the volume uses. A reader that does not
    /// understand every set bit must refuse the volume, so the word is
    /// plaintext and validated before anything else is read.
    pub incompat: u64,
}

impl Superblock {
    /// Encode this superblock into `block`, sealing it with a header that
    /// names physical address `phys` and the matching `fs_uuid`.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] if `block` is too small to hold the
    /// payload (a programming error, surfaced rather than panicked).
    pub fn seal(
        &self,
        block: &mut [u8],
        fs_uuid: u128,
        phys: u64,
        key: &MacKey,
        crypto_header: &[u8; CRYPTO_HEADER_LEN],
    ) -> Result<(), DriverError> {
        if block.len() < P_INCOMPAT + 8 {
            return Err(DriverError::DeviceFault);
        }
        for byte in block.iter_mut() {
            *byte = 0;
        }
        wr_u32(block, P_BLOCK_SIZE, self.block_size);
        wr_u64(block, P_TOTAL_BLOCKS, self.total_blocks);
        wr_u32(block, P_INODE_COUNT, self.inode_count);
        wr_u64(block, P_GENERATION, self.generation);
        wr_u64(block, P_ROOT_PHYS, self.root_phys);
        block[CRYPTO_OFFSET..CRYPTO_OFFSET + CRYPTO_HEADER_LEN].copy_from_slice(crypto_header);
        wr_u64(block, P_INCOMPAT, self.incompat);
        let header = BlockHeader {
            block_type: BlockType::Superblock,
            fs_uuid,
            owner: 0,
            generation: self.generation,
            logical_addr: 0,
            physical_addr: phys,
            payload_len: PAYLOAD_LEN,
        };
        header.seal(block, key)
    }

    /// Decode and validate a superblock slot living at physical address
    /// `phys`, returning `Ok(None)` if the slot is unwritten, torn, foreign,
    /// or otherwise invalid (the ring scan skips such slots).
    ///
    /// On the very first scan the caller does not yet know the volume UUID;
    /// passing `None` accepts whatever UUID a valid slot carries, which the
    /// caller then pins for the rest of the scan.
    ///
    /// The keyed authenticator's `key` is the volume's metadata-authentication
    /// key, which the caller has already recovered by unwrapping the master
    /// key with the volume key (`crate::crypto`); the scan only authenticates
    /// slots under it, it does not derive it.
    ///
    /// # Errors
    ///
    /// [`DriverError::Unsupported`] when the slot authenticates but declares
    /// an on-disk feature outside [`INCOMPAT_SUPPORTED`]. That is a definite
    /// answer, not a slot to skip: the feature word is covered by the keyed
    /// authenticator, so a bit that survives the check is one the volume's
    /// own writer really set, and mounting past it would misread structure
    /// this build does not know. Refusing here states the reason instead of
    /// leaving the ring scan to report the volume as unrecognisable.
    pub fn try_decode(
        block: &[u8],
        expect_uuid: Option<u128>,
        phys: u64,
        key: &MacKey,
    ) -> Result<Option<(Self, u128)>, DriverError> {
        if block.len() < P_INCOMPAT + 8 {
            return Ok(None);
        }
        let probe_uuid = expect_uuid.unwrap_or_else(|| {
            let mut bytes = [0u8; 16];
            bytes.copy_from_slice(&block[16..32]);
            u128::from_le_bytes(bytes)
        });
        let Some(header) =
            BlockHeader::try_decode(block, BlockType::Superblock, probe_uuid, phys, key)
        else {
            return Ok(None);
        };
        let sb = Self {
            block_size: rd_u32(block, P_BLOCK_SIZE),
            total_blocks: rd_u64(block, P_TOTAL_BLOCKS),
            inode_count: rd_u32(block, P_INODE_COUNT),
            generation: rd_u64(block, P_GENERATION),
            root_phys: rd_u64(block, P_ROOT_PHYS),
            incompat: rd_u64(block, P_INCOMPAT),
        };
        if sb.incompat & !INCOMPAT_SUPPORTED != 0 {
            return Err(DriverError::Unsupported);
        }
        if sb.generation != header.generation {
            return Ok(None);
        }
        Ok(Some((sb, header.fs_uuid)))
    }
}
