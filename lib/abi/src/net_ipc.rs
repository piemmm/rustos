//! The network-stack service IPC protocol (`plans/NETWORK.md` N3b):
//! the reserved rendezvous `netstack` binds and the fixed-width,
//! fail-closed requests its admin surface and the sysinfo broker
//! present.
//!
//! Two request classes share the one endpoint:
//!
//! * **Admin mutation and counters** (`InterfaceList`, `AddrAdd`,
//!   `RouteAdd`, `Counters`) — gated on
//!   [`CapabilityId::NET_ADMIN`](crate::CapabilityId::NET_ADMIN).
//! * **Broker reads** (`InterfaceFacts`, `InterfaceState`) — the
//!   whole-system interface facts/state the System Information
//!   service pages on behalf of its own capability-checked clients,
//!   gated on
//!   [`CapabilityId::SYSINFO_INTROSPECT`](crate::CapabilityId::SYSINFO_INTROSPECT)
//!   exactly as the kernel's introspection primitive is: `netstack`
//!   answers whole state only to the sysinfo broker, and all
//!   per-client narrowing lives in that audited broker.
//!
//! Every decode fails closed: an unknown magic, version, operation,
//! family, name spelling, or a dirty reserved tail refuses rather
//! than guessing (`AGENTS.md` §5.4).

use crate::le::{put_u16, put_u32, put_u64, read_u16, read_u32, read_u64};
use crate::reply::{decode_status_reply, encode_status_reply, STATUS_REPLY_LEN};
use crate::time::Duration64;
use crate::Errno;

/// Reserved well-known call-endpoint id of the network-stack service
/// (`"NET1"` little-endian, the [`crate::display_ipc::DISPLAY_ENDPOINT`]
/// convention). Binding it requires `CAP_IPC_BIND_PRIVILEGED`
/// ([`crate::ipc::is_reserved_endpoint`]): a squatter claiming the
/// rendezvous first would receive interface mutations and serve forged
/// network state.
pub const NETSTACK_ENDPOINT: u64 = 0x4E45_5431;

/// Magic number identifying a netstack request (`"NST1"`
/// little-endian).
pub const NETSTACK_REQUEST_MAGIC: u32 = u32::from_le_bytes(*b"NST1");

/// The `netstack-v1` protocol version.
pub const NETSTACK_VERSION_V1: u16 = 1;

/// Byte length of an interface name field: NUL-padded ASCII.
pub const IF_NAME_LEN: usize = 16;

/// Most interfaces one paged read reports per call — a validation
/// bound on the reply size, not an interface-count capacity.
pub const NETSTACK_LIST_LIMIT_MAX: u16 = 32;

/// Largest request the [`NETSTACK_ENDPOINT`] accepts.
///
/// Three distinct fixed-width framed messages arrive on the admin
/// endpoint: the 64-byte [`NetstackRequest`], the wider per-interface
/// [`NetInterfaceConfigMsg`], and the wider still [`NetBondConfigMsg`]
/// (each rich payload does not fit the 64-byte request enum, so each is
/// its own self-identifying frame, decoded by the transport before
/// [`NetstackRequest`], the `BindDriver`-interception precedent). The
/// receive buffer is sized to the largest so none is ever truncated.
pub const NETSTACK_MAX_REQUEST: usize = max_usize(
    NetstackRequest::WIRE_LEN,
    max_usize(NetInterfaceConfigMsg::WIRE_LEN, NetBondConfigMsg::WIRE_LEN),
);

/// The larger of two sizes, as a `const fn` (there is no `const`
/// [`Ord::max`] in a stable `const` context).
const fn max_usize(a: usize, b: usize) -> usize {
    if a >= b {
        a
    } else {
        b
    }
}

/// An IP address family as carried on this protocol.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum NetAddrFamily {
    /// IPv4; the address field's first four bytes are significant.
    V4 = 4,
    /// IPv6; all sixteen address bytes are significant.
    V6 = 6,
}

impl NetAddrFamily {
    /// The wire value for this family.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Recover a family from its wire value.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] if `value` names no family (fail closed).
    pub const fn from_u8(value: u8) -> Result<Self, Errno> {
        match value {
            4 => Ok(Self::V4),
            6 => Ok(Self::V6),
            _ => Err(Errno::OutOfRange),
        }
    }

    /// Widest valid prefix length for this family.
    #[must_use]
    pub const fn max_prefix(self) -> u8 {
        match self {
            Self::V4 => 32,
            Self::V6 => 128,
        }
    }
}

/// Validate an interface-name field: NUL-padded, non-empty, at most
/// [`IF_NAME_LEN`] significant bytes of lowercase ASCII letters or
/// digits (the ALIAS admin-chosen alias grammar), with nothing after
/// the first NUL.
///
/// Returns the significant length.
///
/// # Errors
///
/// [`Errno::OutOfRange`] on an empty name, an illegal byte, or a
/// non-NUL byte after the terminator.
pub fn validate_if_name(name: &[u8; IF_NAME_LEN]) -> Result<usize, Errno> {
    let len = name.iter().position(|&b| b == 0).unwrap_or(IF_NAME_LEN);
    if len == 0 {
        return Err(Errno::OutOfRange);
    }
    if !name[..len]
        .iter()
        .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit())
    {
        return Err(Errno::OutOfRange);
    }
    if name[len..].iter().any(|&b| b != 0) {
        return Err(Errno::OutOfRange);
    }
    Ok(len)
}

/// Stack-wide network policy delivered to `netstack` from the
/// `system.conf` `net.*` registry (`plans/NETWORK.md` §6.2).
///
/// `netstack` is the network-parsing sandbox and holds no filesystem
/// capability, so it cannot read `system.conf` itself: an FS-capable
/// component (init/devmgr) reads the config after the root unlock and
/// pushes these settings over the [`NetstackRequest::ApplyNetworkSettings`]
/// admin op. The mapping from the `lib/sysconfig` registry is exact —
/// `net.ipv4.enabled`, `net.ipv6.enabled`, and `net.tcp.syncookies`
/// (`always` ⇒ [`Self::syncookies_always`]; `auto` ⇒ the bounded
/// default), `net.ipv6.privacy`, and `net.tcp.keepalive`.
// These are independent stack-wide policy flags, each mapped one-to-one
// to a wire byte and a distinct `system.conf` key — not a state that
// enums would model better; grouping them into an enum would obscure the
// exact wire layout and the exact registry mapping.
#[allow(clippy::struct_excessive_bools)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct NetworkSettings {
    /// Whether the IPv4 family is enabled stack-wide. When `false` an
    /// interface binds no IPv4 address and answers no IPv4/ARP, and a
    /// socket `open` for the family is refused.
    pub ipv4_enabled: bool,
    /// Whether the IPv6 family is enabled stack-wide. When `false` an
    /// interface forms no link-local, accepts no IPv6, and a socket
    /// `open` for the family is refused.
    pub ipv6_enabled: bool,
    /// Whether TCP SYN cookies are used unconditionally
    /// (`net.tcp.syncookies always`): the listener holds zero half-open
    /// state and answers every SYN with a stateless cookie. `false`
    /// selects the bounded half-open backlog (`auto`).
    pub syncookies_always: bool,
    /// Whether the stack forms RFC 8981 temporary (privacy) IPv6
    /// addresses in addition to the stable SLAAC address of each
    /// autonomous prefix (`net.ipv6.privacy`). `false` (the default)
    /// leaves only the stable address.
    pub ipv6_privacy: bool,
    /// Whether TCP connections send RFC 9293 §3.8.4 keepalive probes on an
    /// idle link (`net.tcp.keepalive`). When `true`, every new connection
    /// (actively opened and accepted alike) probes an idle peer after the
    /// standard idle interval and is torn down if the peer stops answering.
    /// `false` (the default, RFC 1122 §4.2.3.6) never probes and never
    /// tears an idle connection down for inactivity.
    pub tcp_keepalive: bool,
}

impl Default for NetworkSettings {
    /// The stack's safe pre-delivery defaults, matching the
    /// `lib/sysconfig` registry defaults: both families enabled and SYN
    /// cookies in `auto` mode (a bounded half-open backlog, not
    /// unconditional cookies). These hold until an FS-capable component
    /// delivers the real `system.conf` policy.
    fn default() -> Self {
        Self {
            ipv4_enabled: true,
            ipv6_enabled: true,
            syncookies_always: false,
            ipv6_privacy: false,
            tcp_keepalive: false,
        }
    }
}

/// One netstack-service operation.
///
/// Every request is one fixed-width frame; the service derives the
/// caller's authority from its kernel-attested origin, never from a
/// claimed field.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum NetstackRequest {
    /// List the managed interfaces' names (admin surface).
    InterfaceList,
    /// Assign a static address to a named interface (admin surface).
    AddrAdd {
        /// The interface's admin-chosen alias, NUL-padded.
        iface: [u8; IF_NAME_LEN],
        /// Address family of `addr`.
        family: NetAddrFamily,
        /// On-link prefix length (`1..=max_prefix`).
        prefix: u8,
        /// The address; V4 uses the first four bytes.
        addr: [u8; 16],
    },
    /// Add a route through a named interface (admin surface).
    RouteAdd {
        /// The interface's admin-chosen alias, NUL-padded.
        iface: [u8; IF_NAME_LEN],
        /// Address family of `dest` (and `next_hop` when present).
        family: NetAddrFamily,
        /// Destination prefix length (`0..=max_prefix`; 0 is the
        /// default route).
        prefix: u8,
        /// The destination prefix; V4 uses the first four bytes.
        dest: [u8; 16],
        /// The gateway, or `None` for an on-link route.
        next_hop: Option<[u8; 16]>,
    },
    /// Page the whole system's interface stack counters (sysinfo
    /// broker).
    InterfaceCounters {
        /// First interface index to report.
        offset: u32,
        /// Most records to return (`1..=NETSTACK_LIST_LIMIT_MAX`).
        limit: u16,
    },
    /// Page the whole system's interface facts (sysinfo broker).
    InterfaceFacts {
        /// First interface index to report.
        offset: u32,
        /// Most records to return (`1..=NETSTACK_LIST_LIMIT_MAX`).
        limit: u16,
    },
    /// Page the whole system's interface link/address state (sysinfo
    /// broker).
    InterfaceState {
        /// First interface index to report.
        offset: u32,
        /// Most records to return (`1..=NETSTACK_LIST_LIMIT_MAX`).
        limit: u16,
    },
    /// Page the whole system's live per-interface throughput rates over a
    /// caller-requested window (sysinfo broker). Each record reports the
    /// rates averaged over the window that *actually* elapsed, which may be
    /// shorter than `window` when an interface's history is younger.
    InterfaceRates {
        /// First interface index to report.
        offset: u32,
        /// Most records to return (`1..=NETSTACK_LIST_LIMIT_MAX`).
        limit: u16,
        /// The rate-averaging window the caller requests.
        window: Duration64,
    },
    /// Page the whole system's open sockets (sysinfo broker; the
    /// `ss`/`netstat` socket table). A system-wide diagnostic: the
    /// records name the owning process and every connection's peer
    /// address, gated `CAP_SYSINFO_GLOBAL` at the broker.
    Sockets {
        /// First socket index to report.
        offset: u32,
        /// Most records to return (`1..=NETSTACK_LIST_LIMIT_MAX`).
        limit: u16,
    },
    /// Page every bond interface's members and their live health (sysinfo
    /// broker; `info:net/<bond>/members`, `state:net/<bond>/active-member`,
    /// and per-member health). One [`NetBondMemberRecord`] per (bond,
    /// member) pair, flattened in interface-table order then configured
    /// member order. A system-wide topology-and-state view, gated
    /// `CAP_SYSINFO_GLOBAL` at the broker like the other `state:net`
    /// surfaces.
    BondMembers {
        /// First (bond, member) pair to report.
        offset: u32,
        /// Most records to return (`1..=NETSTACK_LIST_LIMIT_MAX`).
        limit: u16,
    },
    /// Bind a live NIC driver process's device channel to a new managed
    /// interface (admin surface).
    ///
    /// The device manager sends this after it has spawned a NIC driver
    /// process and observed the driver publish its device-channel
    /// rendezvous: the stack becomes the [`crate::driver::net_channel`]
    /// client of `endpoint_id`, sizes the frame region from the device's
    /// facts, attaches, and manages the interface under the admin-chosen
    /// alias `iface`. Every parameter the interface needs beyond these two
    /// — the device MAC, MTU, and offloads — the stack learns from the
    /// driver's `Facts` reply, and the IPv6 interface identifier and IPv4
    /// identification seed it derives itself; the caller names only *which*
    /// endpoint to bind and *what to call* the interface.
    BindDriver {
        /// The reserved device-channel call-endpoint id the NIC driver
        /// process bound (a [`crate::driver::net_channel::NET_CHANNEL_ENDPOINT_BASE`]
        /// block id); the stack `ipc_call`s it as the channel client.
        endpoint_id: u64,
        /// The interface's admin-chosen alias, NUL-padded.
        iface: [u8; IF_NAME_LEN],
        /// The stable hardware location of the NIC — the register-window
        /// base of the device manager's matched hardware-tree node — or
        /// `0` when none was resolved. The stack records it so a
        /// [`NetInterfaceConfigMsg`] may bind an admin alias to *this*
        /// physical device by [`NetInterfaceConfigMsg::match_node`],
        /// independent of MAC or discovery order (the `<iface>.match.node`
        /// key). Purely an identity the stack matches against; it is never
        /// treated as an address the stack may touch.
        node_location: u64,
    },
    /// Apply the stack-wide `net.*` policy (admin surface). Pushed once
    /// after the root unlock by an FS-capable component; the pure state
    /// mutation the dispatcher applies (family enable at socket `open`
    /// and interface auto-config, SYN-cookie mode at `listen`).
    ApplyNetworkSettings(NetworkSettings),
}

