//! The app-data channel: the reserved rendezvous every application reaches its
//! own per-app configuration store through (`plans/APPDATA.md` §3.6).
//!
//! Four properties of the request shapes, each holding by construction rather
//! than by a check the daemon performs:
//!
//! - No *configuration* operation carries a bundle identifier. The daemon
//!   derives which store to open from the [`Origin`](crate::Origin) the kernel
//!   attests for the calling task, so no frame can claim to be another
//!   application. A caller with no attested app identity has no store and is
//!   refused whichever operation it sent.
//! - [`AppDataRequest::PublicRead`] is the one operation that names a foreign
//!   application, and it is a distinct operation precisely so that it carries
//!   no scope field at all: another application's private document is
//!   unreachable because no frame can ask for it.
//! - The sealed scope is deliberately **not** a [`ConfigScope`] variant.
//!   [`AppDataRequest::VaultRead`], [`AppDataRequest::VaultSet`], and
//!   [`AppDataRequest::VaultUnset`] carry no scope field and have no foreign
//!   counterpart, so no configuration frame can name a secret, no vault frame
//!   can name a configuration document, and "one application reads another's
//!   secrets" is unrepresentable rather than refused.
//! - [`AppDataRequest::TempCreate`] names nothing on the way in and no
//!   operation opens a temporary file by name, so an application can only hold
//!   scratch it just created.
//!
//! [`AppDataRequest::ConfigRead`] answers a **whole** merged document or
//! nothing: a document that exceeds the declared reply capacity comes back as
//! the byte count it needs ([`ConfigDocument::NeedsCapacity`]) with no body, so
//! a caller never parses a truncated prefix and never assembles a store out of
//! two snapshots. [`AppDataRequest::ConfigSet`] and
//! [`AppDataRequest::ConfigUnset`] stage against the caller's own session and
//! [`AppDataRequest::ConfigCommit`] publishes one scope, because one document
//! is what a rename can replace atomically. There is no `VaultCommit`: the
//! daemon seals a vault write and publishes it before it replies.
//!
//! [`AppDataRequest::BlobOpen`] and [`AppDataRequest::TempCreate`] answer an
//! `fd_grant` handle rather than bytes — the IPC payload ceiling is far below
//! what a blob holds, so proxying them is impossible rather than merely slower.
//! The delegation is what bounds the authority handed over: it carries the
//! access the mode asked for and, for a writable blob, a byte-extent ceiling the
//! kernel enforces — so an application cannot grow a file past
//! [`APPDATA_BULK_FILE_MAX_BYTES`] however it uses the descriptor.
//! [`APPDATA_BLOB_MAX_COUNT`] and [`APPDATA_TEMP_MAX_COUNT`] bound the other
//! dimension, the file count, at admission. All three are fixed containment
//! bounds rather than capacities, so a larger machine does not raise them.
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
//! 17  u8   mode        BlobOpen only
//! 18  u8   name_len    the operations that name something in the store
//! 19  u8   reserved    zero
//! 20  name   bytes     name_len  bytes of UTF-8
//! ..  key    bytes     key_len   bytes of UTF-8
//! ..  value  bytes     value_len bytes of UTF-8
//! ```
//!
//! The **name** slot carries whichever single store name the operation names —
//! a foreign bundle identifier, a blob name, a temporary file's for
//! [`AppDataRequest::TempRelease`] — and each validates it under the grammar
//! for its own kind. All are one path component
//! in a store the daemon composes, so they share one width and one character
//! grammar; what they do not share is a request shape, so no frame can name an
//! application where a file belongs or the reverse. The record is
//! variable-width rather than padded to its widest form, so a twenty-byte
//! `ConfigRead` does not carry a kilobyte of zeroes on the hot settings path.
//!
//! Every decode fails closed. An unknown magic, version, operation, scope, or
//! blob mode, a declared length that does not match the record, a field an
//! operation does not use left non-zero, non-UTF-8 text, a name outside its
//! grammar, or a trailing byte past the payload all refuse rather than guess.
//!
//! The grammar of a key and a value is not judged here: it has one home, the
//! `key = value` engine in `lib/appconf`, and the daemon applies it through
//! that engine's own validators. This module bounds the transport — the
//! record's shape, its lengths, and its text encoding. The grammars it *does*
//! apply are the two store-name ones
//! ([`validate_bundle_id`](crate::validate_bundle_id),
//! [`validate_bulk_name`]), because a name that becomes a path component in a
//! store is crossing a trust boundary and that grammar lives in this crate.
//!
//! What the daemon does with each request, and why the store is a service at
//! all, are `docs/src/userland/confd.md`.

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

/// Maximum length, in bytes, of a **store name** on the wire: a foreign
/// application's bundle identifier, or one of the caller's own blob names.
///
/// One width for both, because both are one path component in a store the
/// service composes and neither has a reason to be wider than the other. It is
/// the bundle-identifier bound, so the identifier that keys a store and the
/// name of a file inside one cannot disagree about how long a name may be.
pub const APPDATA_NAME_MAX: usize = BUNDLE_ID_MAX;

/// Maximum number of blobs one application may hold in one account.
///
/// A fixed containment bound, not a capacity. Two things turn on it: an
/// application's private working set is a handful of named objects — an index,
/// a cache, a queue — so a store with more than this is a runaway writer
/// rather than a workload; and it is what makes a whole
/// [`AppDataRequest::BlobList`] answer fit one reply, so a listing is never
/// spliced from two snapshots.
pub const APPDATA_BLOB_MAX_COUNT: usize = 64;

