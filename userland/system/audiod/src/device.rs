//! The bound devices, their configured endpoints, the live streams, and the
//! period pump that moves frames between them (`plans/SOUND.md`).
//!
//! # What a period costs
//!
//! Everything a period touches is allocated when a stream opens or an
//! endpoint is configured, and reused: the client-format scratch a stream's
//! frames are read into, its pivot and resampled buffers, the endpoint's one
//! device period, and the mixer's own working set. The pump borrows one
//! shared region at a time — each client ring in turn, then the device ring —
//! which is the discipline [`RegionHost::bytes`] states in its signature, and
//! it folds the live streams into the mixer as an iterator so there is no
//! per-period collection to build either.
//!
//! # Silence is accounted, never hidden
//!
//! A running stream with nothing queued contributes silence for exactly the
//! frames it missed and takes the underrun; the device would otherwise run
//! dry, which is strictly worse and is what its driver would report instead.
//! A *draining* stream is different: the pump emits only the frames it
//! genuinely has, so a drain's tail is a short chunk rather than a padded
//! one, and the drain is then passed down to the device.

use alloc::boxed::Box;
use alloc::vec::Vec;

use tairix_abi::audio::{AudioNotify, StreamGrant, StreamRole, StreamState, AUDIO_NOTIFY_LEN};
use tairix_abi::driver::audio::{AudioEndpointFacts, Frames, Rate, SampleFormat, StreamDirection};
use tairix_abi::driver::audio_channel::{AttachParams, ConfigureGrant, ConfigureParams};
use tairix_abi::driver::audio_ring::{PcmGeometry, PcmRing};
use tairix_abi::Errno;
use tairix_audio::channel::ChannelMatrix;
use tairix_audio::clock::ClockModel;
use tairix_audio::convert;
use tairix_audio::mix::{Mixer, SinkFormat, StreamMix};
use tairix_audio::resample::{FilterBank, Ratio, Resampler};
use tairix_util::fallible;

use crate::channel::{AudioChannelClient, AudioChannelTransport};
use crate::region::{RegionHost, RegionId};
use crate::service::Notifier;

/// The encoding a resampled contribution is staged in for the mixer.
///
/// The resampler answers the pivot and the mixer takes bytes, so a resampled
/// stream is staged in the pivot's own encoding rather than re-quantised back
/// to the client's, which would throw away resolution the filter produced.
const RESAMPLED_FORMAT: SampleFormat = SampleFormat::F32;

/// Device sample encodings an endpoint is run in when the client's own is not
/// among the ones it accepts, widest first.
///
/// Widest first because a wider device encoding never costs the client bits:
/// the pivot carries twenty-four and every scale factor is a power of two, so
/// a sixteen-bit source through a thirty-two-bit device is still exact. The
/// order is fixed, so two identical machines configure identically.
const FORMAT_PREFERENCE: &[SampleFormat] = &[
    SampleFormat::F32,
    SampleFormat::S32,
    SampleFormat::S24In32,
    SampleFormat::S24,
    SampleFormat::S16,
    SampleFormat::U8,
];

/// One driver's device channel, and the endpoints it presents.
pub(crate) struct Device {
    /// The reserved `audiochan-v1` call endpoint `devmgr` handed over — this
    /// channel's identity, so a repeated hand-off is recognised rather than
    /// provisioning a duplicate device.
    pub(crate) channel_endpoint: u64,
    /// The port this service bound and the driver sends its notifies to.
    pub(crate) notify_endpoint: u64,
    /// The control-plane client.
    pub(crate) channel: AudioChannelClient<Box<dyn AudioChannelTransport>>,
    /// Its sinks and sources.
    pub(crate) endpoints: Vec<Endpoint>,
    /// Set once the channel has faulted: its streams hold their positions and
    /// every other device is untouched.
    pub(crate) lost: bool,
}

/// One sink or source of a device, as this service tracks it.
pub(crate) struct Endpoint {
    /// Its index on the device.
    pub(crate) index: u16,
    /// The service-assigned identity a client opens a stream against.
    pub(crate) device_id: u32,
    /// What the endpoint said it can do.
    pub(crate) facts: AudioEndpointFacts,
    /// Present once a stream has caused the endpoint to be programmed.
    pub(crate) active: Option<Active>,
}

/// One phase bank, shared by every stream on this endpoint at one source
/// rate.
///
/// Reference-counted rather than kept for the endpoint's life: the
/// coefficients depend on the rate pair alone, so one bank serves all of that
/// rate's streams, and the last stream leaving takes it with it — otherwise a
/// client naming a fresh rate each time would grow the table without bound.
struct BankSlot {
    rate: Rate,
    bank: FilterBank,
    users: u32,
}

