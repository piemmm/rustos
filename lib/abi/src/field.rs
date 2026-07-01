//! Closed typed-field value model for structured log records.
//!
//! This module is the genuinely-foundational, reusable data model the RustOS
//! system log (`plans/SYSLOG.md`) builds its record schema on. It defines:
//!
//! * [`FieldName`] — a validated caller field name (`[a-z][a-z0-9_]{0,63}`)
//!   that refuses the reserved `record.` / `origin.` / `source.` /
//!   `integrity.` / `sys.` prefixes the journal owns.
//! * [`FieldValue`] — the *closed* set of value types a field may hold. The
//!   set is fixed by the log specification; new shapes are not added casually.
//! * A compact little-endian wire codec ([`FieldValue::encode`] /
//!   [`FieldValue::decode`]) for a single value. This is **not** a record or
//!   segment encoder — the framed record/segment format is the log service's
//!   job, built on top of these values.
//! * [`ToFieldValue`] — the conversion trait callers use to log a typed value.
//!   Secret-bearing wrapper types deliberately do **not** implement it, so a
//!   key, password, or capability token cannot be logged by construction.
//!
//! Records are a *flat* set of typed fields: nested maps are forbidden so
//! search, indexing, validation, and rendering stay cheap. A list value is a
//! same-type, bounded sequence of scalars only; it never nests another list.
//!
//! The model is `no_std` and allocation-free: variable-length values (strings,
//! bytes, lists) are borrowed from the encoded buffer on decode, so the same
//! code runs in the kernel, a freestanding driver, and a WebAssembly userland
//! binary.

use crate::{CapabilityId, Duration64, Errno, Time64};

/// Maximum length, in bytes, of a [`FieldName`] (`[a-z][a-z0-9_]{0,63}`).
pub const FIELD_NAME_MAX: usize = 64;

/// Maximum length, in bytes, of a UTF-8 string field value.
///
/// A bound is mandatory: an unbounded string is a denial-of-service and a
/// search/index cost. High-cardinality bulk text does not belong in a log
/// field (it belongs in `caller.message` only as a short summary, or out of
/// band entirely).
pub const FIELD_STR_MAX: usize = 1024;

/// Maximum length, in bytes, of a byte-string field value.
pub const FIELD_BYTES_MAX: usize = 1024;

/// Maximum number of elements in a list field value.
pub const FIELD_LIST_MAX: usize = 256;

/// Bytes in a UUID.
pub const UUID_LEN: usize = 16;

/// Bytes in an IEEE 802 MAC address.
pub const MAC_LEN: usize = 6;

/// Bytes in an IPv4 address.
pub const IPV4_LEN: usize = 4;

/// Bytes in an IPv6 address.
pub const IPV6_LEN: usize = 16;

/// A 128-bit universally-unique identifier, stored as its 16 raw bytes in
/// network (big-endian) order.
///
/// The log model treats a UUID opaquely: it neither parses a textual form nor
/// interprets the version/variant bits. Rendering and generation are the
/// caller's concern.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct Uuid(pub [u8; UUID_LEN]);

/// An IEEE 802 MAC address (six octets).
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct MacAddr(pub [u8; MAC_LEN]);

/// An IP address, either IPv4 or IPv6, stored as its raw octets.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum IpAddr {
    /// A 32-bit IPv4 address.
    V4([u8; IPV4_LEN]),
    /// A 128-bit IPv6 address.
    V6([u8; IPV6_LEN]),
}

/// A base-10 fixed-point decimal: `mantissa * 10^-scale`.
///
/// For example `Decimal { mantissa: 1050, scale: 2 }` is `10.50`. Fixed-point
/// keeps monetary and metric values exact and orderable without dragging IEEE
/// floating point — and its rounding traps — into the log model.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct Decimal {
    /// The integer mantissa.
    pub mantissa: i64,
    /// The number of base-10 fractional digits.
    pub scale: u8,
}

/// Dotted name prefixes the journal reserves for its own attested namespace.
///
/// The journal owns `record.*`, `origin.*`, `source.*`, `integrity.*`, and
/// `sys.*`; caller content may never masquerade as one of them. A caller
/// [`FieldName`] can never collide with these because the name grammar forbids
/// the `.` separator — these prefixes screen *qualified* names in the journal's
/// full namespace (see [`reserved_prefix`]).
pub const RESERVED_PREFIXES: [&str; 5] = ["record.", "origin.", "source.", "integrity.", "sys."];

/// The reserved prefix `name` begins with, or [`None`].
///
/// Use this to screen a *qualified* field name (one that may contain the `.`
/// separator the [`FieldName`] grammar forbids) before it enters the journal's
/// namespace, so caller-supplied content cannot claim a `record.*` / `origin.*`
/// / `source.*` / `integrity.*` / `sys.*` name.
#[must_use]
pub fn reserved_prefix(name: &str) -> Option<&'static str> {
    RESERVED_PREFIXES
        .iter()
        .copied()
        .find(|prefix| name.starts_with(prefix))
}

/// A validated caller field name.
///
/// The grammar is the case-sensitive ASCII identifier `[a-z][a-z0-9_]{0,63}`:
/// a lowercase letter followed by up to 63 more lowercase letters, digits, or
/// underscores. Because the `.` separator is not in the grammar, a valid name
/// can never collide with a reserved journal prefix (`record.` etc.); those are
/// screened separately by [`reserved_prefix`] at the qualified-name layer.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct FieldName<'a>(&'a str);

