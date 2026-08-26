//! The cross-process device-channel handoff between the user-space
//! network stack and a link-layer network driver process
//! (`plans/NETWORK.md` §2.3, N4c).
//!
//! [`net_ring`](super::net_ring) defines the *in-region* frame transport; this
//! module defines the *IPC control plane* that establishes and drives that
//! region when the driver and the stack are **separate processes** (the true
//! microkernel shape): the driver owns the device (MMIO/DMA/IRQ) and serves a
//! call endpoint; the stack is the client that owns the shared frame-ring
//! region.
//!
//! # Roles (the display D7a `shm_grant` pattern, inverted)
//!
//! The display service is the endpoint *server* and the session is the
//! *client* that `shm_create`s a frame region and `shm_grant`s it to the
//! service's endpoint. A NIC works the same way with the data flowing both
//! ways: the **driver** serves its device endpoint and the **stack** is the
//! client. The stack sizes a [`RingGeometry`]
//! from the device MTU, `shm_create`s the region, `shm_grant`s it to the
//! driver's endpoint, and forwards the unforgeable grant handle in the
//! [`NetChannelRequest::Attach`] message; the driver `shm_map`s exactly that
//! one region (`SHM_MAP` is owner-checked, so the handle is useless to a
//! bystander — no ambient authority).
//!
//! # Operations
//!
//! * [`NetChannelRequest::Facts`] — the stack asks for the device's
//!   [`DeviceFacts`] so it can size the ring geometry before attaching.
//! * [`NetChannelRequest::Attach`] — the stack hands over the granted
//!   region, the agreed geometry, the traffic [`BufferClass`], and the
//!   port name the driver notifies when receive frames arrive.
//! * [`NetChannelRequest::Service`] — the doorbell: the driver services the
//!   mapped rings once (drains TX into the device, delivers device frames
//!   into RX) and replies a [`ServiceReport`].
//! * [`NetChannelRequest::Detach`] — the stack releases the channel; the
//!   driver unmaps the region and forgets the notify port.
//!
//! Between doorbells the driver parks on its device IRQ; when frames arrive it
//! wakes the stack with a [`NetChannelNotify`] `ipc_send` to the attach port,
//! and the stack — parked on that port in its wait set — issues the next
//! [`NetChannelRequest::Service`]. Neither side ever busy-polls.
//!
//! # Fail closed
//!
//! Every decode is total and validates whole: an unknown magic, version,
//! operation byte, a dirty reserved field, an out-of-range geometry, or an
//! over-length frame refuses with one typed [`Errno`] rather than guessing.

use super::net::{
    DeviceFacts, LinkState, MacAddress, McastFilter, NetOffloads, MAC_ADDRESS_LEN, MAX_MCAST_GROUPS,
};
use super::net_ring::{RingGeometry, ServiceReport};
use super::BufferClass;
use crate::le::{put_u16, put_u32, put_u64, read_u16, read_u32, read_u64};
use crate::Errno;

/// Magic number identifying a device-channel request (`"NCHR"`).
pub const NET_CHANNEL_REQUEST_MAGIC: u32 = u32::from_le_bytes(*b"NCHR");

/// Magic number identifying a receive-frames notify (`"NCHN"`).
pub const NET_CHANNEL_NOTIFY_MAGIC: u32 = u32::from_le_bytes(*b"NCHN");

/// The `netchan-v1` protocol version.
pub const NET_CHANNEL_VERSION_V1: u16 = 1;

/// Base of the reserved device-channel call-endpoint id block (`"NCHAN\0\0\0"`
/// little-endian). Each NIC driver process claims the first free id in
/// `NET_CHANNEL_ENDPOINT_BASE .. NET_CHANNEL_ENDPOINT_BASE + NET_CHANNEL_ENDPOINT_COUNT`
/// by binding it (the `drivers/bus/usb/xhci` block-claim precedent), so two NIC
/// drivers never collide on an id without a central allocator.
///
/// The block is a reserved rendezvous
/// ([`crate::ipc::is_reserved_endpoint`]): binding any id in it requires
/// [`CapabilityId::IPC_BIND_PRIVILEGED`](crate::CapabilityId::IPC_BIND_PRIVILEGED),
/// so an unprivileged squatter cannot bind one first and impersonate the
/// driver to the stack. The driver additionally binds it **restricted
/// sender**, requiring the caller to hold
/// [`CapabilityId::NET_RAW`](crate::CapabilityId::NET_RAW) (driving a NIC's
/// raw frame rings *is* raw network access), so the kernel refuses at
/// dispatch every caller but the network stack — the receiver never
/// re-checks.
pub const NET_CHANNEL_ENDPOINT_BASE: u64 = u64::from_le_bytes(*b"NCHAN\0\0\0");

/// Number of concurrently-bindable device-channel endpoint ids: the most
/// NIC driver processes the stack serves at once. A fixed validation bound
/// on the reserved block, not an interface-count capacity.
pub const NET_CHANNEL_ENDPOINT_COUNT: u64 = 16;

/// Device-tree-style `compatible` model name of the hardware-tree node a NIC
/// driver publishes to advertise the device-channel endpoint it claimed.
///
/// The discovery half of this contract, and its single definition: a driver
/// process stamps this key on the child node it emits, and the device manager
/// recognises a node carrying it as a bound NIC's frame channel (rather than
/// a device still awaiting a driver) and hands its endpoint to the network
/// stack. Defined beside the endpoint block so the key emitted and the key
/// looked for can never drift.
pub const NETCHAN_NODE_COMPATIBLE: &[u8] = b"tairix,netchan";

/// Whether `id` is one of the reserved device-channel endpoint ids.
#[must_use]
pub const fn is_net_channel_endpoint(id: u64) -> bool {
    id >= NET_CHANNEL_ENDPOINT_BASE && id < NET_CHANNEL_ENDPOINT_BASE + NET_CHANNEL_ENDPOINT_COUNT
}

/// High tag of a stack-owned device-channel notify-port id (see
/// [`notify_endpoint_for`]).
const NET_NOTIFY_ENDPOINT_TAG: u64 = 0x4E4E_0000_0000_0000;

/// The notify-mailbox endpoint id the network stack binds for one managed
/// channel-backed interface and passes to the driver in
/// [`AttachParams::notify_endpoint`].
///
/// It packs the stack's own kernel task id `pid` (never reused while the
/// stack lives) and the per-interface slot `index` under a fixed high tag,
/// mirroring the window client's `event_endpoint_for`: a distinct,
/// collision-free, **non-reserved** id, so the stack `port_bind`s it
/// without [`CapabilityId::IPC_BIND_PRIVILEGED`](crate::CapabilityId::IPC_BIND_PRIVILEGED)
/// and two interfaces (or two stacks) can never disagree about the id
/// space. The mailbox is owner-only to receive, so the driver only needs
/// the number — it cannot receive the wakes it sends, and a bystander
/// cannot steal them; a spurious notify at worst costs one extra
/// [`NetChannelRequest::Service`] doorbell.
///
/// `index` is bounded by [`NET_CHANNEL_ENDPOINT_COUNT`] (the most channels
/// the stack serves at once) and occupies the low byte; `pid` occupies the
/// next 48 bits, well within a task id's range.
#[must_use]
pub const fn notify_endpoint_for(pid: u64, index: u64) -> u64 {
    NET_NOTIFY_ENDPOINT_TAG | ((pid & 0xFFFF_FFFF_FFFF) << 8) | (index & 0xFF)
}

