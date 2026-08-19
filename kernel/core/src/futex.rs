//! The futex: the one generic blocking primitive userland builds its mutex,
//! condition variable, and thread join over (`plans/THREADS.md` decision 5).
//!
//! A userland lock is a word in the process's own memory. While it is
//! uncontended, acquiring and releasing it is a pair of atomic operations and
//! the kernel never hears about it at all; only when a thread must actually
//! *wait* does it enter the kernel, name the word, and park. That is what keeps
//! a lock cheap and still lets a waiter give the CPU up rather than spin.
//!
//! # The key
//!
//! A wait key is `(ProcessId, user VA)` (decision 6). Address spaces are
//! per-process and hardware-isolated, so the same virtual address in two
//! processes names two unrelated words, and a key is unforgeable: the process
//! half comes from the kernel-attested capability record, never from the
//! caller. Cross-process (shared-memory-backed) futexes are a different
//! abstraction and deliberately absent.
//!
//! # Structure
//!
//! Waiters live in the one tested [`WaitQueue`] definition — its FIFO
//! wake-one fairness, its `O(log n)` deadline index, and its
//! register-before-retest lost-wake discipline — one queue per live key, held
//! in a bucket array sized from the discovered CPU count rather than a
//! hand-picked constant. Each queue is refcounted so a waker (or a waiter
//! about to park) drops the bucket lock *before* touching the scheduler: no
//! path holds a futex lock across an `unpark`, so the bucket locks can never
//! participate in a lock cycle with the scheduler's.
//!
//! A key's queue is created on demand and dropped once its last waiter
//! leaves, so an idle process holds no futex state. Both the registration and
//! the removal happen under the bucket lock, which is what makes the removal
//! safe: a queue that is in the map is never one a waiter is about to join,
//! and a queue a waiter *has* joined is never empty, so no wake can be lost
//! to a concurrent cleanup.
//!
//! Live keys are bounded without a cap of their own: a thread blocks on at
//! most one futex at a time, so a process can hold no more keys than it has
//! threads, which `LimitKind::Threads` already bounds.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::convert::Infallible;

use tairix_kernel_sched_api::TaskId;
use tairix_kernel_sec::ProcessId;
use tairix_sync::once::OnceCell;
use tairix_sync::SpinLock;

use crate::waitq::{WaitQueue, WaitQueueArch};

/// A futex wait key: the process that owns the address space and the user
/// virtual address of the 32-bit word inside it.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct FutexKey {
    /// The owning process, resolved from the caller's kernel-attested
    /// capability record.
    pub process: ProcessId,
    /// The word's user virtual address, naturally aligned.
    pub uaddr: u64,
}

/// One bucket of the key→queue table.
type Bucket = SpinLock<BTreeMap<FutexKey, Arc<WaitQueue>>>;

/// Buckets per discovered CPU.
///
/// Four gives a machine's cores room to contend on *distinct* keys without
/// serialising on one bucket lock, while costing a handful of empty
/// `BTreeMap`s per core — the bucket array is the only fixed-size structure
/// here, and it is sized from the hardware rather than hand-picked, so a
/// 128-core server and a single-core board each get a workable table from the
/// same policy.
const BUCKETS_PER_CPU: usize = 4;

/// The resolved bucket table, published **exactly once** — by
/// [`init_buckets`] at boot, or by the first resolution that finds none.
///
/// An *empty* table is the published "nothing ever sized one" answer, so that
/// decision costs no allocation and cannot fail; [`buckets`] maps it onto
/// [`FALLBACK_BUCKET`].
static BUCKETS: OnceCell<Vec<Bucket>> = OnceCell::new();

/// The single bucket a build that never sized the table uses.
///
/// The bucket *count* is a contention choice, not a correctness one, so
/// falling back to one bucket is honest rather than fail-closed: a futex still
/// blocks and wakes exactly as specified, it just serialises unrelated keys.
static FALLBACK_BUCKET: Bucket = SpinLock::new(BTreeMap::new());

