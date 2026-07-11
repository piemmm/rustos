//! The production transport: pipes plus the kernel sandbox spawn mode.
//!
//! Compiled only into freestanding `Run` binaries (feature `program`).
//! The parent side ([`RtLauncher`]) spawns **its own binary** in a worker
//! role: two fresh pipes are created, the child's fd 0 is wired to the
//! request pipe's read end and its fd 1 to the reply pipe's write end
//! through `SpawnAttach::sandbox`, and everything else is closed — the
//! kernel then brands the child capability-empty and confines it to the
//! sandbox syscall allow-list (`docs/src/security/sandbox.md`). The worker
//! side ([`serve_stdio`]) serves the protocol over those standard streams,
//! exactly the surface the allow-list admits.
//!
//! A program wires the two halves together in its `Run` binary: early in
//! `main`, [`worker_role`] detects the worker invocation and hands control
//! to [`serve_stdio`]; otherwise the program builds a
//! [`ParserSandbox`](crate::host::ParserSandbox) over an [`RtLauncher`]
//! naming its own program path.

use alloc::vec::Vec;

use rustos_abi::{Errno, FdWire, SpawnAttach, STDIN, STDOUT, STD_STREAM_COUNT};

use crate::host::Launcher;
use crate::proto::Channel;
use crate::worker::{serve, ServeEnd, Service};

/// The argument-vector marker a parent passes (as `argv[1]`) when
/// spawning its own binary as a sandbox worker, and [`worker_role`]
/// detects. One shared spelling, so no program invents a colliding flag.
pub const WORKER_ROLE_ARG: &[u8] = b"--parser-sandbox-worker";

/// Whether this invocation is a sandbox-worker role: `argv[1]` is exactly
/// [`WORKER_ROLE_ARG`].
///
/// A `Run` binary checks this before any other argument handling and, when
/// true, runs [`serve_stdio`] and exits — a worker never behaves as the
/// interactive program.
#[must_use]
pub fn worker_role() -> bool {
    rustos_rt::arg(1).is_some_and(|arg| arg == WORKER_ROLE_ARG)
}

/// Serve the sandbox protocol over the wired standard streams until the
/// parent closes the request pipe.
pub fn serve_stdio<S: Service>(service: &mut S) -> ServeEnd {
    let mut chan = StdioChannel;
    serve(&mut chan, service)
}

/// The worker's channel: fd 0 in, fd 1 out — the only descriptors a
/// canonically wired sandbox holds.
struct StdioChannel;

impl Channel for StdioChannel {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Errno> {
        // A zero timeout waits indefinitely; the pipe backing parks the
        // worker until bytes arrive or every write end closes (then
        // end-of-stream, 0).
        rustos_rt::stdin_timeout(buf, 0).map_err(errno_from)
    }

    fn write(&mut self, buf: &[u8]) -> Result<usize, Errno> {
        match rustos_rt::stdout(buf) {
            // `stdout` folds every failure to zero accepted bytes; with a
            // pipe backing that means the parent is gone, which the
            // framing's zero-progress rule reports as the peer closed.
            0 if !buf.is_empty() => Err(Errno::BrokenPipe),
            accepted => Ok(accepted),
        }
    }
}

/// Recover the typed [`Errno`] from a raw negative kernel result.
///
/// An unmappable code (a kernel newer than this vocabulary) folds to
/// [`Errno::DeviceFault`] — still a failure, never silently ignored.
fn errno_from(ret: i64) -> Errno {
    i32::try_from(ret.checked_neg().unwrap_or(i32::MAX.into()))
        .ok()
        .and_then(Errno::from_i32)
        .unwrap_or(Errno::DeviceFault)
}

/// [`Launcher`] that spawns `path` (normally the program's own binary) as
/// a sandboxed worker over a fresh pipe pair per launch.
pub struct RtLauncher {
    path: Vec<u8>,
}

impl RtLauncher {
    /// Build a launcher over the program path to spawn as the worker.
    #[must_use]
    pub fn new(path: &[u8]) -> Self {
        Self {
            path: path.to_vec(),
        }
    }

