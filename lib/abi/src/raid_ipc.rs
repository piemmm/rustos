//! The array-composition IPC protocol (`plans/FIX-IO.md` `IO6c`): the reserved
//! rendezvous the RAID composer binds, and the fixed-width offer a member
//! agent presents its device through.
//!
//! # Why an offer, and why it is a *long* call
//!
//! A RAID array is several block devices driven as one, so one process must
//! hold client authority over every member at once. A driver is spawned for
//! exactly one matched hardware-tree node and receives exactly that node's
//! resource grants, so the composer cannot reach a member by itself — the
//! member's own agent must **delegate** its device's transport to the composer
//! (`call_grant` for the block-service endpoint, `shm_grant` for the data
//! window). A [`MemberOffer`] is how the agent tells the composer that a
//! delegation has arrived and which device it names.
//!
//! The offer is answered only when the membership *ends*: the composer holds
//! the call outstanding for as long as it holds the member. One exchange then
//! carries the whole lifecycle with nothing polled — the agent parks on the
//! reply, and learns the composer has gone away because tearing its endpoint
//! down cancels the outstanding call and wakes every client of it.
//!
//! # What the composer trusts, and what the delegation costs
//!
//! Nothing in the offer beyond the identity of the delegated resources. The
//! array a device belongs to, the slot it occupies, and the generation it last
//! saw are **read from the device itself** through the shared on-disk metadata
//! definition — never taken from the offering agent, which is an ordinary
//! user-space process and could otherwise claim a slot in an array it has
//! nothing to do with. Neither id in the offer conveys authority: the kernel
//! minted the grants, and an id the composer holds no grant for simply fails
//! closed the moment it is used.
//!
//! Delegating necessarily precedes the verdict, because reading the metadata
//! that decides the verdict *is* an access to the device. That is proportionate
//! rather than speculative: a member node is emitted only for a device whose
//! own first block already probed as array metadata, so the composer is handed
//! a disk that discovery has already identified as a member — never an
//! unrelated volume.
//!
//! The rendezvous is a reserved endpoint id ([`is_reserved_endpoint`]), so
//! binding it demands `CAP_IPC_BIND_PRIVILEGED`. That gate is load-bearing
//! rather than cosmetic: an unprivileged squatter that claimed the id first
//! would be handed read/write authority over every array member on the machine
//! as each agent delegated to it in turn.
//!
//! [`is_reserved_endpoint`]: crate::ipc::is_reserved_endpoint

use crate::le::{put_u32, put_u64, read_u16, read_u32, read_u64};
use crate::Errno;

/// The compatible string on the hardware-tree node published for a device
/// recognised as an array member.
///
/// The contract between the volume manager, which recognises a member while
/// probing a device and emits the node, and the RAID driver, which binds it
/// and becomes that member's agent. It lives here because both ends must mean
/// the same string and neither driver may depend on the other.
///
/// Vendor- and bus-neutral by construction: what makes a device an array
/// member is the metadata in its own first block, never who made it or what it
/// hangs off. The node is a pointer for the composer to look, never a datum
/// for it to believe — which array, which slot, and which generation are read
/// back off the device itself.
pub const RAID_MEMBER_COMPATIBLE: &[u8] = b"tairix,raid-member";

/// The compatible string on the hardware-tree node the array composer
/// publishes for an assembled array, served as one logical block device.
///
/// The contract between the composer, which brings an array online and emits
/// the node carrying the array's own block-service endpoint and shared data
/// window, and the volume manager, which binds it and probes the array's
/// filesystems exactly as it would a leaf disk. It lives here because both
/// ends must mean the same string and neither driver may depend on the other.
///
/// Vendor- and bus-neutral by construction: an array is defined by its
/// members' on-disk metadata, never by who made the disks or what they hang
/// off. A consumer treats the array as an ordinary block device; that it is
/// composed of several members is the composer's business alone.
pub const RAID_ARRAY_COMPATIBLE: &[u8] = b"tairix,raid-array";

/// Reserved well-known call-endpoint id of the RAID array composer (`"RA"`
/// hex-spelled prefix, following [`crate::seat::SEATMGR_ENDPOINT`]'s
/// convention).
///
/// One endpoint serves every array: an offer names a device, and the composer
/// decides from that device's own superblock which array — if any — it belongs
/// to, so a further level or a further array is never a second rendezvous.
pub const RAID_REGISTRY_ENDPOINT: u64 = 0x5241_1001;

/// Magic number identifying an array-composition frame (`"RAI1"`
/// little-endian).
pub const RAID_OFFER_MAGIC: u32 = u32::from_le_bytes(*b"RAI1");