/// Wire operation discriminant of [`NetstackRequest::InterfaceList`].
const OP_IF_LIST: u16 = 1;
/// Wire operation discriminant of [`NetstackRequest::AddrAdd`].
const OP_ADDR_ADD: u16 = 2;
/// Wire operation discriminant of [`NetstackRequest::RouteAdd`].
const OP_ROUTE_ADD: u16 = 3;
/// Wire operation discriminant of [`NetstackRequest::InterfaceCounters`].
const OP_IF_COUNTERS: u16 = 4;
/// Wire operation discriminant of [`NetstackRequest::InterfaceFacts`].
const OP_IF_FACTS: u16 = 5;
/// Wire operation discriminant of [`NetstackRequest::InterfaceState`].
const OP_IF_STATE: u16 = 6;
/// Wire operation discriminant of [`NetstackRequest::BindDriver`].
const OP_BIND_DRIVER: u16 = 7;
/// Wire operation discriminant of [`NetstackRequest::InterfaceRates`].
const OP_IF_RATES: u16 = 8;
/// Wire operation discriminant of [`NetstackRequest::Sockets`].
const OP_SOCKETS: u16 = 9;
/// Wire operation discriminant of [`NetstackRequest::ApplyNetworkSettings`].
const OP_APPLY_NET_SETTINGS: u16 = 10;
/// Wire operation discriminant of [`NetstackRequest::BondMembers`].
const OP_BOND_MEMBERS: u16 = 11;

impl NetstackRequest {
    /// Encoded size on the wire: magic (4), version (2), op (2), and a
    /// 56-byte operation block whose unused tail must be zero.
    pub const WIRE_LEN: usize = 64;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, NETSTACK_REQUEST_MAGIC);
        put_u16(&mut out, 4, NETSTACK_VERSION_V1);
        match *self {
            Self::InterfaceList => {
                put_u16(&mut out, 6, OP_IF_LIST);
            }
            Self::AddrAdd {
                iface,
                family,
                prefix,
                addr,
            } => {
                put_u16(&mut out, 6, OP_ADDR_ADD);
                out[8..24].copy_from_slice(&iface);
                out[24] = family.as_u8();
                out[25] = prefix;
                out[26..42].copy_from_slice(&addr);
            }
            Self::RouteAdd {
                iface,
                family,
                prefix,
                dest,
                next_hop,
            } => {
                put_u16(&mut out, 6, OP_ROUTE_ADD);
                out[8..24].copy_from_slice(&iface);
                out[24] = family.as_u8();
                out[25] = prefix;
                out[26..42].copy_from_slice(&dest);
                if let Some(gateway) = next_hop {
                    out[42] = 1;
                    out[43..59].copy_from_slice(&gateway);
                }
            }
            Self::InterfaceCounters { offset, limit } => {
                put_u16(&mut out, 6, OP_IF_COUNTERS);
                put_u32(&mut out, 8, offset);
                put_u16(&mut out, 12, limit);
            }
            Self::InterfaceFacts { offset, limit } => {
                put_u16(&mut out, 6, OP_IF_FACTS);
                put_u32(&mut out, 8, offset);
                put_u16(&mut out, 12, limit);
            }
            Self::InterfaceState { offset, limit } => {
                put_u16(&mut out, 6, OP_IF_STATE);
                put_u32(&mut out, 8, offset);
                put_u16(&mut out, 12, limit);
            }
            Self::InterfaceRates {
                offset,
                limit,
                window,
            } => {
                put_u16(&mut out, 6, OP_IF_RATES);
                put_u32(&mut out, 8, offset);
                put_u16(&mut out, 12, limit);
                out[14..26].copy_from_slice(&window.to_le_bytes());
            }
            Self::Sockets { offset, limit } => {
                put_u16(&mut out, 6, OP_SOCKETS);
                put_u32(&mut out, 8, offset);
                put_u16(&mut out, 12, limit);
            }
            Self::BondMembers { offset, limit } => {
                put_u16(&mut out, 6, OP_BOND_MEMBERS);
                put_u32(&mut out, 8, offset);
                put_u16(&mut out, 12, limit);
            }
            Self::BindDriver {
                endpoint_id,
                iface,
                node_location,
            } => {
                put_u16(&mut out, 6, OP_BIND_DRIVER);
                put_u64(&mut out, 8, endpoint_id);
                out[16..32].copy_from_slice(&iface);
                put_u64(&mut out, 32, node_location);
            }
            Self::ApplyNetworkSettings(settings) => {
                put_u16(&mut out, 6, OP_APPLY_NET_SETTINGS);
                out[8] = u8::from(settings.ipv4_enabled);
                out[9] = u8::from(settings.ipv6_enabled);
                out[10] = u8::from(settings.syncookies_always);
                out[11] = u8::from(settings.ipv6_privacy);
                out[12] = u8::from(settings.tcp_keepalive);
            }
        }
        out
    }

    /// Decode from `bytes`, failing closed on any malformed input.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `bytes` cannot hold a whole
    ///   request.
    /// * [`Errno::BadMagic`] — wrong magic or a dirty reserved tail.
    /// * [`Errno::AbiVersionUnsupported`] — not `netstack-v1`.
    /// * [`Errno::OutOfRange`] — an unknown operation or family, an
    ///   invalid interface name, or a prefix outside the family's
    ///   bounds.
    /// * [`Errno::LengthOutOfRange`] — a paging limit outside
    ///   `1..=`[`NETSTACK_LIST_LIMIT_MAX`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if read_u32(bytes, 0) != NETSTACK_REQUEST_MAGIC {
            return Err(Errno::BadMagic);
        }
        if read_u16(bytes, 4) != NETSTACK_VERSION_V1 {
            return Err(Errno::AbiVersionUnsupported);
        }
        let op = read_u16(bytes, 6);
        match op {
            OP_IF_LIST => {
                reserved_zero(bytes, 8)?;
                Ok(Self::InterfaceList)
            }
            OP_ADDR_ADD => {
                reserved_zero(bytes, 42)?;
                let iface = if_name(bytes)?;
                let family = NetAddrFamily::from_u8(bytes[24])?;
                let prefix = bytes[25];
                if prefix == 0 || prefix > family.max_prefix() {
                    return Err(Errno::OutOfRange);
                }
                let addr = address(bytes, 26, family)?;
                Ok(Self::AddrAdd {
                    iface,
                    family,
                    prefix,
                    addr,
                })
            }
            OP_ROUTE_ADD => {
                reserved_zero(bytes, 59)?;
                let iface = if_name(bytes)?;
                let family = NetAddrFamily::from_u8(bytes[24])?;
                let prefix = bytes[25];
                if prefix > family.max_prefix() {
                    return Err(Errno::OutOfRange);
                }
                let dest = address(bytes, 26, family)?;
                let next_hop = match bytes[42] {
                    0 => {
                        if bytes[43..59].iter().any(|&b| b != 0) {
                            return Err(Errno::BadMagic);
                        }
                        None
                    }
                    1 => Some(address(bytes, 43, family)?),
                    _ => return Err(Errno::OutOfRange),
                };
                Ok(Self::RouteAdd {
                    iface,
                    family,
                    prefix,
                    dest,
                    next_hop,
                })
            }
            OP_IF_FACTS | OP_IF_STATE | OP_IF_COUNTERS => {
                reserved_zero(bytes, 14)?;
                let (offset, limit) = paged_offset_limit(bytes)?;
                Ok(match op {
                    OP_IF_FACTS => Self::InterfaceFacts { offset, limit },
                    OP_IF_STATE => Self::InterfaceState { offset, limit },
                    _ => Self::InterfaceCounters { offset, limit },
                })
            }
            OP_IF_RATES => {
                reserved_zero(bytes, 26)?;
                let (offset, limit) = paged_offset_limit(bytes)?;
                let window = Duration64::from_bytes(&bytes[14..26])?;
                Ok(Self::InterfaceRates {
                    offset,
                    limit,
                    window,
                })
            }
            OP_SOCKETS => {
                reserved_zero(bytes, 14)?;
                let (offset, limit) = paged_offset_limit(bytes)?;
                Ok(Self::Sockets { offset, limit })
            }
            OP_BOND_MEMBERS => {
                reserved_zero(bytes, 14)?;
                let (offset, limit) = paged_offset_limit(bytes)?;
                Ok(Self::BondMembers { offset, limit })
            }
            OP_BIND_DRIVER => decode_bind_driver(bytes),
            OP_APPLY_NET_SETTINGS => Ok(Self::ApplyNetworkSettings(decode_settings(bytes)?)),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// Read and validate the interface-name field at bytes `8..24`.
fn if_name(bytes: &[u8]) -> Result<[u8; IF_NAME_LEN], Errno> {
    let mut name = [0u8; IF_NAME_LEN];
    name.copy_from_slice(&bytes[8..24]);
    validate_if_name(&name)?;
    Ok(name)
}

/// Read an address field, refusing a V4 address with a dirty tail.
fn address(bytes: &[u8], from: usize, family: NetAddrFamily) -> Result<[u8; 16], Errno> {
    let mut addr = [0u8; 16];
    addr.copy_from_slice(&bytes[from..from + 16]);
    if family == NetAddrFamily::V4 && addr[4..].iter().any(|&b| b != 0) {
        return Err(Errno::BadMagic);
    }
    Ok(addr)
}

/// Read and bounds-check the paging `offset` (u32 at 8) and `limit`
/// (u16 at 12) shared by every paged list/rates/sockets request.
///
/// A `limit` of zero or one above [`NETSTACK_LIST_LIMIT_MAX`] fails closed
/// with [`Errno::LengthOutOfRange`]. The reserved-tail check differs per
/// operation (the rates request carries a window past the limit), so it
/// stays in each arm.
fn paged_offset_limit(bytes: &[u8]) -> Result<(u32, u16), Errno> {
    let offset = read_u32(bytes, 8);
    let limit = read_u16(bytes, 12);
    if limit == 0 || limit > NETSTACK_LIST_LIMIT_MAX {
        return Err(Errno::LengthOutOfRange);
    }
    Ok((offset, limit))
}

/// Decode the [`NetstackRequest::BindDriver`] operation block: the
/// endpoint id (bytes 8..16), the interface name (16..32), and the NIC's
/// hardware location (32..40), with a zero reserved tail past it.
fn decode_bind_driver(bytes: &[u8]) -> Result<NetstackRequest, Errno> {
    reserved_zero(bytes, 40)?;
    let endpoint_id = read_u64(bytes, 8);
    let mut iface = [0u8; IF_NAME_LEN];
    iface.copy_from_slice(&bytes[16..32]);
    validate_if_name(&iface)?;
    let node_location = read_u64(bytes, 32);
    Ok(NetstackRequest::BindDriver {
        endpoint_id,
        iface,
        node_location,
    })
}

/// Decode the [`NetworkSettings`] operation block (five wire booleans at
/// bytes 8..13) and enforce its zero reserved tail.
fn decode_settings(bytes: &[u8]) -> Result<NetworkSettings, Errno> {
    reserved_zero(bytes, 13)?;
    Ok(NetworkSettings {
        ipv4_enabled: decode_bool(bytes[8])?,
        ipv6_enabled: decode_bool(bytes[9])?,
        syncookies_always: decode_bool(bytes[10])?,
        ipv6_privacy: decode_bool(bytes[11])?,
        tcp_keepalive: decode_bool(bytes[12])?,
    })
}

/// Decode a wire boolean: exactly `0` or `1`, failing closed on any
/// other byte (a smuggled non-boolean value is rejected, not truncated).
fn decode_bool(byte: u8) -> Result<bool, Errno> {
    match byte {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(Errno::OutOfRange),
    }
}

/// Refuse a request whose reserved tail (from `from` to the end of the
/// fixed frame) carries any non-zero byte.
fn reserved_zero(bytes: &[u8], from: usize) -> Result<(), Errno> {
    if bytes[from..NetstackRequest::WIRE_LEN]
        .iter()
        .any(|&b| b != 0)
    {
        return Err(Errno::BadMagic);
    }
    Ok(())
}

/// Magic number identifying a per-interface configuration message
/// (`"NIC1"` little-endian). Distinct from [`NETSTACK_REQUEST_MAGIC`] so
/// the transport can tell the two framed messages apart on the one admin
/// endpoint.
pub const NET_INTERFACE_CONFIG_MAGIC: u32 = u32::from_le_bytes(*b"NIC1");

/// The `net-iface-config-v1` message version.
pub const NET_INTERFACE_CONFIG_VERSION_V1: u16 = 1;

/// The IPv4 addressing an interface configuration requests
/// (`network.conf` `<iface>.ipv4.*`).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum NetIpv4Config {
    /// IPv4 is administratively disabled on this interface: it binds no
    /// IPv4 address and answers no IPv4/ARP.
    Disabled,
    /// A static IPv4 assignment.
    Static {
        /// The interface address (network byte order).
        addr: [u8; 4],
        /// The on-link prefix length, in bits (`1..=32`).
        prefix: u8,
        /// The optional default gateway (network byte order); the engine
        /// requires it to lie inside the connected subnet.
        gateway: Option<[u8; 4]>,
    },
}

/// The IPv6 addressing an interface configuration requests
/// (`network.conf` `<iface>.ipv6.*`).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum NetIpv6Config {
    /// IPv6 is administratively disabled on this interface: it forms no
    /// link-local and accepts no IPv6.
    Disabled,
    /// Stateless address autoconfiguration (RFC 4862): the interface forms
    /// its link-local and adopts advertised prefixes. A static address may
    /// still be assigned alongside SLAAC via [`Self::Static`].
    Slaac,
    /// A static IPv6 assignment (the interface additionally keeps its
    /// autoconfigured link-local; SLAAC and a static address coexist).
    Static {
        /// The interface address (network byte order).
        addr: [u8; 16],
        /// The on-link prefix length, in bits (`1..=128`).
        prefix: u8,
        /// The optional default-router next hop (network byte order); an
        /// IPv6 gateway is commonly link-local and need not lie inside the
        /// interface prefix, so it is validated only as a unicast address.
        gateway: Option<[u8; 16]>,
    },
}

