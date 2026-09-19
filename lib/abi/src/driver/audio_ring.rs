//! The shared-memory PCM ring both audio hops are carried over
//! (`plans/SOUND.md`).
//!
//! One structure serves client→mixer and mixer→driver alike, because it is
//! the same job: one producer appends interleaved frames, one consumer takes
//! them, and the two run concurrently in different address spaces. A playback
//! stream's client is the producer and the mixer the consumer; a capture
//! stream's is the other way round; the mixer→driver hop repeats the pair one
//! layer down.
//!
//! # Positions are monotone frame counters, and that is the whole trick
//!
//! The header carries two free-running `u64` **frame** positions — total
//! frames produced and total frames consumed — and they never wrap: at
//! 192 kHz a `u64` runs for about three million years. Occupancy is their
//! plain difference, the slot index is a mask of the low bits, and the entire
//! class of wrap-around bugs that byte-indexed rings spend their lives
//! defending against does not arise. It also means a position on the wire and
//! a position in the ring are the same number, so "start at frame N" and "we
//! lost frames N..M" are exact arithmetic rather than a guess.
//!
//! # The ordering discipline
//!
//! * the producer writes a frame's bytes, then **releases** its position;
//! * the consumer **acquires** that position before reading those bytes, and
//!   releases its own only once it has finished with them, so the producer
//!   cannot overwrite frames still being read.
//!
//! The two positions sit in separate cache lines. In one line every publish
//! would invalidate the peer's read of the other and the two CPUs would
//! ping-pong the line every period.
//!
//! # Fail closed
//!
//! Both positions live in memory the *peer* can write, so they are untrusted:
//! every operation snapshots them once, refuses a backwards or over-full pair
//! as [`Errno::OutOfRange`], and works from that snapshot — a peer mutating
//! its position mid-operation cannot steer a second read past the bound the
//! first one checked.
//!
//! # The interleaving oracle
//!
//! A total-store-ordered host would pass this file's whole test suite with
//! the orderings above downgraded to `Relaxed`, because on that hardware a
//! release store and a relaxed one are the same instruction. `tests/loom.rs`
//! is what catches that: under `--cfg loom` the two positions become the
//! model checker's own atomics, and a payload the model writes before
//! publishing and reads after acquiring is ordered by *this* code's
//! release/acquire pair and nothing else — so a downgrade is a reported
//! causality violation rather than a defect that surfaces years later on a
//! weakly-ordered machine.
//!
//! The model checker substitutes its own atomic type, which is not eight
//! bytes of shared memory and cannot be carved out of a mapped region, so
//! [`PcmRing::bind`] does not exist in that build and the model constructs a
//! ring over the two counters directly. The "no torn frame" half stays with
//! `tests/audio_ring_spsc.rs`, which drives both sides concurrently over one
//! aliased region.

#[cfg(loom)]
use loom::sync::atomic::{AtomicU64, Ordering};

#[cfg(not(loom))]
use core::sync::atomic::{AtomicU64, Ordering};

use super::audio::{ring_bounds, Frames, SampleFormat, MAX_CHANNELS};
use super::CACHE_LINE_BYTES;
use crate::Errno;

/// Alignment a PCM ring region must have: that of the header's positions.
const INDEX_ALIGN: usize = align_of::<AtomicU64>();

/// Byte length of a PCM ring's control header: the producer and consumer
/// frame positions, each alone in a cache line.
pub const PCM_RING_HEADER_LEN: usize = 2 * CACHE_LINE_BYTES;

/// Index of the producer position among the header's atomic cells.
///
/// Absent under the interleaving model, which hands the positions in rather
/// than carving them out of a mapped header.
#[cfg(not(loom))]
const PRODUCER_CELL: usize = 0;

/// Index of the consumer position among the header's atomic cells.
#[cfg(not(loom))]
const CONSUMER_CELL: usize = CACHE_LINE_BYTES / INDEX_ALIGN;

