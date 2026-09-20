//! The `audio-v1` server: devices, streams, and every decision the service
//! takes on a client's behalf (`plans/SOUND.md`).
//!
//! Pure over four injected seams — the region host, the device-channel
//! transport each bound driver is reached through, the monotonic clock every
//! period pair is stamped against, and the audit sink — so the whole
//! authority is exercised on a host with no machine attached.
//!
//! # Authority
//!
//! Playback needs no capability: the authorisation is that the caller's
//! session holds the sink's seat lease, checked at open through the one
//! routing policy against the kernel-attested caller. Opening a *source*
//! additionally demands `CAP_AUDIO_CAPTURE`, read from the caller's attested
//! capability summary and never from anything the caller said. A stream id is
//! a service-issued token checked against the attested pid, so a guessed id
//! reaches nothing.

use alloc::boxed::Box;
use alloc::vec::Vec;

use tairix_abi::audio::{
    encode_clock_reply, encode_enumerate_reply, encode_open_reply, encode_state_reply,
    notify_endpoint_for, AudioDeviceDescriptor, AudioNotify, AudioRequest, OpenParams, StreamGrant,
    StreamReport, StreamRole, StreamState, AUDIO_MAX_REPLY, MAX_CLIENT_STREAM_SLOTS,
};
use tairix_abi::driver::audio::{ring_bounds, Frames, StreamDirection, MAX_DEVICE_ENDPOINTS};
use tairix_abi::driver::audio_channel::AudioChannelNotify;
use tairix_abi::driver::audio_ring::PcmGeometry;
use tairix_abi::origin::Origin;
use tairix_abi::reply::encode_status_reply;
use tairix_abi::time::MonotonicClock;
use tairix_abi::{CapabilityId, Errno};
use tairix_audio::route::{self, Routing, SinkState, StreamRequest};
use tairix_audio::volume::{self, VolumeRequest};
use tairix_log::{log, Event, Field, FieldValue, Level, Sink};
use tairix_seat::SeatOwner;
use tairix_util::fallible;

use crate::channel::AudioChannelTransport;
use crate::device::{self, Device, Stream, StreamOpen};
use crate::events;
use crate::region::RegionHost;

/// The client wake-up transport.
///
/// One `ipc_send` to a stream's notify mailbox: the live service backs it
/// with `tairix_rt::ipc_send` and host tests with a recording double. A wake
/// that does not land costs a late refill, never correctness, so it has no
/// result.
pub trait Notifier {
    /// Deliver `frame` to `endpoint`, best effort.
    fn notify(&mut self, endpoint: u64, frame: &[u8]);
}

/// A kernel-attested caller of the audio service.
pub struct Caller {
    /// The origin the kernel vouches for: pid, uid, and the capability
    /// summary a capture open is checked against. Never caller-supplied.
    pub origin: Origin,
    /// The session the caller belongs to, where the seat model can say which
    /// one. Sinks are leased to seats by the seat integration; until that
    /// lands every sink is unleased, so this is `None` and the router admits
    /// any principal to an unclaimed sink — the headless case it already
    /// describes.
    pub seat: Option<SeatOwner>,
}

/// The one mixer, router and audio authority.
pub struct AudioService<H: RegionHost, N: Notifier, C: MonotonicClock> {
    regions: H,
    notifier: N,
    clock: C,
    devices: Vec<Device>,
    streams: Vec<Stream>,
    next_device_id: u32,
    next_stream_id: u64,
    default_sink: Option<u32>,
    default_source: Option<u32>,
}

impl<H: RegionHost, N: Notifier, C: MonotonicClock> AudioService<H, N, C> {
    /// A service with no device bound and no stream open.
    pub fn new(regions: H, notifier: N, clock: C) -> Self {
        Self {
            regions,
            notifier,
            clock,
            devices: Vec::new(),
            streams: Vec::new(),
            next_device_id: 1,
            next_stream_id: 1,
            default_sink: None,
            default_source: None,
        }
    }

    /// Bind one driver's discovered device channel and enumerate what it
    /// presents.
    ///
    /// Idempotent in the endpoint id: a repeated hand-off of a channel
    /// already bound is a no-op rather than a duplicate device, because the
    /// hardware-tree node persists for as long as the driver lives and the
    /// device manager re-offers it on every generation bump.
    ///
    /// # Errors
    ///
    /// The driver's typed refusal, or [`Errno::OutOfMemory`].
    pub fn bind_device(
        &mut self,
        channel_endpoint: u64,
        notify_endpoint: u64,
        transport: Box<dyn AudioChannelTransport>,
        sink: &dyn Sink,
    ) -> Result<(), Errno> {
        if self
            .devices
            .iter()
            .any(|device| device.channel_endpoint == channel_endpoint)
        {
            return Ok(());
        }
        let device = match Device::bind(
            channel_endpoint,
            notify_endpoint,
            transport,
            self.next_device_id,
        ) {
            Ok(device) => device,
            Err(err) => {
                audit(
                    sink,
                    events::DEVICE_BIND_FAILED,
                    Level::Warn,
                    "audio device channel could not be bound",
                    &[
                        Field {
                            key: "endpoint",
                            value: FieldValue::UnsignedInt(channel_endpoint),
                        },
                        Field {
                            key: "error",
                            value: FieldValue::Error(err),
                        },
                    ],
                );
                return Err(err);
            }
        };
        let endpoints = u32::try_from(device.endpoints.len()).unwrap_or(0);
        self.next_device_id = self.next_device_id.saturating_add(endpoints);
        if !fallible::reserve(&mut self.devices, 1) {
            return Err(Errno::OutOfMemory);
        }
        self.devices.push(device);
        self.adopt_defaults(sink);
        audit(
            sink,
            events::DEVICE_BOUND,
            Level::Info,
            "audio device channel bound",
            &[
                Field {
                    key: "endpoint",
                    value: FieldValue::UnsignedInt(channel_endpoint),
                },
                Field {
                    key: "endpoints",
                    value: FieldValue::UnsignedInt(u64::from(endpoints)),
                },
            ],
        );
        Ok(())
    }

