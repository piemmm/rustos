//! Unit tests for the [`super`] link-aggregation (bond) engine.

use super::*;

/// A monitor interval / instant in milliseconds, as a [`Duration64`].
fn ms(millis: u64) -> Duration64 {
    Duration64::from_nanos(millis * 1_000_000)
}

/// A member id from a short ascii name, right-padded with zeros.
fn member(name: &str) -> MemberId {
    let mut id = [0u8; 16];
    let bytes = name.as_bytes();
    id[..bytes.len()].copy_from_slice(bytes);
    id
}

/// A bond with the given mode/primary and a 100 ms monitor interval.
fn bond(mode: BondMode, primary: Option<&str>) -> Bond {
    Bond::new(&BondConfig {
        mode,
        monitor_interval: ms(100),
        primary: primary.map(member),
    })
}

/// Bring a member up and admit it: report link-up at `t0`, then advance
/// past the up-delay. Returns the admission's transition.
fn bring_up(b: &mut Bond, name: &str, t0: u64) -> Option<BondEvent> {
    let m = member(name);
    let down = b.set_member_link(m, LinkState::Up, ms(t0));
    assert_eq!(down, None, "link-up alone must not admit (up-delay)");
    b.advance(ms(t0 + 100))
}

#[test]
fn enrolment_is_bounded_and_rejects_duplicates() {
    let mut b = bond(BondMode::ActiveBackup, None);
    for i in 0..MAX_BOND_MEMBERS {
        assert_eq!(b.add_member(member(&alloc::format!("eth{i}"))), Ok(()));
    }
    assert_eq!(
        b.add_member(member("ethX")),
        Err(BondError::TooManyMembers),
        "past MAX_BOND_MEMBERS must be refused"
    );
    assert_eq!(
        b.add_member(member("eth0")),
        Err(BondError::DuplicateMember),
    );
}

#[test]
fn removing_an_unknown_member_is_an_error() {
    let mut b = bond(BondMode::ActiveBackup, None);
    assert_eq!(
        b.remove_member(member("nope")),
        Err(BondError::UnknownMember)
    );
}

#[test]
fn a_link_up_member_is_admitted_only_after_the_up_delay() {
    let mut b = bond(BondMode::ActiveBackup, None);
    b.add_member(member("eth0")).unwrap();

    // Link-up alone does not admit and does not change the path.
    assert_eq!(
        b.set_member_link(member("eth0"), LinkState::Up, ms(0)),
        None
    );
    assert!(!b.is_up());
    assert_eq!(
        b.transmit_member(0),
        None,
        "unadmitted member cannot carry traffic"
    );

    // The monitor deadline is armed at up_since + interval, then unarms.
    assert_eq!(b.next_deadline(), Some(ms(100)));

    // Advancing before the interval does not admit.
    assert_eq!(b.advance(ms(99)), None);
    assert!(!b.is_up());

    // Advancing at the interval admits and brings the bond *up* — a
    // bring-up, never a failover: there was no previous path.
    assert_eq!(b.advance(ms(100)), Some(BondEvent::CameUp));
    assert!(b.is_up());
    assert_eq!(b.active_member(), Some(member("eth0")));
    assert_eq!(b.transmit_member(0), Some(member("eth0")));
    assert_eq!(b.next_deadline(), None, "no pending admission ⇒ tickless");
}

#[test]
fn active_backup_fails_over_immediately_on_link_down() {
    let mut b = bond(BondMode::ActiveBackup, None);
    b.add_member(member("eth0")).unwrap();
    b.add_member(member("eth1")).unwrap();
    bring_up(&mut b, "eth0", 0);
    bring_up(&mut b, "eth1", 0);
    assert_eq!(
        b.active_member(),
        Some(member("eth0")),
        "first eligible is active"
    );
    assert_eq!(b.eligible_count(), 2);

    // The active member drops: failover is immediate (no advance needed).
    let event = b.set_member_link(member("eth0"), LinkState::Down, ms(500));
    assert_eq!(event, Some(BondEvent::PathChanged));
    assert_eq!(b.active_member(), Some(member("eth1")));
    assert_eq!(b.transmit_member(0), Some(member("eth1")));
}

