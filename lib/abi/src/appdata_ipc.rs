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
//! # Three scopes, and the one request shape that names another app
//!
//! An application's store holds a [`ConfigScope::Private`] document — the
//! user's settings for that app, which nothing else may read — a
//! [`ConfigScope::Public`] one, which the app publishes for other
//! applications to read, and a **sealed** one, encrypted at rest under a key
//! derived per (account, application) and reached only through the
//! `Vault` operations. Every *configuration* operation carries the scope; none
//! of them carries a bundle identifier, because the daemon derives which store
//! to open from the [`Origin`](crate::Origin) the kernel attests for the
//! calling task. So there is no request shape by which an app can claim to be
//! another app.
//!
//! The single exception is [`AppDataRequest::PublicRead`], which names a
//! *foreign* application's identifier — and it is a distinct operation
//! precisely so that it cannot carry a scope at all: a request that names
//! another app is public by construction, and the private scope is
//! unreachable across applications because no frame can ask for it. A caller
//! with no attested app identity — a kernel principal, a boot-floor program
//! with no signed manifest, a parser-sandbox child — has no store and is
//! refused whichever operation it sends.
//!
//! The sealed scope is deliberately **not** a [`ConfigScope`] variant. It is
//! reached by [`AppDataRequest::VaultRead`], [`AppDataRequest::VaultSet`], and
//! [`AppDataRequest::VaultUnset`], none of which carries a scope field — so no
//! configuration frame can name a secret and no vault frame can name a
//! configuration document, in either direction, by construction rather than by
//! a check. It also has no foreign counterpart at all, which is what makes
//! "one application reads another's secrets" unrepresentable rather than
//! refused.
//!
//! # One read for the whole document, not one per key
//!
//! [`AppDataRequest::ConfigRead`] answers with the caller's **whole** merged
//! document for one scope — for the private scope, the machine-wide policy
//! layer with the user's own overrides applied — as canonical `key = value`
//! text the client parses with the one format engine. So an application's
//! start-up costs one call, one store read, and one parse however many
//! settings it goes on to consult; a per-key read would have cost the daemon a
//! file read and a parse *per key*, and a client that reads one setting pays
//! no more than one that reads forty.
//!
//! The reply is a whole document or nothing: the request declares the reply
//! buffer the caller has, and a document that does not fit comes back as the
//! byte count it needs ([`ConfigDocument::NeedsCapacity`]) with no body at
//! all. A caller therefore never parses a truncated prefix, and never reads a
//! document assembled out of two different snapshots — every answer is one
//! point-in-time view.
//!
//! # Staged writes, one atomic publish per scope
//!
//! [`AppDataRequest::ConfigSet`] and [`AppDataRequest::ConfigUnset`] *stage* a
//! change against the caller's own session with the daemon;
//! [`AppDataRequest::ConfigCommit`] publishes the staged document whole. A
//! caller that never commits changes nothing on disk, and a crash mid-publish
//! leaves either the old document or the new one — never a torn one. A staged
//! change is visible to the staging caller's own [`AppDataRequest::ConfigRead`]
//! and to no other principal, so a settings sheet reads back what it just set.
//!
//! Staging and committing are **per scope**: a commit publishes one document,
//! because one document is what a rename can replace atomically. Naming a
//! scope on the commit is what keeps that honest rather than implying an
//! atomicity across two files that no filesystem offers.
//!
//! # A sealed write is immediate, and has no commit
//!
//! [`AppDataRequest::VaultSet`] and [`AppDataRequest::VaultUnset`] carry no
//! staging and there is no `VaultCommit`: the daemon opens the sealed
//! document, applies the one change, re-seals it, and publishes it before it
//! replies. Plaintext secret material therefore exists in the daemon for the
//! span of one request instead of for the life of a staging session, and
//! because the daemon serves requests one at a time the whole
//! read-modify-seal-publish is atomic — so two processes of one application
//! sealing different secrets cannot lose each other's, which a
//! stage-then-commit pair would allow.
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
//! 12  u32  capacity    reads only: the caller's reply buffer
//! 16  u8   scope       configuration operations only
//! 17  u8   bundle_len  PublicRead only
//! 18  bundle bytes     bundle_len bytes of UTF-8
//! ..  key    bytes     key_len    bytes of UTF-8
//! ..  value  bytes     value_len  bytes of UTF-8
//! ```
//!
//! The record is variable-width rather than padded to its widest form: a
//! `ConfigRead` is eighteen bytes and a `ConfigCommit` is eighteen, so padding
//! every request to the width of the longest value would put a kilobyte of
//! zeroes on the hot settings path for nothing.
//!
//! Every decode fails closed. An unknown magic, version, operation, or scope,
//! a declared length that does not match the record, a field an operation does
//! not use left non-zero, non-UTF-8 text, an identifier outside the bundle-id
//! grammar, or a trailing byte past the payload all refuse rather than guess.
//!
//! The **grammar** of a key and a value is not judged here: it has one home,
//! the `key = value` engine in `lib/appconf`, and the daemon applies it
//! through that engine's own validators. This module bounds the transport —
//! the record's shape, its lengths, and its text encoding — exactly as the
//! `users_admin` request codec bounds a record whose field rules live in
//! `lib/users`. The one grammar it *does* apply is
//! [`validate_bundle_id`](crate::validate_bundle_id), because an identifier
//! naming a directory in a store is a path component crossing a trust
//! boundary, and that grammar lives in this crate.

