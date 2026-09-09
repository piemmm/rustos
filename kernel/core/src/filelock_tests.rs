//! Host tests for the advisory byte-range lock manager.
//!
//! The record algebra is checked twice over: by hand for the cases that
//! carry a rule (a split, a merge, an upgrade, a downgrade), and by a
//! randomised run against a per-byte model, which is what makes the
//! interval bookkeeping trustworthy rather than merely plausible.

use alloc::vec::Vec;

use tairix_abi::{Errno, LockMode, LockRange, LOCK_LEN_TO_END};
use tairix_kernel_sec::ProcessId;
use tairix_rng::{FastRng, RandU64};

use super::{
    acquire, dequeue, enqueue, held_ranges, invariants_hold, live_records, locks_present,
    mint_owner, query, registry_guard, release, release_owner, usage, Conflict, Held, OwnerId,
    Refusal, Request,
};
use tairix_abi::FileId;

/// A distinct file identity per test.
fn file(node: u64) -> FileId {
    FileId {
        volume: [7u8; 16],
        node,
    }
}

const PID: ProcessId = ProcessId(11);
const OTHER_PID: ProcessId = ProcessId(12);
/// A bound generous enough that only the cases about the bound reach it.
const ROOMY: u64 = 4096;

/// A byte offset as an index into a per-byte model, which the model's own
/// bounded span guarantees fits.
fn index_of(byte: u64) -> usize {
    usize::try_from(byte).expect("the model span fits a usize")
}

/// The byte space the single-owner randomised run works over. Small on
/// purpose: every operation then overlaps others heavily, which is where the
/// split, merge and convert paths actually interact.
const SINGLE_OWNER_SPAN: usize = 48;

/// The byte space the multi-owner randomised run works over.
const MULTI_OWNER_SPAN: usize = 32;

/// That span as the byte counter the ranges are drawn from.
fn span_bound(span: usize) -> u64 {
    u64::try_from(span).expect("a small span")
}

fn range(start: u64, end_inclusive: u64) -> LockRange {
    LockRange::between(start, end_inclusive).expect("well-formed span")
}

/// Take `held` over `[start, end]` without waiting, expecting it to be
/// granted.
fn take(file: FileId, owner: OwnerId, pid: ProcessId, task: u64, range: LockRange, held: Held) {
    acquire(&Request {
        file,
        owner,
        pid,
        task,
        range,
        held,
        limit: ROOMY,
        waiting: false,
    })
    .expect("granted");
}

/// The mode covering each byte of `0..len` for `owner`, as the records say.
fn coverage(f: FileId, owner: OwnerId, len: u64) -> Vec<Option<Held>> {
    let records = held_ranges(f, owner);
    (0..len)
        .map(|byte| {
            records
                .iter()
                .find(|&&(start, end, _)| start <= byte && byte <= end)
                .map(|&(_, _, held)| held)
        })
        .collect()
}

#[test]
fn an_owner_never_conflicts_with_itself_however_the_ranges_overlap() {
    let _serial = registry_guard();
    let f = file(1);
    let owner = mint_owner();
    take(f, owner, PID, 1, LockRange::WHOLE, Held::Exclusive);
    // A second descriptor on the same description is the same owner, which
    // is what makes a duplicated or inherited handle usable.
    take(f, owner, PID, 1, range(10, 20), Held::Exclusive);
    take(f, owner, PID, 2, range(0, 5), Held::Shared);
    assert!(invariants_hold(f));
}

#[test]
fn two_opens_of_one_file_are_two_owners_and_do_conflict() {
    let _serial = registry_guard();
    let f = file(2);
    let (first, second) = (mint_owner(), mint_owner());
    take(f, first, PID, 1, LockRange::WHOLE, Held::Exclusive);
    // Same process, same thread, a second `fs_open`: a distinct owner. This
    // is the whole reason the owner is the description and not the process.
    let refused = acquire(&Request {
        file: f,
        owner: second,
        pid: PID,
        task: 1,
        range: range(0, 0),
        held: Held::Shared,
        limit: ROOMY,
        waiting: false,
    });
    match refused {
        Err(Refusal::Held(Conflict {
            owner, held, pid, ..
        })) => {
            assert_eq!(owner, first);
            assert_eq!(held, Held::Exclusive);
            assert_eq!(pid, PID);
        }
        other => panic!("expected a held conflict, got {other:?}"),
    }
}

