//! Per-CPU weighted-vruntime run queue for the CFQ policy.
//!
//! Each CPU owns one [`RunQueue`]: an ordered set of ready [`Entry`]
//! records (a [`TaskId`] keyed by its current virtual runtime) kept under
//! a single `SpinLock`, plus a monotonic `min_vruntime` floor and the
//! total weight of the tasks the CPU owns.
//!
//! Unlike the MLFQ sibling's per-band Chase–Lev deques, CFQ orders the
//! ready set by a continuous virtual runtime and always dispatches the
//! task with the *smallest* vruntime — the "leftmost" entity, exactly as
//! Linux CFS picks the leftmost node of its per-runqueue red-black tree
//! (Molnar's Completely Fair Scheduler). The set is a [`BTreeSet`] keyed
//! by `(vruntime, id)`, so the leftmost pick and every insert/remove are
//! `O(log n)` — the right structure from the start, never an `O(n)` scan
//! of a growable list on the dispatch hot path.
//!
//! Virtual runtime is fixed-point with [`SCALE`] sub-units per unit of
//! service so the weighted divisions stay in integer arithmetic (no
//! floats in kernel paths, deterministic).

use alloc::collections::{BTreeSet, VecDeque};

use tairix_sync::SpinLock;

use crate::TaskId;

/// Fixed-point scaling factor for virtual runtime.
///
/// One unit of dispatched service is worth `SCALE` virtual sub-units
/// before the per-task weight division, keeping `service * SCALE /
/// weight` an exact integer for the small integer weights this policy
/// uses.
pub(crate) const SCALE: u64 = 1 << 20;

/// Virtual-runtime head start a task joining the competition is placed
/// ahead of the CPU's timeline floor by.
///
/// Placing a joiner *level* with the leftmost ready entry leaves the
/// `(vruntime, id)` tie-break to settle the pick, which hands the CPU to the
/// lower id every time: a task that wakes among a CPU-bound population it was
/// spawned after then loses a full scheduling round on every wake, so an
/// I/O-bound task pays that round per round trip. One unit of service — the
/// smallest increment [`vslice`] can charge — settles the pick instead, and
/// every dispatch charges at least that much back, so a task that wakes
/// repeatedly cannot outrun one that has been ready all along. The placement
/// is absolute against the monotonic floor, so the head start can never
/// accumulate past this bound (the CFS `place_entity` sleeper credit).
pub(crate) const SLEEPER_CREDIT: u64 = SCALE;

/// Weighted virtual-runtime increment `service_ticks` of execution costs a
/// task of `weight`.
///
/// A `weight`-4 task accrues vruntime a quarter as fast as a `weight`-1
/// task for the same elapsed service. A zero-tick observation is charged one
/// tick so coarse host clocks still make deterministic forward progress, and
/// the quotient/remainder form avoids overflowing `service_ticks * SCALE`.
#[must_use]
pub(crate) fn vslice(service_ticks: u64, weight: u64) -> u64 {
    let service = service_ticks.max(1);
    let divisor = weight.max(1);
    let whole = service / divisor;
    let remainder = service % divisor;
    whole.saturating_mul(SCALE).saturating_add(
        remainder
            .saturating_mul(SCALE)
            .checked_div(divisor)
            .unwrap_or(0),
    )
}

/// A ready task as tracked by a [`RunQueue`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) struct Entry {
    /// The ready task.
    pub id: TaskId,
    /// The task's virtual runtime `vruntime` (fixed point). The smallest
    /// `vruntime` among ready entries wins the pick.
    pub vruntime: u64,
}

