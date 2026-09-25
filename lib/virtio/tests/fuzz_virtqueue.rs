//! Deterministic fuzz harness for the split-virtqueue completion path
//! against a hostile virtio device (of the security
//! charter, CWE-1257 / Thunderclap-class).
//!
//! `SplitQueue::poll_used` consumes the **device-written** used ring, and the
//! device can DMA over the descriptor table it reads. In the threat model those
//! bytes are attacker-controlled: a buggy or malicious device may name a head
//! outside the table, the head of no chain it holds, a chain it already
//! returned, or scribble chain links. Per ("every parser of untrusted
//! input... has a fuzz target") the consumer is driven here against arbitrary
//! device-supplied completions interleaved with honest chain publication.
//!
//! TAIRiX does not pull in an external fuzz runner: a deterministic,
//! per-run-seeded PRNG drives random chains, heads, lengths, and
//! descriptor-table corruption through the in-process [`MockTransport`]
//! hostile-device seams, against a model of which chains the device holds, and
//! asserts the invariants the driver must uphold no matter what it writes:
//!
//! 1. `poll_used` never panics and never dereferences a descriptor outside the
//!    granted table (the run aborting would be the failure).
//! 2. **Fail-closed attribution**: a completion is accepted exactly when it
//!    names the head of a chain the device holds, and is then that chain's,
//!    and every other head is rejected with
//!    [`VirtioError::MalformedCompletion`], reclaiming nothing.
//! 3. **Conservation**: however the device corrupts the table, every
//!    published chain is built from descriptors no held chain owns, as the
//!    table reads right after publication, and the free count plus the
//!    descriptors of every held chain is exactly the queue size — no
//!    descriptor is lost, and none is handed out twice.
//!
//! ## Wall-clock budget
//!
//! A plain `cargo test` runs the [`SMOKE_ITERATIONS`] sweep once from a fresh,
//! logged seed. When
//! `cargo xtask fuzz` exports `TAIRIX_FUZZ_BUDGET_SECS`, the harness keeps
//! drawing from the *same continuing* PRNG stream until the budget
//! elapses, while the logged seed keeps any failure reproducible.

use std::collections::BTreeMap;

use tairix_fuzzseed::Prng;
use tairix_virtio::{
    ChainSegment, Direction, DmaHost, DmaSlab, MockHost, MockTransport, SplitQueue, VirtioError,
};

const SMOKE_ITERATIONS: u64 = 20_000;
const QUEUE_SIZE: u16 = 16;
/// Longest chain the harness publishes.
const MAX_CHAIN: u16 = 4;

/// Build a `'static` `MockHost` the queue can borrow for the process.
fn static_host() -> &'static MockHost {
    Box::leak(Box::new(MockHost::new()))
}

#[test]
fn fuzz_poll_used_is_fail_closed_against_a_hostile_device() {
    let mut rng = Prng::new(tairix_fuzzseed::start(
        "fuzz_poll_used_is_fail_closed_against_a_hostile_device",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let mut t = MockTransport::new(1, QUEUE_SIZE, 0, 0);
    let host = static_host();
    let mut q = SplitQueue::new(&mut t, host, 0, QUEUE_SIZE, MAX_CHAIN).expect("queue setup");
    let region: DmaSlab = host.alloc_dma_zeroed(64).expect("dma");
    // Chain head -> its descriptors, for every chain the device holds.
    let mut with_device: BTreeMap<u16, Vec<u16>> = BTreeMap::new();
    // The held chain each descriptor belongs to.
    let mut owner = [None::<u16>; QUEUE_SIZE as usize];

    let mut rejected = 0u64;
    let mut accepted = 0u64;
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            if rng.next_u64() & 1 == 0 {
                let len = 1 + u16::try_from(rng.next_u64() % u64::from(MAX_CHAIN)).unwrap_or(0);
                let segments: Vec<ChainSegment> = (0..len)
                    .map(|i| ChainSegment {
                        phys: region.phys(),
                        len: 8,
                        direction: if i + 1 == len {
                            Direction::DeviceWrite
                        } else {
                            Direction::DeviceRead
                        },
                    })
                    .collect();
                match q.add_chain(&segments) {
                    Ok(head) => {
                        // Read before anything scribbles on the table: the
                        // descriptors a device walking the chain reaches.
                        let chain = t
                            .chain_descriptors(0, head)
                            .expect("a chain the driver just published");
                        assert_eq!(chain.len(), usize::from(len));
                        assert_eq!(chain.first(), Some(&head));
                        for &desc in &chain {
                            let slot = &mut owner[usize::from(desc)];
                            assert_eq!(
                                *slot, None,
                                "descriptor {desc} was handed out while a held chain owns it"
                            );
                            *slot = Some(head);
                        }
                        with_device.insert(head, chain);
                    }
                    Err(VirtioError::QueueFull) => {
                        assert!(q.free_count() < len, "a chain that fits is never refused");
                    }
                    Err(other) => panic!("unexpected add_chain error {other:?}"),
                }
            }

            // Model a device DMA write scribbling a descriptor field (a
            // chain `next` link). Out-of-range offsets are no-ops by
            // design, so the harness never writes outside driver storage.
            let off = usize::try_from(rng.next_u64() % 320).unwrap_or(0);
            let byte = rng.next_u8();
            t.poke_descriptor(0, off, byte).expect("queue programmed");

            // Half the time a head the device holds, otherwise anything in the
            // `u16` range: out of the table, free, interior, or returned.
            let head = match with_device
                .keys()
                .nth(usize::try_from(rng.next_u64() % (with_device.len() as u64 + 1)).unwrap_or(0))
            {
                Some(&head) if rng.next_u64() & 1 == 0 => head,
                _ => rng.next_u16(),
            };
            t.publish_raw_used(0, head, rng.next_u32())
                .expect("queue programmed");

            // The consumer must be *total*: `Ok` or a typed `Err`, never
            // a panic, never an out-of-region descriptor dereference.
            match q.poll_used() {
                Ok(tok) => {
                    assert_eq!(tok.head, head, "accepted as another chain's");
                    let chain = with_device
                        .remove(&tok.head)
                        .expect("a completion was accepted for a chain the device does not hold");
                    for desc in chain {
                        owner[usize::from(desc)] = None;
                    }
                    accepted += 1;
                }
                Err(VirtioError::MalformedCompletion) => {
                    assert!(
                        !with_device.contains_key(&head),
                        "a completion for a held chain was refused"
                    );
                    rejected += 1;
                }
                Err(other) => panic!("unexpected poll_used error {other:?}"),
            }

            let held_descriptors = owner.iter().filter(|slot| slot.is_some()).count();
            assert_eq!(
                usize::from(q.free_count()) + held_descriptors,
                usize::from(QUEUE_SIZE),
                "every descriptor is free or in exactly one held chain"
            );
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }

    assert!(rejected > 0, "fuzz never exercised the reject path");
    assert!(accepted > 0, "fuzz never exercised the accept path");
}
