//! The production [`Tty`] over a program's inherited standard streams.
//!
//! Every full-screen tool's `Run` binary needs the same channel: rendered
//! bytes go to standard output (fd 1) and input bytes come from standard
//! input (fd 0), both through the userland runtime `tairix-rt`. This module
//! is that channel's one definition — the per-app copies it replaced were
//! the duplication the charter forbids. A program names only its inherited
//! descriptors, never a console device, so the same binary drives a serial
//! terminal, a framebuffer console, or a future windowed terminal
//! unchanged — the stream layer owns which backing that is.

use alloc::vec::Vec;
use core::time::Duration;

use crate::error::{CursesError, Result};
use crate::screen::Tty;

/// The maximum input bytes drained from standard input in one read. A key
/// press (even a multi-byte escape sequence) is a handful of bytes; a small
/// stack buffer absorbs a burst without allocating, and the curses input
/// decoder reassembles sequences that span reads.
const INPUT_CHUNK: usize = 64;

/// The [`Tty`] over the inherited standard streams (fd 0/1).
///
/// * [`Tty::write`] sends bytes through the shared `tairix_rt::io`
///   short-write loop.
/// * [`Tty::read_blocking`] parks the task in the kernel until input
///   arrives; a closed stream is reported as an I/O error so the session
///   ends loudly instead of spinning on a dead channel.
/// * [`Tty::read_timeout`] parks the task until input arrives or the bound
///   elapses; an elapsed bound is `Ok` with no bytes (the caller's tick), a
///   closed stream is an I/O error — the two are never conflated.
/// * [`Tty::read`] honestly reports "nothing available right now": the
///   standard-input backing owns blocking and offers no peek/poll, so a
///   non-blocking read cannot know what is pending and never lies about it.
pub struct StreamTty;

impl Tty for StreamTty {
    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        use tairix_rt::io::Write as _;
        // `write_all` loops over short writes and fails closed (never
        // spins) if the backing stops accepting bytes, which the seam
        // reports as an I/O error.
        tairix_rt::io::Stdout
            .write_all(bytes)
            .map_err(|_| CursesError::Io)
    }

    fn read(&mut self) -> Result<Vec<u8>> {
        Ok(Vec::new())
    }

    fn read_blocking(&mut self) -> Result<Vec<u8>> {
        let mut buf = [0u8; INPUT_CHUNK];
        // `tairix_rt::stdin` parks the task in the kernel until at least
        // one byte arrives, then returns the count read. A zero-length
        // return means the stream ended: the session's input is gone,
        // reported as an error so the tool ends loudly instead of spinning
        // on a dead channel.
        let read = tairix_rt::stdin(&mut buf);
        if read == 0 {
            return Err(CursesError::Io);
        }
        Ok(buf[..read.min(buf.len())].to_vec())
    }

    fn read_timeout(&mut self, timeout: Duration) -> Result<Vec<u8>> {
        let mut buf = [0u8; INPUT_CHUNK];
        // The kernel treats a zero bound as "wait indefinitely", so the
        // delay is floored at one nanosecond — defence in depth, never the
        // caller's wait. The backing parks the task until input arrives or
        // the bound elapses.
        let timeout_ns = u64::try_from(timeout.as_nanos()).unwrap_or(u64::MAX).max(1);
        match tairix_rt::stdin_timeout(&mut buf, timeout_ns) {
            // A successful zero-length read is the closed stream, reported
            // loudly (below an elapsed bound is Ok-with-no-bytes, so a
            // closed channel must not masquerade as a tick).
            Ok(0) => Err(CursesError::Io),
            Ok(read) => Ok(buf[..read.min(buf.len())].to_vec()),
            Err(err) => {
                let timed_out = i32::try_from(-err)
                    .ok()
                    .and_then(tairix_abi::Errno::from_i32)
                    .is_some_and(|errno| errno == tairix_abi::Errno::TimedOut);
                if timed_out {
                    // An elapsed bound is not an error: it is the caller's
                    // tick, reported as "no bytes yet".
                    Ok(Vec::new())
                } else {
                    Err(CursesError::Io)
                }
            }
        }
    }
}
