//! The freestanding body of the RAID array-composer `Run` binary
//! (`main.rs`): the reserved rendezvous, the offer→assemble→publish loop, and
//! the live-array serve path (`plans/FIX-IO.md` `IO6d`).
//!
//! # One process, one park, several event-timed sources
//!
//! The composer owns the reserved [`RAID_REGISTRY_ENDPOINT`] every member
//! agent offers to, and one block-service endpoint per live array. All of them
//! ride a single wait-set, and each turn the composer:
//!
//! 1. drains every pending offer, reading the offered device's superblock
//!    itself and feeding the registry;
//! 2. does what the registry's [`ComposerAction`] says — assemble and publish
//!    a ready array, place a returning member into a live one, or wait;
//! 3. serves every live array's queued block requests through the shared
//!    fault-aware engine;
//! 4. gives each array one bounded turn of self-maintenance — re-admitting a
//!    returning member, advancing a rebuild, verifying the array, or writing
//!    down where it has got to; and
//! 5. parks until the soonest of the registry's settle/backoff deadline, the
//!    arrays' recovery grace windows, and their maintenance deadlines, or
//!    until any endpoint signals.
//!
//! Nothing here spins: every wait is a one-shot timeout or a wait-set wake, so
//! a quiet composer costs a quiet core. A turn that actually moved a rebuild
//! or a scrub forward comes straight back round instead of parking, so an idle
//! array heals at full speed while a busy one keeps yielding to its workload —
//! but every such turn does real I/O, so the loop is never a poll.
//!
//! # A member node is a pointer to look, never a datum to believe
//!
//! An offered endpoint, window, and node id are hostile until proven: the
//! offer decodes fail-closed, the window must map at least the data length,
//! the device must answer geometry, and its array, slot, and generation come
//! from the superblock read off the disk — never from anything the agent said.
//! Any failure refuses that one membership and never disturbs the others.

extern crate alloc;

use alloc::vec::Vec;

use tairix_abi::blkio::{
    recovery_wait_timeout, BlkDeviceClass, BLK_COMPLETION_LEN, BLK_DATA_LEN, BLK_REQUEST_LEN,
};
use tairix_abi::driver::block::Block;
use tairix_abi::hwtree::{HwRemoveFlags, HwResource, HW_NODE_ROOT};
use tairix_abi::raid_admin::{
    RAID_CONTROL_ENDPOINT, RAID_CONTROL_MAX_REPLY, RAID_CONTROL_MAX_REQUEST,
};
use tairix_abi::raid_ipc::{
    MemberOffer, RAID_ARRAY_COMPATIBLE, RAID_MAX_REQUEST, RAID_REGISTRY_ENDPOINT,
};
use tairix_abi::random::RandomFlags;
use tairix_abi::reply::{encode_status_reply, STATUS_REPLY_LEN};
use tairix_abi::sysinfo::BlkHealthTransition;
use tairix_abi::time::Time64;
use tairix_abi::waitset::{WaitSetOp, WaitSourceKind};
use tairix_abi::{CapabilityId, Errno, HwDeviceClass, HwMatchKey, HwNode, Origin, ORIGIN_WIRE_LEN};
use tairix_blkclient::{RemoteBlock, RtBlkCall};
use tairix_caps::CapabilitySet;
use tairix_drv_storage_raid::{
    assemble_array, handle_control, read_superblock, Admission, ArrayHealthEvent, ArrayRuntime,
    Assembled, ComposerAction, ControlAudit, ControlEffects, LiveArrays, MaintenanceStep,
    MemberRegistry, ServiceError,
};
use tairix_drvrt::{RtDriverHost, RtGrantSyscalls};
use tairix_log::{log, Event, EventId, Field, FieldValue, Level};
use tairix_raid::{ArraySuperblock, MaintenanceAction, SuperblockError};
use tairix_rt::LogSink;
use tairix_util::fmt::format_hex_u64;

/// Exit code when the rt-backed driver host could not be built from the
/// kernel-delivered grants. A reserved, fail-closed value.
const EXIT_NO_HOST: i32 = 90;

/// Exit code when the composer could not bind its reserved rendezvous — no
/// member could ever offer, so there is nothing to do but exit for
/// supervision.
const EXIT_NO_REGISTRY: i32 = 91;

/// Exit code when the composer could not mint one of the two wait-sets it
/// parks on — without them every wait would have to become a poll.
const EXIT_NO_WAITSET: i32 = 92;

/// Exit code when the composer could not bind its reserved control endpoint.
/// An array nobody can create, grow, or stop is not the service this driver
/// promises, so it exits for supervision rather than serving half of it.
const EXIT_NO_CONTROL: i32 = 93;

/// Diagnostic event id: the composer bound its rendezvous and is ready for
/// offers.
const RAID_COMPOSER_READY: EventId = EventId(4191);

/// Diagnostic event id: a member's device was read and admitted into the
/// registry, its membership held open.
const RAID_MEMBER_ADMITTED: EventId = EventId(4192);

/// Diagnostic event id: an offered device was refused (a malformed offer, an
/// unmappable window, an unreadable device, a duplicate, or a full registry).
const RAID_MEMBER_REFUSED: EventId = EventId(4193);

/// Diagnostic event id: an array was assembled and published as a block
/// device; `devmgr` now loads the volume manager on it.
const RAID_ARRAY_PUBLISHED: EventId = EventId(4194);

/// Diagnostic event id: an array could not be brought online this attempt (a
/// member unreachable, a re-stamp write refused, or its resources could not be
/// created); the registry backs off before retrying.
const RAID_ARRAY_FAILED: EventId = EventId(4195);

/// Diagnostic event id: an array started short of full redundancy — a slot was
/// absent, or held a copy its own metadata proved is behind. A slot the
/// composer could not see is additionally fenced: the array's generation is
/// bumped and the survivors re-stamped, so that disk rejoins as the rebuild
/// target it is rather than as a copy trusted to be current.
const RAID_ARRAY_DEGRADED: EventId = EventId(4196);

/// Diagnostic event id: a returning or late member was placed into its live
/// array to be rebuilt from the survivors.
const RAID_MEMBER_JOINED: EventId = EventId(4197);

/// Diagnostic event id: an array resumed a verification pass or a rebuild its
/// members' records had recorded, rather than starting the pass over.
const RAID_ARRAY_RESUMED: EventId = EventId(4198);

/// Diagnostic event id: a maintenance turn failed — a re-add refused, a
/// rebuild or verification chunk that the members would not serve, or a
/// position the members would not record. The scheduler backs off before
/// trying again.
const RAID_MAINTENANCE_FAILED: EventId = EventId(4199);

/// Diagnostic event id: a verification pass completed over the whole array,
/// closing the window in which a latent media error could have sat undetected.
const RAID_SCRUB_COMPLETED: EventId = EventId(4200);

/// Diagnostic event id: an array lost redundancy but keeps serving.
const RAID_HEALTH_DEGRADED: EventId = EventId(4201);