    /// Devices bound so far.
    #[must_use]
    pub fn device_count(&self) -> usize {
        self.devices.len()
    }

    /// The notify port the `device`th bound channel's driver wakes this
    /// service on.
    #[must_use]
    pub fn device_notify_endpoint(&self, device: usize) -> Option<u64> {
        self.devices
            .get(device)
            .map(|device| device.notify_endpoint)
    }

    /// Streams open.
    #[must_use]
    pub fn stream_count(&self) -> usize {
        self.streams.len()
    }

    /// Adopt the first sink and the first source this service sees as the
    /// machine's defaults.
    ///
    /// A machine with one sound card should play sound without being
    /// configured; a later machine-wide policy write replaces this choice.
    /// Recorded, because "which device does audio come out of" is a decision
    /// a user is entitled to see.
    fn adopt_defaults(&mut self, sink: &dyn Sink) {
        for direction in [StreamDirection::Playback, StreamDirection::Capture] {
            let current = match direction {
                StreamDirection::Playback => self.default_sink,
                StreamDirection::Capture => self.default_source,
            };
            if current.is_some() {
                continue;
            }
            let Some(device_id) = self
                .devices
                .iter()
                .flat_map(|device| device.endpoints.iter())
                .find(|endpoint| endpoint.facts.direction == direction)
                .map(|endpoint| endpoint.device_id)
            else {
                continue;
            };
            match direction {
                StreamDirection::Playback => self.default_sink = Some(device_id),
                StreamDirection::Capture => self.default_source = Some(device_id),
            }
            audit(
                sink,
                events::DEFAULT_DEVICE_CHANGED,
                Level::Info,
                "default audio device adopted",
                &[
                    Field {
                        key: "device",
                        value: FieldValue::UnsignedInt(u64::from(device_id)),
                    },
                    Field {
                        key: "direction",
                        value: FieldValue::Str(direction_name(direction)),
                    },
                ],
            );
        }
    }

    /// Serve one `audio-v1` request from `caller`, writing the reply into
    /// `reply` and answering its length.
    ///
    /// Total: an undecodable frame, an unknown operation, a stream that is
    /// not the caller's, or any refusal below is a fully-encoded reply
    /// carrying a typed error. Never a panic.
    pub fn handle(
        &mut self,
        caller: &Caller,
        request: &[u8],
        reply: &mut [u8; AUDIO_MAX_REPLY],
        sink: &dyn Sink,
    ) -> usize {
        let decoded = match AudioRequest::decode(request) {
            Ok(decoded) => decoded,
            Err(err) => return put(reply, &encode_status_reply(Err(err))),
        };
        match decoded {
            AudioRequest::Enumerate { direction, index } => put(
                reply,
                &encode_enumerate_reply(self.enumerate(direction, index)),
            ),
            AudioRequest::Open(params) => {
                put(reply, &encode_open_reply(self.open(caller, &params, sink)))
            }
            AudioRequest::Attach {
                stream_id,
                region_grant,
            } => put(
                reply,
                &encode_status_reply(self.attach(caller, stream_id, region_grant)),
            ),
            AudioRequest::Start { stream_id, at } => put(
                reply,
                &encode_status_reply(self.start(caller, stream_id, at, sink)),
            ),
            AudioRequest::Stop { stream_id, at } => put(
                reply,
                &encode_status_reply(self.stop(caller, stream_id, at)),
            ),
            AudioRequest::Drain { stream_id } => put(
                reply,
                &encode_status_reply(self.drain(caller, stream_id, sink)),
            ),
            AudioRequest::Flush { stream_id } => {
                put(reply, &encode_status_reply(self.flush(caller, stream_id)))
            }
            AudioRequest::Clock { stream_id } => put(
                reply,
                &encode_clock_reply(self.clock_report(caller, stream_id)),
            ),
            AudioRequest::Gain {
                stream_id,
                millibel,
            } => put(
                reply,
                &encode_status_reply(self.set_level(caller, stream_id, Some(millibel), None)),
            ),
            AudioRequest::Mute { stream_id, muted } => put(
                reply,
                &encode_status_reply(self.set_level(caller, stream_id, None, Some(muted))),
            ),
            AudioRequest::State { stream_id } => {
                put(reply, &encode_state_reply(self.report(caller, stream_id)))
            }
            AudioRequest::Close { stream_id } => put(
                reply,
                &encode_status_reply(self.close(caller, stream_id, sink)),
            ),
            // Adopting a driver channel needs a transport and a bound notify
            // port, neither of which the pure engine can build; the process
            // half intercepts it before dispatch and calls `bind_device`.
            AudioRequest::BindDriver { .. } => {
                put(reply, &encode_status_reply(Err(Errno::NotSupported)))
            }
        }
    }

