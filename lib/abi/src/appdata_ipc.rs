//! The app-data channel: the reserved rendezvous every application reaches
//! its own per-app configuration store through (`plans/APPDATA.md` §3.6).
//!
//! # Why a service owns the store
//!
//! All of a user's applications run as that one user, so the per-inode
//! owner/mode/ACL model cannot separate them — uid is the only principal it
//! keys on. App-from-app isolation *within* one account is therefore not
//! expressible in the filesystem model at all, and a service that keys every
//! answer on the caller's kernel-attested app identity is the mechanism that
//! provides it. The store trees carry
//! [`CapabilityId::APPDATA_ADMIN`](crate::CapabilityId::APPDATA_ADMIN) as
//! their per-inode gate and the service is its only holder, so there is no
//! second path to the bytes.
//!
//! # No request names its own scope
//!
//! A request carries a key and a value. **It never carries a bundle
//! identifier**: the daemon derives which store to open from the
//! [`Origin`](crate::Origin) the kernel attests for the calling task, so
//! there is no request shape by which an app can claim to be another app. A
//! caller with no attested app identity — a kernel principal, a boot-floor
//! program with no signed manifest, a parser-sandbox child — has no store and
//! is refused.
//!
//! # Staged writes, one atomic publish
//!
//! [`AppDataRequest::ConfigSet`] and [`AppDataRequest::ConfigUnset`] *stage* a
//! change against the caller's own session with the daemon;
//! [`AppDataRequest::ConfigCommit`] publishes the staged document whole. A
//! caller that never commits changes nothing on disk, and a crash mid-publish
//! leaves either the old document or the new one — never a torn one.
//!
//! # Wire shape
//!
//! One length-prefixed little-endian record per call, header first:
//!
//! ```text
//! 0   u32  magic       APPDATA_REQUEST_MAGIC
//! 4   u16  version     APPDATA_VERSION_V1
//! 6   u16  op
//! 8   u16  key_len
//! 10  u16  value_len
//! 12  u32  cursor
//! 16  key   bytes      key_len   bytes of UTF-8
//! ..  value bytes      value_len bytes of UTF-8
//! ```
//!
//! The record is variable-width rather than padded to its widest form: a
//! `ConfigGet` is a couple of dozen bytes and a `ConfigCommit` is sixteen, so
//! padding every request to the width of the longest value would put a
//! kilobyte of zeroes on the hot settings-read path for nothing.
//!
//! Every decode fails closed. An unknown magic, version, or operation, a
//! declared length that does not match the record, a field an operation does
//! not use left non-zero, non-UTF-8 text, or a trailing byte past the payload
//! all refuse rather than guess.
//!
//! The **grammar** of a key and a value is not judged here: it has one home,
//! the `key = value` engine in `lib/appconf`, and the daemon applies it
//! through that engine's own validators. This module bounds the transport —
//! the record's shape, its lengths, and its text encoding — exactly as the
//! `users_admin` request codec bounds a record whose field rules live in
//! `lib/users`.

use crate::le::{put_u16, put_u32, read_u16, read_u32};
use crate::Errno;

/// Reserved well-known call-endpoint id of the app-data service (`"AD"`
/// ASCII hex-spelled prefix, mirroring
/// [`crate::pinboard_ipc::PINBOARD_ENDPOINT`]'s convention).
///
/// Reserved but **not** seat-scoped ([`crate::ipc::is_reserved_endpoint`]):
/// app data is not a property of a seat, and a headless machine serves it
/// exactly as a graphical one does. Binding it therefore requires
/// `CAP_IPC_BIND_PRIVILEGED`, which no ordinary account's ceiling carries —
/// a squatter that claimed the rendezvous first could serve forged settings
/// to every application on the machine, so an unentitled bind fails closed.
pub const APPDATA_ENDPOINT: u64 = 0x4144_1001;

/// Magic number identifying an app-data request (`"APD1"` little-endian).
pub const APPDATA_REQUEST_MAGIC: u32 = u32::from_le_bytes(*b"APD1");

/// The `appdata-v1` protocol version.
pub const APPDATA_VERSION_V1: u16 = 1;

/// Maximum length, in bytes, of a configuration key on the wire — and the
/// `key = value` grammar's own key bound, which `tairix_appconf` imports from
/// here.
///
/// A key is one field of a fixed-shape record, so its bound is part of the
/// `abi-v1` contract a third-party caller compiles against; the format engine
/// may not depend on this crate's *dependents*, and this crate may have no
/// dependencies at all, so the one definition lives here and the engine reads
/// it. Both halves therefore cannot disagree about how long a key may be.
///
/// It is a fixed validation bound on untrusted input, not a capacity: a key is
/// a short dotted identifier, and a bigger machine must not accept a longer
/// hostile one.
pub const APPDATA_KEY_MAX: usize = 128;

