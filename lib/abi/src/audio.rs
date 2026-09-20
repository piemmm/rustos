//! `audio-v1`: the client stream ABI — the one surface a program plays or
//! records sound through (`plans/SOUND.md`).
//!
//! There is exactly one transport and no bypass. A program enumerates the
//! sinks and sources it may see, opens a stream, is told the latency it was
//! granted, shares a PCM ring, and writes frames at exact positions. There is
//! no exclusive mode, no raw device node, and no second client API — the
//! single path is low-latency enough that nothing wants to go around it.
//!
//! # What this protocol deliberately does not have
//!
//! **No period or buffer size.** Those are the device's ring geometry, and
//! every system that leaks them into its client API makes every program
//! re-derive latency from numbers it should never have seen. A client states a
//! latency *target* and is told the latency it was *granted*, in frames and in
//! [`Duration64`], so it never needs to know a sample rate to reason about
//! time.
//!
//! **No mix format.** A request the device cannot meet is answered with what
//! it *can* meet ([`StreamGrant`]) rather than silently resampled, so a single
//! stream at unity gain whose rate and format the device accepts reaches the
//! hardware unaltered. Bit-exactness is a property of the one path, not a mode
//! beside it.
//!
//! # Positions, not offsets
//!
//! Every position is a monotone [`Frames`] count, so `Start` at a frame,
//! gapless playback, and A/V sync are exact arithmetic. The clock is
//! *exported*: [`ClockReport`] hands back the device's own (position, time)
//! pair and its measured rate, so a program that must line sound up with
//! anything else is doing arithmetic rather than guessing.
//!
//! # Authority
//!
//! Playback needs no capability: the authorisation is that the caller's
//! session holds the seat lease on the sink, checked at open against the
//! kernel-attested caller. Opening a *source* additionally demands the capture
//! capability, and every live capture stream is machine state the session
//! draws an indicator from — a recording program cannot suppress it
//! (`plans/SOUND.md`). A stream id is a service-issued token, and the service
//! checks it against the kernel-attested caller, so a guessed id cannot reach
//! another principal's stream.
//!
//! # Fail closed
//!
//! Every decode is total: an unknown magic, version, operation or role, a
//! dirty reserved field, an out-of-range rate or latency, or a notify port
//! naming a reserved rendezvous refuses with one typed [`Errno`].

use crate::driver::audio::{
    ring_bounds, AudioName, ChannelMap, Frames, GainRange, JackState, Rate, RateSupport,
    SampleFormat, SampleFormats, StreamDirection, AUDIO_NAME_MAX, CHANNEL_MAP_WIRE_LEN,
    GAIN_RANGE_WIRE_LEN, RATE_SUPPORT_WIRE_LEN,
};
use crate::le::{put_i32, put_u16, put_u32, put_u64, read_i32, read_u16, read_u32, read_u64};
use crate::time::{Duration64, Time64};
use crate::Errno;

/// Reserved well-known call-endpoint id of the audio service (`"AU"`
/// hex-spelled prefix, the convention every service rendezvous follows).
/// Binding it requires `CAP_IPC_BIND_PRIVILEGED`
/// ([`crate::ipc::is_reserved_endpoint`]): a squatter claiming the rendezvous
/// first would receive every program's samples and learn their shared-memory
/// grants.
pub const AUDIO_ENDPOINT: u64 = 0x4155_1001;

/// Magic number identifying an audio-service request (`"AUD1"`).
pub const AUDIO_REQUEST_MAGIC: u32 = u32::from_le_bytes(*b"AUD1");

/// Magic number identifying an audio-service notification (`"AUDN"`).
pub const AUDIO_NOTIFY_MAGIC: u32 = u32::from_le_bytes(*b"AUDN");

/// The `audio-v1` protocol version.
pub const AUDIO_VERSION_V1: u16 = 1;

/// High tag of a client-owned stream notify-port id (see
/// [`notify_endpoint_for`]).
const AUDIO_CLIENT_NOTIFY_TAG: u64 = 0x4155_0000_0000_0000;

/// Streams one process may hold open at once.
///
/// The notify-port id packs the stream's slot into a byte beside the whole of
/// the pid, so the packing itself bounds it. A fixed containment bound, not a
/// capacity: what actually bounds a principal's streams is the resource limit
/// the service admits against, and a process wanting two hundred and
/// fifty-six simultaneous audio streams is not a use, it is an attack.
pub const MAX_CLIENT_STREAM_SLOTS: u64 = 256;

/// The notify-mailbox endpoint id a client binds for one stream, and the
/// service `ipc_send`s that stream's [`AudioNotify`] to.
///
/// The **service** derives it from the caller's kernel-attested pid and hands
/// it back in the [`StreamGrant`]; the client never names it in a request. A
/// client that could name its own notify port could name somebody *else's*
/// mailbox instead and use the audio service as a proxy to spam it, and no
/// check the service could make would tell the two apart. Deriving it from an
/// identity the caller cannot forge removes the question.
///
/// The id is deliberately **not** reserved, so the client `port_bind`s it
/// without a privileged bind, and the mailbox is owner-only to receive, so a
/// bystander cannot steal the wakes. `slot` occupies the low byte and `pid`
/// the next 40 bits — the whole of [`crate::PID_MAX`] — so the three fields
/// tile the word exactly and no pid can reach the tag.
#[must_use]
pub const fn notify_endpoint_for(pid: u64, slot: u64) -> u64 {
    AUDIO_CLIENT_NOTIFY_TAG | ((pid & crate::PID_MAX) << 8) | (slot % MAX_CLIENT_STREAM_SLOTS)
}

/// What a stream is *for*, which is what lets routing be a policy rather than
/// a per-application configuration file.
///
/// The one input a program gives the router beyond its format: whether the
/// sound is media, a conversation, a notification, or accessibility output.
/// Routing, ducking, and what survives a seat switch are decided from this by
/// one policy function, never from a list of process names.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum StreamRole {
    /// Music, video, games — anything a user chose to play.
    Media = 0,
    /// A live conversation, which ducks media and survives where media does
    /// not.
    Communication = 1,
    /// A short alert. A notification from a session that does not hold the
    /// seat is dropped rather than queued: one that arrives ten minutes late
    /// is noise.
    Notification = 2,
    /// A screen reader or other assistive output, which outranks media.
    Accessibility = 3,
}

