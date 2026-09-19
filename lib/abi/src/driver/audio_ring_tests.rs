//! Unit tests for the PCM ring's geometry, bounds, and single-side
//! behaviour. The concurrent producer/consumer pairing is proved by the
//! `audio_ring_spsc` integration test, which the in-crate tests structurally
//! cannot reach: they drive one side at a time.

use super::*;

/// A geometry that is deliberately awkward: an odd frame size (three-byte
/// packed samples, three channels) so every copy exercises the wrap.
fn geometry() -> PcmGeometry {
    PcmGeometry::new(8, SampleFormat::S24, 3).expect("valid geometry")
}

/// Zeroed backing bytes for one test ring, over-allocated by
/// [`REGION_ALIGN_PADDING`] so an aligned region can be cut from them. Sized
/// for the widest geometry the tests build; `no_std` keeps it an array.
fn backing() -> [u8; 256] {
    [0u8; 256]
}

/// `count` frames whose every byte is derived from its frame index, so a
/// torn or stale frame cannot masquerade as a valid one.
fn stamped(geometry: PcmGeometry, first: u8, count: usize) -> [u8; 256] {
    let mut out = [0u8; 256];
    let frame_bytes = geometry.frame_bytes();
    for frame in 0..count {
        let stamp = first.wrapping_add(u8::try_from(frame & 0xff).expect("masked"));
        for byte in 0..frame_bytes {
            out[frame * frame_bytes + byte] = stamp ^ u8::try_from(byte & 0xff).expect("masked");
        }
    }
    out
}

#[test]
fn a_geometry_refuses_a_shape_the_index_arithmetic_could_not_serve() {
    // Not a power of two: the slot index is a mask, not a division.
    assert_eq!(
        PcmGeometry::new(6, SampleFormat::S16, 2),
        Err(Errno::OutOfRange)
    );
    assert_eq!(
        PcmGeometry::new(ring_bounds::MIN_FRAMES - 1, SampleFormat::S16, 2),
        Err(Errno::OutOfRange)
    );
    assert_eq!(
        PcmGeometry::new(ring_bounds::MAX_FRAMES * 2, SampleFormat::S16, 2),
        Err(Errno::OutOfRange)
    );
    assert_eq!(
        PcmGeometry::new(1024, SampleFormat::S16, 0),
        Err(Errno::OutOfRange)
    );
    assert_eq!(
        PcmGeometry::new(
            1024,
            SampleFormat::S16,
            u8::try_from(MAX_CHANNELS + 1).expect("small")
        ),
        Err(Errno::OutOfRange)
    );
}

#[test]
fn a_geometry_reports_the_region_it_needs() {
    let geometry = PcmGeometry::new(1024, SampleFormat::S24In32, 2).expect("valid");
    assert_eq!(geometry.frames(), 1024);
    assert_eq!(geometry.format(), SampleFormat::S24In32);
    assert_eq!(geometry.channels(), 2);
    assert_eq!(geometry.frame_bytes(), 8);
    assert_eq!(geometry.samples_len(), 1024 * 8);
    assert_eq!(geometry.region_len(), PCM_RING_HEADER_LEN + 1024 * 8);
}

#[test]
fn binding_refuses_a_region_that_is_the_wrong_size_or_misaligned() {
    let geometry = geometry();
    let mut buffer = backing();
    let region = aligned_region(&mut buffer, geometry.region_len()).expect("fits");
    assert_eq!(region.len(), geometry.region_len());

    let mut short = backing();
    let region = aligned_region(&mut short, geometry.region_len() - 1).expect("fits");
    assert!(matches!(
        PcmRing::bind(region, geometry),
        Err(Errno::BufferTooSmall)
    ));

    // One byte past an aligned base is the misalignment a raw in-process
    // buffer produces, and the header's atomics cannot live there.
    let mut skewed = backing();
    let len = geometry.region_len();
    let aligned_at = skewed.as_ptr().align_offset(INDEX_ALIGN);
    let region = &mut skewed[aligned_at + 1..aligned_at + 1 + len];
    assert!(matches!(
        PcmRing::bind(region, geometry),
        Err(Errno::BadAlignment)
    ));
}

#[test]
fn a_fresh_ring_is_empty_and_wholly_writable() {
    let geometry = geometry();
    let mut buffer = backing();
    let region = aligned_region(&mut buffer, geometry.region_len()).expect("fits");
    let ring = PcmRing::bind(region, geometry).expect("valid region");
    assert_eq!(ring.geometry(), geometry);
    assert_eq!(ring.readable_frames(), Ok(0));
    assert_eq!(ring.writable_frames(), Ok(8));
    assert_eq!(ring.producer_position(), Ok(Frames::ZERO));
    assert_eq!(ring.consumer_position(), Ok(Frames::ZERO));
}

