//! TAIRiX app-data service — the dispatcher.
//!
//! `confd` is the only principal in the system holding
//! [`CapabilityId::APPDATA_ADMIN`](tairix_abi::CapabilityId::APPDATA_ADMIN),
//! and that capability is the per-inode gate on every user's per-app store
//! tree. So it is the only path to an application's stored settings, and it
//! answers each request against the store it derives from the caller's
//! **kernel-attested** identity.
//!
//! # Why a service and not a file mode
//!
//! All of a user's applications run as that one user. The per-inode
//! owner/mode/ACL model keys on uid, so it cannot separate two applications of
//! the same account at all — app-from-app isolation inside one account is not
//! expressible in it. Keying on the identity the kernel attests for the
//! calling *bundle* is, and that is what this service exists to do. It is not
//! a convenience wrapper over the filesystem.
//!
//! # What a caller can and cannot ask for
//!
//! A request carries a key and a value. It never carries a bundle identifier,
//! a user, or a path: the service resolves all three from the attested
//! [`Origin`], so no request shape can name another application's store. A
//! caller running no verified bundle — a kernel principal, a boot-floor
//! program with no signed manifest, a parser-sandbox child — has no store and
//! is refused.
//!
//! # One read for the whole document
//!
//! A `ConfigRead` answers with the caller's whole merged document — the
//! machine-wide policy layer, the app's own settings over it, the caller's own
//! staged edits over those — as canonical `key = value` text. An application's
//! start-up therefore costs one call, one store read, and one parse however
//! many settings it goes on to consult; answering per key would have cost a
//! file read and a parse each.
//!
//! # Staged writes
//!
//! A `ConfigSet` or `ConfigUnset` records a *pending edit* against the calling
//! process instance; `ConfigCommit` loads the committed document, applies the
//! pending edits, and publishes the result as one atomic replacement. A caller
//! that never commits changes nothing on the volume, and its own reads see its
//! own pending edits so a settings sheet reads back what it just set.
//!
//! # Layering
//!
//! This crate is `no_std` (with `alloc`) and performs **no I/O**: every read
//! and write goes through the injected [`Storage`] seam, so the whole engine —
//! authorisation, the ownership pin, the layered read, staging, and the atomic
//! publish — is exercised on the host. The service *binary* (`src/run.rs`)
//! supplies the real seam over the `fs_*` syscalls.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::appdata_ipc::{encode_document_reply, AppDataRequest};
use tairix_abi::reply::encode_status_reply;
use tairix_abi::{AppIdentity, Errno, Origin, ProcId};
use tairix_appconf::{validate_key, ConfError, Document, MAX_SETTINGS};
use tairix_log::{Event, EventId, Field, FieldValue, Level, Sink};

pub mod events;
mod owner;
pub mod store;
#[cfg(test)]
mod testfs;

pub use owner::OwnerPin;
pub use store::{AppStore, RootCache, StoreError};

/// The home subdirectory the private configuration scope lives under.
///
/// `Settings/` is where the installed-system contract puts user-scoped
/// configuration; `Library/` holds the bulk and volatile scopes the later
/// stages serve. Both are named once, in the shared home-shape definition.
pub const APPDATA_PARENT: &str = "Settings";

/// How long a staging session may sit untouched before it is reclaimed.
///
/// Staging exists to make a settings sheet's "save" one atomic publish, which
/// is a human-driven act measured in seconds. A caller that stages an edit and
/// wanders off — or exits without committing — must not pin the service's
/// memory for the life of the machine, and there is no primitive by which a
/// server learns that a peer died. A minute is far longer than any real edit
/// and short enough that abandoned sessions drain promptly; losing an
/// abandoned session's edits is exactly the contract ("a caller that never
/// commits changes nothing").
pub const STAGING_IDLE_NS: u64 = 60_000_000_000;

