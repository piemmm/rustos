//! Unit tests for the sync-decision policy and the clock update it produces.

use super::{
    decide, ClockUpdate, Decision, Event, SyncReason, SyncRecord, TimeSync, DEFAULT_REFRESH,
    INITIAL_DELAY_SPAN, STALE_BOOT_GAP, STEP_THRESHOLD,
};
use tairix_abi::time::{Duration64, Time64};
use tairix_abi::{WallClockReading, WallTimeState, PLAUSIBLE_FUTURE_SECS, RELEASE_EPOCH_SECS};
use tairix_net::ntp::{KissCode, NtpTimestamp, PACKET_LEN};

/// A wall instant comfortably inside the plausibility window.
fn plausible() -> Time64 {
    Time64::from_secs(RELEASE_EPOCH_SECS + 30 * 86_400)
}

fn reading(time: Time64, state: WallTimeState) -> WallClockReading {
    WallClockReading::new(time, state)
}

fn unset() -> WallClockReading {
    WallClockReading::UNSET
}

fn seen_at(time: Time64) -> SyncRecord {
    SyncRecord::EMPTY.synced_at(time)
}

/// Assert a correction is `expected` to within the half-round-trip the engine
/// legitimately folds into the applied instant.
fn assert_near(actual: Option<Duration64>, expected: Duration64) {
    let actual = actual.expect("a previous reading existed");
    let slack = Duration64::from_nanos(50_000_000).saturating_total_nanos();
    let delta = actual
        .saturating_total_nanos()
        .abs_diff(expected.saturating_total_nanos());
    assert!(
        delta <= slack,
        "correction {actual:?} should be within 50ms of {expected:?}"
    );
}

// --- The decision matrix -------------------------------------------------

#[test]
fn an_unset_clock_syncs_at_once() {
    assert_eq!(
        decide(unset(), SyncRecord::EMPTY, DEFAULT_REFRESH),
        Decision::SyncNow(SyncReason::ClockUnset)
    );
    // Even with a record to compare against, unset wins: there is nothing to
    // compare.
    assert_eq!(
        decide(unset(), seen_at(plausible()), DEFAULT_REFRESH),
        Decision::SyncNow(SyncReason::ClockUnset)
    );
}

#[test]
fn a_believable_clock_does_not_sync_at_boot() {
    // The reason this policy exists: a machine with a working RTC must not
    // query a public server merely because it rebooted.
    for state in [
        WallTimeState::Firmware,
        WallTimeState::Trusted,
        WallTimeState::Adjusted,
    ] {
        assert_eq!(
            decide(
                reading(plausible(), state),
                seen_at(plausible()),
                DEFAULT_REFRESH
            ),
            Decision::RefreshAfter(DEFAULT_REFRESH),
            "a plausible {state:?} clock must be believed at boot"
        );
    }
    // And with no record at all, there is still no reason to distrust it.
    assert_eq!(
        decide(
            reading(plausible(), WallTimeState::Firmware),
            SyncRecord::EMPTY,
            DEFAULT_REFRESH
        ),
        Decision::RefreshAfter(DEFAULT_REFRESH)
    );
}

#[test]
fn an_implausible_clock_syncs_at_once() {
    for secs in [
        0,
        RELEASE_EPOCH_SECS - 1,
        -1,
        RELEASE_EPOCH_SECS + PLAUSIBLE_FUTURE_SECS + 1,
        i64::MAX,
    ] {
        assert_eq!(
            decide(
                reading(Time64::from_secs(secs), WallTimeState::Firmware),
                SyncRecord::EMPTY,
                DEFAULT_REFRESH
            ),
            Decision::SyncNow(SyncReason::Implausible),
            "a clock reading {secs} must be distrusted"
        );
    }
}

#[test]
fn a_clock_reading_earlier_than_last_seen_syncs_at_once() {
    // The dead-RTC-battery case: the reset date can be perfectly plausible in
    // itself, and only the persisted record exposes it.
    let seen = plausible();
    let earlier = Time64::from_secs(seen.secs() - 1);
    assert_eq!(
        decide(
            reading(earlier, WallTimeState::Firmware),
            seen_at(seen),
            DEFAULT_REFRESH
        ),
        Decision::SyncNow(SyncReason::WentBackwards)
    );
}

