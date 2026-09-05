//! Deterministic fuzz harness for the heap-backed containers.
//!
//! The keys a TAIRiX hash table sees are attacker-influenced by design — a
//! filename off a foreign volume, a DNS name, a 5-tuple — so this drives a map
//! against a plain association list over key streams chosen to be adversarial
//! rather than uniform: narrow windows whose keys crowd the same groups,
//! page-aligned addresses, single-bit strides, and long insert-and-remove runs
//! that saturate the table with tombstones. The invariants are:
//!
//! 1. The map agrees with the naive reference after every single operation.
//! 2. Nothing panics, whatever the operation order or key stream.
//! 3. A tombstone never hides a live entry.
//! 4. Every value is dropped exactly once, including the ones a rebuild moved,
//!    and every `SmallVec` element exactly once across its spill.
//! 5. An `LruMap` agrees with a naive recency list — the same membership, and
//!    the same victim on every eviction — across the same adversarial key
//!    streams, with its node arena bounded by the peak occupancy however long
//!    the churn runs.
//!
//! Runs the fixed smoke sweep under plain `cargo test`; keeps drawing from the
//! same seeded stream until `TAIRIX_FUZZ_BUDGET_SECS` elapses under
//! `cargo xtask fuzz`.

use std::cell::Cell;
use std::rc::Rc;

use tairix_collections::{HashMap, LruMap, SmallVec};
use tairix_fuzzseed::Lcg;
use tairix_hash::{BuildSipHash13, HashSeed};

/// Inline bound the `SmallVec` under test is built at, so the stream crosses
/// its spill repeatedly rather than settling on one storage.
const INLINE: usize = 8;

/// Rounds run per sweep of the loop below. Miri interprets every operation, so
/// it drives a handful of rounds: it is hunting undefined behaviour in the
/// table's unsafe core, which one round already exercises, and the wide input
/// search is the ordinary and budgeted runs'.
const ROUNDS_PER_SWEEP: u32 = if cfg!(miri) { 2 } else { 200 };

/// Operations applied per round.
const OPS_PER_ROUND: u32 = if cfg!(miri) { 150 } else { 400 };

/// A value that reports its own destruction, so a leaked or doubly-dropped
/// entry is a failure rather than a silence.
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

/// Draw a key from one of four streams, three of them chosen to collide.
///
/// The control tag comes from the hash's top bits and the group index from its
/// low bits, so a uniform stream exercises neither a tag collision nor a long
/// probe chain. Narrow, aligned, and strided streams do.
fn draw_key(rng: &mut Lcg, width: usize) -> u64 {
    match rng.below(4) {
        0 => rng.below(width) as u64,
        1 => (rng.below(4096) as u64) << 12,
        2 => 1u64 << rng.below(64),
        _ => rng.next_u64(),
    }
}

fn run_round(rng: &mut Lcg, seed: HashSeed) {
    let live = Rc::new(Cell::new(0i64));
    let mut map: HashMap<u64, Tracked, BuildSipHash13> =
        HashMap::with_hasher(BuildSipHash13::with_seed(seed));
    // The reference is a plain list searched linearly: slow, and obviously
    // right, which is the whole point of it.
    let mut reference: Vec<u64> = Vec::new();
    let width = 1 + rng.below(512);

    for step in 0..OPS_PER_ROUND {
        let key = draw_key(rng, width);
        match rng.below(8) {
            0..=3 => {
                let replaced = map
                    .try_insert(key, Tracked::new(key, &live))
                    .expect("the host allocator serves this harness");
                let known = reference.contains(&key);
                assert_eq!(replaced.is_some(), known, "step {step}, key {key}");
                assert!(replaced.is_none_or(|value| value.key == key));
                if !known {
                    reference.push(key);
                }
            }
            4..=5 => {
                let removed = map.remove(&key);
                let position = reference.iter().position(|k| *k == key);
                assert_eq!(removed.is_some(), position.is_some(), "step {step}");
                assert!(removed.is_none_or(|value| value.key == key));
                if let Some(position) = position {
                    reference.swap_remove(position);
                }
            }
            6 => {
                let found = map.get(&key);
                assert_eq!(
                    found.is_some(),
                    reference.contains(&key),
                    "step {step}, key {key}",
                );
                assert!(found.is_none_or(|value| value.key == key));
            }
            _ => {
                map.retain(|key, value| {
                    assert_eq!(*key, value.key);
                    key % 3 != 0
                });
                reference.retain(|key| key % 3 != 0);
            }
        }

        assert_eq!(map.len(), reference.len(), "step {step}");
        assert_eq!(
            i64::try_from(map.len()).expect("fits"),
            live.get(),
            "step {step}: one undropped value per live entry",
        );
    }

    // Every survivor must still be reachable through whatever tombstones the
    // round left behind, and must cost a bounded probe to reach.
    for key in &reference {
        assert!(map.get(key).is_some(), "key {key} lost");
        let groups = map.probe_groups(key).expect("live");
        assert!(groups >= 1 && groups <= map.capacity(), "key {key}");
    }
    drop(map);
    assert_eq!(live.get(), 0, "every value must be dropped exactly once");
}