impl StreamRole {
    /// Raw on-wire discriminant.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Inverse of [`Self::as_u8`].
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for an undefined discriminant.
    pub const fn from_u8(raw: u8) -> Result<Self, Errno> {
        match raw {
            0 => Ok(Self::Media),
            1 => Ok(Self::Communication),
            2 => Ok(Self::Notification),
            3 => Ok(Self::Accessibility),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// Where a stream stands.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum StreamState {
    /// Opened, not yet started.
    Idle = 0,
    /// Frames are moving.
    Running = 1,
    /// Stopped at a frame boundary, holding its position.
    Paused = 2,
    /// Playing out what is queued, then stopping.
    Draining = 3,
    /// The session does not hold the sink's seat lease, so the stream is
    /// paused at a frame boundary and *told so*. A departing user's music does
    /// not play into the arriving user's room, and it does not silently vanish
    /// either: on switch-back it resumes from the exact frame.
    SeatInactive = 4,
    /// The device went away. The position is intact and the reason is stated;
    /// other devices are untouched.
    DeviceLost = 5,
}

impl StreamState {
    /// Raw on-wire discriminant.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Inverse of [`Self::as_u8`].
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for an undefined discriminant.
    pub const fn from_u8(raw: u8) -> Result<Self, Errno> {
        match raw {
            0 => Ok(Self::Idle),
            1 => Ok(Self::Running),
            2 => Ok(Self::Paused),
            3 => Ok(Self::Draining),
            4 => Ok(Self::SeatInactive),
            5 => Ok(Self::DeviceLost),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// Fixed request header: magic (4) + version (2) + op (1) + reserved (1).
const HEADER_LEN: usize = 8;

/// Operation discriminants (the request's seventh byte).
mod op {
    pub const ENUMERATE: u8 = 1;
    pub const OPEN: u8 = 2;
    pub const ATTACH: u8 = 3;
    pub const START: u8 = 4;
    pub const STOP: u8 = 5;
    pub const DRAIN: u8 = 6;
    pub const FLUSH: u8 = 7;
    pub const CLOCK: u8 = 8;
    pub const GAIN: u8 = 9;
    pub const MUTE: u8 = 10;
    pub const STATE: u8 = 11;
    pub const CLOSE: u8 = 12;
    pub const BIND_DRIVER: u8 = 13;
}

/// Byte offsets within the [`AudioRequest::Enumerate`] body.
mod enumerate {
    pub const DIRECTION: usize = 0;
    pub const RESERVED: usize = 1;
    pub const INDEX: usize = 2;
    pub const LEN: usize = 4;
}

/// Byte offsets within the [`AudioRequest::Open`] body.
mod open {
    use super::CHANNEL_MAP_WIRE_LEN;

    pub const DEVICE: usize = 0;
    pub const DIRECTION: usize = 4;
    pub const FORMAT: usize = 5;
    pub const ROLE: usize = 6;
    pub const RESERVED0: usize = 7;
    pub const RATE: usize = 8;
    pub const LATENCY: usize = 12;
    pub const CHANNEL_MAP: usize = 16;
    pub const RESERVED1: usize = CHANNEL_MAP + CHANNEL_MAP_WIRE_LEN;
    pub const LEN: usize = RESERVED1 + 3;
}

/// Byte offsets within the [`AudioRequest::Attach`] body.
mod attach {
    pub const STREAM: usize = 0;
    pub const GRANT: usize = 8;
    pub const LEN: usize = 16;
}

/// Byte offsets within the transport-control (`Start` / `Stop`) body.
mod transport {
    pub const STREAM: usize = 0;
    pub const AT: usize = 8;
    pub const LEN: usize = 16;
}

/// Byte offsets within the [`AudioRequest::Gain`] / [`AudioRequest::Mute`]
/// body.
mod level {
    pub const STREAM: usize = 0;
    pub const MILLIBEL: usize = 8;
    pub const MUTED: usize = 8;
    pub const RESERVED: usize = 12;
    pub const LEN: usize = 16;
}

/// Wire length of a body naming nothing but its stream, and of the one
/// naming nothing but a driver's device-channel endpoint.
const STREAM_BODY_LEN: usize = 8;

/// Largest audio-service request frame: the header plus the widest body. A
/// fixed validation bound sizing the buffer both sides pin for the endpoint.
pub const AUDIO_MAX_REQUEST: usize = HEADER_LEN + largest_body();

const fn largest_body() -> usize {
    let mut largest = STREAM_BODY_LEN;
    if enumerate::LEN > largest {
        largest = enumerate::LEN;
    }
    if open::LEN > largest {
        largest = open::LEN;
    }
    if attach::LEN > largest {
        largest = attach::LEN;
    }
    if transport::LEN > largest {
        largest = transport::LEN;
    }
    if level::LEN > largest {
        largest = level::LEN;
    }
    largest
}

/// One audio-service operation.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AudioRequest {
    /// Describe the `index`th sink or source the caller may see, or refuse
    /// with [`Errno::NotFound`] once the list is exhausted.
    Enumerate {
        /// Sinks or sources.
        direction: StreamDirection,
        /// Position in that list, from zero.
        index: u16,
    },
    /// Open a stream, and be told what was actually granted.
    Open(OpenParams),
    /// Adopt the caller's shared PCM region for the stream. The region is
    /// exactly [`StreamGrant`]'s geometry; the handle is the endpoint-directed
    /// `shm_grant` the caller minted.
    Attach {
        /// The stream the region carries frames for.
        stream_id: u64,
        /// The `shm_grant` handle minted to the service's serving task.
        region_grant: u64,
    },
    /// Begin moving frames at an exact position.
    Start {
        /// The stream to start.
        stream_id: u64,
        /// The position its first frame belongs at.
        at: Frames,
    },
    /// Stop at an exact position, holding it so a resume is exact.
    Stop {
        /// The stream to stop.
        stream_id: u64,
        /// The position to stop at.
        at: Frames,
    },
    /// Play out everything queued, then stop.
    Drain {
        /// The stream to drain.
        stream_id: u64,
    },
    /// Discard everything queued. The position advances over the discarded
    /// frames, so the stream's arithmetic still describes where what follows
    /// belongs.
    Flush {
        /// The stream to flush.
        stream_id: u64,
    },
    /// Read the device clock this stream is in the domain of.
    Clock {
        /// The stream whose clock is wanted.
        stream_id: u64,
    },
    /// Set this stream's gain.
    Gain {
        /// The stream to set.
        stream_id: u64,
        /// Gain in hundredths of a decibel; zero is unity, and unity is
        /// exactly bit-exact.
        millibel: i32,
    },
    /// Mute or unmute this stream, independently of its gain.
    Mute {
        /// The stream to set.
        stream_id: u64,
        /// Whether it is muted.
        muted: bool,
    },
    /// Read the stream's state, the position it changed at, and its glitch
    /// tallies.
    State {
        /// The stream to report on.
        stream_id: u64,
    },
    /// Close the stream and release its region.
    Close {
        /// The stream to close.
        stream_id: u64,
    },
    /// Adopt a driver's `audiochan-v1` device channel as a sound device.
    ///
    /// Not a client operation: the device manager issues it when a driver
    /// publishes its channel node, and the service admits it only from a
    /// caller the kernel attests holds `CAP_DRV_LOAD` — the authority to put
    /// a driver on the machine, which is exactly the authority to tell the
    /// mixer about one. No new capability is minted for it, and no ordinary
    /// program can reach it.
    BindDriver {
        /// The reserved device-channel endpoint the driver claimed and
        /// published as a hardware-tree resource.
        endpoint_id: u64,
    },
}

/// What a client asks for when it opens a stream.
///
/// There is no channel count: the channel map carries it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct OpenParams {
    /// The device to open on, as enumerated.
    pub device_id: u32,
    /// Playback or capture. Capture demands the capture capability.
    pub direction: StreamDirection,
    /// The sample encoding the client will write or read.
    pub format: SampleFormat,
    /// The rate the client's material is at.
    pub rate: Rate,
    /// The client's channel layout.
    pub channel_map: ChannelMap,
    /// What the sound is for.
    pub role: StreamRole,
    /// The latency the client would like, in frames at its own rate. The
    /// service answers the latency it granted rather than failing.
    pub latency_target_frames: u32,
}

impl AudioRequest {
    /// Largest encoded request frame.
    pub const MAX_WIRE_LEN: usize = AUDIO_MAX_REQUEST;

    /// The operation's wire discriminant byte.
    const fn op_byte(&self) -> u8 {
        match self {
            Self::Enumerate { .. } => op::ENUMERATE,
            Self::Open(_) => op::OPEN,
            Self::Attach { .. } => op::ATTACH,
            Self::Start { .. } => op::START,
            Self::Stop { .. } => op::STOP,
            Self::Drain { .. } => op::DRAIN,
            Self::Flush { .. } => op::FLUSH,
            Self::Clock { .. } => op::CLOCK,
            Self::Gain { .. } => op::GAIN,
            Self::Mute { .. } => op::MUTE,
            Self::State { .. } => op::STATE,
            Self::Close { .. } => op::CLOSE,
            Self::BindDriver { .. } => op::BIND_DRIVER,
        }
    }

    /// Encoded length of this operation's frame.
    const fn wire_len(&self) -> usize {
        HEADER_LEN
            + match self {
                Self::Enumerate { .. } => enumerate::LEN,
                Self::Open(_) => open::LEN,
                Self::Attach { .. } => attach::LEN,
                Self::Start { .. } | Self::Stop { .. } => transport::LEN,
                Self::Gain { .. } | Self::Mute { .. } => level::LEN,
                Self::Drain { .. }
                | Self::Flush { .. }
                | Self::Clock { .. }
                | Self::State { .. }
                | Self::Close { .. }
                | Self::BindDriver { .. } => STREAM_BODY_LEN,
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
        put_u32(frame, 0, AUDIO_REQUEST_MAGIC);
        put_u16(frame, 4, AUDIO_VERSION_V1);
        frame[6] = self.op_byte();
        let body = &mut frame[HEADER_LEN..];
        match self {
            Self::Enumerate { direction, index } => {
                body[enumerate::DIRECTION] = direction.as_u8();
                put_u16(body, enumerate::INDEX, *index);
            }
            Self::Open(params) => encode_open(body, params),
            Self::Attach {
                stream_id,
                region_grant,
            } => {
                put_u64(body, attach::STREAM, *stream_id);
                put_u64(body, attach::GRANT, *region_grant);
            }
            Self::Start { stream_id, at } | Self::Stop { stream_id, at } => {
                put_u64(body, transport::STREAM, *stream_id);
                put_u64(body, transport::AT, at.get());
            }
            Self::Gain {
                stream_id,
                millibel,
            } => {
                put_u64(body, level::STREAM, *stream_id);
                put_i32(body, level::MILLIBEL, *millibel);
            }
            Self::Mute { stream_id, muted } => {
                put_u64(body, level::STREAM, *stream_id);
                body[level::MUTED] = u8::from(*muted);
            }
            Self::Drain { stream_id }
            | Self::Flush { stream_id }
            | Self::Clock { stream_id }
            | Self::State { stream_id }
            | Self::Close { stream_id } => put_u64(body, 0, *stream_id),
            Self::BindDriver { endpoint_id } => put_u64(body, 0, *endpoint_id),
        }
        Ok(len)
    }

    /// Decode a request frame, fail-closed.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — shorter than the operation requires.
    /// * [`Errno::BadMagic`] — wrong magic or a dirty reserved field.
    /// * [`Errno::AbiVersionUnsupported`] — not [`AUDIO_VERSION_V1`].
    /// * [`Errno::OutOfRange`] — an unknown operation byte, a zero stream id,
    ///   or an out-of-range embedded value.
    pub fn decode(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < HEADER_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if read_u32(bytes, 0) != AUDIO_REQUEST_MAGIC {
            return Err(Errno::BadMagic);
        }
        if read_u16(bytes, 4) != AUDIO_VERSION_V1 {
            return Err(Errno::AbiVersionUnsupported);
        }
        if bytes[7] != 0 {
            return Err(Errno::BadMagic);
        }
        let op = bytes[6];
        let body = bytes
            .get(HEADER_LEN..HEADER_LEN + body_len(op)?)
            .ok_or(Errno::BufferTooSmall)?;
        match op {
            op::ENUMERATE => decode_enumerate(body),
            op::OPEN => Ok(Self::Open(decode_open(body)?)),
            op::ATTACH => Ok(Self::Attach {
                stream_id: checked_stream(read_u64(body, attach::STREAM))?,
                region_grant: read_u64(body, attach::GRANT),
            }),
            op::START | op::STOP => {
                let stream_id = checked_stream(read_u64(body, transport::STREAM))?;
                let at = Frames::new(read_u64(body, transport::AT));
                if op == op::START {
                    Ok(Self::Start { stream_id, at })
                } else {
                    Ok(Self::Stop { stream_id, at })
                }
            }
            op::GAIN => decode_gain(body),
            op::MUTE => decode_mute(body),
            // An endpoint id is the driver's, not a stream token, so zero is
            // rejected on its own terms: no endpoint is ever id zero.
            op::BIND_DRIVER => match read_u64(body, 0) {
                0 => Err(Errno::OutOfRange),
                endpoint_id => Ok(Self::BindDriver { endpoint_id }),
            },
            _ => decode_stream_only(op, body),
        }
    }
}

/// Body length of the operation `op` names.
const fn body_len(op: u8) -> Result<usize, Errno> {
    match op {
        op::ENUMERATE => Ok(enumerate::LEN),
        op::OPEN => Ok(open::LEN),
        op::ATTACH => Ok(attach::LEN),
        op::START | op::STOP => Ok(transport::LEN),
        op::GAIN | op::MUTE => Ok(level::LEN),
        op::DRAIN | op::FLUSH | op::CLOCK | op::STATE | op::CLOSE | op::BIND_DRIVER => {
            Ok(STREAM_BODY_LEN)
        }
        _ => Err(Errno::OutOfRange),
    }
}

/// A stream id, refused when it is the zero no stream is ever issued.
///
/// Zero is what an uninitialised or truncated frame carries, so reserving it
/// turns a whole class of confused requests into a refusal rather than an
/// operation on whichever stream happened to be first.
const fn checked_stream(stream_id: u64) -> Result<u64, Errno> {
    if stream_id == 0 {
        return Err(Errno::OutOfRange);
    }
    Ok(stream_id)
}

fn decode_enumerate(body: &[u8]) -> Result<AudioRequest, Errno> {
    if body[enumerate::RESERVED] != 0 {
        return Err(Errno::BadMagic);
    }
    Ok(AudioRequest::Enumerate {
        direction: StreamDirection::from_u8(body[enumerate::DIRECTION])?,
        index: read_u16(body, enumerate::INDEX),
    })
}

fn encode_open(body: &mut [u8], params: &OpenParams) {
    put_u32(body, open::DEVICE, params.device_id);
    body[open::DIRECTION] = params.direction.as_u8();
    body[open::FORMAT] = params.format.as_u8();
    body[open::ROLE] = params.role.as_u8();
    put_u32(body, open::RATE, params.rate.hz());
    put_u32(body, open::LATENCY, params.latency_target_frames);
    body[open::CHANNEL_MAP..open::CHANNEL_MAP + CHANNEL_MAP_WIRE_LEN]
        .copy_from_slice(&params.channel_map.to_wire());
}

fn decode_open(body: &[u8]) -> Result<OpenParams, Errno> {
    if body[open::RESERVED0] != 0 || body[open::RESERVED1..].iter().any(|b| *b != 0) {
        return Err(Errno::BadMagic);
    }
    let latency_target_frames = read_u32(body, open::LATENCY);
    // A target of zero asks for no buffering at all and one past the ring
    // bound asks for memory no grant may pin; both are refused rather than
    // silently clamped, so a client learns its request was nonsense.
    if latency_target_frames == 0 || latency_target_frames > ring_bounds::MAX_FRAMES {
        return Err(Errno::OutOfRange);
    }
    Ok(OpenParams {
        device_id: read_u32(body, open::DEVICE),
        direction: StreamDirection::from_u8(body[open::DIRECTION])?,
        format: SampleFormat::from_u8(body[open::FORMAT])?,
        rate: Rate::new(read_u32(body, open::RATE))?,
        channel_map: ChannelMap::from_wire(&body[open::CHANNEL_MAP..])?,
        role: StreamRole::from_u8(body[open::ROLE])?,
        latency_target_frames,
    })
}

fn decode_gain(body: &[u8]) -> Result<AudioRequest, Errno> {
    if read_u32(body, level::RESERVED) != 0 {
        return Err(Errno::BadMagic);
    }
    Ok(AudioRequest::Gain {
        stream_id: checked_stream(read_u64(body, level::STREAM))?,
        millibel: read_i32(body, level::MILLIBEL),
    })
}

fn decode_mute(body: &[u8]) -> Result<AudioRequest, Errno> {
    if body[level::MUTED + 1..].iter().any(|b| *b != 0) {
        return Err(Errno::BadMagic);
    }
    let muted = match body[level::MUTED] {
        0 => false,
        1 => true,
        _ => return Err(Errno::OutOfRange),
    };
    Ok(AudioRequest::Mute {
        stream_id: checked_stream(read_u64(body, level::STREAM))?,
        muted,
    })
}

fn decode_stream_only(op: u8, body: &[u8]) -> Result<AudioRequest, Errno> {
    let stream_id = checked_stream(read_u64(body, 0))?;
    match op {
        op::DRAIN => Ok(AudioRequest::Drain { stream_id }),
        op::FLUSH => Ok(AudioRequest::Flush { stream_id }),
        op::CLOCK => Ok(AudioRequest::Clock { stream_id }),
        op::STATE => Ok(AudioRequest::State { stream_id }),
        op::CLOSE => Ok(AudioRequest::Close { stream_id }),
        _ => Err(Errno::OutOfRange),
    }
}

/// Byte offsets within an [`AudioDeviceDescriptor`] payload.
mod descriptor {
    use super::{AUDIO_NAME_MAX, CHANNEL_MAP_WIRE_LEN, GAIN_RANGE_WIRE_LEN, RATE_SUPPORT_WIRE_LEN};

    pub const DEVICE: usize = 0;
    pub const DIRECTION: usize = 4;
    pub const JACK: usize = 5;
    pub const DEFAULT: usize = 6;
    pub const RESERVED0: usize = 7;
    pub const FORMATS: usize = 8;
    pub const RESERVED1: usize = 10;
    pub const CHANNEL_MAP: usize = 12;
    pub const RESERVED2: usize = CHANNEL_MAP + CHANNEL_MAP_WIRE_LEN;
    pub const RATES: usize = RESERVED2 + 3;
    pub const GAIN: usize = RATES + RATE_SUPPORT_WIRE_LEN;
    pub const NAME_LEN: usize = GAIN + GAIN_RANGE_WIRE_LEN;
    pub const NAME: usize = NAME_LEN + 1;
    pub const LEN: usize = NAME + AUDIO_NAME_MAX;
}

/// One sink or source, as a client sees it.
///
/// The `audio:` resource reference is derived rather than carried:
/// `audio:sink/<id>` for a playback device and `audio:source/<id>` for a
/// capture one, so the scheme and the descriptor cannot disagree.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct AudioDeviceDescriptor {
    /// The service-assigned identity a stream is opened against.
    pub device_id: u32,
    /// Whether it is a sink or a source.
    pub direction: StreamDirection,
    /// Whether anything is plugged into its connector.
    pub jack: JackState,
    /// Whether it is the machine's configured default for its direction.
    pub is_default: bool,
    /// Sample encodings it accepts without conversion.
    pub formats: SampleFormats,
    /// Its channel layout.
    pub channel_map: ChannelMap,
    /// Rates it can be clocked at.
    pub rates: RateSupport,
    /// Its hardware gain control, where it has one. A user interface shows one
    /// number, so the service reports which part of the volume is the
    /// hardware's.
    pub gain: Option<GainRange>,
    /// What to call it.
    pub name: AudioName,
}

/// Wire length of the `Enumerate` reply: a status word then the descriptor
/// (zeroed on refusal).
pub const AUDIO_ENUMERATE_REPLY_LEN: usize = 4 + descriptor::LEN;

/// Encode the service's reply to [`AudioRequest::Enumerate`].
#[must_use]
pub fn encode_enumerate_reply(
    result: Result<AudioDeviceDescriptor, Errno>,
) -> [u8; AUDIO_ENUMERATE_REPLY_LEN] {
    let mut out = [0u8; AUDIO_ENUMERATE_REPLY_LEN];
    match result {
        Ok(device) => {
            let body = &mut out[4..];
            put_u32(body, descriptor::DEVICE, device.device_id);
            body[descriptor::DIRECTION] = device.direction.as_u8();
            body[descriptor::JACK] = device.jack.as_u8();
            body[descriptor::DEFAULT] = u8::from(device.is_default);
            put_u16(body, descriptor::FORMATS, device.formats.bits());
            body[descriptor::CHANNEL_MAP..descriptor::CHANNEL_MAP + CHANNEL_MAP_WIRE_LEN]
                .copy_from_slice(&device.channel_map.to_wire());
            body[descriptor::RATES..descriptor::RATES + RATE_SUPPORT_WIRE_LEN]
                .copy_from_slice(&device.rates.to_wire());
            body[descriptor::GAIN..descriptor::GAIN + GAIN_RANGE_WIRE_LEN]
                .copy_from_slice(&GainRange::to_wire(device.gain));
            body[descriptor::NAME_LEN] = device.name.len_byte();
            body[descriptor::NAME..].copy_from_slice(device.name.raw_bytes());
        }
        Err(err) => crate::reply::put_refusal(&mut out, err),
    }
    out
}

/// Decode an `Enumerate` reply, fail-closed.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] for a short frame, [`Errno::NotFound`] once the
/// list is exhausted or any other refusal the service returned,
/// [`Errno::BadMagic`] for a dirty reserved field, or whatever the embedded
/// values' own decoders refuse.
pub fn decode_enumerate_reply(bytes: &[u8]) -> Result<AudioDeviceDescriptor, Errno> {
    let body = crate::reply::take_payload(bytes, AUDIO_ENUMERATE_REPLY_LEN)?;
    if body[descriptor::RESERVED0] != 0
        || read_u16(body, descriptor::RESERVED1) != 0
        || body[descriptor::RESERVED2..descriptor::RATES]
            .iter()
            .any(|b| *b != 0)
    {
        return Err(Errno::BadMagic);
    }
    let is_default = match body[descriptor::DEFAULT] {
        0 => false,
        1 => true,
        _ => return Err(Errno::OutOfRange),
    };
    let formats = SampleFormats::from_bits(read_u16(body, descriptor::FORMATS))?;
    if formats.is_empty() {
        return Err(Errno::OutOfRange);
    }
    let mut name = [0u8; AUDIO_NAME_MAX];
    name.copy_from_slice(&body[descriptor::NAME..descriptor::LEN]);
    Ok(AudioDeviceDescriptor {
        device_id: read_u32(body, descriptor::DEVICE),
        direction: StreamDirection::from_u8(body[descriptor::DIRECTION])?,
        jack: JackState::from_u8(body[descriptor::JACK])?,
        is_default,
        formats,
        channel_map: ChannelMap::from_wire(&body[descriptor::CHANNEL_MAP..])?,
        rates: RateSupport::from_wire(&body[descriptor::RATES..])?,
        gain: GainRange::from_wire(&body[descriptor::GAIN..])?,
        name: AudioName::from_wire(body[descriptor::NAME_LEN], &name)?,
    })
}

/// Byte offsets within a [`StreamGrant`] payload.
mod stream_grant {
    use super::{Duration64, CHANNEL_MAP_WIRE_LEN};

    pub const STREAM: usize = 0;
    pub const NOTIFY: usize = 8;
    pub const RATE: usize = 16;
    pub const FORMAT: usize = 20;
    pub const RESERVED0: usize = 21;
    pub const RING_FRAMES: usize = 24;
    pub const LATENCY_FRAMES: usize = 28;
    pub const CLOCK_DOMAIN: usize = 32;
    pub const RESERVED1: usize = 36;
    pub const LATENCY: usize = 40;
    pub const CHANNEL_MAP: usize = LATENCY + Duration64::WIRE_LEN;
    pub const RESERVED2: usize = CHANNEL_MAP + CHANNEL_MAP_WIRE_LEN;
    pub const LEN: usize = RESERVED2 + 3;
}

/// What the service actually granted, which is not always what was asked for.
///
/// The client compares it with its request and owns whatever conversion the
/// difference implies. Nothing is silently resampled on its behalf, which is
/// what makes the bit-exact path a property rather than a mode.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct StreamGrant {
    /// The stream's identity for every later request.
    pub stream_id: u64,
    /// The mailbox the service wakes this stream on, derived from the caller's
    /// kernel-attested pid ([`notify_endpoint_for`]) — the client binds it, it
    /// does not choose it.
    pub notify_endpoint: u64,
    /// The rate the stream actually runs at.
    pub rate: Rate,
    /// The sample encoding it actually runs in.
    pub format: SampleFormat,
    /// The channel layout it actually carries.
    pub channel_map: ChannelMap,
    /// Frames the shared ring holds; the region is exactly this geometry.
    pub ring_frames: u32,
    /// The latency granted, in frames at [`Self::rate`].
    pub granted_latency_frames: u32,
    /// The same latency as a span, so a client never needs a sample rate to
    /// reason about time.
    pub granted_latency: Duration64,
    /// The device clock this stream belongs to. Two streams sharing a domain
    /// share a clock exactly; moving between domains is a re-open with the
    /// position carried across, never a hidden resampler.
    pub clock_domain: u32,
}

/// Wire length of the `Open` reply.
pub const AUDIO_OPEN_REPLY_LEN: usize = 4 + stream_grant::LEN;

/// Encode the service's reply to [`AudioRequest::Open`].
#[must_use]
pub fn encode_open_reply(result: Result<StreamGrant, Errno>) -> [u8; AUDIO_OPEN_REPLY_LEN] {
    let mut out = [0u8; AUDIO_OPEN_REPLY_LEN];
    match result {
        Ok(granted) => {
            let body = &mut out[4..];
            put_u64(body, stream_grant::STREAM, granted.stream_id);
            put_u64(body, stream_grant::NOTIFY, granted.notify_endpoint);
            put_u32(body, stream_grant::RATE, granted.rate.hz());
            body[stream_grant::FORMAT] = granted.format.as_u8();
            put_u32(body, stream_grant::RING_FRAMES, granted.ring_frames);
            put_u32(
                body,
                stream_grant::LATENCY_FRAMES,
                granted.granted_latency_frames,
            );
            put_u32(body, stream_grant::CLOCK_DOMAIN, granted.clock_domain);
            body[stream_grant::LATENCY..stream_grant::LATENCY + Duration64::WIRE_LEN]
                .copy_from_slice(&granted.granted_latency.to_le_bytes());
            body[stream_grant::CHANNEL_MAP..stream_grant::CHANNEL_MAP + CHANNEL_MAP_WIRE_LEN]
                .copy_from_slice(&granted.channel_map.to_wire());
        }
        Err(err) => crate::reply::put_refusal(&mut out, err),
    }
    out
}

/// Decode an `Open` reply, fail-closed.
///
/// # Errors
///
/// * [`Errno::BufferTooSmall`] — shorter than [`AUDIO_OPEN_REPLY_LEN`].
/// * The decoded [`Errno`] — the service refused the open.
/// * [`Errno::BadMagic`] — a dirty reserved field.
/// * [`Errno::OutOfRange`] — a zero stream id, a notify port naming a reserved
///   rendezvous, a ring the index arithmetic could not serve, or a granted
///   latency the ring could not hold.
pub fn decode_open_reply(bytes: &[u8]) -> Result<StreamGrant, Errno> {
    let body = crate::reply::take_payload(bytes, AUDIO_OPEN_REPLY_LEN)?;
    if body[stream_grant::RESERVED0..stream_grant::RING_FRAMES]
        .iter()
        .any(|b| *b != 0)
        || read_u32(body, stream_grant::RESERVED1) != 0
        || body[stream_grant::RESERVED2..].iter().any(|b| *b != 0)
    {
        return Err(Errno::BadMagic);
    }
    let notify_endpoint = read_u64(body, stream_grant::NOTIFY);
    // A grant naming a reserved rendezvous would have the client bind a
    // system service's id — refused here rather than discovered at bind time.
    if crate::ipc::is_reserved_endpoint(notify_endpoint) {
        return Err(Errno::OutOfRange);
    }
    let ring_frames = read_u32(body, stream_grant::RING_FRAMES);
    let granted_latency_frames = read_u32(body, stream_grant::LATENCY_FRAMES);
    if !(ring_bounds::MIN_FRAMES..=ring_bounds::MAX_FRAMES).contains(&ring_frames)
        || !ring_frames.is_power_of_two()
        || granted_latency_frames == 0
        || granted_latency_frames > ring_frames
    {
        return Err(Errno::OutOfRange);
    }
    Ok(StreamGrant {
        stream_id: checked_stream(read_u64(body, stream_grant::STREAM))?,
        notify_endpoint,
        rate: Rate::new(read_u32(body, stream_grant::RATE))?,
        format: SampleFormat::from_u8(body[stream_grant::FORMAT])?,
        channel_map: ChannelMap::from_wire(&body[stream_grant::CHANNEL_MAP..])?,
        ring_frames,
        granted_latency_frames,
        granted_latency: Duration64::from_bytes(&body[stream_grant::LATENCY..])?,
        clock_domain: read_u32(body, stream_grant::CLOCK_DOMAIN),
    })
}

/// Byte offsets within a [`ClockReport`] payload.
mod clock {
    use crate::time::Time64;

    pub const POSITION: usize = 0;
    pub const RATE_MILLIHERTZ: usize = 8;
    pub const RESERVED: usize = 12;
    pub const SAMPLED_AT: usize = 16;
    pub const LEN: usize = SAMPLED_AT + Time64::WIRE_LEN;
}

/// The exported device clock: where the device is, when that was true, and how
/// fast it is *actually* going.
///
/// The measured rate is what makes cross-device drift a number rather than a
/// mystery: a device whose crystal says 48 000 and whose reality says 47 998.6
/// reports the second.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ClockReport {
    /// The device's frame position when it was sampled.
    pub position: Frames,
    /// The measured rate, in thousandths of a hertz.
    pub rate_millihertz: u32,
    /// When [`Self::position`] was sampled.
    pub sampled_at: Time64,
}

/// Wire length of the `Clock` reply.
pub const AUDIO_CLOCK_REPLY_LEN: usize = 4 + clock::LEN;

/// Encode the service's reply to [`AudioRequest::Clock`].
#[must_use]
pub fn encode_clock_reply(result: Result<ClockReport, Errno>) -> [u8; AUDIO_CLOCK_REPLY_LEN] {
    let mut out = [0u8; AUDIO_CLOCK_REPLY_LEN];
    match result {
        Ok(report) => {
            let body = &mut out[4..];
            put_u64(body, clock::POSITION, report.position.get());
            put_u32(body, clock::RATE_MILLIHERTZ, report.rate_millihertz);
            body[clock::SAMPLED_AT..clock::SAMPLED_AT + Time64::WIRE_LEN]
                .copy_from_slice(&report.sampled_at.to_le_bytes());
        }
        Err(err) => crate::reply::put_refusal(&mut out, err),
    }
    out
}

/// Decode a `Clock` reply, fail-closed.
///
/// # Errors
///
/// * [`Errno::BufferTooSmall`] — shorter than [`AUDIO_CLOCK_REPLY_LEN`].
/// * The decoded [`Errno`] — the service refused.
/// * [`Errno::BadMagic`] — a dirty reserved field.
/// * [`Errno::OutOfRange`] — a measured rate outside what any converter runs
///   at, which would poison every fit built on it.
/// * [`Errno::TimestampOutOfRange`] — a non-canonical sample time.
pub fn decode_clock_reply(bytes: &[u8]) -> Result<ClockReport, Errno> {
    let body = crate::reply::take_payload(bytes, AUDIO_CLOCK_REPLY_LEN)?;
    if read_u32(body, clock::RESERVED) != 0 {
        return Err(Errno::BadMagic);
    }
    let rate_millihertz = read_u32(body, clock::RATE_MILLIHERTZ);
    if !(Rate::MIN_HZ * 1_000..=Rate::MAX_HZ * 1_000).contains(&rate_millihertz) {
        return Err(Errno::OutOfRange);
    }
    Ok(ClockReport {
        position: Frames::new(read_u64(body, clock::POSITION)),
        rate_millihertz,
        sampled_at: Time64::from_bytes(&body[clock::SAMPLED_AT..])?,
    })
}

/// Byte offsets within a [`StreamReport`] payload.
mod state {
    pub const STATE: usize = 0;
    pub const RESERVED: usize = 1;
    pub const XRUNS: usize = 4;
    pub const CHANGED_AT: usize = 8;
    pub const XRUN_FRAMES: usize = 16;
    pub const LEN: usize = 24;
}

/// Where a stream stands, and what it has lost.
///
/// Both tallies are reported because neither implies the other: the frame
/// count says how much audio was missed and the event count says how often the
/// user heard a glitch.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct StreamReport {
    /// The stream's state.
    pub state: StreamState,
    /// The position it entered that state at.
    pub changed_at: Frames,
    /// Distinct under- or over-runs since the stream was opened.
    pub xruns: u32,
    /// Frames lost across all of them.
    pub xrun_frames: u64,
}

