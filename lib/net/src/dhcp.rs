//! `DHCPv4` client engine (RFC 2131 / RFC 2132), pure and host-testable.
//!
//! This module is the one definition of the `DHCPv4` client TAIRiX speaks
//! (`plans/DHCP.md`): the BOOTP/DHCP wire codec plus the RFC 2131 §4.4
//! client state machine. It is pure — no I/O, no syscalls, no allocation
//! sized by attacker input — and driven entirely by injected monotonic
//! time and caller-supplied CSPRNG values (the transaction id and the
//! backoff jitter), exactly as [`crate::tcp::conn`] takes its initial
//! sequence number from the caller. The engine never generates randomness.
//!
//! The netstack integration (`plans/DHCP.md` D2) frames the engine's
//! output as UDP(68→67)/IPv4/Ethernet on the interface's own link and feeds
//! received DHCP datagrams back in; it lives beside SLAAC as one more
//! address-configuration source, not a userland socket client.
//!
//! # Security
//!
//! Every server message is attacker-controlled. [`DhcpReply::parse`] is
//! total (never panics), bounded (a fixed option-region walk, fixed-capacity
//! router/DNS/time-server lists — [`MAX_ADDRESSES`]), and fail-closed: a
//! malformed or
//! internally inconsistent message yields `None` and nothing is applied.
//! Off-path spoofing is bounded by the randomised transaction id; the state
//! machine additionally rejects any reply whose `xid` or `chaddr` does not
//! match the outstanding request.

use tairix_abi::driver::net::{MacAddress, MAC_ADDRESS_LEN};

use crate::addr::Ipv4Addr;

/// UDP port a DHCP client listens on / sources from (RFC 2131 §4.1).
pub const CLIENT_PORT: u16 = 68;

/// UDP port a DHCP server listens on / sources from (RFC 2131 §4.1).
pub const SERVER_PORT: u16 = 67;

/// `op` field: a client→server message (RFC 2131 §2). Public so a test
/// harness that speaks the *server* side of the exchange encodes the same
/// wire layout this client codec decodes, never a divergent second copy.
pub const OP_BOOTREQUEST: u8 = 1;
/// `op` field: a server→client message.
pub const OP_BOOTREPLY: u8 = 2;

/// `htype` for a 10 Mb Ethernet address (RFC 2131 §2, per the ARP hardware
/// type registry).
pub const HTYPE_ETHERNET: u8 = 1;

/// `hlen` for an Ethernet hardware address, as the wire `hlen` field
/// carries it. Asserted equal to [`MAC_ADDRESS_LEN`] so the wire value and
/// the address length can never diverge.
pub const HLEN_ETHERNET: u8 = 6;
const _: () = assert!(HLEN_ETHERNET as usize == MAC_ADDRESS_LEN);

/// The four-octet magic cookie that precedes the options field
/// (RFC 2131 §3, value `99.130.83.99`).
pub const MAGIC_COOKIE: [u8; 4] = [99, 130, 83, 99];

/// Length of the fixed BOOTP header preceding the magic cookie
/// (RFC 2131 §2: `op`..`file`, i.e. through the 128-byte boot-file name).
pub const BOOTP_HEADER_LEN: usize = 236;

/// Offset of the magic cookie / start of the options field.
pub const OPTIONS_OFFSET: usize = BOOTP_HEADER_LEN + MAGIC_COOKIE.len();

/// Offset of the four-octet transaction id (`xid`) in the BOOTP header.
pub const XID_OFFSET: usize = 4;
/// Offset of the two-octet `secs` field.
pub const SECS_OFFSET: usize = 8;
/// Offset of the two-octet `flags` field (the broadcast bit lives here).
pub const FLAGS_OFFSET: usize = 10;
/// Offset of the four-octet client address (`ciaddr`).
pub const CIADDR_OFFSET: usize = 12;
/// Offset of the four-octet "your" address (`yiaddr`) the server assigns.
pub const YIADDR_OFFSET: usize = 16;
/// Offset of the client hardware address (`chaddr`); the first
/// [`MAC_ADDRESS_LEN`] octets carry the Ethernet MAC.
pub const CHADDR_OFFSET: usize = 28;

/// Offset of the `sname` (server host name) field within the BOOTP header.
const SNAME_OFFSET: usize = 44;
/// Length of the `sname` field.
const SNAME_LEN: usize = 64;
/// Offset of the `file` (boot file name) field within the BOOTP header.
const FILE_OFFSET: usize = 108;
/// Length of the `file` field.
const FILE_LEN: usize = 128;

/// Largest number of routers, DNS servers, or time servers surfaced from one
/// reply. Extra entries past this fixed bound are ignored (a fixed security
/// bound, never an attacker-sized allocation).
pub const MAX_ADDRESSES: usize = 4;

/// The `0xFFFF_FFFF` lease time meaning "infinite" (RFC 2131 §3.3): no
/// renewal is ever scheduled for such a lease.
pub const INFINITE_LEASE_SECS: u32 = u32::MAX;