#[test]
fn shared_holders_coexist_and_only_an_exclusive_request_is_turned_away() {
    let _serial = registry_guard();
    let f = file(3);
    let (a, b, c) = (mint_owner(), mint_owner(), mint_owner());
    take(f, a, PID, 1, LockRange::WHOLE, Held::Shared);
    take(f, b, OTHER_PID, 2, LockRange::WHOLE, Held::Shared);
    assert!(
        acquire(&Request {
            file: f,
            owner: c,
            pid: PID,
            task: 3,
            range: LockRange::WHOLE,
            held: Held::Shared,
            limit: ROOMY,
            waiting: false,
        })
        .is_ok(),
        "readers do not exclude readers"
    );
    assert!(
        matches!(
            acquire(&Request {
                file: f,
                owner: c,
                pid: PID,
                task: 3,
                range: range(0, 10),
                held: Held::Exclusive,
                limit: ROOMY,
                waiting: false,
            }),
            Err(Refusal::Held(_)),
        ),
        "a writer is excluded by a reader"
    );
    assert!(invariants_hold(f));
}

#[test]
fn disjoint_ranges_never_conflict_in_either_mode() {
    let _serial = registry_guard();
    let f = file(4);
    let (a, b) = (mint_owner(), mint_owner());
    take(f, a, PID, 1, range(0, 99), Held::Exclusive);
    assert!(
        acquire(&Request {
            file: f,
            owner: b,
            pid: OTHER_PID,
            task: 2,
            range: range(100, 199),
            held: Held::Exclusive,
            limit: ROOMY,
            waiting: false,
        })
        .is_ok(),
        "a range that merely abuts is not a range that overlaps"
    );
    assert!(
        matches!(
            acquire(&Request {
                file: f,
                owner: b,
                pid: OTHER_PID,
                task: 2,
                range: range(99, 150),
                held: Held::Shared,
                limit: ROOMY,
                waiting: false,
            }),
            Err(Refusal::Held(_))
        ),
        "one shared byte is enough to conflict"
    );
}

#[test]
fn an_unbounded_range_covers_everything_from_its_start() {
    let _serial = registry_guard();
    let f = file(5);
    let (a, b) = (mint_owner(), mint_owner());
    let tail = LockRange::new(100, LOCK_LEN_TO_END).expect("tail");
    take(f, a, PID, 1, tail, Held::Exclusive);
    assert!(
        acquire(&Request {
            file: f,
            owner: b,
            pid: OTHER_PID,
            task: 2,
            range: range(0, 99),
            held: Held::Exclusive,
            limit: ROOMY,
            waiting: false,
        })
        .is_ok(),
        "the bytes before the tail are free"
    );
    assert!(
        matches!(
            acquire(&Request {
                file: f,
                owner: b,
                pid: OTHER_PID,
                task: 2,
                range: range(u64::MAX, u64::MAX),
                held: Held::Shared,
                limit: ROOMY,
                waiting: false,
            }),
            Err(Refusal::Held(_))
        ),
        "an unbounded lock reaches the last byte of the address space"
    );
}

#[test]
fn abutting_same_mode_locks_merge_into_one_record() {
    let _serial = registry_guard();
    let f = file(6);
    let owner = mint_owner();
    take(f, owner, PID, 1, range(0, 9), Held::Exclusive);
    take(f, owner, PID, 1, range(10, 19), Held::Exclusive);
    assert_eq!(
        held_ranges(f, owner),
        std::vec![(0, 19, Held::Exclusive)],
        "extending a lock must not accumulate a record per extension"
    );
    assert_eq!(usage(PID), 1, "and the charge follows the records");
    assert!(invariants_hold(f));
}