/// Wire discriminant of [`NetIpv4Config::Disabled`].
const IPV4_METHOD_DISABLED: u8 = 0;
/// Wire discriminant of [`NetIpv4Config::Static`].
const IPV4_METHOD_STATIC: u8 = 1;
/// Wire discriminant of [`NetIpv6Config::Disabled`].
const IPV6_METHOD_DISABLED: u8 = 0;
/// Wire discriminant of [`NetIpv6Config::Slaac`].
const IPV6_METHOD_SLAAC: u8 = 1;
/// Wire discriminant of [`NetIpv6Config::Static`].
const IPV6_METHOD_STATIC: u8 = 2;

/// One managed interface's declarative configuration, pushed to
/// `netstack` post-unlock by the FS-capable device manager (which reads
/// `/System/Settings/Network/network.conf` through `lib/netconfig` — the
/// network-parsing sandbox holds no filesystem capability).
///
/// The interface is identified by its **stable hardware identity** — its
/// MAC — not by discovery order: `netstack` is the only component that
/// holds each interface's MAC (from the driver's `Facts`), so it matches a
/// configuration to the bound interface by [`Self::match_mac`] and renames
/// that interface to the admin-chosen [`Self::alias`]. A message with no
/// MAC selector matches an interface already bearing `alias` (the
/// apply-by-alias path).
///
/// The payload's address fields do not fit the 64-byte [`NetstackRequest`]
/// enum, so this is a **separate** self-identifying frame (its own
/// [`NET_INTERFACE_CONFIG_MAGIC`]), decoded by the service transport before
/// [`NetstackRequest`] — the `BindDriver`-interception precedent.
///
/// Every decode fails closed (unknown magic/version/method, an out-of-range
/// prefix, a present-flag that is not `0`/`1`, or a dirty reserved tail);
/// [`Self::validate`] additionally enforces the semantic invariants
/// (unicast addresses, an on-subnet IPv4 gateway) so the service can apply
/// the whole message atomically — a refusal leaves the interface untouched.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct NetInterfaceConfigMsg {
    /// The interface's admin-chosen alias, NUL-padded.
    pub alias: [u8; IF_NAME_LEN],
    /// The stable MAC identity to match, or `None` to match by `alias`.
    pub match_mac: Option<[u8; 6]>,
    /// The stable hardware-node location to match (a NIC's register-window
    /// base, the [`NetstackRequest::BindDriver::node_location`] the stack
    /// recorded), or `None` to match by [`Self::match_mac`]/`alias`. This
    /// is the `<iface>.match.node` binding: it names *which physical device*
    /// the alias belongs to by where it sits on the bus, independent of MAC
    /// or discovery order. Purely an identity to match; never an address
    /// the stack touches.
    pub match_node: Option<u64>,
    /// The requested IPv4 addressing.
    pub ipv4: NetIpv4Config,
    /// The requested IPv6 addressing.
    pub ipv6: NetIpv6Config,
    /// The link MTU override, or `0` to keep the device-reported MTU.
    pub mtu: u16,
}

impl NetInterfaceConfigMsg {
    /// Encoded size on the wire: magic (4), version (2), `mac_present`
    /// (1), reserved (1), alias (16), mac (6), ipv4 {method (1), prefix
    /// (1), addr (4), `gw_present` (1), gw (4)}, ipv6 {method (1), prefix
    /// (1), addr (16), `gw_present` (1), gw (16)}, mtu (2), `node_present`
    /// (1, byte 78), reserved (1, byte 79), `match_node` (8, bytes 80..88),
    /// and a zero reserved tail.
    pub const WIRE_LEN: usize = 96;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, NET_INTERFACE_CONFIG_MAGIC);
        put_u16(&mut out, 4, NET_INTERFACE_CONFIG_VERSION_V1);
        if let Some(mac) = self.match_mac {
            out[6] = 1;
            out[24..30].copy_from_slice(&mac);
        }
        out[8..24].copy_from_slice(&self.alias);
        match self.ipv4 {
            NetIpv4Config::Disabled => out[30] = IPV4_METHOD_DISABLED,
            NetIpv4Config::Static {
                addr,
                prefix,
                gateway,
            } => {
                out[30] = IPV4_METHOD_STATIC;
                out[31] = prefix;
                out[32..36].copy_from_slice(&addr);
                if let Some(gw) = gateway {
                    out[36] = 1;
                    out[37..41].copy_from_slice(&gw);
                }
            }
        }
        match self.ipv6 {
            NetIpv6Config::Disabled => out[41] = IPV6_METHOD_DISABLED,
            NetIpv6Config::Slaac => out[41] = IPV6_METHOD_SLAAC,
            NetIpv6Config::Static {
                addr,
                prefix,
                gateway,
            } => {
                out[41] = IPV6_METHOD_STATIC;
                out[42] = prefix;
                out[43..59].copy_from_slice(&addr);
                if let Some(gw) = gateway {
                    out[59] = 1;
                    out[60..76].copy_from_slice(&gw);
                }
            }
        }
        put_u16(&mut out, 76, self.mtu);
        if let Some(node) = self.match_node {
            out[78] = 1;
            put_u64(&mut out, 80, node);
        }
        out
    }

    /// Decode from `bytes`, failing closed on any malformed input.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `bytes` cannot hold a whole message.
    /// * [`Errno::BadMagic`] — wrong magic, a present-flag that is not
    ///   `0`/`1`, a set field behind a clear present-flag, or a dirty
    ///   reserved tail.
    /// * [`Errno::AbiVersionUnsupported`] — not `net-iface-config-v1`.
    /// * [`Errno::OutOfRange`] — an unknown method, an out-of-range or
    ///   zero static prefix, or an invalid alias.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if read_u32(bytes, 0) != NET_INTERFACE_CONFIG_MAGIC {
            return Err(Errno::BadMagic);
        }
        if read_u16(bytes, 4) != NET_INTERFACE_CONFIG_VERSION_V1 {
            return Err(Errno::AbiVersionUnsupported);
        }
        // Reserved bytes: the mac-present padding (7), the node-present
        // padding (79), and the final tail (88..96) must all be zero.
        if bytes[7] != 0 || bytes[79] != 0 || bytes[88..Self::WIRE_LEN].iter().any(|&b| b != 0) {
            return Err(Errno::BadMagic);
        }
        let match_node = match bytes[78] {
            0 => {
                if bytes[80..88].iter().any(|&b| b != 0) {
                    return Err(Errno::BadMagic);
                }
                None
            }
            1 => Some(read_u64(bytes, 80)),
            _ => return Err(Errno::OutOfRange),
        };
        let match_mac = match bytes[6] {
            0 => {
                if bytes[24..30].iter().any(|&b| b != 0) {
                    return Err(Errno::BadMagic);
                }
                None
            }
            1 => {
                let mut mac = [0u8; 6];
                mac.copy_from_slice(&bytes[24..30]);
                Some(mac)
            }
            _ => return Err(Errno::OutOfRange),
        };
        let mut alias = [0u8; IF_NAME_LEN];
        alias.copy_from_slice(&bytes[8..24]);
        validate_if_name(&alias)?;
        let ipv4 = decode_ipv4_config(bytes)?;
        let ipv6 = decode_ipv6_config(bytes)?;
        let mtu = read_u16(bytes, 76);
        Ok(Self {
            alias,
            match_mac,
            match_node,
            ipv4,
            ipv6,
            mtu,
        })
    }

    /// Enforce the semantic invariants the structural decode cannot, so the
    /// service can apply the whole message atomically.
    ///
    /// * A static IPv4/IPv6 address is a genuine **unicast** host address
    ///   (not unspecified, loopback, multicast, or — for v4 — broadcast).
    /// * A static IPv4 gateway lies inside the connected subnet (the engine
    ///   refuses an off-subnet v4 next hop that could never resolve
    ///   on-link). An IPv6 gateway is only required to be unicast: an IPv6
    ///   default router is routinely a link-local next hop outside the
    ///   interface prefix.
    /// * An MTU override is at least the IPv6 minimum link MTU (1280).
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] on any violated invariant (fail closed).
    pub fn validate(&self) -> Result<(), Errno> {
        if self.mtu != 0 && self.mtu < 1280 {
            return Err(Errno::OutOfRange);
        }
        if let NetIpv4Config::Static {
            addr,
            prefix,
            gateway,
        } = self.ipv4
        {
            let host = core::net::Ipv4Addr::from(addr);
            if host.is_unspecified()
                || host.is_loopback()
                || host.is_multicast()
                || host.is_broadcast()
            {
                return Err(Errno::OutOfRange);
            }
            if let Some(gw) = gateway {
                if !ipv4_same_subnet(addr, gw, prefix) {
                    return Err(Errno::OutOfRange);
                }
            }
        }
        if let NetIpv6Config::Static { addr, gateway, .. } = self.ipv6 {
            let host = core::net::Ipv6Addr::from(addr);
            if host.is_unspecified() || host.is_loopback() || host.is_multicast() {
                return Err(Errno::OutOfRange);
            }
            if let Some(gw) = gateway {
                let gw = core::net::Ipv6Addr::from(gw);
                if gw.is_unspecified() || gw.is_multicast() {
                    return Err(Errno::OutOfRange);
                }
            }
        }
        Ok(())
    }
}

/// Decode the IPv4 configuration block (bytes 30..41), enforcing the
/// disabled-method zero invariants and the `gw_present` flag.
fn decode_ipv4_config(bytes: &[u8]) -> Result<NetIpv4Config, Errno> {
    let prefix = bytes[31];
    let mut addr = [0u8; 4];
    addr.copy_from_slice(&bytes[32..36]);
    match bytes[30] {
        IPV4_METHOD_DISABLED => {
            if prefix != 0 || addr != [0; 4] || bytes[36..41].iter().any(|&b| b != 0) {
                return Err(Errno::BadMagic);
            }
            Ok(NetIpv4Config::Disabled)
        }
        IPV4_METHOD_STATIC => {
            if prefix == 0 || prefix > 32 {
                return Err(Errno::OutOfRange);
            }
            let gateway = decode_gateway_v4(bytes)?;
            Ok(NetIpv4Config::Static {
                addr,
                prefix,
                gateway,
            })
        }
        _ => Err(Errno::OutOfRange),
    }
}

/// Decode the IPv4 gateway (present flag at 36, address at 37..41).
fn decode_gateway_v4(bytes: &[u8]) -> Result<Option<[u8; 4]>, Errno> {
    match bytes[36] {
        0 => {
            if bytes[37..41].iter().any(|&b| b != 0) {
                return Err(Errno::BadMagic);
            }
            Ok(None)
        }
        1 => {
            let mut gw = [0u8; 4];
            gw.copy_from_slice(&bytes[37..41]);
            Ok(Some(gw))
        }
        _ => Err(Errno::OutOfRange),
    }
}

/// Decode the IPv6 configuration block (bytes 41..76), enforcing the
/// disabled/slaac zero invariants and the `gw_present` flag.
fn decode_ipv6_config(bytes: &[u8]) -> Result<NetIpv6Config, Errno> {
    let prefix = bytes[42];
    let mut addr = [0u8; 16];
    addr.copy_from_slice(&bytes[43..59]);
    let addressless = prefix == 0 && addr == [0; 16] && bytes[59..76].iter().all(|&b| b == 0);
    match bytes[41] {
        IPV6_METHOD_DISABLED => {
            if !addressless {
                return Err(Errno::BadMagic);
            }
            Ok(NetIpv6Config::Disabled)
        }
        IPV6_METHOD_SLAAC => {
            if !addressless {
                return Err(Errno::BadMagic);
            }
            Ok(NetIpv6Config::Slaac)
        }
        IPV6_METHOD_STATIC => {
            if prefix == 0 || prefix > 128 {
                return Err(Errno::OutOfRange);
            }
            let gateway = decode_gateway_v6(bytes)?;
            Ok(NetIpv6Config::Static {
                addr,
                prefix,
                gateway,
            })
        }
        _ => Err(Errno::OutOfRange),
    }
}

/// Decode the IPv6 gateway (present flag at 59, address at 60..76).
fn decode_gateway_v6(bytes: &[u8]) -> Result<Option<[u8; 16]>, Errno> {
    match bytes[59] {
        0 => {
            if bytes[60..76].iter().any(|&b| b != 0) {
                return Err(Errno::BadMagic);
            }
            Ok(None)
        }
        1 => {
            let mut gw = [0u8; 16];
            gw.copy_from_slice(&bytes[60..76]);
            Ok(Some(gw))
        }
        _ => Err(Errno::OutOfRange),
    }
}

/// Whether `gateway` lies in the same IPv4 subnet as `addr` at `prefix`
/// bits (`prefix` in `1..=32`, guaranteed by the decoder).
fn ipv4_same_subnet(addr: [u8; 4], gateway: [u8; 4], prefix: u8) -> bool {
    let host = u32::from_be_bytes(addr);
    let gw = u32::from_be_bytes(gateway);
    // `prefix` is 1..=32 here, so `32 - prefix` is 0..=31 and the shift is
    // in range; a /32 masks to the full address.
    let mask = if prefix == 32 {
        u32::MAX
    } else {
        u32::MAX << (32 - prefix)
    };
    host & mask == gw & mask
}

/// Largest number of member NICs a single bond aggregates, on the wire.
///
/// This is the [`NetBondConfigMsg`] frame's own capacity bound. It is the
/// single definition the `lib/net::bond` engine and the `lib/netconfig`
/// grammar both key their own `MAX_BOND_MEMBERS` to, so the wire, the
/// engine, and the configuration store can never disagree on the limit.
pub const NET_BOND_MAX_MEMBERS: usize = 8;

/// Magic number identifying a bond-configuration message (`"NBC1"`
/// little-endian). Distinct from [`NETSTACK_REQUEST_MAGIC`] and
/// [`NET_INTERFACE_CONFIG_MAGIC`] so the transport can tell all three
/// framed messages apart on the one admin endpoint.
pub const NET_BOND_CONFIG_MAGIC: u32 = u32::from_le_bytes(*b"NBC1");

/// The `net-bond-config-v1` message version.
pub const NET_BOND_CONFIG_VERSION_V1: u16 = 1;

