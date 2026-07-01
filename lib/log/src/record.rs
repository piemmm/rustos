//! The logical system-log record model.
//!
//! A committed log entry is a two-layer thing. The **physical container**
//! ([`crate::segment`]) already owns the record's stream, append sequence,
//! originating CPU, monotonic time, boot id, and the whole integrity group
//! (per-record and per-segment hashes, the optional seal). This module owns
//! everything else: the *logical record body* that the container carries as an
//! opaque payload — the effective level, the per-CPU sequence, the per-record
//! wall-clock reading, the kernel-attested [`Origin`], the system-derived
//! source name, the caller-supplied content (message, level, component, tag,
//! event id, and the stream/source the caller *requested*), and the flat set
//! of typed `data.*` fields.
//!
//! The body reuses the shared building blocks rather than re-inventing them:
//! [`FieldValue`]/[`FieldName`] for `data.*`, [`Origin`] for the attested
//! principal, [`WallClockReading`] for wall time, [`Stream`]/[`Level`] for the
//! closed enums, and the shared named-field codec
//! ([`rustos_abi::encode_named_field`]) for the `data.*` pairs.
//!
//! Every multi-byte scalar is little-endian. Decoding is fail-closed: every
//! length, discriminant, and UTF-8 constraint is checked, and any deviation is
//! rejected whole rather than guessed at.

use rustos_abi::field::{decode_named_field, encode_named_field};
use rustos_abi::{Errno, FieldName, FieldValue, Origin, WallClockReading, ORIGIN_WIRE_LEN};

use crate::stream::Stream;
use crate::Level;

/// On-disk logical-record body format version. Distinct from the segment
/// container's own format version; both advance independently.
pub const RECORD_FORMAT_VERSION: u16 = 1;

/// Maximum length, in bytes, of the system-derived `source.name`. Source names
/// are dotted (e.g. `kernel.mem`, `service.<id>`), so this is wider than the
/// bare [`FieldName`] grammar allows.
pub const SOURCE_NAME_MAX: usize = 128;

/// Maximum length, in bytes, of `caller.component`.
pub const CALLER_COMPONENT_MAX: usize = 64;

/// Maximum length, in bytes, of `caller.tag`.
pub const CALLER_TAG_MAX: usize = 64;

/// Maximum length, in bytes, of `caller.event_id` (a source-local identifier).
pub const CALLER_EVENT_ID_MAX: usize = 64;

/// Maximum length, in bytes, of `caller.requested_source`.
pub const CALLER_REQUESTED_SOURCE_MAX: usize = 128;

/// Maximum length, in bytes, of `caller.message`. Shared with the diagnostic
/// record model so a message that fits one channel fits the other.
pub const CALLER_MESSAGE_MAX: usize = rustos_abi::LOG_MESSAGE_MAX;

/// Maximum number of `data.*` fields a record may carry.
pub const MAX_DATA_FIELDS: usize = 32;

/// Maximum length, in bytes, of a single `data.*` field's encoded value.
pub const DATA_FIELD_VALUE_MAX: usize = rustos_abi::LOG_FIELD_VALUE_MAX;

// Caller-optional presence flags, packed into one byte.
const FLAG_LEVEL: u8 = 1 << 0;
const FLAG_COMPONENT: u8 = 1 << 1;
const FLAG_TAG: u8 = 1 << 2;
const FLAG_EVENT_ID: u8 = 1 << 3;
const FLAG_REQUESTED_SOURCE: u8 = 1 << 4;
const FLAG_REQUESTED_STREAM: u8 = 1 << 5;

