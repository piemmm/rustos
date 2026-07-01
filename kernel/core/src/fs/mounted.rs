//! The production [`FilesystemService`]: the `fs_*` syscalls served against
//! a mounted volume (`PREREQUISITES.md` P-A).
//!
//! The hollow [`NULL_FILESYSTEM`](super::service::NULL_FILESYSTEM) default
//! fails every `fs_*` syscall closed. This module is the real producer the
//! boot path installs once the disk is mounted: [`MountedFilesystemService`]
//! resolves the caller's **kernel-attested** identity into full VFS
//! [`Credentials`] and authorises every operation through the secured VFS.
//!
//! # Concurrent, caller-context, per-mount-serialised
//!
//! Each `fs_*` operation runs in the **calling task's own context**, directly
//! against the resolved mount, so N tasks drive N concurrent operations and a
//! task waiting on a slow device completion parks on *its own* block-driver
//! IRQ wait rather than behind a single global server. Operations on
//! *different* mounts proceed fully in parallel. Within one mount the
//! filesystem driver needs `&mut self` per operation and may **park** across a
//! block-device completion IRQ ([`rustos_abi::driver::block::Block::read_blocks`]
//! parks the caller), so the per-mount lock is a scheduler-blocking
//! [`SleepLock`] held across that park — never a `lib/sync` spin lock, which a
//! second contender would busy-spin on while the holder sleeps
//! (`docs/src/architecture/sync.md`). This is the architecture a future
//! async/multi-queue `Block` overlaps operations *within* a device on, with no
//! change above the driver.
//!
//! # Identity is kernel-attested, never caller-supplied
//!
//! The syscall handler supplies the caller's owning `uid` and effective
//! capability set, both read from the task's
//! [`rustos_kernel_sec::TaskCapabilities`] — never anything the caller passed.
//! This service resolves the caller's primary and supplementary **groups**
//! from the authoritative [`IdentityTable`] keyed by that uid (a frozen,
//! credential-free index — it carries no password material), then runs
//! `Vfs::*_via_secured` so every per-inode owner/mode/ACL/`required_cap` and
//! mount-flag check stays kernel-side and fails closed. A principal with no
//! account, or a call made before the identity table or the mount is
//! installed, is denied rather than served.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use rustos_abi::driver::filesystem::{
    FilesystemRead, FilesystemSecurity, FilesystemWrite, NodeKind as DriverNodeKind,
};
use rustos_abi::driver::DriverHandle;
use rustos_abi::sysinfo::MountRecord;
use rustos_abi::{CapabilityQuery, Errno, FileKind, FileStat, OpenFlags};
use rustos_kernel_sec::{GroupId, IdentityTable, UserId, UserRecord};
use rustos_sync::{OnceCell, SpinLock};

use crate::sleeplock::SleepLock;

use super::path::Path;
use super::perm::Credentials;
use super::service::FilesystemService;
use super::{Vfs, VfsError};

/// One backing filesystem driver registered in a [`LateFilesystem`],
/// addressed by the [`DriverHandle`] its mount carries in the VFS mount
/// table.
///
/// The driver needs `&mut self` per operation and may **park** across a
/// block-device completion IRQ, so it is serialised by a sleeping
/// [`SleepLock`] (never a spin lock — a second contender would busy-spin
/// while the holder sleeps). The lock is leaked to `'static` so the hot path
/// copies the reference out of the registry without holding the registry
/// lock across the (possibly parking) operation.
struct DriverEntry<F: 'static> {
    handle: u64,
    driver: &'static SleepLock<F>,
}

