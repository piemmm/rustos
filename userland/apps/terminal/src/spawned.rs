//! The spawned shell's pty wiring: the production [`ShellSource`]
//! (`plans/APPWIN.md` AW4, `plans/PTY.md`).
//!
//! The terminal hosts the user's shell as its own child process over one
//! kernel **pseudo-terminal**: the terminal holds the pty master, and the
//! shell's fd 0/1/2 are all wired to the one pty slave — the slave carries
//! a console-class line discipline, so the shell runs its full interactive
//! editor with local echo, canonical line editing, `Ctrl-C`/`Ctrl-Z` job
//! control, and `ONLCR` newline cooking, exactly as on the hardware
//! console. The elsh `wireplan` machinery is the precedent: the child's
//! standard descriptors are wired at spawn through the attach block's
//! [`FdWire::Handle`] entries, each owner-checked kernel-side against the
//! spawning terminal's own open table.
//!
//! Everything with behaviour lives here, host-tested over injected
//! closures; the `Run` binary only supplies the live syscalls
//! (`pty_create`, `spawn_attached`, `fs_read`, `fs_write`) exactly as the
//! file browser's binary supplies its directory fetcher:
//!
//! * [`shell_wires`] is the one definition of the child's descriptor
//!   layout, so the spawn call and the tests can never disagree about
//!   which end lands where.
//! * [`StreamShellSource`] adapts a read/write primitive pair onto the
//!   [`ShellSource`] seam: reads drain one bounded chunk (the caller only
//!   reads after its wait-set reported the descriptor readable, so a read
//!   never parks the event loop), and writes loop over short writes until
//!   every keystroke byte is delivered.

use alloc::vec::Vec;

use tairix_abi::{Errno, FdWire, SpawnAttach, WaitStatus, STD_STREAM_COUNT};

use crate::shell::ShellSource;

/// The terse reason to report if the hosted shell's reaped exit `status`
/// is a reserved asynchronous *load*-failure status, or `None` for a clean
/// or ordinary exit (which ends the terminal silently).
///
/// `spawn` admits a child immediately and the child loads its own image on
/// its first slice (the asynchronous-launch semantics of
/// `plans/FIX-DESKTOP.md`), so a shell that cannot be read, verified, or
/// built no longer fails the terminal's `spawn_attached` call synchronously
/// — it is admitted and then exits with one of the reserved `LOAD_*`
/// statuses. The terminal reaps that exit and must still state the reason
/// (fail loud: the terminal's whole purpose was to host that shell), which
/// this classifies through the single shared
/// [`tairix_abi::load_failure_reason`] mapping so every launcher words a
/// cause identically.
#[must_use]
pub fn shell_load_failure(status: WaitStatus) -> Option<&'static str> {
    match status {
        WaitStatus::Exited(code) => tairix_abi::load_failure_reason(code),
        WaitStatus::Stopped(_) => None,
    }
}

/// Bytes drained from the shell's output pipe per [`ShellSource::read`]:
/// one bounded chunk per wait-set wake. A still-readable pipe re-reports
/// on the next wait (readiness is a level peek), so a burst larger than
/// one chunk drains across successive wakes without ever blocking the
/// event loop — a bound, not a capacity.
pub const READ_CHUNK: usize = 4096;

/// The spawned shell's standard-descriptor wires: fd 0 (stdin), fd 1
/// (stdout), and fd 2 (stderr) are all the one pty `slave` descriptor. The
/// slave is opened read/write, so it serves the shell's input read *and*
/// its interleaved output/diagnostic writes — a terminal renders stderr
/// beside stdout, exactly as a console-backed shell interleaves them, and
/// the shell sees a single controlling tty. fd 3 (`stdinfo`) is closed:
/// the terminal consumes no advisory records from its shell, and a closed
/// slot fails those writes harmlessly (best-effort by contract).
#[must_use]
pub fn shell_wires(slave: u32) -> SpawnAttach {
    let mut wires = [FdWire::Closed; STD_STREAM_COUNT];
    wires[0] = FdWire::Handle(slave);
    wires[1] = FdWire::Handle(slave);
    wires[2] = FdWire::Handle(slave);
    SpawnAttach {
        wires,
        ..SpawnAttach::INHERIT
    }
}