/// DHCP option codes (RFC 2132). Public so a test harness encoding the
/// server side of the exchange names the same option registry this client
/// codec decodes, never a divergent copy.
pub mod opt {
    /// Padding octet (no length, no value); skipped on parse.
    pub const PAD: u8 = 0;
    /// Subnet mask (option 1).
    pub const SUBNET_MASK: u8 = 1;
    /// Default router list (option 3).
    pub const ROUTER: u8 = 3;
    /// Domain name server list (option 6).
    pub const DOMAIN_NAME_SERVER: u8 = 6;
    /// Network time protocol server list (option 42).
    pub const NTP_SERVER: u8 = 42;
    /// Requested IP address (option 50).
    pub const REQUESTED_IP: u8 = 50;
    /// IP address lease time in seconds (option 51).
    pub const LEASE_TIME: u8 = 51;
    /// Option overload (option 52): `file`/`sname` carry options.
    pub const OVERLOAD: u8 = 52;
    /// DHCP message type (option 53).
    pub const MESSAGE_TYPE: u8 = 53;
    /// Server identifier (option 54).
    pub const SERVER_ID: u8 = 54;
    /// Parameter request list (option 55).
    pub const PARAMETER_REQUEST_LIST: u8 = 55;
    /// Maximum DHCP message size the client can reassemble (option 57).
    pub const MAX_MESSAGE_SIZE: u8 = 57;
    /// Renewal (T1) time in seconds (option 58).
    pub const RENEWAL_TIME: u8 = 58;
    /// Rebinding (T2) time in seconds (option 59).
    pub const REBINDING_TIME: u8 = 59;
    /// Client identifier (option 61): hardware type + hardware address.
    pub const CLIENT_ID: u8 = 61;
    /// End-of-options marker (option 255).
    pub const END: u8 = 255;
}

/// A DHCP message type (RFC 2132 §9.6, option 53).
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum MessageType {
    /// Client broadcast to locate available servers.
    Discover = 1,
    /// Server offer of configuration parameters.
    Offer = 2,
    /// Client request/accept/renew of offered parameters.
    Request = 3,
    /// Client notice that the offered address is already in use.
    Decline = 4,
    /// Server acknowledgement with committed configuration.
    Ack = 5,
    /// Server refusal of the client's request.
    Nak = 6,
    /// Client relinquishment of its lease.
    Release = 7,
    /// Client request for local configuration without an address.
    Inform = 8,
}

impl MessageType {
    /// The wire value.
    #[must_use]
    pub const fn code(self) -> u8 {
        self as u8
    }

    /// The message type a `code` denotes, or `None` for an unrecognised
    /// value (fail closed).
    #[must_use]
    pub const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Discover),
            2 => Some(Self::Offer),
            3 => Some(Self::Request),
            4 => Some(Self::Decline),
            5 => Some(Self::Ack),
            6 => Some(Self::Nak),
            7 => Some(Self::Release),
            8 => Some(Self::Inform),
            _ => None,
        }
    }
}

/// A bounded list of IPv4 addresses surfaced from a reply option (routers,
/// DNS servers, or time servers). Holds at most [`MAX_ADDRESSES`]; entries
/// past that are
/// dropped, so a hostile option can never size an allocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AddressList {
    entries: [Ipv4Addr; MAX_ADDRESSES],
    len: usize,
}

impl Default for AddressList {
    fn default() -> Self {
        Self {
            entries: [Ipv4Addr::UNSPECIFIED; MAX_ADDRESSES],
            len: 0,
        }
    }
}

impl AddressList {
    /// The addresses collected, in wire order.
    #[must_use]
    pub fn as_slice(&self) -> &[Ipv4Addr] {
        &self.entries[..self.len]
    }

    /// The number of addresses collected.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the list is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The first address, if any (the address a single-value consumer —
    /// e.g. the default router — takes).
    #[must_use]
    pub fn first(&self) -> Option<Ipv4Addr> {
        (self.len > 0).then_some(self.entries[0])
    }

    /// Append `addr` unless the fixed capacity is already reached.
    fn push(&mut self, addr: Ipv4Addr) {
        if self.len < MAX_ADDRESSES {
            self.entries[self.len] = addr;
            self.len += 1;
        }
    }

    /// Append every four-octet address in `data`, up to capacity. A
    /// trailing partial group (not a multiple of four) is ignored.
    fn extend_from_bytes(&mut self, data: &[u8]) {
        for group in data.as_chunks::<4>().0 {
            self.push(Ipv4Addr::new(group[0], group[1], group[2], group[3]));
        }
    }
}

/// The configuration a server offers or acknowledges, parsed from a
/// server→client DHCP message. Only the fields a client acts on are
/// surfaced; unrecognised options are ignored.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DhcpReply {
    /// The message type (option 53); always present in a valid reply.
    pub message_type: MessageType,
    /// The transaction id echoed from the client's request.
    pub xid: u32,
    /// The address the server assigns (`yiaddr`). Zero for a NAK.
    pub your_addr: Ipv4Addr,
    /// The DHCP server's identity (option 54), if present. The value a
    /// client unicasts a renewal or a decline to.
    pub server_id: Option<Ipv4Addr>,
    /// The subnet mask (option 1), if present.
    pub subnet_mask: Option<Ipv4Addr>,
    /// The default routers (option 3), in wire order.
    pub routers: AddressList,
    /// The DNS servers (option 6), in wire order.
    pub dns_servers: AddressList,
    /// The network time servers (option 42), in wire order.
    pub ntp_servers: AddressList,
    /// The lease duration in seconds (option 51), if present.
    pub lease_secs: Option<u32>,
    /// The renewal (T1) time in seconds (option 58), if present.
    pub renewal_secs: Option<u32>,
    /// The rebinding (T2) time in seconds (option 59), if present.
    pub rebinding_secs: Option<u32>,
}

/// Mutable accumulator for [`DhcpReply::parse`]: recognised options are
/// folded in as the option regions are walked, then finalised into a
/// [`DhcpReply`] once a message type is known.
#[derive(Default)]
struct ReplyBuilder {
    message_type: Option<MessageType>,
    server_id: Option<Ipv4Addr>,
    subnet_mask: Option<Ipv4Addr>,
    routers: AddressList,
    dns_servers: AddressList,
    ntp_servers: AddressList,
    lease_secs: Option<u32>,
    renewal_secs: Option<u32>,
    rebinding_secs: Option<u32>,
    /// The option-overload value (option 52): bit 0 = `file` field carries
    /// options, bit 1 = `sname` field carries options (RFC 2131 §4.1).
    overload: u8,
}

