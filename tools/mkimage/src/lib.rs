//! Platform image builders for RustOS (`AGENTS.md` §12).
//!
//! `rustos-mkimage` authors flashable images in pure Rust: the partition
//! contents are laid down by the **real** in-tree filesystem drivers — the
//! same code the booted system mounts the volumes with — so the image
//! author and its consumer can never drift (`AGENTS.md` §2.2). There is no
//! shelling out to `parted`/`mkfs`/`xorriso`.
//!
//! ## `images/rustos-aarch64-rpi.img` (`plans/PI.md` P9)
//!
//! [`build_rpi_image`] assembles the flashable Raspberry Pi 4 SD image:
//!
//! - **MBR** ([`mbr`]): two primary partitions, both 1 MiB-aligned.
//! - **Boot partition** ([`fatboot`], FAT32, [`BOOT_PART_SECTORS`]): the
//!   pinned third-party firmware blobs ([`firmware`], `AGENTS.md` §19.3),
//!   the generated `config.txt`, and `kernel8.img` — the freestanding
//!   aarch64 `rustos-kernel` ELF flattened by [`elfflat`].
//! - **Root partition** ([`rootfs`], `RustFS`, [`ROOT_PART_SECTORS`]): an
//!   encrypted volume carrying the `AGENTS.md` §16 directory skeleton. Its
//!   volume key is **derived from a passphrase** (`AGENTS.md` §11): the
//!   build provisions an
//!   [`UnlockDescriptor`] (a
//!   per-volume random salt + PBKDF2 iteration count), derives the volume
//!   key from [`IMAGE_PASSPHRASE`] under it, provisions the root with that
//!   key, and lays the plaintext descriptor on the boot partition
//!   ([`fatboot::ROOT_UNLOCK_NAME`]) so the bootstrap can re-derive the key
//!   before mounting. The passphrase itself is never stored in the image.
//!
//! Two [`ImageProfile`]s exist. **Installer** is the shippable form: the
//! root carries no user accounts, and the §11 installer authors
//! `/System/Security/Users` on first boot. **Debug** is the development
//! form: the root is seeded with a `root`/`root` account
//! ([`DEBUG_USERNAME`]/[`DEBUG_PASSWORD`], salted and hashed per build) so
//! the login prompt is usable without running the installer. A debug image
//! must never ship.
//!
//! The builder is driven by `cargo xtask image --target aarch64-rpi` (or
//! `cargo xtask build --target aarch64-rpi`) and by the `rustos-mkimage`
//! binary directly; see `docs/src/install/raspberry_pi.md`.

use std::fmt;
use std::io::Read;

pub mod device;
pub mod elfflat;
pub mod fatboot;
pub mod firmware;
pub mod rootfs;

pub use rustos_drv_fs_rustfs::{
    EntropySource, UnlockDescriptor, VolumeKey, UNLOCK_DEFAULT_ITERATIONS, UNLOCK_DESCRIPTOR_LEN,
    VOLUME_KEY_LEN,
};

use device::SECTOR_BYTES;
use firmware::FirmwareFile;
use rustos_abi::{CapabilityId, DriverError};
use rustos_caps::CapabilitySet;
use rustos_partition::mbr::{self, MbrError};
use rustos_partition::{Partition, PartitionType};
use rustos_users::{AccountState, Gid, Identity, Salt, Uid, UserRecord, UsersDb};

/// First sector of the FAT32 boot partition (1 MiB alignment, the
/// universal SD-card convention).
pub const BOOT_PART_LBA: u32 = 2048;

/// Sectors in the FAT32 boot partition: 64 MiB — ample for the firmware
/// blobs (~2.5 MiB) plus the kernel, while keeping the image small.
pub const BOOT_PART_SECTORS: u32 = 131_072;

/// First sector of the read-only `RustFS` `/System` partition (contiguous
/// with the boot partition, which already ends 1 MiB-aligned). This is the
/// design-B pre-unlock signed-driver store (`plans/PI.md`).
pub const SYSTEM_PART_LBA: u32 = BOOT_PART_LBA + BOOT_PART_SECTORS;

/// Sectors in the read-only `RustFS` `/System` partition: 64 MiB — the
/// §16.2 skeleton plus headroom for the signed driver bundles that land
/// here in the later design-B increments.
pub const SYSTEM_PART_SECTORS: u32 = 131_072;

/// First sector of the encrypted `RustFS` data-root partition (contiguous
/// with the `/System` partition, which already ends 1 MiB-aligned).
pub const ROOT_PART_LBA: u32 = SYSTEM_PART_LBA + SYSTEM_PART_SECTORS;

