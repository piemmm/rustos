//! The read-only `/System` file service (Design D, D2b-1 —
//! `.junie/next-pi-prompt.md`).
//!
//! Under Design D the one disk the bootstrap floor brought up is owned for
//! the life of the system by the never-returning driver-store kthread
//! (`crate::shared_block::DriverStoreService`, D2a-2), which keeps the
//! read-only signed-bundle `/System` volume mounted (`AGENTS.md` §18.3 /
//! §18.4). Everything that reads that volume — the in-kernel driver
//! autoload today, and the user-space `devmgr` over an IPC endpoint in
//! D2b-2 — needs exactly two operations against it: **list** the
//! `/System/Drivers/` store and **read** a bundle's bytes.
//!
//! This module is the one object that offers both: [`SystemFileService`]
//! owns a window onto the mounted `/System` volume's filesystem driver and
//! exposes [`SystemFileService::list_store`] (the store-directory walk) and
//! the [`ImageSource`] read the signed-load pipeline pulls each bundle's
//! bytes through. It consolidates the two helpers the boot path previously
//! wired ad hoc — [`rustos_kernel_core::enumerate_driver_store`] for the
//! listing and the now-removed `VfsImageSource` for the reads — behind a
//! single seam (`AGENTS.md` §2.2), so D2b-2 wraps *this* service in the
//! `IPC_RECV` loop rather than re-deriving the read path.
//!
//! # Layering
//!
//! The read itself belongs in `kernel/core`, which owns the root-mount
//! builder and the §5.3-checked per-inode delegation
//! ([`rustos_kernel_core::DriverImageReader`]) — but the [`ImageSource`]
//! trait lives in `userland/system/drvhost`, and §17.4 forbids `kernel/core`
//! from depending on a userland crate. The bin crate is the one layer that
//! may name `drvhost` (`AGENTS.md` §17.4), so this service lives here and
//! simply *delegates* to the kernel-core reader and store walk, adding no
//! authority of its own. Every capability and §5.3 check stays in
//! `kernel/core`, which fails closed (`AGENTS.md` §5.4 / §2.9).
//!
//! # Borrow model
//!
//! The volume's filesystem driver needs `&mut` access per operation, but
//! [`ImageSource::read`] takes `&self`, so the driver is held behind a
//! [`RefCell`]. The two operations are strictly sequential and
//! single-threaded — the autoload lists the store once, then reads one
//! bundle at a time — so the borrow never overlaps. The root-backed VFS the
//! reader uses is built once at [`SystemFileService::open`] (`AGENTS.md`
//! §2.16).

use core::cell::RefCell;

use alloc::string::String;
use alloc::vec::Vec;

use rustos_abi::driver::filesystem::{FilesystemRead, FilesystemSecurity};
use rustos_abi::Errno;
use rustos_drvhost::ImageSource;
use rustos_kernel_core::{enumerate_driver_store, DriverImageError, DriverImageReader, VfsError};
use rustos_log::Sink;

/// A read-only file service over the mounted `/System` volume: it lists the
/// signed `/System/Drivers/` store and reads a bundle's bytes through the
/// kernel's root-backed VFS.
///
/// Construct one with [`SystemFileService::open`], then [`list_store`] the
/// store paths and hand `&service` (an [`ImageSource`]) to
/// `rustos_drvhost::store::scan_store` and the signed-load pipeline.
///
/// [`list_store`]: SystemFileService::list_store
pub struct SystemFileService<'a, F: ?Sized> {
    /// The root-backed VFS reader, its private root mount built once
    /// (`AGENTS.md` §2.16). Every read goes through its §5.3-checked,
    /// fail-closed delegation.
    reader: DriverImageReader,
    /// The store root the listing and the reads are rooted at, relative to
    /// the mounted volume's own root (`AGENTS.md` §2.2 — the one definition
    /// the walk and the reader agree on). [`DriverImageReader::read_image`]
    /// validates every requested path lies strictly below it (§5.4).
    store_root: &'a str,
    /// The mounted volume's filesystem driver, behind a [`RefCell`] because
    /// [`ImageSource::read`] is `&self` while the driver needs `&mut`; the
    /// list-then-read sequence never overlaps the borrow.
    fs: RefCell<&'a mut F>,
}

