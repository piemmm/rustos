//! Host tests of the audio service: the whole authority and the whole period
//! path, driven against an in-process device (`plans/SOUND.md` SND4).
//!
//! The doubles are the three I/O seams and nothing else — regions are owned
//! `Vec`s, the device-channel transport dispatches into a real
//! `tairix_audiochan::AudioChannelServer` over a recording device engine, and
//! the notifier keeps the frames it was handed. Everything between is the
//! production code, so a test that plays a signal genuinely drives the
//! router, the matrix, the volume model, the mixer and the rings.

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use tairix_abi::audio::{
    decode_enumerate_reply, decode_open_reply, decode_state_reply, AudioRequest, OpenParams,
    StreamGrant, StreamRole, StreamState, AUDIO_MAX_REPLY, AUDIO_MAX_REQUEST,
};
use tairix_abi::driver::audio::{
    Audio, AudioDeviceFacts, AudioEndpointFacts, AudioInterrupt, AudioName, AudioServiced,
    ChannelMap, Frames, JackState, Rate, RateSet, RateSupport, SampleFormat, SampleFormats,
    StreamDirection,
};
use tairix_abi::driver::audio_channel::{ConfigureGrant, ConfigureParams};
use tairix_abi::driver::audio_ring::{aligned_region, PcmGeometry, PcmRing, REGION_ALIGN_PADDING};
use tairix_abi::origin::{CapabilitySummary, Origin, ProcId, TrustDomain};
use tairix_abi::reply::decode_status_reply;
use tairix_abi::time::{MonotonicClock, Time64};
use tairix_abi::{CapabilityId, DriverError, Errno};
use tairix_audiochan::AudioChannelServer;
use tairix_log::DiscardSink;

use crate::{AudioChannelTransport, AudioService, Caller, Notifier, RegionHost, RegionId};

/// The rate the fixture device is clocked at.
const DEVICE_HZ: u32 = 48_000;
/// Frames the fixture device interrupts on.
const PERIOD: u32 = 256;
/// Frames the fixture device will hold in flight.
const MAX_RING: u32 = 4_096;

fn rate(hz: u32) -> Rate {
    Rate::new(hz).expect("a rate inside the vocabulary")
}

// ---------------------------------------------------------------- doubles

/// A recording in-process audio device: it accepts one stereo `s16` sink and
/// one stereo `s16` source, and keeps every frame it was handed.
struct FixtureDevice {
    configured: Option<ConfigureGrant>,
    running: bool,
    position: Frames,
    played: Vec<u8>,
    /// Frames a capture endpoint hands back, drawn from here in order.
    captured: Vec<u8>,
    xrun_frames: u64,
    now: Time64,
    /// Whether the sink endpoint refuses every rate but the device's own, so
    /// a test can force the resampled path.
    fixed_rate: bool,
}

impl FixtureDevice {
    fn new() -> Self {
        Self {
            configured: None,
            running: false,
            position: Frames::ZERO,
            played: Vec::new(),
            captured: Vec::new(),
            xrun_frames: 0,
            now: Time64::from_secs(1),
            fixed_rate: true,
        }
    }
}

impl Audio for FixtureDevice {
    fn device_facts(&self) -> Result<AudioDeviceFacts, DriverError> {
        Ok(AudioDeviceFacts {
            endpoints: 2,
            name: AudioName::new("fixture").expect("a short name"),
        })
    }

    fn endpoint_facts(&self, endpoint: u16) -> Result<AudioEndpointFacts, DriverError> {
        let direction = match endpoint {
            0 => StreamDirection::Playback,
            1 => StreamDirection::Capture,
            _ => return Err(DriverError::NotFound),
        };
        let rates = if self.fixed_rate {
            RateSupport::Discrete(
                RateSet::new(&[rate(DEVICE_HZ)]).map_err(|_| DriverError::DeviceFault)?,
            )
        } else {
            RateSupport::Continuous {
                min: rate(8_000),
                max: rate(DEVICE_HZ),
            }
        };
        Ok(AudioEndpointFacts {
            index: endpoint,
            direction,
            jack: JackState::Unknown,
            formats: SampleFormats::EMPTY.with(SampleFormat::S16),
            channel_map: ChannelMap::STEREO,
            rates,
            min_period_frames: PERIOD,
            max_period_frames: PERIOD,
            max_ring_frames: MAX_RING,
            gain: None,
            name: AudioName::new("fixture").expect("a short name"),
        })
    }

