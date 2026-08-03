//! The composer's **administration and status** half (`plans/FIX-IO.md`
//! `IO6f`): serving the reserved control endpoint an administrator drives to
//! list, create, grow, shrink, and stop arrays.
//!
//! # A node id names a device, it never grants authority
//!
//! Every operation is decided against two things the caller cannot forge: the
//! caller's kernel-attested capability set (checked *before* any state is read
//! or written), and the devices' own on-disk metadata. A node id in a request
//! only says *which* device the administrator means — a device the composer
//! does not already hold is simply not found, and a device a create names is
//! re-read from the disk itself and refused unless it is genuinely blank.
//!
//! # Pure, so it is proven on the host
//!
//! This module is generic over the member [`Block`] type and takes its live
//! arrays through the [`LiveArrays`] seam and its device access, randomness, and
//! node removal as closures, so the whole of it — the validation order, the
//! fail-closed refusals, the superblock writes, and the create rollback — is
//! provable over the same member/block doubles the rest of the driver uses. The
//! `Run` program supplies the real capability set (from `call_peer_origin`), the
//! block clients, the kernel CSPRNG, and the audit trail; nothing here reaches a
//! syscall.

use alloc::vec::Vec;

use tairix_abi::driver::block::Block;
use tairix_abi::raid::RaidLevel;
use tairix_abi::raid_admin::{
    encode_create_reply, ArrayUuidBytes, MemberNodeList, RaidArrayRecord, RaidControlOp,
    RaidMemberDisposition, RaidMemberRecord, RAID_ARRAY_FLAG_RESYNCING, RAID_ARRAY_FLAG_SCRUBBING,
    RAID_LIST_LIMIT_MAX, RAID_SLOT_NONE,
};
use tairix_abi::reply::{encode_page_reply, encode_status_reply};
use tairix_abi::time::Time64;
use tairix_abi::{CapabilityQuery, Errno};
use tairix_raid::{ArrayIdentity, ArraySuperblock};
use tairix_raidmeta::RESERVED_METADATA_BLOCKS;

use crate::compose::MemberRegistry;
use crate::runtime::ArrayRuntime;
use crate::service::write_superblock;

/// The generation a freshly created array's members are stamped at.
const CREATE_GENERATION: u64 = 1;

/// The live arrays the composer serves, as the administration layer needs to
/// reach them: enough to find one by identity, drive it, and read its state,
/// without knowing how the serve loop stores it alongside its data window.
///
/// The `Run` program implements this over its window-carrying live arrays;
/// the host tests implement it over a plain vector of runtimes. Neither needs
/// to teach the administration logic anything about the other's storage.
pub trait LiveArrays {
    /// The raw member block device the arrays are composed over.
    type Device: Block;

    /// How many live arrays there are.
    fn count(&self) -> usize;

    /// The array at `index`, or [`None`] past the end.
    fn runtime_mut(&mut self, index: usize) -> Option<&mut ArrayRuntime<Self::Device>>;

    /// The index of the live array with identity `array`, or [`None`].
    fn position(&self, array: &ArrayUuidBytes) -> Option<usize>;
}

/// What one administrative request is recorded as in the audit trail.
///
/// Everything here is taken from the *decoded request*, so a refusal names the
/// operation it refused as precisely as an allowance does. An array identity is
/// on-disk metadata every reader of the array can see and a node id is a
/// kernel-assigned name, so neither is a secret.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ControlAudit {
    /// The operation's stable name, or `"unknown"` for a frame that did not
    /// decode into one.
    pub op: &'static str,
    /// Whether the operation changes state. A read is not audited; a mutation
    /// always is, allowed or refused.
    pub mutation: bool,
    /// Leading 64 bits of the array identity the request named, or zero when it
    /// named no array (a create, which mints one, and the paged reads).
    pub array_tag: u64,
    /// The device node the request named — the one device of an add or a
    /// remove, the first member of a create — or zero when it named none.
    pub node: u32,
}

