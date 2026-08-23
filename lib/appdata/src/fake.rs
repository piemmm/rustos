//! A fake app-data service and volume for tests (feature `test-util`).
//!
//! It answers the `appdata-v1` wire exactly as `confd` does — decoding real
//! request frames, encoding real reply frames, holding a committed document
//! per configuration scope, a staging session, and a sealed document written
//! through immediately — so a test drives the codec the client and the service
//! actually share rather than a mock of it. That is also why it lives here
//! rather than in each consumer: an application migrating onto the store needs
//! the same fake, and two copies of it would be two different ideas of what the
//! service does.
//!
//! It does not *encrypt* the sealed scope: the sealing is the service's, behind
//! its own tests, and a fake that reimplemented it would be a second opinion
//! about a key hierarchy. What it does reproduce is everything the client can
//! observe — one document, no layers, no staging, a write applied before the
//! reply, and a refusal that is a refusal rather than an empty vault.
//!
//! Test scaffolding only. It is never part of a TAIRiX build: the feature is
//! enabled by a `[dev-dependencies]` entry, never by a program.

use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::Cell;

use tairix_abi::appdata_ipc::{encode_document_reply, AppDataRequest, ConfigScope};
use tairix_abi::reply::encode_status_reply;
use tairix_abi::Errno;
use tairix_appconf::Document;

use crate::AppDataHost;

/// How many scopes an application's own store has.
const SCOPES: usize = 2;

/// The array slot `scope`'s documents live in.
const fn slot(scope: ConfigScope) -> usize {
    match scope {
        ConfigScope::Private => 0,
        ConfigScope::Public => 1,
    }
}

/// A fake app-data service serving one program's store.
pub struct FakeService {
    /// The command word this fake resolves a bundle for; any other word names
    /// no bundle, exactly as an uninstalled program does.
    word: String,
    /// The committed document of each scope, as the service would hold it.
    committed: [Document; SCOPES],
    /// The sealed document, as the service would hold it — with no staging
    /// beside it, because a sealed write is applied before the reply.
    sealed: Document,
    /// When set, every sealed-scope call is refused with this: the damaged
    /// vault, the unreadable key material, the volume that is not there.
    sealed_refusal: Option<Errno>,
    /// The one caller's staged, uncommitted edits, per scope — because one
    /// commit publishes one document.
    staged: [Vec<(String, Option<String>)>; SCOPES],
    /// What other applications publish, by bundle identifier. An identifier
    /// absent from this list publishes nothing, which is exactly how the
    /// service answers for an application with no store.
    foreign: Vec<(String, Document)>,
    /// Files the bundles ship, by path.
    files: Vec<(String, Vec<u8>)>,
    /// Candidate bundle directories, in resolution order.
    candidates: Vec<String>,
    /// When set, every call is refused with this. Shared so a test can refuse
    /// mid-flight, while a handle holds the host.
    refuse: Rc<Cell<Option<Errno>>>,
    /// When set, only a document *read* is refused — the volume that goes away
    /// between a write that landed and the re-read that follows it. Shared for
    /// the same reason [`FakeService::refuse`] is.
    refuse_reads: Rc<Cell<Option<Errno>>>,
    /// Calls served, so a test can prove a read is one round trip.
    calls: usize,
    /// When set, the private document grows before every read — the concurrent
    /// writer the client's bounded read must not chase for ever.
    growing_writer: bool,
}

impl FakeService {
    /// A fake serving the program installed under `word`, with an empty store
    /// and no bundle.
    #[must_use]
    pub fn for_word(word: &str) -> Self {
        Self {
            word: String::from(word),
            committed: [Document::new(), Document::new()],
            sealed: Document::new(),
            sealed_refusal: None,
            staged: [Vec::new(), Vec::new()],
            foreign: Vec::new(),
            files: Vec::new(),
            candidates: Vec::new(),
            refuse: Rc::new(Cell::new(None)),
            refuse_reads: Rc::new(Cell::new(None)),
            calls: 0,
            growing_writer: false,
        }
    }

    /// Add `dir` as a candidate bundle that ships no defaults.
    #[must_use]
    pub fn with_bundle(mut self, dir: &str) -> Self {
        self.candidates.push(String::from(dir));
        self
    }

