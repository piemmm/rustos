//! The DNS stub resolver (RFC 1035 / RFC 5452), `plans/DNS.md` DNS1.
//!
//! This module is the pure engine behind name resolution: the RFC 1035
//! message codec and the retry/failover state machine of a *stub* resolver
//! — a client that sends a recursion-desired query to a configured
//! recursive server and interprets the answer (RFC 1034 §5.3.1). Like the
//! [`crate::dhcp`] engine it owns no I/O and no randomness: the caller
//! supplies monotonic `now` values and, through an `rng`, the CSPRNG draws
//! RFC 5452 §9 requires for the 16-bit query id.
//!
//! # Security
//!
//! A DNS response is attacker-controlled and off-path spoofing is the
//! canonical threat. Every decode here is total (never panics on any
//! bytes), bounded (a 255-octet name ceiling, a 63-octet label ceiling, a
//! fixed-capacity answer list, a bounded compression-pointer follow count
//! that cannot loop — never an attacker-sized allocation), and fail-closed
//! (a malformed or inconsistent response is rejected whole; nothing partial
//! is surfaced). A response is accepted only when its id matches the
//! outstanding query's random id and its echoed question section matches
//! the queried name (case-insensitively), type, and class; anything else is
//! discarded. A name decoded from a response renders through
//! [`Name`]'s [`Display`](core::fmt::Display) in RFC 1035 §5.1 presentation
//! form, so a hostile `PTR` answer reaches a terminal escaped rather than as
//! raw control bytes.
//!
//! # Forward and reverse
//!
//! [`RecordType::A`] / [`RecordType::Aaaa`] resolve a name to addresses;
//! [`RecordType::Ptr`] resolves the other way, querying the `in-addr.arpa` /
//! `ip6.arpa` name [`Name::reverse`] builds from an address. Both directions
//! run through the same codec, the same acceptance test, and the same
//! retry/failover state machine — there is no second resolver.

use core::fmt::{self, Write};

use tairix_abi::time::Duration64;
use tairix_abi::Errno;

use crate::addr::{IpAddr, Ipv4Addr, Ipv6Addr};
use crate::timeutil::{from_nanos, nanos, NEVER, ONE_SEC_NANOS};

/// The UDP port a DNS server listens on (RFC 1035 §4.2.1).
pub const PORT: u16 = 53;

/// The fixed DNS message header length (RFC 1035 §4.1.1): id + flags + four
/// section counts, each 16 bits.
pub const HEADER_LEN: usize = 12;

/// The largest domain name, in its canonical wire encoding including the
/// terminating zero-length root label (RFC 1035 §2.3.4). A fixed validation
/// bound: no name a peer sends can grow past it.
pub const MAX_NAME_LEN: usize = 255;

/// The largest single label (RFC 1035 §2.3.4): 63 octets, since the two
/// high bits of a label length octet are reserved for compression pointers.
pub const MAX_LABEL_LEN: usize = 63;

/// The largest number of resolved addresses surfaced from one response. A
/// fixed validation bound: a response advertising more is truncated to this
/// many, so a hostile answer section can never size an allocation.
pub const MAX_ADDRESSES: usize = 8;

/// The largest number of recursive servers a [`DnsResolver`] will try. A
/// fixed validation bound on the configured server set.
pub const MAX_SERVERS: usize = 4;

/// The largest query this codec emits: the header, the longest possible
/// name, and the 4-byte QTYPE+QCLASS trailer. [`write_query`] needs a buffer
/// at least this large.
pub const MAX_QUERY_LEN: usize = HEADER_LEN + MAX_NAME_LEN + 4;

// DNS header flag bits (RFC 1035 §4.1.1), in the 16-bit flags word.
const FLAG_QR: u16 = 0x8000;
const FLAG_OPCODE_MASK: u16 = 0x7800;
const FLAG_TC: u16 = 0x0200;
const FLAG_RD: u16 = 0x0100;
const FLAG_RCODE_MASK: u16 = 0x000F;

/// The `IN` (Internet) class (RFC 1035 §3.2.4); the only class this resolver
/// queries or accepts.
const CLASS_IN: u16 = 1;

// Resource-record TYPE values (RFC 1035 §3.2.2, RFC 3596 §2.1).
const TYPE_A: u16 = 1;
const TYPE_CNAME: u16 = 5;
const TYPE_PTR: u16 = 12;
const TYPE_AAAA: u16 = 28;

/// A record type this stub resolver can query for.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum RecordType {
    /// An IPv4 host address (RFC 1035 `A`).
    A,
    /// An IPv6 host address (RFC 3596 `AAAA`).
    Aaaa,
    /// The domain name an address maps back to (RFC 1035 `PTR`), queried
    /// under [`Name::reverse`]'s `in-addr.arpa` / `ip6.arpa` spelling.
    Ptr,
}

impl RecordType {
    /// The wire TYPE value.
    #[must_use]
    pub const fn value(self) -> u16 {
        match self {
            Self::A => TYPE_A,
            Self::Aaaa => TYPE_AAAA,
            Self::Ptr => TYPE_PTR,
        }
    }

    /// The presentation spelling (`A`, `AAAA`, `PTR`) a diagnostic or a
    /// `-t` option names the type by.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::A => "A",
            Self::Aaaa => "AAAA",
            Self::Ptr => "PTR",
        }
    }
}

/// A DNS response code (RFC 1035 §4.1.1, RFC 6895 §2.3).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Rcode {
    /// No error condition (the query succeeded).
    NoError,
    /// The server could not interpret the query (format error).
    FormErr,
    /// The server failed (transient; another server may succeed).
    ServFail,
    /// Authoritative "the queried name does not exist" (RFC 8020).
    NxDomain,
    /// The server does not support the requested query.
    NotImp,
    /// The server refused to answer for policy reasons.
    Refused,
    /// Any other code, carried verbatim.
    Other(u8),
}