/// Diagnostic event id: an array is rebuilding a member back into itself.
const RAID_HEALTH_RECOVERING: EventId = EventId(4202);

/// Diagnostic event id: an array is whole again — every member current.
const RAID_HEALTH_RECOVERED: EventId = EventId(4203);

/// Diagnostic event id: an array can no longer serve; too many members are
/// gone for its level to reconstruct what they held. Its consumers now get
/// typed fail-closed answers rather than data the array cannot vouch for.
const RAID_ARRAY_LOST: EventId = EventId(4204);

/// Audit event id: an administrative request that changes the composition was
/// allowed and carried out.
const RAID_CONTROL_ALLOWED: EventId = EventId(4205);

/// Audit event id: an administrative request that changes the composition was
/// refused — the caller lacked the capability, the frame did not decode, or the
/// operation's own preconditions were not met.
const RAID_CONTROL_REFUSED: EventId = EventId(4206);

/// Audit event id: a control request could not be attributed to a caller, so it
/// was refused unserved. The kernel attests every caller, so this is a
/// torn-down call or a ticket already answered — never a caller's choice.
const RAID_CONTROL_UNATTESTED: EventId = EventId(4207);

/// Diagnostic event id: an offered device carries no array metadata and is held
/// as an unaffiliated candidate — part of no array, and available only to an
/// explicit administrative create or add.
const RAID_CANDIDATE_HELD: EventId = EventId(4208);

/// Outstanding-membership capacity of the reserved rendezvous. Each admitted
/// member holds its offer call open for the life of its membership, so this
/// bounds concurrent memberships — a fail-closed kernel memory bound, not a
/// scaling capacity: past it an agent's offer is refused and its own paced
/// re-offer brings it back as memberships free, so no device is lost. It is
/// generous enough that a realistic machine's disks all register at once.
const REGISTRY_CAPACITY: usize = 256;

/// Outstanding-request capacity of the reserved control endpoint (a fail-closed
/// kernel memory bound, not a scaling capacity). Administration and status are
/// short calls posted by a handful of tools, and the composer answers each one
/// within the same turn it receives it, so a small queue absorbs a burst of
/// them; past it a caller is refused and retries. Deliberately its own queue,
/// so a flood of status calls can never cost the machine a member registration.
const CONTROL_CAPACITY: usize = 8;

/// Outstanding-request capacity of a per-array block-service endpoint. The
/// volume layer and the kernel's mounts submit one request at a time (each
/// blocks on its reply); a small queue absorbs a re-submit racing the previous
/// reply. It mirrors a leaf block driver's per-unit endpoint.
const ARRAY_ENDPOINT_CAPACITY: usize = 4;

/// The shared buffer one array's maintenance chunk is staged through.
///
/// It bounds how much of an array a single rebuild or verification turn
/// touches, so it is deliberately independent of how large the arrays are: a
/// bigger array takes more turns, never a bigger buffer, and one buffer serves
/// every array the composer holds because each turn uses as many whole blocks
/// of it as that array's geometry allows. Sixty-four kibibytes is large enough
/// that the per-turn overhead disappears against the transfer and small enough
/// to sit unnoticed on a machine with a gibibyte of memory serving several
/// arrays at once.
const MAINTENANCE_CHUNK_BYTES: usize = 64 * 1024;

/// Base of the id block the composer's per-array block-service endpoints are
/// bound in (`b"RAY\0"`-tagged, mirroring the leaf drivers' tagged endpoint
/// ranges). Each published array takes the next id up from this base; the
/// counter only advances once an id is actually bound, so a stranded id is
/// never reissued.
const ARRAY_ENDPOINT_BASE: u64 = 0x0052_4159_0000_0000;

/// A registered member, as the composer must reach it again: the block-service
/// endpoint it offered, the region id of the data window it delegated, and the
/// address that window is mapped at.
///
/// Nothing else is kept here — the device's metadata lives once in the
/// registry's reassembly candidate at the same index. The composer connects a
/// fresh client from the endpoint and base whenever it needs the device (at
/// assembly, or to place a returning member), so a failed assembly attempt
/// never strands a half-open client and a later retry simply reconnects. The
/// region id is kept so a later offer naming a window a membership already
/// holds is refused before it is mapped.
#[derive(Copy, Clone)]
struct Member {
    endpoint: u64,
    window: u64,
    window_base: usize,
}

/// The endpoint and data window one array will be served through, bound and
/// created *before* the array can be published.
///
/// A publish takes several steps that can each fail (bind the endpoint, create
/// the window, emit the node), and neither a bound call endpoint nor a created
/// shared region can be handed back to the kernel by a user process. Holding
/// the pair here and **reusing it on the next attempt** is what keeps a
/// repeatedly-failing publish from stranding a fresh endpoint id and region on
/// every retry — an unbounded kernel-memory leak an unassemblable array would
/// otherwise drive on its backoff timer forever. At most one pair is ever
/// outstanding.
struct PendingResources {
    endpoint: u64,
    window_id: u64,
    window_base: usize,
}

/// One live array: its fault-aware runtime and the data window its serve path
/// stages each transfer through.
struct LiveArray {
    runtime: ArrayRuntime<RemoteBlock<'static, RtBlkCall>>,
    window: &'static mut [u8],
}

/// The whole composer's mutable state: the assembly-decision registry, the
/// transport of each registered member (parallel to the registry's
/// candidates), the live arrays, the one wait-set every source rides, and the
/// per-array endpoint-id counter.
struct Composer {
    registry: MemberRegistry,
    members: Vec<Member>,
    arrays: Vec<LiveArray>,
    set: u64,
    /// The wait-set every *member* block transport parks its replies on.
    ///
    /// Deliberately separate from `set`: `set` carries the rendezvous and the
    /// arrays' service endpoints, which this process *serves*, while this one
    /// carries the reply completions of the members it *drives* as a client.
    /// One set for all of them, minted once, so reconnecting a member on every
    /// assembly attempt costs no new kernel object.
    member_set: u64,
    endpoint_counter: u32,
    /// An endpoint and window bound for an array whose publish did not finish,
    /// kept for the next attempt rather than stranded.
    pending: Option<PendingResources>,
    /// The one buffer every array's maintenance chunk is staged through. Empty
    /// only if it could not be allocated at startup, which leaves the arrays
    /// serving but unmaintained rather than taking the whole composer down.
    scratch: Vec<u8>,
}

