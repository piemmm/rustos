//! The array-administration and array-reporting protocol (`plans/FIX-IO.md`
//! `IO6f`): the reserved control endpoint the RAID composer binds, the
//! operations an administrator drives it with, and the records that describe
//! an array and its member devices.
//!
//! # Why this is a second endpoint
//!
//! The composition rendezvous ([`crate::raid_ipc::RAID_REGISTRY_ENDPOINT`])
//! carries memberships, and a membership is a call the composer holds
//! outstanding for as long as it holds the device — for the life of the array.
//! Administration and status are the opposite: short, frequent, and posted by
//! anyone the system lets ask. Sharing one endpoint would let a flood of
//! status calls fill the queue a member agent needs to register through, and a
//! disk that cannot register is a disk missing from its array. They are
//! therefore separate endpoints with separate queues, so administrative
//! traffic can never cost the machine a member.
//!
//! # Authority
//!
//! The endpoint is bound unrestricted-sender so the System Information broker
//! can front the read operations for its own callers, and every operation
//! declares the capability the composer enforces against the caller's
//! kernel-attested origin before it touches anything
//! ([`RaidControlOp::required_capability`], the single definition both the
//! composer and its clients read):
//!
//! * **Reads** ([`RaidControlOp::ListArrays`], [`RaidControlOp::ListMembers`])
//!   require `CAP_SYSINFO_HW` — the same authority the hardware tree itself
//!   is read under, which is what this view is: which storage devices exist,
//!   how they are composed, and how healthy the composition is. Gating the
//!   composer's own read at the identical bar is what stops a caller
//!   side-stepping the System Information query by asking the composer
//!   directly.
//! * **Mutations** (create, stop, add, remove) require `CAP_STORAGE_ADMIN`.
//!   They overwrite disks and change what a mounted filesystem is made of.
//!
//! Neither identity nor authority is ever read from the frame: the composer
//! takes the caller from the kernel and the device identities from the
//! devices' own metadata. A node id in a request names *which* offered device
//! the caller means, and conveys nothing — a device the composer does not
//! already hold simply is not found.

use crate::le::{put_u16, put_u32, put_u64, read_u16, read_u32, read_u64};
use crate::raid::{ArrayHealth, MemberState, RaidLevel};
use crate::{CapabilityId, Errno};

/// Reserved well-known call-endpoint id of the RAID composer's control and
/// status surface (`"RA"` hex-spelled prefix, the sibling of
/// [`crate::raid_ipc::RAID_REGISTRY_ENDPOINT`]).
pub const RAID_CONTROL_ENDPOINT: u64 = 0x5241_1002;

/// Magic number identifying an array-control frame (`"RAC1"` little-endian).
pub const RAID_CONTROL_MAGIC: u32 = u32::from_le_bytes(*b"RAC1");

/// The `raid-control-v1` protocol version.
pub const RAID_CONTROL_VERSION_V1: u16 = 1;

/// Byte width of the header every control frame opens with: the magic, the
/// version, and the operation discriminant.
pub const RAID_CONTROL_HEADER_LEN: usize = 4 + 2 + 2;

/// Most records one paged read returns per call.
///
/// A validation bound on the reply frame, not a limit on how many arrays or
/// devices a machine may have: a caller walks a longer list by paging.
pub const RAID_LIST_LIMIT_MAX: u16 = 16;

/// Slot number reported for a device that occupies no array slot — an
/// unaffiliated candidate, or a member whose array is not assembled.
pub const RAID_SLOT_NONE: u16 = u16::MAX;

/// The most devices one create request may name.
///
/// A validation bound on the request frame, deliberately fixed: it is the
/// widest width any level has a *structural* ceiling at — triple parity's 255
/// data members plus its three syndrome chunks. Levels with no algebraic
/// ceiling (a mirror, a stripe) are bounded here only by the frame, which is
/// far beyond any real array; the composer separately enforces each level's
/// own floor and ceiling, so this bound never decides whether a width is
/// legal, only that the frame cannot be made arbitrarily large.
pub const RAID_CREATE_MAX_MEMBERS: usize = crate::raid::MAX_PARITY_DATA_MEMBERS as usize + 3;