/// A set-once VFS policy layer plus a registry of backing filesystem drivers
/// the boot path installs after the disk(s) come online (mirrors
/// [`crate::users::LateUsersDb`]).
///
/// The syscall layer is built before any disk is mounted, so the handlers
/// hold a `&'static LateFilesystem` from boot; until the VFS is published and
/// the covering mount's driver is registered, every operation fails closed
/// with [`Errno::NotImplemented`] — identical to the hollow
/// [`NULL_FILESYSTEM`](super::service::NULL_FILESYSTEM).
///
/// One VFS owns the whole mount table; **several** backing volumes can be
/// registered (e.g. the read-only `/System` volume and the writable
/// `/System/Logs` subtree of the encrypted root volume). Each `fs_*`
/// operation resolves its path's covering mount, reads that mount's
/// [`DriverHandle`], and runs against the matching driver — so operations on
/// *different* volumes proceed in parallel (each behind its own
/// [`SleepLock`]) and a slow device never stalls an unrelated one.
///
/// `Sync` (through [`OnceCell`], [`SpinLock`], and the per-driver
/// [`SleepLock`]) so the single `&'static` instance is shared by the per-CPU
/// syscall handlers.
pub struct LateFilesystem<F: 'static> {
    /// The shared policy layer: absolute-path resolution, the mount table,
    /// and the per-inode permission model, set once when the layout is known.
    vfs: OnceCell<Vfs>,
    /// The backing drivers, keyed by mount [`DriverHandle`]. Appended to at
    /// boot as each volume comes online (rare); read on every `fs_*` call.
    /// The spin lock is held only for the tiny lookup/append, never across a
    /// filesystem operation (the `&'static SleepLock` reference is copied
    /// out first), so it never spins on a parked holder.
    drivers: SpinLock<Vec<DriverEntry<F>>>,
}

/// An install/registration was refused because the target is already set.
///
/// The VFS cell is immutable after the first successful
/// [`install_vfs`](LateFilesystem::install_vfs), and a [`DriverHandle`] is
/// registered at most once, so neither the live VFS nor a live driver can be
/// replaced by a later code path.
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq)]
pub struct FilesystemAlreadyInstalled;

impl<F: 'static> LateFilesystem<F> {
    /// Construct an empty cell. `const` so a boot path can place it in a
    /// `static` and hand `&LATE_FILESYSTEM` to the handler builder.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            vfs: OnceCell::new(),
            drivers: SpinLock::new(Vec::new()),
        }
    }

    /// Publish the shared VFS policy layer exactly once.
    ///
    /// The VFS mount table already names the [`DriverHandle`] of every
    /// backing volume; [`register`](Self::register) attaches each live driver
    /// to its handle (before or after this call — a request for a not-yet-
    /// registered handle fails closed until it lands).
    ///
    /// # Errors
    ///
    /// [`FilesystemAlreadyInstalled`] if a VFS is already installed.
    pub fn install_vfs(&self, vfs: Vfs) -> Result<(), FilesystemAlreadyInstalled> {
        self.vfs.set(vfs).map_err(|_| FilesystemAlreadyInstalled)
    }

    /// Register the live driver backing the mount addressed by `handle`.
    ///
    /// The driver is wrapped in a [`SleepLock`] and leaked to `'static` (it
    /// lives for the rest of the kernel's life, like every other boot-leaked
    /// kernel state), so the hot path can copy the reference out of the
    /// registry without holding the registry lock across a parking operation.
    ///
    /// # Errors
    ///
    /// [`FilesystemAlreadyInstalled`] if `handle` is already registered — a
    /// driver is bound to its handle exactly once (fail closed; never a
    /// silent re-bind).
    pub fn register(
        &self,
        handle: DriverHandle,
        driver: F,
    ) -> Result<(), FilesystemAlreadyInstalled> {
        let handle = handle.as_u64();
        let mut drivers = self.drivers.lock();
        if drivers.iter().any(|e| e.handle == handle) {
            return Err(FilesystemAlreadyInstalled);
        }
        let driver: &'static SleepLock<F> = Box::leak(Box::new(SleepLock::new(driver)));
        drivers.push(DriverEntry { handle, driver });
        Ok(())
    }

    /// Whether the shared VFS has been installed.
    #[must_use]
    pub fn is_installed(&self) -> bool {
        self.vfs.is_initialised()
    }

    /// The installed VFS, or [`Errno::NotImplemented`] before one is
    /// published (fail closed — a kernel with no mounted volume serves no
    /// `fs_*` syscall).
    fn vfs(&self) -> Result<&Vfs, Errno> {
        match self.vfs.get() {
            Ok(Some(vfs)) => Ok(vfs),
            _ => Err(Errno::NotImplemented),
        }
    }

    /// The driver registered for `handle`, or [`Errno::NotImplemented`] when
    /// none is (fail closed — a mount whose backing volume is not yet online,
    /// or has no driver, serves no operation, never a silent fallback).
    fn driver(&self, handle: DriverHandle) -> Result<&'static SleepLock<F>, Errno> {
        let handle = handle.as_u64();
        let drivers = self.drivers.lock();
        drivers
            .iter()
            .find(|e| e.handle == handle)
            .map(|e| e.driver)
            .ok_or(Errno::NotImplemented)
    }

    /// Every registered driver, for a whole-system `sync`.
    fn all_drivers(&self) -> Vec<&'static SleepLock<F>> {
        self.drivers.lock().iter().map(|e| e.driver).collect()
    }
}

