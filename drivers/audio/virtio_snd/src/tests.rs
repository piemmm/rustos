//! Host tests for the virtio sound device engine, driven against the
//! in-process [`MockTransport`] with a shim that answers as the device does.
//!
//! The shim is the *specification's* device: it answers the information
//! queries from a table, accepts the stream commands, and completes transfer
//! buffers. What the tests then exercise is this driver's reading of those
//! answers, its substitution policy, and the position and loss accounting the
//! mixer's clock is built on.

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::vec;
use core::cell::RefCell;

use tairix_abi::driver::audio_ring::{aligned_region, PcmGeometry, PcmRing, REGION_ALIGN_PADDING};
use tairix_virtio::{ChainView, MockHost, MockTransport, VirtioError};

use super::*;

/// A deterministic monotonic clock the tests advance by hand.
struct StepClock(RefCell<u64>);

impl StepClock {
    fn new() -> Self {
        Self(RefCell::new(0))
    }
}

impl MonotonicClock for StepClock {
    fn now_ns(&self) -> u64 {
        let mut now = self.0.borrow_mut();
        *now += 1_000_000;
        *now
    }
}

/// Which streams the mock device presents and what each can do.
#[derive(Clone)]
struct DeviceSpec {
    jacks: u32,
    chmaps: u32,
    /// Per stream: direction byte, format mask, rate mask, channels min/max.
    streams: alloc::vec::Vec<(u8, u64, u64, u8, u8)>,
    /// Status the next control request answers with, if not `OK`.
    refuse: Option<u32>,
    /// Control codes this device answers `NOT_SUPP` to, leaving the rest
    /// implemented — how a real device declines one query class.
    not_supported: alloc::vec::Vec<u32>,
}

impl DeviceSpec {
    /// A QEMU-shaped device: one output and one input, S16/S32 at the
    /// standard rates, stereo, and jacks and channel maps *advertised in the
    /// configuration but not answerable* — which is what QEMU's
    /// virtio-sound does, and what the end-to-end vertical runs it as.
    fn qemu() -> Self {
        let formats = (1u64 << wire::format::S16) | (1u64 << wire::format::S32);
        let rates = (1u64 << 6) | (1u64 << 7) | (1u64 << 10);
        Self {
            jacks: 1,
            chmaps: 2,
            streams: vec![
                (wire::direction::OUTPUT, formats, rates, 1, 2),
                (wire::direction::INPUT, formats, rates, 1, 2),
            ],
            refuse: None,
            not_supported: vec![wire::request::JACK_INFO, wire::request::CHMAP_INFO],
        }
    }

    /// The same device with every query implemented, as a card that does
    /// describe its topology presents.
    fn describing() -> Self {
        Self {
            not_supported: alloc::vec::Vec::new(),
            ..Self::qemu()
        }
    }
}

/// What the mock device recorded, for assertions about what was programmed.
#[derive(Default)]
struct DeviceLog {
    /// `(code, stream_id)` of every stream command.
    commands: alloc::vec::Vec<(u32, u32)>,
    /// The last `SET_PARAMS` as `(buffer_bytes, period_bytes, channels,
    /// format, rate)`.
    set_params: Option<(u32, u32, u8, u8, u8)>,
    /// Payload bytes the device consumed from transmit buffers, in order.
    played: alloc::vec::Vec<u8>,
}

