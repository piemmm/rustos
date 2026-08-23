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
//! A configuration request carries a scope, a key, and a value. It never
//! carries a bundle identifier, a user, or a path: the service resolves all
//! three from the attested [`Origin`], so no request shape can name another
//! application's store. A caller running no verified bundle — a kernel
//! principal, a boot-floor program with no signed manifest, a parser-sandbox
//! child — has no store and is refused, whichever operation it sent.
//!
//! The one request that *does* name an application is `PublicRead`, and it can
//! reach nothing but that application's **published** document: it carries no
//! scope field, so no frame can ask for another app's private settings. An
//! application that publishes nothing, has never run for this account, or
//! whose store cannot be attested all answer the same empty document, so a
//! foreign read is never an oracle for more than what an app chose to publish.
//!
//! # The sealed scope
//!
//! `VaultRead`, `VaultSet`, and `VaultUnset` reach the caller's **secrets**,
//! encrypted at rest under a key derived per (account, application) from the
//! account's master secret ([`vault`]). They carry no scope field and have no
//! foreign counterpart, so no configuration frame can name a secret, no vault
//! frame can name a configuration document, and no frame at all reaches
//! another application's secrets.
//!
//! A sealed write is **immediate**: the service opens the sealed document,
//! applies the one change, re-seals it, and publishes it before it replies.
//! Plaintext secret material therefore exists here for the span of one request
//! rather than for the life of a staging session, and because requests are
//! served one at a time the whole read-modify-seal-publish is atomic — so two
//! processes of one application sealing different secrets cannot lose each
//! other's, which a stage-then-commit pair would allow. A sealed document that
//! cannot be opened is refused, never answered as an empty vault.
//!
//! # One read for the whole document
//!
//! A `ConfigRead` answers with the caller's whole merged document for one
//! scope — for the private scope, the machine-wide policy layer, the app's own
//! settings over it, and the caller's own staged edits over those — as
//! canonical `key = value` text. An application's start-up therefore costs one
//! call, one store read, and one parse however many settings it goes on to
//! consult; answering per key would have cost a file read and a parse each.
//!
//! # Staged writes
//!
//! A `ConfigSet` or `ConfigUnset` records a *pending edit* against the calling
//! process instance, in the scope it named; `ConfigCommit` loads that scope's
//! committed document, applies the pending edits for it, and publishes the
//! result as one atomic replacement. Edits staged against the caller's other
//! scope are untouched, because one rename replaces one name and a commit that
//! claimed to publish two documents at once would be claiming an atomicity no
//! filesystem offers. A caller that never commits changes nothing on the
//! volume, and its own reads see its own pending edits so a settings sheet
//! reads back what it just set.
//!
//! # Layering
//!
//! This crate is `no_std` (with `alloc`) and performs **no I/O** and draws no
//! randomness of its own: every read and write goes through the injected
//! [`Storage`] seam and every draw through the injected [`Entropy`] seam, so
//! the whole engine — authorisation, the ownership pin, the layered read,
//! staging, the atomic publish, and the sealed scope's key hierarchy — is
//! exercised on the host. The service *binary* (`src/run.rs`) supplies the real
//! seams over the `fs_*` and `random_get` syscalls.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::appdata_ipc::{encode_document_reply, AppDataRequest, ConfigScope};
use tairix_abi::reply::encode_status_reply;
use tairix_abi::{AppIdentity, Errno, Origin, ProcId};
use tairix_appconf::{validate_key, ConfError, Document, MAX_SETTINGS};
use tairix_log::{Event, EventId, Field, FieldValue, Level, Sink};
use zeroize::Zeroize;

pub mod events;
mod owner;
pub mod store;
#[cfg(test)]
mod testfs;
pub mod vault;

pub use owner::OwnerPin;
pub use store::{published_document, AppStore, RootCache, StoreError};
pub use vault::{Entropy, VaultError};

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