/// Maximum number of temporary files one application may hold in one account
/// at once.
///
/// A fixed containment bound like [`APPDATA_BLOB_MAX_COUNT`], and deliberately
/// tighter: a scratch file that still exists is either live or leaked, and the
/// live set of an application is a handful per running instance across a
/// handful of instances. [`AppDataRequest::TempRelease`] frees a slot at once
/// and a boot frees them all, so an application that reaches this has kept
/// scratch files it never finished with rather than met a workload the scope
/// is too small for.
pub const APPDATA_TEMP_MAX_COUNT: usize = 32;

/// Maximum length, in bytes, of one file in an application's bulk store — a
/// blob or a temporary file alike. It is the extent ceiling the descriptor
/// grant carries, enforced by the kernel on every write and truncate through
/// it.
///
/// One figure for both scopes because it answers one question — how big may a
/// file the user can neither list nor delete become — and that question does
/// not turn on whether the file outlives the boot.
///
/// A fixed containment bound, not a capacity, and deliberately not derived
/// from discovered hardware: it bounds what one application may take from the
/// *user's* volume, and there is no honest hardware quantity to scale a disk
/// bound by. Every open file is bounded by this one figure, so an
/// application's bulk store is bounded by
/// ([`APPDATA_BLOB_MAX_COUNT`] + [`APPDATA_TEMP_MAX_COUNT`]) × this — a hard
/// bound rather than an admission estimate.
///
/// What it is sized for is a working set: a mail or search index, a thumbnail
/// cache, a queue, a staged download. Data that genuinely outgrows it is the
/// *user's* data and belongs in the user's own files, where the file manager
/// lists it, backup covers it, and the user can delete it — not hidden in a
/// store the user cannot reach.
pub const APPDATA_BULK_FILE_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Validate the name of one file in an application's bulk store: a blob name,
/// or the name the service minted for a temporary file.
///
/// The name becomes one path component inside the application's own store
/// directory, so it shares the one store-name grammar
/// ([`validate_store_name`](crate::appinfo::validate_store_name)) with a
/// bundle identifier and is bounded by [`APPDATA_NAME_MAX`]: nothing that
/// could traverse, hide, case-fold into another name, or carry a control
/// character can be spelled at all.
///
/// # Errors
///
/// As [`validate_store_name`](crate::appinfo::validate_store_name).
pub fn validate_bulk_name(name: &str) -> Result<(), Errno> {
    crate::appinfo::validate_store_name(name, APPDATA_NAME_MAX)
}

/// Byte length of the fixed request header preceding the store name, the key,
/// and the value.
pub const APPDATA_HEADER_LEN: usize = 20;

/// The widest payload any one operation may carry past the header.
///
/// No operation carries a store name *and* a key: an own-store configuration
/// request names nothing in the store, and a foreign read or a blob operation
/// names no setting. So the widest record is whichever of the two shapes is
/// longer, stated as such rather than as a sum that would silently
/// over-allocate every buffer in the system.
const APPDATA_WIDEST_PAYLOAD: usize = if APPDATA_NAME_MAX > APPDATA_KEY_MAX + APPDATA_VALUE_MAX {
    APPDATA_NAME_MAX
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
/// Byte offset of the blob-access-mode discriminant.
const MODE_OFFSET: usize = 17;
/// Byte offset of the store-name length prefix.
const NAME_LEN_OFFSET: usize = 18;
/// Byte offset of the reserved header byte, which must be zero.
const RESERVED_OFFSET: usize = 19;

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
/// Wire discriminant of [`AppDataRequest::BlobOpen`].
const OP_BLOB_OPEN: u16 = 9;
/// Wire discriminant of [`AppDataRequest::BlobDelete`].
const OP_BLOB_DELETE: u16 = 10;
/// Wire discriminant of [`AppDataRequest::BlobList`].
const OP_BLOB_LIST: u16 = 11;
/// Wire discriminant of [`AppDataRequest::QuotaGet`].
const OP_QUOTA_GET: u16 = 12;
/// Wire discriminant of [`AppDataRequest::TempCreate`].
const OP_TEMP_CREATE: u16 = 13;
/// Wire discriminant of [`AppDataRequest::TempRelease`].
const OP_TEMP_RELEASE: u16 = 14;

/// Wire discriminant of [`ConfigScope::Private`].
const SCOPE_PRIVATE: u8 = 1;
/// Wire discriminant of [`ConfigScope::Public`].
const SCOPE_PUBLIC: u8 = 2;

/// Wire discriminant of [`BlobMode::Read`].
const MODE_READ: u8 = 1;
/// Wire discriminant of [`BlobMode::ReadWrite`].
const MODE_READ_WRITE: u8 = 2;

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

/// How a blob descriptor is opened.
///
/// The two differ in what the delegation conveys, and in whether an absent
/// blob is brought into existence. Neither is zero, so an all-zero frame
/// cannot decode as a blob open: a request that forgot to name a mode is
/// refused rather than silently granted the wider one.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum BlobMode {
    /// Read the blob. A blob the application has not created answers
    /// [`Errno::NotFound`]: a read is never the act that brings one into
    /// existence.
    Read,
    /// Read and write the blob, creating it if the application has none of
    /// that name. Creation is carried by the mode the caller already sends
    /// rather than by a separate flag, so "create but do not write" — a
    /// combination with no meaning — is not representable.
    ///
    /// The delegation carries an extent ceiling of
    /// [`APPDATA_BULK_FILE_MAX_BYTES`], which the kernel enforces on every write
    /// and truncate through the descriptor.
    ReadWrite,
}

impl BlobMode {
    /// The wire discriminant of this mode.
    #[must_use]
    pub const fn as_wire(self) -> u8 {
        match self {
            Self::Read => MODE_READ,
            Self::ReadWrite => MODE_READ_WRITE,
        }
    }

