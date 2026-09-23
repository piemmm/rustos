extern crate alloc;

use alloc::vec::Vec;

use super::{ByteQueue, QueueError};
use crate::TryReserveError;

fn filled(queue: &mut ByteQueue, bytes: &[u8]) {
    assert_eq!(queue.push_slice(bytes), Ok(bytes.len()));
}

#[test]
fn a_lazy_queue_commits_nothing_until_bytes_arrive() {
    let queue = ByteQueue::new(1 << 20);
    assert_eq!(queue.arena.capacity(), 0);
    assert!(queue.is_empty());
    assert_eq!(queue.room(), 1 << 20);
}

#[test]
fn a_committed_queue_never_allocates_again() {
    let mut queue = ByteQueue::committed(64).expect("the host allocator serves");
    let storage = queue.arena.as_ptr();
    for round in 0..50u8 {
        filled(&mut queue, &[round; 40]);
        queue.consume(40);
    }
    filled(&mut queue, &[7; 64]);
    assert_eq!(queue.arena.as_ptr(), storage);
    assert_eq!(queue.arena.len(), 64);
}

#[test]
fn a_queue_leaves_nothing_it_carried_in_memory_it_frees() {
    let mut queue = ByteQueue::committed(32).expect("the host allocator serves");
    filled(&mut queue, &[0xA5; 24]);
    queue.consume(16);
    // Consumed bytes linger in live storage, so the wipe must reach them too.
    assert_eq!(queue.len(), 8);
    assert!(queue.arena[..24].iter().all(|byte| *byte == 0xA5));
    queue.wipe();
    assert!(queue.arena.iter().all(|byte| *byte == 0));
}

#[test]
fn bytes_come_out_in_the_order_they_went_in() {
    let mut queue = ByteQueue::new(32);
    filled(&mut queue, b"abc");
    filled(&mut queue, b"defg");
    assert_eq!(queue.pending(), b"abcdefg");
    queue.consume(2);
    assert_eq!(queue.pending(), b"cdefg");
    filled(&mut queue, b"h");
    assert_eq!(queue.pending(), b"cdefgh");
}

#[test]
fn the_bound_is_never_exceeded() {
    let mut queue = ByteQueue::new(10);
    assert_eq!(queue.push_slice(&[1; 25]), Ok(10));
    assert_eq!(queue.room(), 0);
    assert_eq!(queue.push_slice(&[2; 3]), Ok(0));
    assert_eq!(queue.append_slot(1).err(), Some(QueueError::Full));
    assert_eq!(queue.reserve(1), Err(QueueError::Full));
    assert!(queue.arena.len() <= 10);
    queue.consume(4);
    assert_eq!(queue.push_slice(&[3; 9]), Ok(4));
    assert_eq!(queue.pending(), &[1, 1, 1, 1, 1, 1, 3, 3, 3, 3]);
}

#[test]
fn an_append_slot_is_all_or_nothing() {
    let mut queue = ByteQueue::new(8);
    filled(&mut queue, b"12345");
    assert_eq!(queue.append_slot(4).err(), Some(QueueError::Full));
    assert_eq!(queue.pending(), b"12345");
    let slot = queue.append_slot(3).expect("fits the bound");
    slot.copy_from_slice(b"678");
    assert_eq!(queue.pending(), b"12345678");
}

#[test]
fn a_reservation_compacts_before_it_grows() {
    let mut queue = ByteQueue::new(16);
    filled(&mut queue, &[9; 16]);
    queue.consume(3);
    // Thirteen queued; three bytes of room sit behind the head.
    let storage = queue.arena.as_ptr();
    queue.reserve(3).expect("room exists below the bound");
    assert_eq!(queue.arena.as_ptr(), storage, "compaction, not growth");
    assert_eq!(queue.head, 0);
    assert_eq!(queue.pending(), &[9; 13]);
}

#[test]
fn growth_doubles_so_small_appends_do_not_reallocate_each_time() {
    // Miri interprets every byte the growth copies; the doubling shows as
    // plainly over fewer appends.
    let appends = if cfg!(miri) { 512 } else { 4096 };
    let mut queue = ByteQueue::new(1 << 16);
    let mut allocations = 0;
    let mut last = queue.arena.as_ptr();
    for _ in 0..appends {
        filled(&mut queue, &[5; 16]);
        if queue.arena.as_ptr() != last {
            allocations += 1;
            last = queue.arena.as_ptr();
        }
    }
    assert_eq!(queue.len(), appends * 16);
    // One allocation per doubling from 16 bytes, not one per append.
    assert!(
        allocations <= 14,
        "{allocations} reallocations for {appends} appends"
    );
}

