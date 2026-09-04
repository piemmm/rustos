//! Tests for [`HashMap`] and [`HashSet`], including the deterministic work
//! counters the performance claims are gated on.

extern crate std;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_hash::{BuildFastHash, BuildSipHash13, HashSeed};

use crate::group::GROUP_LEN;
use crate::test_support::Counted;
use crate::{HashMap, HashSet, TryReserveError};

/// A keyed builder for tests, so the maps under test hash the way a
/// production map over untrusted keys does without touching the global
/// publication cell.
fn keyed() -> BuildSipHash13 {
    BuildSipHash13::with_seed(HashSeed::from_words(
        0x5ea1_5ea1_5ea1_5ea1,
        0x0f0e_0d0c_0b0a_0908,
    ))
}

fn map() -> HashMap<u64, u64, BuildSipHash13> {
    HashMap::with_hasher(keyed())
}

/// Sweep length: the stated count normally, a bounded slice under Miri, which
/// interprets every operation and is hunting undefined behaviour rather than a
/// rare key collision. The full-length run is the ordinary test stage's.
const fn sweep(full: u64) -> u64 {
    if cfg!(miri) && full > 300 {
        300
    } else {
        full
    }
}

#[test]
fn an_empty_map_holds_no_memory_and_finds_nothing() {
    let map = map();
    assert_eq!(map.len(), 0);
    assert!(map.is_empty());
    assert_eq!(map.capacity(), 0);
    assert_eq!(map.allocated_bytes(), 0);
    assert_eq!(map.get(&1), None);
    assert_eq!(map.iter().count(), 0);
}

#[test]
fn insert_replaces_and_reports_the_previous_value() {
    let mut map = map();
    assert_eq!(map.try_insert(1, 10), Ok(None));
    assert_eq!(map.try_insert(1, 11), Ok(Some(10)));
    assert_eq!(map.len(), 1);
    assert_eq!(map.get(&1), Some(&11));
}

#[test]
fn a_populated_map_answers_every_key_it_was_given() {
    let mut map = map();
    let entries = sweep(2_000);
    for key in 0..entries {
        assert_eq!(map.try_insert(key, key * 3), Ok(None));
    }
    assert_eq!(map.len() as u64, entries);
    for key in 0..entries {
        assert_eq!(map.get(&key), Some(&(key * 3)), "key {key}");
    }
    assert_eq!(map.get(&entries), None);
}

#[test]
fn removal_leaves_every_other_key_reachable() {
    let mut map = map();
    for key in 0..1_000u64 {
        assert_eq!(map.try_insert(key, key), Ok(None));
    }
    for key in (0..1_000u64).step_by(3) {
        assert_eq!(map.remove(&key), Some(key));
    }
    for key in 0..1_000u64 {
        let expected = if key % 3 == 0 { None } else { Some(&key) };
        assert_eq!(map.get(&key), expected, "key {key}");
    }
    assert_eq!(map.remove(&0), None, "a second removal finds nothing");
}

/// A tombstone must not end a probe chain. Churning far more entries than the
/// table holds at once fills it with tombstones, and every surviving key must
/// still be found.
#[test]
fn churn_through_tombstones_keeps_every_survivor_reachable() {
    let mut map = map();
    let mut live: Vec<u64> = Vec::new();
    for round in 0..sweep(20_000) {
        assert_eq!(map.try_insert(round, round), Ok(None));
        live.push(round);
        if live.len() > 64 {
            let victim = live.remove(0);
            assert_eq!(map.remove(&victim), Some(victim));
        }
        if round % 997 == 0 {
            for key in &live {
                assert_eq!(map.get(key), Some(key), "key {key} lost in round {round}");
            }
        }
    }
    assert_eq!(map.len(), live.len());
    for key in &live {
        assert_eq!(map.get(key), Some(key));
    }
}

