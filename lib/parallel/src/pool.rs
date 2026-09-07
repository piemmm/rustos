//! The fork-join worker pool: [`Pool`].
//!
//! # The protocol
//!
//! A dispatch publishes the work, opens its claim on `count` pieces, bumps
//! `epoch` and wakes the workers parked on it; every participant — the workers
//! *and* the dispatching thread — draws pieces off the claim until they run
//! out. A worker's draw and its hold on the dispatch are the **same** atomic:
//! it becomes a holder by taking a piece, and stops being one when it runs out
//! of them. The dispatcher returns once the pieces are exhausted and no holder
//! is left.
//!
//! # The lifetime argument
//!
//! The published work is a reference to a value on the **dispatcher's stack**,
//! so the dispatcher must not return while any worker could still read it. The
//! claim word makes that decidable, because the holder count and the pieces
//! left sit in one word: a worker reads the published pointer only after a draw
//! that took a piece *and* incremented the holders in a single
//! compare-exchange, so the dispatcher can never observe "no pieces left and no
//! holders" while a worker is still reading. Waiting for the holders to reach
//! zero after the pieces run out is therefore exactly the condition "no worker
//! holds the pointer".
//!
//! Two words cannot express that. Deciding "is there a piece for me" separately
//! from "I am now reading this dispatch" leaves a window in between, so a
//! worker had to register its hold *first* and discover only afterwards whether
//! any work was left.
//!
//! # Why the barrier is over pieces in flight, not over workers
//!
//! A hold taken before the work is known makes a dispatch's latency the time
//! for the scheduler to run a woken worker to completion, whether or not that
//! worker got any work. A worker is woken at the start of every dispatch, so it
//! does reach a CPU, take its hold and then risk preemption — and the
//! dispatcher waits for it either way. On a machine with more runnable threads
//! than cores that is a run-queue wait, not a work wait: it was measured first
//! at 429 ms and then, once the barrier had been narrowed to the workers that
//! registered, at 992 ms on a four-core board — a frame's worth of compositing
//! spent waiting for help it had not been given.
//!
//! Taking the hold *with* the piece removes the case outright. A worker that
//! finds the pieces exhausted touches nothing, holds nothing and parks again,
//! so the dispatcher never waits for it however long it is descheduled. What
//! remains waited for is a piece genuinely in flight, whose result the dispatch
//! needs before it can return.
//!
//! No parallelism is given up: a worker is refused only when there is no piece
//! left to give it.
//!
//! # Pieces are handed out from the top down
//!
//! The index a draw yields is `remaining - 1`, so pieces run in descending
//! order. The count has to live in the same word as the hold, and a *second*
//! atomic holding it would let a worker that read one dispatch's count draw
//! against a later dispatch's word — an out-of-range index, which is unsound
//! rather than merely wrong. [`JobRunner`] contracts that the order pieces run
//! in is not observable, and the `Reversed` test runner exists to hold
//! consumers to it.
//!
//! # Nothing spins
//!
//! A worker with no dispatch to run parks in `futex_wait` on `epoch`; a
//! dispatcher with pieces still in flight parks in `futex_wait` on the claim
//! word. The only cost of an idle pool is the address space its workers' stacks
//! reserve.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

use tairix_rt::sync::Mutex;
use tairix_rt::thread::{JoinHandle, Thread};

use crate::JobRunner;

/// `futex_wait`'s "no timeout" spelling.
const NO_TIMEOUT: u64 = u64::MAX;

/// The work one dispatch is running, borrowed from the dispatching thread's own
/// frame for exactly as long as that dispatch lasts.
///
/// A struct rather than the bare reference because a `&dyn Fn` is a fat
/// pointer and what is published is a thin one: this is the value on the
/// dispatcher's frame that the thin pointer names.
struct Dispatch<'a> {
    /// The piece body, indexed by piece number.
    job: &'a (dyn Fn(usize) + Sync),
}

/// A [`Dispatch`] with its lifetime erased, which is the form it is published in:
/// a shared pointer cannot carry the dispatcher's frame lifetime, so the
/// protocol carries it instead.
type Published = Dispatch<'static>;

/// Bits of a [`Claim`] word given to the pieces still to be handed out; the
/// rest count the workers holding the dispatch.
const REMAINING_BITS: u32 = 16;