/// Operation discriminants (the request's fifth byte).
mod op {
    pub const FACTS: u8 = 1;
    pub const ATTACH: u8 = 2;
    pub const SERVICE: u8 = 3;
    pub const DETACH: u8 = 4;
    pub const SET_MULTICAST: u8 = 5;
    pub const SET_RX_FILTER: u8 = 6;
}

/// Fixed request header: magic (4) + version (2) + op (1) + reserved (1).
const HEADER_LEN: usize = 8;

/// Wire length of the [`NetChannelRequest::Attach`] body that follows the
/// header: geometry `slots` (4) + `rx_slot_capacity` (4) +
/// `tx_slot_capacity` (4) + grant handle (8) + class (1) + `rx_queues`
/// (2) + reserved (1) + notify endpoint id (8).
const ATTACH_BODY_LEN: usize = 4 + 4 + 4 + 8 + 1 + 2 + 1 + 8;

/// Wire length of the [`NetChannelRequest::SetMulticast`] body: the group
/// count (1) + reserved (1) + [`MAX_MCAST_GROUPS`] fixed-width addresses.
///
/// The body is fixed-width rather than count-sized so the frame length is a
/// constant both sides validate against, and a short frame is a refusal
/// rather than a partially-read set.
const SET_MULTICAST_BODY_LEN: usize = 1 + 1 + MAX_MCAST_GROUPS * MAC_ADDRESS_LEN;

/// Local IPv4 addresses a [`RxFilterPolicy`] carries.
///
/// A fixed containment bound, not a capacity: an interface with more
/// addresses than this marks the policy non-exhaustive and the filter then
/// admits all unicast, so the bound can never cost a frame.
pub const MAX_FILTER_V4: usize = 8;

/// Local IPv6 addresses a [`RxFilterPolicy`] carries. An interface normally
/// has a link-local, a global, and a temporary address per prefix.
pub const MAX_FILTER_V6: usize = 8;

/// Joined IPv4 group addresses a [`RxFilterPolicy`] carries.
///
/// A fixed containment bound like [`MAX_FILTER_V4`], and deliberately its own
/// constant: that one bounds the addresses the interface *answers for*, this
/// one the groups it has *joined*. The two coincide today but are not the
/// same quantity, so aliasing them would couple two independent choices.
pub const MAX_FILTER_GROUPS_V4: usize = 8;

/// Joined IPv6 group addresses a [`RxFilterPolicy`] carries. The all-nodes
/// and solicited-node groups are not among them: both are derived from
/// addresses the policy already names.
pub const MAX_FILTER_GROUPS_V6: usize = 8;

/// Wire length of the [`NetChannelRequest::SetRxFilter`] body: the four
/// counts (1 each) + the exhaustive flag (1) + reserved (3) + each family's
/// fixed-width address array, IPv4 twice (the address and its subnet's
/// directed-broadcast address), then each family's joined-group array.
const SET_RX_FILTER_BODY_LEN: usize = 4
    + 1
    + 3
    + MAX_FILTER_V4 * 4 * 2
    + MAX_FILTER_V6 * 16
    + MAX_FILTER_GROUPS_V4 * 4
    + MAX_FILTER_GROUPS_V6 * 16;

/// Byte offset of the IPv6 address array within the `SetRxFilter` body.
const RX_FILTER_V6_AT: usize = 8 + MAX_FILTER_V4 * 8;
/// Byte offset of the IPv4 joined-group array within the body.
const RX_FILTER_GROUPS_V4_AT: usize = RX_FILTER_V6_AT + MAX_FILTER_V6 * 16;
/// Byte offset of the IPv6 joined-group array within the body.
const RX_FILTER_GROUPS_V6_AT: usize = RX_FILTER_GROUPS_V4_AT + MAX_FILTER_GROUPS_V4 * 4;

/// Largest device-channel request frame: the header plus the largest body. A
/// fixed validation bound sizing the buffer both sides pin for the control
/// endpoint.
pub const NET_CHANNEL_MAX_REQUEST: usize = HEADER_LEN + largest_body();

/// The largest request body, so [`NET_CHANNEL_MAX_REQUEST`] tracks whichever
/// operation grows rather than needing a hand-updated comparison.
const fn largest_body() -> usize {
    let mut largest = ATTACH_BODY_LEN;
    if SET_MULTICAST_BODY_LEN > largest {
        largest = SET_MULTICAST_BODY_LEN;
    }
    if SET_RX_FILTER_BODY_LEN > largest {
        largest = SET_RX_FILTER_BODY_LEN;
    }
    largest
}

/// Wire length of a [`NetChannelNotify`] frame: magic (4) + version (2) +
/// link (1) + back-pressure (1) + cumulative filtered count (8).
pub const NET_CHANNEL_NOTIFY_LEN: usize = 16;

/// A device-channel control request the stack issues to the driver's
/// endpoint (`plans/NETWORK.md` N4c). Decoded fail-closed from an untrusted
/// frame; the driver acts only on a fully-validated value.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum NetChannelRequest {
    /// Report the device's [`DeviceFacts`] so the stack can size the ring
    /// geometry before attaching.
    Facts,
    /// Hand over the granted frame-ring region and start frame flow.
    Attach(AttachParams),
    /// The doorbell: service the mapped rings once and report what moved.
    Service,
    /// Release the channel: unmap the region and forget the notify port.
    Detach,
    /// Replace the set of group (multicast) addresses the device admits.
    SetMulticast(McastGroups),
    /// Replace the local addresses the driver's receive pre-filter matches
    /// against, so a frame with no possible local consumer is dropped
    /// without waking the stack.
    SetRxFilter(RxFilterPolicy),
}

/// The local addresses a driver's receive pre-filter matches a frame's
/// destination against (`plans/NETWORK.md` N17).
///
/// The stack publishes this whenever an interface's address or group set
/// changes — a control-plane event, not a per-frame one — and the driver
/// evaluates it on its harvest path.
///
/// It holds exactly the inputs to the stack's own destination-acceptance
/// rule, and **only slow-changing L3 state**: our addresses, the subnet
/// broadcast, and the joined groups. No listening ports, so nothing here can
/// fall behind a socket opening and drop a frame someone wanted — the stack
/// gates delivery of a group or broadcast destination on membership, never on
/// a port, so membership is the whole of what the filter needs to mirror it.
///
/// # It can only cost work, never authority
///
/// The filter is a load-shedding optimisation and is never load-bearing for
/// security: every admitted frame is still fully validated by the stack, and
/// the driver process already owns the device and could drop any frame it
/// liked. Its bias is therefore towards *admitting*: an address set too
/// large to carry sets [`Self::is_exhaustive`] false and the filter then
/// admits all unicast rather than dropping traffic to an address it was not
/// told about.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct RxFilterPolicy {
    v4_count: u8,
    v6_count: u8,
    groups_v4_count: u8,
    groups_v6_count: u8,
    exhaustive: bool,
    v4: [[u8; 4]; MAX_FILTER_V4],
    v4_broadcast: [[u8; 4]; MAX_FILTER_V4],
    v6: [[u8; 16]; MAX_FILTER_V6],
    groups_v4: [[u8; 4]; MAX_FILTER_GROUPS_V4],
    groups_v6: [[u8; 16]; MAX_FILTER_GROUPS_V6],
}

