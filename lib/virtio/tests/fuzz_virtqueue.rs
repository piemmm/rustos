//! Deterministic fuzz harness for the split-virtqueue completion path
//! against a hostile virtio device (`AGENTS.md` §3.6 of the security
//! charter, CWE-1257 / Thunderclap-class).
//!
//! `SplitQueue::poll_used` consumes the **device-written** used ring and
//! the descriptor table the device can DMA over. In the threat model of
//! §4 / §3.6 those bytes are attacker-controlled: a buggy or malicious
//! device may name a descriptor head outside the granted table or
//! scribble a chain `next` link so the driver's reclaim walk would leave
//! the region. Per §19.6 ("every parser of untrusted input ... has a
//! fuzz target") the consumer is driven here against arbitrary
//! device-supplied completions.
//!
//! RustOS does not pull in an external fuzz runner (`AGENTS.md` §2.12): a
//! deterministic, per-run-seeded PRNG drives random heads, lengths, and
//! descriptor-table corruption through the in-process [`MockTransport`]
//! hostile-device seams and asserts the invariants the driver must
//! uphold no matter what the device writes:
//!
//! 1. `poll_used` never panics and never dereferences a descriptor
//!    outside the granted table (the run aborting would be the failure).
//! 2. **Fail-closed** (`AGENTS.md` §5.4): a completion naming a head
//!    `>= queue_size` is rejected with [`VirtioError::MalformedCompletion`],
//!    never reclaimed.
//! 3. An in-range head is accepted (`Ok`); the queue keeps making
//!    progress (`free_count` never exceeds the queue size).
//!
//! ## Wall-clock budget (`AGENTS.md` §19.6)
//!
//! A plain `cargo test` runs the [`SMOKE_ITERATIONS`] sweep once from a fresh,
//! logged seed. When
//! `cargo xtask fuzz` exports `RUSTOS_FUZZ_BUDGET_SECS`, the harness keeps
//! drawing from the *same continuing* PRNG stream until the budget
//! elapses, while the logged seed keeps any failure reproducible.

use rustos_virtio::{
    ChainSegment, ChainView, Direction, DmaHost, DmaSlab, MockHost, MockTransport, SplitQueue,
    VirtioError,
};

const SMOKE_ITERATIONS: u64 = 20_000;
const QUEUE_SIZE: u16 = 16;

/// xor-shift* PRNG. Deterministic, fast, zero-allocation.
struct Rng(u64);
impl Rng {
    const fn new(seed: u64) -> Self {
        Self(seed)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

/// Build a `'static` `MockHost` the queue can borrow for the process.
fn static_host() -> &'static MockHost {
    Box::leak(Box::new(MockHost::new()))
}

#[test]
fn fuzz_poll_used_is_fail_closed_against_a_hostile_device() {
    let mut rng = Rng::new(rustos_fuzzseed::start(
        "fuzz_poll_used_is_fail_closed_against_a_hostile_device",
        rustos_fuzzseed::FUZZ_SEED_ENV,
    ));
    let mut t = MockTransport::new(1, QUEUE_SIZE, 0, 0);
    let host = static_host();
    let mut q = SplitQueue::new(&mut t, host, 0, QUEUE_SIZE).expect("queue setup");

    // A single legitimate round-trip first, to anchor faithfulness before
    // any corruption: an honest device's completion is accepted exactly.
    let mut input: DmaSlab = host.alloc_dma_zeroed(8).expect("dma");
    input.as_bytes_mut()[..4].copy_from_slice(b"PING");
    let output: DmaSlab = host.alloc_dma_zeroed(8).expect("dma");
    t.install_shim(
        0,
        Box::new(|chain: &mut ChainView<'_>| {
            if let Some(out) = chain.device_write.get_mut(0) {
                out.fill(0x42);
            }
            Ok(u32::try_from(chain.device_write.first().map_or(0, |s| s.len())).unwrap_or(0))
        }),
    );
    let head = q
        .add_chain(&[
            ChainSegment {
                phys: input.phys(),
                len: 4,
                direction: Direction::DeviceRead,
            },
            ChainSegment {
                phys: output.phys(),
                len: 8,
                direction: Direction::DeviceWrite,
            },
        ])
        .expect("add chain");
    q.kick(&mut t);
    assert_eq!(t.drain_queue(0).expect("drain"), 1);
    let token = q.poll_used().expect("honest completion accepted");
    assert_eq!(token.head, head, "honest completion is faithful");

    let mut rejected = 0u64;
    let mut accepted = 0u64;
    let deadline = rustos_fuzzseed::budget_deadline(rustos_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            // Model a device DMA write scribbling a descriptor field (a
            // chain `next` link). Out-of-range offsets are no-ops by
            // design, so the harness never writes outside driver storage.
            let off = usize::try_from(rng.next_u64() % 320).unwrap_or(0);
            let byte = (rng.next_u64() & 0xFF) as u8;
            t.poke_descriptor(0, off, byte).expect("queue programmed");

            // Half the time aim inside the table, half the time anywhere
            // in the u16 range so both the accept and reject paths run.
            let head = if rng.next_u64() & 1 == 0 {
                u16::try_from(rng.next_u64() % u64::from(QUEUE_SIZE)).unwrap_or(0)
            } else {
                (rng.next_u64() & 0xFFFF) as u16
            };
            let written = (rng.next_u64() & 0xFFFF_FFFF) as u32;
            t.publish_raw_used(0, head, written)
                .expect("queue programmed");

            // The consumer must be *total*: `Ok` or a typed `Err`, never
            // a panic, never an out-of-region descriptor dereference.
            match q.poll_used() {
                Ok(tok) => {
                    assert!(tok.head < QUEUE_SIZE, "accepted head is in range");
                    accepted += 1;
                }
                Err(VirtioError::MalformedCompletion) => {
                    assert!(head >= QUEUE_SIZE, "rejected head is out of range");
                    rejected += 1;
                }
                Err(VirtioError::NoCompletion) => {}
                Err(other) => panic!("unexpected poll_used error {other:?}"),
            }

            // The free pool must never claim more slots than exist, no
            // matter how the device corrupts the reclaim walk.
            assert!(q.free_count() <= QUEUE_SIZE, "free count stays bounded");
        }
        if !rustos_fuzzseed::within_budget(deadline) {
            break;
        }
    }

    assert!(rejected > 0, "fuzz never exercised the reject path");
    assert!(accepted > 0, "fuzz never exercised the accept path");
}
