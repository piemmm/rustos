//! The discovery channel: the reserved rendezvous a program browses for and
//! resolves link-local services through (`plans/ZEROCONF.md` Z4).
//!
//! The service (`discoveryd`) answers in **sessions**. A caller opens one,
//! naming a private delivery port; starts typed requests in it; and is rung —
//! one small [`encode_doorbell`] message on that port — when answers are
//! waiting, which it then takes with a non-blocking
//! [`DiscoveryRequest::Collect`]. At most one ring is outstanding per session,
//! so a slow reader is never flooded, and the answers themselves travel as the
//! reply to a call, never through a mailbox that could drop them.
//!
//! What a request may ask is typed ([`Query`]), not a raw DNS question: the
//! service derives every name it asks the segment from the request's fields,
//! so the service type a browse is scoped to is a field it reads, never
//! something it infers from a name the caller spelled. Whether the caller may
//! ask at all is decided from its kernel-attested [`Origin`], never from the
//! frame.
//!
//! Every answer names the interface it was learned on. Nothing crossing this
//! channel merges two links: a record learned on one network is never
//! presented as reachable on another.
//!
//! # Wire shape
//!
//! A request is one fixed little-endian header and, for the requests that
//! carry one, a trailing name:
//!
//! ```text
//! 0   u32  magic         DISCOVERY_REQUEST_MAGIC
//! 4   u16  version       DISCOVERY_VERSION_V1
//! 6   u16  op
//! 8   u32  session       all but Open
//! 12  u32  request       Stop only
//! 16  u64  deliver_port  Open only
//! 24  u32  capacity      Collect only: the caller's reply buffer
//! 28  u8   query         Start only
//! 29  u8   families      Host only
//! 30  u8   transport     Browse and Resolve only
//! 31  u8   service_len   Browse and Resolve only
//! 32  [16] service       Browse and Resolve only, zero past service_len
//! 48  u8   family        Reverse only
//! 49  u8   name_len      Resolve (an instance label) and Host (a wire name)
//! 50  u16  reserved      zero
//! 52  [16] address       Reverse only
//! 68  name               name_len bytes
//! ```
//!
//! A field an operation does not use must be zero, and the frame must end
//! exactly where its name does, so no request smuggles one field through
//! another's. The *structure* of every field is judged here; the DNS-SD
//! grammar of a service type or an instance label has one home,
//! `tairix_net::dnssd`, which the service applies.

use core::net::IpAddr;

use crate::le::{put_u16, put_u32, put_u64, read_u16, read_u32, read_u64};
use crate::net_ipc::{address_parts, ip_from_parts, validate_if_name, NetAddrFamily, IF_NAME_LEN};
use crate::origin::{Origin, TrustDomain};
use crate::reply::{decode_status_reply, encode_status_reply, STATUS_REPLY_LEN};
use crate::Errno;

/// Reserved well-known call-endpoint id of the discovery service (`"DSC1"`
/// little-endian).
///
/// Reserved ([`crate::ipc::is_reserved_endpoint`]), so binding it requires
/// `CAP_IPC_BIND_PRIVILEGED`: a squatter claiming it first would answer every
/// program's browse with services of its own choosing.
pub const DISCOVERY_ENDPOINT: u64 = u64::from_le_bytes(*b"DSC1\0\0\0\0");

/// The uid of the service account the discovery service runs as.
///
/// A system account compiled into the kernel's identity table and attested on
/// every IPC origin, so a client authenticates a doorbell by it
/// ([`from_discovery_service`]), and the network stack reserves the multicast
/// DNS port to it.
pub const DISCOVERYD_UID: u32 = 20;

/// Whether a message's kernel-attested `origin` is the discovery service.
///
/// A delivery port is an inbox any process holding its id may post to, so a
/// doorbell from anyone else is forged.
#[must_use]
pub fn from_discovery_service(origin: &Origin) -> bool {
    origin.trust_domain() == TrustDomain::User && origin.uid() == DISCOVERYD_UID
}

/// Magic number identifying a discovery request (`"DSCR"` little-endian).
pub const DISCOVERY_REQUEST_MAGIC: u32 = u32::from_le_bytes(*b"DSCR");

/// Magic number identifying a doorbell (`"DSCB"` little-endian).
pub const DISCOVERY_DOORBELL_MAGIC: u32 = u32::from_le_bytes(*b"DSCB");

/// The `discovery-v1` protocol version.
pub const DISCOVERY_VERSION_V1: u16 = 1;

/// The longest DNS name in wire form (RFC 1035 §2.3.4) — and the bound
/// `tairix_net::dns` holds names to, which it takes from here so the codec and
/// the channel cannot disagree about what fits.
pub const NAME_MAX: usize = 255;

/// The longest DNS label (RFC 1035 §2.3.4), and so the longest DNS-SD
/// instance label (RFC 6763 §4.1.1); `tairix_net::dns` takes it from here.
pub const LABEL_MAX: usize = 63;

/// The longest registered service name (RFC 6335 §5.1), the part of
/// `_ipp._tcp` between the underscore and the transport;
/// `tairix_net::dnssd` takes it from here.
pub const SERVICE_NAME_MAX: usize = 15;

