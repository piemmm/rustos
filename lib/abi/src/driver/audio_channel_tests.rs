//! Unit tests for the `audiochan-v1` control plane: every request and reply
//! round-trips, and every malformed frame is refused rather than guessed at.

use super::*;
use crate::driver::audio::{
    AudioName, GainRange, RateSet, RateSupport, SampleFormats, StreamDirection,
};
use crate::driver::audio_ring::PCM_RING_HEADER_LEN;

fn configure_params() -> ConfigureParams {
    ConfigureParams {
        endpoint: 1,
        rate: Rate::HZ_48000,
        format: SampleFormat::S24In32,
        channel_map: ChannelMap::STEREO,
        period_frames: 240,
    }
}

fn attach_params() -> AttachParams {
    AttachParams {
        endpoint: 1,
        ring_frames: 2_048,
        region_grant: 0x1234_5678_9ABC_DEF0,
        notify_endpoint: notify_endpoint_for(4_096, 3),
    }
}

fn grant() -> ConfigureGrant {
    ConfigureGrant {
        rate: Rate::new(44_100).expect("in range"),
        format: SampleFormat::S16,
        channel_map: ChannelMap::STEREO,
        period_frames: 256,
        max_ring_frames: 8_192,
    }
}

fn every_request() -> [AudioChannelRequest; 10] {
    [
        AudioChannelRequest::Facts,
        AudioChannelRequest::EndpointFacts { endpoint: 2 },
        AudioChannelRequest::Configure(configure_params()),
        AudioChannelRequest::Attach(attach_params()),
        AudioChannelRequest::Start {
            endpoint: 1,
            at: Frames::new(u64::MAX / 3),
        },
        AudioChannelRequest::Stop {
            endpoint: 1,
            at: Frames::new(9_600),
        },
        AudioChannelRequest::Drain { endpoint: 0 },
        AudioChannelRequest::Service { endpoint: 1 },
        AudioChannelRequest::Gain {
            endpoint: 1,
            millibel: -1_250,
            mute: true,
        },
        AudioChannelRequest::Detach { endpoint: 1 },
    ]
}

fn encoded(request: AudioChannelRequest) -> ([u8; AUDIO_CHANNEL_MAX_REQUEST], usize) {
    let mut out = [0u8; AUDIO_CHANNEL_MAX_REQUEST];
    let len = request.encode(&mut out).expect("buffer is the maximum");
    (out, len)
}

#[test]
fn the_reserved_endpoint_block_is_the_spelled_name_and_bounded() {
    assert!(is_audio_channel_endpoint(AUDIO_CHANNEL_ENDPOINT_BASE));
    assert!(is_audio_channel_endpoint(
        AUDIO_CHANNEL_ENDPOINT_BASE + AUDIO_CHANNEL_ENDPOINT_COUNT - 1
    ));
    assert!(!is_audio_channel_endpoint(AUDIO_CHANNEL_ENDPOINT_BASE - 1));
    assert!(!is_audio_channel_endpoint(
        AUDIO_CHANNEL_ENDPOINT_BASE + AUDIO_CHANNEL_ENDPOINT_COUNT
    ));
    // The block is a rendezvous only a privileged binder may claim.
    assert!(crate::ipc::is_reserved_endpoint(
        AUDIO_CHANNEL_ENDPOINT_BASE
    ));
    assert!(crate::ipc::is_reserved_endpoint(
        AUDIO_CHANNEL_ENDPOINT_BASE + AUDIO_CHANNEL_ENDPOINT_COUNT - 1
    ));
}

/// The three fields tile the word exactly, so no pid or slot can reach
/// another's bits or fold two notify ports onto one id — and the result is an
/// ordinary id the mixer binds without the privileged-bind capability.
#[test]
fn a_notify_endpoint_packs_pid_and_slot_without_reaching_the_tag() {
    for pid in [0u64, 1, 4_096, crate::PID_MAX] {
        for index in [0u64, 1, AUDIO_CHANNEL_ENDPOINT_COUNT - 1] {
            let endpoint = notify_endpoint_for(pid, index);
            assert_eq!(endpoint & 0xFF, index);
            assert_eq!((endpoint >> 8) & crate::PID_MAX, pid);
            assert_eq!(endpoint >> 48, AUDIO_NOTIFY_ENDPOINT_TAG >> 48);
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
            AudioChannelRequest::decode(&frame[..len]),
            Ok(request),
            "{request:?} must survive the wire"
        );
    }
}