#[test]
fn unlocking_the_middle_splits_one_record_into_two() {
    let _serial = registry_guard();
    let f = file(7);
    let owner = mint_owner();
    take(f, owner, PID, 1, range(0, 99), Held::Exclusive);
    assert_eq!(usage(PID), 1);
    release(f, owner, range(40, 59), ROOMY).expect("release");
    assert_eq!(
        held_ranges(f, owner),
        std::vec![(0, 39, Held::Exclusive), (60, 99, Held::Exclusive)],
    );
    assert_eq!(
        usage(PID),
        2,
        "the split is charged, because it costs memory"
    );
    assert!(invariants_hold(f));
}

#[test]
fn an_upgrade_converts_the_range_and_leaves_the_rest_alone() {
    let _serial = registry_guard();
    let f = file(8);
    let owner = mint_owner();
    take(f, owner, PID, 1, range(0, 99), Held::Shared);
    take(f, owner, PID, 1, range(40, 59), Held::Exclusive);
    assert_eq!(
        held_ranges(f, owner),
        std::vec![
            (0, 39, Held::Shared),
            (40, 59, Held::Exclusive),
            (60, 99, Held::Shared),
        ],
    );
    assert!(invariants_hold(f));
}

#[test]
fn a_downgrade_over_the_whole_range_collapses_back_to_one_record() {
    let _serial = registry_guard();
    let f = file(9);
    let owner = mint_owner();
    take(f, owner, PID, 1, range(0, 99), Held::Exclusive);
    take(f, owner, PID, 1, range(0, 99), Held::Shared);
    assert_eq!(held_ranges(f, owner), std::vec![(0, 99, Held::Shared)]);
    assert!(invariants_hold(f));
}

#[test]
fn a_release_of_the_whole_range_drops_the_owner_and_the_file() {
    let _serial = registry_guard();
    let f = file(10);
    let owner = mint_owner();
    take(f, owner, PID, 1, range(0, 99), Held::Exclusive);
    assert!(locks_present());
    release(f, owner, LockRange::WHOLE, ROOMY).expect("release");
    assert!(held_ranges(f, owner).is_empty());
    assert_eq!(usage(PID), 0, "the charge is credited back");
    assert!(!locks_present(), "and the fast-path check goes quiet again");
}

#[test]
fn releasing_a_range_no_one_holds_is_a_no_op_rather_than_an_error() {
    let _serial = registry_guard();
    let f = file(11);
    let owner = mint_owner();
    assert!(release(f, owner, LockRange::WHOLE, ROOMY).is_ok());
    let other = mint_owner();
    take(f, owner, PID, 1, range(0, 9), Held::Exclusive);
    assert!(
        release(f, other, range(0, 9), ROOMY).is_ok(),
        "one owner cannot release another's lock, and asking is not an error"
    );
    assert_eq!(held_ranges(f, owner), std::vec![(0, 9, Held::Exclusive)]);
}

#[test]
fn releasing_the_description_drops_every_lock_it_held_on_every_file() {
    let _serial = registry_guard();
    let (one, two) = (file(12), file(13));
    let owner = mint_owner();
    take(f_of(one), owner, PID, 1, range(0, 9), Held::Exclusive);
    take(f_of(two), owner, PID, 1, LockRange::WHOLE, Held::Shared);
    assert_eq!(usage(PID), 2);
    let _ = release_owner(owner);
    assert!(held_ranges(one, owner).is_empty());
    assert!(held_ranges(two, owner).is_empty());
    assert_eq!(usage(PID), 0);
    assert_eq!(live_records(), 0);
}

/// Identity helper so the test above reads as two distinct files.
fn f_of(f: FileId) -> FileId {
    f
}

