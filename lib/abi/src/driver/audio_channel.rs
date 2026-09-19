//! `audiochan-v1`: the device-channel control plane between the audio mixer
//! service and an audio driver process (`plans/SOUND.md`).
//!
//! [`audio_ring`](super::audio_ring) defines the *in-region* PCM transport;
//! this module defines the *IPC control plane* that establishes and drives
//! that region. The driver owns the device — its registers, its DMA window,
//! its interrupt — and serves a call endpoint; the mixer is the one client,
//! and it owns the shared sample region.
//!
//! # The shape, and why it is this one
//!
//! The network device channel established the pattern and audio follows it:
//! the driver serves, the client `shm_create`s the region, `shm_grant`s it to
//! the driver's endpoint, and forwards the unforgeable grant handle in
//! [`AudioChannelRequest::Attach`]. The driver `shm_map`s exactly that one
//! region (`SHM_MAP` is owner-checked, so the handle is useless to a
//! bystander — no ambient authority).
//!
//! Configuration comes *before* attachment, which is the one deliberate
//! difference: a device answers [`AudioChannelRequest::Configure`] with what
//! it can actually meet, and the ring's geometry is derived from that answer.
//! A client that asked for a rate the converter does not have gets told the
//! rate it will get rather than a refusal, and there is exactly one place the
//! two sides compute the region's shape from ([`ConfigureGrant::geometry`]),
//! so they cannot disagree about how large it is.
//!
//! # The driver copies, and that is the point
//!
//! Once per period the driver copies between the shared ring and its own DMA
//! buffer. A zero-copy arrangement would mean publishing the driver's DMA
//! window to another process; the driver owning that window absolutely is
//! worth far more than the copy costs, which at 48 kHz stereo 32-bit and a
//! five-millisecond period is under 400 KiB/s.
//!
//! # Nothing spins
//!
//! Between doorbells the driver parks on its device interrupt. When a period
//! elapses it wakes the mixer with an [`AudioChannelNotify`] `ipc_send` to the
//! attach port, and the mixer — parked on that port in its wait set — issues
//! the next [`AudioChannelRequest::Service`]. The device's own period
//! interrupt is the only timer in the stack.
//!
//! # Fail closed
//!
//! Every decode is total and validates whole: an unknown magic, version or
//! operation byte, a dirty reserved field, an endpoint index past the device's
//! own bound, an out-of-range geometry, or a notification carrying a field its
//! kind does not define refuses with one typed [`Errno`] rather than guessing.

use super::audio::{
    ring_bounds, AudioDeviceFacts, AudioEndpointFacts, ChannelMap, Frames, JackState, Rate,
    SampleFormat, AUDIO_DEVICE_FACTS_WIRE_LEN, AUDIO_ENDPOINT_FACTS_WIRE_LEN, CHANNEL_MAP_WIRE_LEN,
    MAX_DEVICE_ENDPOINTS,
};
use super::audio_ring::PcmGeometry;
use crate::le::{put_i32, put_u16, put_u32, put_u64, read_i32, read_u16, read_u32, read_u64};
use crate::time::Time64;
use crate::Errno;

/// Magic number identifying a device-channel request (`"ACHR"`).
pub const AUDIO_CHANNEL_REQUEST_MAGIC: u32 = u32::from_le_bytes(*b"ACHR");

/// Magic number identifying a device-channel notification (`"ACHN"`).
pub const AUDIO_CHANNEL_NOTIFY_MAGIC: u32 = u32::from_le_bytes(*b"ACHN");

/// The `audiochan-v1` protocol version.
pub const AUDIO_CHANNEL_VERSION_V1: u16 = 1;

/// Base of the reserved device-channel call-endpoint id block
/// (`"ACHAN\0\0\0"` little-endian). Each audio driver process claims the first
/// free id in
/// `AUDIO_CHANNEL_ENDPOINT_BASE .. AUDIO_CHANNEL_ENDPOINT_BASE + AUDIO_CHANNEL_ENDPOINT_COUNT`
/// by binding it, so two audio drivers never collide on an id without a
/// central allocator.
///
/// The block is a reserved rendezvous
/// ([`crate::ipc::is_reserved_endpoint`]): binding any id in it requires
/// [`CapabilityId::IPC_BIND_PRIVILEGED`](crate::CapabilityId::IPC_BIND_PRIVILEGED),
/// so an unprivileged squatter cannot bind one first and impersonate the
/// driver to the mixer. The driver additionally binds it **restricted
/// sender** on the audio-device capability the mixer alone holds
/// (`plans/SOUND.md`), so the kernel refuses at dispatch every caller but the
/// mixer and the driver never re-checks.
pub const AUDIO_CHANNEL_ENDPOINT_BASE: u64 = u64::from_le_bytes(*b"ACHAN\0\0\0");

/// Number of concurrently-bindable device-channel endpoint ids: the most
/// audio driver processes the mixer serves at once. A fixed validation bound
/// on the reserved block, not a device-count capacity.
pub const AUDIO_CHANNEL_ENDPOINT_COUNT: u64 = 16;

/// Device-tree-style `compatible` model name of the hardware-tree node an
/// audio driver publishes to advertise the device-channel endpoint it claimed.
///
/// The discovery half of this contract, and its single definition: a driver
/// process stamps this key on the child node it emits, the device manager
/// recognises a node carrying it as a bound audio device's channel rather than
/// a device still awaiting a driver, and hands its endpoint to the mixer.
/// Defined beside the endpoint block so the key emitted and the key looked for
/// can never drift.
pub const AUDIOCHAN_NODE_COMPATIBLE: &[u8] = b"tairix,audiochan";

