//! The kernel pseudo-terminal (PTY) object: one terminal joining a **master**
//! end (held by a terminal emulator) and a **slave** end (wired as a shell's
//! standard streams), with a full console-class line discipline layered over
//! two byte rings (`plans/PTY.md` PTY2).
//!
//! A [`Pty`] is pure data behind a `SpinLock`, mirroring [`crate::pipe::Pipe`]:
//! it never itself parks or switches context — the syscall handler drives the
//! park loop (the [`crate::waitq`] discipline), calling the non-blocking
//! [`PtyMasterEnd`] / [`PtySlaveEnd`] steps and parking on
//! [`crate::waitq::PIPE_WAITQ`] (the shared stream wait-queue) when a step
//! reports `Empty` / `Full`.
//!
//! ```text
//! terminal (master)                                   shell (slave)
//!    write ───────────►  input ring  ──[cook + signal]──►  slave read
//!    read  ◄───────────  output ring ◄─[ONLCR]────────────  slave write
//!                                     ◄─[echo]──────────────  (echo of input)
//! ```
//!
//! # The shared line discipline (`AGENTS.md` §2.2)
//!
//! The cooking is **not** re-implemented here: input local echo, the `ONLCR`
//! output translation, and the cooked-mode `^C`/`^Z` classification are the
//! shared [`tairix_tty`] discipline the kernel console also runs, and the
//! foreground ownership is the shared [`crate::foreground::ForegroundOwnership`]
//! the console also uses. The pty is the *assembly* of those over two rings.
//!
//! - **Master write** (terminal keystrokes) is the input side: in cooked mode
//!   with a foreground job, `^C`/`^Z` are consumed and reported as signals for
//!   that job; every other byte is buffered for the slave to read.
//! - **Slave read** (the shell reading stdin) drains the input ring and, in the
//!   cooked (echoing) mode, echoes the consumed bytes onto the output ring —
//!   exactly as the console echoes at read time.
//! - **Slave write** (program/prompt output) is cooked (`ONLCR`) onto the
//!   output ring.
//! - **Master read** (the terminal rendering) drains the output ring raw.
//!
//! # Capacity
//!
//! Each ring is bounded by [`crate::pipe::PIPE_CAPACITY`], the same deliberate
//! flow-control bound a pipe uses (`plans/PTY.md`, `AGENTS.md` §24.4): bounding
//! the ring is what creates back-pressure, not a scaling capacity. Echo onto a
//! full output ring is dropped best-effort (echo is cosmetic), never blocking
//! the read the user asked for.
//!
//! # End lifetime
//!
//! Ends are counted through the [`PtyMasterEnd`] / [`PtySlaveEnd`] handles, as
//! [`crate::pipe::PipeEnd`] does: `Clone` increments the side's live count,
//! `Drop` decrements it and wakes the peer's waiters. When the last slave end
//! drops, a master read observes end-of-stream and a master write breaks; when
//! the last master end drops, a slave read observes end-of-stream and a slave
//! write breaks.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;

use tairix_abi::{InputMode, Signal, TerminalSize};
use tairix_kernel_sec::ProcessId;
use tairix_sync::SpinLock;

use crate::foreground::ForegroundOwnership;
use crate::pipe::PIPE_CAPACITY;

/// Which end of the pty a handle grants.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PtyRole {
    /// The master end: held by the terminal emulator. Writes drive the input
    /// discipline; reads drain the (cooked) program output.
    Master,
    /// The slave end: wired as the shell's fd 0/1/2. Reads drain the input;
    /// writes are cooked (`ONLCR`) onto the output.
    Slave,
}

/// The shared pty state: the two bounded rings, the live-end counts, and the
/// line-discipline state.
struct PtyState {
    /// Terminal → shell bytes (what the slave reads). Bounded by
    /// [`PIPE_CAPACITY`].
    input: VecDeque<u8>,
    /// Shell → terminal bytes (what the master reads): cooked program output
    /// and echoed input. Bounded by [`PIPE_CAPACITY`].
    output: VecDeque<u8>,
    /// Live master-end handles. `0` means the shell can never be read again and
    /// its output can never be consumed.
    masters: usize,
    /// Live slave-end handles. `0` means no byte can ever be read by a shell and
    /// no program output can ever arrive.
    slaves: usize,
    /// The current read line discipline (`stream_input_mode`): cooked echoes and
    /// intercepts `^C`/`^Z`; raw/secret pass every byte through with echo off.
    mode: InputMode,
    /// The input local-echo state (the shared discipline), carried across the
    /// many slave reads one edited line spans.
    echo: tairix_tty::EchoLine,
    /// The terminal's character-cell geometry (`terminal_size`), set by the
    /// master at create and on resize.
    size: TerminalSize,
}

