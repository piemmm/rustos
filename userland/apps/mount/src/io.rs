//! The seam through which `mount` attaches a filesystem, and the record it
//! carries.
//!
//! Listing the mount table reuses the [`Transport`](tairix_procinfo::Transport)
//! and [`Output`](tairix_procinfo::Output) seams from `lib/procinfo`; only the privileged *attach* operation needs a seam
//! of its own. Keeping it behind an object-safe trait lets the
//! mount-request logic in [`crate::client`] run against an in-memory fixture
//! with no kernel, mirroring the seam design of the other userland crates
//! (`useradd`'s `UserDb`, `setcap`'s `FileSystem`).

use tairix_abi::driver::filesystem::MountFlags;
use tairix_abi::Errno;

/// The fully-parsed attach request handed to [`Mounter::mount`].
///
/// Every field is borrowed from the parsed
/// [`MountRequest`](crate::command::MountRequest), so the spec allocates
/// nothing of its own. A `None` `fstype` asks the kernel to identify the
/// filesystem by probing its superblock; `mount` never guesses one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MountSpec<'a> {
    /// The backing source (a `/Storage` volume or device identifier).
    pub source: &'a str,
    /// The mount-point path.
    pub target: &'a str,
    /// The requested driver filesystem type, or [`None`] to let the kernel
    /// probe.
    pub fstype: Option<&'a str>,
    /// The mount-policy flags to apply.
    pub flags: MountFlags,
}

/// Attaches a filesystem to the mount table.
///
/// Mounting is privileged — it needs `CAP_FS_MOUNT` — but
/// the **kernel** makes that decision, not this tool:
/// `mount` builds and presents a request and an unauthorised or invalid
/// attempt is refused by the seam and surfaced as
/// [`MountError::Mount`](crate::MountError::Mount).
pub trait Mounter {
    /// Attach the filesystem described by `spec`.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the kernel raises — e.g. [`Errno::PermissionDenied`]
    /// when the caller lacks `CAP_FS_MOUNT`, [`Errno::NotFound`] when the
    /// source or target does not exist, or [`Errno::BadMagic`] when the
    /// on-disk filesystem fails validation.
    fn mount(&self, spec: &MountSpec<'_>) -> Result<(), Errno>;
}
