//! Deterministic fuzz harness for the intrusive list.
//!
//! The operation *stream* a list sees is attacker-influenced even though the
//! indices are the caller's own arena slots: a free list's link/unlink order
//! is whatever a process's allocation pattern makes it, and a recency list's
//! touch/evict order is whatever a remote peer's access pattern makes it. So
//! this drives two lists over one shared store against naive `Vec` models over
//! a random operation stream, asserting:
//!
//! 1. Each list's forward and reverse walk equals its model, after every
//!    single operation, along with its ends and its length.
//! 2. A node is on at most one list: the two models stay disjoint, and every
//!    node on neither reads back unlinked.
//! 3. An operation the model says cannot apply is *refused* — with the
//!    expected error and with the store left byte-identical.
//! 4. Nothing panics, whatever the operation order.
//!
//! The one thing it does not do is hand a list a node the *sibling* list holds
//! as an unlink target or an insertion anchor. That is the case the container
//! documents it cannot detect without a list identity in every link, so
//! driving it would be asserting against a contract the harness had broken.
//! Pushes are drawn freely, because a node another list holds is refused there
//! outright.
//!
//! Runs the fixed smoke sweep under plain `cargo test`; keeps drawing from the
//! same seeded stream until `TAIRIX_FUZZ_BUDGET_SECS` elapses under
//! `cargo xtask fuzz`.

use tairix_fuzzseed::Lcg;
use tairix_inline::intrusive::MAX_INDEX;
use tairix_inline::{IntrusiveList, Link, LinkError};

/// Rounds run per sweep. Miri interprets every operation, so it drives a
/// handful: the list itself holds no `unsafe`, and what the oracle watches
/// here is the harness's own bookkeeping.
const ROUNDS_PER_SWEEP: u32 = if cfg!(miri) { 2 } else { 200 };

/// Operations applied per round.
const OPS_PER_ROUND: u32 = if cfg!(miri) { 60 } else { 400 };

/// Nodes in the shared store. Small enough that the two lists contend for the
/// same nodes constantly, which is where the sibling-list refusals live.
const NODES: usize = 12;

/// Indices drawn from, so the two above the store exercise the absent case.
const DRAW: usize = NODES + 2;

/// One list and the order it is supposed to hold.
struct Tracked {
    list: IntrusiveList,
    model: Vec<usize>,
}

impl Tracked {
    fn new() -> Self {
        Self {
            list: IntrusiveList::new(),
            model: Vec::new(),
        }
    }

    fn position(&self, index: usize) -> Option<usize> {
        self.model.iter().position(|&node| node == index)
    }

    fn holds(&self, index: usize) -> bool {
        self.position(index).is_some()
    }

    /// Hold the list to its model, front to back and back to front.
    fn check(&self, store: &[Link]) {
        assert_eq!(
            self.list.iter(store).collect::<Vec<_>>(),
            self.model,
            "forward walk diverged"
        );
        let mut reversed = self.model.clone();
        reversed.reverse();
        assert_eq!(
            self.list.iter(store).rev().collect::<Vec<_>>(),
            reversed,
            "reverse walk diverged"
        );
        assert_eq!(self.list.len(), self.model.len());
        assert_eq!(self.list.is_empty(), self.model.is_empty());
        assert_eq!(self.list.front(), self.model.first().copied());
        assert_eq!(self.list.back(), self.model.last().copied());
    }
}

/// Draw an index the sibling list does not hold, so the operation stays inside
/// the container's contract. Indices at or above the store's node count are
/// never held, so the candidate set is never empty.
fn draw_own(prng: &mut Lcg, other: &Tracked) -> usize {
    let candidates: Vec<usize> = (0..DRAW).filter(|&node| !other.holds(node)).collect();
    candidates[prng.below(candidates.len())]
}

/// A push takes any index: one another list holds is refused outright, so the
/// full sharing is exercised here.
fn step_push(
    prng: &mut Lcg,
    mine: &mut Tracked,
    other: &Tracked,
    store: &mut [Link],
    before: &[Link],
) {
    let index = prng.below(DRAW);
    let to_front = prng.below(2) == 0;
    let outcome = if to_front {
        mine.list.push_front(store, index)
    } else {
        mine.list.push_back(store, index)
    };

    if index >= NODES {
        assert_eq!(outcome, Err(LinkError::NoSuchNode));
        assert_eq!(store, before);
    } else if mine.holds(index) || other.holds(index) {
        assert_eq!(outcome, Err(LinkError::AlreadyLinked));
        assert_eq!(store, before);
    } else {
        assert_eq!(outcome, Ok(()));
        if to_front {
            mine.model.insert(0, index);
        } else {
            mine.model.push(index);
        }
    }
}