/// Regression: a bond's bring-up was reported as `PathChanged`, so an
/// auditor (and the bond QEMU vertical's failover witness) could not tell a
/// bond acquiring its first member from a live member dying under it. The
/// two transitions must be distinguishable in both modes.
#[test]
fn a_bring_up_is_never_reported_as_a_path_change() {
    for mode in [BondMode::ActiveBackup, BondMode::Balance] {
        let mut b = bond(mode, None);
        b.add_member(member("eth0")).unwrap();
        b.add_member(member("eth1")).unwrap();

        // Both members come up together, so one admission sweep takes the
        // bond from "cannot transmit" to "can": a bring-up.
        b.set_member_link(member("eth0"), LinkState::Up, ms(0));
        b.set_member_link(member("eth1"), LinkState::Up, ms(0));
        assert_eq!(b.advance(ms(100)), Some(BondEvent::CameUp), "{mode:?}");

        // Only now can the path *change*: a member dies under a bond that
        // was already carrying traffic.
        assert_eq!(
            b.set_member_link(member("eth0"), LinkState::Down, ms(200)),
            Some(BondEvent::PathChanged),
            "{mode:?}"
        );

        // And losing the last one is its own transition, not a path change.
        assert_eq!(
            b.set_member_link(member("eth1"), LinkState::Down, ms(300)),
            Some(BondEvent::WentDown),
            "{mode:?}"
        );

        // A recovered member brings it back up, not "over".
        b.set_member_link(member("eth1"), LinkState::Up, ms(400));
        assert_eq!(b.advance(ms(500)), Some(BondEvent::CameUp), "{mode:?}");
    }
}

#[test]
fn active_backup_goes_down_when_the_last_member_dies() {
    let mut b = bond(BondMode::ActiveBackup, None);
    b.add_member(member("eth0")).unwrap();
    bring_up(&mut b, "eth0", 0);
    assert!(b.is_up());

    let event = b.set_member_link(member("eth0"), LinkState::Down, ms(500));
    assert_eq!(event, Some(BondEvent::WentDown));
    assert!(!b.is_up());
    assert_eq!(b.active_member(), None);
    assert_eq!(
        b.transmit_member(0),
        None,
        "a down bond fails closed on transmit"
    );
}

#[test]
fn a_recovered_primary_reclaims_the_path_only_after_the_up_delay() {
    let mut b = bond(BondMode::ActiveBackup, Some("eth0"));
    b.add_member(member("eth0")).unwrap();
    b.add_member(member("eth1")).unwrap();
    bring_up(&mut b, "eth0", 0);
    bring_up(&mut b, "eth1", 0);
    assert_eq!(b.active_member(), Some(member("eth0")), "primary is active");

    // Primary dies ⇒ immediate failover to the backup.
    assert_eq!(
        b.set_member_link(member("eth0"), LinkState::Down, ms(500)),
        Some(BondEvent::PathChanged)
    );
    assert_eq!(b.active_member(), Some(member("eth1")));

    // Primary recovers: it does NOT reclaim the path instantly (no flap).
    assert_eq!(
        b.set_member_link(member("eth0"), LinkState::Up, ms(600)),
        None
    );
    assert_eq!(
        b.active_member(),
        Some(member("eth1")),
        "failback is deliberate"
    );
    assert_eq!(b.next_deadline(), Some(ms(700)));

    // After the up-delay, the primary deliberately reclaims the path.
    assert_eq!(b.advance(ms(700)), Some(BondEvent::PathChanged));
    assert_eq!(b.active_member(), Some(member("eth0")));
}

#[test]
fn without_a_primary_a_recovered_member_does_not_preempt_the_active() {
    let mut b = bond(BondMode::ActiveBackup, None);
    b.add_member(member("eth0")).unwrap();
    b.add_member(member("eth1")).unwrap();
    bring_up(&mut b, "eth0", 0);
    bring_up(&mut b, "eth1", 0);
    assert_eq!(b.active_member(), Some(member("eth0")));

    // eth0 dies ⇒ eth1 active.
    b.set_member_link(member("eth0"), LinkState::Down, ms(500));
    assert_eq!(b.active_member(), Some(member("eth1")));

    // eth0 recovers and is admitted, but must not preempt eth1 (no needless
    // path change without a declared primary).
    b.set_member_link(member("eth0"), LinkState::Up, ms(600));
    assert_eq!(b.advance(ms(700)), None, "no preemption ⇒ no path change");
    assert_eq!(b.active_member(), Some(member("eth1")));
    assert_eq!(b.eligible_count(), 2);
}

