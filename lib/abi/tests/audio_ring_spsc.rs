//! Concurrency proof for the PCM ring transport.
//!
//! Across processes the two sides of a [`PcmRing`] run at the same time: a
//! client fills its playback ring while the mixer drains it a period behind,
//! and neither calls into the other. The positions are therefore atomics with
//! a release/acquire discipline, and this test is what holds that discipline
//! honest — the in-crate unit tests drive one side at a time and structurally
//! cannot.
//!
//! The two sides each bind their *own* `PcmRing` over the *same* bytes,
//! exactly as two processes each map one `shm` region. That aliasing is the
//! situation being tested, so it is created deliberately here (a test may use
//! `unsafe`; the shipped code never aliases a region within one address
//! space).
//!
//! What it proves: every frame crosses exactly once, in order, with its
//! samples intact, and the consumer never reads a frame the producer had not
//! finished writing — a torn frame would show up as samples that do not match
//! the position they arrived at. It also proves the two ends agree on where
//! the stream got to, which is the property the whole clock model rests on.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use tairix_abi::driver::audio::{Frames, SampleFormat};
use tairix_abi::driver::audio_ring::{aligned_region, PcmGeometry, PcmRing, REGION_ALIGN_PADDING};

/// Ring depth: enough frames to allow real overlap, few enough that the ring
/// genuinely fills and the producer has to wait.
const FRAMES_IN_RING: u32 = 8;

/// Frames pushed through the ring. Large enough that the positions cycle the
/// sample area thousands of times and both sides block on each other often.
const TOTAL_FRAMES: u64 = 40_000;

/// Frames one write or read moves. Deliberately not a divisor of the ring
/// depth, so the copies straddle the wrap constantly.
const CHUNK: usize = 3;

/// Interleaved channels. Three, so the frame size is not a power of two and a
/// mis-scaled offset would show up immediately.
const CHANNELS: u8 = 3;

/// The ring's shape. Packed 24-bit samples across three channels is a nine-byte
/// frame: awkward on purpose.
fn geometry() -> PcmGeometry {
    PcmGeometry::new(FRAMES_IN_RING, SampleFormat::S24, CHANNELS).expect("valid geometry")
}

/// The bytes of the frame at stream position `at`: a pattern derived from the
/// position, so a torn, stale, or out-of-order frame cannot masquerade as a
/// valid one.
fn frame_at(at: u64, frame_bytes: usize) -> Vec<u8> {
    let stamp = u8::try_from(at & 0xff).expect("masked to a byte");
    (0..frame_bytes)
        .map(|byte| stamp ^ u8::try_from(byte & 0xff).expect("masked to a byte"))
        .collect()
}

/// A region shared by two threads, mirroring two processes' mappings of one
/// `shm` region.
struct SharedRegion {
    base: *mut u8,
    len: usize,
}

// SAFETY: the pointer addresses a leaked heap allocation that outlives every
// thread here, and the only concurrent access to it goes through `PcmRing`'s
// atomic positions and the sample bytes those positions guard — which is
// precisely the discipline under test. Nothing else in the process touches the
// allocation.
unsafe impl Send for SharedRegion {}
// SAFETY: as for `Send` — the allocation is shared, and every concurrent
// access is mediated by the ring's release/acquire positions.
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
    /// The caller must use the returned slice only through a `PcmRing` bound
    /// over it, and only as that ring's single producer *or* single consumer —
    /// the two roles the atomic positions synchronise. Two views alias, which
    /// models two processes' mappings of one region.
    unsafe fn view(&self) -> &'static mut [u8] {
        // SAFETY: `base` addresses `len` initialised, leaked bytes that live
        // for the rest of the process, so the `'static` lifetime is sound. The
        // aliasing this creates is the cross-process situation under test, and
        // the caller's contract above confines each view to one ring role.
        unsafe { core::slice::from_raw_parts_mut(self.base, self.len) }
    }
}

