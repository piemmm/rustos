//! Unit tests for `audio-v1`: every request, reply and notification
//! round-trips, and every malformed frame is refused rather than guessed at.

use super::*;
use crate::driver::audio::RateSet;

fn open_params() -> OpenParams {
    OpenParams {
        device_id: 7,
        direction: StreamDirection::Playback,
        format: SampleFormat::F32,
        rate: Rate::HZ_48000,
        channel_map: ChannelMap::STEREO,
        role: StreamRole::Media,
        latency_target_frames: 960,
    }
}

fn descriptor() -> AudioDeviceDescriptor {
    AudioDeviceDescriptor {
        device_id: 7,
        direction: StreamDirection::Playback,
        jack: JackState::Present,
        is_default: true,
        formats: SampleFormats::EMPTY
            .with(SampleFormat::S16)
            .with(SampleFormat::F32),
        channel_map: ChannelMap::STEREO,
        rates: RateSupport::Discrete(
            RateSet::new(&[Rate::new(44_100).expect("in range"), Rate::HZ_48000])
                .expect("ascending"),
        ),
        gain: Some(GainRange::new(-6_400, 0, 50).expect("ordered")),
        name: AudioName::new("Headphones").expect("plain text"),
    }
}

fn stream_grant() -> StreamGrant {
    StreamGrant {
        stream_id: 0x0102_0304_0506_0708,
        notify_endpoint: notify_endpoint_for(4_096, 2),
        rate: Rate::HZ_48000,
        format: SampleFormat::F32,
        channel_map: ChannelMap::STEREO,
        ring_frames: 4_096,
        granted_latency_frames: 960,
        granted_latency: Duration64::new(0, 20_000_000).expect("canonical"),
        clock_domain: 3,
    }
}

fn every_request() -> [AudioRequest; 12] {
    [
        AudioRequest::Enumerate {
            direction: StreamDirection::Capture,
            index: 5,
        },
        AudioRequest::Open(open_params()),
        AudioRequest::Attach {
            stream_id: 42,
            region_grant: 0xDEAD_BEEF_CAFE_F00D,
        },
        AudioRequest::Start {
            stream_id: 42,
            at: Frames::new(u64::MAX / 7),
        },
        AudioRequest::Stop {
            stream_id: 42,
            at: Frames::new(96_000),
        },
        AudioRequest::Drain { stream_id: 42 },
        AudioRequest::Flush { stream_id: 42 },
        AudioRequest::Clock { stream_id: 42 },
        AudioRequest::Gain {
            stream_id: 42,
            millibel: -2_000,
        },
        AudioRequest::Mute {
            stream_id: 42,
            muted: true,
        },
        AudioRequest::State { stream_id: 42 },
        AudioRequest::Close { stream_id: 42 },
    ]
}

fn encoded(request: AudioRequest) -> ([u8; AUDIO_MAX_REQUEST], usize) {
    let mut out = [0u8; AUDIO_MAX_REQUEST];
    let len = request.encode(&mut out).expect("buffer is the maximum");
    (out, len)
}

#[test]
fn the_service_rendezvous_is_reserved_so_no_squatter_can_claim_it() {
    assert!(crate::ipc::is_reserved_endpoint(AUDIO_ENDPOINT));
    // It is not seat-scoped: one machine service arbitrates every seat's
    // sound, so a session's lease never substitutes for the bind gate.
    assert!(!crate::ipc::is_seat_scoped_endpoint(AUDIO_ENDPOINT));
}

/// The three fields tile the word exactly, and the result is an ordinary id
/// the client binds without a privileged bind.
#[test]
fn a_stream_notify_endpoint_packs_pid_and_slot_without_reaching_the_tag() {
    for pid in [0u64, 1, 4_096, crate::PID_MAX] {
        for slot in [0u64, 1, MAX_CLIENT_STREAM_SLOTS - 1] {
            let endpoint = notify_endpoint_for(pid, slot);
            assert_eq!(endpoint & 0xFF, slot);
            assert_eq!((endpoint >> 8) & crate::PID_MAX, pid);
            assert_eq!(endpoint >> 48, AUDIO_CLIENT_NOTIFY_TAG >> 48);
            assert!(!crate::ipc::is_reserved_endpoint(endpoint));
        }
    }
    assert_ne!(
        notify_endpoint_for(crate::PID_MAX, 0),
        notify_endpoint_for(crate::PID_MAX - 1, 0)
    );
}

