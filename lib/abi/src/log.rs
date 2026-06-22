//! `abi-v1` wire format for the `log_emit` syscall — a user-space service's
//! structured diagnostic record handed to the kernel's **diagnostic** log
//! sink (`AGENTS.md` §9, §19.4, §20).
//!
//! # Why this exists
//!
//! The kernel's `lib/log` facility owns the one sink that knows how to route
//! a diagnostic record to the right place — the serial UART on a debug
//! build, the video console on a release build (`kernel/arch/*` `SerialSink`
//! / `ConsoleWriter`). That sink is kernel-side, so a user-space service
//! (the device manager, login, …) cannot reach it directly. Before this
//! format a service had to render its `rustos_log::Event`s to its own
//! `stderr` (fd 2), which on a framebuffer-console board lands on the screen
//! and never on the captured serial line. `log_emit` closes that gap: a
//! holder of [`CapabilityId::LOG_EMIT`](crate::CapabilityId::LOG_EMIT)
//! serialises one record into this format and traps into the kernel, which
//! validates it, attributes it to the caller, and emits it through the same
//! diagnostic sink the kernel itself uses.
//!
//! # Security (`AGENTS.md` §5.4, §19.4)
//!
//! This is a **diagnostic** channel, never the hash-chained security audit
//! log: `log_emit` reaches the kernel's `log_sink` only, so user space can
//! never write, forge, or truncate an audit entry (the audit log stays
//! kernel-only). The syscall is capability-gated
//! ([`CapabilityId::LOG_EMIT`](crate::CapabilityId::LOG_EMIT), held only by
//! trusted system services), every field is bounded and validated, and the
//! kernel — not the caller — attributes each record to the calling task, so
//! a record cannot impersonate another principal. A malformed record is
//! rejected fail-closed (`AGENTS.md` §2.9); nothing is partially applied.
//!
//! # Wire layout
//!
//! All scalars are little-endian (`AGENTS.md` — every Tier-1 target is
//! little-endian; the explicit encoding lets a future big-endian port
//! participate). A record is a fixed [`LOG_RECORD_HEADER_LEN`]-byte header
//! followed by the message bytes and then the field records:
//!
//! | Offset | Size | Field |
//! |-------:|-----:|-------|
//! |   0    |  1   | `level` (`0..=`[`LOG_LEVEL_MAX`], the `rustos_log::Level` discriminant) |
//! |   1    |  1   | `field_count` (`<= `[`LOG_FIELDS_MAX`]) |
//! |   2    |  2   | `message_len` (`<= `[`LOG_MESSAGE_MAX`]) |
//! |   4    |  4   | `event_id` (the `rustos_log::EventId` value) |
//! |   8    | `message_len` | `message`, UTF-8 |
//! |  ...   | per field | `key_len` (`u8`), `value_len` (`u8`), `key`, `value`, both UTF-8 |
//!
//! Each field record is `2 + key_len + value_len` bytes; `key_len <= `
//! [`LOG_FIELD_KEY_MAX`] and `value_len <= `[`LOG_FIELD_VALUE_MAX`].

use crate::le::{put_u16, put_u32, read_u16, read_u32};
use crate::Errno;

/// Highest valid `level` byte — the `rustos_log::Level::Error`
/// discriminant. The five levels `Trace`/`Debug`/`Info`/`Warn`/`Error`
/// occupy `0..=4`; this mirrors the `abi-v1` numeric values frozen in
/// `lib/log` (the kernel maps the byte back with `rustos_log::Level::from_u8`,
/// so the enum is defined in exactly one place, `AGENTS.md` §2.2).
pub const LOG_LEVEL_MAX: u8 = 4;

/// Maximum length, in bytes, of a record's message.
///
/// Matches the `lib/log` convention that a message stays within one terminal
/// line, and bounds the kernel's copy-in so a hostile length cannot force a
/// large allocation (`AGENTS.md` §4 / §24.4 — a security bound, fixed, not a
/// growable capacity).
pub const LOG_MESSAGE_MAX: usize = 120;

