//! `abi-v1` wire format for the **journal ingress** IPC endpoint.
//!
//! A user-space process that wants to write an authoritative system-log record
//! (the SYSLOG caller-API model) does not touch a segment file: it frames a
//! [`LogIngressRequest`] and posts it to the well-known
//! [`LOG_INGRESS_ENDPOINT`], which the journal system service binds. The
//! journal attests the caller's identity itself (from the kernel-provided peer
//! origin, never a caller claim), applies stream/source/level policy, and
//! commits the record; the reply is a status word alone (accepted, or the
//! [`Errno`] that refused it).
//!
//! # What a caller controls, and what it does not
//!
//! A request carries only *caller content* — a mandatory message, an optional
//! caller level, an optional subsystem label (honoured only for a trusted
//! kernel emitter), the caller's component/tag/event-id, the stream and source
//! the caller *requests*, and a flat set of typed `data.*` fields. Everything
//! authoritative — the attested origin, the derived source name, the effective
//! stream and level, the append and per-CPU sequences, and every integrity
//! hash — is decided by the journal, never sent here. A request that names a
//! privileged stream or a reserved source is not an error at this layer: it is
//! preserved as a claim and the journal downgrades and flags it.
//!
//! # Layering
//!
//! This module is deliberately unaware of `lib/log`'s `Stream`/`Level` enums
//! (that crate depends on this one, not the reverse): the request carries their
//! discriminants as opaque bytes, which the journal resolves fail-closed. The
//! `data.*` pairs reuse the one shared named-field codec
//! ([`encode_named_field`]/[`decode_named_field`]), so an ingress field and a
//! persisted record field can never drift apart.
//!
//! # Wire layout
//!
//! All scalars are little-endian. A fixed [`LogIngressRequest::HEADER_LEN`]-byte
//! header is followed by the mandatory message, then the flag-selected optional
//! strings in a fixed order, then the `data.*` fields:
//!
//! | Offset | Size | Field |
//! |-------:|-----:|-------|
//! |   0    |  4   | `magic` ([`LOG_INGRESS_REQUEST_MAGIC`]) |
//! |   4    |  2   | `version` ([`crate::ABI_VERSION_CURRENT`]) |
//! |   6    |  2   | `flags` (presence bits; reserved bits zero) |
//! |   8    |  1   | `data_count` (`<= `[`LOG_INGRESS_MAX_DATA_FIELDS`]) |
//! |   9    |  1   | `level` (valid iff `FLAG_LEVEL`; `<= `[`crate::log::LOG_LEVEL_MAX`]) |
//! |  10    |  1   | `stream` (valid iff `FLAG_STREAM`; opaque discriminant) |
//! |  11    |  1   | `reserved` (must be zero) |
//! | ...    | var  | `message`: `u8` len + UTF-8 bytes (always present) |
//! | ...    | var  | optional strings, each `u8` len + UTF-8, in flag order |
//! | ...    | var  | `data_count` named fields (shared codec) |

use crate::field::{decode_named_field, encode_named_field, FieldValue};
use crate::le::{put_i32, put_u16, put_u32, read_i32, read_u16, read_u32};
use crate::log::{LOG_FIELDS_MAX, LOG_FIELD_VALUE_MAX, LOG_LEVEL_MAX, LOG_MESSAGE_MAX};
use crate::{Errno, FIELD_NAME_MAX};

/// Well-known synchronous call-endpoint id the journal service binds and
/// clients name in [`crate::SyscallNumber::IPC_CALL`].
///
/// One OS-wide contract, like [`crate::sysinfo::SYSINFO_ENDPOINT`]: the journal
/// publishes this endpoint at startup (an unrestricted-sender endpoint — any
/// process may write a log record, since authority is decided by the attested
/// origin, not the transport), and every logging client posts its framed
/// request here.
pub const LOG_INGRESS_ENDPOINT: u64 = 0x4C47_1001;

/// Magic word identifying a journal-ingress request (`"LGI1"` little-endian).
pub const LOG_INGRESS_REQUEST_MAGIC: u32 = u32::from_le_bytes(*b"LGI1");

