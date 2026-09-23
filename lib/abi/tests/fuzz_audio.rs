//! Deterministic fuzz harness for the audio wire surface: the `audio-v1`
//! client protocol, the `audiochan-v1` device channel, and the shared PCM
//! ring's peer-written positions.
//!
//! Every one of these crosses a trust boundary. A client request arrives from
//! an ordinary program; a device-channel frame arrives from a driver process
//! that owns hardware but is not the mixer's own code; a reply arrives at a
//! program from a service it did not write; and the ring's two positions live
//! in memory the *peer* can scribble on at any moment. A malformed rate, a
//! channel map with a repeated position, a ring depth the index arithmetic
//! could not serve, a notification carrying a field its kind does not define,
//! or a consumer position ahead of the producer must all be **rejected**
//! fail-closed, never trusted and never a panic. The invariants driven here:
//!
//! * feeding any byte image to any audio decoder never panics and never reads
//!   out of bounds — each returns a validated value or an `Errno`;
//! * anything a decoder *accepts* re-encodes and re-decodes identically, so
//!   nothing is silently normalised on the way in;
//! * an accepted ring depth is a power of two inside the containment bounds,
//!   and an accepted stream grant never names a reserved rendezvous — the two
//!   places a hostile frame could reserve unbounded pinned memory or hijack a
//!   system service's mailbox;
//! * every `PcmRing` operation over arbitrary positions either refuses or
//!   answers a frame count the ring could actually hold.
//!
//! TAIRiX pulls in no external fuzz runner: a per-run-seeded `Prng` mutates valid
//! seed frames and feeds pure noise. A plain `cargo test` runs the fixed smoke
//! sweep; `cargo xtask fuzz` extends the loop to a wall-clock budget.

use tairix_abi::audio::{
    decode_clock_reply, decode_enumerate_reply, decode_open_reply, decode_state_reply,
    encode_clock_reply, encode_enumerate_reply, encode_open_reply, encode_state_reply,
    notify_endpoint_for, AudioDeviceDescriptor, AudioNotify, AudioRequest, ClockReport, OpenParams,
    StreamGrant, StreamReport, StreamRole, StreamState, AUDIO_CLOCK_REPLY_LEN,
    AUDIO_ENUMERATE_REPLY_LEN, AUDIO_MAX_REQUEST, AUDIO_NOTIFY_LEN, AUDIO_OPEN_REPLY_LEN,
    AUDIO_STATE_REPLY_LEN,
};
use tairix_abi::driver::audio::{
    ring_bounds, AudioDeviceFacts, AudioEndpointFacts, AudioName, ChannelMap, Frames, GainRange,
    JackState, Rate, RateSet, RateSupport, SampleFormat, SampleFormats, StreamDirection,
};
use tairix_abi::driver::audio_channel::{
    decode_configure_reply, decode_endpoint_reply, decode_facts_reply, decode_service_reply,
    encode_configure_reply, encode_endpoint_reply, encode_facts_reply, encode_service_reply,
    AttachParams, AudioChannelNotify, AudioChannelRequest, AudioServiceReport, ConfigureGrant,
    ConfigureParams, AUDIO_CHANNEL_CONFIGURE_REPLY_LEN, AUDIO_CHANNEL_ENDPOINT_REPLY_LEN,
    AUDIO_CHANNEL_FACTS_REPLY_LEN, AUDIO_CHANNEL_MAX_REQUEST, AUDIO_CHANNEL_NOTIFY_LEN,
    AUDIO_CHANNEL_SERVICE_REPLY_LEN,
};
use tairix_abi::driver::audio_ring::{
    aligned_region, PcmGeometry, PcmRing, PCM_RING_HEADER_LEN, REGION_ALIGN_PADDING,
};
use tairix_abi::time::{Duration64, Time64};
use tairix_fuzzseed::Prng;

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 8_000;

/// Flip up to `most` bytes of `frame` using `rng`.
fn scramble(frame: &mut [u8], most: usize, rng: &mut Prng) {
    if frame.is_empty() {
        return;
    }
    let flips = rng.at_most(most);
    for _ in 0..flips {
        let pos = rng.below(frame.len());
        frame[pos] ^= rng.next_u8();
    }
}

fn rates() -> RateSupport {
    RateSupport::Discrete(
        RateSet::new(&[Rate::new(44_100).expect("in range"), Rate::HZ_48000]).expect("ascending"),
    )
}

