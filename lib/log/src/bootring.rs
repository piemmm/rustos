//! Bounded per-CPU early-boot log ring buffers.
//!
//! Before `/System/Logs` is writable, the kernel has nowhere durable to put a
//! log record, yet the earliest records — memory sizing, discovery, driver
//! bring-up — are exactly the ones an operator needs when a boot fails. Each
//! CPU therefore owns one [`BootRing`]: a fixed-capacity, allocation-free FIFO
//! over a caller-owned byte arena that retains the most recent records until
//! the journal can import them into the `boot` stream.
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
//! (§8.1) rather than leaving an undetectable gap.
//!
//! The ring is deliberately not internally synchronised, matching
//! [`crate::segment::SegmentWriter`]: a boot ring has a single writer (its own
//! CPU) and is drained once, at import, after that CPU has stopped writing to
//! it. Callers that share one across those phases provide the ordering.

use tairix_abi::{Duration64, Errno};

/// Bytes of per-frame bookkeeping the ring stores ahead of each record body:
/// a little-endian `u32` body length, a `u64` `cpu_seq`, and a
/// [`Duration64`] monotonic timestamp.
pub const FRAME_HEADER_LEN: usize = 4 + 8 + Duration64::WIRE_LEN;

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
/// were lost" rather than an unexplained sequence gap (§8.1).
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

/// A bounded, allocation-free FIFO of early-boot log records for one CPU.
///
/// The backing `buf` is treated as a byte ring: frames are written
/// back-to-back at the tail and may wrap the physical end of the buffer, so no
/// space is wasted padding to the end. The oldest frame is evicted to make
/// room when the ring is full.
pub struct BootRing<'a> {
    buf: &'a mut [u8],
    cpu_id: u32,
    /// Byte index of the oldest retained frame.
    head: usize,
    /// Bytes currently occupied by retained frames (`<= buf.len()`).
    used: usize,
    /// Number of retained frames.
    count: usize,
    /// The `cpu_seq` of the most recently pushed record, for the
    /// strictly-increasing check. `None` until the first push.
    last_pushed_seq: Option<u64>,
    /// Pending loss accumulated since the last [`Self::take_loss`]. `count`
    /// of zero means no loss is pending; the range is meaningful only then.
    lost_lo: u64,
    lost_hi: u64,
    lost_count: u64,
}

impl<'a> BootRing<'a> {
    /// Create a ring for CPU `cpu_id` backed by `buf`.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] if `buf` cannot hold even a zero-body frame
    /// ([`FRAME_HEADER_LEN`]).
    pub fn new(buf: &'a mut [u8], cpu_id: u32) -> Result<Self, Errno> {
        if buf.len() < FRAME_HEADER_LEN {
            return Err(Errno::BufferTooSmall);
        }
        Ok(Self {
            buf,
            cpu_id,
            head: 0,
            used: 0,
            count: 0,
            last_pushed_seq: None,
            lost_lo: 0,
            lost_hi: 0,
            lost_count: 0,
        })
    }

    /// The CPU this ring belongs to.
    #[must_use]
    pub const fn cpu_id(&self) -> u32 {
        self.cpu_id
    }