/// Maximum length, in bytes, of a configuration value on the wire — and the
/// `key = value` grammar's own value bound, which `tairix_appconf` imports
/// from here (see [`APPDATA_KEY_MAX`] for why it lives in this crate).
///
/// A fixed validation bound, not a capacity: a setting holds a short scalar or
/// a path, and bulk data belongs in the store's blob scope, not in a
/// configuration line.
pub const APPDATA_VALUE_MAX: usize = 1024;

/// Byte length of the fixed request header preceding the key and value.
pub const APPDATA_HEADER_LEN: usize = 16;

/// Maximum request, in bytes, the [`APPDATA_ENDPOINT`] accepts: the header
/// plus the widest key and value a record may carry.
pub const APPDATA_MAX_REQUEST: usize = APPDATA_HEADER_LEN + APPDATA_KEY_MAX + APPDATA_VALUE_MAX;

/// Maximum keys one [`AppDataRequest::ConfigList`] page may carry.
///
/// A validation bound on the reply frame, chosen so a page is a few kilobytes
/// and a full store (`tairix_appconf`'s per-document settings bound) takes a
/// small, bounded number of calls to walk. A listing is always paged: no
/// single call may be made to enumerate an unbounded key space.
pub const APPDATA_LIST_PAGE_MAX: u16 = 32;

/// Byte offset of the operation discriminant.
const OP_OFFSET: usize = 6;
/// Byte offset of the key length prefix.
const KEY_LEN_OFFSET: usize = 8;
/// Byte offset of the value length prefix.
const VALUE_LEN_OFFSET: usize = 10;
/// Byte offset of the listing cursor.
const CURSOR_OFFSET: usize = 12;

/// Wire discriminant of [`AppDataRequest::ConfigGet`].
const OP_CONFIG_GET: u16 = 1;
/// Wire discriminant of [`AppDataRequest::ConfigSet`].
const OP_CONFIG_SET: u16 = 2;
/// Wire discriminant of [`AppDataRequest::ConfigUnset`].
const OP_CONFIG_UNSET: u16 = 3;
/// Wire discriminant of [`AppDataRequest::ConfigCommit`].
const OP_CONFIG_COMMIT: u16 = 4;
/// Wire discriminant of [`AppDataRequest::ConfigList`].
const OP_CONFIG_LIST: u16 = 5;

/// One app-data operation on the caller's **own** configuration store.
///
/// Every variant acts on the store the daemon derived from the caller's
/// kernel-attested app identity. None of them names a store, so none of them
/// can reach another application's data.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AppDataRequest<'a> {
    /// Read the committed value of `key`, or be told it is not set.
    ConfigGet {
        /// The key to read.
        key: &'a str,
    },
    /// Stage `key = value`, to be published by [`Self::ConfigCommit`].
    ConfigSet {
        /// The key to write.
        key: &'a str,
        /// Its new value. May be empty: a key set to nothing is a key that
        /// is set, and is distinct from one that is absent.
        value: &'a str,
    },
    /// Stage the removal of `key`, to be published by [`Self::ConfigCommit`].
    ConfigUnset {
        /// The key to remove. Removing a key the store does not carry stages
        /// nothing and is not an error.
        key: &'a str,
    },
    /// Publish every staged change as one atomic document replacement.
    ConfigCommit,
    /// List the keys the store carries, in document order, one bounded page
    /// at a time.
    ConfigList {
        /// Select only keys beginning with these bytes; empty lists every
        /// key. A prefix is not itself a key (`recent.` is the natural way
        /// to ask for a family), so the daemon validates it as a prefix.
        prefix: &'a str,
        /// Index of the first key this page should start at. A page is full
        /// when it carries [`APPDATA_LIST_PAGE_MAX`] keys; the next page
        /// starts `count` further on, and a short page is the last.
        cursor: u32,
    },
}

impl<'a> AppDataRequest<'a> {
    /// Encoded length, in bytes, of this request.
    #[must_use]
    pub const fn wire_len(&self) -> usize {
        let (key, value) = self.payload();
        APPDATA_HEADER_LEN + key.len() + value.len()
    }

