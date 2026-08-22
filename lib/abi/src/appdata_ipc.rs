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
//! # One read for the whole document, not one per key
//!
//! [`AppDataRequest::ConfigRead`] answers with the caller's **whole** merged
//! configuration document — the machine-wide policy layer with the user's own
//! overrides applied — as canonical `key = value` text the client parses with
//! the one format engine. So an application's start-up costs one call, one
//! store read, and one parse however many settings it goes on to consult; a
//! per-key read would have cost the daemon a file read and a parse *per key*,
//! and a client that reads one setting pays no more than one that reads forty.
//!
//! The reply is a whole document or nothing: the request declares the reply
//! buffer the caller has, and a document that does not fit comes back as the
//! byte count it needs ([`ConfigDocument::NeedsCapacity`]) with no body at
//! all. A caller therefore never parses a truncated prefix, and never reads a
//! document assembled out of two different snapshots — every answer is one
//! point-in-time view.
//!
//! # Staged writes, one atomic publish
//!
//! [`AppDataRequest::ConfigSet`] and [`AppDataRequest::ConfigUnset`] *stage* a
//! change against the caller's own session with the daemon;
//! [`AppDataRequest::ConfigCommit`] publishes the staged document whole. A
//! caller that never commits changes nothing on disk, and a crash mid-publish
//! leaves either the old document or the new one — never a torn one. A staged
//! change is visible to the staging caller's own [`AppDataRequest::ConfigRead`]
//! and to no other principal, so a settings sheet reads back what it just set.
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
//! 12  u32  capacity    ConfigRead only: the caller's reply buffer
//! 16  key   bytes      key_len   bytes of UTF-8
//! ..  value bytes      value_len bytes of UTF-8
//! ```
//!
//! The record is variable-width rather than padded to its widest form: a
//! `ConfigRead` is sixteen bytes and a `ConfigCommit` is sixteen, so padding
//! every request to the width of the longest value would put a kilobyte of
//! zeroes on the hot settings path for nothing.
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

/// Maximum length, in bytes, of a whole configuration document — the widest
/// [`AppDataRequest::ConfigRead`] answer, and the `key = value` grammar's own
/// document bound, which `tairix_appconf` imports from here (see
/// [`APPDATA_KEY_MAX`] for why it lives in this crate).
///
/// A fixed validation bound on untrusted input, not a growable capacity: it
/// sizes the work a hostile store can demand of a parser before a byte of it
/// is believed, and a bigger machine must not accept a bigger hostile
/// document.
pub const APPDATA_DOCUMENT_MAX: usize = 64 * 1024;

/// File name of the private configuration scope's document, wherever that
/// scope appears: `<store>/settings.conf` on the volume the service owns, and
/// `<Bundle>.app/DefaultSettings/settings.conf` for the defaults a bundle
/// ships.
///
/// The two are the same document by definition — the bundle's is the fallback
/// layer beneath the store's — so the name is defined once here, in the
/// app-data contract both the service and the client compile against, rather
/// than spelled in each of them.
pub const APPDATA_SETTINGS_FILE: &str = "settings.conf";

/// Byte length of the fixed request header preceding the key and value.
pub const APPDATA_HEADER_LEN: usize = 16;

/// Maximum request, in bytes, the [`APPDATA_ENDPOINT`] accepts: the header
/// plus the widest key and value a record may carry.
pub const APPDATA_MAX_REQUEST: usize = APPDATA_HEADER_LEN + APPDATA_KEY_MAX + APPDATA_VALUE_MAX;

/// Byte offset of the operation discriminant.
const OP_OFFSET: usize = 6;
/// Byte offset of the key length prefix.
const KEY_LEN_OFFSET: usize = 8;
/// Byte offset of the value length prefix.
const VALUE_LEN_OFFSET: usize = 10;
/// Byte offset of the reply-buffer capacity.
const CAPACITY_OFFSET: usize = 12;

/// Wire discriminant of [`AppDataRequest::ConfigRead`].
const OP_CONFIG_READ: u16 = 1;
/// Wire discriminant of [`AppDataRequest::ConfigSet`].
const OP_CONFIG_SET: u16 = 2;
/// Wire discriminant of [`AppDataRequest::ConfigUnset`].
const OP_CONFIG_UNSET: u16 = 3;
/// Wire discriminant of [`AppDataRequest::ConfigCommit`].
const OP_CONFIG_COMMIT: u16 = 4;

