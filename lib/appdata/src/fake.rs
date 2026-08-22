//! A fake app-data service and volume for tests (feature `test-util`).
//!
//! It answers the `appdata-v1` wire exactly as `confd` does — decoding real
//! request frames, encoding real reply frames, holding a committed document
//! and a staging session — so a test drives the codec the client and the
//! service actually share rather than a mock of it. That is also why it lives
//! here rather than in each consumer: an application migrating onto the store
//! needs the same fake, and two copies of it would be two different ideas of
//! what the service does.
//!
//! Test scaffolding only. It is never part of a TAIRiX build: the feature is
//! enabled by a `[dev-dependencies]` entry, never by a program.

use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::Cell;

use tairix_abi::appdata_ipc::{encode_document_reply, AppDataRequest};
use tairix_abi::reply::encode_status_reply;
use tairix_abi::Errno;
use tairix_appconf::Document;

use crate::AppDataHost;

/// A fake app-data service serving one program's store.
pub struct FakeService {
    /// The command word this fake resolves a bundle for; any other word names
    /// no bundle, exactly as an uninstalled program does.
    word: String,
    /// The committed document, as the service would hold it.
    committed: Document,
    /// The one caller's staged, uncommitted edits.
    staged: Vec<(String, Option<String>)>,
    /// Files the bundles ship, by path.
    files: Vec<(String, Vec<u8>)>,
    /// Candidate bundle directories, in resolution order.
    candidates: Vec<String>,
    /// When set, every call is refused with this. Shared so a test can refuse
    /// mid-flight, while a handle holds the host.
    refuse: Rc<Cell<Option<Errno>>>,
    /// Calls served, so a test can prove a read is one round trip.
    calls: usize,
    /// When set, the document grows before every read — the concurrent writer
    /// the client's bounded read must not chase for ever.
    growing_writer: bool,
}

impl FakeService {
    /// A fake serving the program installed under `word`, with an empty store
    /// and no bundle.
    #[must_use]
    pub fn for_word(word: &str) -> Self {
        Self {
            word: String::from(word),
            committed: Document::new(),
            staged: Vec::new(),
            files: Vec::new(),
            candidates: Vec::new(),
            refuse: Rc::new(Cell::new(None)),
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

    /// Seed the committed store with `text`.
    ///
    /// # Panics
    ///
    /// If `text` is not a document the format accepts — a defect in the test's
    /// own fixture.
    #[must_use]
    pub fn with_store(mut self, text: &str) -> Self {
        self.committed = Document::parse(text).expect("a legal store fixture");
        self
    }

    /// Grow the document by one setting before every read.
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

    /// Calls served so far.
    #[must_use]
    pub const fn calls(&self) -> usize {
        self.calls
    }

    /// The committed document — what a publish actually landed.
    #[must_use]
    pub const fn committed(&self) -> &Document {
        &self.committed
    }

    /// The document a read answers with: the committed one plus the caller's
    /// staged edits, exactly as the service composes it.
    fn served(&self) -> Document {
        let mut document = copy_of(&self.committed);
        for (key, value) in &self.staged {
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
        match AppDataRequest::decode(request)? {
            AppDataRequest::ConfigRead { capacity } => {
                if self.growing_writer {
                    let index = self.committed.settings().count();
                    let mut key = String::from("grown.");
                    let _ = core::fmt::Write::write_fmt(&mut key, format_args!("{index}"));
                    let _ = self.committed.set(&key, "a value long enough to matter");
                }
                encode_document_reply(&self.served().render(), capacity, reply)
            }
            AppDataRequest::ConfigSet { key, value } => {
                self.staged
                    .push((String::from(key), Some(String::from(value))));
                Ok(status(Ok(()), reply))
            }
            AppDataRequest::ConfigUnset { key } => {
                self.staged.push((String::from(key), None));
                Ok(status(Ok(()), reply))
            }
            AppDataRequest::ConfigCommit => {
                self.committed = self.served();
                self.staged.clear();
                Ok(status(Ok(()), reply))
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
