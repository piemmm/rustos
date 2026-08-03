//! The composer's **live array** half (`plans/FIX-IO.md` `IO6d`): reading a
//! member's on-disk metadata, assembling a resolved array into an owning
//! composed device, and serving that device fault-aware.
//!
//! # Why this lives behind seams
//!
//! The registration and assembly *decisions* — which members belong to which
//! array, and when one may come online — are the pure `compose::MemberRegistry`
//! next door. This module is the other pure half: given those decisions, it
//! builds and serves the array. It is generic over the member
//! [`Block`] type and takes its clock as a
//! value, so the whole of it is provable on the host over member doubles; the
//! `Run` program supplies the real block clients, the syscalls, and the audit
//! trail.
//!
//! # A member node is a pointer to look, never a datum to believe
//!
//! Nothing an offering agent says about a device is trusted. The composer
//! reads each offered device's superblock **itself** ([`read_superblock`]) and
//! decodes it fail-closed, so a hostile or failing member can, at worst, be
//! refused — never place a disk into an array it has nothing to do with.
//!
//! # Never serve data an array cannot vouch for
//!
//! Two data-integrity rules govern [`assemble_array`]:
//!
//! * An array whose surviving members cannot reconstruct every logical block
//!   is refused, never composed short — the shared
//!   [`RaidLevel::can_serve`](tairix_abi::raid::RaidLevel::can_serve) is the one
//!   definition of that question.
//! * An array brought online with a slot the composer cannot see is **fenced**:
//!   its generation is bumped and every surviving current member is re-stamped
//!   at the new generation, so the disk that is missing — which may still hold
//!   a superblock claiming it is current — can never return masquerading as up
//!   to date. A re-stamp that cannot be written fails the whole bring-up closed
//!   rather than serving an array whose metadata lies. A slot that is present
//!   but *behind* needs no fencing: its own superblock already records it, and
//!   moving the array again on its account would throw away the recorded
//!   position of the rebuild it is the target of.
//! * An array whose composed geometry is not the geometry its members' own
//!   metadata agree on is refused. The members' capacity is what the
//!   composition engine actually measures, while the identity records what the
//!   array was created as; publishing a device shorter than the one a
//!   filesystem was made on would leave every address past the end silently
//!   unreachable.
//!
//! Each member is composed through a [`PartitionBlock`] view that begins at
//! [`RESERVED_METADATA_BLOCKS`], so a member's own superblock can never be
//! read or written as array data.
//!
//! # A pass measured in days resumes where it left off
//!
//! A scrub or a rebuild of a 100 TB+ array runs for longer than the interval
//! between reboots, so the array's position is read back from the members'
//! maintenance records at assembly and restored into the composed device
//! before it serves anything. A record that is missing, foreign, corrupt, or
//! from another generation simply yields no position: the passes start over,
//! which costs time and never correctness.

use alloc::vec::Vec;

use tairix_abi::driver::block::Block;
use tairix_abi::raid::{RaidLevel, SlotDisposition};
use tairix_abi::time::Time64;
use tairix_abi::DriverError;
use tairix_partition::PartitionBlock;
use tairix_raid::{
    fill_members, ArrayIdentity, ArraySuperblock, AssembleError, AssembleMember, Candidate,
    DualParityMember, MirrorMember, OwnedRaidArray, ParityMember, StripeMember, SuperblockError,
    TripleParityMember, SCRATCH_BLOCKS as DUAL_PARITY_SCRATCH_BLOCKS,
    TRIPLE_SCRATCH_BLOCKS as TRIPLE_PARITY_SCRATCH_BLOCKS, WIRE_LEN as SUPERBLOCK_WIRE_LEN,
};
use tairix_raidmeta::{
    ArrayProgress, MaintenanceRecord, MAINTENANCE_BLOCK, RESERVED_METADATA_BLOCKS, SUPERBLOCK_BLOCK,
};

/// The largest logical block size a sane member reports, bounding the stack
/// buffer superblock I/O stages one block through. Every member the composer
/// serves comes through the blkio client, which already refuses a block size
/// outside `512..=4096`; a member reporting a larger one is refused here too
/// rather than staging it through an undersized buffer.
const MAX_BLOCK_SIZE: usize = 4096;