/// Extra bytes an in-process buffer needs so an aligned region can be cut
/// from it (see [`aligned_region`]).
pub const REGION_ALIGN_PADDING: usize = INDEX_ALIGN - 1;

/// Cut an aligned `len`-byte region out of `buffer`, or [`None`] when
/// `buffer` is too short.
///
/// [`PcmRing::bind`] requires an aligned region because the header's
/// positions are atomics. An in-process buffer over-allocated by
/// [`REGION_ALIGN_PADDING`] is trimmed to one here.
#[must_use]
pub fn aligned_region(buffer: &mut [u8], len: usize) -> Option<&mut [u8]> {
    super::aligned_region(buffer, len, INDEX_ALIGN)
}

/// Validated shape of one PCM ring: how many frames it holds, and what one
/// frame is.
///
/// The frame count is **derived**, never hand-picked: the mixer computes it
/// from the device's own reported period bounds and the latency the client
/// asked for, and this type only checks the result lands inside the fixed
/// containment bounds that keep a hostile geometry from reserving unbounded
/// pinned memory.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct PcmGeometry {
    frames: u32,
    format: SampleFormat,
    channels: u8,
}

impl PcmGeometry {
    /// Validate and build a geometry.
    ///
    /// A frame count must be a power of two: the slot index is then a mask of
    /// the monotone position rather than a division on the per-period path.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] when `frames` lies outside
    /// [`ring_bounds::MIN_FRAMES`]`..=`[`ring_bounds::MAX_FRAMES`] or is not
    /// a power of two, or `channels` lies outside `1..=`[`MAX_CHANNELS`].
    pub const fn new(frames: u32, format: SampleFormat, channels: u8) -> Result<Self, Errno> {
        if frames < ring_bounds::MIN_FRAMES
            || frames > ring_bounds::MAX_FRAMES
            || !frames.is_power_of_two()
        {
            return Err(Errno::OutOfRange);
        }
        if channels == 0 || channels as usize > MAX_CHANNELS {
            return Err(Errno::OutOfRange);
        }
        Ok(Self {
            frames,
            format,
            channels,
        })
    }

    /// Frames the ring holds.
    #[must_use]
    pub const fn frames(&self) -> u32 {
        self.frames
    }

    /// Sample encoding of the ring's frames.
    #[must_use]
    pub const fn format(&self) -> SampleFormat {
        self.format
    }

    /// Interleaved channels per frame.
    #[must_use]
    pub const fn channels(&self) -> u8 {
        self.channels
    }

    /// Bytes one frame occupies: one sample per channel.
    #[must_use]
    pub const fn frame_bytes(&self) -> usize {
        self.format.bytes_per_sample() * self.channels as usize
    }

    /// Bytes the sample area occupies.
    #[must_use]
    pub const fn samples_len(&self) -> usize {
        self.frames as usize * self.frame_bytes()
    }

    /// Bytes the whole region occupies: the header then the sample area.
    #[must_use]
    pub const fn region_len(&self) -> usize {
        PCM_RING_HEADER_LEN + self.samples_len()
    }
}

/// One stream's bounded single-producer, single-consumer PCM queue inside a
/// shared region.
#[derive(Debug)]
pub struct PcmRing<'a> {
    /// Total frames the producer has published.
    producer: &'a AtomicU64,
    /// Total frames the consumer has released.
    consumer: &'a AtomicU64,
    /// The sample area — the region past the header, so a frame offset needs
    /// no header bias.
    samples: &'a mut [u8],
    geometry: PcmGeometry,
    /// `frames - 1`, the mask that turns a monotone position into a slot.
    mask: u64,
}