/// Maximum pending edits one staging session may hold.
///
/// A fixed bound on untrusted input, not a capacity: a document may carry at
/// most [`MAX_SETTINGS`] settings, so a session that has staged that many
/// distinct keys has already described a whole document and anything further
/// is a runaway writer rather than a workload.
pub const MAX_PENDING_EDITS: usize = MAX_SETTINGS;

/// The filesystem the store lives on, as the dispatcher needs it.
///
/// Deliberately narrow: whole-file reads and writes, a rename, a directory
/// create, an owner query, and a directory listing. There is no partial write
/// and no seek, because a configuration document is only ever replaced whole —
/// which is what makes the publish atomic. Every method reports
/// [`Errno::NotFound`] for an absent path so the engine can tell "not there"
/// from "cannot be reached" and fail closed on the second.
pub trait Storage {
    /// Read the whole file at `path`.
    ///
    /// # Errors
    ///
    /// [`Errno::NotFound`] when the path does not exist; any other
    /// [`Errno`] for a failed read.
    fn read(&mut self, path: &str) -> Result<Vec<u8>, Errno>;

    /// Replace the file at `path` with `bytes`, creating it if absent.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the create, truncate, or write reports.
    fn write(&mut self, path: &str, bytes: &[u8]) -> Result<(), Errno>;

    /// Rename `src` over `dst` within one directory.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the rename reports.
    fn rename(&mut self, src: &str, dst: &str) -> Result<(), Errno>;

    /// Create the directory `path` with permission bits `mode`.
    ///
    /// # Errors
    ///
    /// [`Errno::AlreadyExists`] when the name is taken; any other [`Errno`]
    /// for a failed create.
    fn mkdir(&mut self, path: &str, mode: u32) -> Result<(), Errno>;

    /// The uid owning the node at `path`.
    ///
    /// # Errors
    ///
    /// [`Errno::NotFound`] when the path does not exist; any other [`Errno`]
    /// for a failed stat.
    fn owner_of(&mut self, path: &str) -> Result<u32, Errno>;

    /// The entry names of the directory `path`, excluding `.` and `..`.
    ///
    /// # Errors
    ///
    /// [`Errno::NotFound`] when the path does not exist; any other [`Errno`]
    /// for a failed listing.
    fn list_dir(&mut self, path: &str) -> Result<Vec<String>, Errno>;
}

/// One staged, uncommitted change to a document.
struct PendingEdit {
    key: String,
    /// The new value, or [`None`] for a staged removal.
    value: Option<String>,
}

/// The uncommitted edits of one calling process instance.
struct Session {
    /// The process instance that staged them. Unforgeable and never reused, so
    /// two processes of the same application can never share a session and one
    /// cannot publish the other's half-finished edits.
    proc_id: ProcId,
    /// The monotonic instant of the last request that touched this session.
    touched_ns: u64,
    edits: Vec<PendingEdit>,
}

impl Session {
    /// Stage `key = value`, or a removal when `value` is [`None`], replacing
    /// any edit already staged for that key.
    ///
    /// # Errors
    ///
    /// [`Errno::LimitExceeded`] when the session already holds
    /// [`MAX_PENDING_EDITS`] distinct keys.
    fn stage(&mut self, key: &str, value: Option<&str>) -> Result<(), Errno> {
        let staged = value.map(String::from);
        if let Some(edit) = self.edits.iter_mut().find(|edit| edit.key == key) {
            edit.value = staged;
            return Ok(());
        }
        if self.edits.len() >= MAX_PENDING_EDITS {
            return Err(Errno::LimitExceeded);
        }
        self.edits.push(PendingEdit {
            key: String::from(key),
            value: staged,
        });
        Ok(())
    }