#[test]
fn the_record_bound_refuses_growth_but_never_a_whole_release() {
    let _serial = registry_guard();
    let f = file(14);
    let owner = mint_owner();
    acquire(&Request {
        file: f,
        owner,
        pid: PID,
        task: 1,
        range: range(0, 99),
        held: Held::Exclusive,
        limit: 1,
        waiting: false,
    })
    .expect("one record fits");
    // A middle unlock would need two records where one is allowed. Refusing
    // is the fail-closed answer; a hostile loop of splits is exactly how a
    // process would otherwise manufacture unbounded kernel state.
    assert_eq!(
        release(f, owner, range(40, 59), 1),
        Err(Refusal::LimitExceeded)
    );
    assert_eq!(
        held_ranges(f, owner),
        std::vec![(0, 99, Held::Exclusive)],
        "and the refusal leaves the records untouched"
    );
    assert!(
        release(f, owner, range(0, 99), 1).is_ok(),
        "releasing the whole range never grows the count, so there is a way out"
    );
    assert_eq!(usage(PID), 0);
}

#[test]
fn a_fresh_lock_past_the_bound_is_refused_before_anything_is_recorded() {
    let _serial = registry_guard();
    let f = file(15);
    let owner = mint_owner();
    acquire(&Request {
        file: f,
        owner,
        pid: PID,
        task: 1,
        range: range(0, 9),
        held: Held::Exclusive,
        limit: 1,
        waiting: false,
    })
    .expect("first fits");
    assert_eq!(
        acquire(&Request {
            file: f,
            owner,
            pid: PID,
            task: 1,
            range: range(100, 109),
            held: Held::Exclusive,
            limit: 1,
            waiting: false,
        }),
        Err(Refusal::LimitExceeded)
    );
    assert_eq!(held_ranges(f, owner), std::vec![(0, 9, Held::Exclusive)]);
    assert_eq!(usage(PID), 1);
}

#[test]
fn a_query_reports_the_blocking_holder_and_says_nothing_when_free() {
    let _serial = registry_guard();
    let f = file(16);
    let (a, b) = (mint_owner(), mint_owner());
    assert!(query(f, b, LockRange::WHOLE, Held::Exclusive).is_none());
    take(f, a, PID, 1, range(10, 19), Held::Exclusive);
    let found = query(f, b, range(15, 25), Held::Shared).expect("a conflict");
    assert_eq!(found.owner, a);
    assert_eq!(found.held, Held::Exclusive);
    assert_eq!(found.range, range(10, 19));
    assert_eq!(found.pid, PID);
    assert!(
        query(f, b, range(20, 30), Held::Exclusive).is_none(),
        "a disjoint range is free"
    );
    assert!(
        query(f, a, range(10, 19), Held::Exclusive).is_none(),
        "an owner is never in its own way"
    );
    assert_eq!(
        found.to_abi().len,
        10,
        "the reported length is the wire spelling of the held range"
    );
}

#[test]
fn a_waiter_is_woken_only_when_the_range_it_waits_on_is_freed() {
    let _serial = registry_guard();
    let f = file(17);
    let (holder, waiter_owner) = (mint_owner(), mint_owner());
    take(f, holder, PID, 1, range(0, 99), Held::Exclusive);
    let key = enqueue(f, waiter_owner, 2, range(50, 59), Held::Exclusive);
    assert!(key > 0);

    let wakes = release(f, holder, range(0, 9), ROOMY).expect("release the head");
    assert!(
        wakes.is_empty(),
        "freeing bytes the waiter does not want disturbs nobody"
    );

    let wakes = release(f, holder, range(50, 59), ROOMY).expect("release the waited range");
    assert_eq!(wakes.tasks, std::vec![2]);
    assert_eq!(wakes.key, Some(key));
    dequeue(2);
}

