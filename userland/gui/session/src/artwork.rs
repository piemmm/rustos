//! The desktop's icon artwork, decoded off the session's event loop
//! (`plans/FIX-DESKTOP.md` DESK-8).
//!
//! Resolving one icon costs a bounded VFS read plus a round trip to the parser
//! sandbox that decodes it. Run on the session's own task — which is where the
//! taskbar, the launcher popup, and the desktop's icon column all draw from —
//! it stalls the compositor, the seat drain, and every application blocked in a
//! window call for as long as the disk and the worker take. A launcher opening
//! on thirty applications pays that thirty times before its first pixel. So the
//! decode runs on a worker thread instead, the draw takes the built-in glyph
//! for the frame it is not ready in, and the session learns the pixels landed
//! through its existing wait-set.
//!
//! [`ArtworkDesk`] is that arrangement's whole policy, and it holds no lock, no
//! thread, and no syscall: what has been asked for, what a worker is producing,
//! what has come back, and the rule that stops a landing chasing its own tail.
//! The `Run` binary wraps it in the runtime's futex mutex, parks a worker on a
//! condition variable over it, and writes one byte to a pipe the wait-set
//! watches.
//!
//! # Asking early
//!
//! A decode costs around a tenth of a second, so a surface that first asks for
//! its icons *as it paints* shows built-in glyphs until they arrive — a
//! launcher opening on twenty applications fills in a round trip per icon
//! after the user is already looking at it. [`ArtworkDesk::want`] is the answer:
//! the desktop knows the whole set the moment it has the catalog naming it,
//! which is long before the surface drawing them is shown, so the wait happens
//! then instead of in front of the user.
//!
//! # Rounds, and why they exist
//!
//! The decode cache is budgeted, so it can be asked to hold more than it will.
//! Without a rule, a decode the cache declines to retain would be asked for by
//! the repaint the landing triggered, decoded again, declined again, and the
//! desktop would repaint itself for ever over a cache it cannot fill.
//!
//! A **round** forecloses that: a key answered once is not decoded again until
//! the desktop next acts. Everything the paints of one round ask for is decoded
//! at most once, whatever the cache then does with it, so the work a landing
//! can create is finite and the loop always runs dry. The session opens a fresh
//! round on any wake that is not a worker's nudge — real input, a window call,
//! a re-list — which is exactly when the visible icons can have changed.
//!
//! Work in flight and work already done both survive a round boundary: the
//! round governs *re-asking*, never the decode itself, so no answer is ever
//! computed twice for want of somewhere to keep it.
//!
//! # Nothing here waits
//!
//! A decode is *recorded*; the answer is *collected*. Both are plain state
//! transitions. The party that blocks is the worker, on its condition variable,
//! and the party that parks is the session, on its wait-set — never this.

use alloc::collections::{BTreeMap, VecDeque};

use tairix_icon::{ArtworkKey, Resolved};
use tairix_raster::Surface;
use tairix_reclaim::CachedBytes;

/// One decode: what to resolve, and the pixel side to resolve it at.
///
/// The pair is the cache's own key, so a worker produces exactly the slot the
/// draw site missed on — a scale change asks for a different side and is a
/// different job, never a resized copy of this one.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ArtworkJob {
    /// The asset or bundle to resolve.
    pub key: ArtworkKey,
    /// The pixel side to rasterise it at.
    pub side: u32,
}

/// Where one job has got to.
enum State {
    /// Recorded, and no worker has taken it.
    Wanted,
    /// A worker is producing it.
    Running,
    /// Produced and waiting to be collected. `None` is a refusal — an absent,
    /// over-long, or undecodable asset — which the cache retains just as it
    /// retains artwork.
    Done(Option<Surface>),
    /// Collected already in this round.
    Answered,
}

/// The artwork arrangement's policy: what has been asked for, what is being
/// produced, what has come back, and what has already been answered.
///
/// Deliberately free of locks, threads, and syscalls, so every rule below is a
/// host test rather than an argument. The embedder supplies the exclusion and
/// the blocking.
#[derive(Default)]
pub struct ArtworkDesk {
    /// Every job this round knows about, indexed for an O(log n) collect —
    /// a paint asks once per icon it draws, so the lookup is on the frame path.
    slots: BTreeMap<ArtworkJob, State>,
    /// The order [`State::Wanted`] jobs are handed out in.
    ///
    /// The fairness discipline: first asked, first decoded, so the icons of the
    /// surface that asked first are the ones that appear first and a busy
    /// surface cannot indefinitely displace a quiet one's single icon.
    queue: VecDeque<ArtworkJob>,
    /// Whether anything has been delivered since the embedder last asked.
    landed: bool,
    /// Set once the embedder is tearing down, so a parked worker leaves
    /// instead of looking for work and no further decode is recorded.
    stopping: bool,
}