/// Build a mock transport whose control queue answers as `spec` describes and
/// whose transfer queues complete every posted buffer.
fn mock_device(spec: &DeviceSpec, log: &Rc<RefCell<DeviceLog>>) -> MockTransport {
    let mut transport = MockTransport::new(
        wire::QUEUE_COUNT,
        64,
        wire::VIRTIO_F_VERSION_1,
        wire::config::LEN,
    );
    transport.set_synchronous_notify(true);
    transport.set_config(wire::config::JACKS, &spec.jacks.to_le_bytes());
    let streams = u32::try_from(spec.streams.len()).expect("small");
    transport.set_config(wire::config::STREAMS, &streams.to_le_bytes());
    transport.set_config(wire::config::CHMAPS, &spec.chmaps.to_le_bytes());

    let control_spec = spec.clone();
    let control_log = Rc::clone(log);
    transport.install_shim(
        wire::CONTROL_QUEUE,
        Box::new(move |chain: &mut ChainView<'_>| {
            let request = chain.device_read.first().copied().unwrap_or(&[]);
            let reply = chain
                .device_write
                .first_mut()
                .ok_or(VirtioError::DeviceFault)?;
            let code = wire::read_u32(request, 0);
            if let Some(status) = control_spec.refuse {
                wire::put_u32(reply, 0, status);
                return Ok(u32::try_from(wire::HDR_LEN).expect("small"));
            }
            if control_spec.not_supported.contains(&code) {
                wire::put_u32(reply, 0, wire::status::NOT_SUPP);
                return Ok(u32::try_from(wire::HDR_LEN).expect("small"));
            }
            wire::put_u32(reply, 0, wire::status::OK);
            let written = match code {
                wire::request::PCM_INFO => {
                    let id = wire::read_u32(request, 4) as usize;
                    let (direction, formats, rates, min, max) = *control_spec
                        .streams
                        .get(id)
                        .ok_or(VirtioError::DeviceFault)?;
                    let body = &mut reply[wire::HDR_LEN..];
                    body[wire::pcm_info::FORMATS..wire::pcm_info::FORMATS + 8]
                        .copy_from_slice(&formats.to_le_bytes());
                    body[wire::pcm_info::RATES..wire::pcm_info::RATES + 8]
                        .copy_from_slice(&rates.to_le_bytes());
                    body[wire::pcm_info::DIRECTION] = direction;
                    body[wire::pcm_info::CHANNELS_MIN] = min;
                    body[wire::pcm_info::CHANNELS_MAX] = max;
                    wire::HDR_LEN + wire::pcm_info::LEN
                }
                wire::request::CHMAP_INFO => {
                    let id = wire::read_u32(request, 4);
                    let body = &mut reply[wire::HDR_LEN..];
                    // Map 0 is the output's stereo layout, map 1 the input's.
                    body[wire::chmap_info::DIRECTION] = if id == 0 {
                        wire::direction::OUTPUT
                    } else {
                        wire::direction::INPUT
                    };
                    body[wire::chmap_info::CHANNELS] = 2;
                    body[wire::chmap_info::POSITIONS] = wire::chmap::FL;
                    body[wire::chmap_info::POSITIONS + 1] = wire::chmap::FR;
                    wire::HDR_LEN + wire::chmap_info::LEN
                }
                wire::request::JACK_INFO => {
                    reply[wire::HDR_LEN + wire::jack_info::CONNECTED] = 1;
                    wire::HDR_LEN + wire::jack_info::LEN
                }
                wire::request::PCM_SET_PARAMS => {
                    control_log.borrow_mut().set_params = Some((
                        wire::read_u32(request, 8),
                        wire::read_u32(request, 12),
                        request[20],
                        request[21],
                        request[22],
                    ));
                    wire::HDR_LEN
                }
                _ => {
                    control_log
                        .borrow_mut()
                        .commands
                        .push((code, wire::read_u32(request, 4)));
                    wire::HDR_LEN
                }
            };
            Ok(u32::try_from(written).expect("small"))
        }),
    );

    install_transfer_shims(&mut transport, log);
    transport
}

/// Install the two transfer-queue shims: the transmit side records what it was
/// played, and the receive side writes a recognisable captured signal.
fn install_transfer_shims(transport: &mut MockTransport, log: &Rc<RefCell<DeviceLog>>) {
    let tx_log = Rc::clone(log);
    transport.install_shim(
        wire::TX_QUEUE,
        Box::new(move |chain: &mut ChainView<'_>| {
            if let Some(payload) = chain.device_read.get(1) {
                tx_log.borrow_mut().played.extend_from_slice(payload);
            }
            let status = chain
                .device_write
                .last_mut()
                .ok_or(VirtioError::DeviceFault)?;
            wire::put_u32(status, 0, wire::status::OK);
            wire::put_u32(status, 4, 0);
            Ok(u32::try_from(wire::XFER_STATUS_LEN).expect("small"))
        }),
    );
    transport.install_shim(
        wire::RX_QUEUE,
        Box::new(move |chain: &mut ChainView<'_>| {
            let mut written = 0u32;
            // The payload descriptor first, the status word last.
            let count = chain.device_write.len();
            for (index, slot) in chain.device_write.iter_mut().enumerate() {
                if index + 1 == count {
                    wire::put_u32(slot, 0, wire::status::OK);
                    wire::put_u32(slot, 4, 0);
                    written += u32::try_from(wire::XFER_STATUS_LEN).expect("small");
                } else {
                    // A recognisable captured signal rather than zeroes, so a
                    // test can tell "the device wrote" from "nothing did".
                    slot.fill(0x5A);
                    written += u32::try_from(slot.len()).expect("small");
                }
            }
            Ok(written)
        }),
    );
}

