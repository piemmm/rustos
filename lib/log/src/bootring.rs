//! Bounded per-CPU early-boot log ring buffers.
//!
//! Before `/System/Logs` is writable, the kernel has nowhere durable to put a
//! log record, yet the earliest records — memory sizing, discovery, driver
//! bring-up — are exactly the ones an operator needs when a boot fails. Each
//! CPU therefore owns one [`BootRing`]: a fixed-capacity, allocation-free FIFO
//! of variable-length frames over an inline byte ring, retaining the most
//! recent records until the journal can import them into the `boot` stream.
//!
//! A ring stores the *same logical record body* the persistent path uses
//! ([`crate::record`]) as an opaque blob, plus the two container-owned facts
//! the body does not carry that import must preserve: the per-CPU record
//! sequence (`cpu_seq`) and the monotonic time the record was produced. The
//! producer supplies `cpu_seq` (it owns the per-CPU counter that the encoded
//! body's own `cpu_seq` field already reflects), so the ring never invents a
//! sequence that could disagree with the body; it only requires the values to
//! arrive strictly increasing and rejects anything else fail-closed.
//!
//! When the ring is full, pushing a new record evicts the oldest — a boot ring
//! keeps *recent* history, never blocks the boot path, and never grows without
//! bound. Eviction is not silent: the ring accumulates the contiguous
//! `cpu_seq` range of every record dropped before it could be drained, so the
//! journal can emit one trusted loss record naming the affected CPU and range
//! (`plans/SYSLOG.md` §8.1) rather than leaving an undetectable gap.
//!
//! `N` is the ring's byte capacity, chosen by whoever declares the per-CPU
//! ring: a diagnostic-tail bound, not a figure that should follow the machine.
//! Owning the bytes inline rather than borrowing an arena is what lets one live
//! in a `static` reached before the allocator exists, and makes an arena too
//! small to hold a frame a build error instead of a runtime refusal.
//!
//! The ring is deliberately not internally synchronised, matching
//! [`crate::segment::SegmentWriter`]: a boot ring has a single writer (its own
//! CPU) and is drained once, at import, after that CPU has stopped writing to
//! it. Callers that share one across those phases provide the ordering.

use core::mem::size_of;

use tairix_abi::{Duration64, Errno};
use tairix_inline::RingBuf;

/// Bytes of the frame header holding the body length.
const BODY_LEN_FIELD: usize = size_of::<u32>();

/// Bytes of the frame header holding the per-CPU record sequence.
const CPU_SEQ_FIELD: usize = size_of::<u64>();

/// Bytes of per-frame bookkeeping the ring stores ahead of each record body:
/// a little-endian `u32` body length, a `u64` `cpu_seq`, and a
/// [`Duration64`] monotonic timestamp.
pub const FRAME_HEADER_LEN: usize = BODY_LEN_FIELD + CPU_SEQ_FIELD + Duration64::WIRE_LEN;

/// Largest logical-record body, in bytes, a single boot record may carry.
///
/// A boot record is a short operational line, not a bulk payload; capping the
/// body keeps one frame from monopolising a ring and bounds the scratch buffer
/// a drainer must supply. A body larger than this is rejected fail-closed
/// rather than truncated.
pub const MAX_BOOT_RECORD_BODY: usize = 4096;

/// A record drained from a [`BootRing`].
///
/// The body itself is copied into the caller's scratch buffer by
/// [`BootRing::pop_oldest`]; this carries the two container-owned facts the
/// body does not, so the journal can rebuild the persistent record.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DrainedRecord {
    /// The per-CPU record sequence the producer assigned.
    pub cpu_seq: u64,
    /// The monotonic time the record was produced.
    pub monotonic: Duration64,
    /// Length, in bytes, of the record body written to the scratch buffer.
    pub body_len: usize,
}

/// The contiguous `cpu_seq` range of records a [`BootRing`] evicted before they
/// could be drained.
///
/// The journal turns this into a single trusted loss record so a boot-log
/// consumer sees an explicit "records `first_seq..=last_seq` from CPU `cpu_id`
/// were lost" rather than an unexplained sequence gap
/// (`plans/SYSLOG.md` §8.1).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct LossRange {
    /// The CPU whose records were lost.
    pub cpu_id: u32,
    /// The `cpu_seq` of the first (oldest) lost record.
    pub first_seq: u64,
    /// The `cpu_seq` of the last (newest) lost record.
    pub last_seq: u64,
    /// The number of records lost. Equal to `last_seq - first_seq + 1`.
    pub count: u64,
}

