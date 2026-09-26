//! Proportional-share accounting the weighted policies share.
//!
//! CFQ and EEVDF divide a CPU between tasks in proportion to a per-band weight,
//! charged in the ticks each run actually consumed. What a band is worth, how a
//! run is charged, and where each task's weight is counted are defined once
//! here, so the two policies cannot drift apart on any of them.

use tairix_sync::SpinLock;

use crate::arch::CpuId;
use crate::task::{Priority, SchedClass};

/// The share a task of `priority` receives relative to the other bands: 4:2:1.
#[must_use]
pub const fn weight_of(priority: Priority) -> u64 {
    match priority {
        Priority::High => 4,
        Priority::Normal => 2,
        Priority::Low => 1,
    }
}

/// Fixed-point sub-units of virtual service per tick.
///
/// The least common multiple of every [`weight_of`], so a weighted charge is
/// exact. Virtual time counts raw [`crate::SchedulerArch::ticks_now`] units, so
/// the scale is kept this small: a multi-gigahertz counter scaled by `2^20`
/// saturates `u64` within hours of CPU time, where this lasts decades.
pub const SCALE: u64 = 4;

const _: () = {
    assert!(SCALE.is_multiple_of(weight_of(Priority::High)));
    assert!(SCALE.is_multiple_of(weight_of(Priority::Normal)));
    assert!(SCALE.is_multiple_of(weight_of(Priority::Low)));
};

/// The weighted virtual service `ticks` of execution cost a task of `weight`.
///
/// A zero-tick run is charged one tick, so a clock coarser than a dispatch
/// still makes progress. The quotient/remainder form cannot overflow the
/// product; a result beyond `u64` saturates.
#[must_use]
pub fn vslice(ticks: u64, weight: u64) -> u64 {
    let ticks = ticks.max(1);
    let weight = weight.max(1);
    (ticks / weight)
        .saturating_mul(SCALE)
        .saturating_add((ticks % weight).saturating_mul(SCALE) / weight)
}

/// Where one task's weight is counted, and how much of it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Counted {
    /// The CPU whose competition holds the weight.
    pub cpu: CpuId,
    /// The weight added there: exactly what comes off again.
    pub weight: u64,
    /// The band the task competes in, since only the time-shared band paces a
    /// virtual clock.
    pub class: SchedClass,
}

impl Counted {
    /// A task of `priority` competing on `cpu` in `class`, at that priority's
    /// band weight.
    #[must_use]
    pub const fn of(cpu: CpuId, priority: Priority, class: SchedClass) -> Self {
        Self {
            cpu,
            weight: weight_of(priority),
            class,
        }
    }
}

/// A policy's per-CPU competing-weight totals, as a [`WeightLedger`] moves them.
pub trait Competition {
    /// Add `counted.weight` to `counted.cpu`'s competition in `counted.class`.
    fn add(&self, counted: Counted);

    /// Take `counted.weight` off `counted.cpu`'s competition in `counted.class`.
    fn remove(&self, counted: Counted);
}

/// A task's record of where its weight is counted.
///
/// Every change to a CPU's competing weight goes through the task's ledger, so
/// what comes off is always what went on, whatever priority or class the task
/// has since taken and whichever CPU a steal or wake has since moved it to. The
/// lock orders a count against a departure: a count re-reads whether the task
/// still competes under it, and a departure performs its state transition
/// under it, so no interleaving leaves a parked task counted or counts one
/// twice.
///
/// Taken before any run-queue lock, never while one is held.
#[derive(Debug)]
pub struct WeightLedger {
    counted: SpinLock<Option<Counted>>,
}