/// The smallest logical block size a member may have: enough to hold either
/// metadata record whole, since each occupies a block of its own. Deriving it
/// from both records rather than naming a number keeps a member that could
/// hold one but not the other from being admitted at all.
const MIN_BLOCK_SIZE: usize = if SUPERBLOCK_WIRE_LEN > MaintenanceRecord::WIRE_LEN {
    SUPERBLOCK_WIRE_LEN
} else {
    MaintenanceRecord::WIRE_LEN
};

/// Logical blocks of scratch a RAID5 distributed-parity array borrows for
/// read-modify-write and reconstruction. Its engine documents the minimum as
/// two logical blocks (an old-data plus old-parity pair, or an accumulator
/// plus a read block) and, unlike the double- and triple-parity levels, does
/// not export a named constant, so the requirement is named once here beside
/// the sizes it shares a purpose with.
const SINGLE_PARITY_SCRATCH_BLOCKS: usize = 2;

/// A reason the live-array half could not read a member, assemble an array, or
/// re-stamp a degraded start. Every variant is a fail-closed refusal: the
/// composer never serves an array it could not build cleanly.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ServiceError {
    /// A member's block size cannot hold the superblock record, or is larger
    /// than the largest a sane device reports.
    BlockSize,
    /// A member device call failed (a transport or device fault).
    Device(DriverError),
    /// A member's superblock did not decode; the device is not trusted.
    Superblock(SuperblockError),
    /// The candidate set does not resolve to a slot table this level can
    /// serve, so composing it would hand a filesystem a device that cannot
    /// read parts of itself.
    Unservable,
    /// The reassembly could not populate the member buffer (a width mismatch,
    /// or a present slot whose device the supplier could not resolve).
    Fill(AssembleError),
    /// A member device is too small to hold both the reserved metadata and any
    /// array data.
    MemberTooSmall,
    /// A composition engine refused the assembled members (a live device
    /// unwell or absent at assembly, an overflowing geometry).
    Assembly,
    /// The array the members compose to is not the array their own metadata
    /// describes: the composed logical geometry differs from the identity's.
    /// Refused rather than published at whichever size the disks happen to
    /// have.
    GeometryMismatch,
    /// A table could not grow to hold the array's members or scratch;
    /// exhaustion is a value, never a panic.
    OutOfMemory,
    /// The maintenance scheduler refused the array: its per-member records did
    /// not match the array's width. The runtime sizes that buffer from the
    /// array itself, so this cannot arise from a disk; it is refused rather
    /// than assumed away, because a scheduler that cannot see every slot would
    /// leave one unable to rejoin.
    Maintenance,
}

/// Read and decode the array superblock from block 0 of `device`, failing
/// closed on a device whose block size cannot hold the record, a device fault,
/// or any malformed on-disk byte.
///
/// The composer reads each offered device's metadata itself: a member node
/// says only "look here", so its array, slot, and generation are read back off
/// the disk rather than believed from the offering agent.
///
/// # Errors
///
/// * [`ServiceError::BlockSize`] — the device's block size cannot hold the
///   record, or exceeds the largest a sane device reports.
/// * [`ServiceError::Device`] — the geometry or read call failed.
/// * [`ServiceError::Superblock`] — the leading bytes are not a valid
///   superblock.
pub fn read_superblock<B: Block>(device: &mut B) -> Result<ArraySuperblock, ServiceError> {
    let block_size = metadata_block_size(device)?;
    let mut buf = [0u8; MAX_BLOCK_SIZE];
    device
        .read_blocks(SUPERBLOCK_BLOCK, &mut buf[..block_size])
        .map_err(ServiceError::Device)?;
    ArraySuperblock::decode(&buf[..block_size]).map_err(ServiceError::Superblock)
}

/// Read the array maintenance record a member carries, or [`None`] when it
/// carries none this array can use.
///
/// The record is a *hint* about where an interrupted verification pass or
/// rebuild had got to, never something the array's correctness rests on, so
/// every way of doubting it — a device that will not answer, a block size that
/// cannot hold the record, a blank block, a torn or corrupt one — yields
/// [`None`] rather than an error. The array then starts its passes over, which
/// is the direction that verifies more, never less. Whether the record belongs
/// to *this* array at *this* generation is decided later, by
/// [`MaintenanceRecord::progress_for`].
pub fn read_maintenance_record<B: Block>(device: &mut B) -> Option<MaintenanceRecord> {
    let block_size = metadata_block_size(device).ok()?;
    let mut buf = [0u8; MAX_BLOCK_SIZE];
    device
        .read_blocks(MAINTENANCE_BLOCK, &mut buf[..block_size])
        .ok()?;
    MaintenanceRecord::decode(&buf[..block_size]).ok()
}

