//! The shell I/O seam the terminal is built on.
//!
//! [`ShellSource`] is the one thing the terminal needs from the outside world:
//! a channel to the shell process it hosts. The terminal *reads* the bytes the
//! shell has written to its standard output and feeds them to the screen
//! model, and it *writes* the user's keystrokes to the shell's standard input.
//! Keeping it a trait means the screen model and the renderer are exhaustively
//! testable against an in-memory queue without a kernel, exactly as the file
//! browser's `DirectorySource`, `appmgr`'s `BundleStore`, and `ps`'s transport
//! are injected seams.
//!
//! On a running system the source is backed by a capability-checked
//! pseudo-terminal channel to the shell process, so the process-spawn and
//! job-control authority lives behind the seam, not in this app.

use alloc::vec::Vec;

use rustos_abi::Errno;

/// A bidirectional byte channel to the hosted shell.
pub trait ShellSource {
    /// Read the shell output produced since the last call.
    ///
    /// Returns the bytes currently available, which may be empty when the
    /// shell has produced nothing new; an empty read is not an error. The
    /// bytes are taken verbatim and fed to the screen model.
    ///
    /// # Errors
    ///
    /// Returns the kernel boundary's [`Errno`] when the channel cannot be
    /// read — for example [`Errno::NotFound`] once the shell has exited.
    fn read(&mut self) -> Result<Vec<u8>, Errno>;

    /// Write `bytes` (the user's input) to the shell.
    ///
    /// # Errors
    ///
    /// Returns the kernel boundary's [`Errno`] when the channel cannot accept
    /// the input — for example [`Errno::NotFound`] once the shell has exited.
    fn write(&mut self, bytes: &[u8]) -> Result<(), Errno>;
}
