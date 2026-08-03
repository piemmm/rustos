//! Shared host-test doubles for the RAID array composer.
//!
//! Both halves of the driver's live side are proven against the same fake
//! disks: the assembly half next door in `service`, and the serving and
//! self-maintenance half in `runtime`. The double lives here so neither test
//! module carries its own copy of a member disk that could drift from the
//! other's.

use alloc::rc::Rc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use crate::service::write_superblock;

use tairix_abi::blkio::{BlkOp, BlkRequest, BLK_REQUEST_LEN};
use tairix_abi::driver::block::{Block, BlockGeometry};
use tairix_abi::driver::DriverError;
use tairix_abi::raid::RaidLevel;
use tairix_abi::time::Time64;
use tairix_raid::{ArrayIdentity, ArraySuperblock, ArrayUuid, Candidate, SuperblockError};
use tairix_raidmeta::RESERVED_METADATA_BLOCKS;

pub(crate) const UUID_A: ArrayUuid = [0xA1; 16];
pub(crate) const UUID_B: ArrayUuid = [0xB2; 16];

/// Logical block size every member in these tests reports.
pub(crate) const BLOCK_SIZE: u32 = 512;

/// Blocks each member device holds in total, its reserved metadata included.
/// Chosen so the data region left past that metadata is a whole number of
/// stripe chunks, which a striped level requires.
pub(crate) const DEVICE_BLOCKS: u64 = 66;

/// Blocks of a member left for array data once its reserved metadata is
/// excluded — the span the composed view covers.
pub(crate) const DATA_BLOCKS: u64 = DEVICE_BLOCKS - RESERVED_METADATA_BLOCKS;

/// The stripe unit every striped array in these tests uses.
pub(crate) const CHUNK: u32 = 8;

/// When a member's metadata was last written before assembly.
pub(crate) const STAMPED_AT: Time64 = Time64::from_secs(1_700_000_000);

/// The instant assembly runs at, distinct from [`STAMPED_AT`] so a re-stamp is
/// visible on the disk rather than having to be taken on trust.
pub(crate) const NOW: Time64 = Time64::from_secs(1_700_000_999);

/// The backing store of one member device double.
struct DiskState {
    bytes: Vec<u8>,
    block_size: u32,
    /// Reject every geometry query: a device that cannot say what it is.
    geometry_fails: bool,
    /// Reject every write: a disk that cannot be re-stamped.
    write_fails: bool,
}

/// A handle to a member device double.
///
/// Cloning it yields a second handle to the *same* disk. Assembly moves a
/// member device into the composed array, so that is how a test inspects what
/// actually landed on a disk the array now owns — the on-disk re-stamp of a
/// degraded start is the assertion that matters most here, and taking it on
/// trust would prove nothing.
#[derive(Clone)]
pub(crate) struct MemberDisk(Rc<RefCell<DiskState>>);

impl MemberDisk {
    /// An empty device of `device_blocks` logical blocks.
    pub(crate) fn new(device_blocks: u64) -> Self {
        let len = usize::try_from(device_blocks).expect("a test device fits the host")
            * BLOCK_SIZE as usize;
        Self(Rc::new(RefCell::new(DiskState {
            bytes: vec![0u8; len],
            block_size: BLOCK_SIZE,
            geometry_fails: false,
            write_fails: false,
        })))
    }

    /// Report a block size of `block_size` rather than the real one.
    pub(crate) fn with_block_size(self, block_size: u32) -> Self {
        self.0.borrow_mut().block_size = block_size;
        self
    }

    /// Fail every geometry query from here on.
    pub(crate) fn breaking_geometry(self) -> Self {
        self.0.borrow_mut().geometry_fails = true;
        self
    }

    /// Fail every write from here on.
    pub(crate) fn refusing_writes(self) -> Self {
        self.0.borrow_mut().write_fails = true;
        self
    }

    /// The byte offset of device block `lba`.
    fn offset(state: &DiskState, lba: u64) -> usize {
        usize::try_from(lba).expect("a test lba fits the host") * state.block_size as usize
    }

    /// Fill device block `lba` with `fill`, bypassing the [`Block`] surface, so
    /// a test can plant data at a known *device* LBA.
    pub(crate) fn plant(&self, lba: u64, fill: u8) {
        let mut state = self.0.borrow_mut();
        let at = Self::offset(&state, lba);
        let end = at + state.block_size as usize;
        state.bytes[at..end].fill(fill);
    }

