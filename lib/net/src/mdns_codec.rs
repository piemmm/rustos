//! The multicast DNS message codec (RFC 6762 §18).
//!
//! The octet layout is RFC 1035's, so the name reader, the integer readers,
//! and [`RecordType`] all come from [`crate::dns`]. What this module adds is
//! the multicast reading of the two class top bits, the record types service
//! discovery needs, and a writer that builds a message into a caller-owned
//! buffer with no allocation.
//!
//! # Reading is two-phase, and the phases fail differently
//!
//! [`Message::parse`] walks the whole datagram once and refuses it outright
//! on any *structural* fault — a truncated header, a name that runs off the
//! end or points forward, an rdata length past the message, more questions
//! or records than the fixed bounds admit. Nothing is surfaced from a
//! message that fails that walk.
//!
//! Within a message that passes, a record whose rdata does not match its own
//! type is **skipped**, exactly as a record of a type this engine has no
//! decoder for is: the protocol is extensible, so a reader that rejected
//! every message containing something it did not understand would be
//! unusable on a real segment. It is never guessed at.

use tairix_inline::ArrayVec;

use crate::addr::{Ipv4Addr, Ipv6Addr};
use crate::dns::{read_u16, read_u32, Name, RecordType, HEADER_LEN, MAX_NAME_LEN};

use super::{
    QuestionType, RData, Record, Service, TxtRecord, TypeBitmap, CLASS_IN, CLASS_TOP_BIT,
    MAX_MESSAGE_QUESTIONS, MAX_MESSAGE_RECORDS,
};

/// Header flag bits (RFC 1035 §4.1.1) in the multicast reading RFC 6762 §18
/// gives them.
const FLAG_QR: u16 = 0x8000;
const FLAG_OPCODE_MASK: u16 = 0x7800;
const FLAG_AA: u16 = 0x0400;
const FLAG_TC: u16 = 0x0200;
const FLAG_RCODE_MASK: u16 = 0x000F;

/// Compression-pointer offsets are 14 bits (RFC 1035 §4.1.4), so a name
/// starting past this cannot be pointed at.
const MAX_POINTER_OFFSET: usize = 0x3FFF;

/// Names the writer remembers as compression targets.
///
/// A size trade with no security content: forgetting a candidate costs
/// octets on the wire, never correctness.
const MAX_COMPRESSION_ENTRIES: usize = 48;

/// Which section of a message a record was found in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Section {
    /// An answer, or — in a query — a known answer the sender already holds
    /// (RFC 6762 §7.1).
    Answer,
    /// In a probe, the records the sender proposes to claim (RFC 6762 §8.1).
    Authority,
    /// Records the sender volunteered alongside the answer.
    Additional,
}

/// One question: a name, what is asked about it, and whether the asker wants
/// the reply unicast.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Question {
    /// The name asked about.
    pub name: Name,
    /// What is asked about it.
    pub qtype: QuestionType,
    /// The RFC 6762 §5.4 `QU` bit: reply directly to me, not to the group.
    pub unicast_response: bool,
}

impl Question {
    /// A question asking for a multicast reply.
    #[must_use]
    pub const fn new(name: Name, qtype: QuestionType) -> Self {
        Self {
            name,
            qtype,
            unicast_response: false,
        }
    }

    /// Whether `record` answers this question.
    ///
    /// The type is compared first because it is one 16-bit value where the
    /// name is up to 255 octets, and most pairs differ in the type.
    #[must_use]
    pub fn answered_by(&self, record: &Record) -> bool {
        self.qtype.matches(record.record_type()) && self.name == record.name
    }
}

/// A structurally validated message, read in place from the datagram.
///
/// Holds no records: the sections are walked lazily so a message costs the
/// reader nothing but the walk, and a record materialises only while it is
/// being acted on.
#[derive(Clone, Copy, Debug)]
pub struct Message<'a> {
    bytes: &'a [u8],
    /// The transaction id, which a legacy unicast reply must echo (RFC 6762
    /// §6.7) and which is otherwise zero.
    pub id: u16,
    /// Whether this is a response rather than a query.
    pub response: bool,
    /// The authoritative-answer bit, which every mDNS response sets.
    pub authoritative: bool,
    /// The truncation bit, which on a *query* means more known answers
    /// follow in a further message (RFC 6762 §7.2).
    pub truncated: bool,
    question_count: u16,
    questions_at: usize,
    records_at: usize,
    answer_count: u16,
    authority_count: u16,
    additional_count: u16,
}