impl ControlAudit {
    /// The record an effect carries before the served operation names it —
    /// which is also what a frame that did not decode is recorded as, there
    /// being no operation to name. Audited as a mutation, because a frame
    /// nobody could decode may well have been one.
    const fn unrecorded() -> Self {
        Self {
            op: "unknown",
            mutation: true,
            array_tag: 0,
            node: 0,
        }
    }

    /// What the decoded `op` is recorded as.
    fn of(op: &RaidControlOp) -> Self {
        let (array_tag, node) = match op {
            RaidControlOp::ListArrays { .. } | RaidControlOp::ListMembers { .. } => (0, 0),
            RaidControlOp::Create { members, .. } => {
                (0, members.as_slice().first().copied().unwrap_or(0))
            }
            RaidControlOp::Stop { array } => (array_tag(array), 0),
            RaidControlOp::Add { array, node } | RaidControlOp::Remove { array, node } => {
                (array_tag(array), *node)
            }
        };
        Self {
            op: op.name(),
            mutation: op.is_mutation(),
            array_tag,
            node,
        }
    }
}

/// The leading 64 bits of an array identity, as the audit trail names it.
fn array_tag(array: &ArrayUuidBytes) -> u64 {
    let mut lead = [0u8; 8];
    lead.copy_from_slice(&array[..8]);
    u64::from_le_bytes(lead)
}

/// What the caller must still do after an operation, beyond sending the reply.
///
/// The administration logic decides *what* must happen but does not itself
/// answer outstanding membership calls, unmap windows, or tear down endpoints:
/// those are the `Run` program's transport-owning concern, so they are returned
/// as instructions the caller carries out and the tests assert on.
pub struct ControlEffects {
    /// Bytes written into the caller's reply buffer.
    pub reply_len: usize,
    /// Registry member indices whose membership the caller must release (in
    /// descending order, so removing them one by one never shifts a later
    /// index): the removed member of a `Remove`, or every member of a stopped
    /// array. Releasing answers each one's outstanding offer so its agent
    /// re-offers the device.
    pub released: Vec<usize>,
    /// The live-array index the caller must tear down (its node already
    /// retired): a successful `Stop`.
    pub stopped: Option<usize>,
    /// The decision, for the audit trail: `Ok` for an allowed operation,
    /// `Err` for a refusal.
    pub outcome: Result<(), Errno>,
    /// What the trail records this request as.
    pub audit: ControlAudit,
}

impl ControlEffects {
    /// An effect that only sends a reply of `reply_len` bytes.
    fn reply(reply_len: usize, outcome: Result<(), Errno>) -> Self {
        Self {
            reply_len,
            released: Vec::new(),
            stopped: None,
            outcome,
            audit: ControlAudit::unrecorded(),
        }
    }

    /// The same effects, recorded in the trail as `audit`.
    fn recorded(mut self, audit: ControlAudit) -> Self {
        self.audit = audit;
        self
    }
}

/// Serve one control frame: decode it, check the caller's authority *before*
/// touching any state, and carry out the operation, writing the reply into
/// `out` and returning the side effects the caller must complete.
///
/// `caps` answers what the caller's kernel-attested origin holds;
/// `connect(index)` yields a fresh block client for the registry member at
/// `index`;
/// `fill_random` fills the array identity a create mints from the kernel
/// CSPRNG; `remove_node(node_id)` retires a stopped array's published node
/// (the orderly, refuse-if-busy removal). `now_wall` stamps the metadata a
/// create or add writes; `now_ns` is the monotonic reading a newly affiliated
/// array's settle window is measured from.
#[allow(clippy::too_many_arguments)]
pub fn handle_control<A: LiveArrays>(
    registry: &mut MemberRegistry,
    arrays: &mut A,
    caps: &dyn CapabilityQuery,
    frame: &[u8],
    now_wall: Time64,
    now_ns: u64,
    connect: impl FnMut(usize) -> Option<A::Device>,
    fill_random: impl FnMut(&mut [u8; 16]) -> bool,
    remove_node: impl FnMut(u32) -> Result<(), Errno>,
    out: &mut [u8],
) -> ControlEffects {
    let op = match RaidControlOp::decode(frame) {
        Ok(op) => op,
        // A malformed, unknown, or oversize frame is refused without touching
        // any state; the caller cannot know the operation, so a status refusal
        // is all that can be said.
        Err(errno) => return ControlEffects::reply(write_status(out, Err(errno)), Err(errno)),
    };
    let audit = ControlAudit::of(&op);
    // Authority is checked against the kernel-attested caller before a single
    // byte of state is read or written; a caller without it is refused closed.
    if !caps.holds(op.required_capability()) {
        return refuse(&op, Errno::PermissionDenied, out).recorded(audit);
    }
    let effects = match op {
        RaidControlOp::ListArrays { offset, limit } => list_arrays(arrays, offset, limit, out),
        RaidControlOp::ListMembers { offset, limit } => {
            list_members(registry, arrays, connect, offset, limit, out)
        }
        RaidControlOp::Create {
            level,
            chunk_blocks,
            members,
        } => create(
            registry,
            level,
            chunk_blocks,
            &members,
            now_wall,
            now_ns,
            connect,
            fill_random,
            out,
        ),
        RaidControlOp::Add { array, node } => add(
            registry, arrays, &array, node, now_wall, now_ns, connect, out,
        ),
        RaidControlOp::Remove { array, node } => {
            remove(registry, arrays, &array, node, now_wall, out)
        }
        RaidControlOp::Stop { array } => stop(registry, arrays, &array, remove_node, out),
    };
    effects.recorded(audit)
}

