//! The framed console-output queue: the pure, allocation-free staging
//! structure a port's transmit path admits records into and drains bytes out
//! of.
//!
//! # Framing
//!
//! Output is stored as whole **frames**, never as loose bytes. A frame is one
//! indivisible unit of output — one rendered log line, or one chunk of a
//! program's own output — carried as
//!
//! ```text
//! [ body length : u16 LE ] [ class ] [ body … ] [ body length : u16 LE ]
//! ```
//!
//! The trailing length repeats the leading one so the **newest** frame can be
//! found and removed in constant time from the tail, which is what makes the
//! admission policy below possible without walking the queue.
//!
//! Only body bytes ever reach the device; the length and class words are
//! bookkeeping. Because admission is all-or-nothing per frame, a line on the
//! wire is either complete or absent — the queue cannot emit a half line, and
//! two producers' bytes can never interleave inside one line.
//!
//! # Admission and loss
//!
//! The queue is a *transmit* queue: bytes leave it in the order they were
//! admitted and the device consumes them from the head. So it sheds load at
//! the **tail**, never the head:
//!
//! 1. A frame that fits is appended.
//! 2. Otherwise an incoming record may evict queued records of **strictly
//!    lower severity** from the tail, newest first, so a `Critical` record
//!    still reaches the wire through a flood of `Debug` output.
//! 3. Otherwise the incoming record is refused whole.
//!
//! Evicting from the tail is safe by construction: a tail frame has not begun
//! transmitting, so removing it cannot truncate or splice what is already on
//! the wire. Program output ([`Class::Stream`]) is never evicted and never
//! silently dropped — a full queue yields a short write the caller can retry.
//!
//! Every refused or evicted frame is counted in [`Loss`]. The counters are the
//! queue's promise that loss is *reported*, never silent: the owning gate
//! renders them as a real diagnostic record at the exact stream position where
//! the gap occurred.
//!
//! # Retained history is a separate concern
//!
//! Shedding the newest output is right *here* because this structure's whole
//! job is ordered, uncorrupted delivery to one device. Keeping the most recent
//! records for later inspection is a different job with a different owner —
//! the boot audit ring and the journal — so this queue does not compromise
//! delivery order to imitate them.

use tairix_log::Level;

/// Leading `[length : u16][class]` bookkeeping bytes of a frame.
const HEADER_BYTES: usize = 3;

/// Trailing `[length : u16]` bookkeeping bytes of a frame, which make the
/// newest frame removable in constant time.
const FOOTER_BYTES: usize = 2;

/// Bookkeeping bytes each queued frame costs on top of its body.
pub const FRAME_OVERHEAD_BYTES: usize = HEADER_BYTES + FOOTER_BYTES;

/// Longest body a single frame may carry.
///
/// This is a validation bound, not a capacity: it keeps one pathological
/// record from monopolising the queue, and keeps the length word inside a
/// `u16`. A longer record is refused whole and counted, never truncated onto
/// the wire.
pub const MAX_RECORD_BYTES: usize = 1024;

/// Smallest capacity a queue may be built with: room for several
/// maximum-length records plus the loss report that describes their loss.
pub const MIN_CAPACITY_BYTES: usize = 4 * (MAX_RECORD_BYTES + FRAME_OVERHEAD_BYTES);

/// Capacity every kernel port builds its console queue with.
///
/// A console has to carry the very first boot record, long before a page
/// allocator or a heap exists, so its storage is a fixed reservation in `.bss`
/// rather than a capacity grown from discovered memory. What it must not be is
/// a size that quietly loses output: whatever does not fit is counted and
/// reported, so the bound is visible rather than silent.
///
/// The size follows the build profile, because the two produce output volumes
/// orders of magnitude apart. A development build streams a verbose
/// per-syscall boot log into a transmitter that carries a few thousand bytes a
/// second, so it is given room for a whole bursty driver bring-up; a shippable
/// build logs sparingly and would rather have the memory. At 115200 baud the
/// small queue represents about two thirds of a second of transmission and the
/// large one about twenty seconds.
///
/// A host build takes the small size: it links this for the unit tests, which
/// build their own queues, and does not want the reservation.
#[cfg(all(debug_assertions, target_os = "none"))]
pub const DEFAULT_CAPACITY_BYTES: usize = 256 * 1024;

