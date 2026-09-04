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
use tairix_collections::{ArrayString, RingBuf};
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
/// `Copy` and self-contained (the message lives inline), so a viewer takes it
/// out from under the ring lock and renders it afterwards without holding the
/// lock. It is also the stored form: the ring keeps these records directly,
/// with no second layout to convert between.
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
    /// The message, truncated on a character boundary when stored so no input
    /// can leave a partial character behind.
    msg: ArrayString<TAIL_MESSAGE_MAX>,
}

impl TailRecord {
    /// The record's message.
    #[must_use]
    pub fn message(&self) -> &str {
        self.msg.as_str()
    }
}

/// The interior, lock-guarded ring state.
///
/// `next_seq` is the sequence the next pushed record will take, so it also
/// counts every record ever written, including those since evicted.
struct RingState<const N: usize> {
    ring: RingBuf<TailRecord, N>,
    next_seq: u64,
}

impl<const N: usize> RingState<N> {
    const fn new() -> Self {
        Self {
            ring: RingBuf::new(),
            next_seq: 0,
        }
    }

    /// Append one record, displacing the oldest when the ring is full.
    ///
    /// A zero-capacity ring stores nothing and still advances the sequence, so
    /// the total stays meaningful.
    fn push(&mut self, level: Level, id: EventId, monotonic: Duration64, message: &str) {
        self.ring.push_back_overwrite(TailRecord {
            seq: self.next_seq,
            level,
            id,
            monotonic,
            msg: ArrayString::from_str_truncating(message),
        });
        self.next_seq = self.next_seq.wrapping_add(1);
    }

    /// The `(oldest, newest)` retained sequence numbers, or `None` when empty.
    fn seq_range(&self) -> Option<(u64, u64)> {
        Some((self.ring.front()?.seq, self.ring.back()?.seq))
    }

    /// Copy out the record with sequence `seq`, or `None` if it is not (or no
    /// longer) retained.
    fn record(&self, seq: u64) -> Option<TailRecord> {
        // Sequences are assigned one per push, so a retained range is
        // contiguous and the offset from the oldest is the ring position.
        let offset = seq.checked_sub(self.seq_range()?.0)?;
        self.ring.get(usize::try_from(offset).ok()?).copied()
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
        self.state.lock().ring.len()
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

#[cfg(test)]
mod tests {
    use super::{BootAuditRing, TAIL_MESSAGE_MAX};
    use tairix_abi::Duration64;
    use tairix_log::{log, Event, EventId, Level, Sink};

    // A scripted monotonic clock: each read returns a strictly increasing
    // second so stored timestamps are distinguishable.
    //
    // Per *thread*, not per process: the harness runs these tests in parallel,
    // and several of them reset the sequence and then assert the exact instants
    // their own writes recorded. One shared counter let two tests interleave
    // their reads and hand each other the wrong seconds — the clock a test
    // scripts must therefore be its own. The stored value is plain (not atomic)
    // because a thread's counter is only ever read and written by that thread.
    std::thread_local! {
        static CLOCK_SECS: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
    }

    fn scripted_clock() -> Duration64 {
        let secs = CLOCK_SECS.with(|c| {
            let now = c.get();
            c.set(now + 1);
            now
        });
        Duration64::from_secs(i64::try_from(secs).expect("test seconds fit i64"))
    }

    fn reset_clock() {
        CLOCK_SECS.with(|c| c.set(0));
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

    /// Each thread scripts its own instants, so tests the harness runs in
    /// parallel cannot consume each other's seconds. With one shared counter
    /// every exact-instant assertion in this module was order-dependent: a
    /// sibling test's writes advanced the sequence between this test's reset and
    /// its own reads, and the recorded timestamps came back wrong.
    #[test]
    fn the_scripted_clock_is_independent_per_thread() {
        reset_clock();
        assert_eq!(scripted_clock(), Duration64::from_secs(0));
        let other = std::thread::spawn(|| {
            reset_clock();
            (scripted_clock(), scripted_clock())
        })
        .join()
        .expect("the scripted-clock thread runs to completion");
        assert_eq!(other, (Duration64::from_secs(0), Duration64::from_secs(1)));
        // Unaffected by the other thread's reads: this sequence resumes at 1.
        assert_eq!(scripted_clock(), Duration64::from_secs(1));
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
        // a record below the threshold never reaches `write_event`. The
        // shared level guard serialises this against every other test that
        // depends on the process-global threshold.
        reset_clock();
        let ring: BootAuditRing<8> = BootAuditRing::new(scripted_clock);
        crate::test_sink::with_log_level(Level::Warn, || {
            assert!(!log(&ring, &event(1, Level::Info, "dropped")));
            assert!(log(&ring, &event(2, Level::Error, "kept")));
        });

        assert_eq!(ring.len(), 1);
        assert_eq!(ring.record(0).expect("kept").id.0, 2);
    }

    use std::string::{String, ToString};
    use std::vec;
    use std::vec::Vec;
}
