//! `DHCPv6` client engine (RFC 8415), pure and host-testable.
//!
//! This module is the one definition of the stateful `DHCPv6` client TAIRiX
//! speaks (`plans/DHCP.md` D4): the RFC 8415 §8 message + §21 option wire
//! codec plus the RFC 8415 §18.2 client state machine. It is a *sibling* of the
//! `DHCPv4` engine ([`crate::dhcp`]), not a `cfg`-fork of it: `DHCPv6` is a
//! distinct protocol (UDP 546↔547, the `ff02::1:2` server multicast,
//! DUID-keyed leases, `IA_NA/IAADDR` bindings, a four-message
//! Solicit/Advertise/Request/Reply exchange). Like every engine in this
//! crate it is pure — no I/O, no syscalls, no allocation sized by attacker
//! input — and driven entirely by injected monotonic time and
//! caller-supplied CSPRNG values (the 24-bit transaction id and the
//! RFC 8415 §15 randomised-retransmission jitter). The engine never
//! generates randomness.
//!
//! The netstack integration (`plans/DHCP.md` D4b) frames the engine's
//! output as UDP(546→547)/IPv6/Ethernet to the all-servers multicast and
//! feeds received `DHCPv6` datagrams back in; it lives beside SLAAC as one
//! more address-configuration source, not a userland socket client.
//!
//! # Security
//!
//! Every server message is attacker-controlled. [`Dhcp6Reply::parse`] is
//! total (never panics), bounded (a fixed option-region walk, fixed-capacity
//! address / DNS / time-server lists — [`MAX_ADDRESSES`]), and fail-closed:
//! a malformed or
//! internally inconsistent message yields `None` and nothing is applied.
//! Off-path spoofing is bounded by the randomised transaction id; the state
//! machine additionally rejects any reply whose transaction id or echoed
//! Client Identifier does not match the outstanding request.

use tairix_abi::driver::net::{MacAddress, MAC_ADDRESS_LEN};

use crate::addr::Ipv6Addr;

/// UDP port a `DHCPv6` client listens on / sources from (RFC 8415 §7.2).
pub const CLIENT_PORT: u16 = 546;

/// UDP port a `DHCPv6` server / relay listens on / sources from
/// (RFC 8415 §7.2).
pub const SERVER_PORT: u16 = 547;

/// The `All_DHCP_Relay_Agents_and_Servers` link-scoped multicast address a
/// client sends to (RFC 8415 §7.1, `ff02::1:2`).
pub const ALL_SERVERS_MULTICAST: Ipv6Addr = Ipv6Addr::new(0xFF02, 0, 0, 0, 0, 0, 1, 2);

/// Length of the fixed `DHCPv6` message header (RFC 8415 §8: a one-octet
/// message type followed by a three-octet transaction id).
pub const MESSAGE_HEADER_LEN: usize = 4;

/// Largest number of addresses (IAADDRs) or DNS servers surfaced from one
/// reply. Extra entries past this fixed bound are ignored (a fixed security
/// bound, never an attacker-sized allocation).
pub const MAX_ADDRESSES: usize = 4;

/// The `0xFFFF_FFFF` lifetime meaning "infinite" (RFC 8415 §7.7): no
/// renewal is ever scheduled for such a binding.
pub const INFINITE_LIFETIME: u32 = u32::MAX;

/// `DHCPv6` option codes (RFC 8415 §21). Public so a test harness encoding
/// the server side of the exchange names the same option registry this
/// client codec decodes, never a divergent copy.
pub mod opt {
    /// Client Identifier (option 1): the client's DUID.
    pub const CLIENT_ID: u16 = 1;
    /// Server Identifier (option 2): the server's DUID.
    pub const SERVER_ID: u16 = 2;
    /// Identity Association for Non-temporary Addresses (option 3).
    pub const IA_NA: u16 = 3;
    /// IA Address, encapsulated within an `IA_NA` (option 5).
    pub const IA_ADDR: u16 = 5;
    /// Option Request Option (option 6): the options the client wants.
    pub const ORO: u16 = 6;
    /// Elapsed Time (option 8): hundredths of a second since the client
    /// began the current exchange.
    pub const ELAPSED_TIME: u16 = 8;
    /// Status Code (option 13): a success/failure code plus a message.
    pub const STATUS_CODE: u16 = 13;
    /// DNS Recursive Name Server list (RFC 3646 option 23).
    pub const DNS_SERVERS: u16 = 23;
    /// SNTP Server list (RFC 4075 option 31), superseded by
    /// [`NTP_SERVER`] but still deployed.
    pub const SNTP_SERVERS: u16 = 31;
    /// NTP Server (RFC 5908 option 56): a set of sub-options rather than a
    /// bare address list.
    pub const NTP_SERVER: u16 = 56;
}

/// Sub-option codes of [`opt::NTP_SERVER`] (RFC 5908 §4).
pub mod ntp_sub {
    /// Unicast NTP server address (sub-option 1): exactly one 16-octet
    /// IPv6 address.
    pub const SRV_ADDR: u16 = 1;
}

/// `DHCPv6` status codes (RFC 8415 §21.13). Only the ones the client acts on
/// are named; any other value is surfaced verbatim and treated as a
/// non-success (fail closed).
pub mod status {
    /// Success.
    pub const SUCCESS: u16 = 0;
    /// The server has no addresses available to assign.
    pub const NO_ADDRS_AVAIL: u16 = 2;
    /// The binding named in the client's message does not exist.
    pub const NO_BINDING: u16 = 3;
}

/// DUID hardware type for a 10 Mb Ethernet address (per the ARP hardware
/// type registry), as carried in a DUID-LL / DUID-LLT.
pub const DUID_HW_ETHERNET: u16 = 1;

/// DUID type 3 — link-layer address (RFC 8415 §11.4). The client forms its
/// own DUID this way: it is stable, derived from the interface MAC, and
/// needs no persisted timestamp (unlike DUID-LLT).
pub const DUID_TYPE_LL: u16 = 3;

/// Maximum length of a DUID (RFC 8415 §11: a DUID is 1–128 octets). A
/// fixed security bound: a Server Identifier longer than this is refused
/// rather than sizing an allocation.
pub const MAX_DUID_LEN: usize = 128;

/// A DHCP Unique Identifier (RFC 8415 §11): an opaque, bounded byte string
/// that names a client or server independently of any address. The client
/// forms its own as a DUID-LL from its MAC ([`Duid::ll_ethernet`]) and
/// stores the server's verbatim to echo it in later messages.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Duid {
    bytes: [u8; MAX_DUID_LEN],
    len: usize,
}

impl Duid {
    /// Form the client's own DUID-LL (type 3) from its Ethernet MAC
    /// address (RFC 8415 §11.4).
    #[must_use]
    pub fn ll_ethernet(mac: MacAddress) -> Self {
        let mut bytes = [0u8; MAX_DUID_LEN];
        bytes[0..2].copy_from_slice(&DUID_TYPE_LL.to_be_bytes());
        bytes[2..4].copy_from_slice(&DUID_HW_ETHERNET.to_be_bytes());
        bytes[4..4 + MAC_ADDRESS_LEN].copy_from_slice(&mac.0);
        Self {
            bytes,
            len: 4 + MAC_ADDRESS_LEN,
        }
    }

    /// Build a DUID from raw option bytes, or `None` (fail closed) if the
    /// length is outside the RFC 8415 §11 range (1–128 octets).
    #[must_use]
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.is_empty() || data.len() > MAX_DUID_LEN {
            return None;
        }
        let mut bytes = [0u8; MAX_DUID_LEN];
        bytes[..data.len()].copy_from_slice(data);
        Some(Self {
            bytes,
            len: data.len(),
        })
    }

    /// The DUID's octets in wire order.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

/// A `DHCPv6` message type (RFC 8415 §7.3).
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum MessageType {
    /// Client message locating available servers.
    Solicit = 1,
    /// Server offer of configuration parameters.
    Advertise = 2,
    /// Client request for the offered parameters.
    Request = 3,
    /// Client check that its addresses are still appropriate to the link.
    Confirm = 4,
    /// Client extension of the lifetimes on its leasing server.
    Renew = 5,
    /// Client extension of the lifetimes on any server.
    Rebind = 6,
    /// Server reply committing (or refusing) configuration.
    Reply = 7,
    /// Client relinquishment of its leased addresses.
    Release = 8,
    /// Client notice that a leased address is already in use.
    Decline = 9,
}

impl MessageType {
    /// The wire value.
    #[must_use]
    pub const fn code(self) -> u8 {
        self as u8
    }

    /// The message type a `code` denotes, or `None` for an unrecognised or
    /// unsupported value (fail closed).
    #[must_use]
    pub const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Solicit),
            2 => Some(Self::Advertise),
            3 => Some(Self::Request),
            4 => Some(Self::Confirm),
            5 => Some(Self::Renew),
            6 => Some(Self::Rebind),
            7 => Some(Self::Reply),
            8 => Some(Self::Release),
            9 => Some(Self::Decline),
            _ => None,
        }
    }
}

/// A bounded list of IPv6 addresses surfaced from a reply (leased
/// addresses or DNS servers). Holds at most [`MAX_ADDRESSES`]; entries past
/// that are dropped, so a hostile message can never size an allocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ipv6List {
    entries: [Ipv6Addr; MAX_ADDRESSES],
    len: usize,
}

impl Default for Ipv6List {
    fn default() -> Self {
        Self {
            entries: [Ipv6Addr::UNSPECIFIED; MAX_ADDRESSES],
            len: 0,
        }
    }
}