/// A bond's transmit policy, on the wire (`plans/NETWORK.md` §6.3). A
/// closed set mirroring the `lib/net::bond::BondMode` engine policy and the
/// `lib/netconfig` grammar; LACP/802.3ad is a future in-place extension.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum NetBondMode {
    /// One transmitting member at a time with ordered failover.
    ActiveBackup = 0,
    /// Flow-hashed transmit spread across the eligible members.
    Balance = 1,
}

impl NetBondMode {
    /// The wire value for this mode.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Recover a mode from its wire value.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] if `value` names no mode (fail closed).
    pub const fn from_u8(value: u8) -> Result<Self, Errno> {
        match value {
            0 => Ok(Self::ActiveBackup),
            1 => Ok(Self::Balance),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// One bond (link-aggregation) interface's declarative configuration,
/// pushed to `netstack` post-unlock by the FS-capable device manager
/// (which reads `/System/Settings/Network/network.conf` through
/// `lib/netconfig` — the network-parsing sandbox holds no filesystem
/// capability). The `plans/NETWORK.md` §6.3 companion of
/// [`NetInterfaceConfigMsg`]: it names the bond's members, transmit
/// policy, failover monitor interval, and optional primary member. The
/// bond's own addressing is applied separately, through a
/// [`NetInterfaceConfigMsg`] naming the bond by its [`Self::alias`].
///
/// The members are named by their admin-chosen aliases (the member
/// interfaces must already be renamed to those aliases by their own
/// [`NetInterfaceConfigMsg`]s); the bond composes them by name, never by
/// discovery order.
///
/// Like [`NetInterfaceConfigMsg`] this is a **separate** self-identifying
/// frame (its own [`NET_BOND_CONFIG_MAGIC`]), decoded by the service
/// transport before [`NetstackRequest`]. Every decode fails closed
/// (unknown magic/version/mode, a bad present-flag, an out-of-range member
/// count, an invalid alias, or a dirty reserved region); [`Self::validate`]
/// additionally enforces the semantic invariants (at least two members, no
/// duplicate members, a primary that is one of the members, a positive
/// monitor interval) so the service can apply the whole message atomically.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct NetBondConfigMsg {
    /// The bond interface's admin-chosen alias, NUL-padded.
    pub alias: [u8; IF_NAME_LEN],
    /// The transmit policy.
    pub mode: NetBondMode,
    /// The failover health-monitor interval (the anti-flap up-delay a
    /// recovered member must stay up for before readmission).
    pub monitor_interval: Duration64,
    /// The primary member's alias that reclaims the transmit path whenever
    /// eligible, or `None` to leave the current active in place until it
    /// fails.
    pub primary: Option<[u8; IF_NAME_LEN]>,
    /// The member interfaces' admin-chosen aliases (the first
    /// [`Self::member_count`] entries are significant, NUL-padded).
    pub members: [[u8; IF_NAME_LEN]; NET_BOND_MAX_MEMBERS],
    /// The number of significant entries in [`Self::members`]
    /// (`2..=NET_BOND_MAX_MEMBERS`).
    pub member_count: u8,
}

impl NetBondConfigMsg {
    /// Encoded size on the wire: magic (4), version (2), mode (1),
    /// `primary_present` (1), alias (16), `monitor_interval`
    /// (`Duration64`), primary (16), `member_count` (1), reserved (3),
    /// and the member-alias table.
    pub const WIRE_LEN: usize = 56 + NET_BOND_MAX_MEMBERS * IF_NAME_LEN;

    /// Byte offset of the member-alias table.
    const MEMBERS_OFF: usize = 56;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, NET_BOND_CONFIG_MAGIC);
        put_u16(&mut out, 4, NET_BOND_CONFIG_VERSION_V1);
        out[6] = self.mode.as_u8();
        if let Some(primary) = self.primary {
            out[7] = 1;
            out[36..52].copy_from_slice(&primary);
        }
        out[8..24].copy_from_slice(&self.alias);
        out[24..36].copy_from_slice(&self.monitor_interval.to_le_bytes());
        out[52] = self.member_count;
        for (index, member) in self.members.iter().enumerate() {
            let base = Self::MEMBERS_OFF + index * IF_NAME_LEN;
            out[base..base + IF_NAME_LEN].copy_from_slice(member);
        }
        out
    }

    /// Decode from `bytes`, failing closed on any malformed input.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `bytes` cannot hold a whole message.
    /// * [`Errno::BadMagic`] — wrong magic, a present-flag that is not
    ///   `0`/`1`, a set primary behind a clear present-flag, or a dirty
    ///   reserved region.
    /// * [`Errno::AbiVersionUnsupported`] — not `net-bond-config-v1`.
    /// * [`Errno::OutOfRange`] — an unknown mode, an out-of-range member
    ///   count, or an invalid alias/member/primary name.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if read_u32(bytes, 0) != NET_BOND_CONFIG_MAGIC {
            return Err(Errno::BadMagic);
        }
        if read_u16(bytes, 4) != NET_BOND_CONFIG_VERSION_V1 {
            return Err(Errno::AbiVersionUnsupported);
        }
        // Reserved region after member_count must be zero.
        if bytes[53..56].iter().any(|&b| b != 0) {
            return Err(Errno::BadMagic);
        }
        let mode = NetBondMode::from_u8(bytes[6])?;
        let mut alias = [0u8; IF_NAME_LEN];
        alias.copy_from_slice(&bytes[8..24]);
        validate_if_name(&alias)?;
        let monitor_interval = Duration64::from_bytes(&bytes[24..36])?;
        let primary = match bytes[7] {
            0 => {
                if bytes[36..52].iter().any(|&b| b != 0) {
                    return Err(Errno::BadMagic);
                }
                None
            }
            1 => {
                let mut name = [0u8; IF_NAME_LEN];
                name.copy_from_slice(&bytes[36..52]);
                validate_if_name(&name)?;
                Some(name)
            }
            _ => return Err(Errno::OutOfRange),
        };
        let member_count = bytes[52];
        if (member_count as usize) > NET_BOND_MAX_MEMBERS {
            return Err(Errno::OutOfRange);
        }
        let mut members = [[0u8; IF_NAME_LEN]; NET_BOND_MAX_MEMBERS];
        for (index, member) in members.iter_mut().enumerate() {
            let base = Self::MEMBERS_OFF + index * IF_NAME_LEN;
            member.copy_from_slice(&bytes[base..base + IF_NAME_LEN]);
            if index < member_count as usize {
                // A significant member must be a valid alias.
                validate_if_name(member)?;
            } else if member.iter().any(|&b| b != 0) {
                // An insignificant slot must be zero (fail closed).
                return Err(Errno::BadMagic);
            }
        }
        Ok(Self {
            alias,
            mode,
            monitor_interval,
            primary,
            members,
            member_count,
        })
    }

    /// The significant member aliases (`&self.members[..member_count]`).
    #[must_use]
    pub fn members(&self) -> &[[u8; IF_NAME_LEN]] {
        &self.members[..self.member_count as usize]
    }

    /// Enforce the semantic invariants the structural decode cannot, so the
    /// service can apply the whole bond atomically.
    ///
    /// * At least two members (a one-member "bond" is not aggregation).
    /// * No duplicate member alias.
    /// * A declared primary is one of the members.
    /// * A positive monitor interval (a zero anti-flap up-delay would admit
    ///   a flapping member instantly).
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] on any violated invariant (fail closed).
    pub fn validate(&self) -> Result<(), Errno> {
        if (self.member_count as usize) < 2 {
            return Err(Errno::OutOfRange);
        }
        let members = self.members();
        for (index, member) in members.iter().enumerate() {
            if members[index + 1..].iter().any(|other| other == member) {
                return Err(Errno::OutOfRange);
            }
        }
        if let Some(primary) = self.primary {
            if !members.contains(&primary) {
                return Err(Errno::OutOfRange);
            }
        }
        if self.monitor_interval.secs() < 0
            || (self.monitor_interval.secs() == 0 && self.monitor_interval.subsec_nanos() == 0)
        {
            return Err(Errno::OutOfRange);
        }
        Ok(())
    }
}

/// The kind of link an interface record describes.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum NetIfKind {
    /// A wired Ethernet-framed device.
    Ethernet = 0,
    /// The stack's own loopback interface.
    Loopback = 1,
    /// A link-aggregation (bond) virtual interface composed over two or
    /// more member NICs (`plans/NETWORK.md` §6.3). It owns the addresses,
    /// routes, and neighbour caches; its members carry none.
    Bond = 2,
}

impl NetIfKind {
    /// The wire value for this kind.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Recover a kind from its wire value.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] if `value` names no kind (fail closed).
    pub const fn from_u8(value: u8) -> Result<Self, Errno> {
        match value {
            0 => Ok(Self::Ethernet),
            1 => Ok(Self::Loopback),
            2 => Ok(Self::Bond),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// One interface's static facts (`info:net/<iface>/…`, plan §5): the
/// response record of the interface-facts page.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct NetInterfaceFactsRecord {
    /// The interface's admin-chosen alias, NUL-padded.
    pub name: [u8; IF_NAME_LEN],
    /// The link kind.
    pub kind: NetIfKind,
    /// The device's 48-bit link-layer address. Sensitive: the sysinfo
    /// broker gates the query exposing it on `CAP_SYSINFO_HW`.
    pub mac: [u8; 6],
    /// Link MTU in bytes.
    pub mtu: u32,
    /// The negotiated offload set
    /// ([`NetOffloads`](crate::driver::net::NetOffloads) bits).
    pub offloads: u32,
    /// Receive-queue count (at least 1).
    pub rx_queues: u16,
}

impl NetInterfaceFactsRecord {
    /// Encoded size: name (16), kind (1), mac (6), reserved (1),
    /// mtu (4), offloads (4), `rx_queues` (2), reserved (2).
    pub const WIRE_LEN: usize = 36;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        out[..16].copy_from_slice(&self.name);
        out[16] = self.kind.as_u8();
        out[17..23].copy_from_slice(&self.mac);
        put_u32(&mut out, 24, self.mtu);
        put_u32(&mut out, 28, self.offloads);
        put_u16(&mut out, 32, self.rx_queues);
        out
    }

    /// Decode from `bytes`, failing closed on any malformed record.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `bytes` cannot hold a record.
    /// * [`Errno::BadMagic`] — a dirty reserved byte.
    /// * [`Errno::OutOfRange`] — an invalid name, kind, offload set,
    ///   MTU, or a zero receive-queue count.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if bytes[23] != 0 || bytes[34] != 0 || bytes[35] != 0 {
            return Err(Errno::BadMagic);
        }
        let mut name = [0u8; IF_NAME_LEN];
        name.copy_from_slice(&bytes[..16]);
        validate_if_name(&name)?;
        let kind = NetIfKind::from_u8(bytes[16])?;
        let mut mac = [0u8; 6];
        mac.copy_from_slice(&bytes[17..23]);
        let mtu = read_u32(bytes, 24);
        let offloads = read_u32(bytes, 28);
        crate::driver::net::NetOffloads::from_bits(offloads)?;
        let rx_queues = read_u16(bytes, 32);
        if !(crate::driver::net::DeviceFacts::MIN_MTU..=crate::driver::net::DeviceFacts::MAX_MTU)
            .contains(&mtu)
            || rx_queues == 0
        {
            return Err(Errno::OutOfRange);
        }
        Ok(Self {
            name,
            kind,
            mac,
            mtu,
            offloads,
            rx_queues,
        })
    }
}

/// Address assignment state carried in a state record.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum NetAddrState {
    /// Assigned and usable.
    Preferred = 0,
    /// Undergoing duplicate address detection.
    Tentative = 1,
    /// Valid but past its preferred lifetime.
    Deprecated = 2,
}

impl NetAddrState {
    /// The wire value for this state.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Recover a state from its wire value.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] if `value` names no state (fail closed).
    pub const fn from_u8(value: u8) -> Result<Self, Errno> {
        match value {
            0 => Ok(Self::Preferred),
            1 => Ok(Self::Tentative),
            2 => Ok(Self::Deprecated),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// One bound address inside a state record.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct NetIfAddr {
    /// Address family.
    pub family: NetAddrFamily,
    /// On-link prefix length.
    pub prefix: u8,
    /// Assignment state.
    pub state: NetAddrState,
    /// The address; V4 uses the first four bytes.
    pub addr: [u8; 16],
}

impl NetIfAddr {
    /// Encoded size: family (1), prefix (1), state (1), reserved (1),
    /// address (16).
    pub const WIRE_LEN: usize = 20;

    fn write(&self, out: &mut [u8]) {
        out[0] = self.family.as_u8();
        out[1] = self.prefix;
        out[2] = self.state.as_u8();
        out[4..20].copy_from_slice(&self.addr);
    }

    fn read(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes[3] != 0 {
            return Err(Errno::BadMagic);
        }
        let family = NetAddrFamily::from_u8(bytes[0])?;
        let prefix = bytes[1];
        if prefix > family.max_prefix() {
            return Err(Errno::OutOfRange);
        }
        let state = NetAddrState::from_u8(bytes[2])?;
        let mut addr = [0u8; 16];
        addr.copy_from_slice(&bytes[4..20]);
        if family == NetAddrFamily::V4 && addr[4..].iter().any(|&b| b != 0) {
            return Err(Errno::BadMagic);
        }
        Ok(Self {
            family,
            prefix,
            state,
            addr,
        })
    }
}

/// Most addresses one state record reports — a reply-size validation
/// bound; an interface with more reports its first
/// [`NET_IF_MAX_ADDRS`] (the engine's own table is separately bounded).
pub const NET_IF_MAX_ADDRS: usize = 8;

/// One interface's live link/address state (`state:net/<iface>/…`,
/// plan §5): the response record of the interface-state page.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct NetInterfaceStateRecord {
    /// The interface's admin-chosen alias, NUL-padded.
    pub name: [u8; IF_NAME_LEN],
    /// Whether the link carries frames.
    pub link_up: bool,
    /// How many entries of `addrs` are significant.
    pub addr_count: u8,
    /// The bound addresses; entries past `addr_count` must be zero.
    pub addrs: [NetIfAddr; NET_IF_MAX_ADDRS],
}

impl NetInterfaceStateRecord {
    /// Encoded size: name (16), link (1), count (1), reserved (2),
    /// addresses (8 × 20).
    pub const WIRE_LEN: usize = 20 + NET_IF_MAX_ADDRS * NetIfAddr::WIRE_LEN;