fn endpoint_facts() -> AudioEndpointFacts {
    AudioEndpointFacts {
        index: 1,
        direction: StreamDirection::Playback,
        jack: JackState::Present,
        formats: SampleFormats::EMPTY
            .with(SampleFormat::S16)
            .with(SampleFormat::F32),
        channel_map: ChannelMap::STEREO,
        rates: rates(),
        min_period_frames: 48,
        max_period_frames: 4_800,
        max_ring_frames: 16_384,
        gain: Some(GainRange::new(-6_400, 0, 50).expect("ordered")),
        name: AudioName::new("Green Line Out").expect("plain text"),
    }
}

fn descriptor() -> AudioDeviceDescriptor {
    AudioDeviceDescriptor {
        device_id: 7,
        direction: StreamDirection::Playback,
        jack: JackState::Present,
        is_default: true,
        formats: SampleFormats::EMPTY.with(SampleFormat::F32),
        channel_map: ChannelMap::STEREO,
        rates: rates(),
        gain: None,
        name: AudioName::new("Headphones").expect("plain text"),
    }
}

/// Encode a device-channel request into a frame of exactly its own length.
fn channel_frame(request: &AudioChannelRequest) -> Vec<u8> {
    let mut out = vec![0u8; AUDIO_CHANNEL_MAX_REQUEST];
    let len = request.encode(&mut out).expect("the seed frame fits");
    out.truncate(len);
    out
}

/// Encode a client request into a frame of exactly its own length.
fn client_frame(request: &AudioRequest) -> Vec<u8> {
    let mut out = vec![0u8; AUDIO_MAX_REQUEST];
    let len = request.encode(&mut out).expect("the seed frame fits");
    out.truncate(len);
    out
}

/// Decode `bytes` as a device-channel request; anything accepted must
/// round-trip, and an accepted attach must name a ring the index arithmetic
/// can serve inside the pinned-memory bounds.
fn exercise_channel_request(bytes: &[u8]) {
    let Ok(request) = AudioChannelRequest::decode(bytes) else {
        return;
    };
    if let AudioChannelRequest::Attach(params) = request {
        assert!(
            params.ring_frames.is_power_of_two()
                && (ring_bounds::MIN_FRAMES..=ring_bounds::MAX_FRAMES)
                    .contains(&params.ring_frames),
            "an accepted ring depth must be one the region can be indexed by \
             and inside the pinned-memory bound ({})",
            params.ring_frames
        );
    }
    let round =
        AudioChannelRequest::decode(&channel_frame(&request)).expect("re-decode of accepted frame");
    assert_eq!(
        round, request,
        "a device-channel request is not round-trip stable"
    );
}

/// Decode `bytes` as a client request; anything accepted must round-trip.
fn exercise_client_request(bytes: &[u8]) {
    let Ok(request) = AudioRequest::decode(bytes) else {
        return;
    };
    if let AudioRequest::Open(params) = request {
        assert!(
            params.latency_target_frames > 0
                && params.latency_target_frames <= ring_bounds::MAX_FRAMES,
            "an accepted latency target must be one a ring could hold ({})",
            params.latency_target_frames
        );
    }
    let round = AudioRequest::decode(&client_frame(&request)).expect("re-decode of accepted frame");
    assert_eq!(round, request, "a client request is not round-trip stable");
}

/// Decode `bytes` as each notification kind; anything accepted must
/// round-trip.
fn exercise_notifications(bytes: &[u8]) {
    if let Ok(notification) = AudioChannelNotify::decode(bytes) {
        assert_eq!(
            AudioChannelNotify::decode(&notification.encode()),
            Ok(notification),
            "a device-channel notification is not round-trip stable"
        );
    }
    if let Ok(notification) = AudioNotify::decode(bytes) {
        assert_eq!(
            AudioNotify::decode(&notification.encode()),
            Ok(notification),
            "a client notification is not round-trip stable"
        );
    }
}

