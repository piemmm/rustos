//! VFS-backed [`ImageSource`] adapter for the signed-driver-store scan
//! (`plans/PI.md` P10 Stage 4.HW item 5; `AGENTS.md` §18.3 / §18.6).
//!
//! The user-space store scan (`rustos_drvhost::store::scan_store`) fetches
//! each bundle's bytes through the `drvhost` [`ImageSource`] seam. On a real
//! installation those bytes live on the mounted root volume under
//! `/System/Drivers/`, so the production boot wiring needs an `ImageSource`
//! that reads them through the kernel's root-backed VFS.
//!
//! That read belongs in `kernel/core`, which owns the root-mount builder and
//! the §5.3-checked per-inode delegation
//! ([`rustos_kernel_core::DriverImageReader`]). But the [`ImageSource`] trait
//! lives in `userland/system/drvhost`, and the §17.4 layering forbids
//! `kernel/core` from depending on a userland crate. The bin crate is the one
//! layer that may name `drvhost` (`AGENTS.md` §17.4), so this thin adapter
//! lives here and simply *delegates* to the kernel-core reader — adding no
//! authority of its own.
//!
//! The adapter holds a single [`DriverImageReader`] (its root-backed VFS built
//! once, `AGENTS.md` §2.16) and the root-volume filesystem driver. The driver
//! needs `&mut` access per read, but [`ImageSource::read`] takes `&self`, so
//! the driver is held behind a [`RefCell`]: the scan is single-threaded and
//! pulls one bundle at a time, so the borrow never overlaps. Every capability
//! and §5.3 check stays in the kernel-core reader, which fails closed
//! (`AGENTS.md` §5.4 / §2.9).

use core::cell::RefCell;

use alloc::vec::Vec;

use rustos_abi::driver::filesystem::{FilesystemRead, FilesystemSecurity};
use rustos_abi::Errno;
use rustos_drvhost::ImageSource;
use rustos_kernel_core::{DriverImageError, DriverImageReader, VfsError};

/// An [`ImageSource`] that reads driver-bundle images off the mounted root
/// volume's `/System/Drivers/` store through the kernel's root-backed VFS.
///
/// Construct one with [`VfsImageSource::open`], then hand `&source` to
/// `rustos_drvhost::store::scan_store` alongside the paths
/// [`rustos_kernel_core::enumerate_driver_store`] returned.
pub struct VfsImageSource<'a, F: ?Sized> {
    reader: DriverImageReader,
    /// The store root the enumerated paths are rooted at, relative to the
    /// scanned volume's root (`AGENTS.md` §2.2 — the one definition the
    /// scan and this reader agree on). [`read_image`](DriverImageReader::read_image)
    /// validates every requested path lies strictly below it (§5.4).
    store_root: &'a str,
    fs: RefCell<&'a mut F>,
}

impl<'a, F> VfsImageSource<'a, F>
where
    F: FilesystemRead + FilesystemSecurity + ?Sized,
{
    /// Build an adapter over the mounted volume's filesystem driver `fs`,
    /// constructing the root-backed VFS once. `store_root` is the store's
    /// path relative to the volume's root (`rustos_kernel_core::DRIVER_STORE_PATH`
    /// on a whole-root volume, `SYSTEM_VOLUME_STORE_PATH` on a `/System`
    /// volume), the same root the paths were enumerated under.
    ///
    /// # Errors
    ///
    /// The [`VfsError`] from [`DriverImageReader::open`] if the private root
    /// mount cannot be built.
    pub fn open(fs: &'a mut F, store_root: &'a str) -> Result<Self, VfsError> {
        Ok(Self {
            reader: DriverImageReader::open()?,
            store_root,
            fs: RefCell::new(fs),
        })
    }
}

impl<F> ImageSource for VfsImageSource<'_, F>
where
    F: FilesystemRead + FilesystemSecurity + ?Sized,
{
    /// Read the bundle at `path`, appending its bytes to `buf`
    /// (the [`ImageSource`] contract). Delegates to
    /// [`DriverImageReader::read_image`]; the precise refusal is mapped to
    /// the stable [`Errno`] the scan records as the bundle's skip reason.
    fn read(&self, path: &str, buf: &mut Vec<u8>) -> Result<(), Errno> {
        let mut fs = self.fs.borrow_mut();
        self.reader
            .read_image(&mut **fs, self.store_root, path, buf)
            .map_err(DriverImageError::to_errno)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloc::vec;

    use crate::test_support::MockRootFs;

    #[test]
    fn read_delegates_to_the_reader_and_appends() {
        let mut fs = MockRootFs::new();
        fs.add_file("/System/Drivers/usb_kbd", b"BUNDLE");
        let source = VfsImageSource::open(&mut fs, "/System/Drivers").expect("root mount builds");

        // The scan pre-clears and reuses one buffer across bundles; prove a
        // non-empty prefix is preserved (the append contract).
        let mut buf = vec![0x01u8];
        source
            .read("/System/Drivers/usb_kbd", &mut buf)
            .expect("a readable in-store bundle");
        assert_eq!(buf, vec![0x01, b'B', b'U', b'N', b'D', b'L', b'E']);
    }

    #[test]
    fn read_serves_multiple_bundles_through_the_one_borrowed_driver() {
        let mut fs = MockRootFs::new();
        fs.add_file("/System/Drivers/a", b"AAA");
        fs.add_file("/System/Drivers/b", b"BB");
        let source = VfsImageSource::open(&mut fs, "/System/Drivers").expect("root mount builds");

        let mut buf = Vec::new();
        source.read("/System/Drivers/a", &mut buf).expect("a");
        assert_eq!(buf, b"AAA");
        buf.clear();
        source.read("/System/Drivers/b", &mut buf).expect("b");
        assert_eq!(buf, b"BB");
    }

    #[test]
    fn a_missing_bundle_maps_to_not_found() {
        let mut fs = MockRootFs::new();
        fs.add_file("/System/Drivers/present", b"x");
        let source = VfsImageSource::open(&mut fs, "/System/Drivers").expect("root mount builds");

        let mut buf = Vec::new();
        assert_eq!(
            source.read("/System/Drivers/absent", &mut buf),
            Err(Errno::NotFound)
        );
        assert!(buf.is_empty());
    }

    #[test]
    fn a_path_outside_the_store_is_denied() {
        let mut fs = MockRootFs::new();
        fs.add_file("/System/Security/Users", b"secret");
        let source = VfsImageSource::open(&mut fs, "/System/Drivers").expect("root mount builds");

        let mut buf = Vec::new();
        assert_eq!(
            source.read("/System/Security/Users", &mut buf),
            Err(Errno::PermissionDenied)
        );
        assert!(buf.is_empty());
    }
}