impl RxFilterPolicy {
    /// A policy that matches nothing and admits everything: the state before
    /// the stack has published an address set, and the state a driver holds
    /// when it has been told nothing.
    #[must_use]
    pub const fn admit_all() -> Self {
        Self {
            v4_count: 0,
            v6_count: 0,
            groups_v4_count: 0,
            groups_v6_count: 0,
            exhaustive: false,
            v4: [[0u8; 4]; MAX_FILTER_V4],
            v4_broadcast: [[0u8; 4]; MAX_FILTER_V4],
            v6: [[0u8; 16]; MAX_FILTER_V6],
            groups_v4: [[0u8; 4]; MAX_FILTER_GROUPS_V4],
            groups_v6: [[0u8; 16]; MAX_FILTER_GROUPS_V6],
        }
    }

    /// Build a policy from an interface's addresses and joined groups.
    ///
    /// `v4` pairs each IPv4 address with its subnet's directed-broadcast
    /// address. **Any** list longer than its bound is *truncated* and the
    /// policy marked non-exhaustive, so the filter widens to admit rather
    /// than silently dropping traffic to something it was not told about.
    #[must_use]
    pub fn new(
        v4: &[([u8; 4], [u8; 4])],
        v6: &[[u8; 16]],
        groups_v4: &[[u8; 4]],
        groups_v6: &[[u8; 16]],
    ) -> Self {
        let mut policy = Self::admit_all();
        policy.exhaustive = v4.len() <= MAX_FILTER_V4
            && v6.len() <= MAX_FILTER_V6
            && groups_v4.len() <= MAX_FILTER_GROUPS_V4
            && groups_v6.len() <= MAX_FILTER_GROUPS_V6;
        for (slot, (address, broadcast)) in v4.iter().take(MAX_FILTER_V4).enumerate() {
            policy.v4[slot] = *address;
            policy.v4_broadcast[slot] = *broadcast;
            policy.v4_count = policy.v4_count.saturating_add(1);
        }
        for (slot, address) in v6.iter().take(MAX_FILTER_V6).enumerate() {
            policy.v6[slot] = *address;
            policy.v6_count = policy.v6_count.saturating_add(1);
        }
        for (slot, group) in groups_v4.iter().take(MAX_FILTER_GROUPS_V4).enumerate() {
            policy.groups_v4[slot] = *group;
            policy.groups_v4_count = policy.groups_v4_count.saturating_add(1);
        }
        for (slot, group) in groups_v6.iter().take(MAX_FILTER_GROUPS_V6).enumerate() {
            policy.groups_v6[slot] = *group;
            policy.groups_v6_count = policy.groups_v6_count.saturating_add(1);
        }
        policy
    }

    /// Whether the policy names every local address of the interface. When
    /// false a consumer must admit all unicast.
    #[must_use]
    pub const fn is_exhaustive(&self) -> bool {
        self.exhaustive
    }

    /// The interface's IPv4 addresses.
    #[must_use]
    pub fn v4_addresses(&self) -> &[[u8; 4]] {
        &self.v4[..self.v4_count as usize]
    }

    /// The directed-broadcast address of each IPv4 address's subnet, in the
    /// same order as [`Self::v4_addresses`].
    #[must_use]
    pub fn v4_broadcasts(&self) -> &[[u8; 4]] {
        &self.v4_broadcast[..self.v4_count as usize]
    }

    /// The interface's IPv6 addresses.
    #[must_use]
    pub fn v6_addresses(&self) -> &[[u8; 16]] {
        &self.v6[..self.v6_count as usize]
    }

    /// The IPv4 group addresses the interface has joined.
    #[must_use]
    pub fn v4_groups(&self) -> &[[u8; 4]] {
        &self.groups_v4[..self.groups_v4_count as usize]
    }

    /// The IPv6 group addresses the interface has joined.
    #[must_use]
    pub fn v6_groups(&self) -> &[[u8; 16]] {
        &self.groups_v6[..self.groups_v6_count as usize]
    }
}

/// The group-address set of a [`NetChannelRequest::SetMulticast`].
///
/// Fixed-capacity and [`Copy`] so the whole request stays a plain value with
/// no allocation and no borrow of the decoded frame; the set is small
/// ([`MAX_MCAST_GROUPS`]) and only ever built when the stack's membership
/// changes, never on the frame path.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct McastGroups {
    count: u8,
    groups: [MacAddress; MAX_MCAST_GROUPS],
}

impl McastGroups {
    /// The empty set: the device admits no group address.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            count: 0,
            groups: [MacAddress::BROADCAST; MAX_MCAST_GROUPS],
        }
    }

    /// Collect `groups` into a set.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] when `groups` holds more than
    /// [`MAX_MCAST_GROUPS`] addresses, or any of them is not a group address
    /// (its IEEE 802 I/G bit is clear) — a unicast address here would widen
    /// the device's filter to a host the stack never asked for.
    pub fn new(groups: &[MacAddress]) -> Result<Self, Errno> {
        if groups.len() > MAX_MCAST_GROUPS {
            return Err(Errno::OutOfRange);
        }
        let mut set = Self::empty();
        for (slot, group) in groups.iter().enumerate() {
            if group.as_octets()[0] & 0x01 == 0 {
                return Err(Errno::OutOfRange);
            }
            set.groups[slot] = *group;
        }
        // `groups.len()` is bounded by `MAX_MCAST_GROUPS` above.
        set.count = u8::try_from(groups.len()).map_err(|_| Errno::OutOfRange)?;
        Ok(set)
    }

    /// The group addresses, in the order the stack supplied them.
    #[must_use]
    pub fn as_slice(&self) -> &[MacAddress] {
        &self.groups[..usize::from(self.count)]
    }
}

/// The parameters of a [`NetChannelRequest::Attach`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct AttachParams {
    /// The ring geometry both sides agree on (the stack derives it from
    /// [`DeviceFacts::mtu`] and sized the region to
    /// [`RingGeometry::region_len`]).
    pub geometry: RingGeometry,
    /// The unforgeable `shm_grant` handle the stack minted for the
    /// driver's endpoint; the driver `shm_map`s exactly this region.
    pub region_grant: u64,
    /// Sensitivity class the driver honours for its internal staging.
    pub class: BufferClass,
    /// The numeric IPC endpoint the driver `ipc_send`s a
    /// [`NetChannelNotify`] to when receive frames have arrived and the
    /// stack should issue the next [`NetChannelRequest::Service`]. The
    /// stack chose it, `port_bind`ed it, and parks on it in its wait set;
    /// the driver only sends to the number (a bystander cannot forge the
    /// wake because the stack owns the receive end).
    pub notify_endpoint: u64,
}

impl NetChannelRequest {
    /// Largest encoded request frame.
    pub const MAX_WIRE_LEN: usize = NET_CHANNEL_MAX_REQUEST;

    /// The operation's wire discriminant byte.
    const fn op_byte(&self) -> u8 {
        match self {
            Self::Facts => op::FACTS,
            Self::Attach(_) => op::ATTACH,
            Self::Service => op::SERVICE,
            Self::Detach => op::DETACH,
            Self::SetMulticast(_) => op::SET_MULTICAST,
            Self::SetRxFilter(_) => op::SET_RX_FILTER,
        }
    }