#[test]
fn a_gap_wider_than_the_stale_boot_bound_syncs_at_once() {
    let seen = plausible();
    let bound = STALE_BOOT_GAP.secs();
    // Exactly at the bound is still believed; one second past it is not.
    assert_eq!(
        decide(
            reading(
                Time64::from_secs(seen.secs() + bound),
                WallTimeState::Firmware
            ),
            seen_at(seen),
            DEFAULT_REFRESH
        ),
        Decision::RefreshAfter(DEFAULT_REFRESH),
        "the bound itself is not yet stale"
    );
    assert_eq!(
        decide(
            reading(
                Time64::from_secs(seen.secs() + bound + 1),
                WallTimeState::Firmware
            ),
            seen_at(seen),
            DEFAULT_REFRESH
        ),
        Decision::SyncNow(SyncReason::StaleBoot)
    );
}

#[test]
fn the_stale_boot_bound_is_five_days() {
    assert_eq!(STALE_BOOT_GAP, Duration64::from_secs(5 * 24 * 3600));
    assert_eq!(DEFAULT_REFRESH, Duration64::from_secs(24 * 3600));
}

#[test]
fn a_missing_record_never_fabricates_a_reason() {
    // A missing or unreadable store must leave the gap rules silent, not fire
    // them against a guessed instant.
    assert_eq!(
        decide(
            reading(plausible(), WallTimeState::Trusted),
            SyncRecord::EMPTY,
            DEFAULT_REFRESH
        ),
        Decision::RefreshAfter(DEFAULT_REFRESH)
    );
}

#[test]
fn the_refresh_cadence_is_whatever_was_configured() {
    let hourly = Duration64::from_secs(3600);
    assert_eq!(
        decide(
            reading(plausible(), WallTimeState::Trusted),
            seen_at(plausible()),
            hourly
        ),
        Decision::RefreshAfter(hourly)
    );
}

// --- The persisted record -------------------------------------------------

#[test]
fn the_record_tracks_the_latest_instant_observed() {
    let early = plausible();
    let late = Time64::from_secs(early.secs() + 10_000);
    let r = SyncRecord::EMPTY.synced_at(early);
    assert_eq!(r.last_sync, Some(early));
    assert_eq!(r.last_seen, Some(early));
    let r = r.synced_at(late);
    assert_eq!(r.last_sync, Some(late));
    assert_eq!(r.last_seen, Some(late));
}

#[test]
fn the_record_never_lets_last_seen_go_backwards() {
    // A later sync that reports an earlier instant must not erase the fact
    // that a later instant was already observed, or the went-backwards rule
    // would be defeated by a single bad sample.
    let late = Time64::from_secs(plausible().secs() + 10_000);
    let r = SyncRecord::EMPTY.synced_at(late).synced_at(plausible());
    assert_eq!(r.last_sync, Some(plausible()));
    assert_eq!(r.last_seen, Some(late));
}

#[test]
fn the_empty_record_is_the_default() {
    assert_eq!(SyncRecord::default(), SyncRecord::EMPTY);
    assert_eq!(SyncRecord::EMPTY.last_sync, None);
    assert_eq!(SyncRecord::EMPTY.last_seen, None);
}

// --- The client's start-up scheduling ------------------------------------

#[test]
fn an_urgent_client_queries_within_the_initial_delay() {
    let mut client = TimeSync::new(
        2,
        DEFAULT_REFRESH,
        unset(),
        SyncRecord::EMPTY,
        Duration64::ZERO,
        0,
    );
    assert_eq!(client.urgency(), Some(SyncReason::ClockUnset));
    let due = client.next_deadline().expect("a query is scheduled");
    assert!(
        due <= INITIAL_DELAY_SPAN,
        "an unset clock must be corrected promptly, got {due:?}"
    );
    assert!(client.poll(due, 1).is_some(), "the query is due at {due:?}");
}

#[test]
fn a_believed_clock_waits_the_refresh_cadence_not_the_initial_delay() {
    let client = TimeSync::new(
        2,
        DEFAULT_REFRESH,
        reading(plausible(), WallTimeState::Firmware),
        seen_at(plausible()),
        Duration64::ZERO,
        u64::MAX / 2,
    );
    assert_eq!(client.urgency(), None, "there is no reason to hurry");
    let due = client.next_deadline().expect("a refresh is scheduled");
    // Jittered by +/-25% of the cadence, so far beyond the initial delay.
    assert!(
        due > INITIAL_DELAY_SPAN,
        "a believed clock must not be queried immediately, got {due:?}"
    );
    let cadence = DEFAULT_REFRESH.saturating_total_nanos();
    let at = due.saturating_total_nanos();
    assert!(
        at >= cadence - cadence / 4 && at <= cadence + cadence / 4,
        "the first refresh {at} should be within jitter of {cadence}"
    );
}

#[test]
fn the_first_query_is_spread_across_a_fleet() {
    // Two machines with different entropy must not converge on one instant.
    let due = |entropy| {
        TimeSync::new(
            1,
            DEFAULT_REFRESH,
            unset(),
            SyncRecord::EMPTY,
            Duration64::ZERO,
            entropy,
        )
        .next_deadline()
        .expect("scheduled")
    };
    assert_ne!(due(0), due(u64::MAX));
}