/// Write the refusal shape the operation `op` expects into `out`: a create
/// reply for a create, the shared status frame for anything else.
fn refuse(op: &RaidControlOp, errno: Errno, out: &mut [u8]) -> ControlEffects {
    let reply_len = match op {
        RaidControlOp::Create { .. } => write_create(out, Err(errno)),
        _ => write_status(out, Err(errno)),
    };
    ControlEffects::reply(reply_len, Err(errno))
}

/// Report the live arrays, paged.
fn list_arrays<A: LiveArrays>(
    arrays: &mut A,
    offset: u32,
    limit: u16,
    out: &mut [u8],
) -> ControlEffects {
    let limit = limit.min(RAID_LIST_LIMIT_MAX);
    let total = arrays.count();
    let start = usize::try_from(offset).unwrap_or(usize::MAX).min(total);
    let mut records: Vec<[u8; RaidArrayRecord::WIRE_LEN]> = Vec::new();
    let mut index = start;
    while index < total && records.len() < usize::from(limit) {
        if let Some(runtime) = arrays.runtime_mut(index) {
            records.push(array_record(runtime).to_le_bytes());
        }
        index += 1;
    }
    let reply_len = encode_page_reply(&records, limit, out).unwrap_or(0);
    ControlEffects::reply(reply_len, Ok(()))
}

/// Build one array record from a live array's own state.
fn array_record<D: Block>(runtime: &mut ArrayRuntime<D>) -> RaidArrayRecord {
    let identity = *runtime.identity();
    let block_count = identity.geometry.block_count;
    let health = runtime.array_health();
    let active = runtime.active_members();
    let progress = runtime.progress();
    let mut flags = 0u8;
    if progress.scrub_cursor.is_some() {
        flags |= RAID_ARRAY_FLAG_SCRUBBING;
    }
    if progress.resync_cursor.is_some() {
        flags |= RAID_ARRAY_FLAG_RESYNCING;
    }
    RaidArrayRecord::new(
        identity.array_uuid,
        identity.raid_level,
        health,
        flags,
        identity.member_count,
        active,
        identity.geometry.block_size,
        identity.chunk_blocks,
        block_count,
        runtime.endpoint(),
        runtime.node_id(),
        progress.scrub_cursor.unwrap_or(block_count),
        progress.resync_cursor.unwrap_or(block_count),
        identity.generation,
    )
}