    /// A zeroed address slot (the padding value past `addr_count`).
    pub const EMPTY_ADDR: NetIfAddr = NetIfAddr {
        family: NetAddrFamily::V4,
        prefix: 0,
        state: NetAddrState::Preferred,
        addr: [0; 16],
    };

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        out[..16].copy_from_slice(&self.name);
        out[16] = u8::from(self.link_up);
        out[17] = self.addr_count;
        for (index, addr) in self.addrs.iter().enumerate().take(self.addr_count as usize) {
            addr.write(&mut out[20 + index * NetIfAddr::WIRE_LEN..]);
        }
        out
    }

    /// Decode from `bytes`, failing closed on any malformed record.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `bytes` cannot hold a record.
    /// * [`Errno::BadMagic`] — a dirty reserved byte, or a non-zero
    ///   address slot past `addr_count`.
    /// * [`Errno::OutOfRange`] — an invalid name, link flag, count,
    ///   family, prefix, or address state.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if bytes[18] != 0 || bytes[19] != 0 {
            return Err(Errno::BadMagic);
        }
        let mut name = [0u8; IF_NAME_LEN];
        name.copy_from_slice(&bytes[..16]);
        validate_if_name(&name)?;
        let link_up = match bytes[16] {
            0 => false,
            1 => true,
            _ => return Err(Errno::OutOfRange),
        };
        let addr_count = bytes[17];
        if addr_count as usize > NET_IF_MAX_ADDRS {
            return Err(Errno::OutOfRange);
        }
        let mut addrs = [Self::EMPTY_ADDR; NET_IF_MAX_ADDRS];
        for (index, slot) in addrs.iter_mut().enumerate() {
            let field =
                &bytes[20 + index * NetIfAddr::WIRE_LEN..20 + (index + 1) * NetIfAddr::WIRE_LEN];
            if index < addr_count as usize {
                *slot = NetIfAddr::read(field)?;
            } else if field.iter().any(|&b| b != 0) {
                return Err(Errno::BadMagic);
            }
        }
        Ok(Self {
            name,
            link_up,
            addr_count,
            addrs,
        })
    }
}

/// One interface's monotonic stack counters (the counter payload of a
/// [`NetInterfaceCountersRecord`]; `stats:net`, plan §5).
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct NetCounters {
    /// Frames received from the device.
    pub rx_frames: u64,
    /// Bytes received from the device (every received frame's whole
    /// Ethernet length, dropped frames included).
    pub rx_bytes: u64,
    /// Received frames dropped by validation or lack of a handler.
    pub rx_dropped: u64,
    /// Frames emitted for transmission.
    pub tx_frames: u64,
    /// Bytes emitted for transmission (every emitted frame's whole
    /// Ethernet length).
    pub tx_bytes: u64,
    /// ICMP/`ICMPv6` errors emitted.
    pub icmp_errors_sent: u64,
    /// ICMP/`ICMPv6` errors suppressed by the rate limiter.
    pub icmp_errors_suppressed: u64,
    /// Reassemblies expired incomplete.
    pub reassembly_expired: u64,
    /// Packets dropped from the pending-resolution queue.
    pub pending_dropped: u64,
}

/// Number of `u64` counters in a [`NetCounters`].
const COUNTERS_FIELDS: usize = 9;

impl NetCounters {
    /// Encoded payload size: nine little-endian `u64` counters.
    pub const WIRE_LEN: usize = COUNTERS_FIELDS * 8;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        for (index, value) in [
            self.rx_frames,
            self.rx_bytes,
            self.rx_dropped,
            self.tx_frames,
            self.tx_bytes,
            self.icmp_errors_sent,
            self.icmp_errors_suppressed,
            self.reassembly_expired,
            self.pending_dropped,
        ]
        .into_iter()
        .enumerate()
        {
            put_u64(&mut out, index * 8, value);
        }
        out
    }

    /// Decode from `bytes`.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] — `bytes` cannot hold the payload.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        Ok(Self {
            rx_frames: read_u64(bytes, 0),
            rx_bytes: read_u64(bytes, 8),
            rx_dropped: read_u64(bytes, 16),
            tx_frames: read_u64(bytes, 24),
            tx_bytes: read_u64(bytes, 32),
            icmp_errors_sent: read_u64(bytes, 40),
            icmp_errors_suppressed: read_u64(bytes, 48),
            reassembly_expired: read_u64(bytes, 56),
            pending_dropped: read_u64(bytes, 64),
        })
    }
}

/// One interface's stack counters keyed by its name
/// (`stats:net/<iface>/…`, plan §5): the response record of the
/// interface-counters page.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct NetInterfaceCountersRecord {
    /// The interface's admin-chosen alias, NUL-padded.
    pub name: [u8; IF_NAME_LEN],
    /// The interface's monotonic stack counters.
    pub counters: NetCounters,
}

impl NetInterfaceCountersRecord {
    /// Encoded size: the name (16) followed by the counter payload.
    pub const WIRE_LEN: usize = IF_NAME_LEN + NetCounters::WIRE_LEN;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        out[..IF_NAME_LEN].copy_from_slice(&self.name);
        out[IF_NAME_LEN..].copy_from_slice(&self.counters.to_le_bytes());
        out
    }

    /// Decode from `bytes`, failing closed on a malformed record.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `bytes` cannot hold a record.
    /// * [`Errno::OutOfRange`] — an invalid interface name.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let mut name = [0u8; IF_NAME_LEN];
        name.copy_from_slice(&bytes[..IF_NAME_LEN]);
        validate_if_name(&name)?;
        let counters = NetCounters::from_bytes(&bytes[IF_NAME_LEN..])?;
        Ok(Self { name, counters })
    }
}

/// One interface's live throughput rates keyed by its name
/// (`stats:net/<iface>/{rx,tx}.{pps,bps}`, plan §5): the response record of
/// the interface-rates page.
///
/// Each rate is an average over [`window`](Self::window) — the span that
/// *actually* elapsed, which may be shorter than the caller requested when
/// the interface's history is younger. A [`Duration64::ZERO`] window means
/// there was no usable baseline yet and every rate is `0`.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct NetInterfaceRatesRecord {
    /// The interface's admin-chosen alias, NUL-padded.
    pub name: [u8; IF_NAME_LEN],
    /// The span the rates were averaged over.
    pub window: Duration64,
    /// Received packets per second.
    pub rx_pps: u64,
    /// Received bits per second.
    pub rx_bps: u64,
    /// Transmitted packets per second.
    pub tx_pps: u64,
    /// Transmitted bits per second.
    pub tx_bps: u64,
}

impl NetInterfaceRatesRecord {
    /// Encoded size: the name (16), the window (12), then four
    /// little-endian `u64` rates.
    pub const WIRE_LEN: usize = IF_NAME_LEN + Duration64::WIRE_LEN + 4 * 8;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        out[..IF_NAME_LEN].copy_from_slice(&self.name);
        out[IF_NAME_LEN..IF_NAME_LEN + Duration64::WIRE_LEN]
            .copy_from_slice(&self.window.to_le_bytes());
        let base = IF_NAME_LEN + Duration64::WIRE_LEN;
        put_u64(&mut out, base, self.rx_pps);
        put_u64(&mut out, base + 8, self.rx_bps);
        put_u64(&mut out, base + 16, self.tx_pps);
        put_u64(&mut out, base + 24, self.tx_bps);
        out
    }

    /// Decode from `bytes`, failing closed on a malformed record.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `bytes` cannot hold a record.
    /// * [`Errno::OutOfRange`] — an invalid interface name.
    /// * [`Errno::TimestampOutOfRange`] — a non-canonical window.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let mut name = [0u8; IF_NAME_LEN];
        name.copy_from_slice(&bytes[..IF_NAME_LEN]);
        validate_if_name(&name)?;
        let window =
            Duration64::from_bytes(&bytes[IF_NAME_LEN..IF_NAME_LEN + Duration64::WIRE_LEN])?;
        let base = IF_NAME_LEN + Duration64::WIRE_LEN;
        Ok(Self {
            name,
            window,
            rx_pps: read_u64(bytes, base),
            rx_bps: read_u64(bytes, base + 8),
            tx_pps: read_u64(bytes, base + 16),
            tx_bps: read_u64(bytes, base + 24),
        })
    }
}

/// The transport protocol of a listed socket, as its IANA protocol
/// number so the wire value is a stable, well-known constant rather than
/// a private discriminant.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum NetSockProto {
    /// ICMP (an IPv4 [`crate::net::SocketType::IcmpEcho`] socket).
    Icmp = 1,
    /// TCP (a [`crate::net::SocketType::Stream`] socket).
    Tcp = 6,
    /// UDP (a [`crate::net::SocketType::Datagram`] socket).
    Udp = 17,
    /// `ICMPv6` (an IPv6 [`crate::net::SocketType::IcmpEcho`] socket).
    Icmpv6 = 58,
}

impl NetSockProto {
    /// The IANA protocol number.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Decode from its IANA protocol number, failing closed.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] — not a protocol the socket layer lists.
    pub const fn from_u8(value: u8) -> Result<Self, Errno> {
        match value {
            1 => Ok(Self::Icmp),
            6 => Ok(Self::Tcp),
            17 => Ok(Self::Udp),
            58 => Ok(Self::Icmpv6),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// The observable state of a listed socket.
///
/// A TCP socket reports its RFC 9293 connection state; a UDP socket
/// reports [`Unconnected`](Self::Unconnected) until a default peer is
/// set with `connect`, then [`Established`](Self::Established) — the
/// same `UNCONN`/`ESTAB` vocabulary `ss` uses, so one column serves both
/// transports.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum NetSockState {
    /// No connection (a closed TCP socket).
    Closed = 0,
    /// A passive TCP socket awaiting connections.
    Listen = 1,
    /// An active TCP open whose SYN is unacknowledged.
    SynSent = 2,
    /// A passive TCP open that has received a SYN.
    SynReceived = 3,
    /// An open TCP connection carrying data.
    Established = 4,
    /// Local close sent; awaiting the peer's ACK and FIN.
    FinWait1 = 5,
    /// Local close acknowledged; awaiting the peer's FIN.
    FinWait2 = 6,
    /// Peer close received; the local side may still send.
    CloseWait = 7,
    /// Simultaneous close in progress.
    Closing = 8,
    /// Local close after a peer close; awaiting the final ACK.
    LastAck = 9,
    /// Orderly close complete; holding down for stray segments.
    TimeWait = 10,
    /// A UDP socket with no default peer (`ss`'s `UNCONN`).
    Unconnected = 11,
}

impl NetSockState {
    /// The wire discriminant.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Decode from its wire discriminant, failing closed.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] — not a recognised socket state.
    pub const fn from_u8(value: u8) -> Result<Self, Errno> {
        match value {
            0 => Ok(Self::Closed),
            1 => Ok(Self::Listen),
            2 => Ok(Self::SynSent),
            3 => Ok(Self::SynReceived),
            4 => Ok(Self::Established),
            5 => Ok(Self::FinWait1),
            6 => Ok(Self::FinWait2),
            7 => Ok(Self::CloseWait),
            8 => Ok(Self::Closing),
            9 => Ok(Self::LastAck),
            10 => Ok(Self::TimeWait),
            11 => Ok(Self::Unconnected),
            _ => Err(Errno::OutOfRange),
        }
    }

    /// The short upper-case label `ss` prints for this state.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Closed => "CLOSED",
            Self::Listen => "LISTEN",
            Self::SynSent => "SYN-SENT",
            Self::SynReceived => "SYN-RECV",
            Self::Established => "ESTAB",
            Self::FinWait1 => "FIN-WAIT-1",
            Self::FinWait2 => "FIN-WAIT-2",
            Self::CloseWait => "CLOSE-WAIT",
            Self::Closing => "CLOSING",
            Self::LastAck => "LAST-ACK",
            Self::TimeWait => "TIME-WAIT",
            Self::Unconnected => "UNCONN",
        }
    }
}

/// One open socket the stack owns (the response record of the
/// [`NetstackRequest::Sockets`] page; the `ss`/`netstat` socket table,
/// plan §5). A system-wide diagnostic exposed only under
/// [`CapabilityId::SYSINFO_GLOBAL`](crate::CapabilityId::SYSINFO_GLOBAL):
/// it names the owning process and the peer address of every connection,
/// so it is never open by default.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct NetSocketRecord {
    /// The socket's transport protocol.
    pub proto: NetSockProto,
    /// The socket's observable state.
    pub state: NetSockState,
    /// Address family of the local (and, when connected, peer) address.
    pub family: NetAddrFamily,
    /// The local address; V4 uses the first four bytes, all-zero means
    /// the unspecified "any" address.
    pub local_addr: [u8; 16],
    /// The bound local port; `0` means unbound.
    pub local_port: u16,
    /// The peer address; all-zero for an unconnected socket.
    pub peer_addr: [u8; 16],
    /// The peer port; `0` for an unconnected socket.
    pub peer_port: u16,
    /// The unforgeable process instance that owns the socket.
    pub owner: u64,
    /// Bytes of in-order received data buffered for the owner to read
    /// (`ss`'s `Recv-Q`).
    pub recv_q: u64,
    /// Bytes queued for transmission not yet acknowledged (`ss`'s
    /// `Send-Q`).
    pub send_q: u64,
}

