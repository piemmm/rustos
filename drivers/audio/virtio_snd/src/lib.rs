//! TAIRiX virtio sound driver (`plans/SOUND.md` SND4).
//!
//! The device logic of a virtio sound device (virtio 1.2 §5.14, device type
//! 25), implementing [`Audio`] over the bus-agnostic split-virtqueue
//! transport in `lib/virtio`. It is the first audio driver and the one that
//! gives an end-to-end QEMU vertical on every Tier-1 architecture, so it is
//! the device the whole stack is proved against before a second one exists.
//!
//! This crate has two targets. The host-testable device engine
//! ([`VirtioSnd`]), the [`BIND_KEYS`] match table and the [`register`] entry
//! are the `lib` target, co-located *in the driver* because a sound card sits
//! well above the bootstrap floor and so has no charter-legal non-driver
//! consumer. `src/main.rs` is the `Run` binary the signed bundle installs.
//!
//! # The device, and what this driver does with it
//!
//! Four virtqueues: a control queue carrying request/response pairs, an event
//! queue the device posts jack and stream notifications on, and a transmit
//! and receive queue carrying PCM payloads. Bring-up reads the device's own
//! configuration (how many jacks, streams and channel maps it presents), then
//! enumerates each with an information request — so what this driver reports
//! is what the device said, never a table keyed on what it claims to be.
//!
//! A stream is programmed with `SET_PARAMS` and `PREPARE`, clocked with
//! `START`, and torn down with `STOP` and `RELEASE`. Payload moves one period
//! at a time: a period's worth of frames is copied out of the mixer's shared
//! ring into one of this driver's own DMA buffers and posted on the transmit
//! queue (or the reverse, on the receive queue). The copy is the point — the
//! DMA window stays this process's alone.
//!
//! # The position never lies
//!
//! A playback stream that is running must be fed every period or the device
//! glitches unpredictably. When the mixer's ring is short, this driver
//! submits **silence for exactly the frames it is missing** and adds them to
//! the stream's loss tally, so the frame position stays exact and the mixer
//! learns precisely what was lost rather than drifting. The device's own
//! `latency_bytes` is subtracted from the submitted total, so the reported
//! position is what has been clocked out rather than what has been handed
//! over.
//!
//! # Nothing spins
//!
//! The driver's serve loop (`lib/audiochan`) parks on the device interrupt;
//! this engine only ever drains what the device has already completed.
//! Control requests are the one bounded exception the silicon dictates: the
//! mixer's call is synchronous, so a control response is waited for on the
//! host's interrupt park with the device's own deadline, never polled.
//!
//! # Capabilities
//!
//! Loading requires [`CapabilityId::DRV_LOAD`]. The process reaches its
//! hardware through the resource grants its matched node requested, and
//! nothing here grants itself authority.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

pub mod wire;

use alloc::vec::Vec;

use tairix_abi::driver::audio::{
    ring_bounds, Audio, AudioDeviceFacts, AudioEndpointFacts, AudioInterrupt, AudioName,
    AudioServiced, ChannelMap, ChannelPosition, Frames, JackState, Rate, RateSet, RateSupport,
    SampleFormat, SampleFormats, StreamDirection, MAX_CHANNELS, MAX_DEVICE_ENDPOINTS,
};
use tairix_abi::driver::audio_channel::{ConfigureGrant, ConfigureParams};
use tairix_abi::driver::audio_ring::PcmRing;
use tairix_abi::driver::virtio::VirtioHost;
use tairix_abi::driver::BufferClass;
use tairix_abi::time::{MonotonicClock, Time64};
use tairix_abi::{CapabilityId, DriverBindKey, DriverError, DriverHandle, DriverHost, HwMatchKey};
use tairix_virtio::{
    BounceBuffer, ChainSegment, Direction, RequestQueue, SplitQueue, Status, Transport, UsedToken,
    VirtioError,
};

/// The virtio device id of a sound device (virtio 1.2 §5.14 — `virtio-snd`
/// is device type 25). [`BIND_KEYS`] is built from it, so a discovered
/// virtio node whose probed device id is 25 binds this driver and nothing
/// else.
pub const VIRTIO_SND_DEVICE_ID: u32 = 25;

/// Per-driver `DriverHandle` marker returned by [`register`].
const REGISTER_HANDLE_MARKER: u64 = 0x5653_4E44_0000_0001; // "VSND"

/// The bind priority [`BIND_KEYS`] carries.
///
/// A virtio device-id match is exact — the discovered node's probed device id
/// either is `virtio-snd` or it is not — so it ranks at the exact-match tier
/// alongside the other concrete-identity drivers.
const BIND_PRIORITY: u16 = 10;

/// This driver's hardware bind table: a virtio sound device, matched by its
/// virtio device id.
///
/// The single source of truth the signed-manifest bind table is authored from
/// and `devmgr` resolves a discovered node against. The match key carries no
/// transport detail: the same driver binds the device however it is attached,
/// because the bus-agnostic transport abstracts the bus.
pub const BIND_KEYS: &[DriverBindKey] = &[DriverBindKey::new(
    BIND_PRIORITY,
    HwMatchKey::virtio(VIRTIO_SND_DEVICE_ID),
)];

/// [`MAX_CHANNELS`] as the byte-wide count the device's records carry.
const MAX_CHANNELS_U8: u8 = 8;

const _: () = assert!(MAX_CHANNELS_U8 as usize == MAX_CHANNELS);

/// Descriptors the control queue is programmed with.
///
/// One two-descriptor request is outstanding at a time; the rest of the ring
/// lets descriptors rotate, so a completion the device repeats names free
/// ones rather than the next request's. A fixed containment bound on the queue
/// this driver asks the device for, not a capacity.
const CONTROL_QUEUE_SIZE: u16 = 8;

/// Descriptors one control request occupies: the request and its reply.
const CONTROL_CHAIN_DESCRIPTORS: u16 = 2;

/// Descriptors the event queue is programmed with, and therefore the events
/// the device may post between two driver wakes.
///
/// A fixed containment bound: the device silently drops an event with no
/// posted buffer, and every event this driver acts on is idempotent (a jack
/// re-read, a period the transfer queue reports anyway), so a lost one costs
/// a late notification rather than a wrong one.
const EVENT_QUEUE_SIZE: u16 = 16;

/// Descriptors one transfer chain occupies: the transfer header, the
/// payload, and the status word the device writes back.
const TRANSFER_CHAIN_DESCRIPTORS: usize = 3;

/// Descriptors every period a device of `streams` streams keeps in flight
/// occupies at once on the transfer queue they share.
///
/// Derived, not chosen: a queue that cannot hold them all refuses the last
/// period as full, after its frames were taken, and the device runs dry.
fn transfer_descriptors(streams: u16) -> u16 {
    let descriptors = usize::from(streams) * PERIODS_IN_FLIGHT * TRANSFER_CHAIN_DESCRIPTORS;
    u16::try_from(descriptors).unwrap_or(u16::MAX)
}

/// Descriptors a transfer queue is programmed with for a device of `streams`
/// streams: [`transfer_descriptors`] in the power of two the split-ring
/// layout wants.
fn transfer_queue_size(streams: u16) -> u16 {
    transfer_descriptors(streams)
        .checked_next_power_of_two()
        .unwrap_or(MAX_TRANSFER_QUEUE_SIZE)
}

/// The number of streams `config` announces.
///
/// An endpoint index must fit the contract's own ceiling, because the mixer
/// addresses one by index and the interrupt bitmaps are that wide. A device
/// claiming more is refused rather than truncated: a silently-hidden stream is
/// a stream nobody can ever reach.
fn stream_count(config: &[u8; wire::config::LEN]) -> Result<u16, DriverError> {
    match u16::try_from(wire::read_u32(config, wire::config::STREAMS)) {
        Ok(streams) if streams != 0 && streams <= MAX_DEVICE_ENDPOINTS => Ok(streams),
        _ => Err(DriverError::DeviceFault),
    }
}

/// The deepest ring [`transfer_queue_size`] asks for: a device at the endpoint
/// ceiling, well inside what the split layout admits.
const MAX_TRANSFER_QUEUE_SIZE: u16 = 1 << 15;

const _: () = assert!(
    (MAX_DEVICE_ENDPOINTS as usize * PERIODS_IN_FLIGHT * TRANSFER_CHAIN_DESCRIPTORS)
        .next_power_of_two()
        <= MAX_TRANSFER_QUEUE_SIZE as usize
);

