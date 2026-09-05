//! Unit tests for the neighbour cache state machine.

use super::*;
use crate::addr::Ipv4Addr;

/// A fixed hash key, so the table's layout is the same on every run.
const TEST_KEY: HashSeed = HashSeed::from_words(0x4E45_4947_4800_0001, 0x4E45_4947_4800_0002);

const MAC_A: MacAddress = MacAddress([0x02, 0, 0, 0, 0, 0xAA]);
const MAC_B: MacAddress = MacAddress([0x02, 0, 0, 0, 0, 0xBB]);

fn ip(last: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(10, 0, 2, last))
}

fn secs(s: i64) -> Duration64 {
    Duration64::from_secs(s)
}

fn table(capacity: usize) -> NeighborTable {
    NeighborTable::new(capacity, NeighborConfig::default(), TEST_KEY)
}

#[test]
fn resolution_happy_path() {
    let mut t = table(4);
    assert_eq!(t.lookup(ip(1), secs(0)), LookupResult::Pending);
    assert_eq!(t.entry(ip(1)), Some((NeighborState::Incomplete, None)));

    // The first solicitation is due immediately.
    let actions = t.advance(secs(0));
    assert_eq!(actions, [NeighborAction::SolicitMulticast { ip: ip(1) }]);

    t.confirm(ip(1), MAC_A, true, true, secs(1));
    assert_eq!(
        t.entry(ip(1)),
        Some((NeighborState::Reachable, Some(MAC_A)))
    );
    assert_eq!(t.lookup(ip(1), secs(2)), LookupResult::Send(MAC_A));

    // Reachable needs no retransmissions: nothing further is due until
    // the reachable window lapses.
    assert!(t.advance(secs(2)).is_empty());
}

#[test]
fn unsolicited_confirmation_never_creates_an_entry() {
    let mut t = table(4);
    t.confirm(ip(9), MAC_A, true, true, secs(0));
    assert!(t.is_empty());
    assert_eq!(t.entry(ip(9)), None);
}

#[test]
fn unsolicited_confirmation_of_incomplete_lands_stale() {
    let mut t = table(4);
    assert_eq!(t.lookup(ip(1), secs(0)), LookupResult::Pending);
    t.confirm(ip(1), MAC_A, false, true, secs(0));
    assert_eq!(t.entry(ip(1)), Some((NeighborState::Stale, Some(MAC_A))));
}

#[test]
fn resolution_failure_after_max_solicitations() {
    let mut t = table(4);
    let config = NeighborConfig::default();
    assert_eq!(t.lookup(ip(1), secs(0)), LookupResult::Pending);

    let mut now = 0;
    for _ in 0..config.max_multicast_solicit {
        let actions = t.advance(secs(now));
        assert_eq!(actions, [NeighborAction::SolicitMulticast { ip: ip(1) }]);
        now += config.retrans_timer.secs();
    }
    let actions = t.advance(secs(now));
    assert_eq!(actions, [NeighborAction::Unreachable { ip: ip(1) }]);
    assert!(t.is_empty());
}

#[test]
fn reachable_ages_to_stale_then_delay_then_probe() {
    let mut t = table(4);
    let config = NeighborConfig::default();
    t.lookup(ip(1), secs(0));
    t.confirm(ip(1), MAC_A, true, true, secs(0));

    // Past the reachable window a sender still gets the last known
    // address, and the entry moves to Delay.
    let after = config.reachable_time.secs() + 1;
    assert_eq!(t.lookup(ip(1), secs(after)), LookupResult::Send(MAC_A));
    assert_eq!(t.entry(ip(1)), Some((NeighborState::Delay, Some(MAC_A))));

    // The delay lapses into unicast probing.
    let probe_at = after + config.delay_first_probe.secs();
    let actions = t.advance(secs(probe_at));
    assert_eq!(
        actions,
        [NeighborAction::SolicitUnicast {
            ip: ip(1),
            mac: MAC_A
        }]
    );
    assert_eq!(t.entry(ip(1)), Some((NeighborState::Probe, Some(MAC_A))));

    // Exhausted probes discard the entry and report unreachability.
    let mut now = probe_at;
    for _ in 1..config.max_unicast_solicit {
        now += config.retrans_timer.secs();
        let actions = t.advance(secs(now));
        assert_eq!(
            actions,
            [NeighborAction::SolicitUnicast {
                ip: ip(1),
                mac: MAC_A
            }]
        );
    }
    now += config.retrans_timer.secs();
    let actions = t.advance(secs(now));
    assert_eq!(actions, [NeighborAction::Unreachable { ip: ip(1) }]);
    assert_eq!(t.entry(ip(1)), None);
}

#[test]
fn probe_confirmation_returns_to_reachable() {
    let mut t = table(4);
    let config = NeighborConfig::default();
    t.lookup(ip(1), secs(0));
    t.confirm(ip(1), MAC_A, true, true, secs(0));
    let after = config.reachable_time.secs() + 1;
    t.lookup(ip(1), secs(after));
    let probe_at = after + config.delay_first_probe.secs();
    t.advance(secs(probe_at));
    assert_eq!(t.entry(ip(1)), Some((NeighborState::Probe, Some(MAC_A))));

    t.confirm(ip(1), MAC_A, true, true, secs(probe_at));
    assert_eq!(
        t.entry(ip(1)),
        Some((NeighborState::Reachable, Some(MAC_A)))
    );
    assert!(t.advance(secs(probe_at)).is_empty());
}