    /// Apply every staged edit to `document`, in the order they were staged.
    ///
    /// # Errors
    ///
    /// [`Errno::LimitExceeded`] when the document is already at the format's
    /// setting or line bound and an edit would add one, [`Errno::OutOfRange`]
    /// for a key or value the format refuses. The two are distinguished
    /// because a caller can act on the first — drop a setting — and on the
    /// second only by fixing what it sent.
    fn apply(&self, document: &mut Document) -> Result<(), Errno> {
        for edit in &self.edits {
            match &edit.value {
                Some(value) => document.set(&edit.key, value).map_err(|err| match err {
                    ConfError::TooManySettings | ConfError::TooManyLines => Errno::LimitExceeded,
                    _ => Errno::OutOfRange,
                })?,
                None => document.unset(&edit.key),
            }
        }
        Ok(())
    }
}

/// The app-data dispatcher: the staging table, and the one entry point that
/// turns a framed request plus an attested origin into a reply.
pub struct AppData<S: Sink> {
    sessions: Vec<Session>,
    roots: RootCache,
    sink: S,
}

impl<S: Sink> AppData<S> {
    /// Build a dispatcher that records its decisions through `sink`.
    #[must_use]
    pub const fn new(sink: S) -> Self {
        Self {
            sessions: Vec::new(),
            roots: RootCache::new(),
            sink,
        }
    }

    /// Serve one request, writing the reply frame into `out` and returning its
    /// length.
    ///
    /// `now_ns` is the monotonic clock, used only to age staging sessions.
    /// `origin` is the caller's kernel-attested identity — never a wire claim —
    /// and is the sole source of which store is reached.
    ///
    /// A refusal is a reply, not an error: every path answers with a frame the
    /// caller can decode, and the security-relevant refusals are audited.
    #[must_use]
    pub fn serve<F: Storage + ?Sized>(
        &mut self,
        fs: &mut F,
        origin: &Origin,
        now_ns: u64,
        request: &[u8],
        out: &mut [u8],
    ) -> usize {
        self.reclaim_idle(now_ns);
        match self.dispatch(fs, origin, now_ns, request, out) {
            Ok(len) => len,
            Err(err) => reply(Err(err), out),
        }
    }

    /// Decode, authorise, and perform one request.
    fn dispatch<F: Storage + ?Sized>(
        &mut self,
        fs: &mut F,
        origin: &Origin,
        now_ns: u64,
        request: &[u8],
        out: &mut [u8],
    ) -> Result<usize, Errno> {
        let decoded = AppDataRequest::decode(request)?;
        // Authority before state: a caller with no attested bundle identity
        // has no store, whatever it asked for.
        let identity = self.attested_app(origin)?;
        match decoded {
            AppDataRequest::ConfigRead { capacity } => {
                self.config_read(fs, origin, &identity, capacity, out)
            }
            AppDataRequest::ConfigSet { key, value } => {
                validate_key(key).map_err(|_| Errno::OutOfRange)?;
                // Reject a value the format could not store *before* it is
                // staged, so a commit cannot fail on an edit accepted earlier.
                let mut probe = Document::new();
                probe.set(key, value).map_err(|_| Errno::OutOfRange)?;
                self.session(origin, now_ns).stage(key, Some(value))?;
                Ok(ok(out))
            }
            AppDataRequest::ConfigUnset { key } => {
                validate_key(key).map_err(|_| Errno::OutOfRange)?;
                self.session(origin, now_ns).stage(key, None)?;
                Ok(ok(out))
            }
            AppDataRequest::ConfigCommit => {
                self.config_commit(fs, origin, &identity)?;
                Ok(ok(out))
            }
        }
    }

    /// The caller's attested app identity, or a refusal.
    ///
    /// The refusal is audited: a principal with no signed bundle reaching the
    /// app-data endpoint is either a misconfiguration or a probe, and either
    /// way an operator should be able to see it.
    fn attested_app(&self, origin: &Origin) -> Result<AppIdentity, Errno> {
        if let Some(identity) = origin.app() {
            return Ok(*identity);
        }
        self.refuse(origin, StoreError::NoAppIdentity, None);
        Err(StoreError::NoAppIdentity.errno())
    }