impl NetSocketRecord {
    /// Encoded size: a fixed 64-byte record.
    pub const WIRE_LEN: usize = 64;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        out[0] = self.proto.as_u8();
        out[1] = self.state.as_u8();
        out[2] = self.family.as_u8();
        // out[3] reserved, left zero.
        out[4..20].copy_from_slice(&self.local_addr);
        put_u16(&mut out, 20, self.local_port);
        out[22..38].copy_from_slice(&self.peer_addr);
        put_u16(&mut out, 38, self.peer_port);
        put_u64(&mut out, 40, self.owner);
        put_u64(&mut out, 48, self.recv_q);
        put_u64(&mut out, 56, self.send_q);
        out
    }

    /// Decode from `bytes`, failing closed on a malformed record.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `bytes` cannot hold a record.
    /// * [`Errno::OutOfRange`] — an unknown protocol, state, or family.
    /// * [`Errno::BadMagic`] — a dirty reserved byte or a V4 address
    ///   with a non-zero tail.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if bytes[3] != 0 {
            return Err(Errno::BadMagic);
        }
        let proto = NetSockProto::from_u8(bytes[0])?;
        let state = NetSockState::from_u8(bytes[1])?;
        let family = NetAddrFamily::from_u8(bytes[2])?;
        let mut local_addr = [0u8; 16];
        local_addr.copy_from_slice(&bytes[4..20]);
        let mut peer_addr = [0u8; 16];
        peer_addr.copy_from_slice(&bytes[22..38]);
        if family == NetAddrFamily::V4
            && (local_addr[4..].iter().any(|&b| b != 0) || peer_addr[4..].iter().any(|&b| b != 0))
        {
            return Err(Errno::BadMagic);
        }
        Ok(Self {
            proto,
            state,
            family,
            local_addr,
            local_port: read_u16(bytes, 20),
            peer_addr,
            peer_port: read_u16(bytes, 38),
            owner: read_u64(bytes, 40),
            recv_q: read_u64(bytes, 48),
            send_q: read_u64(bytes, 56),
        })
    }
}

/// One bond member and its live health (the response record of the
/// [`NetstackRequest::BondMembers`] page; `plans/NETWORK.md` §5, §6.3).
///
/// One record is emitted per (bond, member) pair. It carries the owning
/// bond's alias, the member's own alias, whether the member is the bond's
/// currently-active transmitting member (active-backup; never set in
/// balance mode), and the member's link and eligibility health. The
/// interface aliases are surface topology, so — like the other `state:net`
/// reads — the query is gated `CAP_SYSINFO_GLOBAL` at the broker.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct NetBondMemberRecord {
    /// The owning bond interface's admin-chosen alias, NUL-padded.
    pub bond: [u8; IF_NAME_LEN],
    /// The member interface's admin-chosen alias, NUL-padded.
    pub member: [u8; IF_NAME_LEN],
    /// Whether this member is the bond's currently-active transmitting
    /// member (active-backup only; always `false` in balance mode, where
    /// every eligible member carries flows).
    pub active: bool,
    /// Whether the member's link is currently up.
    pub link_up: bool,
    /// Whether the member is currently eligible to carry traffic (admitted
    /// past the anti-flap up-delay).
    pub eligible: bool,
}

impl NetBondMemberRecord {
    /// Encoded size: a fixed 40-byte record (two 16-byte aliases, three
    /// boolean flags, and a reserved tail that must be zero).
    pub const WIRE_LEN: usize = 40;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        out[0..16].copy_from_slice(&self.bond);
        out[16..32].copy_from_slice(&self.member);
        out[32] = u8::from(self.active);
        out[33] = u8::from(self.link_up);
        out[34] = u8::from(self.eligible);
        // out[35..40] reserved, left zero.
        out
    }

    /// Decode from `bytes`, failing closed on a malformed record.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `bytes` cannot hold a record.
    /// * [`Errno::OutOfRange`] — a flag byte is not exactly `0` or `1`.
    /// * [`Errno::BadMagic`] — a dirty reserved tail.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if bytes[35..40].iter().any(|&b| b != 0) {
            return Err(Errno::BadMagic);
        }
        let mut bond = [0u8; IF_NAME_LEN];
        bond.copy_from_slice(&bytes[0..16]);
        let mut member = [0u8; IF_NAME_LEN];
        member.copy_from_slice(&bytes[16..32]);
        Ok(Self {
            bond,
            member,
            active: decode_bool(bytes[32])?,
            link_up: decode_bool(bytes[33])?,
            eligible: decode_bool(bytes[34])?,
        })
    }
}

/// Largest reply the [`NETSTACK_ENDPOINT`] emits: the status word, the
/// page header, and a full page of state records (the widest reply).
pub const NETSTACK_MAX_REPLY: usize = STATUS_REPLY_LEN
    + PAGE_HEADER_LEN
    + NETSTACK_LIST_LIMIT_MAX as usize * NetInterfaceStateRecord::WIRE_LEN;

/// Byte length of the page header following the status word: the
/// record count (2) and a reserved pair that must be zero (2).
pub const PAGE_HEADER_LEN: usize = 4;

/// Encode a paged reply: the status frame, the count, then `records`
/// packed back-to-back (each already encoded to its fixed width).
///
/// # Errors
///
/// * [`Errno::LengthOutOfRange`] — more records than
///   [`NETSTACK_LIST_LIMIT_MAX`].
/// * [`Errno::BufferTooSmall`] — `out` cannot hold the reply.
pub fn encode_page_reply<const RECORD_LEN: usize>(
    records: &[[u8; RECORD_LEN]],
    out: &mut [u8],
) -> Result<usize, Errno> {
    if records.len() > NETSTACK_LIST_LIMIT_MAX as usize {
        return Err(Errno::LengthOutOfRange);
    }
    let total = STATUS_REPLY_LEN + PAGE_HEADER_LEN + records.len() * RECORD_LEN;
    if out.len() < total {
        return Err(Errno::BufferTooSmall);
    }
    out[..STATUS_REPLY_LEN].copy_from_slice(&encode_status_reply(Ok(())));
    // Record count fits u16: bounded by NETSTACK_LIST_LIMIT_MAX above.
    let count = u16::try_from(records.len()).map_err(|_| Errno::LengthOutOfRange)?;
    put_u16(out, STATUS_REPLY_LEN, count);
    put_u16(out, STATUS_REPLY_LEN + 2, 0);
    let mut cursor = STATUS_REPLY_LEN + PAGE_HEADER_LEN;
    for record in records {
        out[cursor..cursor + RECORD_LEN].copy_from_slice(record);
        cursor += RECORD_LEN;
    }
    Ok(total)
}

