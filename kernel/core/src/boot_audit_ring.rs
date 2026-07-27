//! Retained, tail-able in-memory boot audit-log ring.
//!
//! Before `/System/Logs` is writable, the earliest audit records — memory
//! sizing, discovery, driver bring-up, unlock decisions — are exactly the ones
//! an operator needs when a boot goes wrong, yet they have nowhere durable to
//! live. [`tairix_log::BootRing`] retains them too, but it is a *drain-once*
//! FIFO: importing it into the journal empties it, so it cannot serve a viewer
//! that reads the same history repeatedly. This ring fills that gap. It is a
//! [`Sink`] that copies each record it receives into a bounded ring and lets a
//! viewer read the most recent records **non-destructively**, any number of
//! times — the store the pre-boot Supervisor's `log` command reads
//! (`plans/NEW-SUPERVISOR.md`).
//!
//! # A recorder, not a re-derivation
//!
//! The ring stores only what an audit line already carries: the record's
//! severity ([`Level`]), its stable [`EventId`], the message, and the
//! monotonic time it was produced. It computes none of that: it is composed
//! *alongside* the diagnostic/serial audit sink at boot, so every record the
//! kernel already emits is teed into it — the one source of truth stays where
//! it lives (the charter forbids duplicating it). It does not store the
//! structured [`tairix_log::Field`]s: the tail is a one-line-per-record
//! operator view, and copying unbounded field lists into a fixed slot would
//! blow the per-record budget for data the viewer does not show.
//!
//! # SMP- and ISR-safe by construction
//!
//! A `Sink` is shared behind a `&'static` and written from any context —
//! including an interrupt handler that logs, on a CPU whose interrupted task
//! is mid-write. The ring therefore guards its state with the shared
//! [`IrqSafeSpinLock`], which masks interrupts on the current CPU for the
//! (short, allocation- and I/O-free) duration of a record copy. The lock is
//! **never** held across rendering: a viewer reads one record per lock
//! acquisition (see [`BootAuditRing::record`]), so a slow console write can
//! never keep interrupts masked. `I` is the architecture's
//! [`InterruptControl`] implementation, supplied by the bin crate that
//! instantiates the `static`; a host test uses the default
//! [`NopInterruptControl`].
//!
//! # Stable record identity under a live writer
//!
//! Every record is assigned a strictly increasing global sequence number as
//! it is written. A viewer discovers the retained range with
//! [`BootAuditRing::seq_range`] and fetches each record by its sequence with
//! [`BootAuditRing::record`]. Because a sequence names *the same* record for
//! as long as it is retained, a concurrent write that evicts the oldest
//! record can only make a later fetch return [`None`] (the record aged out) —
//! it can never silently return a *different* record under the same sequence.
//! The viewer skips a `None` and moves on, so a tail read is consistent even
//! while the ring is still being written.
//!
//! # Bounded, never allocating, never panicking
//!
//! The ring is a fixed-capacity `N`-record store over an inline array: it
//! allocates nothing, and pushing into a full ring overwrites the oldest
//! record (a tail keeps *recent* history — losing old records is the point,
//! unlike the import-oriented [`tairix_log::BootRing`]). A message longer than
//! [`TAIL_MESSAGE_MAX`] is truncated on a UTF-8 boundary rather than rejected
//! or truncated mid-character, so a stored message is always valid UTF-8 and
//! no input can make a read panic.

use tairix_abi::Duration64;
use tairix_log::{Event, EventId, Level, Sink};
use tairix_sync::irq::{InterruptControl, NopInterruptControl};
use tairix_sync::IrqSafeSpinLock;

/// Largest message body, in bytes, a single retained record stores.
///
/// Matches the `lib/log` one-line message convention (a record is held to
/// <= 120 characters so it fits one terminal line); a longer message is
/// truncated on a UTF-8 boundary when stored, never rejected, so a record can
/// always be retained.
pub const TAIL_MESSAGE_MAX: usize = 120;

/// The number of most-recent boot audit records the retained ring keeps.
///
/// This is a **diagnostic tail bound**, not a scalable capacity: the ring's
/// whole purpose is "the last N audit records", so a fixed N is the correct
/// shape (a larger machine gains nothing from a longer boot-audit tail, and a
/// smaller one must not pay for one). A boot emits well under this many
/// *audit-level* records — the boot lifecycle markers plus the security
/// decisions (discovery, driver bring-up, the root-unlock verdicts) — so this
/// comfortably retains a whole boot's trail; if a pathological boot exceeds
/// it, the tail keeps the most recent records, which is exactly what an
/// operator inspecting a failed boot wants. Defined once here so every
/// architecture's `BootAuditRing` `static` sizes identically rather than
/// copying a literal.
pub const BOOT_AUDIT_RING_CAPACITY: usize = 128;