/// Size the bucket table from the discovered CPU count.
///
/// Called once per boot, beside the other scheduler-derived publications, and
/// necessarily **before the first key resolves** — a table installed after
/// that is refused, because a key resolves to a bucket by index into whichever
/// table was live, so swapping tables mid-flight would strand a registered
/// waiter in a bucket no waker looks in.
pub fn init_buckets(cpu_count: usize) {
    if BUCKETS.is_initialised() {
        return;
    }
    let count = cpu_count.max(1) * BUCKETS_PER_CPU;
    let mut buckets = Vec::new();
    if buckets.try_reserve_exact(count).is_err() {
        // Deterministic OOM, never a panic: the single-bucket table stands.
        return;
    }
    for _ in 0..count {
        buckets.push(SpinLock::new(BTreeMap::new()));
    }
    let _ = BUCKETS.set(buckets);
}

/// The live bucket table, fixed for the rest of this boot by the first
/// resolution.
///
/// One atomic decision point, so a key can never resolve against two different
/// tables: whoever reaches here first publishes the answer — boot's sized
/// table, or the empty "never sized" one — and every later resolution reads
/// that same publication.
fn buckets() -> &'static [Bucket] {
    match BUCKETS.get_or_try_init(|| Ok::<Vec<Bucket>, Infallible>(Vec::new())) {
        Ok(table) if !table.is_empty() => table.as_slice(),
        // The published empty table (nothing sized one), or a poisoned cell an
        // infallible initialiser cannot actually produce: one bucket.
        _ => core::slice::from_ref(&FALLBACK_BUCKET),
    }
}

/// The index `key` hashes to in a table of `len` buckets.
///
/// Fibonacci hashing (multiply by the 64-bit golden-ratio odd constant, whose
/// high bits mix every input bit) over the process id combined with the word
/// index. Shifting the address by two drops the always-zero alignment bits, so
/// adjacent words in one lock array land in different buckets instead of
/// crowding one.
///
/// A `len` of zero folds to one so the index is always computable; [`buckets`]
/// never yields an empty table.
fn bucket_index(key: FutexKey, len: usize) -> usize {
    const GOLDEN: u64 = 0x9E37_79B9_7F4A_7C15;
    let mixed =
        key.process.0.wrapping_mul(GOLDEN).rotate_left(31) ^ (key.uaddr >> 2).wrapping_mul(GOLDEN);
    // The high bits carry the mixing, so they are the ones folded down.
    (mixed >> 32) as usize % len.max(1)
}

/// The bucket a key belongs to.
fn bucket_of(key: FutexKey) -> &'static Bucket {
    let table = buckets();
    // `table.len()` is at least one, so the index is in range.
    &table[bucket_index(key, table.len())]
}

/// Register `thread` as a waiter on `key` with an absolute monotonic
/// `deadline_ns` ([`crate::waitq::NO_DEADLINE`] for none), returning the queue
/// it joined.
///
/// The caller re-tests the word *after* this returns and parks only if it
/// still holds the expected value, which is the lost-wake discipline: a wake
/// that lands in the window between the test and the park finds the waiter
/// already registered.
#[must_use]
pub fn register(key: FutexKey, thread: TaskId, deadline_ns: u64) -> Arc<WaitQueue> {
    let mut bucket = bucket_of(key).lock();
    let queue = Arc::clone(
        bucket
            .entry(key)
            .or_insert_with(|| Arc::new(WaitQueue::new())),
    );
    // Registered under the bucket lock, so the cleanup below can use
    // "the queue is empty" as a safe removal condition.
    queue.register(thread, deadline_ns);
    queue
}

/// Remove `thread` from `key`'s wait set, dropping the queue when it was the
/// last waiter.
pub fn deregister(key: FutexKey, thread: TaskId) {
    let mut bucket = bucket_of(key).lock();
    let Some(queue) = bucket.get(&key) else {
        return;
    };
    queue.deregister(thread);
    if queue.is_empty() {
        bucket.remove(&key);
    }
}

