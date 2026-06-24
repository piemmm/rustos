//! Production root-volume unlock + users-database load composition
//! (`plans/PI.md` §3 P11 root-mount increment, Chunk A).
//!
//! This is the one place that turns the three artefacts a boot path
//! recovers off the storage device — the plaintext `root.unlock`
//! key-derivation descriptor (planted on the FAT boot partition by
//! `tools/mkimage` / the §11 installer), the passphrase the operator
//! typed at the console, and the encrypted root [`Block`] device — into
//! the validated `users-v1` database the `users_db_read` syscall serves
//! (`kernel/core::load_users_db_source`). It adds no policy of its own; it
//! threads the already-landed building blocks together in the one layer
//! permitted to name both the `rustfs` driver and `kernel/core`
//! (`rustos-kernel`, `Layer::Tooling`, `AGENTS.md` §17.4).
//!
//! The composition is, in order:
//!
//! 1. [`UnlockDescriptor::decode`] parses the on-FAT descriptor
//!    fail-closed (bad magic, unknown KDF, out-of-range cost, short
//!    buffer → refused, never trusted, `AGENTS.md` §5.4.3 / §2.9).
//! 2. [`UnlockDescriptor::derive_volume_key`] derives the volume key from
//!    the typed passphrase via the descriptor's PBKDF2-HMAC-SHA256
//!    parameters (`lib/crypto`, `docs/src/filesystem/rustfs-spec.md` §7).
//!    The derived key is held in a [`Zeroizing`] wrapper so it is wiped on
//!    drop and never lingers on the boot stack (`AGENTS.md` §4 — secret
//!    hygiene; the audited `zeroize` crate, not a hand-rolled primitive,
//!    §2.12).
//! 3. [`RustFs::open`] mounts the encrypted root under that key. A wrong
//!    passphrase never unwraps the master key and the mount is refused
//!    with [`DriverError::PermissionDenied`] — there is no separate
//!    "wrong passphrase" oracle and no fallback to a plaintext mount
//!    (fail closed, `AGENTS.md` §5.4 / §4 — encrypted-by-default).
//! 4. [`load_users_db_source`] reads and validates
//!    `/System/Security/Users` off the mounted root under the kernel's
//!    capability-less `uid 0` bootstrap identity (its own §5.3 permission
//!    check and fail-closed `users-v1` parse), retaining the canonical
//!    text in a [`HeldUsersDbSource`] the boot path installs through
//!    `BootInfo::with_users_db`.
//!
//! Every refusal is audited and yields **no** database, so a system whose
//! root cannot be unlocked or whose database cannot be read serves none
//! rather than inventing accounts (`AGENTS.md` §5.4.5).
//!
//! [`read_root_unlock_descriptor`] reads the first of those three inputs —
//! the plaintext `root.unlock` descriptor — back off the FAT boot
//! partition through the same real FAT32 driver that authored it, and
//! [`mount_root_and_load_users`] is the single boot-path entry that
//! threads it straight into the unlock composition above: given the two
//! brought-up [`Block`] devices and the typed passphrase it reads the
//! descriptor, unlocks the root, and returns the served database (so the
//! boot path neither re-threads the descriptor buffer nor reconciles two
//! error taxonomies itself, `AGENTS.md` §2.2).
//!
//! [`mount_root_disk_and_load_users`] sits one layer above that seam: it
//! takes the **single** whole-disk [`Block`] device a board brings up,
//! parses its partition table (MBR or GPT — scheme- and
//! architecture-neutral, the same definition `tools/mkimage` writes,
//! `AGENTS.md` §2.2 / §2.20), locates the FAT boot and `RustFS` root
//! partitions by role, and threads bounds-checked windows onto each into
//! the composition above.
//!
//! The remaining board-specific bring-up that *produces* the whole-disk
//! [`Block`] device and the typed passphrase — the hardware-tree
//! root-device discovery, the in-kernel block `DriverHost`, and the
//! console passphrase prompt — sits above this seam in the boot path and
//! is wired in the following increment (`plans/PI.md` P11 Chunk B-2);
//! `virtio-blk` proves it on `-M virt`, EMMC2 on metal (§0.4 / P8).

use rustos_abi::driver::block::Block;
use rustos_abi::driver::filesystem::{FilesystemRead, FilesystemSecurity, NodeKind};
use rustos_abi::DriverError;
use rustos_drv_fs_fat32::Fat32;
use rustos_drv_fs_rustfs::{
    RustFs, UnlockDescriptor, VolumeKey, ROOT_UNLOCK_NAME, SYSTEM_VOLUME_KEY, UNLOCK_DESCRIPTOR_LEN,
};
use rustos_kernel_core::{
    load_users_db_source, ConsoleRead, ConsoleWrite, HeldUsersDbSource, LateUsersDb, UsersLoadError,
};
use rustos_log::{log, Event, EventId, Field, Level, Sink};
use rustos_partition::{parse_partition_table, PartitionBlock, PartitionError, PartitionType};
use zeroize::Zeroizing;

/// A mounted root volume, viewed as the read + security surface the
/// driver-store file service needs (`AGENTS.md` §18.3 / §5.3).
///
/// Blanket-implemented for every filesystem driver that is both
/// [`FilesystemRead`] and [`FilesystemSecurity`] (the production `RustFs`
/// among them), so `&mut dyn RootVolume` is the **object-safe** handle the
/// continuation passed to [`with_system_volume`] receives — letting the
/// generic unlock policy hand a freshly mounted, concretely-typed volume to
/// a continuation without itself naming the concrete filesystem type
/// (`AGENTS.md` §17.4). Its supertraits are exactly the bound the
/// [`SystemFileService`](crate::system_files::SystemFileService) the
/// driver-store server builds over the volume requires.
pub trait RootVolume: FilesystemRead + FilesystemSecurity {}

impl<T: FilesystemRead + FilesystemSecurity + ?Sized> RootVolume for T {}

/// Audit event: the encrypted root volume was unlocked under the
/// passphrase-derived key and mounted (`AGENTS.md` §5.4.4 / §19.4). The
/// subsequent users-database read is audited separately by
/// [`load_users_db_source`] (`UsersDbLoaded` / `UsersDbRejected`).
const ROOT_MOUNT_UNLOCKED: EventId = EventId(4133);

/// Audit event: the root unlock was refused by a **structural** failure
/// before a database could be served — the on-FAT descriptor could not be
/// read or failed to decode, the partition table or a partition was
/// missing/malformed, the volume was not rustfs, or the device faulted.
/// The `cause` field names which check refused; no secret (passphrase,
/// key, or volume bytes) is ever logged (`AGENTS.md` §4 / §19.4). The
/// decision fails closed: no database is held (§5.4.5).
///
/// A *wrong passphrase* is **not** one of these — that is an expected
/// authentication non-match recorded at [`ROOT_UNLOCK_KEY_REJECTED`], not
/// a system error.
const ROOT_MOUNT_REJECTED: EventId = EventId(4134);

/// Audit event: the passphrase-derived key did not unlock the encrypted
/// root volume — a wrong passphrase, *including* the silent blank-passphrase
/// probe of a non-blank image that [`unlock_root_disk_interactively`] runs
/// on **every** normal boot (`AGENTS.md` §11). This is an expected
/// fail-closed authentication non-match, not a system error: the master key
/// simply never unwrapped and there is no oracle either way (`AGENTS.md`
/// §5.4). It is recorded at [`Level::Debug`] — below the default
/// [`Level::Info`] filter — so the per-boot probe and routine interactive
/// retries cannot flood the boot log, while the record stays available for
/// brute-force forensics when the level is lowered (`AGENTS.md` §2.1 /
/// §19.4). No secret (passphrase, key, or volume byte) is ever logged.
const ROOT_UNLOCK_KEY_REJECTED: EventId = EventId(4142);

/// Audit event: the read-only, signed-bundle `/System` volume (the
/// design-B pre-unlock driver store, `plans/PI.md`) was discovered and
/// mounted read-only over its `lib/partition` window under the non-secret
/// well-known [`SYSTEM_VOLUME_KEY`] (`AGENTS.md` §18.6 / §19.4). The store
/// itself is consumed in the later design-B increments; B1 proves the
/// volume is reachable and read-only.
const SYSTEM_VOLUME_MOUNTED: EventId = EventId(4140);

/// Audit event: no read-only `/System` volume was mounted — the disk
/// carries no `RustFsSystem` partition, or the volume's window could not be
/// built or opened read-only. In B1 this is **not** fatal: the encrypted
/// root still serves the system, so the boot proceeds (`AGENTS.md` §18.4 /
/// §2.9). The `cause` field names which check declined, secret-free.
const SYSTEM_VOLUME_UNAVAILABLE: EventId = EventId(4141);

/// Why [`unlock_root_and_load_users`] produced no users database.
///
/// Each variant carries the underlying error from the first check that
/// refused; the composition stops at the first failure and returns it
/// (`AGENTS.md` §2.9 — fail closed, never partially applied).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RootMountError {
    /// The plaintext `root.unlock` descriptor could not be read off the
    /// FAT boot partition: the partition did not mount as FAT32, the
    /// [`ROOT_UNLOCK_NAME`] entry was absent ([`DriverError::NotFound`]),
    /// it was not a regular file of exactly [`UNLOCK_DESCRIPTOR_LEN`]
    /// bytes, or the device read faulted. The boot path recovers the
    /// descriptor before anything is decrypted, so a missing or malformed
    /// one yields no database rather than a fabricated default
    /// (`AGENTS.md` §2.9).
    DescriptorRead(DriverError),
    /// The on-FAT `root.unlock` descriptor failed to decode: bad magic,
    /// an unknown KDF id, an out-of-range iteration count, or a short
    /// buffer. The descriptor is plaintext and untrusted, so it is fully
    /// validated before its parameters drive any key derivation
    /// (`AGENTS.md` §5.4.3).
    Descriptor(DriverError),
    /// The root volume could not be mounted under the derived key:
    /// [`DriverError::PermissionDenied`] for a wrong passphrase (the
    /// master key never unwraps), [`DriverError::BadMagic`] for a volume
    /// that is not rustfs, or a device fault. There is no plaintext-mount
    /// fallback (`AGENTS.md` §4 — encrypted by default; §5.4 — fail
    /// closed).
    Mount(DriverError),
    /// The volume mounted but `/System/Security/Users` could not be read
    /// or validated; [`load_users_db_source`] has already audited the
    /// precise cause.
    Users(UsersLoadError),
    /// The disk's partition table could not be parsed: no recognised
    /// scheme, a malformed or forged MBR/GPT table, or a device read
    /// fault. The table is untrusted on-disk input, validated fail-closed
    /// before any partition is trusted (`AGENTS.md` §5.4 / §19.5).
    PartitionTable(PartitionError),
    /// The disk carries no FAT boot partition (the partition holding the
    /// `root.unlock` descriptor), so the encrypted root cannot be
    /// unlocked. No database is served (`AGENTS.md` §2.9).
    NoBootPartition,
    /// The disk carries no `RustFS` root partition to mount.
    NoRootPartition,
    /// A parsed partition extent does not fit the device geometry, so its
    /// bounds-checked window could not be built (`AGENTS.md` §24.4 — a
    /// fixed extent, validated against the device before any access).
    PartitionWindow(DriverError),
}