    /// A driver's notify frame arrived on the `device`th channel's port.
    ///
    /// A frame that does not decode is dropped: the port is the driver's own
    /// and a malformed wake is a driver defect, not something to act on.
    pub fn on_device_notify(&mut self, device: usize, frame: &[u8], sink: &dyn Sink) {
        let Ok(notify) = AudioChannelNotify::decode(frame) else {
            return;
        };
        match notify {
            AudioChannelNotify::PeriodElapsed {
                endpoint,
                position,
                sampled_at,
            } => {
                let Some(slot) = self.endpoint_slot(device, endpoint) else {
                    return;
                };
                if let Some(active) = self.active_mut(device, slot) {
                    active.position = position;
                    let _ = active.clock.observe(position, sampled_at);
                }
                self.pump(device, slot, sink);
            }
            AudioChannelNotify::Xrun {
                endpoint,
                position,
                lost_frames,
            } => {
                let Some(slot) = self.endpoint_slot(device, endpoint) else {
                    return;
                };
                if let Some(active) = self.active_mut(device, slot) {
                    active.position = position;
                    active.xrun_frames = active.xrun_frames.saturating_add(lost_frames);
                }
                for index in 0..self.streams.len() {
                    if !self.streams[index].on(device, slot) || !self.streams[index].is_live() {
                        continue;
                    }
                    let stream = &self.streams[index];
                    let at = stream.position;
                    let notify = AudioNotify::Xrun {
                        stream_id: stream.id,
                        at,
                        lost_frames,
                    };
                    let port = stream.grant.notify_endpoint;
                    self.streams[index].xrun_frames =
                        self.streams[index].xrun_frames.saturating_add(lost_frames);
                    self.streams[index].xruns = self.streams[index].xruns.saturating_add(1);
                    device::send(&mut self.notifier, port, notify);
                }
            }
            AudioChannelNotify::Drained { endpoint, position } => {
                let Some(slot) = self.endpoint_slot(device, endpoint) else {
                    return;
                };
                if let Some(active) = self.active_mut(device, slot) {
                    active.position = position;
                    active.running = false;
                }
                self.settle_drain(device, slot);
            }
            AudioChannelNotify::JackChanged { endpoint, jack } => {
                let Some(slot) = self.endpoint_slot(device, endpoint) else {
                    return;
                };
                if let Some(endpoint) = self
                    .devices
                    .get_mut(device)
                    .and_then(|device| device.endpoints.get_mut(slot))
                {
                    endpoint.facts.jack = jack;
                }
            }
        }
    }

    /// Move whatever frames the endpoint can take or give, and pass a
    /// completed drain down to the device.
    ///
    /// A pump that faults marks the device lost: retrying into a device that
    /// just faulted would storm it, and the streams' positions are intact.
    fn pump(&mut self, device_index: usize, slot: usize, sink: &dyn Sink) {
        let Self {
            regions,
            notifier,
            devices,
            streams,
            ..
        } = self;
        let Some(device) = devices.get_mut(device_index) else {
            return;
        };
        if device.lost {
            return;
        }
        let pumped =
            match device::pump_endpoint(device, device_index, slot, streams, regions, notifier) {
                Ok(pumped) => pumped,
                Err(err) => {
                    device.lost = true;
                    for stream in streams.iter_mut().filter(|s| s.on(device_index, slot)) {
                        let at = stream.position;
                        stream.set_state(StreamState::DeviceLost, at, notifier);
                    }
                    audit(
                        sink,
                        events::DEVICE_LOST,
                        Level::Error,
                        "audio device faulted; its streams hold their positions",
                        &[
                            Field {
                                key: "endpoint",
                                value: FieldValue::UnsignedInt(device.channel_endpoint),
                            },
                            Field {
                                key: "error",
                                value: FieldValue::Error(err),
                            },
                        ],
                    );
                    return;
                }
            };
        self.settle_stops(device_index, slot);
        if pumped.drained {
            self.finish_drain(device_index, slot);
        }
    }

    /// Pause every stream on the endpoint that has reached its scheduled
    /// stop, at exactly the frame it named.
    fn settle_stops(&mut self, device_index: usize, slot: usize) {
        let Self {
            notifier, streams, ..
        } = self;
        for stream in streams.iter_mut() {
            if !stream.on(device_index, slot) {
                continue;
            }
            let Some(at) = stream.stop_at else { continue };
            if stream.state == StreamState::Running && stream.position >= at {
                stream.stop_at = None;
                stream.set_state(StreamState::Paused, at, notifier);
            }
        }
    }

