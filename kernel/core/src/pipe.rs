//! The kernel pipe object: a bounded, unidirectional byte stream connecting
//! a read end and a write end (`plans/SPAWN.md` SP10 — the `cmd | cmd`
//! primitive).
//!
//! A [`Pipe`] is pure data behind a `SpinLock`: a bounded byte ring plus
//! live-end counts. It never itself parks or switches context — the syscall
//! handler drives the park loop (the [`crate::waitq`] discipline), calling
//! the non-blocking [`PipeEnd::try_read`] / [`PipeEnd::try_write`] steps and
//! parking on [`crate::waitq::PIPE_WAITQ`] when a step reports
//! [`ReadStep::Empty`] / [`WriteStep::Full`].
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
//! [`rustos_abi::Errno::BrokenPipe`].
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

use rustos_sync::SpinLock;

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
        for slot in out.iter_mut().take(n) {
            // The length guard above makes `pop_front` infallible for the
            // first `n` slots; fail closed (stop short) rather than panic
            // if the invariant were ever violated.
            match state.buf.pop_front() {
                Some(byte) => *slot = byte,
                None => break,
            }
        }
        ReadStep::Read(n)
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
        state.buf.extend(data.iter().take(n).copied());
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
        {
            let mut state = self.pipe.state.lock();
            match self.role {
                PipeRole::Read => state.readers = state.readers.saturating_sub(1),
                PipeRole::Write => state.writers = state.writers.saturating_sub(1),
            }
        }
        // The peer side's condition may just have changed terminally (a
        // reader must observe EOF, a writer broken-pipe), so wake the
        // parked waiters after the lock is released. A fail-safe no-op
        // before the wait arch is installed.
        crate::waitq::pipe_wake();
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
    fn equality_names_the_end_not_the_content() {
        let (read, write) = Pipe::create();
        let read2 = read.clone();
        assert_eq!(read, read2);
        assert_ne!(read, write);
        let (other_read, _other_write) = Pipe::create();
        assert_ne!(read, other_read);
    }
}