#[test]
fn a_release_of_the_description_wakes_the_waiters_it_was_blocking() {
    let _serial = registry_guard();
    let f = file(18);
    let (holder, other) = (mint_owner(), mint_owner());
    take(f, holder, PID, 1, LockRange::WHOLE, Held::Exclusive);
    let key = enqueue(f, other, 5, range(0, 9), Held::Exclusive);
    let wakes = release_owner(holder);
    assert_eq!(wakes.len(), 1);
    assert_eq!(wakes[0].tasks, std::vec![5]);
    assert_eq!(wakes[0].key, Some(key));
    dequeue(5);
}

#[test]
fn a_queued_writer_makes_an_arriving_reader_queue_behind_it() {
    let _serial = registry_guard();
    let f = file(19);
    let (reader, writer, latecomer) = (mint_owner(), mint_owner(), mint_owner());
    take(f, reader, PID, 1, LockRange::WHOLE, Held::Shared);
    // The writer cannot have it yet, so it queues.
    assert!(matches!(
        acquire(&Request {
            file: f,
            owner: writer,
            pid: OTHER_PID,
            task: 2,
            range: LockRange::WHOLE,
            held: Held::Exclusive,
            limit: ROOMY,
            waiting: true,
        }),
        Err(Refusal::Held(_))
    ));
    let _ = enqueue(f, writer, 2, LockRange::WHOLE, Held::Exclusive);

    // A reader arriving now would be compatible with the holder, but taking
    // it would push the queued writer back indefinitely.
    assert_eq!(
        acquire(&Request {
            file: f,
            owner: latecomer,
            pid: PID,
            task: 3,
            range: LockRange::WHOLE,
            held: Held::Shared,
            limit: ROOMY,
            waiting: true,
        }),
        Err(Refusal::Queued),
        "a blocking request respects the queue, so a writer cannot be starved"
    );
    // A non-blocking request keeps its own contract: it reports whether the
    // lock is free, and it cannot starve anyone by waiting.
    assert!(
        acquire(&Request {
            file: f,
            owner: latecomer,
            pid: PID,
            task: 3,
            range: LockRange::WHOLE,
            held: Held::Shared,
            limit: ROOMY,
            waiting: false,
        })
        .is_ok(),
        "a non-blocking request tests the holders only"
    );
    dequeue(2);
}

#[test]
fn a_conversion_is_exempt_from_the_queue_so_it_cannot_be_boxed_in() {
    let _serial = registry_guard();
    let f = file(20);
    let (holder, writer) = (mint_owner(), mint_owner());
    take(f, holder, PID, 1, range(0, 99), Held::Shared);
    let _ = enqueue(f, writer, 2, range(0, 99), Held::Exclusive);
    // The holder upgrading its own range must not be made to queue behind
    // the newcomer: it cannot release what the newcomer wants without
    // giving up the range it is converting, so queueing could only deadlock.
    assert!(
        acquire(&Request {
            file: f,
            owner: holder,
            pid: PID,
            task: 1,
            range: range(0, 99),
            held: Held::Exclusive,
            limit: ROOMY,
            waiting: true,
        })
        .is_ok(),
        "an upgrade of a held range tests the holders only"
    );
    assert_eq!(held_ranges(f, holder), std::vec![(0, 99, Held::Exclusive)]);
    dequeue(2);
}

#[test]
fn a_two_owner_cycle_is_refused_rather_than_joined() {
    let _serial = registry_guard();
    let (one, two) = (file(21), file(22));
    let (a, b) = (mint_owner(), mint_owner());
    take(one, a, PID, 1, LockRange::WHOLE, Held::Exclusive);
    take(two, b, OTHER_PID, 2, LockRange::WHOLE, Held::Exclusive);

    // `b` waits for the file `a` holds.
    assert!(matches!(
        acquire(&Request {
            file: one,
            owner: b,
            pid: OTHER_PID,
            task: 2,
            range: LockRange::WHOLE,
            held: Held::Exclusive,
            limit: ROOMY,
            waiting: true,
        }),
        Err(Refusal::Held(_))
    ));
    let _ = enqueue(one, b, 2, LockRange::WHOLE, Held::Exclusive);

    // `a` now asking for the file `b` holds would close the cycle.
    assert_eq!(
        acquire(&Request {
            file: two,
            owner: a,
            pid: PID,
            task: 1,
            range: LockRange::WHOLE,
            held: Held::Exclusive,
            limit: ROOMY,
            waiting: true,
        }),
        Err(Refusal::Deadlock)
    );
    // Without the intent to wait there is nothing to deadlock: the caller is
    // told the range is held and decides for itself.
    assert!(matches!(
        acquire(&Request {
            file: two,
            owner: a,
            pid: PID,
            task: 1,
            range: LockRange::WHOLE,
            held: Held::Exclusive,
            limit: ROOMY,
            waiting: false,
        }),
        Err(Refusal::Held(_))
    ));
    dequeue(2);
}