    /// The mode `wire` names.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for anything outside the closed set, zero
    /// included.
    pub const fn from_wire(wire: u8) -> Result<Self, Errno> {
        match wire {
            MODE_READ => Ok(Self::Read),
            MODE_READ_WRITE => Ok(Self::ReadWrite),
            _ => Err(Errno::OutOfRange),
        }
    }

    /// Whether this mode conveys write access.
    #[must_use]
    pub const fn is_write(self) -> bool {
        matches!(self, Self::ReadWrite)
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
    /// Open one of the caller's own blobs and answer with a one-shot
    /// descriptor grant for it.
    ///
    /// The reply carries a handle, never bytes: the caller redeems it and
    /// then reads, writes, truncates, and maps the file directly against the
    /// kernel VFS, so the daemon is on the control path and never on the data
    /// path. The delegation is bounded — the mode's access and, when it
    /// writes, [`APPDATA_BULK_FILE_MAX_BYTES`] of extent — so direct access is not
    /// unbounded access.
    BlobOpen {
        /// The blob to open, validated against [`validate_bulk_name`] on
        /// decode because it becomes a path component in the caller's own
        /// blob directory.
        name: &'a str,
        /// What the delegation conveys, and whether an absent blob is
        /// created.
        mode: BlobMode,
    },
    /// Delete one of the caller's own blobs.
    ///
    /// Deleting a blob the caller does not have changes nothing and is not an
    /// error, so this cannot be used to probe which blobs exist by its
    /// refusal — and cannot bring a store into existence either.
    BlobDelete {
        /// The blob to delete, validated as [`Self::BlobOpen`]'s is.
        name: &'a str,
    },
    /// List the caller's own blobs with their sizes.
    ///
    /// Whole or nothing under the same capacity negotiation a document read
    /// uses, and deliberately without a cursor: the blob count is bounded, so
    /// a whole listing fits one reply and no caller can splice one out of two
    /// snapshots — where a paged listing could name a blob a later page had
    /// already deleted.
    BlobList {
        /// As [`Self::ConfigRead`]'s capacity, against
        /// [`APPDATA_BLOB_LIST_MAX`].
        capacity: u32,
    },
    /// Read the caller's own bulk usage and the ceilings it is bounded by,
    /// across both scopes of the bulk tree.
    ///
    /// What an application does with it is report a refusal in its own terms
    /// rather than failing obscurely when a write is refused: the ceilings are
    /// fixed, so a caller that knows them can say "this cache is full" instead
    /// of surfacing an errno.
    QuotaGet {},
    /// Create a **fresh** temporary file for the caller and answer with a
    /// one-shot descriptor grant for it and the name it was given.
    ///
    /// The caller names nothing: freshness without coordination is the whole
    /// of what a temporary file is for, and two instances of one application
    /// that both chose a name would overwrite each other's scratch. The
    /// service picks the name, so no request can reach a temporary file the
    /// caller did not just create — there is no operation that *opens* one.
    ///
    /// The delegation is read-write and carries an extent ceiling of
    /// [`APPDATA_BULK_FILE_MAX_BYTES`], exactly as a writable blob's does.
    TempCreate {},
    /// Delete one of the caller's own temporary files.
    ///
    /// The name is one [`Self::TempCreate`] answered with. Releasing one the
    /// caller does not hold changes nothing and is not an error, so this is
    /// neither an oracle for what the store holds nor a way to create one.
    ///
    /// An application that never releases leaves its scratch behind until the
    /// next boot, which is what [`APPDATA_TEMP_MAX_COUNT`] bounds; nothing but
    /// that application is affected either way.
    TempRelease {
        /// The temporary file to delete, validated against
        /// [`validate_bulk_name`] on decode because it becomes a path
        /// component in the caller's own temporary directory.
        name: &'a str,
    },
}

impl<'a> AppDataRequest<'a> {
    /// Encoded length, in bytes, of this request.
    #[must_use]
    pub const fn wire_len(&self) -> usize {
        let (bundle, key, value) = self.payload();
        APPDATA_HEADER_LEN + bundle.len() + key.len() + value.len()
    }

    /// The store name, key, and value this request carries, each empty when
    /// the operation has none.
    const fn payload(&self) -> (&'a str, &'a str, &'a str) {
        match *self {
            Self::ConfigUnset { key, .. } | Self::VaultUnset { key } => ("", key, ""),
            Self::ConfigSet { key, value, .. } | Self::VaultSet { key, value } => ("", key, value),
            Self::PublicRead {
                bundle_id: name, ..
            }
            | Self::BlobOpen { name, .. }
            | Self::BlobDelete { name }
            | Self::TempRelease { name } => (name, "", ""),
            Self::ConfigRead { .. }
            | Self::ConfigCommit { .. }
            | Self::VaultRead { .. }
            | Self::BlobList { .. }
            | Self::QuotaGet {}
            | Self::TempCreate {} => ("", "", ""),
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
            Self::BlobOpen { .. } => OP_BLOB_OPEN,
            Self::BlobDelete { .. } => OP_BLOB_DELETE,
            Self::BlobList { .. } => OP_BLOB_LIST,
            Self::QuotaGet {} => OP_QUOTA_GET,
            Self::TempCreate {} => OP_TEMP_CREATE,
            Self::TempRelease { .. } => OP_TEMP_RELEASE,
        }
    }

    /// The scope byte this operation carries, zero for the ones that name
    /// none.
    const fn scope_wire(&self) -> u8 {
        match *self {
            Self::ConfigRead { scope, .. }
            | Self::ConfigSet { scope, .. }
            | Self::ConfigUnset { scope, .. }
            | Self::ConfigCommit { scope } => scope.as_wire(),
            Self::PublicRead { .. }
            | Self::VaultRead { .. }
            | Self::VaultSet { .. }
            | Self::VaultUnset { .. }
            | Self::BlobOpen { .. }
            | Self::BlobDelete { .. }
            | Self::BlobList { .. }
            | Self::QuotaGet {}
            | Self::TempCreate {}
            | Self::TempRelease { .. } => 0,
        }
    }