/// Write `record` into a member's maintenance block, zero-padding the rest.
///
/// The block is reserved for this record alone, so the padding carries no
/// array data. It is a block of its own, separate from the superblock,
/// precisely so a torn write of a routine progress checkpoint can never damage
/// the metadata reassembly depends on.
///
/// # Errors
///
/// * [`ServiceError::BlockSize`] — the device's block size cannot hold the
///   record, or exceeds the largest a sane device reports.
/// * [`ServiceError::Device`] — the geometry or write call failed.
pub fn write_maintenance_record<B: Block>(
    device: &mut B,
    record: &MaintenanceRecord,
) -> Result<(), ServiceError> {
    let block_size = metadata_block_size(device)?;
    let mut buf = [0u8; MAX_BLOCK_SIZE];
    buf[..MaintenanceRecord::WIRE_LEN].copy_from_slice(&record.encode());
    device
        .write_blocks(MAINTENANCE_BLOCK, &buf[..block_size])
        .map_err(ServiceError::Device)
}

/// Write `superblock` into block 0 of `device`, zero-padding the rest of the
/// block.
///
/// Block 0 is reserved for the superblock alone, so padding it carries no
/// array data. Used to re-stamp a surviving current member at a bumped
/// generation when an array starts degraded.
///
/// # Errors
///
/// * [`ServiceError::BlockSize`] — the device's block size cannot hold the
///   record, or exceeds the largest a sane device reports.
/// * [`ServiceError::Device`] — the geometry or write call failed.
pub fn write_superblock<B: Block>(
    device: &mut B,
    superblock: &ArraySuperblock,
) -> Result<(), ServiceError> {
    let block_size = metadata_block_size(device)?;
    let mut buf = [0u8; MAX_BLOCK_SIZE];
    buf[..SUPERBLOCK_WIRE_LEN].copy_from_slice(&superblock.encode());
    device
        .write_blocks(SUPERBLOCK_BLOCK, &buf[..block_size])
        .map_err(ServiceError::Device)
}

/// The validated block size of `device`, refusing a size that cannot hold the
/// array's metadata records or is larger than the staging buffer.
fn metadata_block_size<B: Block>(device: &B) -> Result<usize, ServiceError> {
    let block_size = device.geometry().map_err(ServiceError::Device)?.block_size as usize;
    if !(MIN_BLOCK_SIZE..=MAX_BLOCK_SIZE).contains(&block_size) {
        return Err(ServiceError::BlockSize);
    }
    Ok(block_size)
}

/// Where an assembled array's self-maintenance picks up: what its members'
/// records say it had done, and what the next record it writes must carry.
///
/// Read from the freshest record among the array's own members at assembly.
/// An array with no usable record resumes as one whose history is unknown:
/// nothing in progress, and a verification pass due at once rather than
/// assumed unnecessary.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct MaintenanceResume {
    /// The position the array was restored to, already planted in the composed
    /// device, or [`ArrayProgress::IDLE`] when there was none to restore.
    pub progress: ArrayProgress,
    /// The sequence the next record written must carry, one past the freshest
    /// found, so a later checkpoint always outranks the records already on the
    /// members.
    pub sequence: u64,
    /// When the array's last complete verification pass finished, carried
    /// forward so a checkpoint written before the next pass completes does not
    /// erase it.
    pub last_scrub_completed: Option<Time64>,
    /// How long ago that pass finished, as the maintenance scheduler seeds its
    /// period. [`u64::MAX`] means unknown, which makes a pass due at once.
    pub since_last_scrub_ns: u64,
}