    /// Every draining stream on the endpoint has given up its last frame:
    /// pass the drain down and settle the streams.
    fn finish_drain(&mut self, device_index: usize, slot: usize) {
        let Self {
            devices,
            streams,
            notifier,
            ..
        } = self;
        let Some(device) = devices.get_mut(device_index) else {
            return;
        };
        let Some(endpoint) = device.endpoints.get(slot) else {
            return;
        };
        let draining = streams
            .iter()
            .any(|stream| stream.on(device_index, slot) && stream.state == StreamState::Draining);
        if !draining {
            return;
        }
        let index = endpoint.index;
        if device.lost {
            // Nothing will ever play these out, so the wait would never end.
            Self::settle_streams(device_index, slot, streams, notifier);
            return;
        }
        // Asking the device to drain is not the drain finishing: the frames
        // already handed over are in the driver's own transfers, and only it
        // can say when the last one was heard. The streams stay `Draining`
        // until its `Drained` notify arrives.
        let _ = device.channel.drain(index);
    }

    /// Complete every draining stream on `(device, slot)` — the device has
    /// played out what it held.
    fn settle_drain(&mut self, device_index: usize, slot: usize) {
        let Self {
            streams, notifier, ..
        } = self;
        Self::settle_streams(device_index, slot, streams, notifier);
    }

    /// Move each draining stream on `(device_index, slot)` to `Idle`.
    fn settle_streams(device_index: usize, slot: usize, streams: &mut [Stream], notifier: &mut N) {
        for stream in streams.iter_mut() {
            if !stream.on(device_index, slot) || stream.state != StreamState::Draining {
                continue;
            }
            let at = stream.position;
            stream.set_state(StreamState::Idle, at, notifier);
        }
    }

    /// The `index`th sink or source the caller may see.
    fn enumerate(
        &self,
        direction: StreamDirection,
        index: u16,
    ) -> Result<AudioDeviceDescriptor, Errno> {
        let default = self.default_for(direction);
        self.devices
            .iter()
            .flat_map(|device| device.endpoints.iter())
            .filter(|endpoint| endpoint.facts.direction == direction)
            .nth(usize::from(index))
            .map(|endpoint| AudioDeviceDescriptor {
                device_id: endpoint.device_id,
                direction,
                jack: endpoint.facts.jack,
                is_default: default == Some(endpoint.device_id),
                formats: endpoint.facts.formats,
                channel_map: endpoint.facts.channel_map,
                rates: endpoint.facts.rates,
                gain: endpoint.facts.gain,
                name: endpoint.facts.name,
            })
            .ok_or(Errno::NotFound)
    }

    /// Open a stream, and answer what was granted.
    fn open(
        &mut self,
        caller: &Caller,
        params: &OpenParams,
        sink: &dyn Sink,
    ) -> Result<StreamGrant, Errno> {
        let capture = params.direction == StreamDirection::Capture;
        if capture
            && !caller
                .origin
                .capabilities()
                .holds_cap(CapabilityId::AUDIO_CAPTURE)
        {
            Self::audit_capture(
                sink,
                caller,
                events::CAPTURE_REFUSED,
                "no capture capability",
            );
            return Err(Errno::PermissionDenied);
        }
        let resolved = self.resolve_target(caller, params);
        let (device_index, slot) = match resolved {
            Ok(target) => target,
            Err(err) => {
                if capture {
                    Self::audit_capture(sink, caller, events::CAPTURE_REFUSED, "no such source");
                }
                Self::audit_refusal(sink, params, "no endpoint to route to", err);
                return Err(err);
            }
        };
        let opened = self.build_stream(caller, params, device_index, slot);
        match opened {
            Ok(grant) => {
                if capture {
                    Self::audit_capture(
                        sink,
                        caller,
                        events::CAPTURE_OPENED,
                        "capture stream open",
                    );
                }
                Ok(grant)
            }
            Err(err) => {
                if capture {
                    Self::audit_capture(
                        sink,
                        caller,
                        events::CAPTURE_REFUSED,
                        "source unavailable",
                    );
                }
                Self::audit_refusal(sink, params, "the endpoint could not be programmed", err);
                Err(err)
            }
        }
    }

    /// Record a device that could not be primed, naming what refused it.
    fn audit_refusal_device(sink: &dyn Sink, channel_endpoint: u64, err: Errno) {
        audit(
            sink,
            events::STREAM_REFUSED,
            Level::Warn,
            "the device would not take the stream's first periods",
            &[
                Field {
                    key: "endpoint",
                    value: FieldValue::UnsignedInt(channel_endpoint),
                },
                Field {
                    key: "error",
                    value: FieldValue::Error(err),
                },
            ],
        );
    }

    /// Record a refused open with what refused it, so a machine with no
    /// sound names its reason instead of failing silently.
    fn audit_refusal(sink: &dyn Sink, params: &OpenParams, stage: &'static str, err: Errno) {
        audit(
            sink,
            events::STREAM_REFUSED,
            Level::Warn,
            stage,
            &[
                Field {
                    key: "device",
                    value: FieldValue::UnsignedInt(u64::from(params.device_id)),
                },
                Field {
                    key: "capture",
                    value: FieldValue::Bool(params.direction == StreamDirection::Capture),
                },
                Field {
                    key: "error",
                    value: FieldValue::Error(err),
                },
            ],
        );
    }