/// One frame's header, as it sits at the front of the byte ring.
struct FrameHeader {
    body_len: usize,
    cpu_seq: u64,
    monotonic: Duration64,
}

impl FrameHeader {
    /// Lay the header out for the ring: body length, sequence, timestamp.
    fn encode(&self) -> Result<[u8; FRAME_HEADER_LEN], Errno> {
        let body_len = u32::try_from(self.body_len).map_err(|_| Errno::LengthOutOfRange)?;
        let mut out = [0u8; FRAME_HEADER_LEN];
        let (len_field, rest) = out.split_at_mut(BODY_LEN_FIELD);
        let (seq_field, time_field) = rest.split_at_mut(CPU_SEQ_FIELD);
        len_field.copy_from_slice(&body_len.to_le_bytes());
        seq_field.copy_from_slice(&self.cpu_seq.to_le_bytes());
        time_field.copy_from_slice(&self.monotonic.to_le_bytes());
        Ok(out)
    }

    /// Read a header back. Every field the ring wrote is representable, so the
    /// only failure is a timestamp the shared decoder rejects.
    fn decode(bytes: &[u8; FRAME_HEADER_LEN]) -> Result<Self, Errno> {
        let (len_field, rest) = bytes.split_at(BODY_LEN_FIELD);
        let (seq_field, time_field) = rest.split_at(CPU_SEQ_FIELD);
        let mut len_bytes = [0u8; BODY_LEN_FIELD];
        len_bytes.copy_from_slice(len_field);
        let mut seq_bytes = [0u8; CPU_SEQ_FIELD];
        seq_bytes.copy_from_slice(seq_field);
        let mut time_bytes = [0u8; Duration64::WIRE_LEN];
        time_bytes.copy_from_slice(time_field);
        Ok(Self {
            body_len: usize::try_from(u32::from_le_bytes(len_bytes))
                .map_err(|_| Errno::LengthOutOfRange)?,
            cpu_seq: u64::from_le_bytes(seq_bytes),
            monotonic: Duration64::from_bytes(&time_bytes)?,
        })
    }
}

/// A bounded, allocation-free FIFO of early-boot log records for one CPU, over
/// `N` bytes of inline storage.
///
/// Frames are written back-to-back and may wrap the physical end of the ring,
/// so no space is wasted padding to the end. The oldest frame is evicted to
/// make room when the ring is full.
pub struct BootRing<const N: usize> {
    bytes: RingBuf<u8, N>,
    cpu_id: u32,
    /// Number of whole frames retained. The byte ring counts bytes; frames are
    /// this layer's unit.
    frames: usize,
    /// The `cpu_seq` of the most recently pushed record, for the
    /// strictly-increasing check. `None` until the first push.
    last_pushed_seq: Option<u64>,
    /// Pending loss accumulated since the last [`Self::take_loss`]. `count`
    /// of zero means no loss is pending; the range is meaningful only then.
    lost_lo: u64,
    lost_hi: u64,
    lost_count: u64,
}

impl<const N: usize> BootRing<N> {
    /// A ring for CPU `cpu_id`.
    ///
    /// `N` must be able to hold at least a zero-body frame; a smaller capacity
    /// could store nothing at all, so it fails the build rather than refusing
    /// every record at runtime.
    #[must_use]
    pub const fn new(cpu_id: u32) -> Self {
        const { assert!(N >= FRAME_HEADER_LEN, "a boot ring must hold one frame") }
        Self {
            bytes: RingBuf::new(),
            cpu_id,
            frames: 0,
            last_pushed_seq: None,
            lost_lo: 0,
            lost_hi: 0,
            lost_count: 0,
        }
    }

    /// The CPU this ring belongs to.
    #[must_use]
    pub const fn cpu_id(&self) -> u32 {
        self.cpu_id
    }

