//! The channel between the front and its sandboxed decoder.
//!
//! Every frame is one fixed layout behind a one-byte tag, so either side's
//! reader is a bounds-checked field read and nothing more. The front parses no
//! DNS: an answer crosses as a [`discovery_ipc`](tairix_abi::discovery_ipc)
//! entry, already typed, and a datagram to send crosses unread. Both
//! directions fail closed — a frame whose tag, length, or any field is not
//! exactly what an honest peer sends is refused whole.
//!
//! Encoders write into a buffer the caller holds, because the relay encodes
//! every datagram a segment carries and must not allocate per datagram.

use tairix_abi::discovery_ipc::{Entry, ENTRY_MAX, NAME_MAX};
use tairix_abi::net::SOCKET_MAX_DATAGRAM;
use tairix_abi::net_ipc::{
    address_parts, ip_from_parts, validate_if_name, NetAddrFamily, IF_NAME_LEN,
};
use tairix_net::dns::RecordType;
use tairix_net::mdns::{Destination, MAX_MESSAGE_LEN};
use tairix_net::IpAddr;
use tairix_sandbox::wire::{Reader, WireError};

/// Bytes of the key the decoder's caches are indexed under.
pub const CACHE_KEY_LEN: usize = tairix_hash::HashSeed::LEN;

/// Bytes of the key the decoder's CSPRNG is seeded from.
pub const RNG_KEY_LEN: usize = tairix_rng::STREAM_KEY_LEN;

const TAG_CONFIGURE: u8 = 1;
const TAG_DATAGRAM: u8 = 2;
const TAG_TICK: u8 = 3;
const TAG_LINK: u8 = 4;
const TAG_ASK: u8 = 5;
const TAG_STOP: u8 = 6;
const TAG_REPLAY: u8 = 7;

const TAG_DEADLINE: u8 = 1;
const TAG_ANSWER: u8 = 2;
const TAG_HELD: u8 = 3;
const TAG_REPLAYED: u8 = 4;
const TAG_TRANSMIT: u8 = 5;
const TAG_LINKED: u8 = 6;

const TO_GROUP: u8 = 0;
const TO_PEER: u8 = 1;

/// Length of a [`ToDecoder::Configure`] frame.
pub const CONFIGURE_LEN: usize = 1 + CACHE_KEY_LEN + RNG_KEY_LEN;

/// Length of a [`ToDecoder::Datagram`] frame before its payload: tag, time,
/// interface, address family, address, port.
pub const DATAGRAM_HEADER_LEN: usize = 1 + 8 + IF_NAME_LEN + 1 + 16 + 2;

/// Length of a [`ToDecoder::Tick`] frame.
pub const TICK_LEN: usize = 1 + 8;

/// Length of a [`ToDecoder::Link`] frame.
pub const LINK_LEN: usize = 1 + 8 + IF_NAME_LEN + 1;

/// Length of a [`ToDecoder::Ask`] frame before its name: tag, time, question,
/// form, name length.
pub const ASK_HEADER_LEN: usize = 1 + 8 + 4 + 1 + 1;

/// Length of a [`ToDecoder::Stop`] frame.
pub const STOP_LEN: usize = 1 + 4;

/// Length of a [`ToDecoder::Replay`] frame.
pub const REPLAY_LEN: usize = 1 + 4 + 4;

/// Length of a [`FromDecoder::Deadline`] frame.
pub const DEADLINE_LEN: usize = 1 + 1 + 8;

/// Length of a [`FromDecoder::Replayed`] frame.
pub const REPLAYED_LEN: usize = 1 + 4;

/// Length of a [`FromDecoder::Linked`] frame.
pub const LINKED_LEN: usize = 1 + IF_NAME_LEN + 1;

/// Length of a [`FromDecoder::Transmit`] frame before its payload: tag,
/// interface, destination kind, family, address, port.
pub const TRANSMIT_HEADER_LEN: usize = 1 + IF_NAME_LEN + 1 + 1 + 16 + 2;

/// The longest frame the front sends: a datagram carrying the largest
/// payload the stack delivers.
pub const MAX_TO_DECODER: usize = DATAGRAM_HEADER_LEN + SOCKET_MAX_DATAGRAM;

/// The longest frame the decoder sends: a datagram to transmit.
pub const MAX_FROM_DECODER: usize = TRANSMIT_HEADER_LEN + MAX_MESSAGE_LEN;

