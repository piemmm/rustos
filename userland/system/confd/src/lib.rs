//! TAIRiX app-data service — the dispatcher.
//!
//! `confd` is the only principal in the system holding
//! [`CapabilityId::APPDATA_ADMIN`](tairix_abi::CapabilityId::APPDATA_ADMIN),
//! and that capability is the per-inode gate on every user's per-app store
//! tree. So it is the only path to an application's stored settings, and it
//! answers each request against the store it derives from the caller's
//! kernel-attested [`Origin`] — never from anything on the wire. A caller
//! running no verified bundle has no store and is refused whichever operation
//! it sent.
//!
//! What the engine does that a reader of it should not have to infer:
//!
//! - **A foreign read is no oracle.** An application that publishes nothing,
//!   has never run for this account, or whose store cannot be attested all
//!   answer the same empty document, so `PublicRead` reveals only what an
//!   application chose to publish. The *caller's* own refusals — no home, a
//!   root the service does not own, an unreachable volume — are reported as
//!   themselves, because only those are worth a retry.
//! - **A sealed write is immediate and atomic.** The service opens the sealed
//!   document, applies the one change, re-seals it, and publishes it before it
//!   replies, so plaintext secret material exists here for the span of one
//!   request rather than for the life of a staging session. Requests are served
//!   one at a time, so two processes of one application sealing different
//!   secrets cannot lose each other's — which a stage-then-commit pair would
//!   allow.
//! - **A sealed document that cannot be opened is refused** ([`vault`]), never
//!   answered as an empty vault: "your secrets are damaged" and "you have none"
//!   must not look alike to an application deciding whether to prompt.
//! - **A commit publishes one scope.** Edits staged against the caller's other
//!   scope are untouched, because one rename replaces one name. A caller that
//!   never commits changes nothing on the volume, and its own reads see its own
//!   pending edits, so a settings sheet reads back what it just set.
//! - **Every staging ceiling is decided before an edit is written**, so a
//!   refusal leaves the caller's earlier work untouched, and a commit or the
//!   idle reclaim returns the space.
//! - **A temporary file's lifetime is the boot, carried in its name**, so an
//!   earlier boot's file is invisible to every answer and is reclaimed before
//!   the next is created ([`bulk`]).
//!
//! This crate is `no_std` (with `alloc`) and performs **no I/O** and draws no
//! randomness of its own: every read and write goes through the injected
//! [`Storage`] seam and every draw through the injected [`Entropy`] seam, so
//! the whole engine is exercised on the host. The service *binary*
//! (`src/run.rs`) supplies the real seams over the `fs_*` and `random_get`
//! syscalls.
//!
//! The request table, the tree it serves from, the staging ceilings, and why
//! the store is a service at all are `docs/src/userland/confd.md`.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::appdata_ipc::{
    encode_blob_list_reply, encode_document_reply, encode_grant_reply, encode_quota_reply,
    encode_temp_reply, AppDataRequest, BlobMode, ConfigScope,
};
use tairix_abi::reply::encode_status_reply;
use tairix_abi::{AppIdentity, BootId, Errno, Origin, ProcId};
use tairix_appconf::{validate_key, ConfError, Document, MAX_SETTINGS};
use tairix_log::{Event, EventId, Field, FieldValue, Level, Sink};
use zeroize::Zeroize;

pub mod bulk;
pub mod events;
mod owner;
pub mod store;
#[cfg(test)]
mod testfs;
pub mod vault;

pub use bulk::{BlobStore, TempNames, TempStore};
pub use owner::OwnerPin;
pub use store::{published_document, AppStore, RootCache, StoreError};
pub use vault::{Entropy, VaultError};

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