/// What an administrator asked the composer to do.
///
/// Fixed vocabulary: a frame naming anything else is refused rather than
/// guessed at.
///
/// A create carries its whole membership inline, which makes that variant far
/// larger than the rest. Indirection is not available to fix that here: this
/// crate has no allocator, and the point of the type is to be the decoded
/// form of one request frame, held briefly on the serving loop's stack and
/// never stored in a collection — so the size difference costs a kilobyte of
/// one stack frame and nothing else.
#[allow(clippy::large_enum_variant)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RaidControlOp {
    /// Report the live arrays the composer serves.
    ListArrays {
        /// Index of the first array record to return.
        offset: u32,
        /// Most records to return (`1..=`[`RAID_LIST_LIMIT_MAX`]).
        limit: u16,
    },
    /// Report every device the composer holds: array members and the
    /// unaffiliated candidates a new array can be created over.
    ListMembers {
        /// Index of the first member record to return.
        offset: u32,
        /// Most records to return (`1..=`[`RAID_LIST_LIMIT_MAX`]).
        limit: u16,
    },
    /// Create an array over the named devices, writing each one's member
    /// metadata.
    ///
    /// Destructive: every named device's first blocks are overwritten. The
    /// composer refuses a device that is not an unaffiliated candidate, so a
    /// disk carrying a filesystem or another array's metadata is never
    /// consumed by a mistyped node id.
    Create {
        /// The level to compose.
        level: RaidLevel,
        /// The stripe unit, in logical blocks; zero for a level that does not
        /// stripe.
        chunk_blocks: u32,
        /// The hardware-tree node ids of the member devices, in slot order.
        members: MemberNodeList,
    },
    /// Stop a live array: retire its published node and release its members.
    ///
    /// Refused while any volume is still attached on the array, so an
    /// in-use filesystem is never turned into a surprise removal.
    Stop {
        /// The array to stop.
        array: ArrayUuidBytes,
    },
    /// Admit a device into an absent slot of a live array and rebuild it.
    Add {
        /// The array to admit the device into.
        array: ArrayUuidBytes,
        /// The hardware-tree node id of the candidate device to admit.
        node: u32,
    },
    /// Retire a faulted member from a live array, vacating its slot.
    Remove {
        /// The array to retire the member from.
        array: ArrayUuidBytes,
        /// The hardware-tree node id of the member device to retire.
        node: u32,
    },
}

/// An array's 128-bit identity as it travels on the control wire.
///
/// The bytes of the on-disk array UUID, carried opaquely: the control
/// protocol never interprets them, it only names an array the composer
/// already knows.
pub type ArrayUuidBytes = [u8; 16];

/// The member devices a create request names, in slot order.
///
/// Fixed-capacity and self-describing so a decode never trusts a length field
/// it has not bounded: `count` is validated against the buffer and against
/// [`RAID_CREATE_MAX_MEMBERS`] before any element is read.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct MemberNodeList {
    nodes: [u32; RAID_CREATE_MAX_MEMBERS],
    count: u16,
}

impl MemberNodeList {
    /// Build a list from `nodes`.
    ///
    /// # Errors
    ///
    /// * [`Errno::LengthOutOfRange`] — empty, or longer than
    ///   [`RAID_CREATE_MAX_MEMBERS`].
    /// * [`Errno::NotFound`] — a node id of zero, which names no discovered
    ///   device.
    /// * [`Errno::AlreadyExists`] — the same device named twice, which would
    ///   compose an array from one disk pretending to be two.
    pub fn new(nodes: &[u32]) -> Result<Self, Errno> {
        if nodes.is_empty() || nodes.len() > RAID_CREATE_MAX_MEMBERS {
            return Err(Errno::LengthOutOfRange);
        }
        let mut out = Self {
            nodes: [0; RAID_CREATE_MAX_MEMBERS],
            count: 0,
        };
        for (index, &node) in nodes.iter().enumerate() {
            if node == 0 {
                return Err(Errno::NotFound);
            }
            if nodes[..index].contains(&node) {
                return Err(Errno::AlreadyExists);
            }
            out.nodes[index] = node;
        }
        out.count = u16::try_from(nodes.len()).map_err(|_| Errno::LengthOutOfRange)?;
        Ok(out)
    }

    /// The named devices, in slot order.
    #[must_use]
    pub fn as_slice(&self) -> &[u32] {
        &self.nodes[..self.count as usize]
    }

    /// How many devices the list names.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.count as usize
    }

    /// Whether the list names no device. Never true for a decoded list, which
    /// refuses an empty membership.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }
}

