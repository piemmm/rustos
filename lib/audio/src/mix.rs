//! The mixer: every live stream on one sink summed into one period of device
//! frames.
//!
//! # The bit-exactness property, stated precisely
//!
//! A source of twenty-four bits or fewer, at unity gain, through an identity
//! channel map, at the sink's own rate, with no other stream live, reaches the
//! device **bit-exact**. Three things make that true rather than approximately
//! true:
//!
//! * the `f32` pivot's scale factors are powers of two, so a twenty-four-bit
//!   integer survives the round trip unchanged;
//! * unity gain is exactly `1.0` and the identity channel map is a copy, so
//!   neither stage touches a sample;
//! * the **first** contributor is *assigned* into the accumulator rather than
//!   added to a zeroed one, because `0.0 + -0.0` is `+0.0` and a float source
//!   that wrote a negative zero is entitled to read it back.
//!
//! A thirty-two-bit integer source carries twenty-four bits of mantissa
//! through the mix. That is documented rather than rescued: a wider
//! accumulator would cost every path to save a case no consumer format
//! produces and nobody can hear.
//!
//! # Nothing is allocated per period
//!
//! Every working buffer is sized when the sink is configured and reused, so
//! the per-period path allocates nothing, locks nothing, and does work bounded
//! by (live streams × period frames). Glitchless output is then a property of
//! the arrangement rather than of the machine's mood.

use alloc::vec::Vec;

use tairix_abi::driver::audio::{ChannelMap, Rate, SampleFormat, MAX_CHANNELS};
use tairix_abi::Errno;
use tairix_util::fallible;

use crate::channel::ChannelMatrix;
use crate::convert::{self, Dither, DitherSource};

/// Bits of resolution the `f32` pivot carries: the mantissa's width.
///
/// What a summed, gained, or resampled stream's samples are worth, whatever
/// the encoding they arrived in.
pub const PIVOT_BITS: u32 = 24;

/// What one sink is configured as.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SinkFormat {
    /// The encoding the device is clocked in.
    pub format: SampleFormat,
    /// The rate it is clocked at. Every stream reaching the mixer is already
    /// at this rate; resampling is a per-stream stage above.
    pub rate: Rate,
    /// Its channel layout.
    pub channel_map: ChannelMap,
}

impl SinkFormat {
    /// Interleaved channels one device frame carries.
    #[must_use]
    pub fn channels(&self) -> usize {
        usize::from(self.channel_map.channels())
    }

    /// Bytes one device frame occupies.
    #[must_use]
    pub fn frame_bytes(&self) -> usize {
        self.channels() * self.format.bytes_per_sample()
    }
}

/// One stream's contribution to a period.
#[derive(Copy, Clone, Debug)]
pub struct StreamMix<'a> {
    /// The encoding `samples` is in.
    pub format: SampleFormat,
    /// The map carrying this stream's layout onto the sink's, derived once
    /// when the stream opened.
    pub matrix: &'a ChannelMatrix,
    /// The one multiply, already resolved from every gain the stream is
    /// subject to. Exactly `1.0` leaves the samples alone.
    pub gain: f32,
    /// Whether these samples came through the resampler, and so carry the
    /// pivot's resolution rather than their encoding's.
    pub resampled: bool,
    /// Interleaved frames in this stream's own layout, at the sink's rate. A
    /// short block is the frames the stream had; the rest of the period is
    /// silence from its point of view.
    pub samples: &'a [u8],
}

impl StreamMix<'_> {
    /// Whole frames the block carries.
    fn frames(&self) -> usize {
        let per_frame = self.matrix.sources() * self.format.bytes_per_sample();
        if per_frame == 0 {
            return 0;
        }
        self.samples.len() / per_frame
    }

    /// The resolution these samples are worth.
    fn resolution(&self) -> u32 {
        if self.resampled {
            PIVOT_BITS
        } else {
            self.format.valid_bits()
        }
    }
}

/// One sink's mixer: the working buffers and the one quantisation to the
/// device's encoding.
#[derive(Debug)]
pub struct Mixer {
    sink: SinkFormat,
    period_frames: usize,
    dither: Dither,
    noise: DitherSource,
    /// Decode scratch, sized for the widest layout a stream may present.
    source: Vec<f32>,
    /// Channel-mapped, gained scratch in the sink's layout.
    mapped: Vec<f32>,
    /// The sum.
    accumulator: Vec<f32>,
}