#[test]
fn a_request_frame_is_exactly_as_long_as_its_operation_needs() {
    for request in every_request() {
        let (_, len) = encoded(request);
        assert!((HEADER_LEN..=AUDIO_CHANNEL_MAX_REQUEST).contains(&len));
        let mut short = [0u8; AUDIO_CHANNEL_MAX_REQUEST];
        assert_eq!(
            request.encode(&mut short[..len - 1]),
            Err(Errno::BufferTooSmall)
        );
    }
    assert_eq!(encoded(AudioChannelRequest::Facts).1, HEADER_LEN);
}

/// A body shorter than its operation defines would leave the decoder reading
/// fields the sender never wrote.
#[test]
fn a_truncated_request_is_refused_rather_than_read_short() {
    for request in every_request() {
        let (frame, len) = encoded(request);
        for cut in 0..len {
            assert_eq!(
                AudioChannelRequest::decode(&frame[..cut]),
                Err(Errno::BufferTooSmall),
                "{request:?} truncated to {cut} bytes must be refused"
            );
        }
    }
}

#[test]
fn a_request_header_fails_closed() {
    let (mut frame, len) = encoded(AudioChannelRequest::Service { endpoint: 1 });

    let mut wrong_magic = frame;
    wrong_magic[0] ^= 0xFF;
    assert_eq!(
        AudioChannelRequest::decode(&wrong_magic[..len]),
        Err(Errno::BadMagic)
    );

    let mut wrong_version = frame;
    put_u16(&mut wrong_version, 4, AUDIO_CHANNEL_VERSION_V1 + 1);
    assert_eq!(
        AudioChannelRequest::decode(&wrong_version[..len]),
        Err(Errno::AbiVersionUnsupported)
    );

    frame[7] = 1;
    assert_eq!(
        AudioChannelRequest::decode(&frame[..len]),
        Err(Errno::BadMagic)
    );

    let (mut unknown_op, len) = encoded(AudioChannelRequest::Facts);
    unknown_op[6] = 200;
    assert_eq!(
        AudioChannelRequest::decode(&unknown_op[..len]),
        Err(Errno::OutOfRange)
    );
}

/// An endpoint index no device could present is a corrupt frame, refused
/// before the driver indexes anything with it.
#[test]
fn an_endpoint_index_past_the_device_bound_is_refused_on_every_operation() {
    for request in every_request() {
        let (mut frame, len) = encoded(request);
        if matches!(request, AudioChannelRequest::Facts) {
            continue;
        }
        put_u16(&mut frame, HEADER_LEN, MAX_DEVICE_ENDPOINTS);
        assert_eq!(
            AudioChannelRequest::decode(&frame[..len]),
            Err(Errno::OutOfRange),
            "{request:?} must refuse an out-of-range endpoint"
        );
    }
}

#[test]
fn an_endpoint_scoped_request_refuses_a_dirty_reserved_pair() {
    for request in [
        AudioChannelRequest::EndpointFacts { endpoint: 2 },
        AudioChannelRequest::Drain { endpoint: 0 },
        AudioChannelRequest::Service { endpoint: 1 },
        AudioChannelRequest::Detach { endpoint: 1 },
    ] {
        let (mut frame, len) = encoded(request);
        frame[HEADER_LEN + 2] = 1;
        assert_eq!(
            AudioChannelRequest::decode(&frame[..len]),
            Err(Errno::BadMagic)
        );
    }
}

#[test]
fn configure_fails_closed_on_every_malformed_field() {
    let (frame, len) = encoded(AudioChannelRequest::Configure(configure_params()));
    let body = HEADER_LEN;

    for dirty in [
        body + configure::RESERVED0,
        body + configure::RESERVED1,
        body + configure::RESERVED1 + 2,
    ] {
        let mut corrupt = frame;
        corrupt[dirty] = 1;
        assert_eq!(
            AudioChannelRequest::decode(&corrupt[..len]),
            Err(Errno::BadMagic)
        );
    }

    let mut undefined_format = frame;
    undefined_format[body + configure::FORMAT] = 99;
    assert_eq!(
        AudioChannelRequest::decode(&undefined_format[..len]),
        Err(Errno::OutOfRange)
    );

    let mut impossible_rate = frame;
    put_u32(&mut impossible_rate, body + configure::RATE, 1);
    assert_eq!(
        AudioChannelRequest::decode(&impossible_rate[..len]),
        Err(Errno::OutOfRange)
    );

    for period in [0, ring_bounds::MAX_FRAMES + 1] {
        let mut bad_period = frame;
        put_u32(&mut bad_period, body + configure::PERIOD, period);
        assert_eq!(
            AudioChannelRequest::decode(&bad_period[..len]),
            Err(Errno::OutOfRange)
        );
    }

    let mut no_channels = frame;
    no_channels[body + configure::CHANNEL_MAP] = 0;
    assert_eq!(
        AudioChannelRequest::decode(&no_channels[..len]),
        Err(Errno::LengthOutOfRange)
    );
}