/// Operation discriminants on the wire. A frame naming any other value is
/// refused.
mod op {
    pub const LIST_ARRAYS: u16 = 1;
    pub const LIST_MEMBERS: u16 = 2;
    pub const CREATE: u16 = 3;
    pub const STOP: u16 = 4;
    pub const ADD: u16 = 5;
    pub const REMOVE: u16 = 6;
}

/// Payload width of a paged list request: `offset(4) || limit(2) ||
/// reserved(2)`.
const LIST_PAYLOAD_LEN: usize = 8;

/// Payload width of a create request before its member array:
/// `level(1) || reserved(1) || chunk_blocks(4) || count(2)`.
const CREATE_FIXED_LEN: usize = 8;

/// Payload width of a request naming only an array: `uuid(16)`.
const ARRAY_PAYLOAD_LEN: usize = 16;

/// Payload width of a request naming an array and one of its devices:
/// `uuid(16) || node(4) || reserved(4)`.
const ARRAY_NODE_PAYLOAD_LEN: usize = 24;

/// Largest control request the endpoint accepts: the header and the widest
/// payload, which is a create naming every member the widest level admits.
pub const RAID_CONTROL_MAX_REQUEST: usize =
    RAID_CONTROL_HEADER_LEN + CREATE_FIXED_LEN + RAID_CREATE_MAX_MEMBERS * 4;

impl RaidControlOp {
    /// The capability the composer requires of the caller before it acts.
    ///
    /// The single definition of who may drive each operation: the composer
    /// enforces it against the caller's kernel-attested origin, and a client
    /// reads the same answer rather than keeping its own idea of the rules.
    #[must_use]
    pub const fn required_capability(&self) -> CapabilityId {
        match self {
            Self::ListArrays { .. } | Self::ListMembers { .. } => CapabilityId::SYSINFO_HW,
            Self::Create { .. } | Self::Stop { .. } | Self::Add { .. } | Self::Remove { .. } => {
                CapabilityId::STORAGE_ADMIN
            }
        }
    }

    /// Whether the operation changes the composition rather than reporting it.
    ///
    /// Drives the audit level a decision is recorded at: a mutation is always
    /// worth a record, a read is not.
    #[must_use]
    pub const fn is_mutation(&self) -> bool {
        !matches!(self, Self::ListArrays { .. } | Self::ListMembers { .. })
    }