impl Mixer {
    /// A mixer for `sink` producing at most `period_frames` frames per call.
    ///
    /// Every buffer is allocated here and reused, so no later call allocates.
    ///
    /// # Errors
    ///
    /// * [`Errno::OutOfRange`] — a zero period, or one whose buffers would not
    ///   be addressable.
    /// * [`Errno::OutOfMemory`] — a buffer could not be allocated.
    pub fn new(sink: SinkFormat, period_frames: usize, dither_seed: u64) -> Result<Self, Errno> {
        if period_frames == 0 {
            return Err(Errno::OutOfRange);
        }
        let widest = period_frames
            .checked_mul(MAX_CHANNELS)
            .ok_or(Errno::OutOfRange)?;
        let sink_samples = period_frames
            .checked_mul(sink.channels())
            .ok_or(Errno::OutOfRange)?;
        Ok(Self {
            sink,
            period_frames,
            dither: Dither::TriangularPdf,
            noise: DitherSource::new(dither_seed),
            source: fallible::filled(widest, 0.0f32).ok_or(Errno::OutOfMemory)?,
            mapped: fallible::filled(sink_samples, 0.0f32).ok_or(Errno::OutOfMemory)?,
            accumulator: fallible::filled(sink_samples, 0.0f32).ok_or(Errno::OutOfMemory)?,
        })
    }

    /// Whether the quantisation to the device's encoding is dithered where it
    /// narrows.
    ///
    /// Switchable because a measurement path wants the quantiser bare; the
    /// narrowing test still gates it, so turning it on can never add noise to
    /// a path that was exact.
    pub fn set_dither(&mut self, dither: Dither) {
        self.dither = dither;
    }

    /// Sum `streams` into `frames` device frames written to `out`, returning
    /// the bytes written.
    ///
    /// A period no stream contributed to is the device's own silence, not
    /// zeroed bytes: an unsigned encoding's quiet value is mid-scale, and
    /// zeroes there would be a click at full negative deflection.
    ///
    /// The contributions arrive as an *iterator*, walked exactly once, so a
    /// service whose live streams live in its own records folds them in
    /// without building a per-period collection — the one place an
    /// otherwise allocation-free period path would have had to allocate.
    ///
    /// # Errors
    ///
    /// * [`Errno::OutOfRange`] — `frames` is past the configured period.
    /// * [`Errno::BufferTooSmall`] — `out` cannot hold `frames` device frames.
    /// * [`Errno::NotSupported`] — a stream's matrix does not answer the
    ///   sink's channel count, which means it was derived against a different
    ///   sink.
    pub fn mix<'a>(
        &mut self,
        streams: impl IntoIterator<Item = StreamMix<'a>>,
        frames: usize,
        out: &mut [u8],
    ) -> Result<usize, Errno> {
        if frames > self.period_frames {
            return Err(Errno::OutOfRange);
        }
        let bytes = frames * self.sink.frame_bytes();
        if out.len() < bytes {
            return Err(Errno::BufferTooSmall);
        }
        let channels = self.sink.channels();
        let span = frames * channels;
        let mut contributors = 0usize;
        let mut resolution = 0u32;
        for stream in streams {
            let stream = &stream;
            if stream.matrix.destinations() != channels {
                return Err(Errno::NotSupported);
            }
            let covered = stream.frames().min(frames);
            if covered == 0 {
                continue;
            }
            let decoded = convert::decode(
                stream.format,
                stream.samples,
                &mut self.source[..covered * stream.matrix.sources()],
            );
            let covered = decoded / stream.matrix.sources().max(1);
            if covered == 0 {
                continue;
            }
            stream.matrix.map(
                covered,
                stream.gain,
                &self.source[..covered * stream.matrix.sources()],
                &mut self.mapped[..covered * channels],
            )?;
            let reached = covered * channels;
            if contributors == 0 {
                // Assigned, not added: a zeroed accumulator would turn a
                // float source's negative zero into a positive one.
                self.accumulator[..reached].copy_from_slice(&self.mapped[..reached]);
                self.accumulator[reached..span].fill(0.0);
            } else {
                for (sum, sample) in self.accumulator[..reached]
                    .iter_mut()
                    .zip(&self.mapped[..reached])
                {
                    *sum += *sample;
                }
            }
            contributors += 1;
            resolution = resolution.max(stream.resolution());
        }
        if contributors == 0 {
            convert::silence(self.sink.format, &mut out[..bytes]);
            return Ok(bytes);
        }
        if contributors > 1 {
            // A sum carries content below every contributor's own step, so
            // the material's resolution is the pivot's from here on.
            resolution = resolution.max(PIVOT_BITS);
        }
        if self.dither == Dither::TriangularPdf && convert::narrows(resolution, self.sink.format) {
            self.noise
                .apply(self.sink.format, &mut self.accumulator[..span]);
        }
        Ok(convert::encode(
            self.sink.format,
            &self.accumulator[..span],
            &mut out[..bytes],
        ))
    }
}

#[cfg(test)]
#[path = "mix_tests.rs"]
mod tests;