    fn configure(
        &mut self,
        endpoint: u16,
        params: &ConfigureParams,
    ) -> Result<ConfigureGrant, DriverError> {
        let facts = self.endpoint_facts(endpoint)?;
        let grant = ConfigureGrant {
            rate: facts.rates.nearest(params.rate),
            format: SampleFormat::S16,
            channel_map: ChannelMap::STEREO,
            period_frames: PERIOD,
            max_ring_frames: MAX_RING,
        };
        grant.validate().map_err(|_| DriverError::DeviceFault)?;
        self.configured = Some(grant);
        Ok(grant)
    }

    fn start(&mut self, _endpoint: u16, at: Frames) -> Result<(), DriverError> {
        self.running = true;
        self.position = at;
        Ok(())
    }

    fn stop(&mut self, _endpoint: u16, at: Frames) -> Result<(), DriverError> {
        self.running = false;
        self.position = at;
        Ok(())
    }

    fn drain(&mut self, _endpoint: u16) -> Result<(), DriverError> {
        self.running = false;
        Ok(())
    }

    fn service(
        &mut self,
        endpoint: u16,
        ring: &mut PcmRing<'_>,
    ) -> Result<AudioServiced, DriverError> {
        let frame_bytes = ring.geometry().frame_bytes();
        let transferred = if endpoint == 0 {
            let queued = ring.readable_frames().map_err(|_| DriverError::BadMagic)?;
            let mut block = vec![0u8; queued as usize * frame_bytes];
            let taken = ring.read(&mut block).map_err(|_| DriverError::BadMagic)?;
            self.played
                .extend_from_slice(&block[..taken as usize * frame_bytes]);
            taken
        } else {
            let room = ring.writable_frames().map_err(|_| DriverError::BadMagic)?;
            let offer = (room as usize * frame_bytes).min(self.captured.len());
            let written = ring
                .write(&self.captured[..offer - offer % frame_bytes])
                .map_err(|_| DriverError::BadMagic)?;
            self.captured.drain(..written as usize * frame_bytes);
            written
        };
        self.position = Frames::new(self.position.get() + u64::from(transferred));
        self.now =
            Time64::new(self.now.secs(), self.now.subsec_nanos() + 1_000_000).unwrap_or(self.now);
        Ok(AudioServiced {
            transferred,
            running: self.running,
            position: self.position,
            xrun_frames: self.xrun_frames,
            sampled_at: self.now,
        })
    }

    fn set_gain(&mut self, _endpoint: u16, _millibel: i32, _mute: bool) -> Result<(), DriverError> {
        Err(DriverError::NotImplemented)
    }

    fn release(&mut self, _endpoint: u16) -> Result<(), DriverError> {
        self.configured = None;
        self.running = false;
        Ok(())
    }

    fn take_interrupt(&mut self) -> Result<AudioInterrupt, DriverError> {
        Ok(AudioInterrupt::NONE)
    }

    fn set_event_interrupts(&mut self, _enabled: bool) -> Result<(), DriverError> {
        Ok(())
    }
}

/// The device-channel transport: dispatches straight into the real driver
/// side of the contract over the fixture device.
struct LoopbackChannel {
    server: Rc<RefCell<AudioChannelServer<FixtureDevice>>>,
    regions: Rc<RefCell<FixtureRegions>>,
}

