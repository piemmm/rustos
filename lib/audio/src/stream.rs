//! The client half of `audio-v1`: the code a program links to play or record
//! sound.
//!
//! There is one transport and no bypass, so this is the whole of a program's
//! audio surface. It holds no capability, opens no endpoint and issues no
//! syscall: the IPC round trip and the park on the notify mailbox are the
//! caller's, supplied through [`AudioTransport`], which keeps the client
//! host-testable against a mock service and keeps this crate free of I/O.
//!
//! # Writing at a position, not into a buffer
//!
//! A client names the **frame** its samples belong at. Where that is ahead of
//! what the ring already carries, the distance is closed with the format's own
//! silence, so the position the service reads never lies about where the
//! samples that follow it belong — which is what makes gapless playback and
//! synchronisation exact arithmetic. Where it is behind, the write is refused:
//! those frames are published and may already have been played, and quietly
//! dropping the request would leave the caller believing they were not.
//!
//! # Parking, never polling
//!
//! A client with a full ring parks on the stream's notify mailbox and is woken
//! when the service has taken frames. There is no retry loop and no sleep.

use tairix_abi::audio::{
    decode_clock_reply, decode_open_reply, decode_state_reply, AudioNotify, AudioRequest,
    ClockReport, OpenParams, StreamGrant, StreamReport, StreamState, AUDIO_MAX_REPLY,
    AUDIO_MAX_REQUEST, AUDIO_NOTIFY_LEN,
};
use tairix_abi::driver::audio::{Frames, StreamDirection};
use tairix_abi::driver::audio_ring::PcmRing;
use tairix_abi::reply::decode_status_reply;
use tairix_abi::Errno;

/// The caller's IPC, injected so this crate performs none of its own.
pub trait AudioTransport {
    /// Send `request` to the audio service and receive its reply, returning
    /// the reply's length.
    ///
    /// # Errors
    ///
    /// The transport's own [`Errno`] — a vanished endpoint, a refused send.
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno>;

    /// Park on the stream's notify mailbox until the service wakes it, then
    /// take the frame.
    ///
    /// Blocking by contract: a caller that returns immediately with nothing
    /// turns the client's wait into the busy loop the charter forbids.
    ///
    /// # Errors
    ///
    /// The transport's own [`Errno`].
    fn wait_notify(&mut self, out: &mut [u8]) -> Result<usize, Errno>;
}

/// What one [`StreamClient::write_at`] moved.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct Written {
    /// Frames of gap-filling silence published before the samples.
    pub silence_frames: u32,
    /// Frames of the caller's own samples published.
    pub sample_frames: u32,
}

impl Written {
    /// Whether the ring took nothing at all, so the caller should park.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.silence_frames == 0 && self.sample_frames == 0
    }
}

/// One open stream.
///
/// Holds the grant and the last state the service reported, and nothing else:
/// the ring's positions live in the ring, so there is no second copy of them
/// here to drift from the truth.
///
/// Deliberately neither [`Copy`] nor [`Clone`]: it is a handle on a service
/// resource, and [`Self::close`] consumes it so a closed stream cannot be
/// operated on. A duplicable handle would make that consumption decorative
/// and let two holders each believe they owned the stream.
#[derive(Debug)]
pub struct StreamClient {
    grant: StreamGrant,
    direction: StreamDirection,
    state: StreamState,
    /// The position the last state change happened at, so a resume after a
    /// seat switch starts on the frame the pause stopped on.
    changed_at: Frames,
}

impl StreamClient {
    /// Open a stream and adopt whatever the service granted.
    ///
    /// The grant is not always the request: a device that cannot meet the
    /// asked-for rate, format or layout answers what it *can* meet, and the
    /// caller owns the conversion the difference implies. Nothing is
    /// resampled on its behalf, which is what keeps the bit-exact path a
    /// property rather than a mode.
    ///
    /// # Errors
    ///
    /// The service's refusal, or the transport's.
    pub fn open<T: AudioTransport>(transport: &mut T, params: &OpenParams) -> Result<Self, Errno> {
        let mut reply = [0u8; AUDIO_MAX_REPLY];
        let length = Self::send(transport, &AudioRequest::Open(*params), &mut reply)?;
        let grant = decode_open_reply(&reply[..length])?;
        Ok(Self {
            grant,
            direction: params.direction,
            state: StreamState::Idle,
            changed_at: Frames::ZERO,
        })
    }

    /// What the service granted.
    #[must_use]
    pub const fn grant(&self) -> StreamGrant {
        self.grant
    }

