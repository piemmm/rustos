//! What the file manager reads off its event loop, and the policy that decides
//! when it has an answer.
//!
//! Every read this app makes is a read of somebody's disk: the directory the
//! user navigated to, the folder cue each visible folder draws, and the program
//! stores the *Open With…* chooser is built from. Run on the loop that owes the
//! window a frame, each one freezes the window for as long as that disk takes —
//! which on a slow or contended device is not a frame but a visible stall.
//!
//! So they run on a worker, and the loop learns an answer landed through the
//! wait-set it already parks in. The listing and scan policies are the shared
//! ones ([`tairix_browse::ListingDesk`] and `tairix_util::defer::JobDesk`, the
//! latter linked into the target build alone); what is here is the one policy
//! neither covers.
//!
//! # The folder-occupancy probe
//!
//! A folder draws an empty/non-empty cue, and the only way to know which is to
//! read the folder. The renderer asks for the cue of every folder it draws, on
//! every frame, so the ask must cost nothing: [`Probes`] answers what it
//! already knows and *records* the rest, which is what lets a paint resolve
//! occupancy while performing no I/O at all.
//!
//! The recorded set is drained as one batch rather than one probe per wake: a
//! screenful of folders would otherwise be a screenful of repaints.
//!
//! # The Properties read
//!
//! A Properties window describes one node, which is one `fs_stat` plus one
//! call per extended-attribute key — thirty-odd round trips for a node
//! carrying a full set. [`PropertyReads`] is keyed by *window*, because
//! several Properties windows are open at once and one window's read must not
//! displace another's; a re-read of the same window supersedes its own
//! outstanding one, since only the latest answer describes the node now.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::Errno;
use tairix_browse::{EntryKind, ListingClient, Probe, Properties};

/// The file manager's one directory-listing consumer.
///
/// Named rather than counted, like every other program's: this app browses one
/// place at a time, so its desk has exactly one slot and the round-robin that
/// keeps two consumers fair degrades to serving this one every turn.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FilesClient {
    /// The browser's own current directory.
    Browser,
}

impl ListingClient for FilesClient {
    const ALL: &'static [Self] = &[Self::Browser];
}

/// What the folder cues have asked for and what has come back.
///
/// Deliberately free of locks, threads, and syscalls: the embedder supplies the
/// exclusion and the blocking, so every rule here is a host test.
#[derive(Debug, Default)]
pub struct Probes {
    /// Folders asked about and not yet probed, in the order asked. A batch is
    /// taken from here whole.
    wanted: Vec<Vec<String>>,
    /// Folders being probed right now, so a re-ask during the probe records no
    /// second one.
    probing: Vec<Vec<String>>,
    /// Answers waiting to be drawn, each served once — the renderer latches it
    /// onto the entry, so a later ask is a genuinely fresh question.
    answers: Vec<(Vec<String>, bool)>,
    /// Whether a batch has been delivered since the embedder last asked. The
    /// loop consumes it and resolves the cues, which is what turns a worker's
    /// answer into pixels: without it the answers sit here until some
    /// unrelated repaint happens to latch them.
    landed: bool,
    /// Set once the embedder is tearing down, so nothing further is recorded.
    stopping: bool,
}

impl Probes {
    /// A desk with nothing asked for and nothing answered.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            wanted: Vec::new(),
            probing: Vec::new(),
            answers: Vec::new(),
            landed: false,
            stopping: false,
        }
    }

    /// Answer the cue for `components`, recording the probe if this desk has
    /// neither run it nor been asked for it already.
    ///
    /// [`Probe::Ready`] hands the answer *over* — the renderer latches it onto
    /// the entry — so the slot goes with it. Everything else is
    /// [`Probe::Pending`], which leaves the folder drawn without its cue until
    /// the answer arrives.
    ///
    /// A stopping desk records nothing: no worker is left to answer it.
    ///
    /// The `bool` is whether this ask *recorded* a new probe, so the embedder
    /// wakes a worker for a folder it has not seen rather than once per paint.
    pub fn ask(&mut self, components: &[String]) -> (Probe, bool) {
        if let Some(index) = self
            .answers
            .iter()
            .position(|(path, _)| path.as_slice() == components)
        {
            let (_, occupied) = self.answers.remove(index);
            return (Probe::Ready(occupied), false);
        }
        if self.stopping {
            return (Probe::Pending, false);
        }
        let known = self
            .wanted
            .iter()
            .chain(self.probing.iter())
            .any(|path| path.as_slice() == components);
        if !known {
            self.wanted.push(components.to_vec());
        }
        (Probe::Pending, !known)
    }

    /// Whether any folder has been asked about and not yet probed.
    #[must_use]
    pub fn has_work(&self) -> bool {
        !self.stopping && !self.wanted.is_empty()
    }

    /// Take every outstanding probe as one batch, or `None` when there is
    /// nothing to do.
    ///
    /// A batch rather than one probe at a time because the answers are drawn
    /// together: a screenful of folders answered one wake at a time would be a
    /// screenful of repaints for one screenful of cues.
    pub fn next_batch(&mut self) -> Option<Vec<Vec<String>>> {
        if self.stopping || self.wanted.is_empty() {
            return None;
        }
        let batch = core::mem::take(&mut self.wanted);
        self.probing.clone_from(&batch);
        Some(batch)
    }

    /// Record a batch's answers, answering whether any is worth a repaint.
    ///
    /// The batch *replaces* whatever was held rather than adding to it, which
    /// is what bounds this desk to one screenful. A paint asks about the whole
    /// visible range and consumes every answer it wanted, so anything still
    /// held when the next batch lands is for a folder scrolled out of view —
    /// and keeping those would grow the set once per folder the user ever
    /// scrolled past, which on a directory of a hundred thousand entries is a
    /// capacity nothing bounds. A folder that scrolls back into view is simply
    /// asked again.
    pub fn deliver(&mut self, answers: Vec<(Vec<String>, bool)>) -> bool {
        self.probing.clear();
        if self.stopping || answers.is_empty() {
            self.answers.clear();
            return false;
        }
        self.answers = answers;
        self.landed = true;
        true
    }

    /// Whether a batch has been delivered since this was last asked, clearing
    /// the record.
    ///
    /// The embedder resolves the visible cues on a `true` and presents the
    /// windows whose icons moved, so the answer a worker produced is drawn on
    /// the next turn of the loop rather than whenever some other gesture
    /// happens to repaint.
    pub fn take_landed(&mut self) -> bool {
        core::mem::take(&mut self.landed)
    }

    /// Stop recording, so a parked worker leaves and no further cue is asked
    /// for.
    pub fn stop(&mut self) {
        self.stopping = true;
        self.wanted.clear();
        self.answers.clear();
        // Nothing is left to draw, so a delivery's unconsumed repaint dies
        // with the answers it would have shown.
        self.landed = false;
    }

    /// Whether the embedder has asked workers to leave.
    #[must_use]
    pub const fn stopping(&self) -> bool {
        self.stopping
    }
}