impl<'a> FieldName<'a> {
    /// Validate `name` against the field-name grammar.
    ///
    /// Returns [`Errno::LengthOutOfRange`] if `name` is empty or longer than
    /// [`FIELD_NAME_MAX`], and [`Errno::BadMagic`] if it violates the grammar.
    /// Fail closed: any deviation is rejected whole.
    pub fn new(name: &'a str) -> Result<Self, Errno> {
        let bytes = name.as_bytes();
        if bytes.is_empty() || bytes.len() > FIELD_NAME_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        if !bytes[0].is_ascii_lowercase() {
            return Err(Errno::BadMagic);
        }
        for &b in &bytes[1..] {
            if !(b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') {
                return Err(Errno::BadMagic);
            }
        }
        Ok(Self(name))
    }

    /// The validated name as a string slice.
    #[must_use]
    pub const fn as_str(&self) -> &'a str {
        self.0
    }
}

// Wire tag bytes. The discriminants are part of the field-value wire format
// and must not be renumbered once a consumer relies on them. They are public
// because they are the outward-facing wire contract a non-Rust program encodes
// against (the `log_emit` record and the generated C header).

/// Wire tag: the explicit absence of a value ([`FieldValue::Null`]).
pub const TAG_NULL: u8 = 0;
/// Wire tag: a boolean ([`FieldValue::Bool`]).
pub const TAG_BOOL: u8 = 1;
/// Wire tag: a signed 64-bit integer ([`FieldValue::SignedInt`]).
pub const TAG_SIGNED: u8 = 2;
/// Wire tag: an unsigned 64-bit integer ([`FieldValue::UnsignedInt`]).
pub const TAG_UNSIGNED: u8 = 3;
/// Wire tag: a fixed-point decimal ([`FieldValue::Decimal`]).
pub const TAG_DECIMAL: u8 = 4;
/// Wire tag: an absolute [`Time64`] instant ([`FieldValue::Time`]).
pub const TAG_TIME: u8 = 5;
/// Wire tag: a [`Duration64`] span ([`FieldValue::Duration`]).
pub const TAG_DURATION: u8 = 6;
/// Wire tag: a bounded UTF-8 string ([`FieldValue::Str`]).
pub const TAG_STR: u8 = 7;
/// Wire tag: a bounded byte string ([`FieldValue::Bytes`]).
pub const TAG_BYTES: u8 = 8;
/// Wire tag: a 128-bit UUID ([`FieldValue::Uuid`]).
pub const TAG_UUID: u8 = 9;
/// Wire tag: an IP address ([`FieldValue::Ip`]).
pub const TAG_IP: u8 = 10;
/// Wire tag: a MAC address ([`FieldValue::Mac`]).
pub const TAG_MAC: u8 = 11;
/// Wire tag: a kernel error code ([`FieldValue::Error`]).
pub const TAG_ERROR: u8 = 12;
/// Wire tag: a capability identifier ([`FieldValue::Capability`]).
pub const TAG_CAP: u8 = 13;
/// Wire tag: a same-type bounded list of scalars ([`FieldValue::List`]).
pub const TAG_LIST: u8 = 14;

/// The closed set of scalar value types a list element may hold.
///
/// A list is a same-type, bounded sequence of scalars. The variable-length
/// types (`Str`, `Bytes`) and the `List` type itself are deliberately absent:
/// lists never nest and never carry unbounded text, which keeps a list's wire
/// layout walkable and its rendering cheap.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum ScalarType {
    /// The absence of a value.
    Null,
    /// A boolean.
    Bool,
    /// A signed 64-bit integer.
    SignedInt,
    /// An unsigned 64-bit integer.
    UnsignedInt,
    /// A base-10 fixed-point decimal.
    Decimal,
    /// An absolute [`Time64`] instant.
    Time,
    /// A [`Duration64`] span.
    Duration,
    /// A 128-bit UUID.
    Uuid,
    /// An IP address.
    Ip,
    /// A MAC address.
    Mac,
    /// A kernel error code.
    Error,
    /// A capability identifier (never a raw token).
    Capability,
}

impl ScalarType {
    const fn tag(self) -> u8 {
        match self {
            Self::Null => TAG_NULL,
            Self::Bool => TAG_BOOL,
            Self::SignedInt => TAG_SIGNED,
            Self::UnsignedInt => TAG_UNSIGNED,
            Self::Decimal => TAG_DECIMAL,
            Self::Time => TAG_TIME,
            Self::Duration => TAG_DURATION,
            Self::Uuid => TAG_UUID,
            Self::Ip => TAG_IP,
            Self::Mac => TAG_MAC,
            Self::Error => TAG_ERROR,
            Self::Capability => TAG_CAP,
        }
    }

    const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            TAG_NULL => Some(Self::Null),
            TAG_BOOL => Some(Self::Bool),
            TAG_SIGNED => Some(Self::SignedInt),
            TAG_UNSIGNED => Some(Self::UnsignedInt),
            TAG_DECIMAL => Some(Self::Decimal),
            TAG_TIME => Some(Self::Time),
            TAG_DURATION => Some(Self::Duration),
            TAG_UUID => Some(Self::Uuid),
            TAG_IP => Some(Self::Ip),
            TAG_MAC => Some(Self::Mac),
            TAG_ERROR => Some(Self::Error),
            TAG_CAP => Some(Self::Capability),
            _ => None,
        }
    }
}

/// A same-type, bounded list of scalar values, borrowed from its encoded form.
///
/// The list is stored as the concatenated payloads of its elements plus the
/// element [`ScalarType`] and count; [`FieldList::iter`] walks them lazily so
/// decoding never allocates. The payload was validated whole when the list was
/// decoded, so iteration never produces a partial element.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct FieldList<'a> {
    elem: ScalarType,
    count: u16,
    payload: &'a [u8],
}

impl<'a> FieldList<'a> {
    /// The scalar type every element of this list holds.
    #[must_use]
    pub const fn elem_type(&self) -> ScalarType {
        self.elem
    }