/// Positional insertion, against an anchor the sibling list does not hold.
fn step_insert(
    prng: &mut Lcg,
    mine: &mut Tracked,
    other: &Tracked,
    store: &mut [Link],
    before: &[Link],
) {
    let anchor = draw_own(prng, other);
    let index = prng.below(DRAW);
    let after = prng.below(2) == 0;
    let at = mine.position(anchor);
    let outcome = if after {
        mine.list.insert_after(store, anchor, index)
    } else {
        mine.list.insert_before(store, anchor, index)
    };

    match (at, index >= NODES, mine.holds(index) || other.holds(index)) {
        (None, _, _) => {
            assert!(matches!(
                outcome,
                Err(LinkError::NotLinked | LinkError::NoSuchNode)
            ));
            assert_eq!(store, before);
        }
        (Some(_), true, _) => {
            assert_eq!(outcome, Err(LinkError::NoSuchNode));
            assert_eq!(store, before);
        }
        (Some(_), false, true) => {
            assert_eq!(outcome, Err(LinkError::AlreadyLinked));
            assert_eq!(store, before);
        }
        (Some(at), false, false) => {
            assert_eq!(outcome, Ok(()));
            mine.model.insert(if after { at + 1 } else { at }, index);
        }
    }
}

/// A pop from either end reports exactly the model's end node.
fn step_pop(prng: &mut Lcg, mine: &mut Tracked, store: &mut [Link]) {
    if prng.below(2) == 0 {
        let outcome = mine.list.pop_front(store);
        assert_eq!(outcome, mine.model.first().copied());
        if outcome.is_some() {
            mine.model.remove(0);
        }
    } else {
        let outcome = mine.list.pop_back(store);
        assert_eq!(outcome, mine.model.last().copied());
        if outcome.is_some() {
            mine.model.pop();
        }
    }
}

/// Unlink or touch a node the sibling list does not hold.
fn step_remove(
    prng: &mut Lcg,
    mine: &mut Tracked,
    other: &Tracked,
    store: &mut [Link],
    before: &[Link],
) {
    let index = draw_own(prng, other);
    let action = prng.below(3);
    let outcome = match action {
        0 => mine.list.unlink(store, index),
        1 => mine.list.move_to_front(store, index),
        _ => mine.list.move_to_back(store, index),
    };

    let Some(at) = mine.position(index) else {
        assert!(outcome.is_err());
        assert_eq!(store, before);
        return;
    };
    assert_eq!(outcome, Ok(()));
    mine.model.remove(at);
    match action {
        0 => assert!(!store[index].is_linked()),
        1 => mine.model.insert(0, index),
        _ => mine.model.push(index),
    }
}

/// Apply one drawn operation to whichever list it targets, and to that list's
/// model, requiring the two to agree on whether it could apply at all.
fn step(prng: &mut Lcg, lists: &mut [Tracked; 2], store: &mut [Link]) {
    let which = prng.below(2);
    let (first, second) = lists.split_at_mut(1);
    let (mine, other) = if which == 0 {
        (&mut first[0], &second[0])
    } else {
        (&mut second[0], &first[0])
    };
    let before = store.to_vec();

    match prng.below(8) {
        0..=3 => step_push(prng, mine, other, store, &before),
        4 => step_insert(prng, mine, other, store, &before),
        5 => step_pop(prng, mine, store),
        _ => step_remove(prng, mine, other, store, &before),
    }
}

/// Drive two lists over one store through a random operation stream.
fn sweep(prng: &mut Lcg) {
    let mut store = [Link::UNLINKED; NODES];
    let mut lists = [Tracked::new(), Tracked::new()];

    for _ in 0..OPS_PER_ROUND {
        step(prng, &mut lists, &mut store[..]);

        lists[0].check(&store[..]);
        lists[1].check(&store[..]);
        for (node, link) in store.iter().enumerate() {
            assert!(
                !(lists[0].holds(node) && lists[1].holds(node)),
                "node {node} reached both lists"
            );
            assert_eq!(
                link.is_linked(),
                lists[0].holds(node) || lists[1].holds(node),
                "node {node} link state diverged"
            );
        }
    }

    // Clearing detaches exactly what each list held, whatever the stream left.
    for list in &mut lists {
        assert_eq!(list.list.clear(&mut store[..]), list.model.len());
        list.model.clear();
        assert!(list.list.is_empty());
    }
    assert!(store.iter().all(|link| !link.is_linked()));
}

#[test]
fn a_reserved_index_is_always_refused() {
    let mut list = IntrusiveList::new();
    let mut store = [Link::UNLINKED; 2];

    for index in [MAX_INDEX + 1, MAX_INDEX + 2] {
        assert_eq!(
            list.push_front(&mut store[..], index),
            Err(LinkError::IndexReserved)
        );
        assert_eq!(
            list.unlink(&mut store[..], index),
            Err(LinkError::IndexReserved)
        );
        // The "already at that end" shortcut must decode the empty list's
        // ends rather than compare the caller's index against the sentinel.
        assert_eq!(
            list.move_to_front(&mut store[..], index),
            Err(LinkError::IndexReserved)
        );
        assert_eq!(
            list.move_to_back(&mut store[..], index),
            Err(LinkError::IndexReserved)
        );
    }
    assert!(list.is_empty());
    assert!(store.iter().all(|link| !link.is_linked()));
}

#[test]
fn intrusive_lists_match_their_models_and_never_panic() {
    let mut prng = Lcg::new(tairix_fuzzseed::start(
        "intrusive_lists_match_their_models_and_never_panic",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..ROUNDS_PER_SWEEP {
            sweep(&mut prng);
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}