/// A programmed endpoint: the device ring, the mixer, the clock, and the
/// buffers a period reuses.
pub(crate) struct Active {
    /// What the device answered it would run at.
    pub(crate) grant: ConfigureGrant,
    /// The device ring's shape.
    geometry: PcmGeometry,
    /// The device ring this service created and granted to the driver.
    region: RegionId,
    /// The measured device clock, from the `(position, sampled_at)` pairs the
    /// driver reports.
    pub(crate) clock: ClockModel,
    /// This endpoint's mixer, sized for one period.
    mixer: Mixer,
    /// The format the mixer quantises to.
    sink: SinkFormat,
    /// One device period of frames, reused by every chunk.
    period: Vec<u8>,
    /// One device period decoded, for the capture fan-out.
    pivot: Vec<f32>,
    /// The phase banks this endpoint's streams share.
    banks: Vec<Option<BankSlot>>,
    /// Whether the device is clocking.
    pub(crate) running: bool,
    /// The device's own position as last reported.
    pub(crate) position: Frames,
    /// Cumulative frames the device lost, as the driver reports them.
    pub(crate) xrun_frames: u64,
}

impl Active {
    /// Frames the device interrupts on.
    pub(crate) const fn period_frames(&self) -> u32 {
        self.grant.period_frames
    }

    /// Reserve (or find) the phase bank carrying `source` to this endpoint's
    /// rate, and answer its slot.
    fn acquire_bank(&mut self, source: Rate) -> Result<usize, Errno> {
        if let Some(index) = self
            .banks
            .iter()
            .position(|slot| slot.as_ref().is_some_and(|held| held.rate == source))
        {
            if let Some(held) = self.banks[index].as_mut() {
                held.users = held.users.saturating_add(1);
            }
            return Ok(index);
        }
        let bank = FilterBank::new(source, self.grant.rate)?;
        let slot = BankSlot {
            rate: source,
            bank,
            users: 1,
        };
        if let Some(index) = self.banks.iter().position(Option::is_none) {
            self.banks[index] = Some(slot);
            return Ok(index);
        }
        if !fallible::reserve(&mut self.banks, 1) {
            return Err(Errno::OutOfMemory);
        }
        self.banks.push(Some(slot));
        Ok(self.banks.len() - 1)
    }

    /// Drop a stream's hold on bank slot `index`, freeing the coefficients
    /// with the last user.
    fn release_bank(&mut self, index: usize) {
        let Some(slot) = self.banks.get_mut(index) else {
            return;
        };
        let spent = slot.as_mut().is_some_and(|held| {
            held.users = held.users.saturating_sub(1);
            held.users == 0
        });
        if spent {
            *slot = None;
        }
    }
}

/// One live `audio-v1` stream.
pub(crate) struct Stream {
    /// The service-issued token every later request names.
    pub(crate) id: u64,
    /// The caller's kernel-attested pid; a request naming this stream from
    /// any other process is refused.
    pub(crate) owner_pid: u64,
    /// Index of the device it landed on.
    pub(crate) device: usize,
    /// Index of the endpoint within that device.
    pub(crate) endpoint: usize,
    /// Playback or capture.
    pub(crate) direction: StreamDirection,
    /// What the sound is for.
    pub(crate) role: StreamRole,
    /// What the client was told.
    pub(crate) grant: StreamGrant,
    /// The client ring's shape.
    pub(crate) geometry: PcmGeometry,
    /// The client's ring, once attached.
    pub(crate) region: Option<RegionId>,
    /// This stream's layout onto the device's (playback) or the device's onto
    /// this stream's (capture), derived once at open.
    matrix: ChannelMatrix,
    /// Present only where the stream's rate differs from the device's.
    resampler: Option<Resampler>,
    /// Which of the endpoint's shared banks that resampler runs over.
    bank: usize,
    /// Client-encoding frames moved this period.
    bytes: Vec<u8>,
    /// Those frames decoded, in this stream's own layout.
    pivot: Vec<f32>,
    /// Filtered frames awaiting their mix (playback) or their client ring
    /// (capture), in this stream's layout.
    pending: Vec<f32>,
    /// Frames held in `pending`.
    pending_frames: usize,
    /// `pending` staged in the pivot's encoding for the mixer (a resampled
    /// playback stream only).
    staged: Vec<u8>,
    /// Device frames this stream contributes to the chunk being mixed.
    covered: usize,
    /// Client-rate frames pulled per top-up.
    want_in: usize,
    /// This stream's own gain.
    pub(crate) gain_millibel: i32,
    /// Whether it is muted, independently of its gain.
    pub(crate) muted: bool,
    /// The multiply the mix applies, resolved from every gain in force.
    pub(crate) software_gain: f32,
    /// Where it stands.
    pub(crate) state: StreamState,
    /// The position it entered that state at.
    pub(crate) state_at: Frames,
    /// The frame a scheduled stop settles on, where one was named ahead of
    /// the position the stream had reached.
    pub(crate) stop_at: Option<Frames>,
    /// Frames this service has taken from (or given to) the client ring.
    pub(crate) position: Frames,
    /// Frames lost to under- or over-run.
    pub(crate) xrun_frames: u64,
    /// Distinct under- or over-runs.
    pub(crate) xruns: u32,
}