/// The caller-supplied portion of a record: content the emitting task chose,
/// which the journal stores faithfully but never treats as authority.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CallerContent<'a> {
    /// The severity the caller labelled the event, if any. The journal derives
    /// the authoritative `effective_level` separately.
    pub level: Option<Level>,
    /// The caller's component/module name, if any.
    pub component: Option<&'a str>,
    /// A short caller-chosen tag for grouping, if any.
    pub tag: Option<&'a str>,
    /// A stable source-local event identifier, if any.
    pub event_id: Option<&'a str>,
    /// The source the caller *requested* (advisory; the journal derives the
    /// real source name).
    pub requested_source: Option<&'a str>,
    /// The stream the caller *requested* (advisory; the journal assigns the
    /// effective stream).
    pub requested_stream: Option<Stream>,
    /// The short human-readable message. Always present (may be empty).
    pub message: &'a str,
}

/// A logical log-record body, borrowed for the lifetime of an encode call.
///
/// This is the input to [`Self::encode`]. The container-owned fields (stream,
/// sequence, cpu id, monotonic time, boot id, integrity hashes) are *not* here:
/// the segment container supplies them when the encoded body is appended, so
/// they are never stored twice.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct LogRecord<'a> {
    /// The authoritative severity the journal assigned after policy.
    pub effective_level: Level,
    /// Monotonic per-CPU record sequence (gap detection), supplied by ingress.
    pub cpu_seq: u64,
    /// The per-record wall-clock reading and its trust state.
    pub wall: WallClockReading,
    /// The kernel-attested identity of the emitting principal.
    pub origin: Origin,
    /// The system-derived source name (`kernel.mem`, `service.<id>`, …).
    pub source_name: &'a str,
    /// The caller-supplied content.
    pub caller: CallerContent<'a>,
    /// The flat set of typed `data.*` fields.
    pub data: &'a [(FieldName<'a>, FieldValue<'a>)],
}

impl LogRecord<'_> {
    /// Serialise this record body into `out`, returning the byte length
    /// written. Every bound is checked; a record that violates one is rejected
    /// whole (the caller discards `out` on error).
    ///
    /// # Errors
    ///
    /// * [`Errno::LengthOutOfRange`] — a string, the field count, or an encoded
    ///   value exceeds its maximum.
    /// * [`Errno::BufferTooSmall`] — `out` cannot hold the encoded body.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Errno> {
        if self.source_name.len() > SOURCE_NAME_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        if self.caller.message.len() > CALLER_MESSAGE_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        if self.data.len() > MAX_DATA_FIELDS {
            return Err(Errno::LengthOutOfRange);
        }

        let mut pos = 0usize;
        put_u16(out, &mut pos, RECORD_FORMAT_VERSION)?;
        put_u8(out, &mut pos, self.effective_level.as_u8())?;
        put_u64(out, &mut pos, self.cpu_seq)?;
        put_bytes(out, &mut pos, &self.wall.to_le_bytes())?;
        put_bytes(out, &mut pos, &self.origin.to_le_bytes())?;
        put_str8(out, &mut pos, self.source_name, SOURCE_NAME_MAX)?;

        let flags = self.caller_flags();
        put_u8(out, &mut pos, flags)?;
        if let Some(level) = self.caller.level {
            put_u8(out, &mut pos, level.as_u8())?;
        }
        if let Some(stream) = self.caller.requested_stream {
            put_u8(out, &mut pos, stream.as_u8())?;
        }
        if let Some(component) = self.caller.component {
            put_str8(out, &mut pos, component, CALLER_COMPONENT_MAX)?;
        }
        if let Some(tag) = self.caller.tag {
            put_str8(out, &mut pos, tag, CALLER_TAG_MAX)?;
        }
        if let Some(event_id) = self.caller.event_id {
            put_str8(out, &mut pos, event_id, CALLER_EVENT_ID_MAX)?;
        }
        if let Some(requested_source) = self.caller.requested_source {
            put_str8(out, &mut pos, requested_source, CALLER_REQUESTED_SOURCE_MAX)?;
        }
        put_str16(out, &mut pos, self.caller.message)?;

        // `self.data.len() <= MAX_DATA_FIELDS` (32) fits a `u8`.
        put_u8(
            out,
            &mut pos,
            u8::try_from(self.data.len()).map_err(|_| Errno::LengthOutOfRange)?,
        )?;
        for (name, value) in self.data {
            let written = encode_named_field(
                out.get_mut(pos..).ok_or(Errno::BufferTooSmall)?,
                name.as_str(),
                value,
                rustos_abi::FIELD_NAME_MAX,
                DATA_FIELD_VALUE_MAX,
            )?;
            pos += written;
        }
        Ok(pos)
    }

    fn caller_flags(&self) -> u8 {
        let mut flags = 0u8;
        if self.caller.level.is_some() {
            flags |= FLAG_LEVEL;
        }
        if self.caller.component.is_some() {
            flags |= FLAG_COMPONENT;
        }
        if self.caller.tag.is_some() {
            flags |= FLAG_TAG;
        }
        if self.caller.event_id.is_some() {
            flags |= FLAG_EVENT_ID;
        }
        if self.caller.requested_source.is_some() {
            flags |= FLAG_REQUESTED_SOURCE;
        }
        if self.caller.requested_stream.is_some() {
            flags |= FLAG_REQUESTED_STREAM;
        }
        flags
    }
}

