//! The superblock ring (`.junie/RUSTFS.md` §4 / §14).
//!
//! A rustfs volume opens at a ring of [`RING_SLOTS`] superblock slots, one
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

use rustos_abi::DriverError;

use crate::header::{BlockHeader, BlockType, HEADER_LEN};

/// Number of superblock-ring slots. Four slots retain a short window of
/// recent transaction roots while keeping the ring scan trivial.
pub const RING_SLOTS: u64 = 4;

// Superblock payload field offsets, relative to the end of the header.
const P_BLOCK_SIZE: usize = HEADER_LEN;
const P_TOTAL_BLOCKS: usize = HEADER_LEN + 8;
const P_INODE_COUNT: usize = HEADER_LEN + 16;
const P_GENERATION: usize = HEADER_LEN + 24;
const P_ROOT_PHYS: usize = HEADER_LEN + 32;
/// Bytes of meaningful superblock payload following the header.
const PAYLOAD_LEN: u32 = 40;

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
}

impl Superblock {
    /// Encode this superblock into `block`, sealing it with a header that
    /// names physical address `phys` and the matching `fs_uuid`.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] if `block` is too small to hold the
    /// payload (a programming error, surfaced rather than panicked).
    pub fn seal(&self, block: &mut [u8], fs_uuid: u128, phys: u64) -> Result<(), DriverError> {
        if block.len() < P_ROOT_PHYS + 8 {
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
        let header = BlockHeader {
            block_type: BlockType::Superblock,
            fs_uuid,
            owner: 0,
            generation: self.generation,
            logical_addr: 0,
            physical_addr: phys,
            payload_len: PAYLOAD_LEN,
        };
        header.seal(block)
    }

    /// Decode and validate a superblock slot living at physical address
    /// `phys`, returning `None` if the slot is unwritten, torn, foreign, or
    /// otherwise invalid (the ring scan skips such slots).
    ///
    /// On the very first scan the caller does not yet know the volume UUID;
    /// passing `None` accepts whatever UUID a valid slot carries, which the
    /// caller then pins for the rest of the scan. An invalid slot is `None`.
    #[must_use]
    pub fn try_decode(block: &[u8], expect_uuid: Option<u128>, phys: u64) -> Option<(Self, u128)> {
        if expect_uuid.is_none() && block.len() < HEADER_LEN {
            return None;
        }
        let probe_uuid = expect_uuid.unwrap_or_else(|| {
            let mut bytes = [0u8; 16];
            bytes.copy_from_slice(&block[16..32]);
            u128::from_le_bytes(bytes)
        });
        let header = BlockHeader::try_decode(block, BlockType::Superblock, probe_uuid, phys)?;
        let sb = Self {
            block_size: rd_u32(block, P_BLOCK_SIZE),
            total_blocks: rd_u64(block, P_TOTAL_BLOCKS),
            inode_count: rd_u32(block, P_INODE_COUNT),
            generation: rd_u64(block, P_GENERATION),
            root_phys: rd_u64(block, P_ROOT_PHYS),
        };
        if sb.generation != header.generation {
            return None;
        }
        Some((sb, header.fs_uuid))
    }
}