impl Ipv6List {
    /// The addresses collected, in wire order.
    #[must_use]
    pub fn as_slice(&self) -> &[Ipv6Addr] {
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

    /// The first address, if any.
    #[must_use]
    pub fn first(&self) -> Option<Ipv6Addr> {
        (self.len > 0).then_some(self.entries[0])
    }

    /// Append `addr`, dropping it if the fixed capacity is reached.
    fn push(&mut self, addr: Ipv6Addr) {
        if self.len < MAX_ADDRESSES {
            self.entries[self.len] = addr;
            self.len += 1;
        }
    }

    /// Append every 16-octet address in `data`, stopping at the fixed
    /// capacity or the last whole address (a trailing partial is ignored).
    fn extend_from_bytes(&mut self, data: &[u8]) {
        for chunk in data.as_chunks::<16>().0 {
            self.push(Ipv6Addr::from(*chunk));
        }
    }
}

/// One IA Address (RFC 8415 §21.6) surfaced from an `IA_NA`: the leased
/// address and its preferred / valid lifetimes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LeasedAddress {
    /// The leased IPv6 address.
    pub addr: Ipv6Addr,
    /// The preferred lifetime in seconds (RFC 8415 §7.7).
    pub preferred_lifetime: u32,
    /// The valid lifetime in seconds. An address whose valid lifetime is
    /// zero is a server instruction to stop using it.
    pub valid_lifetime: u32,
}

/// A bounded list of [`LeasedAddress`]es from one `IA_NA`. Holds at most
/// [`MAX_ADDRESSES`]; a hostile `IA_NA` can never size an allocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LeaseList {
    entries: [LeasedAddress; MAX_ADDRESSES],
    len: usize,
}

impl Default for LeaseList {
    fn default() -> Self {
        Self {
            entries: [LeasedAddress {
                addr: Ipv6Addr::UNSPECIFIED,
                preferred_lifetime: 0,
                valid_lifetime: 0,
            }; MAX_ADDRESSES],
            len: 0,
        }
    }
}

impl LeaseList {
    /// The leased addresses collected, in wire order.
    #[must_use]
    pub fn as_slice(&self) -> &[LeasedAddress] {
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

    /// The first usable address (a non-zero valid lifetime), if any — the
    /// one a single-address consumer applies.
    #[must_use]
    pub fn first_usable(&self) -> Option<LeasedAddress> {
        self.as_slice()
            .iter()
            .copied()
            .find(|a| a.valid_lifetime != 0)
    }

    fn push(&mut self, entry: LeasedAddress) {
        if self.len < MAX_ADDRESSES {
            self.entries[self.len] = entry;
            self.len += 1;
        }
    }
}

/// A parsed server→client `DHCPv6` message (an Advertise or a Reply).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Dhcp6Reply {
    /// The message type (Advertise or Reply).
    pub message_type: MessageType,
    /// The 24-bit transaction id, in the low three octets.
    pub transaction_id: u32,
    /// The server's DUID (Server Identifier, option 2). Absent is a
    /// protocol error the state machine rejects.
    pub server_id: Option<Duid>,
    /// The IAID carried in the `IA_NA`, if one was present.
    pub iaid: Option<u32>,
    /// The T1 (renew) time in seconds from the `IA_NA` (0 = "server left it
    /// to the client"; RFC 8415 §21.4).
    pub t1: u32,
    /// The T2 (rebind) time in seconds from the `IA_NA`.
    pub t2: u32,
    /// The addresses the `IA_NA` leases (RFC 8415 §21.6).
    pub addresses: LeaseList,
    /// The top-level Status Code (option 13); [`status::SUCCESS`] when
    /// absent (RFC 8415 §18.2.10.1).
    pub top_status: u16,
    /// The IA_NA-level Status Code; [`status::SUCCESS`] when absent.
    pub ia_status: u16,
    /// The DNS recursive name servers (RFC 3646 option 23), in wire order.
    pub dns_servers: Ipv6List,
    /// The network time servers, in wire order: the RFC 5908 option 56
    /// unicast server addresses when the server supplied any, else the
    /// RFC 4075 option 31 list it supersedes.
    pub ntp_servers: Ipv6List,
}

/// Read a big-endian `u16` at `data[off..off+2]`, or `None` if truncated.
fn be16(data: &[u8], off: usize) -> Option<u16> {
    let bytes = data.get(off..off + 2)?;
    Some(u16::from_be_bytes([bytes[0], bytes[1]]))
}

/// Read a big-endian `u32` at `data[off..off+4]`, or `None` if truncated.
fn be32(data: &[u8], off: usize) -> Option<u32> {
    let bytes = data.get(off..off + 4)?;
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// A total, bounded walk over a `DHCPv6` option region, invoking `visit` for
/// each well-formed `(code, data)` TLV and stopping at the first truncation
/// (fail closed: a partial trailing option is ignored, never guessed).
fn walk_options(region: &[u8], mut visit: impl FnMut(u16, &[u8])) {
    let mut i = 0usize;
    while i + 4 <= region.len() {
        let code = u16::from_be_bytes([region[i], region[i + 1]]);
        let len = usize::from(u16::from_be_bytes([region[i + 2], region[i + 3]]));
        i += 4;
        let Some(data) = region.get(i..i + len) else {
            break;
        };
        visit(code, data);
        i += len;
    }
}

/// Accumulates the recognised options of a reply as they are walked.
#[derive(Default)]
struct ReplyBuilder {
    server_id: Option<Duid>,
    client_id: Option<Duid>,
    iaid: Option<u32>,
    t1: u32,
    t2: u32,
    addresses: LeaseList,
    top_status: u16,
    ia_status: u16,
    dns_servers: Ipv6List,
    /// RFC 5908 option 56 unicast addresses.
    ntp_servers: Ipv6List,
    /// RFC 4075 option 31 addresses, used only when option 56 supplied
    /// none: RFC 5908 supersedes it, so a server offering both is taken at
    /// its newer word rather than at whichever it happened to send first.
    sntp_servers: Ipv6List,
    saw_ia_na: bool,
}

impl Dhcp6Reply {
    /// Parse a server→client `DHCPv6` message from `bytes` (the UDP payload)
    /// under the client's own transaction id `xid` (low 24 bits significant)
    /// and its DUID `client_duid`.
    ///
    /// Returns `None` (fail closed) for any of: a truncated header; a
    /// message type that is not a server response (Advertise / Reply); a
    /// transaction id that does not match; a missing Client Identifier or
    /// one that does not equal `client_duid` (RFC 8415 §16.10 — bounding
    /// off-path spoofing); or an option region that carries no recognisable
    /// message. Unrecognised options are ignored; nested `IA_NA` options are
    /// walked for the IA Address and IA-level Status Code.
    #[must_use]
    pub fn parse(bytes: &[u8], xid: u32, client_duid: &Duid) -> Option<Self> {
        let header = bytes.get(..MESSAGE_HEADER_LEN)?;
        let message_type = MessageType::from_code(header[0])?;
        if !matches!(message_type, MessageType::Advertise | MessageType::Reply) {
            return None;
        }
        let msg_xid = u32::from_be_bytes([0, header[1], header[2], header[3]]);
        if msg_xid != (xid & 0x00FF_FFFF) {
            return None;
        }

        let mut b = ReplyBuilder::default();
        walk_options(&bytes[MESSAGE_HEADER_LEN..], |code, data| {
            b.absorb(code, data);
        });

        // A server response must echo our Client Identifier (RFC 8415
        // §16.10); a missing or mismatched one is discarded.
        match b.client_id {
            Some(id) if id == *client_duid => {}
            _ => return None,
        }
        // Every server response carries the Server Identifier (RFC 8415
        // §16.10 / §21.3); its absence is a protocol error.
        b.server_id?;

        Some(Self {
            message_type,
            transaction_id: msg_xid,
            server_id: b.server_id,
            iaid: b.iaid,
            t1: b.t1,
            t2: b.t2,
            addresses: b.addresses,
            top_status: b.top_status,
            ia_status: b.ia_status,
            dns_servers: b.dns_servers,
            ntp_servers: if b.ntp_servers.is_empty() {
                b.sntp_servers
            } else {
                b.ntp_servers
            },
        })
    }
}

impl ReplyBuilder {
    /// Fold one top-level option into the builder.
    fn absorb(&mut self, code: u16, data: &[u8]) {
        match code {
            opt::SERVER_ID => self.server_id = self.server_id.or_else(|| Duid::from_bytes(data)),
            opt::CLIENT_ID => self.client_id = self.client_id.or_else(|| Duid::from_bytes(data)),
            opt::STATUS_CODE => {
                if let Some(sc) = be16(data, 0) {
                    self.top_status = sc;
                }
            }
            opt::DNS_SERVERS => self.dns_servers.extend_from_bytes(data),
            opt::SNTP_SERVERS => self.sntp_servers.extend_from_bytes(data),
            opt::NTP_SERVER => self.absorb_ntp_server(data),
            opt::IA_NA => self.absorb_ia_na(data),
            _ => {}
        }
    }

    /// Parse an NTP Server option (RFC 5908 §4): a set of sub-options, of
    /// which only the unicast server address is usable — this client speaks
    /// no multicast NTP, and an FQDN sub-option would need a resolver the
    /// address-configuration path does not have.
    fn absorb_ntp_server(&mut self, data: &[u8]) {
        walk_options(data, |code, sub| {
            if code == ntp_sub::SRV_ADDR {
                // RFC 5908 §4.1 fixes the sub-option at one address; a
                // different length is malformed and dropped whole.
                if let Ok(octets) = <[u8; 16]>::try_from(sub) {
                    self.ntp_servers.push(Ipv6Addr::from(octets));
                }
            }
        });
    }

    /// Parse an `IA_NA` option (RFC 8415 §21.4): its IAID + T1/T2 and the
    /// encapsulated IA Address / Status Code options.
    fn absorb_ia_na(&mut self, data: &[u8]) {
        // Only the first IA_NA is honoured (the client requests exactly one).
        if self.saw_ia_na {
            return;
        }
        let (Some(iaid), Some(t1), Some(t2)) = (be32(data, 0), be32(data, 4), be32(data, 8)) else {
            return;
        };
        self.saw_ia_na = true;
        self.iaid = Some(iaid);
        self.t1 = t1;
        self.t2 = t2;
        let Some(inner) = data.get(12..) else {
            return;
        };
        walk_options(inner, |code, opt_data| match code {
            opt::IA_ADDR => {
                if let Some(a) = parse_ia_addr(opt_data) {
                    self.addresses.push(a);
                }
            }
            opt::STATUS_CODE => {
                if let Some(sc) = be16(opt_data, 0) {
                    self.ia_status = sc;
                }
            }
            _ => {}
        });
    }
}

/// Parse an IA Address option body (RFC 8415 §21.6: 16-octet address, then
/// the preferred and valid lifetimes), or `None` if truncated.
fn parse_ia_addr(data: &[u8]) -> Option<LeasedAddress> {
    let addr_bytes = data.get(0..16)?;
    let mut octets = [0u8; 16];
    octets.copy_from_slice(addr_bytes);
    Some(LeasedAddress {
        addr: Ipv6Addr::from(octets),
        preferred_lifetime: be32(data, 16)?,
        valid_lifetime: be32(data, 20)?,
    })
}

// ---------------------------------------------------------------------------
// Client message encoder (RFC 8415 §8, §16, §21)
// ---------------------------------------------------------------------------

/// The options the client asks every server to populate (Option Request
/// Option, RFC 8415 §21.7): the DNS servers an interface wants, and both
/// spellings of the network time servers — RFC 5908's and the RFC 4075 one
/// it supersedes, since a deployed server may answer either.
const REQUESTED_OPTIONS: [u16; 3] = [opt::DNS_SERVERS, opt::NTP_SERVER, opt::SNTP_SERVERS];

/// A buffer this size always suffices for [`write_message`]: the header,
/// every option the client emits (a full-length Server Identifier included),
/// and the encapsulated `IA_NA`. A caller sizes its transmit buffer with this.
pub const MAX_MESSAGE_LEN: usize = MESSAGE_HEADER_LEN
    + (4 + MAX_DUID_LEN) // Client Identifier
    + (4 + MAX_DUID_LEN) // Server Identifier
    + (4 + 2) // Elapsed Time
    + (4 + 12 + 4 + 24) // IA_NA + one IA Address
    + (4 + 2 * REQUESTED_OPTIONS.len()); // Option Request

/// A fully-specified client→server `DHCPv6` message, ready for
/// [`write_message`]. The state machine produces one per transmission; the
/// encoder is the single definition of the wire form that Solicit / Request /
/// Renew / Rebind / Release / Decline all share.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MessageSpec {
    /// The message type.
    pub message_type: MessageType,
    /// The 24-bit transaction id (low three octets significant).
    pub transaction_id: u32,
    /// The client's own DUID (Client Identifier, option 1).
    pub client_duid: Duid,
    /// The server's DUID (Server Identifier, option 2): present on a
    /// message addressed to one specific server (Request/Renew/Release/
    /// Decline), absent on a multicast Solicit/Rebind.
    pub server_id: Option<Duid>,
    /// The Identity Association identifier the client uses for its `IA_NA`.
    pub iaid: u32,
    /// Elapsed time since the exchange began, in hundredths of a second
    /// (Elapsed Time, option 8), clamped into the 16-bit field.
    pub elapsed_centis: u16,
    /// An address to name inside the `IA_NA` (an IA Address option): the
    /// leased address in Renew/Rebind/Release/Decline, absent in Solicit.
    pub ia_addr: Option<Ipv6Addr>,
    /// Whether to emit an Option Request Option (Solicit/Request/Renew/
    /// Rebind ask for DNS servers; Release/Decline do not).
    pub request_options: bool,
}