/// Report every device the composer holds — array members and unaffiliated
/// candidates — paged.
fn list_members<A: LiveArrays>(
    registry: &mut MemberRegistry,
    arrays: &mut A,
    mut connect: impl FnMut(usize) -> Option<A::Device>,
    offset: u32,
    limit: u16,
    out: &mut [u8],
) -> ControlEffects {
    let limit = limit.min(RAID_LIST_LIMIT_MAX);
    let total = registry.members().len();
    let start = usize::try_from(offset).unwrap_or(usize::MAX).min(total);
    let mut records: Vec<[u8; RaidMemberRecord::WIRE_LEN]> = Vec::new();
    let mut index = start;
    while index < total && records.len() < usize::from(limit) {
        records.push(member_record(registry, arrays, &mut connect, index).to_le_bytes());
        index += 1;
    }
    let reply_len = encode_page_reply(&records, limit, out).unwrap_or(0);
    ControlEffects::reply(reply_len, Ok(()))
}

/// Build one member record from the registry and, for a composed member, the
/// live array it belongs to.
fn member_record<A: LiveArrays>(
    registry: &MemberRegistry,
    arrays: &mut A,
    connect: &mut impl FnMut(usize) -> Option<A::Device>,
    index: usize,
) -> RaidMemberRecord {
    let offer = registry.members()[index].offer();
    // The device's own geometry, read from the device rather than assumed; a
    // device that will not answer reports zero rather than a guess.
    let (block_count, block_size) = connect(index)
        .and_then(|device| device.geometry().ok())
        .map_or((0, 0), |geo| (geo.block_count, geo.block_size));
    let (array, disposition, slot, generation) = match registry.member_superblock(index) {
        // A blank candidate belongs to no array.
        None => (
            [0u8; 16],
            RaidMemberDisposition::Candidate,
            RAID_SLOT_NONE,
            0,
        ),
        Some(superblock) => member_disposition(arrays, &superblock),
    };
    RaidMemberRecord::new(
        array,
        disposition,
        slot,
        offer.node,
        offer.endpoint,
        block_count,
        block_size,
        generation,
    )
}

/// Resolve an affiliated member's array, disposition, live slot, and on-disk
/// generation.
fn member_disposition<A: LiveArrays>(
    arrays: &mut A,
    superblock: &ArraySuperblock,
) -> (ArrayUuidBytes, RaidMemberDisposition, u16, u64) {
    let array = superblock.array_uuid;
    let generation = superblock.generation;
    let held = (
        array,
        RaidMemberDisposition::Held,
        RAID_SLOT_NONE,
        generation,
    );
    let Some(position) = arrays.position(&array) else {
        // The metadata names an array that is not assembled.
        return held;
    };
    let Some(runtime) = arrays.runtime_mut(position) else {
        return held;
    };
    match runtime
        .member_state(superblock.member_slot)
        .and_then(RaidMemberDisposition::for_member_state)
    {
        Some(disposition) => (array, disposition, superblock.member_slot, generation),
        None => held,
    }
}

/// Create an array over the named blank candidates, writing each member's
/// superblock at generation one and returning the minted identity.
#[allow(clippy::too_many_arguments)]
fn create<D: Block>(
    registry: &mut MemberRegistry,
    level: RaidLevel,
    chunk_blocks: u32,
    members: &MemberNodeList,
    now_wall: Time64,
    now_ns: u64,
    mut connect: impl FnMut(usize) -> Option<D>,
    mut fill_random: impl FnMut(&mut [u8; 16]) -> bool,
    out: &mut [u8],
) -> ControlEffects {
    match create_inner(
        registry,
        level,
        chunk_blocks,
        members,
        now_wall,
        now_ns,
        &mut connect,
        &mut fill_random,
    ) {
        Ok(uuid) => ControlEffects::reply(write_create(out, Ok(uuid)), Ok(())),
        Err(errno) => ControlEffects::reply(write_create(out, Err(errno)), Err(errno)),
    }
}