/// Whether `id` is one of the reserved device-channel endpoint ids.
#[must_use]
pub const fn is_audio_channel_endpoint(id: u64) -> bool {
    id >= AUDIO_CHANNEL_ENDPOINT_BASE
        && id < AUDIO_CHANNEL_ENDPOINT_BASE + AUDIO_CHANNEL_ENDPOINT_COUNT
}

/// High tag of a mixer-owned device-channel notify-port id (see
/// [`notify_endpoint_for`]).
const AUDIO_NOTIFY_ENDPOINT_TAG: u64 = 0x4141_0000_0000_0000;

/// The notify-mailbox endpoint id the mixer binds for one attached device
/// channel and passes to the driver in [`AttachParams::notify_endpoint`].
///
/// It packs the mixer's own kernel task id `pid` and the per-channel slot
/// `index` under a fixed high tag: a distinct, collision-free,
/// **non-reserved** id, so the mixer `port_bind`s it without
/// [`CapabilityId::IPC_BIND_PRIVILEGED`](crate::CapabilityId::IPC_BIND_PRIVILEGED)
/// and two channels can never disagree about the id space. The mailbox is
/// owner-only to receive, so the driver only needs the number — it cannot
/// receive the wakes it sends, and a bystander cannot steal them; a spurious
/// notify at worst costs one extra [`AudioChannelRequest::Service`] doorbell.
///
/// `index` is bounded by [`AUDIO_CHANNEL_ENDPOINT_COUNT`] and occupies the low
/// byte; `pid` occupies the next 40 bits, which is the whole of
/// [`crate::PID_MAX`], so the three fields tile the word exactly and no pid
/// can reach the tag.
#[must_use]
pub const fn notify_endpoint_for(pid: u64, index: u64) -> u64 {
    AUDIO_NOTIFY_ENDPOINT_TAG | ((pid & crate::PID_MAX) << 8) | (index & 0xFF)
}

/// Operation discriminants (the request's seventh byte).
mod op {
    pub const FACTS: u8 = 1;
    pub const ENDPOINT_FACTS: u8 = 2;
    pub const CONFIGURE: u8 = 3;
    pub const ATTACH: u8 = 4;
    pub const START: u8 = 5;
    pub const STOP: u8 = 6;
    pub const DRAIN: u8 = 7;
    pub const SERVICE: u8 = 8;
    pub const GAIN: u8 = 9;
    pub const DETACH: u8 = 10;
}

/// Fixed request header: magic (4) + version (2) + op (1) + reserved (1).
const HEADER_LEN: usize = 8;

/// Byte offsets within the body of an endpoint-scoped request that carries
/// nothing else: the index and a reserved pair.
mod endpoint_body {
    pub const INDEX: usize = 0;
    pub const LEN: usize = 4;
}

/// Byte offsets within the [`AudioChannelRequest::Configure`] body.
mod configure {
    use super::CHANNEL_MAP_WIRE_LEN;

    pub const INDEX: usize = 0;
    pub const FORMAT: usize = 2;
    pub const RESERVED0: usize = 3;
    pub const RATE: usize = 4;
    pub const PERIOD: usize = 8;
    pub const CHANNEL_MAP: usize = 12;
    pub const RESERVED1: usize = CHANNEL_MAP + CHANNEL_MAP_WIRE_LEN;
    pub const LEN: usize = RESERVED1 + 3;
}

/// Byte offsets within the [`AudioChannelRequest::Attach`] body.
mod attach {
    pub const INDEX: usize = 0;
    pub const RESERVED: usize = 2;
    pub const RING_FRAMES: usize = 4;
    pub const GRANT: usize = 8;
    pub const NOTIFY: usize = 16;
    pub const LEN: usize = 24;
}

/// Byte offsets within the transport-control (`Start` / `Stop`) body.
mod transport {
    pub const INDEX: usize = 0;
    pub const RESERVED: usize = 2;
    pub const AT: usize = 8;
    pub const LEN: usize = 16;
}

/// Byte offsets within the [`AudioChannelRequest::Gain`] body.
mod gain {
    pub const INDEX: usize = 0;
    pub const MUTE: usize = 2;
    pub const RESERVED: usize = 3;
    pub const MILLIBEL: usize = 4;
    pub const LEN: usize = 8;
}

/// Largest device-channel request frame: the header plus the widest body. A
/// fixed validation bound sizing the buffer both sides pin for the control
/// endpoint.
pub const AUDIO_CHANNEL_MAX_REQUEST: usize = HEADER_LEN + largest_body();

/// The widest request body, so [`AUDIO_CHANNEL_MAX_REQUEST`] tracks whichever
/// operation grows rather than needing a hand-updated comparison.
const fn largest_body() -> usize {
    let mut largest = endpoint_body::LEN;
    if configure::LEN > largest {
        largest = configure::LEN;
    }
    if attach::LEN > largest {
        largest = attach::LEN;
    }
    if transport::LEN > largest {
        largest = transport::LEN;
    }
    if gain::LEN > largest {
        largest = gain::LEN;
    }
    largest
}