/// What a stream needs from its caller to be opened.
pub(crate) struct StreamOpen {
    /// The service-issued token.
    pub(crate) id: u64,
    /// The caller's kernel-attested pid.
    pub(crate) owner_pid: u64,
    /// Index of the device the router chose.
    pub(crate) device: usize,
    /// Index of the endpoint within it.
    pub(crate) endpoint: usize,
    /// Playback or capture.
    pub(crate) direction: StreamDirection,
    /// What the sound is for.
    pub(crate) role: StreamRole,
    /// What the client is being told, which fixes its rate, format, layout
    /// and ring.
    pub(crate) grant: StreamGrant,
    /// The client ring's shape, derived from that grant.
    pub(crate) geometry: PcmGeometry,
}

impl Stream {
    /// Allocate a stream's whole working set against `active`, so no later
    /// period allocates.
    ///
    /// # Errors
    ///
    /// [`Errno::NotSupported`] for a pair of channel layouts with no defined
    /// relationship, [`Errno::OutOfMemory`] for a buffer that would not fit,
    /// or whatever the resampler's bank refuses.
    pub(crate) fn open(active: &mut Active, params: &StreamOpen) -> Result<Self, Errno> {
        let client_map = params.grant.channel_map;
        let device_map = active.grant.channel_map;
        let matrix = match params.direction {
            StreamDirection::Playback => ChannelMatrix::derive(&client_map, &device_map)?,
            StreamDirection::Capture => ChannelMatrix::derive(&device_map, &client_map)?,
        };
        let channels = usize::from(client_map.channels());
        let frame_bytes = channels * params.grant.format.bytes_per_sample();
        let period = active.period_frames() as usize;
        let (source, sink_rate) = match params.direction {
            StreamDirection::Playback => (params.grant.rate, active.grant.rate),
            StreamDirection::Capture => (active.grant.rate, params.grant.rate),
        };

        let (resampler, bank) = if source == sink_rate {
            (None, usize::MAX)
        } else {
            let bank = active.acquire_bank(source)?;
            let built = active
                .banks
                .get(bank)
                .and_then(Option::as_ref)
                .ok_or(Errno::NotFound)
                .and_then(|slot| Resampler::new(&slot.bank, channels));
            match built {
                Ok(resampler) => (Some(resampler), bank),
                Err(err) => {
                    active.release_bank(bank);
                    return Err(err);
                }
            }
        };

        // Upper bounds, so the pull loop never leaves input unconsumed and
        // never has to grow a buffer: how much input one device period
        // consumes, and how much output that input can produce.
        let ratio = Ratio::between(source, sink_rate);
        let want_in = period
            .saturating_mul(ratio.input() as usize)
            .div_ceil((ratio.output() as usize).max(1))
            .saturating_add(1);
        let produced = resampler
            .as_ref()
            .map_or(want_in, |filter| filter.max_output_frames(want_in));

        let (bytes_frames, pivot_frames, pending_frames, staged_frames) = match params.direction {
            StreamDirection::Playback => (want_in, want_in, period + produced, period + produced),
            StreamDirection::Capture => (produced, period, produced, 0),
        };
        let bytes = |count: usize| fallible::filled(count, 0u8).ok_or(Errno::OutOfMemory);
        let pivot = |count: usize| fallible::filled(count, 0.0f32).ok_or(Errno::OutOfMemory);
        let stream = Self {
            id: params.id,
            owner_pid: params.owner_pid,
            device: params.device,
            endpoint: params.endpoint,
            direction: params.direction,
            role: params.role,
            grant: params.grant,
            geometry: params.geometry,
            region: None,
            matrix,
            resampler,
            bank,
            bytes: bytes(bytes_frames * frame_bytes)?,
            pivot: pivot(pivot_frames * channels)?,
            pending: pivot(pending_frames * channels)?,
            pending_frames: 0,
            staged: bytes(staged_frames * channels * RESAMPLED_FORMAT.bytes_per_sample())?,
            covered: 0,
            want_in,
            gain_millibel: 0,
            muted: false,
            software_gain: 1.0,
            state: StreamState::Idle,
            state_at: Frames::ZERO,
            stop_at: None,
            position: Frames::ZERO,
            xrun_frames: 0,
            xruns: 0,
        };
        Ok(stream)
    }