/// Errors from [`write_message`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriteError {
    /// `out` is smaller than the encoded message ([`MAX_MESSAGE_LEN`]
    /// always suffices).
    BufferTooSmall,
}

/// A cursor writing `DHCPv6` options into a buffer, failing closed on
/// overflow.
struct OptionWriter<'a> {
    out: &'a mut [u8],
    pos: usize,
}

impl OptionWriter<'_> {
    /// Append raw bytes, or record overflow.
    fn raw(&mut self, data: &[u8]) -> Result<(), WriteError> {
        let end = self.pos + data.len();
        let slot = self
            .out
            .get_mut(self.pos..end)
            .ok_or(WriteError::BufferTooSmall)?;
        slot.copy_from_slice(data);
        self.pos = end;
        Ok(())
    }

    /// Append a `code`/`data` option (2-octet code, 2-octet length, body).
    fn option(&mut self, code: u16, data: &[u8]) -> Result<(), WriteError> {
        let len = u16::try_from(data.len()).map_err(|_| WriteError::BufferTooSmall)?;
        self.raw(&code.to_be_bytes())?;
        self.raw(&len.to_be_bytes())?;
        self.raw(data)
    }
}

/// Encode `spec` into `out`, returning the number of bytes written.
///
/// The output is a complete `DHCPv6` client message: the 4-octet header, the
/// Client Identifier, the optional Server Identifier, the Elapsed Time, the
/// `IA_NA` (with an encapsulated IA Address when `spec.ia_addr` is set), and
/// the optional Option Request Option.
///
/// # Errors
///
/// [`WriteError::BufferTooSmall`] if `out` is shorter than the encoded
/// message; [`MAX_MESSAGE_LEN`] always suffices.
pub fn write_message(spec: &MessageSpec, out: &mut [u8]) -> Result<usize, WriteError> {
    let mut w = OptionWriter { out, pos: 0 };
    // The 24-bit transaction id occupies the low three octets of the
    // big-endian word; the high octet is dropped alongside the type byte.
    let xid = (spec.transaction_id & 0x00FF_FFFF).to_be_bytes();
    w.raw(&[spec.message_type.code(), xid[1], xid[2], xid[3]])?;
    w.option(opt::CLIENT_ID, spec.client_duid.as_slice())?;
    if let Some(server) = spec.server_id {
        w.option(opt::SERVER_ID, server.as_slice())?;
    }
    w.option(opt::ELAPSED_TIME, &spec.elapsed_centis.to_be_bytes())?;

    // The IA_NA body: IAID, T1, T2 (the client leaves T1/T2 to the server,
    // RFC 8415 §18.2.4), then an optional encapsulated IA Address.
    let mut ia = [0u8; 12 + 4 + 24];
    ia[0..4].copy_from_slice(&spec.iaid.to_be_bytes());
    // T1 (ia[4..8]) and T2 (ia[8..12]) stay zero.
    let ia_len = if let Some(addr) = spec.ia_addr {
        ia[12..14].copy_from_slice(&opt::IA_ADDR.to_be_bytes());
        ia[14..16].copy_from_slice(&24u16.to_be_bytes());
        ia[16..32].copy_from_slice(&addr.octets());
        // The requested/relinquished preferred and valid lifetimes stay
        // zero (ia[32..40]): the client does not dictate them.
        12 + 4 + 24
    } else {
        12
    };
    w.option(opt::IA_NA, &ia[..ia_len])?;

    if spec.request_options {
        let mut oro = [0u8; 2 * REQUESTED_OPTIONS.len()];
        for (i, code) in REQUESTED_OPTIONS.iter().enumerate() {
            oro[i * 2..i * 2 + 2].copy_from_slice(&code.to_be_bytes());
        }
        w.option(opt::ORO, &oro)?;
    }
    Ok(w.pos)
}

// ---------------------------------------------------------------------------
// Client state machine (RFC 8415 §18.2)
// ---------------------------------------------------------------------------

use tairix_abi::time::Duration64;

use crate::timeutil::{from_nanos, nanos, NEVER, ONE_SEC_NANOS};

/// The RFC 8415 §15 retransmission parameters (in seconds / counts) for one
/// message type: initial timeout (IRT), max timeout (MRT, 0 = "no cap"),
/// and max retransmission count (MRC, 0 = "no count limit").
struct RetransmitParams {
    irt_secs: u64,
    mrt_secs: u64,
    mrc: u32,
}

/// Solicit (RFC 8415 §7.6): IRT 1s, MRT 3600s, no count limit.
const SOLICIT_PARAMS: RetransmitParams = RetransmitParams {
    irt_secs: 1,
    mrt_secs: 3600,
    mrc: 0,
};
/// Request (RFC 8415 §7.6): IRT 1s, MRT 30s, at most 10 tries.
const REQUEST_PARAMS: RetransmitParams = RetransmitParams {
    irt_secs: 1,
    mrt_secs: 30,
    mrc: 10,
};
/// Renew (RFC 8415 §7.6): IRT 10s, MRT 600s; bounded by T2, not a count.
const RENEW_PARAMS: RetransmitParams = RetransmitParams {
    irt_secs: 10,
    mrt_secs: 600,
    mrc: 0,
};
/// Rebind (RFC 8415 §7.6): IRT 10s, MRT 600s; bounded by valid lifetime.
const REBIND_PARAMS: RetransmitParams = RetransmitParams {
    irt_secs: 10,
    mrt_secs: 600,
    mrc: 0,
};
/// Release (RFC 8415 §7.6): IRT 1s, MRT 0, at most 5 tries.
const RELEASE_PARAMS: RetransmitParams = RetransmitParams {
    irt_secs: 1,
    mrt_secs: 0,
    mrc: 5,
};
/// Decline (RFC 8415 §7.6): IRT 1s, MRT 0, at most 5 tries.
const DECLINE_PARAMS: RetransmitParams = RetransmitParams {
    irt_secs: 1,
    mrt_secs: 0,
    mrc: 5,
};

