//! Platform image builders for TAIRiX.
//!
//! `tairix-mkimage` authors flashable images in pure Rust: the partition
//! contents are laid down by the **real** in-tree filesystem drivers — the
//! same code the booted system mounts the volumes with — so the image
//! author and its consumer can never drift. There is no
//! shelling out to `parted`/`mkfs`/`xorriso`.
//!
//! ## `images/tairix-aarch64-rpi.img` (`plans/PI.md` P9)
//!
//! [`build_rpi_image`] assembles the flashable Raspberry Pi 4 SD image:
//!
//! - **MBR** ([`mbr`]): two primary partitions, both 1 MiB-aligned.
//! - **Boot partition** ([`fatboot`], FAT32, [`BOOT_PART_SECTORS`]): the
//!   pinned third-party firmware blobs ([`firmware`]),
//!   the generated `config.txt`, and `kernel8.img` — the freestanding
//!   aarch64 `tairix-kernel` ELF flattened by [`elfflat`].
//! - **Root partition** ([`rootfs`], `ARXFS`, [`ROOT_PART_SECTORS`]): an
//!   encrypted volume carrying the directory skeleton. Its
//!   volume key is **derived from a passphrase**: the
//!   build provisions an
//!   [`UnlockDescriptor`] (a
//!   per-volume random salt + PBKDF2 iteration count), derives the volume
//!   key from the profile's [`passphrase_for`] under it, provisions the
//!   root with that key, and lays the plaintext descriptor on the boot
//!   partition
//!   ([`fatboot::ROOT_UNLOCK_NAME`]) so the bootstrap can re-derive the key
//!   before mounting. The passphrase itself is never stored in the image.
//!
//! Two [`ImageProfile`]s exist, and both seed **human accounts only**: the
//! system/service identity is compiled into the kernel
//! (`tairix_users::system_accounts` / `system_groups`, `plans/USERS.md`)
//! and never written to disk. **Installer** is the shippable form: it
//! seeds an *empty* users database (the installer authors the first human
//! user on first boot), and its encrypted root is unlocked by a **blank**
//! passphrase ([`INSTALLER_PASSPHRASE`]) the bootstrap auto-enters with no
//! prompt, so a fresh install boots straight into the installer. **Debug**
//! is the development form: it seeds an interactive `root`/`root`
//! administrator ([`DEBUG_USERNAME`]/[`DEBUG_PASSWORD`], uid
//! [`DEBUG_UID`], salted and hashed per build), and its encrypted root is
//! unlocked by the matching `root` passphrase ([`DEBUG_PASSPHRASE`], typed
//! at the `Root passphrase:` prompt), so the login prompt is usable
//! without running the installer. A debug image must never ship.
//!
//! The builder is driven by `cargo xtask image --target aarch64-rpi` (or
//! `cargo xtask build --target aarch64-rpi`) and by the `tairix-mkimage`
//! binary directly; see `docs/src/install/raspberry_pi.md`.

use std::fmt;
use std::io::Read;

use tairix_arch_aarch64::uart_init::CONSOLE_BAUD;

pub mod device;
pub mod elfflat;
pub mod fatboot;
pub mod firmware;
pub mod library;
pub mod rootfs;

pub use tairix_drv_fs_arxfs::{
    EntropySource, UnlockDescriptor, VolumeKey, UNLOCK_DEFAULT_ITERATIONS, UNLOCK_DESCRIPTOR_LEN,
    VOLUME_KEY_LEN,
};

use device::SECTOR_BYTES;
use firmware::FirmwareFile;
use tairix_abi::{DriverError, MACHINE_ID_LEN};
use tairix_partition::mbr::{self, MbrError};
use tairix_partition::{Partition, PartitionType};
use tairix_users::{
    AccountState, Gid, GroupRecord, GroupsDb, Identity, Salt, Uid, UserRecord, UsersDb, STORAGE_GID,
};

/// First sector of the FAT32 boot partition (1 MiB alignment, the
/// universal SD-card convention).
pub const BOOT_PART_LBA: u32 = 2048;

/// Sectors in the FAT32 boot partition: 64 MiB — ample for the firmware
/// blobs (~2.5 MiB) plus the kernel, while keeping the image small.
pub const BOOT_PART_SECTORS: u32 = 131_072;

/// First sector of the read-only `ARXFS` `/System` partition (contiguous
/// with the boot partition, which already ends 1 MiB-aligned). This is the
/// design-B pre-unlock signed-driver store (`plans/PI.md`).
pub const SYSTEM_PART_LBA: u32 = BOOT_PART_LBA + BOOT_PART_SECTORS;

/// Sectors in the read-only `ARXFS` `/System` partition: 128 MiB — the
/// skeleton plus the signed driver bundles and the discovered program and
/// service stores (`/System/Commands`, `/System/Applications`,
/// `/System/Services`), each app a self-contained `Run` rxe beside its
/// `Help/` tree, with headroom for the stores to keep growing as apps are
/// added.
pub const SYSTEM_PART_SECTORS: u32 = 262_144;

/// First sector of the encrypted `ARXFS` data-root partition (contiguous
/// with the `/System` partition, which already ends 1 MiB-aligned).
pub const ROOT_PART_LBA: u32 = SYSTEM_PART_LBA + SYSTEM_PART_SECTORS;

/// Sectors in the `ARXFS` root partition: 64 MiB — the skeleton plus
/// installer headroom. The installer grows the layout on first boot;
/// `ARXFS::grow` expands a volume to its device, so a card-sized root is
/// a first-boot job, not an image-build job.
pub const ROOT_PART_SECTORS: u32 = 131_072;

/// Total sectors in the assembled image.
pub const IMAGE_SECTORS: u32 = ROOT_PART_LBA + ROOT_PART_SECTORS;

/// Everything that can go wrong while authoring an image. Every variant is
/// a refusal: mkimage never emits a best-effort image.
#[derive(Debug)]
pub enum MkimageError {
    /// The firmware pin manifest is malformed or incomplete.
    Manifest(String),
    /// A pinned firmware blob is missing or fails verification.
    Firmware(String),
    /// The kernel ELF cannot be flattened into `kernel8.img`.
    KernelElf(&'static str),
    /// The requested MBR partition table is invalid.
    Partition(MbrError),
    /// Authoring the FAT32 boot partition failed.
    BootPartition(DriverError),
    /// Authoring the read-only `ARXFS` `/System` partition failed.
    SystemPartition(DriverError),
    /// Authoring the `ARXFS` root partition failed.
    RootPartition(DriverError),
    /// Host randomness for the volume key is unavailable.
    Entropy(String),
    /// Provisioning or encoding the passphrase-unlock descriptor failed.
    Unlock(DriverError),
    /// Authoring the seeded user database failed.
    UsersDb(String),
    /// Authoring the seeded group registry failed.
    GroupsDb(String),
    /// Deriving the shipped program-library catalog from the planted
    /// bundles failed.
    LibraryCatalog(String),
    /// The image's `network.conf` addressing default does not parse through
    /// the `tairix_netconfig` engine `netstack` reads it with, so shipping it
    /// would give the booted system a store its own stack rejects.
    NetworkConfig,
}

impl fmt::Display for MkimageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Manifest(msg) => write!(f, "firmware manifest: {msg}"),
            Self::Firmware(msg) => write!(f, "firmware input: {msg}"),
            Self::KernelElf(msg) => write!(f, "kernel ELF: {msg}"),
            Self::Partition(err) => write!(f, "partition table: {err:?}"),
            Self::BootPartition(err) => write!(f, "boot partition: driver error {err:?}"),
            Self::SystemPartition(err) => write!(f, "system partition: driver error {err:?}"),
            Self::RootPartition(err) => write!(f, "root partition: driver error {err:?}"),
            Self::Entropy(msg) => write!(f, "host entropy: {msg}"),
            Self::Unlock(err) => write!(f, "unlock descriptor: driver error {err:?}"),
            Self::NetworkConfig => {
                f.write_str("network configuration: the image's network.conf does not parse")
            }
            Self::UsersDb(msg) => write!(f, "users database: {msg}"),
            Self::GroupsDb(msg) => write!(f, "group registry: {msg}"),
            Self::LibraryCatalog(msg) => write!(f, "program-library catalog: {msg}"),
        }
    }
}

