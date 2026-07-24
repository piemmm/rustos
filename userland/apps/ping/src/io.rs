//! The output seam through which `ping` touches the outside world for text.
//!
//! Keeping it behind an object-safe trait lets the engine in
//! [`crate::client`] run against an in-memory buffer with no kernel,
//! mirroring the seam discipline of the other userland tools (`ss`'s
//! `Output`, `df`'s `Output`). The ping traffic itself flows through the
//! separate [`crate::net::PingIo`] seam.

use tairix_abi::Errno;

/// Writes bytes to the tool's output streams.
///
/// The client uses two plain instances (standard output for the per-reply
/// lines and the statistics, standard error for diagnostics).
pub trait Output {
    /// Write every byte of `bytes` to the stream.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the stream raises (e.g. a closed terminal).
    fn write_all(&self, bytes: &[u8]) -> Result<(), Errno>;
}