/// Read a four-octet IPv4 address from an option value, or `None` if the
/// value is not exactly four bytes (fail closed).
fn addr_option(data: &[u8]) -> Option<Ipv4Addr> {
    let octets: [u8; 4] = data.try_into().ok()?;
    Some(Ipv4Addr::from(octets))
}

/// Read a big-endian `u32` from an option value, or `None` if the value is
/// not exactly four bytes (fail closed).
fn u32_option(data: &[u8]) -> Option<u32> {
    let bytes: [u8; 4] = data.try_into().ok()?;
    Some(u32::from_be_bytes(bytes))
}

impl ReplyBuilder {
    /// Fold one option (already split into `code` and its `data`) into the
    /// builder. Unrecognised codes are ignored; a recognised code with a
    /// malformed value is ignored rather than aborting the whole parse,
    /// since the message-type and consistency checks in
    /// [`DhcpReply::parse`] are what make a reply acceptable.
    fn absorb(&mut self, code: u8, data: &[u8]) {
        match code {
            opt::MESSAGE_TYPE => {
                if let [value] = data {
                    self.message_type = MessageType::from_code(*value);
                }
            }
            opt::SUBNET_MASK => self.subnet_mask = addr_option(data).or(self.subnet_mask),
            opt::ROUTER => self.routers.extend_from_bytes(data),
            opt::DOMAIN_NAME_SERVER => self.dns_servers.extend_from_bytes(data),
            opt::NTP_SERVER => self.ntp_servers.extend_from_bytes(data),
            opt::SERVER_ID => self.server_id = addr_option(data).or(self.server_id),
            opt::LEASE_TIME => self.lease_secs = u32_option(data).or(self.lease_secs),
            opt::RENEWAL_TIME => self.renewal_secs = u32_option(data).or(self.renewal_secs),
            opt::REBINDING_TIME => self.rebinding_secs = u32_option(data).or(self.rebinding_secs),
            opt::OVERLOAD => {
                if let [value] = data {
                    self.overload = *value;
                }
            }
            _ => {}
        }
    }
}

/// Walk one TLV option region, folding each recognised option into
/// `builder`. Returns when the region ends (an `END` option or the region
/// is exhausted). Totally bounded by `region.len()`: `PAD` consumes one
/// byte, every other option consumes its `1 + 1 + len` bytes, and a
/// truncated length/value ends the walk (fail closed).
fn walk_options(builder: &mut ReplyBuilder, region: &[u8]) {
    let mut i = 0;
    while i < region.len() {
        let code = region[i];
        i += 1;
        match code {
            opt::PAD => continue,
            opt::END => break,
            _ => {}
        }
        let Some(&len) = region.get(i) else {
            break;
        };
        i += 1;
        let len = usize::from(len);
        let Some(data) = region.get(i..i + len) else {
            break;
        };
        builder.absorb(code, data);
        i += len;
    }
}

impl DhcpReply {
    /// Parse a server→client DHCP message from `bytes` (the UDP payload)
    /// under the client's own `xid` and hardware address `chaddr`.
    ///
    /// Returns `None` (fail closed) for any of: a truncated BOOTP header, a
    /// wrong `op`/`htype`/`hlen`, a missing or wrong magic cookie, a
    /// transaction id or client hardware address that does not match this
    /// client's outstanding request (bounding off-path spoofing), or an
    /// options field carrying no message type (option 53). Unrecognised
    /// options are ignored; option overload (RFC 2131 §4.1) is honoured by
    /// additionally walking the `file` then `sname` fields.
    #[must_use]
    pub fn parse(bytes: &[u8], xid: u32, chaddr: MacAddress) -> Option<Self> {
        let header = bytes.get(..OPTIONS_OFFSET)?;
        if header[0] != OP_BOOTREPLY || header[1] != HTYPE_ETHERNET {
            return None;
        }
        // hlen must name an Ethernet address; a longer value would mean the
        // 16-byte chaddr field carries something we did not send.
        if usize::from(header[2]) != MAC_ADDRESS_LEN {
            return None;
        }
        let msg_xid = u32::from_be_bytes([
            header[XID_OFFSET],
            header[XID_OFFSET + 1],
            header[XID_OFFSET + 2],
            header[XID_OFFSET + 3],
        ]);
        if msg_xid != xid {
            return None;
        }
        // Match our hardware address in the first six chaddr octets.
        if header[CHADDR_OFFSET..CHADDR_OFFSET + MAC_ADDRESS_LEN] != chaddr.0 {
            return None;
        }
        if header[BOOTP_HEADER_LEN..OPTIONS_OFFSET] != MAGIC_COOKIE {
            return None;
        }
        let your_addr = Ipv4Addr::new(
            header[YIADDR_OFFSET],
            header[YIADDR_OFFSET + 1],
            header[YIADDR_OFFSET + 2],
            header[YIADDR_OFFSET + 3],
        );

        let mut builder = ReplyBuilder::default();
        walk_options(&mut builder, &bytes[OPTIONS_OFFSET..]);
        // RFC 2131 §4.1 option overload: the `file` then the `sname` field
        // may carry the tail of the options, walked in that order.
        if builder.overload & 0b01 != 0 {
            if let Some(region) = bytes.get(FILE_OFFSET..FILE_OFFSET + FILE_LEN) {
                walk_options(&mut builder, region);
            }
        }
        if builder.overload & 0b10 != 0 {
            if let Some(region) = bytes.get(SNAME_OFFSET..SNAME_OFFSET + SNAME_LEN) {
                walk_options(&mut builder, region);
            }
        }

        Some(Self {
            message_type: builder.message_type?,
            xid: msg_xid,
            your_addr,
            server_id: builder.server_id,
            subnet_mask: builder.subnet_mask,
            routers: builder.routers,
            dns_servers: builder.dns_servers,
            ntp_servers: builder.ntp_servers,
            lease_secs: builder.lease_secs,
            renewal_secs: builder.renewal_secs,
            rebinding_secs: builder.rebinding_secs,
        })
    }
}