impl<'a> PcmRing<'a> {
    /// Bind a ring view over `region` with `geometry`.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `region` is not exactly
    ///   [`PcmGeometry::region_len`] bytes.
    /// * [`Errno::BadAlignment`] — `region` is not aligned for the header's
    ///   atomic positions. Use [`aligned_region`] to cut an aligned view from
    ///   a plain in-process buffer.
    #[cfg(not(loom))]
    pub fn bind(region: &'a mut [u8], geometry: PcmGeometry) -> Result<Self, Errno> {
        if region.len() != geometry.region_len() {
            return Err(Errno::BufferTooSmall);
        }
        let (header, samples) = region.split_at_mut(PCM_RING_HEADER_LEN);
        // SAFETY: reinterpreting initialised `u8`s as `AtomicU64`s is the
        // transmute `align_to_mut` documents, and it is valid here:
        // `AtomicU64` has `u64`'s layout and no invalid bit pattern, so every
        // 8-byte group of the header is a legal value. The split is computed
        // rather than assumed, so nothing is taken on trust about the
        // region's alignment — a misaligned base yields a non-empty prefix,
        // rejected below. Atomics rather than plain reads are precisely what
        // a peer process concurrently accessing these bytes requires.
        //
        // The *mut* form matters: these cells are stored to. Casting through
        // a shared `&[u8]` first would derive them from a read-only tag,
        // making every publication a write the borrow never granted.
        let (prefix, cells, _) = unsafe { header.align_to_mut::<AtomicU64>() };
        if !prefix.is_empty() {
            return Err(Errno::BadAlignment);
        }
        // Shared from here, for the region's whole lifetime: the atomics'
        // interior mutability is what the peer's concurrent access needs, and
        // an exclusive borrow would claim a solitude that does not hold
        // across an address space.
        let cells: &'a [AtomicU64] = cells;
        let (Some(producer), Some(consumer)) = (cells.get(PRODUCER_CELL), cells.get(CONSUMER_CELL))
        else {
            return Err(Errno::BadAlignment);
        };
        Ok(Self {
            producer,
            consumer,
            samples,
            geometry,
            mask: u64::from(geometry.frames() - 1),
        })
    }

    /// Bind a ring view over positions the caller already holds.
    ///
    /// Exists only for the interleaving model, which cannot reach the two
    /// positions the way a mapped region does: the model checker substitutes
    /// its own atomic type, so the header's bytes are not eight-byte cells to
    /// be carved out of. The model hands the counters in directly and each
    /// side brings its own sample area, so what the model covers is the
    /// counter pair and the ordering edges between them — which is where the
    /// release/acquire discipline lives.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] when `samples` is not exactly
    /// [`PcmGeometry::samples_len`] bytes.
    #[cfg(loom)]
    pub fn over_counters(
        producer: &'a AtomicU64,
        consumer: &'a AtomicU64,
        samples: &'a mut [u8],
        geometry: PcmGeometry,
    ) -> Result<Self, Errno> {
        if samples.len() != geometry.samples_len() {
            return Err(Errno::BufferTooSmall);
        }
        Ok(Self {
            producer,
            consumer,
            samples,
            geometry,
            mask: u64::from(geometry.frames() - 1),
        })
    }

    /// The ring's shape.
    #[must_use]
    pub const fn geometry(&self) -> PcmGeometry {
        self.geometry
    }

    /// Snapshot both positions and validate them once, so the rest of an
    /// operation works from values a mutating peer can no longer change under
    /// it.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] when the consumer has run past the producer or
    /// the occupancy exceeds the ring — a position the peer corrupted,
    /// refused rather than acted on.
    fn snapshot(&self) -> Result<(u64, u64), Errno> {
        // Acquire on the peer's position: it orders this side's reads of the
        // frames the peer released with that position.
        let producer = self.producer.load(Ordering::Acquire);
        let consumer = self.consumer.load(Ordering::Acquire);
        let occupancy = producer.checked_sub(consumer).ok_or(Errno::OutOfRange)?;
        if occupancy > u64::from(self.geometry.frames()) {
            return Err(Errno::OutOfRange);
        }
        Ok((producer, consumer))
    }

    /// Total frames the producer has published.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] when the peer's positions are corrupt.
    pub fn producer_position(&self) -> Result<Frames, Errno> {
        Ok(Frames::new(self.snapshot()?.0))
    }