/// One Properties window's read: the node to describe and which window asked.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PropertyJob {
    /// The window the answer belongs to.
    pub window: u64,
    /// The node's absolute path, by the one spelling every read and write of
    /// it uses.
    pub path: String,
    /// What the listing called it, which decides how the node is opened to be
    /// described (a link is described as itself, not as its target).
    pub kind: EntryKind,
}

/// What the Properties windows have asked to be read and what has come back.
///
/// Like [`Probes`], free of locks, threads, and syscalls: the embedder
/// supplies the exclusion and the blocking, so every rule here is a host test.
#[derive(Debug, Default)]
pub struct PropertyReads {
    /// Reads asked for and not yet started, in the order asked.
    wanted: Vec<PropertyJob>,
    /// Windows whose read is running, so a window closed mid-read is not
    /// answered into a slot nobody owns.
    reading: Vec<u64>,
    /// Answers waiting to be collected, one per window.
    answers: Vec<(u64, Result<Properties, Errno>)>,
    /// Whether an answer has landed since the embedder last asked.
    landed: bool,
    /// Set once the embedder is tearing down.
    stopping: bool,
}

impl PropertyReads {
    /// A desk with nothing asked for and nothing answered.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            wanted: Vec::new(),
            reading: Vec::new(),
            answers: Vec::new(),
            landed: false,
            stopping: false,
        }
    }

    /// Ask for `job`, answering whether a worker should be woken.
    ///
    /// A window's outstanding request is *replaced*: a re-read follows a write
    /// the window just made, so the older answer describes a node that has
    /// since changed and showing it would undo what the user did. Any answer
    /// the window had not collected goes with it, for the same reason.
    pub fn submit(&mut self, job: PropertyJob) -> bool {
        if self.stopping {
            return false;
        }
        let window = job.window;
        self.wanted.retain(|held| held.window != window);
        self.answers.retain(|(id, _)| *id != window);
        self.wanted.push(job);
        true
    }

    /// Whether any read is waiting for a worker to take it.
    #[must_use]
    pub fn has_work(&self) -> bool {
        !self.stopping && !self.wanted.is_empty()
    }

    /// Take the next read to run, or `None` when there is nothing to do.
    pub fn next_job(&mut self) -> Option<PropertyJob> {
        if self.stopping || self.wanted.is_empty() {
            return None;
        }
        let job = self.wanted.remove(0);
        self.reading.push(job.window);
        Some(job)
    }

    /// Record what reading `window`'s node produced, answering whether the
    /// embedder's loop is owed a wake.
    ///
    /// An answer for a window that is no longer reading — it was closed, or
    /// asked again — is dropped: nothing owns the slot, and holding it would
    /// leave a stale summary for the next window to be given that id.
    pub fn deliver(&mut self, window: u64, answer: Result<Properties, Errno>) -> bool {
        let Some(index) = self.reading.iter().position(|id| *id == window) else {
            return false;
        };
        self.reading.remove(index);
        if self.stopping {
            return false;
        }
        self.answers.retain(|(id, _)| *id != window);
        self.answers.push((window, answer));
        self.landed = true;
        true
    }

    /// Take `window`'s answer, if one has landed.
    pub fn take(&mut self, window: u64) -> Option<Result<Properties, Errno>> {
        let index = self.answers.iter().position(|(id, _)| *id == window)?;
        Some(self.answers.remove(index).1)
    }

    /// Whether an answer has landed since this was last asked, clearing the
    /// record.
    pub fn take_landed(&mut self) -> bool {
        core::mem::take(&mut self.landed)
    }

    /// Forget everything `window` asked for, because it has closed.
    ///
    /// A read already running is left to finish and its answer dropped on
    /// delivery: a worker mid-`fs_stat` cannot be recalled, and the alternative
    /// is a window id that could be reused while an answer for it is still in
    /// flight.
    pub fn forget(&mut self, window: u64) {
        self.wanted.retain(|job| job.window != window);
        self.answers.retain(|(id, _)| *id != window);
        self.reading.retain(|id| *id != window);
    }

    /// Stop handing out work, so a parked worker leaves.
    pub fn stop(&mut self) {
        self.stopping = true;
        self.wanted.clear();
        self.answers.clear();
        self.landed = false;
    }
}

#[cfg(test)]
#[path = "deferred_tests.rs"]
mod tests;