    /// A short, stable name for the operation, for the audit trail.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::ListArrays { .. } => "list_arrays",
            Self::ListMembers { .. } => "list_members",
            Self::Create { .. } => "create",
            Self::Stop { .. } => "stop",
            Self::Add { .. } => "add",
            Self::Remove { .. } => "remove",
        }
    }

    /// Encode into `buf`, returning the bytes written.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] if `buf` cannot hold the frame.
    pub fn encode(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        let payload = match self {
            Self::ListArrays { .. } | Self::ListMembers { .. } => LIST_PAYLOAD_LEN,
            Self::Create { members, .. } => CREATE_FIXED_LEN + members.len() * 4,
            Self::Stop { .. } => ARRAY_PAYLOAD_LEN,
            Self::Add { .. } | Self::Remove { .. } => ARRAY_NODE_PAYLOAD_LEN,
        };
        let total = RAID_CONTROL_HEADER_LEN + payload;
        if buf.len() < total {
            return Err(Errno::BufferTooSmall);
        }
        buf[..total].fill(0);
        put_u32(buf, 0, RAID_CONTROL_MAGIC);
        put_u16(buf, 4, RAID_CONTROL_VERSION_V1);
        let body = &mut buf[RAID_CONTROL_HEADER_LEN..total];
        let code = match self {
            Self::ListArrays { offset, limit } => {
                put_u32(body, 0, *offset);
                put_u16(body, 4, *limit);
                op::LIST_ARRAYS
            }
            Self::ListMembers { offset, limit } => {
                put_u32(body, 0, *offset);
                put_u16(body, 4, *limit);
                op::LIST_MEMBERS
            }
            Self::Create {
                level,
                chunk_blocks,
                members,
            } => {
                body[0] = level.as_u8();
                put_u32(body, 2, *chunk_blocks);
                put_u16(body, 6, members.count);
                for (index, node) in members.as_slice().iter().enumerate() {
                    put_u32(body, CREATE_FIXED_LEN + index * 4, *node);
                }
                op::CREATE
            }
            Self::Stop { array } => {
                body[..16].copy_from_slice(array);
                op::STOP
            }
            Self::Add { array, node } => {
                body[..16].copy_from_slice(array);
                put_u32(body, 16, *node);
                op::ADD
            }
            Self::Remove { array, node } => {
                body[..16].copy_from_slice(array);
                put_u32(body, 16, *node);
                op::REMOVE
            }
        };
        put_u16(buf, 6, code);
        Ok(total)
    }

    /// Decode a control frame, failing closed on anything unrecognised.
    ///
    /// The frame length must be exactly what the operation implies, so a
    /// caller cannot append bytes a future reader might interpret, and a
    /// reserved field must be zero.
    ///
    /// # Errors
    ///
    /// * [`Errno::LengthOutOfRange`] — a frame that is not exactly the
    ///   operation's width, or a member count outside
    ///   `1..=`[`RAID_CREATE_MAX_MEMBERS`].
    /// * [`Errno::BadMagic`] — a foreign magic or version, or a dirty
    ///   reserved field.
    /// * [`Errno::NotImplemented`] — an operation this protocol does not
    ///   define.
    /// * [`Errno::OutOfRange`] — an unknown RAID level, or a paging limit
    ///   outside `1..=`[`RAID_LIST_LIMIT_MAX`].
    /// * [`Errno::NotFound`] — a zero node id or a zero array identity,
    ///   which name nothing.
    /// * [`Errno::AlreadyExists`] — the same device named twice in a create.
    pub fn decode(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < RAID_CONTROL_HEADER_LEN {
            return Err(Errno::LengthOutOfRange);
        }
        if read_u32(bytes, 0) != RAID_CONTROL_MAGIC || read_u16(bytes, 4) != RAID_CONTROL_VERSION_V1
        {
            return Err(Errno::BadMagic);
        }
        let code = read_u16(bytes, 6);
        let body = &bytes[RAID_CONTROL_HEADER_LEN..];
        match code {
            op::LIST_ARRAYS | op::LIST_MEMBERS => {
                let (offset, limit) = decode_list_payload(body)?;
                Ok(if code == op::LIST_ARRAYS {
                    Self::ListArrays { offset, limit }
                } else {
                    Self::ListMembers { offset, limit }
                })
            }
            op::CREATE => decode_create(body),
            op::STOP => {
                let array = decode_array(body, ARRAY_PAYLOAD_LEN)?;
                Ok(Self::Stop { array })
            }
            op::ADD | op::REMOVE => {
                let array = decode_array(body, ARRAY_NODE_PAYLOAD_LEN)?;
                if read_u32(body, 20) != 0 {
                    return Err(Errno::BadMagic);
                }
                let node = read_u32(body, 16);
                if node == 0 {
                    return Err(Errno::NotFound);
                }
                Ok(if code == op::ADD {
                    Self::Add { array, node }
                } else {
                    Self::Remove { array, node }
                })
            }
            _ => Err(Errno::NotImplemented),
        }
    }
}

/// Decode the shared `offset || limit || reserved` paging payload.
fn decode_list_payload(body: &[u8]) -> Result<(u32, u16), Errno> {
    if body.len() != LIST_PAYLOAD_LEN {
        return Err(Errno::LengthOutOfRange);
    }
    if read_u16(body, 6) != 0 {
        return Err(Errno::BadMagic);
    }
    let limit = read_u16(body, 4);
    if limit == 0 || limit > RAID_LIST_LIMIT_MAX {
        return Err(Errno::OutOfRange);
    }
    Ok((read_u32(body, 0), limit))
}

/// Decode a payload that opens with an array identity, checking its width.
fn decode_array(body: &[u8], expected: usize) -> Result<ArrayUuidBytes, Errno> {
    if body.len() != expected {
        return Err(Errno::LengthOutOfRange);
    }
    let mut array = [0u8; 16];
    array.copy_from_slice(&body[..16]);
    if array.iter().all(|&byte| byte == 0) {
        return Err(Errno::NotFound);
    }
    Ok(array)
}