/// The capability set the driver host re-checks up front; the kernel is the
/// authority and re-checks every trap. It is the least-privilege set the
/// composer needs: own the reserved rendezvous and each array's block endpoint
/// and connect to each member (`CAP_IPC_ENDPOINT`), map each member's window
/// and create each array's (`CAP_SHM`), publish the array node
/// (`CAP_HW_EMIT`), record its decisions (`CAP_LOG_EMIT`), and bind the
/// reserved rendezvous id a squatter must not claim first
/// (`CAP_IPC_BIND_PRIVILEGED`). No MMIO, DMA, IRQ, or mount authority.
fn driver_caps() -> CapabilitySet {
    let mut caps = CapabilitySet::empty();
    caps.insert(CapabilityId::IPC_ENDPOINT);
    caps.insert(CapabilityId::SHM);
    caps.insert(CapabilityId::HW_EMIT);
    caps.insert(CapabilityId::LOG_EMIT);
    caps.insert(CapabilityId::IPC_BIND_PRIVILEGED);
    caps
}

/// Emit one diagnostic record carrying a single hex-rendered field.
fn log_hex_event(id: EventId, level: Level, message: &'static str, key: &'static str, value: u64) {
    let mut value_buf = [0u8; 16];
    log(
        &LogSink,
        &Event {
            level,
            id,
            message,
            fields: &[Field {
                key,
                value: FieldValue::Str(format_hex_u64(value, &mut value_buf)),
            }],
        },
    );
}

/// Record a maintenance turn worth an operator's attention.
///
/// A rebuild or verification chunk is not logged per chunk — a pass is
/// thousands of them, and the array's health edges already mark where one
/// began and ended. What is recorded is a turn that *failed*, and the moment a
/// verification pass's completion reaches the members' records, which is when
/// the array can be said to have been verified durably.
fn log_maintenance_step(step: &MaintenanceStep, endpoint: u64) {
    match (step.action, step.outcome) {
        (_, Err(_)) => log_hex_event(
            RAID_MAINTENANCE_FAILED,
            Level::Warn,
            "raid: array maintenance turn refused by the members; backing off",
            "endpoint_hex",
            endpoint,
        ),
        (
            MaintenanceAction::Checkpoint {
                pass_completed: true,
                ..
            },
            Ok(()),
        ) => log_hex_event(
            RAID_SCRUB_COMPLETED,
            Level::Info,
            "raid: array verified end to end and the pass recorded on its members",
            "endpoint_hex",
            endpoint,
        ),
        _ => {}
    }
}

/// Answer an offer call, ending that one membership with a refusal.
fn reply_refused(ticket: u64, errno: Errno) {
    let frame = encode_status_reply(Err(errno));
    let _ = tairix_rt::call_reply(RAID_REGISTRY_ENDPOINT, ticket, &frame);
}

/// Connect a read/write block client over `endpoint`, staging through the
/// member's already-mapped data window at `window_base`.
///
/// The composer opens every member read/write from the one shared client
/// definition: it both reads superblocks and, on a degraded start, re-stamps
/// them.
fn connect(
    endpoint: u64,
    window_base: usize,
    member_set: u64,
) -> Option<RemoteBlock<'static, RtBlkCall>> {
    // SAFETY: `window_base` addresses a shared region this process mapped at
    // `BLK_DATA_LEN` bytes when the member was offered, and the mapping lives
    // for the rest of this process. No two live slices ever alias one window:
    // a membership whose window id another membership already holds is refused
    // before it is mapped, so each base belongs to exactly one member, and
    // that member's device is read by one transient probe that dies inside
    // `handle_offer` before any later client is connected over it.
    let window = unsafe { core::slice::from_raw_parts_mut(window_base as *mut u8, BLK_DATA_LEN) };
    RemoteBlock::connect_read_write(RtBlkCall::new(endpoint, member_set), window).ok()
}

/// Reconnect the block client for a registered member, for assembly or for
/// placing it into a live array.
fn connect_member(member: &Member, member_set: u64) -> Option<RemoteBlock<'static, RtBlkCall>> {
    connect(member.endpoint, member.window_base, member_set)
}

/// What reading an offered device's first block found.
enum Probed {
    /// The device carries valid array metadata, so it is a member of the array
    /// that metadata names.
    Member(ArraySuperblock, BlkDeviceClass),
    /// The device answered and its first block holds no array record at all, so
    /// it is a blank candidate: held, part of no array, and reachable only by an
    /// explicit administrative create or add.
    ///
    /// Metadata that is *present but damaged* is deliberately not this. A member
    /// whose record is merely corrupt must never be mistaken for an empty disk a
    /// create may overwrite, so only the complete absence of a record counts as
    /// blank.
    Blank(BlkDeviceClass),
    /// The device did not answer, or answered with a record that could not be
    /// trusted, so nothing about it can be relied on.
    Unusable,
}

/// Read an offered device's own array metadata and its declared device class
/// over a transient client that dies with this call.
///
/// Nothing the offering agent said about the device is believed: which array it
/// belongs to, which slot it fills, and how current it is are read off the disk
/// here. The same is true of what *kind* of device it is: an agent matched to a
/// candidate node claims the disk is blank, and this is where that claim is
/// checked against the disk itself.
fn probe_member(endpoint: u64, window_base: usize, member_set: u64) -> Probed {
    let Some(mut device) = connect(endpoint, window_base, member_set) else {
        return Probed::Unusable;
    };
    let class = device.device_class();
    match read_superblock(&mut device) {
        Ok(superblock) => Probed::Member(superblock, class),
        Err(ServiceError::Superblock(SuperblockError::BadMagic)) => Probed::Blank(class),
        Err(_) => Probed::Unusable,
    }
}

/// Map an offered data window, returning its mapped base address, or [`None`]
/// when it cannot be mapped or is too small for the block data protocol. A
/// window that maps but is too short is released again, so a stream of
/// undersized offers cannot fill the composer's address space.
fn map_offer_window(window_id: u64) -> Option<usize> {
    let mut len = 0u64;
    let base = tairix_rt::shm_map(window_id, &mut len);
    let base = usize::try_from(base).ok()?;
    if len < BLK_DATA_LEN as u64 {
        unmap_window(base);
        return None;
    }
    Some(base)
}

/// Release a window this process mapped, used on every path that maps one and
/// then refuses the membership it belonged to.
fn unmap_window(base: usize) {
    let _ = tairix_rt::shm_unmap(base as u64, BLK_DATA_LEN);
}

/// Bind the reserved rendezvous the member agents offer to. Grant-restricted
/// receive is unnecessary — the composer owns and drains it — but the id is
/// reserved, so binding it needs `CAP_IPC_BIND_PRIVILEGED`, which stops a
/// squatter from claiming it first and harvesting members' transports.
fn create_registry_endpoint() -> bool {
    let send_caps = CapabilitySet::empty();
    let recv_caps = CapabilitySet::empty();
    tairix_rt::call_create(
        RAID_REGISTRY_ENDPOINT,
        &send_caps,
        &recv_caps,
        RAID_MAX_REQUEST,
        STATUS_REPLY_LEN,
        REGISTRY_CAPACITY,
    ) == 0
}