/// The `DHCPv6` client's position in the RFC 8415 §18.2 lease lifecycle.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum State {
    /// No lease and no exchange in progress. The next [`Dhcp6Client::poll`]
    /// begins acquisition by multicasting a Solicit (unless the client was
    /// left idle after a Release).
    Init,
    /// A Solicit has been sent; awaiting Advertises.
    Soliciting,
    /// An Advertise was accepted and a Request sent; awaiting a Reply.
    Requesting,
    /// A lease is held and in use; renewal is scheduled for T1.
    Bound,
    /// Past T1: Renewing with the leasing server (Server Identifier set).
    Renewing,
    /// Past T2: Rebinding with any server (no Server Identifier).
    Rebinding,
    /// Relinquishing the lease with a Release; the client goes idle once
    /// the exchange finishes (RFC 8415 §18.2.7).
    Releasing,
    /// Declining an in-use address, after which the client re-solicits
    /// (RFC 8415 §18.2.8).
    Declining,
}

/// A committed `DHCPv6` lease, handed to the interface layer to apply.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Lease6 {
    /// The leased address.
    pub addr: Ipv6Addr,
    /// The preferred lifetime in seconds (RFC 8415 §7.7).
    pub preferred_lifetime: u32,
    /// The valid lifetime in seconds.
    pub valid_lifetime: u32,
    /// The T1 (renew) time in seconds actually used (server-supplied or the
    /// client's default).
    pub t1: u32,
    /// The T2 (rebind) time in seconds actually used.
    pub t2: u32,
    /// The DNS recursive name servers the lease carried, in wire order.
    pub dns_servers: Ipv6List,
    /// The network time servers the lease carried, in wire order.
    pub ntp_servers: Ipv6List,
    /// The DUID of the server that granted the lease.
    pub server_id: Option<Duid>,
}

/// A message the client must transmit. Every `DHCPv6` client message is sent
/// to [`ALL_SERVERS_MULTICAST`] on UDP [`SERVER_PORT`]; a message addressed
/// to one specific server carries that server's DUID in its Server
/// Identifier option rather than being unicast (the client implements no
/// Server Unicast option).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SendAction {
    /// The message to encode with [`write_message`].
    pub spec: MessageSpec,
}

/// An action the interface layer must carry out in response to a
/// [`Dhcp6Client::poll`] or [`Dhcp6Client::on_reply`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    /// Transmit a `DHCPv6` message to [`ALL_SERVERS_MULTICAST`]:547.
    Send(SendAction),
    /// A lease was acquired or renewed: apply this configuration.
    Configured(Lease6),
    /// The applied configuration must be withdrawn (expiry, `NoBinding`, or a
    /// changed address on renewal).
    Deconfigured,
}

/// The pure RFC 8415 stateful `DHCPv6` client state machine.
///
/// Construct with [`Dhcp6Client::new`], then drive it event-first:
///
/// - call [`Dhcp6Client::poll`] once at start-up and again whenever the
///   one-shot timer armed from [`Dhcp6Client::next_deadline`] fires, to
///   advance retransmissions and the T1/T2/expiry transitions;
/// - call [`Dhcp6Client::on_reply`] with each server message the interface
///   receives on UDP port 546.
///
/// Both return the [`Action`]s the interface layer must perform. The engine
/// owns no I/O and never blocks; the caller supplies monotonic `now` values
/// and, through `rng`, the CSPRNG randomness RFC 8415 requires for the
/// transaction id (§16.1) and the RFC 8415 §15 randomised retransmission
/// timeout.
#[derive(Clone, Debug)]
pub struct Dhcp6Client {
    client_duid: Duid,
    iaid: u32,
    state: State,
    /// The 24-bit transaction id (low three octets significant).
    xid: u32,
    /// Nanosecond instant the current exchange began (drives Elapsed Time).
    process_started: u128,
    /// Next retransmission instant, or [`NEVER`] when none is armed.
    retransmit: u128,
    /// Current retransmission timeout (RT) in nanoseconds.
    rt: u128,
    /// Retransmissions so far in the current exchange (for MRC bounds).
    retries: u32,
    /// The server chosen in Soliciting, echoed in Request/Renew/Release/
    /// Decline so only that server answers.
    server_id: Option<Duid>,
    /// The address an Advertise offered, carried into the Request.
    offered_addr: Option<Ipv6Addr>,
    /// The committed lease (valid in Bound/Renewing/Rebinding, and while
    /// Releasing/Declining the address being torn down).
    lease: Option<Lease6>,
    /// T1/T2/expiry instants (valid while a finite lease is held).
    t1: u128,
    t2: u128,
    expiry: u128,
}

impl Dhcp6Client {
    /// Construct a client for the interface whose Ethernet MAC is `mac`,
    /// using the interface identity association identifier `iaid`. The
    /// client's DUID-LL is derived from `mac`; the first
    /// [`Dhcp6Client::poll`] begins acquisition.
    #[must_use]
    pub fn new(mac: MacAddress, iaid: u32) -> Self {
        Self {
            client_duid: Duid::ll_ethernet(mac),
            iaid,
            state: State::Init,
            xid: 0,
            process_started: 0,
            retransmit: 0,
            rt: 0,
            retries: 0,
            server_id: None,
            offered_addr: None,
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

    /// The lease currently held, if any.
    #[must_use]
    pub fn lease(&self) -> Option<Lease6> {
        self.lease
    }

    /// The client's DUID, for [`Dhcp6Reply::parse`].
    #[must_use]
    pub fn client_duid(&self) -> Duid {
        self.client_duid
    }

    /// The transaction id of the outstanding exchange (low 24 bits), for
    /// [`Dhcp6Reply::parse`].
    #[must_use]
    pub fn transaction_id(&self) -> u32 {
        self.xid
    }

    /// The next instant [`Dhcp6Client::poll`] has timed work to do, or
    /// `None` when none is armed (a permanent lease in Bound, or an idle
    /// client after a completed Release). The caller arms one one-shot
    /// timer at this instant.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Duration64> {
        let deadline = match self.state {
            State::Init
            | State::Soliciting
            | State::Requesting
            | State::Releasing
            | State::Declining => self.retransmit,
            State::Bound => self.t1,
            State::Renewing => self.retransmit.min(self.t2),
            State::Rebinding => self.retransmit.min(self.expiry),
        };
        (deadline != NEVER).then(|| from_nanos(deadline))
    }
}

impl Dhcp6Client {
    /// The Elapsed Time (option 8) value for a message at `now_nanos`:
    /// hundredths of a second since the current exchange began, clamped
    /// into the 16-bit field (`0xFFFF` means "≥ 655.35 s").
    fn elapsed_centis(&self, now_nanos: u128) -> u16 {
        let centis = now_nanos.saturating_sub(self.process_started) / 10_000_000;
        u16::try_from(centis).unwrap_or(u16::MAX)
    }

    /// Scale `base` nanoseconds by a random factor: `1 + RAND` where RAND is
    /// uniform in `[-0.1, 0.1]` (RFC 8415 §15), or in `[0, 0.1]` when
    /// `signed` is false (the first Solicit, whose RT must not fall below
    /// IRT — RFC 8415 §18.2.1). The result is floored at `base`'s lower
    /// jitter bound and never underflows.
    fn apply_jitter(base: u128, rng: &mut dyn FnMut() -> u32, signed: bool) -> u128 {
        let (span, offset) = if signed {
            (2001u32, 1000i64)
        } else {
            (1001u32, 0i64)
        };
        let r = i64::from(rng() % span) - offset; // permille of `base`, in [-100,100] or [0,100]
                                                  // Every RT stays far below i128::MAX (MRT is at most 3600 s), so the
                                                  // widening never saturates in practice; it is written total anyway.
        let base = i128::try_from(base).unwrap_or(i128::MAX);
        let delta = i128::from(r) * base / 1000;
        let scaled = base.saturating_add(delta);
        u128::try_from(scaled.max(0)).unwrap_or(0)
    }

    /// The initial RT for a new exchange (RFC 8415 §15): `IRT + RAND*IRT`.
    fn first_rt(params: &RetransmitParams, rng: &mut dyn FnMut() -> u32, signed: bool) -> u128 {
        let irt = u128::from(params.irt_secs) * ONE_SEC_NANOS;
        Self::apply_jitter(irt, rng, signed)
    }

    /// The next RT after a retransmission (RFC 8415 §15):
    /// `2*RTprev + RAND*2*RTprev`, capped at `MRT + RAND*MRT` once `2*RTprev`
    /// exceeds MRT (`mrt_secs == 0` means no cap).
    fn grow_rt(&self, params: &RetransmitParams, rng: &mut dyn FnMut() -> u32) -> u128 {
        let doubled = self.rt.saturating_mul(2);
        let mrt = u128::from(params.mrt_secs) * ONE_SEC_NANOS;
        let base = if params.mrt_secs != 0 && doubled > mrt {
            mrt
        } else {
            doubled
        };
        Self::apply_jitter(base, rng, true)
    }

    /// Build a `Send` action for message type `mt` at `now_nanos`.
    fn send(
        &self,
        mt: MessageType,
        now_nanos: u128,
        server_id: Option<Duid>,
        ia_addr: Option<Ipv6Addr>,
        request_options: bool,
    ) -> Action {
        Action::Send(SendAction {
            spec: MessageSpec {
                message_type: mt,
                transaction_id: self.xid,
                client_duid: self.client_duid,
                server_id,
                iaid: self.iaid,
                elapsed_centis: self.elapsed_centis(now_nanos),
                ia_addr,
                request_options,
            },
        })
    }