/// Wake the `count` oldest threads waiting on `key`, returning how many were
/// woken.
///
/// Waking nobody is success: by the register-before-retest discipline a thread
/// that has not parked yet re-tests the word itself.
pub fn wake(arch: &dyn WaitQueueArch, key: FutexKey, count: usize) -> usize {
    // Lift the queue handle out from under the bucket lock and release it
    // before the wake, so the scheduler's locks are never taken while a futex
    // lock is held.
    let queue = {
        let bucket = bucket_of(key).lock();
        bucket.get(&key).map(Arc::clone)
    };
    match queue {
        Some(queue) => queue.wake_n(arch, count),
        None => 0,
    }
}

/// Wake the `count` oldest threads of `process` waiting on `uaddr`, through the
/// boot-installed wait-queue hook.
///
/// The form every in-kernel caller uses: the `futex_wake` syscall handler and
/// the thread-exit clear-on-exit notification both reach the futex through this,
/// so neither has to resolve the scheduler hook itself. A build with no hook
/// installed can have nothing parked, so waking nobody is the honest answer.
#[must_use]
pub fn wake_installed(process: ProcessId, uaddr: u64, count: usize) -> usize {
    let Some(arch) = crate::waitq::wait_arch() else {
        return 0;
    };
    wake(arch, FutexKey { process, uaddr }, count)
}

/// Drop every futex key belonging to `process`.
///
/// Driven by the one shared task-reclaim path, so a process that exits, faults,
/// or is killed leaves no queue behind. Any thread still registered is already
/// dead, so nothing is woken.
pub fn release_process(process: ProcessId) {
    for bucket in buckets() {
        let mut map = bucket.lock();
        map.retain(|key, _| key.process != process);
    }
}

/// The absolute monotonic deadline a `futex_wait`'s relative `timeout_ns`
/// names, or [`crate::waitq::NO_DEADLINE`] when the caller asked for none.
///
/// [`u64::MAX`] is the ABI's "no timeout" spelling. Every other value is added
/// to `now_ns` and clamped one nanosecond short of that sentinel, so a span long
/// enough to saturate still names a *deadline* the sweep fires rather than
/// silently becoming an indefinite wait.
#[must_use]
pub fn deadline_for(now_ns: u64, timeout_ns: u64) -> u64 {
    if timeout_ns == u64::MAX {
        return crate::waitq::NO_DEADLINE;
    }
    now_ns
        .saturating_add(timeout_ns)
        .min(crate::waitq::NO_DEADLINE - 1)
}

/// The soonest finite deadline any futex waiter is holding, or [`None`].
///
/// Folded into the kernel's nearest-armed-wakeup so a timed `futex_wait`
/// cannot be dropped because another queue armed a later one-shot.
#[must_use]
pub fn earliest_deadline() -> Option<u64> {
    let mut soonest: Option<u64> = None;
    for bucket in buckets() {
        // The handles are cloned out and the lock released before the
        // per-queue read, so the bucket lock is never held across another
        // lock acquisition.
        let queues: Vec<Arc<WaitQueue>> = bucket.lock().values().map(Arc::clone).collect();
        for queue in queues {
            if let Some(deadline) = queue.earliest_deadline() {
                soonest = Some(soonest.map_or(deadline, |current: u64| current.min(deadline)));
            }
        }
    }
    soonest
}