/// The ring depth is what the sample region is sized from, so a depth the
/// index arithmetic could not serve is refused at the channel rather than
/// found out at bind time.
#[test]
fn attach_refuses_a_ring_depth_the_region_could_not_be_indexed_by() {
    let (frame, len) = encoded(AudioChannelRequest::Attach(attach_params()));
    let body = HEADER_LEN;

    let mut dirty_reserved = frame;
    put_u16(&mut dirty_reserved, body + attach::RESERVED, 1);
    assert_eq!(
        AudioChannelRequest::decode(&dirty_reserved[..len]),
        Err(Errno::BadMagic)
    );

    for frames in [
        0,
        1,
        3,
        1_000,
        ring_bounds::MAX_FRAMES * 2,
        ring_bounds::MAX_FRAMES + 1,
    ] {
        let mut bad = frame;
        put_u32(&mut bad, body + attach::RING_FRAMES, frames);
        assert_eq!(
            AudioChannelRequest::decode(&bad[..len]),
            Err(Errno::OutOfRange),
            "a {frames}-frame ring must be refused"
        );
    }
}

/// A notify port naming a reserved rendezvous would turn the driver into a
/// proxy for wakes at a system service, so it is refused at the wire rather
/// than sent to.
#[test]
fn attach_refuses_a_notify_port_that_names_a_system_rendezvous() {
    let (frame, len) = encoded(AudioChannelRequest::Attach(attach_params()));
    for hijacked in [
        crate::sysinfo::SYSINFO_ENDPOINT,
        crate::audio::AUDIO_ENDPOINT,
        AUDIO_CHANNEL_ENDPOINT_BASE,
    ] {
        let mut corrupt = frame;
        put_u64(&mut corrupt, HEADER_LEN + attach::NOTIFY, hijacked);
        assert_eq!(
            AudioChannelRequest::decode(&corrupt[..len]),
            Err(Errno::OutOfRange)
        );
    }
    // The mixer's own derivation never produces one.
    assert!(!crate::ipc::is_reserved_endpoint(notify_endpoint_for(
        crate::PID_MAX,
        AUDIO_CHANNEL_ENDPOINT_COUNT - 1
    )));
}

#[test]
fn transport_and_gain_refuse_their_dirty_reserved_fields() {
    let (frame, len) = encoded(AudioChannelRequest::Start {
        endpoint: 1,
        at: Frames::new(9_600),
    });
    for offset in transport::RESERVED..transport::AT {
        let mut corrupt = frame;
        corrupt[HEADER_LEN + offset] = 1;
        assert_eq!(
            AudioChannelRequest::decode(&corrupt[..len]),
            Err(Errno::BadMagic)
        );
    }

    let (frame, len) = encoded(AudioChannelRequest::Gain {
        endpoint: 1,
        millibel: -1_250,
        mute: true,
    });
    let mut dirty = frame;
    dirty[HEADER_LEN + gain::RESERVED] = 1;
    assert_eq!(
        AudioChannelRequest::decode(&dirty[..len]),
        Err(Errno::BadMagic)
    );

    // A flag byte that is neither set nor clear is refused, not read as
    // "unmuted".
    let mut undefined_mute = frame;
    undefined_mute[HEADER_LEN + gain::MUTE] = 2;
    assert_eq!(
        AudioChannelRequest::decode(&undefined_mute[..len]),
        Err(Errno::OutOfRange)
    );
}

#[test]
fn the_facts_replies_round_trip_and_carry_a_refusal_verbatim() {
    let device = AudioDeviceFacts {
        endpoints: 2,
        name: AudioName::new("virtio-snd").expect("plain text"),
    };
    assert_eq!(
        decode_facts_reply(&encode_facts_reply(Ok(device))),
        Ok(device)
    );
    assert_eq!(
        decode_facts_reply(&encode_facts_reply(Err(Errno::DeviceFault))),
        Err(Errno::DeviceFault)
    );
    assert_eq!(
        decode_facts_reply(&encode_facts_reply(Ok(device))[..1]),
        Err(Errno::BufferTooSmall)
    );

    let endpoint = AudioEndpointFacts {
        index: 1,
        direction: StreamDirection::Capture,
        jack: JackState::Absent,
        formats: SampleFormats::EMPTY.with(SampleFormat::S16),
        channel_map: ChannelMap::MONO,
        rates: RateSupport::Discrete(RateSet::new(&[Rate::HZ_48000]).expect("one rate")),
        min_period_frames: 48,
        max_period_frames: 480,
        max_ring_frames: 4_096,
        gain: Some(GainRange::new(-3_000, 0, 100).expect("ordered")),
        name: AudioName::new("Internal Microphone").expect("plain text"),
    };
    assert_eq!(
        decode_endpoint_reply(&encode_endpoint_reply(Ok(endpoint))),
        Ok(endpoint)
    );
    assert_eq!(
        decode_endpoint_reply(&encode_endpoint_reply(Err(Errno::NotFound))),
        Err(Errno::NotFound)
    );
}