/// Decode a create payload and its member list.
fn decode_create(body: &[u8]) -> Result<RaidControlOp, Errno> {
    if body.len() < CREATE_FIXED_LEN {
        return Err(Errno::LengthOutOfRange);
    }
    if body[1] != 0 {
        return Err(Errno::BadMagic);
    }
    let level = RaidLevel::from_u8(body[0])?;
    let count = usize::from(read_u16(body, 6));
    if count == 0 || count > RAID_CREATE_MAX_MEMBERS {
        return Err(Errno::LengthOutOfRange);
    }
    if body.len() != CREATE_FIXED_LEN + count * 4 {
        return Err(Errno::LengthOutOfRange);
    }
    let mut nodes = [0u32; RAID_CREATE_MAX_MEMBERS];
    for (index, node) in nodes[..count].iter_mut().enumerate() {
        *node = read_u32(body, CREATE_FIXED_LEN + index * 4);
    }
    Ok(RaidControlOp::Create {
        level,
        chunk_blocks: read_u32(body, 2),
        members: MemberNodeList::new(&nodes[..count])?,
    })
}

/// Byte width of the reply a successful create returns: the shared status
/// word and the identity the composer minted for the new array.
pub const RAID_CREATE_REPLY_LEN: usize = crate::reply::STATUS_REPLY_LEN + 16;

/// Encode a create outcome: the status word, and on success the identity the
/// composer minted.
///
/// The identity is minted by the composer rather than named by the caller so
/// a request can never collide with a live array's identity, which would make
/// two different arrays indistinguishable to reassembly.
#[must_use]
pub fn encode_create_reply(result: Result<ArrayUuidBytes, Errno>) -> [u8; RAID_CREATE_REPLY_LEN] {
    let mut out = [0u8; RAID_CREATE_REPLY_LEN];
    match result {
        Ok(array) => {
            out[..crate::reply::STATUS_REPLY_LEN]
                .copy_from_slice(&crate::reply::encode_status_reply(Ok(())));
            out[crate::reply::STATUS_REPLY_LEN..].copy_from_slice(&array);
        }
        Err(errno) => {
            out[..crate::reply::STATUS_REPLY_LEN]
                .copy_from_slice(&crate::reply::encode_status_reply(Err(errno)));
        }
    }
    out
}

/// Decode a create reply.
///
/// # Errors
///
/// * [`Errno::BufferTooSmall`] — the frame cannot hold the reply.
/// * [`Errno::NotFound`] — a success carrying a zero identity, which names
///   no array; a composer whose reply says nothing was created is not
///   believed.
/// * The refusal the composer reported.
pub fn decode_create_reply(bytes: &[u8]) -> Result<ArrayUuidBytes, Errno> {
    if bytes.len() < RAID_CREATE_REPLY_LEN {
        return Err(Errno::BufferTooSmall);
    }
    crate::reply::decode_status_reply(&bytes[..crate::reply::STATUS_REPLY_LEN])?;
    let mut array = [0u8; 16];
    array.copy_from_slice(&bytes[crate::reply::STATUS_REPLY_LEN..RAID_CREATE_REPLY_LEN]);
    if array.iter().all(|&byte| byte == 0) {
        return Err(Errno::NotFound);
    }
    Ok(array)
}

/// What a device the composer holds is doing.
///
/// A superset of the composition engines' own [`MemberState`]: it also names
/// the two dispositions a device can be in *before* it is part of a live
/// array, which an engine never sees because such a device is not in one.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub enum RaidMemberDisposition {
    /// An unaffiliated blank device: it carries no array metadata, so it
    /// belongs to no array and is eligible to be created into one or admitted
    /// to an existing one.
    #[default]
    Candidate = 0,
    /// The device carries metadata for an array that is not assembled — it is
    /// waiting for the rest of its members, or for an array that can never be
    /// composed from what is present.
    Held = 1,
    /// A current member of a live array: a read source and a write target.
    InSync = 2,
    /// A member of a live array being rebuilt back into it. Not yet a read
    /// source.
    Resyncing = 3,
    /// A member the array dropped after a whole-device fault. It stays known
    /// so it can be retired deliberately, and so a genuine return recovers it.
    Faulted = 4,
}

impl RaidMemberDisposition {
    /// Raw on-wire discriminant.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Decode a discriminant, failing closed on an unknown value.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for any value outside the closed set.
    pub const fn from_u8(raw: u8) -> Result<Self, Errno> {
        match raw {
            0 => Ok(Self::Candidate),
            1 => Ok(Self::Held),
            2 => Ok(Self::InSync),
            3 => Ok(Self::Resyncing),
            4 => Ok(Self::Faulted),
            _ => Err(Errno::OutOfRange),
        }
    }