    /// The number of elements in the list.
    #[must_use]
    pub fn len(&self) -> usize {
        usize::from(self.count)
    }

    /// `true` when the list has no elements.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Iterate the decoded element values in order.
    #[must_use]
    pub const fn iter(&self) -> FieldListIter<'a> {
        FieldListIter {
            elem: self.elem,
            remaining: self.count,
            rest: self.payload,
        }
    }
}

impl<'a> IntoIterator for &FieldList<'a> {
    type Item = FieldValue<'a>;
    type IntoIter = FieldListIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Lazy iterator over the elements of a [`FieldList`].
#[derive(Clone, Debug)]
pub struct FieldListIter<'a> {
    elem: ScalarType,
    remaining: u16,
    rest: &'a [u8],
}

impl<'a> Iterator for FieldListIter<'a> {
    type Item = FieldValue<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        // The payload was validated when the owning list was decoded, so an
        // element decode here cannot fail; stop defensively rather than panic
        // if some caller hands a hand-built list a malformed payload.
        let (value, consumed) = decode_scalar_payload(self.elem, self.rest).ok()?;
        self.remaining -= 1;
        self.rest = &self.rest[consumed..];
        Some(value)
    }
}

/// A single typed log-field value.
///
/// The set is closed: a field holds exactly one of these shapes, and no shape
/// is a nested map. Variable-length values borrow from the buffer they were
/// decoded from.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FieldValue<'a> {
    /// The explicit absence of a value.
    Null,
    /// A boolean.
    Bool(bool),
    /// A signed 64-bit integer.
    SignedInt(i64),
    /// An unsigned 64-bit integer.
    UnsignedInt(u64),
    /// A base-10 fixed-point decimal.
    Decimal(Decimal),
    /// An absolute instant.
    Time(Time64),
    /// A time span.
    Duration(Duration64),
    /// A bounded UTF-8 string (at most [`FIELD_STR_MAX`] bytes).
    Str(&'a str),
    /// A bounded byte string (at most [`FIELD_BYTES_MAX`] bytes).
    Bytes(&'a [u8]),
    /// A 128-bit UUID.
    Uuid(Uuid),
    /// An IP address.
    Ip(IpAddr),
    /// A MAC address.
    Mac(MacAddr),
    /// A kernel error code.
    Error(Errno),
    /// A capability identifier. Never a raw, unforgeable token — only the
    /// public numeric id, so logging it discloses no authority.
    Capability(CapabilityId),
    /// A same-type, bounded list of scalar values.
    List(FieldList<'a>),
}

impl<'a> FieldValue<'a> {
    /// The wire tag byte for this value's type.
    const fn tag(&self) -> u8 {
        match self {
            Self::Null => TAG_NULL,
            Self::Bool(_) => TAG_BOOL,
            Self::SignedInt(_) => TAG_SIGNED,
            Self::UnsignedInt(_) => TAG_UNSIGNED,
            Self::Decimal(_) => TAG_DECIMAL,
            Self::Time(_) => TAG_TIME,
            Self::Duration(_) => TAG_DURATION,
            Self::Str(_) => TAG_STR,
            Self::Bytes(_) => TAG_BYTES,
            Self::Uuid(_) => TAG_UUID,
            Self::Ip(_) => TAG_IP,
            Self::Mac(_) => TAG_MAC,
            Self::Error(_) => TAG_ERROR,
            Self::Capability(_) => TAG_CAP,
            Self::List(_) => TAG_LIST,
        }
    }

    /// The [`ScalarType`] this value represents, or [`None`] if it is a
    /// variable-length value (`Str` / `Bytes`) or a list — none of which may
    /// be a list element.
    #[must_use]
    pub const fn scalar_type(&self) -> Option<ScalarType> {
        match self {
            Self::Null => Some(ScalarType::Null),
            Self::Bool(_) => Some(ScalarType::Bool),
            Self::SignedInt(_) => Some(ScalarType::SignedInt),
            Self::UnsignedInt(_) => Some(ScalarType::UnsignedInt),
            Self::Decimal(_) => Some(ScalarType::Decimal),
            Self::Time(_) => Some(ScalarType::Time),
            Self::Duration(_) => Some(ScalarType::Duration),
            Self::Uuid(_) => Some(ScalarType::Uuid),
            Self::Ip(_) => Some(ScalarType::Ip),
            Self::Mac(_) => Some(ScalarType::Mac),
            Self::Error(_) => Some(ScalarType::Error),
            Self::Capability(_) => Some(ScalarType::Capability),
            Self::Str(_) | Self::Bytes(_) | Self::List(_) => None,
        }
    }

    /// Encode this value into `out`, returning the number of bytes written.
    ///
    /// The encoding is `tag` byte followed by the value's payload. Returns
    /// [`Errno::BufferTooSmall`] if `out` cannot hold the value, or
    /// [`Errno::LengthOutOfRange`] if a string, byte string, or list exceeds
    /// its bound. Fail closed: nothing partial is written on error.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Errno> {
        match self {
            Self::Str(s) => encode_var(TAG_STR, s.as_bytes(), FIELD_STR_MAX, out),
            Self::Bytes(b) => encode_var(TAG_BYTES, b, FIELD_BYTES_MAX, out),
            Self::List(list) => encode_list_value(list.elem, list.count, list.payload, out),
            scalar => {
                if out.is_empty() {
                    return Err(Errno::BufferTooSmall);
                }
                out[0] = scalar.tag();
                let n = encode_scalar_payload(scalar, &mut out[1..])?;
                Ok(1 + n)
            }
        }
    }

    /// Decode one value from the front of `bytes`.
    ///
    /// Returns the decoded value and the number of bytes it consumed, so a
    /// sequence of values can be walked. Every length, tag, and UTF-8
    /// constraint is checked; any violation fails closed with an [`Errno`].
    pub fn decode(bytes: &'a [u8]) -> Result<(Self, usize), Errno> {
        let tag = *bytes.first().ok_or(Errno::BufferTooSmall)?;
        let rest = &bytes[1..];
        match tag {
            TAG_STR => {
                let (data, consumed) = decode_var(rest, FIELD_STR_MAX)?;
                let s = core::str::from_utf8(data).map_err(|_| Errno::BadMagic)?;
                Ok((Self::Str(s), 1 + consumed))
            }
            TAG_BYTES => {
                let (data, consumed) = decode_var(rest, FIELD_BYTES_MAX)?;
                Ok((Self::Bytes(data), 1 + consumed))
            }
            TAG_LIST => {
                let (list, consumed) = decode_list(rest)?;
                Ok((Self::List(list), 1 + consumed))
            }
            other => {
                let ty = ScalarType::from_tag(other).ok_or(Errno::BadMagic)?;
                let (value, consumed) = decode_scalar_payload(ty, rest)?;
                Ok((value, 1 + consumed))
            }
        }
    }
}

/// Encode a list value into `out` from a slice of elements.
///
/// Every element must be the scalar [`ScalarType`] given by `elem`; an element
/// of the wrong type, a variable-length value, or a nested list is rejected
/// with [`Errno::BadMagic`]. At most [`FIELD_LIST_MAX`] elements are allowed
/// ([`Errno::LengthOutOfRange`] otherwise). Returns the bytes written.
pub fn encode_list(
    elem: ScalarType,
    items: &[FieldValue<'_>],
    out: &mut [u8],
) -> Result<usize, Errno> {
    if items.len() > FIELD_LIST_MAX {
        return Err(Errno::LengthOutOfRange);
    }
    if out.len() < LIST_HEADER_LEN {
        return Err(Errno::BufferTooSmall);
    }
    out[0] = TAG_LIST;
    out[1] = elem.tag();
    let count = u16::try_from(items.len()).map_err(|_| Errno::LengthOutOfRange)?;
    out[2..4].copy_from_slice(&count.to_le_bytes());
    let mut pos = LIST_HEADER_LEN;
    for item in items {
        if item.scalar_type() != Some(elem) {
            return Err(Errno::BadMagic);
        }
        let n = encode_scalar_payload(item, &mut out[pos..])?;
        pos += n;
    }
    Ok(pos)
}

// Bytes in a list value's header: tag + element-type tag + u16 count.
const LIST_HEADER_LEN: usize = 4;

// Re-emit an already-decoded list (its payload is canonical) into `out`.
fn encode_list_value(
    elem: ScalarType,
    count: u16,
    payload: &[u8],
    out: &mut [u8],
) -> Result<usize, Errno> {
    let total = LIST_HEADER_LEN + payload.len();
    if out.len() < total {
        return Err(Errno::BufferTooSmall);
    }
    out[0] = TAG_LIST;
    out[1] = elem.tag();
    out[2..4].copy_from_slice(&count.to_le_bytes());
    out[LIST_HEADER_LEN..total].copy_from_slice(payload);
    Ok(total)
}

fn decode_list(rest: &[u8]) -> Result<(FieldList<'_>, usize), Errno> {
    if rest.len() < 3 {
        return Err(Errno::BufferTooSmall);
    }
    let elem = ScalarType::from_tag(rest[0]).ok_or(Errno::BadMagic)?;
    let count = u16::from_le_bytes([rest[1], rest[2]]);
    if usize::from(count) > FIELD_LIST_MAX {
        return Err(Errno::LengthOutOfRange);
    }
    let body = &rest[3..];
    let mut pos = 0;
    for _ in 0..count {
        let (_, consumed) = decode_scalar_payload(elem, &body[pos..])?;
        pos += consumed;
    }
    let list = FieldList {
        elem,
        count,
        payload: &body[..pos],
    };
    Ok((list, 3 + pos))
}

/// Per-field key prefix: a single `key_len` byte precedes the key bytes.
pub const NAMED_FIELD_KEY_PREFIX_LEN: usize = 1;

/// Encode one named `(key, value)` field into `out`: a `key_len` byte, the key
/// bytes, then the self-describing [`FieldValue`] encoding. Returns the number
/// of bytes written.
///
/// This is the one definition of the named-field wire unit shared by every
/// record format that carries `(name, value)` pairs — the `log_emit`
/// diagnostic record and the system-log record model both build on it, so the
/// two can never drift apart.
///
/// `key_max` bounds the key length (and must itself be `<= 255` so the length
/// fits the prefix byte) and `value_max` bounds the encoded value.
///
/// # Errors
///
/// * [`Errno::LengthOutOfRange`] — the key exceeds `key_max` or the encoded
///   value exceeds `value_max`.
/// * [`Errno::BufferTooSmall`] — `out` cannot hold the encoded field.
pub fn encode_named_field(
    out: &mut [u8],
    key: &str,
    value: &FieldValue<'_>,
    key_max: usize,
    value_max: usize,
) -> Result<usize, Errno> {
    if key.len() > key_max {
        return Err(Errno::LengthOutOfRange);
    }
    let key_end = NAMED_FIELD_KEY_PREFIX_LEN + key.len();
    if out.len() < key_end {
        return Err(Errno::BufferTooSmall);
    }
    // `key.len() <= key_max <= 255` fits the prefix byte.
    out[0] = u8::try_from(key.len()).map_err(|_| Errno::LengthOutOfRange)?;
    out[NAMED_FIELD_KEY_PREFIX_LEN..key_end].copy_from_slice(key.as_bytes());
    let value_len = value.encode(&mut out[key_end..])?;
    if value_len > value_max {
        return Err(Errno::LengthOutOfRange);
    }
    Ok(key_end + value_len)
}

/// Decode one named `(key, value)` field from the front of `bytes`, returning
/// the pair and the number of bytes consumed so a sequence can be walked.
///
/// `key_max` and `value_max` bound the key and encoded value; every length is
/// range-checked and the key is validated as UTF-8. This is the decode half of
/// [`encode_named_field`] — the shared named-field wire unit.
///
/// # Errors
///
/// * [`Errno::LengthOutOfRange`] — `bytes` is too short, the key exceeds
///   `key_max`, or the value exceeds `value_max`.
/// * [`Errno::OutOfRange`] — the key is not valid UTF-8.
/// * [`Errno::BadMagic`] / other — the value fails to decode.
pub fn decode_named_field(
    bytes: &[u8],
    key_max: usize,
    value_max: usize,
) -> Result<((&str, FieldValue<'_>), usize), Errno> {
    if bytes.len() < NAMED_FIELD_KEY_PREFIX_LEN {
        return Err(Errno::LengthOutOfRange);
    }
    let key_len = bytes[0] as usize;
    if key_len > key_max {
        return Err(Errno::LengthOutOfRange);
    }
    let key_start = NAMED_FIELD_KEY_PREFIX_LEN;
    let value_start = key_start + key_len;
    if value_start > bytes.len() {
        return Err(Errno::LengthOutOfRange);
    }
    let key =
        core::str::from_utf8(&bytes[key_start..value_start]).map_err(|_| Errno::OutOfRange)?;
    let (value, consumed) = FieldValue::decode(&bytes[value_start..])?;
    if consumed > value_max {
        return Err(Errno::LengthOutOfRange);
    }
    Ok(((key, value), value_start + consumed))
}

// Encode a variable-length value: tag, u16 length, then the data.
fn encode_var(tag: u8, data: &[u8], max: usize, out: &mut [u8]) -> Result<usize, Errno> {
    if data.len() > max {
        return Err(Errno::LengthOutOfRange);
    }
    let total = 3 + data.len();
    if out.len() < total {
        return Err(Errno::BufferTooSmall);
    }
    out[0] = tag;
    let len = u16::try_from(data.len()).map_err(|_| Errno::LengthOutOfRange)?;
    out[1..3].copy_from_slice(&len.to_le_bytes());
    out[3..total].copy_from_slice(data);
    Ok(total)
}

// Decode a variable-length value's u16 length and borrow its data.
fn decode_var(rest: &[u8], max: usize) -> Result<(&[u8], usize), Errno> {
    if rest.len() < 2 {
        return Err(Errno::BufferTooSmall);
    }
    let len = usize::from(u16::from_le_bytes([rest[0], rest[1]]));
    if len > max {
        return Err(Errno::LengthOutOfRange);
    }
    let end = 2 + len;
    if rest.len() < end {
        return Err(Errno::BufferTooSmall);
    }
    Ok((&rest[2..end], end))
}

// Copy `src` into the front of `out`, returning its length, or fail closed.
fn put(out: &mut [u8], src: &[u8]) -> Result<usize, Errno> {
    if out.len() < src.len() {
        return Err(Errno::BufferTooSmall);
    }
    out[..src.len()].copy_from_slice(src);
    Ok(src.len())
}

// Encode the payload (no tag) of a scalar value. Non-scalar values are
// rejected with `Errno::OutOfRange`: they are never list elements and the
// caller always handles them before reaching here.
fn encode_scalar_payload(value: &FieldValue<'_>, out: &mut [u8]) -> Result<usize, Errno> {
    match value {
        FieldValue::Null => Ok(0),
        FieldValue::Bool(b) => put(out, &[u8::from(*b)]),
        FieldValue::SignedInt(v) => put(out, &v.to_le_bytes()),
        FieldValue::UnsignedInt(v) => put(out, &v.to_le_bytes()),
        FieldValue::Decimal(d) => {
            let mut tmp = [0u8; 9];
            tmp[..8].copy_from_slice(&d.mantissa.to_le_bytes());
            tmp[8] = d.scale;
            put(out, &tmp)
        }
        FieldValue::Time(t) => put(out, &t.to_le_bytes()),
        FieldValue::Duration(d) => put(out, &d.to_le_bytes()),
        FieldValue::Uuid(u) => put(out, &u.0),
        FieldValue::Ip(IpAddr::V4(o)) => {
            let mut tmp = [0u8; 1 + IPV4_LEN];
            tmp[0] = 4;
            tmp[1..].copy_from_slice(o);
            put(out, &tmp)
        }
        FieldValue::Ip(IpAddr::V6(o)) => {
            let mut tmp = [0u8; 1 + IPV6_LEN];
            tmp[0] = 6;
            tmp[1..].copy_from_slice(o);
            put(out, &tmp)
        }
        FieldValue::Mac(m) => put(out, &m.0),
        FieldValue::Error(e) => put(out, &e.as_i32().to_le_bytes()),
        FieldValue::Capability(c) => put(out, &c.as_u16().to_le_bytes()),
        FieldValue::Str(_) | FieldValue::Bytes(_) | FieldValue::List(_) => Err(Errno::OutOfRange),
    }
}

// Decode the payload (no tag) of a scalar value of type `ty`, returning the
// value and the number of bytes consumed.
fn decode_scalar_payload(
    ty: ScalarType,
    bytes: &[u8],
) -> Result<(FieldValue<'static>, usize), Errno> {
    fn take<const N: usize>(bytes: &[u8]) -> Result<[u8; N], Errno> {
        let slice = bytes.get(..N).ok_or(Errno::BufferTooSmall)?;
        let mut out = [0u8; N];
        out.copy_from_slice(slice);
        Ok(out)
    }
    match ty {
        ScalarType::Null => Ok((FieldValue::Null, 0)),
        ScalarType::Bool => {
            let b = *bytes.first().ok_or(Errno::BufferTooSmall)?;
            if b > 1 {
                return Err(Errno::BadMagic);
            }
            Ok((FieldValue::Bool(b == 1), 1))
        }
        ScalarType::SignedInt => Ok((FieldValue::SignedInt(i64::from_le_bytes(take(bytes)?)), 8)),
        ScalarType::UnsignedInt => {
            Ok((FieldValue::UnsignedInt(u64::from_le_bytes(take(bytes)?)), 8))
        }
        ScalarType::Decimal => {
            let raw: [u8; 9] = take(bytes)?;
            let mantissa = i64::from_le_bytes(take::<8>(&raw)?);
            Ok((
                FieldValue::Decimal(Decimal {
                    mantissa,
                    scale: raw[8],
                }),
                9,
            ))
        }
        ScalarType::Time => {
            let raw: [u8; Time64::WIRE_LEN] = take(bytes)?;
            Ok((
                FieldValue::Time(Time64::from_bytes(&raw)?),
                Time64::WIRE_LEN,
            ))
        }
        ScalarType::Duration => {
            let raw: [u8; Duration64::WIRE_LEN] = take(bytes)?;
            Ok((
                FieldValue::Duration(Duration64::from_bytes(&raw)?),
                Duration64::WIRE_LEN,
            ))
        }
        ScalarType::Uuid => Ok((FieldValue::Uuid(Uuid(take(bytes)?)), UUID_LEN)),
        ScalarType::Ip => {
            let family = *bytes.first().ok_or(Errno::BufferTooSmall)?;
            match family {
                4 => Ok((FieldValue::Ip(IpAddr::V4(take(&bytes[1..])?)), 1 + IPV4_LEN)),
                6 => Ok((FieldValue::Ip(IpAddr::V6(take(&bytes[1..])?)), 1 + IPV6_LEN)),
                _ => Err(Errno::BadMagic),
            }
        }
        ScalarType::Mac => Ok((FieldValue::Mac(MacAddr(take(bytes)?)), MAC_LEN)),
        ScalarType::Error => {
            let code = i32::from_le_bytes(take(bytes)?);
            let errno = Errno::from_i32(code).ok_or(Errno::OutOfRange)?;
            Ok((FieldValue::Error(errno), 4))
        }
        ScalarType::Capability => {
            let raw = u16::from_le_bytes(take(bytes)?);
            Ok((FieldValue::Capability(CapabilityId::from_raw(raw)?), 2))
        }
    }
}

/// Conversion from a typed value into a [`FieldValue`] for logging.
///
/// A caller logs a value by passing a type that implements this trait. The
/// trait is the *only* gate between application data and the log: a
/// secret-bearing wrapper type (a key, password, or capability token) MUST NOT
/// implement it, so a secret cannot be logged by construction — there is no
/// blanket impl and no `Display`/`Debug` fallback that would let one slip
/// through. The following does not compile, by design:
///
/// ```compile_fail
/// use rustos_abi::field::ToFieldValue;
/// struct SecretKey([u8; 32]);
/// fn record<V: ToFieldValue + ?Sized>(_value: &V) {}
/// record(&SecretKey([0u8; 32]));
/// ```
pub trait ToFieldValue {
    /// Borrow this value as a [`FieldValue`].
    fn to_field_value(&self) -> FieldValue<'_>;
}

macro_rules! to_field_value_signed {
    ($($t:ty),+) => {$(
        impl ToFieldValue for $t {
            fn to_field_value(&self) -> FieldValue<'_> {
                FieldValue::SignedInt(i64::from(*self))
            }
        }
    )+};
}

macro_rules! to_field_value_unsigned {
    ($($t:ty),+) => {$(
        impl ToFieldValue for $t {
            fn to_field_value(&self) -> FieldValue<'_> {
                FieldValue::UnsignedInt(u64::from(*self))
            }
        }
    )+};
}

to_field_value_signed!(i8, i16, i32);
to_field_value_unsigned!(u8, u16, u32);

impl ToFieldValue for i64 {
    fn to_field_value(&self) -> FieldValue<'_> {
        FieldValue::SignedInt(*self)
    }
}