    /// Begin (or restart) acquisition from INIT: draw a fresh 24-bit
    /// transaction id, reset the exchange clock, enter Soliciting, and emit
    /// a Solicit.
    fn begin_solicit(&mut self, now_nanos: u128, rng: &mut dyn FnMut() -> u32) -> Action {
        self.xid = rng() & 0x00FF_FFFF;
        self.process_started = now_nanos;
        self.server_id = None;
        self.offered_addr = None;
        self.retries = 0;
        self.rt = Self::first_rt(&SOLICIT_PARAMS, rng, false);
        self.retransmit = now_nanos.saturating_add(self.rt);
        self.state = State::Soliciting;
        self.send(MessageType::Solicit, now_nanos, None, None, true)
    }

    /// Advance retransmissions and timed transitions at `now`. Call at
    /// start-up and whenever the [`Dhcp6Client::next_deadline`] timer fires.
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
                    actions.push(self.begin_solicit(now_nanos, rng));
                }
            }
            State::Soliciting => {
                if now_nanos >= self.retransmit {
                    self.rt = self.grow_rt(&SOLICIT_PARAMS, rng);
                    self.retransmit = now_nanos.saturating_add(self.rt);
                    actions.push(self.send(MessageType::Solicit, now_nanos, None, None, true));
                }
            }
            State::Requesting => {
                if now_nanos >= self.retransmit {
                    self.retries += 1;
                    if self.retries >= REQUEST_PARAMS.mrc {
                        // Gave up waiting for a Reply: restart from INIT.
                        actions.push(self.begin_solicit(now_nanos, rng));
                    } else {
                        self.rt = self.grow_rt(&REQUEST_PARAMS, rng);
                        self.retransmit = now_nanos.saturating_add(self.rt);
                        actions.push(self.request_action(now_nanos));
                    }
                }
            }
            State::Bound => {
                if now_nanos >= self.t1 {
                    self.enter_renewing(now_nanos, rng);
                    actions.push(self.renew_action(now_nanos));
                }
            }
            State::Renewing => {
                if now_nanos >= self.t2 {
                    self.enter_rebinding(now_nanos, rng);
                    actions.push(self.rebind_action(now_nanos));
                } else if now_nanos >= self.retransmit {
                    self.rt = self.grow_rt(&RENEW_PARAMS, rng);
                    self.retransmit = now_nanos.saturating_add(self.rt);
                    actions.push(self.renew_action(now_nanos));
                }
            }
            State::Rebinding => {
                if now_nanos >= self.expiry {
                    // The lease expired without a renewal: drop it and
                    // re-acquire from scratch.
                    self.lease = None;
                    actions.push(Action::Deconfigured);
                    actions.push(self.begin_solicit(now_nanos, rng));
                } else if now_nanos >= self.retransmit {
                    self.rt = self.grow_rt(&REBIND_PARAMS, rng);
                    self.retransmit = now_nanos.saturating_add(self.rt);
                    actions.push(self.rebind_action(now_nanos));
                }
            }
            State::Releasing => {
                if now_nanos >= self.retransmit {
                    self.retries += 1;
                    if self.retries >= RELEASE_PARAMS.mrc {
                        // Best-effort teardown done: go idle without
                        // re-acquiring (the address is relinquished).
                        self.lease = None;
                        self.go_idle();
                    } else {
                        self.rt = self.grow_rt(&RELEASE_PARAMS, rng);
                        self.retransmit = now_nanos.saturating_add(self.rt);
                        if let Some(action) = self.release_action(now_nanos) {
                            actions.push(action);
                        }
                    }
                }
            }
            State::Declining => {
                if now_nanos >= self.retransmit {
                    self.retries += 1;
                    if self.retries >= DECLINE_PARAMS.mrc {
                        // Declined enough: re-acquire a fresh address.
                        self.lease = None;
                        actions.push(self.begin_solicit(now_nanos, rng));
                    } else {
                        self.rt = self.grow_rt(&DECLINE_PARAMS, rng);
                        self.retransmit = now_nanos.saturating_add(self.rt);
                        if let Some(action) = self.decline_action(now_nanos) {
                            actions.push(action);
                        }
                    }
                }
            }
        }
        actions
    }

    /// A Request for the offered address to the chosen server.
    fn request_action(&self, now_nanos: u128) -> Action {
        self.send(
            MessageType::Request,
            now_nanos,
            self.server_id,
            self.offered_addr,
            true,
        )
    }

    /// A Renew of the held lease to its leasing server.
    fn renew_action(&self, now_nanos: u128) -> Action {
        let addr = self.lease.map(|l| l.addr);
        self.send(MessageType::Renew, now_nanos, self.server_id, addr, true)
    }

    /// A Rebind of the held lease to any server (no Server Identifier).
    fn rebind_action(&self, now_nanos: u128) -> Action {
        let addr = self.lease.map(|l| l.addr);
        self.send(MessageType::Rebind, now_nanos, None, addr, true)
    }

    /// A Release of the held lease, or `None` if there is nothing to release.
    fn release_action(&self, now_nanos: u128) -> Option<Action> {
        let addr = self.lease?.addr;
        Some(self.send(
            MessageType::Release,
            now_nanos,
            self.server_id,
            Some(addr),
            false,
        ))
    }

    /// A Decline of the held lease, or `None` if there is nothing to decline.
    fn decline_action(&self, now_nanos: u128) -> Option<Action> {
        let addr = self.lease?.addr;
        Some(self.send(
            MessageType::Decline,
            now_nanos,
            self.server_id,
            Some(addr),
            false,
        ))
    }

    /// Enter the RENEWING state at `now_nanos`, arming the first Renew RT.
    fn enter_renewing(&mut self, now_nanos: u128, rng: &mut dyn FnMut() -> u32) {
        self.state = State::Renewing;
        self.xid = rng() & 0x00FF_FFFF;
        self.process_started = now_nanos;
        self.retries = 0;
        self.rt = Self::first_rt(&RENEW_PARAMS, rng, true);
        self.retransmit = now_nanos.saturating_add(self.rt);
    }

    /// Enter the REBINDING state at `now_nanos`, arming the first Rebind RT.
    fn enter_rebinding(&mut self, now_nanos: u128, rng: &mut dyn FnMut() -> u32) {
        self.state = State::Rebinding;
        self.xid = rng() & 0x00FF_FFFF;
        self.process_started = now_nanos;
        self.retries = 0;
        self.rt = Self::first_rt(&REBIND_PARAMS, rng, true);
        self.retransmit = now_nanos.saturating_add(self.rt);
    }

    /// Park the client idle: no lease, no armed timer, no re-acquisition
    /// until the caller drops the client.
    fn go_idle(&mut self) {
        self.state = State::Init;
        self.retransmit = NEVER;
    }

    /// Begin relinquishing the current lease (RFC 8415 §18.2.7). Returns the
    /// Release to transmit, or `None` (going idle) if no lease is held.
    pub fn release(&mut self, now: Duration64, rng: &mut dyn FnMut() -> u32) -> Option<Action> {
        let now_nanos = nanos(now);
        if self.lease.is_none() {
            self.go_idle();
            return None;
        }
        self.state = State::Releasing;
        self.xid = rng() & 0x00FF_FFFF;
        self.process_started = now_nanos;
        self.retries = 0;
        self.rt = Self::first_rt(&RELEASE_PARAMS, rng, true);
        self.retransmit = now_nanos.saturating_add(self.rt);
        self.release_action(now_nanos)
    }

    /// Begin declining the current lease's address (RFC 8415 §18.2.8), used
    /// when duplicate-address detection finds the leased address in use.
    /// Returns the Decline to transmit, or `None` if no lease is held.
    pub fn decline(&mut self, now: Duration64, rng: &mut dyn FnMut() -> u32) -> Option<Action> {
        let now_nanos = nanos(now);
        self.lease?;
        self.state = State::Declining;
        self.xid = rng() & 0x00FF_FFFF;
        self.process_started = now_nanos;
        self.retries = 0;
        self.rt = Self::first_rt(&DECLINE_PARAMS, rng, true);
        self.retransmit = now_nanos.saturating_add(self.rt);
        self.decline_action(now_nanos)
    }
}

impl Dhcp6Client {
    /// Fold a received server message into the state machine at `now`.
    ///
    /// `reply` must already have been parsed against this client's current
    /// [`Dhcp6Client::transaction_id`] and [`Dhcp6Client::client_duid`] (see
    /// [`Dhcp6Reply::parse`]), so an off-path spoof for a different exchange
    /// never reaches here. `rng` supplies the fresh transaction id RFC 8415
    /// §16.1 requires when the reply advances the client to a new message
    /// exchange. Returns the resulting [`Action`]s.
    pub fn on_reply(
        &mut self,
        now: Duration64,
        reply: &Dhcp6Reply,
        rng: &mut dyn FnMut() -> u32,
    ) -> alloc::vec::Vec<Action> {
        let now_nanos = nanos(now);
        let mut actions = alloc::vec::Vec::new();
        // A stale reply for a previous exchange is ignored (the parser
        // matches the transaction id, but a caller could feed one directly).
        if reply.transaction_id != (self.xid & 0x00FF_FFFF) {
            return actions;
        }
        match (self.state, reply.message_type) {
            (State::Soliciting, MessageType::Advertise) => {
                self.accept_advertise(now_nanos, reply, rng, &mut actions);
            }
            (State::Requesting, MessageType::Reply) => {
                if !self.server_matches(reply) {
                    return actions;
                }
                if Self::reply_is_success(reply) {
                    self.commit(now_nanos, reply, &mut actions);
                } else {
                    // The chosen server could not honour the Request:
                    // restart acquisition from a fresh Solicit.
                    actions.push(self.begin_solicit(now_nanos, rng));
                }
            }
            (State::Renewing, MessageType::Reply) => {
                if self.server_matches(reply) {
                    self.on_renew_reply(now_nanos, reply, rng, &mut actions);
                }
            }
            (State::Rebinding, MessageType::Reply) => {
                // A Rebind accepts any server; adopt whichever answered.
                self.on_renew_reply(now_nanos, reply, rng, &mut actions);
            }
            _ => {}
        }
        actions
    }