/// Charged bytes the whole staging table may hold, across every account.
///
/// The service's worst-case staging footprint, and the figure to check against
/// a small machine: 8 MiB is under one percent of the smallest memory profile
/// TAIRiX serves large volumes on, and some four orders of magnitude above any
/// real concurrent load — a settings save is a few hundred bytes and lives from
/// the first edit to the commit. Nothing bounded the sum before, so the table
/// grew with the calls every account's applications could make inside the
/// reclaim window, which on that machine is a denial of service against every
/// application's settings.
///
/// A fixed containment bound rather than a capacity derived from discovered
/// memory: it bounds what untrusted callers may make a boot-floor service hold,
/// and a bigger machine letting them hold proportionally more would be a
/// regression, not flexibility.
pub const STAGING_TOTAL_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Staging sessions the whole table may hold, across every account.
///
/// Bounds the table's *entry* count, which the byte ceiling alone does not: a
/// session holding one one-byte key costs almost nothing yet still has to be
/// searched on every request. Every lookup is a scan of this table, so the
/// count is what keeps that scan cheap.
pub const STAGING_MAX_SESSIONS: usize = 512;

/// How many accounts are guaranteed to be able to stage at the same time.
///
/// The fairness divisor: one account may hold this fraction of the table and no
/// more, so filling it takes at least this many distinct accounts and no single
/// account — or application, which cannot outrank its account — can deny the
/// others their settings. Reaching for a larger number of accounts would shrink
/// each one's share below a document, and the shares are the point.
pub const STAGING_ACCOUNT_SHARES: usize = 16;

/// Charged bytes one account may hold across all of its sessions.
pub const STAGING_ACCOUNT_MAX_BYTES: usize = STAGING_TOTAL_MAX_BYTES / STAGING_ACCOUNT_SHARES;

/// Staging sessions one account may hold at once.
///
/// Its share of the table's entries, for the same reason it has a share of the
/// bytes: without it an account's applications could take every entry with
/// sessions too small to reach the byte ceiling.
pub const STAGING_ACCOUNT_MAX_SESSIONS: usize = STAGING_MAX_SESSIONS / STAGING_ACCOUNT_SHARES;

/// Charged bytes one calling process instance may hold.
///
/// Half its account's share, so one application cannot spend the whole of it
/// and deny a sibling application of the same user — which the per-account
/// ceiling alone would allow, since all of a user's applications run as that
/// user. Half of the share still holds a full rewrite of both of an
/// application's documents at the format's maximum size, so no legal edit is
/// refused by it.
pub const STAGING_SESSION_MAX_BYTES: usize = STAGING_ACCOUNT_MAX_BYTES / 2;

/// One node's metadata, as the store needs it.
///
/// Two questions of one `stat`: who owns a directory the store is about to be
/// served out of, and how long a bulk file is. Asking them separately would be
/// two syscalls for one answer on the quota path, which walks every file an
/// application holds.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct NodeInfo {
    /// The uid owning the node.
    pub uid: u32,
    /// Its length in bytes; meaningless for a directory.
    pub len: u64,
}

/// One entry of a directory listing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirEntry {
    /// The entry's name, with no path component before it.
    pub name: String,
    /// Whether it is itself a directory.
    pub dir: bool,
}

/// The filesystem the store lives on, as the dispatcher needs it.
///
/// Deliberately narrow: whole-file reads and writes, a rename, an unlink, a
/// directory create, a stat, a directory listing, and the descriptor
/// delegation the bulk scopes hand out. Configuration documents have no
/// partial write and no seek, because such a document is only ever replaced
/// whole — which is what makes the publish atomic; a blob is the opposite
/// shape, which is exactly why it is reached as a descriptor rather than
/// through this trait. Every method reports [`Errno::NotFound`] for an absent
/// path so the engine can tell "not there" from "cannot be reached" and fail
/// closed on the second.
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

    /// Remove the file at `path`.
    ///
    /// # Errors
    ///
    /// [`Errno::NotFound`] when the path does not exist; any other [`Errno`]
    /// for a failed unlink.
    fn unlink(&mut self, path: &str) -> Result<(), Errno>;

    /// The metadata of the node at `path`.
    ///
    /// # Errors
    ///
    /// [`Errno::NotFound`] when the path does not exist; any other [`Errno`]
    /// for a failed stat.
    fn stat(&mut self, path: &str) -> Result<NodeInfo, Errno>;

    /// The entries of the directory `path`, excluding `.` and `..`.
    ///
    /// # Errors
    ///
    /// [`Errno::NotFound`] when the path does not exist; any other [`Errno`]
    /// for a failed listing.
    fn list_dir(&mut self, path: &str) -> Result<Vec<DirEntry>, Errno>;

    /// Open the file at `path` and delegate it to the live task `task` as a
    /// one-shot grant, returning the handle the task redeems.
    ///
    /// `write` decides both what the delegation conveys and whether an absent
    /// file is created; `ceiling` is the highest length the holder may write
    /// or truncate it to, and is meaningful only for a writable delegation.
    ///
    /// The service's own descriptor does not outlive the call: a delegation
    /// record is self-contained, and there is no primitive by which a server
    /// learns that a peer closed a descriptor, so one kept open per
    /// outstanding grant could never be reclaimed.
    ///
    /// # Errors
    ///
    /// [`Errno::NotFound`] when a read-only open names a file that does not
    /// exist, or when `task` is not live; any other [`Errno`] the open or the
    /// delegation reports.
    fn grant(&mut self, path: &str, write: bool, ceiling: u64, task: u64) -> Result<u64, Errno>;
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