impl From<MbrError> for MkimageError {
    fn from(err: MbrError) -> Self {
        Self::Partition(err)
    }
}

impl std::error::Error for MkimageError {}

/// Which kind of image to author.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ImageProfile {
    /// Development image: the default system/service account set plus the
    /// interactive [`DEBUG_USERNAME`]/[`DEBUG_PASSWORD`] administrator and
    /// its [`DEBUG_GROUP`] group, so the login prompt is usable — and the
    /// kernel's identity table builds — without the installer. Never shipped.
    Debug,
    /// Shippable image: the default no-login system/service account set
    /// only; the installer appends the first human user to
    /// `/System/Security/Users` and `/System/Security/Groups` on first boot.
    Installer,
}

impl ImageProfile {
    /// The stable name used in image filenames and the CLI.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Installer => "installer",
        }
    }

    /// The profile named `name`, if any. Exact and case-sensitive, so a
    /// profile flag has one spelling (fail closed).
    #[must_use]
    pub fn from_label(name: &str) -> Option<Self> {
        match name {
            "debug" => Some(Self::Debug),
            "installer" => Some(Self::Installer),
            _ => None,
        }
    }

    /// The number of image profiles — the length a per-profile (or
    /// per-`(arch, profile)`) memo table indexed by [`Self::index`] must
    /// have. Kept in lockstep with the variants by
    /// `index_covers_every_profile`.
    pub const COUNT: usize = 2;

    /// This profile's stable index, so a builder can memoise one composed
    /// artefact per `(arch, profile)` pair in a fixed-size array without a
    /// runtime map.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Debug => 0,
            Self::Installer => 1,
        }
    }

    /// The extra `cargo build` arguments that select the Cargo profile this
    /// image compiles its Rust artefacts (the kernel and every user-space
    /// `Run` binary) in: the non-shippable `debug` image builds in Cargo's
    /// `dev` profile (`debug_assertions` on, no extra argument), and the
    /// shippable `installer` image builds `--release` (optimised,
    /// `debug_assertions` off). This is the single source of truth for the
    /// image → Cargo-profile mapping, paired with [`Self::cargo_profile_dir`].
    #[must_use]
    pub const fn cargo_build_args(self) -> &'static [&'static str] {
        match self {
            Self::Debug => &[],
            Self::Installer => &["--release"],
        }
    }

    /// The `target/<triple>/<dir>/` subdirectory Cargo writes this profile's
    /// artefacts to — `debug` for the `dev` profile, `release` for
    /// `--release`. The read-back path counterpart of
    /// [`Self::cargo_build_args`].
    #[must_use]
    pub const fn cargo_profile_dir(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Installer => "release",
        }
    }
}

/// Username of the debug-profile test account.
pub const DEBUG_USERNAME: &str = "root";

/// Password of the debug-profile test account. Knowable by design — the
/// debug image exists for bring-up on development hardware and must never
/// ship; the installer image seeds no login-capable account at all.
pub const DEBUG_PASSWORD: &str = "root";

/// Uid of the debug-profile test account: the first interactive-user id
/// ([`tairix_users::FIRST_USER_UID`]). Uid 0 belongs to the no-login
/// `system` record — powers come from the account's capability ceiling,
/// never from its uid, so the debug administrator is an ordinary
/// user-band principal.
pub const DEBUG_UID: Uid = Uid(tairix_users::FIRST_USER_UID);

/// Primary group id of the debug-profile test account: the first
/// interactive-user gid ([`tairix_users::FIRST_USER_GID`]). Defined once so
/// the seeded user's `primary_gid` and the group registry it must resolve
/// against cannot disagree about which gid exists.
pub const DEBUG_PRIMARY_GID: Gid = Gid(tairix_users::FIRST_USER_GID);

/// Name of the debug-profile primary group (gid [`DEBUG_PRIMARY_GID`]). The
/// conventional administrative group; the seeded `root` account's powers come
/// from capabilities, not from this group.
pub const DEBUG_GROUP: &str = "wheel";

/// Passphrase the **debug** image's encrypted root is unlocked with.
///
/// The debug profile is a bring-up image that must never ship (see
/// [`ImageProfile::Debug`]); its root account is `root`/`root`
/// ([`DEBUG_USERNAME`] / [`DEBUG_PASSWORD`]), and for a consistent,
/// memorable bring-up experience the encrypted-root unlock passphrase is
/// the same word. The operator types `root` at the `Root passphrase:`
/// prompt to unlock the volume. Knowable by design — like the seeded
/// account, it exists only for development hardware.
pub const DEBUG_PASSPHRASE: &[u8] = b"root";

/// Passphrase the **installer** image's encrypted root is provisioned
/// under — **blank**.
///
/// The installer image's root is **re-provisioned by the installer**,
/// which sets the user's real, operator-chosen passphrase when it authors
/// the production root on first boot. Until then a blank passphrase
/// unlocks it (auto-enterable, so a fresh install does not stall). The
/// volume is still fully encrypted: a blank passphrase is run through
/// PBKDF2 over the descriptor's per-volume random salt to derive a real
/// 256-bit [`VolumeKey`], exactly as a typed one would be. A shippable,
/// user-installed root MUST be unlocked by a passphrase the operator
/// chooses at install time — never this blank default.
pub const INSTALLER_PASSPHRASE: &[u8] = b"";

/// The encrypted-root unlock passphrase each [`ImageProfile`] is
/// provisioned under.
///
/// The passphrase is fully determined by the profile (there is no
/// operator choice at *image-build* time — the installer is where a
/// real passphrase is chosen), so it is derived here rather than passed in
/// alongside the profile, which removes any chance of provisioning an
/// image under a passphrase that disagrees with the one its prompt expects
/// (one source of truth).
#[must_use]
pub const fn passphrase_for(profile: ImageProfile) -> &'static [u8] {
    match profile {
        ImageProfile::Debug => DEBUG_PASSPHRASE,
        ImageProfile::Installer => INSTALLER_PASSPHRASE,
    }
}