/// A validated, borrowed view over an encoded logical record body.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct LogRecordRef<'a> {
    effective_level: Level,
    cpu_seq: u64,
    wall: WallClockReading,
    origin: Origin,
    source_name: &'a str,
    caller: CallerContent<'a>,
    data_bytes: &'a [u8],
    data_count: usize,
}

impl<'a> LogRecordRef<'a> {
    /// The authoritative severity.
    #[must_use]
    pub const fn effective_level(&self) -> Level {
        self.effective_level
    }

    /// The per-CPU record sequence.
    #[must_use]
    pub const fn cpu_seq(&self) -> u64 {
        self.cpu_seq
    }

    /// The per-record wall-clock reading and its trust state.
    #[must_use]
    pub const fn wall(&self) -> WallClockReading {
        self.wall
    }

    /// The kernel-attested origin.
    #[must_use]
    pub const fn origin(&self) -> Origin {
        self.origin
    }

    /// The system-derived source name.
    #[must_use]
    pub const fn source_name(&self) -> &'a str {
        self.source_name
    }

    /// The caller-supplied content.
    #[must_use]
    pub const fn caller(&self) -> CallerContent<'a> {
        self.caller
    }

    /// Number of `data.*` fields the record carries.
    #[must_use]
    pub const fn data_count(&self) -> usize {
        self.data_count
    }

    /// Iterate the record's validated `data.*` `(name, value)` pairs.
    #[must_use]
    pub fn data(&self) -> DataFieldIter<'a> {
        DataFieldIter {
            bytes: self.data_bytes,
            offset: 0,
        }
    }
}

/// Iterator over a [`LogRecordRef`]'s validated `data.*` fields.
#[derive(Clone, Debug)]
pub struct DataFieldIter<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Iterator for DataFieldIter<'a> {
    type Item = (FieldName<'a>, FieldValue<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset >= self.bytes.len() {
            return None;
        }
        // `decode` validated every field up front, so this decode and the
        // name re-validation cannot fail; stop defensively rather than panic.
        let ((key, value), consumed) = decode_named_field(
            self.bytes.get(self.offset..)?,
            rustos_abi::FIELD_NAME_MAX,
            DATA_FIELD_VALUE_MAX,
        )
        .ok()?;
        let name = FieldName::new(key).ok()?;
        self.offset += consumed;
        Some((name, value))
    }
}

