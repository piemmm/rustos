//! `abi-v1` wire format for the `log_emit` syscall — a user-space service's
//! structured diagnostic record handed to the kernel's **diagnostic** log
//! sink.
//!
//! # Why this exists
//!
//! The kernel's `lib/log` facility owns the one sink that routes a diagnostic
//! record to the right place — the serial UART on a debug build, the video
//! console on a release build. That sink is kernel-side, so a user-space
//! service cannot reach it directly. `log_emit` closes that gap: a holder of
//! [`CapabilityId::LOG_EMIT`](crate::CapabilityId::LOG_EMIT) serialises one
//! record into this format and traps into the kernel, which validates it,
//! attributes it to the caller, and emits it through the same diagnostic sink
//! the kernel itself uses.
//!
//! # One field model
//!
//! A record's fields carry [`FieldValue`] values —
//! the single typed field-value model shared with the system-log record
//! schema — so a service logs a real integer, error code, capability id,
//! address, or bounded string, never a pre-formatted string it had to render
//! itself. There is no second string-only field encoding.
//!
//! # Security
//!
//! This is a **diagnostic** channel, never the hash-chained security audit
//! log: `log_emit` reaches the kernel's `log_sink` only. The syscall is
//! capability-gated, every field is bounded and validated, and the kernel —
//! not the caller — attributes each record to the calling task. A malformed
//! record is rejected fail-closed; nothing is partially applied.
//!
//! # Wire layout
//!
//! All scalars are little-endian. A record is a fixed
//! [`LOG_RECORD_HEADER_LEN`]-byte header followed by the message bytes and
//! then the field records:
//!
//! | Offset | Size | Field |
//! |-------:|-----:|-------|
//! |   0    |  1   | `level` (`0..=`[`LOG_LEVEL_MAX`]) |
//! |   1    |  1   | `field_count` (`<= `[`LOG_FIELDS_MAX`]) |
//! |   2    |  2   | `message_len` (`<= `[`LOG_MESSAGE_MAX`]) |
//! |   4    |  4   | `event_id` |
//! |   8    | `message_len` | `message`, UTF-8 |
//! |  ...   | per field | `key_len` (`u8`), `key` (UTF-8), then a self-describing [`FieldValue`] encoding |
//!
//! A field's `key_len` is `<= `[`LOG_FIELD_KEY_MAX`]; its encoded value is
//! `<= `[`LOG_FIELD_VALUE_MAX`] bytes.

use crate::field::{
    decode_named_field, encode_named_field, FieldValue, NAMED_FIELD_KEY_PREFIX_LEN,
};
use crate::le::{put_u16, put_u32, read_u16, read_u32};
use crate::Errno;

/// Highest valid `level` byte — the `rustos_log::Level::Critical` discriminant.
pub const LOG_LEVEL_MAX: u8 = 5;

/// Maximum length, in bytes, of a record's message (a security bound, fixed).
pub const LOG_MESSAGE_MAX: usize = 120;

/// Maximum number of structured fields a record may carry.
pub const LOG_FIELDS_MAX: usize = 8;

/// Maximum length, in bytes, of a single field key.
pub const LOG_FIELD_KEY_MAX: usize = 32;

/// Maximum length, in bytes, of a single field's *encoded* [`FieldValue`].
///
/// Diagnostic values are small; this bounds the kernel's per-field copy so a
/// hostile record cannot inflate the buffer (a security bound, fixed).
pub const LOG_FIELD_VALUE_MAX: usize = 256;

/// Fixed size, in bytes, of the record header that precedes the message.
pub const LOG_RECORD_HEADER_LEN: usize = 8;

/// Upper bound, in bytes, on a fully populated encoded record. The kernel
/// copies at most this many bytes from the caller before decoding.
pub const LOG_RECORD_MAX: usize = LOG_RECORD_HEADER_LEN
    + LOG_MESSAGE_MAX
    + LOG_FIELDS_MAX * (NAMED_FIELD_KEY_PREFIX_LEN + LOG_FIELD_KEY_MAX + LOG_FIELD_VALUE_MAX);

