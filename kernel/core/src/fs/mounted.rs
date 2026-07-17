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

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use rustos_abi::driver::filesystem::{
    FilesystemAttrsProvider, FilesystemRead, FilesystemSecurity, FilesystemStats, FilesystemWrite,
    NodeKind as DriverNodeKind, VolumeStats,
};
use rustos_abi::driver::DriverHandle;
use rustos_abi::sysinfo::{MountAvailability, MountRecord};
use rustos_abi::time::Time64;
use rustos_abi::{
    CapabilityQuery, Errno, FileKind, FileStat, OpenFlags, UnlinkFlags, FS_MODE_MASK,
};
use rustos_caps::CapabilitySet;
use rustos_kernel_sec::{GroupId, IdentityTable, UserId};
use rustos_sync::{OnceCell, RwLock, SpinLock};

use crate::sleeplock::SleepLock;

use super::path::Path;
use super::perm::Credentials;
use super::service::{FilesystemService, ReaddirEntry};
use super::{Vfs, VfsError};

/// One backing filesystem driver registered in a [`LateFilesystem`],
/// addressed by the [`DriverHandle`] its mount carries in the VFS mount
/// table.
///
/// The driver needs `&mut self` per operation and may **park** across a
/// block-device completion IRQ, so it is serialised by a sleeping
/// [`SleepLock`] (never a spin lock — a second contender would busy-spin
/// while the holder sleeps). The lock is shared by [`Arc`] so the hot path
/// clones the handle out of the registry without holding the registry
/// lock across the (possibly parking) operation — and so a runtime
/// [`unregister`](LateFilesystem::unregister) (a hotplug volume detach)
/// drops the registry's reference while any in-flight operation keeps the
/// driver alive through its own clone: no leak per detach, no
/// use-after-free.
struct DriverEntry<F: 'static> {
    handle: u64,
    driver: Arc<SleepLock<F>>,
    /// The backing volume's name (partition label / volume identity), as the
    /// mount snapshot reports it. Registration-time facts, not driver state:
    /// the registrar names what it mounted.
    source: String,
    /// The driver's filesystem-type name (`arxfs`, …).
    fstype: String,
    /// The volume's stable published identity (the same 16 bytes the volume
    /// forest publishes for `id::` paths), or all-zero when the registrar
    /// published none. A registration-time fact like `source`, reported by
    /// the mount snapshot so the unmount tooling can name the volume it
    /// detaches.
    volume_id: [u8; 16],
    /// Whether the backing volume is live. Flipped by
    /// [`LateFilesystem::set_availability`] when a surprise removal parks
    /// the volume behind its fail-closed stand-in, so the mount snapshot
    /// never shows a vanished volume as healthy.
    availability: MountAvailability,
}