    /// Build a launcher over this program's own binary, via the kernel's
    /// reserved self token ([`rustos_abi::SPAWN_SELF`]): the kernel
    /// substitutes the exact path it admitted the calling process from and
    /// runs the ordinary load gate over it. `argv[0]` is deliberately not
    /// used — it is data the spawner chose (a shell passes the typed
    /// word), never a spawnable spelling the worker launch could trust.
    #[must_use]
    pub fn own_binary() -> Self {
        Self::new(rustos_abi::SPAWN_SELF)
    }
}

impl Launcher for RtLauncher {
    type Channel = RtChannel;

    fn launch(&mut self) -> Result<RtChannel, Errno> {
        // Request pipe: parent writes, worker fd 0 reads.
        let (req_read, req_write) = rustos_rt::pipe_create().map_err(errno_from)?;
        // Reply pipe: worker fd 1 writes, parent reads.
        let (rep_read, rep_write) = match rustos_rt::pipe_create() {
            Ok(pair) => pair,
            Err(ret) => {
                let _ = rustos_rt::fs_close(req_read);
                let _ = rustos_rt::fs_close(req_write);
                return Err(errno_from(ret));
            }
        };
        let mut wires = [FdWire::Closed; STD_STREAM_COUNT];
        wires[STDIN as usize] = FdWire::Handle(req_read);
        wires[STDOUT as usize] = FdWire::Handle(rep_write);
        let attach = SpawnAttach::sandbox(wires);
        let pid =
            rustos_rt::spawn_attached(&self.path, &attach, &[&self.path, WORKER_ROLE_ARG], &[]);
        // The child holds counted clones of its two wired ends; the
        // parent's own copies are closed regardless of the spawn outcome,
        // so a dead worker's reply pipe reports end-of-stream instead of
        // idling on the parent's dangling write end.
        let _ = rustos_rt::fs_close(req_read);
        let _ = rustos_rt::fs_close(rep_write);
        if pid < 0 {
            let _ = rustos_rt::fs_close(req_write);
            let _ = rustos_rt::fs_close(rep_read);
            return Err(errno_from(pid));
        }
        Ok(RtChannel {
            pid: pid_from(pid),
            write_fd: req_write,
            read_fd: rep_read,
        })
    }

    fn dispose(&mut self, channel: RtChannel) -> Option<i32> {
        let pid = channel.pid;
        // Dropping the channel closes the parent's pipe ends; a still-
        // running worker then sees end-of-stream on fd 0 and exits, so the
        // blocking reap below always completes.
        drop(channel);
        let mut code = 0i32;
        let reaped = rustos_rt::wait_exit(pid, &mut code);
        (reaped >= 0).then_some(code)
    }
}

/// Narrow a non-negative spawn result to the wait-facing PID type.
fn pid_from(pid: i64) -> i32 {
    i32::try_from(pid).unwrap_or(i32::MAX)
}

/// The parent's channel to one spawned worker: the request pipe's write
/// end and the reply pipe's read end in the parent's own descriptor table.
pub struct RtChannel {
    pid: i32,
    write_fd: u32,
    read_fd: u32,
}

impl Channel for RtChannel {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Errno> {
        // A pipe ignores the file offset; end-of-stream reads 0.
        rustos_rt::fs_read(self.read_fd, 0, buf).map_err(errno_from)
    }

    fn write(&mut self, buf: &[u8]) -> Result<usize, Errno> {
        rustos_rt::fs_write(self.write_fd, 0, buf).map_err(errno_from)
    }
}

impl Drop for RtChannel {
    fn drop(&mut self) {
        // Closing the parent's ends is what tells the worker its parent is
        // done (end-of-stream on fd 0): the worker's serve loop then
        // finishes cleanly and the process exits.
        let _ = rustos_rt::fs_close(self.write_fd);
        let _ = rustos_rt::fs_close(self.read_fd);
    }
}