impl RootMountError {
    /// Short, stable, secret-free cause string for the audit record.
    #[must_use]
    pub fn cause(self) -> &'static str {
        match self {
            Self::DescriptorRead(_) => "descriptor_unreadable",
            Self::Descriptor(_) => "descriptor_invalid",
            Self::Mount(DriverError::PermissionDenied) => "unlock_refused",
            Self::Mount(DriverError::BadMagic) => "not_a_rustfs_volume",
            Self::Mount(_) => "mount_failed",
            Self::Users(err) => err.cause(),
            Self::PartitionTable(_) => "partition_table_invalid",
            Self::NoBootPartition => "no_boot_partition",
            Self::NoRootPartition => "no_root_partition",
            Self::PartitionWindow(_) => "partition_out_of_range",
        }
    }
}

/// Unlock the encrypted root volume with the passphrase-derived key and
/// load its `/System/Security/Users` database.
///
/// * `descriptor_bytes` — the plaintext `root.unlock` key-derivation
///   descriptor read from the FAT boot partition.
/// * `passphrase` — the bytes the operator typed at the console prompt.
///   They are used only to derive the volume key and are never logged or
///   retained by this function.
/// * `block` — the encrypted root [`Block`] device the board brought up.
/// * `audit` — the sink the unlock/mount decision and (via
///   [`load_users_db_source`]) the database-read decision are logged
///   through (`AGENTS.md` §19.4).
///
/// On success the returned [`HeldUsersDbSource`] owns the validated
/// `users-v1` text (zeroed on drop, `AGENTS.md` §4); the boot path
/// `Box::leak`s it and installs it through `BootInfo::with_users_db`.
///
/// # Errors
///
/// A [`RootMountError`] naming the first check that refused. Every error
/// path yields no database and is audited; the derived key is wiped
/// regardless of outcome (`AGENTS.md` §4 / §5.4.5).
pub fn unlock_root_and_load_users<B: Block>(
    descriptor_bytes: &[u8],
    passphrase: &[u8],
    block: B,
    audit: &dyn Sink,
) -> Result<HeldUsersDbSource, RootMountError> {
    // 1. Decode the untrusted on-FAT descriptor fail-closed before its
    //    parameters drive any key derivation (`AGENTS.md` §5.4.3).
    let descriptor = match UnlockDescriptor::decode(descriptor_bytes) {
        Ok(descriptor) => descriptor,
        Err(err) => {
            let error = RootMountError::Descriptor(err);
            reject(audit, error);
            return Err(error);
        }
    };

    // 2. Derive the volume key from the typed passphrase. The key is the
    //    most sensitive transient on the boot stack: hold it in a
    //    zero-on-drop wrapper so it is wiped the instant it leaves scope,
    //    whether the mount succeeds or fails (`AGENTS.md` §4).
    let volume_key: Zeroizing<VolumeKey> = Zeroizing::new(descriptor.derive_volume_key(passphrase));

    // 3. Mount the encrypted root. A wrong passphrase fails to unwrap the
    //    master key and is refused fail-closed — no plaintext fallback,
    //    no separate oracle (`AGENTS.md` §4 / §5.4).
    let mut fs = match RustFs::open(block, &volume_key) {
        Ok(fs) => fs,
        Err(err) => {
            let error = RootMountError::Mount(err);
            reject(audit, error);
            return Err(error);
        }
    };
    log(
        audit,
        &Event {
            level: Level::Info,
            id: ROOT_MOUNT_UNLOCKED,
            message: "root-mount: encrypted root volume unlocked and mounted",
            fields: &[],
        },
    );

    // 4. Read and validate the users database off the mounted root. This
    //    audits its own outcome (`UsersDbLoaded` / `UsersDbRejected`) and
    //    retains the canonical text in the returned holder.
    load_users_db_source(&mut fs, audit).map_err(RootMountError::Users)
}

/// Recover the `root.unlock` descriptor off the FAT boot partition and
/// load the encrypted root's users database under the typed passphrase.
///
/// This is the single boot-path entry point for the `plans/PI.md` P11
/// root-mount increment (Chunk B-2): once the board has brought up the
/// two block devices and the operator has typed a passphrase at the
/// console, the boot path calls this one function rather than threading
/// the descriptor buffer and reconciling two error taxonomies itself
/// (`AGENTS.md` §2.2). It composes the already-landed building blocks in
/// order:
///
/// 1. [`read_root_unlock_descriptor`] reads the fixed-length plaintext
///    descriptor off `boot_partition` (B-1). A missing, mis-sized, or
///    unreadable descriptor is audited and returned as
///    [`RootMountError::DescriptorRead`] — no database is served and the
///    encrypted root is never touched (`AGENTS.md` §2.9 / §5.4.5).
/// 2. [`unlock_root_and_load_users`] derives the volume key from
///    `passphrase`, mounts `root_block`, and loads the validated
///    `users-v1` database (Chunk A), auditing its own unlock/mount/read
///    decisions.
///
/// * `boot_partition` — the FAT boot-partition [`Block`] device (the
///   GPU-firmware-readable partition holding `root.unlock`).
/// * `root_block` — the encrypted root [`Block`] device.
/// * `passphrase` — the bytes the operator typed at the console; used
///   only to derive the volume key, never logged or retained.
/// * `audit` — the sink every decision is logged through (`AGENTS.md`
///   §19.4).
///
/// # Errors
///
/// A [`RootMountError`] naming the first check that refused. Every error
/// path yields no database and is audited; no secret is ever logged and
/// the derived key is wiped regardless of outcome (`AGENTS.md` §4 /
/// §5.4.5).
pub fn mount_root_and_load_users<Boot, Root>(
    boot_partition: Boot,
    root_block: Root,
    passphrase: &[u8],
    audit: &dyn Sink,
) -> Result<HeldUsersDbSource, RootMountError>
where
    Boot: Block,
    Root: Block,
{
    let descriptor = match read_root_unlock_descriptor(boot_partition) {
        Ok(descriptor) => descriptor,
        Err(err) => {
            let error = RootMountError::DescriptorRead(err);
            reject(audit, error);
            return Err(error);
        }
    };
    // This two-device entry loads only the users database; driver loading
    // is the design-B `/System`-volume path the `devmgr` drives over the
    // driver-store endpoint ([`with_system_volume`] /
    // [`crate::driver_store_server`]), independent of this read (`AGENTS.md`
    // §2.3).
    unlock_root_and_load_users(&descriptor, passphrase, root_block, audit)
}

/// Bring up the users database from a single partitioned disk: parse its
/// partition table, locate the FAT boot and `RustFS` root partitions, and
/// run the unlock + load composition over a bounds-checked window onto
/// each (`plans/PI.md` P11 Chunk B-2).
///
/// This is the entry the boot path uses when the board brings up **one**
/// block device carrying the whole disk — the common case: an SD card, a
/// USB stick, or a single `virt` virtio-blk image. It is scheme- **and**
/// architecture-neutral: `disk` may be an MBR or a GPT disk
/// ([`parse_partition_table`] detects which), so the same code reads a
/// Raspberry Pi MBR image and a UEFI x86_64 GPT disk without a board
/// `cfg` (`AGENTS.md` §17 / §2.20). The partitions are located by **role**
/// (`FatBoot` / `RustFsRoot`), not by a hard-coded index, and the on-disk
/// definition is the one `tools/mkimage` writes (`AGENTS.md` §2.2).
///
/// The two partitions are opened **in sequence** over a borrowed `disk`
/// (one device, never two simultaneous mutable windows, via the
/// `impl Block for &mut B` forwarding): the FAT boot window is built, the
/// descriptor read, the window dropped to reclaim the disk, then the
/// `RustFS` root window is built for the mount. The untrusted on-disk
/// table is validated fail-closed before any partition is trusted
/// (`AGENTS.md` §5.4 / §19.5), and a disk missing either partition serves
/// no database (`AGENTS.md` §2.9).
///
/// * `disk` — the whole-disk [`Block`] device the board brought up.
/// * `passphrase` — the bytes the operator typed at the console; used
///   only to derive the volume key, never logged or retained.
/// * `audit` — the sink every decision is logged through (`AGENTS.md`
///   §19.4).
///
/// Driver loading is **not** part of this read: it is the design-B
/// `/System`-volume path the user-space `devmgr` drives over the
/// driver-store endpoint ([`with_system_volume`] /
/// [`crate::driver_store_server`]), independent of the users-database load.
///
/// # Errors
///
/// A [`RootMountError`] naming the first check that refused
/// ([`RootMountError::PartitionTable`], [`RootMountError::NoBootPartition`],
/// [`RootMountError::NoRootPartition`], [`RootMountError::PartitionWindow`],
/// or the downstream descriptor/mount/users errors). Every error path
/// yields no database and is audited (`AGENTS.md` §5.4.5).
pub fn mount_root_disk_and_load_users<Disk: Block>(
    mut disk: Disk,
    passphrase: &[u8],
    audit: &dyn Sink,
) -> Result<HeldUsersDbSource, RootMountError> {
    // 1. Parse the untrusted partition table fail-closed (MBR or GPT).
    let table = match parse_partition_table(&mut disk) {
        Ok(table) => table,
        Err(err) => {
            let error = RootMountError::PartitionTable(err);
            reject(audit, error);
            return Err(error);
        }
    };

    // 2. Locate the two partitions RustOS needs by role, not by index.
    let Some(boot_extent) = table.first_of_type(PartitionType::FatBoot) else {
        let error = RootMountError::NoBootPartition;
        reject(audit, error);
        return Err(error);
    };
    let Some(root_extent) = table.first_of_type(PartitionType::RustFsRoot) else {
        let error = RootMountError::NoRootPartition;
        reject(audit, error);
        return Err(error);
    };

    // 3. Read the descriptor off a window onto the FAT boot partition,
    //    then drop the window to reclaim the disk for the root window
    //    (one device, two sequential windows).
    let descriptor = {
        let boot = match PartitionBlock::from_partition(&mut disk, &boot_extent) {
            Ok(boot) => boot,
            Err(err) => {
                let error = RootMountError::PartitionWindow(err);
                reject(audit, error);
                return Err(error);
            }
        };
        match read_root_unlock_descriptor(boot) {
            Ok(descriptor) => descriptor,
            Err(err) => {
                let error = RootMountError::DescriptorRead(err);
                reject(audit, error);
                return Err(error);
            }
        }
    };

    // 4. Open a window onto the RustFS root partition and run the unlock +
    //    users-load composition over it.
    let root = match PartitionBlock::from_partition(&mut disk, &root_extent) {
        Ok(root) => root,
        Err(err) => {
            let error = RootMountError::PartitionWindow(err);
            reject(audit, error);
            return Err(error);
        }
    };
    unlock_root_and_load_users(&descriptor, passphrase, root, audit)
}