/// Periods kept in flight per stream.
///
/// Three is the smallest depth that keeps a device fed while one period is
/// being filled and one is being clocked out; a deeper queue would add
/// latency the mixer did not ask for, which is the number invariant the
/// charter's "one device period and nothing else" rests on.
const PERIODS_IN_FLIGHT: usize = 3;

/// Frames one period may carry, as a fixed containment bound on what this
/// driver will program.
///
/// Not a capacity: it bounds the DMA this driver reserves per stream, which
/// would otherwise be whatever a device asked for. The mixer derives the
/// period it wants from the device's reported bounds and the client's latency
/// target, and 4096 frames is 85 milliseconds at 48 kHz — far past any
/// latency an audio path wants.
const MAX_PERIOD_FRAMES: u32 = 4_096;

/// Largest payload one period buffer holds: [`MAX_PERIOD_FRAMES`] at the
/// widest frame the contract admits, so the two cannot drift.
const MAX_PERIOD_BYTES: usize = MAX_PERIOD_FRAMES as usize * MAX_CHANNELS * 4;

/// Frames the driver reports it can hold in flight for a stream.
///
/// The mixer derives the ring depth from this and the client's latency
/// target; it is the contract's own ceiling, so no hand-picked depth exists
/// here either.
const MAX_RING_FRAMES: u32 = ring_bounds::MAX_FRAMES;

/// How long a control request may take before it is failed closed.
///
/// A bounded budget rather than an unbounded park: a device that never
/// answers its control queue must not wedge the mixer's synchronous call. One
/// second is far past any real response and far short of a user noticing a
/// hang.
const CONTROL_TIMEOUT_NS: u64 = 1_000_000_000;

/// One period buffer in flight, or waiting to be filled.
struct PeriodBuffer {
    /// The DMA region carrying the transfer header, the payload, and the
    /// status word the device writes back.
    dma: BounceBuffer,
    /// Frames this buffer carries while it is posted, so a completion can
    /// advance the stream's position by exactly what the device consumed.
    frames: u32,
    /// The descriptor head the buffer is posted under, or [`None`] while it
    /// is free.
    posted: Option<u16>,
    /// What the device reported writing when it handed the buffer back,
    /// until the stream it belongs to is next serviced: the queue is shared
    /// by every stream of a direction, so any stream's service may collect
    /// another's completion.
    returned: Option<u32>,
}

/// What a transfer chain the device holds carries.
enum Transfer {
    /// Period `slot` of stream `stream`.
    Period { stream: usize, slot: usize },
    /// A period a released stream let go of while the device still held it,
    /// kept until the device hands it back so it is never freed under the
    /// device.
    Lent(BounceBuffer),
}

/// A transfer queue, which every stream of one direction shares, and what
/// each chain it holds carries, by descriptor head.
///
/// The record is carved at bring-up, one entry per descriptor, so neither
/// taking a completion back nor lending a period allocates or searches.
struct TransferQueue {
    ring: SplitQueue,
    carried: Vec<Option<Transfer>>,
}

impl TransferQueue {
    fn new(ring: SplitQueue) -> Result<Self, DriverError> {
        let mut carried = Vec::new();
        carried
            .try_reserve_exact(usize::from(ring.size()))
            .map_err(|_| DriverError::NoSpace)?;
        carried.resize_with(usize::from(ring.size()), || None);
        Ok(Self { ring, carried })
    }

    /// Post period `slot` of `stream` on this queue, carrying `frames` frames
    /// in `bytes` payload bytes.
    ///
    /// The chain is the specification's: the transfer header is always
    /// device-readable, the payload takes the stream's direction, and the
    /// status word is always device-writable.
    fn post<T: Transport>(
        &mut self,
        transport: &mut T,
        stream: &mut Stream,
        slot: usize,
        frames: u32,
        bytes: usize,
    ) -> Result<(), DriverError> {
        let playback = stream.direction == StreamDirection::Playback;
        let Ok(stream_index) = usize::try_from(stream.id) else {
            return Err(DriverError::OutOfRange);
        };
        let period = stream
            .periods
            .get_mut(slot)
            .ok_or(DriverError::DeviceFault)?;
        let base = period.dma.phys();
        let (Ok(hdr_len), Ok(payload_len), Ok(status_len)) = (
            u32::try_from(wire::XFER_HDR_LEN),
            u32::try_from(bytes),
            u32::try_from(wire::XFER_STATUS_LEN),
        ) else {
            return Err(DriverError::OutOfRange);
        };
        // The status word lives at the end of the *allocated* buffer, not at
        // the end of this transfer: a short drain tail must not move it onto
        // payload bytes the device is still reading.
        let status_offset = period.dma.capacity() - wire::XFER_STATUS_LEN;
        let (Ok(payload_at), Ok(status_at)) = (
            u64::try_from(wire::XFER_HDR_LEN),
            u64::try_from(status_offset),
        ) else {
            return Err(DriverError::OutOfRange);
        };
        let segments = [
            ChainSegment {
                phys: base,
                len: hdr_len,
                direction: Direction::DeviceRead,
            },
            ChainSegment {
                phys: base + payload_at,
                len: payload_len,
                direction: if playback {
                    Direction::DeviceRead
                } else {
                    Direction::DeviceWrite
                },
            },
            ChainSegment {
                phys: base + status_at,
                len: status_len,
                direction: Direction::DeviceWrite,
            },
        ];
        // Zero is no status a device writes, so a completion that wrote none
        // is refused rather than read as this buffer's last.
        period.dma.full_region_mut()[status_offset..].fill(0);
        let head = self
            .ring
            .add_chain(&segments)
            .map_err(VirtioError::as_driver_error)?;
        let Some(record) = self.carried.get_mut(usize::from(head)) else {
            return Err(DriverError::DeviceFault);
        };
        *record = Some(Transfer::Period {
            stream: stream_index,
            slot,
        });
        period.posted = Some(head);
        period.frames = frames;
        if playback {
            // Playback: the frames are the device's the moment it is handed
            // them, so the position advances here. Capture credits on
            // delivery instead, because frames nobody received are not frames
            // that arrived.
            stream.transferred = stream
                .transferred
                .checked_add(u64::from(frames))
                .ok_or(DriverError::OutOfRange)?;
        }
        self.ring.kick(transport);
        Ok(())
    }

    /// Keep each of `periods` the device still holds until it hands it back;
    /// the rest are freed.
    fn lend_held(&mut self, periods: Vec<PeriodBuffer>) {
        for mut period in periods {
            let Some(head) = period.posted else {
                continue;
            };
            match self.carried.get_mut(usize::from(head)) {
                Some(record) => *record = Some(Transfer::Lent(period.dma)),
                // Every posted head has a record; one that does not is kept
                // rather than freed under the device.
                None => period.dma.withhold(),
            }
        }
    }

    /// Never return the ring, or a period lent to it, to its pool: the device
    /// may still be using them.
    fn withhold(&mut self) {
        self.ring.withhold();
        for record in &mut self.carried {
            if let Some(Transfer::Lent(dma)) = record {
                dma.withhold();
            }
        }
    }
}

/// What one PCM stream of the device is and what it is doing.
struct Stream {
    /// The device's own stream id, which is its endpoint index.
    id: u32,
    direction: StreamDirection,
    formats: SampleFormats,
    rates: RateSupport,
    channels_min: u8,
    channels_max: u8,
    /// The channel layout the device published for this stream, or [`None`]
    /// when it publishes no channel maps at all.
    published_map: Option<ChannelMap>,
    /// The jack this stream's connector state is read from, if the device
    /// presents any jacks.
    jack: JackState,
    /// What the stream is programmed to, once configured.
    configured: Option<ConfigureGrant>,
    /// Whether the device is clocking it.
    running: bool,
    /// Whether the stream is draining: no new frames are accepted, and it
    /// stops once what is queued has played out.
    draining: bool,
    /// Frames handed to (or taken from) the device across its whole life.
    transferred: Frames,
    /// The device's latest reported latency, in frames, subtracted from
    /// [`Self::transferred`] to give the position actually clocked.
    latency_frames: u64,
    /// Frames of silence substituted for a short ring, cumulative.
    xrun_frames: u64,
    /// The period buffers this stream owns, allocated once at configure.
    periods: Vec<PeriodBuffer>,
}

impl Stream {
    /// The position the device has actually reached: what it was handed, less
    /// what it says is still buffered.
    fn position(&self) -> Frames {
        Frames::new(self.transferred.get().saturating_sub(self.latency_frames))
    }

