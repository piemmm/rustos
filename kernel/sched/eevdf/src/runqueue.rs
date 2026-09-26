//! Per-CPU virtual-time run queue for the EEVDF policy.
//!
//! Each CPU owns one [`RunQueue`]: its ready set, its virtual clock `V`, and
//! the weight competing for it, under one `SpinLock`.
//!
//! EEVDF runs the earliest virtual deadline among the tasks `V` has made
//! eligible. The ready set is two binary heaps: *pending*, keyed by eligible
//! time, and *eligible*, keyed by deadline. `V` never moves backwards, so an
//! entry crosses from pending to eligible at most once per enqueue and every
//! pick is `O(log n)` amortised; nothing on the dispatch path scans the set.
//! No entry is ever removed from the middle — a stale one is discarded when it
//! is picked — so a heap is all the set needs. Both heaps keep room for every
//! entry, reserved fallibly on push, so a promotion never allocates. Equal keys
//! break on arrival order: task ids are drawn at random, and letting one decide
//! who runs first would hand the choice to the draw.
//!
//! Virtual time is fixed-point in the shared [`SCALE`] sub-units per tick of
//! service.

use alloc::collections::{BinaryHeap, VecDeque};
use core::cmp::Reverse;

use tairix_kernel_sched_api::share::SCALE;
use tairix_kernel_sched_api::SchedClass;
use tairix_sync::SpinLock;

use crate::TaskId;

/// A time-shared task as the scheduler queues it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) struct Entry {
    /// The ready task.
    pub id: TaskId,
    /// Virtual eligible time `ve`: the task may run once `V` reaches it.
    pub eligible: u64,
    /// Virtual deadline `vd`: the earliest among eligible entries runs.
    pub deadline: u64,
}

/// An eligible entry, ordered `(deadline, arrival)`.
type EligibleKey = Reverse<(u64, u64, TaskId)>;

/// A pending entry, ordered `(eligible, deadline, arrival)`.
type PendingKey = Reverse<(u64, u64, u64, TaskId)>;

/// Mutable interior of a [`RunQueue`], guarded by one `SpinLock`.
struct Inner {
    /// Strict-priority real-time band, in FIFO arrival order, dispatched (and
    /// stolen) before any time-shared entry whatever its deadline — the
    /// [`SchedClass::Realtime`] guarantee. A task re-enqueued after a yield
    /// goes to the back, so equal real-time peers share the CPU round-robin.
    /// Real-time tasks carry no virtual time.
    rt_ready: VecDeque<TaskId>,
    /// Time-shared entries `V` has reached: the pick set.
    eligible: BinaryHeap<EligibleKey>,
    /// Time-shared entries `V` has not reached yet.
    pending: BinaryHeap<PendingKey>,
    /// Arrival counter supplying the tie-break half of a key. One increment
    /// per enqueue, so it cannot wrap in any real uptime.
    next_seq: u64,
    /// The CPU's virtual time `V`, advanced by time-shared service divided by
    /// [`Self::fair_weight`] and never backwards.
    virtual_time: u64,
    /// Service owed to `V` that has not yet made up a whole unit of it, kept
    /// so short dispatches under a heavy load still move the clock exactly.
    carry: u64,
    /// Weight of every task counted on this CPU, whatever its class: the
    /// load placement balances.
    total_weight: u64,
    /// Weight of the time-shared tasks alone, which paces `V`: real-time
    /// service is not delivered to the fair competition.
    fair_weight: u64,
    /// Bound on the time-shared entries — back-pressure, never growth past it.
    capacity: usize,
}

impl Inner {
    fn fair_len(&self) -> usize {
        self.eligible.len() + self.pending.len()
    }

    /// Move every pending entry `V` has reached into the eligible heap.
    fn promote(&mut self) {
        while let Some(&Reverse((eligible, deadline, seq, id))) = self.pending.peek() {
            if eligible > self.virtual_time {
                break;
            }
            self.pending.pop();
            // Room for every entry is reserved on push, so this never grows.
            self.eligible.push(Reverse((deadline, seq, id)));
        }
    }

    /// The next time-shared entry by the EEVDF rule, or `None` when there is
    /// none: the earliest deadline among eligible entries, else the earliest
    /// eligible entry, ties on its deadline.
    fn take_fair(&mut self) -> Option<(TaskId, u64)> {
        self.promote();
        if let Some(Reverse((_, _, id))) = self.eligible.pop() {
            return Some((id, self.virtual_time));
        }
        let Reverse((eligible, _, _, id)) = self.pending.pop()?;
        Some((id, eligible))
    }
}

/// Per-CPU EEVDF run queue.
pub(crate) struct RunQueue {
    inner: SpinLock<Inner>,
}

