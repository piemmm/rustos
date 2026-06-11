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
//!   encrypted volume carrying the `AGENTS.md` §16 directory skeleton.
//!   The volume key is drawn per image and returned to the operator; it is
//!   never stored inside the image.
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
pub mod mbr;
pub mod rootfs;

pub use rustos_drv_fs_rustfs::{EntropySource, VolumeKey, VOLUME_KEY_LEN};

use device::SECTOR_BYTES;
use firmware::FirmwareFile;
use mbr::{PartitionExtent, PART_TYPE_FAT32_LBA, PART_TYPE_RUSTFS};
use rustos_abi::DriverError;

/// First sector of the FAT32 boot partition (1 MiB alignment, the
/// universal SD-card convention).
pub const BOOT_PART_LBA: u32 = 2048;

/// Sectors in the FAT32 boot partition: 64 MiB — ample for the firmware
/// blobs (~2.5 MiB) plus the kernel, while keeping the image small.
pub const BOOT_PART_SECTORS: u32 = 131_072;

/// First sector of the `RustFS` root partition (contiguous with the boot
/// partition, which already ends 1 MiB-aligned).
pub const ROOT_PART_LBA: u32 = BOOT_PART_LBA + BOOT_PART_SECTORS;

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
    /// The requested partition table is invalid.
    PartitionTable(&'static str),
    /// Authoring the FAT32 boot partition failed.
    BootPartition(DriverError),
    /// Authoring the `RustFS` root partition failed.
    RootPartition(DriverError),
    /// Host randomness for the volume key is unavailable.
    Entropy(String),
    /// A volume-key file is malformed.
    VolumeKeyFile(String),
}

impl fmt::Display for MkimageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Manifest(msg) => write!(f, "firmware manifest: {msg}"),
            Self::Firmware(msg) => write!(f, "firmware input: {msg}"),
            Self::KernelElf(msg) => write!(f, "kernel ELF: {msg}"),
            Self::PartitionTable(msg) => write!(f, "partition table: {msg}"),
            Self::BootPartition(err) => write!(f, "boot partition: driver error {err:?}"),
            Self::RootPartition(err) => write!(f, "root partition: driver error {err:?}"),
            Self::Entropy(msg) => write!(f, "host entropy: {msg}"),
            Self::VolumeKeyFile(msg) => write!(f, "volume-key file: {msg}"),
        }
    }
}

impl std::error::Error for MkimageError {}

/// The assembled image plus the material the operator must keep.
pub struct RpiImage {
    /// The flashable image bytes ([`IMAGE_SECTORS`] sectors).
    pub image: Vec<u8>,
    /// The root volume key the image was provisioned under. Mounting the
    /// root requires it; it exists nowhere inside the image.
    pub root_key: VolumeKey,
}