/// Decode a paged reply's header, returning the record region.
///
/// # Errors
///
/// * [`Errno::BufferTooSmall`] — `bytes` cannot hold the declared
///   records.
/// * [`Errno::BadMagic`] — a dirty reserved pair.
/// * [`Errno::LengthOutOfRange`] — a count beyond
///   [`NETSTACK_LIST_LIMIT_MAX`].
/// * The decoded [`Errno`] itself, when the service refused the
///   request.
pub fn decode_page_reply(bytes: &[u8], record_len: usize) -> Result<(u16, &[u8]), Errno> {
    decode_status_reply(&bytes[..bytes.len().min(STATUS_REPLY_LEN)])?;
    if bytes.len() < STATUS_REPLY_LEN + PAGE_HEADER_LEN {
        return Err(Errno::BufferTooSmall);
    }
    let count = read_u16(bytes, STATUS_REPLY_LEN);
    if read_u16(bytes, STATUS_REPLY_LEN + 2) != 0 {
        return Err(Errno::BadMagic);
    }
    if count > NETSTACK_LIST_LIMIT_MAX {
        return Err(Errno::LengthOutOfRange);
    }
    let body = &bytes[STATUS_REPLY_LEN + PAGE_HEADER_LEN..];
    let need = count as usize * record_len;
    if body.len() < need {
        return Err(Errno::BufferTooSmall);
    }
    Ok((count, &body[..need]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(text: &str) -> [u8; IF_NAME_LEN] {
        let mut out = [0u8; IF_NAME_LEN];
        out[..text.len()].copy_from_slice(text.as_bytes());
        out
    }

    fn v4(a: u8, b: u8, c: u8, d: u8) -> [u8; 16] {
        let mut out = [0u8; 16];
        out[..4].copy_from_slice(&[a, b, c, d]);
        out
    }

    #[test]
    fn magic_is_the_ascii_tag() {
        assert_eq!(NETSTACK_REQUEST_MAGIC, u32::from_le_bytes(*b"NST1"));
    }

    #[test]
    fn requests_round_trip() {
        for request in [
            NetstackRequest::InterfaceList,
            NetstackRequest::AddrAdd {
                iface: name("wan"),
                family: NetAddrFamily::V4,
                prefix: 24,
                addr: v4(10, 0, 2, 15),
            },
            NetstackRequest::AddrAdd {
                iface: name("lan0"),
                family: NetAddrFamily::V6,
                prefix: 64,
                addr: [0x20; 16],
            },
            NetstackRequest::RouteAdd {
                iface: name("wan"),
                family: NetAddrFamily::V4,
                prefix: 0,
                dest: [0; 16],
                next_hop: Some(v4(10, 0, 2, 2)),
            },
            NetstackRequest::RouteAdd {
                iface: name("wan"),
                family: NetAddrFamily::V6,
                prefix: 64,
                dest: [0x21; 16],
                next_hop: None,
            },
            NetstackRequest::InterfaceCounters {
                offset: 5,
                limit: 12,
            },
            NetstackRequest::InterfaceFacts {
                offset: 3,
                limit: 8,
            },
            NetstackRequest::InterfaceState {
                offset: 0,
                limit: NETSTACK_LIST_LIMIT_MAX,
            },
            NetstackRequest::InterfaceRates {
                offset: 2,
                limit: 4,
                window: Duration64::from_secs(1),
            },
            NetstackRequest::InterfaceRates {
                offset: 0,
                limit: NETSTACK_LIST_LIMIT_MAX,
                window: Duration64::from_nanos(500_000_000),
            },
            NetstackRequest::Sockets {
                offset: 7,
                limit: 16,
            },
            NetstackRequest::Sockets {
                offset: 0,
                limit: NETSTACK_LIST_LIMIT_MAX,
            },
            NetstackRequest::BondMembers {
                offset: 4,
                limit: 9,
            },
            NetstackRequest::BondMembers {
                offset: 0,
                limit: NETSTACK_LIST_LIMIT_MAX,
            },
            NetstackRequest::ApplyNetworkSettings(NetworkSettings {
                ipv4_enabled: true,
                ipv6_enabled: false,
                syncookies_always: true,
                ipv6_privacy: false,
                tcp_keepalive: true,
            }),
            NetstackRequest::ApplyNetworkSettings(NetworkSettings {
                ipv4_enabled: false,
                ipv6_enabled: true,
                syncookies_always: false,
                ipv6_privacy: true,
                tcp_keepalive: false,
            }),
            // Bind with no resolved hardware location.
            NetstackRequest::BindDriver {
                endpoint_id: 0x4E43_4841_4E00,
                iface: name("net0"),
                node_location: 0,
            },
            // Bind carrying a full-width hardware location (a >32-bit
            // register base exercises the u64 field).
            NetstackRequest::BindDriver {
                endpoint_id: 0x4E43_4841_4E01,
                iface: name("wan"),
                node_location: 0x1_0a00_0000,
            },
        ] {
            let bytes = request.to_le_bytes();
            assert_eq!(NetstackRequest::from_bytes(&bytes), Ok(request));
        }
    }

    #[test]
    fn bind_driver_fails_closed_on_a_dirty_reserved_tail() {
        let good = NetstackRequest::BindDriver {
            endpoint_id: 7,
            iface: name("net0"),
            node_location: 0x0a00_0000,
        }
        .to_le_bytes();
        assert_eq!(NetstackRequest::from_bytes(&good).map(|_| ()), Ok(()));
        // A dirty byte in the reserved tail past the node location.
        let mut bad = good;
        bad[40] = 1;
        assert_eq!(NetstackRequest::from_bytes(&bad), Err(Errno::BadMagic));
    }

    #[test]
    fn interface_config_messages_round_trip() {
        for msg in [
            // Full dual-stack static config, matched by MAC.
            NetInterfaceConfigMsg {
                alias: name("wan"),
                match_mac: Some([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]),
                match_node: None,
                ipv4: NetIpv4Config::Static {
                    addr: [10, 0, 2, 15],
                    prefix: 24,
                    gateway: Some([10, 0, 2, 2]),
                },
                ipv6: NetIpv6Config::Static {
                    addr: [0x20; 16],
                    prefix: 64,
                    gateway: Some([0xfe; 16]),
                },
                mtu: 1500,
            },
            // Disabled v4, SLAAC v6, no MAC selector, no MTU override.
            NetInterfaceConfigMsg {
                alias: name("lan0"),
                match_mac: None,
                // Bound by hardware node, not MAC — a >32-bit register base
                // exercises the full u64 width.
                match_node: Some(0x1_0000_0000),
                ipv4: NetIpv4Config::Disabled,
                ipv6: NetIpv6Config::Slaac,
                mtu: 0,
            },
            // Static v4 with no gateway, disabled v6.
            NetInterfaceConfigMsg {
                alias: name("eth9"),
                match_mac: Some([0; 6]),
                match_node: None,
                ipv4: NetIpv4Config::Static {
                    addr: [192, 168, 1, 5],
                    prefix: 32,
                    gateway: None,
                },
                ipv6: NetIpv6Config::Disabled,
                mtu: 9000,
            },
        ] {
            let bytes = msg.to_le_bytes();
            assert_eq!(NetInterfaceConfigMsg::from_bytes(&bytes), Ok(msg));
            msg.validate().expect("the sample configs are valid");
        }
    }

    #[test]
    fn interface_config_magic_is_the_ascii_tag() {
        assert_eq!(NET_INTERFACE_CONFIG_MAGIC, u32::from_le_bytes(*b"NIC1"));
    }

    #[test]
    fn interface_config_fails_closed_on_malformed_framing() {
        let good = NetInterfaceConfigMsg {
            alias: name("wan"),
            match_mac: Some([1, 2, 3, 4, 5, 6]),
            match_node: None,
            ipv4: NetIpv4Config::Static {
                addr: [10, 0, 0, 1],
                prefix: 24,
                gateway: Some([10, 0, 0, 254]),
            },
            ipv6: NetIpv6Config::Slaac,
            mtu: 1500,
        }
        .to_le_bytes();
        // A short buffer.
        assert_eq!(
            NetInterfaceConfigMsg::from_bytes(&good[..good.len() - 1]),
            Err(Errno::BufferTooSmall)
        );
        // Wrong magic.
        let mut bad = good;
        bad[0] ^= 0xFF;
        assert_eq!(
            NetInterfaceConfigMsg::from_bytes(&bad),
            Err(Errno::BadMagic)
        );
        // Wrong version.
        let mut bad = good;
        bad[4] = 2;
        assert_eq!(
            NetInterfaceConfigMsg::from_bytes(&bad),
            Err(Errno::AbiVersionUnsupported)
        );
        // A dirty reserved byte (offset 7).
        let mut bad = good;
        bad[7] = 1;
        assert_eq!(
            NetInterfaceConfigMsg::from_bytes(&bad),
            Err(Errno::BadMagic)
        );
        // A dirty reserved tail byte.
        let mut bad = good;
        bad[NetInterfaceConfigMsg::WIRE_LEN - 1] = 1;
        assert_eq!(
            NetInterfaceConfigMsg::from_bytes(&bad),
            Err(Errno::BadMagic)
        );
        // A mac-present flag that is neither 0 nor 1.
        let mut bad = good;
        bad[6] = 2;
        assert_eq!(
            NetInterfaceConfigMsg::from_bytes(&bad),
            Err(Errno::OutOfRange)
        );
        // A MAC set behind a clear present-flag.
        let mut bad = good;
        bad[6] = 0;
        assert_eq!(
            NetInterfaceConfigMsg::from_bytes(&bad),
            Err(Errno::BadMagic)
        );
        // An unknown IPv4 method.
        let mut bad = good;
        bad[30] = 9;
        assert_eq!(
            NetInterfaceConfigMsg::from_bytes(&bad),
            Err(Errno::OutOfRange)
        );
        // An out-of-range IPv4 static prefix.
        let mut bad = good;
        bad[31] = 33;
        assert_eq!(
            NetInterfaceConfigMsg::from_bytes(&bad),
            Err(Errno::OutOfRange)
        );
        // An unknown IPv6 method.
        let mut bad = good;
        bad[41] = 9;
        assert_eq!(
            NetInterfaceConfigMsg::from_bytes(&bad),
            Err(Errno::OutOfRange)
        );
        // A disabled IPv4 method carrying a non-zero prefix.
        let mut bad = good;
        bad[30] = 0;
        bad[31] = 24;
        assert_eq!(
            NetInterfaceConfigMsg::from_bytes(&bad),
            Err(Errno::BadMagic)
        );
        // A node-present flag that is neither 0 nor 1.
        let mut bad = good;
        bad[78] = 2;
        assert_eq!(
            NetInterfaceConfigMsg::from_bytes(&bad),
            Err(Errno::OutOfRange)
        );
        // A node location set behind a clear present-flag.
        let mut bad = good;
        bad[80] = 1;
        assert_eq!(
            NetInterfaceConfigMsg::from_bytes(&bad),
            Err(Errno::BadMagic)
        );
        // A dirty node-present padding byte (offset 79).
        let mut bad = good;
        bad[79] = 1;
        assert_eq!(
            NetInterfaceConfigMsg::from_bytes(&bad),
            Err(Errno::BadMagic)
        );
    }

    #[test]
    fn interface_config_validate_rejects_bad_semantics() {
        // A multicast static v4 address.
        assert_eq!(
            NetInterfaceConfigMsg {
                alias: name("wan"),
                match_mac: None,
                match_node: None,
                ipv4: NetIpv4Config::Static {
                    addr: [224, 0, 0, 1],
                    prefix: 24,
                    gateway: None,
                },
                ipv6: NetIpv6Config::Disabled,
                mtu: 0,
            }
            .validate(),
            Err(Errno::OutOfRange)
        );
        // An off-subnet v4 gateway.
        assert_eq!(
            NetInterfaceConfigMsg {
                alias: name("wan"),
                match_mac: None,
                match_node: None,
                ipv4: NetIpv4Config::Static {
                    addr: [10, 0, 0, 1],
                    prefix: 24,
                    gateway: Some([10, 0, 1, 1]),
                },
                ipv6: NetIpv6Config::Disabled,
                mtu: 0,
            }
            .validate(),
            Err(Errno::OutOfRange)
        );
        // A too-small MTU override.
        assert_eq!(
            NetInterfaceConfigMsg {
                alias: name("wan"),
                match_mac: None,
                match_node: None,
                ipv4: NetIpv4Config::Disabled,
                ipv6: NetIpv6Config::Slaac,
                mtu: 500,
            }
            .validate(),
            Err(Errno::OutOfRange)
        );
        // A multicast static v6 address.
        assert_eq!(
            NetInterfaceConfigMsg {
                alias: name("wan"),
                match_mac: None,
                match_node: None,
                ipv4: NetIpv4Config::Disabled,
                ipv6: NetIpv6Config::Static {
                    addr: {
                        let mut a = [0u8; 16];
                        a[0] = 0xff;
                        a[15] = 1;
                        a
                    },
                    prefix: 64,
                    gateway: None,
                },
                mtu: 0,
            }
            .validate(),
            Err(Errno::OutOfRange)
        );
    }

    /// A well-formed two-member active-backup bond with a primary.
    fn sample_bond() -> NetBondConfigMsg {
        let mut members = [[0u8; IF_NAME_LEN]; NET_BOND_MAX_MEMBERS];
        members[0] = name("eth0");
        members[1] = name("eth1");
        NetBondConfigMsg {
            alias: name("bond0"),
            mode: NetBondMode::ActiveBackup,
            monitor_interval: Duration64::new(1, 0).unwrap(),
            primary: Some(name("eth0")),
            members,
            member_count: 2,
        }
    }

    #[test]
    fn bond_config_messages_round_trip() {
        // The sample, plus a three-member balance bond with no primary.
        let mut balance = sample_bond();
        balance.mode = NetBondMode::Balance;
        balance.primary = None;
        balance.members[2] = name("eth2");
        balance.member_count = 3;
        for msg in [sample_bond(), balance] {
            let bytes = msg.to_le_bytes();
            assert_eq!(NetBondConfigMsg::from_bytes(&bytes), Ok(msg));
            msg.validate().expect("the sample bonds are valid");
        }
    }

    #[test]
    fn bond_config_magic_is_the_ascii_tag() {
        assert_eq!(NET_BOND_CONFIG_MAGIC, u32::from_le_bytes(*b"NBC1"));
    }

    #[test]
    fn bond_config_fails_closed_on_malformed_framing() {
        let good = sample_bond().to_le_bytes();
        assert_eq!(
            NetBondConfigMsg::from_bytes(&good[..good.len() - 1]),
            Err(Errno::BufferTooSmall)
        );
        // Wrong magic / version.
        let mut bad = good;
        bad[0] ^= 0xFF;
        assert_eq!(NetBondConfigMsg::from_bytes(&bad), Err(Errno::BadMagic));
        let mut bad = good;
        bad[4] = 2;
        assert_eq!(
            NetBondConfigMsg::from_bytes(&bad),
            Err(Errno::AbiVersionUnsupported)
        );
        // An unknown mode.
        let mut bad = good;
        bad[6] = 9;
        assert_eq!(NetBondConfigMsg::from_bytes(&bad), Err(Errno::OutOfRange));
        // A primary-present flag that is neither 0 nor 1.
        let mut bad = good;
        bad[7] = 2;
        assert_eq!(NetBondConfigMsg::from_bytes(&bad), Err(Errno::OutOfRange));
        // A dirty reserved byte after member_count.
        let mut bad = good;
        bad[53] = 1;
        assert_eq!(NetBondConfigMsg::from_bytes(&bad), Err(Errno::BadMagic));
        // A member count past the capacity.
        let mut bad = good;
        bad[52] = u8::try_from(NET_BOND_MAX_MEMBERS + 1).expect("fits u8");
        assert_eq!(NetBondConfigMsg::from_bytes(&bad), Err(Errno::OutOfRange));
        // A non-zero insignificant member slot (member_count is 2, slot 2
        // must be zero).
        let mut bad = good;
        let slot2 = 56 + 2 * IF_NAME_LEN;
        bad[slot2] = b'x';
        assert_eq!(NetBondConfigMsg::from_bytes(&bad), Err(Errno::BadMagic));
    }

    #[test]
    fn bond_config_validate_rejects_bad_semantics() {
        // Fewer than two members.
        let mut one = sample_bond();
        one.member_count = 1;
        assert_eq!(one.validate(), Err(Errno::OutOfRange));
        // A duplicate member.
        let mut dup = sample_bond();
        dup.members[1] = name("eth0");
        assert_eq!(dup.validate(), Err(Errno::OutOfRange));
        // A primary that is not a member.
        let mut stray = sample_bond();
        stray.primary = Some(name("eth9"));
        assert_eq!(stray.validate(), Err(Errno::OutOfRange));
        // A zero monitor interval.
        let mut zero = sample_bond();
        zero.monitor_interval = Duration64::new(0, 0).unwrap();
        assert_eq!(zero.validate(), Err(Errno::OutOfRange));
    }

    #[test]
    fn apply_network_settings_rejects_non_boolean_and_dirty_tail() {
        let good = NetstackRequest::ApplyNetworkSettings(NetworkSettings {
            ipv4_enabled: true,
            ipv6_enabled: true,
            syncookies_always: false,
            ipv6_privacy: true,
            tcp_keepalive: true,
        })
        .to_le_bytes();
        // A byte that is neither 0 nor 1 in any flag position fails closed.
        for pos in 8..=12 {
            let mut smuggled = good;
            smuggled[pos] = 2;
            assert_eq!(
                NetstackRequest::from_bytes(&smuggled),
                Err(Errno::OutOfRange)
            );
        }
        // A non-zero reserved tail byte is refused.
        let mut dirty_tail = good;
        dirty_tail[13] = 1;
        assert_eq!(
            NetstackRequest::from_bytes(&dirty_tail),
            Err(Errno::BadMagic)
        );
    }

    #[test]
    fn sockets_bounds_and_reserved_tail_are_enforced() {
        let good = NetstackRequest::Sockets {
            offset: 3,
            limit: 8,
        }
        .to_le_bytes();
        let mut zero_limit = good;
        zero_limit[12] = 0;
        zero_limit[13] = 0;
        assert_eq!(
            NetstackRequest::from_bytes(&zero_limit),
            Err(Errno::LengthOutOfRange)
        );
        let mut dirty_tail = good;
        dirty_tail[14] = 1;
        assert_eq!(
            NetstackRequest::from_bytes(&dirty_tail),
            Err(Errno::BadMagic)
        );
    }

    #[test]
    fn socket_record_round_trips_and_fails_closed() {
        let record = NetSocketRecord {
            proto: NetSockProto::Tcp,
            state: NetSockState::Established,
            family: NetAddrFamily::V4,
            local_addr: v4(10, 0, 2, 15),
            local_port: 4321,
            peer_addr: v4(10, 0, 2, 2),
            peer_port: 80,
            owner: 0x0102_0304_0506_0708,
            recv_q: 128,
            send_q: 512,
        };
        let bytes = record.to_le_bytes();
        assert_eq!(NetSocketRecord::from_bytes(&bytes), Ok(record));
        // A truncated record fails closed.
        assert_eq!(
            NetSocketRecord::from_bytes(&bytes[..NetSocketRecord::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
        // A dirty reserved byte fails closed.
        let mut dirty = bytes;
        dirty[3] = 1;
        assert_eq!(NetSocketRecord::from_bytes(&dirty), Err(Errno::BadMagic));
        // An unknown protocol fails closed.
        let mut bad_proto = bytes;
        bad_proto[0] = 99;
        assert_eq!(
            NetSocketRecord::from_bytes(&bad_proto),
            Err(Errno::OutOfRange)
        );
        // An unknown state fails closed.
        let mut bad_state = bytes;
        bad_state[1] = 200;
        assert_eq!(
            NetSocketRecord::from_bytes(&bad_state),
            Err(Errno::OutOfRange)
        );
        // A V4 address with a dirty tail fails closed.
        let mut wide_v4 = bytes;
        wide_v4[4 + 5] = 1;
        assert_eq!(NetSocketRecord::from_bytes(&wide_v4), Err(Errno::BadMagic));
    }

    #[test]
    fn bond_member_record_round_trips_and_fails_closed() {
        let record = NetBondMemberRecord {
            bond: name("bond0"),
            member: name("eth1"),
            active: true,
            link_up: true,
            eligible: false,
        };
        let bytes = record.to_le_bytes();
        assert_eq!(NetBondMemberRecord::from_bytes(&bytes), Ok(record));
        // A truncated record fails closed.
        assert_eq!(
            NetBondMemberRecord::from_bytes(&bytes[..NetBondMemberRecord::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
        // A non-boolean flag fails closed.
        let mut bad_flag = bytes;
        bad_flag[32] = 2;
        assert_eq!(
            NetBondMemberRecord::from_bytes(&bad_flag),
            Err(Errno::OutOfRange)
        );
        // A dirty reserved tail fails closed.
        let mut dirty = bytes;
        dirty[35] = 1;
        assert_eq!(
            NetBondMemberRecord::from_bytes(&dirty),
            Err(Errno::BadMagic)
        );
    }

    #[test]
    fn bond_members_bounds_and_reserved_tail_are_enforced() {
        let good = NetstackRequest::BondMembers {
            offset: 0,
            limit: 4,
        }
        .to_le_bytes();
        // A zero limit is refused.
        let mut zero_limit = good;
        zero_limit[12] = 0;
        zero_limit[13] = 0;
        assert_eq!(
            NetstackRequest::from_bytes(&zero_limit),
            Err(Errno::LengthOutOfRange)
        );
        // A dirty reserved tail is refused.
        let mut dirty_tail = good;
        dirty_tail[14] = 1;
        assert_eq!(
            NetstackRequest::from_bytes(&dirty_tail),
            Err(Errno::BadMagic)
        );
    }

    #[test]
    fn socket_state_labels_and_protos_are_stable() {
        assert_eq!(NetSockState::Established.label(), "ESTAB");
        assert_eq!(NetSockState::Listen.label(), "LISTEN");
        assert_eq!(NetSockState::Unconnected.label(), "UNCONN");
        assert_eq!(NetSockProto::Icmp.as_u8(), 1);
        assert_eq!(NetSockProto::Tcp.as_u8(), 6);
        assert_eq!(NetSockProto::Udp.as_u8(), 17);
        assert_eq!(NetSockProto::Icmpv6.as_u8(), 58);
        assert_eq!(NetSockProto::from_u8(1), Ok(NetSockProto::Icmp));
        assert_eq!(NetSockProto::from_u8(58), Ok(NetSockProto::Icmpv6));
        for value in 0u8..=11 {
            assert!(NetSockState::from_u8(value).is_ok());
        }
        assert_eq!(NetSockState::from_u8(12), Err(Errno::OutOfRange));
    }

    #[test]
    fn interface_rates_bounds_are_enforced() {
        let good = NetstackRequest::InterfaceRates {
            offset: 0,
            limit: 4,
            window: Duration64::from_secs(1),
        }
        .to_le_bytes();
        // A zero limit is refused.
        let mut zero_limit = good;
        zero_limit[12] = 0;
        zero_limit[13] = 0;
        assert_eq!(
            NetstackRequest::from_bytes(&zero_limit),
            Err(Errno::LengthOutOfRange)
        );
        // A dirty reserved tail is refused.
        let mut dirty_tail = good;
        dirty_tail[26] = 1;
        assert_eq!(
            NetstackRequest::from_bytes(&dirty_tail),
            Err(Errno::BadMagic)
        );
    }

    #[test]
    fn rates_record_round_trips_and_fails_closed() {
        let mut name = [0u8; IF_NAME_LEN];
        name[..3].copy_from_slice(b"wan");
        let record = NetInterfaceRatesRecord {
            name,
            window: Duration64::from_secs(1),
            rx_pps: 1000,
            rx_bps: 12_000_000,
            tx_pps: 800,
            tx_bps: 9_600_000,
        };
        let bytes = record.to_le_bytes();
        assert_eq!(NetInterfaceRatesRecord::from_bytes(&bytes), Ok(record));
        assert_eq!(
            NetInterfaceRatesRecord::from_bytes(&bytes[..NetInterfaceRatesRecord::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
        // A name whose padding is dirty is refused.
        let mut dirty_name = bytes;
        dirty_name[IF_NAME_LEN - 1] = 0xFF;
        assert_eq!(
            NetInterfaceRatesRecord::from_bytes(&dirty_name),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn decode_fails_closed_on_malformed_framing() {
        let good = NetstackRequest::InterfaceList.to_le_bytes();
        assert_eq!(
            NetstackRequest::from_bytes(&good[..NetstackRequest::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
        let mut bad_magic = good;
        bad_magic[0] ^= 0xFF;
        assert_eq!(
            NetstackRequest::from_bytes(&bad_magic),
            Err(Errno::BadMagic)
        );
        let mut bad_version = good;
        bad_version[4] = 9;
        assert_eq!(
            NetstackRequest::from_bytes(&bad_version),
            Err(Errno::AbiVersionUnsupported)
        );
        let mut bad_op = good;
        bad_op[6] = 99;
        assert_eq!(NetstackRequest::from_bytes(&bad_op), Err(Errno::OutOfRange));
        let mut dirty_tail = good;
        dirty_tail[63] = 1;
        assert_eq!(
            NetstackRequest::from_bytes(&dirty_tail),
            Err(Errno::BadMagic)
        );
    }

    #[test]
    fn addr_add_bounds_are_enforced() {
        let good = NetstackRequest::AddrAdd {
            iface: name("wan"),
            family: NetAddrFamily::V4,
            prefix: 24,
            addr: v4(10, 0, 2, 15),
        }
        .to_le_bytes();
        // Prefix 0 and beyond the family maximum are refused.
        let mut zero_prefix = good;
        zero_prefix[25] = 0;
        assert_eq!(
            NetstackRequest::from_bytes(&zero_prefix),
            Err(Errno::OutOfRange)
        );
        let mut wide_prefix = good;
        wide_prefix[25] = 33;
        assert_eq!(
            NetstackRequest::from_bytes(&wide_prefix),
            Err(Errno::OutOfRange)
        );
        // An unknown family is refused.
        let mut bad_family = good;
        bad_family[24] = 5;
        assert_eq!(
            NetstackRequest::from_bytes(&bad_family),
            Err(Errno::OutOfRange)
        );
        // A V4 address with a dirty tail is refused.
        let mut dirty_v4 = good;
        dirty_v4[30] = 1;
        assert_eq!(NetstackRequest::from_bytes(&dirty_v4), Err(Errno::BadMagic));
        // An illegal interface name is refused.
        let mut bad_name = good;
        bad_name[8] = b'W';
        assert_eq!(
            NetstackRequest::from_bytes(&bad_name),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn route_add_gateway_flag_is_validated() {
        let good = NetstackRequest::RouteAdd {
            iface: name("wan"),
            family: NetAddrFamily::V4,
            prefix: 0,
            dest: [0; 16],
            next_hop: None,
        }
        .to_le_bytes();
        // An unknown flag value is refused.
        let mut bad_flag = good;
        bad_flag[42] = 2;
        assert_eq!(
            NetstackRequest::from_bytes(&bad_flag),
            Err(Errno::OutOfRange)
        );
        // A gateway payload with the flag clear is refused.
        let mut smuggled = good;
        smuggled[45] = 7;
        assert_eq!(NetstackRequest::from_bytes(&smuggled), Err(Errno::BadMagic));
    }

    #[test]
    fn paging_limits_are_bounded() {
        let good = NetstackRequest::InterfaceFacts {
            offset: 0,
            limit: 1,
        }
        .to_le_bytes();
        let mut zero = good;
        zero[12..14].copy_from_slice(&0u16.to_le_bytes());
        assert_eq!(
            NetstackRequest::from_bytes(&zero),
            Err(Errno::LengthOutOfRange)
        );
        let mut wide = good;
        wide[12..14].copy_from_slice(&(NETSTACK_LIST_LIMIT_MAX + 1).to_le_bytes());
        assert_eq!(
            NetstackRequest::from_bytes(&wide),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn if_name_grammar_is_enforced() {
        assert!(validate_if_name(&name("wan")).is_ok());
        assert!(validate_if_name(&name("lan0")).is_ok());
        assert_eq!(validate_if_name(&name("")), Err(Errno::OutOfRange));
        assert_eq!(validate_if_name(&name("WAN")), Err(Errno::OutOfRange));
        assert_eq!(validate_if_name(&name("w an")), Err(Errno::OutOfRange));
        // A byte after the NUL terminator is refused.
        let mut smuggled = name("wan");
        smuggled[10] = b'x';
        assert_eq!(validate_if_name(&smuggled), Err(Errno::OutOfRange));
        // A full-width name is legal.
        assert!(validate_if_name(&name("abcdefghijklmnop")).is_ok());
    }

    fn sample_facts() -> NetInterfaceFactsRecord {
        NetInterfaceFactsRecord {
            name: name("wan"),
            kind: NetIfKind::Ethernet,
            mac: [0x52, 0x54, 0, 0x12, 0x34, 0x56],
            mtu: 1500,
            offloads: 0,
            rx_queues: 1,
        }
    }

    #[test]
    fn facts_record_round_trips_and_fails_closed() {
        let record = sample_facts();
        let bytes = record.to_le_bytes();
        assert_eq!(NetInterfaceFactsRecord::from_bytes(&bytes), Ok(record));
        let mut bad_kind = bytes;
        bad_kind[16] = 9;
        assert_eq!(
            NetInterfaceFactsRecord::from_bytes(&bad_kind),
            Err(Errno::OutOfRange)
        );
        let mut bad_offloads = bytes;
        bad_offloads[31] = 0x80;
        assert_eq!(
            NetInterfaceFactsRecord::from_bytes(&bad_offloads),
            Err(Errno::OutOfRange)
        );
        let mut runt_mtu = bytes;
        runt_mtu[24..28].copy_from_slice(&10u32.to_le_bytes());
        assert_eq!(
            NetInterfaceFactsRecord::from_bytes(&runt_mtu),
            Err(Errno::OutOfRange)
        );
        let mut dirty = bytes;
        dirty[35] = 1;
        assert_eq!(
            NetInterfaceFactsRecord::from_bytes(&dirty),
            Err(Errno::BadMagic)
        );
    }

    fn sample_state() -> NetInterfaceStateRecord {
        let mut addrs = [NetInterfaceStateRecord::EMPTY_ADDR; NET_IF_MAX_ADDRS];
        addrs[0] = NetIfAddr {
            family: NetAddrFamily::V4,
            prefix: 24,
            state: NetAddrState::Preferred,
            addr: v4(10, 0, 2, 15),
        };
        addrs[1] = NetIfAddr {
            family: NetAddrFamily::V6,
            prefix: 64,
            state: NetAddrState::Tentative,
            addr: [0xFE; 16],
        };
        NetInterfaceStateRecord {
            name: name("wan"),
            link_up: true,
            addr_count: 2,
            addrs,
        }
    }

    #[test]
    fn state_record_round_trips_and_fails_closed() {
        let record = sample_state();
        let bytes = record.to_le_bytes();
        assert_eq!(NetInterfaceStateRecord::from_bytes(&bytes), Ok(record));
        let mut bad_link = bytes;
        bad_link[16] = 2;
        assert_eq!(
            NetInterfaceStateRecord::from_bytes(&bad_link),
            Err(Errno::OutOfRange)
        );
        let mut wide_count = bytes;
        wide_count[17] = u8::try_from(NET_IF_MAX_ADDRS).expect("fits u8") + 1;
        assert_eq!(
            NetInterfaceStateRecord::from_bytes(&wide_count),
            Err(Errno::OutOfRange)
        );
        // A non-zero address slot past addr_count is refused.
        let mut smuggled = bytes;
        smuggled[20 + 2 * NetIfAddr::WIRE_LEN + 5] = 1;
        assert_eq!(
            NetInterfaceStateRecord::from_bytes(&smuggled),
            Err(Errno::BadMagic)
        );
        // A bad prefix inside a significant slot is refused.
        let mut bad_prefix = bytes;
        bad_prefix[20 + 1] = 33;
        assert_eq!(
            NetInterfaceStateRecord::from_bytes(&bad_prefix),
            Err(Errno::OutOfRange)
        );
    }

    fn sample_counters() -> NetCounters {
        NetCounters {
            rx_frames: 1,
            rx_bytes: 1500,
            rx_dropped: 2,
            tx_frames: 3,
            tx_bytes: 4096,
            icmp_errors_sent: 4,
            icmp_errors_suppressed: 5,
            reassembly_expired: 6,
            pending_dropped: 7,
        }
    }

    #[test]
    fn counters_record_round_trips_and_fails_closed() {
        let record = NetInterfaceCountersRecord {
            name: name("wan"),
            counters: sample_counters(),
        };
        let bytes = record.to_le_bytes();
        assert_eq!(NetInterfaceCountersRecord::from_bytes(&bytes), Ok(record));
        // A malformed name fails closed.
        let mut bad_name = bytes;
        bad_name[0] = 0xFF;
        assert_eq!(
            NetInterfaceCountersRecord::from_bytes(&bad_name),
            Err(Errno::OutOfRange)
        );
        // A truncated record fails closed.
        assert_eq!(
            NetInterfaceCountersRecord::from_bytes(&bytes[..bytes.len() - 1]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn page_reply_round_trips_and_fails_closed() {
        let records = [sample_facts().to_le_bytes(), sample_facts().to_le_bytes()];
        let mut out = [0u8; NETSTACK_MAX_REPLY];
        let len = encode_page_reply(&records, &mut out).expect("encode");
        let (count, body) =
            decode_page_reply(&out[..len], NetInterfaceFactsRecord::WIRE_LEN).expect("decode");
        assert_eq!(count, 2);
        assert_eq!(body.len(), 2 * NetInterfaceFactsRecord::WIRE_LEN);
        for chunk in body.chunks_exact(NetInterfaceFactsRecord::WIRE_LEN) {
            assert_eq!(
                NetInterfaceFactsRecord::from_bytes(chunk),
                Ok(sample_facts())
            );
        }
        // A truncated body fails closed.
        assert_eq!(
            decode_page_reply(&out[..len - 1], NetInterfaceFactsRecord::WIRE_LEN),
            Err(Errno::BufferTooSmall)
        );
        // A dirty reserved pair fails closed.
        let mut dirty = out;
        dirty[STATUS_REPLY_LEN + 2] = 1;
        assert_eq!(
            decode_page_reply(&dirty[..len], NetInterfaceFactsRecord::WIRE_LEN),
            Err(Errno::BadMagic)
        );
        // A refusal decodes to its errno.
        let refusal = encode_status_reply(Err(Errno::PermissionDenied));
        assert_eq!(
            decode_page_reply(&refusal, NetInterfaceFactsRecord::WIRE_LEN),
            Err(Errno::PermissionDenied)
        );
    }

    #[test]
    fn interface_list_reply_reuses_the_page_codec() {
        let names = [name("wan"), name("lan0")];
        let mut out = [0u8; NETSTACK_MAX_REPLY];
        let len = encode_page_reply(&names, &mut out).expect("encode");
        let (count, body) = decode_page_reply(&out[..len], IF_NAME_LEN).expect("decode");
        assert_eq!(count, 2);
        assert_eq!(&body[..IF_NAME_LEN], &name("wan"));
        assert_eq!(&body[IF_NAME_LEN..], &name("lan0"));
    }
}