impl Rcode {
    /// The response code a 4-bit RCODE field value denotes.
    #[must_use]
    pub const fn from_bits(bits: u8) -> Self {
        match bits {
            0 => Self::NoError,
            1 => Self::FormErr,
            2 => Self::ServFail,
            3 => Self::NxDomain,
            4 => Self::NotImp,
            5 => Self::Refused,
            other => Self::Other(other),
        }
    }
}

/// An error building a [`Name`] or encoding a query.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DnsError {
    /// A label was empty (an interior `..`) or exceeded [`MAX_LABEL_LEN`],
    /// or carried a byte outside the printable-ASCII hostname range.
    InvalidLabel,
    /// The encoded name exceeded [`MAX_NAME_LEN`].
    NameTooLong,
    /// The output buffer was smaller than the encoded message.
    BufferTooSmall,
}

/// Fold an ASCII letter to lower case, leaving every other octet unchanged
/// (RFC 4343: DNS name comparison is ASCII-case-insensitive).
const fn ascii_lower(b: u8) -> u8 {
    if b.is_ascii_uppercase() {
        b + 32
    } else {
        b
    }
}

/// The `in-addr.arpa` reverse zone, already in length-prefixed wire form so
/// appending it is a copy with no length arithmetic.
const REVERSE_V4_SUFFIX: &[u8] = b"\x07in-addr\x04arpa";

/// The `ip6.arpa` reverse zone, in wire form.
const REVERSE_V6_SUFFIX: &[u8] = b"\x03ip6\x04arpa";

// The longest name [`Name::reverse`] builds must fit the fixed bound, which
// is what makes it infallible: four 3-digit labels for IPv4, 32 one-nibble
// labels for IPv6, each zone suffix, and the root label.
const _: () = assert!(4 * 4 + REVERSE_V4_SUFFIX.len() < MAX_NAME_LEN);
const _: () = assert!(32 * 2 + REVERSE_V6_SUFFIX.len() < MAX_NAME_LEN);

/// The lower-case hexadecimal digit of `nibble`'s low four bits.
const fn hex_digit(nibble: u8) -> u8 {
    match nibble & 0x0F {
        d @ 0..=9 => b'0' + d,
        d => b'a' + (d - 10),
    }
}

/// Write `octet`'s decimal spelling into `out` as one length-prefixed
/// label, returning the octets written (2, 3, or 4).
fn write_decimal_label(out: &mut [u8], octet: u8) -> usize {
    let (len, digits) = if octet >= 100 {
        (
            3usize,
            [
                b'0' + octet / 100,
                b'0' + (octet / 10) % 10,
                b'0' + octet % 10,
            ],
        )
    } else if octet >= 10 {
        (2, [b'0' + octet / 10, b'0' + octet % 10, 0])
    } else {
        (1, [b'0' + octet, 0, 0])
    };
    // `len` is 1..=3, so the narrowing is exact.
    out[0] = u8::try_from(len).unwrap_or(1);
    out[1..=len].copy_from_slice(&digits[..len]);
    1 + len
}

/// Write one octet of a label in RFC 1035 §5.1 presentation form.
///
/// Everything outside printable ASCII becomes a `\DDD` escape and the two
/// syntactic characters are backslash-escaped. A `PTR` answer is
/// attacker-controlled text that ends up on a terminal, so it never reaches
/// one as raw control bytes.
fn write_presentation_byte(f: &mut fmt::Formatter<'_>, byte: u8) -> fmt::Result {
    match byte {
        b'.' | b'\\' => {
            f.write_char('\\')?;
            f.write_char(char::from(byte))
        }
        0x21..=0x7e => f.write_char(char::from(byte)),
        _ => write!(f, "\\{byte:03}"),
    }
}

/// A domain name in its canonical wire encoding: a sequence of
/// length-prefixed labels ended by a zero-length root label, with every
/// ASCII letter folded to lower case so two names compare equal iff they
/// are equal under RFC 4343 case-insensitivity.
///
/// The encoding is never compressed (compression pointers only ever appear
/// *inside a message*; the internal reader expands them), and is bounded by
/// [`MAX_NAME_LEN`], so a `Name` is a fixed-size value that allocates
/// nothing.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct Name {
    wire: [u8; MAX_NAME_LEN],
    len: usize,
}

impl Name {
    /// The root name (`.`), a single zero-length label.
    #[must_use]
    pub const fn root() -> Self {
        let mut wire = [0u8; MAX_NAME_LEN];
        wire[0] = 0;
        Self { wire, len: 1 }
    }

    /// Encode a dotted domain name (e.g. `"www.example.com"`) into its
    /// canonical wire form.
    ///
    /// A single trailing dot (the fully-qualified form) is accepted; an
    /// empty string or a lone `"."` is the root. Each label must be
    /// non-empty, at most [`MAX_LABEL_LEN`] octets, and composed of
    /// printable-ASCII hostname characters (a control byte, a space, or a
    /// non-ASCII byte is rejected — a resolver queries host names, and a
    /// permissive encoder would be a needless attack surface). Fails closed
    /// with [`DnsError`] on any violation.
    pub fn encode(dotted: &str) -> Result<Self, DnsError> {
        // The empty string and a lone "." both name the DNS root.
        if dotted.is_empty() || dotted == "." {
            return Ok(Self::root());
        }
        let mut wire = [0u8; MAX_NAME_LEN];
        let mut len = 0usize;
        let bytes = dotted.as_bytes();
        // A single trailing dot denotes the fully-qualified form; drop it so
        // "example.com" and "example.com." encode identically.
        let bytes = match bytes.split_last() {
            Some((b'.', rest)) if !rest.is_empty() => rest,
            _ => bytes,
        };
        if !bytes.is_empty() {
            for label in bytes.split(|&b| b == b'.') {
                if label.is_empty() || label.len() > MAX_LABEL_LEN {
                    return Err(DnsError::InvalidLabel);
                }
                // The length octet plus the label must fit, leaving room for
                // the terminating root label written below.
                if len + 1 + label.len() + 1 > MAX_NAME_LEN {
                    return Err(DnsError::NameTooLong);
                }
                wire[len] = u8::try_from(label.len()).map_err(|_| DnsError::InvalidLabel)?;
                len += 1;
                for &b in label {
                    if !(0x21..=0x7e).contains(&b) {
                        return Err(DnsError::InvalidLabel);
                    }
                    wire[len] = ascii_lower(b);
                    len += 1;
                }
            }
        }
        // The zero-length root label terminates every name.
        wire[len] = 0;
        len += 1;
        Ok(Self { wire, len })
    }