/// Discover and mount the read-only signed-bundle `/System` volume on
/// `disk` and run the continuation `f` against the **still-open** volume,
/// returning whatever `f` returns wrapped in [`Some`].
///
/// This is the one place the `/System` discovery + read-only mount + layout
/// confirmation lives (`AGENTS.md` §2.2): the Design D persistent
/// driver-store service
/// ([`crate::driver_store_server::serve_system_store`]) runs its
/// never-returning serve loop through it, building a
/// [`SystemFileService`](crate::system_files::SystemFileService) over the
/// mounted volume to serve the `devmgr` catalogue/load requests. Because the
/// mounted volume borrows
/// the [`PartitionBlock`] window which borrows `disk`, all of it lives on
/// the caller's frame for as long as `f` runs — so a continuation that never
/// returns (the persistent server) keeps the mount live for the life of the
/// system without any `'static` promotion (`AGENTS.md` §2.17).
///
/// The disk's partition table is parsed fail-closed; if it carries a
/// [`PartitionType::RustFsSystem`] partition, a bounds-checked
/// [`PartitionBlock`] window is opened over it and mounted **read-only**
/// under the non-secret well-known [`SYSTEM_VOLUME_KEY`]
/// ([`RustFs::open_read_only`] — the volume holds no secrets and its
/// integrity rests on the per-bundle Ed25519 signatures the load gate
/// verifies, `AGENTS.md` §18.6). The volume's root is probed for the §16.2
/// `Drivers` store directory to confirm it is a real `/System` volume, the
/// mount is audited (`SYSTEM_VOLUME_MOUNTED`), and only then is `f` run.
///
/// **Fail-soft and fail-closed** (`AGENTS.md` §18.4 / §2.9): a disk with no
/// — or an unopenable — `/System` volume returns [`None`] (each decline
/// audited `SYSTEM_VOLUME_UNAVAILABLE`) without running `f`, and never
/// aborts the boot.
///
/// * `disk` — the whole-disk [`Block`] device the board brought up.
/// * `f` — the continuation run against the mounted read-only `/System`
///   volume.
/// * `audit` — the sink every decision is logged through (`AGENTS.md`
///   §19.4). No secret is consumed or logged.
pub fn with_system_volume<Disk: Block, R>(
    disk: &mut Disk,
    audit: &dyn Sink,
    f: impl FnOnce(&mut dyn RootVolume) -> R,
) -> Option<R> {
    let Ok(table) = parse_partition_table(&mut *disk) else {
        system_volume_unavailable(audit, "partition_table_invalid");
        return None;
    };
    let Some(extent) = table.first_of_type(PartitionType::RustFsSystem) else {
        system_volume_unavailable(audit, "no_system_partition");
        return None;
    };
    // A bounds-checked window onto the `/System` extent (`AGENTS.md` §24.4).
    let Ok(window) = PartitionBlock::from_partition(&mut *disk, &extent) else {
        system_volume_unavailable(audit, "system_window_out_of_range");
        return None;
    };
    // Mount read-only under the public key; the volume carries no secrets,
    // so the kernel can never mutate it (`AGENTS.md` §18.6 / §5.4).
    let Ok(mut system) = RustFs::open_read_only(window, &SYSTEM_VOLUME_KEY) else {
        system_volume_unavailable(audit, "system_mount_failed");
        return None;
    };
    // Confirm it is a real `/System` volume: its root carries the §16.2
    // `Drivers` store directory the continuation reads.
    let root = system.root();
    if system.lookup(root, b"Drivers").is_err() {
        system_volume_unavailable(audit, "system_layout_invalid");
        return None;
    }
    log(
        audit,
        &Event {
            level: Level::Info,
            id: SYSTEM_VOLUME_MOUNTED,
            message: "root-mount: read-only /System volume mounted",
            fields: &[],
        },
    );
    Some(f(&mut system))
}

/// Audit a declined `/System` mount with a stable, secret-free `cause`
/// (`AGENTS.md` §19.4). Fail-soft, so this is always informational and the
/// boot proceeds to the passphrase prompt with no driver autoloaded.
fn system_volume_unavailable(audit: &dyn Sink, cause: &'static str) {
    log(
        audit,
        &Event {
            level: Level::Info,
            id: SYSTEM_VOLUME_UNAVAILABLE,
            message: "root-mount: no read-only /System volume mounted",
            fields: &[Field {
                key: "cause",
                value: cause,
            }],
        },
    );
}

/// Audit event: the operator's typed passphrase unlocked the root and the
/// loaded database was published into the late credential cell — login can
/// now authenticate (`AGENTS.md` §19.4). No secret is logged.
const ROOT_UNLOCK_INSTALLED: EventId = EventId(4136);

/// Audit event: a wrong passphrase was refused; the unlock will prompt
/// again because the bounded attempt budget is not yet exhausted
/// (`AGENTS.md` §2.1 — never loop forever). No passphrase byte is logged.
const ROOT_UNLOCK_RETRY: EventId = EventId(4137);

/// Audit event: the interactive unlock gave up fail-closed — the attempt
/// budget was exhausted, the console could not be read, the disk's
/// structure was wrong, or the late cell was already populated. No
/// database is installed and the operator must reboot (`AGENTS.md` §2.9 /
/// §5.4.5). The `cause` field names which check refused, secret-free.
const ROOT_UNLOCK_GAVE_UP: EventId = EventId(4138);

/// Maximum passphrase attempts the interactive unlock allows before it
/// gives up and the system must be rebooted.
///
/// A bounded budget, not an infinite prompt loop (`AGENTS.md` §2.1):
/// after this many wrong passphrases the unlock fails closed and the late
/// credential cell stays empty, so every login is refused until the next
/// boot (`AGENTS.md` §5.4.5). The value is the User-chosen policy for the
/// `plans/PI.md` P11 Chunk B-2 root mount.
pub const MAX_UNLOCK_ATTEMPTS: u32 = 5;

/// Longest passphrase, in bytes, the console prompt accepts.
///
/// A line longer than this is refused as the current attempt (and counts
/// against [`MAX_UNLOCK_ATTEMPTS`]) rather than silently truncated to a
/// shorter — and wrong — secret (`AGENTS.md` §5.4.3). It is a fixed input
/// bound, not a scalable capacity (`AGENTS.md` §24.4): a passphrase is
/// operator-typed, so a generous fixed ceiling is correct and the read
/// buffer is a zeroized on-stack array of exactly this size.
pub const MAX_PASSPHRASE_LEN: usize = 256;

/// The one set-once credential cell the production dispatch hook reads
/// and the in-kernel root-unlock kthread writes.
///
/// The encrypted root is unlocked only *after* the console keyboard is
/// live — past the point where [`BootInfo::with_users_db`] is consumed
/// (`plans/PI.md` P11 Chunk B-2). The boot path therefore hands the
/// dispatch hook `&LATE_USERS_DB` at boot through
/// [`BootInfo::with_users_db`], so `users_db_read` reads
/// [`UsersDbSource::text`] on the same cell on every call; the trusted
/// unlock step publishes the mounted root volume's database into it
/// exactly once via [`LateUsersDb::install`], and the next read serves it.
///
/// Defined here, beside the unlock policy that installs into it, so the
/// dispatch-hook reference and the kthread's install target are one
/// definition rather than two that could diverge (`AGENTS.md` §2.2). It is
/// **not** a global mutable static (`AGENTS.md` §2.1): the cell is set-once
/// and immutable after the first install, with internal synchronisation,
/// so the single `&'static` instance is shared safely across the per-CPU
/// syscall handlers exactly like
/// [`NULL_USERS_DB`](rustos_kernel_core::NULL_USERS_DB). Until an install
/// succeeds it fails every read closed, so wiring the dispatch hook at it
/// changes no boot behaviour over the previous `NULL_USERS_DB` default
/// (`AGENTS.md` §5.4.5).
///
/// [`BootInfo::with_users_db`]: rustos_kernel_core::BootInfo::with_users_db
/// [`UsersDbSource::text`]: rustos_kernel_core::UsersDbSource::text
pub static LATE_USERS_DB: LateUsersDb = LateUsersDb::new();

/// The result of [`unlock_root_disk_interactively`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum UnlockOutcome {
    /// The root unlocked under a typed passphrase and the loaded database
    /// was installed into the late credential cell. Login can authenticate.
    Installed,
    /// The unlock gave up fail-closed: no database was installed, so every
    /// login is refused until the next boot (`AGENTS.md` §5.4.5). The
    /// caller (the unlock kthread) must not retry — the operator reboots.
    GaveUp,
}

/// Why a single passphrase line could not be read off the console.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum PassphraseReadError {
    /// The console input device failed or could not deliver a line (a
    /// device error, or no input source at all). The unlock fails closed
    /// rather than treating it as an empty passphrase (`AGENTS.md` §2.9).
    Console,
    /// The line exceeded [`MAX_PASSPHRASE_LEN`] before a terminator; the
    /// remainder was drained to the newline so the next read starts clean.
    /// Treated as a wrong attempt, never a truncated secret.
    TooLong,
}

/// Publish a successfully unlocked database into `late_db` and audit it.
///
/// Shared by both unlock paths — the silent blank-passphrase attempt and
/// each interactive attempt — so the set-once install, its fail-closed
/// "already installed" guard, and the [`ROOT_UNLOCK_INSTALLED`] audit line
/// have exactly one definition (`AGENTS.md` §2.2). The install
/// ([`LateUsersDb::install`]) is set-once: a refusal means a database was
/// already published (the kthread runs once, so that is a logic error),
/// and the rejected holder is zeroed inside `install` (`AGENTS.md` §4 /
/// §5.4).
fn finish_install(
    source: HeldUsersDbSource,
    late_db: &LateUsersDb,
    audit: &dyn Sink,
) -> UnlockOutcome {
    if late_db.install(source).is_err() {
        gave_up(audit, "already_installed");
        return UnlockOutcome::GaveUp;
    }
    log(
        audit,
        &Event {
            level: Level::Info,
            id: ROOT_UNLOCK_INSTALLED,
            message: "root-unlock: root unlocked; users database installed",
            fields: &[],
        },
    );
    UnlockOutcome::Installed
}