/// Maximum pending edits one staging session may hold **per scope**.
///
/// A fixed bound on untrusted input, not a capacity: a document may carry at
/// most [`MAX_SETTINGS`] settings, so a session that has staged that many
/// distinct keys in one scope has already described a whole document and
/// anything further is a runaway writer rather than a workload. It is per
/// scope because each scope is a document of its own, and one of them filling
/// up must not deny the other.
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
    /// Which of the caller's own documents the change belongs to. Held per
    /// edit rather than per session because one process instance may have a
    /// settings sheet open and be publishing about itself at the same time,
    /// and neither commit may carry the other's work.
    scope: ConfigScope,
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
    /// Stage `key = value` in `scope`, or a removal when `value` is [`None`],
    /// replacing any edit already staged for that key in that scope.
    ///
    /// # Errors
    ///
    /// [`Errno::LimitExceeded`] when the session already holds
    /// [`MAX_PENDING_EDITS`] distinct keys in `scope`.
    fn stage(&mut self, scope: ConfigScope, key: &str, value: Option<&str>) -> Result<(), Errno> {
        let staged = value.map(String::from);
        if let Some(edit) = self
            .edits
            .iter_mut()
            .find(|edit| edit.scope == scope && edit.key == key)
        {
            edit.value = staged;
            return Ok(());
        }
        if self.staged_in(scope) >= MAX_PENDING_EDITS {
            return Err(Errno::LimitExceeded);
        }
        self.edits.push(PendingEdit {
            scope,
            key: String::from(key),
            value: staged,
        });
        Ok(())
    }

    /// How many distinct keys are staged in `scope`.
    fn staged_in(&self, scope: ConfigScope) -> usize {
        self.edits.iter().filter(|edit| edit.scope == scope).count()
    }

    /// Whether anything at all is staged in `scope`. Answered by a search
    /// rather than by [`Self::staged_in`], so it stops at the first match
    /// instead of counting a whole document's worth of edits to say "yes".
    fn has(&self, scope: ConfigScope) -> bool {
        self.edits.iter().any(|edit| edit.scope == scope)
    }

    /// Whether this session holds no edits in any scope, and so is nothing but
    /// a table entry waiting to be reclaimed.
    fn is_empty(&self) -> bool {
        self.edits.is_empty()
    }

    /// Drop every edit staged in `scope`, leaving the other scope's alone.
    fn clear(&mut self, scope: ConfigScope) {
        self.edits.retain(|edit| edit.scope != scope);
    }

    /// Apply every edit staged in `scope` to `document`, in the order they
    /// were staged. Edits in the other scope belong to another document and
    /// are not applied here.
    ///
    /// # Errors
    ///
    /// [`Errno::LimitExceeded`] when the document is already at the format's
    /// setting or line bound and an edit would add one, [`Errno::OutOfRange`]
    /// for a key or value the format refuses. The two are distinguished
    /// because a caller can act on the first — drop a setting — and on the
    /// second only by fixing what it sent.
    fn apply(&self, scope: ConfigScope, document: &mut Document) -> Result<(), Errno> {
        for edit in self.edits.iter().filter(|edit| edit.scope == scope) {
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

/// One refused request, as the audit stream records it.
///
/// Carried as a value rather than as three parameters so that a new audited
/// dimension is added in one place, and so that the *caller's* app and the
/// *target* store — which are the same thing for every operation but a foreign
/// published read — can never be transposed at a call site.
#[derive(Copy, Clone)]
struct Refusal<'a> {
    /// Why the request was refused.
    err: StoreError,
    /// The calling application, or [`None`] for a principal running no
    /// verified bundle.
    app: Option<&'a AppIdentity>,
    /// The application whose store was being read, when that is not the
    /// caller's own.
    target: Option<&'a str>,
}

/// The app-data dispatcher: the staging table, and the one entry point that
/// turns a framed request plus an attested origin into a reply.
///
/// It holds the service's two long-lived facilities — the audit sink and the
/// generator the sealed scope draws from — and borrows the store volume per
/// request. The generator is held rather than passed because it is *stateful*:
/// a generator handed in fresh for each request could repeat a nonce, and
/// reusing a `(key, nonce)` pair under the sealed scope's AEAD is
/// catastrophic.
pub struct AppData<S: Sink, R: Entropy> {
    sessions: Vec<Session>,
    roots: RootCache,
    sink: S,
    entropy: R,
}

impl<S: Sink, R: Entropy> AppData<S, R> {
    /// Build a dispatcher that records its decisions through `sink` and draws
    /// the sealed scope's key material and nonces from `entropy`.
    #[must_use]
    pub const fn new(sink: S, entropy: R) -> Self {
        Self {
            sessions: Vec::new(),
            roots: RootCache::new(),
            sink,
            entropy,
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
            AppDataRequest::ConfigRead { scope, capacity } => {
                self.config_read(fs, origin, &identity, scope, capacity, out)
            }
            AppDataRequest::ConfigSet { scope, key, value } => {
                validate_key(key).map_err(|_| Errno::OutOfRange)?;
                // Reject a value the format could not store *before* it is
                // staged, so a commit cannot fail on an edit accepted earlier.
                let mut probe = Document::new();
                probe.set(key, value).map_err(|_| Errno::OutOfRange)?;
                self.session(origin, now_ns)
                    .stage(scope, key, Some(value))?;
                Ok(ok(out))
            }
            AppDataRequest::ConfigUnset { scope, key } => {
                validate_key(key).map_err(|_| Errno::OutOfRange)?;
                self.session(origin, now_ns).stage(scope, key, None)?;
                Ok(ok(out))
            }
            AppDataRequest::ConfigCommit { scope } => {
                self.config_commit(fs, origin, &identity, scope)?;
                Ok(ok(out))
            }
            AppDataRequest::PublicRead {
                bundle_id,
                capacity,
            } => self.public_read(fs, origin, bundle_id, capacity, out),
            AppDataRequest::VaultRead { capacity } => {
                self.vault_read(fs, origin, &identity, capacity, out)
            }
            AppDataRequest::VaultSet { key, value } => {
                validate_key(key).map_err(|_| Errno::OutOfRange)?;
                // Refuse a value the format could not store before anything is
                // sealed, so a write cannot fail with the vault half replaced.
                let mut probe = Document::new();
                probe.set(key, value).map_err(|_| Errno::OutOfRange)?;
                self.vault_write(fs, origin, &identity, key, Some(value))?;
                Ok(ok(out))
            }
            AppDataRequest::VaultUnset { key } => {
                validate_key(key).map_err(|_| Errno::OutOfRange)?;
                self.vault_write(fs, origin, &identity, key, None)?;
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
        Self::record(
            &self.sink,
            origin,
            Refusal {
                err: StoreError::NoAppIdentity,
                app: None,
                target: None,
            },
        );
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
        scope: ConfigScope,
        capacity: u32,
        out: &mut [u8],
    ) -> Result<usize, Errno> {
        let store = self.open(fs, origin, identity, false)?;
        let outcome = store.merged_document(fs, scope);
        let mut document = self.resolve(origin, outcome)?;
        // A caller sees its own uncommitted work and no other principal's, so
        // a settings sheet reads back what it just set.
        if let Some(session) = self.session_for(origin) {
            session.apply(scope, &mut document)?;
        }
        encode_document_reply(&document.render(), capacity, out)
    }

    /// Answer a foreign read with what the named application publishes.
    ///
    /// Nothing about the *target* is reported to the caller: an application
    /// with no store here, one that publishes nothing, and one whose store
    /// cannot be attested all answer the same empty document, audited. So the
    /// only thing a reader learns is what an application chose to publish,
    /// which is the whole purpose of the scope, and a caller cannot use the
    /// endpoint to probe the state of stores it has no business knowing about.
    fn public_read<F: Storage + ?Sized>(
        &mut self,
        fs: &mut F,
        origin: &Origin,
        bundle_id: &str,
        capacity: u32,
        out: &mut [u8],
    ) -> Result<usize, Errno> {
        let outcome = published_document(fs, &mut self.roots, origin.uid(), bundle_id);
        // Borrowed immutably for the audit record only once the mutable borrow
        // of the cache has ended.
        let document = Self::sift(&self.sink, origin, bundle_id, outcome)?.unwrap_or_default();
        encode_document_reply(&document.render(), capacity, out)
    }

    /// Answer a read with the caller's whole sealed document.
    ///
    /// The sealed scope has no layer beneath it and no staging above it, so
    /// this is exactly what the application last sealed. A vault that cannot be
    /// opened is a refusal, never an empty answer: "your secrets are damaged"
    /// and "you have no secrets" must not look alike.
    ///
    /// The rendered plaintext is wiped once it has been framed, so the only
    /// copy that outlives the framing is the reply itself — which the serve loop
    /// wipes as soon as it has been posted.
    fn vault_read<F: Storage + ?Sized>(
        &mut self,
        fs: &mut F,
        origin: &Origin,
        identity: &AppIdentity,
        capacity: u32,
        out: &mut [u8],
    ) -> Result<usize, Errno> {
        let store = self.open(fs, origin, identity, false)?;
        let outcome = store.vault(fs);
        let document = self.resolve(origin, outcome)?;
        let mut text = document.render();
        let framed = encode_document_reply(&text, capacity, out);
        text.zeroize();
        framed
    }

    /// Apply one change to the caller's sealed document, immediately.
    ///
    /// There is no staging and no commit for the sealed scope: the store reads,
    /// applies, re-seals, and publishes before this returns. Plaintext secret
    /// material therefore exists in the service for the span of one request,
    /// and — because the service serves requests one at a time — the whole
    /// read-modify-seal-publish is atomic, so two processes of one application
    /// sealing different secrets cannot lose each other's.
    ///
    /// A removal passes `create = false`, so it can bring neither a store nor
    /// the account's key material into existence: a caller that removes a
    /// secret it never had changes nothing at all.
    fn vault_write<F: Storage + ?Sized>(
        &mut self,
        fs: &mut F,
        origin: &Origin,
        identity: &AppIdentity,
        key: &str,
        value: Option<&str>,
    ) -> Result<(), Errno> {
        let store = self.open(fs, origin, identity, value.is_some())?;
        let outcome = store.seal_change(fs, &mut self.entropy, key, value);
        self.resolve(origin, outcome)
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
        scope: ConfigScope,
    ) -> Result<(), Errno> {
        // A caller with nothing staged in this scope changed no setting, so
        // nothing is written: rewriting the document would cost its timestamp
        // for no change. Its edits in the *other* scope are left where they
        // are, for that scope's own commit.
        if !self
            .session_index(origin)
            .is_some_and(|at| self.sessions[at].has(scope))
        {
            return Ok(());
        }
        // A commit is the first act that may create the store, so this is the
        // one call that passes `create`.
        let store = self.open(fs, origin, identity, true)?;
        let mut document = self.read_document(fs, origin, &store, scope)?;
        // Re-resolved rather than carried across the mutable borrow above: an
        // index into a table this call may have touched is a trap, and the
        // lookup is over a handful of entries. Nothing below touches the table
        // until the publish has landed, so this one index serves both uses —
        // and a second lookup could otherwise report a failure for a commit
        // that had already succeeded.
        let index = self.session_index(origin).ok_or(Errno::NotFound)?;
        self.sessions[index].apply(scope, &mut document)?;
        let outcome = store.publish(fs, scope, &document);
        self.resolve(origin, outcome)?;
        // The staged edits are dropped only once the publish landed, so a
        // failed commit leaves them for a retry — and only this scope's, so a
        // settings sheet's unsaved work survives a publish about itself. A
        // session with nothing left in either scope is a table entry with no
        // purpose, so it goes with them.
        self.sessions[index].clear(scope);
        if self.sessions[index].is_empty() {
            self.sessions.remove(index);
        }
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
        scope: ConfigScope,
    ) -> Result<Document, Errno> {
        let outcome = store.document(fs, scope);
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
            Self::record(
                sink,
                origin,
                Refusal {
                    err,
                    app: origin.app(),
                    target: None,
                },
            );
            err.errno()
        })
    }

    /// Resolve an outcome on the **foreign** published path.
    ///
    /// A defect of the store being read is audited — naming the target, not the
    /// caller — and answered as an absence; every other refusal is the
    /// caller's own and is reported as itself. That split is what keeps a
    /// foreign read from becoming an oracle while still telling an operator
    /// which application's store is broken.
    fn sift<T>(
        sink: &S,
        origin: &Origin,
        target: &str,
        outcome: Result<T, StoreError>,
    ) -> Result<Option<T>, Errno> {
        match outcome {
            Ok(value) => Ok(Some(value)),
            Err(err) => {
                Self::record(
                    sink,
                    origin,
                    Refusal {
                        err,
                        app: origin.app(),
                        target: Some(target),
                    },
                );
                if err.is_target_defect() {
                    Ok(None)
                } else {
                    Err(err.errno())
                }
            }
        }
    }

    /// Write one refusal to `sink`.
    fn record(sink: &S, origin: &Origin, refusal: Refusal<'_>) {
        let fields = [
            Field {
                key: "bundle",
                value: FieldValue::Str(refusal.app.map_or("<none>", AppIdentity::bundle_id)),
            },
            Field {
                key: "uid",
                value: FieldValue::UnsignedInt(u64::from(origin.uid())),
            },
            // Named only when the store in question is not the caller's own,
            // so a foreign read's audit record says whose store was broken
            // rather than pinning another app's defect on the reader.
            Field {
                key: "target",
                value: FieldValue::Str(refusal.target.unwrap_or("")),
            },
        ];
        let named = if refusal.target.is_some() {
            fields.len()
        } else {
            fields.len() - 1
        };
        let _ = tairix_log::log(
            sink,
            &Event {
                level: level_of(refusal.err),
                id: events::id_of(refusal.err),
                message: refusal.err.reason(),
                fields: &fields[..named],
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
        | StoreError::NoAppIdentity
        // Every sealed-scope refusal is either an attack indication or the loss
        // of an account's key material, and none of them is a state the store
        // reaches in normal operation.
        | StoreError::Vault(_) => Level::Warn,
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