/// A monotonic clock the ring stamps each record with.
///
/// A [`Sink`] receives an [`Event`] with no timestamp, so the ring reads the
/// time itself when a record arrives. The kernel supplies its monotonic
/// since-boot clock; a host test supplies a scripted one. A plain function
/// pointer is `const`-evaluable, so a `BootAuditRing` built from one can live
/// in a `static`.
pub type MonotonicClock = fn() -> Duration64;

/// The kernel's monotonic since-boot clock, as a [`MonotonicClock`] a
/// production [`BootAuditRing`] `static` is built from.
///
/// It reads the single arch-neutral monotonic clock the wait-queue timer
/// already runs on ([`wait_now_ns`](crate::waitq::wait_now_ns)), so every
/// architecture stamps its retained records from the *same* source with no
/// per-port clock code — the values differ, the code does not. Before the
/// boot path installs that clock (the earliest records, emitted while the
/// scheduler is still being built) it returns [`Duration64::ZERO`]: an honest
/// "monotonic time is not running yet" rather than a fabricated instant. The
/// tail is ordered by each record's strictly-increasing sequence, never by
/// this stamp, so a zero on the earliest records never disorders it.
#[must_use]
pub fn boot_audit_clock() -> Duration64 {
    stamp_from(crate::waitq::wait_now_ns())
}

/// Turn an optional monotonic-ns reading into the stamp
/// [`boot_audit_clock`] returns: the elapsed duration when the clock is
/// running, or [`Duration64::ZERO`] before it is installed.
///
/// Split out from [`boot_audit_clock`] so the mapping is unit-testable without
/// installing the global wait-queue clock.
fn stamp_from(monotonic_ns: Option<u64>) -> Duration64 {
    monotonic_ns.map_or(Duration64::ZERO, Duration64::from_nanos)
}

/// One record copied out of a [`BootAuditRing`] for display.
///
/// `Copy` and self-contained (the message lives in an inline buffer), so a
/// viewer takes it out from under the ring lock and renders it afterwards
/// without holding the lock.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct TailRecord {
    /// The record's strictly-increasing global sequence number.
    pub seq: u64,
    /// The record's severity.
    pub level: Level,
    /// The record's stable event identifier.
    pub id: EventId,
    /// The monotonic time the record was produced.
    pub monotonic: Duration64,
    /// The message bytes, always valid UTF-8 (see [`TailRecord::message`]).
    msg: [u8; TAIL_MESSAGE_MAX],
    /// Number of valid bytes in `msg`.
    msg_len: u16,
}

impl TailRecord {
    /// The record's message.
    ///
    /// Always valid UTF-8: the ring truncates on a character boundary when
    /// storing, so this never fails; the fail-safe empty string is returned
    /// only if a byte slice were somehow invalid, which cannot happen.
    #[must_use]
    pub fn message(&self) -> &str {
        let len = self.msg_len as usize;
        core::str::from_utf8(&self.msg[..len]).unwrap_or("")
    }
}

/// The interior, lock-guarded ring state.
///
/// `slots` is a physical ring: `head` is the index of the oldest retained
/// record and `count` the number retained. `next_seq` is the sequence the
/// next pushed record will take, so the newest retained record's sequence is
/// `next_seq - 1` and the oldest retained record's is `next_seq - count`.
struct RingState<const N: usize> {
    slots: [Slot; N],
    head: usize,
    count: usize,
    next_seq: u64,
}

/// One stored record. A plain POD so an array of them is `Copy`/`Send` and can
/// live in a `static` with no initialiser code.
#[derive(Copy, Clone)]
struct Slot {
    seq: u64,
    level: u8,
    id: u32,
    monotonic: Duration64,
    msg: [u8; TAIL_MESSAGE_MAX],
    msg_len: u16,
}

impl Slot {
    const EMPTY: Self = Self {
        seq: 0,
        level: 0,
        id: 0,
        monotonic: Duration64::ZERO,
        msg: [0u8; TAIL_MESSAGE_MAX],
        msg_len: 0,
    };
}