/// One kernel pseudo-terminal: the object both end handles share.
pub struct Pty {
    state: SpinLock<PtyState>,
    /// The controlling (foreground) ownership: the target the cooked-mode
    /// `^C`/`^Z` interception delivers to, and the task allowed to drain the
    /// slave and change its discipline. The shared type the console also uses.
    fg: ForegroundOwnership,
}

/// Outcome of one non-blocking read step (master output drain or slave input
/// drain).
#[derive(Debug, Eq, PartialEq)]
pub enum PtyReadStep {
    /// `n` bytes were copied out (`1..=out.len()`).
    Read(usize),
    /// The ring is empty and the peer side is fully closed: end-of-stream.
    Eof,
    /// The ring is empty but the peer is still live: the caller parks and
    /// retries after a wake.
    Empty,
}

/// Outcome of one non-blocking slave write step (cooked program output).
#[derive(Debug, Eq, PartialEq)]
pub enum PtyWriteStep {
    /// `n` **input** bytes were accepted (`1..=data.len()`); a cooked line feed
    /// expands to two output bytes but still counts as one input byte, so the
    /// caller loops on the returned short count (the POSIX short-write
    /// contract).
    Wrote(usize),
    /// Every master end is closed: the output can never be consumed.
    Broken,
    /// The output ring is full but a master is still live: the caller parks and
    /// retries after a wake.
    Full,
}

/// Outcome of one non-blocking master write step (the input discipline).
#[derive(Debug, Eq, PartialEq)]
pub enum MasterWriteStep {
    /// `consumed` input bytes were accepted, and `signals` is the (possibly
    /// empty) list of cooked-mode job-control signals to deliver to the slave's
    /// foreground task. An empty `signals` allocates nothing.
    Wrote {
        /// Number of input bytes consumed (buffered, or turned into a signal).
        consumed: usize,
        /// `(foreground task, signal)` pairs the caller must deliver.
        signals: Vec<(ProcessId, Signal)>,
    },
    /// Every slave end is closed: the bytes can never be read.
    Broken,
    /// The input ring is full but a slave is still live: the caller parks and
    /// retries after a wake.
    Full,
}

impl Pty {
    /// Create a pty of the given terminal geometry and hand back its two end
    /// handles (`(master, slave)`), each counting one live end. The initial
    /// discipline is the interactive [`InputMode::Cooked`] default.
    #[must_use]
    pub fn create(size: TerminalSize) -> (PtyMasterEnd, PtySlaveEnd) {
        let pty = Arc::new(Pty {
            state: SpinLock::new(PtyState {
                input: VecDeque::new(),
                output: VecDeque::new(),
                masters: 1,
                slaves: 1,
                mode: InputMode::Cooked,
                echo: tairix_tty::EchoLine::new(),
                size,
            }),
            fg: ForegroundOwnership::new(),
        });
        (
            PtyMasterEnd {
                pty: Arc::clone(&pty),
            },
            PtySlaveEnd { pty },
        )
    }

    /// Select the slave read line discipline (`stream_input_mode`), resetting
    /// the echo state to a fresh line (a mode change starts a new edited line,
    /// so a later erase never rubs out into the previous one).
    pub fn set_input_mode(&self, mode: InputMode) {
        let mut state = self.state.lock();
        state.mode = mode;
        state.echo.reset();
    }

    /// The currently selected slave read line discipline.
    #[must_use]
    pub fn input_mode(&self) -> InputMode {
        self.state.lock().mode
    }