    /// How a live array's member state is reported.
    ///
    /// [`None`] for [`MemberState::Absent`]: an absent slot holds no device,
    /// so there is nothing to report a disposition for. The one mapping
    /// between the engines' vocabulary and the reported one, so a member can
    /// never be described differently by two readers.
    #[must_use]
    pub const fn for_member_state(state: MemberState) -> Option<Self> {
        match state {
            MemberState::InSync => Some(Self::InSync),
            MemberState::Resyncing => Some(Self::Resyncing),
            MemberState::Faulted => Some(Self::Faulted),
            MemberState::Absent => None,
        }
    }

    /// Whether a device in this disposition may be created into a new array
    /// or admitted to an existing one.
    ///
    /// Only an unaffiliated candidate may: everything else either holds data
    /// an array depends on or carries metadata that would be silently
    /// destroyed.
    #[must_use]
    pub const fn is_available(self) -> bool {
        matches!(self, Self::Candidate)
    }
}

/// Flag bit reported by [`RaidArrayRecord::scrubbing`] — set while a
/// verification pass is running.
pub const RAID_ARRAY_FLAG_SCRUBBING: u8 = 1 << 0;

/// Flag bit reported by [`RaidArrayRecord::resyncing`] — set while a member is
/// being rebuilt.
pub const RAID_ARRAY_FLAG_RESYNCING: u8 = 1 << 1;

/// Every flag bit this version defines; any other bit is reserved and a
/// record carrying one is refused.
const RAID_ARRAY_FLAGS_KNOWN: u8 = RAID_ARRAY_FLAG_SCRUBBING | RAID_ARRAY_FLAG_RESYNCING;

/// One live array the composer serves.
///
/// Everything here is read from the composer's own live state — the identity
/// and shape the array's metadata defines, the health its engine reports, and
/// the cursors its maintenance has reached. It names no secret and no
/// capability token; the endpoint and node ids identify the array's published
/// block service, and holding one conveys no authority over it.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct RaidArrayRecord {
    /// The array's 128-bit identity.
    array: ArrayUuidBytes,
    /// The level it is composed at.
    level: RaidLevel,
    /// Its current health.
    health: ArrayHealth,
    /// [`RAID_ARRAY_FLAG_SCRUBBING`] and [`RAID_ARRAY_FLAG_RESYNCING`], read
    /// through [`scrubbing`](Self::scrubbing) and
    /// [`resyncing`](Self::resyncing).
    flags: u8,
    /// Slots the array is defined to have, including absent ones.
    member_count: u16,
    /// Slots currently holding a fully in-sync device.
    active_members: u16,
    /// Logical block size of the composed device.
    block_size: u32,
    /// The stripe unit in logical blocks; zero for a level that does not
    /// stripe.
    chunk_blocks: u32,
    /// Logical blocks the composed device presents.
    block_count: u64,
    /// The block-service call endpoint the array is served on.
    endpoint: u64,
    /// The hardware-tree node the array is published as.
    node: u32,
    /// How far a running verification pass has reached, in logical blocks;
    /// the array's block count when no pass is running.
    scrub_cursor: u64,
    /// How far a running rebuild has reached, in logical blocks; the array's
    /// block count when no rebuild is running.
    resync_cursor: u64,
    /// The array's current metadata generation.
    generation: u64,
}

impl RaidArrayRecord {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = 80;