impl ToFieldValue for u64 {
    fn to_field_value(&self) -> FieldValue<'_> {
        FieldValue::UnsignedInt(*self)
    }
}

impl ToFieldValue for bool {
    fn to_field_value(&self) -> FieldValue<'_> {
        FieldValue::Bool(*self)
    }
}

impl ToFieldValue for str {
    fn to_field_value(&self) -> FieldValue<'_> {
        FieldValue::Str(self)
    }
}

impl ToFieldValue for [u8] {
    fn to_field_value(&self) -> FieldValue<'_> {
        FieldValue::Bytes(self)
    }
}

impl ToFieldValue for Decimal {
    fn to_field_value(&self) -> FieldValue<'_> {
        FieldValue::Decimal(*self)
    }
}

impl ToFieldValue for Time64 {
    fn to_field_value(&self) -> FieldValue<'_> {
        FieldValue::Time(*self)
    }
}

impl ToFieldValue for Duration64 {
    fn to_field_value(&self) -> FieldValue<'_> {
        FieldValue::Duration(*self)
    }
}

impl ToFieldValue for Uuid {
    fn to_field_value(&self) -> FieldValue<'_> {
        FieldValue::Uuid(*self)
    }
}

impl ToFieldValue for IpAddr {
    fn to_field_value(&self) -> FieldValue<'_> {
        FieldValue::Ip(*self)
    }
}