    /// Whether the stream is moving frames this period.
    ///
    /// A draining stream still is: it is playing out what it queued, and the
    /// difference is only that the pump stops padding for it.
    pub(crate) fn is_live(&self) -> bool {
        matches!(self.state, StreamState::Running | StreamState::Draining) && self.region.is_some()
    }

    /// Whether the stream is playing out what it has and then stopping.
    fn is_draining(&self) -> bool {
        self.state == StreamState::Draining
    }

    /// Frames this stream may still take before a scheduled stop settles it,
    /// or [`None`] when no stop is pending.
    fn stop_limit(&self) -> Option<usize> {
        let at = self.stop_at?;
        Some(usize::try_from(at.since(self.position).unwrap_or(0)).unwrap_or(usize::MAX))
    }

    /// Discard the filter's memory: the frames that were in flight are gone,
    /// so the history they would have coloured goes with them.
    pub(crate) fn reset_filter(&mut self) {
        if let Some(filter) = self.resampler.as_mut() {
            filter.reset();
        }
        self.pending_frames = 0;
        self.covered = 0;
    }

    /// Whether this stream belongs to `endpoint` of `device`.
    pub(crate) fn on(&self, device: usize, endpoint: usize) -> bool {
        self.device == device && self.endpoint == endpoint
    }

    /// Interleaved channels one client frame carries.
    fn channels(&self) -> usize {
        usize::from(self.grant.channel_map.channels())
    }

    /// Move the stream to `state` at `at`, and tell the client the exact
    /// frame it changed on.
    pub(crate) fn set_state(
        &mut self,
        state: StreamState,
        at: Frames,
        notifier: &mut dyn Notifier,
    ) {
        if self.state == state {
            return;
        }
        self.state = state;
        self.state_at = at;
        send(
            notifier,
            self.grant.notify_endpoint,
            AudioNotify::StateChanged {
                stream_id: self.id,
                state,
                at,
            },
        );
    }

    /// Record `lost` missed frames at the stream's position and tell the
    /// client, so it resynchronises exactly rather than drifting.
    fn account_loss(&mut self, lost: u64, notifier: &mut dyn Notifier) {
        if lost == 0 {
            return;
        }
        self.xrun_frames = self.xrun_frames.saturating_add(lost);
        self.xruns = self.xruns.saturating_add(1);
        send(
            notifier,
            self.grant.notify_endpoint,
            AudioNotify::Xrun {
                stream_id: self.id,
                at: self.position,
                lost_frames: lost,
            },
        );
    }

    /// This stream's contribution to the chunk being mixed, or [`None`] when
    /// it had no frames.
    fn contribution(&self) -> Option<StreamMix<'_>> {
        if self.covered == 0 {
            return None;
        }
        let channels = self.channels();
        let (format, samples) = if self.resampler.is_some() {
            (
                RESAMPLED_FORMAT,
                &self.staged[..self.covered * channels * RESAMPLED_FORMAT.bytes_per_sample()],
            )
        } else {
            (
                self.grant.format,
                &self.bytes[..self.covered * channels * self.grant.format.bytes_per_sample()],
            )
        };
        Some(StreamMix {
            format,
            matrix: &self.matrix,
            gain: self.software_gain,
            resampled: self.resampler.is_some(),
            samples,
        })
    }
}

/// Send one notification, best effort: a lost wake costs the client a late
/// refill, which its next period recovers.
pub(crate) fn send(notifier: &mut dyn Notifier, endpoint: u64, what: AudioNotify) {
    let frame: [u8; AUDIO_NOTIFY_LEN] = what.encode();
    notifier.notify(endpoint, &frame);
}

/// The device encoding an endpoint is programmed in for a client asking for
/// `wanted`: the client's own where the endpoint accepts it — which is what
/// keeps the bit-exact path bit-exact — and otherwise the widest it does.
fn device_format(facts: &AudioEndpointFacts, wanted: SampleFormat) -> Result<SampleFormat, Errno> {
    if facts.formats.contains(wanted) {
        return Ok(wanted);
    }
    FORMAT_PREFERENCE
        .iter()
        .copied()
        .find(|format| facts.formats.contains(*format))
        .ok_or(Errno::NotSupported)
}

