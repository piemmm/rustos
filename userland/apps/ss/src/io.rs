//! The output seam through which `ss` touches the outside world.
//!
//! The socket table itself arrives through the shared
//! [`Transport`](tairix_procinfo::Transport) seam (the `sysinfo-v1`
//! `NET_SOCKETS` paging walk); only the output streams need a seam of
//! their own. Keeping it behind an object-safe trait lets the render
//! logic in [`crate::client`] run against an in-memory buffer with no
//! kernel, mirroring the seam discipline of the other userland tools
//! (`df`'s `Output`, `ps`'s `Transport`).

use tairix_abi::Errno;

/// Writes bytes to the tool's output streams.
///
/// The client uses two plain instances (standard output for the table,
/// standard error for diagnostics); the standard-output instance also
/// carries the fd-3 advisory writer.
pub trait Output {
    /// Write every byte of `bytes` to the stream.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the stream raises (e.g. a closed terminal).
    fn write_all(&self, bytes: &[u8]) -> Result<(), Errno>;

    /// Write one advisory record to the standard information stream
    /// (fd 3), best-effort: fd 3 is ignorable by contract, so failures
    /// are dropped and never affect the report.
    fn info(&self, record: &[u8]) {
        let _ = record;
    }
}