impl<F: 'static> Default for LateFilesystem<F> {
    fn default() -> Self {
        Self::new()
    }
}

/// A set-once cell holding the authoritative user/group identity table the
/// `fs_*` path resolves caller groups against (mirrors
/// [`crate::users::LateUsersDb`]).
///
/// The on-disk accounts are read only after the encrypted root is unlocked,
/// past the point where the handler set is built, so the service holds a
/// `&'static LateIdentity` from boot and the trusted unlock step installs the
/// verified [`IdentityTable`] once it exists. The table is **credential-free**
/// (it carries the uid → group/capability mapping the VFS needs, never the
/// salted password records the `users_db_read` text path serves), so a
/// long-lived `&'static` copy leaks no secret.
///
/// Until [`install`](Self::install), resolving a uid fails closed with
/// [`Errno::NotImplemented`]; an attested uid with no account is denied with
/// [`Errno::PermissionDenied`]. Neither path ever invents a principal.
pub struct LateIdentity {
    table: OnceCell<IdentityTable>,
}

/// An identity-table install was refused because one is already installed.
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq)]
pub struct IdentityAlreadyInstalled;

impl LateIdentity {
    /// Construct an empty cell. `const` so a boot path can place it in a
    /// `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            table: OnceCell::new(),
        }
    }

    /// Publish the verified identity table exactly once.
    ///
    /// # Errors
    ///
    /// [`IdentityAlreadyInstalled`] if a table is already installed — the cell
    /// is immutable after the first successful install, so no later code path
    /// can swap the authoritative identity table.
    pub fn install(&self, table: IdentityTable) -> Result<(), IdentityAlreadyInstalled> {
        self.table.set(table).map_err(|_| IdentityAlreadyInstalled)
    }

    /// Whether an identity table has been installed.
    #[must_use]
    pub fn is_installed(&self) -> bool {
        self.table.is_initialised()
    }

    /// Resolve the record for the attested `uid`.
    ///
    /// # Errors
    ///
    /// * [`Errno::NotImplemented`] before a table is installed (the disk has
    ///   not been unlocked/read yet) — fail closed, never resolve.
    /// * [`Errno::PermissionDenied`] when the table holds no account for
    ///   `uid` — an unknown principal is denied, never granted a guessed
    ///   identity, and the refusal does not distinguish "unknown uid" so it
    ///   cannot be used to probe for valid ids.
    fn resolve(&self, uid: u32) -> Result<&UserRecord, Errno> {
        match self.table.get() {
            Ok(Some(table)) => table.user(UserId(uid)).map_err(|_| Errno::PermissionDenied),
            _ => Err(Errno::NotImplemented),
        }
    }

    /// Resolve the attested group credential (primary group and supplementary
    /// groups) for `uid` from the installed identity table.
    ///
    /// This is the spawn-as-user resolver: when a privileged spawner switches
    /// a child into a target user, the kernel snapshots that user's full group
    /// set onto the child's capability record from the table it vouches for,
    /// so the child's later filesystem checks run under an authoritative,
    /// caller-independent credential. The returned set is owned (a snapshot),
    /// not a borrow into the table, so it can be stored on the task.
    ///
    /// # Errors
    ///
    /// Fails closed exactly as the internal group resolution does:
    /// [`Errno::NotImplemented`] before a table is installed and
    /// [`Errno::PermissionDenied`] for a uid with no account, so a switch to
    /// an unknown or unresolvable user never invents a credential.
    pub fn resolve_credential(&self, uid: u32) -> Result<(GroupId, Vec<GroupId>), Errno> {
        let record = self.resolve(uid)?;
        Ok((record.primary_gid, record.supplementary_gids.clone()))
    }
}