/// Unlock the root disk and publish the loaded database into `late_db`,
/// prompting for the passphrase only when the root is not unlockable
/// without one — retrying a wrong typed passphrase up to
/// [`MAX_UNLOCK_ATTEMPTS`] times before giving up fail-closed
/// (`plans/PI.md` P11 Chunk B-2).
///
/// The **blank** passphrase is tried silently first, before any prompt is
/// drawn. An installer image is provisioned with a blank root passphrase
/// (`rustos_mkimage::INSTALLER_PASSPHRASE`, `AGENTS.md` §11) so a fresh
/// install boots straight into the §11 installer rather than stalling
/// behind a `Root passphrase:` prompt the operator cannot answer. Only
/// when the blank passphrase does **not** unlock the root (a debug or
/// production image with a non-blank passphrase) is the operator prompted
/// interactively. A non-blank passphrase failing the silent attempt is no
/// oracle: the master key simply never unwraps, exactly as for any wrong
/// passphrase (`AGENTS.md` §5.4).
///
/// This is the device-independent unlock *policy* the in-kernel unlock
/// kthread runs once the board has brought up the root block device and
/// the console keyboard is live. It is generic over the [`Block`] disk and
/// takes the console write/read halves as object-safe seams
/// ([`ConsoleWrite`] / [`ConsoleRead`]), so it names no architecture or
/// device type (`AGENTS.md` §17.4) and is host-tested with a mock console
/// over the same MBR + encrypted-`RustFS` disk fixture `tools/mkimage`
/// writes (`AGENTS.md` §2.2). The kthread passes the **blocking** console
/// read ([`BlockingConsoleRead`](rustos_kernel_core::BlockingConsoleRead))
/// so an empty poll parks the task on the scheduler rather than
/// busy-spinning (`AGENTS.md` §2.1); this function only moves bytes.
///
/// Each attempt:
///
/// 1. Writes the `Root passphrase:` prompt to `console` (best effort — a
///    write error does not by itself abort the attempt).
/// 2. Reads one line into a zeroized, fixed-length on-stack buffer
///    ([`MAX_PASSPHRASE_LEN`]); the passphrase is never heap-allocated,
///    logged, or retained past the attempt (`AGENTS.md` §4 / §19.4).
/// 3. Runs [`mount_root_disk_and_load_users`] under the typed bytes.
///
/// On success the loaded [`HeldUsersDbSource`] is published into `late_db`
/// through [`LateUsersDb::install`] (set-once) and the function returns
/// [`UnlockOutcome::Installed`]. A wrong passphrase
/// ([`RootMountError::Mount`]`(`[`DriverError::PermissionDenied`]`)`) is
/// audited and retried until the budget is spent. Any other error is
/// structural (no partition table, no boot/root partition, an unreadable
/// or invalid descriptor, a corrupt database): retrying cannot help, so
/// the unlock gives up immediately. A console read error also gives up.
/// Every give-up path leaves `late_db` empty — login stays fail-closed
/// (`AGENTS.md` §5.4.5).
///
/// * `disk` — the whole-disk [`Block`] device the board brought up.
/// * `console` — the primary console's byte sink for the prompt.
/// * `input` — the primary console's (blocking) byte source.
/// * `late_db` — the set-once cell the loaded database is published into.
/// * `audit` — the sink every decision is logged through (`AGENTS.md`
///   §19.4); no passphrase, key, or volume byte is ever logged.
/// * `on_resolved` — invoked exactly once, after the unlock reaches its
///   terminal outcome (installed or gave up) and on every internal return
///   path, so the caller can release the console it lent the prompt. The
///   in-kernel kthread passes the console-0 hand-off (open the gate, arm
///   the UART receive interrupt, resolve the `LateUsersDb` pending wait);
///   coupling it here makes forgetting it impossible (`AGENTS.md` §5.4.5).
///
/// Driver loading is **not** part of this policy: under design B the
/// user-space `devmgr` loads drivers over the driver-store endpoint served
/// off the read-only `/System` volume ([`with_system_volume`] /
/// [`crate::driver_store_server`]), independent of this encrypted-root
/// (user-data) prompt — the store volume is reachable without this
/// passphrase.
pub fn unlock_root_disk_interactively<Disk: Block>(
    disk: Disk,
    console: &dyn ConsoleWrite,
    input: &dyn ConsoleRead,
    late_db: &LateUsersDb,
    audit: &dyn Sink,
    on_resolved: &dyn Fn(),
) -> UnlockOutcome {
    // Run the interactive unlock to a terminal outcome, then hand the
    // console back to `login` — *exactly once, on every outcome and every
    // internal return path*. Coupling the release to the resolution here,
    // rather than expecting each caller to remember it, is deliberate
    // (`AGENTS.md` §2.2): the in-kernel unlock kthread owns console 0 for
    // the duration of the passphrase prompt (its `GatedConsoleRead` keeps
    // `login` parked), so the moment the unlock resolves — a database
    // installed *or* given up — the gate must open, the UART receive
    // interrupt must arm, and the `LateUsersDb` pending wait must resolve,
    // or no console could ever be typed into even though the root mounted.
    // A successful unlock previously skipped that release (only the
    // fail-closed branches ran it), wedging both the keyboard and serial
    // `login` after a good unlock; threading it through `on_resolved` makes
    // forgetting it impossible (`AGENTS.md` §5.4.5).
    let outcome = unlock_root_disk_interactively_impl(disk, console, input, late_db, audit);
    on_resolved();
    outcome
}

/// The interactive-unlock state machine itself (see
/// [`unlock_root_disk_interactively`], which wraps this with the mandatory
/// console-release-on-resolution). Split out so the release cannot be
/// skipped by any of this function's internal return paths.
fn unlock_root_disk_interactively_impl<Disk: Block>(
    disk: Disk,
    console: &dyn ConsoleWrite,
    input: &dyn ConsoleRead,
    late_db: &LateUsersDb,
    audit: &dyn Sink,
) -> UnlockOutcome {
    // The disk is borrowed mutably for each attempt through the
    // `impl Block for &mut B` forwarding, so one device is reused across
    // retries without re-acquiring it (`AGENTS.md` §2.2).
    let mut disk = disk;

    // Try the blank passphrase silently first, before drawing any prompt.
    // An installer image is provisioned with a **blank** root passphrase
    // (`rustos_mkimage::INSTALLER_PASSPHRASE`, `AGENTS.md` §11): a fresh
    // install must boot straight into the §11 installer, never stall behind
    // a `Root passphrase:` prompt the operator has no value to answer. So
    // if the blank passphrase unlocks the root we install and return with
    // no prompt at all. A debug or production image whose passphrase is
    // non-blank simply fails this attempt — the master key never unwraps,
    // exactly like any wrong passphrase, so it is no oracle (`AGENTS.md`
    // §5.4) — and falls through to the interactive prompt below.
    match mount_root_disk_and_load_users(&mut disk, b"", audit) {
        Ok(source) => return finish_install(source, late_db, audit),
        Err(RootMountError::Mount(DriverError::PermissionDenied)) => {
            // Non-blank passphrase: prompt the operator interactively.
        }
        Err(error) => {
            // A structural failure (no table/partition, unreadable or
            // invalid descriptor, corrupt database): re-prompting cannot
            // fix the disk, so give up now (`4134` already named the
            // cause).
            gave_up(audit, error.cause());
            return UnlockOutcome::GaveUp;
        }
    }

    let mut attempt = 0u32;
    while attempt < MAX_UNLOCK_ATTEMPTS {
        attempt += 1;

        write_all(console, b"\r\nRoot passphrase: ");

        // A zeroized, fixed-length buffer: the typed secret never reaches
        // the heap and is wiped when this attempt's buffer drops
        // (`AGENTS.md` §4).
        let mut passphrase = Zeroizing::new([0u8; MAX_PASSPHRASE_LEN]);
        let len = match read_passphrase_line(input, &mut passphrase[..]) {
            Ok(len) => len,
            Err(PassphraseReadError::TooLong) => {
                // An over-long line is a wrong attempt, not a fatal console
                // fault: count it and prompt again (if budget remains).
                retry(audit);
                continue;
            }
            Err(PassphraseReadError::Console) => {
                // The console could not deliver a line: fail closed rather
                // than retry against a dead input (`AGENTS.md` §2.9).
                gave_up(audit, "console_unreadable");
                return UnlockOutcome::GaveUp;
            }
        };

        match mount_root_disk_and_load_users(&mut disk, &passphrase[..len], audit) {
            Ok(source) => return finish_install(source, late_db, audit),
            Err(RootMountError::Mount(DriverError::PermissionDenied)) => {
                // Wrong passphrase: the master key never unwrapped. Bounded
                // retry — never an oracle and never an infinite loop
                // (`AGENTS.md` §2.1 / §5.4). Falls through to the next loop
                // iteration; the budget check ends it.
                retry(audit);
            }
            Err(error) => {
                // A structural failure (no table, no partition, an
                // unreadable/invalid descriptor, a corrupt database):
                // re-prompting cannot fix the disk, so give up now. The
                // mount step already audited the precise cause (`4134`);
                // record the give-up with the same secret-free cause.
                gave_up(audit, error.cause());
                return UnlockOutcome::GaveUp;
            }
        }
    }

    // The attempt budget is spent: fail closed, leave the cell empty.
    gave_up(audit, "attempts_exhausted");
    UnlockOutcome::GaveUp
}

/// Write every byte of `bytes` to `console`, looping over short writes and
/// stopping on a zero-length write or an error.
///
/// Best effort: the prompt is advisory, so a console that cannot accept it
/// does not abort the unlock — the read still parks for input. Never spins
/// on a stalled device (`AGENTS.md` §2.1).
fn write_all(console: &dyn ConsoleWrite, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        match console.write(bytes) {
            Ok(0) | Err(_) => break,
            Ok(n) => bytes = &bytes[n.min(bytes.len())..],
        }
    }
}

/// Read one passphrase line from `input` into `buf`, returning its length.
///
/// Reads byte by byte so the read never consumes past the line terminator
/// (`CR` or `LF`), which a one-shot prompt could not put back. A
/// `Backspace`/`Delete` (`0x08` / `0x7f`) rubs out the previous byte, so a
/// mistyped passphrase can be corrected without a fresh attempt. A line
/// that fills `buf` before a terminator is drained to the newline and
/// reported as [`PassphraseReadError::TooLong`] rather than truncated.
///
/// # Errors
///
/// [`PassphraseReadError::Console`] if the device read fails, or signals
/// end of input before any byte of a line arrived (fail closed,
/// `AGENTS.md` §2.9); [`PassphraseReadError::TooLong`] for an over-length
/// line.
fn read_passphrase_line(
    input: &dyn ConsoleRead,
    buf: &mut [u8],
) -> Result<usize, PassphraseReadError> {
    let mut len = 0usize;
    loop {
        let mut byte = [0u8; 1];
        let read = input
            .read(&mut byte)
            .map_err(|_| PassphraseReadError::Console)?;
        if read == 0 {
            // End of input. A line with content is accepted as typed; an
            // empty one means the console closed with nothing — fail closed
            // rather than mount under an empty passphrase (`AGENTS.md`
            // §2.9 / §5.4.5).
            return if len == 0 {
                Err(PassphraseReadError::Console)
            } else {
                Ok(len)
            };
        }
        match byte[0] {
            b'\n' | b'\r' => return Ok(len),
            0x08 | 0x7f => len = len.saturating_sub(1),
            b => {
                if len == buf.len() {
                    drain_to_newline(input);
                    return Err(PassphraseReadError::TooLong);
                }
                buf[len] = b;
                len += 1;
            }
        }
    }
}

/// Drain console input up to and including the next line terminator,
/// discarding it, so a subsequent read starts on a fresh line. Stops on a
/// terminator, a zero-length read, or an error (never spins, `AGENTS.md`
/// §2.1).
fn drain_to_newline(input: &dyn ConsoleRead) {
    loop {
        let mut byte = [0u8; 1];
        match input.read(&mut byte) {
            Ok(0) | Err(_) => return,
            Ok(_) if byte[0] == b'\n' || byte[0] == b'\r' => return,
            Ok(_) => {}
        }
    }
}

/// Audit a wrong (or over-long) passphrase attempt that will be retried.
/// No passphrase byte is logged (`AGENTS.md` §4 / §19.4).
fn retry(audit: &dyn Sink) {
    log(
        audit,
        &Event {
            level: Level::Warn,
            id: ROOT_UNLOCK_RETRY,
            message: "root-unlock: passphrase refused; prompting again",
            fields: &[],
        },
    );
}