/// Maximum length, in bytes, of `caller.message`. Shared with the diagnostic
/// record model and the persisted record body so a message that fits one
/// channel fits the others.
pub const LOG_INGRESS_MESSAGE_MAX: usize = LOG_MESSAGE_MAX;

/// Maximum length, in bytes, of the trusted-emitter `subsystem` label used to
/// derive a `kernel.<subsystem>` source.
pub const LOG_INGRESS_SUBSYSTEM_MAX: usize = 64;

/// Maximum length, in bytes, of `caller.component`.
pub const LOG_INGRESS_COMPONENT_MAX: usize = 64;

/// Maximum length, in bytes, of `caller.tag`.
pub const LOG_INGRESS_TAG_MAX: usize = 64;

/// Maximum length, in bytes, of `caller.event_id` (a source-local identifier).
pub const LOG_INGRESS_EVENT_ID_MAX: usize = 64;

/// Maximum length, in bytes, of `caller.requested_source`.
pub const LOG_INGRESS_REQUESTED_SOURCE_MAX: usize = 128;

/// Maximum number of `data.*` fields one request may carry. Shared with the
/// diagnostic record model so a record that fits one channel fits the others,
/// exactly as [`LOG_INGRESS_MESSAGE_MAX`] is.
pub const LOG_INGRESS_MAX_DATA_FIELDS: usize = LOG_FIELDS_MAX;

/// Upper bound, in bytes, on a fully populated encoded request: the header,
/// every optional string at its maximum, and the maximum number of `data.*`
/// fields at their maximum key and value sizes.
///
/// One OS-wide contract shared by the journal service (which sizes the
/// [`LOG_INGRESS_ENDPOINT`] per-call request capacity by it) and every client
/// (which frames its request within it), so neither carries a private copy
/// that could drift from the other. A request larger than this cannot be
/// framed and is rejected before it reaches the journal.
pub const LOG_INGRESS_MAX_REQUEST: usize = LogIngressRequest::HEADER_LEN
    + (1 + LOG_INGRESS_MESSAGE_MAX)
    + (1 + LOG_INGRESS_SUBSYSTEM_MAX)
    + (1 + LOG_INGRESS_COMPONENT_MAX)
    + (1 + LOG_INGRESS_TAG_MAX)
    + (1 + LOG_INGRESS_EVENT_ID_MAX)
    + (1 + LOG_INGRESS_REQUESTED_SOURCE_MAX)
    + LOG_INGRESS_MAX_DATA_FIELDS
        * (crate::field::NAMED_FIELD_KEY_PREFIX_LEN + FIELD_NAME_MAX + LOG_FIELD_VALUE_MAX);

// Presence flags packed into the header's `flags` word.
const FLAG_LEVEL: u16 = 1 << 0;
const FLAG_STREAM: u16 = 1 << 1;
const FLAG_SUBSYSTEM: u16 = 1 << 2;
const FLAG_COMPONENT: u16 = 1 << 3;
const FLAG_TAG: u16 = 1 << 4;
const FLAG_EVENT_ID: u16 = 1 << 5;
const FLAG_REQUESTED_SOURCE: u16 = 1 << 6;
const FLAG_KNOWN: u16 = FLAG_LEVEL
    | FLAG_STREAM
    | FLAG_SUBSYSTEM
    | FLAG_COMPONENT
    | FLAG_TAG
    | FLAG_EVENT_ID
    | FLAG_REQUESTED_SOURCE;

/// The caller-supplied inputs to [`encode_request`].
///
/// `level` and `stream` are raw discriminants (the ABI is unaware of the
/// `lib/log` enums); `None` means the caller made no request and the journal
/// applies its default.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct LogIngressFields<'a> {
    /// The caller-labelled severity discriminant, if any.
    pub level: Option<u8>,
    /// The requested-stream discriminant, if any (advisory).
    pub stream: Option<u8>,
    /// The short human-readable message (always present; may be empty).
    pub message: &'a str,
    /// The trusted-emitter subsystem label, if any.
    pub subsystem: Option<&'a str>,
    /// The caller's component/module name, if any.
    pub component: Option<&'a str>,
    /// A short caller-chosen grouping tag, if any.
    pub tag: Option<&'a str>,
    /// A stable source-local event identifier, if any.
    pub event_id: Option<&'a str>,
    /// The source the caller requests (advisory), if any.
    pub requested_source: Option<&'a str>,
}

