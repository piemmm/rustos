//! The kernel pipe object: a bounded, unidirectional byte stream connecting
//! a read end and a write end (`plans/SPAWN.md` SP10 — the `cmd | cmd`
//! primitive).
//!
//! A [`Pipe`] is pure data behind a `SpinLock`: a bounded byte ring plus
//! live-end counts. It never itself parks or switches context — the syscall
//! handler drives the park loop (the [`crate::waitq`] discipline), calling
//! the non-blocking [`PipeEnd::try_read`] / [`PipeEnd::try_write`] steps and
//! parking on [`crate::waitq::STREAM_WAITQ`] when a step reports
//! [`ReadStep::Empty`] / [`WriteStep::Full`], under the [`RingWaits`] key of
//! the ring side it blocked on so a transfer here never unparks another
//! stream's waiters.
//!
//! # End lifetime
//!
//! Ends are counted through the [`PipeEnd`] handle itself: `Clone`
//! increments the side's live count (a spawn wiring a child onto an end),
//! `Drop` decrements it and wakes the peer's waiters. `fs_close`, a failed
//! spawn's unwind, and task exit (the registry dropping the open table) all
//! release ends through that one `Drop` path, so no count is ever leaked or
//! double-decremented. When the last write end drops, a blocked reader wakes
//! to end-of-stream; when the last read end drops, a blocked writer wakes to
//! [`tairix_abi::Errno::BrokenPipe`].
//!
//! # Capacity
//!
//! [`PIPE_CAPACITY`] is a deliberate flow-control bound, not a scaling
//! capacity: bounding the ring is what creates the back-pressure a pipeline
//! needs (a producer faster than its consumer must block, not balloon
//! kernel memory), exactly as the charter's sanctioned fixed defaults (the
//! random output reserve) are bounds by design.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use tairix_sync::SpinLock;

use crate::waitq::WakeKey;

/// The two wake identities one bounded byte ring carries: the tasks blocked on
/// its bytes (or on end-of-stream) and the tasks blocked on its free space (or
/// on the stream breaking). Minted per ring — a pipe has one, a pty has two —
/// so a wake names one side of one ring and nothing else.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct RingWaits {
    /// Released when bytes are appended, or the last producer closes.
    pub data: WakeKey,
    /// Released when bytes are drained, or the last consumer closes.
    pub space: WakeKey,
}

/// Source of ring identities. Monotonic, so a live ring never shares a key
/// with another.
static NEXT_RING_KEY: AtomicU64 = AtomicU64::new(1);

impl RingWaits {
    /// Mint a fresh pair of identities for one ring.
    #[must_use]
    pub fn mint() -> Self {
        Self {
            data: WakeKey::new(NEXT_RING_KEY.fetch_add(1, Ordering::Relaxed)),
            space: WakeKey::new(NEXT_RING_KEY.fetch_add(1, Ordering::Relaxed)),
        }
    }
}

/// The wake identities one blocking transfer uses: the ring side its caller
/// parks on while it cannot progress, and the ring sides its progress
/// releases.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct StreamWaits {
    park: WakeKey,
    /// [`WakeKey::NONE`] in the second slot means the transfer touches one
    /// ring; [`Self::crossing`] fills it.
    progress: [WakeKey; 2],
}

impl StreamWaits {
    /// A transfer that parks on `park` and whose progress releases
    /// `progress` — the ordinary one-ring read or write.
    #[must_use]
    pub const fn new(park: WakeKey, progress: WakeKey) -> Self {
        Self {
            park,
            progress: [progress, WakeKey::NONE],
        }
    }

    /// A transfer whose progress releases a second ring as well: an echoing
    /// pty read both frees input space and fills the output ring.
    #[must_use]
    pub const fn crossing(park: WakeKey, progress: WakeKey, other: WakeKey) -> Self {
        Self {
            park,
            progress: [progress, other],
        }
    }

    /// The ring side a caller that cannot progress registers and parks on.
    #[must_use]
    pub const fn park(self) -> WakeKey {
        self.park
    }

    /// Wake whatever a completed transfer released.
    pub fn wake_progress(self) {
        for key in self.progress {
            if key != WakeKey::NONE {
                crate::waitq::stream_wake(key);
            }
        }
    }
}

/// Byte capacity of one pipe's ring (64 KiB, the POSIX-conventional size).
///
/// A flow-control bound (see the module docs), not a growable capacity: a
/// writer that outruns its reader blocks once this many bytes are in
/// flight.
pub const PIPE_CAPACITY: usize = 64 * 1024;