impl RunQueue {
    /// Construct an empty queue bounded to `capacity` time-shared entries.
    ///
    /// Returns `None` if `capacity` is not a power of two `>= 2`, the
    /// queue-capacity contract every policy accepts the same
    /// [`crate::SchedulerConfig`] under.
    pub(crate) fn try_new(capacity: usize) -> Option<Self> {
        if capacity < 2 || !capacity.is_power_of_two() {
            return None;
        }
        Some(Self {
            inner: SpinLock::new(Inner {
                rt_ready: VecDeque::new(),
                eligible: BinaryHeap::new(),
                pending: BinaryHeap::new(),
                next_seq: 0,
                virtual_time: 0,
                carry: 0,
                total_weight: 0,
                fair_weight: 0,
                capacity,
            }),
        })
    }

    /// This CPU's virtual time `V`: where a task joining its competition is
    /// placed, with zero lag.
    pub(crate) fn virtual_time(&self) -> u64 {
        self.inner.lock().virtual_time
    }

    /// Add `weight` in `class` to this CPU's competition. Only a task's weight
    /// ledger calls this, so what is added is exactly what it later removes.
    pub(crate) fn add_weight(&self, weight: u64, class: SchedClass) {
        let mut g = self.inner.lock();
        g.total_weight = g.total_weight.saturating_add(weight);
        if !class.is_realtime() {
            g.fair_weight = g.fair_weight.saturating_add(weight);
        }
    }

    /// Take `weight` in `class` off this CPU's competition, the counterpart of
    /// [`Self::add_weight`].
    pub(crate) fn remove_weight(&self, weight: u64, class: SchedClass) {
        let mut g = self.inner.lock();
        g.total_weight = g.total_weight.saturating_sub(weight);
        if !class.is_realtime() {
            g.fair_weight = g.fair_weight.saturating_sub(weight);
        }
    }

    /// Total weight competing on this CPU (`0` when it is idle). The
    /// placement path reads it to put new and woken work on the least-loaded
    /// eligible CPU.
    pub(crate) fn competing_weight(&self) -> u64 {
        self.inner.lock().total_weight
    }

    /// Number of entries queued on this CPU, in either band.
    ///
    /// The task a CPU is *running* is held in the scheduler's current-task
    /// slot, not in this queue, so a non-zero count means at least one
    /// **other** ready task is waiting — a competitor the running task must
    /// be preempted for. The tickless preemption decision reads this to arm
    /// the one-shot timer only when a CPU is contended.
    pub(crate) fn ready_len(&self) -> usize {
        let g = self.inner.lock();
        g.rt_ready.len() + g.fair_len()
    }

    /// Queue a time-shared entry. Returns `Err(id)` when the queue is at its
    /// bound or its storage cannot grow, and the caller routes it to overflow.
    pub(crate) fn push(&self, entry: Entry) -> Result<(), TaskId> {
        let mut g = self.inner.lock();
        let entries = g.fair_len() + 1;
        if entries > g.capacity {
            return Err(entry.id);
        }
        // Each heap keeps room for every entry, so moving one between them
        // never has to allocate.
        let eligible_room = entries.saturating_sub(g.eligible.len());
        let pending_room = entries.saturating_sub(g.pending.len());
        if g.eligible.try_reserve(eligible_room).is_err()
            || g.pending.try_reserve(pending_room).is_err()
        {
            return Err(entry.id);
        }
        let seq = g.next_seq;
        g.next_seq = g.next_seq.wrapping_add(1);
        if entry.eligible <= g.virtual_time {
            g.eligible.push(Reverse((entry.deadline, seq, entry.id)));
        } else {
            g.pending
                .push(Reverse((entry.eligible, entry.deadline, seq, entry.id)));
        }
        Ok(())
    }

    /// Push a real-time task onto the back of the strict-priority band (FIFO
    /// / round-robin). Returns `Err(id)` when the band is at its bound or
    /// cannot grow, exactly like [`Self::push`].
    pub(crate) fn push_rt(&self, id: TaskId) -> Result<(), TaskId> {
        let mut g = self.inner.lock();
        if g.rt_ready.len() >= g.capacity || g.rt_ready.try_reserve(1).is_err() {
            return Err(id);
        }
        g.rt_ready.push_back(id);
        Ok(())
    }

    /// Pick and remove the next task to run here, with the band it came from.
    ///
    /// A ready real-time task comes first. Otherwise the earliest-deadline
    /// eligible entry runs; when none is eligible — which an integer virtual
    /// clock can transiently produce — the earliest-eligible entry runs and
    /// `V` is advanced to it, so a CPU holding runnable work never idles
    /// waiting for virtual time to pass.
    pub(crate) fn pick(&self) -> Option<(TaskId, SchedClass)> {
        let mut g = self.inner.lock();
        if let Some(id) = g.rt_ready.pop_front() {
            return Some((id, SchedClass::Realtime));
        }
        let (id, reached) = g.take_fair()?;
        if reached > g.virtual_time {
            g.virtual_time = reached;
        }
        Some((id, SchedClass::TimeShared))
    }