impl<const N: usize> RingState<N> {
    const fn new() -> Self {
        Self {
            slots: [Slot::EMPTY; N],
            head: 0,
            count: 0,
            next_seq: 0,
        }
    }

    /// Append one record, overwriting the oldest when the ring is full.
    fn push(&mut self, level: Level, id: EventId, monotonic: Duration64, message: &str) {
        // `N == 0` is a degenerate configuration: there is nowhere to store a
        // record, so drop it (still advancing the sequence keeps the counter
        // meaningful). A real ring has `N >= 1`.
        if N == 0 {
            self.next_seq = self.next_seq.wrapping_add(1);
            return;
        }

        let pos = if self.count == N {
            // Full: overwrite the oldest and advance the head past it.
            let oldest = self.head;
            self.head = wrap_index(self.head + 1, N);
            oldest
        } else {
            let tail = wrap_index(self.head + self.count, N);
            self.count += 1;
            tail
        };

        let truncated = truncate_on_char_boundary(message, TAIL_MESSAGE_MAX);
        let bytes = truncated.as_bytes();
        let slot = &mut self.slots[pos];
        slot.seq = self.next_seq;
        slot.level = level.as_u8();
        slot.id = id.0;
        slot.monotonic = monotonic;
        slot.msg[..bytes.len()].copy_from_slice(bytes);
        // `bytes.len() <= TAIL_MESSAGE_MAX <= u16::MAX`, so this cannot lose
        // data; the fallback keeps the store fail-safe.
        slot.msg_len = u16::try_from(bytes.len()).unwrap_or(0);

        self.next_seq = self.next_seq.wrapping_add(1);
    }

    /// The `(oldest, newest)` retained sequence numbers, or `None` when empty.
    fn seq_range(&self) -> Option<(u64, u64)> {
        if self.count == 0 {
            return None;
        }
        let newest = self.next_seq - 1;
        let oldest = self.next_seq - self.count as u64;
        Some((oldest, newest))
    }

    /// Copy out the record with sequence `seq`, or `None` if it is not (or no
    /// longer) retained.
    fn record(&self, seq: u64) -> Option<TailRecord> {
        let (oldest, newest) = self.seq_range()?;
        if seq < oldest || seq > newest {
            return None;
        }
        // `seq - oldest < count <= N`, so it always fits a `usize`; the
        // checked form keeps a read fail-safe rather than truncating.
        let offset = usize::try_from(seq - oldest).ok()?;
        let pos = wrap_index(self.head + offset, N);
        let slot = &self.slots[pos];
        Some(TailRecord {
            seq: slot.seq,
            // A stored level byte always came from `Level::as_u8`, so it is a
            // valid discriminant; the fail-safe keeps a read from panicking.
            level: Level::from_u8(slot.level).unwrap_or(Level::Info),
            id: EventId(slot.id),
            monotonic: slot.monotonic,
            msg: slot.msg,
            msg_len: slot.msg_len,
        })
    }
}

/// A retained, tail-able, SMP-/ISR-safe in-memory ring of the most recent `N`
/// boot audit records.
///
/// See the [module documentation](self) for the design. Construct one in a
/// `static` with [`BootAuditRing::new`], compose it alongside the boot audit
/// sink so every record is teed into it, and read it back non-destructively
/// through [`seq_range`](Self::seq_range) + [`record`](Self::record).
///
/// `I` selects the interrupt-control primitive the guarding lock uses: the
/// bin crate supplies its architecture's implementation so a record copy masks
/// interrupts on the current CPU; host tests use the default
/// [`NopInterruptControl`].
pub struct BootAuditRing<const N: usize, I: InterruptControl = NopInterruptControl> {
    state: IrqSafeSpinLock<RingState<N>, I>,
    clock: MonotonicClock,
}

impl<const N: usize, I: InterruptControl> BootAuditRing<N, I> {
    /// Create an empty ring that stamps each record using `clock`.
    #[must_use]
    pub const fn new(clock: MonotonicClock) -> Self {
        Self {
            state: IrqSafeSpinLock::new(RingState::new()),
            clock,
        }
    }

    /// Number of records currently retained (at most `N`).
    #[must_use]
    pub fn len(&self) -> usize {
        self.state.lock().count
    }

    /// Whether the ring currently retains no records.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Total number of records ever written to the ring, including those since
    /// evicted. A viewer can show "showing the last `k` of `total`".
    #[must_use]
    pub fn total(&self) -> u64 {
        self.state.lock().next_seq
    }