#[test]
fn a_holder_that_is_not_waiting_is_not_a_deadlock() {
    let _serial = registry_guard();
    let f = file(23);
    let (a, b) = (mint_owner(), mint_owner());
    take(f, a, PID, 1, LockRange::WHOLE, Held::Exclusive);
    // `a` holds and is running; `b` waiting on it is an ordinary wait, and
    // reporting a cycle here would refuse a request that will succeed.
    assert!(matches!(
        acquire(&Request {
            file: f,
            owner: b,
            pid: OTHER_PID,
            task: 2,
            range: LockRange::WHOLE,
            held: Held::Exclusive,
            limit: ROOMY,
            waiting: true,
        }),
        Err(Refusal::Held(_))
    ));
}

#[test]
fn a_three_owner_cycle_is_found_through_the_middle_waiter() {
    let _serial = registry_guard();
    let (one, two, three) = (file(24), file(25), file(26));
    let (a, b, c) = (mint_owner(), mint_owner(), mint_owner());
    take(one, a, PID, 1, LockRange::WHOLE, Held::Exclusive);
    take(two, b, PID, 2, LockRange::WHOLE, Held::Exclusive);
    take(three, c, PID, 3, LockRange::WHOLE, Held::Exclusive);
    let _ = enqueue(two, a, 1, LockRange::WHOLE, Held::Exclusive);
    let _ = enqueue(three, b, 2, LockRange::WHOLE, Held::Exclusive);
    // c -> one (held by a), a -> two (held by b), b -> three (held by c).
    assert_eq!(
        acquire(&Request {
            file: one,
            owner: c,
            pid: PID,
            task: 3,
            range: LockRange::WHOLE,
            held: Held::Exclusive,
            limit: ROOMY,
            waiting: true,
        }),
        Err(Refusal::Deadlock)
    );
    dequeue(1);
    dequeue(2);
}

#[test]
fn waiting_on_a_range_the_owner_itself_holds_elsewhere_is_a_self_deadlock() {
    let _serial = registry_guard();
    let f = file(27);
    let (a, b) = (mint_owner(), mint_owner());
    take(f, a, PID, 1, range(0, 9), Held::Exclusive);
    take(f, b, PID, 2, range(10, 19), Held::Exclusive);
    let _ = enqueue(f, b, 2, range(0, 9), Held::Exclusive);
    assert_eq!(
        acquire(&Request {
            file: f,
            owner: a,
            pid: PID,
            task: 1,
            range: range(10, 19),
            held: Held::Exclusive,
            limit: ROOMY,
            waiting: true,
        }),
        Err(Refusal::Deadlock),
        "two descriptions of one process can deadlock each other exactly as \
         two processes can, and the graph sees it because owners are \
         descriptions"
    );
    dequeue(2);
}