    /// The reverse-lookup name of `addr`: `d.c.b.a.in-addr.arpa` for IPv4
    /// (RFC 1035 §3.5) and the address's 32 nibbles, least significant
    /// first, under `ip6.arpa` for IPv6 (RFC 3596 §2.5). This is the name a
    /// [`RecordType::Ptr`] query for that address asks about.
    ///
    /// Total and infallible: the longest form is 74 octets, well inside
    /// [`MAX_NAME_LEN`] (the `const` guards below hold it so).
    #[must_use]
    pub fn reverse(addr: IpAddr) -> Self {
        let mut wire = [0u8; MAX_NAME_LEN];
        let mut len = 0usize;
        let suffix = match addr {
            IpAddr::V4(v4) => {
                for &octet in v4.octets().iter().rev() {
                    len += write_decimal_label(&mut wire[len..], octet);
                }
                REVERSE_V4_SUFFIX
            }
            IpAddr::V6(v6) => {
                for &byte in v6.octets().iter().rev() {
                    wire[len..len + 4].copy_from_slice(&[
                        1,
                        hex_digit(byte & 0x0F),
                        1,
                        hex_digit(byte >> 4),
                    ]);
                    len += 4;
                }
                REVERSE_V6_SUFFIX
            }
        };
        wire[len..len + suffix.len()].copy_from_slice(suffix);
        len += suffix.len();
        wire[len] = 0;
        len += 1;
        Self { wire, len }
    }

    /// The canonical wire encoding, including the terminating root label.
    #[must_use]
    pub fn as_wire(&self) -> &[u8] {
        &self.wire[..self.len]
    }

    /// Read a (possibly compressed) name from `msg` starting at `start`,
    /// expanding it into canonical form and returning it with the offset of
    /// the first octet *after* the name in the record stream (RFC 1035
    /// §4.1.4).
    ///
    /// Compression pointers are followed, but every followed pointer must
    /// target an offset strictly *before* the pointer itself, so the walk
    /// is monotonically decreasing and cannot loop; the expanded length is
    /// bounded by [`MAX_NAME_LEN`]. Returns `None` (fail closed) on any
    /// out-of-range offset, reserved label-type, over-length name, or a
    /// pointer that does not point backwards.
    fn read(msg: &[u8], start: usize) -> Option<(Self, usize)> {
        let mut wire = [0u8; MAX_NAME_LEN];
        let mut len = 0usize;
        let mut pos = start;
        // The offset just past the name as it appears in the record stream,
        // fixed at the first pointer we follow (or the root label if none).
        let mut next_pos: Option<usize> = None;
        // The lowest offset a further pointer is allowed to target: each
        // pointer must jump strictly backwards, which alone guarantees
        // termination.
        let mut min_target = start;
        loop {
            let &first = msg.get(pos)?;
            match first & 0xC0 {
                0x00 => {
                    let label_len = usize::from(first);
                    if label_len == 0 {
                        wire[len] = 0;
                        len += 1;
                        let end = pos + 1;
                        return Some((Self { wire, len }, next_pos.unwrap_or(end)));
                    }
                    let label = msg.get(pos + 1..pos + 1 + label_len)?;
                    // The length octet + label + a future root label must
                    // fit the fixed bound.
                    if len + 1 + label_len + 1 > MAX_NAME_LEN {
                        return None;
                    }
                    wire[len] = first;
                    len += 1;
                    for &b in label {
                        wire[len] = ascii_lower(b);
                        len += 1;
                    }
                    pos += 1 + label_len;
                }
                0xC0 => {
                    // A 14-bit pointer: the low 6 bits of this octet plus the
                    // whole next octet.
                    let &second = msg.get(pos + 1)?;
                    let target = ((usize::from(first & 0x3F)) << 8) | usize::from(second);
                    if target >= min_target {
                        // Not strictly backwards: reject rather than risk a
                        // loop.
                        return None;
                    }
                    if next_pos.is_none() {
                        next_pos = Some(pos + 2);
                    }
                    min_target = target;
                    pos = target;
                }
                // 0x40 and 0x80 are reserved label types (RFC 6891 retired
                // the only ever-defined extended type); reject.
                _ => return None,
            }
        }
    }
}

/// Renders the name in RFC 1035 §5.1 presentation form — labels joined by
/// `.`, without the trailing root dot, every non-printable octet escaped —
/// so a hostile `PTR` answer cannot smuggle control bytes onto a terminal.
/// The root name renders as `.`.
impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let wire = self.as_wire();
        if wire == [0] {
            return f.write_str(".");
        }
        let mut pos = 0usize;
        let mut first = true;
        while let Some(&label_len) = wire.get(pos) {
            let Some(label) = wire.get(pos + 1..pos + 1 + usize::from(label_len)) else {
                break;
            };
            if label.is_empty() {
                break;
            }
            if !first {
                f.write_char('.')?;
            }
            first = false;
            for &byte in label {
                write_presentation_byte(f, byte)?;
            }
            pos += 1 + usize::from(label_len);
        }
        Ok(())
    }
}