#[test]
fn every_request_round_trips() {
    for request in every_request() {
        let (frame, len) = encoded(request);
        assert_eq!(
            AudioRequest::decode(&frame[..len]),
            Ok(request),
            "{request:?} must survive the wire"
        );
    }
}

#[test]
fn a_request_frame_is_exactly_as_long_as_its_operation_needs() {
    for request in every_request() {
        let (_, len) = encoded(request);
        assert!((HEADER_LEN..=AUDIO_MAX_REQUEST).contains(&len));
        let mut short = [0u8; AUDIO_MAX_REQUEST];
        assert_eq!(
            request.encode(&mut short[..len - 1]),
            Err(Errno::BufferTooSmall)
        );
    }
}

#[test]
fn a_truncated_request_is_refused_rather_than_read_short() {
    for request in every_request() {
        let (frame, len) = encoded(request);
        for cut in 0..len {
            assert_eq!(
                AudioRequest::decode(&frame[..cut]),
                Err(Errno::BufferTooSmall),
                "{request:?} truncated to {cut} bytes must be refused"
            );
        }
    }
}

#[test]
fn a_request_header_fails_closed() {
    let (frame, len) = encoded(AudioRequest::Close { stream_id: 42 });

    let mut wrong_magic = frame;
    wrong_magic[0] ^= 0xFF;
    assert_eq!(
        AudioRequest::decode(&wrong_magic[..len]),
        Err(Errno::BadMagic)
    );

    let mut wrong_version = frame;
    put_u16(&mut wrong_version, 4, AUDIO_VERSION_V1 + 1);
    assert_eq!(
        AudioRequest::decode(&wrong_version[..len]),
        Err(Errno::AbiVersionUnsupported)
    );

    let mut dirty_reserved = frame;
    dirty_reserved[7] = 1;
    assert_eq!(
        AudioRequest::decode(&dirty_reserved[..len]),
        Err(Errno::BadMagic)
    );

    let mut unknown_op = frame;
    unknown_op[6] = 250;
    assert_eq!(
        AudioRequest::decode(&unknown_op[..len]),
        Err(Errno::OutOfRange)
    );
}

/// Zero is what a truncated or uninitialised frame carries, so reserving it
/// turns a confused request into a refusal rather than an operation on
/// whichever stream happened to be first.
#[test]
fn a_zero_stream_id_is_refused_on_every_operation_that_names_one() {
    for request in every_request() {
        let (mut frame, len) = encoded(request);
        if matches!(
            request,
            AudioRequest::Enumerate { .. } | AudioRequest::Open(_)
        ) {
            continue;
        }
        put_u64(&mut frame, HEADER_LEN, 0);
        assert_eq!(
            AudioRequest::decode(&frame[..len]),
            Err(Errno::OutOfRange),
            "{request:?} must refuse a zero stream id"
        );
    }
}

#[test]
fn enumerate_fails_closed() {
    let (frame, len) = encoded(AudioRequest::Enumerate {
        direction: StreamDirection::Capture,
        index: 5,
    });

    let mut dirty = frame;
    dirty[HEADER_LEN + enumerate::RESERVED] = 1;
    assert_eq!(AudioRequest::decode(&dirty[..len]), Err(Errno::BadMagic));

    let mut undefined = frame;
    undefined[HEADER_LEN + enumerate::DIRECTION] = 9;
    assert_eq!(
        AudioRequest::decode(&undefined[..len]),
        Err(Errno::OutOfRange)
    );
}