    /// Add `dir` as a candidate bundle shipping `text` as its defaults.
    #[must_use]
    pub fn with_defaults(mut self, dir: &str, text: &str) -> Self {
        self.candidates.push(String::from(dir));
        let mut path = String::from(dir.trim_end_matches('/'));
        path.push_str("/DefaultSettings/settings.conf");
        self.files.push((path, Vec::from(text.as_bytes())));
        self
    }

    /// Seed the committed private scope with `text`.
    ///
    /// # Panics
    ///
    /// If `text` is not a document the format accepts — a defect in the test's
    /// own fixture.
    #[must_use]
    pub fn with_store(self, text: &str) -> Self {
        self.with_scope(ConfigScope::Private, text)
    }

    /// Seed the calling program's own committed **published** scope with
    /// `text`.
    ///
    /// # Panics
    ///
    /// As [`Self::with_store`].
    #[must_use]
    pub fn with_published(self, text: &str) -> Self {
        self.with_scope(ConfigScope::Public, text)
    }

    /// Seed the committed document of `scope` with `text`.
    #[must_use]
    fn with_scope(mut self, scope: ConfigScope, text: &str) -> Self {
        self.committed[slot(scope)] = Document::parse(text).expect("a legal store fixture");
        self
    }

    /// Seed the sealed document with `text`.
    ///
    /// # Panics
    ///
    /// As [`Self::with_store`].
    #[must_use]
    pub fn with_sealed(mut self, text: &str) -> Self {
        self.sealed = Document::parse(text).expect("a legal store fixture");
        self
    }

    /// Refuse every sealed-scope call with `err` — the damaged vault an
    /// application must report rather than treat as empty.
    #[must_use]
    pub fn with_sealed_refusal(mut self, err: Errno) -> Self {
        self.sealed_refusal = Some(err);
        self
    }

    /// The sealed document — what a sealed write actually landed.
    #[must_use]
    pub const fn sealed(&self) -> &Document {
        &self.sealed
    }

    /// Seed what the *other* application `bundle_id` publishes.
    ///
    /// # Panics
    ///
    /// As [`Self::with_store`].
    #[must_use]
    pub fn with_foreign(mut self, bundle_id: &str, text: &str) -> Self {
        self.foreign.push((
            String::from(bundle_id),
            Document::parse(text).expect("a legal store fixture"),
        ));
        self
    }

    /// Grow the private document by one setting before every read.
    #[must_use]
    pub fn with_growing_writer(mut self) -> Self {
        self.growing_writer = true;
        self
    }

    /// The switch that refuses every call, for a test that must fail one
    /// while a handle is live.
    #[must_use]
    pub fn refusal(&self) -> Rc<Cell<Option<Errno>>> {
        Rc::clone(&self.refuse)
    }

    /// The switch that refuses only a document *read*, so a test can land a
    /// write and then fail the re-read that follows it.
    #[must_use]
    pub fn read_refusal(&self) -> Rc<Cell<Option<Errno>>> {
        Rc::clone(&self.refuse_reads)
    }

    /// Calls served so far.
    #[must_use]
    pub const fn calls(&self) -> usize {
        self.calls
    }

    /// The committed private document — what a publish actually landed.
    #[must_use]
    pub fn committed(&self) -> &Document {
        self.scope(ConfigScope::Private)
    }

    /// The committed published document — what other applications would read.
    #[must_use]
    pub fn published(&self) -> &Document {
        self.scope(ConfigScope::Public)
    }

    /// The committed document of `scope`.
    #[must_use]
    pub fn scope(&self, scope: ConfigScope) -> &Document {
        &self.committed[slot(scope)]
    }

    /// The document a read of `scope` answers with: the committed one plus the
    /// caller's staged edits, exactly as the service composes it.
    fn served(&self, scope: ConfigScope) -> Document {
        let mut document = copy_of(self.scope(scope));
        for (key, value) in &self.staged[slot(scope)] {
            match value {
                Some(value) => {
                    let _ = document.set(key, value);
                }
                None => document.unset(key),
            }
        }
        document
    }
}