/// Decode `bytes` as every reply shape; anything accepted must round-trip, and
/// an accepted stream grant must never name a reserved rendezvous or a ring
/// too small for the latency it promises.
fn exercise_replies(bytes: &[u8]) {
    if let Ok(facts) = decode_facts_reply(bytes) {
        assert_eq!(
            decode_facts_reply(&encode_facts_reply(Ok(facts))),
            Ok(facts)
        );
    }
    if let Ok(facts) = decode_endpoint_reply(bytes) {
        assert_eq!(facts.validate(), Ok(()));
        assert_eq!(
            decode_endpoint_reply(&encode_endpoint_reply(Ok(facts))),
            Ok(facts)
        );
    }
    if let Ok(granted) = decode_configure_reply(bytes) {
        assert_eq!(
            decode_configure_reply(&encode_configure_reply(Ok(granted))),
            Ok(granted)
        );
        // The one derivation both sides size the region from must agree with
        // the bound the grant itself carries.
        assert!(granted.geometry(granted.max_ring_frames + 1).is_err());
    }
    if let Ok(report) = decode_service_reply(bytes) {
        assert_eq!(
            decode_service_reply(&encode_service_reply(Ok(report))),
            Ok(report)
        );
    }
    if let Ok(device) = decode_enumerate_reply(bytes) {
        assert!(!device.formats.is_empty());
        assert_eq!(
            decode_enumerate_reply(&encode_enumerate_reply(Ok(device))),
            Ok(device)
        );
    }
    if let Ok(granted) = decode_open_reply(bytes) {
        assert!(
            !tairix_abi::ipc::is_reserved_endpoint(granted.notify_endpoint),
            "an accepted grant must never have a client bind a system rendezvous"
        );
        assert!(
            granted.granted_latency_frames <= granted.ring_frames,
            "an accepted grant must never promise more latency than its ring holds"
        );
        assert_eq!(
            decode_open_reply(&encode_open_reply(Ok(granted))),
            Ok(granted)
        );
    }
    if let Ok(report) = decode_clock_reply(bytes) {
        assert_eq!(
            decode_clock_reply(&encode_clock_reply(Ok(report))),
            Ok(report)
        );
    }
    if let Ok(report) = decode_state_reply(bytes) {
        assert_eq!(
            decode_state_reply(&encode_state_reply(Ok(report))),
            Ok(report)
        );
    }
}

/// Drive every ring operation over a region whose two peer-written positions
/// are arbitrary bytes. Nothing may panic, and anything the ring *answers*
/// must be a frame count it could actually have held.
fn exercise_ring(geometry: PcmGeometry, producer: u64, consumer: u64, rng: &mut Prng) {
    let mut buffer = vec![0u8; geometry.region_len() + REGION_ALIGN_PADDING];
    let region = aligned_region(&mut buffer, geometry.region_len()).expect("aligned region");
    region[..8].copy_from_slice(&producer.to_le_bytes());
    let consumer_at = PCM_RING_HEADER_LEN / 2;
    region[consumer_at..consumer_at + 8].copy_from_slice(&consumer.to_le_bytes());
    let mut pcm = PcmRing::bind(region, geometry).expect("a correctly sized region binds");

    let capacity = geometry.frames();
    if let Ok(readable) = pcm.readable_frames() {
        assert!(readable <= capacity, "queued past the ring's own depth");
    }
    if let Ok(writable) = pcm.writable_frames() {
        assert!(writable <= capacity, "free past the ring's own depth");
    }

    let frames = rng.at_most(capacity as usize * 2);
    let mut samples = vec![0u8; frames * geometry.frame_bytes()];
    rng.fill(&mut samples);
    if let Ok(written) = pcm.write(&samples) {
        assert!(written <= capacity, "wrote more frames than the ring holds");
    }
    if let Ok(silenced) = pcm.write_silence(u32::try_from(frames).unwrap_or(u32::MAX)) {
        assert!(
            silenced <= capacity,
            "silenced more frames than the ring holds"
        );
    }
    let mut out = vec![0u8; frames * geometry.frame_bytes()];
    if let Ok(read) = pcm.read(&mut out) {
        assert!(read <= capacity, "read more frames than the ring holds");
    }
    if let Ok(dropped) = pcm.discard(u32::try_from(frames).unwrap_or(u32::MAX)) {
        assert!(
            dropped <= capacity,
            "discarded more frames than the ring holds"
        );
    }
    // A partial frame is a length no producer could mean, whatever the
    // positions say.
    if geometry.frame_bytes() > 1 {
        let partial = vec![0u8; geometry.frame_bytes() - 1];
        assert!(pcm.write(&partial).is_err());
    }
}

/// Well-formed device-channel requests, as mutation seeds.
fn channel_seeds() -> Vec<Vec<u8>> {
    vec![
        channel_frame(&AudioChannelRequest::Facts),
        channel_frame(&AudioChannelRequest::EndpointFacts { endpoint: 1 }),
        channel_frame(&AudioChannelRequest::Configure(ConfigureParams {
            endpoint: 1,
            rate: Rate::HZ_48000,
            format: SampleFormat::S24In32,
            channel_map: ChannelMap::STEREO,
            period_frames: 240,
        })),
        channel_frame(&AudioChannelRequest::Attach(AttachParams {
            endpoint: 1,
            ring_frames: 2_048,
            region_grant: 0x1234_5678,
            notify_endpoint: 0x4141_0000_0001_0002,
        })),
        channel_frame(&AudioChannelRequest::Start {
            endpoint: 1,
            at: Frames::new(48_000),
        }),
        channel_frame(&AudioChannelRequest::Gain {
            endpoint: 1,
            millibel: -1_250,
            mute: true,
        }),
        channel_frame(&AudioChannelRequest::Service { endpoint: 1 }),
    ]
}