const _: () = assert!(ASK_HEADER_LEN + NAME_MAX <= MAX_TO_DECODER);
const _: () = assert!(1 + 4 + ENTRY_MAX <= MAX_FROM_DECODER);

/// What a question's answers are read as, which fixes the record type it asks
/// for and the [`Answer`](tairix_abi::discovery_ipc::Answer) each record
/// becomes.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Form {
    /// A browse: `PTR` records naming instances of the type asked about.
    Instance = 1,
    /// A resolve's `SRV`.
    Service = 2,
    /// A resolve's `TXT`.
    Text = 3,
    /// A host's `A`.
    AddressV4 = 4,
    /// A host's `AAAA`.
    AddressV6 = 5,
    /// A reverse lookup's `PTR`.
    Pointer = 6,
    /// The RFC 6763 §9 type enumeration: `PTR` records naming types.
    Type = 7,
}

impl Form {
    /// The record type a question of this form asks for.
    #[must_use]
    pub const fn record_type(self) -> RecordType {
        match self {
            Self::Instance | Self::Pointer | Self::Type => RecordType::Ptr,
            Self::Service => RecordType::Srv,
            Self::Text => RecordType::Txt,
            Self::AddressV4 => RecordType::A,
            Self::AddressV6 => RecordType::Aaaa,
        }
    }

    const fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            1 => Self::Instance,
            2 => Self::Service,
            3 => Self::Text,
            4 => Self::AddressV4,
            5 => Self::AddressV6,
            6 => Self::Pointer,
            7 => Self::Type,
            _ => return None,
        })
    }
}

/// A frame the front sends its decoder.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToDecoder<'a> {
    /// The keys a fresh decoder works under. The first frame of every
    /// decoder, and never sent to it again.
    Configure {
        /// The key its caches are indexed under.
        cache_key: &'a [u8; CACHE_KEY_LEN],
        /// The key its CSPRNG is seeded from.
        rng_key: &'a [u8; RNG_KEY_LEN],
    },
    /// One datagram the stack delivered from a sender it found on-link.
    Datagram {
        /// The monotonic instant the front relayed it, in nanoseconds.
        now: u64,
        /// The logical interface it arrived on.
        interface: [u8; IF_NAME_LEN],
        /// The sender's address.
        source: IpAddr,
        /// The sender's port.
        port: u16,
        /// The datagram, unread.
        payload: &'a [u8],
    },
    /// The decoder's reported deadline has come.
    Tick {
        /// The monotonic instant, in nanoseconds.
        now: u64,
    },
    /// An interface came up, and is asked every question; or went down, and
    /// everything learned on it is forgotten.
    Link {
        /// The monotonic instant, in nanoseconds.
        now: u64,
        /// The interface.
        interface: [u8; IF_NAME_LEN],
        /// Whether it is up.
        up: bool,
    },
    /// Ask a question on every interface, now and as each comes up.
    Ask {
        /// The monotonic instant, in nanoseconds.
        now: u64,
        /// The front's id for it, never reused.
        question: u32,
        /// How its answers are read.
        form: Form,
        /// The name asked about, in canonical wire form.
        name: &'a [u8],
    },
    /// Stop asking a question.
    Stop {
        /// The question.
        question: u32,
    },
    /// Send every answer a question holds now, as [`FromDecoder::Held`] under
    /// `token`, then [`FromDecoder::Replayed`].
    Replay {
        /// The question.
        question: u32,
        /// The front's name for this replay.
        token: u32,
    },
}

