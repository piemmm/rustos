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
//! [`Block`](tairix_abi::driver::block::Block) type and takes its clock as a
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
//!   [`RaidLevel::can_serve`](tairix_raid::RaidLevel::can_serve) is the one
//!   definition of that question.
//! * An array brought online with any slot not present-and-current is a
//!   **degraded start**: its generation is bumped and every surviving current
//!   member is re-stamped at the new generation, so a member that was absent or
//!   behind can never return masquerading as up to date. A re-stamp that
//!   cannot be written fails the whole bring-up closed rather than serving an
//!   array whose metadata lies.
//!
//! Each member is composed through a [`PartitionBlock`] view that begins at
//! [`RESERVED_METADATA_BLOCKS`], so a member's own superblock can never be
//! read or written as array data.

use alloc::vec::Vec;

use tairix_abi::blkio::{serve_request_recovering, BlkHealth, BlkHealthState, BLK_COMPLETION_LEN};
use tairix_abi::driver::block::Block;
use tairix_abi::time::Time64;
use tairix_abi::DriverError;
use tairix_partition::PartitionBlock;
use tairix_raid::{
    fill_members, ArrayIdentity, ArraySuperblock, AssembleError, AssembleMember, Candidate,
    DualParityMember, MirrorMember, OwnedRaidArray, ParityMember, RaidLevel, SlotDisposition,
    StripeMember, SuperblockError, TripleParityMember, SCRATCH_BLOCKS as DUAL_PARITY_SCRATCH_BLOCKS,
    TRIPLE_SCRATCH_BLOCKS as TRIPLE_PARITY_SCRATCH_BLOCKS, WIRE_LEN as SUPERBLOCK_WIRE_LEN,
};
use tairix_raidmeta::{RESERVED_METADATA_BLOCKS, SUPERBLOCK_BLOCK};

/// The largest logical block size a sane member reports, bounding the stack
/// buffer superblock I/O stages one block through. Every member the composer
/// serves comes through the blkio client, which already refuses a block size
/// outside `512..=4096`; a member reporting a larger one is refused here too
/// rather than staging it through an undersized buffer.
const MAX_BLOCK_SIZE: usize = 4096;

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
    /// A table could not grow to hold the array's members or scratch;
    /// exhaustion is a value, never a panic.
    OutOfMemory,
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
    let block_size = superblock_block_size(device)?;
    let mut buf = [0u8; MAX_BLOCK_SIZE];
    device
        .read_blocks(SUPERBLOCK_BLOCK, &mut buf[..block_size])
        .map_err(ServiceError::Device)?;
    ArraySuperblock::decode(&buf[..block_size]).map_err(ServiceError::Superblock)
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
    let block_size = superblock_block_size(device)?;
    let mut buf = [0u8; MAX_BLOCK_SIZE];
    buf[..SUPERBLOCK_WIRE_LEN].copy_from_slice(&superblock.encode());
    device
        .write_blocks(SUPERBLOCK_BLOCK, &buf[..block_size])
        .map_err(ServiceError::Device)
}

/// The validated block size of `device`, refusing a size that cannot hold the
/// superblock or is larger than the staging buffer.
fn superblock_block_size<B: Block>(device: &B) -> Result<usize, ServiceError> {
    let block_size = device.geometry().map_err(ServiceError::Device)?.block_size as usize;
    if block_size < SUPERBLOCK_WIRE_LEN || block_size > MAX_BLOCK_SIZE {
        return Err(ServiceError::BlockSize);
    }
    Ok(block_size)
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
    /// Whether the array started degraded and its surviving members were
    /// re-stamped at the bumped generation.
    pub degraded: bool,
}

/// Assemble the array `identity` from its members among `candidates`, taking
/// each present slot's device from `take_raw` keyed by the candidate tag.
///
/// The slot table is resolved with [`ArrayIdentity::fill_slots`] and refused
/// unless [`RaidLevel::can_serve`](tairix_raid::RaidLevel::can_serve) accepts
/// it. An array with any slot missing or behind is a degraded start: the
/// generation is bumped and each surviving current member is re-stamped at it
/// through [`write_superblock`] before the array is composed, so a member that
/// returns later resolves as the stale rebuild target it is. A re-stamp write
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
    let effective = if degraded {
        identity.bump_generation()
    } else {
        identity
    };

    // Take each present device, re-stamp the surviving current members of a
    // degraded start, and wrap every one in its metadata-offset view. Keyed by
    // candidate tag so the placement bridge below maps each slot back to its
    // device.
    let mut prepared: Vec<(usize, PartitionBlock<B>)> = Vec::new();
    for (slot_index, slot) in slots.iter().enumerate() {
        let SlotDisposition::Present { tag, in_sync } = *slot else {
            continue;
        };
        let mut raw = take_raw(tag).ok_or(ServiceError::Fill(AssembleError::MissingDevice { tag }))?;
        if degraded && in_sync {
            let slot = u16::try_from(slot_index).map_err(|_| ServiceError::Unservable)?;
            let restamped = effective
                .member_superblock(slot, now)
                .ok_or(ServiceError::Unservable)?;
            write_superblock(&mut raw, &restamped)?;
        }
        let view = wrap_member(raw)?;
        prepared.try_reserve(1).map_err(|_| ServiceError::OutOfMemory)?;
        prepared.push((tag, view));
    }

    let array = build_owned(&effective, &slots, &mut prepared)?;
    Ok(Assembled {
        array,
        identity: effective,
        slots,
        degraded,
    })
}