/// The largest `TXT` rdata this system holds, emits, or carries, in octets —
/// the bound `tairix_net::mdns` holds records to, taken from here.
///
/// A fixed validation bound: RFC 6763 §6.1 asks that a service's whole `TXT`
/// stay under 400 octets so a response fits one classic message, and this sits
/// just above that.
pub const TXT_MAX: usize = 512;

/// Sessions one account may hold open at once.
///
/// A fixed containment bound, not a capacity: every session holds a queue in
/// the service, and an account opening sessions it never reads must not be
/// able to grow the service without limit. It is per account rather than per
/// process so a user cannot multiply it by starting processes.
pub const SESSIONS_PER_ACCOUNT: usize = 8;

/// Requests one session may hold at once — a fixed containment bound, since
/// each is a standing question the service asks the segment on the caller's
/// behalf.
pub const REQUESTS_PER_SESSION: usize = 16;

/// Bytes of answers the service queues for one session before it stops
/// queueing that session's updates and tells it so ([`Entry::Lost`]).
///
/// A fixed containment bound: a reader that does not collect costs the
/// service this much and no more.
pub const SESSION_QUEUE_BYTES: usize = 64 * 1024;

/// The largest collect reply the service sends.
pub const DISCOVERY_MAX_REPLY: usize = 8192;

/// Length of the fixed request header.
pub const REQUEST_HEADER_LEN: usize = 68;

/// The largest request: the header and a longest name.
pub const DISCOVERY_MAX_REQUEST: usize = REQUEST_HEADER_LEN + NAME_MAX;

/// Length of a session or request reply: the status and the id.
pub const ID_REPLY_LEN: usize = STATUS_REPLY_LEN + 4;

/// Length of a doorbell message.
pub const DOORBELL_LEN: usize = 12;

/// Length of a collect reply's header: the status, the entry count, the
/// flags, and a reserved byte.
pub const COLLECT_HEADER_LEN: usize = STATUS_REPLY_LEN + 4;

/// Length of every entry's fixed header.
pub const ENTRY_HEADER_LEN: usize = 28;

/// The longest entry: a `TXT` of [`TXT_MAX`] octets.
pub const ENTRY_MAX: usize = ENTRY_HEADER_LEN + 2 + TXT_MAX;

/// The least collect capacity a caller may name: one longest entry must
/// always fit, or a session holding one could never be drained.
pub const COLLECT_MIN_CAPACITY: usize = COLLECT_HEADER_LEN + ENTRY_MAX;

const _: () = assert!(COLLECT_MIN_CAPACITY <= DISCOVERY_MAX_REPLY);

const OP_OPEN: u16 = 1;
const OP_CLOSE: u16 = 2;
const OP_START: u16 = 3;
const OP_STOP: u16 = 4;
const OP_COLLECT: u16 = 5;

const QUERY_BROWSE: u8 = 1;
const QUERY_RESOLVE: u8 = 2;
const QUERY_HOST: u8 = 3;
const QUERY_REVERSE: u8 = 4;
const QUERY_TYPES: u8 = 5;

const OFF_SESSION: usize = 8;
const OFF_REQUEST: usize = 12;
const OFF_DELIVER: usize = 16;
const OFF_CAPACITY: usize = 24;
const OFF_QUERY: usize = 28;
const OFF_FAMILIES: usize = 29;
const OFF_TRANSPORT: usize = 30;
const OFF_SERVICE_LEN: usize = 31;
const OFF_SERVICE: usize = 32;
const SERVICE_FIELD_LEN: usize = 16;
const OFF_FAMILY: usize = 48;
const OFF_NAME_LEN: usize = 49;
const OFF_RESERVED: usize = 50;
const OFF_ADDRESS: usize = 52;

const _: () = assert!(SERVICE_NAME_MAX < SERVICE_FIELD_LEN);
const _: () = assert!(OFF_ADDRESS + 16 == REQUEST_HEADER_LEN);

/// The transport a service type runs over (RFC 6763 §4.1.2).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Transport {
    /// `_tcp`.
    Tcp = 1,
    /// `_udp`.
    Udp = 2,
}

impl Transport {
    /// The wire value.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// The transport a wire value names.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for any other value.
    pub const fn from_u8(value: u8) -> Result<Self, Errno> {
        match value {
            1 => Ok(Self::Tcp),
            2 => Ok(Self::Udp),
            _ => Err(Errno::OutOfRange),
        }
    }

    /// The DNS-SD label, underscore included.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Tcp => "_tcp",
            Self::Udp => "_udp",
        }
    }

    /// The transport `label` names, compared without ASCII case as every DNS
    /// label is (RFC 4343).
    #[must_use]
    pub fn from_label(label: &[u8]) -> Option<Self> {
        [Self::Tcp, Self::Udp]
            .into_iter()
            .find(|transport| label.eq_ignore_ascii_case(transport.label().as_bytes()))
    }
}

/// A service type as the channel carries it: the registered name without its
/// underscore, and the transport.
///
/// Only the length is judged here; the RFC 6335 character grammar is
/// `tairix_net::dnssd`'s.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ServiceTypeField<'a> {
    /// The name, `1..=SERVICE_NAME_MAX` octets.
    pub name: &'a [u8],
    /// The transport.
    pub transport: Transport,
}

impl ServiceTypeField<'_> {
    fn check(&self) -> Result<(), Errno> {
        if self.name.is_empty() || self.name.len() > SERVICE_NAME_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        Ok(())
    }
}

