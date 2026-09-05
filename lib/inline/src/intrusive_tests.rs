//! Tests for [`IntrusiveList`], including the deterministic work counter its
//! constant-time-unlink claim is gated on.

extern crate std;

use core::cell::Cell;
use core::mem::size_of;
use std::vec;
use std::vec::Vec;

use super::{IntrusiveList, Link, LinkError, LinkStore, MAX_INDEX, NONE, OFF_LIST};

/// A store that records how many nodes each operation reaches, so "constant
/// time, no search" is asserted as a work count rather than a stopwatch.
struct CountingStore {
    links: Vec<Link>,
    reads: Cell<usize>,
    writes: usize,
}

impl CountingStore {
    fn new(nodes: usize) -> Self {
        Self {
            links: vec![Link::UNLINKED; nodes],
            reads: Cell::new(0),
            writes: 0,
        }
    }

    fn reset(&mut self) {
        self.reads.set(0);
        self.writes = 0;
    }

    fn reads(&self) -> usize {
        self.reads.get()
    }
}

impl LinkStore for CountingStore {
    fn link(&self, index: usize) -> Option<&Link> {
        self.reads.set(self.reads.get() + 1);
        self.links.get(index)
    }

    fn link_mut(&mut self, index: usize) -> Option<&mut Link> {
        self.writes += 1;
        self.links.get_mut(index)
    }
}

/// Collect a list's nodes front to back.
fn order<S: LinkStore + ?Sized>(list: &IntrusiveList, store: &S) -> Vec<usize> {
    list.iter(store).collect()
}

/// A plain store of `nodes` unlinked links.
fn store(nodes: usize) -> Vec<Link> {
    vec![Link::UNLINKED; nodes]
}

#[test]
fn a_link_is_two_words_and_starts_unlinked() {
    assert_eq!(size_of::<Link>(), 2 * size_of::<usize>());
    assert_eq!(size_of::<IntrusiveList>(), 3 * size_of::<usize>());

    let link = Link::new();
    assert!(!link.is_linked());
    assert_eq!(link.prev(), None);
    assert_eq!(link.next(), None);
    assert_eq!(Link::default(), Link::UNLINKED);
}

#[test]
fn the_reserved_indices_are_above_every_nameable_node() {
    const { assert!(OFF_LIST > MAX_INDEX) };
    const { assert!(NONE > MAX_INDEX) };
    assert_eq!(Link::UNLINKED.prev(), None);
}

#[test]
fn an_empty_list_reads_back_nothing() {
    let list = IntrusiveList::new();
    let nodes = store(4);

    assert_eq!(list.len(), 0);
    assert!(list.is_empty());
    assert_eq!(list.front(), None);
    assert_eq!(list.back(), None);
    assert_eq!(order(&list, &nodes[..]), Vec::<usize>::new());
    assert_eq!(IntrusiveList::default(), list);
}

#[test]
fn pushing_at_the_front_reverses_and_at_the_back_preserves() {
    let mut front = IntrusiveList::new();
    let mut back = IntrusiveList::new();
    let mut front_nodes = store(4);
    let mut back_nodes = store(4);

    for i in 0..4 {
        front.push_front(&mut front_nodes[..], i).expect("push");
        back.push_back(&mut back_nodes[..], i).expect("push");
    }

    assert_eq!(order(&front, &front_nodes[..]), vec![3, 2, 1, 0]);
    assert_eq!(order(&back, &back_nodes[..]), vec![0, 1, 2, 3]);
    assert_eq!(front.front(), Some(3));
    assert_eq!(front.back(), Some(0));
    assert_eq!(back.front(), Some(0));
    assert_eq!(back.back(), Some(3));
    assert_eq!(front.len(), 4);
}

#[test]
fn a_fifo_queue_serves_in_arrival_order() {
    let mut list = IntrusiveList::new();
    let mut nodes = store(3);

    for i in 0..3 {
        list.push_back(&mut nodes[..], i).expect("push");
    }

    assert_eq!(list.pop_front(&mut nodes[..]), Some(0));
    assert_eq!(list.pop_front(&mut nodes[..]), Some(1));
    assert_eq!(list.pop_front(&mut nodes[..]), Some(2));
    assert_eq!(list.pop_front(&mut nodes[..]), None);
    assert!(list.is_empty());
    assert!(nodes.iter().all(|link| !link.is_linked()));
}