impl<'a> ToDecoder<'a> {
    /// Encode into the front of `out`, returning the frame's length, or
    /// `None` when `out` cannot hold it or a field is longer than its bound.
    #[must_use]
    pub fn encode(&self, out: &mut [u8]) -> Option<usize> {
        match *self {
            Self::Configure { cache_key, rng_key } => {
                let frame = out.get_mut(..CONFIGURE_LEN)?;
                frame[0] = TAG_CONFIGURE;
                frame[1..=CACHE_KEY_LEN].copy_from_slice(cache_key);
                frame[1 + CACHE_KEY_LEN..].copy_from_slice(rng_key);
                Some(CONFIGURE_LEN)
            }
            Self::Datagram {
                now,
                interface,
                source,
                port,
                payload,
            } => {
                if payload.len() > SOCKET_MAX_DATAGRAM {
                    return None;
                }
                let len = DATAGRAM_HEADER_LEN + payload.len();
                let frame = out.get_mut(..len)?;
                let (family, addr) = address_parts(source);
                frame[0] = TAG_DATAGRAM;
                frame[1..9].copy_from_slice(&now.to_le_bytes());
                frame[9..9 + IF_NAME_LEN].copy_from_slice(&interface);
                frame[25] = family.as_u8();
                frame[26..42].copy_from_slice(&addr);
                frame[42..DATAGRAM_HEADER_LEN].copy_from_slice(&port.to_le_bytes());
                frame[DATAGRAM_HEADER_LEN..].copy_from_slice(payload);
                Some(len)
            }
            Self::Tick { now } => {
                let frame = out.get_mut(..TICK_LEN)?;
                frame[0] = TAG_TICK;
                frame[1..].copy_from_slice(&now.to_le_bytes());
                Some(TICK_LEN)
            }
            Self::Link { now, interface, up } => {
                let frame = out.get_mut(..LINK_LEN)?;
                frame[0] = TAG_LINK;
                frame[1..9].copy_from_slice(&now.to_le_bytes());
                frame[9..9 + IF_NAME_LEN].copy_from_slice(&interface);
                frame[9 + IF_NAME_LEN] = u8::from(up);
                Some(LINK_LEN)
            }
            Self::Ask {
                now,
                question,
                form,
                name,
            } => {
                if name.is_empty() || name.len() > NAME_MAX {
                    return None;
                }
                let len = ASK_HEADER_LEN + name.len();
                let frame = out.get_mut(..len)?;
                frame[0] = TAG_ASK;
                frame[1..9].copy_from_slice(&now.to_le_bytes());
                frame[9..13].copy_from_slice(&question.to_le_bytes());
                frame[13] = form as u8;
                frame[14] = u8::try_from(name.len()).ok()?;
                frame[ASK_HEADER_LEN..].copy_from_slice(name);
                Some(len)
            }
            Self::Stop { question } => {
                let frame = out.get_mut(..STOP_LEN)?;
                frame[0] = TAG_STOP;
                frame[1..].copy_from_slice(&question.to_le_bytes());
                Some(STOP_LEN)
            }
            Self::Replay { question, token } => {
                let frame = out.get_mut(..REPLAY_LEN)?;
                frame[0] = TAG_REPLAY;
                frame[1..5].copy_from_slice(&question.to_le_bytes());
                frame[5..].copy_from_slice(&token.to_le_bytes());
                Some(REPLAY_LEN)
            }
        }
    }

    /// Decode one frame, refusing any an honest front cannot have sent.
    ///
    /// # Errors
    ///
    /// [`WireError`] for a short frame, an unknown tag, trailing bytes, an
    /// interface name the stack would never use, an unknown address family or
    /// form, an IPv4 address with a dirty tail, a flag other than `0` or `1`,
    /// or a field longer than its bound.
    pub fn decode(frame: &'a [u8]) -> Result<Self, WireError> {
        let mut reader = Reader::new(frame);
        let decoded = match reader.u8()? {
            TAG_CONFIGURE => Self::Configure {
                cache_key: fixed(&mut reader)?,
                rng_key: fixed(&mut reader)?,
            },
            TAG_DATAGRAM => {
                let now = reader.u64()?;
                let interface = interface(&mut reader)?;
                let source = address(&mut reader)?;
                let port = u16::from_le_bytes(*fixed(&mut reader)?);
                let payload = reader.take(frame.len().saturating_sub(DATAGRAM_HEADER_LEN))?;
                if payload.len() > SOCKET_MAX_DATAGRAM {
                    return Err(WireError::Malformed);
                }
                Self::Datagram {
                    now,
                    interface,
                    source,
                    port,
                    payload,
                }
            }
            TAG_TICK => Self::Tick { now: reader.u64()? },
            TAG_LINK => Self::Link {
                now: reader.u64()?,
                interface: interface(&mut reader)?,
                up: flag(reader.u8()?)?,
            },
            TAG_ASK => {
                let now = reader.u64()?;
                let question = reader.u32()?;
                let form = Form::from_u8(reader.u8()?).ok_or(WireError::Malformed)?;
                let len = usize::from(reader.u8()?);
                if len == 0 {
                    return Err(WireError::Malformed);
                }
                Self::Ask {
                    now,
                    question,
                    form,
                    name: reader.take(len)?,
                }
            }
            TAG_STOP => Self::Stop {
                question: reader.u32()?,
            },
            TAG_REPLAY => Self::Replay {
                question: reader.u32()?,
                token: reader.u32()?,
            },
            _ => return Err(WireError::Malformed),
        };
        if !reader.is_exhausted() {
            return Err(WireError::Malformed);
        }
        Ok(decoded)
    }
}