/// A table churned entirely through tombstones must not grow without bound:
/// the rebuild that clears them is what keeps the footprint proportional to
/// the live set.
#[test]
fn churn_does_not_grow_the_table_without_bound() {
    let mut map = map();
    for key in 0..64u64 {
        assert_eq!(map.try_insert(key, key), Ok(None));
    }
    let settled = map.allocated_bytes();
    for round in 64..sweep(40_000) {
        assert_eq!(map.try_insert(round, round), Ok(None));
        assert_eq!(map.remove(&(round - 64)), Some(round - 64));
    }
    assert_eq!(map.len(), 64);
    assert_eq!(
        map.allocated_bytes(),
        settled,
        "a steady live set must settle on a steady footprint",
    );
}

#[test]
fn borrowed_keys_look_up_without_allocating_an_owned_key() {
    let mut map: HashMap<String, u32, BuildSipHash13> = HashMap::with_hasher(keyed());
    assert_eq!(map.try_insert("alpha".to_string(), 1), Ok(None));
    assert_eq!(map.get("alpha"), Some(&1));
    assert!(map.contains_key("alpha"));
    assert_eq!(map.get("beta"), None);
    assert_eq!(map.remove("alpha"), Some(1));
}

#[test]
fn iteration_visits_every_entry_exactly_once() {
    let mut map = map();
    for key in 0..500u64 {
        assert_eq!(map.try_insert(key, key * 2), Ok(None));
    }
    let mut seen: Vec<(u64, u64)> = map.iter().map(|(k, v)| (*k, *v)).collect();
    seen.sort_unstable();
    assert_eq!(seen.len(), 500);
    for (index, (key, value)) in seen.into_iter().enumerate() {
        assert_eq!(key, index as u64);
        assert_eq!(value, key * 2);
    }
    assert_eq!(map.iter().len(), 500, "the size hint must be exact");
}

#[test]
fn mutable_iteration_reaches_every_value() {
    let mut map = map();
    for key in 0..200u64 {
        assert_eq!(map.try_insert(key, 0), Ok(None));
    }
    for (key, value) in &mut map {
        *value = *key + 1;
    }
    for key in 0..200u64 {
        assert_eq!(map.get(&key), Some(&(key + 1)));
    }
    for value in map.values_mut() {
        *value = 0;
    }
    assert!(map.values().all(|v| *v == 0));
}

#[test]
fn owning_iteration_yields_every_entry() {
    let mut full = map();
    for key in 0..100u64 {
        assert_eq!(full.try_insert(key, key), Ok(None));
    }
    let mut keys: Vec<u64> = full.into_iter().map(|(k, _)| k).collect();
    keys.sort_unstable();
    assert_eq!(keys, (0..100u64).collect::<Vec<_>>());
}

/// Abandoning an owning iterator part-way must drop the entries it never
/// yielded, exactly once each.
#[test]
fn abandoning_an_owning_iterator_drops_what_is_left() {
    let drops = Counted::counter();
    let mut partial: HashMap<u64, Counted, BuildSipHash13> = HashMap::with_hasher(keyed());
    for key in 0..100u64 {
        assert!(partial.try_insert(key, Counted::new(&drops)).is_ok());
    }
    let mut drain = partial.into_iter();
    for _ in 0..10 {
        drop(drain.next().expect("an entry"));
    }
    assert_eq!(drops.get(), 10);
    drop(drain);
    assert_eq!(drops.get(), 100);
}

#[test]
fn retain_keeps_exactly_the_accepted_entries() {
    let mut map = map();
    for key in 0..300u64 {
        assert_eq!(map.try_insert(key, key), Ok(None));
    }
    map.retain(|key, value| {
        *value += 1_000;
        key % 5 == 0
    });
    assert_eq!(map.len(), 60);
    for key in 0..300u64 {
        let expected = if key % 5 == 0 {
            Some(&(key + 1_000))
        } else {
            None
        };
        assert_eq!(map.get(&key), expected, "key {key}");
    }
}

#[test]
fn clear_drops_the_entries_and_keeps_the_allocation() {
    let mut map = map();
    for key in 0..100u64 {
        assert_eq!(map.try_insert(key, key), Ok(None));
    }
    let bytes = map.allocated_bytes();
    map.clear();
    assert_eq!(map.len(), 0);
    assert_eq!(map.get(&0), None);
    assert_eq!(map.allocated_bytes(), bytes);
    assert_eq!(map.try_insert(1, 1), Ok(None));
    assert_eq!(map.get(&1), Some(&1));
}