    /// Discard everything a finished session left in this pty, so none of it
    /// reaches whoever uses the terminal next (`terminal_purge`): the
    /// keystrokes the terminal typed but the program never read, the program
    /// output the terminal never drew, and the line-discipline state, which
    /// returns to the interactive cooked default.
    ///
    /// The queued bytes are zeroed before they are dropped — a credential
    /// typed into a terminal transits these rings exactly as it transits the
    /// console's type-ahead queue.
    ///
    /// The live-end counts, the geometry, and the controlling ownership are
    /// deliberately untouched: the pty outlives the session running on it, and
    /// releasing the ownership would let a task that never held the terminal
    /// take its control. Freeing ring space can unblock a parked writer, so
    /// the caller wakes the waiters exactly as a drain does.
    pub fn purge_session(&self) {
        let mut state = self.state.lock();
        let PtyState {
            input,
            output,
            mode,
            echo,
            ..
        } = &mut *state;
        for byte in input.iter_mut().chain(output.iter_mut()) {
            *byte = 0;
        }
        input.clear();
        output.clear();
        *mode = InputMode::Cooked;
        echo.reset();
    }

    /// The pty's character-cell geometry (`terminal_size`). Always known — a
    /// pty's size is set by its master, unlike a UART whose remote size the
    /// kernel cannot attest.
    #[must_use]
    pub fn geometry(&self) -> TerminalSize {
        self.state.lock().size
    }

    /// Set the pty's character-cell geometry (the master on create/resize).
    pub fn set_size(&self, size: TerminalSize) {
        self.state.lock().size = size;
    }

    /// The pty's controlling (foreground) ownership, for the shared
    /// `console_foreground` transitions and the input filter's target.
    #[must_use]
    pub fn foreground(&self) -> &ForegroundOwnership {
        &self.fg
    }
}

/// An owned, counted handle on the master end of a [`Pty`].
pub struct PtyMasterEnd {
    pty: Arc<Pty>,
}

/// An owned, counted handle on the slave end of a [`Pty`].
pub struct PtySlaveEnd {
    pty: Arc<Pty>,
}

impl PtyMasterEnd {
    /// This handle's role (always [`PtyRole::Master`]).
    #[must_use]
    pub fn role(&self) -> PtyRole {
        PtyRole::Master
    }

    /// `true` when `other` is a handle on the same underlying pty.
    #[must_use]
    pub fn same_pty(&self, other: &PtyMasterEnd) -> bool {
        Arc::ptr_eq(&self.pty, &other.pty)
    }

    /// The pty this handle belongs to (for the discipline-control calls that a
    /// pty-slave-aware `stream_input_mode` / `terminal_size` /
    /// `console_foreground` route through).
    #[must_use]
    pub fn pty(&self) -> &Pty {
        &self.pty
    }

    /// One non-blocking master write step: push terminal keystrokes through the
    /// input discipline.
    ///
    /// In cooked mode with a foreground job set and `intercept` true, `^C`/`^Z`
    /// are consumed and reported as signals for that job rather than buffered;
    /// every other byte is appended to the input ring (up to its free space).
    /// `intercept` is the caller's "signal delivery is installed" gate: when
    /// false, a job-control byte is buffered like any other rather than swallowed
    /// (no byte is ever consumed for a signal no one will deliver).
    #[must_use]
    pub fn write(&self, data: &[u8], intercept: bool) -> MasterWriteStep {
        let mut state = self.pty.state.lock();
        if state.slaves == 0 {
            return MasterWriteStep::Broken;
        }
        if data.is_empty() {
            return MasterWriteStep::Wrote {
                consumed: 0,
                signals: Vec::new(),
            };
        }
        let target = if intercept && state.mode == InputMode::Cooked {
            self.pty.fg.current()
        } else {
            None
        };
        let mut consumed = 0usize;
        let mut signals: Vec<(ProcessId, Signal)> = Vec::new();
        for &byte in data {
            if let Some(owner) = target {
                if let Some(signal) = tairix_tty::job_control_signal(byte) {
                    signals.push((owner, signal));
                    consumed += 1;
                    continue;
                }
            }
            if state.input.len() >= PIPE_CAPACITY {
                break;
            }
            state.input.push_back(byte);
            consumed += 1;
        }
        if consumed == 0 {
            // Nothing fit and nothing was a signal: the ring is full, park.
            return MasterWriteStep::Full;
        }
        MasterWriteStep::Wrote { consumed, signals }
    }