/// Which address families a host lookup wants.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Families {
    /// `A` records.
    pub v4: bool,
    /// `AAAA` records.
    pub v6: bool,
}

impl Families {
    /// Both families.
    pub const BOTH: Self = Self { v4: true, v6: true };

    fn as_u8(self) -> u8 {
        u8::from(self.v4) | (u8::from(self.v6) << 1)
    }

    const fn from_u8(value: u8) -> Result<Self, Errno> {
        match value {
            1..=3 => Ok(Self {
                v4: value & 1 != 0,
                v6: value & 2 != 0,
            }),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// What one request asks, answered continuously until it is stopped.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Query<'a> {
    /// The instances of one service type (RFC 6763 §4): answered with
    /// [`Answer::Instance`].
    Browse {
        /// The type browsed for.
        service: ServiceTypeField<'a>,
    },
    /// Where one instance is reached and what it says of itself (RFC 6763
    /// §5–6): answered with [`Answer::Service`] and [`Answer::Text`].
    Resolve {
        /// The instance's own label, `1..=LABEL_MAX` octets.
        instance: &'a [u8],
        /// The type it is published under.
        service: ServiceTypeField<'a>,
    },
    /// The addresses of one link-local host: answered with
    /// [`Answer::Address`].
    Host {
        /// The host's name in uncompressed wire form, `1..=NAME_MAX`
        /// octets.
        name: &'a [u8],
        /// Which address families to ask for.
        families: Families,
    },
    /// The name of one link-local address (RFC 6762 §4): answered with
    /// [`Answer::Pointer`].
    Reverse {
        /// The address.
        address: IpAddr,
    },
    /// Every service type the segment offers (RFC 6763 §9): answered with
    /// [`Answer::Type`].
    Types,
}

/// One call on [`DISCOVERY_ENDPOINT`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DiscoveryRequest<'a> {
    /// Open a session, whose doorbell rings on `deliver_port`. Answered with
    /// the session id ([`decode_id_reply`]).
    Open {
        /// A port the caller has bound privately and drains.
        deliver_port: u64,
    },
    /// End a session and every request in it.
    Close {
        /// The session.
        session: u32,
    },
    /// Start a request. Answered with the request id ([`decode_id_reply`]).
    Start {
        /// The session.
        session: u32,
        /// What is asked.
        query: Query<'a>,
    },
    /// Stop one request; nothing further is queued for it.
    Stop {
        /// The session.
        session: u32,
        /// The request.
        request: u32,
    },
    /// Take the answers waiting for a session, as many as fit `capacity`.
    /// Never waits: an empty session answers with no entries.
    Collect {
        /// The session.
        session: u32,
        /// The caller's reply buffer, at least [`COLLECT_MIN_CAPACITY`].
        capacity: u32,
    },
}

impl<'a> DiscoveryRequest<'a> {
    /// The encoded length.
    #[must_use]
    pub fn wire_len(&self) -> usize {
        REQUEST_HEADER_LEN
            + match self {
                Self::Start {
                    query: Query::Resolve { instance, .. },
                    ..
                } => instance.len(),
                Self::Start {
                    query: Query::Host { name, .. },
                    ..
                } => name.len(),
                _ => 0,
            }
    }

    /// Encode into the front of `out`, returning the length.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] when `out` cannot hold the frame, and
    /// [`Errno::LengthOutOfRange`] or [`Errno::OutOfRange`] for a field no
    /// decoder would accept, so an encoder never writes a frame that is
    /// refused.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Errno> {
        let len = self.wire_len();
        let frame = out.get_mut(..len).ok_or(Errno::BufferTooSmall)?;
        frame.fill(0);
        put_u32(frame, 0, DISCOVERY_REQUEST_MAGIC);
        put_u16(frame, 4, DISCOVERY_VERSION_V1);
        match *self {
            Self::Open { deliver_port } => {
                put_u16(frame, 6, OP_OPEN);
                put_u64(frame, OFF_DELIVER, deliver_port);
            }
            Self::Close { session } => {
                put_u16(frame, 6, OP_CLOSE);
                put_u32(frame, OFF_SESSION, session);
            }
            Self::Stop { session, request } => {
                put_u16(frame, 6, OP_STOP);
                put_u32(frame, OFF_SESSION, session);
                put_u32(frame, OFF_REQUEST, request);
            }
            Self::Collect { session, capacity } => {
                put_u16(frame, 6, OP_COLLECT);
                put_u32(frame, OFF_SESSION, session);
                put_u32(frame, OFF_CAPACITY, capacity);
            }
            Self::Start { session, query } => {
                put_u16(frame, 6, OP_START);
                put_u32(frame, OFF_SESSION, session);
                encode_query(&query, frame)?;
            }
        }
        Ok(len)
    }

