//! Unit tests for the PCM vocabulary and the device-facts wire images.

use super::*;

fn stereo_rates() -> RateSupport {
    RateSupport::Discrete(
        RateSet::new(&[
            Rate::new(44_100).expect("in range"),
            Rate::HZ_48000,
            Rate::new(96_000).expect("in range"),
        ])
        .expect("ascending"),
    )
}

fn endpoint_facts() -> AudioEndpointFacts {
    AudioEndpointFacts {
        index: 3,
        direction: StreamDirection::Playback,
        jack: JackState::Present,
        formats: SampleFormats::EMPTY
            .with(SampleFormat::S16)
            .with(SampleFormat::S24In32),
        channel_map: ChannelMap::STEREO,
        rates: stereo_rates(),
        min_period_frames: 48,
        max_period_frames: 4_800,
        max_ring_frames: 16_384,
        gain: Some(GainRange::new(-6_400, 0, 50).expect("ordered")),
        name: AudioName::new("Green Line Out").expect("plain text"),
    }
}

#[test]
fn every_sample_format_round_trips_and_an_undefined_one_is_refused() {
    for format in [
        SampleFormat::U8,
        SampleFormat::S16,
        SampleFormat::S24,
        SampleFormat::S24In32,
        SampleFormat::S32,
        SampleFormat::F32,
    ] {
        assert_eq!(SampleFormat::from_u8(format.as_u8()), Ok(format));
    }
    assert_eq!(SampleFormat::from_u8(0), Err(Errno::OutOfRange));
    assert_eq!(SampleFormat::from_u8(7), Err(Errno::OutOfRange));
}

/// A zero byte is full negative deflection in unsigned 8-bit, so filling a
/// gap with it would click rather than fall silent.
#[test]
fn unsigned_eight_bit_silence_is_mid_scale_and_every_other_format_is_zero() {
    assert_eq!(SampleFormat::U8.silence_byte(), 0x80);
    for format in [
        SampleFormat::S16,
        SampleFormat::S24,
        SampleFormat::S24In32,
        SampleFormat::S32,
        SampleFormat::F32,
    ] {
        assert_eq!(format.silence_byte(), 0x00);
    }
}

#[test]
fn a_sample_formats_storage_width_and_resolution_differ_only_for_packed_24() {
    assert_eq!(SampleFormat::S24.bytes_per_sample(), 3);
    assert_eq!(SampleFormat::S24.valid_bits(), 24);
    assert_eq!(SampleFormat::S24In32.bytes_per_sample(), 4);
    assert_eq!(SampleFormat::S24In32.valid_bits(), 24);
    assert_eq!(SampleFormat::F32.bytes_per_sample(), 4);
    assert!(SampleFormat::F32.is_float());
    assert!(!SampleFormat::S32.is_float());
}

#[test]
fn a_format_set_admits_only_what_was_added_and_refuses_an_undefined_bit() {
    let set = SampleFormats::EMPTY
        .with(SampleFormat::S16)
        .with(SampleFormat::F32);
    assert!(set.contains(SampleFormat::S16));
    assert!(set.contains(SampleFormat::F32));
    assert!(!set.contains(SampleFormat::U8));
    assert!(!set.is_empty());
    assert!(SampleFormats::EMPTY.is_empty());
    assert_eq!(SampleFormats::from_bits(set.bits()), Ok(set));
    assert_eq!(SampleFormats::from_bits(1 << 15), Err(Errno::OutOfRange));
}

#[test]
fn every_channel_position_round_trips_and_an_undefined_one_is_refused() {
    for position in [
        ChannelPosition::Mono,
        ChannelPosition::FrontLeft,
        ChannelPosition::FrontRight,
        ChannelPosition::FrontCentre,
        ChannelPosition::LowFrequency,
        ChannelPosition::RearLeft,
        ChannelPosition::RearRight,
        ChannelPosition::SideLeft,
        ChannelPosition::SideRight,
    ] {
        assert_eq!(ChannelPosition::from_u8(position.as_u8()), Ok(position));
    }
    assert_eq!(ChannelPosition::from_u8(0), Err(Errno::OutOfRange));
    assert_eq!(ChannelPosition::from_u8(10), Err(Errno::OutOfRange));
}