/// Frames the tests' shared ring holds.
const RING_FRAMES: u32 = 512;
/// Frames one test period carries.
const PERIOD_FRAMES: u32 = 128;

/// A stereo 16-bit ring region the driver services against.
struct Ring {
    bytes: alloc::vec::Vec<u8>,
    geometry: PcmGeometry,
}

impl Ring {
    fn new() -> Self {
        let geometry = PcmGeometry::new(RING_FRAMES, SampleFormat::S16, 2).expect("valid");
        Self {
            bytes: vec![0u8; geometry.region_len() + REGION_ALIGN_PADDING],
            geometry,
        }
    }

    fn bind(&mut self) -> PcmRing<'_> {
        let len = self.geometry.region_len();
        let view = aligned_region(&mut self.bytes, len).expect("padded");
        PcmRing::bind(view, self.geometry).expect("binds")
    }
}

/// [`PERIODS_IN_FLIGHT`] as the frame-count arithmetic the assertions do.
fn periods_in_flight() -> u32 {
    u32::try_from(PERIODS_IN_FLIGHT).expect("three")
}

fn params() -> ConfigureParams {
    ConfigureParams {
        endpoint: 0,
        rate: Rate::HZ_48000,
        format: SampleFormat::S16,
        channel_map: ChannelMap::STEREO,
        period_frames: PERIOD_FRAMES,
    }
}

#[test]
fn bring_up_reads_what_the_device_says_rather_than_assuming_it() {
    let log = Rc::new(RefCell::new(DeviceLog::default()));
    let host = MockHost::new();
    let clock = StepClock::new();
    let device = VirtioSnd::open(mock_device(&DeviceSpec::qemu(), &log), &host, &clock)
        .expect("the device comes up");

    let facts = device.device_facts().expect("facts");
    assert_eq!(facts.endpoints, 2);

    let sink = device.endpoint_facts(0).expect("sink facts");
    assert_eq!(sink.direction, StreamDirection::Playback);
    assert!(sink.formats.contains(SampleFormat::S16));
    assert!(sink.formats.contains(SampleFormat::S32));
    // A format the device did not offer is not advertised, even though the
    // engine could convert to it.
    assert!(!sink.formats.contains(SampleFormat::F32));
    assert!(sink.rates.admits(Rate::HZ_48000));
    assert!(sink.rates.admits(Rate::new(44_100).expect("in range")));
    assert!(!sink.rates.admits(Rate::new(192_000).expect("in range")));
    // No jacks were published, so the honest answer is that the endpoint has
    // no detection — never an invented "present".
    assert_eq!(sink.jack, JackState::Unknown);
    // No channel maps were published either, so the conventional layout for
    // the channel count is what the count means.
    assert_eq!(sink.channel_map, ChannelMap::STEREO);
    // The device exposes no gain control this driver negotiates, so it says
    // so and the mixer applies the gain itself.
    assert!(sink.gain.is_none());

    let source = device.endpoint_facts(1).expect("source facts");
    assert_eq!(source.direction, StreamDirection::Capture);

    assert_eq!(device.endpoint_facts(2).unwrap_err(), DriverError::NotFound);
}

#[test]
fn published_jacks_and_channel_maps_are_used_where_the_device_offers_them() {
    let mut spec = DeviceSpec::describing();
    spec.jacks = 2;
    spec.chmaps = 2;
    let log = Rc::new(RefCell::new(DeviceLog::default()));
    let host = MockHost::new();
    let clock = StepClock::new();
    let device = VirtioSnd::open(mock_device(&spec, &log), &host, &clock).expect("comes up");

    let sink = device.endpoint_facts(0).expect("sink facts");
    assert_eq!(sink.jack, JackState::Present);
    assert_eq!(sink.channel_map, ChannelMap::STEREO);
}