/// Encode one ingress request into `out`, returning the byte length written.
///
/// Every bound is checked before a byte is written, so a rejected request
/// leaves nothing partially transmitted.
///
/// # Errors
///
/// * [`Errno::OutOfRange`] — `level` exceeds [`LOG_LEVEL_MAX`].
/// * [`Errno::LengthOutOfRange`] — a string, the field count, or an encoded
///   value exceeds its maximum.
/// * [`Errno::BufferTooSmall`] — `out` cannot hold the encoded request.
pub fn encode_request(
    out: &mut [u8],
    fields: &LogIngressFields<'_>,
    data: &[(&str, FieldValue<'_>)],
) -> Result<usize, Errno> {
    if let Some(level) = fields.level {
        if level > LOG_LEVEL_MAX {
            return Err(Errno::OutOfRange);
        }
    }
    if data.len() > LOG_INGRESS_MAX_DATA_FIELDS {
        return Err(Errno::LengthOutOfRange);
    }

    let mut flags = 0u16;
    if fields.level.is_some() {
        flags |= FLAG_LEVEL;
    }
    if fields.stream.is_some() {
        flags |= FLAG_STREAM;
    }
    if fields.subsystem.is_some() {
        flags |= FLAG_SUBSYSTEM;
    }
    if fields.component.is_some() {
        flags |= FLAG_COMPONENT;
    }
    if fields.tag.is_some() {
        flags |= FLAG_TAG;
    }
    if fields.event_id.is_some() {
        flags |= FLAG_EVENT_ID;
    }
    if fields.requested_source.is_some() {
        flags |= FLAG_REQUESTED_SOURCE;
    }

    if out.len() < LogIngressRequest::HEADER_LEN {
        return Err(Errno::BufferTooSmall);
    }
    put_u32(out, 0, LOG_INGRESS_REQUEST_MAGIC);
    put_u16(out, 4, crate::ABI_VERSION_CURRENT_U16);
    put_u16(out, 6, flags);
    // `data.len() <= LOG_INGRESS_MAX_DATA_FIELDS` (32) fits a `u8`.
    #[allow(clippy::cast_possible_truncation)]
    {
        out[8] = data.len() as u8;
    }
    out[9] = fields.level.unwrap_or(0);
    out[10] = fields.stream.unwrap_or(0);
    out[11] = 0;

    let mut pos = LogIngressRequest::HEADER_LEN;
    put_str(out, &mut pos, fields.message, LOG_INGRESS_MESSAGE_MAX)?;
    if let Some(s) = fields.subsystem {
        put_str(out, &mut pos, s, LOG_INGRESS_SUBSYSTEM_MAX)?;
    }
    if let Some(s) = fields.component {
        put_str(out, &mut pos, s, LOG_INGRESS_COMPONENT_MAX)?;
    }
    if let Some(s) = fields.tag {
        put_str(out, &mut pos, s, LOG_INGRESS_TAG_MAX)?;
    }
    if let Some(s) = fields.event_id {
        put_str(out, &mut pos, s, LOG_INGRESS_EVENT_ID_MAX)?;
    }
    if let Some(s) = fields.requested_source {
        put_str(out, &mut pos, s, LOG_INGRESS_REQUESTED_SOURCE_MAX)?;
    }
    for (key, value) in data {
        pos += encode_named_field(
            out.get_mut(pos..).ok_or(Errno::BufferTooSmall)?,
            key,
            value,
            FIELD_NAME_MAX,
            LOG_FIELD_VALUE_MAX,
        )?;
    }
    Ok(pos)
}

/// A validated, borrowed view over an encoded ingress request.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct LogIngressRequest<'a> {
    flags: u16,
    level: u8,
    stream: u8,
    data_count: usize,
    message: &'a str,
    subsystem: Option<&'a str>,
    component: Option<&'a str>,
    tag: Option<&'a str>,
    event_id: Option<&'a str>,
    requested_source: Option<&'a str>,
    /// Exactly the `data_count` field records, no trailing bytes.
    data_bytes: &'a [u8],
}

impl<'a> LogIngressRequest<'a> {
    /// Fixed size, in bytes, of the request header preceding the message.
    pub const HEADER_LEN: usize = 12;

    /// The caller-labelled severity discriminant, if the caller supplied one.
    #[must_use]
    pub const fn level(&self) -> Option<u8> {
        if self.flags & FLAG_LEVEL != 0 {
            Some(self.level)
        } else {
            None
        }
    }

    /// The requested-stream discriminant, if the caller supplied one.
    #[must_use]
    pub const fn stream(&self) -> Option<u8> {
        if self.flags & FLAG_STREAM != 0 {
            Some(self.stream)
        } else {
            None
        }
    }

    /// The mandatory human-readable message.
    #[must_use]
    pub const fn message(&self) -> &'a str {
        self.message
    }

    /// The trusted-emitter subsystem label, if present.
    #[must_use]
    pub const fn subsystem(&self) -> Option<&'a str> {
        self.subsystem
    }

    /// The caller's component name, if present.
    #[must_use]
    pub const fn component(&self) -> Option<&'a str> {
        self.component
    }

    /// The caller's grouping tag, if present.
    #[must_use]
    pub const fn tag(&self) -> Option<&'a str> {
        self.tag
    }

    /// The caller's source-local event identifier, if present.
    #[must_use]
    pub const fn event_id(&self) -> Option<&'a str> {
        self.event_id
    }

    /// The source the caller requested (advisory), if present.
    #[must_use]
    pub const fn requested_source(&self) -> Option<&'a str> {
        self.requested_source
    }

    /// Number of `data.*` fields the request carries.
    #[must_use]
    pub const fn data_count(&self) -> usize {
        self.data_count
    }

    /// Iterate the request's `data.*` `(key, value)` pairs.
    #[must_use]
    pub const fn data(&self) -> LogIngressFieldIter<'a> {
        LogIngressFieldIter {
            bytes: self.data_bytes,
            offset: 0,
        }
    }

    /// Validate and decode an ingress request.
    ///
    /// Every length is range-checked, every slice is confirmed to lie within
    /// `bytes`, and the message, optional strings, and field values are fully
    /// validated before a view is returned. Any inconsistency is rejected
    /// fail-closed; nothing is partially accepted.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `bytes` is shorter than the header or a
    ///   declared string/field runs past its end.
    /// * [`Errno::BadMagic`] — the magic word or reserved bits/byte are wrong.
    /// * [`Errno::AbiVersionUnsupported`] — the version is not current.
    /// * [`Errno::OutOfRange`] — the level exceeds [`LOG_LEVEL_MAX`], or a
    ///   string is not valid UTF-8.
    /// * [`Errno::LengthOutOfRange`] — a declared length exceeds its maximum,
    ///   or the fields do not exactly tile the remaining bytes.
    pub fn from_bytes(bytes: &'a [u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::HEADER_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if read_u32(bytes, 0) != LOG_INGRESS_REQUEST_MAGIC {
            return Err(Errno::BadMagic);
        }
        if u32::from(read_u16(bytes, 4)) != crate::ABI_VERSION_CURRENT {
            return Err(Errno::AbiVersionUnsupported);
        }
        let flags = read_u16(bytes, 6);
        if flags & !FLAG_KNOWN != 0 {
            return Err(Errno::BadMagic);
        }
        let data_count = bytes[8] as usize;
        if data_count > LOG_INGRESS_MAX_DATA_FIELDS {
            return Err(Errno::LengthOutOfRange);
        }
        let level = bytes[9];
        let stream = bytes[10];
        if bytes[11] != 0 {
            return Err(Errno::BadMagic);
        }
        // A level byte that is not carried must be zero, and a carried level
        // must be in range; both fail closed.
        if flags & FLAG_LEVEL == 0 {
            if level != 0 {
                return Err(Errno::BadMagic);
            }
        } else if level > LOG_LEVEL_MAX {
            return Err(Errno::OutOfRange);
        }
        if flags & FLAG_STREAM == 0 && stream != 0 {
            return Err(Errno::BadMagic);
        }

        let mut pos = Self::HEADER_LEN;
        let message = take_str(bytes, &mut pos, LOG_INGRESS_MESSAGE_MAX)?;
        let subsystem = take_opt(
            bytes,
            &mut pos,
            flags,
            FLAG_SUBSYSTEM,
            LOG_INGRESS_SUBSYSTEM_MAX,
        )?;
        let component = take_opt(
            bytes,
            &mut pos,
            flags,
            FLAG_COMPONENT,
            LOG_INGRESS_COMPONENT_MAX,
        )?;
        let tag = take_opt(bytes, &mut pos, flags, FLAG_TAG, LOG_INGRESS_TAG_MAX)?;
        let event_id = take_opt(
            bytes,
            &mut pos,
            flags,
            FLAG_EVENT_ID,
            LOG_INGRESS_EVENT_ID_MAX,
        )?;
        let requested_source = take_opt(
            bytes,
            &mut pos,
            flags,
            FLAG_REQUESTED_SOURCE,
            LOG_INGRESS_REQUESTED_SOURCE_MAX,
        )?;

        let data_bytes = bytes.get(pos..).ok_or(Errno::BufferTooSmall)?;
        let mut off = 0usize;
        for _ in 0..data_count {
            let (_, consumed) =
                decode_named_field(&data_bytes[off..], FIELD_NAME_MAX, LOG_FIELD_VALUE_MAX)?;
            off += consumed;
        }
        if off != data_bytes.len() {
            return Err(Errno::LengthOutOfRange);
        }

        Ok(Self {
            flags,
            level,
            stream,
            data_count,
            message,
            subsystem,
            component,
            tag,
            event_id,
            requested_source,
            data_bytes,
        })
    }
}