#[test]
fn a_re_queued_waiter_keeps_its_place_in_line() {
    let _serial = registry_guard();
    let f = file(28);
    let (holder, first, second) = (mint_owner(), mint_owner(), mint_owner());
    take(f, holder, PID, 1, LockRange::WHOLE, Held::Exclusive);
    let _ = enqueue(f, first, 2, LockRange::WHOLE, Held::Exclusive);
    let _ = enqueue(f, second, 3, LockRange::WHOLE, Held::Shared);
    // A spurious wake and re-queue must not send the earlier waiter to the
    // back, or a busy file could starve it forever.
    let _ = enqueue(f, first, 2, LockRange::WHOLE, Held::Exclusive);
    let _ = release_owner(holder);
    assert_eq!(
        acquire(&Request {
            file: f,
            owner: second,
            pid: PID,
            task: 3,
            range: LockRange::WHOLE,
            held: Held::Shared,
            limit: ROOMY,
            waiting: true,
        }),
        Err(Refusal::Queued),
        "the re-queued writer is still ahead"
    );
    dequeue(2);
    assert!(
        acquire(&Request {
            file: f,
            owner: second,
            pid: PID,
            task: 3,
            range: LockRange::WHOLE,
            held: Held::Shared,
            limit: ROOMY,
            waiting: true,
        })
        .is_ok(),
        "and once it leaves, the reader proceeds"
    );
    dequeue(3);
}

#[test]
fn usage_is_reported_per_process_and_charged_to_the_first_locker() {
    let _serial = registry_guard();
    let f = file(29);
    let (a, b) = (mint_owner(), mint_owner());
    take(f, a, PID, 1, range(0, 9), Held::Exclusive);
    take(f, b, OTHER_PID, 2, range(20, 29), Held::Exclusive);
    assert_eq!(usage(PID), 1);
    assert_eq!(usage(OTHER_PID), 1);
    assert_eq!(live_records(), 2);
    // A second process acting on a description that first locked as `PID`
    // does not migrate the charge, so the count cannot be shuffled between
    // principals to escape a bound.
    take(f, a, OTHER_PID, 3, range(100, 109), Held::Exclusive);
    assert_eq!(usage(PID), 2);
    assert_eq!(usage(OTHER_PID), 1);
}

#[test]
fn held_mode_maps_onto_the_abi_and_refuses_unlock_as_a_held_state() {
    let _serial = registry_guard();
    assert_eq!(Held::from_mode(LockMode::Shared), Some(Held::Shared));
    assert_eq!(Held::from_mode(LockMode::Exclusive), Some(Held::Exclusive));
    assert_eq!(
        Held::from_mode(LockMode::Unlock),
        None,
        "a release is not a state a record can be in"
    );
    assert_eq!(Held::Shared.as_mode(), LockMode::Shared);
    assert_eq!(Held::Exclusive.as_mode(), LockMode::Exclusive);
}

#[test]
fn owner_ids_are_unique_so_a_reclaimed_description_cannot_inherit_locks() {
    let _serial = registry_guard();
    let first = mint_owner();
    let second = mint_owner();
    assert_ne!(
        first, second,
        "a fresh description never shares a reclaimed one's identity, so it \
         cannot inherit its locks"
    );
}

#[test]
fn the_record_algebra_matches_a_per_byte_model_over_a_randomised_run() {
    let _serial = registry_guard();
    let f = file(30);
    let owner = mint_owner();
    let mut model: Vec<Option<Held>> = std::vec![None; SINGLE_OWNER_SPAN];
    let bound = span_bound(SINGLE_OWNER_SPAN);
    let mut rng = FastRng::<64>::from_key(&[0x5Au8; 32]);

    for step in 0..600u32 {
        let a = rng.next_u64() % bound;
        let b = rng.next_u64() % bound;
        let (start, end) = if a <= b { (a, b) } else { (b, a) };
        let span = range(start, end);
        let choice = rng.next_u64() % 3;

        match choice {
            0 => {
                acquire(&Request {
                    file: f,
                    owner,
                    pid: PID,
                    task: 1,
                    range: span,
                    held: Held::Shared,
                    limit: ROOMY,
                    waiting: false,
                })
                .expect("granted");
                for byte in start..=end {
                    model[index_of(byte)] = Some(Held::Shared);
                }
            }
            1 => {
                acquire(&Request {
                    file: f,
                    owner,
                    pid: PID,
                    task: 1,
                    range: span,
                    held: Held::Exclusive,
                    limit: ROOMY,
                    waiting: false,
                })
                .expect("granted");
                for byte in start..=end {
                    model[index_of(byte)] = Some(Held::Exclusive);
                }
            }
            _ => {
                release(f, owner, span, ROOMY).expect("release");
                for byte in start..=end {
                    model[index_of(byte)] = None;
                }
            }
        }

        assert!(
            invariants_hold(f),
            "step {step}: the derived indexes stopped describing the records"
        );
        assert_eq!(
            coverage(f, owner, bound),
            model,
            "step {step}: the records stopped agreeing with the per-byte model \
             after {choice} over [{start}, {end}]"
        );
        let expected = u64::try_from(held_ranges(f, owner).len()).expect("small");
        assert_eq!(
            usage(PID),
            expected,
            "step {step}: the charge drifted from the records it prices"
        );
    }
}

