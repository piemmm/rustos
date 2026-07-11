//! The seams through which `unmount` touches the outside world.
//!
//! The mount table arrives through the shared
//! [`Transport`](rustos_procinfo::Transport) seam (the `sysinfo-v1`
//! `MOUNT_LIST` paging walk `mount` and `df` use too); only the
//! privileged detach and the output streams need seams of their own.
//! Keeping them behind object-safe traits lets the engine in
//! [`crate::client`] run against in-memory fixtures with no kernel,
//! mirroring the seam discipline of the other userland tools (`mount`'s
//! `Mounter`, `df`'s `PathProbe`).

use rustos_abi::volume::VOLUME_ID_LEN;
use rustos_abi::Errno;

/// Performs the privileged volume detach.
///
/// The implementation performs no authorisation of its own: the kernel's
/// `volume_detach` path requires `CAP_FS_MOUNT`, re-validates the
/// identity against the attached volumes, and audits every decision. A
/// refusal surfaces as the exact [`Errno`] the kernel chose.
pub trait Detacher {
    /// Detach the volume published under `volume_id`; `force` selects
    /// the audited force-unmount that discards retained uncommitted
    /// data when a clean commit is impossible.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the kernel raises — e.g. [`Errno::NotFound`] for an
    /// identity no attached volume carries, [`Errno::DeviceFault`] for a
    /// plain detach of an unavailable volume, or
    /// [`Errno::PermissionDenied`] when the caller lacks `CAP_FS_MOUNT`.
    fn detach(&self, volume_id: [u8; VOLUME_ID_LEN], force: bool) -> Result<(), Errno>;
}

/// Writes bytes to the tool's output streams.
///
/// The client uses two plain instances (standard output for the short
/// help, standard error for diagnostics); the standard-error instance
/// also carries the fd-3 advisory writer for the `--force` suggestion.
pub trait Output {
    /// Write every byte of `bytes` to the stream.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the stream raises (e.g. a closed terminal).
    fn write_all(&self, bytes: &[u8]) -> Result<(), Errno>;

    /// Write one advisory record to the standard information stream
    /// (fd 3), best-effort: fd 3 is ignorable by contract, so failures
    /// are dropped and never affect the outcome.
    fn info(&self, record: &[u8]) {
        let _ = record;
    }
}