/// The create decision, split out so every early refusal writes the create
/// reply once at the call site.
#[allow(clippy::too_many_arguments)]
fn create_inner<D: Block>(
    registry: &mut MemberRegistry,
    level: RaidLevel,
    chunk_blocks: u32,
    members: &MemberNodeList,
    now_wall: Time64,
    now_ns: u64,
    connect: &mut impl FnMut(usize) -> Option<D>,
    fill_random: &mut impl FnMut(&mut [u8; 16]) -> bool,
) -> Result<ArrayUuidBytes, Errno> {
    let nodes = members.as_slice();
    // The width must lie within the level's own structural floor and ceiling,
    // and a stripe unit is required exactly when the level stripes.
    let count = nodes.len();
    if count < usize::from(level.min_members()) || count > usize::from(level.max_members()) {
        return Err(Errno::OutOfRange);
    }
    if (chunk_blocks != 0) != level.is_striped() {
        return Err(Errno::OutOfRange);
    }

    // Every named node must be a held unaffiliated candidate; anything else is
    // refused with nothing written.
    let mut indices: Vec<usize> = Vec::new();
    for &node in nodes {
        let index = registry.index_of_node(node).ok_or(Errno::NotFound)?;
        if !registry.is_candidate(index) {
            return Err(Errno::Busy);
        }
        indices.push(index);
    }

    // Re-read each device: the node id says only "look here", so a device that
    // is no longer blank — a filesystem, another array's metadata, or a
    // partition scheme — is refused rather than overwritten. The geometries are
    // gathered in the same pass and must all agree and leave room past the
    // reserved metadata.
    let mut devices: Vec<D> = Vec::new();
    let mut member_blocks: Option<u64> = None;
    let mut block_size = 0u32;
    for &index in &indices {
        let mut device = connect(index).ok_or(Errno::DeviceFault)?;
        if !is_blank(&mut device) {
            return Err(Errno::Busy);
        }
        let geometry = device.geometry().map_err(|_| Errno::DeviceFault)?;
        match member_blocks {
            None => {
                member_blocks = Some(geometry.block_count);
                block_size = geometry.block_size;
            }
            Some(blocks) => {
                if geometry.block_count != blocks || geometry.block_size != block_size {
                    return Err(Errno::OutOfRange);
                }
            }
        }
        devices.push(device);
    }
    let member_blocks = member_blocks.ok_or(Errno::OutOfRange)?;
    let data_blocks = member_blocks
        .checked_sub(RESERVED_METADATA_BLOCKS)
        .filter(|&blocks| blocks > 0)
        .ok_or(Errno::OutOfRange)?;
    let block_count = level
        .logical_block_count(data_blocks, count as u64)
        .ok_or(Errno::OutOfRange)?;

    let array_uuid = mint_identity(fill_random)?;
    let identity = ArrayIdentity {
        array_uuid,
        raid_level: level,
        member_count: u16::try_from(count).map_err(|_| Errno::OutOfRange)?,
        geometry: tairix_abi::driver::block::BlockGeometry {
            block_size,
            block_count,
        },
        generation: CREATE_GENERATION,
        chunk_blocks,
    };

    // Write each member's superblock. If any write fails, the already-written
    // superblocks are zeroed again so no half-written set can assemble into an
    // array claiming to be whole.
    for (slot, device) in devices.iter_mut().enumerate() {
        let slot = u16::try_from(slot).map_err(|_| Errno::OutOfRange)?;
        let record = identity
            .member_superblock(slot, now_wall)
            .ok_or(Errno::OutOfRange)?;
        if write_superblock(device, &record).is_err() {
            rollback_superblocks(&mut devices, usize::from(slot), block_size);
            return Err(Errno::DeviceFault);
        }
    }

    // The disks now carry the array; record each member's affiliation so the
    // composer's own next turn assembles it.
    for (slot, &index) in indices.iter().enumerate() {
        let slot = u16::try_from(slot).map_err(|_| Errno::OutOfRange)?;
        if let Some(record) = identity.member_superblock(slot, now_wall) {
            registry.affiliate(index, record, now_ns);
        }
    }
    Ok(array_uuid)
}

/// Zero the superblock block of the first `written` devices, undoing a create
/// whose later write failed.
fn rollback_superblocks<D: Block>(devices: &mut [D], written: usize, block_size: u32) {
    let zero = [0u8; 4096];
    let end = (block_size as usize).min(zero.len());
    for device in devices.iter_mut().take(written) {
        let _ = device.write_blocks(0, &zero[..end]);
    }
}