#[test]
fn the_constant_channel_maps_are_the_maps_they_name() {
    assert_eq!(ChannelMap::MONO.channels(), 1);
    assert_eq!(ChannelMap::MONO.positions(), &[ChannelPosition::Mono]);
    assert_eq!(ChannelMap::STEREO.channels(), 2);
    assert_eq!(
        ChannelMap::STEREO.positions(),
        &[ChannelPosition::FrontLeft, ChannelPosition::FrontRight]
    );
    assert_eq!(
        ChannelMap::new(&[ChannelPosition::FrontLeft, ChannelPosition::FrontRight]),
        Ok(ChannelMap::STEREO)
    );
}

/// A duplicate position would make the downmix matrix ambiguous, and a mono
/// channel beside another is a contradiction, so both are refused rather than
/// resolved.
#[test]
fn a_channel_map_refuses_a_degenerate_layout() {
    assert_eq!(ChannelMap::new(&[]), Err(Errno::LengthOutOfRange));
    assert_eq!(
        ChannelMap::new(&[ChannelPosition::FrontLeft; MAX_CHANNELS + 1]),
        Err(Errno::LengthOutOfRange)
    );
    assert_eq!(
        ChannelMap::new(&[ChannelPosition::FrontLeft, ChannelPosition::FrontLeft]),
        Err(Errno::OutOfRange)
    );
    assert_eq!(
        ChannelMap::new(&[ChannelPosition::Mono, ChannelPosition::FrontRight]),
        Err(Errno::OutOfRange)
    );
}

#[test]
fn a_full_width_channel_map_round_trips_through_the_wire() {
    let surround = ChannelMap::new(&[
        ChannelPosition::FrontLeft,
        ChannelPosition::FrontRight,
        ChannelPosition::FrontCentre,
        ChannelPosition::LowFrequency,
        ChannelPosition::RearLeft,
        ChannelPosition::RearRight,
        ChannelPosition::SideLeft,
        ChannelPosition::SideRight,
    ])
    .expect("7.1 is representable");
    assert_eq!(surround.channels(), 8);
    assert_eq!(ChannelMap::from_wire(&surround.to_wire()), Ok(surround));
}

#[test]
fn a_channel_map_decode_fails_closed() {
    let wire = ChannelMap::STEREO.to_wire();
    assert_eq!(
        ChannelMap::from_wire(&wire[..CHANNEL_MAP_WIRE_LEN - 1]),
        Err(Errno::BufferTooSmall)
    );

    let mut zero_count = wire;
    zero_count[0] = 0;
    assert_eq!(
        ChannelMap::from_wire(&zero_count),
        Err(Errno::LengthOutOfRange)
    );

    let mut over_count = wire;
    over_count[0] = u8::try_from(MAX_CHANNELS + 1).expect("small");
    assert_eq!(
        ChannelMap::from_wire(&over_count),
        Err(Errno::LengthOutOfRange)
    );

    // A byte past the declared count would be a field the decoder never saw.
    let mut dirty_tail = wire;
    dirty_tail[CHANNEL_MAP_WIRE_LEN - 1] = 1;
    assert_eq!(ChannelMap::from_wire(&dirty_tail), Err(Errno::BadMagic));

    let mut undefined = wire;
    undefined[1] = 200;
    assert_eq!(ChannelMap::from_wire(&undefined), Err(Errno::OutOfRange));

    // Uniqueness is re-checked on decode, not merely on construction.
    let mut duplicate = wire;
    duplicate[2] = duplicate[1];
    assert_eq!(ChannelMap::from_wire(&duplicate), Err(Errno::OutOfRange));
}