/// Wrap `raw` in the metadata-offset window every member is composed through:
/// a view beginning at [`RESERVED_METADATA_BLOCKS`] and spanning the rest of
/// the device, so the member's own superblock and maintenance record sit below
/// block 0 of the view and can never be served as array data.
fn wrap_member<B: Block>(raw: B) -> Result<PartitionBlock<B>, ServiceError> {
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
            OwnedRaidArray::assemble_stripe(members, chunk_blocks).map_err(|_| ServiceError::Assembly)
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
            OwnedRaidArray::assemble_raid10(members, chunk_blocks).map_err(|_| ServiceError::Assembly)
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
    out.try_reserve(count).map_err(|_| ServiceError::OutOfMemory)?;
    out.resize(count, value);
    Ok(out)
}

/// A fallibly-allocated empty vector with room reserved for `count` elements.
fn try_empty<T>(count: usize) -> Result<Vec<T>, ServiceError> {
    let mut out = Vec::new();
    out.try_reserve(count).map_err(|_| ServiceError::OutOfMemory)?;
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

/// One live array the composer serves: its identity, its owning composed
/// device, its fault-recovery health, and the ids of the block-service
/// endpoint, shared data window, and hardware-tree node it is published
/// through.
///
/// An array answers block requests through the *same* shared
/// [`serve_request_recovering`] engine a leaf device does, so it is fault-aware
/// exactly as a disk is and no second serve path exists.
pub struct ArrayRuntime<B: Block> {
    identity: ArrayIdentity,
    array: OwnedRaidArray<B>,
    health: BlkHealth,
    endpoint: u64,
    window_id: u64,
    node_id: u32,
}

impl<B: Block> ArrayRuntime<B> {
    /// Wrap an assembled array as a live service on `endpoint` (the
    /// composer-created block-service call endpoint), `window_id` (its shared
    /// data window), and `node_id` (its published hardware-tree node).
    ///
    /// The array is served with the most patient of its live members' device
    /// classes — it can only answer as fast as the member it waits on — read
    /// from the composed device itself, never an assumed envelope.
    #[must_use]
    pub fn new(
        identity: ArrayIdentity,
        array: OwnedRaidArray<B>,
        endpoint: u64,
        window_id: u64,
        node_id: u32,
    ) -> Self {
        let health = BlkHealth::new(array.device_class());
        Self {
            identity,
            array,
            health,
            endpoint,
            window_id,
            node_id,
        }
    }

    /// Serve one block request into `reply`, staging its data through
    /// `window`, and return the framed reply length.
    ///
    /// The request funnels through the shared fault-aware engine with the
    /// array's own [`BlkHealth`]: a member blip inside the recovery grace
    /// window is answered reissuably and a valid answer recovers the array,
    /// while a malformed or out-of-range request is refused health-neutrally.
    /// The array is served read/write.
    #[must_use]
    pub fn serve(
        &mut self,
        request: &[u8],
        window: &mut [u8],
        reply: &mut [u8; BLK_COMPLETION_LEN],
        now_ns: u64,
    ) -> usize {
        serve_request_recovering(
            &mut self.array,
            false,
            request,
            window,
            reply,
            &mut self.health,
            now_ns,
        )
    }

    /// Advance the recovery grace window on a pure time tick, so an array left
    /// recovering with no further request still fails closed on time off a
    /// one-shot timer rather than a busy-poll.
    #[must_use]
    pub fn poll(&mut self, now_ns: u64) -> BlkHealthState {
        self.health.poll(now_ns)
    }

    /// The array's fault-recovery health, for folding the serve loop's
    /// one-shot recovery timeout across every live array.
    #[must_use]
    pub const fn health(&self) -> &BlkHealth {
        &self.health
    }

    /// The composer-created block-service endpoint this array is served on.
    #[must_use]
    pub const fn endpoint(&self) -> u64 {
        self.endpoint
    }

    /// The shared data window forwarded to the array's published node.
    #[must_use]
    pub const fn window_id(&self) -> u64 {
        self.window_id
    }

    /// The array's published hardware-tree node id.
    #[must_use]
    pub const fn node_id(&self) -> u32 {
        self.node_id
    }

    /// The identity the array is serving at (its bumped generation for a
    /// degraded start).
    #[must_use]
    pub const fn identity(&self) -> &ArrayIdentity {
        &self.identity
    }

    /// Place a returning or late member device into a currently-absent slot of
    /// the live array, beginning its rebuild from the survivors.
    ///
    /// The device is wrapped in its metadata-offset view first, so its own
    /// superblock is never touched as array data, then installed with the
    /// composed device's own spare-insertion path.
    ///
    /// # Errors
    ///
    /// * [`ServiceError::MemberTooSmall`] / [`ServiceError::Device`] — the
    ///   device could not be wrapped.
    /// * [`ServiceError::Assembly`] — the array refused the placement (the
    ///   slot is out of range or already occupied).
    pub fn place_member(&mut self, slot: u16, raw: B) -> Result<(), ServiceError> {
        let view = wrap_member(raw)?;
        self.array
            .add_member(usize::from(slot), view)
            .map_err(|_| ServiceError::Assembly)
    }
}

#[cfg(test)]
mod tests;