    /// The blob-mode byte this operation carries, zero for the ones that open
    /// no blob.
    const fn mode_wire(&self) -> u8 {
        match *self {
            Self::BlobOpen { mode, .. } => mode.as_wire(),
            Self::ConfigRead { .. }
            | Self::ConfigSet { .. }
            | Self::ConfigUnset { .. }
            | Self::ConfigCommit { .. }
            | Self::PublicRead { .. }
            | Self::VaultRead { .. }
            | Self::VaultSet { .. }
            | Self::VaultUnset { .. }
            | Self::BlobDelete { .. }
            | Self::BlobList { .. }
            | Self::QuotaGet {}
            | Self::TempCreate {}
            | Self::TempRelease { .. } => 0,
        }
    }

    /// The reply-buffer capacity this operation declares, zero for the ones
    /// that read nothing.
    const fn capacity(&self) -> u32 {
        match *self {
            Self::ConfigRead { capacity, .. }
            | Self::PublicRead { capacity, .. }
            | Self::VaultRead { capacity }
            | Self::BlobList { capacity } => capacity,
            Self::ConfigSet { .. }
            | Self::ConfigUnset { .. }
            | Self::ConfigCommit { .. }
            | Self::VaultSet { .. }
            | Self::VaultUnset { .. }
            | Self::BlobOpen { .. }
            | Self::BlobDelete { .. }
            | Self::QuotaGet {}
            | Self::TempCreate {}
            | Self::TempRelease { .. } => 0,
        }
    }

    /// The widest reply this operation's declared capacity may ask for.
    ///
    /// A configuration or sealed read is bounded by the document ceiling and a
    /// blob listing by its own, because the two answers are different shapes:
    /// letting a listing declare a document's capacity would size every
    /// caller's buffer to the wrong thing.
    const fn capacity_bound(&self) -> usize {
        match *self {
            Self::BlobList { .. } => APPDATA_BLOB_LIST_MAX,
            _ => APPDATA_DOCUMENT_MAX,
        }
    }

