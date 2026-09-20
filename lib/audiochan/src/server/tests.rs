//! Host tests for the pure per-endpoint device-channel handler, driven
//! against a mock [`Audio`] device.
//!
//! The mock is a *device*, not a second implementation of the contract: it
//! records what it was asked to do and answers what a real converter would,
//! so what these tests exercise is the server's state machine, its geometry
//! validation, and its fail-closed refusals.

use super::*;
use tairix_abi::driver::audio::{
    AudioDeviceFacts, AudioEndpointFacts, AudioInterrupt, AudioName, ChannelMap, GainRange,
    JackState, Rate, RateSet, RateSupport, SampleFormat, SampleFormats, StreamDirection,
};
use tairix_abi::driver::audio_ring::{aligned_region, REGION_ALIGN_PADDING};
use tairix_abi::reply::decode_status_reply;
use tairix_abi::time::Time64;

/// Endpoints the mock presents: one sink, one source.
const MOCK_ENDPOINTS: u16 = 2;
/// The mock's playback endpoint.
const SINK: u16 = 0;
/// The mock's capture endpoint.
const SOURCE: u16 = 1;
/// The only rate the mock's converter runs at, so a request for another rate
/// exercises the substitution path.
const MOCK_RATE_HZ: u32 = 48_000;
/// Frames the mock interrupts on, whatever was asked for.
const MOCK_PERIOD: u32 = 256;
/// Frames the mock can hold in flight.
const MOCK_MAX_RING: u32 = 4_096;

/// Where one endpoint of the mock device stands in its own lifecycle.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
enum MockState {
    /// Nothing programmed.
    #[default]
    Idle,
    /// Programmed but not clocking.
    Configured,
    /// Clocking.
    Running,
}

/// What one endpoint of the mock device is doing.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
struct MockEndpoint {
    state: MockState,
    drained: bool,
    released: u32,
    position: Frames,
    xrun_frames: u64,
    gain_millibel: i32,
    muted: bool,
}

impl MockEndpoint {
    fn configured(&self) -> bool {
        !matches!(self.state, MockState::Idle)
    }

    fn running(&self) -> bool {
        matches!(self.state, MockState::Running)
    }
}

/// A two-endpoint audio device that answers like a real converter and records
/// what it was told.
struct MockAudio {
    endpoints: [MockEndpoint; MOCK_ENDPOINTS as usize],
    /// Frames the next `service` reports as moved.
    transfer: u32,
    /// Frames the next `service` adds to the endpoint's loss tally.
    add_xrun: u64,
    /// Whether the device reports a gain control.
    has_gain: bool,
    /// What the next `take_interrupt` reports.
    pending: AudioInterrupt,
    /// Event sources armed.
    events_enabled: bool,
    /// Every call the device engine refuses, so the server's error paths are
    /// reachable without a broken mock.
    fault: Option<DriverError>,
}

impl MockAudio {
    fn new() -> Self {
        Self {
            endpoints: [MockEndpoint::default(); MOCK_ENDPOINTS as usize],
            transfer: 0,
            add_xrun: 0,
            has_gain: true,
            pending: AudioInterrupt::NONE,
            events_enabled: false,
            fault: None,
        }
    }

    fn slot(&self, endpoint: u16) -> Result<&MockEndpoint, DriverError> {
        self.endpoints
            .get(usize::from(endpoint))
            .ok_or(DriverError::NotFound)
    }

    fn slot_mut(&mut self, endpoint: u16) -> Result<&mut MockEndpoint, DriverError> {
        if let Some(err) = self.fault {
            return Err(err);
        }
        self.endpoints
            .get_mut(usize::from(endpoint))
            .ok_or(DriverError::NotFound)
    }
}

impl Audio for MockAudio {
    fn device_facts(&self) -> Result<AudioDeviceFacts, DriverError> {
        if let Some(err) = self.fault {
            return Err(err);
        }
        Ok(AudioDeviceFacts {
            endpoints: MOCK_ENDPOINTS,
            name: AudioName::new("Mock Converter").expect("fits"),
        })
    }