/// Wire length of the `State` reply.
pub const AUDIO_STATE_REPLY_LEN: usize = 4 + state::LEN;

/// Encode the service's reply to [`AudioRequest::State`].
#[must_use]
pub fn encode_state_reply(result: Result<StreamReport, Errno>) -> [u8; AUDIO_STATE_REPLY_LEN] {
    let mut out = [0u8; AUDIO_STATE_REPLY_LEN];
    match result {
        Ok(report) => {
            let body = &mut out[4..];
            body[state::STATE] = report.state.as_u8();
            put_u32(body, state::XRUNS, report.xruns);
            put_u64(body, state::CHANGED_AT, report.changed_at.get());
            put_u64(body, state::XRUN_FRAMES, report.xrun_frames);
        }
        Err(err) => crate::reply::put_refusal(&mut out, err),
    }
    out
}

/// Decode a `State` reply, fail-closed.
///
/// # Errors
///
/// * [`Errno::BufferTooSmall`] — shorter than [`AUDIO_STATE_REPLY_LEN`].
/// * The decoded [`Errno`] — the service refused.
/// * [`Errno::BadMagic`] — a dirty reserved field.
/// * [`Errno::OutOfRange`] — an undefined state.
pub fn decode_state_reply(bytes: &[u8]) -> Result<StreamReport, Errno> {
    let body = crate::reply::take_payload(bytes, AUDIO_STATE_REPLY_LEN)?;
    if body[state::RESERVED..state::XRUNS].iter().any(|b| *b != 0) {
        return Err(Errno::BadMagic);
    }
    Ok(StreamReport {
        state: StreamState::from_u8(body[state::STATE])?,
        changed_at: Frames::new(read_u64(body, state::CHANGED_AT)),
        xruns: read_u32(body, state::XRUNS),
        xrun_frames: read_u64(body, state::XRUN_FRAMES),
    })
}