/// A frame the decoder sends its front.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FromDecoder<'a> {
    /// The earliest instant, in monotonic nanoseconds, at which any of the
    /// decoder's engines has work — or `None` while none has any.
    Deadline(Option<u64>),
    /// One answer to a question moved; the entry's `request` field is the
    /// question.
    Answer(Entry<'a>),
    /// One answer a question held when a replay was asked for.
    Held {
        /// The replay.
        token: u32,
        /// The answer, its `request` field the question.
        entry: Entry<'a>,
    },
    /// Every answer of the replay `token` has been sent.
    Replayed {
        /// The replay.
        token: u32,
    },
    /// A datagram to send.
    Transmit {
        /// The interface to send it from.
        interface: [u8; IF_NAME_LEN],
        /// Where to.
        to: Destination,
        /// The datagram.
        payload: &'a [u8],
    },
    /// A [`ToDecoder::Link`] has been carried out: every frame before this
    /// one about `interface` described it as it was before the edge.
    Linked {
        /// The interface.
        interface: [u8; IF_NAME_LEN],
        /// Whether it is now up.
        up: bool,
    },
}

impl<'a> FromDecoder<'a> {
    /// Encode into the front of `out`, returning the frame's length, or
    /// `None` when `out` cannot hold it or a field is out of bounds.
    #[must_use]
    pub fn encode(&self, out: &mut [u8]) -> Option<usize> {
        match *self {
            Self::Deadline(at) => {
                let frame = out.get_mut(..DEADLINE_LEN)?;
                frame.fill(0);
                frame[0] = TAG_DEADLINE;
                if let Some(at) = at {
                    frame[1] = 1;
                    frame[2..].copy_from_slice(&at.to_le_bytes());
                }
                Some(DEADLINE_LEN)
            }
            Self::Answer(entry) => {
                let frame = out.get_mut(..1 + entry.wire_len())?;
                frame[0] = TAG_ANSWER;
                entry.encode(&mut frame[1..]).ok()?;
                Some(frame.len())
            }
            Self::Held { token, entry } => {
                let frame = out.get_mut(..5 + entry.wire_len())?;
                frame[0] = TAG_HELD;
                frame[1..5].copy_from_slice(&token.to_le_bytes());
                entry.encode(&mut frame[5..]).ok()?;
                Some(frame.len())
            }
            Self::Replayed { token } => {
                let frame = out.get_mut(..REPLAYED_LEN)?;
                frame[0] = TAG_REPLAYED;
                frame[1..].copy_from_slice(&token.to_le_bytes());
                Some(REPLAYED_LEN)
            }
            Self::Transmit {
                interface,
                to,
                payload,
            } => {
                if payload.is_empty() || payload.len() > MAX_MESSAGE_LEN {
                    return None;
                }
                let len = TRANSMIT_HEADER_LEN + payload.len();
                let frame = out.get_mut(..len)?;
                frame[..TRANSMIT_HEADER_LEN].fill(0);
                frame[0] = TAG_TRANSMIT;
                frame[1..=IF_NAME_LEN].copy_from_slice(&interface);
                if let Destination::Peer { addr, port } = to {
                    let (family, octets) = address_parts(addr);
                    frame[17] = TO_PEER;
                    frame[18] = family.as_u8();
                    frame[19..35].copy_from_slice(&octets);
                    frame[35..TRANSMIT_HEADER_LEN].copy_from_slice(&port.to_le_bytes());
                } else {
                    frame[17] = TO_GROUP;
                }
                frame[TRANSMIT_HEADER_LEN..].copy_from_slice(payload);
                Some(len)
            }
            Self::Linked { interface, up } => {
                let frame = out.get_mut(..LINKED_LEN)?;
                frame[0] = TAG_LINKED;
                frame[1..=IF_NAME_LEN].copy_from_slice(&interface);
                frame[1 + IF_NAME_LEN] = u8::from(up);
                Some(LINKED_LEN)
            }
        }
    }