/// Every stored value must be dropped exactly once, whether the map is
/// dropped, cleared, overwritten, removed from, or rebuilt by growth.
#[test]
fn every_value_is_dropped_exactly_once() {
    let entries = sweep(500);
    let drops = Counted::counter();
    {
        let mut map: HashMap<u64, Counted, BuildSipHash13> = HashMap::with_hasher(keyed());
        for key in 0..entries {
            assert!(map.try_insert(key, Counted::new(&drops)).is_ok());
        }
        assert_eq!(drops.get(), 0, "growth must move values, never drop them");

        assert!(map.try_insert(0, Counted::new(&drops)).is_ok());
        assert_eq!(drops.get(), 1, "an overwrite drops the value it replaced");

        drop(map.remove(&1));
        assert_eq!(drops.get(), 2);

        // The odd keys, less the one already removed.
        let evens = usize::try_from(entries / 2).expect("fits");
        map.retain(|key, _| *key % 2 == 0);
        assert_eq!(map.len(), evens);
        assert_eq!(drops.get(), evens + 1);
    }
    // Every key inserted, plus the one that overwrote key 0, dropped once.
    assert_eq!(
        drops.get(),
        usize::try_from(entries).expect("fits") + 1,
        "one drop per value ever inserted",
    );
}

#[test]
fn a_reservation_makes_the_following_insertions_free_of_growth() {
    let mut map = map();
    assert_eq!(map.try_reserve(1_000), Ok(()));
    assert!(map.capacity() >= 1_000);
    let bytes = map.allocated_bytes();
    for key in 0..1_000u64 {
        assert_eq!(map.try_insert(key, key), Ok(None));
    }
    assert_eq!(map.allocated_bytes(), bytes, "the reservation must suffice");
}

#[test]
fn an_unrepresentable_capacity_is_refused_rather_than_wrapped() {
    let mut map = map();
    assert_eq!(
        map.try_reserve(usize::MAX),
        Err(TryReserveError::CapacityOverflow),
    );
    assert_eq!(map.len(), 0);
    assert_eq!(map.allocated_bytes(), 0, "a refusal allocates nothing");
}

#[test]
fn the_hasher_choice_changes_the_layout_but_never_the_answers() {
    let mut keyed_map = map();
    let mut fast: HashMap<u64, u64, BuildFastHash> = HashMap::with_hasher(BuildFastHash::new());
    for key in 0..200u64 {
        assert_eq!(keyed_map.try_insert(key, key), Ok(None));
        assert_eq!(fast.try_insert(key, key), Ok(None));
    }
    for key in 0..200u64 {
        assert_eq!(keyed_map.get(&key), fast.get(&key));
    }
    let keyed_order: Vec<u64> = keyed_map.keys().copied().collect();
    let fast_order: Vec<u64> = fast.keys().copied().collect();
    assert_ne!(
        keyed_order, fast_order,
        "two different hashers must lay two tables out differently",
    );
}

// ---- The deterministic work counters ------------------------------------

/// Fill a map to just under its load-factor limit without letting it grow, so
/// the counters below measure the table at its densest.
fn saturated(entries: u64) -> HashMap<u64, u64, BuildSipHash13> {
    let mut map = map();
    map.try_reserve(usize::try_from(entries).expect("fits"))
        .expect("reserve");
    let capacity = map.capacity() as u64;
    for key in 0..capacity {
        assert_eq!(map.try_insert(key, key), Ok(None));
    }
    assert_eq!(map.capacity(), map.len(), "the table must be at its limit");
    map
}

/// The probe gate: at the maximum load factor a hit must cost no more than
/// 1.5 control-group scans on average.
#[test]
fn a_hit_costs_at_most_one_and_a_half_group_scans_at_maximum_load() {
    let map = saturated(sweep(4_000));
    let mut total = 0usize;
    for key in 0..map.len() as u64 {
        total += map.probe_groups(&key).expect("every key is live");
    }
    let live = map.len();
    assert!(
        total * 2 <= live * 3,
        "{total} group scans over {live} hits exceeds 1.5 per hit",
    );
}