#[test]
fn learn_creates_stale_and_refreshes_changed_binding() {
    let mut t = table(4);
    t.learn(ip(1), MAC_A);
    assert_eq!(t.entry(ip(1)), Some((NeighborState::Stale, Some(MAC_A))));

    // Same binding again: a Reachable entry is not downgraded.
    t.confirm(ip(1), MAC_A, true, true, secs(1));
    t.learn(ip(1), MAC_A);
    assert_eq!(
        t.entry(ip(1)),
        Some((NeighborState::Reachable, Some(MAC_A)))
    );

    // Changed binding: refreshed to Stale with the new address.
    t.learn(ip(1), MAC_B);
    assert_eq!(t.entry(ip(1)), Some((NeighborState::Stale, Some(MAC_B))));
}

#[test]
fn non_override_with_different_address_only_degrades_reachable() {
    let mut t = table(4);
    t.lookup(ip(1), secs(0));
    t.confirm(ip(1), MAC_A, true, true, secs(0));

    // Non-override, different address, Reachable entry: keep the cached
    // address, drop to Stale.
    t.confirm(ip(1), MAC_B, true, false, secs(1));
    assert_eq!(t.entry(ip(1)), Some((NeighborState::Stale, Some(MAC_A))));

    // Non-override, different address, already Stale: ignored entirely.
    t.confirm(ip(1), MAC_B, true, false, secs(2));
    assert_eq!(t.entry(ip(1)), Some((NeighborState::Stale, Some(MAC_A))));
}

#[test]
fn override_confirmation_replaces_the_address() {
    let mut t = table(4);
    t.lookup(ip(1), secs(0));
    t.confirm(ip(1), MAC_A, true, true, secs(0));

    t.confirm(ip(1), MAC_B, true, true, secs(1));
    assert_eq!(
        t.entry(ip(1)),
        Some((NeighborState::Reachable, Some(MAC_B)))
    );

    // Unsolicited override with yet another address lands Stale.
    t.confirm(ip(1), MAC_A, false, true, secs(2));
    assert_eq!(t.entry(ip(1)), Some((NeighborState::Stale, Some(MAC_A))));
}

#[test]
fn upper_layer_confirmation_marks_reachable() {
    let mut t = table(4);
    t.learn(ip(1), MAC_A);
    t.upper_layer_confirmation(ip(1), secs(1));
    assert_eq!(
        t.entry(ip(1)),
        Some((NeighborState::Reachable, Some(MAC_A)))
    );

    // No address yet: the hint cannot make an Incomplete entry usable.
    t.lookup(ip(2), secs(1));
    t.upper_layer_confirmation(ip(2), secs(1));
    assert_eq!(t.entry(ip(2)), Some((NeighborState::Incomplete, None)));
}

#[test]
fn capacity_evicts_least_recently_used_resolved_entry() {
    let mut t = table(2);
    t.learn(ip(1), MAC_A);
    t.learn(ip(2), MAC_B);

    // Touch ip(1) so ip(2) is the LRU victim.
    assert_eq!(t.lookup(ip(1), secs(2)), LookupResult::Send(MAC_A));
    t.learn(ip(3), MAC_B);
    assert_eq!(t.len(), 2);
    assert!(t.entry(ip(1)).is_some());
    assert_eq!(t.entry(ip(2)), None);
    assert!(t.entry(ip(3)).is_some());
}

#[test]
fn full_table_of_resolving_entries_fails_closed() {
    let mut t = table(2);
    assert_eq!(t.lookup(ip(1), secs(0)), LookupResult::Pending);
    assert_eq!(t.lookup(ip(2), secs(0)), LookupResult::Pending);

    // A third resolution cannot evict live resolution state.
    assert_eq!(t.lookup(ip(3), secs(0)), LookupResult::TableFull);
    // Nor can a learned binding.
    t.learn(ip(4), MAC_A);
    assert_eq!(t.entry(ip(4)), None);
    assert_eq!(t.len(), 2);
}

#[test]
fn next_deadline_tracks_the_earliest_pending_transition() {
    let mut t = table(4);
    assert_eq!(t.next_deadline(), None);

    // Stale entries have no pending transition.
    t.learn(ip(1), MAC_A);
    assert_eq!(t.next_deadline(), None);

    // A new resolution is due immediately.
    t.lookup(ip(2), secs(7));
    assert_eq!(t.next_deadline(), Some(secs(7)));

    // After soliciting, the retransmission is due one interval later.
    t.advance(secs(7));
    let retrans = NeighborConfig::default().retrans_timer.secs();
    assert_eq!(t.next_deadline(), Some(secs(7 + retrans)));
}

#[test]
fn remove_and_clear_drop_entries() {
    let mut t = table(4);
    t.learn(ip(1), MAC_A);
    t.learn(ip(2), MAC_B);
    t.remove(ip(1));
    assert_eq!(t.entry(ip(1)), None);
    assert_eq!(t.len(), 1);
    t.clear();
    assert!(t.is_empty());
}

#[test]
fn advance_is_idempotent_between_deadlines() {
    let mut t = table(4);
    t.lookup(ip(1), secs(0));
    assert_eq!(t.advance(secs(0)).len(), 1);
    // The same instant again: the retransmission is not yet due.
    assert!(t.advance(secs(0)).is_empty());
}