    /// Bytes one frame of the configured format occupies.
    fn frame_bytes(&self) -> Option<usize> {
        let grant = self.configured.as_ref()?;
        Some(grant.format.bytes_per_sample() * usize::from(grant.channel_map.channels()))
    }
}

/// A virtio sound device.
pub struct VirtioSnd<'h, T: Transport> {
    transport: T,
    host: &'h dyn VirtioHost,
    /// The monotonic source every clock pair is stamped from. Monotonic
    /// rather than wall time because a clock step would otherwise corrupt
    /// every linear fit built on it.
    clock: &'h dyn MonotonicClock,
    controlq: RequestQueue,
    eventq: SplitQueue,
    txq: TransferQueue,
    rxq: TransferQueue,
    /// The one control request/response staging region: a request is
    /// outstanding at a time, so one buffer serves them all.
    control: BounceBuffer,
    /// The event slots posted on the event queue, and which descriptor head
    /// each is posted under.
    events: BounceBuffer,
    event_slots: [Option<u16>; EVENT_QUEUE_SIZE as usize],
    streams: Vec<Stream>,
    /// Interrupt causes accumulated from the event queue and the transfer
    /// completions, handed to the serve loop on its next read.
    pending: AudioInterrupt,
    /// Whether the device's event sources are armed.
    events_armed: bool,
}

/// Largest control message this driver stages: a query-info request, or the
/// largest information record a reply carries.
const CONTROL_BUFFER_LEN: usize = 256;

const _: () = assert!(CONTROL_BUFFER_LEN >= wire::QUERY_INFO_LEN);
const _: () = assert!(CONTROL_BUFFER_LEN >= wire::SET_PARAMS_LEN);
const _: () = assert!(CONTROL_BUFFER_LEN >= wire::HDR_LEN + wire::MAX_INFO_RECORD_LEN);

impl<'h, T: Transport> VirtioSnd<'h, T> {
    /// Bring the device online: negotiate, program the four virtqueues, post
    /// the event pool, and read the device's own description of every jack,
    /// stream and channel map it presents. Once the reset confirms, the device
    /// is declared quiesced to `host`, so memory an earlier instance left with
    /// it can be released; a failure once the device is live resets it again
    /// before its memory is released.
    ///
    /// # Errors
    ///
    /// The transport's or queue setup's [`VirtioError`] mapped to a
    /// [`DriverError`] — [`DriverError::Unsupported`] for a queue too shallow
    /// for what it carries — [`DriverError::DeviceFault`] for a device that
    /// never confirms its reset, clears `FEATURES_OK`, presents fewer than
    /// four queues, or describes more streams than the contract admits, and
    /// any [`DriverError`] a DMA allocation refuses.
    pub fn open(
        mut transport: T,
        host: &'h dyn VirtioHost,
        clock: &'h dyn MonotonicClock,
    ) -> Result<Self, DriverError> {
        transport.reset().map_err(VirtioError::as_driver_error)?;
        host.device_quiesced();
        let mut status = Status::default().with(Status::ACKNOWLEDGE);
        transport.set_status(status);
        status = status.with(Status::DRIVER);
        transport.set_status(status);
        let driver_features = transport.device_features() & wire::VIRTIO_F_VERSION_1;
        transport.set_driver_features(driver_features);
        status = status.with(Status::FEATURES_OK);
        transport.set_status(status);
        if !transport.status().contains(Status::FEATURES_OK) {
            return Err(VirtioError::FeaturesRejected.as_driver_error());
        }
        if transport.num_queues() < wire::QUEUE_COUNT {
            return Err(DriverError::DeviceFault);
        }
        // One reading of the device's description sizes the transfer queues
        // and is what the stream records are then read against, so it cannot
        // change between the two.
        let mut config = [0u8; wire::config::LEN];
        transport.read_config(0, &mut config);
        let streams = stream_count(&config)?;

        let program = |transport: &mut T, index, size, needed| {
            SplitQueue::new(transport, host, index, size, needed)
                .map_err(VirtioError::as_driver_error)
        };
        let controlq = RequestQueue::new(program(
            &mut transport,
            wire::CONTROL_QUEUE,
            CONTROL_QUEUE_SIZE,
            CONTROL_CHAIN_DESCRIPTORS,
        )?);
        // The event pool posts whatever the ring holds, so any depth serves.
        let eventq = program(&mut transport, wire::EVENT_QUEUE, EVENT_QUEUE_SIZE, 1)?;
        // Each transfer queue is sized for every stream, not only its own
        // direction's: a stream's direction is known only once it is
        // enumerated, which needs the queues up.
        let (size, needed) = (transfer_queue_size(streams), transfer_descriptors(streams));
        let txq = TransferQueue::new(program(&mut transport, wire::TX_QUEUE, size, needed)?)?;
        let rxq = TransferQueue::new(program(&mut transport, wire::RX_QUEUE, size, needed)?)?;

        let control = BounceBuffer::new(
            host.alloc_dma_zeroed(CONTROL_BUFFER_LEN * 2)?,
            BufferClass::NonSensitive,
        );
        let event_pool = BounceBuffer::new(
            host.alloc_dma_zeroed(EVENT_QUEUE_SIZE as usize * wire::event::LEN)?,
            BufferClass::NonSensitive,
        );

        status = status.with(Status::DRIVER_OK);
        transport.set_status(status);

        let mut device = Self {
            transport,
            host,
            clock,
            controlq,
            eventq,
            txq,
            rxq,
            control,
            events: event_pool,
            event_slots: [None; EVENT_QUEUE_SIZE as usize],
            streams: Vec::new(),
            pending: AudioInterrupt::NONE,
            events_armed: false,
        };
        device.arm(&config, streams)?;
        Ok(device)
    }

    /// Post the event pool and read the device's description of itself: the
    /// bring-up steps a live device takes part in.
    fn arm(&mut self, config: &[u8; wire::config::LEN], streams: u16) -> Result<(), DriverError> {
        for slot in 0..self.eventq.size() {
            self.post_event_slot(slot)?;
        }
        self.eventq.kick(&mut self.transport);
        self.enumerate(config, streams)
    }

    /// Read every jack, stream and channel-map record the device's
    /// configuration announces, building the endpoint table this driver
    /// reports.
    fn enumerate(
        &mut self,
        config: &[u8; wire::config::LEN],
        streams: u16,
    ) -> Result<(), DriverError> {
        let jacks = wire::read_u32(config, wire::config::JACKS);
        let chmaps = wire::read_u32(config, wire::config::CHMAPS);
        self.streams
            .try_reserve_exact(usize::from(streams))
            .map_err(|_| DriverError::NoSpace)?;
        for id in 0..u32::from(streams) {
            let stream = self.read_stream_info(id)?;
            self.streams.push(stream);
        }
        self.read_chmaps(chmaps)?;
        self.read_jacks(jacks)?;
        Ok(())
    }

    /// Read one stream's PCM information record.
    fn read_stream_info(&mut self, id: u32) -> Result<Stream, DriverError> {
        let record = self.query_info(wire::request::PCM_INFO, id, wire::pcm_info::LEN)?;
        let formats_mask = wire::read_u64(&record, wire::pcm_info::FORMATS);
        let rates_mask = wire::read_u64(&record, wire::pcm_info::RATES);
        let direction = match record[wire::pcm_info::DIRECTION] {
            wire::direction::OUTPUT => StreamDirection::Playback,
            wire::direction::INPUT => StreamDirection::Capture,
            _ => return Err(DriverError::DeviceFault),
        };
        let channels_min = record[wire::pcm_info::CHANNELS_MIN];
        let channels_max = record[wire::pcm_info::CHANNELS_MAX];
        let formats = decode_formats(formats_mask);
        let rates = decode_rates(rates_mask)?;
        // A stream whose usable format or channel set is empty cannot carry
        // audio at all, so it is a device fault rather than a quiet endpoint
        // the mixer would keep trying to open.
        if formats.is_empty()
            || channels_min == 0
            || channels_max < channels_min
            || usize::from(channels_min) > MAX_CHANNELS
        {
            return Err(DriverError::DeviceFault);
        }
        Ok(Stream {
            id,
            direction,
            formats,
            rates,
            channels_min,
            channels_max: channels_max.min(MAX_CHANNELS_U8),
            published_map: None,
            jack: JackState::Unknown,
            configured: None,
            running: false,
            draining: false,
            transferred: Frames::ZERO,
            latency_frames: 0,
            xrun_frames: 0,
            periods: Vec::new(),
        })
    }