    /// Encode `self` little-endian into `out`, returning the bytes written.
    ///
    /// # Errors
    ///
    /// * [`Errno::LengthOutOfRange`] — a key longer than
    ///   [`APPDATA_KEY_MAX`], a value longer than [`APPDATA_VALUE_MAX`], a
    ///   store name longer than [`APPDATA_NAME_MAX`], or a capacity beyond
    ///   the widest reply the operation can produce.
    /// * [`Errno::BufferTooSmall`] — `out` cannot hold the record.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Errno> {
        let (name, key, value) = self.payload();
        if key.len() > APPDATA_KEY_MAX
            || value.len() > APPDATA_VALUE_MAX
            || name.len() > APPDATA_NAME_MAX
        {
            return Err(Errno::LengthOutOfRange);
        }
        if self.capacity() as usize > self.capacity_bound() {
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
        out[MODE_OFFSET] = self.mode_wire();
        out[NAME_LEN_OFFSET] = u8::try_from(name.len()).map_err(|_| Errno::LengthOutOfRange)?;
        let name_end = APPDATA_HEADER_LEN + name.len();
        let key_end = name_end + key.len();
        out[APPDATA_HEADER_LEN..name_end].copy_from_slice(name.as_bytes());
        out[name_end..key_end].copy_from_slice(key.as_bytes());
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
    /// * [`Errno::OutOfRange`] — an operation, scope, or blob mode outside
    ///   its closed set, text that is not valid UTF-8, or a store name outside
    ///   its grammar.
    /// * [`Errno::LengthOutOfRange`] — a declared length beyond its bound, a
    ///   required key or name that is empty, or a capacity beyond the widest
    ///   reply the operation can produce.
    pub fn decode(bytes: &'a [u8]) -> Result<Self, Errno> {
        let record = Record::split(bytes)?;
        match record.op {
            OP_CONFIG_READ | OP_CONFIG_SET | OP_CONFIG_UNSET | OP_CONFIG_COMMIT => {
                Self::configuration(&record)
            }
            OP_PUBLIC_READ => {
                record.without_scope()?;
                record.without_mode()?;
                record.without_key()?;
                record.without_value()?;
                Ok(Self::PublicRead {
                    bundle_id: record.bundle_id()?,
                    capacity: record.capacity(APPDATA_DOCUMENT_MAX)?,
                })
            }
            OP_VAULT_READ | OP_VAULT_SET | OP_VAULT_UNSET => Self::sealed(&record),
            OP_BLOB_OPEN | OP_BLOB_DELETE | OP_BLOB_LIST | OP_QUOTA_GET => Self::blob(&record),
            OP_TEMP_CREATE | OP_TEMP_RELEASE => Self::temp(&record),
            _ => Err(Errno::OutOfRange),
        }
    }

    /// Decode one of the four own-scope configuration operations: the family
    /// that carries a scope, and the only one that does.
    fn configuration(record: &Record<'a>) -> Result<Self, Errno> {
        record.without_name()?;
        record.without_mode()?;
        match record.op {
            OP_CONFIG_READ => {
                record.without_key()?;
                record.without_value()?;
                Ok(Self::ConfigRead {
                    scope: record.scope()?,
                    capacity: record.capacity(APPDATA_DOCUMENT_MAX)?,
                })
            }
            OP_CONFIG_SET => {
                record.without_capacity()?;
                Ok(Self::ConfigSet {
                    scope: record.scope()?,
                    key: record.key()?,
                    value: record.value,
                })
            }
            OP_CONFIG_UNSET => {
                record.without_capacity()?;
                record.without_value()?;
                Ok(Self::ConfigUnset {
                    scope: record.scope()?,
                    key: record.key()?,
                })
            }
            _ => {
                record.without_capacity()?;
                record.without_key()?;
                record.without_value()?;
                Ok(Self::ConfigCommit {
                    scope: record.scope()?,
                })
            }
        }
    }

    /// Decode one of the three sealed-scope operations: the family that
    /// carries neither a scope nor a name, so no frame in it can reach a
    /// configuration document or another application.
    fn sealed(record: &Record<'a>) -> Result<Self, Errno> {
        record.without_scope()?;
        record.without_mode()?;
        record.without_name()?;
        match record.op {
            OP_VAULT_READ => {
                record.without_key()?;
                record.without_value()?;
                Ok(Self::VaultRead {
                    capacity: record.capacity(APPDATA_DOCUMENT_MAX)?,
                })
            }
            OP_VAULT_SET => {
                record.without_capacity()?;
                Ok(Self::VaultSet {
                    key: record.key()?,
                    value: record.value,
                })
            }
            _ => {
                record.without_capacity()?;
                record.without_value()?;
                Ok(Self::VaultUnset { key: record.key()? })
            }
        }
    }

    /// Decode one of the four blob-scope operations: the family that carries
    /// no scope and no setting, so no frame in it can reach a configuration
    /// document either.
    fn blob(record: &Record<'a>) -> Result<Self, Errno> {
        record.without_scope()?;
        record.without_key()?;
        record.without_value()?;
        match record.op {
            OP_BLOB_OPEN => {
                record.without_capacity()?;
                Ok(Self::BlobOpen {
                    name: record.bulk_name()?,
                    mode: record.mode()?,
                })
            }
            OP_BLOB_DELETE => {
                record.without_mode()?;
                record.without_capacity()?;
                Ok(Self::BlobDelete {
                    name: record.bulk_name()?,
                })
            }
            OP_BLOB_LIST => {
                record.without_mode()?;
                record.without_name()?;
                Ok(Self::BlobList {
                    capacity: record.capacity(APPDATA_BLOB_LIST_MAX)?,
                })
            }
            _ => {
                record.without_mode()?;
                record.without_name()?;
                record.without_capacity()?;
                Ok(Self::QuotaGet {})
            }
        }
    }

    /// Decode one of the two temporary-scope operations: the family that
    /// carries no scope, no mode, no setting, and no capacity, because a
    /// temporary file is reached as a descriptor and never as bytes on this
    /// channel.
    fn temp(record: &Record<'a>) -> Result<Self, Errno> {
        record.without_scope()?;
        record.without_mode()?;
        record.without_key()?;
        record.without_value()?;
        record.without_capacity()?;
        if record.op == OP_TEMP_CREATE {
            record.without_name()?;
            return Ok(Self::TempCreate {});
        }
        Ok(Self::TempRelease {
            name: record.bulk_name()?,
        })
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
    mode: u8,
    capacity: u32,
    name: &'a str,
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
        // A reserved byte is a promise about what the sender did not say; a
        // frame that filled it means something this version cannot read.
        if bytes[RESERVED_OFFSET] != 0 {
            return Err(Errno::BadMagic);
        }
        let name_len = usize::from(bytes[NAME_LEN_OFFSET]);
        let key_len = usize::from(read_u16(bytes, KEY_LEN_OFFSET));
        let value_len = usize::from(read_u16(bytes, VALUE_LEN_OFFSET));
        if name_len > APPDATA_NAME_MAX || key_len > APPDATA_KEY_MAX || value_len > APPDATA_VALUE_MAX
        {
            return Err(Errno::LengthOutOfRange);
        }
        let name_end = APPDATA_HEADER_LEN + name_len;
        let key_end = name_end + key_len;
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
            mode: bytes[MODE_OFFSET],
            capacity: read_u32(bytes, CAPACITY_OFFSET),
            name: text(&bytes[APPDATA_HEADER_LEN..name_end])?,
            key: text(&bytes[name_end..key_end])?,
            value: text(&bytes[key_end..total])?,
        })
    }

    /// The scope this record names.
    fn scope(&self) -> Result<ConfigScope, Errno> {
        ConfigScope::from_wire(self.scope)
    }

    /// The blob access mode this record names.
    fn mode(&self) -> Result<BlobMode, Errno> {
        BlobMode::from_wire(self.mode)
    }

    /// The reply capacity this record declares, bounded by the widest reply
    /// the operation can produce.
    fn capacity(&self, bound: usize) -> Result<u32, Errno> {
        if self.capacity as usize > bound {
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
        crate::validate_bundle_id(self.name)?;
        Ok(self.name)
    }

    /// The bulk-store file name this record carries — a blob's or a temporary
    /// file's — inside the same store-name grammar under the store-name bound.
    fn bulk_name(&self) -> Result<&'a str, Errno> {
        validate_bulk_name(self.name)?;
        Ok(self.name)
    }

    /// Refuse a record whose operation names nothing in the store.
    fn without_name(&self) -> Result<(), Errno> {
        Self::absent(self.name.is_empty())
    }