/// Well-formed client requests, as mutation seeds.
fn client_seeds() -> Vec<Vec<u8>> {
    vec![
        client_frame(&AudioRequest::Enumerate {
            direction: StreamDirection::Playback,
            index: 0,
        }),
        client_frame(&AudioRequest::Open(OpenParams {
            device_id: 7,
            direction: StreamDirection::Playback,
            format: SampleFormat::F32,
            rate: Rate::HZ_48000,
            channel_map: ChannelMap::STEREO,
            role: StreamRole::Media,
            latency_target_frames: 960,
        })),
        client_frame(&AudioRequest::Attach {
            stream_id: 42,
            region_grant: 0xDEAD_BEEF,
        }),
        client_frame(&AudioRequest::Stop {
            stream_id: 42,
            at: Frames::new(96_000),
        }),
        client_frame(&AudioRequest::Mute {
            stream_id: 42,
            muted: false,
        }),
        client_frame(&AudioRequest::Close { stream_id: 42 }),
    ]
}

/// Well-formed notifications of both protocols, as mutation seeds.
fn notify_seeds() -> Vec<Vec<u8>> {
    vec![
        AudioChannelNotify::PeriodElapsed {
            endpoint: 1,
            position: Frames::new(480_000),
            sampled_at: Time64::new(1_700_000_000, 7).expect("canonical"),
        }
        .encode()
        .to_vec(),
        AudioChannelNotify::Xrun {
            endpoint: 1,
            position: Frames::new(48_000),
            lost_frames: 96,
        }
        .encode()
        .to_vec(),
        AudioNotify::SpaceAvailable {
            stream_id: 42,
            position: Frames::new(96_000),
        }
        .encode()
        .to_vec(),
        AudioNotify::StateChanged {
            stream_id: 42,
            state: StreamState::SeatInactive,
            at: Frames::new(96_000),
        }
        .encode()
        .to_vec(),
    ]
}

/// Well-formed replies of every shape, as mutation seeds.
fn reply_seeds() -> Vec<Vec<u8>> {
    vec![
        encode_facts_reply(Ok(AudioDeviceFacts {
            endpoints: 2,
            name: AudioName::new("virtio-snd").expect("plain text"),
        }))
        .to_vec(),
        encode_endpoint_reply(Ok(endpoint_facts())).to_vec(),
        encode_configure_reply(Ok(ConfigureGrant {
            rate: Rate::HZ_48000,
            format: SampleFormat::S16,
            channel_map: ChannelMap::STEREO,
            period_frames: 256,
            max_ring_frames: 8_192,
        }))
        .to_vec(),
        encode_service_reply(Ok(AudioServiceReport {
            transferred: 240,
            running: true,
            position: Frames::new(9_600_000),
            xrun_frames: 17,
            sampled_at: Time64::new(1_700_000_000, 123).expect("canonical"),
        }))
        .to_vec(),
        encode_enumerate_reply(Ok(descriptor())).to_vec(),
        encode_open_reply(Ok(StreamGrant {
            stream_id: 42,
            notify_endpoint: notify_endpoint_for(4_096, 2),
            rate: Rate::HZ_48000,
            format: SampleFormat::F32,
            channel_map: ChannelMap::STEREO,
            ring_frames: 4_096,
            granted_latency_frames: 960,
            granted_latency: Duration64::new(0, 20_000_000).expect("canonical"),
            clock_domain: 3,
        }))
        .to_vec(),
        encode_clock_reply(Ok(ClockReport {
            position: Frames::new(144_000_000),
            rate_millihertz: 47_998_600,
            sampled_at: Time64::new(1_700_000_000, 500).expect("canonical"),
        }))
        .to_vec(),
        encode_state_reply(Ok(StreamReport {
            state: StreamState::Running,
            changed_at: Frames::new(48_000),
            xruns: 3,
            xrun_frames: 512,
        }))
        .to_vec(),
    ]
}

/// Ring shapes across every frame width the vocabulary allows, so the wrap
/// arithmetic is driven at each of them.
fn geometries() -> Vec<PcmGeometry> {
    vec![
        PcmGeometry::new(2, SampleFormat::U8, 1).expect("valid"),
        PcmGeometry::new(8, SampleFormat::S24, 3).expect("valid"),
        PcmGeometry::new(16, SampleFormat::F32, 8).expect("valid"),
        PcmGeometry::new(32, SampleFormat::S16, 2).expect("valid"),
    ]
}