/// Draw a non-zero 128-bit array identity from the kernel CSPRNG.
///
/// A zero identity names no array, and a source that cannot supply randomness
/// fails the create closed rather than minting a predictable identity two
/// arrays could share.
fn mint_identity(
    fill_random: &mut impl FnMut(&mut [u8; 16]) -> bool,
) -> Result<ArrayUuidBytes, Errno> {
    let mut identity = [0u8; 16];
    if !fill_random(&mut identity) || identity.iter().all(|&byte| byte == 0) {
        return Err(Errno::EntropyNotReady);
    }
    Ok(identity)
}

/// Whether `device` currently carries no filesystem, no array metadata, and no
/// partition scheme — the create precondition, re-verified off the disk itself.
fn is_blank<D: Block>(device: &mut D) -> bool {
    let Ok(geometry) = device.geometry() else {
        return false;
    };
    let block_size = geometry.block_size as usize;
    if block_size == 0 || block_size > 4096 {
        return false;
    }
    let need = tairix_fsprobe::PROBE_HEAD_LEN
        .div_ceil(block_size)
        .saturating_mul(block_size)
        .min(4096)
        .max(block_size);
    let mut head = [0u8; 4096];
    if device.read_blocks(0, &mut head[..need]).is_err() {
        return false;
    }
    if tairix_fsprobe::probe(&head[..need]).is_some() {
        return false;
    }
    if tairix_fsprobe::probe_raid_member(&head[..need]).is_some() {
        return false;
    }
    // A parseable partition table means the disk is not blank; only the
    // no-scheme outcome leaves it eligible to be created over.
    tairix_partition::parse_partition_table(device).is_err()
}

/// Admit a blank candidate into an absent slot of a live array and begin
/// rebuilding it.
#[allow(clippy::too_many_arguments)]
fn add<A: LiveArrays>(
    registry: &mut MemberRegistry,
    arrays: &mut A,
    array: &ArrayUuidBytes,
    node: u32,
    now_wall: Time64,
    now_ns: u64,
    mut connect: impl FnMut(usize) -> Option<A::Device>,
    out: &mut [u8],
) -> ControlEffects {
    let outcome = add_inner(
        registry,
        arrays,
        array,
        node,
        now_wall,
        now_ns,
        &mut connect,
    );
    ControlEffects::reply(write_status(out, outcome), outcome)
}

/// The add decision.
#[allow(clippy::too_many_arguments)]
fn add_inner<A: LiveArrays>(
    registry: &mut MemberRegistry,
    arrays: &mut A,
    array: &ArrayUuidBytes,
    node: u32,
    now_wall: Time64,
    now_ns: u64,
    connect: &mut impl FnMut(usize) -> Option<A::Device>,
) -> Result<(), Errno> {
    let index = registry.index_of_node(node).ok_or(Errno::NotFound)?;
    if !registry.is_candidate(index) {
        return Err(Errno::Busy);
    }
    let position = arrays.position(array).ok_or(Errno::NotFound)?;
    let slot = {
        let runtime = arrays.runtime_mut(position).ok_or(Errno::NotFound)?;
        absent_slot(runtime).ok_or(Errno::Busy)?
    };
    let device = connect(index).ok_or(Errno::DeviceFault)?;
    let runtime = arrays.runtime_mut(position).ok_or(Errno::NotFound)?;
    runtime
        .admit_member(slot, device, now_wall)
        .map_err(|_| Errno::DeviceFault)?;
    // The disk now serves the array; record its affiliation and mark it placed
    // so it is never offered for placement again. Its on-disk superblock is a
    // generation behind (a rebuild target); the registry only needs it to name
    // the array and slot for a composed member.
    if let Some(record) = runtime.identity().member_superblock(slot, now_wall) {
        registry.affiliate(index, record, now_ns);
    }
    registry.note_joined(index);
    Ok(())
}

/// The first absent slot of a live array, or [`None`] when every slot is
/// occupied.
fn absent_slot<D: Block>(runtime: &mut ArrayRuntime<D>) -> Option<u16> {
    let count = runtime.identity().member_count;
    (0..count)
        .find(|&slot| runtime.member_state(slot) == Some(tairix_abi::raid::MemberState::Absent))
}