use crate::appinfo::BUNDLE_ID_MAX;
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

/// Byte length of the fixed request header preceding the bundle identifier,
/// the key, and the value.
pub const APPDATA_HEADER_LEN: usize = 18;

/// The widest payload any one operation may carry past the header.
///
/// No operation carries a bundle identifier *and* a key: an own-store request
/// names no app, and a foreign read names no setting. So the widest record is
/// whichever of the two shapes is longer, stated as such rather than as a sum
/// that would silently over-allocate every buffer in the system.
const APPDATA_WIDEST_PAYLOAD: usize = if BUNDLE_ID_MAX > APPDATA_KEY_MAX + APPDATA_VALUE_MAX {
    BUNDLE_ID_MAX
} else {
    APPDATA_KEY_MAX + APPDATA_VALUE_MAX
};

/// Maximum request, in bytes, the [`APPDATA_ENDPOINT`] accepts: the header
/// plus the widest payload a record may carry.
pub const APPDATA_MAX_REQUEST: usize = APPDATA_HEADER_LEN + APPDATA_WIDEST_PAYLOAD;

/// Byte offset of the operation discriminant.
const OP_OFFSET: usize = 6;
/// Byte offset of the key length prefix.
const KEY_LEN_OFFSET: usize = 8;
/// Byte offset of the value length prefix.
const VALUE_LEN_OFFSET: usize = 10;
/// Byte offset of the reply-buffer capacity.
const CAPACITY_OFFSET: usize = 12;
/// Byte offset of the scope discriminant.
const SCOPE_OFFSET: usize = 16;
/// Byte offset of the bundle-identifier length prefix.
const BUNDLE_LEN_OFFSET: usize = 17;

/// Wire discriminant of [`AppDataRequest::ConfigRead`].
const OP_CONFIG_READ: u16 = 1;
/// Wire discriminant of [`AppDataRequest::ConfigSet`].
const OP_CONFIG_SET: u16 = 2;
/// Wire discriminant of [`AppDataRequest::ConfigUnset`].
const OP_CONFIG_UNSET: u16 = 3;
/// Wire discriminant of [`AppDataRequest::ConfigCommit`].
const OP_CONFIG_COMMIT: u16 = 4;
/// Wire discriminant of [`AppDataRequest::PublicRead`].
const OP_PUBLIC_READ: u16 = 5;
/// Wire discriminant of [`AppDataRequest::VaultRead`].
const OP_VAULT_READ: u16 = 6;
/// Wire discriminant of [`AppDataRequest::VaultSet`].
const OP_VAULT_SET: u16 = 7;
/// Wire discriminant of [`AppDataRequest::VaultUnset`].
const OP_VAULT_UNSET: u16 = 8;

/// Wire discriminant of [`ConfigScope::Private`].
const SCOPE_PRIVATE: u8 = 1;
/// Wire discriminant of [`ConfigScope::Public`].
const SCOPE_PUBLIC: u8 = 2;

/// Which of an application's own configuration documents an operation acts on.
///
/// The two differ in *who may read them*, which is the whole of the
/// distinction: nothing but the app itself ever reads its private scope, and
/// any application may read its public one.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ConfigScope {
    /// The user's own settings for this application. Readable and writable by
    /// the application alone; no request shape lets any other application
    /// name it.
    Private,
    /// What this application publishes about itself for others to read.
    /// Writable by the application alone, readable by any application through
    /// [`AppDataRequest::PublicRead`].
    Public,
}