    /// Build a record from its parts.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        array: ArrayUuidBytes,
        level: RaidLevel,
        health: ArrayHealth,
        flags: u8,
        member_count: u16,
        active_members: u16,
        block_size: u32,
        chunk_blocks: u32,
        block_count: u64,
        endpoint: u64,
        node: u32,
        scrub_cursor: u64,
        resync_cursor: u64,
        generation: u64,
    ) -> Self {
        Self {
            array,
            level,
            health,
            flags,
            member_count,
            active_members,
            block_size,
            chunk_blocks,
            block_count,
            endpoint,
            node,
            scrub_cursor,
            resync_cursor,
            generation,
        }
    }

    /// The array's 128-bit identity.
    #[must_use]
    pub const fn array(&self) -> ArrayUuidBytes {
        self.array
    }

    /// The level the array is composed at.
    #[must_use]
    pub const fn level(&self) -> RaidLevel {
        self.level
    }

    /// The array's current health.
    #[must_use]
    pub const fn health(&self) -> ArrayHealth {
        self.health
    }

    /// Whether a verification pass is running.
    #[must_use]
    pub const fn scrubbing(&self) -> bool {
        self.flags & RAID_ARRAY_FLAG_SCRUBBING != 0
    }

    /// Whether a member is being rebuilt.
    #[must_use]
    pub const fn resyncing(&self) -> bool {
        self.flags & RAID_ARRAY_FLAG_RESYNCING != 0
    }

    /// Slots the array is defined to have, including absent ones.
    #[must_use]
    pub const fn member_count(&self) -> u16 {
        self.member_count
    }

    /// Slots currently holding a fully in-sync device.
    #[must_use]
    pub const fn active_members(&self) -> u16 {
        self.active_members
    }

    /// Logical block size of the composed device.
    #[must_use]
    pub const fn block_size(&self) -> u32 {
        self.block_size
    }

    /// The stripe unit in logical blocks; zero when the level does not stripe.
    #[must_use]
    pub const fn chunk_blocks(&self) -> u32 {
        self.chunk_blocks
    }

    /// Logical blocks the composed device presents.
    #[must_use]
    pub const fn block_count(&self) -> u64 {
        self.block_count
    }

    /// The block-service endpoint the array is served on.
    #[must_use]
    pub const fn endpoint(&self) -> u64 {
        self.endpoint
    }

    /// The hardware-tree node the array is published as.
    #[must_use]
    pub const fn node(&self) -> u32 {
        self.node
    }

    /// How far a running verification pass has reached, in logical blocks.
    #[must_use]
    pub const fn scrub_cursor(&self) -> u64 {
        self.scrub_cursor
    }

    /// How far a running rebuild has reached, in logical blocks.
    #[must_use]
    pub const fn resync_cursor(&self) -> u64 {
        self.resync_cursor
    }

    /// The array's current metadata generation.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        out[0..16].copy_from_slice(&self.array);
        out[16] = self.level.as_u8();
        out[17] = self.health.as_u8();
        out[18] = self.flags;
        // out[19] is reserved padding, left zero.
        put_u16(&mut out, 20, self.member_count);
        put_u16(&mut out, 22, self.active_members);
        put_u32(&mut out, 24, self.block_size);
        put_u32(&mut out, 28, self.chunk_blocks);
        put_u64(&mut out, 32, self.block_count);
        put_u64(&mut out, 40, self.endpoint);
        put_u32(&mut out, 48, self.node);
        // out[52..56] is reserved padding, left zero.
        put_u64(&mut out, 56, self.scrub_cursor);
        put_u64(&mut out, 64, self.resync_cursor);
        put_u64(&mut out, 72, self.generation);
        out
    }

    /// Decode from `bytes`, validating every discriminant and reserved field.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — shorter than [`Self::WIRE_LEN`].
    /// * [`Errno::OutOfRange`] — an unknown level or health discriminant.
    /// * [`Errno::BadMagic`] — a reserved flag bit or padding byte that is
    ///   not zero, which is an unknown record shape rather than one to guess
    ///   at.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let flags = bytes[18];
        if flags & !RAID_ARRAY_FLAGS_KNOWN != 0
            || bytes[19] != 0
            || bytes[52..56].iter().any(|&byte| byte != 0)
        {
            return Err(Errno::BadMagic);
        }
        let mut array = [0u8; 16];
        array.copy_from_slice(&bytes[0..16]);
        Ok(Self {
            array,
            level: RaidLevel::from_u8(bytes[16])?,
            health: ArrayHealth::from_u8(bytes[17])?,
            flags,
            member_count: read_u16(bytes, 20),
            active_members: read_u16(bytes, 22),
            block_size: read_u32(bytes, 24),
            chunk_blocks: read_u32(bytes, 28),
            block_count: read_u64(bytes, 32),
            endpoint: read_u64(bytes, 40),
            node: read_u32(bytes, 48),
            scrub_cursor: read_u64(bytes, 56),
            resync_cursor: read_u64(bytes, 64),
            generation: read_u64(bytes, 72),
        })
    }
}