#[test]
fn a_rate_outside_the_converter_range_is_refused() {
    assert_eq!(Rate::new(Rate::MIN_HZ).map(Rate::hz), Ok(Rate::MIN_HZ));
    assert_eq!(Rate::new(Rate::MAX_HZ).map(Rate::hz), Ok(Rate::MAX_HZ));
    assert_eq!(Rate::new(Rate::MIN_HZ - 1), Err(Errno::OutOfRange));
    assert_eq!(Rate::new(Rate::MAX_HZ + 1), Err(Errno::OutOfRange));
    assert_eq!(Rate::HZ_48000.hz(), 48_000);
}

#[test]
fn a_rate_set_must_be_strictly_ascending() {
    assert_eq!(RateSet::new(&[]), Err(Errno::LengthOutOfRange));
    assert_eq!(
        RateSet::new(&[Rate::HZ_48000, Rate::new(44_100).expect("in range")]),
        Err(Errno::OutOfRange)
    );
    assert_eq!(
        RateSet::new(&[Rate::HZ_48000, Rate::HZ_48000]),
        Err(Errno::OutOfRange)
    );
    assert_eq!(
        RateSet::new(&[Rate::HZ_48000; MAX_DEVICE_RATES + 1]),
        Err(Errno::LengthOutOfRange)
    );
}

/// Ties go upward: resampling to a higher rate loses no band.
#[test]
fn the_nearest_supported_rate_breaks_a_tie_upward() {
    let support = stereo_rates();
    assert!(support.admits(Rate::HZ_48000));
    assert!(!support.admits(Rate::new(88_200).expect("in range")));
    assert_eq!(support.nearest(Rate::HZ_48000), Rate::HZ_48000);
    assert_eq!(
        support.nearest(Rate::new(46_050).expect("in range")),
        Rate::HZ_48000
    );
    assert_eq!(
        support.nearest(Rate::new(8_000).expect("in range")),
        Rate::new(44_100).expect("in range")
    );
    assert_eq!(
        support.nearest(Rate::new(768_000).expect("in range")),
        Rate::new(96_000).expect("in range")
    );
}

#[test]
fn a_continuous_clock_admits_its_whole_range_and_clamps_outside_it() {
    let support = RateSupport::Continuous {
        min: Rate::new(8_000).expect("in range"),
        max: Rate::new(192_000).expect("in range"),
    };
    assert!(support.admits(Rate::new(44_101).expect("in range")));
    assert!(!support.admits(Rate::new(4_000).expect("in range")));
    assert_eq!(
        support.nearest(Rate::new(4_000).expect("in range")),
        Rate::new(8_000).expect("in range")
    );
    assert_eq!(
        support.nearest(Rate::new(768_000).expect("in range")),
        Rate::new(192_000).expect("in range")
    );
    assert_eq!(RateSupport::from_wire(&support.to_wire()), Ok(support));
}

#[test]
fn a_discrete_rate_list_round_trips_at_its_full_width() {
    let every = [
        8_000, 11_025, 16_000, 22_050, 32_000, 44_100, 48_000, 64_000, 88_200, 96_000, 176_400,
        192_000, 352_800, 384_000, 705_600, 768_000,
    ]
    .map(|hz| Rate::new(hz).expect("in range"));
    assert_eq!(every.len(), MAX_DEVICE_RATES);
    let support = RateSupport::Discrete(RateSet::new(&every).expect("ascending"));
    assert_eq!(RateSupport::from_wire(&support.to_wire()), Ok(support));
}