/// Maximum number of structured key/value fields a record may carry.
pub const LOG_FIELDS_MAX: usize = 8;

/// Maximum length, in bytes, of a single field key.
pub const LOG_FIELD_KEY_MAX: usize = 32;

/// Maximum length, in bytes, of a single field value.
pub const LOG_FIELD_VALUE_MAX: usize = 96;

/// Fixed size, in bytes, of the record header that precedes the message.
pub const LOG_RECORD_HEADER_LEN: usize = 8;

/// Per-field fixed prefix: a `key_len` byte and a `value_len` byte.
const LOG_FIELD_PREFIX_LEN: usize = 2;

/// Upper bound, in bytes, on a fully populated encoded record.
///
/// The kernel copies at most this many bytes from the caller before
/// decoding, so a `len` argument larger than this is rejected without
/// touching the caller's buffer further (`AGENTS.md` §4 / §5.4).
pub const LOG_RECORD_MAX: usize = LOG_RECORD_HEADER_LEN
    + LOG_MESSAGE_MAX
    + LOG_FIELDS_MAX * (LOG_FIELD_PREFIX_LEN + LOG_FIELD_KEY_MAX + LOG_FIELD_VALUE_MAX);

/// Byte length of an encoded record carrying a `message_len`-byte message
/// and `fields_total` bytes of field records.
///
/// Both inputs are bounded by their per-field maxima before this is called,
/// so the sum cannot overflow `usize` on any Tier-1 target.
const fn encoded_len(message_len: usize, fields_total: usize) -> usize {
    LOG_RECORD_HEADER_LEN + message_len + fields_total
}

/// Serialise one diagnostic record into `buf`, returning the byte length
/// written.
///
/// `level` is a `rustos_log::Level` discriminant (`0..=`[`LOG_LEVEL_MAX`]),
/// `event_id` a `rustos_log::EventId` value, `message` the human-readable
/// body, and `fields` the structured key/value pairs. Every bound is checked
/// before a byte is written, so a rejected record leaves `buf` untouched up
/// to the point of failure and is never partially transmitted (the caller
/// drops it, `AGENTS.md` §2.9).
///
/// # Errors
///
/// * [`Errno::OutOfRange`] — `level` exceeds [`LOG_LEVEL_MAX`].
/// * [`Errno::LengthOutOfRange`] — `message`, the field count, or any field
///   key/value exceeds its maximum.
/// * [`Errno::BufferTooSmall`] — `buf` cannot hold the encoded record.
// Every length cast below is bounded by a check earlier in this function
// (`fields.len() <= LOG_FIELDS_MAX`, `message.len() <= LOG_MESSAGE_MAX`, each
// key/value `<= LOG_FIELD_*_MAX`), so each cast is lossless by construction.
#[allow(clippy::cast_possible_truncation)]
pub fn encode_record(
    buf: &mut [u8],
    level: u8,
    event_id: u32,
    message: &str,
    fields: &[(&str, &str)],
) -> Result<usize, Errno> {
    if level > LOG_LEVEL_MAX {
        return Err(Errno::OutOfRange);
    }
    if message.len() > LOG_MESSAGE_MAX {
        return Err(Errno::LengthOutOfRange);
    }
    if fields.len() > LOG_FIELDS_MAX {
        return Err(Errno::LengthOutOfRange);
    }
    let mut fields_total = 0usize;
    for (key, value) in fields {
        if key.len() > LOG_FIELD_KEY_MAX || value.len() > LOG_FIELD_VALUE_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        fields_total += LOG_FIELD_PREFIX_LEN + key.len() + value.len();
    }

    let total = encoded_len(message.len(), fields_total);
    if buf.len() < total {
        return Err(Errno::BufferTooSmall);
    }

    buf[0] = level;
    // `fields.len() <= LOG_FIELDS_MAX` (8) fits a `u8`.
    buf[1] = fields.len() as u8;
    // `message.len() <= LOG_MESSAGE_MAX` (120) fits a `u16`.
    put_u16(buf, 2, message.len() as u16);
    put_u32(buf, 4, event_id);

    let mut offset = LOG_RECORD_HEADER_LEN;
    buf[offset..offset + message.len()].copy_from_slice(message.as_bytes());
    offset += message.len();

    for (key, value) in fields {
        // Lengths bounded above; the casts are lossless.
        buf[offset] = key.len() as u8;
        buf[offset + 1] = value.len() as u8;
        offset += LOG_FIELD_PREFIX_LEN;
        buf[offset..offset + key.len()].copy_from_slice(key.as_bytes());
        offset += key.len();
        buf[offset..offset + value.len()].copy_from_slice(value.as_bytes());
        offset += value.len();
    }

    Ok(offset)
}