/// See the development-build variant above. It is comfortably above
/// [`MIN_CAPACITY_BYTES`], which is the point: a queue too small to hold a few
/// full-length records *and* the report describing what it shed could not keep
/// the promise that lost output is always accounted for.
#[cfg(not(all(debug_assertions, target_os = "none")))]
pub const DEFAULT_CAPACITY_BYTES: usize = 8 * 1024;

/// Marks a [`Class::Stream`] frame in the stored class byte. Values below it
/// are a [`Level`] discriminant, so the two cases cannot collide.
const STREAM_CLASS: u8 = 0x80;

/// What a frame carries, and therefore how the admission policy may treat it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Class {
    /// A rendered diagnostic record at the given severity. Best-effort: it may
    /// be evicted by a more severe record, or refused, and either way counted.
    Record(Level),
    /// A program's own output. Never evicted and never silently dropped; a
    /// full queue reports a short write instead.
    Stream,
}

impl Class {
    /// Stored form.
    const fn encode(self) -> u8 {
        match self {
            Self::Record(level) => level.as_u8(),
            Self::Stream => STREAM_CLASS,
        }
    }

    /// Inverse of [`Self::encode`]. An unrecognised byte decodes to the
    /// never-evictable [`Self::Stream`], so a corrupted class word can only
    /// ever make the queue *more* conservative.
    const fn decode(stored: u8) -> Self {
        match Level::from_u8(stored) {
            Some(level) => Self::Record(level),
            None => Self::Stream,
        }
    }

    /// Whether an incoming record at `incoming` severity may evict this frame.
    const fn evictable_by(self, incoming: Level) -> bool {
        match self {
            Self::Record(level) => level.as_u8() < incoming.as_u8(),
            Self::Stream => false,
        }
    }
}

/// Outcome of admitting one whole frame.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Admit {
    /// The frame is queued in full and will reach the device.
    Queued,
    /// The frame did not fit and has been counted in [`Loss`]. Nothing of it
    /// was stored.
    Refused,
}

/// Output the queue could not accept, pending report.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Loss {
    /// Whole records refused or evicted.
    pub records: u64,
    /// Body bytes those records carried.
    pub bytes: u64,
}

impl Loss {
    /// Whether anything has been lost since the last report.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.records == 0
    }

    /// Charge one lost record of `body` bytes, saturating rather than wrapping
    /// so a long-running flood reports a ceiling instead of a fresh small
    /// number.
    const fn charge(&mut self, body: usize) {
        self.records = self.records.saturating_add(1);
        self.bytes = self.bytes.saturating_add(body as u64);
    }
}

/// The frame being rendered at the tail, before it is committed or rolled
/// back.
#[derive(Copy, Clone, Debug)]
struct Pending {
    /// Stored class byte of the frame under construction.
    class: u8,
    /// Body bytes actually written into the queue.
    stored: usize,
    /// Body bytes the renderer produced, including any that did not fit. The
    /// difference from `stored` is exactly how much room the frame is short.
    produced: usize,
}

/// A bounded, allocation-free queue of whole output frames for one console
/// device.
///
/// `CAP` is the byte capacity, and must be at least [`MIN_CAPACITY_BYTES`].
/// The structure is pure: it owns no lock, touches no device, and every method
/// is total (no panic, no allocation), so it is exercised directly by host
/// tests and wrapped by [`crate::ConsoleGate`] on a target.
pub struct OutQueue<const CAP: usize> {
    /// Frame storage, used as a circular buffer.
    buf: [u8; CAP],
    /// Index of the head frame's first header byte.
    head: usize,
    /// Bytes occupied by committed frames, bodies and bookkeeping together.
    used: usize,
    /// Committed frames.
    frames: usize,
    /// Body bytes of the head frame already handed to the transmitter.
    head_sent: usize,
    /// The frame being rendered at the tail, if any.
    pending: Option<Pending>,
    /// Records refused or evicted since the last report.
    loss: Loss,
}