/// Audit a fail-closed give-up of the interactive unlock, naming the
/// secret-free `cause`. No database is installed (`AGENTS.md` §5.4.5).
fn gave_up(audit: &dyn Sink, cause: &'static str) {
    log(
        audit,
        &Event {
            level: Level::Error,
            id: ROOT_UNLOCK_GAVE_UP,
            message: "root-unlock: gave up; no users database installed (reboot required)",
            fields: &[Field {
                key: "cause",
                value: cause,
            }],
        },
    );
}

/// Classify a refused unlock into the audit record it is logged as.
///
/// A *wrong passphrase* — `Mount(PermissionDenied)`, the case the silent
/// blank-passphrase probe of a non-blank image hits on **every** normal
/// boot and the case a mistyped interactive passphrase hits — is an
/// expected fail-closed authentication non-match, not a system error: the
/// derived key simply never unwrapped the master key and there is no oracle
/// either way (`AGENTS.md` §5.4). It maps to the below-`Info`
/// [`ROOT_UNLOCK_KEY_REJECTED`] so the per-boot probe and routine retries
/// cannot flood the boot log, while the record stays available for
/// brute-force forensics when the level is lowered (`AGENTS.md` §2.1 /
/// §19.4). Every other refusal is a genuine structural failure (a
/// corrupt/missing descriptor, a missing/malformed partition table or
/// partition, a non-rustfs volume, a device fault) and maps to the
/// `Error`-level [`ROOT_MOUNT_REJECTED`].
///
/// A pure mapping so the audit level/event a refusal earns can be asserted
/// directly, without depending on the global log threshold (`AGENTS.md`
/// §2.2 / §7).
fn rejection_record(error: RootMountError) -> (EventId, Level, &'static str) {
    match error {
        RootMountError::Mount(DriverError::PermissionDenied) => (
            ROOT_UNLOCK_KEY_REJECTED,
            Level::Debug,
            "root-mount: derived key did not unlock the volume (wrong passphrase)",
        ),
        _ => (
            ROOT_MOUNT_REJECTED,
            Level::Error,
            "root-mount: root volume unlock refused; no users database served",
        ),
    }
}

/// Audit a refused unlock, naming the failing stage with a secret-free
/// cause string. The database load stage audits itself, so this helper
/// reports a partition-table parse, partition lookup/window, descriptor
/// read, descriptor decode, or mount refusal. The event and level are
/// chosen by [`rejection_record`]: a structural refusal is an `Error`, a
/// wrong passphrase a below-`Info` `Debug` (it is no error, §5.4).
fn reject(audit: &dyn Sink, error: RootMountError) {
    let (id, level, message) = rejection_record(error);
    log(
        audit,
        &Event {
            level,
            id,
            message,
            fields: &[Field {
                key: "cause",
                value: error.cause(),
            }],
        },
    );
}

/// Read the plaintext `root.unlock` key-derivation descriptor off the FAT
/// boot partition.
///
/// The boot path recovers this descriptor *before* anything is decrypted
/// and hands its bytes — together with the passphrase the operator types
/// at the console — to [`unlock_root_and_load_users`]. Reading it through
/// the same real FAT32 driver that `tools/mkimage` / the §11 installer
/// authored it with keeps one on-disk definition for both ends
/// (`AGENTS.md` §2.2); the file name is the shared
/// [`ROOT_UNLOCK_NAME`] constant.
///
/// The descriptor is a fixed-length record ([`UNLOCK_DESCRIPTOR_LEN`]
/// bytes), so the read is strictly bounded and fail-closed (`AGENTS.md`
/// §5.4 / §24.4): the entry's size is checked to be **exactly** that
/// length *before* a byte is read — rejecting both a truncated and an
/// over-long file — and the bytes are read into a fixed on-stack buffer.
/// The returned bytes are still untrusted: [`UnlockDescriptor::decode`]
/// (inside [`unlock_root_and_load_users`]) validates every field before
/// they drive any key derivation (`AGENTS.md` §5.4.3).
///
/// * `boot_partition` — the FAT boot-partition [`Block`] device the board
///   brought up (the GPU-firmware-readable partition on a Pi SD card).
///
/// # Errors
///
/// A [`DriverError`] from the first check that refused: the partition does
/// not mount as FAT32, [`ROOT_UNLOCK_NAME`] is absent
/// ([`DriverError::NotFound`]), the entry is not a regular file
/// ([`DriverError::Unsupported`]), its size is not exactly
/// [`UNLOCK_DESCRIPTOR_LEN`] ([`DriverError::OutOfRange`]), or the device
/// read faulted. No partial descriptor is ever returned (`AGENTS.md`
/// §2.9).
pub fn read_root_unlock_descriptor<B: Block>(
    boot_partition: B,
) -> Result<[u8; UNLOCK_DESCRIPTOR_LEN], DriverError> {
    let mut fs = Fat32::open(boot_partition)?;
    let root = fs.root();
    let node = fs.lookup(root, ROOT_UNLOCK_NAME.as_bytes())?;

    // Validate shape and size *before* reading: a fixed-length record, so
    // anything that is not exactly one is refused rather than partially
    // read (`AGENTS.md` §5.4 / §24.4 — a format bound, not a capacity).
    let info = fs.node_info(node)?;
    if info.kind != NodeKind::RegularFile {
        return Err(DriverError::Unsupported);
    }
    let size = usize::try_from(info.size).map_err(|_| DriverError::OutOfRange)?;
    if size != UNLOCK_DESCRIPTOR_LEN {
        return Err(DriverError::OutOfRange);
    }

    let mut descriptor = [0u8; UNLOCK_DESCRIPTOR_LEN];
    let read = fs.read_at(node, 0, &mut descriptor)?;
    if read != UNLOCK_DESCRIPTOR_LEN {
        return Err(DriverError::DeviceFault);
    }
    Ok(descriptor)
}

#[cfg(test)]
mod tests {
    use super::*;

    use core::cell::RefCell;

    use alloc::vec::Vec;

    use rustos_abi::driver::block::BlockGeometry;
    use rustos_abi::driver::filesystem::FilesystemWrite;
    use rustos_drv_fs_rustfs::{EntropySource, UNLOCK_MIN_ITERATIONS};
    use rustos_kernel_core::UsersDbSource;
    use rustos_log::{Event as LogEvent, Sink as LogSink};
    use rustos_partition::{mbr, Partition};
    use rustos_test_encrypted_root_image as disk_image;
    use rustos_test_rustfs_image as image;
    use rustos_users::UsersDb;

    /// Deterministic entropy for provisioning a descriptor's salt in
    /// tests. A fixed sequence keeps the test reproducible (`AGENTS.md`
    /// §19.3); it is test scaffolding, never a production source.
    struct SeqEntropy {
        next: u8,
    }

    impl EntropySource for SeqEntropy {
        fn fill(&mut self, out: &mut [u8]) -> Result<(), DriverError> {
            for byte in out.iter_mut() {
                *byte = self.next;
                self.next = self.next.wrapping_add(1);
            }
            Ok(())
        }
    }

    /// Records every audited event id so a test can assert the audit trail.
    struct RecordingSink {
        ids: RefCell<Vec<u32>>,
    }

    impl RecordingSink {
        fn new() -> Self {
            Self {
                ids: RefCell::new(Vec::new()),
            }
        }
        fn ids(&self) -> Vec<u32> {
            self.ids.borrow().clone()
        }
    }

    impl LogSink for RecordingSink {
        fn write_event(&self, event: &LogEvent<'_>) {
            self.ids.borrow_mut().push(event.id.0);
        }
    }

    /// The passphrase the test "operator" types; the volume is provisioned
    /// under the key derived from it. Forwarded from the shared whole-disk
    /// fixture so the in-memory split tests and the `-M virt` QEMU vertical
    /// type one passphrase (`AGENTS.md` §2.2).
    const PASSPHRASE: &[u8] = disk_image::PASSPHRASE;

    /// Provision a descriptor (low cost so the test stays fast under
    /// `cargo test`) and return its encoded bytes plus the volume key it
    /// derives from [`PASSPHRASE`].
    fn provision() -> ([u8; UNLOCK_DESCRIPTOR_LEN], VolumeKey) {
        // The policy floor (100k) is the cheapest a descriptor may carry,
        // keeping the per-test PBKDF2 derivations bounded while still
        // exercising the real key-derivation path (`AGENTS.md` §5.4).
        let descriptor =
            UnlockDescriptor::provision(UNLOCK_MIN_ITERATIONS, &mut SeqEntropy { next: 7 })
                .expect("descriptor provisions");
        let key = descriptor.derive_volume_key(PASSPHRASE);
        let mut bytes = [0u8; UNLOCK_DESCRIPTOR_LEN];
        descriptor.encode(&mut bytes).expect("descriptor encodes");
        (bytes, key)
    }

    #[test]
    fn the_correct_passphrase_unlocks_the_root_and_loads_a_usable_database() {
        // The end-to-end Chunk A path: a descriptor + the matching
        // passphrase derive the key the volume was provisioned under, the
        // volume mounts, and the served text is the exact, usable
        // `users-v1` database that authenticates the planted account.
        let (descriptor_bytes, key) = provision();
        let bytes = image::build_users_root_image_with_key(&key).expect("users-root volume builds");
        let block = image::VecBlock::from_bytes(bytes);
        let sink = RecordingSink::new();

        let source = unlock_root_and_load_users(&descriptor_bytes, PASSPHRASE, block, &sink)
            .expect("the correct passphrase unlocks the root and loads the database");

        let text = source.text().expect("a loaded holder serves its text");
        let serialised = image::users_db_text().expect("fixture text serialises");
        assert_eq!(
            text,
            serialised.as_bytes(),
            "the served text is the exact canonical users-v1 database"
        );

        // The served database is usable: it parses and authenticates the
        // planted account but refuses a wrong password (`plans/PI.md` P11).
        let db = UsersDb::parse(core::str::from_utf8(text).expect("utf-8"))
            .expect("the served database parses");
        let record = db
            .authenticate(
                image::USERS_FIXTURE_USERNAME,
                image::USERS_FIXTURE_PASSWORD.as_bytes(),
            )
            .expect("the planted account authenticates");
        assert_eq!(record.username(), image::USERS_FIXTURE_USERNAME);
        assert!(
            db.authenticate(image::USERS_FIXTURE_USERNAME, b"wrong")
                .is_err(),
            "a wrong account password is refused"
        );

        // The unlock and the database load are both audited.
        assert!(sink.ids().contains(&4133), "{:?}", sink.ids());
    }