/// An assembled array ready to be published and served.
pub struct Assembled<B: Block> {
    /// The composed device, owning its members on the heap.
    pub array: OwnedRaidArray<PartitionBlock<B>>,
    /// The identity the array was assembled at — the resolved identity for a
    /// clean start, or the bumped identity a degraded start persisted, so the
    /// caller records the generation now on disk.
    pub identity: ArrayIdentity,
    /// The resolved slot table the array was composed from, so the caller
    /// records exactly which members became part of it, keyed by candidate
    /// tag. Its present slots are the members now serving; its missing slots
    /// are the array's absent or held-back members.
    pub slots: Vec<SlotDisposition>,
    /// Whether the array started short of full redundancy — a slot missing, or
    /// holding a copy the metadata proved is behind.
    pub degraded: bool,
    /// Where the array's self-maintenance picks up, read from its members'
    /// own records and already restored into `array`.
    pub resume: MaintenanceResume,
}

/// Assemble the array `identity` from its members among `candidates`, taking
/// each present slot's device from `take_raw` keyed by the candidate tag.
///
/// The slot table is resolved with [`ArrayIdentity::fill_slots`] and refused
/// unless [`RaidLevel::can_serve`](tairix_abi::raid::RaidLevel::can_serve) accepts
/// it. An array with a slot the composer cannot see is fenced: the generation
/// is bumped and each surviving current member is re-stamped at it through
/// [`write_superblock`] before the array is composed, so the missing disk
/// resolves as the stale rebuild target it is when it returns. A re-stamp write
/// failure fails the whole bring-up closed. Each present device is composed
/// through a [`PartitionBlock`] view beginning at [`RESERVED_METADATA_BLOCKS`],
/// so its own metadata can never be exposed as array data.
///
/// `take_raw(tag)` yields the raw member device for a candidate tag, moving it
/// out of the caller's keeping; it is called at most once per present slot.
/// `now` stamps each re-stamped member's superblock.
///
/// # Errors
///
/// A [`ServiceError`] for an unservable slot table, a member too small for its
/// metadata, a re-stamp or device fault, a supplier that could not resolve a
/// present slot's device, allocation failure, or a composition engine refusal.
pub fn assemble_array<B: Block>(
    identity: ArrayIdentity,
    candidates: &[Candidate],
    now: Time64,
    mut take_raw: impl FnMut(usize) -> Option<B>,
) -> Result<Assembled<B>, ServiceError> {
    let count = usize::from(identity.member_count);
    let mut slots = try_vec(count, SlotDisposition::Missing)?;
    identity
        .fill_slots(candidates, &mut slots)
        .map_err(|_| ServiceError::Unservable)?;
    if !identity.raid_level.can_serve(&slots) {
        return Err(ServiceError::Unservable);
    }

    let degraded = !slots
        .iter()
        .all(|slot| matches!(slot, SlotDisposition::Present { in_sync: true, .. }));
    // Only a slot the composer cannot see needs fencing: that disk may still
    // hold a superblock claiming it is current, so the survivors must move to a
    // generation it cannot match. A slot that is present but *behind* is
    // already fenced by its own superblock, and moving the array again on its
    // account would cost more than it buys — the array's maintenance record
    // names the generation it was written at, so a needless bump discards the
    // recorded position of the very rebuild that member is the target of, and a
    // rebuild long enough to outlive a restart would begin again every time.
    let unfenced = slots
        .iter()
        .any(|slot| matches!(slot, SlotDisposition::Missing));
    let effective = if unfenced {
        identity.bump_generation()
    } else {
        identity
    };

    // Take each present device, re-stamp the surviving current members of a
    // degraded start, and wrap every one in its metadata-offset view. Keyed by
    // candidate tag so the placement bridge below maps each slot back to its
    // device.
    let mut prepared: Vec<(usize, PartitionBlock<B>)> = Vec::new();
    let mut freshest: Option<MaintenanceRecord> = None;
    for (slot_index, slot) in slots.iter().enumerate() {
        let SlotDisposition::Present { tag, in_sync } = *slot else {
            continue;
        };
        let mut raw =
            take_raw(tag).ok_or(ServiceError::Fill(AssembleError::MissingDevice { tag }))?;
        if let Some(record) = read_maintenance_record(&mut raw) {
            if freshest.is_none_or(|held| record.is_fresher_than(&held)) {
                freshest = Some(record);
            }
        }
        if unfenced && in_sync {
            let slot = u16::try_from(slot_index).map_err(|_| ServiceError::Unservable)?;
            let restamped = effective
                .member_superblock(slot, now)
                .ok_or(ServiceError::Unservable)?;
            write_superblock(&mut raw, &restamped)?;
        }
        let view = wrap_member(raw)?;
        prepared
            .try_reserve(1)
            .map_err(|_| ServiceError::OutOfMemory)?;
        prepared.push((tag, view));
    }

    let mut array = build_owned(&effective, &slots, &mut prepared)?;
    // The composed device is measured from the members; the identity records
    // what the array was created as. A disagreement means these disks are not
    // the array this metadata describes, so it is never published.
    if array.array_geometry() != effective.geometry {
        return Err(ServiceError::GeometryMismatch);
    }
    let resume = resume_maintenance(&mut array, freshest.as_ref(), &effective, now);
    Ok(Assembled {
        array,
        identity: effective,
        slots,
        degraded,
        resume,
    })
}