/// One registered volume's snapshot facts, as [`LateFilesystem::entry`]
/// reports them to the mount snapshot: the registration names, the shared
/// driver lock, the volume's published identity, and its availability.
struct SnapshotEntry<F: 'static> {
    source: String,
    fstype: String,
    driver: Arc<SleepLock<F>>,
    volume_id: [u8; 16],
    availability: MountAvailability,
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

    /// Register the live driver backing the mount addressed by `handle`,
    /// naming the backing volume (`source`) and the driver's filesystem type
    /// (`fstype`) for the mount snapshot.
    ///
    /// The driver is wrapped in a [`SleepLock`] behind an [`Arc`], so the
    /// hot path can clone the handle out of the registry without holding
    /// the registry lock across a parking operation, and a runtime
    /// [`unregister`](Self::unregister) can drop the registry's reference
    /// while in-flight operations finish on their own clones.
    ///
    /// Returns a clone of the shared lock so a kernel-internal consumer that
    /// must write the same volume (the `CAP_USER_ADMIN`
    /// account-administration engine's storage) can share the **one** live
    /// driver instance: a volume has exactly one writer, and every mutation
    /// serialises through this lock — a second independent driver over the
    /// same device would corrupt its copy-on-write allocation state.
    ///
    /// # Errors
    ///
    /// [`FilesystemAlreadyInstalled`] if `handle` is already registered — a
    /// driver is bound to its handle exactly once while registered (fail
    /// closed; never a silent re-bind). A handle freed by
    /// [`unregister`](Self::unregister) may be reused by a later attach.
    pub fn register(
        &self,
        handle: DriverHandle,
        driver: F,
        source: &str,
        fstype: &str,
        volume_id: [u8; 16],
    ) -> Result<Arc<SleepLock<F>>, FilesystemAlreadyInstalled> {
        let handle = handle.as_u64();
        let mut drivers = self.drivers.lock();
        if drivers.iter().any(|e| e.handle == handle) {
            return Err(FilesystemAlreadyInstalled);
        }
        let driver = Arc::new(SleepLock::new(driver));
        drivers.push(DriverEntry {
            handle,
            driver: Arc::clone(&driver),
            source: String::from(source),
            fstype: String::from(fstype),
            volume_id,
            availability: MountAvailability::Available,
        });
        Ok(driver)
    }

    /// Record the availability of the volume registered for `handle`, so
    /// the mount snapshot reports a surprise-removed volume as unavailable
    /// rather than healthy.
    ///
    /// # Errors
    ///
    /// [`Errno::NotImplemented`] when `handle` names no registered volume
    /// (fail closed — nothing is marked).
    pub fn set_availability(
        &self,
        handle: DriverHandle,
        availability: MountAvailability,
    ) -> Result<(), Errno> {
        let handle = handle.as_u64();
        let mut drivers = self.drivers.lock();
        let entry = drivers
            .iter_mut()
            .find(|e| e.handle == handle)
            .ok_or(Errno::NotImplemented)?;
        entry.availability = availability;
        Ok(())
    }

    /// Withdraw the driver registered for `handle` (a runtime volume
    /// detach), returning the registry's shared handle so the caller can
    /// flush and drop it.
    ///
    /// Operations already in flight hold their own [`Arc`] clones and
    /// finish safely; every later resolution of `handle` fails closed
    /// [`Errno::NotImplemented`] exactly as before the driver was
    /// registered. Fails closed: an unknown handle removes nothing.
    pub fn unregister(&self, handle: DriverHandle) -> Option<Arc<SleepLock<F>>> {
        let handle = handle.as_u64();
        let mut drivers = self.drivers.lock();
        let pos = drivers.iter().position(|e| e.handle == handle)?;
        Some(drivers.remove(pos).driver)
    }

    /// Whether the shared VFS has been installed.
    #[must_use]
    pub fn is_installed(&self) -> bool {
        self.vfs.is_initialised()
    }

    /// The installed VFS, or [`Errno::NotImplemented`] before one is
    /// published (fail closed — a kernel with no mounted volume serves no
    /// `fs_*` syscall). Public for the runtime volume attach/detach
    /// service, which adds and retracts mounts through the one live mount
    /// table.
    pub fn vfs(&self) -> Result<&Vfs, Errno> {
        match self.vfs.get() {
            Ok(Some(vfs)) => Ok(vfs),
            _ => Err(Errno::NotImplemented),
        }
    }

    /// The driver registered for `handle`, or [`Errno::NotImplemented`] when
    /// none is (fail closed — a mount whose backing volume is not yet online,
    /// or has no driver, serves no operation, never a silent fallback).
    /// Public for the runtime volume detach path, which flushes exactly the
    /// departing volume before retracting it.
    pub fn driver(&self, handle: DriverHandle) -> Result<Arc<SleepLock<F>>, Errno> {
        let handle = handle.as_u64();
        let drivers = self.drivers.lock();
        drivers
            .iter()
            .find(|e| e.handle == handle)
            .map(|e| Arc::clone(&e.driver))
            .ok_or(Errno::NotImplemented)
    }

    /// Every registered driver, for a whole-system `sync`.
    fn all_drivers(&self) -> Vec<Arc<SleepLock<F>>> {
        self.drivers
            .lock()
            .iter()
            .map(|e| Arc::clone(&e.driver))
            .collect()
    }

    /// The registered driver for `handle` together with its registration
    /// facts (names, volume identity, availability), for the mount
    /// snapshot. `None` when the backing volume is not yet online — the
    /// caller reports the mount without names or usage rather than
    /// guessing.
    fn entry(&self, handle: DriverHandle) -> Option<SnapshotEntry<F>> {
        let handle = handle.as_u64();
        let drivers = self.drivers.lock();
        drivers
            .iter()
            .find(|e| e.handle == handle)
            .map(|e| SnapshotEntry {
                source: e.source.clone(),
                fstype: e.fstype.clone(),
                driver: Arc::clone(&e.driver),
                volume_id: e.volume_id,
                availability: e.availability,
            })
    }
}