/// Mask of the pieces-left half of a [`Claim`] word.
const REMAINING_MASK: u32 = (1 << REMAINING_BITS) - 1;

/// Most pieces one dispatch may be split into. A wider `count` is run on the
/// calling thread instead ([`Pool::run`]); [`crate::bands`] answers at most
/// four per participant, so no real caller approaches it.
const PIECES_MAX: u32 = REMAINING_MASK;

/// Most workers one pool may hold, so the holder half of the word cannot carry
/// into the pieces half. [`Pool::with_workers`] creates no more than this and
/// reports what it got, exactly as it does for a thread the kernel refuses.
const HOLDERS_MAX: u32 = REMAINING_MASK;

/// Pieces of the current dispatch still to be handed out.
const fn remaining(word: u32) -> u32 {
    word & REMAINING_MASK
}

/// Workers still holding the dispatch a claim word describes.
const fn holders(word: u32) -> u32 {
    word >> REMAINING_BITS
}

/// A claim word from its two halves.
const fn claim_word(holders: u32, remaining: u32) -> u32 {
    (holders << REMAINING_BITS) | remaining
}

/// What a holder's [`Claim::next`] found.
enum Next {
    /// Another piece, still under the same hold.
    Piece(usize),
    /// No pieces left; the hold is released, and `last` says whether this was
    /// the one the dispatcher is waiting for.
    Released { last: bool },
}

/// The pieces of one dispatch still to be handed out, and the workers still
/// reading it.
///
/// One word, because the two questions must be answered together: a worker
/// becomes a holder *by* taking a piece, so there is no state in which it may
/// read a dispatch the dispatcher believes it has finished with, and none in
/// which the dispatcher waits for a worker that got no work.
///
/// A pool between dispatches has no pieces left, which is the same state as a
/// drained one — so a worker that wakes spuriously, or that starts late and
/// mistakes the epoch it found for a fresh dispatch, is refused rather than
/// reading a stale pointer. There is no separate "closed" flag to keep in step.
struct Claim(AtomicU32);

impl Claim {
    const fn new() -> Self {
        Self(AtomicU32::new(0))
    }

    /// Offer `count` pieces of a fresh dispatch, with nobody holding it yet.
    ///
    /// The release that publishes the dispatch: a participant whose draw
    /// succeeds observes the work pointer stored before this.
    fn open(&self, count: u32) {
        self.0.store(count, Ordering::Release);
    }

    /// Draw a piece for a thread that does not hold the dispatch yet, taking a
    /// hold along with it.
    ///
    /// `None` leaves the word untouched and takes no hold, so a worker that
    /// arrives with the pieces exhausted has read nothing and delays nothing.
    /// Refused at [`HOLDERS_MAX`] for the same reason: a holder count that
    /// carried into the pieces half would fabricate work.
    fn take(&self) -> Option<usize> {
        let mut word = self.0.load(Ordering::Acquire);
        loop {
            let (held, left) = (holders(word), remaining(word));
            if left == 0 || held == HOLDERS_MAX {
                return None;
            }
            let next = claim_word(held + 1, left - 1);
            match self
                .0
                .compare_exchange_weak(word, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Some(left as usize - 1),
                Err(seen) => word = seen,
            }
        }
    }

    /// Draw the next piece for a thread that already holds the dispatch,
    /// releasing the hold when there is none.
    fn next(&self) -> Next {
        let mut word = self.0.load(Ordering::Acquire);
        loop {
            let (held, left) = (holders(word), remaining(word));
            let (next, outcome) = if left == 0 {
                // Saturating because only a holder calls this, so the count is
                // at least one: a decrement that could not underflow into the
                // pieces half is the fail-closed spelling of that invariant.
                (
                    claim_word(held.saturating_sub(1), 0),
                    Next::Released { last: held == 1 },
                )
            } else {
                (claim_word(held, left - 1), Next::Piece(left as usize - 1))
            };
            // Sequentially consistent on the release so it orders against the
            // dispatcher's announce-then-recheck in `await_holders`.
            match self
                .0
                .compare_exchange_weak(word, next, Ordering::SeqCst, Ordering::Acquire)
            {
                Ok(_) => return outcome,
                Err(seen) => word = seen,
            }
        }
    }