/// Retire a faulted member from a live array, vacating its slot and fencing
/// the disk out.
fn remove<A: LiveArrays>(
    registry: &mut MemberRegistry,
    arrays: &mut A,
    array: &ArrayUuidBytes,
    node: u32,
    now_wall: Time64,
    out: &mut [u8],
) -> ControlEffects {
    match remove_inner(registry, arrays, array, node, now_wall) {
        Ok(index) => ControlEffects {
            reply_len: write_status(out, Ok(())),
            released: alloc::vec![index],
            stopped: None,
            outcome: Ok(()),
            audit: ControlAudit::unrecorded(),
        },
        Err(errno) => ControlEffects::reply(write_status(out, Err(errno)), Err(errno)),
    }
}

/// The remove decision, returning the registry index the caller must release.
fn remove_inner<A: LiveArrays>(
    registry: &mut MemberRegistry,
    arrays: &mut A,
    array: &ArrayUuidBytes,
    node: u32,
    now_wall: Time64,
) -> Result<usize, Errno> {
    let index = registry.index_of_node(node).ok_or(Errno::NotFound)?;
    let superblock = registry.member_superblock(index).ok_or(Errno::NotFound)?;
    if &superblock.array_uuid != array {
        return Err(Errno::NotFound);
    }
    let position = arrays.position(array).ok_or(Errno::NotFound)?;
    let runtime = arrays.runtime_mut(position).ok_or(Errno::NotFound)?;
    // Only a faulted member may be retired; a live or rebuilding one is refused
    // so a working copy is never dropped.
    if runtime.member_state(superblock.member_slot) != Some(tairix_abi::raid::MemberState::Faulted)
    {
        return Err(Errno::Busy);
    }
    runtime
        .retire_member(superblock.member_slot, now_wall)
        .map_err(|_| Errno::DeviceFault)?;
    Ok(index)
}

/// Stop a live array: retire its published node, then release every member.
fn stop<A: LiveArrays>(
    registry: &mut MemberRegistry,
    arrays: &mut A,
    array: &ArrayUuidBytes,
    mut remove_node: impl FnMut(u32) -> Result<(), Errno>,
    out: &mut [u8],
) -> ControlEffects {
    let Some(position) = arrays.position(array) else {
        return ControlEffects::reply(
            write_status(out, Err(Errno::NotFound)),
            Err(Errno::NotFound),
        );
    };
    let Some(runtime) = arrays.runtime_mut(position) else {
        return ControlEffects::reply(
            write_status(out, Err(Errno::NotFound)),
            Err(Errno::NotFound),
        );
    };
    let node_id = runtime.node_id();
    // The node removal is the orderly, refuse-if-busy kind: a Busy refusal
    // (a volume still attached) is surfaced unchanged and nothing is released.
    if let Err(errno) = remove_node(node_id) {
        return ControlEffects::reply(write_status(out, Err(errno)), Err(errno));
    }
    let released = members_of(registry, array);
    ControlEffects {
        reply_len: write_status(out, Ok(())),
        released,
        stopped: Some(position),
        outcome: Ok(()),
        audit: ControlAudit::unrecorded(),
    }
}

/// The registry indices of every member of `array`, in descending order so the
/// caller can release them one by one without a later index shifting.
fn members_of(registry: &MemberRegistry, array: &ArrayUuidBytes) -> Vec<usize> {
    let mut indices: Vec<usize> = (0..registry.members().len())
        .filter(|&index| {
            registry
                .member_superblock(index)
                .is_some_and(|superblock| &superblock.array_uuid == array)
        })
        .collect();
    indices.sort_unstable_by(|a, b| b.cmp(a));
    indices
}

/// Write a status reply into `out`, returning its length.
fn write_status(out: &mut [u8], result: Result<(), Errno>) -> usize {
    let frame = encode_status_reply(result);
    let len = frame.len().min(out.len());
    out[..len].copy_from_slice(&frame[..len]);
    len
}

/// Write a create reply into `out`, returning its length.
fn write_create(out: &mut [u8], result: Result<ArrayUuidBytes, Errno>) -> usize {
    let frame = encode_create_reply(result);
    let len = frame.len().min(out.len());
    out[..len].copy_from_slice(&frame[..len]);
    len
}

#[cfg(test)]
mod tests;