/// Which side of the pipe a [`PipeEnd`] handle grants.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PipeRole {
    /// The reading side: drains bytes, observes end-of-stream.
    Read,
    /// The writing side: supplies bytes, observes broken-pipe.
    Write,
}

/// The shared pipe state: the bounded ring and the live-end counts.
struct PipeState {
    /// In-flight bytes, oldest first. Bounded by [`PIPE_CAPACITY`].
    buf: VecDeque<u8>,
    /// Live read-end handles. `0` means no reader can ever drain again.
    readers: usize,
    /// Live write-end handles. `0` means no byte can ever arrive again.
    writers: usize,
}

/// One kernel pipe: the object both [`PipeEnd`] handles share.
pub struct Pipe {
    state: SpinLock<PipeState>,
    /// This pipe's own wake identities, so a transfer wakes only its own
    /// blocked reader or writer.
    waits: RingWaits,
}

/// Outcome of one non-blocking read step.
#[derive(Debug, Eq, PartialEq)]
pub enum ReadStep {
    /// `n` bytes were copied out (`1..=out.len()`).
    Read(usize),
    /// The pipe is empty and every write end is closed: end-of-stream.
    Eof,
    /// The pipe is empty but a writer is still live: the caller parks and
    /// retries after a wake.
    Empty,
}

/// Outcome of one non-blocking write step.
#[derive(Debug, Eq, PartialEq)]
pub enum WriteStep {
    /// `n` bytes were accepted (`1..=data.len()`).
    Wrote(usize),
    /// Every read end is closed: the bytes can never be consumed.
    Broken,
    /// The ring is full but a reader is still live: the caller parks and
    /// retries after a wake.
    Full,
}

impl Pipe {
    /// Create a pipe and hand back its two initial end handles
    /// (`(read, write)`), each counting one live end.
    #[must_use]
    pub fn create() -> (PipeEnd, PipeEnd) {
        let pipe = Arc::new(Pipe {
            state: SpinLock::new(PipeState {
                buf: VecDeque::new(),
                readers: 1,
                writers: 1,
            }),
            waits: RingWaits::mint(),
        });
        (
            PipeEnd {
                pipe: Arc::clone(&pipe),
                role: PipeRole::Read,
            },
            PipeEnd {
                pipe,
                role: PipeRole::Write,
            },
        )
    }
}

/// An owned, counted handle on one side of a [`Pipe`].
///
/// Cloning registers one more live end of the same side; dropping releases
/// it and wakes the peer side's waiters (see the module docs). The handle
/// is deliberately the *only* way to change the counts, so they can never
/// drift from the set of live handles.
pub struct PipeEnd {
    pipe: Arc<Pipe>,
    role: PipeRole,
}

impl PipeEnd {
    /// Which side this handle grants.
    #[must_use]
    pub fn role(&self) -> PipeRole {
        self.role
    }

    /// `true` when `other` is a handle on the same underlying pipe.
    #[must_use]
    pub fn same_pipe(&self, other: &PipeEnd) -> bool {
        Arc::ptr_eq(&self.pipe, &other.pipe)
    }

    /// The wake identities a blocking transfer on this end uses: a reader
    /// parks on the ring's bytes and its drain frees space; a writer parks on
    /// the ring's space and its append supplies bytes.
    #[must_use]
    pub fn waits(&self) -> StreamWaits {
        let RingWaits { data, space } = self.pipe.waits;
        match self.role {
            PipeRole::Read => StreamWaits::new(data, space),
            PipeRole::Write => StreamWaits::new(space, data),
        }
    }

    /// The condition this side's **last** release retires: the readers'
    /// departure breaks the writers, the writers' ends the readers' stream.
    fn retired_condition(&self) -> WakeKey {
        let RingWaits { data, space } = self.pipe.waits;
        match self.role {
            PipeRole::Read => space,
            PipeRole::Write => data,
        }
    }