#[test]
fn a_rate_support_decode_fails_closed() {
    let wire = stereo_rates().to_wire();
    assert_eq!(
        RateSupport::from_wire(&wire[..RATE_SUPPORT_WIRE_LEN - 1]),
        Err(Errno::BufferTooSmall)
    );

    let mut unknown_kind = wire;
    unknown_kind[0] = 9;
    assert_eq!(
        RateSupport::from_wire(&unknown_kind),
        Err(Errno::OutOfRange)
    );

    let mut dirty_reserved = wire;
    dirty_reserved[2] = 1;
    assert_eq!(
        RateSupport::from_wire(&dirty_reserved),
        Err(Errno::BadMagic)
    );

    let mut dirty_tail = wire;
    dirty_tail[RATE_SUPPORT_WIRE_LEN - 1] = 1;
    assert_eq!(RateSupport::from_wire(&dirty_tail), Err(Errno::BadMagic));

    let mut empty_list = wire;
    empty_list[1] = 0;
    assert_eq!(RateSupport::from_wire(&empty_list), Err(Errno::OutOfRange));

    // A declared count past the array bound is a corrupt frame, not a longer
    // list.
    let mut over_count = wire;
    over_count[1] = u8::try_from(MAX_DEVICE_RATES + 1).expect("small");
    assert_eq!(RateSupport::from_wire(&over_count), Err(Errno::OutOfRange));

    // A descending pair would let a `nearest` search walk a list that is not
    // the set it claims to be.
    let mut descending = wire;
    descending[4..8].copy_from_slice(&96_000u32.to_le_bytes());
    descending[12..16].copy_from_slice(&44_100u32.to_le_bytes());
    assert_eq!(RateSupport::from_wire(&descending), Err(Errno::OutOfRange));

    let mut inverted = RateSupport::Continuous {
        min: Rate::new(8_000).expect("in range"),
        max: Rate::new(192_000).expect("in range"),
    }
    .to_wire();
    inverted[4..8].copy_from_slice(&192_000u32.to_le_bytes());
    inverted[8..12].copy_from_slice(&8_000u32.to_le_bytes());
    assert_eq!(RateSupport::from_wire(&inverted), Err(Errno::OutOfRange));

    let mut wrong_count = RateSupport::Continuous {
        min: Rate::new(8_000).expect("in range"),
        max: Rate::new(192_000).expect("in range"),
    }
    .to_wire();
    wrong_count[1] = 1;
    assert_eq!(RateSupport::from_wire(&wrong_count), Err(Errno::OutOfRange));
}

#[test]
fn a_frame_position_converts_to_time_exactly_at_the_second_boundary() {
    assert_eq!(
        Frames::new(48_000).duration(Rate::HZ_48000),
        Duration64::from_secs(1)
    );
    assert_eq!(
        Frames::new(24_000).duration(Rate::HZ_48000),
        Duration64::new(0, 500_000_000).expect("canonical")
    );
    // 1/44100 s is 22675.7… ns; the conversion floors rather than rounding, so
    // a position never reports a time it has not reached.
    let rate = Rate::new(44_100).expect("in range");
    assert_eq!(
        Frames::new(1).duration(rate),
        Duration64::new(0, 22_675).expect("canonical")
    );
    assert_eq!(
        Frames::new(44_100 * 3_600).duration(rate),
        Duration64::from_secs(3_600)
    );
}

#[test]
fn a_frame_position_round_trips_through_a_whole_second_span() {
    let rate = Rate::new(44_100).expect("in range");
    for frames in [0u64, 1, 44_099, 44_100, 44_100 * 7_200] {
        let position = Frames::new(frames);
        let span = position.duration(rate);
        // Flooring costs at most the frame the remainder sat inside.
        let back = Frames::from_duration(span, rate).expect("non-negative");
        assert!(back.get() <= frames && frames - back.get() <= 1);
    }
    assert_eq!(
        Frames::from_duration(Duration64::from_secs(-1), Rate::HZ_48000),
        None
    );
}

/// A backwards span is a corrupt pair of positions; answering zero would hide
/// it.
#[test]
fn a_frame_span_refuses_to_run_backwards() {
    assert_eq!(Frames::new(90).since(Frames::new(40)), Some(50));
    assert_eq!(Frames::new(40).since(Frames::new(90)), None);
    assert_eq!(Frames::ZERO.checked_add(7), Some(Frames::new(7)));
    assert_eq!(Frames::new(u64::MAX).checked_add(1), None);
}