/// Build the profile's `/System/Security/Users` text.
///
/// The on-disk database holds **human** accounts only: the system/service
/// identity is compiled into the kernel (`tairix_users::system_accounts`,
/// `plans/USERS.md`) and the kernel's identity merge refuses any on-disk
/// record colliding with it, so neither profile writes those records to
/// disk. The installer profile therefore seeds an *empty* database (its
/// first human account is created by the installer's first-boot
/// provisioning); the debug profile seeds the interactive
/// [`DEBUG_USERNAME`] administrator (uid [`DEBUG_UID`], its password
/// salted from `entropy` and hashed at the default PBKDF2 cost) granted
/// the administrator capability ceiling
/// (`tairix_users::administrator_ceiling`) a bring-up session needs.
/// Powers come from capabilities, not from a uid: the account is an
/// administrator only because its ceiling says so.
fn users_db(
    profile: ImageProfile,
    entropy: &mut dyn EntropySource,
) -> Result<String, MkimageError> {
    let mut records = Vec::new();
    if profile == ImageProfile::Debug {
        let mut salt: Salt = [0u8; tairix_users::SALT_LEN];
        entropy
            .fill(&mut salt)
            .map_err(|e| MkimageError::Entropy(format!("users salt: {e:?}")))?;

        let record = UserRecord::with_password(
            Identity {
                username: DEBUG_USERNAME,
                uid: DEBUG_UID,
                primary_gid: DEBUG_PRIMARY_GID,
                supplementary_gids: &[STORAGE_GID],
                display_name: "System Administrator",
                home: Some(&tairix_users::default_home(DEBUG_USERNAME)),
                shell: Some(tairix_users::DEFAULT_SHELL),
                capabilities: tairix_users::administrator_ceiling(),
                state: AccountState::Active,
            },
            DEBUG_PASSWORD.as_bytes(),
            salt,
            tairix_users::DEFAULT_ITERATIONS,
        )
        .map_err(|e| MkimageError::UsersDb(format!("debug root record: {e}")))?;
        records.push(record);
    }
    let db = UsersDb::new(records)
        .map_err(|e| MkimageError::UsersDb(format!("seeded database: {e}")))?;
    Ok(db.serialise())
}

/// Build the profile's `/System/Security/Groups` text.
///
/// Both profiles seed the well-known removable-storage group
/// ([`tairix_users::STORAGE_GROUP`]) the unlock resolves by name to arm
/// the hotplug-volume identity map (`plans/DEVICES.md` D3d) — storage
/// membership is admin-managed data about human accounts, so it lives on
/// disk beside them. The `system` and `services` groups are compiled into
/// the kernel with the system accounts (`plans/USERS.md`) and never
/// written to disk. The debug profile appends the [`DEBUG_GROUP`] group
/// (gid [`DEBUG_PRIMARY_GID`]) the seeded administrator's primary gid
/// references — so the kernel's identity merge resolves that reference
/// against a real registry rather than failing closed on a dangling
/// group. Membership is not stored here — it lives in the user records;
/// this is only the authoritative name↔gid set.
fn groups_db(profile: ImageProfile) -> Result<String, MkimageError> {
    let mut records = vec![GroupRecord::new(tairix_users::STORAGE_GROUP, STORAGE_GID)
        .map_err(|e| MkimageError::GroupsDb(format!("storage group record: {e}")))?];
    if profile == ImageProfile::Debug {
        records.push(
            GroupRecord::new(DEBUG_GROUP, DEBUG_PRIMARY_GID)
                .map_err(|e| MkimageError::GroupsDb(format!("debug group record: {e}")))?,
        );
    }
    let db = GroupsDb::new(records)
        .map_err(|e| MkimageError::GroupsDb(format!("seeded registry: {e}")))?;
    Ok(db.serialise())
}

/// Build the debug-profile log-attestation key file image
/// (`PREREQUISITES.md` P-E): a fresh random [`tairix_log::LogAttestationKey`]
/// drawn from `entropy`, serialised to its on-disk form. Only a debug image
/// bakes a key; an installer image's per-installation key is generated at
/// first boot (baking one common key into every installer image would be a
/// shared secret, a security hole).
/// Fails closed if the host entropy source cannot supply the key bytes.
fn debug_log_attestation_key(entropy: &mut dyn EntropySource) -> Result<Vec<u8>, MkimageError> {
    let mut key = [0u8; tairix_log::LOG_ATTESTATION_KEY_LEN];
    entropy
        .fill(&mut key)
        .map_err(|e| MkimageError::Entropy(format!("log-attestation key: {e:?}")))?;
    Ok(tairix_log::LogAttestationKey::from_key(key)
        .to_file_bytes()
        .to_vec())
}

/// Build the debug-profile per-installation machine-id: [`MACHINE_ID_LEN`]
/// fresh random bytes drawn from `entropy`. The machine-id is **non-secret**
/// per-installation identity (the TAIRiX equivalent of `/etc/machine-id`) that
/// the system log binds each stream's hash-chain genesis to
/// (`plans/SYSLOG.md` §7.1); giving each debug image its own random id keeps
/// two images' logs from sharing a genesis. Only a debug image bakes one; an
/// installer image mints its machine-id at first boot, exactly as it does the
/// log-attestation key. Fails closed if the host entropy source cannot supply
/// the bytes.
fn debug_machine_id(entropy: &mut dyn EntropySource) -> Result<[u8; MACHINE_ID_LEN], MkimageError> {
    let mut id = [0u8; MACHINE_ID_LEN];
    entropy
        .fill(&mut id)
        .map_err(|e| MkimageError::Entropy(format!("machine-id: {e:?}")))?;
    Ok(id)
}

/// The assembled image plus the material the operator must keep.
pub struct RpiImage {
    /// The flashable image bytes ([`IMAGE_SECTORS`] sectors).
    pub image: Vec<u8>,
    /// The passphrase-unlock descriptor the root was provisioned under,
    /// laid down in the clear on the boot partition
    /// ([`fatboot::ROOT_UNLOCK_NAME`]). It is not a secret — only the salt
    /// and iteration count needed to re-derive the volume key from the
    /// passphrase.
    pub unlock: UnlockDescriptor,
    /// The root volume key the image was provisioned under, derived from
    /// the profile's [`passphrase_for`] and [`Self::unlock`]. Mounting the
    /// root needs it; the key itself is stored nowhere inside the image (it
    /// can be re-derived from the on-image descriptor and the passphrase).
    pub root_key: VolumeKey,
}