/// Largest reply any audio-service request produces.
///
/// Computed rather than naming whichever reply is biggest today: widening one
/// payload must not silently leave every buffer in the contract short.
pub const AUDIO_MAX_REPLY: usize = largest_reply();

const fn largest_reply() -> usize {
    let mut largest = AUDIO_ENUMERATE_REPLY_LEN;
    if AUDIO_OPEN_REPLY_LEN > largest {
        largest = AUDIO_OPEN_REPLY_LEN;
    }
    if AUDIO_CLOCK_REPLY_LEN > largest {
        largest = AUDIO_CLOCK_REPLY_LEN;
    }
    if AUDIO_STATE_REPLY_LEN > largest {
        largest = AUDIO_STATE_REPLY_LEN;
    }
    if crate::reply::STATUS_REPLY_LEN > largest {
        largest = crate::reply::STATUS_REPLY_LEN;
    }
    largest
}

const _: () = assert!(AUDIO_MAX_REPLY >= AUDIO_ENUMERATE_REPLY_LEN);
const _: () = assert!(AUDIO_MAX_REPLY >= AUDIO_OPEN_REPLY_LEN);
const _: () = assert!(AUDIO_MAX_REPLY >= AUDIO_CLOCK_REPLY_LEN);
const _: () = assert!(AUDIO_MAX_REPLY >= AUDIO_STATE_REPLY_LEN);
const _: () = assert!(AUDIO_MAX_REPLY >= crate::reply::STATUS_REPLY_LEN);
const _: () = assert!(AUDIO_MAX_REQUEST >= HEADER_LEN + open::LEN);