#[test]
fn a_device_that_advertises_jacks_and_maps_but_answers_neither_still_comes_up() {
    // QEMU's virtio-sound advertises the counts its command line was given
    // and answers `NOT_SUPP` to both query classes. Refusing bring-up over
    // that would reject a device the driver can drive; the streams are what
    // carry audio, and they described themselves.
    let log = Rc::new(RefCell::new(DeviceLog::default()));
    let host = MockHost::new();
    let clock = StepClock::new();
    let spec = DeviceSpec::qemu();
    assert!(spec.jacks > 0 && spec.chmaps > 0, "the queries are issued");
    let device = VirtioSnd::open(mock_device(&spec, &log), &host, &clock).expect("comes up");

    // Undescribed, never invented: the conventional layout is what the
    // facts fall back to, and the jack is honestly unknown.
    let sink = device.endpoint_facts(0).expect("sink facts");
    assert_eq!(sink.jack, JackState::Unknown);
    assert_eq!(sink.channel_map, ChannelMap::STEREO);
}

#[test]
fn a_device_that_will_not_describe_its_streams_is_still_refused() {
    // The tolerance above is for the descriptive classes only. A stream
    // that cannot be described cannot be driven, so a refused `PCM_INFO`
    // stays a bring-up failure rather than yielding a device with no
    // usable endpoint.
    let log = Rc::new(RefCell::new(DeviceLog::default()));
    let host = MockHost::new();
    let clock = StepClock::new();
    let mut spec = DeviceSpec::qemu();
    spec.not_supported.push(wire::request::PCM_INFO);
    assert_eq!(
        VirtioSnd::open(mock_device(&spec, &log), &host, &clock)
            .err()
            .expect("refused"),
        DriverError::NotImplemented
    );
}

#[test]
fn a_device_describing_no_streams_or_too_many_is_refused() {
    for streams in [0usize, usize::from(MAX_DEVICE_ENDPOINTS) + 1] {
        let mut spec = DeviceSpec::qemu();
        let formats = 1u64 << wire::format::S16;
        let rates = 1u64 << 7;
        spec.streams = vec![(wire::direction::OUTPUT, formats, rates, 2, 2); streams];
        let log = Rc::new(RefCell::new(DeviceLog::default()));
        let host = MockHost::new();
        let clock = StepClock::new();
        assert_eq!(
            VirtioSnd::open(mock_device(&spec, &log), &host, &clock)
                .err()
                .expect("refused"),
            DriverError::DeviceFault,
            "a device claiming {streams} streams must be refused"
        );
    }
}

#[test]
fn a_stream_offering_no_encoding_this_stack_speaks_is_a_device_fault() {
    let mut spec = DeviceSpec::qemu();
    // `VIRTIO_SND_PCM_FMT_MU_LAW` alone: a real encoding this engine does not
    // convert, so the stream carries nothing it could mix.
    spec.streams = vec![(wire::direction::OUTPUT, 1u64 << 1, 1u64 << 7, 2, 2)];
    let log = Rc::new(RefCell::new(DeviceLog::default()));
    let host = MockHost::new();
    let clock = StepClock::new();
    assert_eq!(
        VirtioSnd::open(mock_device(&spec, &log), &host, &clock)
            .err()
            .expect("refused"),
        DriverError::DeviceFault
    );
}

#[test]
fn configure_programs_the_device_and_answers_what_it_will_run_at() {
    let log = Rc::new(RefCell::new(DeviceLog::default()));
    let host = MockHost::new();
    let clock = StepClock::new();
    let mut device =
        VirtioSnd::open(mock_device(&DeviceSpec::qemu(), &log), &host, &clock).expect("comes up");

    let grant = device.configure(0, &params()).expect("configured");
    assert_eq!(grant.rate, Rate::HZ_48000);
    assert_eq!(grant.format, SampleFormat::S16);
    assert_eq!(grant.channel_map, ChannelMap::STEREO);
    assert_eq!(grant.period_frames, PERIOD_FRAMES);
    grant.validate().expect("a grant a ring can be built from");

    let recorded = log.borrow();
    let (buffer_bytes, period_bytes, channels, format, rate) =
        recorded.set_params.expect("SET_PARAMS reached the device");
    assert_eq!(period_bytes, PERIOD_FRAMES * 4);
    assert_eq!(buffer_bytes, period_bytes * periods_in_flight());
    assert_eq!(channels, 2);
    assert_eq!(format, wire::format::S16);
    assert_eq!(rate, 7, "rate index 7 is 48 kHz");
    assert!(recorded.commands.contains(&(wire::request::PCM_PREPARE, 0)));
}