#[test]
fn every_stream_direction_and_jack_state_round_trips() {
    for direction in [StreamDirection::Playback, StreamDirection::Capture] {
        assert_eq!(StreamDirection::from_u8(direction.as_u8()), Ok(direction));
    }
    assert_eq!(StreamDirection::from_u8(2), Err(Errno::OutOfRange));
    for jack in [JackState::Unknown, JackState::Present, JackState::Absent] {
        assert_eq!(JackState::from_u8(jack.as_u8()), Ok(jack));
    }
    assert_eq!(JackState::from_u8(3), Err(Errno::OutOfRange));
}

#[test]
fn a_gain_control_that_cannot_move_is_absent_rather_than_a_range() {
    assert_eq!(GainRange::new(0, -1, 50), Err(Errno::OutOfRange));
    assert_eq!(GainRange::new(-6_400, 0, 0), Err(Errno::OutOfRange));
    let range = GainRange::new(-6_400, 600, 50).expect("ordered");
    assert_eq!(range.min_millibel(), -6_400);
    assert_eq!(range.max_millibel(), 600);
    assert_eq!(range.step_millibel(), 50);
}

#[test]
fn a_gain_slot_round_trips_present_and_absent() {
    let range = GainRange::new(-6_400, 600, 50).expect("ordered");
    assert_eq!(
        GainRange::from_wire(&GainRange::to_wire(Some(range))),
        Ok(Some(range))
    );
    assert_eq!(GainRange::from_wire(&GainRange::to_wire(None)), Ok(None));
}

#[test]
fn a_gain_slot_decode_fails_closed() {
    let wire = GainRange::to_wire(None);
    assert_eq!(
        GainRange::from_wire(&wire[..GAIN_RANGE_WIRE_LEN - 1]),
        Err(Errno::BufferTooSmall)
    );

    let mut undefined = wire;
    undefined[0] = 2;
    assert_eq!(GainRange::from_wire(&undefined), Err(Errno::OutOfRange));

    let mut dirty_reserved = wire;
    dirty_reserved[3] = 1;
    assert_eq!(GainRange::from_wire(&dirty_reserved), Err(Errno::BadMagic));

    // An absent control carrying bounds would be a hidden field.
    let mut populated_absent = wire;
    populated_absent[4] = 1;
    assert_eq!(
        GainRange::from_wire(&populated_absent),
        Err(Errno::BadMagic)
    );

    let mut zero_step = GainRange::to_wire(Some(GainRange::new(-6_400, 600, 50).expect("ordered")));
    zero_step[12..16].copy_from_slice(&0u32.to_le_bytes());
    assert_eq!(GainRange::from_wire(&zero_step), Err(Errno::OutOfRange));
}

#[test]
fn device_facts_round_trip_and_fail_closed() {
    let facts = AudioDeviceFacts {
        endpoints: 4,
        name: AudioName::new("HDA Intel PCH").expect("plain text"),
    };
    let wire = facts.to_wire();
    assert_eq!(AudioDeviceFacts::from_wire(&wire), Ok(facts));
    assert_eq!(
        AudioDeviceFacts::from_wire(&wire[..AUDIO_DEVICE_FACTS_WIRE_LEN - 1]),
        Err(Errno::BufferTooSmall)
    );

    let mut dirty_reserved = wire;
    dirty_reserved[2] = 1;
    assert_eq!(
        AudioDeviceFacts::from_wire(&dirty_reserved),
        Err(Errno::BadMagic)
    );

    let mut too_many = wire;
    too_many[..2].copy_from_slice(&(MAX_DEVICE_ENDPOINTS + 1).to_le_bytes());
    assert_eq!(
        AudioDeviceFacts::from_wire(&too_many),
        Err(Errno::OutOfRange)
    );

    let mut over_long_name = wire;
    over_long_name[4] = u8::try_from(AUDIO_NAME_MAX + 1).expect("small");
    assert_eq!(
        AudioDeviceFacts::from_wire(&over_long_name),
        Err(Errno::LengthOutOfRange)
    );
}

