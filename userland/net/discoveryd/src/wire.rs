//! The channel between the front and its sandboxed decoder.
//!
//! Every frame is one fixed layout behind a one-byte tag, so either side's
//! reader is a bounds-checked field read and nothing more: the front parses no
//! DNS, and what it reads from its decoder is a flag and an integer. Both
//! directions fail closed — a frame whose tag, length, or any field is not
//! exactly what an honest peer sends is refused whole.
//!
//! Encoders write into a buffer the caller holds, because the relay encodes
//! every datagram a segment carries and must not allocate per datagram.

use tairix_abi::net::SOCKET_MAX_DATAGRAM;
use tairix_abi::net_ipc::{
    address_parts, ip_from_parts, validate_if_name, NetAddrFamily, IF_NAME_LEN,
};
use tairix_net::IpAddr;
use tairix_sandbox::wire::{Reader, WireError};

/// Bytes of the key the decoder's caches are indexed under.
pub const CACHE_KEY_LEN: usize = tairix_hash::HashSeed::LEN;

/// Bytes of the key the decoder's CSPRNG is seeded from.
pub const RNG_KEY_LEN: usize = tairix_rng::STREAM_KEY_LEN;

const TAG_CONFIGURE: u8 = 1;
const TAG_DATAGRAM: u8 = 2;
const TAG_TICK: u8 = 3;
const TAG_DEADLINE: u8 = 1;

/// Length of a [`ToDecoder::Configure`] frame.
pub const CONFIGURE_LEN: usize = 1 + CACHE_KEY_LEN + RNG_KEY_LEN;

/// Length of a [`ToDecoder::Datagram`] frame before its payload: tag, time,
/// interface, address family, address, port.
pub const DATAGRAM_HEADER_LEN: usize = 1 + 8 + IF_NAME_LEN + 1 + 16 + 2;

/// Length of a [`ToDecoder::Tick`] frame.
pub const TICK_LEN: usize = 1 + 8;

/// Length of a [`FromDecoder::Deadline`] frame.
pub const DEADLINE_LEN: usize = 1 + 1 + 8;

/// The longest frame the front sends: a datagram carrying the largest
/// payload the stack delivers.
pub const MAX_TO_DECODER: usize = DATAGRAM_HEADER_LEN + SOCKET_MAX_DATAGRAM;

/// The longest frame the decoder sends.
pub const MAX_FROM_DECODER: usize = DEADLINE_LEN;

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
}

impl<'a> ToDecoder<'a> {
    /// Encode into the front of `out`, returning the frame's length, or
    /// `None` when `out` cannot hold it or a datagram is longer than the
    /// stack ever delivers.
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
        }
    }

    /// Decode one frame, refusing any an honest front cannot have sent.
    ///
    /// # Errors
    ///
    /// [`WireError`] for a short frame, an unknown tag, trailing bytes, an
    /// interface name the stack would never use, an unknown address family,
    /// an IPv4 address with a dirty tail, or an over-long datagram.
    pub fn decode(frame: &'a [u8]) -> Result<Self, WireError> {
        let mut reader = Reader::new(frame);
        let decoded = match reader.u8()? {
            TAG_CONFIGURE => Self::Configure {
                cache_key: fixed(&mut reader)?,
                rng_key: fixed(&mut reader)?,
            },
            TAG_DATAGRAM => {
                let now = reader.u64()?;
                let interface = *fixed::<IF_NAME_LEN>(&mut reader)?;
                validate_if_name(&interface).map_err(|_| WireError::Malformed)?;
                let family =
                    NetAddrFamily::from_u8(reader.u8()?).map_err(|_| WireError::Malformed)?;
                let addr = *fixed::<16>(&mut reader)?;
                if family == NetAddrFamily::V4 && addr[4..].iter().any(|&byte| byte != 0) {
                    return Err(WireError::Malformed);
                }
                let port = u16::from_le_bytes(*fixed(&mut reader)?);
                let payload = reader.take(frame.len().saturating_sub(DATAGRAM_HEADER_LEN))?;
                if payload.len() > SOCKET_MAX_DATAGRAM {
                    return Err(WireError::Malformed);
                }
                Self::Datagram {
                    now,
                    interface,
                    source: ip_from_parts(family, addr),
                    port,
                    payload,
                }
            }
            TAG_TICK => Self::Tick { now: reader.u64()? },
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
pub enum FromDecoder {
    /// The earliest instant, in monotonic nanoseconds, at which any of the
    /// decoder's engines has work — or `None` while none has any.
    Deadline(Option<u64>),
}

impl FromDecoder {
    /// Encode the frame.
    #[must_use]
    pub fn encode(&self) -> [u8; DEADLINE_LEN] {
        let Self::Deadline(at) = *self;
        let mut frame = [0u8; DEADLINE_LEN];
        frame[0] = TAG_DEADLINE;
        if let Some(at) = at {
            frame[1] = 1;
            frame[2..].copy_from_slice(&at.to_le_bytes());
        }
        frame
    }

    /// Decode one frame, refusing any an honest decoder cannot have sent.
    ///
    /// # Errors
    ///
    /// [`WireError`] for a short frame, an unknown tag, trailing bytes, a
    /// presence flag other than `0` or `1`, or an absent deadline whose
    /// instant is not zero.
    pub fn decode(frame: &[u8]) -> Result<Self, WireError> {
        let mut reader = Reader::new(frame);
        if reader.u8()? != TAG_DEADLINE {
            return Err(WireError::Malformed);
        }
        let present = reader.u8()?;
        let at = reader.u64()?;
        if !reader.is_exhausted() {
            return Err(WireError::Malformed);
        }
        match (present, at) {
            (0, 0) => Ok(Self::Deadline(None)),
            (1, at) => Ok(Self::Deadline(Some(at))),
            _ => Err(WireError::Malformed),
        }
    }
}

/// Read `N` raw bytes as a fixed array.
fn fixed<'a, const N: usize>(reader: &mut Reader<'a>) -> Result<&'a [u8; N], WireError> {
    reader.take(N)?.try_into().map_err(|_| WireError::Truncated)
}

#[cfg(test)]
#[path = "wire_tests.rs"]
mod tests;
