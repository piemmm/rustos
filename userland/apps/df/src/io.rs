//! The seams through which `df` touches the outside world.
//!
//! The mount table itself arrives through the shared
//! [`Transport`](rustos_procinfo::Transport) seam (the `sysinfo-v1`
//! `MOUNT_LIST` paging walk `mount` uses too); only the operand probe and
//! the output streams need seams of their own. Keeping them behind
//! object-safe traits lets the report logic in [`crate::client`] run
//! against in-memory fixtures with no kernel, mirroring the seam
//! discipline of the other userland tools (`ps`'s `Transport`, `du`'s
//! `Walk`).

use rustos_abi::Errno;

/// Verifies that a `file` operand names an existing filesystem node.
///
/// `df <file>` reports the filesystem *containing* `file`, so the operand
/// must exist before its covering mount is chosen; a missing or
/// unreachable operand is diagnosed exactly as GNU `df` does. The
/// implementation performs no authorisation of its own: the secured VFS
/// checks the path per-inode under the caller's attested identity.
pub trait PathProbe {
    /// Confirm `path` resolves to an existing node.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises — e.g. [`Errno::NotFound`] for
    /// a missing path or [`Errno::PermissionDenied`] when the caller may
    /// not reach it.
    fn probe(&self, path: &str) -> Result<(), Errno>;
}

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
