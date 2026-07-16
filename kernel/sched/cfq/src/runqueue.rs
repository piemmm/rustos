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

use alloc::collections::BTreeSet;

use rustos_sync::SpinLock;

use crate::TaskId;

/// Fixed-point scaling factor for virtual runtime.
///
/// One unit of dispatched service is worth `SCALE` virtual sub-units
/// before the per-task weight division, keeping `service * SCALE /
/// weight` an exact integer for the small integer weights this policy
/// uses.
pub(crate) const SCALE: u64 = 1 << 20;

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
    /// Ready set keyed by `(vruntime, id)` so the leftmost element is the
    /// smallest-vruntime task (ties broken by the smaller id for
    /// determinism — no flaky ordering). The CFS red-black tree analog.
    ready: BTreeSet<(u64, TaskId)>,
    /// Monotonic floor tracking the front of the CPU's timeline. A task
    /// joining or re-joining this CPU adopts this value as its vruntime
    /// so a task that slept at a low vruntime cannot leap ahead of the
    /// running population and monopolise the CPU (the CFS
    /// `place_entity` / min-vruntime rule).
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
                ready: BTreeSet::new(),
                min_vruntime: 0,
                total_weight: 0,
                capacity,
            }),
        })
    }

    /// Account a task joining this CPU's competition: add its `weight`
    /// and return the vruntime it should adopt — the front of this CPU's
    /// timeline (the smaller of the stored floor and the smallest ready
    /// entry, never below the monotonic floor), so a joiner gets no unfair
    /// head start and a task that slept at a low vruntime cannot leap the
    /// running population (the CFS `place_entity` rule).
    pub(crate) fn admit_weight(&self, weight: u64) -> u64 {
        let mut g = self.inner.lock();
        g.total_weight = g.total_weight.saturating_add(weight);
        g.ready
            .iter()
            .next()
            .map_or(g.min_vruntime, |&(v, _)| v.max(g.min_vruntime))
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
        self.inner.lock().ready.len()
    }

    /// Push a ready entry. Returns `Err(id)` if the queue is at its
    /// compile-time bound (the caller then routes it to overflow).
    pub(crate) fn push(&self, entry: Entry) -> Result<(), TaskId> {
        let mut g = self.inner.lock();
        if g.ready.len() >= g.capacity {
            return Err(entry.id);
        }
        g.ready.insert((entry.vruntime, entry.id));
        Ok(())
    }

    /// Pick and remove the smallest-vruntime ready task (the leftmost
    /// entity), advancing the monotonic floor to it so the timeline never
    /// runs backwards.
    pub(crate) fn pick(&self) -> Option<Entry> {
        let mut g = self.inner.lock();
        let &(vruntime, id) = g.ready.iter().next()?;
        g.ready.remove(&(vruntime, id));
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
        let &(vruntime, id) = g.ready.iter().next()?;
        g.ready.remove(&(vruntime, id));
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

    #[test]
    fn pick_ties_break_on_id() {
        let q = RunQueue::try_new(8).expect("q");
        q.push(e(5, 7)).expect("push");
        q.push(e(2, 7)).expect("push");
        assert_eq!(q.pick().map(|x| x.id), Some(2), "smaller id wins a tie");
    }

    #[test]
    fn floor_is_monotonic_and_places_joiners_at_the_front() {
        let q = RunQueue::try_new(8).expect("q");
        q.push(e(1, 100)).expect("push");
        // Picking 100 advances the floor to 100.
        assert_eq!(q.pick().map(|x| x.vruntime), Some(100));
        // A joiner adopts the front (100), never a stale zero — the
        // monotonic floor advanced when 100 was dispatched.
        assert_eq!(q.admit_weight(1), 100);
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