/// The `raid-v1` composition-protocol version.
pub const RAID_VERSION_V1: u16 = 1;

/// A member agent's offer of its device to the composer.
///
/// Fixed-width, so a decode never has to trust a length field and the
/// composer's receive buffer is a compile-time constant.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct MemberOffer {
    /// The device's block-service call endpoint, already delegated to
    /// [`RAID_REGISTRY_ENDPOINT`] with `call_grant`.
    pub endpoint: u64,
    /// The device's shared data window, already delegated with `shm_grant`.
    pub window: u64,
    /// The agent's own hardware-tree node, so the composer can name the member
    /// in its audit trail and resolve the fault domain it sits in. An
    /// identifier only: the kernel assigned it, and the composer reads the node
    /// from the tree rather than believing anything the offer says about it.
    pub node: u32,
}

impl MemberOffer {
    /// Encoded width, in bytes: magic, version, the two delegated resource
    /// ids, and the node id.
    pub const WIRE_LEN: usize = 4 + 2 + 8 + 8 + 4;

    /// Encode into `buf`, returning the bytes written.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] if `buf` is shorter than [`Self::WIRE_LEN`].
    pub fn encode(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        if buf.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        put_u32(buf, 0, RAID_OFFER_MAGIC);
        buf[4..6].copy_from_slice(&RAID_VERSION_V1.to_le_bytes());
        put_u64(buf, 6, self.endpoint);
        put_u64(buf, 14, self.window);
        put_u32(buf, 22, self.node);
        Ok(Self::WIRE_LEN)
    }

    /// Decode an offer frame, failing closed on anything unrecognised.
    ///
    /// # Errors
    ///
    /// * [`Errno::LengthOutOfRange`] — the frame is shorter than
    ///   [`Self::WIRE_LEN`].
    /// * [`Errno::BadMagic`] — the magic or the version is not this
    ///   protocol's.
    /// * [`Errno::NotFound`] — a resource id of zero, which names no endpoint
    ///   and no region, so the offer can only be malformed or a probe.
    pub fn decode(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::LengthOutOfRange);
        }
        if read_u32(bytes, 0) != RAID_OFFER_MAGIC || read_u16(bytes, 4) != RAID_VERSION_V1 {
            return Err(Errno::BadMagic);
        }
        let endpoint = read_u64(bytes, 6);
        let window = read_u64(bytes, 14);
        if endpoint == 0 || window == 0 {
            return Err(Errno::NotFound);
        }
        Ok(Self {
            endpoint,
            window,
            node: read_u32(bytes, 22),
        })
    }
}

/// Maximum request, in bytes, the [`RAID_REGISTRY_ENDPOINT`] accepts: exactly
/// one fixed-width [`MemberOffer`].
pub const RAID_MAX_REQUEST: usize = MemberOffer::WIRE_LEN;

/// How an outstanding membership offer ended.
///
/// The composer answers an offer only when the membership is over, so every
/// completed call is one of these. The frame on the wire is the shared status
/// reply ([`crate::reply`]); this is how an agent reads that status back as a
/// decision about what to do next.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MembershipEnd {
    /// The composer released the member cleanly — the array was torn down, or
    /// this device was removed from it. The device is free, and offering it
    /// again is right while it is still present.
    Released,
    /// The composer refused the offer: the device carries no array metadata it
    /// can compose, or metadata naming an array it cannot assemble the device
    /// into. That verdict came from reading the device itself, so re-offering
    /// the same unchanged device would only reach it again.
    Refused(Errno),
    /// The composer went away with the offer outstanding — its endpoint was
    /// torn down, which cancels the call. Nothing is known about the array;
    /// the agent re-offers once a composer is listening again.
    ComposerGone,
}

impl MembershipEnd {
    /// Read a completed membership call's outcome.
    ///
    /// `reply` is the status frame the composer sent, or [`None`] when the
    /// call was cancelled rather than answered — which is what a torn-down
    /// composer looks like to its client.
    ///
    /// A malformed reply reads as a refusal rather than a release: a composer
    /// whose frames cannot be decoded is not one to keep offering a disk to.
    #[must_use]
    pub fn from_reply(reply: Option<&[u8]>) -> Self {
        let Some(bytes) = reply else {
            return Self::ComposerGone;
        };
        match crate::reply::decode_status_reply(bytes) {
            Ok(()) => Self::Released,
            Err(errno) => Self::Refused(errno),
        }
    }

    /// Whether the agent should offer its device again.
    #[must_use]
    pub const fn should_reoffer(&self) -> bool {
        matches!(self, Self::Released | Self::ComposerGone)
    }
}

#[cfg(test)]
mod tests;