impl Default for WeightLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl WeightLedger {
    /// A ledger for a task counted nowhere.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            counted: SpinLock::new(None),
        }
    }

    /// Count the task as `next`, moving it off wherever it was counted, if
    /// `competing` — the caller's test that the task is ready or running —
    /// still holds under the ledger lock.
    ///
    /// Returns whether the task is counted as `next`. A `false` means it left
    /// the competition after the caller chose to count it; whoever made it
    /// runnable again counts it then.
    pub fn count(
        &self,
        next: Counted,
        competing: impl FnOnce() -> bool,
        competition: &impl Competition,
    ) -> bool {
        let mut counted = self.counted.lock();
        if !competing() {
            return false;
        }
        if *counted == Some(next) {
            return true;
        }
        if let Some(previous) = counted.take() {
            competition.remove(previous);
        }
        competition.add(next);
        *counted = Some(next);
        true
    }

    /// Run `transition` — the task's move out of the ready set — and, when it
    /// reports the move happened, take the task's weight off wherever it was
    /// counted, both under the ledger lock.
    ///
    /// Returns what `transition` returned.
    pub fn depart(
        &self,
        transition: impl FnOnce() -> bool,
        competition: &impl Competition,
    ) -> bool {
        let mut counted = self.counted.lock();
        if !transition() {
            return false;
        }
        if let Some(previous) = counted.take() {
            competition.remove(previous);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use core::cell::RefCell;

    use alloc::vec::Vec;

    /// Records every change the ledger makes, in order.
    #[derive(Default)]
    struct Ledgered {
        changes: RefCell<Vec<(bool, Counted)>>,
    }

    impl Competition for Ledgered {
        fn add(&self, counted: Counted) {
            self.changes.borrow_mut().push((true, counted));
        }

        fn remove(&self, counted: Counted) {
            self.changes.borrow_mut().push((false, counted));
        }
    }

    fn on(cpu: CpuId, priority: Priority) -> Counted {
        Counted::of(cpu, priority, SchedClass::TimeShared)
    }

    #[test]
    fn the_band_weights_run_four_two_one() {
        assert_eq!(weight_of(Priority::High), 4);
        assert_eq!(weight_of(Priority::Normal), 2);
        assert_eq!(weight_of(Priority::Low), 1);
        assert_eq!(
            on(0, Priority::Normal).weight,
            2,
            "a count takes its band's weight"
        );
    }

    #[test]
    fn a_charge_is_exact_for_every_band_and_never_zero() {
        for priority in [Priority::High, Priority::Normal, Priority::Low] {
            let weight = weight_of(priority);
            assert_eq!(vslice(8, weight), 8 * SCALE / weight);
            assert!(vslice(0, weight) > 0, "a zero-tick run still costs");
        }
        assert_eq!(vslice(0, 1), SCALE, "zero ticks are charged one");
        assert_eq!(vslice(1, 0), SCALE, "a zero weight cannot divide by zero");
        assert_eq!(vslice(u64::MAX, 1), u64::MAX, "saturates, never wraps");
    }

    /// The scale is what keeps virtual time finite over a machine's life: a
    /// 5 GHz counter charged wholly to one weight-1 task for a decade — the
    /// fastest a timeline can grow — still fits.
    #[test]
    fn a_decade_of_a_fast_counter_does_not_saturate() {
        let decade_of_ticks = 5_000_000_000_u64 * 60 * 60 * 24 * 365 * 10;
        assert!(vslice(decade_of_ticks, 1) < u64::MAX);
    }

    #[test]
    fn what_comes_off_is_what_went_on() {
        let ledger = WeightLedger::new();
        let competition = Ledgered::default();
        assert!(ledger.count(on(0, Priority::Normal), || true, &competition));
        // A re-count at a new weight on another CPU moves the old one off first.
        assert!(ledger.count(on(3, Priority::Low), || true, &competition));
        assert!(ledger.depart(|| true, &competition));
        // Nothing is left to take off.
        assert!(ledger.depart(|| true, &competition));
        assert_eq!(
            *competition.changes.borrow(),
            [
                (true, on(0, Priority::Normal)),
                (false, on(0, Priority::Normal)),
                (true, on(3, Priority::Low)),
                (false, on(3, Priority::Low))
            ]
        );
    }

    #[test]
    fn a_repeated_count_changes_nothing() {
        let ledger = WeightLedger::new();
        let competition = Ledgered::default();
        assert!(ledger.count(on(1, Priority::High), || true, &competition));
        assert!(ledger.count(on(1, Priority::High), || true, &competition));
        assert_eq!(competition.changes.borrow().len(), 1);
    }

    #[test]
    fn a_task_that_stopped_competing_is_not_counted() {
        let ledger = WeightLedger::new();
        let competition = Ledgered::default();
        assert!(!ledger.count(on(0, Priority::Normal), || false, &competition));
        assert!(ledger.depart(|| true, &competition));
        assert!(
            competition.changes.borrow().is_empty(),
            "nothing went on, so nothing came off"
        );
    }

    #[test]
    fn a_departure_that_did_not_happen_keeps_the_count() {
        let ledger = WeightLedger::new();
        let competition = Ledgered::default();
        assert!(ledger.count(on(0, Priority::Normal), || true, &competition));
        assert!(!ledger.depart(|| false, &competition));
        assert_eq!(competition.changes.borrow().len(), 1, "still counted");
        assert!(ledger.depart(|| true, &competition));
        assert!(ledger.depart(|| true, &competition));
        assert_eq!(
            *competition.changes.borrow(),
            [
                (true, on(0, Priority::Normal)),
                (false, on(0, Priority::Normal))
            ],
            "the count held until a departure happened, then came off once"
        );
    }
}