/// Validate and decode a logical record body from `bytes`, fail-closed.
///
/// Every length, discriminant, and UTF-8 constraint is checked, and the
/// `data.*` fields must tile the remainder of `bytes` exactly. Any deviation
/// is rejected whole.
///
/// # Errors
///
/// * [`Errno::OutOfRange`] — an unsupported format version, an out-of-range
///   level/stream/flags byte, or a non-UTF-8 string.
/// * [`Errno::LengthOutOfRange`] — a declared length exceeds its maximum or the
///   declared fields do not tile the body exactly.
/// * other [`Errno`] — the wall reading, origin, or a field value fails to
///   decode.
pub fn decode(bytes: &[u8]) -> Result<LogRecordRef<'_>, Errno> {
    let mut pos = 0usize;
    if read_u16(bytes, &mut pos)? != RECORD_FORMAT_VERSION {
        return Err(Errno::OutOfRange);
    }
    let effective_level = Level::from_u8(read_u8(bytes, &mut pos)?).ok_or(Errno::OutOfRange)?;
    let cpu_seq = read_u64(bytes, &mut pos)?;
    let wall = WallClockReading::from_bytes(take(bytes, &mut pos, WallClockReading::WIRE_LEN)?)?;
    let origin = Origin::from_bytes(take(bytes, &mut pos, ORIGIN_WIRE_LEN)?)?;
    let source_name = read_str8(bytes, &mut pos, SOURCE_NAME_MAX)?;

    let flags = read_u8(bytes, &mut pos)?;
    if flags & !ALL_CALLER_FLAGS != 0 {
        return Err(Errno::OutOfRange);
    }
    let level = if flags & FLAG_LEVEL != 0 {
        Some(Level::from_u8(read_u8(bytes, &mut pos)?).ok_or(Errno::OutOfRange)?)
    } else {
        None
    };
    let requested_stream = if flags & FLAG_REQUESTED_STREAM != 0 {
        Some(Stream::from_u8(read_u8(bytes, &mut pos)?)?)
    } else {
        None
    };
    let component = read_opt_str8(
        bytes,
        &mut pos,
        flags & FLAG_COMPONENT != 0,
        CALLER_COMPONENT_MAX,
    )?;
    let tag = read_opt_str8(bytes, &mut pos, flags & FLAG_TAG != 0, CALLER_TAG_MAX)?;
    let event_id = read_opt_str8(
        bytes,
        &mut pos,
        flags & FLAG_EVENT_ID != 0,
        CALLER_EVENT_ID_MAX,
    )?;
    let requested_source = read_opt_str8(
        bytes,
        &mut pos,
        flags & FLAG_REQUESTED_SOURCE != 0,
        CALLER_REQUESTED_SOURCE_MAX,
    )?;
    let message = read_str16(bytes, &mut pos, CALLER_MESSAGE_MAX)?;

    let data_count = read_u8(bytes, &mut pos)? as usize;
    if data_count > MAX_DATA_FIELDS {
        return Err(Errno::LengthOutOfRange);
    }
    let data_bytes = bytes.get(pos..).ok_or(Errno::LengthOutOfRange)?;
    let mut off = 0usize;
    for _ in 0..data_count {
        let ((key, _), consumed) = decode_named_field(
            data_bytes.get(off..).ok_or(Errno::LengthOutOfRange)?,
            rustos_abi::FIELD_NAME_MAX,
            DATA_FIELD_VALUE_MAX,
        )?;
        // A `data.*` key must obey the caller field-name grammar.
        FieldName::new(key)?;
        off += consumed;
    }
    // No trailing bytes past the declared fields.
    if off != data_bytes.len() {
        return Err(Errno::LengthOutOfRange);
    }

    Ok(LogRecordRef {
        effective_level,
        cpu_seq,
        wall,
        origin,
        source_name,
        caller: CallerContent {
            level,
            component,
            tag,
            event_id,
            requested_source,
            requested_stream,
            message,
        },
        data_bytes,
        data_count,
    })
}

// The union of every defined caller-presence flag; any other bit is rejected.
const ALL_CALLER_FLAGS: u8 = FLAG_LEVEL
    | FLAG_COMPONENT
    | FLAG_TAG
    | FLAG_EVENT_ID
    | FLAG_REQUESTED_SOURCE
    | FLAG_REQUESTED_STREAM;