#[test]
fn a_request_the_device_cannot_meet_is_substituted_rather_than_refused() {
    let log = Rc::new(RefCell::new(DeviceLog::default()));
    let host = MockHost::new();
    let clock = StepClock::new();
    let mut device =
        VirtioSnd::open(mock_device(&DeviceSpec::qemu(), &log), &host, &clock).expect("comes up");

    let asked = ConfigureParams {
        // A rate the device does not clock at, and an encoding it does not
        // accept: both are answered with what it *will* do, so the mixer
        // adapts instead of failing.
        rate: Rate::new(192_000).expect("in range"),
        format: SampleFormat::F32,
        ..params()
    };
    let grant = device.configure(0, &asked).expect("configured");
    assert_eq!(grant.rate, Rate::new(96_000).expect("in range"));
    assert_eq!(grant.format, SampleFormat::S32);
    grant.validate().expect("still a usable grant");
}

#[test]
fn a_period_is_rounded_up_to_a_power_of_two_inside_the_drivers_own_bound() {
    let log = Rc::new(RefCell::new(DeviceLog::default()));
    let host = MockHost::new();
    let clock = StepClock::new();
    let mut device =
        VirtioSnd::open(mock_device(&DeviceSpec::qemu(), &log), &host, &clock).expect("comes up");

    let grant = device
        .configure(
            0,
            &ConfigureParams {
                period_frames: 100,
                ..params()
            },
        )
        .expect("configured");
    assert_eq!(grant.period_frames, 128);

    // A period past the driver's own DMA bound is refused rather than
    // reserving whatever was asked for.
    assert_eq!(
        device
            .configure(
                0,
                &ConfigureParams {
                    period_frames: MAX_PERIOD_FRAMES,
                    ..params()
                },
            )
            .expect("clamped to the bound")
            .period_frames,
        MAX_PERIOD_FRAMES
    );
}

#[test]
fn a_configure_on_a_running_stream_is_refused_rather_than_reprogramming_it() {
    let log = Rc::new(RefCell::new(DeviceLog::default()));
    let host = MockHost::new();
    let clock = StepClock::new();
    let mut device =
        VirtioSnd::open(mock_device(&DeviceSpec::qemu(), &log), &host, &clock).expect("comes up");
    device.configure(0, &params()).expect("configured");
    device.start(0, Frames::ZERO).expect("started");
    assert_eq!(
        device.configure(0, &params()).unwrap_err(),
        DriverError::Busy
    );
}

#[test]
fn playback_moves_the_rings_frames_and_the_position_follows_them() {
    let log = Rc::new(RefCell::new(DeviceLog::default()));
    let host = MockHost::new();
    let clock = StepClock::new();
    let mut device =
        VirtioSnd::open(mock_device(&DeviceSpec::qemu(), &log), &host, &clock).expect("comes up");
    device.configure(0, &params()).expect("configured");
    device.start(0, Frames::ZERO).expect("started");

    let mut ring = Ring::new();
    // A recognisable ramp: every frame distinct, so a reordered or dropped
    // period cannot pass.
    let mut signal = vec![0u8; PERIOD_FRAMES as usize * 4];
    for (index, byte) in signal.iter_mut().enumerate() {
        *byte = u8::try_from(index % 251).expect("in range");
    }
    {
        let mut bound = ring.bind();
        assert_eq!(
            bound.write(&signal).expect("written"),
            PERIOD_FRAMES,
            "the whole period reaches the ring"
        );
    }

    let report = {
        let mut bound = ring.bind();
        device.service(0, &mut bound).expect("serviced")
    };
    assert_eq!(report.transferred, PERIOD_FRAMES);
    assert!(report.running);
    assert_eq!(
        report.xrun_frames, 0,
        "nothing was missing, so nothing was lost"
    );
    assert_eq!(report.position, Frames::new(u64::from(PERIOD_FRAMES)));

    let played = &log.borrow().played;
    assert_eq!(
        &played[..signal.len()],
        &signal[..],
        "the device received exactly the frames the ring held"
    );
}