    /// One non-blocking read step: drain up to `out.len()` bytes.
    ///
    /// Returns [`ReadStep::Empty`] when the caller should park and retry,
    /// [`ReadStep::Eof`] at end-of-stream. A zero-length `out` reads zero
    /// bytes (`ReadStep::Read(0)` is never produced for a non-empty
    /// `out`). Calling on a write end drains nothing and reports `Empty`;
    /// the handler rejects the direction before reaching here (defence in
    /// depth: a mis-routed call starves rather than corrupts).
    #[must_use]
    pub fn try_read(&self, out: &mut [u8]) -> ReadStep {
        if self.role != PipeRole::Read {
            return ReadStep::Empty;
        }
        let mut state = self.pipe.state.lock();
        if state.buf.is_empty() {
            return if state.writers == 0 {
                ReadStep::Eof
            } else {
                ReadStep::Empty
            };
        }
        if out.is_empty() {
            return ReadStep::Read(0);
        }
        let n = out.len().min(state.buf.len());
        // Two bulk copies, one per ring segment: a byte-at-a-time drain
        // would make every megabyte a million bounds-checked pops on a
        // path that carries whole documents between processes.
        let (front, back) = state.buf.as_slices();
        let head = front.len().min(n);
        out[..head].copy_from_slice(&front[..head]);
        out[head..n].copy_from_slice(&back[..n - head]);
        state.buf.drain(..n);
        ReadStep::Read(n)
    }

    /// Whether a read on this end would complete without parking: buffered
    /// bytes are waiting, or every write end is closed (the read observes
    /// end-of-stream). A **non-consuming peek** — nothing is drained — so a
    /// wait-set scan can report readiness and leave the bytes for the woken
    /// owner's read (`plans/APPWIN.md` AW4). On a write end it is always
    /// `false` (a write end can never be read; fail closed, matching
    /// [`Self::try_read`]'s direction guard).
    #[must_use]
    pub fn readable(&self) -> bool {
        if self.role != PipeRole::Read {
            return false;
        }
        let state = self.pipe.state.lock();
        !state.buf.is_empty() || state.writers == 0
    }

    /// One non-blocking write step: append up to the ring's free space.
    ///
    /// Returns [`WriteStep::Full`] when the caller should park and retry,
    /// [`WriteStep::Broken`] when no read end remains. A zero-length
    /// `data` writes zero bytes. Calling on a read end appends nothing and
    /// reports `Full` (defence in depth, as for [`Self::try_read`]).
    #[must_use]
    pub fn try_write(&self, data: &[u8]) -> WriteStep {
        if self.role != PipeRole::Write {
            return WriteStep::Full;
        }
        let mut state = self.pipe.state.lock();
        if state.readers == 0 {
            return WriteStep::Broken;
        }
        if data.is_empty() {
            return WriteStep::Wrote(0);
        }
        let free = PIPE_CAPACITY.saturating_sub(state.buf.len());
        if free == 0 {
            return WriteStep::Full;
        }
        let n = data.len().min(free);
        // Grow the ring fallibly, then append in bulk. A kernel
        // allocation failure must never abort, so a refused reservation
        // accepts only what the ring already has room for and the writer
        // retries after the reader drains (or after reclaim runs).
        let spare = state.buf.capacity() - state.buf.len();
        let n = if n <= spare || state.buf.try_reserve_exact(n - spare).is_ok() {
            n
        } else {
            spare
        };
        if n == 0 {
            return WriteStep::Full;
        }
        state.buf.extend(&data[..n]);
        WriteStep::Wrote(n)
    }
}

impl Clone for PipeEnd {
    fn clone(&self) -> Self {
        {
            let mut state = self.pipe.state.lock();
            match self.role {
                PipeRole::Read => state.readers += 1,
                PipeRole::Write => state.writers += 1,
            }
        }
        Self {
            pipe: Arc::clone(&self.pipe),
            role: self.role,
        }
    }
}

impl Drop for PipeEnd {
    fn drop(&mut self) {
        let last = {
            let mut state = self.pipe.state.lock();
            match self.role {
                PipeRole::Read => {
                    state.readers = state.readers.saturating_sub(1);
                    state.readers == 0
                }
                PipeRole::Write => {
                    state.writers = state.writers.saturating_sub(1);
                    state.writers == 0
                }
            }
        };
        // Only the *last* end of a side changes the peer's condition (a reader
        // must then observe EOF, a writer broken-pipe), so a clone released
        // while siblings live wakes nobody. Woken after the lock is released;
        // a fail-safe no-op before the wait arch is installed.
        if last {
            crate::waitq::stream_wake(self.retired_condition());
        }
    }
}