/// The period an endpoint is programmed with for a client wanting
/// `target_frames` of latency at the device's own rate.
///
/// Derived from the device's own reported bounds and the client's target,
/// never a constant: half the target, so the ring sized from it holds the
/// whole target, clamped into what the endpoint will actually do.
fn device_period(facts: &AudioEndpointFacts, target_frames: u32) -> u32 {
    (target_frames / 2)
        .max(1)
        .clamp(facts.min_period_frames, facts.max_period_frames)
}

/// The power-of-two device ring `grant` admits: two periods where the
/// ceiling allows it, one otherwise.
///
/// Two periods is the least depth at which the driver can consume one while
/// the mixer fills the next; a ceiling that admits only one still yields a
/// working (if tighter) ring rather than a refusal.
fn device_ring_frames(grant: &ConfigureGrant) -> Result<u32, Errno> {
    let smallest = grant
        .period_frames
        .checked_next_power_of_two()
        .ok_or(Errno::OutOfRange)?;
    let frames = match smallest.checked_mul(2) {
        Some(doubled) if doubled <= grant.max_ring_frames => doubled,
        _ => smallest,
    };
    if frames > grant.max_ring_frames {
        return Err(Errno::OutOfRange);
    }
    Ok(frames)
}

/// `value` frames at `from` hertz, expressed at `to` hertz and rounded up so
/// a conversion never asks for less audio than was meant.
pub(crate) fn scale_frames(value: u32, from: Rate, to: Rate) -> u32 {
    if from == to {
        return value;
    }
    let scaled = (u64::from(value) * u64::from(to.hz())).div_ceil(u64::from(from.hz()).max(1));
    u32::try_from(scaled).unwrap_or(u32::MAX)
}

impl Device {
    /// Bind a discovered device channel: read its facts and every endpoint's.
    ///
    /// # Errors
    ///
    /// The driver's typed refusal, or [`Errno::NotFound`] when it presents no
    /// endpoint at all — a device with nothing to play or record is a
    /// malformed emission, not a silent sink.
    pub(crate) fn bind(
        channel_endpoint: u64,
        notify_endpoint: u64,
        transport: Box<dyn AudioChannelTransport>,
        first_device_id: u32,
    ) -> Result<Self, Errno> {
        let mut channel = AudioChannelClient::new(transport);
        let facts = channel.facts()?;
        let mut endpoints = Vec::new();
        for index in 0..facts.endpoints {
            let endpoint_facts = channel.endpoint_facts(index)?;
            endpoint_facts.validate()?;
            if !fallible::reserve(&mut endpoints, 1) {
                return Err(Errno::OutOfMemory);
            }
            endpoints.push(Endpoint {
                index,
                device_id: first_device_id
                    .checked_add(u32::from(index))
                    .ok_or(Errno::OutOfRange)?,
                facts: endpoint_facts,
                active: None,
            });
        }
        if endpoints.is_empty() {
            return Err(Errno::NotFound);
        }
        Ok(Self {
            channel_endpoint,
            notify_endpoint,
            channel,
            endpoints,
            lost: false,
        })
    }
}

