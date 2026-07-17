//! The runtime volume attach/detach service seam (`plans/DEVICES.md` D3b).
//!
//! The `volume_attach` / `volume_detach` syscalls validate the caller's
//! authority and the request frame, then delegate the actual work —
//! connecting the kernel blkio client, opening the filesystem, mounting,
//! and publishing/unpublishing the volume's root — to the installed
//! [`VolumeService`]. The concrete service lives in the kernel binary
//! crate (it names the filesystem driver crates, which `kernel/core`'s
//! layering does not), installed through the boot handover exactly like
//! the [`FilesystemService`](super::FilesystemService); until it is
//! installed every attach/detach fails closed.

use tairix_abi::volume::{VolumeAttachRequest, VolumeDetachRequest};
use tairix_abi::Errno;

/// The runtime volume attach/detach operations the syscall handlers
/// delegate to.
///
/// The caller's `CAP_FS_MOUNT` and its endpoint/window resource grants
/// have already been verified by the dispatcher and the handler; the
/// service re-validates everything against live state (the endpoint, the
/// window, the device geometry, name/identity collisions) and fails
/// closed.
pub trait VolumeService: Sync {
    /// Attach a filesystem to the runtime block source `request` names,
    /// mount it under its catalog view location, and publish its stable
    /// identity.
    ///
    /// # Errors
    ///
    /// A stable [`Errno`] for every refusal: an unreachable endpoint or
    /// window, an unusable geometry, an unmountable volume, a name or
    /// identity collision, or [`Errno::NotImplemented`] before a service
    /// is installed.
    fn attach(&self, request: &VolumeAttachRequest<'_>) -> Result<(), Errno>;

    /// Flush, unmount, and unpublish the runtime-attached volume
    /// `request` names.
    ///
    /// # Errors
    ///
    /// [`Errno::NotFound`] for an identity this service did not attach
    /// (the boot volumes are permanent), a flush failure (the volume
    /// stays attached — data is never silently discarded), or
    /// [`Errno::NotImplemented`] before a service is installed.
    fn detach(&self, request: &VolumeDetachRequest) -> Result<(), Errno>;
}

/// The fail-closed default: every operation reports
/// [`Errno::NotImplemented`] until a boot path installs a real service.
pub struct NullVolumeService;

impl VolumeService for NullVolumeService {
    fn attach(&self, _request: &VolumeAttachRequest<'_>) -> Result<(), Errno> {
        Err(Errno::NotImplemented)
    }

    fn detach(&self, _request: &VolumeDetachRequest) -> Result<(), Errno> {
        Err(Errno::NotImplemented)
    }
}

/// The shared [`NullVolumeService`] the syscall handlers default to.
pub static NULL_VOLUME_SERVICE: NullVolumeService = NullVolumeService;

#[cfg(test)]
mod tests {
    use super::*;
    use tairix_abi::volume::VolumeFsType;

    #[test]
    fn the_null_service_fails_closed() {
        let attach = VolumeAttachRequest {
            endpoint: 1,
            window: 1,
            first_lba: 0,
            blocks: 8,
            fstype: VolumeFsType::Fat32,
            name: b"usb1",
        };
        assert_eq!(
            NULL_VOLUME_SERVICE.attach(&attach),
            Err(Errno::NotImplemented)
        );
        assert_eq!(
            NULL_VOLUME_SERVICE.detach(&VolumeDetachRequest {
                volume_id: [7; 16],
                force: false
            }),
            Err(Errno::NotImplemented)
        );
    }
}