/// The footprint gate: at the maximum load factor the table costs no more than
/// 1.15x an entry and its control byte, per live entry. The 8/7 the load
/// factor itself imposes is the whole of it; there is no per-entry node, no
/// partially filled node, and no side table.
#[test]
fn a_saturated_table_costs_at_most_one_and_a_sixth_per_entry() {
    let map = saturated(sweep(4_000));
    let per_entry = core::mem::size_of::<(u64, u64)>() + 1;
    let budget = map.len() * per_entry * 115 / 100;
    assert!(
        map.allocated_bytes() <= budget,
        "{} bytes for {} entries exceeds the {budget}-byte budget",
        map.allocated_bytes(),
        map.len(),
    );
}

/// A lookup, an iteration, and a removal must not touch the allocator. The
/// footprint standing still across all three is the observable form of that.
#[test]
fn read_and_remove_paths_never_allocate() {
    let mut map = saturated(sweep(1_000));
    let bytes = map.allocated_bytes();
    for key in 0..map.len() as u64 {
        assert!(map.get(&key).is_some());
    }
    assert_eq!(map.iter().count(), map.len());
    let live = map.len() as u64;
    for key in 0..live {
        assert!(map.remove(&key).is_some());
    }
    assert_eq!(map.allocated_bytes(), bytes);
}

/// The smallest table is one whole group, so a map holding a single entry
/// still probes one aligned group and nothing more.
#[test]
fn the_smallest_table_is_one_group() {
    let mut map = map();
    assert_eq!(map.try_insert(1, 1), Ok(None));
    assert_eq!(map.capacity(), GROUP_LEN - GROUP_LEN / 8);
    assert_eq!(map.probe_groups(&1), Some(1));
}

// ---- The set --------------------------------------------------------------

#[test]
fn a_set_holds_each_member_once() {
    let mut set: HashSet<u64, BuildSipHash13> = HashSet::with_hasher(keyed());
    assert!(set.is_empty());
    assert_eq!(set.try_insert(1), Ok(true));
    assert_eq!(set.try_insert(1), Ok(false));
    assert_eq!(set.len(), 1);
    assert!(set.contains(&1));
    assert!(set.remove(&1));
    assert!(!set.remove(&1));
    assert!(set.is_empty());
}

#[test]
fn a_set_costs_no_more_than_its_members() {
    let mut set: HashSet<u64, BuildSipHash13> = HashSet::with_hasher(keyed());
    let mut map = map();
    for member in 0..1_000u64 {
        assert_eq!(set.try_insert(member), Ok(true));
        assert_eq!(map.try_insert(member, 0u64), Ok(None));
    }
    assert!(
        set.allocated_bytes() < map.allocated_bytes(),
        "a zero-sized value must cost nothing",
    );
}

#[test]
fn set_iteration_and_retention_agree_with_membership() {
    let mut set: HashSet<u64, BuildSipHash13> = HashSet::with_hasher(keyed());
    for member in 0..300u64 {
        assert_eq!(set.try_insert(member), Ok(true));
    }
    set.retain(|member| member % 4 == 0);
    let mut members: Vec<u64> = set.iter().copied().collect();
    members.sort_unstable();
    assert_eq!(
        members,
        (0..300u64).filter(|m| m % 4 == 0).collect::<Vec<_>>()
    );

    let mut owned: Vec<u64> = set.into_iter().collect();
    owned.sort_unstable();
    assert_eq!(owned, members);
}

#[test]
fn a_set_of_borrowed_keys_matches_by_slice() {
    let mut set: HashSet<String, BuildSipHash13> = HashSet::with_hasher(keyed());
    assert_eq!(set.try_insert("gamma".to_string()), Ok(true));
    assert!(set.contains("gamma"));
    assert_eq!(set.get("gamma").map(String::as_str), Some("gamma"));
    assert_eq!(set.take("gamma").as_deref(), Some("gamma"));
    assert!(set.is_empty());
}