impl core::fmt::Debug for PipeEnd {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The shared pipe object has no meaningful textual form (a byte
        // ring behind a lock); the end's identity for diagnostics is its
        // role, marked non-exhaustive to say a field is elided.
        f.debug_struct("PipeEnd")
            .field("role", &self.role)
            .finish_non_exhaustive()
    }
}

/// Two handles are equal when they grant the same side of the same pipe.
/// The in-flight bytes are deliberately not part of equality — a handle's
/// identity is *which end it is*, not the stream's momentary content.
impl PartialEq for PipeEnd {
    fn eq(&self, other: &Self) -> bool {
        self.same_pipe(other) && self.role == other.role
    }
}

impl Eq for PipeEnd {}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn bytes_flow_in_order_through_the_ring() {
        let (read, write) = Pipe::create();
        assert_eq!(write.try_write(b"hello "), WriteStep::Wrote(6));
        assert_eq!(write.try_write(b"world"), WriteStep::Wrote(5));
        let mut out = [0u8; 16];
        assert_eq!(read.try_read(&mut out), ReadStep::Read(11));
        assert_eq!(&out[..11], b"hello world");
        // Drained: a live writer means the reader parks, not EOF.
        assert_eq!(read.try_read(&mut out), ReadStep::Empty);
    }

    #[test]
    fn a_full_ring_reports_full_and_frees_after_a_drain() {
        let (read, write) = Pipe::create();
        let chunk = vec![7u8; PIPE_CAPACITY];
        assert_eq!(write.try_write(&chunk), WriteStep::Wrote(PIPE_CAPACITY));
        // No space left: the writer must park.
        assert_eq!(write.try_write(b"x"), WriteStep::Full);
        // A partial write accepts exactly the free space.
        let mut out = vec![0u8; 100];
        assert_eq!(read.try_read(&mut out), ReadStep::Read(100));
        assert_eq!(write.try_write(&chunk), WriteStep::Wrote(100));
    }

    #[test]
    fn a_read_spanning_the_ring_wrap_returns_exactly_the_written_bytes() {
        // Transfers are bulk copies, one per ring segment, so the seam a
        // wrapped ring puts in the middle of a read is the case to pin:
        // the reader must see one unbroken stream across it.
        let (read, write) = Pipe::create();
        let filler = vec![0xEEu8; PIPE_CAPACITY];
        assert_eq!(write.try_write(&filler), WriteStep::Wrote(PIPE_CAPACITY));
        // Free the head, then refill it: the live bytes now straddle the
        // ring's end and its start.
        let mut drained = vec![0u8; PIPE_CAPACITY / 2];
        assert_eq!(
            read.try_read(&mut drained),
            ReadStep::Read(PIPE_CAPACITY / 2)
        );
        let tail: alloc::vec::Vec<u8> = (0..PIPE_CAPACITY / 2)
            .map(|i| u8::try_from(i % 251).expect("bounded by the modulus"))
            .collect();
        assert_eq!(write.try_write(&tail), WriteStep::Wrote(tail.len()));

        let mut out = vec![0u8; PIPE_CAPACITY];
        assert_eq!(read.try_read(&mut out), ReadStep::Read(PIPE_CAPACITY));
        assert!(
            out[..PIPE_CAPACITY / 2].iter().all(|&b| b == 0xEE),
            "the bytes before the seam are unchanged"
        );
        assert_eq!(&out[PIPE_CAPACITY / 2..], &tail[..], "and those after it");
        assert_eq!(read.try_read(&mut out), ReadStep::Empty, "nothing is left");
    }

    #[test]
    fn dropping_the_last_writer_yields_eof_after_the_drain() {
        let (read, write) = Pipe::create();
        assert_eq!(write.try_write(b"tail"), WriteStep::Wrote(4));
        drop(write);
        // Buffered bytes are still readable after the writer is gone…
        let mut out = [0u8; 8];
        assert_eq!(read.try_read(&mut out), ReadStep::Read(4));
        // …and only then does the stream end.
        assert_eq!(read.try_read(&mut out), ReadStep::Eof);
    }

    #[test]
    fn dropping_the_last_reader_breaks_the_writer() {
        let (read, write) = Pipe::create();
        drop(read);
        assert_eq!(write.try_write(b"x"), WriteStep::Broken);
    }

    #[test]
    fn cloned_ends_keep_their_side_alive_until_the_last_drop() {
        let (read, write) = Pipe::create();
        let write2 = write.clone();
        drop(write);
        // One write end still lives: the empty pipe means park, not EOF.
        let mut out = [0u8; 4];
        assert_eq!(read.try_read(&mut out), ReadStep::Empty);
        drop(write2);
        assert_eq!(read.try_read(&mut out), ReadStep::Eof);
        // The read side mirrors it.
        let (read_a, write_b) = Pipe::create();
        let read_b = read_a.clone();
        drop(read_a);
        assert_eq!(write_b.try_write(b"y"), WriteStep::Wrote(1));
        drop(read_b);
        assert_eq!(write_b.try_write(b"y"), WriteStep::Broken);
    }

    #[test]
    fn direction_is_enforced_at_the_step_level() {
        let (read, write) = Pipe::create();
        // A mis-routed step starves fail-closed rather than crossing the
        // direction: the handler rejects it before ever parking.
        assert_eq!(write.try_read(&mut [0u8; 4]), ReadStep::Empty);
        assert_eq!(read.try_write(b"z"), WriteStep::Full);
    }

    #[test]
    fn zero_length_transfers_are_inert() {
        let (read, write) = Pipe::create();
        assert_eq!(write.try_write(b""), WriteStep::Wrote(0));
        assert_eq!(write.try_write(b"a"), WriteStep::Wrote(1));
        assert_eq!(read.try_read(&mut []), ReadStep::Read(0));
        let mut out = [0u8; 1];
        assert_eq!(read.try_read(&mut out), ReadStep::Read(1));
        assert_eq!(out[0], b'a');
    }

    #[test]
    fn readable_peeks_without_consuming_and_reports_eof() {
        let (read, write) = Pipe::create();
        // Empty with a live writer: a read would park, so not readable.
        assert!(!read.readable());
        assert_eq!(write.try_write(b"hi"), WriteStep::Wrote(2));
        // Buffered bytes: readable, and the peek consumed nothing.
        assert!(read.readable());
        assert!(read.readable());
        let mut out = [0u8; 4];
        assert_eq!(read.try_read(&mut out), ReadStep::Read(2));
        assert!(!read.readable());
        // Every write end closed: readable (the read observes EOF).
        drop(write);
        assert!(read.readable());
        assert_eq!(read.try_read(&mut out), ReadStep::Eof);
        // A write end is never readable, buffered bytes or not.
        let (read_b, write_b) = Pipe::create();
        assert_eq!(write_b.try_write(b"x"), WriteStep::Wrote(1));
        assert!(!write_b.readable());
        assert!(read_b.readable());
    }

    /// Each pipe carries its own ring identities and its two ends mirror each
    /// other across them, which is what confines a wake to the pipe that
    /// produced it.
    #[test]
    fn each_pipe_waits_on_its_own_ring_and_the_ends_mirror_each_other() {
        let (read, write) = Pipe::create();
        let (r, w) = (read.waits(), write.waits());
        assert_ne!(r.park(), w.park(), "the two directions block separately");
        assert_eq!(
            r,
            StreamWaits::new(r.park(), w.park()),
            "a drain frees the space its writer parks on"
        );
        assert_eq!(
            w,
            StreamWaits::new(w.park(), r.park()),
            "an append supplies the bytes its reader parks on"
        );

        let (read2, write2) = Pipe::create();
        let keys = [
            r.park(),
            w.park(),
            read2.waits().park(),
            write2.waits().park(),
        ];
        for (i, key) in keys.iter().enumerate() {
            assert!(
                !keys[..i].contains(key),
                "two pipes must not share a wake identity"
            );
        }

        // A clone shares its pipe's identities; it is the same ring.
        assert_eq!(read.clone().waits(), r);
    }

    /// A departing side must wake exactly the condition its departure makes
    /// terminal, or the peer parks on an EOF or broken pipe it never hears
    /// about.
    #[test]
    fn the_last_end_of_a_side_retires_exactly_its_peers_condition() {
        let (read, write) = Pipe::create();
        assert_eq!(
            read.retired_condition(),
            write.waits().park(),
            "the last reader breaks the writers waiting for its space"
        );
        assert_eq!(
            write.retired_condition(),
            read.waits().park(),
            "the last writer ends the stream the readers wait on"
        );
    }

    #[test]
    fn equality_names_the_end_not_the_content() {
        let (read, write) = Pipe::create();
        let read2 = read.clone();
        assert_eq!(read, read2);
        assert_ne!(read, write);
        let (other_read, _other_write) = Pipe::create();
        assert_ne!(read, other_read);
    }
}