/// Build the flashable Raspberry Pi 4 SD image.
///
/// `kernel_elf` is the freestanding aarch64 `rustos-kernel` ELF;
/// `firmware` is the verified blob set from
/// [`firmware::FirmwareManifest::load_dir`]; `root_key` is the volume key
/// to provision the root under (drawn from [`HostEntropy`] by the CLI);
/// `entropy` seeds the root volume's internal key hierarchy.
///
/// # Errors
///
/// Any [`MkimageError`] from the kernel conversion, partition authoring,
/// or assembly; the build fails closed rather than emitting a partial
/// image.
pub fn build_rpi_image(
    kernel_elf: &[u8],
    firmware: &[FirmwareFile],
    root_key: &VolumeKey,
    entropy: &mut dyn EntropySource,
) -> Result<RpiImage, MkimageError> {
    let kernel8 = elfflat::elf_to_flat(kernel_elf)?;
    let boot = fatboot::build_boot_partition(u64::from(BOOT_PART_SECTORS), firmware, &kernel8)?;
    let root = rootfs::build_root_partition(u64::from(ROOT_PART_SECTORS), root_key, entropy)?;

    let mbr_sector = mbr::encode_mbr(&[
        PartitionExtent {
            type_byte: PART_TYPE_FAT32_LBA,
            start_lba: BOOT_PART_LBA,
            sectors: BOOT_PART_SECTORS,
        },
        PartitionExtent {
            type_byte: PART_TYPE_RUSTFS,
            start_lba: ROOT_PART_LBA,
            sectors: ROOT_PART_SECTORS,
        },
    ])?;

    let mut image = vec![0u8; IMAGE_SECTORS as usize * SECTOR_BYTES];
    image[..SECTOR_BYTES].copy_from_slice(&mbr_sector);
    let boot_at = BOOT_PART_LBA as usize * SECTOR_BYTES;
    image[boot_at..boot_at + boot.len()].copy_from_slice(&boot);
    let root_at = ROOT_PART_LBA as usize * SECTOR_BYTES;
    image[root_at..root_at + root.len()].copy_from_slice(&root);

    Ok(RpiImage {
        image,
        root_key: *root_key,
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

/// Parse a key file written by [`volume_key_to_hex`].
///
/// # Errors
///
/// [`MkimageError::VolumeKeyFile`] unless the trimmed body is exactly 64
/// hex digits.
pub fn volume_key_from_hex(text: &str) -> Result<VolumeKey, MkimageError> {
    let body = text.trim();
    let bytes = body.as_bytes();
    if bytes.len() != VOLUME_KEY_LEN * 2 {
        return Err(MkimageError::VolumeKeyFile(
            "a volume-key file holds exactly 64 hex digits".into(),
        ));
    }
    let mut key = [0u8; VOLUME_KEY_LEN];
    for (i, pair) in bytes.chunks_exact(2).enumerate() {
        let hi = (pair[0] as char)
            .to_digit(16)
            .ok_or_else(|| MkimageError::VolumeKeyFile("non-hex digit".into()))?;
        let lo = (pair[1] as char)
            .to_digit(16)
            .ok_or_else(|| MkimageError::VolumeKeyFile("non-hex digit".into()))?;
        key[i] = u8::try_from(hi * 16 + lo)
            .map_err(|_| MkimageError::VolumeKeyFile("non-hex digit".into()))?;
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use device::MemBlock;
    use rustos_abi::driver::filesystem::{FilesystemRead, NodeKind};
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

    #[test]
    fn assembles_a_flashable_image_both_partitions_mount() {
        let built = build_rpi_image(
            &test_kernel_elf(),
            &test_firmware(),
            &TEST_KEY,
            &mut TestEntropy(9),
        )
        .expect("image builds");
        assert_eq!(built.image.len(), IMAGE_SECTORS as usize * SECTOR_BYTES);
        assert_eq!(built.root_key, TEST_KEY);

        // The MBR carries the expected table.
        assert_eq!(built.image[510], 0x55);
        assert_eq!(built.image[511], 0xaa);
        assert_eq!(built.image[446 + 4], mbr::PART_TYPE_FAT32_LBA);
        assert_eq!(built.image[446 + 16 + 4], mbr::PART_TYPE_RUSTFS);

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

        // The root partition mounts under the provisioned key.
        let root_at = ROOT_PART_LBA as usize * SECTOR_BYTES;
        let root_len = ROOT_PART_SECTORS as usize * SECTOR_BYTES;
        let root_bytes = built.image[root_at..root_at + root_len].to_vec();
        let mut rfs = RustFs::open(
            MemBlock::from_bytes(root_bytes).expect("whole sectors"),
            &TEST_KEY,
        )
        .expect("root partition mounts");
        let rustfs_root = rfs.root();
        rfs.lookup(rustfs_root, b"System").expect("/System exists");
    }

    #[test]
    fn a_bad_kernel_fails_the_whole_build() {
        assert!(build_rpi_image(
            b"not an elf",
            &test_firmware(),
            &TEST_KEY,
            &mut TestEntropy(9)
        )
        .is_err());
    }

    #[test]
    fn volume_key_hex_round_trips_and_rejects_garbage() {
        let text = volume_key_to_hex(&TEST_KEY);
        assert_eq!(text.len(), VOLUME_KEY_LEN * 2 + 1);
        assert_eq!(volume_key_from_hex(&text).expect("round trip"), TEST_KEY);

        assert!(volume_key_from_hex("too short").is_err());
        let bad = "zz".repeat(VOLUME_KEY_LEN);
        assert!(volume_key_from_hex(&bad).is_err());
    }

    #[test]
    fn host_entropy_draws_nonzero_bytes() {
        let mut out = [0u8; 64];
        HostEntropy.fill(&mut out).expect("host entropy available");
        assert!(out.iter().any(|&b| b != 0));
    }
}
