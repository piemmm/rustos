//! The spawned shell's pipe wiring: the production [`ShellSource`]
//! (`plans/APPWIN.md` AW4).
//!
//! The terminal hosts the user's shell as its own child process over two
//! kernel pipes: one carries the user's keystrokes to the shell's standard
//! input, the other carries everything the shell writes (standard output
//! *and* standard error — a terminal shows both) back to the screen model.
//! The elsh `wireplan` machinery is the precedent: the child's standard
//! descriptors are wired at spawn through the attach block's
//! [`FdWire::Handle`] entries, each owner-checked kernel-side against the
//! spawning terminal's own open table.
//!
//! Everything with behaviour lives here, host-tested over injected
//! closures; the `Run` binary only supplies the live syscalls
//! (`pipe_create`, `spawn_attached`, `fs_read`, `fs_write`) exactly as the
//! file browser's binary supplies its directory fetcher:
//!
//! * [`shell_wires`] is the one definition of the child's descriptor
//!   layout, so the spawn call and the tests can never disagree about
//!   which end lands where.
//! * [`PipeShellSource`] adapts a read/write primitive pair onto the
//!   [`ShellSource`] seam: reads drain one bounded chunk (the caller only
//!   reads after its wait-set reported the descriptor readable, so a read
//!   never parks the event loop), and writes loop over short writes until
//!   every keystroke byte is delivered.

use alloc::vec::Vec;

use rustos_abi::{Errno, FdWire, SpawnAttach, STD_STREAM_COUNT};

use crate::shell::ShellSource;

/// Bytes drained from the shell's output pipe per [`ShellSource::read`]:
/// one bounded chunk per wait-set wake. A still-readable pipe re-reports
/// on the next wait (readiness is a level peek), so a burst larger than
/// one chunk drains across successive wakes without ever blocking the
/// event loop — a bound, not a capacity.
pub const READ_CHUNK: usize = 4096;

/// The spawned shell's standard-descriptor wires: its input is the
/// terminal's keystroke pipe (`shell_stdin`), and both its output *and*
/// its diagnostics land on the terminal's one output pipe
/// (`shell_output`) — a terminal renders stderr beside stdout, exactly as
/// a console-backed shell interleaves them. fd 3 (`stdinfo`) is closed:
/// the terminal consumes no advisory records from its shell, and a closed
/// slot fails those writes harmlessly (best-effort by contract).
#[must_use]
pub fn shell_wires(shell_stdin: u32, shell_output: u32) -> SpawnAttach {
    let mut wires = [FdWire::Closed; STD_STREAM_COUNT];
    wires[0] = FdWire::Handle(shell_stdin);
    wires[1] = FdWire::Handle(shell_output);
    wires[2] = FdWire::Handle(shell_output);
    SpawnAttach {
        wires,
        ..SpawnAttach::INHERIT
    }
}

/// The production [`ShellSource`]: the shell's byte channel over an
/// injected read/write primitive pair.
///
/// `read` is the positional-free read on the output pipe's read end (the
/// `Run` binary passes `fs_read`); `write` is its keystroke-pipe sibling.
/// Both follow the kernel convention: `Ok(n)` bytes transferred, `Ok(0)`
/// from `read` meaning end-of-stream (every shell-side write end closed
/// — the shell exited). Injection keeps the seam host-testable without a
/// kernel, exactly as the file browser injects its directory fetcher.
pub struct PipeShellSource<R, W>
where
    R: FnMut(&mut [u8]) -> Result<usize, Errno>,
    W: FnMut(&[u8]) -> Result<usize, Errno>,
{
    read: R,
    write: W,
}

impl<R, W> PipeShellSource<R, W>
where
    R: FnMut(&mut [u8]) -> Result<usize, Errno>,
    W: FnMut(&[u8]) -> Result<usize, Errno>,
{
    /// A source over the injected primitive pair.
    pub const fn new(read: R, write: W) -> Self {
        Self { read, write }
    }
}

impl<R, W> ShellSource for PipeShellSource<R, W>
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
    /// A pipe accepts up to its free space per write, so a burst paste can
    /// land short; the loop resumes at the undelivered tail. A `0`-byte
    /// acceptance for a non-empty remainder can only mean a broken
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
