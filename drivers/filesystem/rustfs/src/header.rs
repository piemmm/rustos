//! Self-identifying metadata-block header (`.junie/RUSTFS.md` §8 block
//! identity).
//!
//! Every metadata block rustfs writes — superblock-ring slots, transaction
//! roots, commit records, the inode map, inode blocks, indirect blocks, and
//! directory blocks — carries a fixed-size header in its first
//! [`HEADER_LEN`] bytes. The header makes a block self-describing: it records
//! what the block *is* (`magic`, [`BlockType`], format version), which volume
//! and object it belongs to (filesystem UUID, owner object, generation),
//! where it is meant to live (its logical and physical address), and a
//! checksum over the identity plus the payload.
//!
//! Decoding verifies all of that against the address the reader *expected*,
//! so a stale, misdirected, wrong-type, or torn block is rejected at decode
//! time and the caller fails closed (`AGENTS.md` §5.4) rather than trusting
//! corrupt bytes.
//!
//! # Checksum
//!
//! Stage 1 uses the fast physical checksum ([`checksum`]); the keyed
//! authenticator arrives with encryption (`.junie/RUSTFS.md` §5, Stage 3/4)
//! and replaces only [`checksum`], leaving this layout intact.

use rustos_abi::DriverError;

/// Magic in a metadata block header's first eight bytes: `"RUSTFSB\2"`.
pub const HEADER_MAGIC: u64 = 0x5255_5354_4653_4202;

/// On-disk format version understood by this build. A volume written by a
/// different version is refused rather than misread.
pub const FORMAT_VERSION: u32 = 1;

/// Fixed size of a metadata-block header, in bytes. The payload of a block
/// begins at this offset.
pub const HEADER_LEN: usize = 96;

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
    /// A block of the copy-on-write inode map.
    InodeMap = 3,
    /// A block holding packed inode records.
    Inode = 4,
    /// A file's single-indirect pointer block.
    Indirect = 5,
    /// A directory data block.
    Directory = 6,
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
            3 => Some(Self::InodeMap),
            4 => Some(Self::Inode),
            5 => Some(Self::Indirect),
            6 => Some(Self::Directory),
            _ => None,
        }
    }
}

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
const H_CHECKSUM: usize = 72;
const H_CHECKSUM_END: usize = 80;

fn rd_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

fn rd_u64(buf: &[u8], off: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&buf[off..off + 8]);
    u64::from_le_bytes(bytes)
}

fn rd_u128(buf: &[u8], off: usize) -> u128 {
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&buf[off..off + 16]);
    u128::from_le_bytes(bytes)
}

fn wr_u32(buf: &mut [u8], off: usize, value: u32) {
    buf[off..off + 4].copy_from_slice(&value.to_le_bytes());
}

fn wr_u64(buf: &mut [u8], off: usize, value: u64) {
    buf[off..off + 8].copy_from_slice(&value.to_le_bytes());
}

fn wr_u128(buf: &mut [u8], off: usize, value: u128) {
    buf[off..off + 16].copy_from_slice(&value.to_le_bytes());
}

/// The fast physical checksum (FNV-1a, 64-bit) over every byte of `block`
/// except the eight-byte checksum slot. Covers the identity *and* the
/// payload, so any stale, misdirected, or torn write changes the result
/// (`.junie/RUSTFS.md` §5). The keyed authenticator (Stage 3/4) replaces
/// only this function.
#[must_use]
pub fn checksum(block: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for (i, byte) in block.iter().enumerate() {
        if (H_CHECKSUM..H_CHECKSUM_END).contains(&i) {
            continue;
        }
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

impl BlockHeader {
    /// Write `self` into the first [`HEADER_LEN`] bytes of `block` and seal
    /// the block with a fresh [`checksum`] over the whole block.
    ///
    /// `block.len()` is the device block size and must be at least
    /// [`HEADER_LEN`]; the caller always passes a full block buffer.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] if `block` is shorter than
    /// [`HEADER_LEN`] (a programming error, surfaced rather than panicked).
    pub fn seal(&self, block: &mut [u8]) -> Result<(), DriverError> {
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
        wr_u32(block, 68, 0);
        block[H_CHECKSUM..H_CHECKSUM_END].copy_from_slice(&0u64.to_le_bytes());
        let sum = checksum(block);
        wr_u64(block, H_CHECKSUM, sum);
        Ok(())
    }

    /// Decode and fully validate the header of `block`, confirming the
    /// block is the one the reader expected.
    ///
    /// Verifies, in order: the block is large enough; the magic and format
    /// version match; the checksum is intact (rejecting a torn block); the
    /// `block_type`, filesystem UUID, and physical address match what the
    /// caller expected (rejecting a wrong-type, foreign-volume, stale, or
    /// misdirected block). Every failure is [`DriverError::DeviceFault`] so
    /// the caller fails closed (`AGENTS.md` §5.4).
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] on any validation failure.
    pub fn decode_verify(
        block: &[u8],
        expect_type: BlockType,
        expect_uuid: u128,
        expect_physical: u64,
    ) -> Result<Self, DriverError> {
        if block.len() < HEADER_LEN {
            return Err(DriverError::DeviceFault);
        }
        if rd_u64(block, H_MAGIC) != HEADER_MAGIC || rd_u32(block, H_VERSION) != FORMAT_VERSION {
            return Err(DriverError::DeviceFault);
        }
        let stored = rd_u64(block, H_CHECKSUM);
        if stored != checksum(block) {
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
    ) -> Option<Self> {
        Self::decode_verify(block, expect_type, expect_uuid, expect_physical).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID: u128 = 0x0123_4567_89ab_cdef_0123_4567_89ab_cdef;

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
        header.seal(&mut block).expect("seal");
        block
    }

    #[test]
    fn seal_then_decode_round_trips() {
        let block = sealed();
        let header = BlockHeader::decode_verify(&block, BlockType::TxnRoot, UUID, 100)
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
            BlockHeader::decode_verify(&block, BlockType::TxnRoot, UUID, 100),
            Err(DriverError::DeviceFault)
        );
    }

    #[test]
    fn wrong_type_is_rejected() {
        let block = sealed();
        assert_eq!(
            BlockHeader::decode_verify(&block, BlockType::Inode, UUID, 100),
            Err(DriverError::DeviceFault)
        );
    }

    #[test]
    fn wrong_expected_address_is_rejected() {
        let block = sealed();
        assert_eq!(
            BlockHeader::decode_verify(&block, BlockType::TxnRoot, UUID, 101),
            Err(DriverError::DeviceFault)
        );
    }

    #[test]
    fn foreign_uuid_is_rejected() {
        let block = sealed();
        assert_eq!(
            BlockHeader::decode_verify(&block, BlockType::TxnRoot, UUID ^ 1, 100),
            Err(DriverError::DeviceFault)
        );
    }

    #[test]
    fn flipped_checksum_payload_byte_is_rejected() {
        let mut block = sealed();
        block[HEADER_LEN] ^= 0xff;
        assert_eq!(
            BlockHeader::decode_verify(&block, BlockType::TxnRoot, UUID, 100),
            Err(DriverError::DeviceFault)
        );
    }

    #[test]
    fn try_decode_skips_a_torn_slot() {
        let mut block = sealed();
        block[H_CHECKSUM] ^= 0xff;
        assert_eq!(
            BlockHeader::try_decode(&block, BlockType::TxnRoot, UUID, 100),
            None
        );
    }
}