impl PendingEdit {
    /// Charged bytes for an edit of `key` to `value`.
    ///
    /// The record is charged as well as the text it owns, because a thousand
    /// one-byte keys cost the table a thousand records and charging only the
    /// text would let them past every ceiling. Allocator slack above this is a
    /// bounded constant factor and is not modelled.
    fn charge_of(key: &str, value: Option<&str>) -> usize {
        core::mem::size_of::<Self>() + key.len() + value.map_or(0, str::len)
    }

    /// Charged bytes for this edit.
    fn charge(&self) -> usize {
        Self::charge_of(&self.key, self.value.as_deref())
    }
}

/// The uncommitted edits of one calling process instance.
struct Session {
    /// The account the process runs as, so the table can be summed per
    /// principal. Kernel-attested, never a wire claim, and re-read on every
    /// touch so the table holds no stale principal.
    uid: u32,
    /// The process instance that staged them. Unforgeable and never reused, so
    /// two processes of the same application can never share a session and one
    /// cannot publish the other's half-finished edits.
    proc_id: ProcId,
    /// The monotonic instant of the last request that touched this session.
    touched_ns: u64,
    edits: Vec<PendingEdit>,
    /// Charged bytes this session holds, maintained by [`Self::recharge`] after
    /// every change to `edits`. Kept rather than recomputed because the
    /// per-account and whole-table sums are taken on every staged edit, and
    /// walking every session's edits for each of them would make the check
    /// itself the denial of service it exists to prevent.
    charged: usize,
}

impl Session {
    /// An empty session for `origin`, first touched at `now_ns`.
    fn new(origin: &Origin, now_ns: u64) -> Self {
        Self {
            uid: origin.uid(),
            proc_id: origin.proc_id(),
            touched_ns: now_ns,
            edits: Vec::new(),
            charged: Self::EMPTY_CHARGE,
        }
    }

    /// Charged bytes a session costs before it holds any edit.
    const EMPTY_CHARGE: usize = core::mem::size_of::<Self>();

    /// Stage `key = value` in `scope`, or a removal when `value` is [`None`],
    /// replacing any edit already staged for that key in that scope.
    ///
    /// Infallible: whether the table has room is the table's decision
    /// ([`AppData::admit`]), taken before this is reached, so a session never
    /// half-applies an edit it then has to refuse.
    fn stage(&mut self, scope: ConfigScope, key: &str, value: Option<&str>) {
        let staged = value.map(String::from);
        if let Some(edit) = self
            .edits
            .iter_mut()
            .find(|edit| edit.scope == scope && edit.key == key)
        {
            edit.value = staged;
        } else {
            self.edits.push(PendingEdit {
                scope,
                key: String::from(key),
                value: staged,
            });
        }
        self.recharge();
    }

    /// Recompute [`Self::charged`] from the edits held.
    fn recharge(&mut self) {
        self.charged =
            Self::EMPTY_CHARGE + self.edits.iter().map(PendingEdit::charge).sum::<usize>();
    }

