//! The fork-join worker pool: [`Pool`].
//!
//! # The protocol
//!
//! A dispatch opens its `Engagement`, publishes the work, bumps `epoch`, and
//! wakes the workers parked on it; every participant — the workers *and* the
//! dispatching thread — claims pieces off `claim` until they run out. A worker
//! reaches the work only by *joining* the engagement first, and releases its
//! hold when it is done. Once the pieces are exhausted the dispatcher closes the
//! engagement to further joins and returns as soon as every worker that joined
//! has released.
//!
//! # The lifetime argument
//!
//! The published work is a reference to a value on the **dispatcher's stack**, so
//! the dispatcher must not return while any worker could still read it. Joining
//! is what makes that decidable: a worker reads the published pointer only after
//! its join has succeeded, and a join succeeds only while the engagement is open,
//! so closing it and waiting for the holders to reach zero is exactly the
//! condition "no worker holds the pointer".
//!
//! # Why the barrier is over holders, not over workers
//!
//! Waiting for *every worker* instead would make a dispatch's latency the time
//! for the scheduler to run every worker at least once, even when the dispatching
//! thread had already run every piece itself. On a machine with more runnable
//! threads than cores that is unbounded, and it was measured at 429 ms on a
//! four-core board — a frame's worth of compositing spent waiting for help that
//! was no longer needed. Closing the engagement retracts the offer instead: a
//! worker that never got a CPU finds the join refused, touches nothing, and parks
//! again, so the dispatch costs what its work costs.
//!
//! No parallelism is given up. The dispatcher closes only once the pieces are
//! exhausted, and a worker that joined stays a holder until it has drained, so
//! the only join ever refused is one with no work left to do.
//!
//! # Nothing spins
//!
//! A worker with no dispatch to run parks in `futex_wait` on `epoch`; a
//! dispatcher with holders left parks in `futex_wait` on the engagement word. The
//! only cost of an idle pool is the address space its workers' stacks reserve.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicUsize, Ordering};

use tairix_rt::sync::Mutex;
use tairix_rt::thread::{JoinHandle, Thread};

use crate::JobRunner;

/// `futex_wait`'s "no timeout" spelling.
const NO_TIMEOUT: u64 = u64::MAX;

/// The work one dispatch is running, borrowed from the dispatching thread's own
/// frame for exactly as long as that dispatch lasts.
struct Dispatch<'a> {
    /// The number of pieces, so a participant knows when the work is exhausted.
    count: usize,
    /// The piece body, indexed by piece number.
    job: &'a (dyn Fn(usize) + Sync),
}

/// A [`Dispatch`] with its lifetime erased, which is the form it is published in:
/// a shared pointer cannot carry the dispatcher's frame lifetime, so the
/// protocol carries it instead.
type Published = Dispatch<'static>;

/// Set in an [`Engagement`] word once no further worker may join the dispatch.
const CLOSED: u32 = 1 << 31;

/// Workers still holding the dispatch an engagement word describes.
const fn holders(word: u32) -> u32 {
    word & !CLOSED
}

/// Most holders one engagement word can count, which is every bit below
/// [`CLOSED`].
const HOLDERS_MAX: u32 = CLOSED - 1;

/// Who may still reach one dispatch's published work, and who is still reading
/// it.
///
/// Both questions live in one word so that joining, and the dispatcher's
/// retraction of the offer to join, are a single atomic each and cannot
/// interleave into a state where a worker believes it may read a dispatch the
/// dispatcher believes it has finished with.
///
/// A pool between dispatches is closed, so a worker that wakes spuriously — or
/// that starts late and mistakes the epoch it found for a fresh dispatch — is
/// refused rather than reading a stale pointer.
struct Engagement(AtomicU32);

impl Engagement {
    const fn new() -> Self {
        Self(AtomicU32::new(CLOSED))
    }

    /// Admit joins to a fresh dispatch, with nobody holding it yet.
    ///
    /// Ordered by the dispatcher's own epoch bump, which is the release that
    /// publishes this alongside the work pointer.
    fn open(&self) {
        self.0.store(0, Ordering::Relaxed);
    }

    /// Take a hold on the dispatch if it is still admitting them, reporting
    /// whether this thread may now read the published work.
    ///
    /// Refused at [`HOLDERS_MAX`] as well as when closed, so the count can
    /// never carry into [`CLOSED`] and spuriously retract a live dispatch. No
    /// real pool reaches it — holders are bounded by the worker count — and a
    /// refused join costs only the help of one worker, since the dispatching
    /// thread claims every piece nobody else does.
    fn join(&self) -> bool {
        let mut word = self.0.load(Ordering::Acquire);
        loop {
            if word & CLOSED != 0 || holders(word) == HOLDERS_MAX {
                return false;
            }
            match self
                .0
                .compare_exchange_weak(word, word + 1, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return true,
                Err(seen) => word = seen,
            }
        }
    }