/// Sectors in the `RustFS` root partition: 64 MiB — the §16 skeleton plus
/// installer headroom. The installer grows the layout on first boot;
/// `RustFs::grow` expands a volume to its device, so a card-sized root is
/// a first-boot job, not an image-build job.
pub const ROOT_PART_SECTORS: u32 = 131_072;

/// Total sectors in the assembled image.
pub const IMAGE_SECTORS: u32 = ROOT_PART_LBA + ROOT_PART_SECTORS;

/// Everything that can go wrong while authoring an image. Every variant is
/// a refusal: mkimage never emits a best-effort image (`AGENTS.md` §5.4).
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
    /// Authoring the read-only `RustFS` `/System` partition failed.
    SystemPartition(DriverError),
    /// Authoring the `RustFS` root partition failed.
    RootPartition(DriverError),
    /// Host randomness for the volume key is unavailable.
    Entropy(String),
    /// Provisioning or encoding the passphrase-unlock descriptor failed.
    Unlock(DriverError),
    /// Authoring the seeded user database failed.
    UsersDb(String),
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
            Self::UsersDb(msg) => write!(f, "users database: {msg}"),
        }
    }
}

impl From<MbrError> for MkimageError {
    fn from(err: MbrError) -> Self {
        Self::Partition(err)
    }
}

impl std::error::Error for MkimageError {}

/// Which kind of image to author (`AGENTS.md` §12).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ImageProfile {
    /// Development image: the root volume is seeded with the
    /// [`DEBUG_USERNAME`]/[`DEBUG_PASSWORD`] account so the login prompt is
    /// usable without the installer. Never shipped.
    Debug,
    /// Shippable image: no user accounts; the §11 installer authors
    /// `/System/Security/Users` on first boot.
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
}

/// Username of the debug-profile test account.
pub const DEBUG_USERNAME: &str = "root";

/// Password of the debug-profile test account. Knowable by design — the
/// debug image exists for bring-up on development hardware and must never
/// ship; the installer image seeds no account at all.
pub const DEBUG_PASSWORD: &str = "root";

/// Passphrase the `rustos-mkimage` images' encrypted root is unlocked
/// with — **blank** for both profiles (`AGENTS.md` §11).
///
/// These are special-case images: the debug image must never ship, and
/// the installer image's root is **re-provisioned by the §11 installer**,
/// which sets the user's real, operator-chosen passphrase when it authors
/// the production root on first boot. Until then a blank passphrase is
/// auto-entered, so neither image prompts. The volume is still fully
/// encrypted: a blank passphrase is run through PBKDF2 over the
/// descriptor's per-volume random salt to derive a real 256-bit
/// [`VolumeKey`], exactly as a typed one would be. A shippable,
/// user-installed root MUST be unlocked by a passphrase the operator
/// chooses at install time — never this blank default.
pub const IMAGE_PASSPHRASE: &[u8] = b"";

/// Build the debug-profile `/System/Security/Users` text: the single
/// `root` account, its password salted from `entropy` and hashed at the
/// default PBKDF2 cost, granted the administrative capability ceiling a
/// bring-up session needs (`AGENTS.md` §5.2 — powers come from
/// capabilities, not from `uid 0`).
fn debug_users_db(entropy: &mut dyn EntropySource) -> Result<String, MkimageError> {
    let mut salt: Salt = [0u8; rustos_users::SALT_LEN];
    entropy
        .fill(&mut salt)
        .map_err(|e| MkimageError::Entropy(format!("users salt: {e:?}")))?;

    let mut capabilities = CapabilitySet::empty();
    for cap in [
        CapabilityId::USER_ADMIN,
        CapabilityId::FS_MOUNT,
        CapabilityId::PROC_SPAWN,
        CapabilityId::CONSOLE_READ,
        CapabilityId::CONSOLE_WRITE,
    ] {
        capabilities.insert(cap);
    }

    let record = UserRecord::with_password(
        Identity {
            username: DEBUG_USERNAME,
            uid: Uid(0),
            primary_gid: Gid(0),
            supplementary_gids: &[],
            display_name: "System Administrator",
            home: "/Users/root",
            shell: "/Apps/Shell.app/Run",
            capabilities,
            state: AccountState::Active,
        },
        DEBUG_PASSWORD.as_bytes(),
        salt,
        rustos_users::DEFAULT_ITERATIONS,
    )
    .map_err(|e| MkimageError::UsersDb(format!("debug root record: {e}")))?;
    let db = UsersDb::new(vec![record])
        .map_err(|e| MkimageError::UsersDb(format!("debug database: {e}")))?;
    Ok(db.serialise())
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
    /// [`IMAGE_PASSPHRASE`] and [`Self::unlock`]. Mounting the root needs
    /// it; the key itself is stored nowhere inside the image (it can be
    /// re-derived from the on-image descriptor and the passphrase).
    pub root_key: VolumeKey,
}