    /// Decode one request, refusing any an honest caller cannot have sent.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] for a short frame, [`Errno::BadMagic`] for
    /// the wrong magic or a non-zero field the operation does not use,
    /// [`Errno::AbiVersionUnsupported`] for another version,
    /// [`Errno::OutOfRange`] for an unknown operation, query, transport,
    /// family set, or address family, and [`Errno::LengthOutOfRange`] for a
    /// name length out of bounds or a frame that does not end where it
    /// declares.
    pub fn decode(bytes: &'a [u8]) -> Result<Self, Errno> {
        let header = bytes
            .get(..REQUEST_HEADER_LEN)
            .ok_or(Errno::BufferTooSmall)?;
        if read_u32(header, 0) != DISCOVERY_REQUEST_MAGIC {
            return Err(Errno::BadMagic);
        }
        if read_u16(header, 4) != DISCOVERY_VERSION_V1 {
            return Err(Errno::AbiVersionUnsupported);
        }
        let op = read_u16(header, 6);
        let session = read_u32(header, OFF_SESSION);
        let request = read_u32(header, OFF_REQUEST);
        let deliver_port = read_u64(header, OFF_DELIVER);
        let capacity = read_u32(header, OFF_CAPACITY);
        let decoded = match op {
            OP_OPEN => {
                if session != 0 {
                    return Err(Errno::BadMagic);
                }
                Self::Open { deliver_port }
            }
            OP_CLOSE => Self::Close { session },
            OP_STOP => Self::Stop { session, request },
            OP_COLLECT => Self::Collect { session, capacity },
            OP_START => Self::Start {
                session,
                query: decode_query(bytes)?,
            },
            _ => return Err(Errno::OutOfRange),
        };
        // Everything the operation did not read must be zero.
        let (uses_request, uses_deliver, uses_capacity, uses_query) = match decoded {
            Self::Open { .. } => (false, true, false, false),
            Self::Close { .. } => (false, false, false, false),
            Self::Stop { .. } => (true, false, false, false),
            Self::Collect { .. } => (false, false, true, false),
            Self::Start { .. } => (false, false, false, true),
        };
        let unused = [
            (!uses_request).then_some(u64::from(request)),
            (!uses_deliver).then_some(deliver_port),
            (!uses_capacity).then_some(u64::from(capacity)),
        ];
        if unused.iter().flatten().any(|&value| value != 0) {
            return Err(Errno::BadMagic);
        }
        if !uses_query {
            if header[OFF_QUERY..].iter().any(|&byte| byte != 0) {
                return Err(Errno::BadMagic);
            }
            if bytes.len() != REQUEST_HEADER_LEN {
                return Err(Errno::LengthOutOfRange);
            }
        }
        Ok(decoded)
    }
}

fn encode_service(service: &ServiceTypeField<'_>, frame: &mut [u8]) -> Result<(), Errno> {
    service.check()?;
    frame[OFF_TRANSPORT] = service.transport.as_u8();
    frame[OFF_SERVICE_LEN] =
        u8::try_from(service.name.len()).map_err(|_| Errno::LengthOutOfRange)?;
    frame[OFF_SERVICE..OFF_SERVICE + service.name.len()].copy_from_slice(service.name);
    Ok(())
}

fn encode_name(name: &[u8], max: usize, frame: &mut [u8]) -> Result<(), Errno> {
    if name.is_empty() || name.len() > max {
        return Err(Errno::LengthOutOfRange);
    }
    frame[OFF_NAME_LEN] = u8::try_from(name.len()).map_err(|_| Errno::LengthOutOfRange)?;
    frame[REQUEST_HEADER_LEN..REQUEST_HEADER_LEN + name.len()].copy_from_slice(name);
    Ok(())
}

fn encode_query(query: &Query<'_>, frame: &mut [u8]) -> Result<(), Errno> {
    match *query {
        Query::Browse { service } => {
            frame[OFF_QUERY] = QUERY_BROWSE;
            encode_service(&service, frame)
        }
        Query::Resolve { instance, service } => {
            frame[OFF_QUERY] = QUERY_RESOLVE;
            encode_service(&service, frame)?;
            encode_name(instance, LABEL_MAX, frame)
        }
        Query::Host { name, families } => {
            frame[OFF_QUERY] = QUERY_HOST;
            // A lookup asking for no family is refused on decode too.
            frame[OFF_FAMILIES] = Families::from_u8(families.as_u8())?.as_u8();
            encode_name(name, NAME_MAX, frame)
        }
        Query::Reverse { address } => {
            frame[OFF_QUERY] = QUERY_REVERSE;
            let (family, octets) = address_parts(address);
            frame[OFF_FAMILY] = family.as_u8();
            frame[OFF_ADDRESS..OFF_ADDRESS + 16].copy_from_slice(&octets);
            Ok(())
        }
        Query::Types => {
            frame[OFF_QUERY] = QUERY_TYPES;
            Ok(())
        }
    }
}