    #[test]
    fn a_wrong_passphrase_is_refused_fail_closed_with_no_oracle() {
        // §4 / §5.4: the volume is provisioned under the key derived from
        // PASSPHRASE; a *different* passphrase derives a different key that
        // never unwraps the master key, so the mount is refused with
        // PermissionDenied and no database is served.
        let (descriptor_bytes, key) = provision();
        let bytes = image::build_users_root_image_with_key(&key).expect("users-root volume builds");
        let block = image::VecBlock::from_bytes(bytes);
        let sink = RecordingSink::new();

        let err = unlock_root_and_load_users(&descriptor_bytes, b"wrong passphrase", block, &sink)
            .expect_err("a wrong passphrase must be refused");

        assert_eq!(err, RootMountError::Mount(DriverError::PermissionDenied));
        assert_eq!(err.cause(), "unlock_refused");
        // A wrong passphrase is an expected fail-closed authentication
        // non-match, not a system error, so it must NOT surface as the
        // ERROR-level `ROOT_MOUNT_REJECTED` (4134) a structural refusal
        // gets — otherwise the silent blank-passphrase probe floods the
        // boot log on every non-blank boot (`AGENTS.md` §2.1 / §19.4). It
        // maps to the below-`Info` `ROOT_UNLOCK_KEY_REJECTED` (4142),
        // dropped at the default threshold (so absent from this sink) and
        // available for brute-force forensics when the level is lowered.
        assert!(!sink.ids().contains(&4134), "{:?}", sink.ids());
        assert_eq!(
            rejection_record(err),
            (
                ROOT_UNLOCK_KEY_REJECTED,
                Level::Debug,
                "root-mount: derived key did not unlock the volume (wrong passphrase)"
            )
        );
        assert!(!sink.ids().contains(&4133), "{:?}", sink.ids());
    }

    #[test]
    fn a_tampered_descriptor_is_refused_before_any_key_derivation() {
        // §5.4.3: a corrupt descriptor (bad magic) is rejected outright;
        // the passphrase is never even consulted and no mount is attempted.
        let (mut descriptor_bytes, key) = provision();
        descriptor_bytes[0] ^= 0xFF; // corrupt the magic
        let bytes = image::build_users_root_image_with_key(&key).expect("users-root volume builds");
        let block = image::VecBlock::from_bytes(bytes);
        let sink = RecordingSink::new();

        let err = unlock_root_and_load_users(&descriptor_bytes, PASSPHRASE, block, &sink)
            .expect_err("a tampered descriptor must be refused");

        assert!(matches!(err, RootMountError::Descriptor(_)), "{err:?}");
        assert_eq!(err.cause(), "descriptor_invalid");
        assert!(sink.ids().contains(&4134), "{:?}", sink.ids());
        assert!(!sink.ids().contains(&4133), "{:?}", sink.ids());
    }

    #[test]
    fn a_non_rustfs_volume_is_refused() {
        // §5.4 / §2.9: a device that is not a rustfs volume (here a zeroed
        // image of the right geometry) fails the mount closed rather than
        // being misread; no database is served.
        let (descriptor_bytes, _key) = provision();
        let sectors = usize::try_from(image::TOTAL_SECTORS).expect("sector count fits usize");
        let blank = alloc::vec![0u8; sectors * image::SECTOR_BYTES];
        let block = image::VecBlock::from_bytes(blank);
        let sink = RecordingSink::new();

        let err = unlock_root_and_load_users(&descriptor_bytes, PASSPHRASE, block, &sink)
            .expect_err("a non-rustfs volume must be refused");

        assert!(matches!(err, RootMountError::Mount(_)), "{err:?}");
        assert!(sink.ids().contains(&4134), "{:?}", sink.ids());
        assert!(!sink.ids().contains(&4133), "{:?}", sink.ids());
    }

    #[test]
    fn a_wrong_passphrase_is_classified_below_error_every_structural_refusal_is_an_error() {
        // The pure audit classification (`AGENTS.md` §2.1 / §19.4): a wrong
        // passphrase is an expected fail-closed authentication non-match —
        // recorded below the default `Info` filter so the silent
        // blank-passphrase probe and routine retries cannot flood the boot
        // log — while every genuine structural refusal stays an ERROR a
        // boot operator sees. Asserting the mapping directly is independent
        // of the global log threshold (so it cannot be perturbed by another
        // test lowering it, `AGENTS.md` §7).
        assert_eq!(
            rejection_record(RootMountError::Mount(DriverError::PermissionDenied)),
            (
                ROOT_UNLOCK_KEY_REJECTED,
                Level::Debug,
                "root-mount: derived key did not unlock the volume (wrong passphrase)"
            )
        );
        assert!(
            Level::Debug < Level::Info,
            "the probe outcome is below the default filter"
        );

        // Every other refusal — including a non-rustfs volume or a device
        // fault, which are *also* `Mount(_)` but not a wrong passphrase — is
        // the ERROR-level structural rejection.
        for error in [
            RootMountError::Mount(DriverError::BadMagic),
            RootMountError::Mount(DriverError::DeviceFault),
            RootMountError::DescriptorRead(DriverError::NotFound),
            RootMountError::Descriptor(DriverError::BadMagic),
            RootMountError::NoBootPartition,
            RootMountError::NoRootPartition,
            RootMountError::PartitionWindow(DriverError::OutOfRange),
        ] {
            let (id, level, _) = rejection_record(error);
            assert_eq!(id, ROOT_MOUNT_REJECTED, "{error:?}");
            assert_eq!(level, Level::Error, "{error:?}");
        }
    }

    // --- read_root_unlock_descriptor ----------------------------------

    /// 512-byte sector size — the FAT boot partition's geometry.
    const FAT_SECTOR_BYTES: usize = 512;

    /// 64 MiB, the production boot-partition size `tools/mkimage` formats.
    /// A valid FAT32 volume needs far more than the small rustfs fixture's
    /// 2048 sectors, so this `Block` double is sized for a real FAT32 format
    /// rather than reusing `image::VecBlock` (whose fixed geometry is the
    /// rustfs fixture's). Forwarded from the shared whole-disk fixture so
    /// the boot-partition size has one definition (`AGENTS.md` §2.2).
    const FAT_BOOT_SECTORS: u64 = disk_image::FAT_BOOT_SECTORS;

    /// In-memory FAT boot-partition [`Block`] double: a `Vec<u8>` addressed
    /// in [`FAT_SECTOR_BYTES`]-byte sectors, exactly as the board's
    /// SD/virtio-blk device presents the partition. Sized from its backing
    /// length so a real FAT32 volume formats on it.
    struct FatVecBlock {
        store: Vec<u8>,
    }

    impl FatVecBlock {
        fn new(sectors: u64) -> Self {
            let len = usize::try_from(sectors).expect("sector count fits usize") * FAT_SECTOR_BYTES;
            Self {
                store: alloc::vec![0u8; len],
            }
        }

        fn span(&self, lba: u64, len: usize) -> Result<(usize, usize), DriverError> {
            if len == 0 || len % FAT_SECTOR_BYTES != 0 {
                return Err(DriverError::BufferTooSmall);
            }
            let start = usize::try_from(lba)
                .ok()
                .and_then(|l| l.checked_mul(FAT_SECTOR_BYTES))
                .ok_or(DriverError::LengthOutOfRange)?;
            let end = start
                .checked_add(len)
                .ok_or(DriverError::LengthOutOfRange)?;
            if end > self.store.len() {
                return Err(DriverError::LengthOutOfRange);
            }
            Ok((start, end))
        }
    }

    impl Block for FatVecBlock {
        fn geometry(&self) -> Result<BlockGeometry, DriverError> {
            Ok(BlockGeometry {
                block_size: u32::try_from(FAT_SECTOR_BYTES).expect("sector size fits u32"),
                block_count: u64::try_from(self.store.len() / FAT_SECTOR_BYTES)
                    .expect("sector count fits u64"),
            })
        }

        fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
            let (start, end) = self.span(lba, buf.len())?;
            buf.copy_from_slice(&self.store[start..end]);
            Ok(())
        }

        fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
            let (start, end) = self.span(lba, buf.len())?;
            self.store[start..end].copy_from_slice(buf);
            Ok(())
        }
    }

    /// Author a FAT boot partition through the real FAT32 driver and plant
    /// `payload` under [`ROOT_UNLOCK_NAME`] — the exact write `tools/mkimage`
    /// performs (`AGENTS.md` §2.2). Returns the mounted-and-flushed device,
    /// ready to hand straight to [`read_root_unlock_descriptor`].
    fn author_boot_partition(payload: &[u8]) -> FatVecBlock {
        let mut fs = Fat32::format(FatVecBlock::new(FAT_BOOT_SECTORS)).expect("FAT32 formats");
        let root = fs.root();
        fs.create(root, ROOT_UNLOCK_NAME.as_bytes(), NodeKind::RegularFile)
            .expect("the descriptor file is created");
        let written = fs
            .write_at(root, ROOT_UNLOCK_NAME.as_bytes(), 0, payload)
            .expect("the descriptor bytes are written");
        assert_eq!(written, payload.len(), "the whole descriptor is written");
        fs.flush().expect("the partition flushes");
        fs.into_block()
    }

    /// An empty FAT boot partition carrying no `root.unlock` file.
    fn empty_boot_partition() -> FatVecBlock {
        let mut fs = Fat32::format(FatVecBlock::new(FAT_BOOT_SECTORS)).expect("FAT32 formats");
        fs.flush().expect("the partition flushes");
        fs.into_block()
    }

    #[test]
    fn reads_back_the_exact_planted_descriptor() {
        // The on-FAT descriptor round-trips byte-for-byte through the real
        // driver and decodes to the same descriptor it was provisioned as
        // (`AGENTS.md` §2.2 — author and reader share one definition).
        let (descriptor_bytes, _key) = provision();
        let block = author_boot_partition(&descriptor_bytes);

        let read = read_root_unlock_descriptor(block).expect("the planted descriptor reads back");
        assert_eq!(read, descriptor_bytes);
        // The bytes are a genuinely well-formed descriptor.
        UnlockDescriptor::decode(&read).expect("the read descriptor decodes");
    }

    #[test]
    fn the_reader_feeds_the_unlock_composition_end_to_end() {
        // The reader's output is exactly what `unlock_root_and_load_users`
        // consumes: read the descriptor off FAT, then unlock the encrypted
        // root with the matching passphrase and load the database.
        let (descriptor_bytes, key) = provision();
        let boot = author_boot_partition(&descriptor_bytes);
        let read = read_root_unlock_descriptor(boot).expect("descriptor reads back");

        let root_bytes =
            image::build_users_root_image_with_key(&key).expect("users-root volume builds");
        let root = image::VecBlock::from_bytes(root_bytes);
        let sink = RecordingSink::new();
        let source = unlock_root_and_load_users(&read, PASSPHRASE, root, &sink)
            .expect("the FAT-read descriptor unlocks the root");
        assert!(source.text().is_ok(), "a database is served");
    }

    #[test]
    fn a_missing_descriptor_is_not_found() {
        // §2.9: no `root.unlock` on the partition is a fail-closed
        // NotFound, never a fabricated descriptor.
        let block = empty_boot_partition();
        assert_eq!(
            read_root_unlock_descriptor(block),
            Err(DriverError::NotFound)
        );
    }

    #[test]
    fn a_truncated_descriptor_is_refused() {
        // §5.4 / §24.4: a short file is rejected on its size *before* any
        // read, never zero-padded up to the record length.
        let (descriptor_bytes, _key) = provision();
        let block = author_boot_partition(&descriptor_bytes[..UNLOCK_DESCRIPTOR_LEN - 1]);
        assert_eq!(
            read_root_unlock_descriptor(block),
            Err(DriverError::OutOfRange)
        );
    }

    #[test]
    fn an_over_long_descriptor_is_refused() {
        // §5.4 / §24.4: a file longer than the fixed record — extra bytes a
        // tampered partition might append — is refused, not truncated to
        // the prefix.
        let (descriptor_bytes, _key) = provision();
        let mut padded = descriptor_bytes.to_vec();
        padded.push(0);
        let block = author_boot_partition(&padded);
        assert_eq!(
            read_root_unlock_descriptor(block),
            Err(DriverError::OutOfRange)
        );
    }

    #[test]
    fn an_unformatted_partition_does_not_mount() {
        // §2.9: a device that is not a FAT volume (a zeroed image) fails
        // the mount closed rather than being misread.
        let block = FatVecBlock::new(FAT_BOOT_SECTORS);
        assert!(
            read_root_unlock_descriptor(block).is_err(),
            "an unformatted partition must not mount"
        );
    }

    // --- mount_root_and_load_users (the Chunk B-2 boot-path entry) -----

    #[test]
    fn the_boot_partition_and_passphrase_unlock_the_root_end_to_end() {
        // The single boot-path entry: a FAT boot partition carrying the
        // descriptor + the encrypted root + the matching passphrase yields
        // the usable users database, with the unlock audited.
        let (descriptor_bytes, key) = provision();
        let boot = author_boot_partition(&descriptor_bytes);
        let root_bytes =
            image::build_users_root_image_with_key(&key).expect("users-root volume builds");
        let root = image::VecBlock::from_bytes(root_bytes);
        let sink = RecordingSink::new();

        let source = mount_root_and_load_users(boot, root, PASSPHRASE, &sink)
            .expect("the descriptor + passphrase unlock the root and load the database");

        let text = source.text().expect("a loaded holder serves its text");
        let db = UsersDb::parse(core::str::from_utf8(text).expect("utf-8"))
            .expect("the served database parses");
        db.authenticate(
            image::USERS_FIXTURE_USERNAME,
            image::USERS_FIXTURE_PASSWORD.as_bytes(),
        )
        .expect("the planted account authenticates");
        assert!(sink.ids().contains(&4133), "{:?}", sink.ids());
    }

    #[test]
    fn a_missing_descriptor_on_the_boot_partition_serves_no_database() {
        // §2.9 / §5.4.5: no `root.unlock` on the boot partition is a
        // fail-closed DescriptorRead(NotFound). The encrypted root is
        // never touched (no unlock audit) and no database is served.
        let (_descriptor_bytes, key) = provision();
        let boot = empty_boot_partition();
        let root_bytes =
            image::build_users_root_image_with_key(&key).expect("users-root volume builds");
        let root = image::VecBlock::from_bytes(root_bytes);
        let sink = RecordingSink::new();

        let err = mount_root_and_load_users(boot, root, PASSPHRASE, &sink)
            .expect_err("a missing descriptor must be refused");

        assert_eq!(err, RootMountError::DescriptorRead(DriverError::NotFound));
        assert_eq!(err.cause(), "descriptor_unreadable");
        assert!(sink.ids().contains(&4134), "{:?}", sink.ids());
        assert!(!sink.ids().contains(&4133), "{:?}", sink.ids());
    }

    #[test]
    fn a_wrong_passphrase_through_the_full_composition_is_refused() {
        // §4 / §5.4: a readable descriptor but the wrong passphrase derives
        // a key that never unwraps the master key, so the mount is refused
        // and no database is served — no separate oracle.
        let (descriptor_bytes, key) = provision();
        let boot = author_boot_partition(&descriptor_bytes);
        let root_bytes =
            image::build_users_root_image_with_key(&key).expect("users-root volume builds");
        let root = image::VecBlock::from_bytes(root_bytes);
        let sink = RecordingSink::new();

        let err = mount_root_and_load_users(boot, root, b"wrong passphrase", &sink)
            .expect_err("a wrong passphrase must be refused");

        assert_eq!(err, RootMountError::Mount(DriverError::PermissionDenied));
        // Through the full composition too, a wrong passphrase is the
        // below-`Info` authentication non-match, never the ERROR-level
        // structural rejection (`AGENTS.md` §2.1 / §19.4).
        assert!(!sink.ids().contains(&4134), "{:?}", sink.ids());
        assert_eq!(rejection_record(err).0, ROOT_UNLOCK_KEY_REJECTED);
        assert!(!sink.ids().contains(&4133), "{:?}", sink.ids());
    }

    // --- mount_root_disk_and_load_users (the single-disk split) --------

    #[test]
    fn a_partitioned_disk_splits_unlocks_and_loads_end_to_end() {
        // The single-disk boot entry: one whole-disk device carrying an
        // MBR boot+root layout is split by role into bounds-checked
        // windows, the descriptor is read off the FAT boot window, and the
        // encrypted root window mounts under the passphrase to yield the
        // usable users database (`plans/PI.md` P11 Chunk B-2). The disk is
        // the *same* whole-disk image the `-M virt` root-mount->login QEMU
        // vertical plants on its virtio-blk backing, so the in-memory split
        // test and the live (emulated) board exercise one on-disk layout
        // (`AGENTS.md` §2.2).
        let bytes = disk_image::build_image().expect("the whole-disk image assembles");
        let disk = FatVecBlock { store: bytes };
        let sink = RecordingSink::new();

        let source = mount_root_disk_and_load_users(disk, disk_image::PASSPHRASE, &sink)
            .expect("the disk splits and the root unlocks end to end");

        let text = source.text().expect("a loaded holder serves its text");
        let db = UsersDb::parse(core::str::from_utf8(text).expect("utf-8"))
            .expect("the served database parses");
        db.authenticate(disk_image::USERNAME, disk_image::PASSWORD.as_bytes())
            .expect("the planted account authenticates");
        assert!(sink.ids().contains(&4133), "{:?}", sink.ids());
        // The `/System`-volume mount is no longer part of this users-load
        // path; it is the separate `with_system_volume` mount seam (design
        // B), exercised by its own tests below.
        assert!(!sink.ids().contains(&4140), "{:?}", sink.ids());
    }

    #[test]
    fn a_disk_with_no_partition_table_serves_no_database() {
        // §2.9 / §5.4: a device with no recognised partition scheme (a
        // blank disk, no MBR signature, no GPT header) is refused whole;
        // the encrypted root is never touched and no database is served.
        let disk = FatVecBlock::new(64);
        let sink = RecordingSink::new();

        let err = mount_root_disk_and_load_users(disk, PASSPHRASE, &sink)
            .expect_err("a disk with no table must be refused");

        assert!(matches!(err, RootMountError::PartitionTable(_)), "{err:?}");
        assert_eq!(err.cause(), "partition_table_invalid");
        assert!(sink.ids().contains(&4134), "{:?}", sink.ids());
        assert!(!sink.ids().contains(&4133), "{:?}", sink.ids());
    }

    #[test]
    fn a_disk_without_a_root_partition_serves_no_database() {
        // §2.9: a valid table that carries a FAT boot partition but no
        // RustFS root partition is fail-closed NoRootPartition — the
        // encrypted root cannot be located, so none is mounted.
        let (descriptor_bytes, _key) = provision();
        let boot = author_boot_partition(&descriptor_bytes);
        let total_sectors = disk_image::BOOT_LBA + FAT_BOOT_SECTORS;
        let mut store =
            alloc::vec![0u8; usize::try_from(total_sectors).expect("fits") * FAT_SECTOR_BYTES];
        let table = mbr::encode(&[Partition {
            ty: PartitionType::FatBoot,
            start_lba: disk_image::BOOT_LBA,
            block_count: FAT_BOOT_SECTORS,
        }])
        .expect("the boot-only MBR encodes");
        store[..FAT_SECTOR_BYTES].copy_from_slice(&table);
        let boot_at = usize::try_from(disk_image::BOOT_LBA).expect("fits") * FAT_SECTOR_BYTES;
        store[boot_at..boot_at + boot.store.len()].copy_from_slice(&boot.store);
        let disk = FatVecBlock { store };
        let sink = RecordingSink::new();

        let err = mount_root_disk_and_load_users(disk, PASSPHRASE, &sink)
            .expect_err("a disk with no root partition must be refused");

        assert_eq!(err, RootMountError::NoRootPartition);
        assert_eq!(err.cause(), "no_root_partition");
        assert!(sink.ids().contains(&4134), "{:?}", sink.ids());
        assert!(!sink.ids().contains(&4133), "{:?}", sink.ids());
    }

    // --- unlock_root_disk_interactively (the prompt + retry policy) -----

    use core::sync::atomic::{AtomicUsize, Ordering};

    use rustos_abi::Errno;

    /// A console byte sink that accepts everything written to it, so the
    /// prompt never short-writes. The interactive tests assert the audit
    /// trail and the installed state, not the prompt bytes, so a counting
    /// sink would add nothing (`AGENTS.md` §2.3).
    struct AcceptConsole;

    impl ConsoleWrite for AcceptConsole {
        fn write(&self, bytes: &[u8]) -> Result<usize, Errno> {
            Ok(bytes.len())
        }
    }

    /// A scripted console input source: yields the planted bytes one at a
    /// time (matching the byte-by-byte line reader), reports end of input
    /// (`Ok(0)`) once they are spent, and — if `fail_at` is set — fails the
    /// read once the cursor reaches it, modelling a device fault. `Sync`
    /// through an atomic cursor over immutable bytes, as [`ConsoleRead`]
    /// requires.
    struct ScriptInput {
        bytes: Vec<u8>,
        cursor: AtomicUsize,
        fail_at: Option<usize>,
    }

    impl ScriptInput {
        fn new(bytes: Vec<u8>) -> Self {
            Self {
                bytes,
                cursor: AtomicUsize::new(0),
                fail_at: None,
            }
        }

        fn failing() -> Self {
            Self {
                bytes: Vec::new(),
                cursor: AtomicUsize::new(0),
                fail_at: Some(0),
            }
        }
    }

    impl ConsoleRead for ScriptInput {
        fn read(&self, buf: &mut [u8]) -> Result<usize, Errno> {
            if buf.is_empty() {
                return Ok(0);
            }
            let i = self.cursor.load(Ordering::Relaxed);
            if self.fail_at == Some(i) {
                return Err(Errno::NotImplemented);
            }
            if i >= self.bytes.len() {
                return Ok(0);
            }
            buf[0] = self.bytes[i];
            self.cursor.store(i + 1, Ordering::Relaxed);
            Ok(1)
        }
    }

    /// Join `lines` into a newline-terminated byte script the reader
    /// consumes one passphrase per line.
    fn script(lines: &[&[u8]]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for line in lines {
            bytes.extend_from_slice(line);
            bytes.push(b'\n');
        }
        bytes
    }

    /// Build the success whole-disk fixture the interactive tests unlock —
    /// the *same* MBR boot+root image (the root encrypted under
    /// [`PASSPHRASE`]'s derived key) the `-M virt` root-mount->login QEMU
    /// vertical plants on its virtio-blk backing (`AGENTS.md` §2.2).
    fn success_disk() -> FatVecBlock {
        let bytes = disk_image::build_image().expect("the whole-disk image assembles");
        FatVecBlock { store: bytes }
    }

    #[test]
    fn the_typed_passphrase_unlocks_and_installs_into_the_late_cell() {
        // The whole interactive path: the operator types the correct
        // passphrase at the first prompt, the disk unlocks, and the loaded
        // database is published into the set-once cell so login can
        // authenticate (`plans/PI.md` P11 Chunk B-2).
        let input = ScriptInput::new(script(&[PASSPHRASE]));
        let late = LateUsersDb::new();
        let sink = RecordingSink::new();

        let outcome = unlock_root_disk_interactively(
            success_disk(),
            &AcceptConsole,
            &input,
            &late,
            &sink,
            &|| {},
        );

        assert_eq!(outcome, UnlockOutcome::Installed);
        assert!(late.is_installed(), "the database is published");
        let text = late
            .text()
            .expect("the cell now serves the loaded database");
        let db = UsersDb::parse(core::str::from_utf8(text).expect("utf-8"))
            .expect("the served database parses");
        db.authenticate(
            image::USERS_FIXTURE_USERNAME,
            image::USERS_FIXTURE_PASSWORD.as_bytes(),
        )
        .expect("the planted account authenticates through the installed cell");
        assert!(
            sink.ids().contains(&4136),
            "install audited: {:?}",
            sink.ids()
        );
        assert!(
            !sink.ids().contains(&4137),
            "no retry on a first-try success: {:?}",
            sink.ids()
        );
    }

    #[test]
    fn a_blank_passphrase_volume_auto_unlocks_with_no_prompt() {
        // The installer image is provisioned with a **blank** root
        // passphrase (`AGENTS.md` §11): the unlock must mount it silently,
        // with no console read at all, so a fresh install boots straight
        // into the §11 installer rather than stalling behind a prompt the
        // operator cannot answer (`plans/PI.md` P11). The console input is
        // wired to *fail* on every read, so a successful unlock proves the
        // interactive prompt path was never entered.
        let bytes = disk_image::build_image_with_passphrase(b"")
            .expect("the blank-passphrase image assembles");
        let disk = FatVecBlock { store: bytes };
        let late = LateUsersDb::new();
        let sink = RecordingSink::new();

        let outcome = unlock_root_disk_interactively(
            disk,
            &AcceptConsole,
            &ScriptInput::failing(),
            &late,
            &sink,
            &|| {},
        );

        assert_eq!(outcome, UnlockOutcome::Installed);
        assert!(late.is_installed(), "the database is published");
        let text = late.text().expect("the cell serves the loaded database");
        let db = UsersDb::parse(core::str::from_utf8(text).expect("utf-8"))
            .expect("the served database parses");
        db.authenticate(
            image::USERS_FIXTURE_USERNAME,
            image::USERS_FIXTURE_PASSWORD.as_bytes(),
        )
        .expect("the planted account authenticates through the installed cell");
        assert!(
            sink.ids().contains(&4136),
            "install audited: {:?}",
            sink.ids()
        );
        // No prompt was drawn and no console read was attempted, so there
        // is no retry: the silent blank attempt unlocked it outright.
        assert!(
            !sink.ids().contains(&4137),
            "no retry on a silent auto-unlock: {:?}",
            sink.ids()
        );
    }

    #[test]
    fn wrong_passphrases_are_retried_then_the_correct_one_unlocks() {
        // A bounded retry: two wrong passphrases are refused (each audited
        // `4137`, no oracle) and the third, correct one unlocks and
        // installs — the same disk is reused across attempts.
        let input = ScriptInput::new(script(&[b"nope", b"still wrong", PASSPHRASE]));
        let late = LateUsersDb::new();
        let sink = RecordingSink::new();

        let outcome = unlock_root_disk_interactively(
            success_disk(),
            &AcceptConsole,
            &input,
            &late,
            &sink,
            &|| {},
        );

        assert_eq!(outcome, UnlockOutcome::Installed);
        assert!(late.is_installed());
        let retries = sink.ids().iter().filter(|&&id| id == 4137).count();
        assert_eq!(retries, 2, "two wrong attempts retried: {:?}", sink.ids());
        assert!(sink.ids().contains(&4136), "{:?}", sink.ids());
    }

    #[test]
    fn the_attempt_budget_is_bounded_then_gives_up_fail_closed() {
        // §2.1 / §5.4.5: after MAX_UNLOCK_ATTEMPTS wrong passphrases the
        // unlock gives up rather than looping forever, and the late cell
        // stays empty so every login is refused until reboot.
        let lines = alloc::vec![b"wrong" as &[u8]; MAX_UNLOCK_ATTEMPTS as usize];
        let input = ScriptInput::new(script(&lines));
        let late = LateUsersDb::new();
        let sink = RecordingSink::new();

        let outcome = unlock_root_disk_interactively(
            success_disk(),
            &AcceptConsole,
            &input,
            &late,
            &sink,
            &|| {},
        );

        assert_eq!(outcome, UnlockOutcome::GaveUp);
        assert!(!late.is_installed(), "no database is installed on give-up");
        let retries = sink.ids().iter().filter(|&&id| id == 4137).count();
        assert_eq!(
            retries,
            MAX_UNLOCK_ATTEMPTS as usize,
            "every attempt is a refused retry: {:?}",
            sink.ids()
        );
        assert!(
            sink.ids().contains(&4138),
            "give-up audited: {:?}",
            sink.ids()
        );
    }

    #[test]
    fn a_structural_failure_gives_up_without_retrying() {
        // A disk with no partition table cannot be fixed by re-prompting,
        // so the unlock gives up on the first attempt (no `4137` retry) and
        // fails closed.
        let input = ScriptInput::new(script(&[PASSPHRASE]));
        let late = LateUsersDb::new();
        let sink = RecordingSink::new();

        let outcome = unlock_root_disk_interactively(
            FatVecBlock::new(64),
            &AcceptConsole,
            &input,
            &late,
            &sink,
            &|| {},
        );

        assert_eq!(outcome, UnlockOutcome::GaveUp);
        assert!(!late.is_installed());
        assert!(
            !sink.ids().contains(&4137),
            "a structural failure is not retried: {:?}",
            sink.ids()
        );
        assert!(sink.ids().contains(&4138), "{:?}", sink.ids());
    }

    #[test]
    fn the_console_is_released_on_every_unlock_outcome() {
        // Regression (`plans/PI.md` P11): the unlock kthread owns console 0
        // for the passphrase prompt and must hand it back to `login` the
        // instant the unlock resolves — on **both** a successful install and
        // a fail-closed give-up. A successful unlock once skipped that
        // release, leaving the console-0 gate latched shut and the UART
        // receive interrupt masked, so neither the keyboard nor the serial
        // `login` could be typed into even though the root had mounted.
        // `unlock_root_disk_interactively` now fires `on_resolved` exactly
        // once on every internal return path; prove it for both outcomes.
        use core::cell::Cell;

        // Success path (a correct passphrase installs a database).
        let releases = Cell::new(0u32);
        let outcome = unlock_root_disk_interactively(
            success_disk(),
            &AcceptConsole,
            &ScriptInput::new(script(&[PASSPHRASE])),
            &LateUsersDb::new(),
            &RecordingSink::new(),
            &|| releases.set(releases.get() + 1),
        );
        assert_eq!(outcome, UnlockOutcome::Installed);
        assert_eq!(
            releases.get(),
            1,
            "a successful unlock releases console 0 exactly once"
        );

        // Give-up path (a structural failure resolves with no database).
        let releases = Cell::new(0u32);
        let outcome = unlock_root_disk_interactively(
            FatVecBlock::new(64),
            &AcceptConsole,
            &ScriptInput::new(script(&[PASSPHRASE])),
            &LateUsersDb::new(),
            &RecordingSink::new(),
            &|| releases.set(releases.get() + 1),
        );
        assert_eq!(outcome, UnlockOutcome::GaveUp);
        assert_eq!(
            releases.get(),
            1,
            "a fail-closed unlock still releases console 0 exactly once"
        );
    }

    #[test]
    fn an_unreadable_console_gives_up_fail_closed() {
        // §2.9: a console whose read faults cannot deliver a passphrase;
        // the unlock fails closed rather than mounting under an empty or
        // fabricated secret, and never touches the disk.
        let input = ScriptInput::failing();
        let late = LateUsersDb::new();
        let sink = RecordingSink::new();

        let outcome = unlock_root_disk_interactively(
            success_disk(),
            &AcceptConsole,
            &input,
            &late,
            &sink,
            &|| {},
        );

        assert_eq!(outcome, UnlockOutcome::GaveUp);
        assert!(!late.is_installed());
        assert!(
            !sink.ids().contains(&4133),
            "the disk is never unlocked: {:?}",
            sink.ids()
        );
        assert!(sink.ids().contains(&4138), "{:?}", sink.ids());
    }

    #[test]
    fn an_over_long_line_is_a_wrong_attempt_not_a_truncated_secret() {
        // §5.4.3: a line longer than MAX_PASSPHRASE_LEN is drained and
        // counted as a wrong attempt (audited `4137`), never silently
        // truncated to a shorter secret; the next, correct line unlocks.
        let over_long = alloc::vec![b'a'; MAX_PASSPHRASE_LEN + 16];
        let input = ScriptInput::new(script(&[&over_long, PASSPHRASE]));
        let late = LateUsersDb::new();
        let sink = RecordingSink::new();

        let outcome = unlock_root_disk_interactively(
            success_disk(),
            &AcceptConsole,
            &input,
            &late,
            &sink,
            &|| {},
        );

        assert_eq!(outcome, UnlockOutcome::Installed);
        assert!(late.is_installed());
        let retries = sink.ids().iter().filter(|&&id| id == 4137).count();
        assert_eq!(
            retries,
            1,
            "the over-long line is one retry: {:?}",
            sink.ids()
        );
    }

    // --- with_system_volume (the one design-B /System mount seam) --------

    #[test]
    fn with_system_volume_runs_the_continuation_and_returns_its_result() {
        // The one mount seam (`AGENTS.md` §2.2): it mounts the read-only
        // `/System` volume, runs `f` against the still-open volume, and
        // returns `Some(f(..))`. The volume reads cleanly inside `f` — proven
        // by reading the §16.2 `Drivers` store directory back out.
        let mut disk = success_disk();
        let sink = RecordingSink::new();

        let found_store = with_system_volume(&mut disk, &sink, |volume| {
            let root = volume.root();
            volume.lookup(root, b"Drivers").is_ok()
        });

        assert_eq!(
            found_store,
            Some(true),
            "the continuation runs against the mounted volume and its result is returned"
        );
        assert!(sink.ids().contains(&4140), "{:?}", sink.ids());
        assert!(!sink.ids().contains(&4141), "{:?}", sink.ids());
    }

    #[test]
    fn with_system_volume_returns_none_and_never_runs_the_continuation_without_a_volume() {
        // §18.4 / §2.9: with no recognised partition table there is no
        // `/System` volume, so `with_system_volume` returns `None` without
        // ever running the continuation, audits the unavailable case
        // (`4141`), and never the mounted one (`4140`).
        let mut disk = FatVecBlock::new(64);
        let sink = RecordingSink::new();

        let ran = with_system_volume(&mut disk, &sink, |_volume| {
            panic!("the continuation must not run when no /System volume mounts");
        });

        assert_eq!(ran, None);
        assert!(sink.ids().contains(&4141), "{:?}", sink.ids());
        assert!(!sink.ids().contains(&4140), "{:?}", sink.ids());
    }
}