/// Drive a `SmallVec` across its spill against a `Vec` model.
fn sweep_smallvec(prng: &mut Lcg, live: &Rc<Cell<i64>>) {
    let mut vec: SmallVec<Tracked, INLINE> = SmallVec::new();
    let mut model: Vec<u64> = Vec::new();
    let mut spilled = false;
    for key in 0..u64::from(OPS_PER_ROUND) {
        match prng.next_u64() % 5 {
            0..=2 => {
                vec.try_push(Tracked::new(key, live))
                    .expect("the host heap");
                model.push(key);
            }
            3 => assert_eq!(vec.pop().map(|t| t.key), model.pop()),
            _ => {
                let index = usize::try_from(prng.next_u64() % 20).expect("small");
                let taken = vec.remove(index).map(|t| t.key);
                let expect = (index < model.len()).then(|| model.remove(index));
                assert_eq!(taken, expect);
            }
        }
        assert_eq!(vec.len(), model.len(), "length diverged");
        assert!(
            vec.iter().map(|t| t.key).eq(model.iter().copied()),
            "contents diverged"
        );
        assert_eq!(vec.spilled(), model.len() > INLINE || spilled);
        // Once spilled the vector stays spilled, whatever it shrinks back to.
        spilled |= vec.spilled();
    }
}

/// Drive an `LruMap` against a naive `(membership, recency order)` model.
///
/// The model is a plain vector of keys, oldest first, searched linearly — the
/// definition of the order the map claims to keep in constant time.
fn sweep_lru(rng: &mut Lcg, seed: HashSeed, live: &Rc<Cell<i64>>) {
    let mut map: LruMap<u64, Tracked, BuildSipHash13> =
        LruMap::with_hasher(BuildSipHash13::with_seed(seed));
    let mut order: Vec<u64> = Vec::new();
    let width = 1 + rng.below(256);
    // The bound the churn is held to, so eviction is exercised throughout.
    let bound = 1 + rng.below(64);

    let touch = |order: &mut Vec<u64>, key: u64| {
        if let Some(at) = order.iter().position(|held| *held == key) {
            order.remove(at);
            order.push(key);
        }
    };

    for step in 0..OPS_PER_ROUND {
        let key = draw_key(rng, width);
        match rng.below(8) {
            0..=3 => {
                let replaced = map
                    .try_insert(key, Tracked::new(key, live))
                    .expect("the host allocator serves this harness");
                let known = order.contains(&key);
                assert_eq!(replaced.is_some(), known, "step {step}, key {key}");
                assert!(replaced.is_none_or(|value| value.key == key));
                touch(&mut order, key);
                if !known {
                    order.push(key);
                }
            }
            4 => {
                let found = map.get(&key).map(|value| value.key);
                assert_eq!(found.is_some(), order.contains(&key), "step {step}");
                assert!(found.is_none_or(|found| found == key));
                touch(&mut order, key);
            }
            5 => {
                let peeked = map.peek(&key).map(|value| value.key);
                assert_eq!(peeked.is_some(), order.contains(&key), "step {step}");
            }
            6 => {
                let removed = map.remove(&key).map(|value| value.key);
                let at = order.iter().position(|held| *held == key);
                assert_eq!(removed.is_some(), at.is_some(), "step {step}");
                assert!(removed.is_none_or(|found| found == key));
                if let Some(at) = at {
                    order.remove(at);
                }
            }
            _ => {
                let evicted = map.pop_lru().map(|(key, _)| key);
                let expected = (!order.is_empty()).then(|| order.remove(0));
                assert_eq!(evicted, expected, "step {step}: wrong victim");
            }
        }
        while order.len() > bound {
            let evicted = map.pop_lru().map(|(key, _)| key);
            assert_eq!(evicted, Some(order.remove(0)), "step {step}: wrong victim");
        }

        assert_eq!(map.len(), order.len(), "step {step}");
        assert!(
            map.iter_lru()
                .map(|(key, _)| *key)
                .eq(order.iter().copied()),
            "step {step}: recency order diverged",
        );
        assert_eq!(
            i64::try_from(map.len()).expect("fits"),
            live.get(),
            "step {step}: one undropped value per live entry",
        );
    }

    // The arena is bounded by the peak occupancy: a freed node is reused, so
    // a long churn does not grow it once the bound is reached.
    assert!(
        map.capacity() <= 2 * bound + INLINE,
        "arena grew to {} for a bound of {bound}",
        map.capacity(),
    );
    drop(map);
    assert_eq!(live.get(), 0, "every value must be dropped exactly once");
}

#[test]
fn heap_backed_containers_agree_with_their_models() {
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "heap_backed_containers_agree_with_their_models",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..ROUNDS_PER_SWEEP {
            // A fresh hash key per round, so no one table layout is ever the
            // only one exercised.
            let key = HashSeed::from_words(rng.next_u64(), rng.next_u64());
            run_round(&mut rng, key);

            let live = Rc::new(Cell::new(0i64));
            sweep_smallvec(&mut rng, &live);
            assert_eq!(live.get(), 0, "a `SmallVec` leaked or double-dropped");

            let live = Rc::new(Cell::new(0i64));
            sweep_lru(&mut rng, key, &live);
            assert_eq!(live.get(), 0, "an `LruMap` leaked or double-dropped");
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}