    /// Whether an IA-bearing Reply reports success at both the top level and
    /// inside the `IA_NA` (RFC 8415 §18.2.10).
    fn reply_is_success(reply: &Dhcp6Reply) -> bool {
        reply.top_status == status::SUCCESS && reply.ia_status == status::SUCCESS
    }

    /// Whether `reply` came from the server this client is talking to. A
    /// Rebind sets no server, so it matches any; a Request/Renew requires
    /// the Server Identifier to equal the one the client selected.
    fn server_matches(&self, reply: &Dhcp6Reply) -> bool {
        match self.server_id {
            Some(chosen) => reply.server_id == Some(chosen),
            None => true,
        }
    }

    /// Handle an Advertise in SOLICITING: adopt the server + offered
    /// address and send the Request, unless the Advertise is unusable.
    fn accept_advertise(
        &mut self,
        now_nanos: u128,
        reply: &Dhcp6Reply,
        rng: &mut dyn FnMut() -> u32,
        actions: &mut alloc::vec::Vec<Action>,
    ) {
        let Some(server_id) = reply.server_id else {
            return;
        };
        // A server advertising no available address (RFC 8415 §18.2.9) is
        // not selected; the client keeps soliciting.
        if !Self::reply_is_success(reply) {
            return;
        }
        let Some(offered) = reply.addresses.first_usable() else {
            return;
        };
        self.server_id = Some(server_id);
        self.offered_addr = Some(offered.addr);
        self.state = State::Requesting;
        self.xid = rng() & 0x00FF_FFFF;
        self.process_started = now_nanos;
        self.retries = 0;
        self.rt = Self::first_rt(&REQUEST_PARAMS, rng, true);
        self.retransmit = now_nanos.saturating_add(self.rt);
        actions.push(self.request_action(now_nanos));
    }

    /// Handle a Reply while RENEWING or REBINDING.
    fn on_renew_reply(
        &mut self,
        now_nanos: u128,
        reply: &Dhcp6Reply,
        rng: &mut dyn FnMut() -> u32,
        actions: &mut alloc::vec::Vec<Action>,
    ) {
        if reply.ia_status == status::NO_BINDING {
            // The server has no record of our binding (RFC 8415 §18.2.10.1):
            // drop the lease and re-acquire from scratch.
            self.lease = None;
            actions.push(Action::Deconfigured);
            actions.push(self.begin_solicit(now_nanos, rng));
        } else if Self::reply_is_success(reply) {
            self.commit(now_nanos, reply, actions);
        }
        // A transient error keeps the client in place; it retransmits.
    }

    /// Commit the lease a successful Reply carries: compute T1/T2/expiry,
    /// enter BOUND, and push the configuration actions (a `Deconfigured`
    /// first when the address changed on renewal, then `Configured`). A
    /// success Reply with no usable address is treated as a failed renewal.
    fn commit(
        &mut self,
        now_nanos: u128,
        reply: &Dhcp6Reply,
        actions: &mut alloc::vec::Vec<Action>,
    ) {
        let Some(offered) = reply.addresses.first_usable() else {
            return;
        };
        let (t1s, t2s) = renewal_times(offered.preferred_lifetime, reply.t1, reply.t2);
        let previous_addr = self.lease.map(|l| l.addr);
        let lease = Lease6 {
            addr: offered.addr,
            preferred_lifetime: offered.preferred_lifetime,
            valid_lifetime: offered.valid_lifetime,
            t1: t1s,
            t2: t2s,
            dns_servers: reply.dns_servers,
            ntp_servers: reply.ntp_servers,
            server_id: reply.server_id.or(self.server_id),
        };
        self.state = State::Bound;
        self.retransmit = NEVER;
        self.retries = 0;
        self.server_id = lease.server_id;
        self.offered_addr = None;
        if offered.preferred_lifetime == INFINITE_LIFETIME {
            self.t1 = NEVER;
            self.t2 = NEVER;
        } else {
            self.t1 = now_nanos.saturating_add(u128::from(t1s) * ONE_SEC_NANOS);
            self.t2 = now_nanos.saturating_add(u128::from(t2s) * ONE_SEC_NANOS);
        }
        self.expiry = if offered.valid_lifetime == INFINITE_LIFETIME {
            NEVER
        } else {
            now_nanos.saturating_add(u128::from(offered.valid_lifetime) * ONE_SEC_NANOS)
        };
        self.lease = Some(lease);
        // A renewal onto a different address withdraws the old one first.
        if matches!(previous_addr, Some(prev) if prev != lease.addr) {
            actions.push(Action::Deconfigured);
        }
        actions.push(Action::Configured(lease));
    }
}