#[test]
fn a_recency_list_evicts_from_the_back() {
    let mut list = IntrusiveList::new();
    let mut nodes = store(3);

    for i in 0..3 {
        list.push_front(&mut nodes[..], i).expect("push");
    }
    list.move_to_front(&mut nodes[..], 0).expect("touch");

    assert_eq!(order(&list, &nodes[..]), vec![0, 2, 1]);
    assert_eq!(list.pop_back(&mut nodes[..]), Some(1));
    assert_eq!(list.pop_back(&mut nodes[..]), Some(2));
    assert_eq!(list.pop_back(&mut nodes[..]), Some(0));
    assert_eq!(list.pop_back(&mut nodes[..]), None);
}

#[test]
fn unlinking_works_at_either_end_and_in_the_middle() {
    for victim in 0..5 {
        let mut list = IntrusiveList::new();
        let mut nodes = store(5);
        for i in 0..5 {
            list.push_back(&mut nodes[..], i).expect("push");
        }

        list.unlink(&mut nodes[..], victim).expect("unlink");

        let expect: Vec<usize> = (0..5).filter(|&i| i != victim).collect();
        assert_eq!(order(&list, &nodes[..]), expect);
        assert_eq!(list.len(), 4);
        assert!(!nodes[victim].is_linked());
        assert_eq!(list.front(), expect.first().copied());
        assert_eq!(list.back(), expect.last().copied());
    }
}

#[test]
fn moving_a_node_already_at_that_end_changes_nothing() {
    let mut list = IntrusiveList::new();
    let mut nodes = store(3);
    for i in 0..3 {
        list.push_back(&mut nodes[..], i).expect("push");
    }
    let before = nodes.clone();

    list.move_to_front(&mut nodes[..], 0).expect("touch");
    list.move_to_back(&mut nodes[..], 2).expect("touch");

    assert_eq!(nodes, before);
    assert_eq!(order(&list, &nodes[..]), vec![0, 1, 2]);
}

#[test]
fn positional_insertion_lands_either_side_of_its_anchor() {
    let mut list = IntrusiveList::new();
    let mut nodes = store(5);
    list.push_back(&mut nodes[..], 0).expect("push");
    list.push_back(&mut nodes[..], 1).expect("push");

    list.insert_after(&mut nodes[..], 0, 2).expect("after head");
    list.insert_after(&mut nodes[..], 1, 3).expect("after tail");
    list.insert_before(&mut nodes[..], 0, 4)
        .expect("before head");

    assert_eq!(order(&list, &nodes[..]), vec![4, 0, 2, 1, 3]);
    assert_eq!(list.front(), Some(4));
    assert_eq!(list.back(), Some(3));
    assert_eq!(list.len(), 5);
    assert_eq!(order(&list, &nodes[..]).len(), list.len());
}

#[test]
fn insertion_before_the_tail_keeps_both_ends() {
    let mut list = IntrusiveList::new();
    let mut nodes = store(3);
    list.push_back(&mut nodes[..], 0).expect("push");
    list.push_back(&mut nodes[..], 1).expect("push");

    list.insert_before(&mut nodes[..], 1, 2)
        .expect("before tail");

    assert_eq!(order(&list, &nodes[..]), vec![0, 2, 1]);
    assert_eq!(list.back(), Some(1));
}

#[test]
fn a_node_is_on_at_most_one_list() {
    let mut first = IntrusiveList::new();
    let mut second = IntrusiveList::new();
    let mut nodes = store(2);
    first.push_back(&mut nodes[..], 0).expect("push");

    assert_eq!(
        second.push_back(&mut nodes[..], 0),
        Err(LinkError::AlreadyLinked)
    );
    assert_eq!(
        first.push_front(&mut nodes[..], 0),
        Err(LinkError::AlreadyLinked)
    );
    assert_eq!(
        first.insert_after(&mut nodes[..], 0, 0),
        Err(LinkError::AlreadyLinked)
    );
    assert!(second.is_empty());
    assert_eq!(order(&first, &nodes[..]), vec![0]);
}