#[test]
fn a_partial_frame_is_refused_rather_than_rotating_every_later_channel() {
    let geometry = geometry();
    let mut buffer = backing();
    let region = aligned_region(&mut buffer, geometry.region_len()).expect("fits");
    let mut ring = PcmRing::bind(region, geometry).expect("valid region");
    let samples = stamped(geometry, 1, 2);
    assert_eq!(
        ring.write(&samples[..=geometry.frame_bytes()]),
        Err(Errno::LengthOutOfRange)
    );
    assert_eq!(ring.readable_frames(), Ok(0));
}

#[test]
fn frames_cross_the_ring_intact_and_positions_advance_by_exactly_what_moved() {
    let geometry = geometry();
    let frame_bytes = geometry.frame_bytes();
    let mut buffer = backing();
    let region = aligned_region(&mut buffer, geometry.region_len()).expect("fits");
    let mut ring = PcmRing::bind(region, geometry).expect("valid region");

    let written = stamped(geometry, 0x40, 5);
    assert_eq!(ring.write(&written[..5 * frame_bytes]), Ok(5));
    assert_eq!(ring.producer_position(), Ok(Frames::new(5)));
    assert_eq!(ring.readable_frames(), Ok(5));
    assert_eq!(ring.writable_frames(), Ok(3));

    let mut out = [0u8; 256];
    assert_eq!(ring.read(&mut out[..5 * frame_bytes]), Ok(5));
    assert_eq!(&out[..5 * frame_bytes], &written[..5 * frame_bytes]);
    assert_eq!(ring.consumer_position(), Ok(Frames::new(5)));
    assert_eq!(ring.readable_frames(), Ok(0));
}

/// The sample area wraps while the positions do not, which is the whole point
/// of counting frames rather than indexing bytes.
#[test]
fn a_write_that_wraps_the_sample_area_is_read_back_contiguously() {
    let geometry = geometry();
    let frame_bytes = geometry.frame_bytes();
    let mut buffer = backing();
    let region = aligned_region(&mut buffer, geometry.region_len()).expect("fits");
    let mut ring = PcmRing::bind(region, geometry).expect("valid region");

    // Advance to frame 6 so the next six-frame write straddles the end.
    let priming = stamped(geometry, 0x10, 6);
    assert_eq!(ring.write(&priming[..6 * frame_bytes]), Ok(6));
    let mut sink = [0u8; 256];
    assert_eq!(ring.read(&mut sink[..6 * frame_bytes]), Ok(6));

    let straddling = stamped(geometry, 0x90, 6);
    assert_eq!(ring.write(&straddling[..6 * frame_bytes]), Ok(6));
    assert_eq!(ring.producer_position(), Ok(Frames::new(12)));

    let mut out = [0u8; 256];
    assert_eq!(ring.read(&mut out[..6 * frame_bytes]), Ok(6));
    assert_eq!(&out[..6 * frame_bytes], &straddling[..6 * frame_bytes]);
    assert_eq!(ring.consumer_position(), Ok(Frames::new(12)));
}

#[test]
fn a_full_ring_takes_no_more_and_a_short_write_reports_what_it_took() {
    let geometry = geometry();
    let frame_bytes = geometry.frame_bytes();
    let mut buffer = backing();
    let region = aligned_region(&mut buffer, geometry.region_len()).expect("fits");
    let mut ring = PcmRing::bind(region, geometry).expect("valid region");

    let samples = stamped(geometry, 0x21, 8);
    assert_eq!(ring.write(&samples[..8 * frame_bytes]), Ok(8));
    assert_eq!(ring.writable_frames(), Ok(0));
    assert_eq!(ring.write(&samples[..frame_bytes]), Ok(0));

    // Freeing three frames admits exactly three more, not four.
    let mut out = [0u8; 256];
    assert_eq!(ring.read(&mut out[..3 * frame_bytes]), Ok(3));
    assert_eq!(ring.write(&samples[..8 * frame_bytes]), Ok(3));
    assert_eq!(ring.writable_frames(), Ok(0));
}

#[test]
fn a_read_into_a_short_buffer_takes_only_the_whole_frames_that_fit() {
    let geometry = geometry();
    let frame_bytes = geometry.frame_bytes();
    let mut buffer = backing();
    let region = aligned_region(&mut buffer, geometry.region_len()).expect("fits");
    let mut ring = PcmRing::bind(region, geometry).expect("valid region");

    let samples = stamped(geometry, 0x55, 4);
    assert_eq!(ring.write(&samples[..4 * frame_bytes]), Ok(4));

    let mut out = [0u8; 256];
    // Two frames and a byte: the trailing byte cannot carry a frame.
    assert_eq!(ring.read(&mut out[..=(2 * frame_bytes)]), Ok(2));
    assert_eq!(&out[..2 * frame_bytes], &samples[..2 * frame_bytes]);
    assert_eq!(out[2 * frame_bytes], 0);
    assert_eq!(ring.readable_frames(), Ok(2));
    assert_eq!(ring.read(&mut out[..0]), Ok(0));
}