#[test]
fn balance_keeps_a_flow_on_one_member_and_spreads_across_the_set() {
    let mut b = bond(BondMode::Balance, None);
    b.add_member(member("eth0")).unwrap();
    b.add_member(member("eth1")).unwrap();
    assert_eq!(
        b.active_member(),
        None,
        "balance has no single active member"
    );

    // The first admitted member brings the ring up from empty.
    assert_eq!(bring_up(&mut b, "eth0", 0), Some(BondEvent::CameUp));
    // Second member joining changes an already-transmitting eligible set.
    assert_eq!(bring_up(&mut b, "eth1", 0), Some(BondEvent::PathChanged));
    assert_eq!(b.eligible_count(), 2);

    // The same flow hash always maps to the same member.
    let h = flow_hash(TEST_KEY, &[10, 0, 0, 1], &[10, 0, 0, 2], 40000, 80);
    let first = b.transmit_member(h).unwrap();
    assert_eq!(b.transmit_member(h), Some(first));

    // Two flows that hash to different parities land on different members.
    let even = b.transmit_member(0).unwrap();
    let odd = b.transmit_member(1).unwrap();
    assert_ne!(even, odd, "the two-member ring spreads even/odd hashes");
}

#[test]
fn balance_member_loss_moves_its_flows_and_going_empty_fails_closed() {
    let mut b = bond(BondMode::Balance, None);
    b.add_member(member("eth0")).unwrap();
    b.add_member(member("eth1")).unwrap();
    bring_up(&mut b, "eth0", 0);
    bring_up(&mut b, "eth1", 0);

    // Losing a member is a path change; every flow now maps to the survivor.
    let event = b.set_member_link(member("eth1"), LinkState::Down, ms(500));
    assert_eq!(event, Some(BondEvent::PathChanged));
    assert_eq!(b.transmit_member(0), Some(member("eth0")));
    assert_eq!(b.transmit_member(1), Some(member("eth0")));

    // Losing the last member fails closed.
    let event = b.set_member_link(member("eth0"), LinkState::Down, ms(600));
    assert_eq!(event, Some(BondEvent::WentDown));
    assert_eq!(b.transmit_member(0), None);
}

#[test]
fn removing_the_active_member_fails_over() {
    let mut b = bond(BondMode::ActiveBackup, None);
    b.add_member(member("eth0")).unwrap();
    b.add_member(member("eth1")).unwrap();
    bring_up(&mut b, "eth0", 0);
    bring_up(&mut b, "eth1", 0);
    assert_eq!(b.active_member(), Some(member("eth0")));

    let event = b.remove_member(member("eth0")).unwrap();
    assert_eq!(event, Some(BondEvent::PathChanged));
    assert_eq!(b.active_member(), Some(member("eth1")));
    assert_eq!(b.member_ids(), alloc::vec![member("eth1")]);
}

#[test]
fn switching_mode_at_runtime_recomputes_the_path() {
    let mut b = bond(BondMode::ActiveBackup, None);
    b.add_member(member("eth0")).unwrap();
    b.add_member(member("eth1")).unwrap();
    bring_up(&mut b, "eth0", 0);
    bring_up(&mut b, "eth1", 0);
    assert_eq!(b.active_member(), Some(member("eth0")));

    // Active-backup ⇒ balance: the whole eligible set becomes the ring.
    assert_eq!(b.set_mode(BondMode::Balance), Some(BondEvent::PathChanged));
    assert_eq!(b.mode(), BondMode::Balance);
    assert_eq!(b.active_member(), None);
    assert_eq!(b.transmit_member(1), Some(member("eth1")));

    // Idempotent: setting the same mode is a no-op.
    assert_eq!(b.set_mode(BondMode::Balance), None);

    // A switch on a *down* bond has no path to change, so it reports
    // nothing — and in particular not a bring-up, even though the switch
    // clears the selection state.
    let mut down = bond(BondMode::ActiveBackup, None);
    down.add_member(member("eth0")).unwrap();
    assert_eq!(down.set_mode(BondMode::Balance), None);
    assert!(!down.is_up());
}

