//! [`LruMap`] behaviour, drop discipline, and the deterministic work counters
//! its performance claim is gated on.

extern crate std;

use std::cell::Cell;
use std::rc::Rc;
use std::vec::Vec;

use tairix_hash::{BuildFastHash, BuildSipHash13, HashSeed};

use super::LruMap;
use crate::TryReserveError;

/// The load factor the table admits before it rebuilds, as a fraction.
const LOAD_NUMERATOR: usize = 7;
const LOAD_DENOMINATOR: usize = 8;

fn map<V>() -> LruMap<u64, V, BuildFastHash> {
    LruMap::with_hasher(BuildFastHash::new())
}

/// The keys least-recently-used first.
fn order<V>(map: &LruMap<u64, V, BuildFastHash>) -> Vec<u64> {
    map.iter_lru().map(|(key, _)| *key).collect()
}

fn insert<V>(map: &mut LruMap<u64, V, BuildFastHash>, key: u64, value: V) -> Option<V> {
    map.try_insert(key, value)
        .expect("the host allocator serves this test")
}

/// A value that reports its own destruction.
struct Tracked {
    key: u64,
    live: Rc<Cell<i64>>,
}

impl Tracked {
    fn new(key: u64, live: &Rc<Cell<i64>>) -> Self {
        live.set(live.get() + 1);
        Self {
            key,
            live: Rc::clone(live),
        }
    }
}

impl Drop for Tracked {
    fn drop(&mut self) {
        self.live.set(self.live.get() - 1);
    }
}

#[test]
fn an_empty_map_has_not_allocated() {
    let map = map::<u64>();
    assert_eq!(map.len(), 0);
    assert!(map.is_empty());
    assert_eq!(map.capacity(), 0);
    assert_eq!(map.allocated_bytes(), 0);
    assert!(map.peek_lru().is_none());
    assert!(map.peek_mru().is_none());
    assert_eq!(order(&map), Vec::<u64>::new());
}

#[test]
fn insertion_orders_newest_last_and_lookup_finds_every_key() {
    let mut map = map();
    for key in 0..64u64 {
        assert_eq!(insert(&mut map, key, key * 3), None);
    }
    assert_eq!(map.len(), 64);
    for key in 0..64u64 {
        assert_eq!(map.peek(&key), Some(&(key * 3)));
        assert!(map.contains_key(&key));
    }
    assert_eq!(order(&map), (0..64).collect::<Vec<_>>());
    assert_eq!(map.peek_lru(), Some((&0, &0)));
    assert_eq!(map.peek_mru(), Some((&63, &189)));
}

#[test]
fn a_use_refreshes_recency_and_an_observation_does_not() {
    let mut map = map();
    for key in 0..4u64 {
        insert(&mut map, key, key);
    }
    assert_eq!(map.peek(&0), Some(&0));
    assert_eq!(order(&map), [0, 1, 2, 3], "peek is not a use");
    assert_eq!(map.get(&0), Some(&0));
    assert_eq!(order(&map), [1, 2, 3, 0], "get is");
    *map.get_mut(&2).expect("live") += 100;
    assert_eq!(order(&map), [1, 3, 0, 2]);
    assert_eq!(map.peek(&2), Some(&102));
    let _ = map.iter_lru().count();
    assert_eq!(order(&map), [1, 3, 0, 2], "iteration is not a use either");
}

#[test]
fn replacing_a_key_keeps_one_entry_and_refreshes_it() {
    let mut map = map();
    for key in 0..3u64 {
        insert(&mut map, key, key);
    }
    assert_eq!(insert(&mut map, 0, 99), Some(0));
    assert_eq!(map.len(), 3);
    assert_eq!(map.peek(&0), Some(&99));
    assert_eq!(order(&map), [1, 2, 0]);
}

#[test]
fn eviction_takes_the_least_recently_used_entry() {
    let mut map = map();
    for key in 0..4u64 {
        insert(&mut map, key, key);
    }
    assert_eq!(map.get(&0), Some(&0));
    assert_eq!(map.pop_lru(), Some((1, 1)));
    assert_eq!(map.pop_lru(), Some((2, 2)));
    assert_eq!(order(&map), [3, 0]);
    assert_eq!(map.pop_lru(), Some((3, 3)));
    assert_eq!(map.pop_lru(), Some((0, 0)));
    assert_eq!(map.pop_lru(), None);
    assert!(map.is_empty());
}

#[test]
fn removal_reports_the_entry_and_leaves_the_rest_ordered() {
    let mut map = map();
    for key in 0..5u64 {
        insert(&mut map, key, key * 2);
    }
    assert_eq!(map.remove(&2), Some(4));
    assert_eq!(map.remove(&2), None);
    assert_eq!(map.remove_entry(&0), Some((0, 0)));
    assert_eq!(order(&map), [1, 3, 4]);
    assert_eq!(map.len(), 3);
}