/// Decode a `Start` frame's query, requiring every field the query does not
/// use to be zero and the frame to end exactly after its name.
fn decode_query(bytes: &[u8]) -> Result<Query<'_>, Errno> {
    let header = &bytes[..REQUEST_HEADER_LEN];
    let kind = header[OFF_QUERY];
    let families = header[OFF_FAMILIES];
    let transport = header[OFF_TRANSPORT];
    let service_len = usize::from(header[OFF_SERVICE_LEN]);
    let service_field = &header[OFF_SERVICE..OFF_SERVICE + SERVICE_FIELD_LEN];
    let family = header[OFF_FAMILY];
    let name_len = usize::from(header[OFF_NAME_LEN]);
    let address = &header[OFF_ADDRESS..OFF_ADDRESS + 16];
    if read_u16(header, OFF_RESERVED) != 0 {
        return Err(Errno::BadMagic);
    }
    let name = bytes
        .get(REQUEST_HEADER_LEN..)
        .ok_or(Errno::BufferTooSmall)?;
    if name.len() != name_len {
        return Err(Errno::LengthOutOfRange);
    }

    let uses_service = matches!(kind, QUERY_BROWSE | QUERY_RESOLVE);
    let service = if uses_service {
        if service_len == 0 || service_len > SERVICE_NAME_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        if service_field[service_len..].iter().any(|&byte| byte != 0) {
            return Err(Errno::BadMagic);
        }
        Some(ServiceTypeField {
            name: &service_field[..service_len],
            transport: Transport::from_u8(transport)?,
        })
    } else {
        if transport != 0 || service_len != 0 || service_field.iter().any(|&byte| byte != 0) {
            return Err(Errno::BadMagic);
        }
        None
    };
    if kind != QUERY_HOST && families != 0 {
        return Err(Errno::BadMagic);
    }
    if kind != QUERY_REVERSE && (family != 0 || address.iter().any(|&byte| byte != 0)) {
        return Err(Errno::BadMagic);
    }
    let name_max = match kind {
        QUERY_RESOLVE => LABEL_MAX,
        QUERY_HOST => NAME_MAX,
        _ => 0,
    };
    if name_max == 0 {
        if name_len != 0 {
            return Err(Errno::LengthOutOfRange);
        }
    } else if name_len == 0 || name_len > name_max {
        return Err(Errno::LengthOutOfRange);
    }

    match (kind, service) {
        (QUERY_BROWSE, Some(service)) => Ok(Query::Browse { service }),
        (QUERY_RESOLVE, Some(service)) => Ok(Query::Resolve {
            instance: name,
            service,
        }),
        (QUERY_HOST, None) => Ok(Query::Host {
            name,
            families: Families::from_u8(families)?,
        }),
        (QUERY_REVERSE, None) => Ok(Query::Reverse {
            address: read_address(family, address)?,
        }),
        (QUERY_TYPES, None) => Ok(Query::Types),
        _ => Err(Errno::OutOfRange),
    }
}

/// An address field: a family and sixteen octets, an IPv4 address's tail
/// zero.
fn read_address(family: u8, octets: &[u8]) -> Result<IpAddr, Errno> {
    let family = NetAddrFamily::from_u8(family)?;
    let mut addr = [0u8; 16];
    addr.copy_from_slice(octets);
    if family == NetAddrFamily::V4 && addr[4..].iter().any(|&byte| byte != 0) {
        return Err(Errno::BadMagic);
    }
    Ok(ip_from_parts(family, addr))
}

/// Encode an `Open` or `Start` outcome: the status and, on success, the id.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] when `out` cannot hold the reply.
pub fn encode_id_reply(result: Result<u32, Errno>, out: &mut [u8]) -> Result<usize, Errno> {
    let len = match result {
        Ok(_) => ID_REPLY_LEN,
        Err(_) => STATUS_REPLY_LEN,
    };
    let frame = out.get_mut(..len).ok_or(Errno::BufferTooSmall)?;
    frame[..STATUS_REPLY_LEN].copy_from_slice(&encode_status_reply(result.map(|_| ())));
    if let Ok(id) = result {
        put_u32(frame, STATUS_REPLY_LEN, id);
    }
    Ok(len)
}

/// Decode an `Open` or `Start` reply.
///
/// # Errors
///
/// The service's refusal, or [`Errno::BufferTooSmall`] for a reply too short
/// to carry its id.
pub fn decode_id_reply(bytes: &[u8]) -> Result<u32, Errno> {
    decode_status_reply(bytes)?;
    if bytes.len() != ID_REPLY_LEN {
        return Err(Errno::BufferTooSmall);
    }
    Ok(read_u32(bytes, STATUS_REPLY_LEN))
}

/// Encode the doorbell for `session`.
#[must_use]
pub fn encode_doorbell(session: u32) -> [u8; DOORBELL_LEN] {
    let mut frame = [0u8; DOORBELL_LEN];
    put_u32(&mut frame, 0, DISCOVERY_DOORBELL_MAGIC);
    put_u16(&mut frame, 4, DISCOVERY_VERSION_V1);
    put_u32(&mut frame, 8, session);
    frame
}

/// Decode a doorbell, returning the session it rings for.
///
/// # Errors
///
/// [`Errno::LengthOutOfRange`] for another length, [`Errno::BadMagic`] for the
/// wrong magic or a non-zero reserved field, and
/// [`Errno::AbiVersionUnsupported`] for another version.
pub fn decode_doorbell(bytes: &[u8]) -> Result<u32, Errno> {
    if bytes.len() != DOORBELL_LEN {
        return Err(Errno::LengthOutOfRange);
    }
    if read_u32(bytes, 0) != DISCOVERY_DOORBELL_MAGIC || read_u16(bytes, 6) != 0 {
        return Err(Errno::BadMagic);
    }
    if read_u16(bytes, 4) != DISCOVERY_VERSION_V1 {
        return Err(Errno::AbiVersionUnsupported);
    }
    Ok(read_u32(bytes, 8))
}