/// One device-channel control operation the mixer issues to the driver's
/// endpoint. Decoded fail-closed from an untrusted frame; the driver acts only
/// on a fully-validated value.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AudioChannelRequest {
    /// Report what the device is, and how many endpoints it presents.
    Facts,
    /// Report what one sink or source of the device can do.
    EndpointFacts {
        /// The endpoint's index, below the reported endpoint count.
        endpoint: u16,
    },
    /// Program an endpoint's rate, format, channel layout and period, at a
    /// period boundary. The reply is what the device could actually meet.
    Configure(ConfigureParams),
    /// Hand over the granted sample region and name the notify port.
    Attach(AttachParams),
    /// Begin clocking the endpoint at an exact frame position.
    Start {
        /// The endpoint to start.
        endpoint: u16,
        /// The stream position its first frame belongs at.
        at: Frames,
    },
    /// Stop clocking the endpoint at an exact frame position, keeping the
    /// position so a resume is exact.
    Stop {
        /// The endpoint to stop.
        endpoint: u16,
        /// The stream position to stop at.
        at: Frames,
    },
    /// Play out everything already queued, then stop.
    Drain {
        /// The endpoint to drain.
        endpoint: u16,
    },
    /// The doorbell: move one period between the shared ring and the device,
    /// and report the clock pair.
    Service {
        /// The endpoint to service.
        endpoint: u16,
    },
    /// Set the endpoint's hardware gain and mute.
    Gain {
        /// The endpoint to set.
        endpoint: u16,
        /// Gain in hundredths of a decibel; the device quantises it into its
        /// own reported range.
        millibel: i32,
        /// Whether the endpoint is muted, independently of the gain.
        mute: bool,
    },
    /// Release the channel: unmap the region and forget the notify port.
    Detach {
        /// The endpoint to detach.
        endpoint: u16,
    },
}

/// The parameters of an [`AudioChannelRequest::Configure`].
///
/// There is no separate channel count: the channel map carries it, so the two
/// cannot disagree.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ConfigureParams {
    /// The endpoint to program.
    pub endpoint: u16,
    /// The rate to clock it at.
    pub rate: Rate,
    /// The sample encoding to run it in.
    pub format: SampleFormat,
    /// The channel layout to interleave.
    pub channel_map: ChannelMap,
    /// Frames the device should interrupt on, within its reported bounds.
    pub period_frames: u32,
}

/// What the device could actually meet, and therefore what the ring is shaped
/// from.
///
/// A device that cannot do the asked-for rate answers the rate it will run at
/// rather than refusing, so a client adapts instead of failing. What it may
/// *not* do is answer something the request never mentioned: the mixer
/// compares the grant with what it asked for and re-derives the conversion it
/// then owes.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ConfigureGrant {
    /// The rate the device will clock at.
    pub rate: Rate,
    /// The sample encoding it will run in.
    pub format: SampleFormat,
    /// The channel layout it will interleave.
    pub channel_map: ChannelMap,
    /// Frames it will interrupt on.
    pub period_frames: u32,
    /// Most frames the ring may hold for this configuration.
    pub max_ring_frames: u32,
}

impl ConfigureGrant {
    /// The ring shape for `ring_frames` frames of this configuration.
    ///
    /// The one definition both sides size the shared region from: the mixer
    /// creates a region of exactly [`PcmGeometry::region_len`] bytes and the
    /// driver binds a ring of exactly this shape over it, so no second
    /// derivation exists to drift.
    ///
    /// # Errors
    ///
    /// * [`Errno::OutOfRange`] — `ring_frames` exceeds
    ///   [`Self::max_ring_frames`], cannot hold one period, or falls outside
    ///   what [`PcmGeometry::new`] admits.
    pub const fn geometry(&self, ring_frames: u32) -> Result<PcmGeometry, Errno> {
        if ring_frames > self.max_ring_frames || ring_frames < self.period_frames {
            return Err(Errno::OutOfRange);
        }
        PcmGeometry::new(ring_frames, self.format, self.channel_map.channels())
    }
}

/// The parameters of an [`AudioChannelRequest::Attach`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct AttachParams {
    /// The endpoint the region carries frames for.
    pub endpoint: u16,
    /// Frames the ring holds, agreed through [`ConfigureGrant::geometry`].
    pub ring_frames: u32,
    /// The unforgeable `shm_grant` handle the mixer minted for the driver's
    /// endpoint; the driver `shm_map`s exactly this region.
    pub region_grant: u64,
    /// The numeric IPC endpoint the driver `ipc_send`s an
    /// [`AudioChannelNotify`] to when a period elapses. The mixer chose it,
    /// `port_bind`ed it, and parks on it in its wait set; the driver only
    /// sends to the number.
    pub notify_endpoint: u64,
}

impl AudioChannelRequest {
    /// Largest encoded request frame.
    pub const MAX_WIRE_LEN: usize = AUDIO_CHANNEL_MAX_REQUEST;

    /// The operation's wire discriminant byte.
    const fn op_byte(&self) -> u8 {
        match self {
            Self::Facts => op::FACTS,
            Self::EndpointFacts { .. } => op::ENDPOINT_FACTS,
            Self::Configure(_) => op::CONFIGURE,
            Self::Attach(_) => op::ATTACH,
            Self::Start { .. } => op::START,
            Self::Stop { .. } => op::STOP,
            Self::Drain { .. } => op::DRAIN,
            Self::Service { .. } => op::SERVICE,
            Self::Gain { .. } => op::GAIN,
            Self::Detach { .. } => op::DETACH,
        }
    }

    /// Encoded length of this operation's frame.
    const fn wire_len(&self) -> usize {
        HEADER_LEN
            + match self {
                Self::Facts => 0,
                Self::EndpointFacts { .. }
                | Self::Drain { .. }
                | Self::Service { .. }
                | Self::Detach { .. } => endpoint_body::LEN,
                Self::Configure(_) => configure::LEN,
                Self::Attach(_) => attach::LEN,
                Self::Start { .. } | Self::Stop { .. } => transport::LEN,
                Self::Gain { .. } => gain::LEN,
            }
    }