/// Build the flashable Raspberry Pi 4 SD image.
///
/// `kernel_elf` is the freestanding aarch64 `tairix-kernel` ELF;
/// `firmware` is the verified blob set from
/// [`firmware::FirmwareManifest::load_dir`]; `entropy` draws the unlock
/// descriptor's salt, the root volume's internal key hierarchy, and, on a
/// debug build, the seeded account's password salt; `profile` selects the
/// [`ImageProfile`] **and** the encrypted-root unlock passphrase
/// ([`passphrase_for`]) — `root` for the debug image, blank for the
/// installer. The passphrase is not a separate argument:
/// deriving it from the profile makes it impossible to provision an image
/// under a passphrase that disagrees with the one its prompt expects.
///
/// The root is encrypted under a [`VolumeKey`] **derived** from the
/// profile's passphrase through a freshly provisioned
/// [`UnlockDescriptor`]; the
/// plaintext descriptor is laid on the boot partition
/// ([`fatboot::ROOT_UNLOCK_NAME`]) so the bootstrap re-derives the key
/// from the passphrase before mounting.
///
/// `drivers` is the set of signed `.rxe` driver bundles to install into the
/// read-only `/System/Drivers/` store, each as `(components, bytes)` where
/// `components` is the bundle's leaf path **relative to the `/System` volume
/// root** (for example `&[b"Drivers", b"bus_mailbox", b"vcmailbox", b"Run"]`)
/// and `bytes` is the bundle exactly as the store scan reads it back.
/// The caller cross-compiles and signs each bundle (this crate stays pure —
/// it never drives `cargo`); `build_rpi_image` only plants the bytes. The
/// kernel verifies every bundle against its embedded trust anchor at load, so
/// a tampered read-only store fails closed. An empty slice
/// produces an image with no autoloaded drivers (every node left unbound).
///
/// `apps` is the set of application-bundle files to plant into the read-only
/// system program and service stores, each as `(components, bytes)` where
/// `components` is the file's path relative to the `/System` volume root
/// (for example `&[b"Commands", b"ls.app", b"AppInfo"]`) — every program's
/// signed `AppInfo` + `Run` beside its `Help/` tree, so each bundle is a
/// complete, self-contained on-disk directory (`plans/APPS.md` deliverable
/// 8). As with `drivers`, the caller composes and signs the files; this
/// crate only plants them.
///
/// `network_conf` is the per-interface network configuration the image ships,
/// planted on the read-only `/System` volume at the volume-relative path its
/// only reader — the device manager's pre-unlock store read — resolves. The
/// caller composes it (so *which* interfaces an image manages is a property
/// of the image, not of this writer) and this crate validates it through the
/// one `tairix_netconfig` engine before planting it, so an image can never
/// ship an addressing default its own stack would reject.
///
/// # Errors
///
/// Any [`MkimageError`] from the kernel conversion, descriptor
/// provisioning, partition authoring, or assembly; the build fails closed
/// rather than emitting a partial image. [`MkimageError::NetworkConfig`] if
/// `network_conf` does not parse.
pub fn build_rpi_image(
    kernel_elf: &[u8],
    firmware: &[FirmwareFile],
    entropy: &mut dyn EntropySource,
    profile: ImageProfile,
    drivers: &[(&[&[u8]], &[u8])],
    apps: &[(&[&[u8]], &[u8])],
    network_conf: &str,
) -> Result<RpiImage, MkimageError> {
    let users_db = users_db(profile, entropy)?;
    let groups_db = groups_db(profile)?;
    // The debug image's interactive administrator gets its recorded home
    // provisioned account-owned and owner-only (a recorded home is a real
    // inode, never a dangling path); the installer image seeds no
    // login-capable account, so no home either.
    let home_dirs: &[(&str, u32, u32)] = match profile {
        ImageProfile::Debug => &[(DEBUG_USERNAME, DEBUG_UID.0, DEBUG_PRIMARY_GID.0)],
        ImageProfile::Installer => &[],
    };
    // Only a debug image bakes a log-attestation key; an installer image's
    // per-installation key is generated at first boot (a common baked key
    // would be a shared secret, a security hole). When present the bytes are
    // the `tairix_log::LogAttestationKey` on-disk image.
    let log_key_file = match profile {
        ImageProfile::Debug => Some(debug_log_attestation_key(entropy)?),
        ImageProfile::Installer => None,
    };
    // Likewise the non-secret per-installation machine-id: a debug image bakes
    // a random one (so two debug images do not share a log genesis); an
    // installer image mints it at first boot.
    let machine_id = match profile {
        ImageProfile::Debug => Some(debug_machine_id(entropy)?),
        ImageProfile::Installer => None,
    };
    // The machine-wide program-library catalog ships derived from the very
    // bundles this image plants (never a hand-maintained list); an image
    // with no listed application ships the canonical empty store.
    let library_conf = library::library_catalog(apps)?;
    let kernel8 = elfflat::elf_to_flat(kernel_elf, elfflat::KERNEL_LOAD_ADDR)?;

    // Derive the root volume key from the profile's passphrase under a
    // fresh per-volume descriptor, then lay the (non-secret) descriptor
    // beside the volume on the boot partition so the bootstrap can
    // re-derive it.
    let passphrase = passphrase_for(profile);
    let unlock = UnlockDescriptor::provision(UNLOCK_DEFAULT_ITERATIONS, entropy)
        .map_err(MkimageError::Unlock)?;
    let root_key = unlock.derive_volume_key(passphrase);
    let mut descriptor = [0u8; UNLOCK_DESCRIPTOR_LEN];
    unlock
        .encode(&mut descriptor)
        .map_err(MkimageError::Unlock)?;

    let boot = fatboot::build_boot_partition(
        u64::from(BOOT_PART_SECTORS),
        firmware,
        &kernel8,
        &descriptor,
        CONSOLE_BAUD,
    )?;
    let system = rootfs::build_system_partition(
        u64::from(SYSTEM_PART_SECTORS),
        entropy,
        drivers,
        apps,
        network_conf,
    )?;
    let root = rootfs::build_root_partition(
        u64::from(ROOT_PART_SECTORS),
        &root_key,
        entropy,
        &rootfs::RootSeed {
            users_db: &users_db,
            groups_db: &groups_db,
            home_dirs,
            log_attestation_key: log_key_file.as_deref(),
            machine_id: machine_id.as_ref().map(<[u8; MACHINE_ID_LEN]>::as_slice),
            library_conf: &library_conf,
        },
    )?;

    let mbr_sector = mbr::encode(&[
        Partition {
            ty: PartitionType::FatBoot,
            start_lba: u64::from(BOOT_PART_LBA),
            block_count: u64::from(BOOT_PART_SECTORS),
        },
        Partition {
            ty: PartitionType::ARXFSSystem,
            start_lba: u64::from(SYSTEM_PART_LBA),
            block_count: u64::from(SYSTEM_PART_SECTORS),
        },
        Partition {
            ty: PartitionType::ARXFSRoot,
            start_lba: u64::from(ROOT_PART_LBA),
            block_count: u64::from(ROOT_PART_SECTORS),
        },
    ])?;

    let mut image = vec![0u8; IMAGE_SECTORS as usize * SECTOR_BYTES];
    image[..SECTOR_BYTES].copy_from_slice(&mbr_sector);
    let boot_at = BOOT_PART_LBA as usize * SECTOR_BYTES;
    image[boot_at..boot_at + boot.len()].copy_from_slice(&boot);
    let system_at = SYSTEM_PART_LBA as usize * SECTOR_BYTES;
    image[system_at..system_at + system.len()].copy_from_slice(&system);
    let root_at = ROOT_PART_LBA as usize * SECTOR_BYTES;
    image[root_at..root_at + root.len()].copy_from_slice(&root);

    Ok(RpiImage {
        image,
        unlock,
        root_key,
    })
}

/// Cryptographic randomness from the host operating system
/// (`/dev/urandom`), used for the image's root volume key and the volume's
/// internal key hierarchy. Fails closed when the host source is
/// unavailable — an image is never provisioned with predictable key
/// material.
pub struct HostEntropy;

impl EntropySource for HostEntropy {
    fn fill(&mut self, out: &mut [u8]) -> Result<(), DriverError> {
        let mut source =
            std::fs::File::open("/dev/urandom").map_err(|_| DriverError::DeviceFault)?;
        source.read_exact(out).map_err(|_| DriverError::DeviceFault)
    }
}