    /// Refuse a record whose operation opens no blob.
    fn without_mode(&self) -> Result<(), Errno> {
        Self::absent(self.mode == 0)
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
/// [`AppDataRequest::ConfigRead`] answer, which is wider than any blob reply.
/// The value the endpoint is created with, so a reply can never be refused for
/// want of room.
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

/// Byte length of a [`AppDataRequest::BlobOpen`] reply past the status word:
/// the one-shot grant handle the caller redeems.
pub const APPDATA_GRANT_REPLY_LEN: usize = crate::reply::STATUS_REPLY_LEN + 8;

/// Encode a descriptor-grant reply carrying `handle`.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] — `out` cannot hold the reply.
pub fn encode_grant_reply(handle: u64, out: &mut [u8]) -> Result<usize, Errno> {
    if out.len() < APPDATA_GRANT_REPLY_LEN {
        return Err(Errno::BufferTooSmall);
    }
    out[..crate::reply::STATUS_REPLY_LEN]
        .copy_from_slice(&crate::reply::encode_status_reply(Ok(())));
    crate::le::put_u64(out, crate::reply::STATUS_REPLY_LEN, handle);
    Ok(APPDATA_GRANT_REPLY_LEN)
}

/// Decode a descriptor-grant reply.
///
/// # Errors
///
/// * The daemon's own refusal, decoded from the status word.
/// * [`Errno::BufferTooSmall`] — the frame is shorter than the reply.
/// * [`Errno::BadMagic`] — a zero handle, which is the reserved invalid
///   value and never something a mint produced.
pub fn decode_grant_reply(bytes: &[u8]) -> Result<u64, Errno> {
    crate::reply::decode_status_reply(bytes)?;
    if bytes.len() < APPDATA_GRANT_REPLY_LEN {
        return Err(Errno::BufferTooSmall);
    }
    let handle = crate::le::read_u64(bytes, crate::reply::STATUS_REPLY_LEN);
    if handle == 0 {
        return Err(Errno::BadMagic);
    }
    Ok(handle)
}

/// Byte width of a store-name field inside a fixed-width reply record: a
/// length byte and the name at its full width, zero-padded.
///
/// One definition, because a blob-listing entry and a temporary file's reply
/// both carry a store name and neither has any reason to spell the field
/// differently — two encodings of one field would be two chances to disagree
/// about what hides past a declared length.
const NAME_FIELD_LEN: usize = 1 + APPDATA_NAME_MAX;

/// Write `name` into the fixed-width store-name field at the head of `out`,
/// zero-filling the rest of the field.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] when `out` cannot hold the field;
/// [`Errno::LengthOutOfRange`] or [`Errno::OutOfRange`] when `name` is outside
/// the store-name grammar, so no record can carry it.
fn put_store_name(name: &str, out: &mut [u8]) -> Result<(), Errno> {
    if out.len() < NAME_FIELD_LEN {
        return Err(Errno::BufferTooSmall);
    }
    validate_bulk_name(name)?;
    out[..NAME_FIELD_LEN].fill(0);
    // Bounded by the grammar above, so the conversion is exact.
    out[0] = u8::try_from(name.len()).map_err(|_| Errno::LengthOutOfRange)?;
    out[1..=name.len()].copy_from_slice(name.as_bytes());
    Ok(())
}

/// Read the fixed-width store-name field at the head of `bytes`.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] when `bytes` is shorter than the field;
/// [`Errno::LengthOutOfRange`] or [`Errno::OutOfRange`] for a declared length
/// past the field, text that is not UTF-8, or a name outside the grammar; and
/// [`Errno::BadMagic`] when anything hides past the declared length, which is
/// not the field the record described.
fn take_store_name(bytes: &[u8]) -> Result<&str, Errno> {
    if bytes.len() < NAME_FIELD_LEN {
        return Err(Errno::BufferTooSmall);
    }
    let len = usize::from(bytes[0]);
    if len > APPDATA_NAME_MAX {
        return Err(Errno::LengthOutOfRange);
    }
    let name = text(&bytes[1..=len])?;
    validate_bulk_name(name)?;
    if bytes[(1 + len)..NAME_FIELD_LEN].iter().any(|b| *b != 0) {
        return Err(Errno::BadMagic);
    }
    Ok(name)
}

/// Encoded length of one blob-listing entry: the fixed-width store-name field
/// and the blob's byte length.
///
/// Fixed-width, unlike the request record: a listing is a *sequence*, so a
/// reader that walks it with a stride reads no length prefix it might
/// mis-trust, and the whole answer's size is known before it is asked for.
pub const APPDATA_BLOB_ENTRY_LEN: usize = NAME_FIELD_LEN + 8;

/// Maximum blob-listing reply body, in bytes: every blob an application may
/// hold, at the widest entry.
///
/// The bound is what lets [`AppDataRequest::BlobList`] answer whole-or-nothing
/// with no cursor at all.
pub const APPDATA_BLOB_LIST_MAX: usize = APPDATA_BLOB_MAX_COUNT * APPDATA_BLOB_ENTRY_LEN;

/// Byte length of the blob-listing reply header following the status word: the
/// whole listing's length in bytes, whether or not its entries fitted.
pub const APPDATA_BLOB_LIST_HEADER_LEN: usize = 4;

/// One blob, as a listing reports it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct BlobEntry<'a> {
    /// The blob's name, inside the store-name grammar.
    pub name: &'a str,
    /// Its length in bytes, as the volume last reported it.
    pub len: u64,
}

/// What a blob listing answered.
///
/// Two states and no third, exactly as [`ConfigDocument`]: a caller either
/// holds the whole listing or knows how big a buffer to ask again with. A
/// partly-transferred listing is not representable, so no caller can act on a
/// listing that is missing entries it would have deleted or read.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum BlobListing<'a> {
    /// The whole listing, as a sequence of fixed-width entries.
    Whole(&'a [u8]),
    /// The listing did not fit the capacity the request declared. This is its
    /// whole length in bytes: ask again with at least this much.
    NeedsCapacity(usize),
}

impl<'a> BlobListing<'a> {
    /// Iterate the entries of a whole listing, or nothing at all when the
    /// listing did not fit.
    ///
    /// Each entry is validated as it is read — a name outside the grammar, or
    /// a declared length past the field, ends the walk rather than yielding a
    /// name a caller might compose a path from.
    pub fn entries(&self) -> impl Iterator<Item = BlobEntry<'a>> + 'a {
        let body: &'a [u8] = match *self {
            Self::Whole(body) => body,
            Self::NeedsCapacity(_) => &[],
        };
        let (entries, _) = body.as_chunks::<APPDATA_BLOB_ENTRY_LEN>();
        entries
            .iter()
            .map_while(|entry| decode_blob_entry(entry).ok())
    }
}