    /// Encode `self` into `out`, returning the number of bytes written.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] if `out` cannot hold the encoded frame.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Errno> {
        let len = self.wire_len();
        let Some(frame) = out.get_mut(..len) else {
            return Err(Errno::BufferTooSmall);
        };
        frame.fill(0);
        put_u32(frame, 0, AUDIO_CHANNEL_REQUEST_MAGIC);
        put_u16(frame, 4, AUDIO_CHANNEL_VERSION_V1);
        frame[6] = self.op_byte();
        let body = &mut frame[HEADER_LEN..];
        match self {
            Self::Facts => {}
            Self::EndpointFacts { endpoint }
            | Self::Drain { endpoint }
            | Self::Service { endpoint }
            | Self::Detach { endpoint } => put_u16(body, endpoint_body::INDEX, *endpoint),
            Self::Configure(params) => encode_configure(body, params),
            Self::Attach(params) => encode_attach(body, params),
            Self::Start { endpoint, at } | Self::Stop { endpoint, at } => {
                put_u16(body, transport::INDEX, *endpoint);
                put_u64(body, transport::AT, at.get());
            }
            Self::Gain {
                endpoint,
                millibel,
                mute,
            } => {
                put_u16(body, gain::INDEX, *endpoint);
                body[gain::MUTE] = u8::from(*mute);
                put_i32(body, gain::MILLIBEL, *millibel);
            }
        }
        Ok(len)
    }

    /// Decode a request frame, fail-closed.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — shorter than the operation requires.
    /// * [`Errno::BadMagic`] — wrong magic or a dirty reserved field.
    /// * [`Errno::AbiVersionUnsupported`] — not
    ///   [`AUDIO_CHANNEL_VERSION_V1`].
    /// * [`Errno::OutOfRange`] — an unknown operation byte, an endpoint index
    ///   past [`MAX_DEVICE_ENDPOINTS`], or an out-of-range embedded value.
    pub fn decode(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < HEADER_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if read_u32(bytes, 0) != AUDIO_CHANNEL_REQUEST_MAGIC {
            return Err(Errno::BadMagic);
        }
        if read_u16(bytes, 4) != AUDIO_CHANNEL_VERSION_V1 {
            return Err(Errno::AbiVersionUnsupported);
        }
        if bytes[7] != 0 {
            return Err(Errno::BadMagic);
        }
        let op = bytes[6];
        if op == op::FACTS {
            return Ok(Self::Facts);
        }
        let body = body_of(bytes, body_len(op)?)?;
        match op {
            op::ENDPOINT_FACTS => Ok(Self::EndpointFacts {
                endpoint: decode_endpoint_body(body)?,
            }),
            op::DRAIN => Ok(Self::Drain {
                endpoint: decode_endpoint_body(body)?,
            }),
            op::SERVICE => Ok(Self::Service {
                endpoint: decode_endpoint_body(body)?,
            }),
            op::DETACH => Ok(Self::Detach {
                endpoint: decode_endpoint_body(body)?,
            }),
            op::CONFIGURE => Ok(Self::Configure(decode_configure(body)?)),
            op::ATTACH => Ok(Self::Attach(decode_attach(body)?)),
            op::START | op::STOP => {
                let (endpoint, at) = decode_transport(body)?;
                if op == op::START {
                    Ok(Self::Start { endpoint, at })
                } else {
                    Ok(Self::Stop { endpoint, at })
                }
            }
            op::GAIN => decode_gain(body),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// Body length of the operation `op` names.
const fn body_len(op: u8) -> Result<usize, Errno> {
    match op {
        op::ENDPOINT_FACTS | op::DRAIN | op::SERVICE | op::DETACH => Ok(endpoint_body::LEN),
        op::CONFIGURE => Ok(configure::LEN),
        op::ATTACH => Ok(attach::LEN),
        op::START | op::STOP => Ok(transport::LEN),
        op::GAIN => Ok(gain::LEN),
        _ => Err(Errno::OutOfRange),
    }
}

/// The `len`-byte body of a request frame.
fn body_of(bytes: &[u8], len: usize) -> Result<&[u8], Errno> {
    bytes
        .get(HEADER_LEN..HEADER_LEN + len)
        .ok_or(Errno::BufferTooSmall)
}

/// An endpoint index, refused when it names an endpoint no device could have.
fn checked_endpoint(index: u16) -> Result<u16, Errno> {
    if index >= MAX_DEVICE_ENDPOINTS {
        return Err(Errno::OutOfRange);
    }
    Ok(index)
}

fn decode_endpoint_body(body: &[u8]) -> Result<u16, Errno> {
    if read_u16(body, endpoint_body::INDEX + 2) != 0 {
        return Err(Errno::BadMagic);
    }
    checked_endpoint(read_u16(body, endpoint_body::INDEX))
}

fn encode_configure(body: &mut [u8], params: &ConfigureParams) {
    put_u16(body, configure::INDEX, params.endpoint);
    body[configure::FORMAT] = params.format.as_u8();
    put_u32(body, configure::RATE, params.rate.hz());
    put_u32(body, configure::PERIOD, params.period_frames);
    body[configure::CHANNEL_MAP..configure::CHANNEL_MAP + CHANNEL_MAP_WIRE_LEN]
        .copy_from_slice(&params.channel_map.to_wire());
}

fn decode_configure(body: &[u8]) -> Result<ConfigureParams, Errno> {
    if body[configure::RESERVED0] != 0 || body[configure::RESERVED1..].iter().any(|b| *b != 0) {
        return Err(Errno::BadMagic);
    }
    let period_frames = read_u32(body, configure::PERIOD);
    if period_frames == 0 || period_frames > ring_bounds::MAX_FRAMES {
        return Err(Errno::OutOfRange);
    }
    Ok(ConfigureParams {
        endpoint: checked_endpoint(read_u16(body, configure::INDEX))?,
        rate: Rate::new(read_u32(body, configure::RATE))?,
        format: SampleFormat::from_u8(body[configure::FORMAT])?,
        channel_map: ChannelMap::from_wire(&body[configure::CHANNEL_MAP..])?,
        period_frames,
    })
}

fn encode_attach(body: &mut [u8], params: &AttachParams) {
    put_u16(body, attach::INDEX, params.endpoint);
    put_u32(body, attach::RING_FRAMES, params.ring_frames);
    put_u64(body, attach::GRANT, params.region_grant);
    put_u64(body, attach::NOTIFY, params.notify_endpoint);
}

fn decode_attach(body: &[u8]) -> Result<AttachParams, Errno> {
    if read_u16(body, attach::RESERVED) != 0 {
        return Err(Errno::BadMagic);
    }
    let ring_frames = read_u32(body, attach::RING_FRAMES);
    if !(ring_bounds::MIN_FRAMES..=ring_bounds::MAX_FRAMES).contains(&ring_frames)
        || !ring_frames.is_power_of_two()
    {
        return Err(Errno::OutOfRange);
    }
    let notify_endpoint = read_u64(body, attach::NOTIFY);
    // A notify port naming a reserved rendezvous would turn the driver into a
    // proxy for wakes at a system service. The mixer's own
    // `notify_endpoint_for` never produces one, so refusing costs nothing.
    if crate::ipc::is_reserved_endpoint(notify_endpoint) {
        return Err(Errno::OutOfRange);
    }
    Ok(AttachParams {
        endpoint: checked_endpoint(read_u16(body, attach::INDEX))?,
        ring_frames,
        region_grant: read_u64(body, attach::GRANT),
        notify_endpoint,
    })
}

fn decode_transport(body: &[u8]) -> Result<(u16, Frames), Errno> {
    if body[transport::RESERVED..transport::AT]
        .iter()
        .any(|b| *b != 0)
    {
        return Err(Errno::BadMagic);
    }
    Ok((
        checked_endpoint(read_u16(body, transport::INDEX))?,
        Frames::new(read_u64(body, transport::AT)),
    ))
}

fn decode_gain(body: &[u8]) -> Result<AudioChannelRequest, Errno> {
    if body[gain::RESERVED] != 0 {
        return Err(Errno::BadMagic);
    }
    let mute = match body[gain::MUTE] {
        0 => false,
        1 => true,
        _ => return Err(Errno::OutOfRange),
    };
    Ok(AudioChannelRequest::Gain {
        endpoint: checked_endpoint(read_u16(body, gain::INDEX))?,
        millibel: read_i32(body, gain::MILLIBEL),
        mute,
    })
}

/// Byte offsets within an [`AudioServiceReport`] payload.
mod service {
    pub const TRANSFERRED: usize = 0;
    pub const RUNNING: usize = 4;
    pub const RESERVED: usize = 5;
    pub const POSITION: usize = 8;
    pub const XRUN_FRAMES: usize = 16;
    pub const SAMPLED_AT: usize = 24;
    pub const LEN: usize = 36;
}

/// What one [`AudioChannelRequest::Service`] doorbell moved, and where the
/// device's clock stands.
///
/// The `(position, sampled_at)` pair is the clock the whole stack is built on:
/// the mixer maintains a linear fit of it per device, so a client writing at a
/// frame position is doing exact arithmetic rather than guessing at latency.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct AudioServiceReport {
    /// Frames moved between the shared ring and the device this call.
    pub transferred: u32,
    /// Whether the device is clocking.
    pub running: bool,
    /// The device's frame position when it was sampled.
    pub position: Frames,
    /// Frames lost to under- or over-run since the endpoint was configured.
    /// Cumulative, so a consumer keeps the latest value it saw.
    pub xrun_frames: u64,
    /// When [`Self::position`] was sampled.
    pub sampled_at: Time64,
}

/// Wire length of the `Facts` reply: a status word then the payload (zeroed on
/// refusal).
pub const AUDIO_CHANNEL_FACTS_REPLY_LEN: usize = 4 + AUDIO_DEVICE_FACTS_WIRE_LEN;

/// Wire length of the `EndpointFacts` reply.
pub const AUDIO_CHANNEL_ENDPOINT_REPLY_LEN: usize = 4 + AUDIO_ENDPOINT_FACTS_WIRE_LEN;

/// Byte offsets within a [`ConfigureGrant`] payload.
mod grant {
    use super::CHANNEL_MAP_WIRE_LEN;

    pub const RATE: usize = 0;
    pub const FORMAT: usize = 4;
    pub const RESERVED0: usize = 5;
    pub const PERIOD: usize = 8;
    pub const MAX_RING: usize = 12;
    pub const CHANNEL_MAP: usize = 16;
    pub const RESERVED1: usize = CHANNEL_MAP + CHANNEL_MAP_WIRE_LEN;
    pub const LEN: usize = RESERVED1 + 3;
}

/// Wire length of the `Configure` reply.
pub const AUDIO_CHANNEL_CONFIGURE_REPLY_LEN: usize = 4 + grant::LEN;

/// Wire length of the `Service` reply.
pub const AUDIO_CHANNEL_SERVICE_REPLY_LEN: usize = 4 + service::LEN;

/// Largest reply any device-channel request produces.
///
/// Computed rather than naming whichever reply is biggest today: widening one
/// payload must not silently leave every buffer in the contract short.
pub const AUDIO_CHANNEL_MAX_REPLY: usize = largest_reply();

const fn largest_reply() -> usize {
    let mut largest = AUDIO_CHANNEL_FACTS_REPLY_LEN;
    if AUDIO_CHANNEL_ENDPOINT_REPLY_LEN > largest {
        largest = AUDIO_CHANNEL_ENDPOINT_REPLY_LEN;
    }
    if AUDIO_CHANNEL_CONFIGURE_REPLY_LEN > largest {
        largest = AUDIO_CHANNEL_CONFIGURE_REPLY_LEN;
    }
    if AUDIO_CHANNEL_SERVICE_REPLY_LEN > largest {
        largest = AUDIO_CHANNEL_SERVICE_REPLY_LEN;
    }
    if crate::reply::STATUS_REPLY_LEN > largest {
        largest = crate::reply::STATUS_REPLY_LEN;
    }
    largest
}

const _: () = assert!(AUDIO_CHANNEL_MAX_REPLY >= AUDIO_CHANNEL_FACTS_REPLY_LEN);
const _: () = assert!(AUDIO_CHANNEL_MAX_REPLY >= AUDIO_CHANNEL_ENDPOINT_REPLY_LEN);
const _: () = assert!(AUDIO_CHANNEL_MAX_REPLY >= AUDIO_CHANNEL_CONFIGURE_REPLY_LEN);
const _: () = assert!(AUDIO_CHANNEL_MAX_REPLY >= AUDIO_CHANNEL_SERVICE_REPLY_LEN);
const _: () = assert!(AUDIO_CHANNEL_MAX_REPLY >= crate::reply::STATUS_REPLY_LEN);

/// Both sides size their request buffer to [`AUDIO_CHANNEL_MAX_REQUEST`], so a
/// body that outgrew it must be a build failure rather than a `BufferTooSmall`
/// doorbell at run time.
const _: () = assert!(AUDIO_CHANNEL_MAX_REQUEST >= HEADER_LEN + attach::LEN);
const _: () = assert!(AUDIO_CHANNEL_MAX_REQUEST >= HEADER_LEN + configure::LEN);

/// Encode the driver's reply to [`AudioChannelRequest::Facts`].
#[must_use]
pub fn encode_facts_reply(
    result: Result<AudioDeviceFacts, Errno>,
) -> [u8; AUDIO_CHANNEL_FACTS_REPLY_LEN] {
    let mut out = [0u8; AUDIO_CHANNEL_FACTS_REPLY_LEN];
    match result {
        Ok(facts) => out[4..].copy_from_slice(&facts.to_wire()),
        Err(err) => crate::reply::put_refusal(&mut out, err),
    }
    out
}

/// Decode a `Facts` reply, fail-closed.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] for a short frame, the decoded [`Errno`] when the
/// driver refused, or whatever [`AudioDeviceFacts::from_wire`] refuses.
pub fn decode_facts_reply(bytes: &[u8]) -> Result<AudioDeviceFacts, Errno> {
    AudioDeviceFacts::from_wire(crate::reply::take_payload(
        bytes,
        AUDIO_CHANNEL_FACTS_REPLY_LEN,
    )?)
}

/// Encode the driver's reply to [`AudioChannelRequest::EndpointFacts`].
#[must_use]
pub fn encode_endpoint_reply(
    result: Result<AudioEndpointFacts, Errno>,
) -> [u8; AUDIO_CHANNEL_ENDPOINT_REPLY_LEN] {
    let mut out = [0u8; AUDIO_CHANNEL_ENDPOINT_REPLY_LEN];
    match result {
        Ok(facts) => out[4..].copy_from_slice(&facts.to_wire()),
        Err(err) => crate::reply::put_refusal(&mut out, err),
    }
    out
}

/// Decode an `EndpointFacts` reply, fail-closed.
///
/// # Errors
///
/// As for [`decode_facts_reply`], with [`AudioEndpointFacts::from_wire`]'s own
/// refusals.
pub fn decode_endpoint_reply(bytes: &[u8]) -> Result<AudioEndpointFacts, Errno> {
    AudioEndpointFacts::from_wire(crate::reply::take_payload(
        bytes,
        AUDIO_CHANNEL_ENDPOINT_REPLY_LEN,
    )?)
}

/// Encode the driver's reply to [`AudioChannelRequest::Configure`].
#[must_use]
pub fn encode_configure_reply(
    result: Result<ConfigureGrant, Errno>,
) -> [u8; AUDIO_CHANNEL_CONFIGURE_REPLY_LEN] {
    let mut out = [0u8; AUDIO_CHANNEL_CONFIGURE_REPLY_LEN];
    match result {
        Ok(granted) => {
            let body = &mut out[4..];
            put_u32(body, grant::RATE, granted.rate.hz());
            body[grant::FORMAT] = granted.format.as_u8();
            put_u32(body, grant::PERIOD, granted.period_frames);
            put_u32(body, grant::MAX_RING, granted.max_ring_frames);
            body[grant::CHANNEL_MAP..grant::CHANNEL_MAP + CHANNEL_MAP_WIRE_LEN]
                .copy_from_slice(&granted.channel_map.to_wire());
        }
        Err(err) => crate::reply::put_refusal(&mut out, err),
    }
    out
}

/// Decode a `Configure` reply, fail-closed.
///
/// # Errors
///
/// * [`Errno::BufferTooSmall`] — shorter than
///   [`AUDIO_CHANNEL_CONFIGURE_REPLY_LEN`].
/// * The decoded [`Errno`] — the driver refused the configuration.
/// * [`Errno::BadMagic`] — a dirty reserved field.
/// * [`Errno::OutOfRange`] — a grant naming a period or ring depth outside the
///   ring bounds, or a ring that could not hold one period.
pub fn decode_configure_reply(bytes: &[u8]) -> Result<ConfigureGrant, Errno> {
    let body = crate::reply::take_payload(bytes, AUDIO_CHANNEL_CONFIGURE_REPLY_LEN)?;
    if body[grant::RESERVED0..grant::PERIOD]
        .iter()
        .any(|b| *b != 0)
        || body[grant::RESERVED1..].iter().any(|b| *b != 0)
    {
        return Err(Errno::BadMagic);
    }
    let period_frames = read_u32(body, grant::PERIOD);
    let max_ring_frames = read_u32(body, grant::MAX_RING);
    if period_frames == 0
        || max_ring_frames > ring_bounds::MAX_FRAMES
        || max_ring_frames < period_frames
        || max_ring_frames < ring_bounds::MIN_FRAMES
    {
        return Err(Errno::OutOfRange);
    }
    Ok(ConfigureGrant {
        rate: Rate::new(read_u32(body, grant::RATE))?,
        format: SampleFormat::from_u8(body[grant::FORMAT])?,
        channel_map: ChannelMap::from_wire(&body[grant::CHANNEL_MAP..])?,
        period_frames,
        max_ring_frames,
    })
}

/// Encode the driver's reply to [`AudioChannelRequest::Service`].
#[must_use]
pub fn encode_service_reply(
    result: Result<AudioServiceReport, Errno>,
) -> [u8; AUDIO_CHANNEL_SERVICE_REPLY_LEN] {
    let mut out = [0u8; AUDIO_CHANNEL_SERVICE_REPLY_LEN];
    match result {
        Ok(report) => {
            let body = &mut out[4..];
            put_u32(body, service::TRANSFERRED, report.transferred);
            body[service::RUNNING] = u8::from(report.running);
            put_u64(body, service::POSITION, report.position.get());
            put_u64(body, service::XRUN_FRAMES, report.xrun_frames);
            body[service::SAMPLED_AT..service::SAMPLED_AT + Time64::WIRE_LEN]
                .copy_from_slice(&report.sampled_at.to_le_bytes());
        }
        Err(err) => crate::reply::put_refusal(&mut out, err),
    }
    out
}

/// Decode a `Service` reply, fail-closed.
///
/// # Errors
///
/// * [`Errno::BufferTooSmall`] — shorter than
///   [`AUDIO_CHANNEL_SERVICE_REPLY_LEN`].
/// * The decoded [`Errno`] — the driver refused the doorbell.
/// * [`Errno::BadMagic`] — a dirty reserved field.
/// * [`Errno::OutOfRange`] — a running flag that is neither `0` nor `1`.
/// * [`Errno::TimestampOutOfRange`] — a non-canonical sample time.
pub fn decode_service_reply(bytes: &[u8]) -> Result<AudioServiceReport, Errno> {
    let body = crate::reply::take_payload(bytes, AUDIO_CHANNEL_SERVICE_REPLY_LEN)?;
    if body[service::RESERVED..service::POSITION]
        .iter()
        .any(|b| *b != 0)
    {
        return Err(Errno::BadMagic);
    }
    let running = match body[service::RUNNING] {
        0 => false,
        1 => true,
        _ => return Err(Errno::OutOfRange),
    };
    Ok(AudioServiceReport {
        transferred: read_u32(body, service::TRANSFERRED),
        running,
        position: Frames::new(read_u64(body, service::POSITION)),
        xrun_frames: read_u64(body, service::XRUN_FRAMES),
        sampled_at: Time64::from_bytes(&body[service::SAMPLED_AT..])?,
    })
}

/// Byte offsets within an [`AudioChannelNotify`] frame.
mod notify {
    use crate::time::Time64;

    pub const KIND: usize = 6;
    pub const RESERVED0: usize = 7;
    pub const ENDPOINT: usize = 8;
    pub const JACK: usize = 10;
    pub const RESERVED1: usize = 11;
    pub const POSITION: usize = 12;
    pub const LOST_FRAMES: usize = 20;
    pub const SAMPLED_AT: usize = 28;
    pub const LEN: usize = SAMPLED_AT + Time64::WIRE_LEN;
}

/// Wire length of an [`AudioChannelNotify`] frame.
pub const AUDIO_CHANNEL_NOTIFY_LEN: usize = notify::LEN;

/// Wire byte for a period-elapsed notification.
const NOTIFY_PERIOD: u8 = 1;
/// Wire byte for an under/over-run notification.
const NOTIFY_XRUN: u8 = 2;
/// Wire byte for a jack-state-change notification.
const NOTIFY_JACK: u8 = 3;

/// The driver → mixer wake.
///
/// The driver `ipc_send`s this fixed frame to
/// [`AttachParams::notify_endpoint`] after its device interrupt. *Which*
/// channel woke is the port it arrived on; which endpoint is in the frame.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AudioChannelNotify {
    /// A period boundary passed: here is the clock pair the mixer's linear fit
    /// is built from, and the mixer should issue the next
    /// [`AudioChannelRequest::Service`].
    PeriodElapsed {
        /// The endpoint whose period elapsed.
        endpoint: u16,
        /// The device's frame position when it was sampled.
        position: Frames,
        /// When that position was sampled.
        sampled_at: Time64,
    },
    /// Frames were lost. The position never lies, so the mixer resynchronises
    /// exactly rather than drifting.
    Xrun {
        /// The endpoint that lost frames.
        endpoint: u16,
        /// The position the loss started at.
        position: Frames,
        /// How many frames were lost.
        lost_frames: u64,
    },
    /// A connector was occupied or vacated.
    JackChanged {
        /// The endpoint whose connector changed.
        endpoint: u16,
        /// Its new state.
        jack: JackState,
    },
}