    /// Answer a read with the caller's whole merged document: the machine-wide
    /// policy layer, the app's own settings over it, and the caller's own
    /// staged edits over those.
    ///
    /// One call, one store read, one parse — however many settings the caller
    /// goes on to consult. A per-key read would have cost a file read and a
    /// parse *each*, which is the pessimisation this shape exists to avoid.
    fn config_read<F: Storage + ?Sized>(
        &mut self,
        fs: &mut F,
        origin: &Origin,
        identity: &AppIdentity,
        capacity: u32,
        out: &mut [u8],
    ) -> Result<usize, Errno> {
        let store = self.open(fs, origin, identity, false)?;
        let outcome = store.merged_document(fs);
        let mut document = self.resolve(origin, outcome)?;
        // A caller sees its own uncommitted work and no other principal's, so
        // a settings sheet reads back what it just set.
        if let Some(session) = self.session_for(origin) {
            session.apply(&mut document)?;
        }
        encode_document_reply(&document.render(), capacity, out)
    }

    /// Publish the caller's staged edits as one atomic document replacement.
    ///
    /// A commit with nothing staged succeeds and writes nothing: a caller that
    /// changed no setting must not rewrite the user's file, which would cost
    /// the document's timestamp for no change.
    fn config_commit<F: Storage + ?Sized>(
        &mut self,
        fs: &mut F,
        origin: &Origin,
        identity: &AppIdentity,
    ) -> Result<(), Errno> {
        let Some(staged) = self.session_index(origin) else {
            return Ok(());
        };
        if self.sessions[staged].edits.is_empty() {
            self.sessions.remove(staged);
            return Ok(());
        }
        // A commit is the first act that may create the store, so this is the
        // one call that passes `create`.
        let store = self.open(fs, origin, identity, true)?;
        let mut document = self.read_document(fs, origin, &store)?;
        // Re-resolved rather than carried across the borrow above: an index
        // into a table this call may have touched is a trap, and the lookup is
        // over a handful of entries.
        let index = self.session_index(origin).ok_or(Errno::NotFound)?;
        self.sessions[index].apply(&mut document)?;
        let outcome = store.publish(fs, &document);
        self.resolve(origin, outcome)?;
        // The session is dropped only once the publish landed, so a failed
        // commit leaves the edits staged for a retry.
        self.sessions.remove(index);
        Ok(())
    }

    /// Open the caller's store, auditing and translating any refusal.
    fn open<F: Storage + ?Sized>(
        &mut self,
        fs: &mut F,
        origin: &Origin,
        identity: &AppIdentity,
        create: bool,
    ) -> Result<AppStore, Errno> {
        let outcome = AppStore::open(fs, &mut self.roots, origin.uid(), identity, create);
        // Borrowed immutably for the audit record only once the mutable borrow
        // of the cache has ended.
        Self::judge(&self.sink, origin, outcome)
    }

    /// Number of remembered store roots. Test and diagnostic surface.
    #[must_use]
    pub fn resolved_roots(&self) -> usize {
        self.roots.len()
    }

    /// Read the app's own committed document, auditing any refusal.
    fn read_document<F: Storage + ?Sized>(
        &self,
        fs: &mut F,
        origin: &Origin,
        store: &AppStore,
    ) -> Result<Document, Errno> {
        let outcome = store.document(fs);
        self.resolve(origin, outcome)
    }

    /// Translate a [`StoreError`] into a typed refusal, auditing it on the way.
    fn resolve<T>(&self, origin: &Origin, outcome: Result<T, StoreError>) -> Result<T, Errno> {
        Self::judge(&self.sink, origin, outcome)
    }

