//! Multicast DNS (RFC 6762) and the records DNS-based service discovery
//! carries over it (RFC 6763), `plans/ZEROCONF.md` Z1.
//!
//! mDNS is DNS on the wire with link-local semantics, so this engine reuses
//! [`crate::dns`]'s [`Name`] and [`RecordType`] rather than defining a second
//! codec. What differs is everything above the octets: there is no server,
//! every host both asks and answers, the top bit of a question's class is a
//! request for a unicast reply, the top bit of a record's class means "flush
//! what you had", and a name is claimed by probing for it rather than
//! delegated.
//!
//! # Shape
//!
//! One [`MdnsEngine`] per interface, pure in the tradition of the rest of
//! this crate: it owns no socket, no clock, and no randomness. The caller
//! feeds it received datagrams with the address they came from, feeds it
//! monotonic `now` values, supplies CSPRNG draws for the jitter the protocol
//! requires, and transmits the datagrams the engine writes into a buffer it
//! provides. [`MdnsEngine::next_deadline`] folds every timer the protocol
//! needs into the one instant the caller arms a one-shot for, so a quiet
//! segment costs nothing and there is no polling loop.
//!
//! # Security
//!
//! Every byte here arrives unsolicited from an unauthenticated peer that
//! chooses its own arrival rate, so:
//!
//! - The codec is total, allocation-free, and fail-closed: a malformed
//!   message is dropped whole and nothing partial is cached.
//! - Per-packet cost is bounded and indexed. The cache is keyed under a
//!   per-boot secret, so a peer cannot choose a set of names that all land in
//!   one bucket and turn every arriving record into a scan of the table.
//! - The cache bounds ([`MAX_RECORDS`], [`MAX_RECORDS_PER_SOURCE`]) are fixed
//!   security bounds, not capacities that grow with the segment: total
//!   resident state is the per-interface ceiling times the interface count,
//!   independent of how many hosts are shouting.
//! - A sender the stack does not find on-link ([`Sender::on_link`]) is never
//!   answered and never cached. Reflected mDNS is a documented amplifier.
//! - Renaming after a name conflict is bounded. A peer that keeps claiming
//!   our name walks a stock implementation to `host-47.local`; here the
//!   budget runs out and the interface fails closed to *not published*.

use tairix_inline::ArrayVec;

use crate::addr::{Ipv4Addr, Ipv6Addr};
use crate::dns::{DnsError, Name, RecordType, MAX_NAME_LEN};

#[path = "mdns_codec.rs"]
pub mod codec;

#[path = "mdns_cache.rs"]
pub mod cache;

#[path = "mdns_engine.rs"]
mod engine;

pub use cache::{CachedRecord, Learned, RecordCache};
pub use codec::{Message, MessageWriter, Question, Section};
pub use engine::{
    Destination, Emit, MdnsEngine, MdnsEvent, PublishError, PublishId, QuestionId, Sender,
    ServiceState,
};

/// The UDP port every multicast DNS message is sent from and to (RFC 6762
/// §5.1).
pub const PORT: u16 = 5353;

/// The IPv4 link-local multicast group mDNS runs on (RFC 6762 §3).
pub const GROUP_V4: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);

/// The IPv6 link-local multicast group mDNS runs on (RFC 6762 §3).
pub const GROUP_V6: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0x00fb);

/// The parent domain every multicast name lives under (RFC 6762 §3).
pub const LOCAL_LABEL: &[u8] = b"local";

/// The largest `TXT` record this engine will hold or emit, in rdata octets.
///
/// A fixed validation bound, not a capacity: RFC 6763 §6.1 asks that a
/// service's whole `TXT` stay under 400 octets so a response fits one
/// classic 512-octet message, and this sits just above that. A larger record
/// is refused rather than cached, because the cache's resident size must be
/// a figure a small machine can afford whatever the segment sends.
pub const MAX_TXT_LEN: usize = 512;

/// The largest number of records one interface's cache holds.
///
/// A fixed security bound. Eviction is least-recently-used within it, and
/// [`MAX_RECORDS_PER_SOURCE`] keeps one shouting peer from spending more
/// than its share of it.
pub const MAX_RECORDS: usize = 512;

/// The largest number of cached records attributable to any one source
/// address. A peer past it evicts its **own** oldest record, never a
/// neighbour's.
pub const MAX_RECORDS_PER_SOURCE: usize = 32;