/// Byte offsets within an [`AudioNotify`] frame.
mod notify {
    pub const KIND: usize = 6;
    pub const STATE: usize = 7;
    pub const STREAM: usize = 8;
    pub const POSITION: usize = 16;
    pub const LOST_FRAMES: usize = 24;
    pub const LEN: usize = 32;
}

/// Wire length of an [`AudioNotify`] frame.
pub const AUDIO_NOTIFY_LEN: usize = notify::LEN;

/// Wire byte for a space-available notification.
const NOTIFY_SPACE: u8 = 1;
/// Wire byte for a state-change notification.
const NOTIFY_STATE: u8 = 2;
/// Wire byte for an under/over-run notification.
const NOTIFY_XRUN: u8 = 3;

/// The service → client wake, sent to the stream's
/// [`StreamGrant::notify_endpoint`].
///
/// A client parks on this port in its wait set and never polls its ring.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AudioNotify {
    /// The ring has room for more frames (playback), or frames have arrived in
    /// it (capture). `position` is the consumer position the service has
    /// reached.
    SpaceAvailable {
        /// The stream whose ring moved.
        stream_id: u64,
        /// The service's position in that ring.
        position: Frames,
    },
    /// The stream changed state, and here is the exact frame it changed at —
    /// so a seat switch resumes from the frame it paused on rather than near
    /// it.
    StateChanged {
        /// The stream that changed.
        stream_id: u64,
        /// Its new state.
        state: StreamState,
        /// The position it changed at.
        at: Frames,
    },
    /// Frames were lost. The position never lies, so a client resynchronises
    /// exactly rather than drifting.
    Xrun {
        /// The stream that lost frames.
        stream_id: u64,
        /// The position the loss started at.
        at: Frames,
        /// How many frames were lost.
        lost_frames: u64,
    },
}