#[test]
fn a_randomised_run_across_several_owners_keeps_exclusion_exact() {
    let _serial = registry_guard();
    let f = file(31);
    let owners = [mint_owner(), mint_owner(), mint_owner()];
    // Per owner, the mode covering each byte — the whole truth the manager
    // must never contradict.
    let mut model: Vec<Vec<Option<Held>>> = std::vec![std::vec![None; MULTI_OWNER_SPAN]; 3];
    let bound = span_bound(MULTI_OWNER_SPAN);
    let mut rng = FastRng::<64>::from_key(&[0xA5u8; 32]);

    for step in 0..800u32 {
        let who = (rng.next_u64() % 3) as usize;
        let a = rng.next_u64() % bound;
        let b = rng.next_u64() % bound;
        let (start, end) = if a <= b { (a, b) } else { (b, a) };
        let span = range(start, end);
        let want = match rng.next_u64() % 3 {
            0 => None,
            1 => Some(Held::Shared),
            _ => Some(Held::Exclusive),
        };

        match want {
            None => {
                release(f, owners[who], span, ROOMY).expect("release");
                for byte in start..=end {
                    model[who][index_of(byte)] = None;
                }
            }
            Some(held) => {
                // Whether the model says someone else is in the way, which
                // is what the manager's answer must match.
                let blocked = (0..3).filter(|&other| other != who).any(|other| {
                    (start..=end).any(|byte| {
                        model[other][index_of(byte)]
                            .is_some_and(|theirs| !held.as_mode().compatible_with(theirs.as_mode()))
                    })
                });
                let outcome = acquire(&Request {
                    file: f,
                    owner: owners[who],
                    pid: PID,
                    task: u64::try_from(who).expect("a small index"),
                    range: span,
                    held,
                    limit: ROOMY,
                    waiting: false,
                });
                assert_eq!(
                    outcome.is_err(),
                    blocked,
                    "step {step}: owner {who} asking {held:?} over [{start}, {end}] \
                     disagreed with the model about being blocked"
                );
                if !blocked {
                    for byte in start..=end {
                        model[who][index_of(byte)] = Some(held);
                    }
                }
            }
        }

        assert!(invariants_hold(f), "step {step}: invariants broke");
        for (index, &owner) in owners.iter().enumerate() {
            assert_eq!(
                coverage(f, owner, bound),
                model[index],
                "step {step}: owner {index}'s records drifted from the model"
            );
        }
    }
}

#[test]
fn an_error_from_the_abi_range_constructor_never_reaches_the_registry() {
    let _serial = registry_guard();
    // The handler validates the range before the manager sees it, so the
    // manager's contract starts at a well-formed span. Pinned here so a
    // future caller cannot quietly pass an unvalidated pair.
    assert_eq!(LockRange::new(u64::MAX, 2), Err(Errno::OutOfRange));
    assert_eq!(LockRange::between(9, 8), Err(Errno::OutOfRange));
}