impl AudioChannelTransport for LoopbackChannel {
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
        use tairix_abi::driver::audio_channel::AudioChannelRequest as Req;
        let decoded = Req::decode(request)?;
        let mut server = self.server.borrow_mut();
        let body: Vec<u8> = match decoded {
            Req::Facts => server.facts_reply().to_vec(),
            Req::EndpointFacts { endpoint } => server.endpoint_facts_reply(endpoint).to_vec(),
            Req::Configure(params) => server.configure_reply(&params).to_vec(),
            Req::Attach(params) => {
                // The grant handle is the region id here, because the two
                // halves share one address space; the driver side's
                // "mapping" is the same bytes the service holds, which a
                // `Service` binds its ring over.
                let id = RegionId(u32::try_from(params.region_grant).unwrap_or(0));
                self.regions.borrow_mut().bytes(id)?;
                server.attach(&params).to_vec()
            }
            Req::Start { endpoint, at } => server.start(endpoint, at).to_vec(),
            Req::Stop { endpoint, at } => server.stop(endpoint, at).to_vec(),
            Req::Drain { endpoint } => server.drain(endpoint).to_vec(),
            Req::Service { endpoint } => {
                let mut regions = self.regions.borrow_mut();
                let id = regions.device_region.ok_or(Errno::NotAttached)?;
                let bytes = regions.bytes(id)?;
                server.service_reply(endpoint, bytes).to_vec()
            }
            Req::Gain {
                endpoint,
                millibel,
                mute,
            } => server.set_gain(endpoint, millibel, mute).to_vec(),
            Req::Detach { endpoint } => server.detach(endpoint).to_vec(),
        };
        let len = body.len().min(reply.len());
        reply[..len].copy_from_slice(&body[..len]);
        Ok(len)
    }
}

/// In-process shared regions: owned buffers, with the grant handle being the
/// region id, since both halves live in one address space here.
struct FixtureRegions {
    /// Each region's id, its backing store, and the aligned `offset..offset +
    /// len` view that *is* the region — exactly the geometry both halves
    /// agreed, since a ring binds nothing larger.
    buffers: Vec<(RegionId, Vec<u8>, usize, usize)>,
    next: u32,
    /// The device ring the service created, which the loopback channel's
    /// `Service` binds over.
    device_region: Option<RegionId>,
}

impl FixtureRegions {
    fn new() -> Self {
        Self {
            buffers: Vec::new(),
            next: 1,
            device_region: None,
        }
    }

    fn slot(&self, region: RegionId) -> Option<usize> {
        self.buffers.iter().position(|(id, ..)| *id == region)
    }

    /// Add a buffer of `len` usable bytes, aligned for the ring header.
    fn add(&mut self, len: usize) -> RegionId {
        let id = RegionId(self.next);
        self.next += 1;
        let mut backing = vec![0u8; len + REGION_ALIGN_PADDING];
        let base = backing.as_ptr() as usize;
        let offset = aligned_region(&mut backing, len)
            .map(|view| view.as_ptr() as usize)
            .expect("an aligned view fits the over-allocated buffer")
            - base;
        self.buffers.push((id, backing, offset, len));
        id
    }
}

impl RegionHost for FixtureRegions {
    fn create(&mut self, len: usize) -> Result<RegionId, Errno> {
        let id = self.add(len);
        self.device_region = Some(id);
        Ok(id)
    }

    fn adopt(&mut self, grant: u64, len: usize) -> Result<RegionId, Errno> {
        let id = RegionId(u32::try_from(grant).map_err(|_| Errno::NotFound)?);
        let slot = self.slot(id).ok_or(Errno::NotFound)?;
        if self.buffers[slot].3 < len {
            return Err(Errno::BufferTooSmall);
        }
        Ok(id)
    }

    fn grant(&mut self, region: RegionId, _endpoint: u64) -> Result<u64, Errno> {
        self.slot(region).ok_or(Errno::NotFound)?;
        Ok(u64::from(region.0))
    }

    fn bytes(&mut self, region: RegionId) -> Result<&mut [u8], Errno> {
        let slot = self.slot(region).ok_or(Errno::NotFound)?;
        let (_, backing, offset, len) = &mut self.buffers[slot];
        let (offset, len) = (*offset, *len);
        Ok(&mut backing[offset..offset + len])
    }

    fn release(&mut self, _region: RegionId) {
        // The fixture keeps every buffer so a test can read what was played
        // after the stream that produced it has gone.
    }
}

/// A notifier that keeps every frame it was handed.
#[derive(Default)]
struct Recorder {
    frames: Vec<(u64, Vec<u8>)>,
}

impl Notifier for Recorder {
    fn notify(&mut self, endpoint: u64, frame: &[u8]) {
        self.frames.push((endpoint, frame.to_vec()));
    }
}