    /// Encode `self` into `out`, returning the number of bytes written.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] if `out` cannot hold the encoded frame.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Errno> {
        let len = match self {
            Self::Facts | Self::Service | Self::Detach => HEADER_LEN,
            Self::Attach(_) => HEADER_LEN + ATTACH_BODY_LEN,
            Self::SetMulticast(_) => HEADER_LEN + SET_MULTICAST_BODY_LEN,
            Self::SetRxFilter(_) => HEADER_LEN + SET_RX_FILTER_BODY_LEN,
        };
        if out.len() < len {
            return Err(Errno::BufferTooSmall);
        }
        for byte in &mut out[..len] {
            *byte = 0;
        }
        put_u32(out, 0, NET_CHANNEL_REQUEST_MAGIC);
        put_u16(out, 4, NET_CHANNEL_VERSION_V1);
        out[6] = self.op_byte();
        // out[7] reserved, left zero.
        if let Self::Attach(params) = self {
            put_u32(out, HEADER_LEN, params.geometry.slots());
            put_u32(out, HEADER_LEN + 4, params.geometry.rx_slot_capacity());
            put_u32(out, HEADER_LEN + 8, params.geometry.tx_slot_capacity());
            put_u64(out, HEADER_LEN + 12, params.region_grant);
            out[HEADER_LEN + 20] = params.class.as_u8();
            put_u16(out, HEADER_LEN + 21, params.geometry.rx_queues());
            // out[HEADER_LEN + 23] reserved, left zero.
            put_u64(out, HEADER_LEN + 24, params.notify_endpoint);
        }
        if let Self::SetMulticast(groups) = self {
            out[HEADER_LEN] = groups.count;
            // out[HEADER_LEN + 1] reserved, left zero.
            for (slot, group) in groups.as_slice().iter().enumerate() {
                let at = HEADER_LEN + 2 + slot * MAC_ADDRESS_LEN;
                out[at..at + MAC_ADDRESS_LEN].copy_from_slice(group.as_octets());
            }
        }
        if let Self::SetRxFilter(policy) = self {
            out[HEADER_LEN] = policy.v4_count;
            out[HEADER_LEN + 1] = policy.v6_count;
            out[HEADER_LEN + 2] = policy.groups_v4_count;
            out[HEADER_LEN + 3] = policy.groups_v6_count;
            out[HEADER_LEN + 4] = u8::from(policy.exhaustive);
            // out[HEADER_LEN + 5..8] reserved, left zero.
            let mut at = HEADER_LEN + 8;
            for (address, broadcast) in policy.v4_addresses().iter().zip(policy.v4_broadcasts()) {
                out[at..at + 4].copy_from_slice(address);
                out[at + 4..at + 8].copy_from_slice(broadcast);
                at += 8;
            }
            let mut at = HEADER_LEN + RX_FILTER_V6_AT;
            for address in policy.v6_addresses() {
                out[at..at + 16].copy_from_slice(address);
                at += 16;
            }
            let mut at = HEADER_LEN + RX_FILTER_GROUPS_V4_AT;
            for group in policy.v4_groups() {
                out[at..at + 4].copy_from_slice(group);
                at += 4;
            }
            let mut at = HEADER_LEN + RX_FILTER_GROUPS_V6_AT;
            for group in policy.v6_groups() {
                out[at..at + 16].copy_from_slice(group);
                at += 16;
            }
        }
        Ok(len)
    }

    /// Decode a request frame, fail-closed.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — shorter than the operation requires.
    /// * [`Errno::BadMagic`] — wrong magic or a dirty reserved byte.
    /// * [`Errno::AbiVersionUnsupported`] — not [`NET_CHANNEL_VERSION_V1`].
    /// * [`Errno::OutOfRange`] — an unknown operation byte or an
    ///   out-of-range geometry / class.
    pub fn decode(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < HEADER_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if read_u32(bytes, 0) != NET_CHANNEL_REQUEST_MAGIC {
            return Err(Errno::BadMagic);
        }
        if read_u16(bytes, 4) != NET_CHANNEL_VERSION_V1 {
            return Err(Errno::AbiVersionUnsupported);
        }
        if bytes[7] != 0 {
            return Err(Errno::BadMagic);
        }
        match bytes[6] {
            op::FACTS => Ok(Self::Facts),
            op::SERVICE => Ok(Self::Service),
            op::DETACH => Ok(Self::Detach),
            op::ATTACH => Self::decode_attach(bytes),
            op::SET_MULTICAST => Self::decode_set_multicast(bytes),
            op::SET_RX_FILTER => Self::decode_set_rx_filter(bytes),
            _ => Err(Errno::OutOfRange),
        }
    }

    fn decode_set_rx_filter(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < HEADER_LEN + SET_RX_FILTER_BODY_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let v4_count = usize::from(bytes[HEADER_LEN]);
        let v6_count = usize::from(bytes[HEADER_LEN + 1]);
        let groups_v4_count = usize::from(bytes[HEADER_LEN + 2]);
        let groups_v6_count = usize::from(bytes[HEADER_LEN + 3]);
        // A count past its fixed bound is a corrupt frame, refused whole
        // rather than clamped into a filter that would then drop traffic.
        if v4_count > MAX_FILTER_V4
            || v6_count > MAX_FILTER_V6
            || groups_v4_count > MAX_FILTER_GROUPS_V4
            || groups_v6_count > MAX_FILTER_GROUPS_V6
        {
            return Err(Errno::OutOfRange);
        }
        let exhaustive = match bytes[HEADER_LEN + 4] {
            0 => false,
            1 => true,
            _ => return Err(Errno::OutOfRange),
        };
        if bytes[HEADER_LEN + 5..HEADER_LEN + 8]
            .iter()
            .any(|b| *b != 0)
        {
            return Err(Errno::BadMagic);
        }
        let mut policy = RxFilterPolicy::admit_all();
        policy.exhaustive = exhaustive;
        for slot in 0..v4_count {
            let at = HEADER_LEN + 8 + slot * 8;
            policy.v4[slot].copy_from_slice(&bytes[at..at + 4]);
            policy.v4_broadcast[slot].copy_from_slice(&bytes[at + 4..at + 8]);
        }
        for slot in 0..v6_count {
            let at = HEADER_LEN + RX_FILTER_V6_AT + slot * 16;
            policy.v6[slot].copy_from_slice(&bytes[at..at + 16]);
        }
        for slot in 0..groups_v4_count {
            let at = HEADER_LEN + RX_FILTER_GROUPS_V4_AT + slot * 4;
            policy.groups_v4[slot].copy_from_slice(&bytes[at..at + 4]);
        }
        for slot in 0..groups_v6_count {
            let at = HEADER_LEN + RX_FILTER_GROUPS_V6_AT + slot * 16;
            policy.groups_v6[slot].copy_from_slice(&bytes[at..at + 16]);
        }
        // Widened only after every field validated, so a refused frame
        // never leaves a half-applied policy.
        policy.v4_count = u8::try_from(v4_count).map_err(|_| Errno::OutOfRange)?;
        policy.v6_count = u8::try_from(v6_count).map_err(|_| Errno::OutOfRange)?;
        policy.groups_v4_count = u8::try_from(groups_v4_count).map_err(|_| Errno::OutOfRange)?;
        policy.groups_v6_count = u8::try_from(groups_v6_count).map_err(|_| Errno::OutOfRange)?;
        Ok(Self::SetRxFilter(policy))
    }

    fn decode_attach(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < HEADER_LEN + ATTACH_BODY_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let slots = read_u32(bytes, HEADER_LEN);
        let rx_slot_capacity = read_u32(bytes, HEADER_LEN + 4);
        let tx_slot_capacity = read_u32(bytes, HEADER_LEN + 8);
        let region_grant = read_u64(bytes, HEADER_LEN + 12);
        let class = BufferClass::from_u8(bytes[HEADER_LEN + 20]).map_err(|_| Errno::OutOfRange)?;
        let rx_queues = read_u16(bytes, HEADER_LEN + 21);
        let geometry = RingGeometry::new(slots, rx_slot_capacity, tx_slot_capacity, rx_queues)?;
        if bytes[HEADER_LEN + 23] != 0 {
            return Err(Errno::BadMagic);
        }
        let notify_endpoint = read_u64(bytes, HEADER_LEN + 24);
        Ok(Self::Attach(AttachParams {
            geometry,
            region_grant,
            class,
            notify_endpoint,
        }))
    }

    fn decode_set_multicast(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < HEADER_LEN + SET_MULTICAST_BODY_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if bytes[HEADER_LEN + 1] != 0 {
            return Err(Errno::BadMagic);
        }
        let count = usize::from(bytes[HEADER_LEN]);
        if count > MAX_MCAST_GROUPS {
            return Err(Errno::OutOfRange);
        }
        let mut groups = [MacAddress::BROADCAST; MAX_MCAST_GROUPS];
        for (slot, group) in groups.iter_mut().take(count).enumerate() {
            let at = HEADER_LEN + 2 + slot * MAC_ADDRESS_LEN;
            let mut octets = [0u8; MAC_ADDRESS_LEN];
            octets.copy_from_slice(&bytes[at..at + MAC_ADDRESS_LEN]);
            *group = MacAddress::new(octets);
        }
        // Re-validate through the constructor so a hostile frame cannot
        // smuggle a unicast address into the device's filter.
        Self::new_set_multicast(&groups[..count])
    }

    fn new_set_multicast(groups: &[MacAddress]) -> Result<Self, Errno> {
        Ok(Self::SetMulticast(McastGroups::new(groups)?))
    }
}

