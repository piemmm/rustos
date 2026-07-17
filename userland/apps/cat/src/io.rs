//! The seams through which `cat` touches the outside world.
//!
//! Keeping the filesystem, standard input, and the terminal behind
//! object-safe traits is what lets the streaming logic in [`crate::client`]
//! run against in-memory fixtures with no kernel, mirroring the seam design
//! of the other userland crates (`init`'s `Spawner`/`Reaper`, `login`'s
//! `Prompt`, `sysinfo`'s `Transport`).

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

/// Writes rendered bytes to the terminal.
///
/// The client hands [`write_all`](Output::write_all) either a verbatim input
/// chunk or a line-numbered transformation of it; the seam owns the whole
/// write so a fixture can capture the exact byte stream.
pub trait Output {
    /// Write every byte of `bytes` to the terminal.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the console raises (e.g. a closed terminal).
    fn write_all(&self, bytes: &[u8]) -> Result<(), Errno>;
}