/// Restore the position `record` holds into `array` and report where its
/// maintenance picks up.
///
/// A record that is not this array's is disregarded whole: a recycled or
/// hostile disk must be unable to inject a position into this array, and
/// equally unable to talk it out of verifying itself with a completion that
/// was never this array's. A cursor the composed array will not accept is
/// dropped rather than made a reason to refuse the array: the position only
/// says where to *resume*, so losing it costs a pass from the beginning and
/// never correctness. The completion stamp survives that, because when the
/// array was last verified is true whatever the cursors say.
fn resume_maintenance<B: Block>(
    array: &mut OwnedRaidArray<PartitionBlock<B>>,
    record: Option<&MaintenanceRecord>,
    identity: &ArrayIdentity,
    now: Time64,
) -> MaintenanceResume {
    let record = record.filter(|record| record.belongs_to(identity));
    let stored = record.map_or(ArrayProgress::IDLE, |record| record.progress_for(identity));
    let progress = if stored.is_active() && array.restore_progress(stored).is_err() {
        ArrayProgress::IDLE
    } else {
        stored
    };
    MaintenanceResume {
        progress,
        sequence: record.map_or(0, |record| record.sequence.saturating_add(1)),
        last_scrub_completed: record.and_then(|record| record.last_scrub_completed),
        since_last_scrub_ns: record.map_or(u64::MAX, |record| record.since_last_scrub_ns(now)),
    }
}

/// Wrap `raw` in the metadata-offset window every member is composed through:
/// a view beginning at [`RESERVED_METADATA_BLOCKS`] and spanning the rest of
/// the device, so the member's own superblock and maintenance record sit below
/// block 0 of the view and can never be served as array data.
pub(crate) fn wrap_member<B: Block>(raw: B) -> Result<PartitionBlock<B>, ServiceError> {
    let block_count = raw.geometry().map_err(ServiceError::Device)?.block_count;
    let data_blocks = block_count
        .checked_sub(RESERVED_METADATA_BLOCKS)
        .filter(|&blocks| blocks > 0)
        .ok_or(ServiceError::MemberTooSmall)?;
    PartitionBlock::new(raw, RESERVED_METADATA_BLOCKS, data_blocks).map_err(ServiceError::Device)
}