/// A monotonic clock that advances a millisecond per reading.
struct TickClock {
    now: RefCell<u64>,
}

impl MonotonicClock for TickClock {
    fn now_ns(&self) -> u64 {
        let mut now = self.now.borrow_mut();
        *now += 1_000_000;
        *now
    }
}

/// The whole fixture: the service plus the halves a test reaches into.
struct Fixture {
    service: AudioService<SharedRegions, Recorder, TickClock>,
    regions: Rc<RefCell<FixtureRegions>>,
    device: Rc<RefCell<AudioChannelServer<FixtureDevice>>>,
}

/// The region host the service holds: a handle onto the shared fixture
/// buffers, so the loopback channel can bind the same bytes.
struct SharedRegions(Rc<RefCell<FixtureRegions>>);

impl RegionHost for SharedRegions {
    fn create(&mut self, len: usize) -> Result<RegionId, Errno> {
        self.0.borrow_mut().create(len)
    }

    fn adopt(&mut self, grant: u64, len: usize) -> Result<RegionId, Errno> {
        self.0.borrow_mut().adopt(grant, len)
    }

    fn grant(&mut self, region: RegionId, endpoint: u64) -> Result<u64, Errno> {
        self.0.borrow_mut().grant(region, endpoint)
    }

    fn bytes(&mut self, region: RegionId) -> Result<&mut [u8], Errno> {
        let mut regions = self.0.borrow_mut();
        let bytes = regions.bytes(region)?;
        let (ptr, len) = (bytes.as_mut_ptr(), bytes.len());
        // SAFETY: `ptr`/`len` name a live subrange of one region's own heap
        // buffer. That buffer outlives the fixture — `FixtureRegions` never
        // drops or reallocates it (`release` is a no-op, and pushing to
        // `buffers` moves only the `Vec` headers, never the allocations they
        // point at) — so the pointer stays valid for the whole test. The
        // engine borrows one region at a time and never holds a previous
        // slice across a later `bytes` call, so no second live `&mut` to
        // these bytes exists. This is the single-address-space stand-in for
        // the two independent `shm` mappings the live service and driver
        // hold of one region, which the `RefCell` borrow alone cannot
        // express because the lifetime would not escape it.
        Ok(unsafe { core::slice::from_raw_parts_mut(ptr, len) })
    }

    fn release(&mut self, region: RegionId) {
        self.0.borrow_mut().release(region);
    }
}

impl Fixture {
    fn new() -> Self {
        let regions = Rc::new(RefCell::new(FixtureRegions::new()));
        let device = Rc::new(RefCell::new(AudioChannelServer::new(FixtureDevice::new())));
        let service = AudioService::new(
            SharedRegions(Rc::clone(&regions)),
            Recorder::default(),
            TickClock {
                now: RefCell::new(0),
            },
        );
        Self {
            service,
            regions,
            device,
        }
    }

    /// Bind the fixture's one device channel.
    fn bind(&mut self) {
        let transport = LoopbackChannel {
            server: Rc::clone(&self.device),
            regions: Rc::clone(&self.regions),
        };
        self.service
            .bind_device(
                0x4143_4841_4E00_0000,
                0x0A55_0001,
                Box::new(transport),
                &DiscardSink,
            )
            .expect("the fixture channel binds");
    }

    /// Serve one request and answer the reply bytes.
    fn call(&mut self, caller: &Caller, request: &AudioRequest) -> Vec<u8> {
        let mut frame = [0u8; AUDIO_MAX_REQUEST];
        let len = request.encode(&mut frame).expect("encoded");
        let mut reply = [0u8; AUDIO_MAX_REPLY];
        let reply_len = self
            .service
            .handle(caller, &frame[..len], &mut reply, &DiscardSink);
        reply[..reply_len].to_vec()
    }