/// The driver → stack wake (`plans/NETWORK.md` N4c, N17).
///
/// The driver `ipc_send`s this fixed frame to the
/// [`AttachParams::notify_endpoint`] after its device interrupt has
/// harvested frames into the shared region. *Which* channel woke is the
/// port it arrived on.
///
/// It carries the two things the driver knows and the stack would otherwise
/// have to ask for, so a receive that needs no transmit costs no call at
/// all:
///
/// * `link` — the device's live link state. Without it a link change on an
///   otherwise idle interface would go unseen until some unrelated transmit
///   provoked a doorbell, and a bond failover keys on exactly that report.
/// * `back_pressure` — the driver has masked its completion source and only
///   a [`NetChannelRequest::Service`] can release it. The stack must issue
///   one after draining, even with nothing to transmit, or the device stays
///   masked.
/// * `filtered` — the device's cumulative receive-pre-filter count. A pure
///   receive rings no doorbell, so without it here the only reader of
///   [`ServiceReport::filtered`](super::net_ring::ServiceReport::filtered)
///   would be a doorbell the receive path deliberately never rings, and the
///   operator's `stats:net/<iface>/rx.filtered` would sit frozen at whatever
///   the last transmit happened to observe.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct NetChannelNotify {
    /// The device's link state as the driver last observed it.
    pub link: LinkState,
    /// The driver masked its completion source; a `Service` is needed to
    /// release it.
    pub back_pressure: bool,
    /// Frames the device's receive pre-filter has shed since it was opened
    /// — the same cumulative counter
    /// [`ServiceReport::filtered`](super::net_ring::ServiceReport::filtered)
    /// carries, so a consumer keeps the latest value it saw from either.
    pub filtered: u64,
}

/// Wire byte for a notify reporting back-pressure.
const NOTIFY_BACK_PRESSURE: u8 = 1;
/// Wire byte for a notify reporting none.
const NOTIFY_FLOWING: u8 = 0;

impl NetChannelNotify {
    /// Encoded length of the notify frame.
    pub const WIRE_LEN: usize = NET_CHANNEL_NOTIFY_LEN;

    /// Encode the notify frame.
    #[must_use]
    pub fn encode(&self) -> [u8; NET_CHANNEL_NOTIFY_LEN] {
        let mut out = [0u8; NET_CHANNEL_NOTIFY_LEN];
        put_u32(&mut out, 0, NET_CHANNEL_NOTIFY_MAGIC);
        put_u16(&mut out, 4, NET_CHANNEL_VERSION_V1);
        out[6] = match self.link {
            LinkState::Up => LINK_UP,
            LinkState::Down => LINK_DOWN,
        };
        out[7] = if self.back_pressure {
            NOTIFY_BACK_PRESSURE
        } else {
            NOTIFY_FLOWING
        };
        put_u64(&mut out, 8, self.filtered);
        out
    }

    /// Decode a notify frame, fail-closed.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — shorter than [`NET_CHANNEL_NOTIFY_LEN`].
    /// * [`Errno::BadMagic`] — wrong magic.
    /// * [`Errno::AbiVersionUnsupported`] — not [`NET_CHANNEL_VERSION_V1`].
    /// * [`Errno::OutOfRange`] — an undefined link or flag byte.
    pub fn decode(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < NET_CHANNEL_NOTIFY_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if read_u32(bytes, 0) != NET_CHANNEL_NOTIFY_MAGIC {
            return Err(Errno::BadMagic);
        }
        if read_u16(bytes, 4) != NET_CHANNEL_VERSION_V1 {
            return Err(Errno::AbiVersionUnsupported);
        }
        let link = match bytes[6] {
            LINK_UP => LinkState::Up,
            LINK_DOWN => LinkState::Down,
            _ => return Err(Errno::OutOfRange),
        };
        // An undefined flag byte is refused rather than read as "flowing":
        // guessing here would leave a masked device wedged.
        let back_pressure = match bytes[7] {
            NOTIFY_BACK_PRESSURE => true,
            NOTIFY_FLOWING => false,
            _ => return Err(Errno::OutOfRange),
        };
        Ok(Self {
            link,
            back_pressure,
            filtered: read_u64(bytes, 8),
        })
    }
}

// --- DeviceFacts wire codec (the Facts reply payload) -------------------

/// Wire length of a [`DeviceFacts`] payload: mac (6) + mtu (4) + offloads
/// (4) + `rx_queues` (2) + link (1) + multicast-filter kind (1) +
/// multicast slots (2).
const FACTS_PAYLOAD_LEN: usize = MAC_ADDRESS_LEN + 4 + 4 + 2 + 1 + 1 + 2;

/// Wire byte for [`McastFilter::Unfiltered`].
const MCAST_UNFILTERED: u8 = 0;
/// Wire byte for [`McastFilter::Slots`].
const MCAST_SLOTS: u8 = 1;

/// Wire length of the Facts reply: a status word then the payload (zeroed
/// on refusal).
pub const NET_CHANNEL_FACTS_REPLY_LEN: usize = 4 + FACTS_PAYLOAD_LEN;

/// Wire byte for [`LinkState::Up`].
const LINK_UP: u8 = 1;
/// Wire byte for [`LinkState::Down`].
const LINK_DOWN: u8 = 0;