    /// Which endpoint a request lands on: the routing policy for a sink, the
    /// named-or-default source for a capture.
    fn resolve_target(
        &mut self,
        caller: &Caller,
        params: &OpenParams,
    ) -> Result<(usize, usize), Errno> {
        let device_id = match params.direction {
            StreamDirection::Playback => {
                let sinks = self.sink_states()?;
                let request = StreamRequest {
                    role: params.role,
                    requested_device: (params.device_id != 0).then_some(params.device_id),
                    owner: caller.seat,
                };
                match route::route(&request, &sinks) {
                    Routing::Play { device_id } | Routing::Pause { device_id } => device_id,
                    // A notification the router would discard is refused at
                    // open rather than accepted and silently dropped: a
                    // client is owed the truth about where its sound went.
                    Routing::Drop => return Err(Errno::SeatNotOwner),
                    Routing::Refuse(err) => return Err(err),
                }
            }
            StreamDirection::Capture => {
                if params.device_id != 0 {
                    params.device_id
                } else {
                    self.default_source.ok_or(Errno::DeviceOffline)?
                }
            }
        };
        self.locate(device_id, params.direction)
    }

    /// The sinks the routing policy sees.
    fn sink_states(&self) -> Result<Vec<SinkState>, Errno> {
        let default = self.default_sink;
        let states = self
            .devices
            .iter()
            .flat_map(|device| device.endpoints.iter())
            .filter(|endpoint| endpoint.facts.direction == StreamDirection::Playback)
            .map(|endpoint| SinkState {
                device_id: endpoint.device_id,
                is_default: default == Some(endpoint.device_id),
                // Sinks are leased to seats by the seat integration; until it
                // lands no sink is claimed, which is the router's own
                // headless case.
                leased_to: None,
            });
        let count = self
            .devices
            .iter()
            .flat_map(|device| device.endpoints.iter())
            .filter(|endpoint| endpoint.facts.direction == StreamDirection::Playback)
            .count();
        fallible::collected(count, states).ok_or(Errno::OutOfMemory)
    }

    /// The (device, endpoint) a client-visible identity names.
    fn locate(&self, device_id: u32, direction: StreamDirection) -> Result<(usize, usize), Errno> {
        for (index, device) in self.devices.iter().enumerate() {
            for (slot, endpoint) in device.endpoints.iter().enumerate() {
                if endpoint.device_id == device_id {
                    if endpoint.facts.direction != direction {
                        return Err(Errno::NotSupported);
                    }
                    if device.lost {
                        return Err(Errno::DeviceOffline);
                    }
                    return Ok((index, slot));
                }
            }
        }
        Err(Errno::NotFound)
    }

    /// Program the endpoint if it is not already, size the client's ring, and
    /// record the stream.
    fn build_stream(
        &mut self,
        caller: &Caller,
        params: &OpenParams,
        device_index: usize,
        slot: usize,
    ) -> Result<StreamGrant, Errno> {
        let pid = caller.origin.pid();
        let stream_slot = self.free_slot(pid).ok_or(Errno::LimitExceeded)?;
        let seed = self.clock.now_ns();
        let Self {
            devices, regions, ..
        } = self;
        let device = devices.get_mut(device_index).ok_or(Errno::NotFound)?;
        device::configure_endpoint(
            device,
            slot,
            regions,
            params.rate,
            params.format,
            params.latency_target_frames,
            seed,
        )?;
        let clock_domain = device
            .endpoints
            .get(slot)
            .map_or(0, |endpoint| endpoint.device_id);
        let active = device
            .endpoints
            .get_mut(slot)
            .and_then(|endpoint| endpoint.active.as_mut())
            .ok_or(Errno::NotConnected)?;
        let period_at_client =
            device::scale_frames(active.period_frames(), active.grant.rate, params.rate);
        let ring_frames = client_ring_frames(params.latency_target_frames, period_at_client)?;
        // The granted latency *is* the ring: one device period of it is the
        // mixer's own contribution and the rest is what the client may have
        // queued, so the figure a client reasons about is the whole of what
        // it can have in flight.
        let latency_frames = ring_frames;
        let geometry = PcmGeometry::new(ring_frames, params.format, params.channel_map.channels())?;
        let stream_id = self.next_stream_id;
        let grant = StreamGrant {
            stream_id,
            notify_endpoint: notify_endpoint_for(pid, stream_slot),
            rate: params.rate,
            format: params.format,
            channel_map: params.channel_map,
            ring_frames,
            granted_latency_frames: latency_frames,
            granted_latency: Frames::new(u64::from(latency_frames)).duration(params.rate),
            clock_domain,
        };
        let stream = Stream::open(
            active,
            &StreamOpen {
                id: stream_id,
                owner_pid: pid,
                device: device_index,
                endpoint: slot,
                direction: params.direction,
                role: params.role,
                grant,
                geometry,
            },
        )?;
        if !fallible::reserve(&mut self.streams, 1) {
            return Err(Errno::OutOfMemory);
        }
        self.streams.push(stream);
        self.next_stream_id = self.next_stream_id.saturating_add(1);
        self.resolve_gains(device_index, slot);
        Ok(grant)
    }

    /// The lowest per-process stream slot `pid` is not already using.
    fn free_slot(&self, pid: u64) -> Option<u64> {
        (0..MAX_CLIENT_STREAM_SLOTS).find(|slot| {
            let endpoint = notify_endpoint_for(pid, *slot);
            !self
                .streams
                .iter()
                .any(|stream| stream.owner_pid == pid && stream.grant.notify_endpoint == endpoint)
        })
    }