    /// The `(oldest, newest)` retained sequence numbers, or `None` when empty.
    ///
    /// A viewer walks `oldest..=newest` (bounded to the last `k` it wants to
    /// show) and fetches each with [`record`](Self::record).
    #[must_use]
    pub fn seq_range(&self) -> Option<(u64, u64)> {
        self.state.lock().seq_range()
    }

    /// Copy out the record with sequence `seq`, or `None` if it is not (or no
    /// longer) retained.
    ///
    /// Each call takes the lock for only the copy, so a caller may render the
    /// returned record without holding the lock — a slow console write never
    /// keeps interrupts masked.
    #[must_use]
    pub fn record(&self, seq: u64) -> Option<TailRecord> {
        self.state.lock().record(seq)
    }
}

impl<const N: usize, I: InterruptControl> Sink for BootAuditRing<N, I> {
    fn write_event(&self, event: &Event<'_>) {
        let monotonic = (self.clock)();
        self.state
            .lock()
            .push(event.level, event.id, monotonic, event.message);
    }
}

/// Reduce a logical (possibly one-past-the-end) index into `0..cap`.
///
/// `index` is always `< 2 * cap` at every call site, so a single subtraction
/// suffices and the ring stays division-free.
const fn wrap_index(index: usize, cap: usize) -> usize {
    if index >= cap {
        index - cap
    } else {
        index
    }
}

