//! Boot-time enumeration of the `/System/Drivers/` signed-driver store off
//! the mounted root volume (`plans/PI.md` P10
//! Stage 4.HW item 5).
//!
//! TAIRiX does not ship a compiled-in list of *which* drivers exist: the discovered driver set is found at runtime by
//! scanning the installed signed bundles under `/System/Drivers/` and
//! reading each bundle's manifest bind table. [`enumerate_driver_store`]
//! is the kernel's half of that scan — the *path enumeration* that turns
//! the on-disk store tree into the list of bundle image paths the
//! user-space driver-store scan (`tairix_drvhost::store::scan_store`)
//! reads, bind-decodes, and hands to the `devmgr` autoloader.
//!
//! It mirrors [`crate::users::load_users_db`]: given the live
//! [`FilesystemRead`] + [`FilesystemSecurity`] driver of the mounted root
//! volume (arxfs on a real installation), it builds a minimal root-backed
//! VFS (`crate::fs::root_backed_vfs`) and walks [`DRIVER_STORE_PATH`]
//! through the VFS's-checked per-inode delegation, collecting the
//! path of every regular file it finds.
//!
//! # What the walk does — and what it deliberately does not
//!
//! The walk is *structural path discovery only*. It yields the image path
//! of each regular file under `/System/Drivers/` (the store tree is
//! organised `<class>[/<vendor>]/<driver>`); it does **not**
//! read, parse, signature-verify, or otherwise trust a bundle. That is the
//! load gate's job (`tairix_drvhost::Host::load`), run only when — and
//! only when — a candidate wins a hardware-tree node.
//!
//! # Fail closed, never fatal
//!
//! Every refusal is contained to the offending entry: a sub-directory that
//! cannot be listed, an entry that cannot be `stat`-ed, a name that is not
//! a single well-formed path component, or anything past the bounds below
//! is **skipped** (counted, not collected) and the walk continues. A
//! `/System/Drivers/` that does not exist is not an error — a headless or
//! driverless install simply enumerates nothing and autoloads nothing. The scan yields whatever well-formed paths it
//! found; it never panics and never aborts the boot.
//!
//! # Credentials of the boot read
//!
//! Like the users-database read, the walk runs under the kernel's
//! bootstrap identity — `uid 0`, `gid 0`, **no** capabilities — which
//! carries no ambient power. The store directories are
//! reachable because their stored records make them searchable to
//! that identity, not because the kernel bypasses the check.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::driver::filesystem::{FilesystemRead, FilesystemSecurity, NodeKind};
use tairix_abi::Errno;
use tairix_caps::CapabilitySet;
use tairix_kernel_sec::{GroupId, UserId};
use tairix_log::{Field, Level, Sink};
use tairix_util::fmt::format_usize;

use crate::audit::{emit, AuditEvent};
use crate::fs::{Credentials, Path, Vfs, VfsError};

/// Canonical, global absolute path of the signed-driver store
/// (drivers live under `/System/Drivers/`).
///
/// This is the store's address in the *whole* filesystem namespace, i.e.
/// on a volume whose own root is the namespace root `/` (the legacy whole-
/// root model the host tests model). The store path a scan walks is always
/// taken **relative to the root of the volume being scanned**, so a
/// dedicated `/System` volume — whose own root *is* `/System` (design B,
/// `plans/PI.md`) — carries the store at [`SYSTEM_VOLUME_STORE_PATH`]
/// instead. The scan APIs take that root explicitly rather than baking one
/// path in (one definition, two mount models).
pub const DRIVER_STORE_PATH: &str = "/System/Drivers";

/// Path of the signed-driver store **relative to the root of a dedicated
/// read-only `/System` volume** (design B, `plans/PI.md`).
///
/// On the design-B layout `/System` is its own volume mounted at the
/// `/System` mount point, so the volume's own root *is* `/System` and the
/// `/System/Drivers/` store sits at the volume-relative `/Drivers`.
/// This is the same store names globally [`DRIVER_STORE_PATH`]; only
/// the volume it is addressed on differs. The kernel boot path passes this
/// when it scans the pre-unlock `/System` volume.
pub const SYSTEM_VOLUME_STORE_PATH: &str = "/Drivers";