impl<F: 'static> Default for LateFilesystem<F> {
    fn default() -> Self {
        Self::new()
    }
}

/// A cell holding the authoritative user/group identity table the `fs_*`
/// path resolves caller groups against (mirrors
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
/// [`install`](Self::install) is set-once: the boot unlock publishes the
/// first table and every later install is refused. The only path that
/// changes the table afterwards is [`replace`](Self::replace), called
/// exclusively by the `CAP_USER_ADMIN` admin engine after it has
/// validated, re-verified, and persisted an edited account database
/// (`plans/CAPABILITY_USE.md` CU4) — an edit binds at the next
/// resolution/spawn; running tasks keep the credentials they were
/// admitted with.
///
/// Until [`install`](Self::install), resolving a uid fails closed with
/// [`Errno::NotImplemented`]; an attested uid with no account is denied with
/// [`Errno::PermissionDenied`]. Neither path ever invents a principal.
pub struct LateIdentity {
    table: RwLock<Option<IdentityTable>>,
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
            table: RwLock::new(None),
        }
    }

    /// Publish the verified identity table exactly once.
    ///
    /// # Errors
    ///
    /// [`IdentityAlreadyInstalled`] if a table is already installed — the
    /// boot unlock is the only path that may publish the *first* table;
    /// later changes go through the audited [`replace`](Self::replace)
    /// path alone.
    pub fn install(&self, table: IdentityTable) -> Result<(), IdentityAlreadyInstalled> {
        let mut held = self.table.write();
        if held.is_some() {
            return Err(IdentityAlreadyInstalled);
        }
        *held = Some(table);
        Ok(())
    }

    /// Replace the installed table with a re-verified one
    /// (`plans/CAPABILITY_USE.md` CU4).
    ///
    /// Called exclusively by the `CAP_USER_ADMIN` admin engine after the
    /// edited databases passed the same verifying build as the boot load;
    /// never a user-reachable install path. An edit binds at the next
    /// resolution (the next spawn/login); running tasks keep the
    /// credentials they were admitted with.
    ///
    /// # Errors
    ///
    /// [`Errno::NotImplemented`] when no table has been installed yet: a
    /// replacement can never create the first table (fail closed).
    pub fn replace(&self, table: IdentityTable) -> Result<(), Errno> {
        let mut held = self.table.write();
        if held.is_none() {
            return Err(Errno::NotImplemented);
        }
        *held = Some(table);
        Ok(())
    }

    /// Whether an identity table has been installed.
    #[must_use]
    pub fn is_installed(&self) -> bool {
        self.table.read().is_some()
    }

    /// Resolve the attested `uid`'s group credential as an owned snapshot
    /// (primary gid, supplementary gids).
    ///
    /// Owned rather than borrowed so no read borrow is held across a
    /// filesystem operation (which may park on device completion) while
    /// the table stays replaceable underneath.
    ///
    /// The **system principal** (`uid 0`) is kernel-defined, not
    /// database-defined: it exists before any account table can be read
    /// (PID 1 and the boot services must load their store bundles off the
    /// read-only `/System` volume before the encrypted root is unlocked)
    /// and on an installer image no table ever defines it. It therefore
    /// resolves to the same capability-less bootstrap identity the boot
    /// readers use (`gid 0`, no supplementary groups) whenever the table is
    /// absent or holds no `uid 0` record; a table record for `uid 0` (the
    /// compiled-in `system` account) wins when present. The fallback
    /// grants no ambient power: every per-inode owner/mode/ACL and
    /// mount-flag check still applies, and `uid 0` tasks exist only through
    /// kernel-attested spawn.
    ///
    /// # Errors
    ///
    /// * [`Errno::NotImplemented`] for a non-zero `uid` before a table is
    ///   installed (the disk has not been unlocked/read yet) — fail closed,
    ///   never resolve.
    /// * [`Errno::PermissionDenied`] when the table holds no account for a
    ///   non-zero `uid` — an unknown principal is denied, never granted a
    ///   guessed identity, and the refusal does not distinguish "unknown
    ///   uid" so it cannot be used to probe for valid ids.
    fn resolve_groups(&self, uid: u32) -> Result<(GroupId, Vec<GroupId>), Errno> {
        match &*self.table.read() {
            Some(table) => match table.user(UserId(uid)) {
                Ok(record) => Ok((record.primary_gid, record.supplementary_gids.clone())),
                Err(_) if uid == 0 => Ok((GroupId(0), Vec::new())),
                Err(_) => Err(Errno::PermissionDenied),
            },
            None if uid == 0 => Ok((GroupId(0), Vec::new())),
            None => Err(Errno::NotImplemented),
        }
    }

    /// Resolve the attested credential — primary group, supplementary
    /// groups, and the account's capability ceiling — for `uid` from the
    /// installed identity table.
    ///
    /// This is the spawn-as-user resolver: when a privileged spawner switches
    /// a child into a target user, the kernel snapshots that user's full group
    /// set **and** its `capability_grants` ceiling onto the child's capability
    /// record from the table it vouches for, so the child's later filesystem
    /// checks run under an authoritative, caller-independent credential and
    /// its effective capability set is derived as `manifest ∩ ceiling`
    /// (`plans/CAPABILITY_USE.md` CU1). The returned values are owned (a
    /// snapshot), not a borrow into the table, so they can be stored on the
    /// task.
    ///
    /// # Errors
    ///
    /// Fails closed exactly as the internal group resolution does:
    /// [`Errno::NotImplemented`] before a table is installed and
    /// [`Errno::PermissionDenied`] for a uid with no account, so a switch to
    /// an unknown or unresolvable user never invents a credential.
    pub fn resolve_credential(
        &self,
        uid: u32,
    ) -> Result<(GroupId, Vec<GroupId>, CapabilitySet), Errno> {
        match &*self.table.read() {
            Some(table) => {
                let record = table
                    .user(UserId(uid))
                    .map_err(|_| Errno::PermissionDenied)?;
                Ok((
                    record.primary_gid,
                    record.supplementary_gids.clone(),
                    record.capability_grants,
                ))
            }
            None => Err(Errno::NotImplemented),
        }
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
    F: FilesystemRead + FilesystemWrite + FilesystemSecurity + FilesystemStats + Send + 'static,
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
        // An owned snapshot, so no identity-table borrow is held across the
        // operation (which may park on device completion) while the table
        // stays replaceable underneath.
        let (gid, supplementary_gids) = self.identity.resolve_groups(uid)?;
        let path = Path::parse(path).map_err(VfsError::to_errno)?;
        // Route to the driver backing the path's covering mount. A path under
        // a backing-less mount (the in-RAM default-layout dirs) has no driver
        // to delegate to — its delegated op would itself fail `NotFound`, so
        // it fails closed the same way here, never against a guessed volume.
        let driver = self.resolve_driver(vfs, &path)?;
        let cred = Credentials {
            uid: UserId(uid),
            gid,
            supplementary_gids: &supplementary_gids,
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
    fn resolve_driver(&self, vfs: &Vfs, path: &Path) -> Result<Arc<SleepLock<F>>, Errno> {
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
    F: FilesystemRead
        + FilesystemWrite
        + FilesystemSecurity
        + FilesystemStats
        + FilesystemAttrsProvider
        + Send
        + 'static,
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
    ) -> Result<Vec<ReaddirEntry>, Errno> {
        self.with_secured(uid, caps, path, |vfs, fs, cred, path| {
            // Each entry's kind and sizes come from the listing driver
            // itself, never from a per-child path re-resolution: a child
            // path can be covered by a *different* mount (the read-only
            // `/System` volume's own `Logs`/`Settings` beneath the writable
            // exceptions), re-resolving it here would judge it against the
            // wrong volume and fail the whole listing closed, and each
            // re-resolution would repeat the child's full walk.
            let entries = vfs.list_via_secured(cred, path, fs)?;
            let mut out: Vec<ReaddirEntry> = entries
                .into_iter()
                .map(|(info, modified, name)| ReaddirEntry {
                    kind: file_kind(info.kind),
                    size: info.size,
                    allocated: info.allocated,
                    modified,
                    name,
                })
                .collect();
            // A covered mount point is part of its parent's listing even
            // when the parent volume holds no node of that name — the
            // runtime `/Storage/<name>` mounts, i.e. the `Storage:` catalog
            // enumeration (drives.md §15). A same-named node the parent
            // volume *does* hold already listed above and is not repeated.
            // The merged entry is structural: a mount point is a directory
            // by construction, and no per-node stamp is reachable through
            // the parent volume, so it carries the same `UNIX_EPOCH` stamp
            // any stampless backing reports.
            let mounts = vfs.mounts();
            for mount in mounts.direct_children(path) {
                let Some(name) = mount.path().components().last() else {
                    continue;
                };
                if out.iter().any(|entry| entry.name == *name) {
                    continue;
                }
                out.push(ReaddirEntry {
                    kind: FileKind::Directory,
                    size: 0,
                    allocated: 0,
                    modified: Time64::UNIX_EPOCH,
                    name: name.clone(),
                });
            }
            Ok(out)
        })
    }

    fn stat(&self, uid: u32, caps: &dyn CapabilityQuery, path: &str) -> Result<FileStat, Errno> {
        self.with_secured(uid, caps, path, |vfs, fs, cred, path| {
            let info = vfs.stat_via_secured(cred, path, fs)?;
            Ok(FileStat {
                kind: file_kind(info.kind),
                size: info.size,
                allocated: info.allocated,
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

    fn unlink(
        &self,
        uid: u32,
        caps: &dyn CapabilityQuery,
        path: &str,
        flags: UnlinkFlags,
    ) -> Result<(), Errno> {
        self.with_secured(uid, caps, path, |vfs, fs, cred, path| {
            vfs.remove_via_secured(cred, path, fs, flags.is_directory_only())
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
        // The same owned-snapshot resolution as `with_secured`: no table
        // borrow is held across the operation.
        let (gid, supplementary_gids) = self.identity.resolve_groups(uid)?;
        let src = Path::parse(src).map_err(VfsError::to_errno)?;
        let dst = Path::parse(dst).map_err(VfsError::to_errno)?;
        let driver = self.resolve_driver(vfs, &src)?;
        let cred = Credentials {
            uid: UserId(uid),
            gid,
            supplementary_gids: &supplementary_gids,
            caps,
        };
        let mut fs = driver.lock();
        vfs.rename_via_secured(&cred, &src, &dst, &mut *fs)
            .map_err(VfsError::to_errno)
    }

    fn set_mode(
        &self,
        uid: u32,
        caps: &dyn CapabilityQuery,
        path: &str,
        mode: u32,
    ) -> Result<(), Errno> {
        // Defence in depth behind the dispatcher's own mask check: a mode
        // word carrying a bit above the permission mask is refused here too,
        // so no in-kernel caller can write a corrupt record through this
        // seam.
        if mode & !FS_MODE_MASK != 0 {
            return Err(Errno::OutOfRange);
        }
        self.with_secured(uid, caps, path, |vfs, fs, cred, path| {
            vfs.set_mode_via_secured(cred, path, fs, mode)
        })
    }

    fn attr_get(
        &self,
        uid: u32,
        caps: &dyn CapabilityQuery,
        path: &str,
        key: &[u8],
        value_out: &mut [u8],
    ) -> Result<usize, Errno> {
        // A mount whose format stores no attributes answers with the typed
        // refusal, decided per driver through the attribute facet; every
        // permission decision stays in the secured VFS.
        self.with_secured(uid, caps, path, |vfs, fs, cred, path| {
            let Some(fs) = fs.attrs_fs() else {
                return Err(VfsError::NotSupported);
            };
            vfs.get_attr_via_secured(cred, path, fs, key, value_out)
        })
    }

    fn attr_set(
        &self,
        uid: u32,
        caps: &dyn CapabilityQuery,
        path: &str,
        key: &[u8],
        value: &[u8],
    ) -> Result<(), Errno> {
        self.with_secured(uid, caps, path, |vfs, fs, cred, path| {
            let Some(fs) = fs.attrs_fs() else {
                return Err(VfsError::NotSupported);
            };
            vfs.set_attr_via_secured(cred, path, fs, key, value)
        })
    }

    fn attr_list(
        &self,
        uid: u32,
        caps: &dyn CapabilityQuery,
        path: &str,
        index: u64,
        key_out: &mut [u8],
    ) -> Result<Option<usize>, Errno> {
        self.with_secured(uid, caps, path, |vfs, fs, cred, path| {
            let Some(fs) = fs.attrs_fs() else {
                return Err(VfsError::NotSupported);
            };
            vfs.list_attr_via_secured(cred, path, fs, index, key_out)
        })
    }

    fn attr_remove(
        &self,
        uid: u32,
        caps: &dyn CapabilityQuery,
        path: &str,
        key: &[u8],
    ) -> Result<(), Errno> {
        self.with_secured(uid, caps, path, |vfs, fs, cred, path| {
            let Some(fs) = fs.attrs_fs() else {
                return Err(VfsError::NotSupported);
            };
            vfs.remove_attr_via_secured(cred, path, fs, key)
        })
    }

    fn mount_snapshot(&self) -> Vec<MountRecord> {
        // Before any volume is online there is no VFS; report "no mounts"
        // truthfully rather than fabricating one (fail closed).
        let Ok(vfs) = self.mount.vfs() else {
            return Vec::new();
        };
        // Snapshot the mount list under the short read lock and drop the
        // guard before touching any driver: `driver.lock()` below may park
        // on a busy volume, and a spinning mount-table guard must never be
        // held across a park.
        let mounts: Vec<super::MountPoint> = vfs.mounts().iter().cloned().collect();
        mounts
            .iter()
            .filter_map(|mount| {
                // The mount table records the mount *point* (target) and its
                // permission flags authoritatively; the backing volume's
                // name, filesystem type, and space accounting come from the
                // driver registry. A backing-less mount (the in-RAM layout
                // dirs) or one whose volume is not yet online reports empty
                // names and the all-zero usage — the truthful "nothing
                // known", never a guess. A driver fault while reading its
                // accounting likewise degrades to the all-zero usage: the
                // mount itself is still reported (it exists — only its
                // numbers are unavailable).
                let (source, fstype, usage, volume_id, availability) =
                    match mount.backing().and_then(|handle| self.mount.entry(handle)) {
                        Some(entry) => {
                            let usage = entry.driver.lock().stats().unwrap_or_default();
                            (
                                entry.source,
                                entry.fstype,
                                usage,
                                entry.volume_id,
                                entry.availability,
                            )
                        }
                        None => (
                            String::new(),
                            String::new(),
                            VolumeStats::default(),
                            [0u8; 16],
                            MountAvailability::Available,
                        ),
                    };
                // `MountRecord::new` only fails on an over-long field or an
                // inconsistent usage report, which a validated VFS `Path`
                // and a sane driver cannot produce; a defensive `ok()`
                // drops any such entry rather than panicking.
                MountRecord::new(
                    source.as_bytes(),
                    path_str(mount.path()).as_bytes(),
                    fstype.as_bytes(),
                    mount.flags(),
                    usage,
                    availability,
                    volume_id,
                )
                .ok()
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