impl Default for LateIdentity {
    fn default() -> Self {
        Self::new()
    }
}

/// The production [`FilesystemService`]: serves the `fs_*` syscalls against a
/// late-installed mount, resolving caller groups from a late-installed
/// authoritative identity table.
///
/// Holds only two `&'static` borrows — the mount cell and the identity cell —
/// and adds no authority of its own; every check stays kernel-side in the
/// secured VFS and fails closed. The trait signature is unchanged from the
/// landed handlers, so the handler logic and its mock-backed tests stand
/// as-is — only this production impl and its boot wiring are new.
pub struct MountedFilesystemService<F: 'static> {
    /// The mounted volume the operations run against.
    mount: &'static LateFilesystem<F>,
    /// The authoritative identity table caller groups are resolved against.
    identity: &'static LateIdentity,
}

impl<F: 'static> MountedFilesystemService<F> {
    /// Build the service over the boot-installed mount and identity cells.
    #[must_use]
    pub const fn new(mount: &'static LateFilesystem<F>, identity: &'static LateIdentity) -> Self {
        Self { mount, identity }
    }
}

impl<F> MountedFilesystemService<F>
where
    F: FilesystemRead + FilesystemWrite + FilesystemSecurity + Send + 'static,
{
    /// Resolve the mount and the caller's record, parse `path`, and run `op`
    /// against the secured VFS under the per-mount lock.
    ///
    /// The caller's full [`Credentials`] are built here — `uid`/`caps` are the
    /// kernel-attested values the handler supplied, and the groups come from
    /// the authoritative identity table — so an operation never sees a
    /// caller-supplied identity. The lock is held for the whole operation,
    /// including any device-completion park, then released.
    fn with_secured<R>(
        &self,
        uid: u32,
        caps: &dyn CapabilityQuery,
        path: &str,
        op: impl FnOnce(&Vfs, &mut F, &Credentials<'_>, &Path) -> Result<R, VfsError>,
    ) -> Result<R, Errno> {
        let vfs = self.mount.vfs()?;
        let record = self.identity.resolve(uid)?;
        let path = Path::parse(path).map_err(VfsError::to_errno)?;
        // Route to the driver backing the path's covering mount. A path under
        // a backing-less mount (the in-RAM default-layout dirs) has no driver
        // to delegate to — its delegated op would itself fail `NotFound`, so
        // it fails closed the same way here, never against a guessed volume.
        let driver = self.resolve_driver(vfs, &path)?;
        let cred = Credentials {
            uid: UserId(uid),
            gid: record.primary_gid,
            supplementary_gids: &record.supplementary_gids,
            caps,
        };
        let mut fs = driver.lock();
        op(vfs, &mut fs, &cred, &path).map_err(VfsError::to_errno)
    }

    /// The driver backing the mount covering `path`, locked by the caller.
    ///
    /// Resolves the covering mount in the shared VFS, reads its
    /// [`DriverHandle`], and returns the registered driver for it. A
    /// backing-less covering mount yields [`VfsError::NotFound`] (no volume
    /// to delegate to); a backed mount whose driver is not yet registered
    /// yields [`Errno::NotImplemented`] — both fail closed.
    fn resolve_driver(&self, vfs: &Vfs, path: &Path) -> Result<&'static SleepLock<F>, Errno> {
        let handle = vfs
            .mounts()
            .resolve(path)
            .backing()
            .ok_or_else(|| VfsError::NotFound.to_errno())?;
        self.mount.driver(handle)
    }
}

/// Map a driver structural node kind to the userland [`FileKind`] the
/// `fs_*` contract exposes.
fn file_kind(kind: DriverNodeKind) -> FileKind {
    match kind {
        DriverNodeKind::Directory => FileKind::Directory,
        DriverNodeKind::RegularFile => FileKind::Regular,
    }
}