/// The 4-bit RCODE field extracted from the flags word (RFC 1035 §4.1.1).
fn rcode_bits(flags: u16) -> u8 {
    // The mask keeps the value in 0..=15, so the narrowing is lossless.
    u8::try_from(flags & FLAG_RCODE_MASK).unwrap_or(0)
}

/// Read a big-endian `u16` at `off`, or `None` if out of range.
fn read_u16(msg: &[u8], off: usize) -> Option<u16> {
    let b = msg.get(off..off + 2)?;
    Some(u16::from_be_bytes([b[0], b[1]]))
}

/// Read a big-endian `u32` at `off`, or `None` if out of range.
fn read_u32(msg: &[u8], off: usize) -> Option<u32> {
    let b = msg.get(off..off + 4)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// The outstanding query a [`DnsResponse`] is matched against and that
/// [`write_query`] serialises.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QuerySpec {
    /// The 16-bit transaction id (a CSPRNG draw — RFC 5452 §9).
    pub id: u16,
    /// The name being resolved.
    pub name: Name,
    /// The record type being resolved.
    pub record_type: RecordType,
    /// Whether the RD (recursion desired) bit is set (a stub resolver
    /// always sets it, querying a recursive server).
    pub recursion_desired: bool,
}

/// Encode `spec` as a single standard DNS query into `out`, returning the
/// number of octets written.
///
/// Emits the 12-byte header (id, flags with the query/opcode/RD bits, one
/// question, no other records) followed by the one question. Fails closed
/// with [`DnsError::BufferTooSmall`] when `out` is shorter than the encoded
/// message ([`MAX_QUERY_LEN`] always suffices).
pub fn write_query(spec: &QuerySpec, out: &mut [u8]) -> Result<usize, DnsError> {
    let name = spec.name.as_wire();
    let total = HEADER_LEN + name.len() + 4;
    if out.len() < total {
        return Err(DnsError::BufferTooSmall);
    }
    let flags = if spec.recursion_desired { FLAG_RD } else { 0 };
    out[0..2].copy_from_slice(&spec.id.to_be_bytes());
    out[2..4].copy_from_slice(&flags.to_be_bytes());
    out[4..6].copy_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    out[6..8].copy_from_slice(&0u16.to_be_bytes()); // ANCOUNT
    out[8..10].copy_from_slice(&0u16.to_be_bytes()); // NSCOUNT
    out[10..12].copy_from_slice(&0u16.to_be_bytes()); // ARCOUNT
    let mut pos = HEADER_LEN;
    out[pos..pos + name.len()].copy_from_slice(name);
    pos += name.len();
    out[pos..pos + 2].copy_from_slice(&spec.record_type.value().to_be_bytes());
    out[pos + 2..pos + 4].copy_from_slice(&CLASS_IN.to_be_bytes());
    Ok(total)
}

/// A bounded list of resolved IP addresses surfaced from a response. Holds
/// at most [`MAX_ADDRESSES`]; entries past that are dropped, so a hostile
/// answer section can never size an allocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AddrList {
    entries: [IpAddr; MAX_ADDRESSES],
    len: usize,
}

impl Default for AddrList {
    fn default() -> Self {
        Self {
            entries: [IpAddr::V4(Ipv4Addr::UNSPECIFIED); MAX_ADDRESSES],
            len: 0,
        }
    }
}

impl AddrList {
    /// Build a list from `addrs`, keeping at most [`MAX_ADDRESSES`] (a longer
    /// slice is truncated to the fixed capacity).
    ///
    /// Lets a caller construct the public [`Resolution`] type — e.g. a test
    /// double standing in for a real lookup, or a future synthetic resolver —
    /// without reaching into the private layout.
    #[must_use]
    pub fn from_addrs(addrs: &[IpAddr]) -> Self {
        let mut list = Self::default();
        for addr in addrs {
            list.push(*addr);
        }
        list
    }

    /// The addresses collected, in wire order.
    #[must_use]
    pub fn as_slice(&self) -> &[IpAddr] {
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
    pub fn first(&self) -> Option<IpAddr> {
        (self.len > 0).then_some(self.entries[0])
    }

    /// Append `addr` unless the fixed capacity is already reached.
    fn push(&mut self, addr: IpAddr) {
        if self.len < MAX_ADDRESSES {
            self.entries[self.len] = addr;
            self.len += 1;
        }
    }
}

/// What a response or a finished resolution answered with, by query type.
///
/// One value rather than parallel fields, so an address answer cannot carry
/// a name nor a `PTR` answer an address.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Answer {
    /// `A`/`AAAA`: the addresses, in wire order. Empty for a negative,
    /// timed-out, or unanswered lookup.
    Addresses(AddrList),
    /// `PTR`: the domain name the queried address maps back to, absent for
    /// a negative, timed-out, or unanswered lookup.
    ///
    /// Only the first `PTR` record of an answer is surfaced — the
    /// `getnameinfo` contract every consumer of a reverse lookup wants, and
    /// RFC 1912 §2.1 discourages several records at one address.
    Pointer(Option<Name>),
}

impl Answer {
    /// The empty answer to a query of `record_type`: what a negative,
    /// timed-out, or not-yet-started resolution carries.
    #[must_use]
    pub fn empty(record_type: RecordType) -> Self {
        match record_type {
            RecordType::A | RecordType::Aaaa => Self::Addresses(AddrList::default()),
            RecordType::Ptr => Self::Pointer(None),
        }
    }