impl AudioNotify {
    /// Encoded length of the notify frame.
    pub const WIRE_LEN: usize = AUDIO_NOTIFY_LEN;

    /// Encode the notify frame.
    #[must_use]
    pub fn encode(&self) -> [u8; AUDIO_NOTIFY_LEN] {
        let mut out = [0u8; AUDIO_NOTIFY_LEN];
        put_u32(&mut out, 0, AUDIO_NOTIFY_MAGIC);
        put_u16(&mut out, 4, AUDIO_VERSION_V1);
        match self {
            Self::SpaceAvailable {
                stream_id,
                position,
            } => {
                out[notify::KIND] = NOTIFY_SPACE;
                put_u64(&mut out, notify::STREAM, *stream_id);
                put_u64(&mut out, notify::POSITION, position.get());
            }
            Self::StateChanged {
                stream_id,
                state,
                at,
            } => {
                out[notify::KIND] = NOTIFY_STATE;
                out[notify::STATE] = state.as_u8();
                put_u64(&mut out, notify::STREAM, *stream_id);
                put_u64(&mut out, notify::POSITION, at.get());
            }
            Self::Xrun {
                stream_id,
                at,
                lost_frames,
            } => {
                out[notify::KIND] = NOTIFY_XRUN;
                put_u64(&mut out, notify::STREAM, *stream_id);
                put_u64(&mut out, notify::POSITION, at.get());
                put_u64(&mut out, notify::LOST_FRAMES, *lost_frames);
            }
        }
        out
    }