/// The largest number of records one interface may publish.
pub const MAX_PUBLISHED: usize = 64;

/// The largest number of continuous questions one interface may ask.
pub const MAX_QUESTIONS: usize = 16;

/// The largest number of records one built message may carry in a section.
pub const MAX_SECTION_RECORDS: usize = 64;

/// The largest number of questions one received message may carry.
///
/// A message with more is truncated to this many rather than rejected: the
/// questions past the bound are simply not answered, which costs a peer
/// nothing it could not have achieved by sending two messages, and bounds
/// the work one datagram can ask for.
pub const MAX_MESSAGE_QUESTIONS: usize = 32;

/// The largest number of records this engine reads from one received
/// message, across all four sections.
pub const MAX_MESSAGE_RECORDS: usize = 128;

/// The renames a conflicting name is walked through before the publication
/// fails closed to withdrawn.
///
/// A fixed bound, and the reason it exists: RFC 6762 §9 renaming has no
/// natural end, so a peer that keeps claiming our name can otherwise walk us
/// through the integers forever while the user watches their host name
/// change.
pub const MAX_RENAMES: u8 = 8;

/// The TTL of a record naming a host (RFC 6762 §10): short, because a host
/// can move between links between one query and the next.
pub const TTL_HOST_SECS: u32 = 120;

/// The TTL of every other record (RFC 6762 §10): 75 minutes.
pub const TTL_OTHER_SECS: u32 = 4500;

/// The TTL a withdrawal carries (RFC 6762 §10.1): expire this now.
pub const TTL_GOODBYE_SECS: u32 = 0;

/// The `IN` class value (RFC 1035 §3.2.4), the only class mDNS uses.
pub(crate) const CLASS_IN: u16 = 1;

/// `QCLASS`/`CLASS` top bit: on a question it asks for a unicast reply (RFC
/// 6762 §5.4), on a record it means the receiver should flush what it held
/// for that name and type (RFC 6762 §10.2).
pub(crate) const CLASS_TOP_BIT: u16 = 0x8000;

/// The `ANY` query type (RFC 1035 §3.2.3), which a probe asks with.
pub(crate) const QTYPE_ANY: u16 = 255;

/// What a question asks for: one record type, or every type at the name.
///
/// A probe (RFC 6762 §8.1) asks `ANY`, which is a query type and never a
/// record type — keeping them apart is what stops an `ANY` reaching the
/// cache as though it were a record someone published.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum QuestionType {
    /// One record type.
    Record(RecordType),
    /// Every type held at the owner name.
    Any,
}

impl QuestionType {
    /// The wire `QTYPE` value.
    #[must_use]
    pub const fn value(self) -> u16 {
        match self {
            Self::Record(record) => record.value(),
            Self::Any => QTYPE_ANY,
        }
    }

    /// The question type a wire `QTYPE` names, or `None` for one this engine
    /// has no decoder for (a question it leaves unanswered rather than
    /// guesses at).
    #[must_use]
    pub const fn from_value(value: u16) -> Option<Self> {
        if value == QTYPE_ANY {
            Some(Self::Any)
        } else {
            match RecordType::from_value(value) {
                Some(record) => Some(Self::Record(record)),
                None => None,
            }
        }
    }

    /// Whether a record of `record_type` answers this question.
    #[must_use]
    pub const fn matches(self, record_type: RecordType) -> bool {
        match self {
            Self::Any => true,
            Self::Record(wanted) => wanted.value() == record_type.value(),
        }
    }
}

/// The `SRV` rdata (RFC 2782): where a service instance is actually reached.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Service {
    /// Lower is preferred among targets of one name.
    pub priority: u16,
    /// Relative share among equal-priority targets.
    pub weight: u16,
    /// The TCP or UDP port.
    pub port: u16,
    /// The host name the service runs on.
    pub target: Name,
}

/// A `TXT` record's rdata: a sequence of length-prefixed strings (RFC 1035
/// §3.3.14), which RFC 6763 §6 reads as a service's key/value attributes.
///
/// Held as the validated wire octets rather than a parsed map, because the
/// key/value grammar is the service-discovery layer's reading of them and
/// re-encoding a parse would not round-trip an attribute this engine does
/// not understand.
#[derive(Clone, Copy)]
pub struct TxtRecord {
    octets: [u8; MAX_TXT_LEN],
    len: u16,
}