    /// Remove the task this CPU would run next, for another CPU to run.
    ///
    /// The same rule as [`Self::pick`], without advancing this CPU's clock:
    /// the task will run against the stealing CPU's. Weight bookkeeping is the
    /// caller's, as the task changes CPU.
    pub(crate) fn steal(&self) -> Option<(TaskId, SchedClass)> {
        let mut g = self.inner.lock();
        if let Some(id) = g.rt_ready.pop_front() {
            return Some((id, SchedClass::Realtime));
        }
        let (id, _) = g.take_fair()?;
        Some((id, SchedClass::TimeShared))
    }

    /// Advance `V` by `ticks` of time-shared service apportioned over the
    /// time-shared weight competing here.
    ///
    /// A zero-tick run is one tick, as its charge is. With no time-shared
    /// weight there is nothing to apportion the service over, so it is dropped.
    pub(crate) fn advance(&self, ticks: u64) {
        let mut g = self.inner.lock();
        let Some(weight) = core::num::NonZeroU64::new(g.fair_weight) else {
            g.carry = 0;
            return;
        };
        let owed = ticks.max(1).saturating_mul(SCALE).saturating_add(g.carry);
        g.virtual_time = g.virtual_time.saturating_add(owed / weight);
        g.carry = owed % weight;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloc::vec::Vec;

    use tairix_fuzzseed::Prng;

    const FAIR: SchedClass = SchedClass::TimeShared;

    fn e(id: TaskId, eligible: u64, deadline: u64) -> Entry {
        Entry {
            id,
            eligible,
            deadline,
        }
    }

    fn picked(q: &RunQueue) -> Option<TaskId> {
        q.pick().map(|(id, _)| id)
    }

    #[test]
    fn rejects_bad_capacity() {
        assert!(RunQueue::try_new(0).is_none());
        assert!(RunQueue::try_new(1).is_none());
        assert!(RunQueue::try_new(3).is_none());
        assert!(RunQueue::try_new(4).is_some());
    }

    #[test]
    fn push_is_bounded_across_both_heaps() {
        let q = RunQueue::try_new(2).expect("q");
        assert!(q.push(e(1, 0, 10)).is_ok(), "eligible");
        assert!(q.push(e(2, 50, 60)).is_ok(), "pending");
        assert_eq!(q.push(e(3, 0, 30)), Err(3));
    }

    #[test]
    fn pick_prefers_earliest_eligible_deadline() {
        let q = RunQueue::try_new(8).expect("q");
        q.push(e(1, 0, 30)).expect("push");
        q.push(e(2, 0, 10)).expect("push");
        q.push(e(3, 0, 20)).expect("push");
        assert_eq!(picked(&q), Some(2));
        assert_eq!(picked(&q), Some(3));
        assert_eq!(picked(&q), Some(1));
        assert_eq!(picked(&q), None);
    }

    /// An ineligible entry never runs ahead of an eligible one, however
    /// early its deadline: that is what bounds a task's lead over its share.
    #[test]
    fn an_ineligible_entry_waits_behind_every_eligible_one() {
        let q = RunQueue::try_new(8).expect("q");
        q.push(e(1, 100, 101)).expect("push");
        q.push(e(2, 0, 500)).expect("push");
        assert_eq!(picked(&q), Some(2));
        assert_eq!(picked(&q), Some(1), "then it runs, V fast-forwarded");
        assert_eq!(q.virtual_time(), 100);
    }

    #[test]
    fn ineligible_entry_fast_forwards_virtual_time() {
        let q = RunQueue::try_new(4).expect("q");
        q.push(e(7, 100, 200)).expect("push");
        assert_eq!(picked(&q), Some(7));
        assert_eq!(q.virtual_time(), 100);
    }

    /// Entries that become eligible together at a fast-forward run by
    /// deadline, not by which was queued first.
    #[test]
    fn a_fast_forward_picks_the_earliest_deadline_at_the_new_time() {
        let q = RunQueue::try_new(8).expect("q");
        q.push(e(1, 100, 300)).expect("push");
        q.push(e(2, 100, 200)).expect("push");
        assert_eq!(picked(&q), Some(2));
        assert_eq!(picked(&q), Some(1));
    }

    #[test]
    fn equal_deadlines_run_in_arrival_order() {
        let q = RunQueue::try_new(8).expect("q");
        for id in [9, 2, 5] {
            q.push(e(id, 0, 7)).expect("push");
        }
        assert_eq!(picked(&q), Some(9), "first in, first picked");
        assert_eq!(picked(&q), Some(2));
        assert_eq!(picked(&q), Some(5));
    }

    #[test]
    fn a_realtime_task_runs_ahead_of_any_deadline() {
        let q = RunQueue::try_new(8).expect("q");
        q.push(e(1, 0, 1)).expect("push");
        q.push_rt(2).expect("push rt");
        assert_eq!(q.pick(), Some((2, SchedClass::Realtime)));
        assert_eq!(q.pick(), Some((1, FAIR)));
    }

    #[test]
    fn steal_takes_the_next_pick_without_moving_the_clock() {
        let q = RunQueue::try_new(8).expect("q");
        q.push(e(1, 100, 150)).expect("push");
        q.push(e(2, 40, 400)).expect("push");
        assert_eq!(q.steal(), Some((2, FAIR)), "the earliest eligible time");
        assert_eq!(q.virtual_time(), 0, "the victim's clock is its own");
        assert_eq!(q.steal(), Some((1, FAIR)));
        assert_eq!(q.steal(), None);
    }

    #[test]
    fn advance_scales_by_the_fair_weight() {
        let q = RunQueue::try_new(4).expect("q");
        q.add_weight(2, FAIR);
        q.advance(3);
        assert_eq!(q.virtual_time(), 3 * SCALE / 2);
    }

    #[test]
    fn realtime_weight_does_not_slow_the_fair_clock() {
        let q = RunQueue::try_new(4).expect("q");
        q.add_weight(2, FAIR);
        q.add_weight(4, SchedClass::Realtime);
        assert_eq!(q.competing_weight(), 6, "placement still counts it");
        q.advance(3);
        assert_eq!(q.virtual_time(), 3 * SCALE / 2);
    }

    #[test]
    fn advance_without_fair_weight_is_a_noop() {
        let q = RunQueue::try_new(4).expect("q");
        q.add_weight(1, SchedClass::Realtime);
        q.advance(5);
        assert_eq!(q.virtual_time(), 0);
    }

    /// Service too small to move `V` a whole unit under a heavy load is
    /// carried, not lost: a thousand one-tick runs over a weight the scale
    /// does not divide still add up exactly.
    #[test]
    fn short_runs_under_a_heavy_load_move_the_clock_exactly() {
        let q = RunQueue::try_new(4).expect("q");
        q.add_weight(1_000, FAIR);
        for _ in 0..1_000 {
            q.advance(1);
        }
        assert_eq!(q.virtual_time(), SCALE);
    }

    fn draw(rng: &mut Prng, below: usize) -> u64 {
        u64::try_from(rng.below(below)).expect("a draw below a small bound fits")
    }

    /// The heaps against the rule they implement, stated as the linear scan
    /// the queue replaced: over a long random mix of pushes, picks and clock
    /// advances, every pick must be exactly the one the scan chooses.
    #[test]
    fn every_pick_matches_the_earliest_eligible_deadline_scan() {
        let mut rng = Prng::new(0x0EE7_DF01);
        let q = RunQueue::try_new(1 << 12).expect("q");
        q.add_weight(3, FAIR);
        // (eligible, deadline, arrival, id)
        let mut model: Vec<(u64, u64, u64, TaskId)> = Vec::new();
        let mut model_v = 0u64;
        let mut model_carry = 0u64;
        let mut arrival = 0u64;
        let mut next_id: TaskId = 1;
        for _ in 0..20_000 {
            match rng.below(3) {
                0 if model.len() < 1 << 12 => {
                    let ahead = draw(&mut rng, 64);
                    let behind = draw(&mut rng, 64).min(model_v);
                    let eligible = model_v + ahead - behind;
                    let deadline = eligible + 1 + draw(&mut rng, 64);
                    q.push(e(next_id, eligible, deadline)).expect("room");
                    model.push((eligible, deadline, arrival, next_id));
                    arrival += 1;
                    next_id += 1;
                }
                1 => {
                    let ticks = draw(&mut rng, 8);
                    q.advance(ticks);
                    let owed = ticks.max(1) * SCALE + model_carry;
                    model_v += owed / 3;
                    model_carry = owed % 3;
                }
                _ => {
                    let expected = model
                        .iter()
                        .enumerate()
                        .filter(|(_, x)| x.0 <= model_v)
                        .min_by_key(|(_, x)| (x.1, x.2))
                        .or_else(|| {
                            model
                                .iter()
                                .enumerate()
                                .min_by_key(|(_, x)| (x.0, x.1, x.2))
                        })
                        .map(|(i, _)| i);
                    let want = expected.map(|i| model.remove(i));
                    if let Some(entry) = want {
                        model_v = model_v.max(entry.0);
                    }
                    assert_eq!(picked(&q), want.map(|x| x.3));
                    assert_eq!(q.virtual_time(), model_v);
                }
            }
        }
    }
}
