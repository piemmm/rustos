//! Unit and property tests for routing, default routers, path MTU,
//! and source-address selection.

use super::*;
use alloc::vec::Vec;

fn v4(a: u8, b: u8, c: u8, d: u8) -> Ipv4Addr {
    Ipv4Addr::new(a, b, c, d)
}

fn p4(a: u8, b: u8, c: u8, d: u8, len: u8) -> Prefix<Ipv4Addr> {
    Prefix::new(v4(a, b, c, d), len).expect("valid prefix")
}

#[test]
fn prefix_validation_fails_closed() {
    assert!(Prefix::new(v4(10, 0, 0, 0), 8).is_some());
    assert!(Prefix::new(v4(10, 0, 0, 0), 33).is_none());
    // Host bits set.
    assert!(Prefix::new(v4(10, 0, 0, 1), 8).is_none());
    assert!(Prefix::new(v4(10, 0, 0, 1), 32).is_some());
    assert!(Prefix::new(Ipv6Addr::new(0x2001, 0xDB8, 0, 0, 0, 0, 0, 1), 64).is_none());
    assert!(Prefix::new(Ipv6Addr::new(0x2001, 0xDB8, 0, 0, 0, 0, 0, 0), 64).is_some());
    // The zero-length default prefix is valid.
    assert!(Prefix::new(v4(0, 0, 0, 0), 0).is_some());
    assert!(Prefix::new(Ipv6Addr::UNSPECIFIED, 0).is_some());
}

#[test]
fn prefix_contains_matches_leading_bits() {
    let prefix = p4(192, 168, 4, 0, 22);
    assert!(prefix.contains(v4(192, 168, 4, 1)));
    assert!(prefix.contains(v4(192, 168, 7, 255)));
    assert!(!prefix.contains(v4(192, 168, 8, 0)));
    let default = p4(0, 0, 0, 0, 0);
    assert!(default.contains(v4(255, 255, 255, 255)));
}

#[test]
fn lookup_prefers_the_longest_prefix() {
    let mut table: RoutingTable<Ipv4Addr, u32> = RoutingTable::new();
    table.insert(p4(0, 0, 0, 0, 0), Some(v4(10, 0, 0, 1)), 0);
    table.insert(p4(10, 0, 0, 0, 8), None, 1);
    table.insert(p4(10, 1, 0, 0, 16), Some(v4(10, 0, 0, 2)), 2);
    assert_eq!(table.lookup(v4(10, 1, 2, 3)).expect("matches").metadata, 2);
    assert_eq!(table.lookup(v4(10, 2, 2, 3)).expect("matches").metadata, 1);
    assert_eq!(table.lookup(v4(8, 8, 8, 8)).expect("matches").metadata, 0);
    assert!(table.is_on_link(v4(10, 2, 2, 3)));
    assert!(!table.is_on_link(v4(8, 8, 8, 8)));
}

#[test]
fn lookup_without_default_route_misses() {
    let mut table: RoutingTable<Ipv4Addr, ()> = RoutingTable::new();
    table.insert(p4(10, 0, 0, 0, 8), None, ());
    assert!(table.lookup(v4(11, 0, 0, 1)).is_none());
    assert!(!table.is_on_link(v4(11, 0, 0, 1)));
}

#[test]
fn insert_replaces_and_remove_deletes() {
    let mut table: RoutingTable<Ipv4Addr, u32> = RoutingTable::new();
    let prefix = p4(10, 0, 0, 0, 8);
    table.insert(prefix, None, 1);
    table.insert(prefix, Some(v4(10, 0, 0, 1)), 2);
    assert_eq!(table.len(), 1);
    assert_eq!(table.lookup(v4(10, 1, 1, 1)).expect("matches").metadata, 2);
    assert!(table.remove(prefix));
    assert!(table.is_empty());
    assert!(table.lookup(v4(10, 1, 1, 1)).is_none());
    assert!(!table.remove(prefix));
}