/// Encode the driver's reply to [`NetChannelRequest::Facts`]: `0` status
/// and the validated facts, or a `-errno` status and a zeroed payload.
#[must_use]
pub fn encode_facts_reply(result: Result<DeviceFacts, Errno>) -> [u8; NET_CHANNEL_FACTS_REPLY_LEN] {
    let mut out = [0u8; NET_CHANNEL_FACTS_REPLY_LEN];
    match result {
        Ok(facts) => {
            // status stays 0.
            let body = &mut out[4..];
            body[..MAC_ADDRESS_LEN].copy_from_slice(facts.mac.as_octets());
            put_u32(body, MAC_ADDRESS_LEN, facts.mtu);
            put_u32(body, MAC_ADDRESS_LEN + 4, facts.offloads.bits());
            put_u16(body, MAC_ADDRESS_LEN + 8, facts.rx_queues);
            body[MAC_ADDRESS_LEN + 10] = match facts.link {
                LinkState::Up => LINK_UP,
                LinkState::Down => LINK_DOWN,
            };
            let (kind, slots) = match facts.multicast_filter {
                McastFilter::Unfiltered => (MCAST_UNFILTERED, 0),
                McastFilter::Slots(slots) => (MCAST_SLOTS, slots),
            };
            body[MAC_ADDRESS_LEN + 11] = kind;
            put_u16(body, MAC_ADDRESS_LEN + 12, slots);
        }
        Err(err) => {
            let status = (-err.as_i32()).to_le_bytes();
            out[..4].copy_from_slice(&status);
        }
    }
    out
}

/// Decode a Facts reply, fail-closed.
///
/// # Errors
///
/// * [`Errno::BufferTooSmall`] — shorter than [`NET_CHANNEL_FACTS_REPLY_LEN`].
/// * The decoded [`Errno`] — the driver refused the query.
/// * [`Errno::OutOfRange`] — a corrupt status, link byte, reserved byte, or
///   offload/facts value that fails validation.
pub fn decode_facts_reply(bytes: &[u8]) -> Result<DeviceFacts, Errno> {
    if bytes.len() < NET_CHANNEL_FACTS_REPLY_LEN {
        return Err(Errno::BufferTooSmall);
    }
    let mut status = [0u8; 4];
    status.copy_from_slice(&bytes[..4]);
    let status = i32::from_le_bytes(status);
    if status != 0 {
        let errno = Errno::try_from_status(status).ok_or(Errno::OutOfRange)?;
        return Err(errno);
    }
    let body = &bytes[4..];
    let mut mac = [0u8; MAC_ADDRESS_LEN];
    mac.copy_from_slice(&body[..MAC_ADDRESS_LEN]);
    let mtu = read_u32(body, MAC_ADDRESS_LEN);
    let offloads = NetOffloads::from_bits(read_u32(body, MAC_ADDRESS_LEN + 4))?;
    let rx_queues = read_u16(body, MAC_ADDRESS_LEN + 8);
    let link = match body[MAC_ADDRESS_LEN + 10] {
        LINK_UP => LinkState::Up,
        LINK_DOWN => LinkState::Down,
        _ => return Err(Errno::OutOfRange),
    };
    let slots = read_u16(body, MAC_ADDRESS_LEN + 12);
    let multicast_filter = match body[MAC_ADDRESS_LEN + 11] {
        // An unfiltered device has no slot count; a dirty one is a corrupt
        // report, not a device that filters.
        MCAST_UNFILTERED if slots == 0 => McastFilter::Unfiltered,
        MCAST_SLOTS => McastFilter::Slots(slots),
        _ => return Err(Errno::OutOfRange),
    };
    let facts = DeviceFacts {
        mac: MacAddress::new(mac),
        mtu,
        link,
        offloads,
        rx_queues,
        multicast_filter,
    };
    facts.validate()?;
    Ok(facts)
}

// --- ServiceReport wire codec (the Service reply payload) ---------------

/// Byte offsets of each [`ServiceReport`] field within the Service reply's
/// payload, so the encoder, the decoder, and the tests that corrupt a
/// specific byte all read one layout. `FILTERED` is a `u64` (it is a
/// cumulative device counter); the two above it are per-call `u32`s.
mod service {
    pub const TRANSMITTED: usize = 0;
    pub const RECEIVED: usize = 4;
    pub const FILTERED: usize = 8;
    pub const RX_RING_FULL: usize = 16;
    pub const LINK: usize = 17;
    pub const LEN: usize = 18;
}

/// Wire length of a [`ServiceReport`] payload.
const SERVICE_PAYLOAD_LEN: usize = service::LEN;

/// Wire length of the Service reply: a status word then the payload (zeroed
/// on refusal).
pub const NET_CHANNEL_SERVICE_REPLY_LEN: usize = 4 + SERVICE_PAYLOAD_LEN;

/// Largest reply any device-channel request produces: the [`Facts`] reply is
/// the widest ([`NET_CHANNEL_FACTS_REPLY_LEN`]), ahead of the [`Service`]
/// reply and the [`STATUS_REPLY_LEN`](crate::reply::STATUS_REPLY_LEN)-byte
/// status the [`Attach`]/[`Detach`] operations answer. A fixed bound the
/// driver's call endpoint and the stack's client both size their reply buffer
/// to, so the endpoint's `max_reply` is one definition, never a per-site
/// guess.
///
/// [`Facts`]: NetChannelRequest::Facts
/// [`Service`]: NetChannelRequest::Service
/// [`Attach`]: NetChannelRequest::Attach
/// [`Detach`]: NetChannelRequest::Detach
pub const NET_CHANNEL_MAX_REPLY: usize = NET_CHANNEL_FACTS_REPLY_LEN;

/// Encode the driver's reply to [`NetChannelRequest::Service`]: `0` status
/// and the report, or a `-errno` status and a zeroed payload.
#[must_use]
pub fn encode_service_reply(
    result: Result<ServiceReport, Errno>,
) -> [u8; NET_CHANNEL_SERVICE_REPLY_LEN] {
    let mut out = [0u8; NET_CHANNEL_SERVICE_REPLY_LEN];
    match result {
        Ok(report) => {
            let body = &mut out[4..];
            put_u32(body, service::TRANSMITTED, report.transmitted);
            put_u32(body, service::RECEIVED, report.received);
            put_u64(body, service::FILTERED, report.filtered);
            body[service::RX_RING_FULL] = u8::from(report.rx_ring_full);
            body[service::LINK] = match report.link {
                LinkState::Up => LINK_UP,
                LinkState::Down => LINK_DOWN,
            };
        }
        Err(err) => {
            let status = (-err.as_i32()).to_le_bytes();
            out[..4].copy_from_slice(&status);
        }
    }
    out
}