/// Build the environment handed to the hosted shell: this terminal's own
/// inherited environment (`inherited`, the `NAME=value` byte entries the
/// desktop session forwarded — `USER`, `HOME`, `LOGNAME`, `PATH`, `LANG`,
/// …), with `TERM` replaced by `term` (the emulator this terminal presents).
///
/// The shell is the logged-in user's shell, so its prompt and its children
/// need the same identity and locale the session runs under; forwarding the
/// whole environment rather than a hand-picked subset keeps the terminal from
/// having to know which variables the shell cares about (the shell's prompt
/// reads `USER`/`HOSTNAME`/`HOME`, apps read `LANG`, and so on). Any inherited
/// `TERM` is dropped so the terminal's own `TERM` is authoritative — a stale
/// inherited value must never describe a different emulator. The environment
/// is data and carries no authority.
///
/// Returned owned so the caller (the `Run` binary reading `tairix_rt::env`,
/// or a test) can borrow the entries into the `&[&[u8]]` the spawn takes; the
/// logic is pure so it is host-tested without a kernel, exactly as
/// [`shell_wires`] is.
#[must_use]
pub fn shell_env<'a>(
    term: &str,
    inherited: impl IntoIterator<Item = &'a [u8]>,
) -> Vec<alloc::vec::Vec<u8>> {
    let mut env: Vec<alloc::vec::Vec<u8>> = Vec::new();
    for entry in inherited {
        if entry.starts_with(b"TERM=") {
            continue;
        }
        env.push(entry.to_vec());
    }
    let mut term_entry = alloc::vec::Vec::from(&b"TERM="[..]);
    term_entry.extend_from_slice(term.as_bytes());
    env.push(term_entry);
    env
}

/// The production [`ShellSource`]: the shell's byte channel over an
/// injected read/write primitive pair.
///
/// `read` is the positional-free read on the pty master end (the `Run`
/// binary passes `fs_read` — it drains the shell's cooked output); `write`
/// is its master-end sibling (it feeds keystrokes through the input
/// discipline). Both follow the kernel convention: `Ok(n)` bytes
/// transferred, `Ok(0)` from `read` meaning end-of-stream (every slave end
/// closed — the shell exited). Injection keeps the seam host-testable
/// without a kernel, exactly as the file browser injects its directory
/// fetcher.
pub struct StreamShellSource<R, W>
where
    R: FnMut(&mut [u8]) -> Result<usize, Errno>,
    W: FnMut(&[u8]) -> Result<usize, Errno>,
{
    read: R,
    write: W,
}

impl<R, W> StreamShellSource<R, W>
where
    R: FnMut(&mut [u8]) -> Result<usize, Errno>,
    W: FnMut(&[u8]) -> Result<usize, Errno>,
{
    /// A source over the injected primitive pair.
    pub const fn new(read: R, write: W) -> Self {
        Self { read, write }
    }
}

impl<R, W> ShellSource for StreamShellSource<R, W>
where
    R: FnMut(&mut [u8]) -> Result<usize, Errno>,
    W: FnMut(&[u8]) -> Result<usize, Errno>,
{
    /// Drain one bounded chunk of shell output.
    ///
    /// The caller reads only after its wait-set reported the descriptor
    /// readable, so the read returns without parking. End-of-stream (the
    /// shell exited and the pipe drained) surfaces as
    /// [`Errno::NotFound`] — the seam's documented "shell has exited"
    /// refusal — never as a fabricated empty read that would leave the
    /// terminal parked on a dead channel.
    fn read(&mut self) -> Result<Vec<u8>, Errno> {
        let mut chunk = alloc::vec![0u8; READ_CHUNK];
        let n = (self.read)(&mut chunk)?;
        if n == 0 {
            return Err(Errno::NotFound);
        }
        chunk.truncate(n);
        Ok(chunk)
    }

    /// Deliver every keystroke byte, looping over short writes.
    ///
    /// A pty input ring accepts up to its free space per write, so a burst
    /// paste can land short; the loop resumes at the undelivered tail. A
    /// `0`-byte acceptance for a non-empty remainder can only mean a broken
    /// channel, and fails closed rather than spinning.
    fn write(&mut self, bytes: &[u8]) -> Result<(), Errno> {
        let mut sent = 0;
        while sent < bytes.len() {
            let n = (self.write)(&bytes[sent..])?;
            if n == 0 {
                return Err(Errno::BrokenPipe);
            }
            sent += n;
        }
        Ok(())
    }
}