impl ToFieldValue for MacAddr {
    fn to_field_value(&self) -> FieldValue<'_> {
        FieldValue::Mac(*self)
    }
}

impl ToFieldValue for Errno {
    fn to_field_value(&self) -> FieldValue<'_> {
        FieldValue::Error(*self)
    }
}

impl ToFieldValue for CapabilityId {
    fn to_field_value(&self) -> FieldValue<'_> {
        FieldValue::Capability(*self)
    }
}

/// Render `bytes` as lowercase hex into `f`, no separators.
fn write_hex(f: &mut core::fmt::Formatter<'_>, bytes: &[u8]) -> core::fmt::Result {
    for &b in bytes {
        write!(f, "{b:02x}")?;
    }
    Ok(())
}

impl core::fmt::Display for Decimal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.scale == 0 {
            return write!(f, "{}", self.mantissa);
        }
        // Split into integer and fractional parts without floating point.
        // A scale that would overflow the `10^scale` divisor is rendered in
        // scientific-ish `mantissa e-scale` form rather than lying about the
        // value.
        if self.scale > 18 {
            return write!(f, "{}e-{}", self.mantissa, self.scale);
        }
        let divisor = 10i128.pow(u32::from(self.scale));
        let mantissa = i128::from(self.mantissa);
        let sign = if mantissa < 0 { "-" } else { "" };
        let magnitude = mantissa.unsigned_abs();
        let int_part = magnitude / divisor.unsigned_abs();
        let frac_part = magnitude % divisor.unsigned_abs();
        write!(
            f,
            "{sign}{int_part}.{frac_part:0width$}",
            width = usize::from(self.scale)
        )
    }
}