impl AudioChannelNotify {
    /// Encoded length of the notify frame.
    pub const WIRE_LEN: usize = AUDIO_CHANNEL_NOTIFY_LEN;

    /// Encode the notify frame.
    #[must_use]
    pub fn encode(&self) -> [u8; AUDIO_CHANNEL_NOTIFY_LEN] {
        let mut out = [0u8; AUDIO_CHANNEL_NOTIFY_LEN];
        put_u32(&mut out, 0, AUDIO_CHANNEL_NOTIFY_MAGIC);
        put_u16(&mut out, 4, AUDIO_CHANNEL_VERSION_V1);
        match self {
            Self::PeriodElapsed {
                endpoint,
                position,
                sampled_at,
            } => {
                out[notify::KIND] = NOTIFY_PERIOD;
                put_u16(&mut out, notify::ENDPOINT, *endpoint);
                put_u64(&mut out, notify::POSITION, position.get());
                out[notify::SAMPLED_AT..notify::SAMPLED_AT + Time64::WIRE_LEN]
                    .copy_from_slice(&sampled_at.to_le_bytes());
            }
            Self::Xrun {
                endpoint,
                position,
                lost_frames,
            } => {
                out[notify::KIND] = NOTIFY_XRUN;
                put_u16(&mut out, notify::ENDPOINT, *endpoint);
                put_u64(&mut out, notify::POSITION, position.get());
                put_u64(&mut out, notify::LOST_FRAMES, *lost_frames);
            }
            Self::JackChanged { endpoint, jack } => {
                out[notify::KIND] = NOTIFY_JACK;
                put_u16(&mut out, notify::ENDPOINT, *endpoint);
                out[notify::JACK] = jack.as_u8();
            }
        }
        out
    }