    /// Decode one frame, refusing any an honest decoder cannot have sent.
    ///
    /// # Errors
    ///
    /// [`WireError`] for a short frame, an unknown tag, trailing bytes, a
    /// presence flag other than `0` or `1`, an absent deadline whose instant
    /// is not zero, an entry the discovery channel would refuse, an interface
    /// name the stack would never use, a group destination with an address, or
    /// a datagram empty or past [`MAX_MESSAGE_LEN`].
    pub fn decode(frame: &'a [u8]) -> Result<Self, WireError> {
        let mut reader = Reader::new(frame);
        let decoded = match reader.u8()? {
            TAG_DEADLINE => {
                let present = reader.u8()?;
                let at = reader.u64()?;
                match (present, at) {
                    (0, 0) => Self::Deadline(None),
                    (1, at) => Self::Deadline(Some(at)),
                    _ => return Err(WireError::Malformed),
                }
            }
            TAG_ANSWER => Self::Answer(entry(reader.take(frame.len() - 1)?)?),
            TAG_HELD => Self::Held {
                token: reader.u32()?,
                entry: entry(reader.take(frame.len().saturating_sub(5))?)?,
            },
            TAG_REPLAYED => Self::Replayed {
                token: reader.u32()?,
            },
            TAG_TRANSMIT => {
                let interface = interface(&mut reader)?;
                let kind = reader.u8()?;
                let family = reader.u8()?;
                let octets = *fixed::<16>(&mut reader)?;
                let port = u16::from_le_bytes(*fixed(&mut reader)?);
                let to = match kind {
                    TO_GROUP if family == 0 && port == 0 && octets == [0; 16] => Destination::Group,
                    TO_PEER => Destination::Peer {
                        addr: address_from(family, octets)?,
                        port,
                    },
                    _ => return Err(WireError::Malformed),
                };
                let payload = reader.take(frame.len().saturating_sub(TRANSMIT_HEADER_LEN))?;
                if payload.is_empty() || payload.len() > MAX_MESSAGE_LEN {
                    return Err(WireError::Malformed);
                }
                Self::Transmit {
                    interface,
                    to,
                    payload,
                }
            }
            TAG_LINKED => Self::Linked {
                interface: interface(&mut reader)?,
                up: flag(reader.u8()?)?,
            },
            _ => return Err(WireError::Malformed),
        };
        if !reader.is_exhausted() {
            return Err(WireError::Malformed);
        }
        Ok(decoded)
    }
}

/// Read `N` raw bytes as a fixed array.
fn fixed<'a, const N: usize>(reader: &mut Reader<'a>) -> Result<&'a [u8; N], WireError> {
    reader.take(N)?.try_into().map_err(|_| WireError::Truncated)
}

/// An interface name the stack would use.
fn interface(reader: &mut Reader<'_>) -> Result<[u8; IF_NAME_LEN], WireError> {
    let name = *fixed::<IF_NAME_LEN>(reader)?;
    validate_if_name(&name).map_err(|_| WireError::Malformed)?;
    Ok(name)
}

/// A family byte and sixteen octets, an IPv4 address's tail zero.
fn address(reader: &mut Reader<'_>) -> Result<IpAddr, WireError> {
    let family = reader.u8()?;
    address_from(family, *fixed::<16>(reader)?)
}

fn address_from(family: u8, octets: [u8; 16]) -> Result<IpAddr, WireError> {
    let family = NetAddrFamily::from_u8(family).map_err(|_| WireError::Malformed)?;
    if family == NetAddrFamily::V4 && octets[4..].iter().any(|&byte| byte != 0) {
        return Err(WireError::Malformed);
    }
    Ok(ip_from_parts(family, octets))
}

fn flag(byte: u8) -> Result<bool, WireError> {
    match byte {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(WireError::Malformed),
    }
}

/// An answer entry: an [`Entry::Answer`], never a flush or a loss, which only
/// the front writes.
fn entry(bytes: &[u8]) -> Result<Entry<'_>, WireError> {
    match Entry::decode(bytes) {
        Ok(entry @ Entry::Answer { .. }) => Ok(entry),
        _ => Err(WireError::Malformed),
    }
}

#[cfg(test)]
#[path = "wire_tests.rs"]
mod tests;