/// Bind the reserved control endpoint an administrator drives.
///
/// Left open to any sender on purpose: which callers may *read* the composition
/// and which may *change* it differ per operation, so the gate is the
/// per-request check against the caller's kernel-attested capabilities rather
/// than one endpoint-wide rule. The id is reserved, so binding it needs
/// `CAP_IPC_BIND_PRIVILEGED` — that is what stops a squatter from answering
/// create and stop requests in the composer's name.
fn create_control_endpoint() -> bool {
    let send_caps = CapabilitySet::empty();
    let recv_caps = CapabilitySet::empty();
    tairix_rt::call_create(
        RAID_CONTROL_ENDPOINT,
        &send_caps,
        &recv_caps,
        RAID_CONTROL_MAX_REQUEST,
        RAID_CONTROL_MAX_REPLY,
        CONTROL_CAPACITY,
    ) == 0
}

/// Create one array's own block-service endpoint `id`. Binding it
/// grant-restricted (`send_caps` carries `CAP_IPC_ENDPOINT`) makes the kernel
/// mint the composer the matching per-endpoint grant, which it forwards onto
/// the published array node so the volume manager inherits exactly the right
/// to drive this one array. `true` when the id was bound.
fn create_array_endpoint(id: u64) -> bool {
    let mut send_caps = CapabilitySet::empty();
    send_caps.insert(CapabilityId::IPC_ENDPOINT);
    let recv_caps = CapabilitySet::empty();
    tairix_rt::call_create(
        id,
        &send_caps,
        &recv_caps,
        BLK_REQUEST_LEN,
        BLK_COMPLETION_LEN,
        ARRAY_ENDPOINT_CAPACITY,
    ) == 0
}

/// Create and map one array's shared data window, returning its mapped base
/// and its region id.
fn create_array_window() -> Option<(usize, u64)> {
    let mut id = 0u64;
    let base = tairix_rt::shm_create(BLK_DATA_LEN, &mut id);
    Some((usize::try_from(base).ok()?, id))
}

/// Build the array's hardware-tree node, carrying the `tairix,raid-array`
/// compatible key and this array's block endpoint and shared window as
/// resources, or [`None`] if the node's fixed contents somehow overflow it.
fn build_array_node(endpoint: u64, window_id: u64) -> Option<HwNode> {
    let mut node = HwNode::new(0, HW_NODE_ROOT, HwDeviceClass::Storage);
    let built = HwMatchKey::compatible(RAID_ARRAY_COMPATIBLE)
        .and_then(|key| node.push_match_key(key))
        .and_then(|()| node.push_resource(HwResource::endpoint(endpoint)))
        .and_then(|()| node.push_resource(HwResource::shared(window_id)));
    built.ok().map(|()| node)
}

/// The composer's live arrays, as the administration layer reaches them.
///
/// Borrowing the arrays alone is what lets one control request see the
/// registry, the member transports, and the arrays at once without any of them
/// borrowing the whole composer.
struct Live<'a>(&'a mut [LiveArray]);

impl LiveArrays for Live<'_> {
    type Device = RemoteBlock<'static, RtBlkCall>;

    fn count(&self) -> usize {
        self.0.len()
    }

    fn runtime_mut(&mut self, index: usize) -> Option<&mut ArrayRuntime<Self::Device>> {
        self.0.get_mut(index).map(|live| &mut live.runtime)
    }

    fn position(&self, array: &[u8; 16]) -> Option<usize> {
        self.0
            .iter()
            .position(|live| live.runtime.identity().array_uuid == *array)
    }
}

/// Read the kernel-attested identity of the caller holding `ticket`.
///
/// Nothing in the request frame is consulted. The authority an operation is
/// judged against comes from the kernel's own record of who called, which a
/// caller can neither forge nor inflate.
fn peer_origin(ticket: u64) -> Option<Origin> {
    let mut bytes = [0u8; ORIGIN_WIRE_LEN];
    let len = tairix_rt::call_peer_origin(RAID_CONTROL_ENDPOINT, ticket, &mut bytes).ok()?;
    if len != bytes.len() {
        return None;
    }
    Origin::from_bytes(&bytes).ok()
}

/// Fill a new array's identity from the kernel CSPRNG, reporting whether it
/// could be filled.
///
/// A create mints the identity here rather than taking one from its caller: two
/// arrays sharing an identity would be indistinguishable to reassembly, which
/// could place one array's disk into the other. A draw that cannot be served
/// leaves the create refused rather than falling back to a guessable value.
fn fill_random(bytes: &mut [u8; 16]) -> bool {
    tairix_rt::random_get(bytes, RandomFlags::empty()).is_ok_and(|len| len == bytes.len())
}

/// Retire a stopped array's published node, refusing while a volume is still
/// attached to it.
///
/// The orderly removal is what keeps an array from being stopped out from under
/// a mounted filesystem: the kernel answers `Busy` while a volume is attached
/// on an endpoint the node declares, and that refusal reaches the administrator
/// unchanged with the array left running.
fn remove_node_orderly(node_id: u32) -> Result<(), Errno> {
    let outcome = tairix_rt::hw_remove_node(node_id, HwRemoveFlags::ORDERLY);
    if outcome == 0 {
        return Ok(());
    }
    Err(Errno::from_syscall(outcome))
}

/// Record an administrative decision on the audit trail.
///
/// Reads change nothing and a status poll would drown the trail, so only
/// mutations are recorded — and every one of them is, allowed or refused,
/// naming the operation, the array and device it named, and the refusal reason.
fn log_control_decision(audit: &ControlAudit, outcome: Result<(), Errno>) {
    if !audit.mutation {
        return;
    }
    let (id, level, message) = match outcome {
        Ok(()) => (
            RAID_CONTROL_ALLOWED,
            Level::Info,
            "raid: administrative request allowed and carried out",
        ),
        Err(_) => (
            RAID_CONTROL_REFUSED,
            Level::Warn,
            "raid: administrative request refused",
        ),
    };
    let errno = match outcome {
        Ok(()) => 0,
        Err(refusal) => refusal as u64,
    };
    let mut array_buf = [0u8; 16];
    let mut node_buf = [0u8; 16];
    let mut errno_buf = [0u8; 16];
    log(
        &LogSink,
        &Event {
            level,
            id,
            message,
            fields: &[
                Field {
                    key: "op",
                    value: FieldValue::Str(audit.op),
                },
                Field {
                    key: "array_hex",
                    value: FieldValue::Str(format_hex_u64(audit.array_tag, &mut array_buf)),
                },
                Field {
                    key: "node_hex",
                    value: FieldValue::Str(format_hex_u64(u64::from(audit.node), &mut node_buf)),
                },
                Field {
                    key: "errno_hex",
                    value: FieldValue::Str(format_hex_u64(errno, &mut errno_buf)),
                },
            ],
        },
    );
}

/// Reconnect a member's device and add it to the matching live array's absent
/// slot, returning whether the placement took.
fn place_returning_member(
    members: &[Member],
    arrays: &mut [LiveArray],
    member_set: u64,
    array_uuid: [u8; 16],
    member: usize,
    slot: u16,
) -> bool {
    let Some(live) = arrays
        .iter_mut()
        .find(|live| live.runtime.identity().array_uuid == array_uuid)
    else {
        return false;
    };
    let Some(raw) = members
        .get(member)
        .and_then(|held| connect_member(held, member_set))
    else {
        return false;
    };
    live.runtime.place_member(slot, raw).is_ok()
}