/// One app-data operation on the caller's **own** configuration store.
///
/// Every variant acts on the store the daemon derived from the caller's
/// kernel-attested app identity. None of them names a store, so none of them
/// can reach another application's data.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AppDataRequest<'a> {
    /// Read the caller's whole merged configuration document.
    ConfigRead {
        /// How many document bytes the caller's reply buffer can hold, past
        /// the frame's own header. A document longer than this comes back as
        /// [`ConfigDocument::NeedsCapacity`] and no body, so the caller can
        /// size a buffer exactly and ask again rather than parse a fragment.
        /// Zero is legal and asks only for the length.
        capacity: u32,
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
}

impl<'a> AppDataRequest<'a> {
    /// Encoded length, in bytes, of this request.
    #[must_use]
    pub const fn wire_len(&self) -> usize {
        let (key, value) = self.payload();
        APPDATA_HEADER_LEN + key.len() + value.len()
    }

    /// The key and value this request carries, each empty when the operation
    /// has none.
    const fn payload(&self) -> (&'a str, &'a str) {
        match *self {
            Self::ConfigUnset { key } => (key, ""),
            Self::ConfigSet { key, value } => (key, value),
            Self::ConfigRead { .. } | Self::ConfigCommit => ("", ""),
        }
    }

    /// The wire discriminant of this operation.
    const fn op(&self) -> u16 {
        match *self {
            Self::ConfigRead { .. } => OP_CONFIG_READ,
            Self::ConfigSet { .. } => OP_CONFIG_SET,
            Self::ConfigUnset { .. } => OP_CONFIG_UNSET,
            Self::ConfigCommit => OP_CONFIG_COMMIT,
        }
    }