    /// One non-blocking master read step: drain the (cooked) program output.
    #[must_use]
    pub fn read(&self, out: &mut [u8]) -> PtyReadStep {
        let mut state = self.pty.state.lock();
        let slaves = state.slaves;
        drain(&mut state.output, slaves, out, DrainBound::Available)
    }

    /// Whether a master read would complete without parking: buffered output,
    /// or every slave end closed (end-of-stream). A non-consuming peek.
    #[must_use]
    pub fn readable(&self) -> bool {
        let state = self.pty.state.lock();
        !state.output.is_empty() || state.slaves == 0
    }
}

impl PtySlaveEnd {
    /// This handle's role (always [`PtyRole::Slave`]).
    #[must_use]
    pub fn role(&self) -> PtyRole {
        PtyRole::Slave
    }

    /// `true` when `other` is a handle on the same underlying pty.
    #[must_use]
    pub fn same_pty(&self, other: &PtySlaveEnd) -> bool {
        Arc::ptr_eq(&self.pty, &other.pty)
    }

    /// The pty this handle belongs to (for the discipline-control calls that a
    /// pty-slave-aware `stream_input_mode` / `terminal_size` /
    /// `console_foreground` route through).
    #[must_use]
    pub fn pty(&self) -> &Pty {
        &self.pty
    }

    /// One non-blocking slave read step: drain **at most one line** of the
    /// input ring into `out`, then, in the cooked (echoing) mode, echo the
    /// consumed bytes onto the output ring (best-effort — a full output ring
    /// drops the echo rather than failing the read).
    ///
    /// The slave end is a terminal, so its input carries the same read bound
    /// the console's type-ahead queue applies
    /// ([`tairix_tty::read_bounded`]): a shell reading its prompt cannot take
    /// the keystrokes typed ahead for the child it is about to run.
    #[must_use]
    pub fn read(&self, out: &mut [u8]) -> PtyReadStep {
        let mut state = self.pty.state.lock();
        let masters = state.masters;
        let step = drain(&mut state.input, masters, out, DrainBound::Line);
        if let PtyReadStep::Read(n) = step {
            if state.mode.echoes() && n > 0 {
                let PtyState {
                    ref mut echo,
                    ref mut output,
                    ..
                } = *state;
                echo.echo(&out[..n], |echoed| push_bounded(output, echoed));
            }
        }
        step
    }

    /// One non-blocking slave write step: cook program `data` (`ONLCR`) onto the
    /// output ring.
    #[must_use]
    pub fn write(&self, data: &[u8]) -> PtyWriteStep {
        let mut state = self.pty.state.lock();
        if state.masters == 0 {
            return PtyWriteStep::Broken;
        }
        if data.is_empty() {
            return PtyWriteStep::Wrote(0);
        }
        let output = &mut state.output;
        // `write_cooked` never errors here (the ring sink is infallible); it
        // reports `Ok(0)` when the ring is full.
        let consumed = tairix_tty::write_cooked(data, |chunk| {
            let free = PIPE_CAPACITY.saturating_sub(output.len());
            let n = chunk.len().min(free);
            output.extend(chunk.iter().take(n).copied());
            Ok(n)
        })
        .unwrap_or(0);
        if consumed == 0 {
            return PtyWriteStep::Full;
        }
        PtyWriteStep::Wrote(consumed)
    }

    /// Whether a slave read would complete without parking: buffered input, or
    /// every master end closed (end-of-stream). A non-consuming peek.
    #[must_use]
    pub fn readable(&self) -> bool {
        let state = self.pty.state.lock();
        !state.input.is_empty() || state.masters == 0
    }
}

/// How much of a ring one read step may take: the two directions of a pty are
/// bounded differently, and each call site says which it is.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum DrainBound {
    /// At most one line — the terminal-input bound the slave side reads under
    /// ([`tairix_tty::read_bounded`]): keystrokes queued behind the line the
    /// reader asked for stay in the pty for whoever reads next, which for a
    /// shell running a foreground child is a different process.
    Line,
    /// Everything queued that fits. Program output on its way to the terminal
    /// is a byte stream, not terminal input: it has no reader to protect it
    /// from and no line boundary worth stopping at.
    Available,
}