    /// Whether nothing was answered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Addresses(list) => list.is_empty(),
            Self::Pointer(name) => name.is_none(),
        }
    }

    /// The addresses answered, in wire order; empty for a `PTR` answer.
    #[must_use]
    pub fn addresses(&self) -> &[IpAddr] {
        match self {
            Self::Addresses(list) => list.as_slice(),
            Self::Pointer(_) => &[],
        }
    }

    /// The name a `PTR` answer pointed at, or `None`.
    #[must_use]
    pub fn pointer(&self) -> Option<&Name> {
        match self {
            Self::Pointer(name) => name.as_ref(),
            Self::Addresses(_) => None,
        }
    }
}

/// A parsed, validated DNS response to an outstanding [`QuerySpec`].
///
/// Only the fields a stub resolver acts on are surfaced: the response code,
/// the truncation flag, the answer for the queried type (with any CNAME
/// chain in the answer section followed first), and the minimum TTL across
/// the records used.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DnsResponse {
    /// The transaction id (equal to the matched query's id).
    pub id: u16,
    /// The response code.
    pub rcode: Rcode,
    /// Whether the TC (truncation) bit was set. Over UDP without EDNS0 this
    /// means the answer did not fit; the resolver treats it as a soft
    /// per-server failure.
    pub truncated: bool,
    /// What the answer section resolved to for the queried type.
    pub answer: Answer,
    /// The minimum TTL (seconds) across the records used, or `0` when none
    /// were used.
    pub min_ttl: u32,
}

impl DnsResponse {
    /// Parse and validate a response datagram against the outstanding
    /// `query`.
    ///
    /// Returns `None` (fail closed) unless the message is a well-formed
    /// response whose id, opcode, and single echoed question exactly match
    /// `query` (QNAME case-insensitively, QTYPE, QCLASS `IN`) — the RFC 5452
    /// §9 acceptance test that, together with the random id, bounds off-path
    /// spoofing. Any structural error in the header, question, or answer
    /// section rejects the whole message. Answer records are followed
    /// through a CNAME chain from the queried name to collect the
    /// matching-type answer (addresses bounded by [`MAX_ADDRESSES`]; the
    /// first `PTR` name for a reverse lookup).
    #[must_use]
    pub fn parse(bytes: &[u8], query: &QuerySpec) -> Option<Self> {
        if bytes.len() < HEADER_LEN {
            return None;
        }
        let id = read_u16(bytes, 0)?;
        // Match the transaction id before anything else (spoof defence).
        if id != query.id {
            return None;
        }
        let flags = read_u16(bytes, 2)?;
        // Must be a response to a standard query (opcode 0).
        if flags & FLAG_QR == 0 || flags & FLAG_OPCODE_MASK != 0 {
            return None;
        }
        let rcode = Rcode::from_bits(rcode_bits(flags));
        let truncated = flags & FLAG_TC != 0;
        let qdcount = read_u16(bytes, 4)?;
        let ancount = read_u16(bytes, 6)?;
        // A standard response echoes exactly the one question we asked.
        if qdcount != 1 {
            return None;
        }
        let (qname, mut pos) = Name::read(bytes, HEADER_LEN)?;
        let qtype = read_u16(bytes, pos)?;
        let qclass = read_u16(bytes, pos + 2)?;
        pos += 4;
        // The echoed question must match the outstanding query exactly.
        if qname != query.name || qtype != query.record_type.value() || qclass != CLASS_IN {
            return None;
        }

        let mut answer = Answer::empty(query.record_type);
        let mut min_ttl = u32::MAX;
        let mut used_any = false;
        // The owner name we are currently resolving; a CNAME record retargets
        // it so the alias chain is followed within this one response.
        let mut target = query.name;
        let wanted = query.record_type.value();

        for _ in 0..ancount {
            let (owner, after_name) = Name::read(bytes, pos)?;
            pos = after_name;
            let rtype = read_u16(bytes, pos)?;
            let rclass = read_u16(bytes, pos + 2)?;
            let ttl = read_u32(bytes, pos + 4)?;
            let rdlength = usize::from(read_u16(bytes, pos + 8)?);
            pos += 10;
            let rdata_start = pos;
            // The record data must lie within the message.
            bytes.get(rdata_start..rdata_start + rdlength)?;
            if rclass == CLASS_IN && owner == target {
                if rtype == wanted {
                    if fold_rdata(&mut answer, query.record_type, bytes, rdata_start, rdlength) {
                        min_ttl = min_ttl.min(ttl);
                        used_any = true;
                    }
                } else if rtype == TYPE_CNAME {
                    // Follow the alias: the RDATA is a (possibly compressed)
                    // name naming the canonical target.
                    let (cname, _) = Name::read(bytes, rdata_start)?;
                    target = cname;
                    min_ttl = min_ttl.min(ttl);
                }
            }
            pos = rdata_start + rdlength;
        }

        Some(Self {
            id,
            rcode,
            truncated,
            answer,
            min_ttl: if used_any { min_ttl } else { 0 },
        })
    }
}