// --- Little-endian body cursor helpers (fail-closed, no allocation). ---

fn put_bytes(out: &mut [u8], pos: &mut usize, src: &[u8]) -> Result<(), Errno> {
    let end = pos.checked_add(src.len()).ok_or(Errno::BufferTooSmall)?;
    if end > out.len() {
        return Err(Errno::BufferTooSmall);
    }
    out[*pos..end].copy_from_slice(src);
    *pos = end;
    Ok(())
}

fn put_u8(out: &mut [u8], pos: &mut usize, v: u8) -> Result<(), Errno> {
    put_bytes(out, pos, &[v])
}

fn put_u16(out: &mut [u8], pos: &mut usize, v: u16) -> Result<(), Errno> {
    put_bytes(out, pos, &v.to_le_bytes())
}

fn put_u64(out: &mut [u8], pos: &mut usize, v: u64) -> Result<(), Errno> {
    put_bytes(out, pos, &v.to_le_bytes())
}

// A `u8`-length-prefixed string, bounded by `max` (which must be `<= 255`).
fn put_str8(out: &mut [u8], pos: &mut usize, s: &str, max: usize) -> Result<(), Errno> {
    if s.len() > max {
        return Err(Errno::LengthOutOfRange);
    }
    put_u8(
        out,
        pos,
        u8::try_from(s.len()).map_err(|_| Errno::LengthOutOfRange)?,
    )?;
    put_bytes(out, pos, s.as_bytes())
}

// A `u16`-length-prefixed string, bounded by [`CALLER_MESSAGE_MAX`].
fn put_str16(out: &mut [u8], pos: &mut usize, s: &str) -> Result<(), Errno> {
    if s.len() > CALLER_MESSAGE_MAX {
        return Err(Errno::LengthOutOfRange);
    }
    put_u16(
        out,
        pos,
        u16::try_from(s.len()).map_err(|_| Errno::LengthOutOfRange)?,
    )?;
    put_bytes(out, pos, s.as_bytes())
}

fn take<'a>(bytes: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8], Errno> {
    let end = pos.checked_add(n).ok_or(Errno::LengthOutOfRange)?;
    let slice = bytes.get(*pos..end).ok_or(Errno::LengthOutOfRange)?;
    *pos = end;
    Ok(slice)
}

fn read_u8(bytes: &[u8], pos: &mut usize) -> Result<u8, Errno> {
    Ok(take(bytes, pos, 1)?[0])
}

fn read_u16(bytes: &[u8], pos: &mut usize) -> Result<u16, Errno> {
    let s = take(bytes, pos, 2)?;
    Ok(u16::from_le_bytes([s[0], s[1]]))
}

fn read_u64(bytes: &[u8], pos: &mut usize) -> Result<u64, Errno> {
    let s = take(bytes, pos, 8)?;
    let mut a = [0u8; 8];
    a.copy_from_slice(s);
    Ok(u64::from_le_bytes(a))
}

fn read_str8<'a>(bytes: &'a [u8], pos: &mut usize, max: usize) -> Result<&'a str, Errno> {
    let len = read_u8(bytes, pos)? as usize;
    if len > max {
        return Err(Errno::LengthOutOfRange);
    }
    core::str::from_utf8(take(bytes, pos, len)?).map_err(|_| Errno::OutOfRange)
}

fn read_str16<'a>(bytes: &'a [u8], pos: &mut usize, max: usize) -> Result<&'a str, Errno> {
    let len = read_u16(bytes, pos)? as usize;
    if len > max {
        return Err(Errno::LengthOutOfRange);
    }
    core::str::from_utf8(take(bytes, pos, len)?).map_err(|_| Errno::OutOfRange)
}