    /// Read every channel-map record and attach it to the stream whose
    /// direction and channel count it fits.
    ///
    /// A device that publishes no channel maps leaves every stream's
    /// [`Stream::published_map`] empty; the facts path then reports the
    /// conventional layout for the channel count, which is what a channel
    /// count with no positions means.
    /// Run one *descriptive* info query, or report that the device declines
    /// the whole class.
    ///
    /// Jacks and channel maps describe a device; they are not what drives
    /// it. A device may advertise a count in its configuration and still
    /// answer `NOT_SUPP` (QEMU's virtio-sound does exactly that), and such a
    /// device plays audio perfectly well — so the refusal leaves the stream
    /// undescribed rather than failing a bring-up that would otherwise have
    /// succeeded. Nothing is invented in its place: the map stays absent and
    /// the jack unknown. A refused `PCM_INFO` does not come through here and
    /// stays fatal, because a stream that cannot be described cannot be
    /// driven.
    fn describe(
        &mut self,
        code: u32,
        id: u32,
        size: usize,
    ) -> Result<Option<[u8; wire::MAX_INFO_RECORD_LEN]>, DriverError> {
        match self.query_info(code, id, size) {
            Ok(record) => Ok(Some(record)),
            Err(DriverError::NotImplemented) => Ok(None),
            Err(err) => Err(err),
        }
    }

    fn read_chmaps(&mut self, chmaps: u32) -> Result<(), DriverError> {
        for id in 0..chmaps {
            // The refusal is of the class, not of this record, so there is
            // nothing to gain from asking for the rest.
            let Some(record) =
                self.describe(wire::request::CHMAP_INFO, id, wire::chmap_info::LEN)?
            else {
                break;
            };
            let direction = match record[wire::chmap_info::DIRECTION] {
                wire::direction::OUTPUT => StreamDirection::Playback,
                wire::direction::INPUT => StreamDirection::Capture,
                // A map naming a direction the specification does not is
                // ignored rather than refused: it describes no stream this
                // driver drives, and the conventional layout still applies.
                _ => continue,
            };
            let channels = record[wire::chmap_info::CHANNELS];
            let Some(map) = decode_chmap(&record, channels) else {
                continue;
            };
            for stream in &mut self.streams {
                if stream.direction == direction
                    && stream.published_map.is_none()
                    && channels >= stream.channels_min
                    && channels <= stream.channels_max
                {
                    stream.published_map = Some(map);
                    break;
                }
            }
        }
        Ok(())
    }

    /// Read every jack record and attach its connector state to the streams
    /// of that direction.
    ///
    /// The device relates a jack to a stream only through the HDA function
    /// node id both carry, which a device with no HDA topology leaves zero.
    /// Direction is therefore what this driver relates them by — the honest
    /// available answer — and a device with no jacks leaves every stream's
    /// state [`JackState::Unknown`], which is exactly what "this endpoint has
    /// no detection" means.
    fn read_jacks(&mut self, jacks: u32) -> Result<(), DriverError> {
        for id in 0..jacks {
            let Some(record) = self.describe(wire::request::JACK_INFO, id, wire::jack_info::LEN)?
            else {
                break;
            };
            let connected = record[wire::jack_info::CONNECTED] != 0;
            let state = if connected {
                JackState::Present
            } else {
                JackState::Absent
            };
            // One jack per stream, in publication order: the specification
            // gives no stronger relation, so claiming one would be inventing
            // a topology the device never described.
            if let Some(stream) = self.streams.get_mut(id as usize) {
                stream.jack = state;
            }
        }
        Ok(())
    }

    /// Issue one `*_INFO` query for a single item and return its record.
    fn query_info(
        &mut self,
        code: u32,
        start_id: u32,
        size: usize,
    ) -> Result<[u8; wire::MAX_INFO_RECORD_LEN], DriverError> {
        let mut request = [0u8; wire::QUERY_INFO_LEN];
        wire::put_u32(&mut request, 0, code);
        wire::put_u32(&mut request, 4, start_id);
        wire::put_u32(&mut request, 8, 1);
        let Ok(size_u32) = u32::try_from(size) else {
            return Err(DriverError::OutOfRange);
        };
        wire::put_u32(&mut request, 12, size_u32);
        let reply = self.control_request(&request, wire::HDR_LEN + size)?;
        let mut record = [0u8; wire::MAX_INFO_RECORD_LEN];
        let Some(body) = reply.get(wire::HDR_LEN..wire::HDR_LEN + size) else {
            return Err(DriverError::DeviceFault);
        };
        let Some(into) = record.get_mut(..size) else {
            return Err(DriverError::DeviceFault);
        };
        into.copy_from_slice(body);
        Ok(record)
    }

    /// Post `request` on the control queue, wait for the device's answer, and
    /// return the reply bytes (its status word included).
    ///
    /// The mixer's call is synchronous, so this waits — on the host's
    /// interrupt park with a bounded budget, never a spin — and fails closed
    /// on a device that says nothing.
    fn control_request(&mut self, request: &[u8], reply_len: usize) -> Result<&[u8], DriverError> {
        if request.len() > CONTROL_BUFFER_LEN || reply_len > CONTROL_BUFFER_LEN {
            return Err(DriverError::BufferTooSmall);
        }
        // The staging is the device's while a request it never answered is
        // out.
        self.controlq.settle(&mut self.transport, self.host)?;
        let region = self.control.full_region_mut();
        let (out, back) = region.split_at_mut(CONTROL_BUFFER_LEN);
        out[..request.len()].copy_from_slice(request);
        back[..reply_len].fill(0);
        let base = self.control.phys();
        let Ok(request_len) = u32::try_from(request.len()) else {
            return Err(DriverError::OutOfRange);
        };
        let Ok(reply_len_u32) = u32::try_from(reply_len) else {
            return Err(DriverError::OutOfRange);
        };
        let Ok(reply_offset) = u64::try_from(CONTROL_BUFFER_LEN) else {
            return Err(DriverError::OutOfRange);
        };
        let segments = [
            ChainSegment {
                phys: base,
                len: request_len,
                direction: Direction::DeviceRead,
            },
            ChainSegment {
                phys: base + reply_offset,
                len: reply_len_u32,
                direction: Direction::DeviceWrite,
            },
        ];
        let token = self.controlq.submit_and_wait(
            &mut self.transport,
            self.host,
            &segments,
            CONTROL_TIMEOUT_NS,
        )?;
        if (token.written as usize) < wire::HDR_LEN {
            return Err(DriverError::DeviceFault);
        }
        let reply = self
            .control
            .full_region_mut()
            .get(CONTROL_BUFFER_LEN..CONTROL_BUFFER_LEN + reply_len)
            .ok_or(DriverError::DeviceFault)?;
        match wire::read_u32(reply, 0) {
            wire::status::OK => Ok(reply),
            wire::status::NOT_SUPP => Err(DriverError::NotImplemented),
            wire::status::BAD_MSG => Err(DriverError::OutOfRange),
            // `IO_ERR` and an undefined status alike: the device failed, and
            // a status nobody defined is no more trustworthy than one that
            // says so.
            _ => Err(DriverError::DeviceFault),
        }
    }

    /// Hand one event buffer back to the device.
    ///
    /// The slot is zeroed first — no event the device posts has code zero —
    /// so a completion that wrote nothing is not read as the slot's last
    /// event.
    fn post_event_slot(&mut self, slot: u16) -> Result<(), DriverError> {
        let offset = usize::from(slot) * wire::event::LEN;
        let Ok(offset_u64) = u64::try_from(offset) else {
            return Err(DriverError::OutOfRange);
        };
        let Ok(len) = u32::try_from(wire::event::LEN) else {
            return Err(DriverError::OutOfRange);
        };
        self.events
            .full_region_mut()
            .get_mut(offset..offset + wire::event::LEN)
            .ok_or(DriverError::DeviceFault)?
            .fill(0);
        let head = self
            .eventq
            .add_chain(&[ChainSegment {
                phys: self.events.phys() + offset_u64,
                len,
                direction: Direction::DeviceWrite,
            }])
            .map_err(VirtioError::as_driver_error)?;
        let Some(record) = self.event_slots.get_mut(usize::from(head)) else {
            return Err(DriverError::DeviceFault);
        };
        *record = Some(slot);
        Ok(())
    }