/// Render a volume key as the 64-hex-digit key-file body.
#[must_use]
pub fn volume_key_to_hex(key: &VolumeKey) -> String {
    let mut text = String::with_capacity(VOLUME_KEY_LEN * 2 + 1);
    for byte in key {
        use fmt::Write as _;
        // Writing to a String cannot fail.
        let _ = write!(text, "{byte:02x}");
    }
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use device::MemBlock;
    use tairix_abi::driver::filesystem::{FilesystemRead, FilesystemWrite, NodeKind};
    use tairix_abi::CapabilityId;
    use tairix_drv_fs_arxfs::ARXFS;
    use tairix_drv_fs_fat32::Fat32;
    use tairix_users::STORAGE_GROUP;

    const TEST_KEY: VolumeKey = [0x42; VOLUME_KEY_LEN];

    /// Every profile has a distinct index below `COUNT`, so a
    /// `(arch, profile)` memo table sized `PieArch::COUNT * COUNT` and
    /// addressed by `index` never aliases two profiles onto one slot — the
    /// defect that let the `installer` image reuse the `debug` image's
    /// composed bundles.
    #[test]
    fn index_covers_every_profile() {
        let mut seen = [false; ImageProfile::COUNT];
        for profile in [ImageProfile::Debug, ImageProfile::Installer] {
            let idx = profile.index();
            assert!(idx < ImageProfile::COUNT, "{profile:?} index in range");
            assert!(!seen[idx], "{profile:?} index must be unique");
            seen[idx] = true;
        }
        assert!(seen.iter().all(|&s| s), "every slot is claimed once");
    }

    /// The image → Cargo-profile mapping: the non-shippable `debug` image
    /// builds in Cargo's `dev` profile (no `--release`, artefacts under
    /// `debug/`) and the shippable `installer` image builds `--release`
    /// (artefacts under `release/`). The single source both the kernel build
    /// and the user-space `Run` cross-compiles read.
    #[test]
    fn cargo_profile_mapping_matches_the_image_profile() {
        assert!(ImageProfile::Debug.cargo_build_args().is_empty());
        assert_eq!(ImageProfile::Debug.cargo_profile_dir(), "debug");
        assert_eq!(ImageProfile::Installer.cargo_build_args(), &["--release"]);
        assert_eq!(ImageProfile::Installer.cargo_profile_dir(), "release");
    }

    struct TestEntropy(u8);

    impl EntropySource for TestEntropy {
        fn fill(&mut self, out: &mut [u8]) -> Result<(), DriverError> {
            for byte in out.iter_mut() {
                *byte = self.0;
                self.0 = self.0.wrapping_add(1);
            }
            Ok(())
        }
    }

    /// A minimal valid kernel ELF reused from the converter's test
    /// builder: one code segment at the Pi load address.
    fn test_kernel_elf() -> Vec<u8> {
        elfflat::tests_support::sample_kernel(&[0xde, 0xad, 0xbe, 0xef])
    }

    /// The canonical empty `network.conf` these image tests ship: they plant
    /// no NIC driver, so the image manages no interface beyond loopback.
    fn test_network_conf() -> String {
        tairix_netconfig::NetworkConfig::default().render()
    }

    fn test_firmware() -> Vec<FirmwareFile> {
        vec![
            FirmwareFile {
                name: "start4.elf".into(),
                bytes: vec![0x11; 2048],
            },
            FirmwareFile {
                name: "fixup4.dat".into(),
                bytes: vec![0x22; 64],
            },
            FirmwareFile {
                name: "bcm2711-rpi-4-b.dtb".into(),
                bytes: vec![0x33; 512],
            },
        ]
    }

    /// Read the encoded unlock descriptor planted on a built image's FAT
    /// boot partition.
    fn read_unlock_descriptor(image: &[u8]) -> UnlockDescriptor {
        let boot_at = BOOT_PART_LBA as usize * SECTOR_BYTES;
        let boot_len = BOOT_PART_SECTORS as usize * SECTOR_BYTES;
        let boot = image[boot_at..boot_at + boot_len].to_vec();
        let mut fat = Fat32::open(MemBlock::from_bytes(boot).expect("whole sectors"))
            .expect("boot partition mounts");
        let root = fat.root();
        let node = fat
            .lookup(root, fatboot::ROOT_UNLOCK_NAME.as_bytes())
            .expect("root.unlock present");
        let mut bytes = [0u8; UNLOCK_DESCRIPTOR_LEN];
        let n = fat.read_at(node, 0, &mut bytes).expect("descriptor reads");
        assert_eq!(n, UNLOCK_DESCRIPTOR_LEN);
        UnlockDescriptor::decode(&bytes).expect("descriptor decodes")
    }

    #[test]
    fn assembles_a_flashable_image_both_partitions_mount() {
        let built = build_rpi_image(
            &test_kernel_elf(),
            &test_firmware(),
            &mut TestEntropy(9),
            ImageProfile::Installer,
            &[],
            &[],
            &test_network_conf(),
        )
        .expect("image builds");
        assert_eq!(built.image.len(), IMAGE_SECTORS as usize * SECTOR_BYTES);

        // The MBR carries the expected three-partition table: FAT boot,
        // read-only `/System`, encrypted data root.
        assert_eq!(built.image[510], 0x55);
        assert_eq!(built.image[511], 0xaa);
        assert_eq!(built.image[446 + 4], mbr::PART_TYPE_FAT32_LBA);
        assert_eq!(built.image[446 + 16 + 4], mbr::PART_TYPE_ARXFS_SYSTEM);
        assert_eq!(built.image[446 + 32 + 4], mbr::PART_TYPE_ARXFS);

        // The boot partition mounts and carries the flat kernel.
        let boot_at = BOOT_PART_LBA as usize * SECTOR_BYTES;
        let boot_len = BOOT_PART_SECTORS as usize * SECTOR_BYTES;
        let boot = built.image[boot_at..boot_at + boot_len].to_vec();
        let mut fat = Fat32::open(MemBlock::from_bytes(boot).expect("whole sectors"))
            .expect("boot partition mounts");
        let root = fat.root();
        let node = fat
            .lookup(root, fatboot::KERNEL_IMG_NAME.as_bytes())
            .expect("kernel8.img present");
        let mut kernel8 = [0u8; 16];
        let n = fat.read_at(node, 0, &mut kernel8).expect("kernel reads");
        assert_eq!(&kernel8[..n], &[0xde, 0xad, 0xbe, 0xef]);
        let info = fat.node_info(node).expect("kernel node info");
        assert_eq!(info.kind, NodeKind::RegularFile);

        // The on-disk descriptor re-derives exactly the volume key the
        // image was provisioned under (the bootstrap's path).
        let descriptor = read_unlock_descriptor(&built.image);
        assert_eq!(descriptor, built.unlock);
        assert_eq!(
            descriptor.derive_volume_key(INSTALLER_PASSPHRASE),
            built.root_key
        );

        // The root partition mounts under that re-derived key.
        let root_at = ROOT_PART_LBA as usize * SECTOR_BYTES;
        let root_len = ROOT_PART_SECTORS as usize * SECTOR_BYTES;
        let root_bytes = built.image[root_at..root_at + root_len].to_vec();
        let mut rfs = ARXFS::open(
            MemBlock::from_bytes(root_bytes).expect("whole sectors"),
            &descriptor.derive_volume_key(INSTALLER_PASSPHRASE),
        )
        .expect("root partition mounts");
        let arxfs_root = rfs.root();
        rfs.lookup(arxfs_root, b"System").expect("/System exists");

        // An installer image seeds the human-account security databases
        // (an empty users database — the first human user is a first-boot
        // job; the installer-profile content is pinned by
        // `an_installer_image_seeds_an_empty_users_database`).
        let system = rfs.lookup(arxfs_root, b"System").expect("/System");
        let security = rfs.lookup(system, b"Security").expect("Security");
        rfs.lookup(security, rootfs::USERS_DB_NAME.as_bytes())
            .expect("Users database exists");
        rfs.lookup(security, rootfs::GROUPS_DB_NAME.as_bytes())
            .expect("Groups registry exists");
    }

    #[test]
    fn the_system_partition_mounts_read_only_and_carries_the_skeleton() {
        use tairix_drv_fs_arxfs::SYSTEM_VOLUME_KEY;
        use tairix_partition::{parse_partition_table, PartitionBlock, PartitionType};

        let built = build_rpi_image(
            &test_kernel_elf(),
            &test_firmware(),
            &mut TestEntropy(9),
            ImageProfile::Installer,
            &[],
            &[],
            &test_network_conf(),
        )
        .expect("image builds");

        // The whole-disk table parses and locates the read-only `/System`
        // partition by role at the documented offset.
        let mut disk = MemBlock::from_bytes(built.image.clone()).expect("whole sectors");
        let table = parse_partition_table(&mut disk).expect("the MBR parses");
        let system = table
            .first_of_type(PartitionType::ARXFSSystem)
            .expect("a /System partition is present");
        assert_eq!(system.start_lba, u64::from(SYSTEM_PART_LBA));
        assert_eq!(system.block_count, u64::from(SYSTEM_PART_SECTORS));

        // It mounts read-only under the non-secret well-known key and its
        // root *is* `/System`, carrying the skeleton directly.
        let window = PartitionBlock::from_partition(disk, &system).expect("the /System window");
        let mut sys = ARXFS::open_read_only(window, &SYSTEM_VOLUME_KEY)
            .expect("/System mounts read-only under the public key");
        let sys_root = sys.root();
        sys.lookup(sys_root, b"Drivers").expect("/System/Drivers");
        let security = sys.lookup(sys_root, b"Security").expect("/System/Security");
        sys.lookup(security, b"Keys")
            .expect("/System/Security/Keys");
        // The store carries no users database (that secret stays on the
        // encrypted root).
        assert!(sys
            .lookup(security, rootfs::USERS_DB_NAME.as_bytes())
            .is_err());

        // A read-only mount refuses mutation fail-closed.
        assert_eq!(
            sys.create(sys_root, b"x", NodeKind::Directory),
            Err(DriverError::PermissionDenied)
        );
    }

    #[test]
    fn an_installed_driver_bundle_reads_back_from_the_readonly_system_store() {
        use tairix_drv_fs_arxfs::SYSTEM_VOLUME_KEY;
        use tairix_partition::{parse_partition_table, PartitionBlock, PartitionType};

        // A synthetic bundle blob: this test proves the *planting* (path +
        // bytes + intermediate-directory creation), not signature validity —
        // the signed-bundle composition is exercised where it is built. The store path mirrors the real mailbox
        // service-driver install (`Drivers/<class>/<leaf>/Run`).
        const BUNDLE: &[u8] = b"a signed .rxe driver bundle's bytes (synthetic)";
        let store_path: &[&[u8]] = &[b"Drivers", b"bus_mailbox", b"vcmailbox", b"Run"];

        let built = build_rpi_image(
            &test_kernel_elf(),
            &test_firmware(),
            &mut TestEntropy(9),
            ImageProfile::Installer,
            &[(store_path, BUNDLE)],
            &[],
            &test_network_conf(),
        )
        .expect("image builds");

        // Mount the read-only `/System` volume and read the planted bundle
        // back at its store path, byte-for-byte — the shape the store
        // scan reads.
        let mut disk = MemBlock::from_bytes(built.image).expect("whole sectors");
        let table = parse_partition_table(&mut disk).expect("the MBR parses");
        let system = table
            .first_of_type(PartitionType::ARXFSSystem)
            .expect("a /System partition is present");
        let window = PartitionBlock::from_partition(disk, &system).expect("the /System window");
        let mut sys = ARXFS::open_read_only(window, &SYSTEM_VOLUME_KEY)
            .expect("/System mounts read-only under the public key");

        let mut node = sys.root();
        for component in store_path {
            node = sys.lookup(node, component).expect("store path component");
        }
        let mut buf = vec![0u8; BUNDLE.len() + 16];
        let read = sys.read_at(node, 0, &mut buf).expect("bundle reads back");
        assert_eq!(&buf[..read], BUNDLE);
    }

    #[test]
    fn the_network_configuration_reads_back_from_the_readonly_system_store() {
        use tairix_abi::driver_store::SystemConfigFile;
        use tairix_drv_fs_arxfs::SYSTEM_VOLUME_KEY;
        use tairix_partition::{parse_partition_table, PartitionBlock, PartitionType};

        // The device manager reads this document off the read-only `/System`
        // volume through the pre-unlock store endpoint, so the whole assembly
        // has to land it there — not on the encrypted root, whose
        // `/System/Settings` sub-mount no bootstrap client can reach. An
        // image that plants it anywhere else boots with no managed interface
        // and never starts its DHCP client.
        let conf = "wan.kind ethernet\nwan.match.node 0xfd580000\nwan.ipv4.method dhcp\n";
        let built = build_rpi_image(
            &test_kernel_elf(),
            &test_firmware(),
            &mut TestEntropy(23),
            ImageProfile::Installer,
            &[],
            &[],
            conf,
        )
        .expect("image builds");

        let mut disk = MemBlock::from_bytes(built.image).expect("whole sectors");
        let table = parse_partition_table(&mut disk).expect("the MBR parses");
        let system = table
            .first_of_type(PartitionType::ARXFSSystem)
            .expect("a /System partition is present");
        let window = PartitionBlock::from_partition(disk, &system).expect("the /System window");
        let mut sys = ARXFS::open_read_only(window, &SYSTEM_VOLUME_KEY)
            .expect("/System mounts read-only under the public key");

        let mut node = sys.root();
        for component in SystemConfigFile::Network.volume_path().split('/') {
            node = sys
                .lookup(node, component.as_bytes())
                .expect("the ABI volume path resolves on the /System volume");
        }
        let mut buf = [0u8; tairix_netconfig::MAX_CONFIG_LEN];
        let read = sys.read_at(node, 0, &mut buf).expect("the document reads");
        let parsed = tairix_netconfig::NetworkConfig::parse(
            core::str::from_utf8(&buf[..read]).expect("utf-8"),
        )
        .expect("the planted document parses");
        let wan = parsed.interface("wan").expect("the planted interface");
        assert_eq!(wan.match_node, Some(0xfd58_0000));
        assert_eq!(wan.ipv4_method(), tairix_netconfig::Ipv4Method::Dhcp);
    }

    #[test]
    fn an_installed_app_bundle_reads_back_beside_its_help_tree() {
        use tairix_drv_fs_arxfs::SYSTEM_VOLUME_KEY;
        use tairix_partition::{parse_partition_table, PartitionBlock, PartitionType};

        // Synthetic bundle files: this test proves the *planting* of a
        // complete self-contained bundle (`AppInfo` + `Run` beside the
        // discovered `Help/` tree), not signature validity — the signed
        // composition is exercised where it is built. The manifests must
        // still *decode* (the program-library derivation reads every
        // planted app-store manifest and fails the build closed on
        // garbage); the `Run` bytes stay synthetic. The store paths mirror
        // the real installs (`Apps/<name>.app/…`, `Services/<name>.app/…`).
        const RUN: &[u8] = b"a Run rxe image's bytes (synthetic)";
        let ls_appinfo = crate::library::test_manifest("ls", None, None);
        let login_appinfo = crate::library::test_manifest("login", None, None);
        let app_files: [(&[&[u8]], &[u8]); 4] = [
            (&[b"Commands", b"ls.app", b"AppInfo"], ls_appinfo.as_slice()),
            (&[b"Commands", b"ls.app", b"Run"], RUN),
            (
                &[b"Services", b"login.app", b"AppInfo"],
                login_appinfo.as_slice(),
            ),
            (&[b"Services", b"login.app", b"Run"], RUN),
        ];

        let built = build_rpi_image(
            &test_kernel_elf(),
            &test_firmware(),
            &mut TestEntropy(9),
            ImageProfile::Installer,
            &[],
            &app_files,
            &test_network_conf(),
        )
        .expect("image builds");

        let mut disk = MemBlock::from_bytes(built.image).expect("whole sectors");
        let table = parse_partition_table(&mut disk).expect("the MBR parses");
        let system = table
            .first_of_type(PartitionType::ARXFSSystem)
            .expect("a /System partition is present");
        let window = PartitionBlock::from_partition(disk, &system).expect("the /System window");
        let mut sys = ARXFS::open_read_only(window, &SYSTEM_VOLUME_KEY)
            .expect("/System mounts read-only under the public key");

        // Every planted bundle file reads back byte-for-byte at its store
        // path…
        for (components, bytes) in &app_files {
            let mut node = sys.root();
            for component in *components {
                node = sys.lookup(node, component).expect("store path component");
            }
            let mut buf = vec![0u8; bytes.len() + 16];
            let read = sys.read_at(node, 0, &mut buf).expect("file reads back");
            assert_eq!(&buf[..read], *bytes);
        }
        // …and the command app's bundle directory also carries its
        // discovered `Help/` tree, so the on-disk bundle is complete.
        let mut help = sys.root();
        for component in [b"Commands".as_slice(), b"ls.app", b"Help", b"en-US"] {
            help = sys.lookup(help, component).expect("Help path component");
        }
        sys.lookup(help, b"ls.md")
            .expect("the bundle's default help document is planted beside Run");
    }

    #[test]
    fn the_desktop_icon_artwork_reads_back_from_system_graphics() {
        use tairix_drv_fs_arxfs::SYSTEM_VOLUME_KEY;
        use tairix_partition::{parse_partition_table, PartitionBlock, PartitionType};

        // The desktop's icon class masters ship under `/System/Graphics`,
        // discovered from disk (`tairix_syshelp::GRAPHICS_FILES`). One
        // master of each class format is read back, so neither the vector
        // nor the raster arm can rot, and an accidentally empty walk cannot
        // pass this test silently.
        let built = build_rpi_image(
            &test_kernel_elf(),
            &test_firmware(),
            &mut TestEntropy(9),
            ImageProfile::Installer,
            &[],
            &[],
            &test_network_conf(),
        )
        .expect("image builds");

        let mut disk = MemBlock::from_bytes(built.image).expect("whole sectors");
        let table = parse_partition_table(&mut disk).expect("the MBR parses");
        let system = table
            .first_of_type(PartitionType::ARXFSSystem)
            .expect("a /System partition is present");
        let window = PartitionBlock::from_partition(disk, &system).expect("the /System window");
        let mut sys = ARXFS::open_read_only(window, &SYSTEM_VOLUME_KEY)
            .expect("/System mounts read-only under the public key");

        for file in ["folder.svg", "file.png"] {
            let master = tairix_syshelp::GRAPHICS_FILES
                .iter()
                .find(|asset| {
                    asset.family == tairix_syshelp::GraphicsFamilyKind::Icon && asset.file == file
                })
                .expect("the icon master ships");
            let mut node = sys.root();
            for component in [b"Graphics".as_slice(), b"Icons", file.as_bytes()] {
                node = sys
                    .lookup(node, component)
                    .expect("Graphics/Icons path component");
            }
            let mut buf = vec![0u8; master.bytes.len() + 16];
            let read = sys.read_at(node, 0, &mut buf).expect("the icon reads back");
            assert_eq!(
                &buf[..read],
                master.bytes,
                "the planted {file} is byte-identical to the shipped master"
            );
        }
    }

    #[test]
    fn the_root_only_mounts_under_the_passphrase_derived_key() {
        let built = build_rpi_image(
            &test_kernel_elf(),
            &test_firmware(),
            &mut TestEntropy(9),
            ImageProfile::Installer,
            &[],
            &[],
            &test_network_conf(),
        )
        .expect("image builds");
        let descriptor = read_unlock_descriptor(&built.image);

        // A wrong passphrase derives a different key, which the volume's
        // AEAD-wrapped master key rejects — no separate oracle.
        let wrong = descriptor.derive_volume_key(b"not the passphrase");
        assert_ne!(wrong, built.root_key);
        let root_at = ROOT_PART_LBA as usize * SECTOR_BYTES;
        let root_len = ROOT_PART_SECTORS as usize * SECTOR_BYTES;
        let root_bytes = built.image[root_at..root_at + root_len].to_vec();
        assert!(ARXFS::open(
            MemBlock::from_bytes(root_bytes).expect("whole sectors"),
            &wrong,
        )
        .is_err());
    }

    #[test]
    fn a_debug_image_seeds_a_root_account_that_authenticates() {
        let built = build_rpi_image(
            &test_kernel_elf(),
            &test_firmware(),
            &mut TestEntropy(9),
            ImageProfile::Debug,
            &[],
            &[],
            &test_network_conf(),
        )
        .expect("image builds");

        let root_at = ROOT_PART_LBA as usize * SECTOR_BYTES;
        let root_len = ROOT_PART_SECTORS as usize * SECTOR_BYTES;
        let root_bytes = built.image[root_at..root_at + root_len].to_vec();
        let mut rfs = ARXFS::open(
            MemBlock::from_bytes(root_bytes).expect("whole sectors"),
            &built.root_key,
        )
        .expect("root partition mounts");
        let arxfs_root = rfs.root();
        let system = rfs.lookup(arxfs_root, b"System").expect("/System");
        let security = rfs.lookup(system, b"Security").expect("Security");

        let users = rfs
            .lookup(security, rootfs::USERS_DB_NAME.as_bytes())
            .expect("Users database exists");
        let mut buf = vec![0u8; tairix_users::MAX_DB_LEN];
        let read = rfs
            .read_at(users, 0, &mut buf)
            .expect("Users database reads");
        let text = core::str::from_utf8(&buf[..read]).expect("valid UTF-8");
        let db = UsersDb::parse(text).expect("seeded database parses");

        let record = db
            .authenticate(DEBUG_USERNAME, DEBUG_PASSWORD.as_bytes())
            .expect("root/root authenticates");
        // The debug administrator is an ordinary user-band principal: uid 0
        // belongs to the compiled-in `system` identity, never to a record
        // on disk.
        assert_eq!(record.uid(), DEBUG_UID);
        assert_eq!(record.shell(), Some("/System/Commands/elsh.app/Run"));
        // The seeded grant is exactly the shared administrator ceiling
        // (session baseline + administrative set) — the B3 regression
        // (`plans/CAPABILITY_USE.md` CU3): a root account without
        // `CAP_FS_ACCESS` cannot use the filesystem even once the
        // intersection is wired.
        assert_eq!(record.capabilities(), tairix_users::administrator_ceiling());
        assert!(record.capabilities().contains(CapabilityId::FS_ACCESS));
        assert!(db.authenticate(DEBUG_USERNAME, b"wrong").is_err());

        // The on-disk database holds the one human debug account and
        // nothing else: the system/service identity is compiled into the
        // kernel and never written to disk, and the kernel's identity
        // merge would refuse any on-disk record colliding with it
        // (`plans/USERS.md`).
        assert_eq!(db.records().len(), 1);
        for account in tairix_users::system_accounts().expect("compiled accounts build") {
            assert!(
                db.lookup(account.username()).is_none(),
                "compiled account {} must never be seeded on disk",
                account.username()
            );
        }
    }

    #[test]
    fn an_installer_image_seeds_an_empty_users_database() {
        let built = build_rpi_image(
            &test_kernel_elf(),
            &test_firmware(),
            &mut TestEntropy(9),
            ImageProfile::Installer,
            &[],
            &[],
            &test_network_conf(),
        )
        .expect("image builds");

        // The installer root is provisioned under the blank passphrase.
        let root_at = ROOT_PART_LBA as usize * SECTOR_BYTES;
        let root_len = ROOT_PART_SECTORS as usize * SECTOR_BYTES;
        let root_bytes = built.image[root_at..root_at + root_len].to_vec();
        let mut rfs = ARXFS::open(
            MemBlock::from_bytes(root_bytes).expect("whole sectors"),
            &built.root_key,
        )
        .expect("root partition mounts");
        let arxfs_root = rfs.root();
        let system = rfs.lookup(arxfs_root, b"System").expect("/System");
        let security = rfs.lookup(system, b"Security").expect("Security");

        let users = rfs
            .lookup(security, rootfs::USERS_DB_NAME.as_bytes())
            .expect("Users database exists");
        let mut buf = vec![0u8; tairix_users::MAX_DB_LEN];
        let read = rfs
            .read_at(users, 0, &mut buf)
            .expect("Users database reads");
        let text = core::str::from_utf8(&buf[..read]).expect("valid UTF-8");
        let db = UsersDb::parse(text).expect("seeded database parses");

        // An empty database — no debug account and nothing that can start
        // a session: the installer authors the first human user on first
        // boot, and the system/service identity is compiled into the
        // kernel, never written to disk (`plans/USERS.md`).
        assert!(db.records().is_empty());
        assert!(db.lookup(DEBUG_USERNAME).is_none());

        // The matching group registry carries only the well-known
        // removable-storage group — no debug group and no compiled system
        // group.
        let groups = rfs
            .lookup(security, rootfs::GROUPS_DB_NAME.as_bytes())
            .expect("Groups registry exists");
        let mut buf = vec![0u8; tairix_users::MAX_GROUPS_DB_LEN];
        let read = rfs
            .read_at(groups, 0, &mut buf)
            .expect("Groups registry reads");
        let text = core::str::from_utf8(&buf[..read]).expect("valid UTF-8");
        let registry = tairix_users::GroupsDb::parse(text).expect("seeded registry parses");
        assert_eq!(registry.records().len(), 1);
        assert!(registry.lookup(DEBUG_GROUP).is_none());
        assert_eq!(
            registry
                .lookup(tairix_users::STORAGE_GROUP)
                .map(GroupRecord::gid),
            Some(STORAGE_GID)
        );
    }

    #[test]
    fn a_debug_image_seeds_a_group_registry_the_root_account_resolves_against() {
        use tairix_users::GroupsDb;

        let built = build_rpi_image(
            &test_kernel_elf(),
            &test_firmware(),
            &mut TestEntropy(9),
            ImageProfile::Debug,
            &[],
            &[],
            &test_network_conf(),
        )
        .expect("image builds");

        let root_at = ROOT_PART_LBA as usize * SECTOR_BYTES;
        let root_len = ROOT_PART_SECTORS as usize * SECTOR_BYTES;
        let root_bytes = built.image[root_at..root_at + root_len].to_vec();
        let mut rfs = ARXFS::open(
            MemBlock::from_bytes(root_bytes).expect("whole sectors"),
            &built.root_key,
        )
        .expect("root partition mounts");
        let arxfs_root = rfs.root();
        let system = rfs.lookup(arxfs_root, b"System").expect("/System");
        let security = rfs.lookup(system, b"Security").expect("Security");

        let groups = rfs
            .lookup(security, rootfs::GROUPS_DB_NAME.as_bytes())
            .expect("Groups registry exists");
        let mut buf = vec![0u8; tairix_users::MAX_GROUPS_DB_LEN];
        let read = rfs
            .read_at(groups, 0, &mut buf)
            .expect("Groups registry reads");
        let text = core::str::from_utf8(&buf[..read]).expect("valid UTF-8");
        let db = GroupsDb::parse(text).expect("seeded registry parses");
        // The seeded root account's primary gid must resolve to a real group,
        // or the kernel's identity-table build would fail closed.
        assert!(db.lookup_gid(DEBUG_PRIMARY_GID).is_some());
        assert_eq!(
            db.lookup(DEBUG_GROUP).map(GroupRecord::gid),
            Some(DEBUG_PRIMARY_GID)
        );
        // The well-known removable-storage group ships too, so the unlock
        // resolves it by name and arms the hotplug-volume identity map.
        assert_eq!(
            db.lookup(STORAGE_GROUP).map(GroupRecord::gid),
            Some(STORAGE_GID)
        );
        // The `system` and `services` groups are compiled into the kernel
        // with the system accounts, never seeded to disk — a reserved
        // record here would be refused by the kernel's identity merge.
        assert!(db.lookup(tairix_users::SYSTEM_GROUP).is_none());
        assert!(db.lookup(tairix_users::SERVICES_GROUP).is_none());
    }

    #[test]
    fn a_bad_kernel_fails_the_whole_build() {
        assert!(build_rpi_image(
            b"not an elf",
            &test_firmware(),
            &mut TestEntropy(9),
            ImageProfile::Installer,
            &[],
            &[],
            &test_network_conf(),
        )
        .is_err());
    }

    #[test]
    fn each_profile_maps_to_its_unlock_passphrase() {
        // The debug image unlocks with `root` (typed at the prompt); the
        // installer with a blank passphrase (auto-entered, no prompt) —
        // . The two differ, so the wrong-passphrase test
        // above is a genuine negative.
        assert_eq!(passphrase_for(ImageProfile::Debug), DEBUG_PASSPHRASE);
        assert_eq!(
            passphrase_for(ImageProfile::Installer),
            INSTALLER_PASSPHRASE
        );
        assert_eq!(passphrase_for(ImageProfile::Debug), b"root");
        assert_eq!(passphrase_for(ImageProfile::Installer), b"");
        assert_ne!(
            passphrase_for(ImageProfile::Debug),
            passphrase_for(ImageProfile::Installer)
        );
    }

    #[test]
    fn generated_config_uses_the_architecture_console_baud() {
        assert_eq!(CONSOLE_BAUD, 115_200);
        assert!(fatboot::config_txt(false, CONSOLE_BAUD).contains("init_uart_baud=115200\n"));
    }

    #[test]
    fn image_profiles_have_one_spelling_each() {
        assert_eq!(ImageProfile::from_label("debug"), Some(ImageProfile::Debug));
        assert_eq!(
            ImageProfile::from_label("installer"),
            Some(ImageProfile::Installer)
        );
        assert_eq!(ImageProfile::from_label("Debug"), None);
        assert_eq!(ImageProfile::from_label(""), None);
        assert_eq!(ImageProfile::Debug.label(), "debug");
        assert_eq!(ImageProfile::Installer.label(), "installer");
    }

    #[test]
    fn volume_key_renders_as_hex() {
        let text = volume_key_to_hex(&TEST_KEY);
        assert_eq!(text.len(), VOLUME_KEY_LEN * 2 + 1);
        assert!(text.ends_with('\n'));
        assert!(text.trim().bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(text.starts_with("4242"));
    }

    #[test]
    fn host_entropy_draws_nonzero_bytes() {
        let mut out = [0u8; 64];
        HostEntropy.fill(&mut out).expect("host entropy available");
        assert!(out.iter().any(|&b| b != 0));
    }
}