#[test]
fn draining_everything_resets_rather_than_moving() {
    let mut queue = ByteQueue::new(64);
    filled(&mut queue, &[1; 48]);
    queue.consume(48);
    assert_eq!((queue.head, queue.tail), (0, 0));
}

#[test]
fn consuming_past_half_compacts_the_prefix_away() {
    let mut queue = ByteQueue::committed(64).expect("the host allocator serves");
    filled(&mut queue, &[1; 40]);
    queue.consume(31);
    assert_eq!(queue.head, 31);
    queue.consume(1);
    assert_eq!(queue.head, 0);
    assert_eq!(queue.pending(), &[1; 8]);
}

#[test]
fn consume_and_truncate_saturate_at_the_queued_length() {
    let mut queue = ByteQueue::new(16);
    filled(&mut queue, b"abcdef");
    queue.truncate(100);
    assert_eq!(queue.pending(), b"abcdef");
    queue.truncate(4);
    assert_eq!(queue.pending(), b"abcd");
    queue.consume(100);
    assert!(queue.is_empty());
}

#[test]
fn fill_offers_only_committed_room_and_keeps_what_read_reports() {
    let mut queue = ByteQueue::new(32);
    assert_eq!(queue.fill(|window| Ok::<_, ()>(window.len())), Ok(0));
    queue.reserve(8).expect("fits the bound");
    let kept = queue.fill(|window| {
        window[..5].copy_from_slice(b"hello");
        Ok::<_, ()>(5)
    });
    assert_eq!(kept, Ok(5));
    assert_eq!(queue.pending(), b"hello");
    // A reader overstating its count is clamped to the window it was given.
    let window = queue.arena.len() - queue.tail;
    assert_eq!(queue.fill(|_| Ok::<_, ()>(usize::MAX)), Ok(window));
    assert_eq!(queue.fill(|_| Err::<usize, _>("refused")), Err("refused"));
}

#[test]
fn pending_mut_transforms_in_place() {
    let mut queue = ByteQueue::new(8);
    filled(&mut queue, b"abcd");
    for byte in queue.pending_mut() {
        *byte = byte.to_ascii_uppercase();
    }
    assert_eq!(queue.pending(), b"ABCD");
}

#[test]
fn an_unrepresentable_arena_is_refused_not_a_panic() {
    assert_eq!(
        ByteQueue::committed(usize::MAX).err(),
        Some(TryReserveError::AllocFailed)
    );
    let mut queue = ByteQueue::new(usize::MAX);
    assert_eq!(
        queue.reserve(usize::MAX),
        Err(QueueError::Alloc(TryReserveError::AllocFailed))
    );
    assert!(queue.is_empty());
}

#[test]
fn the_queue_agrees_with_a_reference_over_mixed_operations() {
    const BOUND: usize = 97;
    let mut rng = tairix_fuzzseed::Prng::new(0x5EED_B17E_0000_0001);
    let mut queue = ByteQueue::new(BOUND);
    let mut reference: Vec<u8> = Vec::new();
    let steps = if cfg!(miri) { 200 } else { 2000 };
    for _ in 0..steps {
        let n = rng.at_most(40);
        match rng.below(4) {
            0 | 1 => {
                let mut bytes = alloc::vec![0u8; n];
                rng.fill(&mut bytes);
                let took = queue.push_slice(&bytes).expect("the host allocator serves");
                assert_eq!(took, n.min(BOUND - reference.len()));
                reference.extend_from_slice(&bytes[..took]);
            }
            2 => {
                queue.consume(n);
                reference.drain(..n.min(reference.len()));
            }
            _ => {
                let keep = reference.len().saturating_sub(n / 4);
                queue.truncate(keep);
                reference.truncate(keep);
            }
        }
        assert_eq!(queue.pending(), &reference[..]);
        assert!(queue.arena.len() <= BOUND);
    }
}