    /// Total frames the consumer has taken.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] when the peer's positions are corrupt.
    pub fn consumer_position(&self) -> Result<Frames, Errno> {
        Ok(Frames::new(self.snapshot()?.1))
    }

    /// Frames queued and not yet taken.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] when the peer's positions are corrupt.
    pub fn readable_frames(&self) -> Result<u32, Errno> {
        let (producer, consumer) = self.snapshot()?;
        Ok(self.queued(producer, consumer))
    }

    /// Frames the producer may publish before the ring is full.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] when the peer's positions are corrupt.
    pub fn writable_frames(&self) -> Result<u32, Errno> {
        let (producer, consumer) = self.snapshot()?;
        Ok(self.geometry.frames() - self.queued(producer, consumer))
    }

    /// Frames queued in a validated snapshot. The occupancy was checked
    /// against the frame count when the snapshot was taken, so it fits.
    fn queued(&self, producer: u64, consumer: u64) -> u32 {
        u32::try_from(producer - consumer).unwrap_or(self.geometry.frames())
    }

    /// Byte offset of `position`'s frame within the sample area.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] if the masked index could not be expressed as a
    /// byte offset on this target.
    fn frame_offset(&self, position: u64) -> Result<usize, Errno> {
        let index = usize::try_from(position & self.mask).map_err(|_| Errno::OutOfRange)?;
        index
            .checked_mul(self.geometry.frame_bytes())
            .ok_or(Errno::OutOfRange)
    }

    /// Copy `data` into the ring starting at `position`, wrapping the sample
    /// area at most once.
    fn copy_in(&mut self, position: u64, data: &[u8]) -> Result<(), Errno> {
        // The wrap is a single split, so a run longer than the sample area
        // would overrun its second leg. Checked rather than argued from the
        // caller, so no reasoning outside this function keeps it sound.
        if data.len() > self.samples.len() {
            return Err(Errno::OutOfRange);
        }
        let start = self.frame_offset(position)?;
        let head = (self.samples.len() - start).min(data.len());
        self.samples[start..start + head].copy_from_slice(&data[..head]);
        let tail = data.len() - head;
        self.samples[..tail].copy_from_slice(&data[head..]);
        Ok(())
    }

    /// Copy `out.len()` bytes out of the ring starting at `position`, wrapping
    /// the sample area at most once.
    fn copy_out(&self, position: u64, out: &mut [u8]) -> Result<(), Errno> {
        if out.len() > self.samples.len() {
            return Err(Errno::OutOfRange);
        }
        let start = self.frame_offset(position)?;
        let head = (self.samples.len() - start).min(out.len());
        out[..head].copy_from_slice(&self.samples[start..start + head]);
        let tail = out.len() - head;
        out[head..].copy_from_slice(&self.samples[..tail]);
        Ok(())
    }

    /// Fill `bytes` of the ring from `position` with `value`, wrapping the
    /// sample area at most once.
    fn fill(&mut self, position: u64, bytes: usize, value: u8) -> Result<(), Errno> {
        if bytes > self.samples.len() {
            return Err(Errno::OutOfRange);
        }
        let start = self.frame_offset(position)?;
        let head = (self.samples.len() - start).min(bytes);
        self.samples[start..start + head].fill(value);
        self.samples[..bytes - head].fill(value);
        Ok(())
    }

    /// The position `frames` past `producer`, refusing an overflow.
    ///
    /// A real stream cannot reach the end of a `u64` frame counter, but the
    /// position lives in memory the peer can write, so the arithmetic is
    /// checked rather than assumed. Checked *before* any byte is copied, so a
    /// refusal leaves the ring untouched.
    fn next_producer(producer: u64, frames: u32) -> Result<u64, Errno> {
        producer
            .checked_add(u64::from(frames))
            .ok_or(Errno::OutOfRange)
    }