impl<'a> Message<'a> {
    /// Validate `bytes` as one multicast DNS message.
    ///
    /// Returns `None` — dropping the datagram whole — for a short or
    /// malformed header, a non-zero opcode or rcode (RFC 6762 §18.3, §18.11
    /// require those to be ignored, and a receiver that acted on one would
    /// be answering a protocol it does not speak), a name or rdata that runs
    /// off the end of the message, a compression pointer that does not point
    /// strictly backwards, or section counts past the fixed bounds.
    #[must_use]
    pub fn parse(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < HEADER_LEN {
            return None;
        }
        let id = read_u16(bytes, 0)?;
        let flags = read_u16(bytes, 2)?;
        if flags & FLAG_OPCODE_MASK != 0 || flags & FLAG_RCODE_MASK != 0 {
            return None;
        }
        let question_count = read_u16(bytes, 4)?;
        let answer_count = read_u16(bytes, 6)?;
        let authority_count = read_u16(bytes, 8)?;
        let additional_count = read_u16(bytes, 10)?;
        let records = usize::from(answer_count)
            .checked_add(usize::from(authority_count))?
            .checked_add(usize::from(additional_count))?;
        if usize::from(question_count) > MAX_MESSAGE_QUESTIONS || records > MAX_MESSAGE_RECORDS {
            return None;
        }

        // The structural walk: every name expands, every rdata span lies
        // inside the message. A fault here rejects the datagram whole.
        let questions_at = HEADER_LEN;
        let mut pos = questions_at;
        for _ in 0..question_count {
            let (_, after) = Name::read(bytes, pos)?;
            pos = after.checked_add(4)?;
            if pos > bytes.len() {
                return None;
            }
        }
        let records_at = pos;
        for _ in 0..records {
            pos = skip_record(bytes, pos)?;
        }

        Some(Self {
            bytes,
            id,
            response: flags & FLAG_QR != 0,
            authoritative: flags & FLAG_AA != 0,
            truncated: flags & FLAG_TC != 0,
            question_count,
            questions_at,
            records_at,
            answer_count,
            authority_count,
            additional_count,
        })
    }

    /// The questions, in wire order.
    #[must_use]
    pub fn questions(&self) -> Questions<'a> {
        Questions {
            bytes: self.bytes,
            pos: self.questions_at,
            left: self.question_count,
        }
    }

    /// The records of every section, in wire order, tagged with the section
    /// they came from.
    ///
    /// A record of a type this engine has no decoder for, or one whose rdata
    /// does not match its type, is skipped.
    #[must_use]
    pub fn records(&self) -> Records<'a> {
        Records {
            bytes: self.bytes,
            pos: self.records_at,
            left: [
                self.answer_count,
                self.authority_count,
                self.additional_count,
            ],
        }
    }

    /// Whether the message carries no question and no record — a datagram
    /// that asks and asserts nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.question_count == 0
            && self.answer_count == 0
            && self.authority_count == 0
            && self.additional_count == 0
    }
}

/// Advance past one resource record, validating that it lies inside the
/// message. Returns the offset just past it.
fn skip_record(bytes: &[u8], pos: usize) -> Option<usize> {
    let (_, after_name) = Name::read(bytes, pos)?;
    let rdlength = usize::from(read_u16(bytes, after_name.checked_add(8)?)?);
    let rdata_at = after_name.checked_add(10)?;
    let end = rdata_at.checked_add(rdlength)?;
    if end > bytes.len() {
        return None;
    }
    Some(end)
}

/// The questions of a [`Message`].
#[derive(Clone, Debug)]
pub struct Questions<'a> {
    bytes: &'a [u8],
    pos: usize,
    left: u16,
}

impl Iterator for Questions<'_> {
    type Item = Question;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.left == 0 {
                return None;
            }
            self.left -= 1;
            // The structural walk in `Message::parse` proved these reads.
            let (name, after) = Name::read(self.bytes, self.pos)?;
            let qtype = read_u16(self.bytes, after)?;
            let qclass = read_u16(self.bytes, after + 2)?;
            self.pos = after + 4;
            let unicast_response = qclass & CLASS_TOP_BIT != 0;
            if qclass & !CLASS_TOP_BIT != CLASS_IN {
                continue;
            }
            let Some(qtype) = QuestionType::from_value(qtype) else {
                continue;
            };
            return Some(Question {
                name,
                qtype,
                unicast_response,
            });
        }
    }
}

impl core::iter::FusedIterator for Questions<'_> {}

/// The records of a [`Message`], tagged with their section.
#[derive(Clone, Debug)]
pub struct Records<'a> {
    bytes: &'a [u8],
    pos: usize,
    /// Records left in the answer, authority, and additional sections.
    left: [u16; 3],
}

