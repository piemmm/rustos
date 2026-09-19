//! Loom model-checking harness for the shared PCM ring.
//!
//! Driven by `cargo xtask loom`, which is the only thing that builds this
//! file: loom replaces the ring's atomics wholesale, so it needs a
//! whole-crate rebuild under its own `--cfg` and cannot ride the ordinary
//! test pass. Directly, that is:
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test -p tairix-abi --test loom --release
//! ```
//!
//! Without the `loom` cfg the file compiles to an empty test binary, so the
//! default `cargo test` workflow stays fast.
//!
//! # What this catches that the matrix cannot
//!
//! [`PcmRing`] is a lock-free single-producer/single-consumer protocol: the
//! producer writes a frame's bytes then **releases** its position, and the
//! consumer **acquires** that position before reading them. On a
//! total-store-ordered host — every x86 developer machine and CI runner —
//! a release store and a relaxed one compile to the same instruction, so the
//! whole test suite would pass with both orderings downgraded and the defect
//! would surface years later on a weakly-ordered machine.
//!
//! A model over the counters alone could not catch that either: per-location
//! coherence already gives monotonicity, and the edge's whole purpose is the
//! visibility of the *data* it orders. So the model below hangs a payload off
//! the real edge. The producer writes a cell, then calls [`PcmRing::write`];
//! the consumer calls [`PcmRing::read`] and, **only** where that returned
//! frames, reads the cell. Nothing else connects the two threads — no join,
//! no lock, no second atomic — so the cell's two accesses are ordered by this
//! code's own release/acquire pair or not at all. Downgrade either and loom
//! reports the race on the cell.
//!
//! The two sides carry their own sample areas, because the shared byte region
//! two processes map is not something the model checker can see into: it
//! instruments its own cells and atomics, and a plain `&mut [u8]` is
//! invisible to it. The "no torn frame" half therefore stays with
//! `tests/audio_ring_spsc.rs`, which drives both sides concurrently over one
//! genuinely aliased region, and with `fuzz_audio`, which drives every
//! operation over positions a hostile peer could have written.

#![cfg(loom)]

use loom::cell::UnsafeCell;
use loom::sync::atomic::AtomicU64;
use loom::sync::Arc;
use loom::thread;

use tairix_abi::driver::audio::SampleFormat;
use tairix_abi::driver::audio_ring::{PcmGeometry, PcmRing};

/// Frames the modelled ring holds. Two is the protocol's own minimum and the
/// smallest state space that still lets the ring fill.
const FRAMES: u32 = 2;

/// The payload the ring's publication is being asked to order.
///
/// A cell rather than an atomic on purpose: loom only reports a causality
/// violation for an access it can see is unsynchronised, and an atomic would
/// be synchronised by definition.
struct Payload(UnsafeCell<u64>);

// SAFETY: the two accesses are ordered by the ring's own release/acquire
// pair, which is precisely the claim under test — if that ordering does not
// hold, loom reports it rather than the program misbehaving silently.
unsafe impl Sync for Payload {}
// SAFETY: as for `Sync` — the value is shared between the model's two
// threads and reached only through the ring's publication.
unsafe impl Send for Payload {}

/// The value the producer publishes, chosen so a stale read of the cell's
/// initial zero is unmistakable.
const PUBLISHED: u64 = 0xA5A5_5A5A_DEAD_BEEF;

fn geometry() -> PcmGeometry {
    PcmGeometry::new(FRAMES, SampleFormat::S16, 1).expect("a valid geometry")
}

/// One frame in the modelled geometry.
fn frame() -> [u8; 2] {
    [0x34, 0x12]
}

/// The producer's release must be what makes its payload visible to a
/// consumer that acquired the published position.
#[test]
fn loom_pcm_ring_publication_orders_the_frames_it_points_at() {
    loom::model(|| {
        let producer = Arc::new(AtomicU64::new(0));
        let consumer = Arc::new(AtomicU64::new(0));
        let payload = Arc::new(Payload(UnsafeCell::new(0)));

        let writer = {
            let producer = Arc::clone(&producer);
            let consumer = Arc::clone(&consumer);
            let payload = Arc::clone(&payload);
            thread::spawn(move || {
                // SAFETY: this thread is the sole writer of the cell, and it
                // writes before publishing the position the reader acquires.
                payload.0.with_mut(|slot| unsafe { *slot = PUBLISHED });
                let mut samples = [0u8; (FRAMES as usize) * 2];
                let mut ring =
                    PcmRing::over_counters(&producer, &consumer, &mut samples, geometry())
                        .expect("the producer binds");
                ring.write(&frame()).expect("a healthy ring");
            })
        };

        let mut samples = [0u8; (FRAMES as usize) * 2];
        let mut ring = PcmRing::over_counters(&producer, &consumer, &mut samples, geometry())
            .expect("the consumer binds");
        let mut out = [0u8; 2];
        // Bounded attempts rather than a spin: loom explores every ordering,
        // so the executions where the write is already visible are covered,
        // and the ones where it is not simply have nothing to assert. There
        // is deliberately no join before this — a join would order the two
        // threads by itself and hide exactly the defect being hunted.
        for _ in 0..3 {
            if ring.read(&mut out).expect("a healthy ring") > 0 {
                // SAFETY: the read returned frames, so this side acquired the
                // position the writer released after storing the payload.
                let seen = payload.0.with(|slot| unsafe { *slot });
                assert_eq!(
                    seen, PUBLISHED,
                    "a consumer that observed the position saw stale bytes"
                );
                break;
            }
            thread::yield_now();
        }

        writer.join().expect("the producer thread");
    });
}

/// Whatever the interleaving, neither side ever sees a pair the ring refuses
/// and neither position ever goes backwards.
#[test]
fn loom_pcm_ring_positions_stay_coherent_under_every_interleaving() {
    loom::model(|| {
        let producer = Arc::new(AtomicU64::new(0));
        let consumer = Arc::new(AtomicU64::new(0));

        let writer = {
            let producer = Arc::clone(&producer);
            let consumer = Arc::clone(&consumer);
            thread::spawn(move || {
                let mut samples = [0u8; (FRAMES as usize) * 2];
                let mut ring =
                    PcmRing::over_counters(&producer, &consumer, &mut samples, geometry())
                        .expect("the producer binds");
                let mut published = 0u64;
                for _ in 0..2 {
                    let taken = ring.write(&frame()).expect("positions stay sane");
                    published += u64::from(taken);
                    let now = ring.producer_position().expect("positions stay sane");
                    assert_eq!(now.get(), published, "the producer position lied");
                    assert!(
                        ring.writable_frames().expect("positions stay sane") <= FRAMES,
                        "the ring offered more room than it has"
                    );
                }
            })
        };

        let mut samples = [0u8; (FRAMES as usize) * 2];
        let mut ring = PcmRing::over_counters(&producer, &consumer, &mut samples, geometry())
            .expect("the consumer binds");
        let mut out = [0u8; 2];
        let mut taken_total = 0u64;
        for _ in 0..3 {
            let taken = ring.read(&mut out).expect("positions stay sane");
            taken_total += u64::from(taken);
            let now = ring.consumer_position().expect("positions stay sane");
            assert_eq!(now.get(), taken_total, "the consumer position lied");
            assert!(
                ring.readable_frames().expect("positions stay sane") <= FRAMES,
                "the ring offered more frames than it holds"
            );
        }

        writer.join().expect("the producer thread");
        assert!(
            ring.readable_frames().expect("positions stay sane") <= FRAMES,
            "the ring ended over-full"
        );
    });
}