/// Serialise one diagnostic record into `buf`, returning the byte length
/// written.
///
/// `fields` are `(key, value)` pairs; each value is encoded through the shared
/// [`FieldValue`] codec. Every bound is checked before a byte is written, so a
/// rejected record leaves `buf` untouched up to the point of failure and is
/// never partially transmitted.
///
/// # Errors
///
/// * [`Errno::OutOfRange`] — `level` exceeds [`LOG_LEVEL_MAX`].
/// * [`Errno::LengthOutOfRange`] — `message`, the field count, a key, or an
///   encoded value exceeds its maximum.
/// * [`Errno::BufferTooSmall`] — `buf` cannot hold the encoded record.
#[allow(clippy::cast_possible_truncation)]
pub fn encode_record(
    buf: &mut [u8],
    level: u8,
    event_id: u32,
    message: &str,
    fields: &[(&str, FieldValue<'_>)],
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
    for (key, _) in fields {
        if key.len() > LOG_FIELD_KEY_MAX {
            return Err(Errno::LengthOutOfRange);
        }
    }

    let header_and_message = LOG_RECORD_HEADER_LEN + message.len();
    if buf.len() < header_and_message {
        return Err(Errno::BufferTooSmall);
    }

    buf[0] = level;
    // `fields.len() <= LOG_FIELDS_MAX` (8) fits a `u8`.
    buf[1] = fields.len() as u8;
    // `message.len() <= LOG_MESSAGE_MAX` (120) fits a `u16`.
    put_u16(buf, 2, message.len() as u16);
    put_u32(buf, 4, event_id);
    buf[LOG_RECORD_HEADER_LEN..header_and_message].copy_from_slice(message.as_bytes());

    let mut offset = header_and_message;
    for (key, value) in fields {
        offset += encode_named_field(
            &mut buf[offset..],
            key,
            value,
            LOG_FIELD_KEY_MAX,
            LOG_FIELD_VALUE_MAX,
        )?;
    }

    Ok(offset)
}

/// A validated, borrowed view over an encoded diagnostic record.
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
    type Item = (&'a str, FieldValue<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset >= self.bytes.len() {
            return None;
        }
        // `decode_record` validated every field up front, so this decode
        // cannot fail; stop defensively rather than panic on the (impossible)
        // bad slice.
        let ((key, value), consumed) = decode_named_field(
            self.bytes.get(self.offset..)?,
            LOG_FIELD_KEY_MAX,
            LOG_FIELD_VALUE_MAX,
        )
        .ok()?;
        self.offset += consumed;
        Some((key, value))
    }
}

/// Validate and decode an encoded diagnostic record.
///
/// Every length is range-checked, every slice is confirmed to lie within
/// `bytes`, and the message, keys, and values are fully validated before a
/// view is returned. Any inconsistency is rejected fail-closed.
///
/// # Errors
///
/// * [`Errno::LengthOutOfRange`] — `bytes` is shorter than the header, a
///   declared length exceeds its maximum, or the declared fields do not
///   exactly tile `bytes`.
/// * [`Errno::OutOfRange`] — the level byte exceeds [`LOG_LEVEL_MAX`], or the
///   message is not valid UTF-8.
/// * [`Errno::BadMagic`] / other — a field key is not valid UTF-8 or a value
///   fails to decode.
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
    // the buffer, its key is valid UTF-8, and its value decodes. The records
    // must tile the remainder of `bytes` exactly.
    let fields_bytes = &bytes[message_end..];
    let mut offset = 0usize;
    for _ in 0..field_count {
        let (_, consumed) = decode_named_field(
            &fields_bytes[offset..],
            LOG_FIELD_KEY_MAX,
            LOG_FIELD_VALUE_MAX,
        )?;
        offset += consumed;
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
        decode_record, encode_record, LOG_FIELDS_MAX, LOG_FIELD_KEY_MAX, LOG_LEVEL_MAX,
        LOG_MESSAGE_MAX, LOG_RECORD_HEADER_LEN, LOG_RECORD_MAX,
    };
    use crate::field::FieldValue;
    use crate::Errno;

    #[test]
    fn round_trips_a_record_with_typed_fields() {
        let mut buf = [0u8; LOG_RECORD_MAX];
        let fields = [
            ("driver", FieldValue::Str("vcmailbox")),
            ("node", FieldValue::UnsignedInt(42)),
        ];
        let len = encode_record(&mut buf, 2, 7030, "bundle accepted", &fields)
            .expect("encodes within bounds");
        let record = decode_record(&buf[..len]).expect("decodes the encoded record");
        assert_eq!(record.level(), 2);
        assert_eq!(record.event_id(), 7030);
        assert_eq!(record.message(), "bundle accepted");
        assert_eq!(record.field_count(), 2);
        let mut iter = record.fields();
        assert_eq!(iter.next(), Some(("driver", FieldValue::Str("vcmailbox"))));
        assert_eq!(iter.next(), Some(("node", FieldValue::UnsignedInt(42))));
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
        let pairs = [("k", FieldValue::Null); LOG_FIELDS_MAX + 1];
        assert_eq!(
            encode_record(&mut buf, 2, 0, "m", &pairs),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn encode_rejects_an_over_long_key() {
        let mut buf = [0u8; LOG_RECORD_MAX];
        let key_raw = [b'k'; LOG_FIELD_KEY_MAX + 1];
        let key = core::str::from_utf8(&key_raw).expect("ascii");
        assert_eq!(
            encode_record(&mut buf, 2, 0, "m", &[(key, FieldValue::Null)]),
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
        buf[2] = 5;
        assert_eq!(decode_record(&buf), Err(Errno::LengthOutOfRange));
    }

    #[test]
    fn decode_rejects_trailing_bytes_past_the_declared_fields() {
        let mut buf = [0u8; LOG_RECORD_MAX];
        let len =
            encode_record(&mut buf, 2, 0, "m", &[("k", FieldValue::Bool(true))]).expect("encodes");
        let mut extended = [0u8; LOG_RECORD_MAX];
        extended[..len].copy_from_slice(&buf[..len]);
        assert_eq!(
            decode_record(&extended[..=len]),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn decode_rejects_a_non_utf8_message() {
        let mut buf = [0u8; LOG_RECORD_HEADER_LEN + 1];
        buf[0] = 2;
        buf[2] = 1;
        buf[LOG_RECORD_HEADER_LEN] = 0xFF;
        assert_eq!(decode_record(&buf), Err(Errno::OutOfRange));
    }
}