    fn endpoint_facts(&self, endpoint: u16) -> Result<AudioEndpointFacts, DriverError> {
        if let Some(err) = self.fault {
            return Err(err);
        }
        if endpoint >= MOCK_ENDPOINTS {
            return Err(DriverError::NotFound);
        }
        Ok(AudioEndpointFacts {
            index: endpoint,
            direction: if endpoint == SINK {
                StreamDirection::Playback
            } else {
                StreamDirection::Capture
            },
            jack: JackState::Present,
            formats: SampleFormats::EMPTY.with(SampleFormat::S16),
            channel_map: ChannelMap::STEREO,
            rates: RateSupport::Discrete(
                RateSet::new(&[Rate::new(MOCK_RATE_HZ).expect("in range")]).expect("ascending"),
            ),
            min_period_frames: MOCK_PERIOD,
            max_period_frames: MOCK_PERIOD,
            max_ring_frames: MOCK_MAX_RING,
            gain: self
                .has_gain
                .then(|| GainRange::new(-6_000, 0, 50).expect("ordered")),
            name: AudioName::new("Line Out").expect("fits"),
        })
    }

    fn configure(
        &mut self,
        endpoint: u16,
        _params: &ConfigureParams,
    ) -> Result<ConfigureGrant, DriverError> {
        self.slot_mut(endpoint)?.state = MockState::Configured;
        Ok(ConfigureGrant {
            rate: Rate::new(MOCK_RATE_HZ).expect("in range"),
            format: SampleFormat::S16,
            channel_map: ChannelMap::STEREO,
            period_frames: MOCK_PERIOD,
            max_ring_frames: MOCK_MAX_RING,
        })
    }

    fn start(&mut self, endpoint: u16, at: Frames) -> Result<(), DriverError> {
        let slot = self.slot_mut(endpoint)?;
        if !slot.configured() {
            return Err(DriverError::DeviceFault);
        }
        slot.state = MockState::Running;
        slot.position = at;
        Ok(())
    }

    fn stop(&mut self, endpoint: u16, at: Frames) -> Result<(), DriverError> {
        let slot = self.slot_mut(endpoint)?;
        slot.state = MockState::Configured;
        slot.position = at;
        Ok(())
    }

    fn drain(&mut self, endpoint: u16) -> Result<(), DriverError> {
        self.slot_mut(endpoint)?.drained = true;
        Ok(())
    }

    fn service(
        &mut self,
        endpoint: u16,
        ring: &mut PcmRing<'_>,
    ) -> Result<AudioServiced, DriverError> {
        let transfer = self.transfer;
        let add_xrun = self.add_xrun;
        let direction = if endpoint == SINK {
            StreamDirection::Playback
        } else {
            StreamDirection::Capture
        };
        let slot = self.slot_mut(endpoint)?;
        if !slot.configured() {
            return Err(DriverError::DeviceFault);
        }
        // A real converter reads the ring (playback) or writes it (capture);
        // the mock does the same so a torn geometry would surface here.
        let moved = match direction {
            StreamDirection::Playback => {
                ring.discard(transfer).map_err(|_| DriverError::BadMagic)?
            }
            StreamDirection::Capture => ring
                .write_silence(transfer)
                .map_err(|_| DriverError::BadMagic)?,
        };
        slot.xrun_frames += add_xrun;
        slot.position = Frames::new(slot.position.get() + u64::from(moved));
        Ok(AudioServiced {
            transferred: moved,
            running: slot.running(),
            position: slot.position,
            xrun_frames: slot.xrun_frames,
            sampled_at: Time64::from_secs(7),
        })
    }

    fn set_gain(&mut self, endpoint: u16, millibel: i32, mute: bool) -> Result<(), DriverError> {
        if !self.has_gain {
            return Err(DriverError::NotImplemented);
        }
        let slot = self.slot_mut(endpoint)?;
        slot.gain_millibel = millibel;
        slot.muted = mute;
        Ok(())
    }

    fn release(&mut self, endpoint: u16) -> Result<(), DriverError> {
        let slot = self.slot_mut(endpoint)?;
        slot.state = MockState::Idle;
        slot.released += 1;
        Ok(())
    }

    fn take_interrupt(&mut self) -> Result<AudioInterrupt, DriverError> {
        if let Some(err) = self.fault {
            return Err(err);
        }
        Ok(core::mem::replace(&mut self.pending, AudioInterrupt::NONE))
    }

    fn set_event_interrupts(&mut self, enabled: bool) -> Result<(), DriverError> {
        if let Some(err) = self.fault {
            return Err(err);
        }
        self.events_enabled = enabled;
        Ok(())
    }
}

/// Frames the tests' shared region holds: the smallest power of two that can
/// carry a mock period.
const RING_FRAMES: u32 = 512;

/// A region large enough for [`RING_FRAMES`] stereo 16-bit frames, plus the
/// padding an aligned view is cut from.
struct Region {
    bytes: [u8; RING_BYTES + REGION_ALIGN_PADDING],
}