/// Mutable interior of a [`RunQueue`], guarded by one `SpinLock`.
struct Inner {
    /// Strict-priority real-time band, in FIFO arrival order. Any entry
    /// here is dispatched (and stolen) before *any* time-shared entry in
    /// [`Self::ready`], regardless of virtual runtime — the
    /// [`tairix_kernel_sched_api::SchedClass::Realtime`] guarantee. A task
    /// re-enqueued after a yield goes to the back, so equal real-time peers
    /// share the CPU round-robin. Real-time tasks carry no virtual runtime;
    /// only [`Self::total_weight`] counts them (for load-balanced
    /// placement). Bounded by [`Self::capacity`] exactly like the fair set.
    rt_ready: VecDeque<TaskId>,
    /// Ready set keyed by `(vruntime, seq, id)` so the leftmost element is
    /// the smallest-vruntime task, ties broken by arrival order. The CFS
    /// red-black tree analog.
    ///
    /// The tie-break is the enqueue sequence rather than the task id
    /// because equal virtual runtimes are not rare — every task admitted
    /// before the floor has advanced is placed at the same point, so a whole
    /// burst of fresh tasks ties — and ordering those by identity would let
    /// an id decide who runs first. Task ids are drawn at random, so that
    /// would be an arbitrary winner; even ordered ids would systematically
    /// favour one task over another. Arrival order is the fair answer and
    /// the one a reader expects of a run queue.
    ready: BTreeSet<(u64, u64, TaskId)>,
    /// Monotonic enqueue counter supplying the FIFO half of a `ready` key.
    ///
    /// One increment per enqueue, so it cannot wrap in any real uptime; a
    /// wrap would only reorder entries that share a virtual runtime, never
    /// lose one.
    next_seq: u64,
    /// Monotonic floor tracking the front of the CPU's timeline. A task
    /// joining or re-joining this CPU is placed one [`SLEEPER_CREDIT`] ahead
    /// of it, so a task that slept at a low vruntime gets a bounded head
    /// start rather than leaping the running population and monopolising the
    /// CPU (the CFS `place_entity` / min-vruntime rule).
    min_vruntime: u64,
    /// Sum of the weights of every task this CPU currently owns (queued
    /// here or running off it). Read by the placement path to balance
    /// load across CPUs.
    total_weight: u64,
    /// Compile-time bound on `ready.len()` — back-pressure, never a
    /// reallocation past this point (bounded queues; an unbounded queue
    /// is a `DoS` amplifier).
    capacity: usize,
}

/// Per-CPU CFQ run queue.
pub(crate) struct RunQueue {
    inner: SpinLock<Inner>,
}

impl RunQueue {
    /// Construct an empty queue bounded to `capacity` ready entries.
    ///
    /// Returns `None` if `capacity` is not a power of two `>= 2`,
    /// mirroring the sibling policies' queue-capacity contract so every
    /// policy accepts the same [`crate::SchedulerConfig`].
    pub(crate) fn try_new(capacity: usize) -> Option<Self> {
        if capacity < 2 || !capacity.is_power_of_two() {
            return None;
        }
        Some(Self {
            inner: SpinLock::new(Inner {
                rt_ready: VecDeque::new(),
                ready: BTreeSet::new(),
                next_seq: 0,
                min_vruntime: 0,
                total_weight: 0,
                capacity,
            }),
        })
    }

    /// Account a task joining this CPU's competition: add its `weight` and
    /// return the vruntime it should adopt — one [`SLEEPER_CREDIT`] ahead of
    /// this CPU's monotonic timeline floor, so it sorts strictly before the
    /// population that has been running rather than tying with it.
    ///
    /// The floor advances only to a *picked* task's vruntime, so the head
    /// start is bounded by that one credit however long the joiner slept: it
    /// cannot leap the running population (the CFS `place_entity` rule).
    pub(crate) fn admit_weight(&self, weight: u64) -> u64 {
        let mut g = self.inner.lock();
        g.total_weight = g.total_weight.saturating_add(weight);
        g.min_vruntime.saturating_sub(SLEEPER_CREDIT)
    }