/// Decode one fixed-width listing entry.
fn decode_blob_entry(entry: &[u8; APPDATA_BLOB_ENTRY_LEN]) -> Result<BlobEntry<'_>, Errno> {
    Ok(BlobEntry {
        name: take_store_name(entry)?,
        len: crate::le::read_u64(entry, NAME_FIELD_LEN),
    })
}

/// Encode one fixed-width listing entry into `out`.
///
/// # Errors
///
/// * [`Errno::BufferTooSmall`] — `out` is shorter than
///   [`APPDATA_BLOB_ENTRY_LEN`].
/// * [`Errno::LengthOutOfRange`] or [`Errno::OutOfRange`] — `name` is outside
///   the blob-name grammar, so no listing can carry it.
pub fn encode_blob_entry(entry: &BlobEntry<'_>, out: &mut [u8]) -> Result<(), Errno> {
    if out.len() < APPDATA_BLOB_ENTRY_LEN {
        return Err(Errno::BufferTooSmall);
    }
    put_store_name(entry.name, out)?;
    crate::le::put_u64(out, NAME_FIELD_LEN, entry.len);
    Ok(())
}

/// Encode a blob-listing reply carrying `listing` — a whole number of
/// [`APPDATA_BLOB_ENTRY_LEN`] entries — whose bytes are sent only if they fit
/// the `capacity` the request declared.
///
/// # Errors
///
/// * [`Errno::LengthOutOfRange`] — `listing` is longer than
///   [`APPDATA_BLOB_LIST_MAX`] or is not a whole number of entries.
/// * [`Errno::BufferTooSmall`] — `out` cannot hold the reply.
pub fn encode_blob_list_reply(
    listing: &[u8],
    capacity: u32,
    out: &mut [u8],
) -> Result<usize, Errno> {
    if listing.len() > APPDATA_BLOB_LIST_MAX
        || !listing.len().is_multiple_of(APPDATA_BLOB_ENTRY_LEN)
    {
        return Err(Errno::LengthOutOfRange);
    }
    let header = crate::reply::STATUS_REPLY_LEN + APPDATA_BLOB_LIST_HEADER_LEN;
    let body = if listing.len() <= capacity as usize {
        listing.len()
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
        // Bounded by `APPDATA_BLOB_LIST_MAX` above, far inside a u32.
        u32::try_from(listing.len()).map_err(|_| Errno::LengthOutOfRange)?,
    );
    out[header..total].copy_from_slice(&listing[..body]);
    Ok(total)
}

/// Decode a blob-listing reply.
///
/// # Errors
///
/// * The daemon's own refusal, decoded from the status word.
/// * [`Errno::BufferTooSmall`] — the frame is shorter than its header.
/// * [`Errno::BadMagic`] — a body that is neither empty nor the whole listing
///   the header declares.
/// * [`Errno::LengthOutOfRange`] — a declared length beyond
///   [`APPDATA_BLOB_LIST_MAX`], or one that is not a whole number of entries.
pub fn decode_blob_list_reply(bytes: &[u8]) -> Result<BlobListing<'_>, Errno> {
    crate::reply::decode_status_reply(bytes)?;
    let header = crate::reply::STATUS_REPLY_LEN + APPDATA_BLOB_LIST_HEADER_LEN;
    if bytes.len() < header {
        return Err(Errno::BufferTooSmall);
    }
    let declared = usize::try_from(read_u32(bytes, crate::reply::STATUS_REPLY_LEN))
        .map_err(|_| Errno::LengthOutOfRange)?;
    if declared > APPDATA_BLOB_LIST_MAX || !declared.is_multiple_of(APPDATA_BLOB_ENTRY_LEN) {
        return Err(Errno::LengthOutOfRange);
    }
    let body = &bytes[header..];
    if body.is_empty() && declared > 0 {
        return Ok(BlobListing::NeedsCapacity(declared));
    }
    if body.len() != declared {
        return Err(Errno::BadMagic);
    }
    Ok(BlobListing::Whole(body))
}

/// Byte length of a [`AppDataRequest::QuotaGet`] reply past the status word:
/// the four usage figures and the three ceilings they are measured against.
pub const APPDATA_QUOTA_REPLY_LEN: usize = crate::reply::STATUS_REPLY_LEN + 7 * 8;

/// An application's bulk-store usage in one account, and the ceilings it is
/// bounded by.
///
/// Both scopes of the bulk tree in one answer, because they are one store: an
/// application that must decide whether to spill to scratch or to evict a
/// cached index needs both figures, and two calls could report two moments.
///
/// The ceilings are reported rather than assumed: they are fixed in this
/// contract, but an application that reads them can say "this cache is full"
/// in its own terms instead of surfacing an errno, and a diagnostic tool can
/// show usage against the bound without compiling the bound in.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct BulkQuota {
    /// Blobs the application currently holds.
    pub blobs: u64,
    /// Their total length in bytes.
    pub blob_bytes: u64,
    /// Temporary files it currently holds — those of *this* boot, since an
    /// earlier boot's are reclaimed before the next one is created and are
    /// reachable by nothing in the meantime.
    pub temps: u64,
    /// Their total length in bytes.
    pub temp_bytes: u64,
    /// Most blobs it may hold ([`APPDATA_BLOB_MAX_COUNT`]).
    pub blob_max: u64,
    /// Most temporary files it may hold at once
    /// ([`APPDATA_TEMP_MAX_COUNT`]).
    pub temp_max: u64,
    /// Longest any one file in either scope may be
    /// ([`APPDATA_BULK_FILE_MAX_BYTES`]) — the extent ceiling the kernel
    /// enforces on a writable descriptor, so the application's whole bulk
    /// store is bounded by `(blob_max + temp_max) × file_bytes_max`.
    pub file_bytes_max: u64,
}