    /// Drain the event queue into [`Self::pending`], reposting each buffer.
    ///
    /// At most a ring's worth per call: each buffer goes straight back, so a
    /// device completing them as fast as they are reposted would otherwise
    /// hold the drain for ever. What is left waits for the next call.
    fn drain_events(&mut self) -> Result<(), DriverError> {
        let mut reposted = false;
        for _ in 0..self.eventq.size() {
            let token = match self.eventq.poll_used() {
                Ok(token) => token,
                Err(VirtioError::NoCompletion) => break,
                Err(err) => return Err(err.as_driver_error()),
            };
            let slot = self
                .event_slots
                .get_mut(usize::from(token.head))
                .and_then(Option::take)
                .ok_or(DriverError::DeviceFault)?;
            if token.written as usize >= wire::event::LEN {
                let offset = usize::from(slot) * wire::event::LEN;
                let bytes = self
                    .events
                    .full_region_mut()
                    .get(offset..offset + wire::event::LEN)
                    .ok_or(DriverError::DeviceFault)?;
                let code = wire::read_u32(bytes, 0);
                let data = wire::read_u32(bytes, 4);
                self.fold_event(code, data);
            }
            self.post_event_slot(slot)?;
            reposted = true;
        }
        if reposted {
            self.eventq.kick(&mut self.transport);
        }
        Ok(())
    }

    /// Fold one device event into the pending interrupt causes.
    fn fold_event(&mut self, code: u32, data: u32) {
        let Ok(index) = u16::try_from(data) else {
            return;
        };
        if index >= MAX_DEVICE_ENDPOINTS {
            return;
        }
        let bit = 1u32 << index;
        match code {
            wire::event::PCM_PERIOD_ELAPSED => self.pending.period_elapsed |= bit,
            wire::event::PCM_XRUN => self.pending.xrun |= bit,
            wire::event::JACK_CONNECTED | wire::event::JACK_DISCONNECTED => {
                let state = if code == wire::event::JACK_CONNECTED {
                    JackState::Present
                } else {
                    JackState::Absent
                };
                // A jack record names a *jack*, and this driver relates jack
                // `n` to stream `n`; a device with fewer streams than jacks
                // simply has a connector nothing here reports.
                if let Some(stream) = self.streams.get_mut(index as usize) {
                    stream.jack = state;
                    self.pending.jack_changed |= bit;
                }
            }
            // An event code this driver does not model is ignored rather
            // than guessed at; the buffer is reposted either way.
            _ => {}
        }
    }

    /// Resolve an endpoint index onto its stream.
    fn stream(&self, endpoint: u16) -> Result<&Stream, DriverError> {
        self.streams
            .get(usize::from(endpoint))
            .ok_or(DriverError::NotFound)
    }

    /// Resolve an endpoint index onto its stream, mutably.
    fn stream_mut(&mut self, endpoint: u16) -> Result<&mut Stream, DriverError> {
        self.streams
            .get_mut(usize::from(endpoint))
            .ok_or(DriverError::NotFound)
    }

    /// Issue one `virtio_snd_pcm_hdr`-shaped stream command.
    fn stream_command(&mut self, code: u32, id: u32) -> Result<(), DriverError> {
        let mut request = [0u8; wire::PCM_HDR_LEN];
        wire::put_u32(&mut request, 0, code);
        wire::put_u32(&mut request, 4, id);
        self.control_request(&request, wire::HDR_LEN)?;
        Ok(())
    }
}

/// Turn a device format mask into the encodings this stack converts between.
///
/// Formats outside the closed set are dropped rather than refused: a device
/// offering an encoding the engine does not speak simply does not offer it to
/// the mixer, and it is the empty result that is a fault.
fn decode_formats(mask: u64) -> SampleFormats {
    let mut formats = SampleFormats::EMPTY;
    for (bit, format) in [
        (wire::format::U8, SampleFormat::U8),
        (wire::format::S16, SampleFormat::S16),
        (wire::format::S24_3, SampleFormat::S24),
        (wire::format::S24, SampleFormat::S24In32),
        (wire::format::S32, SampleFormat::S32),
        (wire::format::FLOAT, SampleFormat::F32),
    ] {
        if mask & (1u64 << bit) != 0 {
            formats = formats.with(format);
        }
    }
    formats
}

/// Turn a device rate mask into the discrete rates it clocks at.
fn decode_rates(mask: u64) -> Result<RateSupport, DriverError> {
    let mut rates: [Rate; tairix_abi::driver::audio::MAX_DEVICE_RATES] =
        [Rate::HZ_48000; tairix_abi::driver::audio::MAX_DEVICE_RATES];
    let mut count = 0;
    for (bit, hz) in wire::RATES {
        if mask & (1u64 << bit) == 0 {
            continue;
        }
        let Ok(rate) = Rate::new(*hz) else {
            continue;
        };
        // The ascending table is walked in order, so the set is ascending by
        // construction; a device offering more rates than the contract
        // carries keeps its lowest, which are the ones a converter is most
        // likely to be asked for.
        if count == rates.len() {
            break;
        }
        rates[count] = rate;
        count += 1;
    }
    if count == 0 {
        return Err(DriverError::DeviceFault);
    }
    RateSet::new(&rates[..count])
        .map(RateSupport::Discrete)
        .map_err(|_| DriverError::DeviceFault)
}

/// Turn a device channel-map record into a channel layout.
///
/// Returns [`None`] for a record naming a count the contract does not admit
/// or a position it does not name, so a map this stack could not derive a
/// matrix from is left unpublished rather than half-read.
fn decode_chmap(record: &[u8], channels: u8) -> Option<ChannelMap> {
    if channels == 0 || usize::from(channels) > MAX_CHANNELS {
        return None;
    }
    let mut positions = [ChannelPosition::Mono; MAX_CHANNELS];
    for (slot, position) in positions.iter_mut().enumerate().take(usize::from(channels)) {
        let raw = *record.get(wire::chmap_info::POSITIONS + slot)?;
        *position = match raw {
            wire::chmap::MONO => ChannelPosition::Mono,
            wire::chmap::FL => ChannelPosition::FrontLeft,
            wire::chmap::FR => ChannelPosition::FrontRight,
            wire::chmap::FC => ChannelPosition::FrontCentre,
            wire::chmap::LFE => ChannelPosition::LowFrequency,
            wire::chmap::RL => ChannelPosition::RearLeft,
            wire::chmap::RR => ChannelPosition::RearRight,
            wire::chmap::SL => ChannelPosition::SideLeft,
            wire::chmap::SR => ChannelPosition::SideRight,
            _ => return None,
        };
    }
    ChannelMap::new(&positions[..usize::from(channels)]).ok()
}

/// The conventional layout for `channels` channels, which is what a device
/// reporting a channel count and no positions has said.
///
/// Mono and stereo are unambiguous; wider counts follow the standard
/// interleave order every consumer format uses. A count with no conventional
/// reading is refused rather than guessed at.
fn conventional_map(channels: u8) -> Option<ChannelMap> {
    use ChannelPosition::{
        FrontCentre, FrontLeft, FrontRight, LowFrequency, Mono, RearLeft, RearRight, SideLeft,
        SideRight,
    };
    let positions: &[ChannelPosition] = match channels {
        1 => &[Mono],
        2 => &[FrontLeft, FrontRight],
        3 => &[FrontLeft, FrontRight, FrontCentre],
        4 => &[FrontLeft, FrontRight, RearLeft, RearRight],
        6 => &[
            FrontLeft,
            FrontRight,
            FrontCentre,
            LowFrequency,
            RearLeft,
            RearRight,
        ],
        8 => &[
            FrontLeft,
            FrontRight,
            FrontCentre,
            LowFrequency,
            RearLeft,
            RearRight,
            SideLeft,
            SideRight,
        ],
        _ => return None,
    };
    ChannelMap::new(positions).ok()
}

/// Turn a monotonic nanosecond reading into the instant a clock pair is
/// stamped with.
///
/// The reading's epoch is unspecified and only differences are meaningful,
/// which is exactly what a linear rate fit needs: a wall clock stepped by a
/// time-synchronisation service would corrupt every fit built across the
/// step, so the audio clock domain is deliberately monotonic.
fn monotonic_instant(nanos: u64) -> Time64 {
    const NANOS_PER_SEC: u64 = 1_000_000_000;
    let secs = i64::try_from(nanos / NANOS_PER_SEC).unwrap_or(i64::MAX);
    // The remainder is below a billion by construction, so the only way the
    // constructor can refuse is a value it cannot produce.
    let nanos = u32::try_from(nanos % NANOS_PER_SEC).unwrap_or(0);
    Time64::new(secs, nanos).unwrap_or(Time64::UNIX_EPOCH)
}