    /// Account a real-time task joining this CPU's competition: add its
    /// `weight` so load-balanced placement still counts it, without the
    /// virtual-runtime placement the fair band's [`Self::admit_weight`]
    /// performs (a real-time task carries no vruntime).
    pub(crate) fn add_weight(&self, weight: u64) {
        let mut g = self.inner.lock();
        g.total_weight = g.total_weight.saturating_add(weight);
    }

    /// Account a task leaving this CPU's competition: subtract `weight`.
    pub(crate) fn remove_weight(&self, weight: u64) {
        let mut g = self.inner.lock();
        g.total_weight = g.total_weight.saturating_sub(weight);
    }

    /// Total weight currently competing on this CPU (`0` when it is
    /// idle). The placement path reads it to put new and woken work on
    /// the least-loaded eligible CPU.
    pub(crate) fn competing_weight(&self) -> u64 {
        self.inner.lock().total_weight
    }

    /// Number of ready entries currently queued on this CPU.
    ///
    /// The task a CPU is *running* is held in the scheduler's current-task
    /// slot, not in this queue, so a non-zero count means at least one
    /// **other** ready task is waiting — a competitor.
    pub(crate) fn ready_len(&self) -> usize {
        let g = self.inner.lock();
        g.rt_ready.len() + g.ready.len()
    }

    /// Push a ready entry. Returns `Err(id)` if the queue is at its
    /// compile-time bound (the caller then routes it to overflow).
    pub(crate) fn push(&self, entry: Entry) -> Result<(), TaskId> {
        let mut g = self.inner.lock();
        if g.ready.len() >= g.capacity {
            return Err(entry.id);
        }
        let seq = g.next_seq;
        g.next_seq = g.next_seq.wrapping_add(1);
        g.ready.insert((entry.vruntime, seq, entry.id));
        Ok(())
    }

    /// Push a real-time task onto the back of the strict-priority band
    /// (FIFO / round-robin). Returns `Err(id)` if the band is at its
    /// compile-time bound (the caller then routes it to overflow), exactly
    /// like [`Self::push`].
    pub(crate) fn push_rt(&self, id: TaskId) -> Result<(), TaskId> {
        let mut g = self.inner.lock();
        if g.rt_ready.len() >= g.capacity {
            return Err(id);
        }
        g.rt_ready.push_back(id);
        Ok(())
    }

    /// Pick and remove the smallest-vruntime ready task (the leftmost
    /// entity), advancing the monotonic floor to it so the timeline never
    /// runs backwards.
    pub(crate) fn pick(&self) -> Option<Entry> {
        let mut g = self.inner.lock();
        // Strict priority: any ready real-time task is dispatched before the
        // smallest-vruntime fair task, and its pick never advances the fair
        // timeline floor (a real-time task carries no vruntime).
        if let Some(id) = g.rt_ready.pop_front() {
            return Some(Entry { id, vruntime: 0 });
        }
        let &key = g.ready.iter().next()?;
        g.ready.remove(&key);
        let (vruntime, _seq, id) = key;
        if vruntime > g.min_vruntime {
            g.min_vruntime = vruntime;
        }
        Some(Entry { id, vruntime })
    }

    /// Steal the smallest-vruntime ready entry for another CPU, if any.
    /// Weight bookkeeping is settled by the caller as the task changes
    /// owning CPU.
    pub(crate) fn steal(&self) -> Option<Entry> {
        let mut g = self.inner.lock();
        // Steal a waiting real-time task first: moving it to an idle CPU
        // shortens its dispatch latency, and it stays strict-priority there.
        if let Some(id) = g.rt_ready.pop_front() {
            return Some(Entry { id, vruntime: 0 });
        }
        let &key = g.ready.iter().next()?;
        g.ready.remove(&key);
        let (vruntime, _seq, id) = key;
        Some(Entry { id, vruntime })
    }