/// Iterator over a [`LogIngressRequest`]'s `data.*` `(key, value)` pairs.
#[derive(Clone, Debug)]
pub struct LogIngressFieldIter<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Iterator for LogIngressFieldIter<'a> {
    type Item = (&'a str, FieldValue<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset >= self.bytes.len() {
            return None;
        }
        // `from_bytes` validated every field up front, so this decode cannot
        // fail; stop defensively rather than panic on the (impossible) bad
        // slice.
        let ((key, value), consumed) = decode_named_field(
            self.bytes.get(self.offset..)?,
            FIELD_NAME_MAX,
            LOG_FIELD_VALUE_MAX,
        )
        .ok()?;
        self.offset += consumed;
        Some((key, value))
    }
}

/// Length, in bytes, of the status word every ingress reply carries.
pub const LOG_INGRESS_REPLY_LEN: usize = 4;

/// Frame an ingress reply: `0` when the record was accepted, else the negated
/// [`Errno`] the journal refused it with.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] if `out` is shorter than [`LOG_INGRESS_REPLY_LEN`].
pub fn encode_reply(result: Result<(), Errno>, out: &mut [u8]) -> Result<usize, Errno> {
    if out.len() < LOG_INGRESS_REPLY_LEN {
        return Err(Errno::BufferTooSmall);
    }
    let status = match result {
        Ok(()) => 0,
        // `Errno` discriminants are positive, so `-code` is a distinct negative
        // status; `0` is reserved for success.
        Err(err) => -err.as_i32(),
    };
    put_i32(out, 0, status);
    Ok(LOG_INGRESS_REPLY_LEN)
}