/// Drain from `ring` into `out` under `bound`, reporting end-of-stream when the
/// ring is empty and `peers` (the count of live peer ends that could still
/// produce) is zero. Shared by both read directions.
fn drain(ring: &mut VecDeque<u8>, peers: usize, out: &mut [u8], bound: DrainBound) -> PtyReadStep {
    if ring.is_empty() {
        return if peers == 0 {
            PtyReadStep::Eof
        } else {
            PtyReadStep::Empty
        };
    }
    if out.is_empty() {
        return PtyReadStep::Read(0);
    }
    let n = match bound {
        DrainBound::Line => tairix_tty::read_bounded(out, || ring.pop_front()),
        DrainBound::Available => {
            let n = out.len().min(ring.len());
            for slot in out.iter_mut().take(n) {
                match ring.pop_front() {
                    Some(byte) => *slot = byte,
                    None => break,
                }
            }
            n
        }
    };
    PtyReadStep::Read(n)
}

/// Append as many of `bytes` as fit under [`PIPE_CAPACITY`] to `ring`, dropping
/// the overflow. Best-effort, for the cosmetic echo path.
fn push_bounded(ring: &mut VecDeque<u8>, bytes: &[u8]) {
    let free = PIPE_CAPACITY.saturating_sub(ring.len());
    for &byte in bytes.iter().take(free) {
        ring.push_back(byte);
    }
}

impl Clone for PtyMasterEnd {
    fn clone(&self) -> Self {
        self.pty.state.lock().masters += 1;
        Self {
            pty: Arc::clone(&self.pty),
        }
    }
}

impl Clone for PtySlaveEnd {
    fn clone(&self) -> Self {
        self.pty.state.lock().slaves += 1;
        Self {
            pty: Arc::clone(&self.pty),
        }
    }
}

impl Drop for PtyMasterEnd {
    fn drop(&mut self) {
        {
            let mut state = self.pty.state.lock();
            state.masters = state.masters.saturating_sub(1);
        }
        // A slave reader must observe EOF and a slave writer broken-pipe once
        // the last master is gone; wake the shared stream waiters.
        crate::waitq::pipe_wake();
    }
}

impl Drop for PtySlaveEnd {
    fn drop(&mut self) {
        {
            let mut state = self.pty.state.lock();
            state.slaves = state.slaves.saturating_sub(1);
        }
        crate::waitq::pipe_wake();
    }
}

impl core::fmt::Debug for PtyMasterEnd {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PtyMasterEnd").finish_non_exhaustive()
    }
}

impl core::fmt::Debug for PtySlaveEnd {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PtySlaveEnd").finish_non_exhaustive()
    }
}

impl PartialEq for PtyMasterEnd {
    fn eq(&self, other: &Self) -> bool {
        self.same_pty(other)
    }
}

impl Eq for PtyMasterEnd {}

impl PartialEq for PtySlaveEnd {
    fn eq(&self, other: &Self) -> bool {
        self.same_pty(other)
    }
}