    /// Number of records currently retained.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.count
    }

    /// Whether the ring currently retains no records.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The ring's total capacity in bytes.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.buf.len()
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
        // `body.len() <= MAX_BOOT_RECORD_BODY`, so this conversion cannot fail;
        // the checked form keeps that guarantee explicit and fails closed.
        let body_len = u32::try_from(body.len()).map_err(|_| Errno::LengthOutOfRange)?;
        let need = FRAME_HEADER_LEN + body.len();
        if need > self.buf.len() {
            return Err(Errno::BufferTooSmall);
        }

        // Evict the oldest frames until the new one fits. `need <= capacity`,
        // so this terminates with room.
        while self.used + need > self.buf.len() {
            self.evict_oldest();
        }

        let tail = self.wrap(self.head + self.used);
        let mut pos = tail;
        self.write_u32(&mut pos, body_len);
        self.write_u64(&mut pos, cpu_seq);
        self.write_bytes(&mut pos, &monotonic.to_le_bytes());
        self.write_bytes(&mut pos, body);

        self.used += need;
        self.count += 1;
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
        if self.count == 0 {
            return Ok(None);
        }
        let mut pos = self.head;
        let body_len = self.read_u32(&mut pos) as usize;
        let cpu_seq = self.read_u64(&mut pos);
        let monotonic = self.read_duration(&mut pos)?;
        if body_len > scratch.len() {
            return Err(Errno::BufferTooSmall);
        }
        self.read_bytes(&mut pos, &mut scratch[..body_len]);

        let frame_len = FRAME_HEADER_LEN + body_len;
        self.head = self.wrap(self.head + frame_len);
        self.used -= frame_len;
        self.count -= 1;
        Ok(Some(DrainedRecord {
            cpu_seq,
            monotonic,
            body_len,
        }))
    }

    /// Take the pending loss range accumulated since the last call, clearing
    /// it.
    ///
    /// Returns `None` when no record has been evicted before being drained.
    /// The journal calls this before draining so it can emit the trusted loss
    /// record ahead of the surviving records (§8.1).
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

    /// Drop the oldest frame, folding its sequence into the pending loss range.
    fn evict_oldest(&mut self) {
        let mut pos = self.head;
        let body_len = self.read_u32(&mut pos) as usize;
        let cpu_seq = self.read_u64(&mut pos);
        let frame_len = FRAME_HEADER_LEN + body_len;

        if self.lost_count == 0 {
            self.lost_lo = cpu_seq;
        }
        self.lost_hi = cpu_seq;
        self.lost_count += 1;

        self.head = self.wrap(self.head + frame_len);
        self.used -= frame_len;
        self.count -= 1;
    }

    /// Reduce a logical (possibly past-the-end) index into the ring range.
    ///
    /// The addends are each `< 2 * buf.len()`, so one subtraction is enough;
    /// the branch keeps it allocation- and division-free on the hot path.
    fn wrap(&self, index: usize) -> usize {
        let cap = self.buf.len();
        if index >= cap {
            index - cap
        } else {
            index
        }
    }

    fn write_bytes(&mut self, pos: &mut usize, src: &[u8]) {
        let cap = self.buf.len();
        for &b in src {
            self.buf[*pos] = b;
            *pos = if *pos + 1 == cap { 0 } else { *pos + 1 };
        }
    }

    fn write_u32(&mut self, pos: &mut usize, v: u32) {
        self.write_bytes(pos, &v.to_le_bytes());
    }

    fn write_u64(&mut self, pos: &mut usize, v: u64) {
        self.write_bytes(pos, &v.to_le_bytes());
    }

    fn read_bytes(&self, pos: &mut usize, dst: &mut [u8]) {
        let cap = self.buf.len();
        for b in dst.iter_mut() {
            *b = self.buf[*pos];
            *pos = if *pos + 1 == cap { 0 } else { *pos + 1 };
        }
    }

    fn read_u32(&self, pos: &mut usize) -> u32 {
        let mut a = [0u8; 4];
        self.read_bytes(pos, &mut a);
        u32::from_le_bytes(a)
    }

    fn read_u64(&self, pos: &mut usize) -> u64 {
        let mut a = [0u8; 8];
        self.read_bytes(pos, &mut a);
        u64::from_le_bytes(a)
    }

    fn read_duration(&self, pos: &mut usize) -> Result<Duration64, Errno> {
        let mut a = [0u8; Duration64::WIRE_LEN];
        self.read_bytes(pos, &mut a);
        Duration64::from_bytes(&a)
    }
}

#[cfg(test)]
mod tests {
    use super::{BootRing, Duration64, Errno, FRAME_HEADER_LEN, MAX_BOOT_RECORD_BODY};