#[test]
fn open_fails_closed_on_every_malformed_field() {
    let (frame, len) = encoded(AudioRequest::Open(open_params()));
    let body = HEADER_LEN;

    for dirty in [
        body + open::RESERVED0,
        body + open::RESERVED1,
        body + open::RESERVED1 + 2,
    ] {
        let mut corrupt = frame;
        corrupt[dirty] = 1;
        assert_eq!(
            AudioRequest::decode(&corrupt[..len]),
            Err(Errno::BadMagic),
            "reserved byte {dirty} must be refused"
        );
    }

    for (offset, value) in [(open::DIRECTION, 9u8), (open::FORMAT, 99), (open::ROLE, 42)] {
        let mut corrupt = frame;
        corrupt[body + offset] = value;
        assert_eq!(
            AudioRequest::decode(&corrupt[..len]),
            Err(Errno::OutOfRange)
        );
    }

    let mut impossible_rate = frame;
    put_u32(&mut impossible_rate, body + open::RATE, 3);
    assert_eq!(
        AudioRequest::decode(&impossible_rate[..len]),
        Err(Errno::OutOfRange)
    );

    // A target of zero asks for no buffering, and one past the ring bound asks
    // for memory no grant may pin; neither is silently clamped.
    for latency in [0, ring_bounds::MAX_FRAMES + 1] {
        let mut corrupt = frame;
        put_u32(&mut corrupt, body + open::LATENCY, latency);
        assert_eq!(
            AudioRequest::decode(&corrupt[..len]),
            Err(Errno::OutOfRange)
        );
    }

    let mut no_channels = frame;
    no_channels[body + open::CHANNEL_MAP] = 0;
    assert_eq!(
        AudioRequest::decode(&no_channels[..len]),
        Err(Errno::LengthOutOfRange)
    );
}

#[test]
fn gain_and_mute_refuse_their_dirty_reserved_fields() {
    let (frame, len) = encoded(AudioRequest::Gain {
        stream_id: 42,
        millibel: -2_000,
    });
    let mut dirty = frame;
    dirty[HEADER_LEN + level::RESERVED] = 1;
    assert_eq!(AudioRequest::decode(&dirty[..len]), Err(Errno::BadMagic));

    let (frame, len) = encoded(AudioRequest::Mute {
        stream_id: 42,
        muted: false,
    });
    let mut dirty = frame;
    dirty[HEADER_LEN + level::MUTED + 3] = 1;
    assert_eq!(AudioRequest::decode(&dirty[..len]), Err(Errno::BadMagic));

    let mut undefined = frame;
    undefined[HEADER_LEN + level::MUTED] = 3;
    assert_eq!(
        AudioRequest::decode(&undefined[..len]),
        Err(Errno::OutOfRange)
    );
}

#[test]
fn a_device_descriptor_round_trips_and_carries_a_refusal_verbatim() {
    let device = descriptor();
    assert_eq!(
        decode_enumerate_reply(&encode_enumerate_reply(Ok(device))),
        Ok(device)
    );
    // Past the end of the list is an ordinary answer, not a failure of the
    // protocol.
    assert_eq!(
        decode_enumerate_reply(&encode_enumerate_reply(Err(Errno::NotFound))),
        Err(Errno::NotFound)
    );
    assert_eq!(
        decode_enumerate_reply(&encode_enumerate_reply(Ok(device))[..3]),
        Err(Errno::BufferTooSmall)
    );
}

#[test]
fn a_device_descriptor_decode_fails_closed() {
    let wire = encode_enumerate_reply(Ok(descriptor()));

    for dirty in [
        descriptor::RESERVED0,
        descriptor::RESERVED1,
        descriptor::RESERVED2,
    ] {
        let mut corrupt = wire;
        corrupt[4 + dirty] = 1;
        assert_eq!(
            decode_enumerate_reply(&corrupt),
            Err(Errno::BadMagic),
            "reserved byte {dirty} must be refused"
        );
    }

    let mut undefined_default = wire;
    undefined_default[4 + descriptor::DEFAULT] = 2;
    assert_eq!(
        decode_enumerate_reply(&undefined_default),
        Err(Errno::OutOfRange)
    );

    // A device accepting no format at all could never be opened.
    let mut no_formats = wire;
    put_u16(&mut no_formats, 4 + descriptor::FORMATS, 0);
    assert_eq!(decode_enumerate_reply(&no_formats), Err(Errno::OutOfRange));

    let mut undefined_jack = wire;
    undefined_jack[4 + descriptor::JACK] = 7;
    assert_eq!(
        decode_enumerate_reply(&undefined_jack),
        Err(Errno::OutOfRange)
    );
}

