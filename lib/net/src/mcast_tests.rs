//! Unit tests for the [`super`] host multicast-membership engine.

use super::*;
use crate::addr::{Ipv4Addr, Ipv6Addr, ALL_NODES};

const SEED: u64 = 0x5254_0012_3456;

fn v4_group() -> Ipv4Addr {
    Ipv4Addr::new(239, 1, 2, 3)
}

fn at(secs: i64) -> Duration64 {
    Duration64::from_secs(secs)
}

/// Collect the reasons of the reports for a given group.
fn reasons<A: Copy + PartialEq>(reports: &[MembershipReport<A>], group: A) -> Vec<ReportReason> {
    reports
        .iter()
        .filter(|r| r.group == group)
        .map(|r| r.reason)
        .collect()
}

/// Advance repeatedly so every unsolicited state-change retransmission
/// (each scheduled one interval after the last actually fired) drains,
/// leaving only query-scheduled responses pending.
fn settle<P: McastProtocol>(m: &mut Membership<P>) {
    for t in 0..=4 {
        let _ = m.advance(at(i64::from(t) * 10));
    }
}

#[test]
fn join_emits_robustness_unsolicited_reports() {
    let mut m: Membership<Igmp> = Membership::new(8, SEED);
    assert!(m.join(v4_group(), at(0)).expect("join"));
    assert!(m.is_member(v4_group()));

    // First unsolicited report is due immediately.
    let first = m.advance(at(0));
    assert_eq!(reasons(&first, v4_group()), [ReportReason::JoinGroup]);

    // The retransmission is one Unsolicited Report Interval later.
    assert!(m.advance(at(0)).is_empty());
    let second = m.advance(at(10));
    assert_eq!(reasons(&second, v4_group()), [ReportReason::JoinGroup]);

    // Robustness is 2, so no third.
    assert!(m.advance(at(30)).is_empty());
}

#[test]
fn refcount_holds_membership_until_last_leave() {
    let mut m: Membership<Igmp> = Membership::new(8, SEED);
    assert!(m.join(v4_group(), at(0)).expect("first"));
    assert!(!m.join(v4_group(), at(0)).expect("second ref"));
    let _ = m.advance(at(30));

    // First leave only drops a reference; still a member, no leave report.
    assert!(!m.leave(v4_group(), at(31)));
    assert!(m.is_member(v4_group()));
    assert!(m.advance(at(31)).is_empty());

    // Final leave sends the Leave Group message and drops membership.
    assert!(m.leave(v4_group(), at(32)));
    assert!(!m.is_member(v4_group()));
    let reports = m.advance(at(32));
    assert_eq!(reasons(&reports, v4_group()), [ReportReason::LeaveGroup]);
    // Robustness retransmit, then the record disappears.
    let reports = m.advance(at(42));
    assert_eq!(reasons(&reports, v4_group()), [ReportReason::LeaveGroup]);
    assert!(m.advance(at(60)).is_empty());
    assert!(m.is_empty());
}

#[test]
fn general_query_schedules_a_response_per_member() {
    let mut m: Membership<Igmp> = Membership::new(8, SEED);
    m.join(v4_group(), at(0)).expect("join");
    settle(&mut m); // drain the join reports

    m.on_query(None, Duration64::from_secs(2), at(100));
    // The response is scheduled within the window; nothing before it.
    let deadline = m.next_deadline().expect("scheduled");
    assert!(deadline.secs() >= 100 && deadline.secs() <= 102);
    let reports = m.advance(at(103));
    assert_eq!(reasons(&reports, v4_group()), [ReportReason::QueryResponse]);
}

#[test]
fn group_specific_query_only_targets_that_group() {
    let mut m: Membership<Igmp> = Membership::new(8, SEED);
    let a = Ipv4Addr::new(239, 0, 0, 10);
    let b = Ipv4Addr::new(239, 0, 0, 11);
    m.join(a, at(0)).expect("a");
    m.join(b, at(0)).expect("b");
    settle(&mut m);

    m.on_query(Some(a), Duration64::from_secs(1), at(50));
    let reports = m.advance(at(52));
    assert_eq!(reasons(&reports, a), [ReportReason::QueryResponse]);
    assert!(reasons(&reports, b).is_empty());
}