/// Fold one matching-type record's RDATA into `answer`, reporting whether it
/// was well-formed and used. A record whose length or shape does not match
/// the queried type is skipped, never accepted — an `A` record claiming 16
/// octets of RDATA is malformed, not an IPv6 address.
fn fold_rdata(
    answer: &mut Answer,
    record_type: RecordType,
    msg: &[u8],
    start: usize,
    rdlength: usize,
) -> bool {
    match (record_type, answer) {
        (RecordType::A, Answer::Addresses(list)) if rdlength == 4 => {
            let Some(b) = msg.get(start..start + 4) else {
                return false;
            };
            list.push(IpAddr::V4(Ipv4Addr::new(b[0], b[1], b[2], b[3])));
            true
        }
        (RecordType::Aaaa, Answer::Addresses(list)) if rdlength == 16 => {
            let Some(b) = msg.get(start..start + 16) else {
                return false;
            };
            let mut octets = [0u8; 16];
            octets.copy_from_slice(b);
            list.push(IpAddr::V6(Ipv6Addr::from(octets)));
            true
        }
        // The RDATA is one (possibly compressed) domain name that must span
        // exactly the declared length; only the first record answers.
        (RecordType::Ptr, Answer::Pointer(slot @ None)) => {
            let Some((name, end)) = Name::read(msg, start) else {
                return false;
            };
            if end != start + rdlength {
                return false;
            }
            *slot = Some(name);
            true
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Resolver state machine (RFC 1034 §5.3.1 stub resolver)
// ---------------------------------------------------------------------------

/// The initial per-attempt retransmission timeout, in seconds. Doubled on
/// each retransmission to the same server, capped at [`MAX_TIMEOUT_SECS`].
const INITIAL_TIMEOUT_SECS: u64 = 1;

/// The ceiling for the doubled per-attempt timeout, in seconds.
const MAX_TIMEOUT_SECS: u64 = 5;

/// Transmissions to a single server before failing over to the next. The
/// classic resolver default: an initial send plus one retransmission.
const ATTEMPTS_PER_SERVER: u32 = 2;

/// The outcome classification of a finished resolution.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ResolveStatus {
    /// A server answered `NoError` with at least one address of the queried
    /// type.
    Success,
    /// A server answered `NoError` but the answer held no record of the
    /// queried type (RFC 2308 NODATA, or a CNAME chain whose target this
    /// stub did not itself pursue).
    NoData,
    /// A server authoritatively answered that the name does not exist
    /// (`NXDOMAIN`, RFC 8020).
    NonExistent,
    /// No configured server produced a usable answer within the retry
    /// budget.
    Timeout,
}

/// The result of a finished resolution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Resolution {
    /// How the resolution concluded.
    pub status: ResolveStatus,
    /// What was resolved (empty unless `status` is
    /// [`ResolveStatus::Success`]).
    pub answer: Answer,
    /// The minimum TTL (seconds) a caching caller may hold the answer for,
    /// or `0` when there is nothing to cache.
    pub ttl_secs: u32,
}

impl Resolution {
    /// The resolved addresses, in wire order; empty for a reverse lookup or
    /// a negative answer.
    #[must_use]
    pub fn addresses(&self) -> &[IpAddr] {
        self.answer.addresses()
    }

    /// The name a reverse lookup resolved to, or `None`.
    #[must_use]
    pub fn pointer(&self) -> Option<&Name> {
        self.answer.pointer()
    }
}

/// An action a [`DnsResolver`] poll or response-fold asks the caller to
/// perform.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    /// Encode `query` with [`write_query`] and send it to `server` on UDP
    /// [`PORT`].
    Send {
        /// The query to transmit.
        query: QuerySpec,
        /// The recursive server to transmit it to.
        server: IpAddr,
    },
    /// The resolution finished; act on the [`Resolution`] and drive this
    /// resolver no further.
    Finished(Resolution),
}

/// A bounded list of the recursive servers a [`DnsResolver`] tries, in
/// order. Holds at most [`MAX_SERVERS`]; a longer configured list is
/// truncated (a fixed validation bound).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ServerList {
    entries: [IpAddr; MAX_SERVERS],
    len: usize,
}

impl ServerList {
    fn from_slice(servers: &[IpAddr]) -> Self {
        let mut entries = [IpAddr::V4(Ipv4Addr::UNSPECIFIED); MAX_SERVERS];
        let len = servers.len().min(MAX_SERVERS);
        entries[..len].copy_from_slice(&servers[..len]);
        Self { entries, len }
    }

    fn get(&self, idx: usize) -> Option<IpAddr> {
        (idx < self.len).then(|| self.entries[idx])
    }
}

/// The resolver's internal phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    /// Not started; the next [`DnsResolver::poll`] sends the first query.
    Idle,
    /// A query is outstanding; awaiting a response or the retransmit
    /// deadline.
    Waiting,
    /// The resolution has finished; the resolver is inert.
    Done,
}

/// The pure stub-resolver state machine for one name and record type.
///
/// Construct with [`DnsResolver::new`], then drive it event-first, exactly
/// like [`crate::dhcp::DhcpClient`]:
///
/// - call [`DnsResolver::poll`] once to start, and again whenever the
///   one-shot timer armed from [`DnsResolver::next_deadline`] fires, to
///   retransmit and fail over between servers;
/// - call [`DnsResolver::on_response`] with each datagram received on the
///   query socket.
///
/// Both return the [`Action`] the caller performs. The engine owns no I/O
/// and never blocks; the caller supplies monotonic `now` values and, through
/// `rng`, the CSPRNG draws for the query id (RFC 5452 §9) and the
/// retransmission jitter.
#[derive(Clone, Debug)]
pub struct DnsResolver {
    name: Name,
    record_type: RecordType,
    servers: ServerList,
    phase: Phase,
    server_idx: usize,
    attempts: u32,
    id: u16,
    timeout_secs: u64,
    retransmit: u128,
}

impl DnsResolver {
    /// Construct a resolver for `name`/`record_type` that will try
    /// `servers` in order (truncated to [`MAX_SERVERS`]). The first
    /// [`DnsResolver::poll`] begins the query; an empty server list finishes
    /// immediately as [`ResolveStatus::Timeout`].
    #[must_use]
    pub fn new(name: Name, record_type: RecordType, servers: &[IpAddr]) -> Self {
        Self {
            name,
            record_type,
            servers: ServerList::from_slice(servers),
            phase: Phase::Idle,
            server_idx: 0,
            attempts: 0,
            id: 0,
            timeout_secs: INITIAL_TIMEOUT_SECS,
            retransmit: NEVER,
        }
    }

    /// The record type being resolved.
    #[must_use]
    pub fn record_type(&self) -> RecordType {
        self.record_type
    }

    /// Whether the resolution has finished (a [`DnsResolver::poll`] or
    /// [`DnsResolver::on_response`] returned [`Action::Finished`]).
    #[must_use]
    pub fn is_done(&self) -> bool {
        matches!(self.phase, Phase::Done)
    }

