//! The seams through which `tee` touches the outside world.
//!
//! Keeping the filesystem, standard input, and the two output streams
//! behind object-safe traits is what lets the fan-out logic in
//! [`crate::client`] run against in-memory fixtures with no kernel,
//! mirroring the seam design of the other userland tools (`head`'s
//! `FileSource`/`Input`/`Output`, `cp`'s `FileSystem`).

use rustos_abi::Errno;

/// Reads the next bytes of standard input.
///
/// The client streams standard input by repeatedly calling
/// [`read`](Input::read) until a call returns `0` (end-of-input).
pub trait Input {
    /// Read up to `buf.len()` bytes of standard input, returning the number
    /// of bytes written into `buf` (`0` at end-of-input).
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the console raises (e.g. a closed terminal).
    fn read(&self, buf: &mut [u8]) -> Result<usize, Errno>;
}

/// Opens and sequentially writes the file operands.
///
/// Each operand is its own output, keyed by its command-line position
/// `id`, so a file named twice gets two independent streams exactly as two
/// GNU file descriptors would. The client opens every operand up front and
/// then writes each input chunk to every still-live output in operand
/// order.
pub trait FileSink {
    /// Open operand `id` at `path`: create it if absent and either
    /// truncate it (`append == false`) or position every write at the end
    /// of file (`append == true`).
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises — e.g. [`Errno::NotFound`] for
    /// an unreachable path or [`Errno::PermissionDenied`] when the caller
    /// may not write it.
    fn open(&self, id: usize, path: &str, append: bool) -> Result<(), Errno>;

    /// Write every byte of `bytes` to the open operand `id`, after all
    /// previously written bytes.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises while writing.
    fn write(&self, id: usize, bytes: &[u8]) -> Result<(), Errno>;
}

/// Writes bytes to one of the tool's output streams.
///
/// The client uses two instances: standard output for the copied data, and
/// standard error for the per-output diagnostics it reports per the
/// selected output-error mode.
pub trait Output {
    /// Write every byte of `bytes` to the stream.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the stream raises (e.g. a closed consumer).
    fn write_all(&self, bytes: &[u8]) -> Result<(), Errno>;
}