/// Decode a Service reply, fail-closed.
///
/// # Errors
///
/// * [`Errno::BufferTooSmall`] — shorter than [`NET_CHANNEL_SERVICE_REPLY_LEN`].
/// * The decoded [`Errno`] — the driver refused the doorbell.
/// * [`Errno::OutOfRange`] — a corrupt status or a flag byte that is not
///   `0` or `1`.
pub fn decode_service_reply(bytes: &[u8]) -> Result<ServiceReport, Errno> {
    if bytes.len() < NET_CHANNEL_SERVICE_REPLY_LEN {
        return Err(Errno::BufferTooSmall);
    }
    let mut status = [0u8; 4];
    status.copy_from_slice(&bytes[..4]);
    let status = i32::from_le_bytes(status);
    if status != 0 {
        let errno = Errno::try_from_status(status).ok_or(Errno::OutOfRange)?;
        return Err(errno);
    }
    let body = &bytes[4..];
    let transmitted = read_u32(body, service::TRANSMITTED);
    let received = read_u32(body, service::RECEIVED);
    let filtered = read_u64(body, service::FILTERED);
    let rx_ring_full = match body[service::RX_RING_FULL] {
        0 => false,
        1 => true,
        _ => return Err(Errno::OutOfRange),
    };
    let link = match body[service::LINK] {
        LINK_UP => LinkState::Up,
        LINK_DOWN => LinkState::Down,
        _ => return Err(Errno::OutOfRange),
    };
    Ok(ServiceReport {
        transmitted,
        received,
        filtered,
        rx_ring_full,
        link,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry() -> RingGeometry {
        // A GSO-class transmit capacity distinct from the receive MTU,
        // and a multi-queue receive count, so the round-trip proves both
        // capacities and `rx_queues` survive the wire.
        RingGeometry::new(256, 1514, 65_549, 4).expect("valid geometry")
    }

    fn attach() -> NetChannelRequest {
        NetChannelRequest::Attach(AttachParams {
            geometry: geometry(),
            region_grant: 0xDEAD_BEEF_0BAD_F00D,
            class: BufferClass::Sensitive,
            notify_endpoint: NET_CHANNEL_ENDPOINT_BASE,
        })
    }

    #[test]
    fn set_multicast_round_trips_and_rejects_a_unicast_address() {
        let groups = [
            MacAddress::new([0x33, 0x33, 0x00, 0x00, 0x00, 0x01]),
            MacAddress::new([0x01, 0x00, 0x5E, 0x7F, 0x00, 0x02]),
        ];
        let request =
            NetChannelRequest::SetMulticast(McastGroups::new(&groups).expect("group set"));
        let mut out = [0u8; NetChannelRequest::MAX_WIRE_LEN];
        let len = request.encode(&mut out).expect("encodes");
        assert_eq!(NetChannelRequest::decode(&out[..len]), Ok(request));

        // A unicast address here would widen the device's filter to a host
        // the stack never asked for: refused at construction and on decode.
        assert_eq!(
            McastGroups::new(&[MacAddress::new([0x02, 0x11, 0x22, 0x33, 0x44, 0x55])]).err(),
            Some(Errno::OutOfRange)
        );
        let mut smuggled = out;
        smuggled[HEADER_LEN + 2] = 0x02;
        assert_eq!(
            NetChannelRequest::decode(&smuggled[..len]),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn an_over_long_or_short_multicast_set_is_refused() {
        let mut out = [0u8; NetChannelRequest::MAX_WIRE_LEN];
        let len = NetChannelRequest::SetMulticast(McastGroups::empty())
            .encode(&mut out)
            .expect("encodes");
        // A count past the fixed-width body cannot be honoured.
        let mut over = out;
        // `MAX_MCAST_GROUPS` fits a u8, so one past it does too.
        over[HEADER_LEN] = u8::try_from(MAX_MCAST_GROUPS).expect("fits") + 1;
        assert_eq!(
            NetChannelRequest::decode(&over[..len]),
            Err(Errno::OutOfRange)
        );
        // A dirty reserved byte is a malformed frame, never ignored.
        let mut dirty = out;
        dirty[HEADER_LEN + 1] = 1;
        assert_eq!(
            NetChannelRequest::decode(&dirty[..len]),
            Err(Errno::BadMagic)
        );
        // A truncated body is refused rather than partially read.
        assert_eq!(
            NetChannelRequest::decode(&out[..len - 1]),
            Err(Errno::BufferTooSmall)
        );
        assert_eq!(McastGroups::empty().as_slice(), &[]);
    }

    #[test]
    fn the_multicast_filter_survives_the_facts_round_trip() {
        for filter in [
            McastFilter::Unfiltered,
            McastFilter::Slots(0),
            McastFilter::Slots(15),
        ] {
            let mut expected = facts();
            expected.multicast_filter = filter;
            let reply = encode_facts_reply(Ok(expected));
            assert_eq!(decode_facts_reply(&reply), Ok(expected), "{filter:?}");
        }
        // An unfiltered report carrying a slot count is corrupt, not a
        // device that filters: fail closed rather than pick a reading.
        let mut reply = encode_facts_reply(Ok(facts()));
        reply[4 + MAC_ADDRESS_LEN + 11] = MCAST_UNFILTERED;
        put_u16(&mut reply, 4 + MAC_ADDRESS_LEN + 12, 3);
        assert_eq!(decode_facts_reply(&reply), Err(Errno::OutOfRange));
        // An unknown filter kind is refused.
        let mut unknown = encode_facts_reply(Ok(facts()));
        unknown[4 + MAC_ADDRESS_LEN + 11] = 9;
        assert_eq!(decode_facts_reply(&unknown), Err(Errno::OutOfRange));
    }

    fn facts() -> DeviceFacts {
        DeviceFacts {
            mac: MacAddress::new([0x02, 0x11, 0x22, 0x33, 0x44, 0x55]),
            mtu: 1500,
            link: LinkState::Up,
            offloads: NetOffloads::from_bits(NetOffloads::TX_CSUM_UDP.bits())
                .expect("defined bits"),
            multicast_filter: McastFilter::Slots(15),
            rx_queues: 2,
        }
    }

    #[test]
    fn request_round_trips_every_operation() {
        for req in [
            NetChannelRequest::Facts,
            NetChannelRequest::Service,
            NetChannelRequest::Detach,
            attach(),
            NetChannelRequest::SetRxFilter(RxFilterPolicy::admit_all()),
            NetChannelRequest::SetRxFilter(rx_filter()),
        ] {
            let mut buf = [0u8; NetChannelRequest::MAX_WIRE_LEN];
            let len = req.encode(&mut buf).expect("encode");
            assert_eq!(NetChannelRequest::decode(&buf[..len]), Ok(req));
        }
    }

    /// A policy exercising every array in the body, so a field the codec
    /// forgets shows up as a round-trip mismatch rather than as traffic the
    /// driver silently sheds.
    fn rx_filter() -> RxFilterPolicy {
        RxFilterPolicy::new(
            &[([10, 0, 2, 15], [10, 0, 2, 255])],
            &[[0x20; 16], [0xFE; 16]],
            &[[224, 0, 0, 1], [239, 1, 2, 3]],
            &[[0xFF; 16]],
        )
    }

    #[test]
    fn an_rx_filter_carries_every_address_and_group_over_the_wire() {
        let policy = rx_filter();
        let mut buf = [0u8; NetChannelRequest::MAX_WIRE_LEN];
        let len = NetChannelRequest::SetRxFilter(policy)
            .encode(&mut buf)
            .expect("encode");
        let Ok(NetChannelRequest::SetRxFilter(back)) = NetChannelRequest::decode(&buf[..len])
        else {
            panic!("a SetRxFilter frame must decode as one");
        };
        assert!(back.is_exhaustive());
        assert_eq!(back.v4_addresses(), [[10, 0, 2, 15]]);
        assert_eq!(back.v4_broadcasts(), [[10, 0, 2, 255]]);
        assert_eq!(back.v6_addresses(), [[0x20; 16], [0xFE; 16]]);
        assert_eq!(back.v4_groups(), [[224, 0, 0, 1], [239, 1, 2, 3]]);
        assert_eq!(back.v6_groups(), [[0xFF; 16]]);
    }

    #[test]
    fn an_over_capacity_group_set_widens_the_filter_instead_of_dropping() {
        // Truncation must never be silent: a policy that could not name every
        // joined group admits everything rather than shedding a group it was
        // not told about.
        let groups = [[224, 0, 0, 1]; MAX_FILTER_GROUPS_V4 + 1];
        let policy = RxFilterPolicy::new(&[([10, 0, 2, 15], [10, 0, 2, 255])], &[], &groups, &[]);
        assert!(!policy.is_exhaustive());
        let v6 = [[0x20; 16]; MAX_FILTER_GROUPS_V6 + 1];
        let policy = RxFilterPolicy::new(&[], &[], &[], &v6);
        assert!(!policy.is_exhaustive());
    }

    #[test]
    fn an_rx_filter_count_past_its_bound_is_refused() {
        let mut buf = [0u8; NetChannelRequest::MAX_WIRE_LEN];
        let len = NetChannelRequest::SetRxFilter(rx_filter())
            .encode(&mut buf)
            .expect("encode");
        for offset in 0..4 {
            let mut bad = buf;
            bad[HEADER_LEN + offset] = 0xFF;
            assert_eq!(
                NetChannelRequest::decode(&bad[..len]),
                Err(Errno::OutOfRange),
                "a corrupt count is refused whole, never clamped"
            );
        }
        // Every reserved byte is checked, not just the first.
        for offset in 5..8 {
            let mut bad = buf;
            bad[HEADER_LEN + offset] = 1;
            assert_eq!(NetChannelRequest::decode(&bad[..len]), Err(Errno::BadMagic));
        }
    }

    #[test]
    fn encode_into_short_buffer_fails_closed() {
        let mut small = [0u8; HEADER_LEN - 1];
        assert_eq!(
            NetChannelRequest::Facts.encode(&mut small),
            Err(Errno::BufferTooSmall)
        );
        let mut header_only = [0u8; HEADER_LEN];
        assert_eq!(
            attach().encode(&mut header_only),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn decode_rejects_bad_magic_version_reserved_and_op() {
        let mut buf = [0u8; NetChannelRequest::MAX_WIRE_LEN];
        let len = NetChannelRequest::Facts.encode(&mut buf).expect("encode");
        // Good baseline.
        assert!(NetChannelRequest::decode(&buf[..len]).is_ok());
        // Bad magic.
        let mut bad = buf;
        bad[0] ^= 0xFF;
        assert_eq!(NetChannelRequest::decode(&bad[..len]), Err(Errno::BadMagic));
        // Bad version.
        let mut bad = buf;
        bad[4] = 0xFF;
        assert_eq!(
            NetChannelRequest::decode(&bad[..len]),
            Err(Errno::AbiVersionUnsupported)
        );
        // Dirty reserved byte.
        let mut bad = buf;
        bad[7] = 1;
        assert_eq!(NetChannelRequest::decode(&bad[..len]), Err(Errno::BadMagic));
        // Unknown op.
        let mut bad = buf;
        bad[6] = 0xEE;
        assert_eq!(
            NetChannelRequest::decode(&bad[..len]),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn decode_attach_rejects_bad_geometry_class_reserved() {
        let mut buf = [0u8; NetChannelRequest::MAX_WIRE_LEN];
        let len = attach().encode(&mut buf).expect("encode");
        // Zero slots -> out of range geometry.
        let mut bad = buf;
        put_u32(&mut bad, HEADER_LEN, 0);
        assert_eq!(
            NetChannelRequest::decode(&bad[..len]),
            Err(Errno::OutOfRange)
        );
        // Unknown buffer class byte.
        let mut bad = buf;
        bad[HEADER_LEN + 20] = 0x7F;
        assert_eq!(
            NetChannelRequest::decode(&bad[..len]),
            Err(Errno::OutOfRange)
        );
        // Out-of-range receive-queue count (0) -> out of range geometry.
        let mut bad = buf;
        put_u16(&mut bad, HEADER_LEN + 21, 0);
        assert_eq!(
            NetChannelRequest::decode(&bad[..len]),
            Err(Errno::OutOfRange)
        );
        // Dirty reserved byte in the attach body (past `rx_queues`).
        let mut bad = buf;
        bad[HEADER_LEN + 23] = 1;
        assert_eq!(NetChannelRequest::decode(&bad[..len]), Err(Errno::BadMagic));
        // Truncated attach body.
        assert_eq!(
            NetChannelRequest::decode(&buf[..=HEADER_LEN]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn notify_round_trips_and_fails_closed() {
        for link in [LinkState::Up, LinkState::Down] {
            for back_pressure in [false, true] {
                for filtered in [0, 1, u64::MAX] {
                    let notify = NetChannelNotify {
                        link,
                        back_pressure,
                        filtered,
                    };
                    let frame = notify.encode();
                    assert_eq!(NetChannelNotify::decode(&frame), Ok(notify));
                }
            }
        }
        let frame = NetChannelNotify {
            link: LinkState::Up,
            back_pressure: false,
            filtered: 0,
        }
        .encode();
        let mut bad = frame;
        bad[0] ^= 0xFF;
        assert_eq!(NetChannelNotify::decode(&bad), Err(Errno::BadMagic));
        // An undefined link or flag byte is refused, never read as a
        // default: guessing "flowing" would leave a masked device wedged.
        let mut bad = frame;
        bad[6] = 0xFF;
        assert_eq!(NetChannelNotify::decode(&bad), Err(Errno::OutOfRange));
        let mut bad = frame;
        bad[7] = 0xFF;
        assert_eq!(NetChannelNotify::decode(&bad), Err(Errno::OutOfRange));
        assert_eq!(
            NetChannelNotify::decode(&frame[..NET_CHANNEL_NOTIFY_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn facts_reply_round_trips_and_carries_errors() {
        let ok = encode_facts_reply(Ok(facts()));
        assert_eq!(decode_facts_reply(&ok), Ok(facts()));
        let err = encode_facts_reply(Err(Errno::DeviceFault));
        assert_eq!(decode_facts_reply(&err), Err(Errno::DeviceFault));
    }

    #[test]
    fn facts_reply_rejects_corrupt_payload() {
        let mut ok = encode_facts_reply(Ok(facts()));
        // Corrupt the link byte.
        ok[4 + MAC_ADDRESS_LEN + 10] = 0x55;
        assert_eq!(decode_facts_reply(&ok), Err(Errno::OutOfRange));
        // A runt MTU fails DeviceFacts::validate.
        let mut ok = encode_facts_reply(Ok(facts()));
        put_u32(&mut ok, 4 + MAC_ADDRESS_LEN, 1);
        assert_eq!(decode_facts_reply(&ok), Err(Errno::OutOfRange));
    }

    #[test]
    fn service_reply_round_trips_and_carries_errors() {
        let report = ServiceReport {
            transmitted: 3,
            received: 7,
            filtered: 11,
            rx_ring_full: true,
            link: LinkState::Down,
        };
        let ok = encode_service_reply(Ok(report));
        assert_eq!(decode_service_reply(&ok), Ok(report));
        // The other link state also round-trips.
        let up = ServiceReport {
            link: LinkState::Up,
            ..report
        };
        assert_eq!(decode_service_reply(&encode_service_reply(Ok(up))), Ok(up));
        let err = encode_service_reply(Err(Errno::BadMagic));
        assert_eq!(decode_service_reply(&err), Err(Errno::BadMagic));
        // A flag byte that is neither 0 nor 1 is refused.
        let mut bad = encode_service_reply(Ok(report));
        bad[4 + service::RX_RING_FULL] = 2;
        assert_eq!(decode_service_reply(&bad), Err(Errno::OutOfRange));
        // A link byte that is neither LINK_UP nor LINK_DOWN is refused.
        let mut bad = encode_service_reply(Ok(report));
        bad[4 + service::LINK] = 0x55;
        assert_eq!(decode_service_reply(&bad), Err(Errno::OutOfRange));
    }
}
