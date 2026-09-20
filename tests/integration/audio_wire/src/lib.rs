//! The one definition both ends of the audio vertical agree on
//! (`plans/SOUND.md` SND4).
//!
//! The guest plays the [`sample`] ramp through the whole stack and QEMU's
//! `wav` audio backend writes what the emulated sound card received to a
//! file on the host; [`payload_matches_signal`] then checks that file holds
//! exactly those samples. Both halves read this crate, so "what was played"
//! and "what is asserted" cannot drift.
//!
//! # Why the run is deterministic rather than a race
//!
//! The ring is sized to hold the **whole** signal, so the guest writes every
//! frame *before* it starts the device. The device can then never run dry
//! however slowly the emulated machine runs, which is what lets the
//! assertion be sample-exact and the reported under-run tally be exactly
//! zero — a glitch would be a real defect rather than a slow host.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

/// The rate the vertical runs at, matching the `wav` backend's own.
pub const RATE_HZ: u32 = 48_000;

/// Interleaved channels. Stereo, so a channel swap or a stride error shows
/// up as a mismatch rather than as silence.
pub const CHANNELS: usize = 2;

/// Bytes one interleaved 16-bit stereo frame occupies.
pub const FRAME_BYTES: usize = CHANNELS * 2;

/// Frames the guest plays.
///
/// A quarter of a second, and comfortably inside [`RING_FRAMES`] so the whole
/// signal is queued before the device is started.
pub const SIGNAL_FRAMES: usize = 12_000;

/// The latency target the guest asks for, and therefore the ring it is
/// granted: a power of two holding the whole signal with room to spare, and
/// inside the PCM ring vocabulary's fixed ceiling.
pub const RING_FRAMES: u32 = 16_384;

/// The witness line the guest prints once the drain has completed and the
/// device has reported no lost frames.
pub const PASS_MARKER: &str = "AUDIO PASS";

/// Samples quieter than this count as silence when the host trims the
/// backend's own lead-in and tail.
///
/// Exactly zero: the signal never reaches zero on both channels at once (see
/// [`sample`]), and QEMU's backend writes true zeroes before the device
/// starts, so the trim cannot eat a frame the guest played.
pub const SILENCE: i16 = 0;

/// The `frame`th sample of channel `channel`.
///
/// A pair of counter ramps in opposite directions: every frame differs from
/// its neighbours, the two channels differ from each other, and no frame is
/// silent on both channels — so a dropped frame, a channel swap, a stride
/// error and a truncation are each a visible mismatch rather than a
/// plausible-looking waveform.
///
/// The right ramp is offset by one so the two zero crossings fall on
/// different frames: a frame silent on *every* channel would look like the
/// host backend's own lead-in and be trimmed away, which
/// [`payload_matches_signal`] relies on not happening.
#[must_use]
pub fn sample(frame: usize, channel: usize) -> i16 {
    // Below the ramp's period, so the narrowing is total rather than
    // merely expected to hold.
    let step = i16::try_from(frame % 2_000).unwrap_or(0);
    if channel == 0 {
        step - 1_000
    } else {
        999 - step
    }
}

/// Write the whole signal, interleaved little-endian `s16`, into `out`.
///
/// Answers the bytes written, which is zero when `out` cannot hold
/// [`SIGNAL_FRAMES`] frames — the caller sizes its buffer from this crate, so
/// a short buffer is its own bug and is refused rather than truncated.
pub fn fill_signal(out: &mut [u8]) -> usize {
    let bytes = SIGNAL_FRAMES * FRAME_BYTES;
    if out.len() < bytes {
        return 0;
    }
    for frame in 0..SIGNAL_FRAMES {
        for channel in 0..CHANNELS {
            let at = (frame * CHANNELS + channel) * 2;
            out[at..at + 2].copy_from_slice(&sample(frame, channel).to_le_bytes());
        }
    }
    bytes
}

/// Why a captured WAV did not hold the signal.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum WavMismatch {
    /// Shorter than a RIFF/WAVE header.
    TooShort,
    /// Not a RIFF/WAVE file at all.
    NotWave,
    /// The backend recorded a geometry the vertical did not ask for.
    WrongFormat {
        /// Channels the header declares.
        channels: u16,
        /// Rate the header declares.
        rate: u32,
        /// Bits per sample the header declares.
        bits: u16,
    },
    /// The file carries fewer frames than the guest played.
    Short {
        /// Non-silent frames found.
        frames: usize,
    },
    /// A frame differs from the one the guest played.
    Differs {
        /// The first differing frame, counted from the signal's start.
        frame: usize,
    },
}