/// One device the composer holds: a member of an array, or an unaffiliated
/// candidate a new array can be created over.
///
/// The node id is how an administrator names the device in a request — it is
/// the identity discovery gave the disk, and it conveys no authority over it.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct RaidMemberRecord {
    /// The array the device belongs to, or all zero when it belongs to none.
    array: ArrayUuidBytes,
    /// What the device is doing.
    disposition: RaidMemberDisposition,
    /// The array slot it occupies, or [`RAID_SLOT_NONE`].
    slot: u16,
    /// The hardware-tree node the device was offered under.
    node: u32,
    /// The device's block-service call endpoint.
    endpoint: u64,
    /// Logical blocks on the device.
    block_count: u64,
    /// Logical block size of the device.
    block_size: u32,
    /// The metadata generation the device's own superblock carries; zero when
    /// it carries none.
    generation: u64,
}

impl RaidMemberRecord {
    /// Encoded size on the wire.
    pub const WIRE_LEN: usize = 56;

    /// Build a record from its parts.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        array: ArrayUuidBytes,
        disposition: RaidMemberDisposition,
        slot: u16,
        node: u32,
        endpoint: u64,
        block_count: u64,
        block_size: u32,
        generation: u64,
    ) -> Self {
        Self {
            array,
            disposition,
            slot,
            node,
            endpoint,
            block_count,
            block_size,
            generation,
        }
    }

    /// The array the device belongs to, or all zero when it belongs to none.
    #[must_use]
    pub const fn array(&self) -> ArrayUuidBytes {
        self.array
    }

    /// Whether the device belongs to no array, and so may be composed into
    /// one.
    #[must_use]
    pub fn is_unaffiliated(&self) -> bool {
        self.array.iter().all(|&byte| byte == 0)
    }

    /// What the device is doing.
    #[must_use]
    pub const fn disposition(&self) -> RaidMemberDisposition {
        self.disposition
    }

    /// The array slot the device occupies, or [`RAID_SLOT_NONE`].
    #[must_use]
    pub const fn slot(&self) -> u16 {
        self.slot
    }

    /// The hardware-tree node the device was offered under.
    #[must_use]
    pub const fn node(&self) -> u32 {
        self.node
    }

    /// The device's block-service call endpoint.
    #[must_use]
    pub const fn endpoint(&self) -> u64 {
        self.endpoint
    }

    /// Logical blocks on the device.
    #[must_use]
    pub const fn block_count(&self) -> u64 {
        self.block_count
    }

    /// Logical block size of the device.
    #[must_use]
    pub const fn block_size(&self) -> u32 {
        self.block_size
    }

    /// The metadata generation the device's own superblock carries.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        out[0..16].copy_from_slice(&self.array);
        out[16] = self.disposition.as_u8();
        // out[17] is reserved padding, left zero.
        put_u16(&mut out, 18, self.slot);
        put_u32(&mut out, 20, self.node);
        put_u64(&mut out, 24, self.endpoint);
        put_u64(&mut out, 32, self.block_count);
        put_u32(&mut out, 40, self.block_size);
        // out[44..48] is reserved padding, left zero.
        put_u64(&mut out, 48, self.generation);
        out
    }

    /// Decode from `bytes`, validating the disposition and the reserved
    /// padding.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — shorter than [`Self::WIRE_LEN`].
    /// * [`Errno::OutOfRange`] — an unknown disposition discriminant.
    /// * [`Errno::BadMagic`] — a non-zero reserved byte.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if bytes[17] != 0 || bytes[44..48].iter().any(|&byte| byte != 0) {
            return Err(Errno::BadMagic);
        }
        let mut array = [0u8; 16];
        array.copy_from_slice(&bytes[0..16]);
        Ok(Self {
            array,
            disposition: RaidMemberDisposition::from_u8(bytes[16])?,
            slot: read_u16(bytes, 18),
            node: read_u32(bytes, 20),
            endpoint: read_u64(bytes, 24),
            block_count: read_u64(bytes, 32),
            block_size: read_u32(bytes, 40),
            generation: read_u64(bytes, 48),
        })
    }
}

/// Largest reply the control endpoint emits: the status word, the page
/// header, and a full page of the widest record.
pub const RAID_CONTROL_MAX_REPLY: usize = crate::reply::STATUS_REPLY_LEN
    + crate::reply::PAGE_HEADER_LEN
    + RAID_LIST_LIMIT_MAX as usize * RaidArrayRecord::WIRE_LEN;

#[cfg(test)]
mod tests;
