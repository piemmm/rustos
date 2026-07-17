//! The output seam through which `lspci` touches the outside world.
//!
//! The hardware tree itself arrives through the shared
//! [`Transport`](tairix_procinfo::Transport) seam (the `sysinfo-v1`
//! `HARDWARE_TREE` query); only the output streams need a seam of their
//! own. Keeping it behind an object-safe trait lets the listing logic in
//! [`crate::client`] run against in-memory fixtures with no kernel,
//! mirroring the seam discipline of the other userland tools (`df`'s
//! `Output`, `ps`'s `Transport`).

use tairix_abi::Errno;

/// Writes bytes to the tool's output streams.
///
/// The client uses two plain instances (standard output for the listing,
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
    /// are dropped and never affect the listing.
    fn info(&self, record: &[u8]) {
        let _ = record;
    }
}