#[test]
fn a_short_ring_is_padded_with_silence_and_the_loss_is_counted_exactly() {
    let log = Rc::new(RefCell::new(DeviceLog::default()));
    let host = MockHost::new();
    let clock = StepClock::new();
    let mut device =
        VirtioSnd::open(mock_device(&DeviceSpec::qemu(), &log), &host, &clock).expect("comes up");
    device.configure(0, &params()).expect("configured");
    device.start(0, Frames::ZERO).expect("started");

    let mut ring = Ring::new();
    let short = 32u32;
    {
        let mut bound = ring.bind();
        bound
            .write(&vec![0x11u8; short as usize * 4])
            .expect("written");
    }
    let report = {
        let mut bound = ring.bind();
        device.service(0, &mut bound).expect("serviced")
    };
    // A running device must be fed, so the missing frames are silence — and
    // they are *counted*, so the mixer learns exactly what was lost rather
    // than discovering a drift later.
    assert_eq!(report.transferred, PERIOD_FRAMES);
    assert_eq!(report.xrun_frames, u64::from(PERIOD_FRAMES - short));
    assert_eq!(report.position, Frames::new(u64::from(PERIOD_FRAMES)));

    let played = log.borrow();
    let payload = &played.played[..PERIOD_FRAMES as usize * 4];
    assert!(payload[..short as usize * 4].iter().all(|b| *b == 0x11));
    assert!(
        payload[short as usize * 4..].iter().all(|b| *b == 0),
        "the shortfall is the format's own silence, not stale bytes"
    );
}

#[test]
fn a_stopped_stream_is_never_fed_silence_to_keep_it_busy() {
    let log = Rc::new(RefCell::new(DeviceLog::default()));
    let host = MockHost::new();
    let clock = StepClock::new();
    let mut device =
        VirtioSnd::open(mock_device(&DeviceSpec::qemu(), &log), &host, &clock).expect("comes up");
    device.configure(0, &params()).expect("configured");

    let mut ring = Ring::new();
    let report = {
        let mut bound = ring.bind();
        device.service(0, &mut bound).expect("serviced")
    };
    assert_eq!(report.transferred, 0);
    assert_eq!(report.xrun_frames, 0);
    assert!(!report.running);
    assert!(log.borrow().played.is_empty());
}

#[test]
fn a_ring_that_is_not_the_configured_shape_is_refused() {
    let log = Rc::new(RefCell::new(DeviceLog::default()));
    let host = MockHost::new();
    let clock = StepClock::new();
    let mut device =
        VirtioSnd::open(mock_device(&DeviceSpec::qemu(), &log), &host, &clock).expect("comes up");
    device.configure(0, &params()).expect("configured");

    let geometry = PcmGeometry::new(RING_FRAMES, SampleFormat::S32, 2).expect("valid");
    let mut bytes = vec![0u8; geometry.region_len() + REGION_ALIGN_PADDING];
    let view = aligned_region(&mut bytes, geometry.region_len()).expect("padded");
    let mut wrong = PcmRing::bind(view, geometry).expect("binds");
    assert_eq!(
        device.service(0, &mut wrong).unwrap_err(),
        DriverError::BadMagic,
        "frames of another encoding are not this stream's frames"
    );
}