#[test]
fn the_boot_instant_offsets_the_schedule() {
    // A client built partway into a boot schedules relative to that instant,
    // not to zero.
    let start = Duration64::from_secs(500);
    let client = TimeSync::new(1, DEFAULT_REFRESH, unset(), SyncRecord::EMPTY, start, 0);
    assert!(client.next_deadline().expect("scheduled") >= start);
}

// --- Applying a sample ----------------------------------------------------

/// A well-formed stratum-2 reply echoing `nonce` and reporting `at`.
fn reply(nonce: u64, at: Time64) -> [u8; PACKET_LEN] {
    let ntp_secs = u32::try_from((at.secs() + 2_208_988_800).rem_euclid(1 << 32))
        .expect("reduced modulo 2^32");
    let ts = NtpTimestamp::from_raw(u64::from(ntp_secs) << 32);
    let mut p = [0u8; PACKET_LEN];
    p[0] = (4 << 3) | 4; // version 4, mode 4 (server)
    p[1] = 2; // stratum
    p[24..32].copy_from_slice(&nonce.to_be_bytes());
    p[32..40].copy_from_slice(&ts.raw().to_be_bytes());
    p[40..48].copy_from_slice(&ts.raw().to_be_bytes());
    p
}

/// Drive a client to an in-flight request, returning the nonce it carries and
/// a receive instant 20 ms later — a realistic round trip, well inside the
/// engine's ceiling.
fn in_flight(client: &mut TimeSync) -> (u64, Duration64) {
    const NONCE: u64 = 0xDEAD_BEEF_FEED_FACE;
    let due = client.next_deadline().expect("scheduled");
    client.poll(due, NONCE).expect("a query is due");
    let received = Duration64::from_nanos(due.saturating_total_nanos() + 20_000_000);
    (NONCE, received)
}

fn urgent_client() -> TimeSync {
    TimeSync::new(
        1,
        DEFAULT_REFRESH,
        unset(),
        SyncRecord::EMPTY,
        Duration64::ZERO,
        0,
    )
}

#[test]
fn establishing_an_unset_clock_is_trusted_a_step_and_has_no_correction() {
    let mut client = urgent_client();
    let (nonce, at) = in_flight(&mut client);
    let Event::Apply(update) = client.on_datagram(at, unset(), &reply(nonce, plausible())) else {
        panic!("a well-formed reply must produce a clock update");
    };
    assert_eq!(update.state, WallTimeState::Trusted);
    assert_eq!(
        update.correction, None,
        "there is no previous reading to measure a correction against"
    );
    assert!(update.stepped, "establishing the clock is a step");
    assert_eq!(update.stratum, 2);
    assert_eq!(update.wall.secs(), plausible().secs());
    // The urgency is discharged once the clock is set.
    assert_eq!(client.urgency(), None);
}

#[test]
fn a_small_correction_is_a_refinement_and_still_trusted() {
    let mut client = urgent_client();
    let (nonce, at) = in_flight(&mut client);
    // The clock already reads the same second the server reports.
    let current = reading(plausible(), WallTimeState::Firmware);
    let Event::Apply(update) = client.on_datagram(at, current, &reply(nonce, plausible())) else {
        panic!("expected a clock update");
    };
    assert_eq!(
        update.state,
        WallTimeState::Trusted,
        "the source is the network"
    );
    assert!(
        !update.stepped,
        "a sub-threshold correction is a refinement"
    );
    let correction = update.correction.expect("a previous reading existed");
    assert!(correction <= STEP_THRESHOLD);
}

#[test]
fn a_large_correction_is_reported_as_a_step() {
    let mut client = urgent_client();
    let (nonce, at) = in_flight(&mut client);
    // The clock is an hour behind what the server reports.
    let behind = Time64::from_secs(plausible().secs() - 3600);
    let Event::Apply(update) = client.on_datagram(
        at,
        reading(behind, WallTimeState::Firmware),
        &reply(nonce, plausible()),
    ) else {
        panic!("expected a clock update");
    };
    assert!(update.stepped, "an hour is a step, not a refinement");
    // About an hour: the applied instant is the server's transmit plus half
    // the measured round trip, so the correction carries that 10 ms too.
    assert_near(update.correction, Duration64::from_secs(3600));
    assert_eq!(update.state, WallTimeState::Trusted);
}