#[test]
fn endpoint_facts_round_trip_through_the_wire() {
    let facts = endpoint_facts();
    assert_eq!(facts.validate(), Ok(()));
    assert_eq!(AudioEndpointFacts::from_wire(&facts.to_wire()), Ok(facts));

    let bare = AudioEndpointFacts {
        index: 0,
        direction: StreamDirection::Capture,
        jack: JackState::Unknown,
        formats: SampleFormats::EMPTY.with(SampleFormat::U8),
        channel_map: ChannelMap::MONO,
        rates: RateSupport::Continuous {
            min: Rate::new(8_000).expect("in range"),
            max: Rate::new(48_000).expect("in range"),
        },
        min_period_frames: 1,
        max_period_frames: 1,
        max_ring_frames: ring_bounds::MIN_FRAMES,
        gain: None,
        name: AudioName::new("").expect("optional"),
    };
    assert_eq!(bare.validate(), Ok(()));
    assert_eq!(AudioEndpointFacts::from_wire(&bare.to_wire()), Ok(bare));
}

#[test]
fn endpoint_facts_refuse_an_impossible_device() {
    let mut facts = endpoint_facts();
    facts.index = MAX_DEVICE_ENDPOINTS;
    assert_eq!(facts.validate(), Err(Errno::OutOfRange));

    let mut facts = endpoint_facts();
    facts.formats = SampleFormats::EMPTY;
    assert_eq!(facts.validate(), Err(Errno::OutOfRange));

    let mut facts = endpoint_facts();
    facts.min_period_frames = 0;
    assert_eq!(facts.validate(), Err(Errno::OutOfRange));

    let mut facts = endpoint_facts();
    facts.min_period_frames = facts.max_period_frames + 1;
    assert_eq!(facts.validate(), Err(Errno::OutOfRange));

    // A ring that cannot hold one period could never be serviced, so a device
    // whose *longest* period outruns its whole buffer is reporting nonsense
    // rather than a deep ring.
    let mut facts = endpoint_facts();
    facts.max_ring_frames = facts.min_period_frames - 1;
    assert_eq!(facts.validate(), Err(Errno::OutOfRange));

    let mut facts = endpoint_facts();
    facts.max_ring_frames = facts.max_period_frames - 1;
    assert_eq!(facts.validate(), Err(Errno::OutOfRange));

    let mut facts = endpoint_facts();
    facts.max_ring_frames = facts.max_period_frames;
    assert_eq!(facts.validate(), Ok(()));

    let mut facts = endpoint_facts();
    facts.max_ring_frames = ring_bounds::MAX_FRAMES + 1;
    assert_eq!(facts.validate(), Err(Errno::OutOfRange));
}

#[test]
fn endpoint_facts_decode_fails_closed() {
    let wire = endpoint_facts().to_wire();
    assert_eq!(
        AudioEndpointFacts::from_wire(&wire[..AUDIO_ENDPOINT_FACTS_WIRE_LEN - 1]),
        Err(Errno::BufferTooSmall)
    );

    for dirty in [endpoint::RESERVED0, endpoint::RESERVED1] {
        let mut corrupt = wire;
        corrupt[dirty] = 1;
        assert_eq!(
            AudioEndpointFacts::from_wire(&corrupt),
            Err(Errno::BadMagic),
            "reserved byte {dirty} must be refused"
        );
    }

    let mut undefined_direction = wire;
    undefined_direction[endpoint::DIRECTION] = 9;
    assert_eq!(
        AudioEndpointFacts::from_wire(&undefined_direction),
        Err(Errno::OutOfRange)
    );

    // The embedded values are re-validated, not trusted because the frame
    // reached the right length.
    let mut no_formats = wire;
    no_formats[endpoint::FORMATS..endpoint::FORMATS + 2].copy_from_slice(&0u16.to_le_bytes());
    assert_eq!(
        AudioEndpointFacts::from_wire(&no_formats),
        Err(Errno::OutOfRange)
    );

    let mut over_deep = wire;
    over_deep[endpoint::MAX_RING..endpoint::MAX_RING + 4]
        .copy_from_slice(&(ring_bounds::MAX_FRAMES + 1).to_le_bytes());
    assert_eq!(
        AudioEndpointFacts::from_wire(&over_deep),
        Err(Errno::OutOfRange)
    );
}