/// Build the level-specific [`OwnedRaidArray`] from the resolved `slots`,
/// drawing each present slot's device from `prepared` by candidate tag.
fn build_owned<B: Block>(
    identity: &ArrayIdentity,
    slots: &[SlotDisposition],
    prepared: &mut Vec<(usize, PartitionBlock<B>)>,
) -> Result<OwnedRaidArray<PartitionBlock<B>>, ServiceError> {
    let count = slots.len();
    let chunk_blocks = identity.chunk_blocks;
    let block_size = identity.geometry.block_size;
    match identity.raid_level {
        RaidLevel::Mirror => {
            let members: Vec<MirrorMember<PartitionBlock<B>>> =
                placed_members(count, slots, prepared)?;
            OwnedRaidArray::assemble_mirror(members).map_err(|_| ServiceError::Assembly)
        }
        RaidLevel::Stripe => {
            // A stripe carries no stale/absent vocabulary, so the shared
            // placement bridge does not build its members; `can_serve`
            // guarantees every slot present, and they are placed in slot order.
            let mut members: Vec<StripeMember<PartitionBlock<B>>> = try_empty(count)?;
            for slot in slots {
                let SlotDisposition::Present { tag, .. } = *slot else {
                    return Err(ServiceError::Unservable);
                };
                let device = take_prepared(prepared, tag)
                    .ok_or(ServiceError::Fill(AssembleError::MissingDevice { tag }))?;
                members.push(StripeMember::new(device));
            }
            OwnedRaidArray::assemble_stripe(members, chunk_blocks)
                .map_err(|_| ServiceError::Assembly)
        }
        RaidLevel::Parity => {
            let members: Vec<ParityMember<PartitionBlock<B>>> =
                placed_members(count, slots, prepared)?;
            let scratch = try_scratch(SINGLE_PARITY_SCRATCH_BLOCKS, block_size)?;
            OwnedRaidArray::assemble_parity(members, scratch, chunk_blocks)
                .map_err(|_| ServiceError::Assembly)
        }
        RaidLevel::DualParity => {
            let members: Vec<DualParityMember<PartitionBlock<B>>> =
                placed_members(count, slots, prepared)?;
            let scratch = try_scratch(DUAL_PARITY_SCRATCH_BLOCKS, block_size)?;
            OwnedRaidArray::assemble_dual_parity(members, scratch, chunk_blocks)
                .map_err(|_| ServiceError::Assembly)
        }
        RaidLevel::TripleParity => {
            let members: Vec<TripleParityMember<PartitionBlock<B>>> =
                placed_members(count, slots, prepared)?;
            let scratch = try_scratch(TRIPLE_PARITY_SCRATCH_BLOCKS, block_size)?;
            OwnedRaidArray::assemble_triple_parity(members, scratch, chunk_blocks)
                .map_err(|_| ServiceError::Assembly)
        }
        RaidLevel::Raid10 => {
            let members: Vec<MirrorMember<PartitionBlock<B>>> =
                placed_members(count, slots, prepared)?;
            OwnedRaidArray::assemble_raid10(members, chunk_blocks)
                .map_err(|_| ServiceError::Assembly)
        }
    }
}

/// Build a redundant level's member buffer through the shared [`fill_members`]
/// bridge, drawing each present slot's device from `prepared` by candidate tag
/// so a slot the metadata proved stale joins as a rebuild target, never a
/// trusted read source.
fn placed_members<B, M>(
    count: usize,
    slots: &[SlotDisposition],
    prepared: &mut Vec<(usize, PartitionBlock<B>)>,
) -> Result<Vec<M>, ServiceError>
where
    B: Block,
    M: AssembleMember<PartitionBlock<B>>,
{
    let mut members = try_empty(count)?;
    for _ in 0..count {
        members.push(M::make_absent());
    }
    fill_members(slots, &mut members, |tag| take_prepared(prepared, tag))
        .map_err(ServiceError::Fill)?;
    Ok(members)
}

/// Remove the prepared device for candidate `tag`, or [`None`] if none is
/// held for it.
fn take_prepared<B: Block>(
    prepared: &mut Vec<(usize, PartitionBlock<B>)>,
    tag: usize,
) -> Option<PartitionBlock<B>> {
    let position = prepared.iter().position(|(held, _)| *held == tag)?;
    Some(prepared.swap_remove(position).1)
}

/// A fallibly-allocated vector of `count` copies of `value`, so exhaustion is
/// a value the caller fails closed on rather than a panic.
fn try_vec<T: Clone>(count: usize, value: T) -> Result<Vec<T>, ServiceError> {
    let mut out = Vec::new();
    out.try_reserve(count)
        .map_err(|_| ServiceError::OutOfMemory)?;
    out.resize(count, value);
    Ok(out)
}

/// A fallibly-allocated empty vector with room reserved for `count` elements.
fn try_empty<T>(count: usize) -> Result<Vec<T>, ServiceError> {
    let mut out = Vec::new();
    out.try_reserve(count)
        .map_err(|_| ServiceError::OutOfMemory)?;
    Ok(out)
}

/// A fallibly-allocated, zeroed scratch buffer of `blocks` logical blocks for
/// a parity level's reconstruction and read-modify-write.
fn try_scratch(blocks: usize, block_size: u32) -> Result<Vec<u8>, ServiceError> {
    let len = blocks
        .checked_mul(block_size as usize)
        .ok_or(ServiceError::Assembly)?;
    try_vec(len, 0u8)
}

#[cfg(test)]
mod tests;
