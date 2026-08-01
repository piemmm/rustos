//! Unit tests for [`Publisher`].

use tairix_abi::switchboard_ipc::{TrayPermille, TraySummary};

use super::{Publisher, KEEPALIVE_NS};

fn summary(cpu_busy: u16) -> TraySummary {
    TraySummary {
        jobs: 0,
        recovery: 0,
        cpu_busy_permille: TrayPermille::new(cpu_busy).expect("within bounds"),
        pressure: None,
        top_task: None,
        power_capable: false,
    }
}

#[test]
fn the_first_offer_always_publishes() {
    let mut publisher = Publisher::new();
    assert_eq!(publisher.offer(summary(0), 0), Some(summary(0)));
}

#[test]
fn an_unchanged_summary_is_not_republished_before_the_keepalive() {
    let mut publisher = Publisher::new();
    let first = publisher.offer(summary(10), 0).expect("first offer");
    publisher.record_ack(first);

    assert_eq!(publisher.offer(summary(10), 1_000), None);
}

#[test]
fn a_changed_summary_republishes_immediately() {
    let mut publisher = Publisher::new();
    let first = publisher.offer(summary(10), 0).expect("first offer");
    publisher.record_ack(first);

    assert_eq!(publisher.offer(summary(20), 1_000), Some(summary(20)));
}

#[test]
fn the_keepalive_republishes_an_unchanged_summary() {
    let mut publisher = Publisher::new();
    let first = publisher.offer(summary(10), 0).expect("first offer");
    publisher.record_ack(first);

    assert_eq!(publisher.offer(summary(10), KEEPALIVE_NS - 1), None);
    assert_eq!(
        publisher.offer(summary(10), KEEPALIVE_NS),
        Some(summary(10))
    );
}

#[test]
fn an_unacknowledged_attempt_retries_on_the_very_next_offer() {
    let mut publisher = Publisher::new();
    // The first attempt is never acknowledged (simulating a publish that
    // failed): the baseline for change detection stays unset, so the very
    // next offer of the same summary retries immediately rather than
    // waiting for the keepalive.
    let _ = publisher.offer(summary(10), 0);
    assert_eq!(publisher.offer(summary(10), 1), Some(summary(10)));
}

#[test]
fn record_ack_clears_the_failure_count() {
    let mut publisher = Publisher::new();
    assert_eq!(publisher.record_failure(), 1);
    assert_eq!(publisher.record_failure(), 2);
    publisher.record_ack(summary(0));
    assert_eq!(publisher.consecutive_failures(), 0);
}

#[test]
fn record_failure_increments_and_saturates() {
    let mut publisher = Publisher::new();
    for expected in 1..=3u32 {
        assert_eq!(publisher.record_failure(), expected);
    }
    assert_eq!(publisher.consecutive_failures(), 3);
}