impl<F> FilesystemService for MountedFilesystemService<F>
where
    F: FilesystemRead + FilesystemWrite + FilesystemSecurity + Send + 'static,
{
    fn open(
        &self,
        uid: u32,
        caps: &dyn CapabilityQuery,
        path: &str,
        flags: OpenFlags,
    ) -> Result<(), Errno> {
        self.with_secured(uid, caps, path, |vfs, fs, cred, path| {
            match vfs.stat_via_secured(cred, path, fs) {
                Ok(info) => {
                    // An exclusive create demands the path not already exist.
                    if flags.contains(OpenFlags::CREATE) && flags.contains(OpenFlags::EXCLUSIVE) {
                        return Err(VfsError::AlreadyExists);
                    }
                    // A directory open must name a directory; a byte-access
                    // open must not name one.
                    if flags.contains(OpenFlags::DIRECTORY) {
                        if info.kind != DriverNodeKind::Directory {
                            return Err(VfsError::NotADirectory);
                        }
                    } else if info.kind == DriverNodeKind::Directory
                        && (flags.is_read() || flags.is_write())
                    {
                        return Err(VfsError::IsADirectory);
                    }
                    // Truncate-on-open zeroes the file; it requires write
                    // access (enforced at `OpenFlags::from_bits`) and is
                    // authorised by the secured truncate.
                    if flags.contains(OpenFlags::TRUNCATE) {
                        vfs.truncate_via_secured(cred, path, fs, 0)?;
                    }
                    Ok(())
                }
                // A missing path is created only when asked, and `open` only
                // ever creates a regular file (directories are made by
                // `mkdir`); a directory-typed create is a contradiction and
                // fails closed.
                Err(VfsError::NotFound) if flags.contains(OpenFlags::CREATE) => {
                    if flags.contains(OpenFlags::DIRECTORY) {
                        return Err(VfsError::NotADirectory);
                    }
                    vfs.create_via_secured(cred, path, fs)
                }
                Err(err) => Err(err),
            }
        })
    }

    fn read(
        &self,
        uid: u32,
        caps: &dyn CapabilityQuery,
        path: &str,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, Errno> {
        self.with_secured(uid, caps, path, |vfs, fs, cred, path| {
            vfs.read_via_secured(cred, path, fs, offset, buf)
        })
    }

    fn write(
        &self,
        uid: u32,
        caps: &dyn CapabilityQuery,
        path: &str,
        offset: u64,
        append: bool,
        data: &[u8],
    ) -> Result<usize, Errno> {
        self.with_secured(uid, caps, path, |vfs, fs, cred, path| {
            // An append write ignores the supplied offset and writes at the
            // current end of file (the journal-append posture), resolved under
            // the same lock so the size cannot change before the write.
            let offset = if append {
                vfs.stat_via_secured(cred, path, fs)?.size
            } else {
                offset
            };
            vfs.write_via_secured(cred, path, fs, offset, data)
        })
    }

    fn readdir(
        &self,
        uid: u32,
        caps: &dyn CapabilityQuery,
        path: &str,
    ) -> Result<Vec<(FileKind, String)>, Errno> {
        self.with_secured(uid, caps, path, |vfs, fs, cred, path| {
            let names = vfs.list_via_secured(cred, path, fs)?;
            let mut entries = Vec::with_capacity(names.len());
            for name in names {
                // Resolve each child's kind under the same authorised
                // traversal; a child that vanished between the listing and the
                // stat fails the whole call closed rather than reporting a
                // guessed kind.
                let mut child = path_str(path);
                if !child.ends_with('/') {
                    child.push('/');
                }
                child.push_str(&name);
                let child = Path::parse(&child)?;
                let info = vfs.stat_via_secured(cred, &child, fs)?;
                entries.push((file_kind(info.kind), name));
            }
            Ok(entries)
        })
    }

    fn stat(&self, uid: u32, caps: &dyn CapabilityQuery, path: &str) -> Result<FileStat, Errno> {
        self.with_secured(uid, caps, path, |vfs, fs, cred, path| {
            let info = vfs.stat_via_secured(cred, path, fs)?;
            Ok(FileStat {
                kind: file_kind(info.kind),
                size: info.size,
                mode: u32::from(info.meta.mode.bits()),
                uid: info.meta.owner.0,
                gid: info.meta.group.0,
            })
        })
    }

    fn truncate(
        &self,
        uid: u32,
        caps: &dyn CapabilityQuery,
        path: &str,
        size: u64,
    ) -> Result<(), Errno> {
        self.with_secured(uid, caps, path, |vfs, fs, cred, path| {
            vfs.truncate_via_secured(cred, path, fs, size)
        })
    }

    fn sync(&self, _uid: u32, _caps: &dyn CapabilityQuery) -> Result<(), Errno> {
        // Flush *every* mounted volume's buffered writes to its backing
        // device. `sync` carries no per-inode (or per-volume) target, so it
        // is whole-system; gated by `CAP_FS_ACCESS` at dispatch. A
        // read-through driver flushes as a no-op. Fail closed before any
        // volume is online (no VFS yet), and on the first device fault.
        self.mount.vfs()?;
        for driver in self.mount.all_drivers() {
            // `abi-v1` has no dedicated I/O errno; a device fault collapses
            // onto the same code the VFS uses for a driver fault.
            driver.lock().flush().map_err(|_| VfsError::Io.to_errno())?;
        }
        Ok(())
    }

    fn mkdir(&self, uid: u32, caps: &dyn CapabilityQuery, path: &str) -> Result<(), Errno> {
        self.with_secured(uid, caps, path, |vfs, fs, cred, path| {
            vfs.mkdir_via_secured(cred, path, fs)
        })
    }

    fn unlink(&self, uid: u32, caps: &dyn CapabilityQuery, path: &str) -> Result<(), Errno> {
        self.with_secured(uid, caps, path, |vfs, fs, cred, path| {
            vfs.remove_via_secured(cred, path, fs)
        })
    }

    fn rename(
        &self,
        uid: u32,
        caps: &dyn CapabilityQuery,
        src: &str,
        dst: &str,
    ) -> Result<(), Errno> {
        // Rename names two paths, so it resolves both under one lock rather
        // than through the single-path `with_secured`. Identity is attested
        // exactly as elsewhere; both paths must lie under the same mount
        // (`rename_via_secured` refuses a cross-mount move), so the source
        // path's covering-mount driver serves the whole operation.
        let vfs = self.mount.vfs()?;
        let record = self.identity.resolve(uid)?;
        let src = Path::parse(src).map_err(VfsError::to_errno)?;
        let dst = Path::parse(dst).map_err(VfsError::to_errno)?;
        let driver = self.resolve_driver(vfs, &src)?;
        let cred = Credentials {
            uid: UserId(uid),
            gid: record.primary_gid,
            supplementary_gids: &record.supplementary_gids,
            caps,
        };
        let mut fs = driver.lock();
        vfs.rename_via_secured(&cred, &src, &dst, &mut *fs)
            .map_err(VfsError::to_errno)
    }

    fn mount_snapshot(&self) -> Vec<MountRecord> {
        // Before any volume is online there is no VFS; report "no mounts"
        // truthfully rather than fabricating one (fail closed).
        let Ok(vfs) = self.mount.vfs() else {
            return Vec::new();
        };
        vfs.mounts()
            .iter()
            .filter_map(|mount| {
                // The mount table records the mount *point* (target) and its
                // permission flags authoritatively; it does not yet model a
                // backing-device name or a filesystem-type string, so those
                // are reported empty rather than guessed. `MountRecord::new`
                // only fails on an over-long field, which a validated VFS
                // `Path` cannot produce; a defensive `ok()` drops any such
                // entry rather than panicking.
                MountRecord::new(b"", path_str(mount.path()).as_bytes(), b"", mount.flags()).ok()
            })
            .collect()
    }
}

/// Reconstruct the absolute path string of `path` for building a child path.
///
/// The VFS [`Path`] stores validated components; joining a child for the
/// per-entry stat in `readdir` needs the textual parent. The root is the bare
/// `"/"`; a deeper path is `"/" + components.join("/")`.
fn path_str(path: &Path) -> String {
    let mut out = String::from("/");
    let mut first = true;
    for component in path.components() {
        if !first {
            out.push('/');
        }
        out.push_str(component);
        first = false;
    }
    out
}

#[cfg(test)]
#[path = "mounted_tests.rs"]
mod tests;