/// The `flags` broadcast bit (RFC 2131 §4.1): set when the client cannot
/// yet receive a unicast reply (it has no configured address), so the
/// server must broadcast its answer.
const FLAG_BROADCAST: u16 = 0x8000;

/// The maximum DHCP message length the client advertises it can reassemble
/// (option 57). RFC 2132 §9.10 requires at least 576; the client's replies
/// are small, and 576 is the guaranteed-deliverable IPv4 datagram size.
const MAX_MESSAGE_SIZE_ADVERTISED: u16 = 576;

/// The parameter request list (option 55) the client asks every server to
/// populate: the addressing facts an interface needs to come up.
const PARAMETER_REQUEST_LIST: [u8; 7] = [
    opt::SUBNET_MASK,
    opt::ROUTER,
    opt::DOMAIN_NAME_SERVER,
    opt::NTP_SERVER,
    opt::LEASE_TIME,
    opt::RENEWAL_TIME,
    opt::REBINDING_TIME,
];

/// Smallest DHCP message the client emits. Legacy BOOTP relays and some
/// servers drop a message whose BOOTP portion is shorter than 300 octets,
/// so the options field is padded with `PAD` up to this length (the extra
/// bytes are semantically empty). RFC 2131 §4.1 permits the padding.
const MIN_MESSAGE_LEN: usize = 300;

/// The buffer size that always suffices for [`write_message`]: the fixed
/// header, cookie, every option the client emits, and the minimum-length
/// padding. A caller sizes its transmit buffer with this constant.
pub const MAX_MESSAGE_LEN: usize = MIN_MESSAGE_LEN;

/// A fully-specified client→server DHCP message, ready for [`write_message`].
/// The state machine produces one of these per transmission; the encoder is the
/// single definition of the wire form that DISCOVER / REQUEST / DECLINE /
/// RELEASE all share.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MessageSpec {
    /// The message type (option 53).
    pub message_type: MessageType,
    /// The transaction id tying the exchange together.
    pub xid: u32,
    /// Seconds elapsed since the client began the current acquisition,
    /// clamped into the 16-bit `secs` field.
    pub secs: u16,
    /// Whether to set the broadcast flag (the client cannot yet receive a
    /// unicast reply).
    pub broadcast: bool,
    /// The client's current address (`ciaddr`): the leased address in
    /// RENEWING/REBINDING/RELEASE, unspecified (`0.0.0.0`) otherwise.
    pub client_addr: Ipv4Addr,
    /// The client's hardware address.
    pub chaddr: MacAddress,
    /// The requested address (option 50): set in a SELECTING REQUEST and a
    /// DECLINE, absent otherwise.
    pub requested_addr: Option<Ipv4Addr>,
    /// The server identifier (option 54): set in a SELECTING REQUEST, a
    /// DECLINE, and a RELEASE, absent otherwise.
    pub server_id: Option<Ipv4Addr>,
}

/// Errors from [`write_message`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriteError {
    /// `out` is smaller than the encoded message ([`MAX_MESSAGE_LEN`]
    /// always suffices).
    BufferTooSmall,
}

/// A cursor writing options into a buffer, tracking the write position and
/// failing closed once the buffer is full.
struct OptionWriter<'a> {
    out: &'a mut [u8],
    pos: usize,
}

impl OptionWriter<'_> {
    /// Append one raw byte, or record overflow.
    fn byte(&mut self, value: u8) -> Result<(), WriteError> {
        let slot = self
            .out
            .get_mut(self.pos)
            .ok_or(WriteError::BufferTooSmall)?;
        *slot = value;
        self.pos += 1;
        Ok(())
    }

    /// Append a `code`/`data` TLV option.
    fn option(&mut self, code: u8, data: &[u8]) -> Result<(), WriteError> {
        let len = u8::try_from(data.len()).map_err(|_| WriteError::BufferTooSmall)?;
        self.byte(code)?;
        self.byte(len)?;
        let end = self.pos + data.len();
        let slot = self
            .out
            .get_mut(self.pos..end)
            .ok_or(WriteError::BufferTooSmall)?;
        slot.copy_from_slice(data);
        self.pos = end;
        Ok(())
    }
}

/// Encode `spec` into `out`, returning the number of bytes written.
///
/// The output is a complete BOOTP/DHCP client message: the fixed header,
/// the magic cookie, the DHCP options (message type, client identifier,
/// max message size, parameter request list, and the optional requested
/// address and server identifier), an `END` option, and `PAD` padding to
/// the minimum message length ([`MAX_MESSAGE_LEN`]).
///
/// # Errors
///
/// [`WriteError::BufferTooSmall`] if `out` is shorter than the encoded
/// message; [`MAX_MESSAGE_LEN`] always suffices.
pub fn write_message(spec: &MessageSpec, out: &mut [u8]) -> Result<usize, WriteError> {
    let out = out
        .get_mut(..MIN_MESSAGE_LEN)
        .ok_or(WriteError::BufferTooSmall)?;
    out.fill(0);
    out[0] = OP_BOOTREQUEST;
    out[1] = HTYPE_ETHERNET;
    out[2] = HLEN_ETHERNET;
    // hops = 0 (out[3]); a client never sets it.
    out[XID_OFFSET..XID_OFFSET + 4].copy_from_slice(&spec.xid.to_be_bytes());
    out[SECS_OFFSET..SECS_OFFSET + 2].copy_from_slice(&spec.secs.to_be_bytes());
    let flags = if spec.broadcast { FLAG_BROADCAST } else { 0 };
    out[FLAGS_OFFSET..FLAGS_OFFSET + 2].copy_from_slice(&flags.to_be_bytes());
    out[CIADDR_OFFSET..CIADDR_OFFSET + 4].copy_from_slice(&spec.client_addr.octets());
    // yiaddr/siaddr/giaddr stay zero for a client message.
    out[CHADDR_OFFSET..CHADDR_OFFSET + MAC_ADDRESS_LEN].copy_from_slice(&spec.chaddr.0);
    out[BOOTP_HEADER_LEN..OPTIONS_OFFSET].copy_from_slice(&MAGIC_COOKIE);

    let mut w = OptionWriter {
        out,
        pos: OPTIONS_OFFSET,
    };
    w.option(opt::MESSAGE_TYPE, &[spec.message_type.code()])?;
    // Client identifier (RFC 2132 §9.14): hardware type + hardware address,
    // so the server keys the lease to us independently of the address.
    let mut client_id = [0u8; 1 + MAC_ADDRESS_LEN];
    client_id[0] = HTYPE_ETHERNET;
    client_id[1..].copy_from_slice(&spec.chaddr.0);
    w.option(opt::CLIENT_ID, &client_id)?;
    w.option(
        opt::MAX_MESSAGE_SIZE,
        &MAX_MESSAGE_SIZE_ADVERTISED.to_be_bytes(),
    )?;
    if let Some(addr) = spec.requested_addr {
        w.option(opt::REQUESTED_IP, &addr.octets())?;
    }
    if let Some(id) = spec.server_id {
        w.option(opt::SERVER_ID, &id.octets())?;
    }
    w.option(opt::PARAMETER_REQUEST_LIST, &PARAMETER_REQUEST_LIST)?;
    w.byte(opt::END)?;
    // The remaining bytes are already zero (PAD) from the initial fill,
    // padding the message up to MIN_MESSAGE_LEN.
    Ok(MIN_MESSAGE_LEN)
}

