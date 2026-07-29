//! Deterministic fuzz-style integration test for the early-boot log ring.
//!
//! A [`tairix_log::BootRing`] is a kernel-side, allocation-free FIFO that must
//! never panic, never fabricate or lose a sequence silently, and always drain
//! records in push order. This harness drives a random stream of push / pop
//! operations against the ring while a simple shadow model (a `VecDeque` of the
//! records that *should* still be live) predicts every outcome, asserting:
//!
//! * drained records match the model front exactly (FIFO order, sequence,
//!   monotonic time, and body bytes preserved);
//! * every eviction is reported as a contiguous loss range whose `count`
//!   equals `last_seq - first_seq + 1` and whose sequences are exactly the
//!   model records it displaced — so no record vanishes without a trusted loss
//!   record naming it; and
//! * the ring never panics on any operation ordering.
//!
//! Seed selection, the start-of-test seed log, and the smoke / soak loop are
//! the shared `tairix_fuzzseed` seam (one definition).

use std::collections::VecDeque;

use tairix_abi::Duration64;
use tairix_log::bootring::{FRAME_HEADER_LEN, MAX_BOOT_RECORD_BODY};
use tairix_log::BootRing;

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 20_000;

/// One live record the model expects the ring to still hold.
struct Shadow {
    seq: u64,
    secs: i64,
    body: Vec<u8>,
}

fn drain_and_check(ring: &mut BootRing<'_>, model: &mut VecDeque<Shadow>, scratch: &mut [u8]) {
    match ring
        .pop_oldest(scratch)
        .expect("pop never errors with a large scratch")
    {
        Some(rec) => {
            let expect = model
                .pop_front()
                .expect("ring had a record, so must the model");
            assert_eq!(rec.cpu_seq, expect.seq, "FIFO sequence mismatch");
            assert_eq!(
                rec.monotonic,
                Duration64::from_secs(expect.secs),
                "time mismatch"
            );
            assert_eq!(
                &scratch[..rec.body_len],
                expect.body.as_slice(),
                "body mismatch"
            );
        }
        None => assert!(model.is_empty(), "ring empty but model is not"),
    }
}

fn apply_loss(ring: &mut BootRing<'_>, model: &mut VecDeque<Shadow>) {
    if let Some(loss) = ring.take_loss() {
        assert_eq!(
            loss.last_seq - loss.first_seq + 1,
            loss.count,
            "loss range must be contiguous"
        );
        // The evicted records are exactly the oldest `count` model entries.
        let mut expect_seq = loss.first_seq;
        for _ in 0..loss.count {
            let dropped = model
                .pop_front()
                .expect("a loss range names live model records");
            assert_eq!(dropped.seq, expect_seq, "evicted the wrong record");
            expect_seq += 1;
        }
        assert_eq!(
            expect_seq,
            loss.last_seq + 1,
            "loss range and count disagree"
        );
    }
}

#[test]
fn boot_ring_matches_a_shadow_model_and_never_panics() {
    let mut prng = tairix_fuzzseed::Lcg::new(tairix_fuzzseed::start(
        "boot_ring_matches_a_shadow_model_and_never_panics",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));

    let mut scratch = [0u8; MAX_BOOT_RECORD_BODY];
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            // A small ring so eviction is frequent, but big enough for a few
            // frames. Capacity is drawn per run.
            let cap = FRAME_HEADER_LEN + 8 + (prng.next_u64() % 200) as usize;
            let mut buf = vec![0u8; cap];
            let cpu_id = (prng.next_u64() & 0xff) as u32;
            let mut ring = BootRing::new(&mut buf, cpu_id).expect("cap holds a frame");
            let mut model: VecDeque<Shadow> = VecDeque::new();
            let mut next_seq = 0u64;

            let ops = prng.next_u64() % 64;
            for _ in 0..ops {
                if prng.next_u64().is_multiple_of(3) {
                    drain_and_check(&mut ring, &mut model, &mut scratch);
                } else {
                    // Body up to a little larger than the ring can hold, so the
                    // "never fits" fail-closed path is exercised too.
                    let len = usize::try_from(prng.next_u64() % (cap as u64 + 4))
                        .expect("a value below cap+4 fits usize");
                    let mut body = vec![0u8; len];
                    prng.fill(&mut body);
                    let secs = i64::try_from(next_seq).expect("seq fits i64") * 2;
                    // A rejected push (body too big to ever fit) leaves the ring
                    // unchanged, so the model is left unchanged too.
                    if ring
                        .push(next_seq, Duration64::from_secs(secs), &body)
                        .is_ok()
                    {
                        apply_loss(&mut ring, &mut model);
                        model.push_back(Shadow {
                            seq: next_seq,
                            secs,
                            body,
                        });
                        next_seq += 1;
                    }
                }
                assert_eq!(ring.len(), model.len(), "retained count diverged");
            }

            // Fully drain and confirm the tail matches, including any final loss.
            apply_loss(&mut ring, &mut model);
            while !ring.is_empty() {
                drain_and_check(&mut ring, &mut model, &mut scratch);
            }
            assert!(model.is_empty(), "model retained records the ring did not");
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}