/// How one answer moved.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Change {
    /// It is now held.
    Added = 1,
    /// It was asserted again, renewing its lifetime.
    Refreshed = 2,
    /// It is no longer held: it expired, was withdrawn, or its link went
    /// down.
    Retired = 3,
}

impl Change {
    const fn from_u8(value: u8) -> Result<Self, Errno> {
        match value {
            1 => Ok(Self::Added),
            2 => Ok(Self::Refreshed),
            3 => Ok(Self::Retired),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// What one answer says.
///
/// Every name here was authored by an unauthenticated peer on the segment:
/// it is structurally valid, never display-safe, and a renderer escapes it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Answer<'a> {
    /// A browse found an instance: its own label, `1..=LABEL_MAX` octets.
    Instance {
        /// The instance label.
        label: &'a [u8],
    },
    /// A resolve found where the instance is reached (RFC 2782).
    Service {
        /// Lower is preferred.
        priority: u16,
        /// Share among equal priorities.
        weight: u16,
        /// The port.
        port: u16,
        /// The target host, uncompressed wire form, `1..=NAME_MAX` octets.
        target: &'a [u8],
    },
    /// A resolve found the instance's attributes: `TXT` rdata,
    /// `1..=TXT_MAX` octets.
    Text {
        /// The rdata.
        octets: &'a [u8],
    },
    /// A host lookup found an address.
    Address {
        /// The address.
        address: IpAddr,
    },
    /// A reverse lookup found a name, uncompressed wire form,
    /// `1..=NAME_MAX` octets.
    Pointer {
        /// The name.
        target: &'a [u8],
    },
    /// A type enumeration found a service type.
    Type {
        /// The type.
        service: ServiceTypeField<'a>,
    },
}

/// One entry of a collect reply.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Entry<'a> {
    /// An answer to a request moved.
    Answer {
        /// The request.
        request: u32,
        /// The interface it was learned on.
        interface: [u8; IF_NAME_LEN],
        /// How it moved.
        change: Change,
        /// Its lifetime as the peer stated it, in seconds.
        ttl: u32,
        /// What it says.
        answer: Answer<'a>,
    },
    /// Every answer the request held on `interface` is void: the link went
    /// down, or the service had to start over. What is still true arrives
    /// again as [`Change::Added`].
    Flush {
        /// The request.
        request: u32,
        /// The interface.
        interface: [u8; IF_NAME_LEN],
    },
    /// Updates for the request were dropped because the session's queue was
    /// full, so its answers are incomplete from here. Stopping and starting
    /// it again answers with what is held now.
    Lost {
        /// The request.
        request: u32,
    },
}

const KIND_INSTANCE: u8 = 1;
const KIND_SERVICE: u8 = 2;
const KIND_TEXT: u8 = 3;
const KIND_ADDRESS: u8 = 4;
const KIND_POINTER: u8 = 5;
const KIND_TYPE: u8 = 6;
const KIND_FLUSH: u8 = 7;
const KIND_LOST: u8 = 8;

impl<'a> Entry<'a> {
    /// Decode one entry spanning exactly `bytes`, its length prefix included.
    ///
    /// # Errors
    ///
    /// As [`CollectReply::parse`] for one entry, plus
    /// [`Errno::LengthOutOfRange`] for bytes past the entry's own length.
    pub fn decode(bytes: &'a [u8]) -> Result<Self, Errno> {
        let (entry, rest) = split_entry(bytes)?;
        if !rest.is_empty() {
            return Err(Errno::LengthOutOfRange);
        }
        decode_entry(entry)
    }
}

impl Entry<'_> {
    /// The same entry, naming `request`.
    #[must_use]
    pub const fn for_request(self, request: u32) -> Self {
        match self {
            Self::Answer {
                interface,
                change,
                ttl,
                answer,
                ..
            } => Self::Answer {
                request,
                interface,
                change,
                ttl,
                answer,
            },
            Self::Flush { interface, .. } => Self::Flush { request, interface },
            Self::Lost { .. } => Self::Lost { request },
        }
    }

    /// The encoded length.
    #[must_use]
    pub fn wire_len(&self) -> usize {
        ENTRY_HEADER_LEN
            + match self {
                Self::Answer { answer, .. } => match answer {
                    Answer::Instance { label } => 1 + label.len(),
                    Answer::Service { target, .. } => 7 + target.len(),
                    Answer::Text { octets } => 2 + octets.len(),
                    Answer::Address { .. } => 17,
                    Answer::Pointer { target } => 1 + target.len(),
                    Answer::Type { service } => 2 + service.name.len(),
                },
                Self::Flush { .. } | Self::Lost { .. } => 0,
            }
    }

    /// Encode into the front of `out`, returning the length.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] when `out` cannot hold it, and
    /// [`Errno::LengthOutOfRange`] or [`Errno::OutOfRange`] for a field no
    /// decoder would accept.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Errno> {
        let len = self.wire_len();
        let frame = out.get_mut(..len).ok_or(Errno::BufferTooSmall)?;
        frame.fill(0);
        put_u16(
            frame,
            0,
            u16::try_from(len).map_err(|_| Errno::LengthOutOfRange)?,
        );
        match *self {
            Self::Answer {
                request,
                interface,
                change,
                ttl,
                answer,
            } => {
                validate_if_name(&interface)?;
                frame[3] = change as u8;
                put_u32(frame, 4, request);
                frame[8..24].copy_from_slice(&interface);
                put_u32(frame, 24, ttl);
                frame[2] = encode_answer(&answer, &mut frame[ENTRY_HEADER_LEN..])?;
            }
            Self::Flush { request, interface } => {
                validate_if_name(&interface)?;
                frame[2] = KIND_FLUSH;
                put_u32(frame, 4, request);
                frame[8..24].copy_from_slice(&interface);
            }
            Self::Lost { request } => {
                frame[2] = KIND_LOST;
                put_u32(frame, 4, request);
            }
        }
        Ok(len)
    }
}

