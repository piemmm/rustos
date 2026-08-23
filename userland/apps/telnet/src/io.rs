//! The output seam through which `telnet` writes text.
//!
//! Keeping it behind an object-safe trait lets the engine in
//! [`crate::client`] run against an in-memory buffer with no kernel, mirroring
//! the seam discipline of the other userland tools (`ping`'s `Output`, `ss`'s
//! `Output`). The connection's own traffic flows through the separate
//! [`crate::net::TelnetIo`] seam.

use tairix_abi::Errno;

/// Writes bytes to the tool's output streams.
///
/// The client uses two plain instances: standard output for everything the
/// server sends and for the command interpreter's own replies, standard error
/// for diagnostics.
pub trait Output {
    /// Write every byte of `bytes` to the stream.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the stream raises (e.g. a closed terminal).
    fn write_all(&self, bytes: &[u8]) -> Result<(), Errno>;
}