    /// Encode `self` little-endian into `out`, returning the bytes written.
    ///
    /// # Errors
    ///
    /// * [`Errno::LengthOutOfRange`] — a key longer than
    ///   [`APPDATA_KEY_MAX`], a value longer than [`APPDATA_VALUE_MAX`], or a
    ///   capacity beyond [`APPDATA_DOCUMENT_MAX`].
    /// * [`Errno::BufferTooSmall`] — `out` cannot hold the record.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Errno> {
        let (key, value) = self.payload();
        if key.len() > APPDATA_KEY_MAX || value.len() > APPDATA_VALUE_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        if let Self::ConfigRead { capacity } = *self {
            if capacity as usize > APPDATA_DOCUMENT_MAX {
                return Err(Errno::LengthOutOfRange);
            }
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
        if let Self::ConfigRead { capacity } = *self {
            put_u32(out, CAPACITY_OFFSET, capacity);
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
    ///   its bound, a required key that is empty, or a capacity beyond
    ///   [`APPDATA_DOCUMENT_MAX`].
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
        let capacity = read_u32(bytes, CAPACITY_OFFSET);

        match read_u16(bytes, OP_OFFSET) {
            OP_CONFIG_READ => {
                if !key.is_empty() || value_len != 0 {
                    return Err(Errno::BadMagic);
                }
                if capacity as usize > APPDATA_DOCUMENT_MAX {
                    return Err(Errno::LengthOutOfRange);
                }
                Ok(Self::ConfigRead { capacity })
            }
            OP_CONFIG_SET => {
                if capacity != 0 {
                    return Err(Errno::BadMagic);
                }
                if key.is_empty() {
                    return Err(Errno::LengthOutOfRange);
                }
                Ok(Self::ConfigSet { key, value })
            }
            OP_CONFIG_UNSET => {
                Self::keyed(key, value_len, capacity).map(|key| Self::ConfigUnset { key })
            }
            OP_CONFIG_COMMIT => {
                if !key.is_empty() || value_len != 0 || capacity != 0 {
                    return Err(Errno::BadMagic);
                }
                Ok(Self::ConfigCommit)
            }
            _ => Err(Errno::OutOfRange),
        }
    }

    /// Check the shape of an operation that carries a key and nothing else: a
    /// non-empty key, no value, no capacity.
    fn keyed(key: &'a str, value_len: usize, capacity: u32) -> Result<&'a str, Errno> {
        if value_len != 0 || capacity != 0 {
            return Err(Errno::BadMagic);
        }
        if key.is_empty() {
            return Err(Errno::LengthOutOfRange);
        }
        Ok(key)
    }
}

/// Byte length of the document-reply header following the shared status word:
/// the document's whole length, whether or not its bytes fitted.
pub const APPDATA_DOCUMENT_HEADER_LEN: usize = 4;

/// Maximum reply, in bytes, the [`APPDATA_ENDPOINT`] produces — the widest
/// [`AppDataRequest::ConfigRead`] answer. The value the endpoint is created
/// with, so a reply can never be refused for want of room.
pub const APPDATA_MAX_REPLY: usize =
    crate::reply::STATUS_REPLY_LEN + APPDATA_DOCUMENT_HEADER_LEN + APPDATA_DOCUMENT_MAX;

/// What an [`AppDataRequest::ConfigRead`] answered.
///
/// Two states, and no third: a caller either holds the whole document or
/// knows exactly how big a buffer to ask again with. A partly-transferred
/// document is not representable, so no caller can parse a fragment as if it
/// were a store.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ConfigDocument<'a> {
    /// The whole merged document, as canonical `key = value` text.
    Whole(&'a str),
    /// The document did not fit the capacity the request declared. This is
    /// its whole length in bytes: ask again with at least this much.
    NeedsCapacity(usize),
}

/// Encode an [`AppDataRequest::ConfigRead`] reply carrying `document`, whose
/// bytes are sent only if they fit the `capacity` the request declared.
///
/// # Errors
///
/// * [`Errno::LengthOutOfRange`] — `document` is longer than
///   [`APPDATA_DOCUMENT_MAX`].
/// * [`Errno::BufferTooSmall`] — `out` cannot hold the reply.
pub fn encode_document_reply(
    document: &str,
    capacity: u32,
    out: &mut [u8],
) -> Result<usize, Errno> {
    if document.len() > APPDATA_DOCUMENT_MAX {
        return Err(Errno::LengthOutOfRange);
    }
    let header = crate::reply::STATUS_REPLY_LEN + APPDATA_DOCUMENT_HEADER_LEN;
    let body = if document.len() <= capacity as usize {
        document.len()
    } else {
        0
    };
    let total = header + body;
    if out.len() < total {
        return Err(Errno::BufferTooSmall);
    }
    out[..crate::reply::STATUS_REPLY_LEN]
        .copy_from_slice(&crate::reply::encode_status_reply(Ok(())));
    put_u32(
        out,
        crate::reply::STATUS_REPLY_LEN,
        // Bounded by `APPDATA_DOCUMENT_MAX` above, which is far inside a u32.
        u32::try_from(document.len()).map_err(|_| Errno::LengthOutOfRange)?,
    );
    out[header..total].copy_from_slice(&document.as_bytes()[..body]);
    Ok(total)
}

/// Decode an [`AppDataRequest::ConfigRead`] reply.
///
/// # Errors
///
/// * The daemon's own refusal, decoded from the status word.
/// * [`Errno::BufferTooSmall`] — the frame is shorter than its header.
/// * [`Errno::BadMagic`] — a body that is neither empty nor the whole
///   document the header declares.
/// * [`Errno::LengthOutOfRange`] — a declared length beyond
///   [`APPDATA_DOCUMENT_MAX`].
/// * [`Errno::OutOfRange`] — a document that is not valid UTF-8.
pub fn decode_document_reply(bytes: &[u8]) -> Result<ConfigDocument<'_>, Errno> {
    crate::reply::decode_status_reply(bytes)?;
    let header = crate::reply::STATUS_REPLY_LEN + APPDATA_DOCUMENT_HEADER_LEN;
    if bytes.len() < header {
        return Err(Errno::BufferTooSmall);
    }
    let declared = usize::try_from(read_u32(bytes, crate::reply::STATUS_REPLY_LEN))
        .map_err(|_| Errno::LengthOutOfRange)?;
    if declared > APPDATA_DOCUMENT_MAX {
        return Err(Errno::LengthOutOfRange);
    }
    let body = &bytes[header..];
    if body.is_empty() && declared > 0 {
        return Ok(ConfigDocument::NeedsCapacity(declared));
    }
    // Anything else must be the document entire: a short or over-long body is
    // not the answer the header described.
    if body.len() != declared {
        return Err(Errno::BadMagic);
    }
    core::str::from_utf8(body)
        .map(ConfigDocument::Whole)
        .map_err(|_| Errno::OutOfRange)
}

#[cfg(test)]
#[path = "appdata_ipc_tests.rs"]
mod tests;