impl Composer {
    /// A composer serving on `set`, driving its members over `member_set`, and
    /// staging its arrays' maintenance chunks through `scratch`.
    fn new(set: u64, member_set: u64, scratch: Vec<u8>) -> Self {
        Self {
            registry: MemberRegistry::new(),
            members: Vec::new(),
            arrays: Vec::new(),
            set,
            member_set,
            endpoint_counter: 0,
            pending: None,
            scratch,
        }
    }

    /// The endpoint and window the next array will be served through: the pair
    /// a previous unfinished publish left behind, or a freshly bound and
    /// created one.
    ///
    /// Reusing the outstanding pair is what bounds the cost of a repeatedly
    /// failing publish at one endpoint and one region for the whole process,
    /// rather than one of each per backoff-timed retry.
    fn take_resources(&mut self) -> Option<PendingResources> {
        if let Some(held) = self.pending.take() {
            return Some(held);
        }
        let endpoint = ARRAY_ENDPOINT_BASE | u64::from(self.endpoint_counter);
        if !create_array_endpoint(endpoint) {
            return None;
        }
        // The id is bound from here on, so the counter advances even if the
        // steps below fail: a bound id is never re-derived for another array.
        self.endpoint_counter = self.endpoint_counter.saturating_add(1);
        let (window_base, window_id) = create_array_window()?;
        // The endpoint is bound but the window could not be created, so hand
        // the endpoint back for the next attempt rather than stranding it.
        self.pending = Some(PendingResources {
            endpoint,
            window_id,
            window_base,
        });
        self.pending.take()
    }

    /// Drain every pending offer on the rendezvous, without blocking.
    fn drain_offers(&mut self, now_ns: u64) {
        loop {
            let mut request = [0u8; RAID_MAX_REQUEST];
            let mut ticket = 0u64;
            match tairix_rt::call_recv_nonblock(RAID_REGISTRY_ENDPOINT, &mut request, &mut ticket) {
                Ok(len) => {
                    let end = len.min(request.len());
                    self.handle_offer(ticket, &request[..end], now_ns);
                }
                Err(_) => return,
            }
        }
    }

    /// Read one offered device and register it, holding its membership open
    /// when the registry accepts it and refusing the offer when it does not.
    ///
    /// Every refusal after the window is mapped releases that mapping again,
    /// so an agent that keeps offering devices the composer cannot use cannot
    /// grow the composer's address space without bound.
    fn handle_offer(&mut self, ticket: u64, bytes: &[u8], now_ns: u64) {
        let offer = match MemberOffer::decode(bytes) {
            Ok(offer) => offer,
            Err(errno) => {
                reply_refused(ticket, errno);
                return;
            }
        };
        // A window another membership already holds is refused before it is
        // mapped: two members staging their transfers over one region would
        // corrupt each other's data, and mapping it a second time would make
        // two exclusive slices over the same bytes.
        if self.members.iter().any(|held| held.window == offer.window) {
            reply_refused(ticket, Errno::AlreadyExists);
            log_hex_event(
                RAID_MEMBER_REFUSED,
                Level::Warn,
                "raid: offered window is already held by another membership",
                "window_hex",
                offer.window,
            );
            return;
        }
        let Some(window_base) = map_offer_window(offer.window) else {
            reply_refused(ticket, Errno::NotFound);
            log_hex_event(
                RAID_MEMBER_REFUSED,
                Level::Warn,
                "raid: offered window could not be mapped",
                "window_hex",
                offer.window,
            );
            return;
        };
        let (admission, admitted) = match probe_member(offer.endpoint, window_base, self.member_set)
        {
            Probed::Member(superblock, class) => (
                self.registry
                    .admit(ticket, offer, class, superblock, now_ns),
                (
                    RAID_MEMBER_ADMITTED,
                    "raid: member admitted and membership held open",
                ),
            ),
            Probed::Blank(class) => (
                self.registry.admit_candidate(ticket, offer, class),
                (
                    RAID_CANDIDATE_HELD,
                    "raid: blank device held as an unaffiliated array candidate",
                ),
            ),
            Probed::Unusable => {
                unmap_window(window_base);
                reply_refused(ticket, Errno::BadMagic);
                log_hex_event(
                    RAID_MEMBER_REFUSED,
                    Level::Warn,
                    "raid: offered device did not answer with metadata that can be trusted",
                    "endpoint_hex",
                    offer.endpoint,
                );
                return;
            }
        };
        self.note_admission(admission, ticket, offer, window_base, admitted);
    }