    /// Which way frames flow.
    #[must_use]
    pub const fn direction(&self) -> StreamDirection {
        self.direction
    }

    /// The last state the service reported.
    #[must_use]
    pub const fn state(&self) -> StreamState {
        self.state
    }

    /// The position the stream last changed state at — the frame a paused
    /// stream resumes from.
    #[must_use]
    pub const fn changed_at(&self) -> Frames {
        self.changed_at
    }

    /// Hand the service the shared region the grant's geometry describes.
    ///
    /// # Errors
    ///
    /// The service's refusal, or the transport's.
    pub fn attach<T: AudioTransport>(
        &self,
        transport: &mut T,
        region_grant: u64,
    ) -> Result<(), Errno> {
        Self::status(
            transport,
            &AudioRequest::Attach {
                stream_id: self.grant.stream_id,
                region_grant,
            },
        )
    }

    /// Begin moving frames, the first of them at `at`.
    ///
    /// # Errors
    ///
    /// The service's refusal, or the transport's.
    pub fn start<T: AudioTransport>(&mut self, transport: &mut T, at: Frames) -> Result<(), Errno> {
        Self::status(
            transport,
            &AudioRequest::Start {
                stream_id: self.grant.stream_id,
                at,
            },
        )?;
        self.state = StreamState::Running;
        self.changed_at = at;
        Ok(())
    }

    /// Stop at `at`, holding the position so a resume is exact.
    ///
    /// # Errors
    ///
    /// The service's refusal, or the transport's.
    pub fn stop<T: AudioTransport>(&mut self, transport: &mut T, at: Frames) -> Result<(), Errno> {
        Self::status(
            transport,
            &AudioRequest::Stop {
                stream_id: self.grant.stream_id,
                at,
            },
        )?;
        self.state = StreamState::Paused;
        self.changed_at = at;
        Ok(())
    }

    /// Play out everything queued, then stop.
    ///
    /// # Errors
    ///
    /// The service's refusal, or the transport's.
    pub fn drain<T: AudioTransport>(&mut self, transport: &mut T) -> Result<(), Errno> {
        Self::status(
            transport,
            &AudioRequest::Drain {
                stream_id: self.grant.stream_id,
            },
        )?;
        self.state = StreamState::Draining;
        Ok(())
    }

    /// Discard everything queued. The position advances over the discarded
    /// frames, so what follows still belongs where the arithmetic says.
    ///
    /// # Errors
    ///
    /// The service's refusal, or the transport's.
    pub fn flush<T: AudioTransport>(&self, transport: &mut T) -> Result<(), Errno> {
        Self::status(
            transport,
            &AudioRequest::Flush {
                stream_id: self.grant.stream_id,
            },
        )
    }

    /// Read the device clock this stream is in the domain of.
    ///
    /// # Errors
    ///
    /// The service's refusal, or the transport's.
    pub fn clock<T: AudioTransport>(&self, transport: &mut T) -> Result<ClockReport, Errno> {
        let mut reply = [0u8; AUDIO_MAX_REPLY];
        let length = Self::send(
            transport,
            &AudioRequest::Clock {
                stream_id: self.grant.stream_id,
            },
            &mut reply,
        )?;
        decode_clock_reply(&reply[..length])
    }

    /// Set this stream's own gain. Zero is unity, and unity is bit-exact.
    ///
    /// # Errors
    ///
    /// The service's refusal, or the transport's.
    pub fn set_gain<T: AudioTransport>(
        &self,
        transport: &mut T,
        millibel: i32,
    ) -> Result<(), Errno> {
        Self::status(
            transport,
            &AudioRequest::Gain {
                stream_id: self.grant.stream_id,
                millibel,
            },
        )
    }

    /// Mute or unmute, independently of the gain.
    ///
    /// # Errors
    ///
    /// The service's refusal, or the transport's.
    pub fn set_mute<T: AudioTransport>(&self, transport: &mut T, muted: bool) -> Result<(), Errno> {
        Self::status(
            transport,
            &AudioRequest::Mute {
                stream_id: self.grant.stream_id,
                muted,
            },
        )
    }

    /// Read the stream's state, the position it changed at, and its glitch
    /// tallies, adopting the state locally.
    ///
    /// # Errors
    ///
    /// The service's refusal, or the transport's.
    pub fn report<T: AudioTransport>(&mut self, transport: &mut T) -> Result<StreamReport, Errno> {
        let mut reply = [0u8; AUDIO_MAX_REPLY];
        let length = Self::send(
            transport,
            &AudioRequest::State {
                stream_id: self.grant.stream_id,
            },
            &mut reply,
        )?;
        let report = decode_state_reply(&reply[..length])?;
        self.state = report.state;
        self.changed_at = report.changed_at;
        Ok(report)
    }