#[test]
fn a_concurrent_producer_and_consumer_move_every_frame_exactly_once() {
    let geometry = geometry();
    let frame_bytes = geometry.frame_bytes();
    let shared = Arc::new(SharedRegion::leak(geometry.region_len()));
    // Set when the producer has published its last frame, so a consumer that
    // finds the ring empty knows whether more is coming.
    let done = Arc::new(AtomicBool::new(false));

    let producer = {
        let shared = Arc::clone(&shared);
        let done = Arc::clone(&done);
        thread::spawn(move || {
            // SAFETY: this thread is the ring's sole producer, as `view`
            // requires.
            let region = unsafe { shared.view() };
            let mut ring = PcmRing::bind(region, geometry).expect("producer binds");
            let mut at = 0u64;
            while at < TOTAL_FRAMES {
                let wanted = CHUNK.min(usize::try_from(TOTAL_FRAMES - at).expect("bounded"));
                let mut chunk = Vec::with_capacity(wanted * frame_bytes);
                for frame in 0..wanted {
                    chunk.extend_from_slice(&frame_at(at + frame as u64, frame_bytes));
                }
                // A full ring is back-pressure, not an error: yield and retry
                // until the consumer releases frames.
                let taken = match ring.write(&chunk) {
                    Ok(0) => {
                        thread::yield_now();
                        continue;
                    }
                    Ok(taken) => taken,
                    Err(other) => panic!("producer write failed: {other:?}"),
                };
                at += u64::from(taken);
                assert_eq!(
                    ring.producer_position(),
                    Ok(Frames::new(at)),
                    "the producer position must be exactly what it has published"
                );
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
            let mut ring = PcmRing::bind(region, geometry).expect("consumer binds");
            let mut out = vec![0u8; CHUNK * frame_bytes];
            let mut at = 0u64;
            while at < TOTAL_FRAMES {
                // Read the flag *before* the read. Read after, it could report
                // writes that happened since the ring was observed empty, and
                // the emptiness check below would fire on a healthy run — the
                // flag has to be the older observation for "finished and
                // empty" to mean "drained".
                let finished = done.load(Ordering::Acquire);
                match ring.read(&mut out) {
                    Ok(0) => {
                        assert!(
                            !finished,
                            "producer finished but only {at} of {TOTAL_FRAMES} frames arrived"
                        );
                        thread::yield_now();
                    }
                    Ok(taken) => {
                        for frame in 0..usize::try_from(taken).expect("bounded") {
                            let offset = frame * frame_bytes;
                            assert_eq!(
                                &out[offset..offset + frame_bytes],
                                frame_at(at + frame as u64, frame_bytes).as_slice(),
                                "the frame at position {} arrived torn, stale, or out of order",
                                at + frame as u64
                            );
                        }
                        at += u64::from(taken);
                        assert_eq!(
                            ring.consumer_position(),
                            Ok(Frames::new(at)),
                            "the consumer position must be exactly what it has taken"
                        );
                    }
                    Err(other) => panic!("consumer read failed: {other:?}"),
                }
            }
        })
    };

    producer.join().expect("producer thread");
    consumer.join().expect("consumer thread");

    // SAFETY: both threads have joined, so this view is the only one alive.
    let region = unsafe { shared.view() };
    let ring = PcmRing::bind(region, geometry).expect("binds after the run");
    assert_eq!(ring.producer_position(), Ok(Frames::new(TOTAL_FRAMES)));
    assert_eq!(ring.consumer_position(), Ok(Frames::new(TOTAL_FRAMES)));
    assert_eq!(ring.readable_frames(), Ok(0));
}

/// Silence is published through the same release, so a gap the producer fills
/// is as ordered as the samples around it — and the consumer sees exactly the
/// positions the producer meant.
#[test]
fn silence_written_concurrently_lands_between_the_frames_it_separates() {
    let geometry = geometry();
    let frame_bytes = geometry.frame_bytes();
    let shared = Arc::new(SharedRegion::leak(geometry.region_len()));
    let done = Arc::new(AtomicBool::new(false));
    // Alternate one stamped frame and one frame of silence, so every second
    // position must come back as the format's quiet value.
    let pairs = 4_000u64;

    let producer = {
        let shared = Arc::clone(&shared);
        let done = Arc::clone(&done);
        thread::spawn(move || {
            // SAFETY: this thread is the ring's sole producer, as `view`
            // requires.
            let region = unsafe { shared.view() };
            let mut ring = PcmRing::bind(region, geometry).expect("producer binds");
            for pair in 0..pairs {
                let frame = frame_at(pair * 2, frame_bytes);
                while ring.write(&frame).expect("healthy ring") == 0 {
                    thread::yield_now();
                }
                while ring.write_silence(1).expect("healthy ring") == 0 {
                    thread::yield_now();
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
            let mut ring = PcmRing::bind(region, geometry).expect("consumer binds");
            let mut out = vec![0u8; frame_bytes];
            let quiet = vec![geometry.format().silence_byte(); frame_bytes];
            let mut at = 0u64;
            while at < pairs * 2 {
                let finished = done.load(Ordering::Acquire);
                match ring.read(&mut out) {
                    Ok(0) => {
                        assert!(!finished, "producer finished with {at} frames delivered");
                        thread::yield_now();
                    }
                    Ok(_) => {
                        let expected = if at.is_multiple_of(2) {
                            frame_at(at, frame_bytes)
                        } else {
                            quiet.clone()
                        };
                        assert_eq!(out, expected, "position {at} carried the wrong frame");
                        at += 1;
                    }
                    Err(other) => panic!("consumer read failed: {other:?}"),
                }
            }
        })
    };

    producer.join().expect("producer thread");
    consumer.join().expect("consumer thread");
}