    /// Record the registry's verdict on an offered device: hold its transport
    /// when it was registered, and give back the window and the membership when
    /// it was not.
    ///
    /// `admitted` is the record to emit when the device was registered — a
    /// member of an array, or a blank candidate — which is the one thing that
    /// differs between the two kinds of device the composer holds.
    fn note_admission(
        &mut self,
        admission: Admission,
        ticket: u64,
        offer: MemberOffer,
        window_base: usize,
        admitted: (EventId, &'static str),
    ) {
        match admission {
            // The registry's index is the reassembly tag the composer will be
            // asked to supply a device for, and this table is what resolves
            // that tag to a physical disk. Checking that the two agree rather
            // than assuming they do is not defensive noise: a desync would
            // silently hand one member's slot another member's device, so a
            // rebuild would overwrite a healthy disk with a sibling's data.
            Admission::Registered { index } if index == self.members.len() => {
                self.members.push(Member {
                    endpoint: offer.endpoint,
                    window: offer.window,
                    window_base,
                });
                let (id, message) = admitted;
                log_hex_event(id, Level::Info, message, "endpoint_hex", offer.endpoint);
            }
            Admission::Registered { index } => {
                // Unreachable while the composer never releases a member, and
                // fatal to data integrity if it ever became reachable, so the
                // membership is refused and the disk left untouched.
                self.registry.release(index);
                unmap_window(window_base);
                reply_refused(ticket, Errno::OutOfRange);
                log_hex_event(
                    RAID_MEMBER_REFUSED,
                    Level::Error,
                    "raid: member index desynchronised from its transport; refusing",
                    "index_hex",
                    index as u64,
                );
            }
            Admission::Duplicate => {
                unmap_window(window_base);
                reply_refused(ticket, Errno::AlreadyExists);
            }
            Admission::OutOfMemory => {
                unmap_window(window_base);
                reply_refused(ticket, Errno::OutOfMemory);
            }
        }
    }

    /// Drain every pending administration or status request, without blocking.
    fn drain_control(&mut self, now_ns: u64, now_wall: Time64) {
        loop {
            let mut request = [0u8; RAID_CONTROL_MAX_REQUEST];
            let mut ticket = 0u64;
            match tairix_rt::call_recv_nonblock(RAID_CONTROL_ENDPOINT, &mut request, &mut ticket) {
                Ok(len) => {
                    let end = len.min(request.len());
                    self.serve_control(ticket, &request[..end], now_ns, now_wall);
                }
                Err(_) => return,
            }
        }
    }

    /// Serve one administration or status request against the caller's
    /// kernel-attested authority, carry out what the decision requires of the
    /// transports the composer owns, record it, and answer the caller.
    fn serve_control(&mut self, ticket: u64, frame: &[u8], now_ns: u64, now_wall: Time64) {
        let Some(origin) = peer_origin(ticket) else {
            // The kernel attests every caller, so a call it will not name is one
            // already gone — its ticket cancelled or answered. Nothing is read,
            // written, or answered on its behalf.
            log_hex_event(
                RAID_CONTROL_UNATTESTED,
                Level::Warn,
                "raid: control request could not be attributed to a caller; refused unserved",
                "ticket_hex",
                ticket,
            );
            return;
        };
        let mut reply = [0u8; RAID_CONTROL_MAX_REPLY];
        // Disjoint borrows: one request may read the registry, reconnect a
        // member from the transport table, and drive a live array at once.
        let Self {
            registry,
            members,
            arrays,
            member_set,
            ..
        } = self;
        let member_set = *member_set;
        let mut live = Live(arrays.as_mut_slice());
        let effects = handle_control(
            registry,
            &mut live,
            origin.capabilities(),
            frame,
            now_wall,
            now_ns,
            |index| {
                members
                    .get(index)
                    .and_then(|held| connect_member(held, member_set))
            },
            fill_random,
            remove_node_orderly,
            &mut reply,
        );
        let reply_len = effects.reply_len;
        // The effects come before the answer, so the state the caller sees on
        // its next request is the state its answer describes.
        self.carry_out(&effects);
        log_control_decision(&effects.audit, effects.outcome);
        let _ = tairix_rt::call_reply(RAID_CONTROL_ENDPOINT, ticket, &reply[..reply_len]);
    }

    /// Carry out what a served request left for the owner of the transports:
    /// tear down a stopped array's serve state, and end the membership of every
    /// device the operation freed.
    fn carry_out(&mut self, effects: &ControlEffects) {
        // The array is torn down first: its runtime owns the block clients that
        // stage through the members' data windows, and those windows are given
        // back by the releases below, so dropping the clients afterwards would
        // leave them addressing memory this process no longer holds.
        if let Some(position) = effects.stopped {
            self.retire_array(position);
        }
        // Descending order, so releasing one member never shifts a later index.
        for &index in &effects.released {
            self.release_membership(index);
        }
    }

    /// End the membership of the registry member at `index`: give back the
    /// window this process mapped for it and answer its outstanding offer, which
    /// is what tells its agent the device is free to be offered again.
    fn release_membership(&mut self, index: usize) {
        if index >= self.members.len() {
            return;
        }
        let Some(held) = self.registry.release(index) else {
            return;
        };
        let member = self.members.remove(index);
        unmap_window(member.window_base);
        let frame = encode_status_reply(Ok(()));
        let _ = tairix_rt::call_reply(RAID_REGISTRY_ENDPOINT, held.membership(), &frame);
    }

    /// Tear down the serve state of the array at `position`, whose published
    /// node has already been retired: stop watching its endpoint, drop the
    /// runtime — which gives up the member devices it composed — and release its
    /// data window.
    ///
    /// The endpoint id is deliberately **not** reused for a later array. A user
    /// process cannot hand a bound id back to the kernel, but the grant over it
    /// was forwarded to whoever drove this array, and re-publishing that id
    /// would let the previous consumer's grant reach a different array's data.
    /// One stranded id per administrative stop is the honest price.
    fn retire_array(&mut self, position: usize) {
        if position >= self.arrays.len() {
            return;
        }
        let live = self.arrays.remove(position);
        let endpoint = live.runtime.endpoint();
        let window_base = live.window.as_ptr() as usize;
        let _ = tairix_rt::waitset_ctl(
            self.set,
            WaitSetOp::Del,
            WaitSourceKind::Endpoint,
            endpoint,
            endpoint,
        );
        drop(live);
        unmap_window(window_base);
    }

    /// Drive the registry's decisions until it asks to wait, returning the
    /// absolute settle/backoff deadline to park on (if any).
    fn drive_actions(&mut self, now_ns: u64, now_wall: Time64) -> Option<u64> {
        loop {
            match self.registry.next_action(now_ns) {
                ComposerAction::Assemble { array_uuid } => {
                    self.assemble_and_publish(now_ns, now_wall, array_uuid);
                }
                ComposerAction::Join {
                    array_uuid,
                    member,
                    slot,
                    in_sync: _,
                } => self.join_member(array_uuid, member, slot, now_ns),
                ComposerAction::Wait { deadline_ns } => return deadline_ns,
            }
        }
    }

    /// Assemble a ready array and publish it, or record a failure so the
    /// registry backs off before retrying.
    fn assemble_and_publish(&mut self, now_ns: u64, now_wall: Time64, array_uuid: [u8; 16]) {
        // The resources come first: they are what a failed attempt must be
        // able to hand back, and assembling before them would leave a composed
        // array (holding its members' devices) to be dropped and rebuilt.
        let Some((resources, node)) = self.claim_resources(array_uuid, now_ns) else {
            return;
        };
        let Some(assembled) = self.assemble(array_uuid, now_wall, now_ns) else {
            self.pending = Some(resources);
            return;
        };
        self.publish(assembled, resources, &node, array_uuid, now_ns);
    }

    /// Claim the endpoint and data window a ready array will be served
    /// through, and the node that names them.
    ///
    /// [`None`] backs the registry off: the resources could not be created, or
    /// they exist but the node describing them could not be built, in which
    /// case they are held for the next attempt rather than stranded.
    fn claim_resources(
        &mut self,
        array_uuid: [u8; 16],
        now_ns: u64,
    ) -> Option<(PendingResources, HwNode)> {
        let Some(resources) = self.take_resources() else {
            self.registry.note_assembly_failed(array_uuid, now_ns);
            log_hex_event(
                RAID_ARRAY_FAILED,
                Level::Warn,
                "raid: array endpoint or data window unavailable; backing off",
                "array_hex",
                self.arrays.len() as u64,
            );
            return None;
        };
        let Some(node) = build_array_node(resources.endpoint, resources.window_id) else {
            self.pending = Some(resources);
            self.registry.note_assembly_failed(array_uuid, now_ns);
            return None;
        };
        Some((resources, node))
    }

    /// Compose the array from the members the registry holds, reconnecting
    /// each one's device as assembly asks for it.
    ///
    /// [`None`] has already backed the registry off; the caller still owns the
    /// resources and hands them back.
    fn assemble(
        &mut self,
        array_uuid: [u8; 16],
        now_wall: Time64,
        now_ns: u64,
    ) -> Option<Assembled<RemoteBlock<'static, RtBlkCall>>> {
        let Some(identity) = self.registry.identity(array_uuid) else {
            self.registry.note_assembly_failed(array_uuid, now_ns);
            return None;
        };
        let member_set = self.member_set;
        let members = &self.members;
        let outcome = assemble_array(identity, self.registry.candidates(), now_wall, |tag| {
            members
                .get(tag)
                .and_then(|held| connect_member(held, member_set))
        });
        let Ok(assembled) = outcome else {
            self.registry.note_assembly_failed(array_uuid, now_ns);
            log_hex_event(
                RAID_ARRAY_FAILED,
                Level::Warn,
                "raid: array could not be assembled; backing off",
                "members_hex",
                self.members.len() as u64,
            );
            return None;
        };
        Some(assembled)
    }