    /// The next instant [`DnsResolver::poll`] has timed work to do (the
    /// retransmit/failover deadline), or `None` when none is armed.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Duration64> {
        (matches!(self.phase, Phase::Waiting) && self.retransmit != NEVER)
            .then(|| from_nanos(self.retransmit))
    }

    /// Start the resolution or advance its retransmission/failover timers.
    ///
    /// Returns the [`Action`] to perform, or `None` when no timed work is
    /// due yet (the caller keeps the armed timer) or the resolution is
    /// already finished.
    pub fn poll(&mut self, now: Duration64, rng: &mut dyn FnMut() -> u32) -> Option<Action> {
        match self.phase {
            Phase::Idle => Some(self.begin(now, rng)),
            Phase::Waiting => {
                if self.retransmit != NEVER && nanos(now) >= self.retransmit {
                    Some(self.on_timeout(now, rng))
                } else {
                    None
                }
            }
            Phase::Done => None,
        }
    }

    /// Fold a response datagram into the resolver.
    ///
    /// A datagram that does not parse and match the outstanding query
    /// (`None` from [`DnsResponse::parse`]) is ignored and the resolver
    /// keeps waiting. A matching response either finishes the resolution or
    /// fails the current server over to the next; the returned [`Action`] is
    /// the resulting send or [`Action::Finished`].
    pub fn on_response(
        &mut self,
        now: Duration64,
        bytes: &[u8],
        rng: &mut dyn FnMut() -> u32,
    ) -> Option<Action> {
        if !matches!(self.phase, Phase::Waiting) {
            return None;
        }
        let spec = QuerySpec {
            id: self.id,
            name: self.name,
            record_type: self.record_type,
            recursion_desired: true,
        };
        let resp = DnsResponse::parse(bytes, &spec)?;
        match resp.rcode {
            Rcode::NoError if resp.truncated => Some(self.next_server(now, rng)),
            Rcode::NoError => {
                let status = if resp.answer.is_empty() {
                    ResolveStatus::NoData
                } else {
                    ResolveStatus::Success
                };
                Some(self.finish(status, &resp.answer, resp.min_ttl))
            }
            Rcode::NxDomain => Some(self.finish(
                ResolveStatus::NonExistent,
                &Answer::empty(self.record_type),
                0,
            )),
            // A transient or policy failure: try the next server.
            Rcode::ServFail | Rcode::Refused | Rcode::NotImp | Rcode::FormErr | Rcode::Other(_) => {
                Some(self.next_server(now, rng))
            }
        }
    }

    /// Begin the resolution: pick the first server (or finish closed when
    /// none are configured).
    fn begin(&mut self, now: Duration64, rng: &mut dyn FnMut() -> u32) -> Action {
        self.server_idx = 0;
        self.attempts = 0;
        self.timeout_secs = INITIAL_TIMEOUT_SECS;
        self.send_to_current(now, rng, true).unwrap_or_else(|| {
            self.finish(ResolveStatus::Timeout, &Answer::empty(self.record_type), 0)
        })
    }

    /// Handle a fired retransmit deadline: resend to the current server
    /// while the per-server budget remains, else fail over.
    fn on_timeout(&mut self, now: Duration64, rng: &mut dyn FnMut() -> u32) -> Action {
        if self.attempts < ATTEMPTS_PER_SERVER {
            self.timeout_secs = (self.timeout_secs * 2).min(MAX_TIMEOUT_SECS);
            // Retransmit to the same server with the same id (a duplicate the
            // server simply re-answers).
            self.send_to_current(now, rng, false).unwrap_or_else(|| {
                self.finish(ResolveStatus::Timeout, &Answer::empty(self.record_type), 0)
            })
        } else {
            self.next_server(now, rng)
        }
    }

    /// Advance to the next configured server, or finish as a timeout when
    /// the list is exhausted.
    fn next_server(&mut self, now: Duration64, rng: &mut dyn FnMut() -> u32) -> Action {
        self.server_idx += 1;
        self.attempts = 0;
        self.timeout_secs = INITIAL_TIMEOUT_SECS;
        self.send_to_current(now, rng, true).unwrap_or_else(|| {
            self.finish(ResolveStatus::Timeout, &Answer::empty(self.record_type), 0)
        })
    }

    /// Send the query to the current server, arming the retransmit deadline.
    /// Returns `None` when `server_idx` is past the end of the list (the
    /// caller turns that into a timeout finish).
    fn send_to_current(
        &mut self,
        now: Duration64,
        rng: &mut dyn FnMut() -> u32,
        fresh_id: bool,
    ) -> Option<Action> {
        let server = self.servers.get(self.server_idx)?;
        if fresh_id {
            // A fresh 16-bit id per server; the top half of the CSPRNG word
            // (its strongest bits) folded into the field losslessly.
            self.id = u16::try_from(rng() >> 16).unwrap_or(0);
        }
        self.attempts += 1;
        // A jitter in [0, 1s) desynchronises retransmissions across many
        // resolvers (the CSPRNG draw keeps it unpredictable to an off-path
        // observer trying to guess the send instant).
        let jitter = u128::from(rng() % 1_000_000_000);
        let delay = u128::from(self.timeout_secs) * ONE_SEC_NANOS + jitter;
        self.retransmit = nanos(now).saturating_add(delay);
        self.phase = Phase::Waiting;
        Some(Action::Send {
            query: QuerySpec {
                id: self.id,
                name: self.name,
                record_type: self.record_type,
                recursion_desired: true,
            },
            server,
        })
    }

    /// Mark the resolution finished and build its [`Resolution`].
    fn finish(&mut self, status: ResolveStatus, answer: &Answer, ttl_secs: u32) -> Action {
        self.phase = Phase::Done;
        self.retransmit = NEVER;
        Action::Finished(Resolution {
            status,
            answer: *answer,
            ttl_secs,
        })
    }
}