impl<const CAP: usize> OutQueue<CAP> {
    /// Compile-time capacity floor, evaluated by [`Self::new`] so an
    /// undersized queue is a build error rather than a runtime surprise.
    const CAPACITY_FLOOR: () = assert!(
        CAP >= MIN_CAPACITY_BYTES,
        "a console-output queue must hold several maximum-length records plus a loss report"
    );

    /// An empty queue.
    #[must_use]
    pub const fn new() -> Self {
        let () = Self::CAPACITY_FLOOR;
        Self {
            buf: [0; CAP],
            head: 0,
            used: 0,
            frames: 0,
            head_sent: 0,
            pending: None,
            loss: Loss {
                records: 0,
                bytes: 0,
            },
        }
    }

    /// Whether no committed frame is waiting for the device.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.frames == 0
    }

    /// Committed frames waiting for the device.
    #[must_use]
    pub const fn frames(&self) -> usize {
        self.frames
    }

    /// Bytes occupied by committed frames, bodies and bookkeeping together.
    #[must_use]
    pub const fn used(&self) -> usize {
        self.used
    }

    /// Output refused or evicted since the last [`Self::take_loss`].
    #[must_use]
    pub const fn loss(&self) -> Loss {
        self.loss
    }

    /// Take the pending loss for reporting, clearing the counters.
    ///
    /// The caller reports what it takes: a report that itself fails to reach
    /// the queue must be charged back with [`Self::restore_loss`] so no loss
    /// is ever forgotten.
    pub const fn take_loss(&mut self) -> Loss {
        let taken = self.loss;
        self.loss = Loss {
            records: 0,
            bytes: 0,
        };
        taken
    }

    /// Charge previously taken loss back, for a loss report that could not be
    /// queued.
    pub const fn restore_loss(&mut self, loss: Loss) {
        self.loss.records = self.loss.records.saturating_add(loss.records);
        self.loss.bytes = self.loss.bytes.saturating_add(loss.bytes);
    }

    /// Body bytes a new frame could hold right now.
    #[must_use]
    pub const fn free(&self) -> usize {
        if self.used + FRAME_OVERHEAD_BYTES >= CAP {
            0
        } else {
            CAP - self.used - FRAME_OVERHEAD_BYTES
        }
    }

    /// Start rendering a frame at the tail.
    ///
    /// Any frame already under construction is rolled back first, so an
    /// interrupted render can never leave a half frame behind.
    pub fn begin(&mut self, class: Class) {
        self.rollback();
        self.pending = Some(Pending {
            class: class.encode(),
            stored: 0,
            produced: 0,
        });
    }

    /// Append one body byte to the frame under construction.
    ///
    /// Bytes beyond the room available are counted but not stored, so
    /// [`Self::commit`] knows exactly how short the frame is without the
    /// renderer having to measure anything. A byte pushed with no frame open
    /// is ignored.
    pub fn push(&mut self, byte: u8) {
        let Some(mut pending) = self.pending else {
            return;
        };
        pending.produced += 1;
        if pending.produced <= self.free() && pending.produced <= MAX_RECORD_BYTES {
            let at = Self::wrap(self.head + self.used + HEADER_BYTES + pending.stored);
            self.buf[at] = byte;
            pending.stored = pending.produced;
        }
        self.pending = Some(pending);
    }

    /// Commit the frame under construction, or refuse it whole.
    ///
    /// A frame that fits is sealed with its header and footer and becomes
    /// visible to [`Self::peek`]. A frame that does not fit is rolled back and
    /// charged to [`Loss`]; [`Self::shortfall`] reports how much room a retry
    /// would need.
    pub fn commit(&mut self) -> Admit {
        let Some(pending) = self.pending.take() else {
            return Admit::Refused;
        };
        if pending.produced == 0 {
            // An empty body owes the device nothing, so it is delivered by
            // definition. Sealing it would leave a committed frame that
            // [`Self::peek`] can never yield a byte for, so the queue would
            // never retire it and would report output owed forever.
            return Admit::Queued;
        }
        if pending.produced != pending.stored || pending.produced > MAX_RECORD_BYTES {
            self.zero(self.head + self.used + HEADER_BYTES, pending.stored);
            self.loss.charge(pending.produced);
            return Admit::Refused;
        }
        let length = pending.stored;
        let Ok(length_word) = u16::try_from(length) else {
            // Unreachable: the check above bounds a body by `MAX_RECORD_BYTES`,
            // far below the length word's range. Converting rather than
            // asserting keeps that honest, and refusing is the fail-closed
            // answer if the bound is ever widened past the word.
            self.zero(self.head + self.used + HEADER_BYTES, pending.stored);
            self.loss.charge(pending.produced);
            return Admit::Refused;
        };
        let start = Self::wrap(self.head + self.used);
        self.write_length(start, length_word);
        self.buf[Self::wrap(start + 2)] = pending.class;
        self.write_length(start + HEADER_BYTES + length, length_word);
        self.used += FRAME_OVERHEAD_BYTES + length;
        self.frames += 1;
        Admit::Queued
    }

    /// Discard the frame under construction, if any, without charging loss.
    fn rollback(&mut self) {
        if let Some(pending) = self.pending.take() {
            self.zero(self.head + self.used + HEADER_BYTES, pending.stored);
        }
    }

    /// Admit one whole frame of already-formatted bytes.
    ///
    /// The all-or-nothing counterpart of [`Self::begin`] / [`Self::push`] /
    /// [`Self::commit`], for output that is already a contiguous slice.
    pub fn admit(&mut self, class: Class, body: &[u8]) -> Admit {
        self.begin(class);
        for &byte in body {
            self.push(byte);
        }
        self.commit()
    }

    /// Admit as much of `body` as fits, as one frame, returning the bytes
    /// accepted.
    ///
    /// The honest-short-write path for program output: the caller learns
    /// exactly what was taken and retries the rest, so nothing is written into
    /// a void and reported as delivered.
    pub fn admit_prefix(&mut self, class: Class, body: &[u8]) -> usize {
        let room = self.free().min(MAX_RECORD_BYTES);
        let take = body.len().min(room);
        if take == 0 {
            return 0;
        }
        match self.admit(class, &body[..take]) {
            Admit::Queued => take,
            Admit::Refused => 0,
        }
    }

    /// Body bytes a refused frame of `produced` bytes is short of fitting.
    #[must_use]
    pub const fn shortfall(&self, produced: usize) -> usize {
        produced.saturating_sub(self.free())
    }

    /// Free `wanted` body bytes by evicting queued records of severity
    /// strictly below `incoming` from the tail, newest first.
    ///
    /// Returns whether the room now exists. Eviction stops at the first frame
    /// it may not take — program output, an equally or more severe record, or
    /// the head frame once it has begun transmitting — so the wire never loses
    /// a line it has already started, and a flood of low-severity output can
    /// never crowd out a severe record.
    pub fn evict_tail_below(&mut self, incoming: Level, wanted: usize) -> bool {
        while self.free() < wanted {
            if self.frames == 0 || (self.frames == 1 && self.head_sent != 0) {
                return false;
            }
            let tail = Self::wrap(self.head + self.used);
            let length = self.read_length(tail + CAP - FOOTER_BYTES);
            let start = Self::wrap(tail + CAP - FOOTER_BYTES - length - HEADER_BYTES);
            if !Class::decode(self.buf[Self::wrap(start + 2)]).evictable_by(incoming) {
                return false;
            }
            self.zero(start, HEADER_BYTES + length + FOOTER_BYTES);
            self.used -= FRAME_OVERHEAD_BYTES + length;
            self.frames -= 1;
            self.loss.charge(length);
        }
        true
    }

    /// The next contiguous run of untransmitted body bytes at the head, or an
    /// empty slice when nothing is queued.
    ///
    /// Bookkeeping bytes are never returned, so a caller can hand the slice
    /// straight to the device.
    #[must_use]
    pub fn peek(&self) -> &[u8] {
        if self.frames == 0 {
            return &[];
        }
        let length = self.read_length(self.head);
        let from = Self::wrap(self.head + HEADER_BYTES + self.head_sent);
        let run = (length - self.head_sent).min(CAP - from);
        &self.buf[from..from + run]
    }

    /// Release `count` bytes the device has accepted from the run
    /// [`Self::peek`] returned, retiring the head frame once its body is fully
    /// transmitted.
    ///
    /// Transmitted bytes are zeroed as they are released: this queue carries a
    /// program's own output, which may hold whatever the program wrote, so
    /// delivered bytes do not linger in kernel memory.
    pub fn consume(&mut self, count: usize) {
        if self.frames == 0 {
            return;
        }
        let length = self.read_length(self.head);
        let taken = count.min(length - self.head_sent);
        self.zero(self.head + HEADER_BYTES + self.head_sent, taken);
        self.head_sent += taken;
        if self.head_sent == length {
            self.zero(self.head, HEADER_BYTES);
            self.zero(self.head + HEADER_BYTES + length, FOOTER_BYTES);
            self.head = Self::wrap(self.head + FRAME_OVERHEAD_BYTES + length);
            self.used -= FRAME_OVERHEAD_BYTES + length;
            self.frames -= 1;
            self.head_sent = 0;
        }
    }

    /// Abandon the untransmitted remainder of the head frame, counting it as
    /// lost.
    ///
    /// For a transmitter that has stopped draining mid-line: the bytes it
    /// already took cannot be recalled, so the rest of that line is dropped
    /// rather than resumed out of context later, and the wire picks up at the
    /// next line boundary if the device recovers.
    pub fn discard_head_frame(&mut self) {
        if self.frames == 0 {
            return;
        }
        let length = self.read_length(self.head);
        self.loss.charge(length - self.head_sent);
        self.zero(self.head, FRAME_OVERHEAD_BYTES + length);
        self.head = Self::wrap(self.head + FRAME_OVERHEAD_BYTES + length);
        self.used -= FRAME_OVERHEAD_BYTES + length;
        self.frames -= 1;
        self.head_sent = 0;
    }

    /// The raw frame storage, so a test can prove a scrub actually happened.
    #[cfg(test)]
    fn storage(&self) -> &[u8] {
        &self.buf
    }

    /// Reduce an index to the buffer's bounds.
    const fn wrap(index: usize) -> usize {
        index % CAP
    }

    /// Read a little-endian length word that may straddle the wrap.
    fn read_length(&self, at: usize) -> usize {
        let low = self.buf[Self::wrap(at)];
        let high = self.buf[Self::wrap(at + 1)];
        usize::from(u16::from_le_bytes([low, high]))
    }

    /// Write a little-endian length word that may straddle the wrap.
    const fn write_length(&mut self, at: usize, length: u16) {
        let bytes = length.to_le_bytes();
        let low = Self::wrap(at);
        let high = Self::wrap(at + 1);
        self.buf[low] = bytes[0];
        self.buf[high] = bytes[1];
    }

    /// Zero `len` bytes from `start`, wrapping.
    const fn zero(&mut self, start: usize, len: usize) {
        let mut offset = 0;
        while offset < len {
            let at = Self::wrap(start + offset);
            self.buf[at] = 0;
            offset += 1;
        }
    }
}

impl<const CAP: usize> Default for OutQueue<CAP> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[path = "queue_tests.rs"]
mod tests;