    /// Open a playback stream and adopt a ring for it.
    fn open_playback(&mut self, caller: &Caller, hz: u32, latency: u32) -> StreamGrant {
        let reply = self.call(
            caller,
            &AudioRequest::Open(OpenParams {
                device_id: 0,
                direction: StreamDirection::Playback,
                format: SampleFormat::S16,
                rate: rate(hz),
                channel_map: ChannelMap::STEREO,
                role: StreamRole::Media,
                latency_target_frames: latency,
            }),
        );
        let grant = decode_open_reply(&reply).expect("the open is granted");
        let geometry = PcmGeometry::new(grant.ring_frames, grant.format, 2).expect("geometry");
        let region = self.regions.borrow_mut().add(geometry.region_len());
        let reply = self.call(
            caller,
            &AudioRequest::Attach {
                stream_id: grant.stream_id,
                region_grant: u64::from(region.0),
            },
        );
        decode_status_reply(&reply).expect("the ring is adopted");
        grant
    }

    /// Write `samples` into the stream's ring, which the client owns.
    fn client_write(&mut self, grant: &StreamGrant, samples: &[u8]) -> u32 {
        let geometry = PcmGeometry::new(grant.ring_frames, grant.format, 2).expect("geometry");
        let mut regions = self.regions.borrow_mut();
        // The client's ring is the last one the fixture added for it.
        let id = regions
            .buffers
            .iter()
            .rev()
            .map(|(id, ..)| *id)
            .find(|id| Some(*id) != regions.device_region)
            .expect("a client ring");
        let bytes = regions.bytes(id).expect("mapped");
        let mut ring = PcmRing::bind(bytes, geometry).expect("ring");
        ring.write(samples).expect("written")
    }

    /// Run one device period exactly as the driver's interrupt path does:
    /// the driver services its own ring and *then* wakes the mixer with the
    /// clock pair, which is why a period notify means "refill", not "move
    /// frames for me".
    fn period(&mut self) {
        let serviced = {
            let mut regions = self.regions.borrow_mut();
            let id = regions.device_region.expect("a device ring");
            let bytes = regions.bytes(id).expect("mapped");
            self.device.borrow_mut().service(0, bytes)
        };
        let Ok(serviced) = serviced else { return };
        let frame = tairix_abi::driver::audio_channel::AudioChannelNotify::PeriodElapsed {
            endpoint: 0,
            position: serviced.report.position,
            sampled_at: serviced.report.sampled_at,
        }
        .encode();
        self.service.on_device_notify(0, &frame, &DiscardSink);
    }

    /// What the device was handed.
    fn played(&self) -> Vec<u8> {
        self.device.borrow().audio().played.clone()
    }
}

/// A caller with the given pid and capability set.
fn caller(pid: u64, caps: &[CapabilityId]) -> Caller {
    let mut summary = CapabilitySummary::EMPTY;
    for cap in caps {
        summary.insert(*cap);
    }
    Caller {
        origin: Origin::new(
            TrustDomain::User,
            1_000,
            1_000,
            pid,
            ProcId::from_raw([0u8; tairix_abi::origin::PROC_ID_LEN]),
            summary,
            0,
        ),
        seat: None,
    }
}

/// A deterministic stereo `s16` signal of `frames` frames.
fn signal(frames: usize) -> Vec<u8> {
    let mut samples = Vec::with_capacity(frames * 4);
    for frame in 0..frames {
        let left = i16::try_from(i32::try_from(frame % 2_000).unwrap_or(0) - 1_000).unwrap_or(0);
        let right = left.wrapping_neg();
        samples.extend_from_slice(&left.to_le_bytes());
        samples.extend_from_slice(&right.to_le_bytes());
    }
    samples
}

// ------------------------------------------------------------------ tests

#[test]
fn a_bound_channel_enumerates_its_sink_and_its_source() {
    let mut fixture = Fixture::new();
    fixture.bind();
    let caller = caller(7, &[]);
    let reply = fixture.call(
        &caller,
        &AudioRequest::Enumerate {
            direction: StreamDirection::Playback,
            index: 0,
        },
    );
    let sink = decode_enumerate_reply(&reply).expect("a sink");
    assert_eq!(sink.direction, StreamDirection::Playback);
    assert!(sink.is_default, "the only sink is the machine's default");
    let reply = fixture.call(
        &caller,
        &AudioRequest::Enumerate {
            direction: StreamDirection::Playback,
            index: 1,
        },
    );
    assert_eq!(decode_enumerate_reply(&reply), Err(Errno::NotFound));
    let reply = fixture.call(
        &caller,
        &AudioRequest::Enumerate {
            direction: StreamDirection::Capture,
            index: 0,
        },
    );
    let source = decode_enumerate_reply(&reply).expect("a source");
    assert_ne!(source.device_id, sink.device_id);
}