#[test]
fn two_lists_over_one_store_keep_their_own_membership() {
    let mut evens = IntrusiveList::new();
    let mut odds = IntrusiveList::new();
    let mut nodes = store(6);

    for i in 0..6 {
        if i % 2 == 0 {
            evens.push_back(&mut nodes[..], i).expect("push");
        } else {
            odds.push_back(&mut nodes[..], i).expect("push");
        }
    }

    assert_eq!(order(&evens, &nodes[..]), vec![0, 2, 4]);
    assert_eq!(order(&odds, &nodes[..]), vec![1, 3, 5]);

    evens.unlink(&mut nodes[..], 2).expect("unlink");
    assert_eq!(order(&evens, &nodes[..]), vec![0, 4]);
    assert_eq!(order(&odds, &nodes[..]), vec![1, 3, 5]);

    // The sibling's node still declares a front the other list does not hold.
    assert_eq!(evens.unlink(&mut nodes[..], 1), Err(LinkError::NotLinked));
    assert_eq!(order(&odds, &nodes[..]), vec![1, 3, 5]);
}

#[test]
fn an_absent_or_reserved_index_is_refused() {
    let mut list = IntrusiveList::new();
    let mut nodes = store(2);

    assert_eq!(
        list.push_back(&mut nodes[..], 2),
        Err(LinkError::NoSuchNode)
    );
    assert_eq!(
        list.push_front(&mut nodes[..], usize::MAX),
        Err(LinkError::IndexReserved)
    );
    assert_eq!(
        list.push_front(&mut nodes[..], MAX_INDEX + 1),
        Err(LinkError::IndexReserved)
    );
    assert!(list.push_front(&mut nodes[..], MAX_INDEX).is_err());
    assert!(list.is_empty());
    assert!(nodes.iter().all(|link| !link.is_linked()));
}

#[test]
fn a_reserved_index_is_refused_by_the_move_fast_path_too() {
    let mut empty = IntrusiveList::new();
    let mut nodes = store(2);

    // An empty list's ends hold the sentinel, so the "already at that end"
    // shortcut must compare against the decoded front, not the raw word.
    for index in [MAX_INDEX + 1, MAX_INDEX + 2] {
        assert_eq!(
            empty.move_to_front(&mut nodes[..], index),
            Err(LinkError::IndexReserved)
        );
        assert_eq!(
            empty.move_to_back(&mut nodes[..], index),
            Err(LinkError::IndexReserved)
        );
    }
    assert!(empty.is_empty());
}

#[test]
fn unlinking_a_node_on_no_list_is_refused() {
    let mut list = IntrusiveList::new();
    let mut nodes = store(2);
    list.push_back(&mut nodes[..], 0).expect("push");

    assert_eq!(list.unlink(&mut nodes[..], 1), Err(LinkError::NotLinked));
    assert_eq!(list.unlink(&mut nodes[..], 2), Err(LinkError::NoSuchNode));
    assert_eq!(
        list.move_to_front(&mut nodes[..], 1),
        Err(LinkError::NotLinked)
    );
    assert_eq!(
        list.move_to_back(&mut nodes[..], 1),
        Err(LinkError::NotLinked)
    );
    assert_eq!(order(&list, &nodes[..]), vec![0]);
}

#[test]
fn an_anchor_on_no_list_is_refused() {
    let mut list = IntrusiveList::new();
    let mut nodes = store(3);
    list.push_back(&mut nodes[..], 0).expect("push");

    assert_eq!(
        list.insert_after(&mut nodes[..], 1, 2),
        Err(LinkError::NotLinked)
    );
    assert_eq!(
        list.insert_before(&mut nodes[..], 1, 2),
        Err(LinkError::NotLinked)
    );
    assert_eq!(
        list.insert_after(&mut nodes[..], 3, 2),
        Err(LinkError::NoSuchNode)
    );
    assert_eq!(order(&list, &nodes[..]), vec![0]);
    assert!(!nodes[2].is_linked());
}