    /// The index of a stream the caller genuinely owns.
    fn owned(&self, caller: &Caller, stream_id: u64) -> Result<usize, Errno> {
        self.streams
            .iter()
            .position(|stream| stream.id == stream_id)
            .filter(|index| self.streams[*index].owner_pid == caller.origin.pid())
            .ok_or(Errno::NotFound)
    }

    /// Adopt the caller's granted ring for its stream.
    fn attach(&mut self, caller: &Caller, stream_id: u64, region_grant: u64) -> Result<(), Errno> {
        let index = self.owned(caller, stream_id)?;
        let len = self.streams[index].geometry.region_len();
        let adopted = self.regions.adopt(region_grant, len)?;
        if let Some(previous) = self.streams[index].region.replace(adopted) {
            self.regions.release(previous);
        }
        Ok(())
    }

    /// Begin moving frames at an exact position.
    fn start(
        &mut self,
        caller: &Caller,
        stream_id: u64,
        at: Frames,
        sink: &dyn Sink,
    ) -> Result<(), Errno> {
        let index = self.owned(caller, stream_id)?;
        if self.streams[index].region.is_none() {
            return Err(Errno::NotAttached);
        }
        if self.streams[index].state == StreamState::DeviceLost {
            return Err(Errno::DeviceOffline);
        }
        let position = self.streams[index].position;
        // A start behind the position the stream has already reached names a
        // frame that is gone; there is no honest answer but a refusal.
        let Some(skip) = at.since(position) else {
            return Err(Errno::OutOfRange);
        };
        if skip != 0 {
            self.discard(index, skip)?;
        }
        self.streams[index].stop_at = None;
        let (device_index, slot) = (self.streams[index].device, self.streams[index].endpoint);
        let at = self.streams[index].position;
        {
            let Self {
                streams, notifier, ..
            } = self;
            streams[index].set_state(StreamState::Running, at, notifier);
        }
        self.resolve_gains(device_index, slot);
        self.run_endpoint(device_index, slot, sink)
    }

    /// Prime (playback) or arm (capture) the endpoint and set it clocking.
    fn run_endpoint(
        &mut self,
        device_index: usize,
        slot: usize,
        sink: &dyn Sink,
    ) -> Result<(), Errno> {
        let direction = self
            .devices
            .get(device_index)
            .and_then(|device| device.endpoints.get(slot))
            .map(|endpoint| endpoint.facts.direction)
            .ok_or(Errno::NotFound)?;
        let already = self
            .devices
            .get(device_index)
            .and_then(|device| device.endpoints.get(slot))
            .and_then(|endpoint| endpoint.active.as_ref())
            .is_some_and(|active| active.running);
        // A sink is primed before it is clocked, or its first period is a
        // gap; a source has nothing to prime and is clocked first. Filling
        // the ring is only half of it: the driver has to be told to move
        // those frames onto the device, because nothing has interrupted yet
        // to make it do so on its own.
        if direction == StreamDirection::Playback && !already {
            self.pump(device_index, slot, sink);
            let device = self.devices.get_mut(device_index).ok_or(Errno::NotFound)?;
            if let Err(err) = device::prime_endpoint(device, slot) {
                Self::audit_refusal_device(sink, device.channel_endpoint, err);
                return Err(err);
            }
        }
        if !already {
            let device = self.devices.get_mut(device_index).ok_or(Errno::NotFound)?;
            let endpoint_index = device
                .endpoints
                .get(slot)
                .map(|endpoint| endpoint.index)
                .ok_or(Errno::NotFound)?;
            let position = device
                .endpoints
                .get(slot)
                .and_then(|endpoint| endpoint.active.as_ref())
                .map_or(Frames::ZERO, |active| active.position);
            device.channel.start(endpoint_index, position)?;
            if let Some(active) = device
                .endpoints
                .get_mut(slot)
                .and_then(|endpoint| endpoint.active.as_mut())
            {
                active.running = true;
            }
        } else if direction == StreamDirection::Playback {
            self.pump(device_index, slot, sink);
        }
        Ok(())
    }

    /// Stop at an exact position, holding it so a resume is exact.
    fn stop(&mut self, caller: &Caller, stream_id: u64, at: Frames) -> Result<(), Errno> {
        let index = self.owned(caller, stream_id)?;
        let position = self.streams[index].position;
        let (device_index, slot) = (self.streams[index].device, self.streams[index].endpoint);
        if at > position {
            // A stop scheduled ahead: the pump stops pulling this stream at
            // exactly that frame and settles it there.
            self.streams[index].stop_at = Some(at);
            return Ok(());
        }
        self.streams[index].stop_at = None;
        {
            let Self {
                streams, notifier, ..
            } = self;
            streams[index].set_state(StreamState::Paused, position, notifier);
        }
        self.quiesce_endpoint(device_index, slot);
        Ok(())
    }