impl ArtworkDesk {
    /// A desk with nothing asked for and nothing answered.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Answer a draw site's miss on `key` at `side`, recording the decode if
    /// this desk has neither run it nor been asked for it already.
    ///
    /// [`Resolved::Done`] once per round — the answer is *moved out*, because
    /// the caller is the cache that will retain it. Every other state is
    /// [`Resolved::Pending`]: the draw takes the tier below it, which for the
    /// last tier is the built-in glyph.
    ///
    /// A desk that is stopping records nothing. There is no worker left to
    /// answer it, and a session on its way out draws glyphs rather than
    /// waiting on pixels nobody will produce.
    pub fn collect(&mut self, key: &ArtworkKey, side: u32) -> Resolved {
        let job = ArtworkJob {
            key: key.clone(),
            side,
        };
        match self.slots.get_mut(&job) {
            Some(state @ State::Done(_)) => {
                let State::Done(artwork) = core::mem::replace(state, State::Answered) else {
                    return Resolved::Pending;
                };
                Resolved::Done(artwork)
            }
            Some(State::Wanted | State::Running | State::Answered) => Resolved::Pending,
            None => {
                if !self.stopping {
                    self.slots.insert(job.clone(), State::Wanted);
                    self.queue.push_back(job);
                }
                Resolved::Pending
            }
        }
    }

    /// Record `key` at `side` as wanted, without collecting anything.
    ///
    /// The prefetch half of [`collect`](Self::collect): a surface that knows
    /// what it is *about* to draw asks for it now, so the decode is finished
    /// before the frame that needs it. Without it a launcher opening on twenty
    /// applications paints twenty built-in glyphs and replaces them a round trip
    /// per icon later, which is the whole visible cost of moving the decode off
    /// this task.
    ///
    /// A key this desk already knows — wanted, running, done, or answered this
    /// round — is left exactly as it is, so a prefetch can never consume an
    /// answer a draw is about to collect, nor re-queue one this round has
    /// already given out.
    pub fn want(&mut self, key: &ArtworkKey, side: u32) {
        if self.stopping {
            return;
        }
        let job = ArtworkJob {
            key: key.clone(),
            side,
        };
        if self.slots.contains_key(&job) {
            return;
        }
        self.slots.insert(job.clone(), State::Wanted);
        self.queue.push_back(job);
    }

    /// Whether any recorded decode is waiting for a worker to take it.
    #[must_use]
    pub fn has_work(&self) -> bool {
        !self.stopping && !self.queue.is_empty()
    }

    /// Take the next decode to run, or `None` when there is nothing to do.
    pub fn next_job(&mut self) -> Option<ArtworkJob> {
        if self.stopping {
            return None;
        }
        // The queue is only the hand-out *order*; the slots are the authority
        // on whether a job is still wanted. Deciding that here rather than
        // scanning the queue whenever a slot changes keeps a paint from paying
        // for the worker's bookkeeping, and taking the next entry rather than
        // giving up means a job that somehow lost its slot costs one decode not
        // started, never a worker that stops taking work.
        while let Some(job) = self.queue.pop_front() {
            if let Some(state @ State::Wanted) = self.slots.get_mut(&job) {
                *state = State::Running;
                return Some(job);
            }
        }
        None
    }

    /// Record what decoding `job` produced.
    ///
    /// Answers `false` — and keeps nothing — when the desk is no longer
    /// holding that job as in flight: it stopped, or the key was answered from
    /// an earlier decode. The caller uses that to decide whether a wake is owed
    /// at all.
    pub fn deliver(&mut self, job: &ArtworkJob, artwork: Option<Surface>) -> bool {
        let Some(state) = self.slots.get_mut(job) else {
            return false;
        };
        if !matches!(state, State::Running) {
            return false;
        }
        *state = State::Done(artwork);
        self.landed = true;
        true
    }

    /// Whether anything has been delivered since this was last asked, clearing
    /// the record.
    ///
    /// The session repaints on a `true`, so the surfaces that drew a glyph for
    /// want of pixels draw the pixels — and a wake that delivered nothing costs
    /// no frame.
    pub fn take_landed(&mut self) -> bool {
        core::mem::take(&mut self.landed)
    }

    /// Open a fresh round: every key answered in the last one may be asked for
    /// again.
    ///
    /// Work in flight and answers not yet collected are kept, so a round
    /// boundary never discards a decode or causes one to be run twice.
    pub fn begin_round(&mut self) {
        self.slots
            .retain(|_, state| !matches!(state, State::Answered));
    }

    /// Stop handing out work, so a parked worker leaves its loop.
    ///
    /// Every decode still held is overwritten before it is dropped, on the same
    /// terms the artwork cache wipes its own: one user's rendered pixels do not
    /// outlive their session in reusable heap.
    pub fn stop(&mut self) {
        self.stopping = true;
        for state in self.slots.values_mut() {
            if let State::Done(Some(artwork)) = state {
                artwork.wipe();
            }
        }
        self.slots.clear();
        self.queue.clear();
    }

    /// Whether the embedder has asked workers to leave.
    #[must_use]
    pub const fn stopping(&self) -> bool {
        self.stopping
    }
}

#[cfg(test)]
#[path = "artwork_tests.rs"]
mod tests;