impl ConfigScope {
    /// The wire discriminant of this scope.
    ///
    /// Neither is zero, so an all-zero frame cannot decode as a scoped
    /// operation: a request that forgot to name a scope is refused rather than
    /// silently served the private one.
    #[must_use]
    pub const fn as_wire(self) -> u8 {
        match self {
            Self::Private => SCOPE_PRIVATE,
            Self::Public => SCOPE_PUBLIC,
        }
    }

    /// The scope `wire` names.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for anything outside the closed set, zero
    /// included.
    pub const fn from_wire(wire: u8) -> Result<Self, Errno> {
        match wire {
            SCOPE_PRIVATE => Ok(Self::Private),
            SCOPE_PUBLIC => Ok(Self::Public),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// One app-data operation.
///
/// Every variant but [`Self::PublicRead`] acts on the store the daemon derived
/// from the caller's kernel-attested app identity, and names no store at all.
/// `PublicRead` is the one shape that names another application — and it can
/// reach nothing but that application's published document.
///
/// The `Vault*` variants reach the sealed scope. They carry no scope field and
/// have no foreign counterpart, so a configuration frame cannot name a secret,
/// a vault frame cannot name a configuration document, and no frame at all
/// reaches another application's secrets.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AppDataRequest<'a> {
    /// Read the caller's whole merged document for one of its own scopes.
    ConfigRead {
        /// Which of the caller's own documents to read.
        scope: ConfigScope,
        /// How many document bytes the caller's reply buffer can hold, past
        /// the frame's own header. A document longer than this comes back as
        /// [`ConfigDocument::NeedsCapacity`] and no body, so the caller can
        /// size a buffer exactly and ask again rather than parse a fragment.
        /// Zero is legal and asks only for the length.
        capacity: u32,
    },
    /// Stage `key = value` in one of the caller's own scopes, to be published
    /// by [`Self::ConfigCommit`] for that scope.
    ConfigSet {
        /// Which of the caller's own documents to stage against.
        scope: ConfigScope,
        /// The key to write.
        key: &'a str,
        /// Its new value. May be empty: a key set to nothing is a key that
        /// is set, and is distinct from one that is absent.
        value: &'a str,
    },
    /// Stage the removal of `key` from one of the caller's own scopes.
    ConfigUnset {
        /// Which of the caller's own documents to stage against.
        scope: ConfigScope,
        /// The key to remove. Removing a key the store does not carry stages
        /// nothing and is not an error.
        key: &'a str,
    },
    /// Publish every change staged against one scope as one atomic document
    /// replacement. Edits staged against the caller's other scopes are
    /// untouched.
    ConfigCommit {
        /// Which of the caller's own documents to publish.
        scope: ConfigScope,
    },
    /// Read another application's published document.
    ///
    /// The only operation that names an application, and it can name nothing
    /// but the public scope: there is no scope field to set, so the private
    /// scope is unreachable across applications by construction. An
    /// application that has published nothing — or whose store cannot be
    /// attested — answers the empty document, so this is not an oracle for
    /// anything but what an app chose to publish.
    PublicRead {
        /// The signed bundle identifier of the application to read. Validated
        /// against [`validate_bundle_id`](crate::validate_bundle_id) on
        /// decode, because it becomes a path component in the store tree.
        bundle_id: &'a str,
        /// As [`Self::ConfigRead`]'s capacity.
        capacity: u32,
    },
    /// Read the caller's whole **sealed** document.
    ///
    /// The sealed scope carries no scope field and has no foreign
    /// counterpart: no frame names it but the caller's own, and no frame
    /// reaches another application's. A caller that has sealed nothing reads
    /// the empty document; a sealed document that fails authentication is
    /// refused, never reported as empty, because "your secrets are damaged"
    /// and "you have no secrets" must not look alike.
    VaultRead {
        /// As [`Self::ConfigRead`]'s capacity.
        capacity: u32,
    },
    /// Seal `key = value` into the caller's sealed document, immediately.
    ///
    /// Unlike a configuration write this is **not** staged: the daemon opens
    /// the sealed document, applies the one change, re-seals, and publishes it
    /// before it replies. Two reasons, and both are the sealed scope's alone.
    /// Plaintext secret material then exists in the daemon only for the span
    /// of one request rather than for the life of a staging session; and
    /// because the daemon serves requests one at a time, the whole
    /// read-modify-seal-publish is atomic, so two processes of one application
    /// writing different secrets cannot lose each other's — where a
    /// stage-then-commit pair can.
    VaultSet {
        /// The key to seal.
        key: &'a str,
        /// Its new value. May be empty: a key sealed to nothing is a key that
        /// is set, and is distinct from one that is absent.
        value: &'a str,
    },
    /// Remove `key` from the caller's sealed document, immediately.
    ///
    /// A key the sealed document does not carry is removed by writing nothing
    /// at all, so this cannot be used to bring a sealed document — or a
    /// store — into existence.
    VaultUnset {
        /// The key to remove.
        key: &'a str,
    },
}

impl<'a> AppDataRequest<'a> {
    /// Encoded length, in bytes, of this request.
    #[must_use]
    pub const fn wire_len(&self) -> usize {
        let (bundle, key, value) = self.payload();
        APPDATA_HEADER_LEN + bundle.len() + key.len() + value.len()
    }

    /// The bundle identifier, key, and value this request carries, each empty
    /// when the operation has none.
    const fn payload(&self) -> (&'a str, &'a str, &'a str) {
        match *self {
            Self::ConfigUnset { key, .. } | Self::VaultUnset { key } => ("", key, ""),
            Self::ConfigSet { key, value, .. } | Self::VaultSet { key, value } => ("", key, value),
            Self::PublicRead { bundle_id, .. } => (bundle_id, "", ""),
            Self::ConfigRead { .. } | Self::ConfigCommit { .. } | Self::VaultRead { .. } => {
                ("", "", "")
            }
        }
    }

    /// The wire discriminant of this operation.
    const fn op(&self) -> u16 {
        match *self {
            Self::ConfigRead { .. } => OP_CONFIG_READ,
            Self::ConfigSet { .. } => OP_CONFIG_SET,
            Self::ConfigUnset { .. } => OP_CONFIG_UNSET,
            Self::ConfigCommit { .. } => OP_CONFIG_COMMIT,
            Self::PublicRead { .. } => OP_PUBLIC_READ,
            Self::VaultRead { .. } => OP_VAULT_READ,
            Self::VaultSet { .. } => OP_VAULT_SET,
            Self::VaultUnset { .. } => OP_VAULT_UNSET,
        }
    }

    /// The scope byte this operation carries, zero for the one that names none.
    const fn scope_wire(&self) -> u8 {
        match *self {
            Self::ConfigRead { scope, .. }
            | Self::ConfigSet { scope, .. }
            | Self::ConfigUnset { scope, .. }
            | Self::ConfigCommit { scope } => scope.as_wire(),
            Self::PublicRead { .. }
            | Self::VaultRead { .. }
            | Self::VaultSet { .. }
            | Self::VaultUnset { .. } => 0,
        }
    }

    /// The reply-buffer capacity this operation declares, zero for the ones
    /// that read nothing.
    const fn capacity(&self) -> u32 {
        match *self {
            Self::ConfigRead { capacity, .. }
            | Self::PublicRead { capacity, .. }
            | Self::VaultRead { capacity } => capacity,
            Self::ConfigSet { .. }
            | Self::ConfigUnset { .. }
            | Self::ConfigCommit { .. }
            | Self::VaultSet { .. }
            | Self::VaultUnset { .. } => 0,
        }
    }

    /// Encode `self` little-endian into `out`, returning the bytes written.
    ///
    /// # Errors
    ///
    /// * [`Errno::LengthOutOfRange`] — a key longer than
    ///   [`APPDATA_KEY_MAX`], a value longer than [`APPDATA_VALUE_MAX`], a
    ///   bundle identifier longer than
    ///   [`BUNDLE_ID_MAX`], or a capacity
    ///   beyond [`APPDATA_DOCUMENT_MAX`].
    /// * [`Errno::BufferTooSmall`] — `out` cannot hold the record.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Errno> {
        let (bundle, key, value) = self.payload();
        if key.len() > APPDATA_KEY_MAX
            || value.len() > APPDATA_VALUE_MAX
            || bundle.len() > BUNDLE_ID_MAX
        {
            return Err(Errno::LengthOutOfRange);
        }
        if self.capacity() as usize > APPDATA_DOCUMENT_MAX {
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
        // Every length is bounded above, so none of these truncates.
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
        put_u32(out, CAPACITY_OFFSET, self.capacity());
        out[SCOPE_OFFSET] = self.scope_wire();
        out[BUNDLE_LEN_OFFSET] = u8::try_from(bundle.len()).map_err(|_| Errno::LengthOutOfRange)?;
        let bundle_end = APPDATA_HEADER_LEN + bundle.len();
        let key_end = bundle_end + key.len();
        out[APPDATA_HEADER_LEN..bundle_end].copy_from_slice(bundle.as_bytes());
        out[bundle_end..key_end].copy_from_slice(key.as_bytes());
        out[key_end..total].copy_from_slice(value.as_bytes());
        Ok(total)
    }

    /// Decode a request from `bytes`, failing closed on anything malformed.
    ///
    /// The borrowed identifier, key, and value point into `bytes`, so a decode
    /// copies nothing.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `bytes` is shorter than the header, or
    ///   than the payload the header declares.
    /// * [`Errno::BadMagic`] — wrong magic, a trailing byte past the
    ///   declared payload, or a field the operation does not use left
    ///   non-zero.
    /// * [`Errno::AbiVersionUnsupported`] — not `appdata-v1`.
    /// * [`Errno::OutOfRange`] — an operation or scope outside its closed
    ///   set, text that is not valid UTF-8, or an identifier outside the
    ///   bundle-id grammar.
    /// * [`Errno::LengthOutOfRange`] — a declared length beyond its bound, a
    ///   required key or identifier that is empty, or a capacity beyond
    ///   [`APPDATA_DOCUMENT_MAX`].
    pub fn decode(bytes: &'a [u8]) -> Result<Self, Errno> {
        let record = Record::split(bytes)?;
        match record.op {
            OP_CONFIG_READ => {
                record.without_bundle()?;
                record.without_key()?;
                record.without_value()?;
                Ok(Self::ConfigRead {
                    scope: record.scope()?,
                    capacity: record.capacity()?,
                })
            }
            OP_CONFIG_SET => {
                record.without_bundle()?;
                record.without_capacity()?;
                Ok(Self::ConfigSet {
                    scope: record.scope()?,
                    key: record.key()?,
                    value: record.value,
                })
            }
            OP_CONFIG_UNSET => {
                record.without_bundle()?;
                record.without_capacity()?;
                record.without_value()?;
                Ok(Self::ConfigUnset {
                    scope: record.scope()?,
                    key: record.key()?,
                })
            }
            OP_CONFIG_COMMIT => {
                record.without_bundle()?;
                record.without_capacity()?;
                record.without_key()?;
                record.without_value()?;
                Ok(Self::ConfigCommit {
                    scope: record.scope()?,
                })
            }
            OP_PUBLIC_READ => {
                record.without_scope()?;
                record.without_key()?;
                record.without_value()?;
                Ok(Self::PublicRead {
                    bundle_id: record.bundle_id()?,
                    capacity: record.capacity()?,
                })
            }
            OP_VAULT_READ => {
                record.without_scope()?;
                record.without_bundle()?;
                record.without_key()?;
                record.without_value()?;
                Ok(Self::VaultRead {
                    capacity: record.capacity()?,
                })
            }
            OP_VAULT_SET => {
                record.without_scope()?;
                record.without_bundle()?;
                record.without_capacity()?;
                Ok(Self::VaultSet {
                    key: record.key()?,
                    value: record.value,
                })
            }
            OP_VAULT_UNSET => {
                record.without_scope()?;
                record.without_bundle()?;
                record.without_capacity()?;
                record.without_value()?;
                Ok(Self::VaultUnset { key: record.key()? })
            }
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// One request frame split into its fields, before any operation has claimed
/// them.
///
/// Splitting and *judging* are separate steps on purpose: the split bounds
/// every length against the frame, and each operation then states exactly
/// which fields it uses — so a field an operation does not use is checked
/// empty by the same rule everywhere, and a new operation cannot silently
/// tolerate a field it ignores.
struct Record<'a> {
    op: u16,
    scope: u8,
    capacity: u32,
    bundle: &'a str,
    key: &'a str,
    value: &'a str,
}

impl<'a> Record<'a> {
    /// Split `bytes` into its fields, bounding every declared length.
    fn split(bytes: &'a [u8]) -> Result<Self, Errno> {
        if bytes.len() < APPDATA_HEADER_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if read_u32(bytes, 0) != APPDATA_REQUEST_MAGIC {
            return Err(Errno::BadMagic);
        }
        if read_u16(bytes, 4) != APPDATA_VERSION_V1 {
            return Err(Errno::AbiVersionUnsupported);
        }
        let bundle_len = usize::from(bytes[BUNDLE_LEN_OFFSET]);
        let key_len = usize::from(read_u16(bytes, KEY_LEN_OFFSET));
        let value_len = usize::from(read_u16(bytes, VALUE_LEN_OFFSET));
        if bundle_len > BUNDLE_ID_MAX || key_len > APPDATA_KEY_MAX || value_len > APPDATA_VALUE_MAX
        {
            return Err(Errno::LengthOutOfRange);
        }
        let bundle_end = APPDATA_HEADER_LEN + bundle_len;
        let key_end = bundle_end + key_len;
        let total = key_end + value_len;
        if bytes.len() < total {
            return Err(Errno::BufferTooSmall);
        }
        // A request is exactly one record: a trailing byte means the frame
        // was not the one the sender described.
        if bytes.len() > total {
            return Err(Errno::BadMagic);
        }
        Ok(Self {
            op: read_u16(bytes, OP_OFFSET),
            scope: bytes[SCOPE_OFFSET],
            capacity: read_u32(bytes, CAPACITY_OFFSET),
            bundle: text(&bytes[APPDATA_HEADER_LEN..bundle_end])?,
            key: text(&bytes[bundle_end..key_end])?,
            value: text(&bytes[key_end..total])?,
        })
    }

    /// The scope this record names.
    fn scope(&self) -> Result<ConfigScope, Errno> {
        ConfigScope::from_wire(self.scope)
    }

    /// The reply capacity this record declares, bounded by the widest document
    /// that can answer it.
    fn capacity(&self) -> Result<u32, Errno> {
        if self.capacity as usize > APPDATA_DOCUMENT_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        Ok(self.capacity)
    }

    /// The non-empty key this record carries.
    fn key(&self) -> Result<&'a str, Errno> {
        if self.key.is_empty() {
            return Err(Errno::LengthOutOfRange);
        }
        Ok(self.key)
    }

    /// The bundle identifier this record carries, inside the one grammar every
    /// consumer applies to an identifier that becomes a path component.
    fn bundle_id(&self) -> Result<&'a str, Errno> {
        crate::validate_bundle_id(self.bundle)?;
        Ok(self.bundle)
    }

    /// Refuse a record whose operation names no application.
    fn without_bundle(&self) -> Result<(), Errno> {
        Self::absent(self.bundle.is_empty())
    }

    /// Refuse a record whose operation names no key.
    fn without_key(&self) -> Result<(), Errno> {
        Self::absent(self.key.is_empty())
    }

    /// Refuse a record whose operation carries no value.
    fn without_value(&self) -> Result<(), Errno> {
        Self::absent(self.value.is_empty())
    }

    /// Refuse a record whose operation reads nothing.
    fn without_capacity(&self) -> Result<(), Errno> {
        Self::absent(self.capacity == 0)
    }

    /// Refuse a record whose operation names no scope.
    fn without_scope(&self) -> Result<(), Errno> {
        Self::absent(self.scope == 0)
    }

    /// A field an operation does not use must be empty; anything else is a
    /// frame that does not mean what its operation says.
    const fn absent(empty: bool) -> Result<(), Errno> {
        if empty {
            Ok(())
        } else {
            Err(Errno::BadMagic)
        }
    }
}

/// `bytes` as UTF-8 text, refused rather than replaced when it is not.
fn text(bytes: &[u8]) -> Result<&str, Errno> {
    core::str::from_utf8(bytes).map_err(|_| Errno::OutOfRange)
}

/// Byte length of the document-reply header following the shared status word:
/// the document's whole length, whether or not its bytes fitted.
pub const APPDATA_DOCUMENT_HEADER_LEN: usize = 4;

/// Maximum reply, in bytes, the [`APPDATA_ENDPOINT`] produces — the widest
/// [`AppDataRequest::ConfigRead`] answer. The value the endpoint is created
/// with, so a reply can never be refused for want of room.
pub const APPDATA_MAX_REPLY: usize =
    crate::reply::STATUS_REPLY_LEN + APPDATA_DOCUMENT_HEADER_LEN + APPDATA_DOCUMENT_MAX;

/// What a document read answered.
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

/// Encode a document reply carrying `document`, whose bytes are sent only if
/// they fit the `capacity` the request declared.
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

/// Decode a document reply.
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
    text(body).map(ConfigDocument::Whole)
}

#[cfg(test)]
#[path = "appdata_ipc_tests.rs"]
mod tests;