#[test]
fn a_repeated_hand_off_of_one_channel_is_not_a_second_device() {
    let mut fixture = Fixture::new();
    fixture.bind();
    fixture.bind();
    assert_eq!(fixture.service.device_count(), 1);
}

#[test]
fn a_single_stream_at_unity_reaches_the_device_bit_exact() {
    let mut fixture = Fixture::new();
    fixture.bind();
    let caller = caller(7, &[]);
    let grant = fixture.open_playback(&caller, DEVICE_HZ, 2_048);
    assert_eq!(grant.rate, rate(DEVICE_HZ));
    assert_eq!(grant.format, SampleFormat::S16);
    assert_eq!(grant.ring_frames, 2_048);

    let samples = signal(1_500);
    assert_eq!(fixture.client_write(&grant, &samples), 1_500);
    let reply = fixture.call(
        &caller,
        &AudioRequest::Start {
            stream_id: grant.stream_id,
            at: Frames::ZERO,
        },
    );
    decode_status_reply(&reply).expect("started");
    let reply = fixture.call(
        &caller,
        &AudioRequest::Drain {
            stream_id: grant.stream_id,
        },
    );
    decode_status_reply(&reply).expect("draining");
    // Each period the device consumes wakes the service, which refills.
    for _ in 0..16 {
        fixture.period();
    }
    let played = fixture.played();
    assert_eq!(
        played.len(),
        samples.len(),
        "the drain's tail is the frames the stream had, never a padded period"
    );
    assert!(
        played == samples,
        "a stereo s16 source at the device's own rate and unity gain must \
         reach the hardware byte for byte"
    );
    let reply = fixture.call(
        &caller,
        &AudioRequest::State {
            stream_id: grant.stream_id,
        },
    );
    let report = decode_state_reply(&reply).expect("a report");
    assert_eq!(report.state, StreamState::Idle, "the drain completed");
    assert_eq!(report.xrun_frames, 0, "nothing was lost");
}

#[test]
fn a_running_stream_with_nothing_queued_takes_the_underrun() {
    let mut fixture = Fixture::new();
    fixture.bind();
    let caller = caller(7, &[]);
    let grant = fixture.open_playback(&caller, DEVICE_HZ, 2_048);
    fixture.client_write(&grant, &signal(100));
    let reply = fixture.call(
        &caller,
        &AudioRequest::Start {
            stream_id: grant.stream_id,
            at: Frames::ZERO,
        },
    );
    decode_status_reply(&reply).expect("started");
    fixture.period();
    let reply = fixture.call(
        &caller,
        &AudioRequest::State {
            stream_id: grant.stream_id,
        },
    );
    let report = decode_state_reply(&reply).expect("a report");
    assert!(
        report.xrun_frames >= u64::from(PERIOD) - 100,
        "the frames the stream could not supply are accounted, not hidden: \
         {report:?}"
    );
    assert!(report.xruns >= 1);
}

#[test]
fn a_rate_the_device_cannot_meet_is_filtered_rather_than_refused() {
    let mut fixture = Fixture::new();
    fixture.bind();
    let caller = caller(7, &[]);
    // The fixture sink admits 48 kHz alone, so a 24 kHz client is resampled.
    let grant = fixture.open_playback(&caller, 24_000, 2_048);
    assert_eq!(grant.rate, rate(24_000), "the client keeps its own rate");
    fixture.client_write(&grant, &signal(1_024));
    let reply = fixture.call(
        &caller,
        &AudioRequest::Start {
            stream_id: grant.stream_id,
            at: Frames::ZERO,
        },
    );
    decode_status_reply(&reply).expect("started");
    for _ in 0..16 {
        fixture.period();
    }
    let played = fixture.played().len() / 4;
    assert!(
        played >= 1_800,
        "1024 frames at 24 kHz should reach the 48 kHz device as about 2048 \
         frames, not {played}"
    );
}