/// A copy of `document`, through the format's own render/parse round trip —
/// the fixed point the format's fuzz harness holds.
fn copy_of(document: &Document) -> Document {
    Document::parse(&document.render()).expect("a rendered document re-parses")
}

impl AppDataHost for FakeService {
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
        self.calls += 1;
        if let Some(err) = self.refuse.get() {
            return Err(err);
        }
        let request = AppDataRequest::decode(request)?;
        if let Some(err) = self.refuse_reads.get() {
            if matches!(
                request,
                AppDataRequest::ConfigRead { .. }
                    | AppDataRequest::PublicRead { .. }
                    | AppDataRequest::VaultRead { .. }
            ) {
                return Err(err);
            }
        }
        // One gate for the whole sealed scope: a damaged vault, unreadable key
        // material, or an unreachable volume refuses every sealed operation,
        // and a refusal is never an empty vault.
        if let Some(err) = self.sealed_refusal {
            if matches!(
                request,
                AppDataRequest::VaultRead { .. }
                    | AppDataRequest::VaultSet { .. }
                    | AppDataRequest::VaultUnset { .. }
            ) {
                return Err(err);
            }
        }
        match request {
            AppDataRequest::ConfigRead { scope, capacity } => {
                if self.growing_writer && matches!(scope, ConfigScope::Private) {
                    let index = self.committed[slot(scope)].settings().count();
                    let mut key = String::from("grown.");
                    let _ = core::fmt::Write::write_fmt(&mut key, format_args!("{index}"));
                    let _ = self.committed[slot(scope)].set(&key, "a value long enough to matter");
                }
                encode_document_reply(&self.served(scope).render(), capacity, reply)
            }
            AppDataRequest::ConfigSet { scope, key, value } => {
                self.staged[slot(scope)].push((String::from(key), Some(String::from(value))));
                Ok(status(Ok(()), reply))
            }
            AppDataRequest::ConfigUnset { scope, key } => {
                self.staged[slot(scope)].push((String::from(key), None));
                Ok(status(Ok(()), reply))
            }
            AppDataRequest::ConfigCommit { scope } => {
                self.committed[slot(scope)] = self.served(scope);
                self.staged[slot(scope)].clear();
                Ok(status(Ok(()), reply))
            }
            AppDataRequest::VaultRead { capacity } => {
                encode_document_reply(&self.sealed.render(), capacity, reply)
            }
            AppDataRequest::VaultSet { key, value } => {
                // Applied before the reply, exactly as the service does: there
                // is no commit for the sealed scope.
                self.sealed.set(key, value).map_err(|_| Errno::OutOfRange)?;
                Ok(status(Ok(()), reply))
            }
            AppDataRequest::VaultUnset { key } => {
                self.sealed.unset(key);
                Ok(status(Ok(()), reply))
            }
            AppDataRequest::PublicRead {
                bundle_id,
                capacity,
            } => {
                // The committed document, never a staged edit: a published
                // value is what every other application sees. An identifier
                // nothing published under answers empty, exactly as the service
                // answers for an application with no store.
                let text = self
                    .foreign
                    .iter()
                    .find(|(known, _)| known == bundle_id)
                    .map_or_else(String::new, |(_, document)| document.render());
                encode_document_reply(&text, capacity, reply)
            }
        }
    }

    fn read_file(&mut self, path: &str, cap: usize) -> Result<Vec<u8>, Errno> {
        let bytes = self
            .files
            .iter()
            .find(|(known, _)| known == path)
            .map(|(_, bytes)| bytes.clone())
            .ok_or(Errno::NotFound)?;
        if bytes.len() > cap {
            return Err(Errno::LengthOutOfRange);
        }
        Ok(bytes)
    }

    fn bundle_candidates(&mut self, word: &str) -> Vec<String> {
        if word == self.word {
            self.candidates.clone()
        } else {
            Vec::new()
        }
    }
}

/// Write the shared status frame into `reply`, returning its length.
fn status(outcome: Result<(), Errno>, reply: &mut [u8]) -> usize {
    let frame = encode_status_reply(outcome);
    let len = frame.len().min(reply.len());
    reply[..len].copy_from_slice(&frame[..len]);
    len
}