impl TxtRecord {
    /// The empty attribute set, which RFC 6763 §6.1 spells as one
    /// zero-length string rather than as no octets at all.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            octets: [0u8; MAX_TXT_LEN],
            len: 1,
        }
    }

    /// Validate `octets` as a run of length-prefixed strings that exactly
    /// spans the slice, and hold them.
    ///
    /// An empty slice is normalised to [`Self::empty`]: RFC 6763 §6.1 says
    /// an mDNS `TXT` with no attributes carries a single zero-length
    /// string, and a zero-length rdata is seen in the field meaning the
    /// same thing. Normalising here leaves one representation, so the
    /// codec round trip is a fixed point and two equal records compare
    /// equal.
    ///
    /// # Errors
    ///
    /// [`TxtError::TooLong`] past [`MAX_TXT_LEN`], [`TxtError::Malformed`]
    /// when a length prefix runs past the end of the slice.
    pub fn new(octets: &[u8]) -> Result<Self, TxtError> {
        if octets.len() > MAX_TXT_LEN {
            return Err(TxtError::TooLong);
        }
        if octets.is_empty() {
            return Ok(Self::empty());
        }
        let mut pos = 0usize;
        while pos < octets.len() {
            let len = usize::from(octets[pos]);
            pos = pos
                .checked_add(1 + len)
                .filter(|end| *end <= octets.len())
                .ok_or(TxtError::Malformed)?;
        }
        let mut held = [0u8; MAX_TXT_LEN];
        held[..octets.len()].copy_from_slice(octets);
        // The length fits: `octets.len()` is bounded by MAX_TXT_LEN above.
        let len = u16::try_from(octets.len()).map_err(|_| TxtError::TooLong)?;
        Ok(Self { octets: held, len })
    }

    /// The validated rdata octets.
    #[must_use]
    pub fn as_octets(&self) -> &[u8] {
        &self.octets[..usize::from(self.len)]
    }

    /// The length-prefixed strings, in order.
    #[must_use]
    pub fn strings(&self) -> TxtStrings<'_> {
        TxtStrings {
            octets: self.as_octets(),
            pos: 0,
        }
    }
}

/// Compares the held octets only, so two `TXT` records are equal exactly
/// when their rdata is.
impl PartialEq for TxtRecord {
    fn eq(&self, other: &Self) -> bool {
        self.as_octets() == other.as_octets()
    }
}

impl Eq for TxtRecord {}

/// Prints the octet count rather than the octets: a `TXT` record is
/// peer-authored text and a debug line is not a display-safe surface.
impl core::fmt::Debug for TxtRecord {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "TxtRecord({} octets)", self.len)
    }
}

/// The length-prefixed strings of a [`TxtRecord`].
#[derive(Clone, Debug)]
pub struct TxtStrings<'a> {
    octets: &'a [u8],
    pos: usize,
}

impl<'a> TxtStrings<'a> {
    /// Walk a run of length-prefixed strings that is not (yet) a whole
    /// record — the rdata a DNS-SD builder reads back mid-build. Total on
    /// any slice, so it carries no invariant [`TxtRecord::new`] has not
    /// already established.
    pub(crate) fn over(octets: &'a [u8]) -> Self {
        Self { octets, pos: 0 }
    }
}

impl<'a> Iterator for TxtStrings<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        let len = usize::from(*self.octets.get(self.pos)?);
        let string = self.octets.get(self.pos + 1..self.pos + 1 + len)?;
        self.pos += 1 + len;
        Some(string)
    }
}

impl core::iter::FusedIterator for TxtStrings<'_> {}

/// Why a [`TxtRecord`] was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TxtError {
    /// Past [`MAX_TXT_LEN`].
    TooLong,
    /// A length prefix ran past the end of the rdata.
    Malformed,
}

/// The types an owner name *does* have, which an mDNS `NSEC` asserts by
/// omission (RFC 6762 §6.1).
///
/// Only the first type-bitmap window is represented, because every type
/// service discovery uses is below 256 and RFC 6762 defines the record's
/// multicast reading over exactly that range. A higher window on the wire is
/// ignored rather than rejected: it asserts nothing about the types this
/// engine asks for.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TypeBitmap {
    types: tairix_inline::BitSet256,
}