fn read_opt_str8<'a>(
    bytes: &'a [u8],
    pos: &mut usize,
    present: bool,
    max: usize,
) -> Result<Option<&'a str>, Errno> {
    if present {
        Ok(Some(read_str8(bytes, pos, max)?))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        decode, CallerContent, LogRecord, ALL_CALLER_FLAGS, CALLER_MESSAGE_MAX, MAX_DATA_FIELDS,
        SOURCE_NAME_MAX,
    };
    use crate::stream::Stream;
    use crate::Level;
    use rustos_abi::{
        CapabilitySummary, FieldName, FieldValue, Origin, ProcId, TrustDomain, WallClockReading,
        WallTimeState,
    };

    // A generous fixed scratch buffer; the largest record we build fits.
    const BUF: usize = 4096;

    fn sample_origin() -> Origin {
        Origin::new(
            TrustDomain::User,
            1000,
            1000,
            42,
            ProcId::from_raw([7u8; 16]),
            CapabilitySummary::from_raw([0u8; 32]),
        )
    }

    fn sample_wall() -> WallClockReading {
        WallClockReading::new(
            rustos_abi::Time64::new(1_700_000_000, 250).expect("valid time"),
            WallTimeState::Trusted,
        )
    }

    #[test]
    fn full_record_round_trips() {
        let data = [
            (FieldName::new("iface").unwrap(), FieldValue::Str("net0")),
            (
                FieldName::new("elapsed").unwrap(),
                FieldValue::Duration(rustos_abi::Duration64::from_secs(10)),
            ),
            (
                FieldName::new("attempt").unwrap(),
                FieldValue::UnsignedInt(3),
            ),
        ];
        let record = LogRecord {
            effective_level: Level::Warn,
            cpu_seq: 12345,
            wall: sample_wall(),
            origin: sample_origin(),
            source_name: "service.dhcp",
            caller: CallerContent {
                level: Some(Level::Critical),
                component: Some("dhcp"),
                tag: Some("lease"),
                event_id: Some("dhcp.timeout"),
                requested_source: Some("dhcp"),
                requested_stream: Some(Stream::Runtime),
                message: "dhcp timeout",
            },
            data: &data,
        };
        let mut buf = [0u8; BUF];
        let len = record.encode(&mut buf).expect("encodes");
        let view = decode(&buf[..len]).expect("decodes");

        assert_eq!(view.effective_level(), Level::Warn);
        assert_eq!(view.cpu_seq(), 12345);
        assert_eq!(view.wall(), sample_wall());
        assert_eq!(view.origin(), sample_origin());
        assert_eq!(view.source_name(), "service.dhcp");
        let caller = view.caller();
        assert_eq!(caller.level, Some(Level::Critical));
        assert_eq!(caller.component, Some("dhcp"));
        assert_eq!(caller.tag, Some("lease"));
        assert_eq!(caller.event_id, Some("dhcp.timeout"));
        assert_eq!(caller.requested_source, Some("dhcp"));
        assert_eq!(caller.requested_stream, Some(Stream::Runtime));
        assert_eq!(caller.message, "dhcp timeout");

        assert_eq!(view.data_count(), 3);
        let mut it = view.data();
        let (n0, v0) = it.next().unwrap();
        assert_eq!(n0.as_str(), "iface");
        assert_eq!(v0, FieldValue::Str("net0"));
        let (n1, _v1) = it.next().unwrap();
        assert_eq!(n1.as_str(), "elapsed");
        let (n2, v2) = it.next().unwrap();
        assert_eq!(n2.as_str(), "attempt");
        assert_eq!(v2, FieldValue::UnsignedInt(3));
        assert!(it.next().is_none());
    }

    #[test]
    fn minimal_record_round_trips() {
        let record = LogRecord {
            effective_level: Level::Info,
            cpu_seq: 0,
            wall: WallClockReading::default(),
            origin: sample_origin(),
            source_name: "kernel.core",
            caller: CallerContent {
                level: None,
                component: None,
                tag: None,
                event_id: None,
                requested_source: None,
                requested_stream: None,
                message: "started",
            },
            data: &[],
        };
        let mut buf = [0u8; BUF];
        let len = record.encode(&mut buf).expect("encodes");
        let view = decode(&buf[..len]).expect("decodes");
        assert_eq!(view.caller().message, "started");
        assert_eq!(view.caller().level, None);
        assert_eq!(view.caller().requested_stream, None);
        assert_eq!(view.data_count(), 0);
        assert!(view.data().next().is_none());
        assert_eq!(view.wall().state(), WallTimeState::Unset);
    }

    fn base_record() -> LogRecord<'static> {
        LogRecord {
            effective_level: Level::Info,
            cpu_seq: 1,
            wall: WallClockReading::default(),
            origin: sample_origin(),
            source_name: "kernel.core",
            caller: CallerContent {
                level: None,
                component: None,
                tag: None,
                event_id: None,
                requested_source: None,
                requested_stream: None,
                message: "hi",
            },
            data: &[],
        }
    }

    #[test]
    fn over_long_source_is_rejected() {
        let long = [b'x'; SOURCE_NAME_MAX + 1];
        let s = core::str::from_utf8(&long).unwrap();
        let mut record = base_record();
        record.source_name = s;
        let mut buf = [0u8; BUF];
        assert!(record.encode(&mut buf).is_err());
    }

    #[test]
    fn over_long_message_is_rejected() {
        let long = [b'm'; CALLER_MESSAGE_MAX + 1];
        let s = core::str::from_utf8(&long).unwrap();
        let mut record = base_record();
        record.caller.message = s;
        let mut buf = [0u8; BUF];
        assert!(record.encode(&mut buf).is_err());
    }

    #[test]
    fn too_many_data_fields_are_rejected() {
        let name = FieldName::new("k").unwrap();
        let big: [(FieldName<'_>, FieldValue<'_>); MAX_DATA_FIELDS + 1] =
            [(name, FieldValue::Null); MAX_DATA_FIELDS + 1];
        let mut record = base_record();
        record.data = &big;
        let mut buf = [0u8; BUF];
        assert!(record.encode(&mut buf).is_err());
    }

    #[test]
    fn encode_into_short_buffer_fails_closed() {
        let record = base_record();
        let mut small = [0u8; 4];
        assert!(record.encode(&mut small).is_err());
    }

    #[test]
    fn decode_rejects_a_bad_format_version() {
        let record = base_record();
        let mut buf = [0u8; BUF];
        let len = record.encode(&mut buf).unwrap();
        buf[0] = 0xFF; // corrupt the version's low byte
        assert!(decode(&buf[..len]).is_err());
    }

    #[test]
    fn decode_rejects_unknown_caller_flags() {
        let record = base_record();
        let mut buf = [0u8; BUF];
        let len = record.encode(&mut buf).unwrap();
        // The flags byte sits right after version(2) + level(1) + cpu_seq(8) +
        // wall + origin + source(1 len + "kernel.core").
        let flags_off = 2
            + 1
            + 8
            + WallClockReading::WIRE_LEN
            + rustos_abi::ORIGIN_WIRE_LEN
            + 1
            + "kernel.core".len();
        assert_eq!(buf[flags_off], 0, "base record has no caller flags set");
        buf[flags_off] = ALL_CALLER_FLAGS | 0x80; // a bit outside the defined set
        assert!(decode(&buf[..len]).is_err());
    }

    #[test]
    fn decode_rejects_trailing_bytes() {
        let record = base_record();
        let mut buf = [0u8; BUF];
        let len = record.encode(&mut buf).unwrap();
        // One extra byte past the declared (zero) data fields must not tile.
        assert!(decode(&buf[..=len]).is_err());
    }

    #[test]
    fn decode_rejects_truncated_body() {
        let record = base_record();
        let mut buf = [0u8; BUF];
        let len = record.encode(&mut buf).unwrap();
        for cut in 0..len {
            assert!(
                decode(&buf[..cut]).is_err(),
                "a truncated body must be rejected (cut = {cut})"
            );
        }
    }
}