#[test]
fn setting_a_primary_at_runtime_reclaims_the_path() {
    let mut b = bond(BondMode::ActiveBackup, None);
    b.add_member(member("eth0")).unwrap();
    b.add_member(member("eth1")).unwrap();
    bring_up(&mut b, "eth0", 0);
    bring_up(&mut b, "eth1", 0);
    // eth0 dies, eth1 active.
    b.set_member_link(member("eth0"), LinkState::Down, ms(500));
    b.set_member_link(member("eth0"), LinkState::Up, ms(600));
    b.advance(ms(700));
    assert_eq!(
        b.active_member(),
        Some(member("eth1")),
        "no preempt without primary"
    );

    // Declaring eth0 the primary makes it reclaim the path (it is eligible).
    assert_eq!(
        b.set_primary(Some(member("eth0"))),
        Some(BondEvent::PathChanged)
    );
    assert_eq!(b.active_member(), Some(member("eth0")));
    assert_eq!(b.primary(), Some(member("eth0")));
}

#[test]
fn introspection_reports_member_link_and_eligibility() {
    let mut b = bond(BondMode::ActiveBackup, None);
    b.add_member(member("eth0")).unwrap();
    assert_eq!(b.is_member_link_up(member("eth0")), Some(false));
    assert_eq!(b.is_member_eligible(member("eth0")), Some(false));
    assert_eq!(b.is_member_link_up(member("absent")), None);

    b.set_member_link(member("eth0"), LinkState::Up, ms(0));
    assert_eq!(b.is_member_link_up(member("eth0")), Some(true));
    assert_eq!(
        b.is_member_eligible(member("eth0")),
        Some(false),
        "up but not yet admitted"
    );

    b.advance(ms(100));
    assert_eq!(b.is_member_eligible(member("eth0")), Some(true));
}

#[test]
fn a_stale_report_from_a_removed_member_is_harmless() {
    let mut b = bond(BondMode::ActiveBackup, None);
    b.add_member(member("eth0")).unwrap();
    bring_up(&mut b, "eth0", 0);
    b.remove_member(member("eth0")).unwrap();
    // A late report for the now-removed member changes nothing.
    assert_eq!(
        b.set_member_link(member("eth0"), LinkState::Down, ms(900)),
        None
    );
    assert!(!b.is_up());
}

/// A fixed key for the flow-hash assertions; production draws the process's
/// own.
const TEST_KEY: HashSeed = HashSeed::from_words(0x1122_3344_5566_7788, 0x99aa_bbcc_ddee_ff00);

#[test]
fn flow_hash_is_deterministic_and_direction_sensitive() {
    let a = flow_hash(TEST_KEY, &[10, 0, 0, 1], &[10, 0, 0, 2], 1234, 80);
    let b = flow_hash(TEST_KEY, &[10, 0, 0, 1], &[10, 0, 0, 2], 1234, 80);
    assert_eq!(a, b, "same 4-tuple ⇒ same hash");

    let swapped = flow_hash(TEST_KEY, &[10, 0, 0, 2], &[10, 0, 0, 1], 80, 1234);
    assert_ne!(a, swapped, "reversed tuple ⇒ different hash");

    // Works for v6-length octets too (address-family agnostic).
    let v6 = flow_hash(TEST_KEY, &[0u8; 16], &[1u8; 16], 5000, 443);
    assert_ne!(v6, 0);
}

/// The key is the whole defence: a peer that could predict the hash would
/// steer every flow of an attack onto one member. The same tuples must
/// therefore select differently under a different key.
#[test]
fn the_key_decides_which_member_a_chosen_flow_selects() {
    let other = HashSeed::from_words(0x1122_3344_5566_7788, 0x99aa_bbcc_ddee_ff01);
    let moved = (0..64u16)
        .filter(|&port| {
            let mine = flow_hash(TEST_KEY, &[], &[203, 0, 113, 7], port, 443);
            let theirs = flow_hash(other, &[], &[203, 0, 113, 7], port, 443);
            mine != theirs
        })
        .count();
    assert_eq!(moved, 64, "a different key left a flow hash unchanged");
}