/// Bytes [`RING_FRAMES`] stereo 16-bit frames need, header included.
const RING_BYTES: usize =
    tairix_abi::driver::audio_ring::PCM_RING_HEADER_LEN + RING_FRAMES as usize * 2 * 2;

impl Region {
    fn new() -> Self {
        Self {
            bytes: [0u8; RING_BYTES + REGION_ALIGN_PADDING],
        }
    }

    fn view(&mut self) -> &mut [u8] {
        aligned_region(&mut self.bytes, RING_BYTES).expect("padded for alignment")
    }
}

fn params(endpoint: u16) -> ConfigureParams {
    ConfigureParams {
        endpoint,
        rate: Rate::new(MOCK_RATE_HZ).expect("in range"),
        format: SampleFormat::S16,
        channel_map: ChannelMap::STEREO,
        period_frames: MOCK_PERIOD,
    }
}

fn attach_params(endpoint: u16, ring_frames: u32) -> AttachParams {
    AttachParams {
        endpoint,
        ring_frames,
        region_grant: 0x5AFE,
        notify_endpoint: 0xACE0 + u64::from(endpoint),
    }
}

/// Configure and attach `endpoint` with a [`RING_FRAMES`] ring.
fn ready(server: &mut AudioChannelServer<MockAudio>, endpoint: u16) {
    assert!(tairix_abi::driver::audio_channel::decode_configure_reply(
        &server.configure_reply(&params(endpoint))
    )
    .is_ok());
    assert_eq!(
        decode_status_reply(&server.attach(&attach_params(endpoint, RING_FRAMES))),
        Ok(())
    );
}

#[test]
fn a_fresh_server_answers_facts_and_refuses_everything_that_needs_a_region() {
    let mut server = AudioChannelServer::new(MockAudio::new());

    let facts = tairix_abi::driver::audio_channel::decode_facts_reply(&server.facts_reply())
        .expect("the device answers its facts before anything is configured");
    assert_eq!(facts.endpoints, MOCK_ENDPOINTS);

    let endpoint = tairix_abi::driver::audio_channel::decode_endpoint_reply(
        &server.endpoint_facts_reply(SINK),
    )
    .expect("and each endpoint's");
    assert_eq!(endpoint.direction, StreamDirection::Playback);

    // Everything that would clock the device refuses: there is nowhere for
    // frames to come from or go.
    assert_eq!(
        decode_status_reply(&server.start(SINK, Frames::ZERO)),
        Err(Errno::NotConnected)
    );
    assert_eq!(
        decode_status_reply(&server.stop(SINK, Frames::ZERO)),
        Err(Errno::NotConnected)
    );
    assert_eq!(
        decode_status_reply(&server.drain(SINK)),
        Err(Errno::NotConnected)
    );
    let mut region = Region::new();
    assert_eq!(
        server.service(SINK, region.view()),
        Err(Errno::NotConnected)
    );
    assert!(!server.any_attached());
}

#[test]
fn an_endpoint_the_contract_does_not_admit_is_refused_before_the_device_is_touched() {
    let mut server = AudioChannelServer::new(MockAudio::new());
    let past = MAX_DEVICE_ENDPOINTS;
    assert_eq!(
        tairix_abi::driver::audio_channel::decode_configure_reply(
            &server.configure_reply(&params(past))
        ),
        Err(Errno::NotFound)
    );
    assert_eq!(
        decode_status_reply(&server.attach(&attach_params(past, RING_FRAMES))),
        Err(Errno::NotFound)
    );
    assert_eq!(
        decode_status_reply(&server.start(past, Frames::ZERO)),
        Err(Errno::NotFound)
    );
    assert_eq!(
        decode_status_reply(&server.set_gain(past, 0, false)),
        Err(Errno::NotFound)
    );
    assert_eq!(
        decode_status_reply(&server.detach(past)),
        Err(Errno::NotFound)
    );
    // The device never saw any of it.
    assert!(server.audio().endpoints.iter().all(|e| !e.configured()));
}

#[test]
fn attach_refuses_a_ring_the_devices_own_grant_does_not_admit() {
    let mut server = AudioChannelServer::new(MockAudio::new());
    assert!(tairix_abi::driver::audio_channel::decode_configure_reply(
        &server.configure_reply(&params(SINK))
    )
    .is_ok());

    for bad in [
        MOCK_PERIOD / 2,   // smaller than one period
        MOCK_MAX_RING * 2, // past the device's ceiling
        RING_FRAMES + 1,   // not a power of two
    ] {
        assert_eq!(
            decode_status_reply(&server.attach(&attach_params(SINK, bad))),
            Err(Errno::OutOfRange),
            "ring of {bad} frames must be refused"
        );
        assert!(!server.is_attached(SINK), "a refused attach never binds");
    }

    assert_eq!(
        decode_status_reply(&server.attach(&attach_params(SINK, RING_FRAMES))),
        Ok(())
    );
    assert!(server.is_attached(SINK));
    assert_eq!(server.geometry(SINK).map(|g| g.frames()), Some(RING_FRAMES));
    assert_eq!(server.notify_endpoint(SINK), Some(0xACE0));
}