/// Program endpoint `slot` for a client at `rate`/`format` wanting
/// `latency_target_frames`, create the device ring, and attach it.
///
/// Idempotent once configured: a second stream joins the running
/// configuration rather than reprogramming the hardware underneath the first,
/// because a reconfiguration is a device-wide event a live stream has not
/// agreed to.
///
/// # Errors
///
/// The driver's typed refusal, or the region host's.
pub(crate) fn configure_endpoint<H: RegionHost>(
    device: &mut Device,
    slot: usize,
    regions: &mut H,
    rate: Rate,
    format: SampleFormat,
    latency_target_frames: u32,
    dither_seed: u64,
) -> Result<(), Errno> {
    let endpoint = device.endpoints.get_mut(slot).ok_or(Errno::NotFound)?;
    if endpoint.active.is_some() {
        return Ok(());
    }
    let device_rate = endpoint.facts.rates.nearest(rate);
    let device_format = device_format(&endpoint.facts, format)?;
    let channel_map = endpoint.facts.channel_map;
    let target = scale_frames(latency_target_frames.max(2), rate, device_rate);
    let grant = device.channel.configure(ConfigureParams {
        endpoint: endpoint.index,
        rate: device_rate,
        format: device_format,
        channel_map,
        period_frames: device_period(&endpoint.facts, target),
    })?;
    let ring_frames = device_ring_frames(&grant)?;
    let geometry = grant.geometry(ring_frames)?;
    let region = regions.create(geometry.region_len())?;
    let attached = regions
        .grant(region, device.channel_endpoint)
        .and_then(|region_grant| {
            device.channel.attach(AttachParams {
                endpoint: endpoint.index,
                ring_frames,
                region_grant,
                notify_endpoint: device.notify_endpoint,
            })
        });
    if let Err(err) = attached {
        regions.release(region);
        return Err(err);
    }
    let sink = SinkFormat {
        format: grant.format,
        rate: grant.rate,
        channel_map: grant.channel_map,
    };
    let period = grant.period_frames as usize;
    endpoint.active = Some(Active {
        grant,
        geometry,
        region,
        clock: ClockModel::new(grant.rate),
        mixer: Mixer::new(sink, period, dither_seed)?,
        sink,
        period: fallible::filled(period * sink.frame_bytes(), 0u8).ok_or(Errno::OutOfMemory)?,
        pivot: fallible::filled(period * sink.channels(), 0.0f32).ok_or(Errno::OutOfMemory)?,
        banks: Vec::new(),
        running: false,
        position: Frames::ZERO,
        xrun_frames: 0,
    });
    Ok(())
}

/// Release a programmed endpoint: detach the driver's view of the ring and
/// drop this service's.
pub(crate) fn release_endpoint<H: RegionHost>(device: &mut Device, slot: usize, regions: &mut H) {
    let Some(endpoint) = device.endpoints.get_mut(slot) else {
        return;
    };
    let Some(active) = endpoint.active.take() else {
        return;
    };
    if !device.lost {
        let _ = device.channel.detach(endpoint.index);
    }
    regions.release(active.region);
}

/// Give a closed stream's hold on its endpoint's shared phase bank back.
pub(crate) fn release_stream_bank(active: &mut Active, stream: &Stream) {
    if stream.resampler.is_some() {
        active.release_bank(stream.bank);
    }
}

/// What one pump of an endpoint did.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Pumped {
    /// Device frames published into (or taken from) the device ring.
    pub(crate) moved: u32,
    /// Every live stream on the endpoint is draining and has nothing left,
    /// so the drain may be passed down to the device.
    pub(crate) drained: bool,
}

/// Move frames for one endpoint: refill a sink, or harvest a source.
///
/// # Errors
///
/// A corrupt ring state or a region the host no longer holds, as a typed
/// [`Errno`]. The caller treats a fault as the device being lost rather than
/// retrying into it.
pub(crate) fn pump_endpoint<H: RegionHost>(
    device: &mut Device,
    device_index: usize,
    slot: usize,
    streams: &mut [Stream],
    regions: &mut H,
    notifier: &mut dyn Notifier,
) -> Result<Pumped, Errno> {
    let endpoint = device.endpoints.get_mut(slot).ok_or(Errno::NotFound)?;
    let Some(active) = endpoint.active.as_mut() else {
        return Ok(Pumped::default());
    };
    match endpoint.facts.direction {
        StreamDirection::Playback => {
            refill_sink(active, device_index, slot, streams, regions, notifier)
        }
        StreamDirection::Capture => {
            harvest_source(active, device_index, slot, streams, regions, notifier)
        }
    }
}