    /// Append whole frames from `samples`, returning how many were taken.
    ///
    /// A short return means the ring filled: the caller writes the rest once
    /// the consumer has made room, and parks on its space-available notify
    /// rather than retrying. The producer's own view of free space can only
    /// grow underneath it — the consumer never un-consumes — so a short write
    /// is never a lost race.
    ///
    /// # Errors
    ///
    /// * [`Errno::LengthOutOfRange`] — `samples` is not a whole number of
    ///   frames. A partial frame would silently rotate every later channel.
    /// * [`Errno::OutOfRange`] — the peer's positions are corrupt.
    pub fn write(&mut self, samples: &[u8]) -> Result<u32, Errno> {
        let frame_bytes = self.geometry.frame_bytes();
        if !samples.len().is_multiple_of(frame_bytes) {
            return Err(Errno::LengthOutOfRange);
        }
        let (producer, consumer) = self.snapshot()?;
        let offered = u32::try_from(samples.len() / frame_bytes).unwrap_or(u32::MAX);
        let taken = offered.min(self.geometry.frames() - self.queued(producer, consumer));
        if taken == 0 {
            return Ok(0);
        }
        let next = Self::next_producer(producer, taken)?;
        let bytes = taken as usize * frame_bytes;
        self.copy_in(producer, &samples[..bytes])?;
        // Release last: the peer must not observe the position before the
        // frames it points at.
        self.producer.store(next, Ordering::Release);
        Ok(taken)
    }

    /// Append `frames` frames of silence, returning how many were taken.
    ///
    /// The gap-filling half of writing at an exact position: a client whose
    /// next frame belongs later than the ring's producer position closes the
    /// distance with silence, so the position never lies about where the
    /// samples that follow it belong.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] — the peer's positions are corrupt.
    pub fn write_silence(&mut self, frames: u32) -> Result<u32, Errno> {
        let (producer, consumer) = self.snapshot()?;
        let taken = frames.min(self.geometry.frames() - self.queued(producer, consumer));
        if taken == 0 {
            return Ok(0);
        }
        let next = Self::next_producer(producer, taken)?;
        let bytes = taken as usize * self.geometry.frame_bytes();
        self.fill(producer, bytes, self.geometry.format().silence_byte())?;
        self.producer.store(next, Ordering::Release);
        Ok(taken)
    }

    /// Take as many queued frames as `out` holds, returning how many were
    /// written into it. Bytes of `out` past the returned frames are untouched.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] — the peer's positions are corrupt.
    pub fn read(&mut self, out: &mut [u8]) -> Result<u32, Errno> {
        let frame_bytes = self.geometry.frame_bytes();
        let (producer, consumer) = self.snapshot()?;
        let wanted = u32::try_from(out.len() / frame_bytes).unwrap_or(u32::MAX);
        let taken = wanted.min(self.queued(producer, consumer));
        if taken == 0 {
            return Ok(0);
        }
        let bytes = taken as usize * frame_bytes;
        self.copy_out(consumer, &mut out[..bytes])?;
        // Release only now: the frames are the producer's to overwrite once
        // their bytes have been copied out, not before.
        self.consumer
            .store(consumer + u64::from(taken), Ordering::Release);
        Ok(taken)
    }

    /// Drop up to `frames` queued frames without copying them, returning how
    /// many were dropped.
    ///
    /// What a flush is made of: the frames are finished with, and the position
    /// advances over them so the stream's arithmetic still describes where the
    /// samples that follow belong.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] — the peer's positions are corrupt.
    pub fn discard(&mut self, frames: u32) -> Result<u32, Errno> {
        let (producer, consumer) = self.snapshot()?;
        let dropped = frames.min(self.queued(producer, consumer));
        if dropped == 0 {
            return Ok(0);
        }
        self.consumer
            .store(consumer + u64::from(dropped), Ordering::Release);
        Ok(dropped)
    }
}

#[cfg(test)]
#[path = "audio_ring_tests.rs"]
mod tests;