/// Build the flashable Raspberry Pi 4 SD image.
///
/// `kernel_elf` is the freestanding aarch64 `rustos-kernel` ELF;
/// `firmware` is the verified blob set from
/// [`firmware::FirmwareManifest::load_dir`]; `passphrase` is the operator
/// passphrase the root's volume key is derived from (blank for both
/// `rustos-mkimage` profiles — see [`IMAGE_PASSPHRASE`]); `entropy` draws
/// the unlock descriptor's salt, the root volume's internal key
/// hierarchy, and, on a debug build, the seeded account's password salt;
/// `profile` selects the [`ImageProfile`].
///
/// The root is encrypted under a [`VolumeKey`] **derived** from
/// `passphrase` through a freshly provisioned
/// [`UnlockDescriptor`]; the
/// plaintext descriptor is laid on the boot partition
/// ([`fatboot::ROOT_UNLOCK_NAME`]) so the bootstrap re-derives the key
/// from the passphrase before mounting (`AGENTS.md` §11).
///
/// # Errors
///
/// Any [`MkimageError`] from the kernel conversion, descriptor
/// provisioning, partition authoring, or assembly; the build fails closed
/// rather than emitting a partial image.
pub fn build_rpi_image(
    kernel_elf: &[u8],
    firmware: &[FirmwareFile],
    passphrase: &[u8],
    entropy: &mut dyn EntropySource,
    profile: ImageProfile,
) -> Result<RpiImage, MkimageError> {
    let users_db = match profile {
        ImageProfile::Debug => Some(debug_users_db(entropy)?),
        ImageProfile::Installer => None,
    };
    let kernel8 = elfflat::elf_to_flat(kernel_elf)?;

    // Derive the root volume key from the passphrase under a fresh
    // per-volume descriptor, then lay the (non-secret) descriptor beside
    // the volume on the boot partition so the bootstrap can re-derive it.
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
    )?;
    let system = rootfs::build_system_partition(u64::from(SYSTEM_PART_SECTORS), entropy)?;
    let root = rootfs::build_root_partition(
        u64::from(ROOT_PART_SECTORS),
        &root_key,
        entropy,
        users_db.as_deref(),
    )?;

    let mbr_sector = mbr::encode(&[
        Partition {
            ty: PartitionType::FatBoot,
            start_lba: u64::from(BOOT_PART_LBA),
            block_count: u64::from(BOOT_PART_SECTORS),
        },
        Partition {
            ty: PartitionType::RustFsSystem,
            start_lba: u64::from(SYSTEM_PART_LBA),
            block_count: u64::from(SYSTEM_PART_SECTORS),
        },
        Partition {
            ty: PartitionType::RustFsRoot,
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
/// material (`AGENTS.md` §5.4).
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
    use rustos_abi::driver::filesystem::{FilesystemRead, FilesystemWrite, NodeKind};
    use rustos_drv_fs_fat32::Fat32;
    use rustos_drv_fs_rustfs::RustFs;

    const TEST_KEY: VolumeKey = [0x42; VOLUME_KEY_LEN];

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
            IMAGE_PASSPHRASE,
            &mut TestEntropy(9),
            ImageProfile::Installer,
        )
        .expect("image builds");
        assert_eq!(built.image.len(), IMAGE_SECTORS as usize * SECTOR_BYTES);

        // The MBR carries the expected three-partition table: FAT boot,
        // read-only `/System`, encrypted data root.
        assert_eq!(built.image[510], 0x55);
        assert_eq!(built.image[511], 0xaa);
        assert_eq!(built.image[446 + 4], mbr::PART_TYPE_FAT32_LBA);
        assert_eq!(built.image[446 + 16 + 4], mbr::PART_TYPE_RUSTFS_SYSTEM);
        assert_eq!(built.image[446 + 32 + 4], mbr::PART_TYPE_RUSTFS);

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
        // image was provisioned under (the bootstrap's path, §11).
        let descriptor = read_unlock_descriptor(&built.image);
        assert_eq!(descriptor, built.unlock);
        assert_eq!(
            descriptor.derive_volume_key(IMAGE_PASSPHRASE),
            built.root_key
        );

        // The root partition mounts under that re-derived key.
        let root_at = ROOT_PART_LBA as usize * SECTOR_BYTES;
        let root_len = ROOT_PART_SECTORS as usize * SECTOR_BYTES;
        let root_bytes = built.image[root_at..root_at + root_len].to_vec();
        let mut rfs = RustFs::open(
            MemBlock::from_bytes(root_bytes).expect("whole sectors"),
            &descriptor.derive_volume_key(IMAGE_PASSPHRASE),
        )
        .expect("root partition mounts");
        let rustfs_root = rfs.root();
        rfs.lookup(rustfs_root, b"System").expect("/System exists");

        // An installer image ships no user accounts (§11 first-boot job).
        let system = rfs.lookup(rustfs_root, b"System").expect("/System");
        let security = rfs.lookup(system, b"Security").expect("Security");
        assert!(rfs
            .lookup(security, rootfs::USERS_DB_NAME.as_bytes())
            .is_err());
    }

    #[test]
    fn the_system_partition_mounts_read_only_and_carries_the_skeleton() {
        use rustos_drv_fs_rustfs::SYSTEM_VOLUME_KEY;
        use rustos_partition::{parse_partition_table, PartitionBlock, PartitionType};

        let built = build_rpi_image(
            &test_kernel_elf(),
            &test_firmware(),
            IMAGE_PASSPHRASE,
            &mut TestEntropy(9),
            ImageProfile::Installer,
        )
        .expect("image builds");

        // The whole-disk table parses and locates the read-only `/System`
        // partition by role at the documented offset.
        let mut disk = MemBlock::from_bytes(built.image.clone()).expect("whole sectors");
        let table = parse_partition_table(&mut disk).expect("the MBR parses");
        let system = table
            .first_of_type(PartitionType::RustFsSystem)
            .expect("a /System partition is present");
        assert_eq!(system.start_lba, u64::from(SYSTEM_PART_LBA));
        assert_eq!(system.block_count, u64::from(SYSTEM_PART_SECTORS));

        // It mounts read-only under the non-secret well-known key and its
        // root *is* `/System`, carrying the §16.2 skeleton directly.
        let window = PartitionBlock::from_partition(disk, &system).expect("the /System window");
        let mut sys = RustFs::open_read_only(window, &SYSTEM_VOLUME_KEY)
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

        // A read-only mount refuses mutation fail-closed (`AGENTS.md` §5.4).
        assert_eq!(
            sys.create(sys_root, b"x", NodeKind::Directory),
            Err(DriverError::PermissionDenied)
        );
    }

    #[test]
    fn the_root_only_mounts_under_the_passphrase_derived_key() {
        let built = build_rpi_image(
            &test_kernel_elf(),
            &test_firmware(),
            IMAGE_PASSPHRASE,
            &mut TestEntropy(9),
            ImageProfile::Installer,
        )
        .expect("image builds");
        let descriptor = read_unlock_descriptor(&built.image);

        // A wrong passphrase derives a different key, which the volume's
        // AEAD-wrapped master key rejects — no separate oracle (§5.4).
        let wrong = descriptor.derive_volume_key(b"not the passphrase");
        assert_ne!(wrong, built.root_key);
        let root_at = ROOT_PART_LBA as usize * SECTOR_BYTES;
        let root_len = ROOT_PART_SECTORS as usize * SECTOR_BYTES;
        let root_bytes = built.image[root_at..root_at + root_len].to_vec();
        assert!(RustFs::open(
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
            IMAGE_PASSPHRASE,
            &mut TestEntropy(9),
            ImageProfile::Debug,
        )
        .expect("image builds");

        let root_at = ROOT_PART_LBA as usize * SECTOR_BYTES;
        let root_len = ROOT_PART_SECTORS as usize * SECTOR_BYTES;
        let root_bytes = built.image[root_at..root_at + root_len].to_vec();
        let mut rfs = RustFs::open(
            MemBlock::from_bytes(root_bytes).expect("whole sectors"),
            &built.root_key,
        )
        .expect("root partition mounts");
        let rustfs_root = rfs.root();
        let system = rfs.lookup(rustfs_root, b"System").expect("/System");
        let security = rfs.lookup(system, b"Security").expect("Security");

        let users = rfs
            .lookup(security, rootfs::USERS_DB_NAME.as_bytes())
            .expect("Users database exists");
        let mut buf = vec![0u8; rustos_users::MAX_DB_LEN];
        let read = rfs
            .read_at(users, 0, &mut buf)
            .expect("Users database reads");
        let text = core::str::from_utf8(&buf[..read]).expect("valid UTF-8");
        let db = UsersDb::parse(text).expect("seeded database parses");

        let record = db
            .authenticate(DEBUG_USERNAME, DEBUG_PASSWORD.as_bytes())
            .expect("root/root authenticates");
        assert_eq!(record.uid(), Uid(0));
        assert_eq!(record.shell(), "/Apps/Shell.app/Run");
        assert!(record.capabilities().contains(CapabilityId::USER_ADMIN));
        assert!(db.authenticate(DEBUG_USERNAME, b"wrong").is_err());
    }

    #[test]
    fn a_bad_kernel_fails_the_whole_build() {
        assert!(build_rpi_image(
            b"not an elf",
            &test_firmware(),
            IMAGE_PASSPHRASE,
            &mut TestEntropy(9),
            ImageProfile::Installer
        )
        .is_err());
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