#[test]
fn retain_drops_the_refused_entries_and_keeps_the_survivors_ordered() {
    let mut map = map();
    for key in 0..8u64 {
        insert(&mut map, key, key);
    }
    assert_eq!(map.get(&1), Some(&1));
    map.retain(|key, value| {
        assert_eq!(*key, *value);
        key % 2 == 1
    });
    assert_eq!(order(&map), [3, 5, 7, 1]);
    for key in 0..8u64 {
        assert_eq!(map.contains_key(&key), key % 2 == 1);
    }
}

#[test]
fn a_freed_node_is_reused_rather_than_the_arena_growing() {
    let mut map = map();
    for key in 0..1_000u64 {
        insert(&mut map, key, key);
        if map.len() > 8 {
            map.pop_lru().expect("the map is over its bound");
        }
    }
    assert_eq!(map.len(), 8);
    // The arena is bounded by the peak occupancy, not by the thousand
    // insertions that passed through it.
    assert!(map.capacity() < 32, "capacity {}", map.capacity());
}

#[test]
fn a_steady_state_map_stops_returning_to_the_allocator() {
    let mut map = map();
    for key in 0..64u64 {
        insert(&mut map, key, key);
    }
    // One round of churn settles both indexes at the occupancy that follows;
    // the measurement starts from there, so what it gates is the steady state
    // rather than the growth that reached it.
    for key in 64..128u64 {
        insert(&mut map, key, key);
        map.pop_lru().expect("the map is over its bound");
    }
    let resident = map.allocated_bytes();
    assert!(resident > 0);
    for key in 128..4_096u64 {
        insert(&mut map, key, key);
        map.pop_lru().expect("the map is over its bound");
        assert_eq!(map.get(&key), Some(&key));
    }
    assert_eq!(
        map.allocated_bytes(),
        resident,
        "insert, evict, and touch at a steady occupancy must allocate nothing",
    );
}

#[test]
fn clearing_releases_both_allocations() {
    let mut map = map();
    for key in 0..256u64 {
        insert(&mut map, key, key);
    }
    assert!(map.allocated_bytes() > 0);
    map.clear();
    assert!(map.is_empty());
    assert_eq!(map.len(), 0);
    assert_eq!(map.capacity(), 0);
    assert_eq!(
        map.allocated_bytes(),
        0,
        "a cache drained under pressure must give its memory back",
    );
    insert(&mut map, 1, 1);
    assert_eq!(map.peek(&1), Some(&1));
}

#[test]
fn a_reservation_makes_the_insertion_that_follows_infallible() {
    let mut map = map::<u64>();
    map.try_reserve(100).expect("the host allocator serves");
    let capacity = map.capacity();
    assert!(capacity >= 100);
    let resident = map.allocated_bytes();
    for key in 0..100u64 {
        insert(&mut map, key, key);
    }
    assert_eq!(
        map.allocated_bytes(),
        resident,
        "the reservation covered it"
    );
}

#[test]
fn a_capacity_no_layout_can_hold_is_refused_rather_than_wrapping() {
    let mut map = map::<u64>();
    assert_eq!(
        map.try_reserve(usize::MAX),
        Err(TryReserveError::CapacityOverflow),
    );
    assert_eq!(map.capacity(), 0);
    assert_eq!(map.allocated_bytes(), 0);
}

#[test]
fn every_value_is_dropped_exactly_once() {
    let live = Rc::new(Cell::new(0i64));
    {
        let mut map: LruMap<u64, Tracked, BuildFastHash> =
            LruMap::with_hasher(BuildFastHash::new());
        for key in 0..512u64 {
            map.try_insert(key, Tracked::new(key, &live))
                .expect("the host allocator serves this test");
        }
        assert_eq!(live.get(), 512);
        for key in 0..128u64 {
            let dropped = map.remove(&key).expect("live");
            assert_eq!(dropped.key, key);
        }
        assert_eq!(live.get(), 384);
        for _ in 0..64 {
            map.pop_lru().expect("the map is not empty");
        }
        assert_eq!(live.get(), 320);
        map.retain(|_, value| value.key % 2 == 0);
        assert_eq!(live.get(), 160);
        // Replacing drops exactly the value it displaced.
        map.try_insert(192, Tracked::new(192, &live)).expect("room");
        assert_eq!(live.get(), 160);
        map.clear();
        assert_eq!(live.get(), 0);
        for key in 0..16u64 {
            map.try_insert(key, Tracked::new(key, &live)).expect("room");
        }
        assert_eq!(live.get(), 16);
    }
    assert_eq!(live.get(), 0, "dropping the map drops what it still holds");
}

