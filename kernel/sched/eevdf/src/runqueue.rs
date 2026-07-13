//! Per-CPU virtual-time run queue for the EEVDF policy.
//!
//! Each CPU owns one [`RunQueue`]: a bounded set of [`Entry`] records
//! (a [`TaskId`] with its current virtual deadline / eligible time)
//! kept under a single `SpinLock`, plus that CPU's monotonically
//! advancing virtual time and the total weight of the tasks it owns.
//!
//! The queue is *not* a priority array like the MLFQ sibling's
//! Chase–Lev deques: EEVDF orders by a continuous virtual deadline, so
//! the ready set is scanned for the earliest-eligible-virtual-deadline
//! task on each pick. The scan is `O(n)` in the per-CPU ready count,
//! which is the textbook EEVDF selection rule; a future tree-backed
//! index can replace it behind this same module boundary without
//! changing the scheduler (no interface creep).
//!
//! Virtual time is fixed-point with [`SCALE`] sub-units per unit of
//! service so the weighted divisions stay in integer arithmetic
//! (no floats in kernel paths, deterministic).

use alloc::vec::Vec;

use rustos_sync::SpinLock;

use crate::TaskId;

/// Fixed-point scaling factor for virtual time.
///
/// One unit of dispatched service is worth `SCALE` virtual sub-units
/// before the per-task weight division, keeping `service * SCALE /
/// weight` an exact integer for the small integer weights this policy
/// uses.
pub(crate) const SCALE: u64 = 1 << 20;

/// Service charged for a single dispatch (one body invocation).
///
/// The cooperative dispatch model runs a task body exactly once per
/// [`crate::Scheduler::step`]; that counts as one unit of service for
/// virtual-time accounting. It is deliberately the request size too, so
/// a task's virtual deadline after admission is `ve + SCALE/weight`.
pub(crate) const SERVICE_PER_DISPATCH: u64 = 1;

/// A ready task as tracked by a [`RunQueue`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) struct Entry {
    /// The ready task.
    pub id: TaskId,
    /// Virtual eligible time `ve` (fixed point). The task may be
    /// dispatched only once the owning CPU's virtual time reaches `ve`.
    pub eligible: u64,
    /// Virtual deadline `vd` (fixed point). The earliest deadline among
    /// eligible entries wins the pick.
    pub deadline: u64,
}

/// Mutable interior of a [`RunQueue`], guarded by one `SpinLock`.
struct Inner {
    ready: Vec<Entry>,
    /// The CPU's current virtual time `V` (fixed point). Advances by
    /// `service * SCALE / total_weight` as the CPU dispatches work.
    virtual_time: u64,
    /// Sum of the weights of every task this CPU currently owns
    /// (whether queued here or running off it). Drives the rate at
    /// which `virtual_time` advances so the share is proportional.
    total_weight: u64,
    /// Compile-time bound on `ready.len()` — back-pressure, never a
    /// reallocation past this point (bounded queues).
    capacity: usize,
}

/// Per-CPU EEVDF run queue.
pub(crate) struct RunQueue {
    inner: SpinLock<Inner>,
}

impl RunQueue {
    /// Construct an empty queue bounded to `capacity` ready entries.
    ///
    /// Returns `None` if `capacity` is not a power of two `>= 2`,
    /// mirroring the MLFQ sibling's queue-capacity contract so the two
    /// policies accept the same [`crate::SchedulerConfig`].
    pub(crate) fn try_new(capacity: usize) -> Option<Self> {
        if capacity < 2 || !capacity.is_power_of_two() {
            return None;
        }
        Some(Self {
            inner: SpinLock::new(Inner {
                ready: Vec::new(),
                virtual_time: 0,
                total_weight: 0,
                capacity,
            }),
        })
    }

    /// Current virtual time `V` of this CPU. Used by the crate's tests to
    /// assert the EEVDF clock advances proportionally to weight.
    #[cfg(test)]
    pub(crate) fn virtual_time(&self) -> u64 {
        self.inner.lock().virtual_time
    }

    /// Account a task joining this CPU's competition: add its `weight`
    /// and return the virtual eligible time it should adopt (the current
    /// `V`, giving it zero initial lag — the EEVDF admission rule).
    pub(crate) fn admit_weight(&self, weight: u64) -> u64 {
        let mut g = self.inner.lock();
        g.total_weight = g.total_weight.saturating_add(weight);
        g.virtual_time
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
    /// **other** ready task is waiting — a competitor the running task
    /// must be preempted for. The tickless preemption decision
    /// (`crate::Scheduler::dispatch`) reads this to arm the one-shot timer
    /// only when a CPU is contended.
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
        g.ready.push(entry);
        Ok(())
    }