/// Derive the T1 (renew) and T2 (rebind) seconds from a server's `IA_NA`
/// values and the offered preferred lifetime (RFC 8415 §14, §18.2.4). A
/// zero from the server means "the client chooses"; the client uses
/// T1 = ½·preferred and T2 = ⅘·preferred, then clamps `T1 ≤ T2`.
fn renewal_times(preferred: u32, server_t1: u32, server_t2: u32) -> (u32, u32) {
    if preferred == INFINITE_LIFETIME {
        return (INFINITE_LIFETIME, INFINITE_LIFETIME);
    }
    let default_t1 = preferred / 2;
    let default_t2 = u32::try_from(u64::from(preferred) * 4 / 5).unwrap_or(u32::MAX);
    let t1 = if server_t1 == 0 {
        default_t1
    } else {
        server_t1
    };
    let t2 = if server_t2 == 0 {
        default_t2
    } else {
        server_t2
    };
    // A server that supplies an inconsistent T1 > T2 (or defaults that
    // invert) is clamped so renew never lands after rebind.
    (t1.min(t2), t2)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: MacAddress = MacAddress([0x52, 0x54, 0, 1, 2, 3]);
    const IAID: u32 = 0x0A0B_0C0D;

    pub(super) fn server_duid() -> Duid {
        Duid::from_bytes(&[0, 3, 0, 1, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]).unwrap()
    }

    pub(super) fn addr() -> Ipv6Addr {
        Ipv6Addr::new(0x2001, 0xDB8, 0, 0, 0, 0, 0, 0x1234)
    }

    /// A fixed CSPRNG stub returning a deterministic sequence.
    pub(super) fn seq_rng(values: &[u32]) -> impl FnMut() -> u32 + '_ {
        let mut i = 0usize;
        move || {
            let v = values.get(i).copied().unwrap_or(0x1357_9BDF);
            i += 1;
            v
        }
    }

    /// Build a server Reply/Advertise with one `IA_NA` + IA Address.
    pub(super) fn build_message(
        mt: MessageType,
        xid: u32,
        addr: Ipv6Addr,
        preferred: u32,
        valid: u32,
        t1: u32,
        t2: u32,
    ) -> alloc::vec::Vec<u8> {
        let mut out = alloc::vec::Vec::new();
        out.push(mt.code());
        out.extend_from_slice(&xid.to_be_bytes()[1..4]);
        push_option(&mut out, opt::CLIENT_ID, Duid::ll_ethernet(MAC).as_slice());
        push_option(&mut out, opt::SERVER_ID, server_duid().as_slice());
        // IA_NA body.
        let mut ia = alloc::vec::Vec::new();
        ia.extend_from_slice(&IAID.to_be_bytes());
        ia.extend_from_slice(&t1.to_be_bytes());
        ia.extend_from_slice(&t2.to_be_bytes());
        let mut iaddr = alloc::vec::Vec::new();
        iaddr.extend_from_slice(&addr.octets());
        iaddr.extend_from_slice(&preferred.to_be_bytes());
        iaddr.extend_from_slice(&valid.to_be_bytes());
        push_option(&mut ia, opt::IA_ADDR, &iaddr);
        push_option(&mut out, opt::IA_NA, &ia);
        out
    }

    pub(super) fn push_option(out: &mut alloc::vec::Vec<u8>, code: u16, data: &[u8]) {
        out.extend_from_slice(&code.to_be_bytes());
        out.extend_from_slice(&u16::try_from(data.len()).unwrap().to_be_bytes());
        out.extend_from_slice(data);
    }

    #[test]
    fn duid_ll_round_trips() {
        let d = Duid::ll_ethernet(MAC);
        assert_eq!(d.as_slice(), &[0, 3, 0, 1, 0x52, 0x54, 0, 1, 2, 3]);
        assert_eq!(Duid::from_bytes(d.as_slice()), Some(d));
    }

    #[test]
    fn duid_rejects_empty_and_oversize() {
        assert_eq!(Duid::from_bytes(&[]), None);
        assert_eq!(Duid::from_bytes(&[0u8; MAX_DUID_LEN + 1]), None);
        assert!(Duid::from_bytes(&[0u8; MAX_DUID_LEN]).is_some());
    }

    #[test]
    fn write_message_never_exceeds_max_len() {
        let spec = MessageSpec {
            message_type: MessageType::Request,
            transaction_id: 0x00AB_CDEF,
            client_duid: Duid::ll_ethernet(MAC),
            server_id: Some(Duid::from_bytes(&[0u8; MAX_DUID_LEN]).unwrap()),
            iaid: IAID,
            elapsed_centis: 42,
            ia_addr: Some(addr()),
            request_options: true,
        };
        let mut buf = [0u8; MAX_MESSAGE_LEN];
        let n = write_message(&spec, &mut buf).unwrap();
        assert!(n <= MAX_MESSAGE_LEN);
        // The 24-bit transaction id lands in the low three octets.
        assert_eq!(&buf[0..4], &[MessageType::Request.code(), 0xAB, 0xCD, 0xEF]);
    }

    #[test]
    fn parse_round_trips_reply() {
        let client = Duid::ll_ethernet(MAC);
        let server = server_duid();
        let bytes = build_message(
            MessageType::Reply,
            0x00AB_CDEF,
            addr(),
            3600,
            7200,
            1800,
            2880,
        );
        let reply = Dhcp6Reply::parse(&bytes, 0x00AB_CDEF, &client).expect("well-formed reply");
        assert_eq!(reply.message_type, MessageType::Reply);
        assert_eq!(reply.server_id, Some(server));
        assert_eq!(reply.iaid, Some(IAID));
        assert_eq!(reply.t1, 1800);
        assert_eq!(reply.t2, 2880);
        let lease = reply.addresses.first_usable().unwrap();
        assert_eq!(lease.addr, addr());
        assert_eq!(lease.valid_lifetime, 7200);
    }

    #[test]
    fn the_option_request_asks_for_both_time_server_spellings() {
        let spec = MessageSpec {
            message_type: MessageType::Solicit,
            transaction_id: 1,
            client_duid: Duid::ll_ethernet(MAC),
            server_id: None,
            iaid: IAID,
            elapsed_centis: 0,
            ia_addr: None,
            request_options: true,
        };
        let mut buf = [0u8; MAX_MESSAGE_LEN];
        let n = write_message(&spec, &mut buf).expect("write");
        let mut asked = alloc::vec::Vec::new();
        walk_options(&buf[MESSAGE_HEADER_LEN..n], |code, data| {
            if code == opt::ORO {
                for pair in data.as_chunks::<2>().0 {
                    asked.push(u16::from_be_bytes(*pair));
                }
            }
        });
        assert!(
            asked.contains(&opt::NTP_SERVER) && asked.contains(&opt::SNTP_SERVERS),
            "a server only supplies an option it was asked for: {asked:?}"
        );
    }

    #[test]
    fn the_rfc_5908_time_servers_supersede_the_rfc_4075_list() {
        let client = Duid::ll_ethernet(MAC);
        let sntp = Ipv6Addr::new(0x2001, 0xDB8, 0, 0, 0, 0, 0, 0x1F);
        let ntp = Ipv6Addr::new(0x2001, 0xDB8, 0, 0, 0, 0, 0, 0x7B);
        let mut bytes = build_message(MessageType::Reply, 7, addr(), 3600, 7200, 0, 0);
        push_option(&mut bytes, opt::SNTP_SERVERS, &sntp.octets());
        let mut sub = alloc::vec::Vec::new();
        push_option(&mut sub, ntp_sub::SRV_ADDR, &ntp.octets());
        push_option(&mut bytes, opt::NTP_SERVER, &sub);
        let reply = Dhcp6Reply::parse(&bytes, 7, &client).expect("parse");
        assert_eq!(
            reply.ntp_servers.as_slice(),
            &[ntp],
            "option 56 wins outright; option 31 is not merged in"
        );
    }

    #[test]
    fn the_rfc_4075_time_servers_are_used_when_no_rfc_5908_option_arrives() {
        let client = Duid::ll_ethernet(MAC);
        let sntp = Ipv6Addr::new(0x2001, 0xDB8, 0, 0, 0, 0, 0, 0x1F);
        let mut bytes = build_message(MessageType::Reply, 7, addr(), 3600, 7200, 0, 0);
        push_option(&mut bytes, opt::SNTP_SERVERS, &sntp.octets());
        let reply = Dhcp6Reply::parse(&bytes, 7, &client).expect("parse");
        assert_eq!(reply.ntp_servers.as_slice(), &[sntp]);
    }

    #[test]
    fn a_time_server_sub_option_of_the_wrong_length_is_dropped() {
        let client = Duid::ll_ethernet(MAC);
        let good = Ipv6Addr::new(0x2001, 0xDB8, 0, 0, 0, 0, 0, 0x7B);
        let mut bytes = build_message(MessageType::Reply, 7, addr(), 3600, 7200, 0, 0);
        let mut sub = alloc::vec::Vec::new();
        // Two addresses in one sub-option, which RFC 5908 §4.1 fixes at one,
        // then a well-formed one: the malformed sub-option is dropped whole
        // rather than read as an address list.
        let mut pair = alloc::vec::Vec::new();
        pair.extend_from_slice(&good.octets());
        pair.extend_from_slice(&good.octets());
        push_option(&mut sub, ntp_sub::SRV_ADDR, &pair);
        push_option(&mut sub, ntp_sub::SRV_ADDR, &good.octets());
        // A multicast sub-option this client does not speak, ignored.
        push_option(
            &mut sub,
            2,
            &Ipv6Addr::new(0xFF02, 0, 0, 0, 0, 0, 0, 0x101).octets(),
        );
        push_option(&mut bytes, opt::NTP_SERVER, &sub);
        let reply = Dhcp6Reply::parse(&bytes, 7, &client).expect("parse");
        assert_eq!(reply.ntp_servers.as_slice(), &[good]);
    }

    #[test]
    fn parse_rejects_wrong_xid() {
        let client = Duid::ll_ethernet(MAC);
        let bytes = build_message(MessageType::Reply, 0x00AB_CDEF, addr(), 1, 1, 0, 0);
        assert!(Dhcp6Reply::parse(&bytes, 0x0012_3456, &client).is_none());
    }

    #[test]
    fn parse_rejects_mismatched_client_id() {
        let other = Duid::ll_ethernet(MacAddress([9, 9, 9, 9, 9, 9]));
        let bytes = build_message(MessageType::Reply, 1, addr(), 1, 1, 0, 0);
        assert!(Dhcp6Reply::parse(&bytes, 1, &other).is_none());
    }

    #[test]
    fn parse_rejects_missing_server_id() {
        let client = Duid::ll_ethernet(MAC);
        let mut out = alloc::vec::Vec::new();
        out.push(MessageType::Reply.code());
        out.extend_from_slice(&[0, 0, 1]);
        push_option(&mut out, opt::CLIENT_ID, client.as_slice());
        assert!(Dhcp6Reply::parse(&out, 1, &client).is_none());
    }

    #[test]
    fn parse_rejects_client_message_type() {
        let client = Duid::ll_ethernet(MAC);
        let bytes = build_message(MessageType::Solicit, 1, addr(), 1, 1, 0, 0);
        assert!(Dhcp6Reply::parse(&bytes, 1, &client).is_none());
    }
}

#[cfg(test)]
mod machine_tests {
    use super::tests::*;
    use super::*;

    const MAC: MacAddress = MacAddress([0x52, 0x54, 0, 1, 2, 3]);
    const IAID: u32 = 0x0A0B_0C0D;

    fn at(secs: i64) -> Duration64 {
        Duration64::from_secs(secs)
    }

    fn only_send(actions: &[Action]) -> MessageSpec {
        match actions {
            [Action::Send(SendAction { spec })] => *spec,
            other => panic!("expected exactly one Send, got {other:?}"),
        }
    }

    #[test]
    fn solicit_then_request_then_bound() {
        let client_duid = Duid::ll_ethernet(MAC);
        let server = server_duid();
        let mut rng = seq_rng(&[0x0011_2233, 0x00AA_BBCC]);
        let mut client = Dhcp6Client::new(MAC, IAID);

        let spec = only_send(&client.poll(at(0), &mut rng));
        assert_eq!(spec.message_type, MessageType::Solicit);
        assert_eq!(client.state(), State::Soliciting);
        let solicit_xid = client.transaction_id();
        assert_eq!(spec.transaction_id & 0x00FF_FFFF, solicit_xid);

        // The server advertises an address.
        let adv = build_message(
            MessageType::Advertise,
            solicit_xid,
            addr(),
            3600,
            7200,
            0,
            0,
        );
        let reply = Dhcp6Reply::parse(&adv, solicit_xid, &client_duid).unwrap();
        let spec = only_send(&client.on_reply(at(1), &reply, &mut rng));
        assert_eq!(spec.message_type, MessageType::Request);
        assert_eq!(client.state(), State::Requesting);
        assert_eq!(spec.server_id, Some(server));
        assert_eq!(spec.ia_addr, Some(addr()));

        // The server replies, committing the lease.
        let req_xid = client.transaction_id();
        let rep = build_message(MessageType::Reply, req_xid, addr(), 3600, 7200, 1800, 2880);
        let reply = Dhcp6Reply::parse(&rep, req_xid, &client_duid).unwrap();
        let actions = client.on_reply(at(2), &reply, &mut rng);
        assert!(matches!(actions.as_slice(), [Action::Configured(_)]));
        assert_eq!(client.state(), State::Bound);
        let lease = client.lease().unwrap();
        assert_eq!(lease.addr, addr());
        assert_eq!(lease.t1, 1800);
        assert_eq!(lease.t2, 2880);
        // The next deadline is T1 (renew), 1800 s after the Reply.
        let d = client.next_deadline().unwrap();
        assert_eq!(d.secs(), 2 + 1800);
    }