    /// Decode a notify frame, fail-closed.
    ///
    /// A field the notification's own kind does not define must be zero: a
    /// populated one would be a value the decoder never looked at, which is
    /// how a second meaning gets smuggled into a fixed frame.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — shorter than
    ///   [`AUDIO_CHANNEL_NOTIFY_LEN`].
    /// * [`Errno::BadMagic`] — wrong magic, a dirty reserved byte, or a field
    ///   the kind does not define.
    /// * [`Errno::AbiVersionUnsupported`] — not
    ///   [`AUDIO_CHANNEL_VERSION_V1`].
    /// * [`Errno::OutOfRange`] — an unknown kind byte, an endpoint past
    ///   [`MAX_DEVICE_ENDPOINTS`], or an undefined jack state.
    pub fn decode(bytes: &[u8]) -> Result<Self, Errno> {
        // Exactly the frame, so the "nothing past this kind's fields" checks
        // cannot read a caller's longer buffer.
        let Some(bytes) = bytes.get(..AUDIO_CHANNEL_NOTIFY_LEN) else {
            return Err(Errno::BufferTooSmall);
        };
        if read_u32(bytes, 0) != AUDIO_CHANNEL_NOTIFY_MAGIC {
            return Err(Errno::BadMagic);
        }
        if read_u16(bytes, 4) != AUDIO_CHANNEL_VERSION_V1 {
            return Err(Errno::AbiVersionUnsupported);
        }
        if bytes[notify::RESERVED0] != 0 || bytes[notify::RESERVED1] != 0 {
            return Err(Errno::BadMagic);
        }
        let endpoint = checked_endpoint(read_u16(bytes, notify::ENDPOINT))?;
        let position = Frames::new(read_u64(bytes, notify::POSITION));
        let lost_frames = read_u64(bytes, notify::LOST_FRAMES);
        let sampled_at_clean = bytes[notify::SAMPLED_AT..].iter().all(|b| *b == 0);
        match bytes[notify::KIND] {
            NOTIFY_PERIOD => {
                if lost_frames != 0 || bytes[notify::JACK] != 0 {
                    return Err(Errno::BadMagic);
                }
                Ok(Self::PeriodElapsed {
                    endpoint,
                    position,
                    sampled_at: Time64::from_bytes(&bytes[notify::SAMPLED_AT..])?,
                })
            }
            NOTIFY_XRUN => {
                if bytes[notify::JACK] != 0 || !sampled_at_clean {
                    return Err(Errno::BadMagic);
                }
                Ok(Self::Xrun {
                    endpoint,
                    position,
                    lost_frames,
                })
            }
            NOTIFY_JACK => {
                if position.get() != 0 || lost_frames != 0 || !sampled_at_clean {
                    return Err(Errno::BadMagic);
                }
                Ok(Self::JackChanged {
                    endpoint,
                    jack: JackState::from_u8(bytes[notify::JACK])?,
                })
            }
            _ => Err(Errno::OutOfRange),
        }
    }
}

#[cfg(test)]
#[path = "audio_channel_tests.rs"]
mod tests;