#[test]
fn capture_posts_buffers_and_carries_what_the_device_wrote_into_the_ring() {
    let log = Rc::new(RefCell::new(DeviceLog::default()));
    let host = MockHost::new();
    let clock = StepClock::new();
    let mut device =
        VirtioSnd::open(mock_device(&DeviceSpec::qemu(), &log), &host, &clock).expect("comes up");
    device
        .configure(
            1,
            &ConfigureParams {
                endpoint: 1,
                ..params()
            },
        )
        .expect("configured");
    device.start(1, Frames::ZERO).expect("started");

    let mut ring = Ring::new();
    // The first service only hands the device its buffers: nothing has been
    // captured yet, so the position has not moved. Frames nobody received are
    // not frames that arrived.
    let first = {
        let mut bound = ring.bind();
        device.service(1, &mut bound).expect("serviced")
    };
    assert!(first.running);
    assert_eq!(first.transferred, 0);
    assert_eq!(first.position, Frames::ZERO);

    // The second reaps them, and the captured frames reach the ring.
    let second = {
        let mut bound = ring.bind();
        device.service(1, &mut bound).expect("serviced")
    };
    let captured = PERIOD_FRAMES * periods_in_flight();
    assert_eq!(second.transferred, captured);
    assert_eq!(second.position, Frames::new(u64::from(captured)));
    assert_eq!(second.xrun_frames, 0);

    let mut out = vec![0u8; captured as usize * 4];
    let read = {
        let mut bound = ring.bind();
        bound.read(&mut out).expect("readable")
    };
    assert_eq!(read, captured);
    assert!(
        out.iter().all(|byte| *byte == 0x5A),
        "the ring holds what the device wrote, not what was left in the buffer"
    );
}

#[test]
fn capture_the_mixer_did_not_drain_is_counted_as_lost_rather_than_dropped_silently() {
    let log = Rc::new(RefCell::new(DeviceLog::default()));
    let host = MockHost::new();
    let clock = StepClock::new();
    let mut device =
        VirtioSnd::open(mock_device(&DeviceSpec::qemu(), &log), &host, &clock).expect("comes up");
    device
        .configure(
            1,
            &ConfigureParams {
                endpoint: 1,
                period_frames: RING_FRAMES,
                ..params()
            },
        )
        .expect("configured");
    device.start(1, Frames::ZERO).expect("started");

    // Each period is the whole ring, so the second and third completions have
    // nowhere to go.
    let mut ring = Ring::new();
    {
        let mut bound = ring.bind();
        device.service(1, &mut bound).expect("serviced");
    }
    let report = {
        let mut bound = ring.bind();
        device.service(1, &mut bound).expect("serviced")
    };
    assert_eq!(report.transferred, RING_FRAMES);
    assert_eq!(
        report.xrun_frames,
        u64::from(RING_FRAMES) * u64::from(periods_in_flight() - 1),
        "what the ring could not hold is over-run, and it is counted"
    );
}

#[test]
fn a_control_refusal_reaches_the_caller_as_the_typed_error_the_device_named() {
    for (status, expected) in [
        (wire::status::NOT_SUPP, DriverError::NotImplemented),
        (wire::status::BAD_MSG, DriverError::OutOfRange),
        (wire::status::IO_ERR, DriverError::DeviceFault),
    ] {
        let mut spec = DeviceSpec::qemu();
        spec.refuse = Some(status);
        let log = Rc::new(RefCell::new(DeviceLog::default()));
        let host = MockHost::new();
        let clock = StepClock::new();
        assert_eq!(
            VirtioSnd::open(mock_device(&spec, &log), &host, &clock)
                .err()
                .expect("refused"),
            expected,
            "status {status:#x} must surface as its own error"
        );
    }
}

#[test]
fn release_stops_and_releases_the_device_and_a_second_release_is_harmless() {
    let log = Rc::new(RefCell::new(DeviceLog::default()));
    let host = MockHost::new();
    let clock = StepClock::new();
    let mut device =
        VirtioSnd::open(mock_device(&DeviceSpec::qemu(), &log), &host, &clock).expect("comes up");
    device.configure(0, &params()).expect("configured");
    device.start(0, Frames::ZERO).expect("started");
    device.release(0).expect("released");

    let recorded = log.borrow();
    assert!(recorded.commands.contains(&(wire::request::PCM_STOP, 0)));
    assert!(recorded.commands.contains(&(wire::request::PCM_RELEASE, 0)));
    drop(recorded);

    // An unconfigured endpoint has nothing to release, and saying so is not
    // an error: the channel server calls this whenever a channel goes away.
    device.release(0).expect("idempotent");
    assert_eq!(device.release(9).unwrap_err(), DriverError::NotFound);
}