#[test]
fn a_stream_grant_round_trips() {
    let granted = stream_grant();
    assert_eq!(
        decode_open_reply(&encode_open_reply(Ok(granted))),
        Ok(granted)
    );
    assert_eq!(
        decode_open_reply(&encode_open_reply(Err(Errno::PermissionDenied))),
        Err(Errno::PermissionDenied)
    );
    assert_eq!(
        decode_open_reply(&encode_open_reply(Ok(granted))[..AUDIO_OPEN_REPLY_LEN - 1]),
        Err(Errno::BufferTooSmall)
    );
}

#[test]
fn a_stream_grant_decode_fails_closed() {
    let wire = encode_open_reply(Ok(stream_grant()));

    for dirty in [
        stream_grant::RESERVED0,
        stream_grant::RESERVED1,
        stream_grant::RESERVED2,
    ] {
        let mut corrupt = wire;
        corrupt[4 + dirty] = 1;
        assert_eq!(
            decode_open_reply(&corrupt),
            Err(Errno::BadMagic),
            "reserved byte {dirty} must be refused"
        );
    }

    let mut zero_stream = wire;
    put_u64(&mut zero_stream, 4 + stream_grant::STREAM, 0);
    assert_eq!(decode_open_reply(&zero_stream), Err(Errno::OutOfRange));

    // A grant naming a reserved rendezvous would have the client bind a system
    // service's id.
    let mut hijacked_notify = wire;
    put_u64(
        &mut hijacked_notify,
        4 + stream_grant::NOTIFY,
        crate::sysinfo::SYSINFO_ENDPOINT,
    );
    assert_eq!(decode_open_reply(&hijacked_notify), Err(Errno::OutOfRange));

    for frames in [0, 3, 1_000, ring_bounds::MAX_FRAMES * 2] {
        let mut corrupt = wire;
        put_u32(&mut corrupt, 4 + stream_grant::RING_FRAMES, frames);
        assert_eq!(
            decode_open_reply(&corrupt),
            Err(Errno::OutOfRange),
            "a {frames}-frame ring must be refused"
        );
    }

    // A latency the ring could not hold describes a stream that cannot exist.
    for latency in [0, 8_192] {
        let mut corrupt = wire;
        put_u32(&mut corrupt, 4 + stream_grant::LATENCY_FRAMES, latency);
        assert_eq!(decode_open_reply(&corrupt), Err(Errno::OutOfRange));
    }
}

#[test]
fn a_clock_report_round_trips_and_refuses_an_impossible_rate() {
    let report = ClockReport {
        // The measured rate of a device whose crystal claims 48 kHz.
        rate_millihertz: 47_998_600,
        position: Frames::new(144_000_000),
        sampled_at: Time64::new(1_700_000_000, 500).expect("canonical"),
    };
    assert_eq!(
        decode_clock_reply(&encode_clock_reply(Ok(report))),
        Ok(report)
    );
    assert_eq!(
        decode_clock_reply(&encode_clock_reply(Err(Errno::NotFound))),
        Err(Errno::NotFound)
    );

    let wire = encode_clock_reply(Ok(report));
    assert_eq!(
        decode_clock_reply(&wire[..AUDIO_CLOCK_REPLY_LEN - 1]),
        Err(Errno::BufferTooSmall)
    );

    let mut dirty = wire;
    dirty[4 + clock::RESERVED] = 1;
    assert_eq!(decode_clock_reply(&dirty), Err(Errno::BadMagic));

    for rate in [0, 1_000, u32::MAX] {
        let mut corrupt = wire;
        put_u32(&mut corrupt, 4 + clock::RATE_MILLIHERTZ, rate);
        assert_eq!(decode_clock_reply(&corrupt), Err(Errno::OutOfRange));
    }

    let mut bad_time = wire;
    put_u32(&mut bad_time, 4 + clock::SAMPLED_AT + 8, 1_000_000_000);
    assert_eq!(
        decode_clock_reply(&bad_time),
        Err(Errno::TimestampOutOfRange)
    );
}