/// The device's own bit index for a sample format.
fn format_bit(format: SampleFormat) -> u8 {
    match format {
        SampleFormat::U8 => wire::format::U8,
        SampleFormat::S16 => wire::format::S16,
        SampleFormat::S24 => wire::format::S24_3,
        SampleFormat::S24In32 => wire::format::S24,
        SampleFormat::S32 => wire::format::S32,
        SampleFormat::F32 => wire::format::FLOAT,
    }
}

/// The device's own bit index for a rate, or [`None`] for one it cannot name.
fn rate_bit(rate: Rate) -> Option<u8> {
    wire::RATES
        .iter()
        .find(|(_, hz)| *hz == rate.hz())
        .map(|(bit, _)| *bit)
}

/// Driver entry point.
///
/// # Errors
///
/// * [`DriverError::PermissionDenied`] if the host did not grant
///   [`CapabilityId::DRV_LOAD`].
///
/// # Capabilities
///
/// Requires [`CapabilityId::DRV_LOAD`].
pub fn register(host: &dyn DriverHost) -> Result<DriverHandle, DriverError> {
    if !host.has_capability(CapabilityId::DRV_LOAD) {
        return Err(DriverError::PermissionDenied);
    }
    DriverHandle::from_raw(REGISTER_HANDLE_MARKER)
}

impl<T: Transport> Audio for VirtioSnd<'_, T> {
    fn device_facts(&self) -> Result<AudioDeviceFacts, DriverError> {
        let Ok(endpoints) = u16::try_from(self.streams.len()) else {
            return Err(DriverError::DeviceFault);
        };
        Ok(AudioDeviceFacts {
            endpoints,
            name: AudioName::new("Virtio Sound").map_err(|_| DriverError::DeviceFault)?,
        })
    }

    fn endpoint_facts(&self, endpoint: u16) -> Result<AudioEndpointFacts, DriverError> {
        let stream = self.stream(endpoint)?;
        let channel_map = match stream.published_map {
            Some(map) => map,
            None => conventional_map(stream.channels_max)
                .or_else(|| conventional_map(stream.channels_min))
                .ok_or(DriverError::DeviceFault)?,
        };
        let facts = AudioEndpointFacts {
            index: endpoint,
            direction: stream.direction,
            jack: stream.jack,
            formats: stream.formats,
            channel_map,
            rates: stream.rates,
            // The device states no period bounds of its own, so the driver's
            // are the contract's floor and this driver's own DMA ceiling —
            // both derived, neither hand-picked for a machine.
            min_period_frames: ring_bounds::MIN_FRAMES,
            max_period_frames: MAX_PERIOD_FRAMES,
            max_ring_frames: MAX_RING_FRAMES,
            // Volume is the engine's one model; the device exposes its own
            // only behind the control-element feature this driver does not
            // negotiate, so it honestly reports none and the mixer applies
            // the gain itself.
            gain: None,
            name: AudioName::new(match stream.direction {
                StreamDirection::Playback => "Output",
                StreamDirection::Capture => "Input",
            })
            .map_err(|_| DriverError::DeviceFault)?,
        };
        facts.validate().map_err(|_| DriverError::DeviceFault)?;
        Ok(facts)
    }

    fn configure(
        &mut self,
        endpoint: u16,
        params: &ConfigureParams,
    ) -> Result<ConfigureGrant, DriverError> {
        let facts = self.endpoint_facts(endpoint)?;
        let stream = self.stream(endpoint)?;
        if stream.running {
            return Err(DriverError::Busy);
        }
        let id = stream.id;
        // A device substitutes rather than refuses: what it cannot do
        // exactly, it answers with the nearest thing it can, and the mixer
        // owns the conversion the difference implies.
        let format = if stream.formats.contains(params.format) {
            params.format
        } else {
            // The widest encoding the device offers, so the substitution
            // never loses resolution the source had.
            [
                SampleFormat::F32,
                SampleFormat::S32,
                SampleFormat::S24In32,
                SampleFormat::S24,
                SampleFormat::S16,
                SampleFormat::U8,
            ]
            .into_iter()
            .find(|candidate| stream.formats.contains(*candidate))
            .ok_or(DriverError::DeviceFault)?
        };
        let rate = stream.rates.nearest(params.rate);
        let channels = params
            .channel_map
            .channels()
            .clamp(stream.channels_min, stream.channels_max);
        let channel_map = if channels == params.channel_map.channels() {
            params.channel_map
        } else {
            conventional_map(channels).ok_or(DriverError::Unsupported)?
        };
        let period_frames = params
            .period_frames
            .clamp(facts.min_period_frames, facts.max_period_frames)
            // A power-of-two period keeps the ring's own power-of-two depth a
            // whole number of periods, so a wrap never splits one.
            .next_power_of_two();
        let frame_bytes = format.bytes_per_sample() * usize::from(channels);
        let period_bytes = period_frames as usize * frame_bytes;
        if period_bytes > MAX_PERIOD_BYTES {
            return Err(DriverError::OutOfRange);
        }
        let Some(rate_index) = rate_bit(rate) else {
            return Err(DriverError::Unsupported);
        };
        let Ok(period_bytes_u32) = u32::try_from(period_bytes) else {
            return Err(DriverError::OutOfRange);
        };
        let Ok(buffer_bytes) = u32::try_from(period_bytes * PERIODS_IN_FLIGHT) else {
            return Err(DriverError::OutOfRange);
        };

        // Release first: a stream the device still holds programmed refuses
        // its own re-programming, and this path is reached on every
        // reconfiguration.
        let _ = self.release_stream(endpoint);

        let mut request = [0u8; wire::SET_PARAMS_LEN];
        wire::put_u32(&mut request, 0, wire::request::PCM_SET_PARAMS);
        wire::put_u32(&mut request, 4, id);
        wire::put_u32(&mut request, 8, buffer_bytes);
        wire::put_u32(&mut request, 12, period_bytes_u32);
        wire::put_u32(&mut request, 16, 0);
        request[20] = channels;
        request[21] = format_bit(format);
        request[22] = rate_index;
        self.control_request(&request, wire::HDR_LEN)?;
        self.stream_command(wire::request::PCM_PREPARE, id)?;

        // The period buffers are allocated here, once, and reused for every
        // period the stream ever moves: the per-period path allocates
        // nothing.
        let mut periods = Vec::new();
        periods
            .try_reserve_exact(PERIODS_IN_FLIGHT)
            .map_err(|_| DriverError::NoSpace)?;
        for _ in 0..PERIODS_IN_FLIGHT {
            let slab = self
                .host
                .alloc_dma_zeroed(wire::XFER_HDR_LEN + period_bytes + wire::XFER_STATUS_LEN)?;
            let mut dma = BounceBuffer::new(slab, BufferClass::NonSensitive);
            // The transfer header names the stream and never changes, so it
            // is written once rather than per period.
            wire::put_u32(dma.full_region_mut(), 0, id);
            periods.push(PeriodBuffer {
                dma,
                frames: 0,
                posted: None,
                returned: None,
            });
        }

        let grant = ConfigureGrant {
            rate,
            format,
            channel_map,
            period_frames,
            max_ring_frames: MAX_RING_FRAMES,
        };
        let stream = self.stream_mut(endpoint)?;
        stream.configured = Some(grant);
        stream.running = false;
        stream.draining = false;
        stream.transferred = Frames::ZERO;
        stream.latency_frames = 0;
        stream.xrun_frames = 0;
        stream.periods = periods;
        Ok(grant)
    }

    fn start(&mut self, endpoint: u16, at: Frames) -> Result<(), DriverError> {
        let stream = self.stream(endpoint)?;
        if stream.configured.is_none() {
            return Err(DriverError::DeviceFault);
        }
        let id = stream.id;
        self.stream_command(wire::request::PCM_START, id)?;
        let stream = self.stream_mut(endpoint)?;
        stream.running = true;
        stream.draining = false;
        // The mixer names the position its first frame belongs at, so the
        // stream's arithmetic describes the same timeline the client's does.
        stream.transferred = at;
        Ok(())
    }

    fn stop(&mut self, endpoint: u16, at: Frames) -> Result<(), DriverError> {
        let stream = self.stream(endpoint)?;
        if stream.configured.is_none() {
            return Err(DriverError::DeviceFault);
        }
        let id = stream.id;
        self.stream_command(wire::request::PCM_STOP, id)?;
        let stream = self.stream_mut(endpoint)?;
        stream.running = false;
        stream.draining = false;
        stream.transferred = at;
        stream.latency_frames = 0;
        Ok(())
    }

    fn drain(&mut self, endpoint: u16) -> Result<(), DriverError> {
        let stream = self.stream_mut(endpoint)?;
        if stream.configured.is_none() {
            return Err(DriverError::DeviceFault);
        }
        // No device command: the specification has no drain, so draining is
        // "accept no more frames and stop once the queued ones have gone",
        // which the service path completes.
        stream.draining = true;
        Ok(())
    }

    fn service(
        &mut self,
        endpoint: u16,
        ring: &mut PcmRing<'_>,
    ) -> Result<AudioServiced, DriverError> {
        self.drain_events()?;
        let stream = self.stream(endpoint)?;
        let grant = stream.configured.ok_or(DriverError::DeviceFault)?;
        // The ring the mixer mapped must be the shape the grant agreed, or
        // the frames it holds are not the frames this stream is programmed
        // for.
        if ring.geometry().format() != grant.format
            || ring.geometry().channels() != grant.channel_map.channels()
        {
            return Err(DriverError::BadMagic);
        }
        let direction = stream.direction;
        let transferred_before = stream.transferred;
        self.reap_transfers(endpoint, ring)?;
        match direction {
            StreamDirection::Playback => self.fill_playback(endpoint, ring)?,
            StreamDirection::Capture => self.post_capture(endpoint)?,
        }
        if direction == StreamDirection::Playback {
            self.finish_drain_if_played_out(endpoint, ring)?;
        }
        let stream = self.stream(endpoint)?;
        let moved = stream
            .transferred
            .get()
            .saturating_sub(transferred_before.get());
        let Ok(transferred) = u32::try_from(moved) else {
            return Err(DriverError::DeviceFault);
        };
        Ok(AudioServiced {
            transferred,
            running: stream.running,
            position: stream.position(),
            xrun_frames: stream.xrun_frames,
            sampled_at: monotonic_instant(self.clock.now_ns()),
        })
    }

    fn set_gain(&mut self, endpoint: u16, _millibel: i32, _mute: bool) -> Result<(), DriverError> {
        // The endpoint honestly reports no gain range, so refusing here is
        // the answer that makes the mixer apply the gain itself rather than
        // believe the hardware did.
        self.stream(endpoint)?;
        Err(DriverError::NotImplemented)
    }

    fn release(&mut self, endpoint: u16) -> Result<(), DriverError> {
        let stream = self.stream(endpoint)?;
        if stream.configured.is_none() {
            return Ok(());
        }
        if stream.running {
            let id = stream.id;
            let _ = self.stream_command(wire::request::PCM_STOP, id);
        }
        self.release_stream(endpoint)
    }

    fn take_interrupt(&mut self) -> Result<AudioInterrupt, DriverError> {
        self.transport.ack_interrupt();
        self.drain_events()?;
        // A completed transfer is a period boundary whether or not the device
        // also posted an event: a device that suppresses the event queue
        // would otherwise leave the mixer waiting for a wake that never
        // comes.
        for (index, stream) in self.streams.iter().enumerate() {
            if stream
                .periods
                .iter()
                .any(|p| p.posted.is_some() || p.returned.is_some())
            {
                if let Ok(bit) = u16::try_from(index) {
                    if bit < MAX_DEVICE_ENDPOINTS {
                        self.pending.period_elapsed |= 1u32 << bit;
                    }
                }
            }
        }
        Ok(core::mem::replace(&mut self.pending, AudioInterrupt::NONE))
    }

    fn set_event_interrupts(&mut self, enabled: bool) -> Result<(), DriverError> {
        // The device has no interrupt-mask register: suppressing the used
        // rings' interrupts is what the split-virtqueue layout offers, and it
        // is exactly the "stop waking me" the serve loop wants.
        self.txq.ring.suppress_used_interrupts(!enabled);
        self.rxq.ring.suppress_used_interrupts(!enabled);
        self.eventq.suppress_used_interrupts(!enabled);
        self.events_armed = enabled;
        Ok(())
    }
}