// ---------------------------------------------------------------------------
// Blocking driver over an abstract datagram transport
// ---------------------------------------------------------------------------

/// The largest DNS message this resolver receives over UDP. Classic RFC
/// 1035 §4.2.1 messages are capped at 512 octets (no EDNS0 here — a larger
/// answer sets the TC bit, which the engine treats as a per-server soft
/// failure), so a fixed 512-octet reception buffer is a fixed validation
/// bound: a datagram longer than this is truncated to it before parsing,
/// and a hostile server can never size an allocation.
pub const MAX_MESSAGE_LEN: usize = 512;

/// The outcome of a single [`DnsTransport::wait`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Wait {
    /// A datagram arrived; its first `usize` octets were written into the
    /// buffer `wait` was given.
    Datagram(usize),
    /// The armed deadline elapsed with no datagram (drive a retransmit).
    TimedOut,
}

/// The datagram transport a [`resolve`] loop drives the pure [`DnsResolver`]
/// over.
///
/// The engine owns no I/O; this trait is the seam between it and a real
/// UDP socket (the `netsock-v1` client of `lib/resolver`) or a test double.
/// It carries the monotonic clock the resolver's deadlines are read against,
/// so a single implementation controls time and I/O coherently. Every
/// method fails closed with a typed [`Errno`]; a transport error aborts the
/// resolution rather than being silently ignored.
pub trait DnsTransport {
    /// The current monotonic instant, on the same clock the resolver's
    /// deadlines use.
    fn now(&mut self) -> Duration64;

    /// Send an already-encoded `query` datagram to `server` on UDP
    /// [`PORT`].
    ///
    /// # Errors
    ///
    /// The transport's own typed [`Errno`] (e.g. an unreachable network),
    /// which aborts the resolution.
    fn send(&mut self, server: IpAddr, query: &[u8]) -> Result<(), Errno>;

    /// Block until a datagram arrives or `deadline` passes, writing an
    /// arriving datagram's bytes into `buf` (truncated to its length).
    ///
    /// `deadline` is the absolute instant the caller must not wait past; it
    /// is always `Some` in the driver loop (the engine always arms a
    /// retransmit deadline while a query is outstanding).
    ///
    /// # Errors
    ///
    /// The transport's own typed [`Errno`], which aborts the resolution.
    fn wait(&mut self, deadline: Duration64, buf: &mut [u8]) -> Result<Wait, Errno>;
}

/// Resolve `name`/`record_type` against `servers` by driving the pure
/// [`DnsResolver`] over `transport`, blocking until the resolution
/// concludes (success, negative answer, or the retry budget is spent) and
/// returning the [`Resolution`].
///
/// This is the one shared driver loop: the live socket client and the unit
/// tests exercise the *same* code, so there is no second copy of the
/// "send / wait / fold / retransmit / fail over" orchestration (the engine
/// itself owns the timers and failover; this loop only performs the I/O the
/// engine's [`Action`]s ask for). `rng` supplies the CSPRNG draws the engine
/// needs (the query id and retransmit jitter) and is kept distinct from the
/// transport so neither aliases the other.
///
/// # Errors
///
/// A transport [`Errno`] from [`DnsTransport::send`] or
/// [`DnsTransport::wait`]; the resolution is abandoned fail-closed rather
/// than reported as a spurious answer.
pub fn resolve<T: DnsTransport + ?Sized>(
    name: Name,
    record_type: RecordType,
    servers: &[IpAddr],
    transport: &mut T,
    rng: &mut dyn FnMut() -> u32,
) -> Result<Resolution, Errno> {
    let mut resolver = DnsResolver::new(name, record_type, servers);
    let mut query_buf = [0u8; MAX_QUERY_LEN];
    let mut rx = [0u8; MAX_MESSAGE_LEN];

    let now = transport.now();
    // The Idle poll always acts (a first send, or an immediate timeout finish
    // when no server is configured); a `None` here would be an engine
    // contract break, so fail closed to a timeout rather than looping.
    let Some(mut action) = resolver.poll(now, rng) else {
        return Ok(Resolution {
            status: ResolveStatus::Timeout,
            answer: Answer::empty(record_type),
            ttl_secs: 0,
        });
    };

    loop {
        match action {
            Action::Send { query, server } => {
                // MAX_QUERY_LEN always suffices, so encoding cannot fail; a
                // BufferTooSmall would be a construction bug, not runtime
                // input, so surface it fail-closed rather than panic.
                let len = write_query(&query, &mut query_buf).map_err(|_| Errno::OutOfRange)?;
                transport.send(server, &query_buf[..len])?;
            }
            Action::Finished(resolution) => return Ok(resolution),
        }

        // Await a response or the retransmit/failover deadline. Unmatched
        // datagrams (spoofed, stale, or malformed) are dropped by the engine
        // and we keep waiting against the *same* deadline; a fired deadline
        // that yields no action (a spurious early wakeup) also re-waits.
        action = loop {
            // The engine always arms a deadline while waiting; if it somehow
            // did not, treat "no deadline" as due now so we re-poll rather
            // than block forever.
            let deadline = resolver.next_deadline().unwrap_or_else(|| transport.now());
            match transport.wait(deadline, &mut rx)? {
                Wait::Datagram(len) => {
                    let now = transport.now();
                    if let Some(next) = resolver.on_response(now, &rx[..len], rng) {
                        break next;
                    }
                }
                Wait::TimedOut => {
                    let now = transport.now();
                    if let Some(next) = resolver.poll(now, rng) {
                        break next;
                    }
                }
            }
        };
    }
}

#[cfg(test)]
#[path = "dns_tests.rs"]
mod tests;