#[test]
fn gain_is_refused_because_the_endpoint_honestly_reports_none() {
    let log = Rc::new(RefCell::new(DeviceLog::default()));
    let host = MockHost::new();
    let clock = StepClock::new();
    let mut device =
        VirtioSnd::open(mock_device(&DeviceSpec::qemu(), &log), &host, &clock).expect("comes up");
    assert_eq!(
        device.set_gain(0, -600, false).unwrap_err(),
        DriverError::NotImplemented
    );
    assert_eq!(
        device.set_gain(7, 0, false).unwrap_err(),
        DriverError::NotFound
    );
}

#[test]
fn a_posted_transfer_is_reported_as_a_period_boundary_even_with_no_device_event() {
    let log = Rc::new(RefCell::new(DeviceLog::default()));
    let host = MockHost::new();
    let clock = StepClock::new();
    let mut device =
        VirtioSnd::open(mock_device(&DeviceSpec::qemu(), &log), &host, &clock).expect("comes up");
    // Nothing is posted yet, so there is nothing to report.
    assert!(device.take_interrupt().expect("read").is_empty());

    device
        .configure(
            1,
            &ConfigureParams {
                endpoint: 1,
                ..params()
            },
        )
        .expect("configured");
    device.start(1, Frames::ZERO).expect("started");
    let mut ring = Ring::new();
    {
        let mut bound = ring.bind();
        device.service(1, &mut bound).expect("serviced");
    }
    // The mock completes synchronously, so the buffers are already back; the
    // engine reports nothing it cannot substantiate.
    let causes = device.take_interrupt().expect("read");
    assert_eq!(causes.xrun, 0);
    assert_eq!(causes.jack_changed, 0);
}

#[test]
fn masking_the_event_sources_is_the_used_ring_suppression_the_bus_offers() {
    let log = Rc::new(RefCell::new(DeviceLog::default()));
    let host = MockHost::new();
    let clock = StepClock::new();
    let mut device =
        VirtioSnd::open(mock_device(&DeviceSpec::qemu(), &log), &host, &clock).expect("comes up");
    assert!(!device.events_armed());
    device.set_event_interrupts(true).expect("armed");
    assert!(device.events_armed());
    device.set_event_interrupts(false).expect("masked");
    assert!(!device.events_armed());
}

#[test]
fn the_bind_table_matches_a_virtio_sound_node_and_nothing_else() {
    assert_eq!(BIND_KEYS.len(), 1);
    assert_eq!(BIND_KEYS[0].priority, BIND_PRIORITY);
    assert!(BIND_KEYS[0]
        .key
        .matches(&HwMatchKey::virtio(VIRTIO_SND_DEVICE_ID)));
    assert!(!BIND_KEYS[0].key.matches(&HwMatchKey::virtio(18)));
}

#[test]
fn the_conventional_layout_is_only_claimed_where_one_exists() {
    assert_eq!(conventional_map(1).map(|m| m.channels()), Some(1));
    assert_eq!(conventional_map(2), Some(ChannelMap::STEREO));
    assert_eq!(conventional_map(6).map(|m| m.channels()), Some(6));
    assert_eq!(conventional_map(8).map(|m| m.channels()), Some(8));
    // Five and seven channels have no conventional reading, so none is
    // invented.
    assert_eq!(conventional_map(5), None);
    assert_eq!(conventional_map(7), None);
    assert_eq!(conventional_map(0), None);
    assert_eq!(conventional_map(9), None);
}

#[test]
fn a_channel_map_naming_a_position_this_stack_cannot_place_is_left_unpublished() {
    let mut record = [0u8; wire::MAX_INFO_RECORD_LEN];
    record[wire::chmap_info::CHANNELS] = 2;
    record[wire::chmap_info::POSITIONS] = wire::chmap::FL;
    // `VIRTIO_SND_CHMAP_RC` (rear centre) has no position in this vocabulary,
    // so the whole map is refused rather than half-read.
    record[wire::chmap_info::POSITIONS + 1] = 11;
    assert_eq!(decode_chmap(&record, 2), None);

    record[wire::chmap_info::POSITIONS + 1] = wire::chmap::FR;
    assert_eq!(decode_chmap(&record, 2), Some(ChannelMap::STEREO));
    assert_eq!(decode_chmap(&record, 0), None);
    assert_eq!(decode_chmap(&record, 9), None);
}