#[test]
fn a_stream_report_round_trips_for_every_state() {
    for state in [
        StreamState::Idle,
        StreamState::Running,
        StreamState::Paused,
        StreamState::Draining,
        StreamState::SeatInactive,
        StreamState::DeviceLost,
    ] {
        let report = StreamReport {
            state,
            changed_at: Frames::new(48_000),
            xruns: 3,
            xrun_frames: 512,
        };
        assert_eq!(
            decode_state_reply(&encode_state_reply(Ok(report))),
            Ok(report)
        );
    }
    assert_eq!(
        decode_state_reply(&encode_state_reply(Err(Errno::NotFound))),
        Err(Errno::NotFound)
    );

    let wire = encode_state_reply(Ok(StreamReport {
        state: StreamState::Running,
        changed_at: Frames::ZERO,
        xruns: 0,
        xrun_frames: 0,
    }));
    assert_eq!(
        decode_state_reply(&wire[..AUDIO_STATE_REPLY_LEN - 1]),
        Err(Errno::BufferTooSmall)
    );

    let mut dirty = wire;
    dirty[4 + state::RESERVED] = 1;
    assert_eq!(decode_state_reply(&dirty), Err(Errno::BadMagic));

    let mut undefined = wire;
    undefined[4 + state::STATE] = 9;
    assert_eq!(decode_state_reply(&undefined), Err(Errno::OutOfRange));
}

#[test]
fn every_notification_round_trips() {
    for notification in [
        AudioNotify::SpaceAvailable {
            stream_id: 42,
            position: Frames::new(96_000),
        },
        AudioNotify::StateChanged {
            stream_id: 42,
            state: StreamState::SeatInactive,
            at: Frames::new(96_000),
        },
        AudioNotify::Xrun {
            stream_id: 42,
            at: Frames::new(96_000),
            lost_frames: 480,
        },
    ] {
        assert_eq!(
            AudioNotify::decode(&notification.encode()),
            Ok(notification),
            "{notification:?} must survive the wire"
        );
    }
}

#[test]
fn a_notification_fails_closed() {
    let wire = AudioNotify::SpaceAvailable {
        stream_id: 42,
        position: Frames::new(96_000),
    }
    .encode();
    assert_eq!(
        AudioNotify::decode(&wire[..AUDIO_NOTIFY_LEN - 1]),
        Err(Errno::BufferTooSmall)
    );

    let mut wrong_magic = wire;
    wrong_magic[0] ^= 0xFF;
    assert_eq!(AudioNotify::decode(&wrong_magic), Err(Errno::BadMagic));

    let mut wrong_version = wire;
    put_u16(&mut wrong_version, 4, 0);
    assert_eq!(
        AudioNotify::decode(&wrong_version),
        Err(Errno::AbiVersionUnsupported)
    );

    let mut unknown_kind = wire;
    unknown_kind[notify::KIND] = 9;
    assert_eq!(AudioNotify::decode(&unknown_kind), Err(Errno::OutOfRange));

    let mut zero_stream = wire;
    put_u64(&mut zero_stream, notify::STREAM, 0);
    assert_eq!(AudioNotify::decode(&zero_stream), Err(Errno::OutOfRange));

    // A field the kind does not define would be a value the decoder never
    // looked at.
    for dirty in [notify::STATE, notify::LOST_FRAMES] {
        let mut corrupt = wire;
        corrupt[dirty] = 1;
        assert_eq!(AudioNotify::decode(&corrupt), Err(Errno::BadMagic));
    }

    let mut xrun = AudioNotify::Xrun {
        stream_id: 42,
        at: Frames::new(96_000),
        lost_frames: 480,
    }
    .encode();
    xrun[notify::STATE] = 1;
    assert_eq!(AudioNotify::decode(&xrun), Err(Errno::BadMagic));

    let mut changed = AudioNotify::StateChanged {
        stream_id: 42,
        state: StreamState::Paused,
        at: Frames::new(96_000),
    }
    .encode();
    changed[notify::LOST_FRAMES] = 1;
    assert_eq!(AudioNotify::decode(&changed), Err(Errno::BadMagic));
}

/// Every reply buffer in the contract is sized to one constant, and the widest
/// reply is the one this names.
#[test]
fn the_reply_bound_covers_every_reply_shape() {
    assert_eq!(
        AUDIO_MAX_REPLY, AUDIO_ENUMERATE_REPLY_LEN,
        "the device descriptor is the widest reply"
    );
}