    fn mono(secs: u64) -> Duration64 {
        Duration64::from_secs(i64::try_from(secs).expect("test seconds fit i64"))
    }

    #[test]
    fn new_rejects_a_buffer_too_small_for_a_frame() {
        let mut buf = [0u8; FRAME_HEADER_LEN - 1];
        assert_eq!(
            BootRing::new(&mut buf, 0).err(),
            Some(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn push_then_drain_preserves_order_seq_time_and_body() {
        let mut buf = [0u8; 1024];
        let mut ring = BootRing::new(&mut buf, 3).expect("room");
        ring.push(0, mono(1), b"first").expect("fits");
        ring.push(1, mono(2), b"second").expect("fits");
        ring.push(2, mono(3), b"third").expect("fits");
        assert_eq!(ring.len(), 3);
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
        let mut buf = [0u8; 2 * (FRAME_HEADER_LEN + 4)];
        let mut ring = BootRing::new(&mut buf, 7).expect("room");
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

    #[test]
    fn wrapping_body_round_trips() {
        // A capacity that is not a multiple of the frame size, holding a few
        // frames, forces later frames to straddle the physical end of the
        // buffer as head/tail advance.
        let mut buf = [0u8; FRAME_HEADER_LEN * 4 + 7];
        let mut ring = BootRing::new(&mut buf, 1).expect("room");
        let mut scratch = [0u8; MAX_BOOT_RECORD_BODY];
        let push = |ring: &mut BootRing<'_>, seq: u64| {
            let payload = [(seq & 0xff) as u8; 9];
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
            assert_eq!(&scratch[..rec.body_len], &[(expect_seq & 0xff) as u8; 9]);
        }
        assert!(
            ring.take_loss().is_none(),
            "two resident frames never evict"
        );
    }

    #[test]
    fn push_rejects_non_increasing_sequence_without_side_effects() {
        let mut buf = [0u8; 512];
        let mut ring = BootRing::new(&mut buf, 0).expect("room");
        ring.push(5, mono(1), b"x").expect("fits");
        assert_eq!(ring.push(5, mono(2), b"y").err(), Some(Errno::OutOfRange));
        assert_eq!(ring.push(4, mono(2), b"y").err(), Some(Errno::OutOfRange));
        assert_eq!(ring.len(), 1, "rejected pushes left the ring untouched");
        ring.push(6, mono(2), b"z").expect("increasing accepted");
        assert_eq!(ring.len(), 2);
    }

    #[test]
    fn push_rejects_a_body_larger_than_the_cap() {
        let mut buf = [0u8; 8192];
        let mut ring = BootRing::new(&mut buf, 0).expect("room");
        let big = [0u8; MAX_BOOT_RECORD_BODY + 1];
        assert_eq!(
            ring.push(0, mono(1), &big).err(),
            Some(Errno::LengthOutOfRange)
        );
        assert!(ring.is_empty());
    }

    #[test]
    fn push_rejects_a_record_that_can_never_fit_the_ring() {
        let mut buf = [0u8; FRAME_HEADER_LEN + 4];
        let mut ring = BootRing::new(&mut buf, 0).expect("room");
        // Body of 5 needs FRAME_HEADER_LEN + 5, one more than the ring holds.
        assert_eq!(
            ring.push(0, mono(1), b"12345").err(),
            Some(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn pop_into_too_small_scratch_leaves_the_record() {
        let mut buf = [0u8; 512];
        let mut ring = BootRing::new(&mut buf, 0).expect("room");
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
        let mut buf = [0u8; 512];
        let mut ring = BootRing::new(&mut buf, 2).expect("room");
        ring.push(0, mono(9), b"").expect("fits");
        let mut scratch = [0u8; 4];
        let rec = ring.pop_oldest(&mut scratch).expect("ok").expect("rec");
        assert_eq!(rec.body_len, 0);
        assert_eq!(rec.cpu_seq, 0);
        assert_eq!(rec.monotonic, mono(9));
    }
}
