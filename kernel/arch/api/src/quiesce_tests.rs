//! Host tests for the cross-CPU quiesce coordinator.
//!
//! The bounded peer-selection and ack-wait logic is exercised over plain
//! local slices through the pure [`super::poke_others`] / [`super::wait_for_acks`]
//! helpers, so both the all-acknowledged and the fail-closed timeout paths are
//! deterministic and fast (a tiny budget, no dependence on the process-global
//! published tables). A single lifecycle test covers the set-once
//! [`super::publish_tables`] and the [`super::acknowledge`] /
//! [`super::stop_requested`] statics, because those are genuinely
//! process-global and cannot be re-published per test.

extern crate std;

use super::{
    acknowledge, poke_others, publish_tables, stop_requested, wait_for_acks, PublishError,
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
fn wait_for_acks_returns_ok_once_every_online_peer_has_acknowledged() {
    let online = table(3);
    for slot in &online {
        slot.store(true, Ordering::Relaxed);
    }
    let ack = table(3);
    // Peers 1 and 2 acknowledge (0 is current, need not ack).
    ack[1].store(true, Ordering::Relaxed);
    ack[2].store(true, Ordering::Relaxed);
    assert_eq!(wait_for_acks(0, &online, &ack, 8), Ok(()));
}

#[test]
fn wait_for_acks_ignores_offline_peers() {
    let online = table(3);
    online[0].store(true, Ordering::Relaxed);
    online[1].store(true, Ordering::Relaxed);
    // CPU 2 is offline and never acknowledges; it must not hold the wait up.
    let ack = table(3);
    ack[1].store(true, Ordering::Relaxed);
    assert_eq!(wait_for_acks(0, &online, &ack, 8), Ok(()));
}

#[test]
fn wait_for_acks_fails_closed_naming_the_stuck_peer() {
    let online = table(3);
    for slot in &online {
        slot.store(true, Ordering::Relaxed);
    }
    let ack = table(3);
    // Peer 1 acknowledges; peer 2 never does, so a bounded wait times out and
    // names peer 2 (fail closed).
    ack[1].store(true, Ordering::Relaxed);
    assert_eq!(wait_for_acks(0, &online, &ack, 4), Err(2));
}

#[test]
fn wait_for_acks_with_no_peers_returns_ok_immediately() {
    // A single online CPU that is itself the current CPU: nothing to wait for.
    let online = table(1);
    online[0].store(true, Ordering::Relaxed);
    let ack = table(1);
    assert_eq!(wait_for_acks(0, &online, &ack, 1), Ok(()));
}

/// The process-global statics: publish is set-once, length is validated, and
/// `acknowledge` sets exactly the caller's slot and is observable.
///
/// One test owns the whole lifecycle because the publish slot is genuinely
/// set-once per process (mirrors the SMP hand-off's single-lifecycle test).
#[test]
fn published_tables_are_set_once_and_acknowledge_targets_the_callers_slot() {
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

    // `stop_requested` reflects the latch; it is only set by `quiesce_others`,
    // so before any quiesce it reads false.
    let _ = stop_requested();
}
