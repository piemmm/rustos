//! The deferred-decode desk: what a draw site has asked for, what a producer
//! is running, and what has come back (`plans/FIX-DESKTOP.md` DESK-8).
//!
//! Resolving one icon costs a bounded read plus a round trip to the parser
//! sandbox that decodes it. Performed inside a paint, that stalls whatever the
//! paint is on — the desktop's compositor and seat drain, or a file manager's
//! own input — for as long as the disk and the worker take, once per icon. So
//! the decode is *recorded* here, the draw takes the built-in glyph for the
//! frame it is not ready in, and the pixels are collected when they land.
//!
//! [`ArtworkDesk`] is the whole policy and holds no lock, thread, or syscall,
//! so every rule below is a host test rather than an argument. Two embedders
//! drive it differently over the same rules: the desktop session parks a
//! worker thread on it behind the runtime's futex mutex, and the file manager
//! pumps one job per turn of its own event loop.
//!
//! # Rounds
//!
//! The decode cache is budgeted, so it can be asked to hold more than it will.
//! Without a rule, a decode the cache declined to retain would be asked for
//! again by the very repaint its landing drove, decoded again, and declined
//! again — for ever. A **round** forecloses that: a key answered once is not
//! decoded again until the embedder next acts. The embedder opens a fresh one
//! on any wake that is not its own landing repaint, which is exactly when what
//! is on screen can have changed.
//!
//! Work in flight and answers not yet collected survive a round boundary, so
//! no decode is ever run twice for want of somewhere to keep it.

use alloc::collections::{BTreeMap, VecDeque};

use tairix_raster::Surface;
use tairix_reclaim::CachedBytes;

use crate::artwork::{ArtworkKey, ArtworkResolver, Resolved};

/// One decode: what to resolve, and the pixel side to resolve it at.
///
/// The pair is the cache's own key, so a producer yields exactly the slot the
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
    /// Recorded, and no producer has taken it.
    Wanted,
    /// A producer is running it.
    Running,
    /// Produced and waiting to be collected. `None` is a refusal — an absent,
    /// over-long, or undecodable asset — which the cache retains just as it
    /// retains artwork.
    Done(Option<Surface>),
    /// Collected already in this round.
    Answered,
    /// Collected, and the cache could not keep it: no room the current
    /// pressure band allows. Kept across rounds, unlike [`State::Answered`],
    /// because decoding it again would only be refused again.
    Declined,
}

/// What has been asked for, what is being produced, what has come back, and
/// what has already been answered this round.
///
/// The embedder supplies the exclusion and the blocking; nothing here waits.
pub struct ArtworkDesk {
    /// Every job this round knows about, indexed for an O(log n) collect —
    /// a paint asks once per icon it draws, so the lookup is on the frame path.
    slots: BTreeMap<ArtworkJob, State>,
    /// The order [`State::Wanted`] jobs are handed out in: first asked, first
    /// decoded, so a busy surface cannot indefinitely displace a quiet one's
    /// single icon.
    queue: VecDeque<ArtworkJob>,
    /// Whether anything has been delivered since the embedder last asked.
    landed: bool,
    /// Set once the embedder is tearing down, so a parked producer leaves
    /// instead of looking for work and no further decode is recorded.
    stopping: bool,
}

impl ArtworkDesk {
    /// A desk with nothing asked for and nothing answered.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            slots: BTreeMap::new(),
            queue: VecDeque::new(),
            landed: false,
            stopping: false,
        }
    }

    /// Answer a draw site's miss on `key` at `side`, recording the decode if
    /// this desk has neither run it nor been asked for it already.
    ///
    /// [`Resolved::Done`] once per round — the answer is *moved out*, because
    /// the caller is the cache that will retain it. Every other state is
    /// [`Resolved::Pending`]: the draw takes the tier below it, which for the
    /// last tier is the built-in glyph.
    ///
    /// A desk that is stopping records nothing: there is no producer left to
    /// answer it.
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
            Some(State::Wanted | State::Running | State::Answered | State::Declined) => {
                Resolved::Pending
            }
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
    /// what it is *about* to draw asks now, so the decode finishes before the
    /// frame that needs it rather than a round trip per icon after it.
    ///
    /// A key this desk already knows is left exactly as it is, so a prefetch
    /// can never consume an answer a draw is about to collect, nor re-queue
    /// one this round has already given out.
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

    /// Whether any recorded decode is waiting for a producer to take it.
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
        // for the producer's bookkeeping, and taking the next entry rather
        // than giving up means a job that somehow lost its slot costs one
        // decode not started, never a producer that stops taking work.
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
    /// an earlier decode. The caller uses that to decide whether a wake is
    /// owed at all.
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
    /// The embedder repaints on a `true`, so the surfaces that drew a glyph for
    /// want of pixels draw the pixels — and a wake that delivered nothing costs
    /// no frame.
    pub fn take_landed(&mut self) -> bool {
        core::mem::take(&mut self.landed)
    }

    /// Note that the cache could not keep what `job` produced, so this desk
    /// stops offering it until the band that refused it moves.
    ///
    /// Without this the refusal is silent and self-renewing: the round the
    /// landing triggered asks again, the answer is refused again, and every
    /// icon on screen is read and decoded on every repaint — precisely when
    /// the machine is short of the memory that would have held the answer.
    pub fn decline(&mut self, key: &ArtworkKey, side: u32) {
        let job = ArtworkJob {
            key: key.clone(),
            side,
        };
        if let Some(state) = self.slots.get_mut(&job) {
            *state = State::Declined;
        }
    }

    /// Offer every declined key again, because the pressure band moved and the
    /// answer may now be retainable.
    pub fn retry_declined(&mut self) {
        self.slots
            .retain(|_, state| !matches!(state, State::Declined));
    }

    /// Open a fresh round: every key answered in the last one may be asked for
    /// again.
    ///
    /// Work in flight and answers not yet collected are kept, so a round
    /// boundary never discards a decode or causes one to be run twice.
    /// Declined keys are kept too: a round boundary is not what makes a
    /// refused answer retainable.
    pub fn begin_round(&mut self) {
        self.slots
            .retain(|_, state| !matches!(state, State::Answered));
    }

    /// Stop handing out work, so a parked producer leaves its loop.
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

    /// Whether the embedder has asked producers to leave.
    #[must_use]
    pub const fn stopping(&self) -> bool {
        self.stopping
    }
}

impl Default for ArtworkDesk {
    fn default() -> Self {
        Self::new()
    }
}

/// The desk *is* the deferring resolver: it answers what a producer has
/// already delivered and records everything else.
///
/// An embedder that owns the desk outright — a single-threaded event loop that
/// pumps one job per turn — hands the cache a plain `&mut` to it and needs no
/// wrapper. One that shares the desk with a worker thread implements the trait
/// over its own mutex instead, so the notify happens inside the same critical
/// section as the state change.
impl ArtworkResolver for ArtworkDesk {
    fn resolve(&mut self, key: &ArtworkKey, side: u32) -> Resolved {
        self.collect(key, side)
    }

    fn prefetch(&mut self, key: &ArtworkKey, side: u32) {
        self.want(key, side);
    }

    fn declined(&mut self, key: &ArtworkKey, side: u32) {
        self.decline(key, side);
    }
}

#[cfg(test)]
#[path = "desk_tests.rs"]
mod tests;