/// Path of the machine-wide settings tree **relative to the root of a
/// dedicated read-only `/System` volume** (design B, `plans/PI.md`).
///
/// On the design-B layout `/System` is its own volume, so the
/// `/System/Settings/` tree sits at the volume-relative `/Settings`. The
/// read-only `/System` store service confines its config reads (the device
/// manager's pre-unlock `network.conf` / `system.conf` reads, served over
/// the store endpoint because the general VFS is not mounted until unlock)
/// strictly below this root — the settings analogue of
/// [`SYSTEM_VOLUME_STORE_PATH`].
pub const SYSTEM_VOLUME_SETTINGS_PATH: &str = "/Settings";

/// Maximum directory depth the walk descends *below* [`DRIVER_STORE_PATH`].
///
/// The store tree is `<class>[/<vendor>]/<driver>`, at most a
/// few levels deep; this is a fail-closed validation bound (a defence against a malformed or hostile on-disk tree, not a
/// scalable capacity), not a limit a legitimate store ever reaches. A
/// node deeper than this is skipped.
pub const MAX_STORE_DEPTH: usize = 8;

/// Maximum number of driver bundle image paths the walk collects.
///
/// A fail-closed validation bound: a store presenting
/// more entries than this is malformed, and the surplus is skipped rather
/// than allowed to grow the scan without limit.
pub const MAX_STORE_DRIVERS: usize = 256;

/// Enumerate the signed-driver store rooted at `store_root` on the mounted
/// volume `fs`, returning the image path of every driver bundle found.
///
/// `store_root` is the store's path **relative to the root of `fs`** — the
/// global [`DRIVER_STORE_PATH`] on a whole-root volume, or
/// [`SYSTEM_VOLUME_STORE_PATH`] on a dedicated `/System` volume (design B).
///
/// The returned paths are absolute and rooted at `store_root`, in the
/// driver's on-disk enumeration order. Each is understood verbatim by the
/// user-space scan (`tairix_drvhost::store::scan_store`, which reads and
/// bind-decodes the bundle) and by the load gate
/// (`tairix_drvhost::Host::load`, which verifies it). This function does
/// none of that — it only finds the paths.
///
/// A single [`AuditEvent::DriverStoreScanned`] record is emitted with the
/// count of paths found and the count of entries skipped fail-closed. The scan never errors: a missing store, an
/// unreadable sub-directory, or a malformed entry all simply contribute
/// fewer paths.
#[must_use]
pub fn enumerate_driver_store<F>(fs: &mut F, store_root: &str, audit: &dyn Sink) -> Vec<String>
where
    F: FilesystemRead + FilesystemSecurity + ?Sized,
{
    let mut drivers: Vec<String> = Vec::new();
    let mut skipped: usize = 0;

    match crate::fs::root_backed_vfs() {
        Ok(vfs) => {
            let caps = CapabilitySet::empty();
            let cred = bootstrap_credentials(&caps);
            walk_dir(&vfs, &cred, fs, store_root, 0, &mut drivers, &mut skipped);
        }
        // The private root mount could not be built; nothing to scan. The
        // walk is fail-closed, so this surfaces as an
        // empty store rather than a panic.
        Err(_) => skipped += 1,
    }

    audit_scan(audit, store_root, drivers.len(), skipped);
    drivers
}

/// Recursively collect the regular-file image paths under the directory at
/// `dir`, descending into sub-directories up to [`MAX_STORE_DEPTH`].
///
/// `depth` counts levels below the store root (`store_root` itself is depth
/// `0`). Every fail-closed refusal increments `skipped` and the walk
/// continues; nothing here returns an error.
fn walk_dir<F>(
    vfs: &Vfs,
    cred: &Credentials<'_>,
    fs: &mut F,
    dir: &str,
    depth: usize,
    drivers: &mut Vec<String>,
    skipped: &mut usize,
) where
    F: FilesystemRead + FilesystemSecurity + ?Sized,
{
    let Ok(dir_path) = Path::parse(dir) else {
        *skipped += 1;
        return;
    };

    // A directory the boot identity may not list, a driver fault, or a
    // store that simply does not exist all leave this subtree empty. A non-root listing failure is a skipped entry;
    // a missing store root is the legitimate "no drivers" case and is not
    // counted, because the empty result already says so.
    let entries = match vfs.list_via_secured(cred, &dir_path, fs) {
        Ok(entries) => entries,
        Err(_) if depth == 0 => return,
        Err(_) => {
            *skipped += 1;
            return;
        }
    };

    for (info, name) in entries {
        if drivers.len() >= MAX_STORE_DRIVERS {
            // The store presents more entries than the validation bound
            // permits; the surplus is refused fail-closed rather than growing the scan without limit.
            *skipped += 1;
            continue;
        }

        let child = if dir == "/" {
            let mut s = String::with_capacity(1 + name.len());
            s.push('/');
            s.push_str(&name);
            s
        } else {
            let mut s = String::with_capacity(dir.len() + 1 + name.len());
            s.push_str(dir);
            s.push('/');
            s.push_str(&name);
            s
        };

        let Ok(child_path) = Path::parse(&child) else {
            *skipped += 1;
            continue;
        };
        // Defend against a driver returning a name that is not a single
        // path component (an embedded `/`, an empty or dotted token): the
        // child must add exactly one level to its parent, or it is refused
        // (validate every input).
        if child_path.depth() != dir_path.depth() + 1 {
            *skipped += 1;
            continue;
        }

        // The listing carries each entry's structural kind, so the walk
        // never re-resolves a child by path.
        match info.kind {
            NodeKind::RegularFile => drivers.push(child),
            NodeKind::Directory => {
                if depth >= MAX_STORE_DEPTH {
                    // Deeper than the validation bound; refuse rather
                    // than recurse without limit.
                    *skipped += 1;
                } else {
                    walk_dir(vfs, cred, fs, &child, depth + 1, drivers, skipped);
                }
            }
        }
    }
}