// ---------------------------------------------------------------------------
// Client state machine (RFC 2131 §4.4)
// ---------------------------------------------------------------------------

use tairix_abi::time::Duration64;

use crate::timeutil::{from_nanos, nanos, NEVER, ONE_SEC_NANOS};

/// Initial retransmission delay (RFC 2131 §4.1): four seconds, doubled on
/// each retransmission up to [`MAX_BACKOFF_SECS`].
const INITIAL_BACKOFF_SECS: u64 = 4;

/// Ceiling for the doubled retransmission delay (RFC 2131 §4.1).
const MAX_BACKOFF_SECS: u64 = 64;

/// Retransmissions of a SELECTING-form REQUEST before the client gives up
/// and restarts from INIT (RFC 2131 §4.4.1: "retransmit ... four times").
const MAX_REQUEST_RETRIES: u32 = 4;

/// Floor for the renewing/rebinding retransmission interval (RFC 2131
/// §4.4.5: "a minimum of 60 seconds").
const MIN_RENEW_RETRANSMIT_SECS: u128 = 60;

/// The DHCP client's position in the RFC 2131 §4.4 lease lifecycle.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum State {
    /// No lease and no exchange in progress; the next [`DhcpClient::poll`]
    /// begins acquisition by broadcasting a DISCOVER.
    Init,
    /// A DISCOVER has been sent; awaiting OFFERs.
    Selecting,
    /// An OFFER was accepted and a REQUEST sent; awaiting ACK/NAK.
    Requesting,
    /// A lease is held and in use; renewal is scheduled for T1.
    Bound,
    /// Past T1: unicasting REQUESTs to the leasing server to renew.
    Renewing,
    /// Past T2: broadcasting REQUESTs to any server to rebind.
    Rebinding,
}

/// A committed lease's configuration, handed to the interface layer to
/// apply (or, on withdrawal, the layer already knows what to remove).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Lease {
    /// The leased address.
    pub addr: Ipv4Addr,
    /// The subnet mask (option 1), if the server supplied one.
    pub subnet_mask: Option<Ipv4Addr>,
    /// The default router (the first of option 3), if any.
    pub router: Option<Ipv4Addr>,
    /// The DNS servers (option 6), in wire order.
    pub dns_servers: AddressList,
    /// The network time servers (option 42), in wire order.
    pub ntp_servers: AddressList,
    /// The DHCP server that granted the lease (option 54), if known.
    pub server_id: Option<Ipv4Addr>,
    /// The lease duration in seconds ([`INFINITE_LEASE_SECS`] for a
    /// permanent lease).
    pub lease_secs: u32,
}

/// Where a [`SendAction`] message must be delivered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Destination {
    /// Link-layer broadcast to `255.255.255.255:67` (the client cannot yet
    /// unicast, or is rebinding to any server).
    Broadcast,
    /// Unicast to a known server at `:67` (a RENEWING renewal).
    Server(Ipv4Addr),
}

/// A message the client must transmit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SendAction {
    /// The message to encode with [`write_message`].
    pub spec: MessageSpec,
    /// Where to send it.
    pub destination: Destination,
}

/// An action the interface layer must carry out in response to a
/// [`DhcpClient::poll`] or [`DhcpClient::on_reply`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    /// Transmit a DHCP message.
    Send(SendAction),
    /// A lease was acquired or renewed: apply this configuration.
    Configured(Lease),
    /// The lease was lost (NAK or expiry): withdraw the applied
    /// configuration. The client returns to INIT and re-acquires.
    Deconfigured,
}