    /// Refuse further joins, reporting how many workers still hold the dispatch.
    fn close(&self) -> u32 {
        holders(self.0.fetch_or(CLOSED, Ordering::SeqCst))
    }

    /// Release this thread's hold, reporting the holders left after it.
    ///
    /// Only ever called by a thread whose [`join`](Self::join) succeeded, so the
    /// count is at least one and the decrement cannot reach into [`CLOSED`].
    fn release(&self) -> u32 {
        holders(self.0.fetch_sub(1, Ordering::SeqCst).saturating_sub(1))
    }

    /// The word as it stands, which is what a park compares against.
    fn load(&self) -> u32 {
        self.0.load(Ordering::Acquire)
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
    /// The next piece number a participant claims. Reset per dispatch.
    claim: AtomicUsize,
    /// Who may reach the current dispatch, and who still holds it.
    engagement: Engagement,
    /// Non-zero while the dispatching thread is parked on the engagement word,
    /// so the last holder pays for a wake syscall only when there is someone to
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
            claim: AtomicUsize::new(0),
            engagement: Engagement::new(),
            waiting: AtomicU32::new(0),
            stop: AtomicU32::new(0),
        }
    }

    /// Claim and run pieces of `dispatch` until none are left.
    fn drain(&self, dispatch: &Dispatch<'_>) {
        loop {
            let index = self.claim.fetch_add(1, Ordering::Relaxed);
            if index >= dispatch.count {
                return;
            }
            (dispatch.job)(index);
        }
    }

    /// Join the current dispatch and run it, or return having touched nothing
    /// because the dispatcher has already retracted it.
    fn join_and_drain(&self) {
        if !self.engagement.join() {
            return;
        }
        let published = self.dispatch.load(Ordering::Acquire);
        // SAFETY: this join succeeded, so the engagement was open — the
        // dispatcher had therefore neither closed it nor cleared the pointer,
        // and it will not return until this hold is released below. The pointer
        // is non-null because the epoch bump that woke this worker is a release
        // over both the publication and the open engagement, so observing the
        // new epoch observes them. The value outlives every read here and the
        // reinstated lifetime is no wider: the borrow ends with this function.
        let dispatch = unsafe { &*published };
        self.drain(dispatch);
        self.finish();
    }

    /// Release this worker's hold, waking the dispatching thread if it was the
    /// last and that thread is parked.
    fn finish(&self) {
        if self.engagement.release() != 0 {
            return;
        }
        // Sequentially consistent with the dispatcher's announce-then-recheck in
        // `await_holders`: between this thread's release and its read of
        // `waiting`, and that thread's store to `waiting` and its read of the
        // engagement, at least one must observe the other — so the wake is never
        // both skipped here and waited for there.
        if self.waiting.load(Ordering::SeqCst) != 0 {
            wake(self.engagement.word(), 1);
        }
    }

    /// Refuse further joins, then park until every worker that joined has
    /// released its hold.
    ///
    /// A dispatch whose pieces the dispatching thread ran itself closes with no
    /// holders and returns here without a single syscall, which is what keeps a
    /// dispatch's latency the cost of its work rather than of scheduling every
    /// worker.
    fn await_holders(&self) {
        if self.engagement.close() == 0 {
            return;
        }
        loop {
            let word = self.engagement.load();
            if holders(word) == 0 {
                break;
            }
            self.waiting.store(1, Ordering::SeqCst);
            // Re-read after announcing: a holder that released in between would
            // have seen `waiting` still clear and skipped its wake.
            if holders(self.engagement.load()) == 0 {
                break;
            }
            // The kernel compares the word as it parks, so a value that has since
            // changed refuses the park and re-tests above rather than stranding
            // this thread on a stale expectation.
            wait(self.engagement.word(), word);
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
        // No rendezvous: a pool between dispatches is closed, so a worker still
        // on its way to its loop can only find a join refused. Nothing waits for
        // one to arrive.
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
// 1. Each index reaches `job` at most once: `claim` is reset to zero per dispatch
//    and every participant takes its index with a single `fetch_add`, so no two
//    participants can be handed the same one.
// 2. `run` does not return until every invocation has: a worker reaches `job`
//    only through a successful join, the dispatcher drains until the pieces are
//    exhausted and then closes the engagement and waits for its holders to reach
//    zero, and a holder releases only after its own draining has returned. A join
//    refused by the close ran no piece at all.
//
// The inline paths (no workers, one piece, or a dispatch already in flight) run
// the jobs in a plain loop on the calling thread and are trivially both.
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
        // runs here rather than waiting for the pool. This is what makes the pool
        // total: no arrangement of callers can deadlock it.
        let held = self.gate.try_lock();
        if held.is_none() || count == 1 || self.workers.is_empty() {
            for index in 0..count {
                job(index);
            }
            return;
        }
        let dispatch = Dispatch { count, job };
        let shared = &*self.shared;
        let workers = u32::try_from(self.workers.len()).unwrap_or(u32::MAX);
        shared.claim.store(0, Ordering::Relaxed);
        shared.engagement.open();
        shared.dispatch.store(erase(&dispatch), Ordering::Relaxed);
        // The release: a worker that observes the new epoch observes the pointer,
        // the open engagement, and the reset claim with it.
        shared.epoch.fetch_add(1, Ordering::Release);
        wake(&shared.epoch, workers);

        shared.drain(&dispatch);
        shared.await_holders();
        // No worker can read it again: the engagement is closed to further joins
        // and every hold taken under it has been released.
        shared
            .dispatch
            .store(core::ptr::null_mut(), Ordering::Relaxed);
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        // `&mut self` excludes a concurrent dispatch, so the engagement is closed
        // and no worker is holding a published pointer.
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
/// The lifetime is reinstated by `Shared::join_and_drain` under the protocol's
/// guarantee, which is where the argument for it lives.
fn erase(dispatch: &Dispatch<'_>) -> *mut Published {
    core::ptr::from_ref(dispatch).cast_mut().cast::<Published>()
}

/// One worker's whole life: park, join and run a dispatch, park again.
fn work(shared: &Shared) {
    // The epoch this worker has already run. One dispatch completes before the
    // next begins, so a worker is never more than one dispatch behind and the
    // counter cannot wrap past what it has seen. A worker that starts mid-flight
    // reads whatever epoch it finds: taking that for a dispatch it has run only
    // costs it the next one, and taking it for a fresh one only reaches a join
    // the engagement decides.
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
            // leaving without joining is correct.
            return;
        }
        shared.join_and_drain();
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

    /// A pool between dispatches admits nobody, so a worker that wakes without a
    /// dispatch — spuriously, or having started late and misread the epoch —
    /// never reaches a stale published pointer.
    #[test]
    fn an_idle_pool_refuses_a_join() {
        let engagement = Engagement::new();
        assert!(!engagement.join());
        assert_eq!(engagement.close(), 0);
    }

    /// The whole point of the engagement: a dispatcher that ran every piece
    /// itself closes with nothing held and is free to return, however many
    /// workers exist and whether or not any of them was ever scheduled.
    #[test]
    fn a_dispatch_no_worker_joined_has_nothing_to_wait_for() {
        let engagement = Engagement::new();
        engagement.open();
        assert_eq!(engagement.close(), 0, "no holder means no wait");
        assert!(
            !engagement.join(),
            "a worker arriving after the close is refused"
        );
    }

    /// A worker that joined before the close is waited for, and the count the
    /// close reports is what the dispatcher must see released.
    #[test]
    fn a_close_reports_the_holders_it_must_wait_for() {
        let engagement = Engagement::new();
        engagement.open();
        assert!(engagement.join());
        assert!(engagement.join());
        assert_eq!(engagement.close(), 2);
        assert_eq!(engagement.release(), 1);
        assert_eq!(engagement.release(), 0, "the last release frees the frame");
    }

    /// Releasing before the dispatcher closes is ordinary: the holder count is
    /// already zero by the time it asks, so it still returns without parking.
    #[test]
    fn a_holder_that_releases_before_the_close_is_not_waited_for() {
        let engagement = Engagement::new();
        engagement.open();
        assert!(engagement.join());
        assert_eq!(engagement.release(), 0);
        assert_eq!(engagement.close(), 0);
    }

    /// The closed flag survives every release, so a park that compares the whole
    /// word is comparing a value only a release can change.
    #[test]
    fn the_closed_flag_outlives_its_holders() {
        let engagement = Engagement::new();
        engagement.open();
        assert!(engagement.join());
        engagement.close();
        assert_eq!(engagement.load(), CLOSED | 1);
        engagement.release();
        assert_eq!(engagement.load(), CLOSED);
        assert!(!engagement.join(), "a closed engagement stays closed");
    }

    /// Reopening is what makes the next dispatch reachable, and it clears the
    /// previous one's holders rather than inheriting them.
    #[test]
    fn reopening_admits_joins_again_with_no_holders_carried_over() {
        let engagement = Engagement::new();
        engagement.open();
        assert!(engagement.join());
        engagement.close();
        engagement.release();
        engagement.open();
        assert_eq!(holders(engagement.load()), 0);
        assert!(engagement.join());
        assert_eq!(engagement.close(), 1);
    }
}
