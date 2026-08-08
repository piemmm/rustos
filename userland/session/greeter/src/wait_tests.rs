use tairix_abi::time::{Duration64, Time64};

use super::{park_timeout, Cooldown, FOREVER, NANOS_PER_SEC};

/// A wall time `secs` seconds and `nanos` nanoseconds past a minute boundary.
fn past_the_minute(secs: i64, nanos: u32) -> Time64 {
    Time64::new(1_700_000_040 + secs, nanos).expect("a canonical nanosecond field")
}

#[test]
fn an_idle_screen_arms_no_timer() {
    assert_eq!(park_timeout(None, Duration64::ZERO), FOREVER);
}

#[test]
fn the_clock_alone_wakes_at_the_next_minute() {
    let timeout = park_timeout(Some(past_the_minute(20, 0)), Duration64::ZERO);
    assert_eq!(timeout, 40 * NANOS_PER_SEC);
}

#[test]
fn a_sub_second_reading_is_subtracted_from_the_wait() {
    let timeout = park_timeout(Some(past_the_minute(20, 250_000_000)), Duration64::ZERO);
    assert_eq!(timeout, 39 * NANOS_PER_SEC + 750_000_000);
}

#[test]
fn a_reading_on_the_boundary_waits_a_whole_minute_not_zero() {
    let timeout = park_timeout(Some(past_the_minute(0, 0)), Duration64::ZERO);
    assert_eq!(timeout, 60 * NANOS_PER_SEC);
}

#[test]
fn a_running_lockout_ticks_once_a_second() {
    let timeout = park_timeout(Some(past_the_minute(20, 0)), Duration64::from_secs(30));
    assert_eq!(timeout, NANOS_PER_SEC);
}

#[test]
fn the_last_of_a_lockout_wakes_exactly_when_it_ends() {
    let nearly_done = Duration64::new(0, 400_000_000).expect("a canonical field");
    let timeout = park_timeout(Some(past_the_minute(20, 0)), nearly_done);
    assert_eq!(timeout, 400_000_000);
}

#[test]
fn the_deadline_is_the_nearer_of_the_two() {
    let close_to_the_minute = past_the_minute(59, 500_000_000);
    let clock_only = park_timeout(Some(close_to_the_minute), Duration64::ZERO);
    assert_eq!(clock_only, 500_000_000);

    let with_a_longer_tick = park_timeout(Some(close_to_the_minute), Duration64::from_secs(30));
    assert_eq!(with_a_longer_tick, 500_000_000, "the clock is nearer");

    let with_a_shorter_tick = park_timeout(
        Some(close_to_the_minute),
        Duration64::new(0, 100_000_000).expect("a canonical field"),
    );
    assert_eq!(with_a_shorter_tick, 100_000_000, "the lockout is nearer");
}

#[test]
fn a_lockout_with_no_clock_still_ticks() {
    let timeout = park_timeout(None, Duration64::from_secs(5));
    assert_eq!(timeout, NANOS_PER_SEC);
}

#[test]
fn a_lockout_counts_down_against_the_monotonic_clock() {
    let mut cooldown = Cooldown::default();
    assert_eq!(cooldown.remaining(0), Duration64::ZERO);
    assert!(!cooldown.is_running(0));

    cooldown.start(1_000, Duration64::from_secs(2));
    assert!(cooldown.is_running(1_000));
    assert_eq!(cooldown.remaining(1_000), Duration64::from_secs(2));
    assert_eq!(
        cooldown.remaining(1_000 + NANOS_PER_SEC),
        Duration64::from_secs(1)
    );
}

#[test]
fn a_lockout_that_has_run_out_reports_zero() {
    let mut cooldown = Cooldown::default();
    cooldown.start(0, Duration64::from_secs(2));
    let after = 3 * NANOS_PER_SEC;
    assert_eq!(cooldown.remaining(after), Duration64::ZERO);
    assert!(!cooldown.is_running(after));
}

#[test]
fn a_zero_or_negative_span_is_not_a_lockout() {
    let mut cooldown = Cooldown::default();
    cooldown.start(0, Duration64::from_secs(5));
    cooldown.start(0, Duration64::ZERO);
    assert!(!cooldown.is_running(0));

    cooldown.start(0, Duration64::from_secs(5));
    cooldown.start(0, Duration64::from_secs(-5));
    assert!(!cooldown.is_running(0));
}

#[test]
fn a_clock_that_ran_backwards_never_lengthens_a_lockout() {
    let mut cooldown = Cooldown::default();
    cooldown.start(10 * NANOS_PER_SEC, Duration64::from_secs(1));
    assert_eq!(cooldown.remaining(0), Duration64::from_secs(11));
    assert_eq!(cooldown.remaining(u64::MAX), Duration64::ZERO);
}