    /// The key (or listing prefix) and value this request carries, each empty
    /// when the operation has none.
    const fn payload(&self) -> (&'a str, &'a str) {
        match *self {
            Self::ConfigGet { key } | Self::ConfigUnset { key } => (key, ""),
            Self::ConfigSet { key, value } => (key, value),
            Self::ConfigCommit => ("", ""),
            Self::ConfigList { prefix, .. } => (prefix, ""),
        }
    }

    /// The wire discriminant of this operation.
    const fn op(&self) -> u16 {
        match *self {
            Self::ConfigGet { .. } => OP_CONFIG_GET,
            Self::ConfigSet { .. } => OP_CONFIG_SET,
            Self::ConfigUnset { .. } => OP_CONFIG_UNSET,
            Self::ConfigCommit => OP_CONFIG_COMMIT,
            Self::ConfigList { .. } => OP_CONFIG_LIST,
        }
    }

    /// Encode `self` little-endian into `out`, returning the bytes written.
    ///
    /// # Errors
    ///
    /// * [`Errno::LengthOutOfRange`] — a key longer than
    ///   [`APPDATA_KEY_MAX`] or a value longer than [`APPDATA_VALUE_MAX`].
    /// * [`Errno::BufferTooSmall`] — `out` cannot hold the record.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Errno> {
        let (key, value) = self.payload();
        if key.len() > APPDATA_KEY_MAX || value.len() > APPDATA_VALUE_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        let total = self.wire_len();
        if out.len() < total {
            return Err(Errno::BufferTooSmall);
        }
        out[..total].fill(0);
        put_u32(out, 0, APPDATA_REQUEST_MAGIC);
        put_u16(out, 4, APPDATA_VERSION_V1);
        put_u16(out, OP_OFFSET, self.op());
        // Both lengths are bounded above, so neither truncates.
        put_u16(
            out,
            KEY_LEN_OFFSET,
            u16::try_from(key.len()).map_err(|_| Errno::LengthOutOfRange)?,
        );
        put_u16(
            out,
            VALUE_LEN_OFFSET,
            u16::try_from(value.len()).map_err(|_| Errno::LengthOutOfRange)?,
        );
        if let Self::ConfigList { cursor, .. } = *self {
            put_u32(out, CURSOR_OFFSET, cursor);
        }
        let key_end = APPDATA_HEADER_LEN + key.len();
        out[APPDATA_HEADER_LEN..key_end].copy_from_slice(key.as_bytes());
        out[key_end..total].copy_from_slice(value.as_bytes());
        Ok(total)
    }

    /// Decode a request from `bytes`, failing closed on anything malformed.
    ///
    /// The borrowed key and value point into `bytes`, so a decode copies
    /// nothing.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `bytes` is shorter than the header, or
    ///   than the payload the header declares.
    /// * [`Errno::BadMagic`] — wrong magic, a trailing byte past the
    ///   declared payload, or a field the operation does not use left
    ///   non-zero.
    /// * [`Errno::AbiVersionUnsupported`] — not `appdata-v1`.
    /// * [`Errno::OutOfRange`] — an operation outside the closed set, or a
    ///   key or value that is not valid UTF-8.
    /// * [`Errno::LengthOutOfRange`] — a declared key or value length beyond
    ///   its bound, or a required key that is empty.
    pub fn decode(bytes: &'a [u8]) -> Result<Self, Errno> {
        if bytes.len() < APPDATA_HEADER_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if read_u32(bytes, 0) != APPDATA_REQUEST_MAGIC {
            return Err(Errno::BadMagic);
        }
        if read_u16(bytes, 4) != APPDATA_VERSION_V1 {
            return Err(Errno::AbiVersionUnsupported);
        }
        let key_len = usize::from(read_u16(bytes, KEY_LEN_OFFSET));
        let value_len = usize::from(read_u16(bytes, VALUE_LEN_OFFSET));
        if key_len > APPDATA_KEY_MAX || value_len > APPDATA_VALUE_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        let key_end = APPDATA_HEADER_LEN + key_len;
        let total = key_end + value_len;
        if bytes.len() < total {
            return Err(Errno::BufferTooSmall);
        }
        // A request is exactly one record: a trailing byte means the frame
        // was not the one the sender described.
        if bytes.len() > total {
            return Err(Errno::BadMagic);
        }
        let key = core::str::from_utf8(&bytes[APPDATA_HEADER_LEN..key_end])
            .map_err(|_| Errno::OutOfRange)?;
        let value = core::str::from_utf8(&bytes[key_end..total]).map_err(|_| Errno::OutOfRange)?;
        let cursor = read_u32(bytes, CURSOR_OFFSET);

        match read_u16(bytes, OP_OFFSET) {
            OP_CONFIG_GET => Self::keyed(key, value_len, cursor).map(|key| Self::ConfigGet { key }),
            OP_CONFIG_SET => {
                if cursor != 0 {
                    return Err(Errno::BadMagic);
                }
                if key.is_empty() {
                    return Err(Errno::LengthOutOfRange);
                }
                Ok(Self::ConfigSet { key, value })
            }
            OP_CONFIG_UNSET => {
                Self::keyed(key, value_len, cursor).map(|key| Self::ConfigUnset { key })
            }
            OP_CONFIG_COMMIT => {
                if !key.is_empty() || value_len != 0 || cursor != 0 {
                    return Err(Errno::BadMagic);
                }
                Ok(Self::ConfigCommit)
            }
            OP_CONFIG_LIST => {
                if value_len != 0 {
                    return Err(Errno::BadMagic);
                }
                // A prefix is legitimately empty: that lists the whole store.
                Ok(Self::ConfigList {
                    prefix: key,
                    cursor,
                })
            }
            _ => Err(Errno::OutOfRange),
        }
    }

    /// Check the shape shared by the operations that carry a key and nothing
    /// else: a non-empty key, no value, no cursor.
    fn keyed(key: &'a str, value_len: usize, cursor: u32) -> Result<&'a str, Errno> {
        if value_len != 0 || cursor != 0 {
            return Err(Errno::BadMagic);
        }
        if key.is_empty() {
            return Err(Errno::LengthOutOfRange);
        }
        Ok(key)
    }
}