/// Release every futex waiter whose finite deadline is at or before `now_ns`.
///
/// Part of the kernel's dispatcher-context deadline sweep, so a
/// `futex_wait(timeout)` fires even on an otherwise-idle CPU.
pub fn sweep(arch: &dyn WaitQueueArch, now_ns: u64) {
    for bucket in buckets() {
        let queues: Vec<Arc<WaitQueue>> = bucket.lock().values().map(Arc::clone).collect();
        for queue in queues {
            queue.sweep(arch, now_ns);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::waitq::NO_DEADLINE;

    extern crate std;

    use core::cell::RefCell;

    /// A [`WaitQueueArch`] double recording every `unpark`, so the wake
    /// fan-out is observable without a scheduler.
    struct MockArch {
        unparked: RefCell<Vec<TaskId>>,
    }

    impl MockArch {
        fn new() -> Self {
            Self {
                unparked: RefCell::new(Vec::new()),
            }
        }

        fn taken(&self) -> Vec<TaskId> {
            core::mem::take(&mut self.unparked.borrow_mut())
        }
    }

    // SAFETY-INVARIANT: the double is only ever driven from one test thread
    // at a time; the `Sync` claim exists solely to satisfy the trait bound.
    unsafe impl Sync for MockArch {}

    impl WaitQueueArch for MockArch {
        fn unpark(&self, task: TaskId) {
            self.unparked.borrow_mut().push(task);
        }

        fn now_ns(&self) -> u64 {
            0
        }

        fn set_wakeup(&self, _deadline_ns: Option<u64>) {}
    }

    /// Keys unique to each test, so the process-global bucket table is never
    /// shared between parallel test threads.
    fn key(process: u64, uaddr: u64) -> FutexKey {
        FutexKey {
            process: ProcessId(process),
            uaddr,
        }
    }

    #[test]
    fn a_key_holds_no_queue_until_a_thread_waits_and_none_after_it_leaves() {
        let k = key(0x9001, 0x4000);
        assert_eq!(wake(&MockArch::new(), k, 1), 0, "no waiter, nothing woken");
        let _queue = register(k, 1, NO_DEADLINE);
        assert!(bucket_of(k).lock().contains_key(&k));
        deregister(k, 1);
        assert!(
            !bucket_of(k).lock().contains_key(&k),
            "the last waiter's exit drops the queue"
        );
    }

    #[test]
    fn wake_releases_the_oldest_waiters_first_and_bounds_the_count() {
        let k = key(0x9002, 0x4000);
        let arch = MockArch::new();
        for id in 1..=3 {
            let _ = register(k, id, NO_DEADLINE);
        }
        assert_eq!(wake(&arch, k, 2), 2);
        assert_eq!(arch.taken(), std::vec![1, 2]);
        // A count past the waiter count wakes exactly the waiters present.
        assert_eq!(wake(&arch, k, usize::MAX), 3);
        for id in 1..=3 {
            deregister(k, id);
        }
    }

    #[test]
    fn two_processes_at_one_address_are_distinct_keys() {
        let mine = key(0x9003, 0x4000);
        let theirs = key(0x9004, 0x4000);
        let arch = MockArch::new();
        let _ = register(mine, 7, NO_DEADLINE);
        assert_eq!(
            wake(&arch, theirs, usize::MAX),
            0,
            "another process's word must not reach my waiter"
        );
        assert_eq!(wake(&arch, mine, usize::MAX), 1);
        deregister(mine, 7);
    }

    #[test]
    fn a_finite_deadline_is_visible_to_the_sweep_and_the_nearest_arming() {
        let k = key(0x9005, 0x4000);
        let arch = MockArch::new();
        let _ = register(k, 11, 500);
        assert_eq!(
            earliest_deadline().map(|d| d <= 500),
            Some(true),
            "the futex deadline joins the kernel's nearest armed wakeup"
        );
        sweep(&arch, 499);
        assert!(arch.taken().is_empty(), "not yet due");
        sweep(&arch, 500);
        assert_eq!(arch.taken(), std::vec![11]);
        deregister(k, 11);
    }

    #[test]
    fn a_relative_timeout_becomes_an_absolute_deadline_that_cannot_read_as_none() {
        assert_eq!(deadline_for(1_000, 500), 1_500);
        assert_eq!(deadline_for(0, 0), 0, "an elapsed timeout is due at once");
        assert_eq!(
            deadline_for(1_000, u64::MAX),
            NO_DEADLINE,
            "the ABI's u64::MAX means explicit wake only"
        );
        // A span that saturates must still name a real deadline: rounding up to
        // the sentinel would turn a timed wait into an indefinite one.
        let saturated = deadline_for(u64::MAX - 1, u64::MAX - 1);
        assert_eq!(saturated, NO_DEADLINE - 1);
        assert_ne!(saturated, NO_DEADLINE);
    }

    #[test]
    fn a_dead_process_leaves_no_key_behind() {
        let k = key(0x9006, 0x4000);
        let other = key(0x9007, 0x4000);
        let _ = register(k, 21, NO_DEADLINE);
        let _ = register(other, 22, NO_DEADLINE);
        release_process(ProcessId(0x9006));
        assert!(!bucket_of(k).lock().contains_key(&k));
        assert!(
            bucket_of(other).lock().contains_key(&other),
            "another process's keys are untouched"
        );
        deregister(other, 22);
    }

    /// A per-CPU-sized table is only worth having if neighbouring lock words
    /// do not all crowd one bucket.
    ///
    /// Asserted against the pure index function rather than by installing a
    /// table: the live table is fixed for the boot by its first use, so a test
    /// that sized it would decide which table every *other* test's keys resolve
    /// against — and one that registered a key before the swap would then wake
    /// against a bucket its waiter is not in.
    #[test]
    fn adjacent_words_of_one_lock_array_spread_across_buckets() {
        let len = 8 * BUCKETS_PER_CPU;
        let mut seen = alloc::collections::BTreeSet::new();
        for word in 0..16u64 {
            seen.insert(bucket_index(key(0x9008, 0x4000 + word * 4), len));
        }
        assert!(
            seen.len() > 1,
            "16 adjacent lock words landed in a single bucket"
        );
    }

    /// One process's whole lock array must not collapse onto one bucket
    /// either — the process id is mixed, not just the address.
    #[test]
    fn distinct_processes_at_one_address_spread_across_buckets() {
        let len = 8 * BUCKETS_PER_CPU;
        let mut seen = alloc::collections::BTreeSet::new();
        for process in 0..16u64 {
            seen.insert(bucket_index(key(0x9100 + process, 0x4000), len));
        }
        assert!(
            seen.len() > 1,
            "16 processes' words at one address landed in a single bucket"
        );
    }

    /// Every index the hash produces is a valid subscript, including for the
    /// degenerate one-bucket table the never-sized build resolves against.
    #[test]
    fn a_bucket_index_is_always_in_range() {
        for len in [1usize, 2, 3, 4 * BUCKETS_PER_CPU, 128 * BUCKETS_PER_CPU] {
            for word in 0..64u64 {
                let index = bucket_index(key(word, word * 4), len);
                assert!(index < len, "index {index} out of a {len}-bucket table");
            }
        }
    }

    /// The table is fixed for the boot by its first use: a sizing that arrives
    /// after a key has resolved is refused rather than stranding that key's
    /// waiters in a bucket no waker looks in.
    ///
    /// The whole test binary shares one table and every sibling test resolves a
    /// key, so by the time this runs the table is already latched — precisely
    /// the state being asserted. A swap here made the sibling wake and sweep
    /// tests fail intermittently, with a waiter the waker could not find
    /// (`plans/OPEN-DEFECTS.md` D45).
    #[test]
    fn a_resolved_table_is_never_swapped_out_from_under_a_live_key() {
        let k = key(0x9009, 0x4000);
        let _queue = register(k, 31, NO_DEADLINE);
        let before = core::ptr::from_ref(bucket_of(k)) as usize;
        let len_before = buckets().len();

        // A late sizing must change nothing: neither the table nor where this
        // live key resolves.
        init_buckets(8);
        assert_eq!(buckets().len(), len_before, "the live table was replaced");
        assert_eq!(
            core::ptr::from_ref(bucket_of(k)) as usize,
            before,
            "a live key moved bucket"
        );
        assert_eq!(wake(&MockArch::new(), k, 1), 1, "the waiter is still found");
        deregister(k, 31);
    }
}