    /// Decode a notify frame, fail-closed.
    ///
    /// A field the notification's own kind does not define must be zero: a
    /// populated one would be a value the decoder never looked at.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — shorter than [`AUDIO_NOTIFY_LEN`].
    /// * [`Errno::BadMagic`] — wrong magic or a field the kind does not
    ///   define.
    /// * [`Errno::AbiVersionUnsupported`] — not [`AUDIO_VERSION_V1`].
    /// * [`Errno::OutOfRange`] — an unknown kind byte, a zero stream id, or an
    ///   undefined state.
    pub fn decode(bytes: &[u8]) -> Result<Self, Errno> {
        let Some(bytes) = bytes.get(..AUDIO_NOTIFY_LEN) else {
            return Err(Errno::BufferTooSmall);
        };
        if read_u32(bytes, 0) != AUDIO_NOTIFY_MAGIC {
            return Err(Errno::BadMagic);
        }
        if read_u16(bytes, 4) != AUDIO_VERSION_V1 {
            return Err(Errno::AbiVersionUnsupported);
        }
        let stream_id = checked_stream(read_u64(bytes, notify::STREAM))?;
        let position = Frames::new(read_u64(bytes, notify::POSITION));
        let lost_frames = read_u64(bytes, notify::LOST_FRAMES);
        match bytes[notify::KIND] {
            NOTIFY_SPACE => {
                if bytes[notify::STATE] != 0 || lost_frames != 0 {
                    return Err(Errno::BadMagic);
                }
                Ok(Self::SpaceAvailable {
                    stream_id,
                    position,
                })
            }
            NOTIFY_STATE => {
                if lost_frames != 0 {
                    return Err(Errno::BadMagic);
                }
                Ok(Self::StateChanged {
                    stream_id,
                    state: StreamState::from_u8(bytes[notify::STATE])?,
                    at: position,
                })
            }
            NOTIFY_XRUN => {
                if bytes[notify::STATE] != 0 {
                    return Err(Errno::BadMagic);
                }
                Ok(Self::Xrun {
                    stream_id,
                    at: position,
                    lost_frames,
                })
            }
            _ => Err(Errno::OutOfRange),
        }
    }
}

#[cfg(test)]
#[path = "audio_tests.rs"]
mod tests;