/// The largest prefix of `s` that fits in `max` bytes and ends on a UTF-8
/// character boundary.
fn truncate_on_char_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    // `end` is a valid char boundary (0 always is), so slicing cannot panic.
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::{BootAuditRing, TAIL_MESSAGE_MAX};
    use core::sync::atomic::{AtomicU64, Ordering};
    use tairix_abi::Duration64;
    use tairix_log::{log, set_max_level, Event, EventId, Level, Sink};

    /// A scripted monotonic clock: each read returns a strictly increasing
    /// second so stored timestamps are distinguishable.
    static CLOCK_SECS: AtomicU64 = AtomicU64::new(0);

    fn scripted_clock() -> Duration64 {
        let secs = CLOCK_SECS.fetch_add(1, Ordering::Relaxed);
        Duration64::from_secs(i64::try_from(secs).expect("test seconds fit i64"))
    }

    fn reset_clock() {
        CLOCK_SECS.store(0, Ordering::Relaxed);
    }

    fn event(id: u32, level: Level, message: &str) -> Event<'_> {
        Event {
            level,
            id: EventId(id),
            message,
            fields: &[],
        }
    }

    /// Collect the whole retained tail, oldest-first, via the public reader.
    fn drain_tail<const N: usize>(ring: &BootAuditRing<N>) -> Vec<(u64, u32, String)> {
        let mut out = Vec::new();
        if let Some((oldest, newest)) = ring.seq_range() {
            for seq in oldest..=newest {
                if let Some(rec) = ring.record(seq) {
                    out.push((rec.seq, rec.id.0, rec.message().to_string()));
                }
            }
        }
        out
    }

    #[test]
    fn stamp_is_zero_before_the_clock_runs_and_elapsed_after() {
        use super::stamp_from;
        // No installed clock (earliest boot): an honest zero, not a fabricated
        // instant.
        assert_eq!(stamp_from(None), Duration64::ZERO);
        // A running clock projects the elapsed nanoseconds forward.
        assert_eq!(stamp_from(Some(0)), Duration64::ZERO);
        assert_eq!(
            stamp_from(Some(2_500_000_001)),
            Duration64::from_nanos(2_500_000_001)
        );
    }

    #[test]
    fn empty_ring_reads_back_nothing() {
        let ring: BootAuditRing<8> = BootAuditRing::new(scripted_clock);
        assert!(ring.is_empty());
        assert_eq!(ring.len(), 0);
        assert_eq!(ring.total(), 0);
        assert_eq!(ring.seq_range(), None);
        assert_eq!(ring.record(0), None);
    }

    #[test]
    fn records_are_retained_and_read_non_destructively() {
        reset_clock();
        let ring: BootAuditRing<8> = BootAuditRing::new(scripted_clock);
        ring.write_event(&event(10, Level::Info, "first"));
        ring.write_event(&event(11, Level::Warn, "second"));
        ring.write_event(&event(12, Level::Error, "third"));

        assert_eq!(ring.len(), 3);
        assert_eq!(ring.total(), 3);
        assert_eq!(ring.seq_range(), Some((0, 2)));

        let expected = vec![
            (0u64, 10u32, "first".to_string()),
            (1, 11, "second".to_string()),
            (2, 12, "third".to_string()),
        ];
        // Reading twice yields the same records: the reader never drains.
        assert_eq!(drain_tail(&ring), expected);
        assert_eq!(drain_tail(&ring), expected);

        // The severity and timestamp survive the round trip.
        let second = ring.record(1).expect("retained");
        assert_eq!(second.level, Level::Warn);
        assert_eq!(second.monotonic, Duration64::from_secs(1));
    }

    #[test]
    fn full_ring_overwrites_oldest_and_keeps_the_newest_n() {
        reset_clock();
        let ring: BootAuditRing<3> = BootAuditRing::new(scripted_clock);
        for i in 0..7u32 {
            ring.write_event(&event(100 + i, Level::Info, "x"));
        }
        // Only the newest three survive; total still counts every push.
        assert_eq!(ring.len(), 3);
        assert_eq!(ring.total(), 7);
        assert_eq!(ring.seq_range(), Some((4, 6)));

        let tail = drain_tail(&ring);
        assert_eq!(
            tail,
            vec![
                (4u64, 104u32, "x".to_string()),
                (5, 105, "x".to_string()),
                (6, 106, "x".to_string()),
            ]
        );
        // An evicted sequence reads back as `None`, never as a wrong record.
        assert_eq!(ring.record(3), None);
        assert_eq!(ring.record(0), None);
    }

    #[test]
    fn record_out_of_range_returns_none() {
        reset_clock();
        let ring: BootAuditRing<4> = BootAuditRing::new(scripted_clock);
        ring.write_event(&event(1, Level::Info, "only"));
        assert_eq!(ring.seq_range(), Some((0, 0)));
        assert!(ring.record(0).is_some());
        assert_eq!(ring.record(1), None); // not yet written
        assert_eq!(ring.record(u64::MAX), None);
    }

    #[test]
    fn an_over_long_message_is_truncated_on_a_char_boundary() {
        reset_clock();
        let ring: BootAuditRing<2> = BootAuditRing::new(scripted_clock);
        // Fill to one byte below the cap with ASCII, then a 2-byte char that
        // would straddle the byte boundary and must not be split.
        let mut message = String::new();
        for _ in 0..(TAIL_MESSAGE_MAX - 1) {
            message.push('a');
        }
        message.push('é'); // 2 bytes: would land at TAIL_MESSAGE_MAX + 1
        ring.write_event(&event(1, Level::Info, &message));

        let rec = ring.record(0).expect("stored");
        let stored = rec.message();
        // The 2-byte char was dropped whole; the ASCII prefix survives.
        assert_eq!(stored.len(), TAIL_MESSAGE_MAX - 1);
        assert!(stored.chars().all(|c| c == 'a'));
        assert!(core::str::from_utf8(stored.as_bytes()).is_ok());
    }

    #[test]
    fn degenerate_zero_capacity_ring_never_panics() {
        reset_clock();
        let ring: BootAuditRing<0> = BootAuditRing::new(scripted_clock);
        ring.write_event(&event(1, Level::Info, "dropped"));
        ring.write_event(&event(2, Level::Info, "dropped"));
        assert!(ring.is_empty());
        assert_eq!(ring.seq_range(), None);
        // The sequence counter still advances, so `total` stays meaningful.
        assert_eq!(ring.total(), 2);
        assert_eq!(ring.record(0), None);
    }

    #[test]
    fn filters_below_threshold_are_never_recorded() {
        // The ring is a `Sink`, so the shared `log()` level filter gates it:
        // a record below the threshold never reaches `write_event`.
        let guard = LEVEL_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_clock();
        set_max_level(Level::Warn);
        let ring: BootAuditRing<8> = BootAuditRing::new(scripted_clock);
        assert!(!log(&ring, &event(1, Level::Info, "dropped")));
        assert!(log(&ring, &event(2, Level::Error, "kept")));
        set_max_level(Level::Info);
        drop(guard);

        assert_eq!(ring.len(), 1);
        assert_eq!(ring.record(0).expect("kept").id.0, 2);
    }

    // Serialises tests that touch the shared global level filter.
    static LEVEL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    use std::string::{String, ToString};
    use std::vec;
    use std::vec::Vec;
}