#[test]
fn a_configure_grant_round_trips_and_shapes_the_ring() {
    let granted = grant();
    assert_eq!(
        decode_configure_reply(&encode_configure_reply(Ok(granted))),
        Ok(granted)
    );
    assert_eq!(
        decode_configure_reply(&encode_configure_reply(Err(Errno::NotSupported))),
        Err(Errno::NotSupported)
    );

    // The one place both sides derive the region's shape from.
    let geometry = granted.geometry(1_024).expect("within the grant");
    assert_eq!(geometry.frames(), 1_024);
    assert_eq!(geometry.format(), SampleFormat::S16);
    assert_eq!(geometry.channels(), 2);
    assert_eq!(geometry.region_len(), PCM_RING_HEADER_LEN + 1_024 * 4);

    // A ring deeper than the grant, or too shallow to hold one period, is not
    // a ring this configuration can be served through.
    assert_eq!(granted.geometry(16_384), Err(Errno::OutOfRange));
    assert_eq!(granted.geometry(128), Err(Errno::OutOfRange));
    assert_eq!(granted.geometry(1_000), Err(Errno::OutOfRange));
}

#[test]
fn a_configure_reply_decode_fails_closed() {
    let wire = encode_configure_reply(Ok(grant()));
    assert_eq!(
        decode_configure_reply(&wire[..AUDIO_CHANNEL_CONFIGURE_REPLY_LEN - 1]),
        Err(Errno::BufferTooSmall)
    );

    for dirty in [grant::RESERVED0, grant::RESERVED1] {
        let mut corrupt = wire;
        corrupt[4 + dirty] = 1;
        assert_eq!(decode_configure_reply(&corrupt), Err(Errno::BadMagic));
    }

    let mut zero_period = wire;
    put_u32(&mut zero_period, 4 + grant::PERIOD, 0);
    assert_eq!(decode_configure_reply(&zero_period), Err(Errno::OutOfRange));

    // A ring bound that cannot hold one period would grant a stream that can
    // never be serviced.
    let mut shallow = wire;
    put_u32(&mut shallow, 4 + grant::MAX_RING, 16);
    assert_eq!(decode_configure_reply(&shallow), Err(Errno::OutOfRange));

    let mut unbounded = wire;
    put_u32(
        &mut unbounded,
        4 + grant::MAX_RING,
        ring_bounds::MAX_FRAMES + 1,
    );
    assert_eq!(decode_configure_reply(&unbounded), Err(Errno::OutOfRange));
}

#[test]
fn a_service_report_round_trips_and_fails_closed() {
    let report = AudioServiceReport {
        transferred: 240,
        running: true,
        position: Frames::new(9_600_000),
        xrun_frames: 17,
        sampled_at: Time64::new(1_700_000_000, 123_456_789).expect("canonical"),
    };
    assert_eq!(
        decode_service_reply(&encode_service_reply(Ok(report))),
        Ok(report)
    );
    assert_eq!(
        decode_service_reply(&encode_service_reply(Err(Errno::DeviceOffline))),
        Err(Errno::DeviceOffline)
    );

    let wire = encode_service_reply(Ok(report));
    assert_eq!(
        decode_service_reply(&wire[..AUDIO_CHANNEL_SERVICE_REPLY_LEN - 1]),
        Err(Errno::BufferTooSmall)
    );

    let mut dirty = wire;
    dirty[4 + service::RESERVED] = 1;
    assert_eq!(decode_service_reply(&dirty), Err(Errno::BadMagic));

    let mut undefined_running = wire;
    undefined_running[4 + service::RUNNING] = 7;
    assert_eq!(
        decode_service_reply(&undefined_running),
        Err(Errno::OutOfRange)
    );

    // A non-canonical nanosecond field would make every clock fit wrong.
    let mut bad_time = wire;
    put_u32(&mut bad_time, 4 + service::SAMPLED_AT + 8, 1_000_000_000);
    assert_eq!(
        decode_service_reply(&bad_time),
        Err(Errno::TimestampOutOfRange)
    );
}