    /// Draw a piece without taking a hold, for the dispatching thread — which
    /// cannot outlive itself and so needs none.
    fn advance(&self) -> Option<usize> {
        let mut word = self.0.load(Ordering::Acquire);
        loop {
            let left = remaining(word);
            if left == 0 {
                return None;
            }
            let next = claim_word(holders(word), left - 1);
            match self
                .0
                .compare_exchange_weak(word, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Some(left as usize - 1),
                Err(seen) => word = seen,
            }
        }
    }

    /// The word as it stands, which is what a park compares against.
    fn load(&self) -> u32 {
        self.0.load(Ordering::SeqCst)
    }

    /// The word's address, for the futex calls.
    fn word(&self) -> &AtomicU32 {
        &self.0
    }
}

/// The state every participant of a pool reads.
struct Shared {
    /// The current dispatch, or null between dispatches. Published before
    /// [`Shared::epoch`] is bumped and cleared once no holder is left.
    dispatch: AtomicPtr<Published>,
    /// Bumped once per dispatch. A parked worker waits on this word, so the bump
    /// plus a wake is what starts a dispatch.
    epoch: AtomicU32,
    /// The pieces of the current dispatch left to hand out, and who is still
    /// reading it.
    claim: Claim,
    /// Non-zero while the dispatching thread is parked on the claim word, so
    /// the last holder pays for a wake syscall only when there is someone to
    /// wake.
    waiting: AtomicU32,
    /// Non-zero once the pool is being dropped, so a woken worker leaves its loop
    /// instead of looking for work.
    stop: AtomicU32,
}

impl Shared {
    const fn new() -> Self {
        Self {
            dispatch: AtomicPtr::new(core::ptr::null_mut()),
            epoch: AtomicU32::new(0),
            claim: Claim::new(),
            waiting: AtomicU32::new(0),
            stop: AtomicU32::new(0),
        }
    }

    /// Take a piece of the live dispatch and run it, and every further piece
    /// nobody else got to, or return having touched nothing because the pieces
    /// were already exhausted.
    ///
    /// The hold comes with the first piece, so a worker that finds none has
    /// read no pointer and the dispatcher is not waiting for it.
    fn take_and_drain(&self) {
        let Some(first) = self.claim.take() else {
            return;
        };
        let published = self.dispatch.load(Ordering::Acquire);
        // SAFETY: this draw took a piece and a hold in one compare-exchange, so
        // the dispatcher cannot have passed its `await_holders` and will not
        // return until the hold below is released. The pointer is non-null
        // because opening the claim is a release over the publication, and this
        // draw acquired it. The value outlives every read here and the
        // reinstated lifetime is no wider: the borrow ends with this function.
        let dispatch = unsafe { &*published };
        let mut index = first;
        loop {
            (dispatch.job)(index);
            match self.claim.next() {
                Next::Piece(further) => index = further,
                Next::Released { last } => {
                    if last {
                        self.wake_dispatcher();
                    }
                    return;
                }
            }
        }
    }

    /// Wake the dispatching thread if it is parked waiting for the hold this
    /// thread just released.
    fn wake_dispatcher(&self) {
        // Sequentially consistent with the dispatcher's announce-then-recheck
        // in `await_holders`: between this thread's release and its read of
        // `waiting`, and that thread's store to `waiting` and its read of the
        // claim, at least one must observe the other — so the wake is never
        // both skipped here and waited for there.
        if self.waiting.load(Ordering::SeqCst) != 0 {
            wake(self.claim.word(), 1);
        }
    }

    /// Park until every piece handed to a worker has been run.
    ///
    /// A dispatch whose pieces the dispatching thread drew itself has no holder
    /// and returns here without a single syscall, however many workers exist
    /// and whether or not any of them was ever scheduled. That is what keeps a
    /// dispatch's latency the cost of its work.
    fn await_holders(&self) {
        // The common case, and the one worth keeping free of barriers: no
        // worker took a piece, so there is nothing to announce and nothing to
        // clear.
        if holders(self.claim.load()) == 0 {
            return;
        }
        loop {
            let word = self.claim.load();
            if holders(word) == 0 {
                break;
            }
            self.waiting.store(1, Ordering::SeqCst);
            // Re-read after announcing: a holder that released in between would
            // have seen `waiting` still clear and skipped its wake.
            if holders(self.claim.load()) == 0 {
                break;
            }
            // The kernel compares the word as it parks, so a value that has since
            // changed refuses the park and re-tests above rather than stranding
            // this thread on a stale expectation.
            wait(self.claim.word(), word);
        }
        self.waiting.store(0, Ordering::SeqCst);
    }
}