    /// Stop the device once nothing on the endpoint is running.
    fn quiesce_endpoint(&mut self, device_index: usize, slot: usize) {
        if self
            .streams
            .iter()
            .any(|stream| stream.on(device_index, slot) && stream.is_live())
        {
            self.resolve_gains(device_index, slot);
            return;
        }
        let Self {
            devices, regions, ..
        } = self;
        let Some(device) = devices.get_mut(device_index) else {
            return;
        };
        let Some(endpoint) = device.endpoints.get(slot) else {
            return;
        };
        let index = endpoint.index;
        let running = endpoint
            .active
            .as_ref()
            .is_some_and(|active| active.running);
        if !device.lost {
            if running {
                let position = endpoint
                    .active
                    .as_ref()
                    .map_or(Frames::ZERO, |active| active.position);
                let _ = device.channel.stop(index, position);
            }
            // Nobody is playing on it, so hand the endpoint back instead of
            // holding the device's stream and the shared region open for the
            // life of the driver. The next open configures it afresh.
            let _ = device.channel.detach(index);
        }
        if let Some(active) = device
            .endpoints
            .get_mut(slot)
            .and_then(|endpoint| endpoint.active.take())
        {
            regions.release(active.region);
        }
    }

    /// Play out everything queued, then stop.
    fn drain(&mut self, caller: &Caller, stream_id: u64, sink: &dyn Sink) -> Result<(), Errno> {
        let index = self.owned(caller, stream_id)?;
        if self.streams[index].state != StreamState::Running {
            return Err(Errno::NotConnected);
        }
        let (device_index, slot) = (self.streams[index].device, self.streams[index].endpoint);
        let at = self.streams[index].position;
        {
            let Self {
                streams, notifier, ..
            } = self;
            streams[index].set_state(StreamState::Draining, at, notifier);
        }
        self.pump(device_index, slot, sink);
        Ok(())
    }

    /// Discard everything queued; the position advances over the discarded
    /// frames, so what follows still describes where it belongs.
    fn flush(&mut self, caller: &Caller, stream_id: u64) -> Result<(), Errno> {
        let index = self.owned(caller, stream_id)?;
        let queued = self.queued_frames(index)?;
        self.discard(index, u64::from(queued))?;
        self.streams[index].reset_filter();
        Ok(())
    }

    /// The device clock this stream is in the domain of.
    fn clock_report(
        &self,
        caller: &Caller,
        stream_id: u64,
    ) -> Result<tairix_abi::audio::ClockReport, Errno> {
        let index = self.owned(caller, stream_id)?;
        let stream = &self.streams[index];
        self.devices
            .get(stream.device)
            .and_then(|device| device.endpoints.get(stream.endpoint))
            .and_then(|endpoint| endpoint.active.as_ref())
            .ok_or(Errno::NotConnected)?
            .clock
            .report()
            // No period has been reported yet, so there is no fit to hand
            // back and no honest value to invent.
            .ok_or(Errno::WouldBlock)
    }

    /// Set this stream's gain, its mute, or both.
    fn set_level(
        &mut self,
        caller: &Caller,
        stream_id: u64,
        millibel: Option<i32>,
        muted: Option<bool>,
    ) -> Result<(), Errno> {
        let index = self.owned(caller, stream_id)?;
        if let Some(millibel) = millibel {
            self.streams[index].gain_millibel = millibel;
        }
        if let Some(muted) = muted {
            self.streams[index].muted = muted;
        }
        let (device_index, slot) = (self.streams[index].device, self.streams[index].endpoint);
        self.resolve_gains(device_index, slot);
        Ok(())
    }

    /// Where the stream stands, and what it has lost.
    fn report(&self, caller: &Caller, stream_id: u64) -> Result<StreamReport, Errno> {
        let index = self.owned(caller, stream_id)?;
        let stream = &self.streams[index];
        Ok(StreamReport {
            state: stream.state,
            changed_at: stream.state_at,
            xruns: stream.xruns,
            xrun_frames: stream.xrun_frames,
        })
    }

    /// Close the stream and release its region; the endpoint follows when it
    /// was the last one on it.
    fn close(&mut self, caller: &Caller, stream_id: u64, sink: &dyn Sink) -> Result<(), Errno> {
        let index = self.owned(caller, stream_id)?;
        let stream = self.streams.remove(index);
        if let Some(region) = stream.region {
            self.regions.release(region);
        }
        let (device_index, slot) = (stream.device, stream.endpoint);
        if let Some(active) = self.active_mut(device_index, slot) {
            device::release_stream_bank(active, &stream);
        }
        if stream.direction == StreamDirection::Capture {
            Self::audit_capture(
                sink,
                caller,
                events::CAPTURE_CLOSED,
                "capture stream closed",
            );
        }
        self.quiesce_endpoint(device_index, slot);
        if !self
            .streams
            .iter()
            .any(|other| other.on(device_index, slot))
        {
            let Self {
                devices, regions, ..
            } = self;
            if let Some(device) = devices.get_mut(device_index) {
                device::release_endpoint(device, slot, regions);
            }
        }
        Ok(())
    }

    /// Re-resolve every stream's one multiply on the endpoint, because the
    /// ducking rule reads the roles of the streams beside it.
    fn resolve_gains(&mut self, device_index: usize, slot: usize) {
        let mut others: Vec<StreamRole> = Vec::new();
        let count = self
            .streams
            .iter()
            .filter(|stream| stream.on(device_index, slot) && stream.is_live())
            .count();
        if fallible::reserve(&mut others, count) {
            others.extend(
                self.streams
                    .iter()
                    .filter(|stream| stream.on(device_index, slot) && stream.is_live())
                    .map(|stream| stream.role),
            );
        }
        for stream in &mut self.streams {
            if !stream.on(device_index, slot) {
                continue;
            }
            let request = VolumeRequest {
                stream_millibel: stream.gain_millibel,
                application_millibel: 0,
                // The sink's own level and the device's control are the
                // per-sink volume surface's, which arrives with the desktop
                // integration; a stream's gain is its own and is therefore
                // always the software multiply.
                sink_millibel: 0,
                duck_millibel: route::duck_millibel(stream.role, &others),
                muted: stream.muted,
            };
            stream.software_gain = volume::resolve(&request, None).software;
        }
    }