    /// Number of records currently retained.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.frames
    }

    /// Whether the ring currently retains no records.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.frames == 0
    }

    /// The ring's total capacity in bytes.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        N
    }

    /// Append one record produced at monotonic time `monotonic` with per-CPU
    /// sequence `cpu_seq`, evicting the oldest retained records if the ring is
    /// full.
    ///
    /// `cpu_seq` MUST be strictly greater than the previous push's; the ring
    /// uses it only to name a loss range and refuses a non-increasing value so
    /// a bogus sequence cannot fabricate a false range.
    ///
    /// A rejected push changes nothing: no bytes are written, no eviction
    /// happens, and the sequence is not recorded as pushed.
    ///
    /// # Errors
    ///
    /// * [`Errno::OutOfRange`] — `cpu_seq` is not strictly greater than the
    ///   previous push's sequence.
    /// * [`Errno::LengthOutOfRange`] — `body` exceeds [`MAX_BOOT_RECORD_BODY`].
    /// * [`Errno::BufferTooSmall`] — `body` plus its frame header cannot fit
    ///   the whole ring even when empty; such a record can never be stored.
    pub fn push(&mut self, cpu_seq: u64, monotonic: Duration64, body: &[u8]) -> Result<(), Errno> {
        if let Some(prev) = self.last_pushed_seq {
            if cpu_seq <= prev {
                return Err(Errno::OutOfRange);
            }
        }
        if body.len() > MAX_BOOT_RECORD_BODY {
            return Err(Errno::LengthOutOfRange);
        }
        let need = FRAME_HEADER_LEN + body.len();
        if need > N {
            return Err(Errno::BufferTooSmall);
        }
        let header = FrameHeader {
            body_len: body.len(),
            cpu_seq,
            monotonic,
        }
        .encode()?;

        // `need <= N`, and each eviction frees a whole frame, so an empty ring
        // is always reachable. The guard makes that structural rather than
        // trusted: a ring that somehow could not evict breaks out and the
        // push below fails closed instead of spinning.
        while self.bytes.remaining_capacity() < need && self.evict_oldest() {}

        self.bytes
            .try_push_slice(&header)
            .and_then(|()| self.bytes.try_push_slice(body))
            .map_err(|_| Errno::BufferTooSmall)?;
        self.frames += 1;
        self.last_pushed_seq = Some(cpu_seq);
        Ok(())
    }

    /// Remove and return the oldest retained record, copying its body into
    /// `scratch`.
    ///
    /// Returns `Ok(None)` when the ring is empty. On success the body occupies
    /// `scratch[..returned.body_len]`.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] if `scratch` is shorter than the oldest
    /// record's body; the record is left in place so the caller can retry with
    /// a larger buffer.
    pub fn pop_oldest(&mut self, scratch: &mut [u8]) -> Result<Option<DrainedRecord>, Errno> {
        let Some(header) = self.peek_header()? else {
            return Ok(None);
        };
        if header.body_len > scratch.len() {
            return Err(Errno::BufferTooSmall);
        }
        self.bytes.discard_front(FRAME_HEADER_LEN);
        // A frame is pushed whole and removed whole, so the declared body is
        // queued behind its header; the count actually copied is reported so a
        // caller can never read past what it was given.
        let body_len = self.bytes.pop_slice(&mut scratch[..header.body_len]);
        self.frames -= 1;
        Ok(Some(DrainedRecord {
            cpu_seq: header.cpu_seq,
            monotonic: header.monotonic,
            body_len,
        }))
    }

    /// Take the pending loss range accumulated since the last call, clearing
    /// it.
    ///
    /// Returns `None` when no record has been evicted before being drained.
    /// The journal calls this before draining so it can emit the trusted loss
    /// record ahead of the surviving records (`plans/SYSLOG.md` §8.1).
    #[must_use]
    pub fn take_loss(&mut self) -> Option<LossRange> {
        if self.lost_count == 0 {
            return None;
        }
        let range = LossRange {
            cpu_id: self.cpu_id,
            first_seq: self.lost_lo,
            last_seq: self.lost_hi,
            count: self.lost_count,
        };
        self.lost_count = 0;
        Some(range)
    }

    /// Read the oldest frame's header without consuming it, or `None` when no
    /// frame is retained.
    fn peek_header(&self) -> Result<Option<FrameHeader>, Errno> {
        if self.frames == 0 {
            return Ok(None);
        }
        let mut bytes = [0u8; FRAME_HEADER_LEN];
        if self.bytes.peek_slice(0, &mut bytes) != FRAME_HEADER_LEN {
            return Err(Errno::BufferTooSmall);
        }
        FrameHeader::decode(&bytes).map(Some)
    }

    /// Drop the oldest frame, folding its sequence into the pending loss range.
    /// Reports whether a frame was dropped, so the caller's eviction loop
    /// always makes progress.
    fn evict_oldest(&mut self) -> bool {
        let Ok(Some(header)) = self.peek_header() else {
            return false;
        };
        if self.lost_count == 0 {
            self.lost_lo = header.cpu_seq;
        }
        self.lost_hi = header.cpu_seq;
        self.lost_count += 1;

        self.bytes.discard_front(FRAME_HEADER_LEN + header.body_len);
        self.frames -= 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::{BootRing, Duration64, Errno, FRAME_HEADER_LEN, MAX_BOOT_RECORD_BODY};

    fn mono(secs: u64) -> Duration64 {
        Duration64::from_secs(i64::try_from(secs).expect("test seconds fit i64"))
    }

    /// A capacity too small for one frame is a build error, not a runtime
    /// refusal, so there is nothing left to test at that end: the smallest
    /// legal ring holds exactly a zero-body frame.
    #[test]
    fn the_smallest_legal_ring_holds_one_empty_record() {
        let mut ring: BootRing<FRAME_HEADER_LEN> = BootRing::new(0);
        assert_eq!(ring.capacity(), FRAME_HEADER_LEN);
        ring.push(0, mono(1), b"").expect("a zero-body frame fits");
        assert_eq!(ring.len(), 1);
        // One byte of body could never fit, so it is refused rather than
        // evicting the record already there.
        assert_eq!(
            ring.push(1, mono(2), b"x").err(),
            Some(Errno::BufferTooSmall)
        );
        assert_eq!(ring.len(), 1);
    }

    #[test]
    fn push_then_drain_preserves_order_seq_time_and_body() {
        let mut ring: BootRing<1024> = BootRing::new(3);
        ring.push(0, mono(1), b"first").expect("fits");
        ring.push(1, mono(2), b"second").expect("fits");
        ring.push(2, mono(3), b"third").expect("fits");
        assert_eq!(ring.len(), 3);
        assert_eq!(ring.cpu_id(), 3);
        assert!(ring.take_loss().is_none(), "nothing evicted");

        let mut scratch = [0u8; MAX_BOOT_RECORD_BODY];
        for (seq, secs, body) in [
            (0u64, 1u64, &b"first"[..]),
            (1, 2, b"second"),
            (2, 3, b"third"),
        ] {
            let rec = ring
                .pop_oldest(&mut scratch)
                .expect("no error")
                .expect("a record");
            assert_eq!(rec.cpu_seq, seq);
            assert_eq!(rec.monotonic, mono(secs));
            assert_eq!(&scratch[..rec.body_len], body);
        }
        assert!(ring.pop_oldest(&mut scratch).expect("no error").is_none());
        assert!(ring.is_empty());
    }

    #[test]
    fn full_ring_evicts_oldest_and_reports_a_loss_range() {
        // Sized to hold exactly two 4-byte-body frames.
        let mut ring: BootRing<{ 2 * (FRAME_HEADER_LEN + 4) }> = BootRing::new(7);
        for seq in 0..5u64 {
            ring.push(seq, mono(seq), b"beef").expect("fits");
        }
        // Only the two newest survive; seq 0..=2 were evicted.
        assert_eq!(ring.len(), 2);
        let loss = ring.take_loss().expect("loss pending");
        assert_eq!(loss.cpu_id, 7);
        assert_eq!(loss.first_seq, 0);
        assert_eq!(loss.last_seq, 2);
        assert_eq!(loss.count, 3);
        assert!(ring.take_loss().is_none(), "loss cleared after taking");

        let mut scratch = [0u8; 16];
        let a = ring.pop_oldest(&mut scratch).expect("ok").expect("rec");
        assert_eq!(a.cpu_seq, 3);
        let b = ring.pop_oldest(&mut scratch).expect("ok").expect("rec");
        assert_eq!(b.cpu_seq, 4);
    }

    /// A frame that straddles the physical end of the byte ring must round-trip
    /// intact. The capacity is not a multiple of the frame size, so successive
    /// frames land at every possible offset relative to the wrap.
    #[test]
    fn wrapping_body_round_trips() {
        let mut ring: BootRing<{ FRAME_HEADER_LEN * 4 + 7 }> = BootRing::new(1);
        let mut scratch = [0u8; MAX_BOOT_RECORD_BODY];
        let push = |ring: &mut BootRing<{ FRAME_HEADER_LEN * 4 + 7 }>, seq: u64| {
            let payload = [u8::try_from(seq & 0xff).expect("masked"); 9];
            ring.push(seq, mono(seq), &payload).expect("fits");
        };
        // Prime two resident frames, then push one / pop one each round: the
        // two frames stay resident (never enough to evict) while head and tail
        // keep advancing and wrapping the physical end of the buffer.
        let mut next_seq = 0u64;
        push(&mut ring, next_seq);
        next_seq += 1;
        push(&mut ring, next_seq);
        next_seq += 1;
        for expect_seq in 0..50u64 {
            push(&mut ring, next_seq);
            next_seq += 1;
            let rec = ring.pop_oldest(&mut scratch).expect("ok").expect("rec");
            assert_eq!(rec.cpu_seq, expect_seq);
            assert_eq!(rec.monotonic, mono(expect_seq));
            assert_eq!(
                &scratch[..rec.body_len],
                &[u8::try_from(expect_seq & 0xff).expect("masked"); 9]
            );
        }
        assert!(
            ring.take_loss().is_none(),
            "two resident frames never evict"
        );
    }

    #[test]
    fn push_rejects_non_increasing_sequence_without_side_effects() {
        let mut ring: BootRing<512> = BootRing::new(0);
        ring.push(5, mono(1), b"x").expect("fits");
        assert_eq!(ring.push(5, mono(2), b"y").err(), Some(Errno::OutOfRange));
        assert_eq!(ring.push(4, mono(2), b"y").err(), Some(Errno::OutOfRange));
        assert_eq!(ring.len(), 1, "rejected pushes left the ring untouched");
        ring.push(6, mono(2), b"z").expect("increasing accepted");
        assert_eq!(ring.len(), 2);
    }

    #[test]
    fn push_rejects_a_body_larger_than_the_cap() {
        let mut ring: BootRing<8192> = BootRing::new(0);
        let big = [0u8; MAX_BOOT_RECORD_BODY + 1];
        assert_eq!(
            ring.push(0, mono(1), &big).err(),
            Some(Errno::LengthOutOfRange)
        );
        assert!(ring.is_empty());
    }

    #[test]
    fn push_rejects_a_record_that_can_never_fit_the_ring() {
        let mut ring: BootRing<{ FRAME_HEADER_LEN + 4 }> = BootRing::new(0);
        // Body of 5 needs FRAME_HEADER_LEN + 5, one more than the ring holds.
        assert_eq!(
            ring.push(0, mono(1), b"12345").err(),
            Some(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn pop_into_too_small_scratch_leaves_the_record() {
        let mut ring: BootRing<512> = BootRing::new(0);
        ring.push(0, mono(1), b"abcdef").expect("fits");
        let mut tiny = [0u8; 3];
        assert_eq!(
            ring.pop_oldest(&mut tiny).err(),
            Some(Errno::BufferTooSmall)
        );
        // The record survived; a larger buffer drains it.
        let mut ok = [0u8; 16];
        let rec = ring.pop_oldest(&mut ok).expect("ok").expect("rec");
        assert_eq!(&ok[..rec.body_len], b"abcdef");
    }

    #[test]
    fn empty_body_records_are_supported() {
        let mut ring: BootRing<512> = BootRing::new(2);
        ring.push(0, mono(9), b"").expect("fits");
        let mut scratch = [0u8; 4];
        let rec = ring.pop_oldest(&mut scratch).expect("ok").expect("rec");
        assert_eq!(rec.body_len, 0);
        assert_eq!(rec.cpu_seq, 0);
        assert_eq!(rec.monotonic, mono(9));
    }
}