#[test]
fn a_capture_open_is_refused_without_the_capability_and_granted_with_it() {
    let mut fixture = Fixture::new();
    fixture.bind();
    let params = OpenParams {
        device_id: 0,
        direction: StreamDirection::Capture,
        format: SampleFormat::S16,
        rate: rate(DEVICE_HZ),
        channel_map: ChannelMap::STEREO,
        role: StreamRole::Media,
        latency_target_frames: 2_048,
    };
    let plain = caller(7, &[]);
    let reply = fixture.call(&plain, &AudioRequest::Open(params));
    assert_eq!(decode_open_reply(&reply), Err(Errno::PermissionDenied));

    let recorder = caller(8, &[CapabilityId::AUDIO_CAPTURE]);
    let reply = fixture.call(&recorder, &AudioRequest::Open(params));
    let grant = decode_open_reply(&reply).expect("the capture open is granted");
    assert_eq!(grant.rate, rate(DEVICE_HZ));
}

#[test]
fn a_stream_that_is_not_the_callers_is_unreachable() {
    let mut fixture = Fixture::new();
    fixture.bind();
    let owner = caller(7, &[]);
    let grant = fixture.open_playback(&owner, DEVICE_HZ, 2_048);
    let stranger = caller(9, &[]);
    for request in [
        AudioRequest::Start {
            stream_id: grant.stream_id,
            at: Frames::ZERO,
        },
        AudioRequest::Close {
            stream_id: grant.stream_id,
        },
        AudioRequest::State {
            stream_id: grant.stream_id,
        },
    ] {
        let reply = fixture.call(&stranger, &request);
        assert_eq!(
            decode_status_reply(&reply),
            Err(Errno::NotFound),
            "a guessed stream id must reach nothing"
        );
    }
}

#[test]
fn the_driver_bind_operation_is_not_reachable_through_the_client_surface() {
    let mut fixture = Fixture::new();
    fixture.bind();
    let caller = caller(7, &[]);
    let reply = fixture.call(
        &caller,
        &AudioRequest::BindDriver {
            endpoint_id: 0x4143_4841_4E00_0001,
        },
    );
    assert_eq!(decode_status_reply(&reply), Err(Errno::NotSupported));
    assert_eq!(fixture.service.device_count(), 1);
}

#[test]
fn a_latency_target_earns_a_power_of_two_ring_that_holds_it() {
    let mut fixture = Fixture::new();
    fixture.bind();
    let caller = caller(7, &[]);
    // A target that is not a power of two rounds up; the grant reports both
    // the ring and the latency it bought.
    let grant = fixture.open_playback(&caller, DEVICE_HZ, 3_000);
    assert_eq!(grant.ring_frames, 4_096);
    // One device period of the ring is the mixer's own contribution and the
    // rest is the client's queue, so the granted latency is the whole ring.
    assert_eq!(grant.granted_latency_frames, 4_096);
    assert_eq!(
        grant.granted_latency,
        Frames::new(4_096).duration(rate(DEVICE_HZ))
    );
}

#[test]
fn a_clock_read_before_the_first_period_refuses_rather_than_inventing_one() {
    let mut fixture = Fixture::new();
    fixture.bind();
    let caller = caller(7, &[]);
    let grant = fixture.open_playback(&caller, DEVICE_HZ, 2_048);
    let reply = fixture.call(
        &caller,
        &AudioRequest::Clock {
            stream_id: grant.stream_id,
        },
    );
    assert_eq!(
        tairix_abi::audio::decode_clock_reply(&reply),
        Err(Errno::WouldBlock)
    );
}

#[test]
fn closing_the_last_stream_releases_the_endpoint() {
    let mut fixture = Fixture::new();
    fixture.bind();
    let caller = caller(7, &[]);
    let grant = fixture.open_playback(&caller, DEVICE_HZ, 2_048);
    assert_eq!(fixture.service.stream_count(), 1);
    let reply = fixture.call(
        &caller,
        &AudioRequest::Close {
            stream_id: grant.stream_id,
        },
    );
    decode_status_reply(&reply).expect("closed");
    assert_eq!(fixture.service.stream_count(), 0);
    assert!(
        fixture.device.borrow().audio().configured.is_none(),
        "the device is released with its last stream"
    );
}