    /// Whether device block `lba` is entirely zero.
    pub(crate) fn block_is_blank(&self, lba: u64) -> bool {
        let state = self.0.borrow();
        let at = Self::offset(&state, lba);
        state.bytes[at..at + state.block_size as usize]
            .iter()
            .all(|&byte| byte == 0)
    }

    /// Corrupt one byte inside the sealed superblock record.
    pub(crate) fn corrupt_metadata(&self) {
        self.0.borrow_mut().bytes[16] ^= 0xFF;
    }

    /// The metadata currently on the disk, decoded exactly as a later
    /// discovery would decode it.
    pub(crate) fn on_disk_metadata(&self) -> Result<ArraySuperblock, SuperblockError> {
        ArraySuperblock::decode(&self.0.borrow().bytes)
    }
}

impl Block for MemberDisk {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        let state = self.0.borrow();
        if state.geometry_fails {
            return Err(DriverError::DeviceOffline);
        }
        Ok(BlockGeometry {
            block_size: state.block_size,
            block_count: (state.bytes.len() / state.block_size as usize) as u64,
        })
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        let state = self.0.borrow();
        let at = Self::offset(&state, lba);
        let end = at.checked_add(buf.len()).ok_or(DriverError::OutOfRange)?;
        if end > state.bytes.len() {
            return Err(DriverError::OutOfRange);
        }
        buf.copy_from_slice(&state.bytes[at..end]);
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        let mut state = self.0.borrow_mut();
        if state.write_fails {
            return Err(DriverError::MediumError);
        }
        let at = Self::offset(&state, lba);
        let end = at.checked_add(buf.len()).ok_or(DriverError::OutOfRange)?;
        if end > state.bytes.len() {
            return Err(DriverError::OutOfRange);
        }
        state.bytes[at..end].copy_from_slice(buf);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        Ok(())
    }
}

/// The array geometry a `count`-member array of `level` over these member
/// devices genuinely presents, sized through the shared capacity oracle rather
/// than a restated literal.
pub(crate) fn array_geometry(level: RaidLevel, count: u16) -> BlockGeometry {
    BlockGeometry {
        block_size: BLOCK_SIZE,
        block_count: level
            .logical_block_count(DATA_BLOCKS, u64::from(count))
            .expect("a composable width"),
    }
}

/// A superblock claiming `slot` of a `count`-member array of `level` at
/// `generation`, declaring the array geometry such an array really presents.
pub(crate) fn superblock(
    level: RaidLevel,
    array: ArrayUuid,
    count: u16,
    slot: u16,
    generation: u64,
) -> ArraySuperblock {
    ArraySuperblock {
        array_uuid: array,
        raid_level: level,
        member_count: count,
        member_slot: slot,
        geometry: array_geometry(level, count),
        generation,
        updated_at: STAMPED_AT,
        chunk_blocks: if level.is_striped() { CHUNK } else { 0 },
    }
}

/// A member device of the stated size carrying `superblock` in its first
/// block, exactly as a discovered array member does.
pub(crate) fn stamped_device(superblock: &ArraySuperblock, device_blocks: u64) -> MemberDisk {
    let disk = MemberDisk::new(device_blocks);
    write_superblock(&mut disk.clone(), superblock)
        .expect("a fresh device accepts its own superblock");
    disk
}

/// A full-size member device carrying `superblock`.
pub(crate) fn stamped(superblock: &ArraySuperblock) -> MemberDisk {
    stamped_device(superblock, DEVICE_BLOCKS)
}

/// The reassembly view of `members`: candidate `i` describes device `i`, which
/// is the correspondence [`assemble_array`]'s supplier is keyed by.
pub(crate) fn candidates(members: &[ArraySuperblock]) -> Vec<Candidate> {
    members
        .iter()
        .enumerate()
        .map(|(tag, superblock)| Candidate {
            tag,
            superblock: *superblock,
        })
        .collect()
}

/// The authoritative shape `members` agree on.
pub(crate) fn identity_of(array: ArrayUuid, members: &[ArraySuperblock]) -> ArrayIdentity {
    ArrayIdentity::resolve(array, &candidates(members)).expect("a claimed array resolves")
}

/// Encode one block request frame.
pub(crate) fn request(op: BlkOp, lba: u64, blocks: u32) -> [u8; BLK_REQUEST_LEN] {
    let mut frame = [0u8; BLK_REQUEST_LEN];
    BlkRequest { op, lba, blocks }
        .encode(&mut frame)
        .expect("the frame is exactly wide enough");
    frame
}