/// Check a host-captured WAV holds exactly the signal the guest played.
///
/// The backend opens its file before the guest starts the device and closes
/// it after the device stops, so it brackets the recording with true silence.
/// That lead-in and tail are the *host* backend's, not the guest's, so they
/// are trimmed before the comparison; everything between them must be the
/// signal byte for byte.
///
/// # Errors
///
/// A [`WavMismatch`] naming what was wrong, so a failure says which frame
/// diverged rather than only that one did.
pub fn payload_matches_signal(wav: &[u8]) -> Result<(), WavMismatch> {
    let (channels, rate, bits, payload) = parse_wav(wav)?;
    if usize::from(channels) != CHANNELS || rate != RATE_HZ || bits != 16 {
        return Err(WavMismatch::WrongFormat {
            channels,
            rate,
            bits,
        });
    }
    let frames = payload.len() / FRAME_BYTES;
    let first = (0..frames)
        .find(|frame| !frame_is_silent(payload, *frame))
        .ok_or(WavMismatch::Short { frames: 0 })?;
    let last = (0..frames)
        .rev()
        .find(|frame| !frame_is_silent(payload, *frame))
        .unwrap_or(first);
    let recorded = last - first + 1;
    if recorded < SIGNAL_FRAMES {
        return Err(WavMismatch::Short { frames: recorded });
    }
    for frame in 0..recorded {
        for channel in 0..CHANNELS {
            let at = ((first + frame) * CHANNELS + channel) * 2;
            let got = i16::from_le_bytes([payload[at], payload[at + 1]]);
            // Past the signal the guest played nothing, so the device must
            // have received nothing either: a longer run means the stack
            // manufactured frames.
            let want = if frame < SIGNAL_FRAMES {
                sample(frame, channel)
            } else {
                SILENCE
            };
            if got != want {
                return Err(WavMismatch::Differs { frame });
            }
        }
    }
    Ok(())
}

/// Whether every channel of `frame` is silent.
fn frame_is_silent(payload: &[u8], frame: usize) -> bool {
    (0..CHANNELS).all(|channel| {
        let at = (frame * CHANNELS + channel) * 2;
        payload
            .get(at..at + 2)
            .is_some_and(|bytes| i16::from_le_bytes([bytes[0], bytes[1]]) == SILENCE)
    })
}

/// The `(channels, rate, bits, payload)` of a canonical RIFF/WAVE file.
fn parse_wav(wav: &[u8]) -> Result<(u16, u32, u16, &[u8]), WavMismatch> {
    if wav.len() < 44 {
        return Err(WavMismatch::TooShort);
    }
    if &wav[0..4] != b"RIFF" || &wav[8..12] != b"WAVE" {
        return Err(WavMismatch::NotWave);
    }
    let mut at = 12;
    let mut format = None;
    while at + 8 <= wav.len() {
        let id = &wav[at..at + 4];
        let len = u32::from_le_bytes([wav[at + 4], wav[at + 5], wav[at + 6], wav[at + 7]]) as usize;
        let body = at + 8;
        let end = body.saturating_add(len).min(wav.len());
        if id == b"fmt " && len >= 16 {
            format = Some((
                u16::from_le_bytes([wav[body + 2], wav[body + 3]]),
                u32::from_le_bytes([wav[body + 4], wav[body + 5], wav[body + 6], wav[body + 7]]),
                u16::from_le_bytes([wav[body + 14], wav[body + 15]]),
            ));
        }
        if id == b"data" {
            let (channels, rate, bits) = format.ok_or(WavMismatch::NotWave)?;
            return Ok((channels, rate, bits, &wav[body..end]));
        }
        // Chunks are padded to an even length.
        at = body + len + (len & 1);
    }
    Err(WavMismatch::TooShort)
}

#[cfg(test)]
mod tests {
    use super::*;

    extern crate alloc;
    use alloc::vec;
    use alloc::vec::Vec;

    /// A canonical 48 kHz stereo 16-bit WAV around `payload`.
    fn wav(payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&u32::try_from(36 + payload.len()).unwrap_or(0).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&RATE_HZ.to_le_bytes());
        out.extend_from_slice(&(RATE_HZ * 4).to_le_bytes());
        out.extend_from_slice(&4u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&u32::try_from(payload.len()).unwrap_or(0).to_le_bytes());
        out.extend_from_slice(payload);
        out
    }

    fn signal_bytes() -> Vec<u8> {
        let mut bytes = vec![0u8; SIGNAL_FRAMES * FRAME_BYTES];
        assert_eq!(fill_signal(&mut bytes), bytes.len());
        bytes
    }

    #[test]
    fn the_signal_is_accepted_with_or_without_the_backends_own_silence() {
        let bare = wav(&signal_bytes());
        assert_eq!(payload_matches_signal(&bare), Ok(()));
        let mut padded = vec![0u8; 64 * FRAME_BYTES];
        padded.extend_from_slice(&signal_bytes());
        padded.extend(core::iter::repeat_n(0u8, 32 * FRAME_BYTES));
        assert_eq!(payload_matches_signal(&wav(&padded)), Ok(()));
    }

    #[test]
    fn one_altered_frame_is_named_rather_than_tolerated() {
        let mut bytes = signal_bytes();
        let at = 500 * FRAME_BYTES;
        bytes[at] ^= 0x01;
        assert_eq!(
            payload_matches_signal(&wav(&bytes)),
            Err(WavMismatch::Differs { frame: 500 })
        );
    }

    #[test]
    fn a_truncated_or_mis_shaped_capture_is_refused() {
        let short = &signal_bytes()[..1_000 * FRAME_BYTES];
        assert!(matches!(
            payload_matches_signal(&wav(short)),
            Err(WavMismatch::Short { .. })
        ));
        assert_eq!(
            payload_matches_signal(b"not a wav"),
            Err(WavMismatch::TooShort)
        );
        assert_eq!(
            payload_matches_signal(&[0u8; 64]),
            Err(WavMismatch::NotWave)
        );
    }

    #[test]
    fn no_frame_of_the_signal_is_silent_on_both_channels() {
        // The host trim relies on this: a frame the guest played must never
        // look like the backend's own lead-in.
        for frame in 0..SIGNAL_FRAMES {
            assert!(
                (0..CHANNELS).any(|channel| sample(frame, channel) != SILENCE),
                "frame {frame} is silent on every channel"
            );
        }
    }
}