#[test]
fn an_anchor_that_is_a_foreign_end_is_refused() {
    let mut owner = IntrusiveList::new();
    let mut other = IntrusiveList::new();
    let mut nodes = store(3);
    owner.push_back(&mut nodes[..], 0).expect("push");
    other.push_back(&mut nodes[..], 1).expect("push");

    // Node 1 is the other list's sole node, so it is both a front and a back
    // this list does not hold.
    assert_eq!(
        owner.insert_after(&mut nodes[..], 1, 2),
        Err(LinkError::NotLinked)
    );
    assert_eq!(
        owner.insert_before(&mut nodes[..], 1, 2),
        Err(LinkError::NotLinked)
    );
    assert!(!nodes[2].is_linked());
}

#[test]
fn a_neighbour_that_does_not_link_back_stops_the_splice() {
    let mut list = IntrusiveList::new();
    let mut nodes = store(4);
    for i in 0..3 {
        list.push_back(&mut nodes[..], i).expect("push");
    }
    // Corrupt the middle node's back-pointer, as a stray write would.
    nodes[1] = Link {
        prev: 2,
        next: nodes[1].next,
    };

    assert_eq!(list.unlink(&mut nodes[..], 1), Err(LinkError::Corrupt));
    assert_eq!(
        list.insert_after(&mut nodes[..], 0, 3),
        Err(LinkError::Corrupt)
    );
    assert!(!nodes[3].is_linked());
    assert_eq!(list.len(), 3);
}

#[test]
fn a_link_naming_an_absent_node_stops_the_splice() {
    let mut list = IntrusiveList::new();
    let mut nodes = store(2);
    list.push_back(&mut nodes[..], 0).expect("push");
    list.push_back(&mut nodes[..], 1).expect("push");
    nodes[1] = Link {
        prev: nodes[1].prev,
        next: 9,
    };

    assert_eq!(list.unlink(&mut nodes[..], 1), Err(LinkError::Corrupt));
    assert_eq!(list.len(), 2);
}

#[test]
fn iteration_is_bounded_by_the_length_even_over_a_cycle() {
    let mut list = IntrusiveList::new();
    let mut nodes = store(3);
    for i in 0..3 {
        list.push_back(&mut nodes[..], i).expect("push");
    }
    // A stray write closes the chain into a ring.
    nodes[2] = Link {
        prev: nodes[2].prev,
        next: 0,
    };

    assert_eq!(order(&list, &nodes[..]), vec![0, 1, 2]);
}

#[test]
fn the_walk_meets_in_the_middle_from_both_ends() {
    let mut list = IntrusiveList::new();
    let mut nodes = store(5);
    for i in 0..5 {
        list.push_back(&mut nodes[..], i).expect("push");
    }

    let mut walk = list.iter(&nodes[..]);
    // The remaining count is an upper bound, not a promise: the store belongs
    // to the caller, so a chain that ends early is a shorter walk.
    assert_eq!(walk.size_hint(), (0, Some(5)));
    assert_eq!(walk.next(), Some(0));
    assert_eq!(walk.next_back(), Some(4));
    assert_eq!(walk.next(), Some(1));
    assert_eq!(walk.next_back(), Some(3));
    assert_eq!(walk.size_hint(), (0, Some(1)));
    assert_eq!(walk.next_back(), Some(2));
    assert_eq!(walk.next(), None);
    assert_eq!(walk.next_back(), None);
    assert_eq!(walk.size_hint(), (0, Some(0)));

    assert_eq!(
        list.iter(&nodes[..]).rev().collect::<Vec<_>>(),
        vec![4, 3, 2, 1, 0]
    );
}

#[test]
fn a_walk_over_a_foreign_store_ends_rather_than_over_reporting() {
    let mut list = IntrusiveList::new();
    let mut nodes = store(4);
    for i in 0..4 {
        list.push_back(&mut nodes[..], i).expect("push");
    }
    let foreign = store(4);

    let mut walk = list.iter(&foreign[..]);
    assert_eq!(walk.size_hint(), (0, Some(4)));
    // The foreign store's node 0 is on no list, so its end reads as no
    // neighbour and the walk stops there rather than continuing on a count.
    assert_eq!(walk.next(), Some(0));
    assert_eq!(walk.next(), None);
    assert_eq!(walk.size_hint(), (0, Some(0)));
    assert_eq!(walk.next_back(), None);
}