/// Write `answer`'s payload into `payload`, returning its kind.
fn encode_answer(answer: &Answer<'_>, payload: &mut [u8]) -> Result<u8, Errno> {
    let bounded = |bytes: &[u8], max: usize| {
        if bytes.is_empty() || bytes.len() > max {
            Err(Errno::LengthOutOfRange)
        } else {
            Ok(())
        }
    };
    match *answer {
        Answer::Instance { label } => {
            bounded(label, LABEL_MAX)?;
            payload[0] = u8::try_from(label.len()).map_err(|_| Errno::LengthOutOfRange)?;
            payload[1..].copy_from_slice(label);
            Ok(KIND_INSTANCE)
        }
        Answer::Service {
            priority,
            weight,
            port,
            target,
        } => {
            bounded(target, NAME_MAX)?;
            put_u16(payload, 0, priority);
            put_u16(payload, 2, weight);
            put_u16(payload, 4, port);
            payload[6] = u8::try_from(target.len()).map_err(|_| Errno::LengthOutOfRange)?;
            payload[7..].copy_from_slice(target);
            Ok(KIND_SERVICE)
        }
        Answer::Text { octets } => {
            bounded(octets, TXT_MAX)?;
            put_u16(
                payload,
                0,
                u16::try_from(octets.len()).map_err(|_| Errno::LengthOutOfRange)?,
            );
            payload[2..].copy_from_slice(octets);
            Ok(KIND_TEXT)
        }
        Answer::Address { address } => {
            let (family, octets) = address_parts(address);
            payload[0] = family.as_u8();
            payload[1..17].copy_from_slice(&octets);
            Ok(KIND_ADDRESS)
        }
        Answer::Pointer { target } => {
            bounded(target, NAME_MAX)?;
            payload[0] = u8::try_from(target.len()).map_err(|_| Errno::LengthOutOfRange)?;
            payload[1..].copy_from_slice(target);
            Ok(KIND_POINTER)
        }
        Answer::Type { service } => {
            service.check()?;
            payload[0] = service.transport.as_u8();
            payload[1] = u8::try_from(service.name.len()).map_err(|_| Errno::LengthOutOfRange)?;
            payload[2..].copy_from_slice(service.name);
            Ok(KIND_TYPE)
        }
    }
}

/// Decode one entry spanning exactly `bytes`.
fn decode_entry(bytes: &[u8]) -> Result<Entry<'_>, Errno> {
    let header = bytes.get(..ENTRY_HEADER_LEN).ok_or(Errno::BufferTooSmall)?;
    let kind = header[2];
    let change = header[3];
    let request = read_u32(header, 4);
    let mut interface = [0u8; IF_NAME_LEN];
    interface.copy_from_slice(&header[8..24]);
    let ttl = read_u32(header, 24);
    let payload = &bytes[ENTRY_HEADER_LEN..];
    let exact = |len: usize| {
        if payload.len() == len {
            Ok(())
        } else {
            Err(Errno::LengthOutOfRange)
        }
    };
    let counted = |at: usize, max: usize| -> Result<&[u8], Errno> {
        let len = usize::from(*payload.get(at).ok_or(Errno::BufferTooSmall)?);
        if len == 0 || len > max {
            return Err(Errno::LengthOutOfRange);
        }
        exact(at + 1 + len)?;
        Ok(&payload[at + 1..])
    };
    let answer = match kind {
        KIND_FLUSH | KIND_LOST => {
            if change != 0 || ttl != 0 || !payload.is_empty() {
                return Err(Errno::BadMagic);
            }
            if kind == KIND_LOST {
                if interface.iter().any(|&byte| byte != 0) {
                    return Err(Errno::BadMagic);
                }
                return Ok(Entry::Lost { request });
            }
            validate_if_name(&interface)?;
            return Ok(Entry::Flush { request, interface });
        }
        KIND_INSTANCE => Answer::Instance {
            label: counted(0, LABEL_MAX)?,
        },
        KIND_SERVICE => {
            if payload.len() < 7 {
                return Err(Errno::BufferTooSmall);
            }
            Answer::Service {
                priority: read_u16(payload, 0),
                weight: read_u16(payload, 2),
                port: read_u16(payload, 4),
                target: counted(6, NAME_MAX)?,
            }
        }
        KIND_TEXT => {
            let len = usize::from(read_u16(payload.get(..2).ok_or(Errno::BufferTooSmall)?, 0));
            if len == 0 || len > TXT_MAX {
                return Err(Errno::LengthOutOfRange);
            }
            exact(2 + len)?;
            Answer::Text {
                octets: &payload[2..],
            }
        }
        KIND_ADDRESS => {
            exact(17)?;
            Answer::Address {
                address: read_address(payload[0], &payload[1..17])?,
            }
        }
        KIND_POINTER => Answer::Pointer {
            target: counted(0, NAME_MAX)?,
        },
        KIND_TYPE => {
            let transport = Transport::from_u8(*payload.first().ok_or(Errno::BufferTooSmall)?)?;
            Answer::Type {
                service: ServiceTypeField {
                    name: counted(1, SERVICE_NAME_MAX)?,
                    transport,
                },
            }
        }
        _ => return Err(Errno::OutOfRange),
    };
    validate_if_name(&interface)?;
    Ok(Entry::Answer {
        request,
        interface,
        change: Change::from_u8(change)?,
        ttl,
        answer,
    })
}