impl TypeBitmap {
    /// The empty bitmap.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            types: tairix_inline::BitSet256::new(),
        }
    }

    /// Assert that the owner name holds `record_type`.
    pub fn insert(&mut self, record_type: RecordType) {
        self.types.insert(record_type.value());
    }

    /// Whether the owner name is asserted to hold `record_type`.
    #[must_use]
    pub fn contains(&self, record_type: RecordType) -> bool {
        self.types.contains(record_type.value())
    }

    /// Whether the bitmap asserts nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.types.is_empty()
    }

    /// The raw window-0 bit indices set, ascending.
    pub(crate) fn bits(&self) -> impl Iterator<Item = u16> + '_ {
        self.types.iter()
    }

    /// Set the raw window-0 bit `bit`; used by the decoder, which must hold
    /// an assertion about a type it has no decoder for so the bitmap round
    /// trips.
    pub(crate) fn insert_bit(&mut self, bit: u16) {
        self.types.insert(bit);
    }
}

/// The type-specific half of a resource record.
///
/// The type is carried by the variant rather than beside it, so a record
/// whose type and data disagree cannot be built.
//
// The `Txt` variant is far larger than the rest, and deliberately so: its
// payload is held inline because the receive path must not allocate per
// arriving record, and a fixed footprint is what makes the cache's resident
// size a figure a small machine can predict whatever the segment sends.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RData {
    /// An IPv4 host address.
    A(Ipv4Addr),
    /// An IPv6 host address.
    Aaaa(Ipv6Addr),
    /// A name this name points at: a service type's instances, or an
    /// address's host name.
    Ptr(Name),
    /// Where a service instance is reached.
    Srv(Service),
    /// A service instance's attributes.
    Txt(TxtRecord),
    /// The types the owner name holds, asserting the absence of the rest.
    Nsec(TypeBitmap),
}

impl RData {
    /// The record type this data is.
    #[must_use]
    pub const fn record_type(&self) -> RecordType {
        match self {
            Self::A(_) => RecordType::A,
            Self::Aaaa(_) => RecordType::Aaaa,
            Self::Ptr(_) => RecordType::Ptr,
            Self::Srv(_) => RecordType::Srv,
            Self::Txt(_) => RecordType::Txt,
            Self::Nsec(_) => RecordType::Nsec,
        }
    }

    /// Whether this record names a host and therefore takes the short TTL
    /// (RFC 6762 §10).
    #[must_use]
    pub const fn names_a_host(&self) -> bool {
        matches!(self, Self::A(_) | Self::Aaaa(_) | Self::Srv(_))
    }

    /// The default TTL for a record carrying this data (RFC 6762 §10).
    #[must_use]
    pub const fn default_ttl(&self) -> u32 {
        if self.names_a_host() {
            TTL_HOST_SECS
        } else {
            TTL_OTHER_SECS
        }
    }
}

/// One resource record: an owner name, its data, and how long it is good
/// for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Record {
    /// The name the record is held at.
    pub name: Name,
    /// The type-specific data.
    pub data: RData,
    /// Seconds the record stays valid; zero withdraws it (RFC 6762 §10.1).
    pub ttl: u32,
    /// The RFC 6762 §10.2 cache-flush bit: the sender is the unique owner of
    /// this name and type, and a receiver replaces rather than adds.
    pub cache_flush: bool,
}

impl Record {
    /// A record carrying the default TTL for its type, without the
    /// cache-flush bit — the shape a *shared* record (RFC 6762 §2) takes.
    #[must_use]
    pub const fn shared(name: Name, data: RData) -> Self {
        Self {
            name,
            data,
            ttl: data.default_ttl(),
            cache_flush: false,
        }
    }

    /// A record whose name and type this host claims sole ownership of, so
    /// it carries the cache-flush bit and must be probed for before it is
    /// announced (RFC 6762 §8.1).
    #[must_use]
    pub const fn unique(name: Name, data: RData) -> Self {
        Self {
            name,
            data,
            ttl: data.default_ttl(),
            cache_flush: true,
        }
    }

    /// The record's type.
    #[must_use]
    pub const fn record_type(&self) -> RecordType {
        self.data.record_type()
    }

    /// Whether this and `other` are the same record ignoring TTL and the
    /// cache-flush bit — the identity a cache entry has and known-answer
    /// suppression compares on.
    #[must_use]
    pub fn same_record(&self, other: &Self) -> bool {
        self.name == other.name && self.data == other.data
    }
}