#[test]
fn igmp_suppresses_response_on_hearing_another_report() {
    let mut m: Membership<Igmp> = Membership::new(8, SEED);
    m.join(v4_group(), at(0)).expect("join");
    settle(&mut m);

    m.on_query(None, Duration64::from_secs(5), at(100));
    assert!(m.next_deadline().is_some());
    // Another host reports first: cancel our pending response.
    m.on_report_seen(v4_group());
    assert!(m.next_deadline().is_none());
    assert!(m.advance(at(110)).is_empty());
}

#[test]
fn mld_does_not_suppress() {
    let mut m: Membership<Mld> = Membership::new(8, SEED);
    let group = Ipv6Addr::new(0xFF15, 0, 0, 0, 0, 0, 0, 1);
    m.join(group, at(0)).expect("join");
    settle(&mut m);

    m.on_query(None, Duration64::from_secs(2), at(100));
    m.on_report_seen(group); // no effect for MLD
    let reports = m.advance(at(103));
    assert_eq!(reasons(&reports, group), [ReportReason::QueryResponse]);
}

#[test]
fn all_systems_group_is_joined_but_never_reported() {
    let mut m: Membership<Igmp> = Membership::new(8, SEED);
    let all_systems = Ipv4Addr::new(224, 0, 0, 1);
    assert!(m.join(all_systems, at(0)).expect("join"));
    assert!(m.is_member(all_systems));
    // No state-change report, and a general query does not schedule one.
    assert!(m.advance(at(0)).is_empty());
    m.on_query(None, Duration64::from_secs(1), at(1));
    assert!(m.next_deadline().is_none());
    // Leaving it just drops the record, no Leave Group message.
    assert!(m.leave(all_systems, at(2)));
    assert!(m.advance(at(2)).is_empty());
    assert!(m.is_empty());
}

#[test]
fn all_nodes_group_is_never_reported_for_mld() {
    let mut m: Membership<Mld> = Membership::new(8, SEED);
    assert!(m.join(ALL_NODES, at(0)).expect("join"));
    assert!(m.is_member(ALL_NODES));
    assert!(m.advance(at(0)).is_empty());
    m.on_query(None, Duration64::from_secs(1), at(1));
    assert!(m.next_deadline().is_none());
}

#[test]
fn capacity_is_bounded_and_fails_closed() {
    let mut m: Membership<Igmp> = Membership::new(2, SEED);
    assert!(m.join(Ipv4Addr::new(239, 0, 0, 1), at(0)).is_ok());
    assert!(m.join(Ipv4Addr::new(239, 0, 0, 2), at(0)).is_ok());
    assert_eq!(
        m.join(Ipv4Addr::new(239, 0, 0, 3), at(0)),
        Err(JoinError::CapacityExhausted)
    );
    // Re-joining an existing group never fails, even when full.
    assert!(m.join(Ipv4Addr::new(239, 0, 0, 1), at(0)).is_ok());
}

#[test]
fn leaving_a_group_never_joined_is_a_no_op() {
    let mut m: Membership<Igmp> = Membership::new(8, SEED);
    assert!(!m.leave(v4_group(), at(0)));
    assert!(m.advance(at(0)).is_empty());
}

#[test]
fn query_keeps_the_earlier_pending_response() {
    let mut m: Membership<Igmp> = Membership::new(8, SEED);
    m.join(v4_group(), at(0)).expect("join");
    settle(&mut m);

    m.on_query(None, Duration64::from_secs(1), at(100));
    let first = m.next_deadline().expect("scheduled");
    // A later, wider query must not push the response out.
    m.on_query(None, Duration64::from_secs(100), at(100));
    let second = m.next_deadline().expect("still scheduled");
    assert!(second.secs() <= first.secs() + 1);
}

#[test]
fn rejoin_while_leaving_restores_membership() {
    let mut m: Membership<Igmp> = Membership::new(8, SEED);
    m.join(v4_group(), at(0)).expect("join");
    let _ = m.advance(at(30));
    assert!(m.leave(v4_group(), at(31)));
    // Rejoin before the leave reports finish draining.
    assert!(!m.join(v4_group(), at(31)).expect("rejoin"));
    assert!(m.is_member(v4_group()));
    // A join state-change is queued again.
    let reports = m.advance(at(31));
    assert_eq!(reasons(&reports, v4_group()), [ReportReason::JoinGroup]);
}