impl Iterator for Records<'_> {
    type Item = (Section, Record);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let section = match self.left {
                [0, 0, 0] => return None,
                [0, 0, _] => Section::Additional,
                [0, _, _] => Section::Authority,
                _ => Section::Answer,
            };
            let index = match section {
                Section::Answer => 0,
                Section::Authority => 1,
                Section::Additional => 2,
            };
            self.left[index] -= 1;

            // Proved in place by `Message::parse`'s structural walk.
            let (name, after_name) = Name::read(self.bytes, self.pos)?;
            let rtype = read_u16(self.bytes, after_name)?;
            let class = read_u16(self.bytes, after_name + 2)?;
            let ttl = read_u32(self.bytes, after_name + 4)?;
            let rdlength = usize::from(read_u16(self.bytes, after_name + 8)?);
            let rdata_at = after_name + 10;
            self.pos = rdata_at + rdlength;

            if class & !CLASS_TOP_BIT != CLASS_IN {
                continue;
            }
            let Some(record_type) = RecordType::from_value(rtype) else {
                continue;
            };
            let Some(data) = read_rdata(record_type, self.bytes, rdata_at, rdlength, &name) else {
                continue;
            };
            return Some((
                section,
                Record {
                    name,
                    data,
                    ttl,
                    cache_flush: class & CLASS_TOP_BIT != 0,
                },
            ));
        }
    }
}

impl core::iter::FusedIterator for Records<'_> {}

/// Decode one record's rdata, or `None` when it does not match its type.
fn read_rdata(
    record_type: RecordType,
    msg: &[u8],
    at: usize,
    len: usize,
    owner: &Name,
) -> Option<RData> {
    let rdata = msg.get(at..at.checked_add(len)?)?;
    match record_type {
        RecordType::A => {
            let octets: [u8; 4] = rdata.try_into().ok()?;
            Some(RData::A(Ipv4Addr::from(octets)))
        }
        RecordType::Aaaa => {
            let octets: [u8; 16] = rdata.try_into().ok()?;
            Some(RData::Aaaa(Ipv6Addr::from(octets)))
        }
        RecordType::Ptr => {
            // RFC 1035 rdata may hold a compression pointer, so it is read
            // against the whole message and must span exactly the rdata.
            let (target, end) = Name::read(msg, at)?;
            (end == at + len).then_some(RData::Ptr(target))
        }
        RecordType::Srv => {
            let priority = read_u16(msg, at)?;
            let weight = read_u16(msg, at + 2)?;
            let port = read_u16(msg, at + 4)?;
            let (target, end) = Name::read(msg, at + 6)?;
            (end == at + len).then_some(RData::Srv(Service {
                priority,
                weight,
                port,
                target,
            }))
        }
        RecordType::Txt => TxtRecord::new(rdata).ok().map(RData::Txt),
        RecordType::Nsec => {
            // RFC 6762 §6.1 fixes the next-domain field to the owner name; a
            // record that says otherwise is asserting absence on a name it
            // does not own, so it is dropped rather than believed.
            let (next, end) = Name::read(msg, at)?;
            if next != *owner {
                return None;
            }
            read_type_bitmap(msg.get(end..at + len)?).map(RData::Nsec)
        }
    }
}

/// Decode an RFC 4034 §4.1.2 type bitmap, keeping window 0.
///
/// Higher windows are structurally validated and then ignored: they assert
/// absence only of types above 255, and nothing service discovery asks for
/// lives there.
fn read_type_bitmap(mut rest: &[u8]) -> Option<TypeBitmap> {
    let mut bitmap = TypeBitmap::new();
    while !rest.is_empty() {
        let window = *rest.first()?;
        let len = usize::from(*rest.get(1)?);
        if len == 0 || len > 32 {
            return None;
        }
        let block = rest.get(2..2 + len)?;
        if window == 0 {
            for (index, &byte) in block.iter().enumerate() {
                for bit in 0..8u16 {
                    if byte & (0x80 >> bit) != 0 {
                        // Window 0 holds types 0..=255, so the index fits.
                        let ty = u16::try_from(index)
                            .ok()?
                            .checked_mul(8)?
                            .checked_add(bit)?;
                        bitmap.insert_bit(ty);
                    }
                }
            }
        }
        rest = rest.get(2 + len..)?;
    }
    Some(bitmap)
}

