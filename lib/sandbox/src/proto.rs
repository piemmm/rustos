//! The length-framed byte protocol both sides of the sandbox boundary
//! speak.
//!
//! A frame is a 4-byte little-endian payload length followed by exactly
//! that many payload bytes, bounded by [`MAX_FRAME`]. The framing is
//! deliberately this small: the sandbox boundary carries opaque payloads
//! whose meaning belongs to the service (`crate::worker::Service`), and a
//! smaller protocol is a smaller attack surface. Both directions are
//! hostile to their reader — the worker has parsed attacker-controlled
//! bytes and may be compromised; the parent could in principle be buggy —
//! so both sides refuse an oversize declared length *before* allocating or
//! reading a payload byte (fail closed).
//!
//! The transport is any [`Channel`]: the production pipes on a TAIRiX
//! target, an in-memory fake in host tests.

use alloc::vec;
use alloc::vec::Vec;
use tairix_abi::Errno;

/// Largest payload a single frame may carry, in bytes.
///
/// This is a fixed validation bound on the untrusted channel, not a
/// growable capacity: a declared length above it is refused before any
/// allocation, so neither side can be driven into a huge read by a hostile
/// peer. The value is sized for the seam's real workloads — an executable
/// container prefix or a decode window plus its framed reply — with
/// comfortable headroom.
pub const MAX_FRAME: usize = 8 << 20;

/// Number of bytes in a frame header (the little-endian payload length).
pub const FRAME_HEADER_LEN: usize = 4;

/// A blocking, bidirectional byte stream the protocol runs over.
///
/// The production implementation is a pipe pair; host tests use in-memory
/// fakes. Semantics follow the kernel pipe contract: `read` blocks until
/// bytes arrive and reports `Ok(0)` only at end-of-stream (every writer
/// closed); `write` blocks until at least one byte is accepted and fails
/// once no reader remains.
pub trait Channel {
    /// Read up to `buf.len()` bytes, blocking until at least one arrives.
    /// `Ok(0)` means end-of-stream.
    ///
    /// # Errors
    ///
    /// The transport's typed failure.
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Errno>;

    /// Write up to `buf.len()` bytes, blocking until at least one is
    /// accepted, and report how many were.
    ///
    /// # Errors
    ///
    /// The transport's typed failure (e.g. [`Errno::BrokenPipe`] once no
    /// reader remains).
    fn write(&mut self, buf: &[u8]) -> Result<usize, Errno>;
}

/// Typed framing failure.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ProtoError {
    /// The stream ended mid-frame, or a write found no reader: the peer is
    /// gone.
    PeerClosed,
    /// The transport itself failed with the carried error.
    Channel(Errno),
    /// The peer declared a payload longer than [`MAX_FRAME`]. Refused
    /// before any payload byte is read or allocated.
    Oversize,
}

/// Send one frame: the header, then the whole payload.
///
/// # Errors
///
/// [`ProtoError::Oversize`] when `payload` exceeds [`MAX_FRAME`] (nothing
/// is written); [`ProtoError::PeerClosed`] / [`ProtoError::Channel`] on
/// transport failure.
pub fn send_frame<C: Channel>(chan: &mut C, payload: &[u8]) -> Result<(), ProtoError> {
    if payload.len() > MAX_FRAME {
        return Err(ProtoError::Oversize);
    }
    // The bound above keeps the length in u32 range on every target.
    let len = u32::try_from(payload.len()).map_err(|_| ProtoError::Oversize)?;
    write_all(chan, &len.to_le_bytes())?;
    write_all(chan, payload)
}

/// Receive one frame's payload.
///
/// `Ok(None)` is the clean end of the conversation: the stream ended
/// exactly on a frame boundary (the peer finished and closed). A stream
/// that ends *inside* a frame is [`ProtoError::PeerClosed`] — a truncated
/// conversation is a failure, never silently shortened data.
///
/// # Errors
///
/// [`ProtoError::Oversize`] for a declared length above [`MAX_FRAME`]
/// (refused before any payload byte is read); [`ProtoError::PeerClosed`] /
/// [`ProtoError::Channel`] on transport failure.
pub fn recv_frame<C: Channel>(chan: &mut C) -> Result<Option<Vec<u8>>, ProtoError> {
    let mut header = [0u8; FRAME_HEADER_LEN];
    match read_exact(chan, &mut header)? {
        ReadOutcome::Eof => return Ok(None),
        ReadOutcome::Filled => {}
    }
    let len = u32::from_le_bytes(header) as usize;
    if len > MAX_FRAME {
        return Err(ProtoError::Oversize);
    }
    let mut payload = vec![0u8; len];
    match read_exact(chan, &mut payload)? {
        // EOF inside a declared payload: the peer died mid-frame.
        ReadOutcome::Eof => Err(ProtoError::PeerClosed),
        ReadOutcome::Filled => Ok(Some(payload)),
    }
}

/// Whether [`read_exact`] filled the buffer or hit end-of-stream before
/// its first byte.
enum ReadOutcome {
    Filled,
    Eof,
}