    /// Pick and remove the earliest-eligible-virtual-deadline task.
    ///
    /// Among entries whose `eligible <= V` the smallest `deadline` wins
    /// (ties broken by the smaller [`TaskId`] for determinism). If no
    /// entry is eligible yet — which the integer virtual clock can
    /// transiently produce — the earliest `eligible` entry is taken and
    /// `V` is advanced to it so the CPU always makes progress when it
    /// owns runnable work (no spin-waiting for time
    /// to pass).
    pub(crate) fn pick(&self) -> Option<Entry> {
        let mut g = self.inner.lock();
        if g.ready.is_empty() {
            return None;
        }
        let v = g.virtual_time;
        // The earliest-deadline entry whose eligible time has arrived.
        let eligible_best = g
            .ready
            .iter()
            .enumerate()
            .filter(|(_, e)| e.eligible <= v)
            .min_by_key(|(_, e)| (e.deadline, e.id))
            .map(|(i, _)| i);
        let idx = if let Some(i) = eligible_best {
            i
        } else {
            // Nothing eligible yet: take the earliest-eligible entry and
            // fast-forward V to it so the CPU never idles while holding
            // runnable work (no spin-waiting for time).
            let earliest = g
                .ready
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| (e.eligible, e.id))
                .map_or(0, |(i, _)| i);
            let ve = g.ready[earliest].eligible;
            if ve > g.virtual_time {
                g.virtual_time = ve;
            }
            earliest
        };
        Some(g.ready.swap_remove(idx))
    }

    /// Advance this CPU's virtual time by one dispatch of `service`
    /// units against `total_weight`. A queue with no competing weight
    /// leaves `V` unchanged (there is nothing to apportion).
    pub(crate) fn advance(&self, service: u64) {
        let mut g = self.inner.lock();
        if g.total_weight == 0 {
            return;
        }
        let delta = service.saturating_mul(SCALE) / g.total_weight;
        g.virtual_time = g.virtual_time.saturating_add(delta);
    }

    /// Steal the earliest-deadline ready entry for another CPU, if any.
    /// Used by the work-stealing path; weight bookkeeping is settled by
    /// the caller as the task changes owning CPU.
    pub(crate) fn steal(&self) -> Option<Entry> {
        let mut g = self.inner.lock();
        if g.ready.is_empty() {
            return None;
        }
        let mut best = 0usize;
        for (i, e) in g.ready.iter().enumerate() {
            let cur = g.ready[best];
            if (e.deadline, e.id) < (cur.deadline, cur.id) {
                best = i;
            }
        }
        Some(g.ready.swap_remove(best))
    }

    /// Drop the weight of a stolen task from this queue's competition.
    pub(crate) fn release_weight(&self, weight: u64) {
        self.remove_weight(weight);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(id: TaskId, eligible: u64, deadline: u64) -> Entry {
        Entry {
            id,
            eligible,
            deadline,
        }
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
        assert!(q.push(e(1, 0, 10)).is_ok());
        assert!(q.push(e(2, 0, 20)).is_ok());
        assert_eq!(q.push(e(3, 0, 30)), Err(3));
    }

    #[test]
    fn pick_prefers_earliest_eligible_deadline() {
        let q = RunQueue::try_new(8).expect("q");
        q.push(e(1, 0, 30)).expect("push");
        q.push(e(2, 0, 10)).expect("push");
        q.push(e(3, 0, 20)).expect("push");
        assert_eq!(q.pick().map(|x| x.id), Some(2));
        assert_eq!(q.pick().map(|x| x.id), Some(3));
        assert_eq!(q.pick().map(|x| x.id), Some(1));
        assert_eq!(q.pick(), None);
    }

    #[test]
    fn ineligible_entry_fast_forwards_virtual_time() {
        let q = RunQueue::try_new(4).expect("q");
        // Only entry is not yet eligible; pick must still return it and
        // advance V to its eligible time.
        q.push(e(7, 100, 200)).expect("push");
        let picked = q.pick().expect("pick");
        assert_eq!(picked.id, 7);
        assert_eq!(q.virtual_time(), 100);
    }

    #[test]
    fn advance_scales_by_weight() {
        let q = RunQueue::try_new(4).expect("q");
        q.admit_weight(2);
        q.advance(SERVICE_PER_DISPATCH);
        assert_eq!(q.virtual_time(), SCALE / 2);
    }

    #[test]
    fn advance_without_weight_is_noop() {
        let q = RunQueue::try_new(4).expect("q");
        q.advance(SERVICE_PER_DISPATCH);
        assert_eq!(q.virtual_time(), 0);
    }
}