/// Encode a quota reply.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] — `out` cannot hold the reply.
pub fn encode_quota_reply(quota: &BulkQuota, out: &mut [u8]) -> Result<usize, Errno> {
    if out.len() < APPDATA_QUOTA_REPLY_LEN {
        return Err(Errno::BufferTooSmall);
    }
    out[..crate::reply::STATUS_REPLY_LEN]
        .copy_from_slice(&crate::reply::encode_status_reply(Ok(())));
    let at = crate::reply::STATUS_REPLY_LEN;
    for (slot, figure) in [
        quota.blobs,
        quota.blob_bytes,
        quota.temps,
        quota.temp_bytes,
        quota.blob_max,
        quota.temp_max,
        quota.file_bytes_max,
    ]
    .into_iter()
    .enumerate()
    {
        crate::le::put_u64(out, at + slot * 8, figure);
    }
    Ok(APPDATA_QUOTA_REPLY_LEN)
}

/// Decode a quota reply.
///
/// # Errors
///
/// * The daemon's own refusal, decoded from the status word.
/// * [`Errno::BufferTooSmall`] — the frame is shorter than the reply.
/// * [`Errno::OutOfRange`] — a usage figure past its own ceiling, which is
///   not a state the daemon can be in and so is refused rather than shown.
pub fn decode_quota_reply(bytes: &[u8]) -> Result<BulkQuota, Errno> {
    crate::reply::decode_status_reply(bytes)?;
    if bytes.len() < APPDATA_QUOTA_REPLY_LEN {
        return Err(Errno::BufferTooSmall);
    }
    let at = crate::reply::STATUS_REPLY_LEN;
    let figure = |slot: usize| crate::le::read_u64(bytes, at + slot * 8);
    let quota = BulkQuota {
        blobs: figure(0),
        blob_bytes: figure(1),
        temps: figure(2),
        temp_bytes: figure(3),
        blob_max: figure(4),
        temp_max: figure(5),
        file_bytes_max: figure(6),
    };
    let within = |count: u64, bytes: u64, ceiling: u64| {
        count <= ceiling && bytes <= ceiling.saturating_mul(quota.file_bytes_max)
    };
    if !within(quota.blobs, quota.blob_bytes, quota.blob_max)
        || !within(quota.temps, quota.temp_bytes, quota.temp_max)
    {
        return Err(Errno::OutOfRange);
    }
    Ok(quota)
}

/// Byte length of a [`AppDataRequest::TempCreate`] reply past the status word:
/// the one-shot grant handle the caller redeems, and the name the service gave
/// the file.
pub const APPDATA_TEMP_REPLY_LEN: usize = crate::reply::STATUS_REPLY_LEN + 8 + NAME_FIELD_LEN;

// The widest name the service could mint must still frame, or a create could
// land on the volume and fail to be answered.
const _: () = assert!(APPDATA_TEMP_REPLY_LEN <= APPDATA_MAX_REPLY);

/// Encode a temporary-file reply carrying `handle` and the `name` the service
/// minted.
///
/// The name is answered because [`AppDataRequest::TempRelease`] is the only
/// thing that can be done with it: there is no operation that *opens* a
/// temporary file by name, so a caller holding one can free its own scratch
/// and nothing else.
///
/// # Errors
///
/// * [`Errno::BufferTooSmall`] — `out` cannot hold the reply.
/// * [`Errno::LengthOutOfRange`] or [`Errno::OutOfRange`] — `name` is outside
///   the store-name grammar, so no reply can carry it.
pub fn encode_temp_reply(handle: u64, name: &str, out: &mut [u8]) -> Result<usize, Errno> {
    if out.len() < APPDATA_TEMP_REPLY_LEN {
        return Err(Errno::BufferTooSmall);
    }
    out[..crate::reply::STATUS_REPLY_LEN]
        .copy_from_slice(&crate::reply::encode_status_reply(Ok(())));
    let at = crate::reply::STATUS_REPLY_LEN;
    crate::le::put_u64(out, at, handle);
    put_store_name(name, &mut out[at + 8..])?;
    Ok(APPDATA_TEMP_REPLY_LEN)
}

/// Decode a temporary-file reply into the grant handle and the file's name.
///
/// # Errors
///
/// * The daemon's own refusal, decoded from the status word.
/// * [`Errno::BufferTooSmall`] — the frame is shorter than the reply.
/// * [`Errno::BadMagic`] — a zero handle, which is the reserved invalid value
///   and never something a mint produced, or a name field with bytes hiding
///   past its declared length.
/// * [`Errno::LengthOutOfRange`] or [`Errno::OutOfRange`] — a name outside the
///   store-name grammar, which is not a name this service could have minted.
pub fn decode_temp_reply(bytes: &[u8]) -> Result<(u64, &str), Errno> {
    crate::reply::decode_status_reply(bytes)?;
    if bytes.len() < APPDATA_TEMP_REPLY_LEN {
        return Err(Errno::BufferTooSmall);
    }
    let at = crate::reply::STATUS_REPLY_LEN;
    let handle = crate::le::read_u64(bytes, at);
    if handle == 0 {
        return Err(Errno::BadMagic);
    }
    Ok((handle, take_store_name(&bytes[at + 8..])?))
}

#[cfg(test)]
#[path = "appdata_ipc_tests.rs"]
mod tests;