/// The kernel's bootstrap filesystem identity — `uid 0`, `gid 0`, **no**
/// capabilities.
///
/// Defined once so the store enumeration and the [`DriverImageReader`]
/// read share the exact same credential rather than each carrying its own
/// copy. The identity carries no ambient power: store
/// paths are reachable only because their stored records make them
/// searchable/readable to it, never because the kernel bypasses the check.
fn bootstrap_credentials(caps: &CapabilitySet) -> Credentials<'_> {
    Credentials {
        uid: UserId(0),
        gid: GroupId(0),
        supplementary_gids: &[],
        caps,
    }
}

/// Maximum size, in bytes, of a single driver-bundle image the boot reader
/// will load into memory.
///
/// A fail-closed validation bound (a defence against a
/// malformed or hostile on-disk bundle, not a scalable capacity): a store
/// entry larger than this is refused rather than allowed to exhaust the
/// boot heap. A legitimate `.rxe` driver bundle (manifest + program) sits
/// far below this ceiling.
pub const MAX_DRIVER_IMAGE_LEN: usize = 16 * 1024 * 1024;

/// Why a [`DriverImageReader::read_image`] read refused.
///
/// Every variant is a fail-closed refusal. The
/// precise reason is retained for in-kernel logging; [`Self::to_errno`]
/// maps it to the stable [`Errno`] the user-space scan
/// (`tairix_drvhost::store::scan_store`) records as the bundle's skip
/// reason.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DriverImageError {
    /// The path does not lie strictly within the store root the read was
    /// scoped to. The reader only ever reads driver bundles, never an
    /// arbitrary file (validate every input).
    OutsideStore,
    /// The path names a directory (or other non-file), not a regular file.
    NotAFile,
    /// The file is larger than [`MAX_DRIVER_IMAGE_LEN`].
    TooLarge,
    /// The driver reported a different byte count than its stated size
    /// between `stat` and `read` (a short read); the partial bytes are
    /// discarded.
    ShortRead,
    /// The backing root-volume VFS refused the operation (not found,
    /// permission denied, driver I/O fault, …).
    Vfs(VfsError),
}

impl DriverImageError {
    /// Map to the stable user/kernel [`Errno`].
    #[must_use]
    pub const fn to_errno(self) -> Errno {
        match self {
            // A read aimed outside the sanctioned store is denied, not
            // merely "not found": treat it as a permission refusal.
            Self::OutsideStore => Errno::PermissionDenied,
            Self::NotAFile => Errno::OutOfRange,
            Self::TooLarge => Errno::LengthOutOfRange,
            // A short read is an I/O-shaped failure; `abi-v1` has no
            // dedicated `EIO`, so it collapses onto `NotImplemented` as
            // `VfsError::Io` does.
            Self::ShortRead => Errno::NotImplemented,
            Self::Vfs(err) => err.to_errno(),
        }
    }
}

impl From<VfsError> for DriverImageError {
    fn from(err: VfsError) -> Self {
        Self::Vfs(err)
    }
}

/// `true` iff `path` names a node strictly *below* `store_root` (the store
/// directory itself, or any path outside it, is rejected).
fn path_within_store(store_root: &str, path: &str) -> bool {
    match path.strip_prefix(store_root) {
        Some(rest) => rest.starts_with('/') && rest.len() > 1,
        None => false,
    }
}