#[test]
fn attaching_before_configuring_is_refused() {
    let mut server = AudioChannelServer::new(MockAudio::new());
    assert_eq!(
        decode_status_reply(&server.attach(&attach_params(SINK, RING_FRAMES))),
        Err(Errno::NotConnected)
    );
}

#[test]
fn a_reconfiguration_drops_the_attached_region_rather_than_leaving_it_the_wrong_shape() {
    let mut server = AudioChannelServer::new(MockAudio::new());
    ready(&mut server, SINK);
    assert!(server.is_attached(SINK));

    assert!(tairix_abi::driver::audio_channel::decode_configure_reply(
        &server.configure_reply(&params(SINK))
    )
    .is_ok());
    assert!(
        !server.is_attached(SINK),
        "the region's size follows the grant, so a re-grant invalidates it"
    );
    let mut region = Region::new();
    assert_eq!(
        server.service(SINK, region.view()),
        Err(Errno::NotConnected)
    );
}

#[test]
fn a_grant_no_power_of_two_ring_could_hold_is_refused_as_a_device_fault() {
    struct ImpossibleGrant;
    impl Audio for ImpossibleGrant {
        fn device_facts(&self) -> Result<AudioDeviceFacts, DriverError> {
            Err(DriverError::NotImplemented)
        }
        fn endpoint_facts(&self, _: u16) -> Result<AudioEndpointFacts, DriverError> {
            Err(DriverError::NotImplemented)
        }
        fn configure(
            &mut self,
            _: u16,
            _: &ConfigureParams,
        ) -> Result<ConfigureGrant, DriverError> {
            // 1000 rounds up to a 1024-frame ring, which its own ceiling of
            // 1023 cannot hold: no attach could ever succeed.
            Ok(ConfigureGrant {
                rate: Rate::HZ_48000,
                format: SampleFormat::S16,
                channel_map: ChannelMap::STEREO,
                period_frames: 1_000,
                max_ring_frames: 1_023,
            })
        }
        fn start(&mut self, _: u16, _: Frames) -> Result<(), DriverError> {
            Err(DriverError::NotImplemented)
        }
        fn stop(&mut self, _: u16, _: Frames) -> Result<(), DriverError> {
            Err(DriverError::NotImplemented)
        }
        fn drain(&mut self, _: u16) -> Result<(), DriverError> {
            Err(DriverError::NotImplemented)
        }
        fn service(&mut self, _: u16, _: &mut PcmRing<'_>) -> Result<AudioServiced, DriverError> {
            Err(DriverError::NotImplemented)
        }
        fn set_gain(&mut self, _: u16, _: i32, _: bool) -> Result<(), DriverError> {
            Err(DriverError::NotImplemented)
        }
        fn release(&mut self, _: u16) -> Result<(), DriverError> {
            Ok(())
        }
        fn take_interrupt(&mut self) -> Result<AudioInterrupt, DriverError> {
            Ok(AudioInterrupt::NONE)
        }
        fn set_event_interrupts(&mut self, _: bool) -> Result<(), DriverError> {
            Ok(())
        }
    }

    let mut server = AudioChannelServer::new(ImpossibleGrant);
    assert_eq!(
        tairix_abi::driver::audio_channel::decode_configure_reply(
            &server.configure_reply(&params(SINK))
        ),
        Err(Errno::DeviceFault)
    );
    assert!(!server.is_attached(SINK));
}

#[test]
fn a_serviced_period_reports_what_moved_and_the_loss_since_the_last_one() {
    let mut server = AudioChannelServer::new(MockAudio::new());
    ready(&mut server, SOURCE);
    assert_eq!(
        decode_status_reply(&server.start(SOURCE, Frames::ZERO)),
        Ok(())
    );

    let mut region = Region::new();
    server.audio_mut().transfer = 128;
    let first = server.service(SOURCE, region.view()).expect("serviced");
    assert_eq!(first.report.transferred, 128);
    assert!(first.report.running);
    assert_eq!(first.lost_frames, 0);

    // A loss is reported once, as the delta: the wire report carries the
    // running total, the notify carries what just happened.
    server.audio_mut().add_xrun = 64;
    let second = server.service(SOURCE, region.view()).expect("serviced");
    assert_eq!(second.report.xrun_frames, 64);
    assert_eq!(second.lost_frames, 64);

    server.audio_mut().add_xrun = 0;
    let third = server.service(SOURCE, region.view()).expect("serviced");
    assert_eq!(third.report.xrun_frames, 64);
    assert_eq!(third.lost_frames, 0, "the same loss is not reported twice");
}