/// Builds one message into a caller-owned buffer.
///
/// Sections are written in wire order and the writer refuses a push that
/// would go backwards, so a half-built message cannot claim a layout it does
/// not have. Every push reports whether the record fitted; a caller that
/// runs out of room sets the truncation bit and continues in a second
/// message, which is how a long known-answer list travels (RFC 6762 §7.2).
#[derive(Debug)]
pub struct MessageWriter<'a> {
    out: &'a mut [u8],
    len: usize,
    counts: [u16; 4],
    names: ArrayVec<(u16, u32), MAX_COMPRESSION_ENTRIES>,
}

impl<'a> MessageWriter<'a> {
    /// Start a message in `out`, which must hold at least a header.
    ///
    /// `id` is zero for everything but a legacy unicast reply, which echoes
    /// the query's (RFC 6762 §6.7).
    #[must_use]
    pub fn new(out: &'a mut [u8], id: u16, response: bool) -> Option<Self> {
        if out.len() < HEADER_LEN {
            return None;
        }
        out[0..2].copy_from_slice(&id.to_be_bytes());
        // Every mDNS response is authoritative (RFC 6762 §18.4); a query
        // carries no flags but its own.
        let flags = if response { FLAG_QR | FLAG_AA } else { 0 };
        out[2..4].copy_from_slice(&flags.to_be_bytes());
        out[4..HEADER_LEN].fill(0);
        Some(Self {
            out,
            len: HEADER_LEN,
            counts: [0; 4],
            names: ArrayVec::new(),
        })
    }

    /// Append a question. Questions precede every record.
    pub fn push_question(&mut self, question: &Question) -> bool {
        if self.counts[1..].iter().any(|count| *count != 0) {
            return false;
        }
        let start = self.len;
        let class = CLASS_IN
            | if question.unicast_response {
                CLASS_TOP_BIT
            } else {
                0
            };
        let ok = self.write_name(&question.name, true)
            && self.write_u16(question.qtype.value())
            && self.write_u16(class);
        if ok {
            self.counts[0] += 1;
            true
        } else {
            self.rewind(start);
            false
        }
    }

    /// Append a record to `section`, which may not precede the section the
    /// writer is already in.
    pub fn push_record(&mut self, section: Section, record: &Record) -> bool {
        let index = match section {
            Section::Answer => 1,
            Section::Authority => 2,
            Section::Additional => 3,
        };
        if self.counts[index + 1..].iter().any(|count| *count != 0) {
            return false;
        }
        let start = self.len;
        let class = CLASS_IN | if record.cache_flush { CLASS_TOP_BIT } else { 0 };
        let header_ok = self.write_name(&record.name, true)
            && self.write_u16(record.record_type().value())
            && self.write_u16(class)
            && self.write_u32(record.ttl)
            && self.write_u16(0);
        if !header_ok {
            self.rewind(start);
            return false;
        }
        let rdlength_at = self.len - 2;
        if !self.write_rdata(&record.data, &record.name) {
            self.rewind(start);
            return false;
        }
        let Ok(rdlength) = u16::try_from(self.len - rdlength_at - 2) else {
            self.rewind(start);
            return false;
        };
        self.out[rdlength_at..rdlength_at + 2].copy_from_slice(&rdlength.to_be_bytes());
        self.counts[index] += 1;
        true
    }

    /// Set the truncation bit, which on a query promises a further message
    /// of known answers (RFC 6762 §7.2).
    pub fn set_truncated(&mut self) {
        let flags = u16::from_be_bytes([self.out[2], self.out[3]]) | FLAG_TC;
        self.out[2..4].copy_from_slice(&flags.to_be_bytes());
    }