/// The pure RFC 2131 `DHCPv4` client state machine.
///
/// Construct with [`DhcpClient::new`], then drive it event-first:
///
/// - call [`DhcpClient::poll`] once at start-up and again whenever the
///   one-shot timer armed from [`DhcpClient::next_deadline`] fires, to
///   advance retransmissions and the T1/T2/expiry transitions;
/// - call [`DhcpClient::on_reply`] with each server message the interface
///   receives on UDP port 68.
///
/// Both return the [`Action`]s the interface layer must perform. The engine
/// owns no I/O and never blocks; the caller supplies monotonic `now` values
/// and, through `rng`, the CSPRNG randomness RFC 2131 requires for the
/// transaction id and backoff jitter.
#[derive(Clone, Debug)]
pub struct DhcpClient {
    chaddr: MacAddress,
    state: State,
    xid: u32,
    /// Nanosecond instant the current acquisition/renewal process began
    /// (drives the wire `secs` field).
    process_started: u128,
    /// Nanosecond instant the REQUEST that produced the current lease was
    /// sent (RFC 2131 §4.4.5 anchors T1/T2/expiry to the request send).
    lease_anchor: u128,
    /// Next retransmission instant, or [`NEVER`] when none is armed.
    retransmit: u128,
    /// Current retransmission backoff in seconds (SELECTING/REQUESTING).
    backoff_secs: u64,
    /// REQUEST retransmissions so far in REQUESTING.
    request_retries: u32,
    /// The offered address and server chosen in SELECTING, carried into the
    /// REQUEST and its retransmissions.
    offered_addr: Ipv4Addr,
    offer_server_id: Option<Ipv4Addr>,
    /// The committed lease (valid in BOUND/RENEWING/REBINDING).
    lease: Option<Lease>,
    /// T1/T2/expiry instants (valid while a finite lease is held).
    t1: u128,
    t2: u128,
    expiry: u128,
}

impl DhcpClient {
    /// Construct a client for the interface whose hardware address is
    /// `chaddr`, in the INIT state. The first [`DhcpClient::poll`] begins
    /// acquisition.
    #[must_use]
    pub fn new(chaddr: MacAddress) -> Self {
        Self {
            chaddr,
            state: State::Init,
            xid: 0,
            process_started: 0,
            lease_anchor: 0,
            retransmit: 0,
            backoff_secs: INITIAL_BACKOFF_SECS,
            request_retries: 0,
            offered_addr: Ipv4Addr::UNSPECIFIED,
            offer_server_id: None,
            lease: None,
            t1: NEVER,
            t2: NEVER,
            expiry: NEVER,
        }
    }

    /// The current lifecycle state.
    #[must_use]
    pub fn state(&self) -> State {
        self.state
    }

    /// The lease currently held, if any (present in BOUND/RENEWING/
    /// REBINDING).
    #[must_use]
    pub fn lease(&self) -> Option<Lease> {
        self.lease
    }