    /// Publish a composed array: emit its node, build the runtime that serves
    /// it, and put it on the wait-set.
    ///
    /// Every failure hands the resources back and backs the registry off, so a
    /// publish that cannot finish costs nothing that cannot be retried.
    fn publish(
        &mut self,
        assembled: Assembled<RemoteBlock<'static, RtBlkCall>>,
        resources: PendingResources,
        node: &HwNode,
        array_uuid: [u8; 16],
        now_ns: u64,
    ) {
        // The fields are copies; the pair itself is what a failure below hands
        // back, so the next attempt reuses the same bound endpoint and region.
        let PendingResources {
            endpoint,
            window_id,
            window_base,
        } = resources;
        let Ok(node_id) = u32::try_from(tairix_rt::hw_emit_node(node)) else {
            self.pending = Some(resources);
            self.registry.note_assembly_failed(array_uuid, now_ns);
            log_hex_event(
                RAID_ARRAY_FAILED,
                Level::Warn,
                "raid: array node could not be published; backing off",
                "endpoint_hex",
                endpoint,
            );
            return;
        };
        if assembled.degraded {
            log_hex_event(
                RAID_ARRAY_DEGRADED,
                Level::Warn,
                "raid: array started short of full redundancy",
                "endpoint_hex",
                endpoint,
            );
        }
        let resumed = assembled.resume.progress.is_active();
        let Ok(runtime) = ArrayRuntime::new(
            assembled.identity,
            assembled.array,
            endpoint,
            window_id,
            node_id,
            assembled.resume,
            now_ns,
        ) else {
            // The node is already published but nothing will serve it, so
            // withdraw it rather than leave the volume manager driving an
            // endpoint with no server behind it.
            let _ = tairix_rt::hw_remove_node(node_id, tairix_abi::HwRemoveFlags::empty());
            self.pending = Some(resources);
            self.registry.note_assembly_failed(array_uuid, now_ns);
            log_hex_event(
                RAID_ARRAY_FAILED,
                Level::Warn,
                "raid: array runtime could not be built; backing off",
                "endpoint_hex",
                endpoint,
            );
            return;
        };
        if resumed {
            log_hex_event(
                RAID_ARRAY_RESUMED,
                Level::Info,
                "raid: array resumed the maintenance pass its members had recorded",
                "endpoint_hex",
                endpoint,
            );
        }
        self.registry.note_composed(array_uuid, &assembled.slots);
        // SAFETY: `window_base` addresses the `BLK_DATA_LEN` cacheable RW bytes
        // `create_array_window` minted and mapped for this array, and the
        // mapping lives for the rest of this process. The region is reached
        // only through `pending`/`arrays`, each of which holds it at most once,
        // so this is the only slice over it.
        let window =
            unsafe { core::slice::from_raw_parts_mut(window_base as *mut u8, BLK_DATA_LEN) };
        self.arrays.push(LiveArray { runtime, window });
        let _ = tairix_rt::waitset_ctl(
            self.set,
            WaitSetOp::Add,
            WaitSourceKind::Endpoint,
            endpoint,
            endpoint,
        );
        log_hex_event(
            RAID_ARRAY_PUBLISHED,
            Level::Info,
            "raid: array assembled and published as a block device",
            "endpoint_hex",
            endpoint,
        );
    }

    /// Place a returning or late member into its live array so it can be
    /// rebuilt from the survivors, then mark it placed so it is not offered
    /// again.
    ///
    /// A placement that cannot proceed — the array is gone, the device cannot
    /// be reconnected, or the array refuses the slot — is still marked placed
    /// and logged, so a member that cannot join does not drive a hot re-offer
    /// loop; resuming its rebuild is later maintenance work.
    ///
    /// Either way the array is told the device has demonstrably returned: the
    /// commonest reason a placement is refused is that the slot still *holds*
    /// that device as a faulted member, and the fresh offer is exactly the
    /// evidence that re-probing it now is worth doing rather than waiting out
    /// an escalated backoff.
    fn join_member(&mut self, array_uuid: [u8; 16], member: usize, slot: u16, now_ns: u64) {
        let placed = place_returning_member(
            &self.members,
            &mut self.arrays,
            self.member_set,
            array_uuid,
            member,
            slot,
        );
        if let Some(live) = self
            .arrays
            .iter_mut()
            .find(|live| live.runtime.identity().array_uuid == array_uuid)
        {
            live.runtime.note_member_returned(slot, now_ns);
        }
        self.registry.note_joined(member);
        if placed {
            log_hex_event(
                RAID_MEMBER_JOINED,
                Level::Info,
                "raid: returning member placed into its live array for rebuild",
                "slot_hex",
                u64::from(slot),
            );
        } else {
            log_hex_event(
                RAID_MEMBER_REFUSED,
                Level::Warn,
                "raid: returning member could not be placed into its live array",
                "slot_hex",
                u64::from(slot),
            );
        }
    }

    /// Serve every queued block request across all live arrays, without
    /// blocking on any one endpoint.
    fn serve_arrays(&mut self, now_ns: u64) {
        for live in &mut self.arrays {
            let endpoint = live.runtime.endpoint();
            loop {
                let mut request = [0u8; BLK_REQUEST_LEN];
                let mut ticket = 0u64;
                match tairix_rt::call_recv_nonblock(endpoint, &mut request, &mut ticket) {
                    Ok(len) => {
                        let end = len.min(request.len());
                        let mut reply = [0u8; BLK_COMPLETION_LEN];
                        let framed =
                            live.runtime
                                .serve(&request[..end], live.window, &mut reply, now_ns);
                        let _ = tairix_rt::call_reply(endpoint, ticket, &reply[..framed]);
                    }
                    Err(_) => break,
                }
            }
        }
    }

    /// Advance every live array's recovery grace window on a pure time tick,
    /// so an array left recovering with no further request still fails closed
    /// on time off the one-shot timer rather than a busy-poll.
    fn poll_arrays(&mut self, now_ns: u64) {
        for live in &mut self.arrays {
            let _ = live.runtime.poll(now_ns);
        }
    }

