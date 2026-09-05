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
//! 6. A `RangeSet` agrees with a per-element set, and a `RangeMap` with a naive
//!    entry list, over `(base, count)` streams whose lengths are the ones a
//!    caller does not control — a `mem_map` page count, a run off a foreign
//!    volume — including the counts that run past the top of the key space.
//!
//! Runs the fixed smoke sweep under plain `cargo test`; keeps drawing from the
//! same seeded stream until `TAIRIX_FUZZ_BUDGET_SECS` elapses under
//! `cargo xtask fuzz`.

use std::cell::Cell;
use std::rc::Rc;

use tairix_collections::{HashMap, LruMap, RangeError, RangeKey, RangeMap, RangeSet, SmallVec};
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

/// Windows the range sweeps run over: at the bottom of the key space, deep
/// inside it, and hard against the top, so a length that runs past `u64::MAX`
/// is exercised rather than assumed impossible.
///
/// The per-step model check probes every query in the window, so the cost is
/// quadratic in it. Miri gets a narrow one: the range tier has no `unsafe`
/// core for the interpreter to find anything in, and the wide search is the
/// ordinary and budgeted runs'.
const RANGE_WINDOW: u64 = if cfg!(miri) { 6 } else { 64 };
const RANGE_BASES: [u64; 3] = [0, 1 << 40, u64::MAX - RANGE_WINDOW];

/// Draw a `(start, count)` pair, mostly inside the window and occasionally a
/// count that overruns it — the shapes a caller supplies and the container
/// must refuse rather than wrap. At the topmost base the window's end *is*
/// `u64::MAX`, so an overrunning count is also one past the key space.
fn draw_span(rng: &mut Lcg, base: u64) -> (u64, u64) {
    let start = base + rng.below(usize::try_from(RANGE_WINDOW).expect("small")) as u64;
    let count = match rng.below(8) {
        0 => 0,
        1 => u64::MAX,
        2 => RANGE_WINDOW * 4,
        _ => 1 + rng.below(9) as u64,
    };
    (start, count)
}

/// Drive a `RangeSet` against a per-element model: the definition of what a
/// set of ranges holds.
fn sweep_range_set(rng: &mut Lcg, base: u64) {
    let mut set: RangeSet<u64> = RangeSet::new();
    let mut model: Vec<bool> = std::vec![false; usize::try_from(RANGE_WINDOW).expect("small")];
    let top = base + RANGE_WINDOW;

    for step in 0..OPS_PER_ROUND {
        let (start, count) = draw_span(rng, base);
        // Both sides see the same range, clipped to the window the model can
        // track. At the topmost base that clip is `u64::MAX`, so the extreme
        // range is still exercised.
        let run = start..start.saturating_add(count).min(top);
        match rng.below(6) {
            0..=2 => {
                set.insert(run.clone());
                for element in run {
                    model[usize::try_from(element - base).expect("in window")] = true;
                }
            }
            3..=4 => {
                set.remove(run.clone());
                for element in run {
                    model[usize::try_from(element - base).expect("in window")] = false;
                }
            }
            _ => {
                let popped = set.pop_first();
                let expected = (base..top)
                    .find(|element| model[usize::try_from(element - base).expect("in window")]);
                assert_eq!(popped.map(|run| run.start), expected, "step {step}");
                if let Some(from) = expected {
                    let mut element = from;
                    while element < top
                        && model[usize::try_from(element - base).expect("in window")]
                    {
                        model[usize::try_from(element - base).expect("in window")] = false;
                        element += 1;
                    }
                }
            }
        }

        // Membership, the covered count, and the canonical form must all agree
        // with the model after every single operation.
        let mut covered = 0u64;
        let mut previous_end = None;
        for element in base..top {
            let held = model[usize::try_from(element - base).expect("in window")];
            assert_eq!(set.contains(element), held, "step {step}, at {element}");
            covered += u64::from(held);
        }
        assert_eq!(set.covered(), covered, "step {step}");
        for held in &set {
            assert!(held.start < held.end, "step {step}: an empty range");
            if let Some(previous_end) = previous_end {
                assert!(held.start > previous_end, "step {step}: not coalesced");
            }
            previous_end = Some(held.end);
        }
        // Both walk primitives answer what the model says about every query.
        for probe in base..top {
            let first = |held: bool| {
                (probe..top)
                    .find(|element| {
                        model[usize::try_from(element - base).expect("in window")] == held
                    })
                    .map(|from| {
                        let to = (from..top)
                            .find(|element| {
                                model[usize::try_from(element - base).expect("in window")] != held
                            })
                            .unwrap_or(top);
                        from..to
                    })
            };
            assert_eq!(set.first_overlap(probe..top), first(true), "step {step}");
            assert_eq!(set.first_gap(probe..top), first(false), "step {step}");
        }
    }
}