/// Reads driver-bundle images off the mounted root volume's
/// `/System/Drivers/` store.
///
/// [`enumerate_driver_store`] finds *which* bundle paths exist; this reader
/// fetches the *bytes* of a chosen bundle, so the user-space scan
/// (`tairix_drvhost::store::scan_store`) can parse and bind-decode it. The
/// reader is the byte-fetching half the scan's `ImageSource` seam needs;
/// the bin crate's `ImageSource` adapter (the one layer that may name
/// `drvhost`) delegates to it.
///
/// The root-backed VFS is built **once** at [`open`](Self::open) and reused
/// across every read (no per-read VFS construction),
/// mirroring [`enumerate_driver_store`]'s single walk.
///
/// Like that walk and [`crate::users::load_users_db`], every read runs
/// under the kernel's bootstrap identity (`uid 0`, no capabilities): a bundle is reachable only because its stored
/// record makes it readable to that identity, never through an ambient
/// bypass.
pub struct DriverImageReader {
    vfs: Vfs,
}

impl DriverImageReader {
    /// Build a reader whose root mount is backed by the mounted root
    /// volume's driver, sharing the one root-backed-VFS builder.
    ///
    /// # Errors
    ///
    /// The [`VfsError`] from the shared root-mount builder if the private
    /// root mount cannot be constructed.
    pub fn open() -> Result<Self, VfsError> {
        Ok(Self {
            vfs: crate::fs::root_backed_vfs()?,
        })
    }

    /// Read the bundle image at `path` from the mounted volume,
    /// **appending** its bytes to `buf`.
    ///
    /// `path` must be an absolute path strictly below `store_root` (the
    /// store root the path was enumerated under, typically one
    /// [`enumerate_driver_store`] returned with the same `store_root`). The
    /// read is bounded against [`MAX_DRIVER_IMAGE_LEN`] before a single
    /// byte is read and fails closed on any refusal,
    /// leaving `buf` unchanged from its entry length on error.
    ///
    /// Appending (rather than overwriting) matches the
    /// `tairix_drvhost::ImageSource` contract the bin-crate adapter
    /// fulfils.
    ///
    /// # Errors
    ///
    /// The [`DriverImageError`] naming the first check that refused.
    pub fn read_image<F>(
        &self,
        fs: &mut F,
        store_root: &str,
        path: &str,
        buf: &mut Vec<u8>,
    ) -> Result<(), DriverImageError>
    where
        F: FilesystemRead + FilesystemSecurity + ?Sized,
    {
        if !path_within_store(store_root, path) {
            return Err(DriverImageError::OutsideStore);
        }

        let caps = CapabilitySet::empty();
        let cred = bootstrap_credentials(&caps);
        let parsed = Path::parse(path)?;

        // Bound the bundle against the validation maximum before reading a
        // single byte.
        let info = self.vfs.stat_via_secured(&cred, &parsed, fs)?;
        if info.kind != NodeKind::RegularFile {
            return Err(DriverImageError::NotAFile);
        }
        if info.size > MAX_DRIVER_IMAGE_LEN as u64 {
            return Err(DriverImageError::TooLarge);
        }
        let size = usize::try_from(info.size).map_err(|_| DriverImageError::TooLarge)?;

        // Append into `buf`; unwind the reservation on any short read or
        // driver refusal so the buffer is unchanged on error.
        let start = buf.len();
        buf.resize(start + size, 0);
        match self
            .vfs
            .read_via_secured(&cred, &parsed, fs, 0, &mut buf[start..])
        {
            Ok(read) if read == size => Ok(()),
            Ok(_) => {
                buf.truncate(start);
                Err(DriverImageError::ShortRead)
            }
            Err(err) => {
                buf.truncate(start);
                Err(DriverImageError::from(err))
            }
        }
    }
}

fn audit_scan(audit: &dyn Sink, store_root: &str, drivers: usize, skipped: usize) {
    let mut drivers_buf = [0u8; 12];
    let mut skipped_buf = [0u8; 12];
    emit(
        audit,
        Level::Info,
        AuditEvent::DriverStoreScanned,
        &[
            Field {
                key: "path",
                value: tairix_log::FieldValue::Str(store_root),
            },
            Field {
                key: "drivers",
                value: tairix_log::FieldValue::Str(format_usize(drivers, &mut drivers_buf)),
            },
            Field {
                key: "skipped",
                value: tairix_log::FieldValue::Str(format_usize(skipped, &mut skipped_buf)),
            },
        ],
    );
}

#[cfg(test)]
#[path = "driver_store_tests.rs"]
mod tests;