/// Decode an ingress reply's status word into the journal's verdict.
///
/// # Errors
///
/// * [`Errno::BufferTooSmall`] if `bytes` is shorter than the status word.
/// * [`Errno::BadMagic`] if the status is neither `0` nor a known negated
///   [`Errno`].
pub fn decode_reply(bytes: &[u8]) -> Result<(), Errno> {
    if bytes.len() < LOG_INGRESS_REPLY_LEN {
        return Err(Errno::BufferTooSmall);
    }
    match read_i32(bytes, 0) {
        0 => Ok(()),
        neg if neg < 0 => Err(Errno::try_from_status(neg).ok_or(Errno::BadMagic)?),
        _ => Err(Errno::BadMagic),
    }
}

fn put_str(out: &mut [u8], pos: &mut usize, s: &str, max: usize) -> Result<(), Errno> {
    if s.len() > max {
        return Err(Errno::LengthOutOfRange);
    }
    let end = *pos + 1 + s.len();
    if out.len() < end {
        return Err(Errno::BufferTooSmall);
    }
    // `s.len() <= max <= 255` fits the length byte.
    #[allow(clippy::cast_possible_truncation)]
    {
        out[*pos] = s.len() as u8;
    }
    out[*pos + 1..end].copy_from_slice(s.as_bytes());
    *pos = end;
    Ok(())
}