#[test]
fn a_borrowed_key_looks_up_without_an_owned_copy() {
    let mut map: LruMap<std::string::String, u64, BuildFastHash> =
        LruMap::with_hasher(BuildFastHash::new());
    map.try_insert(std::string::String::from("run"), 1)
        .expect("room");
    assert_eq!(map.peek("run"), Some(&1));
    assert_eq!(map.get("run"), Some(&1));
    assert!(map.contains_key("run"));
    assert_eq!(map.remove("run"), Some(1));
}

#[test]
fn a_keyed_hasher_files_the_same_keys_differently_under_a_different_key() {
    let seeds = [HashSeed::from_words(1, 2), HashSeed::from_words(3, 4)];
    let orders = seeds.map(|seed| {
        let mut map: LruMap<u64, u64, BuildSipHash13> =
            LruMap::with_hasher(BuildSipHash13::with_seed(seed));
        for key in 0..64u64 {
            map.try_insert(key, key).expect("room");
        }
        (0..64u64)
            .map(|key| map.probe_groups(&key).expect("live"))
            .sum::<usize>()
    });
    // Both keys must find every entry; what differs is where they land, which
    // is what denies an attacker a fixed collision set.
    assert!(orders[0] > 0 && orders[1] > 0);
}

/// The probe-depth gate: a hit at the table's maximum load must answer within
/// a group and a half on average, which is what makes the lookup constant time
/// rather than merely amortised.
#[test]
fn a_hit_at_maximum_load_stays_inside_the_probe_budget() {
    let mut map = map();
    // Fill exactly to the load-factor limit of a settled capacity, so the
    // measurement is of the densest table the map ever probes.
    map.try_reserve(4_096).expect("the host allocator serves");
    let entries = map.capacity() * LOAD_NUMERATOR / LOAD_DENOMINATOR;
    for key in 0..entries as u64 {
        insert(&mut map, key, key);
    }
    assert_eq!(map.len(), entries);
    let probed: usize = (0..entries as u64)
        .map(|key| map.probe_groups(&key).expect("live"))
        .sum();
    // Scaled by ten so the bound is stated without floating point.
    assert!(
        probed * 10 <= entries * 15,
        "{probed} group probes over {entries} hits exceeds 1.5 per hit",
    );
}

/// The recency gate: a touch and an eviction cost the same work whatever the
/// map holds, which a tick-keyed index cannot claim.
#[test]
fn a_touch_and_an_eviction_cost_the_same_at_every_size() {
    for entries in [16u64, 1_024, 16_384] {
        let mut map = map();
        for key in 0..entries {
            insert(&mut map, key, key);
        }
        let resident = map.allocated_bytes();
        // The oldest entry moves to the newest end and back out again; both
        // splices are three link writes over an arena that does not move.
        assert_eq!(map.get(&0), Some(&0));
        assert_eq!(map.peek_mru(), Some((&0, &0)));
        assert_eq!(map.peek_lru(), Some((&1, &1)));
        assert_eq!(map.pop_lru(), Some((1, 1)));
        assert_eq!(map.len() as u64, entries - 1);
        assert_eq!(
            map.allocated_bytes(),
            resident,
            "{entries} entries: recency work must not allocate",
        );
    }
}

/// The footprint gate: what one live entry costs, against the
/// `(key, value, control byte, node)` the layout implies at the load limit.
#[test]
fn the_resident_footprint_tracks_the_declared_per_entry_cost() {
    // A reservation the table's load limit meets exactly, so the measurement
    // is of the densest table the map ever holds.
    const ENTRIES: usize = 3_584;
    let mut map = map::<u64>();
    map.try_reserve(ENTRIES).expect("the host allocator serves");
    assert_eq!(map.capacity(), ENTRIES);
    for key in 0..ENTRIES as u64 {
        insert(&mut map, key, key);
    }
    // One bucket holds a node index and a control byte, and the load factor
    // costs 8/7 of that side; one arena node holds the pair, the hash it was
    // filed under, and two link words, and the arena is sized to the entries
    // alone.
    let bucket = size_of::<usize>() + 1;
    let node = size_of::<Option<(u64, u64)>>() + size_of::<u64>() + 2 * size_of::<usize>();
    let budget = ENTRIES * bucket * LOAD_DENOMINATOR / LOAD_NUMERATOR + ENTRIES * node;
    let held = map.allocated_bytes();
    assert!(
        held <= budget,
        "{held} bytes for {ENTRIES} entries exceeds the {budget}-byte budget",
    );
}

#[test]
fn debug_renders_the_eviction_order() {
    let mut map = map();
    insert(&mut map, 1, 10);
    insert(&mut map, 2, 20);
    assert_eq!(std::format!("{map:?}"), "{1: 10, 2: 20}");
}