    /// The next instant [`DhcpClient::poll`] has timed work to do, or
    /// `None` when none is armed (a permanent lease in BOUND). The caller
    /// arms one one-shot timer at this instant.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Duration64> {
        let deadline = match self.state {
            State::Init | State::Selecting | State::Requesting => self.retransmit,
            State::Bound => self.t1,
            State::Renewing => self.retransmit.min(self.t2),
            State::Rebinding => self.retransmit.min(self.expiry),
        };
        (deadline != NEVER).then(|| from_nanos(deadline))
    }

    /// The `secs` field value for a message sent at `now_nanos`: seconds
    /// since the current process began, clamped into the 16-bit field.
    fn secs_field(&self, now_nanos: u128) -> u16 {
        let elapsed = now_nanos.saturating_sub(self.process_started) / ONE_SEC_NANOS;
        u16::try_from(elapsed).unwrap_or(u16::MAX)
    }

    /// A retransmission delay in nanoseconds: `backoff_secs` jittered by a
    /// random value in [-1s, +1s] (RFC 2131 §4.1), floored at one second.
    fn backoff_nanos(&self, rng: &mut dyn FnMut() -> u32) -> u128 {
        let base = u128::from(self.backoff_secs) * ONE_SEC_NANOS;
        // A jitter in the closed range [-1s, +1s], in nanoseconds. The
        // draw is a u32 in [0, 2e9]; subtracting 1e9 (both well within i64)
        // yields the signed offset without any lossy cast.
        let jitter = i64::from(rng() % 2_000_000_001) - 1_000_000_000;
        let delayed = base.saturating_add_signed(i128::from(jitter));
        delayed.max(ONE_SEC_NANOS)
    }

    /// Advance the backoff for the next retransmission (double, capped).
    fn grow_backoff(&mut self) {
        self.backoff_secs = (self.backoff_secs * 2).min(MAX_BACKOFF_SECS);
    }

    /// Build a DISCOVER for the current transaction.
    fn discover_action(&self, now_nanos: u128) -> Action {
        Action::Send(SendAction {
            spec: MessageSpec {
                message_type: MessageType::Discover,
                xid: self.xid,
                secs: self.secs_field(now_nanos),
                broadcast: true,
                client_addr: Ipv4Addr::UNSPECIFIED,
                chaddr: self.chaddr,
                requested_addr: None,
                server_id: None,
            },
            destination: Destination::Broadcast,
        })
    }

    /// Build a SELECTING-form REQUEST (broadcast, requested-address +
    /// server-id set, `ciaddr` unspecified).
    fn selecting_request_action(&self, now_nanos: u128) -> Action {
        Action::Send(SendAction {
            spec: MessageSpec {
                message_type: MessageType::Request,
                xid: self.xid,
                secs: self.secs_field(now_nanos),
                broadcast: true,
                client_addr: Ipv4Addr::UNSPECIFIED,
                chaddr: self.chaddr,
                requested_addr: Some(self.offered_addr),
                server_id: self.offer_server_id,
            },
            destination: Destination::Broadcast,
        })
    }

    /// Build a renew/rebind-form REQUEST (`ciaddr` = leased address, no
    /// requested-address or server-id option). `broadcast` distinguishes
    /// REBINDING (broadcast, any server) from RENEWING (unicast).
    fn renew_request_action(&self, now_nanos: u128, addr: Ipv4Addr, dest: Destination) -> Action {
        Action::Send(SendAction {
            spec: MessageSpec {
                message_type: MessageType::Request,
                xid: self.xid,
                secs: self.secs_field(now_nanos),
                broadcast: matches!(dest, Destination::Broadcast),
                client_addr: addr,
                chaddr: self.chaddr,
                requested_addr: None,
                server_id: None,
            },
            destination: dest,
        })
    }

    /// Begin (or restart) acquisition from INIT: draw a fresh transaction
    /// id, reset the process clock and backoff, enter SELECTING, and emit a
    /// DISCOVER.
    fn begin_discover(&mut self, now_nanos: u128, rng: &mut dyn FnMut() -> u32) -> Action {
        self.xid = rng();
        self.process_started = now_nanos;
        self.backoff_secs = INITIAL_BACKOFF_SECS;
        self.state = State::Selecting;
        self.retransmit = now_nanos.saturating_add(self.backoff_nanos(rng));
        self.discover_action(now_nanos)
    }

    /// Advance retransmissions and timed transitions at `now`. Call at
    /// start-up and whenever the [`DhcpClient::next_deadline`] timer fires.
    pub fn poll(
        &mut self,
        now: Duration64,
        rng: &mut dyn FnMut() -> u32,
    ) -> alloc::vec::Vec<Action> {
        let now_nanos = nanos(now);
        let mut actions = alloc::vec::Vec::new();
        match self.state {
            State::Init => {
                if now_nanos >= self.retransmit {
                    actions.push(self.begin_discover(now_nanos, rng));
                }
            }
            State::Selecting => {
                if now_nanos >= self.retransmit {
                    // Keep soliciting: DISCOVER is retransmitted with
                    // growing backoff until an OFFER arrives.
                    self.grow_backoff();
                    self.retransmit = now_nanos.saturating_add(self.backoff_nanos(rng));
                    actions.push(self.discover_action(now_nanos));
                }
            }
            State::Requesting => {
                if now_nanos >= self.retransmit {
                    if self.request_retries >= MAX_REQUEST_RETRIES {
                        // Gave up waiting for ACK/NAK: restart from INIT.
                        self.enter_init(now_nanos);
                        actions.push(self.begin_discover(now_nanos, rng));
                    } else {
                        self.request_retries += 1;
                        self.grow_backoff();
                        self.retransmit = now_nanos.saturating_add(self.backoff_nanos(rng));
                        actions.push(self.selecting_request_action(now_nanos));
                    }
                }
            }
            State::Bound => {
                if now_nanos >= self.t1 {
                    self.enter_renewing(now_nanos, rng);
                    if let Some(lease) = self.lease {
                        actions.push(self.renew_request_action(
                            now_nanos,
                            lease.addr,
                            server_dest(lease.server_id),
                        ));
                    }
                }
            }
            State::Renewing => {
                if now_nanos >= self.t2 {
                    self.enter_rebinding(now_nanos, rng);
                    if let Some(lease) = self.lease {
                        actions.push(self.renew_request_action(
                            now_nanos,
                            lease.addr,
                            Destination::Broadcast,
                        ));
                    }
                } else if now_nanos >= self.retransmit {
                    self.arm_renew_retransmit(now_nanos, self.t2);
                    if let Some(lease) = self.lease {
                        actions.push(self.renew_request_action(
                            now_nanos,
                            lease.addr,
                            server_dest(lease.server_id),
                        ));
                    }
                }
            }
            State::Rebinding => {
                if now_nanos >= self.expiry {
                    // The lease expired without a renewal: drop it and
                    // re-acquire from scratch.
                    self.lease = None;
                    self.enter_init(now_nanos);
                    actions.push(Action::Deconfigured);
                    actions.push(self.begin_discover(now_nanos, rng));
                } else if now_nanos >= self.retransmit {
                    self.arm_renew_retransmit(now_nanos, self.expiry);
                    if let Some(lease) = self.lease {
                        actions.push(self.renew_request_action(
                            now_nanos,
                            lease.addr,
                            Destination::Broadcast,
                        ));
                    }
                }
            }
        }
        actions
    }

    /// Reset the transient acquisition state and arm INIT for an immediate
    /// re-DISCOVER at `now`.
    fn enter_init(&mut self, now_nanos: u128) {
        self.state = State::Init;
        self.retransmit = now_nanos;
        self.backoff_secs = INITIAL_BACKOFF_SECS;
        self.request_retries = 0;
        self.t1 = NEVER;
        self.t2 = NEVER;
        self.expiry = NEVER;
    }

    /// Enter RENEWING: a fresh transaction unicasting REQUESTs to the
    /// leasing server, halving the remaining time to T2 per retransmission.
    fn enter_renewing(&mut self, now_nanos: u128, rng: &mut dyn FnMut() -> u32) {
        self.state = State::Renewing;
        self.xid = rng();
        self.process_started = now_nanos;
        self.lease_anchor = now_nanos;
        self.arm_renew_retransmit(now_nanos, self.t2);
    }

    /// Enter REBINDING: broadcast REQUESTs to any server, halving the
    /// remaining time to expiry per retransmission.
    fn enter_rebinding(&mut self, now_nanos: u128, rng: &mut dyn FnMut() -> u32) {
        self.state = State::Rebinding;
        self.xid = rng();
        self.process_started = now_nanos;
        self.lease_anchor = now_nanos;
        self.arm_renew_retransmit(now_nanos, self.expiry);
    }

    /// Arm the next renew/rebind retransmission: half the remaining time to
    /// `target`, floored at 60 seconds (RFC 2131 §4.4.5).
    fn arm_renew_retransmit(&mut self, now_nanos: u128, target: u128) {
        let remaining = target.saturating_sub(now_nanos);
        let half = (remaining / 2).max(MIN_RENEW_RETRANSMIT_SECS * ONE_SEC_NANOS);
        self.retransmit = now_nanos.saturating_add(half);
    }

    /// Fold a received server message into the state machine at `now`.
    ///
    /// `reply` must already have been parsed against this client's current
    /// [`DhcpClient::transaction_id`] and hardware address (see
    /// [`DhcpReply::parse`]), so an off-path spoof for a different
    /// transaction never reaches here. Returns the resulting [`Action`]s.
    pub fn on_reply(&mut self, now: Duration64, reply: &DhcpReply) -> alloc::vec::Vec<Action> {
        let now_nanos = nanos(now);
        let mut actions = alloc::vec::Vec::new();
        // A stale reply for a previous transaction is ignored (the parser
        // matches xid, but a caller could feed one directly).
        if reply.xid != self.xid {
            return actions;
        }
        match (self.state, reply.message_type) {
            (State::Selecting, MessageType::Offer) => {
                // Accept the first well-formed offer. An offer without a
                // server identifier cannot be requested (RFC 2131 §4.3.2
                // requires the server-id in the REQUEST), so it is ignored.
                let Some(server_id) = reply.server_id else {
                    return actions;
                };
                self.offered_addr = reply.your_addr;
                self.offer_server_id = Some(server_id);
                self.state = State::Requesting;
                self.request_retries = 0;
                self.backoff_secs = INITIAL_BACKOFF_SECS;
                self.lease_anchor = now_nanos;
                self.retransmit =
                    now_nanos.saturating_add(u128::from(INITIAL_BACKOFF_SECS) * ONE_SEC_NANOS);
                actions.push(self.selecting_request_action(now_nanos));
            }
            (State::Requesting | State::Renewing | State::Rebinding, MessageType::Ack) => {
                if let Some(lease) = self.commit_lease(reply) {
                    actions.push(Action::Configured(lease));
                }
            }
            (State::Requesting, MessageType::Nak) => {
                // The server refused: discard the offer and restart.
                self.offer_server_id = None;
                self.enter_init(now_nanos);
            }
            (State::Renewing | State::Rebinding, MessageType::Nak) => {
                // The lease is gone: withdraw configuration and re-acquire.
                self.lease = None;
                self.enter_init(now_nanos);
                actions.push(Action::Deconfigured);
            }
            _ => {}
        }
        actions
    }

    /// Commit the lease an ACK carries: compute T1/T2/expiry, enter BOUND,
    /// and record the lease. Returns the lease to apply, or `None` if the
    /// ACK lacks the mandatory lease-time option (RFC 2131 §3.3) and is
    /// therefore unusable — the client stays put and retransmits.
    fn commit_lease(&mut self, reply: &DhcpReply) -> Option<Lease> {
        let lease_secs = reply.lease_secs?;
        let lease = Lease {
            addr: reply.your_addr,
            subnet_mask: reply.subnet_mask,
            router: reply.routers.first(),
            dns_servers: reply.dns_servers,
            ntp_servers: reply.ntp_servers,
            server_id: reply.server_id.or(self.offer_server_id),
            lease_secs,
        };
        self.lease = Some(lease);
        self.state = State::Bound;
        self.request_retries = 0;
        self.retransmit = NEVER;
        if lease_secs == INFINITE_LEASE_SECS {
            self.t1 = NEVER;
            self.t2 = NEVER;
            self.expiry = NEVER;
        } else {
            let (t1_secs, t2_secs) =
                renewal_times(lease_secs, reply.renewal_secs, reply.rebinding_secs);
            self.t1 = self
                .lease_anchor
                .saturating_add(u128::from(t1_secs) * ONE_SEC_NANOS);
            self.t2 = self
                .lease_anchor
                .saturating_add(u128::from(t2_secs) * ONE_SEC_NANOS);
            self.expiry = self
                .lease_anchor
                .saturating_add(u128::from(lease_secs) * ONE_SEC_NANOS);
        }
        Some(lease)
    }

    /// The transaction id of the outstanding exchange. The interface layer
    /// passes this to [`DhcpReply::parse`] so only a reply to the current
    /// transaction is accepted.
    #[must_use]
    pub fn transaction_id(&self) -> u32 {
        self.xid
    }

    /// The client's hardware address, for [`DhcpReply::parse`].
    #[must_use]
    pub fn hardware_addr(&self) -> MacAddress {
        self.chaddr
    }
}