    /// Close the stream and release its region.
    ///
    /// # Errors
    ///
    /// The service's refusal, or the transport's.
    pub fn close<T: AudioTransport>(self, transport: &mut T) -> Result<(), Errno> {
        Self::status(
            transport,
            &AudioRequest::Close {
                stream_id: self.grant.stream_id,
            },
        )
    }

    /// Park until the service reports something about this stream, adopting
    /// any state change it carries.
    ///
    /// A notification for another stream is returned as it arrived rather
    /// than swallowed: a client with several streams demultiplexes on the
    /// stream id it names.
    ///
    /// # Errors
    ///
    /// * [`Errno::BadMagic`] / [`Errno::OutOfRange`] — a malformed frame.
    /// * The transport's own refusal.
    pub fn await_notify<T: AudioTransport>(
        &mut self,
        transport: &mut T,
    ) -> Result<AudioNotify, Errno> {
        let mut frame = [0u8; AUDIO_NOTIFY_LEN];
        let length = transport.wait_notify(&mut frame)?;
        let notify = AudioNotify::decode(&frame[..length])?;
        self.adopt(notify);
        Ok(notify)
    }

    /// Adopt a notification's state change, if it is this stream's.
    pub fn adopt(&mut self, notify: AudioNotify) {
        if let AudioNotify::StateChanged {
            stream_id,
            state,
            at,
        } = notify
        {
            if stream_id == self.grant.stream_id {
                self.state = state;
                self.changed_at = at;
            }
        }
    }

    /// Publish `samples` so their first frame lands at `at`.
    ///
    /// A gap between the ring's producer position and `at` is closed with the
    /// format's own silence first, so a client that skipped material says so
    /// rather than sliding everything after it earlier. A short result means
    /// the ring filled: park on the notify mailbox and offer the remainder,
    /// never retry in a loop.
    ///
    /// # Errors
    ///
    /// * [`Errno::OutOfRange`] — `at` is behind the ring's producer position.
    ///   Those frames are published and may already be audible.
    /// * [`Errno::LengthOutOfRange`] — `samples` is not a whole number of
    ///   frames.
    /// * Whatever the ring refuses when its peer's positions are corrupt.
    pub fn write_at(
        &self,
        ring: &mut PcmRing<'_>,
        at: Frames,
        samples: &[u8],
    ) -> Result<Written, Errno> {
        if self.direction != StreamDirection::Playback {
            return Err(Errno::NotSupported);
        }
        let producer = ring.producer_position()?;
        let gap = at.since(producer).ok_or(Errno::OutOfRange)?;
        let mut written = Written::default();
        if gap > 0 {
            let wanted = u32::try_from(gap).unwrap_or(u32::MAX);
            written.silence_frames = ring.write_silence(wanted)?;
            if written.silence_frames < wanted {
                // The ring filled inside the gap. The caller's samples still
                // belong at `at`, so they are offered again once there is
                // room rather than published at the wrong position.
                return Ok(written);
            }
        }
        written.sample_frames = ring.write(samples)?;
        Ok(written)
    }

    /// Take up to `out`'s capacity of captured frames, returning how many
    /// arrived.
    ///
    /// # Errors
    ///
    /// * [`Errno::NotSupported`] — the stream is a sink, so nothing arrives
    ///   on it.
    /// * Whatever the ring refuses when its peer's positions are corrupt.
    pub fn read_into(&self, ring: &mut PcmRing<'_>, out: &mut [u8]) -> Result<u32, Errno> {
        if self.direction != StreamDirection::Capture {
            return Err(Errno::NotSupported);
        }
        ring.read(out)
    }

    /// Issue `request` and expect the shared status-only reply.
    fn status<T: AudioTransport>(transport: &mut T, request: &AudioRequest) -> Result<(), Errno> {
        let mut reply = [0u8; AUDIO_MAX_REPLY];
        let length = Self::send(transport, request, &mut reply)?;
        decode_status_reply(&reply[..length])
    }

    /// Encode `request`, hand it to the transport, and return the reply's
    /// length.
    fn send<T: AudioTransport>(
        transport: &mut T,
        request: &AudioRequest,
        reply: &mut [u8],
    ) -> Result<usize, Errno> {
        let mut frame = [0u8; AUDIO_MAX_REQUEST];
        let length = request.encode(&mut frame)?;
        transport.call(&frame[..length], reply)
    }
}

#[cfg(test)]
#[path = "stream_tests.rs"]
mod tests;