/// Builds a collect reply into a caller-held buffer.
#[derive(Debug)]
pub struct CollectWriter<'a> {
    out: &'a mut [u8],
    len: usize,
    count: u16,
}

impl<'a> CollectWriter<'a> {
    /// Start a successful reply in `out`, which must hold at least its
    /// header.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] when it cannot.
    pub fn new(out: &'a mut [u8]) -> Result<Self, Errno> {
        if out.len() < COLLECT_HEADER_LEN {
            return Err(Errno::BufferTooSmall);
        }
        Ok(Self {
            out,
            len: COLLECT_HEADER_LEN,
            count: 0,
        })
    }

    /// Append `entry`, or report that it does not fit and leave the reply as
    /// it was.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] when it does not fit, or the entry's own
    /// refusal.
    pub fn push(&mut self, entry: &Entry<'_>) -> Result<(), Errno> {
        let count = self.count.checked_add(1).ok_or(Errno::LengthOutOfRange)?;
        let written = entry.encode(&mut self.out[self.len..])?;
        self.len += written;
        self.count = count;
        Ok(())
    }

    /// Whether nothing has been appended.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Write the header and return the reply's length. `more` tells the
    /// caller further entries are waiting, so it collects again rather than
    /// waiting for a ring.
    #[must_use]
    pub fn finish(self, more: bool) -> usize {
        self.out[..STATUS_REPLY_LEN].copy_from_slice(&encode_status_reply(Ok(())));
        put_u16(self.out, STATUS_REPLY_LEN, self.count);
        self.out[STATUS_REPLY_LEN + 2] = u8::from(more);
        self.out[STATUS_REPLY_LEN + 3] = 0;
        self.len
    }
}

/// A decoded collect reply: its entries, each validated before any is
/// yielded, and whether more are waiting.
#[derive(Copy, Clone, Debug)]
pub struct CollectReply<'a> {
    entries: &'a [u8],
    count: u16,
    /// Further entries are waiting: collect again at once.
    pub more: bool,
}

impl<'a> CollectReply<'a> {
    /// Decode a collect reply, validating every entry.
    ///
    /// # Errors
    ///
    /// The service's refusal, or [`Errno::BadMagic`],
    /// [`Errno::LengthOutOfRange`], [`Errno::BufferTooSmall`], or
    /// [`Errno::OutOfRange`] for any malformed part: a reply is taken whole
    /// or not at all.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Errno> {
        decode_status_reply(bytes)?;
        let header = bytes
            .get(..COLLECT_HEADER_LEN)
            .ok_or(Errno::BufferTooSmall)?;
        let count = read_u16(header, STATUS_REPLY_LEN);
        let more = match header[STATUS_REPLY_LEN + 2] {
            0 => false,
            1 => true,
            _ => return Err(Errno::BadMagic),
        };
        if header[STATUS_REPLY_LEN + 3] != 0 {
            return Err(Errno::BadMagic);
        }
        let reply = Self {
            entries: &bytes[COLLECT_HEADER_LEN..],
            count,
            more,
        };
        let mut seen = 0u16;
        let mut rest = reply.entries;
        while !rest.is_empty() {
            let (entry, tail) = split_entry(rest)?;
            decode_entry(entry)?;
            seen = seen.checked_add(1).ok_or(Errno::LengthOutOfRange)?;
            rest = tail;
        }
        if seen != count {
            return Err(Errno::LengthOutOfRange);
        }
        Ok(reply)
    }

    /// The number of entries.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.count as usize
    }

    /// Whether there are none.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The entries, in the order the service queued them.
    pub fn entries(&self) -> impl Iterator<Item = Entry<'a>> + 'a {
        let mut rest = self.entries;
        core::iter::from_fn(move || {
            let (entry, tail) = split_entry(rest).ok()?;
            rest = tail;
            // Validated whole by `parse`.
            decode_entry(entry).ok()
        })
    }
}

/// Split the first length-prefixed entry off `bytes`.
fn split_entry(bytes: &[u8]) -> Result<(&[u8], &[u8]), Errno> {
    let len = usize::from(read_u16(bytes.get(..2).ok_or(Errno::BufferTooSmall)?, 0));
    if !(ENTRY_HEADER_LEN..=ENTRY_MAX).contains(&len) {
        return Err(Errno::LengthOutOfRange);
    }
    if bytes.len() < len {
        return Err(Errno::BufferTooSmall);
    }
    Ok(bytes.split_at(len))
}

#[cfg(test)]
#[path = "discovery_ipc_tests.rs"]
mod tests;