/// Byte length of the value-reply header following the shared status word:
/// the value length prefix (2) and a reserved pair that must be zero (2).
pub const APPDATA_VALUE_HEADER_LEN: usize = 4;

/// Maximum [`AppDataRequest::ConfigGet`] reply, in bytes.
pub const APPDATA_MAX_VALUE_REPLY: usize =
    crate::reply::STATUS_REPLY_LEN + APPDATA_VALUE_HEADER_LEN + APPDATA_VALUE_MAX;

/// Encode a successful [`AppDataRequest::ConfigGet`] reply carrying `value`.
///
/// A key that is absent is *not* an empty value: the daemon answers that with
/// the shared status frame carrying [`Errno::NotFound`], so a caller can tell
/// "set to nothing" from "not set" without a second call.
///
/// # Errors
///
/// * [`Errno::LengthOutOfRange`] — `value` is longer than
///   [`APPDATA_VALUE_MAX`].
/// * [`Errno::BufferTooSmall`] — `out` cannot hold the reply.
pub fn encode_value_reply(value: &str, out: &mut [u8]) -> Result<usize, Errno> {
    if value.len() > APPDATA_VALUE_MAX {
        return Err(Errno::LengthOutOfRange);
    }
    let total = crate::reply::STATUS_REPLY_LEN + APPDATA_VALUE_HEADER_LEN + value.len();
    if out.len() < total {
        return Err(Errno::BufferTooSmall);
    }
    out[..crate::reply::STATUS_REPLY_LEN]
        .copy_from_slice(&crate::reply::encode_status_reply(Ok(())));
    put_u16(
        out,
        crate::reply::STATUS_REPLY_LEN,
        u16::try_from(value.len()).map_err(|_| Errno::LengthOutOfRange)?,
    );
    put_u16(out, crate::reply::STATUS_REPLY_LEN + 2, 0);
    out[crate::reply::STATUS_REPLY_LEN + APPDATA_VALUE_HEADER_LEN..total]
        .copy_from_slice(value.as_bytes());
    Ok(total)
}

/// Decode a [`AppDataRequest::ConfigGet`] reply.
///
/// # Errors
///
/// * The daemon's own refusal, decoded from the status word — [`Errno::NotFound`]
///   for a key the store does not set.
/// * [`Errno::BufferTooSmall`] — the frame is shorter than its header or than
///   the value it declares.
/// * [`Errno::BadMagic`] — a dirty reserved pair, or a trailing byte past the
///   declared value.
/// * [`Errno::LengthOutOfRange`] — a declared length beyond
///   [`APPDATA_VALUE_MAX`].
/// * [`Errno::OutOfRange`] — a value that is not valid UTF-8.
pub fn decode_value_reply(bytes: &[u8]) -> Result<&str, Errno> {
    crate::reply::decode_status_reply(bytes)?;
    let header = crate::reply::STATUS_REPLY_LEN + APPDATA_VALUE_HEADER_LEN;
    if bytes.len() < header {
        return Err(Errno::BufferTooSmall);
    }
    let len = usize::from(read_u16(bytes, crate::reply::STATUS_REPLY_LEN));
    if read_u16(bytes, crate::reply::STATUS_REPLY_LEN + 2) != 0 {
        return Err(Errno::BadMagic);
    }
    if len > APPDATA_VALUE_MAX {
        return Err(Errno::LengthOutOfRange);
    }
    let total = header + len;
    if bytes.len() < total {
        return Err(Errno::BufferTooSmall);
    }
    if bytes.len() > total {
        return Err(Errno::BadMagic);
    }
    core::str::from_utf8(&bytes[header..total]).map_err(|_| Errno::OutOfRange)
}