    /// What [`Self::charged`] would become with `key = value` staged in
    /// `scope`, without staging it.
    ///
    /// Summed over the edits that would survive rather than adjusted from
    /// [`Self::charged`], so an edit that replaces one already staged cannot be
    /// counted on top of it and the prediction stands on the edits themselves.
    fn charge_with(&self, scope: ConfigScope, key: &str, value: Option<&str>) -> usize {
        Self::EMPTY_CHARGE
            + PendingEdit::charge_of(key, value)
            + self
                .edits
                .iter()
                .filter(|edit| edit.scope != scope || edit.key != key)
                .map(PendingEdit::charge)
                .sum::<usize>()
    }

    /// Whether staging `key` in `scope` would stay inside the per-scope key
    /// bound: either the key is already staged there, or the scope has room.
    fn admits(&self, scope: ConfigScope, key: &str) -> bool {
        self.edits
            .iter()
            .any(|edit| edit.scope == scope && edit.key == key)
            || self.staged_in(scope) < MAX_PENDING_EDITS
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
        self.recharge();
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
    /// The naming rule the temporary scope serves under, or [`None`] when the
    /// running boot has no identity and the scope is refused whole.
    ///
    /// Read once, at construction: the kernel mints one boot identity per boot
    /// and never a second, so a value that is absent here stays absent for the
    /// life of this boot and re-reading it per request would buy a syscall
    /// nothing.
    temp: Option<TempNames>,
}

impl<S: Sink, R: Entropy> AppData<S, R> {
    /// Build a dispatcher that records its decisions through `sink`, draws the
    /// sealed scope's key material and the temporary scope's names from
    /// `entropy`, and tells this boot's scratch from an earlier boot's by
    /// `boot`.
    #[must_use]
    pub fn new(sink: S, entropy: R, boot: BootId) -> Self {
        Self {
            sessions: Vec::new(),
            roots: RootCache::new(),
            sink,
            entropy,
            temp: TempNames::of(boot),
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
    ///
    /// `request` is **consumed**: a sealed-scope frame carries a secret in the
    /// clear, so every byte of it is wiped before this returns, on the served
    /// and the refused path alike. Doing it here rather than in each host is
    /// what makes it cover a transport that would otherwise leave a plaintext
    /// frame in a buffer it reuses across callers.
    #[must_use]
    pub fn serve<F: Storage + ?Sized>(
        &mut self,
        fs: &mut F,
        origin: &Origin,
        now_ns: u64,
        request: &mut [u8],
        out: &mut [u8],
    ) -> usize {
        self.reclaim_idle(now_ns);
        let served = match self.dispatch(fs, origin, now_ns, request, out) {
            Ok(len) => len,
            Err(err) => reply(Err(err), out),
        };
        request.zeroize();
        served
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
                self.stage(origin, now_ns, scope, key, Some(value))?;
                Ok(ok(out))
            }
            AppDataRequest::ConfigUnset { scope, key } => {
                validate_key(key).map_err(|_| Errno::OutOfRange)?;
                self.stage(origin, now_ns, scope, key, None)?;
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
            AppDataRequest::BlobOpen { name, mode } => {
                self.blob_open(fs, origin, &identity, name, mode, out)
            }
            AppDataRequest::BlobDelete { name } => {
                self.blob_delete(fs, origin, &identity, name)?;
                Ok(ok(out))
            }
            AppDataRequest::BlobList { capacity } => {
                self.blob_list(fs, origin, &identity, capacity, out)
            }
            AppDataRequest::QuotaGet {} => self.bulk_quota(fs, origin, &identity, out),
            AppDataRequest::TempCreate {} => self.temp_create(fs, origin, &identity, out),
            AppDataRequest::TempRelease { name } => {
                self.temp_release(fs, origin, &identity, name)?;
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

    /// Answer a blob open with a one-shot descriptor grant for it.
    ///
    /// The grant is minted to the caller's **attested** task id, so it can be
    /// redeemed by nothing else — the handle in the reply is useless to a
    /// bystander that intercepted it. The service never touches a byte of the
    /// blob: it decides here, once, and the application then reads and writes
    /// directly against the kernel VFS, bounded by the extent the delegation
    /// carries.
    ///
    /// A write creates the blob and so may create the store; a read passes
    /// `create = false` throughout, so a caller cannot bring a store into
    /// existence by asking to read from one.
    fn blob_open<F: Storage + ?Sized>(
        &mut self,
        fs: &mut F,
        origin: &Origin,
        identity: &AppIdentity,
        name: &str,
        mode: BlobMode,
        out: &mut [u8],
    ) -> Result<usize, Errno> {
        let create = mode.is_write();
        let blobs = self.blobs(fs, origin, identity, create)?;
        let outcome = blobs.grant(fs, name, mode, origin.pid());
        let handle = self.resolve(origin, outcome)?;
        encode_grant_reply(handle, out)
    }

    /// Delete one of the caller's own blobs.
    ///
    /// Nothing is created on this path: a delete of a blob — or of a store —
    /// that does not exist removes nothing and succeeds, so it is neither an
    /// oracle for what exists nor a way to provision a store.
    fn blob_delete<F: Storage + ?Sized>(
        &mut self,
        fs: &mut F,
        origin: &Origin,
        identity: &AppIdentity,
        name: &str,
    ) -> Result<(), Errno> {
        let blobs = self.blobs(fs, origin, identity, false)?;
        let outcome = blobs.delete(fs, name);
        self.resolve(origin, outcome)
    }

    /// Answer a listing with every blob the caller holds and its length.
    ///
    /// Whole or nothing, under the same capacity negotiation a document read
    /// uses: the blob count is bounded, so a whole listing fits one reply and
    /// no caller acts on one spliced out of two snapshots.
    fn blob_list<F: Storage + ?Sized>(
        &mut self,
        fs: &mut F,
        origin: &Origin,
        identity: &AppIdentity,
        capacity: u32,
        out: &mut [u8],
    ) -> Result<usize, Errno> {
        let blobs = self.blobs(fs, origin, identity, false)?;
        let outcome = blobs.listing(fs).and_then(|listing| {
            let rendered = bulk::render_listing(&listing)?;
            Ok(rendered)
        });
        let rendered = self.resolve(origin, outcome)?;
        encode_blob_list_reply(&rendered, capacity, out)
    }

    /// Answer a quota read with the caller's bulk usage and its ceilings.
    ///
    /// Both scopes in one answer, off one resolution of the configuration
    /// store: an application deciding whether to spill to scratch or evict a
    /// cached index needs both figures, and two calls could report two moments.
    fn bulk_quota<F: Storage + ?Sized>(
        &mut self,
        fs: &mut F,
        origin: &Origin,
        identity: &AppIdentity,
        out: &mut [u8],
    ) -> Result<usize, Errno> {
        let store = self.open(fs, origin, identity, false)?;
        let blobs = Self::judge(&self.sink, origin, BlobStore::open(fs, &store, false))?;
        let blobs = self.resolve(origin, blobs.usage(fs))?;
        // An application whose boot has no temporary scope holds no temporary
        // files, which is exactly what the scope answers everywhere else — the
        // usage read is not the place to refuse a caller asking about its
        // blobs.
        let temps = match self.temp {
            Some(names) => {
                let temp = TempStore::open(fs, &store, names, false);
                let temp = Self::judge(&self.sink, origin, temp)?;
                self.resolve(origin, temp.usage(fs))?
            }
            None => (0, 0),
        };
        encode_quota_reply(&bulk::quota(blobs, temps), out)
    }

    /// Answer a temporary-file create with a one-shot descriptor grant and the
    /// name the service gave the file.
    ///
    /// The caller named nothing, so there is nothing to validate on the way in:
    /// the name comes from this boot's naming rule and a drawn slot, which is
    /// what makes a fresh file fresh without two instances of one application
    /// having to agree on anything.
    fn temp_create<F: Storage + ?Sized>(
        &mut self,
        fs: &mut F,
        origin: &Origin,
        identity: &AppIdentity,
        out: &mut [u8],
    ) -> Result<usize, Errno> {
        let temp = self.temps(fs, origin, identity, true)?;
        let outcome = temp.create(fs, &mut self.entropy, origin.pid());
        let (handle, name) = self.resolve(origin, outcome)?;
        encode_temp_reply(handle, &name, out)
    }

    /// Delete one of the caller's own temporary files.
    ///
    /// Nothing is created on this path: releasing a file — or a store — that
    /// does not exist removes nothing and succeeds, so it is neither an oracle
    /// for what exists nor a way to provision a store.
    fn temp_release<F: Storage + ?Sized>(
        &mut self,
        fs: &mut F,
        origin: &Origin,
        identity: &AppIdentity,
        name: &str,
    ) -> Result<(), Errno> {
        let temp = self.temps(fs, origin, identity, false)?;
        let outcome = temp.release(fs, name);
        self.resolve(origin, outcome)
    }

    /// Open the caller's blob store, auditing and translating any refusal.
    ///
    /// The configuration store is opened first, because the ownership pin lives
    /// there and governs both trees: a squatting publisher is refused before a
    /// byte of another developer's blobs is reachable. `create` is threaded
    /// into *both* opens, which is what makes that guarantee hold rather than
    /// merely apply to applications that also wrote a setting — creating a
    /// blob creates the pin first, so a store can never hold data whose owner
    /// was never recorded, and a later publisher claiming the identifier
    /// cannot inherit an unattested one. A read creates neither.
    fn blobs<F: Storage + ?Sized>(
        &mut self,
        fs: &mut F,
        origin: &Origin,
        identity: &AppIdentity,
        create: bool,
    ) -> Result<BlobStore, Errno> {
        let store = self.open(fs, origin, identity, create)?;
        let outcome = BlobStore::open(fs, &store, create);
        Self::judge(&self.sink, origin, outcome)
    }

    /// Open the caller's temporary files, auditing and translating any refusal.
    ///
    /// The same pin discipline [`Self::blobs`] follows, for the same reason:
    /// one `.owner` record governs both trees, so a squatting publisher is
    /// refused before a byte of another developer's scratch is reachable.
    ///
    /// A boot with no identity refuses here, before the store is opened: the
    /// service could not tell this boot's scratch from an earlier boot's, so it
    /// serves none rather than leaving files it could never reclaim.
    fn temps<F: Storage + ?Sized>(
        &mut self,
        fs: &mut F,
        origin: &Origin,
        identity: &AppIdentity,
        create: bool,
    ) -> Result<TempStore, Errno> {
        let names = self
            .temp
            .ok_or_else(|| Self::refuse(&self.sink, origin, StoreError::TempUnavailable))?;
        let store = self.open(fs, origin, identity, create)?;
        let outcome = TempStore::open(fs, &store, names, create);
        Self::judge(&self.sink, origin, outcome)
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
        outcome.map_err(|err| Self::refuse(sink, origin, err))
    }

    /// Audit `err` against the caller's own store and answer the typed refusal
    /// it maps to.
    ///
    /// The one place a store refusal becomes an errno, so a path that refuses
    /// before there is an outcome to judge cannot pick a different one — or
    /// forget the audit record.
    fn refuse(sink: &S, origin: &Origin, err: StoreError) -> Errno {
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

    /// Stage `key = value` in `scope` against the calling process instance's
    /// session, creating it if absent, or refuse.
    ///
    /// Every ceiling is decided before anything is written, so a refused edit
    /// leaves the caller's earlier work exactly as it was.
    ///
    /// # Errors
    ///
    /// [`Errno::LimitExceeded`] when any staging ceiling has no room for the
    /// edit. Which one is in the audit stream, not in the reply.
    fn stage(
        &mut self,
        origin: &Origin,
        now_ns: u64,
        scope: ConfigScope,
        key: &str,
        value: Option<&str>,
    ) -> Result<(), Errno> {
        let index = self.session_index(origin);
        let charged = match index {
            Some(at) => {
                let session = &self.sessions[at];
                if !session.admits(scope, key) {
                    return Err(Self::refuse(&self.sink, origin, StoreError::StagingSpent));
                }
                session.charge_with(scope, key, value)
            }
            None => Session::EMPTY_CHARGE + PendingEdit::charge_of(key, value),
        };
        self.admit(origin, index, charged)?;
        let at = if let Some(at) = index {
            at
        } else {
            self.sessions.push(Session::new(origin, now_ns));
            self.sessions.len() - 1
        };
        self.sessions[at].uid = origin.uid();
        self.sessions[at].touched_ns = now_ns;
        self.sessions[at].stage(scope, key, value);
        Ok(())
    }

    /// Whether the table has room for the session at `index` — or for a new one
    /// when that is [`None`] — to cost `charged` bytes.
    ///
    /// The one place every staging ceiling is decided. Each sum skips the
    /// session being changed, so `charged` replaces its current cost rather
    /// than adding to it.
    ///
    /// # Errors
    ///
    /// [`Errno::LimitExceeded`] for whichever ceiling has no room. A ceiling
    /// the caller can free by committing is told from one it cannot in the
    /// audit stream; the reply is the same either way, so being refused reports
    /// nothing about another account's staging.
    fn admit(&self, origin: &Origin, index: Option<usize>, charged: usize) -> Result<(), Errno> {
        if charged > STAGING_SESSION_MAX_BYTES {
            return Err(Self::refuse(&self.sink, origin, StoreError::StagingSpent));
        }
        let uid = origin.uid();
        let entry_taken = index.is_none()
            && (self.sessions.len() >= STAGING_MAX_SESSIONS
                || self.account_sessions(uid) >= STAGING_ACCOUNT_MAX_SESSIONS);
        let share_spent = self.account_charge(uid, index) + charged > STAGING_ACCOUNT_MAX_BYTES
            || self.total_charge(index) + charged > STAGING_TOTAL_MAX_BYTES;
        if entry_taken || share_spent {
            return Err(Self::refuse(
                &self.sink,
                origin,
                StoreError::StagingUnavailable,
            ));
        }
        Ok(())
    }

    /// Charged bytes held by `uid`'s sessions, skipping the one at `except`.
    fn account_charge(&self, uid: u32, except: Option<usize>) -> usize {
        self.charge_where(except, |session| session.uid == uid)
    }

    /// Charged bytes held by every session, skipping the one at `except`.
    fn total_charge(&self, except: Option<usize>) -> usize {
        self.charge_where(except, |_| true)
    }

    /// Charged bytes held by the sessions `keep` selects, skipping `except`.
    fn charge_where(&self, except: Option<usize>, keep: impl Fn(&Session) -> bool) -> usize {
        self.sessions
            .iter()
            .enumerate()
            .filter(|(at, session)| Some(*at) != except && keep(session))
            .map(|(_, session)| session.charged)
            .sum()
    }

    /// How many sessions `uid` holds.
    fn account_sessions(&self, uid: u32) -> usize {
        self.sessions
            .iter()
            .filter(|session| session.uid == uid)
            .count()
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

    /// Charged bytes the staging table holds, against
    /// [`STAGING_TOTAL_MAX_BYTES`]. Test and diagnostic surface.
    #[must_use]
    pub fn staging_charged(&self) -> usize {
        self.total_charge(None)
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
        | StoreError::Vault(_)
        // A store name the wire decoder already refuses can only reach the
        // store if a check was bypassed, so it is an attack indication.
        | StoreError::StoreNameRefused
        // A boot with no identity means the kernel's random reserve never
        // seeded, which an operator has to know about.
        | StoreError::TempUnavailable
        // A staging table with no room denies every account's settings, so an
        // operator has to see it whether the cause is abuse or genuine load.
        | StoreError::StagingUnavailable => Level::Warn,
        StoreError::NoHome
        | StoreError::DocumentRefused
        | StoreError::Unavailable
        // An application reading a blob it has not created yet is its first
        // launch, and one that has filled either scope has outgrown the working
        // set that scope is for. Neither is an attack indication.
        | StoreError::BlobNotFound
        | StoreError::BlobLimit
        | StoreError::TempLimit
        // A caller that has staged its whole allowance is a settings sheet that
        // never saves, or a runaway writer; either way not an attack.
        | StoreError::StagingSpent => Level::Info,
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