/// Fill the device ring with mixed periods while it has room and a live
/// stream has something to say.
fn refill_sink<H: RegionHost>(
    active: &mut Active,
    device_index: usize,
    slot: usize,
    streams: &mut [Stream],
    regions: &mut H,
    notifier: &mut dyn Notifier,
) -> Result<Pumped, Errno> {
    let mut pumped = Pumped::default();
    loop {
        let room = ring_room(active, regions)?;
        let chunk = usize::try_from(active.period_frames().min(room)).unwrap_or(0);
        if chunk == 0 {
            break;
        }
        let mut live = false;
        let mut all_draining = true;
        let mut covered_max = 0usize;
        for stream in streams.iter_mut() {
            if !stream.on(device_index, slot) || !stream.is_live() {
                continue;
            }
            live = true;
            // A stream stopping at a named frame takes no more than the
            // frames left before it, so the stop lands exactly there.
            let cap = stream.stop_limit().map_or(chunk, |limit| limit.min(chunk));
            all_draining &= stream.is_draining() || cap < chunk;
            covered_max = covered_max.max(pull_playback(stream, active, regions, cap)?);
        }
        if !live {
            break;
        }
        let frames = if all_draining {
            covered_max.min(chunk)
        } else {
            chunk
        };
        if frames == 0 {
            pumped.drained = all_draining;
            break;
        }
        let Active {
            mixer,
            period,
            sink,
            ..
        } = active;
        let bytes = frames * sink.frame_bytes();
        let contributions = streams
            .iter()
            .filter(|stream| stream.on(device_index, slot) && stream.is_live())
            .filter_map(Stream::contribution);
        mixer.mix(contributions, frames, &mut period[..bytes])?;
        let published = write_device_ring(active, regions, bytes)?;
        pumped.moved = pumped.moved.saturating_add(published);
        // A running stream that had nothing queued was mixed as silence for
        // the frames it missed; a draining one asked for exactly what it had.
        for stream in streams.iter_mut() {
            if !stream.on(device_index, slot) || !stream.is_live() {
                continue;
            }
            if !stream.is_draining() && stream.stop_at.is_none() && stream.covered < frames {
                let short = u32::try_from(frames - stream.covered).unwrap_or(u32::MAX);
                let lost = scale_frames(short, active.grant.rate, stream.grant.rate);
                stream.account_loss(u64::from(lost), notifier);
            }
            stream.covered = 0;
        }
        if published == 0 {
            break;
        }
    }
    wake_clients(device_index, slot, streams, notifier);
    Ok(pumped)
}

/// Take the device's captured periods and fan them out to every live capture
/// stream on the endpoint.
fn harvest_source<H: RegionHost>(
    active: &mut Active,
    device_index: usize,
    slot: usize,
    streams: &mut [Stream],
    regions: &mut H,
    notifier: &mut dyn Notifier,
) -> Result<Pumped, Errno> {
    let mut pumped = Pumped::default();
    loop {
        let period_bytes = active.period_frames() as usize * active.sink.frame_bytes();
        let taken = {
            let bytes = regions.bytes(active.region)?;
            let mut ring = PcmRing::bind(bytes, active.geometry)?;
            ring.read(&mut active.period[..period_bytes])?
        };
        if taken == 0 {
            break;
        }
        pumped.moved = pumped.moved.saturating_add(taken);
        let frames = taken as usize;
        let decoded = convert::decode(
            active.sink.format,
            &active.period[..frames * active.sink.frame_bytes()],
            &mut active.pivot[..frames * active.sink.channels()],
        );
        for stream in streams.iter_mut() {
            if !stream.on(device_index, slot) || !stream.is_live() {
                continue;
            }
            push_capture(stream, active, regions, decoded, notifier)?;
        }
        if streams
            .iter()
            .all(|stream| !(stream.on(device_index, slot) && stream.is_live()))
        {
            break;
        }
    }
    pumped.drained = streams
        .iter()
        .filter(|stream| stream.on(device_index, slot) && stream.is_live())
        .all(Stream::is_draining);
    wake_clients(device_index, slot, streams, notifier);
    Ok(pumped)
}

/// Frames the device ring can still take.
fn ring_room<H: RegionHost>(active: &mut Active, regions: &mut H) -> Result<u32, Errno> {
    let bytes = regions.bytes(active.region)?;
    let ring = PcmRing::bind(bytes, active.geometry)?;
    ring.writable_frames()
}

/// Publish the first `bytes` of the endpoint's staged period into the device
/// ring, answering the frames the ring took.
fn write_device_ring<H: RegionHost>(
    active: &mut Active,
    regions: &mut H,
    bytes: usize,
) -> Result<u32, Errno> {
    let staged = &active.period[..bytes];
    let region = regions.bytes(active.region)?;
    let mut ring = PcmRing::bind(region, active.geometry)?;
    ring.write(staged)
}