/// One key in an [`AppDataRequest::ConfigList`] page: a fixed-width record so
/// the page shares the one paged-reply codec
/// ([`crate::reply::encode_page_reply`]) every enumerating service uses.
#[derive(Copy, Clone, Eq, PartialEq)]
pub struct AppDataKeyRecord {
    bytes: [u8; APPDATA_KEY_MAX],
    len: u16,
}

impl AppDataKeyRecord {
    /// Encoded size on the wire: the length prefix plus the full-width key
    /// buffer.
    pub const WIRE_LEN: usize = 2 + APPDATA_KEY_MAX;

    /// Build a record naming `key`.
    ///
    /// # Errors
    ///
    /// [`Errno::LengthOutOfRange`] — empty, or longer than
    /// [`APPDATA_KEY_MAX`].
    pub fn new(key: &str) -> Result<Self, Errno> {
        if key.is_empty() || key.len() > APPDATA_KEY_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        let mut bytes = [0u8; APPDATA_KEY_MAX];
        bytes[..key.len()].copy_from_slice(key.as_bytes());
        Ok(Self {
            bytes,
            // Bounded by `APPDATA_KEY_MAX` above.
            len: u16::try_from(key.len()).map_err(|_| Errno::LengthOutOfRange)?,
        })
    }

    /// The key this record names.
    #[must_use]
    pub fn as_str(&self) -> &str {
        // Validated as UTF-8 at construction and at decode; an impossible
        // failure reads as empty rather than panicking.
        core::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or("")
    }

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u16(&mut out, 0, self.len);
        out[2..].copy_from_slice(&self.bytes);
        out
    }

    /// Decode one record, failing closed on a malformed one.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `bytes` cannot hold a whole record.
    /// * [`Errno::BadMagic`] — a byte past the declared key is non-zero.
    /// * [`Errno::LengthOutOfRange`] — a declared length of zero or beyond
    ///   [`APPDATA_KEY_MAX`].
    /// * [`Errno::OutOfRange`] — a key that is not valid UTF-8.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let len = usize::from(read_u16(bytes, 0));
        if len == 0 || len > APPDATA_KEY_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        let mut buf = [0u8; APPDATA_KEY_MAX];
        buf.copy_from_slice(&bytes[2..Self::WIRE_LEN]);
        if buf[len..].iter().any(|&b| b != 0) {
            return Err(Errno::BadMagic);
        }
        core::str::from_utf8(&buf[..len]).map_err(|_| Errno::OutOfRange)?;
        Ok(Self {
            bytes: buf,
            // `len` is bounded by `APPDATA_KEY_MAX` above.
            len: u16::try_from(len).map_err(|_| Errno::LengthOutOfRange)?,
        })
    }
}

impl core::fmt::Debug for AppDataKeyRecord {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("AppDataKeyRecord")
            .field(&self.as_str())
            .finish()
    }
}

/// Maximum [`AppDataRequest::ConfigList`] reply, in bytes: a full page of
/// keys behind the shared paged-reply header.
pub const APPDATA_MAX_LIST_REPLY: usize = crate::reply::STATUS_REPLY_LEN
    + crate::reply::PAGE_HEADER_LEN
    + APPDATA_LIST_PAGE_MAX as usize * AppDataKeyRecord::WIRE_LEN;

/// Maximum reply, in bytes, the [`APPDATA_ENDPOINT`] produces — the widest of
/// the status, value, and listing frames. The value the endpoint is created
/// with, so a reply can never be refused for want of room.
pub const APPDATA_MAX_REPLY: usize = if APPDATA_MAX_LIST_REPLY > APPDATA_MAX_VALUE_REPLY {
    APPDATA_MAX_LIST_REPLY
} else {
    APPDATA_MAX_VALUE_REPLY
};

#[cfg(test)]
#[path = "appdata_ipc_tests.rs"]
mod tests;