    /// Give every live array one bounded turn of self-maintenance, reporting
    /// whether any of them did work.
    ///
    /// A turn that did work means the next chunk may already be due, so the
    /// caller comes back round — draining any request that arrived meanwhile —
    /// rather than parking. Each turn is real I/O against the members, so that
    /// is a worker running, never a poll: the moment the foreground workload
    /// or the duty share says to wait, the scheduler answers idle and the loop
    /// parks on its deadline.
    fn maintain_arrays(&mut self, now_wall: Time64) -> bool {
        let Self {
            arrays, scratch, ..
        } = self;
        if scratch.is_empty() {
            return false;
        }
        let mut clock = tairix_rt::clock_get;
        let mut worked = false;
        for live in arrays.iter_mut() {
            let Some(step) = live.runtime.maintain(scratch, now_wall, &mut clock) else {
                continue;
            };
            worked = true;
            log_maintenance_step(&step, live.runtime.endpoint());
        }
        worked
    }

    /// Record every live array's change of health, so an operator sees a
    /// degrade, a rebuild, and a recovery as they happen rather than only in
    /// the mount table.
    fn report_health(&mut self) {
        for live in &mut self.arrays {
            let Some(event) = live.runtime.health_event() else {
                continue;
            };
            let endpoint = live.runtime.endpoint();
            let (id, level, message) = match event {
                ArrayHealthEvent::Health(BlkHealthTransition::Degraded) => (
                    RAID_HEALTH_DEGRADED,
                    Level::Warn,
                    "raid: array lost redundancy and is serving degraded",
                ),
                ArrayHealthEvent::Health(BlkHealthTransition::Recovering) => (
                    RAID_HEALTH_RECOVERING,
                    Level::Warn,
                    "raid: array is rebuilding a member back into itself",
                ),
                ArrayHealthEvent::Health(BlkHealthTransition::Recovered) => (
                    RAID_HEALTH_RECOVERED,
                    Level::Info,
                    "raid: array is whole again; every member current",
                ),
                ArrayHealthEvent::Lost => (
                    RAID_ARRAY_LOST,
                    Level::Error,
                    "raid: array can no longer serve; too many members gone to reconstruct",
                ),
            };
            log_hex_event(id, level, message, "endpoint_hex", endpoint);
        }
    }

    /// The soonest relative timeout to park on: the nearest of the registry's
    /// absolute settle/backoff deadline, the arrays' maintenance deadlines,
    /// and their recovery grace windows, or "no timeout" when none of them
    /// arms one.
    fn park_timeout(&self, wait_deadline_ns: Option<u64>, now_ns: u64) -> u64 {
        let maintenance = self
            .arrays
            .iter()
            .filter_map(|live| live.runtime.maintenance_deadline_ns())
            .min();
        let absolute = [wait_deadline_ns, maintenance]
            .into_iter()
            .flatten()
            .min()
            .map(|deadline| deadline.saturating_sub(now_ns));
        let recovery =
            recovery_wait_timeout(self.arrays.iter().map(|live| live.runtime.health()), now_ns);
        [absolute, recovery]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(u64::MAX)
    }
}

fn main() -> i32 {
    let Ok(_host) = RtDriverHost::from_grants_query(driver_caps(), RtGrantSyscalls, None) else {
        return EXIT_NO_HOST;
    };
    if !create_registry_endpoint() {
        log_hex_event(
            RAID_MEMBER_REFUSED,
            Level::Error,
            "raid: could not bind the array-composer rendezvous, exiting",
            "endpoint_hex",
            RAID_REGISTRY_ENDPOINT,
        );
        return EXIT_NO_REGISTRY;
    }
    if !create_control_endpoint() {
        log_hex_event(
            RAID_CONTROL_REFUSED,
            Level::Error,
            "raid: could not bind the array-composer control endpoint, exiting",
            "endpoint_hex",
            RAID_CONTROL_ENDPOINT,
        );
        return EXIT_NO_CONTROL;
    }
    let Ok(set) = u64::try_from(tairix_rt::waitset_create()) else {
        return EXIT_NO_WAITSET;
    };
    // A second set, for the members this process drives as a *client*. Minted
    // once here so opening a member transport — which happens on every
    // assembly attempt — never mints a kernel object of its own.
    let Ok(member_set) = u64::try_from(tairix_rt::waitset_create()) else {
        return EXIT_NO_WAITSET;
    };
    // Both served endpoints ride the one set, so a single park covers a member
    // offering itself and an administrator driving the composition; neither
    // needs a thread or a poll of its own.
    for endpoint in [RAID_REGISTRY_ENDPOINT, RAID_CONTROL_ENDPOINT] {
        if tairix_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::Endpoint,
            endpoint,
            endpoint,
        ) < 0
        {
            return EXIT_NO_WAITSET;
        }
    }
    log_hex_event(
        RAID_COMPOSER_READY,
        Level::Info,
        "raid: array composer ready; awaiting member offers and administration",
        "endpoint_hex",
        RAID_REGISTRY_ENDPOINT,
    );

    let mut composer = Composer::new(set, member_set, maintenance_scratch());
    loop {
        let now_ns = tairix_rt::clock_get();
        let now_wall = tairix_rt::wall_time()
            .map(|reading| reading.time())
            .unwrap_or_default();

        composer.drain_offers(now_ns);
        composer.drain_control(now_ns, now_wall);
        let wait_deadline_ns = composer.drive_actions(now_ns, now_wall);
        composer.serve_arrays(now_ns);
        let maintained = composer.maintain_arrays(now_wall);
        composer.report_health();

        // Maintenance moves real bytes, so the reading taken at the top of the
        // turn is stale by now; the grace windows and the park must both be
        // measured against the clock as it stands.
        let now_ns = tairix_rt::clock_get();
        composer.poll_arrays(now_ns);
        if maintained {
            continue;
        }

        let timeout_ns = composer.park_timeout(wait_deadline_ns, now_ns);
        let mut token = 0u64;
        let _ = tairix_rt::waitset_wait(set, timeout_ns, &mut token);
    }
}

/// The one buffer every array's maintenance chunk is staged through, or an
/// empty one when it could not be allocated.
///
/// Failing to reserve it is reported and survived rather than fatal: an array
/// that serves but cannot rebuild is a great deal better than no arrays at
/// all, and on a machine this short of memory a restart would fare no better.
fn maintenance_scratch() -> Vec<u8> {
    let mut scratch = Vec::new();
    if scratch.try_reserve(MAINTENANCE_CHUNK_BYTES).is_err() {
        log_hex_event(
            RAID_MAINTENANCE_FAILED,
            Level::Error,
            "raid: no memory for the maintenance buffer; arrays serve but cannot self-heal",
            "bytes_hex",
            MAINTENANCE_CHUNK_BYTES as u64,
        );
        return scratch;
    }
    scratch.resize(MAINTENANCE_CHUNK_BYTES, 0);
    scratch
}

tairix_rt::entry!(main);
