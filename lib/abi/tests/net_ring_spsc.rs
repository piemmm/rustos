//! Concurrency proof for the frame-ring transport.
//!
//! Across processes the two sides of a [`FrameRing`] run at the same time:
//! a NIC driver woken by its device interrupt fills the receive ring and
//! rings a doorbell, and the network stack drains that ring without calling
//! back into the driver. The counters are therefore atomics with a
//! release/acquire discipline, and this test is what holds that discipline
//! honest — the in-crate unit tests drive one side at a time and cannot.
//!
//! The two sides each bind their *own* `FrameRing` over the *same* bytes,
//! exactly as two processes each map one `shm` region. That aliasing is the
//! situation being tested, so it is created deliberately here (a test may
//! use `unsafe`; the shipped code never aliases a region within one address
//! space).
//!
//! What it proves: every frame crosses exactly once, in order, with its
//! payload intact, and the consumer never reads a slot the producer had not
//! finished writing — a torn slot would show up as a payload that does not
//! match its sequence stamp.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use tairix_abi::driver::net_ring::{aligned_region, FrameRing, RingGeometry, REGION_ALIGN_PADDING};

/// Ring dimensions: enough slots to allow real overlap, small enough that
/// the ring genuinely fills and the producer has to wait.
const SLOTS: u32 = 8;

/// Slot capacity, comfortably above the stamped payload below.
const CAP: u32 = 256;

/// Frames pushed through the ring. Large enough that the counters cycle the
/// slot array hundreds of times and both sides block on each other often.
const FRAMES: u32 = 20_000;

/// Payload length of each frame.
const PAYLOAD_LEN: usize = 200;

/// Build the frame for `seq`: the sequence number little-endian, then a
/// byte pattern derived from it, so a torn or stale slot cannot masquerade
/// as a valid frame.
fn frame_for(seq: u32) -> Vec<u8> {
    let mut frame = vec![0u8; PAYLOAD_LEN];
    frame[..4].copy_from_slice(&seq.to_le_bytes());
    let fill = u8::try_from(seq & 0xff).expect("masked to a byte");
    for (offset, byte) in frame[4..].iter_mut().enumerate() {
        *byte = fill ^ u8::try_from(offset & 0xff).expect("masked to a byte");
    }
    frame
}

/// A region shared by two threads, mirroring two processes' mappings of one
/// `shm` region.
struct SharedRegion {
    base: *mut u8,
    len: usize,
}

// SAFETY: the pointer addresses a leaked heap allocation that outlives every
// thread here, and the only concurrent access to it goes through
// `FrameRing`'s atomic counters and the slot bytes those counters guard —
// which is precisely the discipline under test. Nothing else in the process
// touches the allocation.
unsafe impl Send for SharedRegion {}
// SAFETY: as for `Send` — the allocation is shared, and every concurrent
// access is mediated by the ring's release/acquire counters.
unsafe impl Sync for SharedRegion {}

impl SharedRegion {
    /// Leak an aligned, zeroed region of `len` bytes.
    fn leak(len: usize) -> Self {
        let mut buffer = vec![0u8; len + REGION_ALIGN_PADDING];
        let region = aligned_region(&mut buffer, len).expect("aligned region");
        let base = region.as_mut_ptr();
        // The allocation must outlive both threads, and both threads hold
        // views into it, so ownership is dropped rather than tracked.
        std::mem::forget(buffer);
        Self { base, len }
    }

    /// One side's exclusive view of the region.
    ///
    /// # Safety
    ///
    /// The caller must use the returned slice only through a `FrameRing`
    /// bound over it, and only as that ring's single producer *or* single
    /// consumer — the two roles the atomic counters synchronise. Two views
    /// alias, which models two processes' mappings of one region.
    unsafe fn view(&self) -> &'static mut [u8] {
        // SAFETY: `base` addresses `len` initialised, leaked bytes that live
        // for the rest of the process, so the `'static` lifetime is sound.
        // The aliasing this creates is the cross-process situation under
        // test, and the caller's contract above confines each view to one
        // ring role.
        unsafe { core::slice::from_raw_parts_mut(self.base, self.len) }
    }
}

#[test]
fn a_concurrent_producer_and_consumer_move_every_frame_exactly_once() {
    // One ring's length, via the public geometry (both directions share
    // the slot count and capacity here).
    let ring_len = RingGeometry::new(SLOTS, SLOTS, CAP, CAP, 1)
        .expect("valid geometry")
        .rx_ring_len();
    let shared = Arc::new(SharedRegion::leak(ring_len));
    // Set when the producer has pushed its last frame, so a consumer that
    // finds the ring empty knows whether more is coming.
    let done = Arc::new(AtomicBool::new(false));

    let producer = {
        let shared = Arc::clone(&shared);
        let done = Arc::clone(&done);
        thread::spawn(move || {
            // SAFETY: this thread is the ring's sole producer, as `view`
            // requires.
            let region = unsafe { shared.view() };
            let mut ring = FrameRing::bind(region, SLOTS, CAP).expect("producer binds");
            for seq in 0..FRAMES {
                let frame = frame_for(seq);
                // A full ring is back-pressure, not an error: yield and
                // retry until the consumer releases a slot.
                loop {
                    match ring.push(&frame) {
                        Ok(()) => break,
                        Err(tairix_abi::Errno::NoSpace) => thread::yield_now(),
                        Err(other) => panic!("producer push failed: {other:?}"),
                    }
                }
            }
            done.store(true, Ordering::Release);
        })
    };

    let consumer = {
        let shared = Arc::clone(&shared);
        let done = Arc::clone(&done);
        thread::spawn(move || {
            // SAFETY: this thread is the ring's sole consumer, as `view`
            // requires.
            let region = unsafe { shared.view() };
            let mut ring = FrameRing::bind(region, SLOTS, CAP).expect("consumer binds");
            let mut out = vec![0u8; CAP as usize];
            let mut expected = 0u32;
            while expected < FRAMES {
                // Read the flag *before* the pop. Read after, it could
                // report pushes that happened since the ring was observed
                // empty, and the emptiness check below would fire on a
                // healthy run — the flag has to be the older observation
                // for "finished and empty" to mean "drained".
                let finished = done.load(Ordering::Acquire);
                match ring.pop(&mut out) {
                    Ok(Some(len)) => {
                        assert_eq!(len, PAYLOAD_LEN, "frame {expected} arrived truncated");
                        assert_eq!(
                            &out[..len],
                            frame_for(expected).as_slice(),
                            "frame {expected} arrived torn, stale, or out of order"
                        );
                        expected += 1;
                    }
                    // Empty is not end-of-stream unless the producer had
                    // already finished when we looked.
                    Ok(None) => {
                        assert!(
                            !finished,
                            "producer finished but only {expected} of {FRAMES} frames arrived"
                        );
                        thread::yield_now();
                    }
                    Err(other) => panic!("consumer pop failed: {other:?}"),
                }
            }
            expected
        })
    };

    producer.join().expect("producer thread");
    let received = consumer.join().expect("consumer thread");
    assert_eq!(received, FRAMES, "every frame crossed exactly once");
}