#[test]
fn a_service_over_a_region_that_is_not_the_agreed_shape_is_refused() {
    let mut server = AudioChannelServer::new(MockAudio::new());
    ready(&mut server, SINK);

    let mut short = [0u8; 64];
    assert_eq!(server.service(SINK, &mut short), Err(Errno::BufferTooSmall));
}

#[test]
fn gain_is_device_state_and_is_accepted_before_a_region_exists() {
    let mut server = AudioChannelServer::new(MockAudio::new());
    assert_eq!(
        decode_status_reply(&server.set_gain(SINK, -1_200, true)),
        Ok(())
    );
    assert_eq!(
        server.audio().slot(SINK).expect("present").gain_millibel,
        -1_200
    );
    assert!(server.audio().slot(SINK).expect("present").muted);

    // A device with no control refuses, which is how the mixer learns to
    // apply the gain itself rather than believing the hardware did.
    server.audio_mut().has_gain = false;
    assert_eq!(
        decode_status_reply(&server.set_gain(SINK, -600, false)),
        Err(Errno::NotImplemented)
    );
}

#[test]
fn detach_releases_the_device_and_returns_the_endpoint_to_unconfigured() {
    let mut server = AudioChannelServer::new(MockAudio::new());
    ready(&mut server, SINK);
    ready(&mut server, SOURCE);
    assert!(server.any_attached());

    assert_eq!(decode_status_reply(&server.detach(SINK)), Ok(()));
    assert_eq!(server.audio().slot(SINK).expect("present").released, 1);
    assert!(!server.audio().slot(SINK).expect("present").configured());
    assert!(!server.is_attached(SINK));
    assert_eq!(server.notify_endpoint(SINK), None);

    // The sibling endpoint is untouched: state is per endpoint, not per
    // channel.
    assert!(server.is_attached(SOURCE));
    assert!(server.any_attached());
    assert_eq!(server.audio().slot(SOURCE).expect("present").released, 0);

    assert_eq!(decode_status_reply(&server.detach(SOURCE)), Ok(()));
    assert!(!server.any_attached());
}

#[test]
fn a_release_the_hardware_refused_still_forgets_the_channel_state() {
    let mut server = AudioChannelServer::new(MockAudio::new());
    ready(&mut server, SINK);
    server.audio_mut().fault = Some(DriverError::DeviceFault);

    assert_eq!(
        decode_status_reply(&server.detach(SINK)),
        Err(Errno::DeviceFault)
    );
    // The process is about to unmap the region, so keeping the state would
    // leave a later `Service` binding a mapping that no longer exists.
    assert!(!server.is_attached(SINK));
    assert!(!server.any_attached());
}

#[test]
fn a_device_fault_reaches_the_caller_as_a_typed_refusal_rather_than_a_panic() {
    let mut server = AudioChannelServer::new(MockAudio::new());
    ready(&mut server, SINK);
    server.audio_mut().fault = Some(DriverError::DeviceFault);

    assert_eq!(
        tairix_abi::driver::audio_channel::decode_facts_reply(&server.facts_reply()),
        Err(Errno::DeviceFault)
    );
    assert_eq!(
        decode_status_reply(&server.start(SINK, Frames::ZERO)),
        Err(Errno::DeviceFault)
    );
    let mut region = Region::new();
    assert_eq!(server.service(SINK, region.view()), Err(Errno::DeviceFault));
}

#[test]
fn the_service_reply_carries_the_running_total_the_wire_contract_states() {
    let mut server = AudioChannelServer::new(MockAudio::new());
    ready(&mut server, SINK);
    assert_eq!(
        decode_status_reply(&server.start(SINK, Frames::new(9))),
        Ok(())
    );

    let mut region = Region::new();
    server.audio_mut().add_xrun = 5;
    let reply = server.service_reply(SINK, region.view());
    let report = tairix_abi::driver::audio_channel::decode_service_reply(&reply).expect("decodes");
    assert_eq!(report.xrun_frames, 5);
    assert_eq!(report.position, Frames::new(9));
    assert!(report.running);
    assert_eq!(report.sampled_at, Time64::from_secs(7));
}