impl<'a, F> SystemFileService<'a, F>
where
    F: FilesystemRead + FilesystemSecurity + ?Sized,
{
    /// Open a service over the mounted volume's filesystem driver `fs`,
    /// constructing the root-backed VFS once. `store_root` is the store's
    /// path relative to the volume's root
    /// ([`rustos_kernel_core::SYSTEM_VOLUME_STORE_PATH`] on a `/System`
    /// volume, [`rustos_kernel_core::DRIVER_STORE_PATH`] on a whole-root
    /// volume).
    ///
    /// # Errors
    ///
    /// The [`VfsError`] from [`DriverImageReader::open`] if the private root
    /// mount cannot be built — the sole fail-closed refusal that prevents
    /// any read (`AGENTS.md` §2.9).
    pub fn open(fs: &'a mut F, store_root: &'a str) -> Result<Self, VfsError> {
        Ok(Self {
            reader: DriverImageReader::open()?,
            store_root,
            fs: RefCell::new(fs),
        })
    }

    /// List the installed `/System/Drivers/` bundle paths by walking the
    /// store directory off the mounted volume (`AGENTS.md` §18.6).
    ///
    /// This is a structural walk only: it grants no authority and verifies
    /// no signature, and is fail-closed — a missing, unreadable, or empty
    /// store yields fewer (or zero) paths and audits its own outcome
    /// through `audit`, never an error (`AGENTS.md` §18.4 / §2.9).
    #[must_use]
    pub fn list_store(&self, audit: &dyn Sink) -> Vec<String> {
        enumerate_driver_store(&mut **self.fs.borrow_mut(), self.store_root, audit)
    }
}

impl<F> ImageSource for SystemFileService<'_, F>
where
    F: FilesystemRead + FilesystemSecurity + ?Sized,
{
    /// Read the bundle at `path`, appending its bytes to `buf`
    /// (the [`ImageSource`] contract). Delegates to
    /// [`DriverImageReader::read_image`]; the precise refusal (a path
    /// outside the store, a non-file, an over-large image, a short read) is
    /// mapped to the stable [`Errno`] the scan records as the bundle's skip
    /// reason (`AGENTS.md` §5.4 / §2.9).
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

    /// A sink that discards every event; the list walk audits its outcome
    /// but these tests assert the returned paths, not the audit trail.
    struct NullSink;

    impl Sink for NullSink {
        fn write_event(&self, _event: &rustos_log::Event<'_>) {}
    }

    #[test]
    fn list_store_walks_the_mounted_store_directory() {
        let mut fs = MockRootFs::new();
        fs.add_file("/System/Drivers/input/kbd", b"K");
        fs.add_file("/System/Drivers/storage/blk", b"B");
        let service =
            SystemFileService::open(&mut fs, "/System/Drivers").expect("root mount builds");

        let mut paths = service.list_store(&NullSink);
        paths.sort();
        assert_eq!(
            paths,
            vec![
                String::from("/System/Drivers/input/kbd"),
                String::from("/System/Drivers/storage/blk"),
            ],
            "every installed bundle path is discovered"
        );
    }

    #[test]
    fn list_store_of_an_empty_store_is_not_an_error() {
        // A volume with no store directory yields no paths and never errors
        // (`AGENTS.md` §18.4) — the same service still reads cleanly.
        let mut fs = MockRootFs::new();
        let service =
            SystemFileService::open(&mut fs, "/System/Drivers").expect("root mount builds");
        assert!(service.list_store(&NullSink).is_empty());
    }

    #[test]
    fn read_delegates_to_the_reader_and_appends() {
        let mut fs = MockRootFs::new();
        fs.add_file("/System/Drivers/usb_kbd", b"BUNDLE");
        let service =
            SystemFileService::open(&mut fs, "/System/Drivers").expect("root mount builds");

        // The scan pre-clears and reuses one buffer across bundles; prove a
        // non-empty prefix is preserved (the append contract).
        let mut buf = vec![0x01u8];
        service
            .read("/System/Drivers/usb_kbd", &mut buf)
            .expect("a readable in-store bundle");
        assert_eq!(buf, vec![0x01, b'B', b'U', b'N', b'D', b'L', b'E']);
    }

    #[test]
    fn read_serves_multiple_bundles_through_the_one_borrowed_driver() {
        let mut fs = MockRootFs::new();
        fs.add_file("/System/Drivers/a", b"AAA");
        fs.add_file("/System/Drivers/b", b"BB");
        let service =
            SystemFileService::open(&mut fs, "/System/Drivers").expect("root mount builds");

        let mut buf = Vec::new();
        service.read("/System/Drivers/a", &mut buf).expect("a");
        assert_eq!(buf, b"AAA");
        buf.clear();
        service.read("/System/Drivers/b", &mut buf).expect("b");
        assert_eq!(buf, b"BB");
    }

    #[test]
    fn a_missing_bundle_maps_to_not_found() {
        let mut fs = MockRootFs::new();
        fs.add_file("/System/Drivers/present", b"x");
        let service =
            SystemFileService::open(&mut fs, "/System/Drivers").expect("root mount builds");

        let mut buf = Vec::new();
        assert_eq!(
            service.read("/System/Drivers/absent", &mut buf),
            Err(Errno::NotFound)
        );
        assert!(buf.is_empty());
    }

    #[test]
    fn a_path_outside_the_store_is_denied() {
        let mut fs = MockRootFs::new();
        fs.add_file("/System/Security/Users", b"secret");
        let service =
            SystemFileService::open(&mut fs, "/System/Drivers").expect("root mount builds");

        let mut buf = Vec::new();
        assert_eq!(
            service.read("/System/Security/Users", &mut buf),
            Err(Errno::PermissionDenied)
        );
        assert!(buf.is_empty());
    }
}
