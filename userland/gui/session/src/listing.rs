//! The desktop's directory listings, read off the session's event loop
//! (`plans/FIX-DESKTOP.md` DESK-4).
//!
//! Listing a directory is a `fs_open` + `fs_readdir` on whatever disk holds it.
//! Run on the session's own task it stalls the compositor, the seat drain, and
//! every application blocked in a window call for as long as that disk takes —
//! which on a slow or contended device is not a frame but a visible freeze. So
//! it runs on a worker thread instead, and the session learns the answer has
//! landed through its existing wait-set.
//!
//! [`ListingDesk`] is that arrangement's whole policy, and it holds no lock, no
//! thread, and no syscall: one request slot and one answer slot per consumer,
//! the staleness rule that drops an answer nobody wants any more, and the
//! round-robin that stops one busy consumer starving the other. The `Run`
//! binary wraps it in the runtime's futex mutex, parks a worker on a condition
//! variable over it, and writes one byte to a pipe the wait-set watches.
//!
//! # Two consumers, named rather than counted
//!
//! Exactly two things in the session list directories: the desktop's own icon
//! column and the trusted file picker. That is a structural fact about the
//! session — not a capacity a bigger machine outgrows — so they are an
//! enumeration with a slot each, and neither can lose its place to the other.
//!
//! # Nothing here waits
//!
//! A request is *recorded*; the answer is *collected*. Both are plain state
//! transitions. The party that blocks is the worker, on its condition variable,
//! and the party that parks is the session, on its wait-set — never this.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::Errno;
use tairix_browse::{Entry, Listing};

/// Which of the session's two directory-listing consumers a request belongs to.
///
/// Named, not counted: the desktop has exactly these two, and giving each its
/// own slot is what keeps a picker navigating fast from ever displacing the icon
/// column's pending re-list.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ListingClient {
    /// The desktop's own icon column — the user's `Desktop` folder.
    Pinboard,
    /// The trusted file picker the window channel opens on an app's behalf.
    Picker,
}

impl ListingClient {
    /// Every consumer, in slot order.
    const ALL: [Self; 2] = [Self::Pinboard, Self::Picker];

    /// This consumer's slot index.
    const fn slot(self) -> usize {
        match self {
            Self::Pinboard => 0,
            Self::Picker => 1,
        }
    }
}

/// What one consumer has asked for and what it has been answered.
#[derive(Default)]
struct Slot {
    /// The directory this consumer is waiting on, cleared when its answer is
    /// stored. Recording a different one abandons the old request.
    wanted: Option<Vec<String>>,
    /// Whether a reader has taken [`Slot::wanted`] and not yet answered it, so
    /// the same request is never handed out twice.
    reading: bool,
    /// The answer, kept until the consumer asks for that same directory.
    done: Option<Answer>,
}

/// One completed read: which directory, and what the read produced.
struct Answer {
    target: Vec<String>,
    result: Result<Vec<Entry>, Errno>,
}

/// The listing arrangement's policy: who has asked for what, what has come
/// back, and which consumer a reader serves next.
///
/// Deliberately free of locks, threads, and syscalls, so every rule below is a
/// host test rather than an argument. The embedder supplies the exclusion and
/// the blocking.
#[derive(Default)]
pub struct ListingDesk {
    slots: [Slot; 2],
    /// The consumer to consider first, rotated after each job is handed out.
    ///
    /// The fairness discipline: round-robin over the consumers, so a picker
    /// walking a deep tree cannot hold the icon column's re-list behind it
    /// indefinitely.
    next: usize,
    /// Set once the embedder is tearing down, so a parked reader leaves instead
    /// of looking for work.
    stopping: bool,
}

impl ListingDesk {
    /// A desk with nothing asked for and nothing answered.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Answer `client`'s request for `components`, recording the request if
    /// this desk does not already hold the answer.
    ///
    /// [`Listing::Ready`] once — the answer is *consumed*, because the consumer
    /// has adopted it and a later ask means a genuinely fresh read. An answer
    /// for a different directory is dropped as stale rather than served.
    ///
    /// # Errors
    ///
    /// Whatever the read reported, once, on the same consume-it rule.
    pub fn take(&mut self, client: ListingClient, components: &[String]) -> Result<Listing, Errno> {
        let slot = &mut self.slots[client.slot()];
        match slot.done.take() {
            Some(answer) if answer.target == components => {
                slot.wanted = None;
                return answer.result.map(Listing::Ready);
            }
            // For somewhere the consumer has since navigated away from: gone.
            Some(_) | None => {}
        }
        if slot.wanted.as_deref() != Some(components) {
            slot.wanted = Some(components.to_vec());
        }
        Ok(Listing::Pending)
    }

    /// Whether any consumer has an unanswered request no reader has taken.
    #[must_use]
    pub fn has_work(&self) -> bool {
        !self.stopping
            && self
                .slots
                .iter()
                .any(|slot| slot.wanted.is_some() && !slot.reading)
    }

    /// Take the next directory to read, or `None` when there is nothing to do.
    ///
    /// Round-robin from wherever the last hand-out left the cursor, so two
    /// consumers asking continuously each get every other read.
    pub fn next_job(&mut self) -> Option<(ListingClient, Vec<String>)> {
        if self.stopping {
            return None;
        }
        for step in 0..ListingClient::ALL.len() {
            let index = (self.next + step) % ListingClient::ALL.len();
            let client = ListingClient::ALL[index];
            let slot = &mut self.slots[client.slot()];
            if slot.reading {
                continue;
            }
            if let Some(target) = slot.wanted.clone() {
                slot.reading = true;
                self.next = (index + 1) % ListingClient::ALL.len();
                return Some((client, target));
            }
        }
        None
    }

    /// Record the result of reading `target` for `client`.
    ///
    /// Answers `false` — and keeps nothing — when the consumer has since asked
    /// for somewhere else: the read was for a directory nobody is looking at,
    /// so serving it would put stale entries on screen. The caller uses that to
    /// decide whether a wake is owed at all.
    pub fn deliver(
        &mut self,
        client: ListingClient,
        target: Vec<String>,
        result: Result<Vec<Entry>, Errno>,
    ) -> bool {
        let slot = &mut self.slots[client.slot()];
        slot.reading = false;
        if slot.wanted.as_deref() != Some(target.as_slice()) {
            return false;
        }
        slot.done = Some(Answer { target, result });
        true
    }

    /// Stop handing out work, so a parked reader leaves its loop.
    pub fn stop(&mut self) {
        self.stopping = true;
    }

    /// Whether the embedder has asked readers to leave.
    #[must_use]
    pub const fn stopping(&self) -> bool {
        self.stopping
    }
}

#[cfg(test)]
#[path = "listing_tests.rs"]
mod tests;