/// A process's worker pool: a fixed set of threads that run the pieces of a
/// dispatch alongside the dispatching thread.
///
/// # What a pool costs
///
/// Idle, a worker holds its kernel-owned stack reservation and nothing else — it
/// is parked on a futex, consuming no CPU. Per dispatch it costs one wake syscall
/// from the dispatcher, one park syscall per worker as it runs out of work, and
/// one wake back to the dispatcher if it parked. That is why [`bands`] exists:
/// work too small to amortise those syscalls is never handed off, and a pool the
/// caller never dispatches wide costs exactly nothing.
///
/// [`bands`]: crate::bands
pub struct Pool {
    shared: Arc<Shared>,
    workers: Vec<JoinHandle<()>>,
    /// Held for one dispatch. A second dispatch — from another thread, or from
    /// inside a piece of the first — finds it taken and runs its work on the
    /// calling thread, so the pool can neither be re-entered nor deadlock.
    gate: Mutex<()>,
}

impl Pool {
    /// A pool with up to `workers` worker threads beside the dispatching thread.
    ///
    /// Creation degrades rather than fails: a thread the kernel refuses — the
    /// process is at its `threads` limit, or memory is exhausted — simply is not
    /// created, and the pool runs with the workers it did get.
    /// [`worker_count`](Self::worker_count) reports what that was, so a caller
    /// that cares can say so.
    #[must_use]
    pub fn with_workers(workers: usize) -> Self {
        // Bounded by the claim word's holder half, so a hold can never carry
        // into the pieces it counts alongside.
        let workers = workers.min(HOLDERS_MAX as usize);
        let shared = Arc::new(Shared::new());
        let mut threads = Vec::new();
        // Reserved once, so no `push` below can reallocate and a machine that
        // cannot afford the handles gets a one-participant pool rather than an
        // allocation abort.
        let room = threads.try_reserve_exact(workers).is_ok();
        for _ in 0..workers {
            if !room {
                break;
            }
            let mine = Arc::clone(&shared);
            match Thread::spawn(move || work(&mine)) {
                Ok(handle) => threads.push(handle),
                // The kernel would not grant another thread; stop asking.
                Err(_) => break,
            }
        }
        // No rendezvous: a pool between dispatches has no pieces to give, so a
        // worker still on its way to its loop can only find a draw refused.
        // Nothing waits for one to arrive.
        Self {
            shared,
            workers: threads,
            gate: Mutex::new(()),
        }
    }

    /// A pool for a machine with `online` online CPUs.
    ///
    /// The policy is one participant per CPU: the dispatching thread is one of
    /// them, so the pool creates `online - 1` workers. A single-CPU machine
    /// therefore creates no thread at all and every dispatch runs where it was
    /// issued — the same code, none of the cost. An `online` of `0` (a caller
    /// that could not discover the count) reads as `1` and fails closed the same
    /// way.
    ///
    /// The count is a *discovered* quantity, never a constant: a consumer reads
    /// it from the System Information API's per-core records and passes it here.
    #[must_use]
    pub fn for_cpus(online: usize) -> Self {
        Self::with_workers(online.max(1) - 1)
    }

    /// How many worker threads this pool actually holds, which is at most what it
    /// asked for.
    #[must_use]
    pub fn worker_count(&self) -> usize {
        self.workers.len()
    }
}

// SAFETY: `run` satisfies both of `JobRunner`'s obligations.
//
// 1. Each index reaches `job` at most once: the pieces left are set to `count`
//    per dispatch and every draw decrements them by one under a
//    compare-exchange, so an index is handed to exactly one participant.
// 2. `run` does not return until every invocation has: a worker reaches `job`
//    only through a draw that took a piece and a hold in one atomic, the
//    dispatcher draws until the pieces are exhausted and then waits for the
//    holders to reach zero, and a hold is released only after its holder has
//    run every piece it drew. A draw that took no piece ran none.
//
// The inline paths (no workers, one piece, a count wider than the claim word,
// or a dispatch already in flight) run the jobs in a plain loop on the calling
// thread and are trivially both.
unsafe impl JobRunner for Pool {
    fn width(&self) -> usize {
        // The dispatching thread is a participant, so a pool with no workers is
        // exactly as wide as no pool at all.
        self.workers.len().saturating_add(1)
    }