    /// [`Self::resolve`] over a borrowed sink, for the paths that hold a
    /// mutable borrow of the dispatcher's own state.
    fn judge<T>(sink: &S, origin: &Origin, outcome: Result<T, StoreError>) -> Result<T, Errno> {
        outcome.map_err(|err| {
            Self::record(sink, origin, err, origin.app());
            err.errno()
        })
    }

    /// Record a refused request.
    fn refuse(&self, origin: &Origin, err: StoreError, identity: Option<&AppIdentity>) {
        Self::record(&self.sink, origin, err, identity);
    }

    /// Write one refusal to `sink`.
    fn record(sink: &S, origin: &Origin, err: StoreError, identity: Option<&AppIdentity>) {
        let bundle = identity.map_or("<none>", AppIdentity::bundle_id);
        let _ = tairix_log::log(
            sink,
            &Event {
                level: level_of(err),
                id: events::id_of(err),
                message: err.reason(),
                fields: &[
                    Field {
                        key: "bundle",
                        value: FieldValue::Str(bundle),
                    },
                    Field {
                        key: "uid",
                        value: FieldValue::UnsignedInt(u64::from(origin.uid())),
                    },
                ],
            },
        );
    }

    /// The calling process instance's staging session, creating it if absent.
    fn session(&mut self, origin: &Origin, now_ns: u64) -> &mut Session {
        let index = if let Some(index) = self.session_index(origin) {
            index
        } else {
            self.sessions.push(Session {
                proc_id: origin.proc_id(),
                touched_ns: now_ns,
                edits: Vec::new(),
            });
            self.sessions.len() - 1
        };
        self.sessions[index].touched_ns = now_ns;
        &mut self.sessions[index]
    }

    /// The calling process instance's staging session, if it has one.
    fn session_for(&self, origin: &Origin) -> Option<&Session> {
        self.session_index(origin)
            .map(|index| &self.sessions[index])
    }

    /// Index of the calling process instance's staging session.
    fn session_index(&self, origin: &Origin) -> Option<usize> {
        let proc_id = origin.proc_id();
        self.sessions
            .iter()
            .position(|session| session.proc_id == proc_id)
    }

    /// Drop every session untouched for longer than [`STAGING_IDLE_NS`].
    ///
    /// A monotonic clock that went backwards (it cannot, but the arithmetic
    /// must not wrap) ages nothing rather than dropping everything.
    fn reclaim_idle(&mut self, now_ns: u64) {
        self.sessions
            .retain(|session| now_ns.saturating_sub(session.touched_ns) < STAGING_IDLE_NS);
    }

    /// Number of live staging sessions. Test and diagnostic surface.
    #[must_use]
    pub fn staging_sessions(&self) -> usize {
        self.sessions.len()
    }
}

/// The audit level a refusal is recorded at: an attack indication is a
/// warning, an ordinary absence is informational.
fn level_of(err: StoreError) -> Level {
    match err {
        StoreError::PublisherMismatch
        | StoreError::PinMalformed
        | StoreError::RootNotOwned
        | StoreError::NoAppIdentity => Level::Warn,
        StoreError::NoHome | StoreError::DocumentRefused | StoreError::Unavailable => Level::Info,
    }
}

/// Encode the shared status frame for `outcome`, returning its length.
///
/// A reply buffer too small for four bytes is a defect in the endpoint's own
/// configuration, not a request outcome; answering nothing is the fail-closed
/// reply, and the caller's pending call fails rather than reading a half frame.
fn reply(outcome: Result<(), Errno>, out: &mut [u8]) -> usize {
    let frame = encode_status_reply(outcome);
    if out.len() < frame.len() {
        return 0;
    }
    out[..frame.len()].copy_from_slice(&frame);
    frame.len()
}

/// Encode a success status frame.
fn ok(out: &mut [u8]) -> usize {
    reply(Ok(()), out)
}

/// The audit event id for `err`. Re-exported for the binary's own records.
#[must_use]
pub fn event_id(err: StoreError) -> EventId {
    events::id_of(err)
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