/// Fill `buf` completely, or report a clean `Eof` when the stream ends
/// before the first byte.
///
/// An end-of-stream after a partial fill is [`ProtoError::PeerClosed`]:
/// only a boundary EOF is clean.
fn read_exact<C: Channel>(chan: &mut C, buf: &mut [u8]) -> Result<ReadOutcome, ProtoError> {
    let mut at = 0;
    while at < buf.len() {
        match chan.read(&mut buf[at..]) {
            Ok(0) if at == 0 => return Ok(ReadOutcome::Eof),
            Ok(0) => return Err(ProtoError::PeerClosed),
            Ok(read) => at += read.min(buf.len() - at),
            Err(errno) => return Err(map_errno(errno)),
        }
    }
    Ok(ReadOutcome::Filled)
}

/// Write every byte of `buf`.
fn write_all<C: Channel>(chan: &mut C, buf: &[u8]) -> Result<(), ProtoError> {
    let mut at = 0;
    while at < buf.len() {
        match chan.write(&buf[at..]) {
            // A zero-byte write cannot make progress; treat it as the
            // peer being gone rather than spinning.
            Ok(0) => return Err(ProtoError::PeerClosed),
            Ok(wrote) => at += wrote.min(buf.len() - at),
            Err(errno) => return Err(map_errno(errno)),
        }
    }
    Ok(())
}

/// Fold a transport errno into the protocol vocabulary: a vanished peer
/// is [`ProtoError::PeerClosed`]; everything else carries its errno.
fn map_errno(errno: Errno) -> ProtoError {
    if errno == Errno::BrokenPipe {
        ProtoError::PeerClosed
    } else {
        ProtoError::Channel(errno)
    }
}

#[cfg(test)]
mod tests {
    use super::{recv_frame, send_frame, Channel, ProtoError, MAX_FRAME};
    use alloc::vec;
    use alloc::vec::Vec;
    use tairix_abi::Errno;

    /// In-memory loopback: reads consume `input`, writes append to
    /// `output`, one byte at a time to exercise the short-read/short-write
    /// loops.
    struct Loopback {
        input: Vec<u8>,
        at: usize,
        output: Vec<u8>,
        write_fails: Option<Errno>,
    }

    impl Loopback {
        fn over(input: Vec<u8>) -> Self {
            Self {
                input,
                at: 0,
                output: Vec::new(),
                write_fails: None,
            }
        }
    }

    impl Channel for Loopback {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, Errno> {
            if self.at == self.input.len() || buf.is_empty() {
                return Ok(0);
            }
            buf[0] = self.input[self.at];
            self.at += 1;
            Ok(1)
        }

        fn write(&mut self, buf: &[u8]) -> Result<usize, Errno> {
            if let Some(errno) = self.write_fails {
                return Err(errno);
            }
            if buf.is_empty() {
                return Ok(0);
            }
            self.output.push(buf[0]);
            Ok(1)
        }
    }

    #[test]
    fn a_frame_round_trips_through_the_byte_at_a_time_channel() {
        let mut sender = Loopback::over(Vec::new());
        send_frame(&mut sender, b"payload").expect("send succeeds");
        let mut receiver = Loopback::over(sender.output);
        let got = recv_frame(&mut receiver).expect("recv succeeds");
        assert_eq!(got.as_deref(), Some(&b"payload"[..]));
    }

    #[test]
    fn an_empty_payload_is_a_legal_frame() {
        let mut sender = Loopback::over(Vec::new());
        send_frame(&mut sender, b"").expect("send succeeds");
        let mut receiver = Loopback::over(sender.output);
        assert_eq!(recv_frame(&mut receiver), Ok(Some(Vec::new())));
    }

    #[test]
    fn eof_on_a_frame_boundary_is_the_clean_end() {
        let mut receiver = Loopback::over(Vec::new());
        assert_eq!(recv_frame(&mut receiver), Ok(None));
    }

    #[test]
    fn eof_inside_the_header_or_payload_is_peer_closed() {
        // One header byte then EOF.
        let mut receiver = Loopback::over(vec![3]);
        assert_eq!(recv_frame(&mut receiver), Err(ProtoError::PeerClosed));
        // A full header declaring three bytes, then only one.
        let mut bytes = 3u32.to_le_bytes().to_vec();
        bytes.push(b'x');
        let mut receiver = Loopback::over(bytes);
        assert_eq!(recv_frame(&mut receiver), Err(ProtoError::PeerClosed));
    }

    #[test]
    fn a_declared_length_over_the_cap_is_refused_before_any_payload_read() {
        let declared = u32::try_from(MAX_FRAME + 1).expect("fits");
        let mut receiver = Loopback::over(declared.to_le_bytes().to_vec());
        assert_eq!(recv_frame(&mut receiver), Err(ProtoError::Oversize));
    }

    #[test]
    fn sending_an_oversize_payload_is_refused_without_writing() {
        let mut sender = Loopback::over(Vec::new());
        let payload = vec![0u8; MAX_FRAME + 1];
        assert_eq!(send_frame(&mut sender, &payload), Err(ProtoError::Oversize));
        assert!(sender.output.is_empty());
    }

    #[test]
    fn a_broken_pipe_write_reports_the_peer_gone() {
        let mut sender = Loopback::over(Vec::new());
        sender.write_fails = Some(Errno::BrokenPipe);
        assert_eq!(send_frame(&mut sender, b"x"), Err(ProtoError::PeerClosed));
    }

    #[test]
    fn any_other_transport_errno_is_carried_verbatim() {
        let mut sender = Loopback::over(Vec::new());
        sender.write_fails = Some(Errno::BadAddress);
        assert_eq!(
            send_frame(&mut sender, b"x"),
            Err(ProtoError::Channel(Errno::BadAddress))
        );
    }
}