    #[test]
    fn advertise_with_no_addrs_avail_is_ignored() {
        let client_duid = Duid::ll_ethernet(MAC);
        let server = server_duid();
        let mut rng = seq_rng(&[1, 2, 3]);
        let mut client = Dhcp6Client::new(MAC, IAID);
        let _ = client.poll(at(0), &mut rng);
        let xid = client.transaction_id();

        // Advertise with a top-level NoAddrsAvail status.
        let mut adv = alloc::vec::Vec::new();
        adv.push(MessageType::Advertise.code());
        adv.extend_from_slice(&xid.to_be_bytes()[1..4]);
        push_opt(&mut adv, opt::CLIENT_ID, client_duid.as_slice());
        push_opt(&mut adv, opt::SERVER_ID, server.as_slice());
        let mut sc = alloc::vec::Vec::new();
        sc.extend_from_slice(&status::NO_ADDRS_AVAIL.to_be_bytes());
        push_opt(&mut adv, opt::STATUS_CODE, &sc);

        let reply = Dhcp6Reply::parse(&adv, xid, &client_duid).unwrap();
        let actions = client.on_reply(at(1), &reply, &mut rng);
        assert!(actions.is_empty());
        assert_eq!(client.state(), State::Soliciting);
    }

    fn push_opt(out: &mut alloc::vec::Vec<u8>, code: u16, data: &[u8]) {
        out.extend_from_slice(&code.to_be_bytes());
        out.extend_from_slice(&u16::try_from(data.len()).unwrap().to_be_bytes());
        out.extend_from_slice(data);
    }

    fn drive_to_bound(client: &mut Dhcp6Client, rng: &mut dyn FnMut() -> u32) {
        let client_duid = Duid::ll_ethernet(MAC);
        let _ = client.poll(at(0), rng);
        let sx = client.transaction_id();
        let adv = build_message(MessageType::Advertise, sx, addr(), 3600, 7200, 1800, 2880);
        let r = Dhcp6Reply::parse(&adv, sx, &client_duid).unwrap();
        let _ = client.on_reply(at(1), &r, rng);
        let rx = client.transaction_id();
        let rep = build_message(MessageType::Reply, rx, addr(), 3600, 7200, 1800, 2880);
        let r = Dhcp6Reply::parse(&rep, rx, &client_duid).unwrap();
        let _ = client.on_reply(at(2), &r, rng);
        assert_eq!(client.state(), State::Bound);
    }

    #[test]
    fn t1_drives_renew() {
        let mut rng = seq_rng(&[1, 2, 3, 4, 5]);
        let mut client = Dhcp6Client::new(MAC, IAID);
        drive_to_bound(&mut client, &mut rng);
        // At T1 the client renews with the leasing server named.
        let spec = only_send(&client.poll(at(2 + 1800), &mut rng));
        assert_eq!(spec.message_type, MessageType::Renew);
        assert_eq!(spec.server_id, Some(server_duid()));
        assert_eq!(client.state(), State::Renewing);
    }

    #[test]
    fn t2_drives_rebind() {
        let mut rng = seq_rng(&[1, 2, 3, 4, 5, 6]);
        let mut client = Dhcp6Client::new(MAC, IAID);
        drive_to_bound(&mut client, &mut rng);
        let _ = client.poll(at(2 + 1800), &mut rng); // -> Renewing
                                                     // At T2 the client rebinds to any server (no Server Identifier).
        let spec = only_send(&client.poll(at(2 + 2880), &mut rng));
        assert_eq!(spec.message_type, MessageType::Rebind);
        assert_eq!(spec.server_id, None);
        assert_eq!(client.state(), State::Rebinding);
    }

    #[test]
    fn expiry_withdraws_and_resolicits() {
        let mut rng = seq_rng(&[1, 2, 3, 4, 5, 6, 7]);
        let mut client = Dhcp6Client::new(MAC, IAID);
        drive_to_bound(&mut client, &mut rng);
        let _ = client.poll(at(2 + 1800), &mut rng); // Renewing
        let _ = client.poll(at(2 + 2880), &mut rng); // Rebinding
                                                     // The valid lifetime (7200 s from the Reply at t=2) expires.
        let actions = client.poll(at(2 + 7200), &mut rng);
        assert!(matches!(actions.first(), Some(Action::Deconfigured)));
        assert!(matches!(actions.get(1), Some(Action::Send(_))));
        assert_eq!(client.state(), State::Soliciting);
        assert!(client.lease().is_none());
    }

    #[test]
    fn release_relinquishes_then_goes_idle() {
        let mut rng = seq_rng(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let mut client = Dhcp6Client::new(MAC, IAID);
        drive_to_bound(&mut client, &mut rng);
        let action = client.release(at(10), &mut rng).unwrap();
        assert!(matches!(
            action,
            Action::Send(SendAction { spec }) if spec.message_type == MessageType::Release
        ));
        assert_eq!(client.state(), State::Releasing);
        // Retransmit until MRC is reached, then the client goes idle.
        let mut t = 10i64;
        for _ in 0..RELEASE_PARAMS.mrc + 2 {
            t += 700;
            let _ = client.poll(at(t), &mut rng);
        }
        assert_eq!(client.state(), State::Init);
        assert!(client.next_deadline().is_none());
        assert!(client.lease().is_none());
    }

    #[test]
    fn release_without_lease_goes_idle() {
        let mut rng = seq_rng(&[1]);
        let mut client = Dhcp6Client::new(MAC, IAID);
        assert!(client.release(at(0), &mut rng).is_none());
        assert!(client.next_deadline().is_none());
    }

    #[test]
    fn decline_then_reacquires() {
        let mut rng = seq_rng(&[1, 2, 3, 4, 5, 6, 7, 8, 9]);
        let mut client = Dhcp6Client::new(MAC, IAID);
        drive_to_bound(&mut client, &mut rng);
        let action = client.decline(at(10), &mut rng).unwrap();
        assert!(matches!(
            action,
            Action::Send(SendAction { spec }) if spec.message_type == MessageType::Decline
        ));
        assert_eq!(client.state(), State::Declining);
        let mut t = 10i64;
        for _ in 0..DECLINE_PARAMS.mrc + 2 {
            t += 5;
            let _ = client.poll(at(t), &mut rng);
        }
        // After enough declines the client solicits a fresh address.
        assert_eq!(client.state(), State::Soliciting);
    }

    #[test]
    fn no_binding_on_renew_reacquires() {
        let client_duid = Duid::ll_ethernet(MAC);
        let server = server_duid();
        let mut rng = seq_rng(&[1, 2, 3, 4, 5, 6, 7]);
        let mut client = Dhcp6Client::new(MAC, IAID);
        drive_to_bound(&mut client, &mut rng);
        let _ = client.poll(at(2 + 1800), &mut rng); // Renewing
        let rx = client.transaction_id();
        // A Reply with an IA-level NoBinding status.
        let mut out = alloc::vec::Vec::new();
        out.push(MessageType::Reply.code());
        out.extend_from_slice(&rx.to_be_bytes()[1..4]);
        push_opt(&mut out, opt::CLIENT_ID, client_duid.as_slice());
        push_opt(&mut out, opt::SERVER_ID, server.as_slice());
        let mut ia = alloc::vec::Vec::new();
        ia.extend_from_slice(&IAID.to_be_bytes());
        ia.extend_from_slice(&0u32.to_be_bytes());
        ia.extend_from_slice(&0u32.to_be_bytes());
        let mut sc = alloc::vec::Vec::new();
        sc.extend_from_slice(&status::NO_BINDING.to_be_bytes());
        push_opt(&mut ia, opt::STATUS_CODE, &sc);
        push_opt(&mut out, opt::IA_NA, &ia);
        let reply = Dhcp6Reply::parse(&out, rx, &client_duid).unwrap();
        let actions = client.on_reply(at(2000), &reply, &mut rng);
        assert!(matches!(actions.first(), Some(Action::Deconfigured)));
        assert_eq!(client.state(), State::Soliciting);
    }

    #[test]
    fn renewal_times_defaults_and_clamp() {
        // Server leaves T1/T2 to the client: ½ and ⅘ of the preferred.
        assert_eq!(renewal_times(1000, 0, 0), (500, 800));
        // Server-supplied values pass through.
        assert_eq!(renewal_times(1000, 400, 700), (400, 700));
        // An inverted pair is clamped so T1 ≤ T2.
        assert_eq!(renewal_times(1000, 900, 700), (700, 700));
        // Infinite preferred arms no renewal.
        assert_eq!(
            renewal_times(INFINITE_LIFETIME, 0, 0),
            (INFINITE_LIFETIME, INFINITE_LIFETIME)
        );
    }

    #[test]
    fn infinite_lease_arms_no_deadline() {
        let client_duid = Duid::ll_ethernet(MAC);
        let mut rng = seq_rng(&[1, 2, 3, 4]);
        let mut client = Dhcp6Client::new(MAC, IAID);
        let _ = client.poll(at(0), &mut rng);
        let sx = client.transaction_id();
        let adv = build_message(
            MessageType::Advertise,
            sx,
            addr(),
            INFINITE_LIFETIME,
            INFINITE_LIFETIME,
            0,
            0,
        );
        let r = Dhcp6Reply::parse(&adv, sx, &client_duid).unwrap();
        let _ = client.on_reply(at(1), &r, &mut rng);
        let rx = client.transaction_id();
        let rep = build_message(
            MessageType::Reply,
            rx,
            addr(),
            INFINITE_LIFETIME,
            INFINITE_LIFETIME,
            0,
            0,
        );
        let r = Dhcp6Reply::parse(&rep, rx, &client_duid).unwrap();
        let _ = client.on_reply(at(2), &r, &mut rng);
        assert_eq!(client.state(), State::Bound);
        assert!(client.next_deadline().is_none());
    }
}