#[test]
fn silence_fills_the_gap_with_the_formats_own_quiet_value() {
    let geometry = PcmGeometry::new(4, SampleFormat::U8, 2).expect("valid");
    let mut buffer = backing();
    let region = aligned_region(&mut buffer, geometry.region_len()).expect("fits");
    let mut ring = PcmRing::bind(region, geometry).expect("valid region");

    assert_eq!(ring.write_silence(3), Ok(3));
    assert_eq!(ring.producer_position(), Ok(Frames::new(3)));
    let mut out = [0u8; 8];
    assert_eq!(ring.read(&mut out[..6]), Ok(3));
    assert_eq!(&out[..6], &[0x80; 6]);

    // Silence never overruns the ring either.
    assert_eq!(ring.write_silence(99), Ok(4));
    assert_eq!(ring.writable_frames(), Ok(0));
}

#[test]
fn discarding_advances_the_position_over_the_frames_it_drops() {
    let geometry = geometry();
    let frame_bytes = geometry.frame_bytes();
    let mut buffer = backing();
    let region = aligned_region(&mut buffer, geometry.region_len()).expect("fits");
    let mut ring = PcmRing::bind(region, geometry).expect("valid region");

    let samples = stamped(geometry, 0x70, 6);
    assert_eq!(ring.write(&samples[..6 * frame_bytes]), Ok(6));
    assert_eq!(ring.discard(2), Ok(2));
    assert_eq!(ring.consumer_position(), Ok(Frames::new(2)));
    assert_eq!(ring.readable_frames(), Ok(4));

    // The frames that follow a discard are still the frames that followed it.
    let mut out = [0u8; 256];
    assert_eq!(ring.read(&mut out[..4 * frame_bytes]), Ok(4));
    assert_eq!(
        &out[..4 * frame_bytes],
        &samples[2 * frame_bytes..6 * frame_bytes]
    );

    // A discard past the queue drops only what was queued.
    assert_eq!(ring.discard(99), Ok(0));
}

/// Both positions live in memory the peer can write, so a pair that could not
/// have arisen from the protocol is refused rather than acted on — an
/// occupancy past the ring would otherwise index outside the sample area.
#[test]
fn a_corrupt_position_pair_is_refused_rather_than_indexed_with() {
    let geometry = geometry();
    let mut buffer = backing();
    let region = aligned_region(&mut buffer, geometry.region_len()).expect("fits");

    // The consumer ahead of the producer: frames taken that were never
    // published.
    region[CACHE_LINE_BYTES..CACHE_LINE_BYTES + 8].copy_from_slice(&9u64.to_le_bytes());
    let mut ring = PcmRing::bind(region, geometry).expect("valid region");
    assert_eq!(ring.readable_frames(), Err(Errno::OutOfRange));
    assert_eq!(ring.writable_frames(), Err(Errno::OutOfRange));
    assert_eq!(ring.producer_position(), Err(Errno::OutOfRange));
    assert_eq!(ring.discard(1), Err(Errno::OutOfRange));
    let mut out = [0u8; 64];
    assert_eq!(ring.read(&mut out), Err(Errno::OutOfRange));
    assert_eq!(ring.write_silence(1), Err(Errno::OutOfRange));

    let mut over_full = backing();
    let region = aligned_region(&mut over_full, geometry.region_len()).expect("fits");
    // More frames in flight than the ring holds.
    region[..8].copy_from_slice(&u64::from(geometry.frames() + 1).to_le_bytes());
    let mut ring = PcmRing::bind(region, geometry).expect("valid region");
    assert_eq!(ring.readable_frames(), Err(Errno::OutOfRange));
    assert_eq!(ring.write(&[0u8; 0]), Err(Errno::OutOfRange));
}

/// Positions are monotone and never wrap, so a stream that has run for a
/// geological age still addresses its sample area correctly.
#[test]
fn an_enormous_position_still_addresses_the_right_frame() {
    let geometry = geometry();
    let frame_bytes = geometry.frame_bytes();
    let mut buffer = backing();
    let region = aligned_region(&mut buffer, geometry.region_len()).expect("fits");
    // A position far beyond anything a real stream reaches, and not a
    // multiple of the frame count, so the mask is doing real work.
    let far = u64::MAX - 8;
    region[..8].copy_from_slice(&far.to_le_bytes());
    region[CACHE_LINE_BYTES..CACHE_LINE_BYTES + 8].copy_from_slice(&far.to_le_bytes());
    let mut ring = PcmRing::bind(region, geometry).expect("valid region");

    let samples = stamped(geometry, 0xC3, 5);
    assert_eq!(ring.write(&samples[..5 * frame_bytes]), Ok(5));
    assert_eq!(ring.producer_position(), Ok(Frames::new(far + 5)));
    let mut out = [0u8; 256];
    assert_eq!(ring.read(&mut out[..5 * frame_bytes]), Ok(5));
    assert_eq!(&out[..5 * frame_bytes], &samples[..5 * frame_bytes]);
}