#[test]
fn removal_keeps_shorter_prefixes_reachable() {
    let mut table: RoutingTable<Ipv4Addr, u32> = RoutingTable::new();
    table.insert(p4(10, 0, 0, 0, 8), None, 1);
    table.insert(p4(10, 1, 0, 0, 16), None, 2);
    assert!(table.remove(p4(10, 1, 0, 0, 16)));
    assert_eq!(table.lookup(v4(10, 1, 2, 3)).expect("matches").metadata, 1);
}

#[test]
fn route_churn_reuses_pruned_nodes() {
    let mut table: RoutingTable<Ipv4Addr, ()> = RoutingTable::new();
    table.insert(p4(10, 0, 0, 0, 8), None, ());
    let deep = p4(10, 1, 2, 3, 32);
    table.insert(deep, None, ());
    let nodes_before = table.nodes.len();
    for _ in 0..100 {
        assert!(table.remove(deep));
        table.insert(deep, None, ());
    }
    assert_eq!(table.nodes.len(), nodes_before);
}

#[test]
fn ipv6_lookup_works_at_full_width() {
    let mut table: RoutingTable<Ipv6Addr, u32> = RoutingTable::new();
    let prefix = Prefix::new(Ipv6Addr::new(0x2001, 0xDB8, 0, 0, 0, 0, 0, 0), 64).expect("valid");
    let host = Prefix::new(Ipv6Addr::new(0x2001, 0xDB8, 0, 0, 0, 0, 0, 1), 128).expect("valid");
    table.insert(prefix, None, 1);
    table.insert(host, Some(Ipv6Addr::new(0xFE80, 0, 0, 0, 0, 0, 0, 1)), 2);
    assert_eq!(
        table
            .lookup(Ipv6Addr::new(0x2001, 0xDB8, 0, 0, 0, 0, 0, 1))
            .expect("matches")
            .metadata,
        2
    );
    assert_eq!(
        table
            .lookup(Ipv6Addr::new(0x2001, 0xDB8, 0, 0, 0, 0, 0, 2))
            .expect("matches")
            .metadata,
        1
    );
    assert!(table
        .lookup(Ipv6Addr::new(0x2001, 0xDB9, 0, 0, 0, 0, 0, 1))
        .is_none());
}

/// Deterministic LCG (the shared fuzz-harness generator shape).
struct Lcg(u64);

impl Lcg {
    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        self.0
    }
}

#[test]
fn property_lookup_matches_naive_oracle() {
    let mut rng = Lcg(0xCAFE_F00D);
    for _ in 0..50 {
        let mut table: RoutingTable<Ipv4Addr, usize> = RoutingTable::new();
        let mut oracle: Vec<(Prefix<Ipv4Addr>, usize)> = Vec::new();
        for id in 0..64usize {
            let len = (rng.next_u64() % 33) as u8;
            let raw = (rng.next_u64() & 0xFFFF_FFFF) as u32;
            let masked = if len == 0 {
                0
            } else {
                raw & (u32::MAX << (32 - u32::from(len)))
            };
            let prefix = Prefix::new(Ipv4Addr::from(masked.to_be_bytes()), len).expect("masked");
            // The oracle mirrors the table's replace-on-equal-prefix.
            oracle.retain(|(p, _)| *p != prefix);
            oracle.push((prefix, id));
            table.insert(prefix, None, id);
        }
        // Random removals.
        for _ in 0..16 {
            if oracle.is_empty() {
                break;
            }
            let victim = ((rng.next_u64() & 0xFFFF) as usize) % oracle.len();
            let (prefix, _) = oracle.swap_remove(victim);
            assert!(table.remove(prefix));
        }
        for _ in 0..256 {
            let addr = Ipv4Addr::from(((rng.next_u64() & 0xFFFF_FFFF) as u32).to_be_bytes());
            let expected = oracle
                .iter()
                .filter(|(p, _)| p.contains(addr))
                .max_by_key(|(p, _)| p.prefix_len())
                .map(|(_, id)| *id);
            let got = table.lookup(addr).map(|r| r.metadata);
            assert_eq!(got, expected, "divergence for {addr:?}");
        }
    }
}

