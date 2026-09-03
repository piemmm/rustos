//! Host tests for the cross-CPU quiesce coordinator.
//!
//! The bounded peer-selection and ack-wait logic is exercised over plain
//! local slices through the pure [`super::poke_others`] / [`super::wait_counted`]
//! helpers, so the all-acknowledged, partial, and timed-out paths are all
//! deterministic and fast (a tiny budget, no dependence on the process-global
//! published tables). A single lifecycle test covers the set-once
//! [`super::publish_tables`] and the [`super::acknowledge`] /
//! [`super::stop_requested`] statics, because those are genuinely
//! process-global and cannot be re-published per test.

extern crate std;

use super::{
    acknowledge, poke_others, publish_tables, stop_requested, wait_counted, PublishError,
    StopOutcome,
};
use crate::CpuId;
use core::sync::atomic::{AtomicBool, Ordering};
use std::vec::Vec;

/// A liveness/ack table of `n` `AtomicBool`s, all `false`.
fn table(n: usize) -> Vec<AtomicBool> {
    (0..n).map(|_| AtomicBool::new(false)).collect()
}

#[test]
fn poke_others_pokes_every_online_peer_but_never_the_current_cpu() {
    // Four CPUs; 0 (current) and 3 online, 2 online, 1 offline.
    let online = table(4);
    online[0].store(true, Ordering::Relaxed);
    online[2].store(true, Ordering::Relaxed);
    online[3].store(true, Ordering::Relaxed);
    let mut poked: Vec<CpuId> = Vec::new();
    poke_others(0, &online, |cpu| poked.push(cpu));
    // The current CPU (0) is never poked; the offline CPU (1) is never poked.
    assert_eq!(poked, [2, 3]);
}

#[test]
fn wait_counted_reports_every_online_peer_stopped_once_all_have_acknowledged() {
    let online = table(3);
    for slot in &online {
        slot.store(true, Ordering::Relaxed);
    }
    let ack = table(3);
    // Peers 1 and 2 acknowledge (0 is current, need not ack).
    ack[1].store(true, Ordering::Relaxed);
    ack[2].store(true, Ordering::Relaxed);
    assert_eq!(
        wait_counted(0, &online, &ack, 8),
        StopOutcome {
            asked: 2,
            stopped: 2,
            unresponsive: None
        }
    );
}

#[test]
fn wait_counted_ignores_offline_peers() {
    let online = table(3);
    online[0].store(true, Ordering::Relaxed);
    online[1].store(true, Ordering::Relaxed);
    // CPU 2 is offline and never acknowledges; it must not hold the wait up.
    let ack = table(3);
    ack[1].store(true, Ordering::Relaxed);
    // Only the one online peer is asked, so an offline CPU cannot hold the
    // wait up or inflate the count.
    assert_eq!(
        wait_counted(0, &online, &ack, 8),
        StopOutcome {
            asked: 1,
            stopped: 1,
            unresponsive: None
        }
    );
}

#[test]
fn wait_counted_names_the_stuck_peer_and_counts_the_one_that_stopped() {
    let online = table(3);
    for slot in &online {
        slot.store(true, Ordering::Relaxed);
    }
    let ack = table(3);
    // Peer 1 acknowledges; peer 2 never does, so a bounded wait times out and
    // names peer 2 (fail closed).
    ack[1].store(true, Ordering::Relaxed);
    // The budget elapses; the partial result is what a fatal report carries,
    // and peer 2 is named — a peer that cannot answer a pending IPI is wedged
    // with interrupts masked.
    assert_eq!(
        wait_counted(0, &online, &ack, 4),
        StopOutcome {
            asked: 2,
            stopped: 1,
            unresponsive: Some(2)
        }
    );
}

#[test]
fn wait_counted_with_no_peers_reports_nothing_asked_immediately() {
    // A single online CPU that is itself the current CPU: nothing to wait for.
    let online = table(1);
    online[0].store(true, Ordering::Relaxed);
    let ack = table(1);
    assert_eq!(wait_counted(0, &online, &ack, 1), StopOutcome::NONE);
}

/// The process-global statics: publish is set-once, length is validated,
/// `acknowledge` sets exactly the caller's slot, and a latched request exempts
/// its own requester.
///
/// One test owns the whole lifecycle because the publish slot is genuinely
/// set-once per process (mirrors the SMP hand-off's single-lifecycle test).
#[test]
fn published_tables_are_set_once_and_a_latched_stop_exempts_its_requester() {
    // A mismatched publish is refused *before* the set-once guard is consumed.
    let short: &'static [AtomicBool] = Vec::leak(table(2));
    let long: &'static [AtomicBool] = Vec::leak(table(3));
    assert_eq!(
        publish_tables(short, long),
        Err(PublishError::LengthMismatch)
    );

    // The first well-formed publish succeeds; a second is refused.
    let online: &'static [AtomicBool] = Vec::leak(table(4));
    let ack: &'static [AtomicBool] = Vec::leak(table(4));
    assert_eq!(publish_tables(online, ack), Ok(()));
    let online2: &'static [AtomicBool] = Vec::leak(table(4));
    let ack2: &'static [AtomicBool] = Vec::leak(table(4));
    assert_eq!(
        publish_tables(online2, ack2),
        Err(PublishError::AlreadyPublished)
    );

    // `acknowledge(cpu)` sets exactly `cpu`'s slot in the published ack table.
    assert!(!ack[2].load(Ordering::Acquire));
    acknowledge(2);
    assert!(ack[2].load(Ordering::Acquire));
    assert!(
        !ack[1].load(Ordering::Acquire),
        "only the caller's slot is set"
    );

    // An out-of-range id is a no-op, never an out-of-bounds write.
    acknowledge(CpuId::MAX);

    // No initiator has run yet, so nothing is latched and no CPU has been
    // asked to stop.
    assert!(!stop_requested(0));
    assert!(!stop_requested(2));

    // Latch a stop from CPU 1 through the best-effort initiator. The tables
    // published above have no CPU marked online, so nothing is poked and the
    // wait returns immediately.
    let outcome = super::stop_others_best_effort(1, |_| unreachable!("no peer is online"));
    assert_eq!(outcome, StopOutcome::NONE);

    // The requester is exempt from its own request: a late poke arriving back
    // at it must not park the very core writing the record of its death.
    assert!(
        !stop_requested(1),
        "the requester must reach its own halt, not be parked by its own poke"
    );
    assert!(stop_requested(0), "every other CPU must stop");
    assert!(stop_requested(2));
}