    /// Frames queued in a stream's client ring.
    fn queued_frames(&mut self, index: usize) -> Result<u32, Errno> {
        let Some(region) = self.streams[index].region else {
            return Ok(0);
        };
        let geometry = self.streams[index].geometry;
        let bytes = self.regions.bytes(region)?;
        let ring = tairix_abi::driver::audio_ring::PcmRing::bind(bytes, geometry)?;
        match self.streams[index].direction {
            StreamDirection::Playback => ring.readable_frames(),
            StreamDirection::Capture => Ok(0),
        }
    }

    /// Drop `frames` queued frames of a playback stream, advancing its
    /// position over them.
    fn discard(&mut self, index: usize, frames: u64) -> Result<(), Errno> {
        let Some(region) = self.streams[index].region else {
            return Ok(());
        };
        if self.streams[index].direction != StreamDirection::Playback {
            return Ok(());
        }
        let geometry = self.streams[index].geometry;
        let wanted = u32::try_from(frames).unwrap_or(u32::MAX);
        let dropped = {
            let bytes = self.regions.bytes(region)?;
            let mut ring = tairix_abi::driver::audio_ring::PcmRing::bind(bytes, geometry)?;
            ring.discard(wanted)?
        };
        self.streams[index].position = self.streams[index]
            .position
            .checked_add(u64::from(dropped))
            .unwrap_or(self.streams[index].position);
        Ok(())
    }

    /// The endpoint slot a driver's endpoint index names.
    fn endpoint_slot(&self, device: usize, endpoint: u16) -> Option<usize> {
        if endpoint >= MAX_DEVICE_ENDPOINTS {
            return None;
        }
        self.devices
            .get(device)?
            .endpoints
            .iter()
            .position(|candidate| candidate.index == endpoint)
    }

    /// The programmed state of one endpoint.
    fn active_mut(&mut self, device: usize, slot: usize) -> Option<&mut crate::device::Active> {
        self.devices
            .get_mut(device)?
            .endpoints
            .get_mut(slot)?
            .active
            .as_mut()
    }

    /// The machine's default for a direction.
    fn default_for(&self, direction: StreamDirection) -> Option<u32> {
        match direction {
            StreamDirection::Playback => self.default_sink,
            StreamDirection::Capture => self.default_source,
        }
    }

    /// Record a capture decision against the principal that took it.
    fn audit_capture(
        sink: &dyn Sink,
        caller: &Caller,
        id: tairix_log::EventId,
        message: &'static str,
    ) {
        let level = if id == events::CAPTURE_REFUSED {
            Level::Warn
        } else {
            Level::Info
        };
        audit(
            sink,
            id,
            level,
            message,
            &[
                Field {
                    key: "uid",
                    value: FieldValue::UnsignedInt(u64::from(caller.origin.uid())),
                },
                Field {
                    key: "pid",
                    value: FieldValue::UnsignedInt(caller.origin.pid()),
                },
            ],
        );
    }
}

/// The client ring a latency target of `target` frames earns, at least two
/// device periods deep and inside the ring vocabulary's fixed bounds.
///
/// A power of two, because the ring's slot index is a mask of the monotone
/// position rather than a division on the per-period path. Two periods is
/// the floor because one of them is the mixer's own contribution, so a
/// shallower ring would leave the client nothing to queue.
fn client_ring_frames(target: u32, period_at_client_rate: u32) -> Result<u32, Errno> {
    let floor = period_at_client_rate
        .max(1)
        .saturating_mul(2)
        .max(ring_bounds::MIN_FRAMES)
        .checked_next_power_of_two()
        .ok_or(Errno::OutOfRange)?;
    let wanted = target
        .max(ring_bounds::MIN_FRAMES)
        .checked_next_power_of_two()
        .ok_or(Errno::OutOfRange)?;
    let frames = wanted.max(floor).min(ring_bounds::MAX_FRAMES);
    if frames < floor {
        // The floor itself is past the vocabulary's ceiling: this device's
        // period cannot be served by any ring the contract admits.
        return Err(Errno::OutOfRange);
    }
    Ok(frames)
}

/// Copy `body` into the reply buffer and answer its length.
fn put(reply: &mut [u8; AUDIO_MAX_REPLY], body: &[u8]) -> usize {
    let len = body.len().min(reply.len());
    reply[..len].copy_from_slice(&body[..len]);
    len
}

/// The direction's name for an audit field.
const fn direction_name(direction: StreamDirection) -> &'static str {
    match direction {
        StreamDirection::Playback => "sink",
        StreamDirection::Capture => "source",
    }
}

/// Emit one audit record.
fn audit(
    sink: &dyn Sink,
    id: tairix_log::EventId,
    level: Level,
    message: &'static str,
    fields: &[Field<'_>],
) {
    log(
        sink,
        &Event {
            level,
            id,
            message,
            fields,
        },
    );
}