const ROUTER_A: Ipv6Addr = Ipv6Addr::new(0xFE80, 0, 0, 0, 0, 0, 0, 0xA);
const ROUTER_B: Ipv6Addr = Ipv6Addr::new(0xFE80, 0, 0, 0, 0, 0, 0, 0xB);

#[test]
fn default_router_list_lifecycle() {
    let mut list = DefaultRouterList::new(4);
    let now = Duration64::from_secs(100);
    assert!(list.select(|_| true).is_none());
    list.update(ROUTER_A, 1800, now);
    list.update(ROUTER_B, 60, now);
    assert_eq!(list.len(), 2);
    // A zero lifetime resigns the router.
    list.update(ROUTER_B, 0, now);
    assert_eq!(list.len(), 1);
    list.update(ROUTER_B, 60, now);
    // Expiry drops ROUTER_B (60 s) but keeps ROUTER_A (1800 s).
    assert!(list.next_deadline().is_some());
    list.advance(Duration64::from_secs(200));
    assert_eq!(list.len(), 1);
    assert_eq!(list.select(|_| true), Some(ROUTER_A));
}

#[test]
fn default_router_selection_prefers_reachable_then_rotates() {
    let mut list = DefaultRouterList::new(4);
    let now = Duration64::from_secs(1);
    list.update(ROUTER_A, 1800, now);
    list.update(ROUTER_B, 1800, now);
    // A reachable router is always chosen.
    assert_eq!(list.select(|r| r == ROUTER_B), Some(ROUTER_B));
    // With none reachable, selection rotates over both.
    let first = list.select(|_| false).expect("some");
    let second = list.select(|_| false).expect("some");
    assert_ne!(first, second);
}

#[test]
fn default_router_list_is_bounded() {
    let mut list = DefaultRouterList::new(1);
    let now = Duration64::from_secs(1);
    list.update(ROUTER_A, 1800, now);
    list.update(ROUTER_B, 1800, now);
    assert_eq!(list.len(), 1);
    assert_eq!(list.select(|_| true), Some(ROUTER_A));
}

const DEST: Ipv6Addr = Ipv6Addr::new(0x2001, 0xDB8, 0, 0, 0, 0, 0, 99);

#[test]
fn path_mtu_defaults_to_link_and_learns_reductions() {
    let mut cache = PathMtuCache::new(4, Duration64::from_secs(600));
    let now = Duration64::from_secs(1);
    assert_eq!(cache.mtu(DEST, 1500, now), 1500);
    cache.packet_too_big(DEST, 1400, 1500, now);
    assert_eq!(cache.mtu(DEST, 1500, now), 1400);
    // Only reductions are accepted.
    cache.packet_too_big(DEST, 1450, 1500, now);
    assert_eq!(cache.mtu(DEST, 1500, now), 1400);
    cache.packet_too_big(DEST, 1300, 1500, now);
    assert_eq!(cache.mtu(DEST, 1500, now), 1300);
}

#[test]
fn path_mtu_never_drops_below_the_floor() {
    let mut cache = PathMtuCache::new(4, Duration64::from_secs(600));
    let now = Duration64::from_secs(1);
    cache.packet_too_big(DEST, 500, 1500, now);
    assert_eq!(cache.mtu(DEST, 1500, now), 1280);
}

#[test]
fn path_mtu_expires_back_to_link_mtu() {
    let mut cache = PathMtuCache::new(4, Duration64::from_secs(600));
    cache.packet_too_big(DEST, 1400, 1500, Duration64::from_secs(1));
    assert!(cache.next_deadline().is_some());
    cache.advance(Duration64::from_secs(700));
    assert_eq!(cache.mtu(DEST, 1500, Duration64::from_secs(700)), 1500);
    assert!(cache.next_deadline().is_none());
}