    fn run(&self, count: usize, job: &(dyn Fn(usize) + Sync)) {
        if count == 0 {
            return;
        }
        // A dispatch already in flight — a nested one, or a second thread's —
        // runs on the calling thread rather than waiting for the pool. This is
        // what makes the pool total: no arrangement of callers can deadlock it.
        let held = self.gate.try_lock();
        // A count wider than the pieces half of the claim word saturates past
        // the bound and runs on the calling thread with the other shapes that
        // are not worth a dispatch.
        let pieces = u32::try_from(count).unwrap_or(u32::MAX);
        if held.is_none() || count == 1 || pieces > PIECES_MAX || self.workers.is_empty() {
            for index in 0..count {
                job(index);
            }
            return;
        }
        let dispatch = Dispatch { job };
        let shared = &*self.shared;
        // The dispatching thread takes one piece itself, so a worker beyond the
        // rest could only wake, find the pieces gone and park again — two
        // syscalls and a scheduler activation for nothing.
        let rousing = u32::try_from(self.workers.len().min(count - 1)).unwrap_or(u32::MAX);
        shared.dispatch.store(erase(&dispatch), Ordering::Relaxed);
        // Opening the claim releases the publication: a participant whose draw
        // succeeds observes the pointer stored above.
        shared.claim.open(pieces);
        // And the epoch bump releases it to a worker that is still parked.
        shared.epoch.fetch_add(1, Ordering::Release);
        wake(&shared.epoch, rousing);

        while let Some(index) = shared.claim.advance() {
            job(index);
        }
        shared.await_holders();
        // No worker can read it again: the pieces are exhausted, so no further
        // draw can take a hold, and every hold already taken has been released.
        shared
            .dispatch
            .store(core::ptr::null_mut(), Ordering::Relaxed);
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        // `&mut self` excludes a concurrent dispatch, so the claim has no pieces
        // left and no worker is holding a published pointer.
        self.shared.stop.store(1, Ordering::Release);
        self.shared.epoch.fetch_add(1, Ordering::Release);
        wake(
            &self.shared.epoch,
            u32::try_from(self.workers.len()).unwrap_or(u32::MAX),
        );
        for handle in self.workers.drain(..) {
            // A worker that died on its own is already gone; either way this
            // thread does not proceed until it is.
            let _ = handle.join();
        }
    }
}

/// Erase a dispatch's borrow of its dispatching frame, for publication.
///
/// The lifetime is reinstated by `Shared::take_and_drain` under the protocol's
/// guarantee, which is where the argument for it lives.
fn erase(dispatch: &Dispatch<'_>) -> *mut Published {
    core::ptr::from_ref(dispatch).cast_mut().cast::<Published>()
}

/// One worker's whole life: park, take and run what a dispatch has left, park
/// again.
fn work(shared: &Shared) {
    // The epoch this worker has already run. One dispatch completes before the
    // next begins, so a worker is never more than one dispatch behind and the
    // counter cannot wrap past what it has seen. A worker that starts mid-flight
    // reads whatever epoch it finds: taking that for a dispatch it has run only
    // costs it the next one, and taking it for a fresh one only reaches a draw
    // the claim decides.
    let mut seen = shared.epoch.load(Ordering::Acquire);
    loop {
        while shared.epoch.load(Ordering::Acquire) == seen {
            if shared.stop.load(Ordering::Acquire) != 0 {
                return;
            }
            wait(&shared.epoch, seen);
        }
        seen = shared.epoch.load(Ordering::Acquire);
        if shared.stop.load(Ordering::Acquire) != 0 {
            // The teardown bump publishes no dispatch and waits for nobody, so
            // leaving without drawing is correct.
            return;
        }
        shared.take_and_drain();
    }
}

/// The address of an atomic word, in the form the futex syscalls take.
fn word_of(word: &AtomicU32) -> u64 {
    core::ptr::from_ref(word) as usize as u64
}