/// The widest reply frame any audio protocol answers with, so noise long
/// enough to reach every decoder's length check is generated.
fn widest_reply() -> usize {
    [
        AUDIO_CHANNEL_FACTS_REPLY_LEN,
        AUDIO_CHANNEL_ENDPOINT_REPLY_LEN,
        AUDIO_CHANNEL_CONFIGURE_REPLY_LEN,
        AUDIO_CHANNEL_SERVICE_REPLY_LEN,
        AUDIO_ENUMERATE_REPLY_LEN,
        AUDIO_OPEN_REPLY_LEN,
        AUDIO_CLOCK_REPLY_LEN,
        AUDIO_STATE_REPLY_LEN,
    ]
    .into_iter()
    .max()
    .expect("a non-empty list")
}

#[test]
fn decoding_any_audio_frame_never_panics() {
    let channel_seeds = channel_seeds();
    let client_seeds = client_seeds();
    let notify_seeds = notify_seeds();
    let reply_seeds = reply_seeds();
    let geometries = geometries();
    let widest_reply = widest_reply();
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);

    let mut rng = Prng::new(tairix_fuzzseed::start(
        "decoding_any_audio_frame_never_panics",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));

    let mut iteration: u64 = 0;
    loop {
        // 1. A valid device-channel request with a handful of bytes flipped.
        let seed = rng.pick(&channel_seeds);
        let mut mutated = seed.clone();
        scramble(&mut mutated, 12, &mut rng);
        exercise_channel_request(&mutated);

        // 2. A truncation, driving the exact-length checks, and an over-long
        //    frame, which must be read as its own prefix and nothing more.
        exercise_channel_request(&seed[..rng.at_most(seed.len())]);
        let mut longer = seed.clone();
        longer.push(rng.next_u8());
        exercise_channel_request(&longer);

        // 3. Pure noise as a device-channel request.
        let mut noise = vec![0u8; rng.at_most(AUDIO_CHANNEL_MAX_REQUEST + 8)];
        rng.fill(&mut noise);
        exercise_channel_request(&noise);

        // 4. The same three shapes for a client request.
        let seed = rng.pick(&client_seeds);
        let mut mutated = seed.clone();
        scramble(&mut mutated, 12, &mut rng);
        exercise_client_request(&mutated);
        exercise_client_request(&seed[..rng.at_most(seed.len())]);
        let mut noise = vec![0u8; rng.at_most(AUDIO_MAX_REQUEST + 8)];
        rng.fill(&mut noise);
        exercise_client_request(&noise);

        // 5. Notifications: a mutated valid frame, a truncation, and noise.
        //    Both notification decoders see every image, so a frame one of
        //    them accepts cannot confuse the other.
        let seed = rng.pick(&notify_seeds);
        let mut mutated = seed.clone();
        scramble(&mut mutated, 8, &mut rng);
        exercise_notifications(&mutated);
        exercise_notifications(&seed[..rng.at_most(seed.len())]);
        let widest_notify = AUDIO_CHANNEL_NOTIFY_LEN.max(AUDIO_NOTIFY_LEN);
        let mut noise = vec![0u8; rng.at_most(widest_notify + 8)];
        rng.fill(&mut noise);
        exercise_notifications(&noise);

        // 6. Replies: every decoder sees every image, so a frame meant for one
        //    reply shape cannot be mistaken for another.
        let seed = rng.pick(&reply_seeds);
        let mut mutated = seed.clone();
        scramble(&mut mutated, 16, &mut rng);
        exercise_replies(&mutated);
        exercise_replies(&seed[..rng.at_most(seed.len())]);
        let mut noise = vec![0u8; rng.at_most(widest_reply + 8)];
        rng.fill(&mut noise);
        exercise_replies(&noise);

        // 7. The ring over positions a hostile peer could have written:
        //    backwards, over-full, and at the very top of the counter where a
        //    careless publish would overflow.
        let geometry = *rng.pick(&geometries);
        let producer = match rng.at_most(3) {
            0 => rng.next_u64(),
            1 => u64::MAX - u64::from(rng.next_u8()),
            2 => u64::from(rng.next_u8()),
            _ => 0,
        };
        let consumer = match rng.at_most(3) {
            0 => rng.next_u64(),
            1 => producer.wrapping_sub(u64::from(rng.next_u8())),
            2 => producer.wrapping_add(u64::from(rng.next_u8())),
            _ => producer,
        };
        exercise_ring(geometry, producer, consumer, &mut rng);

        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
