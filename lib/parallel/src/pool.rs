//! The fork-join worker pool: [`Pool`].
//!
//! # The protocol
//!
//! A dispatch publishes the work, bumps `epoch`, and wakes the workers parked on
//! it; every participant — the workers *and* the dispatching thread — claims
//! pieces off `claim` until they run out; each worker then decrements
//! `outstanding`, and the last one to reach zero wakes the dispatcher if it
//! parked. The dispatcher returns only once `outstanding` is zero.
//!
//! That last sentence is the whole lifetime argument. The published work is a
//! reference to a value on the **dispatcher's stack**, so the dispatcher must not
//! return while any worker could still read it. Waiting for every worker to
//! acknowledge the dispatch — not merely for every piece to be claimed — is what
//! guarantees that: a worker still between "saw the epoch" and "claimed a piece"
//! is one that has not decremented `outstanding` yet.
//!
//! # Why construction waits for the workers
//!
//! Because the barrier is over *workers*, every worker must be able to reach it.
//! A worker that has not run yet reads the epoch as it finds it — already bumped —
//! decides the dispatch is one it has seen, and parks without acknowledging;
//! `outstanding` then never reaches zero and the dispatch never returns. So
//! [`Pool::with_workers`] does not return until every worker has read its starting
//! epoch and counted itself in, which is what makes "the epoch a worker starts
//! from precedes every dispatch" true rather than likely. Removing that wait
//! reintroduces a hang whose appearance depends on scheduling order.
//!
//! # Nothing spins
//!
//! A worker with no dispatch to run parks in `futex_wait` on `epoch`; a
//! dispatcher whose workers are still running parks in `futex_wait` on
//! `outstanding`. The only cost of an idle pool is the address space its
//! workers' stacks reserve.

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

/// The state every participant of a pool reads.
struct Shared {
    /// The current dispatch, or null between dispatches. Published before
    /// [`Shared::epoch`] is bumped and cleared after every worker has finished
    /// with it.
    dispatch: AtomicPtr<Published>,
    /// Bumped once per dispatch. A parked worker waits on this word, so the bump
    /// plus a wake is what starts a dispatch.
    epoch: AtomicU32,
    /// The next piece number a participant claims. Reset per dispatch.
    claim: AtomicUsize,
    /// Workers that have not yet finished with the current dispatch. Set to the
    /// full worker count before the epoch bump, so it counts workers that have
    /// not even woken yet.
    outstanding: AtomicU32,
    /// Non-zero while the dispatching thread is parked on
    /// [`Shared::outstanding`], so the last worker pays for a wake syscall only
    /// when there is someone to wake.
    waiting: AtomicU32,
    /// Workers that have reached their loop and are therefore able to acknowledge
    /// a dispatch. [`Pool::with_workers`] does not return until this reaches the
    /// worker count.
    ready: AtomicU32,
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
            outstanding: AtomicU32::new(0),
            waiting: AtomicU32::new(0),
            ready: AtomicU32::new(0),
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

    /// Run whatever dispatch is published, if any.
    fn drain_published(&self) {
        let published = self.dispatch.load(Ordering::Acquire);
        if published.is_null() {
            return;
        }
        // SAFETY: the dispatcher publishes the pointer before the epoch bump this
        // worker observed, and neither returns nor clears the pointer until
        // `outstanding` reaches zero — which this worker decrements only after
        // this call. The value therefore outlives every read here, and the
        // reinstated lifetime is no wider: the borrow ends with this function.
        let dispatch = unsafe { &*published };
        self.drain(dispatch);
    }

    /// Record that this worker is finished with the current dispatch, waking the
    /// dispatching thread if it was the last and that thread is parked.
    fn finish(&self) {
        if self.outstanding.fetch_sub(1, Ordering::SeqCst) != 1 {
            return;
        }
        // Sequentially consistent with the dispatcher's announce-then-recheck in
        // `await_workers`: between this thread's decrement and its read of
        // `waiting`, and that thread's store to `waiting` and its read of
        // `outstanding`, at least one must observe the other — so the wake is
        // never both skipped here and waited for there.
        if self.waiting.load(Ordering::SeqCst) != 0 {
            wake(&self.outstanding, 1);
        }
    }

    /// Park until every worker has finished with the current dispatch.
    fn await_workers(&self) {
        loop {
            let left = self.outstanding.load(Ordering::Acquire);
            if left == 0 {
                break;
            }
            self.waiting.store(1, Ordering::SeqCst);
            // Re-read after announcing: a worker that finished in between would
            // have seen `waiting` still clear and skipped its wake.
            if self.outstanding.load(Ordering::SeqCst) == 0 {
                break;
            }
            // The kernel compares the word as it parks, so a value that has since
            // changed refuses the park and re-tests above rather than stranding
            // this thread on a stale expectation.
            wait(&self.outstanding, left);
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
        // A dispatch's barrier waits for *every* worker to acknowledge it, so a
        // worker the kernel has created but not yet run could never be waited for.
        // Waiting here for all of them to reach their loop is what makes that
        // barrier achievable — and it terminates because each of these threads
        // exists and is runnable, and its first act is to count itself in.
        let expected = u32::try_from(threads.len()).unwrap_or(u32::MAX);
        loop {
            let counted = shared.ready.load(Ordering::Acquire);
            if counted >= expected {
                break;
            }
            wait(&shared.ready, counted);
        }
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
// 2. `run` does not return until every invocation has: the dispatcher drains
//    until the pieces are exhausted and then waits for `outstanding` to reach
//    zero, and a worker decrements `outstanding` only after its own draining has
//    returned.
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
        shared.outstanding.store(workers, Ordering::Relaxed);
        shared.dispatch.store(erase(&dispatch), Ordering::Relaxed);
        // The release: a worker that observes the new epoch observes the pointer
        // and the reset counters with it.
        shared.epoch.fetch_add(1, Ordering::Release);
        wake(&shared.epoch, workers);

        shared.drain(&dispatch);
        shared.await_workers();
        // No worker can read it again: every one has finished with this dispatch,
        // and the next begins with a fresh publish.
        shared
            .dispatch
            .store(core::ptr::null_mut(), Ordering::Relaxed);
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        // `&mut self` excludes a concurrent dispatch, so no worker is holding a
        // published pointer and `outstanding` is already zero.
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
/// The lifetime is reinstated by [`Shared::drain_published`] under the protocol's
/// guarantee, which is where the argument for it lives.
fn erase(dispatch: &Dispatch<'_>) -> *mut Published {
    core::ptr::from_ref(dispatch).cast_mut().cast::<Published>()
}

/// One worker's whole life: park, run a dispatch, acknowledge it, park again.
fn work(shared: &Shared) {
    // The epoch this worker has already run. The dispatcher waits for every
    // worker before starting another, so a worker is never more than one dispatch
    // behind and the counter cannot wrap past what it has seen.
    //
    // Read before counting in: `with_workers` has not returned yet, so no
    // dispatch can have happened and this is the pre-dispatch epoch. Counting in
    // afterwards is what releases the constructor.
    let mut seen = shared.epoch.load(Ordering::Acquire);
    shared.ready.fetch_add(1, Ordering::Release);
    wake(&shared.ready, 1);
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
            // leaving without acknowledging is correct.
            return;
        }
        shared.drain_published();
        shared.finish();
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
}