/// Park until `word` is woken, unless it no longer holds `expected`.
fn wait(word: &AtomicU32, expected: u32) {
    // SAFETY: `word` is a live, naturally aligned `AtomicU32` inside the `Shared`
    // an `Arc` keeps alive for every participant of this pool, so it is valid for
    // the call; the kernel resolves the address against this process's own space.
    let _ = unsafe { tairix_rt::futex_wait(word_of(word), expected, NO_TIMEOUT) };
}

/// Wake up to `count` participants parked on `word`.
fn wake(word: &AtomicU32, count: u32) {
    // SAFETY: as `wait`. The futex key is `(this process, address)` and the kernel
    // dereferences nothing, so a word with no waiters wakes no one.
    let _ = unsafe { tairix_rt::futex_wake(word_of(word), count) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{bands, for_each};
    use core::sync::atomic::AtomicUsize;

    /// On the host there is no syscall trap, so no thread is ever created and
    /// every pool is a one-participant pool. That is the degradation path, and it
    /// is exactly what a single-core machine gets — worth asserting rather than
    /// assuming.
    #[test]
    fn a_pool_that_gets_no_workers_is_one_participant_wide() {
        let pool = Pool::with_workers(4);
        assert_eq!(pool.worker_count(), 0);
        assert_eq!(pool.width(), 1);
        assert_eq!(bands(&pool, 1_000_000, 1), 1);
    }

    #[test]
    fn a_pool_with_no_workers_still_visits_every_piece() {
        let pool = Pool::with_workers(4);
        let mut items = [0u32; 32];
        for_each(&pool, &mut items, &|item| *item += 1);
        assert!(items.iter().all(|&seen| seen == 1));
    }

    /// The single-CPU policy: one participant, no thread asked for at all.
    #[test]
    fn one_online_cpu_asks_for_no_worker() {
        assert_eq!(Pool::for_cpus(1).worker_count(), 0);
        // An undiscoverable count fails closed the same way.
        assert_eq!(Pool::for_cpus(0).worker_count(), 0);
    }

    /// A dispatch issued from inside a piece of another must complete, not
    /// deadlock waiting for a pool it is itself occupying.
    #[test]
    fn a_nested_dispatch_runs_on_the_calling_thread() {
        let pool = Pool::with_workers(4);
        let visited = [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)];
        pool.run(visited.len(), &|index| {
            let mut inner = [0u32; 3];
            for_each(&pool, &mut inner, &|slot| *slot += 1);
            assert!(
                inner.iter().all(|&seen| seen == 1),
                "the nested dispatch must still visit every piece"
            );
            if let Some(slot) = visited.get(index) {
                slot.fetch_add(1, Ordering::Relaxed);
            }
        });
        assert!(visited.iter().all(|slot| slot.load(Ordering::Relaxed) == 1));
    }

    #[test]
    fn a_pool_drops_cleanly_with_no_dispatch_outstanding() {
        drop(Pool::with_workers(2));
    }

    /// A pool between dispatches has no piece to give, so a worker that wakes
    /// without a dispatch — spuriously, or having started late and misread the
    /// epoch — never reaches a stale published pointer, and takes no hold on
    /// the way to finding that out.
    #[test]
    fn an_idle_pool_refuses_a_draw_without_taking_a_hold() {
        let claim = Claim::new();
        assert!(claim.take().is_none());
        assert!(claim.advance().is_none());
        assert_eq!(holders(claim.load()), 0);
    }

    /// The whole point of the claim word, and the regression test for the
    /// 992 ms drag pause: a dispatcher that drew every piece itself has no
    /// holder to wait for, however many workers exist and whether or not any of
    /// them was ever scheduled. A worker arriving afterwards holds nothing.
    #[test]
    fn a_dispatch_the_dispatcher_drew_has_nothing_to_wait_for() {
        let claim = Claim::new();
        claim.open(3);
        assert_eq!(claim.advance(), Some(2));
        assert_eq!(claim.advance(), Some(1));
        assert_eq!(claim.advance(), Some(0));
        assert_eq!(claim.advance(), None);
        assert_eq!(holders(claim.load()), 0, "drawing takes no hold");
        assert!(
            claim.take().is_none(),
            "a worker arriving with the pieces gone takes no hold"
        );
        assert_eq!(holders(claim.load()), 0);
    }

    /// A worker's first draw takes the piece and the hold together, so the
    /// dispatcher can never see the pieces exhausted while that worker still
    /// reads the dispatch.
    #[test]
    fn a_holder_is_counted_from_the_draw_that_took_its_piece() {
        let claim = Claim::new();
        claim.open(1);
        assert_eq!(claim.take(), Some(0));
        let word = claim.load();
        assert_eq!((holders(word), remaining(word)), (1, 0));
        assert!(matches!(claim.next(), Next::Released { last: true }));
        assert_eq!(holders(claim.load()), 0);
    }

    /// One hold covers every piece its holder goes on to draw: a worker pays
    /// one hold for the dispatch, not one per piece.
    #[test]
    fn a_holder_keeps_its_hold_across_the_pieces_it_draws() {
        let claim = Claim::new();
        claim.open(3);
        assert_eq!(claim.take(), Some(2));
        assert!(matches!(claim.next(), Next::Piece(1)));
        assert!(matches!(claim.next(), Next::Piece(0)));
        assert_eq!(holders(claim.load()), 1, "still one hold, not three");
        assert!(matches!(claim.next(), Next::Released { last: true }));
        assert_eq!(claim.load(), 0);
    }

    /// Only the release that empties the holders wakes the dispatcher, so a
    /// dispatch with several workers pays one wake syscall rather than one each.
    #[test]
    fn only_the_last_release_reports_itself_as_last() {
        let claim = Claim::new();
        claim.open(2);
        assert_eq!(claim.take(), Some(1));
        assert_eq!(claim.take(), Some(0));
        assert_eq!(holders(claim.load()), 2);
        assert!(matches!(claim.next(), Next::Released { last: false }));
        assert!(matches!(claim.next(), Next::Released { last: true }));
        assert_eq!(holders(claim.load()), 0);
    }

    /// Every index is handed out exactly once across the dispatching thread and
    /// the workers, whatever order they interleave in — the first of
    /// `JobRunner`'s two obligations, over the word that decides it.
    #[test]
    fn every_piece_is_handed_out_exactly_once() {
        const COUNT: usize = 8;
        let claim = Claim::new();
        claim.open(8);
        let mut seen = Vec::new();
        // A worker takes its first piece, and from then on it and the
        // dispatching thread alternate draws against the same word.
        let mut worker = claim.take();
        while let Some(index) = worker {
            seen.push(index);
            if let Some(drawn) = claim.advance() {
                seen.push(drawn);
            }
            worker = match claim.next() {
                Next::Piece(further) => Some(further),
                Next::Released { .. } => None,
            };
        }
        while let Some(index) = claim.advance() {
            seen.push(index);
        }
        assert_eq!(holders(claim.load()), 0);
        seen.sort_unstable();
        assert_eq!(seen, (0..COUNT).collect::<Vec<_>>());
    }

    /// Opening the next dispatch offers its own pieces and carries no holder
    /// over from the last one.
    #[test]
    fn reopening_offers_fresh_pieces_with_no_holders_carried_over() {
        let claim = Claim::new();
        claim.open(1);
        assert_eq!(claim.take(), Some(0));
        assert!(matches!(claim.next(), Next::Released { last: true }));
        claim.open(2);
        let word = claim.load();
        assert_eq!((holders(word), remaining(word)), (0, 2));
    }

    /// A count the claim word cannot express runs on the calling thread rather
    /// than being split wrongly. `bands` answers at most four pieces per
    /// participant, so only a direct `run` can reach this.
    #[test]
    fn a_count_wider_than_the_claim_word_still_visits_every_piece() {
        let pool = Pool::with_workers(4);
        let count = PIECES_MAX as usize + 1;
        let visits = AtomicUsize::new(0);
        pool.run(count, &|_| {
            visits.fetch_add(1, Ordering::Relaxed);
        });
        assert_eq!(visits.load(Ordering::Relaxed), count);
    }

    /// More workers than the holder half of the word can count are not created,
    /// and the pool reports the number it holds rather than the number asked
    /// for — the same degradation as a thread the kernel refuses.
    #[test]
    fn a_worker_request_beyond_the_word_is_bounded_not_wrapped() {
        let pool = Pool::with_workers(HOLDERS_MAX as usize + 7);
        assert!(pool.worker_count() <= HOLDERS_MAX as usize);
        assert_eq!(pool.width(), pool.worker_count() + 1);
    }
}
