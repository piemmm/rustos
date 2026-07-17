//! The seams through which `head` touches the outside world.
//!
//! Keeping the filesystem, standard input, and the two output streams
//! behind object-safe traits is what lets the streaming logic in
//! [`crate::client`] run against in-memory fixtures with no kernel,
//! mirroring the seam design of the other userland tools (`cat`'s
//! `FileSource`/`Input`/`Output`, `rm`'s `Prompt`).

use tairix_abi::Errno;

/// Reads a byte range of a named file.
///
/// The client streams a file by repeatedly calling [`read`](FileSource::read)
/// with an advancing `offset` until a call returns `0`, which marks
/// end-of-file. An implementation must return at most `buf.len()` bytes and
/// must report a short read (fewer than requested) only at end-of-file or by
/// returning an [`Errno`].
pub trait FileSource {
    /// Read up to `buf.len()` bytes of `path` starting at `offset`, returning
    /// the number of bytes written into `buf` (`0` at end-of-file).
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises — e.g. [`Errno::NotFound`] for a
    /// missing path or [`Errno::PermissionDenied`] when the caller may not
    /// read it.
    fn read(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, Errno>;
}

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

/// Writes bytes to one of the tool's output streams.
///
/// The client uses two instances: standard output for the selected data and
/// headers, and standard error for the per-file diagnostics it reports
/// before moving to the next operand.
pub trait Output {
    /// Write every byte of `bytes` to the stream.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the stream raises (e.g. a closed terminal).
    fn write_all(&self, bytes: &[u8]) -> Result<(), Errno>;
}