/// A validated, borrowed view over an encoded diagnostic record.
///
/// [`decode_record`] fully validates the wire bytes — every length is in
/// bounds, every slice lies within the buffer, and the message and every
/// field key/value are valid UTF-8 — before constructing this view, so each
/// accessor is total and allocation-free.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct LogRecordRef<'a> {
    level: u8,
    event_id: u32,
    message: &'a str,
    /// The slice holding the `field_count` field records, exactly
    /// `field_count` records long with no trailing bytes.
    fields_bytes: &'a [u8],
    field_count: usize,
}

impl<'a> LogRecordRef<'a> {
    /// The record's level discriminant (`0..=`[`LOG_LEVEL_MAX`]).
    #[must_use]
    pub const fn level(&self) -> u8 {
        self.level
    }

    /// The record's event identifier.
    #[must_use]
    pub const fn event_id(&self) -> u32 {
        self.event_id
    }

    /// The record's human-readable message.
    #[must_use]
    pub const fn message(&self) -> &'a str {
        self.message
    }

    /// Number of structured fields the record carries.
    #[must_use]
    pub const fn field_count(&self) -> usize {
        self.field_count
    }

    /// Iterate the record's `(key, value)` field pairs.
    ///
    /// Both halves are guaranteed valid UTF-8 within the bounds by
    /// [`decode_record`], so the iterator is infallible.
    #[must_use]
    pub fn fields(&self) -> LogFieldIter<'a> {
        LogFieldIter {
            bytes: self.fields_bytes,
            offset: 0,
        }
    }
}

/// Iterator over a [`LogRecordRef`]'s `(key, value)` field pairs.
#[derive(Clone, Debug)]
pub struct LogFieldIter<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Iterator for LogFieldIter<'a> {
    type Item = (&'a str, &'a str);

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset >= self.bytes.len() {
            return None;
        }
        // `decode_record` validated every record up front, so the indexing
        // and the UTF-8 conversions below cannot fail; the `unwrap_or` keeps
        // the iterator total without a panic on the (impossible) bad slice
        // (`AGENTS.md` §2.9).
        let key_len = self.bytes[self.offset] as usize;
        let value_len = self.bytes[self.offset + 1] as usize;
        let key_start = self.offset + LOG_FIELD_PREFIX_LEN;
        let value_start = key_start + key_len;
        let value_end = value_start + value_len;
        let key = core::str::from_utf8(&self.bytes[key_start..value_start]).unwrap_or("");
        let value = core::str::from_utf8(&self.bytes[value_start..value_end]).unwrap_or("");
        self.offset = value_end;
        Some((key, value))
    }
}