    /// Whether anything has been appended.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.counts.iter().all(|count| *count == 0)
    }

    /// Write the section counts and return the message length.
    #[must_use]
    pub fn finish(self) -> usize {
        for (index, count) in self.counts.iter().enumerate() {
            let at = 4 + index * 2;
            self.out[at..at + 2].copy_from_slice(&count.to_be_bytes());
        }
        self.len
    }

    /// Undo a partial record, leaving the buffer exactly as it was.
    fn rewind(&mut self, to: usize) {
        self.names.retain(|(offset, _)| usize::from(*offset) < to);
        self.len = to;
    }

    fn write_u16(&mut self, value: u16) -> bool {
        self.write(&value.to_be_bytes())
    }

    fn write_u32(&mut self, value: u32) -> bool {
        self.write(&value.to_be_bytes())
    }

    fn write(&mut self, bytes: &[u8]) -> bool {
        let Some(slot) = self.out.get_mut(self.len..self.len + bytes.len()) else {
            return false;
        };
        slot.copy_from_slice(bytes);
        self.len += bytes.len();
        true
    }

    /// Write `name`, reusing an earlier appearance where `compress` allows.
    ///
    /// RFC 3597 §4 forbids compressing a name inside the rdata of a type
    /// defined after RFC 1035, so `SRV` targets and the `NSEC` next-domain
    /// field are always written in full; a `PTR` target and every owner name
    /// may point at an earlier copy.
    fn write_name(&mut self, name: &Name, compress: bool) -> bool {
        let wire = name.as_wire();
        let start = self.len;
        if compress {
            // Longest suffix first: the deepest match saves the most.
            let mut offset = 0usize;
            while offset < wire.len() {
                if wire[offset] == 0 {
                    break;
                }
                if let Some(target) = self.lookup(&wire[offset..]) {
                    if !self.write(&wire[..offset]) {
                        self.rewind(start);
                        return false;
                    }
                    let pointer = 0xC000u16 | target;
                    if !self.write_u16(pointer) {
                        self.rewind(start);
                        return false;
                    }
                    self.remember(start, wire, offset);
                    return true;
                }
                offset += 1 + usize::from(wire[offset]);
            }
        }
        if !self.write(wire) {
            self.rewind(start);
            return false;
        }
        self.remember(start, wire, wire.len());
        true
    }

    /// An earlier offset whose expansion equals the name `suffix` spells, or
    /// `None`.
    ///
    /// The stored hash filters first, so the exact comparison — which has to
    /// expand the candidate out of the buffer — runs at most once per real
    /// match.
    fn lookup(&self, suffix: &[u8]) -> Option<u16> {
        let wanted = hash_name(suffix);
        if !self.names.iter().any(|(_, hash)| *hash == wanted) {
            return None;
        }
        let candidate = Name::read(suffix, 0)?.0;
        self.names.iter().find_map(|&(offset, hash)| {
            if hash != wanted {
                return None;
            }
            let (expanded, _) = Name::read(&self.out[..self.len], usize::from(offset))?;
            (expanded == candidate).then_some(offset)
        })
    }

    /// Record the label boundaries of the name just written that a later
    /// name may point at: those inside the literal part, whose bytes are in
    /// the buffer, up to `literal_len` octets of `wire`.
    fn remember(&mut self, start: usize, wire: &[u8], literal_len: usize) {
        let mut offset = 0usize;
        while offset < literal_len && wire[offset] != 0 {
            let at = start + offset;
            if at > MAX_POINTER_OFFSET {
                return;
            }
            let Ok(at) = u16::try_from(at) else {
                return;
            };
            if self
                .names
                .try_push((at, hash_name(&wire[offset..])))
                .is_err()
            {
                return;
            }
            offset += 1 + usize::from(wire[offset]);
        }
    }

    fn write_rdata(&mut self, data: &RData, owner: &Name) -> bool {
        match data {
            RData::A(addr) => self.write(&addr.octets()),
            RData::Aaaa(addr) => self.write(&addr.octets()),
            RData::Ptr(target) => self.write_name(target, true),
            RData::Srv(service) => {
                self.write_u16(service.priority)
                    && self.write_u16(service.weight)
                    && self.write_u16(service.port)
                    && self.write_name(&service.target, false)
            }
            RData::Txt(txt) => self.write(txt.as_octets()),
            RData::Nsec(bitmap) => self.write_nsec(bitmap, owner),
        }
    }

    /// An mDNS `NSEC` repeats its owner as the next-domain field (RFC 6762
    /// §6.1), written uncompressed per RFC 3597 §4.
    fn write_nsec(&mut self, bitmap: &TypeBitmap, owner: &Name) -> bool {
        if !self.write_name(owner, false) {
            return false;
        }
        let mut block = [0u8; 32];
        let mut highest = 0usize;
        for bit in bitmap.bits() {
            let index = usize::from(bit / 8);
            let Some(slot) = block.get_mut(index) else {
                continue;
            };
            *slot |= 0x80 >> (bit % 8);
            highest = highest.max(index);
        }
        if bitmap.is_empty() {
            return true;
        }
        self.write(&[0, u8::try_from(highest + 1).unwrap_or(32)]) && self.write(&block[..=highest])
    }
}

/// A case-folded hash of a name's wire octets, used only to skip
/// non-matching compression candidates before the exact comparison.
fn hash_name(wire: &[u8]) -> u32 {
    const OFFSET: u32 = 0x811C_9DC5;
    const PRIME: u32 = 0x0100_0193;
    let mut hash = OFFSET;
    for &byte in wire.iter().take(MAX_NAME_LEN) {
        hash ^= u32::from(byte.to_ascii_lowercase());
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(test)]
#[path = "mdns_codec_tests.rs"]
mod tests;