/// Rewrite `name`'s first label to the next candidate after a conflict (RFC
/// 6762 §9).
///
/// A host name takes `-2`, `-3`, …; a service instance name takes ` (2)`,
/// ` (3)`, … — the two conventions the field settled on, so a renamed
/// TAIRiX host looks like every other host on the segment. An existing
/// suffix is *incremented* rather than appended to, so a name conflicting
/// repeatedly does not grow a tail of them.
///
/// # Errors
///
/// [`DnsError::InvalidLabel`] for a name with no labels to rewrite,
/// [`DnsError::NameTooLong`] when the rewritten name would exceed
/// [`MAX_NAME_LEN`].
pub fn rename(name: &Name, kind: NameKind) -> Result<Name, DnsError> {
    let mut labels = name.labels();
    let first = labels.next().ok_or(DnsError::InvalidLabel)?;
    let (stem, ordinal) = split_ordinal(first, kind);
    let next = ordinal.saturating_add(1).max(2);
    let (open, close): (&[u8], &[u8]) = match kind {
        NameKind::Host => (b"-", b""),
        NameKind::Instance => (b" (", b")"),
    };

    let mut digits = [0u8; 3];
    let digits = decimal(next, &mut digits);
    let mut rewritten = [0u8; MAX_NAME_LEN];
    let mut len = 0usize;
    for part in [stem, open, digits, close] {
        let end = len.checked_add(part.len()).ok_or(DnsError::NameTooLong)?;
        rewritten
            .get_mut(len..end)
            .ok_or(DnsError::NameTooLong)?
            .copy_from_slice(part);
        len = end;
    }

    let mut all: ArrayVec<&[u8], MAX_RENAME_LABELS> = ArrayVec::new();
    all.try_push(&rewritten[..len])
        .map_err(|_| DnsError::NameTooLong)?;
    for label in labels {
        all.try_push(label).map_err(|_| DnsError::NameTooLong)?;
    }
    Name::from_labels(all.as_slice())
}

/// The labels a renamed name may hold. A DNS-SD instance name is four
/// (`instance`, `_type`, `_proto`, `local`); the headroom is for a deeper
/// domain, and a name past it is refused rather than truncated.
const MAX_RENAME_LABELS: usize = 16;

/// Which renaming convention a conflicting name follows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NameKind {
    /// A host name: `printer.local` → `printer-2.local`.
    Host,
    /// A DNS-SD service instance name: `Hall Printer._ipp._tcp.local` →
    /// `Hall Printer (2)._ipp._tcp.local`.
    Instance,
}

/// Split a label into its stem and the ordinal a previous rename appended,
/// or zero when it carries none.
fn split_ordinal(label: &[u8], kind: NameKind) -> (&[u8], u16) {
    let (open, close): (&[u8], &[u8]) = match kind {
        NameKind::Host => (b"-", b""),
        NameKind::Instance => (b" (", b")"),
    };
    let Some(body) = label.strip_suffix(close) else {
        return (label, 0);
    };
    // Scan back over the digits, then require the opening delimiter.
    let digits_start = body
        .iter()
        .rposition(|b| !b.is_ascii_digit())
        .map_or(0, |i| i + 1);
    let digits = &body[digits_start..];
    if digits.is_empty() || digits.len() > 3 {
        return (label, 0);
    }
    let Some(stem) = body[..digits_start].strip_suffix(open) else {
        return (label, 0);
    };
    if stem.is_empty() {
        return (label, 0);
    }
    let mut value = 0u16;
    for &digit in digits {
        value = value
            .saturating_mul(10)
            .saturating_add(u16::from(digit - b'0'));
    }
    (stem, value)
}

/// Render `value` (at most three digits) into `buf`, returning the digits.
fn decimal(value: u16, buf: &mut [u8; 3]) -> &[u8] {
    let value = value.min(999);
    let mut len = 0usize;
    if value >= 100 {
        buf[len] = b'0' + u8::try_from(value / 100).unwrap_or(0);
        len += 1;
    }
    if value >= 10 {
        buf[len] = b'0' + u8::try_from((value / 10) % 10).unwrap_or(0);
        len += 1;
    }
    buf[len] = b'0' + u8::try_from(value % 10).unwrap_or(0);
    len += 1;
    &buf[..len]
}

#[cfg(test)]
#[path = "mdns_tests.rs"]
mod tests;