/// Drive a `RangeMap` against a naive entry list, checking the disjointness
/// refusal and that a refused value is dropped rather than leaked.
fn sweep_range_map(rng: &mut Lcg, base: u64, live: &Rc<Cell<i64>>) {
    let mut map: RangeMap<u64, Tracked> = RangeMap::new();
    let mut model: Vec<(u64, u64)> = Vec::new();
    let window = base..base + RANGE_WINDOW;

    for step in 0..OPS_PER_ROUND {
        let (start, count) = draw_span(rng, base);
        match rng.below(8) {
            0..=3 => {
                // A count that runs past the key space has no range, so the
                // degenerate empty one is what the map is handed.
                let range = start.span(count).unwrap_or(start..start);
                let overlaps = |&(held_start, held_end): &(u64, u64)| {
                    held_start < range.end && range.start < held_end
                };
                let expected = if range.start >= range.end {
                    Err(RangeError::Empty)
                } else if model.iter().any(overlaps) {
                    Err(RangeError::Overlap)
                } else {
                    Ok(())
                };
                let got = map.insert(range.clone(), Tracked::new(start, live));
                assert_eq!(got, expected, "step {step}, {start}+{count}");
                if got.is_ok() {
                    model.push((range.start, range.end));
                }
            }
            4 => {
                let removed = map.remove(start).map(|(held, _)| (held.start, held.end));
                let at = model.iter().position(|&(held, _)| held == start);
                assert_eq!(removed.is_some(), at.is_some(), "step {step}");
                if let Some(at) = at {
                    assert_eq!(removed, Some(model.swap_remove(at)));
                }
            }
            5 => {
                let covering = map.covering(start).map(|(held, _)| (held.start, held.end));
                let expected = model
                    .iter()
                    .copied()
                    .find(|&(held_start, held_end)| held_start <= start && start < held_end);
                assert_eq!(covering, expected, "step {step}, at {start}");
            }
            6 => {
                let ending = map.ending_at_or_below(start).map(|held| held.end);
                let expected = model
                    .iter()
                    .map(|&(_, held_end)| held_end)
                    .filter(|&held_end| held_end <= start)
                    .max();
                assert_eq!(ending, expected, "step {step}, at {start}");
            }
            _ => {
                // First-fit placement must land in a genuine gap of the
                // window, never over a held range.
                let placed = map.place(window.clone(), count, Tracked::new(start, live));
                if let Some(ref held) = placed {
                    assert!(held.start >= window.start && held.end <= window.end);
                    assert!(
                        !model.iter().any(|&(s, e)| s < held.end && held.start < e),
                        "step {step}: placed over a held range"
                    );
                    model.push((held.start, held.end));
                }
            }
        }

        assert_eq!(map.len(), model.len(), "step {step}");
        assert_eq!(
            i64::try_from(map.len()).expect("fits"),
            live.get(),
            "step {step}: one undropped value per live entry",
        );
        // Iteration is ascending and disjoint, whatever the stream did.
        let mut previous_end = None;
        for (held, _) in &map {
            if let Some(previous_end) = previous_end {
                assert!(held.start >= previous_end, "step {step}: out of order");
            }
            previous_end = Some(held.end);
        }
    }
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

            let base = RANGE_BASES[rng.below(RANGE_BASES.len())];
            sweep_range_set(&mut rng, base);
            let live = Rc::new(Cell::new(0i64));
            sweep_range_map(&mut rng, base, &live);
            assert_eq!(live.get(), 0, "a `RangeMap` leaked or double-dropped");
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}