    /// Drop the weight of a stolen task from this queue's competition.
    pub(crate) fn release_weight(&self, weight: u64) {
        self.remove_weight(weight);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(id: TaskId, vruntime: u64) -> Entry {
        Entry { id, vruntime }
    }

    #[test]
    fn rejects_bad_capacity() {
        assert!(RunQueue::try_new(0).is_none());
        assert!(RunQueue::try_new(1).is_none());
        assert!(RunQueue::try_new(3).is_none());
        assert!(RunQueue::try_new(4).is_some());
    }

    #[test]
    fn push_is_bounded() {
        let q = RunQueue::try_new(2).expect("q");
        assert!(q.push(e(1, 0)).is_ok());
        assert!(q.push(e(2, 10)).is_ok());
        assert_eq!(q.push(e(3, 20)), Err(3));
    }

    #[test]
    fn pick_prefers_smallest_vruntime() {
        let q = RunQueue::try_new(8).expect("q");
        q.push(e(1, 30)).expect("push");
        q.push(e(2, 10)).expect("push");
        q.push(e(3, 20)).expect("push");
        assert_eq!(q.pick().map(|x| x.id), Some(2));
        assert_eq!(q.pick().map(|x| x.id), Some(3));
        assert_eq!(q.pick().map(|x| x.id), Some(1));
        assert_eq!(q.pick(), None);
    }

    /// Equal virtual runtimes are picked in arrival order, so identity
    /// never decides who runs first — task ids are drawn at random, and an
    /// id-ordered tie-break would hand the choice to the draw.
    #[test]
    fn pick_ties_break_on_arrival_order() {
        let q = RunQueue::try_new(8).expect("q");
        q.push(e(5, 7)).expect("push");
        q.push(e(2, 7)).expect("push");
        q.push(e(9, 7)).expect("push");
        assert_eq!(q.pick().map(|x| x.id), Some(5), "first in, first picked");
        assert_eq!(q.pick().map(|x| x.id), Some(2));
        assert_eq!(q.pick().map(|x| x.id), Some(9));
    }

    #[test]
    fn floor_is_monotonic_and_places_joiners_just_ahead_of_it() {
        let q = RunQueue::try_new(8).expect("q");
        q.push(e(1, 100 * SCALE)).expect("push");
        // Picking advances the floor to the picked task's vruntime.
        assert_eq!(q.pick().map(|x| x.vruntime), Some(100 * SCALE));
        // A joiner lands one credit ahead of that floor — never a stale zero,
        // and never level with it, which would leave arrival order to decide
        // the pick and put the joiner behind the queued population.
        assert_eq!(q.admit_weight(1), 100 * SCALE - SLEEPER_CREDIT);
    }

    #[test]
    fn a_joiner_sorts_ahead_of_every_ready_entry_at_the_floor() {
        let q = RunQueue::try_new(8).expect("q");
        // A population parked at the floor, every one of them enqueued
        // before the joiner below.
        for id in 1..=4 {
            q.push(e(id, 100 * SCALE)).expect("push");
        }
        assert_eq!(q.pick().map(|x| x.id), Some(1), "the floor is established");
        let joiner = q.admit_weight(1);
        q.push(e(9, joiner)).expect("push");
        assert_eq!(
            q.pick().map(|x| x.id),
            Some(9),
            "a joiner outranks entries level with the floor despite arriving last"
        );
    }

    #[test]
    fn an_empty_queues_floor_cannot_underflow() {
        let q = RunQueue::try_new(8).expect("q");
        assert_eq!(q.admit_weight(1), 0, "a zero floor saturates, never wraps");
    }

    #[test]
    fn vruntime_delta_scales_elapsed_service_by_weight() {
        assert_eq!(vslice(8, 4), SCALE * 2);
        assert_eq!(vslice(8, 2), SCALE * 4);
        assert_eq!(vslice(8, 1), SCALE * 8);
        assert_eq!(vslice(0, 4), SCALE / 4);
        // A malformed zero weight cannot divide by zero (fail closed).
        assert_eq!(vslice(1, 0), SCALE);
        assert_eq!(vslice(u64::MAX, 1), u64::MAX);
    }
}