impl Eq for PtySlaveEnd {}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn pty() -> (PtyMasterEnd, PtySlaveEnd) {
        Pty::create(TerminalSize::new(24, 80).unwrap())
    }

    fn wrote(step: MasterWriteStep) -> (usize, Vec<(ProcessId, Signal)>) {
        match step {
            MasterWriteStep::Wrote { consumed, signals } => (consumed, signals),
            other => panic!("expected Wrote, got {other:?}"),
        }
    }

    #[test]
    fn raw_input_flows_master_to_slave_in_order_without_echo() {
        let (m, s) = pty();
        m.pty().set_input_mode(InputMode::Raw);
        assert_eq!(wrote(m.write(b"hello", true)).0, 5);
        let mut out = [0u8; 16];
        assert_eq!(s.read(&mut out), PtyReadStep::Read(5));
        assert_eq!(&out[..5], b"hello");
        // Raw mode does not echo, so the master sees no output.
        assert_eq!(m.read(&mut out), PtyReadStep::Empty);
        // Drained with a live master: the slave parks, not EOF.
        assert_eq!(s.read(&mut out), PtyReadStep::Empty);
    }

    #[test]
    fn a_slave_read_stops_at_the_end_of_a_line() {
        let (m, s) = pty();
        m.pty().set_input_mode(InputMode::Raw);
        // Two commands typed at the terminal before the shell read either.
        assert_eq!(wrote(m.write(b"sleep 3600\ntrue\n", true)).0, 16);
        let mut out = [0u8; 64];
        // The shell is handed the first line only: the second stays in the
        // pty, so the keystrokes survive the shell running the foreground
        // job the first line asked for.
        assert_eq!(s.read(&mut out), PtyReadStep::Read(11));
        assert_eq!(&out[..11], b"sleep 3600\n");
        assert_eq!(s.read(&mut out), PtyReadStep::Read(5));
        assert_eq!(&out[..5], b"true\n");
        assert_eq!(s.read(&mut out), PtyReadStep::Empty);
    }

    /// The session boundary of a pty: everything a finished session left in
    /// it goes — the keystrokes the terminal typed but the program never
    /// read, the output the terminal never drew, and the discipline the
    /// session selected — while the pty itself survives for the next session.
    #[test]
    fn purging_a_session_empties_both_rings_and_restores_the_discipline() {
        let (m, s) = pty();
        m.pty().set_input_mode(InputMode::Raw);
        assert_eq!(wrote(m.write(b"typed ahead", true)).0, 11);
        assert_eq!(s.write(b"program output"), PtyWriteStep::Wrote(14));

        m.pty().purge_session();

        let mut out = [0u8; 64];
        // Neither end sees the ended session's bytes, and neither reports
        // end-of-file: both ends are still live.
        assert_eq!(s.read(&mut out), PtyReadStep::Empty);
        assert_eq!(m.read(&mut out), PtyReadStep::Empty);
        assert_eq!(m.pty().input_mode(), InputMode::Cooked);
        assert_eq!(m.pty().geometry(), TerminalSize::new(24, 80).unwrap());

        // The emptied pty carries the next session as before: the cooked
        // discipline releases the line at the carriage return and echoes it
        // back to the terminal.
        assert_eq!(wrote(m.write(b"next\r", true)).0, 5);
        assert_eq!(s.read(&mut out), PtyReadStep::Read(5));
        assert_eq!(&out[..5], b"next\r");
        assert_eq!(m.read(&mut out), PtyReadStep::Read(6));
        assert_eq!(&out[..6], b"next\r\n");
    }

    #[test]
    fn a_master_read_is_not_bounded_by_program_output_lines() {
        let (m, s) = pty();
        // Program output is a byte stream on its way to the terminal, not
        // terminal input: the terminal drains every buffered line at once.
        assert_eq!(s.write(b"one\ntwo\n"), PtyWriteStep::Wrote(8));
        let mut out = [0u8; 64];
        // `ONLCR` expands each bare line feed to CR LF.
        assert_eq!(m.read(&mut out), PtyReadStep::Read(10));
        assert_eq!(&out[..10], b"one\r\ntwo\r\n");
    }

    #[test]
    fn a_full_input_ring_reports_full_and_frees_after_a_drain() {
        let (m, s) = pty();
        m.pty().set_input_mode(InputMode::Raw);
        let chunk = vec![7u8; PIPE_CAPACITY];
        assert_eq!(wrote(m.write(&chunk, true)).0, PIPE_CAPACITY);
        assert_eq!(m.write(b"x", true), MasterWriteStep::Full);
        let mut out = vec![0u8; 100];
        assert_eq!(s.read(&mut out), PtyReadStep::Read(100));
        assert_eq!(wrote(m.write(&chunk, true)).0, 100);
    }

    #[test]
    fn a_full_output_ring_reports_full_and_frees_after_a_drain() {
        let (m, s) = pty();
        let chunk = vec![b'a'; PIPE_CAPACITY];
        assert_eq!(s.write(&chunk), PtyWriteStep::Wrote(PIPE_CAPACITY));
        assert_eq!(s.write(b"x"), PtyWriteStep::Full);
        let mut out = vec![0u8; 100];
        assert_eq!(m.read(&mut out), PtyReadStep::Read(100));
        assert_eq!(s.write(&chunk), PtyWriteStep::Wrote(100));
    }

    #[test]
    fn dropping_the_last_master_yields_eof_on_the_slave_after_the_drain() {
        let (m, s) = pty();
        m.pty().set_input_mode(InputMode::Raw);
        assert_eq!(wrote(m.write(b"tail", true)).0, 4);
        drop(m);
        let mut out = [0u8; 8];
        assert_eq!(s.read(&mut out), PtyReadStep::Read(4));
        assert_eq!(&out[..4], b"tail");
        assert_eq!(s.read(&mut out), PtyReadStep::Eof);
        // A slave write with no master is broken.
        assert_eq!(s.write(b"x"), PtyWriteStep::Broken);
    }

    #[test]
    fn dropping_the_last_slave_breaks_the_master_and_ends_its_read() {
        let (m, s) = pty();
        assert_eq!(s.write(b"out"), PtyWriteStep::Wrote(3));
        drop(s);
        // Buffered output is still readable after the slave is gone…
        let mut out = [0u8; 8];
        assert_eq!(m.read(&mut out), PtyReadStep::Read(3));
        assert_eq!(&out[..3], b"out");
        // …then the stream ends, and a master write breaks.
        assert_eq!(m.read(&mut out), PtyReadStep::Eof);
        assert_eq!(m.write(b"x", true), MasterWriteStep::Broken);
    }

    #[test]
    fn slave_write_cooks_lf_to_crlf() {
        let (m, s) = pty();
        assert_eq!(s.write(b"a\nb"), PtyWriteStep::Wrote(3));
        let mut out = [0u8; 8];
        assert_eq!(m.read(&mut out), PtyReadStep::Read(4));
        assert_eq!(&out[..4], b"a\r\nb");
    }

    #[test]
    fn cooked_slave_read_echoes_the_consumed_bytes_to_the_output() {
        let (m, s) = pty();
        // Cooked (default), no foreground: the bytes are buffered, not signals.
        assert_eq!(wrote(m.write(b"ab\r", true)).0, 3);
        let mut out = [0u8; 8];
        assert_eq!(s.read(&mut out), PtyReadStep::Read(3));
        assert_eq!(&out[..3], b"ab\r");
        // The echo cooks the CR to CRLF onto the output ring.
        let mut echoed = [0u8; 8];
        assert_eq!(m.read(&mut echoed), PtyReadStep::Read(4));
        assert_eq!(&echoed[..4], b"ab\r\n");
    }

    #[test]
    fn raw_slave_read_does_not_echo() {
        let (m, s) = pty();
        m.pty().set_input_mode(InputMode::Raw);
        assert_eq!(wrote(m.write(b"ab", true)).0, 2);
        let mut out = [0u8; 8];
        assert_eq!(s.read(&mut out), PtyReadStep::Read(2));
        assert_eq!(m.read(&mut out), PtyReadStep::Empty);
    }

    #[test]
    fn cooked_ctrl_c_with_a_foreground_job_is_a_signal_not_input() {
        let (m, s) = pty();
        m.pty()
            .foreground()
            .grant(ProcessId(1), ProcessId(2))
            .unwrap();
        let (consumed, signals) = wrote(m.write(&[0x03], true));
        assert_eq!(consumed, 1);
        assert_eq!(signals, vec![(ProcessId(2), Signal::Interrupt)]);
        // The byte was consumed as a signal, not buffered.
        let mut out = [0u8; 4];
        assert_eq!(s.read(&mut out), PtyReadStep::Empty);
    }

    #[test]
    fn cooked_ctrl_z_with_a_foreground_job_maps_to_stop() {
        let (m, _s) = pty();
        m.pty()
            .foreground()
            .grant(ProcessId(1), ProcessId(2))
            .unwrap();
        let (_c, signals) = wrote(m.write(&[0x1A], true));
        assert_eq!(signals, vec![(ProcessId(2), Signal::Stop)]);
    }

    #[test]
    fn cooked_ctrl_c_without_a_foreground_job_is_buffered() {
        let (m, s) = pty();
        let (consumed, signals) = wrote(m.write(&[0x03], true));
        assert_eq!(consumed, 1);
        assert!(signals.is_empty());
        let mut out = [0u8; 4];
        assert_eq!(s.read(&mut out), PtyReadStep::Read(1));
        assert_eq!(out[0], 0x03);
    }

    #[test]
    fn intercept_false_buffers_the_control_byte_even_with_a_foreground_job() {
        let (m, s) = pty();
        m.pty()
            .foreground()
            .grant(ProcessId(1), ProcessId(2))
            .unwrap();
        let (consumed, signals) = wrote(m.write(&[0x03], false));
        assert_eq!(consumed, 1);
        assert!(signals.is_empty());
        let mut out = [0u8; 4];
        assert_eq!(s.read(&mut out), PtyReadStep::Read(1));
        assert_eq!(out[0], 0x03);
    }

    #[test]
    fn raw_mode_passes_control_bytes_through_even_with_a_foreground_job() {
        let (m, s) = pty();
        m.pty()
            .foreground()
            .grant(ProcessId(1), ProcessId(2))
            .unwrap();
        m.pty().set_input_mode(InputMode::Raw);
        let (consumed, signals) = wrote(m.write(&[0x03], true));
        assert_eq!(consumed, 1);
        assert!(signals.is_empty());
        let mut out = [0u8; 4];
        assert_eq!(s.read(&mut out), PtyReadStep::Read(1));
        assert_eq!(out[0], 0x03);
    }

    #[test]
    fn input_mode_round_trips_and_size_is_settable_and_shared() {
        let (m, s) = pty();
        assert_eq!(m.pty().input_mode(), InputMode::Cooked);
        m.pty().set_input_mode(InputMode::Raw);
        assert_eq!(s.pty().input_mode(), InputMode::Raw);
        assert_eq!(m.pty().geometry(), TerminalSize::new(24, 80).unwrap());
        m.pty().set_size(TerminalSize::new(50, 100).unwrap());
        assert_eq!(s.pty().geometry(), TerminalSize::new(50, 100).unwrap());
    }

    #[test]
    fn cloned_slave_ends_keep_the_side_alive_until_the_last_drop() {
        let (m, s) = pty();
        m.pty().set_input_mode(InputMode::Raw);
        let s2 = s.clone();
        drop(s);
        assert_eq!(wrote(m.write(b"x", true)).0, 1);
        drop(s2);
        assert_eq!(m.write(b"y", true), MasterWriteStep::Broken);
    }

    #[test]
    fn cloned_master_ends_keep_the_side_alive_until_the_last_drop() {
        let (m, s) = pty();
        let m2 = m.clone();
        drop(m);
        assert_eq!(s.write(b"z"), PtyWriteStep::Wrote(1));
        drop(m2);
        assert_eq!(s.write(b"z"), PtyWriteStep::Broken);
    }

    #[test]
    fn zero_length_transfers_are_inert() {
        let (m, s) = pty();
        m.pty().set_input_mode(InputMode::Raw);
        assert_eq!(wrote(m.write(b"", true)).0, 0);
        assert_eq!(s.write(b""), PtyWriteStep::Wrote(0));
        assert_eq!(wrote(m.write(b"a", true)).0, 1);
        // A zero-length destination reads nothing but does not report EOF.
        assert_eq!(s.read(&mut []), PtyReadStep::Read(0));
        let mut out = [0u8; 1];
        assert_eq!(s.read(&mut out), PtyReadStep::Read(1));
        assert_eq!(out[0], b'a');
    }

    #[test]
    fn readable_peeks_without_consuming_and_reports_eof() {
        let (m, s) = pty();
        m.pty().set_input_mode(InputMode::Raw);
        assert!(!s.readable());
        assert_eq!(wrote(m.write(b"hi", true)).0, 2);
        assert!(s.readable());
        assert!(s.readable());
        let mut out = [0u8; 4];
        assert_eq!(s.read(&mut out), PtyReadStep::Read(2));
        assert!(!s.readable());
        drop(m);
        assert!(s.readable());
        assert_eq!(s.read(&mut out), PtyReadStep::Eof);
    }

    #[test]
    fn master_readable_tracks_output_and_eof() {
        let (m, s) = pty();
        assert!(!m.readable());
        assert_eq!(s.write(b"o"), PtyWriteStep::Wrote(1));
        assert!(m.readable());
        let mut out = [0u8; 4];
        assert_eq!(m.read(&mut out), PtyReadStep::Read(1));
        assert!(!m.readable());
        drop(s);
        assert!(m.readable());
    }

    #[test]
    fn equality_names_the_end_not_the_content() {
        let (m, s) = pty();
        let m2 = m.clone();
        assert_eq!(m, m2);
        assert_eq!(m.role(), PtyRole::Master);
        assert_eq!(s.role(), PtyRole::Slave);
        let (other_m, _other_s) = pty();
        assert_ne!(m, other_m);
    }
}