impl core::fmt::Display for IpAddr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::V4(o) => write!(f, "{}.{}.{}.{}", o[0], o[1], o[2], o[3]),
            Self::V6(octets) => {
                for group in 0..8 {
                    if group != 0 {
                        f.write_str(":")?;
                    }
                    let hi = octets[group * 2];
                    let lo = octets[group * 2 + 1];
                    write!(f, "{:x}", u16::from(hi) << 8 | u16::from(lo))?;
                }
                Ok(())
            }
        }
    }
}

impl core::fmt::Display for MacAddr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for (i, byte) in self.0.iter().enumerate() {
            if i != 0 {
                f.write_str(":")?;
            }
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl core::fmt::Display for Uuid {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write_hex(f, &self.0)
    }
}

impl core::fmt::Display for FieldValue<'_> {
    /// Render the value as diagnostic text.
    ///
    /// This is the one text rendering of a field value, used by the console
    /// log sinks (they format `key={value}`). It is total and allocation-free:
    /// every variant renders without a panic. Numbers are decimal, `Bytes` and
    /// `Uuid` are lowercase hex, addresses use their conventional notation, and
    /// a list renders as space-separated elements in square brackets.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Null => f.write_str("null"),
            Self::Bool(b) => write!(f, "{b}"),
            Self::SignedInt(v) => write!(f, "{v}"),
            Self::UnsignedInt(v) => write!(f, "{v}"),
            Self::Decimal(d) => write!(f, "{d}"),
            Self::Time(t) => write!(f, "{}.{:09}", t.secs(), t.subsec_nanos()),
            Self::Duration(d) => write!(f, "{}.{:09}s", d.secs(), d.subsec_nanos()),
            Self::Str(s) => f.write_str(s),
            Self::Bytes(b) => write_hex(f, b),
            Self::Uuid(u) => write!(f, "{u}"),
            Self::Ip(ip) => write!(f, "{ip}"),
            Self::Mac(m) => write!(f, "{m}"),
            Self::Error(e) => write!(f, "{e}"),
            Self::Capability(c) => write!(f, "cap{}", c.as_u16()),
            Self::List(list) => {
                f.write_str("[")?;
                for (i, elem) in list.iter().enumerate() {
                    if i != 0 {
                        f.write_str(" ")?;
                    }
                    write!(f, "{elem}")?;
                }
                f.write_str("]")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(value: FieldValue<'_>) {
        let mut buf = [0u8; 2048];
        let written = value.encode(&mut buf).expect("encode");
        let (decoded, consumed) = FieldValue::decode(&buf[..written]).expect("decode");
        assert_eq!(
            consumed, written,
            "value {value:?} must consume what it wrote"
        );
        assert_eq!(decoded, value, "value {value:?} must survive a round trip");
    }

    #[test]
    fn every_scalar_value_round_trips() {
        round_trip(FieldValue::Null);
        round_trip(FieldValue::Bool(true));
        round_trip(FieldValue::Bool(false));
        round_trip(FieldValue::SignedInt(i64::MIN));
        round_trip(FieldValue::SignedInt(-1));
        round_trip(FieldValue::UnsignedInt(u64::MAX));
        round_trip(FieldValue::Decimal(Decimal {
            mantissa: -1050,
            scale: 2,
        }));
        // Pre-1970 and post-2038 instants both survive (64-bit-native time).
        round_trip(FieldValue::Time(Time64::from_secs(-86_400)));
        round_trip(FieldValue::Time(Time64::from_secs(4_000_000_000)));
        round_trip(FieldValue::Duration(Duration64::from_secs(10)));
        round_trip(FieldValue::Uuid(Uuid([0xAB; UUID_LEN])));
        round_trip(FieldValue::Ip(IpAddr::V4([10, 0, 0, 1])));
        round_trip(FieldValue::Ip(IpAddr::V6([0x20; IPV6_LEN])));
        round_trip(FieldValue::Mac(MacAddr([
            0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01,
        ])));
        round_trip(FieldValue::Error(Errno::NotFound));
        round_trip(FieldValue::Capability(CapabilityId::FS_ACCESS));
    }

    #[test]
    fn variable_length_values_round_trip() {
        round_trip(FieldValue::Str("dhcp timeout"));
        round_trip(FieldValue::Str(""));
        round_trip(FieldValue::Bytes(&[0, 1, 2, 3, 0xFF]));
        round_trip(FieldValue::Bytes(&[]));
    }

    #[test]
    fn string_value_rejects_over_bound() {
        let big = [b'x'; FIELD_STR_MAX + 1];
        let s = core::str::from_utf8(&big).unwrap();
        let mut buf = [0u8; FIELD_STR_MAX + 8];
        assert_eq!(
            FieldValue::Str(s).encode(&mut buf),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn encode_into_short_buffer_fails_closed() {
        let mut tiny = [0u8; 2];
        assert_eq!(
            FieldValue::SignedInt(7).encode(&mut tiny),
            Err(Errno::BufferTooSmall)
        );
        let mut empty = [0u8; 0];
        assert_eq!(
            FieldValue::Null.encode(&mut empty),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn decode_rejects_unknown_tag_and_noncanonical_bool() {
        assert_eq!(FieldValue::decode(&[]), Err(Errno::BufferTooSmall));
        assert_eq!(FieldValue::decode(&[200]), Err(Errno::BadMagic));
        assert_eq!(FieldValue::decode(&[TAG_BOOL, 2]), Err(Errno::BadMagic));
        // Truncated signed integer payload.
        assert_eq!(
            FieldValue::decode(&[TAG_SIGNED, 1, 2, 3]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn decode_rejects_non_utf8_string() {
        // Length 1, byte 0xFF is not valid UTF-8.
        assert_eq!(
            FieldValue::decode(&[TAG_STR, 1, 0, 0xFF]),
            Err(Errno::BadMagic)
        );
    }

    #[test]
    fn list_round_trips_and_iterates() {
        let items = [
            FieldValue::UnsignedInt(1),
            FieldValue::UnsignedInt(2),
            FieldValue::UnsignedInt(3),
        ];
        let mut buf = [0u8; 128];
        let written = encode_list(ScalarType::UnsignedInt, &items, &mut buf).expect("encode list");
        let (decoded, consumed) = FieldValue::decode(&buf[..written]).expect("decode list");
        assert_eq!(consumed, written);
        let FieldValue::List(list) = decoded else {
            panic!("expected a list, got {decoded:?}");
        };
        assert_eq!(list.len(), 3);
        assert_eq!(list.elem_type(), ScalarType::UnsignedInt);
        let collected: [FieldValue<'_>; 3] = {
            let mut iter = list.iter();
            [
                iter.next().unwrap(),
                iter.next().unwrap(),
                iter.next().unwrap(),
            ]
        };
        assert_eq!(collected, items);
        // A decoded list re-encodes to the identical bytes.
        let mut buf2 = [0u8; 128];
        let again = decoded.encode(&mut buf2).expect("re-encode list");
        assert_eq!(&buf2[..again], &buf[..written]);
    }

    #[test]
    fn empty_list_round_trips() {
        let mut buf = [0u8; 16];
        let written = encode_list(ScalarType::Bool, &[], &mut buf).expect("encode empty list");
        let (decoded, _) = FieldValue::decode(&buf[..written]).expect("decode empty list");
        let FieldValue::List(list) = decoded else {
            panic!("expected a list");
        };
        assert!(list.is_empty());
        assert_eq!(list.iter().count(), 0);
    }

    #[test]
    fn list_rejects_mismatched_element_type() {
        let items = [FieldValue::UnsignedInt(1), FieldValue::Bool(true)];
        let mut buf = [0u8; 64];
        assert_eq!(
            encode_list(ScalarType::UnsignedInt, &items, &mut buf),
            Err(Errno::BadMagic)
        );
    }

    #[test]
    fn list_rejects_non_scalar_element() {
        // A variable-length value can never be a list element: `scalar_type`
        // is `None`, so it is refused whatever element type is declared.
        assert_eq!(FieldValue::Str("nope").scalar_type(), None);
        let items = [FieldValue::Str("nope")];
        let mut buf = [0u8; 64];
        assert_eq!(
            encode_list(ScalarType::UnsignedInt, &items, &mut buf),
            Err(Errno::BadMagic)
        );
    }

    #[test]
    fn field_name_grammar_accepts_valid_and_rejects_invalid() {
        assert!(FieldName::new("iface").is_ok());
        assert!(FieldName::new("a").is_ok());
        assert!(FieldName::new("a_1_b2").is_ok());
        assert_eq!(FieldName::new("a").unwrap().as_str(), "a");
        // Empty.
        assert_eq!(FieldName::new(""), Err(Errno::LengthOutOfRange));
        // Leading digit / uppercase / illegal characters.
        assert_eq!(FieldName::new("1abc"), Err(Errno::BadMagic));
        assert_eq!(FieldName::new("Abc"), Err(Errno::BadMagic));
        assert_eq!(FieldName::new("a-b"), Err(Errno::BadMagic));
        assert_eq!(FieldName::new("a.b"), Err(Errno::BadMagic));
        // Max length and one past it.
        let max = [b'a'; FIELD_NAME_MAX];
        assert!(FieldName::new(core::str::from_utf8(&max).unwrap()).is_ok());
        let over = [b'a'; FIELD_NAME_MAX + 1];
        assert_eq!(
            FieldName::new(core::str::from_utf8(&over).unwrap()),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn reserved_prefix_screens_qualified_names() {
        assert_eq!(reserved_prefix("origin.uid"), Some("origin."));
        assert_eq!(reserved_prefix("record.seq"), Some("record."));
        assert_eq!(reserved_prefix("sys.boot_id"), Some("sys."));
        // A caller name with no dotted prefix is free.
        assert_eq!(reserved_prefix("system"), None);
        assert_eq!(reserved_prefix("sys"), None);
        assert_eq!(reserved_prefix("iface"), None);
    }
}