/// Validate and decode an encoded diagnostic record.
///
/// Every length is range-checked, every slice is confirmed to lie within
/// `bytes`, and the message and each field key/value are confirmed to be
/// valid UTF-8 before a view is returned (`AGENTS.md` §5.4 — validate every
/// input). Any inconsistency is rejected fail-closed; nothing is partially
/// accepted.
///
/// # Errors
///
/// * [`Errno::LengthOutOfRange`] — `bytes` is shorter than the header, a
///   declared length exceeds its maximum, or the record's declared lengths
///   do not exactly tile `bytes`.
/// * [`Errno::OutOfRange`] — the level byte exceeds [`LOG_LEVEL_MAX`], or the
///   message or a field key/value is not valid UTF-8.
pub fn decode_record(bytes: &[u8]) -> Result<LogRecordRef<'_>, Errno> {
    if bytes.len() < LOG_RECORD_HEADER_LEN {
        return Err(Errno::LengthOutOfRange);
    }
    let level = bytes[0];
    if level > LOG_LEVEL_MAX {
        return Err(Errno::OutOfRange);
    }
    let field_count = bytes[1] as usize;
    if field_count > LOG_FIELDS_MAX {
        return Err(Errno::LengthOutOfRange);
    }
    let message_len = read_u16(bytes, 2) as usize;
    if message_len > LOG_MESSAGE_MAX {
        return Err(Errno::LengthOutOfRange);
    }
    let event_id = read_u32(bytes, 4);

    let message_end = LOG_RECORD_HEADER_LEN + message_len;
    if bytes.len() < message_end {
        return Err(Errno::LengthOutOfRange);
    }
    let message = core::str::from_utf8(&bytes[LOG_RECORD_HEADER_LEN..message_end])
        .map_err(|_| Errno::OutOfRange)?;

    // Walk the declared number of field records, validating each lies within
    // the buffer and is valid UTF-8. The records must tile the remainder of
    // `bytes` exactly — a trailing byte or a short buffer is malformed.
    let fields_bytes = &bytes[message_end..];
    let mut offset = 0usize;
    for _ in 0..field_count {
        if offset + LOG_FIELD_PREFIX_LEN > fields_bytes.len() {
            return Err(Errno::LengthOutOfRange);
        }
        let key_len = fields_bytes[offset] as usize;
        let value_len = fields_bytes[offset + 1] as usize;
        if key_len > LOG_FIELD_KEY_MAX || value_len > LOG_FIELD_VALUE_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        let key_start = offset + LOG_FIELD_PREFIX_LEN;
        let value_start = key_start + key_len;
        let value_end = value_start + value_len;
        if value_end > fields_bytes.len() {
            return Err(Errno::LengthOutOfRange);
        }
        core::str::from_utf8(&fields_bytes[key_start..value_start])
            .map_err(|_| Errno::OutOfRange)?;
        core::str::from_utf8(&fields_bytes[value_start..value_end])
            .map_err(|_| Errno::OutOfRange)?;
        offset = value_end;
    }
    // No trailing bytes past the declared fields.
    if offset != fields_bytes.len() {
        return Err(Errno::LengthOutOfRange);
    }

    Ok(LogRecordRef {
        level,
        event_id,
        message,
        fields_bytes,
        field_count,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        decode_record, encode_record, LOG_FIELDS_MAX, LOG_FIELD_KEY_MAX, LOG_FIELD_VALUE_MAX,
        LOG_LEVEL_MAX, LOG_MESSAGE_MAX, LOG_RECORD_HEADER_LEN, LOG_RECORD_MAX,
    };
    use crate::Errno;

    #[test]
    fn round_trips_a_record_with_fields() {
        let mut buf = [0u8; LOG_RECORD_MAX];
        let fields = [("driver", "vcmailbox"), ("node", "42")];
        let len = encode_record(&mut buf, 2, 7030, "bundle accepted", &fields)
            .expect("encodes within bounds");
        let record = decode_record(&buf[..len]).expect("decodes the encoded record");
        assert_eq!(record.level(), 2);
        assert_eq!(record.event_id(), 7030);
        assert_eq!(record.message(), "bundle accepted");
        assert_eq!(record.field_count(), 2);
        let mut iter = record.fields();
        assert_eq!(iter.next(), Some(("driver", "vcmailbox")));
        assert_eq!(iter.next(), Some(("node", "42")));
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn round_trips_a_record_with_no_fields() {
        let mut buf = [0u8; LOG_RECORD_MAX];
        let len = encode_record(&mut buf, 4, 1, "hello", &[]).expect("encodes");
        assert_eq!(len, LOG_RECORD_HEADER_LEN + "hello".len());
        let record = decode_record(&buf[..len]).expect("decodes");
        assert_eq!(record.field_count(), 0);
        assert_eq!(record.fields().count(), 0);
        assert_eq!(record.message(), "hello");
    }

    #[test]
    fn encode_rejects_a_level_above_the_maximum() {
        let mut buf = [0u8; LOG_RECORD_MAX];
        assert_eq!(
            encode_record(&mut buf, LOG_LEVEL_MAX + 1, 0, "x", &[]),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn encode_rejects_an_over_long_message() {
        let mut buf = [0u8; LOG_RECORD_MAX];
        let raw = [b'm'; LOG_MESSAGE_MAX + 1];
        let message = core::str::from_utf8(&raw).expect("ascii");
        assert_eq!(
            encode_record(&mut buf, 2, 0, message, &[]),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn encode_rejects_too_many_fields() {
        let mut buf = [0u8; LOG_RECORD_MAX];
        let pairs = [("k", "v"); LOG_FIELDS_MAX + 1];
        assert_eq!(
            encode_record(&mut buf, 2, 0, "m", &pairs),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn encode_rejects_an_over_long_field() {
        let mut buf = [0u8; LOG_RECORD_MAX];
        let key_raw = [b'k'; LOG_FIELD_KEY_MAX + 1];
        let key = core::str::from_utf8(&key_raw).expect("ascii");
        assert_eq!(
            encode_record(&mut buf, 2, 0, "m", &[(key, "v")]),
            Err(Errno::LengthOutOfRange)
        );
        let value_raw = [b'v'; LOG_FIELD_VALUE_MAX + 1];
        let value = core::str::from_utf8(&value_raw).expect("ascii");
        assert_eq!(
            encode_record(&mut buf, 2, 0, "m", &[("k", value)]),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn encode_rejects_a_buffer_that_is_too_small() {
        let mut buf = [0u8; LOG_RECORD_HEADER_LEN + 2];
        assert_eq!(
            encode_record(&mut buf, 2, 0, "abc", &[]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn decode_rejects_a_short_header() {
        assert_eq!(
            decode_record(&[0u8; LOG_RECORD_HEADER_LEN - 1]),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn decode_rejects_a_level_above_the_maximum() {
        let mut buf = [0u8; LOG_RECORD_HEADER_LEN];
        buf[0] = LOG_LEVEL_MAX + 1;
        assert_eq!(decode_record(&buf), Err(Errno::OutOfRange));
    }

    #[test]
    fn decode_rejects_a_message_length_past_the_buffer() {
        let mut buf = [0u8; LOG_RECORD_HEADER_LEN];
        buf[0] = 2;
        // Claim a 5-byte message with no bytes following.
        buf[2] = 5;
        assert_eq!(decode_record(&buf), Err(Errno::LengthOutOfRange));
    }

    #[test]
    fn decode_rejects_trailing_bytes_past_the_declared_fields() {
        let mut buf = [0u8; LOG_RECORD_MAX];
        let len = encode_record(&mut buf, 2, 0, "m", &[("k", "v")]).expect("encodes");
        // One extra trailing byte makes the field records no longer tile the
        // buffer exactly.
        let decoded = decode_record(&buf[..=len]);
        assert_eq!(decoded, Err(Errno::LengthOutOfRange));
    }

    #[test]
    fn decode_rejects_a_non_utf8_message() {
        let mut buf = [0u8; LOG_RECORD_HEADER_LEN + 1];
        buf[0] = 2;
        buf[2] = 1;
        buf[LOG_RECORD_HEADER_LEN] = 0xFF; // not valid UTF-8
        assert_eq!(decode_record(&buf), Err(Errno::OutOfRange));
    }
}