/// Read up to one chunk's worth of a playback stream's frames, filtering them
/// to the device's rate where the two differ, and answer the device frames it
/// can contribute.
fn pull_playback<H: RegionHost>(
    stream: &mut Stream,
    active: &Active,
    regions: &mut H,
    chunk: usize,
) -> Result<usize, Errno> {
    let Some(region) = stream.region else {
        stream.covered = 0;
        return Ok(0);
    };
    let channels = stream.channels();
    let frame_bytes = channels * stream.grant.format.bytes_per_sample();
    if stream.resampler.is_none() {
        let wanted = (chunk * frame_bytes).min(stream.bytes.len());
        let got = {
            let bytes = regions.bytes(region)?;
            let mut ring = PcmRing::bind(bytes, stream.geometry)?;
            ring.read(&mut stream.bytes[..wanted])?
        };
        stream.position = advance(stream.position, u64::from(got));
        stream.covered = got as usize;
        return Ok(stream.covered);
    }
    let bank = active
        .banks
        .get(stream.bank)
        .and_then(Option::as_ref)
        .ok_or(Errno::NotFound)?;
    while stream.pending_frames < chunk {
        let wanted = stream.want_in * frame_bytes;
        let got = {
            let bytes = regions.bytes(region)?;
            let mut ring = PcmRing::bind(bytes, stream.geometry)?;
            ring.read(&mut stream.bytes[..wanted])?
        } as usize;
        if got == 0 {
            break;
        }
        stream.position = advance(stream.position, got as u64);
        let Stream {
            bytes,
            pivot,
            pending,
            pending_frames,
            resampler,
            ..
        } = stream;
        let decoded = convert::decode(
            stream.grant.format,
            &bytes[..got * frame_bytes],
            &mut pivot[..got * channels],
        );
        let Some(filter) = resampler.as_mut() else {
            break;
        };
        let (_, made) = filter.process(
            &bank.bank,
            &pivot[..decoded],
            &mut pending[*pending_frames * channels..],
        )?;
        *pending_frames += made;
        if made == 0 {
            break;
        }
    }
    let take = stream.pending_frames.min(chunk);
    let samples = take * channels;
    convert::encode(
        RESAMPLED_FORMAT,
        &stream.pending[..samples],
        &mut stream.staged[..samples * RESAMPLED_FORMAT.bytes_per_sample()],
    );
    stream
        .pending
        .copy_within(samples..stream.pending_frames * channels, 0);
    stream.pending_frames -= take;
    stream.covered = take;
    Ok(take)
}

/// Map, filter and encode one captured period for a capture stream, and give
/// it to that stream's client ring.
fn push_capture<H: RegionHost>(
    stream: &mut Stream,
    active: &Active,
    regions: &mut H,
    decoded: usize,
    notifier: &mut dyn Notifier,
) -> Result<(), Errno> {
    let Some(region) = stream.region else {
        return Ok(());
    };
    let device_channels = active.sink.channels();
    if device_channels == 0 {
        return Err(Errno::OutOfRange);
    }
    let frames = decoded / device_channels;
    let channels = stream.channels();
    stream.matrix.map(
        frames,
        stream.software_gain,
        &active.pivot[..frames * device_channels],
        &mut stream.pivot[..frames * channels],
    )?;
    let produced = if let Some(filter) = stream.resampler.as_mut() {
        {
            let bank = active
                .banks
                .get(stream.bank)
                .and_then(Option::as_ref)
                .ok_or(Errno::NotFound)?;
            let (_, made) = filter.process(
                &bank.bank,
                &stream.pivot[..frames * channels],
                &mut stream.pending[..],
            )?;
            convert::encode(
                stream.grant.format,
                &stream.pending[..made * channels],
                &mut stream.bytes[..],
            );
            made
        }
    } else {
        convert::encode(
            stream.grant.format,
            &stream.pivot[..frames * channels],
            &mut stream.bytes[..],
        );
        frames
    };
    let frame_bytes = channels * stream.grant.format.bytes_per_sample();
    let written = {
        let bytes = regions.bytes(region)?;
        let mut ring = PcmRing::bind(bytes, stream.geometry)?;
        ring.write(&stream.bytes[..produced * frame_bytes])?
    } as usize;
    stream.position = advance(stream.position, written as u64);
    // The client's ring could not hold everything the device gave: the frames
    // that did not fit are over-run and are counted, never silently dropped.
    stream.account_loss((produced - written) as u64, notifier);
    Ok(())
}

/// Tell every live stream on the endpoint that its ring moved.
fn wake_clients(device_index: usize, slot: usize, streams: &[Stream], notifier: &mut dyn Notifier) {
    for stream in streams {
        if !stream.on(device_index, slot) || !stream.is_live() {
            continue;
        }
        send(
            notifier,
            stream.grant.notify_endpoint,
            AudioNotify::SpaceAvailable {
                stream_id: stream.id,
                position: stream.position,
            },
        );
    }
}

/// `position` advanced by `frames`, saturating rather than wrapping — a real
/// stream cannot reach the end of a `u64` frame counter, and pinning it there
/// is a stopped clock rather than a position that lies.
fn advance(position: Frames, frames: u64) -> Frames {
    position
        .checked_add(frames)
        .unwrap_or_else(|| Frames::new(u64::MAX))
}