#[test]
fn path_mtu_ignores_useless_reports_and_bounds_entries() {
    let mut cache = PathMtuCache::new(1, Duration64::from_secs(600));
    let now = Duration64::from_secs(1);
    // A report no smaller than the link MTU teaches nothing.
    cache.packet_too_big(DEST, 1500, 1500, now);
    assert_eq!(cache.mtu(DEST, 1500, now), 1500);
    // Capacity 1: the second destination evicts the least recently
    // used first.
    let other = Ipv6Addr::new(0x2001, 0xDB8, 0, 0, 0, 0, 0, 100);
    cache.packet_too_big(DEST, 1400, 1500, now);
    cache.packet_too_big(other, 1300, 1500, Duration64::from_secs(2));
    assert_eq!(cache.mtu(other, 1500, Duration64::from_secs(3)), 1300);
    assert_eq!(cache.mtu(DEST, 1500, Duration64::from_secs(3)), 1500);
}

fn candidate(addr: Ipv6Addr) -> CandidateAddr {
    CandidateAddr {
        addr,
        deprecated: false,
        prefix_len: 64,
    }
}

#[test]
fn source_selection_prefers_the_destination_itself() {
    let same = candidate(DEST);
    let other = candidate(Ipv6Addr::new(0x2001, 0xDB8, 0, 0, 0, 0, 0, 1));
    assert_eq!(select_source(&[other, same], DEST), Some(DEST));
}

#[test]
fn source_selection_prefers_appropriate_scope() {
    let link_local = candidate(Ipv6Addr::new(0xFE80, 0, 0, 0, 0, 0, 0, 1));
    let global = candidate(Ipv6Addr::new(0x2001, 0xDB8, 0, 0, 0, 0, 0, 1));
    // Global destination: the link-local source is too small.
    assert_eq!(
        select_source(&[link_local, global], DEST),
        Some(global.addr)
    );
    // Link-local destination: the link-local source is the smaller
    // sufficient scope.
    let ll_dest = Ipv6Addr::new(0xFE80, 0, 0, 0, 0, 0, 0, 9);
    assert_eq!(
        select_source(&[global, link_local], ll_dest),
        Some(link_local.addr)
    );
}

#[test]
fn source_selection_avoids_deprecated() {
    let deprecated = CandidateAddr {
        deprecated: true,
        ..candidate(Ipv6Addr::new(0x2001, 0xDB8, 0, 0, 0, 0, 0, 1))
    };
    let preferred = candidate(Ipv6Addr::new(0x2001, 0xDB8, 0, 0, 0, 0, 1, 1));
    assert_eq!(
        select_source(&[deprecated, preferred], DEST),
        Some(preferred.addr)
    );
}

#[test]
fn source_selection_prefers_matching_label() {
    // Destination is ULA (label 13): the ULA source's label matches,
    // the global source's does not.
    let ula_dest = Ipv6Addr::new(0xFD00, 0, 0, 0, 0, 0, 0, 9);
    let ula = candidate(Ipv6Addr::new(0xFD00, 0xBEEF, 0, 0, 0, 0, 0, 1));
    let global = candidate(Ipv6Addr::new(0x2001, 0xDB8, 0, 0, 0, 0, 0, 1));
    assert_eq!(select_source(&[global, ula], ula_dest), Some(ula.addr));
}

#[test]
fn source_selection_uses_longest_matching_prefix_last() {
    let near = candidate(Ipv6Addr::new(0x2001, 0xDB8, 0, 0, 0, 0, 0, 1));
    let far = candidate(Ipv6Addr::new(0x2001, 0xDB8, 0xFFFF, 0, 0, 0, 0, 1));
    assert_eq!(select_source(&[far, near], DEST), Some(near.addr));
}

#[test]
fn source_selection_fails_closed_on_empty_set() {
    assert_eq!(select_source(&[], DEST), None);
}