#[test]
fn every_notification_round_trips() {
    for notification in [
        AudioChannelNotify::PeriodElapsed {
            endpoint: 1,
            position: Frames::new(480_000),
            sampled_at: Time64::new(1_700_000_000, 7).expect("canonical"),
        },
        AudioChannelNotify::Xrun {
            endpoint: 0,
            position: Frames::new(48_000),
            lost_frames: 96,
        },
        AudioChannelNotify::JackChanged {
            endpoint: 2,
            jack: JackState::Present,
        },
    ] {
        assert_eq!(
            AudioChannelNotify::decode(&notification.encode()),
            Ok(notification),
            "{notification:?} must survive the wire"
        );
    }
}

#[test]
fn a_notification_header_fails_closed() {
    let wire = AudioChannelNotify::JackChanged {
        endpoint: 2,
        jack: JackState::Present,
    }
    .encode();
    assert_eq!(
        AudioChannelNotify::decode(&wire[..AUDIO_CHANNEL_NOTIFY_LEN - 1]),
        Err(Errno::BufferTooSmall)
    );

    let mut wrong_magic = wire;
    wrong_magic[0] ^= 0xFF;
    assert_eq!(
        AudioChannelNotify::decode(&wrong_magic),
        Err(Errno::BadMagic)
    );

    let mut wrong_version = wire;
    put_u16(&mut wrong_version, 4, 0);
    assert_eq!(
        AudioChannelNotify::decode(&wrong_version),
        Err(Errno::AbiVersionUnsupported)
    );

    let mut unknown_kind = wire;
    unknown_kind[notify::KIND] = 9;
    assert_eq!(
        AudioChannelNotify::decode(&unknown_kind),
        Err(Errno::OutOfRange)
    );

    for dirty in [notify::RESERVED0, notify::RESERVED1] {
        let mut corrupt = wire;
        corrupt[dirty] = 1;
        assert_eq!(AudioChannelNotify::decode(&corrupt), Err(Errno::BadMagic));
    }

    let mut far_endpoint = wire;
    put_u16(&mut far_endpoint, notify::ENDPOINT, MAX_DEVICE_ENDPOINTS);
    assert_eq!(
        AudioChannelNotify::decode(&far_endpoint),
        Err(Errno::OutOfRange)
    );

    let mut undefined_jack = wire;
    undefined_jack[notify::JACK] = 9;
    assert_eq!(
        AudioChannelNotify::decode(&undefined_jack),
        Err(Errno::OutOfRange)
    );
}

/// A field a notification's kind does not define must be zero: a populated one
/// would be a value the decoder never looked at, which is how a second meaning
/// gets smuggled into a fixed frame.
#[test]
fn a_notification_refuses_a_field_its_kind_does_not_define() {
    let period = AudioChannelNotify::PeriodElapsed {
        endpoint: 1,
        position: Frames::new(480_000),
        sampled_at: Time64::new(1_700_000_000, 7).expect("canonical"),
    }
    .encode();
    for dirty in [notify::JACK, notify::LOST_FRAMES] {
        let mut corrupt = period;
        corrupt[dirty] = 1;
        assert_eq!(AudioChannelNotify::decode(&corrupt), Err(Errno::BadMagic));
    }

    let xrun = AudioChannelNotify::Xrun {
        endpoint: 0,
        position: Frames::new(48_000),
        lost_frames: 96,
    }
    .encode();
    for dirty in [notify::JACK, notify::SAMPLED_AT] {
        let mut corrupt = xrun;
        corrupt[dirty] = 1;
        assert_eq!(AudioChannelNotify::decode(&corrupt), Err(Errno::BadMagic));
    }

    let jack = AudioChannelNotify::JackChanged {
        endpoint: 2,
        jack: JackState::Present,
    }
    .encode();
    for dirty in [notify::POSITION, notify::LOST_FRAMES, notify::SAMPLED_AT] {
        let mut corrupt = jack;
        corrupt[dirty] = 1;
        assert_eq!(AudioChannelNotify::decode(&corrupt), Err(Errno::BadMagic));
    }
}

/// Every reply buffer in the contract is sized to one constant, and the
/// widest reply is the one this names — a payload that outgrew it would leave
/// every buffer short.
#[test]
fn the_reply_bound_covers_every_reply_shape() {
    assert_eq!(
        AUDIO_CHANNEL_MAX_REPLY, AUDIO_CHANNEL_ENDPOINT_REPLY_LEN,
        "the endpoint facts are the widest reply"
    );
}