#[test]
fn clearing_detaches_every_node_and_empties_the_list() {
    let mut list = IntrusiveList::new();
    let mut nodes = store(4);
    for i in 0..4 {
        list.push_back(&mut nodes[..], i).expect("push");
    }

    assert_eq!(list.clear(&mut nodes[..]), 4);
    assert!(list.is_empty());
    assert_eq!(list.front(), None);
    assert_eq!(list.back(), None);
    assert!(nodes.iter().all(|link| !link.is_linked()));
    assert_eq!(list.clear(&mut nodes[..]), 0);

    // A cleared node is free to join another list.
    let mut second = IntrusiveList::new();
    second.push_back(&mut nodes[..], 2).expect("push");
    assert_eq!(order(&second, &nodes[..]), vec![2]);
}

#[test]
fn clearing_a_corrupted_chain_still_empties_the_list() {
    let mut list = IntrusiveList::new();
    let mut nodes = store(4);
    for i in 0..4 {
        list.push_back(&mut nodes[..], i).expect("push");
    }
    nodes[1] = Link {
        prev: nodes[1].prev,
        next: 9,
    };

    assert_eq!(list.clear(&mut nodes[..]), 2);
    assert!(list.is_empty());
    assert_eq!(list.front(), None);
}

/// Nodes reached is what "constant time, no search" means, and unlike a
/// stopwatch it is reproducible: a mid-list unlink must cost the same over
/// three nodes as over ten thousand.
#[test]
fn unlink_and_touch_reach_a_constant_number_of_nodes() {
    let mut unlinks = Vec::new();
    let mut touches = Vec::new();

    for &nodes in &[3usize, 100, 10_000] {
        let mut list = IntrusiveList::new();
        let mut counting = CountingStore::new(nodes);
        for i in 0..nodes {
            list.push_back(&mut counting, i).expect("push");
        }

        counting.reset();
        list.unlink(&mut counting, nodes / 2).expect("unlink");
        unlinks.push((counting.reads(), counting.writes));

        counting.reset();
        list.move_to_front(&mut counting, nodes - 1).expect("touch");
        touches.push((counting.reads(), counting.writes));
    }

    assert_eq!(unlinks[0], unlinks[1]);
    assert_eq!(unlinks[1], unlinks[2]);
    assert_eq!(touches[0], touches[1]);
    assert_eq!(touches[1], touches[2]);
    // A mid-list unlink reaches the departing node and its two neighbours, and
    // writes exactly those three links.
    assert_eq!(unlinks[0], (3, 3));
    // A touch is that unlink at the back — one neighbour, so one fewer of
    // each — followed by a two-node push at the front.
    assert_eq!(touches[0], (4, 4));
}

#[test]
fn a_long_list_stays_consistent_through_churn() {
    const NODES: usize = 512;
    let mut list = IntrusiveList::new();
    let mut nodes = store(NODES);

    for i in 0..NODES {
        list.push_back(&mut nodes[..], i).expect("push");
    }
    // Drop every third node, then re-admit each at the front.
    for i in (0..NODES).step_by(3) {
        list.unlink(&mut nodes[..], i).expect("unlink");
    }
    for i in (0..NODES).step_by(3) {
        list.push_front(&mut nodes[..], i).expect("push");
    }

    let walk = order(&list, &nodes[..]);
    assert_eq!(walk.len(), NODES);
    assert_eq!(list.len(), NODES);
    let mut seen = walk.clone();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(seen.len(), NODES);
    assert_eq!(list.iter(&nodes[..]).rev().count(), NODES);
    assert_eq!(list.front(), Some(walk[0]));
    assert_eq!(list.back(), Some(walk[NODES - 1]));
}

#[test]
fn the_error_text_names_the_refusal() {
    use std::format;

    assert_eq!(
        format!("{}", LinkError::AlreadyLinked),
        "node is already on a list"
    );
    assert_eq!(
        format!("{}", LinkError::Corrupt),
        "the store's links are inconsistent"
    );
}