impl<T: Transport> VirtioSnd<'_, T> {
    /// Whether the device's event sources are armed (host-test access).
    #[must_use]
    pub fn events_armed(&self) -> bool {
        self.events_armed
    }

    /// Collect every completed transfer, advancing the stream's position by
    /// what the device consumed or produced.
    ///
    /// A capture completion also copies the captured frames into the shared
    /// ring, because the payload is only readable while the buffer is back in
    /// this driver's hands.
    /// Stop a draining endpoint once everything it held has reached the
    /// device: the shared ring is empty and no transfer is still in flight.
    ///
    /// Only this side can tell: the frames the mixer handed over live in
    /// this driver's own in-flight buffers, so a ring that has run dry is
    /// not yet a stream that has been heard. The report's `running` going
    /// false is what tells the mixer the drain is done.
    fn finish_drain_if_played_out(
        &mut self,
        endpoint: u16,
        ring: &mut PcmRing<'_>,
    ) -> Result<(), DriverError> {
        let stream = self.stream(endpoint)?;
        if !stream.draining || !stream.running {
            return Ok(());
        }
        let in_flight = stream.periods.iter().any(|period| period.posted.is_some());
        let readable = ring.readable_frames().map_err(|_| DriverError::BadMagic)?;
        if in_flight || readable != 0 {
            return Ok(());
        }
        let id = self.stream(endpoint)?.id;
        self.stream_command(wire::request::PCM_STOP, id)?;
        let stream = self.stream_mut(endpoint)?;
        stream.running = false;
        stream.draining = false;
        Ok(())
    }

    fn reap_transfers(&mut self, endpoint: u16, ring: &mut PcmRing<'_>) -> Result<(), DriverError> {
        let direction = self.stream(endpoint)?.direction;
        self.collect_transfers(direction)?;
        for slot in 0..self.stream(endpoint)?.periods.len() {
            let returned = self.stream_mut(endpoint)?.periods[slot].returned.take();
            if let Some(written) = returned {
                self.complete_transfer(endpoint, slot, written, ring)?;
            }
        }
        Ok(())
    }

    /// Collect every transfer the device has handed back on `direction`'s
    /// queue, marking it on the stream that posted it, or freeing it if a
    /// released stream lent it.
    fn collect_transfers(&mut self, direction: StreamDirection) -> Result<(), DriverError> {
        loop {
            match self.transfer_queue(direction).ring.poll_used() {
                Ok(token) => self.hand_back(direction, token)?,
                Err(VirtioError::NoCompletion) => return Ok(()),
                Err(err) => return Err(err.as_driver_error()),
            }
        }
    }

    /// The transfer queue every stream of `direction` shares.
    fn transfer_queue(&mut self, direction: StreamDirection) -> &mut TransferQueue {
        match direction {
            StreamDirection::Playback => &mut self.txq,
            StreamDirection::Capture => &mut self.rxq,
        }
    }

    /// Take back the period buffer `token` names on `direction`'s queue.
    fn hand_back(
        &mut self,
        direction: StreamDirection,
        token: UsedToken,
    ) -> Result<(), DriverError> {
        // The queue answers only for a chain it holds, and every chain it
        // holds was recorded when it was posted.
        let carried = self
            .transfer_queue(direction)
            .carried
            .get_mut(usize::from(token.head))
            .and_then(Option::take);
        match carried {
            Some(Transfer::Period { stream, slot }) => {
                let period = self
                    .streams
                    .get_mut(stream)
                    .and_then(|owner| owner.periods.get_mut(slot))
                    .ok_or(DriverError::DeviceFault)?;
                period.posted = None;
                period.returned = Some(token.written);
                Ok(())
            }
            // Back at last, so the lent buffer is freed here.
            Some(Transfer::Lent(_)) => Ok(()),
            None => Err(DriverError::DeviceFault),
        }
    }

    /// Have the device let go of `endpoint`'s buffers, then let go of them
    /// here: what the device has handed back is freed, and a buffer it still
    /// holds is lent until it does. The stream is left unprogrammed whatever
    /// the device answers.
    fn release_stream(&mut self, endpoint: u16) -> Result<(), DriverError> {
        let (id, direction) = {
            let stream = self.stream(endpoint)?;
            (stream.id, stream.direction)
        };
        let released = self.stream_command(wire::request::PCM_RELEASE, id);
        // A device completes every transfer it holds for a stream before it
        // answers a release, so all of them are collectable now.
        let collected = if released.is_ok() {
            self.collect_transfers(direction)
        } else {
            Ok(())
        };
        let stream = self.stream_mut(endpoint)?;
        stream.configured = None;
        stream.running = false;
        stream.draining = false;
        stream.latency_frames = 0;
        let periods = core::mem::take(&mut stream.periods);
        self.transfer_queue(direction).lend_held(periods);
        released.and(collected)
    }

    /// Fold one completed transfer buffer back into the stream.
    ///
    /// A capture completion also delivers: the payload is only readable while
    /// the buffer is back in this driver's hands, so the frames the device
    /// wrote are copied into the mixer's ring here. What the ring cannot hold
    /// is over-run — counted, never silently dropped.
    fn complete_transfer(
        &mut self,
        endpoint: u16,
        slot: usize,
        written: u32,
        ring: &mut PcmRing<'_>,
    ) -> Result<(), DriverError> {
        let frame_bytes = self
            .stream(endpoint)?
            .frame_bytes()
            .ok_or(DriverError::DeviceFault)?;
        let direction = self.stream(endpoint)?.direction;
        let stream = self
            .streams
            .get_mut(usize::from(endpoint))
            .ok_or(DriverError::NotFound)?;
        let period = stream
            .periods
            .get_mut(slot)
            .ok_or(DriverError::DeviceFault)?;
        let asked = period.frames;
        period.frames = 0;
        let status_offset = period.dma.capacity() - wire::XFER_STATUS_LEN;
        let region = period.dma.full_region_mut();
        let status = wire::read_u32(region, status_offset);
        let latency_bytes = wire::read_u32(region, status_offset + 4);
        if status != wire::status::OK {
            return Err(DriverError::DeviceFault);
        }
        stream.latency_frames = u64::from(latency_bytes) / frame_bytes as u64;
        if direction == StreamDirection::Playback {
            // A playback buffer's frames were credited when it was posted, so
            // its completion only refreshes the latency the reported position
            // is derived from.
            return Ok(());
        }
        // The device reports what it wrote, status word included; a capture
        // payload is whatever of that is not the status.
        let captured_bytes = (written as usize)
            .saturating_sub(wire::XFER_STATUS_LEN)
            .min(asked as usize * frame_bytes);
        let captured = u32::try_from(captured_bytes / frame_bytes).unwrap_or(0);
        if captured == 0 {
            return Ok(());
        }
        let delivered_bytes = captured as usize * frame_bytes;
        let payload = region
            .get(wire::XFER_HDR_LEN..wire::XFER_HDR_LEN + delivered_bytes)
            .ok_or(DriverError::DeviceFault)?;
        let delivered = ring.write(payload).map_err(|_| DriverError::BadMagic)?;
        stream.transferred = stream
            .transferred
            .checked_add(u64::from(delivered))
            .ok_or(DriverError::OutOfRange)?;
        // The mixer did not drain fast enough, so the tail of this period is
        // gone. Counted, because a capture that silently loses frames is a
        // recording with an invisible edit in it.
        stream.xrun_frames += u64::from(captured - delivered);
        Ok(())
    }

    /// Fill and post playback periods, substituting silence only where the
    /// device would otherwise run dry.
    ///
    /// Three cases, and nothing else posts a buffer: a whole period is
    /// available and goes as it is; the stream is draining and what is left
    /// goes as a short final transfer; or the stream is running, the device
    /// has nothing left in flight, and the ring is short — then and only then
    /// the missing frames are silence, counted as lost. Padding a period the
    /// device has not yet asked for would manufacture a glitch out of frames
    /// that were merely going to arrive in time.
    fn fill_playback(&mut self, endpoint: u16, ring: &mut PcmRing<'_>) -> Result<(), DriverError> {
        let (period_frames, frame_bytes, running, draining, mut in_flight) = {
            let stream = self.stream(endpoint)?;
            let grant = stream.configured.ok_or(DriverError::DeviceFault)?;
            (
                grant.period_frames,
                stream.frame_bytes().ok_or(DriverError::DeviceFault)?,
                stream.running,
                stream.draining,
                stream
                    .periods
                    .iter()
                    .filter(|period| period.posted.is_some())
                    .count(),
            )
        };
        for slot in 0..PERIODS_IN_FLIGHT {
            if self.stream(endpoint)?.periods[slot].posted.is_some() {
                continue;
            }
            // Periods a refused release lent the device still hold
            // descriptors, and frames taken for a period the ring cannot hold
            // would be lost: they wait in the ring instead.
            if usize::from(self.txq.ring.free_count()) < TRANSFER_CHAIN_DESCRIPTORS {
                break;
            }
            let readable = ring.readable_frames().map_err(|_| DriverError::BadMagic)?;
            let (take, shortfall) = if readable >= period_frames {
                (period_frames, 0)
            } else if draining && readable > 0 {
                // The tail of a drain is a short transfer, not a padded one:
                // nothing was lost, so nothing is counted, and the device
                // plays exactly what remains.
                (readable, 0)
            } else if running && !draining && in_flight == 0 {
                (readable, period_frames - readable)
            } else {
                break;
            };
            let carried = take + shortfall;
            let payload_bytes = carried as usize * frame_bytes;
            let Self {
                txq,
                transport,
                streams,
                ..
            } = self;
            let stream = streams
                .get_mut(usize::from(endpoint))
                .ok_or(DriverError::NotFound)?;
            let quiet = stream
                .configured
                .map_or(0, |grant| grant.format.silence_byte());
            let period = &mut stream.periods[slot];
            let region = period.dma.full_region_mut();
            let Some(payload) =
                region.get_mut(wire::XFER_HDR_LEN..wire::XFER_HDR_LEN + payload_bytes)
            else {
                return Err(DriverError::DeviceFault);
            };
            let taken_bytes = take as usize * frame_bytes;
            let read = ring
                .read(&mut payload[..taken_bytes])
                .map_err(|_| DriverError::BadMagic)?;
            if read != take {
                return Err(DriverError::DeviceFault);
            }
            if shortfall != 0 {
                // Silence for exactly the frames the mixer could not supply,
                // counted, so the position stays exact and the loss is a
                // number rather than a drift.
                payload[taken_bytes..].fill(quiet);
                stream.xrun_frames += u64::from(shortfall);
            }
            txq.post(transport, stream, slot, carried, payload_bytes)?;
            in_flight += 1;
        }
        Ok(())
    }

    /// Post every free capture period back to the device, and move whatever
    /// arrived into the shared ring.
    fn post_capture(&mut self, endpoint: u16) -> Result<(), DriverError> {
        let (period_frames, frame_bytes) = {
            let stream = self.stream(endpoint)?;
            let grant = stream.configured.ok_or(DriverError::DeviceFault)?;
            (
                grant.period_frames,
                stream.frame_bytes().ok_or(DriverError::DeviceFault)?,
            )
        };
        let period_bytes = period_frames as usize * frame_bytes;
        for slot in 0..PERIODS_IN_FLIGHT {
            if self.stream(endpoint)?.periods[slot].posted.is_some() {
                continue;
            }
            // As for playback: lent periods may still hold the room.
            if usize::from(self.rxq.ring.free_count()) < TRANSFER_CHAIN_DESCRIPTORS {
                break;
            }
            let Self {
                rxq,
                transport,
                streams,
                ..
            } = self;
            let stream = streams
                .get_mut(usize::from(endpoint))
                .ok_or(DriverError::NotFound)?;
            rxq.post(transport, stream, slot, period_frames, period_bytes)?;
        }
        Ok(())
    }
}

impl<T: Transport> Drop for VirtioSnd<'_, T> {
    /// Reset the device before its memory goes: a device that will not confirm
    /// may still master its rings, pools and periods, which are then held for
    /// the kernel to quarantine when the driver exits.
    fn drop(&mut self) {
        if self.transport.reset().is_err() {
            self.controlq.withhold();
            self.eventq.withhold();
            self.txq.withhold();
            self.rxq.withhold();
            self.control.withhold();
            self.events.withhold();
            for period in self
                .streams
                .iter_mut()
                .flat_map(|stream| stream.periods.iter_mut())
            {
                period.dma.withhold();
            }
        }
    }
}

#[cfg(test)]
mod tests;