fn take_opt<'a>(
    bytes: &'a [u8],
    pos: &mut usize,
    flags: u16,
    bit: u16,
    max: usize,
) -> Result<Option<&'a str>, Errno> {
    if flags & bit == 0 {
        return Ok(None);
    }
    Ok(Some(take_str(bytes, pos, max)?))
}

fn take_str<'a>(bytes: &'a [u8], pos: &mut usize, max: usize) -> Result<&'a str, Errno> {
    let len_at = *pos;
    if bytes.len() <= len_at {
        return Err(Errno::BufferTooSmall);
    }
    let len = bytes[len_at] as usize;
    if len > max {
        return Err(Errno::LengthOutOfRange);
    }
    let start = len_at + 1;
    let end = start + len;
    if bytes.len() < end {
        return Err(Errno::BufferTooSmall);
    }
    let s = core::str::from_utf8(&bytes[start..end]).map_err(|_| Errno::OutOfRange)?;
    *pos = end;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::{
        decode_reply, encode_reply, encode_request, LogIngressFields, LogIngressRequest,
        LOG_INGRESS_MESSAGE_MAX, LOG_INGRESS_REQUEST_MAGIC,
    };
    use crate::field::FieldValue;
    use crate::log::LOG_LEVEL_MAX;
    use crate::Errno;
    use crate::LOG_INGRESS_REPLY_LEN;

    fn buf() -> [u8; 1024] {
        [0u8; 1024]
    }

    #[test]
    fn minimal_request_round_trips() {
        let mut b = buf();
        let fields = LogIngressFields {
            message: "started",
            ..LogIngressFields::default()
        };
        let n = encode_request(&mut b, &fields, &[]).unwrap();
        let req = LogIngressRequest::from_bytes(&b[..n]).unwrap();
        assert_eq!(req.message(), "started");
        assert_eq!(req.level(), None);
        assert_eq!(req.stream(), None);
        assert_eq!(req.subsystem(), None);
        assert_eq!(req.component(), None);
        assert_eq!(req.data_count(), 0);
        assert_eq!(req.data().count(), 0);
    }

    #[test]
    fn full_request_round_trips_with_fields() {
        let mut b = buf();
        let fields = LogIngressFields {
            level: Some(4),
            stream: Some(3),
            message: "dhcp timeout",
            subsystem: Some("net"),
            component: Some("dhcp"),
            tag: Some("lease"),
            event_id: Some("dhcp.timeout"),
            requested_source: Some("kernel.mem"),
        };
        let data = [
            ("iface", FieldValue::Str("net0")),
            ("count", FieldValue::UnsignedInt(9842)),
        ];
        let n = encode_request(&mut b, &fields, &data).unwrap();
        let req = LogIngressRequest::from_bytes(&b[..n]).unwrap();
        assert_eq!(req.level(), Some(4));
        assert_eq!(req.stream(), Some(3));
        assert_eq!(req.message(), "dhcp timeout");
        assert_eq!(req.subsystem(), Some("net"));
        assert_eq!(req.component(), Some("dhcp"));
        assert_eq!(req.tag(), Some("lease"));
        assert_eq!(req.event_id(), Some("dhcp.timeout"));
        assert_eq!(req.requested_source(), Some("kernel.mem"));
        assert_eq!(req.data_count(), 2);
        let mut it = req.data();
        assert_eq!(it.next(), Some(("iface", FieldValue::Str("net0"))));
        assert_eq!(it.next(), Some(("count", FieldValue::UnsignedInt(9842))));
        assert_eq!(it.next(), None);
    }

    #[test]
    fn encode_rejects_a_level_above_the_maximum() {
        let mut b = buf();
        let fields = LogIngressFields {
            level: Some(LOG_LEVEL_MAX + 1),
            message: "x",
            ..LogIngressFields::default()
        };
        assert_eq!(encode_request(&mut b, &fields, &[]), Err(Errno::OutOfRange));
    }

    #[test]
    fn decode_rejects_bad_magic() {
        let mut b = buf();
        let fields = LogIngressFields {
            message: "m",
            ..LogIngressFields::default()
        };
        let n = encode_request(&mut b, &fields, &[]).unwrap();
        b[0] ^= 0xFF;
        assert_eq!(LogIngressRequest::from_bytes(&b[..n]), Err(Errno::BadMagic));
    }

    #[test]
    fn decode_rejects_a_short_header() {
        assert_eq!(
            LogIngressRequest::from_bytes(&[0u8; 4]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn decode_rejects_trailing_bytes_past_the_fields() {
        let mut b = buf();
        let fields = LogIngressFields {
            message: "m",
            ..LogIngressFields::default()
        };
        let n = encode_request(&mut b, &fields, &[("k", FieldValue::Bool(true))]).unwrap();
        // Append one stray byte and decode the longer slice.
        assert_eq!(
            LogIngressRequest::from_bytes(&b[..=n]),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn decode_rejects_an_over_long_message() {
        let mut b = buf();
        // Hand-build a header claiming a 200-byte message.
        super::put_u32(&mut b, 0, LOG_INGRESS_REQUEST_MAGIC);
        super::put_u16(&mut b, 4, crate::ABI_VERSION_CURRENT_U16);
        b[LogIngressRequest::HEADER_LEN] = u8::try_from(LOG_INGRESS_MESSAGE_MAX + 1).unwrap();
        assert_eq!(
            LogIngressRequest::from_bytes(&b),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn decode_rejects_an_unknown_flag_bit() {
        let mut b = buf();
        let fields = LogIngressFields {
            message: "m",
            ..LogIngressFields::default()
        };
        let n = encode_request(&mut b, &fields, &[]).unwrap();
        super::put_u16(&mut b, 6, 1 << 15);
        assert_eq!(LogIngressRequest::from_bytes(&b[..n]), Err(Errno::BadMagic));
    }

    #[test]
    fn reply_round_trips_ok_and_error() {
        let mut b = [0u8; super::LOG_INGRESS_REPLY_LEN];
        encode_reply(Ok(()), &mut b).unwrap();
        assert_eq!(decode_reply(&b), Ok(()));
        encode_reply(Err(Errno::PermissionDenied), &mut b).unwrap();
        assert_eq!(decode_reply(&b), Err(Errno::PermissionDenied));
    }
    #[test]
    fn the_most_negative_status_word_fails_closed_instead_of_aborting() {
        // The status word comes from a peer, so the decode must be total over
        // every `i32`: negating `i32::MIN` overflows, and the workspace builds
        // every profile with overflow checks and `panic = "abort"`.
        let mut reply = [0u8; LOG_INGRESS_REPLY_LEN];
        reply[..4].copy_from_slice(&i32::MIN.to_le_bytes());
        assert_eq!(decode_reply(&reply), Err(Errno::BadMagic));
    }
}