#[test]
fn a_correction_backwards_is_measured_as_a_magnitude() {
    let mut client = urgent_client();
    let (nonce, at) = in_flight(&mut client);
    // The clock is an hour *ahead*; the correction magnitude is the same.
    let ahead = Time64::from_secs(plausible().secs() + 3600);
    let Event::Apply(update) = client.on_datagram(
        at,
        reading(ahead, WallTimeState::Trusted),
        &reply(nonce, plausible()),
    ) else {
        panic!("expected a clock update");
    };
    assert_near(update.correction, Duration64::from_secs(3600));
    assert!(update.stepped);
}

#[test]
fn the_client_never_reports_adjusted_provenance() {
    // Adjusted means "corrected after the fact by something other than its
    // source", which never describes a network sync.
    let mut client = urgent_client();
    let (nonce, at) = in_flight(&mut client);
    for state in [
        WallTimeState::Unset,
        WallTimeState::Firmware,
        WallTimeState::Trusted,
        WallTimeState::Adjusted,
    ] {
        let mut c = client.clone();
        let current = if state == WallTimeState::Unset {
            unset()
        } else {
            reading(plausible(), state)
        };
        if let Event::Apply(update) = c.on_datagram(at, current, &reply(nonce, plausible())) {
            assert_eq!(update.state, WallTimeState::Trusted, "from {state:?}");
        } else {
            panic!("expected a clock update from {state:?}");
        }
    }
}

// --- Events other than a sample -----------------------------------------

#[test]
fn a_spoofed_reply_is_ignored_and_the_clock_is_untouched() {
    let mut client = urgent_client();
    let (nonce, at) = in_flight(&mut client);
    assert_eq!(
        client.on_datagram(at, unset(), &reply(nonce ^ 1, plausible())),
        Event::Ignored,
        "a reply that does not echo the nonce must never set the clock"
    );
    // The urgency stands, because nothing was applied.
    assert_eq!(client.urgency(), Some(SyncReason::ClockUnset));
}

#[test]
fn an_implausible_sample_is_refused_not_applied() {
    let mut client = urgent_client();
    let (nonce, at) = in_flight(&mut client);
    let event = client.on_datagram(at, unset(), &reply(nonce, Time64::UNIX_EPOCH));
    assert!(
        matches!(event, Event::Refused(_)),
        "a server insisting on 1970 must be refused, got {event:?}"
    );
    assert_eq!(client.urgency(), Some(SyncReason::ClockUnset));
}

#[test]
fn a_kiss_of_death_surfaces_as_its_own_event() {
    let mut client = TimeSync::new(
        2,
        DEFAULT_REFRESH,
        unset(),
        SyncRecord::EMPTY,
        Duration64::ZERO,
        0,
    );
    let (nonce, at) = in_flight(&mut client);
    let mut p = reply(nonce, plausible());
    p[1] = 0; // stratum 0 marks a kiss
    p[12..16].copy_from_slice(b"DENY");
    assert_eq!(
        client.on_datagram(at, unset(), &p),
        Event::ServerRetired {
            server: 0,
            code: KissCode::Deny,
        }
    );
}

#[test]
fn exhausting_every_server_leaves_nothing_to_wait_for() {
    let mut client = TimeSync::new(
        1,
        DEFAULT_REFRESH,
        unset(),
        SyncRecord::EMPTY,
        Duration64::ZERO,
        0,
    );
    let (nonce, at) = in_flight(&mut client);
    let mut p = reply(nonce, plausible());
    p[1] = 0;
    p[12..16].copy_from_slice(b"RSTR");
    let _ = client.on_datagram(at, unset(), &p);
    assert!(client.is_exhausted());
    assert_eq!(client.next_deadline(), None);
}

#[test]
fn an_empty_server_configuration_never_queries() {
    let mut client = TimeSync::new(
        0,
        DEFAULT_REFRESH,
        unset(),
        SyncRecord::EMPTY,
        Duration64::ZERO,
        0,
    );
    assert!(client.is_exhausted());
    assert_eq!(client.next_deadline(), None);
    assert!(client.poll(Duration64::from_secs(1_000_000), 1).is_none());
}

#[test]
fn the_clock_update_is_inspectable_for_audit() {
    // Every field the audit record needs is present on one value.
    let mut client = urgent_client();
    let (nonce, at) = in_flight(&mut client);
    let Event::Apply(ClockUpdate {
        wall,
        state,
        correction,
        stepped,
        round_trip,
        stratum,
    }) = client.on_datagram(at, unset(), &reply(nonce, plausible()))
    else {
        panic!("expected a clock update");
    };
    assert_eq!(wall.secs(), plausible().secs());
    assert_eq!(state, WallTimeState::Trusted);
    assert_eq!(correction, None);
    assert!(stepped);
    assert!(round_trip <= Duration64::from_secs(3));
    assert_eq!(stratum, 2);
}