/// The unicast destination for a server whose identity is known, falling
/// back to broadcast when it is not.
fn server_dest(server_id: Option<Ipv4Addr>) -> Destination {
    server_id.map_or(Destination::Broadcast, Destination::Server)
}

/// Compute the T1 (renewal) and T2 (rebinding) times in seconds for a lease
/// of `lease_secs`, honouring the server-supplied option 58/59 values only
/// when they are internally consistent (`0 < T1 < T2 < lease`), and
/// otherwise falling back to the RFC 2131 §4.4.5 defaults (T1 = lease/2,
/// T2 = lease·7⁄8). This is fail-safe: a server that offers a nonsensical
/// pair never produces an out-of-order or overlong timer.
fn renewal_times(lease_secs: u32, renewal: Option<u32>, rebinding: Option<u32>) -> (u32, u32) {
    let default_t1 = lease_secs / 2;
    // 0.875 · lease, in u64 to avoid overflowing the `· 7` for a large
    // lease; the result is always below `lease_secs`, so the narrowing
    // never truncates (the `unwrap_or` is unreachable, and fails safe).
    let default_t2 = u32::try_from(u64::from(lease_secs) * 7 / 8).unwrap_or(default_t1);
    let t1 = renewal
        .filter(|&t1| t1 > 0 && t1 < lease_secs)
        .unwrap_or(default_t1);
    let t2 = rebinding
        .filter(|&t2| t2 > t1 && t2 < lease_secs)
        .unwrap_or(default_t2);
    // The default T2 can fall at or below a server-supplied T1; keep the
    // ordering strict so the RENEWING→REBINDING transition is well-defined.
    if t1 >= t2 {
        (t2.saturating_sub(1).min(t2 / 2), t2)
    } else {
        (t1, t2)
    }
}

#[cfg(test)]
#[path = "dhcp_tests.rs"]
mod tests;
